import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { platform } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

import { browser } from "@wdio/globals";
import { expect } from "chai";

import { DaemonWsClient } from "../../../src/ws-client.js";
import type {
  DaemonMessage,
  RepoEntry,
  SessionSnapshot,
} from "../../../src/types.js";

type SessionUpdatedMessage = DaemonMessage & {
  type: "session_updated";
  session: SessionSnapshot;
};

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;
const SESSION_OUTPUT_TIMEOUT = 20_000;
const CTRL_KEY = "\uE009";
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
            ? [{ repo_id: registeredRepoId, remove_worktree: false }]
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

  it("pastes the full clipboard text with Ctrl+V", async function () {
    expect(ws, "ws").to.not.be.null;
    expect(fixtureRepo, "fixtureRepo").to.not.be.null;
    if (!ws || !fixtureRepo) throw new Error("setup failed");

    const repo = await registerRepo(ws, fixtureRepo);
    registeredRepoId = repo.id;
    spawnedSessionId = await spawnFakeClaude(ws, registeredRepoId);
    await openSessionPane(spawnedSessionId);
    await waitForBufferMarkers(spawnedSessionId, [
      "[fake-claude] ready",
    ], SESSION_OUTPUT_TIMEOUT);
    await enableBracketedPasteMode(spawnedSessionId);

    const payload =
      `RT_PASTE_BEGIN_${"0123456789abcdef".repeat(8192)}_RT_PASTE_END\n`;
    await setClipboardText(payload);
    await focusTerminal(spawnedSessionId);
    await pressCtrlV();

    const text = await waitForBufferMarkers(
      spawnedSessionId,
      ["RT_PASTE_BEGIN", "RT_PASTE_END"],
      SESSION_OUTPUT_TIMEOUT,
    );
    expect(text).to.include("RT_PASTE_BEGIN");
    expect(text).to.include("RT_PASTE_END");
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

async function spawnFakeClaude(
  ws: DaemonWsClient,
  repoId: string,
): Promise<string> {
  const spawnPromise = ws.waitFor(isSessionUpdated, { timeoutMs: 15_000 });
  ws.send({
    type: "spawn_session",
    label: "paste",
    target: {
      kind: "single",
      repo_id: repoId,
      branch_name: "main",
      base_branch: null,
      use_worktree: false,
    },
    mode: "interactive",
    initial_prompt: null,
    dangerously_skip_permissions: false,
    agent: "claude",
    model: null,
    permission_mode: null,
    codex_sandbox: null,
    extra_env: [],
    prompt_injector: null,
  });
  const msg = await spawnPromise;
  return msg.session.id;
}

async function openSessionPane(sessionId: string): Promise<void> {
  const sidebarRow = await browser.$(
    `[data-testid=sidebar-session][data-session-id="${sessionId}"]`,
  );
  await sidebarRow.waitForExist({ timeout: 10_000 });
  await sidebarRow.click();

  const pane = await browser.$(
    `[data-testid=session-pane][data-session-id="${sessionId}"]`,
  );
  await pane.waitForExist({ timeout: 10_000 });
}

async function setClipboardText(text: string): Promise<void> {
  const browserWriteWorked = (await browser.execute(
    `
    return (async () => {
      try {
        await globalThis.navigator.clipboard.writeText(${JSON.stringify(text)});
        return true;
      } catch {
        return false;
      }
    })();
    `,
  )) as unknown as boolean;
  if (browserWriteWorked) return;

  if (platform() !== "win32") {
    throw new Error("navigator clipboard write failed");
  }
  execFileSync(
    "powershell.exe",
    [
      "-NoProfile",
      "-NonInteractive",
      "-Command",
      "$text = [Console]::In.ReadToEnd(); Set-Clipboard -Value $text",
    ],
    { input: text, encoding: "utf8" },
  );
}

async function focusTerminal(sessionId: string): Promise<void> {
  await browser.execute(
    `
    const term = globalThis.__rt_terms && globalThis.__rt_terms.get(${JSON.stringify(sessionId)});
    if (!term) {
      throw new Error("terminal not found");
    }
    term.focus();
    `,
  );
}

async function enableBracketedPasteMode(sessionId: string): Promise<void> {
  await browser.execute(
    `
    const term = globalThis.__rt_terms && globalThis.__rt_terms.get(${JSON.stringify(sessionId)});
    if (!term) {
      throw new Error("terminal not found");
    }
    term.write("\x1b[?2004h");
    `,
  );
}

async function pressCtrlV(): Promise<void> {
  await browser.performActions([
    {
      type: "key",
      id: "keyboard",
      actions: [
        { type: "keyDown", value: CTRL_KEY },
        { type: "keyDown", value: "v" },
        { type: "keyUp", value: "v" },
        { type: "keyUp", value: CTRL_KEY },
      ],
    },
  ]);
  await browser.releaseActions();
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

function isSessionUpdated(msg: DaemonMessage): msg is SessionUpdatedMessage {
  if (msg.type !== "session_updated") return false;
  const session = (msg as { session?: unknown }).session;
  return typeof session === "object" && session !== null;
}

function runGit(cwd: string, args: string[]): void {
  execFileSync("git", args, { cwd, stdio: "ignore" });
}
