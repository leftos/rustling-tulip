import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { platform } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

import { browser } from "@wdio/globals";
import { expect } from "chai";

import { DaemonWsClient } from "../../../src/ws-client.js";
import type { DaemonMessage, SessionSnapshot } from "../../../src/types.js";

type SessionUpdatedMessage = DaemonMessage & {
  type: "session_updated";
  session: SessionSnapshot;
};

interface TerminalLinkEvent {
  kind: "url" | "path";
  text: string;
  target: string;
  line: number | null;
  column: number | null;
  baseDirs: string[];
}

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;
const SESSION_OUTPUT_TIMEOUT = 20_000;
const CTRL_KEY = "\uE009";
const repoRoot = resolve(
  fileURLToPath(new URL("../../../../..", import.meta.url)),
);

describe("terminal links", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let fixtureDir: string | null = null;
  let spawnedSessionId: string | null = null;

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    const parent = join(repoRoot, ".tmp", "e2e");
    await mkdir(parent, { recursive: true });
    fixtureDir = await mkdtemp(join(parent, "rt-e2e-terminal-links-"));
    await mkdir(join(fixtureDir, "src"), { recursive: true });
    await writeFile(
      join(fixtureDir, "src", "linked-file.ts"),
      "one\ntwo\nthree\n",
      "utf8",
    );
  });

  after(async function () {
    await removeLinkListener();
    if (ws && spawnedSessionId) {
      try {
        ws.send({ type: "stop_session", session_id: spawnedSessionId, cleanup: [] });
        await delay(500);
        ws.send({
          type: "discard_session",
          session_id: spawnedSessionId,
          cleanup: [],
        });
      } catch {
        /* best-effort cleanup */
      }
    }
    if (ws) await ws.close();
    if (fixtureDir) {
      await rm(fixtureDir, {
        recursive: true,
        force: true,
        maxRetries: 20,
        retryDelay: 100,
      });
    }
  });

  it("opens URLs and local paths only on Ctrl-click", async function () {
    if (!ws || !fixtureDir) throw new Error("setup failed");
    const cwd = fixtureDir;
    await installLinkListener();

    const url = "https://example.com/rustling-tulip/docs";
    const filePath = "src/linked-file.ts:3:2";
    spawnedSessionId = await spawnStandaloneShell(ws, cwd);
    await openSessionPane(spawnedSessionId);
    await waitForBufferText(spawnedSessionId, cwd, SESSION_OUTPUT_TIMEOUT);

    ws.send({
      type: "send_input",
      session_id: spawnedSessionId,
      data_b64: Buffer.from(linkOutputCommand(url, filePath)).toString("base64"),
    });
    await waitForBufferText(spawnedSessionId, url, SESSION_OUTPUT_TIMEOUT);
    await waitForBufferText(spawnedSessionId, filePath, SESSION_OUTPUT_TIMEOUT);

    await focusTerminal(spawnedSessionId);
    await clickTerminalText(spawnedSessionId, url, false);
    await delay(300);
    expect(await terminalLinkEvents()).to.have.length(0);

    await clickTerminalText(spawnedSessionId, url, true);
    const urlEvent = await waitForTerminalLinkEvent(spawnedSessionId, 0);
    expect(urlEvent.kind).to.equal("url");
    expect(urlEvent.target).to.equal(url);

    await clickTerminalText(spawnedSessionId, filePath, true);
    const pathEvent = await waitForTerminalLinkEvent(spawnedSessionId, 1);
    expect(pathEvent.kind).to.equal("path");
    expect(pathEvent.target).to.equal("src/linked-file.ts");
    expect(pathEvent.line).to.equal(3);
    expect(pathEvent.column).to.equal(2);
    expect(pathEvent.baseDirs.some((dir) => pathsEqual(dir, cwd))).to.equal(true);
  });
});

async function spawnStandaloneShell(
  ws: DaemonWsClient,
  cwd: string,
): Promise<string> {
  const spawned = ws.waitFor(
    (msg): msg is SessionUpdatedMessage =>
      isSessionUpdated(msg) &&
      msg.session.kind === "standalone" &&
      msg.session.mode === "plain_shell",
    { timeoutMs: 20_000 },
  );
  ws.send({
    type: "spawn_session",
    label: "links",
    target: { kind: "standalone", cwd },
    mode: "plain_shell",
    initial_prompt: null,
    dangerously_skip_permissions: false,
    agent: "claude",
    model: null,
    permission_mode: null,
    codex_sandbox: null,
    extra_env: [],
    prompt_injector: null,
  });
  return (await spawned).session.id;
}

async function openSessionPane(sessionId: string): Promise<void> {
  const row = await browser.$(
    `[data-testid=sidebar-session][data-session-id="${sessionId}"]`,
  );
  await row.waitForExist({ timeout: 10_000 });
  await row.click();
  const pane = await browser.$(
    `[data-testid=session-pane][data-session-id="${sessionId}"]`,
  );
  await pane.waitForExist({ timeout: 10_000 });
}

async function installLinkListener(): Promise<void> {
  await browser.execute(`
    globalThis.__rtTerminalLinkEvents = [];
    globalThis.__rtTerminalLinkListener = (event) => {
      event.preventDefault();
      globalThis.__rtTerminalLinkEvents.push(event.detail);
    };
    window.addEventListener("rt:terminal-link-open", globalThis.__rtTerminalLinkListener);
  `);
}

async function removeLinkListener(): Promise<void> {
  await browser.execute(`
    if (globalThis.__rtTerminalLinkListener) {
      window.removeEventListener("rt:terminal-link-open", globalThis.__rtTerminalLinkListener);
      delete globalThis.__rtTerminalLinkListener;
    }
    delete globalThis.__rtTerminalLinkEvents;
  `).catch(() => undefined);
}

async function terminalLinkEvents(): Promise<TerminalLinkEvent[]> {
  return (await browser.execute(`
    return globalThis.__rtTerminalLinkEvents ?? [];
  `)) as unknown as TerminalLinkEvent[];
}

async function waitForTerminalLinkEvent(
  sessionId: string,
  index: number,
): Promise<TerminalLinkEvent> {
  const deadline = Date.now() + 5_000;
  while (Date.now() < deadline) {
    const events = await terminalLinkEvents();
    if (events.length > index) return events[index]!;
    await delay(100);
  }
  const debug = await terminalLinkDebugState(sessionId);
  throw new Error(
    `terminal link event ${index} was not dispatched\n${JSON.stringify(debug, null, 2)}`,
  );
}

async function terminalLinkDebugState(sessionId: string): Promise<unknown> {
  return browser.execute(`
    const sessionId = ${JSON.stringify(sessionId)};
    const term = globalThis.__rt_terms && globalThis.__rt_terms.get(sessionId);
    const container = document.querySelector(
      '[data-testid="terminal-container"][data-session-id="' + sessionId + '"]'
    );
    const core = term && term._core;
    const linkifier =
      core && (core._linkifier2 || core._linkifier || core.linkifier2 || core.linkifier);
    const linkProviderService =
      core && (
        core._linkProviderService ||
        core.linkProviderService ||
        core._instantiationService?._services?.get?.("ILinkProviderService")
      );
    const currentLink = linkifier && linkifier.currentLink;
    return {
      hasTerm: Boolean(term),
      containerCtrlLinkMode: container?.dataset?.ctrlLinkMode ?? null,
      xtermPointerClass: container?.querySelector(".xterm")?.classList.contains("xterm-cursor-pointer") ?? false,
      coreKeys: core ? Object.keys(core).filter((key) => key.toLowerCase().includes("link")) : [],
      providerCount: linkProviderService?.linkProviders?.length ?? null,
      currentLinkText: currentLink?.link?.text ?? null,
      currentLinkRange: currentLink?.link?.range ?? null,
      currentLinkDecorations: currentLink?.state?.decorations ?? null,
      events: globalThis.__rtTerminalLinkEvents ?? [],
    };
  `);
}

async function focusTerminal(sessionId: string): Promise<void> {
  await browser.execute(`
    const term = globalThis.__rt_terms && globalThis.__rt_terms.get(${JSON.stringify(sessionId)});
    if (!term) throw new Error("terminal not found");
    term.focus();
  `);
}

async function clickTerminalText(
  sessionId: string,
  needle: string,
  ctrl: boolean,
): Promise<void> {
  const point = (await browser.execute(`
    const sessionId = ${JSON.stringify(sessionId)};
    const needle = ${JSON.stringify(needle)};
    const term = globalThis.__rt_terms && globalThis.__rt_terms.get(sessionId);
    if (!term) throw new Error("terminal not found");
    const container = document.querySelector(
      '[data-testid="terminal-container"][data-session-id="' + sessionId + '"]'
    );
    if (!container) throw new Error("terminal container not found");
    const screen = container.querySelector(".xterm-screen") || container;
    const rect = screen.getBoundingClientRect();
    const dims =
      term._core?._renderService?._dimensions ??
      term._core?._renderService?.dimensions;
    const cellWidth = dims?.css?.cell?.width || rect.width / term.cols;
    const cellHeight = dims?.css?.cell?.height || rect.height / term.rows;
    const buffer = term.buffer.active;
    for (let i = 0; i < buffer.length; i++) {
      const line = buffer.getLine(i);
      const text = line ? line.translateToString(true) : "";
      const col = text.indexOf(needle);
      if (col === -1) continue;
      const row = i - buffer.viewportY;
      if (row < 0 || row >= term.rows) continue;
      const innerCol = Math.min(Math.max(needle.length - 1, 0), 5);
      return {
        x: Math.round(rect.left + (col + innerCol + 0.5) * cellWidth),
        y: Math.round(rect.top + (row + 0.5) * cellHeight),
      };
    }
    throw new Error("text not visible in terminal: " + needle);
  `)) as unknown as { x: number; y: number };

  try {
    if (ctrl) {
      await browser.performActions([
        {
          type: "key",
          id: "keyboard",
          actions: [{ type: "keyDown", value: CTRL_KEY }],
        },
      ]);
      await delay(100);
    }
    await browser.performActions([
      {
        type: "pointer",
        id: "mouse",
        parameters: { pointerType: "mouse" },
        actions: [
          { type: "pointerMove", duration: 0, origin: "viewport", x: point.x, y: point.y },
          { type: "pause", duration: 150 },
          { type: "pointerDown", button: 0 },
          { type: "pointerUp", button: 0 },
        ],
      },
    ]);
  } finally {
    if (ctrl) {
      await browser.performActions([
        {
          type: "key",
          id: "keyboard",
          actions: [{ type: "keyUp", value: CTRL_KEY }],
        },
      ]).catch(() => undefined);
    }
    await browser.releaseActions();
  }
}

async function waitForBufferText(
  sessionId: string,
  needle: string,
  timeoutMs: number,
): Promise<string> {
  const deadline = Date.now() + timeoutMs;
  let lastSeen = "";
  while (Date.now() < deadline) {
    const text = (await browser.execute(`
      const term = globalThis.__rt_terms && globalThis.__rt_terms.get(${JSON.stringify(sessionId)});
      if (!term) return "";
      const buf = term.buffer.active;
      const lines = [];
      for (let i = 0; i < buf.length; i++) {
        const line = buf.getLine(i);
        if (line) lines.push(line.translateToString(true));
      }
      return lines.join("\\n");
    `)) as unknown as string;
    lastSeen = text;
    if (text.includes(needle)) return text;
    await delay(250);
  }
  throw new Error(
    `xterm buffer for ${sessionId} never contained "${needle}" within ${timeoutMs}ms.\n` +
      `Last seen:\n${lastSeen.slice(-1_000)}`,
  );
}

function linkOutputCommand(url: string, filePath: string): string {
  if (platform() === "win32") {
    return `Write-Output "LINK ${url}"; Write-Output "FILE ${filePath}"\r`;
  }
  return `printf 'LINK ${url}\\nFILE ${filePath}\\n'\r`;
}

function pathsEqual(actual: string, expected: string): boolean {
  return normalizePathForCompare(actual) === normalizePathForCompare(expected);
}

function normalizePathForCompare(path: string): string {
  const windowsLike = /^[A-Za-z]:[\\/]/.test(path) || path.includes("\\");
  const normalized = windowsLike ? path.replace(/\//g, "\\") : path;
  const trimmed =
    normalized.length > 3
      ? normalized.replace(windowsLike ? /\\+$/g : /\/+$/g, "")
      : normalized;
  return process.platform === "win32" ? trimmed.toLowerCase() : trimmed;
}

function isSessionUpdated(msg: DaemonMessage): msg is SessionUpdatedMessage {
  if (msg.type !== "session_updated") return false;
  const session = (msg as { session?: unknown }).session;
  return typeof session === "object" && session !== null;
}
