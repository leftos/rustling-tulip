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

type TabUpdatedMessage = DaemonMessage & {
  type: "tab_updated";
  tab: TabEntry;
};

type SessionUpdatedMessage = DaemonMessage & {
  type: "session_updated";
  session: SessionSnapshot;
};

describe("pane swapping", function () {
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
    if (!ws) {
      return;
    }
    for (const sessionId of [...spawnedSessionIds].reverse()) {
      try {
        ws.send({ type: "stop_session", session_id: sessionId, cleanup: [] });
        await delay(300);
        ws.send({ type: "discard_session", session_id: sessionId, cleanup: [] });
        await delay(100);
      } catch {
        /* best-effort cleanup */
      }
    }
    for (const tabId of [...createdTabIds].reverse()) {
      try {
        ws.send({ type: "close_tab", tab_id: tabId });
        await delay(100);
      } catch {
        /* best-effort cleanup */
      }
    }
    await ws.close();
  });

  it("swaps two same-tab shell panes by dropping on the target center", async function () {
    if (!ws) {
      throw new Error("setup failed");
    }

    const suffix = Date.now().toString(36);
    const first = await spawnStandaloneShell(
      ws,
      `pane-swap-first-${suffix}`,
      spawnedSessionIds,
    );
    const second = await spawnStandaloneShell(
      ws,
      `pane-swap-second-${suffix}`,
      spawnedSessionIds,
    );

    let tab = await createTab(ws, `Pane swap ${suffix}`, first.id);
    createdTabIds.push(tab.id);
    await tabPill(tab.id).click();

    const rootPane = collectPanes(tab)[0];
    if (!rootPane) {
      throw new Error("new tab did not contain a root pane");
    }

    const split = ws.waitFor(
      (msg): msg is TabUpdatedMessage =>
        isTabUpdated(msg) &&
        msg.tab.id === tab.id &&
        paneSessionId(msg.tab, rootPane.pane_id) === first.id &&
        tabContainsSession(msg.tab, second.id),
      { timeoutMs: 5_000 },
    );
    ws.send({
      type: "split_pane",
      tab_id: tab.id,
      pane_id: rootPane.pane_id,
      direction: "horizontal",
      place: "second",
      new_session_id: second.id,
    });
    tab = (await split).tab;

    const firstPane = paneForSession(tab, first.id);
    const secondPane = paneForSession(tab, second.id);
    if (!firstPane || !secondPane) {
      throw new Error("split tab did not contain both shell sessions");
    }

    await sessionPane(first.id).waitForExist({ timeout: 10_000 });
    await sessionPane(second.id).waitForExist({ timeout: 10_000 });

    await beginCenterDrag(
      `[data-testid="session-pane"][data-session-id="${first.id}"] .session-header`,
      `.grid-pane[data-pane-id="${secondPane.pane_id}"]`,
    );
    const overlay = await browser.$(
      `.grid-pane[data-pane-id="${secondPane.pane_id}"] .grid-drop-overlay[data-drop-edge="replace"]`,
    );
    await overlay.waitForExist({
      timeout: 5_000,
      timeoutMsg: "swap drop overlay was not shown",
    });
    expect(await overlay.getText()).to.equal("Swap panes");
    await browser.saveScreenshot(
      join(repoRoot, ".tmp", "e2e", "pane-swap-preview.png"),
    );

    const swapped = ws.waitFor(
      (msg): msg is TabUpdatedMessage =>
        isTabUpdated(msg) &&
        msg.tab.id === tab.id &&
        paneSessionId(msg.tab, firstPane.pane_id) === second.id &&
        paneSessionId(msg.tab, secondPane.pane_id) === first.id,
      { timeoutMs: 5_000 },
    );
    await finishCenterDrag(
      `[data-testid="session-pane"][data-session-id="${first.id}"] .session-header`,
      `.grid-pane[data-pane-id="${secondPane.pane_id}"]`,
    );
    const swappedTab = (await swapped).tab;

    expect(paneSessionId(swappedTab, firstPane.pane_id)).to.equal(second.id);
    expect(paneSessionId(swappedTab, secondPane.pane_id)).to.equal(first.id);
    expect(collectPanes(swappedTab).map((pane) => pane.pane_id)).to.deep.equal(
      collectPanes(tab).map((pane) => pane.pane_id),
    );

    const shelf = await browser.$('[data-testid="undo-shelf"]');
    await shelf.waitForExist({ timeout: 5_000 });
    expect(await shelf.getText()).to.include("Swapped panes");
    await browser.saveScreenshot(join(repoRoot, ".tmp", "e2e", "pane-swap.png"));
  });
});

async function createTab(
  ws: DaemonWsClient,
  name: string,
  initialSessionId: string,
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
  knownSessionIds: string[],
): Promise<SessionSnapshot> {
  const spawned = ws.waitFor(
    (msg): msg is SessionUpdatedMessage =>
      isSessionUpdated(msg) &&
      msg.session.kind === "standalone" &&
      msg.session.mode === "plain_shell" &&
      !knownSessionIds.includes(msg.session.id),
    { timeoutMs: 20_000 },
  );
  ws.send(
    buildSpawnMessage({
      label,
      target: { kind: "standalone", cwd: null },
      mode: "plain_shell",
    }),
  );
  const msg = await spawned;
  knownSessionIds.push(msg.session.id);
  return msg.session;
}

async function beginCenterDrag(
  sourceSelector: string,
  targetSelector: string,
): Promise<void> {
  await browser.execute(
    (source: string, target: string) => {
      const scope = globalThis as unknown as Partial<BrowserScope>;
      if (!scope.document || !scope.DataTransfer || !scope.DragEvent) {
        throw new Error("browser drag APIs are unavailable");
      }
      const sourceElement = scope.document.querySelector(source);
      const targetElement = scope.document.querySelector(target);
      if (!sourceElement) {
        throw new Error(`drag source not found: ${source}`);
      }
      if (!targetElement) {
        throw new Error(`drag target not found: ${target}`);
      }
      const rect = targetElement.getBoundingClientRect();
      const dataTransfer = new scope.DataTransfer();
      scope.__rtPaneSwapDataTransfer = dataTransfer;
      sourceElement.dispatchEvent(
        new scope.DragEvent("dragstart", {
          bubbles: true,
          cancelable: true,
          dataTransfer,
        }),
      );
      targetElement.dispatchEvent(
        new scope.DragEvent("dragover", {
          bubbles: true,
          cancelable: true,
          dataTransfer,
          clientX: rect.left + rect.width / 2,
          clientY: rect.top + rect.height / 2,
        }),
      );
    },
    sourceSelector,
    targetSelector,
  );
}

async function finishCenterDrag(
  sourceSelector: string,
  targetSelector: string,
): Promise<void> {
  await browser.execute(
    (source: string, target: string) => {
      const scope = globalThis as unknown as Partial<BrowserScope>;
      if (!scope.document || !scope.DataTransfer || !scope.DragEvent) {
        throw new Error("browser drag APIs are unavailable");
      }
      const sourceElement = scope.document.querySelector(source);
      const targetElement = scope.document.querySelector(target);
      if (!sourceElement) {
        throw new Error(`drag source not found: ${source}`);
      }
      if (!targetElement) {
        throw new Error(`drag target not found: ${target}`);
      }
      const rect = targetElement.getBoundingClientRect();
      const dataTransfer =
        scope.__rtPaneSwapDataTransfer ?? new scope.DataTransfer();
      targetElement.dispatchEvent(
        new scope.DragEvent("drop", {
          bubbles: true,
          cancelable: true,
          dataTransfer,
          clientX: rect.left + rect.width / 2,
          clientY: rect.top + rect.height / 2,
        }),
      );
      sourceElement.dispatchEvent(
        new scope.DragEvent("dragend", {
          bubbles: true,
          cancelable: true,
          dataTransfer,
        }),
      );
      scope.__rtPaneSwapDataTransfer = undefined;
    },
    sourceSelector,
    targetSelector,
  );
}

interface BrowserScope {
  document: {
    querySelector: (selector: string) => BrowserElement | null;
  };
  DataTransfer: new () => unknown;
  DragEvent: new (type: string, init: DragEventInit) => unknown;
  __rtPaneSwapDataTransfer?: unknown;
}

interface BrowserElement {
  dispatchEvent: (event: unknown) => boolean;
  getBoundingClientRect: () => {
    left: number;
    top: number;
    width: number;
    height: number;
  };
}

interface DragEventInit {
  bubbles: boolean;
  cancelable: boolean;
  dataTransfer: unknown;
  clientX?: number;
  clientY?: number;
}

function isTabUpdated(msg: DaemonMessage): msg is TabUpdatedMessage {
  return msg.type === "tab_updated";
}

function isSessionUpdated(msg: DaemonMessage): msg is SessionUpdatedMessage {
  return msg.type === "session_updated";
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

function paneForSession(
  tab: TabEntry,
  sessionId: string,
): { pane_id: string; session_id: string | null } | null {
  return collectPanes(tab).find((pane) => pane.session_id === sessionId) ?? null;
}

function paneSessionId(tab: TabEntry, paneId: string): string | null {
  return (
    collectPanes(tab).find((pane) => pane.pane_id === paneId)?.session_id ?? null
  );
}

function tabContainsSession(tab: TabEntry, sessionId: string): boolean {
  return collectPanes(tab).some((pane) => pane.session_id === sessionId);
}

function sessionPane(sessionId: string) {
  return browser.$(`[data-testid="session-pane"][data-session-id="${sessionId}"]`);
}

function tabPill(tabId: string) {
  return browser.$(`[data-testid="tab-pill"][data-tab-id="${tabId}"]`);
}
