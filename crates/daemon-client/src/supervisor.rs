//! Locate and (if needed) spawn the rustling-tulipd binary, then return its
//! handshake to the caller.

use crate::handshake_file;
use anyhow::{Context as _, anyhow};
use protocol::DaemonHandshake;
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
// Bumped from 8s to 30s so a freshly-spawned daemon has time to finish
// `reattach_orphans` before the supervisor declares it stuck. Each
// per-session tracer reattach can take up to PIPE_CONNECT_TIMEOUT (15s)
// when the pipe is busy from a stale daemon connection — two sessions
// in that state already exceeds 8s.
const SPAWN_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_WAIT_TIMEOUT: Duration = Duration::from_secs(8);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How [`ensure_running`] treats a healthy daemon that is already running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetirePolicy {
    /// Retire a daemon whose protocol this client does not support or whose
    /// executable is not the current cached copy of this build's binary, then
    /// spawn this build's daemon.
    RetireStale,
    /// Reuse any healthy daemon whose protocol this client supports, whatever
    /// build it came from; retire only a protocol-incompatible one. A reused
    /// daemon needs no local daemon binary: one is located only to spawn.
    ReuseCompatible,
}

enum ExistingDaemon {
    Compatible(DaemonHandshake),
    Incompatible(DaemonHandshake),
    StaleBinary(DaemonHandshake),
    Missing,
}

/// The shape of [`ExistingDaemon`] without its handshake, for the pure
/// [`action_for`] decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExistingKind {
    Compatible,
    Incompatible,
    StaleBinary,
    Missing,
}

/// What [`ensure_running`] does about the daemon it found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Reuse,
    RetireThenSpawn,
    Spawn,
}

impl ExistingDaemon {
    fn kind(&self) -> ExistingKind {
        match self {
            Self::Compatible(_) => ExistingKind::Compatible,
            Self::Incompatible(_) => ExistingKind::Incompatible,
            Self::StaleBinary(_) => ExistingKind::StaleBinary,
            Self::Missing => ExistingKind::Missing,
        }
    }

    fn handshake(&self) -> Option<&DaemonHandshake> {
        match self {
            Self::Compatible(h) | Self::Incompatible(h) | Self::StaleBinary(h) => Some(h),
            Self::Missing => None,
        }
    }
}

fn action_for(existing: ExistingKind, policy: RetirePolicy) -> Action {
    match (existing, policy) {
        (ExistingKind::Compatible, _)
        | (ExistingKind::StaleBinary, RetirePolicy::ReuseCompatible) => Action::Reuse,
        (ExistingKind::Incompatible, _)
        | (ExistingKind::StaleBinary, RetirePolicy::RetireStale) => Action::RetireThenSpawn,
        (ExistingKind::Missing, _) => Action::Spawn,
    }
}

/// The handshake of `existing` when `policy` reuses it, logging the reuse
/// with `message` (or the different-build message for a stale binary).
fn reused_handshake(
    existing: &ExistingDaemon,
    policy: RetirePolicy,
    message: &str,
) -> Option<DaemonHandshake> {
    if action_for(existing.kind(), policy) != Action::Reuse {
        return None;
    }
    let handshake = existing.handshake()?.clone();
    let (port, protocol_version) = (handshake.port, handshake.protocol_version);
    if existing.kind() == ExistingKind::StaleBinary {
        info!(
            port,
            protocol_version, "reusing running daemon from a different build"
        );
    } else {
        info!(port, protocol_version, "{message}");
    }
    Some(handshake)
}

fn retire_reason(existing: ExistingKind) -> &'static str {
    if existing == ExistingKind::Incompatible {
        "protocol mismatch"
    } else {
        "daemon binary changed"
    }
}

struct CurrentDaemonBinary {
    template: PathBuf,
    cached: PathBuf,
}

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

/// Return the handshake of a healthy daemon this client can speak to,
/// spawning or replacing one when needed.
///
/// Under [`RetirePolicy::RetireStale`] a running daemon is reused only when its
/// protocol is supported and its executable is the current cached copy of the
/// shipped binary. Under [`RetirePolicy::ReuseCompatible`] any healthy daemon
/// with a supported protocol is reused, and no local daemon binary is located
/// unless one has to be spawned. A daemon the policy does not reuse is retired
/// (graceful `/shutdown`, then a force kill) and a fresh one is spawned from
/// the binary cache. Concurrent callers serialize on a process-wide lock so
/// only one of them spawns.
///
/// # Errors
///
/// Fails when the daemon binary is needed but cannot be located or cached, a
/// stale daemon cannot be stopped, the spawn fails, or the new daemon does not
/// report a healthy handshake within the spawn timeout.
pub async fn ensure_running(policy: RetirePolicy) -> anyhow::Result<DaemonHandshake> {
    // RetireStale retires a daemon whose executable is not this build's
    // cached binary, so it locates and caches the binary up front and a
    // missing binary is an error. ReuseCompatible compares best-effort: it
    // checks the running executable against the path the cache would hold,
    // only to log a reuse across builds, and locates a binary only to spawn.
    let current = match policy {
        RetirePolicy::RetireStale => Some(resolve_current_daemon_binary()?),
        RetirePolicy::ReuseCompatible => None,
    };
    let expected = match &current {
        Some(current) => Some(current.cached.clone()),
        None => expected_daemon_path(),
    };
    let cached = expected.as_deref();

    // Fast path: a healthy daemon the policy reuses.
    let existing = classify_existing_daemon(cached, policy).await;
    if let Some(handshake) = reused_handshake(&existing, policy, "reusing running daemon") {
        return Ok(handshake);
    }

    // Slow path: a spawn or graceful replacement might be needed. Take the
    // global lock so concurrent callers serialize. Once we hold it, re-check
    // the handshake -- the caller ahead of us may have spawned the daemon
    // already.
    let _guard = spawn_lock().lock().await;
    let existing = classify_existing_daemon(cached, policy).await;
    if let Some(handshake) = reused_handshake(
        &existing,
        policy,
        "reusing daemon spawned by concurrent caller",
    ) {
        return Ok(handshake);
    }
    // Resolve before retiring, so a missing binary never leaves the user with
    // no daemon at all.
    let current = match current {
        Some(current) => current,
        None => resolve_current_daemon_binary()?,
    };
    if action_for(existing.kind(), policy) == Action::RetireThenSpawn
        && let Some(handshake) = existing.handshake()
    {
        retire_daemon(handshake, retire_reason(existing.kind())).await?;
    }
    spawn_current_daemon(&current).await
}

fn resolve_current_daemon_binary() -> anyhow::Result<CurrentDaemonBinary> {
    let template = locate_daemon_binary()?;
    let cached = cache_daemon_binary(&template)?;
    Ok(CurrentDaemonBinary { template, cached })
}

async fn spawn_current_daemon(current: &CurrentDaemonBinary) -> anyhow::Result<DaemonHandshake> {
    let cache_dir = current
        .cached
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("cached daemon binary has no parent dir"))?;

    // Reap any leftover daemon processes before spawning. We're in the slow
    // path because no healthy daemon was found, so any `rustling-tulipd.exe`
    // still running from this instance's binary cache is a stale instance —
    // most often a previous run that crashed without removing `daemon.json`,
    // or one that survived a `cargo build` retry. Scope the sweep to our
    // binary cache so e2e and regular launchers cannot reap each other.
    // Best-effort: failures log and don't block startup.
    reap_orphan_daemons(&cache_dir).await;

    info!(
        template = ?current.template,
        cached = ?current.cached,
        "spawning daemon from cache"
    );
    // The daemon needs to find rt-tracer.exe to spawn supervisors. Once we
    // start running it from the cache dir, the "sibling of current_exe()"
    // lookup no longer points at the install/target dir where rt-tracer is
    // shipped. Tell the daemon where the templates actually live.
    let template_dir = current
        .template
        .parent()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| anyhow!("template has no parent dir"))?;
    spawn_daemon(&current.cached, &template_dir)?;

    wait_for_handshake().await
}

/// Classify the daemon named by `daemon.json`. With no `current_daemon` to
/// compare against, a healthy daemon with a supported protocol counts as
/// compatible whatever executable it runs. A daemon from a different build is
/// reported at warn only when `policy` retires it; a policy that reuses it
/// reports the difference at debug.
async fn classify_existing_daemon(
    current_daemon: Option<&Path>,
    policy: RetirePolicy,
) -> ExistingDaemon {
    let stale_is_warning = policy == RetirePolicy::RetireStale;
    if let Some(handshake) = load_existing_if_alive().await {
        if daemon_protocol_is_supported(handshake.protocol_version) {
            let Some(current_daemon) = current_daemon else {
                return ExistingDaemon::Compatible(handshake);
            };
            if running_daemon_matches_current_binary(
                handshake.pid,
                current_daemon,
                stale_is_warning,
            ) {
                ExistingDaemon::Compatible(handshake)
            } else {
                let (pid, port, protocol_version) =
                    (handshake.pid, handshake.port, handshake.protocol_version);
                let current_daemon = current_daemon.display();
                if stale_is_warning {
                    warn!(pid, port, protocol_version, %current_daemon, "running daemon binary is not current");
                } else {
                    debug!(pid, port, protocol_version, %current_daemon, "running daemon binary is not current");
                }
                ExistingDaemon::StaleBinary(handshake)
            }
        } else {
            warn!(
                daemon_protocol = handshake.protocol_version,
                supported = ?protocol::SUPPORTED_PROTOCOL_VERSIONS,
                port = handshake.port,
                pid = handshake.pid,
                "running daemon protocol is incompatible with this app"
            );
            ExistingDaemon::Incompatible(handshake)
        }
    } else {
        ExistingDaemon::Missing
    }
}

fn daemon_protocol_is_supported(protocol_version: u32) -> bool {
    protocol::SUPPORTED_PROTOCOL_VERSIONS.contains(&protocol_version)
}

fn running_daemon_matches_current_binary(
    pid: u32,
    expected: &Path,
    stale_is_warning: bool,
) -> bool {
    let Some(actual) = process_exe(pid) else {
        if stale_is_warning {
            warn!(pid, "could not inspect running daemon executable path");
        } else {
            debug!(pid, "could not inspect running daemon executable path");
        }
        return false;
    };
    let is_current = daemon_exe_matches_expected(&actual, expected);
    if !is_current {
        let (actual, expected) = (actual.display(), expected.display());
        if stale_is_warning {
            warn!(pid, %actual, %expected, "running daemon executable differs from current cached daemon");
        } else {
            debug!(pid, %actual, %expected, "running daemon executable differs from current cached daemon");
        }
    }
    is_current
}

fn process_exe(pid: u32) -> Option<PathBuf> {
    let pid = sysinfo::Pid::from_u32(pid);
    let process_refresh = sysinfo::ProcessRefreshKind::new().with_exe(sysinfo::UpdateKind::Always);
    let mut sys = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::new().with_processes(process_refresh),
    );
    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[pid]),
        true,
        process_refresh,
    );
    sys.process(pid)
        .and_then(|process| process.exe())
        .map(Path::to_path_buf)
}

fn daemon_exe_matches_expected(actual: &Path, expected: &Path) -> bool {
    normalize_process_path(actual) == normalize_process_path(expected)
}

async fn retire_daemon(handshake: &DaemonHandshake, reason: &str) -> anyhow::Result<()> {
    info!(
        pid = handshake.pid,
        port = handshake.port,
        protocol_version = handshake.protocol_version,
        supported = ?protocol::SUPPORTED_PROTOCOL_VERSIONS,
        reason,
        "retiring running daemon before spawning current version"
    );

    match request_graceful_shutdown(handshake).await {
        Ok(()) => {
            if wait_for_daemon_to_stop(handshake, SHUTDOWN_WAIT_TIMEOUT).await {
                remove_matching_handshake_file(handshake).await;
                return Ok(());
            }
            warn!(
                pid = handshake.pid,
                port = handshake.port,
                "daemon did not exit after graceful shutdown request"
            );
        }
        Err(err) => {
            warn!(
                ?err,
                pid = handshake.pid,
                port = handshake.port,
                "graceful daemon shutdown request failed"
            );
        }
    }

    if !probe_health(handshake.port).await {
        remove_matching_handshake_file(handshake).await;
        return Ok(());
    }

    warn!(
        pid = handshake.pid,
        port = handshake.port,
        "falling back to force-stopping daemon"
    );
    crate::kill_pid(handshake.pid).await?;
    let _ = wait_for_daemon_to_stop(handshake, SHUTDOWN_WAIT_TIMEOUT).await;
    remove_matching_handshake_file(handshake).await;
    Ok(())
}

async fn request_graceful_shutdown(handshake: &DaemonHandshake) -> anyhow::Result<()> {
    let url = shutdown_url(handshake.port);
    let client = reqwest::Client::builder()
        .timeout(HEALTH_TIMEOUT)
        .build()
        .context("building shutdown client")?;
    let response = client
        .post(url)
        .bearer_auth(&handshake.auth_token)
        .send()
        .await
        .context("sending shutdown request")?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!("shutdown endpoint returned {status}"));
    }
    Ok(())
}

fn shutdown_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/shutdown")
}

async fn wait_for_daemon_to_stop(handshake: &DaemonHandshake, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if !probe_health(handshake.port).await {
            return true;
        }
        sleep(POLL_INTERVAL).await;
    }
    false
}

async fn remove_matching_handshake_file(expected: &DaemonHandshake) {
    let Ok(path) = handshake_file() else {
        return;
    };
    let Some(current) = try_load_handshake(&path).await else {
        return;
    };
    if current.pid == expected.pid && current.port == expected.port {
        let _ = tokio::fs::remove_file(path).await;
    }
}

/// Find every `rustling-tulipd*` process launched from this instance's binary
/// cache and kill it. Called from the spawn path; relies on the caller having
/// already verified no healthy daemon exists in the current config dir.
/// Skips our own pid as a belt-and-braces guard.
async fn reap_orphan_daemons(cache_dir: &Path) {
    let our_pid = std::process::id();
    let pids: Vec<u32> = enumerate_daemon_processes(cache_dir)
        .into_iter()
        .filter(|pid| *pid != our_pid)
        .collect();
    if pids.is_empty() {
        return;
    }
    info!(count = pids.len(), pids = ?pids, "reaping stale daemon processes");
    for pid in pids {
        if let Err(err) = crate::kill_pid(pid).await {
            warn!(pid, err = %format!("{err:#}"), "failed to reap stale daemon");
        }
    }
}

fn enumerate_daemon_processes(cache_dir: &Path) -> Vec<u32> {
    let process_refresh = sysinfo::ProcessRefreshKind::new().with_exe(sysinfo::UpdateKind::Always);
    let mut sys = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::new().with_processes(process_refresh),
    );
    sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, process_refresh);
    let mut out = Vec::new();
    for (pid, proc_) in sys.processes() {
        if is_daemon_image(&proc_.name().to_string_lossy())
            && proc_.exe().is_some_and(|exe| path_is_under(exe, cache_dir))
        {
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

fn path_is_under(path: &Path, root: &Path) -> bool {
    let path = normalize_process_path(path);
    let root = normalize_process_path(root);
    path == root || path.starts_with(&format!("{root}{}", std::path::MAIN_SEPARATOR))
}

#[cfg(windows)]
fn normalize_process_path(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('/', "\\");
    let trimmed = raw
        .strip_prefix(r"\\?\UNC\")
        .map(|rest| format!(r"\\{rest}"))
        .or_else(|| raw.strip_prefix(r"\\?\").map(str::to_string))
        .unwrap_or(raw);
    trimmed.trim_end_matches('\\').to_ascii_lowercase()
}

#[cfg(not(windows))]
fn normalize_process_path(path: &Path) -> String {
    path.to_string_lossy().trim_end_matches('/').to_string()
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

/// Locate the shipped `rustling-tulipd` binary (the template the cache copies
/// from).
///
/// Looks next to the current executable first (production install), then in
/// the workspace's `target/debug` and `target/release` (dev).
///
/// # Errors
///
/// Fails, naming every path it tried, when none of them holds the binary.
pub fn locate_daemon_binary() -> anyhow::Result<PathBuf> {
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

    // 2. Workspace target dir (dev).
    let workspace_root = dev_workspace_root()?;
    let dev_candidate = workspace_root.join("target").join("debug").join(exe_name);
    if dev_candidate.is_file() {
        return Ok(dev_candidate);
    }
    let release_candidate = workspace_root.join("target").join("release").join(exe_name);
    if release_candidate.is_file() {
        return Ok(release_candidate);
    }

    Err(anyhow!(
        "rustling-tulipd binary not found. Looked in:\n  \
         - <exe_dir>/{exe_name}\n  \
         - {}\n  \
         - {}\n\
         Run `cargo build -p daemon` from the workspace root.",
        dev_candidate.display(),
        release_candidate.display(),
    ))
}

/// Workspace root this crate was compiled in, rebuilt from the compile-time
/// `CARGO_MANIFEST_DIR` (`<root>/crates/daemon-client`).
pub(crate) fn dev_workspace_root() -> anyhow::Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("could not resolve workspace root from CARGO_MANIFEST_DIR"))
}

/// Copy the daemon template into the content-addressed cache and return the
/// cached path. Spawning from the cached copy means the shipped template can
/// be replaced (rebuild, NSIS reinstall) without colliding with a running
/// daemon — the running process retains a handle on the cached file, the
/// shipped one is unlocked. The cache is shared with the daemon's
/// `binary_cache` module (resolved by the same `directories::ProjectDirs`
/// call) and pruned by the daemon at startup.
/// The content-addressed cache file name for `template`, whose contents are
/// `bytes`: `<stem>-<hash prefix>[.<ext>]`.
fn cached_file_name(template: &Path, bytes: &[u8]) -> String {
    let hash = hash_prefix(bytes);
    let stem = template
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("rustling-tulipd");
    let extension = template
        .extension()
        .map(|e| e.to_string_lossy().into_owned());
    match extension.as_deref() {
        Some(ext) if !ext.is_empty() => format!("{stem}-{hash}.{ext}"),
        _ => format!("{stem}-{hash}"),
    }
}

/// Where the cached copy of `template` lives, or would once cached, without
/// copying anything into the cache.
fn cached_daemon_path(template: &Path) -> anyhow::Result<PathBuf> {
    let bytes =
        fs::read(template).with_context(|| format!("reading template {}", template.display()))?;
    Ok(resolve_binaries_dir()?.join(cached_file_name(template, &bytes)))
}

/// Best effort: the cache path this build's daemon runs from, so a running
/// daemon from a different build can be told apart. `None` when no local
/// daemon binary is found, which only skips that comparison.
fn expected_daemon_path() -> Option<PathBuf> {
    locate_daemon_binary()
        .and_then(|template| cached_daemon_path(&template))
        .inspect_err(|err| debug!("not comparing the running daemon's build: {err:#}"))
        .ok()
}

fn cache_daemon_binary(template: &Path) -> anyhow::Result<PathBuf> {
    let cache_dir = resolve_binaries_dir()?;
    fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating cache dir {}", cache_dir.display()))?;

    let bytes =
        fs::read(template).with_context(|| format!("reading template {}", template.display()))?;
    let filename = cached_file_name(template, &bytes);
    let cached = cache_dir.join(&filename);

    if cached.exists() {
        debug!(cached = %cached.display(), "daemon binary cache: hit");
        return Ok(cached);
    }

    let tmp = cache_dir.join(format!("{filename}.tmp"));
    fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
    // `fs::write` yields a non-executable 0644 file on Unix; without the
    // template's mode the spawned daemon fails with EACCES. No-op on Windows.
    let template_perms = fs::metadata(template)
        .with_context(|| format!("reading template metadata {}", template.display()))?
        .permissions();
    fs::set_permissions(&tmp, template_perms)
        .with_context(|| format!("setting permissions on {}", tmp.display()))?;
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

/// Resolve the cached-binary root. Mirrors `resolve_binaries_dir` in
/// `crates/daemon/src/paths.rs` so the daemon and its clients share one cache.
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
fn spawn_daemon(bin: &std::path::Path, template_dir: &std::path::Path) -> anyhow::Result<()> {
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
        .context("failed to spawn daemon")
}

#[cfg(not(windows))]
fn spawn_daemon(bin: &std::path::Path, template_dir: &std::path::Path) -> anyhow::Result<()> {
    let mut cmd = Command::new(bin);
    cmd.env("RUSTLING_TULIP_BIN_TEMPLATES", template_dir);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn()
        .map(|child| {
            drop(child);
        })
        .context("failed to spawn daemon")
}

async fn try_load_handshake(path: &std::path::Path) -> Option<DaemonHandshake> {
    if !path.exists() {
        return None;
    }
    let bytes = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice::<DaemonHandshake>(&bytes).ok()
}

async fn wait_for_handshake() -> anyhow::Result<DaemonHandshake> {
    let path = handshake_file()?;
    let started = tokio::time::Instant::now();
    let deadline = started + SPAWN_WAIT_TIMEOUT;
    let mut next_progress = started + Duration::from_secs(1);
    while tokio::time::Instant::now() < deadline {
        if let Some(parsed) = try_load_handshake(&path).await
            && probe_health(parsed.port).await
        {
            info!(
                elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                port = parsed.port,
                protocol_version = parsed.protocol_version,
                "wait_for_handshake: daemon ready"
            );
            return Ok(parsed);
        }
        let now = tokio::time::Instant::now();
        if now >= next_progress {
            // One-line tick per second so we can see exactly how long the
            // supervisor waited vs when the daemon actually appeared. Without
            // this the only signal was a single failure message at timeout.
            let handshake_exists = tokio::fs::try_exists(&path).await.unwrap_or(false);
            debug!(
                elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                handshake_exists, "wait_for_handshake: still waiting"
            );
            next_progress = now + Duration::from_secs(1);
        }
        sleep(POLL_INTERVAL).await;
    }
    warn!(
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        timeout_secs = SPAWN_WAIT_TIMEOUT.as_secs(),
        "daemon never reported handshake"
    );
    Err(anyhow!("daemon failed to start within the timeout"))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{
        Action, ExistingKind, RetirePolicy, action_for, daemon_exe_matches_expected,
        daemon_protocol_is_supported, dev_workspace_root, is_daemon_image, path_is_under,
        shutdown_url,
    };
    use std::path::Path;

    #[test]
    fn retire_stale_reuses_compatible() {
        assert_eq!(
            action_for(ExistingKind::Compatible, RetirePolicy::RetireStale),
            Action::Reuse
        );
    }

    #[test]
    fn retire_stale_retires_stale_binary() {
        assert_eq!(
            action_for(ExistingKind::StaleBinary, RetirePolicy::RetireStale),
            Action::RetireThenSpawn
        );
    }

    #[test]
    fn retire_stale_retires_incompatible() {
        assert_eq!(
            action_for(ExistingKind::Incompatible, RetirePolicy::RetireStale),
            Action::RetireThenSpawn
        );
    }

    #[test]
    fn reuse_compatible_reuses_stale_binary() {
        assert_eq!(
            action_for(ExistingKind::StaleBinary, RetirePolicy::ReuseCompatible),
            Action::Reuse
        );
    }

    #[test]
    fn reuse_compatible_retires_incompatible() {
        assert_eq!(
            action_for(ExistingKind::Incompatible, RetirePolicy::ReuseCompatible),
            Action::RetireThenSpawn
        );
    }

    #[test]
    fn missing_spawns_under_both_policies() {
        for policy in [RetirePolicy::RetireStale, RetirePolicy::ReuseCompatible] {
            assert_eq!(
                action_for(ExistingKind::Missing, policy),
                Action::Spawn,
                "policy {policy:?}"
            );
        }
    }

    #[test]
    fn dev_fallback_root_is_workspace_root() {
        let root = dev_workspace_root().expect("workspace root resolves");
        let manifest = root.join("Cargo.toml");
        let text = std::fs::read_to_string(&manifest)
            .unwrap_or_else(|e| format!("<unreadable {}: {e}>", manifest.display()));
        assert!(
            text.lines().any(|line| line.trim() == "[workspace]"),
            "{} has no [workspace] table: {text}",
            manifest.display()
        );
    }

    #[test]
    fn current_protocol_is_supported() {
        assert!(daemon_protocol_is_supported(protocol::PROTOCOL_VERSION));
    }

    #[test]
    fn zero_protocol_is_not_supported() {
        assert!(!daemon_protocol_is_supported(0));
    }

    #[test]
    fn shutdown_url_targets_loopback_port() {
        assert_eq!(shutdown_url(51418), "http://127.0.0.1:51418/shutdown");
    }

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

    #[cfg(windows)]
    #[test]
    fn path_scope_matches_only_cache_children() {
        let root = Path::new(r"C:\rt\.tmp\e2e\binaries");
        assert!(path_is_under(
            Path::new(r"C:\rt\.tmp\e2e\binaries\rustling-tulipd-hash.exe"),
            root,
        ));
        assert!(!path_is_under(
            Path::new(r"C:\rt\.tmp\e2e\binaries-other\rustling-tulipd-hash.exe"),
            root,
        ));
    }

    #[cfg(windows)]
    #[test]
    fn daemon_exe_match_normalizes_windows_process_paths() {
        assert!(daemon_exe_matches_expected(
            Path::new(r"\\?\C:\rt\binaries\rustling-tulipd-aaaaaaaaaaaaaaaa.exe"),
            Path::new(r"C:\rt\binaries\rustling-tulipd-aaaaaaaaaaaaaaaa.exe"),
        ));
    }

    #[cfg(windows)]
    #[test]
    fn daemon_exe_match_rejects_stale_cached_daemon() {
        assert!(!daemon_exe_matches_expected(
            Path::new(r"C:\rt\binaries\rustling-tulipd-aaaaaaaaaaaaaaaa.exe"),
            Path::new(r"C:\rt\binaries\rustling-tulipd-bbbbbbbbbbbbbbbb.exe"),
        ));
    }

    #[cfg(not(windows))]
    #[test]
    fn path_scope_matches_only_cache_children() {
        let root = Path::new("/tmp/rt/.tmp/e2e/binaries");
        assert!(path_is_under(
            Path::new("/tmp/rt/.tmp/e2e/binaries/rustling-tulipd-hash"),
            root,
        ));
        assert!(!path_is_under(
            Path::new("/tmp/rt/.tmp/e2e/binaries-other/rustling-tulipd-hash"),
            root,
        ));
    }

    #[cfg(not(windows))]
    #[test]
    fn daemon_exe_match_rejects_stale_cached_daemon() {
        assert!(!daemon_exe_matches_expected(
            Path::new("/tmp/rt/binaries/rustling-tulipd-aaaaaaaaaaaaaaaa"),
            Path::new("/tmp/rt/binaries/rustling-tulipd-bbbbbbbbbbbbbbbb"),
        ));
    }
}
