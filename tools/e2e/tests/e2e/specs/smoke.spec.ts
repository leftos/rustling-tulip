/**
 * Smoke spec: launches the real Tauri app under tauri-driver, opens a
 * side-channel WS to the daemon, registers a temp git repo, spawns an
 * interactive session against fake-claude, and asserts the fake-claude
 * banner appears in the xterm buffer.
 *
 * Cleans up after itself: stops the spawned session and removes the temp
 * repo from the daemon's state, then deletes the temp directory.
 *
 * Run with `pnpm test`.
 */
import { execFileSync } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";

import { expect } from "chai";

import { browser } from "@wdio/globals";

import { DaemonWsClient } from "../../../src/ws-client.js";
import type {
  DaemonMessage,
  RepoEntry,
  SessionSnapshot,
} from "../../../src/types.js";

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;
const SESSION_OUTPUT_TIMEOUT = 20_000;

describe("rustling-tulip smoke", function () {
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

    // The supervisor should have written daemon.json by now (or be about to).
    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    // Init a tiny fixture repo. Done at runtime so we don't ship a ceremonial
    // empty .git/ in the source tree.
    fixtureRepo = await mkdtemp(join(tmpdir(), "rt-e2e-"));
    await writeFile(join(fixtureRepo, "README.md"), "fixture repo for e2e\n");
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
            ? [{ repo_id: registeredRepoId, remove_worktree: true }]
            : [],
        });
        await delay(500);
      } catch {
        // best-effort
      }
    }
    if (ws && registeredRepoId) {
      try {
        ws.send({ type: "remove_repo", repo_id: registeredRepoId });
        await delay(200);
      } catch {
        // best-effort
      }
    }
    if (ws) await ws.close();
    if (fixtureRepo) await rm(fixtureRepo, { recursive: true, force: true });
  });

  it("connects, registers a repo, and spawns a session whose output reaches the xterm buffer", async function () {
    expect(ws, "ws").to.not.be.null;
    expect(fixtureRepo, "fixtureRepo").to.not.be.null;
    if (!ws || !fixtureRepo) throw new Error("setup failed");

    // Register the fixture repo via the side channel and wait for the
    // broadcast `repos` snapshot to confirm the daemon accepted it.
    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({ type: "add_repo", path: fixtureRepo, name: "rt-e2e-fixture" });
    const repos = await reposPromise;
    const fixturePath = fixtureRepo;
    const fixture = repos.repos.find(
      (r: RepoEntry) =>
        r.path === fixturePath || r.path === fixturePath.replace(/\\/g, "/"),
    );
    expect(fixture, "fixture repo registered").to.exist;
    registeredRepoId = fixture!.id;

    // Spawn an interactive session. use_worktree=false so the daemon runs
    // claude (the fake) in the repo's primary dir — no worktree gymnastics.
    const spawnPromise = ws.waitFor(
      (m): m is DaemonMessage & { type: "session_updated"; session: SessionSnapshot } =>
        m.type === "session_updated",
      { timeoutMs: 15_000 },
    );
    ws.send({
      type: "spawn_session",
      label: "smoke",
      target: {
        kind: "single",
        repo_id: registeredRepoId,
        branch_name: "main",
        base_branch: null,
        use_worktree: false,
      },
      mode: "interactive",
      initial_prompt: null,
      dangerously_skip_permissions: false,
      model: null,
      permission_mode: null,
      extra_env: [],
      prompt_injector: null,
    });
    const spawnMsg = await spawnPromise;
    spawnedSessionId = spawnMsg.session.id;

    // Open the session in the UI by clicking its sidebar entry.
    const sidebarSelector = `[data-testid=sidebar-session][data-session-id="${spawnedSessionId}"]`;
    const sidebarRow = await browser.$(sidebarSelector);
    await sidebarRow.waitForExist({ timeout: 10_000 });
    await sidebarRow.click();

    // Wait for the session pane (and its xterm) to mount.
    const pane = await browser.$(
      `[data-testid=session-pane][data-session-id="${spawnedSessionId}"]`,
    );
    await pane.waitForExist({ timeout: 10_000 });

    // Poll the xterm buffer until the fake-claude banner appears. xterm
    // renders to a canvas, so we read the buffer through the dev-only
    // `__rt_terms` global the React Terminal component populates.
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
    // Read every line of the xterm buffer for `sessionId` and concatenate.
    // Cast through `unknown` because TS doesn't know about window.__rt_terms
    // (set on the renderer side in dev mode).
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
