//! Polling the usage endpoint through a fake transport: reserve, fetch, record, and the shared budget's rules.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs;
#[cfg(windows)]
use std::fs::{File, OpenOptions};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
#[cfg(windows)]
use std::path::Path;

use claude_accounts::engine::URGENT_INTERVAL_S;
use claude_accounts::{
    AccessToken, AccountLock, AutoSwitchSettings, CredentialBlob, FetchError, PlanContext,
    PlanRequest, PollError, PollResult, RawUsage, StoreErrorKind, TransportError, UsageEntry,
    UsageTable, UsageTransport, plan_after_fetch, poll_once,
};
use serde_json::{Value, json};

use crate::support::{
    FETCHED_AT, ONE, ONE_ORG, Scratch, TWO, after_fetch, epoch_seconds, identities,
};

/// A fake endpoint's answer to the token labelled by its argument.
type Answer = fn(&str) -> Result<RawUsage, TransportError>;

/// A transport that answers each account by the label of its fake token and remembers which tokens it was sent, by
/// label. Tokens never leave it: the labels stand in for them in every assertion.
struct FakeTransport {
    answer: Answer,
    calls: RefCell<Vec<&'static str>>,
    /// The prepaid-credits endpoint's answer; with none, the balance request answers as the trait's default does.
    prepaid: Option<Answer>,
    prepaid_calls: RefCell<Vec<(&'static str, String)>>,
}

impl FakeTransport {
    fn new(answer: Answer) -> Self {
        Self {
            answer,
            calls: RefCell::new(Vec::new()),
            prepaid: None,
            prepaid_calls: RefCell::new(Vec::new()),
        }
    }

    /// A transport that also answers the prepaid-credits request with `prepaid`.
    fn with_prepaid(answer: Answer, prepaid: Answer) -> Self {
        Self {
            prepaid: Some(prepaid),
            ..Self::new(answer)
        }
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls.borrow().clone()
    }

    /// Each balance request, as the token's label and the organization UUID it named.
    fn prepaid_calls(&self) -> Vec<(&'static str, String)> {
        self.prepaid_calls.borrow().clone()
    }
}

fn label_of(token: &AccessToken) -> &'static str {
    match token.expose() {
        "fake-access-one" => "one",
        "fake-access-two" => "two",
        "fake-access-live" => "live",
        _ => "unknown",
    }
}

impl UsageTransport for FakeTransport {
    fn get_usage(&self, token: &AccessToken) -> Result<RawUsage, TransportError> {
        let label = label_of(token);
        self.calls.borrow_mut().push(label);
        (self.answer)(label)
    }

    fn get_prepaid(&self, token: &AccessToken, org_uuid: &str) -> Result<RawUsage, TransportError> {
        let Some(prepaid) = self.prepaid else {
            return Err(TransportError::Unsupported);
        };
        let label = label_of(token);
        self.prepaid_calls
            .borrow_mut()
            .push((label, org_uuid.to_owned()));
        prepaid(label)
    }
}

/// A transport that implements only the usage request, so the balance request is the trait's default.
struct UsageOnly(FakeTransport);

impl UsageTransport for UsageOnly {
    fn get_usage(&self, token: &AccessToken) -> Result<RawUsage, TransportError> {
        self.0.get_usage(token)
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "it is a fake transport's answer, which returns a Result"
)]
fn success(_label: &str) -> Result<RawUsage, TransportError> {
    Ok(RawUsage {
        status: 200,
        body: json!({
            "five_hour": {"utilization": 30.0, "resets_at": null},
            "seven_day": {"utilization": 12.0, "resets_at": null},
            "extra_usage": {"is_enabled": false},
        }),
        retry_after: None,
    })
}

fn rate_limited_one(label: &str) -> Result<RawUsage, TransportError> {
    if label == "one" {
        return Ok(RawUsage {
            status: 429,
            body: json!({"error": {"type": "rate_limit_error"}}),
            retry_after: Some(120.0),
        });
    }
    success(label)
}

/// An answer and the `lastGood` the poll makes of it, display strings left out.
fn scoped_spend_fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/scoped-spend.json"))
        .expect("the scoped-spend fixture is JSON")
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "it is a fake transport's answer, which returns a Result"
)]
fn scoped_spend(_label: &str) -> Result<RawUsage, TransportError> {
    Ok(RawUsage {
        status: 200,
        body: scoped_spend_fixture()["response"].clone(),
        retry_after: None,
    })
}

fn result_of(outcomes: &[claude_accounts::PollOutcome], number: u32) -> PollResult {
    outcomes
        .iter()
        .find(|outcome| outcome.number == number)
        .map(|outcome| outcome.result.clone())
        .expect("every account has an outcome")
}

fn seconds(row: &Value, key: &str) -> f64 {
    row[key]
        .as_f64()
        .expect("the row holds the time as seconds")
}

/// A valid live credential whose access token the fake transport labels `live`.
const LIVE_CREDENTIALS: &str = r#"{"claudeAiOauth":{"accessToken":"fake-access-live","refreshToken":"fake-refresh-live","expiresAt":4102444800000}}"#;

/// The fake secret the `~/.claude.json` fixture carries, which must never surface.
const CLAUDE_JSON_SECRET: &str = "sk-ant-api-fake";

/// A `~/.claude.json` whose `oauthAccount` names `email` in organization `org` (empty for none), with keys the reader
/// must skip, one of them secret-bearing.
fn claude_json(email: &str, org: &str) -> String {
    json!({
        "numStartups": 12,
        "primaryApiKey": CLAUDE_JSON_SECRET,
        "oauthAccount": {"emailAddress": email, "organizationUuid": org, "accountUuid": "00000000-0000-4000-8000-000000000099"},
    })
    .to_string()
}

/// Replaces account `number`'s stored (backup) credentials with `text`.
fn write_backup_credentials(scratch: &Scratch, number: u32, text: &str) {
    let lock = AccountLock::acquire(&scratch.root()).expect("the account lock is free");
    scratch
        .store()
        .write_credentials(&lock, number, &CredentialBlob::new(text.to_owned()))
        .expect("the credentials are written");
}

#[test]
fn rate_limit_writes_backoff() {
    let scratch = Scratch::new("rate_limit_writes_backoff");
    let transport = FakeTransport::new(rate_limited_one);
    let now = after_fetch(7200);

    let outcomes = poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    assert!(matches!(
        result_of(&outcomes, 1),
        PollResult::Failed(FetchError::Http { status: 429, .. })
    ));
    let row = scratch.usage_row(1);
    let at = epoch_seconds(now);
    assert_eq!(row["lastError"], json!("http-429"));
    assert!(
        (seconds(&row, "last429At") - at).abs() < 1e-3,
        "the 429 is stamped at the poll's time"
    );
    assert!(
        (seconds(&row, "backoffUntil") - (at + 120.0)).abs() < 1e-3,
        "a Retry-After of 120 s above the first failure's 30 s is honoured as the wait"
    );
    assert_eq!(row["consecutiveFailures"], json!(1));
    assert_eq!(row["claimId"], Value::Null, "recording releases the lease");
    assert_eq!(
        row["lastGood"]["five_hour"]["pct"],
        json!(25.0),
        "a failure keeps the last good reading"
    );
    assert!(matches!(
        result_of(&outcomes, 2),
        PollResult::Fetched { .. }
    ));
}

#[test]
fn fresh_row_is_not_fetched() {
    let scratch = Scratch::new("fresh_row_is_not_fetched");
    let transport = FakeTransport::new(success);
    let now = after_fetch(60);

    let outcomes = poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    assert_eq!(
        transport.calls(),
        vec!["two"],
        "account 1, fetched 60 s ago, is not asked again"
    );
    assert_eq!(result_of(&outcomes, 1), PollResult::NotDue);
    assert_eq!(
        scratch.usage_row(1)["lastAttemptAt"],
        json!(1_790_000_000.0),
        "the fresh row is left alone"
    );
}

#[test]
fn live_claim_blocks_second_reserve() {
    let scratch = Scratch::new("live_claim_blocks_second_reserve");
    let now = after_fetch(7200);

    let first =
        UsageTable::reserve(&scratch.root(), &identities(), now).expect("the first reserve runs");
    let second = UsageTable::reserve(&scratch.root(), &identities(), after_fetch(7210))
        .expect("the second reserve runs");
    let after_lease = UsageTable::reserve(&scratch.root(), &identities(), after_fetch(7291))
        .expect("the third reserve runs");

    assert!(
        first.contains(1) && first.contains(2),
        "both due accounts are claimed"
    );
    assert!(
        second.is_empty(),
        "a live claim keeps a second collector off both accounts"
    );
    assert!(
        after_lease.contains(1),
        "a lease that has lapsed no longer blocks"
    );
}

#[test]
fn stale_claim_is_fenced() {
    let scratch = Scratch::new("stale_claim_is_fenced");
    let root = scratch.root();
    let stale = UsageTable::reserve(&root, &identities(), after_fetch(7200))
        .expect("the first reserve runs");
    let current = UsageTable::reserve(&root, &identities(), after_fetch(7291))
        .expect("the lapsed lease is claimed again");
    let outcomes = BTreeMap::from([
        (1, Ok(Some(json!({"five_hour": {"pct": 99.0}})))),
        (2, Err(FetchError::Timeout)),
    ]);

    let accepted = UsageTable::record(
        &root,
        &outcomes,
        &stale,
        &PlanContext::new(Some(1), 90.0, Vec::new()),
        &|| after_fetch(7292),
    )
    .expect("the record runs");

    assert!(
        accepted.is_empty(),
        "a result fetched under an outdated claim is dropped"
    );
    let row = scratch.usage_row(1);
    assert_eq!(
        row["claimId"].as_str(),
        current.claim_id(1),
        "the newer lease stays"
    );
    assert_eq!(
        row["lastGood"]["five_hour"]["pct"],
        json!(25.0),
        "the newer row is not overwritten"
    );
    assert_eq!(scratch.usage_row(2)["consecutiveFailures"], json!(1));
}

#[test]
fn unknown_keys_survive_record() {
    let scratch = Scratch::new("unknown_keys_survive_record");
    let root = scratch.root();
    let now = after_fetch(7200);
    let claims = UsageTable::reserve(&root, &identities(), now).expect("the reserve runs");
    let outcomes = BTreeMap::from([(
        1,
        Ok(Some(
            json!({"five_hour": {"pct": 31.0}, "seven_day": {"pct": 12.0}}),
        )),
    )]);

    let accepted = UsageTable::record(
        &root,
        &outcomes,
        &claims,
        &PlanContext::new(Some(1), 90.0, Vec::new()),
        &|| now,
    )
    .expect("the record runs");

    assert!(accepted.contains(&1));
    let document = scratch.read_json("cache/usage.json");
    assert_eq!(document["schemaVersion"], json!(3));
    assert_eq!(
        document["futureTopLevel"],
        json!("kept"),
        "a top-level key this crate does not model survives"
    );
    let row = &document["accounts"]["1"];
    assert_eq!(
        row["futureRowKey"],
        json!({"kept": true}),
        "a row key this crate does not model survives"
    );
    assert_eq!(row["authDeadStrikes"], json!(0));
    assert_eq!(row["lastGood"]["five_hour"]["pct"], json!(31.0));
}

#[test]
fn expired_token_is_not_sent() {
    let scratch = Scratch::new("expired_token_is_not_sent");
    let expired = r#"{"claudeAiOauth":{"accessToken":"fake-access-two","refreshToken":"fake-refresh-two","expiresAt":1000}}"#;
    write_backup_credentials(&scratch, 2, expired);
    let transport = FakeTransport::new(success);
    let now = after_fetch(7200);

    let outcomes = poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    assert_eq!(result_of(&outcomes, 2), PollResult::TokenExpired);
    assert_eq!(
        transport.calls(),
        vec!["one"],
        "the expired token is never sent"
    );
    let row = scratch.usage_row(2);
    assert_eq!(
        row["lastAttemptAt"],
        json!(1_789_999_900.0),
        "no claim is taken for an expired token"
    );
}

#[test]
fn poll_writes_next_poll_at() {
    let scratch = Scratch::new("poll_writes_next_poll_at");
    let transport = FakeTransport::new(success);
    let now = after_fetch(7200);

    let outcomes = poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    let PollResult::Fetched { five_hour, .. } = result_of(&outcomes, 1) else {
        unreachable!("account 1 is fetched");
    };
    assert!(five_hour.is_some_and(|window| (window.pct - 30.0).abs() < f64::EPSILON));
    let row = scratch.usage_row(1);
    let at = epoch_seconds(now);
    assert!((seconds(&row, "fetchedAt") - at).abs() < 1e-3);
    // The active account's binding window moved from 25 % to 30 %: its 300 s interval halves to the 180 s floor.
    assert_eq!(row["pollIntervalS"], json!(180.0));
    let next = seconds(&row, "nextPollAt");
    assert!(
        next >= at + 162.0 - 1e-3 && next <= at + 198.0 + 1e-3,
        "the next poll is the interval away, give or take 10 %"
    );
    assert!(
        scratch.usage_row(2)["nextPollAt"]
            .as_f64()
            .is_some_and(|next| next > at)
    );
}

#[test]
fn active_account_uses_live_credential() {
    let scratch = Scratch::new("active_account_uses_live_credential");
    let expired = r#"{"claudeAiOauth":{"accessToken":"fake-access-one","refreshToken":"fake-refresh-one","expiresAt":1000}}"#;
    write_backup_credentials(&scratch, 1, expired);
    let live = r#"{"claudeAiOauth":{"accessToken":"fake-access-live","refreshToken":"fake-refresh-live","expiresAt":4102444800000}}"#;
    scratch.write_live_credentials(live);
    scratch.write_claude_json(&claude_json(ONE, ONE_ORG));
    let transport = FakeTransport::new(success);
    let now = after_fetch(7200);

    let outcomes = poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    assert!(matches!(
        result_of(&outcomes, 1),
        PollResult::Fetched { .. }
    ));
    assert_eq!(
        transport.calls(),
        vec!["live", "two"],
        "the active account is fetched with Claude Code's live token, the other with its backup"
    );
    let after = fs::read_to_string(scratch.dir.join("claude").join(".credentials.json"))
        .expect("the live file is still there");
    assert!(after == live, "the live credential file is only read");
}

#[test]
fn active_account_falls_back_to_backup() {
    let scratch = Scratch::new("active_account_falls_back_to_backup");
    scratch.write_claude_json(&claude_json(ONE, ONE_ORG));
    let transport = FakeTransport::new(success);
    let now = after_fetch(7200);

    let outcomes = poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    assert!(matches!(
        result_of(&outcomes, 1),
        PollResult::Fetched { .. }
    ));
    assert_eq!(
        transport.calls(),
        vec!["one", "two"],
        "with no live credential file the active account's backup token is used"
    );
}

#[test]
fn active_is_found_by_claude_json_identity() {
    let scratch = Scratch::new("active_is_found_by_claude_json_identity");
    let expired = r#"{"claudeAiOauth":{"accessToken":"fake-access-two","refreshToken":"fake-refresh-two","expiresAt":1000}}"#;
    write_backup_credentials(&scratch, 2, expired);
    scratch.write_live_credentials(LIVE_CREDENTIALS);
    scratch.write_claude_json(&claude_json(TWO, ""));
    let transport = FakeTransport::new(success);
    let now = after_fetch(7200);

    let outcomes = poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    assert!(matches!(
        result_of(&outcomes, 1),
        PollResult::Fetched { .. }
    ));
    assert!(matches!(
        result_of(&outcomes, 2),
        PollResult::Fetched { .. }
    ));
    assert_eq!(
        transport.calls(),
        vec!["one", "live"],
        "the account ~/.claude.json names gets the live token, whatever accounts.json calls active"
    );
    assert!(
        !format!("{outcomes:?}").contains(CLAUDE_JSON_SECRET),
        "the poll's results never carry ~/.claude.json's API key"
    );
}

#[test]
fn org_uuid_falsy_counts_as_empty() {
    let scratch = Scratch::new("org_uuid_falsy_counts_as_empty");
    scratch.write_live_credentials(LIVE_CREDENTIALS);
    let config = json!({"oauthAccount": {"emailAddress": TWO, "organizationUuid": false}});
    scratch.write_claude_json(&config.to_string());
    let transport = FakeTransport::new(success);
    let now = after_fetch(7200);

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    assert_eq!(
        transport.calls(),
        vec!["one", "live"],
        "an organizationUuid of false is empty, so the login matches account 2 and its organization of none"
    );
}

/// Each account's binding window as the fixture already holds it (account 1 at 25 %, account 2 at 80 %), so neither
/// moved and each plan grows its interval toward its ceiling: 300 s for the active account, 600 s for the others.
#[expect(
    clippy::unnecessary_wraps,
    reason = "it is a fake transport's answer, which returns a Result"
)]
fn unchanged(label: &str) -> Result<RawUsage, TransportError> {
    let (five_hour, seven_day) = if label == "one" {
        (25.0, 10.0)
    } else {
        (80.0, 55.5)
    };
    Ok(RawUsage {
        status: 200,
        body: json!({
            "five_hour": {"utilization": five_hour, "resets_at": null},
            "seven_day": {"utilization": seven_day, "resets_at": null},
        }),
        retry_after: None,
    })
}

#[test]
fn cadence_follows_logged_in_account() {
    let scratch = Scratch::new("cadence_follows_logged_in_account");
    scratch.write_claude_json(&claude_json(TWO, ""));
    let transport = FakeTransport::new(unchanged);
    let now = after_fetch(7200);

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    let at = epoch_seconds(now);
    // Account 2, logged in: its 600 s interval × 1.5 is capped at the active ceiling of 300 s.
    let logged_in = scratch.usage_row(2);
    assert_eq!(
        logged_in["pollIntervalS"],
        json!(300.0),
        "the logged-in account polls at the active cadence"
    );
    let next = seconds(&logged_in, "nextPollAt");
    assert!(
        next >= at + 270.0 - 1e-3 && next <= at + 330.0 + 1e-3,
        "its next poll is 300 s away, give or take 10 %"
    );
    // Account 1, active only in accounts.json: its 300 s interval grows × 1.5 to 450 s under the candidate ceiling.
    let candidate = scratch.usage_row(1);
    assert_eq!(
        candidate["pollIntervalS"],
        json!(450.0),
        "accounts.json's active account polls at the candidate cadence"
    );
    let next = seconds(&candidate, "nextPollAt");
    assert!(
        next >= at + 405.0 - 1e-3 && next <= at + 495.0 + 1e-3,
        "its next poll is 450 s away, give or take 10 %"
    );
}

#[test]
fn no_logged_in_account_means_no_active_cadence() {
    let scratch = Scratch::new("no_logged_in_account_means_no_active_cadence");
    let transport = FakeTransport::new(unchanged);
    let now = after_fetch(7200);

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    // Account 1, active in accounts.json but with no ~/.claude.json naming it: 300 s × 1.5 under the candidate ceiling.
    assert_eq!(
        scratch.usage_row(1)["pollIntervalS"],
        json!(450.0),
        "with no logged-in account, accounts.json's active account polls at the candidate cadence"
    );
}

#[test]
fn unknown_live_identity_uses_no_live_token() {
    let scratch = Scratch::new("unknown_live_identity_uses_no_live_token");
    scratch.write_live_credentials(LIVE_CREDENTIALS);
    scratch.write_claude_json(&claude_json("stranger@example.test", ""));
    let transport = FakeTransport::new(success);
    let now = after_fetch(7200);

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    assert_eq!(
        transport.calls(),
        vec!["one", "two"],
        "a live login no stored account matches lends its token to none"
    );
}

#[test]
fn last_good_keeps_scoped_and_spend() {
    let scratch = Scratch::new("last_good_keeps_scoped_and_spend");
    let transport = FakeTransport::new(scoped_spend);
    let now = after_fetch(7200);

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    assert_eq!(
        scratch.usage_row(1)["lastGood"],
        scoped_spend_fixture()["lastGood"],
        "lastGood holds the windows, spend and per-model windows as the poll writes them"
    );
}

#[test]
fn record_stamps_time_after_fetch() {
    let scratch = Scratch::new("record_stamps_time_after_fetch");
    let transport = FakeTransport::new(rate_limited_one);
    let start = after_fetch(7200);
    let next = Cell::new(start);
    let last = Cell::new(start);
    let clock = || {
        let reading = next.get();
        next.set(
            reading
                .checked_add(jiff::SignedDuration::from_secs(4))
                .expect("the clock stays in range"),
        );
        last.set(reading);
        reading
    };

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &clock,
    )
    .expect("the poll runs");

    assert!(
        last.get() > start,
        "the clock is read again after the fetches"
    );
    let recorded = epoch_seconds(last.get());
    let failed = scratch.usage_row(1);
    assert!(
        (seconds(&failed, "last429At") - recorded).abs() < 1e-3,
        "the 429 is stamped when it is recorded"
    );
    assert!(
        (seconds(&failed, "backoffUntil") - (recorded + 120.0)).abs() < 1e-3,
        "the Retry-After counts from the record, not the reserve"
    );
    assert!(
        (seconds(&scratch.usage_row(2), "fetchedAt") - recorded).abs() < 1e-3,
        "fetchedAt is the record's time"
    );
}

#[test]
fn corrupt_cache_reads_empty_and_heals() {
    for (name, body) in [
        ("torn", "{\"schemaVersion\": 2, \"acc"),
        ("empty", ""),
        ("garbage", "[1, 2, 3]"),
        ("array", "[{\"schemaVersion\": 3, \"accounts\": {}}]"),
    ] {
        let scratch = Scratch::new(&format!("corrupt_cache_reads_empty_and_heals_{name}"));
        scratch.write("cache/usage.json", body);
        let transport = FakeTransport::new(success);
        let now = after_fetch(7200);

        let entries = UsageTable::entries(&scratch.root(), &identities());
        let outcomes = poll_once(
            &scratch.store(),
            &scratch.claude(),
            &transport,
            &AutoSwitchSettings::default(),
            &|| now,
        )
        .expect("a corrupt table does not stop the poll");

        assert!(
            entries
                .values()
                .all(|entry| *entry == UsageEntry::default()),
            "a {name} table reads as empty"
        );
        assert!(
            matches!(result_of(&outcomes, 1), PollResult::Fetched { .. }),
            "a {name} table is polled"
        );
        let document = scratch.read_json("cache/usage.json");
        assert_eq!(
            document["schemaVersion"],
            json!(3),
            "a {name} table is rewritten as schema 3"
        );
        for number in ["1", "2"] {
            assert!(
                document["accounts"][number]["fetchedAt"].as_f64().is_some(),
                "a {name} table gets account {number}'s reading"
            );
        }
    }
}

/// Writes the fixture's table with its `schemaVersion` set to `version` (removed for `None`), runs one poll (a reserve
/// and a record), and asserts the poll stops with [`PollError::UnsupportedCache`], fetches nothing, and leaves the bytes
/// as written.
fn assert_never_rewritten(test: &str, version: Option<Value>) {
    let scratch = Scratch::new(test);
    let mut document = scratch.read_json("cache/usage.json");
    match version {
        Some(version) => document["schemaVersion"] = version,
        None => {
            document
                .as_object_mut()
                .expect("the table is an object")
                .shift_remove("schemaVersion");
        }
    }
    let body = document.to_string();
    scratch.write("cache/usage.json", &body);
    let transport = FakeTransport::new(success);
    let now = after_fetch(7200);

    let error = poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect_err("another writer's table stops the poll");

    assert!(matches!(error, PollError::UnsupportedCache), "{error:?}");
    assert!(transport.calls().is_empty(), "nothing is fetched");
    let after = fs::read_to_string(scratch.backup().join("cache").join("usage.json"))
        .expect("the table is still there");
    assert_eq!(after, body, "another writer's table is never written");
}

#[test]
fn schema_three_as_float_is_never_rewritten() {
    assert_never_rewritten("schema_three_as_float_is_never_rewritten", Some(json!(3.0)));
}

#[test]
fn newer_schema_is_still_refused() {
    assert_never_rewritten("newer_schema_is_still_refused", Some(json!(4)));
}

#[test]
fn a_usage_table_of_version_2_is_never_rewritten() {
    assert_never_rewritten(
        "a_usage_table_of_version_2_is_never_rewritten",
        Some(json!(2)),
    );
}

#[test]
fn a_usage_table_with_a_string_version_is_never_rewritten() {
    assert_never_rewritten(
        "a_usage_table_with_a_string_version_is_never_rewritten",
        Some(json!("3")),
    );
}

#[test]
fn a_usage_table_with_no_version_is_never_rewritten() {
    assert_never_rewritten("a_usage_table_with_no_version_is_never_rewritten", None);
}

/// A table whose `schemaVersion` turns to 4 between the reserve and the record: the record refuses it with
/// [`PollError::UnsupportedCache`] and leaves its bytes exactly as the other writer left them.
#[test]
fn a_table_turned_version_4_after_the_reserve_is_left_untouched_by_the_record() {
    let scratch =
        Scratch::new("a_table_turned_version_4_after_the_reserve_is_left_untouched_by_the_record");
    let root = scratch.root();
    let claims = UsageTable::reserve(&root, &identities(), after_fetch(7200))
        .expect("the store's own table takes the reserve");
    assert!(
        claims.contains(1),
        "account 1 is claimed before another writer turns the table newer"
    );
    let mut document = scratch.read_json("cache/usage.json");
    document["schemaVersion"] = json!(4);
    let body = document.to_string();
    scratch.write("cache/usage.json", &body);
    let outcomes = BTreeMap::from([(1, Ok(Some(json!({"five_hour": {"pct": 99.0}}))))]);

    let recorded = UsageTable::record(
        &root,
        &outcomes,
        &claims,
        &PlanContext::new(Some(1), 90.0, Vec::new()),
        &|| after_fetch(7300),
    );

    assert!(
        matches!(recorded, Err(PollError::UnsupportedCache)),
        "{recorded:?}"
    );
    let after = fs::read_to_string(scratch.backup().join("cache").join("usage.json"))
        .expect("the table is still there");
    assert_eq!(after, body, "a version 4 table is never written");
}

/// Bytes that do not parse as JSON are a torn write of this store's own table: the next poll rewrites them as a table of
/// version 3 holding its readings.
#[test]
fn torn_usage_bytes_are_rewritten_as_version_3() {
    let scratch = Scratch::new("torn_usage_bytes_are_rewritten_as_version_3");
    scratch.write(
        "cache/usage.json",
        "{\"schemaVersion\": 3, \"accounts\": {\"1\": {\"ema",
    );
    let transport = FakeTransport::new(success);
    let now = after_fetch(7200);

    let outcomes = poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("a torn table does not stop the poll");

    assert!(
        matches!(result_of(&outcomes, 1), PollResult::Fetched { .. }),
        "account 1 is polled"
    );
    let document = scratch.read_json("cache/usage.json");
    assert_eq!(
        document["schemaVersion"],
        json!(3),
        "the table is rewritten as version 3"
    );
    assert!(
        document["accounts"]["1"]["fetchedAt"].as_f64().is_some(),
        "the rewrite holds account 1's reading"
    );
}

/// An older binary in a store a newer version wrote (`accounts.json` one `schemaVersion` past this crate's) writes
/// nothing to the usage table: a poll, a reserve and a record of a claim taken before the store turned newer each stop
/// with [`PollError::StoreRefused`], nothing is fetched, and the table keeps its bytes.
#[test]
fn an_older_binary_leaves_the_usage_table_alone_in_a_newer_store() {
    let scratch = Scratch::new("an_older_binary_leaves_the_usage_table_alone_in_a_newer_store");
    let root = scratch.root();
    let claims = UsageTable::reserve(&root, &identities(), after_fetch(7200))
        .expect("a store of this version takes the reserve");
    assert!(
        claims.contains(1),
        "account 1 is claimed before the store turns newer"
    );
    let mut accounts = scratch.read_json("accounts.json");
    accounts["schemaVersion"] = json!(2);
    scratch.write("accounts.json", &accounts.to_string());
    let table = scratch.backup().join("cache").join("usage.json");
    let before = fs::read(&table).expect("the usage table reads");
    let transport = FakeTransport::new(success);
    let now = after_fetch(7300);

    let polled = poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    );
    let reserved = UsageTable::reserve(&root, &identities(), now);
    let outcomes = BTreeMap::from([(1, Ok(Some(json!({"five_hour": {"pct": 99.0}}))))]);
    let recorded = UsageTable::record(
        &root,
        &outcomes,
        &claims,
        &PlanContext::new(Some(1), 90.0, Vec::new()),
        &|| now,
    );

    for (entry, error) in [
        ("poll_once", polled.err()),
        ("reserve", reserved.err()),
        ("record", recorded.err()),
    ] {
        assert!(
            matches!(
                error,
                Some(PollError::StoreRefused(StoreErrorKind::UnknownSchema))
            ),
            "{entry} is refused in a newer store: {error:?}"
        );
    }
    assert!(transport.calls().is_empty(), "nothing is fetched");
    assert_eq!(
        fs::read(&table).expect("the usage table reads"),
        before,
        "the usage table keeps its bytes"
    );
}

/// A low 5-hour and 7-day window, and a per-model `Fable` window at `fable` percent.
fn model_usage(five_hour: f64, fable: f64) -> Value {
    json!({
        "five_hour": {"pct": five_hour},
        "seven_day": {"pct": 0.0},
        "scoped": [{"name": "Fable", "pct": fable}],
    })
}

#[test]
fn scoped_binding_window_keeps_active_cadence_tight() {
    let prev = model_usage(5.0, 84.0);
    let new = model_usage(5.0, 88.0);
    let fable = ["Fable".to_owned()];
    let request = |models: &[String]| {
        plan_after_fetch(&PlanRequest {
            prev_interval_s: Some(180.0),
            prev_usage: Some(&prev),
            new_usage: Some(&new),
            is_active: true,
            threshold: 90.0,
            models,
            recent_429: false,
            now: jiff::Timestamp::from_second(1000).expect("the time is in range"),
            jitter: 0.5,
        })
    };

    let scoped = request(&fable);
    let unscoped = request(&[]);

    assert!(
        (scoped.interval_s - URGENT_INTERVAL_S).abs() < 1e-9,
        "Fable moving at 88 % inside the escalation band engages the urgent cadence: {scoped:?}"
    );
    assert!(
        unscoped.interval_s > 180.0,
        "with no model configured the quiet 5-hour window alone decays the interval: {unscoped:?}"
    );
}

/// A 200 reading 5 % of the 5-hour window, none of the 7-day one, and 88 % of a per-model `Fable` window.
#[expect(
    clippy::unnecessary_wraps,
    reason = "it is a fake transport's answer, which returns a Result"
)]
fn fable_at_88(_label: &str) -> Result<RawUsage, TransportError> {
    Ok(RawUsage {
        status: 200,
        body: json!({
            "five_hour": {"utilization": 5.0, "resets_at": null},
            "seven_day": {"utilization": 0.0, "resets_at": null},
            "limits": [{"scope": {"model": {"display_name": "Fable"}}, "percent": 88.0, "resets_at": null}],
        }),
        retry_after: None,
    })
}

/// Account 1's written `pollIntervalS` after one poll in which it is the account Claude Code is logged in as, its prior
/// row reading [`model_usage`]`(5, 84)` on a 180 s interval and the fetch answering [`fable_at_88`], with the settings'
/// `models` holding `model` (empty for `None`).
fn active_interval_after_fable_poll(test: &str, model: Option<&str>) -> Value {
    let scratch = Scratch::new(test);
    let mut settings = AutoSwitchSettings::default();
    settings.models = model.map(str::to_owned).into_iter().collect();
    scratch.write_claude_json(&claude_json(ONE, ONE_ORG));
    let mut document = scratch.read_json("cache/usage.json");
    let row = &mut document["accounts"]["1"];
    row["lastGood"] = model_usage(5.0, 84.0);
    row["pollIntervalS"] = json!(180.0);
    scratch.write("cache/usage.json", &document.to_string());
    let transport = FakeTransport::new(fable_at_88);
    let now = after_fetch(7200);

    let outcomes = poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &settings,
        &|| now,
    )
    .expect("the poll runs");

    assert!(
        matches!(result_of(&outcomes, 1), PollResult::Fetched { .. }),
        "account 1 is fetched"
    );
    scratch.usage_row(1)["pollIntervalS"].clone()
}

#[test]
fn poll_once_plans_the_active_account_by_its_configured_model_window() {
    let interval = active_interval_after_fable_poll(
        "poll_once_plans_the_active_account_by_its_configured_model_window",
        Some("Fable"),
    );

    assert_eq!(
        interval,
        json!(URGENT_INTERVAL_S),
        "the configured Fable window moving from 84 % to 88 % puts the active account on the urgent cadence"
    );
}

#[test]
fn poll_once_without_a_model_ignores_scoped_windows() {
    let interval =
        active_interval_after_fable_poll("poll_once_without_a_model_ignores_scoped_windows", None);

    assert!(
        interval
            .as_f64()
            .is_some_and(|seconds| seconds > URGENT_INTERVAL_S),
        "with no model configured only the quiet 5-hour window counts, so the interval is not urgent: {interval}"
    );
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
fn a_failed_usage_cache_write_leaves_no_temp_file() {
    let scratch = Scratch::new("a_failed_usage_cache_write_leaves_no_temp_file");
    let target = scratch.backup().join("cache/usage.json");
    let before = fs::read(&target).expect("the usage cache reads");

    let held = hold_open_against_replace(&target);
    let claimed = UsageTable::reserve(&scratch.root(), &identities(), after_fetch(7200));
    assert!(
        matches!(claimed, Err(PollError::CacheWrite(_))),
        "a cache that cannot be replaced is a write error: {claimed:?}"
    );
    drop(held);

    assert_eq!(
        fs::read(&target).expect("the usage cache reads"),
        before,
        "the cache is untouched"
    );
    assert_eq!(
        temp_files(&scratch.backup().join("cache")),
        Vec::<String>::new(),
        "no temp file is left behind"
    );
}

fn prepaid_fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/prepaid.json"))
        .expect("the prepaid fixture is JSON")
}

/// A 200 usage answer with both windows and `extra_usage` as given.
#[expect(
    clippy::unnecessary_wraps,
    reason = "it builds a fake transport's answer, which returns a Result"
)]
fn usage_with_extra(extra: &Value) -> Result<RawUsage, TransportError> {
    Ok(RawUsage {
        status: 200,
        body: json!({
            "five_hour": {"utilization": 30.0, "resets_at": null},
            "seven_day": {"utilization": 12.0, "resets_at": null},
            "extra_usage": extra.clone(),
        }),
        retry_after: None,
    })
}

fn uncapped_spend(_label: &str) -> Result<RawUsage, TransportError> {
    usage_with_extra(&prepaid_fixture()["extraUsageUncapped"])
}

fn spend_in_thousandths(_label: &str) -> Result<RawUsage, TransportError> {
    usage_with_extra(&json!({
        "is_enabled": true,
        "used_credits": 31640,
        "monthly_limit": 100_000,
        "utilization": 31.64,
        "currency": "EUR",
        "decimal_places": 3,
    }))
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "it is a fake transport's answer, which returns a Result"
)]
fn exhausted(_label: &str) -> Result<RawUsage, TransportError> {
    Ok(RawUsage {
        status: 200,
        body: json!({"five_hour": {"utilization": 100.0, "resets_at": null}, "seven_day": {"utilization": 40.0, "resets_at": null}}),
        retry_after: None,
    })
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "it is a fake transport's answer, which returns a Result"
)]
fn funded(_label: &str) -> Result<RawUsage, TransportError> {
    Ok(RawUsage {
        status: 200,
        body: prepaid_fixture()["funded"].clone(),
        retry_after: None,
    })
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "it is a fake transport's answer, which returns a Result"
)]
fn unfunded(_label: &str) -> Result<RawUsage, TransportError> {
    Ok(RawUsage {
        status: 200,
        body: prepaid_fixture()["unfunded"].clone(),
        retry_after: None,
    })
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "it is a fake transport's answer, which returns a Result"
)]
fn prepaid_rate_limited(_label: &str) -> Result<RawUsage, TransportError> {
    Ok(RawUsage {
        status: 429,
        body: json!({"error": {"type": "rate_limit_error"}}),
        retry_after: Some(120.0),
    })
}

/// Makes account `number`'s row due for its next usage poll at once, leaving its `prepaid` alone.
fn make_poll_due(scratch: &Scratch, number: u32) {
    let mut table = scratch.read_json("cache/usage.json");
    table["accounts"][number.to_string()]["nextPollAt"] = json!(0.0);
    scratch.write("cache/usage.json", &table.to_string());
}

#[test]
fn spend_kept_with_no_monthly_limit() {
    let scratch = Scratch::new("spend_kept_with_no_monthly_limit");
    let transport = FakeTransport::new(uncapped_spend);
    let now = after_fetch(7200);

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    let spend = &scratch.usage_row(1)["lastGood"]["spend"];
    assert_eq!(
        spend["used"],
        json!(31.64),
        "used credits are scaled by decimal_places: {spend}"
    );
    assert_eq!(
        spend["limit"],
        Value::Null,
        "an uncapped block keeps its spend with no limit"
    );
    assert_eq!(spend["pct"], Value::Null);
    assert_eq!(spend["currency"], json!("USD"));
}

#[test]
fn spend_scales_by_decimal_places() {
    let scratch = Scratch::new("spend_scales_by_decimal_places");
    let transport = FakeTransport::new(spend_in_thousandths);
    let now = after_fetch(7200);

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    let spend = &scratch.usage_row(1)["lastGood"]["spend"];
    assert_eq!(spend["used"], json!(31.64), "{spend}");
    assert_eq!(spend["limit"], json!(100.0), "{spend}");
    assert_eq!(spend["currency"], json!("EUR"));
}

#[test]
fn extra_usage_flags_recorded() {
    let scratch = Scratch::new("extra_usage_flags_recorded");
    let transport = FakeTransport::new(success);
    let now = after_fetch(7200);

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    let last_good = &scratch.usage_row(1)["lastGood"];
    assert_eq!(
        last_good["extra_usage"],
        json!({"enabled": false}),
        "an extra_usage block that is off is recorded as off, the flags it leaves out as unknown"
    );
    assert!(
        last_good.get("spend").is_none(),
        "a block that is off has no spend: {last_good}"
    );
}

fn disabled_for_a_reason(_label: &str) -> Result<RawUsage, TransportError> {
    usage_with_extra(&json!({"is_enabled": true, "disabled_reason": "org_disabled"}))
}

/// A block's `disabled_reason` is recorded as `disabled_reason_set`: true for a non-null reason, false for a null one,
/// and left out for an absent one.
#[test]
fn extra_usage_records_disabled_reason() {
    let now = after_fetch(7200);
    let cases: [(&str, Answer, Option<bool>); 3] = [
        (
            "extra_usage_records_disabled_reason_set",
            disabled_for_a_reason,
            Some(true),
        ),
        (
            "extra_usage_records_disabled_reason_null",
            uncapped_spend,
            Some(false),
        ),
        ("extra_usage_records_disabled_reason_absent", success, None),
    ];
    for (name, answer, set) in cases {
        let scratch = Scratch::new(name);
        let transport = FakeTransport::new(answer);

        poll_once(
            &scratch.store(),
            &scratch.claude(),
            &transport,
            &AutoSwitchSettings::default(),
            &|| now,
        )
        .expect("the poll runs");

        let extra = &scratch.usage_row(1)["lastGood"]["extra_usage"];
        assert_eq!(
            extra.get("disabled_reason_set").and_then(Value::as_bool),
            set,
            "{name}: {extra}"
        );
        assert!(
            extra.get("enabled").is_some(),
            "{name}: the other flags are kept: {extra}"
        );
    }
}

fn user_disabled_omitted(_label: &str) -> Result<RawUsage, TransportError> {
    usage_with_extra(
        &json!({"is_enabled": true, "spend_limit_reached": false, "disabled_reason": null}),
    )
}

fn every_flag_omitted(_label: &str) -> Result<RawUsage, TransportError> {
    usage_with_extra(&json!({"is_enabled": true, "user_disabled": null}))
}

/// A flag the reply leaves out (or sends as null) is recorded as absent, never as false, so the paid fallback reads it
/// as unknown and fails closed; `enabled` keeps its meaning, false when `is_enabled` is left out.
#[test]
fn an_omitted_extra_usage_flag_reads_as_unknown() {
    let now = after_fetch(7200);
    let cases: [(&str, Answer, Value); 3] = [
        (
            "an_omitted_extra_usage_flag_user_disabled",
            user_disabled_omitted,
            json!({"enabled": true, "spend_limit_reached": false, "disabled_reason_set": false}),
        ),
        (
            "an_omitted_extra_usage_flag_every_one",
            every_flag_omitted,
            json!({"enabled": true}),
        ),
        (
            "an_omitted_extra_usage_flag_is_enabled",
            success,
            json!({"enabled": false}),
        ),
    ];
    for (name, answer, recorded) in cases {
        let scratch = Scratch::new(name);
        let transport = FakeTransport::new(answer);

        poll_once(
            &scratch.store(),
            &scratch.claude(),
            &transport,
            &AutoSwitchSettings::default(),
            &|| now,
        )
        .expect("the poll runs");

        assert_eq!(
            scratch.usage_row(1)["lastGood"]["extra_usage"],
            recorded,
            "{name}"
        );
    }
}

#[test]
fn prepaid_read_after_usage_success() {
    let scratch = Scratch::new("prepaid_read_after_usage_success");
    let transport = FakeTransport::with_prepaid(success, funded);
    let now = after_fetch(7200);

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    assert_eq!(
        transport.prepaid_calls(),
        vec![("one", ONE_ORG.to_owned())],
        "the balance is read with the token the usage read used, for the account's organization"
    );
    let prepaid = &scratch.usage_row(1)["prepaid"];
    assert_eq!(prepaid["amountMinor"], json!(102_003), "{prepaid}");
    assert_eq!(prepaid["exponent"], json!(2));
    assert_eq!(prepaid["currency"], json!("USD"));
    assert_eq!(prepaid["error"], Value::Null);
    assert!((seconds(prepaid, "fetchedAt") - epoch_seconds(now)).abs() < 1e-3);
    let entry = UsageTable::entries(&scratch.root(), &identities())
        .remove(&1)
        .expect("account 1 has an entry");
    assert!(
        !entry.extra.contains_key("prepaid"),
        "prepaid is a modelled key"
    );
    let modelled = entry.prepaid.expect("the entry models the balance");
    assert_eq!(modelled.amount_minor, Some(102_003));
    assert_eq!(modelled.currency.as_deref(), Some("USD"));
}

/// The funded answer with `auto_reload_settings` replaced by `settings`.
fn funded_with_auto_reload(settings: Value) -> Result<RawUsage, TransportError> {
    let mut answer = funded("")?;
    answer.body["auto_reload_settings"] = settings;
    Ok(answer)
}

fn auto_reload_on(_label: &str) -> Result<RawUsage, TransportError> {
    funded_with_auto_reload(json!({"enabled": true, "threshold": 1000}))
}

fn auto_reload_off(_label: &str) -> Result<RawUsage, TransportError> {
    funded_with_auto_reload(json!({"enabled": false}))
}

fn auto_reload_malformed(_label: &str) -> Result<RawUsage, TransportError> {
    funded_with_auto_reload(json!({"enabled": "yes"}))
}

#[test]
fn prepaid_records_auto_reload() {
    let cases: [(&str, Answer, Value); 4] = [
        ("on", auto_reload_on, json!(true)),
        ("off", auto_reload_off, json!(false)),
        ("null", funded, Value::Null),
        ("malformed", auto_reload_malformed, Value::Null),
    ];
    for (case, prepaid, expected) in cases {
        let scratch = Scratch::new(&format!("prepaid_records_auto_reload_{case}"));
        let transport = FakeTransport::with_prepaid(success, prepaid);
        let now = after_fetch(7200);

        poll_once(
            &scratch.store(),
            &scratch.claude(),
            &transport,
            &AutoSwitchSettings::default(),
            &|| now,
        )
        .expect("the poll runs");

        let row = scratch.usage_row(1);
        assert_eq!(
            row["prepaid"]["autoReload"], expected,
            "auto reload {case}: {row}"
        );
        let entry = UsageTable::entries(&scratch.root(), &identities())
            .remove(&1)
            .expect("account 1 has an entry");
        assert_eq!(
            entry
                .prepaid
                .expect("the entry models the balance")
                .auto_reload,
            expected.as_bool()
        );
    }
}

#[test]
fn prepaid_unfunded_is_zero_not_error() {
    let scratch = Scratch::new("prepaid_unfunded_is_zero_not_error");
    let transport = FakeTransport::with_prepaid(success, unfunded);
    let now = after_fetch(7200);

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    let prepaid = &scratch.usage_row(1)["prepaid"];
    assert_eq!(
        prepaid["error"],
        Value::Null,
        "no balance is not a failure: {prepaid}"
    );
    assert_eq!(prepaid["amountMinor"], json!(0));
    assert_eq!(prepaid["currency"], Value::Null);
}

#[test]
fn prepaid_failure_leaves_usage_row_alone() {
    let plain = Scratch::new("prepaid_failure_leaves_usage_row_alone_plain");
    let failing = Scratch::new("prepaid_failure_leaves_usage_row_alone");
    let now = after_fetch(7200);

    poll_once(
        &plain.store(),
        &plain.claude(),
        &FakeTransport::with_prepaid(success, funded),
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");
    poll_once(
        &failing.store(),
        &failing.claude(),
        &FakeTransport::with_prepaid(success, prepaid_rate_limited),
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    let failed = failing.usage_row(1);
    assert_eq!(failed["prepaid"]["error"], json!("http-429"), "{failed}");
    assert!(
        failed["prepaid"].get("amountMinor").is_none(),
        "a failure invents no figure"
    );
    let succeeded = plain.usage_row(1);
    for key in [
        "lastError",
        "last429At",
        "backoffUntil",
        "consecutiveFailures",
        "fetchedAt",
        "lastGood",
    ] {
        assert_eq!(
            failed.get(key),
            succeeded.get(key),
            "{key} is as after a plain success"
        );
    }
}

#[test]
fn prepaid_not_reread_within_interval() {
    let scratch = Scratch::new("prepaid_not_reread_within_interval");
    let start = after_fetch(7200);
    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &FakeTransport::with_prepaid(success, funded),
        &AutoSwitchSettings::default(),
        &|| start,
    )
    .expect("the poll runs");

    make_poll_due(&scratch, 1);
    let soon = FakeTransport::with_prepaid(success, funded);
    let at_300 = after_fetch(7200 + 300);
    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &soon,
        &AutoSwitchSettings::default(),
        &|| at_300,
    )
    .expect("the poll runs");
    assert!(soon.calls().contains(&"one"), "the usage read itself ran");
    assert_eq!(
        soon.prepaid_calls(),
        Vec::new(),
        "300 s after the last balance read, none is sent"
    );

    make_poll_due(&scratch, 1);
    let later = FakeTransport::with_prepaid(success, funded);
    let at_481 = after_fetch(7200 + 481);
    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &later,
        &AutoSwitchSettings::default(),
        &|| at_481,
    )
    .expect("the poll runs");
    assert_eq!(
        later.prepaid_calls(),
        vec![("one", ONE_ORG.to_owned())],
        "481 s after the last balance read, one is sent"
    );
}

#[test]
fn prepaid_read_on_early_exhausted_poll() {
    let scratch = Scratch::new("prepaid_read_on_early_exhausted_poll");
    let start = after_fetch(7200);
    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &FakeTransport::with_prepaid(exhausted, funded),
        &AutoSwitchSettings::default(),
        &|| start,
    )
    .expect("the poll runs");

    // The exhausted cadence is 600 s with ±10 % jitter: its earliest poll lands 540 s on.
    make_poll_due(&scratch, 1);
    let early = FakeTransport::with_prepaid(exhausted, funded);
    let at_540 = after_fetch(7200 + 540);
    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &early,
        &AutoSwitchSettings::default(),
        &|| at_540,
    )
    .expect("the poll runs");

    assert_eq!(
        early.prepaid_calls(),
        vec![("one", ONE_ORG.to_owned())],
        "a poll at the exhausted cadence's earliest jitter still reads the balance"
    );
}

#[test]
fn prepaid_skipped_without_org_uuid() {
    let scratch = Scratch::new("prepaid_skipped_without_org_uuid");
    let transport = FakeTransport::with_prepaid(success, funded);
    let now = after_fetch(7200);

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    assert!(
        transport.calls().contains(&"two"),
        "account 2's usage is read"
    );
    assert!(
        transport
            .prepaid_calls()
            .iter()
            .all(|(label, _)| *label != "two"),
        "account 2 has no organization, so no balance request is sent for it"
    );
    assert!(
        scratch.usage_row(2).get("prepaid").is_none(),
        "nor is a balance written"
    );
}

/// A 200 with no window at all, only the `extra_usage` block `extra`.
fn windowless(extra: &Value) -> Result<RawUsage, TransportError> {
    let mut answer = usage_with_extra(extra)?;
    let body = answer
        .body
        .as_object_mut()
        .expect("the answer is an object");
    body.remove("five_hour");
    body.remove("seven_day");
    Ok(answer)
}

fn extra_usage_off_only(_label: &str) -> Result<RawUsage, TransportError> {
    windowless(&json!({"is_enabled": false, "user_disabled": true}))
}

fn spend_only(_label: &str) -> Result<RawUsage, TransportError> {
    windowless(&json!({
        "is_enabled": true,
        "used_credits": 3164.0,
        "monthly_limit": 10_000,
        "utilization": 31.64,
        "currency": "USD",
        "decimal_places": 2,
    }))
}

#[test]
fn a_reply_without_windows_is_no_reading_even_with_extra_usage() {
    let scratch = Scratch::new("a_reply_without_windows_is_no_reading_even_with_extra_usage");
    let transport = FakeTransport::with_prepaid(extra_usage_off_only, funded);
    let now = after_fetch(7200);

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    let row = scratch.usage_row(1);
    assert_eq!(
        row["lastGood"],
        Value::Null,
        "an extra_usage block that is off makes no reading on its own: {row}"
    );
    assert_eq!(
        transport.prepaid_calls(),
        Vec::new(),
        "no reading, so no balance read beside it"
    );
    assert!(
        row.get("prepaid").is_none(),
        "nor is a balance written: {row}"
    );
}

#[test]
fn a_reply_with_only_spend_is_a_reading() {
    let scratch = Scratch::new("a_reply_with_only_spend_is_a_reading");
    let transport = FakeTransport::with_prepaid(spend_only, funded);
    let now = after_fetch(7200);

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    let row = scratch.usage_row(1);
    assert_eq!(
        row["lastGood"],
        json!({
            "spend": {"used": 31.64, "limit": 100.0, "pct": 31.64, "currency": "USD"},
            "extra_usage": {"enabled": true},
        }),
        "an enabled block with spend is a reading: {row}"
    );
    assert_eq!(
        transport.prepaid_calls(),
        vec![("one", ONE_ORG.to_owned())],
        "the reading brings the balance read along"
    );
}

#[test]
fn prepaid_failure_keeps_last_good_figure() {
    let scratch = Scratch::new("prepaid_failure_keeps_last_good_figure");
    let start = after_fetch(7200);
    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &FakeTransport::with_prepaid(success, funded),
        &AutoSwitchSettings::default(),
        &|| start,
    )
    .expect("the poll runs");

    make_poll_due(&scratch, 1);
    let failing = FakeTransport::with_prepaid(success, prepaid_rate_limited);
    let at_481 = after_fetch(7200 + 481);
    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &failing,
        &AutoSwitchSettings::default(),
        &|| at_481,
    )
    .expect("the poll runs");

    assert_eq!(
        failing.prepaid_calls().len(),
        1,
        "the due balance read was sent"
    );
    let prepaid = &scratch.usage_row(1)["prepaid"];
    assert_eq!(
        prepaid["amountMinor"],
        json!(102_003),
        "the last good figure stays: {prepaid}"
    );
    assert_eq!(prepaid["exponent"], json!(2));
    assert_eq!(prepaid["currency"], json!("USD"));
    assert!(
        (seconds(prepaid, "fetchedAt") - epoch_seconds(start)).abs() < 1e-3,
        "its fetchedAt stays"
    );
    assert_eq!(prepaid["error"], json!("http-429"));
    assert!(
        (seconds(prepaid, "attemptedAt") - epoch_seconds(at_481)).abs() < 1e-3,
        "the failed attempt is stamped"
    );
}

#[test]
fn prepaid_failure_retried_only_after_due() {
    let scratch = Scratch::new("prepaid_failure_retried_only_after_due");
    let start = after_fetch(7200);
    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &FakeTransport::with_prepaid(success, prepaid_rate_limited),
        &AutoSwitchSettings::default(),
        &|| start,
    )
    .expect("the poll runs");
    assert_eq!(scratch.usage_row(1)["prepaid"]["error"], json!("http-429"));

    make_poll_due(&scratch, 1);
    let soon = FakeTransport::with_prepaid(success, funded);
    let at_300 = after_fetch(7200 + 300);
    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &soon,
        &AutoSwitchSettings::default(),
        &|| at_300,
    )
    .expect("the poll runs");
    assert!(soon.calls().contains(&"one"), "the usage read itself ran");
    assert_eq!(
        soon.prepaid_calls(),
        Vec::new(),
        "300 s after a failed balance read, none is sent"
    );

    make_poll_due(&scratch, 1);
    let later = FakeTransport::with_prepaid(success, funded);
    let at_481 = after_fetch(7200 + 481);
    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &later,
        &AutoSwitchSettings::default(),
        &|| at_481,
    )
    .expect("the poll runs");
    assert_eq!(
        later.prepaid_calls(),
        vec![("one", ONE_ORG.to_owned())],
        "481 s after a failed balance read, it is retried"
    );
}

/// A transport that, inside each balance request, rewrites account 1's `prepaid` object to `newer` (another collector
/// recording a later read meanwhile) before answering as `inner` does.
struct Meddling<'s> {
    inner: FakeTransport,
    scratch: &'s Scratch,
    newer: Value,
}

impl UsageTransport for Meddling<'_> {
    fn get_usage(&self, token: &AccessToken) -> Result<RawUsage, TransportError> {
        self.inner.get_usage(token)
    }

    fn get_prepaid(&self, token: &AccessToken, org_uuid: &str) -> Result<RawUsage, TransportError> {
        let mut table = self.scratch.read_json("cache/usage.json");
        table["accounts"]["1"]["prepaid"] = self.newer.clone();
        self.scratch.write("cache/usage.json", &table.to_string());
        self.inner.get_prepaid(token, org_uuid)
    }
}

/// Polls once at `now` through a [`Meddling`] transport whose balance answer is `prepaid` and whose meddler writes
/// `newer`, and returns account 1's `prepaid` object afterwards.
fn prepaid_after_meddling(
    test: &str,
    prepaid: Answer,
    newer: &Value,
    now: jiff::Timestamp,
) -> Value {
    let scratch = Scratch::new(test);
    let transport = Meddling {
        inner: FakeTransport::with_prepaid(success, prepaid),
        scratch: &scratch,
        newer: newer.clone(),
    };

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    assert_eq!(
        transport.inner.prepaid_calls().len(),
        1,
        "the balance read was sent"
    );
    scratch.usage_row(1)["prepaid"].clone()
}

/// A transport whose answers advance the shared clock by their own amounts, as the time a slow read takes passes.
struct Drifting<'s> {
    inner: FakeTransport,
    offset: &'s Cell<i64>,
    usage_advance: i64,
    prepaid_advance: i64,
}

impl UsageTransport for Drifting<'_> {
    fn get_usage(&self, token: &AccessToken) -> Result<RawUsage, TransportError> {
        let answer = self.inner.get_usage(token);
        self.offset.set(self.offset.get() + self.usage_advance);
        answer
    }

    fn get_prepaid(&self, token: &AccessToken, org_uuid: &str) -> Result<RawUsage, TransportError> {
        let answer = self.inner.get_prepaid(token, org_uuid);
        self.offset.set(self.offset.get() + self.prepaid_advance);
        answer
    }
}

#[test]
fn a_due_balance_is_skipped_late_in_the_lease() {
    let scratch = Scratch::new("a_due_balance_is_skipped_late_in_the_lease");
    let offset = Cell::new(0_i64);
    let transport = Drifting {
        inner: FakeTransport::with_prepaid(success, funded),
        offset: &offset,
        usage_advance: 50,
        prepaid_advance: 0,
    };

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| after_fetch(7200 + offset.get()),
    )
    .expect("the poll runs");

    assert_eq!(
        transport.inner.prepaid_calls(),
        Vec::new(),
        "a balance due once the pass has used half the lease is not read"
    );
    assert_eq!(
        scratch.usage_row(1)["lastGood"]["five_hour"]["pct"],
        json!(30.0),
        "the usage reading is still written"
    );
    assert!(
        scratch.usage_row(1).get("prepaid").is_none(),
        "a skipped read writes no prepaid row, so it stays due for the next pass"
    );
}

#[test]
fn a_due_balance_before_the_cutoff_is_still_read() {
    let scratch = Scratch::new("a_due_balance_before_the_cutoff_is_still_read");
    let offset = Cell::new(0_i64);
    let transport = Drifting {
        inner: FakeTransport::with_prepaid(success, funded),
        offset: &offset,
        usage_advance: 10,
        prepaid_advance: 0,
    };

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| after_fetch(7200 + offset.get()),
    )
    .expect("the poll runs");

    assert_eq!(
        transport.inner.prepaid_calls(),
        vec![("one", ONE_ORG.to_owned())],
        "a balance due before the pass has used half the lease is read"
    );
}

#[test]
fn a_due_balance_at_exactly_the_cutoff_is_skipped() {
    let scratch = Scratch::new("a_due_balance_at_exactly_the_cutoff_is_skipped");
    let offset = Cell::new(0_i64);
    let transport = Drifting {
        inner: FakeTransport::with_prepaid(success, funded),
        offset: &offset,
        // `PREPAID_CUTOFF_S`: half the 90 s lease.
        usage_advance: 45,
        prepaid_advance: 0,
    };

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| after_fetch(7200 + offset.get()),
    )
    .expect("the poll runs");

    assert_eq!(
        transport.inner.prepaid_calls(),
        Vec::new(),
        "a pass that has used exactly the cutoff reads no balance"
    );
    assert!(
        scratch.usage_row(1).get("prepaid").is_none(),
        "a skipped read writes no prepaid row, so it stays due for the next pass"
    );
}

#[test]
fn prepaid_fetched_at_is_when_the_read_returned() {
    let scratch = Scratch::new("prepaid_fetched_at_is_when_the_read_returned");
    let offset = Cell::new(0_i64);
    let transport = Drifting {
        inner: FakeTransport::with_prepaid(success, funded),
        offset: &offset,
        usage_advance: 20,
        prepaid_advance: 10,
    };

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| after_fetch(7200 + offset.get()),
    )
    .expect("the poll runs");

    let prepaid = &scratch.usage_row(1)["prepaid"];
    // Account 1's usage read ends at +20 s, its balance read at +30 s, and the row is written after account 2's read
    // ends at +50 s: the balance is dated at its own read's return, not at the write.
    let returned = epoch_seconds(after_fetch(7200 + 30));
    assert!(
        (seconds(prepaid, "fetchedAt") - returned).abs() < 1e-3,
        "the balance is dated when its read returned, not when the row was written: {prepaid}"
    );
}

#[test]
fn prepaid_success_keeps_unknown_keys() {
    let scratch = Scratch::new("prepaid_success_keeps_unknown_keys");
    let mut table = scratch.read_json("cache/usage.json");
    table["accounts"]["1"]["prepaid"] = json!({
        "futureKey": {"kept": true},
        "amountMinor": 1, "exponent": 2, "currency": "USD", "autoReload": null,
        "fetchedAt": FETCHED_AT, "error": "timeout", "attemptedAt": FETCHED_AT,
    });
    scratch.write("cache/usage.json", &table.to_string());
    let transport = FakeTransport::with_prepaid(success, funded);
    let now = after_fetch(7200);

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    let prepaid = &scratch.usage_row(1)["prepaid"];
    assert_eq!(
        prepaid["futureKey"],
        json!({"kept": true}),
        "a key this crate does not model survives a balance read: {prepaid}"
    );
    assert_eq!(
        prepaid["amountMinor"],
        json!(102_003),
        "the modelled figure is the new one: {prepaid}"
    );
    assert_eq!(prepaid["error"], Value::Null, "{prepaid}");
}

#[test]
fn prepaid_non_object_is_replaced() {
    let scratch = Scratch::new("prepaid_non_object_is_replaced");
    let mut table = scratch.read_json("cache/usage.json");
    table["accounts"]["1"]["prepaid"] = json!(5);
    scratch.write("cache/usage.json", &table.to_string());
    let transport = FakeTransport::with_prepaid(success, funded);
    let now = after_fetch(7200);

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    let prepaid = &scratch.usage_row(1)["prepaid"];
    assert_eq!(
        prepaid["amountMinor"],
        json!(102_003),
        "a stored non-object is replaced by a fresh prepaid object: {prepaid}"
    );
    assert_eq!(prepaid["currency"], json!("USD"), "{prepaid}");
}

#[test]
fn prepaid_due_when_the_last_attempt_is_far_in_the_future() {
    let scratch = Scratch::new("prepaid_due_when_the_last_attempt_is_far_in_the_future");
    let now = after_fetch(7200);
    let mut table = scratch.read_json("cache/usage.json");
    table["accounts"]["1"]["prepaid"] = json!({
        "amountMinor": 1, "exponent": 2, "currency": "USD", "autoReload": null,
        "fetchedAt": epoch_seconds(now) - 1000.0, "error": null, "attemptedAt": epoch_seconds(now) + 3600.0,
    });
    scratch.write("cache/usage.json", &table.to_string());
    let transport = FakeTransport::with_prepaid(success, funded);

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    assert_eq!(
        transport.prepaid_calls(),
        vec![("one", ONE_ORG.to_owned())],
        "an attempt stamped an hour ahead does not keep the balance from being read"
    );
    let prepaid = &scratch.usage_row(1)["prepaid"];
    assert!(
        (seconds(prepaid, "attemptedAt") - epoch_seconds(now)).abs() < 1e-3,
        "the read is stamped at now: {prepaid}"
    );
}

#[test]
fn prepaid_not_due_when_the_last_attempt_is_within_the_slack() {
    let scratch = Scratch::new("prepaid_not_due_when_the_last_attempt_is_within_the_slack");
    let now = after_fetch(7200);
    let mut table = scratch.read_json("cache/usage.json");
    table["accounts"]["1"]["prepaid"] = json!({
        "amountMinor": 1, "exponent": 2, "currency": "USD", "autoReload": null,
        "fetchedAt": epoch_seconds(now) - 1000.0, "error": null, "attemptedAt": epoch_seconds(now) + 30.0,
    });
    scratch.write("cache/usage.json", &table.to_string());
    let transport = FakeTransport::with_prepaid(success, funded);

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    assert!(
        transport.calls().contains(&"one"),
        "the usage read itself ran, so the balance was judged due on its own"
    );
    assert_eq!(
        transport.prepaid_calls(),
        Vec::new(),
        "an attempt stamped inside the slack is a read another collector made just now"
    );
}

#[test]
fn prepaid_failure_over_a_far_future_attempt_is_recorded() {
    let now = after_fetch(7200);
    let at = epoch_seconds(now);
    let newer = json!({
        "amountMinor": 700, "exponent": 2, "currency": "USD", "autoReload": null,
        "fetchedAt": at - 1000.0, "error": "timeout", "attemptedAt": at + 3600.0,
    });

    let prepaid = prepaid_after_meddling(
        "prepaid_failure_over_a_far_future_attempt_is_recorded",
        prepaid_rate_limited,
        &newer,
        now,
    );

    assert_eq!(
        prepaid["error"],
        json!("http-429"),
        "a failure is recorded over an attempt stamped far ahead: {prepaid}"
    );
    assert!(
        (seconds(&prepaid, "attemptedAt") - at).abs() < 1e-3,
        "the failure is stamped at the read's own time: {prepaid}"
    );
    assert_eq!(
        prepaid["amountMinor"],
        json!(700),
        "the stored figure stays: {prepaid}"
    );
}

#[test]
fn prepaid_older_fetch_never_replaces_newer() {
    let now = after_fetch(7200);
    let later = epoch_seconds(now) + 60.0;
    let newer = json!({
        "amountMinor": 700, "exponent": 2, "currency": "USD", "autoReload": null,
        "fetchedAt": later, "error": null, "attemptedAt": later,
    });

    let prepaid = prepaid_after_meddling(
        "prepaid_older_fetch_never_replaces_newer",
        funded,
        &newer,
        now,
    );

    assert_eq!(
        prepaid, newer,
        "a balance read before the stored one's fetchedAt replaces nothing"
    );
}

#[test]
fn prepaid_older_failure_never_replaces_newer_attempt() {
    let now = after_fetch(7200);
    let at = epoch_seconds(now);
    let newer = json!({
        "amountMinor": 700, "exponent": 2, "currency": "USD", "autoReload": null,
        "fetchedAt": at - 1000.0, "error": "timeout", "attemptedAt": at + 60.0,
    });

    let prepaid = prepaid_after_meddling(
        "prepaid_older_failure_never_replaces_newer_attempt",
        prepaid_rate_limited,
        &newer,
        now,
    );

    assert_eq!(
        prepaid, newer,
        "a failure older than the stored attempt records nothing"
    );
}

/// A transport whose usage request answers [`success`] and whose balance request answers 200 with the body it holds.
struct PrepaidBody(Value);

impl UsageTransport for PrepaidBody {
    fn get_usage(&self, _token: &AccessToken) -> Result<RawUsage, TransportError> {
        success("")
    }

    fn get_prepaid(
        &self,
        _token: &AccessToken,
        _org_uuid: &str,
    ) -> Result<RawUsage, TransportError> {
        Ok(RawUsage {
            status: 200,
            body: self.0.clone(),
            retry_after: None,
        })
    }
}

#[test]
fn prepaid_currency_must_be_three_uppercase_letters() {
    let cases = [
        ("usd", json!("usd"), json!("bad-response")),
        ("two-letters", json!("US"), json!("bad-response")),
        ("four-letters", json!("USDX"), json!("bad-response")),
        ("symbol", json!("U$D"), json!("bad-response")),
        ("non-ascii", json!("ÜSD"), json!("bad-response")),
        ("number", json!(840), json!("bad-response")),
        ("object", json!({"code": "USD"}), json!("bad-response")),
        ("eur", json!("EUR"), Value::Null),
    ];
    for (case, currency, error) in cases {
        let scratch = Scratch::new(&format!(
            "prepaid_currency_must_be_three_uppercase_letters_{case}"
        ));
        let mut body = prepaid_fixture()["funded"].clone();
        body["currency"] = currency.clone();
        let now = after_fetch(7200);

        poll_once(
            &scratch.store(),
            &scratch.claude(),
            &PrepaidBody(body),
            &AutoSwitchSettings::default(),
            &|| now,
        )
        .expect("the poll runs");

        let prepaid = &scratch.usage_row(1)["prepaid"];
        assert_eq!(prepaid["error"], error, "currency {currency}: {prepaid}");
        if error.is_null() {
            assert_eq!(prepaid["currency"], currency, "{prepaid}");
        } else {
            assert!(
                prepaid.get("amountMinor").is_none(),
                "a refused answer records no figure: {prepaid}"
            );
        }
    }
}

#[test]
fn read_prepaid_refuses_a_bad_uuid_before_the_transport() {
    let scratch = Scratch::new("read_prepaid_refuses_a_bad_uuid_before_the_transport");
    let mut accounts = scratch.read_json("accounts.json");
    accounts["accounts"]["2"]["orgUuid"] =
        json!("00000000-0000-4000-8000-0000000000b1/../../usage");
    scratch.write("accounts.json", &accounts.to_string());
    // The fixture's credential was sealed for no organization, so it no longer opens: write the account's afresh.
    fs::remove_file(scratch.root().credential_path(2))
        .expect("account 2's fixture credential can be removed");
    write_backup_credentials(
        &scratch,
        2,
        r#"{"claudeAiOauth":{"accessToken":"fake-access-two","refreshToken":"fake-refresh-two","expiresAt":4102444800000}}"#,
    );
    let transport = FakeTransport::with_prepaid(success, funded);
    let now = after_fetch(7200);

    let outcomes = poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    assert!(
        matches!(result_of(&outcomes, 2), PollResult::Fetched { .. }),
        "account 2's usage is read"
    );
    assert_eq!(
        transport.prepaid_calls(),
        vec![("one", ONE_ORG.to_owned())],
        "an organization UUID that is not canonical never reaches the transport"
    );
    assert!(
        scratch.usage_row(2).get("prepaid").is_none(),
        "and nothing is written for it"
    );
}

fn spend_in_float_places(_label: &str) -> Result<RawUsage, TransportError> {
    usage_with_extra(&json!({
        "is_enabled": true,
        "used_credits": 3164.0,
        "monthly_limit": 10_000.0,
        "utilization": 31.64,
        "currency": "USD",
        "decimal_places": 2.0,
    }))
}

#[test]
fn decimal_places_two_point_zero_still_scales_spend() {
    let scratch = Scratch::new("decimal_places_two_point_zero_still_scales_spend");
    let transport = FakeTransport::new(spend_in_float_places);
    let now = after_fetch(7200);

    poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    let spend = &scratch.usage_row(1)["lastGood"]["spend"];
    assert_eq!(
        spend["used"],
        json!(31.64),
        "a decimal_places of 2.0 is 2: {spend}"
    );
    assert_eq!(spend["limit"], json!(100.0), "{spend}");
}

#[test]
fn prepaid_entry_exponent_over_nine_is_none() {
    for (exponent, modelled) in [(9, true), (10, false)] {
        let scratch = Scratch::new(&format!(
            "prepaid_entry_exponent_over_nine_is_none_{exponent}"
        ));
        let mut table = scratch.read_json("cache/usage.json");
        table["accounts"]["1"]["prepaid"] = json!({
            "amountMinor": 102_003, "exponent": exponent, "currency": "USD", "autoReload": null,
            "fetchedAt": FETCHED_AT, "error": null, "attemptedAt": FETCHED_AT,
        });
        scratch.write("cache/usage.json", &table.to_string());

        let entry = UsageTable::entries(&scratch.root(), &identities())
            .remove(&1)
            .expect("account 1 has an entry");

        assert_eq!(
            entry.prepaid.is_some(),
            modelled,
            "exponent {exponent}: {:?}",
            entry.prepaid
        );
    }
}

#[test]
fn prepaid_skipped_when_transport_unsupported() {
    let scratch = Scratch::new("prepaid_skipped_when_transport_unsupported");
    let transport = UsageOnly(FakeTransport::new(success));
    let now = after_fetch(7200);

    let outcomes = poll_once(
        &scratch.store(),
        &scratch.claude(),
        &transport,
        &AutoSwitchSettings::default(),
        &|| now,
    )
    .expect("the poll runs");

    assert!(matches!(
        result_of(&outcomes, 1),
        PollResult::Fetched { .. }
    ));
    assert!(
        scratch.usage_row(1).get("prepaid").is_none(),
        "a transport with no balance request writes no prepaid key"
    );
}
