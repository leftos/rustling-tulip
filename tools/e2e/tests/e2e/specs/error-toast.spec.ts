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
    // Send an unknown message variant through the React app's own WS. The
    // daemon's serde decoder rejects it and the dispatcher replies with
    // DaemonMessage::Error, which the React app's onMessage handler routes
    // through pushToast.
    await browser.execute(
      `globalThis.__rt_daemon_client.send({ type: "bogus_message_for_test" });`,
    );

    const toast = await browser.$('[data-testid=error-toast]');
    await toast.waitForExist({ timeout: 5_000 });

    const severity = await toast.getAttribute("data-toast-severity");
    expect(severity).to.equal("error");

    const text = await toast.getText();
    expect(text).to.match(/Daemon error/i);
    expect(text).to.match(/bogus_message_for_test|unknown variant|malformed/i);
  });

  it("dismisses the toast when the close button is clicked", async function () {
    await browser.execute(
      `globalThis.__rt_daemon_client.send({ type: "another_bogus" });`,
    );

    // Wait for a toast referencing "another_bogus" to render.
    await browser.waitUntil(
      async () => {
        const allToasts = await browser.$$('[data-testid=error-toast]');
        const texts = await allToasts.map((t) => t.getText());
        return texts.some((t: string) => t.includes("another_bogus"));
      },
      {
        timeout: 5_000,
        timeoutMsg: "expected 'another_bogus' toast to appear",
      },
    );

    // Find that specific toast and click its close button.
    const allToasts = await browser.$$('[data-testid=error-toast]');
    let dismissed = false;
    for (const toast of allToasts) {
      const text = await toast.getText();
      if (text.includes("another_bogus")) {
        const closeBtn = await toast.$('[data-testid=error-toast-close]');
        await closeBtn.click();
        dismissed = true;
        break;
      }
    }
    expect(dismissed, "found and clicked dismiss on 'another_bogus' toast").to
      .be.true;

    await browser.waitUntil(
      async () => {
        const allToasts = await browser.$$('[data-testid=error-toast]');
        const texts = await allToasts.map((t) => t.getText());
        return !texts.some((t: string) => t.includes("another_bogus"));
      },
      {
        timeout: 5_000,
        timeoutMsg: "expected 'another_bogus' toast to be dismissed",
      },
    );
  });
});
