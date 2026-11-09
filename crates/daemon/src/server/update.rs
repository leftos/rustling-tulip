//! The host update request: `RemoteUpdate` asks this host to update itself to
//! the newest official release. The request names nothing; the daemon checks,
//! in a fixed order, that it can update the install it runs from, and answers
//! an `Error` carrying the request's id with the first reason it cannot.
//!
//! The checks, first that applies: not Windows, a dev build, an elevated
//! daemon, no install found, no updates folder, an install that is not this
//! daemon's own, a process that would block the swap (the app included),
//! another update already running in this daemon. The checks run on a
//! blocking thread, since the process list and the registry read are slow.
//!
//! When every check passes, the update runs on a plain thread of its own (a
//! blocking task would hold up the daemon's exit for a long download): it
//! takes the update mutex, clears old downloads, downloads and verifies the
//! release, and starts its installer with fixed arguments. The mutex is
//! taken, held and dropped on that one thread, and the verified file stays
//! open until the installer has started. The answer is
//! `RemoteUpdateStarted` once the installer runs, else an `Error`; exactly
//! one per request.

use super::Hub;
use super::connection::ConnCtx;
use crate::cli_apply_update::{CLIENT_EXE, ProcessList, RunningExe};
use crate::sync::lock;
use protocol::error_codes::{BUSY, INTERNAL, UPDATE_CANCELED, UPDATE_REFUSED};
use protocol::{ClientMessage, DaemonMessage};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

/// The daemon's file name in an install folder: an install holds one.
#[cfg(windows)]
const DAEMON_EXE: &str = "rustling-tulipd.exe";
#[cfg(not(windows))]
const DAEMON_EXE: &str = "rustling-tulipd";

/// The folder in an install that the installer stages an update into.
const STAGING_DIR: &str = ".staging";

/// The installer's arguments: silent, in its update mode, restarting the
/// daemon after the swap. Nothing in them comes from the request.
const INSTALLER_ARGS: [&str; 3] = ["/S", "/UPDATE", "/RESTARTDAEMON"];

const NOT_WINDOWS: &str = "This host can't update itself remotely yet on this system.";
const DEV_BUILD: &str = "This host runs a development build, which doesn't update itself.";
const ELEVATED: &str = "This host's daemon runs elevated; update it from the host.";
const NO_INSTALL: &str = "No installed copy of rustling-tulip was found on this host.";
const NO_UPDATES_DIR: &str = "This host has no updates folder.";
const NOT_OWN_INSTALL: &str =
    "This daemon doesn't run from the installed copy, so it can't update it.";
const ANOTHER_UPDATE: &str = "Another update is running on this host.";
const APP_OPEN: &str = "rustling-tulip is open on this host; close it there and try again.";
const SOMETHING_RUNS: &str =
    "Something else is running from this host's install folder; close it and try again.";
const NO_LOCK: &str = "This host couldn't take the update lock.";
const ALREADY_NEWEST: &str = "This host already runs the newest release.";
const DOWNLOAD_FAILED: &str = "This host couldn't download the update; try again later.";
const CANCELED: &str = "The update was cancelled on the host.";
const NO_INSTALLER: &str = "This host couldn't start the installer.";

/// A refusal's error code and message.
type Refusal = (&'static str, &'static str);

/// Where this host's installed copy is.
pub(crate) trait InstallLocator {
    /// The install folder, when an installed copy (one holding the daemon) is
    /// found.
    fn install_folder(&self) -> Option<PathBuf>;
}

/// The update mutex the installer's `apply-update` also takes.
pub(crate) trait UpdateLock: Send + Sync {
    /// Takes the mutex without waiting: the hold keeps it until dropped, on
    /// the thread that took it; `None` when another process holds it.
    ///
    /// # Errors
    ///
    /// Fails when the mutex cannot be created or waited on.
    fn try_lock(&self) -> anyhow::Result<Option<Box<dyn std::any::Any>>>;
}

/// Where a release newer than this host's comes from.
pub(crate) trait UpdateSource: Send + Sync {
    /// The release newer than `current`, downloaded and verified under
    /// `root`; `None` when this host runs the newest.
    ///
    /// # Errors
    ///
    /// Fails when the feed, the download or the verification fails.
    fn fetch(
        &self,
        current: &semver::Version,
        root: &Path,
    ) -> anyhow::Result<Option<update_core::Ready>>;
}

/// Starts the verified installer.
pub(crate) trait Launch: Send + Sync {
    /// Starts `program` with `args`, detached from this daemon.
    ///
    /// # Errors
    ///
    /// Fails when the program does not start.
    fn launch(&self, program: &Path, args: &[OsString]) -> anyhow::Result<()>;
}

/// The update's own seams: the mutex, the release source, the installer
/// launch, and the updates folder downloads go under.
pub(crate) struct Seams {
    pub(crate) lock: Box<dyn UpdateLock>,
    pub(crate) source: Box<dyn UpdateSource>,
    pub(crate) launcher: Box<dyn Launch>,
    pub(crate) updates_dir: Box<dyn Fn() -> anyhow::Result<PathBuf> + Send + Sync>,
}

impl Seams {
    /// This machine's: the named mutex, the release feed, a detached launch
    /// and the client's updates folder.
    pub(crate) fn system() -> Self {
        Self {
            lock: Box::new(NamedMutex),
            source: Box::new(ReleaseFeed),
            launcher: Box::new(DetachedLaunch),
            updates_dir: Box::new(daemon_client::updates_dir),
        }
    }
}

/// What a remote update checks the host against, the seams it runs through,
/// and the guard that keeps one update at a time in this daemon.
pub(crate) struct HostUpdate {
    install: Box<dyn InstallLocator + Send + Sync>,
    processes: Box<dyn ProcessList + Send + Sync>,
    seams: Seams,
    /// True while an update runs in this daemon; checked and set in one lock
    /// hold, never held across an `.await`. It stays set once the installer
    /// has started, which is about to stop this daemon.
    in_flight: std::sync::Mutex<bool>,
    /// True from the update's start until it answers anything but started:
    /// the idle-exit watcher's hold-off for it.
    pub(crate) update_in_flight: Arc<AtomicBool>,
}

impl HostUpdate {
    /// Checks against `install` and `processes`, updating through `seams`, no
    /// update running.
    pub(crate) fn new(
        install: Box<dyn InstallLocator + Send + Sync>,
        processes: Box<dyn ProcessList + Send + Sync>,
        seams: Seams,
    ) -> Self {
        Self {
            install,
            processes,
            seams,
            in_flight: std::sync::Mutex::new(false),
            update_in_flight: Arc::default(),
        }
    }

    /// Checks against this machine, the installer's registry entry and the
    /// running processes, and updates through its seams.
    pub(crate) fn system() -> Self {
        Self::new(
            Box::new(RegistryInstall),
            Box::new(SystemProcesses),
            Seams::system(),
        )
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
        Self::new(Box::new(Nothing), Box::new(Nothing), Seams::system())
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

    /// Releases the guard and the idle-exit hold-off.
    fn finish(&self) {
        self.update_in_flight.store(false, Ordering::SeqCst);
        *lock(&self.in_flight) = false;
    }
}

/// What the checks hand the update: the version this host runs and the
/// updates folder.
struct Plan {
    current: semver::Version,
    updates_root: PathBuf,
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
    let checking = hub.clone();
    let checked = tokio::task::spawn_blocking(move || begin(&checking)).await;
    let plan = match checked {
        Ok(Ok(plan)) => plan,
        Ok(Err((code, message))) => {
            send(
                ctx.out_tx,
                refusal_reply(request_id, code, message.to_owned()),
            );
            return Ok(());
        }
        Err(err) => {
            let message = format!("remote update: the checks failed to run: {err}");
            send(ctx.out_tx, refusal_reply(request_id, INTERNAL, message));
            return Ok(());
        }
    };
    start(
        Arc::clone(&hub.host_update),
        plan,
        request_id,
        ctx.out_tx.clone(),
    );
    Ok(())
}

/// The checks, then the guard: the plan when the update may start. Blocking
/// work.
fn begin(hub: &Hub) -> Result<Plan, Refusal> {
    let plan = host_check(hub)?;
    if !hub.host_update.try_begin() {
        return Err((BUSY, ANOTHER_UPDATE));
    }
    Ok(plan)
}

/// Starts the update on its own thread, and a task that sends its one answer
/// on `out_tx`. The guard is already held.
fn start(
    update: Arc<HostUpdate>,
    plan: Plan,
    request_id: String,
    out_tx: mpsc::UnboundedSender<DaemonMessage>,
) {
    let (answer_tx, answer_rx) = oneshot::channel();
    update.update_in_flight.store(true, Ordering::SeqCst);
    let worker = Arc::clone(&update);
    let spawned = std::thread::Builder::new()
        .name("remote-update".to_owned())
        .spawn(move || {
            let outcome = run(&worker, &plan, || true);
            if outcome.is_err() {
                worker.finish();
            }
            // A dropped receiver is a daemon shutting down.
            let _ = answer_tx.send(outcome);
        });
    if let Err(err) = spawned {
        warn!(error = %err, "remote update: the update thread didn't start");
        update.finish();
        let message = "remote update: the update thread didn't start".to_owned();
        send(&out_tx, refusal_reply(request_id, INTERNAL, message));
        return;
    }
    tokio::spawn(async move {
        let reply = match answer_rx.await {
            Ok(Ok(version)) => {
                info!(%request_id, %version, "remote update: installer started");
                DaemonMessage::RemoteUpdateStarted {
                    request_id,
                    version: version.to_string(),
                }
            }
            Ok(Err((code, message))) => refusal_reply(request_id, code, message.to_owned()),
            Err(_) => {
                update.finish();
                let message = "remote update: the update ended without an answer".to_owned();
                refusal_reply(request_id, INTERNAL, message)
            }
        };
        send(&out_tx, reply);
    });
}

/// The update, on its own thread: the mutex, old downloads cleared, the
/// download, `go`'s word, the launch. The version started, or why not.
fn run(
    update: &HostUpdate,
    plan: &Plan,
    go: impl FnOnce() -> bool,
) -> Result<semver::Version, Refusal> {
    let seams = &update.seams;
    let hold = take_lock(seams.lock.as_ref())?;
    update_core::clean_old(&plan.updates_root, None);
    let ready = download(seams.source.as_ref(), plan)?;
    if !go() {
        info!(version = %ready.version, "remote update: cancelled on the host");
        return Err((UPDATE_CANCELED, CANCELED));
    }
    launch(seams.launcher.as_ref(), &ready)?;
    // The installer's `apply-update` takes the mutex next; the verified file
    // stays open until the installer has started.
    drop(hold);
    let version = ready.version.clone();
    drop(ready);
    Ok(version)
}

/// The update mutex's hold, or why it can't be taken.
fn take_lock(lock: &dyn UpdateLock) -> Result<Box<dyn std::any::Any>, Refusal> {
    match lock.try_lock() {
        Ok(Some(hold)) => Ok(hold),
        Ok(None) => Err((BUSY, ANOTHER_UPDATE)),
        Err(err) => {
            warn!(error = %format!("{err:#}"), "remote update: taking the update lock failed");
            Err((UPDATE_REFUSED, NO_LOCK))
        }
    }
}

/// The verified release newer than this host's, or why there is none. A
/// failure's text stays in the log: it can name paths.
fn download(source: &dyn UpdateSource, plan: &Plan) -> Result<update_core::Ready, Refusal> {
    match source.fetch(&plan.current, &plan.updates_root) {
        Ok(Some(ready)) => Ok(ready),
        Ok(None) => Err((UPDATE_REFUSED, ALREADY_NEWEST)),
        Err(err) => {
            warn!(error = %format!("{err:#}"), "remote update: the download failed");
            Err((UPDATE_REFUSED, DOWNLOAD_FAILED))
        }
    }
}

/// Starts `ready`'s installer with [`INSTALLER_ARGS`].
fn launch(launcher: &dyn Launch, ready: &update_core::Ready) -> Result<(), Refusal> {
    let args: Vec<OsString> = INSTALLER_ARGS.iter().map(OsString::from).collect();
    launcher.launch(&ready.path, &args).map_err(|err| {
        warn!(error = %format!("{err:#}"), "remote update: the installer didn't start");
        (UPDATE_REFUSED, NO_INSTALLER)
    })
}

/// An `Error` answering `request_id` with `code` and `message`.
fn refusal_reply(request_id: String, code: &str, message: String) -> DaemonMessage {
    info!(%request_id, code, "remote update: refused");
    DaemonMessage::Error {
        message,
        request_id: Some(request_id),
        code: Some(code.to_owned()),
    }
}

fn send(out_tx: &mpsc::UnboundedSender<DaemonMessage>, reply: DaemonMessage) {
    // A closed outbox is a connection already ending.
    let _ = out_tx.send(reply);
}

/// The first check that refuses this host's update; the plan when every
/// check passes.
fn host_check(hub: &Hub) -> Result<Plan, Refusal> {
    if let Some(refusal) = platform_refusal()
        .or_else(dev_build_refusal)
        .or_else(|| elevated_refusal(hub))
    {
        return Err(refusal);
    }
    let current =
        semver::Version::parse(protocol::BUILD_VERSION).map_err(|_| (UPDATE_REFUSED, DEV_BUILD))?;
    let update = &hub.host_update;
    let Some(install) = update.install.install_folder() else {
        return Err((UPDATE_REFUSED, NO_INSTALL));
    };
    let updates_root = (update.seams.updates_dir)().map_err(|err| {
        warn!(error = %format!("{err:#}"), "remote update: no updates folder");
        (UPDATE_REFUSED, NO_UPDATES_DIR)
    })?;
    if let Some(refusal) = own_install_refusal(&install, hub.dirs.bin_templates.as_deref())
        .or_else(|| process_refusal(&install, &update.processes.running(), &updates_root))
    {
        return Err(refusal);
    }
    Ok(Plan {
        current,
        updates_root,
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
/// folder or from under `updates`; refused while the app, or anything else,
/// runs from `install` itself.
fn process_refusal(install: &Path, running: &[RunningExe], updates: &Path) -> Option<Refusal> {
    let own_pid = std::process::id();
    let others = || running.iter().filter(move |process| process.pid != own_pid);
    let staging = install.join(STAGING_DIR);
    let updating = others().any(|process| {
        crate::path_is_under(&process.exe, &staging) || crate::path_is_under(&process.exe, updates)
    });
    if updating {
        return Some((BUSY, ANOTHER_UPDATE));
    }
    let folder = crate::normalize_process_path(install);
    let in_install: Vec<&RunningExe> = others()
        .filter(|process| {
            process
                .exe
                .parent()
                .is_some_and(|parent| crate::normalize_process_path(parent) == folder)
        })
        .collect();
    if in_install.iter().any(|process| is_client(&process.exe)) {
        return Some((UPDATE_REFUSED, APP_OPEN));
    }
    (!in_install.is_empty()).then_some((UPDATE_REFUSED, SOMETHING_RUNS))
}

/// Whether `exe` is the app, which would hold up the installer until closed.
fn is_client(exe: &Path) -> bool {
    exe.file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(CLIENT_EXE))
}

/// The installer's named update mutex.
struct NamedMutex;

impl UpdateLock for NamedMutex {
    fn try_lock(&self) -> anyhow::Result<Option<Box<dyn std::any::Any>>> {
        update_core::try_lock(update_core::UPDATE_MUTEX)
    }
}

/// The published releases' feed.
struct ReleaseFeed;

impl UpdateSource for ReleaseFeed {
    fn fetch(
        &self,
        current: &semver::Version,
        root: &Path,
    ) -> anyhow::Result<Option<update_core::Ready>> {
        update_core::check_and_download(current, root, &update_core::feed_base())
    }
}

/// A launch detached from this daemon, as the app starts its installer.
struct DetachedLaunch;

impl Launch for DetachedLaunch {
    #[cfg(any(windows, target_os = "macos"))]
    fn launch(&self, program: &Path, args: &[OsString]) -> anyhow::Result<()> {
        update_core::launch_detached(program, args)
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    fn launch(&self, program: &Path, _args: &[OsString]) -> anyhow::Result<()> {
        anyhow::bail!(
            "no installer launch on this system for {}",
            program.display()
        )
    }
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
    use super::super::test_support::{HANG_GUARD, ScratchDir, dispatch_on, test_hub_in};
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use tokio::sync::mpsc;

    /// The folder under the scratch dir the hub's daemon runs from.
    const OWN: &str = "install";

    /// Text the failing download's error carries, which no reply may.
    const DOWNLOAD_ERROR: &str = r"C:\private\updates\0.99.0-x\setup.exe: connection reset";

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

    /// The update lock: free, or held by another process.
    struct FakeLock {
        held: bool,
    }

    impl UpdateLock for FakeLock {
        fn try_lock(&self) -> anyhow::Result<Option<Box<dyn std::any::Any>>> {
            Ok((!self.held).then(|| Box::new(()) as Box<dyn std::any::Any>))
        }
    }

    /// What the release feed has.
    #[derive(Clone, Copy)]
    enum Feed {
        /// Nothing newer than this host.
        Newest,
        /// A download that fails.
        Failing,
        /// Release `0.99.0`, downloaded and verified.
        Release,
    }

    struct FakeSource {
        feed: Feed,
        /// The verified installer a release hands back.
        installer: PathBuf,
    }

    impl UpdateSource for FakeSource {
        fn fetch(
            &self,
            _current: &semver::Version,
            _root: &Path,
        ) -> anyhow::Result<Option<update_core::Ready>> {
            match self.feed {
                Feed::Newest => Ok(None),
                Feed::Failing => anyhow::bail!("{DOWNLOAD_ERROR}"),
                Feed::Release => Ok(Some(update_core::Ready {
                    version: semver::Version::new(0, 99, 0),
                    path: self.installer.clone(),
                    file: std::fs::File::open(&self.installer)?,
                    notes: String::new(),
                })),
            }
        }
    }

    /// Every launch: the program and its arguments.
    type Launches = Arc<std::sync::Mutex<Vec<(PathBuf, Vec<OsString>)>>>;

    struct FakeLauncher {
        fails: bool,
        seen: Launches,
    }

    impl Launch for FakeLauncher {
        fn launch(&self, program: &Path, args: &[OsString]) -> anyhow::Result<()> {
            lock(&self.seen).push((program.to_path_buf(), args.to_vec()));
            if self.fails {
                anyhow::bail!("the installer would not start");
            }
            Ok(())
        }
    }

    /// How the fakes behind the checks answer.
    #[derive(Clone, Copy)]
    struct Setup {
        lock_held: bool,
        feed: Feed,
        launch_fails: bool,
    }

    const SETUP: Setup = Setup {
        lock_held: false,
        feed: Feed::Release,
        launch_fails: false,
    };

    /// A hub over the fakes, and what its launcher saw.
    struct Rig {
        hub: Hub,
        _scratch: ScratchDir,
        launches: Launches,
        installer: PathBuf,
    }

    /// A hub whose daemon runs from `<scratch>/install`, the locator finding
    /// `<scratch>/<install>`, other processes running the exes at
    /// `<scratch>/<running>`, and the update seams answering as `setup` says.
    fn rig(tag: &str, install: Option<&str>, running: &[&str], setup: Setup) -> Rig {
        let scratch = ScratchDir::new(tag);
        let root = scratch.path().to_path_buf();
        let downloads = root.join("downloads");
        std::fs::create_dir_all(&downloads).unwrap_or_else(|err| panic!("downloads folder: {err}"));
        let installer = downloads.join("rustling-tulip-0.99.0-setup.exe");
        std::fs::write(&installer, b"installer").unwrap_or_else(|err| panic!("installer: {err}"));
        let (mut hub, scratch) = test_hub_in(scratch, Some(root.join(OWN)));
        let other = std::process::id().wrapping_add(1);
        let processes = running
            .iter()
            .map(|exe| RunningExe {
                pid: other,
                exe: root.join(exe),
            })
            .collect();
        let launches = Launches::default();
        let updates_root = root.join("update-root");
        let seams = Seams {
            lock: Box::new(FakeLock {
                held: setup.lock_held,
            }),
            source: Box::new(FakeSource {
                feed: setup.feed,
                installer: installer.clone(),
            }),
            launcher: Box::new(FakeLauncher {
                fails: setup.launch_fails,
                seen: Arc::clone(&launches),
            }),
            updates_dir: Box::new(move || Ok(updates_root.clone())),
        };
        hub.host_update = Arc::new(HostUpdate::new(
            Box::new(FakeInstall(install.map(|folder| root.join(folder)))),
            Box::new(FakeProcesses(processes)),
            seams,
        ));
        Rig {
            hub,
            _scratch: scratch,
            launches,
            installer,
        }
    }

    /// A rig whose every check passes, its seams answering as `setup` says.
    fn passing(tag: &str, setup: Setup) -> Rig {
        rig(tag, Some(OWN), &["elsewhere/rustling-tulipd.exe"], setup)
    }

    fn request() -> ClientMessage {
        ClientMessage::RemoteUpdate {
            request_id: "u1".to_owned(),
        }
    }

    /// Sends the request and returns its one reply, failing when a second
    /// comes or none does.
    async fn ask(hub: &Hub) -> DaemonMessage {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        dispatch_on(hub, request(), 1, &out_tx).await;
        drop(out_tx);
        let reply = tokio::time::timeout(HANG_GUARD, out_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("hung: no reply to the update request"))
            .unwrap_or_else(|| panic!("the connection closed with no reply"));
        let more = tokio::time::timeout(HANG_GUARD, out_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("hung: the reply's sender was never dropped"));
        assert!(more.is_none(), "a second reply: {more:?}");
        reply
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

    /// Whether this daemon's update guard is set.
    fn guard_set(hub: &Hub) -> bool {
        *lock(&hub.host_update.in_flight)
    }

    /// Whether the idle-exit hold-off for an update is set.
    fn held_off(hub: &Hub) -> bool {
        hub.host_update.update_in_flight.load(Ordering::SeqCst)
    }

    #[tokio::test]
    async fn remote_update_refuses_a_host_with_no_install() {
        let rig = rig("update-no-install", None, &[], SETUP);

        let reply = refusal(ask(&rig.hub).await);

        assert_eq!(reply, expected(UPDATE_REFUSED, NO_INSTALL));
    }

    #[tokio::test]
    async fn remote_update_refuses_an_install_that_is_not_this_daemons() {
        let rig = rig("update-not-own", Some("elsewhere"), &[], SETUP);

        let reply = refusal(ask(&rig.hub).await);

        assert_eq!(reply, expected(UPDATE_REFUSED, NOT_OWN_INSTALL));
    }

    #[tokio::test]
    async fn remote_update_is_busy_while_a_process_runs_from_staging() {
        let rig = rig(
            "update-staging",
            Some(OWN),
            &["install/.staging/rustling-tulipd.exe"],
            SETUP,
        );

        let reply = refusal(ask(&rig.hub).await);

        assert_eq!(reply, expected(BUSY, ANOTHER_UPDATE));
    }

    #[tokio::test]
    async fn remote_update_refuses_while_a_bridge_runs_from_the_install() {
        let rig = rig(
            "update-bridge",
            Some(OWN),
            &["install/rustling-tulipd.exe"],
            SETUP,
        );

        let reply = refusal(ask(&rig.hub).await);

        assert_eq!(reply, expected(UPDATE_REFUSED, SOMETHING_RUNS));
    }

    #[tokio::test]
    async fn remote_update_is_busy_while_another_update_runs() {
        let rig = passing("update-in-flight", SETUP);
        *lock(&rig.hub.host_update.in_flight) = true;

        let reply = refusal(ask(&rig.hub).await);

        assert_eq!(reply, expected(BUSY, ANOTHER_UPDATE));
        assert!(guard_set(&rig.hub), "the running update keeps its guard");
        assert!(lock(&rig.launches).is_empty(), "nothing launched");
    }

    #[tokio::test]
    async fn remote_update_refuses_while_the_app_runs_from_the_install() {
        let rig = rig(
            "update-app-open",
            Some(OWN),
            &[
                "install/rustling-tulip-native.exe",
                "elsewhere/rustling-tulipd.exe",
            ],
            SETUP,
        );

        let reply = refusal(ask(&rig.hub).await);

        assert_eq!(reply, expected(UPDATE_REFUSED, APP_OPEN));
        assert!(
            !guard_set(&rig.hub),
            "a refusal before the guard takes none"
        );
        assert!(lock(&rig.launches).is_empty(), "nothing launched");
    }

    #[tokio::test]
    async fn remote_update_launches_the_verified_installer_and_answers_started() {
        let rig = passing("update-started", SETUP);

        let reply = ask(&rig.hub).await;

        let DaemonMessage::RemoteUpdateStarted {
            request_id,
            version,
        } = reply
        else {
            panic!("expected the update to start, got {reply:?}");
        };
        assert_eq!((request_id.as_str(), version.as_str()), ("u1", "0.99.0"));
        let args: Vec<OsString> = ["/S", "/UPDATE", "/RESTARTDAEMON"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(*lock(&rig.launches), vec![(rig.installer.clone(), args)]);
        assert!(guard_set(&rig.hub), "a launched update keeps its guard");
        assert!(held_off(&rig.hub), "a launched update holds off idle exit");
    }

    #[tokio::test]
    async fn remote_update_is_busy_when_the_update_lock_is_held() {
        let rig = passing(
            "update-lock-held",
            Setup {
                lock_held: true,
                ..SETUP
            },
        );

        let reply = refusal(ask(&rig.hub).await);

        assert_eq!(reply, expected(BUSY, ANOTHER_UPDATE));
        assert!(!guard_set(&rig.hub), "the guard is released");
        assert!(!held_off(&rig.hub), "idle exit is no longer held off");
        assert!(lock(&rig.launches).is_empty(), "nothing launched");
    }

    #[tokio::test]
    async fn remote_update_refuses_when_the_host_is_on_the_newest_release() {
        let rig = passing(
            "update-newest",
            Setup {
                feed: Feed::Newest,
                ..SETUP
            },
        );

        let reply = refusal(ask(&rig.hub).await);

        assert_eq!(reply, expected(UPDATE_REFUSED, ALREADY_NEWEST));
        assert!(!guard_set(&rig.hub), "the guard is released");
        assert!(!held_off(&rig.hub), "idle exit is no longer held off");
        assert!(lock(&rig.launches).is_empty(), "nothing launched");
    }

    #[tokio::test]
    async fn remote_update_refuses_when_the_download_fails() {
        let rig = passing(
            "update-download-fails",
            Setup {
                feed: Feed::Failing,
                ..SETUP
            },
        );

        let reply = refusal(ask(&rig.hub).await);

        assert_eq!(reply, expected(UPDATE_REFUSED, DOWNLOAD_FAILED));
        assert!(
            !reply.1.contains(DOWNLOAD_ERROR),
            "the error's text stays on the host: {}",
            reply.1
        );
        assert!(!guard_set(&rig.hub), "the guard is released");
        assert!(lock(&rig.launches).is_empty(), "nothing launched");
    }

    #[tokio::test]
    async fn remote_update_refuses_when_the_installer_does_not_start() {
        let rig = passing(
            "update-launch-fails",
            Setup {
                launch_fails: true,
                ..SETUP
            },
        );

        let reply = refusal(ask(&rig.hub).await);

        assert_eq!(reply, expected(UPDATE_REFUSED, NO_INSTALLER));
        assert!(!guard_set(&rig.hub), "the guard is released");
        assert!(!held_off(&rig.hub), "idle exit is no longer held off");
    }

    #[tokio::test]
    async fn remote_update_answers_once_per_request() {
        let outcomes = [
            ("update-once-started", SETUP),
            (
                "update-once-busy",
                Setup {
                    lock_held: true,
                    ..SETUP
                },
            ),
            (
                "update-once-newest",
                Setup {
                    feed: Feed::Newest,
                    ..SETUP
                },
            ),
            (
                "update-once-download",
                Setup {
                    feed: Feed::Failing,
                    ..SETUP
                },
            ),
            (
                "update-once-launch",
                Setup {
                    launch_fails: true,
                    ..SETUP
                },
            ),
        ];
        for (tag, setup) in outcomes {
            let rig = passing(tag, setup);

            // `ask` fails on a second reply, and on none.
            let _reply = ask(&rig.hub).await;
        }
        let refused = rig(
            "update-once-app",
            Some(OWN),
            &["install/rustling-tulip-native.exe"],
            SETUP,
        );
        let _reply = ask(&refused.hub).await;
    }
}
