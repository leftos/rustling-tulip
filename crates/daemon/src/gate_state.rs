//! The gate's live state, read through leftos/gate's `gate-state` crate.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the hub's gate poll calls these, and is wired separately"
    )
)]

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use tracing::warn;

/// A count as the wire carries it, saturating at `u32::MAX`.
fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// An ISO 8601 moment as the gate writes it, or `None` when it does not parse.
fn iso(text: &str) -> Option<DateTime<Utc>> {
    text.parse::<DateTime<Utc>>().ok()
}

/// A slot's claim or stale claim as the wire carries it: each field from its own camel case key, each `None` when
/// missing or of another type, and every field `None` for a value that is not an object.
fn claim_of(value: &Value) -> protocol::GateClaim {
    let Value::Object(fields) = value else {
        return protocol::GateClaim::default();
    };
    protocol::GateClaim {
        pid: fields
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok()),
        started: fields.get("started").and_then(Value::as_str).and_then(iso),
        cwd: fields.get("cwd").and_then(Value::as_str).map(str::to_owned),
        command: fields
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_owned),
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
fn slot_of(row: &::gate_state::SlotRow) -> protocol::GateSlot {
    protocol::GateSlot {
        index: row.index,
        name: row.name.clone(),
        state: match row.state {
            ::gate_state::SlotState::Free => protocol::GateSlotState::Free,
            ::gate_state::SlotState::Busy => protocol::GateSlotState::Busy,
            ::gate_state::SlotState::Held => protocol::GateSlotState::Held,
        },
        claim: row.claim.as_ref().map(claim_of),
        stale: row.stale.as_ref().map(claim_of),
        processes: count(row.processes),
        last_line: row.last_line.clone(),
        critical: row.critical,
        extra: row.extra,
    }
}

/// A gate waiting for a slot.
fn waiting_of(gate: &::gate_state::Waiting) -> protocol::GateWaiting {
    protocol::GateWaiting {
        pid: gate.pid,
        since: gate.since.as_deref().and_then(iso),
        cwd: gate.cwd.clone(),
        command_line: gate.command_line.clone(),
    }
}

/// One local pool of slots.
fn pool_of(pool: &::gate_state::PoolState) -> protocol::GatePool {
    protocol::GatePool {
        kind: match pool.kind {
            ::gate_state::SlotKind::Heavy => protocol::GateSlotKind::Heavy,
            ::gate_state::SlotKind::Light => protocol::GateSlotKind::Light,
        },
        count: pool.count,
        default: pool.default_count,
        set: pool.set.map(::gate_state::SlotCount::get),
        problem: pool.problem.clone(),
        slots: pool.slots.iter().map(slot_of).collect(),
        waiting: pool.waiting.iter().map(waiting_of).collect(),
    }
}

/// The remote pool.
fn remote_of(pool: &::gate_state::RemotePool) -> protocol::GateRemotePool {
    protocol::GateRemotePool {
        count: pool.count,
        problem: pool.problem.clone(),
        slots: pool.slots.iter().map(slot_of).collect(),
    }
}

/// One builder's own remote pool.
fn builder_of(pool: &::gate_state::BuilderPool) -> protocol::GateBuilderPool {
    protocol::GateBuilderPool {
        id: pool.id.clone(),
        name: pool.name.clone(),
        host: pool.host.clone(),
        os: pool.os.clone(),
        count: pool.count,
        problem: pool.problem.clone(),
        routed: pool.routed,
        slots: pool.slots.iter().map(slot_of).collect(),
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
fn ticket_of(ticket: &::gate_state::QueueTicket) -> protocol::GateQueueTicket {
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
        command_line: ticket.command_line.clone(),
        reason: ticket.reason.clone(),
        expected: ticket.expected.as_ref().map(expected_of),
    }
}

/// Converts the gate's dashboard state to the wire state the client reads.
pub(crate) fn to_wire(state: &::gate_state::DashboardState) -> protocol::GateState {
    protocol::GateState {
        now: iso(&state.now),
        machine: state.machine.clone(),
        processors: count(state.processors),
        critical: Some(protocol::GateCriticalState {
            holder: Some(slot_of(&state.critical.holder)),
            waiting: state.critical.waiting.iter().map(waiting_of).collect(),
        }),
        queue: state.queue.iter().map(ticket_of).collect(),
        pools: state.pools.iter().map(pool_of).collect(),
        remote: state.remote.as_ref().map(remote_of),
        builders: state.builders.iter().map(builder_of).collect(),
        covered: state.covered.iter().map(covered_of).collect(),
    }
}

/// Reads the gate's state from the given inputs and probes, or `None` when no gate has ever run on this machine.
///
/// The signal is the `gate` folder under the per-user local application data folder, which the gate creates before it
/// writes anything; a failed check also reads as no state, and is logged.
pub(crate) fn read_once(
    inputs: &::gate_state::StateInputs,
    processes: &dyn ::proc_table::ProcessTable,
    mutexes: &dyn ::gate_state::MutexProbe,
    cwds: &dyn ::gate_state::CwdReader,
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
    Some(to_wire(&::gate_state::dashboard_state(
        inputs, processes, mutexes, cwds,
    )))
}

/// Reads this host's live gate state, or `None` when the gate's inputs cannot be read or no gate has run here.
#[cfg(all(windows, target_pointer_width = "64"))]
#[cfg_attr(
    test,
    expect(
        dead_code,
        reason = "the hub's gate poll calls it, and is wired separately"
    )
)]
pub(crate) fn read_live() -> Option<protocol::GateState> {
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

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert setup and fixtures with expect"
)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use ::gate_state::{
        BuilderPool, CoveredRun, CriticalState, CwdReader, DashboardState, Generation, MutexProbe,
        PoolState, QueueTicket, RemotePool, SlotCount, SlotKind, SlotRow, SlotState, StateInputs,
        Waiting,
    };
    use ::proc_table::EmptyTable;
    use chrono::{DateTime, Utc};
    use serde_json::{Value, json};

    use protocol::{GateClaim, GateExpected, GateSlotKind, GateSlotState, GateWaiting};

    use super::{read_once, same_state, to_wire};

    /// The moment a test describes.
    const NOW: &str = "2026-10-01T12:00:00.1234567Z";
    /// When a test's gate started, as the gate writes it.
    const START: &str = "2026-10-01T11:00:00.5000000Z";

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
        }
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

        let wire = to_wire(&dashboard(vec![heavy_pool(vec![
            busy, held, free, past_count,
        ])]));

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
        let wire = to_wire(&busy_dashboard());

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
            }]
        );
        assert_eq!(
            wire.pools[0].waiting,
            vec![GateWaiting {
                pid: 700,
                since: Some(start()),
                cwd: Some("X:\\dev\\repo".to_owned()),
                command_line: "cargo build --release".to_owned(),
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

        let wire = to_wire(&dashboard(vec![heavy_pool(vec![busy])]));

        assert_eq!(
            wire.pools[0].slots[0].claim,
            Some(GateClaim {
                pid: None,
                started: Some(start()),
                cwd: Some("X:\\dev\\repo".to_owned()),
                command: Some("cargo test".to_owned()),
                log: None,
                timeout_seconds: None,
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

        let wire = to_wire(&dashboard(vec![heavy_pool(vec![
            unreadable,
            not_an_object,
        ])]));

        assert_eq!(wire.pools[0].slots[0].stale, Some(GateClaim::default()));
        assert_eq!(wire.pools[0].slots[1].stale, Some(GateClaim::default()));
    }

    /// An unparsable moment reaches the wire as `None` rather than failing the conversion.
    #[test]
    fn gate_state_keeps_an_unparsable_time_as_none() {
        let mut state = dashboard(vec![heavy_pool(Vec::new())]);
        state.now = "not a time".to_owned();

        let wire = to_wire(&state);

        assert_eq!(wire.now, None);
        assert_eq!(wire.machine, "TESTBOX");
    }

    /// No `gate` folder means no gate has ever run here, so there is no state to report.
    #[test]
    fn gate_state_reports_none_where_no_gate_ever_ran() {
        let dir = fresh_dir("gate_state_reports_none_where_no_gate_ever_ran");

        let state = read_once(&inputs(&dir), &EmptyTable, &QuietProbe, &QuietProbe);

        assert_eq!(state, None);
    }

    /// A `gate` folder with no slot in it is an idle gate, which reports its two pools.
    #[test]
    fn gate_state_reads_an_idle_gate() {
        let dir = fresh_dir("gate_state_reads_an_idle_gate");
        fs::create_dir_all(dir.join("gate").join("slots"))
            .expect("the gate's slots folder is created");

        let state = read_once(&inputs(&dir), &EmptyTable, &QuietProbe, &QuietProbe)
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
        assert!(same_state(&to_wire(&first), &to_wire(&later)));

        let mut changed = first.clone();
        changed.pools[0].slots[0].state = SlotState::Held;
        assert!(!same_state(&to_wire(&first), &to_wire(&changed)));
    }
}
