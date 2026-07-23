import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

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
  TabEntry,
} from "../../../src/types.js";

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;
const repoRoot = resolve(
  fileURLToPath(new URL("../../../../..", import.meta.url)),
);

describe("pane controls discoverability", function () {
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

    const parent = join(repoRoot, ".tmp", "e2e");
    await mkdir(parent, { recursive: true });
    fixtureRepo = await mkdtemp(join(parent, "rt-e2e-pane-controls-"));
    await writeFile(join(fixtureRepo, "README.md"), "pane controls fixture\n");
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);

    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({
      type: "add_repo",
      path: fixtureRepo,
      name: "rt-e2e-pane-controls",
    });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (r) =>
        r.path === fixtureRepo || r.path === fixtureRepo?.replace(/\\/g, "/"),
    );
    if (!fixture) throw new Error("fixture repo never registered");
    registeredRepoId = fixture.id;

    const session = await spawnSession(ws, {
      label: "pane-controls-shell",
      repoId: registeredRepoId,
      mode: "plain_shell",
      timeoutMs: 20_000,
    });
    spawnedSessionId = session.id;

    const tabPromise = ws.waitFor(isTabUpdated, { timeoutMs: 5_000 });
    ws.send({
      type: "create_tab",
      name: "Pane controls",
      initial_session_id: spawnedSessionId,
    });
    await tabPromise;

    const pane = await browser.$(
      `[data-testid=session-pane][data-session-id="${spawnedSessionId}"]`,
    );
    await pane.waitForExist({ timeout: 10_000 });
  });

  after(async function () {
    if (ws && spawnedSessionId) {
      try {
        ws.send({ type: "stop_session", session_id: spawnedSessionId, cleanup: [] });
        await delay(500);
        ws.send({
          type: "discard_session",
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
    if (fixtureRepo) {
      await rm(fixtureRepo, {
        recursive: true,
        force: true,
        maxRetries: 20,
        retryDelay: 100,
      });
    }
  });

  it("names icon-only activity and pane-header controls", async function () {
    await assertNamedIconButton('[data-testid="activity-btn-sessions"]');
    await assertNamedIconButton('[data-testid="activity-btn-source-control"]');
    await assertNamedIconButton('[data-testid="pane-split-right"]');
    await assertNamedIconButton('[data-testid="pane-split-down"]');
    await assertNamedIconButton('[data-testid="pane-extract"]');
    await assertNamedIconButton('[data-testid="pane-close"]');

    const splitGroup = await browser.$(
      '//button[@data-testid="pane-split-right"]/ancestor::*[@role="group"][1]',
    );
    expect(await splitGroup.getAttribute("aria-label")).to.equal("Split pane");

    await assertNoSessionHeaderOverflow();
    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "pane-controls-normal.png"),
    );
  });

  it("names icon-only sidebar and tab controls", async function () {
    await assertNamedIconButton('[data-testid="sidebar-settings-btn"]');
    await assertNamedIconButton('[data-testid="tab-bar-new"]');
    await assertNamedIconButton('[data-testid="tab-pill-close"]');

    const settingsButton = await browser.$('[data-testid="sidebar-settings-btn"]');
    await settingsButton.click();
    await assertNamedIconButton(".modal-close");
    const modalClose = await browser.$(".modal-close");
    await modalClose.click();

    const sessionsButton = await browser.$('[data-testid="activity-btn-sessions"]');
    await sessionsButton.click();
    const containerViewButton = await browser.$(
      '[data-testid="sidebar-view-container"]',
    );
    await containerViewButton.click();

    await assertNamedIconButton('[data-testid="container-launch-last-quick"]');
    await assertNamedIconButton('[data-testid="sidebar-remove-repo"]');
    await assertNoTabBarOverflow();
  });

  it("keeps pane controls legible after splitting into smaller panes", async function () {
    const splitRight = await browser.$('[data-testid="pane-split-right"]');
    await splitRight.click();
    await waitForEmptyPaneControls();

    const splitDown = await browser.$('[data-testid="pane-split-down"]');
    await splitDown.click();
    await waitForEmptyPaneControls();

    await assertNamedIconButton('[data-testid="grid-pane-split-right"]');
    await assertNamedIconButton('[data-testid="grid-pane-split-down"]');
    await assertNamedIconButton('[data-testid="grid-pane-close"]');
    await assertNoSessionHeaderOverflow();
    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "pane-controls-small.png"),
    );
  });

  it("explains disabled sidebar toolbar actions", async function () {
    if (!ws || !spawnedSessionId || !registeredRepoId) {
      throw new Error("fixture session and repo must exist before cleanup test");
    }

    ws.send({ type: "stop_session", session_id: spawnedSessionId, cleanup: [] });
    await delay(500);
    ws.send({
      type: "discard_session",
      session_id: spawnedSessionId,
      cleanup: [],
    });
    await delay(300);

    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({ type: "remove_repo", repo_id: registeredRepoId });
    await reposPromise;
    spawnedSessionId = null;
    registeredRepoId = null;

    const addSession = await browser.$('[data-testid="sidebar-add-session"]');
    await browser.waitUntil(
      async () => (await addSession.getAttribute("disabled")) !== null,
      {
        timeout: 5_000,
        timeoutMsg: "expected add-session toolbar action to become disabled",
      },
    );
    expect(await addSession.getText()).to.contain("needs repo");
    expect(await addSession.getAttribute("title")).to.equal(
      "Register a repo to spawn repo-tied sessions.",
    );
    expect(await addSession.getAttribute("aria-describedby")).to.equal(
      "sidebar-add-session-disabled-reason",
    );

    const addWorkspace = await browser.$(
      '[data-testid="sidebar-add-workspace"]',
    );
    expect(await addWorkspace.getText()).to.contain("needs 2 repos");
    expect(await addWorkspace.getAttribute("title")).to.equal(
      "Register at least 2 repos to create a workspace.",
    );
    expect(await addWorkspace.getAttribute("aria-describedby")).to.equal(
      "sidebar-add-workspace-disabled-reason",
    );
    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "sidebar-disabled-toolbar.png"),
    );
  });
});

function isRepos(
  m: DaemonMessage,
): m is DaemonMessage & { type: "repos"; repos: RepoEntry[] } {
  return m.type === "repos";
}

function isSessionUpdated(
  m: DaemonMessage,
): m is DaemonMessage & { type: "session_updated"; session: SessionSnapshot } {
  return m.type === "session_updated";
}

function isTabUpdated(
  m: DaemonMessage,
): m is DaemonMessage & { type: "tab_updated"; tab: TabEntry } {
  return m.type === "tab_updated";
}

function runGit(cwd: string, args: string[]): void {
  execFileSync("git", args, { cwd, stdio: "ignore" });
}

async function assertNamedIconButton(selector: string): Promise<void> {
  const button = await browser.$(selector);
  await button.waitForExist({ timeout: 5_000 });
  const ariaLabel = await button.getAttribute("aria-label");
  const title = await button.getAttribute("title");
  expect(ariaLabel, `${selector} aria-label`).to.be.a("string").and.not.empty;
  expect(title, `${selector} title`).to.be.a("string").and.not.empty;

  const icon = await button.$("svg.rt-icon");
  await icon.waitForExist({ timeout: 5_000 });
}

async function waitForEmptyPaneControls(): Promise<void> {
  const split = await browser.$('[data-testid="grid-pane-split-right"]');
  await split.waitForExist({ timeout: 5_000 });
}

async function assertNoSessionHeaderOverflow(): Promise<void> {
  const overflow = (await browser.execute(`
    return Array.from(document.querySelectorAll(".session-header"))
      .map((node, index) => ({
        index,
        clientWidth: node.clientWidth,
        scrollWidth: node.scrollWidth,
      }))
      .filter((entry) => entry.scrollWidth > entry.clientWidth + 1);
  `)) as unknown as Array<{
    index: number;
    clientWidth: number;
    scrollWidth: number;
  }>;
  expect(overflow).to.deep.equal([]);
}

async function assertNoTabBarOverflow(): Promise<void> {
  const overflow = (await browser.execute(`
    return Array.from(document.querySelectorAll(".tab-bar, .tab-pill"))
      .map((node, index) => ({
        index,
        className: node.className,
        clientWidth: node.clientWidth,
        scrollWidth: node.scrollWidth,
      }))
      .filter((entry) => entry.scrollWidth > entry.clientWidth + 1);
  `)) as unknown as Array<{
    index: number;
    className: string;
    clientWidth: number;
    scrollWidth: number;
  }>;
  expect(overflow).to.deep.equal([]);
}
