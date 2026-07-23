/**
 * Sidebar tab-view + tab-pill bindings.
 *
 * Iter 5 of UX audit triage: the sidebar gained a `[Repos | Tabs]` view
 * toggle and inline tab pills on session leaves. This spec asserts:
 *
 * 1. The toggle is present and starts on "Repos" (default).
 * 2. A spawned session shows a `T:<tab-name>` pill in the leaf row.
 * 3. Switching to "Tabs" view renders tabs as top-level containers and
 *    the spawned session appears under its tab.
 * 4. Clicking the `[unbound]` pill on a session whose tab was closed
 *    fires `create_tab { initial_session_id }` and the session reappears
 *    in a new tab.
 *
 * Drag-and-drop between tabs is not asserted here — webdriver's synthetic
 * mouse events don't fire HTML5 dragstart/dragend reliably and the same
 * protocol path (`MovePane`) is already covered by the grid's pane-drag
 * code; what this spec uniquely covers is the *sidebar-as-source* affordance.
 */
import { execFileSync } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";

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
  SessionSnapshot,
} from "../../../src/types.js";

/// Minimal shape we need for this spec. The full TabEntry has a grid tree;
/// we only need `id` to fire `close_tab`. Mirror this if the wire format
/// evolves.
interface TabEntryStub {
  id: string;
  name: string;
}

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;

describe("sidebar tab-view", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let registeredRepoId: string | null = null;
  let spawnedSessionId: string | null = null;

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
    await dismissLayoutChooser(APP_BOOT_TIMEOUT);

    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    fixtureRepo = await mkdtemp(join(tmpdir(), "rt-e2e-tabview-"));
    await writeFile(join(fixtureRepo, "README.md"), "fixture for tab-view e2e\n");
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);

    // Reset sidebar view to default so tests start from a known state. The
    // view is persisted to localStorage; clearing it ensures we don't carry
    // over from a previous spec run.
    await browser.execute(() => {
      try {
        localStorage.removeItem("rt.sidebar.view");
      } catch {
        /* localStorage unavailable */
      }
    });
  });

  after(async function () {
    if (ws && spawnedSessionId) {
      try {
        ws.send({
          type: "stop_session",
          session_id: spawnedSessionId,
          cleanup: [],
        });
        await delay(300);
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
    if (ws) await ws.close();
    if (fixtureRepo) await rm(fixtureRepo, { recursive: true, force: true });
  });

  it("renders the [Repos | Tabs] view toggle with Repos selected by default", async function () {
    const toggle = await browser.$('[data-testid=sidebar-view-toggle]');
    await toggle.waitForExist({ timeout: 5_000 });

    const reposBtn = await browser.$('[data-testid=sidebar-view-container]');
    const tabsBtn = await browser.$('[data-testid=sidebar-view-tab]');
    expect(await reposBtn.isExisting()).to.be.true;
    expect(await tabsBtn.isExisting()).to.be.true;
    expect(await reposBtn.getAttribute("aria-selected")).to.equal("true");
    expect(await tabsBtn.getAttribute("aria-selected")).to.equal("false");
  });

  it("shows a tab pill on a spawned session leaf, and the same session under its tab in tab-view", async function () {
    expect(ws, "ws").to.not.be.null;
    expect(fixtureRepo, "fixtureRepo").to.not.be.null;
    if (!ws || !fixtureRepo) throw new Error("setup failed");
    const fixturePath = fixtureRepo;

    // Register the fixture + spawn a session.
    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({
      type: "add_repo",
      path: fixturePath,
      name: "rt-e2e-tabview-fixture",
    });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (r: RepoEntry) =>
        r.path === fixturePath || r.path === fixturePath.replace(/\\/g, "/"),
    );
    expect(fixture, "fixture repo registered").to.exist;
    registeredRepoId = fixture!.id;

    const session = await spawnSession(ws, {
      label: "tab-view",
      repoId: registeredRepoId,
      timeoutMs: 15_000,
    });
    spawnedSessionId = session.id;

    // Side-channel spawns don't trigger the React app's auto-tab logic
    // (that fires only for spawns the React client itself originated), so
    // we explicitly bind the session to a tab here. This mirrors what the
    // app does after a UI-driven spawn lands.
    ws.send({
      type: "create_tab",
      name: null,
      initial_session_id: spawnedSessionId,
    });

    await browser.waitUntil(
      async () => {
        const leaf = await browser.$(
          `[data-testid=sidebar-session][data-session-id="${spawnedSessionId}"]`,
        );
        if (!(await leaf.isExisting())) return false;
        const count = await leaf.getAttribute("data-tab-binding-count");
        return count !== null && count !== "0";
      },
      { timeout: 10_000, timeoutMsg: "leaf never gained a tab binding" },
    );

    const leaf = await browser.$(
      `[data-testid=sidebar-session][data-session-id="${spawnedSessionId}"]`,
    );
    const pill = await leaf.$('[data-testid=session-tab-pill]');
    expect(await pill.isExisting(), "bound pill is present").to.be.true;
    const pillText = await pill.getText();
    expect(pillText, "pill text").to.match(/^T:/);

    // Switch to Tabs view; assert the toggle reflects the change and the
    // session appears under a tab container.
    const tabsBtn = await browser.$('[data-testid=sidebar-view-tab]');
    await tabsBtn.click();
    expect(await tabsBtn.getAttribute("aria-selected")).to.equal("true");

    // The container row for the spawned session's tab should now exist.
    await browser.waitUntil(
      async () => {
        const containers = await browser.$$(
          '[data-testid=sidebar-container-tab]',
        );
        return (await containers.length) >= 1;
      },
      { timeout: 5_000, timeoutMsg: "tab containers never rendered" },
    );

    // The same session leaf should still exist, now under its tab.
    const leafInTabView = await browser.$(
      `[data-testid=sidebar-session][data-session-id="${spawnedSessionId}"]`,
    );
    expect(await leafInTabView.isExisting(), "leaf in tab view").to.be.true;
  });

  it("clicking [unbound] on an unbound session opens a fresh tab via create_tab", async function () {
    if (!ws || !spawnedSessionId) throw new Error("setup failed");

    // Close the session's tab so the session becomes unbound. We do this
    // via the side-channel WS so we can wait for the tabs message that
    // confirms the close landed.
    const tabsBefore = await ws.waitFor(
      (m): m is DaemonMessage & { type: "tabs"; tabs: TabEntryStub[] } =>
        m.type === "tabs",
      { timeoutMs: 1_000 },
    ).catch(() => null);
    let openTabs: TabEntryStub[];
    if (tabsBefore) {
      openTabs = tabsBefore.tabs;
    } else {
      // No spontaneous tabs message — ask explicitly.
      const tabsPromise = ws.waitFor(
        (m): m is DaemonMessage & { type: "tabs"; tabs: TabEntryStub[] } =>
          m.type === "tabs",
        { timeoutMs: 5_000 },
      );
      ws.send({ type: "list_tabs" });
      const tabsMsg = await tabsPromise;
      openTabs = tabsMsg.tabs;
    }
    expect(openTabs.length, "at least one tab open before close").to.be.at.least(
      1,
    );
    for (const t of openTabs) {
      ws.send({ type: "close_tab", tab_id: t.id });
    }

    // Wait for the React UI to reflect the leaf becoming unbound.
    await browser.waitUntil(
      async () => {
        const leaf = await browser.$(
          `[data-testid=sidebar-session][data-session-id="${spawnedSessionId}"]`,
        );
        if (!(await leaf.isExisting())) return false;
        const count = await leaf.getAttribute("data-tab-binding-count");
        return count === "0";
      },
      { timeout: 10_000, timeoutMsg: "leaf never became unbound" },
    );

    // Click the [unbound] pill. The handler should send
    // create_tab { initial_session_id }, which produces a new tab whose
    // grid references our session.
    const leaf = await browser.$(
      `[data-testid=sidebar-session][data-session-id="${spawnedSessionId}"]`,
    );
    const unboundPill = await leaf.$('[data-testid=session-tab-pill-unbound]');
    expect(await unboundPill.isExisting(), "unbound pill present").to.be.true;
    await unboundPill.click();

    // Wait for the leaf to flip back to bound.
    await browser.waitUntil(
      async () => {
        const reread = await browser.$(
          `[data-testid=sidebar-session][data-session-id="${spawnedSessionId}"]`,
        );
        const count = await reread.getAttribute("data-tab-binding-count");
        return count !== null && count !== "0";
      },
      { timeout: 5_000, timeoutMsg: "leaf never returned to bound" },
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
