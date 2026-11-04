//! The paid fallback's account test and ranking: whether an account may carry the fleet on its prepaid extra-usage
//! credits once every account's subscription usage is exhausted ([`paid_eligible`]), and which eligible account to
//! spend on first ([`paid_ranking`]).

use jiff::Timestamp;
use serde_json::Value;

use crate::poll::FUTURE_FETCH_SLACK_S;
use crate::store::{AccountKind, AccountRecord};
use crate::usage::{PrepaidEntry, secs_from_timestamp};

/// The oldest prepaid balance reading the engine spends on, in seconds: 15 minutes.
pub const PAID_BALANCE_MAX_AGE_S: f64 = 900.0;
/// How often the engine reads the balance of the account it is spending on, in seconds: every 2 minutes.
pub const PAID_STAY_READ_S: f64 = 120.0;

/// One account as [`paid_eligible`] judges it: what the caller already holds for the tick.
#[derive(Clone, Copy, Debug)]
pub struct PaidCandidate<'a> {
    /// The account's `accounts.json` record.
    pub record: &'a AccountRecord,
    /// The account's trusted reading: the [`crate::engine::decision_value`] the tick ranks on, `None` when the reading
    /// is not trusted or the poll result is a sentinel.
    pub reading: Option<&'a Value>,
    /// The account's prepaid balance as its usage row holds it.
    pub prepaid: Option<&'a PrepaidEntry>,
    /// Whether the account is available to switch to: enabled, with a stored credential and a config backup, not
    /// quarantined in the state file, and not an identity conflict whose quarantine is not written yet.
    pub available: bool,
}

/// An eligible account's prepaid balance.
#[derive(Clone, Debug, PartialEq)]
pub struct PaidBalance {
    /// The balance in major units: `amountMinor / 10^exponent`.
    pub amount: f64,
    /// The balance's currency code.
    pub currency: String,
    /// How old the balance reading is, in seconds.
    pub age_s: f64,
}

/// Why an account is not a paid target: the first clause of the eligibility test it fails, in the order
/// [`paid_eligible`] checks them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Ineligible {
    /// The record is a managed API key, not an OAuth account.
    NotOauth,
    /// The account is held out of automatic switching.
    Disabled,
    /// The owner has not allowed the account's credits to be spent (`maySpendCredits` is not `true`).
    FlagOff,
    /// The account cannot be switched to: no stored credential or config backup, quarantined, or conflicted.
    NotSwitchable,
    /// The trusted reading, its `extra_usage` object or one of the object's flags is missing.
    ExtraUsageUnknown,
    /// Extra usage is off for the account.
    ExtraUsageOff,
    /// The user turned extra usage off.
    UserDisabled,
    /// The plan's extra-usage spend limit is reached.
    SpendLimitReached,
    /// Extra usage names a reason it is disabled.
    DisabledReasonSet,
    /// No prepaid balance has been read.
    NoBalance,
    /// The prepaid balance is zero or less.
    BalanceZero,
    /// The prepaid balance names no currency.
    CurrencyMissing,
    /// The prepaid balance reading is older than [`PAID_BALANCE_MAX_AGE_S`], dated more than 60 s after the test's
    /// time, or has no fetch time.
    StaleBalance,
}

impl Ineligible {
    /// The clause's name in event and log lines, such as `flag-off`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotOauth => "not-oauth",
            Self::Disabled => "disabled",
            Self::FlagOff => "flag-off",
            Self::NotSwitchable => "not-switchable",
            Self::ExtraUsageUnknown => "extra-usage-unknown",
            Self::ExtraUsageOff => "extra-usage-off",
            Self::UserDisabled => "user-disabled",
            Self::SpendLimitReached => "spend-limit-reached",
            Self::DisabledReasonSet => "disabled-reason-set",
            Self::NoBalance => "no-balance",
            Self::BalanceZero => "balance-zero",
            Self::CurrencyMissing => "currency-missing",
            Self::StaleBalance => "stale-balance",
        }
    }
}

impl std::fmt::Display for Ineligible {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Whether `candidate` may carry the fleet on its prepaid credits at `now`, and its balance when it may: an OAuth
/// record that is not disabled, opted in (`maySpendCredits`), switchable, whose trusted reading has extra usage
/// enabled, not user-disabled, under its spend limit and with no disabled reason (every one of those flags present,
/// else it fails closed), and whose prepaid balance is positive, in a named currency, and read at most
/// [`PAID_BALANCE_MAX_AGE_S`] before `now` and no more than 60 s after it. Auto-reload never excludes an account: the
/// opt-in is the consent.
///
/// # Errors
///
/// The first clause the account fails, as [`Ineligible`].
pub fn paid_eligible(
    candidate: &PaidCandidate<'_>,
    now: Timestamp,
) -> Result<PaidBalance, Ineligible> {
    record_eligible(candidate)?;
    extra_usage_on(candidate.reading)?;
    prepaid_balance(candidate.prepaid, now)
}

/// [`paid_eligible`]'s record clauses alone: an OAuth record that is not disabled, opted in and switchable. The
/// reading and the balance are not judged.
pub(crate) fn record_eligible(candidate: &PaidCandidate<'_>) -> Result<(), Ineligible> {
    let record = candidate.record;
    if record.kind != AccountKind::Oauth {
        return Err(Ineligible::NotOauth);
    }
    if record.disabled {
        return Err(Ineligible::Disabled);
    }
    if !record.may_spend_credits {
        return Err(Ineligible::FlagOff);
    }
    if !candidate.available {
        return Err(Ineligible::NotSwitchable);
    }
    Ok(())
}

/// Whether the reading's `extra_usage` says the account cannot spend credits: extra usage off, turned off by the user,
/// at its spend limit, or disabled for a named reason. A missing reading, object or flag is not known off.
pub(crate) fn extra_usage_known_off(reading: Option<&Value>) -> bool {
    matches!(
        extra_usage_on(reading),
        Err(Ineligible::ExtraUsageOff
            | Ineligible::UserDisabled
            | Ineligible::SpendLimitReached
            | Ineligible::DisabledReasonSet)
    )
}

/// The reading's `extra_usage` flags as the paid fallback needs them; a missing reading, object or flag fails closed.
pub(crate) fn extra_usage_on(reading: Option<&Value>) -> Result<(), Ineligible> {
    let extra = reading
        .and_then(|reading| reading.get("extra_usage"))
        .and_then(Value::as_object)
        .ok_or(Ineligible::ExtraUsageUnknown)?;
    let flag = |key: &str| {
        extra
            .get(key)
            .and_then(Value::as_bool)
            .ok_or(Ineligible::ExtraUsageUnknown)
    };
    let (enabled, user_disabled) = (flag("enabled")?, flag("user_disabled")?);
    let (limit_reached, reason_set) = (flag("spend_limit_reached")?, flag("disabled_reason_set")?);
    if !enabled {
        return Err(Ineligible::ExtraUsageOff);
    }
    if user_disabled {
        return Err(Ineligible::UserDisabled);
    }
    if limit_reached {
        return Err(Ineligible::SpendLimitReached);
    }
    if reason_set {
        return Err(Ineligible::DisabledReasonSet);
    }
    Ok(())
}

/// The prepaid entry's balance when it is positive, in a named currency, and fresh at `now`. A fetch time at most
/// [`FUTURE_FETCH_SLACK_S`] after `now` (another machine's clock) counts as age 0; one further ahead is stale.
fn prepaid_balance(
    prepaid: Option<&PrepaidEntry>,
    now: Timestamp,
) -> Result<PaidBalance, Ineligible> {
    let prepaid = prepaid.ok_or(Ineligible::NoBalance)?;
    let (Some(amount_minor), Some(exponent)) = (prepaid.amount_minor, prepaid.exponent) else {
        return Err(Ineligible::NoBalance);
    };
    if amount_minor <= 0 {
        return Err(Ineligible::BalanceZero);
    }
    let currency = prepaid
        .currency
        .clone()
        .ok_or(Ineligible::CurrencyMissing)?;
    let fetched = prepaid.fetched_at.ok_or(Ineligible::StaleBalance)?;
    let age_s = secs_from_timestamp(now) - secs_from_timestamp(fetched);
    if age_s > PAID_BALANCE_MAX_AGE_S || age_s < -FUTURE_FETCH_SLACK_S {
        return Err(Ineligible::StaleBalance);
    }
    let age_s = age_s.max(0.0);
    Ok(PaidBalance {
        amount: major_units(amount_minor, exponent),
        currency,
        age_s,
    })
}

/// The balance `prepaid` holds when it is positive and in a named currency, whatever its age: the amount a paid
/// target whose only failure is [`Ineligible::StaleBalance`] is ranked on before its balance is read again. Its age is
/// infinite when it has no fetch time.
pub(crate) fn cached_balance(prepaid: &PrepaidEntry, now: Timestamp) -> Option<PaidBalance> {
    let (Some(amount_minor), Some(exponent)) = (prepaid.amount_minor, prepaid.exponent) else {
        return None;
    };
    let currency = prepaid.currency.clone().filter(|_| amount_minor > 0)?;
    let age_s = prepaid.fetched_at.map_or(f64::INFINITY, |fetched| {
        secs_from_timestamp(now) - secs_from_timestamp(fetched)
    });
    Some(PaidBalance {
        amount: major_units(amount_minor, exponent),
        currency,
        age_s,
    })
}

/// Whether `prepaid` is still a balance the engine may spend on at `now`: positive, and read at most
/// [`PAID_BALANCE_MAX_AGE_S`] before `now` and no more than [`FUTURE_FETCH_SLACK_S`] after it.
pub(crate) fn balance_still_spendable(prepaid: &PrepaidEntry, now: Timestamp) -> bool {
    let Some(fetched) = prepaid.fetched_at else {
        return false;
    };
    let age_s = secs_from_timestamp(now) - secs_from_timestamp(fetched);
    prepaid.amount_minor.is_some_and(|amount| amount > 0)
        && (-FUTURE_FETCH_SLACK_S..=PAID_BALANCE_MAX_AGE_S).contains(&age_s)
}

/// Whether `prepaid` says the balance is used up at `now`: zero or less, and read at most [`PAID_BALANCE_MAX_AGE_S`]
/// before `now` and no more than [`FUTURE_FETCH_SLACK_S`] after it. A stale zero is not known used up.
pub(crate) fn balance_used_up(prepaid: Option<&PrepaidEntry>, now: Timestamp) -> bool {
    let Some(prepaid) = prepaid else {
        return false;
    };
    let Some(fetched) = prepaid.fetched_at else {
        return false;
    };
    let age_s = secs_from_timestamp(now) - secs_from_timestamp(fetched);
    prepaid.amount_minor.is_some_and(|amount| amount <= 0)
        && (-FUTURE_FETCH_SLACK_S..=PAID_BALANCE_MAX_AGE_S).contains(&age_s)
}

/// `amount_minor / 10^exponent`.
#[expect(
    clippy::cast_precision_loss,
    reason = "a balance in minor units is far below 2^53"
)]
fn major_units(amount_minor: i64, exponent: u32) -> f64 {
    let scale = i32::try_from(exponent).unwrap_or(i32::MAX);
    amount_minor as f64 / 10_f64.powi(scale)
}

/// The eligible accounts in the order to spend on them: the largest major-units balance first, ties in `rotation`
/// order (an account `rotation` does not name after those it does, in `candidates` order). Balances in different
/// currencies are compared in major units as they are, with one warning naming the accounts and the currencies.
#[must_use]
pub fn paid_ranking(candidates: &[(u32, PaidBalance)], rotation: &[u32]) -> Vec<u32> {
    warn_on_mixed_currencies(candidates);
    let place = |number: u32| {
        rotation
            .iter()
            .position(|known| *known == number)
            .unwrap_or(rotation.len())
    };
    let mut ordered: Vec<&(u32, PaidBalance)> = candidates.iter().collect();
    // Stable sorts: rotation order first, then the balance, so rotation order breaks ties, as `rank_view`'s does.
    ordered.sort_by_key(|(number, _)| place(*number));
    ordered.sort_by(|left, right| right.1.amount.total_cmp(&left.1.amount));
    ordered.into_iter().map(|(number, _)| *number).collect()
}

/// One warning when `candidates` hold balances in more than one currency, naming the first two that differ.
fn warn_on_mixed_currencies(candidates: &[(u32, PaidBalance)]) {
    let Some((first, first_balance)) = candidates.first() else {
        return;
    };
    if let Some((other, other_balance)) = candidates
        .iter()
        .find(|(_, balance)| balance.currency != first_balance.currency)
    {
        tracing::warn!(
            account = first,
            currency = %first_balance.currency,
            other_account = other,
            other_currency = %other_balance.currency,
            "paid fallback balances are in different currencies; they are compared in major units as they are"
        );
    }
}
