/**
 * Keyboard accessibility / Escape-dismissal coverage.
 *
 * Iter 6 of UX audit triage added a shared `useEscape` hook + autofocus to
 * the three modal dialogs (SpawnDialog, RepoRemoveDialog, PresetLaunchDialog)
 * and the two context menus (sidebar container, tab pill). This spec drives
 * the cheapest modal to set up — RepoRemoveDialog — and asserts:
 *
 *   1. Pressing Escape closes the modal (same effect as Cancel / backdrop).
 *   2. The Cancel button is focused on open (so keyboard users land on the
 *      least-destructive default action).
 *   3. The session leaf in the sidebar is keyboard-focusable (role=button +
 *      tabIndex=0), so a sighted screen-reader user can navigate to it.
 *
 * The SpawnDialog and PresetLaunchDialog go through the same hook; covering
 * one modal is enough to confirm the pattern fires.
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

describe("keyboard a11y", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  let fixtureRepo: string | null = null;
  let registeredRepoId: string | null = null;
  let spawnedSessionId: string | null = null;

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });

    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });

    fixtureRepo = await mkdtemp(join(tmpdir(), "rt-e2e-a11y-"));
    await writeFile(join(fixtureRepo, "README.md"), "fixture for a11y e2e\n");
    runGit(fixtureRepo, ["init", "-b", "main"]);
    runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
    runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
    runGit(fixtureRepo, ["add", "README.md"]);
    runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);

    // Register the repo and spawn a session so we have something to open
    // the repo-remove modal against. Both steps are deterministic via the
    // side-channel WS (same pattern as repo-remove-confirm.spec.ts).
    const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
    ws.send({
      type: "add_repo",
      path: fixtureRepo,
      name: "rt-e2e-a11y-fixture",
    });
    const repos = await reposPromise;
    const fixture = repos.repos.find(
      (r: RepoEntry) =>
        r.path === fixtureRepo || r.path === fixtureRepo!.replace(/\\/g, "/"),
    );
    if (!fixture) throw new Error("fixture repo never registered");
    registeredRepoId = fixture.id;

    const spawnPromise = ws.waitFor(
      (m): m is DaemonMessage & {
        type: "session_updated";
        session: SessionSnapshot;
      } => m.type === "session_updated",
      { timeoutMs: 15_000 },
    );
    ws.send({
      type: "spawn_session",
      label: "a11y",
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
    spawnedSessionId = spawnMsg.session.id;

    // Wait for the repo row to render in the sidebar.
    const repoRow = await browser.$(
      `[data-testid=sidebar-container-repo][data-container-id="${registeredRepoId}"]`,
    );
    await repoRow.waitForExist({ timeout: 10_000 });
  });

  after(async function () {
    if (ws && spawnedSessionId) {
      try {
        ws.send({ type: "stop_session", session_id: spawnedSessionId, cleanup: [] });
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

  it("Escape dismisses the RepoRemoveDialog (and Cancel is focused on open)", async function () {
    expect(registeredRepoId, "registeredRepoId").to.not.equal(null);
    if (!registeredRepoId) throw new Error("setup failed");

    // Open the repo-remove modal by clicking × on the repo row. With a
    // live session, that path goes through the 3-way modal (not the inline
    // confirm).
    const repoRow = await browser.$(
      `[data-testid=sidebar-container-repo][data-container-id="${registeredRepoId}"]`,
    );
    const removeBtn = await repoRow.$('[data-testid=sidebar-remove-repo]');
    await removeBtn.click();

    const modal = await browser.$('[data-testid=repo-remove-dialog]');
    await modal.waitForExist({ timeout: 5_000 });

    // Cancel button should be focused on open. The e2e tsconfig has no DOM
    // lib, so we route through browser.execute with a string body to read
    // document.activeElement directly in the browser context.
    const activeTestId = (await browser.execute(
      `return document.activeElement
        ? document.activeElement.getAttribute("data-testid")
        : null;`,
    )) as unknown as string | null;
    expect(activeTestId, "Cancel button is focused").to.equal(
      "repo-remove-cancel",
    );

    // Press Escape — modal should close.
    await browser.keys("Escape");
    await modal.waitForExist({ timeout: 2_000, reverse: true });
  });

  it("session leaves are keyboard-focusable (tabIndex=0)", async function () {
    if (!spawnedSessionId) throw new Error("setup failed");
    const leaf = await browser.$(
      `[data-testid=sidebar-session][data-session-id="${spawnedSessionId}"]`,
    );
    await leaf.waitForExist({ timeout: 5_000 });

    const tabIndex = await leaf.getAttribute("tabindex");
    expect(tabIndex, "tabindex set on session leaf").to.equal("0");

    const role = await leaf.getAttribute("role");
    expect(role, "role=button on session leaf").to.equal("button");

    const ariaLabel = await leaf.getAttribute("aria-label");
    expect(ariaLabel, "aria-label populated").to.match(/^Session /);
  });

  it("workspace/repo container headers expose aria-expanded state", async function () {
    if (!registeredRepoId || !spawnedSessionId) throw new Error("setup failed");
    const header = await browser.$(
      `[data-testid=sidebar-container-repo][data-container-id="${registeredRepoId}"]`,
    );

    const role = await header.getAttribute("role");
    expect(role, "role=button on expandable container").to.equal("button");

    const tabIndex = await header.getAttribute("tabindex");
    expect(tabIndex, "tabindex set on expandable container").to.equal("0");

    const ariaExpanded = await header.getAttribute("aria-expanded");
    // Has at least one child session, so it should be expanded by default.
    expect(ariaExpanded, "aria-expanded populated").to.be.oneOf([
      "true",
      "false",
    ]);

    const chip = await header.$('[data-testid=tree-toggle-chip]');
    await chip.waitForExist({ timeout: 5_000 });
    expect(await chip.getAttribute("aria-expanded")).to.equal(ariaExpanded);

    await chip.click();
    await browser.waitUntil(
      async () => (await header.getAttribute("aria-expanded")) !== ariaExpanded,
      {
        timeout: 5_000,
        timeoutMsg: "tree toggle chip did not change container expansion",
      },
    );
    const leaf = await browser.$(
      `[data-testid=sidebar-session][data-session-id="${spawnedSessionId}"]`,
    );
    await leaf.waitForExist({ timeout: 5_000, reverse: ariaExpanded === "true" });

    const restoreChip = await header.$('[data-testid=tree-toggle-chip]');
    await restoreChip.click();
    await browser.waitUntil(
      async () => (await header.getAttribute("aria-expanded")) === ariaExpanded,
      {
        timeout: 5_000,
        timeoutMsg: "tree toggle chip did not restore container expansion",
      },
    );
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
