import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

import { browser } from "@wdio/globals";
import { expect } from "chai";

import { DaemonWsClient } from "../../../src/ws-client.js";
import { dismissLayoutChooser } from "../../../src/session-helpers.js";
import { waitForBranchSuggestion } from "../../../src/spawn-dialog.js";
import type {
  DaemonMessage,
  RepoEntry,
  WorkspaceEntry,
} from "../../../src/types.js";

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;
const repoRoot = resolve(
  fileURLToPath(new URL("../../../../..", import.meta.url)),
);

/**
 * Regression coverage for the workspace spawn dialog seeding its branch NAME
 * field with the remote-preferred default (`origin/main`). A branch literally
 * named `origin/main` materializes `refs/heads/origin/main`, after which
 * every bare `origin/main` reference in the repo is ambiguous.
 *
 * The base-branch field is the one that must prefer `origin/main`; the
 * branch-name field must stay on the local name. The test waits for the base
 * field to show the remote ref first — that proves the remote branch list has
 * loaded, which is exactly the moment the old code re-seeded the branch field
 * with the remote name.
 */
describe("workspace spawn dialog branch seeding", function () {
  this.timeout(240_000);

  let ws: DaemonWsClient | null = null;
  let remoteRepo: string | null = null;
  let originRepo: string | null = null;
  let plainRepo: string | null = null;
  let remoteRepoId: string | null = null;
  let plainRepoId: string | null = null;
  let workspaceId: string | null = null;

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
    await dismissLayoutChooser(APP_BOOT_TIMEOUT);
    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    const parent = join(repoRoot, ".tmp", "e2e");
    await mkdir(parent, { recursive: true });

    // First member: a repo whose origin/main exists, so the dialog's remote
    // branch list is non-empty — the precondition for the regression.
    remoteRepo = await mkdtemp(join(parent, "rt-e2e-ws-seed-a-"));
    originRepo = `${remoteRepo}-origin`;
    await mkdir(originRepo, { recursive: true });
    runGit(originRepo, ["init", "--bare", "-b", "main"]);
    await initRepo(remoteRepo);
    runGit(remoteRepo, ["remote", "add", "origin", originRepo]);
    runGit(remoteRepo, ["push", "-u", "origin", "main"]);
    runGit(remoteRepo, ["fetch", "--prune"]);

    // Second member: any local repo — a workspace needs two registered repos.
    plainRepo = await mkdtemp(join(parent, "rt-e2e-ws-seed-b-"));
    await initRepo(plainRepo);

    remoteRepoId = await addRepo(ws, remoteRepo, "rt-e2e-ws-seed-remote");
    plainRepoId = await addRepo(ws, plainRepo, "rt-e2e-ws-seed-plain");

    const workspacesPromise = ws.waitFor(isWorkspaces, { timeoutMs: 5_000 });
    ws.send({
      type: "upsert_workspace",
      id: null,
      name: "rt-e2e-ws-seed",
      member_repo_ids: [remoteRepoId, plainRepoId],
    });
    const workspaces = await workspacesPromise;
    const created = workspaces.workspaces.find(
      (w) => w.name === "rt-e2e-ws-seed",
    );
    if (!created) throw new Error("workspace was not created");
    workspaceId = created.id;
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
      if (workspaceId) {
        ws.send({ type: "remove_workspace", workspace_id: workspaceId });
        await delay(200);
      }
      for (const id of [remoteRepoId, plainRepoId]) {
        if (!id) continue;
        ws.send({ type: "remove_repo", repo_id: id });
        await delay(200);
      }
      await ws.close();
    }
    for (const path of [remoteRepo, originRepo, plainRepo]) {
      if (!path) continue;
      await rm(path, {
        recursive: true,
        force: true,
        maxRetries: 20,
        retryDelay: 100,
      });
    }
  });

  it("keeps the branch name local while the base prefers the remote ref", async function () {
    if (!workspaceId) throw new Error("setup failed");

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
    await target.selectByAttribute("value", `workspace:${workspaceId}`);

    // Worktree mode is the default; the base field only renders there. The
    // dialog fetches on open, so origin/main may take a beat to appear in the
    // remote list that seeds it. Once it has, the remote refs are loaded —
    // the exact state in which the branch field used to pick up origin/main.
    const base = await dialog.$('[data-testid="spawn-workspace-base-branch"]');
    await base.waitForExist({ timeout: 10_000 });
    await browser.waitUntil(async () => (await base.getValue()) === "origin/main", {
      timeout: 20_000,
      timeoutMsg: `base branch seeded with "${await base.getValue()}", expected origin/main`,
    });

    // The name comes from the daemon now, so the field is empty until
    // `branch_name_suggestion` lands.
    await waitForBranchSuggestion(dialog);
    const branch = await dialog.$('[data-testid="spawn-workspace-branch"]');
    expect(await branch.getValue()).to.match(
      /^wt\//,
      "worktree mode suggests a random wt/ branch name",
    );

    // Toggle worktree off: the field reverts from the wt/ suggestion to the
    // default seed, which must be the LOCAL default branch. Before the fix it
    // reverted to origin/main, and spawning then created a local branch by
    // that name.
    const worktree = await dialog.$('[data-testid="spawn-workspace-worktree"]');
    await worktree.click();
    await browser.waitUntil(async () => !(await worktree.isSelected()), {
      timeout: 5_000,
      timeoutMsg: "worktree toggle did not turn off",
    });
    await browser.waitUntil(async () => (await branch.getValue()) === "main", {
      timeout: 5_000,
      timeoutMsg: `branch field seeded with "${await branch.getValue()}", expected the local "main"`,
    });
  });
});

async function initRepo(path: string): Promise<void> {
  await writeFile(join(path, "README.md"), "workspace seed fixture\n");
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
  const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
  ws.send({ type: "add_repo", path, name });
  const repos = await reposPromise;
  const added = repos.repos.find(
    (r: RepoEntry) => r.path === path || r.path === path.replace(/\\/g, "/"),
  );
  if (!added) throw new Error(`fixture repo was not registered: ${path}`);
  return added.id;
}

function runGit(cwd: string, args: string[]): void {
  execFileSync("git", args, { cwd, stdio: "ignore" });
}

function isRepos(
  m: DaemonMessage,
): m is DaemonMessage & { type: "repos"; repos: RepoEntry[] } {
  return m.type === "repos";
}

function isWorkspaces(
  m: DaemonMessage,
): m is DaemonMessage & { type: "workspaces"; workspaces: WorkspaceEntry[] } {
  return m.type === "workspaces";
}
