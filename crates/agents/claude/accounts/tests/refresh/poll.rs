//! Polling with refresh: an inactive account's expired backup token is refreshed through the consume gate before its
//! fetch, a 401 refreshes once and retries once, and a dead lineage strikes the generation it posted.

use std::collections::BTreeSet;
use std::fs;
use std::sync::Mutex;

use claude_accounts::AutoSwitchSettings;
use claude_accounts::{
    AccessToken, AccountLock, CredentialBlob, DeferReason, FetchError, FetchMode, FetchScope,
    PollOutcome, PollResult, RawUsage, Store, StoreErrorKind, TransportError, UsageTransport,
    credential_fingerprint, poll_once_with_refresh, poll_with_refresh,
};
use serde_json::{Value, json};

use crate::support::{FakeEndpoint, Rig, Scratch, answer, credential, granted, ms_after};

const TWO: &str = "two@example.test";

/// A usage endpoint that answers 401 to every token named in `refused` and a reading to any other, and remembers the
/// tokens it was sent. Its prepaid-credits endpoint answers 401 to every token named in `prepaid_refused` and a funded
/// balance to any other. They are fake tokens, so the test compares them directly.
struct FakeUsage {
    refused: &'static [&'static str],
    tokens: Mutex<Vec<String>>,
    prepaid_refused: &'static [&'static str],
    /// Every request in the order sent: the endpoint (`usage` or `prepaid`) and the token it carried.
    calls: Mutex<Vec<(&'static str, String)>>,
}

impl FakeUsage {
    fn refusing(refused: &'static [&'static str]) -> Self {
        Self {
            refused,
            tokens: Mutex::new(Vec::new()),
            prepaid_refused: &[],
            calls: Mutex::new(Vec::new()),
        }
    }

    /// The same endpoints, the balance request refusing the tokens in `prepaid_refused` with a 401.
    fn refusing_prepaid(self, prepaid_refused: &'static [&'static str]) -> Self {
        Self {
            prepaid_refused,
            ..self
        }
    }

    /// The tokens sent for account 2, whose fake tokens all contain `two` or `new`.
    fn sent_for_two(&self) -> Vec<String> {
        let tokens = self
            .tokens
            .lock()
            .expect("no test thread panics holding the fake's lock");
        tokens
            .iter()
            .filter(|token| !token.contains("-one"))
            .cloned()
            .collect()
    }

    /// Every usage and balance request sent for account 2, in order, as the endpoint and the token.
    fn calls_for_two(&self) -> Vec<(&'static str, String)> {
        let calls = self
            .calls
            .lock()
            .expect("no test thread panics holding the fake's lock");
        calls
            .iter()
            .filter(|(_, token)| !token.contains("-one"))
            .cloned()
            .collect()
    }
}

impl UsageTransport for FakeUsage {
    fn get_usage(&self, token: &AccessToken) -> Result<RawUsage, TransportError> {
        let token = token.expose().to_owned();
        let refused = self.refused.contains(&token.as_str());
        self.calls
            .lock()
            .expect("no test thread panics holding the fake's lock")
            .push(("usage", token.clone()));
        self.tokens
            .lock()
            .expect("no test thread panics holding the fake's lock")
            .push(token);
        if refused {
            return Ok(RawUsage {
                status: 401,
                body: Value::Null,
                retry_after: None,
            });
        }
        let body = json!({
            "five_hour": { "utilization": 10.0, "resets_at": "2026-09-21T18:00:00Z" },
            "seven_day": { "utilization": 20.0, "resets_at": "2026-09-25T00:00:00Z" },
        });
        Ok(RawUsage {
            status: 200,
            body,
            retry_after: None,
        })
    }

    fn get_prepaid(
        &self,
        token: &AccessToken,
        _org_uuid: &str,
    ) -> Result<RawUsage, TransportError> {
        let token = token.expose().to_owned();
        let refused = self.prepaid_refused.contains(&token.as_str());
        self.calls
            .lock()
            .expect("no test thread panics holding the fake's lock")
            .push(("prepaid", token));
        let (status, body) = if refused {
            (401, Value::Null)
        } else {
            (
                200,
                json!({ "amount": 102_003, "currency": "USD", "balance": { "credits": { "amount_minor": 102_003, "exponent": 2 } } }),
            )
        };
        Ok(RawUsage {
            status,
            body,
            retry_after: None,
        })
    }
}

/// Account 2's organization in the rigs that give it one ([`rig_in_org`]).
const TWO_ORG: &str = "00000000-0000-4000-8000-0000000000b2";

fn expired_two() -> String {
    credential("fake-access-two", Some("fake-refresh-two"), ms_after(-3600))
}

fn fresh_two() -> String {
    credential("fake-access-two", Some("fake-refresh-two"), ms_after(3600))
}

fn fingerprint(text: &str) -> Option<String> {
    credential_fingerprint(&CredentialBlob::new(text.to_owned()))
}

/// A rig whose account 2 stores `stored` and whose usage table starts empty, so every account is due.
fn rig(test: &str, stored: &str, endpoint: FakeEndpoint) -> Rig {
    let scratch = Scratch::new(test);
    put_two(&scratch, stored);
    fs::remove_file(scratch.backup().join("cache").join("usage.json"))
        .expect("the fixture's usage table can be removed");
    Rig::new(scratch, endpoint)
}

/// [`rig`], with account 2 in organization [`TWO_ORG`], so its prepaid balance is read beside its usage.
fn rig_in_org(test: &str, stored: &str, endpoint: FakeEndpoint) -> Rig {
    let scratch = Scratch::new(test);
    scratch.set_account(2, "orgUuid", json!(TWO_ORG));
    // The fixture's credential was sealed for no organization, so it no longer opens: write the account's afresh.
    fs::remove_file(scratch.root().credential_path(2))
        .expect("account 2's fixture credential can be removed");
    put_two(&scratch, stored);
    fs::remove_file(scratch.backup().join("cache").join("usage.json"))
        .expect("the fixture's usage table can be removed");
    Rig::new(scratch, endpoint)
}

#[test]
fn refreshing_fetch_reads_the_balance_after_its_first_fetch() {
    let endpoint = FakeEndpoint::answering(vec![granted(
        "fake-access-new",
        Some("fake-refresh-new"),
        3600,
    )]);
    let rig = rig_in_org("poll_prepaid_refreshed", &expired_two(), endpoint);
    let usage = FakeUsage::refusing(&[]);

    let result = poll(&rig, &usage, 2);

    assert!(
        matches!(result, PollResult::Fetched { .. }),
        "got {result:?}"
    );
    assert_eq!(
        usage.calls_for_two(),
        [
            ("usage", "fake-access-new".to_owned()),
            ("prepaid", "fake-access-new".to_owned())
        ],
        "the balance is read with the refreshed token, right after the usage read it served"
    );
    assert_eq!(usage_row(&rig, 2)["prepaid"]["amountMinor"], json!(102_003));
}

#[test]
fn retry_after_401_reads_the_balance_with_the_successor_token() {
    let endpoint = FakeEndpoint::answering(vec![granted(
        "fake-access-new",
        Some("fake-refresh-new"),
        3600,
    )]);
    let rig = rig_in_org("poll_prepaid_retry", &fresh_two(), endpoint);
    let usage = FakeUsage::refusing(&["fake-access-two"]);

    let result = poll(&rig, &usage, 2);

    assert!(
        matches!(result, PollResult::Fetched { .. }),
        "got {result:?}"
    );
    assert_eq!(
        usage.calls_for_two(),
        [
            ("usage", "fake-access-two".to_owned()),
            ("usage", "fake-access-new".to_owned()),
            ("prepaid", "fake-access-new".to_owned()),
        ],
        "no balance request beside the refused read; one with the successor after the retry"
    );
    assert_eq!(usage_row(&rig, 2)["prepaid"]["amountMinor"], json!(102_003));
}

#[test]
fn inactive_balance_401_spends_no_grant() {
    let endpoint = FakeEndpoint::answering(vec![granted(
        "fake-access-new",
        Some("fake-refresh-new"),
        3600,
    )]);
    let rig = rig_in_org("poll_prepaid_401", &fresh_two(), endpoint);
    let usage = FakeUsage::refusing(&[]).refusing_prepaid(&["fake-access-two"]);

    let result = poll(&rig, &usage, 2);

    assert!(
        matches!(result, PollResult::Fetched { .. }),
        "got {result:?}"
    );
    assert!(
        rig.endpoint.bodies().is_empty(),
        "a refused balance read spends no grant"
    );
    assert_eq!(
        usage.calls_for_two(),
        [
            ("usage", "fake-access-two".to_owned()),
            ("prepaid", "fake-access-two".to_owned())
        ],
        "nor is anything retried"
    );
    let row = usage_row(&rig, 2);
    assert_eq!(row["prepaid"]["error"], json!("http-401"), "{row}");
    assert_eq!(
        row["lastError"],
        Value::Null,
        "the usage row's failure state is untouched"
    );
    assert_eq!(
        access_two(&rig),
        "fake-access-two",
        "the stored token is untouched"
    );
}

fn put_two(scratch: &Scratch, text: &str) {
    let store = Store::open(scratch.root()).expect("the scratch store opens");
    let lock = AccountLock::acquire(&scratch.root()).expect("the account lock is free");
    store
        .write_credentials(&lock, 2, &CredentialBlob::new(text.to_owned()))
        .expect("the credential can be written");
}

fn stored_two(rig: &Rig) -> String {
    let blob = rig
        .store
        .read_credentials(2)
        .expect("the credential can be read");
    blob.expect("account 2 holds a credential")
        .expose()
        .to_owned()
}

/// Polls once with refresh and returns account `number`'s result.
fn poll(rig: &Rig, usage: &FakeUsage, number: u32) -> PollResult {
    let outcomes = poll_once_with_refresh(usage, &AutoSwitchSettings::default(), &rig.context())
        .expect("the poll runs");
    outcomes
        .into_iter()
        .find(|outcome: &PollOutcome| outcome.number == number)
        .expect("the account has an outcome")
        .result
}

fn usage_row(rig: &Rig, number: u32) -> Value {
    let bytes = fs::read(rig.scratch.backup().join("cache").join("usage.json"))
        .expect("the usage table can be read");
    let table: Value = serde_json::from_slice(&bytes).expect("the usage table is JSON");
    table["accounts"][number.to_string()].clone()
}

fn access_two(rig: &Rig) -> String {
    crate::support::access_of(&stored_two(rig))
}

#[test]
fn expired_backup_token_refreshes_before_usage() {
    let endpoint = FakeEndpoint::answering(vec![granted(
        "fake-access-new",
        Some("fake-refresh-new"),
        3600,
    )]);
    let rig = rig("poll_expired", &expired_two(), endpoint);
    let usage = FakeUsage::refusing(&[]);

    let result = poll(&rig, &usage, 2);

    assert!(
        matches!(result, PollResult::Fetched { .. }),
        "got {result:?}"
    );
    assert_eq!(rig.endpoint.posted_refresh_tokens(), ["fake-refresh-two"]);
    assert_eq!(
        usage.sent_for_two(),
        ["fake-access-new"],
        "the expired token is never sent"
    );
    assert_eq!(
        access_two(&rig),
        "fake-access-new",
        "the successor is stored"
    );
}

#[test]
fn an_unopenable_backup_is_unreadable_not_absent() {
    let endpoint = FakeEndpoint::answering(Vec::new());
    let rig = rig("poll_backup_unreadable", &expired_two(), endpoint);
    let path = rig.scratch.root().credential_path(2);
    let mut bytes = fs::read(&path).expect("account 2's backup can be read");
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    fs::write(&path, &bytes).expect("account 2's backup can be damaged");

    let result = poll(&rig, &FakeUsage::refusing(&[]), 2);

    assert_eq!(
        result,
        PollResult::CredentialsUnreadable(StoreErrorKind::CredentialsUnopenable),
        "a file that exists and does not open is not a missing one"
    );
    assert!(
        rig.endpoint.bodies().is_empty(),
        "nothing is posted for a backup that cannot be opened"
    );
}

#[test]
fn usage_401_refreshes_once_and_retries_once() {
    let endpoint = FakeEndpoint::answering(vec![granted(
        "fake-access-new",
        Some("fake-refresh-new"),
        3600,
    )]);
    let rig = rig("poll_401_retry", &fresh_two(), endpoint);
    let usage = FakeUsage::refusing(&["fake-access-two"]);

    let result = poll(&rig, &usage, 2);

    assert!(
        matches!(result, PollResult::Fetched { .. }),
        "got {result:?}"
    );
    assert_eq!(
        rig.endpoint.posted_refresh_tokens(),
        ["fake-refresh-two"],
        "one refresh"
    );
    assert_eq!(
        usage.sent_for_two(),
        ["fake-access-two", "fake-access-new"],
        "one retry with the new token"
    );
}

#[test]
fn second_401_after_refresh_is_not_retried_again() {
    let endpoint = FakeEndpoint::answering(vec![granted(
        "fake-access-new",
        Some("fake-refresh-new"),
        3600,
    )]);
    let rig = rig("poll_second_401", &fresh_two(), endpoint);
    let usage = FakeUsage::refusing(&["fake-access-two", "fake-access-new"]);

    let result = poll(&rig, &usage, 2);

    assert_eq!(
        result,
        PollResult::Failed(FetchError::Http {
            status: 401,
            retry_after: None
        })
    );
    assert_eq!(rig.endpoint.posted_refresh_tokens().len(), 1, "one refresh");
    assert_eq!(usage.sent_for_two().len(), 2, "one retry, no more");
    assert_eq!(usage_row(&rig, 2)["lastError"], "http-401");
}

#[test]
fn invalid_grant_writes_a_strike_with_the_posted_fingerprint() {
    let endpoint = FakeEndpoint::answering(vec![answer(400, json!({ "error": "invalid_grant" }))]);
    let rig = rig("poll_invalid_grant", &expired_two(), endpoint);
    let usage = FakeUsage::refusing(&[]);

    let result = poll(&rig, &usage, 2);

    assert_eq!(result, PollResult::AuthDead);
    assert!(
        usage.sent_for_two().is_empty(),
        "a dead lineage's expired token is never sent"
    );
    let row = usage_row(&rig, 2);
    assert_eq!(row["authDeadStrikes"], 1);
    assert_eq!(row["struckFingerprint"], json!(fingerprint(&expired_two())));
    assert_eq!(row["lastError"], "invalid_grant");
    assert_eq!(row["consecutiveFailures"], 1);

    let again = poll(&rig, &usage, 2);
    assert_eq!(
        again,
        PollResult::AuthDead,
        "the struck generation stays quarantined"
    );
    assert_eq!(
        rig.endpoint.posted_refresh_tokens().len(),
        1,
        "a quarantined account is not refreshed again"
    );
}

#[test]
fn strike_lapses_when_stored_fingerprint_changes() {
    let endpoint = FakeEndpoint::answering(vec![answer(400, json!({ "error": "invalid_grant" }))]);
    let rig = rig("poll_strike_lapses", &expired_two(), endpoint);
    let usage = FakeUsage::refusing(&[]);
    assert_eq!(poll(&rig, &usage, 2), PollResult::AuthDead);

    let relogin = credential(
        "fake-access-two-relogin",
        Some("fake-refresh-two-relogin"),
        ms_after(3600),
    );
    put_two(&rig.scratch, &relogin);
    // The strike's failure backoff has lifted by the next pass; the test clock stands still, so lift it by hand.
    let usage_json = rig.scratch.backup().join("cache").join("usage.json");
    let mut table: Value =
        serde_json::from_slice(&fs::read(&usage_json).expect("the usage table can be read"))
            .expect("it is JSON");
    table["accounts"]["2"]["backoffUntil"] = Value::Null;
    fs::write(&usage_json, table.to_string()).expect("the usage table can be written");
    assert_eq!(
        usage_row(&rig, 2)["authDeadStrikes"],
        1,
        "the strike is still on the row"
    );
    let result = poll(&rig, &usage, 2);

    assert!(
        matches!(result, PollResult::Fetched { .. }),
        "a new generation lifts the strike: {result:?}"
    );
    assert_eq!(usage.sent_for_two(), ["fake-access-two-relogin"]);
    assert_eq!(
        usage_row(&rig, 2)["authDeadStrikes"],
        0,
        "a success clears the strike"
    );
}

/// A store that refuses writes refuses the inactive spend before the gate's POST, and the poll puts the account off: its
/// expired token is never sent, no token is posted, and its row gains no failure and no backoff.
#[test]
fn a_store_refusing_writes_defers_the_expired_inactive_account_without_a_request() {
    let endpoint = FakeEndpoint::answering(vec![granted(
        "fake-access-new",
        Some("fake-refresh-new"),
        3600,
    )]);
    let mut rig = rig("poll_store_unwritable", &expired_two(), endpoint);
    rig.store =
        Store::open_with_canary(rig.scratch.root(), || false).expect("the scratch store opens");
    let usage = FakeUsage::refusing(&["fake-access-two"]);

    let result = poll(&rig, &usage, 2);

    assert_eq!(result, PollResult::Deferred(DeferReason::StoreUnwritable));
    assert!(rig.endpoint.bodies().is_empty(), "no token was posted");
    assert!(
        usage.sent_for_two().is_empty(),
        "no usage request was sent for the account"
    );
    let row = usage_row(&rig, 2);
    assert_eq!(
        row.get("consecutiveFailures"),
        None,
        "no failure is counted"
    );
    assert_eq!(row.get("backoffUntil"), None, "no backoff is set");
    assert_eq!(row.get("lastError"), None, "no failure is recorded");
    assert_eq!(row.get("authDeadStrikes"), None, "nothing strikes");
}

/// A stashed successor of the expired generation is adopted before the gate's own write check, and a store refusing
/// writes refuses that adoption: the account is put off as for any refused write, with no request and no backoff.
#[test]
fn a_store_refusing_the_adoption_of_a_stashed_successor_defers_without_a_request() {
    let endpoint = FakeEndpoint::answering(vec![granted(
        "fake-access-new",
        Some("fake-refresh-new"),
        3600,
    )]);
    let mut rig = rig("poll_store_unwritable_adopt", &expired_two(), endpoint);
    let successor = credential(
        "fake-access-stashed",
        Some("fake-refresh-stashed"),
        ms_after(3600),
    );
    let id = "20260921T140000-abcdefabcdef-a1b2c3";
    rig.scratch.write_stash_entry_for(id, &successor, "2", "");
    let row = json!({
        "createdAt": "2026-09-21T14:00:00Z",
        "reason": "consume-gate-persist-failed",
        "configSlot": "2",
        "consumedFp": fingerprint(&expired_two()),
        "fingerprint": fingerprint(&successor),
        "organization": "",
    });
    let manifest = json!({ "schemaVersion": 1, "entries": { id: row } });
    fs::write(rig.scratch.manifest_path(), manifest.to_string())
        .expect("the manifest can be written");
    rig.store =
        Store::open_with_canary(rig.scratch.root(), || false).expect("the scratch store opens");
    let usage = FakeUsage::refusing(&["fake-access-two"]);

    let result = poll(&rig, &usage, 2);

    assert_eq!(result, PollResult::Deferred(DeferReason::StoreUnwritable));
    assert!(rig.endpoint.bodies().is_empty(), "no token was posted");
    assert!(
        usage.sent_for_two().is_empty(),
        "no usage request was sent for the account"
    );
    let row_now = usage_row(&rig, 2);
    assert_eq!(
        row_now.get("consecutiveFailures"),
        None,
        "no failure is counted"
    );
    assert_eq!(row_now.get("backoffUntil"), None, "no backoff is set");
    assert_eq!(
        rig.scratch.manifest()["entries"][id],
        row,
        "the stash row stays for a later pass"
    );
}

/// After an unexpired token's 401, a store refusing writes leaves the 401 as the result, as the logged-in account's
/// path does: no token is posted and nothing strikes.
#[test]
fn a_store_refusing_writes_after_a_401_keeps_the_401() {
    let endpoint = FakeEndpoint::answering(vec![granted(
        "fake-access-new",
        Some("fake-refresh-new"),
        3600,
    )]);
    let mut rig = rig("poll_store_unwritable_after_401", &fresh_two(), endpoint);
    rig.store =
        Store::open_with_canary(rig.scratch.root(), || false).expect("the scratch store opens");
    let usage = FakeUsage::refusing(&["fake-access-two"]);

    let result = poll(&rig, &usage, 2);

    assert_eq!(
        result,
        PollResult::Failed(FetchError::Http {
            status: 401,
            retry_after: None
        })
    );
    assert!(rig.endpoint.bodies().is_empty(), "no token was posted");
    assert_eq!(
        usage.sent_for_two(),
        ["fake-access-two"],
        "the one fetch, not retried"
    );
    assert_eq!(
        usage_row(&rig, 2).get("authDeadStrikes"),
        None,
        "nothing strikes"
    );
}

/// A degenerate grant kept for its new refresh token leaves an empty access token behind, here with the legacy
/// `expiresAt` 0 that older stores left (a grant kept now gets a stand-in expiry): the slot is refreshed on the next
/// poll, not shown as having no credentials.
#[test]
fn slot_with_an_empty_access_token_is_refreshed_not_no_credentials() {
    let endpoint = FakeEndpoint::answering(vec![granted(
        "fake-access-new",
        Some("fake-refresh-new"),
        3600,
    )]);
    let rig = rig(
        "poll_empty_access",
        &credential("", Some("fake-refresh-two"), 0),
        endpoint,
    );
    let usage = FakeUsage::refusing(&[]);

    let result = poll(&rig, &usage, 2);

    assert!(
        matches!(result, PollResult::Fetched { .. }),
        "got {result:?}"
    );
    assert_eq!(rig.endpoint.posted_refresh_tokens(), ["fake-refresh-two"]);
    assert_eq!(
        usage.sent_for_two(),
        ["fake-access-new"],
        "the empty token is never sent"
    );
    assert_eq!(
        access_two(&rig),
        "fake-access-new",
        "the successor is stored"
    );
}

#[test]
fn active_account_is_never_refreshed_here() {
    let endpoint = FakeEndpoint::answering(vec![granted(
        "fake-access-new",
        Some("fake-refresh-new"),
        3600,
    )]);
    let rig = rig("poll_active", &expired_two(), endpoint);
    let expired_one = credential("fake-access-one", Some("fake-refresh-one"), ms_after(-3600));
    crate::support::put_credentials(&rig.scratch.root(), &expired_one);
    let identity = json!({ "oauthAccount": { "emailAddress": TWO } });
    fs::write(rig.scratch.dir.join(".claude.json"), identity.to_string())
        .expect("the global config can be written");
    let usage = FakeUsage::refusing(&[]);

    assert_eq!(
        poll(&rig, &usage, 1),
        PollResult::TokenExpired,
        "accounts.json's active account"
    );
    assert_eq!(
        poll(&rig, &usage, 2),
        PollResult::TokenExpired,
        "the account Claude Code is logged in as"
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
}

/// The tests' clock in seconds since the epoch, as the usage table writes times.
const NOW: f64 = 1_790_000_000.0;

fn secs(offset: i32) -> f64 {
    NOW + f64::from(offset)
}

/// A rig whose accounts 1 and 2 both store fresh credentials: account 1 is `accounts.json`'s active account, fetched
/// read-only, and account 2 an inactive one. Account 1's fixture row was fetched at the tests' clock, so it is never due.
fn scope_rig(test: &str) -> Rig {
    let scratch = Scratch::new(test);
    put_two(&scratch, &fresh_two());
    crate::support::put_credentials(
        &scratch.root(),
        &credential("fake-access-one", Some("fake-refresh-one"), ms_after(3600)),
    );
    Rig::new(scratch, FakeEndpoint::answering(Vec::new()))
}

/// Replaces account 2's usage row with its identity, no lease, and `fields`.
fn set_row_two(rig: &Rig, fields: &Value) {
    let path = rig.scratch.backup().join("cache").join("usage.json");
    let mut table: Value =
        serde_json::from_slice(&fs::read(&path).expect("the usage table can be read"))
            .expect("it is JSON");
    let mut row =
        json!({ "email": TWO, "organizationUuid": "", "claimId": null, "claimUntil": 0.0 });
    for (key, value) in fields.as_object().expect("the fields are an object") {
        row[key] = value.clone();
    }
    table["accounts"]["2"] = row;
    fs::write(&path, table.to_string()).expect("the usage table can be written");
}

/// Polls once with refresh within `scope` and returns account `number`'s result.
fn poll_in(rig: &Rig, usage: &FakeUsage, scope: FetchScope<'_>, number: u32) -> PollResult {
    let outcomes = poll_with_refresh(usage, &AutoSwitchSettings::default(), &rig.context(), scope)
        .expect("the poll runs");
    outcomes
        .into_iter()
        .find(|outcome| outcome.number == number)
        .expect("the account has an outcome")
        .result
}

fn only_two(mode: FetchMode) -> (BTreeSet<u32>, FetchMode) {
    (BTreeSet::from([2]), mode)
}

fn fetched(result: &PollResult) -> bool {
    matches!(result, PollResult::Fetched { .. })
}

#[test]
fn respect_plans_waits_for_next_poll() {
    let rig = scope_rig("scope_respect_plans");
    let usage = FakeUsage::refusing(&[]);
    set_row_two(
        &rig,
        &json!({ "fetchedAt": secs(-1000), "nextPollAt": secs(300), "pollIntervalS": 300.0 }),
    );

    assert_eq!(
        poll_in(&rig, &usage, FetchScope::OnDemand, 2),
        PollResult::NotDue,
        "a stale row waits for its plan"
    );

    set_row_two(
        &rig,
        &json!({ "fetchedAt": secs(-1000), "nextPollAt": secs(-1), "pollIntervalS": 300.0 }),
    );
    let result = poll_in(&rig, &usage, FetchScope::OnDemand, 2);
    assert!(
        fetched(&result),
        "a stale row past its plan is fetched: {result:?}"
    );
    assert_eq!(usage.sent_for_two(), ["fake-access-two"]);
}

#[test]
fn overslept_plan_is_repaired_but_a_valid_plan_written_since_is_kept() {
    let rig = scope_rig("scope_overslept_repair");
    let usage = FakeUsage::refusing(&[]);
    set_row_two(
        &rig,
        &json!({ "fetchedAt": secs(-1000), "nextPollAt": secs(86_400), "pollIntervalS": 300.0 }),
    );

    let result = poll_in(&rig, &usage, FetchScope::OnDemand, 2);
    assert!(
        fetched(&result),
        "a plan no planner could have written is repaired: {result:?}"
    );

    set_row_two(
        &rig,
        &json!({ "fetchedAt": secs(-181), "nextPollAt": secs(300), "pollIntervalS": 300.0 }),
    );
    let (accounts, mode) = only_two(FetchMode::Scheduled);
    assert_eq!(
        poll_in(
            &rig,
            &usage,
            FetchScope::Only {
                accounts: &accounts,
                mode
            },
            2
        ),
        PollResult::NotDue,
        "a valid plan written since is not bypassed"
    );
    assert_eq!(usage.sent_for_two(), ["fake-access-two"]);
}

#[test]
fn reserve_force_ignores_ttl_and_plan() {
    let rig = scope_rig("scope_force_ignores_plan");
    let usage = FakeUsage::refusing(&[]);
    let inside_ttl =
        json!({ "fetchedAt": secs(-1), "nextPollAt": secs(599), "pollIntervalS": 600.0 });
    let accounts = BTreeSet::from([2]);

    for scope in [
        FetchScope::OnDemand,
        FetchScope::Only {
            accounts: &accounts,
            mode: FetchMode::Escalation,
        },
        FetchScope::Only {
            accounts: &accounts,
            mode: FetchMode::Scheduled,
        },
    ] {
        set_row_two(&rig, &inside_ttl);
        assert_eq!(
            poll_in(&rig, &usage, scope, 2),
            PollResult::NotDue,
            "{scope:?} keeps a fresh row with a future plan"
        );
    }
    set_row_two(&rig, &inside_ttl);
    let result = poll_in(
        &rig,
        &usage,
        FetchScope::Only {
            accounts: &accounts,
            mode: FetchMode::Force,
        },
        2,
    );

    assert!(fetched(&result), "force fetches it: {result:?}");
    assert_eq!(usage.sent_for_two(), ["fake-access-two"]);
}

#[test]
fn reserve_force_respects_backoff_held_dead_and_claims() {
    let rig = scope_rig("scope_force_respects");
    let usage = FakeUsage::refusing(&[]);
    let (accounts, mode) = only_two(FetchMode::Force);
    let force = FetchScope::Only {
        accounts: &accounts,
        mode,
    };

    let struck = json!({ "fetchedAt": secs(-5000), "authDeadStrikes": 1, "struckFingerprint": fingerprint(&fresh_two()) });
    set_row_two(&rig, &struck);
    assert_eq!(
        poll_in(&rig, &usage, force, 2),
        PollResult::AuthDead,
        "a strike refuses"
    );

    set_row_two(
        &rig,
        &json!({ "fetchedAt": secs(-5000), "claimId": "other", "claimUntil": secs(30) }),
    );
    assert_eq!(
        poll_in(&rig, &usage, force, 2),
        PollResult::NotDue,
        "a live lease refuses"
    );

    set_row_two(
        &rig,
        &json!({ "fetchedAt": secs(-5000), "consecutiveFailures": 1, "backoffUntil": secs(10) }),
    );
    assert_eq!(
        poll_in(&rig, &usage, force, 2),
        PollResult::NotDue,
        "a backoff refuses"
    );

    set_row_two(
        &rig,
        &json!({ "fetchedAt": secs(-5000), "heldUntil": secs(600) }),
    );
    assert_eq!(
        poll_in(&rig, &usage, force, 2),
        PollResult::NotDue,
        "a hold refuses"
    );
    assert!(usage.sent_for_two().is_empty(), "nothing was fetched");

    set_row_two(
        &rig,
        &json!({ "fetchedAt": secs(-5000), "claimId": "other", "claimUntil": secs(-1) }),
    );
    let result = poll_in(&rig, &usage, force, 2);
    assert!(
        fetched(&result),
        "a lapsed lease does not refuse: {result:?}"
    );
}

#[test]
fn every_mode_refuses_backoff_429_hold_strike_and_live_claim() {
    let rig = scope_rig("scope_every_mode_refuses");
    let usage = FakeUsage::refusing(&[]);
    let accounts = BTreeSet::from([2]);
    let scopes = [
        FetchScope::OnDemand,
        FetchScope::Only {
            accounts: &accounts,
            mode: FetchMode::Escalation,
        },
        FetchScope::Only {
            accounts: &accounts,
            mode: FetchMode::Scheduled,
        },
        FetchScope::Only {
            accounts: &accounts,
            mode: FetchMode::Force,
        },
    ];
    let due = json!({ "fetchedAt": secs(-5000), "nextPollAt": secs(-1), "pollIntervalS": 300.0 });
    let refusals = [
        (
            "a backoff",
            json!({ "consecutiveFailures": 1, "lastError": "timeout", "backoffUntil": secs(10) }),
        ),
        (
            "a 429's backoff",
            json!({ "consecutiveFailures": 1, "lastError": "http-429", "last429At": secs(-10), "backoffUntil": secs(500) }),
        ),
        ("a hold", json!({ "heldUntil": secs(600) })),
        (
            "a live lease",
            json!({ "claimId": "other", "claimUntil": secs(30) }),
        ),
    ];

    for scope in scopes {
        for (refusal, fields) in &refusals {
            let mut row = due.clone();
            for (key, value) in fields.as_object().expect("the fields are an object") {
                row[key] = value.clone();
            }
            set_row_two(&rig, &row);
            assert_eq!(
                poll_in(&rig, &usage, scope, 2),
                PollResult::NotDue,
                "{scope:?} refuses {refusal}"
            );
        }
        let mut struck = due.clone();
        struck["authDeadStrikes"] = json!(1);
        struck["struckFingerprint"] = json!(fingerprint(&fresh_two()));
        set_row_two(&rig, &struck);
        assert_eq!(
            poll_in(&rig, &usage, scope, 2),
            PollResult::AuthDead,
            "{scope:?} refuses a strike"
        );
    }
    assert!(usage.sent_for_two().is_empty(), "nothing was fetched");

    for scope in scopes {
        set_row_two(&rig, &due);
        let result = poll_in(&rig, &usage, scope, 2);
        assert!(
            fetched(&result),
            "{scope:?} fetches the same row once nothing refuses it: {result:?}"
        );
    }
}

#[test]
fn only_scope_never_replaces_a_row_outside_the_set() {
    let rig = scope_rig("scope_outside_row_kept");
    let usage = FakeUsage::refusing(&[]);
    set_row_two(
        &rig,
        &json!({ "email": "someone-else@example.test", "fetchedAt": secs(-1000) }),
    );
    let accounts = BTreeSet::from([1]);

    let result = poll_in(
        &rig,
        &usage,
        FetchScope::Only {
            accounts: &accounts,
            mode: FetchMode::Force,
        },
        2,
    );

    assert_eq!(result, PollResult::NotDue);
    let row = usage_row(&rig, 2);
    assert_eq!(
        row["email"], "someone-else@example.test",
        "the other account's row is kept"
    );
    assert_eq!(row["fetchedAt"], json!(secs(-1000)));
    assert_eq!(row["claimId"], Value::Null, "no lease was taken");
}

#[test]
fn only_scope_never_reserves_accounts_outside_the_set() {
    let rig = scope_rig("scope_outside_the_set");
    let usage = FakeUsage::refusing(&[]);
    set_row_two(
        &rig,
        &json!({ "fetchedAt": secs(-1000), "nextPollAt": secs(-1), "pollIntervalS": 300.0 }),
    );
    let accounts = BTreeSet::from([1]);

    let result = poll_in(
        &rig,
        &usage,
        FetchScope::Only {
            accounts: &accounts,
            mode: FetchMode::Force,
        },
        2,
    );

    assert_eq!(
        result,
        PollResult::NotDue,
        "a due account outside the set is reported, not fetched"
    );
    assert!(
        usage.sent_for_two().is_empty(),
        "nothing was fetched for account 2"
    );
    let row = usage_row(&rig, 2);
    assert_eq!(row["claimId"], Value::Null, "no lease was taken");
    assert_eq!(row.get("lastAttemptAt"), None, "no attempt was stamped");
}

#[test]
fn escalation_fetches_a_not_yet_due_stale_account() {
    let rig = scope_rig("scope_escalation");
    let usage = FakeUsage::refusing(&[]);
    let stale_with_plan =
        json!({ "fetchedAt": secs(-1000), "nextPollAt": secs(600), "pollIntervalS": 600.0 });
    set_row_two(&rig, &stale_with_plan);
    assert_eq!(
        poll_in(&rig, &usage, FetchScope::OnDemand, 2),
        PollResult::NotDue,
        "on demand waits for the plan"
    );

    let (accounts, mode) = only_two(FetchMode::Escalation);
    let result = poll_in(
        &rig,
        &usage,
        FetchScope::Only {
            accounts: &accounts,
            mode,
        },
        2,
    );

    assert!(
        fetched(&result),
        "escalation fetches the stale row: {result:?}"
    );
    assert_eq!(usage.sent_for_two(), ["fake-access-two"]);
}

#[test]
fn scheduled_keeps_a_valid_future_plan() {
    let rig = scope_rig("scope_scheduled");
    let usage = FakeUsage::refusing(&[]);
    let (accounts, mode) = only_two(FetchMode::Scheduled);
    let scheduled = FetchScope::Only {
        accounts: &accounts,
        mode,
    };
    set_row_two(
        &rig,
        &json!({ "fetchedAt": secs(-1000), "nextPollAt": secs(600), "pollIntervalS": 600.0 }),
    );

    assert_eq!(
        poll_in(&rig, &usage, scheduled, 2),
        PollResult::NotDue,
        "a stale row keeps its valid plan"
    );

    set_row_two(
        &rig,
        &json!({ "fetchedAt": secs(-60), "nextPollAt": secs(-1), "pollIntervalS": 60.0 }),
    );
    let result = poll_in(&rig, &usage, scheduled, 2);
    assert!(
        fetched(&result),
        "a due plan beats the serve TTL: {result:?}"
    );
    assert_eq!(usage.sent_for_two(), ["fake-access-two"]);
}

#[test]
fn expired_logged_in_account_left_unclaimed_reports_token_expired() {
    let rig = scope_rig("scope_expired_active");
    let identity = json!({ "oauthAccount": { "emailAddress": TWO } });
    fs::write(rig.scratch.dir.join(".claude.json"), identity.to_string())
        .expect("the global config can be written");
    fs::create_dir_all(rig.scratch.dir.join("claude")).expect("Claude Code's folder can be made");
    fs::write(rig.claude.credentials_file(), expired_two())
        .expect("the live credential can be written");
    let usage = FakeUsage::refusing(&[]);

    set_row_two(
        &rig,
        &json!({ "fetchedAt": secs(-1000), "consecutiveFailures": 1, "backoffUntil": secs(100) }),
    );
    assert_eq!(
        poll_in(&rig, &usage, FetchScope::OnDemand, 2),
        PollResult::TokenExpired,
        "an expired live token the backoff keeps unclaimed shows as expired"
    );

    set_row_two(
        &rig,
        &json!({ "fetchedAt": secs(-1000), "heldUntil": secs(600) }),
    );
    assert_eq!(
        poll_in(&rig, &usage, FetchScope::OnDemand, 2),
        PollResult::NotDue,
        "a held row waits on another machine's reading, not on the token"
    );
    assert!(rig.endpoint.bodies().is_empty(), "nothing was posted");
    assert!(
        usage.sent_for_two().is_empty(),
        "the expired token is never sent"
    );
}
