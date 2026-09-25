//! Daemon supervision shared by the rustling-tulip clients.
//!
//! A client calls [`ensure_running`] to get the handshake of a healthy daemon
//! it can speak to: it reuses a running `rustling-tulipd`, retires one whose
//! protocol (or, under [`RetirePolicy::RetireStale`], binary) is out of date,
//! or spawns a fresh one from the
//! content-addressed binary cache. The crate also resolves the per-user files
//! every client shares with the daemon (the config dir, `daemon.json`, the
//! client-identity file) and force-stops the daemon ([`stop`]).
//!
//! Paths are resolved the same way `crates/daemon/src/paths.rs` resolves them,
//! so a client and the daemon it spawns agree on where each file lives.

mod supervisor;

pub use supervisor::{RetirePolicy, ensure_running, locate_daemon_binary};

use anyhow::{Context as _, anyhow, bail};
use protocol::DaemonHandshake;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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

/// Force-stop the running daemon: read its pid from `daemon.json`, kill the
/// process and remove the handshake file.
///
/// Kills by pid rather than sending a `Shutdown` WS message because the WS may
/// already be closed, and a pid kill works in both states. The daemon's drop
/// guard would normally remove `daemon.json` on graceful exit; it is removed
/// here too so a later [`ensure_running`] doesn't mistake a stale handshake for
/// a live daemon.
///
/// # Errors
///
/// Fails when there is no handshake on disk, it cannot be read or parsed, or
/// the kill fails.
pub async fn stop() -> anyhow::Result<()> {
    stop_in(&config_dir()?).await
}

/// [`stop`] for the daemon whose config dir is `config_dir`.
///
/// # Errors
///
/// As [`stop`].
pub async fn stop_in(config_dir: &Path) -> anyhow::Result<()> {
    let path = handshake_file_in(config_dir);
    if !path.exists() {
        bail!("no daemon handshake on disk — daemon may not be running");
    }
    let parsed = read_handshake_in(config_dir)?;
    kill_pid(parsed.pid).await?;
    // Best-effort cleanup; absence will be detected on the next ensure_running
    // regardless. Don't error if the daemon's drop guard beat us to it.
    let _ = tokio::fs::remove_file(&path).await;
    Ok(())
}

/// Force-kill a process by pid with `taskkill /F`.
///
/// # Errors
///
/// Fails when the kill command cannot be run or exits unsuccessfully.
#[cfg(windows)]
pub async fn kill_pid(pid: u32) -> anyhow::Result<()> {
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
pub async fn kill_pid(pid: u32) -> anyhow::Result<()> {
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

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::client_identity;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::{Mutex, PoisonError};
    use uuid::Uuid;

    const ID_FILE: &str = "client-id-test";
    const CONFIG_DIR_VAR: &str = "RUSTLING_TULIP_CONFIG_DIR";

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

    /// A directory under `<workspace>/.tmp` that the `*_in` functions are
    /// pointed at explicitly; the environment is left alone. Drop removes it.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(label: &str) -> Self {
            let path = crate::supervisor::dev_workspace_root()
                .expect("workspace root resolves")
                .join(".tmp")
                .join(format!("daemon-client-{label}-{}", Uuid::new_v4().simple()));
            std::fs::create_dir_all(&path).expect("create scratch dir");
            Self(path)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_handshake(dir: &std::path::Path, pid: u32) {
        let handshake = protocol::DaemonHandshake {
            protocol_version: 7,
            port: 40123,
            auth_token: "token".to_owned(),
            pid,
        };
        let json = serde_json::to_vec(&handshake).expect("encode handshake");
        std::fs::write(super::handshake_file_in(dir), json).expect("write daemon.json");
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build a test runtime")
            .block_on(future)
    }

    #[test]
    fn read_handshake_in_reads_that_dirs_daemon_json() {
        let scratch = ScratchDir::new("handshake");
        write_handshake(&scratch.0, 99);

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

    #[cfg(windows)]
    #[test]
    fn stop_in_kills_the_named_pid_and_removes_the_handshake() {
        let scratch = ScratchDir::new("stop-kill");
        let mut child = std::process::Command::new("ping")
            .args(["-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn a stand-in daemon");
        write_handshake(&scratch.0, child.id());

        let stopped = block_on(super::stop_in(&scratch.0));
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
}
