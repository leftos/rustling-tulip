//! The daemon's `apply-update` subcommand: put a staged release in place.
//!
//! The installer extracts every file it ships into `<install>\.staging` and
//! runs the staged daemon as `rustling-tulipd apply-update --from <staging>
//! --to <install> [--old-version <v>] [--update] [--relaunch]`. This takes the
//! update lock, waits for the client to exit, stops the running daemon before
//! any file is written, refuses while anything still runs from the install
//! folder, swaps each file in through `<name>.new` and `<name>.old` (the client
//! last, every swap rolled back on a failure), records the outcome in
//! `last-result.json` and, when asked, starts the client again.
//!
//! Like `shutdown`, it starts no tracing, takes no instance lock, rotates no log
//! and creates no folder but the one the result file lives in.

use anyhow::Context as _;
use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{Duration, Instant};

/// The client's file name in the install folder; it is swapped in last and is
/// what `--relaunch` starts.
#[cfg(windows)]
pub const CLIENT_EXE: &str = "rustling-tulip-native.exe";
#[cfg(not(windows))]
pub const CLIENT_EXE: &str = "rustling-tulip-native";

/// The named mutex every updater and installer holds while it runs.
#[cfg(windows)]
pub const UPDATE_MUTEX: &str = r"Local\rustling-tulip-update";

/// This build's version, the one an update installs.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long to wait for the client to exit before checking what still runs.
const CLIENT_WAIT: Duration = Duration::from_secs(30);

/// How often the client wait looks at the process list again.
const CLIENT_POLL: Duration = Duration::from_millis(250);

/// The daemon stop must finish inside this, as for `shutdown`.
const STOP_TIMEOUT: Duration = Duration::from_secs(25);

/// Exit codes: a bad command line or unresolvable folders, a failed daemon
/// stop, something still running from the install folder, a held update lock,
/// and a failed swap.
const USAGE_CODE: u8 = 1;
const STOP_FAILED_CODE: u8 = 2;
const IN_USE_CODE: u8 = 3;
const LOCKED_CODE: u8 = 4;
const SWAP_FAILED_CODE: u8 = 5;

/// True when the daemon's arguments ask for `apply-update`: the first argument
/// is exactly `apply-update`.
pub fn requested(args: impl IntoIterator<Item = OsString>) -> bool {
    args.into_iter()
        .next()
        .is_some_and(|arg| arg == "apply-update")
}

/// True when the daemon's arguments ask for its version: the first argument is
/// exactly `--version`.
pub fn version_requested(args: impl IntoIterator<Item = OsString>) -> bool {
    args.into_iter()
        .next()
        .is_some_and(|arg| arg == "--version")
}

/// Write `rustling-tulipd <version>` to `out`.
///
/// # Errors
///
/// When the line cannot be written.
pub fn write_version(out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out, "rustling-tulipd {VERSION}")
}

/// What one `apply-update` run was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The folder holding the staged files.
    pub from: PathBuf,
    /// The install folder the staged files replace their copies in.
    pub to: PathBuf,
    /// The version being replaced, as the installer read it; `None` on a first
    /// install.
    pub old_version: Option<String>,
    /// Start the client once the run ends, whatever its outcome.
    pub relaunch: bool,
}

/// Parse the arguments after `apply-update`.
///
/// # Errors
///
/// An unknown argument, a flag missing its value, an `--old-version` that is
/// not UTF-8, or a missing `--from` or `--to`.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> anyhow::Result<Request> {
    let mut args = args.into_iter();
    let (mut from, mut to, mut old_version, mut relaunch) = (None, None, None, false);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--from") => from = Some(PathBuf::from(value(&mut args, "--from")?)),
            Some("--to") => to = Some(PathBuf::from(value(&mut args, "--to")?)),
            Some("--old-version") => {
                let version = value(&mut args, "--old-version")?;
                let version = version
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("--old-version is not UTF-8"))?;
                old_version = Some(version);
            }
            Some("--relaunch") => relaunch = true,
            // The installer's update mode changes only what it writes to the
            // registry; the files are applied the same way.
            Some("--update") => {}
            _ => anyhow::bail!("unknown argument {}", arg.display()),
        }
    }
    Ok(Request {
        from: from.context("--from <staging folder> is required")?,
        to: to.context("--to <install folder> is required")?,
        old_version,
        relaunch,
    })
}

fn value(args: &mut impl Iterator<Item = OsString>, flag: &str) -> anyhow::Result<OsString> {
    args.next().with_context(|| format!("{flag} needs a value"))
}

/// One running process and the exe it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningExe {
    pub pid: u32,
    pub exe: PathBuf,
}

/// The running processes whose exe path could be read.
pub trait ProcessList {
    fn running(&self) -> Vec<RunningExe>;
}

/// Stops the running daemon. `Ok` when it stopped or none was running; `Err`
/// with the reason otherwise.
pub trait DaemonStop {
    fn stop(&self) -> Pin<Box<dyn Future<Output = Result<(), String>> + '_>>;
}

/// Starts a program detached from this process.
pub trait Launcher {
    /// # Errors
    ///
    /// When the program could not be started.
    fn launch(&self, program: &Path, args: &[OsString]) -> anyhow::Result<()>;
}

/// Whatever keeps the update lock held until it is dropped.
pub type LockHold = Box<dyn std::any::Any>;

/// The lock that keeps two updates from running at once.
pub trait UpdateLock {
    /// `Ok(None)` when another process holds it.
    ///
    /// # Errors
    ///
    /// When the lock could not be created or waited on.
    fn try_lock(&self) -> anyhow::Result<Option<LockHold>>;
}

/// The outside world one run touches, so tests replace each part.
pub struct Hooks<'a> {
    pub processes: &'a dyn ProcessList,
    pub daemon: &'a dyn DaemonStop,
    pub launcher: &'a dyn Launcher,
    pub lock: &'a dyn UpdateLock,
    /// How long to wait for the client to exit.
    pub client_wait: Duration,
}

/// Apply the update `request` describes, writing one line per event to `out`,
/// record the outcome in `result_file` and return the exit code: `0` applied,
/// `2` the daemon did not stop, `3` something still runs from the install
/// folder, `4` another update holds the lock, `5` a file could not be swapped
/// (every swap was rolled back).
pub async fn run(
    request: &Request,
    result_file: &Path,
    hooks: &Hooks<'_>,
    out: &mut impl Write,
) -> u8 {
    let hold = match hooks.lock.try_lock() {
        Ok(Some(hold)) => hold,
        Ok(None) => {
            say(out, "another update is running");
            return LOCKED_CODE;
        }
        Err(err) => {
            say(out, &format!("could not take the update lock: {err:#}"));
            return LOCKED_CODE;
        }
    };
    let outcome = apply(request, hooks, out).await;
    say(out, &outcome.message);
    if let Err(err) = write_result(result_file, request, &outcome) {
        say(
            out,
            &format!("could not write {}: {err:#}", result_file.display()),
        );
    }
    if request.relaunch {
        relaunch(request, hooks.launcher, out);
    }
    drop(hold);
    outcome.code
}

/// Parse the arguments after `apply-update`, resolve the config and updates
/// folders, and [`run`] against this machine: its processes, its daemon, a
/// detached launch and the update mutex.
pub async fn run_from_args(args: impl IntoIterator<Item = OsString>, out: &mut impl Write) -> u8 {
    let resolved = parse(args).and_then(|request| {
        let config_dir = daemon_client::config_dir()?;
        let result_file = daemon_client::updates_dir()?.join("last-result.json");
        Ok((request, config_dir, result_file))
    });
    let (request, config_dir, result_file) = match resolved {
        Ok(resolved) => resolved,
        Err(err) => {
            say(out, &format!("{err:#}"));
            return USAGE_CODE;
        }
    };
    #[cfg(windows)]
    let lock = NamedMutex {
        name: UPDATE_MUTEX.to_string(),
    };
    #[cfg(not(windows))]
    let lock = NoLock;
    let hooks = Hooks {
        processes: &SystemProcesses,
        daemon: &ConfigDirDaemon(config_dir),
        launcher: &DetachedLauncher,
        lock: &lock,
        client_wait: CLIENT_WAIT,
    };
    run(&request, &result_file, &hooks, out).await
}

/// How one run ended: its exit code, whether the update is in place, and the
/// line that says what happened.
struct Outcome {
    code: u8,
    ok: bool,
    message: String,
}

impl Outcome {
    fn failed(code: u8, message: String) -> Self {
        Self {
            code,
            ok: false,
            message,
        }
    }
}

/// Write one `apply-update: <message>` line. A line that cannot be written is
/// dropped: the run's outcome is in its exit code and the result file, and
/// stopping half-way through a swap over a closed pipe would be worse.
fn say(out: &mut impl Write, message: &str) {
    let _ = writeln!(out, "apply-update: {message}");
}

async fn apply(request: &Request, hooks: &Hooks<'_>, out: &mut impl Write) -> Outcome {
    wait_for_clients(&request.to, hooks).await;
    if let Err(reason) = hooks.daemon.stop().await {
        return Outcome::failed(
            STOP_FAILED_CODE,
            format!("could not stop the daemon: {reason}; nothing was changed"),
        );
    }
    if let Some(busy) = running_from(&request.to, hooks.processes).first() {
        let name = busy.exe.file_name().unwrap_or(busy.exe.as_os_str());
        return Outcome::failed(
            IN_USE_CODE,
            format!(
                "{} is still running from the install folder (pid {}); nothing was changed",
                name.display(),
                busy.pid
            ),
        );
    }
    match swap_all(&request.from, &request.to) {
        Ok(swapped) => {
            clean_up(&request.from, &swapped, out);
            Outcome {
                code: 0,
                ok: true,
                message: format!("updated to {VERSION}"),
            }
        }
        Err(message) => Outcome::failed(SWAP_FAILED_CODE, message),
    }
}

/// Wait until no client runs from `to`, or until the hooks' client wait ends.
async fn wait_for_clients(to: &Path, hooks: &Hooks<'_>) {
    let deadline = Instant::now() + hooks.client_wait;
    while running_from(to, hooks.processes)
        .iter()
        .any(|process| is_client(process.exe.file_name()))
        && Instant::now() < deadline
    {
        tokio::time::sleep(CLIENT_POLL).await;
    }
}

/// The processes, this one aside, whose exe lies directly in `folder`.
fn running_from(folder: &Path, processes: &dyn ProcessList) -> Vec<RunningExe> {
    let folder = crate::normalize_process_path(folder);
    let own_pid = std::process::id();
    processes
        .running()
        .into_iter()
        .filter(|process| process.pid != own_pid)
        .filter(|process| {
            process
                .exe
                .parent()
                .is_some_and(|parent| crate::normalize_process_path(parent) == folder)
        })
        .collect()
}

fn is_client(name: Option<&OsStr>) -> bool {
    name.is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(CLIENT_EXE))
}

/// One file put in place: the installed path, and where its old copy went
/// when there was one.
struct Swap {
    target: PathBuf,
    old: Option<PathBuf>,
}

/// A swap that failed, with the part of it that needs rolling back.
struct SwapError {
    err: std::io::Error,
    partial: Option<Swap>,
}

/// Swap every file staged in `from` into `to`, the client last. On a failure,
/// every swap made is rolled back and every `.new` file deleted, and the error
/// is the line to report.
fn swap_all(from: &Path, to: &Path) -> Result<Vec<Swap>, String> {
    let names = staged_names(from).map_err(|err| {
        format!(
            "could not read {}: {err}; nothing was changed",
            from.display()
        )
    })?;
    let names = swap_order(names);
    let mut done = Vec::new();
    for name in &names {
        match swap_one(from, to, name) {
            Ok(swap) => done.push(swap),
            Err(SwapError { err, partial }) => {
                done.extend(partial);
                let mut stuck = roll_back(&done);
                stuck.extend(remove_new_files(to, &names));
                let name = name.display();
                return Err(if stuck.is_empty() {
                    format!("could not replace {name}: {err}; every file was put back")
                } else {
                    format!(
                        "could not replace {name}: {err}; could not put back {}",
                        stuck.join(", ")
                    )
                });
            }
        }
    }
    Ok(done)
}

/// The names of the regular files directly in `from`.
fn staged_names(from: &Path) -> std::io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            names.push(entry.file_name());
        }
    }
    Ok(names)
}

/// `names` in the order they are swapped: by name, the client last, so a
/// failure before it leaves the old client whole to relaunch.
fn swap_order(mut names: Vec<OsString>) -> Vec<OsString> {
    names.sort_by(|a, b| (is_client(Some(a)), a).cmp(&(is_client(Some(b)), b)));
    names
}

/// Copy `from/name` to `to/name.new`, move `to/name` aside to `name.old` when
/// it exists, and rename `name.new` into place.
fn swap_one(from: &Path, to: &Path, name: &OsStr) -> Result<Swap, SwapError> {
    let target = to.join(name);
    let new = sibling(&target, "new");
    let old = sibling(&target, "old");
    std::fs::copy(from.join(name), &new).map_err(|err| SwapError { err, partial: None })?;
    let had_old = match std::fs::rename(&target, &old) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(SwapError { err, partial: None }),
    };
    let swap = Swap {
        target,
        old: had_old.then_some(old),
    };
    match std::fs::rename(&new, &swap.target) {
        Ok(()) => Ok(swap),
        Err(err) => Err(SwapError {
            err,
            partial: Some(swap),
        }),
    }
}

/// `path` with `.suffix` added to its file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

/// Undo `done`, newest first: each old copy goes back into place, and a file
/// that had none is deleted. Returns the paths that could not be restored.
fn roll_back(done: &[Swap]) -> Vec<String> {
    let mut stuck = Vec::new();
    for swap in done.iter().rev() {
        let restored = match &swap.old {
            Some(old) => std::fs::rename(old, &swap.target),
            None => match std::fs::remove_file(&swap.target) {
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            },
        };
        if let Err(err) = restored {
            stuck.push(format!("{} ({err})", swap.target.display()));
        }
    }
    stuck
}

/// Delete every `<name>.new` in `to`; returns those that could not be.
fn remove_new_files(to: &Path, names: &[OsString]) -> Vec<String> {
    let mut stuck = Vec::new();
    for name in names {
        let new = sibling(&to.join(name), "new");
        match std::fs::remove_file(&new) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => stuck.push(format!("{} ({err})", new.display())),
        }
    }
    stuck
}

/// Remove the staging folder and the old copies this run moved aside. What
/// cannot be removed is reported and left: the installer removes the staging
/// folder after this exits (this process's own exe is in it), and the client
/// deletes leftover `.old` files at launch.
fn clean_up(from: &Path, swapped: &[Swap], out: &mut impl Write) {
    if let Err(err) = std::fs::remove_dir_all(from) {
        say(out, &format!("could not remove {}: {err}", from.display()));
    }
    for old in swapped.iter().filter_map(|swap| swap.old.as_ref()) {
        if let Err(err) = std::fs::remove_file(old) {
            say(out, &format!("could not remove {}: {err}", old.display()));
        }
    }
}

/// Write `{"from", "to", "ok", "message"}` to `path` atomically, creating its
/// folder first.
fn write_result(path: &Path, request: &Request, outcome: &Outcome) -> anyhow::Result<()> {
    let record = serde_json::json!({
        "from": request.old_version,
        "to": VERSION,
        "ok": outcome.ok,
        "message": outcome.message,
    });
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder)
            .with_context(|| format!("creating {}", folder.display()))?;
    }
    crate::orphan::write_atomic(path, &serde_json::to_vec(&record)?)
}

/// Start the installed client once, telling it which version it replaced.
fn relaunch(request: &Request, launcher: &dyn Launcher, out: &mut impl Write) {
    let client = request.to.join(CLIENT_EXE);
    let args: Vec<OsString> = request
        .old_version
        .iter()
        .flat_map(|version| [OsString::from("--updated-from"), OsString::from(version)])
        .collect();
    if let Err(err) = launcher.launch(&client, &args) {
        say(
            out,
            &format!("could not start {}: {err:#}", client.display()),
        );
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

/// The daemon the config dir's `daemon.json` names, stopped the way
/// `shutdown` stops it.
struct ConfigDirDaemon(PathBuf);

impl DaemonStop for ConfigDirDaemon {
    fn stop(&self) -> Pin<Box<dyn Future<Output = Result<(), String>> + '_>> {
        Box::pin(async move {
            use daemon_client::Shutdown;
            match tokio::time::timeout(STOP_TIMEOUT, daemon_client::shutdown_in(&self.0)).await {
                Ok(Ok(Shutdown::Stopped | Shutdown::NoDaemon | Shutdown::Stale)) => Ok(()),
                Ok(Ok(Shutdown::NotProven(_))) => {
                    Err("its listener did not prove it holds its token".to_string())
                }
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(_) => Err(format!(
                    "it did not stop within {} s",
                    STOP_TIMEOUT.as_secs()
                )),
            }
        })
    }
}

/// Starts a program with no console and no std handles, breaking away from the
/// installer's job when the job allows it.
struct DetachedLauncher;

impl Launcher for DetachedLauncher {
    fn launch(&self, program: &Path, args: &[OsString]) -> anyhow::Result<()> {
        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
            /// What `CreateProcess` returns when the caller's job forbids breakaway.
            const ERROR_ACCESS_DENIED: i32 = 5;
            command.creation_flags(DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB);
            match command.spawn() {
                Ok(_) => return Ok(()),
                Err(err) if err.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                    command.creation_flags(DETACHED_PROCESS);
                }
                Err(err) => {
                    return Err(err).with_context(|| format!("starting {}", program.display()));
                }
            }
        }
        command
            .spawn()
            .with_context(|| format!("starting {}", program.display()))?;
        Ok(())
    }
}

/// No update lock where there is no installer.
#[cfg(not(windows))]
struct NoLock;

#[cfg(not(windows))]
impl UpdateLock for NoLock {
    fn try_lock(&self) -> anyhow::Result<Option<LockHold>> {
        Ok(Some(Box::new(())))
    }
}

/// A named Windows mutex, owned without waiting or not at all.
#[cfg(windows)]
pub struct NamedMutex {
    pub name: String,
}

#[cfg(windows)]
impl UpdateLock for NamedMutex {
    fn try_lock(&self) -> anyhow::Result<Option<LockHold>> {
        use windows::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
        use windows::core::PCWSTR;

        let name: Vec<u16> = self.name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `name` is NUL-terminated UTF-16 that outlives the call.
        let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }
            .with_context(|| format!("creating the mutex {}", self.name))?;
        let handle = MutexHandle(handle);
        // SAFETY: `handle.0` is the live mutex handle just created.
        let waited = unsafe { WaitForSingleObject(handle.0, 0) };
        if waited == WAIT_OBJECT_0 || waited == WAIT_ABANDONED {
            Ok(Some(Box::new(OwnedMutex(handle))))
        } else if waited == WAIT_TIMEOUT {
            Ok(None)
        } else {
            anyhow::bail!("waiting on the mutex {} returned {}", self.name, waited.0)
        }
    }
}

/// A mutex handle, closed on drop.
#[cfg(windows)]
struct MutexHandle(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for MutexHandle {
    fn drop(&mut self) {
        // A failed close has no one to tell: this subcommand keeps no log, and
        // the handle closes when the process exits a moment later anyway.
        // SAFETY: the handle came from `CreateMutexW`, and this value owns it
        // alone and closes it once.
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// A mutex this thread owns, released on drop before its handle closes.
#[cfg(windows)]
struct OwnedMutex(MutexHandle);

#[cfg(windows)]
impl Drop for OwnedMutex {
    fn drop(&mut self) {
        // A mutex left unreleased is abandoned when the process exits, and the
        // next updater's wait takes an abandoned mutex as free, so a failed
        // release changes nothing and has no log to go to.
        // SAFETY: this thread owns the mutex behind the live handle.
        let _ = unsafe { windows::Win32::System::Threading::ReleaseMutex(self.0.0) };
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::server::test_support::ScratchDir;
    use std::cell::{Cell, RefCell};

    const DAEMON: &str = "rustling-tulipd.exe";
    const TRACER: &str = "rt-tracer.exe";
    const SCRIPT: &str = "user-path.ps1";

    struct Processes(Vec<RunningExe>);

    impl ProcessList for Processes {
        fn running(&self) -> Vec<RunningExe> {
            self.0.clone()
        }
    }

    struct Daemon {
        result: Result<(), String>,
        stops: Cell<usize>,
    }

    impl Daemon {
        fn stopping() -> Self {
            Self {
                result: Ok(()),
                stops: Cell::new(0),
            }
        }

        fn failing(reason: &str) -> Self {
            Self {
                result: Err(reason.to_string()),
                stops: Cell::new(0),
            }
        }
    }

    impl DaemonStop for Daemon {
        fn stop(&self) -> Pin<Box<dyn Future<Output = Result<(), String>> + '_>> {
            self.stops.set(self.stops.get() + 1);
            Box::pin(std::future::ready(self.result.clone()))
        }
    }

    #[derive(Default)]
    struct Launches(RefCell<Vec<(PathBuf, Vec<OsString>)>>);

    impl Launcher for Launches {
        fn launch(&self, program: &Path, args: &[OsString]) -> anyhow::Result<()> {
            self.0
                .borrow_mut()
                .push((program.to_path_buf(), args.to_vec()));
            Ok(())
        }
    }

    struct FreeLock;

    impl UpdateLock for FreeLock {
        fn try_lock(&self) -> anyhow::Result<Option<LockHold>> {
            Ok(Some(Box::new(())))
        }
    }

    /// A staged release in `from` over an installed one in `to`, with the
    /// result file under `updates/`.
    struct Fixture {
        _dir: ScratchDir,
        from: PathBuf,
        to: PathBuf,
        result: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = ScratchDir::new(tag);
            let from = dir.path().join("install").join(".staging");
            let to = dir.path().join("install");
            std::fs::create_dir_all(&from).expect("create the staging folder");
            for name in [DAEMON, TRACER, CLIENT_EXE] {
                std::fs::write(to.join(name), format!("old {name}")).expect("write an old file");
            }
            for name in [DAEMON, TRACER, CLIENT_EXE, SCRIPT] {
                std::fs::write(from.join(name), format!("new {name}"))
                    .expect("write a staged file");
            }
            let result = dir.path().join("updates").join("last-result.json");
            Self {
                _dir: dir,
                from,
                to,
                result,
            }
        }

        fn request(&self, relaunch: bool) -> Request {
            Request {
                from: self.from.clone(),
                to: self.to.clone(),
                old_version: Some("0.1.0".to_string()),
                relaunch,
            }
        }

        fn installed(&self, name: &str) -> Option<String> {
            std::fs::read_to_string(self.to.join(name)).ok()
        }

        /// The install folder still holds the old release, no new file and no
        /// `.new` file, and the staging folder is untouched.
        fn assert_unchanged(&self) {
            for name in [DAEMON, TRACER, CLIENT_EXE] {
                assert_eq!(
                    self.installed(name).as_deref(),
                    Some(format!("old {name}").as_str()),
                    "{name} is the old file"
                );
            }
            assert_eq!(self.installed(SCRIPT), None, "no new file was left");
            assert_eq!(self.leftovers(".new"), Vec::<String>::new());
            for name in [DAEMON, TRACER, CLIENT_EXE, SCRIPT] {
                assert!(self.from.join(name).is_file(), "{name} is still staged");
            }
        }

        /// The install folder's entries whose name ends in `suffix`.
        fn leftovers(&self, suffix: &str) -> Vec<String> {
            std::fs::read_dir(&self.to)
                .expect("list the install folder")
                .map(|entry| {
                    entry
                        .expect("read an entry")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .filter(|name| name.ends_with(suffix))
                .collect()
        }

        fn result_json(&self) -> serde_json::Value {
            let bytes = std::fs::read(&self.result).expect("the result file is written");
            serde_json::from_slice(&bytes).expect("the result file is JSON")
        }
    }

    fn hooks<'a>(
        processes: &'a Processes,
        daemon: &'a Daemon,
        launches: &'a Launches,
        lock: &'a dyn UpdateLock,
    ) -> Hooks<'a> {
        Hooks {
            processes,
            daemon,
            launcher: launches,
            lock,
            client_wait: Duration::ZERO,
        }
    }

    async fn apply(
        fixture: &Fixture,
        request: &Request,
        processes: &Processes,
        daemon: &Daemon,
        launches: &Launches,
    ) -> (u8, String) {
        let mut out = Vec::new();
        let code = run(
            request,
            &fixture.result,
            &hooks(processes, daemon, launches, &FreeLock),
            &mut out,
        )
        .await;
        (code, String::from_utf8(out).expect("the output is UTF-8"))
    }

    fn bridge_in(to: &Path) -> Processes {
        Processes(vec![RunningExe {
            pid: 4242,
            exe: to.join(DAEMON),
        }])
    }

    #[tokio::test]
    async fn apply_swaps_every_file_client_last() {
        let fixture = Fixture::new("apply-update-swap");
        let staged_daemon = RunningExe {
            pid: 7,
            exe: fixture.from.join(DAEMON),
        };
        let daemon = Daemon::stopping();
        let launches = Launches::default();

        let (code, out) = apply(
            &fixture,
            &fixture.request(false),
            &Processes(vec![staged_daemon]),
            &daemon,
            &launches,
        )
        .await;

        assert_eq!(code, 0, "{out}");
        assert_eq!(out, format!("apply-update: updated to {VERSION}\n"));
        for name in [DAEMON, TRACER, CLIENT_EXE, SCRIPT] {
            assert_eq!(
                fixture.installed(name).as_deref(),
                Some(format!("new {name}").as_str()),
                "{name} is the new file"
            );
        }
        assert_eq!(fixture.leftovers(".new"), Vec::<String>::new());
        assert_eq!(fixture.leftovers(".old"), Vec::<String>::new());
        assert!(!fixture.from.exists(), "the staging folder is removed");
        assert_eq!(daemon.stops.get(), 1);
        assert!(launches.0.borrow().is_empty(), "no relaunch was asked for");
        let order = swap_order(
            [CLIENT_EXE, TRACER, DAEMON, "zz.txt"]
                .map(OsString::from)
                .to_vec(),
        );
        assert_eq!(
            order.last().map(OsString::as_os_str),
            Some(OsStr::new(CLIENT_EXE)),
            "the client is swapped last: {order:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_daemon_stop_changes_nothing() {
        let fixture = Fixture::new("apply-update-stop");
        let daemon = Daemon::failing("connection refused");

        let (code, out) = apply(
            &fixture,
            &fixture.request(false),
            &Processes(Vec::new()),
            &daemon,
            &Launches::default(),
        )
        .await;

        assert_eq!(code, 2, "{out}");
        let message = "could not stop the daemon: connection refused; nothing was changed";
        assert_eq!(out, format!("apply-update: {message}\n"));
        fixture.assert_unchanged();
        assert_eq!(fixture.result_json()["message"], message);
    }

    #[tokio::test]
    async fn a_bridge_running_from_the_install_folder_changes_nothing() {
        let fixture = Fixture::new("apply-update-bridge");

        let (code, out) = apply(
            &fixture,
            &fixture.request(false),
            &bridge_in(&fixture.to),
            &Daemon::stopping(),
            &Launches::default(),
        )
        .await;

        assert_eq!(code, 3, "{out}");
        let message = format!(
            "{DAEMON} is still running from the install folder (pid 4242); nothing was changed"
        );
        assert_eq!(out, format!("apply-update: {message}\n"));
        fixture.assert_unchanged();
        assert_eq!(fixture.result_json()["message"], message.as_str());
    }

    #[tokio::test]
    async fn a_failed_rename_rolls_every_file_back() {
        let fixture = Fixture::new("apply-update-rollback");
        // A folder where the client's `.old` goes makes its rename fail after
        // every other file was swapped.
        let blocker = fixture.to.join(format!("{CLIENT_EXE}.old"));
        std::fs::create_dir_all(&blocker).expect("create the blocking folder");
        std::fs::write(blocker.join("keep"), "keep").expect("fill the blocking folder");

        let (code, out) = apply(
            &fixture,
            &fixture.request(false),
            &Processes(Vec::new()),
            &Daemon::stopping(),
            &Launches::default(),
        )
        .await;

        assert_eq!(code, 5, "{out}");
        assert!(
            out.starts_with(&format!("apply-update: could not replace {CLIENT_EXE}: ")),
            "{out}"
        );
        assert!(out.ends_with("; every file was put back\n"), "{out}");
        fixture.assert_unchanged();
        assert_eq!(fixture.leftovers(".old"), vec![format!("{CLIENT_EXE}.old")]);
        assert_eq!(fixture.result_json()["ok"], false);
    }

    #[tokio::test]
    async fn the_result_file_is_written_on_success_and_on_failure() {
        let fixture = Fixture::new("apply-update-result-ok");
        let (code, _) = apply(
            &fixture,
            &fixture.request(false),
            &Processes(Vec::new()),
            &Daemon::stopping(),
            &Launches::default(),
        )
        .await;
        assert_eq!(code, 0);
        assert_eq!(
            fixture.result_json(),
            serde_json::json!({
                "from": "0.1.0",
                "to": VERSION,
                "ok": true,
                "message": format!("updated to {VERSION}"),
            })
        );

        let fixture = Fixture::new("apply-update-result-failed");
        let (code, _) = apply(
            &fixture,
            &fixture.request(false),
            &Processes(Vec::new()),
            &Daemon::failing("timed out"),
            &Launches::default(),
        )
        .await;
        assert_eq!(code, 2);
        assert_eq!(
            fixture.result_json(),
            serde_json::json!({
                "from": "0.1.0",
                "to": VERSION,
                "ok": false,
                "message": "could not stop the daemon: timed out; nothing was changed",
            })
        );
    }

    #[tokio::test]
    async fn the_result_records_the_old_version_it_was_given() {
        for (tag, old_version) in [("given", Some("0.0.9")), ("absent", None)] {
            let fixture = Fixture::new(&format!("apply-update-old-version-{tag}"));
            let request = Request {
                old_version: old_version.map(str::to_string),
                ..fixture.request(false)
            };
            let (code, out) = apply(
                &fixture,
                &request,
                &Processes(Vec::new()),
                &Daemon::stopping(),
                &Launches::default(),
            )
            .await;
            assert_eq!(code, 0, "{out}");
            assert_eq!(
                fixture.result_json()["from"],
                serde_json::json!(old_version),
                "{tag}"
            );
        }
    }

    #[tokio::test]
    async fn relaunch_starts_the_client_once_on_success_and_on_failure() {
        let expected_args = ["--updated-from", "0.1.0"].map(OsString::from).to_vec();

        let fixture = Fixture::new("apply-update-relaunch-ok");
        let launches = Launches::default();
        let (code, _) = apply(
            &fixture,
            &fixture.request(true),
            &Processes(Vec::new()),
            &Daemon::stopping(),
            &launches,
        )
        .await;
        assert_eq!(code, 0);
        assert_eq!(
            *launches.0.borrow(),
            vec![(fixture.to.join(CLIENT_EXE), expected_args.clone())]
        );

        let fixture = Fixture::new("apply-update-relaunch-failed");
        let launches = Launches::default();
        let (code, _) = apply(
            &fixture,
            &fixture.request(true),
            &bridge_in(&fixture.to),
            &Daemon::stopping(),
            &launches,
        )
        .await;
        assert_eq!(code, 3);
        assert_eq!(
            *launches.0.borrow(),
            vec![(fixture.to.join(CLIENT_EXE), expected_args)]
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_held_update_mutex_exits_without_touching_files() {
        let fixture = Fixture::new("apply-update-held");
        let name = format!(r"Local\rustling-tulip-update-test-{}", std::process::id());
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        // A mutex is re-entrant for the thread that owns it, so another thread
        // holds it, as another updater's process would.
        let holder = std::thread::spawn({
            let name = name.clone();
            move || {
                let hold = NamedMutex { name }
                    .try_lock()
                    .expect("create the test mutex")
                    .expect("the test mutex is free");
                locked_tx.send(()).expect("signal the hold");
                let _ = release_rx.recv();
                drop(hold);
            }
        });
        locked_rx.recv().expect("the holder took the mutex");
        let daemon = Daemon::stopping();
        let launches = Launches::default();
        let lock = NamedMutex { name };
        let mut out = Vec::new();

        let code = run(
            &fixture.request(true),
            &fixture.result,
            &hooks(&Processes(Vec::new()), &daemon, &launches, &lock),
            &mut out,
        )
        .await;

        release_tx.send(()).expect("release the holder");
        holder.join().expect("the holder thread ends");
        assert_eq!(code, 4);
        assert_eq!(
            String::from_utf8(out).expect("the output is UTF-8"),
            "apply-update: another update is running\n"
        );
        fixture.assert_unchanged();
        assert!(!fixture.result.exists(), "the other update owns the result");
        assert_eq!(daemon.stops.get(), 0, "the daemon was left alone");
        assert!(
            launches.0.borrow().is_empty(),
            "the other update relaunches"
        );
    }

    #[test]
    fn version_flag_prints_the_build() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(version_requested(args(&["--version"])));
        for list in [&[][..], &["apply-update"], &["shutdown", "--version"]] {
            assert!(!version_requested(args(list)), "{list:?}");
        }
        let mut out = Vec::new();
        write_version(&mut out).expect("the line is written");
        assert_eq!(
            String::from_utf8(out).expect("the line is UTF-8"),
            format!("rustling-tulipd {VERSION}\n")
        );
    }
}
