/**
 * WebdriverIO configuration for the canned e2e specs.
 *
 * On `onPrepare` we ensure the Tauri debug binary exists and shut down any
 * already-running daemon (so the new one inherits our `RUSTLING_TULIP_CLAUDE`
 * env). On `beforeSession` we spawn `tauri-driver`. The capabilities point
 * tauri-driver at the debug Tauri binary and pass the env through to the
 * spawned app process — the daemon supervisor will inherit it.
 */
import { spawn, type ChildProcess } from "node:child_process";
import { existsSync, mkdirSync, rmSync } from "node:fs";
import { unlink } from "node:fs/promises";
import { homedir, platform } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { captureFailureDump, printFailureDump } from "./src/diagnostics.js";
import { ensureTauriBinary } from "./src/driver.js";
import { handshakeFilePath } from "./src/handshake.js";
import { shutdownExistingDaemon } from "./src/lifecycle.js";

const here = fileURLToPath(new URL(".", import.meta.url));
const repoRoot = resolve(here, "..", "..");

// Per-run test config dir. The daemon's `paths::Dirs::ensure` and the Tauri
// app's `config_dir` both honor RUSTLING_TULIP_CONFIG_DIR; setting it here
// at module evaluation ensures `handshakeFilePath()` (called by
// `shutdownExistingDaemon` in onPrepare) and every downstream child process
// resolve to this directory instead of the user's real %APPDATA%. Without
// this, a crashed test could corrupt the user's real state.json, and
// leftover repos/workspaces from real use would contaminate specs.
const testConfigDir = join(repoRoot, ".tmp", "e2e", "config");
mkdirSync(testConfigDir, { recursive: true });
process.env["RUSTLING_TULIP_CONFIG_DIR"] = testConfigDir;

// Resolve the fake-claude shim once. The daemon's `claude_program()` honors
// RUSTLING_TULIP_CLAUDE; on Windows we point it at the .cmd shim, on POSIX
// at the bash one. The tauri:options.env block carries this through to the
// Tauri child — and from there to the daemon it spawns.
const fakeClaudePath =
  platform() === "win32"
    ? join(here, "fake-claude", "fake-claude.cmd")
    : join(here, "fake-claude", "fake-claude.sh");

const tauriBinary =
  platform() === "win32"
    ? join(repoRoot, "target", "debug", "rustling-tulip-app.exe")
    : join(repoRoot, "target", "debug", "rustling-tulip-app");

let tauriDriver: ChildProcess | null = null;
let exited = false;

export const config: WebdriverIO.Config = {
  runner: "local",
  specs: [join(here, "tests", "e2e", "specs", "*.spec.ts")],
  exclude: [],
  maxInstances: 1,

  capabilities: [
    {
      // No `browserName` — see comment in src/driver.ts. Setting it makes
      // msedgedriver open a separate Edge window instead of attaching to
      // the Tauri WebView.
      //
      // We do NOT set `env` here either: tauri-driver 2.0.6's TauriOptions
      // accepts only `application`, `args`, and (Windows) `webviewOptions`.
      // Any `env` field is silently ignored. To pass env to the Tauri child
      // we inject it into tauri-driver's own process env in beforeSession
      // — msedgedriver inherits it, and so does the Tauri app it spawns.
      "tauri:options": {
        application: tauriBinary,
      },
    },
  ],

  logLevel: "warn",
  bail: 0,
  baseUrl: "",
  waitforTimeout: 10_000,
  connectionRetryTimeout: 30_000,
  connectionRetryCount: 3,
  framework: "mocha",
  reporters: ["spec"],
  mochaOpts: {
    ui: "bdd",
    timeout: 120_000,
  },
  hostname: "127.0.0.1",
  port: 4444,

  onPrepare: async () => {
    // eslint-disable-next-line no-console
    console.log("[wdio] preparing — ensuring binaries + frontend are fresh…");
    // ensureTauriBinary is mtime-gated for the frontend and relies on
    // cargo's own incremental check for the Rust binaries (app + daemon).
    // On a no-change re-run this completes in well under a second; on a
    // change it rebuilds exactly what's stale.
    await ensureTauriBinary();
    if (!existsSync(fakeClaudePath)) {
      throw new Error(`fake-claude shim missing at ${fakeClaudePath}`);
    }
    // eslint-disable-next-line no-console
    console.log("[wdio] shutting down any existing daemon…");
    const { killed } = await shutdownExistingDaemon({ timeoutMs: 4_000 });
    // eslint-disable-next-line no-console
    console.log(
      killed
        ? "[wdio] killed existing daemon"
        : "[wdio] no existing daemon found",
    );
    // Belt-and-braces: a previous aborted run can leave a corrupt or stale
    // daemon.json behind that confuses both the Tauri supervisor and the
    // side-channel handshake reader. shutdownExistingDaemon unlinks on the
    // error path; do it unconditionally here so every run starts clean.
    await unlink(handshakeFilePath()).catch(() => undefined);

    // Wipe leftover test state (state.json, sessions/, logs/) so every run
    // starts hermetic. Done AFTER shutdownExistingDaemon so any prior test
    // daemon is gone — wiping while one is alive would orphan its process.
    rmSync(testConfigDir, { recursive: true, force: true });
    mkdirSync(testConfigDir, { recursive: true });
  },

  beforeSession: () => {
    const tdPath = join(
      homedir(),
      ".cargo",
      "bin",
      platform() === "win32" ? "tauri-driver.exe" : "tauri-driver",
    );
    if (!existsSync(tdPath)) {
      throw new Error(
        `tauri-driver not found at ${tdPath}. ` +
          "Install with: cargo install tauri-driver --locked",
      );
    }
    // Inject the fake-claude shim path and the test config dir into
    // tauri-driver's environment. The driver inherits its env to
    // msedgedriver, which in turn inherits it to the spawned Tauri app —
    // and the daemon supervisor inherits it to the daemon. This is the
    // only way to feed env to the app under tauri-driver 2.0.6 (the
    // `tauri:options.env` field is unsupported). `process.env` already
    // carries RUSTLING_TULIP_CONFIG_DIR from module load; making it
    // explicit here documents the contract.
    tauriDriver = spawn(tdPath, [], {
      stdio: ["ignore", "inherit", "inherit"],
      env: {
        ...process.env,
        RUSTLING_TULIP_CLAUDE: fakeClaudePath,
        RUSTLING_TULIP_CONFIG_DIR: testConfigDir,
      },
    });
    tauriDriver.on("error", (err) => {
      // eslint-disable-next-line no-console
      console.error("[wdio] tauri-driver error:", err);
      if (!exited) process.exit(1);
    });
    tauriDriver.on("exit", (code) => {
      if (!exited) {
        // eslint-disable-next-line no-console
        console.error(`[wdio] tauri-driver exited with ${code}`);
        process.exit(1);
      }
    });
  },

  afterSession: () => {
    exited = true;
    tauriDriver?.kill();
    tauriDriver = null;
  },

  // Failure-diagnostics hooks. These fire for BOTH regular tests and
  // before/after hooks — the spec-level `afterEach` would miss
  // before-hook crashes (Mocha skips afterEach when no test ran). On a
  // green run nothing is captured.
  afterTest: async (test, _context, result) => {
    if (result.passed) return;
    await dumpFailure(`${test.parent} ${test.title}`);
  },
  afterHook: async (test, _context, result) => {
    if (result.passed) return;
    const label = test?.title ?? "hook";
    await dumpFailure(`hook: ${label}`);
  },
};

async function dumpFailure(name: string): Promise<void> {
  try {
    const dump = await captureFailureDump(name);
    printFailureDump(name, dump);
  } catch (err) {
    // eslint-disable-next-line no-console
    console.error(`[diagnostics] capture failed: ${String(err)}`);
  }
}

const cleanup = (): void => {
  exited = true;
  tauriDriver?.kill();
};
process.on("exit", cleanup);
process.on("SIGINT", cleanup);
process.on("SIGTERM", cleanup);
process.on("SIGBREAK", cleanup);
