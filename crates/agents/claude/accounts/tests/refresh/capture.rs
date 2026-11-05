//! Capturing the live login into the account store: a new login added as the next account and made active, a matched
//! account's credential written only when the live copy is later, and every refusal made before any write.

use std::fs;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt as _;

use claude_accounts::seal::{self, AssociatedData, SealPurpose};
use claude_accounts::{
    AccessToken, AccountLock, CaptureError, CaptureKind, CaptureOutcome, CredentialBlob, Email,
    ProfileTransport, RawProfile, RefreshContext, TransportError, capture_live_login,
    credential_fingerprint,
};
use serde_json::{Value, json};

use crate::links::{junction, snapshot};
use crate::support::{
    FakeEndpoint, FakeProfile, ONE, ONE_ORG, Rig, Scratch, credential, fixture_credential_one,
    ms_after, profile,
};

/// Account 1's and account 2's uuids in the usage fixture; account 2 has no organization.
const ONE_UUID: &str = "00000000-0000-4000-8000-000000000011";
const TWO: &str = "two@example.test";
const TWO_UUID: &str = "00000000-0000-4000-8000-000000000012";
/// A login no account has.
const THREE: &str = "three@example.test";
const THREE_ORG: &str = "00000000-0000-4000-8000-0000000000c3";
const THREE_UUID: &str = "00000000-0000-4000-8000-000000000013";
const STRANGER_UUID: &str = "00000000-0000-4000-8000-0000000000ff";
/// An hour past account 1's fixture credential's `expiresAt` (2100-01-01), in ms since the epoch.
const AFTER_FIXTURE_MS: i64 = 4_102_444_800_000 + 3_600_000;

fn oauth_account(email: &str, org: &str, uuid: &str) -> Value {
    json!({"emailAddress": email, "organizationUuid": org, "accountUuid": uuid, "organizationName": "Org", "displayName": "Someone"})
}

fn write_global(scratch: &Scratch, config: &Value) {
    fs::write(scratch.dir.join(".claude.json"), config.to_string())
        .expect("the global config can be written");
}

fn write_live(scratch: &Scratch, live: &str) {
    fs::create_dir_all(scratch.dir.join("claude")).expect("Claude Code's folder can be made");
    fs::write(scratch.claude().credentials_file(), live)
        .expect("the live credential can be written");
}

/// A rig logged in as `email` in `org` with account uuid `uuid`, whose live credential file holds `live` and whose
/// profile endpoint answers `answers` in turn.
fn logged_in(
    test: &str,
    (email, org, uuid): (&str, &str, &str),
    live: &str,
    answers: Vec<Result<RawProfile, TransportError>>,
) -> Rig {
    let scratch = Scratch::new(test);
    write_global(
        &scratch,
        &json!({"numStartups": 2, "oauthAccount": oauth_account(email, org, uuid)}),
    );
    write_live(&scratch, live);
    let mut rig = Rig::new(scratch, FakeEndpoint::answering(Vec::new()));
    rig.profile = FakeProfile::answering(answers);
    rig
}

/// A rig logged in as account 1, its profile endpoint confirming it once.
fn logged_in_as_one(test: &str, live: &str) -> Rig {
    logged_in(
        test,
        (ONE, ONE_ORG, ONE_UUID),
        live,
        vec![profile(ONE_UUID, ONE, ONE_ORG)],
    )
}

fn outcome(kind: CaptureKind, number: u32, email: &str) -> CaptureOutcome {
    CaptureOutcome {
        kind,
        number,
        email: Email::parse(email).expect("the test email is safe"),
    }
}

fn accounts_bytes(rig: &Rig) -> Vec<u8> {
    fs::read(rig.scratch.backup().join("accounts.json")).expect("accounts.json can be read")
}

fn accounts_json(rig: &Rig) -> Value {
    serde_json::from_slice(&accounts_bytes(rig)).expect("accounts.json is JSON")
}

fn stored(rig: &Rig, number: u32) -> Option<String> {
    rig.store
        .read_credentials(number)
        .expect("the stored credential opens")
        .map(|blob| blob.expose().to_owned())
}

fn config_backup(rig: &Rig, number: u32) -> Value {
    let bytes =
        fs::read(rig.scratch.root().config_path(number)).expect("the config backup can be read");
    serde_json::from_slice(&bytes).expect("the config backup is JSON")
}

#[test]
fn capture_adds_a_new_login_as_the_next_number_and_makes_it_active() {
    let live = credential(
        "fake-access-three",
        Some("fake-refresh-three"),
        ms_after(3600),
    );
    let rig = logged_in(
        "capture_adds_a_new_login",
        (THREE, THREE_ORG, THREE_UUID),
        &live,
        vec![profile(THREE_UUID, THREE, THREE_ORG)],
    );

    let captured = capture_live_login(&rig.context()).expect("the new login is added");

    assert_eq!(captured, outcome(CaptureKind::Added, 3, THREE));
    let sequence = accounts_json(&rig);
    assert_eq!(
        sequence["active"],
        json!(3),
        "the added account is the active one"
    );
    assert_eq!(sequence["order"], json!([1, 2, 3]));
    assert_eq!(sequence["nextNumber"], json!(4));
    let record = &sequence["accounts"]["3"];
    assert_eq!(record["email"], json!(THREE));
    assert_eq!(record["uuid"], json!(THREE_UUID));
    assert_eq!(record["orgUuid"], json!(THREE_ORG));
    assert_eq!(record["orgName"], json!("Org"));
    assert_eq!(
        stored(&rig, 3),
        Some(live),
        "the whole live credential text is stored"
    );
    assert_eq!(
        config_backup(&rig, 3),
        json!({"oauthAccount": oauth_account(THREE, THREE_ORG, THREE_UUID)})
    );
    assert_eq!(
        rig.profile.asked(),
        vec!["fake-access-three".to_owned()],
        "the oracle was asked once"
    );
}

#[test]
fn capture_refreshes_a_matched_account_when_the_live_copy_is_later() {
    let live = credential(
        "fake-access-one-new",
        Some("fake-refresh-one-new"),
        AFTER_FIXTURE_MS,
    );
    let rig = logged_in_as_one("capture_refreshes_a_matched_account", &live);
    rig.scratch.set_active(2);

    let captured = capture_live_login(&rig.context()).expect("the matched account is refreshed");

    assert_eq!(captured, outcome(CaptureKind::Refreshed, 1, ONE));
    assert_eq!(stored(&rig, 1), Some(live), "the live copy is stored");
    let sequence = accounts_json(&rig);
    assert_eq!(
        sequence["active"],
        json!(1),
        "the refreshed account is the active one"
    );
    assert_eq!(sequence["order"], json!([1, 2]), "no account was added");
    assert_eq!(
        config_backup(&rig, 1),
        json!({"oauthAccount": oauth_account(ONE, ONE_ORG, ONE_UUID)})
    );
}

/// Stash ids of account 1's rows in the capture tests.
const HELD_ID: &str = "20260921T140000-abcdefabcdef-a1b2c3";
const FOREIGN_ID: &str = "20260921T140001-abcdefabcdef-d4e5f6";

/// A capture that stores a new login retires the stash rows that held up the account's old generation (their entry
/// does not open), and keeps a row that records no consumed generation, which may be another login's only copy.
#[test]
fn a_capture_that_stores_a_new_login_retires_the_entries_that_held_the_old_one() {
    let live = credential(
        "fake-access-one-new",
        Some("fake-refresh-one-new"),
        AFTER_FIXTURE_MS,
    );
    let rig = logged_in_as_one("capture_retires_held_entries", &live);
    let parked = credential(
        "fake-access-parked",
        Some("fake-refresh-parked"),
        AFTER_FIXTURE_MS,
    );
    rig.scratch
        .write_stash_entry_for(HELD_ID, &parked, "2", ONE_ORG);
    rig.scratch.write_stash_entry(FOREIGN_ID, &parked);
    let fingerprint = |text: &str| credential_fingerprint(&CredentialBlob::new(text.to_owned()));
    let row = |reason: &str, consumed: Value| {
        json!({
            "createdAt": "2026-09-21T14:00:00Z",
            "reason": reason,
            "configSlot": "1",
            "consumedFp": consumed,
            "fingerprint": fingerprint(&parked),
            "organization": ONE_ORG,
        })
    };
    let foreign = row("foreign", Value::Null);
    let entries = json!({
        HELD_ID: row("consume-gate-persist-failed", json!(fingerprint(&fixture_credential_one()))),
        FOREIGN_ID: foreign.clone(),
    });
    let manifest = json!({ "schemaVersion": 1, "entries": entries });
    fs::write(rig.scratch.manifest_path(), manifest.to_string())
        .expect("the manifest can be written");
    let foreign_bytes = rig.scratch.stash_bytes(FOREIGN_ID);

    let captured = capture_live_login(&rig.context()).expect("the matched account is refreshed");

    assert_eq!(captured, outcome(CaptureKind::Refreshed, 1, ONE));
    assert_eq!(
        rig.scratch.stash_bytes(HELD_ID),
        None,
        "the held entry's file is deleted"
    );
    assert_eq!(
        rig.scratch.manifest()["entries"],
        json!({ FOREIGN_ID: foreign }),
        "only the held entry's row goes"
    );
    assert_eq!(
        rig.scratch.stash_bytes(FOREIGN_ID),
        foreign_bytes,
        "a row with no consumed generation keeps its entry"
    );
}

/// A rig logged in as account 1 with a later live copy, whose stash holds one row under [`HELD_ID`] keyed to the stored
/// fixture credential; the test puts what it wants at the entry's name. Returns the rig and the row.
fn held_before_capture(test: &str) -> (Rig, Value) {
    let live = credential(
        "fake-access-one-new",
        Some("fake-refresh-one-new"),
        AFTER_FIXTURE_MS,
    );
    let rig = logged_in_as_one(test, &live);
    let fingerprint = |text: &str| credential_fingerprint(&CredentialBlob::new(text.to_owned()));
    let row = json!({
        "createdAt": "2026-09-21T14:00:00Z",
        "reason": "consume-gate-persist-failed",
        "configSlot": "1",
        "consumedFp": fingerprint(&fixture_credential_one()),
        "fingerprint": "sha256-full:another-credential",
        "organization": ONE_ORG,
    });
    let manifest = json!({ "schemaVersion": 1, "entries": { HELD_ID: row.clone() } });
    fs::write(rig.scratch.manifest_path(), manifest.to_string())
        .expect("the manifest can be written");
    (rig, row)
}

#[test]
fn a_capture_removes_a_held_entry_that_is_a_junction_and_never_what_it_points_at() {
    let (rig, _) = held_before_capture("capture_retires_a_junction_entry");
    let outside = rig.scratch.dir.join("outside-entry");
    fs::create_dir_all(&outside).expect("the outside folder can be made");
    fs::write(outside.join("planted.txt"), b"not the stash's")
        .expect("the planted file can be written");
    junction(&rig.scratch.stash_path(HELD_ID), &outside);
    let before = snapshot(&outside);

    let captured = capture_live_login(&rig.context()).expect("the matched account is refreshed");

    assert_eq!(captured, outcome(CaptureKind::Refreshed, 1, ONE));
    assert!(
        fs::symlink_metadata(rig.scratch.stash_path(HELD_ID)).is_err(),
        "the junction itself is removed"
    );
    assert_eq!(
        rig.scratch.manifest()["entries"],
        json!({}),
        "its row is removed"
    );
    assert_eq!(
        snapshot(&outside),
        before,
        "the folder the junction pointed at keeps its file"
    );
}

#[cfg(windows)]
#[test]
fn a_capture_keeps_the_row_of_a_held_entry_whose_file_cannot_be_removed() {
    /// `FILE_SHARE_READ`: a handle opened with it lets others read the file but not delete it.
    const FILE_SHARE_READ: u32 = 1;
    let (rig, row) = held_before_capture("capture_keeps_an_unremovable_entry");
    rig.scratch
        .write_stash_raw(HELD_ID, b"bytes that do not open");
    let path = rig.scratch.stash_path(HELD_ID);
    let handle = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&path)
        .expect("the entry can be held open");

    let captured = capture_live_login(&rig.context()).expect("the matched account is refreshed");

    drop(handle);
    assert_eq!(captured, outcome(CaptureKind::Refreshed, 1, ONE));
    assert!(path.exists(), "the entry's file stays");
    assert_eq!(
        rig.scratch.manifest()["entries"][HELD_ID],
        row,
        "its row stays, so its bytes are not orphaned"
    );
}

#[test]
fn capture_keeps_a_stored_copy_newer_than_the_live_one() {
    let live = credential(
        "fake-access-one-old",
        Some("fake-refresh-one-old"),
        ms_after(3600),
    );
    let rig = logged_in_as_one("capture_keeps_a_newer_stored_copy", &live);
    let before = accounts_bytes(&rig);

    let captured = capture_live_login(&rig.context()).expect("the capture answers");

    assert_eq!(captured, outcome(CaptureKind::Kept, 1, ONE));
    assert_eq!(
        stored(&rig, 1),
        Some(fixture_credential_one()),
        "the newer stored copy is kept"
    );
    assert_eq!(accounts_bytes(&rig), before, "nothing is written");
}

#[test]
fn capture_writes_nothing_when_the_live_copy_is_the_stored_one() {
    // The fixture's active account is account 1 already, so not even the active account is written.
    let rig = logged_in_as_one(
        "capture_writes_nothing_for_the_stored_copy",
        &fixture_credential_one(),
    );
    let before = accounts_bytes(&rig);
    let credential_before =
        fs::read(rig.scratch.credential_file()).expect("account 1's file can be read");

    let captured = capture_live_login(&rig.context()).expect("the capture answers");

    assert_eq!(captured, outcome(CaptureKind::Unchanged, 1, ONE));
    assert_eq!(accounts_bytes(&rig), before, "accounts.json is untouched");
    assert_eq!(
        fs::read(rig.scratch.credential_file()).expect("account 1's file can be read"),
        credential_before,
        "the credential file is untouched"
    );
    assert!(
        !rig.scratch.root().config_path(1).exists(),
        "no config backup is written"
    );
}

#[test]
fn capture_refuses_an_unverified_login_before_any_lock_or_write() {
    let live = credential(
        "fake-access-three",
        Some("fake-refresh-three"),
        ms_after(3600),
    );
    let rig = logged_in(
        "capture_refuses_an_unverified_login",
        (THREE, THREE_ORG, THREE_UUID),
        &live,
        vec![
            Err(TransportError::Connect),
            profile(STRANGER_UUID, THREE, THREE_ORG),
        ],
    );
    let before = accounts_bytes(&rig);
    // Held for the whole test: a capture that waited for the account lock would answer locks busy, not unverified.
    let _held = AccountLock::acquire(&rig.scratch.root()).expect("the account lock is free");

    let offline = capture_live_login(&rig.context())
        .expect_err("an oracle that does not answer confirms nothing");
    let foreign = capture_live_login(&rig.context())
        .expect_err("an oracle naming another account confirms nothing");

    assert_eq!(offline, CaptureError::Unverified);
    assert_eq!(foreign, CaptureError::Unverified);
    assert_eq!(rig.profile.asked().len(), 2);
    assert_eq!(accounts_bytes(&rig), before, "nothing is written");
    assert!(
        !rig.scratch.root().credential_path(3).exists(),
        "no credential file is written"
    );
}

#[test]
fn capture_refuses_an_api_key_login_and_a_login_without_a_refresh_token() {
    let live = credential(
        "fake-access-one-new",
        Some("fake-refresh-one-new"),
        AFTER_FIXTURE_MS,
    );
    let rig = logged_in_as_one("capture_refuses_an_api_key_login", &live);
    let before = accounts_bytes(&rig);
    write_global(
        &rig.scratch,
        &json!({"primaryApiKey": "fake-api-key", "oauthAccount": oauth_account(ONE, ONE_ORG, ONE_UUID)}),
    );

    let api_key =
        capture_live_login(&rig.context()).expect_err("an API-key login is no subscription login");

    write_global(
        &rig.scratch,
        &json!({"oauthAccount": oauth_account(ONE, ONE_ORG, ONE_UUID)}),
    );
    write_live(
        &rig.scratch,
        &credential("fake-access-one-new", None, AFTER_FIXTURE_MS),
    );
    let no_refresh =
        capture_live_login(&rig.context()).expect_err("a login without a refresh token is refused");

    assert_eq!(api_key, CaptureError::ApiKeyLogin);
    assert_eq!(no_refresh, CaptureError::NoRefreshToken);
    assert!(rig.profile.asked().is_empty(), "neither reached the oracle");
    assert_eq!(accounts_bytes(&rig), before, "nothing is written");
    assert_eq!(stored(&rig, 1), Some(fixture_credential_one()));
}

#[test]
fn capture_refuses_when_the_stored_credential_does_not_open() {
    let live = credential(
        "fake-access-one-new",
        Some("fake-refresh-one-new"),
        AFTER_FIXTURE_MS,
    );
    let rig = logged_in_as_one("capture_refuses_an_unopenable_stored_credential", &live);
    let other = seal::seal(
        SealPurpose::Credentials,
        &AssociatedData::credentials(2, "", 1, TWO),
        &CredentialBlob::new(live.clone()),
    )
    .expect("another account's file seals");
    fs::write(rig.scratch.credential_file(), other.as_bytes())
        .expect("account 1's file can be replaced");
    let before = accounts_bytes(&rig);

    let error = capture_live_login(&rig.context())
        .expect_err("an unopenable stored credential is not replaced");

    assert_eq!(error, CaptureError::StoredUnopenable { number: 1 });
    assert_eq!(
        fs::read(rig.scratch.credential_file()).expect("account 1's file can be read"),
        other.as_bytes(),
        "the stored file is left as it is"
    );
    assert_eq!(accounts_bytes(&rig), before, "nothing is written");
}

#[test]
fn capture_refuses_an_oauth_login_that_collides_with_an_api_key_record() {
    let live = credential(
        "fake-access-two-new",
        Some("fake-refresh-two-new"),
        AFTER_FIXTURE_MS,
    );
    let rig = logged_in(
        "capture_refuses_a_collision_with_an_api_key_record",
        (TWO, "", TWO_UUID),
        &live,
        vec![profile(TWO_UUID, TWO, "")],
    );
    rig.scratch.set_account(2, "kind", json!("api_key"));
    let before = accounts_bytes(&rig);

    let error = capture_live_login(&rig.context())
        .expect_err("an OAuth login never matches an API-key record");

    assert_eq!(error, CaptureError::StoredAsApiKey);
    assert_eq!(accounts_bytes(&rig), before, "nothing is written");
    assert!(
        !rig.scratch.root().credential_path(3).exists(),
        "no account is added beside it"
    );
}

#[test]
fn capture_keeps_disabled_alias_and_credits_on_a_matched_account() {
    let live = credential(
        "fake-access-one-new",
        Some("fake-refresh-one-new"),
        AFTER_FIXTURE_MS,
    );
    let rig = logged_in_as_one("capture_keeps_disabled_alias_and_credits", &live);
    rig.scratch.set_account(1, "disabled", json!(true));
    rig.scratch.set_account(1, "alias", json!("work"));
    rig.scratch.set_account(1, "maySpendCredits", json!(true));
    rig.scratch.set_account(1, "uuid", json!(""));

    let captured = capture_live_login(&rig.context()).expect("the matched account is refreshed");

    assert_eq!(captured, outcome(CaptureKind::Refreshed, 1, ONE));
    let record = &accounts_json(&rig)["accounts"]["1"];
    assert_eq!(record["disabled"], json!(true));
    assert_eq!(record["alias"], json!("work"));
    assert_eq!(record["maySpendCredits"], json!(true));
    assert_eq!(
        record["uuid"],
        json!(ONE_UUID),
        "the empty uuid is filled from the login"
    );
    assert_eq!(stored(&rig, 1), Some(live));
}

#[test]
fn capture_records_the_resolved_uuid_on_a_new_login_without_one() {
    let live = credential(
        "fake-access-three",
        Some("fake-refresh-three"),
        ms_after(3600),
    );
    let rig = logged_in(
        "capture_records_the_resolved_uuid",
        (THREE, THREE_ORG, ""),
        &live,
        vec![profile(THREE_UUID, THREE, THREE_ORG)],
    );

    let captured = capture_live_login(&rig.context()).expect("the new login is added");

    assert_eq!(captured, outcome(CaptureKind::Added, 3, THREE));
    let sequence = accounts_json(&rig);
    assert_eq!(
        sequence["accounts"]["3"]["uuid"],
        json!(THREE_UUID),
        "the uuid the profile endpoint resolved is recorded on the new record"
    );
    assert_eq!(
        config_backup(&rig, 3),
        json!({"oauthAccount": oauth_account(THREE, THREE_ORG, "")}),
        "the config backup is left as Claude Code wrote it, with no uuid added"
    );
}

#[test]
fn capture_backfills_a_matched_account_without_a_uuid_from_the_profile() {
    let live = credential(
        "fake-access-one-new",
        Some("fake-refresh-one-new"),
        AFTER_FIXTURE_MS,
    );
    let rig = logged_in(
        "capture_backfills_a_matched_account_without_a_uuid",
        (ONE, ONE_ORG, ""),
        &live,
        vec![profile(ONE_UUID, ONE, ONE_ORG)],
    );
    rig.scratch.set_account(1, "uuid", json!(""));

    let captured = capture_live_login(&rig.context()).expect("the matched account is refreshed");

    assert_eq!(captured, outcome(CaptureKind::Refreshed, 1, ONE));
    assert_eq!(
        accounts_json(&rig)["accounts"]["1"]["uuid"],
        json!(ONE_UUID),
        "the empty uuid is filled from the profile endpoint's answer"
    );
}

#[test]
fn capture_backfills_the_uuid_when_the_stored_copy_is_kept() {
    let live = credential(
        "fake-access-one-old",
        Some("fake-refresh-one-old"),
        ms_after(3600),
    );
    let rig = logged_in(
        "capture_backfills_the_uuid_when_the_stored_copy_is_kept",
        (ONE, ONE_ORG, ""),
        &live,
        vec![profile(ONE_UUID, ONE, ONE_ORG)],
    );
    rig.scratch.set_account(1, "uuid", json!(""));

    let captured = capture_live_login(&rig.context()).expect("the capture answers");

    assert_eq!(captured, outcome(CaptureKind::Kept, 1, ONE));
    assert_eq!(
        accounts_json(&rig)["accounts"]["1"]["uuid"],
        json!(ONE_UUID),
        "the empty uuid is filled even though the stored copy is kept"
    );
    assert_eq!(
        stored(&rig, 1),
        Some(fixture_credential_one()),
        "the newer stored copy is kept"
    );
}

#[test]
fn capture_backfills_the_uuid_when_the_live_copy_is_unchanged() {
    let rig = logged_in(
        "capture_backfills_the_uuid_when_the_live_copy_is_unchanged",
        (ONE, ONE_ORG, ""),
        &fixture_credential_one(),
        vec![profile(ONE_UUID, ONE, ONE_ORG)],
    );
    rig.scratch.set_account(1, "uuid", json!(""));
    let credential_before =
        fs::read(rig.scratch.credential_file()).expect("account 1's file can be read");

    let captured = capture_live_login(&rig.context()).expect("the capture answers");

    assert_eq!(captured, outcome(CaptureKind::Unchanged, 1, ONE));
    assert_eq!(
        accounts_json(&rig)["accounts"]["1"]["uuid"],
        json!(ONE_UUID),
        "the empty uuid is filled even though the live copy is the stored one"
    );
    assert_eq!(
        fs::read(rig.scratch.credential_file()).expect("account 1's file can be read"),
        credential_before,
        "the credential file is untouched"
    );
    assert!(
        !rig.scratch.root().config_path(1).exists(),
        "no config backup is written"
    );
}

#[test]
fn capture_refuses_a_matched_account_whose_uuid_differs_from_the_resolved_one() {
    let live = credential(
        "fake-access-one-new",
        Some("fake-refresh-one-new"),
        AFTER_FIXTURE_MS,
    );
    // The live `oauthAccount` names no uuid, so the profile answer resolves one, and the record already holds another.
    let rig = logged_in(
        "capture_refuses_a_matched_account_with_another_uuid",
        (ONE, ONE_ORG, ""),
        &live,
        vec![profile(ONE_UUID, ONE, ONE_ORG)],
    );
    rig.scratch.set_account(1, "uuid", json!(STRANGER_UUID));
    let before = accounts_bytes(&rig);
    let credential_before =
        fs::read(rig.scratch.credential_file()).expect("account 1's file can be read");

    let error = capture_live_login(&rig.context())
        .expect_err("a record naming another account id is not refreshed");

    assert_eq!(error, CaptureError::AnotherAccountId { number: 1 });
    assert!(
        !rig.scratch.root().config_path(1).exists(),
        "no config backup is written"
    );
    assert_eq!(accounts_bytes(&rig), before, "accounts.json is unchanged");
    assert_eq!(
        fs::read(rig.scratch.credential_file()).expect("account 1's file can be read"),
        credential_before,
        "the stored credential is unchanged"
    );
    assert_eq!(
        stored(&rig, 1),
        Some(fixture_credential_one()),
        "the stored copy is left as it is"
    );
}

#[test]
fn capture_refuses_a_pure_api_key_login() {
    let rig = logged_in_as_one(
        "capture_refuses_a_pure_api_key_login",
        r#"{"mcpOAuth":{"keep":"me"}}"#,
    );
    write_global(
        &rig.scratch,
        &json!({"primaryApiKey": "fake-api-key", "numStartups": 1}),
    );
    let before = accounts_bytes(&rig);

    let error =
        capture_live_login(&rig.context()).expect_err("a login that is only an API key is refused");

    assert_eq!(
        error,
        CaptureError::ApiKeyLogin,
        "named as an API key, not as no login"
    );
    assert!(rig.profile.asked().is_empty(), "the oracle was not asked");
    assert_eq!(accounts_bytes(&rig), before, "nothing is written");
}

#[test]
fn capture_keeps_a_same_lineage_stored_copy_with_a_later_expiry() {
    // Account 1's own refresh token, with an access token that expires before the stored one does.
    let live = credential(
        "fake-access-one-older",
        Some("fake-refresh-one"),
        ms_after(3600),
    );
    let rig = logged_in_as_one("capture_keeps_a_same_lineage_stored_copy", &live);
    let before = accounts_bytes(&rig);

    let captured = capture_live_login(&rig.context()).expect("the capture answers");

    assert_eq!(captured, outcome(CaptureKind::Kept, 1, ONE));
    assert_eq!(
        stored(&rig, 1),
        Some(fixture_credential_one()),
        "the later stored copy is kept"
    );
    assert_eq!(accounts_bytes(&rig), before, "nothing is written");
}

#[test]
fn capture_keeps_a_stored_copy_with_no_expiry() {
    let live = credential(
        "fake-access-one-new",
        Some("fake-refresh-one-new"),
        AFTER_FIXTURE_MS,
    );
    let rig = logged_in_as_one("capture_keeps_a_stored_copy_with_no_expiry", &live);
    let no_expiry = r#"{"claudeAiOauth":{"accessToken":"fake-access-one-stored","refreshToken":"fake-refresh-one-stored"}}"#;
    crate::support::put_credentials(&rig.scratch.root(), no_expiry);
    let before = accounts_bytes(&rig);

    let captured = capture_live_login(&rig.context()).expect("the capture answers");

    assert_eq!(captured, outcome(CaptureKind::Kept, 1, ONE));
    assert_eq!(
        stored(&rig, 1).as_deref(),
        Some(no_expiry),
        "a stored copy whose age cannot be told is never overwritten"
    );
    assert_eq!(accounts_bytes(&rig), before, "nothing is written");
}

#[test]
fn capture_unchanged_makes_the_matched_account_active() {
    let rig = logged_in_as_one(
        "capture_unchanged_makes_the_matched_account_active",
        &fixture_credential_one(),
    );
    rig.scratch.set_active(2);
    let credential_before =
        fs::read(rig.scratch.credential_file()).expect("account 1's file can be read");

    let captured = capture_live_login(&rig.context()).expect("the capture answers");

    assert_eq!(captured, outcome(CaptureKind::Unchanged, 1, ONE));
    assert_eq!(
        accounts_json(&rig)["active"],
        json!(1),
        "the live login's account is made active"
    );
    assert_eq!(
        fs::read(rig.scratch.credential_file()).expect("account 1's file can be read"),
        credential_before,
        "the credential file is untouched"
    );
}

/// A profile endpoint that confirms account 1's login and, while it answers, replaces the live credential file with
/// another login's tokens, as a `/login` landing between the capture's two reads would.
struct RewritingProfile {
    live: std::path::PathBuf,
    replacement: String,
}

impl ProfileTransport for RewritingProfile {
    fn get_profile(&self, _access: &AccessToken) -> Result<RawProfile, TransportError> {
        fs::write(&self.live, &self.replacement).expect("the live credential can be rewritten");
        profile(ONE_UUID, ONE, ONE_ORG)
    }
}

#[test]
fn capture_refuses_a_login_that_changes_between_the_reads() {
    let live = credential(
        "fake-access-one-new",
        Some("fake-refresh-one-new"),
        AFTER_FIXTURE_MS,
    );
    let rig = logged_in_as_one(
        "capture_refuses_a_login_that_changes_between_the_reads",
        &live,
    );
    let rewriting = RewritingProfile {
        live: rig.claude.credentials_file(),
        replacement: credential(
            "fake-access-one-other",
            Some("fake-refresh-one-other"),
            AFTER_FIXTURE_MS,
        ),
    };
    let before = accounts_bytes(&rig);
    let context = RefreshContext {
        profile: &rewriting,
        ..rig.context()
    };

    let error = capture_live_login(&context)
        .expect_err("a login that changed under the capture is refused");

    assert_eq!(error, CaptureError::LoginChanged);
    assert_eq!(
        stored(&rig, 1),
        Some(fixture_credential_one()),
        "the stored copy is untouched"
    );
    assert_eq!(accounts_bytes(&rig), before, "nothing is written");
}

#[test]
fn capture_refreshes_a_same_lineage_live_copy_with_a_later_expiry() {
    // Account 1's own refresh token, with an access token that expires after the stored one does.
    let live = credential(
        "fake-access-one-later",
        Some("fake-refresh-one"),
        AFTER_FIXTURE_MS,
    );
    let rig = logged_in_as_one("capture_refreshes_a_same_lineage_live_copy", &live);

    let captured = capture_live_login(&rig.context()).expect("the matched account is refreshed");

    assert_eq!(captured, outcome(CaptureKind::Refreshed, 1, ONE));
    assert_eq!(stored(&rig, 1), Some(live), "the later live copy is stored");
}
