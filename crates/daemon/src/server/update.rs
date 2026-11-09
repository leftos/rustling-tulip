//! The host update request: `RemoteUpdate` asks this host to update itself to
//! the newest official release. The request names nothing; the daemon checks,
//! in a fixed order, that it can update the install it runs from, and answers
//! an `Error` carrying the request's id with the first reason it cannot.
//!
//! The checks, first that applies: not Windows, a dev build, an elevated
//! daemon, no install found, an install that is not this daemon's own, a
//! process that would block the swap (the app aside), another update already
//! running in this daemon. The checks run on a blocking thread, since the
//! process list and the registry read are slow.

use super::Hub;
use super::connection::ConnCtx;
use crate::cli_apply_update::{CLIENT_EXE, ProcessList, RunningExe};
use crate::sync::lock;
use protocol::error_codes::{BUSY, INTERNAL, UPDATE_REFUSED};
use protocol::{ClientMessage, DaemonMessage};
use std::path::{Path, PathBuf};
use tracing::{debug, info};

/// The daemon's file name in an install folder: an install holds one.
#[cfg(windows)]
const DAEMON_EXE: &str = "rustling-tulipd.exe";
#[cfg(not(windows))]
const DAEMON_EXE: &str = "rustling-tulipd";

/// The folder in an install that the installer stages an update into.
const STAGING_DIR: &str = ".staging";

const NOT_WINDOWS: &str = "This host can't update itself remotely yet on this system.";
const DEV_BUILD: &str = "This host runs a development build, which doesn't update itself.";
const ELEVATED: &str = "This host's daemon runs elevated; update it from the host.";
const NO_INSTALL: &str = "No installed copy of rustling-tulip was found on this host.";
const NOT_OWN_INSTALL: &str =
    "This daemon doesn't run from the installed copy, so it can't update it.";
const ANOTHER_UPDATE: &str = "Another update is running on this host.";
const SOMETHING_RUNS: &str =
    "Something else is running from this host's install folder; close it and try again.";
const NOT_YET: &str = "This host can't update itself remotely yet.";

/// A refusal's error code and message.
type Refusal = (&'static str, &'static str);

/// Where this host's installed copy is.
pub(crate) trait InstallLocator {
    /// The install folder, when an installed copy (one holding the daemon) is
    /// found.
    fn install_folder(&self) -> Option<PathBuf>;
}

/// What a remote update checks the host against, and the guard that keeps one
/// update at a time in this daemon.
pub(crate) struct HostUpdate {
    install: Box<dyn InstallLocator + Send + Sync>,
    processes: Box<dyn ProcessList + Send + Sync>,
    /// True while an update runs in this daemon; checked and set in one lock
    /// hold, never held across an `.await`.
    in_flight: std::sync::Mutex<bool>,
}

impl HostUpdate {
    /// Checks against `install` and `processes`, no update running.
    pub(crate) fn new(
        install: Box<dyn InstallLocator + Send + Sync>,
        processes: Box<dyn ProcessList + Send + Sync>,
    ) -> Self {
        Self {
            install,
            processes,
            in_flight: std::sync::Mutex::new(false),
        }
    }

    /// Checks against this machine: the installer's registry entry and the
    /// running processes.
    pub(crate) fn system() -> Self {
        Self::new(Box::new(RegistryInstall), Box::new(SystemProcesses))
    }

    /// A test hub's checks: no install, no processes.
    #[cfg(test)]
    pub(crate) fn nothing_installed() -> Self {
        struct Nothing;
        impl InstallLocator for Nothing {
            fn install_folder(&self) -> Option<PathBuf> {
                None
            }
        }
        impl ProcessList for Nothing {
            fn running(&self) -> Vec<RunningExe> {
                Vec::new()
            }
        }
        Self::new(Box::new(Nothing), Box::new(Nothing))
    }

    /// Claims the guard: false when an update already holds it.
    fn try_begin(&self) -> bool {
        let mut in_flight = lock(&self.in_flight);
        if *in_flight {
            return false;
        }
        *in_flight = true;
        true
    }

    /// Releases the guard.
    fn finish(&self) {
        *lock(&self.in_flight) = false;
    }
}

/// Handle the host update message `dispatch` routes here.
pub(super) async fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) -> anyhow::Result<()> {
    let ClientMessage::RemoteUpdate { request_id } = msg else {
        debug!(
            ?msg,
            "remote update: a message that is not an update request was routed here"
        );
        return Ok(());
    };
    let hub = hub.clone();
    let answered = tokio::task::spawn_blocking(move || answer(&hub)).await;
    let (code, message) = match answered {
        Ok((code, message)) => (code, message.to_owned()),
        Err(err) => (
            INTERNAL,
            format!("remote update: the checks failed to run: {err}"),
        ),
    };
    info!(%request_id, code, "remote update: refused");
    // A closed outbox is a connection already ending.
    let _ = ctx.out_tx.send(DaemonMessage::Error {
        message,
        request_id: Some(request_id),
        code: Some(code.to_owned()),
    });
    Ok(())
}

/// The answer to an update request. Blocking work.
fn answer(hub: &Hub) -> Refusal {
    if let Some(refusal) = host_refusal(hub) {
        return refusal;
    }
    let update = &hub.host_update;
    if !update.try_begin() {
        return (BUSY, ANOTHER_UPDATE);
    }
    update.finish();
    (UPDATE_REFUSED, NOT_YET)
}

/// Why this host cannot update itself now, the first check that applies;
/// `None` when every check passes.
fn host_refusal(hub: &Hub) -> Option<Refusal> {
    if let Some(refusal) = platform_refusal()
        .or_else(dev_build_refusal)
        .or_else(|| elevated_refusal(hub))
    {
        return Some(refusal);
    }
    let Some(install) = hub.host_update.install.install_folder() else {
        return Some((UPDATE_REFUSED, NO_INSTALL));
    };
    let updates = daemon_client::updates_dir()
        .inspect_err(|err| tracing::warn!(error = %err, "remote update: no updates folder; its busy check is skipped"))
        .ok();
    own_install_refusal(&install, hub.dirs.bin_templates.as_deref()).or_else(|| {
        process_refusal(
            &install,
            &hub.host_update.processes.running(),
            updates.as_deref(),
        )
    })
}

fn platform_refusal() -> Option<Refusal> {
    (!cfg!(windows)).then_some((UPDATE_REFUSED, NOT_WINDOWS))
}

fn dev_build_refusal() -> Option<Refusal> {
    (!update_core::checks_allowed()).then_some((UPDATE_REFUSED, DEV_BUILD))
}

fn elevated_refusal(hub: &Hub) -> Option<Refusal> {
    hub.integrity
        .is_elevated()
        .then_some((UPDATE_REFUSED, ELEVATED))
}

/// Refused unless this daemon's own folder (`bin_templates`) is `install`.
fn own_install_refusal(install: &Path, own: Option<&Path>) -> Option<Refusal> {
    let own = own.map(crate::normalize_process_path);
    (own != Some(crate::normalize_process_path(install)))
        .then_some((UPDATE_REFUSED, NOT_OWN_INSTALL))
}

/// Busy while a process other than this one runs from `install`'s staging
/// folder or from under `updates`; refused while one other than the app runs
/// from `install` itself.
fn process_refusal(
    install: &Path,
    running: &[RunningExe],
    updates: Option<&Path>,
) -> Option<Refusal> {
    let own_pid = std::process::id();
    let others = || running.iter().filter(move |process| process.pid != own_pid);
    let staging = install.join(STAGING_DIR);
    let updating = others().any(|process| {
        crate::path_is_under(&process.exe, &staging)
            || updates.is_some_and(|updates| crate::path_is_under(&process.exe, updates))
    });
    if updating {
        return Some((BUSY, ANOTHER_UPDATE));
    }
    let folder = crate::normalize_process_path(install);
    others()
        .any(|process| {
            !is_client(&process.exe)
                && process
                    .exe
                    .parent()
                    .is_some_and(|parent| crate::normalize_process_path(parent) == folder)
        })
        .then_some((UPDATE_REFUSED, SOMETHING_RUNS))
}

/// Whether `exe` is the app, which an update closes itself.
fn is_client(exe: &Path) -> bool {
    exe.file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(CLIENT_EXE))
}

/// The installer's uninstall entry: its `InstallLocation`, when the folder
/// holds the daemon.
struct RegistryInstall;

impl InstallLocator for RegistryInstall {
    fn install_folder(&self) -> Option<PathBuf> {
        let folder = PathBuf::from(registry::install_location()?);
        folder.join(DAEMON_EXE).is_file().then_some(folder)
    }
}

#[cfg(windows)]
mod registry {
    use tracing::warn;
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::w;

    /// Reads of a value whose size can change between the size query and the
    /// read, the size query included.
    const ATTEMPTS: usize = 4;

    /// The installer's `InstallLocation` under its per-user uninstall key
    /// (`UNINSTKEY` in `tools/installer/rustling-tulip.nsi`). `None` when the
    /// key or value is absent or can't be read (logged).
    pub(super) fn install_location() -> Option<String> {
        let mut size: u32 = 0;
        for _ in 0..ATTEMPTS {
            let mut buf = vec![0u16; (size as usize).div_ceil(2)];
            let mut written = u32::try_from(buf.len() * 2).unwrap_or(u32::MAX);
            let data = (!buf.is_empty()).then(|| buf.as_mut_ptr().cast());
            // SAFETY: the key and value names are valid null-terminated wide
            // strings; `buf` holds `written` bytes and outlives the call, which
            // writes at most that many bytes into it and the byte count it
            // needs or wrote into `written`; with no buffer it writes only the
            // size.
            let status = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    w!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\rustling-tulip"),
                    w!("InstallLocation"),
                    RRF_RT_REG_SZ,
                    None,
                    data,
                    Some(&raw mut written),
                )
            };
            if status == ERROR_FILE_NOT_FOUND {
                return None;
            }
            if status == ERROR_MORE_DATA || (status == ERROR_SUCCESS && buf.is_empty()) {
                size = written;
                continue;
            }
            if status != ERROR_SUCCESS {
                warn!(
                    code = status.0,
                    "remote update: reading the install location failed"
                );
                return None;
            }
            let chars = (written as usize / 2).min(buf.len());
            let text = &buf[..chars];
            let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
            return Some(String::from_utf16_lossy(&text[..end]));
        }
        warn!("remote update: the install location kept growing while being read");
        None
    }
}

#[cfg(not(windows))]
mod registry {
    /// No installer registry off Windows.
    pub(super) fn install_location() -> Option<String> {
        None
    }
}

/// This machine's processes, read through sysinfo.
struct SystemProcesses;

impl ProcessList for SystemProcesses {
    fn running(&self) -> Vec<RunningExe> {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind};

        let refresh = ProcessRefreshKind::new().with_exe(UpdateKind::Always);
        let mut sys = System::new_with_specifics(RefreshKind::new().with_processes(refresh));
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        sys.processes()
            .iter()
            .filter_map(|(pid, process)| {
                Some(RunningExe {
                    pid: pid.as_u32(),
                    exe: process.exe()?.to_path_buf(),
                })
            })
            .collect()
    }
}

#[cfg(all(test, windows))]
#[expect(
    clippy::panic,
    reason = "tests fail with the reply they got; panic messages aid debugging"
)]
mod tests {
    use super::super::test_support::{ScratchDir, dispatch_one, test_hub_in};
    use super::*;
    use std::sync::Arc;

    /// The folder under the scratch dir the hub's daemon runs from.
    const OWN: &str = "install";

    struct FakeInstall(Option<PathBuf>);

    impl InstallLocator for FakeInstall {
        fn install_folder(&self) -> Option<PathBuf> {
            self.0.clone()
        }
    }

    struct FakeProcesses(Vec<RunningExe>);

    impl ProcessList for FakeProcesses {
        fn running(&self) -> Vec<RunningExe> {
            self.0.clone()
        }
    }

    /// A hub whose daemon runs from `<scratch>/install`, the locator finding
    /// `<scratch>/<install>`, and other processes running the exes at
    /// `<scratch>/<running>`.
    fn hub_over(tag: &str, install: Option<&str>, running: &[&str]) -> (Hub, ScratchDir) {
        let scratch = ScratchDir::new(tag);
        let root = scratch.path().to_path_buf();
        let (mut hub, scratch) = test_hub_in(scratch, Some(root.join(OWN)));
        let other = std::process::id().wrapping_add(1);
        let processes = running
            .iter()
            .map(|exe| RunningExe {
                pid: other,
                exe: root.join(exe),
            })
            .collect();
        hub.host_update = Arc::new(HostUpdate::new(
            Box::new(FakeInstall(install.map(|folder| root.join(folder)))),
            Box::new(FakeProcesses(processes)),
        ));
        (hub, scratch)
    }

    fn request() -> ClientMessage {
        ClientMessage::RemoteUpdate {
            request_id: "u1".to_owned(),
        }
    }

    /// The code and message of a refusal carrying the request's id.
    fn refusal(reply: DaemonMessage) -> (String, String) {
        let DaemonMessage::Error {
            message,
            request_id,
            code,
        } = reply
        else {
            panic!("expected a refusal, got {reply:?}");
        };
        assert_eq!(request_id.as_deref(), Some("u1"));
        (code.unwrap_or_default(), message)
    }

    fn expected(code: &str, message: &str) -> (String, String) {
        (code.to_owned(), message.to_owned())
    }

    #[tokio::test]
    async fn remote_update_refuses_a_host_with_no_install() {
        let (hub, _scratch) = hub_over("update-no-install", None, &[]);

        let reply = refusal(dispatch_one(&hub, request()).await);

        assert_eq!(reply, expected(UPDATE_REFUSED, NO_INSTALL));
    }

    #[tokio::test]
    async fn remote_update_refuses_an_install_that_is_not_this_daemons() {
        let (hub, _scratch) = hub_over("update-not-own", Some("elsewhere"), &[]);

        let reply = refusal(dispatch_one(&hub, request()).await);

        assert_eq!(reply, expected(UPDATE_REFUSED, NOT_OWN_INSTALL));
    }

    #[tokio::test]
    async fn remote_update_is_busy_while_a_process_runs_from_staging() {
        let (hub, _scratch) = hub_over(
            "update-staging",
            Some(OWN),
            &["install/.staging/rustling-tulipd.exe"],
        );

        let reply = refusal(dispatch_one(&hub, request()).await);

        assert_eq!(reply, expected(BUSY, ANOTHER_UPDATE));
    }

    #[tokio::test]
    async fn remote_update_refuses_while_a_bridge_runs_from_the_install() {
        let (hub, _scratch) =
            hub_over("update-bridge", Some(OWN), &["install/rustling-tulipd.exe"]);

        let reply = refusal(dispatch_one(&hub, request()).await);

        assert_eq!(reply, expected(UPDATE_REFUSED, SOMETHING_RUNS));
    }

    #[tokio::test]
    async fn remote_update_is_busy_while_another_update_runs() {
        let (hub, _scratch) = hub_over("update-in-flight", Some(OWN), &[]);
        *lock(&hub.host_update.in_flight) = true;

        let reply = refusal(dispatch_one(&hub, request()).await);

        assert_eq!(reply, expected(BUSY, ANOTHER_UPDATE));
        assert!(
            *lock(&hub.host_update.in_flight),
            "the running update keeps its guard"
        );
    }

    #[tokio::test]
    async fn remote_update_answers_the_placeholder_when_every_check_passes() {
        let (hub, _scratch) = hub_over(
            "update-placeholder",
            Some(OWN),
            &[
                "install/rustling-tulip-native.exe",
                "elsewhere/rustling-tulipd.exe",
            ],
        );

        let reply = refusal(dispatch_one(&hub, request()).await);

        assert_eq!(reply, expected(UPDATE_REFUSED, NOT_YET));
        assert!(
            !*lock(&hub.host_update.in_flight),
            "the placeholder releases the guard"
        );
    }
}
