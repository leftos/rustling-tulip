//! `autoswitch_state.json`: read as an engine writes it, written back with every key this crate does not model, and
//! only ever under `.autoswitch_state.lock`.

use std::fs;
#[cfg(windows)]
use std::fs::{File, OpenOptions};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
#[cfg(windows)]
use std::path::Path;
use std::time::Duration;

use claude_accounts::engine::{
    Event, EventBody, Lease, LeaseHolder, LeaseOwner, OwnerExit, PaidFallback,
};
use claude_accounts::{
    AutoswitchState, EngineError, FileLock, LockError, QuarantineRecord, StoreErrorKind,
};
use proc_table::EmptyTable;
use serde_json::json;

use crate::support::{Scratch, read_json, write};

/// A state file as an engine writes it after a proactive switch from account 1 to account 2, with account 3 quarantined.
const WRITTEN_STATE: &str = r#"{
  "schemaVersion": 1,
  "quarantine": {
    "3": {
      "email": "carol@example.com",
      "reason": "invalid_grant",
      "at": "2026-09-21T13:00:00Z",
      "refreshTokenFingerprint": "0f1e2d3c4b5a6978"
    }
  },
  "lastSwitchAt": 1789999000.25,
  "lastSwitchTo": "2",
  "lastSwitchFrom": 1,
  "leftHeadroom": 12.5,
  "leftRecoveryAt": null,
  "leftTrigger": "proactive"
}"#;

#[test]
fn state_reads_a_file_another_engine_wrote() {
    let scratch = Scratch::new("state_reads_a_file_another_engine_wrote");
    let root = scratch.root();
    write(&root.autoswitch_state(), WRITTEN_STATE);

    let state = AutoswitchState::read(&root).expect("the state file reads");

    assert_eq!(state.schema_version, Some(1));
    assert_eq!(state.last_switch_at, Some(1_789_999_000.25));
    assert_eq!(state.last_switch_to.as_deref(), Some("2"));
    assert_eq!(state.last_switch_from, Some(1));
    assert_eq!(state.left_headroom, Some(Some(12.5)));
    assert_eq!(
        state.left_recovery_at,
        Some(None),
        "a null recovery time is present, and means never"
    );
    assert_eq!(state.left_trigger.as_deref(), Some("proactive"));
    let entry = state
        .quarantine
        .get("3")
        .and_then(QuarantineRecord::entry)
        .expect("account 3 is quarantined with an entry of the quarantine's shape");
    assert_eq!(entry.email, "carol@example.com");
    assert_eq!(entry.reason, "invalid_grant");
    assert_eq!(entry.at, "2026-09-21T13:00:00Z");
    assert_eq!(
        entry.refresh_token_fingerprint.as_deref(),
        Some("0f1e2d3c4b5a6978")
    );
    assert!(
        state.extra.is_empty(),
        "every key of the written file is modelled: {:?}",
        state.extra
    );
}

#[test]
fn state_keeps_unknown_keys_on_write() {
    let scratch = Scratch::new("state_keeps_unknown_keys_on_write");
    let root = scratch.root();
    let mut document: serde_json::Value =
        serde_json::from_str(WRITTEN_STATE).expect("the fixture is JSON");
    document["futureKey"] = json!({"nested": [1, 2, 3]});
    document["quarantine"]["3"]["note"] = json!("kept");
    write(&root.autoswitch_state(), &document.to_string());

    let written =
        AutoswitchState::update(&root, |state| state.last_switch_at = Some(1_790_000_000.5))
            .expect("the update writes");

    assert_eq!(written.last_switch_at, Some(1_790_000_000.5));
    let on_disk = read_json(&root.autoswitch_state());
    assert_eq!(
        on_disk["futureKey"],
        json!({"nested": [1, 2, 3]}),
        "an unknown top-level key round-trips"
    );
    assert_eq!(
        on_disk["quarantine"]["3"]["note"],
        json!("kept"),
        "an unknown quarantine key round-trips"
    );
    assert_eq!(on_disk["lastSwitchAt"], json!(1_790_000_000.5));
    assert_eq!(
        on_disk["leftRecoveryAt"],
        json!(null),
        "a null recovery time stays null"
    );
    assert_eq!(on_disk["lastSwitchFrom"], json!(1));
    assert_eq!(
        on_disk["quarantine"]["3"]["refreshTokenFingerprint"],
        json!("0f1e2d3c4b5a6978")
    );
}

#[test]
fn a_missing_or_corrupt_state_file_reads_as_default() {
    let scratch = Scratch::new("a_missing_or_corrupt_state_file_reads_as_default");
    let root = scratch.root();

    assert_eq!(
        AutoswitchState::read(&root).expect("a missing file reads"),
        AutoswitchState::default()
    );
    for corrupt in ["{not json", "[1, 2]"] {
        write(&root.autoswitch_state(), corrupt);
        let state = AutoswitchState::read(&root).expect("a corrupt file reads");
        assert_eq!(
            state,
            AutoswitchState::default(),
            "{corrupt} reads as the default"
        );
    }

    let written = AutoswitchState::update(&root, |state| {
        state.left_trigger = Some("failover".to_owned());
    })
    .expect("the update writes");
    assert_eq!(
        written.schema_version,
        Some(1),
        "an update stamps the state schema version"
    );
    assert_eq!(
        read_json(&root.autoswitch_state()),
        json!({"schemaVersion": 1, "leftTrigger": "failover"})
    );

    // A key of the wrong type reads as absent but is kept, as the state is read key by key.
    write(
        &root.autoswitch_state(),
        r#"{"lastSwitchAt": "soon", "lastSwitchFrom": "1", "leftTrigger": "proactive"}"#,
    );
    let state = AutoswitchState::read(&root).expect("a file with a wrong-typed key reads");
    assert_eq!(state.last_switch_at, None);
    assert_eq!(state.last_switch_from, None);
    assert_eq!(
        state.left_trigger.as_deref(),
        Some("proactive"),
        "the well-typed keys still read"
    );
    AutoswitchState::update(&root, |state| {
        state.left_trigger = Some("failover".to_owned());
    })
    .expect("the update writes");
    assert_eq!(
        read_json(&root.autoswitch_state()),
        json!({"lastSwitchAt": "soon", "lastSwitchFrom": "1", "leftTrigger": "failover", "schemaVersion": 1}),
        "the wrong-typed keys survive the update unchanged"
    );
}

#[test]
fn a_set_field_replaces_a_wrong_typed_key_in_place() {
    let scratch = Scratch::new("a_set_field_replaces_a_wrong_typed_key_in_place");
    let root = scratch.root();
    write(
        &root.autoswitch_state(),
        r#"{"futureKey": 1, "lastSwitchAt": "soon", "leftTrigger": "proactive", "otherKey": 2}"#,
    );

    AutoswitchState::update(&root, |state| state.last_switch_at = Some(1_700_000_000.5))
        .expect("the update writes");

    let text = fs::read_to_string(root.autoswitch_state()).expect("the state file reads");
    assert_eq!(
        text.matches("\"lastSwitchAt\"").count(),
        1,
        "the file has one lastSwitchAt key: {text}"
    );
    let on_disk = read_json(&root.autoswitch_state());
    assert_eq!(
        on_disk["lastSwitchAt"],
        json!(1_700_000_000.5),
        "the set field replaced the string"
    );
    let keys: Vec<&str> = on_disk
        .as_object()
        .expect("the file is an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "futureKey",
            "lastSwitchAt",
            "otherKey",
            "schemaVersion",
            "leftTrigger"
        ],
        "the kept keys keep their order, the replaced one its place"
    );
}

#[test]
fn null_keys_read_as_none_and_are_written_back_as_null() {
    let scratch = Scratch::new("null_keys_read_as_none_and_are_written_back_as_null");
    let root = scratch.root();
    write(
        &root.autoswitch_state(),
        r#"{"lastSwitchTo": null, "lastSwitchFrom": null, "leftTrigger": null}"#,
    );

    let state = AutoswitchState::read(&root).expect("the state file reads");
    assert_eq!(state.last_switch_to, None);
    assert_eq!(state.last_switch_from, None);
    assert_eq!(state.left_trigger, None);

    AutoswitchState::update(&root, |_| {}).expect("the update writes");
    let on_disk = read_json(&root.autoswitch_state());
    for key in ["lastSwitchTo", "lastSwitchFrom", "leftTrigger"] {
        assert_eq!(
            on_disk.get(key),
            Some(&json!(null)),
            "{key} is still present, and null"
        );
    }
}

#[test]
fn a_malformed_quarantine_entry_survives_an_update() {
    let scratch = Scratch::new("a_malformed_quarantine_entry_survives_an_update");
    let root = scratch.root();
    let mut document: serde_json::Value =
        serde_json::from_str(WRITTEN_STATE).expect("the fixture is JSON");
    document["quarantine"]["4"] = json!({"reason": "invalid_grant", "at": 17});
    document["quarantine"]["5"] = json!("not an entry");
    write(&root.autoswitch_state(), &document.to_string());

    let state = AutoswitchState::read(&root).expect("the state file reads");
    assert_eq!(
        state.quarantine.len(),
        3,
        "every quarantined account is still quarantined"
    );
    assert!(matches!(
        state.quarantine.get("3"),
        Some(QuarantineRecord::Entry(_))
    ));
    assert!(matches!(
        state.quarantine.get("4"),
        Some(QuarantineRecord::Malformed(_))
    ));
    assert!(matches!(
        state.quarantine.get("5"),
        Some(QuarantineRecord::Malformed(_))
    ));

    AutoswitchState::update(&root, |state| state.last_switch_at = Some(1_790_000_000.5))
        .expect("the update writes");

    let on_disk = read_json(&root.autoswitch_state());
    assert_eq!(
        on_disk["quarantine"]["4"],
        json!({"reason": "invalid_grant", "at": 17}),
        "a malformed entry round-trips"
    );
    assert_eq!(
        on_disk["quarantine"]["5"],
        json!("not an entry"),
        "a non-object entry round-trips"
    );
    assert_eq!(
        on_disk["quarantine"]["3"], document["quarantine"]["3"],
        "the well-formed entry round-trips"
    );
    assert_eq!(
        on_disk["lastSwitchFrom"],
        json!(1),
        "the rest of the file is kept"
    );
}

#[test]
fn an_unreadable_state_file_is_an_error_and_never_overwritten() {
    let scratch = Scratch::new("an_unreadable_state_file_is_an_error_and_never_overwritten");
    let root = scratch.root();
    fs::create_dir(root.autoswitch_state()).expect("a folder stands where the state file goes");

    let read = AutoswitchState::read(&root);
    assert!(
        matches!(read, Err(EngineError::Read(_))),
        "an unreadable state file is an error: {read:?}"
    );
    let update = AutoswitchState::update(&root, |state| {
        state.left_trigger = Some("failover".to_owned());
    });
    assert!(
        matches!(update, Err(EngineError::Read(_))),
        "an update of an unreadable file fails: {update:?}"
    );
    assert!(
        root.autoswitch_state().is_dir(),
        "nothing was written in its place"
    );
    let leftovers: Vec<_> = fs::read_dir(root.root())
        .expect("the account store lists")
        .collect();
    assert_eq!(
        leftovers.len(),
        2,
        "only the folder and the state lock are in the account store: {leftovers:?}"
    );
}

#[test]
fn state_update_holds_the_state_lock() {
    let scratch = Scratch::new("state_update_holds_the_state_lock");
    let root = scratch.root();
    write(&root.autoswitch_state(), WRITTEN_STATE);
    let held = FileLock::acquire(&root.autoswitch_state_lock(), Duration::from_secs(1))
        .expect("the state lock is free");

    let result = AutoswitchState::update_with(&root, Duration::from_millis(300), |state| {
        state.last_switch_at = None;
    });

    assert!(
        matches!(result, Err(EngineError::Lock(LockError::Timeout))),
        "a held state lock times out: {result:?}"
    );
    assert_eq!(
        fs::read_to_string(root.autoswitch_state()).expect("the state file reads"),
        WRITTEN_STATE,
        "nothing was written"
    );
    drop(held);
    AutoswitchState::update_with(&root, Duration::from_millis(300), |state| {
        state.last_switch_at = None;
    })
    .expect("a released lock is taken");
}

/// `FILE_SHARE_READ` and `FILE_SHARE_WRITE`: a handle that lets others read and write the file, but not replace it.
#[cfg(windows)]
const FILE_SHARE_READ: u32 = 0x1;
#[cfg(windows)]
const FILE_SHARE_WRITE: u32 = 0x2;

#[cfg(windows)]
/// Opens `path` for reading, sharing reads and writes but not deletes, so a read of the target succeeds and a rename
/// over it fails. Dropping the handle releases it.
fn hold_open_against_replace(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)
        .expect("the file opens without delete sharing")
}

/// The names of `folder`'s files that end in `.tmp`.
#[cfg(windows)]
fn temp_files(folder: &Path) -> Vec<String> {
    fs::read_dir(folder)
        .expect("the folder lists")
        .map(|entry| {
            entry
                .expect("an entry is readable")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| {
            Path::new(name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("tmp"))
        })
        .collect()
}

#[cfg(windows)]
#[test]
fn a_failed_state_write_leaves_no_temp_file() {
    let scratch = Scratch::new("a_failed_state_write_leaves_no_temp_file");
    let root = scratch.root();
    write(&root.autoswitch_state(), WRITTEN_STATE);
    let before = fs::read(root.autoswitch_state()).expect("the state file reads");

    let held = hold_open_against_replace(&root.autoswitch_state());
    let updated =
        AutoswitchState::update(&root, |state| state.last_switch_at = Some(1_700_000_000.5));
    assert!(
        matches!(updated, Err(EngineError::Write(_))),
        "a state file that cannot be replaced is a write error: {updated:?}"
    );
    drop(held);

    assert_eq!(
        fs::read(root.autoswitch_state()).expect("the state file reads"),
        before,
        "the state file is untouched"
    );
    assert_eq!(
        temp_files(root.root()),
        Vec::<String>::new(),
        "no temp file is left behind"
    );
}

/// A state file of a newer version is read and never written: `update` and `update_if` refuse it before the change
/// reaches the disk, so a newer engine's `paidFallback`, its `ownerExit` bar included, survives an older engine.
#[test]
fn a_newer_state_file_is_never_rewritten() {
    let scratch = Scratch::new("a_newer_state_file_is_never_rewritten");
    let root = scratch.root();
    let body = json!({
        "schemaVersion": 2,
        "lastSwitchAt": 1_789_999_000.25,
        "paidFallback": {
            "number": 2,
            "from": 1,
            "since": 1_789_999_000.25,
            "ownerExit": {"at": 1_789_999_500.0, "optedIn": [2]},
            "leaseOwner": {"owner": "rustling-tulip", "pid": 101, "startedAt": 1_789_998_000.5},
        },
    })
    .to_string();
    write(&root.autoswitch_state(), &body);

    let updated =
        AutoswitchState::update(&root, |state| state.last_switch_at = Some(1_700_000_000.5));
    let updated_if = AutoswitchState::update_if(&root, Duration::from_secs(5), |state| {
        state.paid_fallback = None;
        ((), true)
    });

    assert!(
        matches!(updated, Err(EngineError::UnknownVersion)),
        "update refuses a newer file: {updated:?}"
    );
    assert!(
        matches!(updated_if, Err(EngineError::UnknownVersion)),
        "update_if refuses a newer file: {updated_if:?}"
    );
    assert_eq!(
        fs::read_to_string(root.autoswitch_state()).expect("the state file reads"),
        body,
        "the newer file keeps its bytes"
    );
    assert_eq!(
        read_json(&root.autoswitch_state())["paidFallback"]["ownerExit"]["optedIn"],
        json!([2]),
        "the newer engine's ownerExit bar is still there"
    );
}

/// In a store a newer version wrote (`accounts.json` one `schemaVersion` past this crate's), a state write is refused
/// before anything changes: no state file is made, and an existing one keeps its bytes.
#[test]
fn a_state_write_into_a_newer_store_changes_nothing() {
    let scratch = Scratch::new("a_state_write_into_a_newer_store_changes_nothing");
    let root = scratch.root();
    write(
        &root.accounts_json(),
        &json!({"schemaVersion": 2, "order": [], "accounts": {}}).to_string(),
    );

    let fresh =
        AutoswitchState::update(&root, |state| state.last_switch_at = Some(1_700_000_000.5));
    assert!(
        matches!(
            fresh,
            Err(EngineError::StoreRefused(StoreErrorKind::UnknownSchema))
        ),
        "a newer store refuses the first state write: {fresh:?}"
    );
    assert!(!root.autoswitch_state().exists(), "no state file is made");

    write(&root.autoswitch_state(), WRITTEN_STATE);
    let existing =
        AutoswitchState::update(&root, |state| state.last_switch_at = Some(1_700_000_000.5));
    assert!(
        matches!(
            existing,
            Err(EngineError::StoreRefused(StoreErrorKind::UnknownSchema))
        ),
        "a newer store refuses a state write over an existing file: {existing:?}"
    );
    assert_eq!(
        fs::read_to_string(root.autoswitch_state()).expect("the state file reads"),
        WRITTEN_STATE,
        "the existing state file keeps its bytes"
    );
}

/// `paidFallback` reads as its typed field (no longer in `extra`), is written back with its bar and lease owner, and
/// every other key round-trips beside it, the unknown keys inside `paidFallback` and `ownerExit` included; clearing it
/// removes the key.
#[test]
fn paid_fallback_round_trips_and_keeps_other_keys() {
    let scratch = Scratch::new("paid_fallback_round_trips_and_keeps_other_keys");
    let root = scratch.root();
    let mut document: serde_json::Value =
        serde_json::from_str(WRITTEN_STATE).expect("the fixture is JSON");
    document["futureKey"] = json!("kept");
    document["paidFallback"] = json!({
        "number": 2,
        "from": 1,
        "since": 1_789_999_000.25,
        "ownerExit": {"at": 1_789_999_500.0, "optedIn": [2], "futureExitKey": 7},
        "leaseOwner": {"owner": "rustling-tulip", "pid": 101, "startedAt": 1_789_998_000.5},
        "futurePaidKey": "kept",
    });
    write(&root.autoswitch_state(), &document.to_string());

    let state = AutoswitchState::read(&root).expect("the state reads");
    let paid = state
        .paid_fallback
        .clone()
        .expect("the paid fallback is read");
    assert_eq!(
        (paid.number, paid.from, paid.since),
        (2, 1, 1_789_999_000.25)
    );
    assert_eq!(
        paid.lease_owner,
        LeaseOwner::new("rustling-tulip", 101, 1_789_998_000.5)
    );
    let bar = paid.owner_exit.clone().expect("the bar is read");
    assert_eq!(
        (bar.at, bar.opted_in.as_deref()),
        (1_789_999_500.0, Some([2].as_slice()))
    );
    assert!(
        !state.extra.contains_key("paidFallback"),
        "the key is typed, not kept in extra"
    );

    AutoswitchState::update(&root, |_| {}).expect("the update writes");
    let on_disk = read_json(&root.autoswitch_state());
    assert_eq!(
        on_disk["paidFallback"], document["paidFallback"],
        "the inner unknown keys round-trip"
    );

    AutoswitchState::update(&root, |state| {
        let paid = state
            .paid_fallback
            .as_mut()
            .expect("the paid fallback is read");
        paid.owner_exit = Some(OwnerExit::new(1_790_000_000.0, vec![3, 2, 3]));
    })
    .expect("the update writes");
    let on_disk = read_json(&root.autoswitch_state());
    assert_eq!(
        on_disk["paidFallback"],
        json!({
            "number": 2,
            "from": 1,
            "since": 1_789_999_000.25,
            "ownerExit": {"at": 1_790_000_000.0, "optedIn": [2, 3]},
            "leaseOwner": {"owner": "rustling-tulip", "pid": 101, "startedAt": 1_789_998_000.5},
            "futurePaidKey": "kept",
        })
    );
    assert_eq!(on_disk["futureKey"], json!("kept"));
    assert_eq!(on_disk["lastSwitchTo"], json!("2"));
    assert_eq!(on_disk["quarantine"]["3"]["reason"], json!("invalid_grant"));
    let reread = AutoswitchState::read(&root).expect("the state reads");
    assert_eq!(
        reread.paid_fallback.and_then(|paid| paid.owner_exit),
        Some(OwnerExit::new(1_790_000_000.0, vec![2, 3]))
    );

    AutoswitchState::update(&root, |state| state.paid_fallback = None).expect("the update writes");
    let on_disk = read_json(&root.autoswitch_state());
    assert!(
        on_disk.get("paidFallback").is_none(),
        "a cleared paid fallback removes the key: {on_disk}"
    );
    assert_eq!(on_disk["futureKey"], json!("kept"));
}

/// A paid fallback names the engine that entered it by owner, pid and the start of its lease: an engine in a process
/// that reused the pid claims a lease that starts later, so the paid fallback is not its own.
#[test]
fn a_paid_fallback_from_a_recycled_pid_is_not_this_engines() {
    let scratch = Scratch::new("a_paid_fallback_from_a_recycled_pid_is_not_this_engines");
    let root = scratch.root();
    let me = LeaseHolder::new("rustling-tulip", 101, 60.0);
    let lease = Lease::new(&root);
    assert_eq!(
        lease.started_at(&me).expect("the lease reads"),
        None,
        "no lease is no start"
    );
    let first = jiff::Timestamp::from_second(1_790_000_000).expect("the time is in range");
    lease
        .claim_or_heartbeat(&me, first, &EmptyTable)
        .expect("the lease is claimed");
    let started = lease
        .started_at(&me)
        .expect("the lease reads")
        .expect("the lease is ours");
    let entered = PaidFallback::new(
        2,
        1,
        1_790_000_010.0,
        LeaseOwner::new("rustling-tulip", 101, started),
    );
    assert!(
        entered.lease_owner.is(&me, started),
        "the engine that entered it"
    );
    assert!(
        !entered
            .lease_owner
            .is(&LeaseHolder::new("rustling-tulip", 102, 60.0), started),
        "another process is another engine"
    );

    lease.release(&me).expect("the lease is released");
    let later = jiff::Timestamp::from_second(1_790_000_600).expect("the time is in range");
    lease
        .claim_or_heartbeat(&me, later, &EmptyTable)
        .expect("the lease is claimed again");
    let restarted = lease
        .started_at(&me)
        .expect("the lease reads")
        .expect("the lease is ours");

    assert!(restarted > started, "the new lease starts later");
    assert!(
        !entered.lease_owner.is(&me, restarted),
        "the same owner and pid on a later lease is a recycled pid, not this engine"
    );
    assert_eq!(
        lease
            .started_at(&LeaseHolder::new("rustling-tulip", 102, 60.0))
            .expect("the lease reads"),
        None,
        "another engine's lease has no start for this one"
    );
}

/// A `paidFallback` of another shape reads as none, and the next write drops it: no paid state from a bad file.
#[test]
fn a_malformed_paid_fallback_reads_as_none() {
    let scratch = Scratch::new("a_malformed_paid_fallback_reads_as_none");
    let root = scratch.root();
    let malformed = [
        json!("2"),
        json!(null),
        json!({"number": 2, "from": 1, "since": 1.0}),
        json!({"number": 2, "from": 1, "since": 1.0, "leaseOwner": {"owner": "rustling-tulip", "pid": 101}}),
        json!({"number": "2", "from": 1, "since": 1.0, "leaseOwner": {"owner": "rustling-tulip", "pid": 101, "startedAt": 1.0}}),
        json!({"number": 2, "from": 1, "since": 1.0, "ownerExit": null, "leaseOwner": "x"}),
    ];
    for value in malformed {
        write(
            &root.autoswitch_state(),
            &json!({"lastSwitchTo": "2", "paidFallback": value}).to_string(),
        );

        let state = AutoswitchState::read(&root).expect("the state reads");
        assert_eq!(state.paid_fallback, None, "{value}");
        assert!(!state.extra.contains_key("paidFallback"), "{value}");

        AutoswitchState::update(&root, |_| {}).expect("the update writes");
        let on_disk = read_json(&root.autoswitch_state());
        assert!(
            on_disk.get("paidFallback").is_none(),
            "the bad key is dropped at the next write: {value}"
        );
        assert_eq!(on_disk["lastSwitchTo"], json!("2"));
    }
}

/// A `paidFallback` whose `ownerExit` is malformed still reads as a bar, so a bad file never lets the engine enter
/// against the owner's move, and the next write puts the bar back as an object with no opted-in set.
#[test]
fn a_malformed_owner_exit_reads_as_a_bar() {
    let scratch = Scratch::new("a_malformed_owner_exit_reads_as_a_bar");
    let root = scratch.root();
    let barred = json!({
        "number": 2,
        "from": 1,
        "since": 1.0,
        "ownerExit": 5,
        "leaseOwner": {"owner": "rustling-tulip", "pid": 101, "startedAt": 1.0},
    });
    write(
        &root.autoswitch_state(),
        &json!({"lastSwitchTo": "2", "paidFallback": barred}).to_string(),
    );
    let state = AutoswitchState::read(&root).expect("the state reads");
    let bar = state
        .paid_fallback
        .and_then(|paid| paid.owner_exit)
        .expect("a malformed ownerExit is still a bar");
    assert_eq!(
        bar.opted_in, None,
        "the bar's opted-in set could not be read"
    );
    AutoswitchState::update(&root, |_| {}).expect("the update writes");
    let on_disk = read_json(&root.autoswitch_state());
    assert_eq!(
        on_disk["paidFallback"]["ownerExit"],
        json!({"at": 0.0}),
        "the bar is written back as an object with no opted-in set"
    );
    assert_eq!(on_disk["lastSwitchTo"], json!("2"));
}

/// An `ownerExit` without its `optedIn` set, in a `paidFallback` whose `leaseOwner` is malformed too, is still a bar.
#[test]
fn an_owner_exit_missing_its_opted_in_set_is_still_a_bar() {
    let scratch = Scratch::new("an_owner_exit_missing_its_opted_in_set_is_still_a_bar");
    let root = scratch.root();
    let paid =
        json!({"number": 2, "from": 1, "since": 1.0, "leaseOwner": "x", "ownerExit": {"at": 5.0}});
    write(
        &root.autoswitch_state(),
        &json!({"paidFallback": paid}).to_string(),
    );

    let state = AutoswitchState::read(&root).expect("the state reads");
    let paid = state
        .paid_fallback
        .expect("the paid fallback carries the bar");
    assert_eq!((paid.number, paid.from, paid.since), (2, 1, 1.0));
    let bar = paid.owner_exit.expect("the bar is read");
    assert_eq!((bar.at, bar.opted_in), (5.0, None));
}

/// A salvaged bar keeps the unknown keys of both the `paidFallback` and its `ownerExit`, and writes them back.
#[test]
fn salvage_keeps_unknown_keys_at_both_levels() {
    let scratch = Scratch::new("salvage_keeps_unknown_keys_at_both_levels");
    let root = scratch.root();
    let paid = json!({
        "number": 2, "from": 1, "since": 1.0, "note": "outer",
        "ownerExit": {"at": "x", "optedIn": [1], "why": "inner"},
        "leaseOwner": {"owner": "rustling-tulip", "pid": 101, "startedAt": 1.0},
    });
    write(
        &root.autoswitch_state(),
        &json!({"paidFallback": paid}).to_string(),
    );

    let state = AutoswitchState::read(&root).expect("the state reads");
    let paid = state.paid_fallback.expect("the malformed bar is salvaged");
    assert_eq!(paid.extra.get("note"), Some(&json!("outer")));
    let bar = paid.owner_exit.expect("the bar is read");
    assert_eq!((bar.at, bar.opted_in), (0.0, Some(vec![1])));
    assert_eq!(bar.extra.get("why"), Some(&json!("inner")));

    AutoswitchState::update(&root, |_| {}).expect("the update writes");
    let on_disk = read_json(&root.autoswitch_state());
    assert_eq!(
        on_disk["paidFallback"]["note"],
        json!("outer"),
        "the outer unknown key is written back"
    );
    assert_eq!(
        on_disk["paidFallback"]["ownerExit"],
        json!({"at": 0.0, "optedIn": [1], "why": "inner"}),
        "the inner unknown key is written back"
    );
}

/// A salvaged bar whose `leaseOwner` is malformed writes it back in normal form, naming no engine.
#[test]
fn a_salvaged_lease_owner_is_written_back_in_normal_form() {
    let scratch = Scratch::new("a_salvaged_lease_owner_is_written_back_in_normal_form");
    let root = scratch.root();
    let paid =
        json!({"number": 2, "from": 1, "since": 1.0, "leaseOwner": "x", "ownerExit": {"at": 5.0}});
    write(
        &root.autoswitch_state(),
        &json!({"paidFallback": paid}).to_string(),
    );

    AutoswitchState::update(&root, |_| {}).expect("the update writes");
    let on_disk = read_json(&root.autoswitch_state());
    assert_eq!(
        on_disk["paidFallback"]["leaseOwner"],
        json!({"owner": "", "pid": 0, "startedAt": 0.0})
    );
}

#[test]
fn paid_fallback_events_have_their_kinds_and_fields() {
    let at = jiff::Timestamp::from_second(1_790_000_000).expect("the time is in range");
    let entered = Event {
        at,
        body: EventBody::PaidFallbackEntered {
            number: 2,
            from: 1,
            amount: 1020.03,
            currency: "USD".to_owned(),
        },
    };
    let left = Event {
        at,
        body: EventBody::PaidFallbackLeft {
            number: 2,
            reason: "refill",
            to: None,
            headroom_pct: None,
        },
    };

    assert_eq!(entered.kind(), "paid-fallback-entered");
    assert_eq!(left.kind(), "paid-fallback-left");
    assert_eq!(
        entered.to_json(),
        json!({
            "schemaVersion": 1,
            "event": "paid-fallback-entered",
            "ts": "2026-09-21T14:13:20Z",
            "number": 2,
            "from": 1,
            "amount": 1020.03,
            "currency": "USD",
        })
    );
    assert_eq!(
        left.to_json(),
        json!({"schemaVersion": 1, "event": "paid-fallback-left", "ts": "2026-09-21T14:13:20Z", "number": 2, "reason": "refill"})
    );
}
