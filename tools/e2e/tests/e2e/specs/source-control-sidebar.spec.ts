/**
 * Phase A of the Source Control sidebar (`docs/plans/source-control-sidebar.md`).
 *
 * The per-session `Terminal | Git` toggle is gone; git access now happens
 * through an activity-bar-driven global sidebar. This spec confirms:
 *
 *   1. The ActivityBar renders both icons and Sessions is selected on first run.
 *   2. Switching to Source Control reveals the new sidebar (and the Sessions
 *      one disappears).
 *   3. The selection is persisted to localStorage (re-reading after switch
 *      returns "source-control").
 *   4. With a repo registered, the SourceControlSidebar's Changes tab is the
 *      default and the active-repo line names the registered repo.
 *
 * The actual git data fetch (repo_status round-trip) is exercised through
 * existing daemon-side coverage; this spec is about the UI host swap.
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

describe("source-control sidebar (Phase A)", function () {
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
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);

    // Reset activity-bar to default so a previous spec run can't carry over.
    await browser.execute(() => {
      try {
        localStorage.removeItem("rt.activity");
      } catch {
        /* unavailable */
      }
    });

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

    expect(await sessionsBtn.isExisting()).to.be.true;
    expect(await scBtn.isExisting()).to.be.true;
    expect(await sessionsBtn.getAttribute("aria-selected")).to.equal("true");
    expect(await scBtn.getAttribute("aria-selected")).to.equal("false");

    // The Sessions sidebar should be rendering, not the source-control one.
    const sidebar = await browser.$("[data-testid=sidebar]");
    expect(await sidebar.isExisting()).to.be.true;
    const sc = await browser.$("[data-testid=source-control-sidebar]");
    expect(await sc.isExisting()).to.be.false;
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
    expect(await sidebar.isExisting(), "Sessions sidebar hidden").to.be.false;

    // Persisted to localStorage under the documented key.
    const stored = (await browser.execute(
      `return localStorage.getItem("rt.activity");`,
    )) as unknown as string | null;
    expect(stored).to.equal("source-control");
  });

  it("source-control sidebar shows the registered repo + Changes tab by default", async function () {
    const sc = await browser.$("[data-testid=source-control-sidebar]");
    expect(await sc.isExisting()).to.be.true;

    // Active-repo header should name the fixture.
    const headerText = await sc.getText();
    expect(headerText).to.include("rt-e2e-scsidebar");

    // Changes tab is the default selected one.
    const changesTab = await browser.$(
      "[data-testid=source-control-tab-changes]",
    );
    const historyTab = await browser.$(
      "[data-testid=source-control-tab-history]",
    );
    expect(await changesTab.getAttribute("class")).to.match(/active/);
    expect(await historyTab.getAttribute("class")).to.not.match(/active/);

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
