//! The gate model the Gates rail item and panel read: This PC's gate state
//! as the daemon pushes it ([`protocol::GateState`]), turned into the badge
//! count, the per-pool rows, the queue and waiting lists in age order, and
//! the short command and folder labels a row shows.

use chrono::{DateTime, Utc};
use protocol::{
    GatePool, GateQueueTicket, GateSlot, GateSlotKind, GateSlotState, GateState, GateWaiting,
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
    },
}

/// Whether `slot` counts as taken: busy, or held with no live claim.
fn taken(slot: &GateSlot) -> bool {
    matches!(slot.state, GateSlotState::Busy | GateSlotState::Held)
}

/// How many of `slots` are busy or held.
fn taken_count<'a>(slots: impl IntoIterator<Item = &'a GateSlot>) -> u32 {
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

/// A pool kind's name in a header, upper-case.
fn kind_title(kind: GateSlotKind) -> &'static str {
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
    let mut pools: Vec<&GatePool> = state.pools.iter().collect();
    pools.sort_by_key(|pool| kind_rank(pool.kind));
    let mut rows = Vec::new();
    for pool in pools {
        rows.push(GateRow::PoolHeader {
            kind: pool.kind,
            text: header_text(pool),
        });
        let mut slots: Vec<&GateSlot> = pool.slots.iter().collect();
        slots.sort_by_key(|slot| (slot.extra.unwrap_or(false), slot.index));
        rows.extend(slots.into_iter().map(|slot| {
            let claim = slot.claim.as_ref();
            GateRow::Slot {
                kind: pool.kind,
                index: slot.index,
                state: slot.state,
                command: claim.and_then(|claim| claim.command.clone()),
                cwd: claim.and_then(|claim| claim.cwd.clone()),
                started: claim.and_then(|claim| claim.started),
                pid: claim.and_then(|claim| claim.pid),
                last_line: slot.last_line.clone(),
                stale_pid: slot.stale.as_ref().and_then(|stale| stale.pid),
                extra: slot.extra.unwrap_or(false),
            }
        }));
    }
    rows
}

/// The key that sorts `time` oldest first, an unknown time last.
fn oldest_first(time: Option<DateTime<Utc>>) -> (bool, Option<DateTime<Utc>>) {
    (time.is_none(), time)
}

/// The queue tickets, oldest arrival first; a ticket with no arrival time
/// sorts last.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the Gates view lists the queue")
)]
#[must_use]
pub(crate) fn queue(state: &GateState) -> Vec<&GateQueueTicket> {
    let mut tickets: Vec<&GateQueueTicket> = state.queue.iter().collect();
    tickets.sort_by_key(|ticket| oldest_first(ticket.arrived));
    tickets
}

/// The gates waiting for a slot of any local pool, oldest first; one with
/// no start time sorts last.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the Gates view lists the waiting gates")
)]
#[must_use]
pub(crate) fn waiting(state: &GateState) -> Vec<&GateWaiting> {
    let mut waiting: Vec<&GateWaiting> =
        state.pools.iter().flat_map(|pool| &pool.waiting).collect();
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
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the Gates view's slot rows show it")
)]
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
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the Gates view's slot rows show it")
)]
#[must_use]
pub(crate) fn folder_label(path: &str) -> String {
    let parts: Vec<&str> = path
        .split(['\\', '/'])
        .filter(|part| !part.is_empty())
        .collect();
    let from = parts.len().saturating_sub(2);
    parts[from..].join("/")
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::*;
    use protocol::GateClaim;

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
        }
    }

    fn waiter(pid: u32, since: Option<&str>) -> GateWaiting {
        GateWaiting {
            pid,
            since: since.map(time),
            cwd: None,
            command_line: String::new(),
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
            } => format!(
                "slot {kind:?} {index} {state:?} {command:?} {cwd:?} {} {pid:?} {last_line:?} {stale_pid:?} {extra}",
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
                "slot Heavy 0 Free None None false None None Some(40) false",
                r#"slot Heavy 1 Busy Some("cargo nextest run") Some("X:\\dev\\repo") true Some(41) Some("Compiling") None false"#,
                r#"slot Heavy 2 Busy Some("cargo nextest run") Some("X:\\dev\\repo") true Some(42) None None true"#,
                "header Light LIGHT · 1 of 1 busy",
                "slot Light 0 Held None None false None None None false",
                "header Unknown UNKNOWN · 0 of 1 busy",
                "slot Unknown 0 Free None None false None None None false",
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
    fn waiting_is_oldest_first_across_pools_with_no_time_last() {
        let mut heavy = pool(GateSlotKind::Heavy, 1, Vec::new());
        heavy.waiting = vec![waiter(1, None), waiter(2, Some("2026-11-05T10:00:00Z"))];
        let mut light = pool(GateSlotKind::Light, 1, Vec::new());
        light.waiting = vec![waiter(3, Some("2026-11-05T09:00:00Z"))];
        let gates = state(vec![heavy, light]);
        let pids: Vec<u32> = waiting(&gates).iter().map(|w| w.pid).collect();
        assert_eq!(pids, [3, 2, 1]);
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
}
