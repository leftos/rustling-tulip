/**
 * `user-select: text` reaches content surfaces.
 *
 * The body has `user-select: none` to keep chrome (sidebar, tab bar, modals)
 * non-selectable for drag UX. Content surfaces (diff text, headless logs,
 * session label, terminal host, preset preview list) must re-enable selection
 * so users can copy what they see.
 *
 * This spec asserts the CSS rule is present in the loaded stylesheet by
 * injecting a synthetic element with each target class and reading
 * `getComputedStyle().userSelect`. That isolates the CSS rule from session
 * lifecycle (no daemon spawn / fake-claude / tab clicks needed).
 *
 * Before the fix at `apps/tauri-app/src/styles.css`, these elements would
 * inherit `none` from body. After: each reports `text`.
 */
import { browser } from "@wdio/globals";
import { expect } from "chai";

const APP_BOOT_TIMEOUT = 60_000;

// Every class that the body's user-select: none would otherwise leak into.
// Keep in sync with the rule block in apps/tauri-app/src/styles.css.
const CONTENT_SELECTORS = [
  "diff-pane",
  "headless-log",
  "preset-preview-list",
  "terminal-host",
] as const;

describe("user-select on content surfaces", function () {
  this.timeout(120_000);

  before(async function () {
    const root = await browser.$("[data-testid=app-root]");
    await root.waitForExist({ timeout: APP_BOOT_TIMEOUT });
  });

  it("body still blocks selection (chrome stays non-selectable)", async function () {
    const value = await readUserSelectOf("body");
    expect(value).to.equal("none");
  });

  for (const cls of CONTENT_SELECTORS) {
    it(`.${cls} re-enables text selection`, async function () {
      const value = await syntheticUserSelectFor(cls);
      expect(value, `.${cls} user-select`).to.equal("text");
    });
  }

  it(".session-title h2 (the session label) re-enables text selection", async function () {
    const value = await syntheticNestedUserSelect("session-title", "h2");
    expect(value).to.equal("text");
  });
});

async function readUserSelectOf(selector: string): Promise<string> {
  return (await browser.execute(
    `
    const el = document.querySelector(${JSON.stringify(selector)});
    if (!el) return "";
    const s = getComputedStyle(el);
    return s.userSelect || s.webkitUserSelect || "";
    `,
  )) as unknown as string;
}

/**
 * Inject a fresh element with the given class into <body>, read the computed
 * `user-select` style, then remove it. Avoids any dependency on the app
 * rendering this surface naturally — we're verifying the CSS rule reaches a
 * matching selector regardless of mount state.
 */
async function syntheticUserSelectFor(className: string): Promise<string> {
  return (await browser.execute(
    `
    const el = document.createElement('div');
    el.className = ${JSON.stringify(className)};
    document.body.appendChild(el);
    const s = getComputedStyle(el);
    const v = s.userSelect || s.webkitUserSelect || "";
    el.remove();
    return v;
    `,
  )) as unknown as string;
}

/**
 * Inject `<div class="<parent>"><h2 ...></div>` and read the user-select of
 * the inner element. Mirrors the .session-title h2 selector pattern used by
 * the session pane header.
 */
async function syntheticNestedUserSelect(
  parentClass: string,
  innerTag: string,
): Promise<string> {
  return (await browser.execute(
    `
    const parent = document.createElement('div');
    parent.className = ${JSON.stringify(parentClass)};
    const inner = document.createElement(${JSON.stringify(innerTag)});
    parent.appendChild(inner);
    document.body.appendChild(parent);
    const s = getComputedStyle(inner);
    const v = s.userSelect || s.webkitUserSelect || "";
    parent.remove();
    return v;
    `,
  )) as unknown as string;
}
