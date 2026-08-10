/**
 * Restart-in-place coverage.
 *
 * Clicking Restart on a stopped session's pane must reuse that pane: the
 * respawned clone takes over the dead session's grid slot instead of the
 * pane collapsing and the clone landing unbound in the sidebar.
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
  isSessionUpdated,
  openSessionPane,
  spawnSession,
  type SessionUpdatedMessage,
} from "../../../src/session-helpers.js";
import type { DaemonMessage, RepoEntry } from "../../../src/types.js";

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;

describe("session restart in pane", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let registeredRepoId: string | null = null;
  /** Sessions that may still be alive at teardown; stopped+discarded in after. */
  const cleanupSessionIds: string[] = [];

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
    await dismissLayoutChooser(APP_BOOT_TIMEOUT);

    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    fixtureRepo = await mkdtemp(join(tmpdir(), "rt-e2e-restart-"));
    await writeFile(join(fixtureRepo, "README.md"), "fixture for restart e2e\n");
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);

    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({
      type: "add_repo",
      path: fixtureRepo,
      name: "rt-e2e-restart-fixture",
    });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (r: RepoEntry) =>
        r.path === fixtureRepo || r.path === fixtureRepo!.replace(/\\/g, "/"),
    );
    if (!fixture) throw new Error("fixture repo was not registered");
    registeredRepoId = fixture.id;
  });

  after(async function () {
    if (ws) {
      for (const sessionId of cleanupSessionIds) {
        try {
          // Stop before discard: a still-running child with cwd inside the
          // fixture repo makes the rm below EBUSY on Windows.
          ws.send({ type: "stop_session", session_id: sessionId, cleanup: [] });
          await delay(500);
          ws.send({
            type: "discard_session",
            session_id: sessionId,
            cleanup: [],
          });
          await delay(300);
        } catch {
          /* best-effort */
        }
      }
      if (registeredRepoId) {
        try {
          ws.send({ type: "remove_repo", repo_id: registeredRepoId });
          await delay(200);
        } catch {
          /* best-effort */
        }
      }
      await ws.close();
    }
    if (fixtureRepo) {
      // Child-handle teardown can lag the stop by a moment; retry briefly.
      for (let attempt = 0; attempt < 5; attempt += 1) {
        try {
          await rm(fixtureRepo, { recursive: true, force: true });
          break;
        } catch {
          await delay(500);
        }
      }
    }
  });

  /** Spawn a labelled session, open its pane, stop it via the pane UI, and
   *  wait for the exited placeholder. Returns the stopped session's id. */
  async function spawnOpenAndStop(label: string): Promise<string> {
    if (!ws || !registeredRepoId) throw new Error("setup failed");
    const session = await spawnSession(ws, {
      label,
      repoId: registeredRepoId,
      timeoutMs: 15_000,
    });
    cleanupSessionIds.push(session.id);
    await openSessionPane(session.id);

    const stoppedPromise = ws.waitFor(
      (m): m is SessionUpdatedMessage =>
        isSessionUpdated(m) &&
        m.session.id === session.id &&
        m.session.status === "stopped",
      { timeoutMs: 10_000 },
    );
    const pane = await browser.$(
      `[data-testid=session-pane][data-session-id="${session.id}"]`,
    );
    const stopButton = await pane.$("[data-testid=session-stop]");
    await stopButton.click();
    const confirmStop = await pane.$("[data-testid=session-stop-confirm]");
    await confirmStop.waitForExist({ timeout: 2_000 });
    await confirmStop.click();
    await stoppedPromise;
    return session.id;
  }

  /** Wait for the replacement session to mount into a pane and assert the tab
   *  topology did not change and the session is tab-bound. */
  async function expectTookOverPane(
    newSessionId: string,
    expectedTabCount: number,
  ): Promise<void> {
    const pane = await browser.$(
      `[data-testid=session-pane][data-session-id="${newSessionId}"]`,
    );
    await pane.waitForExist({
      timeout: 10_000,
      timeoutMsg: "replacement session never mounted into a pane",
    });
    expect(await countTabPills(), "tab count unchanged").to.equal(
      expectedTabCount,
    );
    const leaf = await browser.$(
      `[data-testid=sidebar-session][data-session-id="${newSessionId}"]`,
    );
    await leaf.waitForExist({ timeout: 5_000 });
    expect(
      await leaf.getAttribute("data-tab-binding-count"),
      "replacement session is bound to a tab",
    ).to.not.equal("0");
  }

  it("respawns the clone into the same pane without opening a new tab", async function () {
    if (!ws) throw new Error("setup failed");
    const stoppedId = await spawnOpenAndStop("restart-in-pane");
    const tabCountBefore = await countTabPills();
    expect(tabCountBefore, "session opened into a tab").to.be.greaterThan(0);

    const restartButton = await browser.$(
      `[data-testid=session-pane][data-session-id="${stoppedId}"] [data-testid=session-restart]`,
    );
    await restartButton.waitForExist({ timeout: 5_000 });

    // Restart: expect a fresh clone session AND the old one to be removed.
    const clonePromise = ws.waitFor(
      (m): m is SessionUpdatedMessage =>
        isSessionUpdated(m) && !cleanupSessionIds.includes(m.session.id),
      { timeoutMs: 15_000 },
    );
    const removedPromise = ws.waitFor(
      (m): m is DaemonMessage & { type: "session_removed"; session_id: string } =>
        m.type === "session_removed" && m.session_id === stoppedId,
      { timeoutMs: 15_000 },
    );
    await restartButton.click();
    const clone = (await clonePromise).session;
    cleanupSessionIds.push(clone.id);
    await removedPromise;

    await expectTookOverPane(clone.id, tabCountBefore);
  });
});

function isRepos(
  m: DaemonMessage,
): m is DaemonMessage & { type: "repos"; repos: RepoEntry[] } {
  return m.type === "repos";
}

async function countTabPills(): Promise<number> {
  const pills = await browser.$$("[data-testid=tab-pill]");
  return pills.length;
}

function runGit(cwd: string, args: string[]): void {
  execFileSync("git", args, { cwd, stdio: "ignore" });
}
