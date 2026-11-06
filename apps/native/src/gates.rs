//! The gate model the Gates rail item and panel read: This PC's gate state
//! as the daemon pushes it ([`protocol::GateState`]), turned into the badge
//! count, the per-pool rows, the queue and waiting lists in age order, and
//! the short command and folder labels a row shows.

use chrono::{DateTime, Utc};
use protocol::{
    GateCommandSummary, GateHistoryRun, GatePool, GateQueueTicket, GateRunState, GateSlot,
    GateSlotKind, GateSlotState, GateState, GateWaiting,
};

/// One row of the Gates panel's pool list, in [`rows`]'s order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GateRow {
    /// A pool's header line, such as `HEAVY · 2 of 3 busy`.
    PoolHeader { kind: GateSlotKind, text: String },
    /// One slot of the pool the last header names.
    Slot {
        kind: GateSlotKind,
        index: u32,
        state: GateSlotState,
        /// The live claim's command.
        command: Option<String>,
        /// The live claim's working folder.
        cwd: Option<String>,
        /// When the live claim took the slot.
        started: Option<DateTime<Utc>>,
        /// The live claim's process id.
        pid: Option<u32>,
        /// The last non-blank line of the holding gate's log.
        last_line: Option<String>,
        /// The process id of a claim that is not live.
        stale_pid: Option<u32>,
        /// Past the pool's count, shown only because it has a claim.
        extra: bool,
        /// The live claim's log file.
        log: Option<String>,
        /// The live claim's ceiling, in seconds.
        timeout_seconds: Option<f64>,
        /// How many processes the slot's gate runs.
        processes: u32,
        /// The live claim's command as the history's summary keys it.
        command_key: Option<String>,
        /// The repo the live claim's folder belongs to.
        repo: Option<String>,
    },
}

/// The fewest slots a pool's count stepper sets.
pub(crate) const MIN_COUNT: u32 = 1;
/// The most slots a pool's count stepper sets.
pub(crate) const MAX_COUNT: u32 = 64;
/// A time younger than this, in seconds, reads in seconds.
const SECONDS_SHOWN_BELOW: i64 = 60;

/// Whether `slot` counts as taken: busy, or held with no live claim.
fn taken(slot: &GateSlot) -> bool {
    matches!(slot.state, GateSlotState::Busy | GateSlotState::Held)
}

/// How many of `slots` are busy or held.
#[must_use]
pub(crate) fn taken_count<'a>(slots: impl IntoIterator<Item = &'a GateSlot>) -> u32 {
    let count = slots.into_iter().filter(|slot| taken(slot)).count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// The rail badge's count: the busy and held slots of every local pool,
/// leaving out the remote and builder pools.
#[must_use]
pub(crate) fn busy_count(state: &GateState) -> u32 {
    taken_count(state.pools.iter().flat_map(|pool| &pool.slots))
}

/// Where a pool kind sorts: heavy, then light, then a kind this build does
/// not know.
fn kind_rank(kind: GateSlotKind) -> u8 {
    match kind {
        GateSlotKind::Heavy => 0,
        GateSlotKind::Light => 1,
        GateSlotKind::Unknown => 2,
    }
}

/// A pool kind's name in a slot's name and a selector, lower-case.
#[must_use]
pub(crate) fn kind_name(kind: GateSlotKind) -> &'static str {
    match kind {
        GateSlotKind::Heavy => "heavy",
        GateSlotKind::Light => "light",
        GateSlotKind::Unknown => "unknown",
    }
}

/// A pool kind's name in a header, upper-case.
#[must_use]
pub(crate) fn kind_title(kind: GateSlotKind) -> &'static str {
    match kind {
        GateSlotKind::Heavy => "HEAVY",
        GateSlotKind::Light => "LIGHT",
        GateSlotKind::Unknown => "UNKNOWN",
    }
}

/// `pool`'s header text: `"<KIND> · <busy> of <count> busy"`, busy counting
/// its busy and held slots.
fn header_text(pool: &GatePool) -> String {
    format!(
        "{} · {} of {} busy",
        kind_title(pool.kind),
        taken_count(&pool.slots),
        pool.count
    )
}

/// The local pools' rows: per pool, heavy first, then light, then any kind
/// this build does not know, a header and then its slots by index, the
/// extra slots past the count last.
#[must_use]
pub(crate) fn rows(state: &GateState) -> Vec<GateRow> {
    let mut rows = Vec::new();
    for pool in local_pools(state) {
        rows.push(GateRow::PoolHeader {
            kind: pool.kind,
            text: header_text(pool),
        });
        rows.extend(slot_rows(pool.kind, &pool.slots));
    }
    rows
}

/// The local pools, heavy first, then light, then any kind this build does
/// not know.
#[must_use]
pub(crate) fn local_pools(state: &GateState) -> Vec<&GatePool> {
    let mut pools: Vec<&GatePool> = state.pools.iter().collect();
    pools.sort_by_key(|pool| kind_rank(pool.kind));
    pools
}

/// `slots` as rows of a pool of `kind`, by index with the extra slots past
/// the count last. A remote or builder pool's rows take
/// [`GateSlotKind::Unknown`], since they have no local kind.
#[must_use]
pub(crate) fn slot_rows(kind: GateSlotKind, slots: &[GateSlot]) -> Vec<GateRow> {
    let mut slots: Vec<&GateSlot> = slots.iter().collect();
    slots.sort_by_key(|slot| (slot.extra.unwrap_or(false), slot.index));
    slots
        .into_iter()
        .map(|slot| {
            let claim = slot.claim.as_ref();
            GateRow::Slot {
                kind,
                index: slot.index,
                state: slot.state,
                command: claim.and_then(|claim| claim.command.clone()),
                cwd: claim.and_then(|claim| claim.cwd.clone()),
                started: claim.and_then(|claim| claim.started),
                pid: claim.and_then(|claim| claim.pid),
                last_line: slot.last_line.clone(),
                stale_pid: slot.stale.as_ref().and_then(|stale| stale.pid),
                extra: slot.extra.unwrap_or(false),
                log: claim.and_then(|claim| claim.log.clone()),
                timeout_seconds: claim.and_then(|claim| claim.timeout_seconds),
                processes: slot.processes,
                command_key: claim.and_then(|claim| claim.command_key.clone()),
                repo: claim.and_then(|claim| claim.repo.clone()),
            }
        })
        .collect()
}

/// The next count a stepper press sets from `count`, one up or down;
/// `None` past [`MIN_COUNT`] to [`MAX_COUNT`].
#[must_use]
pub(crate) fn stepped(count: u32, up: bool) -> Option<u32> {
    let next = if up {
        count.checked_add(1)?
    } else {
        count.checked_sub(1)?
    };
    (MIN_COUNT..=MAX_COUNT).contains(&next).then_some(next)
}

/// The critical block's note: who holds the critical gate and how many
/// wait for it; `None` when nothing holds or waits.
#[must_use]
pub(crate) fn critical_note(state: &GateState) -> Option<String> {
    let critical = state.critical.as_ref()?;
    let holder = critical.holder.as_ref().map(|slot| {
        slot.claim
            .as_ref()
            .and_then(|claim| claim.command.as_deref())
            .map_or_else(|| "a gate with no claim".to_owned(), short_command)
    });
    let waiting = critical.waiting.len();
    match (holder, waiting) {
        (None, 0) => None,
        (None, n) => Some(format!("{n} waiting")),
        (Some(holder), 0) => Some(format!("held by {holder}")),
        (Some(holder), n) => Some(format!("held by {holder} · {n} waiting")),
    }
}

/// The footer strip's text: `Gates <taken>/<total>` over the local pools.
#[must_use]
pub(crate) fn strip_text(state: &GateState) -> String {
    let total: u32 = state.pools.iter().map(|pool| pool.count).sum();
    format!("Gates {}/{total}", busy_count(state))
}

/// A busy row's tooltip: the full command and folder, then the log, the
/// ceiling and the process count, each line only when known.
#[must_use]
pub(crate) fn slot_tooltip(
    command: Option<&str>,
    cwd: Option<&str>,
    log: Option<&str>,
    timeout_seconds: Option<f64>,
    processes: u32,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.extend(command.map(str::to_owned));
    lines.extend(cwd.map(str::to_owned));
    lines.extend(log.map(|log| format!("Log: {log}")));
    lines.extend(timeout_seconds.map(|secs| format!("Ceiling: {secs:.0} s")));
    lines.push(format!("Processes: {processes}"));
    lines.join("\n")
}

/// Whether some time the panel shows, a claim's hold or a gate's wait,
/// still reads in seconds at `now`.
#[must_use]
pub(crate) fn fresh(state: &GateState, now: DateTime<Utc>) -> bool {
    let young = |time: Option<DateTime<Utc>>| {
        time.is_some_and(|time| (now - time).num_seconds() < SECONDS_SHOWN_BELOW)
    };
    let slots = state
        .pools
        .iter()
        .flat_map(|pool| &pool.slots)
        .chain(state.remote.iter().flat_map(|remote| &remote.slots))
        .chain(state.builders.iter().flat_map(|builder| &builder.slots));
    let waits = state
        .pools
        .iter()
        .flat_map(|pool| &pool.waiting)
        .chain(state.critical.iter().flat_map(|critical| &critical.waiting));
    slots
        .filter_map(|slot| slot.claim.as_ref())
        .any(|claim| young(claim.started))
        || waits.into_iter().any(|gate| young(gate.since))
        || state.queue.iter().any(|ticket| young(ticket.arrived))
}

/// `n` as an English ordinal: `1st`, `2nd`, `3rd`, `4th`, `11th`, `21st`.
#[must_use]
pub(crate) fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// The key that sorts `time` oldest first, an unknown time last.
fn oldest_first(time: Option<DateTime<Utc>>) -> (bool, Option<DateTime<Utc>>) {
    (time.is_none(), time)
}

/// The queue tickets, oldest arrival first; a ticket with no arrival time
/// sorts last.
#[must_use]
pub(crate) fn queue(state: &GateState) -> Vec<&GateQueueTicket> {
    let mut tickets: Vec<&GateQueueTicket> = state.queue.iter().collect();
    tickets.sort_by_key(|ticket| oldest_first(ticket.arrived));
    tickets
}

/// The gates waiting for a slot of `pool`, oldest first; one with no start
/// time sorts last.
#[must_use]
pub(crate) fn pool_waiting(pool: &GatePool) -> Vec<&GateWaiting> {
    let mut waiting: Vec<&GateWaiting> = pool.waiting.iter().collect();
    waiting.sort_by_key(|gate| oldest_first(gate.since));
    waiting
}

/// The program extensions a row drops from a program's name, and that an
/// unquoted program path with spaces in it may end in; lower-case.
const PROGRAM_EXTENSIONS: [&str; 4] = [".exe", ".cmd", ".bat", ".com"];

/// `text` without its program extension, in any case, when it ends in one;
/// `None` when it ends in none.
fn without_program_extension(text: &str) -> Option<&str> {
    let lower = text.to_ascii_lowercase();
    PROGRAM_EXTENSIONS
        .iter()
        .find(|ext| lower.ends_with(*ext))
        .and_then(|ext| text.get(..text.len() - ext.len()))
}

/// Whether `text` ends in a program extension, in any case.
fn has_program_extension(text: &str) -> bool {
    without_program_extension(text).is_some()
}

/// Splits `command` into its program and the rest. A quoted program runs
/// to its closing quote; an unquoted one runs to the first space, unless
/// the first word is a path and a longer prefix that ends at a space ends
/// in a program extension with a path separator after its first space
/// (`C:\Program Files\...\pwsh.exe`).
fn split_program(command: &str) -> (&str, &str) {
    if let Some(quoted) = command.strip_prefix('"') {
        return match quoted.find('"') {
            Some(end) => (&quoted[..end], &quoted[end + 1..]),
            None => (quoted, ""),
        };
    }
    let mut ends = command
        .char_indices()
        .filter(|(_, c)| c.is_whitespace())
        .map(|(at, _)| at)
        .chain(std::iter::once(command.len()));
    let first = ends.next().unwrap_or(command.len());
    if command[..first].contains(['\\', '/']) {
        // A prefix past the first word counts only when a separator follows
        // its first space, so the space sits inside the path rather than
        // before an argument.
        let program = std::iter::once(first).chain(ends).find(|&end| {
            let inside_path = end == first || command[first..end].contains(['\\', '/']);
            inside_path && has_program_extension(&command[..end])
        });
        if let Some(end) = program {
            return (&command[..end], &command[end..]);
        }
    }
    (&command[..first], &command[first..])
}

/// A command as a row shows it: the program's file name with no folder and
/// no program extension (`.exe`, `.cmd`, `.bat`, `.com`; any other dot
/// stays), then a space and the rest unchanged; a quoted program loses
/// its quotes, and an empty command stays empty.
#[must_use]
pub(crate) fn short_command(command: &str) -> String {
    let (program, rest) = split_program(command.trim_start());
    let name = program.rsplit(['\\', '/']).next().unwrap_or(program);
    let name = without_program_extension(name).unwrap_or(name);
    let rest = rest.trim_start();
    if rest.is_empty() {
        name.to_owned()
    } else {
        format!("{name} {rest}")
    }
}

/// A folder as a row shows it: its last two components joined by `/`; a
/// one-component path is itself.
#[must_use]
pub(crate) fn folder_label(path: &str) -> String {
    let parts: Vec<&str> = path
        .split(['\\', '/'])
        .filter(|part| !part.is_empty())
        .collect();
    let from = parts.len().saturating_sub(2);
    parts[from..].join("/")
}

/// How far back the Recent runs block reaches for a finished run.
const RECENT_WINDOW: chrono::TimeDelta = chrono::TimeDelta::hours(2);
/// The most rows the Recent runs block shows.
const RECENT_MAX: usize = 5;
/// The most rows the Last 7 days block shows.
const WEEK_MAX: usize = 4;

/// How a recent run ended, as its row's glyph and word show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunResult {
    Ok,
    Running,
    Failed,
    /// Failed with status 124, the gate's timeout.
    TimedOut,
    Abandoned,
    /// A parent, covered, unrecorded or unknown run.
    Other,
}

impl RunResult {
    /// The row's leading glyph.
    #[must_use]
    pub(crate) fn glyph(self) -> &'static str {
        match self {
            Self::Ok => "✓",
            Self::Running => "◉",
            Self::Failed => "✕",
            Self::TimedOut => "⏱",
            Self::Abandoned => "⏸",
            Self::Other => "·",
        }
    }

    /// The word a bad row's second line carries.
    #[must_use]
    pub(crate) fn word(self) -> Option<&'static str> {
        match self {
            Self::Failed => Some("failed"),
            Self::TimedOut => Some("timed out"),
            Self::Abandoned => Some("abandoned"),
            Self::Ok | Self::Running | Self::Other => None,
        }
    }

    /// Whether the row is washed and barred in the danger colour.
    #[must_use]
    pub(crate) fn bad(self) -> bool {
        self.word().is_some()
    }
}

/// `run`'s result: its state, with a failure at status 124 timed out.
#[must_use]
pub(crate) fn run_result(run: &GateHistoryRun) -> RunResult {
    match run.state {
        GateRunState::Ok => RunResult::Ok,
        GateRunState::Running => RunResult::Running,
        GateRunState::Failed if run.status == Some(124) => RunResult::TimedOut,
        GateRunState::Failed => RunResult::Failed,
        GateRunState::Abandoned => RunResult::Abandoned,
        GateRunState::Unrecorded
        | GateRunState::Parent
        | GateRunState::Covered
        | GateRunState::Unknown => RunResult::Other,
    }
}

/// Whether `run` is still going: its gate is live or its state says so.
fn run_running(run: &GateHistoryRun) -> bool {
    run.running || run.state == GateRunState::Running
}

/// When `run` ended: its start plus its held time, else its start, else
/// when it queued.
#[must_use]
pub(crate) fn run_end(run: &GateHistoryRun) -> Option<DateTime<Utc>> {
    match (run.started, run.held_seconds) {
        (Some(started), Some(held)) => {
            Some(started + chrono::TimeDelta::milliseconds(rounded(held.max(0.0) * 1000.0)))
        }
        (Some(started), None) => Some(started),
        (None, _) => run.queued,
    }
}

/// `value` rounded to a whole number, saturating at the bounds of `i64`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "a time in seconds or milliseconds is far inside i64; `as` saturates past it"
)]
fn rounded(value: f64) -> i64 {
    value.round() as i64
}

/// The runs the Recent runs block shows: `open` and `runs` merged by pid
/// and process start, the open entry winning; covered runs left out, as
/// their parent's row shows the work; a run kept while running or when it
/// ended within two hours of `now`; running runs first, then newest end
/// first; at most five.
#[must_use]
pub(crate) fn recent_runs<'a>(
    runs: &'a [GateHistoryRun],
    open: &'a [GateHistoryRun],
    now: DateTime<Utc>,
) -> Vec<&'a GateHistoryRun> {
    let key = |run: &GateHistoryRun| (run.pid, run.process_start);
    let mut merged: Vec<&GateHistoryRun> = open.iter().collect();
    merged.extend(
        runs.iter()
            .filter(|run| !open.iter().any(|live| key(live) == key(run))),
    );
    merged.retain(|run| {
        run.state != GateRunState::Covered
            && (run_running(run) || run_end(run).is_some_and(|end| now - end <= RECENT_WINDOW))
    });
    merged.sort_by_key(|run| std::cmp::Reverse((run_running(run), run_end(run))));
    merged.truncate(RECENT_MAX);
    merged
}

/// A held time as a row shows it: `42s`, `1m 48s`, `1h 03m`; empty when
/// unknown.
#[must_use]
pub(crate) fn held_text(seconds: Option<f64>) -> String {
    let Some(seconds) = seconds else {
        return String::new();
    };
    let seconds = rounded(seconds.max(0.0));
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m {:02}s", seconds % 60);
    }
    format!("{}h {:02}m", minutes / 60, minutes % 60)
}

/// A recent run's second line before its tag and age: the repo (the
/// folder's label with none), `· {kind} #{slot + 1}` (the kind alone with
/// no slot), and on a bad row `· {word}` and ` · exit {status}`.
#[must_use]
pub(crate) fn run_detail(run: &GateHistoryRun) -> String {
    let repo = run
        .repo
        .clone()
        .or_else(|| run.cwd.as_deref().map(folder_label))
        .unwrap_or_default();
    let slot = match run.slot {
        Some(slot) => format!("{} #{}", run.kind, u64::from(slot) + 1),
        None => run.kind.clone(),
    };
    let mut detail = format!("{repo} · {slot}");
    if let Some(word) = run_result(run).word() {
        detail.push_str(" · ");
        detail.push_str(word);
        if let Some(status) = run.status {
            detail.push_str(" · exit ");
            detail.push_str(&status.to_string());
        }
    }
    detail
}

/// A Last 7 days row's run count: `1 run`, else `{count} runs`.
#[must_use]
pub(crate) fn runs_text(count: u32) -> String {
    if count == 1 {
        "1 run".to_owned()
    } else {
        format!("{count} runs")
    }
}

/// The rows the Last 7 days block shows: the first four, in wire order.
#[must_use]
pub(crate) fn week_rows(summary: &[GateCommandSummary]) -> &[GateCommandSummary] {
    &summary[..summary.len().min(WEEK_MAX)]
}

/// The usual time of a live gate, in seconds: the `p50_seconds` of the
/// summary row whose command, repo and remote flag are the gate's
/// `command_key`, `repo` and `remote`; `None` with no key or no such row.
#[must_use]
pub(crate) fn usual(
    summary: &[GateCommandSummary],
    command_key: Option<&str>,
    repo: Option<&str>,
    remote: bool,
) -> Option<f64> {
    let key = command_key?;
    summary
        .iter()
        .find(|row| row.command == key && row.repo.as_deref() == repo && row.remote == remote)
        .map(|row| row.p50_seconds)
}

/// A usual time as a row shows it after its held time or age: ` / 4m`,
/// read as [`crate::needs_you::waited`] reads a wait.
#[must_use]
pub(crate) fn usual_text(seconds: f64) -> String {
    let since = DateTime::<Utc>::UNIX_EPOCH;
    let now = since + chrono::TimeDelta::milliseconds(rounded(seconds.max(0.0) * 1000.0));
    format!(" / {}", crate::needs_you::waited(now, since))
}

/// Whether a held time or age of `seconds` is over the `usual`.
#[must_use]
pub(crate) fn over_usual(seconds: i64, usual: Option<f64>) -> bool {
    #[expect(
        clippy::cast_precision_loss,
        reason = "a held time in seconds is far inside f64's exact range"
    )]
    let seconds = seconds as f64;
    usual.is_some_and(|usual| seconds > usual)
}

/// How long the footer keeps a failed run's tail after it ended.
const TAIL_WINDOW: chrono::TimeDelta = chrono::TimeDelta::minutes(10);

/// The footer pill's tail for a run that failed lately.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FailureTail {
    /// `✕`, or `⏱` for a timeout.
    pub(crate) glyph: &'static str,
    /// `failed`, or `timed out` for a timeout.
    pub(crate) word: &'static str,
    /// The run's repo, else its folder's label; `None` with neither.
    pub(crate) repo: Option<String>,
    /// When the run ended, by [`run_end`].
    pub(crate) end: DateTime<Utc>,
}

impl FailureTail {
    /// The tail's text after the pill's count:
    /// ` · ✕ failed · {repo} · {ago} ago`, the repo part left out when
    /// unknown.
    #[must_use]
    pub(crate) fn text(&self, now: DateTime<Utc>) -> String {
        let mut text = format!(" · {} {}", self.glyph, self.word);
        if let Some(repo) = &self.repo {
            text.push_str(" · ");
            text.push_str(repo);
        }
        text.push_str(" · ");
        text.push_str(&crate::needs_you::waited(now, self.end));
        text.push_str(" ago");
        text
    }
}

/// The footer's failure tail: the newest failed run of `runs` by its end,
/// when it ended within ten minutes of `now`; a failure at status 124 is a
/// timeout. `None` when the newest failure is older or there is none.
#[must_use]
pub(crate) fn failure_tail(runs: &[GateHistoryRun], now: DateTime<Utc>) -> Option<FailureTail> {
    let (run, end) = runs
        .iter()
        .filter(|run| run.state == GateRunState::Failed)
        .filter_map(|run| run_end(run).map(|end| (run, end)))
        .max_by_key(|(_, end)| *end)?;
    if now - end > TAIL_WINDOW {
        return None;
    }
    let result = run_result(run);
    Some(FailureTail {
        glyph: result.glyph(),
        word: result.word().unwrap_or("failed"),
        repo: run
            .repo
            .clone()
            .or_else(|| run.cwd.as_deref().map(folder_label)),
        end,
    })
}

/// Whether some time the history shows, a recent run's age or the failure
/// tail's, still reads in seconds at `now`: a run of `runs` or `open` that
/// ended (or, running, started) under a minute ago.
#[must_use]
pub(crate) fn history_fresh(
    runs: &[GateHistoryRun],
    open: &[GateHistoryRun],
    now: DateTime<Utc>,
) -> bool {
    runs.iter()
        .chain(open)
        .filter_map(run_end)
        .any(|end| (now - end).num_seconds() < SECONDS_SHOWN_BELOW)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::*;
    use protocol::{
        GateClaim, GateCommandSummary, GateCriticalState, GateHistoryRun, GateRunState,
    };

    fn time(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("an RFC 3339 time")
            .with_timezone(&Utc)
    }

    fn slot(index: u32, state: GateSlotState) -> GateSlot {
        GateSlot {
            index,
            name: format!("gate-slot-{index}"),
            state,
            claim: None,
            stale: None,
            processes: 0,
            last_line: None,
            critical: false,
            extra: Some(false),
        }
    }

    fn claim(pid: u32) -> GateClaim {
        GateClaim {
            pid: Some(pid),
            started: Some(time("2026-11-05T09:58:00Z")),
            cwd: Some(r"X:\dev\repo".to_owned()),
            command: Some("cargo nextest run".to_owned()),
            log: None,
            timeout_seconds: None,
            command_key: None,
            repo: None,
        }
    }

    fn pool(kind: GateSlotKind, count: u32, slots: Vec<GateSlot>) -> GatePool {
        GatePool {
            kind,
            count,
            default: count,
            set: None,
            problem: None,
            slots,
            waiting: Vec::new(),
        }
    }

    fn state(pools: Vec<GatePool>) -> GateState {
        GateState {
            now: None,
            machine: "BOX".to_owned(),
            processors: 16,
            critical: None,
            queue: Vec::new(),
            pools,
            remote: None,
            builders: Vec::new(),
            covered: Vec::new(),
        }
    }

    fn ticket(pid: u32, arrived: Option<&str>) -> GateQueueTicket {
        GateQueueTicket {
            kind: "heavy".to_owned(),
            holds: false,
            remote: false,
            pool: None,
            pinned: false,
            pid,
            arrived: arrived.map(time),
            age_seconds: 0.0,
            cwd: None,
            command_line: None,
            reason: None,
            expected: None,
            command_key: None,
            repo: None,
        }
    }

    fn waiter(pid: u32, since: Option<&str>) -> GateWaiting {
        GateWaiting {
            pid,
            since: since.map(time),
            cwd: None,
            command_line: String::new(),
            command_key: None,
            repo: None,
        }
    }

    /// A row as one line, every field read, so a test compares rows whole.
    fn line(row: &GateRow) -> String {
        match row {
            GateRow::PoolHeader { kind, text } => format!("header {kind:?} {text}"),
            GateRow::Slot {
                kind,
                index,
                state,
                command,
                cwd,
                started,
                pid,
                last_line,
                stale_pid,
                extra,
                log,
                timeout_seconds,
                processes,
                command_key: _,
                repo: _,
            } => format!(
                "slot {kind:?} {index} {state:?} {command:?} {cwd:?} {} {pid:?} {last_line:?} {stale_pid:?} {extra} {log:?} {timeout_seconds:?} {processes}",
                started.is_some()
            ),
        }
    }

    #[test]
    fn busy_count_counts_busy_and_held_local_slots() {
        let mut gates = state(vec![
            pool(
                GateSlotKind::Heavy,
                3,
                vec![
                    slot(0, GateSlotState::Busy),
                    slot(1, GateSlotState::Held),
                    slot(2, GateSlotState::Free),
                ],
            ),
            pool(
                GateSlotKind::Light,
                2,
                vec![
                    slot(0, GateSlotState::Busy),
                    slot(1, GateSlotState::Unknown),
                ],
            ),
        ]);
        gates.remote = Some(protocol::GateRemotePool {
            count: 1,
            problem: None,
            slots: vec![slot(0, GateSlotState::Busy)],
        });
        gates.builders = vec![protocol::GateBuilderPool {
            id: "b1".to_owned(),
            name: None,
            host: None,
            os: None,
            count: 1,
            problem: None,
            routed: true,
            slots: vec![slot(0, GateSlotState::Held)],
        }];
        assert_eq!(
            busy_count(&gates),
            3,
            "busy and held in both local pools; not free, unknown, remote or builder slots"
        );
        assert_eq!(busy_count(&state(Vec::new())), 0);
    }

    #[test]
    fn rows_put_heavy_then_light_then_unknown_with_extras_last() {
        let busy = GateSlot {
            claim: Some(claim(41)),
            last_line: Some("Compiling".to_owned()),
            ..slot(1, GateSlotState::Busy)
        };
        let free_stale = GateSlot {
            stale: Some(claim(40)),
            ..slot(0, GateSlotState::Free)
        };
        let extra = GateSlot {
            claim: Some(claim(42)),
            extra: Some(true),
            ..slot(2, GateSlotState::Busy)
        };
        let gates = state(vec![
            pool(GateSlotKind::Unknown, 1, vec![slot(0, GateSlotState::Free)]),
            pool(GateSlotKind::Light, 1, vec![slot(0, GateSlotState::Held)]),
            pool(GateSlotKind::Heavy, 2, vec![extra, busy, free_stale]),
        ]);
        let lines: Vec<String> = rows(&gates).iter().map(line).collect();
        assert_eq!(
            lines,
            [
                "header Heavy HEAVY · 2 of 2 busy",
                "slot Heavy 0 Free None None false None None Some(40) false None None 0",
                r#"slot Heavy 1 Busy Some("cargo nextest run") Some("X:\\dev\\repo") true Some(41) Some("Compiling") None false None None 0"#,
                r#"slot Heavy 2 Busy Some("cargo nextest run") Some("X:\\dev\\repo") true Some(42) None None true None None 0"#,
                "header Light LIGHT · 1 of 1 busy",
                "slot Light 0 Held None None false None None None false None None 0",
                "header Unknown UNKNOWN · 0 of 1 busy",
                "slot Unknown 0 Free None None false None None None false None None 0",
            ]
        );
    }

    #[test]
    fn queue_is_oldest_first_with_no_time_last() {
        let mut gates = state(Vec::new());
        gates.queue = vec![
            ticket(1, None),
            ticket(2, Some("2026-11-05T10:00:00Z")),
            ticket(3, Some("2026-11-05T09:00:00Z")),
        ];
        let pids: Vec<u32> = queue(&gates).iter().map(|t| t.pid).collect();
        assert_eq!(pids, [3, 2, 1]);
    }

    #[test]
    fn pool_waiting_is_oldest_first_with_no_time_last() {
        let mut heavy = pool(GateSlotKind::Heavy, 1, Vec::new());
        heavy.waiting = vec![
            waiter(1, None),
            waiter(2, Some("2026-11-05T10:00:00Z")),
            waiter(3, Some("2026-11-05T09:00:00Z")),
        ];
        let pids: Vec<u32> = pool_waiting(&heavy).iter().map(|w| w.pid).collect();
        assert_eq!(pids, [3, 2, 1]);
        let none: Vec<u32> = pool_waiting(&pool(GateSlotKind::Light, 1, Vec::new()))
            .iter()
            .map(|w| w.pid)
            .collect();
        assert_eq!(none, Vec::<u32>::new());
    }

    #[test]
    fn stepped_stays_within_one_to_sixty_four() {
        assert_eq!(stepped(3, true), Some(4));
        assert_eq!(stepped(3, false), Some(2));
        assert_eq!(stepped(64, true), None, "64 is the most");
        assert_eq!(stepped(1, false), None, "1 is the least");
        assert_eq!(
            stepped(0, true),
            Some(1),
            "an out-of-range count steps back in"
        );
        assert_eq!(stepped(65, false), Some(64));
    }

    #[test]
    fn critical_note_names_the_holder_and_the_waiting_count() {
        let mut gates = state(Vec::new());
        assert_eq!(critical_note(&gates), None, "no critical state");
        gates.critical = Some(GateCriticalState {
            holder: None,
            waiting: Vec::new(),
        });
        assert_eq!(critical_note(&gates), None, "no holder and no waiter");
        gates.critical = Some(GateCriticalState {
            holder: None,
            waiting: vec![waiter(1, None)],
        });
        assert_eq!(critical_note(&gates).as_deref(), Some("1 waiting"));
        gates.critical = Some(GateCriticalState {
            holder: Some(GateSlot {
                claim: Some(claim(7)),
                ..slot(0, GateSlotState::Busy)
            }),
            waiting: vec![waiter(1, None), waiter(2, None)],
        });
        assert_eq!(
            critical_note(&gates).as_deref(),
            Some("held by cargo nextest run · 2 waiting")
        );
        gates.critical = Some(GateCriticalState {
            holder: Some(slot(0, GateSlotState::Held)),
            waiting: Vec::new(),
        });
        assert_eq!(
            critical_note(&gates).as_deref(),
            Some("held by a gate with no claim")
        );
    }

    #[test]
    fn strip_text_counts_the_taken_local_slots_of_their_total() {
        let mut gates = state(vec![
            pool(
                GateSlotKind::Heavy,
                3,
                vec![
                    slot(0, GateSlotState::Busy),
                    slot(1, GateSlotState::Held),
                    slot(2, GateSlotState::Free),
                ],
            ),
            pool(
                GateSlotKind::Light,
                2,
                vec![slot(0, GateSlotState::Busy), slot(1, GateSlotState::Free)],
            ),
        ]);
        gates.remote = Some(protocol::GateRemotePool {
            count: 1,
            problem: None,
            slots: vec![slot(0, GateSlotState::Busy)],
        });
        assert_eq!(
            strip_text(&gates),
            "Gates 3/5",
            "the remote pool is left out"
        );
    }

    #[test]
    fn slot_tooltip_lists_command_folder_log_ceiling_and_processes() {
        assert_eq!(
            slot_tooltip(
                Some("cargo nextest run"),
                Some(r"X:\dev\repo"),
                Some(r"C:\logs\g.log"),
                Some(600.0),
                3
            ),
            "cargo nextest run\nX:\\dev\\repo\nLog: C:\\logs\\g.log\nCeiling: 600 s\nProcesses: 3"
        );
        assert_eq!(slot_tooltip(None, None, None, None, 0), "Processes: 0");
    }

    #[test]
    fn fresh_holds_while_a_claim_or_a_wait_is_under_a_minute() {
        let now = time("2026-11-05T10:00:00Z");
        let young = GateSlot {
            claim: Some(GateClaim {
                started: Some(time("2026-11-05T09:59:30Z")),
                ..claim(1)
            }),
            ..slot(0, GateSlotState::Busy)
        };
        assert!(fresh(
            &state(vec![pool(GateSlotKind::Heavy, 1, vec![young])]),
            now
        ));
        let old = GateSlot {
            claim: Some(claim(1)),
            ..slot(0, GateSlotState::Busy)
        };
        assert!(
            !fresh(&state(vec![pool(GateSlotKind::Heavy, 1, vec![old])]), now),
            "two minutes held"
        );
        let mut light = pool(GateSlotKind::Light, 1, Vec::new());
        light.waiting = vec![waiter(2, Some("2026-11-05T09:59:50Z"))];
        assert!(fresh(&state(vec![light]), now), "a young wait");
        assert!(!fresh(&state(Vec::new()), now));
    }

    #[test]
    fn ordinal_reads_1st_2nd_3rd_then_th() {
        let read: Vec<String> = [1, 2, 3, 4, 11, 12, 13, 21, 22]
            .into_iter()
            .map(ordinal)
            .collect();
        assert_eq!(
            read,
            [
                "1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd"
            ]
        );
    }

    #[test]
    fn short_command_drops_the_programs_folder_and_extension() {
        assert_eq!(
            short_command(r"C:\Program Files\PowerShell\7\pwsh.exe -NoProfile -File x.ps1"),
            "pwsh -NoProfile -File x.ps1"
        );
        assert_eq!(
            short_command(r#""C:\Program Files\Git\bin\git.exe" status --short"#),
            "git status --short",
            "a quoted first word loses its quotes"
        );
        assert_eq!(short_command("cargo nextest run"), "cargo nextest run");
        assert_eq!(short_command(r"C:\tools\gate.exe"), "gate", "one word");
        assert_eq!(short_command("cargo"), "cargo");
        assert_eq!(short_command(""), "");
    }

    #[test]
    fn short_command_keeps_a_path_programs_arguments_out_of_it() {
        assert_eq!(
            short_command(r"C:\Program Files\PowerShell\7\pwsh.exe -NoProfile"),
            "pwsh -NoProfile",
            "the space sits inside the path"
        );
        assert_eq!(
            short_command("/usr/bin/cargo run tool.exe --x"),
            "cargo run tool.exe --x",
            "an argument ending in .exe is no part of the program"
        );
        assert_eq!(
            short_command(r"C:\tools\cargo run x.exe"),
            "cargo run x.exe"
        );
    }

    #[test]
    fn short_command_strips_only_a_program_extension() {
        assert_eq!(short_command("python3.11 -c x"), "python3.11 -c x");
        assert_eq!(short_command(r"C:\Tools\PWSH.EXE"), "PWSH", "any case");
        assert_eq!(short_command("node.js-runner"), "node.js-runner");
    }

    #[test]
    fn folder_label_is_the_last_two_components() {
        assert_eq!(
            folder_label(r"X:\dev\rustling-tulip.wt\rt-12"),
            "rustling-tulip.wt/rt-12"
        );
        assert_eq!(folder_label("/home/me/src/repo/"), "src/repo");
        assert_eq!(folder_label("repo"), "repo", "one component is itself");
        assert_eq!(folder_label(""), "");
    }

    /// The fixed now the history tests measure from.
    fn history_now() -> DateTime<Utc> {
        time("2026-11-05T12:00:00Z")
    }

    /// A finished run of `pid` in `state` that held its slot one minute and
    /// ended `ago` minutes before [`history_now`].
    fn history_run(pid: u32, state: GateRunState, ago: i64) -> GateHistoryRun {
        let end = history_now() - chrono::Duration::minutes(ago);
        GateHistoryRun {
            command: format!("cargo test {pid}"),
            cwd: Some(r"X:\dev\repo".to_owned()),
            repo: Some("repo".to_owned()),
            kind: "heavy".to_owned(),
            slot: Some(0),
            remote: false,
            host: None,
            fallback: None,
            reason: None,
            pid,
            process_start: Some(end - chrono::Duration::minutes(2)),
            parent_pid: None,
            state,
            running: false,
            queued: Some(end - chrono::Duration::minutes(2)),
            started: Some(end - chrono::Duration::minutes(1)),
            waited_seconds: Some(60.0),
            held_seconds: Some(60.0),
            status: (state == GateRunState::Ok).then_some(0),
            verdict: None,
            wall_seconds: None,
            adjusted_seconds: None,
            abandoned: Vec::new(),
            remote_take: None,
            cold: None,
            critical_pid: None,
        }
    }

    fn pids(runs: &[&GateHistoryRun]) -> Vec<u32> {
        runs.iter().map(|run| run.pid).collect()
    }

    #[test]
    fn recent_runs_dedupe_runs_and_open_with_the_open_entry_winning() {
        let ended = history_run(10, GateRunState::Ok, 5);
        let mut live = history_run(10, GateRunState::Running, 5);
        live.running = true;
        let other = history_run(11, GateRunState::Ok, 7);
        let runs = [ended, other];
        let open = [live];
        let recent = recent_runs(&runs, &open, history_now());
        assert_eq!(pids(&recent), [10, 11]);
        assert_eq!(
            recent[0].state,
            GateRunState::Running,
            "the open entry wins"
        );

        let mut restarted = history_run(10, GateRunState::Failed, 3);
        restarted.process_start = Some(history_now() - chrono::Duration::minutes(4));
        let runs = [restarted];
        let recent = recent_runs(&runs, &open, history_now());
        assert_eq!(
            recent.len(),
            2,
            "the same pid with another process start is another run"
        );
    }

    #[test]
    fn recent_runs_skip_covered_runs() {
        let runs = [
            history_run(1, GateRunState::Covered, 1),
            history_run(2, GateRunState::Parent, 2),
        ];
        assert_eq!(pids(&recent_runs(&runs, &[], history_now())), [2]);
    }

    #[test]
    fn recent_runs_drop_a_run_that_ended_more_than_two_hours_ago() {
        let runs = [
            history_run(1, GateRunState::Ok, 121),
            history_run(2, GateRunState::Ok, 119),
        ];
        assert_eq!(pids(&recent_runs(&runs, &[], history_now())), [2]);

        let mut queued_only = history_run(3, GateRunState::Abandoned, 0);
        queued_only.started = None;
        queued_only.held_seconds = None;
        queued_only.queued = Some(history_now() - chrono::Duration::minutes(121));
        assert_eq!(
            pids(&recent_runs(&[queued_only], &[], history_now())),
            [0u32; 0],
            "with no start, the queue time is the end"
        );
    }

    #[test]
    fn recent_runs_keep_a_running_run_whatever_its_age() {
        let mut old = history_run(1, GateRunState::Running, 600);
        old.running = true;
        old.held_seconds = None;
        assert_eq!(pids(&recent_runs(&[], &[old], history_now())), [1]);
    }

    #[test]
    fn recent_runs_are_newest_first_and_at_most_five() {
        let mut running = history_run(9, GateRunState::Running, 90);
        running.running = true;
        let runs = [
            history_run(1, GateRunState::Ok, 50),
            history_run(2, GateRunState::Ok, 10),
            history_run(3, GateRunState::Failed, 30),
            history_run(4, GateRunState::Ok, 20),
            history_run(5, GateRunState::Ok, 40),
            history_run(6, GateRunState::Ok, 60),
        ];
        assert_eq!(
            pids(&recent_runs(&runs, &[running], history_now())),
            [9, 2, 4, 3, 5],
            "a running run first, then by end, newest first; five kept"
        );
    }

    #[test]
    fn run_result_maps_each_state_and_124_to_timed_out() {
        let result = |state, status| {
            let mut run = history_run(1, state, 1);
            run.status = status;
            run_result(&run)
        };
        assert_eq!(result(GateRunState::Ok, Some(0)), RunResult::Ok);
        assert_eq!(result(GateRunState::Running, None), RunResult::Running);
        assert_eq!(result(GateRunState::Failed, Some(1)), RunResult::Failed);
        assert_eq!(result(GateRunState::Failed, None), RunResult::Failed);
        assert_eq!(result(GateRunState::Failed, Some(124)), RunResult::TimedOut);
        assert_eq!(result(GateRunState::Abandoned, None), RunResult::Abandoned);
        for state in [
            GateRunState::Parent,
            GateRunState::Unrecorded,
            GateRunState::Unknown,
            GateRunState::Covered,
        ] {
            assert_eq!(result(state, None), RunResult::Other, "{state:?}");
        }

        let shown = |result: RunResult| (result.glyph(), result.word(), result.bad());
        assert_eq!(shown(RunResult::Ok), ("✓", None, false));
        assert_eq!(shown(RunResult::Running), ("◉", None, false));
        assert_eq!(shown(RunResult::Failed), ("✕", Some("failed"), true));
        assert_eq!(shown(RunResult::TimedOut), ("⏱", Some("timed out"), true));
        assert_eq!(shown(RunResult::Abandoned), ("⏸", Some("abandoned"), true));
        assert_eq!(shown(RunResult::Other), ("·", None, false));
    }

    #[test]
    fn held_text_reads_seconds_minutes_and_hours() {
        assert_eq!(held_text(None), "");
        assert_eq!(held_text(Some(42.0)), "42s");
        assert_eq!(held_text(Some(108.0)), "1m 48s");
        assert_eq!(held_text(Some(3780.0)), "1h 03m");
    }

    #[test]
    fn runs_text_is_singular_for_one_run() {
        assert_eq!(runs_text(1), "1 run");
        assert_eq!(runs_text(2), "2 runs");
    }

    #[test]
    fn week_rows_are_the_first_four_in_wire_order() {
        let row = |command: &str, count| GateCommandSummary {
            command: command.to_owned(),
            repo: Some("repo".to_owned()),
            remote: false,
            count,
            p50_seconds: 60.0,
            p90_seconds: 90.0,
            max_seconds: 120.0,
        };
        let summary = [
            row("a", 1),
            row("b", 9),
            row("c", 3),
            row("d", 7),
            row("e", 20),
        ];
        let commands: Vec<&str> = week_rows(&summary)
            .iter()
            .map(|row| row.command.as_str())
            .collect();
        assert_eq!(commands, ["a", "b", "c", "d"]);
        assert!(week_rows(&summary[..2]).len() == 2, "fewer than four stay");
    }

    /// A summary row of `command` in `repo`, local or `remote`, whose
    /// median is `p50` seconds.
    fn summary_row(command: &str, repo: &str, remote: bool, p50: f64) -> GateCommandSummary {
        GateCommandSummary {
            command: command.to_owned(),
            repo: Some(repo.to_owned()),
            remote,
            count: 5,
            p50_seconds: p50,
            p90_seconds: p50 * 2.0,
            max_seconds: p50 * 3.0,
        }
    }

    #[test]
    fn usual_is_the_p50_of_the_row_matching_key_repo_and_remote() {
        let summary = [
            summary_row("cargo test", "other", false, 10.0),
            summary_row("cargo build", "repo", false, 20.0),
            summary_row("cargo test", "repo", false, 30.0),
            summary_row("cargo test", "repo", true, 40.0),
        ];
        assert_eq!(
            usual(&summary, Some("cargo test"), Some("repo"), false),
            Some(30.0)
        );
        assert_eq!(
            usual(&summary, Some("cargo test"), Some("repo"), true),
            Some(40.0)
        );
        assert_eq!(
            usual(&summary, Some("cargo test"), Some("other"), false),
            Some(10.0)
        );
    }

    #[test]
    fn usual_ignores_a_local_row_for_a_remote_gate() {
        let summary = [summary_row("cargo test", "repo", false, 30.0)];
        assert_eq!(
            usual(&summary, Some("cargo test"), Some("repo"), true),
            None
        );
    }

    #[test]
    fn usual_is_none_with_no_matching_row() {
        let summary = [summary_row("cargo test", "repo", false, 30.0)];
        assert_eq!(
            usual(&summary, Some("cargo build"), Some("repo"), false),
            None
        );
        assert_eq!(usual(&summary, Some("cargo test"), None, false), None);
        assert_eq!(usual(&[], Some("cargo test"), Some("repo"), false), None);
    }

    #[test]
    fn usual_is_none_with_no_command_key() {
        let summary = [summary_row("cargo test", "repo", false, 30.0)];
        assert_eq!(usual(&summary, None, Some("repo"), false), None);
    }

    #[test]
    fn failure_tail_shows_a_run_failed_nine_minutes_ago() {
        let mut run = history_run(7, GateRunState::Failed, 9);
        run.status = Some(1);
        let tail = failure_tail(&[run], history_now()).expect("a tail");
        assert_eq!(tail.glyph, "✕");
        assert_eq!(tail.word, "failed");
        assert_eq!(tail.repo.as_deref(), Some("repo"));
        assert_eq!(tail.end, history_now() - chrono::Duration::minutes(9));
        assert_eq!(tail.text(history_now()), " · ✕ failed · repo · 9m ago");
    }

    #[test]
    fn failure_tail_drops_a_run_failed_eleven_minutes_ago() {
        let run = history_run(7, GateRunState::Failed, 11);
        assert_eq!(failure_tail(&[run], history_now()), None);
    }

    #[test]
    fn failure_tail_reads_status_124_as_timed_out() {
        let mut run = history_run(7, GateRunState::Failed, 3);
        run.status = Some(124);
        let tail = failure_tail(&[run], history_now()).expect("a tail");
        assert_eq!(tail.glyph, "⏱");
        assert_eq!(tail.word, "timed out");
    }

    #[test]
    fn failure_tail_takes_the_newest_of_two_failed_runs() {
        let mut older = history_run(1, GateRunState::Failed, 8);
        older.repo = Some("older".to_owned());
        let mut newer = history_run(2, GateRunState::Failed, 2);
        newer.repo = Some("newer".to_owned());
        let tail = failure_tail(&[older, newer], history_now()).expect("a tail");
        assert_eq!(tail.repo.as_deref(), Some("newer"));
    }

    #[test]
    fn failure_tail_is_none_for_an_ok_run() {
        let run = history_run(7, GateRunState::Ok, 1);
        assert_eq!(failure_tail(&[run], history_now()), None);
    }
}
