//! The daemon's `apply-update` subcommand: put a staged release in place.
//!
//! The installer extracts everything it ships into `<install>\.staging` and
//! runs the staged daemon as `rustling-tulipd apply-update --from <staging>
//! --to <install> [--old-version <v>] [--update] [--relaunch] [--restart-daemon]`. This takes the
//! update lock, waits for the client to exit, stops the running daemon before
//! anything is written, refuses while anything still runs from the install
//! folder, swaps each file in through `<name>.new` and `<name>.old` and each
//! folder [`STAGED_DIRS`] names whole by rename (the client last, every swap
//! rolled back on a failure), records the outcome in `last-result.json` and,
//! when asked, starts the client again, or the installed daemon when no client
//! was started.
//!
//! When `--to` is an `.app` bundle the bundle replaces those files: the
//! installed app is renamed to `<name>.app.old`, the staged app is renamed into
//! its place, and a failure puts the old app back, so the signature seal is
//! never broken half-way.
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

/// The daemon's file name in the install folder; it is what `--restart-daemon`
/// starts.
#[cfg(windows)]
pub const DAEMON_EXE: &str = "rustling-tulipd.exe";
#[cfg(not(windows))]
pub const DAEMON_EXE: &str = "rustling-tulipd";

/// The folder inside an installed `.app` bundle that holds its executables.
const BUNDLE_EXECUTABLES: [&str; 2] = ["Contents", "MacOS"];

/// The folders a staging may ship and this command moves whole. A fixed list,
/// so a stray staged folder cannot replace an arbitrary install subfolder.
const STAGED_DIRS: [&str; 1] = ["pwa"];

/// The probe file an update creates in a bundle's parent folder to prove it can
/// put a bundle there.
const UPDATE_PROBE: &str = ".rustling-tulip-update-probe";

/// The named mutex every updater and installer holds while it runs.
#[cfg(windows)]
pub const UPDATE_MUTEX: &str = r"Local\rustling-tulip-update";

/// This build's version, the one an update installs.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long to wait for the client to exit before checking what still runs.
const CLIENT_WAIT: Duration = Duration::from_secs(30);

/// How often the client wait looks at the process list again.
const CLIENT_POLL: Duration = Duration::from_millis(250);

/// How long to wait for bridges to leave after the daemon stopped.
const BRIDGE_WAIT: Duration = Duration::from_secs(30);

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
/// exactly `--version` or `-V`.
pub fn version_requested(args: impl IntoIterator<Item = OsString>) -> bool {
    args.into_iter()
        .next()
        .is_some_and(|arg| arg == "--version" || arg == "-V")
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
    /// Start the installed daemon once the run ends, unless a relaunched client
    /// starts it.
    pub restart_daemon: bool,
}

/// Parse the arguments after `apply-update`; `--relaunch` and
/// `--restart-daemon` are the flags that start a program once the run ends.
///
/// # Errors
///
/// An unknown argument, a flag missing its value, an `--old-version` that is
/// not UTF-8, a missing `--from` or `--to`, or a `--to` that is an `.app`
/// bundle staged from something that is not one.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> anyhow::Result<Request> {
    let mut args = args.into_iter();
    let (mut from, mut to, mut old_version, mut relaunch, mut restart_daemon) =
        (None, None, None, false, false);
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
            Some("--restart-daemon") => restart_daemon = true,
            // The installer's update mode changes only what it writes to the
            // registry; the files are applied the same way.
            Some("--update") => {}
            _ => anyhow::bail!("unknown argument {}", arg.display()),
        }
    }
    let from = from.context("--from <staging folder> is required")?;
    let to = to.context("--to <install folder> is required")?;
    if is_bundle(&to) && !is_bundle(&from) {
        anyhow::bail!("--from must be a .app bundle when --to is one");
    }
    Ok(Request {
        from,
        to,
        old_version,
        relaunch,
        restart_daemon,
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
/// folder, `4` another update holds the lock, `5` a file or a staged folder
/// could not be swapped (every swap was rolled back). When `to` is an `.app`
/// bundle, the whole bundle is swapped by rename instead of each entry, and
/// the old app is put back when that fails. Afterwards it starts the client
/// again when `--relaunch` asks or `--restart-daemon` saw one running at the
/// start, and otherwise starts the installed daemon under `--restart-daemon`.
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
    let client_was_running = running_from(&exe_folder(&request.to), hooks.processes)
        .iter()
        .any(|process| is_client(process.exe.file_name()));
    let outcome = apply(request, hooks, out).await;
    say(out, &outcome.message);
    if let Err(err) = write_result(result_file, request, &outcome) {
        say(
            out,
            &format!("could not write {}: {err:#}", result_file.display()),
        );
    }
    restart(request, client_was_running, hooks.launcher, out);
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
    let bundle = is_bundle(&request.to);
    if bundle && let Err(message) = check_replaceable(&request.to) {
        return Outcome::failed(SWAP_FAILED_CODE, message);
    }
    let installed = exe_folder(&request.to);
    wait_for_clients(&installed, hooks).await;
    if let Err(reason) = hooks.daemon.stop().await {
        return Outcome::failed(
            STOP_FAILED_CODE,
            format!("could not stop the daemon: {reason}; nothing was changed"),
        );
    }
    wait_for_bridges(&installed, hooks).await;
    if let Some(busy) = running_from(&installed, hooks.processes).first() {
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
    if bundle {
        return match swap_bundle(&request.from, &request.to) {
            Ok(()) => {
                clean_up_bundle(&request.to, out);
                updated()
            }
            Err(message) => Outcome::failed(SWAP_FAILED_CODE, message),
        };
    }
    match swap_all(&request.from, &request.to) {
        Ok(swapped) => {
            clean_up(&request.from, &swapped, out);
            updated()
        }
        Err(message) => Outcome::failed(SWAP_FAILED_CODE, message),
    }
}

/// The outcome of an update now in place.
fn updated() -> Outcome {
    Outcome {
        code: 0,
        ok: true,
        message: format!("updated to {VERSION}"),
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

/// Wait until no daemon exe runs directly from `to` (a bridge, which leaves
/// once the daemon closed its socket), or until [`BRIDGE_WAIT`] ends. The
/// deadline is tokio's clock, which a paused test runtime moves with the
/// polls the wait sleeps on.
async fn wait_for_bridges(to: &Path, hooks: &Hooks<'_>) {
    let deadline = tokio::time::Instant::now() + BRIDGE_WAIT;
    while running_from(to, hooks.processes)
        .iter()
        .any(|process| is_daemon(process.exe.file_name()))
        && tokio::time::Instant::now() < deadline
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

fn is_daemon(name: Option<&OsStr>) -> bool {
    name.is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(DAEMON_EXE))
}

/// One entry put in place: the installed path, and where its old copy went
/// when there was one.
struct Swap {
    target: PathBuf,
    old: Option<PathBuf>,
    /// A folder moved whole: it is put back or deleted with all it holds.
    dir: bool,
}

/// A swap that failed, with the part of it that needs rolling back.
struct SwapError {
    err: std::io::Error,
    partial: Option<Swap>,
}

/// Swap every staged entry in `from` into `to`: the staged folders first, then
/// the files, the client last. On a failure, every swap made is rolled back
/// and every `.new` entry deleted, and the error is the line to report.
fn swap_all(from: &Path, to: &Path) -> Result<Vec<Swap>, String> {
    let plan = staged_plan(from).map_err(|err| {
        format!(
            "could not read {}: {err}; nothing was changed",
            from.display()
        )
    })?;
    let mut done = Vec::new();
    for (name, kind) in &plan {
        let swapped = match kind {
            Staged::Dir => swap_dir(from, to, name),
            Staged::File => swap_one(from, to, name),
        };
        match swapped {
            Ok(swap) => done.push(swap),
            Err(SwapError { err, partial }) => {
                done.extend(partial);
                let mut stuck = roll_back(&done);
                stuck.extend(remove_new(to, &plan));
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

/// One staged entry to put in place, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Staged {
    /// A regular file, copied through `<name>.new`.
    File,
    /// A folder of [`STAGED_DIRS`], moved whole by rename.
    Dir,
}

/// The staged entries in `from` to put in place: each [`STAGED_DIRS`] folder
/// present, then the regular files in swap order. Any other folder in the
/// staging is ignored.
fn staged_plan(from: &Path) -> std::io::Result<Vec<(OsString, Staged)>> {
    let mut plan = Vec::new();
    for name in STAGED_DIRS {
        if from.join(name).is_dir() {
            plan.push((OsString::from(name), Staged::Dir));
        }
    }
    for name in swap_order(staged_names(from)?) {
        plan.push((name, Staged::File));
    }
    Ok(plan)
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
        dir: false,
    };
    match std::fs::rename(&new, &swap.target) {
        Ok(()) => Ok(swap),
        Err(err) => Err(SwapError {
            err,
            partial: Some(swap),
        }),
    }
}

/// Move the staged folder `from/name` into `to/name` whole: a leftover `.new`
/// or `.old` is removed first, the staged folder is renamed to `.new`, the
/// installed one to `.old`, and `.new` into place.
fn swap_dir(from: &Path, to: &Path, name: &OsStr) -> Result<Swap, SwapError> {
    let target = to.join(name);
    let new = sibling(&target, "new");
    let old = sibling(&target, "old");
    remove_dir(&new).map_err(|err| SwapError { err, partial: None })?;
    remove_dir(&old).map_err(|err| SwapError { err, partial: None })?;
    std::fs::rename(from.join(name), &new).map_err(|err| SwapError { err, partial: None })?;
    let had_old = match std::fs::rename(&target, &old) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(SwapError { err, partial: None }),
    };
    let swap = Swap {
        target,
        old: had_old.then_some(old),
        dir: true,
    };
    match std::fs::rename(&new, &swap.target) {
        Ok(()) => Ok(swap),
        Err(err) => Err(SwapError {
            err,
            partial: Some(swap),
        }),
    }
}

/// Move the folder at `path` aside to its `.new` name, where [`remove_new`]
/// clears it; an absent folder has nothing to move.
fn move_aside(path: &Path) -> std::io::Result<()> {
    match std::fs::rename(path, sibling(path, "new")) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Remove the folder at `path` with everything in it; absent is fine.
fn remove_dir(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// `path` with `.suffix` added to its file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

/// Undo `done`, newest first: each old copy goes back into place, and an entry
/// that had none is deleted. A folder's swapped copy is moved aside to
/// `<name>.new` first, never deleted, so [`remove_new`] clears it and a failure
/// anywhere leaves the install with a whole folder. Returns the paths that
/// could not be restored.
fn roll_back(done: &[Swap]) -> Vec<String> {
    let mut stuck = Vec::new();
    for swap in done.iter().rev() {
        let restored = match &swap.old {
            Some(old) if swap.dir => {
                move_aside(&swap.target).and_then(|()| std::fs::rename(old, &swap.target))
            }
            Some(old) => std::fs::rename(old, &swap.target),
            None if swap.dir => move_aside(&swap.target),
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

/// Delete every `<name>.new` entry the plan could have made in `to`; returns
/// those that could not be.
fn remove_new(to: &Path, plan: &[(OsString, Staged)]) -> Vec<String> {
    let mut stuck = Vec::new();
    for (name, kind) in plan {
        let new = sibling(&to.join(name), "new");
        let removed = match kind {
            Staged::Dir => remove_dir(&new),
            Staged::File => match std::fs::remove_file(&new) {
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            },
        };
        if let Err(err) = removed {
            stuck.push(format!("{} ({err})", new.display()));
        }
    }
    stuck
}

/// True when `path` names an `.app` bundle, whatever the case of its extension.
fn is_bundle(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("app"))
}

/// The folder inside the bundle at `to` that holds its executables, and where
/// the bundle's client is looked for.
fn bundle_executables(to: &Path) -> PathBuf {
    BUNDLE_EXECUTABLES
        .iter()
        .fold(to.to_path_buf(), |path, part| path.join(part))
}

/// The folder the processes running from `to` are read in: `to` itself, or
/// the bundle's executables folder.
fn exe_folder(to: &Path) -> PathBuf {
    if is_bundle(to) {
        bundle_executables(to)
    } else {
        to.to_path_buf()
    }
}

/// Where `--relaunch` starts the client: in the install folder, or inside the
/// bundle's executables folder.
fn client_path(to: &Path) -> PathBuf {
    if is_bundle(to) {
        bundle_executables(to).join(CLIENT_EXE)
    } else {
        to.join(CLIENT_EXE)
    }
}

/// Where `--restart-daemon` starts the daemon: in the install folder, or
/// inside the bundle's executables folder.
fn daemon_path(to: &Path) -> PathBuf {
    if is_bundle(to) {
        bundle_executables(to).join(DAEMON_EXE)
    } else {
        to.join(DAEMON_EXE)
    }
}

/// Why the bundle at `to` cannot be replaced by a rename, as the line to
/// report, or `Ok` when it can: a bundle the system runs translocated, or one
/// whose parent folder this process cannot write a probe file in.
fn check_replaceable(to: &Path) -> Result<(), String> {
    const REFUSED: &str = "rustling-tulip.app runs from a place the update cannot replace; \
                           move it to Applications and update again; nothing was changed";
    if to
        .components()
        .any(|part| part.as_os_str() == OsStr::new("AppTranslocation"))
    {
        return Err(REFUSED.to_string());
    }
    let Some(parent) = to.parent() else {
        return Err(REFUSED.to_string());
    };
    let probe = parent.join(UPDATE_PROBE);
    let writable = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    if writable {
        Ok(())
    } else {
        Err(REFUSED.to_string())
    }
}

/// Replace the whole bundle at `to` with the staged one at `from`: the old app
/// moves to `<name>.app.old` and the new one is renamed in, with the old app
/// put back when that rename fails.
fn swap_bundle(from: &Path, to: &Path) -> Result<(), String> {
    let old = sibling(to, "old");
    if old.exists() {
        std::fs::remove_dir_all(&old).map_err(|err| {
            format!(
                "could not remove {}: {err}; nothing was changed",
                old.display()
            )
        })?;
    }
    std::fs::rename(to, &old).map_err(|err| {
        format!(
            "could not replace {}: {err}; nothing was changed",
            to.display()
        )
    })?;
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(err) => Err(match std::fs::rename(&old, to) {
            Ok(()) => format!(
                "could not replace {}: {err}; the old app was put back",
                to.display()
            ),
            Err(back) => format!(
                "could not replace {}: {err}; could not put back the old app ({back})",
                to.display()
            ),
        }),
    }
}

/// Remove the bundle this run moved aside. What cannot be removed is reported
/// and left: the client removes it at its next launch.
fn clean_up_bundle(to: &Path, out: &mut impl Write) {
    let old = sibling(to, "old");
    if let Err(err) = std::fs::remove_dir_all(&old) {
        say(out, &format!("could not remove {}: {err}", old.display()));
    }
}

/// Remove the staging folder and the old copies this run moved aside. What
/// cannot be removed is reported and left: the installer removes the staging
/// folder after this exits (this process's own exe is in it), and the client
/// deletes leftover `.old` files at launch.
fn clean_up(from: &Path, swapped: &[Swap], out: &mut impl Write) {
    if let Err(err) = std::fs::remove_dir_all(from) {
        say(out, &format!("could not remove {}: {err}", from.display()));
    }
    for swap in swapped {
        let Some(old) = swap.old.as_ref() else {
            continue;
        };
        let removed = if swap.dir {
            std::fs::remove_dir_all(old)
        } else {
            std::fs::remove_file(old)
        };
        if let Err(err) = removed {
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
    crate::atomic_file::write_atomic(path, &serde_json::to_vec(&record)?)
}

/// Start the client again when the run asks for one, and the installed daemon
/// when it asks for that and no client took the start.
fn restart(
    request: &Request,
    client_was_running: bool,
    launcher: &dyn Launcher,
    out: &mut impl Write,
) {
    let wanted = request.relaunch || (request.restart_daemon && client_was_running);
    if wanted && relaunch(request, launcher, out) {
        return;
    }
    if request.restart_daemon {
        start_daemon(request, launcher, out);
    }
}

/// Start the installed daemon detached from this process.
fn start_daemon(request: &Request, launcher: &dyn Launcher, out: &mut impl Write) {
    let daemon = daemon_path(&request.to);
    if let Err(err) = launcher.launch(&daemon, &[OsString::from("--detach")]) {
        say(
            out,
            &format!("could not start {}: {err:#}", daemon.display()),
        );
    }
}

/// Start the installed client once, telling it which version it replaced;
/// `true` when it started.
fn relaunch(request: &Request, launcher: &dyn Launcher, out: &mut impl Write) -> bool {
    let client = client_path(&request.to);
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
        return false;
    }
    true
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

    const DAEMON: &str = DAEMON_EXE;
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
        /// Whether a stop was asked for, which the leaving bridge reads.
        stopped: Cell<bool>,
    }

    impl Daemon {
        fn stopping() -> Self {
            Self {
                result: Ok(()),
                stops: Cell::new(0),
                stopped: Cell::new(false),
            }
        }

        fn failing(reason: &str) -> Self {
            Self {
                result: Err(reason.to_string()),
                stops: Cell::new(0),
                stopped: Cell::new(false),
            }
        }
    }

    impl DaemonStop for Daemon {
        fn stop(&self) -> Pin<Box<dyn Future<Output = Result<(), String>> + '_>> {
            self.stops.set(self.stops.get() + 1);
            self.stopped.set(true);
            Box::pin(std::future::ready(self.result.clone()))
        }
    }

    /// A bridge that leaves as the daemon stops: the first two looks after the
    /// stop still find it, and every later one does not.
    struct BridgeLeavingOnStop<'a> {
        daemon: &'a Daemon,
        looks: Cell<usize>,
        exe: PathBuf,
    }

    impl ProcessList for BridgeLeavingOnStop<'_> {
        fn running(&self) -> Vec<RunningExe> {
            if self.daemon.stopped.get() {
                let look = self.looks.get() + 1;
                self.looks.set(look);
                if look <= 2 {
                    return vec![RunningExe {
                        pid: 4242,
                        exe: self.exe.clone(),
                    }];
                }
            }
            Vec::new()
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

    /// Records every launch and fails the client's, so a daemon start after a
    /// failed relaunch can be told from one that was never asked for.
    #[derive(Default)]
    struct FailingClientLaunches(RefCell<Vec<(PathBuf, Vec<OsString>)>>);

    impl Launcher for FailingClientLaunches {
        fn launch(&self, program: &Path, args: &[OsString]) -> anyhow::Result<()> {
            self.0
                .borrow_mut()
                .push((program.to_path_buf(), args.to_vec()));
            if program.file_name() == Some(OsStr::new(CLIENT_EXE)) {
                anyhow::bail!("the client would not start");
            }
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
            self.request_with(relaunch, false)
        }

        fn request_with(&self, relaunch: bool, restart_daemon: bool) -> Request {
            Request {
                from: self.from.clone(),
                to: self.to.clone(),
                old_version: Some("0.1.0".to_string()),
                relaunch,
                restart_daemon,
            }
        }

        fn installed(&self, name: &str) -> Option<String> {
            std::fs::read_to_string(self.to.join(name)).ok()
        }

        /// Gives the install folder a `pwa` holding an old index and asset.
        fn with_installed_pwa(&self) {
            write_pwa(&self.to, "old");
        }

        /// Gives the staging folder a `pwa` holding a new index and asset.
        fn with_staged_pwa(&self) {
            write_pwa(&self.from, "new");
        }

        /// The install folder's `pwa/<relative>`, when it exists.
        fn installed_pwa(&self, relative: &str) -> Option<String> {
            std::fs::read_to_string(self.to.join("pwa").join(relative)).ok()
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
        processes: &'a dyn ProcessList,
        daemon: &'a Daemon,
        launches: &'a dyn Launcher,
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
        processes: &dyn ProcessList,
        daemon: &Daemon,
        launches: &dyn Launcher,
    ) -> (u8, String) {
        apply_to(&fixture.result, request, processes, daemon, launches).await
    }

    async fn apply_to(
        result: &Path,
        request: &Request,
        processes: &dyn ProcessList,
        daemon: &Daemon,
        launches: &dyn Launcher,
    ) -> (u8, String) {
        let mut out = Vec::new();
        let code = run(
            request,
            result,
            &hooks(processes, daemon, launches, &FreeLock),
            &mut out,
        )
        .await;
        (code, String::from_utf8(out).expect("the output is UTF-8"))
    }

    /// A `pwa` folder under `root` holding a `<body> index` and the asset named
    /// after `body`, so a replacement is told from a merge.
    fn write_pwa(root: &Path, body: &str) {
        let pwa = root.join("pwa");
        std::fs::create_dir_all(pwa.join("assets")).expect("create the pwa folder");
        std::fs::write(pwa.join("index.html"), format!("{body} index")).expect("write the index");
        std::fs::write(pwa.join("assets").join(format!("{body}.js")), body)
            .expect("write the asset");
    }

    fn bridge_in(to: &Path) -> Processes {
        Processes(vec![RunningExe {
            pid: 4242,
            exe: to.join(DAEMON),
        }])
    }

    const APP: &str = "rustling-tulip.app";

    /// A staged `.app` bundle in `from` over an installed one in `to`, both
    /// under `Applications/`, with the result file under `updates/`.
    struct Bundle {
        _dir: ScratchDir,
        from: PathBuf,
        to: PathBuf,
        result: PathBuf,
    }

    impl Bundle {
        fn new(tag: &str) -> Self {
            let dir = ScratchDir::new(tag);
            let applications = dir.path().join("Applications");
            Self::under(dir, &applications)
        }

        /// A bundle under the folder the system mounts translocated apps in.
        fn translocated(tag: &str) -> Self {
            let dir = ScratchDir::new(tag);
            let applications = dir.path().join("AppTranslocation").join("X").join("d");
            Self::under(dir, &applications)
        }

        fn under(dir: ScratchDir, applications: &Path) -> Self {
            let to = applications.join(APP);
            let from = applications.join(".stage").join(APP);
            for (root, body) in [(&to, "old"), (&from, "new")] {
                let executables = bundle_executables(root);
                std::fs::create_dir_all(&executables).expect("create the bundle");
                std::fs::write(executables.join(CLIENT_EXE), body)
                    .expect("write the bundle's client");
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
            self.request_with(relaunch, false)
        }

        fn request_with(&self, relaunch: bool, restart_daemon: bool) -> Request {
            Request {
                from: self.from.clone(),
                to: self.to.clone(),
                old_version: Some("0.1.0".to_string()),
                relaunch,
                restart_daemon,
            }
        }

        fn old(&self) -> PathBuf {
            sibling(&self.to, "old")
        }

        fn result_json(&self) -> serde_json::Value {
            let bytes = std::fs::read(&self.result).expect("the result file is written");
            serde_json::from_slice(&bytes).expect("the result file is JSON")
        }
    }

    /// The client file's contents inside the bundle at `root`.
    fn bundle_client(root: &Path) -> Option<String> {
        std::fs::read_to_string(bundle_executables(root).join(CLIENT_EXE)).ok()
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

    #[tokio::test(start_paused = true)]
    async fn a_bridge_that_never_leaves_still_changes_nothing() {
        let fixture = Fixture::new("apply-update-bridge");

        let start = tokio::time::Instant::now();
        let (code, out) = apply(
            &fixture,
            &fixture.request(false),
            &bridge_in(&fixture.to),
            &Daemon::stopping(),
            &Launches::default(),
        )
        .await;

        assert_eq!(code, 3, "{out}");
        assert!(
            start.elapsed() >= BRIDGE_WAIT,
            "the wait gave the bridge its whole window"
        );
        let message = format!(
            "{DAEMON} is still running from the install folder (pid 4242); nothing was changed"
        );
        assert_eq!(out, format!("apply-update: {message}\n"));
        fixture.assert_unchanged();
        assert_eq!(fixture.result_json()["message"], message.as_str());
    }

    #[tokio::test(start_paused = true)]
    async fn a_bridge_that_leaves_after_the_daemon_stops_lets_the_swap_run() {
        let fixture = Fixture::new("apply-update-bridge-leaves");
        let daemon = Daemon::stopping();
        let processes = BridgeLeavingOnStop {
            daemon: &daemon,
            looks: Cell::new(0),
            exe: fixture.to.join(DAEMON),
        };

        let start = tokio::time::Instant::now();
        let (code, out) = apply(
            &fixture,
            &fixture.request(false),
            &processes,
            &daemon,
            &Launches::default(),
        )
        .await;

        assert_eq!(code, 0, "{out}");
        assert!(
            start.elapsed() >= CLIENT_POLL,
            "the wait polled again for the leaving bridge"
        );
        assert_eq!(
            fixture.installed(CLIENT_EXE).as_deref(),
            Some(format!("new {CLIENT_EXE}").as_str()),
            "the client was swapped"
        );
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
    async fn apply_moves_the_staged_pwa_folder_into_place_replacing_the_old_one() {
        let fixture = Fixture::new("apply-update-pwa-swap");
        fixture.with_installed_pwa();
        fixture.with_staged_pwa();

        let (code, out) = apply(
            &fixture,
            &fixture.request(false),
            &Processes(Vec::new()),
            &Daemon::stopping(),
            &Launches::default(),
        )
        .await;

        assert_eq!(code, 0, "{out}");
        assert_eq!(
            fixture.installed_pwa("index.html").as_deref(),
            Some("new index"),
            "the new index is in place"
        );
        let assets = fixture.to.join("pwa").join("assets");
        assert!(
            !assets.join("old.js").exists(),
            "the old asset is gone, not merged"
        );
        assert!(assets.join("new.js").is_file(), "the new asset is there");
        assert_eq!(fixture.leftovers(".new"), Vec::<String>::new());
        assert_eq!(fixture.leftovers(".old"), Vec::<String>::new());
        assert!(!fixture.from.exists(), "the staging folder is removed");
        assert_eq!(
            fixture.installed(CLIENT_EXE).as_deref(),
            Some(format!("new {CLIENT_EXE}").as_str()),
            "the client was swapped too"
        );

        let order = Fixture::new("apply-update-pwa-order");
        order.with_staged_pwa();
        let plan = staged_plan(&order.from).expect("the staging folder reads");
        assert_eq!(
            plan.first().map(|(name, kind)| (name.clone(), *kind)),
            Some((OsString::from("pwa"), Staged::Dir)),
            "a folder goes in first: {plan:?}"
        );
        assert_eq!(
            plan.last().map(|(name, kind)| (name.clone(), *kind)),
            Some((OsString::from(CLIENT_EXE), Staged::File)),
            "the client is swapped last: {plan:?}"
        );
    }

    #[tokio::test]
    async fn apply_installs_the_pwa_folder_when_none_was_installed() {
        let fixture = Fixture::new("apply-update-pwa-first-install");
        fixture.with_staged_pwa();

        let (code, out) = apply(
            &fixture,
            &fixture.request(false),
            &Processes(Vec::new()),
            &Daemon::stopping(),
            &Launches::default(),
        )
        .await;

        assert_eq!(code, 0, "{out}");
        assert_eq!(
            fixture.installed_pwa("index.html").as_deref(),
            Some("new index")
        );
        assert!(
            fixture
                .to
                .join("pwa")
                .join("assets")
                .join("new.js")
                .is_file()
        );
        assert_eq!(fixture.leftovers(".old"), Vec::<String>::new());
        assert_eq!(fixture.leftovers(".new"), Vec::<String>::new());
    }

    #[tokio::test]
    async fn apply_keeps_the_installed_pwa_folder_when_the_staging_has_none() {
        let fixture = Fixture::new("apply-update-pwa-keep");
        fixture.with_installed_pwa();

        let (code, out) = apply(
            &fixture,
            &fixture.request(false),
            &Processes(Vec::new()),
            &Daemon::stopping(),
            &Launches::default(),
        )
        .await;

        assert_eq!(code, 0, "{out}");
        assert_eq!(
            fixture.installed_pwa("index.html").as_deref(),
            Some("old index"),
            "the installed folder is untouched"
        );
        assert!(
            fixture
                .to
                .join("pwa")
                .join("assets")
                .join("old.js")
                .is_file()
        );
        assert_eq!(fixture.leftovers(".old"), Vec::<String>::new());
        assert_eq!(fixture.leftovers(".new"), Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_failed_rename_puts_the_old_pwa_folder_back() {
        let fixture = Fixture::new("apply-update-pwa-rollback");
        fixture.with_installed_pwa();
        fixture.with_staged_pwa();
        // A folder where the client's `.old` goes makes its rename fail after
        // the pwa folder was swapped.
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
        assert!(out.ends_with("; every file was put back\n"), "{out}");
        assert_eq!(
            fixture.installed_pwa("index.html").as_deref(),
            Some("old index"),
            "the old folder is back"
        );
        let assets = fixture.to.join("pwa").join("assets");
        assert!(assets.join("old.js").is_file(), "the old asset is back");
        assert!(!assets.join("new.js").exists(), "no new asset was left");
        assert!(!fixture.to.join("pwa.new").exists(), "no new copy was left");
        assert!(
            !fixture.to.join("pwa.old").exists(),
            "nothing was left aside"
        );
        fixture.assert_unchanged();
        assert_eq!(fixture.result_json()["ok"], false);
    }

    #[tokio::test]
    async fn a_failed_first_install_leaves_no_pwa_folder() {
        let fixture = Fixture::new("apply-update-pwa-first-install-rollback");
        fixture.with_staged_pwa();
        // The client's swap fails after the pwa folder went in, so the folder
        // has to come out again with nothing to put back.
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
            !fixture.to.join("pwa").exists(),
            "no half-installed folder is left"
        );
        assert!(
            !fixture.to.join("pwa.new").exists(),
            "the swapped copy is cleared"
        );
        fixture.assert_unchanged();
    }

    #[tokio::test]
    async fn a_stale_pwa_old_folder_does_not_block_the_swap() {
        let fixture = Fixture::new("apply-update-pwa-stale-old");
        fixture.with_installed_pwa();
        fixture.with_staged_pwa();
        let stale = fixture.to.join("pwa.old");
        std::fs::create_dir_all(&stale).expect("create the stale folder");
        std::fs::write(stale.join("junk"), "junk").expect("fill the stale folder");

        let (code, out) = apply(
            &fixture,
            &fixture.request(false),
            &Processes(Vec::new()),
            &Daemon::stopping(),
            &Launches::default(),
        )
        .await;

        assert_eq!(code, 0, "{out}");
        assert_eq!(
            fixture.installed_pwa("index.html").as_deref(),
            Some("new index")
        );
        assert!(!stale.exists(), "the stale copy is removed");
        assert!(!fixture.to.join("pwa.new").exists());
    }

    #[tokio::test]
    async fn only_the_pwa_folder_is_swapped_from_the_staging() {
        let fixture = Fixture::new("apply-update-pwa-whitelist");
        fixture.with_staged_pwa();
        let evil = fixture.from.join("evil");
        std::fs::create_dir_all(&evil).expect("create another staged folder");
        std::fs::write(evil.join("payload"), "payload").expect("fill it");

        let (code, out) = apply(
            &fixture,
            &fixture.request(false),
            &Processes(Vec::new()),
            &Daemon::stopping(),
            &Launches::default(),
        )
        .await;

        assert_eq!(code, 0, "{out}");
        assert!(fixture.installed_pwa("index.html").is_some());
        assert!(
            !fixture.to.join("evil").exists(),
            "another staged folder is not moved"
        );
    }

    #[tokio::test]
    async fn a_bundle_is_swapped_whole_and_its_old_copy_removed() {
        let bundle = Bundle::new("apply-update-bundle-swap");
        let daemon = Daemon::stopping();

        let (code, out) = apply_to(
            &bundle.result,
            &bundle.request(false),
            &Processes(Vec::new()),
            &daemon,
            &Launches::default(),
        )
        .await;

        assert_eq!(code, 0, "{out}");
        assert_eq!(out, format!("apply-update: updated to {VERSION}\n"));
        assert_eq!(
            bundle_client(&bundle.to).as_deref(),
            Some("new"),
            "the new app is in place"
        );
        assert!(!bundle.old().exists(), "the old app is removed");
        assert!(!bundle.from.exists(), "the staged app is moved away");
        assert_eq!(daemon.stops.get(), 1);
    }

    #[tokio::test]
    async fn a_failed_bundle_rename_puts_the_old_app_back() {
        let bundle = Bundle::new("apply-update-bundle-rollback");
        // The staged bundle is gone by the time the swap tries to move it in, as
        // when another process removed it after the command line was checked.
        std::fs::remove_dir_all(&bundle.from).expect("remove the staged app");

        let (code, out) = apply_to(
            &bundle.result,
            &bundle.request(false),
            &Processes(Vec::new()),
            &Daemon::stopping(),
            &Launches::default(),
        )
        .await;

        assert_eq!(code, 5, "{out}");
        assert!(
            out.starts_with(&format!(
                "apply-update: could not replace {}: ",
                bundle.to.display()
            )),
            "{out}"
        );
        assert!(out.ends_with("; the old app was put back\n"), "{out}");
        assert_eq!(
            bundle_client(&bundle.to).as_deref(),
            Some("old"),
            "the old app is back"
        );
        assert!(!bundle.old().exists(), "nothing was left aside");
        assert_eq!(bundle.result_json()["ok"], false);
    }

    #[tokio::test]
    async fn a_translocated_bundle_is_refused_untouched() {
        let bundle = Bundle::translocated("apply-update-bundle-translocated");
        let daemon = Daemon::stopping();

        let (code, out) = apply_to(
            &bundle.result,
            &bundle.request(false),
            &Processes(Vec::new()),
            &daemon,
            &Launches::default(),
        )
        .await;

        assert_eq!(code, 5, "{out}");
        let message = "rustling-tulip.app runs from a place the update cannot replace; \
                       move it to Applications and update again; nothing was changed";
        assert_eq!(out, format!("apply-update: {message}\n"));
        assert_eq!(daemon.stops.get(), 0, "the daemon was left running");
        assert_eq!(
            bundle_client(&bundle.to).as_deref(),
            Some("old"),
            "the app is untouched"
        );
        assert_eq!(
            bundle_client(&bundle.from).as_deref(),
            Some("new"),
            "the staged app is untouched"
        );
        let parent = bundle.to.parent().expect("the bundle has a parent");
        assert!(
            !parent.join(UPDATE_PROBE).exists(),
            "the probe was removed again"
        );
    }

    #[tokio::test]
    async fn a_client_running_inside_the_bundle_blocks_the_swap() {
        let bundle = Bundle::new("apply-update-bundle-in-use");
        let running = Processes(vec![RunningExe {
            pid: 4242,
            exe: bundle_executables(&bundle.to).join(CLIENT_EXE),
        }]);

        let (code, out) = apply_to(
            &bundle.result,
            &bundle.request(false),
            &running,
            &Daemon::stopping(),
            &Launches::default(),
        )
        .await;

        assert_eq!(code, 3, "{out}");
        let message = format!(
            "{CLIENT_EXE} is still running from the install folder (pid 4242); nothing was changed"
        );
        assert_eq!(out, format!("apply-update: {message}\n"));
        assert_eq!(
            bundle_client(&bundle.to).as_deref(),
            Some("old"),
            "the app is untouched"
        );
        assert_eq!(
            bundle_client(&bundle.from).as_deref(),
            Some("new"),
            "the staged app is untouched"
        );
    }

    #[tokio::test]
    async fn bundle_relaunch_runs_the_client_inside_the_bundle() {
        let bundle = Bundle::new("apply-update-bundle-relaunch");
        let launches = Launches::default();

        let (code, out) = apply_to(
            &bundle.result,
            &bundle.request(true),
            &Processes(Vec::new()),
            &Daemon::stopping(),
            &launches,
        )
        .await;

        assert_eq!(code, 0, "{out}");
        let client = bundle_executables(&bundle.to).join(CLIENT_EXE);
        let args = ["--updated-from", "0.1.0"].map(OsString::from).to_vec();
        assert_eq!(*launches.0.borrow(), vec![(client, args)]);
    }

    #[tokio::test]
    async fn restart_daemon_relaunches_a_client_running_inside_a_bundle() {
        let bundle = Bundle::new("apply-update-bundle-restart-daemon");
        let launches = Launches::default();
        let client = bundle_executables(&bundle.to).join(CLIENT_EXE);

        let (code, out) = apply_to(
            &bundle.result,
            &bundle.request_with(false, true),
            &Processes(vec![RunningExe {
                pid: std::process::id().wrapping_add(1),
                exe: client.clone(),
            }]),
            &Daemon::stopping(),
            &launches,
        )
        .await;

        assert_eq!(code, 3, "{out}");
        let args = ["--updated-from", "0.1.0"].map(OsString::from).to_vec();
        assert_eq!(*launches.0.borrow(), vec![(client, args)]);
    }

    #[test]
    fn a_bundle_to_needs_a_bundle_from() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        let err = parse(args(&["--to", "C:/Apps/X.app", "--from", "C:/Apps/folder"]))
            .expect_err("a folder cannot be staged into an .app bundle");
        assert_eq!(
            err.to_string(),
            "--from must be a .app bundle when --to is one"
        );
        for (to, from) in [
            ("X.app", "Stage.app"),
            ("X.APP", "Stage.app"),
            ("X.app", "Stage.APP"),
        ] {
            assert!(
                parse(args(&["--to", to, "--from", from])).is_ok(),
                "{to} from {from}"
            );
        }
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
            &Processes(vec![RunningExe {
                pid: 4242,
                exe: fixture.to.join(TRACER),
            }]),
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

    #[tokio::test]
    async fn restart_daemon_starts_the_daemon_when_no_client_ran() {
        let fixture = Fixture::new("apply-update-restart-daemon");
        let launches = Launches::default();
        let (code, out) = apply(
            &fixture,
            &fixture.request_with(false, true),
            &Processes(Vec::new()),
            &Daemon::stopping(),
            &launches,
        )
        .await;
        assert_eq!(code, 0, "{out}");
        assert_eq!(
            *launches.0.borrow(),
            vec![(
                fixture.to.join(DAEMON_EXE),
                vec![OsString::from("--detach")]
            )]
        );
    }

    #[tokio::test]
    async fn restart_daemon_relaunches_a_client_that_was_running() {
        let expected_args = ["--updated-from", "0.1.0"].map(OsString::from).to_vec();
        let fixture = Fixture::new("apply-update-restart-daemon-client");
        let launches = Launches::default();
        let (code, out) = apply(
            &fixture,
            &fixture.request_with(false, true),
            &Processes(vec![RunningExe {
                pid: std::process::id().wrapping_add(1),
                exe: fixture.to.join(CLIENT_EXE),
            }]),
            &Daemon::stopping(),
            &launches,
        )
        .await;
        assert_eq!(code, 3, "{out}");
        assert_eq!(
            *launches.0.borrow(),
            vec![(fixture.to.join(CLIENT_EXE), expected_args)]
        );
    }

    #[tokio::test]
    async fn restart_daemon_starts_the_daemon_when_the_client_launch_fails() {
        let expected_args = ["--updated-from", "0.1.0"].map(OsString::from).to_vec();
        let fixture = Fixture::new("apply-update-restart-daemon-failed-client");
        let launches = FailingClientLaunches::default();
        let (code, out) = apply(
            &fixture,
            &fixture.request_with(true, true),
            &Processes(Vec::new()),
            &Daemon::stopping(),
            &launches,
        )
        .await;
        assert_eq!(code, 0, "{out}");
        assert_eq!(
            *launches.0.borrow(),
            vec![
                (fixture.to.join(CLIENT_EXE), expected_args),
                (
                    fixture.to.join(DAEMON_EXE),
                    vec![OsString::from("--detach")]
                ),
            ]
        );
    }

    #[test]
    fn restart_daemon_is_parsed() {
        let parsed = parse(["--from", "a", "--to", "b", "--restart-daemon"].map(OsString::from))
            .expect("the flags parse");
        assert!(parsed.restart_daemon);
        let parsed =
            parse(["--from", "a", "--to", "b"].map(OsString::from)).expect("the flags parse");
        assert!(!parsed.restart_daemon);
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

    #[test]
    fn version_requested_matches_the_short_flag() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        for list in [&["-V"][..], &["--version"][..]] {
            assert!(version_requested(args(list)), "{list:?}");
        }
        for list in [&["-v"][..], &["--ver"][..], &[][..], &["apply-update"][..]] {
            assert!(!version_requested(args(list)), "{list:?}");
        }
    }
}
