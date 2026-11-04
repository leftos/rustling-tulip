//! The engine's freshen and quarantine helpers, ported from cswap's `_freshen_target`, `_note_token_identity`,
//! `_quarantine` and `_release_recovered_quarantines`, against a fake token endpoint and a scratch account store.

use std::fs;

use claude_accounts::engine::{
    AutoswitchState, Freshened, ReleaseReason, freshen_target, quarantine,
    release_recovered_quarantines,
};
use claude_accounts::{AccountRecord, CredentialBlob, GrantError, Store, credential_fingerprint};
use serde_json::{Value, json};

use crate::support::{
    FakeEndpoint, ONE, ONE_ORG, Rig, Scratch, credential, email, granted, ms_after, now,
    put_credentials, with_account,
};

/// Account 1's uuid in the usage fixture.
const ONE_UUID: &str = "00000000-0000-4000-8000-000000000011";

/// A rig whose account 1 is a backup account (account 2 is the active one) holding a credential that expired at 0 and
/// a config backup naming its login, so a freshen spends its grant.
fn expiring_rig(test: &str, endpoint: FakeEndpoint) -> Rig {
    let scratch = Scratch::new(test);
    scratch.set_active(2);
    let rig = Rig::new(scratch, endpoint);
    put_credentials(
        &rig.scratch.root(),
        &credential("fake-access-one", Some("fake-refresh-one"), 0),
    );
    write_config_backup(&rig, ONE, ONE_ORG);
    rig
}

/// Writes account 1's config backup with an `oauthAccount` naming `email` in `org`.
fn write_config_backup(rig: &Rig, email: &str, org: &str) {
    let path = rig.scratch.root().config_path(1);
    fs::create_dir_all(path.parent().expect("a config backup has a folder"))
        .expect("the configs folder can be made");
    let config = json!({ "oauthAccount": { "emailAddress": email, "organizationUuid": org } });
    fs::write(&path, config.to_string()).expect("account 1's config backup can be written");
}

/// Every file under the rig's account store, lock files aside, with its bytes.
fn stored_files(rig: &Rig) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    fn collect(
        dir: &std::path::Path,
        found: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
    ) {
        for entry in fs::read_dir(dir).expect("a scratch folder can be listed") {
            let path = entry.expect("an entry can be read").path();
            if path.is_dir() {
                collect(&path, found);
            } else if path.extension().is_none_or(|extension| extension != "lock") {
                found.insert(
                    path.clone(),
                    fs::read(&path).expect("a scratch file can be read"),
                );
            }
        }
    }
    let mut found = std::collections::BTreeMap::new();
    collect(&rig.scratch.backup(), &mut found);
    found
}

/// A store that refuses writes spends no grant for a freshen: nothing is posted and nothing stored changes.
#[test]
fn freshen_on_a_refusing_store_spends_nothing() {
    let endpoint = granting_as(
        json!({ "uuid": ONE_UUID, "email_address": ONE }),
        json!({ "uuid": ONE_ORG }),
    );
    let mut rig = expiring_rig("freshen_refusing_store", endpoint);
    rig.store =
        Store::open_with_canary(rig.scratch.root(), || false).expect("the scratch store opens");
    let before = stored_files(&rig);

    let freshened =
        freshen_target(&rig.context(), &account_one(&rig)).expect("no quarantine write was needed");

    assert_eq!(freshened, Freshened::Failed(GrantError::StoreUnwritable));
    assert_eq!(freshened.kind(), "store-unwritable");
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(stored_files(&rig), before, "nothing stored changed");
    assert_eq!(quarantined_reason(&rig, "1"), None);
}

/// A target whose config backup names another login than its record is refused before its grant is spent: nothing is
/// posted and nothing stored changes.
#[test]
fn freshen_refuses_a_target_whose_config_backup_names_another_login_without_spending() {
    let endpoint = granting_as(
        json!({ "uuid": ONE_UUID, "email_address": ONE }),
        json!({ "uuid": ONE_ORG }),
    );
    let rig = expiring_rig("freshen_identity_mismatch", endpoint);
    write_config_backup(&rig, "other@example.test", ONE_ORG);
    let before = stored_files(&rig);

    let freshened =
        freshen_target(&rig.context(), &account_one(&rig)).expect("nothing was written");

    assert_eq!(freshened, Freshened::IdentityMismatch);
    assert_eq!(freshened.kind(), "identity-mismatch");
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert_eq!(stored_files(&rig), before, "nothing stored changed");
}

/// The token endpoint granting a successor and naming the account `account` in organization `organization`.
fn granting_as(account: Value, organization: Value) -> FakeEndpoint {
    FakeEndpoint::answering(vec![with_account(
        granted("fake-access-new", Some("fake-refresh-new"), 3600),
        account,
        organization,
    )])
}

fn account_one(rig: &Rig) -> AccountRecord {
    rig.store
        .read_sequence()
        .expect("accounts.json can be read")
        .expect("the fixture has a accounts.json")
        .account(1)
        .expect("the fixture has account 1")
}

fn quarantined_reason(rig: &Rig, number: &str) -> Option<String> {
    let state = AutoswitchState::read(&rig.scratch.root()).expect("the state file can be read");
    state
        .quarantine
        .get(number)
        .and_then(|record| record.entry())
        .map(|entry| entry.reason.clone())
}

fn stored_access(rig: &Rig) -> Option<String> {
    rig.stored().map(|text| crate::support::access_of(&text))
}

#[test]
fn test_uuid_backfill_from_token_account_on_freshen() {
    let endpoint = granting_as(
        json!({ "uuid": "uuid-1-real", "email_address": ONE }),
        json!({ "uuid": "" }),
    );
    let rig = expiring_rig("freshen_backfill", endpoint);
    rig.scratch.set_account(1, "uuid", json!(""));

    let freshened =
        freshen_target(&rig.context(), &account_one(&rig)).expect("no quarantine write was needed");

    assert_eq!(freshened, Freshened::Ready);
    assert_eq!(
        account_one(&rig).uuid,
        "uuid-1-real",
        "the blank uuid is backfilled from the token's account"
    );
    assert_eq!(quarantined_reason(&rig, "1"), None);
}

#[test]
fn test_conflicting_token_identity_returns_identity_conflict() {
    let endpoint = granting_as(
        json!({ "uuid": "uuid-somebody-else", "email_address": "z@example.test" }),
        json!({ "uuid": "" }),
    );
    let rig = expiring_rig("freshen_conflict", endpoint);

    let freshened =
        freshen_target(&rig.context(), &account_one(&rig)).expect("the quarantine is written");

    assert_eq!(freshened, Freshened::IdentityConflict);
    assert_eq!(freshened.kind(), "identity-conflict");
    assert_eq!(
        stored_access(&rig).as_deref(),
        Some("fake-access-new"),
        "the spent grant's successor is persisted regardless"
    );
    assert_eq!(
        account_one(&rig).uuid,
        ONE_UUID,
        "a conflicting uuid is never written"
    );
}

#[test]
fn test_identity_conflict_quarantines_instead_of_activating() {
    let endpoint = granting_as(
        json!({ "uuid": "uuid-somebody-else", "email_address": "z@example.test" }),
        json!({ "uuid": "" }),
    );
    let rig = expiring_rig("freshen_conflict_quarantines", endpoint);

    let freshened =
        freshen_target(&rig.context(), &account_one(&rig)).expect("the quarantine is written");

    assert_ne!(
        freshened,
        Freshened::Ready,
        "a conflicted target is not switchable"
    );
    let state = AutoswitchState::read(&rig.scratch.root()).expect("the state file can be read");
    let entry = state
        .quarantine
        .get("1")
        .and_then(|record| record.entry())
        .expect("account 1 is quarantined");
    assert_eq!(entry.reason, "identity-conflict");
    assert_eq!(entry.email, ONE);
    let successor = CredentialBlob::new(rig.stored().expect("the successor is stored"));
    assert_eq!(
        entry.refresh_token_fingerprint,
        credential_fingerprint(&successor),
        "the quarantine binds to the stored credential, so replacing it releases the quarantine"
    );
}

#[test]
fn test_same_uuid_different_org_is_identity_conflict() {
    let endpoint = granting_as(
        json!({ "uuid": ONE_UUID, "email_address": ONE }),
        json!({ "uuid": "org-other" }),
    );
    let rig = expiring_rig("freshen_org_conflict", endpoint);

    let freshened =
        freshen_target(&rig.context(), &account_one(&rig)).expect("the quarantine is written");

    assert_eq!(freshened, Freshened::IdentityConflict);
    assert_eq!(
        quarantined_reason(&rig, "1").as_deref(),
        Some("identity-conflict")
    );
}

#[test]
fn test_malformed_token_identity_never_breaks_freshen() {
    let endpoint = granting_as(
        json!({ "uuid": 12345, "email_address": ["weird"] }),
        json!("not an object"),
    );
    let rig = expiring_rig("freshen_malformed_identity", endpoint);

    let freshened =
        freshen_target(&rig.context(), &account_one(&rig)).expect("no quarantine write was needed");

    assert_eq!(freshened, Freshened::Ready);
    assert_eq!(stored_access(&rig).as_deref(), Some("fake-access-new"));
    assert_eq!(account_one(&rig).uuid, ONE_UUID);
    assert_eq!(quarantined_reason(&rig, "1"), None);
}

#[test]
fn test_freshen_calls_consume_gate() {
    // Inside the 10-minute freshen buffer but outside Claude Code's 5-minute one: still spent.
    let scratch = Scratch::new("freshen_consume_gate");
    scratch.set_active(2);
    let rig = Rig::new(
        scratch,
        FakeEndpoint::answering(vec![granted(
            "fake-access-new",
            Some("fake-refresh-new"),
            3600,
        )]),
    );
    put_credentials(
        &rig.scratch.root(),
        &credential("fake-access-one", Some("fake-refresh-one"), ms_after(540)),
    );
    write_config_backup(&rig, ONE, ONE_ORG);

    let freshened =
        freshen_target(&rig.context(), &account_one(&rig)).expect("no quarantine write was needed");

    assert_eq!(freshened, Freshened::Ready);
    assert_eq!(
        rig.endpoint.posted_refresh_tokens(),
        vec!["fake-refresh-one".to_owned()]
    );
    assert_eq!(stored_access(&rig).as_deref(), Some("fake-access-new"));

    // The gate's refusal of an unmirrored store shows the freshen POSTs nowhere but through the gate.
    let mut rig = expiring_rig(
        "freshen_consume_gate_refuses",
        FakeEndpoint::answering(Vec::new()),
    );
    rig.securestorage_dir_set = true;
    let freshened =
        freshen_target(&rig.context(), &account_one(&rig)).expect("no quarantine write was needed");
    assert_eq!(freshened, Freshened::Failed(GrantError::StoreUnmirrored));
    assert_eq!(freshened.kind(), "store-unmirrored");
    assert!(
        rig.endpoint.bodies().is_empty(),
        "nothing was POSTed outside the gate"
    );
}

#[test]
fn test_replaced_credentials_lift_quarantine() {
    let rig = expiring_rig(
        "freshen_release_replaced",
        FakeEndpoint::answering(Vec::new()),
    );
    quarantine(&rig.store, 1, &email(), "invalid_grant", now()).expect("the quarantine is written");

    let state = AutoswitchState::read(&rig.scratch.root()).expect("the state file can be read");
    let (state, released) =
        release_recovered_quarantines(&rig.store, state).expect("nothing to release");
    assert!(
        released.is_empty(),
        "an unchanged credential stays quarantined"
    );
    assert!(state.quarantine.contains_key("1"));

    put_credentials(
        &rig.scratch.root(),
        &credential("fake-access-two", Some("fake-refresh-two"), ms_after(3600)),
    );
    let (state, released) =
        release_recovered_quarantines(&rig.store, state).expect("the release is written");

    assert_eq!(released.len(), 1);
    assert_eq!(released[0].number, "1");
    assert_eq!(released[0].email, ONE);
    assert_eq!(released[0].reason, ReleaseReason::CredentialsReplaced);
    assert_eq!(released[0].reason.kind(), "credentials-replaced");
    assert!(!state.quarantine.contains_key("1"));
    assert_eq!(
        quarantined_reason(&rig, "1"),
        None,
        "the release is written to the state file"
    );
}

#[test]
fn test_blank_uuid_slot_with_org_conflict_quarantines_not_backfills() {
    let endpoint = granting_as(
        json!({ "uuid": "uuid-real", "email_address": "z@example.test" }),
        json!({ "uuid": "org-B" }),
    );
    let rig = expiring_rig("freshen_blank_uuid_org_conflict", endpoint);
    rig.scratch.set_account(1, "uuid", json!(""));
    rig.scratch.set_account(1, "orgUuid", json!("org-A"));
    // A record whose organization changed no longer opens the credential sealed under the old one: it is re-added.
    fs::remove_file(rig.scratch.credential_file())
        .expect("the credential sealed under the old organization is removed");
    put_credentials(
        &rig.scratch.root(),
        &credential("fake-access-one", Some("fake-refresh-one"), 0),
    );
    write_config_backup(&rig, ONE, "org-A");

    let freshened =
        freshen_target(&rig.context(), &account_one(&rig)).expect("the quarantine is written");

    assert_eq!(freshened, Freshened::IdentityConflict);
    assert_eq!(
        account_one(&rig).uuid,
        "",
        "the foreign uuid is not backfilled"
    );
    assert_eq!(
        stored_access(&rig).as_deref(),
        Some("fake-access-new"),
        "the successor is persisted"
    );
}

#[test]
fn a_padded_record_uuid_matching_the_token_is_no_conflict() {
    let endpoint = granting_as(
        json!({ "uuid": ONE_UUID, "email_address": ONE }),
        json!({ "uuid": ONE_ORG }),
    );
    let rig = expiring_rig("freshen_padded_uuid", endpoint);
    rig.scratch
        .set_account(1, "uuid", json!(format!("  {ONE_UUID} ")));

    let freshened =
        freshen_target(&rig.context(), &account_one(&rig)).expect("no quarantine write was needed");

    assert_eq!(
        freshened,
        Freshened::Ready,
        "both uuids are compared trimmed, as cswap strips them"
    );
    assert_eq!(quarantined_reason(&rig, "1"), None);
}

#[test]
fn freshen_reads_an_empty_oauth_block_as_a_dead_lineage() {
    let rig = expiring_rig("freshen_empty_oauth", FakeEndpoint::answering(Vec::new()));
    put_credentials(
        &rig.scratch.root(),
        &json!({ "claudeAiOauth": {} }).to_string(),
    );

    let freshened =
        freshen_target(&rig.context(), &account_one(&rig)).expect("no quarantine write was needed");

    assert_eq!(freshened, Freshened::Failed(GrantError::NoRefreshToken));
    assert_eq!(freshened.kind(), "invalid_grant");
    assert!(
        rig.endpoint.bodies().is_empty(),
        "a credential with no OAuth content has no grant to spend"
    );
}

#[test]
fn freshen_judges_the_identity_against_the_record_after_the_grant() {
    let scratch = Scratch::new("freshen_record_after_grant");
    scratch.set_active(2);
    scratch.set_account(1, "uuid", json!(""));
    let sequence = scratch.backup().join("accounts.json");
    let endpoint = granting_as(
        json!({ "uuid": "uuid-1-real", "email_address": ONE }),
        json!({ "uuid": "" }),
    )
    .during_post(move || {
        // Another writer backfills the record while the grant is in flight.
        let mut value: Value =
            serde_json::from_slice(&std::fs::read(&sequence).expect("accounts.json can be read"))
                .expect("it is JSON");
        value["accounts"]["1"]["uuid"] = json!("uuid-somebody-else");
        std::fs::write(&sequence, value.to_string()).expect("accounts.json can be written");
    });
    let rig = Rig::new(scratch, endpoint);
    put_credentials(
        &rig.scratch.root(),
        &credential("fake-access-one", Some("fake-refresh-one"), 0),
    );
    write_config_backup(&rig, ONE, ONE_ORG);
    let before = account_one(&rig);
    assert_eq!(before.uuid, "", "the caller's record has no uuid");

    let freshened = freshen_target(&rig.context(), &before).expect("the quarantine is written");

    assert_eq!(
        freshened,
        Freshened::IdentityConflict,
        "the record as it stands after the grant is the one judged"
    );
    assert_eq!(account_one(&rig).uuid, "uuid-somebody-else");
}

#[test]
fn test_replaced_account_lifts_quarantine() {
    let rig = expiring_rig(
        "freshen_release_account_replaced",
        FakeEndpoint::answering(Vec::new()),
    );
    quarantine(&rig.store, 1, &email(), "invalid_grant", now()).expect("the quarantine is written");
    rig.scratch
        .set_account(1, "email", json!("other@example.test"));

    let state = AutoswitchState::read(&rig.scratch.root()).expect("the state file can be read");
    let (state, released) =
        release_recovered_quarantines(&rig.store, state).expect("the release is written");

    assert_eq!(released.len(), 1);
    assert_eq!(released[0].number, "1");
    assert_eq!(
        released[0].email, ONE,
        "the entry's email names the account that was replaced"
    );
    assert_eq!(released[0].reason, ReleaseReason::AccountReplaced);
    assert_eq!(released[0].reason.kind(), "account-replaced");
    assert!(!state.quarantine.contains_key("1"));
}

#[test]
fn freshen_leaves_an_unexpiring_token_alone() {
    let rig = expiring_rig(
        "freshen_unexpiring",
        FakeEndpoint::answering(vec![granted(
            "fake-access-new",
            Some("fake-refresh-new"),
            3600,
        )]),
    );
    put_credentials(
        &rig.scratch.root(),
        &credential("fake-access-one", Some("fake-refresh-one"), ms_after(601)),
    );

    let freshened =
        freshen_target(&rig.context(), &account_one(&rig)).expect("no quarantine write was needed");

    assert_eq!(freshened, Freshened::Ready);
    assert_eq!(freshened.kind(), "ok");
    assert!(
        rig.endpoint.bodies().is_empty(),
        "a token outliving the buffer is not refreshed"
    );
    assert_eq!(stored_access(&rig).as_deref(), Some("fake-access-one"));
}
