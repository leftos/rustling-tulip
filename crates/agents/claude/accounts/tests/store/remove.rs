//! Removing an account: its record and `order` entry leave `accounts.json`, `nextNumber` stays, and its files go,
//! under its consume lock and the account lock.

use claude_accounts::{ClaudePaths, ConsumeLock, StoreErrorKind};

use crate::support::Scratch;

/// Claude Code's files under the scratch folder, with no `~/.claude.json`: no live login.
fn no_login(scratch: &Scratch) -> ClaudePaths {
    ClaudePaths::new(scratch.dir.join("claude"), scratch.dir.join("claude.json"))
}

/// Claude Code's files under the scratch folder, `~/.claude.json` naming `email` in `org` as the live login.
fn live_login(scratch: &Scratch, email: &str, org: Option<&str>) -> ClaudePaths {
    let mut account = serde_json::json!({ "emailAddress": email });
    if let Some(org) = org {
        account["organizationUuid"] = serde_json::Value::from(org);
    }
    let paths = no_login(scratch);
    std::fs::write(
        paths.global_config(),
        serde_json::json!({ "oauthAccount": account, "other": 1 }).to_string(),
    )
    .expect("the global config can be written");
    paths
}

/// A stash entry id of the stash's form, numbered `n`.
fn stash_id(n: u32) -> String {
    format!("20260901T10000{n}-0123456789ab-abcde{n}")
}

#[test]
fn remove_account_drops_the_accounts_stash_entries() {
    let scratch = Scratch::new("remove_account_drops_the_accounts_stash_entries");
    let (bob_one, bob_two, alice) = (stash_id(1), stash_id(2), stash_id(3));
    let row = |slot: &str| {
        serde_json::json!({
            "createdAt": "2026-09-01T10:00:00Z",
            "reason": "persist_failed",
            "configSlot": slot,
            "consumedFp": "fp",
            "fingerprint": "fp2",
            "organization": "",
        })
    };
    let manifest = serde_json::json!({
        "schemaVersion": 1,
        "entries": {
            bob_one.clone(): row("2"),
            bob_two.clone(): row("2"),
            alice.clone(): row("1"),
        },
    });
    scratch.write(
        "credentials/.unclaimed-manifest.json",
        &manifest.to_string(),
    );
    for id in [&bob_one, &bob_two, &alice] {
        scratch.write(&format!("credentials/.unclaimed-{id}.cred"), "sealed entry");
    }
    let store = scratch.store();

    assert!(
        store
            .remove_account(&no_login(&scratch), 2)
            .expect("account 2 can be removed")
    );

    for id in [&bob_one, &bob_two] {
        assert!(
            !scratch.exists(&format!("credentials/.unclaimed-{id}.cred")),
            "account 2's stash entry {id} is removed"
        );
    }
    assert!(
        scratch.exists(&format!("credentials/.unclaimed-{alice}.cred")),
        "account 1's stash entry stays"
    );
    let written = scratch.read_json("credentials/.unclaimed-manifest.json");
    let ids: Vec<&str> = written["entries"]
        .as_object()
        .expect("the manifest's entries are an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(ids, [alice.as_str()], "only account 1's row stays");
    assert_eq!(written["entries"][alice.as_str()], row("1"));
}

#[test]
fn remove_account_refuses_the_live_login() {
    let scratch = Scratch::new("remove_account_refuses_the_live_login");
    give_bob_files(&scratch);
    let store = scratch.store();
    let before = scratch.read("accounts.json");

    let claude = live_login(&scratch, "bob@example.com", None);
    let error = store
        .remove_account(&claude, 2)
        .expect_err("Claude Code's live login is not removed");
    assert_eq!(error.kind(), StoreErrorKind::ActiveAccount);
    assert_eq!(error.account(), Some(2));
    assert_eq!(
        scratch.read("accounts.json"),
        before,
        "the file is untouched"
    );
    assert!(scratch.exists("credentials/2.cred"));
    assert!(scratch.exists("configs/2.json"));

    let claude = live_login(
        &scratch,
        "bob@example.com",
        Some("00000000-0000-4000-8000-0000000000ff"),
    );
    assert!(
        store
            .remove_account(&claude, 2)
            .expect("the same email in another organization is another login")
    );
}

/// Gives account 2 a config, a credential file, a kept previous one and a consume lock file.
fn give_bob_files(scratch: &Scratch) {
    scratch.write("configs/2.json", r#"{"emailAddress":"bob@example.com"}"#);
    scratch.write("credentials/2.cred", "sealed bytes");
    scratch.write("credentials/2.cred.prev", "older sealed bytes");
    drop(ConsumeLock::acquire(&scratch.root(), 2).expect("account 2's consume lock is free"));
    assert!(scratch.exists("credentials/.consume-2.lock"));
}

#[test]
fn remove_account_drops_record_order_and_files() {
    let scratch = Scratch::new("remove_account_drops_record_order_and_files");
    give_bob_files(&scratch);
    let store = scratch.store();

    assert!(
        store
            .remove_account(&no_login(&scratch), 2)
            .expect("account 2 can be removed")
    );

    let written = scratch.read_json("accounts.json");
    assert_eq!(written["order"], serde_json::json!([1]));
    let numbers: Vec<&str> = written["accounts"]
        .as_object()
        .expect("accounts is an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(numbers, ["1"]);
    assert_eq!(written["active"], 1);
    assert_eq!(written["futureTopLevel"], "kept");
    for file in [
        "configs/2.json",
        "credentials/2.cred",
        "credentials/2.cred.prev",
    ] {
        assert!(!scratch.exists(file), "{file} is removed");
    }
    assert!(
        scratch.exists("credentials/.consume-2.lock"),
        "the consume lock file stays: a waiter's handle on it must keep naming the one lock"
    );
    assert!(scratch.exists("configs/1.json"), "account 1's config stays");
    assert!(
        scratch.exists("credentials/1.cred"),
        "account 1's credentials stay"
    );

    let sequence = store
        .read_sequence()
        .expect("the roster reads")
        .expect("the roster exists");
    assert!(sequence.account(2).is_none());
    assert_eq!(sequence.numbers(), [1]);
}

#[test]
fn remove_account_keeps_next_number() {
    let scratch = Scratch::new("remove_account_keeps_next_number");
    let store = scratch.store();

    assert!(
        store
            .remove_account(&no_login(&scratch), 2)
            .expect("account 2 can be removed")
    );

    let written = scratch.read_json("accounts.json");
    assert_eq!(
        written["nextNumber"], 3,
        "a removed number is never given again"
    );
    let sequence = store
        .read_sequence()
        .expect("the roster reads")
        .expect("the roster exists");
    assert_eq!(sequence.next_number(), 3);
}

#[test]
fn remove_account_refuses_the_active_account() {
    let scratch = Scratch::new("remove_account_refuses_the_active_account");
    let store = scratch.store();
    let before = scratch.read("accounts.json");

    let error = store
        .remove_account(&no_login(&scratch), 1)
        .expect_err("the active account is not removed");
    assert_eq!(error.kind(), StoreErrorKind::ActiveAccount);
    assert_eq!(error.account(), Some(1));
    assert_eq!(
        scratch.read("accounts.json"),
        before,
        "the file is untouched"
    );
    assert!(scratch.exists("credentials/1.cred"));
    assert!(scratch.exists("configs/1.json"));
}

#[test]
fn remove_account_of_unknown_number_is_refused() {
    let scratch = Scratch::new("remove_account_of_unknown_number_is_refused");
    let store = scratch.store();
    let before = scratch.read("accounts.json");

    let error = store
        .remove_account(&no_login(&scratch), 9)
        .expect_err("account 9 does not exist");
    assert_eq!(error.kind(), StoreErrorKind::UnknownAccount);
    assert_eq!(error.account(), Some(9));
    assert_eq!(
        scratch.read("accounts.json"),
        before,
        "the file is untouched"
    );
    assert!(
        !scratch.exists("credentials/.consume-9.lock"),
        "a refused removal leaves no lock file behind"
    );
}

#[test]
fn remove_account_tolerates_missing_files() {
    let scratch = Scratch::new("remove_account_tolerates_missing_files");
    let store = scratch.store();
    assert!(!scratch.exists("configs/2.json"));
    assert!(!scratch.exists("credentials/2.cred"));

    assert!(
        store
            .remove_account(&no_login(&scratch), 2)
            .expect("an account without files is removed")
    );

    let written = scratch.read_json("accounts.json");
    assert_eq!(written["order"], serde_json::json!([1]));
    assert!(written["accounts"].get("2").is_none());
}
