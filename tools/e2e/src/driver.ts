/**
 * Shared WebDriver setup. Used by both the long-lived host (`host.ts`) and
 * the WebdriverIO test runner (`wdio.conf.ts`).
 *
 * Boots `tauri-driver` as a child process and constructs a WebdriverIO
 * `Browser` against it. The Tauri binary path is discovered (or built on
 * demand) under `target/debug/`.
 */

import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import { existsSync, readdirSync, statSync, writeFileSync } from "node:fs";
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

function daemonBinary(): string {
  const exe =
    platform() === "win32" ? "rustling-tulipd.exe" : "rustling-tulipd";
  return join(REPO_ROOT, "target", "debug", exe);
}

/**
 * Recursively find the newest mtime under `root`. Returns 0 if the path is
 * absent. We use this to decide whether `dist/` is stale relative to the
 * frontend sources, so we only re-invoke vite when something actually
 * changed. Symlinks and node_modules-style directories are not expected
 * inside the watched paths, so we don't filter them.
 */
function newestMtimeMs(root: string): number {
  if (!existsSync(root)) return 0;
  const st = statSync(root);
  if (st.isFile()) return st.mtimeMs;
  let max = st.mtimeMs;
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const child = join(root, entry.name);
    const childMs = newestMtimeMs(child);
    if (childMs > max) max = childMs;
  }
  return max;
}

/**
 * Sentinel written next to `dist/index.html` whenever we rebuild in
 * dev-mode for E2E. Its presence (and freshness vs `index.html`) means
 * "the dist bundle has the dev shims (`__rt_console`, `__rt_terms`) the
 * diagnostics rely on". A subsequent `pnpm build` / `pnpm tauri build`
 * clobbers `dist/` without touching this marker, so the freshness check
 * spots the regression and forces a dev rebuild.
 */
const DEV_DIST_MARKER = join(
  REPO_ROOT,
  "apps",
  "tauri-app",
  "dist",
  ".e2e-dev-build",
);

/**
 * Cheap check for whether `dist/` needs to be rebuilt. Three triggers:
 *  1. `dist/index.html` is missing (first run / clobbered tree).
 *  2. A source/config file is newer than the dist entry.
 *  3. The dev-mode marker is missing or older than the dist entry — meaning
 *     a production build wrote the dist files and the dev shims are gone.
 */
function frontendNeedsRebuild(): boolean {
  const distEntry = join(REPO_ROOT, "apps", "tauri-app", "dist", "index.html");
  if (!existsSync(distEntry)) return true;
  const distMs = statSync(distEntry).mtimeMs;

  if (!existsSync(DEV_DIST_MARKER)) return true;
  if (statSync(DEV_DIST_MARKER).mtimeMs < distMs) return true;

  const watchPaths = [
    join(REPO_ROOT, "apps", "tauri-app", "src"),
    join(REPO_ROOT, "apps", "tauri-app", "index.html"),
    join(REPO_ROOT, "apps", "tauri-app", "vite.config.ts"),
    join(REPO_ROOT, "apps", "tauri-app", "tsconfig.json"),
    join(REPO_ROOT, "apps", "tauri-app", "tsconfig.app.json"),
    join(REPO_ROOT, "apps", "tauri-app", "package.json"),
  ];
  for (const p of watchPaths) {
    if (newestMtimeMs(p) > distMs) return true;
  }
  return false;
}

/**
 * Rebuild `apps/tauri-app/dist/` in vite's `development` mode. Skipping
 * minification + tree-shaking-for-prod shaves ~700ms off every test run.
 * The output is still a normal static bundle the Tauri binary loads from
 * disk — there's no dev-server dependency at runtime.
 */
function buildFrontendDev(): void {
  // eslint-disable-next-line no-console
  console.log("[driver] rebuilding frontend (vite --mode development)…");
  const result = spawnSync(
    "pnpm",
    ["exec", "vite", "build", "--mode", "development"],
    {
      cwd: join(REPO_ROOT, "apps", "tauri-app"),
      stdio: "inherit",
      shell: true,
    },
  );
  if (result.status !== 0) {
    throw new Error(`vite build exited with ${result.status}`);
  }
  // Stamp the dev-build marker AFTER the build so it's strictly newer than
  // `dist/index.html`. A later production build won't touch this marker.
  writeFileSync(DEV_DIST_MARKER, `built at ${new Date().toISOString()}\n`);
}

/**
 * Run `cargo build` for the Tauri app and the daemon in a single
 * invocation. Cargo's incremental compilation makes this a sub-second
 * no-op when nothing changed, but if either source tree was touched
 * (including `crates/daemon/`) the rebuild happens automatically — so the
 * test runner never silently exercises a stale daemon binary.
 *
 * The `custom-protocol` feature MUST be enabled on the Tauri app — without
 * it the binary loads `devUrl` (http://localhost:1420) at runtime instead
 * of the on-disk `dist/` bundle, and the WebView ends up at a
 * chrome-error page. `pnpm tauri build` auto-enables this feature; when
 * driving cargo directly we have to set it ourselves.
 */
function buildRustBinaries(): void {
  // eslint-disable-next-line no-console
  console.log(
    "[driver] cargo build -p rustling-tulip-app (custom-protocol) -p daemon…",
  );
  const result = spawnSync(
    "cargo",
    [
      "build",
      "-p",
      "rustling-tulip-app",
      "-F",
      "rustling-tulip-app/custom-protocol",
      "-p",
      "daemon",
    ],
    {
      cwd: REPO_ROOT,
      stdio: "inherit",
      shell: true,
    },
  );
  if (result.status !== 0) {
    throw new Error(`cargo build exited with ${result.status}`);
  }
}

/**
 * Make sure the Tauri binary, the daemon binary, and the frontend bundle
 * are up to date. Each step is independently mtime-gated, so the typical
 * cost on a no-change re-run is dominated by cargo's incremental check
 * (~300-500ms total). Pass `forceFrontend` to nuke the frontend mtime
 * gate; cargo always decides for itself.
 */
export async function ensureTauriBinary(opts: {
  forceFrontend?: boolean;
} = {}): Promise<string> {
  if (opts.forceFrontend || frontendNeedsRebuild()) {
    buildFrontendDev();
  } else {
    // eslint-disable-next-line no-console
    console.log("[driver] frontend bundle is fresh; skipping vite build");
  }
  buildRustBinaries();

  const appPath = tauriAppBinary();
  if (!existsSync(appPath)) {
    throw new Error(
      `Tauri binary missing after build: ${appPath}. ` +
        `Run \`cargo build -p rustling-tulip-app\` from the workspace root and check the output.`,
    );
  }
  const daemonPath = daemonBinary();
  if (!existsSync(daemonPath)) {
    throw new Error(
      `Daemon binary missing after build: ${daemonPath}. ` +
        `Run \`cargo build -p daemon\` from the workspace root and check the output.`,
    );
  }
  return appPath;
}

export interface DriverHandle {
  browser: Browser;
  tauriDriver: ChildProcess;
  appBinary: string;
}

export interface StartDriverOptions {
  /**
   * Force a full vite rebuild even if `dist/` looks fresh. The cargo build
   * is always run (cargo's own mtime check is fast and authoritative).
   */
  forceFrontend?: boolean;
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
    ...(opts.forceFrontend !== undefined && {
      forceFrontend: opts.forceFrontend,
    }),
  });

  const tdPath = tauriDriverPath();
  if (!existsSync(tdPath)) {
    throw new Error(
      `tauri-driver not found at ${tdPath}. Install with: cargo install tauri-driver --locked`,
    );
  }

  // eslint-disable-next-line no-console
  console.log(`[driver] starting tauri-driver (${tdPath})`);

  // tauri-driver 2.0.6's TauriOptions struct accepts only `application`,
  // `args`, and (Windows) `webviewOptions` — there is NO `env` field. To
  // pass env vars to the Tauri child we set them on tauri-driver's own
  // process; msedgedriver inherits them, and the spawned Tauri app
  // inherits them in turn (along with the daemon it supervises).
  const driverEnv = filterEnv({ ...process.env, ...(opts.env ?? {}) });
  const tauriDriver = spawn(tdPath, [], {
    stdio: ["ignore", "inherit", "inherit"],
    env: driverEnv,
  });

  // Give tauri-driver a moment to bind the port before WebdriverIO connects.
  await delay(500);

  // Critically: do NOT set `browserName: "wry"` here. The Tauri 2 docs
  // include both a Selenium example (which sets browserName) and a
  // WebdriverIO example (which does not). Setting it tells webdriverio to
  // negotiate against a real Edge browser session — msedgedriver opens its
  // own Edge window and the Tauri app's WebView ends up parked at
  // about:blank. Omitting the field lets the WebDriver session attach to
  // the Tauri WebView2 instance directly. This took an hour to bisect; the
  // doc inconsistency is real, the working incantation is "no browserName".
  const browser = await remote({
    hostname: TAURI_DRIVER_HOST,
    port: TAURI_DRIVER_PORT,
    capabilities: {
      "tauri:options": {
        application: appBinary,
      },
    },
    logLevel: "warn",
    waitforTimeout: opts.sessionTimeoutMs ?? 15_000,
    connectionRetryTimeout: 30_000,
    connectionRetryCount: 3,
  });

  return { browser, tauriDriver, appBinary };
}

/**
 * Forward only string env values. Node's `process.env` sometimes contains
 * `undefined`s (esp. on Windows after deletion); the child_process spawn
 * env contract requires `Record<string, string>`.
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
