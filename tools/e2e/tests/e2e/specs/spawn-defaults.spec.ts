import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
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

describe("spawn defaults", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let registeredRepoId: string | null = null;

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
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
    expect(fixture, "fixture repo registered").to.exist;
    registeredRepoId = fixture!.id;
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
    if (fixtureRepo) {
      await rm(fixtureRepo, {
        recursive: true,
        force: true,
        maxRetries: 20,
        retryDelay: 100,
      });
    }
  });

  it("defaults skip permissions off for fresh settings", async function () {
    if (!registeredRepoId) throw new Error("setup failed");

    await seedSettings(null);
    await reloadAndWaitForRepo(registeredRepoId);

    const dialog = await openSpawnDialog();
    const skipPerms = await dialog.$('[data-testid="spawn-skip-perms"]');
    expect(await skipPerms.isSelected()).to.equal(false);
    await assertStoredSkipPermissionsDefault(false);
    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "spawn-default-safe-after.png"),
    );
    await closeSpawnDialog();
  });

  it("preserves an existing saved skip-permissions default", async function () {
    if (!registeredRepoId) throw new Error("setup failed");

    await seedSettings(true);
    await reloadAndWaitForRepo(registeredRepoId);

    const dialog = await openSpawnDialog();
    const skipPerms = await dialog.$('[data-testid="spawn-skip-perms"]');
    expect(await skipPerms.isSelected()).to.equal(true);
    await assertStoredSkipPermissionsDefault(true);
    await closeSpawnDialog();
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
