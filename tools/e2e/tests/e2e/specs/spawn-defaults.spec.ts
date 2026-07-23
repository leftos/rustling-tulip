import { execFileSync } from "node:child_process";
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

describe("spawn defaults", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let registeredRepoId: string | null = null;
  const spawnedSessionIds: string[] = [];

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
    await dismissLayoutChooser(APP_BOOT_TIMEOUT);
    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    const parent = join(repoRoot, ".tmp", "e2e");
    await mkdir(parent, { recursive: true });
    fixtureRepo = await mkdtemp(join(parent, "rt-e2e-spawn-defaults-"));
    await writeFile(join(fixtureRepo, "README.md"), "fixture for spawn defaults e2e\n");
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);

    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({
      type: "add_repo",
      path: fixtureRepo,
      name: "rt-e2e-spawn-defaults-fixture",
    });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (r: RepoEntry) =>
        r.path === fixtureRepo || r.path === fixtureRepo!.replace(/\\/g, "/"),
    );
    if (!fixture) throw new Error("fixture repo was not registered");
    registeredRepoId = fixture.id;
  });

  after(async function () {
    if (ws) {
      for (const sessionId of [...spawnedSessionIds].reverse()) {
        try {
          ws.send({ type: "stop_session", session_id: sessionId, cleanup: [] });
          await delay(500);
          ws.send({ type: "discard_session", session_id: sessionId, cleanup: [] });
          await delay(300);
        } catch {
          /* best-effort */
        }
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

  it("defaults trusted launch off for fresh settings", async function () {
    if (!registeredRepoId) throw new Error("setup failed");

    await seedSettings(null);
    await reloadAndWaitForRepo(registeredRepoId);

    const dialog = await openSpawnDialog();
    const authorityToggle = await dialog.$(
      '[data-testid="spawn-authority-toggle"]',
    );
    expect(await authorityToggle.getText()).to.include("Trusted launch");
    const skipPerms = await dialog.$('[data-testid="spawn-skip-perms"]');
    expect(await skipPerms.isSelected()).to.equal(false);
    const inactiveWarning = await dialog.$(
      '[data-testid="spawn-trusted-launch-warning"]',
    );
    expect(await inactiveWarning.isExisting()).to.equal(false);

    await skipPerms.click();
    const warning = await dialog.$(
      '[data-testid="spawn-trusted-launch-warning"]',
    );
    await warning.waitForExist({ timeout: 5_000 });
    expect((await warning.getText()).toLowerCase()).to.include("trusted launch");
    await assertStoredSkipPermissionsDefault(false);
    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "spawn-trusted-launch-warning.png"),
    );
    await closeSpawnDialog();
  });

  it("preserves an existing saved trusted launch default", async function () {
    if (!registeredRepoId) throw new Error("setup failed");

    await seedSettings(true);
    await reloadAndWaitForRepo(registeredRepoId);

    const dialog = await openSpawnDialog();
    const skipPerms = await dialog.$('[data-testid="spawn-skip-perms"]');
    expect(await skipPerms.isSelected()).to.equal(true);
    const warning = await dialog.$(
      '[data-testid="spawn-trusted-launch-warning"]',
    );
    await warning.waitForExist({ timeout: 5_000 });
    expect(await warning.getText()).to.include("--dangerously-skip-permissions");
    await assertStoredSkipPermissionsDefault(true);
    await closeSpawnDialog();
  });

  it("keeps elevated state out of the compact sidebar row", async function () {
    if (!ws || !registeredRepoId) throw new Error("setup failed");

    await reloadAndWaitForRepo(registeredRepoId);

    const session = await spawnElevatedSession(
      ws,
      registeredRepoId,
      "trusted-launch-session",
    );
    spawnedSessionIds.push(session.id);

    const row = await sidebarRow(session.id);
    expect(await row.getAttribute("data-session-elevated")).to.equal("true");
    const rowRuntime = await row.$('[data-testid="session-runtime-tag"]');
    await rowRuntime.waitForExist({ timeout: 5_000 });
    expect((await rowRuntime.getText()).toLowerCase()).to.equal("claude");
    expect((await rowRuntime.getAttribute("title"))?.toLowerCase()).to.include(
      "approval prompts were bypassed",
    );
    const rowBadge = await row.$('[data-testid="session-authority-badge"]');
    expect(await rowBadge.isExisting()).to.equal(false);

    await openSessionPane(session.id);
    const pane = await sessionPane(session.id);
    expect(await pane.getAttribute("data-session-elevated")).to.equal("true");
    const paneBadge = await pane.$('[data-testid="session-authority-badge"]');
    await paneBadge.waitForExist({ timeout: 5_000 });
    expect((await paneBadge.getText()).toLowerCase()).to.include("trusted");

    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "spawn-elevated-session-badge.png"),
    );
  });

  it("opens the spawn dialog before replaying elevated launch-last", async function () {
    if (!ws || !registeredRepoId) throw new Error("setup failed");

    await reloadAndWaitForRepo(registeredRepoId);

    const session = await spawnElevatedSession(
      ws,
      registeredRepoId,
      "trusted-launch-last-seed",
    );
    spawnedSessionIds.push(session.id);

    await openRepoContextMenu(registeredRepoId);
    const launchLast = await browser.$(
      '[data-testid="container-launch-last-new"]',
    );
    await launchLast.waitForExist({ timeout: 5_000 });
    await browser.waitUntil(async () => launchLast.isEnabled(), {
      timeout: 10_000,
      timeoutMsg: "launch-last did not become enabled",
    });
    await launchLast.click();

    const dialog = await browser.$('[data-testid="spawn-dialog"]');
    await dialog.waitForExist({ timeout: 10_000 });
    const skipPerms = await dialog.$('[data-testid="spawn-skip-perms"]');
    expect(await skipPerms.isSelected()).to.equal(true);
    const warning = await dialog.$(
      '[data-testid="spawn-trusted-launch-warning"]',
    );
    await warning.waitForExist({ timeout: 5_000 });
    expect((await warning.getText()).toLowerCase()).to.include("trusted launch");
    await closeSpawnDialog();
  });

  it("replays launch last from the compact repo row action", async function () {
    if (!ws || !registeredRepoId) throw new Error("setup failed");

    await reloadAndWaitForRepo(registeredRepoId);

    const seed = await spawnRepoSession(
      ws,
      registeredRepoId,
      "compact-launch-last-seed",
      false,
    );
    spawnedSessionIds.push(seed.id);

    const repoRow = await repoContainer(registeredRepoId);
    const summary = await repoRow.$('[data-testid="container-launch-summary"]');
    await summary.waitForExist({ timeout: 5_000 });
    expect(await summary.getText()).to.include("Claude");
    expect(await summary.getText()).to.include("main");

    const quickLaunch = await repoRow.$(
      '[data-testid="container-launch-last-quick"]',
    );
    await quickLaunch.waitForExist({ timeout: 5_000 });
    const knownSessionIds = new Set(spawnedSessionIds);
    const replayPromise = ws.waitFor(
      (msg): msg is SessionUpdatedMessage =>
        isSessionUpdated(msg) &&
        !knownSessionIds.has(msg.session.id) &&
        msg.session.members.some((m) => m.repo_id === registeredRepoId),
      { timeoutMs: 15_000 },
    );
    await quickLaunch.click();

    const replayed = (await replayPromise).session;
    spawnedSessionIds.push(replayed.id);
    await (await sidebarRow(replayed.id)).waitForExist({ timeout: 10_000 });
    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "spawn-compact-launch-last.png"),
    );
  });

  it("opens the full spawn dialog from compact launch-last edit", async function () {
    if (!ws || !registeredRepoId) throw new Error("setup failed");

    await reloadAndWaitForRepo(registeredRepoId);

    const seed = await spawnRepoSession(
      ws,
      registeredRepoId,
      "compact-launch-edit-seed",
      false,
    );
    spawnedSessionIds.push(seed.id);

    await openRepoContextMenu(registeredRepoId);
    const summary = await browser.$(
      '[data-testid="container-launch-last-summary"]',
    );
    await summary.waitForExist({ timeout: 5_000 });
    await browser.waitUntil(
      async () => (await summary.getText()).includes("main"),
      {
        timeout: 10_000,
        timeoutMsg: "launch-last summary did not update to the saved main branch",
      },
    );
    expect(await summary.getText()).to.include("Claude");

    const edit = await browser.$('[data-testid="container-launch-last-edit"]');
    await browser.waitUntil(async () => edit.isEnabled(), {
      timeout: 5_000,
      timeoutMsg: "launch-last edit did not become enabled",
    });
    await edit.click();

    const dialog = await browser.$('[data-testid="spawn-dialog"]');
    await dialog.waitForExist({ timeout: 10_000 });
    const target = await dialog.$('[data-testid="spawn-target-select"]');
    expect(await target.getValue()).to.equal(`repo:${registeredRepoId}`);
    const claude = await dialog.$('[data-testid="spawn-agent-claude"]');
    expect(await claude.isSelected()).to.equal(true);
    const branch = await dialog.$('[data-testid="spawn-single-branch"]');
    expect(await branch.getValue()).to.equal("main");
    const worktree = await dialog.$('[data-testid="spawn-single-worktree"]');
    expect(await worktree.isSelected()).to.equal(false);
    const baseBranch = await dialog.$('[data-testid="spawn-single-base-branch"]');
    expect(await baseBranch.isExisting()).to.equal(false);
    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "spawn-compact-launch-edit.png"),
    );
    await closeSpawnDialog();
  });
});

async function spawnRepoSession(
  ws: DaemonWsClient,
  repoId: string,
  label: string,
  dangerouslySkipPermissions: boolean,
): Promise<SessionSnapshot> {
  const spawnPromise = ws.waitFor(
    (msg): msg is SessionUpdatedMessage =>
      isSessionUpdated(msg) &&
      msg.session.label === label &&
      Boolean(msg.session.elevated_authority) === dangerouslySkipPermissions,
    { timeoutMs: 15_000 },
  );
  ws.send(buildSpawnMessage({ label, repoId, dangerouslySkipPermissions }));

  return (await spawnPromise).session;
}

async function spawnElevatedSession(
  ws: DaemonWsClient,
  repoId: string,
  label: string,
): Promise<SessionSnapshot> {
  return spawnRepoSession(ws, repoId, label, true);
}

function isRepos(
  m: DaemonMessage,
): m is DaemonMessage & { type: "repos"; repos: RepoEntry[] } {
  return m.type === "repos";
}

type SessionUpdatedMessage = DaemonMessage & {
  type: "session_updated";
  session: SessionSnapshot;
};

function isSessionUpdated(msg: DaemonMessage): msg is SessionUpdatedMessage {
  return msg.type === "session_updated";
}

function runGit(cwd: string, args: string[]): void {
  execFileSync("git", args, { cwd, stdio: "ignore" });
}

async function seedSettings(skipPermissionsDefault: boolean | null): Promise<void> {
  await browser.execute((value: boolean | null) => {
    const key = "rt.settings";
    localStorage.removeItem("rt.sidebar.view");
    if (value === null) {
      localStorage.removeItem(key);
      return;
    }
    localStorage.setItem(
      key,
      JSON.stringify({
        version: 1,
        notifications: {
          awaiting_input: false,
          stopped: false,
          error: false,
        },
        sidebar: { default_view: "container" },
        spawn: {
          skip_permissions_default: value,
          default_permission_mode: null,
          default_codex_sandbox: null,
          standalone_shell_default_dir: null,
        },
        terminal: {
          font_size: 13,
          font_family: null,
          font_bold: false,
          copy_on_selection: true,
        },
      }),
    );
  }, skipPermissionsDefault);
}

async function reloadAndWaitForRepo(repoId: string): Promise<void> {
  await browser.refresh();
  const root = await browser.$("[data-testid=app-root]");
  await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
  await waitForAppDaemonConnection();

  const repoRow = await browser.$(
    `[data-testid="sidebar-container-repo"][data-container-id="${repoId}"]`,
  );
  await repoRow.waitForExist({ timeout: 10_000 });
}

async function waitForAppDaemonConnection(): Promise<void> {
  const footer = await browser.$('[data-testid="daemon-footer"]');
  await footer.waitForExist({ timeout: 10_000 });
  await browser.waitUntil(async () => /:\d+/.test(await footer.getText()), {
    timeout: 20_000,
    timeoutMsg: "app websocket never rendered a daemon port",
  });
}

async function openSpawnDialog() {
  const spawnButton = await browser.$('[data-testid="sidebar-add-session"]');
  await spawnButton.waitForExist({ timeout: 10_000 });
  await browser.waitUntil(async () => spawnButton.isEnabled(), {
    timeout: 10_000,
    timeoutMsg: "spawn button did not become enabled",
  });
  await spawnButton.click();
  const dialog = await browser.$('[data-testid="spawn-dialog"]');
  await dialog.waitForExist({ timeout: 10_000 });
  return dialog;
}

async function closeSpawnDialog(): Promise<void> {
  const close = await browser.$('[data-testid="spawn-close"]');
  await close.click();
  const dialog = await browser.$('[data-testid="spawn-dialog"]');
  await dialog.waitForExist({ timeout: 5_000, reverse: true });
}

async function openRepoContextMenu(repoId: string): Promise<void> {
  const repoRow = await repoContainer(repoId);
  await repoRow.waitForExist({ timeout: 10_000 });
  await repoRow.click({ button: "right" });
}

async function repoContainer(repoId: string) {
  return browser.$(
    `[data-testid="sidebar-container-repo"][data-container-id="${repoId}"]`,
  );
}

async function sidebarRow(sessionId: string) {
  const row = await browser.$(
    `[data-testid="sidebar-session"][data-session-id="${sessionId}"]`,
  );
  await row.waitForExist({ timeout: 10_000 });
  return row;
}

async function sessionPane(sessionId: string) {
  const pane = await browser.$(
    `[data-testid="session-pane"][data-session-id="${sessionId}"]`,
  );
  await pane.waitForExist({ timeout: 10_000 });
  return pane;
}

async function assertStoredSkipPermissionsDefault(expected: boolean): Promise<void> {
  const stored = await browser.execute(() => {
    const raw = localStorage.getItem("rt.settings");
    if (!raw) return null;
    const parsed = JSON.parse(raw) as {
      spawn?: { skip_permissions_default?: unknown };
    };
    return parsed.spawn?.skip_permissions_default ?? null;
  });
  expect(stored).to.equal(expected);
}
