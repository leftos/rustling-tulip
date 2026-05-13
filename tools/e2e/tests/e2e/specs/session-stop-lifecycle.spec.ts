/**
 * Session stop lifecycle coverage.
 *
 * Stop should terminate the child process while retaining the session record
 * and pane binding. Removing the stopped session is a separate discard action.
 */
import { execFileSync } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";

import { browser } from "@wdio/globals";
import { expect } from "chai";

import { DaemonWsClient } from "../../../src/ws-client.js";
import type {
  DaemonMessage,
  RepoEntry,
  SessionSnapshot,
} from "../../../src/types.js";

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;

describe("session stop lifecycle", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let registeredRepoId: string | null = null;
  let spawnedSessionId: string | null = null;

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });

    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    fixtureRepo = await mkdtemp(join(tmpdir(), "rt-e2e-stop-"));
    await writeFile(join(fixtureRepo, "README.md"), "fixture for stop e2e\n");
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);
  });

  after(async function () {
    if (ws && spawnedSessionId) {
      try {
        ws.send({ type: "discard_session", session_id: spawnedSessionId, cleanup: [] });
        await delay(300);
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

  it("leaves a stopped placeholder in the pane until discard removes it", async function () {
    if (!ws || !fixtureRepo) throw new Error("setup failed");

    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({
      type: "add_repo",
      path: fixtureRepo,
      name: "rt-e2e-stop-fixture",
    });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (r: RepoEntry) =>
        r.path === fixtureRepo || r.path === fixtureRepo!.replace(/\\/g, "/"),
    );
    expect(fixture, "fixture repo registered").to.exist;
    registeredRepoId = fixture!.id;

    const spawnPromise = ws.waitFor(isSessionUpdated, { timeoutMs: 15_000 });
    ws.send({
      type: "spawn_session",
      label: "stop-lifecycle",
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
      agent: "claude",
      model: null,
      permission_mode: null,
      codex_sandbox: null,
      extra_env: [],
      prompt_injector: null,
    });
    const spawnMsg = await spawnPromise;
    spawnedSessionId = spawnMsg.session.id;

    const sidebarRow = await browser.$(
      `[data-testid=sidebar-session][data-session-id="${spawnedSessionId}"]`,
    );
    await sidebarRow.waitForExist({ timeout: 10_000 });
    await sidebarRow.click();

    const paneSelector = `[data-testid=session-pane][data-session-id="${spawnedSessionId}"]`;
    const pane = await browser.$(paneSelector);
    await pane.waitForExist({ timeout: 10_000 });

    const stoppedPromise = ws.waitFor(
      (m): m is DaemonMessage & { type: "session_updated"; session: SessionSnapshot } =>
        isSessionUpdated(m) &&
        m.session.id === spawnedSessionId &&
        m.session.status === "stopped",
      { timeoutMs: 10_000 },
    );
    const stopButton = await pane.$("[data-testid=session-stop]");
    await stopButton.click();
    const confirmStop = await pane.$("[data-testid=session-stop-confirm]");
    await confirmStop.waitForExist({ timeout: 2_000 });
    await confirmStop.click();
    await stoppedPromise;

    await browser.waitUntil(
      async () => {
        const stoppedPane = await browser.$(paneSelector);
        if (!(await stoppedPane.isExisting())) return false;
        return (await stoppedPane.getAttribute("data-session-status")) === "stopped";
      },
      { timeout: 10_000, timeoutMsg: "pane never switched to stopped state" },
    );
    const exited = await browser.$("[data-testid=session-exited]");
    await exited.waitForExist({ timeout: 5_000 });

    const leaf = await browser.$(
      `[data-testid=sidebar-session][data-session-id="${spawnedSessionId}"]`,
    );
    expect(await leaf.isExisting(), "session remains in sidebar").to.be.true;
    expect(await leaf.getAttribute("data-tab-binding-count")).to.not.equal("0");

    const removedPromise = ws.waitFor(
      (m): m is DaemonMessage & { type: "session_removed"; session_id: string } =>
        m.type === "session_removed" && m.session_id === spawnedSessionId,
      { timeoutMs: 10_000 },
    );
    const removePane = await browser.$("[data-testid=session-discard-worktree]");
    await removePane.click();
    await removedPromise;
    spawnedSessionId = null;

    await pane.waitForExist({ timeout: 5_000, reverse: true });
  });
});

function isRepos(
  m: DaemonMessage,
): m is DaemonMessage & { type: "repos"; repos: RepoEntry[] } {
  return m.type === "repos";
}

function isSessionUpdated(
  m: DaemonMessage,
): m is DaemonMessage & { type: "session_updated"; session: SessionSnapshot } {
  return m.type === "session_updated";
}

function runGit(cwd: string, args: string[]): void {
  execFileSync("git", args, { cwd, stdio: "ignore" });
}
