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
import { existsSync } from "node:fs";
import { homedir, platform } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { ensureTauriBinary } from "./src/driver.js";
import { shutdownExistingDaemon } from "./src/lifecycle.js";

const here = fileURLToPath(new URL(".", import.meta.url));
const repoRoot = resolve(here, "..", "..");

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
  specs: [join(here, "tests", "e2e", "specs", "**", "*.spec.ts")],
  exclude: [],
  maxInstances: 1,

  capabilities: [
    {
      browserName: "wry",
      browserVersion: "stable",
      "tauri:options": {
        application: tauriBinary,
        env: {
          RUSTLING_TULIP_CLAUDE: fakeClaudePath,
          // Force dev mode so __rt_terms is exposed even in the built debug
          // binary. (Vite's `import.meta.env.DEV` is a build-time constant
          // resolved against MODE; setting it at runtime has no effect in
          // production builds, but the debug build embeds DEV=true.)
          NODE_ENV: "development",
        },
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
    console.log("[wdio] preparing — building Tauri debug binary if needed…");
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
    tauriDriver = spawn(tdPath, [], {
      stdio: ["ignore", "inherit", "inherit"],
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
};

const cleanup = (): void => {
  exited = true;
  tauriDriver?.kill();
};
process.on("exit", cleanup);
process.on("SIGINT", cleanup);
process.on("SIGTERM", cleanup);
process.on("SIGBREAK", cleanup);
