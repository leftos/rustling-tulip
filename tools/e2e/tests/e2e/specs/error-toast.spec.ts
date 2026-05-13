/**
 * Daemon error surfacing — toast appears on Error / preset_launch_failed.
 *
 * The audit's finding at App.tsx:848-850: daemon Error messages used to
 * land in console.error only, invisible to end users. The toast component
 * now renders them in the bottom-right corner.
 *
 * Daemon Error replies are per-client (out_tx, not broadcast), so this spec
 * must trigger an error through the React app's own WS — not through the
 * side-channel client the harness usually uses. The dev-build global
 * `window.__rt_daemon_client` is the integration seam (mirrors `__rt_terms`).
 */
import { browser } from "@wdio/globals";
import { expect } from "chai";

const APP_BOOT_TIMEOUT = 60_000;

describe("daemon error toast", function () {
  this.timeout(120_000);

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });

    // Wait for the dev-only daemon client global to appear. The supervisor
    // boots the daemon → React connects → this global is assigned in App.tsx.
    await browser.waitUntil(
      async () =>
        (await browser.execute(
          `return !!(globalThis.__rt_daemon_client);`,
        )) as unknown as boolean,
      {
        timeout: 30_000,
        timeoutMsg: "__rt_daemon_client never appeared",
      },
    );
  });

  it("surfaces a daemon Error reply as a bottom-right toast", async function () {
    // Send a well-formed request that targets a missing tab through the React
    // app's own WS. Unknown future message variants are intentionally ignored
    // by the daemon for forward compatibility; missing-resource failures still
    // exercise the DaemonMessage::Error toast path.
    await browser.execute(
      `globalThis.__rt_daemon_client.send({ type: "rename_tab", tab_id: "missing_tab_for_test", name: "Nope" });`,
    );

    const toast = await browser.$('[data-testid=error-toast]');
    await toast.waitForExist({ timeout: 5_000 });

    const severity = await toast.getAttribute("data-toast-severity");
    expect(severity).to.equal("error");

    const text = await toast.getText();
    expect(text).to.match(/Daemon error/i);
    expect(text).to.include("missing_tab_for_test");
  });

  it("dismisses the toast when the close button is clicked", async function () {
    await browser.execute(
      `globalThis.__rt_daemon_client.send({ type: "rename_tab", tab_id: "another_missing_tab", name: "Nope" });`,
    );

    // Wait for a toast referencing "another_missing_tab" to render.
    await browser.waitUntil(
      async () => {
        const allToasts = await browser.$$('[data-testid=error-toast]');
        const texts = await allToasts.map((t) => t.getText());
        return texts.some((t: string) => t.includes("another_missing_tab"));
      },
      {
        timeout: 5_000,
        timeoutMsg: "expected 'another_missing_tab' toast to appear",
      },
    );

    // Find that specific toast and click its close button.
    const allToasts = await browser.$$('[data-testid=error-toast]');
    let dismissed = false;
    for (const toast of allToasts) {
      const text = await toast.getText();
      if (text.includes("another_missing_tab")) {
        const closeBtn = await toast.$('[data-testid=error-toast-close]');
        await closeBtn.click();
        dismissed = true;
        break;
      }
    }
    expect(dismissed, "found and clicked dismiss on 'another_missing_tab' toast").to
      .be.true;

    await browser.waitUntil(
      async () => {
        const allToasts = await browser.$$('[data-testid=error-toast]');
        const texts = await allToasts.map((t) => t.getText());
        return !texts.some((t: string) => t.includes("another_missing_tab"));
      },
      {
        timeout: 5_000,
        timeoutMsg: "expected 'another_missing_tab' toast to be dismissed",
      },
    );
  });
});
