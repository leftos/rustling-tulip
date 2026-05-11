/**
 * Failure diagnostics for WebView E2E specs. Dumps what we can see in the
 * WebView when a test fails: the URL the WebView is currently parked at,
 * the page title, the captured app-side console (via `window.__rt_console`
 * installed by `installConsoleCapture` in dev mode), any WebDriver-side
 * browser logs we can pry out of msedgedriver, and a screenshot under
 * `.tmp/e2e/`.
 *
 * The goal is "never need to copy-paste from DevTools again". On a green
 * run nothing is written; on a red run the diagnostics land in the test
 * output and on disk so the next iteration can be data-driven.
 */
import { mkdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { browser } from "@wdio/globals";

const here = fileURLToPath(new URL(".", import.meta.url));
const REPO_ROOT = resolve(here, "..", "..", "..");
const DUMP_DIR = join(REPO_ROOT, ".tmp", "e2e");

interface ConsoleEntry {
  level: string;
  args: unknown[];
  ts: number;
}

export interface FailureDump {
  url: string | null;
  title: string | null;
  bodyHtmlSnippet: string | null;
  consoleEntries: ConsoleEntry[];
  webdriverLogs: unknown[];
  screenshotPath: string | null;
}

function safeName(s: string): string {
  return s.replace(/[^a-zA-Z0-9._-]+/g, "_").slice(0, 120);
}

/**
 * Collect everything we can about the WebView's state and write a
 * `<test>.{json,png}` pair under `.tmp/e2e/`. Returns the captured dump so
 * the caller can also print salient bits inline.
 */
export async function captureFailureDump(testName: string): Promise<FailureDump> {
  const dump: FailureDump = {
    url: null,
    title: null,
    bodyHtmlSnippet: null,
    consoleEntries: [],
    webdriverLogs: [],
    screenshotPath: null,
  };

  try {
    dump.url = await browser.getUrl();
  } catch {
    /* WebDriver may already be detached */
  }
  try {
    dump.title = await browser.getTitle();
  } catch {
    /* ignore */
  }
  try {
    dump.bodyHtmlSnippet = (await browser.execute(
      "return document.body ? document.body.innerHTML.slice(0, 2000) : null;",
    )) as unknown as string | null;
  } catch {
    /* ignore */
  }
  try {
    const entries = (await browser.execute(
      "return Array.isArray(window.__rt_console) ? window.__rt_console.map(function(e){ return { level: e.level, args: e.args.map(function(a){ try { return typeof a === 'string' ? a : JSON.stringify(a); } catch (_) { return String(a); } }), ts: e.ts }; }) : [];",
    )) as unknown as ConsoleEntry[];
    dump.consoleEntries = entries;
  } catch {
    /* ignore */
  }
  try {
    // msedgedriver/Chromedriver expose `GET /session/<id>/se/log` with
    // `{type: 'browser'}`. The `getLogs` shim in webdriverio calls that
    // endpoint. If the driver doesn't support it we just get an exception
    // and move on; the app-side console capture is the primary source.
    const wdLogs = await browser.getLogs("browser");
    dump.webdriverLogs = Array.isArray(wdLogs) ? wdLogs : [];
  } catch {
    /* not all drivers expose this */
  }
  try {
    mkdirSync(DUMP_DIR, { recursive: true });
    const stem = safeName(testName) || "failure";
    const png = join(DUMP_DIR, `${stem}.png`);
    await browser.saveScreenshot(png);
    dump.screenshotPath = png;
    writeFileSync(
      join(DUMP_DIR, `${stem}.json`),
      JSON.stringify(dump, null, 2),
      "utf8",
    );
  } catch {
    /* best-effort */
  }

  return dump;
}

/**
 * Pretty-print the most useful slice of a dump to stderr so it shows up
 * in test output without needing to open the JSON file.
 */
export function printFailureDump(testName: string, dump: FailureDump): void {
  const lines: string[] = [];
  lines.push(`\n  ─── E2E failure diagnostics: ${testName} ───`);
  lines.push(`  url:   ${dump.url ?? "(unknown)"}`);
  lines.push(`  title: ${dump.title ?? "(unknown)"}`);
  if (dump.consoleEntries.length > 0) {
    lines.push(`  __rt_console (${dump.consoleEntries.length} entries):`);
    for (const e of dump.consoleEntries.slice(-30)) {
      lines.push(`    [${e.level}] ${e.args.join(" ")}`);
    }
  } else {
    lines.push(`  __rt_console: (empty or unavailable)`);
  }
  if (dump.webdriverLogs.length > 0) {
    lines.push(`  webdriver-browser-logs (${dump.webdriverLogs.length} entries):`);
    for (const e of dump.webdriverLogs.slice(-30)) {
      lines.push(`    ${JSON.stringify(e)}`);
    }
  }
  if (dump.screenshotPath) {
    lines.push(`  screenshot: ${dump.screenshotPath}`);
  }
  if (dump.bodyHtmlSnippet) {
    lines.push(`  body[0..200]: ${dump.bodyHtmlSnippet.slice(0, 200)}`);
  }
  // eslint-disable-next-line no-console
  console.error(lines.join("\n"));
}
