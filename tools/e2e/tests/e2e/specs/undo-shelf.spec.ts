import { mkdir } from "node:fs/promises";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

import { browser } from "@wdio/globals";
import { expect } from "chai";

import { DaemonWsClient } from "../../../src/ws-client.js";
import {
  buildSpawnMessage,
  dismissLayoutChooser,
} from "../../../src/session-helpers.js";
import type {
  DaemonMessage,
  GridNode,
  SessionSnapshot,
  TabEntry,
} from "../../../src/types.js";

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;
const repoRoot = resolve(
  fileURLToPath(new URL("../../../../..", import.meta.url)),
);

type TabUpdatedMessage = {
  type: "tab_updated";
  tab: TabEntry;
} & Record<string, unknown>;

type TabRemovedMessage = {
  type: "tab_removed";
  tab_id: string;
} & Record<string, unknown>;

type SessionUpdatedMessage = {
  type: "session_updated";
  session: SessionSnapshot;
} & Record<string, unknown>;

describe("undo shelf", function () {
  this.timeout(120_000);

  let ws: DaemonWsClient | null = null;
  const createdTabIds: string[] = [];
  const spawnedSessionIds: string[] = [];

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
    await dismissLayoutChooser(APP_BOOT_TIMEOUT);

    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });
    await mkdir(join(repoRoot, ".tmp", "e2e"), { recursive: true });
  });

  after(async function () {
    if (ws) {
      for (const sessionId of [...spawnedSessionIds].reverse()) {
        try {
          ws.send({ type: "stop_session", session_id: sessionId, cleanup: [] });
          await delay(300);
          ws.send({
            type: "discard_session",
            session_id: sessionId,
            cleanup: [],
          });
          await delay(100);
        } catch {
          /* best-effort */
        }
      }
      for (const tabId of [...createdTabIds].reverse()) {
        try {
          ws.send({ type: "close_tab", tab_id: tabId });
          await delay(100);
        } catch {
          /* best-effort */
        }
      }
      await ws.close();
    }
  });

  it("restores a closed active tab to its previous position", async function () {
    if (!ws) {
      throw new Error("setup failed");
    }

    const first = await createTab(ws, "Undo first");
    const second = await createTab(ws, "Undo second");
    createdTabIds.push(first.id, second.id);

    await tabPill(second.id).click();
    await browser.waitUntil(
      async () =>
        (await tabPill(second.id).getAttribute("data-tab-active")) === "true",
      { timeout: 5_000, timeoutMsg: "second tab did not become active" },
    );

    const removed = ws.waitFor(
      (msg): msg is TabRemovedMessage => isTabRemoved(msg, second.id),
      { timeoutMs: 5_000 },
    );
    const closeButton = await tabPill(second.id).$(
      '[data-testid="tab-pill-close"]',
    );
    await closeButton.click();
    await removed;

    await browser.waitUntil(async () => !(await tabPill(second.id).isExisting()), {
      timeout: 5_000,
      timeoutMsg: "closed tab still rendered",
    });
    const shelf = await browser.$('[data-testid="undo-shelf"]');
    await shelf.waitForExist({ timeout: 5_000 });
    expect(await shelf.getText()).to.include('Closed tab "Undo second"');
    await browser.saveScreenshot(join(repoRoot, ".tmp", "e2e", "undo-shelf.png"));

    const restored = ws.waitFor(
      (msg): msg is TabUpdatedMessage =>
        isTabUpdated(msg) && msg.tab.id === second.id,
      { timeoutMs: 5_000 },
    );
    await browser.$('[data-testid="undo-action"]').click();
    await restored;

    const restoredPill = await tabPill(second.id);
    await restoredPill.waitForExist({ timeout: 5_000 });
    expect(await restoredPill.getAttribute("data-tab-active")).to.equal("true");

    const labels = await browser.$$('[data-testid="tab-pill"]');
    const visibleNames: Array<string | null> = [];
    for (const label of labels) {
      visibleNames.push(await label.getAttribute("data-tab-name"));
    }
    expect(visibleNames.indexOf("Undo first")).to.be.lessThan(
      visibleNames.indexOf("Undo second"),
    );
  });

  it("restores a closed pane layout in the active tab", async function () {
    if (!ws) {
      throw new Error("setup failed");
    }

    const tab = await createTab(ws, "Undo pane");
    createdTabIds.push(tab.id);
    await tabPill(tab.id).click();

    const rootPaneId = paneIds(tab)[0];
    if (!rootPaneId) {
      throw new Error("new tab did not contain a root pane");
    }

    const split = ws.waitFor(
      (msg): msg is TabUpdatedMessage =>
        isTabUpdated(msg) && msg.tab.id === tab.id && paneIds(msg.tab).length === 2,
      { timeoutMs: 5_000 },
    );
    ws.send({
      type: "split_pane",
      tab_id: tab.id,
      pane_id: rootPaneId,
      direction: "horizontal",
      place: "second",
      new_session_id: null,
    });
    const splitTab = (await split).tab;
    const beforeClosePaneIds = paneIds(splitTab);
    const closePaneId = beforeClosePaneIds[1];
    if (!closePaneId) {
      throw new Error("split tab did not contain a second pane");
    }

    const closed = ws.waitFor(
      (msg): msg is TabUpdatedMessage =>
        isTabUpdated(msg) && msg.tab.id === tab.id && paneIds(msg.tab).length === 1,
      { timeoutMs: 5_000 },
    );
    const closeButton = await browser.$(
      `.grid-pane[data-pane-id="${closePaneId}"] [data-testid="grid-pane-close"]`,
    );
    await closeButton.waitForExist({ timeout: 5_000 });
    await closeButton.click();
    await closed;

    await browser.waitUntil(
      async () =>
        !(await browser
          .$(`.grid-pane[data-pane-id="${closePaneId}"]`)
          .isExisting()),
      { timeout: 5_000, timeoutMsg: "closed pane still rendered" },
    );
    const shelf = await browser.$('[data-testid="undo-shelf"]');
    await shelf.waitForExist({ timeout: 5_000 });
    expect(await shelf.getText()).to.include("Closed empty pane");
    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "undo-close-pane-shelf.png"),
    );

    const restored = ws.waitFor(
      (msg): msg is TabUpdatedMessage =>
        isTabUpdated(msg) &&
        msg.tab.id === tab.id &&
        paneIds(msg.tab).join("|") === beforeClosePaneIds.join("|"),
      { timeoutMs: 5_000 },
    );
    await browser.$('[data-testid="undo-action"]').click();
    await restored;

    const restoredPane = await browser.$(
      `.grid-pane[data-pane-id="${closePaneId}"]`,
    );
    await restoredPane.waitForExist({ timeout: 5_000 });
  });

  it("restores a moved pane to its source tab", async function () {
    if (!ws) {
      throw new Error("setup failed");
    }

    const session = await spawnStandaloneShell(ws, "undo-move-shell");
    spawnedSessionIds.push(session.id);

    const suffix = Date.now().toString(36);
    const source = await createTab(ws, `Move source ${suffix}`, session.id);
    const target = await createTab(ws, `Move target ${suffix}`);
    createdTabIds.push(source.id, target.id);

    await tabPill(source.id).click();
    const pane = await browser.$(
      `[data-testid="session-pane"][data-session-id="${session.id}"]`,
    );
    await pane.waitForExist({ timeout: 10_000 });
    const header = await pane.$(".session-header");
    await header.click({ button: "right" });
    await openContextSubmenu("session-context-move-menu");

    const moveButton = await browser.$(
      `[data-testid="session-context-move-to-tab"][data-tab-id="${target.id}"]`,
    );
    await moveButton.waitForDisplayed({ timeout: 5_000 });

    const sourceRemoved = ws.waitFor(
      (msg): msg is TabRemovedMessage => isTabRemoved(msg, source.id),
      { timeoutMs: 5_000 },
    );
    const targetUpdated = ws.waitFor(
      (msg): msg is TabUpdatedMessage =>
        isTabUpdated(msg) &&
        msg.tab.id === target.id &&
        tabContainsSession(msg.tab, session.id),
      { timeoutMs: 5_000 },
    );
    await moveButton.click();
    await sourceRemoved;
    await targetUpdated;

    const shelf = await browser.$('[data-testid="undo-shelf"]');
    await shelf.waitForExist({ timeout: 5_000 });
    expect(await shelf.getText()).to.include("Moved pane");
    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "undo-move-pane-shelf.png"),
    );

    const sourceRestored = ws.waitFor(
      (msg): msg is TabUpdatedMessage =>
        isTabUpdated(msg) &&
        msg.tab.id === source.id &&
        tabContainsSession(msg.tab, session.id),
      { timeoutMs: 5_000 },
    );
    const targetRestored = ws.waitFor(
      (msg): msg is TabUpdatedMessage =>
        isTabUpdated(msg) &&
        msg.tab.id === target.id &&
        !tabContainsSession(msg.tab, session.id),
      { timeoutMs: 5_000 },
    );
    await browser.$('[data-testid="undo-action"]').click();
    await sourceRestored;
    await targetRestored;

    const sourcePill = await tabPill(source.id);
    await sourcePill.waitForExist({ timeout: 5_000 });
    expect(await sourcePill.getAttribute("data-tab-active")).to.equal("true");
    const restoredPane = await browser.$(
      `[data-testid="session-pane"][data-session-id="${session.id}"]`,
    );
    await restoredPane.waitForExist({ timeout: 10_000 });
  });

  it("binds a new session into an empty pane via double-click", async function () {
    if (!ws) {
      throw new Error("setup failed");
    }

    const session = await spawnStandaloneShell(ws, "undo-bind-shell");
    spawnedSessionIds.push(session.id);

    const suffix = Date.now().toString(36);
    const tab = await createTab(ws, `Bind target ${suffix}`);
    createdTabIds.push(tab.id);
    await tabPill(tab.id).click();

    const paneId = paneIds(tab)[0];
    if (!paneId) {
      throw new Error("new tab did not contain a root pane");
    }
    const pane = await browser.$(`.grid-pane[data-pane-id="${paneId}"]`);
    await pane.waitForExist({ timeout: 5_000 });
    await pane.click();

    const row = await browser.$(
      `[data-testid="sidebar-session"][data-session-id="${session.id}"]`,
    );
    await row.waitForExist({ timeout: 10_000 });
    expect(await row.getAttribute("data-tab-binding-count")).to.equal("0");

    const bound = ws.waitFor(
      (msg): msg is TabUpdatedMessage =>
        isTabUpdated(msg) &&
        msg.tab.id === tab.id &&
        tabContainsSession(msg.tab, session.id),
      { timeoutMs: 5_000 },
    );
    // Binding an unbound session into the focused empty pane is a
    // double-click gesture — single left-click on an unbound row is a
    // deliberate no-op (see App.tsx onSelectSession). Binding is intentionally
    // not undoable (the "Bound session" undo entry was dropped in 20d92bb when
    // the gesture was reworked), so there is no undo-shelf entry to assert.
    await row.$(".tree-label").doubleClick();
    await bound;

    // The row re-renders on binding, replacing its DOM node — re-query on each
    // poll so a cached handle doesn't go stale.
    const rowSel = `[data-testid="sidebar-session"][data-session-id="${session.id}"]`;
    await browser.waitUntil(
      async () =>
        (await (await browser.$(rowSel)).getAttribute("data-tab-binding-count")) ===
        "1",
      {
        timeout: 5_000,
        timeoutMsg: "session did not bind into the empty pane",
      },
    );
    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "undo-bind-session.png"),
    );
  });
});

async function createTab(
  ws: DaemonWsClient,
  name: string,
  initialSessionId: string | null = null,
): Promise<TabEntry> {
  const tabPromise = ws.waitFor(
    (msg): msg is TabUpdatedMessage =>
      isTabUpdated(msg) && msg.tab.name === name,
    { timeoutMs: 5_000 },
  );
  ws.send({ type: "create_tab", name, initial_session_id: initialSessionId });
  return (await tabPromise).tab;
}

async function spawnStandaloneShell(
  ws: DaemonWsClient,
  label: string,
): Promise<SessionSnapshot> {
  const spawned = ws.waitFor(
    (msg): msg is SessionUpdatedMessage =>
      isSessionUpdated(msg) &&
      msg.session.kind === "standalone" &&
      msg.session.mode === "plain_shell",
    { timeoutMs: 20_000 },
  );
  ws.send(
    buildSpawnMessage({
      label,
      target: { kind: "standalone", cwd: null },
      mode: "plain_shell",
    }),
  );
  return (await spawned).session;
}

function isTabUpdated(msg: DaemonMessage): msg is TabUpdatedMessage {
  const candidate = msg as {
    type?: unknown;
    tab?: { id?: unknown; name?: unknown };
  };
  return (
    candidate.type === "tab_updated" &&
    typeof candidate.tab?.id === "string" &&
    typeof candidate.tab.name === "string"
  );
}

function isTabRemoved(
  msg: DaemonMessage,
  tabId: string,
): msg is TabRemovedMessage {
  const candidate = msg as { type?: unknown; tab_id?: unknown };
  return candidate.type === "tab_removed" && candidate.tab_id === tabId;
}

function isSessionUpdated(msg: DaemonMessage): msg is SessionUpdatedMessage {
  const candidate = msg as { type?: unknown; session?: unknown };
  return (
    candidate.type === "session_updated" &&
    typeof candidate.session === "object" &&
    candidate.session !== null
  );
}

function paneIds(tab: TabEntry): string[] {
  if (tab.content.kind !== "grid") {
    return [];
  }
  return collectPaneIds(tab.content.grid);
}

function collectPaneIds(node: GridNode): string[] {
  if (node.kind === "pane") {
    return [node.pane_id];
  }
  return [...collectPaneIds(node.first), ...collectPaneIds(node.second)];
}

function tabContainsSession(tab: TabEntry, sessionId: string): boolean {
  if (tab.content.kind !== "grid") {
    return false;
  }
  return gridContainsSession(tab.content.grid, sessionId);
}

function gridContainsSession(node: GridNode, sessionId: string): boolean {
  if (node.kind === "pane") {
    return node.session_id === sessionId;
  }
  return (
    gridContainsSession(node.first, sessionId) ||
    gridContainsSession(node.second, sessionId)
  );
}

function tabPill(tabId: string) {
  return browser.$(`[data-testid="tab-pill"][data-tab-id="${tabId}"]`);
}

async function openContextSubmenu(testId: string): Promise<void> {
  const trigger = await browser.$(`[data-testid="${testId}"]`);
  await trigger.waitForDisplayed({ timeout: 5_000 });
  await trigger.moveTo();
  await trigger.click();
}
