/**
 * Pop-out pane stop/discard lifecycle spec.
 *
 * Verifies that stopping a session leaves a pane pop-out open on the stopped
 * placeholder, and discarding the stopped session closes the pop-out.
 *
 * This spec exercises the multi-window WebDriver path: `captureNewWindow`
 * records the set of WebDriver handles before the pop-out opens, clicks the
 * "Pop out" button, polls until a new handle appears, then uses
 * `switchToWindow` to assert content in the pop-out before switching back.
 *
 * After the stopped session is discarded via the side-channel WS, the pane
 * loses its binding and the pop-out's `useEffect` closes the window.
 */
import { execFileSync } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";

import { browser } from "@wdio/globals";
import { expect } from "chai";

import { captureNewWindow } from "../../../src/popout.js";
import { DaemonWsClient } from "../../../src/ws-client.js";
import type {
  DaemonMessage,
  RepoEntry,
  SessionSnapshot,
} from "../../../src/types.js";

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;

describe("pop-out pane stop/discard lifecycle", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let registeredRepoId: string | null = null;
  let spawnedSessionId: string | null = null;

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    fixtureRepo = await mkdtemp(join(tmpdir(), "rt-e2e-popout-"));
    await writeFile(join(fixtureRepo, "README.md"), "fixture for popout e2e\n");
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);
  });

  after(async function () {
    if (ws && spawnedSessionId) {
      try {
        ws.send({
          type: "discard_session",
          session_id: spawnedSessionId,
          cleanup: registeredRepoId
            ? [{ repo_id: registeredRepoId, remove_worktree: false }]
            : [],
        });
        await delay(500);
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

  it("keeps the pop-out open when stopped, then closes it when discarded", async function () {
    expect(ws, "ws").to.not.be.null;
    expect(fixtureRepo, "fixtureRepo").to.not.be.null;
    if (!ws || !fixtureRepo) throw new Error("setup failed");
    const fixturePath = fixtureRepo;

    // Register the fixture repo via the side channel.
    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({
      type: "add_repo",
      path: fixturePath,
      name: "rt-e2e-popout-fixture",
    });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (r: RepoEntry) =>
        r.path === fixturePath || r.path === fixturePath.replace(/\\/g, "/"),
    );
    expect(fixture, "fixture repo registered").to.exist;
    registeredRepoId = fixture!.id;

    // Spawn an interactive session.
    const spawnPromise = ws.waitFor(
      (m): m is DaemonMessage & {
        type: "session_updated";
        session: SessionSnapshot;
      } => m.type === "session_updated",
      { timeoutMs: 15_000 },
    );
    ws.send({
      type: "spawn_session",
      label: "popout-autoclose",
      target: {
        kind: "single",
        repo_id: registeredRepoId,
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
    const spawnMsg = await spawnPromise;
    const sessionId = spawnMsg.session.id;
    spawnedSessionId = sessionId;

    // Click the session leaf in the sidebar to render it in a pane.
    const sidebarRow = await browser.$(
      `[data-testid=sidebar-session][data-session-id="${sessionId}"]`,
    );
    await sidebarRow.waitForExist({ timeout: 10_000 });
    await sidebarRow.click();

    // Wait for the session pane to appear.
    const pane = await browser.$(
      `[data-testid=session-pane][data-session-id="${sessionId}"]`,
    );
    await pane.waitForExist({ timeout: 10_000 });

    // Click "Pop out" and capture the resulting pane-backed pop-out window.
    const popOutBtn = await browser.$("[data-testid=session-pop-out]");
    await popOutBtn.waitForExist({ timeout: 5_000 });

    const win = await captureNewWindow(
      async () => {
        await popOutBtn.click();
      },
      { timeoutMs: 10_000 },
    );

    const poppedOutPlaceholder = await browser.$(
      `[data-testid=popped-out-pane][data-session-id="${sessionId}"]`,
    );
    await poppedOutPlaceholder.waitForExist({ timeout: 5_000 });

    // Switch to the pop-out, assert the pane window is rendered, and dock it
    // back. This verifies the source tab kept a real pane slot while popped
    // out, and that docking restores the original in-tab SessionPane.
    await win.activate();
    const paneRoot = await browser.$("[data-testid=pane-window-root]");
    await paneRoot.waitForExist({ timeout: 5_000 });
    const dockBack = await browser.$("[data-testid=pane-dock-back]");
    await dockBack.click();
    await win.waitForClose({ timeoutMs: 10_000 });
    await win.deactivate();

    await browser.waitUntil(
      async () => {
        const restoredPane = await browser.$(
          `[data-testid=session-pane][data-session-id="${sessionId}"]`,
        );
        if (!(await restoredPane.isExisting())) return false;
        const placeholder = await browser.$(
          `[data-testid=popped-out-pane][data-session-id="${sessionId}"]`,
        );
        return !(await placeholder.isExisting());
      },
      { timeout: 5_000, timeoutMsg: "pane never docked back into source tab" },
    );

    const reopened = await captureNewWindow(
      async () => {
        const restoredPopOutBtn = await browser.$("[data-testid=session-pop-out]");
        await restoredPopOutBtn.click();
      },
      { timeoutMs: 10_000 },
    );
    await reopened.activate();
    const reopenedRoot = await browser.$("[data-testid=pane-window-root]");
    await reopenedRoot.waitForExist({ timeout: 5_000 });
    await reopened.deactivate();

    // Stop the session from the main window via the side-channel WS. Stop now
    // retains the session record and pane binding, so the pop-out should stay
    // open and render the same stopped placeholder as the docked pane.
    ws.send({
      type: "stop_session",
      session_id: sessionId,
      cleanup: [{ repo_id: registeredRepoId, remove_worktree: false }],
    });
    await delay(500);
    expect(
      (await browser.getWindowHandles()).includes(reopened.handle),
      "pop-out remains open after stop",
    ).to.be.true;
    await reopened.activate();
    const exited = await browser.$("[data-testid=session-exited]");
    await exited.waitForExist({ timeout: 10_000 });
    await reopened.deactivate();

    // Discard is the explicit removal path. Once the pane binding disappears,
    // the pane pop-out should close.
    ws.send({
      type: "discard_session",
      session_id: sessionId,
      cleanup: [{ repo_id: registeredRepoId, remove_worktree: false }],
    });
    spawnedSessionId = null;
    await reopened.waitForClose({ timeoutMs: 10_000 });
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
