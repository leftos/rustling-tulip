/**
 * Session identity label coverage.
 *
 * The primary session label should stay stable even when a terminal emits a
 * noisy OSC title. User renames remain the explicit primary override.
 */
import { execFileSync } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";

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
const DEFAULT_BRANCH = "main";
const NOISY_TERMINAL_TITLE = "C:\\Windows\\system32\\cmd.exe";
const USER_LABEL = "My shell label";

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
    expect(fixture, "fixture repo registered").to.exist;
    registeredRepoId = fixture!.id;
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
    const pane = await sessionPane(spawnedSessionId);
    await assertRenderedLabel(spawnedSessionId, expectedDefaultLabel);

    await assertRenderedLabel(spawnedSessionId, expectedDefaultLabel);
    await assertLabelTooltipIncludes(spawnedSessionId, NOISY_TERMINAL_TITLE);

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

async function assertLabelTooltipIncludes(
  sessionId: string,
  expectedText: string,
): Promise<void> {
  await browser.waitUntil(
    async () => {
      const pane = await sessionPane(sessionId);
      const tooltip = await (await pane.$(".session-title h2")).getAttribute("title");
      return (tooltip ?? "").includes(expectedText);
    },
    {
      timeout: 10_000,
      timeoutMsg: `session label tooltip never included "${expectedText}"`,
    },
  );
}
