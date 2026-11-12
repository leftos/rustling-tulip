//! The gate's live state, read through leftos/gate's `gate-state` crate, and the hub's poll that publishes it.

#![cfg_attr(
    not(any(test, all(windows, target_pointer_width = "64"))),
    expect(
        dead_code,
        reason = "only 64-bit Windows reads live gate inputs, so elsewhere the reader and its conversion are reached only by tests"
    )
)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use tokio::sync::{Notify, broadcast, watch};
use tracing::warn;

use crate::server::StateEvent;

/// A count as the wire carries it, saturating at `u32::MAX`.
fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// An ISO 8601 moment as the gate writes it, or `None` when it does not parse.
fn iso(text: &str) -> Option<DateTime<Utc>> {
    text.parse::<DateTime<Utc>>().ok()
}

/// The key of a live gate's command in the history's summary: the journal's normalization of `command`.
fn command_key(command: &str) -> String {
    ::gate_state::normalize_command(command)
}

/// The byte spans of a command line's tokens, split on whitespace outside double quotes.
fn token_spans(line: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    let mut quoted = false;
    for (index, ch) in line.char_indices() {
        if ch == '"' {
            quoted = !quoted;
            start.get_or_insert(index);
        } else if ch.is_whitespace() && !quoted {
            if let Some(from) = start.take() {
                spans.push((from, index));
            }
        } else {
            start.get_or_insert(index);
        }
    }
    if let Some(from) = start {
        spans.push((from, line.len()));
    }
    spans
}

/// Whether a token names the gate program: its quotes and any folder removed, its file name is `gate` or
/// `gate.exe`, in any case.
fn is_gate_program(token: &str) -> bool {
    let token = token.trim_matches('"');
    let name = token.rsplit(['\\', '/']).next().unwrap_or(token);
    name.eq_ignore_ascii_case("gate") || name.eq_ignore_ascii_case("gate.exe")
}

/// The command a gate run line wraps: the line after its first standalone `--` when the line is a gate run and
/// something follows that `--`, the line itself otherwise.
fn inner_command(line: &str) -> &str {
    let spans = token_spans(line);
    let first = spans.first().map(|&(from, to)| &line[from..to]);
    let second = spans.get(1).map(|&(from, to)| &line[from..to]);
    if !first.is_some_and(is_gate_program) || second != Some("run") {
        return line;
    }
    let Some(&(_, end)) = spans.iter().find(|&&(from, to)| &line[from..to] == "--") else {
        return line;
    };
    match line[end..].trim_start() {
        "" => line,
        inner => inner,
    }
}

/// The repo a live gate's folder belongs to, as the history names it.
fn repo_of(repos: &RepoFolders, cwd: Option<&str>) -> Option<String> {
    cwd.and_then(|cwd| repos.repo_of(cwd))
}

/// A slot's claim or stale claim as the wire carries it: each field from its own camel case key, each `None` when
/// missing or of another type, and every field `None` for a value that is not an object. The command's key and the
/// folder's repo come from the command and the folder.
fn claim_of(value: &Value, repos: &RepoFolders) -> protocol::GateClaim {
    let Value::Object(fields) = value else {
        return protocol::GateClaim::default();
    };
    let cwd = fields.get("cwd").and_then(Value::as_str).map(str::to_owned);
    let command = fields
        .get("command")
        .and_then(Value::as_str)
        .map(str::to_owned);
    protocol::GateClaim {
        pid: fields
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok()),
        started: fields.get("started").and_then(Value::as_str).and_then(iso),
        command_key: command.as_deref().map(command_key),
        repo: repo_of(repos, cwd.as_deref()),
        cwd,
        command,
        log: fields.get("log").and_then(Value::as_str).map(str::to_owned),
        timeout_seconds: fields.get("timeoutSeconds").and_then(Value::as_f64),
    }
}

/// What the durations store expected of a queued gate, from its ticket's `expected` object.
fn expected_of(fields: &Map<String, Value>) -> protocol::GateExpected {
    protocol::GateExpected {
        p50: fields.get("p50").and_then(Value::as_f64),
        p90: fields.get("p90").and_then(Value::as_f64),
        n: fields
            .get("n")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        key: fields.get("key").and_then(Value::as_str).map(str::to_owned),
    }
}

/// One slot's row.
fn slot_of(row: &::gate_state::SlotRow, repos: &RepoFolders) -> protocol::GateSlot {
    protocol::GateSlot {
        index: row.index,
        name: row.name.clone(),
        state: match row.state {
            ::gate_state::SlotState::Free => protocol::GateSlotState::Free,
            ::gate_state::SlotState::Busy => protocol::GateSlotState::Busy,
            ::gate_state::SlotState::Held => protocol::GateSlotState::Held,
        },
        claim: row.claim.as_ref().map(|claim| claim_of(claim, repos)),
        stale: row.stale.as_ref().map(|claim| claim_of(claim, repos)),
        processes: count(row.processes),
        last_line: row.last_line.clone(),
        critical: row.critical,
        extra: row.extra,
    }
}

/// A gate waiting for a slot.
fn waiting_of(gate: &::gate_state::Waiting, repos: &RepoFolders) -> protocol::GateWaiting {
    let command_line = inner_command(&gate.command_line).to_owned();
    let command_key = Some(command_key(&command_line));
    protocol::GateWaiting {
        pid: gate.pid,
        since: gate.since.as_deref().and_then(iso),
        cwd: gate.cwd.clone(),
        command_line,
        command_key,
        repo: repo_of(repos, gate.cwd.as_deref()),
    }
}

/// One local pool of slots.
fn pool_of(pool: &::gate_state::PoolState, repos: &RepoFolders) -> protocol::GatePool {
    protocol::GatePool {
        kind: match pool.kind {
            ::gate_state::SlotKind::Heavy => protocol::GateSlotKind::Heavy,
            ::gate_state::SlotKind::Light => protocol::GateSlotKind::Light,
        },
        count: pool.count,
        default: pool.default_count,
        set: pool.set.map(::gate_state::SlotCount::get),
        problem: pool.problem.clone(),
        slots: pool.slots.iter().map(|row| slot_of(row, repos)).collect(),
        waiting: pool
            .waiting
            .iter()
            .map(|gate| waiting_of(gate, repos))
            .collect(),
    }
}

/// The remote pool.
fn remote_of(pool: &::gate_state::RemotePool, repos: &RepoFolders) -> protocol::GateRemotePool {
    protocol::GateRemotePool {
        count: pool.count,
        problem: pool.problem.clone(),
        slots: pool.slots.iter().map(|row| slot_of(row, repos)).collect(),
    }
}

/// One builder's own remote pool.
fn builder_of(pool: &::gate_state::BuilderPool, repos: &RepoFolders) -> protocol::GateBuilderPool {
    protocol::GateBuilderPool {
        id: pool.id.clone(),
        name: pool.name.clone(),
        host: pool.host.clone(),
        os: pool.os.clone(),
        count: pool.count,
        problem: pool.problem.clone(),
        routed: pool.routed,
        slots: pool.slots.iter().map(|row| slot_of(row, repos)).collect(),
    }
}

/// A live gate a parent gate's slot covers.
fn covered_of(run: &::gate_state::CoveredRun) -> protocol::GateCoveredRun {
    protocol::GateCoveredRun {
        pid: run.pid,
        start: iso(&run.start),
        parent_pid: run.parent_pid,
        command: run.command.clone(),
        cwd: run.cwd.clone(),
        since: iso(&run.since),
    }
}

/// One queued gate's ticket.
fn ticket_of(ticket: &::gate_state::QueueTicket, repos: &RepoFolders) -> protocol::GateQueueTicket {
    let command_line = ticket
        .command_line
        .as_deref()
        .map(|line| inner_command(line).to_owned());
    let command_key = command_line.as_deref().map(command_key);
    protocol::GateQueueTicket {
        kind: ticket.kind.clone(),
        holds: ticket.holds,
        remote: ticket.remote,
        pool: ticket.pool.clone(),
        pinned: ticket.pinned,
        pid: ticket.pid,
        arrived: iso(&ticket.arrived),
        age_seconds: ticket.age_seconds,
        cwd: ticket.cwd.clone(),
        command_line,
        reason: ticket.reason.clone(),
        expected: ticket.expected.as_ref().map(expected_of),
        command_key,
        repo: repo_of(repos, ticket.cwd.as_deref()),
    }
}

/// Converts the gate's dashboard state to the wire state the client reads, naming each live gate's repo from `repos`.
pub(crate) fn to_wire(
    state: &::gate_state::DashboardState,
    repos: &RepoFolders,
) -> protocol::GateState {
    protocol::GateState {
        now: iso(&state.now),
        machine: state.machine.clone(),
        processors: count(state.processors),
        critical: Some(protocol::GateCriticalState {
            holder: Some(slot_of(&state.critical.holder, repos)),
            waiting: state
                .critical
                .waiting
                .iter()
                .map(|gate| waiting_of(gate, repos))
                .collect(),
        }),
        queue: state
            .queue
            .iter()
            .map(|ticket| ticket_of(ticket, repos))
            .collect(),
        pools: state
            .pools
            .iter()
            .map(|pool| pool_of(pool, repos))
            .collect(),
        remote: state.remote.as_ref().map(|pool| remote_of(pool, repos)),
        builders: state
            .builders
            .iter()
            .map(|pool| builder_of(pool, repos))
            .collect(),
        covered: state.covered.iter().map(covered_of).collect(),
    }
}

/// Reads the gate's state from the given inputs and probes, or `None` when the gate's state folder is not there.
///
/// The signal is the `gate` folder under the per-user local application data folder, which the gate's installer and
/// every gate run create before anything is written; a failed check also reads as no state, and is logged.
pub(crate) fn read_once(
    inputs: &::gate_state::StateInputs,
    processes: &dyn ::proc_table::ProcessTable,
    mutexes: &dyn ::gate_state::MutexProbe,
    cwds: &dyn ::gate_state::CwdReader,
    repos: &RepoFolders,
) -> Option<protocol::GateState> {
    let dir = inputs.local_app_data.join("gate");
    match dir.try_exists() {
        Ok(true) => {}
        Ok(false) => return None,
        Err(error) => {
            warn!(%error, path = %dir.display(), "gate state: the gate's state folder could not be checked");
            return None;
        }
    }
    Some(to_wire(
        &::gate_state::dashboard_state(inputs, processes, mutexes, cwds),
        repos,
    ))
}

/// The process environment, as [`gate_installed`] asks it.
fn process_env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// The user install's gate program, `%LOCALAPPDATA%\gate\bin\gate.exe`, or `None` when the variable is unset or empty.
fn local_gate(env: &impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let root = env("LOCALAPPDATA").filter(|root| !root.is_empty())?;
    Some(Path::new(&root).join("gate").join("bin").join("gate.exe"))
}

/// The admin or builder install's gate program, `<Program Files root>\gate\bin\gate.exe`, where the root is
/// `GATE_LAUNCHER_PROGRAM_FILES` when it is set and not empty, else `%ProgramFiles%`, or `None` when neither is set.
fn program_files_gate(env: &impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let root = env("GATE_LAUNCHER_PROGRAM_FILES")
        .filter(|root| !root.is_empty())
        .or_else(|| env("ProgramFiles").filter(|root| !root.is_empty()))?;
    Some(Path::new(&root).join("gate").join("bin").join("gate.exe"))
}

/// Whether `path` holds a program file: a `NotFound` is no program and logs nothing, and any other failure is logged
/// and reads as no program.
fn gate_program_present(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(metadata) => metadata.is_file(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            warn!(%error, path = %path.display(), "gate state: a gate program could not be checked");
            false
        }
    }
}

/// Whether this host is set up to run gates: a `gate.exe` at the two install paths `tools/gate.ps1` uses, the user
/// install under `%LOCALAPPDATA%\gate\bin` and the admin or builder install under `<Program Files root>\gate\bin`,
/// whose root `GATE_LAUNCHER_PROGRAM_FILES` overrides when it is set and not empty. `GATE_BOX_EXE`, a routed builder
/// job's own gate, is deliberately not consulted: it names the gate of that run, not the one this host installed. PATH
/// is not searched, and a leftover `gate.exe.*.old` does not count.
///
/// `env` and `present` are the daemon's process environment and program check; tests pass fakes.
fn gate_installed(env: impl Fn(&str) -> Option<OsString>, present: impl Fn(&Path) -> bool) -> bool {
    program_files_gate(&env).is_some_and(|exe| present(&exe))
        || local_gate(&env).is_some_and(|exe| present(&exe))
}

/// Reads this host's live gate state, naming each live gate's repo from `repos`, or `None` when the gate is not
/// installed here, the gate's inputs cannot be read, or the gate's state folder is not there.
#[cfg(all(windows, target_pointer_width = "64"))]
pub(crate) fn read_live(repos: &RepoFolders) -> Option<protocol::GateState> {
    if !gate_installed(process_env, gate_program_present) {
        return None;
    }
    let inputs = match ::gate_state::live_inputs() {
        Ok(inputs) => inputs,
        Err(error) => {
            warn!(%error, "gate state: the gate's inputs could not be read");
            return None;
        }
    };
    read_once(
        &inputs,
        &::proc_table::Win32ProcessTable,
        &::gate_state::Win32MutexProbe,
        &::gate_state::Win32CwdReader,
        repos,
    )
}

/// Whether two states are the same for the send-on-change comparison, ignoring `now` and the ages computed from it.
pub(crate) fn same_state(a: &protocol::GateState, b: &protocol::GateState) -> bool {
    without_ages(a) == without_ages(b)
}

/// The state with the fields computed from `now` cleared, since the client computes time held from `started` and
/// `arrived`.
fn without_ages(state: &protocol::GateState) -> protocol::GateState {
    let mut state = state.clone();
    state.now = None;
    for ticket in &mut state.queue {
        ticket.age_seconds = 0.0;
    }
    state
}

/// The wait between two reads while a client is connected.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How the poll reads the gate's state, given the registered repos' folders: [`read_live`] in the daemon, a scripted
/// closure in tests. It blocks, so the poll calls it on a blocking thread.
pub(crate) type GateReader = Arc<dyn Fn(&RepoFolders) -> Option<protocol::GateState> + Send + Sync>;

/// The reader the daemon's poll runs: [`read_live`] on 64-bit Windows, the one platform with live gate inputs, and
/// `None` elsewhere, where the hub holds no gate state and starts no poll.
#[cfg(all(windows, target_pointer_width = "64"))]
pub(crate) const LIVE_READER: Option<fn(&RepoFolders) -> Option<protocol::GateState>> =
    Some(read_live);
/// The reader the daemon's poll runs: none off 64-bit Windows, where the hub holds no gate state and starts no poll.
#[cfg(not(all(windows, target_pointer_width = "64")))]
pub(crate) const LIVE_READER: Option<fn(&RepoFolders) -> Option<protocol::GateState>> = None;

/// Where the gate's counts file is, looked up at each slot-count write: the live inputs' path in the daemon, a temp
/// file in tests; `None` when it cannot be found. It may block, so a write calls it on a blocking thread.
pub(crate) type CountsPath = Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>;

/// This host's gate counts file, read from the gate's live inputs, or `None` when they cannot be read.
#[cfg(all(windows, target_pointer_width = "64"))]
pub(crate) fn live_counts_path() -> Option<PathBuf> {
    match ::gate_state::live_inputs() {
        Ok(inputs) => Some(::gate_state::slot_count_path(&inputs)),
        Err(error) => {
            warn!(%error, "gate state: the gate's inputs could not be read for its counts file");
            None
        }
    }
}

/// This host's gate counts file: none off 64-bit Windows, where the hub holds no gate state.
#[cfg(not(all(windows, target_pointer_width = "64")))]
pub(crate) fn live_counts_path() -> Option<PathBuf> {
    None
}

/// A run's state as the wire carries it.
fn run_state_of(state: ::gate_state::RunState) -> protocol::GateRunState {
    match state {
        ::gate_state::RunState::Ok => protocol::GateRunState::Ok,
        ::gate_state::RunState::Failed => protocol::GateRunState::Failed,
        ::gate_state::RunState::Abandoned => protocol::GateRunState::Abandoned,
        ::gate_state::RunState::Running => protocol::GateRunState::Running,
        ::gate_state::RunState::Unrecorded => protocol::GateRunState::Unrecorded,
        ::gate_state::RunState::Parent => protocol::GateRunState::Parent,
        ::gate_state::RunState::Covered => protocol::GateRunState::Covered,
    }
}

/// One run of the gate's journal as the wire carries it, without its machine snapshot and slot names.
fn history_run_of(run: &::gate_state::HistoryRun) -> protocol::GateHistoryRun {
    protocol::GateHistoryRun {
        command: run.command.clone(),
        cwd: run.cwd.clone(),
        repo: run.repo.clone(),
        kind: run.kind.clone(),
        slot: run.slot,
        remote: run.remote,
        host: run.host.clone(),
        fallback: run.fallback.clone(),
        reason: run.reason.clone(),
        pid: run.pid,
        process_start: iso(&run.process_start),
        parent_pid: run.parent_pid,
        state: run_state_of(run.state),
        running: run.running,
        queued: run.queued.as_deref().and_then(iso),
        started: run.started.as_deref().and_then(iso),
        waited_seconds: run.waited_seconds,
        held_seconds: run.held_seconds,
        status: run.status,
        verdict: run.verdict.clone(),
        wall_seconds: run.wall_seconds,
        adjusted_seconds: run.adjusted_seconds,
        abandoned: run.abandoned.clone(),
        remote_take: run
            .remote_take
            .as_ref()
            .map(|take| protocol::GateRemoteTake {
                started: iso(&take.started),
                waited_seconds: take.waited_seconds,
                fell_back_at: iso(&take.fell_back_at),
            }),
        cold: run.cold,
        critical_pid: run.critical_pid,
    }
}

/// One summary row as the wire carries it.
fn summary_of(row: &::gate_state::CommandSummary) -> protocol::GateCommandSummary {
    protocol::GateCommandSummary {
        command: row.command.clone(),
        repo: row.repo.clone(),
        remote: row.remote,
        count: count(row.count),
        p50_seconds: row.p50_seconds,
        p90_seconds: row.p90_seconds,
        max_seconds: row.max_seconds,
    }
}

/// The gate's run history as a `GateHistory` reply carries it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct HistoryLists {
    pub(crate) runs: Vec<protocol::GateHistoryRun>,
    pub(crate) open: Vec<protocol::GateHistoryRun>,
    pub(crate) summary: Vec<protocol::GateCommandSummary>,
}

/// Converts the gate's journal history to the lists the wire carries.
pub(crate) fn history_to_wire(history: &::gate_state::History) -> HistoryLists {
    HistoryLists {
        runs: history.runs.iter().map(history_run_of).collect(),
        open: history.open.iter().map(history_run_of).collect(),
        summary: history.summary.iter().map(summary_of).collect(),
    }
}

/// The registered repos' folders, snapshotted for a history read, which names the repo of each run's folder.
#[derive(Debug, Clone, Default)]
pub(crate) struct RepoFolders {
    /// Each repo's folder as `paths::normalize_path_key` gives it, with the repo's name.
    folders: Vec<(String, String)>,
}

impl RepoFolders {
    /// The folders of the given `(folder, name)` pairs.
    pub(crate) fn new<F: AsRef<str>, N: AsRef<str>>(
        repos: impl IntoIterator<Item = (F, N)>,
    ) -> Self {
        Self {
            folders: repos
                .into_iter()
                .map(|(folder, name)| {
                    (
                        crate::paths::normalize_path_key(folder.as_ref()),
                        name.as_ref().to_owned(),
                    )
                })
                .filter(|(folder, _)| !folder.is_empty())
                .collect(),
        }
    }

    /// The folders of the repos `state`'s registry holds now.
    pub(crate) fn registered(state: &crate::state::AppState) -> Self {
        state.with_persisted(|persisted| {
            Self::new(
                persisted
                    .repos
                    .iter()
                    .map(|repo| (repo.path.as_str(), repo.name.as_str())),
            )
        })
    }

    /// The repo `folder` belongs to: the registered repo whose folder is the longest one holding it; else, for a
    /// folder under `<repo>.wt\<name>`, `<repo>`; else none.
    pub(crate) fn repo_of(&self, folder: &str) -> Option<String> {
        let key = crate::paths::normalize_path_key(folder);
        self.folders
            .iter()
            .filter(|(repo, _)| {
                key.strip_prefix(repo.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '\\']))
            })
            .max_by_key(|(repo, _)| repo.len())
            .map(|(_, name)| name.clone())
            .or_else(|| worktree_repo(folder))
    }
}

/// The `<repo>` of a folder at or under `<repo>.wt\<name>` (`.wt` in any case), the last such component winning.
fn worktree_repo(folder: &str) -> Option<String> {
    let parts: Vec<&str> = folder
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .collect();
    parts.windows(2).rev().find_map(|pair| {
        let [name, _] = pair else {
            return None;
        };
        let cut = name.len().checked_sub(3)?;
        let repo = name.get(..cut)?;
        let suffix = name.get(cut..)?;
        (!repo.is_empty() && suffix.eq_ignore_ascii_case(".wt")).then(|| repo.to_owned())
    })
}

/// How a `GateHistory` request reads the gate's run history, given the registered repos' folders: the journal through
/// one [`::gate_state::JournalCache`] in the daemon, a scripted closure in tests; `None` when the journal cannot be read.
/// It blocks, so the request calls it on a blocking thread.
pub(crate) type HistorySource = Arc<dyn Fn(RepoFolders) -> Option<HistoryLists> + Send + Sync>;

/// The daemon's history source: a closure owning the one journal cache, which it locks only while it reads, after a
/// process snapshot for the journal's liveness check. A failed read is logged and shows no history.
#[cfg(all(windows, target_pointer_width = "64"))]
pub(crate) fn live_history_source() -> HistorySource {
    use ::proc_table::ProcessTable as _;
    let cache = Arc::new(std::sync::Mutex::new(::gate_state::JournalCache::default()));
    Arc::new(move |repos: RepoFolders| {
        let processes = ::proc_table::Win32ProcessTable.snapshot();
        let mut cache = crate::sync::lock(&cache);
        match ::gate_state::live_history(&mut cache, &processes, |folder| repos.repo_of(folder)) {
            Ok(history) => Some(history_to_wire(&history)),
            Err(error) => {
                warn!(%error, "gate state: the gate's journal could not be read; no history shown");
                None
            }
        }
    })
}

/// The daemon's history source: none off 64-bit Windows, where the hub holds no gate state.
#[cfg(not(all(windows, target_pointer_width = "64")))]
pub(crate) fn live_history_source() -> HistorySource {
    Arc::new(|_: RepoFolders| None)
}

/// Whether this host is set up to run gates, asked before a slot-count write or a history read.
pub(crate) type InstalledCheck = Arc<dyn Fn() -> bool + Send + Sync>;

/// The hub's current gate state, `None` until a read finds a gate. The poll is its only writer; a connection reads it
/// at its initial state and when its state stream lags, and otherwise takes the poll's broadcasts. A slot-count write
/// never publishes: it asks the poll to read again ([`GateHub::repoll`]), so a write and a tick never publish out of
/// order.
#[derive(Clone)]
pub(crate) struct GateHub {
    current: Arc<watch::Sender<Option<protocol::GateState>>>,
    counts_path: CountsPath,
    history: HistorySource,
    installed: InstalledCheck,
    repoll: Arc<Notify>,
}

impl GateHub {
    /// A hub gate state that holds no state yet, writing slot counts to the file `counts_path` names and reading the
    /// run history from `history`.
    pub(crate) fn new(counts_path: CountsPath, history: HistorySource) -> Self {
        Self {
            current: Arc::new(watch::Sender::new(None)),
            counts_path,
            history,
            installed: Arc::new(|| gate_installed(process_env, gate_program_present)),
            repoll: Arc::new(Notify::new()),
        }
    }

    /// Whether this host is set up to run gates, asked before a slot-count write or a history read.
    pub(crate) fn installed(&self) -> bool {
        (self.installed)()
    }

    /// Answer [`GateHub::installed`] with `check` from now on, so a test can stand on a host without the gate.
    #[cfg(test)]
    pub(crate) fn set_installed_check(&mut self, check: InstalledCheck) {
        self.installed = check;
    }

    /// The lookup of the gate's counts file.
    pub(crate) fn counts_path(&self) -> CountsPath {
        Arc::clone(&self.counts_path)
    }

    /// The read of the gate's run history.
    pub(crate) fn history(&self) -> HistorySource {
        Arc::clone(&self.history)
    }

    /// Ask the poll to read at once rather than at its next tick; a request made while it is parked is kept for the
    /// next connect, whose read it joins.
    pub(crate) fn repoll(&self) {
        self.repoll.notify_one();
    }

    /// The current state. Every change is broadcast under the watch's write lock, so a connection that holds this
    /// borrow across its send cannot see a later broadcast before it.
    pub(crate) fn current(&self) -> watch::Ref<'_, Option<protocol::GateState>> {
        self.current.borrow()
    }

    /// Store `state` and, when it differs from the current one by more than its moment and ages, broadcast it on
    /// `events` as a [`StateEvent::GateState`]; returns whether it was broadcast. An equal state still replaces the
    /// stored one silently, so a connection's initial state carries the latest moment.
    pub(crate) fn publish(
        &self,
        state: Option<protocol::GateState>,
        events: &broadcast::Sender<StateEvent>,
    ) -> bool {
        self.current.send_if_modified(|current| {
            let same = match (&*current, &state) {
                (None, None) => true,
                (Some(old), Some(new)) => same_state(old, new),
                _ => false,
            };
            *current = state;
            if !same {
                let _ = events.send(StateEvent::GateState(current.clone().map(Box::new)));
            }
            !same
        })
    }
}

/// Start the gate poll for the daemon's life. While no client is connected it parks and reads nothing; when the count
/// rises from zero it reads at once, then every [`POLL_INTERVAL`] and at once on each [`GateHub::repoll`], each read on
/// a blocking thread with the repos `state` registers at that moment, publishing each result to `gate` (broadcast on
/// `events` when it changed).
pub(crate) fn spawn_poll(
    reader: GateReader,
    gate: GateHub,
    state: Arc<crate::state::AppState>,
    events: broadcast::Sender<StateEvent>,
    mut clients: watch::Receiver<usize>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if clients.wait_for(|count| *count > 0).await.is_err() {
                return;
            }
            let mut interval = tokio::time::interval(POLL_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick completes at once: the connect's read.
            loop {
                tokio::select! {
                    biased;
                    changed = clients.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        if *clients.borrow_and_update() == 0 {
                            break;
                        }
                    }
                    () = gate.repoll.notified() => read_and_publish(&reader, &gate, &state, &events).await,
                    _ = interval.tick() => read_and_publish(&reader, &gate, &state, &events).await,
                }
            }
        }
    })
}

/// Snapshot the registered repos, read once on a blocking thread and publish the result; a read that stopped is
/// logged and skips the tick.
async fn read_and_publish(
    reader: &GateReader,
    gate: &GateHub,
    state: &crate::state::AppState,
    events: &broadcast::Sender<StateEvent>,
) {
    let reader = Arc::clone(reader);
    let repos = RepoFolders::registered(state);
    match tokio::task::spawn_blocking(move || reader(&repos)).await {
        Ok(state) => {
            gate.publish(state, events);
        }
        Err(error) => warn!(%error, "gate state: the read stopped; skipping this tick"),
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert setup and fixtures with expect"
)]
mod tests {
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    use ::gate_state::{
        BuilderPool, CommandSummary, CoveredRun, CriticalState, CwdReader, DashboardState,
        Generation, History, HistoryRun, MutexProbe, PoolState, QueueTicket, RemotePool,
        RemoteTake, RunState, SlotCount, SlotKind, SlotRow, SlotState, StateInputs, Waiting,
    };
    use ::proc_table::EmptyTable;
    use chrono::{DateTime, Utc};
    use serde_json::{Value, json};

    use protocol::{
        GateClaim, GateCommandSummary, GateExpected, GateHistoryRun, GateRemoteTake, GateRunState,
        GateSlotKind, GateSlotState, GateWaiting,
    };

    use tokio::sync::{broadcast, mpsc, watch};

    use protocol::{AppearanceOverrides, RepoEntry};

    use crate::paths::Dirs;
    use crate::server::StateEvent;
    use crate::state::AppState;

    use super::{
        GateHub, GateReader, POLL_INTERVAL, RepoFolders, command_key, gate_installed,
        history_to_wire, inner_command, read_once, same_state, spawn_poll, ticket_of, to_wire,
        waiting_of,
    };

    /// The moment a test describes.
    const NOW: &str = "2026-10-01T12:00:00.1234567Z";
    /// When a test's gate started, as the gate writes it.
    const START: &str = "2026-10-01T11:00:00.5000000Z";
    /// A gate run line as a waiting gate's or a ticket's `command_line` carries it.
    const GATE_RUN: &str = "gate.exe run --cd X:/dev/repo --log .tmp/a.log --slot heavy -- cargo nextest run -p daemon";
    /// The command `GATE_RUN` wraps.
    const INNER: &str = "cargo nextest run -p daemon";

    /// A machine with no gate slot's mutex open and no process whose working directory can be read.
    struct QuietProbe;

    impl MutexProbe for QuietProbe {
        fn is_open(&self, _name: &str) -> bool {
            false
        }
    }

    impl CwdReader for QuietProbe {
        fn current_dir(&self, _pid: u32) -> Option<String> {
            None
        }
    }

    /// The moment `NOW` names.
    fn now() -> DateTime<Utc> {
        NOW.parse().expect("NOW is a parseable moment")
    }

    /// The moment `START` names.
    fn start() -> DateTime<Utc> {
        START.parse().expect("START is a parseable moment")
    }

    /// A fresh folder for a test, emptied first and left with no `gate` folder in it.
    fn fresh_dir(test: &str) -> PathBuf {
        let root =
            option_env!("CARGO_TARGET_TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
        let dir = root.join("rt-gate-state").join(test);
        if dir.exists() {
            fs::remove_dir_all(&dir).expect("the test's folder clears");
        }
        fs::create_dir_all(&dir).expect("the test's folder is created");
        dir
    }

    /// The state inputs of a test folder, read as a machine with no cutover flag.
    fn inputs(dir: &Path) -> StateInputs {
        StateInputs {
            now: NOW.parse().expect("NOW is a parseable moment"),
            machine: "TESTBOX".to_owned(),
            processors: 16,
            local_app_data: dir.to_path_buf(),
            slot_counts_override: None,
            slot_prefix: String::new(),
            generation: Generation::detect(dir).expect("the cutover flag is looked for"),
        }
    }

    /// A slot row with nothing on it.
    fn empty_slot(index: u32, name: &str, state: SlotState) -> SlotRow {
        SlotRow {
            index,
            name: name.to_owned(),
            state,
            claim: None,
            stale: None,
            processes: 0,
            last_line: None,
            critical: false,
            extra: None,
        }
    }

    /// A heavy pool of three slots around the given rows.
    fn heavy_pool(slots: Vec<SlotRow>) -> PoolState {
        PoolState {
            kind: SlotKind::Heavy,
            count: 3,
            default_count: 2,
            set: SlotCount::new(2),
            problem: None,
            slots,
            waiting: Vec::new(),
        }
    }

    /// A dashboard state of the given pools, and otherwise nothing.
    fn dashboard(pools: Vec<PoolState>) -> DashboardState {
        DashboardState {
            now: NOW.to_owned(),
            machine: "TESTBOX".to_owned(),
            processors: 16,
            critical: CriticalState {
                holder: empty_slot(0, "gate-critical", SlotState::Free),
                waiting: Vec::new(),
            },
            queue: Vec::new(),
            pools,
            remote: None,
            builders: Vec::new(),
            covered: Vec::new(),
        }
    }

    /// A claim file's JSON as the gate writes it, every key set.
    fn full_claim() -> Value {
        json!({
            "v": 1,
            "kind": "heavy",
            "index": 0,
            "pid": 4321,
            "processStart": START,
            "started": START,
            "cwd": "X:\\dev\\repo",
            "command": "cargo test",
            "log": "X:\\dev\\repo\\.tmp\\gate.log",
            "timeoutSeconds": 600.0
        })
    }

    /// The wire claim `full_claim` converts to.
    fn full_wire_claim() -> GateClaim {
        GateClaim {
            pid: Some(4321),
            started: Some(start()),
            cwd: Some("X:\\dev\\repo".to_owned()),
            command: Some("cargo test".to_owned()),
            log: Some("X:\\dev\\repo\\.tmp\\gate.log".to_owned()),
            timeout_seconds: Some(600.0),
            command_key: Some("cargo test".to_owned()),
            repo: None,
        }
    }

    /// No registered repo, and so no repo for any folder outside a `<repo>.wt` folder.
    fn no_repos() -> RepoFolders {
        RepoFolders::default()
    }

    /// A queued heavy gate's ticket, arrived at `START`, `age_seconds` old.
    fn queued(age_seconds: f64) -> QueueTicket {
        QueueTicket {
            kind: "heavy".to_owned(),
            holds: false,
            remote: false,
            pool: None,
            pinned: false,
            pid: 4242,
            arrived: START.to_owned(),
            age_seconds,
            cwd: Some("X:\\dev\\repo".to_owned()),
            command_line: Some("cargo build".to_owned()),
            reason: None,
            expected: None,
        }
    }

    /// A gate run line shows the command it wraps: a quoted program path, and a second `--` inside the inner
    /// command, both leave the rest of the line as written.
    #[test]
    fn inner_command_is_what_follows_the_first_double_dash() {
        assert_eq!(inner_command(GATE_RUN), INNER);
        assert_eq!(
            inner_command(r#""C:\Users\me\bin\gate.exe" run --log X:\dev\x.log -- dotnet build"#),
            "dotnet build"
        );
        assert_eq!(
            inner_command("gate run --slot light -- prek run --files a.rs -- b"),
            "prek run --files a.rs -- b"
        );
    }

    /// A line that is not a gate run, a gate run with no `--`, and a gate run whose quoted reason holds a `--`
    /// show their own command.
    #[test]
    fn inner_command_leaves_other_lines_alone() {
        for line in [
            "cargo build --release",
            "gate.exe status",
            "gate.exe run --slot heavy",
        ] {
            assert_eq!(inner_command(line), line);
        }
        assert_eq!(
            inner_command(r#"gate.exe run --reason "a -- b" --slot heavy -- cargo test"#),
            "cargo test"
        );
    }

    /// A waiting gate's row shows the command its own gate run line wraps, keyed by that command.
    #[test]
    fn a_waiting_gate_shows_its_inner_command_and_key() {
        let gate = Waiting {
            pid: 700,
            since: Some(START.to_owned()),
            cwd: Some("X:\\dev\\repo".to_owned()),
            command_line: GATE_RUN.to_owned(),
        };

        assert_eq!(
            waiting_of(&gate, &no_repos()),
            GateWaiting {
                pid: 700,
                since: Some(start()),
                cwd: Some("X:\\dev\\repo".to_owned()),
                command_line: INNER.to_owned(),
                command_key: Some(command_key(INNER)),
                repo: None,
            }
        );
    }

    /// A queue ticket's row shows the command its own gate run line wraps, keyed by that command.
    #[test]
    fn a_queue_ticket_shows_its_inner_command_and_key() {
        let mut ticket = queued(5.0);
        ticket.command_line = Some(GATE_RUN.to_owned());

        let wire = ticket_of(&ticket, &no_repos());

        assert_eq!(wire.command_line.as_deref(), Some(INNER));
        assert_eq!(wire.command_key, Some(command_key(INNER)));
    }

    /// The state assembly runs from this crate: two empty pools, heavy then light.
    #[test]
    fn dashboard_state_reads_two_empty_pools() -> anyhow::Result<()> {
        let root =
            option_env!("CARGO_TARGET_TMPDIR").map_or_else(std::env::temp_dir, PathBuf::from);
        let dir = root
            .join("rt-gate-state")
            .join("dashboard_state_reads_two_empty_pools");
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(dir.join("gate").join("slots"))?;

        let inputs = ::gate_state::StateInputs {
            now: "2026-10-01T12:00:00Z".parse()?,
            machine: "TESTBOX".to_owned(),
            processors: 16,
            local_app_data: dir.clone(),
            slot_counts_override: None,
            slot_prefix: String::new(),
            generation: ::gate_state::Generation::detect(&dir)?,
        };

        let state = ::gate_state::dashboard_state(&inputs, &EmptyTable, &QuietProbe, &QuietProbe);

        anyhow::ensure!(
            state.pools.len() == 2,
            "two pools, got {}",
            state.pools.len()
        );
        anyhow::ensure!(state.pools[0].kind == SlotKind::Heavy, "heavy first");
        anyhow::ensure!(state.pools[1].kind == SlotKind::Light, "light second");
        anyhow::ensure!(
            state.machine == "TESTBOX",
            "the machine is TESTBOX, got {}",
            state.machine
        );
        Ok(())
    }

    /// Each slot's row reaches the wire with its index, state, name, processes, last line and claim.
    #[test]
    fn gate_state_converts_a_busy_held_free_and_stale_slot() {
        let mut busy = empty_slot(0, "gate-slot-0", SlotState::Busy);
        busy.claim = Some(full_claim());
        busy.processes = 3;
        busy.last_line = Some("Compiling libc v0.2.170".to_owned());
        let held = empty_slot(1, "gate-slot-1", SlotState::Held);
        let mut free = empty_slot(2, "gate-slot-2", SlotState::Free);
        free.stale = Some(json!({"pid": 99, "started": START, "cwd": "X:\\dev\\old"}));
        let mut past_count = empty_slot(3, "gate-slot-3", SlotState::Busy);
        past_count.claim = Some(json!({"pid": 77}));
        past_count.extra = Some(true);

        let wire = to_wire(
            &dashboard(vec![heavy_pool(vec![busy, held, free, past_count])]),
            &no_repos(),
        );

        assert_eq!(wire.now, Some(now()));
        assert_eq!(wire.machine, "TESTBOX");
        assert_eq!(wire.processors, 16);
        assert_eq!(wire.pools.len(), 1);
        let pool = &wire.pools[0];
        assert_eq!(pool.kind, GateSlotKind::Heavy);
        assert_eq!(pool.count, 3);
        assert_eq!(pool.default, 2);
        assert_eq!(pool.set, Some(2));
        assert_eq!(pool.problem, None);
        assert_eq!(pool.slots.len(), 4);

        let busy = &pool.slots[0];
        assert_eq!(busy.index, 0);
        assert_eq!(busy.name, "gate-slot-0");
        assert_eq!(busy.state, GateSlotState::Busy);
        assert_eq!(busy.processes, 3);
        assert_eq!(busy.last_line.as_deref(), Some("Compiling libc v0.2.170"));
        assert!(!busy.critical);
        assert_eq!(busy.extra, None);
        assert_eq!(busy.claim, Some(full_wire_claim()));
        assert_eq!(busy.stale, None);

        let held = &pool.slots[1];
        assert_eq!(held.index, 1);
        assert_eq!(held.name, "gate-slot-1");
        assert_eq!(held.state, GateSlotState::Held);
        assert_eq!(held.claim, None);
        assert_eq!(held.stale, None);

        let free = &pool.slots[2];
        assert_eq!(free.state, GateSlotState::Free);
        assert_eq!(
            free.stale,
            Some(GateClaim {
                pid: Some(99),
                started: Some(start()),
                cwd: Some("X:\\dev\\old".to_owned()),
                ..GateClaim::default()
            })
        );
        assert_eq!(free.claim, None);

        let past_count = &pool.slots[3];
        assert_eq!(past_count.index, 3);
        assert_eq!(past_count.extra, Some(true));
        assert_eq!(
            past_count.claim,
            Some(GateClaim {
                pid: Some(77),
                ..GateClaim::default()
            })
        );
    }

    /// A dashboard state whose queue, critical request, remote and builder pools and covered run are all populated.
    fn busy_dashboard() -> DashboardState {
        let mut holder = empty_slot(0, "gate-critical", SlotState::Busy);
        holder.claim = Some(json!({"pid": 500, "started": START, "command": "cargo build"}));
        let blocked = Waiting {
            pid: 700,
            since: Some(START.to_owned()),
            cwd: Some("X:\\dev\\repo".to_owned()),
            command_line: "cargo build --release".to_owned(),
        };
        let mut state = dashboard(vec![heavy_pool(Vec::new())]);
        state.critical = CriticalState {
            holder,
            waiting: vec![blocked.clone()],
        };
        state.pools[0].waiting = vec![blocked];
        let mut ticket = queued(7.0);
        ticket.kind = "critical".to_owned();
        ticket.holds = true;
        ticket.pid = 500;
        ticket.reason = Some("release".to_owned());
        ticket.expected =
            json!({"p50": 240.5, "p90": 380.25, "n": 30, "key": "local::repo | cargo build"})
                .as_object()
                .cloned();
        state.queue = vec![ticket];
        state.remote = Some(RemotePool {
            count: 2,
            problem: Some("no ssh".to_owned()),
            slots: vec![empty_slot(0, "gate-remote-slot-0", SlotState::Busy)],
        });
        state.builders = vec![BuilderPool {
            id: "0123abcd".to_owned(),
            name: Some("mac-1".to_owned()),
            host: Some("mac-1".to_owned()),
            os: Some("macos".to_owned()),
            count: 2,
            problem: None,
            routed: true,
            slots: vec![empty_slot(
                0,
                "gate-builder-0123abcd-slot-0",
                SlotState::Held,
            )],
        }];
        state.covered = vec![CoveredRun {
            pid: 800,
            start: START.to_owned(),
            parent_pid: 500,
            command: "cargo test".to_owned(),
            cwd: "X:\\dev\\repo".to_owned(),
            since: START.to_owned(),
        }];
        state
    }

    /// The queue, the critical request, the remote and builder pools and the covered runs reach the wire.
    #[test]
    fn gate_state_converts_the_queue_critical_remote_and_builders() {
        let wire = to_wire(&busy_dashboard(), &no_repos());

        let critical = wire.critical.as_ref().expect("a critical request");
        let holder = critical.holder.as_ref().expect("the critical row");
        assert_eq!(holder.index, 0);
        assert_eq!(holder.name, "gate-critical");
        assert_eq!(holder.state, GateSlotState::Busy);
        assert_eq!(holder.extra, None);
        assert_eq!(
            holder.claim,
            Some(GateClaim {
                pid: Some(500),
                started: Some(start()),
                command: Some("cargo build".to_owned()),
                command_key: Some("cargo build".to_owned()),
                ..GateClaim::default()
            })
        );
        assert_eq!(
            critical.waiting,
            vec![GateWaiting {
                pid: 700,
                since: Some(start()),
                cwd: Some("X:\\dev\\repo".to_owned()),
                command_line: "cargo build --release".to_owned(),
                command_key: Some("cargo build --release".to_owned()),
                repo: None,
            }]
        );
        assert_eq!(
            wire.pools[0].waiting,
            vec![GateWaiting {
                pid: 700,
                since: Some(start()),
                cwd: Some("X:\\dev\\repo".to_owned()),
                command_line: "cargo build --release".to_owned(),
                command_key: Some("cargo build --release".to_owned()),
                repo: None,
            }]
        );

        let ticket = &wire.queue[0];
        assert_eq!(ticket.kind, "critical");
        assert!(ticket.holds);
        assert!(!ticket.remote);
        assert!(!ticket.pinned);
        assert_eq!(ticket.pool, None);
        assert_eq!(ticket.pid, 500);
        assert_eq!(ticket.arrived, Some(start()));
        assert_eq!(ticket.age_seconds, 7.0);
        assert_eq!(ticket.cwd.as_deref(), Some("X:\\dev\\repo"));
        assert_eq!(ticket.command_line.as_deref(), Some("cargo build"));
        assert_eq!(ticket.reason.as_deref(), Some("release"));
        assert_eq!(
            ticket.expected,
            Some(GateExpected {
                p50: Some(240.5),
                p90: Some(380.25),
                n: Some(30),
                key: Some("local::repo | cargo build".to_owned()),
            })
        );

        let remote = wire.remote.as_ref().expect("a remote pool");
        assert_eq!(remote.count, 2);
        assert_eq!(remote.problem.as_deref(), Some("no ssh"));
        assert_eq!(remote.slots.len(), 1);
        assert_eq!(remote.slots[0].name, "gate-remote-slot-0");

        let builder = &wire.builders[0];
        assert_eq!(builder.id, "0123abcd");
        assert_eq!(builder.name.as_deref(), Some("mac-1"));
        assert_eq!(builder.host.as_deref(), Some("mac-1"));
        assert_eq!(builder.os.as_deref(), Some("macos"));
        assert_eq!(builder.count, 2);
        assert_eq!(builder.problem, None);
        assert!(builder.routed);
        assert_eq!(builder.slots.len(), 1);
        assert_eq!(builder.slots[0].state, GateSlotState::Held);

        let covered = &wire.covered[0];
        assert_eq!(covered.pid, 800);
        assert_eq!(covered.parent_pid, 500);
        assert_eq!(covered.start, Some(start()));
        assert_eq!(covered.since, Some(start()));
        assert_eq!(covered.command, "cargo test");
        assert_eq!(covered.cwd, "X:\\dev\\repo");
    }

    /// An unknown claim key is ignored, and a field of the wrong type becomes `None` without failing the claim.
    #[test]
    fn gate_state_ignores_unknown_and_wrong_typed_claim_fields() {
        let mut busy = empty_slot(0, "gate-slot-0", SlotState::Busy);
        busy.claim = Some(json!({
            "pid": "not a pid",
            "started": START,
            "cwd": "X:\\dev\\repo",
            "command": "cargo test",
            "log": null,
            "timeoutSeconds": "oops",
            "futureKey": {"nested": true}
        }));

        let wire = to_wire(&dashboard(vec![heavy_pool(vec![busy])]), &no_repos());

        assert_eq!(
            wire.pools[0].slots[0].claim,
            Some(GateClaim {
                pid: None,
                started: Some(start()),
                cwd: Some("X:\\dev\\repo".to_owned()),
                command: Some("cargo test".to_owned()),
                log: None,
                timeout_seconds: None,
                command_key: Some("cargo test".to_owned()),
                repo: None,
            })
        );
    }

    /// A stale claim that is `{"unreadable": …}`, or no object at all, reads as a claim with every field `None`.
    #[test]
    fn gate_state_reads_an_unreadable_stale_claim_as_empty() {
        let mut unreadable = empty_slot(0, "gate-slot-0", SlotState::Free);
        unreadable.stale = Some(json!({"unreadable": "the claim is not an object"}));
        let mut not_an_object = empty_slot(1, "gate-slot-1", SlotState::Free);
        not_an_object.stale = Some(json!([1, 2, 3]));

        let wire = to_wire(
            &dashboard(vec![heavy_pool(vec![unreadable, not_an_object])]),
            &no_repos(),
        );

        assert_eq!(wire.pools[0].slots[0].stale, Some(GateClaim::default()));
        assert_eq!(wire.pools[0].slots[1].stale, Some(GateClaim::default()));
    }

    /// An unparsable moment reaches the wire as `None` rather than failing the conversion.
    #[test]
    fn gate_state_keeps_an_unparsable_time_as_none() {
        let mut state = dashboard(vec![heavy_pool(Vec::new())]);
        state.now = "not a time".to_owned();

        let wire = to_wire(&state, &no_repos());

        assert_eq!(wire.now, None);
        assert_eq!(wire.machine, "TESTBOX");
    }

    /// No `gate` folder means no gate has ever run here, so there is no state to report.
    #[test]
    fn gate_state_reports_none_where_no_gate_ever_ran() {
        let dir = fresh_dir("gate_state_reports_none_where_no_gate_ever_ran");

        let state = read_once(
            &inputs(&dir),
            &EmptyTable,
            &QuietProbe,
            &QuietProbe,
            &no_repos(),
        );

        assert_eq!(state, None);
    }

    /// A `gate` folder with no slot in it is an idle gate, which reports its two pools.
    #[test]
    fn gate_state_reads_an_idle_gate() {
        let dir = fresh_dir("gate_state_reads_an_idle_gate");
        fs::create_dir_all(dir.join("gate").join("slots"))
            .expect("the gate's slots folder is created");

        let state = read_once(
            &inputs(&dir),
            &EmptyTable,
            &QuietProbe,
            &QuietProbe,
            &no_repos(),
        )
        .expect("an idle gate reports a state");

        assert_eq!(state.machine, "TESTBOX");
        assert_eq!(state.now, Some(now()));
        assert_eq!(state.pools.len(), 2);
        assert_eq!(state.pools[0].kind, GateSlotKind::Heavy);
        assert_eq!(state.pools[1].kind, GateSlotKind::Light);
    }

    /// The change comparison ignores the moment and the ages it computes, and sees any other difference.
    #[test]
    fn gate_state_same_state_ignores_now_and_ages() {
        let mut first = dashboard(vec![heavy_pool(vec![empty_slot(
            0,
            "gate-slot-0",
            SlotState::Busy,
        )])]);
        first.queue = vec![queued(5.0)];
        let mut later = first.clone();
        later.now = "2026-10-01T12:00:02.1234567Z".to_owned();
        later.queue[0].age_seconds = 9.0;
        let repos = no_repos();
        assert!(same_state(
            &to_wire(&first, &repos),
            &to_wire(&later, &repos)
        ));

        let mut changed = first.clone();
        changed.pools[0].slots[0].state = SlotState::Held;
        assert!(!same_state(
            &to_wire(&first, &repos),
            &to_wire(&changed, &repos)
        ));
    }

    /// The registered repos the key tests name their folders by.
    fn dev_repos() -> RepoFolders {
        RepoFolders::new([("X:\\dev\\repo", "repo")])
    }

    /// A dashboard whose heavy pool has a busy slot, a waiting gate and a queued ticket, all in `X:\dev\repo`, their
    /// raw commands normalizing to `cargo test` (the claim), `cargo build --release` (the waiting gate) and
    /// `cargo build …` (the ticket).
    fn keyed_dashboard() -> DashboardState {
        let mut busy = empty_slot(0, "gate-slot-0", SlotState::Busy);
        busy.claim = Some(json!({
            "pid": 4321,
            "started": START,
            "cwd": "X:\\dev\\repo\\crates\\a",
            "command": "cargo  test --manifest-path X:\\dev\\repo\\Cargo.toml"
        }));
        let mut pool = heavy_pool(vec![busy]);
        pool.waiting = vec![Waiting {
            pid: 700,
            since: Some(START.to_owned()),
            cwd: Some("X:\\dev\\repo".to_owned()),
            command_line: "cargo build\t--release".to_owned(),
        }];
        let mut state = dashboard(vec![pool]);
        let mut ticket = queued(3.0);
        ticket.command_line = Some("cargo build .tmp/out.txt".to_owned());
        state.queue = vec![ticket];
        state
    }

    /// A claim, a waiting gate and a queue ticket carry their raw command's normalized key, whether or not a repo
    /// holds their folder.
    #[test]
    fn gate_state_keys_live_gates_by_their_normalized_command() {
        for repos in [dev_repos(), no_repos()] {
            let wire = to_wire(&keyed_dashboard(), &repos);
            let claim = wire.pools[0].slots[0].claim.as_ref().expect("a claim");
            assert_eq!(claim.command_key.as_deref(), Some("cargo test"));
            assert_eq!(
                wire.pools[0].waiting[0].command_key.as_deref(),
                Some("cargo build --release")
            );
            assert_eq!(wire.queue[0].command_key.as_deref(), Some("cargo build …"));
        }
    }

    /// A claim, a waiting gate and a queue ticket in a registered repo's folder carry its name; with no repo holding
    /// the folder they carry none.
    #[test]
    fn gate_state_names_the_repo_of_live_gates_folders() {
        let wire = to_wire(&keyed_dashboard(), &dev_repos());
        let claim = wire.pools[0].slots[0].claim.as_ref().expect("a claim");
        assert_eq!(claim.repo.as_deref(), Some("repo"));
        assert_eq!(wire.pools[0].waiting[0].repo.as_deref(), Some("repo"));
        assert_eq!(wire.queue[0].repo.as_deref(), Some("repo"));

        let wire = to_wire(&keyed_dashboard(), &no_repos());
        let claim = wire.pools[0].slots[0].claim.as_ref().expect("a claim");
        assert_eq!(claim.repo, None);
        assert_eq!(wire.pools[0].waiting[0].repo, None);
        assert_eq!(wire.queue[0].repo, None);
    }

    /// A journal run in `state` with every field set, its moments at `START` and `NOW`.
    fn journal_run(state: RunState, pid: u32) -> HistoryRun {
        HistoryRun {
            command: "cargo test".to_owned(),
            cwd: Some("X:\\dev\\repo".to_owned()),
            repo: Some("repo".to_owned()),
            kind: "heavy".to_owned(),
            slot: Some(1),
            slots: vec!["Local\\gate-slot-1".to_owned()],
            machine: None,
            remote: false,
            host: None,
            fallback: None,
            reason: Some("release".to_owned()),
            pid,
            process_start: START.to_owned(),
            parent_pid: None,
            state,
            running: false,
            queued: Some(START.to_owned()),
            started: Some(NOW.to_owned()),
            waited_seconds: Some(1.5),
            held_seconds: Some(42.25),
            status: Some(0),
            verdict: Some("ok".to_owned()),
            wall_seconds: Some(40.0),
            adjusted_seconds: Some(38.5),
            abandoned: vec!["Local\\gate-slot-0".to_owned()],
            remote_take: None,
            cold: Some(false),
            critical_pid: Some(9),
        }
    }

    /// The wire run `journal_run(state, pid)` converts to.
    fn wire_run(state: GateRunState, pid: u32) -> GateHistoryRun {
        GateHistoryRun {
            command: "cargo test".to_owned(),
            cwd: Some("X:\\dev\\repo".to_owned()),
            repo: Some("repo".to_owned()),
            kind: "heavy".to_owned(),
            slot: Some(1),
            remote: false,
            host: None,
            fallback: None,
            reason: Some("release".to_owned()),
            pid,
            process_start: Some(start()),
            parent_pid: None,
            state,
            running: false,
            queued: Some(start()),
            started: Some(now()),
            waited_seconds: Some(1.5),
            held_seconds: Some(42.25),
            status: Some(0),
            verdict: Some("ok".to_owned()),
            wall_seconds: Some(40.0),
            adjusted_seconds: Some(38.5),
            abandoned: vec!["Local\\gate-slot-0".to_owned()],
            remote_take: None,
            cold: Some(false),
            critical_pid: Some(9),
        }
    }

    /// Every run state, the parent pid, the remote take, an unparsable moment, the open list and the summary's counts
    /// reach the wire; the machine snapshot and the slot names do not.
    #[test]
    fn gate_history_converts_runs_open_and_summary() {
        let states = [
            (RunState::Ok, GateRunState::Ok),
            (RunState::Failed, GateRunState::Failed),
            (RunState::Abandoned, GateRunState::Abandoned),
            (RunState::Running, GateRunState::Running),
            (RunState::Unrecorded, GateRunState::Unrecorded),
            (RunState::Parent, GateRunState::Parent),
            (RunState::Covered, GateRunState::Covered),
        ];
        let mut runs: Vec<HistoryRun> = (1..)
            .zip(states)
            .map(|(pid, (state, _))| journal_run(state, pid))
            .collect();
        let mut expected: Vec<GateHistoryRun> = (1..)
            .zip(states)
            .map(|(pid, (_, state))| wire_run(state, pid))
            .collect();
        runs[5].parent_pid = Some(100);
        expected[5].parent_pid = Some(100);

        let mut fell_back = journal_run(RunState::Ok, 20);
        fell_back.remote = true;
        fell_back.host = Some("mac-1".to_owned());
        fell_back.fallback = Some("unreachable".to_owned());
        fell_back.remote_take = Some(RemoteTake {
            started: START.to_owned(),
            waited_seconds: Some(3.0),
            fell_back_at: NOW.to_owned(),
        });
        runs.push(fell_back);
        let mut wire_fell_back = wire_run(GateRunState::Ok, 20);
        wire_fell_back.remote = true;
        wire_fell_back.host = Some("mac-1".to_owned());
        wire_fell_back.fallback = Some("unreachable".to_owned());
        wire_fell_back.remote_take = Some(GateRemoteTake {
            started: Some(start()),
            waited_seconds: Some(3.0),
            fell_back_at: Some(now()),
        });
        expected.push(wire_fell_back);

        let mut garbled = journal_run(RunState::Ok, 21);
        garbled.process_start = "not a time".to_owned();
        garbled.queued = Some("not a time".to_owned());
        garbled.started = None;
        garbled.remote_take = Some(RemoteTake {
            started: "not a time".to_owned(),
            waited_seconds: None,
            fell_back_at: "not a time".to_owned(),
        });
        runs.push(garbled);
        let mut wire_garbled = wire_run(GateRunState::Ok, 21);
        wire_garbled.process_start = None;
        wire_garbled.queued = None;
        wire_garbled.started = None;
        wire_garbled.remote_take = Some(GateRemoteTake {
            started: None,
            waited_seconds: None,
            fell_back_at: None,
        });
        expected.push(wire_garbled);

        let mut open = journal_run(RunState::Running, 30);
        open.running = true;
        let mut wire_open = wire_run(GateRunState::Running, 30);
        wire_open.running = true;

        let summary_row = |count| CommandSummary {
            command: "cargo test".to_owned(),
            repo: Some("repo".to_owned()),
            remote: true,
            count,
            p50_seconds: 10.0,
            p90_seconds: 20.0,
            max_seconds: 30.0,
        };
        let wire_summary_row = |count| GateCommandSummary {
            command: "cargo test".to_owned(),
            repo: Some("repo".to_owned()),
            remote: true,
            count,
            p50_seconds: 10.0,
            p90_seconds: 20.0,
            max_seconds: 30.0,
        };

        let wire = history_to_wire(&History {
            runs,
            open: vec![open],
            summary: vec![summary_row(7), summary_row(usize::MAX)],
        });

        assert_eq!(wire.runs.len(), expected.len());
        for (got, want) in wire.runs.iter().zip(&expected) {
            assert_eq!(got, want, "run {}", want.pid);
        }
        assert_eq!(wire.open, vec![wire_open]);
        assert_eq!(
            wire.summary,
            vec![wire_summary_row(7), wire_summary_row(u32::MAX)]
        );
    }

    /// A folder under a registered repo is that repo's, the deepest registered folder winning; a `<repo>.wt\<name>`
    /// folder is `<repo>`'s; any other folder is no repo's.
    #[test]
    fn gate_repo_of_maps_registered_folders_and_wt_folders() {
        let repos = RepoFolders::new([
            ("X:\\dev\\monorepo", "monorepo"),
            ("X:\\dev\\monorepo\\vendor\\lib\\", "lib"),
            ("X:\\dev\\rustling-tulip", "rustling-tulip"),
        ]);

        let cases = [
            ("X:\\dev\\monorepo", Some("monorepo")),
            ("X:\\dev\\monorepo\\crates\\a", Some("monorepo")),
            ("X:\\dev\\monorepo\\vendor\\lib", Some("lib")),
            ("X:\\dev\\monorepo\\vendor\\lib\\src", Some("lib")),
            ("X:\\dev\\monorepository", None),
            ("X:\\dev\\yaat.wt\\fix-12", Some("yaat")),
            ("X:\\dev\\yaat.WT\\fix-12\\crates\\core", Some("yaat")),
            ("X:/dev/yaat.wt/fix-12", Some("yaat")),
            ("X:\\dev\\yaat.wt", None),
            ("X:\\dev\\.wt\\fix-12", None),
            ("X:\\dev\\unknown", None),
            ("", None),
        ];
        for (folder, repo) in cases {
            assert_eq!(repos.repo_of(folder).as_deref(), repo, "{folder}");
        }
    }

    /// A step-count guard on a paused clock, far past any passing wait.
    const GUARD: Duration = Duration::from_hours(7 * 24);

    /// A wait far past any tick the tests schedule.
    const A_DAY: Duration = Duration::from_hours(24);

    /// An idle gate's wire state, on the machine `machine`.
    fn idle_on(machine: &str) -> protocol::GateState {
        let mut state = dashboard(vec![heavy_pool(Vec::new())]);
        machine.clone_into(&mut state.machine);
        to_wire(&state, &no_repos())
    }

    /// A distinct number for each poll rig's registry folder within this process; with the process id it keeps
    /// tests run in parallel processes from sharing one.
    fn next_rig() -> usize {
        static RIGS: AtomicUsize = AtomicUsize::new(0);
        RIGS.fetch_add(1, Ordering::SeqCst)
    }

    /// A daemon registry in a fresh folder named `test`, holding a repo per `(name, folder)` pair.
    fn registry(test: &str, repos: &[(&str, &str)]) -> Arc<AppState> {
        let dir = fresh_dir(test);
        let state = AppState::load_or_default(&Dirs {
            config: dir.clone(),
            state_file: dir.join("state.json"),
            handshake_file: dir.join("daemon.json"),
            remote_config_file: dir.join("remote.json"),
            sessions_dir: dir.join("sessions"),
            worktrees_dir: dir.join("worktrees"),
            binaries_dir: dir.join("binaries"),
            bin_templates: None,
        })
        .expect("the registry loads");
        if repos.is_empty() {
            return Arc::new(state);
        }
        state
            .mutate(|persisted| {
                persisted
                    .repos
                    .extend(repos.iter().map(|(name, folder)| RepoEntry {
                        id: (*name).to_owned(),
                        name: (*name).to_owned(),
                        path: (*folder).to_owned(),
                        default_branch: None,
                        default_use_worktree: true,
                        is_git: true,
                        appearance: AppearanceOverrides::default(),
                        last_agent: None,
                        last_spawn_config: None,
                    }));
            })
            .expect("the registry saves");
        Arc::new(state)
    }

    /// The user install's gate program under a fake `%LOCALAPPDATA%`.
    const USER_GATE: &str = r"C:\Users\me\AppData\Local\gate\bin\gate.exe";
    /// The admin install's gate program under a fake `%ProgramFiles%`.
    const ADMIN_GATE: &str = r"C:\Program Files\gate\bin\gate.exe";

    /// An environment holding exactly `vars`.
    fn env_of(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let vars: Vec<(String, OsString)> = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
            .collect();
        move |name: &str| {
            vars.iter()
                .find(|(known, _)| known.as_str() == name)
                .map(|(_, value)| value.clone())
        }
    }

    /// A file set holding exactly `paths`.
    fn files(paths: &[&str]) -> impl Fn(&Path) -> bool {
        let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        move |path: &Path| paths.iter().any(|known| known.as_path() == path)
    }

    #[test]
    fn gate_installed_finds_the_user_install() {
        let env = env_of(&[("LOCALAPPDATA", r"C:\Users\me\AppData\Local")]);

        assert!(gate_installed(env, files(&[USER_GATE])));
    }

    #[test]
    fn gate_installed_finds_the_admin_install() {
        let env = env_of(&[
            ("LOCALAPPDATA", r"C:\Users\me\AppData\Local"),
            ("ProgramFiles", r"C:\Program Files"),
        ]);

        assert!(gate_installed(env, files(&[ADMIN_GATE])));
    }

    #[test]
    fn gate_installed_honours_the_launcher_program_files_root() {
        let env = env_of(&[
            ("GATE_LAUNCHER_PROGRAM_FILES", r"D:\Program Files"),
            ("ProgramFiles", r"C:\Program Files"),
        ]);

        assert!(gate_installed(
            env,
            files(&[r"D:\Program Files\gate\bin\gate.exe"])
        ));
    }

    #[test]
    fn gate_installed_is_false_with_only_the_gate_folder() {
        let env = env_of(&[
            ("LOCALAPPDATA", r"C:\Users\me\AppData\Local"),
            ("ProgramFiles", r"C:\Program Files"),
        ]);
        let present = files(&[
            r"C:\Users\me\AppData\Local\gate\cutover.flag",
            r"C:\Users\me\AppData\Local\gate\v1\state.json",
        ]);

        assert!(!gate_installed(env, present));
    }

    #[test]
    fn gate_installed_ignores_program_files_under_a_launcher_root() {
        let env = env_of(&[
            ("GATE_LAUNCHER_PROGRAM_FILES", r"D:\Program Files"),
            ("ProgramFiles", r"C:\Program Files"),
        ]);

        assert!(!gate_installed(env, files(&[ADMIN_GATE])));
    }

    #[test]
    fn gate_installed_falls_back_to_program_files_under_an_empty_launcher_root() {
        let env = env_of(&[
            ("GATE_LAUNCHER_PROGRAM_FILES", ""),
            ("ProgramFiles", r"C:\Program Files"),
        ]);

        assert!(gate_installed(env, files(&[ADMIN_GATE])));
    }

    #[test]
    fn gate_installed_skips_an_empty_install_root() {
        let env = env_of(&[("LOCALAPPDATA", ""), ("ProgramFiles", "")]);
        let present = files(&[r"gate\bin\gate.exe", r"C:\Program Files\gate\bin\gate.exe"]);

        assert!(!gate_installed(env, present));
    }

    #[test]
    fn gate_installed_is_false_for_a_leftover_old_program() {
        let env = env_of(&[("LOCALAPPDATA", r"C:\Users\me\AppData\Local")]);

        assert!(!gate_installed(
            env,
            files(&[r"C:\Users\me\AppData\Local\gate\bin\gate.exe.123.old"])
        ));
    }

    #[test]
    fn gate_installed_is_false_without_the_environment() {
        assert!(!gate_installed(
            env_of(&[]),
            files(&[USER_GATE, ADMIN_GATE])
        ));
    }

    /// The poll names each live gate's repo from the repos the registry holds at the read.
    #[tokio::test(start_paused = true)]
    async fn gate_poll_names_repos_from_the_registry() {
        let state = registry(
            "gate_poll_names_repos_from_the_registry",
            &[("repo", "X:\\dev\\repo")],
        );
        let reader: GateReader =
            Arc::new(|repos: &RepoFolders| Some(to_wire(&keyed_dashboard(), repos)));
        let (events_tx, mut events) = broadcast::channel(16);
        let (_clients, clients_rx) = watch::channel(1_usize);
        let gate = GateHub::new(Arc::new(|| None), Arc::new(|_: RepoFolders| None));
        let task = spawn_poll(reader, gate, state, events_tx.clone(), clients_rx);

        let event = tokio::time::timeout(GUARD, events.recv())
            .await
            .expect("a broadcast came")
            .expect("the channel is open");
        let published = match event {
            StateEvent::GateState(Some(published)) => Some(published),
            _ => None,
        }
        .expect("a gate state was published");
        let claim = published.pools[0].slots[0].claim.as_ref().expect("a claim");
        assert_eq!(claim.repo.as_deref(), Some("repo"));
        assert_eq!(published.pools[0].waiting[0].repo.as_deref(), Some("repo"));
        assert_eq!(published.queue[0].repo.as_deref(), Some("repo"));
        task.abort();
    }

    /// A poll over a scripted reader, with the channels a test drives it by.
    struct PollRig {
        reads: Arc<AtomicUsize>,
        calls: mpsc::UnboundedReceiver<()>,
        events: broadcast::Receiver<StateEvent>,
        _events_tx: broadcast::Sender<StateEvent>,
        clients: watch::Sender<usize>,
        gate: GateHub,
        task: tokio::task::JoinHandle<()>,
    }

    /// A poll whose reads answer `script` in order, then its last entry again, with `clients` connected; each
    /// read is counted and signalled.
    fn poll_rig(script: Vec<Option<protocol::GateState>>, clients: usize) -> PollRig {
        let reads = Arc::new(AtomicUsize::new(0));
        let (signal, calls) = mpsc::unbounded_channel();
        let last = script.last().cloned().flatten();
        let queue = Mutex::new(VecDeque::from(script));
        let counted = Arc::clone(&reads);
        let reader: GateReader = Arc::new(move |_: &RepoFolders| {
            let next = queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front()
                .unwrap_or_else(|| last.clone());
            counted.fetch_add(1, Ordering::SeqCst);
            let _ = signal.send(());
            next
        });
        let (events_tx, events) = broadcast::channel(16);
        let (clients, clients_rx) = watch::channel(clients);
        let gate = GateHub::new(Arc::new(|| None), Arc::new(|_: RepoFolders| None));
        let state = registry(
            &format!("poll-rig-{}-{}", std::process::id(), next_rig()),
            &[],
        );
        let task = spawn_poll(reader, gate.clone(), state, events_tx.clone(), clients_rx);
        PollRig {
            reads,
            calls,
            events,
            _events_tx: events_tx,
            clients,
            gate,
            task,
        }
    }

    impl PollRig {
        fn reads(&self) -> usize {
            self.reads.load(Ordering::SeqCst)
        }

        /// Wait for the next read's signal.
        async fn next_read(&mut self) {
            tokio::time::timeout(GUARD, self.calls.recv())
                .await
                .expect("a read came")
                .expect("the reader is alive");
        }

        /// Wait for the next broadcast: the gate state it carries, or `None` for another event.
        async fn next_event(&mut self) -> Option<Option<protocol::GateState>> {
            let event = tokio::time::timeout(GUARD, self.events.recv())
                .await
                .expect("a broadcast came")
                .expect("the channel is open");
            let StateEvent::GateState(state) = event else {
                return None;
            };
            Some(state.map(|state| *state))
        }

        fn no_event(&mut self) -> bool {
            matches!(
                self.events.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            )
        }
    }

    /// Let the tasks run; yields only, so a passing run takes no wall time.
    async fn settle() {
        for _ in 0..1_000 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn gate_poll_sends_once_for_equal_reads() {
        let first = idle_on("TESTBOX");
        let mut later = first.clone();
        later.now = None;
        let mut rig = poll_rig(vec![Some(first.clone()), Some(later)], 1);

        rig.next_read().await;
        assert_eq!(rig.next_event().await, Some(Some(first)));
        rig.next_read().await;
        // The third read starts only once the second one's result was compared.
        rig.next_read().await;

        assert!(rig.no_event(), "an equal read sends nothing");
        rig.task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn gate_poll_sends_a_change() {
        let first = idle_on("TESTBOX");
        let second = idle_on("OTHERBOX");
        let mut rig = poll_rig(vec![Some(first.clone()), Some(second.clone())], 1);

        assert_eq!(rig.next_event().await, Some(Some(first)));
        assert_eq!(rig.next_event().await, Some(Some(second.clone())));
        assert_eq!(
            *rig.gate.current(),
            Some(second),
            "the watch holds what was sent"
        );
        rig.task.abort();
    }

    /// A read that stops finding the gate tells the clients the state is gone.
    #[tokio::test(start_paused = true)]
    async fn gate_poll_sends_the_state_gone() {
        let first = idle_on("TESTBOX");
        let mut rig = poll_rig(vec![Some(first.clone()), None], 1);

        assert_eq!(rig.next_event().await, Some(Some(first)));
        assert_eq!(
            rig.next_event().await,
            Some(None),
            "the clients are told the state is gone"
        );
        assert_eq!(*rig.gate.current(), None, "the watch holds no state");
        rig.task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn gate_poll_parks_with_no_client() {
        let mut rig = poll_rig(vec![Some(idle_on("TESTBOX"))], 0);

        tokio::time::sleep(A_DAY).await;
        settle().await;
        assert_eq!(rig.reads(), 0, "no client, no read");

        rig.clients.send_replace(1);
        rig.next_read().await;
        rig.clients.send_replace(0);
        tokio::time::sleep(A_DAY).await;
        settle().await;
        assert_eq!(rig.reads(), 1, "the last client leaving parks the poll");
        rig.task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn gate_poll_reads_at_once_on_connect() {
        let mut rig = poll_rig(vec![Some(idle_on("TESTBOX"))], 0);
        settle().await;
        let connected = tokio::time::Instant::now();

        rig.clients.send_replace(1);
        rig.next_read().await;

        assert_eq!(rig.reads(), 1);
        assert!(
            connected.elapsed() < POLL_INTERVAL,
            "the connect's read waits for no tick"
        );
        rig.task.abort();
    }
}
