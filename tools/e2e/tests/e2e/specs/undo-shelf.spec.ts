import { mkdir } from "node:fs/promises";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

import { browser } from "@wdio/globals";
import { expect } from "chai";

import { DaemonWsClient } from "../../../src/ws-client.js";
import type { DaemonMessage, TabEntry } from "../../../src/types.js";

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

describe("undo shelf", function () {
  this.timeout(120_000);

  let ws: DaemonWsClient | null = null;
  const createdTabIds: string[] = [];

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });

    ws = await DaemonWsClient.open({ waitTimeoutMs: DAEMON_BOOT_TIMEOUT });
    await mkdir(join(repoRoot, ".tmp", "e2e"), { recursive: true });
  });

  after(async function () {
    if (ws) {
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
});

async function createTab(ws: DaemonWsClient, name: string): Promise<TabEntry> {
  const tabPromise = ws.waitFor(
    (msg): msg is TabUpdatedMessage =>
      isTabUpdated(msg) && msg.tab.name === name,
    { timeoutMs: 5_000 },
  );
  ws.send({ type: "create_tab", name, initial_session_id: null });
  return (await tabPromise).tab;
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

function tabPill(tabId: string) {
  return browser.$(`[data-testid="tab-pill"][data-tab-id="${tabId}"]`);
}
