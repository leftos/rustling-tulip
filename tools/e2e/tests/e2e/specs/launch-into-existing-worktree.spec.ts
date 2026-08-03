import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

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
  RootWorktreeEntry,
  SessionSnapshot,
} from "../../../src/types.js";

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;
/** Branch the first session's worktree is created for. */
const ORIGINAL_BRANCH = "wt/leftover";
/**
 * Branch name sent alongside the pin on the relaunch. Deliberately different
 * from ORIGINAL_BRANCH: deriving a path from it would produce
 * `wt.wt-decoy/…`, so if the spawn lands anywhere but the pinned directory the
 * assertions catch it.
 */
const DECOY_BRANCH = "wt/decoy";
const repoRoot = resolve(
  fileURLToPath(new URL("../../../../..", import.meta.url)),
);

/**
 * Launching a session into a worktree that outlived the session that made it.
 *
 * The setup reproduces how these appear in the wild: spawn with a worktree,
 * stop, then discard the session record *without* cleanup. The worktree stays
 * on disk with nothing referencing it — `RootWorktreeStatus::Stale` — which
 * before path-pinned targets was a dead end you could only delete.
 *
 * What's asserted, in increasing order of what it would cost to get wrong:
 * the group reports as stale and resolves to its originating repo; the modal
 * offers a Launch button for it; and a pinned spawn runs in that exact
 * directory even when the branch name sent along would derive a different one.
 */
describe("launching into an existing worktree", function () {
  this.timeout(240_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let registeredRepoId: string | null = null;
  let leftoverWorktree: string | null = null;
  const spawnedSessionIds: string[] = [];

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
    await dismissLayoutChooser(APP_BOOT_TIMEOUT);
    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    const parent = join(repoRoot, ".tmp", "e2e");
    await mkdir(parent, { recursive: true });
    fixtureRepo = await mkdtemp(join(parent, "rt-e2e-relaunch-wt-"));
    await writeFile(join(fixtureRepo, "README.md"), "relaunch fixture\n");
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
    runGit(fixtureRepo, ["config", "commit.gpgsign", "false"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);

    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({ type: "add_repo", path: fixtureRepo, name: "rt-e2e-relaunch" });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (repo) =>
        repo.path === fixtureRepo ||
        repo.path === fixtureRepo?.replace(/\\/g, "/"),
    );
    if (!fixture) throw new Error("fixture repo never registered");
    registeredRepoId = fixture.id;
  });

  after(async function () {
    if (ws) {
      for (const id of spawnedSessionIds) {
        try {
          ws.send({ type: "stop_session", session_id: id, cleanup: [] });
          await delay(200);
          ws.send({ type: "discard_session", session_id: id, cleanup: [] });
          await delay(200);
        } catch {
          /* best-effort cleanup */
        }
      }
      if (registeredRepoId) {
        try {
          ws.send({ type: "remove_repo", repo_id: registeredRepoId });
          await delay(200);
        } catch {
          /* best-effort cleanup */
        }
      }
    }
    if (fixtureRepo && leftoverWorktree) {
      try {
        runGit(fixtureRepo, ["worktree", "remove", "--force", leftoverWorktree]);
      } catch {
        /* best-effort cleanup */
      }
      await rm(leftoverWorktree, {
        recursive: true,
        force: true,
        maxRetries: 20,
        retryDelay: 100,
      });
    }
    await ws?.close();
    if (fixtureRepo) {
      await rm(fixtureRepo, {
        recursive: true,
        force: true,
        maxRetries: 20,
        retryDelay: 100,
      });
    }
  });

  it("leaves a stale group behind that still resolves to its repo", async function () {
    if (!ws || !registeredRepoId) throw new Error("setup failed");

    const session = await spawnSession(ws, {
      label: "relaunch-source",
      repoId: registeredRepoId,
      branchName: ORIGINAL_BRANCH,
      useWorktree: true,
    });
    const worktreePath = session.members[0]?.worktree_path;
    if (!worktreePath) throw new Error("session spawned without a worktree");
    leftoverWorktree = worktreePath;

    // Stop, then discard the record with no cleanup: the process and the
    // session row go away, the directory stays. This is the state a user is in
    // after clearing out the sidebar without ticking "delete worktree".
    ws.send({ type: "stop_session", session_id: session.id, cleanup: [] });
    await delay(500);
    ws.send({ type: "discard_session", session_id: session.id, cleanup: [] });
    await delay(500);

    const entry = await findGroup(ws, worktreePath);
    expect(entry.status.kind).to.equal(
      "stale",
      "a discarded session must leave its group unreferenced",
    );
    expect(entry.launch_blocked_reason).to.equal(
      null,
      `launch unexpectedly blocked: ${entry.launch_blocked_reason}`,
    );
    expect(entry.launch?.kind).to.equal("single");
    if (entry.launch?.kind === "single") {
      expect(entry.launch.repo_id).to.equal(registeredRepoId);
      expect(entry.launch.branch).to.equal(
        ORIGINAL_BRANCH,
        "branch must come from the worktree's HEAD, not the directory name",
      );
      expect(normalize(entry.launch.worktree_path)).to.equal(
        normalize(worktreePath),
      );
    }
  });

  it("offers Launch on the stale row in the worktrees manager", async function () {
    await dismissLayoutChooser();
    await (await browser.$("[data-testid=sidebar-settings-btn]")).click();
    const worktreesTab = await browser.$("[data-testid=settings-tab-worktrees]");
    await worktreesTab.waitForExist({ timeout: 10_000 });
    await worktreesTab.click();
    const openManager = await browser.$(
      "[data-testid=settings-worktrees-open-manager]",
    );
    await openManager.waitForExist({ timeout: 10_000 });
    await openManager.click();

    const modal = await browser.$("[data-testid=worktrees-manager-modal]");
    await modal.waitForExist({ timeout: 10_000 });
    const launchButtons = await browser.$$(
      "[data-testid=worktrees-manager-row-launch]",
    );
    expect(launchButtons.length).to.be.greaterThan(
      0,
      "the manager must offer a launch action per row",
    );
    // The fixture's group is stale and its repo is registered, so at least one
    // row has to be launchable — a blocked-only list would mean resolution
    // failed for every group under the test root.
    let anyEnabled = false;
    for (const button of launchButtons) {
      if (await button.isEnabled()) {
        anyEnabled = true;
        break;
      }
    }
    expect(anyEnabled).to.equal(
      true,
      "the stale fixture group's Launch button must be enabled",
    );

    await (await browser.$("[data-testid=worktrees-manager-close]")).click();
    await modal.waitForExist({ timeout: 5_000, reverse: true });
    const settings = await browser.$("[data-testid=settings-close]");
    if (await settings.isExisting()) await settings.click();
  });

  it("runs in the pinned directory instead of deriving one from the branch", async function () {
    if (!ws || !registeredRepoId || !leftoverWorktree) {
      throw new Error("setup failed");
    }
    const beforeCount = worktreeCount(fixtureRepo!);

    const session = await spawnSession(ws, {
      label: "relaunch-pinned",
      target: {
        kind: "single",
        repo_id: registeredRepoId,
        // A branch that derives a *different* directory than the pin. The pin
        // has to win, otherwise a second worktree appears at wt.wt-decoy/.
        branch_name: DECOY_BRANCH,
        base_branch: null,
        use_worktree: true,
        existing_worktree: leftoverWorktree,
      },
    });
    spawnedSessionIds.push(session.id);

    expect(normalize(session.members[0]?.worktree_path ?? "")).to.equal(
      normalize(leftoverWorktree),
      "the session must run in the pinned worktree",
    );
    expect(session.members[0]?.branch).to.equal(
      ORIGINAL_BRANCH,
      "the member must report the branch the worktree is really on",
    );
    expect(worktreeCount(fixtureRepo!)).to.equal(
      beforeCount,
      "pinning must not add a worktree",
    );
  });

  it("refuses a pin whose directory is gone", async function () {
    if (!ws || !registeredRepoId) throw new Error("setup failed");
    const missing = join(repoRoot, ".tmp", "e2e", "rt-e2e-not-a-worktree");
    // Spawn failures surface as the blocking ActionFailed modal, not the
    // generic error toast.
    const errored = ws.waitFor(
      (msg): msg is DaemonMessage & { type: "action_failed"; detail: string } =>
        msg.type === "action_failed" &&
        typeof (msg as { detail?: unknown }).detail === "string",
      { timeoutMs: 15_000 },
    );
    ws.send({
      type: "spawn_session",
      label: "relaunch-missing",
      target: {
        kind: "single",
        repo_id: registeredRepoId,
        branch_name: DECOY_BRANCH,
        base_branch: null,
        use_worktree: true,
        existing_worktree: missing,
      },
      mode: "interactive",
      initial_prompt: null,
      dangerously_skip_permissions: false,
      agent_options: { kind: "claude", permission_mode: null },
      model: null,
      extra_env: [],
      prompt_injector: null,
    });
    const err = await errored;
    expect(err.detail).to.contain(
      "pinned worktree",
      "a missing pin must name itself rather than silently recreating",
    );
  });
});

function isRepos(
  msg: DaemonMessage,
): msg is DaemonMessage & { type: "repos"; repos: RepoEntry[] } {
  return msg.type === "repos" && Array.isArray((msg as { repos?: unknown }).repos);
}

/** Ask for a root snapshot and return the group containing `memberPath`. */
async function findGroup(
  ws: DaemonWsClient,
  memberPath: string,
): Promise<RootWorktreeEntry> {
  const snapshot = ws.waitFor(
    (
      msg,
    ): msg is DaemonMessage & {
      type: "worktrees_root_snapshot";
      entries: RootWorktreeEntry[];
    } => msg.type === "worktrees_root_snapshot",
    { timeoutMs: 15_000 },
  );
  ws.send({ type: "inspect_worktrees_root" });
  const entries = (await snapshot).entries;
  const found = entries.find((entry) =>
    entry.members.some(
      (m) => normalize(m.worktree_path) === normalize(memberPath),
    ),
  );
  if (!found) {
    throw new Error(
      `no group contains ${memberPath}; saw ${entries.map((e) => e.path).join(", ")}`,
    );
  }
  return found;
}

/** Paths cross git, the daemon, and the walker with mixed separators/case. */
function normalize(p: string): string {
  const trimmed = p.replace(/[/\\]+$/, "");
  return process.platform === "win32"
    ? trimmed.toLowerCase().replace(/\\/g, "/")
    : trimmed;
}

function worktreeCount(repo: string): number {
  const out = execFileSync("git", ["-C", repo, "worktree", "list", "--porcelain"], {
    encoding: "utf8",
  });
  return out.split("\n").filter((l) => l.startsWith("worktree ")).length;
}

function runGit(cwd: string, args: string[]): void {
  execFileSync("git", args, { cwd, stdio: "pipe" });
}
