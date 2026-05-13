import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { basename, join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

import { browser } from "@wdio/globals";
import { expect } from "chai";

import { DaemonWsClient } from "../../../src/ws-client.js";
import type { DaemonMessage, SessionSnapshot } from "../../../src/types.js";

type SessionUpdatedMessage = DaemonMessage & {
  type: "session_updated";
  session: SessionSnapshot;
};

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;
const repoRoot = resolve(
  fileURLToPath(new URL("../../../../..", import.meta.url)),
);

describe("standalone shell launch", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let shellDir: string | null = null;
  let modalShellDir: string | null = null;
  const spawnedSessionIds: string[] = [];

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    const parent = join(repoRoot, ".tmp", "e2e");
    await mkdir(parent, { recursive: true });
    shellDir = await mkdtemp(join(parent, "rt-e2e-standalone-shell-"));
  });

  after(async function () {
    if (ws) {
      for (const sessionId of [...spawnedSessionIds].reverse()) {
        try {
          ws.send({ type: "stop_session", session_id: sessionId, cleanup: [] });
          await delay(500);
          ws.send({
            type: "discard_session",
            session_id: sessionId,
            cleanup: [],
          });
          await delay(300);
        } catch {
          /* best-effort cleanup */
        }
      }
    }
    if (ws) await ws.close();
    if (shellDir) {
      await rm(shellDir, {
        recursive: true,
        force: true,
        maxRetries: 20,
        retryDelay: 100,
      });
    }
    if (modalShellDir) {
      await rm(modalShellDir, {
        recursive: true,
        force: true,
        maxRetries: 20,
        retryDelay: 100,
      });
    }
  });

  it("launches a quick standalone shell in the saved default folder", async function () {
    if (!ws || !shellDir) throw new Error("setup failed");
    await waitForAppDaemonConnection();
    await setStandaloneShellDefault(shellDir);

    const spawned = ws.waitFor(
      (msg): msg is SessionUpdatedMessage =>
        isSessionUpdated(msg) &&
        msg.session.kind === "standalone" &&
        msg.session.mode === "plain_shell" &&
        msg.session.members.length === 0,
      { timeoutMs: 20_000 },
    );

    const quickShell = await browser.$('[data-testid="sidebar-add-shell-default"]');
    await quickShell.waitForExist({ timeout: 5_000 });
    await quickShell.click();

    const msg = await spawned;
    spawnedSessionIds.push(msg.session.id);

    expect(msg.session.label).to.include(basename(shellDir));
    expect(msg.session.workspace_id).to.equal(null);

    const container = await browser.$('[data-testid="sidebar-container-standalone"]');
    await container.waitForExist({ timeout: 10_000 });

    const row = await browser.$(
      `[data-testid="sidebar-session"][data-session-id="${msg.session.id}"]`,
    );
    await row.waitForExist({ timeout: 10_000 });

    const pane = await browser.$(
      `[data-testid="session-pane"][data-session-id="${msg.session.id}"]`,
    );
    await pane.waitForExist({ timeout: 10_000 });

    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "standalone-shell-after.png"),
    );
  });

  it("launches a standalone shell from a specific folder modal", async function () {
    if (!ws) throw new Error("setup failed");
    await waitForAppDaemonConnection();

    const parent = join(repoRoot, ".tmp", "e2e");
    await mkdir(parent, { recursive: true });
    modalShellDir = await mkdtemp(join(parent, "rt-e2e-standalone-modal-"));

    const openModal = await browser.$('[data-testid="sidebar-add-shell-folder"]');
    await openModal.waitForExist({ timeout: 5_000 });
    await openModal.click();

    const dialog = await browser.$('[data-testid="standalone-shell-dialog"]');
    await dialog.waitForExist({ timeout: 5_000 });
    const pathInput = await browser.$('[data-testid="standalone-shell-path"]');
    await pathInput.setValue(modalShellDir);

    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "standalone-shell-dialog.png"),
    );

    const spawned = ws.waitFor(
      (msg): msg is SessionUpdatedMessage =>
        isSessionUpdated(msg) &&
        msg.session.kind === "standalone" &&
        msg.session.mode === "plain_shell" &&
        msg.session.label.includes(basename(modalShellDir!)),
      { timeoutMs: 20_000 },
    );

    await browser.$('[data-testid="standalone-shell-submit"]').click();
    const msg = await spawned;
    spawnedSessionIds.push(msg.session.id);

    const pane = await browser.$(
      `[data-testid="session-pane"][data-session-id="${msg.session.id}"]`,
    );
    await pane.waitForExist({ timeout: 10_000 });
  });
});

async function waitForAppDaemonConnection(): Promise<void> {
  const footer = await browser.$('[data-testid="daemon-footer"]');
  await footer.waitForExist({ timeout: 10_000 });
  await browser.waitUntil(
    async () => /:\d+/.test(await footer.getText()),
    {
      timeout: 20_000,
      timeoutMsg: "app websocket never rendered a daemon port",
    },
  );
}

async function setStandaloneShellDefault(path: string): Promise<void> {
  await browser.execute((defaultDir: string) => {
    const fallback = {
      version: 1,
      notifications: {
        awaiting_input: false,
        stopped: false,
        error: false,
      },
      sidebar: { default_view: "container" },
      spawn: {
        skip_permissions_default: false,
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
    };
    const raw = localStorage.getItem("rt.settings");
    const current = raw ? JSON.parse(raw) : fallback;
    const next = {
      ...fallback,
      ...current,
      spawn: {
        ...fallback.spawn,
        ...current.spawn,
        standalone_shell_default_dir: defaultDir,
      },
    };
    localStorage.setItem("rt.settings", JSON.stringify(next));
    const scope = globalThis as unknown as {
      dispatchEvent: (event: unknown) => boolean;
      CustomEvent: new (type: string, init: { detail: unknown }) => unknown;
    };
    scope.dispatchEvent(
      new scope.CustomEvent("rt:settings-changed", { detail: next }),
    );
  }, path);
}

function isSessionUpdated(msg: DaemonMessage): msg is SessionUpdatedMessage {
  if (msg.type !== "session_updated") return false;
  const session = (msg as { session?: unknown }).session;
  return typeof session === "object" && session !== null;
}
