//! Disabling and enabling an account, and making one active: the `disabled` flag and `active` in `accounts.json`,
//! under the account lock.

use claude_accounts::StoreErrorKind;

use crate::support::Scratch;

#[test]
fn disable_writes_the_flag() {
    let scratch = Scratch::new("disable_writes_the_flag");
    let store = scratch.store();
    let guard = scratch.lock();

    assert!(
        store
            .set_disabled(&guard, 1, true)
            .expect("account 1 can be disabled")
    );

    let written = scratch.read_json("accounts.json");
    assert_eq!(
        written["accounts"]["1"]["disabled"],
        serde_json::Value::Bool(true)
    );
    let keys: Vec<&str> = written["accounts"]["1"]
        .as_object()
        .expect("account 1 is an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "email",
            "uuid",
            "orgUuid",
            "orgName",
            "added",
            "alias",
            "futureField",
            "generation",
            "disabled"
        ]
    );
    assert_eq!(written["futureTopLevel"], "kept");

    let sequence = store
        .read_sequence()
        .expect("the roster reads")
        .expect("the roster exists");
    assert!(sequence.account(1).expect("account 1 is kept").disabled);
}

#[test]
fn enable_removes_flag() {
    let scratch = Scratch::new("enable_removes_flag");
    let store = scratch.store();
    let guard = scratch.lock();

    assert!(
        store
            .set_disabled(&guard, 2, false)
            .expect("account 2 can be enabled")
    );

    let written = scratch.read_json("accounts.json");
    let keys: Vec<&str> = written["accounts"]["2"]
        .as_object()
        .expect("account 2 is an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["email", "uuid", "added", "kind"],
        "the flag is removed and the other keys keep their order"
    );
    let sequence = store
        .read_sequence()
        .expect("the roster reads")
        .expect("the roster exists");
    assert!(!sequence.account(2).expect("account 2 is kept").disabled);
}

#[test]
fn disable_is_noop_when_already_set() {
    let scratch = Scratch::new("disable_is_noop_when_already_set");
    let store = scratch.store();
    let guard = scratch.lock();
    let before = scratch.read("accounts.json");

    assert!(
        !store
            .set_disabled(&guard, 2, true)
            .expect("an already disabled account is fine")
    );
    assert!(
        !store
            .set_disabled(&guard, 1, false)
            .expect("an already enabled account is fine")
    );
    assert_eq!(
        scratch.read("accounts.json"),
        before,
        "the file is untouched, byte for byte"
    );
}

#[test]
fn disable_unknown_account_errors() {
    let scratch = Scratch::new("disable_unknown_account_errors");
    let store = scratch.store();
    let guard = scratch.lock();
    let before = scratch.read("accounts.json");

    let error = store
        .set_disabled(&guard, 9, true)
        .expect_err("account 9 does not exist");
    assert_eq!(error.kind(), StoreErrorKind::UnknownAccount);
    assert_eq!(error.account(), Some(9));
    assert_eq!(scratch.read("accounts.json"), before);

    std::fs::remove_file(scratch.path("accounts.json")).expect("the roster can be removed");
    let error = store
        .set_disabled(&guard, 1, true)
        .expect_err("no roster, no account");
    assert_eq!(error.kind(), StoreErrorKind::AccountsMissing);
    assert!(
        !scratch.exists("accounts.json"),
        "a missing roster is not created"
    );
}

#[test]
fn disable_refused_while_sealing_fails() {
    let scratch = Scratch::new("disable_refused_while_sealing_fails");
    let store = scratch.store_without_sealing();
    let guard = scratch.lock();
    let before = scratch.read("accounts.json");

    let error = store
        .set_disabled(&guard, 1, true)
        .expect_err("a failed canary refuses the write");
    assert_eq!(error.kind(), StoreErrorKind::StoreReadOnly);
    assert_eq!(scratch.read("accounts.json"), before);
}

#[test]
fn set_active_writes_the_active_account_under_the_account_lock() {
    let scratch = Scratch::new("set_active_writes_the_active_account");
    let other = Scratch::new("set_active_other_root");
    let store = scratch.store();
    let before = scratch.read("accounts.json");

    let error = store
        .set_active(&other.lock(), 2)
        .expect_err("another root's lock is refused");
    assert_eq!(error.kind(), StoreErrorKind::WrongLock);
    assert_eq!(
        scratch.read("accounts.json"),
        before,
        "a refused write leaves the file untouched"
    );

    let guard = scratch.lock();
    assert!(
        store
            .set_active(&guard, 2)
            .expect("account 2 can be made active")
    );
    let written = scratch.read_json("accounts.json");
    assert_eq!(written["active"], 2);
    let keys: Vec<&str> = written
        .as_object()
        .expect("the roster is an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "schemaVersion",
            "nextNumber",
            "order",
            "active",
            "accounts",
            "futureTopLevel"
        ],
        "every key keeps its place"
    );
    assert_eq!(
        written["accounts"],
        serde_json::from_slice::<serde_json::Value>(&before).expect("the fixture is JSON")["accounts"]
    );
    let sequence = store
        .read_sequence()
        .expect("the roster reads")
        .expect("the roster exists");
    assert_eq!(sequence.active(), Some(2));

    let now = scratch.read("accounts.json");
    assert!(
        !store
            .set_active(&guard, 2)
            .expect("the active account again is fine")
    );
    assert_eq!(
        scratch.read("accounts.json"),
        now,
        "an already active account leaves the file untouched"
    );
    let error = store
        .set_active(&guard, 9)
        .expect_err("account 9 does not exist");
    assert_eq!(error.kind(), StoreErrorKind::UnknownAccount);
    assert_eq!(error.account(), Some(9));
    assert_eq!(scratch.read("accounts.json"), now);
}
