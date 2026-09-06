/**
 * Full WebView E2E spec: launches the Tauri shell under tauri-driver, opens
 * a side-channel WS to the daemon to bypass the native "add repo" dialog,
 * spawns a session against fake-claude, and asserts the banner appears in
 * the xterm scrollback (read through the dev-time `window.__rt_terms` map
 * the Terminal component publishes).
 *
 * Run via the wdio config: `pnpm test:wdio`. The plain Mocha runner driven
 * by `pnpm test` only runs the daemon smoke (it's faster and doesn't need
 * a tauri-driver binary on PATH). Both specs live side-by-side because the
 * daemon spec is the first line of defense and the WebView spec is the
 * full end-to-end check.
 */
import { execFileSync } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
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

describe("rustling-tulip webview", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let registeredRepoId: string | null = null;
  let spawnedSessionId: string | null = null;

  before(async function () {
    // Wait for the React app to mount so we know the Tauri shell + the
    // daemon supervisor have completed bootstrap.
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });

    // A fresh e2e client always gets the mandatory first-connect LayoutChooser;
    // dismiss it so its backdrop doesn't intercept later session-pane clicks.
    await dismissLayoutChooser(APP_BOOT_TIMEOUT);

    // The supervisor should have written daemon.json by now (or be about
    // to). The handshake-poll inside DaemonWsClient.open handles the wait.
    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    // Init a tiny fixture repo. Done at runtime so we don't ship a
    // ceremonial empty .git/ in the source tree.
    fixtureRepo = await mkdtemp(join(tmpdir(), "rt-e2e-"));
    await writeFile(join(fixtureRepo, "README.md"), "fixture for e2e\n");
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
        /* best-effort */
      }
    }
    if (ws && registeredRepoId) {
      try {
        ws.send({ type: "remove_repo", repo_id: registeredRepoId });
        await delay(200);
      } catch {
        /* best-effort */
      }
    }
    if (ws) await ws.close();
    if (fixtureRepo) await rm(fixtureRepo, { recursive: true, force: true });
  });

  it("registers a repo, spawns a session, and surfaces output in the xterm buffer", async function () {
    expect(ws, "ws").to.not.equal(null);
    expect(fixtureRepo, "fixtureRepo").to.not.equal(null);
    if (!ws || !fixtureRepo) throw new Error("setup failed");
    const fixturePath = fixtureRepo;

    // Register the fixture repo via the side channel and wait for the
    // broadcast `repos` snapshot to confirm the daemon accepted it.
    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({ type: "add_repo", path: fixturePath, name: "rt-e2e-fixture" });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (r: RepoEntry) =>
        r.path === fixturePath || r.path === fixturePath.replace(/\\/g, "/"),
    );
    expect(fixture, "fixture repo registered").to.not.equal(undefined);
    registeredRepoId = fixture!.id;

    // Spawn an interactive session against fake-claude.
    const session = await spawnSession(ws, {
      label: "smoke",
      repoId: registeredRepoId,
      timeoutMs: 15_000,
    });
    spawnedSessionId = session.id;

    // Open the session in the UI (dismisses the LayoutChooser if still up).
    await openSessionPane(spawnedSessionId);

    // Poll the xterm buffer until the fake-claude banner appears. xterm
    // renders to a canvas, so we read the buffer through the
    // `window.__rt_terms` global the Terminal component populates.
    const banner = await waitForBufferText(
      spawnedSessionId,
      "[fake-claude] ready",
      SESSION_OUTPUT_TIMEOUT,
    );
    expect(banner, "fake-claude banner in xterm buffer").to.include(
      "[fake-claude] ready",
    );
  });
});

function isRepos(
  m: DaemonMessage,
): m is DaemonMessage & { type: "repos"; repos: RepoEntry[] } {
  return m.type === "repos";
}

function runGit(cwd: string, args: string[]): void {
  execFileSync("git", args, { cwd, stdio: "ignore" });
}

async function waitForBufferText(
  sessionId: string,
  needle: string,
  timeoutMs: number,
): Promise<string> {
  const deadline = Date.now() + timeoutMs;
  let lastSeen = "";
  while (Date.now() < deadline) {
    // Cast through `unknown` because TS doesn't know about window.__rt_terms.
    const text = (await browser.execute(
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
    lastSeen = text;
    if (text.includes(needle)) return text;
    await delay(250);
  }
  throw new Error(
    `xterm buffer for ${sessionId} never contained "${needle}" within ${timeoutMs}ms.\n` +
      `Last seen (truncated to 500 chars):\n${lastSeen.slice(0, 500)}`,
  );
}
