import { execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

import { browser } from "@wdio/globals";
import { expect } from "chai";

import { DaemonWsClient } from "../../../src/ws-client.js";
import {
  buildSpawnMessage,
  dismissLayoutChooser,
} from "../../../src/session-helpers.js";
import { waitForBranchSuggestion } from "../../../src/spawn-dialog.js";
import type {
  DaemonMessage,
  RepoEntry,
  SessionSnapshot,
  SuggestTarget,
} from "../../../src/types.js";

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;
const repoRoot = resolve(
  fileURLToPath(new URL("../../../../..", import.meta.url)),
);

/**
 * The word pool the daemon draws session branch names from. Mirrors
 * `ADJECTIVES` / `NOUNS` in `crates/daemon/src/branch_names.rs` — the point of
 * these tests is to fill the pool, so the copy has to stay in step with it. A
 * drift shows up immediately: `seedPoolBranches` would leave more than one
 * name free and the "only one name is left" assertions would fail.
 */
const ADJECTIVES = [
  "sleepy",
  "brave",
  "calm",
  "eager",
  "gentle",
  "happy",
  "jolly",
  "kind",
  "lively",
  "mighty",
  "nimble",
  "polite",
  "quick",
  "silly",
  "witty",
  "zen",
];

const NOUNS = [
  "otter",
  "panda",
  "lynx",
  "fox",
  "hawk",
  "raven",
  "tiger",
  "whale",
  "koala",
  "ferret",
  "badger",
  "marten",
  "lemur",
  "weasel",
  "gibbon",
  "gecko",
];

/** The one pool name left unclaimed in the "pool" fixture repo. */
const FREE_NAME = "wt/zen-gecko";

/** Branch a leftover is staged on for the `refuse_leftover` test. */
const LEFTOVER_BRANCH = "rt-e2e-refuse-leftover";

/** Branch the launch-last seed session runs on. */
const LAUNCH_LAST_SEED_BRANCH = "rt-e2e-launch-last-seed";

/**
 * The daemon — not the client — names a session's worktree branch, because
 * only it can check a candidate against every member repo's refs and against
 * the worktree directories already on disk. These specs cover the three things
 * that buys: a suggestion never lands on an existing branch, an exhausted pool
 * degrades to a suffix instead of colliding, and the spawns that were never
 * shown a collision prompt (`refuse_leftover`) stop rather than silently
 * attach a leftover branch at its stale tip.
 */
describe("daemon-picked worktree branch names", function () {
  this.timeout(300_000);

  let ws: DaemonWsClient | null = null;
  /** Repo whose branch pool is (nearly) exhausted — suggestion tests only. */
  let poolRepo: string | null = null;
  let poolRepoId: string | null = null;
  /** Repo the spawning tests use, kept out of the exhausted pool so the names
   *  it is handed still look like `wt/<adjective>-<noun>`. */
  let spawnRepo: string | null = null;
  let spawnRepoId: string | null = null;
  const spawnedSessionIds = new Set<string>();
  const worktreePaths: string[] = [];

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
    await dismissLayoutChooser(APP_BOOT_TIMEOUT);
    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    const parent = join(repoRoot, ".tmp", "e2e");
    await mkdir(parent, { recursive: true });

    poolRepo = await mkdtemp(join(parent, "rt-e2e-branch-pool-"));
    await initRepo(poolRepo, "branch pool fixture\n");
    seedPoolBranches(poolRepo, FREE_NAME);

    spawnRepo = await mkdtemp(join(parent, "rt-e2e-branch-spawn-"));
    await initRepo(spawnRepo, "branch suggestion spawn fixture\n");

    poolRepoId = await addRepo(ws, poolRepo, "rt-e2e-branch-pool");
    spawnRepoId = await addRepo(ws, spawnRepo, "rt-e2e-branch-spawn");
  });

  afterEach(async function () {
    const dialog = await browser.$('[data-testid="spawn-dialog"]');
    if (await dialog.isExisting()) {
      const close = await browser.$('[data-testid="spawn-close"]');
      await close.click().catch(() => undefined);
      await dialog
        .waitForExist({ timeout: 5_000, reverse: true })
        .catch(() => undefined);
    }
  });

  after(async function () {
    if (ws) {
      for (const sessionId of spawnedSessionIds) {
        try {
          ws.send({ type: "stop_session", session_id: sessionId, cleanup: [] });
          await delay(400);
          ws.send({
            type: "discard_session",
            session_id: sessionId,
            cleanup: spawnRepoId
              ? [
                  {
                    repo_id: spawnRepoId,
                    remove_worktree: true,
                    branch: "delete",
                  },
                ]
              : [],
          });
          await delay(400);
        } catch {
          /* best-effort cleanup */
        }
      }
      for (const id of [poolRepoId, spawnRepoId]) {
        if (!id) continue;
        try {
          ws.send({ type: "remove_repo", repo_id: id });
          await delay(200);
        } catch {
          /* best-effort cleanup */
        }
      }
      await ws.close();
    }
    if (spawnRepo) {
      // The refusal test deliberately leaves this branch behind.
      try {
        runGit(spawnRepo, ["branch", "-D", LEFTOVER_BRANCH]);
      } catch {
        /* already gone */
      }
      for (const worktreePath of worktreePaths) {
        try {
          runGit(spawnRepo, ["worktree", "remove", "--force", worktreePath]);
        } catch {
          /* already gone */
        }
        await rm(worktreePath, {
          recursive: true,
          force: true,
          maxRetries: 20,
          retryDelay: 100,
        });
      }
    }
    for (const path of [poolRepo, spawnRepo]) {
      if (!path) continue;
      await rm(path, {
        recursive: true,
        force: true,
        maxRetries: 20,
        retryDelay: 100,
      });
    }
  });

  it("suggests the one name no existing branch has taken", async function () {
    if (!ws || !poolRepoId) throw new Error("setup failed");

    const dialog = await openSpawnDialogFor(poolRepoId);
    await ensureWorktreeMode(dialog);
    await waitForBranchSuggestion(dialog);

    const branch = await dialog.$('[data-testid="spawn-single-branch"]');
    expect(
      await branch.getValue(),
      "the only free name in a 255-of-256 pool",
    ).to.equal(FREE_NAME);

    // The dice asks for another draw. With one name free that draw can only
    // land on the same name — what it must never do is hand back one of the
    // 255 branches that already exist.
    const dice = await dialog.$(".branch-dice");
    await dice.waitForExist({ timeout: 5_000 });
    await dice.click();
    await waitForBranchSuggestion(dialog);
    await browser.waitUntil(async () => (await branch.getValue()) === FREE_NAME, {
      timeout: 15_000,
      timeoutMsg: `dice re-roll settled on "${await branch.getValue()}", expected ${FREE_NAME}`,
    });

    // The same question asked straight over the wire, so the answer is the
    // daemon's and not the dialog's cache.
    const name = await requestSuggestion(ws, poolRepoId);
    expect(name, "the wire reply agrees with the dialog").to.equal(FREE_NAME);
  });

  it("falls back to a numeric suffix once the pool is exhausted", async function () {
    if (!ws || !poolRepo || !poolRepoId) throw new Error("setup failed");

    // Claim the last free combination: every `wt/<adjective>-<noun>` is now a
    // branch in this repo.
    runGit(poolRepo, ["branch", FREE_NAME]);

    const name = await requestSuggestion(ws, poolRepoId);
    expect(name, "an exhausted pool suffixes rather than colliding").to.match(
      /^wt\/[a-z]+-[a-z]+-2$/,
    );
    expect(
      gitOutput(poolRepo, ["branch", "--list", name]),
      "the suffixed name is still free",
    ).to.equal("");
  });

  it("refuses a branch-only leftover instead of attaching it", async function () {
    if (!ws || !spawnRepo || !spawnRepoId) throw new Error("setup failed");

    // Stage the leftover exactly the way a discarded session leaves one: a
    // worktree session that committed work, then a discard that removed the
    // worktree and kept the branch.
    const seed = await spawnPlainShell(ws, spawnRepoId, {
      branchName: LEFTOVER_BRANCH,
      label: "refuse-leftover-seed",
      useWorktree: true,
    });
    spawnedSessionIds.add(seed.id);
    const worktreePath = seed.members[0]?.worktree_path;
    if (!worktreePath) throw new Error("seed session reported no worktree");

    await writeFile(join(worktreePath, "leftover.txt"), "work in progress\n");
    runGit(worktreePath, ["add", "leftover.txt"]);
    runGit(worktreePath, ["commit", "-m", "leftover session work"]);

    ws.send({ type: "stop_session", session_id: seed.id, cleanup: [] });
    await delay(500);
    const removed = waitForSessionRemoved(ws, seed.id);
    ws.send({
      type: "discard_session",
      session_id: seed.id,
      cleanup: [
        { repo_id: spawnRepoId, remove_worktree: true, branch: "keep" },
      ],
    });
    await removed;
    spawnedSessionIds.delete(seed.id);

    expect(
      gitOutput(spawnRepo, ["branch", "--list", LEFTOVER_BRANCH]),
      "the discard kept the branch",
    ).to.not.equal("");
    expect(
      existsSync(worktreePath),
      "the discard removed the worktree directory",
    ).to.equal(false);
    const tip = gitOutput(spawnRepo, ["rev-parse", "--short", LEFTOVER_BRANCH]);

    // Move the base on by one commit. Staleness is the whole reason the spawn
    // is refused, and a leftover cut from an unmoved base trails it by zero —
    // which would prove nothing about the number being reported at all.
    await writeFile(join(spawnRepo, "moved-on.txt"), "base moved on\n");
    runGit(spawnRepo, ["add", "moved-on.txt"]);
    runGit(spawnRepo, ["commit", "-m", "advance main past the leftover"]);

    // Anything the refused spawn creates would be a session; watch for one.
    const strayUpdates: string[] = [];
    const known = new Set(spawnedSessionIds);
    const unsubscribe = ws.onMessage((msg) => {
      if (!isSessionUpdated(msg)) return;
      if (!known.has(msg.session.id)) strayUpdates.push(msg.session.id);
    });

    const failed = ws.waitFor(isActionFailed, { timeoutMs: 20_000 });
    ws.send(
      buildSpawnMessage({
        label: "refuse-leftover-retry",
        repoId: spawnRepoId,
        branchName: LEFTOVER_BRANCH,
        useWorktree: true,
        mode: "plain_shell",
        worktreeReuse: "refuse_leftover",
      }),
    );
    const refusal = await failed;
    // A session that slipped through would announce itself a beat after the
    // refusal, not before it.
    await delay(1_000);
    unsubscribe();

    expect(refusal.title).to.equal("Leftover branch in the way");
    expect(refusal.detail).to.include(LEFTOVER_BRANCH);
    expect(refusal.detail).to.include(tip);
    expect(
      refusal.detail,
      "the refusal reports how far the leftover trails the base",
    ).to.include("1 commit behind main");
    expect(
      existsSync(worktreePath),
      "the refused spawn created no worktree",
    ).to.equal(false);
    expect(strayUpdates, "the refused spawn created no session").to.deep.equal(
      [],
    );
  });

  it("launches last again on a name the daemon picked", async function () {
    if (!ws || !spawnRepoId) throw new Error("setup failed");

    const seed = await spawnPlainShell(ws, spawnRepoId, {
      branchName: LAUNCH_LAST_SEED_BRANCH,
      label: "launch-last-seed",
      useWorktree: true,
    });
    spawnedSessionIds.add(seed.id);
    if (seed.members[0]?.worktree_path) {
      worktreePaths.push(seed.members[0].worktree_path);
    }

    const repoRow = await browser.$(
      `[data-testid="sidebar-container-repo"][data-container-id="${spawnRepoId}"]`,
    );
    await repoRow.waitForExist({ timeout: 15_000 });
    const quickLaunch = await repoRow.$(
      '[data-testid="container-launch-last-quick"]',
    );
    await quickLaunch.waitForExist({ timeout: 15_000 });

    const known = new Set(spawnedSessionIds);
    const replayPromise = ws.waitFor(
      (msg): msg is SessionUpdatedMessage =>
        isSessionUpdated(msg) &&
        !known.has(msg.session.id) &&
        msg.session.members.some((m) => m.repo_id === spawnRepoId),
      { timeoutMs: 30_000 },
    );
    await quickLaunch.click();
    const replayed = (await replayPromise).session;
    spawnedSessionIds.add(replayed.id);
    if (replayed.members[0]?.worktree_path) {
      worktreePaths.push(replayed.members[0].worktree_path);
    }

    const branch = replayed.members[0]?.branch;
    expect(branch, "the replay ran on a pool name from the daemon").to.match(
      /^wt\/[a-z]+-[a-z]+$/,
    );
    expect(
      branch,
      "the replay does not reuse the saved session's branch",
    ).to.not.equal(LAUNCH_LAST_SEED_BRANCH);
  });
});

// ---------- daemon round trips ----------

type BranchNameSuggestionMessage = DaemonMessage & {
  type: "branch_name_suggestion";
  target: SuggestTarget;
  name: string;
};

type ActionFailedMessage = DaemonMessage & {
  type: "action_failed";
  title: string;
  detail: string;
};

type SessionUpdatedMessage = DaemonMessage & {
  type: "session_updated";
  session: SessionSnapshot;
};

/** Ask the daemon to name a branch for `repoId` and return its answer. */
async function requestSuggestion(
  ws: DaemonWsClient,
  repoId: string,
): Promise<string> {
  const reply = ws.waitFor(
    (msg): msg is BranchNameSuggestionMessage =>
      isBranchNameSuggestion(msg) &&
      msg.target.kind === "repo" &&
      msg.target.repo_id === repoId,
    { timeoutMs: 20_000 },
  );
  ws.send({
    type: "suggest_branch_name",
    target: { kind: "repo", repo_id: repoId },
  });
  return (await reply).name;
}

function isBranchNameSuggestion(
  msg: DaemonMessage,
): msg is BranchNameSuggestionMessage {
  if (msg.type !== "branch_name_suggestion") return false;
  const target = (msg as { target?: { kind?: unknown } }).target;
  return typeof target === "object" && target !== null;
}

function isActionFailed(msg: DaemonMessage): msg is ActionFailedMessage {
  return msg.type === "action_failed";
}

function isSessionUpdated(msg: DaemonMessage): msg is SessionUpdatedMessage {
  if (msg.type !== "session_updated") return false;
  const session = (msg as { session?: unknown }).session;
  return typeof session === "object" && session !== null;
}

function waitForSessionRemoved(
  ws: DaemonWsClient,
  sessionId: string,
): Promise<DaemonMessage & { type: "session_removed"; session_id: string }> {
  return ws.waitFor(
    (
      msg,
    ): msg is DaemonMessage & {
      type: "session_removed";
      session_id: string;
    } => msg.type === "session_removed" && msg.session_id === sessionId,
    { timeoutMs: 30_000 },
  );
}

async function spawnPlainShell(
  ws: DaemonWsClient,
  repoId: string,
  options: { branchName: string; label: string; useWorktree: boolean },
): Promise<SessionSnapshot> {
  const spawned = ws.waitFor(
    (msg): msg is SessionUpdatedMessage =>
      isSessionUpdated(msg) &&
      msg.session.label === options.label &&
      msg.session.mode === "plain_shell",
    { timeoutMs: 30_000 },
  );
  ws.send(
    buildSpawnMessage({
      label: options.label,
      repoId,
      branchName: options.branchName,
      useWorktree: options.useWorktree,
      mode: "plain_shell",
    }),
  );
  return (await spawned).session;
}

// ---------- UI ----------

async function openSpawnDialogFor(repoId: string) {
  const spawnButton = await browser.$('[data-testid="sidebar-add-session"]');
  await spawnButton.waitForExist({ timeout: 10_000 });
  await browser.waitUntil(async () => spawnButton.isEnabled(), {
    timeout: 10_000,
    timeoutMsg: "spawn button did not become enabled",
  });
  await spawnButton.click();
  const dialog = await browser.$('[data-testid="spawn-dialog"]');
  await dialog.waitForExist({ timeout: 10_000 });

  const target = await dialog.$('[data-testid="spawn-target-select"]');
  await target.waitForExist({ timeout: 10_000 });
  await target.selectByAttribute("value", `repo:${repoId}`);
  return dialog;
}

async function ensureWorktreeMode(
  dialog: Awaited<ReturnType<typeof browser.$>>,
): Promise<void> {
  const worktree = await dialog.$('[data-testid="spawn-single-worktree"]');
  await worktree.waitForExist({ timeout: 10_000 });
  if (!(await worktree.isSelected())) await worktree.click();
  await browser.waitUntil(async () => worktree.isSelected(), {
    timeout: 5_000,
    timeoutMsg: "worktree toggle did not turn on",
  });
}

// ---------- fixtures ----------

async function initRepo(path: string, readme: string): Promise<void> {
  await writeFile(join(path, "README.md"), readme);
  runGit(path, ["init", "-b", "main"]);
  runGit(path, ["config", "user.email", "e2e@rustling-tulip.test"]);
  runGit(path, ["config", "user.name", "rt-e2e"]);
  runGit(path, ["config", "commit.gpgsign", "false"]);
  runGit(path, ["add", "README.md"]);
  runGit(path, ["commit", "-m", "initial fixture commit"]);
}

/**
 * Create a branch for every pool combination except `free`.
 *
 * One `git update-ref --stdin` rather than 255 `git branch` calls: process
 * spawns dominate this on Windows, and the refs are identical either way.
 */
function seedPoolBranches(repo: string, free: string): void {
  const updates = ADJECTIVES.flatMap((adjective) =>
    NOUNS.map((noun) => `wt/${adjective}-${noun}`),
  )
    .filter((name) => name !== free)
    .map((name) => `create refs/heads/${name} HEAD\n`)
    .join("");
  execFileSync("git", ["-C", repo, "update-ref", "--stdin"], {
    input: updates,
    stdio: ["pipe", "ignore", "pipe"],
  });
}

async function addRepo(
  ws: DaemonWsClient,
  path: string,
  name: string,
): Promise<string> {
  const reposPromise = ws.waitFor(isRepos, { timeoutMs: 10_000 });
  ws.send({ type: "add_repo", path, name });
  const repos = await reposPromise;
  const added = repos.repos.find(
    (r: RepoEntry) => r.path === path || r.path === path.replace(/\\/g, "/"),
  );
  if (!added) throw new Error(`fixture repo was not registered: ${path}`);
  return added.id;
}

function isRepos(
  msg: DaemonMessage,
): msg is DaemonMessage & { type: "repos"; repos: RepoEntry[] } {
  return msg.type === "repos";
}

function runGit(cwd: string, args: string[]): void {
  execFileSync("git", args, { cwd, stdio: "ignore" });
}

/** `runGit`, but captures trimmed stdout — for assertions on git's answer. */
function gitOutput(cwd: string, args: string[]): string {
  return execFileSync("git", args, { cwd, encoding: "utf8" }).trim();
}
