/**
 * Exercises the stage / unstage / commit round-trip end-to-end:
 *
 *   1. Make an unstaged change in a fixture repo.
 *   2. Request `repo_status`; expect the change to land in `worktree_changes`.
 *   3. Stage via the row context menu; expect the file to flip into
 *      `index_changes` and the STAGED bucket to render.
 *   4. Unstage via the staged-row context menu; expect it to flip back.
 *   5. Stage again, type a commit message, click Commit; expect a clean
 *      status and the new commit in git history.
 *
 * The harness watches the daemon's broadcast `repo_status` messages directly
 * — those are the authoritative signal that the write happened. UI assertions
 * confirm the visible split rendering.
 */
import { execFileSync } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";

import { browser } from "@wdio/globals";
import { expect } from "chai";

import { DaemonWsClient } from "../../../src/ws-client.js";
import type { DaemonMessage, RepoEntry } from "../../../src/types.js";

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;

interface GitFileChange {
  path: string;
  status: string;
  from_path: string | null;
}

interface RepoStatusMessage {
  type: "repo_status";
  repo_id: string;
  index_changes: GitFileChange[];
  worktree_changes: GitFileChange[];
  [k: string]: unknown;
}

describe("source-control sidebar stage/unstage/commit", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let registeredRepoId: string | null = null;

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });

    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    fixtureRepo = await mkdtemp(join(tmpdir(), "rt-e2e-scwrite-"));
    await writeFile(join(fixtureRepo, "README.md"), "fixture for sc-write e2e\n");
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);

    // Reset activity-bar so the test starts deterministically.
    await browser.execute(() => {
      try {
        localStorage.removeItem("rt.activity");
        localStorage.removeItem("rt.sourceControl.repoOverride");
      } catch {
        /* unavailable */
      }
    });

    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({ type: "add_repo", path: fixtureRepo, name: "rt-e2e-scwrite" });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (r: RepoEntry) =>
        r.path === fixtureRepo || r.path === fixtureRepo!.replace(/\\/g, "/"),
    );
    if (!fixture) throw new Error("fixture repo never registered");
    registeredRepoId = fixture.id;

    // Make an unstaged modification so the worktree bucket has something.
    await writeFile(
      join(fixtureRepo, "README.md"),
      "fixture for sc-write e2e\nmodified line\n",
    );

    // Activate the Source Control sidebar.
    const scBtn = await browser.$("[data-testid=activity-btn-source-control]");
    await scBtn.click();
    const sc = await browser.$("[data-testid=source-control-sidebar]");
    await sc.waitForExist({ timeout: 5_000 });
  });

  after(async function () {
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

  it("unstaged change shows up in the worktree bucket", async function () {
    if (!ws || !registeredRepoId) throw new Error("setup");
    const statusPromise = waitForRepoStatus(ws, registeredRepoId);
    ws.send({ type: "repo_status", repo_id: registeredRepoId });
    const status = await statusPromise;
    expect(status.worktree_changes.map((c) => c.path)).to.include("README.md");
    expect(status.index_changes.map((c) => c.path)).to.not.include("README.md");

    const worktreeBucket = await browser.$(
      "[data-testid=source-control-bucket-worktree]",
    );
    await worktreeBucket.waitForExist({ timeout: 5_000 });
  });

  it("right-clicking the row can stage the file (moves to index bucket)", async function () {
    if (!ws || !registeredRepoId) throw new Error("setup");
    const row = await browser.$(
      `[data-testid=source-control-changes-row][data-file-path="README.md"]`,
    );
    await row.waitForExist({ timeout: 5_000 });
    await row.click({ button: "right" });

    const stageBtn = await browser.$(
      `[data-testid=source-control-context-stage][data-file-path="README.md"]`,
    );
    await stageBtn.waitForExist({ timeout: 5_000 });
    await stageBtn.waitForEnabled({ timeout: 5_000 });

    const statusPromise = waitForRepoStatusWhere(
      ws,
      registeredRepoId,
      (status) =>
        status.index_changes.some((c) => c.path === "README.md") &&
        !status.worktree_changes.some((c) => c.path === "README.md"),
    );
    await stageBtn.click();
    const status = await statusPromise;
    expect(status.index_changes.map((c) => c.path)).to.include("README.md");
    expect(status.worktree_changes.map((c) => c.path)).to.not.include("README.md");

    const indexBucket = await browser.$(
      "[data-testid=source-control-bucket-index]",
    );
    await indexBucket.waitForExist({ timeout: 5_000 });
  });

  it("right-clicking a staged row can unstage it", async function () {
    if (!ws || !registeredRepoId) throw new Error("setup");
    const row = await browser.$(
      `[data-testid=source-control-changes-row][data-file-path="README.md"]`,
    );
    await row.waitForExist({ timeout: 5_000 });
    await row.click({ button: "right" });

    const unstageBtn = await browser.$(
      `[data-testid=source-control-context-unstage][data-file-path="README.md"]`,
    );
    await unstageBtn.waitForExist({ timeout: 5_000 });
    await unstageBtn.waitForEnabled({ timeout: 5_000 });

    const statusPromise = waitForRepoStatusWhere(
      ws,
      registeredRepoId,
      (status) =>
        !status.index_changes.some((c) => c.path === "README.md") &&
        status.worktree_changes.some((c) => c.path === "README.md"),
    );
    await unstageBtn.click();
    const status = await statusPromise;
    expect(status.index_changes.map((c) => c.path)).to.not.include("README.md");
    expect(status.worktree_changes.map((c) => c.path)).to.include("README.md");
  });

  it("commits cleanly after staging + typing a message", async function () {
    if (!ws || !registeredRepoId || !fixtureRepo) throw new Error("setup");
    // Stage again.
    const stageBtn = await browser.$(
      `[data-testid=source-control-row-stage][data-file-path="README.md"]`,
    );
    await stageBtn.waitForExist({ timeout: 5_000 });
    await stageBtn.waitForEnabled({ timeout: 5_000 });
    const stagePromise = waitForRepoStatusWhere(
      ws,
      registeredRepoId,
      (status) =>
        status.index_changes.some((c) => c.path === "README.md") &&
        !status.worktree_changes.some((c) => c.path === "README.md"),
    );
    await stageBtn.click();
    await stagePromise;

    // Type a commit message.
    const messageEl = await browser.$(
      "[data-testid=source-control-commit-message]",
    );
    await messageEl.setValue("e2e: commit from sidebar");

    // Click commit, then wait for a clean status broadcast.
    const cleanStatusPromise = ws.waitFor(
      (m): m is RepoStatusMessage => {
        if (m.type !== "repo_status") return false;
        const rec = m as unknown as RepoStatusMessage;
        return (
          rec.repo_id === registeredRepoId &&
          rec.index_changes.length === 0 &&
          rec.worktree_changes.length === 0
        );
      },
      { timeoutMs: 10_000 },
    );

    const submit = await browser.$("[data-testid=source-control-commit-submit]");
    await submit.waitForEnabled({ timeout: 5_000 });
    await submit.click();

    await cleanStatusPromise;
    const latestSubject = readGit(fixtureRepo, ["log", "-1", "--pretty=%s"]);
    expect(latestSubject).to.equal("e2e: commit from sidebar");

    // Visible empty state.
    const sc = await browser.$("[data-testid=source-control-sidebar]");
    await browser.waitUntil(
      async () => {
        const text = await sc.getText();
        return text.includes("working tree clean");
      },
      {
        timeout: 5_000,
        timeoutMsg: "post-commit empty state never rendered",
      },
    );
  });
});

function isRepos(
  m: DaemonMessage,
): m is DaemonMessage & { type: "repos"; repos: RepoEntry[] } {
  return m.type === "repos";
}

function waitForRepoStatus(
  ws: DaemonWsClient,
  repoId: string,
): Promise<RepoStatusMessage> {
  return waitForRepoStatusWhere(ws, repoId, () => true);
}

function waitForRepoStatusWhere(
  ws: DaemonWsClient,
  repoId: string,
  predicate: (status: RepoStatusMessage) => boolean,
): Promise<RepoStatusMessage> {
  return ws.waitFor(
    (m): m is RepoStatusMessage => {
      if (m.type !== "repo_status") return false;
      const rec = m as unknown as RepoStatusMessage;
      return rec.repo_id === repoId && predicate(rec);
    },
    { timeoutMs: 5_000 },
  );
}

function runGit(cwd: string, args: string[]): void {
  execFileSync("git", args, { cwd, stdio: "ignore" });
}

function readGit(cwd: string, args: string[]): string {
  return execFileSync("git", args, { cwd, encoding: "utf8" }).trim();
}
