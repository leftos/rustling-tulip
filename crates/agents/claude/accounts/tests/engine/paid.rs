//! The paid fallback's eligibility test, [`paid_eligible`], one test per clause, and its ranking, [`paid_ranking`].

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use claude_accounts::engine::{
    Ineligible, PAID_BALANCE_MAX_AGE_S, PaidBalance, PaidCandidate, paid_eligible, paid_ranking,
};
use claude_accounts::{AccountRecord, PrepaidEntry, Store};
use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};

use crate::support::{Scratch, at, write};

/// Account 1 as `accounts.json` holds it: an opted-in OAuth account, with `edit` applied to its record.
fn record(test: &str, edit: impl FnOnce(&mut serde_json::Map<String, Value>)) -> AccountRecord {
    let scratch = Scratch::new(&format!("paid_{test}"));
    let root = scratch.root();
    let mut account = json!({
        "email": "one@example.test",
        "uuid": "00000000-0000-4000-8000-000000000001",
        "added": "2026-09-01T10:00:00Z",
        "maySpendCredits": true,
    });
    edit(account.as_object_mut().expect("the record is an object"));
    write(
        &root.accounts_json(),
        &json!({"active": 1, "order": [1], "accounts": {"1": account}}).to_string(),
    );
    let store = Store::open(root).expect("the scratch store opens");
    let sequence = store
        .read_sequence()
        .expect("the roster reads")
        .expect("the roster exists");
    sequence.account(1).expect("account 1 is in the roster")
}

fn opted_in(test: &str) -> AccountRecord {
    record(test, |_| {})
}

/// A trusted reading at its limit with extra usage on, with `edit` applied to its `extra_usage` flags.
fn reading(edit: impl FnOnce(&mut serde_json::Map<String, Value>)) -> Value {
    let mut extra = json!({"enabled": true, "user_disabled": false, "spend_limit_reached": false, "disabled_reason_set": false});
    edit(extra.as_object_mut().expect("the flags are an object"));
    json!({"five_hour": {"pct": 100.0}, "seven_day": {"pct": 40.0}, "extra_usage": extra})
}

fn extra_usage_on() -> Value {
    reading(|_| {})
}

fn now() -> Timestamp {
    at(0)
}

/// A balance of `amount_minor` hundredths of `currency`, read `age_s` seconds before [`now`].
fn prepaid(amount_minor: i64, currency: Option<&str>, age_s: i64) -> PrepaidEntry {
    let mut entry = PrepaidEntry::default();
    entry.amount_minor = Some(amount_minor);
    entry.exponent = Some(2);
    entry.currency = currency.map(str::to_owned);
    entry.auto_reload = Some(false);
    entry.fetched_at = Some(now() - SignedDuration::from_secs(age_s));
    entry.attempted_at = entry.fetched_at;
    entry
}

fn funded() -> PrepaidEntry {
    prepaid(102_003, Some("USD"), 60)
}

fn judge(
    record: &AccountRecord,
    reading: Option<&Value>,
    prepaid: Option<&PrepaidEntry>,
    available: bool,
) -> Result<PaidBalance, Ineligible> {
    paid_eligible(
        &PaidCandidate {
            record,
            reading,
            prepaid,
            available,
        },
        now(),
    )
}

fn balance(amount: f64, currency: &str) -> PaidBalance {
    PaidBalance {
        amount,
        currency: currency.to_owned(),
        age_s: 0.0,
    }
}

#[test]
fn paid_target_is_the_largest_balance_then_rotation_order() {
    let candidates = [
        (1, balance(50.0, "USD")),
        (2, balance(1020.03, "USD")),
        (3, balance(50.0, "USD")),
        (4, balance(1020.03, "USD")),
    ];

    assert_eq!(
        paid_ranking(&candidates, &[3, 4, 1, 2]),
        [4, 2, 3, 1],
        "largest first, ties in rotation order"
    );
    assert_eq!(paid_ranking(&candidates, &[1, 2, 3, 4]), [2, 4, 1, 3]);
    assert_eq!(
        paid_ranking(
            &[(1, balance(5.0, "USD")), (2, balance(9.0, "EUR"))],
            &[1, 2]
        ),
        [2, 1],
        "different currencies compare in major units as they are"
    );
    assert_eq!(paid_ranking(&[], &[1, 2]), Vec::<u32>::new());
}

#[test]
fn eligible_with_auto_reload_on() {
    let record = opted_in("eligible_with_auto_reload_on");
    let reading = extra_usage_on();
    let mut prepaid = funded();
    prepaid.auto_reload = Some(true);

    let balance = judge(&record, Some(&reading), Some(&prepaid), true)
        .expect("auto-reload never excludes an opted-in account");

    assert!(
        (balance.amount - 1020.03).abs() < 1e-9,
        "major units: {balance:?}"
    );
    assert_eq!(balance.currency, "USD");
    assert!(
        (balance.age_s - 60.0).abs() < 1e-9,
        "the balance's own age: {balance:?}"
    );
}

#[test]
fn ineligible_when_flag_off() {
    let reading = extra_usage_on();
    for (case, flag) in [
        ("absent", None),
        ("false", Some(json!(false))),
        ("one", Some(json!(1))),
    ] {
        let record = record(
            &format!("ineligible_when_flag_off_{case}"),
            |account| match flag {
                Some(flag) => {
                    account.insert("maySpendCredits".to_owned(), flag);
                }
                None => {
                    account.shift_remove("maySpendCredits");
                }
            },
        );
        assert_eq!(
            judge(&record, Some(&reading), Some(&funded()), true),
            Err(Ineligible::FlagOff),
            "{case}"
        );
    }
}

#[test]
fn ineligible_when_disabled() {
    let record = record("ineligible_when_disabled", |account| {
        account.insert("disabled".to_owned(), json!(true));
    });
    let reading = extra_usage_on();

    assert_eq!(
        judge(&record, Some(&reading), Some(&funded()), true),
        Err(Ineligible::Disabled)
    );
}

#[test]
fn ineligible_when_not_switchable() {
    let account = opted_in("ineligible_when_not_switchable");
    let reading = extra_usage_on();

    assert_eq!(
        judge(&account, Some(&reading), Some(&funded()), false),
        Err(Ineligible::NotSwitchable)
    );

    let api_key = record("ineligible_when_not_switchable_api_key", |account| {
        account.insert("kind".to_owned(), json!("api_key"));
    });
    assert_eq!(
        judge(&api_key, Some(&reading), Some(&funded()), true),
        Err(Ineligible::NotOauth)
    );
}

#[test]
fn ineligible_when_extra_usage_off() {
    let record = opted_in("ineligible_when_extra_usage_off");
    let reading = reading(|extra| {
        extra.insert("enabled".to_owned(), json!(false));
    });

    assert_eq!(
        judge(&record, Some(&reading), Some(&funded()), true),
        Err(Ineligible::ExtraUsageOff)
    );
}

#[test]
fn ineligible_when_user_disabled() {
    let record = opted_in("ineligible_when_user_disabled");
    let reading = reading(|extra| {
        extra.insert("user_disabled".to_owned(), json!(true));
    });

    assert_eq!(
        judge(&record, Some(&reading), Some(&funded()), true),
        Err(Ineligible::UserDisabled)
    );
}

#[test]
fn ineligible_when_spend_limit_reached() {
    let record = opted_in("ineligible_when_spend_limit_reached");
    let reading = reading(|extra| {
        extra.insert("spend_limit_reached".to_owned(), json!(true));
    });

    assert_eq!(
        judge(&record, Some(&reading), Some(&funded()), true),
        Err(Ineligible::SpendLimitReached)
    );
}

#[test]
fn ineligible_when_disabled_reason_set() {
    let record = opted_in("ineligible_when_disabled_reason_set");
    let reading = reading(|extra| {
        extra.insert("disabled_reason_set".to_owned(), json!(true));
    });

    assert_eq!(
        judge(&record, Some(&reading), Some(&funded()), true),
        Err(Ineligible::DisabledReasonSet)
    );
}

#[test]
fn ineligible_when_extra_usage_fields_missing() {
    let record = opted_in("ineligible_when_extra_usage_fields_missing");
    let prepaid = funded();
    for key in [
        "enabled",
        "user_disabled",
        "spend_limit_reached",
        "disabled_reason_set",
    ] {
        let reading = reading(|extra| {
            extra.shift_remove(key);
        });
        assert_eq!(
            judge(&record, Some(&reading), Some(&prepaid), true),
            Err(Ineligible::ExtraUsageUnknown),
            "{key}"
        );
    }
    let no_object = json!({"five_hour": {"pct": 100.0}});
    assert_eq!(
        judge(&record, Some(&no_object), Some(&prepaid), true),
        Err(Ineligible::ExtraUsageUnknown)
    );
    assert_eq!(
        judge(&record, None, Some(&prepaid), true),
        Err(Ineligible::ExtraUsageUnknown),
        "no trusted reading"
    );
}

#[test]
fn ineligible_when_balance_zero() {
    let record = opted_in("ineligible_when_balance_zero");
    let reading = extra_usage_on();

    assert_eq!(
        judge(
            &record,
            Some(&reading),
            Some(&prepaid(0, Some("USD"), 60)),
            true
        ),
        Err(Ineligible::BalanceZero)
    );
    assert_eq!(
        judge(
            &record,
            Some(&reading),
            Some(&prepaid(-5, Some("USD"), 60)),
            true
        ),
        Err(Ineligible::BalanceZero)
    );
    assert_eq!(
        judge(&record, Some(&reading), None, true),
        Err(Ineligible::NoBalance),
        "never read"
    );
}

#[test]
fn ineligible_when_currency_missing() {
    let record = opted_in("ineligible_when_currency_missing");
    let reading = extra_usage_on();

    assert_eq!(
        judge(
            &record,
            Some(&reading),
            Some(&prepaid(102_003, None, 60)),
            true
        ),
        Err(Ineligible::CurrencyMissing)
    );
}

#[test]
fn ineligible_when_balance_older_than_900_s() {
    let record = opted_in("ineligible_when_balance_older_than_900_s");
    let reading = extra_usage_on();
    assert!((PAID_BALANCE_MAX_AGE_S - 900.0).abs() < f64::EPSILON);

    assert!(
        judge(
            &record,
            Some(&reading),
            Some(&prepaid(102_003, Some("USD"), 900)),
            true
        )
        .is_ok(),
        "900 s old is still fresh"
    );
    assert_eq!(
        judge(
            &record,
            Some(&reading),
            Some(&prepaid(102_003, Some("USD"), 901)),
            true
        ),
        Err(Ineligible::StaleBalance)
    );
    let mut undated = funded();
    undated.fetched_at = None;
    assert_eq!(
        judge(&record, Some(&reading), Some(&undated), true),
        Err(Ineligible::StaleBalance)
    );
}

/// A balance read more than 60 s after `now` (another machine's clock) is stale; within 60 s it counts as age 0.
#[test]
fn a_balance_from_the_future_is_stale() {
    let record = opted_in("a_balance_from_the_future_is_stale");
    let reading = extra_usage_on();

    assert_eq!(
        judge(
            &record,
            Some(&reading),
            Some(&prepaid(102_003, Some("USD"), -61)),
            true
        ),
        Err(Ineligible::StaleBalance)
    );
    let balance = judge(
        &record,
        Some(&reading),
        Some(&prepaid(102_003, Some("USD"), -60)),
        true,
    )
    .expect("60 s ahead is fresh");
    assert!(
        balance.age_s.abs() < f64::EPSILON,
        "a balance ahead of the clock is age 0: {balance:?}"
    );
}

/// The flags as the poll records a block that names only `is_enabled`: the omitted ones are unknown, never off.
#[test]
fn ineligible_when_the_poll_omitted_a_flag() {
    let record = opted_in("ineligible_when_the_poll_omitted_a_flag");
    let recorded = json!({"five_hour": {"pct": 100.0}, "extra_usage": {"enabled": true}});

    assert_eq!(
        judge(&record, Some(&recorded), Some(&funded()), true),
        Err(Ineligible::ExtraUsageUnknown)
    );
}

/// A prepaid row whose every read failed holds only `error` and `attemptedAt`: no balance.
#[test]
fn no_balance_when_the_prepaid_row_holds_only_an_error() {
    let record = opted_in("no_balance_when_the_prepaid_row_holds_only_an_error");
    let reading = extra_usage_on();
    let mut failed = PrepaidEntry::default();
    failed.error = Some("http-429".to_owned());
    failed.attempted_at = Some(now());

    assert_eq!(
        judge(&record, Some(&reading), Some(&failed), true),
        Err(Ineligible::NoBalance)
    );
}

#[test]
fn paid_ranking_keeps_unnamed_accounts_in_their_input_order_after_named_ones() {
    let candidates = [
        (5, balance(10.0, "USD")),
        (1, balance(10.0, "USD")),
        (7, balance(10.0, "USD")),
        (2, balance(10.0, "USD")),
    ];

    assert_eq!(
        paid_ranking(&candidates, &[2, 1]),
        [2, 1, 5, 7],
        "named accounts in rotation order, then the rest as given"
    );
}

/// The warnings an event's fields make, as `name=value` text, one line per warning.
#[derive(Clone, Default)]
struct Warnings(Arc<Mutex<Vec<String>>>);

impl Warnings {
    fn lines(&self) -> Vec<String> {
        self.0
            .lock()
            .expect("no test panics holding the warnings")
            .clone()
    }
}

struct Fields(String);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        // Writing to a String cannot fail.
        let _written = write!(self.0, "{}={value:?} ", field.name());
    }
}

impl tracing::Subscriber for Warnings {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if *event.metadata().level() == tracing::Level::WARN {
            let mut fields = Fields(String::new());
            event.record(&mut fields);
            self.0
                .lock()
                .expect("no test panics holding the warnings")
                .push(fields.0);
        }
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

#[test]
fn paid_ranking_warns_on_mixed_currencies() {
    let warnings = Warnings::default();
    let mixed = [
        (1, balance(5.0, "USD")),
        (2, balance(9.0, "EUR")),
        (3, balance(7.0, "USD")),
    ];
    let single = [(1, balance(5.0, "USD")), (3, balance(7.0, "USD"))];

    tracing::subscriber::with_default(warnings.clone(), || {
        assert_eq!(paid_ranking(&single, &[1, 3]), [3, 1]);
        assert!(warnings.lines().is_empty(), "one currency: no warning");
        assert_eq!(paid_ranking(&mixed, &[1, 2, 3]), [2, 3, 1]);
    });

    let lines = warnings.lines();
    assert_eq!(lines.len(), 1, "one warning: {lines:?}");
    for field in [
        "account=1 ",
        "currency=USD ",
        "other_account=2 ",
        "other_currency=EUR ",
    ] {
        assert!(
            lines[0].contains(field),
            "the warning names {field}: {}",
            lines[0]
        );
    }
    assert!(!lines[0].contains('@'), "no email: {}", lines[0]);
}
