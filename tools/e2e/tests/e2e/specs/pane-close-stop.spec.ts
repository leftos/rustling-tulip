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
import type {
  DaemonMessage,
  GridNode,
  RepoEntry,
  SessionSnapshot,
  TabEntry,
} from "../../../src/types.js";

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;
const repoRoot = resolve(
  fileURLToPath(new URL("../../../../..", import.meta.url)),
);
type ChainableElement = ReturnType<typeof browser.$>;

describe("pane close stop cleanup", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let registeredRepoId: string | null = null;
  const spawnedSessionIds = new Set<string>();
  const retainedWorktreePaths: string[] = [];

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
    await dismissLayoutChooser(APP_BOOT_TIMEOUT);

    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    const parent = join(repoRoot, ".tmp", "e2e");
    await mkdir(parent, { recursive: true });
    fixtureRepo = await mkdtemp(join(parent, "rt-e2e-pane-close-stop-"));
    await writeFile(join(fixtureRepo, "README.md"), "pane close stop fixture\n");
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);

    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({
      type: "add_repo",
      path: fixtureRepo,
      name: "rt-e2e-pane-close-stop",
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
    for (const worktreePath of retainedWorktreePaths) {
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
      await rm(fixtureRepo, {
        recursive: true,
        force: true,
        maxRetries: 20,
        retryDelay: 100,
      });
    }
  });

  it("offers session-only choices without worktree wording for normal repos", async function () {
    if (!ws || !registeredRepoId) throw new Error("setup failed");

    const spawn = await spawnPlainShell(ws, registeredRepoId, {
      branchName: "main",
      label: "pane-close-stop-shell",
      useWorktree: false,
    });
    spawnedSessionIds.add(spawn.id);

    await createTabForSession(ws, "Pane close stop", spawn.id);
    const dialog = await openPaneCloseDialog(spawn.id);

    await expectButtonText(
      dialog,
      "pane-close-dialog-pane-only",
      "Close pane but keep session in sidebar",
    );
    await expectButtonText(
      dialog,
      "pane-close-dialog-close-session-keep-worktree",
      "Close pane and close session",
    );
    await expectMissingButton(
      dialog,
      "pane-close-dialog-close-session-delete-worktree",
    );

    const paneId = await getGridPaneIdForSession(spawn.id);
    const removed = waitForSessionRemoved(ws, spawn.id);
    await clickDialogButton(dialog, "pane-close-dialog-close-session-keep-worktree");
    await removed;
    spawnedSessionIds.delete(spawn.id);

    await expectSessionRemovedFromSidebar(spawn.id);
    await expectGridPaneRemoved(paneId);
  });

  it("offers separate session and worktree choices for worktree sessions", async function () {
    if (!ws || !registeredRepoId || !fixtureRepo) throw new Error("setup failed");

    const branch = "rt-e2e-pane-close-stop-worktree";
    const spawn = await spawnPlainShell(ws, registeredRepoId, {
      branchName: branch,
      label: "pane-close-stop-worktree-shell",
      useWorktree: true,
    });
    spawnedSessionIds.add(spawn.id);

    const retainedWorktreePath = spawn.members[0]?.worktree_path;
    if (!retainedWorktreePath) {
      throw new Error("worktree session did not report a worktree path");
    }
    retainedWorktreePaths.push(retainedWorktreePath);

    await createTabForSession(ws, "Pane close stop worktree", spawn.id);
    const dialog = await openPaneCloseDialog(spawn.id);

    await expectButtonText(
      dialog,
      "pane-close-dialog-pane-only",
      "Close pane, keep session, keep worktree",
    );
    await expectButtonText(
      dialog,
      "pane-close-dialog-close-session-keep-worktree",
      "Close pane, don't keep session, keep worktree",
    );
    await expectButtonText(
      dialog,
      "pane-close-dialog-close-session-delete-worktree",
      "Close pane, don't keep session, delete worktree",
    );

    // The delete-worktree choice hands over to the branch-fate confirm
    // instead of discarding straight away. This branch was cut from main and
    // never committed to, so every target reports it as already landed and
    // the dialog offers the single unambiguous delete.
    const paneId = await getGridPaneIdForSession(spawn.id);
    await clickDialogButton(
      dialog,
      "pane-close-dialog-close-session-delete-worktree",
    );
    const fateDialog = await openDeleteWorktreeDialog();

    const members = await fateDialog.$$('[data-testid="delete-worktree-member"]');
    expect(members.length, "one member row for a single-repo session").to.equal(1);
    const memberText = await members[0]!.getText();
    expect(memberText).to.contain(branch);
    expect(memberText).to.contain("already merged into main");

    await expectButtonText(
      fateDialog,
      "delete-worktree-and-branch",
      "Delete worktree and branch",
    );
    await expectMissingButton(fateDialog, "delete-worktree-keep-branch");

    const removed = waitForSessionRemoved(ws, spawn.id);
    await clickDialogButton(fateDialog, "delete-worktree-and-branch");
    await removed;
    spawnedSessionIds.delete(spawn.id);

    await expectSessionRemovedFromSidebar(spawn.id);
    await expectGridPaneRemoved(paneId);

    expect(
      existsSync(retainedWorktreePath),
      "worktree directory removed from disk",
    ).to.equal(false);
    expect(
      gitOutput(fixtureRepo, ["branch", "--list", branch]),
      "session branch reaped",
    ).to.equal("");
  });
});

/**
 * Wait for the branch-fate confirm to open and finish its `preview_discard`
 * round trip, so button labels and member rows are the daemon's answer rather
 * than the loading placeholder.
 */
async function openDeleteWorktreeDialog() {
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

async function spawnPlainShell(
  ws: DaemonWsClient,
  repoId: string,
  options: {
    branchName: string;
    label: string;
    useWorktree: boolean;
  },
): Promise<SessionSnapshot> {
  const spawned = ws.waitFor(
    (msg): msg is DaemonMessage & {
      type: "session_updated";
      session: SessionSnapshot;
    } =>
      isSessionUpdated(msg) &&
      msg.session.label === options.label &&
      msg.session.mode === "plain_shell",
    { timeoutMs: 20_000 },
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

async function createTabForSession(
  ws: DaemonWsClient,
  name: string,
  sessionId: string,
): Promise<void> {
  const tabPromise = ws.waitFor(
    (msg): msg is DaemonMessage & { type: "tab_updated"; tab: TabEntry } =>
      isTabUpdated(msg) && tabContainsSession(msg.tab, sessionId),
    { timeoutMs: 5_000 },
  );
  ws.send({
    type: "create_tab",
    name,
    initial_session_id: sessionId,
  });
  await tabPromise;
}

async function openPaneCloseDialog(sessionId: string) {
  const leaf = await browser.$(
    `[data-testid=sidebar-session][data-session-id="${sessionId}"]`,
  );
  await leaf.waitForExist({ timeout: 10_000 });
  await leaf.click();

  const pane = await browser.$(
    `[data-testid=session-pane][data-session-id="${sessionId}"]`,
  );
  await pane.waitForExist({ timeout: 10_000 });

  await (await pane.$('[data-testid="pane-close"]')).click();
  const dialog = await browser.$('[data-testid="pane-close-dialog"]');
  await dialog.waitForExist({ timeout: 5_000 });
  return dialog;
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
): Promise<DaemonMessage & { type: "session_removed"; session_id: string }> {
  return ws.waitFor(
    (msg): msg is DaemonMessage & {
      type: "session_removed";
      session_id: string;
    } => msg.type === "session_removed" && msg.session_id === sessionId,
    { timeoutMs: 5_000 },
  );
}

async function expectSessionRemovedFromSidebar(sessionId: string): Promise<void> {
  await browser.waitUntil(
    async () =>
      !(await browser
        .$(`[data-testid=sidebar-session][data-session-id="${sessionId}"]`)
        .isExisting()),
    {
      timeout: 5_000,
      timeoutMsg: "closed session remained in the sidebar",
    },
  );
}

async function getGridPaneIdForSession(sessionId: string): Promise<string> {
  const result: unknown = await browser.execute(`
    return (() => {
      const sessionId = ${JSON.stringify(sessionId)};
      const sessionPanes = Array.from(document.querySelectorAll('[data-testid="session-pane"]'));
      const sessionPane = sessionPanes.find((candidate) =>
        candidate.getAttribute('data-session-id') === sessionId
      );
      const gridPane = sessionPane?.closest('[data-pane-id]');
      return gridPane?.getAttribute('data-pane-id') ?? null;
    })()
  `);
  const paneId = typeof result === "string" ? result : null;
  if (!paneId) {
    throw new Error(`grid pane not found for session: ${sessionId}`);
  }
  return paneId;
}

async function expectGridPaneRemoved(paneId: string): Promise<void> {
  await browser.waitUntil(
    async () => !(await browser.$(`[data-pane-id="${paneId}"]`).isExisting()),
    {
      timeout: 5_000,
      timeoutMsg: "closed pane remained in the grid",
    },
  );
}

function isRepos(
  msg: DaemonMessage,
): msg is DaemonMessage & { type: "repos"; repos: RepoEntry[] } {
  return msg.type === "repos";
}

function isTabUpdated(
  msg: DaemonMessage,
): msg is DaemonMessage & { type: "tab_updated"; tab: TabEntry } {
  return msg.type === "tab_updated";
}

function tabContainsSession(tab: TabEntry, sessionId: string): boolean {
  if (tab.content.kind !== "grid") {
    return false;
  }
  return gridContainsSession(tab.content.grid, sessionId);
}

function gridContainsSession(node: GridNode, sessionId: string): boolean {
  if (node.kind === "pane") {
    return node.session_id === sessionId;
  }
  return (
    gridContainsSession(node.first, sessionId) ||
    gridContainsSession(node.second, sessionId)
  );
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
