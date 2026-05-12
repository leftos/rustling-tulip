/**
 * Multi-window helper for E2E specs that need to interact with Tauri pop-out
 * windows (session pop-outs `?session=<id>` and tab pop-outs `?tab=<id>`).
 *
 * Tauri opens each pop-out as a separate OS window with its own WebView2
 * instance. msedgedriver (the WebDriver backend used by tauri-driver on
 * Windows) exposes all open windows in the same process via the standard
 * `GET /session/{id}/window/handles` endpoint. `captureNewWindow` wraps the
 * record-action-poll pattern so specs don't have to repeat it.
 *
 * Basic usage:
 * ```ts
 * const win = await captureNewWindow(async () => {
 *   const btn = await browser.$('[data-testid=session-pop-out]');
 *   await btn.click();
 * });
 * await win.activate();
 * // ... assertions in pop-out context ...
 * await win.deactivate();
 * // ... trigger close (e.g. stop session) from main window ...
 * await win.waitForClose();
 * ```
 */
import { setTimeout as delay } from "node:timers/promises";

import { browser } from "@wdio/globals";

export interface WindowSwitch {
  /** WebDriver handle string for the pop-out window. */
  readonly handle: string;
  /** Switch the active WebDriver context to this pop-out window. */
  activate(): Promise<void>;
  /** Switch the active WebDriver context back to the original (main) window. */
  deactivate(): Promise<void>;
  /**
   * Wait until this window's handle is no longer returned by
   * `getWindowHandles()`. Resolves when the window has closed; rejects on
   * timeout.
   */
  waitForClose(opts?: { timeoutMs?: number }): Promise<void>;
}

/**
 * Record the current set of WebDriver window handles, call `action()` (which
 * must open a new OS window), then poll until a new handle appears and return
 * a `WindowSwitch` for driving the new window.
 *
 * The returned `WindowSwitch` remembers the original handle so `deactivate()`
 * can switch back without the caller having to track it.
 *
 * @param action - An async function that triggers a new window to open (e.g.
 *   clicking a "Pop out" button or calling a Tauri command via `browser.execute`).
 * @param opts.timeoutMs - How long to wait for the new handle to appear
 *   (default: 10 000 ms).
 */
export async function captureNewWindow(
  action: () => Promise<void>,
  opts: { timeoutMs?: number } = {},
): Promise<WindowSwitch> {
  const timeoutMs = opts.timeoutMs ?? 10_000;
  const originalHandle = await browser.getWindowHandle();
  const handlesBefore = new Set(await browser.getWindowHandles());

  await action();

  const deadline = Date.now() + timeoutMs;
  let newHandle: string | undefined;
  while (Date.now() < deadline) {
    const current = await browser.getWindowHandles();
    newHandle = current.find((h) => !handlesBefore.has(h));
    if (newHandle !== undefined) break;
    await delay(100);
  }

  if (newHandle === undefined) {
    throw new Error(
      `captureNewWindow: no new WebDriver window handle appeared within ${timeoutMs}ms. ` +
        "If this is a Tauri multi-window issue, verify that msedgedriver enumerates " +
        "all WebView2 windows via getWindowHandles().",
    );
  }

  const capturedHandle = newHandle;

  return {
    handle: capturedHandle,
    activate: async () => {
      await browser.switchToWindow(capturedHandle);
    },
    deactivate: async () => {
      await browser.switchToWindow(originalHandle);
    },
    waitForClose: async ({ timeoutMs: closeTimeout = 10_000 } = {}) => {
      const closeDeadline = Date.now() + closeTimeout;
      while (Date.now() < closeDeadline) {
        const current = await browser.getWindowHandles();
        if (!current.includes(capturedHandle)) return;
        await delay(100);
      }
      throw new Error(
        `captureNewWindow: pop-out handle ${capturedHandle} did not disappear from ` +
          `window list within ${closeTimeout}ms.`,
      );
    },
  };
}
