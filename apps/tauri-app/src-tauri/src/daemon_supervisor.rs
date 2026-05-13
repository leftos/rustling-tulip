//! Locate and (if needed) spawn the rustling-tulipd binary, then return its
//! handshake to the frontend.

use crate::{DaemonHandshake, handshake_file};
use anyhow::{Context as _, anyhow};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
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

    let template = locate_daemon_binary()?;
    let bin = cache_daemon_binary(&template).map_err(|e| e.to_string())?;

    // Reap any leftover daemon processes before spawning. We're in the slow
    // path because no healthy daemon was found, so any `rustling-tulipd.exe`
    // still running is a stale instance — most often a previous run that
    // crashed without removing `daemon.json`, or one that survived a
    // `cargo build` retry. Killing them avoids two daemons racing for the
    // handshake file when our fresh spawn comes up. Best-effort: failures
    // log and don't block startup.
    reap_orphan_daemons().await;

    info!(template = ?template, cached = ?bin, "spawning daemon from cache");
    // The daemon needs to find rt-tracer.exe to spawn supervisors. Once we
    // start running it from the cache dir, the "sibling of current_exe()"
    // lookup no longer points at the install/target dir where rt-tracer is
    // shipped. Tell the daemon where the templates actually live.
    let template_dir = template
        .parent()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| "template has no parent dir".to_string())?;
    spawn_daemon(&bin, &template_dir)?;

    wait_for_handshake().await
}

/// Find every `rustling-tulipd*` process (including content-addressed cache
/// copies named `rustling-tulipd-<hash>.exe`) and kill it. Called from the
/// spawn path; relies on the caller having already verified no healthy
/// daemon exists (otherwise we'd kill the live one). Skips our own pid as a
/// belt-and-braces guard — Tauri isn't named `rustling-tulipd`, but if a
/// future rename ever collided we'd want the safety net.
async fn reap_orphan_daemons() {
    let our_pid = std::process::id();
    let pids: Vec<u32> = enumerate_daemon_processes()
        .into_iter()
        .filter(|pid| *pid != our_pid)
        .collect();
    if pids.is_empty() {
        return;
    }
    info!(count = pids.len(), pids = ?pids, "reaping stale daemon processes");
    for pid in pids {
        if let Err(err) = crate::kill_pid(pid).await {
            warn!(pid, %err, "failed to reap stale daemon");
        }
    }
}

fn enumerate_daemon_processes() -> Vec<u32> {
    let mut sys = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::new().with_processes(sysinfo::ProcessRefreshKind::new()),
    );
    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::All,
        true,
        sysinfo::ProcessRefreshKind::new(),
    );
    let mut out = Vec::new();
    for (pid, proc_) in sys.processes() {
        if is_daemon_image(&proc_.name().to_string_lossy()) {
            out.push(pid.as_u32());
        }
    }
    out
}

/// Match `rustling-tulipd.exe`, `rustling-tulipd`, or any cached copy named
/// `rustling-tulipd-<hash>.exe`. The leading `-` after the stem prevents
/// matching unrelated processes that happen to share the prefix.
fn is_daemon_image(name: &str) -> bool {
    let stem = name
        .to_ascii_lowercase()
        .strip_suffix(".exe")
        .map_or_else(|| name.to_ascii_lowercase(), str::to_string);
    stem == "rustling-tulipd" || stem.starts_with("rustling-tulipd-")
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

/// Copy the daemon template into the content-addressed cache and return the
/// cached path. Spawning from the cached copy means the shipped template can
/// be replaced (rebuild, NSIS reinstall) without colliding with a running
/// daemon — the running process retains a handle on the cached file, the
/// shipped one is unlocked. The cache is shared with the daemon's
/// `binary_cache` module (resolved by the same `directories::ProjectDirs`
/// call) and pruned by the daemon at startup.
fn cache_daemon_binary(template: &Path) -> anyhow::Result<PathBuf> {
    let cache_dir = resolve_binaries_dir()?;
    fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating cache dir {}", cache_dir.display()))?;

    let bytes = fs::read(template)
        .with_context(|| format!("reading template {}", template.display()))?;
    let hash = hash_prefix(&bytes);
    let stem = template
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("rustling-tulipd");
    let extension = template
        .extension()
        .map(|e| e.to_string_lossy().into_owned());
    let filename = match extension.as_deref() {
        Some(ext) if !ext.is_empty() => format!("{stem}-{hash}.{ext}"),
        _ => format!("{stem}-{hash}"),
    };
    let cached = cache_dir.join(&filename);

    if cached.exists() {
        debug!(cached = %cached.display(), "daemon binary cache: hit");
        return Ok(cached);
    }

    let tmp = cache_dir.join(format!("{filename}.tmp"));
    fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
    if let Err(err) = fs::rename(&tmp, &cached) {
        if cached.exists() {
            let _ = fs::remove_file(&tmp);
            return Ok(cached);
        }
        return Err(err)
            .with_context(|| format!("renaming {} to {}", tmp.display(), cached.display()));
    }
    info!(cached = %cached.display(), "daemon binary cache: populated");
    Ok(cached)
}

fn resolve_binaries_dir() -> anyhow::Result<PathBuf> {
    if let Ok(value) = std::env::var("RUSTLING_TULIP_BINARIES_DIR")
        && !value.is_empty()
    {
        return Ok(PathBuf::from(value));
    }
    let pd = directories::ProjectDirs::from("dev", "leftos", "rustling-tulip")
        .ok_or_else(|| anyhow!("could not resolve binaries directory"))?;
    Ok(pd.data_local_dir().join("binaries"))
}

fn hash_prefix(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        let _ = write!(&mut hex, "{b:02x}");
    }
    hex.truncate(16);
    hex
}

#[cfg(windows)]
fn spawn_daemon(bin: &std::path::Path, template_dir: &std::path::Path) -> Result<(), String> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    let mut cmd = Command::new(bin);
    cmd.env("RUSTLING_TULIP_BIN_TEMPLATES", template_dir);
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
fn spawn_daemon(bin: &std::path::Path, template_dir: &std::path::Path) -> Result<(), String> {
    let mut cmd = Command::new(bin);
    cmd.env("RUSTLING_TULIP_BIN_TEMPLATES", template_dir);
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

#[cfg(test)]
mod tests {
    use super::is_daemon_image;

    #[test]
    fn matches_template_names() {
        assert!(is_daemon_image("rustling-tulipd.exe"));
        assert!(is_daemon_image("rustling-tulipd"));
        assert!(is_daemon_image("Rustling-TulipD.EXE"));
    }

    #[test]
    fn matches_cached_hashed_names() {
        assert!(is_daemon_image("rustling-tulipd-aaaaaaaaaaaaaaaa.exe"));
        assert!(is_daemon_image("rustling-tulipd-0123456789abcdef"));
    }

    #[test]
    fn rejects_unrelated_names() {
        assert!(!is_daemon_image("rustling-tulipdfoo.exe"));
        assert!(!is_daemon_image("rustling-tulip.exe")); // GUI app, not daemon
        assert!(!is_daemon_image("rt-tracer.exe"));
        assert!(!is_daemon_image(""));
    }
}
