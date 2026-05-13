/**
 * Session color customization coverage.
 *
 * A named preset color should apply to both the session's sidebar tree row and
 * its pane gutter/header.
 */
import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

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
const PRESET_COLOR = "#2f81f7";
const CUSTOM_COLOR = "#22c55e";
const repoRoot = resolve(
  fileURLToPath(new URL("../../../../..", import.meta.url)),
);

describe("session color customization", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let registeredRepoId: string | null = null;
  const spawnedSessionIds: string[] = [];

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });

    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    const parent = join(repoRoot, ".tmp", "e2e");
    await mkdir(parent, { recursive: true });
    fixtureRepo = await mkdtemp(join(parent, "rt-e2e-color-"));
    await writeFile(join(fixtureRepo, "README.md"), "fixture for color e2e\n");
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);

    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({
      type: "add_repo",
      path: fixtureRepo,
      name: "rt-e2e-color-fixture",
    });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (r: RepoEntry) =>
        r.path === fixtureRepo || r.path === fixtureRepo!.replace(/\\/g, "/"),
    );
    expect(fixture, "fixture repo registered").to.not.equal(undefined);
    registeredRepoId = fixture!.id;
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

  it("applies a named preset color to the sidebar row and pane", async function () {
    if (!ws || !registeredRepoId) throw new Error("setup failed");

    const spawnedSessionId = await spawnColorSession(
      ws,
      registeredRepoId,
      "color-session",
    );
    spawnedSessionIds.push(spawnedSessionId);

    const row = await sidebarRow(spawnedSessionId);
    await row.click();
    await sessionPane(spawnedSessionId);

    await row.click({ button: "right" });
    await openContextSubmenu("session-context-color-menu");
    const preset = await browser.$('[data-testid="session-context-color-0"]');
    await preset.waitForDisplayed({ timeout: 5_000 });
    const presetButtons = await browser.$$(".session-color-preset");
    expect(presetButtons.length).to.equal(12);
    expect(await preset.getText()).to.include("Blue");
    expect(await preset.getText()).to.include(PRESET_COLOR);
    expect(await preset.getAttribute("data-session-color")).to.equal(PRESET_COLOR);
    expect(await styleAttribute('[data-testid="session-context-color-0"]')).to.include(
      `--session-accent: ${PRESET_COLOR}`,
    );
    await preset.click();

    await browser.waitUntil(
      async () => {
        const updatedRow = await sidebarRow(spawnedSessionId!);
        const updatedPane = await sessionPane(spawnedSessionId!);
        const rowColor = await updatedRow.getAttribute("data-session-color");
        const paneColor = await updatedPane.getAttribute("data-session-color");
        return rowColor === PRESET_COLOR && paneColor === PRESET_COLOR;
      },
      { timeout: 10_000, timeoutMsg: "session color never applied to row and pane" },
    );

    expect(await styleAttribute(sidebarSelector(spawnedSessionId))).to.include(
      `--session-accent: ${PRESET_COLOR}`,
    );
    expect(await styleAttribute(paneSelector(spawnedSessionId))).to.include(
      `--session-accent: ${PRESET_COLOR}`,
    );
  });

  it("keeps a custom color after the app reloads session state", async function () {
    if (!ws || !registeredRepoId) throw new Error("setup failed");

    const spawnedSessionId = await spawnColorSession(
      ws,
      registeredRepoId,
      "custom-color-session",
    );
    spawnedSessionIds.push(spawnedSessionId);

    const row = await sidebarRow(spawnedSessionId);
    await row.click();
    await sessionPane(spawnedSessionId);

    await row.click({ button: "right" });
    await openContextSubmenu("session-context-color-menu");
    const colorApplied = ws.waitFor(
      (msg): msg is DaemonMessage & { type: "session_updated"; session: SessionSnapshot } =>
        isSessionUpdated(msg) &&
        msg.session.id === spawnedSessionId &&
        msg.session.accent_color === CUSTOM_COLOR,
      { timeoutMs: 10_000 },
    );
    await setCustomColor(CUSTOM_COLOR);
    await colorApplied;

    await waitForColor(spawnedSessionId, CUSTOM_COLOR);

    await browser.refresh();
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
    await waitForAppDaemonConnection();
    await waitForColor(spawnedSessionId, CUSTOM_COLOR);

    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "session-color-custom-reload.png"),
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

function runGit(cwd: string, args: string[]): void {
  execFileSync("git", args, { cwd, stdio: "ignore" });
}

async function spawnColorSession(
  ws: DaemonWsClient,
  repoId: string,
  label: string,
): Promise<string> {
  const spawnPromise = ws.waitFor(isSessionUpdated, { timeoutMs: 15_000 });
  ws.send({
    type: "spawn_session",
    label,
    target: {
      kind: "single",
      repo_id: repoId,
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

  return (await spawnPromise).session.id;
}

async function sidebarRow(sessionId: string) {
  const row = await browser.$(sidebarSelector(sessionId));
  await row.waitForExist({ timeout: 10_000 });
  return row;
}

async function sessionPane(sessionId: string) {
  const pane = await browser.$(paneSelector(sessionId));
  await pane.waitForExist({ timeout: 10_000 });
  return pane;
}

function sidebarSelector(sessionId: string): string {
  return `[data-testid=sidebar-session][data-session-id="${sessionId}"]`;
}

function paneSelector(sessionId: string): string {
  return `[data-testid=session-pane][data-session-id="${sessionId}"]`;
}

async function styleAttribute(selector: string): Promise<string> {
  const element = await browser.$(selector);
  return (await element.getAttribute("style")) ?? "";
}

async function setCustomColor(color: string): Promise<void> {
  const input = await browser.$('[data-testid="session-context-color-custom"]');
  await input.waitForDisplayed({ timeout: 5_000 });
  await browser.execute((nextColor: string) => {
    type ColorInput = {
      value: string;
      dispatchEvent: (event: unknown) => boolean;
    };
    type BrowserScope = {
      document: { querySelector: (selector: string) => ColorInput | null };
      Event: new (type: string, init: { bubbles: boolean }) => unknown;
    };
    const scope = globalThis as unknown as BrowserScope;
    const input = scope.document.querySelector(
      '[data-testid="session-context-color-custom"]',
    );
    if (!input) {
      throw new Error("custom color input not found");
    }
    const descriptor = Object.getOwnPropertyDescriptor(
      Object.getPrototypeOf(input),
      "value",
    );
    if (descriptor?.set) {
      descriptor.set.call(input, nextColor);
    } else {
      input.value = nextColor;
    }
    input.dispatchEvent(new scope.Event("input", { bubbles: true }));
    input.dispatchEvent(new scope.Event("change", { bubbles: true }));
  }, color);
}

async function openContextSubmenu(testId: string): Promise<void> {
  const trigger = await browser.$(`[data-testid="${testId}"]`);
  await trigger.waitForDisplayed({ timeout: 5_000 });
  await trigger.moveTo();
  await trigger.click();
}

async function waitForColor(sessionId: string, color: string): Promise<void> {
  await browser.waitUntil(
    async () => {
      const row = await sidebarRow(sessionId);
      const pane = await sessionPane(sessionId);
      const rowColor = await row.getAttribute("data-session-color");
      const paneColor = await pane.getAttribute("data-session-color");
      return rowColor === color && paneColor === color;
    },
    { timeout: 10_000, timeoutMsg: "custom session color was not rendered" },
  );
}

async function waitForAppDaemonConnection(): Promise<void> {
  const footer = await browser.$('[data-testid="daemon-footer"]');
  await footer.waitForExist({ timeout: 10_000 });
  await browser.waitUntil(async () => /:\d+/.test(await footer.getText()), {
    timeout: 20_000,
    timeoutMsg: "app websocket never rendered a daemon port",
  });
}
