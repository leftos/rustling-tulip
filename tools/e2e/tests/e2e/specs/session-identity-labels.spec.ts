/**
 * Session identity label coverage.
 *
 * The primary session label should stay stable even when a terminal emits a
 * noisy OSC title. User renames remain the explicit primary override.
 */
import { execFileSync } from "node:child_process";
import { Buffer } from "node:buffer";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
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
const SESSION_OUTPUT_TIMEOUT = 15_000;
const DEFAULT_BRANCH = "main";
const TERMINAL_TITLE = "Codex style title from slash command";
const USER_LABEL = "My shell label";
const WORKING_DOT_COLOR = "rgb(232, 165, 49)";
const repoRoot = resolve(
  fileURLToPath(new URL("../../../../..", import.meta.url)),
);

describe("session identity labels", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let registeredRepoId: string | null = null;
  let spawnedSessionId: string | null = null;
  let expectedDefaultLabel: string | null = null;

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });

    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    fixtureRepo = await mkdtemp(join(tmpdir(), "rt-e2e-identity-"));
    await writeFile(join(fixtureRepo, "README.md"), "fixture for identity e2e\n");
    runGit(fixtureRepo, ["init", "-b", DEFAULT_BRANCH]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);
  });

  after(async function () {
    if (ws && spawnedSessionId) {
      try {
        ws.send({ type: "stop_session", session_id: spawnedSessionId, cleanup: [] });
        await delay(500);
        ws.send({ type: "discard_session", session_id: spawnedSessionId, cleanup: [] });
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

  it("keeps terminal titles secondary and user renames primary", async function () {
    if (!ws || !fixtureRepo) throw new Error("setup failed");

    const repoName = "rt-e2e-identity-fixture";
    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({ type: "add_repo", path: fixtureRepo, name: repoName });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (r: RepoEntry) =>
        r.path === fixtureRepo || r.path === fixtureRepo!.replace(/\\/g, "/"),
    );
    if (!fixture) {
      throw new Error("fixture repo was not registered");
    }
    registeredRepoId = fixture.id;
    expectedDefaultLabel = `${repoName}: ${DEFAULT_BRANCH}`;

    const spawnPromise = ws.waitFor(isSessionUpdated, { timeoutMs: 15_000 });
    ws.send({
      type: "spawn_session",
      label: null,
      target: {
        kind: "single",
        repo_id: registeredRepoId,
        branch_name: DEFAULT_BRANCH,
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
    const spawnMsg = await spawnPromise;
    spawnedSessionId = spawnMsg.session.id;
    expect(spawnMsg.session.label).to.equal(expectedDefaultLabel);

    const sidebarRow = await sessionSidebarRow(spawnedSessionId);
    await sidebarRow.click();
    await sessionPane(spawnedSessionId);
    await assertRenderedLabel(spawnedSessionId, expectedDefaultLabel);

    const banner = await waitForBufferText(
      spawnedSessionId,
      "[fake-claude] ready",
      SESSION_OUTPUT_TIMEOUT,
    );
    expect(banner, "fake-claude banner in xterm buffer").to.include(
      "[fake-claude] ready",
    );

    const titlePromise = ws.waitFor(
      (m): m is DaemonMessage & { type: "session_updated"; session: SessionSnapshot } =>
        isSessionUpdated(m) &&
        m.session.id === spawnedSessionId &&
        m.session.terminal_title === TERMINAL_TITLE,
      { timeoutMs: 10_000 },
    );
    sendInput(ws, spawnedSessionId, `/rename ${TERMINAL_TITLE}\r`);
    const titleUpdate = await titlePromise;
    expect(titleUpdate.session.label).to.equal(expectedDefaultLabel);

    await waitForBufferText(
      spawnedSessionId,
      `[fake-claude] renamed: ${TERMINAL_TITLE}`,
      SESSION_OUTPUT_TIMEOUT,
    );
    await assertRenderedLabel(spawnedSessionId, expectedDefaultLabel);
    await assertLabelTooltipsInclude(spawnedSessionId, TERMINAL_TITLE);

    sendInput(ws, spawnedSessionId, "/stream\r");
    await assertWorkingDotIsOrangeAndPulsing(spawnedSessionId);
    await waitForBufferText(
      spawnedSessionId,
      "[fake-claude] stream 20",
      SESSION_OUTPUT_TIMEOUT,
    );

    const renamePromise = ws.waitFor(
      (m): m is DaemonMessage & { type: "session_updated"; session: SessionSnapshot } =>
        isSessionUpdated(m) &&
        m.session.id === spawnedSessionId &&
        m.session.label === USER_LABEL,
      { timeoutMs: 5_000 },
    );
    ws.send({
      type: "rename_session",
      session_id: spawnedSessionId,
      label: USER_LABEL,
    });
    await renamePromise;

    await assertRenderedLabel(spawnedSessionId, USER_LABEL);
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

function sendInput(ws: DaemonWsClient, sessionId: string, input: string): void {
  ws.send({
    type: "send_input",
    session_id: sessionId,
    data_b64: Buffer.from(input, "utf8").toString("base64"),
  });
}

async function sessionSidebarRow(sessionId: string) {
  const row = await browser.$(
    `[data-testid=sidebar-session][data-session-id="${sessionId}"]`,
  );
  await row.waitForExist({ timeout: 10_000 });
  return row;
}

async function sessionPane(sessionId: string) {
  const pane = await browser.$(
    `[data-testid=session-pane][data-session-id="${sessionId}"]`,
  );
  await pane.waitForExist({ timeout: 10_000 });
  return pane;
}

async function assertRenderedLabel(
  sessionId: string,
  expectedLabel: string,
): Promise<void> {
  await browser.waitUntil(
    async () => {
      const rowLabel = await (await sessionSidebarRow(sessionId)).$(".tree-label").getText();
      const paneLabel = await (await sessionPane(sessionId)).$(".session-title h2").getText();
      return rowLabel === expectedLabel && paneLabel === expectedLabel;
    },
    {
      timeout: 10_000,
      timeoutMsg: `session label never rendered as "${expectedLabel}"`,
    },
  );
}

async function assertWorkingDotIsOrangeAndPulsing(
  sessionId: string,
): Promise<void> {
  await browser.waitUntil(
    async () => {
      const row = await sessionSidebarRow(sessionId);
      const pane = await sessionPane(sessionId);
      return (
        (await row.getAttribute("data-session-status")) === "working" &&
        (await pane.getAttribute("data-session-status")) === "working"
      );
    },
    {
      timeout: 10_000,
      timeoutMsg: "session never entered working state",
    },
  );

  await browser.waitUntil(
    async () => {
      const styles = await statusDotStyles(sessionId);
      return styles.every(
        (style) =>
          style !== null &&
          style.backgroundColor === WORKING_DOT_COLOR &&
          style.animationName === "status-pulse" &&
          style.animationDuration !== "0s",
      );
    },
    {
      timeout: 5_000,
      timeoutMsg: "working status dots were not orange and pulsing",
    },
  );

  await mkdir(join(repoRoot, ".tmp", "e2e"), { recursive: true });
  await browser.saveScreenshot(
    join(repoRoot, ".tmp", "e2e", "session-working-orange-dot.png"),
  );
}

async function statusDotStyles(sessionId: string): Promise<
  Array<{
    backgroundColor: string;
    animationName: string;
    animationDuration: string;
  } | null>
> {
  return (await browser.execute(
    `
    const sessionId = ${JSON.stringify(sessionId)};
    const selectors = [
      \`[data-testid=sidebar-session][data-session-id="\${sessionId}"] .status-dot\`,
      \`[data-testid=session-pane][data-session-id="\${sessionId}"] .session-title .status-dot\`,
    ];
    return selectors.map((selector) => {
      const el = document.querySelector(selector);
      if (!el) return null;
      const style = getComputedStyle(el);
      return {
        backgroundColor: style.backgroundColor,
        animationName: style.animationName,
        animationDuration: style.animationDuration,
      };
    });
    `,
  )) as unknown as Array<{
    backgroundColor: string;
    animationName: string;
    animationDuration: string;
  } | null>;
}

async function assertLabelTooltipsInclude(
  sessionId: string,
  expectedText: string,
): Promise<void> {
  await browser.waitUntil(
    async () => {
      const pane = await sessionPane(sessionId);
      const paneTooltip = await (await pane.$(".session-title h2")).getAttribute("title");
      const sidebarTooltip = await (await (await sessionSidebarRow(sessionId)).$(".tree-label")).getAttribute(
        "title",
      );
      return (
        (paneTooltip ?? "").includes(expectedText) &&
        (sidebarTooltip ?? "").includes(expectedText)
      );
    },
    {
      timeout: 10_000,
      timeoutMsg: `session label tooltips never included "${expectedText}"`,
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
      const lines = [];
      for (let i = 0; i < buf.length; i++) {
        const line = buf.getLine(i);
        if (line) lines.push(line.translateToString(true));
      }
      return lines.join("\\n");
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
