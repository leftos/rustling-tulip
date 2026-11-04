//! `accounts.json`: where the store lives, the accounts as read, unknown keys and order kept through a write, account
//! numbers never reused, a write that would re-identify an account refused, a file of an unknown schema read but never
//! written, and a torn file refused.

use claude_accounts::{AccountKind, AccountsRoot, CredentialBlob, Identity, StoreErrorKind};
use serde_json::{Value, json};

use crate::support::{ALICE, ALICE_CRED, BOB, Scratch};

const ALICE_CONFIG: &str = "configs/1.json";

/// The store lives at the root its caller supplies, and every store file is under it.
#[test]
fn the_store_lives_at_the_root_the_caller_supplies() {
    let supplied = std::env::temp_dir().join("the_store_lives_at_the_root_the_caller_supplies");

    let root = AccountsRoot::new(supplied.clone());

    assert_eq!(root.root(), supplied.as_path());
    assert_eq!(root.accounts_json(), root.root().join("accounts.json"));
    assert_eq!(
        root.credential_path(7),
        root.root().join("credentials").join("7.cred")
    );
    assert_eq!(
        root.config_path(7),
        root.root().join("configs").join("7.json")
    );
}

/// A store file replaced by a link to another file is refused as unreadable rather than followed.
#[cfg(windows)]
#[test]
#[expect(
    clippy::print_stdout,
    reason = "a test that cannot make its symlink says why it was skipped"
)]
fn a_linked_store_file_is_refused_as_unreadable() {
    let scratch = Scratch::new("a_linked_store_file_is_refused_as_unreadable");
    let accounts = scratch.path("accounts.json");
    let elsewhere = scratch.dir.join("elsewhere-accounts.json");
    std::fs::rename(&accounts, &elsewhere).expect("accounts.json can be moved aside");
    let made = std::os::windows::fs::symlink_file(&elsewhere, &accounts);
    if made.as_ref().err().and_then(std::io::Error::raw_os_error) == Some(1314) {
        println!("skipped: creating a symlink needs Developer Mode or elevation");
        return;
    }
    made.expect("the symlink can be made");

    let error = scratch
        .store()
        .read_sequence()
        .expect_err("a linked accounts.json is refused");

    assert_eq!(error.kind(), StoreErrorKind::AccountsUnreadable);
}

/// The fixture reads as the same accounts 1 and 2 the old layout's fixture held, and a write gives back every key,
/// known or not, at the top level and in each account, in its original order.
#[test]
fn round_trip_keeps_unknown_keys_and_order() {
    let scratch = Scratch::new("round_trip_keeps_unknown_keys_and_order");
    let store = scratch.store();
    let sequence = store
        .read_sequence()
        .expect("a read works")
        .expect("the fixture has accounts");

    assert_eq!(sequence.active(), Some(1));
    assert_eq!(sequence.numbers(), [1, 2]);
    assert_eq!(sequence.next_number(), 3);
    let alice = sequence.account(1).expect("account 1 is in the fixture");
    assert_eq!(alice.number, 1);
    assert_eq!(alice.email.as_str(), ALICE);
    assert_eq!(alice.uuid, "00000000-0000-4000-8000-000000000001");
    assert_eq!(alice.org_uuid, "00000000-0000-4000-8000-0000000000a1");
    assert_eq!(alice.org_name, "Example Org");
    assert_eq!(alice.added, "2026-09-01T10:00:00Z");
    assert_eq!(alice.alias.as_deref(), Some("work"));
    assert!(!alice.disabled);
    assert_eq!(alice.kind, AccountKind::Oauth);
    let bob = sequence.account(2).expect("account 2 is in the fixture");
    assert_eq!(bob.email.as_str(), BOB);
    assert_eq!(bob.uuid, "00000000-0000-4000-8000-000000000002");
    assert_eq!(bob.org_uuid, "");
    assert_eq!(bob.org_name, "");
    assert_eq!(bob.added, "2026-09-02T11:00:00Z");
    assert_eq!(bob.alias, None);
    assert!(bob.disabled);
    assert_eq!(bob.kind, AccountKind::ApiKey);
    assert!(sequence.account(3).is_none());

    let guard = scratch.lock();
    store
        .write_sequence(&guard, &sequence)
        .expect("the accounts write");

    let original: Value =
        serde_json::from_slice(&scratch.read("accounts.json")).expect("the roster is JSON");
    let expected = serde_json::to_string_pretty(&original).expect("the roster serializes");
    let written =
        String::from_utf8(scratch.read("accounts.json")).expect("the written file is UTF-8");
    assert_eq!(
        written, expected,
        "every key, known or not, is written back in its original order"
    );
    assert!(written.contains("\"futureTopLevel\""));
    assert!(written.contains("\"futureField\""));
    let leftovers: Vec<_> = std::fs::read_dir(scratch.backup())
        .expect("the store lists")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "the temp file is gone after the replace"
    );
}

/// A file with neither `schemaVersion` nor `nextNumber` reads as version 1 with the next number past the highest
/// account, and the first write writes both.
#[test]
fn a_write_writes_the_schema_version_and_the_next_number() {
    let scratch = Scratch::new("a_write_writes_the_schema_version_and_the_next_number");
    scratch.edit_json("accounts.json", |accounts| {
        let document = accounts.as_object_mut().expect("the fixture is an object");
        document.shift_remove("schemaVersion");
        document.shift_remove("nextNumber");
    });
    let store = scratch.store();
    let sequence = store
        .read_sequence()
        .expect("a file with no version reads")
        .expect("the file exists");
    assert_eq!(sequence.next_number(), 3, "past the highest account number");

    let guard = scratch.lock();
    assert!(
        store
            .set_active(&guard, 2)
            .expect("account 2 can be made active")
    );

    let written = scratch.read_json("accounts.json");
    assert_eq!(written["schemaVersion"], json!(1));
    assert_eq!(written["nextNumber"], json!(3));
    assert_eq!(written["active"], json!(2));
}

/// An account removed from the file by hand never gives its number to a later account: `nextNumber` stays past it,
/// and a stored `nextNumber` lower than an account's number is raised past it on read and write.
#[test]
fn an_account_number_is_never_reused_after_its_account_is_removed() {
    let scratch = Scratch::new("an_account_number_is_never_reused_after_its_account_is_removed");
    scratch.edit_json("accounts.json", |accounts| {
        accounts["accounts"]
            .as_object_mut()
            .expect("accounts is an object")
            .shift_remove("2");
        accounts["order"] = json!([1]);
    });
    let store = scratch.store();
    let guard = scratch.lock();

    let sequence = store
        .read_sequence()
        .expect("the file reads")
        .expect("the file exists");
    assert_eq!(sequence.next_number(), 3, "account 2's number stays taken");
    assert!(
        store
            .set_disabled(&guard, 1, true)
            .expect("account 1 can be disabled")
    );
    assert_eq!(
        scratch.read_json("accounts.json")["nextNumber"],
        json!(3),
        "a write never lowers nextNumber"
    );

    scratch.edit_json("accounts.json", |accounts| {
        accounts["nextNumber"] = json!(1);
    });
    let sequence = store
        .read_sequence()
        .expect("the file reads")
        .expect("the file exists");
    assert_eq!(
        sequence.next_number(),
        2,
        "a stored number at or under an account's is raised past it"
    );
    assert!(
        store
            .set_disabled(&guard, 1, false)
            .expect("account 1 can be enabled")
    );
    assert_eq!(scratch.read_json("accounts.json")["nextNumber"], json!(2));
}

/// A file a later version wrote (a higher `schemaVersion`) is served to every read, and every write is refused with
/// every byte left as it was.
#[test]
fn an_unknown_schema_version_is_read_and_never_written() {
    let scratch = Scratch::new("an_unknown_schema_version_is_read_and_never_written");
    scratch.edit_json("accounts.json", |accounts| {
        accounts["schemaVersion"] = json!(2);
    });
    let store = scratch.store();
    let before = [
        scratch.read("accounts.json"),
        scratch.read(ALICE_CRED),
        scratch.read(ALICE_CONFIG),
    ];

    let sequence = store
        .read_sequence()
        .expect("a read works")
        .expect("the file exists");
    assert_eq!(sequence.numbers(), [1, 2]);
    assert_eq!(
        sequence
            .account(1)
            .expect("account 1 is read")
            .email
            .as_str(),
        ALICE
    );
    assert!(store.read_credentials(1).expect("a read works").is_some());
    assert!(
        store
            .read_account_config(1)
            .expect("a read works")
            .is_some()
    );

    let guard = scratch.lock();
    let blob = CredentialBlob::new("replacement".to_owned());
    let refusals = [
        ("set_active", store.set_active(&guard, 2).err()),
        ("set_disabled", store.set_disabled(&guard, 1, true).err()),
        (
            "write_sequence",
            store.write_sequence(&guard, &sequence).err(),
        ),
        (
            "write_credentials",
            store.write_credentials(&guard, 1, &blob).err(),
        ),
        (
            "write_credentials new",
            store.write_credentials(&guard, 3, &blob).err(),
        ),
        (
            "write_account_config",
            store.write_account_config(&guard, 1, &json!({})).err(),
        ),
        (
            "backfill_uuid",
            store
                .backfill_uuid(&guard, 2, "new-uuid", &Identity::new(BOB, ""))
                .err(),
        ),
    ];
    for (write, error) in refusals {
        assert_eq!(
            error.map(|error| error.kind()),
            Some(StoreErrorKind::UnknownSchema),
            "{write}"
        );
    }
    assert_eq!(
        store.write_refusal(),
        Some(StoreErrorKind::UnknownSchema),
        "the pre-spend gates refuse too"
    );

    let after = [
        scratch.read("accounts.json"),
        scratch.read(ALICE_CRED),
        scratch.read(ALICE_CONFIG),
    ];
    assert_eq!(after, before, "every byte is unchanged");
    assert!(!scratch.exists("credentials/1.cred.prev"));
    assert!(!scratch.exists("credentials/3.cred"));
}

/// A file a later version wrote in a shape version 1 cannot read is that version's, not a corrupt one: it reads as
/// [`StoreErrorKind::UnknownSchema`], never with the "repair or move it" of a corrupt file.
#[test]
fn a_later_versions_file_of_another_shape_is_unknown_not_corrupt() {
    let scratch = Scratch::new("a_later_versions_file_of_another_shape_is_unknown_not_corrupt");
    scratch.write(
        "accounts.json",
        r#"{"schemaVersion": 2, "order": "by-usage", "accounts": [{"email": "a@example.com"}]}"#,
    );
    let store = scratch.store();

    let error = store
        .read_sequence()
        .expect_err("a shape version 1 cannot read is no account list");

    assert_eq!(error.kind(), StoreErrorKind::UnknownSchema);
    assert_eq!(store.write_refusal(), Some(StoreErrorKind::UnknownSchema));
}

/// A `Sequence` read from a later version's file is never written, even once the file on disk reads as version 1.
#[test]
fn a_sequence_read_from_a_later_versions_file_is_never_written() {
    let scratch = Scratch::new("a_sequence_read_from_a_later_versions_file_is_never_written");
    scratch.edit_json("accounts.json", |accounts| {
        accounts["schemaVersion"] = json!(2);
    });
    let store = scratch.store();
    let sequence = store
        .read_sequence()
        .expect("a read works")
        .expect("the file exists");
    scratch.edit_json("accounts.json", |accounts| {
        accounts["schemaVersion"] = json!(1);
    });
    let before = scratch.read("accounts.json");
    let guard = scratch.lock();

    let error = store
        .write_sequence(&guard, &sequence)
        .expect_err("a later version's accounts are never written");

    assert_eq!(error.kind(), StoreErrorKind::UnknownSchema);
    assert_eq!(
        scratch.read("accounts.json"),
        before,
        "every byte is unchanged"
    );
}

/// An `accounts.json` that is there but cannot be read, or does not parse, refuses every write and every pre-spend
/// gate: its version cannot be known. A missing one refuses nothing.
#[test]
fn a_present_accounts_json_that_cannot_be_read_refuses_every_write() {
    let scratch = Scratch::new("a_present_accounts_json_that_cannot_be_read_refuses_every_write");
    let store = scratch.store();
    let guard = scratch.lock();
    let blob = CredentialBlob::new("replacement".to_owned());
    let credential = scratch.read(ALICE_CRED);
    let whole = scratch.read("accounts.json");
    let torn = String::from_utf8(whole[..whole.len() / 2].to_vec()).expect("the fixture is ASCII");

    scratch.write("accounts.json", &torn);
    assert_eq!(store.write_refusal(), Some(StoreErrorKind::AccountsCorrupt));
    let error = store
        .write_credentials(&guard, 1, &blob)
        .expect_err("a torn accounts.json refuses the write");
    assert_eq!(error.kind(), StoreErrorKind::AccountsCorrupt);
    let error = store
        .write_account_config(&guard, 1, &json!({}))
        .expect_err("a torn accounts.json refuses the write");
    assert_eq!(error.kind(), StoreErrorKind::AccountsCorrupt);

    std::fs::remove_file(scratch.path("accounts.json")).expect("accounts.json can be removed");
    std::fs::create_dir(scratch.path("accounts.json"))
        .expect("a folder can take accounts.json's place");
    assert_eq!(
        store.write_refusal(),
        Some(StoreErrorKind::AccountsUnreadable)
    );
    let error = store
        .write_credentials(&guard, 1, &blob)
        .expect_err("an unreadable accounts.json refuses the write");
    assert_eq!(error.kind(), StoreErrorKind::AccountsUnreadable);
    assert_eq!(
        scratch.read(ALICE_CRED),
        credential,
        "the credential file is unchanged"
    );
    assert!(!scratch.exists("credentials/1.cred.prev"));

    std::fs::remove_dir(scratch.path("accounts.json")).expect("the folder can be removed");
    assert_eq!(
        store.write_refusal(),
        None,
        "a missing accounts.json refuses nothing"
    );
    // A credential is sealed under the record's identity, and the missing roster names none: the write is refused
    // rather than sealing a file the record could never open.
    let error = store
        .write_credentials(&guard, 1, &blob)
        .expect_err("a number no record names is never sealed");
    assert_eq!(error.kind(), StoreErrorKind::UnknownAccount);
    assert_eq!(
        scratch.read(ALICE_CRED),
        credential,
        "the credential file is unchanged"
    );
}

/// An account numbered `u32::MAX` leaves no number for the next account, so the file is refused as corrupt.
#[test]
fn an_account_numbered_u32_max_is_corrupt() {
    let scratch = Scratch::new("an_account_numbered_u32_max_is_corrupt");
    scratch.write(
        "accounts.json",
        &json!({"order": [u32::MAX], "accounts": {u32::MAX.to_string(): {"email": ALICE}}})
            .to_string(),
    );

    let error = scratch
        .store()
        .read_sequence()
        .expect_err("no number is left for the next account");

    assert_eq!(error.kind(), StoreErrorKind::AccountsCorrupt);
}

/// A `Sequence` read before a credential write raised an account's generation never lowers it: the write keeps the
/// floor on disk, as it keeps `nextNumber`.
#[test]
fn a_stale_write_never_lowers_a_generation() {
    let scratch = Scratch::new("a_stale_write_never_lowers_a_generation");
    let store = scratch.store();
    let guard = scratch.lock();
    let stale = store
        .read_sequence()
        .expect("a read works")
        .expect("the file exists");
    store
        .write_credentials(&guard, 1, &CredentialBlob::new("replacement".to_owned()))
        .expect("the credential write lands");
    let raised = scratch.read_json("accounts.json")["accounts"]["1"]["generation"]
        .as_u64()
        .expect("the write raised the floor");
    assert!(raised > stale.account(1).expect("account 1 is there").generation);

    store
        .write_sequence(&guard, &stale)
        .expect("the stale write lands");

    assert_eq!(
        scratch.read_json("accounts.json")["accounts"]["1"]["generation"],
        json!(raised),
        "a stale sequence never lowers the generation floor"
    );
}

/// A `Sequence` read before another writer raised `nextNumber` never lowers it: the write takes the larger of its own
/// and the one on disk under the lock.
#[test]
fn a_write_never_lowers_the_next_number_on_disk() {
    let scratch = Scratch::new("a_write_never_lowers_the_next_number_on_disk");
    let store = scratch.store();
    let sequence = store
        .read_sequence()
        .expect("a read works")
        .expect("the file exists");
    assert_eq!(sequence.next_number(), 3);
    scratch.edit_json("accounts.json", |accounts| {
        accounts["nextNumber"] = json!(10);
    });
    let guard = scratch.lock();

    store
        .write_sequence(&guard, &sequence)
        .expect("the accounts write");

    assert_eq!(scratch.read_json("accounts.json")["nextNumber"], json!(10));
}

/// An API-key record is kept as written, but is no account to switch to, poll or show.
#[test]
fn an_api_key_record_is_found_but_not_switchable() {
    let scratch = Scratch::new("an_api_key_record_is_found_but_not_switchable");
    let sequence = scratch
        .store()
        .read_sequence()
        .expect("a read works")
        .expect("the fixture has accounts");

    let bob = sequence.account(2).expect("the API-key record is kept");
    assert_eq!(bob.kind, AccountKind::ApiKey);
    assert!(
        sequence.switchable_account(2).is_none(),
        "an API-key account is not switchable"
    );
    assert_eq!(
        sequence.switchable_account(1),
        sequence.account(1),
        "an OAuth account is"
    );
}

#[test]
fn torn_accounts_json_is_error_not_empty() {
    let scratch = Scratch::new("torn_accounts_json_is_error_not_empty");
    let whole = scratch.read("accounts.json");
    let torn = String::from_utf8(whole[..whole.len() / 2].to_vec()).expect("the fixture is ASCII");
    scratch.write("accounts.json", &torn);
    let store = scratch.store();

    let error = store.read_sequence().expect_err("a torn file is an error");
    assert_eq!(error.kind(), StoreErrorKind::AccountsCorrupt);

    for contents in [
        "[1, 2]",
        r#"{"accounts": {"x": {"email": "a@example.com"}}}"#,
        r#"{"accounts": {"1": {"uuid": "u"}}}"#,
        r#"{"schemaVersion": "1", "accounts": {}}"#,
        r#"{"nextNumber": -1, "accounts": {}}"#,
        r#"{"order": [1, "2"], "accounts": {}}"#,
    ] {
        scratch.write("accounts.json", contents);
        let error = store
            .read_sequence()
            .expect_err("a malformed file is an error");
        assert_eq!(error.kind(), StoreErrorKind::AccountsCorrupt, "{contents}");
    }

    scratch.write("accounts.json", &torn);
    let guard = scratch.lock();
    let error = store
        .set_disabled(&guard, 1, true)
        .expect_err("a torn file is never rewritten");
    assert_eq!(error.kind(), StoreErrorKind::AccountsCorrupt);
    assert_eq!(scratch.read("accounts.json"), torn.as_bytes());
}

#[test]
fn missing_accounts_json_is_none() {
    let scratch = Scratch::new("missing_accounts_json_is_none");
    std::fs::remove_file(scratch.path("accounts.json")).expect("accounts.json can be removed");
    assert!(
        scratch
            .store()
            .read_sequence()
            .expect("a missing file is not an error")
            .is_none()
    );

    let empty = Scratch::empty("missing_accounts_json_is_none_root");
    let store = empty.store();
    assert!(
        store
            .read_sequence()
            .expect("a missing root is not an error")
            .is_none()
    );
    assert!(
        store
            .read_credentials(1)
            .expect("a missing root is not an error")
            .is_none()
    );
    assert!(
        store
            .read_account_config(1)
            .expect("a missing root is not an error")
            .is_none()
    );
}

/// `maySpendCredits` is on only when it is JSON `true`: never the Python truthiness `disabled` reads with.
#[test]
fn a_flag_that_is_not_json_true_is_off() {
    let scratch = Scratch::new("a_flag_that_is_not_json_true_is_off");
    let store = scratch.store();
    let cases = [
        (Some(json!(true)), true),
        (Some(json!(1)), false),
        (Some(json!("true")), false),
        (Some(json!("yes")), false),
        (Some(json!(false)), false),
        (Some(Value::Null), false),
        (None, false),
    ];
    for (flag, on) in cases {
        scratch.edit_json("accounts.json", |accounts| {
            let record = accounts["accounts"]["1"]
                .as_object_mut()
                .expect("account 1 is an object");
            match &flag {
                Some(value) => {
                    record.insert("maySpendCredits".to_owned(), value.clone());
                }
                None => {
                    record.shift_remove("maySpendCredits");
                }
            }
        });
        let sequence = store
            .read_sequence()
            .expect("the roster reads")
            .expect("the roster exists");
        let account = sequence.account(1).expect("account 1 is in the fixture");
        assert_eq!(account.may_spend_credits, on, "{flag:?}");
    }
}

/// Setting the flag stores `true` beside every other key, `disabled` included; clearing it removes the key; a set to
/// the state already on disk writes nothing and returns false.
#[test]
fn may_spend_credits_set_and_clear_keeps_other_keys() {
    let scratch = Scratch::new("may_spend_credits_set_and_clear_keeps_other_keys");
    let store = scratch.store();
    let guard = scratch.lock();
    assert!(
        store
            .set_disabled(&guard, 1, true)
            .expect("account 1 can be disabled")
    );

    assert!(
        store
            .set_may_spend_credits(&guard, 1, true)
            .expect("account 1 can be opted in")
    );
    let written = scratch.read_json("accounts.json");
    let record = written["accounts"]["1"]
        .as_object()
        .expect("account 1 is an object");
    assert_eq!(record["maySpendCredits"], json!(true));
    assert_eq!(
        record["disabled"],
        json!(true),
        "the disabled flag is untouched"
    );
    assert!(
        record.contains_key("futureField"),
        "an unknown key round-trips"
    );
    assert_eq!(written["futureTopLevel"], "kept");
    let sequence = store
        .read_sequence()
        .expect("the roster reads")
        .expect("the roster exists");
    let account = sequence.account(1).expect("account 1 is kept");
    assert!(account.may_spend_credits);
    assert!(account.disabled);

    let before = scratch.read("accounts.json");
    assert!(
        !store
            .set_may_spend_credits(&guard, 1, true)
            .expect("a second identical set is fine")
    );
    assert_eq!(
        scratch.read("accounts.json"),
        before,
        "an unchanged flag writes nothing"
    );

    assert!(
        store
            .set_may_spend_credits(&guard, 1, false)
            .expect("account 1 can be opted out")
    );
    let written = scratch.read_json("accounts.json");
    let record = written["accounts"]["1"]
        .as_object()
        .expect("account 1 is an object");
    assert!(
        !record.contains_key("maySpendCredits"),
        "clearing removes the key"
    );
    assert_eq!(
        record["disabled"],
        json!(true),
        "the disabled flag is still untouched"
    );
    assert!(record.contains_key("futureField"));
}

/// A store that refuses writes (sealing fails, or a later schema) refuses the flag too, leaving the file as it was.
#[test]
fn may_spend_credits_refuses_a_read_only_store() {
    let scratch = Scratch::new("may_spend_credits_refuses_a_read_only_store");
    let guard = scratch.lock();
    let before = scratch.read("accounts.json");

    let error = scratch
        .store_without_sealing()
        .set_may_spend_credits(&guard, 1, true)
        .expect_err("a failed canary refuses the write");
    assert_eq!(error.kind(), StoreErrorKind::StoreReadOnly);
    assert_eq!(scratch.read("accounts.json"), before);

    scratch.edit_json("accounts.json", |accounts| {
        accounts["schemaVersion"] = json!(2);
    });
    let before = scratch.read("accounts.json");
    let error = scratch
        .store()
        .set_may_spend_credits(&guard, 1, true)
        .expect_err("an unknown schema refuses the write");
    assert_eq!(error.kind(), StoreErrorKind::UnknownSchema);
    assert_eq!(scratch.read("accounts.json"), before);
}

/// An account's files are named by its number alone, so a write that changes an existing number's email would hand the
/// old identity's files to a new one: it is refused, and the file is left as it was.
#[test]
fn a_write_that_changes_an_accounts_email_is_refused() {
    let scratch = Scratch::new("a_write_that_changes_an_accounts_email_is_refused");
    let store = scratch.store();
    let sequence = store
        .read_sequence()
        .expect("a read works")
        .expect("the fixture has accounts");
    scratch.edit_json("accounts.json", |accounts| {
        accounts["accounts"]["1"]["email"] = json!(BOB);
    });
    let before = scratch.read("accounts.json");
    let guard = scratch.lock();

    let error = store
        .write_sequence(&guard, &sequence)
        .expect_err("a write that changes account 1's email is refused");

    assert_eq!(error.kind(), StoreErrorKind::AccountReidentified);
    assert_eq!(error.account(), Some(1));
    assert_eq!(
        scratch.read("accounts.json"),
        before,
        "every byte is unchanged"
    );
}

/// As [`a_write_that_changes_an_accounts_email_is_refused`], for the organization UUID.
#[test]
fn a_write_that_changes_an_accounts_org_uuid_is_refused() {
    let scratch = Scratch::new("a_write_that_changes_an_accounts_org_uuid_is_refused");
    let store = scratch.store();
    let sequence = store
        .read_sequence()
        .expect("a read works")
        .expect("the fixture has accounts");
    scratch.edit_json("accounts.json", |accounts| {
        accounts["accounts"]["1"]["orgUuid"] = json!("00000000-0000-4000-8000-0000000000b1");
    });
    let before = scratch.read("accounts.json");
    let guard = scratch.lock();

    let error = store
        .write_sequence(&guard, &sequence)
        .expect_err("a write that changes account 1's organization UUID is refused");

    assert_eq!(error.kind(), StoreErrorKind::AccountReidentified);
    assert_eq!(error.account(), Some(1));
    assert_eq!(
        scratch.read("accounts.json"),
        before,
        "every byte is unchanged"
    );
}

/// Filling an empty uuid is not a re-identification: the write lands and the uuid reads back.
#[test]
fn a_write_that_fills_an_empty_uuid_is_allowed() {
    let scratch = Scratch::new("a_write_that_fills_an_empty_uuid_is_allowed");
    scratch.edit_json("accounts.json", |accounts| {
        accounts["accounts"]["1"]
            .as_object_mut()
            .expect("account 1 is an object")
            .shift_remove("uuid");
    });
    let store = scratch.store();
    let guard = scratch.lock();
    let identity = Identity::new(ALICE, "00000000-0000-4000-8000-0000000000a1");

    assert!(
        store
            .backfill_uuid(&guard, 1, "00000000-0000-4000-8000-0000000000ff", &identity)
            .expect("filling an empty uuid is allowed")
    );

    assert_eq!(
        scratch.read_json("accounts.json")["accounts"]["1"]["uuid"],
        json!("00000000-0000-4000-8000-0000000000ff"),
        "the filled uuid is on disk"
    );
}

/// A new order rewrites `order` and nothing else: every other key, known or not, is written back as it was.
#[test]
fn set_order_rewrites_only_order() {
    let scratch = Scratch::new("set_order_rewrites_only_order");
    let store = scratch.store();
    let guard = scratch.lock();
    let mut expected = scratch.read_json("accounts.json");
    expected["order"] = json!([2, 1]);

    assert!(
        store
            .set_order(&guard, &[2, 1])
            .expect("the order can be set")
    );

    assert_eq!(
        scratch.read_json("accounts.json"),
        expected,
        "only the order changed"
    );
    let sequence = store
        .read_sequence()
        .expect("the roster reads")
        .expect("the roster exists");
    assert_eq!(sequence.numbers(), [2, 1]);
}

/// The keys this crate does not know, at the top level and in an account, survive a new order.
#[test]
fn set_order_keeps_unknown_keys() {
    let scratch = Scratch::new("set_order_keeps_unknown_keys");
    let guard = scratch.lock();

    assert!(
        scratch
            .store()
            .set_order(&guard, &[2, 1])
            .expect("the order can be set")
    );

    let written = scratch.read_json("accounts.json");
    assert_eq!(written["futureTopLevel"], "kept");
    let record = written["accounts"]["1"]
        .as_object()
        .expect("account 1 is an object");
    assert!(
        record.contains_key("futureField"),
        "an unknown account key round-trips"
    );
}

/// A list that misses an account, names one the order does not hold, or names one twice is refused, the file as it was.
#[test]
fn set_order_refuses_a_list_that_is_not_a_permutation() {
    let scratch = Scratch::new("set_order_refuses_a_list_that_is_not_a_permutation");
    let store = scratch.store();
    let guard = scratch.lock();
    let before = scratch.read("accounts.json");

    for order in [&[1][..], &[], &[1, 2, 3], &[2, 3], &[1, 1], &[2, 1, 1]] {
        let error = store
            .set_order(&guard, order)
            .expect_err("a list that is not a permutation is refused");
        assert_eq!(error.kind(), StoreErrorKind::OrderMismatch, "{order:?}");
        assert_eq!(
            scratch.read("accounts.json"),
            before,
            "{order:?} wrote nothing"
        );
    }
}

/// The order already on disk writes nothing and answers false.
#[test]
fn set_order_with_the_same_order_writes_nothing() {
    let scratch = Scratch::new("set_order_with_the_same_order_writes_nothing");
    let guard = scratch.lock();
    let before = scratch.read("accounts.json");

    assert!(
        !scratch
            .store()
            .set_order(&guard, &[1, 2])
            .expect("the same order is fine")
    );

    assert_eq!(
        scratch.read("accounts.json"),
        before,
        "an unchanged order writes nothing"
    );
}

/// Another store's lock, a store whose sealing fails and a later schema each refuse a new order, the file as it was.
#[test]
fn set_order_refuses_without_the_lock_or_on_a_read_only_store() {
    let scratch = Scratch::new("set_order_refuses_without_the_lock_or_on_a_read_only_store");
    let other = Scratch::new("set_order_refuses_without_the_lock_other_root");
    let before = scratch.read("accounts.json");

    let error = scratch
        .store()
        .set_order(&other.lock(), &[2, 1])
        .expect_err("another root's lock is refused");
    assert_eq!(error.kind(), StoreErrorKind::WrongLock);
    assert_eq!(scratch.read("accounts.json"), before);

    let guard = scratch.lock();
    let error = scratch
        .store_without_sealing()
        .set_order(&guard, &[2, 1])
        .expect_err("a failed canary refuses the write");
    assert_eq!(error.kind(), StoreErrorKind::StoreReadOnly);
    assert_eq!(scratch.read("accounts.json"), before);

    scratch.edit_json("accounts.json", |accounts| {
        accounts["schemaVersion"] = json!(2);
    });
    let before = scratch.read("accounts.json");
    let error = scratch
        .store()
        .set_order(&guard, &[2, 1])
        .expect_err("an unknown schema refuses the write");
    assert_eq!(error.kind(), StoreErrorKind::UnknownSchema);
    assert_eq!(scratch.read("accounts.json"), before);
}
