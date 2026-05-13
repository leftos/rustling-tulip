/**
 * Monaco diff tabs opened from the source-control sidebar.
 *
 * Asserts the round trip:
 *   1. Dirty the worktree, click a file row in CHANGES.
 *   2. A new tab appears with content.kind = "diff" and the right testids.
 *   3. The diff pane loads (the daemon answers `get_file_snapshot`).
 *   4. Clicking the same file again does NOT create a duplicate tab —
 *      the dedup keyed on (repo_id, path, against) just focuses the
 *      existing one.
 *   5. Closing the tab removes it.
 *
 * Monaco itself runs inside the WebView; this spec doesn't poke at the
 * editor instance directly. The DiffEditor mounts a [data-testid=diff-pane-editor]
 * div and Monaco fills it in.
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

describe("source-control sidebar diff tabs", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let decoyRepo: string | null = null;
  let fixtureRepo: string | null = null;
  let decoyRepoId: string | null = null;
  let registeredRepoId: string | null = null;
  let spawnedSessionId: string | null = null;

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });

    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    decoyRepo = await mkdtemp(join(tmpdir(), "rt-e2e-scalt-"));
    await writeFile(join(decoyRepo, "README.md"), "decoy repo for sc-diff e2e\n");
    runGit(decoyRepo, ["init", "-b", "main"]);
    runGit(decoyRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(decoyRepo, ["config", "user.name", "rt-e2e"]);
    runGit(decoyRepo, ["add", "README.md"]);
    runGit(decoyRepo, ["commit", "-m", "initial decoy commit"]);

    fixtureRepo = await mkdtemp(join(tmpdir(), "rt-e2e-scdiff-"));
    await writeFile(join(fixtureRepo, "README.md"), "fixture for sc-diff e2e\n");
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);

    await browser.execute(() => {
      try {
        localStorage.removeItem("rt.activity");
        localStorage.removeItem("rt.sourceControl.repoOverride");
      } catch {
        /* unavailable */
      }
    });

    const decoyReposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({ type: "add_repo", path: decoyRepo, name: "rt-e2e-altrepo" });
    const decoyRepos = await decoyReposPromise;
    const decoy = decoyRepos.repos.find(
      (r: RepoEntry) =>
        r.path === decoyRepo || r.path === decoyRepo!.replace(/\\/g, "/"),
    );
    if (!decoy) throw new Error("decoy repo never registered");
    decoyRepoId = decoy.id;

    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({ type: "add_repo", path: fixtureRepo, name: "rt-e2e-scdiff" });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (r: RepoEntry) =>
        r.path === fixtureRepo || r.path === fixtureRepo!.replace(/\\/g, "/"),
    );
    if (!fixture) throw new Error("fixture repo never registered");
    registeredRepoId = fixture.id;

    const spawnPromise = ws.waitFor(
      (m): m is DaemonMessage & {
        type: "session_updated";
        session: SessionSnapshot;
      } => m.type === "session_updated",
      { timeoutMs: 15_000 },
    );
    ws.send({
      type: "spawn_session",
      label: "sc-diff",
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
    ws.send({
      type: "create_tab",
      name: null,
      initial_session_id: spawnedSessionId,
    });

    const leaf = await browser.$(
      `[data-testid=sidebar-session][data-session-id="${spawnedSessionId}"]`,
    );
    await leaf.waitForExist({ timeout: 10_000 });
    await leaf.click();

    // Make an unstaged modification so CHANGES has a row to click.
    await writeFile(
      join(fixtureRepo, "README.md"),
      "fixture for sc-diff e2e\nmodified for diff tab test\n",
    );

    const scBtn = await browser.$("[data-testid=activity-btn-source-control]");
    await scBtn.click();
    const sc = await browser.$("[data-testid=source-control-sidebar]");
    await sc.waitForExist({ timeout: 5_000 });
    const activeRepo = await browser.$("[data-testid=source-control-active-repo]");
    await browser.waitUntil(
      async () => {
        const text = await activeRepo.getText();
        return text.includes("rt-e2e-scdiff");
      },
      {
        timeout: 5_000,
        timeoutMsg: "source-control sidebar never followed fixture repo",
      },
    );
  });

  after(async function () {
    if (ws && spawnedSessionId) {
      try {
        ws.send({ type: "stop_session", session_id: spawnedSessionId, cleanup: [] });
        await delay(500);
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
    if (ws && decoyRepoId) {
      try {
        ws.send({ type: "remove_repo", repo_id: decoyRepoId });
        await delay(200);
      } catch {
        /* best-effort */
      }
    }
    if (ws) await ws.close();
    if (fixtureRepo) await rm(fixtureRepo, { recursive: true, force: true });
    if (decoyRepo) await rm(decoyRepo, { recursive: true, force: true });
  });

  it("clicking a changed file opens a diff tab", async function () {
    // Click the file row (the label area, not the +/− action button).
    const row = await browser.$(
      `[data-testid=source-control-changes-row][data-file-path="README.md"]`,
    );
    await row.waitForExist({ timeout: 10_000 });
    await row.click();

    // A new tab pill with kind=diff should appear.
    await browser.waitUntil(
      async () => {
        const pills = await browser.$$('[data-testid=tab-pill][data-tab-kind=diff]');
        const count = await pills.length;
        return count >= 1;
      },
      {
        timeout: 5_000,
        timeoutMsg: "diff tab pill never appeared",
      },
    );

    // The DiffPane host renders.
    const pane = await browser.$("[data-testid=diff-pane]");
    await pane.waitForExist({ timeout: 5_000 });

    // The Monaco editor host element is present (Monaco fills it in
    // asynchronously; we just assert the container).
    const editor = await browser.$("[data-testid=diff-pane-editor]");
    await editor.waitForExist({ timeout: 5_000 });

    await browser.waitUntil(
      async () => {
        const activeRepo = await browser.$(
          "[data-testid=source-control-active-repo]",
        );
        const text = await activeRepo.getText();
        return text.includes("rt-e2e-scdiff") && !text.includes("rt-e2e-altrepo");
      },
      {
        timeout: 5_000,
        timeoutMsg: "diff tab activation moved sidebar away from changed repo",
      },
    );

    const selectedRow = await browser.$(
      `[data-testid=source-control-changes-row][data-file-path="README.md"]`,
    );
    await selectedRow.waitForExist({ timeout: 5_000 });
    expect(await selectedRow.getAttribute("class")).to.include("selected");
  });

  it("clicking the same file again focuses the existing tab (no duplicate)", async function () {
    const pillsBefore = await browser.$$('[data-testid=tab-pill][data-tab-kind=diff]');
    const before = await pillsBefore.length;

    const row = await browser.$(
      `[data-testid=source-control-changes-row][data-file-path="README.md"]`,
    );
    await row.click();
    await delay(500);

    const pillsAfter = await browser.$$('[data-testid=tab-pill][data-tab-kind=diff]');
    const after = await pillsAfter.length;
    expect(after).to.equal(before);
  });

  it("closing the diff tab removes it from the tab bar", async function () {
    const closer = await browser.$(
      '[data-testid=tab-pill][data-tab-kind=diff] [data-testid=tab-pill-close]',
    );
    await closer.waitForExist({ timeout: 5_000 });
    await closer.click();

    await browser.waitUntil(
      async () => {
        const pills = await browser.$$('[data-testid=tab-pill][data-tab-kind=diff]');
        const count = await pills.length;
        return count === 0;
      },
      {
        timeout: 5_000,
        timeoutMsg: "diff tab pill never went away",
      },
    );
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
