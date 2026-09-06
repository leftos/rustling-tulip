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
  isSessionUpdated,
} from "../../../src/session-helpers.js";
import type { SessionUpdatedMessage } from "../../../src/session-helpers.js";
import type {
  DaemonMessage,
  RepoEntry,
  SessionSnapshot,
} from "../../../src/types.js";

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;
const repoRoot = resolve(
  fileURLToPath(new URL("../../../../..", import.meta.url)),
);

/** Branch the duplicated worktree session runs on. */
const SOURCE_BRANCH = "rt-e2e-dup-source";
/** Branch the leftover-staging test duplicates from. */
const SECOND_SOURCE_BRANCH = "rt-e2e-dup-source2";
/** Branch a branch-only leftover is staged on. */
const LEFTOVER_BRANCH = "rt-e2e-dup-leftover";
/** The `wt/<adjective>-<noun>` shape `branch_names::suggest` hands out. */
const POOL_NAME = /^wt\/[a-z]+-[a-z]+$/;

/**
 * A duplicate is "another session like this one", not "another process in the
 * same checkout": the daemon names a fresh branch for a worktree clone so it
 * gets its own directory instead of competing for the one the source holds,
 * and refuses any leftover under that name. An in-place session has no
 * worktree to compete for, so its duplicate keeps replaying the same branch.
 */
describe("duplicating a session", function () {
  this.timeout(300_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let repoId: string | null = null;
  /** Every session this spec spawned, and whether it owns a worktree — the
   *  discard cleanup only asks for worktree removal where there is one. */
  const spawned = new Map<string, { worktree: boolean }>();
  /** Every `action_failed` the daemon sent, in arrival order. */
  const actionFailures: ActionFailedMessage[] = [];

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
    await dismissLayoutChooser(APP_BOOT_TIMEOUT);
    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });
    ws.onMessage((msg) => {
      if (isActionFailed(msg)) actionFailures.push(msg);
    });

    const parent = join(repoRoot, ".tmp", "e2e");
    await mkdir(parent, { recursive: true });
    fixtureRepo = await mkdtemp(join(parent, "rt-e2e-session-duplicate-"));
    await initRepo(fixtureRepo, "session duplicate fixture\n");
    repoId = await addRepo(ws, fixtureRepo, "rt-e2e-session-duplicate");
  });

  after(async function () {
    if (ws) {
      for (const [sessionId, { worktree }] of spawned) {
        try {
          ws.send({ type: "stop_session", session_id: sessionId, cleanup: [] });
          await delay(400);
          ws.send({
            type: "discard_session",
            session_id: sessionId,
            cleanup:
              repoId && worktree
                ? [
                    {
                      repo_id: repoId,
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
      if (repoId) {
        try {
          ws.send({ type: "remove_repo", repo_id: repoId });
          await delay(200);
        } catch {
          /* best-effort cleanup */
        }
      }
      await ws.close();
    }
    if (fixtureRepo) {
      // The leftover test deliberately leaves this branch behind.
      try {
        runGit(fixtureRepo, ["branch", "-D", LEFTOVER_BRANCH]);
      } catch {
        /* already gone */
      }
      try {
        runGit(fixtureRepo, ["worktree", "prune"]);
      } catch {
        /* best-effort cleanup */
      }
      await rm(fixtureRepo, {
        recursive: true,
        force: true,
        maxRetries: 20,
        retryDelay: 100,
      });
    }
  });

  it("gives a worktree duplicate its own fresh branch", async function () {
    if (!ws || !repoId) throw new Error("setup failed");

    const source = await spawnPlainShell(ws, repoId, {
      branchName: SOURCE_BRANCH,
      label: "dup-worktree-source",
      useWorktree: true,
    });
    remember(spawned, source, true);
    const sourceWorktree = source.members[0]?.worktree_path;
    if (!sourceWorktree) throw new Error("source session reported no worktree");

    const clone = await duplicate(ws, source, spawned, actionFailures);
    remember(spawned, clone, true);

    const branch = clone.members[0]?.branch;
    expect(branch, "the clone ran on a pool name from the daemon").to.match(
      POOL_NAME,
    );
    expect(branch, "the clone does not replay the source branch").to.not.equal(
      SOURCE_BRANCH,
    );

    const cloneWorktree = clone.members[0]?.worktree_path;
    expect(cloneWorktree, "the clone reported a worktree").to.be.a("string");
    expect(
      cloneWorktree,
      "the clone got a directory of its own",
    ).to.not.equal(sourceWorktree);
    expect(
      existsSync(cloneWorktree ?? ""),
      "the clone's worktree exists on disk",
    ).to.equal(true);

    const sessions = await listSessions(ws);
    const sourceNow = sessions.find((s) => s.id === source.id);
    expect(sourceNow, "the source session is still registered").to.not.equal(
      undefined,
    );
    expect(
      sourceNow?.status,
      "duplicating leaves the source running",
    ).to.not.equal("stopped");
  });

  it("keeps the branch when the source runs in place", async function () {
    if (!ws || !repoId) throw new Error("setup failed");

    const source = await spawnPlainShell(ws, repoId, {
      branchName: "main",
      label: "dup-in-place-source",
      useWorktree: false,
    });
    remember(spawned, source, false);

    const clone = await duplicate(ws, source, spawned, actionFailures);
    remember(spawned, clone, false);

    expect(
      clone.members[0]?.branch,
      "an in-place duplicate stays on the source's branch",
    ).to.equal("main");
    expect(
      clone.members[0]?.worktree_path,
      "an in-place duplicate runs in the same directory",
    ).to.equal(source.members[0]?.worktree_path);
  });

  it("is not derailed by a leftover branch from a discarded session", async function () {
    if (!ws || !repoId || !fixtureRepo) throw new Error("setup failed");

    // Stage the leftover the way a discarded session leaves one: a worktree
    // session that committed work, then a discard that removed the worktree
    // and kept the branch.
    const seed = await spawnPlainShell(ws, repoId, {
      branchName: LEFTOVER_BRANCH,
      label: "dup-leftover-seed",
      useWorktree: true,
    });
    remember(spawned, seed, true);
    const seedWorktree = seed.members[0]?.worktree_path;
    if (!seedWorktree) throw new Error("seed session reported no worktree");

    await writeFile(join(seedWorktree, "leftover.txt"), "work in progress\n");
    runGit(seedWorktree, ["add", "leftover.txt"]);
    runGit(seedWorktree, ["commit", "-m", "leftover session work"]);

    ws.send({ type: "stop_session", session_id: seed.id, cleanup: [] });
    await delay(500);
    const removed = waitForSessionRemoved(ws, seed.id);
    ws.send({
      type: "discard_session",
      session_id: seed.id,
      cleanup: [{ repo_id: repoId, remove_worktree: true, branch: "keep" }],
    });
    await removed;
    spawned.delete(seed.id);

    expect(
      gitOutput(fixtureRepo, ["branch", "--list", LEFTOVER_BRANCH]),
      "the discard kept the branch",
    ).to.not.equal("");
    expect(
      existsSync(seedWorktree),
      "the discard removed the worktree directory",
    ).to.equal(false);

    const source = await spawnPlainShell(ws, repoId, {
      branchName: SECOND_SOURCE_BRANCH,
      label: "dup-leftover-source",
      useWorktree: true,
    });
    remember(spawned, source, true);

    const failuresBefore = actionFailures.length;
    const clone = await duplicate(ws, source, spawned, actionFailures);
    remember(spawned, clone, true);
    // A refusal would land a beat after the spawn, not before it.
    await delay(1_000);

    const branch = clone.members[0]?.branch;
    expect(branch, "the clone ran on a pool name from the daemon").to.match(
      POOL_NAME,
    );
    expect(branch, "the clone did not land on the leftover").to.not.equal(
      LEFTOVER_BRANCH,
    );
    expect(
      actionFailures.slice(failuresBefore).map((f) => `${f.title}: ${f.detail}`),
      "the duplicate reported no failure",
    ).to.deep.equal([]);
  });
});

// ---------- daemon round trips ----------

type ActionFailedMessage = DaemonMessage & {
  type: "action_failed";
  title: string;
  detail: string;
};

function isActionFailed(msg: DaemonMessage): msg is ActionFailedMessage {
  return msg.type === "action_failed";
}

/**
 * Duplicate `source` and resolve with the clone's snapshot. The clone is the
 * first plain-shell `session_updated` for a session id this spec has not seen
 * before — the source and every earlier session keep emitting updates as their
 * status settles.
 */
async function duplicate(
  ws: DaemonWsClient,
  source: SessionSnapshot,
  known: Map<string, unknown>,
  failures: ActionFailedMessage[],
): Promise<SessionSnapshot> {
  const seen = new Set(known.keys());
  const cloned = ws.waitFor(
    (msg): msg is SessionUpdatedMessage =>
      isSessionUpdated(msg) &&
      msg.session.id !== source.id &&
      !seen.has(msg.session.id) &&
      msg.session.mode === "plain_shell",
    { timeoutMs: 30_000 },
  );
  const failuresBefore = failures.length;
  ws.send({ type: "duplicate_session", session_id: source.id });
  try {
    return (await cloned).session;
  } catch (err) {
    const refusals = failures
      .slice(failuresBefore)
      .map((f) => `${f.title}: ${f.detail}`)
      .join("; ");
    throw new Error(
      `duplicate of ${source.id} produced no session (${String(err)})` +
        (refusals ? `; daemon said: ${refusals}` : ""),
    );
  }
}

async function spawnPlainShell(
  ws: DaemonWsClient,
  repoId: string,
  options: { branchName: string; label: string; useWorktree: boolean },
): Promise<SessionSnapshot> {
  const spawnedSession = ws.waitFor(
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
  return (await spawnedSession).session;
}

async function listSessions(ws: DaemonWsClient): Promise<SessionSnapshot[]> {
  const reply = ws.waitFor(
    (
      msg,
    ): msg is DaemonMessage & {
      type: "sessions";
      sessions: SessionSnapshot[];
    } => msg.type === "sessions",
    { timeoutMs: 20_000 },
  );
  ws.send({ type: "list_sessions" });
  return (await reply).sessions;
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

function remember(
  spawned: Map<string, { worktree: boolean }>,
  session: SessionSnapshot,
  worktree: boolean,
): void {
  spawned.set(session.id, { worktree });
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
