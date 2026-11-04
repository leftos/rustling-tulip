//! The engine lease, `engine_lease.json`: one live auto-switch engine per account store, claimed, kept by heartbeats,
//! taken over once three of its holder's intervals pass without one or once its holder is no longer running, and
//! released only by its holder.

use std::fs;

use claude_accounts::engine::LEASE_SCHEMA_VERSION;
use claude_accounts::{EngineError, Lease, LeaseHolder, LeaseState, StoreErrorKind};
use proc_table::{EmptyTable, ProcessInfo, ProcessTable};
use serde_json::json;

use crate::support::{NOW_S, Scratch, at, read_json, write};

/// The interval this test engine records, in seconds.
const INTERVAL_S: f64 = 60.0;

/// The other engine's process id, as its lease names it.
const OTHER_PID: u32 = 4242;

/// A process table listing a fixed set of processes.
struct Table(Vec<ProcessInfo>);

impl ProcessTable for Table {
    fn snapshot(&self) -> Vec<ProcessInfo> {
        self.0.clone()
    }
}

fn process(pid: u32, created: Option<jiff::Timestamp>) -> ProcessInfo {
    ProcessInfo {
        pid,
        parent_pid: 1,
        name: "rustling-tulipd.exe".to_owned(),
        command_line: None,
        created,
    }
}

/// A table where the other engine runs, started a day before the tests' clock, so before any lease here started.
fn running() -> Table {
    Table(vec![
        process(1, None),
        process(OTHER_PID, Some(at(-86_400_000))),
    ])
}

/// A table with processes in it, none of them the other engine.
fn not_running() -> Table {
    Table(vec![
        process(1, None),
        process(OTHER_PID + 1, Some(at(-86_400_000))),
    ])
}

fn me() -> LeaseHolder {
    LeaseHolder::new("ops-tower", 1234, INTERVAL_S)
}

/// Another engine's lease, its last heartbeat `ago_ms` before the tests' clock and its start an hour before that.
fn other_lease(ago_ms: i64) -> String {
    #[expect(
        clippy::cast_precision_loss,
        reason = "the test clock's milliseconds are far inside f64's exact range"
    )]
    let heartbeat = (NOW_S * 1000 - ago_ms) as f64 / 1000.0;
    json!({
        "owner": "rustling-tulip",
        "pid": OTHER_PID,
        "startedAt": heartbeat - 3600.0,
        "heartbeatAt": heartbeat,
        "intervalSeconds": INTERVAL_S,
    })
    .to_string()
}

/// The moment `other_lease(ago_ms)` says its engine started, moved by `offset_ms`.
fn other_started_at(ago_ms: i64, offset_ms: i64) -> jiff::Timestamp {
    at(-ago_ms - 3_600_000 + offset_ms)
}

fn held_by_other(age_seconds: f64) -> LeaseState {
    LeaseState::HeldByOther {
        owner: "rustling-tulip".to_owned(),
        pid: OTHER_PID,
        age_seconds,
    }
}

#[test]
fn a_free_lease_is_claimed() {
    let scratch = Scratch::new("a_free_lease_is_claimed");
    let root = scratch.root();
    let lease = Lease::new(&root);
    let table = running();

    assert_eq!(
        lease
            .observe(&me(), at(0), &table)
            .expect("the lease is observed"),
        LeaseState::Free
    );
    assert_eq!(
        lease
            .claim_or_heartbeat(&me(), at(0), &table)
            .expect("the lease is claimed"),
        LeaseState::Ours
    );

    #[expect(
        clippy::cast_precision_loss,
        reason = "the test clock's seconds are far inside f64's exact range"
    )]
    let now = NOW_S as f64;
    assert_eq!(
        read_json(&root.engine_lease()),
        json!({"schemaVersion": LEASE_SCHEMA_VERSION, "owner": "ops-tower", "pid": 1234, "startedAt": now, "heartbeatAt": now, "intervalSeconds": INTERVAL_S})
    );
    assert_eq!(
        lease
            .observe(&me(), at(1000), &table)
            .expect("the lease is observed"),
        LeaseState::Ours
    );
}

#[test]
fn a_fresh_lease_held_by_another_engine_is_not_taken() {
    let scratch = Scratch::new("a_fresh_lease_held_by_another_engine_is_not_taken");
    let root = scratch.root();
    let lease = Lease::new(&root);
    let table = running();
    write(&root.engine_lease(), &other_lease(30_000));

    assert_eq!(
        lease
            .observe(&me(), at(0), &table)
            .expect("the lease is observed"),
        held_by_other(30.0)
    );
    assert_eq!(
        lease
            .claim_or_heartbeat(&me(), at(0), &table)
            .expect("the claim is tried"),
        held_by_other(30.0)
    );
    assert_eq!(
        read_json(&root.engine_lease())["owner"],
        json!("rustling-tulip"),
        "the other engine keeps its lease"
    );

    let same_owner_other_pid = LeaseHolder::new("rustling-tulip", 1234, INTERVAL_S);
    assert_eq!(
        lease
            .observe(&same_owner_other_pid, at(0), &table)
            .expect("the lease is observed"),
        held_by_other(30.0),
        "a lease is ours only when its owner and its pid both match"
    );
}

#[test]
fn a_lease_older_than_three_intervals_is_taken_over() {
    let scratch = Scratch::new("a_lease_older_than_three_intervals_is_taken_over");
    let root = scratch.root();
    let lease = Lease::new(&root);
    let table = running();

    write(&root.engine_lease(), &other_lease(179_500));
    assert_eq!(
        lease
            .claim_or_heartbeat(&me(), at(0), &table)
            .expect("the claim is tried"),
        held_by_other(179.5),
        "just under three intervals the lease is still held"
    );

    for ago_ms in [180_000, 500_000] {
        write(&root.engine_lease(), &other_lease(ago_ms));
        assert_eq!(
            lease
                .observe(&me(), at(0), &table)
                .expect("the lease is observed"),
            LeaseState::Free,
            "{ago_ms} ms old is stale"
        );
        assert_eq!(
            lease
                .claim_or_heartbeat(&me(), at(0), &table)
                .expect("the lease is taken over"),
            LeaseState::Ours
        );
        assert_eq!(read_json(&root.engine_lease())["owner"], json!("ops-tower"));
    }
}

#[test]
fn a_fresh_lease_whose_holder_is_not_running_is_claimed() {
    let scratch = Scratch::new("a_fresh_lease_whose_holder_is_not_running_is_claimed");
    let root = scratch.root();
    let lease = Lease::new(&root);
    let table = not_running();
    write(&root.engine_lease(), &other_lease(30_000));

    assert_eq!(
        lease
            .observe(&me(), at(0), &table)
            .expect("the lease is observed"),
        LeaseState::Free
    );
    assert_eq!(
        lease
            .claim_or_heartbeat(&me(), at(0), &table)
            .expect("the lease is claimed"),
        LeaseState::Ours
    );

    #[expect(
        clippy::cast_precision_loss,
        reason = "the test clock's seconds are far inside f64's exact range"
    )]
    let now = NOW_S as f64;
    assert_eq!(
        read_json(&root.engine_lease()),
        json!({"schemaVersion": LEASE_SCHEMA_VERSION, "owner": "ops-tower", "pid": 1234, "startedAt": now, "heartbeatAt": now, "intervalSeconds": INTERVAL_S}),
        "the claim names us and starts anew"
    );
}

#[test]
fn a_fresh_lease_whose_pid_was_reused_is_claimed() {
    let scratch = Scratch::new("a_fresh_lease_whose_pid_was_reused_is_claimed");
    let root = scratch.root();
    let lease = Lease::new(&root);
    write(&root.engine_lease(), &other_lease(30_000));

    let within_a_second = Table(vec![process(
        OTHER_PID,
        Some(other_started_at(30_000, 1000)),
    )]);
    assert_eq!(
        lease
            .observe(&me(), at(0), &within_a_second)
            .expect("the lease is observed"),
        held_by_other(30.0),
        "a process started up to a second after the lease's start is its holder"
    );

    let reused = Table(vec![process(
        OTHER_PID,
        Some(other_started_at(30_000, 10_000)),
    )]);
    assert_eq!(
        lease
            .observe(&me(), at(0), &reused)
            .expect("the lease is observed"),
        LeaseState::Free,
        "a process started after the lease did is not its holder"
    );
    assert_eq!(
        lease
            .claim_or_heartbeat(&me(), at(0), &reused)
            .expect("the lease is claimed"),
        LeaseState::Ours
    );
    assert_eq!(read_json(&root.engine_lease())["owner"], json!("ops-tower"));
}

#[test]
fn a_fresh_lease_whose_holder_start_is_unknown_is_held() {
    let scratch = Scratch::new("a_fresh_lease_whose_holder_start_is_unknown_is_held");
    let root = scratch.root();
    let lease = Lease::new(&root);
    let table = Table(vec![process(OTHER_PID, None)]);
    write(&root.engine_lease(), &other_lease(30_000));

    assert_eq!(
        lease
            .observe(&me(), at(0), &table)
            .expect("the lease is observed"),
        held_by_other(30.0)
    );
    assert_eq!(
        lease
            .claim_or_heartbeat(&me(), at(0), &table)
            .expect("the claim is tried"),
        held_by_other(30.0)
    );
    assert_eq!(
        read_json(&root.engine_lease())["owner"],
        json!("rustling-tulip")
    );
}

#[test]
fn an_empty_process_table_keeps_the_time_rule() {
    let scratch = Scratch::new("an_empty_process_table_keeps_the_time_rule");
    let root = scratch.root();
    let lease = Lease::new(&root);

    write(&root.engine_lease(), &other_lease(30_000));
    assert_eq!(
        lease
            .observe(&me(), at(0), &EmptyTable)
            .expect("the lease is observed"),
        held_by_other(30.0),
        "an empty table says nothing of the holder, so a fresh lease stays held"
    );
    assert_eq!(
        lease
            .claim_or_heartbeat(&me(), at(0), &EmptyTable)
            .expect("the claim is tried"),
        held_by_other(30.0)
    );

    write(&root.engine_lease(), &other_lease(180_000));
    assert_eq!(
        lease
            .observe(&me(), at(0), &EmptyTable)
            .expect("the lease is observed"),
        LeaseState::Free,
        "a stale lease is free whatever the table says"
    );
}

#[test]
fn our_lease_heartbeat_moves_heartbeat_at() {
    let scratch = Scratch::new("our_lease_heartbeat_moves_heartbeat_at");
    let root = scratch.root();
    let lease = Lease::new(&root);
    let table = running();
    lease
        .claim_or_heartbeat(&me(), at(0), &table)
        .expect("the lease is claimed");
    let me_faster = LeaseHolder::new("ops-tower", 1234, 30.0);

    assert_eq!(
        lease
            .observe(&me_faster, at(600_000), &table)
            .expect("the lease is observed"),
        LeaseState::Ours,
        "our own lease is ours even after it went stale"
    );
    assert_eq!(
        lease
            .claim_or_heartbeat(&me_faster, at(600_000), &table)
            .expect("the heartbeat writes"),
        LeaseState::Ours
    );

    let on_disk = read_json(&root.engine_lease());
    #[expect(
        clippy::cast_precision_loss,
        reason = "the test clock's seconds are far inside f64's exact range"
    )]
    let now = NOW_S as f64;
    assert_eq!(
        on_disk["startedAt"],
        json!(now),
        "a heartbeat keeps the claim's start, even on a stale lease"
    );
    assert_eq!(on_disk["heartbeatAt"], json!(now + 600.0));
    assert_eq!(
        on_disk["intervalSeconds"],
        json!(30.0),
        "a heartbeat records the holder's current interval"
    );
}

#[test]
fn release_removes_only_our_lease() {
    let scratch = Scratch::new("release_removes_only_our_lease");
    let root = scratch.root();
    let lease = Lease::new(&root);

    assert!(
        !lease.release(&me()).expect("releasing no lease succeeds"),
        "there was nothing to release"
    );

    write(&root.engine_lease(), &other_lease(1000));
    assert!(
        !lease.release(&me()).expect("the release is tried"),
        "another engine's lease is not ours to release"
    );
    assert!(root.engine_lease().exists(), "another engine's lease stays");

    write(&root.engine_lease(), &other_lease(1000));
    lease
        .claim_or_heartbeat(&me(), at(600_000), &running())
        .expect("the stale lease is taken over");
    assert!(
        lease.release(&me()).expect("our lease is released"),
        "our lease is removed"
    );
    assert!(!root.engine_lease().exists());
}

#[test]
fn a_corrupt_lease_counts_as_stale() {
    let scratch = Scratch::new("a_corrupt_lease_counts_as_stale");
    let root = scratch.root();
    let lease = Lease::new(&root);
    let table = running();

    for corrupt in ["{not json", r#"{"owner": "rustling-tulip"}"#] {
        write(&root.engine_lease(), corrupt);
        assert_eq!(
            lease
                .observe(&me(), at(0), &table)
                .expect("the lease is observed"),
            LeaseState::Free,
            "{corrupt} is stale"
        );
        assert_eq!(
            lease
                .claim_or_heartbeat(&me(), at(0), &table)
                .expect("the lease is claimed"),
            LeaseState::Ours
        );
        assert_eq!(read_json(&root.engine_lease())["owner"], json!("ops-tower"));
    }
}

/// Writes an `accounts.json` one `schemaVersion` past the one this crate knows, as a newer binary leaves the store.
fn make_store_newer(root: &claude_accounts::AccountsRoot) {
    write(
        &root.accounts_json(),
        &json!({"schemaVersion": 2, "order": [], "accounts": {}}).to_string(),
    );
}

/// A lease of a newer version is held by its engine whatever its times or the process table say: this engine neither
/// claims it nor removes it, and the file keeps its bytes.
#[test]
fn a_lease_of_an_unknown_version_holds_the_older_engine_off() {
    let scratch = Scratch::new("a_lease_of_an_unknown_version_holds_the_older_engine_off");
    let root = scratch.root();
    let lease = Lease::new(&root);
    let body =
        json!({"schemaVersion": 2, "owner": "newer", "pid": OTHER_PID, "heartbeatAt": NOW_S})
            .to_string();
    write(&root.engine_lease(), &body);
    let held = |state: &LeaseState| matches!(state, LeaseState::HeldByOther { owner, pid: OTHER_PID, .. } if owner == "newer");

    let observed = lease
        .observe(&me(), at(0), &not_running())
        .expect("the lease is observed");
    let claimed = lease
        .claim_or_heartbeat(&me(), at(0), &not_running())
        .expect("the claim is tried");
    let much_later = lease
        .claim_or_heartbeat(&me(), at(3_600_000), &EmptyTable)
        .expect("the claim is tried an hour later");

    assert!(
        held(&observed),
        "observe sees the newer engine's lease held: {observed:?}"
    );
    assert!(
        held(&claimed),
        "the claim leaves the newer engine's lease held: {claimed:?}"
    );
    assert!(
        held(&much_later),
        "an hour without a heartbeat still leaves it held: {much_later:?}"
    );
    assert!(
        !lease.release(&me()).expect("the release is tried"),
        "a newer engine's lease is not ours to release"
    );
    assert_eq!(
        fs::read_to_string(root.engine_lease()).expect("the lease reads"),
        body,
        "the newer engine's lease keeps its bytes"
    );
}

/// A lease written before the lease carried `schemaVersion` is this version's: ours stays ours, and its heartbeat
/// rewrites it with the version and its start kept.
#[test]
fn a_lease_written_before_its_version_field_is_still_ours() {
    let scratch = Scratch::new("a_lease_written_before_its_version_field_is_still_ours");
    let root = scratch.root();
    let lease = Lease::new(&root);
    write(
        &root.engine_lease(),
        &json!({
            "owner": "ops-tower",
            "pid": 1234,
            "startedAt": 1_789_996_400.0,
            "heartbeatAt": 1_789_999_990.0,
            "intervalSeconds": INTERVAL_S,
        })
        .to_string(),
    );

    assert_eq!(
        lease
            .observe(&me(), at(0), &running())
            .expect("the lease is observed"),
        LeaseState::Ours
    );
    assert_eq!(
        lease
            .claim_or_heartbeat(&me(), at(0), &running())
            .expect("the lease is heartbeaten"),
        LeaseState::Ours
    );
    let written = read_json(&root.engine_lease());
    assert_eq!(
        written["schemaVersion"],
        json!(LEASE_SCHEMA_VERSION),
        "the heartbeat writes the version"
    );
    assert_eq!(
        written["startedAt"],
        json!(1_789_996_400.0),
        "the heartbeat keeps the lease's start"
    );
    assert_eq!(written["heartbeatAt"], json!(1_790_000_000.0));
}

/// In a store a newer version wrote, a claim is refused before the lease file is written.
#[test]
fn a_lease_claim_into_a_newer_store_changes_nothing() {
    let scratch = Scratch::new("a_lease_claim_into_a_newer_store_changes_nothing");
    let root = scratch.root();
    make_store_newer(&root);

    let claimed = Lease::new(&root).claim_or_heartbeat(&me(), at(0), &running());

    assert!(
        matches!(
            claimed,
            Err(EngineError::StoreRefused(StoreErrorKind::UnknownSchema))
        ),
        "a newer store refuses the claim: {claimed:?}"
    );
    assert!(!root.engine_lease().exists(), "no lease file is written");
}

/// Releasing our own lease is never refused, even in a store a newer version wrote.
#[test]
fn release_is_never_refused() {
    let scratch = Scratch::new("release_is_never_refused");
    let root = scratch.root();
    let lease = Lease::new(&root);
    lease
        .claim_or_heartbeat(&me(), at(0), &running())
        .expect("the free lease is claimed");
    make_store_newer(&root);

    assert!(
        lease.release(&me()).expect("our lease is released"),
        "our lease is removed"
    );
    assert!(!root.engine_lease().exists(), "the lease file is gone");
}
