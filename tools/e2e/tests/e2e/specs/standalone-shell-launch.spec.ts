import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { basename, join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

import { browser } from "@wdio/globals";
import { expect } from "chai";

import { DaemonWsClient } from "../../../src/ws-client.js";
import { dismissLayoutChooser } from "../../../src/session-helpers.js";
import type {
  DaemonMessage,
  RepoEntry,
  SessionSnapshot,
} from "../../../src/types.js";

type SessionUpdatedMessage = DaemonMessage & {
  type: "session_updated";
  session: SessionSnapshot;
};

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;
const SESSION_OUTPUT_TIMEOUT = 20_000;
const repoRoot = resolve(
  fileURLToPath(new URL("../../../../..", import.meta.url)),
);

describe("standalone shell launch", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let shellDir: string | null = null;
  let modalShellDir: string | null = null;
  const spawnedSessionIds: string[] = [];
  const registeredRepoIds: string[] = [];

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
    await dismissLayoutChooser(APP_BOOT_TIMEOUT);
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
      for (const repoId of [...registeredRepoIds].reverse()) {
        try {
          ws.send({ type: "remove_repo", repo_id: repoId });
          await delay(200);
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
    expectPathEqual(msg.session.current_cwd ?? null, shellDir);

    const container = await waitForStandaloneContainer(msg.session.id, shellDir);
    const addRepoButton = await container.$('[data-testid="sidebar-cwd-add-repo"]');
    await addRepoButton.waitForExist({ timeout: 5_000 });
    expect(
      await (await container.$('[data-testid="sidebar-cwd-add-workspace"]')).isExisting(),
      "workspace action hidden without .code-workspace",
    ).to.equal(false);

    const row = await browser.$(
      `[data-testid="sidebar-session"][data-session-id="${msg.session.id}"]`,
    );
    await row.waitForExist({ timeout: 10_000 });

    const pane = await browser.$(
      `[data-testid="session-pane"][data-session-id="${msg.session.id}"]`,
    );
    await pane.waitForExist({ timeout: 10_000 });
    await waitForRenderedShellStatus(msg.session.id, "idle");
    await waitForBufferText(
      msg.session.id,
      basename(shellDir),
      SESSION_OUTPUT_TIMEOUT,
    );

    const working = waitForSessionStatus(ws, msg.session.id, "working");
    ws.send({
      type: "send_input",
      session_id: msg.session.id,
      data_b64: Buffer.from(shellActivityCommand()).toString("base64"),
    });
    await working;
    await waitForRenderedShellStatus(msg.session.id, "working");
    await waitForBufferText(
      msg.session.id,
      "rt-shell-working-20",
      SESSION_OUTPUT_TIMEOUT,
    );
    await waitForSessionStatus(ws, msg.session.id, "idle");
    await waitForRenderedShellStatus(msg.session.id, "idle");

    const nestedDir = join(shellDir, "nested-cwd");
    await mkdir(nestedDir, { recursive: true });
    initGitRepo(nestedDir);
    await writeFile(
      join(nestedDir, "nested.code-workspace"),
      JSON.stringify({ folders: [{ path: "." }] }),
      "utf8",
    );

    const cwdUpdated = waitForSessionCwd(ws, msg.session.id, nestedDir);
    ws.send({
      type: "send_input",
      session_id: msg.session.id,
      data_b64: Buffer.from(shellCdCommand(nestedDir)).toString("base64"),
    });
    await cwdUpdated;
    const nestedContainer = await waitForStandaloneContainer(
      msg.session.id,
      nestedDir,
    );
    await waitForVisibleShellLabel(msg.session.id, basename(nestedDir));
    await (
      await nestedContainer.$('[data-testid="sidebar-cwd-add-workspace"]')
    ).waitForExist({
      timeout: 10_000,
      timeoutMsg: "workspace action did not appear for .code-workspace cwd",
    });
    const registered = waitForRepo(ws, nestedDir);
    await nestedContainer.$('[data-testid="sidebar-cwd-add-repo"]').click();
    const registeredRepoId = (await registered).id;
    registeredRepoIds.push(registeredRepoId);

    await waitForContainer("repo", registeredRepoId);
    await waitForSessionInsideContainer("repo", registeredRepoId, msg.session.id);
    await waitForStandaloneContainerGone(msg.session.id);

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
    await waitForBufferText(
      msg.session.id,
      basename(modalShellDir),
      SESSION_OUTPUT_TIMEOUT,
    );
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

async function waitForRepo(
  ws: DaemonWsClient,
  path: string,
): Promise<RepoEntry> {
  const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
  const repos = await reposPromise;
  const repo = repos.repos.find((candidate) =>
    pathsEqual(candidate.path, path),
  );
  if (!repo) throw new Error(`repo was not registered: ${path}`);
  return repo;
}

function initGitRepo(path: string): void {
  execFileSync("git", ["init", "-b", "main"], { cwd: path, stdio: "ignore" });
}

function isRepos(
  msg: DaemonMessage,
): msg is DaemonMessage & { type: "repos"; repos: RepoEntry[] } {
  return msg.type === "repos";
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

async function waitForContainer(
  kind: "repo" | "workspace" | "cwd",
  containerId: string,
): Promise<void> {
  const container = await browser.$(
    `[data-testid="sidebar-container-${kind}"][data-container-id="${containerId}"]`,
  );
  await container.waitForExist({
    timeout: 10_000,
    timeoutMsg: `${kind}:${containerId} did not appear`,
  });
}

async function waitForSessionInsideContainer(
  kind: "repo" | "workspace" | "cwd",
  containerId: string,
  sessionId: string,
): Promise<void> {
  await browser.waitUntil(
    async () => {
      const container = await browser.$(
        `[data-testid="sidebar-container-${kind}"][data-container-id="${containerId}"]`,
      );
      if (!(await container.isExisting())) return false;
      const item = await container.$("./ancestor::li[1]");
      if (!(await item.isExisting())) return false;
      const row = await item.$(
        `[data-testid="sidebar-session"][data-session-id="${sessionId}"]`,
      );
      return row.isExisting();
    },
    {
      timeout: 10_000,
      timeoutMsg: `${sessionId} did not move under ${kind}:${containerId}`,
    },
  );
}

async function waitForStandaloneContainer(
  sessionId: string,
  expectedPath: string,
): Promise<WebdriverIO.Element> {
  await browser.waitUntil(
    async () => {
      const container = await findStandaloneContainer(sessionId);
      const path = container
        ? await container.getAttribute("data-container-path")
        : null;
      return pathsEqual(path, expectedPath);
    },
    {
      timeout: 10_000,
      timeoutMsg: `standalone container ${sessionId} did not move to ${expectedPath}`,
    },
  );
  const found = await findStandaloneContainer(sessionId);
  if (!found) {
    throw new Error(`standalone container missing after wait: ${sessionId}`);
  }
  return found;
}

async function findStandaloneContainer(
  sessionId: string,
): Promise<WebdriverIO.Element | null> {
  const container = await browser.$(
    `[data-testid="sidebar-container-standalone"][data-container-id="${sessionId}"]`,
  );
  if (!(await container.isExisting())) return null;
  return container as unknown as WebdriverIO.Element;
}

async function waitForStandaloneContainerGone(sessionId: string): Promise<void> {
  await browser.waitUntil(
    async () => {
      const container = await findStandaloneContainer(sessionId);
      return container === null;
    },
    {
      timeout: 10_000,
      timeoutMsg: `standalone container still present after regroup: ${sessionId}`,
    },
  );
}

async function waitForBufferText(
  sessionId: string,
  needle: string,
  timeoutMs: number,
): Promise<string> {
  const deadline = Date.now() + timeoutMs;
  let lastSeen = "";
  while (Date.now() < deadline) {
    const text = (await browser.execute(
      `
      const w = window;
      const term = w.__rt_terms && w.__rt_terms.get(${JSON.stringify(sessionId)});
      if (!term) return "";
      const buf = term.buffer.active;
      // Join wrapped continuation rows without a separator so a prompt line
      // that soft-wraps (e.g. a long cwd path in a narrow pane) stays a
      // single contiguous logical line — otherwise a needle split across the
      // wrap boundary would never match.
      let text = "";
      for (let i = 0; i < buf.length; i++) {
        const line = buf.getLine(i);
        if (!line) continue;
        const str = line.translateToString(true);
        if (i > 0 && line.isWrapped) {
          text += str;
        } else {
          text += (text.length > 0 ? "\\n" : "") + str;
        }
      }
      return text;
      `,
    )) as unknown as string;
    lastSeen = text;
    if (text.includes(needle)) return text;
    await delay(250);
  }
  throw new Error(
    `xterm buffer for ${sessionId} never contained "${needle}" within ${timeoutMs}ms.\n` +
      `Last seen (truncated to 500 chars):\n${lastSeen.slice(0, 500)}`,
  );
}

async function waitForSessionCwd(
  ws: DaemonWsClient,
  sessionId: string,
  cwd: string,
): Promise<SessionSnapshot> {
  const msg = await ws.waitFor(
    (candidate): candidate is SessionUpdatedMessage =>
      isSessionUpdated(candidate) &&
      candidate.session.id === sessionId &&
      pathsEqual(candidate.session.current_cwd ?? null, cwd),
    { timeoutMs: 10_000 },
  );
  return msg.session;
}

async function waitForSessionStatus(
  ws: DaemonWsClient,
  sessionId: string,
  status: SessionSnapshot["status"],
): Promise<SessionSnapshot> {
  const msg = await ws.waitFor(
    (candidate): candidate is SessionUpdatedMessage =>
      isSessionUpdated(candidate) &&
      candidate.session.id === sessionId &&
      candidate.session.status === status,
    { timeoutMs: 10_000 },
  );
  return msg.session;
}

async function waitForRenderedShellStatus(
  sessionId: string,
  status: SessionSnapshot["status"],
): Promise<void> {
  const selector = `[data-testid="sidebar-session"][data-session-id="${sessionId}"]`;
  const row = await browser.$(selector);
  await row.waitForExist({ timeout: 10_000 });
  await browser.waitUntil(
    async () => (await row.getAttribute("data-session-status")) === status,
    {
      timeout: 10_000,
      timeoutMsg: `plain shell row did not render ${status}`,
    },
  );
  const dot = await row.$(`.status-dot.status-${status}`);
  await dot.waitForExist({ timeout: 5_000 });
}

async function waitForVisibleShellLabel(
  sessionId: string,
  expected: string,
): Promise<void> {
  await browser.waitUntil(
    async () => {
      const leaf = await browser.$(
        `[data-testid="sidebar-session"][data-session-id="${sessionId}"] .tree-label`,
      );
      const paneTitle = await browser.$(
        `[data-testid="session-pane"][data-session-id="${sessionId}"] .session-title h2`,
      );
      return (
        (await leaf.isExisting()) &&
        (await paneTitle.isExisting()) &&
        (await leaf.getText()) === expected &&
        (await paneTitle.getText()) === expected
      );
    },
    {
      timeout: 5_000,
      timeoutMsg: `visible shell label did not update to ${expected}`,
    },
  );
}

function shellCdCommand(path: string): string {
  const escaped = path.replace(/"/g, '\\"');
  return `cd "${escaped}"\r`;
}

function shellActivityCommand(): string {
  if (process.platform === "win32") {
    return '1..24 | ForEach-Object { "rt-shell-working-$_"; Start-Sleep -Milliseconds 20 }\r';
  }
  return 'for i in $(seq 1 24); do echo "rt-shell-working-$i"; sleep 0.02; done\r';
}

function expectPathEqual(actual: string | null, expected: string): void {
  expect(
    actual && normalizePathForCompare(actual),
    `path mismatch: ${actual ?? "(null)"} !== ${expected}`,
  ).to.equal(normalizePathForCompare(expected));
}

function pathsEqual(actual: string | null, expected: string): boolean {
  return actual !== null && normalizePathForCompare(actual) === normalizePathForCompare(expected);
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
