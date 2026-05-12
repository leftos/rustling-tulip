//! Locate and (if needed) spawn the rustling-tulipd binary, then return its
//! handshake to the frontend.

use crate::{DaemonHandshake, handshake_file};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{debug, info, warn};

const HEALTH_TIMEOUT: Duration = Duration::from_millis(800);
const SPAWN_WAIT_TIMEOUT: Duration = Duration::from_secs(8);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Global lock around the "check + spawn" sequence. React 18 strict-mode
/// dev double-mounts the App component, which fires two
/// `invoke("ensure_daemon_started")` calls in quick succession. Without
/// serialization both calls see "no daemon" and both spawn one, leading
/// to two daemons racing for the handshake file and port. With the lock,
/// the second caller awaits, then either reuses the freshly-spawned
/// daemon's handshake or (if the first call failed) retries the spawn
/// itself. Pop-out windows also call this concurrently; same fix.
fn spawn_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub async fn ensure_running(_app: &tauri::AppHandle) -> Result<DaemonHandshake, String> {
    // Fast path: handshake already present and the daemon is healthy.
    // No reason to serialize concurrent readers in that case.
    if let Some(handshake) = load_existing_if_alive().await {
        info!(port = handshake.port, "reusing running daemon");
        return Ok(handshake);
    }

    // Slow path: a spawn might be needed. Take the global lock so
    // concurrent callers serialize. Once we hold it, re-check the
    // handshake -- the caller ahead of us may have spawned the daemon
    // already.
    let _guard = spawn_lock().lock().await;
    if let Some(handshake) = load_existing_if_alive().await {
        info!(
            port = handshake.port,
            "reusing daemon spawned by concurrent caller"
        );
        return Ok(handshake);
    }

    let bin = locate_daemon_binary()?;
    info!(?bin, "spawning daemon");
    spawn_daemon(&bin)?;

    wait_for_handshake().await
}

async fn load_existing_if_alive() -> Option<DaemonHandshake> {
    let path = handshake_file().ok()?;
    if !path.exists() {
        return None;
    }
    let bytes = tokio::fs::read(&path).await.ok()?;
    let parsed: DaemonHandshake = serde_json::from_slice(&bytes).ok()?;
    if probe_health(parsed.port).await {
        Some(parsed)
    } else {
        debug!(port = parsed.port, "stale handshake, daemon not responding");
        None
    }
}

async fn probe_health(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/health");
    let Ok(client) = reqwest::Client::builder().timeout(HEALTH_TIMEOUT).build() else {
        return false;
    };
    matches!(client.get(&url).send().await, Ok(resp) if resp.status().is_success())
}

fn locate_daemon_binary() -> Result<PathBuf, String> {
    let exe_name = if cfg!(windows) {
        "rustling-tulipd.exe"
    } else {
        "rustling-tulipd"
    };

    // 1. Sibling of the current executable (production install).
    if let Ok(current_exe) = std::env::current_exe()
        && let Some(dir) = current_exe.parent()
    {
        let candidate = dir.join(exe_name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    // 2. Workspace target dir (dev). Tauri sets CARGO_MANIFEST_DIR at compile
    //    time via build.rs; we rebuild the workspace-relative path from there.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = PathBuf::from(manifest_dir)
        .ancestors()
        .nth(3)
        .map(PathBuf::from)
        .ok_or_else(|| "could not resolve workspace root from CARGO_MANIFEST_DIR".to_string())?;
    let dev_candidate = workspace_root.join("target").join("debug").join(exe_name);
    if dev_candidate.is_file() {
        return Ok(dev_candidate);
    }
    let release_candidate = workspace_root.join("target").join("release").join(exe_name);
    if release_candidate.is_file() {
        return Ok(release_candidate);
    }

    Err(format!(
        "rustling-tulipd binary not found. Looked in:\n  \
         - <exe_dir>/{exe_name}\n  \
         - {}\n  \
         - {}\n\
         Run `cargo build -p daemon` from the workspace root.",
        dev_candidate.display(),
        release_candidate.display(),
    ))
}

#[cfg(windows)]
fn spawn_daemon(bin: &std::path::Path) -> Result<(), String> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    let mut cmd = Command::new(bin);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    cmd.spawn()
        .map(|child| {
            // We intentionally don't await the child — the daemon outlives us.
            drop(child);
        })
        .map_err(|e| format!("failed to spawn daemon: {e}"))
}

#[cfg(not(windows))]
fn spawn_daemon(bin: &std::path::Path) -> Result<(), String> {
    let mut cmd = Command::new(bin);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn()
        .map(|child| {
            drop(child);
        })
        .map_err(|e| format!("failed to spawn daemon: {e}"))
}

async fn try_load_handshake(path: &std::path::Path) -> Option<DaemonHandshake> {
    if !path.exists() {
        return None;
    }
    let bytes = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice::<DaemonHandshake>(&bytes).ok()
}

async fn wait_for_handshake() -> Result<DaemonHandshake, String> {
    let path = handshake_file()?;
    let deadline = tokio::time::Instant::now() + SPAWN_WAIT_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if let Some(parsed) = try_load_handshake(&path).await
            && probe_health(parsed.port).await
        {
            return Ok(parsed);
        }
        sleep(POLL_INTERVAL).await;
    }
    warn!("daemon never reported handshake");
    Err("daemon failed to start within the timeout".to_string())
}
