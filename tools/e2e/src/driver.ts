/**
 * Shared WebDriver setup. Used by both the long-lived host (`host.ts`) and
 * the WebdriverIO test runner (`wdio.conf.ts`).
 *
 * Boots `tauri-driver` as a child process and constructs a WebdriverIO
 * `Browser` against it. The Tauri binary path is discovered (or built on
 * demand) under `target/debug/`.
 */

import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import { existsSync } from "node:fs";
import { homedir, platform } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

import { remote, type Browser } from "webdriverio";

const here = fileURLToPath(new URL(".", import.meta.url));
export const REPO_ROOT = resolve(here, "..", "..", "..");

const TAURI_DRIVER_PORT = 4444;
const TAURI_DRIVER_HOST = "127.0.0.1";

function tauriDriverPath(): string {
  const exe = platform() === "win32" ? "tauri-driver.exe" : "tauri-driver";
  // tauri-driver always installs to ~/.cargo/bin via `cargo install`.
  return join(homedir(), ".cargo", "bin", exe);
}

export function tauriAppBinary(): string {
  const exe =
    platform() === "win32"
      ? "rustling-tulip-app.exe"
      : "rustling-tulip-app";
  return join(REPO_ROOT, "target", "debug", exe);
}

/**
 * Build the Tauri debug binary in-place. `--no-bundle` skips MSI/EXE
 * bundling — we only need the executable for WebDriver to launch.
 */
export function buildTauriDebug(): void {
  // eslint-disable-next-line no-console
  console.log(
    "[driver] building Tauri debug binary (this can take a few minutes the first time)…",
  );
  const result = spawnSync(
    "pnpm",
    ["tauri", "build", "--debug", "--no-bundle"],
    {
      cwd: join(REPO_ROOT, "apps", "tauri-app"),
      stdio: "inherit",
      shell: true,
    },
  );
  if (result.status !== 0) {
    throw new Error(
      `pnpm tauri build --debug --no-bundle exited with ${result.status}`,
    );
  }
}

export async function ensureTauriBinary(opts: {
  forceBuild?: boolean;
} = {}): Promise<string> {
  const path = tauriAppBinary();
  if (opts.forceBuild || !existsSync(path)) buildTauriDebug();
  if (!existsSync(path)) {
    throw new Error(
      `Tauri binary still missing after build: ${path}. ` +
        `Run \`pnpm tauri build --debug --no-bundle\` from apps/tauri-app and check the output.`,
    );
  }
  return path;
}

export interface DriverHandle {
  browser: Browser;
  tauriDriver: ChildProcess;
  appBinary: string;
}

export interface StartDriverOptions {
  /** Force a debug rebuild even if the binary already exists. */
  forceBuild?: boolean;
  /** Extra env vars merged into the Tauri child's environment. */
  env?: Record<string, string>;
  /**
   * Timeout for the WebDriver session to come up. tauri-driver itself is
   * fast; the long part is Tauri's first-frame paint.
   */
  sessionTimeoutMs?: number;
}

export async function startDriver(
  opts: StartDriverOptions = {},
): Promise<DriverHandle> {
  const appBinary = await ensureTauriBinary({
    ...(opts.forceBuild !== undefined && { forceBuild: opts.forceBuild }),
  });

  const tdPath = tauriDriverPath();
  if (!existsSync(tdPath)) {
    throw new Error(
      `tauri-driver not found at ${tdPath}. Install with: cargo install tauri-driver --locked`,
    );
  }

  // eslint-disable-next-line no-console
  console.log(`[driver] starting tauri-driver (${tdPath})`);
  const tauriDriver = spawn(tdPath, [], {
    stdio: ["ignore", "inherit", "inherit"],
  });

  // Give tauri-driver a moment to bind the port before WebdriverIO connects.
  await delay(500);

  const sessionEnv = { ...process.env, ...(opts.env ?? {}) };

  // WebdriverIO `remote()` only forwards env to the Tauri child via
  // tauri:options.env. tauri-driver merges this into the spawned app process.
  const browser = await remote({
    hostname: TAURI_DRIVER_HOST,
    port: TAURI_DRIVER_PORT,
    capabilities: {
      browserName: "wry",
      "tauri:options": {
        application: appBinary,
        // tauri-driver supports arguments + env passthrough.
        env: filterEnv(sessionEnv),
      },
      // WebdriverIO expects `browserVersion` to be set so the BiDi protocol
      // negotiation doesn't try to upgrade. Tauri-driver ignores it.
      browserVersion: "stable",
    },
    logLevel: "warn",
    waitforTimeout: opts.sessionTimeoutMs ?? 15_000,
    connectionRetryTimeout: 30_000,
    connectionRetryCount: 3,
  });

  return { browser, tauriDriver, appBinary };
}

/**
 * Forward only string env values to tauri-driver. Node's `process.env`
 * sometimes contains `undefined`s (esp. on Windows after deletion); the
 * WebDriver wire format requires strings.
 */
function filterEnv(env: NodeJS.ProcessEnv): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(env)) {
    if (typeof v === "string") out[k] = v;
  }
  return out;
}

export async function stopDriver(handle: DriverHandle): Promise<void> {
  try {
    await handle.browser.deleteSession();
  } catch {
    // best-effort; tauri-driver may have already gone
  }
  try {
    handle.tauriDriver.kill();
  } catch {
    // ditto
  }
  // Brief wait so tauri-driver releases its port before the next start.
  await delay(200);
}
