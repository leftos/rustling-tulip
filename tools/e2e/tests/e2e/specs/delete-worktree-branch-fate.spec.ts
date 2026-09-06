/**
 * Branch-fate coverage for the delete-worktree confirm dialog.
 *
 * Every "delete worktree" gesture routes through one modal that asks the
 * daemon what a discard would do to each member's branch. These specs drive
 * the answers that matter against a real fixture repo — work that only exists
 * on the branch, the same work after it lands on main by cherry-pick, and a
 * cancelled prompt — and check what survives on disk, not just the wording.
 */
import { execFileSync } from "node:child_process";
import { existsSync, writeFileSync } from "node:fs";
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
  openSessionPane,
} from "../../../src/session-helpers.js";
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
type ChainableElement = ReturnType<typeof browser.$>;

/** Branches these specs create; force-deleted in `after` whatever happened. */
const FATE_BRANCHES = [
  "rt-e2e-fate-unique",
  "rt-e2e-fate-force",
  "rt-e2e-fate-picked",
  "rt-e2e-fate-cancel",
];

describe("delete worktree branch fate", function () {
  this.timeout(240_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let registeredRepoId: string | null = null;
  const spawnedSessionIds = new Set<string>();
  const worktreePaths: string[] = [];

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
    await dismissLayoutChooser(APP_BOOT_TIMEOUT);

    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    const parent = join(repoRoot, ".tmp", "e2e");
    await mkdir(parent, { recursive: true });
    fixtureRepo = await mkdtemp(join(parent, "rt-e2e-branch-fate-"));
    await writeFile(join(fixtureRepo, "README.md"), "branch fate fixture\n");
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);

    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({
      type: "add_repo",
      path: fixtureRepo,
      name: "rt-e2e-branch-fate",
    });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (r) =>
        r.path === fixtureRepo || r.path === fixtureRepo?.replace(/\\/g, "/"),
    );
    if (!fixture) throw new Error("fixture repo never registered");
    registeredRepoId = fixture.id;
  });

  after(async function () {
    if (ws) {
      for (const sessionId of spawnedSessionIds) {
        try {
          ws.send({ type: "stop_session", session_id: sessionId, cleanup: [] });
          await delay(300);
          ws.send({
            type: "discard_session",
            session_id: sessionId,
            cleanup: [],
          });
          await delay(300);
        } catch {
          /* best-effort cleanup */
        }
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
    await ws?.close();
    for (const worktreePath of worktreePaths) {
      if (fixtureRepo) {
        try {
          runGit(fixtureRepo, ["worktree", "remove", "--force", worktreePath]);
        } catch {
          /* best-effort cleanup */
        }
      }
      await rm(worktreePath, {
        recursive: true,
        force: true,
        maxRetries: 20,
        retryDelay: 100,
      });
    }
    if (fixtureRepo) {
      try {
        runGit(fixtureRepo, ["worktree", "prune"]);
      } catch {
        /* best-effort cleanup */
      }
      for (const branch of FATE_BRANCHES) {
        try {
          runGit(fixtureRepo, ["branch", "-D", branch]);
        } catch {
          /* already reaped by the discard under test */
        }
      }
      await rm(fixtureRepo, {
        recursive: true,
        force: true,
        maxRetries: 20,
        retryDelay: 100,
      });
    }
  });

  it("keeps a branch holding a unique commit when the user picks keep", async function () {
    if (!ws || !registeredRepoId || !fixtureRepo)
      throw new Error("setup failed");

    const branch = "rt-e2e-fate-unique";
    const session = await spawnWorktreeShell(ws, registeredRepoId, {
      branchName: branch,
      label: "fate-unique-shell",
    });
    spawnedSessionIds.add(session.id);
    const worktree = worktreeOf(session);
    worktreePaths.push(worktree);
    commitFile(worktree, "unique.txt", "work that never landed");

    const paneDialog = await openPaneCloseDialog(session.id);
    await clickDialogButton(
      paneDialog,
      "pane-close-dialog-close-session-delete-worktree",
    );

    const dialog = await openDeleteWorktreeDialog();
    expect(await memberFateText(dialog, branch)).to.contain(
      "1 commit not in main",
    );
    await expectButtonText(
      dialog,
      "delete-worktree-keep-branch",
      "Delete worktree, keep branch",
    );
    await expectButtonText(
      dialog,
      "delete-worktree-and-branch",
      "Delete worktree and branch (loses 1 commit)",
    );

    const removed = waitForSessionRemoved(ws, session.id);
    await clickDialogButton(dialog, "delete-worktree-keep-branch");
    await removed;
    spawnedSessionIds.delete(session.id);

    expect(existsSync(worktree), "worktree directory removed").to.equal(false);
    expect(
      gitOutput(fixtureRepo, ["branch", "--list", branch]),
      "branch survives the keep choice",
    ).to.contain(branch);
  });

  it("deletes an unmerged branch when the user picks delete", async function () {
    if (!ws || !registeredRepoId || !fixtureRepo)
      throw new Error("setup failed");

    const branch = "rt-e2e-fate-force";
    const session = await spawnWorktreeShell(ws, registeredRepoId, {
      branchName: branch,
      label: "fate-force-shell",
    });
    spawnedSessionIds.add(session.id);
    const worktree = worktreeOf(session);
    worktreePaths.push(worktree);
    commitFile(worktree, "force.txt", "work the user chooses to drop");

    const paneDialog = await openPaneCloseDialog(session.id);
    await clickDialogButton(
      paneDialog,
      "pane-close-dialog-close-session-delete-worktree",
    );

    const dialog = await openDeleteWorktreeDialog();
    expect(await memberFateText(dialog, branch)).to.contain(
      "1 commit not in main",
    );

    const removed = waitForSessionRemoved(ws, session.id);
    await clickDialogButton(dialog, "delete-worktree-and-branch");
    await removed;
    spawnedSessionIds.delete(session.id);

    expect(existsSync(worktree), "worktree directory removed").to.equal(false);
    expect(
      gitOutput(fixtureRepo, ["branch", "--list", branch]),
      "explicit delete reaps the branch",
    ).to.equal("");
  });

  it("reads cherry-picked work as landed and offers a single delete", async function () {
    if (!ws || !registeredRepoId || !fixtureRepo)
      throw new Error("setup failed");

    const branch = "rt-e2e-fate-picked";
    const session = await spawnWorktreeShell(ws, registeredRepoId, {
      branchName: branch,
      label: "fate-picked-shell",
    });
    spawnedSessionIds.add(session.id);
    const worktree = worktreeOf(session);
    worktreePaths.push(worktree);
    commitFile(worktree, "picked.txt", "work that lands by cherry-pick");
    const sha = gitOutput(worktree, ["rev-parse", "HEAD"]);
    // Move main on before replaying the patch. Cherry-picking straight onto
    // the branch's own parent reproduces the branch commit byte for byte
    // (same tree, parent, message, identity, and — within the same second —
    // committer date), so git hands back the identical sha and the fate reads
    // as plain ancestry. A commit in between forces the distinct sha that
    // makes this the patch-equivalence case.
    commitFile(fixtureRepo, "mainline.txt", "unrelated work on main");
    runGit(fixtureRepo, ["cherry-pick", sha]);

    const paneDialog = await openPaneCloseDialog(session.id);
    await clickDialogButton(
      paneDialog,
      "pane-close-dialog-close-session-delete-worktree",
    );

    const dialog = await openDeleteWorktreeDialog();
    expect(await memberFateText(dialog, branch)).to.contain(
      "cherry-picked or rebased",
    );
    await expectMissingButton(dialog, "delete-worktree-keep-branch");
    await expectButtonText(
      dialog,
      "delete-worktree-and-branch",
      "Delete worktree and branch",
    );

    const removed = waitForSessionRemoved(ws, session.id);
    await clickDialogButton(dialog, "delete-worktree-and-branch");
    await removed;
    spawnedSessionIds.delete(session.id);

    expect(existsSync(worktree), "worktree directory removed").to.equal(false);
    expect(
      gitOutput(fixtureRepo, ["branch", "--list", branch]),
      "landed branch reaped",
    ).to.equal("");
  });

  it("leaves the session running when the delete prompt is cancelled", async function () {
    if (!ws || !registeredRepoId) throw new Error("setup failed");

    const session = await spawnWorktreeShell(ws, registeredRepoId, {
      branchName: "rt-e2e-fate-cancel",
      label: "fate-cancel-shell",
    });
    spawnedSessionIds.add(session.id);
    worktreePaths.push(worktreeOf(session));

    const row = await browser.$(
      `[data-testid=sidebar-session][data-session-id="${session.id}"]`,
    );
    await row.waitForExist({ timeout: 10_000 });
    await row.click({ button: "right" });
    const stopEntry = await browser.$("[data-testid=session-context-close]");
    await stopEntry.waitForDisplayed({ timeout: 5_000 });
    await stopEntry.click();
    const removeEntry = await browser.$(
      "[data-testid=session-context-close-remove-worktree]",
    );
    await removeEntry.waitForDisplayed({ timeout: 5_000 });
    await removeEntry.click();

    const dialog = await browser.$('[data-testid="delete-worktree-dialog"]');
    await dialog.waitForExist({ timeout: 10_000 });
    await clickDialogButton(dialog, "delete-worktree-cancel");
    await dialog.waitForExist({ timeout: 5_000, reverse: true });

    // "Stop and delete worktree" no longer stops the session up front, so a
    // cancelled prompt has to leave the child alive.
    await delay(1_000);
    const status = await sessionStatus(ws, session.id);
    expect(status, "session survives a cancelled delete prompt").to.not.equal(
      null,
    );
    expect(
      status,
      "session was not stopped by the cancelled prompt",
    ).to.not.equal("stopped");

    ws.send({ type: "stop_session", session_id: session.id, cleanup: [] });
    await delay(500);
    const removed = waitForSessionRemoved(ws, session.id, 20_000);
    ws.send({
      type: "discard_session",
      session_id: session.id,
      cleanup: [
        { repo_id: registeredRepoId, remove_worktree: true, branch: "delete" },
      ],
    });
    await removed;
    spawnedSessionIds.delete(session.id);
  });
});

async function spawnWorktreeShell(
  ws: DaemonWsClient,
  repoId: string,
  options: { branchName: string; label: string },
): Promise<SessionSnapshot> {
  const spawned = ws.waitFor(
    (msg): msg is DaemonMessage & {
      type: "session_updated";
      session: SessionSnapshot;
    } =>
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
      useWorktree: true,
      mode: "plain_shell",
    }),
  );
  return (await spawned).session;
}

function worktreeOf(session: SessionSnapshot): string {
  const path = session.members[0]?.worktree_path;
  if (!path) throw new Error("worktree session did not report a worktree path");
  return path;
}

/** Write a file into a working tree and commit it on that tree's branch. */
function commitFile(dir: string, fileName: string, message: string): void {
  writeFileSync(join(dir, fileName), `${message}\n`);
  runGit(dir, ["add", fileName]);
  runGit(dir, ["commit", "-m", message]);
}

async function openPaneCloseDialog(
  sessionId: string,
): Promise<ChainableElement> {
  await openSessionPane(sessionId);
  const pane = await browser.$(
    `[data-testid=session-pane][data-session-id="${sessionId}"]`,
  );
  await pane.waitForExist({ timeout: 10_000 });
  await (await pane.$('[data-testid="pane-close"]')).click();
  const dialog = await browser.$('[data-testid="pane-close-dialog"]');
  await dialog.waitForExist({ timeout: 5_000 });
  return dialog;
}

/**
 * Wait for the branch-fate confirm to open and finish its `preview_discard`
 * round trip, so button labels and member rows carry the daemon's answer
 * rather than the loading placeholder or the timeout fallback.
 */
async function openDeleteWorktreeDialog(): Promise<ChainableElement> {
  const dialog = await browser.$('[data-testid="delete-worktree-dialog"]');
  await dialog.waitForExist({ timeout: 10_000 });
  const loading = await dialog.$(
    '[data-testid="delete-worktree-dialog-loading"]',
  );
  await loading.waitForExist({ timeout: 15_000, reverse: true });
  const timedOut = await dialog.$(
    '[data-testid="delete-worktree-dialog-timeout"]',
  );
  expect(
    await timedOut.isExisting(),
    "daemon answered preview_discard before the dialog timed out",
  ).to.equal(false);
  return dialog;
}

/** Text of the single member row, asserted to name `branch`. */
async function memberFateText(
  dialog: ChainableElement,
  branch: string,
): Promise<string> {
  const members = await dialog.$$('[data-testid="delete-worktree-member"]');
  expect(members.length, "one member row for a single-repo session").to.equal(
    1,
  );
  const text = await members[0]!.getText();
  expect(text, "member row names the session branch").to.contain(branch);
  return text;
}

async function expectButtonText(
  dialog: ChainableElement,
  testId: string,
  expected: string,
): Promise<void> {
  const button = await dialog.$(`[data-testid="${testId}"]`);
  await button.waitForExist({ timeout: 2_000 });
  expect(await button.getText()).to.equal(expected);
}

async function expectMissingButton(
  dialog: ChainableElement,
  testId: string,
): Promise<void> {
  const button = await dialog.$(`[data-testid="${testId}"]`);
  expect(await button.isExisting()).to.equal(false);
}

async function clickDialogButton(
  dialog: ChainableElement,
  testId: string,
): Promise<void> {
  const button = await dialog.$(`[data-testid="${testId}"]`);
  await button.click();
}

function waitForSessionRemoved(
  ws: DaemonWsClient,
  sessionId: string,
  timeoutMs = 10_000,
): Promise<DaemonMessage & { type: "session_removed"; session_id: string }> {
  return ws.waitFor(
    (msg): msg is DaemonMessage & {
      type: "session_removed";
      session_id: string;
    } => msg.type === "session_removed" && msg.session_id === sessionId,
    { timeoutMs },
  );
}

/** The daemon's own view of a session's status; null once it's gone. */
async function sessionStatus(
  ws: DaemonWsClient,
  sessionId: string,
): Promise<string | null> {
  const sessions = ws.waitFor(isSessions, { timeoutMs: 5_000 });
  ws.send({ type: "list_sessions" });
  const msg = await sessions;
  return msg.sessions.find((s) => s.id === sessionId)?.status ?? null;
}

function isRepos(
  msg: DaemonMessage,
): msg is DaemonMessage & { type: "repos"; repos: RepoEntry[] } {
  return msg.type === "repos";
}

function isSessions(
  msg: DaemonMessage,
): msg is DaemonMessage & { type: "sessions"; sessions: SessionSnapshot[] } {
  return msg.type === "sessions";
}

function isSessionUpdated(
  msg: DaemonMessage,
): msg is DaemonMessage & { type: "session_updated"; session: SessionSnapshot } {
  return msg.type === "session_updated";
}

function runGit(cwd: string, args: string[]): void {
  execFileSync("git", args, { cwd, stdio: "ignore" });
}

/** `runGit`, but captures trimmed stdout — for assertions on git's answer. */
function gitOutput(cwd: string, args: string[]): string {
  return execFileSync("git", args, { cwd, encoding: "utf8" }).trim();
}
