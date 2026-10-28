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
pub mod device_tokens;
#[cfg(windows)]
mod pinned_process;
mod proof;
mod supervisor;
#[cfg(test)]
mod test_fakes;

pub use proof::{Proof, ProvenConnectError, connect_proven};
pub use supervisor::{PROOF_TIMEOUT, ensure_running};

use anyhow::{Context as _, anyhow, bail};
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

/// Path the daemon writes its handshake (`daemon.json`) to.
///
/// Mirrors `daemon::paths::Dirs::ensure().handshake_file` in
/// `crates/daemon/src/paths.rs`.
///
/// # Errors
///
/// Fails when [`config_dir`] does.
pub fn handshake_file() -> anyhow::Result<PathBuf> {
    Ok(handshake_file_in(&config_dir()?))
}

/// Path of the handshake file (`daemon.json`) under `config_dir`.
#[must_use]
pub fn handshake_file_in(config_dir: &Path) -> PathBuf {
    config_dir.join("daemon.json")
}

/// Read and parse `daemon.json` without checking that the daemon it names is
/// alive.
///
/// # Errors
///
/// Fails when the file cannot be read (`read <path>: …`) or is not a valid
/// handshake (`parse handshake: …`).
pub fn read_handshake() -> anyhow::Result<DaemonHandshake> {
    read_handshake_in(&config_dir()?)
}

/// [`read_handshake`] for the daemon whose config dir is `config_dir`.
///
/// # Errors
///
/// As [`read_handshake`].
pub fn read_handshake_in(config_dir: &Path) -> anyhow::Result<DaemonHandshake> {
    let path = handshake_file_in(config_dir);
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).context("parse handshake")
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

/// Force-stop the running daemon: read `daemon.json`, have the listener on its
/// port prove it holds its token, kill the process its pid names and remove
/// the handshake file.
///
/// On Windows the pid is pinned by a process handle before the proof and
/// killed through that handle, so a pid reused by another process is never
/// killed. Kills by pid rather than sending a `Shutdown` WS message because
/// the WS may already be closed, and a pid kill works in both states. The
/// daemon's drop guard would normally remove `daemon.json` on graceful exit;
/// it is removed here too so a later [`ensure_running`] doesn't mistake a
/// stale handshake for a live daemon.
///
/// # Errors
///
/// Fails when there is no handshake on disk or it cannot be read or parsed;
/// on Windows only, when its pid is not running (the stale file is removed)
/// or this client may not terminate it (nothing is stopped); when its
/// listener does not prove it is the daemon (nothing is killed; the file is
/// removed, except when the listener did not answer at all); or when the kill
/// fails. The file is only ever removed while it still holds the token that
/// was read.
pub async fn stop() -> anyhow::Result<()> {
    stop_in(&config_dir()?).await
}

/// [`stop`] for the daemon whose config dir is `config_dir`.
///
/// # Errors
///
/// As [`stop`].
pub async fn stop_in(config_dir: &Path) -> anyhow::Result<()> {
    stop_in_with(config_dir, supervisor::PROOF_TIMEOUT).await
}

/// [`stop_in`] with `proof_budget` for the listener's proof.
///
/// # Errors
///
/// As [`stop`].
pub(crate) async fn stop_in_with(config_dir: &Path, proof_budget: Duration) -> anyhow::Result<()> {
    let path = handshake_file_in(config_dir);
    if !path.exists() {
        bail!("no daemon handshake on disk — daemon may not be running");
    }
    let parsed = read_handshake_in(config_dir)?;
    #[cfg(windows)]
    let pinned = pin_for_stop(config_dir, &parsed, &path).await?;
    let proof = proof::prove(parsed.port, &parsed.auth_token, proof_budget).await;
    refuse_unproven(proof, config_dir, &parsed, &path).await?;
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
    match supervisor::retire_daemon_in(config_dir, &handshake, proof_budget, shutdown_wait).await {
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
    use super::{Proof, Shutdown, client_identity, handshake_file_in};
    use crate::test_fakes::{
        FakeListener, ScratchDir, block_on, handshake_naming, stand_in, write_daemon_json,
    };
    use std::ffi::OsString;
    use std::path::PathBuf;
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

        let stopped = block_on(super::stop_in_with(&scratch.0, HANG_GUARD));
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

        let stopped = block_on(super::stop_in_with(&scratch.0, HANG_GUARD));
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
        let stopped = block_on(super::stop_in_with(&scratch.0, Duration::from_millis(200)));
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

    #[cfg(windows)]
    #[test]
    fn stop_in_kills_the_proven_pid_and_removes_the_handshake() {
        let scratch = ScratchDir::new("stop-kill");
        let fake = FakeListener::proving("token");
        let mut child = stand_in();
        write_handshake(&scratch.0, child.id(), fake.port, "token");

        let stopped = block_on(super::stop_in_with(&scratch.0, HANG_GUARD));
        // Reap the child whatever happened, so a failure leaves nothing behind.
        if stopped.is_err() {
            let _ = child.kill();
        }
        let status = child.wait().expect("wait for the stand-in");

        stopped.expect("stop_in succeeds");
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
}
