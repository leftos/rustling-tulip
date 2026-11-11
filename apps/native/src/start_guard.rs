//! The start guard: marks each run in the guard file until it ends cleanly, so
//! a start after any crash (a panic, a hard fault, a kill from Task Manager or
//! a power cut, at any point in a run) asks whether to open the tabs, and keeps
//! dated copies of what a safe start sets aside. Plain Rust with no GPUI: the
//! panic hook calls into it while GPUI's own state cannot be trusted.
//!
//! [`begin`] marks the run as starting, [`mark_stable`] as past the risky part
//! of its start, and [`mark_clean_end`] clears the mark at every clean end (a
//! quit, an update's restart, the end of the OS session). Only a run that died
//! before stable counts towards the third start's reset of `native-ui.json`.
//! Nothing here panics: a failed write logs and the start carries on.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use protocol::TabEntry;
use serde::{Deserialize, Serialize};

/// The sidebar layout file, in the client's config dir.
pub const UI_FILE: &str = "native-ui.json";
/// The guard's file in the config dir.
pub const GUARD_FILE: &str = "start-guard.json";
/// How long after its first tab list from This PC a run counts as past the
/// risky part of a start.
pub const STABLE_AFTER: Duration = Duration::from_secs(60);
/// How long a cloaked run waits before it counts as stable: long enough that a
/// stalled machine never lets a driven spec's panic land after it.
const CLOAKED_STABLE_AFTER: Duration = Duration::from_secs(3600);

/// How long this run waits after its first tab list before it counts as stable.
#[must_use]
pub fn stable_after(cloaked: bool) -> Duration {
    if cloaked {
        CLOAKED_STABLE_AFTER
    } else {
        STABLE_AFTER
    }
}
/// How long a run with no tab list from its daemon counts as starting.
const NO_TABS_STABLE_AFTER: Duration = Duration::from_secs(600);
/// The same for a cloaked run, which a loaded machine can hold up for long.
const CLOAKED_NO_TABS_STABLE_AFTER: Duration = Duration::from_secs(7200);

/// How long a run waits for a tab list before it counts as stable without one.
#[must_use]
pub fn no_tabs_stable_after(cloaked: bool) -> Duration {
    if cloaked {
        CLOAKED_NO_TABS_STABLE_AFTER
    } else {
        NO_TABS_STABLE_AFTER
    }
}
/// The prefix of a kept copy of `native-ui.json`.
pub const UI_COPY_PREFIX: &str = "native-ui.json.corrupt-";
/// The prefix of a kept copy of the tab list a safe start closed.
pub const LAYOUT_COPY_PREFIX: &str = "layout-set-aside-";
/// The title of the prompt a start after a crashed one raises.
pub const PROMPT_TITLE: &str = "The last start crashed";
/// The prompt's button that opens the tabs as they were.
pub const PROMPT_OPEN: &str = "Open tabs";
/// The prompt's button that closes the tabs, keeping a copy.
pub const PROMPT_WITHOUT: &str = "Start without tabs";
/// The toast's title once a start went on without the tabs.
pub const STARTED_WITHOUT_TITLE: &str = "Started without your tabs";
/// The toast's title when a start with no tabs to ask about reset the UI.
pub const UI_RESET_TITLE: &str = "Window and sidebar settings were reset";

/// How many copies of one prefix are kept; the older ones go.
const KEPT_COPIES: usize = 3;
/// The most of a panic's message the message box shows, in characters.
const PANIC_MESSAGE_CAP: usize = 600;

/// Whether this run is past the risky part of its start.
static STABLE: AtomicBool = AtomicBool::new(false);
/// Whether this process has already counted a panic.
static PANIC_RECORDED: AtomicBool = AtomicBool::new(false);
/// Whether this process has already shown the crash message.
static PANIC_BOX_SHOWN: AtomicBool = AtomicBool::new(false);
/// Whether this run has had a tab list from its daemon.
static TABS_SEEN: AtomicBool = AtomicBool::new(false);
/// Whether a safe start still has to act on This PC's first tab list.
static SAFE_START_PENDING: AtomicBool = AtomicBool::new(false);
/// Whether this run's main window failed to open.
static WINDOW_FAILED: AtomicBool = AtomicBool::new(false);

/// How a start opens, from how many starts before it panicked: normally after
/// none, asking whether to open the saved tabs after one or more, and with
/// `native-ui.json` set aside too after three or more.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum StartMode {
    /// Open the saved layout as it is.
    #[default]
    Normal,
    /// Ask whether to open the saved tabs or start without them.
    Ask,
    /// Also sets `native-ui.json` aside; `kept_ui` is the copy made of it.
    AskAndResetUi { kept_ui: Option<PathBuf> },
}

impl StartMode {
    /// The copy this start made of `native-ui.json`, if it made one.
    #[must_use]
    pub fn kept_ui(&self) -> Option<&Path> {
        match self {
            Self::AskAndResetUi { kept_ui } => kept_ui.as_deref(),
            Self::Normal | Self::Ask => None,
        }
    }
}

/// What the prompt says on a start after a crashed one, once This PC's tab
/// list has come.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartPrompt {
    kept_ui: Option<PathBuf>,
}

impl StartPrompt {
    /// The prompt for a start in `mode`.
    #[must_use]
    pub fn new(mode: &StartMode) -> Self {
        Self {
            kept_ui: mode.kept_ui().map(Path::to_path_buf),
        }
    }

    /// The lines under the title, in drawing order.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![
            "Open your tabs as they were, or start without them? Your sessions keep running \
             either way, and starting without them keeps a copy of the layout."
                .to_owned(),
        ];
        if let Some(path) = &self.kept_ui {
            lines.push(format!(
                "Your window and sidebar settings went back to their defaults too. A copy is in \
                 {}.",
                path.display()
            ));
        }
        lines
    }
}

/// The detail of the toast a start without the tabs shows, naming `copy`.
#[must_use]
pub fn started_without_detail(copy: &Path) -> String {
    format!("A copy of the layout is in {}.", copy.display())
}

/// The detail of the toast a start with no tabs to ask about shows when it
/// set `native-ui.json` aside to `copy`.
#[must_use]
pub fn ui_reset_detail(copy: &Path) -> String {
    format!(
        "The last starts crashed. A copy of your settings is in {}.",
        copy.display()
    )
}

/// The ids of the tabs a safe start closed, so they are never drawn again
/// until the daemon no longer has them.
#[derive(Debug, Default)]
pub struct ClosedTabs(HashSet<String>);

impl ClosedTabs {
    /// Holds `id` as closed by the safe start.
    pub fn note(&mut self, id: impl Into<String>) {
        self.0.insert(id.into());
    }

    /// Releases `id`; the daemon no longer has a tab with it.
    pub fn forget(&mut self, id: &str) {
        self.0.remove(id);
    }

    /// Whether the safe start closed `id`.
    #[must_use]
    pub fn holds(&self, id: &str) -> bool {
        self.0.contains(id)
    }

    /// Takes every held tab out of `tabs` and returns its id; a held id `tabs`
    /// no longer lists is forgotten, since the daemon no longer has it.
    pub fn filter_tabs(&mut self, tabs: &mut Vec<TabEntry>) -> Vec<String> {
        let listed: HashSet<&str> = tabs.iter().map(|tab| tab.id.as_str()).collect();
        self.0.retain(|id| listed.contains(id.as_str()));
        let closed: Vec<String> = tabs
            .iter()
            .filter(|tab| self.holds(&tab.id))
            .map(|tab| tab.id.clone())
            .collect();
        tabs.retain(|tab| !self.holds(&tab.id));
        closed
    }
}

/// The guard file's shape.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct Guard {
    failed_starts: u32,
    last_panic: Option<String>,
    /// Whether a start has already set `native-ui.json` aside.
    #[serde(default)]
    ui_set_aside: bool,
    /// The run under way, or the last one if it never ended cleanly; none
    /// after a clean end and in a file from an earlier release.
    #[serde(default)]
    run: Option<RunMark>,
}

/// A run the guard file marks: how far it got, and its process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct RunMark {
    state: RunState,
    pid: u32,
}

/// How far a marked run got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RunState {
    /// Still in the risky part of its start.
    Starting,
    /// Past it: a crash from here asks, but does not count towards the reset.
    Stable,
    /// Ended cleanly while the prompt was up: the next start asks again and
    /// counts nothing.
    Asking,
    /// Panicked past stable: dead whatever its pid says, so the next start
    /// asks, counting nothing.
    Crashed,
    /// A state from a later release: read as stable.
    #[serde(other)]
    Unknown,
}

impl RunMark {
    /// This process's mark in `state`.
    fn own(state: RunState) -> Self {
        Self {
            state,
            pid: std::process::id(),
        }
    }

    /// Whether this process wrote the mark.
    fn is_own(self) -> bool {
        self.pid == std::process::id()
    }
}

/// Whether `run` is this process's mark, or no mark at all, which this
/// process may then write.
fn ours_or_none(run: Option<RunMark>) -> bool {
    run.is_none_or(RunMark::is_own)
}

/// The guard in `dir`; a fresh one when the file is missing or unusable.
fn read_guard(dir: &Path) -> Guard {
    let path = dir.join(GUARD_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Guard::default(),
        Err(err) => {
            tracing::warn!("reading {}: {err}; counting from zero", path.display());
            return Guard::default();
        }
    };
    serde_json::from_str(&text).unwrap_or_else(|err| {
        tracing::warn!("{} is not valid: {err}; counting from zero", path.display());
        Guard::default()
    })
}

/// Writes `guard` to `dir` through a temporary file, flushed to the disk
/// before the rename when `durable`, so a power cut cannot lose it; a failure
/// logs.
fn write_guard(dir: &Path, guard: &Guard, durable: bool) {
    use std::io::Write as _;
    let write = || -> anyhow::Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(GUARD_FILE);
        let tmp = dir.join(format!("{GUARD_FILE}.tmp"));
        let json = serde_json::to_vec_pretty(guard).context("serializing the start guard")?;
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(&json)
            .with_context(|| format!("writing {}", tmp.display()))?;
        if durable {
            file.sync_all()
                .with_context(|| format!("flushing {}", tmp.display()))?;
        }
        drop(file);
        std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))
    };
    if let Err(err) = write() {
        tracing::warn!("saving the start guard: {err:#}");
    }
}

/// How a start opens after `failed_starts` failed starts.
fn mode_for(failed_starts: u32) -> StartMode {
    match failed_starts {
        0 => StartMode::Normal,
        1 | 2 => StartMode::Ask,
        _ => StartMode::AskAndResetUi { kept_ui: None },
    }
}

/// How a start opens from the guard the last run left, and the guard it
/// writes back. A run still marked as starting died before stable: it counts
/// and asks. One marked stable, asking, crashed or in an unknown state asks
/// and counts nothing. A mark of a process `live` says still runs is a second
/// client's, unless it is marked crashed: it is left as it is, and this start
/// neither counts nor asks. With no mark, the count alone decides. The guard
/// written back marks this run as starting.
fn decide(guard: Guard, live: impl Fn(u32) -> bool) -> (StartMode, Guard) {
    let mut next = guard;
    let crashed = match next.run {
        Some(mark) if mark.state != RunState::Crashed && !mark.is_own() && live(mark.pid) => {
            return (StartMode::Normal, next);
        }
        Some(RunMark {
            state: RunState::Starting,
            ..
        }) => {
            next.failed_starts = next.failed_starts.saturating_add(1);
            true
        }
        Some(_) => true,
        None => false,
    };
    let asked = if crashed {
        next.failed_starts.max(1)
    } else {
        next.failed_starts
    };
    next.run = Some(RunMark::own(RunState::Starting));
    (mode_for(asked), next)
}

/// Reads the guard in `dir`, writes back that this run is starting, and says
/// how to open: normally after a clean end with no failed start, asking about
/// the tabs after a crashed run or one or two failed starts, and with
/// `native-ui.json` set aside too after three. A later start leaves the file
/// alone, since the safe start has saved defaults there by then. A run marked
/// by a second client that still runs is left alone.
#[must_use]
pub fn begin(dir: &Path) -> StartMode {
    let guard = read_guard(dir);
    let (mode, next) = decide(guard.clone(), process_alive);
    if next == guard {
        return mode;
    }
    write_guard(dir, &next, true);
    if next.failed_starts == 3 {
        return reset_ui_once(dir, next);
    }
    mode
}

/// Whether process `pid` still runs; a process this one may not open runs.
#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, STILL_ACTIVE};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if pid == 0 {
        return false;
    }
    // SAFETY: opens a query-only handle, closed below.
    let process = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(process) => process,
        Err(err) => return err.code() == ERROR_ACCESS_DENIED.to_hresult(),
    };
    let mut code = 0_u32;
    // SAFETY: the handle opened above and an out-pointer to a local.
    let read = unsafe { GetExitCodeProcess(process, &raw mut code) };
    // SAFETY: the handle opened above, closed once.
    let _ = unsafe { CloseHandle(process) };
    read.is_ok() && code == STILL_ACTIVE.0.cast_unsigned()
}

/// Whether process `pid` still runs; a process this one may not signal runs.
#[cfg(target_os = "macos")]
fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid == 0 {
        return false;
    }
    // SAFETY: signal 0 sends nothing; it only checks that `pid` exists.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Whether process `pid` still runs: unknown here, so no.
#[cfg(not(any(windows, target_os = "macos")))]
fn process_alive(_pid: u32) -> bool {
    false
}

/// The third start's reset: sets `native-ui.json` aside and marks the guard, so
/// no later start sets the defaults it saved aside again.
fn reset_ui_once(dir: &Path, guard: Guard) -> StartMode {
    if guard.ui_set_aside {
        return StartMode::AskAndResetUi { kept_ui: None };
    }
    let kept_ui = set_aside_ui_file(dir);
    // A set-aside that failed leaves the file in place, and marking the reset
    // done would skip it for good.
    if !dir.join(UI_FILE).exists() {
        write_guard(
            dir,
            &Guard {
                ui_set_aside: true,
                ..guard
            },
            true,
        );
    }
    StartMode::AskAndResetUi { kept_ui }
}

/// Moves `native-ui.json` in `dir` aside, returning its copy; a file that is
/// not there has none, and one that cannot be copied stays where it is.
fn set_aside_ui_file(dir: &Path) -> Option<PathBuf> {
    let path = dir.join(UI_FILE);
    if !path.exists() {
        return None;
    }
    let copy = keep_copy(&path, UI_COPY_PREFIX, Utc::now())
        .inspect_err(|err| tracing::warn!("keeping a copy of {}: {err:#}", path.display()))
        .ok()?;
    std::fs::remove_file(&path)
        .inspect_err(|err| tracing::warn!("removing {}: {err}", path.display()))
        .ok()?;
    Some(copy)
}

/// Records that this start panicked, once per process and only before
/// [`mark_stable`], and returns the failed-start count the guard file holds
/// afterwards. The count it adds makes the next start ask, so this run's mark
/// goes, or the next start would count the same run again. A panic after
/// stable counts nothing and marks the run crashed, so a start while the crash
/// message still holds this process open asks rather than taking it for a
/// second client.
#[must_use]
pub fn record_panic(dir: &Path, message: &str) -> u32 {
    if PANIC_RECORDED.swap(true, Ordering::SeqCst) {
        return read_guard(dir).failed_starts;
    }
    let mut guard = read_guard(dir);
    if is_stable() {
        if ours_or_none(guard.run) {
            guard.run = Some(RunMark::own(RunState::Crashed));
        }
    } else {
        guard.failed_starts = guard.failed_starts.saturating_add(1);
        guard.last_panic = Some(message.to_owned());
        if ours_or_none(guard.run) {
            guard.run = None;
        }
    }
    write_guard(dir, &guard, false);
    read_guard(dir).failed_starts
}

/// Removes the guard in `dir`, so the next start counts from zero; a removal
/// that fails for any reason but not-found writes a zero guard over it
/// instead, since a stale count would close tabs on a healthy next start.
fn forget_guard(dir: &Path, remove: impl FnOnce(&Path) -> std::io::Result<()>) {
    let path = dir.join(GUARD_FILE);
    match remove(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::warn!(
                "removing {}: {err}; counting from zero instead",
                path.display()
            );
            write_guard(dir, &Guard::default(), true);
        }
    }
}

/// This run is past the risky part: forget the failed starts, the last panic
/// and the reset mark, and mark the run stable, so a crash from here asks
/// without counting. A second client's mark stays.
pub fn mark_stable(dir: &Path) {
    STABLE.store(true, Ordering::SeqCst);
    let run = read_guard(dir).run;
    let run = if ours_or_none(run) {
        Some(RunMark::own(RunState::Stable))
    } else {
        run
    };
    write_guard(
        dir,
        &Guard {
            run,
            ..Guard::default()
        },
        false,
    );
}

/// The main window failed to open: the quit that follows is not a clean end,
/// so the run stays marked.
pub fn note_window_failed() {
    WINDOW_FAILED.store(true, Ordering::SeqCst);
}

/// This run ends cleanly: its mark goes, keeping the failed starts, and the
/// file goes once nothing is left in it. While a safe start still waits on
/// the prompt the run is marked as asking instead, so the next start asks
/// again without counting. A run whose window failed to open keeps its mark,
/// and a second client's mark stays.
pub fn mark_clean_end(dir: &Path) {
    if WINDOW_FAILED.load(Ordering::SeqCst) {
        return;
    }
    let mut guard = read_guard(dir);
    if ours_or_none(guard.run) {
        guard.run = safe_start_pending().then(|| RunMark::own(RunState::Asking));
    }
    if guard.failed_starts == 0 && guard.run.is_none() {
        forget_guard(dir, |path| std::fs::remove_file(path));
    } else {
        write_guard(dir, &guard, true);
    }
}

/// Whether this run is past the risky part of its start.
#[must_use]
pub fn is_stable() -> bool {
    STABLE.load(Ordering::SeqCst)
}

/// This run has a tab list from its daemon, so the no-tabs window is over and
/// a tab list arriving after it re-arms the start window.
pub fn tabs_arrived() {
    TABS_SEEN.store(true, Ordering::SeqCst);
    STABLE.store(false, Ordering::SeqCst);
}

/// Records that a safe start still has to act, so a quit before it does not
/// forget the count.
pub fn set_safe_start_pending(pending: bool) {
    SAFE_START_PENDING.store(pending, Ordering::SeqCst);
}

/// Whether a safe start still has to act on This PC's tab list.
#[must_use]
pub fn safe_start_pending() -> bool {
    SAFE_START_PENDING.load(Ordering::SeqCst)
}

/// Counts this run stable when no tab list ever came, keeping the count a
/// pending safe start still needs.
pub fn stable_without_tabs() {
    if !TABS_SEEN.load(Ordering::SeqCst) {
        STABLE.store(true, Ordering::SeqCst);
        // A tab list that arrived in between wins: its panic must still count.
        if TABS_SEEN.load(Ordering::SeqCst) {
            STABLE.store(false, Ordering::SeqCst);
        }
    }
}

/// The no-tabs window is over: when no tab list came, counts this run stable
/// and marks it so in the guard in `dir`, so a kill from here asks without
/// counting. A safe start still waiting on This PC's tab list keeps its count,
/// as a quit then does; otherwise the failed starts are forgotten.
pub fn mark_stable_without_tabs(dir: &Path) {
    if TABS_SEEN.load(Ordering::SeqCst) {
        return;
    }
    if !safe_start_pending() {
        mark_stable(dir);
        return;
    }
    STABLE.store(true, Ordering::SeqCst);
    let mut guard = read_guard(dir);
    if ours_or_none(guard.run) {
        guard.run = Some(RunMark::own(RunState::Stable));
        write_guard(dir, &guard, false);
    }
}

/// Whether this process should show the crash message: the first panic only,
/// since unwinding out of a Win32 callback runs the hook again.
#[must_use]
pub fn claim_panic_box() -> bool {
    !PANIC_BOX_SHOWN.swap(true, Ordering::SeqCst)
}

/// What the next start does after a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextStart {
    /// Ask whether to open the saved tabs.
    AsksAboutTabs,
    /// Ask about the saved tabs, with `native-ui.json` set aside.
    AsksAndResetsUi,
    /// The panic changes nothing about the next start: there is no config dir
    /// to record it in.
    Unaffected,
}

/// What a panic means for the next start, from whether this run was still
/// starting and how many starts in a row have now failed.
#[must_use]
pub fn next_start_after(during_start: bool, failed_starts: u32) -> NextStart {
    if !during_start {
        // The run is marked crashed, so the next start asks without counting.
        return NextStart::AsksAboutTabs;
    }
    match failed_starts {
        3 => NextStart::AsksAndResetsUi,
        _ => NextStart::AsksAboutTabs,
    }
}

/// The message box's text for a panic.
#[must_use]
pub fn panic_text(message: &str, next: NextStart, log_path: &Path) -> String {
    let message: String = message.chars().take(PANIC_MESSAGE_CAP).collect();
    let log = log_path.display();
    match next {
        NextStart::AsksAboutTabs => format!(
            "Rustling Tulip crashed while starting.\n\n{message}\n\nStart Rustling Tulip again \
             to carry on. The next start asks whether to open your tabs. Your sessions keep \
             running. The log is at {log}."
        ),
        NextStart::AsksAndResetsUi => format!(
            "Rustling Tulip crashed while starting.\n\n{message}\n\nThe next start asks whether \
             to open your tabs, and your window and sidebar settings go back to their defaults, \
             with a copy of them kept. Your sessions keep running. The log is at {log}."
        ),
        NextStart::Unaffected => format!(
            "Rustling Tulip crashed.\n\n{message}\n\nYour sessions keep running. Start Rustling \
             Tulip again to carry on. The log is at {log}."
        ),
    }
}

/// Copies `source` to `<prefix><UTC stamp>` beside it and keeps the three
/// newest of that prefix.
///
/// # Errors
/// When `source` has no folder or cannot be copied.
pub fn keep_copy(source: &Path, prefix: &str, at: DateTime<Utc>) -> anyhow::Result<PathBuf> {
    let folder = source.parent().context("the file has no folder")?;
    let target = free_copy_path(folder, prefix, at);
    std::fs::copy(source, &target)
        .with_context(|| format!("copying {} aside", source.display()))?;
    prune_copies(folder, prefix, &target);
    Ok(target)
}

/// Writes `bytes` to `<dir>/<prefix><UTC stamp>` and keeps the three newest of
/// that prefix.
///
/// # Errors
/// When the folder cannot be made or the file cannot be written.
pub fn keep_bytes(
    dir: &Path,
    prefix: &str,
    bytes: &[u8],
    at: DateTime<Utc>,
) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let target = free_copy_path(dir, prefix, at);
    std::fs::write(&target, bytes).with_context(|| format!("writing {}", target.display()))?;
    prune_copies(dir, prefix, &target);
    Ok(target)
}

/// The first free `<prefix><stamp>` path in `folder`: the stamp from `at`, then
/// `-2`, `-3`, … while a copy of that name is already there.
fn free_copy_path(folder: &Path, prefix: &str, at: DateTime<Utc>) -> PathBuf {
    let stamp = at.format("%Y%m%dT%H%M%SZ");
    let first = folder.join(format!("{prefix}{stamp}"));
    if !first.exists() {
        return first;
    }
    let mut n = 2_u32;
    loop {
        let candidate = folder.join(format!("{prefix}{stamp}-{n}"));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

/// Deletes every copy of `prefix` in `folder` but the newest few, judged by
/// name (the stamp sorts), never touching `kept`, the copy just made. A copy
/// that will not go stays with a warning.
fn prune_copies(folder: &Path, prefix: &str, kept: &Path) {
    let entries = match std::fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::warn!("listing {}: {err}", folder.display());
            return;
        }
    };
    let mut copies: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(prefix))
        })
        .collect();
    copies.sort();
    let excess = copies.len().saturating_sub(KEPT_COPIES);
    for path in copies.into_iter().take(excess) {
        if path == kept {
            continue;
        }
        if let Err(err) = std::fs::remove_file(&path) {
            tracing::warn!("removing the old copy {}: {err}", path.display());
        }
    }
}

/// Shows `text` in a message box and waits for it to be closed: `MessageBoxW`
/// on Windows, `osascript` on macOS, nothing elsewhere. A failure to show logs.
#[cfg(windows)]
pub fn show_panic_box(text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{
        MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MessageBoxW,
    };
    use windows::core::HSTRING;
    // SAFETY: both strings outlive the call, and no owner window is passed.
    let pressed = unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from("Rustling Tulip"),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        )
    };
    if pressed.0 == 0 {
        tracing::warn!(
            "showing the crash message: {}",
            windows::core::Error::from_win32()
        );
    }
}

/// Shows `text` in a message box and waits for it to be closed: `MessageBoxW`
/// on Windows, `osascript` on macOS, nothing elsewhere. A failure to show logs.
#[cfg(target_os = "macos")]
pub fn show_panic_box(text: &str) {
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("display alert \"Rustling Tulip\" message \"{escaped}\" as critical");
    let shown = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .status();
    match shown {
        Ok(status) if status.success() => {}
        Ok(status) => tracing::warn!("showing the crash message: osascript ended with {status}"),
        Err(err) => tracing::warn!("showing the crash message: {err}"),
    }
}

/// Shows `text` in a message box and waits for it to be closed: `MessageBoxW`
/// on Windows, `osascript` on macOS, nothing elsewhere. A failure to show logs.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn show_panic_box(_text: &str) {}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;
    use serde_json::{Value, json};

    /// A scratch folder of this test's own, gone when dropped.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("rt-start-guard-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create the test dir");
            Self(path)
        }

        fn seed(&self, text: &str) {
            std::fs::write(self.0.join(GUARD_FILE), text).expect("seed the guard");
        }

        fn guard(&self) -> Value {
            let text = std::fs::read_to_string(self.0.join(GUARD_FILE)).expect("read the guard");
            serde_json::from_str(&text).expect("the guard is valid JSON")
        }

        fn names(&self, prefix: &str) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.0)
                .expect("list the test dir")
                .map(|entry| {
                    entry
                        .expect("an entry")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .filter(|name| name.starts_with(prefix))
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn seeded(tag: &str, failed: u32) -> TestDir {
        let dir = TestDir::new(tag);
        dir.seed(&json!({"failed_starts": failed, "last_panic": "x"}).to_string());
        dir
    }

    fn at(second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 8, 12, 0, second)
            .single()
            .expect("a valid instant")
    }

    #[test]
    fn no_guard_file_is_a_normal_start() {
        let dir = TestDir::new("none");
        assert_eq!(begin(&dir.0), StartMode::Normal);
        assert_eq!(dir.guard()["failed_starts"], json!(0));
    }

    /// A pid no process has.
    const DEAD: u32 = u32::MAX;

    /// The marker this process writes in `state`.
    fn own_mark(state: &str) -> Value {
        json!({"state": state, "pid": std::process::id()})
    }

    /// Seeds a guard of `failed` starts whose run, in `state`, was `pid`'s.
    fn seed_run(dir: &TestDir, failed: u32, state: &str, pid: u32) {
        dir.seed(
            &json!({
                "failed_starts": failed,
                "last_panic": null,
                "ui_set_aside": false,
                "run": {"state": state, "pid": pid},
            })
            .to_string(),
        );
    }

    #[test]
    fn a_run_killed_before_stable_asks_and_counts() {
        let dir = TestDir::new("killed");
        seed_run(&dir, 0, "starting", DEAD);
        assert_eq!(begin(&dir.0), StartMode::Ask);
        assert_eq!(dir.guard()["failed_starts"], json!(1));
        assert_eq!(dir.guard()["run"], own_mark("starting"));
    }

    #[test]
    fn a_run_killed_after_stable_asks_without_counting() {
        let dir = TestDir::new("killed-stable");
        seed_run(&dir, 0, "stable", DEAD);
        assert_eq!(begin(&dir.0), StartMode::Ask);
        assert_eq!(dir.guard()["failed_starts"], json!(0), "the count stays");
        assert_eq!(dir.guard()["run"], own_mark("starting"));
    }

    #[test]
    fn a_clean_end_clears_the_marker_and_the_file() {
        let dir = TestDir::new("clean-end");
        assert_eq!(begin(&dir.0), StartMode::Normal);
        mark_stable(&dir.0);

        mark_clean_end(&dir.0);

        assert!(!dir.0.join(GUARD_FILE).exists(), "nothing is left to ask");
        assert_eq!(begin(&dir.0), StartMode::Normal, "the next start is normal");

        let counted = seeded("clean-end-counted", 1);
        assert_eq!(begin(&counted.0), StartMode::Ask);
        mark_clean_end(&counted.0);
        assert_eq!(counted.guard()["run"], Value::Null, "the marker goes");
        assert_eq!(
            counted.guard()["failed_starts"],
            json!(1),
            "the count stays"
        );
    }

    #[test]
    fn a_clean_end_with_the_prompt_up_keeps_the_marker() {
        let dir = seeded("clean-end-prompt", 1);
        assert_eq!(begin(&dir.0), StartMode::Ask);
        set_safe_start_pending(true);

        mark_clean_end(&dir.0);

        assert_eq!(dir.guard()["run"], own_mark("asking"));
        assert_eq!(dir.guard()["failed_starts"], json!(1));
    }

    #[test]
    fn quitting_with_the_prompt_up_asks_next_time_without_counting() {
        let dir = seeded("prompt-quit", 2);
        assert_eq!(begin(&dir.0), StartMode::Ask);
        set_safe_start_pending(true);

        mark_clean_end(&dir.0);

        assert_eq!(
            dir.guard(),
            json!({
                "failed_starts": 2,
                "last_panic": "x",
                "ui_set_aside": false,
                "run": own_mark("asking"),
            })
        );
        assert_eq!(begin(&dir.0), StartMode::Ask, "the next start asks");
        assert_eq!(dir.guard()["failed_starts"], json!(2), "and counts nothing");
    }

    #[test]
    fn a_window_that_failed_to_open_keeps_the_mark() {
        let dir = TestDir::new("window-failed");
        assert_eq!(begin(&dir.0), StartMode::Normal);
        note_window_failed();

        mark_clean_end(&dir.0);

        assert_eq!(dir.guard()["run"], own_mark("starting"));
    }

    #[test]
    fn a_panic_after_stable_marks_the_run_crashed() {
        let dir = seeded("stable-panic", 0);
        assert_eq!(begin(&dir.0), StartMode::Normal);
        mark_stable(&dir.0);

        assert_eq!(record_panic(&dir.0, "late"), 0, "the count stays");

        assert_eq!(dir.guard()["run"], own_mark("crashed"));
    }

    #[test]
    fn a_crashed_mark_of_a_live_pid_still_asks() {
        let dir = TestDir::new("crashed-live");
        seed_run(&dir, 0, "crashed", 4242);

        let (mode, next) = decide(read_guard(&dir.0), |_| true);

        assert_eq!(
            mode,
            StartMode::Ask,
            "a crashed run is dead whatever its pid"
        );
        assert_eq!(next.failed_starts, 0, "and counts nothing");
        assert_eq!(next.run, Some(RunMark::own(RunState::Starting)));
    }

    #[test]
    fn an_unknown_run_state_asks_without_counting() {
        let dir = TestDir::new("unknown-state");
        seed_run(&dir, 1, "from_a_later_release", DEAD);

        assert_eq!(begin(&dir.0), StartMode::Ask);

        assert_eq!(dir.guard()["failed_starts"], json!(1), "the count stays");
        assert_eq!(dir.guard()["run"], own_mark("starting"));
    }

    #[test]
    fn a_run_stable_without_tabs_marks_the_run_stable() {
        let dir = seeded("no-tabs-stable", 1);
        assert_eq!(begin(&dir.0), StartMode::Ask);
        set_safe_start_pending(true);

        mark_stable_without_tabs(&dir.0);

        assert!(is_stable());
        assert_eq!(dir.guard()["run"], own_mark("stable"));
        assert_eq!(
            dir.guard()["failed_starts"],
            json!(1),
            "a safe start still waiting keeps its count"
        );

        set_safe_start_pending(false);
        mark_stable_without_tabs(&dir.0);
        assert_eq!(dir.guard()["failed_starts"], json!(0), "then it goes");
        assert_eq!(dir.guard()["run"], own_mark("stable"));
    }

    #[test]
    fn three_runs_killed_before_stable_reset_the_ui_once() {
        let dir = TestDir::new("killed-three");
        let ui = dir.0.join(UI_FILE);
        std::fs::write(&ui, "real").expect("seed native-ui.json");

        assert_eq!(begin(&dir.0), StartMode::Normal, "the first run");
        assert_eq!(begin(&dir.0), StartMode::Ask, "after one kill");
        assert_eq!(begin(&dir.0), StartMode::Ask, "after two kills");
        let third = begin(&dir.0);

        let names = dir.names(UI_COPY_PREFIX);
        assert_eq!(names.len(), 1, "{names:?}");
        assert_eq!(
            third,
            StartMode::AskAndResetUi {
                kept_ui: Some(dir.0.join(&names[0]))
            }
        );
        std::fs::write(&ui, "defaults").expect("the defaults the reset saved");
        assert_eq!(begin(&dir.0), StartMode::AskAndResetUi { kept_ui: None });
        assert_eq!(dir.names(UI_COPY_PREFIX), names, "still the one copy");
        assert_eq!(
            std::fs::read_to_string(&ui).expect("read the file"),
            "defaults"
        );
    }

    #[test]
    fn three_runs_killed_after_stable_never_reset_the_ui() {
        let dir = TestDir::new("killed-stable-three");
        let ui = dir.0.join(UI_FILE);
        std::fs::write(&ui, "real").expect("seed native-ui.json");

        assert_eq!(begin(&dir.0), StartMode::Normal);
        for _ in 0..4 {
            mark_stable(&dir.0);
            assert_eq!(begin(&dir.0), StartMode::Ask, "a killed run asks");
        }

        assert_eq!(dir.guard()["failed_starts"], json!(0));
        assert_eq!(dir.names(UI_COPY_PREFIX), Vec::<String>::new());
        assert_eq!(std::fs::read_to_string(&ui).expect("read the file"), "real");
    }

    #[test]
    fn a_marker_of_a_live_process_is_ignored() {
        let dir = TestDir::new("live");
        seed_run(&dir, 1, "starting", 4242);
        let guard = read_guard(&dir.0);

        let (mode, next) = decide(guard, |pid| pid == 4242);

        assert_eq!(mode, StartMode::Normal, "another client's run asks nothing");
        assert_eq!(next.failed_starts, 1, "and counts nothing");
        assert_eq!(
            next.run,
            Some(RunMark {
                state: RunState::Starting,
                pid: 4242
            }),
            "its marker stays"
        );
    }

    #[test]
    fn a_guard_without_a_run_marker_reads_as_none() {
        for (tag, text) in [
            (
                "v0-11-full",
                r#"{"failed_starts":1,"last_panic":"boom","ui_set_aside":false}"#,
            ),
            ("v0-11-short", r#"{"failed_starts":1,"last_panic":"boom"}"#),
        ] {
            let dir = TestDir::new(tag);
            dir.seed(text);

            let guard = read_guard(&dir.0);
            assert_eq!(guard.run, None, "{text}");
            assert_eq!(guard.failed_starts, 1, "{text}");

            let (mode, next) = decide(guard, |_| true);
            assert_eq!(mode, StartMode::Ask, "the old count still asks: {text}");
            assert_eq!(next.failed_starts, 1, "and no run is counted: {text}");
        }
    }

    #[test]
    fn one_failed_start_asks_about_the_tabs() {
        let dir = seeded("one", 1);
        assert_eq!(begin(&dir.0), StartMode::Ask);
        assert_eq!(dir.guard()["failed_starts"], json!(1));
    }

    #[test]
    fn two_failed_starts_ask_about_the_tabs() {
        let dir = seeded("two", 2);
        let ui = dir.0.join(UI_FILE);
        std::fs::write(&ui, r#"{"a":1}"#).expect("seed native-ui.json");

        assert_eq!(begin(&dir.0), StartMode::Ask);

        assert_eq!(
            dir.names(UI_COPY_PREFIX),
            Vec::<String>::new(),
            "the file gets no copy yet"
        );
        assert_eq!(
            std::fs::read_to_string(&ui).expect("read the file"),
            r#"{"a":1}"#,
            "the file is untouched"
        );
    }

    #[test]
    fn three_failed_starts_also_reset_the_ui() {
        let dir = seeded("three", 3);
        let ui = dir.0.join(UI_FILE);
        std::fs::write(&ui, r#"{"a":1}"#).expect("seed native-ui.json");

        let mode = begin(&dir.0);

        let names = dir.names(UI_COPY_PREFIX);
        assert_eq!(names.len(), 1, "{names:?}");
        let copy = dir.0.join(&names[0]);
        assert_eq!(
            mode,
            StartMode::AskAndResetUi {
                kept_ui: Some(copy.clone())
            }
        );
        assert_eq!(
            std::fs::read_to_string(&copy).expect("read the copy"),
            r#"{"a":1}"#
        );
        assert!(!ui.exists(), "the file moved aside");
    }

    #[test]
    fn a_reset_with_no_ui_file_keeps_nothing() {
        let dir = seeded("nofile", 3);
        assert_eq!(begin(&dir.0), StartMode::AskAndResetUi { kept_ui: None });
        assert_eq!(dir.names(UI_COPY_PREFIX), Vec::<String>::new());
    }

    #[test]
    fn a_later_reset_keeps_the_first_copy() {
        let dir = seeded("later", 3);
        let ui = dir.0.join(UI_FILE);
        std::fs::write(&ui, r#"{"a":1}"#).expect("seed native-ui.json");

        let first = begin(&dir.0);

        let names = dir.names(UI_COPY_PREFIX);
        assert_eq!(names.len(), 1, "{names:?}");
        let copy = dir.0.join(&names[0]);
        assert_eq!(
            first,
            StartMode::AskAndResetUi {
                kept_ui: Some(copy.clone())
            }
        );

        std::fs::write(&ui, r#"{"b":2}"#).expect("a new native-ui.json");
        dir.seed(&json!({"failed_starts": 4, "last_panic": "x"}).to_string());
        let later = begin(&dir.0);

        assert_eq!(
            later,
            StartMode::AskAndResetUi { kept_ui: None },
            "the fourth start sets nothing aside"
        );
        assert_eq!(dir.names(UI_COPY_PREFIX), names, "still the one copy");
        assert_eq!(
            std::fs::read_to_string(&copy).expect("read the copy"),
            r#"{"a":1}"#
        );
        assert_eq!(
            std::fs::read_to_string(&ui).expect("read the new file"),
            r#"{"b":2}"#,
            "the user's settings are still there"
        );
    }

    #[test]
    fn a_kill_during_the_reset_start_keeps_the_real_copy() {
        let dir = seeded("kill-reset", 3);
        let ui = dir.0.join(UI_FILE);
        std::fs::write(&ui, "real").expect("seed native-ui.json");

        let first = begin(&dir.0);

        let names = dir.names(UI_COPY_PREFIX);
        assert_eq!(names.len(), 1, "{names:?}");
        let copy = dir.0.join(&names[0]);
        assert_eq!(
            first,
            StartMode::AskAndResetUi {
                kept_ui: Some(copy.clone())
            }
        );
        assert_eq!(
            std::fs::read_to_string(&copy).expect("read the copy"),
            "real"
        );

        std::fs::write(&ui, "defaults").expect("the defaults the reset saved");
        assert_eq!(
            begin(&dir.0),
            StartMode::AskAndResetUi { kept_ui: None },
            "the second start at three sets nothing aside"
        );
        assert_eq!(
            std::fs::read_to_string(&ui).expect("read the file"),
            "defaults",
            "the defaults stay"
        );
        assert_eq!(dir.names(UI_COPY_PREFIX), names, "still the one copy");
    }

    #[test]
    fn a_failed_set_aside_leaves_the_reset_unmarked() {
        let dir = seeded("failed-set-aside", 3);
        let ui = dir.0.join(UI_FILE);
        std::fs::create_dir(&ui).expect("hold native-ui.json");

        let mode = begin(&dir.0);

        assert_eq!(mode, StartMode::AskAndResetUi { kept_ui: None });
        assert!(
            !read_guard(&dir.0).ui_set_aside,
            "a reset that did not happen is not marked done"
        );
        assert!(ui.is_dir(), "the file stays where it is");
    }

    #[test]
    fn many_failed_starts_stay_at_the_last_level() {
        let dir = seeded("many", 9);
        let ui = dir.0.join(UI_FILE);
        std::fs::write(&ui, r#"{"a":1}"#).expect("seed native-ui.json");
        assert_eq!(begin(&dir.0), StartMode::AskAndResetUi { kept_ui: None });
        assert_eq!(dir.names(UI_COPY_PREFIX), Vec::<String>::new());
        assert_eq!(
            std::fs::read_to_string(&ui).expect("read the file"),
            r#"{"a":1}"#,
            "no file is touched"
        );
    }

    #[test]
    fn a_malformed_guard_is_a_normal_start() {
        let dir = TestDir::new("malformed");
        dir.seed("not json");
        assert_eq!(begin(&dir.0), StartMode::Normal);
        assert_eq!(dir.guard()["failed_starts"], json!(0));
    }

    #[test]
    fn a_guard_from_v0_11_reads_with_no_reset_mark() {
        let dir = TestDir::new("v0-11");
        dir.seed(r#"{"failed_starts":3,"last_panic":"x"}"#);

        let guard = read_guard(&dir.0);
        assert_eq!(guard.failed_starts, 3);
        assert!(!guard.ui_set_aside, "an old file has set nothing aside");

        let ui = dir.0.join(UI_FILE);
        std::fs::write(&ui, "real").expect("seed native-ui.json");
        let mode = begin(&dir.0);

        let names = dir.names(UI_COPY_PREFIX);
        assert_eq!(names.len(), 1, "{names:?}");
        assert_eq!(
            mode,
            StartMode::AskAndResetUi {
                kept_ui: Some(dir.0.join(&names[0]))
            }
        );
        assert!(
            read_guard(&dir.0).ui_set_aside,
            "the first reset marks the file, so it happens once"
        );
    }

    #[test]
    fn a_panic_before_stable_counts_once() {
        let dir = TestDir::new("counts");
        assert_eq!(begin(&dir.0), StartMode::Normal);
        assert_eq!(record_panic(&dir.0, "first"), 1, "the panic counts");
        assert_eq!(
            record_panic(&dir.0, "second"),
            1,
            "the second panic in this process adds nothing"
        );
        assert_eq!(dir.guard()["failed_starts"], json!(1));
        assert_eq!(dir.guard()["last_panic"], json!("first"));
        assert_eq!(
            dir.guard()["run"],
            Value::Null,
            "the count asks next time, and a marker would count the run again"
        );
    }

    #[test]
    fn a_panic_after_stable_does_not_count() {
        let dir = TestDir::new("stable");
        assert_eq!(begin(&dir.0), StartMode::Normal);
        assert!(dir.0.join(GUARD_FILE).exists(), "the start wrote its guard");
        mark_stable(&dir.0);
        assert_eq!(
            record_panic(&dir.0, "late"),
            0,
            "a stable run counts nothing"
        );
        assert_eq!(
            dir.guard(),
            json!({
                "failed_starts": 0,
                "last_panic": null,
                "ui_set_aside": false,
                "run": own_mark("crashed"),
            })
        );
    }

    #[test]
    fn a_panic_keeps_the_reset_mark() {
        let dir = TestDir::new("keeps-mark");
        dir.seed(&json!({"failed_starts": 3, "last_panic": "x", "ui_set_aside": true}).to_string());

        assert_eq!(record_panic(&dir.0, "boom"), 4);

        let guard = read_guard(&dir.0);
        assert_eq!(guard.failed_starts, 4);
        assert!(guard.ui_set_aside, "the reset mark stays");
    }

    #[test]
    fn a_run_with_no_tabs_stops_counting_after_its_window() {
        stable_without_tabs();

        assert!(is_stable(), "a run with no tab list counts stable");
    }

    fn a_tab(id: &str) -> TabEntry {
        serde_json::from_value(json!({
            "id": id,
            "name": id,
            "content": { "kind": "dashboard" },
            "created_at": "2026-01-01T00:00:00Z",
        }))
        .expect("a tab fixture")
    }

    #[test]
    fn closed_tabs_filter_only_what_they_hold() {
        let mut closed = ClosedTabs::default();
        closed.note("a");
        closed.note("b");
        let mut tabs = vec![a_tab("a"), a_tab("c")];

        let taken = closed.filter_tabs(&mut tabs);

        assert_eq!(taken, ["a"]);
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].id, "c");
        assert!(closed.holds("a"), "a stays held while the daemon lists it");
        assert!(!closed.holds("b"), "b is forgotten, the daemon lost it");
    }

    #[test]
    fn tabs_arriving_late_count_again() {
        stable_without_tabs();
        assert!(is_stable());

        tabs_arrived();

        assert!(!is_stable(), "a late tab list re-arms the start window");
        stable_without_tabs();
        assert!(!is_stable(), "the no-tabs window is over once tabs came");
    }

    #[test]
    fn a_panic_after_the_start_asks_next_time() {
        assert_eq!(next_start_after(false, 5), NextStart::AsksAboutTabs);
        assert_eq!(next_start_after(false, 0), NextStart::AsksAboutTabs);
    }

    #[test]
    fn a_first_failed_start_asks_next_time() {
        assert_eq!(next_start_after(true, 1), NextStart::AsksAboutTabs);
    }

    #[test]
    fn a_second_failed_start_asks_next_time() {
        assert_eq!(next_start_after(true, 2), NextStart::AsksAboutTabs);
    }

    #[test]
    fn a_third_failed_start_asks_and_resets_the_ui_next_time() {
        assert_eq!(next_start_after(true, 3), NextStart::AsksAndResetsUi);
    }

    #[test]
    fn later_failed_starts_ask_next_time() {
        assert_eq!(next_start_after(true, 4), NextStart::AsksAboutTabs);
        assert_eq!(next_start_after(true, 9), NextStart::AsksAboutTabs);
    }

    #[test]
    fn an_uncounted_failed_start_still_asks_next_time() {
        assert_eq!(next_start_after(true, 0), NextStart::AsksAboutTabs);
    }

    #[test]
    fn a_failed_removal_forgets_the_count() {
        let dir = seeded("forget", 2);

        forget_guard(&dir.0, |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "locked",
            ))
        });

        assert_eq!(
            read_guard(&dir.0).failed_starts,
            0,
            "the count is forgotten"
        );
        assert_eq!(
            begin(&dir.0),
            StartMode::Normal,
            "the next start opens normally"
        );
    }

    #[test]
    fn mark_stable_with_no_file_is_fine() {
        let dir = TestDir::new("nofile");
        mark_stable(&dir.0);
        assert!(is_stable());
    }

    #[test]
    fn the_panic_box_is_claimed_once() {
        assert!(claim_panic_box(), "the first panic shows the box");
        assert!(!claim_panic_box(), "the second does not");
    }

    #[test]
    fn a_cloaked_run_waits_longer() {
        assert_eq!(stable_after(false), STABLE_AFTER);
        assert!(stable_after(true) > STABLE_AFTER);
    }

    #[test]
    fn panic_text_says_the_settings_reset_too() {
        assert_eq!(
            panic_text(
                "boom",
                NextStart::AsksAndResetsUi,
                Path::new("C:/logs/native.log")
            ),
            "Rustling Tulip crashed while starting.\n\nboom\n\nThe next start asks whether to \
             open your tabs, and your window and sidebar settings go back to their defaults, \
             with a copy of them kept. Your sessions keep running. The log is at \
             C:/logs/native.log."
        );
    }

    #[test]
    fn panic_text_during_start_says_the_next_start_asks() {
        assert_eq!(
            panic_text(
                "boom",
                NextStart::AsksAboutTabs,
                Path::new("C:/logs/native.log")
            ),
            "Rustling Tulip crashed while starting.\n\nboom\n\nStart Rustling Tulip again to \
             carry on. The next start asks whether to open your tabs. Your sessions keep \
             running. The log is at C:/logs/native.log."
        );
    }

    #[test]
    fn panic_text_later_says_start_again() {
        assert_eq!(
            panic_text(
                "boom",
                NextStart::Unaffected,
                Path::new("C:/logs/native.log")
            ),
            "Rustling Tulip crashed.\n\nboom\n\nYour sessions keep running. Start Rustling Tulip \
             again to carry on. The log is at C:/logs/native.log."
        );
    }

    #[test]
    fn panic_text_caps_a_long_message_on_a_char_boundary() {
        let text = panic_text(&"·".repeat(2000), NextStart::Unaffected, Path::new("log"));
        assert_eq!(text.chars().filter(|c| *c == '·').count(), 600);
    }

    #[test]
    fn keep_copy_keeps_the_three_newest() {
        let dir = TestDir::new("copies");
        for name in [
            "20260101T000000Z",
            "20260102T000000Z",
            "20260103T000000Z",
            "20260104T000000Z",
        ] {
            std::fs::write(dir.0.join(format!("{UI_COPY_PREFIX}{name}")), name)
                .expect("seed an old copy");
        }
        let source = dir.0.join("native-ui.json");
        std::fs::write(&source, "held").expect("write the source");

        let kept = keep_copy(&source, UI_COPY_PREFIX, at(0)).expect("keep a copy");

        assert_eq!(
            std::fs::read_to_string(&kept).expect("read the copy"),
            "held"
        );
        assert_eq!(
            dir.names(UI_COPY_PREFIX),
            [
                format!("{UI_COPY_PREFIX}20260103T000000Z"),
                format!("{UI_COPY_PREFIX}20260104T000000Z"),
                format!("{UI_COPY_PREFIX}20261108T120000Z"),
            ]
        );
        assert!(source.exists(), "a copy leaves its source in place");
    }

    #[test]
    fn a_taken_name_gets_the_next_number() {
        let dir = TestDir::new("taken");
        let first = keep_bytes(&dir.0, LAYOUT_COPY_PREFIX, b"a", at(0)).expect("keep");
        let second = keep_bytes(&dir.0, LAYOUT_COPY_PREFIX, b"b", at(0)).expect("keep");
        assert_ne!(first, second);
        assert!(second.to_string_lossy().ends_with("20261108T120000Z-2"));
    }

    #[test]
    fn the_prompt_says_only_what_happened() {
        let question = "Open your tabs as they were, or start without them? Your sessions keep \
                        running either way, and starting without them keeps a copy of the \
                        layout.";
        assert_eq!(StartPrompt::new(&StartMode::Ask).lines(), [question]);
        assert_eq!(
            StartPrompt::new(&StartMode::AskAndResetUi { kept_ui: None }).lines(),
            [question]
        );
        let full = StartPrompt::new(&StartMode::AskAndResetUi {
            kept_ui: Some(PathBuf::from("ui-copy")),
        });
        assert_eq!(
            full.lines(),
            [
                question,
                "Your window and sidebar settings went back to their defaults too. A copy is in \
                 ui-copy.",
            ]
        );
        assert_eq!(PROMPT_TITLE, "The last start crashed");
        assert_eq!(PROMPT_OPEN, "Open tabs");
        assert_eq!(PROMPT_WITHOUT, "Start without tabs");
    }

    #[test]
    fn the_toasts_name_the_copies() {
        assert_eq!(STARTED_WITHOUT_TITLE, "Started without your tabs");
        assert_eq!(
            started_without_detail(Path::new("layout-copy")),
            "A copy of the layout is in layout-copy."
        );
        assert_eq!(UI_RESET_TITLE, "Window and sidebar settings were reset");
        assert_eq!(
            ui_reset_detail(Path::new("ui-copy")),
            "The last starts crashed. A copy of your settings is in ui-copy."
        );
    }
}
