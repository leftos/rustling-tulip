//! Collecting usage for the auto-switch engine: the start-up warm-up and the scheduled two-phase collection, against a
//! fake usage endpoint and a scratch account store whose account 1 is `accounts.json`'s active account.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::Arc;

use claude_accounts::AutoSwitchSettings;
use claude_accounts::engine::{
    ScheduleInputs, collect_scheduled, collect_warmup, due_candidate, polled_since, warm_settled,
};
use claude_accounts::{
    AccessToken, AccountLock, ClaudePaths, CredentialBlob, DeferReason, Drain, FetchError,
    Identity, LockTiming, PollResult, ProbeVerdicts, ProfileTransport, RawProfile, RawToken,
    RawUsage, RefreshContext, RefreshRequest, RefreshTransport, Store, StoreErrorKind,
    TransportError, UsageEntry, UsageTable, UsageTransport,
};
use serde_json::{Map, Value, json};

use crate::support::{Scratch, at, write};

/// The tests' clock in seconds since the epoch, as the usage table writes times.
const NOW: f64 = 1_790_000_000.0;

fn secs(offset: i32) -> f64 {
    NOW + f64::from(offset)
}

fn now() -> jiff::Timestamp {
    at(0)
}

fn email(number: u32) -> String {
    format!("account{number}@example.test")
}

/// A usage endpoint that answers each account by the number in its fake token, `fake-access-<n>`: a timeout for an
/// account in `failing`, else a reading whose 5-hour window is at the utilization `pct` names (10 by default). It counts
/// the fetches per account.
#[derive(Default)]
struct FakeUsage {
    pct: BTreeMap<u32, f64>,
    failing: BTreeSet<u32>,
    counts: RefCell<BTreeMap<u32, u32>>,
}

impl FakeUsage {
    fn at(pct: &[(u32, f64)]) -> Self {
        Self {
            pct: pct.iter().copied().collect(),
            ..Self::default()
        }
    }

    fn failing(mut self, number: u32) -> Self {
        self.failing.insert(number);
        self
    }

    fn counts(&self) -> BTreeMap<u32, u32> {
        self.counts.borrow().clone()
    }
}

impl UsageTransport for FakeUsage {
    fn get_usage(&self, token: &AccessToken) -> Result<RawUsage, TransportError> {
        let number: u32 = token
            .expose()
            .trim_start_matches("fake-access-")
            .parse()
            .expect("a fake token names its account");
        *self.counts.borrow_mut().entry(number).or_default() += 1;
        if self.failing.contains(&number) {
            return Err(TransportError::Timeout);
        }
        let pct = self.pct.get(&number).copied().unwrap_or(10.0);
        Ok(RawUsage {
            status: 200,
            body: json!({
                "five_hour": { "utilization": pct, "resets_at": null },
                "seven_day": { "utilization": 5.0, "resets_at": null },
            }),
            retry_after: None,
        })
    }
}

/// No credential here is refreshed, so the token endpoint is never reached.
struct NoRefresh;

impl RefreshTransport for NoRefresh {
    fn post_token(&self, _request: &RefreshRequest) -> Result<RawToken, TransportError> {
        Err(TransportError::Connect)
    }
}

/// No account here is logged in, so the profile oracle is never asked.
struct NoProfile;

impl ProfileTransport for NoProfile {
    fn get_profile(&self, _access: &AccessToken) -> Result<RawProfile, TransportError> {
        Err(TransportError::Connect)
    }
}

/// A scratch account store of `accounts` accounts, account 1 active and no login live; each account named in
/// `with_credentials` stores a fresh credential whose access token is `fake-access-<n>`.
struct Bench {
    scratch: Scratch,
    store: Store,
    claude: ClaudePaths,
    drain: Arc<Drain>,
    verdicts: ProbeVerdicts,
}

impl Bench {
    fn new(test: &str, accounts: u32) -> Self {
        Self::with_credentials(test, accounts, &(1..=accounts).collect::<Vec<_>>())
    }

    fn with_credentials(test: &str, accounts: u32, with_credentials: &[u32]) -> Self {
        let scratch = Scratch::new(test);
        let root = scratch.root();
        let mut records = Map::new();
        for number in 1..=accounts {
            let uuid = format!("00000000-0000-4000-8000-0000000000{number:02}");
            records.insert(
                number.to_string(),
                json!({ "email": email(number), "uuid": uuid, "added": "2026-09-01T10:00:00Z" }),
            );
        }
        let order: Vec<u32> = (1..=accounts).collect();
        let sequence = json!({ "active": 1, "order": order, "accounts": records });
        write(&root.accounts_json(), &sequence.to_string());
        let store = Store::open(root.clone()).expect("the scratch store opens");
        let lock = AccountLock::acquire(&root).expect("the account lock is free");
        for &number in with_credentials {
            let credential = json!({ "claudeAiOauth": {
                "accessToken": format!("fake-access-{number}"),
                "refreshToken": format!("fake-refresh-{number}"),
                "expiresAt": 4_102_444_800_000_i64,
            } });
            store
                .write_credentials(&lock, number, &CredentialBlob::new(credential.to_string()))
                .expect("the credential can be written");
        }
        drop(lock);
        let home = root
            .root()
            .parent()
            .expect("the account store has a parent")
            .to_path_buf();
        Self {
            scratch,
            store,
            claude: ClaudePaths::new(home.join("claude"), home.join(".claude.json")),
            drain: Arc::new(Drain::new()),
            verdicts: ProbeVerdicts::new(),
        }
    }

    fn context(&self) -> RefreshContext<'_> {
        RefreshContext {
            store: &self.store,
            claude: &self.claude,
            transport: &NoRefresh,
            drain: &self.drain,
            securestorage_dir_set: false,
            clock: &now,
            profile: &NoProfile,
            verdicts: &self.verdicts,
            claude_locks: LockTiming::CLAUDE,
        }
    }

    /// Writes the usage table with one row per `(account, fields)`: the account's identity, no lease, and `fields`.
    fn rows(&self, rows: &[(u32, Value)]) {
        let root = self.scratch.root();
        fs::create_dir_all(root.cache_dir()).expect("the cache folder can be made");
        let mut accounts = Map::new();
        for (number, fields) in rows {
            let mut row = json!({ "email": email(*number), "organizationUuid": "", "claimId": null, "claimUntil": 0.0 });
            for (key, value) in fields.as_object().expect("the fields are an object") {
                row[key] = value.clone();
            }
            accounts.insert(number.to_string(), row);
        }
        write(
            &root.usage_json(),
            &json!({ "schemaVersion": 3, "accounts": accounts }).to_string(),
        );
    }

    fn entries(&self) -> BTreeMap<u32, UsageEntry> {
        let identities = (1..=3)
            .map(|number| (number, Identity::new(&email(number), "")))
            .collect();
        UsageTable::entries(&self.scratch.root(), &identities)
    }
}

/// A reading whose binding window is the 5-hour one at `pct`.
fn reading(pct: f64) -> Value {
    json!({ "five_hour": { "pct": pct }, "seven_day": { "pct": 5.0 } })
}

/// A row the store held before the engine started: read 30 s ago, with a future 600 s plan.
fn pre_boot(pct: f64) -> Value {
    json!({ "lastGood": reading(pct), "fetchedAt": secs(-30), "lastAttemptAt": secs(-30), "nextPollAt": secs(570), "pollIntervalS": 600.0 })
}

fn with(mut row: Value, fields: &Value) -> Value {
    for (key, value) in fields.as_object().expect("the fields are an object") {
        row[key] = value.clone();
    }
    row
}

fn counts(pairs: &[(u32, u32)]) -> BTreeMap<u32, u32> {
    pairs.iter().copied().collect()
}

/// Account 1's entry read back from a row of `fields`.
fn entry_of(test: &str, fields: &Value) -> UsageEntry {
    let bench = Bench::new(test, 1);
    bench.rows(&[(1, fields.clone())]);
    bench.entries().remove(&1).expect("account 1 has an entry")
}

fn inputs(candidates: &[u32], idle_hold: bool) -> ScheduleInputs<'_> {
    ScheduleInputs {
        current: 1,
        candidates,
        idle_hold,
        threshold: 90.0,
        models: &[],
        paid_mode: false,
    }
}

#[test]
fn warmup_forces_fetch_ignoring_ttl_and_plans() {
    let bench = Bench::new("collect_warmup_forces", 3);
    bench.rows(&[
        (1, pre_boot(10.0)),
        (2, pre_boot(20.0)),
        (3, pre_boot(30.0)),
    ]);
    let usage = FakeUsage::default();

    let warm = collect_warmup(
        &usage,
        &AutoSwitchSettings::default(),
        &bench.context(),
        1,
        &[2, 3],
        now(),
    )
    .expect("the warm-up runs");

    assert_eq!(
        usage.counts(),
        counts(&[(1, 1), (2, 1), (3, 1)]),
        "fresh rows with future plans are fetched"
    );
    for number in 1..=3 {
        assert_eq!(
            warm.collected.entries[&number].fetched_at,
            Some(now()),
            "account {number} recorded its fetch"
        );
    }
    assert_eq!((warm.settled, warm.total), (3, 3));
}

#[test]
fn warmup_respects_backoff_and_counts_it_settled() {
    let bench = Bench::new("collect_warmup_backoff", 3);
    let backing_off = with(
        pre_boot(30.0),
        &json!({ "consecutiveFailures": 1, "lastError": "http-429", "lastAttemptAt": secs(-1), "last429At": secs(-1), "backoffUntil": secs(599) }),
    );
    bench.rows(&[(1, pre_boot(10.0)), (2, pre_boot(20.0)), (3, backing_off)]);
    let usage = FakeUsage::default();

    let warm = collect_warmup(
        &usage,
        &AutoSwitchSettings::default(),
        &bench.context(),
        1,
        &[2, 3],
        now(),
    )
    .expect("the warm-up runs");

    assert_eq!(
        usage.counts(),
        counts(&[(1, 1), (2, 1)]),
        "the backoff is not defeated"
    );
    assert_eq!(
        (warm.settled, warm.total),
        (3, 3),
        "and does not hold the gate"
    );
}

#[test]
fn warmup_counts_failed_attempt_as_settled() {
    let bench = Bench::new("collect_warmup_failed", 3);
    let usage = FakeUsage::default().failing(3);

    let warm = collect_warmup(
        &usage,
        &AutoSwitchSettings::default(),
        &bench.context(),
        1,
        &[2, 3],
        now(),
    )
    .expect("the warm-up runs");

    assert_eq!(usage.counts(), counts(&[(1, 1), (2, 1), (3, 1)]));
    assert_eq!(
        warm.collected.results[&3],
        PollResult::Failed(FetchError::Timeout)
    );
    assert_eq!((warm.settled, warm.total), (3, 3));

    let since = at(-10_000);
    let failed = json!({ "consecutiveFailures": 1, "lastAttemptAt": secs(-5), "claimUntil": 0.0 });
    let entry = entry_of("collect_settled_failed", &failed);
    assert!(
        warm_settled(Some(&entry), None, since, now()),
        "a concluded failure since the start settles"
    );
    let claimed = entry_of(
        "collect_settled_claimed",
        &with(failed.clone(), &json!({ "claimUntil": secs(60) })),
    );
    assert!(
        !warm_settled(Some(&claimed), None, since, now()),
        "a live claim is a concurrent fetch, not a result"
    );
    let earlier = entry_of(
        "collect_settled_earlier",
        &with(failed, &json!({ "lastAttemptAt": secs(-10_001) })),
    );
    assert!(
        !warm_settled(Some(&earlier), None, since, now()),
        "a failure before the start is not a poll since"
    );
}

#[test]
fn warmup_counts_sentinel_accounts_settled() {
    let bench = Bench::with_credentials("collect_warmup_sentinel", 3, &[1, 2]);
    let usage = FakeUsage::default();

    let warm = collect_warmup(
        &usage,
        &AutoSwitchSettings::default(),
        &bench.context(),
        1,
        &[2, 3],
        now(),
    )
    .expect("the warm-up runs");

    assert_eq!(usage.counts(), counts(&[(1, 1), (2, 1)]));
    assert_eq!(warm.collected.results[&3], PollResult::NoCredentials);
    assert_eq!(
        (warm.settled, warm.total),
        (3, 3),
        "an account with nothing to fetch does not hold the gate"
    );

    let never = UsageEntry::default();
    let sentinels = [
        PollResult::Disabled,
        PollResult::NoCredentials,
        PollResult::CredentialsUnreadable(StoreErrorKind::RootUnreadable),
        PollResult::TokenExpired,
        PollResult::AuthDead,
        PollResult::ForeignCredential,
        PollResult::Deferred(DeferReason::LocksBusy),
        PollResult::LiveWriteFailed,
    ];
    for result in sentinels {
        assert!(
            warm_settled(Some(&never), Some(&result), now(), now()),
            "{result:?} settles"
        );
    }
    for result in [
        PollResult::NotDue,
        PollResult::Fenced,
        PollResult::Draining,
        PollResult::ConsumeBusy,
    ] {
        assert!(
            !warm_settled(Some(&never), Some(&result), now(), now()),
            "{result:?} does not settle"
        );
    }
    assert!(
        warm_settled(None, None, now(), now()),
        "an account with no entry has nothing to wait for"
    );
}

#[test]
fn warmup_reports_progress_while_a_concurrent_fetch_runs() {
    let bench = Bench::new("collect_warmup_progress", 3);
    let claimed = with(
        pre_boot(20.0),
        &json!({ "claimId": "other", "claimUntil": secs(60), "lastAttemptAt": secs(-1) }),
    );
    bench.rows(&[(1, pre_boot(95.0)), (2, pre_boot(10.0)), (3, claimed)]);
    let usage = FakeUsage::default();

    let warm = collect_warmup(
        &usage,
        &AutoSwitchSettings::default(),
        &bench.context(),
        1,
        &[2, 3],
        now(),
    )
    .expect("the warm-up runs");

    assert_eq!(
        usage.counts(),
        counts(&[(1, 1), (2, 1)]),
        "the claimed account is left to its collector"
    );
    assert_eq!((warm.settled, warm.total), (2, 3));
}

#[test]
fn due_candidate_skips_dead_token() {
    let bench = Bench::new("collect_due_dead", 1);
    let struck = json!({
        "fetchedAt": secs(-10_000),
        "consecutiveFailures": 2,
        "lastError": "invalid_grant",
        "backoffUntil": secs(-5000),
        "authDeadStrikes": 2,
    });
    bench.rows(&[(1, struck)]);
    let entries = bench.entries();

    let dead = BTreeMap::from([(1, PollResult::AuthDead)]);
    assert_eq!(
        due_candidate(&[1], &entries, &dead, now()),
        None,
        "a dead token is never nominated"
    );
    assert_eq!(
        due_candidate(&[1], &entries, &BTreeMap::new(), now()),
        Some(1),
        "the row alone is due"
    );
}

#[test]
fn due_candidate_skips_a_held_slot() {
    let bench = Bench::new("collect_due_held", 2);
    let held = json!({ "lastGood": reading(10.0), "fetchedAt": secs(-61), "heldUntil": secs(539), "nextPollAt": secs(-1), "pollIntervalS": 60.0 });
    bench.rows(&[(1, held.clone()), (2, held)]);

    assert_eq!(
        due_candidate(&[1, 2], &bench.entries(), &BTreeMap::new(), now()),
        None
    );
}

#[test]
fn due_candidate_prefers_never_fetched_then_oldest() {
    let bench = Bench::new("collect_due_order", 3);
    bench.rows(&[
        (1, json!({ "fetchedAt": secs(-500) })),
        (2, json!({ "fetchedAt": secs(-900) })),
    ]);
    let entries = bench.entries();
    let none = BTreeMap::new();

    assert_eq!(
        due_candidate(&[1, 2, 3], &entries, &none, now()),
        Some(3),
        "never fetched first"
    );
    assert_eq!(
        due_candidate(&[1, 2], &entries, &none, now()),
        Some(2),
        "then the oldest fetch"
    );
    assert_eq!(
        due_candidate(&[10, 9], &BTreeMap::new(), &none, now()),
        Some(9),
        "ties go to the lower number"
    );
}

#[test]
fn polled_since_counts_a_success_after_the_start() {
    let after = entry_of("collect_polled_success", &json!({ "fetchedAt": secs(5) }));
    let before = entry_of("collect_polled_before", &json!({ "fetchedAt": secs(-5) }));

    assert!(polled_since(Some(&after), now(), at(10_000)));
    assert!(!polled_since(Some(&before), now(), at(10_000)));
    assert!(!polled_since(None, now(), at(10_000)));
}

#[test]
fn polled_since_counts_a_failure_after_the_start_with_no_live_claim() {
    let failed = entry_of(
        "collect_polled_failure",
        &json!({ "consecutiveFailures": 1, "lastAttemptAt": secs(5), "claimUntil": 0.0 }),
    );

    assert!(polled_since(Some(&failed), now(), at(10_000)));
}

#[test]
fn polled_since_skips_a_failure_under_a_live_claim() {
    let claimed = entry_of(
        "collect_polled_claimed",
        &json!({ "consecutiveFailures": 1, "lastAttemptAt": secs(5), "claimUntil": secs(60) }),
    );

    assert!(!polled_since(Some(&claimed), now(), at(10_000)));
}

#[test]
fn near_threshold_escalates_to_full_refresh() {
    let bench = Bench::new("collect_near_threshold", 3);
    let usage = FakeUsage::at(&[(1, 80.0), (2, 10.0), (3, 20.0)]);

    collect_scheduled(
        &usage,
        &AutoSwitchSettings::default(),
        &bench.context(),
        &inputs(&[2, 3], false),
    )
    .expect("the collection runs");

    assert_eq!(
        usage.counts(),
        counts(&[(1, 1), (2, 1), (3, 1)]),
        "everyone got refreshed, once"
    );
}

#[test]
fn active_unknown_escalates_before_failover() {
    let bench = Bench::new("collect_active_unknown", 3);
    let usage = FakeUsage::at(&[(2, 10.0), (3, 50.0)]).failing(1);

    let collected = collect_scheduled(
        &usage,
        &AutoSwitchSettings::default(),
        &bench.context(),
        &inputs(&[2, 3], false),
    )
    .expect("the collection runs");

    assert_eq!(
        usage.counts(),
        counts(&[(1, 1), (2, 1), (3, 1)]),
        "candidate data was refreshed"
    );
    assert!(
        matches!(collected.results[&3], PollResult::Fetched { .. }),
        "the escalation's result stands: {:?}",
        collected.results[&3]
    );
}

#[test]
fn stale_candidate_plan_never_gates_the_active() {
    let bench = Bench::new("collect_stale_candidate_plan", 2);
    let candidate = json!({ "lastGood": reading(20.0), "fetchedAt": secs(-30), "nextPollAt": secs(570), "pollIntervalS": 600.0 });
    let usage = FakeUsage::at(&[(1, 50.0)]);

    let inside_the_cap = json!({ "lastGood": reading(50.0), "fetchedAt": secs(-240), "nextPollAt": secs(360), "pollIntervalS": 600.0 });
    bench.rows(&[(1, inside_the_cap), (2, candidate.clone())]);
    collect_scheduled(
        &usage,
        &AutoSwitchSettings::default(),
        &bench.context(),
        &inputs(&[2], false),
    )
    .expect("the collection runs");
    assert_eq!(
        usage.counts(),
        counts(&[]),
        "inside the active age cap the plan stands"
    );

    let past_the_cap = json!({ "lastGood": reading(50.0), "fetchedAt": secs(-360), "nextPollAt": secs(240), "pollIntervalS": 600.0 });
    bench.rows(&[(1, past_the_cap), (2, candidate)]);
    collect_scheduled(
        &usage,
        &AutoSwitchSettings::default(),
        &bench.context(),
        &inputs(&[2], false),
    )
    .expect("the collection runs");
    assert_eq!(
        usage.counts(),
        counts(&[(1, 1)]),
        "past the cap the candidate-cadence plan is overridden"
    );
}

#[test]
fn all_exhausted_escalation_preserves_wider_plan() {
    let bench = Bench::new("collect_all_exhausted", 3);
    let due = json!({ "lastGood": reading(100.0), "fetchedAt": secs(-700), "nextPollAt": secs(-1), "pollIntervalS": 600.0 });
    let wide = json!({ "lastGood": reading(100.0), "fetchedAt": secs(-700), "nextPollAt": secs(1100), "pollIntervalS": 1800.0 });
    bench.rows(&[(1, due.clone()), (2, wide), (3, due)]);
    let usage = FakeUsage::at(&[(1, 100.0), (2, 100.0), (3, 100.0)]);

    collect_scheduled(
        &usage,
        &AutoSwitchSettings::default(),
        &bench.context(),
        &inputs(&[2, 3], false),
    )
    .expect("the collection runs");

    assert_eq!(
        usage.counts(),
        counts(&[(1, 1), (3, 1)]),
        "the wider post-429 plan is not defeated"
    );
}

#[test]
fn idle_hold_polls_no_candidate() {
    let bench = Bench::new("collect_idle_hold", 3);
    let usage = FakeUsage::at(&[(1, 10.0)]);

    collect_scheduled(
        &usage,
        &AutoSwitchSettings::default(),
        &bench.context(),
        &inputs(&[2, 3], true),
    )
    .expect("the collection runs");

    assert_eq!(
        usage.counts(),
        counts(&[(1, 1)]),
        "only the active account is polled"
    );
}

#[test]
fn warmup_leaves_accounts_polled_since_the_start_alone() {
    let bench = Bench::new("collect_warmup_polled", 3);
    bench.rows(&[
        (1, pre_boot(10.0)),
        (2, pre_boot(20.0)),
        (3, pre_boot(30.0)),
    ]);
    let usage = FakeUsage::default();

    let warm = collect_warmup(
        &usage,
        &AutoSwitchSettings::default(),
        &bench.context(),
        1,
        &[2, 3],
        at(-60_000_000),
    )
    .expect("the warm-up runs");

    assert_eq!(
        usage.counts(),
        counts(&[]),
        "a reading fetched since the start is not fetched again"
    );
    assert_eq!((warm.settled, warm.total), (3, 3));
}

#[test]
fn expired_logged_in_active_does_not_escalate() {
    let bench = Bench::new("collect_expired_active", 3);
    fs::write(
        bench.claude.global_config(),
        json!({ "oauthAccount": { "emailAddress": email(1) } }).to_string(),
    )
    .expect("the global config can be written");
    fs::create_dir_all(
        bench
            .claude
            .credentials_file()
            .parent()
            .expect("the live file has a folder"),
    )
    .expect("Claude Code's folder can be made");
    let expired = json!({ "claudeAiOauth": { "accessToken": "fake-access-1", "refreshToken": "fake-refresh-1", "expiresAt": 1000 } });
    fs::write(bench.claude.credentials_file(), expired.to_string())
        .expect("the live credential can be written");
    let stale_with_plan = json!({ "lastGood": reading(10.0), "fetchedAt": secs(-1000), "nextPollAt": secs(600), "pollIntervalS": 600.0 });
    bench.rows(&[
        (1, pre_boot(50.0)),
        (2, stale_with_plan.clone()),
        (3, stale_with_plan),
    ]);
    let usage = FakeUsage::default();

    let collected = collect_scheduled(
        &usage,
        &AutoSwitchSettings::default(),
        &bench.context(),
        &inputs(&[2, 3], false),
    )
    .expect("the collection runs");

    assert_eq!(collected.results[&1], PollResult::TokenExpired);
    assert_eq!(
        usage.counts(),
        counts(&[]),
        "an expired active token idle-holds instead of refreshing every candidate"
    );
}

#[test]
fn escalation_fetches_a_stale_candidate_with_a_valid_plan() {
    let bench = Bench::new("collect_escalation_mode", 2);
    let stale_with_plan = json!({ "lastGood": reading(10.0), "fetchedAt": secs(-1000), "nextPollAt": secs(600), "pollIntervalS": 600.0 });
    bench.rows(&[(2, stale_with_plan)]);
    let usage = FakeUsage::at(&[(1, 80.0)]);

    collect_scheduled(
        &usage,
        &AutoSwitchSettings::default(),
        &bench.context(),
        &inputs(&[2], false),
    )
    .expect("the collection runs");

    assert_eq!(
        usage.counts(),
        counts(&[(1, 1), (2, 1)]),
        "phase B fetches the stale candidate past its plan"
    );
}
