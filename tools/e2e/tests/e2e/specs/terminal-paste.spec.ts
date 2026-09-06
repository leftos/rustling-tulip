import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

import { browser } from "@wdio/globals";
import { expect } from "chai";

import { DaemonWsClient } from "../../../src/ws-client.js";
import {
  dismissLayoutChooser,
  openSessionPane,
  spawnSession,
} from "../../../src/session-helpers.js";
import type { DaemonMessage, RepoEntry } from "../../../src/types.js";

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;
const SESSION_OUTPUT_TIMEOUT = 20_000;
const repoRoot = resolve(
  fileURLToPath(new URL("../../../../..", import.meta.url)),
);

describe("terminal paste", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let registeredRepoId: string | null = null;
  let spawnedSessionId: string | null = null;

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
    // A fresh e2e client always gets the mandatory first-connect LayoutChooser;
    // dismiss it so its backdrop doesn't intercept later session-pane clicks.
    await dismissLayoutChooser(APP_BOOT_TIMEOUT);
    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    const parent = join(repoRoot, ".tmp", "e2e");
    await mkdir(parent, { recursive: true });
    fixtureRepo = await mkdtemp(join(parent, "rt-e2e-terminal-paste-"));
    await writeFile(join(fixtureRepo, "README.md"), "fixture for paste e2e\n");
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);
  });

  after(async function () {
    if (ws && spawnedSessionId) {
      try {
        ws.send({
          type: "stop_session",
          session_id: spawnedSessionId,
          cleanup: registeredRepoId
            ? [
                { repo_id: registeredRepoId, remove_worktree: false, branch: "auto" },
              ]
            : [],
        });
        await delay(500);
      } catch {
        /* best-effort cleanup */
      }
    }
    if (ws && registeredRepoId) {
      try {
        ws.send({ type: "remove_repo", repo_id: registeredRepoId });
        await delay(200);
      } catch {
        /* best-effort cleanup */
      }
    }
    if (ws) await ws.close();
    if (fixtureRepo) {
      await rm(fixtureRepo, { recursive: true, force: true });
    }
  });

  it("delivers a large paste byte-for-byte to the agent", async function () {
    expect(ws, "ws").to.not.equal(null);
    expect(fixtureRepo, "fixtureRepo").to.not.equal(null);
    if (!ws || !fixtureRepo) throw new Error("setup failed");

    const repo = await registerRepo(ws, fixtureRepo);
    registeredRepoId = repo.id;
    const session = await spawnSession(ws, {
      label: "paste",
      repoId: registeredRepoId,
      timeoutMs: 15_000,
    });
    spawnedSessionId = session.id;
    await openSessionPane(spawnedSessionId);
    await waitForBufferMarkers(spawnedSessionId, [
      "[fake-claude] ready",
    ], SESSION_OUTPUT_TIMEOUT);
    // Single line, no newline, EOT-terminated: the shim checksums the payload's
    // own bytes (the trailing EOT is just the delivery terminator, dropped
    // before hashing). A missing middle — the reported bug — changes both the
    // byte count and the sha, which the old "does BEGIN and END appear?"
    // assertion could never detect.
    const payload =
      `RT_PASTE_BEGIN_${"0123456789abcdef".repeat(8192)}_RT_PASTE_END`;
    const expectedBytes = Buffer.byteLength(payload, "utf8");
    const expectedSha = createHash("sha256").update(payload, "utf8")
      .digest("hex");

    // Two platform limits shape this test. WebDriver can't synthesize a real
    // WebView2 clipboard `paste` event, so the browser clipboard read and the
    // native-clipboard override are covered by hand-testing, not here. And
    // ConPTY strips bracketed-paste markers when delivering raw-mode input to a
    // Node child, so the payload is EOT-terminated instead. Driving xterm's
    // `paste()` API still runs the full onData → send_input → daemon-chunking →
    // tracer → ConPTY → fake-claude path — where a dropped middle would occur.
    await pasteViaXterm(spawnedSessionId, `${payload}\x04`);

    const result = await waitForPasteResult(
      spawnedSessionId,
      SESSION_OUTPUT_TIMEOUT,
    );
    expect(result.bytes, "received byte count").to.equal(expectedBytes);
    expect(result.sha, "received payload sha256").to.equal(expectedSha);
  });
});

async function registerRepo(
  ws: DaemonWsClient,
  fixturePath: string,
): Promise<RepoEntry> {
  const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
  ws.send({ type: "add_repo", path: fixturePath, name: "rt-e2e-paste" });
  const repos = await reposPromise;
  const repo = repos.repos.find(
    (r: RepoEntry) =>
      r.path === fixturePath || r.path === fixturePath.replace(/\\/g, "/"),
  );
  if (!repo) {
    throw new Error(`fixture repo was not registered: ${fixturePath}`);
  }
  return repo;
}

async function pasteViaXterm(sessionId: string, text: string): Promise<void> {
  await browser.execute(
    `
    const term = globalThis.__rt_terms && globalThis.__rt_terms.get(${JSON.stringify(sessionId)});
    if (!term) {
      throw new Error("terminal not found");
    }
    term.paste(${JSON.stringify(text)});
    `,
  );
}

async function waitForBufferMarkers(
  sessionId: string,
  markers: string[],
  timeoutMs: number,
): Promise<string> {
  const deadline = Date.now() + timeoutMs;
  let lastSeen = "";
  while (Date.now() < deadline) {
    const text = await readTerminalBuffer(sessionId);
    lastSeen = text;
    if (markers.every((marker) => text.includes(marker))) return text;
    await delay(250);
  }
  throw new Error(
    `xterm buffer for ${sessionId} never contained ${markers.join(", ")} ` +
      `within ${timeoutMs}ms.\nLast seen:\n${lastSeen.slice(-1_000)}`,
  );
}

async function waitForPasteResult(
  sessionId: string,
  timeoutMs: number,
): Promise<{ bytes: number; sha: string }> {
  const deadline = Date.now() + timeoutMs;
  let lastSeen = "";
  while (Date.now() < deadline) {
    const text = await readTerminalBuffer(sessionId);
    lastSeen = text;
    // The shim prints one short, unwrapped line — reliably rendered even
    // though the pasted payload itself is collapsed by bracketed-paste mode.
    const m = text.match(/RT_PASTE_RESULT bytes=(\d+) sha=([0-9a-f]{64})/);
    if (m && m[1] !== undefined && m[2] !== undefined) {
      return { bytes: Number(m[1]), sha: m[2] };
    }
    await delay(250);
  }
  throw new Error(
    `xterm buffer for ${sessionId} never reported RT_PASTE_RESULT within ` +
      `${timeoutMs}ms.\nLast seen:\n${lastSeen.slice(-1_000)}`,
  );
}

async function readTerminalBuffer(sessionId: string): Promise<string> {
  return (await browser.execute(
    `
    const w = window;
    const term = w.__rt_terms && w.__rt_terms.get(${JSON.stringify(sessionId)});
    if (!term) return "";
    const buf = term.buffer.active;
    const lines = [];
    for (let i = 0; i < buf.length; i++) {
      const line = buf.getLine(i);
      if (line) lines.push(line.translateToString(true));
    }
    return lines.join("\\n");
    `,
  )) as unknown as string;
}

function isRepos(
  msg: DaemonMessage,
): msg is DaemonMessage & { type: "repos"; repos: RepoEntry[] } {
  return msg.type === "repos";
}

function runGit(cwd: string, args: string[]): void {
  execFileSync("git", args, { cwd, stdio: "ignore" });
}
