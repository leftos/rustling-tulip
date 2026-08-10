/**
 * Confirmation flow for repo removal with live sessions.
 *
 * The audit's "Removing a repo silently abandons its live sessions" finding:
 * clicking × on a repo used to immediately call remove_repo, dropping any
 * running sessions into the Detached bucket without explanation. Iter 3
 * routes that path through a 3-way modal: Cancel / Remove anyway / Stop
 * sessions and remove.
 *
 * This spec drives the modal end-to-end via the React app's own daemon
 * client (exposed in dev builds as `window.__rt_daemon_client`).
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
  spawnSession,
} from "../../../src/session-helpers.js";
import type {
  DaemonMessage,
  RepoEntry,
} from "../../../src/types.js";

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;

describe("repo remove confirm modal", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let registeredRepoId: string | null = null;
  let spawnedSessionId: string | null = null;

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
    await dismissLayoutChooser(APP_BOOT_TIMEOUT);

    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    fixtureRepo = await mkdtemp(join(tmpdir(), "rt-e2e-rmconf-"));
    await writeFile(join(fixtureRepo, "README.md"), "fixture for repo-remove e2e\n");
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
          cleanup: [],
        });
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

  it("opens a 3-way modal when × clicked on a repo with live sessions, and dismisses via Cancel", async function () {
    expect(ws, "ws").to.not.equal(null);
    expect(fixtureRepo, "fixtureRepo").to.not.equal(null);
    if (!ws || !fixtureRepo) throw new Error("setup failed");
    const fixturePath = fixtureRepo;

    // Register the fixture repo and spawn a session against it. We do this
    // via the side-channel WS so the IDs are easy to read back.
    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({
      type: "add_repo",
      path: fixturePath,
      name: "rt-e2e-rmconf-fixture",
    });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (r: RepoEntry) =>
        r.path === fixturePath || r.path === fixturePath.replace(/\\/g, "/"),
    );
    expect(fixture, "fixture repo registered").to.not.equal(undefined);
    registeredRepoId = fixture!.id;

    const session = await spawnSession(ws, {
      label: "rm-confirm",
      repoId: registeredRepoId,
      timeoutMs: 15_000,
    });
    spawnedSessionId = session.id;

    // Wait for the sidebar to render this repo. We use a CSS attribute
    // selector on data-container-id rather than walking the DOM tree —
    // it's resilient to renames and re-renders.
    const repoRow = await browser.$(
      `[data-testid=sidebar-container-repo][data-container-id="${registeredRepoId}"]`,
    );
    await repoRow.waitForExist({ timeout: 10_000 });

    // Click × on the repo. With a live session, this should NOT fire the
    // inline two-state confirm — it should open the modal directly.
    const removeBtn = await repoRow.$('[data-testid=sidebar-remove-repo]');
    await removeBtn.click();

    const modal = await browser.$('[data-testid=repo-remove-dialog]');
    await modal.waitForExist({ timeout: 5_000 });

    const sessionList = await browser.$('[data-testid=repo-remove-session-list]');
    const sessionItem = await sessionList.$(
      `[data-session-id="${spawnedSessionId}"]`,
    );
    expect(await sessionItem.isExisting(), "spawned session listed in modal")
      .to.equal(true);

    // Cancel → modal closes, repo and session both still present.
    const cancelBtn = await browser.$('[data-testid=repo-remove-cancel]');
    await cancelBtn.click();

    await modal.waitForExist({ timeout: 2_000, reverse: true });

    // Repo row still present in the sidebar.
    const stillThere = await browser.$(
      `[data-testid=sidebar-container-repo][data-container-id="${registeredRepoId}"]`,
    );
    expect(await stillThere.isExisting(), "repo still in sidebar after cancel")
      .to.equal(true);
  });

  it("'Remove anyway' removes the repo but keeps the session alive (migrates to Detached)", async function () {
    if (!ws || !registeredRepoId || !spawnedSessionId) {
      throw new Error("setup failed");
    }

    // Re-open the modal.
    const repoRow = await browser.$(
      `[data-testid=sidebar-container-repo][data-container-id="${registeredRepoId}"]`,
    );
    const removeBtn = await repoRow.$('[data-testid=sidebar-remove-repo]');
    await removeBtn.click();

    const modal = await browser.$('[data-testid=repo-remove-dialog]');
    await modal.waitForExist({ timeout: 5_000 });

    // Click "Remove anyway".
    const anywayBtn = await browser.$('[data-testid=repo-remove-anyway]');
    await anywayBtn.click();

    // Modal closes, repo gone.
    await modal.waitForExist({ timeout: 2_000, reverse: true });

    await browser.waitUntil(
      async () => {
        const stillThere = await browser.$(
          `[data-testid=sidebar-container-repo][data-container-id="${registeredRepoId}"]`,
        );
        return !(await stillThere.isExisting());
      },
      { timeout: 5_000, timeoutMsg: "repo row never disappeared" },
    );

    // Session migrated to Detached. The detached bucket renders a session
    // row with the same data-session-id; assert it's still there.
    const detachedSession = await browser.$(
      `[data-testid=sidebar-session][data-session-id="${spawnedSessionId}"]`,
    );
    await detachedSession.waitForExist({ timeout: 5_000 });

    // Repo is gone from our tracking — don't try to remove it again in
    // after(). The session is still alive and will be stopped in after().
    registeredRepoId = null;
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
