/**
 * WebdriverIO configuration for the canned e2e specs.
 *
 * On `onPrepare` we ensure the Tauri debug binary exists and shut down any
 * already-running daemon (so the new one inherits our `RUSTLING_TULIP_CLAUDE`
 * env). On `beforeSession` we spawn `tauri-driver`. The capabilities point
 * tauri-driver at the debug Tauri binary and pass the env through to the
 * spawned app process — the daemon supervisor will inherit it.
 *
 * Specs run in parallel. See the per-worker namespacing below for what that
 * required and why it was previously impossible.
 */
import { spawn, type ChildProcess } from "node:child_process";
import { existsSync, mkdirSync, rmSync } from "node:fs";
import { unlink } from "node:fs/promises";
import { cpus, homedir, platform } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { captureFailureDump, printFailureDump } from "./src/diagnostics.js";
import { assertBinariesPresent, ensureTauriBinary } from "./src/driver.js";
import { handshakeFilePath } from "./src/handshake.js";
import { shutdownExistingDaemon } from "./src/lifecycle.js";

const here = fileURLToPath(new URL(".", import.meta.url));
const repoRoot = resolve(here, "..", "..");

// wdio forks one worker process per spec file and stamps that worker's cid
// into WDIO_WORKER_ID. Every resource the app tree keys off is namespaced by
// it: the config dir (and therefore daemon.json, and therefore *which daemon a
// spec talks to*), the worktrees root, the binary cache, the tracer pipe
// namespace, and both driver ports. Sharing any one of those is what pinned
// this suite to maxInstances: 1 — all workers would have found the same
// daemon.json and driven a single daemon, seeing each other's sessions.
//
// The binary cache in particular must not be shared: daemon/tracer reaping is
// scoped by executable path, so one worker's startup sweep would kill another
// worker's live tracers.
//
// The launcher process has no cid and keeps the shared root, which is what its
// one-time prepare/wipe pass wants.
const workerId = process.env["WDIO_WORKER_ID"] ?? "";
// cid is `<runner>-<spec>`. With the single capability this config declares,
// the runner index is always 0, so the spec index alone is unique across the
// run — port ranges never collide and never reuse a socket still in TIME_WAIT
// from an earlier spec. Adding a second capability would need this to account
// for the runner index too.
const workerSlot =
  Number.parseInt(workerId.split("-").pop() ?? "0", 10) || 0;
const testRoot = join(repoRoot, ".tmp", "e2e");
const workerRoot = workerId ? join(testRoot, `w-${workerId}`) : testRoot;
const testConfigDir = join(workerRoot, "config");
const testWorktreesDir = join(workerRoot, "worktrees");
const testBinariesDir = join(workerRoot, "binaries");
const testTracerPipePrefix = `rt-e2e-tracer-${workerSlot}`;
const driverPort = 4444 + workerSlot * 2;
const nativeDriverPort = driverPort + 1;

// Each worker drives a full Tauri app + WebView2 + daemon, so this is bounded
// by RAM and by how many WebView2 hosts the machine tolerates rather than by
// CPU. One worker per four logical CPUs, clamped to a sane range, and
// overridable for bisecting a parallelism-sensitive failure
// (`RT_E2E_WORKERS=1`).
const workerCount = Number.parseInt(process.env["RT_E2E_WORKERS"] ?? "", 10);
const maxInstances = Number.isFinite(workerCount) && workerCount > 0
  ? workerCount
  : Math.max(2, Math.min(6, Math.floor(cpus().length / 4)));

mkdirSync(testConfigDir, { recursive: true });
mkdirSync(testWorktreesDir, { recursive: true });
mkdirSync(testBinariesDir, { recursive: true });
process.env["RUSTLING_TULIP_CONFIG_DIR"] = testConfigDir;
process.env["RUSTLING_TULIP_WORKTREES_DIR"] = testWorktreesDir;
process.env["RUSTLING_TULIP_BINARIES_DIR"] = testBinariesDir;
process.env["RUSTLING_TULIP_TRACER_PIPE_PREFIX"] = testTracerPipePrefix;
process.env["VITE_RT_E2E"] = "1";

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
  maxInstances,

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
  port: driverPort,

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
    // The whole root goes, not just this process's dirs: it also holds the
    // previous run's per-worker trees, fixture repos, and screenshots.
    rmSync(testRoot, { recursive: true, force: true });
    mkdirSync(testConfigDir, { recursive: true });
    mkdirSync(testWorktreesDir, { recursive: true });
    mkdirSync(testBinariesDir, { recursive: true });
    // eslint-disable-next-line no-console
    console.log(`[wdio] running specs across ${maxInstances} workers`);
  },

  beforeSession: async () => {
    // WebdriverIO logs onPrepare failures but can still schedule workers, so
    // the binaries are rechecked before tauri-driver starts — a stale or
    // missing debug binary would otherwise launch into a dead devUrl page.
    // This is an existence check, not a build: onPrepare already ran the
    // build, and re-running cargo + vite once per spec file cost a second
    // each and serialized workers behind cargo's target-dir lock.
    assertBinariesPresent();

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
    // Per-session reset: kill any lingering daemon from the prior spec and
    // wipe daemon.json so the new Tauri session's supervisor spawns a fresh
    // daemon instead of inheriting a possibly-dying one. Daemons survive
    // their parent Tauri inconsistently on Windows (DETACHED_PROCESS +
    // job-object inheritance), and concurrent daemons race the rename of
    // daemon.json.tmp → daemon.json. Clean-slate per spec is simpler than
    // making the daemon's startup race-safe.
    await shutdownExistingDaemon({ timeoutMs: 4_000 });
    await unlink(handshakeFilePath()).catch(() => undefined);
    // Inject the fake-claude shim path and test roots into tauri-driver's
    // environment. The driver inherits its env to msedgedriver, which in turn
    // inherits it to the spawned Tauri app — and the daemon supervisor
    // inherits it to the daemon. This is the only way to feed env to the app
    // under tauri-driver 2.0.6 (the `tauri:options.env` field is unsupported).
    // RUSTLING_TULIP_OFFSCREEN_WINDOW tells the Tauri setup hook to create e2e
    // windows hidden, unfocused, and offscreen so test runs do not flash over
    // the user's workspace.
    // Both ports are per-worker: tauri-driver's own intermediary port (which
    // `config.port` above matches) and the msedgedriver it fronts. Leaving
    // either at its default would make the second concurrent worker fail to
    // bind.
    tauriDriver = spawn(
      tdPath,
      [
        "--port",
        String(driverPort),
        "--native-port",
        String(nativeDriverPort),
      ],
      {
        stdio: ["ignore", "inherit", "inherit"],
        env: {
          ...process.env,
          RUSTLING_TULIP_CLAUDE: fakeClaudePath,
          RUSTLING_TULIP_BINARIES_DIR: testBinariesDir,
          RUSTLING_TULIP_CONFIG_DIR: testConfigDir,
          RUSTLING_TULIP_OFFSCREEN_WINDOW: "1",
          RUSTLING_TULIP_TRACER_PIPE_PREFIX: testTracerPipePrefix,
          RUSTLING_TULIP_WORKTREES_DIR: testWorktreesDir,
        },
      },
    );
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

  afterSession: async () => {
    exited = true;
    await shutdownExistingDaemon({ timeoutMs: 4_000 }).catch((err: unknown) => {
      // eslint-disable-next-line no-console
      console.error("[wdio] daemon shutdown after session failed:", err);
    });
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
