/**
 * Session appearance customization coverage.
 *
 * Session-level appearance should apply live to the sidebar row and pane, and
 * should survive an app reload through daemon session state.
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
const PRESET_COLOR = "#111318";
const CUSTOM_ACCENT = "#22c55e";
const CUSTOM_BACKGROUND = "#0b1020";
const CUSTOM_FRAME = "#1a1024";
const repoRoot = resolve(
  fileURLToPath(new URL("../../../../..", import.meta.url)),
);

describe("session appearance customization", function () {
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
    fixtureRepo = await mkdtemp(join(parent, "rt-e2e-appearance-"));
    await writeFile(join(fixtureRepo, "README.md"), "fixture for appearance e2e\n");
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);

    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({
      type: "add_repo",
      path: fixtureRepo,
      name: "rt-e2e-appearance-fixture",
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

  it("applies a preset accent to the sidebar row and pane", async function () {
    if (!ws || !registeredRepoId) throw new Error("setup failed");

    const spawnedSessionId = await spawnAppearanceSession(
      ws,
      registeredRepoId,
      "appearance-session",
    );
    spawnedSessionIds.push(spawnedSessionId);

    const row = await sidebarRow(spawnedSessionId);
    await row.click();
    await sessionPane(spawnedSessionId);

    await openSessionAppearance(spawnedSessionId);
    const preset = await browser.$('[data-testid="appearance-accent_color-graphite"]');
    await preset.waitForDisplayed({ timeout: 5_000 });
    await preset.click();

    await waitForAppearance(spawnedSessionId, {
      accent: PRESET_COLOR,
    });

    expect(await styleAttribute(sidebarSelector(spawnedSessionId))).to.include(
      `--session-accent: ${PRESET_COLOR}`,
    );
    expect(await styleAttribute(paneSelector(spawnedSessionId))).to.include(
      `--session-accent: ${PRESET_COLOR}`,
    );
    await closeAppearanceModal();
  });

  it("keeps custom session appearance after the app reloads state", async function () {
    if (!ws || !registeredRepoId) throw new Error("setup failed");

    const spawnedSessionId = await spawnAppearanceSession(
      ws,
      registeredRepoId,
      "custom-appearance-session",
    );
    spawnedSessionIds.push(spawnedSessionId);

    const row = await sidebarRow(spawnedSessionId);
    await row.click();
    await sessionPane(spawnedSessionId);

    await openSessionAppearance(spawnedSessionId);
    const appearanceApplied = ws.waitFor(
      (msg): msg is DaemonMessage & { type: "session_updated"; session: SessionSnapshot } =>
        isSessionUpdated(msg) &&
        msg.session.id === spawnedSessionId &&
        msg.session.appearance?.accent_color === CUSTOM_ACCENT &&
        msg.session.appearance.terminal_background_color === CUSTOM_BACKGROUND &&
        msg.session.appearance.terminal_frame_color === CUSTOM_FRAME,
      { timeoutMs: 10_000 },
    );
    await setAppearanceColor("accent_color", CUSTOM_ACCENT);
    await setAppearanceColor("terminal_background_color", CUSTOM_BACKGROUND);
    await setAppearanceColor("terminal_frame_color", CUSTOM_FRAME);
    await appearanceApplied;

    await waitForAppearance(spawnedSessionId, {
      accent: CUSTOM_ACCENT,
      background: CUSTOM_BACKGROUND,
      frame: CUSTOM_FRAME,
    });
    await closeAppearanceModal();

    await browser.refresh();
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
    await waitForAppDaemonConnection();
    await waitForAppearance(spawnedSessionId, {
      accent: CUSTOM_ACCENT,
      background: CUSTOM_BACKGROUND,
      frame: CUSTOM_FRAME,
    });

    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "session-appearance-custom-reload.png"),
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

async function spawnAppearanceSession(
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

async function openSessionAppearance(sessionId: string): Promise<void> {
  const row = await sidebarRow(sessionId);
  await row.click({ button: "right" });
  const item = await browser.$('[data-testid="session-context-appearance"]');
  await item.waitForDisplayed({ timeout: 5_000 });
  await item.click();
  const modal = await browser.$('[data-testid="appearance-modal"]');
  await modal.waitForDisplayed({ timeout: 5_000 });
}

async function setAppearanceColor(field: string, color: string): Promise<void> {
  const selector = `[data-testid="appearance-${field}-custom"]`;
  const input = await browser.$(selector);
  await input.waitForDisplayed({ timeout: 5_000 });
  await browser.execute(
    ({ inputSelector, nextColor }: { inputSelector: string; nextColor: string }) => {
      type ColorInput = {
        value: string;
        dispatchEvent: (event: unknown) => boolean;
      };
      type BrowserScope = {
        document: { querySelector: (selector: string) => ColorInput | null };
        Event: new (type: string, init: { bubbles: boolean }) => unknown;
      };
      const scope = globalThis as unknown as BrowserScope;
      const input = scope.document.querySelector(inputSelector);
      if (!input) {
        throw new Error(`appearance input not found: ${inputSelector}`);
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
    },
    { inputSelector: selector, nextColor: color },
  );
}

async function closeAppearanceModal(): Promise<void> {
  const close = await browser.$('[data-testid="appearance-close"]');
  await close.waitForDisplayed({ timeout: 5_000 });
  await close.click();
}

async function waitForAppearance(
  sessionId: string,
  expected: {
    accent?: string;
    background?: string;
    frame?: string;
  },
): Promise<void> {
  await browser.waitUntil(
    async () => {
      const row = await sidebarRow(sessionId);
      const pane = await sessionPane(sessionId);
      const rowColor = await row.getAttribute("data-session-color");
      const paneColor = await pane.getAttribute("data-session-color");
      const background = await pane.getAttribute("data-terminal-background");
      const frame = await pane.getAttribute("data-terminal-frame");
      return (
        (expected.accent === undefined ||
          (rowColor === expected.accent && paneColor === expected.accent)) &&
        (expected.background === undefined || background === expected.background) &&
        (expected.frame === undefined || frame === expected.frame)
      );
    },
    { timeout: 10_000, timeoutMsg: "session appearance was not rendered" },
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
