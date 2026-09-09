/**
 * Middle-click on a tab pill closes the tab through the same path as its
 * X button, including the two-step confirm for non-trivial tabs:
 *
 * 1. An empty tab needs no confirm — one middle-click sends `close_tab`.
 * 2. A tab with a bound session arms the confirm on the first middle-click
 *    (the X button flips to the checkmark) and closes on the second.
 */
import { setTimeout as delay } from "node:timers/promises";

import { browser } from "@wdio/globals";
import { expect } from "chai";

import { DaemonWsClient } from "../../../src/ws-client.js";
import {
  buildSpawnMessage,
  dismissLayoutChooser,
} from "../../../src/session-helpers.js";
import type {
  DaemonMessage,
  SessionSnapshot,
  TabEntry,
} from "../../../src/types.js";

const APP_BOOT_TIMEOUT = 60_000;
const DAEMON_BOOT_TIMEOUT = 30_000;
const MIDDLE_BUTTON = 1;

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

describe("tab middle-click close", function () {
  this.timeout(120_000);

  let ws: DaemonWsClient | null = null;
  const createdTabIds: string[] = [];
  const spawnedSessionIds: string[] = [];

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
    await dismissLayoutChooser(APP_BOOT_TIMEOUT);
    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });
  });

  after(async function () {
    if (!ws) return;
    for (const sessionId of [...spawnedSessionIds].reverse()) {
      try {
        ws.send({ type: "stop_session", session_id: sessionId, cleanup: [] });
        await delay(300);
        ws.send({ type: "discard_session", session_id: sessionId, cleanup: [] });
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
  });

  it("closes an empty tab with a single middle-click", async function () {
    if (!ws) throw new Error("setup failed");

    const tab = await createTab(ws, "Middle empty");
    createdTabIds.push(tab.id);
    await tabPill(tab.id).waitForExist({ timeout: 5_000 });

    const removed = ws.waitFor(
      (msg): msg is TabRemovedMessage => isTabRemoved(msg, tab.id),
      { timeoutMs: 5_000 },
    );
    await middleClick(tab.id);
    await removed;

    await browser.waitUntil(async () => !(await tabPill(tab.id).isExisting()), {
      timeout: 5_000,
      timeoutMsg: "middle-clicked empty tab still rendered",
    });
  });

  it("arms the confirm on a bound tab, then closes on the second middle-click", async function () {
    if (!ws) throw new Error("setup failed");

    const session = await spawnStandaloneShell(ws, "Middle bound shell");
    spawnedSessionIds.push(session.id);
    const tab = await createTab(ws, "Middle bound", session.id);
    createdTabIds.push(tab.id);
    await tabPill(tab.id).waitForExist({ timeout: 5_000 });

    const closeButton = tabPill(tab.id).$('[data-testid="tab-pill-close"]');
    expect(await closeButton.getAttribute("data-confirming")).to.equal("false");

    let removedEarly = false;
    const earlyRemoval = ws
      .waitFor(
        (msg): msg is TabRemovedMessage => isTabRemoved(msg, tab.id),
        { timeoutMs: 1_500 },
      )
      .then(() => {
        removedEarly = true;
      })
      .catch(() => undefined);

    await middleClick(tab.id);
    await browser.waitUntil(
      async () => (await closeButton.getAttribute("data-confirming")) === "true",
      {
        timeout: 5_000,
        timeoutMsg: "first middle-click did not arm the close confirm",
      },
    );
    await earlyRemoval;
    expect(removedEarly, "bound tab closed without confirmation").to.equal(false);

    const removed = ws.waitFor(
      (msg): msg is TabRemovedMessage => isTabRemoved(msg, tab.id),
      { timeoutMs: 5_000 },
    );
    await middleClick(tab.id);
    await removed;

    await browser.waitUntil(async () => !(await tabPill(tab.id).isExisting()), {
      timeout: 5_000,
      timeoutMsg: "confirmed middle-click tab still rendered",
    });
  });
});

async function middleClick(tabId: string): Promise<void> {
  const label = await tabPill(tabId).$(".tab-pill-label");
  await browser
    .action("pointer")
    .move({ origin: label })
    .down({ button: MIDDLE_BUTTON })
    .up({ button: MIDDLE_BUTTON })
    .perform();
}

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

function tabPill(tabId: string) {
  return browser.$(`[data-testid="tab-pill"][data-tab-id="${tabId}"]`);
}
