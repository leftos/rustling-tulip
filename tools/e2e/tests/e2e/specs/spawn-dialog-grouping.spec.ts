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
  GridNode,
  RepoEntry,
  SessionSnapshot,
  TabEntry,
} from "../../../src/types.js";

type SessionUpdatedMessage = DaemonMessage & {
  type: "session_updated";
  session: SessionSnapshot;
};
type TabUpdatedMessage = DaemonMessage & {
  type: "tab_updated";
  tab: TabEntry;
};

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;
const repoRoot = resolve(
  fileURLToPath(new URL("../../../../..", import.meta.url)),
);

describe("spawn dialog tab grouping", function () {
  this.timeout(180_000);

  let ws: DaemonWsClient | null = null;
  const fixtureRepos: string[] = [];
  const registeredRepoIds: string[] = [];
  const spawnedSessionIds: string[] = [];

  beforeEach(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });
  });

  afterEach(async function () {
    if (ws) {
      for (const sessionId of [...spawnedSessionIds].reverse()) {
        try {
          await stopSession(ws, sessionId);
        } catch {
          /* best-effort cleanup */
        }
      }
      for (const repoId of registeredRepoIds) {
        try {
          ws.send({ type: "remove_repo", repo_id: repoId });
          await delay(100);
        } catch {
          /* best-effort cleanup */
        }
      }
      await ws.close();
    }
    for (const repo of fixtureRepos) {
      await removeFixtureRepo(repo);
    }
    ws = null;
    fixtureRepos.length = 0;
    registeredRepoIds.length = 0;
    spawnedSessionIds.length = 0;
  });

  it("adds a same-repo spawn beside that repo's existing panes", async function () {
    if (!ws) throw new Error("setup failed");

    const repoA = await createFixtureRepo("rt-e2e-group-a-");
    const repoB = await createFixtureRepo("rt-e2e-group-b-");
    fixtureRepos.push(repoA, repoB);
    const repoAId = await registerRepo(ws, repoA, "rt-e2e-group-a");
    const repoBId = await registerRepo(ws, repoB, "rt-e2e-group-b");
    registeredRepoIds.push(repoAId, repoBId);

    const b1 = await spawnPlainShell(ws, repoBId, spawnedSessionIds);
    const b2 = await spawnPlainShell(ws, repoBId, spawnedSessionIds);
    const a1 = await spawnPlainShell(ws, repoAId, spawnedSessionIds);

    let tab = await createTab(ws, b1.id);
    tab = await splitAfterSession(ws, tab, b1.id, b2.id);
    tab = await splitAfterSession(ws, tab, b2.id, a1.id);

    await browser
      .$(`[data-testid="tab-pill"][data-tab-id="${tab.id}"]`)
      .waitForExist({ timeout: 10_000 });

    const spawnButton = await browser.$('[data-testid="sidebar-add-session"]');
    await spawnButton.click();
    const dialog = await browser.$('[data-testid="spawn-dialog"]');
    await dialog.waitForExist({ timeout: 5_000 });

    const targetSelect = await browser.$('[data-testid="spawn-target-select"]');
    await targetSelect.selectByAttribute("value", `repo:${repoAId}`);
    const currentPlacement = await browser.$(
      '[data-testid="spawn-placement-current-tab"]',
    );
    if ((await currentPlacement.getAttribute("aria-checked")) !== "true") {
      await currentPlacement.click();
    }
    const plainShell = await browser.$('[data-testid="spawn-agent-plain-shell"]');
    await plainShell.click();
    const worktreeToggle = await browser.$(
      '[data-testid="spawn-single-worktree"]',
    );
    if (await worktreeToggle.isSelected()) {
      await worktreeToggle.click();
    }

    const spawned = ws.waitFor(
      (msg): msg is SessionUpdatedMessage =>
        isSessionUpdated(msg) &&
        msg.session.members[0]?.repo_id === repoAId &&
        !spawnedSessionIds.includes(msg.session.id),
      { timeoutMs: 20_000 },
    );
    await browser.$('[data-testid="spawn-single-submit"]').click();
    const spawnedMsg = await spawned;
    spawnedSessionIds.push(spawnedMsg.session.id);

    const updated = await ws.waitFor(
      (msg): msg is TabUpdatedMessage =>
        isTabUpdated(msg) &&
        msg.tab.id === tab.id &&
        sessionOrder(msg.tab).includes(spawnedMsg.session.id),
      { timeoutMs: 10_000 },
    );
    const order = sessionOrder(updated.tab);
    const oldRepoIndex = order.indexOf(a1.id);
    const newRepoIndex = order.indexOf(spawnedMsg.session.id);

    expect(oldRepoIndex).to.be.greaterThan(-1);
    expect(newRepoIndex).to.be.greaterThan(-1);
    expect(Math.abs(newRepoIndex - oldRepoIndex)).to.equal(1);
    expect(order.slice(0, 2)).to.deep.equal([b1.id, b2.id]);

    await browser.saveScreenshot(".tmp/spawn-dialog-grouping-after.png");
  });
});

async function createFixtureRepo(prefix: string): Promise<string> {
  const parent = join(repoRoot, ".tmp", "e2e");
  await mkdir(parent, { recursive: true });
  const fixtureRepo = await mkdtemp(join(parent, prefix));
  await writeFile(join(fixtureRepo, "README.md"), "fixture for grouping e2e\n");
  runGit(fixtureRepo, ["init", "-b", "main"]);
  runGit(fixtureRepo, ["config", "user.email", "e2e@rustling-tulip.test"]);
  runGit(fixtureRepo, ["config", "user.name", "rt-e2e"]);
  runGit(fixtureRepo, ["add", "README.md"]);
  runGit(fixtureRepo, ["commit", "-m", "initial fixture commit"]);
  return fixtureRepo;
}

async function registerRepo(
  ws: DaemonWsClient,
  path: string,
  name: string,
): Promise<string> {
  const reposPromise = ws.waitFor(isRepos, { timeoutMs: 5_000 });
  ws.send({ type: "add_repo", path, name });
  const repos = await reposPromise;
  const registered = repos.repos.find(
    (r) => r.path === path || r.path === path.replace(/\\/g, "/"),
  );
  if (!registered) {
    throw new Error(`repo was not registered: ${path}`);
  }
  return registered.id;
}

async function spawnPlainShell(
  ws: DaemonWsClient,
  repoId: string,
  knownSessionIds: string[],
): Promise<SessionSnapshot> {
  const spawned = ws.waitFor(
    (msg): msg is SessionUpdatedMessage =>
      isSessionUpdated(msg) &&
      msg.session.members[0]?.repo_id === repoId &&
      !knownSessionIds.includes(msg.session.id),
    { timeoutMs: 20_000 },
  );
  ws.send({
    type: "spawn_session",
    label: null,
    target: {
      kind: "single",
      repo_id: repoId,
      branch_name: "main",
      base_branch: null,
      use_worktree: false,
    },
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
  const msg = await spawned;
  knownSessionIds.push(msg.session.id);
  return msg.session;
}

async function createTab(
  ws: DaemonWsClient,
  sessionId: string,
): Promise<TabEntry> {
  const tabPromise = ws.waitFor(isTabUpdated, { timeoutMs: 5_000 });
  ws.send({ type: "create_tab", name: "Grouping", initial_session_id: sessionId });
  return (await tabPromise).tab;
}

async function splitAfterSession(
  ws: DaemonWsClient,
  tab: TabEntry,
  anchorSessionId: string,
  newSessionId: string,
): Promise<TabEntry> {
  const anchor = collectPanes(tab).find((p) => p.session_id === anchorSessionId);
  if (!anchor) {
    throw new Error(`anchor session not found in tab: ${anchorSessionId}`);
  }
  const updated = ws.waitFor(
    (msg): msg is TabUpdatedMessage =>
      isTabUpdated(msg) &&
      msg.tab.id === tab.id &&
      sessionOrder(msg.tab).includes(newSessionId),
    { timeoutMs: 5_000 },
  );
  ws.send({
    type: "split_pane",
    tab_id: tab.id,
    pane_id: anchor.pane_id,
    direction: "horizontal",
    place: "second",
    new_session_id: newSessionId,
  });
  return (await updated).tab;
}

function isRepos(
  msg: DaemonMessage,
): msg is DaemonMessage & { type: "repos"; repos: RepoEntry[] } {
  return msg.type === "repos";
}

function isTabUpdated(
  msg: DaemonMessage,
): msg is TabUpdatedMessage {
  if (msg.type !== "tab_updated") return false;
  const tab = (msg as { tab?: unknown }).tab;
  return typeof tab === "object" && tab !== null;
}

function isSessionUpdated(
  msg: DaemonMessage,
): msg is SessionUpdatedMessage {
  if (msg.type !== "session_updated") return false;
  const session = (msg as { session?: unknown }).session;
  return typeof session === "object" && session !== null;
}

async function stopSession(
  ws: DaemonWsClient,
  sessionId: string,
): Promise<void> {
  const removed = ws
    .waitFor(
      (msg): msg is DaemonMessage & { type: "session_removed" } =>
        msg.type === "session_removed" && msg.session_id === sessionId,
      { timeoutMs: 5_000 },
    )
    .catch(() => null);
  ws.send({ type: "stop_session", session_id: sessionId, cleanup: [] });
  await removed;
  await delay(150);
}

async function removeFixtureRepo(path: string): Promise<void> {
  await rm(path, {
    recursive: true,
    force: true,
    maxRetries: 20,
    retryDelay: 100,
  });
}

function sessionOrder(tab: TabEntry): string[] {
  return collectPanes(tab)
    .map((pane) => pane.session_id)
    .filter((sessionId): sessionId is string => sessionId !== null);
}

function collectPanes(
  tab: TabEntry,
): Array<{ pane_id: string; session_id: string | null }> {
  if (tab.content.kind !== "grid") {
    return [];
  }
  return collectGridPanes(tab.content.grid);
}

function collectGridPanes(
  node: GridNode,
): Array<{ pane_id: string; session_id: string | null }> {
  if (node.kind === "pane") {
    return [{ pane_id: node.pane_id, session_id: node.session_id }];
  }
  return [...collectGridPanes(node.first), ...collectGridPanes(node.second)];
}

function runGit(cwd: string, args: string[]): void {
  execFileSync("git", args, { cwd, stdio: "ignore" });
}
