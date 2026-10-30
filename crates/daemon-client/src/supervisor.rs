//! Locate and (if needed) spawn the rustling-tulipd binary, then return its
//! handshake to the caller.

#[cfg(windows)]
use crate::pinned_process::{OpenFailure, PinnedProcess};
use crate::proof::{self, Proof};
use crate::{handshake_file_in, remove_handshake_holding_token};
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

/// The budget for one proof exchange, and for the shutdown request's answer.
pub const PROOF_TIMEOUT: Duration = Duration::from_millis(800);
// Bumped from 8s to 30s so a freshly-spawned daemon has time to finish
// `reattach_orphans` before the supervisor declares it stuck. Each
// per-session tracer reattach can take up to PIPE_CONNECT_TIMEOUT (15s)
// when the pipe is busy from a stale daemon connection — two sessions
// in that state already exceeds 8s.
pub const SPAWN_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const SHUTDOWN_WAIT_TIMEOUT: Duration = Duration::from_secs(8);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

enum ExistingDaemon {
    Compatible(DaemonHandshake),
    Incompatible(DaemonHandshake),
    StaleBinary(DaemonHandshake),
    OlderBuild(DaemonHandshake),
    NewerBuild(DaemonHandshake),
    Missing,
}

/// The shape of [`ExistingDaemon`] without its handshake, for the pure
/// [`action_for`] decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExistingKind {
    Compatible,
    Incompatible,
    StaleBinary,
    OlderBuild,
    NewerBuild,
    Missing,
}

/// What [`ensure_running`] does about the daemon it found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Reuse,
    RetireThenSpawn,
    Spawn,
    Refuse,
}

impl ExistingDaemon {
    fn kind(&self) -> ExistingKind {
        match self {
            Self::Compatible(_) => ExistingKind::Compatible,
            Self::Incompatible(_) => ExistingKind::Incompatible,
            Self::StaleBinary(_) => ExistingKind::StaleBinary,
            Self::OlderBuild(_) => ExistingKind::OlderBuild,
            Self::NewerBuild(_) => ExistingKind::NewerBuild,
            Self::Missing => ExistingKind::Missing,
        }
    }

    fn handshake(&self) -> Option<&DaemonHandshake> {
        match self {
            Self::Compatible(h)
            | Self::Incompatible(h)
            | Self::StaleBinary(h)
            | Self::OlderBuild(h)
            | Self::NewerBuild(h) => Some(h),
            Self::Missing => None,
        }
    }
}

/// A healthy daemon of this client's build whose protocol the client speaks
/// is reused whatever executable it runs. A protocol-incompatible daemon and a
/// daemon of an older build are retired; a daemon of a newer build is never
/// touched, and the client refuses to go on.
fn action_for(existing: ExistingKind) -> Action {
    match existing {
        ExistingKind::Compatible | ExistingKind::StaleBinary => Action::Reuse,
        ExistingKind::Incompatible | ExistingKind::OlderBuild => Action::RetireThenSpawn,
        ExistingKind::NewerBuild => Action::Refuse,
        ExistingKind::Missing => Action::Spawn,
    }
}

/// How a daemon's build version orders against the client's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildOrder {
    Older,
    Same,
    Newer,
}

/// Orders `daemon_build` (a handshake's `build_version`) against
/// `client_build` by semver; a missing or unparsable version counts as older.
fn build_order(daemon_build: &str, client_build: &semver::Version) -> BuildOrder {
    let Ok(daemon_build) = semver::Version::parse(daemon_build) else {
        return BuildOrder::Older;
    };
    match daemon_build.cmp(client_build) {
        std::cmp::Ordering::Less => BuildOrder::Older,
        std::cmp::Ordering::Equal => BuildOrder::Same,
        std::cmp::Ordering::Greater => BuildOrder::Newer,
    }
}

/// The refusal [`ensure_running`] hands back when the running daemon is a
/// newer build than the client: such a daemon is never retired, and the
/// client is the one to update.
#[derive(Debug)]
pub struct DaemonIsNewer {
    /// The running daemon's build version.
    pub daemon: String,
    /// This client's build version.
    pub client: String,
}

impl std::fmt::Display for DaemonIsNewer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "daemon is newer ({}) than this client ({}); update the client",
            self.daemon, self.client
        )
    }
}

impl std::error::Error for DaemonIsNewer {}

/// A [`DaemonIsNewer`] error when `existing` is a newer build than
/// `client_build`, else `Ok`.
fn refuse_newer(existing: &ExistingDaemon, client_build: &semver::Version) -> anyhow::Result<()> {
    match existing {
        ExistingDaemon::NewerBuild(handshake) => Err(anyhow::Error::new(DaemonIsNewer {
            daemon: handshake.build_version.clone(),
            client: client_build.to_string(),
        })),
        _ => Ok(()),
    }
}

/// Why a daemon of `kind` is retired, for the retire log line.
fn retire_reason(kind: ExistingKind) -> &'static str {
    if kind == ExistingKind::OlderBuild {
        "older build"
    } else {
        "protocol mismatch"
    }
}

/// The handshake of `existing` when it is reused, logging the reuse with
/// `message` (or the different-build message for a stale binary).
fn reused_handshake(existing: &ExistingDaemon, message: &str) -> Option<DaemonHandshake> {
    if action_for(existing.kind()) != Action::Reuse {
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

struct CurrentDaemonBinary {
    template: PathBuf,
    cached: PathBuf,
}

/// Global lock around the "check + spawn" sequence. Two [`ensure_running`]
/// calls in one process (a client's reconnect overlapping an attempt it
/// abandoned, or tests running in parallel) would otherwise both see "no
/// daemon" and both spawn one, leaving two daemons racing for the handshake
/// file and port. With the lock, the second caller awaits, then either
/// reuses the freshly-spawned daemon's handshake or (if the first call
/// failed) retries the spawn itself.
fn spawn_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Return the handshake of a healthy daemon this client can speak to,
/// spawning or replacing one when needed.
///
/// A daemon counts as running only when the listener on the port `daemon.json`
/// names proves it holds that file's token (`GET /proof`); a listener that
/// does not is treated as no daemon and is never sent the token.
///
/// `client_versions` lists the protocol versions the calling client speaks; a
/// running daemon is protocol-compatible when it speaks at least one of them.
/// `client_build` is the calling client's own build version (its
/// `CARGO_PKG_VERSION`), ordered by semver against the daemon's
/// `build_version`.
///
/// A proven daemon of the client's build with a compatible protocol is reused,
/// whatever executable it runs, and no local daemon binary is located unless
/// one has to be spawned. A protocol-incompatible daemon, and a daemon of an
/// older build (a missing or unparsable `build_version` counts as older), is
/// retired (graceful `/shutdown`, then a force kill through a handle pinned
/// before its proof) and a fresh one is spawned from the binary cache. A
/// daemon of a newer build is never retired, whatever its protocol.
/// Concurrent callers serialize on a process-wide lock so only one of them
/// spawns.
///
/// # Errors
///
/// Fails with a [`DaemonIsNewer`] when the running daemon is a newer build
/// than this client. Fails when `client_build` is not a semver version, the
/// daemon binary is needed but cannot be located or cached, a stale daemon
/// cannot be stopped, the spawn fails, or the new daemon does not report a
/// healthy handshake within the spawn timeout.
pub async fn ensure_running(
    client_versions: &[u32],
    client_build: &str,
) -> anyhow::Result<DaemonHandshake> {
    let client_build = semver::Version::parse(client_build)
        .with_context(|| format!("this client's version {client_build:?} is not semver"))?;
    // Best-effort: the running executable is compared against the path the
    // cache would hold only to log a reuse across builds; a binary is located
    // for real only to spawn.
    let expected = expected_daemon_path();
    let cached = expected.as_deref();

    // Fast path: a healthy daemon that is reused.
    let existing = classify_existing_daemon(cached, client_versions, &client_build).await;
    if let Some(handshake) = reused_handshake(&existing, "reusing running daemon") {
        return Ok(handshake);
    }

    // Slow path: a spawn or graceful replacement might be needed. Take the
    // global lock so concurrent callers serialize. Once we hold it, re-check
    // the handshake -- the caller ahead of us may have spawned the daemon
    // already.
    let _guard = spawn_lock().lock().await;
    let existing = classify_existing_daemon(cached, client_versions, &client_build).await;
    if let Some(handshake) =
        reused_handshake(&existing, "reusing daemon spawned by concurrent caller")
    {
        return Ok(handshake);
    }
    refuse_newer(&existing, &client_build)?;
    // Resolve before retiring, so a missing binary never leaves the user with
    // no daemon at all.
    let current = resolve_current_daemon_binary()?;
    if action_for(existing.kind()) == Action::RetireThenSpawn
        && let Some(handshake) = existing.handshake()
    {
        retire_daemon(handshake, retire_reason(existing.kind()), client_versions).await?;
    }
    spawn_current_daemon(&current, client_versions).await
}

fn resolve_current_daemon_binary() -> anyhow::Result<CurrentDaemonBinary> {
    let template = locate_daemon_binary()?;
    let cached = cache_daemon_binary(&template)?;
    Ok(CurrentDaemonBinary { template, cached })
}

async fn spawn_current_daemon(
    current: &CurrentDaemonBinary,
    client_versions: &[u32],
) -> anyhow::Result<DaemonHandshake> {
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
    spawn_daemon(&current.cached, &template_dir).await?;

    wait_for_handshake(client_versions).await
}

/// Classify the daemon named by `daemon.json` in the config dir; see
/// [`classify_existing_daemon_in`].
async fn classify_existing_daemon(
    current_daemon: Option<&Path>,
    client_versions: &[u32],
    client_build: &semver::Version,
) -> ExistingDaemon {
    match crate::config_dir() {
        Ok(config_dir) => {
            let client = Client {
                versions: client_versions,
                build: client_build,
            };
            classify_existing_daemon_in(&config_dir, current_daemon, client, PROOF_TIMEOUT).await
        }
        Err(err) => {
            debug!("no config dir, so no daemon to classify: {err:#}");
            ExistingDaemon::Missing
        }
    }
}

/// Classify the daemon named by `config_dir`'s `daemon.json`: read once, and
/// classified from that parsed copy only when its listener proves it holds
/// the copy's token; anything else is [`ExistingDaemon::Missing`].
async fn classify_existing_daemon_in(
    config_dir: &Path,
    current_daemon: Option<&Path>,
    client: Client<'_>,
    proof_budget: Duration,
) -> ExistingDaemon {
    match load_existing_if_alive(config_dir, proof_budget).await {
        Some(handshake) => classify_proven(handshake, current_daemon, client),
        None => ExistingDaemon::Missing,
    }
}

/// What the calling client is: the protocol versions it speaks and its own
/// build version.
#[derive(Clone, Copy)]
struct Client<'a> {
    versions: &'a [u32],
    build: &'a semver::Version,
}

/// Classify a proven daemon. A daemon of a newer or an older build than the
/// client is classified by that alone, whatever its protocol. Otherwise, with
/// no `current_daemon` to compare against, a daemon whose protocol is
/// compatible with the client's counts as compatible whatever executable it
/// runs; a daemon running another executable is still reused, so the
/// difference is reported at debug.
fn classify_proven(
    handshake: DaemonHandshake,
    current_daemon: Option<&Path>,
    client: Client<'_>,
) -> ExistingDaemon {
    let client_versions = client.versions;
    match build_order(&handshake.build_version, client.build) {
        BuildOrder::Newer => {
            warn!(
                daemon_build = %handshake.build_version,
                client_build = %client.build,
                pid = handshake.pid,
                port = handshake.port,
                "running daemon is a newer build than this client; leaving it running"
            );
            return ExistingDaemon::NewerBuild(handshake);
        }
        BuildOrder::Older => {
            info!(
                daemon_build = %handshake.build_version,
                client_build = %client.build,
                pid = handshake.pid,
                port = handshake.port,
                "running daemon is an older build than this client"
            );
            return ExistingDaemon::OlderBuild(handshake);
        }
        BuildOrder::Same => {}
    }
    if protocol_kind(&handshake, client_versions) != ExistingKind::Compatible {
        warn!(
            daemon_protocol = handshake.protocol_version,
            daemon_supported = ?handshake.supported(),
            client_supported = ?client_versions,
            port = handshake.port,
            pid = handshake.pid,
            "running daemon protocol is incompatible with this app"
        );
        return ExistingDaemon::Incompatible(handshake);
    }
    let Some(current_daemon) = current_daemon else {
        return ExistingDaemon::Compatible(handshake);
    };
    if running_daemon_matches_current_binary(handshake.pid, current_daemon) {
        ExistingDaemon::Compatible(handshake)
    } else {
        let (pid, port, protocol_version) =
            (handshake.pid, handshake.port, handshake.protocol_version);
        let current_daemon = current_daemon.display();
        debug!(pid, port, protocol_version, %current_daemon, "running daemon binary is not current");
        ExistingDaemon::StaleBinary(handshake)
    }
}

/// Whether the daemon behind `daemon` speaks at least one of `client_versions`.
fn protocol_compatible(daemon: &DaemonHandshake, client_versions: &[u32]) -> bool {
    daemon
        .supported()
        .iter()
        .any(|version| client_versions.contains(version))
}

/// The protocol half of the classification: [`ExistingKind::Compatible`] when
/// the daemon and the client share a protocol version, else
/// [`ExistingKind::Incompatible`].
fn protocol_kind(daemon: &DaemonHandshake, client_versions: &[u32]) -> ExistingKind {
    if protocol_compatible(daemon, client_versions) {
        ExistingKind::Compatible
    } else {
        ExistingKind::Incompatible
    }
}

fn running_daemon_matches_current_binary(pid: u32, expected: &Path) -> bool {
    let Some(actual) = process_exe(pid) else {
        debug!(pid, "could not inspect running daemon executable path");
        return false;
    };
    let is_current = daemon_exe_matches_expected(&actual, expected);
    if !is_current {
        let (actual, expected) = (actual.display(), expected.display());
        debug!(pid, %actual, %expected, "running daemon executable differs from current cached daemon");
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

/// Retire the daemon `handshake` names from the config dir's `daemon.json`;
/// see [`retire_daemon_in`].
async fn retire_daemon(
    handshake: &DaemonHandshake,
    reason: &str,
    client_versions: &[u32],
) -> anyhow::Result<()> {
    info!(
        pid = handshake.pid,
        port = handshake.port,
        protocol_version = handshake.protocol_version,
        daemon_build = %handshake.build_version,
        daemon_supported = ?handshake.supported(),
        client_supported = ?client_versions,
        reason,
        "retiring running daemon before spawning current version"
    );
    let config_dir = crate::config_dir()?;
    retire_daemon_in(&config_dir, handshake, PROOF_TIMEOUT, SHUTDOWN_WAIT_TIMEOUT).await
}

/// Retire the daemon `handshake` (read from `config_dir`'s `daemon.json`)
/// names: pin its pid (Windows), open a connection, have the listener prove
/// it holds the token within `proof_budget`, then send the bearer `/shutdown`
/// on that connection. A daemon still alive after `shutdown_wait` is
/// force-stopped through the pin. `daemon.json` is removed only while it
/// still holds this token.
///
/// # Errors
///
/// Fails when the listener does not prove itself (it is sent nothing and
/// nothing is killed), so the caller does not replace a daemon it could not
/// retire; when this client may not terminate the daemon and it is still
/// running after the shutdown request; or when the force-stop fails. A pid
/// that is no longer running is `Ok`.
pub(crate) async fn retire_daemon_in(
    config_dir: &Path,
    handshake: &DaemonHandshake,
    proof_budget: Duration,
    shutdown_wait: Duration,
) -> anyhow::Result<()> {
    #[cfg(windows)]
    let pinned = match PinnedProcess::open(handshake.pid) {
        Ok(pinned) => Some(pinned),
        Err(OpenFailure::NotRunning) => {
            info!(
                pid = handshake.pid,
                "the daemon named by daemon.json is no longer running; nothing to retire"
            );
            remove_handshake_holding_token(config_dir, &handshake.auth_token).await;
            return Ok(());
        }
        Err(OpenFailure::NoRights(err)) => {
            warn!(
                pid = handshake.pid,
                %err,
                "this client may not terminate the daemon; asking it to shut down only"
            );
            None
        }
    };
    let stream = connect_then_prove(handshake, proof_budget).await?;
    request_shutdown(stream, handshake, proof_budget).await;
    #[cfg(windows)]
    {
        match &pinned {
            Some(pinned) => stop_pinned(pinned, handshake, shutdown_wait).await?,
            None => stop_by_request_only(handshake, proof_budget, shutdown_wait).await?,
        }
    }
    #[cfg(not(windows))]
    stop_unpinned(handshake, proof_budget, shutdown_wait).await?;
    remove_handshake_holding_token(config_dir, &handshake.auth_token).await;
    Ok(())
}

/// A connection to the daemon's port opened before its listener proved,
/// within `proof_budget`, that it holds the token.
///
/// # Errors
///
/// Fails (logging why) with a [`NotProven`] when the connection could not be
/// opened or the proof failed: the daemon classified as proven stopped proving
/// itself.
async fn connect_then_prove(
    handshake: &DaemonHandshake,
    proof_budget: Duration,
) -> anyhow::Result<tokio::net::TcpStream> {
    let port = handshake.port;
    let stream = match proof::connect(port, proof_budget).await {
        Ok(stream) => stream,
        Err(err) => {
            info!(port, err = %format!("{err:#}"), "nothing listens on the daemon's port");
            return Err(anyhow::Error::new(NotProven {
                port,
                proof: Proof::Refused,
            }));
        }
    };
    let proof = proof::prove(port, &handshake.auth_token, proof_budget).await;
    if proof != Proof::Proven {
        info!(
            port,
            ?proof,
            "daemon.json names a listener that did not prove it holds the token; nothing to retire"
        );
        return Err(anyhow::Error::new(NotProven { port, proof }));
    }
    Ok(stream)
}

/// The refusal a retire hands back when the listener on `port` did not prove
/// it holds the token `daemon.json` named, carrying what the proof said.
#[derive(Debug)]
pub(crate) struct NotProven {
    /// The port `daemon.json` named.
    pub(crate) port: u16,
    /// What the listener answered when asked to prove itself.
    pub(crate) proof: Proof,
}

impl std::fmt::Display for NotProven {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the daemon on 127.0.0.1:{} stopped proving it holds its token while being retired; \
             not replacing it",
            self.port
        )
    }
}

impl std::error::Error for NotProven {}

/// Sends the bearer `/shutdown` on `stream`; a failure is logged, since the
/// wait that follows decides whether the daemon stopped.
async fn request_shutdown(
    stream: tokio::net::TcpStream,
    handshake: &DaemonHandshake,
    budget: Duration,
) {
    if let Err(err) =
        proof::post_shutdown(stream, handshake.port, &handshake.auth_token, budget).await
    {
        warn!(
            err = %format!("{err:#}"),
            pid = handshake.pid,
            port = handshake.port,
            "graceful daemon shutdown request failed"
        );
    }
}

/// Waits for a daemon this client may not terminate to stop proving itself
/// after its shutdown request.
///
/// # Errors
///
/// Fails when it still proves itself after `shutdown_wait`.
#[cfg(windows)]
async fn stop_by_request_only(
    handshake: &DaemonHandshake,
    proof_budget: Duration,
    shutdown_wait: Duration,
) -> anyhow::Result<()> {
    if wait_until_unproven(handshake, proof_budget, shutdown_wait).await {
        return Ok(());
    }
    Err(anyhow!(
        "the daemon (pid {}) did not stop after a shutdown request, and this client may not \
         terminate it",
        handshake.pid
    ))
}

/// Waits for the pinned daemon to exit after its shutdown request, then
/// force-terminates it through the pin if it has not.
#[cfg(windows)]
async fn stop_pinned(
    pinned: &PinnedProcess,
    handshake: &DaemonHandshake,
    shutdown_wait: Duration,
) -> anyhow::Result<()> {
    if pinned.wait(shutdown_wait).await {
        return Ok(());
    }
    warn!(
        pid = handshake.pid,
        port = handshake.port,
        "daemon did not exit after graceful shutdown request; falling back to force-stopping daemon"
    );
    pinned.terminate()?;
    if !pinned.wait(shutdown_wait).await {
        warn!(
            pid = handshake.pid,
            "daemon did not exit after it was terminated"
        );
    }
    Ok(())
}

/// Waits for the daemon to stop proving itself after its shutdown request;
/// if it still does, proves it once more right before `kill -TERM`.
#[cfg(not(windows))]
async fn stop_unpinned(
    handshake: &DaemonHandshake,
    proof_budget: Duration,
    shutdown_wait: Duration,
) -> anyhow::Result<()> {
    if wait_until_unproven(handshake, proof_budget, shutdown_wait).await {
        return Ok(());
    }
    if proof::prove(handshake.port, &handshake.auth_token, proof_budget).await != Proof::Proven {
        return Ok(());
    }
    warn!(
        pid = handshake.pid,
        port = handshake.port,
        "daemon did not exit after graceful shutdown request; falling back to force-stopping daemon"
    );
    crate::kill_pid(handshake.pid).await?;
    if !wait_until_unproven(handshake, proof_budget, shutdown_wait).await {
        warn!(pid = handshake.pid, "daemon did not exit after kill -TERM");
    }
    Ok(())
}

/// Whether the daemon stopped proving it holds its token within `timeout`,
/// each proof given `proof_budget`.
async fn wait_until_unproven(
    handshake: &DaemonHandshake,
    proof_budget: Duration,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if proof::prove(handshake.port, &handshake.auth_token, proof_budget).await != Proof::Proven
        {
            return true;
        }
        sleep(POLL_INTERVAL).await;
    }
    false
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

/// The handshake in `config_dir`'s `daemon.json`, read once, when the listener
/// on its port proves it holds its token; `None` otherwise.
async fn load_existing_if_alive(
    config_dir: &Path,
    proof_budget: Duration,
) -> Option<DaemonHandshake> {
    let parsed = try_load_handshake(&handshake_file_in(config_dir)).await?;
    let port = parsed.port;
    match proof::prove(port, &parsed.auth_token, proof_budget).await {
        Proof::Proven => Some(parsed),
        proof @ (Proof::Unproven | Proof::NoResponse) => {
            info!(
                port,
                ?proof,
                "daemon.json names a listener that did not prove it holds the token; treating it as no daemon"
            );
            None
        }
        Proof::Refused => {
            debug!(port, "stale handshake, nothing listens on its port");
            None
        }
    }
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
pub(crate) fn locate_daemon_binary() -> anyhow::Result<PathBuf> {
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
async fn spawn_daemon(bin: &std::path::Path, template_dir: &std::path::Path) -> anyhow::Result<()> {
    spawn_daemon_with(bin, &[], template_dir, crate::wmi_launch::launch)
        .await
        .map(drop)
}

/// How long the WMI launch may take before the daemon is spawned inside the
/// caller's job instead.
#[cfg(windows)]
const WMI_LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Starts the daemon outside the caller's job and returns its pid: with
/// `CREATE_BREAKAWAY_FROM_JOB`, else (the job forbids breakaway) through
/// `launch_outside_job`, else inside the caller's job, so it always starts.
///
/// An SSH session, a terminal, an IDE task runner or a supervisor can run its
/// command in a job that kills every process in it when it closes, and the
/// daemon (with every tracer it starts) must outlive whoever started it.
#[cfg(windows)]
async fn spawn_daemon_with(
    bin: &std::path::Path,
    args: &[std::ffi::OsString],
    template_dir: &std::path::Path,
    launch_outside_job: impl FnOnce(
        &std::path::Path,
        &[std::ffi::OsString],
        &std::path::Path,
    ) -> anyhow::Result<u32>
    + Send
    + 'static,
) -> anyhow::Result<u32> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    /// What `CreateProcess` returns when the caller's job forbids breakaway.
    const ERROR_ACCESS_DENIED: i32 = 5;

    let flags = CREATE_NO_WINDOW | DETACHED_PROCESS;
    match spawn_daemon_with_flags(bin, args, template_dir, flags | CREATE_BREAKAWAY_FROM_JOB) {
        Ok(child) => {
            info!("daemon launched with breakaway from the caller's job");
            return pid_of(&child);
        }
        Err(err) if err.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {}
        Err(err) => return Err(err).context("failed to spawn daemon"),
    }
    match launch_bounded(bin, args, template_dir, launch_outside_job).await {
        Ok(pid) => {
            info!("daemon launched through WMI, outside the caller's job (it forbids breakaway)");
            return Ok(pid);
        }
        Err(err) => warn!("the WMI launch of the daemon failed: {err:#}"),
    }
    let child = spawn_daemon_with_flags(bin, args, template_dir, flags)
        .context("failed to spawn daemon")?;
    info!("daemon launched inside the caller's job (WMI launch failed)");
    pid_of(&child)
}

/// Runs `launch` on a blocking thread, so a slow WMI call never stalls the
/// caller's runtime, and gives up after [`WMI_LAUNCH_TIMEOUT`].
#[cfg(windows)]
async fn launch_bounded(
    bin: &std::path::Path,
    args: &[std::ffi::OsString],
    template_dir: &std::path::Path,
    launch: impl FnOnce(
        &std::path::Path,
        &[std::ffi::OsString],
        &std::path::Path,
    ) -> anyhow::Result<u32>
    + Send
    + 'static,
) -> anyhow::Result<u32> {
    let bin = bin.to_path_buf();
    let args = args.to_vec();
    let template_dir = template_dir.to_path_buf();
    let task = tokio::task::spawn_blocking(move || launch(&bin, &args, &template_dir));
    match tokio::time::timeout(WMI_LAUNCH_TIMEOUT, task).await {
        Ok(Ok(launched)) => launched,
        Ok(Err(join)) => Err(anyhow!("the WMI launch task ended abnormally: {join}")),
        Err(_) => Err(anyhow!(
            "timed out after {} s",
            WMI_LAUNCH_TIMEOUT.as_secs()
        )),
    }
}

/// The pid of a just-spawned daemon. The child is not awaited: the daemon
/// outlives us.
#[cfg(windows)]
fn pid_of(child: &tokio::process::Child) -> anyhow::Result<u32> {
    child
        .id()
        .ok_or_else(|| anyhow!("the spawned daemon has no pid"))
}

#[cfg(windows)]
fn spawn_daemon_with_flags(
    bin: &std::path::Path,
    args: &[std::ffi::OsString],
    template_dir: &std::path::Path,
    creation_flags: u32,
) -> std::io::Result<tokio::process::Child> {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    cmd.env("RUSTLING_TULIP_BIN_TEMPLATES", template_dir);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(creation_flags);
    cmd.spawn()
}

#[cfg(not(windows))]
#[expect(
    clippy::unused_async,
    reason = "async in step with the Windows spawn, which awaits the WMI launch"
)]
async fn spawn_daemon(bin: &std::path::Path, template_dir: &std::path::Path) -> anyhow::Result<()> {
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

/// Wait for the config dir's `daemon.json` to name a ready daemon; see
/// [`wait_for_handshake_in`].
async fn wait_for_handshake(client_versions: &[u32]) -> anyhow::Result<DaemonHandshake> {
    wait_for_handshake_in(
        &crate::config_dir()?,
        client_versions,
        SPAWN_WAIT_TIMEOUT,
        PROOF_TIMEOUT,
    )
    .await
}

/// The handshake `path` holds when its listener proves it holds the token and
/// speaks one of `client_versions`. A proven but incompatible daemon is
/// warned about once per wait (`warned` remembers it).
async fn ready_handshake(
    path: &Path,
    client_versions: &[u32],
    warned: &mut bool,
    proof_budget: Duration,
) -> Option<DaemonHandshake> {
    let parsed = try_load_handshake(path).await?;
    if proof::prove(parsed.port, &parsed.auth_token, proof_budget).await != Proof::Proven {
        return None;
    }
    if protocol_compatible(&parsed, client_versions) {
        return Some(parsed);
    }
    if !*warned {
        warn!(
            port = parsed.port,
            daemon_supported = ?parsed.supported(),
            client_supported = ?client_versions,
            "daemon.json names a daemon that speaks none of this client's protocol versions; still waiting"
        );
        *warned = true;
    }
    None
}

/// Wait up to `timeout` for `config_dir`'s `daemon.json` to name a daemon
/// that proves it holds the file's token and speaks one of
/// `client_versions`. A stale file never satisfies the wait; the daemon's pid
/// need not be the one this client spawned.
async fn wait_for_handshake_in(
    config_dir: &Path,
    client_versions: &[u32],
    timeout: Duration,
    proof_budget: Duration,
) -> anyhow::Result<DaemonHandshake> {
    let path = handshake_file_in(config_dir);
    let started = tokio::time::Instant::now();
    let deadline = started + timeout;
    let mut next_progress = started + Duration::from_secs(1);
    let mut warned_incompatible = false;
    while tokio::time::Instant::now() < deadline {
        if let Some(parsed) = ready_handshake(
            &path,
            client_versions,
            &mut warned_incompatible,
            proof_budget,
        )
        .await
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
        timeout_secs = timeout.as_secs(),
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
        Action, Client, ExistingKind, action_for, classify_existing_daemon_in, classify_proven,
        daemon_exe_matches_expected, dev_workspace_root, is_daemon_image, path_is_under,
        protocol_compatible, protocol_kind, refuse_newer, wait_for_handshake_in,
    };
    #[cfg(windows)]
    use super::{spawn_daemon, spawn_daemon_with};
    use crate::supervisor::retire_daemon_in;
    use crate::test_fakes::{
        FakeListener, ScratchDir, block_on, handshake_naming, stand_in, write_daemon_json,
    };
    use protocol::DaemonHandshake;
    #[cfg(windows)]
    use std::ffi::OsString;
    #[cfg(windows)]
    use std::io::{Read as _, Write as _};
    #[cfg(windows)]
    use std::os::windows::io::AsRawHandle as _;
    use std::path::Path;
    #[cfg(windows)]
    use std::process::Stdio;
    use std::time::Duration;
    #[cfg(windows)]
    use std::time::Instant;
    #[cfg(windows)]
    use tracing::warn;
    #[cfg(windows)]
    use windows::Win32::Foundation::{CloseHandle, HANDLE, STILL_ACTIVE};
    #[cfg(windows)]
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT,
        JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_LIMIT_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    };
    #[cfg(windows)]
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_TERMINATE, TerminateProcess,
    };
    #[cfg(windows)]
    use windows::core::{BOOL, PCWSTR};

    const SUPPORTED: &[u32] = protocol::SUPPORTED_PROTOCOL_VERSIONS;
    /// Only a hang guard: no test asserts how long anything took.
    const HANG_GUARD: Duration = Duration::from_secs(10);
    /// The client's build version: this crate's, which is the workspace's.
    const CLIENT_BUILD: &str = env!("CARGO_PKG_VERSION");

    /// The calling client: speaking [`SUPPORTED`], built as [`CLIENT_BUILD`].
    fn client() -> Client<'static> {
        static BUILD: std::sync::OnceLock<semver::Version> = std::sync::OnceLock::new();
        Client {
            versions: SUPPORTED,
            build: BUILD.get_or_init(|| {
                semver::Version::parse(CLIENT_BUILD).expect("the crate version is semver")
            }),
        }
    }

    /// A handshake of `build` speaking `supported_versions`.
    fn of_build(build: &str, supported_versions: &[u32]) -> DaemonHandshake {
        DaemonHandshake {
            build_version: build.to_owned(),
            ..handshake(supported_versions[0], supported_versions)
        }
    }

    /// What `ensure_running` does about a proven `daemon`, judged by [`client`].
    fn action_on(daemon: DaemonHandshake) -> (ExistingKind, Action) {
        let kind = classify_proven(daemon, None, client()).kind();
        (kind, action_for(kind))
    }

    fn handshake(protocol_version: u32, supported_versions: &[u32]) -> DaemonHandshake {
        DaemonHandshake {
            protocol_version,
            port: 40123,
            auth_token: "token".to_owned(),
            pid: 7,
            supported_versions: supported_versions.to_vec(),
            build_version: CLIENT_BUILD.to_owned(),
        }
    }

    #[test]
    fn reuses_compatible() {
        assert_eq!(action_for(ExistingKind::Compatible), Action::Reuse);
    }

    #[test]
    fn reuses_stale_binary() {
        assert_eq!(action_for(ExistingKind::StaleBinary), Action::Reuse);
    }

    #[test]
    fn retires_incompatible() {
        assert_eq!(
            action_for(ExistingKind::Incompatible),
            Action::RetireThenSpawn
        );
    }

    #[test]
    fn missing_spawns() {
        assert_eq!(action_for(ExistingKind::Missing), Action::Spawn);
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
    fn current_protocol_is_compatible_with_itself() {
        let daemon = handshake(
            protocol::PROTOCOL_VERSION,
            protocol::SUPPORTED_PROTOCOL_VERSIONS,
        );
        assert!(protocol_compatible(
            &daemon,
            protocol::SUPPORTED_PROTOCOL_VERSIONS
        ));
    }

    #[test]
    fn zero_protocol_is_not_compatible() {
        assert!(!protocol_compatible(
            &handshake(0, &[]),
            protocol::SUPPORTED_PROTOCOL_VERSIONS
        ));
    }

    #[test]
    fn native_client_retires_daemon_that_speaks_only_an_older_version() {
        let daemon = handshake(24, &[24]);
        assert_eq!(
            action_for(protocol_kind(&daemon, &[25])),
            Action::RetireThenSpawn
        );
    }

    #[test]
    fn native_client_reuses_daemon_speaking_25() {
        let daemon = handshake(25, &[25]);
        assert!(protocol_compatible(&daemon, &[25]));
        assert_eq!(action_for(protocol_kind(&daemon, &[25])), Action::Reuse);
    }

    #[test]
    fn a_compatible_daemon_of_an_older_build_is_retired() {
        for older in ["0.0.1", "0.0.1-alpha"] {
            assert_eq!(
                action_on(of_build(older, SUPPORTED)),
                (ExistingKind::OlderBuild, Action::RetireThenSpawn),
                "daemon build {older}"
            );
        }
    }

    #[test]
    fn a_daemon_of_a_newer_build_is_never_retired() {
        for supported in [SUPPORTED, &[0]] {
            let daemon = of_build("999.0.0", supported);
            assert_eq!(
                action_on(daemon.clone()),
                (ExistingKind::NewerBuild, Action::Refuse),
                "daemon speaking {supported:?}"
            );
            let refusal = refuse_newer(&classify_proven(daemon, None, client()), client().build)
                .expect_err("a newer daemon is refused");
            assert_eq!(
                refusal.to_string(),
                format!(
                    "daemon is newer (999.0.0) than this client ({CLIENT_BUILD}); update the client"
                )
            );
        }
    }

    #[test]
    fn a_daemon_of_the_same_build_is_reused() {
        assert_eq!(
            action_on(of_build(CLIENT_BUILD, SUPPORTED)),
            (ExistingKind::Compatible, Action::Reuse)
        );
        assert_eq!(
            action_on(of_build(CLIENT_BUILD, &[0])),
            (ExistingKind::Incompatible, Action::RetireThenSpawn),
            "a same-build daemon of another protocol is retired as before"
        );
    }

    #[test]
    fn a_handshake_without_build_version_counts_as_older() {
        let without: DaemonHandshake = serde_json::from_value(serde_json::json!({
            "protocol_version": SUPPORTED[0],
            "port": 40123,
            "auth_token": "token",
            "pid": 7,
            "supported_versions": SUPPORTED,
        }))
        .expect("a handshake without build_version decodes");
        assert_eq!(without.build_version, "");
        assert_eq!(
            action_on(without),
            (ExistingKind::OlderBuild, Action::RetireThenSpawn)
        );
        assert_eq!(
            action_on(of_build("not-a-version", SUPPORTED)),
            (ExistingKind::OlderBuild, Action::RetireThenSpawn),
            "an unparsable build counts as older"
        );
    }

    #[test]
    fn a_listener_answering_health_without_a_proof_is_not_a_daemon() {
        let scratch = ScratchDir::new("classify-health");
        let fake = FakeListener::healthy();
        let file = handshake_naming(4242, fake.port, "token", SUPPORTED);
        write_daemon_json(&scratch.0, &file);

        let existing = block_on(classify_existing_daemon_in(
            &scratch.0,
            None,
            client(),
            HANG_GUARD,
        ));

        assert_eq!(existing.kind(), ExistingKind::Missing);
        assert!(!fake.saw_authorization(), "heads: {:?}", fake.heads());
    }

    #[test]
    fn a_proven_listener_is_classified_from_the_file_it_proved() {
        let scratch = ScratchDir::new("classify-proven");
        let fake = FakeListener::proving("good-token");

        let compatible = handshake_naming(4242, fake.port, "good-token", SUPPORTED);
        write_daemon_json(&scratch.0, &compatible);
        let existing = block_on(classify_existing_daemon_in(
            &scratch.0,
            None,
            client(),
            HANG_GUARD,
        ));
        assert_eq!(existing.kind(), ExistingKind::Compatible);
        assert_eq!(
            existing.handshake().map(|h| h.port),
            Some(fake.port),
            "classified from another file"
        );

        let incompatible = handshake_naming(4242, fake.port, "good-token", &[0]);
        write_daemon_json(&scratch.0, &incompatible);
        let existing = block_on(classify_existing_daemon_in(
            &scratch.0,
            None,
            client(),
            HANG_GUARD,
        ));
        assert_eq!(existing.kind(), ExistingKind::Incompatible);
    }

    #[test]
    fn wait_for_handshake_skips_a_stale_file_whose_port_answers_health() {
        let scratch = ScratchDir::new("wait-stale");
        let (stale, polled) = FakeListener::healthy_signalling();
        let daemon = FakeListener::proving("good-token");
        write_daemon_json(
            &scratch.0,
            &handshake_naming(4242, stale.port, "stale-token", SUPPORTED),
        );

        let dir = scratch.0.clone();
        let waiter = std::thread::spawn(move || {
            block_on(wait_for_handshake_in(
                &dir, SUPPORTED, HANG_GUARD, HANG_GUARD,
            ))
        });
        polled
            .recv_timeout(HANG_GUARD)
            .expect("hung: the wait never challenged the stale file's port");
        let good = handshake_naming(4343, daemon.port, "good-token", SUPPORTED);
        let staged = scratch.0.join("daemon.json.tmp");
        std::fs::write(
            &staged,
            serde_json::to_vec(&good).expect("encode handshake"),
        )
        .expect("write the staged daemon.json");
        std::fs::rename(&staged, crate::handshake_file_in(&scratch.0))
            .expect("rename the good daemon.json in");

        let found = waiter
            .join()
            .expect("the waiting thread")
            .expect("the wait accepts the proven file");
        assert_eq!(found.port, daemon.port);
        assert_eq!(found.auth_token, "good-token");
    }

    /// Join the test process to a fresh job with `flags` and return the job.
    ///
    /// A process stays in a job for good, so each test that calls this needs a
    /// test process of its own: nextest runs every test in its own process
    /// (under `cargo test` they would share one, and the counts would mix).
    /// The test process may already sit in a job (the terminal's, the gate's);
    /// jobs nest on Windows 8 and later, so joining another one still works.
    #[cfg(windows)]
    fn join_new_job(flags: JOB_OBJECT_LIMIT) -> HANDLE {
        let job = new_job(flags);
        // SAFETY: `job` is a live job handle and GetCurrentProcess returns a pseudo handle.
        unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) }.expect(
            "join the test process to a new job; jobs nest only on Windows 8 and later, \
             so this test cannot prove anything without one",
        );
        job
    }

    /// A fresh job with the extended limit `flags`.
    #[cfg(windows)]
    fn new_job(flags: JOB_OBJECT_LIMIT) -> HANDLE {
        // SAFETY: no security attributes and no name; the handle lives as long as the test process.
        let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }.expect("create a job object");
        // The breakaway flags are extended limits, which the basic class refuses.
        let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: flags,
                ..Default::default()
            },
            ..Default::default()
        };
        let len = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
            .expect("the limit struct's size fits in u32");
        // SAFETY: `limits` is a live JOBOBJECT_EXTENDED_LIMIT_INFORMATION and `len` is its size.
        unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                len,
            )
        }
        .expect("set the job's limit flags");
        job
    }

    /// How many processes have ever been in `job`, the exited ones included.
    #[cfg(windows)]
    fn processes_ever_in(job: HANDLE) -> u32 {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        let len = u32::try_from(size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>())
            .expect("the accounting struct's size fits in u32");
        // SAFETY: `info` is a live JOBOBJECT_BASIC_ACCOUNTING_INFORMATION and `len` is its size.
        unsafe {
            QueryInformationJobObject(
                Some(job),
                JobObjectBasicAccountingInformation,
                (&raw mut info).cast(),
                len,
                None,
            )
        }
        .expect("read the job's accounting");
        info.TotalProcesses
    }

    /// `cmd.exe` stands in for the daemon: with a null stdin it reads EOF and exits.
    #[cfg(windows)]
    fn spawn_stand_in_daemon() -> anyhow::Result<()> {
        block_on(spawn_daemon(
            Path::new("cmd.exe"),
            Path::new("unused-templates"),
        ))
    }

    #[cfg(windows)]
    #[test]
    fn spawned_daemon_leaves_a_job_that_allows_breakaway() {
        let job = join_new_job(JOB_OBJECT_LIMIT_BREAKAWAY_OK);

        spawn_stand_in_daemon().expect("spawn the stand-in daemon");

        assert_eq!(
            processes_ever_in(job),
            1,
            "the daemon joined the caller's job instead of breaking away"
        );
    }

    #[cfg(windows)]
    #[test]
    fn spawned_daemon_escapes_a_job_that_forbids_breakaway() {
        let job = join_new_job(JOB_OBJECT_LIMIT(0));

        spawn_stand_in_daemon().expect("spawn the stand-in daemon outside the job");

        assert_eq!(
            processes_ever_in(job),
            1,
            "the daemon started inside the caller's job instead of escaping it"
        );
    }

    #[cfg(windows)]
    #[test]
    fn plain_spawn_when_the_wmi_launch_fails() {
        let job = join_new_job(JOB_OBJECT_LIMIT(0));

        block_on(spawn_daemon_with(
            Path::new("cmd.exe"),
            &[],
            Path::new("unused-templates"),
            |_, _, _| Err(anyhow::anyhow!("injected WMI launch failure")),
        ))
        .expect("spawn the stand-in daemon inside the job");

        assert_eq!(
            processes_ever_in(job),
            2,
            "the daemon should have started inside the caller's job"
        );
    }

    /// Set on the re-executed child of the kill-on-close test, to the path of
    /// the file it writes the stand-in daemon's pid to.
    #[cfg(windows)]
    const KILL_ON_CLOSE_CHILD_ENV: &str = "RT_TEST_KILL_ON_CLOSE_CHILD_PID_FILE";
    /// Only a hang guard, generous because a WMI launch is slow on a busy machine.
    #[cfg(windows)]
    const LAUNCH_HANG_GUARD: Duration = Duration::from_secs(120);

    /// The daemon must survive the job it was started from. Closing a job the
    /// test process sits in would kill the test, so the test binary re-runs
    /// itself as a child inside a kill-on-close job that forbids breakaway,
    /// the child starts the stand-in daemon, and the parent closes the job.
    #[cfg(windows)]
    #[test]
    fn daemon_survives_closing_a_kill_on_close_job_that_forbids_breakaway() {
        if let Some(pid_file) = std::env::var_os(KILL_ON_CLOSE_CHILD_ENV) {
            spawn_long_lived_stand_in(Path::new(&pid_file));
            return;
        }
        let scratch = ScratchDir::new("kill-on-close");
        let pid_file = scratch.0.join("stand-in.pid");
        let job = new_job(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE);
        let mut child = std::process::Command::new(
            std::env::current_exe().expect("the test binary's path"),
        )
        .args([
            "--exact",
            "supervisor::tests::daemon_survives_closing_a_kill_on_close_job_that_forbids_breakaway",
            "--nocapture",
        ])
        .env(KILL_ON_CLOSE_CHILD_ENV, &pid_file)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("re-run the test binary as the job's child");
        // SAFETY: `job` is live, and the child's handle lives as long as `child`.
        unsafe { AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())) }
            .expect("put the child in the kill-on-close job");
        child
            .stdin
            .take()
            .expect("the child's stdin")
            .write_all(b"go\n")
            .expect("tell the child to start the stand-in");

        let stand_in = KillOnDrop(wait_for_pid_file(&pid_file, &mut child));
        assert!(
            !is_in_job(stand_in.0, job),
            "the daemon started inside the caller's kill-on-close job"
        );
        // SAFETY: `job` is our only handle to the job, closed once, here.
        unsafe { CloseHandle(job) }.expect("close the kill-on-close job");
        child.wait().expect("wait for the job's child to end");

        assert!(
            is_running(stand_in.0),
            "the daemon died with the caller's job"
        );
    }

    /// The re-executed child: waits for the parent's go line (sent once the
    /// child is in the job), starts a long-lived stand-in daemon and writes its
    /// pid to `pid_file`.
    #[cfg(windows)]
    fn spawn_long_lived_stand_in(pid_file: &Path) {
        let mut go = String::new();
        std::io::stdin()
            .read_line(&mut go)
            .expect("read the parent's go line");
        if go.trim_end() != "go" {
            // EOF or anything else: the parent never put this child in its job.
            return;
        }
        let args: Vec<OsString> = ["-n", "600", "127.0.0.1"]
            .into_iter()
            .map(OsString::from)
            .collect();
        let pid = block_on(spawn_daemon_with(
            Path::new(r"C:\Windows\System32\PING.EXE"),
            &args,
            Path::new("unused-templates"),
            crate::wmi_launch::launch,
        ))
        .expect("spawn the long-lived stand-in daemon");
        let staged = pid_file.with_extension("tmp");
        std::fs::write(&staged, pid.to_string()).expect("write the stand-in's pid");
        std::fs::rename(&staged, pid_file).expect("move the stand-in's pid file in place");
    }

    /// The pid the child wrote to `pid_file`, failing as hung after
    /// [`LAUNCH_HANG_GUARD`] and at once when the child ends without writing it.
    #[cfg(windows)]
    fn wait_for_pid_file(pid_file: &Path, child: &mut std::process::Child) -> u32 {
        let deadline = Instant::now() + LAUNCH_HANG_GUARD;
        while !pid_file.exists() {
            if let Some(status) = child.try_wait().expect("poll the job's child") {
                if pid_file.exists() {
                    break;
                }
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    pipe.read_to_string(&mut stderr)
                        .expect("read the child's stderr");
                }
                assert!(
                    status.success(),
                    "the job's child failed ({status}): {stderr}"
                );
                assert!(
                    pid_file.exists(),
                    "the job's child ended without writing the pid: {stderr}"
                );
            }
            assert!(
                Instant::now() < deadline,
                "hung: the job's child wrote no stand-in pid"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        std::fs::read_to_string(pid_file)
            .expect("read the stand-in's pid")
            .trim()
            .parse()
            .expect("the pid file holds a pid")
    }

    /// Whether the process `pid` is in `job`.
    #[cfg(windows)]
    fn is_in_job(pid: u32, job: HANDLE) -> bool {
        // SAFETY: no pointer arguments; the handle is closed below.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .expect("open the stand-in daemon");
        let mut inside = BOOL::default();
        // SAFETY: both handles are live and `inside` is a live BOOL.
        let checked = unsafe { IsProcessInJob(process, Some(job), &raw mut inside) };
        // SAFETY: `process` was opened above and is closed once, here.
        unsafe { CloseHandle(process) }.expect("close the stand-in's handle");
        checked.expect("ask whether the stand-in is in the job");
        inside.as_bool()
    }

    /// Whether the process `pid` is still running.
    #[cfg(windows)]
    fn is_running(pid: u32) -> bool {
        // SAFETY: no pointer arguments; the handle is closed below.
        let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
        else {
            return false;
        };
        let mut code = 0;
        // SAFETY: `process` is live and `code` is a live u32.
        let read = unsafe { GetExitCodeProcess(process, &raw mut code) };
        // SAFETY: `process` was opened above and is closed once, here.
        unsafe { CloseHandle(process) }.expect("close the stand-in's handle");
        read.expect("read the stand-in's exit code");
        code == STILL_ACTIVE.0.cast_unsigned()
    }

    /// Terminates the process with this pid on drop, so a test leaves no
    /// stand-in daemon behind.
    #[cfg(windows)]
    struct KillOnDrop(u32);

    #[cfg(windows)]
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            // SAFETY: no pointer arguments; the returned handle is closed below.
            let handle = match unsafe { OpenProcess(PROCESS_TERMINATE, false, self.0) } {
                Ok(handle) => handle,
                Err(err) => {
                    warn!(
                        ?err,
                        pid = self.0,
                        "cannot open the stand-in daemon to end it"
                    );
                    return;
                }
            };
            // SAFETY: `handle` was opened above with PROCESS_TERMINATE.
            if let Err(err) = unsafe { TerminateProcess(handle, 1) } {
                warn!(?err, pid = self.0, "cannot end the stand-in daemon");
            }
            // SAFETY: `handle` was opened above and is closed once, here.
            if let Err(err) = unsafe { CloseHandle(handle) } {
                warn!(
                    ?err,
                    pid = self.0,
                    "cannot close the stand-in daemon's handle"
                );
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn retire_never_kills_a_pid_no_proof_vouched_for() {
        let scratch = ScratchDir::new("retire-unproven");
        let fake = FakeListener::healthy();
        let mut child = stand_in();
        let file = handshake_naming(child.id(), fake.port, "token", &[0]);
        write_daemon_json(&scratch.0, &file);

        let retired = block_on(retire_daemon_in(&scratch.0, &file, HANG_GUARD, HANG_GUARD));
        let alive = child.try_wait().expect("poll the stand-in").is_none();
        let _ = child.kill();
        let _ = child.wait();

        let err = retired.expect_err("an unproven listener is not reported as retired");
        assert_eq!(format!("{err:#}"), stopped_proving(fake.port));
        assert!(alive, "the stand-in was killed");
        assert!(!fake.saw_authorization(), "heads: {:?}", fake.heads());
    }

    fn stopped_proving(port: u16) -> String {
        format!(
            "the daemon on 127.0.0.1:{port} stopped proving it holds its token while being \
             retired; not replacing it"
        )
    }

    #[test]
    fn retire_does_not_report_success_when_the_daemon_stops_proving() {
        let scratch = ScratchDir::new("retire-silent");
        let fake = FakeListener::silent();
        let mut child = stand_in();
        let file = handshake_naming(child.id(), fake.port, "token", &[0]);
        write_daemon_json(&scratch.0, &file);

        // The silent listener never answers, so a short proof budget only
        // decides how soon the retire gives up waiting for nothing.
        let retired = block_on(retire_daemon_in(
            &scratch.0,
            &file,
            Duration::from_millis(200),
            HANG_GUARD,
        ));
        let alive = child.try_wait().expect("poll the stand-in").is_none();
        let _ = child.kill();
        let _ = child.wait();

        let err = retired.expect_err("a daemon that stopped proving is not reported as retired");
        assert_eq!(format!("{err:#}"), stopped_proving(fake.port));
        assert!(alive, "the stand-in was killed");
        assert!(!fake.saw_authorization(), "heads: {:?}", fake.heads());
    }

    #[cfg(windows)]
    #[test]
    fn retire_shuts_down_a_proven_daemon_through_its_pinned_pid() {
        let scratch = ScratchDir::new("retire-proven");
        let fake = FakeListener::proving("good-token");
        let mut child = stand_in();
        let file = handshake_naming(child.id(), fake.port, "good-token", &[0]);
        write_daemon_json(&scratch.0, &file);

        // The stand-in ignores `/shutdown`, so the graceful wait always runs
        // out and the pin's terminate is what ends it.
        let retired = block_on(retire_daemon_in(
            &scratch.0,
            &file,
            HANG_GUARD,
            Duration::from_millis(300),
        ));
        if retired.is_err() {
            let _ = child.kill();
        }
        let status = child.wait().expect("wait for the stand-in");

        retired.expect("retiring a proven daemon succeeds");
        assert!(
            fake.heads()
                .iter()
                .any(|head| head.starts_with("POST /shutdown ")
                    && head.contains("\r\nAuthorization: Bearer good-token\r\n")),
            "no bearer shutdown reached the proven daemon: {:?}",
            fake.heads()
        );
        assert!(
            !status.success(),
            "the stand-in exited on its own: {status}"
        );
        assert!(
            !crate::handshake_file_in(&scratch.0).exists(),
            "daemon.json survived"
        );
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
