import { execFileSync } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";

import { browser } from "@wdio/globals";
import { expect } from "chai";

import { DaemonWsClient } from "../../../src/ws-client.js";
import { dismissLayoutChooser } from "../../../src/session-helpers.js";
import type {
  DaemonMessage,
  RepoEntry,
  SessionSnapshot,
} from "../../../src/types.js";

interface TabEntryStub {
  id: string;
  name: string;
}

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;

describe("spawn dialog placement", function () {
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

    fixtureRepo = await mkdtemp(join(tmpdir(), "rt-e2e-spawn-placement-"));
    await writeFile(
      join(fixtureRepo, "README.md"),
      "fixture for spawn placement e2e\n",
    );
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);

    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({
      type: "add_repo",
      path: fixtureRepo,
      name: "rt-e2e-spawn-placement-fixture",
    });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (r) =>
        r.path === fixtureRepo || r.path === fixtureRepo?.replace(/\\/g, "/"),
    );
    if (!fixture) {
      throw new Error("fixture repo was not registered");
    }
    registeredRepoId = fixture.id;

    const tabPromise = ws.waitFor(isTabUpdated, { timeoutMs: 5_000 });
    ws.send({ type: "create_tab", name: "Current", initial_session_id: null });
    const tab = await tabPromise;

    await browser.waitUntil(
      async () => {
        const tabs = await browser.$$('[data-testid="tab-pill"]');
        if ((await tabs.length) !== 1) return false;
        const active = await browser.$(
          `[data-testid="tab-pill"][data-tab-id="${tab.tab.id}"]`,
        );
        return active.isExisting();
      },
      { timeout: 10_000, timeoutMsg: "initial current tab did not render" },
    );
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
        /* best-effort cleanup */
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
    if (ws) await ws.close();
    if (fixtureRepo) await rm(fixtureRepo, { recursive: true, force: true });
  });

  it("defaults UI spawns into the current tab and keeps shell beside the agents", async function () {
    if (!ws) throw new Error("setup failed");

    const spawnButton = await browser.$('[data-testid="sidebar-add-session"]');
    await spawnButton.click();

    const dialog = await browser.$('[data-testid="spawn-dialog"]');
    await dialog.waitForExist({ timeout: 5_000 });

    const currentPlacement = await browser.$(
      '[data-testid="spawn-placement-current-tab"]',
    );
    const newTabPlacement = await browser.$(
      '[data-testid="spawn-placement-new-tab"]',
    );
    expect(await currentPlacement.getAttribute("aria-checked")).to.equal("true");
    expect(await newTabPlacement.getAttribute("aria-checked")).to.equal("false");

    await browser.saveScreenshot(".tmp/spawn-dialog-placement.png");

    const runtimeFieldset = await browser.$(
      '//input[@data-testid="spawn-agent-plain-shell"]/ancestor::fieldset[1]',
    );
    const runtimeText = await runtimeFieldset.getText();
    expect(runtimeText).to.include("claude");
    expect(runtimeText).to.include("codex");
    expect(runtimeText).to.include("Plain shell");

    const runModeFieldset = await browser.$(
      '//input[@data-testid="spawn-runmode-interactive"]/ancestor::fieldset[1]',
    );
    const runModeText = await runModeFieldset.getText();
    expect(runModeText).to.include("Interactive");
    expect(runModeText).to.not.include("Plain shell");

    await newTabPlacement.click();
    expect(await newTabPlacement.getAttribute("aria-checked")).to.equal("true");
    await currentPlacement.click();
    expect(await currentPlacement.getAttribute("aria-checked")).to.equal("true");

    const worktreeToggle = await browser.$(
      '[data-testid="spawn-single-worktree"]',
    );
    if (await worktreeToggle.isSelected()) {
      await worktreeToggle.click();
    }

    const spawned = ws.waitFor(isSessionUpdated, { timeoutMs: 20_000 });
    await browser.$('[data-testid="spawn-single-submit"]').click();
    const spawnMsg = await spawned;
    spawnedSessionId = spawnMsg.session.id;

    await browser.waitUntil(
      async () => {
        const tabs = await browser.$$('[data-testid="tab-pill"]');
        return (await tabs.length) === 1;
      },
      { timeout: 10_000, timeoutMsg: "spawn created an extra tab" },
    );

    const leaf = await browser.$(
      `[data-testid="sidebar-session"][data-session-id="${spawnedSessionId}"]`,
    );
    await leaf.waitForExist({ timeout: 10_000 });
    expect(await leaf.getAttribute("data-tab-binding-count")).to.equal("1");
  });
});

function isRepos(
  m: DaemonMessage,
): m is DaemonMessage & { type: "repos"; repos: RepoEntry[] } {
  return m.type === "repos";
}

function isTabUpdated(
  m: DaemonMessage,
): m is DaemonMessage & { type: "tab_updated"; tab: TabEntryStub } {
  return m.type === "tab_updated";
}

function isSessionUpdated(
  m: DaemonMessage,
): m is DaemonMessage & {
  type: "session_updated";
  session: SessionSnapshot;
} {
  return m.type === "session_updated";
}

function runGit(cwd: string, args: string[]): void {
  execFileSync("git", args, { cwd, stdio: "ignore" });
}
