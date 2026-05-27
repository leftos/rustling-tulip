/**
 * The per-session `Terminal | Git` toggle is gone; git access now happens
 * through an activity-bar-driven global sidebar. This spec confirms:
 *
 *   1. The ActivityBar renders both icons and Sessions is selected on first run.
 *   2. Switching to Source Control reveals the new sidebar (and the Sessions
 *      one disappears).
 *   3. The selection is persisted to localStorage (re-reading after switch
 *      returns "source-control").
 *   4. With a repo registered, the SourceControlSidebar renders Changes and
 *      History in one split sidebar and the active-repo line names the repo.
 *
 * The actual git data fetch (repo_status round-trip) is exercised through
 * existing daemon-side coverage; this spec is about the UI host swap.
 */
import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

import { browser } from "@wdio/globals";
import { expect } from "chai";

import { DaemonWsClient } from "../../../src/ws-client.js";
import type { DaemonMessage, RepoEntry } from "../../../src/types.js";

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;
const repoRoot = resolve(
  fileURLToPath(new URL("../../../../..", import.meta.url)),
);

interface HistoryRowGeometry {
  chipLeft: number;
  chipRight: number;
  rowRight: number;
  subjectLeft: number;
  subjectRight: number;
}

interface OverflowProbe {
  selector: string;
  clientWidth: number;
  scrollWidth: number;
}

describe("source-control sidebar", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let registeredRepoId: string | null = null;

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });

    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    fixtureRepo = await mkdtemp(join(tmpdir(), "rt-e2e-scsidebar-"));
    await writeFile(join(fixtureRepo, "README.md"), "fixture for sc-sidebar e2e\n");
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "Jane E2E"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, [
      "commit",
      "-m",
      "initial fixture commit with a long subject that should truncate before the author chip",
    ]);

    // Reset activity-bar to default so a previous spec run can't carry over.
    await browser.execute(() => {
      try {
        localStorage.removeItem("rt.activity");
        localStorage.removeItem("rt.sourceControl.repoOverride");
        localStorage.removeItem("rt:split:source-control.sidebar");
        localStorage.removeItem("rt:split:source-control.history");
      } catch {
        /* unavailable */
      }
    });
    await mkdir(join(repoRoot, ".tmp", "e2e"), { recursive: true });

    // Register the fixture so the source-control sidebar has something to
    // show beyond the empty state.
    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({ type: "add_repo", path: fixtureRepo, name: "rt-e2e-scsidebar" });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (r: RepoEntry) =>
        r.path === fixtureRepo || r.path === fixtureRepo!.replace(/\\/g, "/"),
    );
    if (!fixture) throw new Error("fixture repo never registered");
    registeredRepoId = fixture.id;
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

  it("ActivityBar renders both icons with Sessions active by default", async function () {
    const bar = await browser.$("[data-testid=activity-bar]");
    await bar.waitForExist({ timeout: 5_000 });

    const sessionsBtn = await browser.$("[data-testid=activity-btn-sessions]");
    const scBtn = await browser.$("[data-testid=activity-btn-source-control]");

    expect(await sessionsBtn.isExisting()).to.equal(true);
    expect(await scBtn.isExisting()).to.equal(true);
    expect(await sessionsBtn.getAttribute("aria-selected")).to.equal("true");
    expect(await scBtn.getAttribute("aria-selected")).to.equal("false");

    // The Sessions sidebar should be rendering, not the source-control one.
    const sidebar = await browser.$("[data-testid=sidebar]");
    expect(await sidebar.isExisting()).to.equal(true);
    const sc = await browser.$("[data-testid=source-control-sidebar]");
    expect(await sc.isExisting()).to.equal(false);
  });

  it("clicking Source Control swaps the sidebar and persists the choice", async function () {
    const scBtn = await browser.$("[data-testid=activity-btn-source-control]");
    await scBtn.click();

    // Active state flips on the activity bar.
    await browser.waitUntil(
      async () => (await scBtn.getAttribute("aria-selected")) === "true",
      { timeout: 2_000, timeoutMsg: "Source-control button never became active" },
    );

    // Source-control sidebar renders; the Sessions one is gone.
    const sc = await browser.$("[data-testid=source-control-sidebar]");
    await sc.waitForExist({ timeout: 5_000 });
    const sidebar = await browser.$("[data-testid=sidebar]");
    expect(await sidebar.isExisting(), "Sessions sidebar hidden").to.equal(
      false,
    );

    // Persisted to localStorage under the documented key.
    const stored = (await browser.execute(
      `return localStorage.getItem("rt.activity");`,
    )) as unknown as string | null;
    expect(stored).to.equal("source-control");
  });

  it("source-control sidebar shows the registered repo with changes and history together", async function () {
    const sc = await browser.$("[data-testid=source-control-sidebar]");
    expect(await sc.isExisting()).to.equal(true);

    // Active-repo header should name the fixture.
    const activeRepo = await browser.$("[data-testid=source-control-active-repo]");
    await activeRepo.waitForExist({ timeout: 5_000 });
    expect(await activeRepo.getText()).to.include("rt-e2e-scsidebar");

    const tabControls = await browser.$$(
      "[data-testid=source-control-tab-changes], [data-testid=source-control-tab-history]",
    );
    expect(await tabControls.length).to.equal(0);

    const changes = await browser.$("[data-testid=source-control-changes-list]");
    await changes.waitForExist({ timeout: 5_000 });

    const history = await browser.$("[data-testid=source-control-history-list]");
    await history.waitForExist({ timeout: 5_000 });
    const changesLocation = await changes.getLocation();
    const historyLocation = await history.getLocation();
    expect(historyLocation.y).to.be.greaterThan(changesLocation.y);

    // Working tree clean → the empty state copy renders.
    await browser.waitUntil(
      async () => {
        const text = await sc.getText();
        return text.includes("working tree clean");
      },
      {
        timeout: 5_000,
        timeoutMsg: "changes list never showed working-tree-clean",
      },
    );

    const historyRow = await browser.$("[data-testid=source-control-history-row]");
    await historyRow.waitForExist({ timeout: 5_000 });
  });

  it("clean source-control state has no horizontal overflow", async function () {
    const sc = await browser.$("[data-testid=source-control-sidebar]");
    await sc.waitForExist({ timeout: 5_000 });

    const commitBox = await browser.$("[data-testid=source-control-commit-message]");
    expect(await commitBox.isExisting()).to.equal(false);

    await assertNoHorizontalOverflow();
    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "source-control-clean.png"),
    );
  });

  it("history rows append the author chip without covering the subject", async function () {
    const historyRow = await browser.$("[data-testid=source-control-history-row]");
    await historyRow.waitForExist({ timeout: 5_000 });

    const author = await historyRow.$("[data-testid=source-control-history-author]");
    await author.waitForExist({ timeout: 5_000 });
    expect(await author.getText()).to.equal("Jane E2E");

    const geometry = (await browser.execute(`
      const row = document.querySelector(
        "[data-testid=source-control-history-row]"
      );
      const subject = row?.querySelector(
        "[data-testid=source-control-history-subject]"
      );
      const chip = row?.querySelector(
        "[data-testid=source-control-history-author]"
      );
      if (!row || !subject || !chip) {
        return null;
      }
      const rowRect = row.getBoundingClientRect();
      const subjectRect = subject.getBoundingClientRect();
      const chipRect = chip.getBoundingClientRect();
      return {
        chipLeft: chipRect.left,
        chipRight: chipRect.right,
        rowRight: rowRect.right,
        subjectLeft: subjectRect.left,
        subjectRight: subjectRect.right,
      };
    `)) as unknown as HistoryRowGeometry | null;
    if (!geometry) throw new Error("history row geometry unavailable");
    expect(geometry.chipLeft).to.be.greaterThan(geometry.subjectLeft);
    expect(geometry.subjectRight).to.be.at.most(geometry.chipLeft);
    expect(geometry.chipRight).to.be.at.most(geometry.rowRight);
  });

  it("dirty source-control state lays out without horizontal overflow", async function () {
    if (!fixtureRepo) throw new Error("setup");
    await writeFile(
      join(fixtureRepo, "README.md"),
      "fixture for sc-sidebar e2e\nmodified for density screenshot\n",
    );

    const refresh = await browser.$("[data-testid=source-control-refresh]");
    await refresh.click();

    const worktreeBucket = await browser.$(
      "[data-testid=source-control-bucket-worktree]",
    );
    await worktreeBucket.waitForExist({ timeout: 5_000 });

    // History no longer auto-collapses when changes appear — Changes and
    // History share the split and the user re-allocates space via the
    // divider. Just verify the layout doesn't overflow.
    const toggle = await browser.$("[data-testid=source-control-history-toggle]");
    await toggle.waitForExist({ timeout: 5_000 });

    await assertNoHorizontalOverflow();
    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "source-control-dirty.png"),
    );
  });

  it("expanded history-selected state has no horizontal overflow", async function () {
    const toggle = await browser.$("[data-testid=source-control-history-toggle]");
    await toggle.waitForExist({ timeout: 5_000 });
    // Ensure the panel is expanded regardless of starting state, then keep
    // it expanded for the rest of the assertions.
    const initialExpanded = (await toggle.getAttribute("aria-expanded")) === "true";
    if (!initialExpanded) {
      await toggle.click();
    }
    await browser.waitUntil(
      async () => (await toggle.getAttribute("aria-expanded")) === "true",
      {
        timeout: 2_000,
        timeoutMsg: "history section did not expand",
      },
    );

    const row = await browser.$("[data-testid=source-control-history-row]");
    await row.waitForExist({ timeout: 5_000 });
    await row.click();

    const diff = await browser.$("[data-testid=source-control-history-diff]");
    await diff.waitForExist({ timeout: 5_000 });
    await browser.waitUntil(
      async () => {
        const text = await diff.getText();
        return text.includes("Files:");
      },
      {
        timeout: 5_000,
        timeoutMsg: "history detail diff did not load",
      },
    );

    await assertNoHorizontalOverflow();
    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "source-control-history-selected.png"),
    );
  });

  it("the in-pane Terminal/Git toggle is gone (no session-view-git testid anywhere)", async function () {
    // Even with no session spawned, the testid string should not appear in
    // the DOM. Any future regression that re-introduces the toggle will
    // surface here.
    const stragglers = await browser.$$("[data-testid=session-view-git]");
    expect(await stragglers.length).to.equal(0);
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

async function assertNoHorizontalOverflow(): Promise<void> {
  const overflow = (await browser.execute(`
    const selectors = [
      "[data-testid=source-control-sidebar]",
      "[data-testid=source-control-changes-list]",
      "[data-testid=source-control-history-panel]",
      "[data-testid=source-control-history-list]",
      "[data-testid=source-control-history-diff]",
    ];
    return selectors
      .map((selector) => {
        const node = document.querySelector(selector);
        if (!node) return null;
        return {
          selector,
          clientWidth: node.clientWidth,
          scrollWidth: node.scrollWidth,
        };
      })
      .filter((entry) => entry !== null)
      .filter((entry) => entry.scrollWidth > entry.clientWidth + 1);
  `)) as unknown as OverflowProbe[];
  expect(overflow).to.deep.equal([]);
}
