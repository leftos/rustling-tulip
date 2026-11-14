//! Daemon supervision shared by the rustling-tulip clients.
//!
//! A client calls [`ensure_running`] to get the handshake of a healthy daemon
//! it can speak to: it reuses a running `rustling-tulipd` whose protocol it
//! speaks, retires one whose protocol it does not, or spawns a fresh one from
//! the content-addressed binary cache. The crate also resolves the per-user files
//! every client shares with the daemon (the config dir, `daemon.json`, the
//! client-identity file) and force-stops the daemon ([`stop`]).
//!
//! Paths are resolved the same way `crates/daemon/src/paths.rs` resolves them,
//! so a client and the daemon it spawns agree on where each file lives.

pub mod autostart;
pub mod capped_log;
pub mod device_tokens;
pub mod isolation;
mod listener_owner;
mod logon_task;
#[cfg(windows)]
mod pinned_process;
pub mod profile;
mod proof;
pub mod protected_root;
#[cfg(windows)]
pub mod spawn_detached;
mod supervisor;
#[cfg(test)]
mod test_fakes;
#[cfg(windows)]
mod wmi_launch;

pub use proof::{Proof, ProvenConnectError, connect_proven};
pub use supervisor::{
    DaemonIsElevated, DaemonIsNewer, DaemonIsOrdinary, ELEVATED_DAEMON_REFUSAL,
    OrdinaryDaemonIncompatible, PROOF_TIMEOUT, SPAWN_WAIT_TIMEOUT, ensure_running,
    ensure_running_for_bridge,
};

use anyhow::{Context as _, anyhow, bail};
use profile::Profile;
use protocol::DaemonHandshake;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Resolve the per-user config directory.
///
/// Mirrors `daemon::paths::Dirs::ensure`'s resolution in
/// `crates/daemon/src/paths.rs`: honors `RUSTLING_TULIP_CONFIG_DIR` if set
/// (used by the e2e harness to isolate test runs to a tmpdir), otherwise falls
/// back to `ProjectDirs::from("dev", "leftos", "rustling-tulip").config_dir()`.
///
/// # Errors
///
/// Fails when no home directory can be resolved for the current user.
pub fn config_dir() -> anyhow::Result<PathBuf> {
    if let Ok(value) = std::env::var("RUSTLING_TULIP_CONFIG_DIR")
        && !value.is_empty()
    {
        return Ok(PathBuf::from(value));
    }
    let pd = directories::ProjectDirs::from("dev", "leftos", "rustling-tulip")
        .ok_or_else(|| anyhow!("could not resolve config directory"))?;
    Ok(pd.config_dir().to_path_buf())
}

/// Resolve the folder files fetched from a remote host are saved under, one
/// subfolder per host.
///
/// Honors `RUSTLING_TULIP_REMOTE_FILES_DIR` if set and non-empty (used to
/// isolate test runs), otherwise falls back to
/// `ProjectDirs::from("dev", "leftos", "rustling-tulip").data_local_dir()`
/// joined with `remote-files`: machine-local, so the copies never roam.
///
/// # Errors
///
/// Fails when no home directory can be resolved for the current user.
pub fn remote_files_dir() -> anyhow::Result<PathBuf> {
    if let Ok(value) = std::env::var("RUSTLING_TULIP_REMOTE_FILES_DIR")
        && !value.is_empty()
    {
        return Ok(PathBuf::from(value));
    }
    let pd = directories::ProjectDirs::from("dev", "leftos", "rustling-tulip")
        .ok_or_else(|| anyhow!("could not resolve the local data directory"))?;
    Ok(pd.data_local_dir().join("remote-files"))
}

/// Resolve the folder the native client's updater downloads releases into.
///
/// Honors `RUSTLING_TULIP_UPDATES_DIR` if set and non-empty (used to isolate
/// test runs), otherwise falls back to
/// `ProjectDirs::from("dev", "leftos", "rustling-tulip").data_local_dir()`
/// joined with `updates`, machine-local like [`remote_files_dir`].
///
/// # Errors
///
/// Fails when no home directory can be resolved for the current user.
pub fn updates_dir() -> anyhow::Result<PathBuf> {
    if let Ok(value) = std::env::var("RUSTLING_TULIP_UPDATES_DIR")
        && !value.is_empty()
    {
        return Ok(PathBuf::from(value));
    }
    let pd = directories::ProjectDirs::from("dev", "leftos", "rustling-tulip")
        .ok_or_else(|| anyhow!("could not resolve the local data directory"))?;
    Ok(pd.data_local_dir().join("updates"))
}

/// Path the daemon this caller may use writes its handshake (`daemon.json`)
/// to.
///
/// The file is the one [`locate_daemon`] names: the running daemon's profile's
/// config dir, or this caller's own profile when none runs.
///
/// # Errors
///
/// Fails when [`locate_daemon`] does.
pub fn handshake_file() -> anyhow::Result<PathBuf> {
    Ok(handshake_file_in(&locate_daemon()?.config_dir))
}

/// Path of the handshake file (`daemon.json`) under `config_dir`.
#[must_use]
pub fn handshake_file_in(config_dir: &Path) -> PathBuf {
    config_dir.join("daemon.json")
}

/// The daemon a caller may use: its profile, and the config dir its
/// `daemon.json` lives in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedDaemon {
    /// The profile the daemon runs in.
    pub profile: Profile,
    /// The config dir its `daemon.json` lives in, or would.
    pub config_dir: PathBuf,
}

/// Find the daemon this caller may use, by its own integrity level.
///
/// The caller's own profile and the folder overrides give the profiles to
/// probe, in order ([`profile::roots_to_probe`]: an elevated caller looks for
/// an ordinary daemon of its account first, since reusing one gives no
/// elevation). The first profile whose `daemon.json` exists and whose daemon is
/// alive wins; a file that is there but unreadable still names its profile, so a
/// labelled handshake an elevated daemon left behind is never skipped. A file
/// whose daemon is gone is a crashed daemon's leftover and is passed over, so an
/// elevated caller reaches a running elevated daemon instead of a dead ordinary
/// one. With no live daemon in any of them, the caller's own profile is
/// returned: the folder a daemon it starts would write to.
///
/// Nothing is created: no folder, no daemon. Reading a handshake and trying the
/// instance lock is all this does, and a stale labelled one is cleared the way
/// [`read_handshake_file`] clears it.
///
/// # Errors
///
/// Fails when this process's profile or the app's folders cannot be resolved.
pub fn locate_daemon() -> anyhow::Result<LocatedDaemon> {
    let caller = profile::own_profile()?;
    let overrides = profile::Overrides::from_env()?;
    let candidates: Vec<LocatedDaemon> = profile::roots_to_probe(caller, &overrides)?
        .into_iter()
        .map(|(profile, roots)| LocatedDaemon {
            profile,
            config_dir: roots.config,
        })
        .collect();
    locate_in(&candidates, daemon_named_in, daemon_alive_in)
        .ok_or_else(|| anyhow!("no profile to look for a daemon in"))
}

/// The first of `candidates` before the last whose config dir `exists` names a
/// `daemon.json` and whose instance lock `alive` finds held; else the last
/// candidate itself.
///
/// `candidates` come in probe order, the caller's own profile last
/// ([`profile::roots_to_probe`]); `exists` and `alive` are given each config dir
/// in turn. A handshake whose daemon is gone is a crashed daemon's leftover and
/// is passed over. The last candidate is the caller's own profile, the folder a
/// daemon it starts would write to, and is returned whether or not one runs
/// there.
fn locate_in(
    candidates: &[LocatedDaemon],
    exists: impl Fn(&Path) -> bool,
    alive: impl Fn(&Path) -> bool,
) -> Option<LocatedDaemon> {
    let (own, earlier) = candidates.split_last()?;
    earlier
        .iter()
        .find(|daemon| exists(&daemon.config_dir) && alive(&daemon.config_dir))
        .cloned()
        .or_else(|| Some(own.clone()))
}

/// Whether `config_dir` holds a `daemon.json`: any [`read_handshake_file`]
/// outcome but a missing file counts, so a handshake this process may not read
/// still names the profile its daemon runs in.
fn daemon_named_in(config_dir: &Path) -> bool {
    !matches!(
        read_handshake_file(&handshake_file_in(config_dir)),
        Ok(HandshakeFile::Missing(_))
    )
}

/// Whether a daemon holds `config_dir`'s instance lock: a `daemon.json` whose
/// lock is free is a crashed daemon's leftover, not a daemon to talk to. A lock
/// file that is missing, or cannot be opened or checked, counts as no live
/// daemon.
fn daemon_alive_in(config_dir: &Path) -> bool {
    matches!(
        instance_lock(&handshake_file_in(config_dir)),
        LockState::Held
    )
}

/// Read and parse the `daemon.json` of the daemon this caller may use, without
/// checking that the daemon it names is alive.
///
/// # Errors
///
/// Fails with a [`DaemonIsElevated`] when [`read_handshake_file`] does, when
/// [`locate_daemon`] fails, when the file cannot be read otherwise
/// (`read <path>: …`), or when it is not a valid handshake
/// (`parse handshake: …`).
pub fn read_handshake() -> anyhow::Result<DaemonHandshake> {
    read_handshake_in(&locate_daemon()?.config_dir)
}

/// [`read_handshake`] for the daemon whose config dir is `config_dir`.
///
/// # Errors
///
/// As [`read_handshake`].
pub fn read_handshake_in(config_dir: &Path) -> anyhow::Result<DaemonHandshake> {
    let path = handshake_file_in(config_dir);
    let bytes = match read_handshake_file(&path)? {
        HandshakeFile::Bytes(bytes) => bytes,
        HandshakeFile::Missing(err) | HandshakeFile::Failed(err) => {
            return Err(anyhow::Error::new(err).context(format!("read {}", path.display())));
        }
    };
    serde_json::from_slice(&bytes).context("parse handshake")
}

/// Name of the daemon's single-instance lock file inside the config dir. A
/// running daemon holds an exclusive lock on it for its whole life.
pub const INSTANCE_LOCK_FILE: &str = "daemon.lock";

/// How many times a read of `daemon.json` is attempted while it is denied,
/// before the denial is taken for a High integrity label. Windows also denies
/// a file that is delete-pending or being replaced, for a moment; these match
/// the attempts and spacing the daemon's atomic replace retries the same
/// refusal with (`MAX_ATTEMPTS` in `crates/daemon/src/atomic_file.rs`).
pub const HANDSHAKE_READ_ATTEMPTS: u32 = 5;

/// The pause between two denied reads of `daemon.json`, matching the daemon's
/// atomic replace (`RETRY_DELAY` in `crates/daemon/src/atomic_file.rs`).
pub const HANDSHAKE_READ_RETRY_DELAY: Duration = Duration::from_millis(20);

/// What reading `daemon.json` found, once a denied read has been classified.
#[derive(Debug)]
pub enum HandshakeFile {
    /// The file's bytes.
    Bytes(Vec<u8>),
    /// There is no file: none was there, or the stale one an elevated daemon
    /// left behind was removed. The error says which.
    Missing(std::io::Error),
    /// The read failed for another reason than a denial or a missing file.
    Failed(std::io::Error),
}

/// What the check for a live daemon behind a denied `daemon.json` found.
#[derive(Debug)]
enum StaleCheck {
    /// A daemon holds the instance lock (or the lock could not be checked,
    /// which counts as live): the file is an elevated daemon's.
    DaemonLive,
    /// No daemon holds the lock, and the stale file was removed.
    Removed,
    /// No daemon holds the lock, but removing the stale file failed.
    NotRemoved(std::io::Error),
}

/// Read the handshake file at `path`, the one place a client turns a denied
/// read into a decision. A denial is retried [`HANDSHAKE_READ_ATTEMPTS`]
/// times, [`HANDSHAKE_READ_RETRY_DELAY`] apart, since a file being replaced is
/// denied for a moment too. A denial that persists is a High-labelled file an
/// elevated daemon wrote: when a daemon holds the config dir's instance lock it
/// is that daemon, and the read is refused; when none does, the file is stale
/// and is removed (while holding the lock, so no daemon starts meanwhile), and
/// the read reports it missing.
///
/// # Errors
///
/// [`DaemonIsElevated::Running`] when an elevated daemon runs, and
/// [`DaemonIsElevated::StaleHandshake`] when none runs but its file could not
/// be removed.
pub fn read_handshake_file(path: &Path) -> Result<HandshakeFile, DaemonIsElevated> {
    classify_handshake_read(
        path,
        || std::fs::read(path),
        std::thread::sleep,
        || clear_if_no_daemon(path),
    )
}

/// [`read_handshake_file`] with its seams: `read` reads the file, `pause`
/// waits between denied reads, and `clear_stale` checks for a live daemon and
/// removes the file when there is none.
fn classify_handshake_read(
    path: &Path,
    mut read: impl FnMut() -> std::io::Result<Vec<u8>>,
    mut pause: impl FnMut(Duration),
    clear_stale: impl FnOnce() -> StaleCheck,
) -> Result<HandshakeFile, DaemonIsElevated> {
    let mut attempt = 1;
    loop {
        match read() {
            Ok(bytes) => return Ok(HandshakeFile::Bytes(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(HandshakeFile::Missing(err));
            }
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                if attempt >= HANDSHAKE_READ_ATTEMPTS {
                    break;
                }
                attempt += 1;
                pause(HANDSHAKE_READ_RETRY_DELAY);
            }
            Err(err) => return Ok(HandshakeFile::Failed(err)),
        }
    }
    match clear_stale() {
        StaleCheck::DaemonLive => Err(DaemonIsElevated::Running),
        StaleCheck::Removed => {
            tracing::warn!(
                path = %path.display(),
                "removed the daemon.json an elevated daemon left behind; no daemon holds the instance lock"
            );
            Ok(HandshakeFile::Missing(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "removed the stale handshake an elevated daemon left behind",
            )))
        }
        StaleCheck::NotRemoved(err) => {
            tracing::warn!(
                path = %path.display(),
                %err,
                "no daemon holds the instance lock, but the daemon.json an elevated daemon left behind could not be removed"
            );
            Err(DaemonIsElevated::StaleHandshake(path.to_owned()))
        }
    }
}

/// What the instance lock beside a `daemon.json` says.
enum LockState {
    /// A daemon holds it, or it could not be opened or checked, which counts as
    /// held: nothing is removed or passed over on a guess.
    Held,
    /// No daemon holds it. The open lock file is carried when there was one, so
    /// a caller can act on the handshake while holding it.
    Free(Option<std::fs::File>),
}

/// Open `handshake`'s sibling instance lock without creating it and try to take
/// it: held means a daemon is alive. A lock that cannot be opened or checked
/// counts as held.
fn instance_lock(handshake: &Path) -> LockState {
    let lock_path = handshake.with_file_name(INSTANCE_LOCK_FILE);
    let lock = match std::fs::File::open(&lock_path) {
        Ok(file) => Some(file),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            tracing::warn!(lock = %lock_path.display(), %err, "could not open the instance lock; counting the daemon as live");
            return LockState::Held;
        }
    };
    if let Some(file) = &lock {
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return LockState::Held,
            Err(std::fs::TryLockError::Error(err)) => {
                tracing::warn!(lock = %lock_path.display(), %err, "could not check the instance lock; counting the daemon as live");
                return LockState::Held;
            }
        }
    }
    LockState::Free(lock)
}

/// Remove the handshake file at `handshake` when no daemon holds the instance
/// lock beside it, holding that lock during the removal. A lock that cannot
/// be opened or checked counts as a live daemon, so nothing is removed on a
/// guess.
fn clear_if_no_daemon(handshake: &Path) -> StaleCheck {
    let lock = match instance_lock(handshake) {
        LockState::Held => return StaleCheck::DaemonLive,
        LockState::Free(lock) => lock,
    };
    let removed = match std::fs::remove_file(handshake) {
        Ok(()) => StaleCheck::Removed,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => StaleCheck::Removed,
        Err(err) => StaleCheck::NotRemoved(err),
    };
    drop(lock);
    removed
}

/// Stable per-install client identity for per-client tab layouts.
///
/// `client_id` is generated once and persisted in the config dir so a window
/// and its pop-outs (same install) share one layout; `client_name` is the
/// machine hostname, shown when another client offers to clone this layout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientIdentity {
    pub client_id: String,
    pub client_name: Option<String>,
}

/// Load the client identity, creating its id file on first use.
///
/// `id_file_name` names the file under [`config_dir`] that holds the id, so
/// each kind of client keeps its own identity. The file holds a bare UUID v4
/// with no trailing newline; it is trimmed on read, and a missing or empty file
/// gets a freshly generated id.
///
/// # Errors
///
/// Fails when the config dir cannot be resolved or created, or the id file
/// cannot be written.
pub fn client_identity(id_file_name: &str) -> anyhow::Result<ClientIdentity> {
    client_identity_in(&config_dir()?, id_file_name)
}

/// [`client_identity`] with its id file under `dir` instead of the config
/// dir.
///
/// # Errors
///
/// Fails when `dir` cannot be created or the id file cannot be written.
pub fn client_identity_in(dir: &Path, id_file_name: &str) -> anyhow::Result<ClientIdentity> {
    let path = dir.join(id_file_name);
    let client_id = match std::fs::read_to_string(&path) {
        Ok(existing) if !existing.trim().is_empty() => existing.trim().to_string(),
        _ => {
            let id = uuid::Uuid::new_v4().to_string();
            std::fs::create_dir_all(dir).context("create config dir")?;
            std::fs::write(&path, &id).with_context(|| format!("write {id_file_name}"))?;
            id
        }
    };
    Ok(ClientIdentity {
        client_id,
        client_name: sysinfo::System::host_name(),
    })
}

/// How long [`stop_in`] waits for a terminated daemon to exit.
#[cfg(windows)]
const STOP_WAIT: Duration = Duration::from_secs(5);

/// Whether a caller of profile `caller` may stop a daemon whose process runs as
/// `daemon`, or whose integrity could not be read (`Err`): a caller stops a
/// daemon of its own integrity only, since supervision never crosses integrity.
///
/// The daemon's integrity is its process's, not its folder's: an elevated
/// daemon an older release left in the ordinary profile is the caller's own
/// integrity and may be stopped. An integrity that cannot be read is not
/// proven to be the caller's own, so it counts as ordinary and an elevated
/// caller is refused it, as it is an ordinary daemon, and sent to an ordinary
/// terminal instead. An ordinary caller never reaches an elevated daemon's
/// handshake at all ([`DaemonIsElevated`]).
///
/// # Errors
///
/// [`DaemonIsOrdinary`] when an elevated caller would stop an ordinary daemon
/// or one whose integrity cannot be read.
pub fn may_stop(caller: Profile, daemon: anyhow::Result<Profile>) -> Result<(), DaemonIsOrdinary> {
    let daemon = daemon.unwrap_or_else(|err| {
        tracing::warn!(
            err = %format!("{err:#}"),
            "could not read the daemon's integrity; treating it as ordinary"
        );
        Profile::Ordinary
    });
    if caller == Profile::Elevated && daemon == Profile::Ordinary {
        return Err(DaemonIsOrdinary);
    }
    Ok(())
}

/// Refuse the stop of `located`'s daemon when this caller may not stop it
/// ([`may_stop`]), judged by the integrity of the process its handshake names.
///
/// A missing or unparseable handshake is `Ok`: there is nothing to stop, and
/// the stop reports that on its own no-daemon path. So is a handshake naming a
/// pid no process has: nothing runs to stop across integrity, and the stop's
/// own path removes the stale file.
///
/// # Errors
///
/// [`DaemonIsOrdinary`] as [`may_stop`] returns it.
pub fn refuse_stop_across_integrity(
    caller: Profile,
    located: &LocatedDaemon,
) -> Result<(), DaemonIsOrdinary> {
    let Ok(handshake) = read_handshake_in(&located.config_dir) else {
        return Ok(());
    };
    match profile::profile_of_process(handshake.pid) {
        Ok(None) => Ok(()),
        Ok(Some(daemon)) => may_stop(caller, Ok(daemon)),
        Err(err) => may_stop(caller, Err(err)),
    }
}

/// Force-stop the daemon this caller may use, if [`may_stop`] allows it: read
/// `daemon.json`, have the listener on its port prove it holds its token, kill
/// the process its pid names and remove the handshake file.
///
/// On Windows the pid is pinned by a process handle before the proof and
/// killed through that handle, so a pid reused by another process is never
/// killed, and only when that pid owns the proven listener on the
/// handshake's port (a failed read of the listener table counts as not
/// owned). Kills by pid rather than sending a `Shutdown` WS message because
/// the WS may already be closed, and a pid kill works in both states. The
/// daemon's drop guard would normally remove `daemon.json` on graceful exit;
/// it is removed here too so a later [`ensure_running`] doesn't mistake a
/// stale handshake for a live daemon.
///
/// # Errors
///
/// Fails with a [`DaemonIsOrdinary`] when an elevated caller locates a daemon
/// whose process runs ordinary or, while running, has an integrity that cannot
/// be read ([`refuse_stop_across_integrity`]), or when [`locate_daemon`] does;
/// fails when there is no handshake on disk or it cannot be read or parsed; on
/// Windows only, when its pid is not running (the stale file is removed),
/// this client may not terminate it, or it does not own the proven listener
/// (in both, nothing is stopped and the file is kept); when its listener
/// does not prove it is the daemon (nothing is killed; the file is removed,
/// except when the listener did not answer at all); or when the kill fails.
/// The file is only ever removed while it still holds the token that was read.
pub async fn stop() -> anyhow::Result<()> {
    let located = locate_daemon()?;
    refuse_stop_across_integrity(profile::own_profile()?, &located)?;
    stop_in(&located.config_dir).await
}

/// [`stop`] for the daemon whose config dir is `config_dir`.
///
/// # Errors
///
/// As [`stop`].
pub async fn stop_in(config_dir: &Path) -> anyhow::Result<()> {
    stop_in_with(
        config_dir,
        supervisor::PROOF_TIMEOUT,
        &listener_owner::listener_owner,
    )
    .await
}

/// [`stop_in`] with `proof_budget` for the listener's proof and `owner` to
/// read which pid owns the proven listener.
///
/// # Errors
///
/// As [`stop`].
pub(crate) async fn stop_in_with(
    config_dir: &Path,
    proof_budget: Duration,
    owner: &listener_owner::OwnerLookup,
) -> anyhow::Result<()> {
    let path = handshake_file_in(config_dir);
    if !path.exists() {
        bail!("no daemon handshake on disk — daemon may not be running");
    }
    let parsed = read_handshake_in(config_dir)?;
    #[cfg(windows)]
    let pinned = pin_for_stop(config_dir, &parsed, &path).await?;
    let proof = proof::prove(parsed.port, &parsed.auth_token, proof_budget).await;
    refuse_unproven(proof, config_dir, &parsed, &path).await?;
    // The lookup may block up to 3 s (the lsof deadline); stop and retire are
    // rare and already waiting on the daemon.
    if !supervisor::owns_listener(owner, &parsed) {
        bail!(
            "pid {} named by {} does not own the daemon's listener on 127.0.0.1:{}; nothing was stopped",
            parsed.pid,
            path.display(),
            parsed.port
        );
    }
    #[cfg(windows)]
    {
        pinned.terminate()?;
        if !pinned.wait(STOP_WAIT).await {
            bail!("pid {} did not exit after it was terminated", parsed.pid);
        }
    }
    #[cfg(not(windows))]
    kill_pid(parsed.pid).await?;
    remove_handshake_holding_token(config_dir, &parsed.auth_token).await;
    Ok(())
}

/// Pins the pid `parsed` names for [`stop_in`].
///
/// # Errors
///
/// Fails when the pid is not running (removing the handshake while it still
/// holds `parsed`'s token) or this client may not terminate it (nothing is
/// stopped and the handshake is kept).
#[cfg(windows)]
async fn pin_for_stop(
    config_dir: &Path,
    parsed: &DaemonHandshake,
    path: &Path,
) -> anyhow::Result<pinned_process::PinnedProcess> {
    match pinned_process::PinnedProcess::open(parsed.pid) {
        Ok(pinned) => Ok(pinned),
        Err(pinned_process::OpenFailure::NotRunning) => {
            remove_handshake_holding_token(config_dir, &parsed.auth_token).await;
            bail!(
                "no daemon is running: pid {} named by {} is not running; removed the stale handshake",
                parsed.pid,
                path.display()
            )
        }
        Err(pinned_process::OpenFailure::NoRights(err)) => {
            tracing::info!(pid = parsed.pid, %err, "this client may not terminate the daemon");
            bail!(
                "pid {} named by {} is running but this client may not stop it; nothing was stopped",
                parsed.pid,
                path.display()
            )
        }
    }
}

/// The error [`stop_in`] returns for a listener that did not prove it is the
/// daemon `parsed` names, removing the handshake at `path` (while it still
/// holds `parsed`'s token) when the listener answered or nothing listens;
/// `Ok` when it proved itself.
async fn refuse_unproven(
    proof: proof::Proof,
    config_dir: &Path,
    parsed: &DaemonHandshake,
    path: &Path,
) -> anyhow::Result<()> {
    match proof {
        proof::Proof::Proven => Ok(()),
        proof::Proof::Unproven | proof::Proof::Refused => {
            remove_handshake_holding_token(config_dir, &parsed.auth_token).await;
            bail!(
                "the listener on 127.0.0.1:{} named by {} did not prove it is the daemon; \
                 nothing was stopped and the stale handshake was removed",
                parsed.port,
                path.display()
            )
        }
        proof::Proof::NoResponse => bail!(
            "the daemon on 127.0.0.1:{} did not answer its proof; nothing was stopped",
            parsed.port
        ),
    }
}

/// Removes the handshake at `path`. Best effort: a file already gone (the
/// daemon's drop guard beat us to it) is fine, and any other failure is
/// logged, since the next [`ensure_running`] proves the file anyway.
async fn remove_handshake(path: &Path) {
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "could not remove the daemon handshake");
        }
    }
}

/// Removes `config_dir`'s `daemon.json` only while it still holds `token`, so
/// a handshake a newer daemon wrote in the meantime survives.
pub(crate) async fn remove_handshake_holding_token(config_dir: &Path, token: &str) {
    let path = handshake_file_in(config_dir);
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return;
    };
    let holds_token = serde_json::from_slice::<DaemonHandshake>(&bytes)
        .is_ok_and(|current| current.auth_token == token);
    if holds_token {
        remove_handshake(&path).await;
    }
}

/// Force-kill a process by pid with `taskkill /F`.
///
/// # Errors
///
/// Fails when the kill command cannot be run or exits unsuccessfully.
#[cfg(windows)]
pub(crate) async fn kill_pid(pid: u32) -> anyhow::Result<()> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let status = tokio::process::Command::new("taskkill")
        .arg("/PID")
        .arg(pid.to_string())
        .arg("/F")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .context("taskkill")?;
    if !status.success() {
        bail!("taskkill exited with {status}");
    }
    Ok(())
}

/// Ask a process to terminate by sending it `SIGTERM` (`kill -TERM`). The
/// process may handle the signal and exit on its own schedule; this is not a
/// force kill.
///
/// # Errors
///
/// Fails when the kill command cannot be run or exits unsuccessfully.
#[cfg(not(windows))]
pub(crate) async fn kill_pid(pid: u32) -> anyhow::Result<()> {
    let status = tokio::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .context("kill")?;
    if !status.success() {
        bail!("kill exited with {status}");
    }
    Ok(())
}

/// What [`shutdown_in`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shutdown {
    /// No `daemon.json`: no daemon to stop.
    NoDaemon,
    /// The daemon stopped.
    Stopped,
    /// `daemon.json` named a pid that is no longer running; only the stale file
    /// was removed.
    Stale,
    /// Its listener did not prove it holds the token: nothing was sent, nothing
    /// killed, the file kept.
    NotProven(Proof),
}

/// Retire the daemon `config_dir`'s `daemon.json` names, without spawning a
/// replacement: on Windows its pid is pinned, the listener proves it holds the
/// token, the bearer `/shutdown` goes out on that proven connection, and a
/// daemon still running after the wait is force-stopped through the pin.
///
/// Every `rt-tracer.exe` is left alive, so the next daemon reattaches the
/// user's sessions.
///
/// # Errors
///
/// Fails when `daemon.json` holds something that is not a handshake, or when
/// this client may not terminate the daemon and it is still running after the
/// shutdown wait. A missing file, a pid that already exited and a listener that
/// does not prove itself are not errors: they are returned as
/// [`Shutdown::NoDaemon`], [`Shutdown::Stale`] and [`Shutdown::NotProven`].
pub async fn shutdown_in(config_dir: &Path) -> anyhow::Result<Shutdown> {
    shutdown_in_with(
        config_dir,
        supervisor::PROOF_TIMEOUT,
        supervisor::SHUTDOWN_WAIT_TIMEOUT,
    )
    .await
}

/// [`shutdown_in`] with `proof_budget` for the listener's proof and
/// `shutdown_wait` for the daemon to stop after the shutdown request.
///
/// # Errors
///
/// As [`shutdown_in`].
pub(crate) async fn shutdown_in_with(
    config_dir: &Path,
    proof_budget: Duration,
    shutdown_wait: Duration,
) -> anyhow::Result<Shutdown> {
    let handshake = match read_handshake_in(config_dir) {
        Ok(handshake) => handshake,
        Err(err) if names_no_file(&err) => return Ok(Shutdown::NoDaemon),
        Err(err) => return Err(err),
    };
    // The pid is opened before the retire so a daemon that already exited is
    // reported as the stale file it left behind, not as a stopped daemon.
    #[cfg(windows)]
    {
        if matches!(
            pinned_process::PinnedProcess::open(handshake.pid),
            Err(pinned_process::OpenFailure::NotRunning)
        ) {
            remove_handshake_holding_token(config_dir, &handshake.auth_token).await;
            return Ok(Shutdown::Stale);
        }
    }
    match supervisor::retire_daemon_in(
        config_dir,
        &handshake,
        proof_budget,
        shutdown_wait,
        &listener_owner::listener_owner,
    )
    .await
    {
        Ok(()) => Ok(Shutdown::Stopped),
        Err(err) => match err.downcast_ref::<supervisor::NotProven>() {
            Some(refusal) => Ok(Shutdown::NotProven(refusal.proof)),
            None => Err(err),
        },
    }
}

/// Whether `err` is a read that found no `daemon.json`: the daemon went away
/// before the read, so there is nothing to stop.
fn names_no_file(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|io| io.kind() == std::io::ErrorKind::NotFound)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    #[cfg(windows)]
    use super::Proof;
    use super::{
        DaemonIsElevated, DaemonIsOrdinary, HANDSHAKE_READ_ATTEMPTS, HANDSHAKE_READ_RETRY_DELAY,
        HandshakeFile, INSTANCE_LOCK_FILE, LocatedDaemon, Shutdown, StaleCheck,
        classify_handshake_read, clear_if_no_daemon, client_identity, handshake_file_in, locate_in,
        may_stop,
    };
    use crate::profile::Profile;
    use crate::test_fakes::{
        FakeListener, ScratchDir, block_on, handshake_naming, stand_in, write_daemon_json,
    };
    use anyhow::anyhow;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, PoisonError};
    use std::time::Duration;
    use uuid::Uuid;

    const ID_FILE: &str = "client-id-test";
    const CONFIG_DIR_VAR: &str = "RUSTLING_TULIP_CONFIG_DIR";
    /// Only a hang guard: no test asserts how long anything took.
    const HANG_GUARD: Duration = Duration::from_secs(10);

    /// Serialises the tests that point `RUSTLING_TULIP_CONFIG_DIR` somewhere:
    /// env vars are process-global and tests run on parallel threads.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Config dir under `<workspace>/.tmp` that `RUSTLING_TULIP_CONFIG_DIR`
    /// points at while the guard lives. Drop restores the var's prior value
    /// (unsetting it only when it was unset) and removes the dir.
    struct ScratchConfigDir {
        path: PathBuf,
        prior: Option<OsString>,
    }

    impl ScratchConfigDir {
        fn new(label: &str) -> Self {
            let path = crate::supervisor::dev_workspace_root()
                .expect("workspace root resolves")
                .join(".tmp")
                .join(format!("daemon-client-{label}-{}", Uuid::new_v4().simple()));
            std::fs::create_dir_all(&path).expect("create scratch config dir");
            let prior = std::env::var_os(CONFIG_DIR_VAR);
            // SAFETY: callers hold ENV_LOCK for the guard's lifetime, and no
            // other test in this crate reads the environment.
            unsafe { std::env::set_var(CONFIG_DIR_VAR, &path) };
            Self { path, prior }
        }
    }

    impl Drop for ScratchConfigDir {
        fn drop(&mut self) {
            // SAFETY: as in `new` — ENV_LOCK is still held by the caller.
            unsafe {
                match &self.prior {
                    Some(value) => std::env::set_var(CONFIG_DIR_VAR, value),
                    None => std::env::remove_var(CONFIG_DIR_VAR),
                }
            }
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn is_uuid_v4(text: &str) -> bool {
        Uuid::parse_str(text).is_ok_and(|id| id.get_version_num() == 4)
    }

    /// The Apple config dir is `Library/Application Support/<bundle id>` under
    /// the home directory, which iOS points at the app's container: the phone
    /// app keeps its identity and settings there with no path of its own.
    #[test]
    #[cfg(target_os = "macos")]
    fn config_dir_is_under_application_support_on_apple() {
        /// Restores `RUSTLING_TULIP_CONFIG_DIR` however the test ends.
        struct Restore(Option<OsString>);

        impl Drop for Restore {
            fn drop(&mut self) {
                // SAFETY: the test holds ENV_LOCK until after this guard drops.
                unsafe {
                    match self.0.take() {
                        Some(value) => std::env::set_var(CONFIG_DIR_VAR, value),
                        None => std::env::remove_var(CONFIG_DIR_VAR),
                    }
                }
            }
        }

        let _lock = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let prior = std::env::var_os(CONFIG_DIR_VAR);
        // SAFETY: ENV_LOCK is held for the rest of the test, and no other test
        // in this crate reads the environment without it.
        unsafe { std::env::remove_var(CONFIG_DIR_VAR) };
        let _restore = Restore(prior);

        let dir = super::config_dir().expect("a config dir without the override");

        assert!(
            dir.ends_with("Library/Application Support/dev.leftos.rustling-tulip"),
            "not under Application Support: {}",
            dir.display()
        );
    }

    #[test]
    fn client_identity_creates_then_reuses_id_file() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let scratch = ScratchConfigDir::new("reuse");

        let first = client_identity(ID_FILE).expect("first call creates the id file");
        let second = client_identity(ID_FILE).expect("second call reads the id file");

        assert!(
            is_uuid_v4(&first.client_id),
            "not a UUID v4: {}",
            first.client_id
        );
        assert_eq!(first.client_id, second.client_id);
        let on_disk = std::fs::read_to_string(scratch.path.join(ID_FILE)).expect("id file exists");
        assert_eq!(
            on_disk, first.client_id,
            "id file must hold the bare id, no newline"
        );
    }

    #[test]
    fn client_identity_regenerates_when_file_empty() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let scratch = ScratchConfigDir::new("empty");
        let id_file = scratch.path.join(ID_FILE);
        std::fs::write(&id_file, "").expect("write empty id file");

        let identity = client_identity(ID_FILE).expect("empty id file is regenerated");

        assert!(
            is_uuid_v4(&identity.client_id),
            "not a UUID v4: {}",
            identity.client_id
        );
        let on_disk = std::fs::read_to_string(&id_file).expect("id file exists");
        assert_eq!(on_disk, identity.client_id);
    }

    fn write_handshake(dir: &std::path::Path, pid: u32, port: u16, token: &str) {
        write_daemon_json(dir, &handshake_naming(pid, port, token, &[]));
    }

    /// Whether `child` is still running, then kills and reaps it so a test
    /// leaves nothing behind.
    fn alive_then_reaped(child: &mut std::process::Child) -> bool {
        let alive = child.try_wait().expect("poll the stand-in").is_none();
        let _ = child.kill();
        let _ = child.wait();
        alive
    }

    #[test]
    fn read_handshake_in_reads_that_dirs_daemon_json() {
        let scratch = ScratchDir::new("handshake");
        write_handshake(&scratch.0, 99, 40123, "token");

        let handshake = super::read_handshake_in(&scratch.0).expect("handshake parses");

        assert_eq!(handshake.port, 40123);
        assert_eq!(handshake.pid, 99);
        assert_eq!(handshake.auth_token, "token");
    }

    #[test]
    fn read_handshake_in_names_the_missing_file() {
        let scratch = ScratchDir::new("no-handshake");

        let err = super::read_handshake_in(&scratch.0).expect_err("no daemon.json");

        let message = format!("{err:#}");
        assert!(message.contains("daemon.json"), "error was: {message}");
    }

    fn denied() -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    }

    /// A denied `daemon.json` with no daemon holding the instance lock is
    /// stale: it is removed and reads as missing, or, when it cannot be
    /// removed, named with how to delete it. A live daemon keeps the refusal.
    #[test]
    fn stale_labelled_handshake_with_no_live_daemon_is_cleared_or_named() {
        let path = PathBuf::from(r"C:\config\daemon.json");
        let classify =
            |check: StaleCheck| classify_handshake_read(&path, denied, |_| {}, move || check);

        assert_eq!(
            classify(StaleCheck::DaemonLive).expect_err("a live daemon is refused"),
            DaemonIsElevated::Running
        );
        let missing = classify(StaleCheck::Removed).expect("a removed stale file reads");
        assert!(
            matches!(&missing, HandshakeFile::Missing(err) if err.kind() == std::io::ErrorKind::NotFound),
            "{missing:?}"
        );
        let stuck = classify(StaleCheck::NotRemoved(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        )))
        .expect_err("a stale file that stays is named");
        assert_eq!(stuck, DaemonIsElevated::StaleHandshake(path.clone()));
        assert_eq!(
            stuck.to_string(),
            r"a rustling-tulip daemon that ran as administrator left C:\config\daemon.json behind; delete it from an elevated terminal, then start the app again"
        );

        // The real check: a held instance lock is a live daemon and the file
        // stays; once it is free, the file is removed.
        let scratch = ScratchDir::new("stale-labelled");
        let handshake = handshake_file_in(&scratch.0);
        std::fs::write(&handshake, b"{}").expect("write a stand-in daemon.json");
        let holder = std::fs::File::create(scratch.0.join(INSTANCE_LOCK_FILE))
            .expect("create the instance lock");
        holder.try_lock().expect("take the instance lock");
        assert!(matches!(
            clear_if_no_daemon(&handshake),
            StaleCheck::DaemonLive
        ));
        assert!(handshake.exists(), "a live daemon's file is never removed");
        drop(holder);
        assert!(matches!(
            clear_if_no_daemon(&handshake),
            StaleCheck::Removed
        ));
        assert!(!handshake.exists(), "a stale file is removed");
        assert!(
            scratch.0.join(INSTANCE_LOCK_FILE).exists(),
            "the lock file is left for the next daemon"
        );
    }

    /// A denial is retried as the daemon's atomic replace retries it, and
    /// only a denial that outlasts every attempt checks for a stale file.
    #[test]
    fn transient_access_denied_is_retried_before_classifying() {
        let path = PathBuf::from("daemon.json");
        let mut reads = 0;
        let mut pauses = Vec::new();
        let mut checked = false;
        let read = classify_handshake_read(
            &path,
            || {
                reads += 1;
                if reads < 3 {
                    denied()
                } else {
                    Ok(b"{}".to_vec())
                }
            },
            |pause| pauses.push(pause),
            || {
                checked = true;
                StaleCheck::DaemonLive
            },
        )
        .expect("the third read succeeds");
        assert!(matches!(read, HandshakeFile::Bytes(_)), "{read:?}");
        assert!(
            !checked,
            "a read that succeeds never checks for a stale file"
        );
        assert_eq!(reads, 3);
        assert_eq!(pauses, vec![HANDSHAKE_READ_RETRY_DELAY; 2]);

        let mut reads = 0;
        let mut pauses = 0;
        checked = false;
        let refused = classify_handshake_read(
            &path,
            || {
                reads += 1;
                denied()
            },
            |_| pauses += 1,
            || {
                checked = true;
                StaleCheck::DaemonLive
            },
        );
        assert_eq!(
            refused.expect_err("a lasting denial"),
            DaemonIsElevated::Running
        );
        assert_eq!(reads, HANDSHAKE_READ_ATTEMPTS);
        assert_eq!(pauses, HANDSHAKE_READ_ATTEMPTS - 1);
        assert!(checked, "a lasting denial checks for a live daemon");
    }

    #[test]
    fn client_identity_in_keeps_its_id_in_that_dir() {
        let scratch = ScratchDir::new("identity-in");

        let first = super::client_identity_in(&scratch.0, ID_FILE).expect("creates the id file");
        let second = super::client_identity_in(&scratch.0, ID_FILE).expect("reads the id file");

        assert!(
            is_uuid_v4(&first.client_id),
            "not a UUID v4: {}",
            first.client_id
        );
        assert_eq!(first.client_id, second.client_id);
        let on_disk = std::fs::read_to_string(scratch.0.join(ID_FILE)).expect("id file exists");
        assert_eq!(on_disk, first.client_id);
    }

    #[test]
    fn stop_in_without_a_handshake_fails() {
        let scratch = ScratchDir::new("stop-none");

        let err = block_on(super::stop_in(&scratch.0)).expect_err("nothing to stop");

        assert!(
            format!("{err:#}").contains("no daemon handshake on disk"),
            "error was: {err:#}"
        );
    }

    #[test]
    fn stop_in_refuses_a_pid_that_is_not_the_daemon() {
        let scratch = ScratchDir::new("stop-unproven");
        let fake = FakeListener::healthy();
        let mut child = stand_in();
        write_handshake(&scratch.0, child.id(), fake.port, "token");

        let stopped = block_on(super::stop_in_with(
            &scratch.0,
            HANG_GUARD,
            &crate::listener_owner::listener_owner,
        ));
        let alive = alive_then_reaped(&mut child);

        let err = stopped.expect_err("an unproven listener is not stopped");
        let path = super::handshake_file_in(&scratch.0);
        assert_eq!(
            format!("{err:#}"),
            format!(
                "the listener on 127.0.0.1:{} named by {} did not prove it is the daemon; \
                 nothing was stopped and the stale handshake was removed",
                fake.port,
                path.display()
            )
        );
        assert!(alive, "the stand-in was killed");
        assert!(!path.exists(), "the stale daemon.json survived");
        assert!(!fake.saw_authorization(), "heads: {:?}", fake.heads());
    }

    #[test]
    fn stop_in_keeps_a_handshake_a_newer_daemon_wrote() {
        let scratch = ScratchDir::new("stop-newer");
        let mut child = stand_in();
        let pid = child.id();
        let dir = scratch.0.clone();
        // The listener answers its proof request only after a newer daemon's
        // handshake has replaced the file, so no timing is involved.
        let fake = FakeListener::healthy_then(move || {
            write_handshake(&dir, pid, 40999, "newer-token");
        });
        write_handshake(&scratch.0, pid, fake.port, "token");

        let stopped = block_on(super::stop_in_with(
            &scratch.0,
            HANG_GUARD,
            &crate::listener_owner::listener_owner,
        ));
        let alive = alive_then_reaped(&mut child);

        stopped.expect_err("an unproven listener is not stopped");
        assert!(alive, "the stand-in was killed");
        let kept = super::read_handshake_in(&scratch.0).expect("the newer handshake survives");
        assert_eq!(kept.auth_token, "newer-token");
    }

    #[test]
    fn stop_in_leaves_a_silent_daemon_alone() {
        let scratch = ScratchDir::new("stop-silent");
        let fake = FakeListener::silent();
        let mut child = stand_in();
        write_handshake(&scratch.0, child.id(), fake.port, "token");

        // The listener never answers, so a short proof budget only decides how
        // soon stop gives up waiting for nothing.
        let stopped = block_on(super::stop_in_with(
            &scratch.0,
            Duration::from_millis(200),
            &crate::listener_owner::listener_owner,
        ));
        let alive = alive_then_reaped(&mut child);

        let err = stopped.expect_err("a silent daemon is not stopped");
        assert_eq!(
            format!("{err:#}"),
            format!(
                "the daemon on 127.0.0.1:{} did not answer its proof; nothing was stopped",
                fake.port
            )
        );
        assert!(alive, "the stand-in was killed");
        assert!(
            super::handshake_file_in(&scratch.0).exists(),
            "daemon.json was removed"
        );
    }

    #[test]
    fn stop_in_kills_the_proven_pid_and_removes_the_handshake() {
        let scratch = ScratchDir::new("stop-kill");
        let fake = FakeListener::proving("token");
        let mut child = stand_in();
        let pid = child.id();
        write_handshake(&scratch.0, pid, fake.port, "token");

        // The fake listener lives in this test process, so the stand-in is
        // named as its owner.
        let stopped = block_on(super::stop_in_with(&scratch.0, HANG_GUARD, &move |_| {
            Ok(Some(pid))
        }));
        // Reap the child whatever happened, so a failure leaves nothing behind.
        if stopped.is_err() {
            let _ = child.kill();
        }
        let status = child.wait().expect("wait for the stand-in");

        stopped.expect("stop_in succeeds");
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;

            assert_eq!(
                status.signal(),
                Some(15),
                "the stand-in was not sent SIGTERM: {status}"
            );
        }
        assert!(
            !status.success(),
            "the stand-in exited on its own: {status}"
        );
        assert!(
            !super::handshake_file_in(&scratch.0).exists(),
            "daemon.json survived"
        );
    }

    #[test]
    fn stop_refuses_a_pid_that_does_not_own_the_proven_listener() {
        let scratch = ScratchDir::new("stop-not-owner");
        let fake = FakeListener::proving("token");
        let mut child = stand_in();
        let pid = child.id();
        write_handshake(&scratch.0, pid, fake.port, "token");

        // The proving listener lives in this test process, so this process,
        // not the stand-in, is named as its owner.
        let stopped = block_on(super::stop_in_with(&scratch.0, HANG_GUARD, &|_| {
            Ok(Some(std::process::id()))
        }));
        let alive = alive_then_reaped(&mut child);

        let err = stopped.expect_err("a pid that does not own the listener is not stopped");
        let path = crate::handshake_file_in(&scratch.0);
        assert_eq!(
            format!("{err:#}"),
            format!(
                "pid {pid} named by {} does not own the daemon's listener on 127.0.0.1:{}; \
                 nothing was stopped",
                path.display(),
                fake.port
            )
        );
        assert!(alive, "the stand-in was killed");
        assert!(path.exists(), "daemon.json was removed");
    }

    #[test]
    fn stop_refuses_when_the_owner_cannot_be_read() {
        let scratch = ScratchDir::new("stop-owner-unreadable");
        let fake = FakeListener::proving("token");
        let mut child = stand_in();
        let pid = child.id();
        write_handshake(&scratch.0, pid, fake.port, "token");

        let stopped = block_on(super::stop_in_with(&scratch.0, HANG_GUARD, &|_| {
            Err(anyhow!("lsof failed"))
        }));
        let alive = alive_then_reaped(&mut child);

        let err = stopped.expect_err("an unreadable owner is refused");
        let path = crate::handshake_file_in(&scratch.0);
        assert_eq!(
            format!("{err:#}"),
            format!(
                "pid {pid} named by {} does not own the daemon's listener on 127.0.0.1:{}; \
                 nothing was stopped",
                path.display(),
                fake.port
            )
        );
        assert!(alive, "the stand-in was killed");
        assert!(path.exists(), "daemon.json was removed");
    }

    #[test]
    fn remote_files_dir_honours_the_override() {
        const VAR: &str = "RUSTLING_TULIP_REMOTE_FILES_DIR";
        let _lock = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let prior = std::env::var_os(VAR);
        let chosen = PathBuf::from("override").join("remote-files");

        // SAFETY: ENV_LOCK is held until the end of the test, and no other
        // test in this crate reads the environment without it.
        unsafe { std::env::set_var(VAR, &chosen) };
        let overridden = super::remote_files_dir();
        // SAFETY: as above.
        unsafe { std::env::set_var(VAR, "") };
        let empty = super::remote_files_dir();
        // SAFETY: as above.
        unsafe {
            match &prior {
                Some(value) => std::env::set_var(VAR, value),
                None => std::env::remove_var(VAR),
            }
        }

        assert_eq!(overridden.expect("the override resolves"), chosen);
        let fallback = empty.expect("an empty override falls back to the data dir");
        assert!(
            fallback.ends_with("remote-files"),
            "the default is <data_local_dir>/remote-files: {}",
            fallback.display()
        );
        assert_ne!(fallback, chosen);
    }

    #[test]
    fn updates_dir_honours_the_override() {
        const VAR: &str = "RUSTLING_TULIP_UPDATES_DIR";
        let _lock = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let prior = std::env::var_os(VAR);
        let chosen = PathBuf::from("override").join("updates");

        // SAFETY: ENV_LOCK is held until the end of the test, and no other
        // test in this crate reads the environment without it.
        unsafe { std::env::set_var(VAR, &chosen) };
        let overridden = super::updates_dir();
        // SAFETY: as above.
        unsafe { std::env::set_var(VAR, "") };
        let empty = super::updates_dir();
        // SAFETY: as above.
        unsafe {
            match &prior {
                Some(value) => std::env::set_var(VAR, value),
                None => std::env::remove_var(VAR),
            }
        }

        assert_eq!(overridden.expect("the override resolves"), chosen);
        let fallback = empty.expect("an empty override falls back to the data dir");
        assert!(
            fallback.ends_with("updates"),
            "the default is <data_local_dir>/updates: {}",
            fallback.display()
        );
        assert_ne!(fallback, chosen);
        assert_ne!(fallback, PathBuf::new(), "an empty value is not the folder");
    }

    #[cfg(windows)]
    #[test]
    fn shutdown_in_sends_no_token_to_an_unproven_listener() {
        let scratch = ScratchDir::new("shutdown-unproven");
        let fake = FakeListener::healthy();
        let mut child = stand_in();
        let file = handshake_naming(child.id(), fake.port, "token", &[0]);
        write_daemon_json(&scratch.0, &file);

        let stopped = block_on(super::shutdown_in_with(&scratch.0, HANG_GUARD, HANG_GUARD));
        let alive = alive_then_reaped(&mut child);

        let outcome = stopped.expect("an unproven listener is reported, not failed");
        assert_eq!(outcome, Shutdown::NotProven(Proof::Unproven));
        assert!(alive, "the stand-in was killed");
        assert!(!fake.saw_authorization(), "heads: {:?}", fake.heads());
        assert!(
            handshake_file_in(&scratch.0).exists(),
            "daemon.json was removed"
        );
    }

    #[test]
    fn shutdown_in_without_a_handshake_has_nothing_to_stop() {
        let scratch = ScratchDir::new("shutdown-no-daemon");

        let stopped = block_on(super::shutdown_in_with(&scratch.0, HANG_GUARD, HANG_GUARD));

        let outcome = stopped.expect("nothing to stop is not a failure");
        assert_eq!(outcome, Shutdown::NoDaemon);
    }

    #[cfg(windows)]
    #[test]
    fn shutdown_in_reports_a_gone_pid_as_stale() {
        let scratch = ScratchDir::new("shutdown-gone-pid");
        let mut child = stand_in();
        let pid = child.id();
        let _ = child.kill();
        let _ = child.wait();
        let file = handshake_naming(pid, 40123, "token", &[0]);
        write_daemon_json(&scratch.0, &file);

        let stopped = block_on(super::shutdown_in_with(&scratch.0, HANG_GUARD, HANG_GUARD));

        let outcome = stopped.expect("a pid that is gone is not a failure");
        assert_eq!(outcome, Shutdown::Stale);
        assert!(
            !handshake_file_in(&scratch.0).exists(),
            "the stale daemon.json survived"
        );
    }

    fn located(profile: Profile, config_dir: &str) -> LocatedDaemon {
        LocatedDaemon {
            profile,
            config_dir: PathBuf::from(config_dir),
        }
    }

    fn is(dir: &Path, name: &str) -> bool {
        dir == std::path::Path::new(name)
    }

    #[test]
    fn locate_finds_the_ordinary_daemon_first_for_an_elevated_caller() {
        let candidates = [
            located(Profile::Ordinary, "ordinary"),
            located(Profile::Elevated, "elevated"),
        ];

        let found = locate_in(
            &candidates,
            |dir| is(dir, "ordinary") || is(dir, "elevated"),
            |dir| is(dir, "ordinary") || is(dir, "elevated"),
        );

        assert_eq!(found, Some(located(Profile::Ordinary, "ordinary")));
    }

    #[test]
    fn locate_finds_the_elevated_daemon_when_no_ordinary_one_runs() {
        let candidates = [
            located(Profile::Ordinary, "ordinary"),
            located(Profile::Elevated, "elevated"),
        ];

        let found = locate_in(
            &candidates,
            |dir| is(dir, "elevated"),
            |dir| is(dir, "elevated"),
        );

        assert_eq!(found, Some(located(Profile::Elevated, "elevated")));
    }

    #[test]
    fn locate_falls_back_to_the_callers_own_profile() {
        let candidates = [
            located(Profile::Ordinary, "ordinary"),
            located(Profile::Elevated, "elevated"),
        ];

        let found = locate_in(&candidates, |_| false, |_| false);

        assert_eq!(
            found,
            Some(located(Profile::Elevated, "elevated")),
            "with no daemon running the caller names its own profile's folder"
        );
    }

    #[test]
    fn a_dead_ordinary_daemon_falls_through_to_the_elevated_profile() {
        let candidates = [
            located(Profile::Ordinary, "ordinary"),
            located(Profile::Elevated, "elevated"),
        ];

        // Both folders hold a `daemon.json`, but only the elevated one's daemon
        // holds its instance lock: the ordinary file is a crashed daemon's.
        let found = locate_in(&candidates, |_| true, |dir| is(dir, "elevated"));

        assert_eq!(found, Some(located(Profile::Elevated, "elevated")));
    }

    #[test]
    fn an_ordinary_caller_locates_only_the_ordinary_profile() {
        let candidates = [located(Profile::Ordinary, "ordinary")];
        let looked_at = std::cell::RefCell::new(Vec::new());

        let found = locate_in(
            &candidates,
            |dir| {
                looked_at.borrow_mut().push(dir.to_path_buf());
                false
            },
            |_| false,
        );

        assert_eq!(found, Some(located(Profile::Ordinary, "ordinary")));
        assert!(
            looked_at.borrow().is_empty(),
            "its own profile is returned unprobed and no other profile is looked at"
        );
    }

    #[test]
    fn an_elevated_caller_may_not_stop_an_ordinary_daemon() {
        let refusal = may_stop(Profile::Elevated, Ok(Profile::Ordinary))
            .expect_err("an elevated caller must not stop an ordinary daemon");

        assert_eq!(refusal, DaemonIsOrdinary);
        assert_eq!(
            refusal.to_string(),
            "the running rustling-tulip daemon runs without administrator rights; \
             stop it from an ordinary terminal with \"rustling-tulipd shutdown\""
        );
    }

    #[test]
    fn a_caller_may_stop_a_daemon_of_its_own_profile() {
        assert_eq!(may_stop(Profile::Ordinary, Ok(Profile::Ordinary)), Ok(()));
        assert_eq!(may_stop(Profile::Elevated, Ok(Profile::Elevated)), Ok(()));
    }

    #[test]
    fn an_elevated_caller_may_stop_an_elevated_daemon_left_in_the_ordinary_folder() {
        // The folder is the ordinary profile's, but the process runs at the
        // caller's own integrity, as a daemon an older release left there does.
        assert_eq!(may_stop(Profile::Elevated, Ok(Profile::Elevated)), Ok(()));
    }

    #[test]
    fn an_unreadable_daemon_integrity_blocks_an_elevated_stop() {
        let refusal = may_stop(
            Profile::Elevated,
            Err(anyhow!("reading process 4242's integrity: access denied")),
        );

        assert_eq!(
            refusal,
            Err(DaemonIsOrdinary),
            "an integrity that cannot be read is not proven to be the caller's own"
        );
        assert_eq!(
            may_stop(
                Profile::Ordinary,
                Err(anyhow!("reading process 4242's integrity: access denied"))
            ),
            Ok(())
        );
    }

    #[cfg(windows)]
    #[test]
    fn an_elevated_stop_of_a_dead_daemon_is_not_refused() {
        let scratch = ScratchDir::new("stop-dead-elevated");
        // Windows pids are multiples of 4, so no process can have this one.
        let dead_pid = u32::MAX;
        write_handshake(&scratch.0, dead_pid, 40123, "token");

        let refusal = super::refuse_stop_across_integrity(
            Profile::Elevated,
            &LocatedDaemon {
                profile: Profile::Elevated,
                config_dir: scratch.0.clone(),
            },
        );

        assert_eq!(
            refusal,
            Ok(()),
            "nothing runs to stop across integrity; the stop's own path removes the stale file"
        );
    }

    #[cfg(windows)]
    #[test]
    fn an_elevated_stop_of_a_running_ordinary_pid_is_refused() {
        let scratch = ScratchDir::new("stop-running-ordinary");
        let mut child = stand_in();
        write_handshake(&scratch.0, child.id(), 40123, "token");

        let refusal = super::refuse_stop_across_integrity(
            Profile::Elevated,
            &LocatedDaemon {
                profile: Profile::Elevated,
                config_dir: scratch.0.clone(),
            },
        );
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(
            refusal,
            Err(DaemonIsOrdinary),
            "a running ordinary daemon is not an elevated caller's to stop"
        );
    }
}
