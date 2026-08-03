import { readFile } from "node:fs/promises";
import { homedir, platform } from "node:os";
import { dirname, join } from "node:path";

import protocolVersion from "../../../protocol-version.json" with { type: "json" };

/**
 * Mirror of `protocol::DaemonHandshake` (crates/protocol/src/lib.rs:18).
 * The daemon writes this to `daemon.json` on startup; we read it to discover
 * the WS port and the auth token clients must echo back in `Hello`.
 */
export interface DaemonHandshake {
  protocol_version: number;
  port: number;
  auth_token: string;
  pid: number;
}

/** Subset of `protocol::PROTOCOL_VERSION` we know how to speak. */
export const PROTOCOL_VERSION: number = protocolVersion.version;

/**
 * Reproduces `crates/daemon/src/paths.rs:Dirs::handshake_file` — the
 * path layout the `directories` crate produces from
 * `ProjectDirs::from("dev", "leftos", "rustling-tulip")`, with an env-var
 * override (`RUSTLING_TULIP_CONFIG_DIR`) that mirrors the daemon's and
 * Tauri app's resolution logic. The override lets the harness isolate test
 * runs to a tmpdir instead of writing to the user's real `%APPDATA%`.
 *
 *   Override : $RUSTLING_TULIP_CONFIG_DIR/daemon.json (when set)
 *   Windows  : %APPDATA%\leftos\rustling-tulip\config\daemon.json
 *   macOS    : $HOME/Library/Application Support/dev.leftos.rustling-tulip/daemon.json
 *   Linux    : ${XDG_CONFIG_HOME:-$HOME/.config}/rustling-tulip/daemon.json
 */
export function handshakeFilePath(): string {
  const override = process.env["RUSTLING_TULIP_CONFIG_DIR"];
  if (override !== undefined && override !== "") {
    return join(override, "daemon.json");
  }
  const plat = platform();
  if (plat === "win32") {
    const appData = process.env["APPDATA"];
    if (!appData) {
      throw new Error("APPDATA is not set; cannot locate daemon.json");
    }
    return join(appData, "leftos", "rustling-tulip", "config", "daemon.json");
  }
  if (plat === "darwin") {
    return join(
      homedir(),
      "Library",
      "Application Support",
      "dev.leftos.rustling-tulip",
      "daemon.json",
    );
  }
  const xdg = process.env["XDG_CONFIG_HOME"] ?? join(homedir(), ".config");
  return join(xdg, "rustling-tulip", "daemon.json");
}

/**
 * Read the UI's stable per-install `client_id` from the config dir (written by
 * the Tauri app's `get_client_identity`). Tab layouts are keyed per client_id,
 * so the side-channel harness must send the SAME id in its Hello for its
 * `create_tab` calls to land in the layout the UI actually renders. Returns
 * null if the file isn't there yet (the caller then omits client_id).
 */
export async function readClientId(): Promise<string | null> {
  try {
    const path = join(dirname(handshakeFilePath()), "client-id");
    const id = (await readFile(path, "utf8")).trim();
    return id.length > 0 ? id : null;
  } catch {
    return null;
  }
}

export async function readHandshake(): Promise<DaemonHandshake> {
  const path = handshakeFilePath();
  const bytes = await readFile(path, "utf8");
  const parsed = JSON.parse(bytes) as DaemonHandshake;
  if (parsed.protocol_version !== PROTOCOL_VERSION) {
    // Both numbers come from this checkout — the harness reads
    // protocol-version.json, the daemon bakes it in at build time. So a daemon
    // *behind* the harness means the running binary predates the current
    // source, not that the mirrors need updating. That happens when
    // `target/<profile>/rustling-tulipd.exe` is a plain copy rather than
    // cargo's hardlink: cargo reports "Finished" while leaving it untouched.
    const cause =
      parsed.protocol_version < PROTOCOL_VERSION
        ? "The running daemon binary is older than this checkout. Delete " +
          "target/debug/rustling-tulipd.exe and target/debug/rt-tracer.exe, " +
          "then re-run `cargo build -p daemon -p tracer`."
        : "The daemon is newer than the harness — update src/types.ts to " +
          "mirror the new protocol.";
    throw new Error(
      `daemon.json reports protocol_version=${parsed.protocol_version}, ` +
        `harness speaks ${PROTOCOL_VERSION}. ${cause}`,
    );
  }
  // Liveness: between sequential wdio specs, the previous Tauri session's
  // daemon dies but its daemon.json lingers on disk pointing at a dead port.
  // Reject stale handshakes so waitForHandshake keeps polling until the new
  // supervisor writes a fresh file with a live pid.
  if (!isPidAlive(parsed.pid)) {
    throw new Error(`stale daemon.json: pid ${parsed.pid} is not alive`);
  }
  return parsed;
}

/**
 * Probe whether `pid` belongs to a running process. `process.kill(pid, 0)`
 * never actually signals the target on POSIX (signal 0 is a no-op),
 * and on Node-on-Windows it likewise resolves to a liveness probe via
 * `OpenProcess` — see Node's libuv wrapper. Returns false on any error
 * (ESRCH for dead pid, EPERM for not-our-process which we treat as
 * "exists but inaccessible" → also "alive" in this context).
 */
function isPidAlive(pid: number): boolean {
  if (!Number.isInteger(pid) || pid <= 0) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    const code = (err as NodeJS.ErrnoException).code;
    // EPERM means the process exists but we lack rights to signal it.
    // Treat as alive — we don't need to signal, just probe.
    return code === "EPERM";
  }
}

/**
 * Poll `daemon.json` until the daemon reports a fresh handshake. We compare
 * pids so we don't latch onto a stale file from a previous run.
 *
 * `expectedFreshAfter` is a Date — any handshake whose pid matches a
 * still-living process AND whose file mtime is at/after that timestamp counts.
 */
export async function waitForHandshake(opts: {
  timeoutMs: number;
  pollIntervalMs?: number;
}): Promise<DaemonHandshake> {
  const interval = opts.pollIntervalMs ?? 100;
  const deadline = Date.now() + opts.timeoutMs;
  let lastErr: unknown = null;
  while (Date.now() < deadline) {
    try {
      return await readHandshake();
    } catch (err) {
      lastErr = err;
      await new Promise((r) => setTimeout(r, interval));
    }
  }
  throw new Error(
    `daemon handshake did not appear within ${opts.timeoutMs}ms` +
      (lastErr ? ` (last error: ${String(lastErr)})` : ""),
  );
}
