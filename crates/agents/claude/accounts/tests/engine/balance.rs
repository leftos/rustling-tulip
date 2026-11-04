//! The engine's balance-only read, [`claude_accounts::engine::TransportCollector`]'s `read_balance`: written only into
//! the account's own usage row under `cache/.usage.lock`, never over a newer reading, a failure recorded as the balance's
//! own error alone, and no usage request or grant spent.

use std::cell::{Cell, RefCell};

use claude_accounts::{
    AccessToken, BalanceReadError, FetchError, PollError, PollResult, ProfileTransport, RawProfile,
    RawUsage, TransportError, UsageTransport,
};
use serde_json::{Value, json};

use crate::support::{NOW_S, read_json, write};
use crate::tick::Bench;

/// An `expiresAt` far in the future, in milliseconds.
const FAR_MS: i64 = 4_102_444_800_000;

/// Account 2's organization.
const ORG_TWO: &str = "00000000-0000-4000-8000-0000000000b2";
/// Account 3's organization.
const ORG_THREE: &str = "00000000-0000-4000-8000-0000000000b3";

type Answer = fn() -> Result<RawUsage, TransportError>;

/// A usage endpoint that answers every balance request with `prepaid`, recording each one, and counts usage requests.
struct Endpoint {
    prepaid: Option<Answer>,
    usage_calls: Cell<usize>,
    prepaid_calls: RefCell<Vec<String>>,
}

impl Endpoint {
    fn answering(prepaid: Answer) -> Self {
        Self {
            prepaid: Some(prepaid),
            usage_calls: Cell::new(0),
            prepaid_calls: RefCell::new(Vec::new()),
        }
    }

    /// An endpoint that keeps the trait's default for the balance request.
    fn without_prepaid() -> Self {
        Self {
            prepaid: None,
            ..Self::answering(funded)
        }
    }
}

impl UsageTransport for Endpoint {
    fn get_usage(&self, _token: &AccessToken) -> Result<RawUsage, TransportError> {
        self.usage_calls.set(self.usage_calls.get() + 1);
        Err(TransportError::Connect)
    }

    fn get_prepaid(
        &self,
        _token: &AccessToken,
        org_uuid: &str,
    ) -> Result<RawUsage, TransportError> {
        self.prepaid_calls.borrow_mut().push(org_uuid.to_owned());
        match self.prepaid {
            Some(answer) => answer(),
            None => Err(TransportError::Unsupported),
        }
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "it is a fake transport's answer, which returns a Result"
)]
fn funded() -> Result<RawUsage, TransportError> {
    Ok(RawUsage {
        status: 200,
        body: json!({"amount": 102_003, "currency": "USD", "auto_reload_settings": {"enabled": false}}),
        retry_after: None,
    })
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "it is a fake transport's answer, which returns a Result"
)]
fn rate_limited() -> Result<RawUsage, TransportError> {
    Ok(RawUsage {
        status: 429,
        body: json!({"error": {"type": "rate_limit_error"}}),
        retry_after: Some(120.0),
    })
}

/// The tests' clock, in seconds since the epoch.
fn now_s() -> f64 {
    f64::from(i32::try_from(NOW_S).expect("the test clock fits an i32"))
}

/// A bench of three accounts, 2 and 3 in organizations of their own, with the usage table `rows`.
fn bench(test: &str, rows: &Value) -> Bench {
    let bench = Bench::new(&format!("balance_{test}"), 3);
    bench.set_account(2, "orgUuid", json!(ORG_TWO));
    bench.set_account(3, "orgUuid", json!(ORG_THREE));
    bench.reseal_credential(2);
    bench.reseal_credential(3);
    std::fs::create_dir_all(bench.root().cache_dir()).expect("the cache folder can be made");
    write(
        &bench.root().usage_json(),
        &json!({"schemaVersion": 3, "accounts": rows}).to_string(),
    );
    bench
}

fn table(bench: &Bench) -> Value {
    read_json(&bench.root().usage_json())
}

/// Account 2's own row with a reading fetched 30 s ago, and `prepaid` when given.
fn row_two(prepaid: Option<Value>) -> Value {
    let mut row = json!({
        "email": "account2@example.test",
        "organizationUuid": ORG_TWO,
        "lastGood": {"five_hour": {"pct": 100.0}, "seven_day": {"pct": 40.0}},
        "fetchedAt": now_s() - 30.0,
        "consecutiveFailures": 2,
        "lastError": "timeout",
        "backoffUntil": now_s() + 300.0,
        "nextPollAt": now_s() + 600.0,
    });
    if let Some(prepaid) = prepaid {
        row["prepaid"] = prepaid;
    }
    row
}

/// A row in account 3's slot naming another login.
fn foreign_row_three() -> Value {
    json!({"email": "someone-else@example.test", "organizationUuid": ORG_THREE, "fetchedAt": now_s() - 30.0})
}

#[test]
fn a_balance_is_written_only_to_its_own_row_and_never_goes_back() {
    let bench = bench(
        "own_row_only",
        &json!({"2": row_two(None), "3": foreign_row_three()}),
    );
    let endpoint = Endpoint::answering(funded);

    let entry = bench
        .read_balance(&endpoint, 2, ORG_TWO)
        .expect("account 2's balance is read");

    assert_eq!(entry.amount_minor, Some(102_003));
    assert_eq!(entry.exponent, Some(2));
    assert_eq!(entry.currency.as_deref(), Some("USD"));
    assert_eq!(entry.fetched_at, Some(bench.now()));
    let written = table(&bench);
    let two = &written["accounts"]["2"];
    assert_eq!(two["prepaid"]["amountMinor"], json!(102_003));
    assert_eq!(two["prepaid"]["fetchedAt"], json!(now_s()));
    let mut usage_part = two.clone();
    usage_part
        .as_object_mut()
        .expect("the row is an object")
        .shift_remove("prepaid");
    assert_eq!(
        usage_part,
        row_two(None),
        "the usage reading and its failure state are left alone"
    );
    assert_eq!(written["accounts"]["3"], foreign_row_three());

    let error = bench
        .read_balance(&endpoint, 3, ORG_THREE)
        .expect_err("account 3's slot holds another login's row");
    assert!(matches!(error, BalanceReadError::NoRow), "{error:?}");
    assert_eq!(
        table(&bench)["accounts"]["3"],
        foreign_row_three(),
        "another login's row is never written"
    );

    let error = bench
        .read_balance(&endpoint, 2, ORG_THREE)
        .expect_err("account 3's organization is not account 2's");
    assert!(matches!(error, BalanceReadError::Organization), "{error:?}");
    let error = bench
        .read_balance(&endpoint, 9, ORG_TWO)
        .expect_err("no account 9");
    assert!(
        matches!(error, BalanceReadError::UnknownAccount),
        "{error:?}"
    );
    assert_eq!(
        endpoint.prepaid_calls.borrow().as_slice(),
        [ORG_TWO, ORG_THREE],
        "a refused organization or account sends nothing"
    );

    let mut later = table(&bench);
    later["accounts"]["2"]["prepaid"]["amountMinor"] = json!(500);
    later["accounts"]["2"]["prepaid"]["fetchedAt"] = json!(now_s() + 30.0);
    write(&bench.root().usage_json(), &later.to_string());
    let entry = bench
        .read_balance(&endpoint, 2, ORG_TWO)
        .expect("account 2's balance is read");
    assert_eq!(
        entry.amount_minor,
        Some(500),
        "a later read another collector recorded wins"
    );
    assert_eq!(
        table(&bench)["accounts"]["2"]["prepaid"]["amountMinor"],
        json!(500),
        "a balance never goes back"
    );
}

/// A stored balance dated more than 60 s after the read (another machine's clock) counts as older than any real read,
/// so the new read replaces it; one within 60 s is a later read and stays.
#[test]
fn a_future_dated_balance_does_not_block_a_new_read() {
    let ahead = |seconds: f64| {
        json!({
            "amountMinor": 500, "exponent": 2, "currency": "USD", "autoReload": false,
            "fetchedAt": now_s() + seconds, "error": null, "attemptedAt": now_s() + seconds,
        })
    };
    let bench = bench("future_dated", &json!({"2": row_two(Some(ahead(61.0)))}));
    let endpoint = Endpoint::answering(funded);

    let entry = bench
        .read_balance(&endpoint, 2, ORG_TWO)
        .expect("account 2's balance is read");

    assert_eq!(
        entry.amount_minor,
        Some(102_003),
        "a balance dated more than 60 s ahead does not block the read"
    );
    assert_eq!(
        table(&bench)["accounts"]["2"]["prepaid"]["fetchedAt"],
        json!(now_s())
    );

    write(
        &bench.root().usage_json(),
        &json!({"schemaVersion": 3, "accounts": {"2": row_two(Some(ahead(60.0)))}}).to_string(),
    );
    let entry = bench
        .read_balance(&endpoint, 2, ORG_TWO)
        .expect("account 2's balance is read");
    assert_eq!(
        entry.amount_minor,
        Some(500),
        "a balance at most 60 s ahead is a later read"
    );
}

/// A profile endpoint that names another login for every token.
struct Stranger;

impl ProfileTransport for Stranger {
    fn get_profile(&self, _access: &AccessToken) -> Result<RawProfile, TransportError> {
        Ok(RawProfile {
            status: 200,
            body: json!({
                "account": {"uuid": "00000000-0000-4000-8000-0000000000ff", "email": "someone-else@example.test"},
                "organization": {"uuid": ORG_TWO},
            }),
        })
    }
}

/// A usage endpoint that answers every usage request with a reading and reads no balance.
struct Reading;

impl UsageTransport for Reading {
    fn get_usage(&self, _token: &AccessToken) -> Result<RawUsage, TransportError> {
        Ok(RawUsage {
            status: 200,
            body: json!({"five_hour": {"utilization": 30.0, "resets_at": null}, "seven_day": {"utilization": 12.0, "resets_at": null}}),
            retry_after: None,
        })
    }
}

/// The live credential's lineage, which the oracle named as another login's, refuses a balance read with its token
/// before anything is sent: no prepaid request, nothing written.
#[test]
fn a_balance_read_refuses_a_foreign_live_credential() {
    let bench = bench("foreign_live", &json!({}));
    let live = json!({"claudeAiOauth": {
        "accessToken": "fake-access-2", "refreshToken": "fake-refresh-elsewhere", "expiresAt": FAR_MS,
    }});
    bench.set_live(2, ORG_TWO, &live.to_string());
    let outcomes = bench.poll_with_profile(&Reading, &Stranger);
    let two = outcomes
        .iter()
        .find(|outcome| outcome.number == 2)
        .expect("account 2 is polled");
    assert!(
        matches!(two.result, PollResult::ForeignCredential),
        "the oracle names the live credential another login's: {:?}",
        two.result
    );
    write(
        &bench.root().usage_json(),
        &json!({"schemaVersion": 3, "accounts": {"2": row_two(None)}}).to_string(),
    );
    let before = table(&bench);
    let endpoint = Endpoint::answering(funded);

    let error = bench
        .read_balance(&endpoint, 2, ORG_TWO)
        .expect_err("the live token is another login's");

    assert!(
        matches!(error, BalanceReadError::ForeignCredential),
        "{error:?}"
    );
    assert!(
        endpoint.prepaid_calls.borrow().is_empty(),
        "no prepaid request is sent"
    );
    assert_eq!(table(&bench), before, "nothing is written");
}

#[test]
fn a_balance_failure_leaves_the_usage_backoff_alone() {
    let fetched = now_s() - 400.0;
    let prepaid = json!({
        "amountMinor": 500, "exponent": 2, "currency": "USD", "autoReload": false,
        "fetchedAt": fetched, "error": null, "attemptedAt": fetched,
    });
    let bench = bench(
        "failure_alone",
        &json!({"2": row_two(Some(prepaid.clone()))}),
    );
    let endpoint = Endpoint::answering(rate_limited);

    let error = bench
        .read_balance(&endpoint, 2, ORG_TWO)
        .expect_err("a 429 is a failed read");

    assert!(
        matches!(
            error,
            BalanceReadError::Failed(FetchError::Http { status: 429, .. })
        ),
        "{error:?}"
    );
    let two = table(&bench)["accounts"]["2"].clone();
    let mut expected = row_two(Some(prepaid));
    expected["prepaid"]["error"] = json!("http-429");
    expected["prepaid"]["attemptedAt"] = json!(now_s());
    assert_eq!(
        two, expected,
        "only the balance's own error and attempt time change"
    );
}

#[test]
fn a_balance_read_never_spends_a_grant() {
    let bench = bench("no_grant", &json!({"2": row_two(None)}));
    let endpoint = Endpoint::answering(funded);

    bench
        .read_balance(&endpoint, 2, ORG_TWO)
        .expect("account 2's balance is read");
    let failing = Endpoint::answering(rate_limited);
    bench
        .read_balance(&failing, 2, ORG_TWO)
        .expect_err("a 429 is a failed read");

    assert_eq!(
        endpoint.usage_calls.get() + failing.usage_calls.get(),
        0,
        "no usage request"
    );
    assert_eq!(bench.token_posts(), 0, "no refresh grant");

    let before = table(&bench);
    let unsupported = Endpoint::without_prepaid();
    let error = bench
        .read_balance(&unsupported, 2, ORG_TWO)
        .expect_err("the transport reads no balance");
    assert!(matches!(error, BalanceReadError::Unsupported), "{error:?}");
    let error = bench
        .read_balance_by_default(2, ORG_TWO)
        .expect_err("a collector that reads no balance");
    assert!(matches!(error, BalanceReadError::Unsupported), "{error:?}");
    assert_eq!(table(&bench), before, "an unsupported read writes nothing");
    assert_eq!(unsupported.usage_calls.get(), 0);
    assert_eq!(bench.token_posts(), 0);
}

/// A table another writer turned version 4 keeps its bytes: the balance read refuses it with
/// [`PollError::UnsupportedCache`] and writes nothing into a table this crate does not understand.
#[test]
fn a_table_turned_version_4_is_left_untouched_by_the_balance_read() {
    let bench = bench("turned_version_4", &json!({"2": row_two(None)}));
    let endpoint = Endpoint::answering(funded);
    let path = bench.root().usage_json();
    let mut document = read_json(&path);
    document["schemaVersion"] = json!(4);
    write(&path, &document.to_string());
    let before = std::fs::read(&path).expect("the usage table reads");

    let error = bench
        .read_balance(&endpoint, 2, ORG_TWO)
        .expect_err("a version 4 table is never written");

    assert!(
        matches!(error, BalanceReadError::Cache(PollError::UnsupportedCache)),
        "{error:?}"
    );
    assert_eq!(
        std::fs::read(&path).expect("the usage table reads"),
        before,
        "a version 4 table keeps its bytes"
    );
}
