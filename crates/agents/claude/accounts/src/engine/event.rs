//! What a tick reports, cswap's `AutoSwitchEvent` family: each [`Event`] is one thing the engine saw or did, with the
//! time it happened, and [`Event::to_json`] writes it in cswap's JSONL shape (`schemaVersion`, `event`, `ts` and the
//! kind's own fields), so a reader of cswap's `auto --json` stream reads these too.

use std::collections::BTreeMap;

use jiff::Timestamp;
use serde_json::{Map, Value, json};

use crate::engine::{ReleaseReason, Trigger};

/// The `schemaVersion` every event's JSON carries, cswap's JSON output schema version.
pub const EVENT_SCHEMA_VERSION: u64 = 1;

/// An account as an event names it, cswap's `account_ref`: its number (none for a login no managed account has) and
/// its email.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountRef {
    /// The managed account's number; `None` for a live login no managed account has.
    pub number: Option<u32>,
    /// The account's email, empty when the record has none.
    pub email: String,
}

impl AccountRef {
    /// Account `number` with `email`.
    #[must_use]
    pub fn new(number: Option<u32>, email: impl Into<String>) -> Self {
        Self {
            number,
            email: email.into(),
        }
    }

    fn to_json(&self) -> Value {
        json!({ "number": self.number, "email": self.email })
    }
}

/// One thing a tick saw or did, and when.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// When the tick reported it.
    pub at: Timestamp,
    /// What it was.
    pub body: EventBody,
}

/// What an [`Event`] says, one variant per cswap event kind ([`Event::kind`]).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum EventBody {
    /// The usage the tick decided on (`poll`).
    Poll {
        /// The live login's account, `None` when there is none.
        active: Option<AccountRef>,
        /// Each account's headroom in percent, `None` when its usage is unknown this tick.
        headroom: BTreeMap<u32, Option<f64>>,
        /// The threshold the tick read.
        threshold: f64,
        /// The last fetch error of each account whose usage is unknown, such as `http-429` or `timeout`.
        fetch_errors: BTreeMap<u32, String>,
        /// Each readable account's decision windows, in order: `5h`, `7d`, then each scoped window the configured
        /// models name, under its API name, with their utilization in percent.
        windows: BTreeMap<u32, Vec<(String, f64)>>,
    },
    /// The live login moved (`switch`).
    Switch {
        /// Why.
        trigger: Trigger,
        /// The account it left.
        from: Option<AccountRef>,
        /// The account it is now.
        to: AccountRef,
        /// What the switch warned of.
        warnings: Vec<String>,
    },
    /// The tick did not switch (`no-switch`), with cswap's reason, such as `below-threshold` or `cooldown`.
    NoSwitch {
        /// cswap's reason name.
        reason: &'static str,
        /// The detail, empty when there is none.
        detail: String,
    },
    /// An account was taken out of rotation (`account-quarantined`).
    Quarantined {
        /// The account's number.
        number: u32,
        /// The account's email.
        email: String,
        /// Why: `invalid_grant` or `identity-conflict`.
        reason: String,
    },
    /// A quarantine was lifted (`account-unquarantined`).
    Unquarantined {
        /// The account's number, the state file's key.
        number: String,
        /// The account's email.
        email: String,
        /// Why it was lifted.
        reason: ReleaseReason,
    },
    /// Every candidate is at its limit (`all-exhausted`).
    AllExhausted {
        /// The soonest moment an account is usable again, when it can be proven.
        earliest_reset_at: Option<Timestamp>,
    },
    /// Something failed (`error`).
    Error {
        /// What failed, in words; never a token, a path or a response body.
        message: String,
        /// Whether a later tick may succeed without anyone acting.
        transient: bool,
    },
    /// A setting that parses but provably does nothing (`config-warning`), such as a model name no usage window has.
    ConfigWarning {
        /// What, in words.
        message: String,
    },
    /// Something the user should know of that is not an error (`warning`), such as an idle hold past its cap.
    Warning {
        /// What, in words.
        message: String,
    },
    /// Something the owner did that the log should show, neither a fault nor a warning (`note`), such as turning
    /// auto-switch off; the caller's own event, never a tick's.
    Note {
        /// What, in words.
        message: String,
    },
    /// The engine is about to wait longer than usual (`sleep`), the loop's own event.
    Sleep {
        /// How long it waits, in seconds.
        seconds: f64,
        /// When it wakes.
        until: Timestamp,
    },
    /// Every account was exhausted and the engine moved onto an opted-in account's prepaid extra-usage credits
    /// (`paid-fallback-entered`).
    PaidFallbackEntered {
        /// The account whose credits are spent on.
        number: u32,
        /// The account the login left; `number` itself for an entry in place.
        from: u32,
        /// The prepaid balance at entry, in major units.
        amount: f64,
        /// The balance's currency code.
        currency: String,
    },
    /// The engine stopped spending on account `number`'s credits (`paid-fallback-left`).
    PaidFallbackLeft {
        /// The account whose credits were spent on.
        number: u32,
        /// Why: `refill`, `own-refill`, `flag-off`, `balance-zero`, `extra-usage-off`, `stale-balance`, `owner-exit` or
        /// `ineligible`.
        reason: &'static str,
        /// The account the login moved to on a `refill` leave (`to`); `None` for every other reason.
        to: Option<u32>,
        /// That account's trusted headroom in percent at the decision (`headroomPct`); `None` for every other reason.
        headroom_pct: Option<f64>,
    },
}

impl Event {
    /// cswap's event kind: `poll`, `switch`, `no-switch`, `account-quarantined`, `account-unquarantined`,
    /// `all-exhausted`, `error`, `config-warning`, `warning` (an event cswap only logs), `sleep`, or this crate's own
    /// `note`, `paid-fallback-entered` and `paid-fallback-left`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self.body {
            EventBody::Poll { .. } => "poll",
            EventBody::Switch { .. } => "switch",
            EventBody::NoSwitch { .. } => "no-switch",
            EventBody::Quarantined { .. } => "account-quarantined",
            EventBody::Unquarantined { .. } => "account-unquarantined",
            EventBody::AllExhausted { .. } => "all-exhausted",
            EventBody::Error { .. } => "error",
            EventBody::ConfigWarning { .. } => "config-warning",
            EventBody::Warning { .. } => "warning",
            EventBody::Note { .. } => "note",
            EventBody::Sleep { .. } => "sleep",
            EventBody::PaidFallbackEntered { .. } => "paid-fallback-entered",
            EventBody::PaidFallbackLeft { .. } => "paid-fallback-left",
        }
    }

    /// The event as cswap's `to_json` writes it: `schemaVersion`, `event` (the kind), `ts` (UTC to the second, `Z`)
    /// and the kind's fields, account numbers as JSON keys or strings where cswap writes them so.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert(
            "schemaVersion".to_owned(),
            Value::from(EVENT_SCHEMA_VERSION),
        );
        map.insert("event".to_owned(), Value::from(self.kind()));
        map.insert(
            "ts".to_owned(),
            Value::from(self.at.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()),
        );
        if let Value::Object(fields) = self.fields() {
            map.extend(fields);
        }
        Value::Object(map)
    }

    fn fields(&self) -> Value {
        match &self.body {
            EventBody::Poll {
                active,
                headroom,
                threshold,
                fetch_errors,
                windows,
            } => poll_fields(active.as_ref(), headroom, *threshold, fetch_errors, windows),
            EventBody::Switch {
                trigger,
                from,
                to,
                warnings,
            } => json!({
                "trigger": trigger.as_str(),
                "from": from.as_ref().map(AccountRef::to_json),
                "to": to.to_json(),
                "warnings": warnings,
                "dryRun": false,
            }),
            EventBody::NoSwitch { reason, detail } => json!({ "reason": reason, "detail": detail }),
            EventBody::Quarantined {
                number,
                email,
                reason,
            } => json!({ "number": number.to_string(), "email": email, "reason": reason }),
            EventBody::Unquarantined {
                number,
                email,
                reason,
            } => json!({ "number": number, "email": email, "reason": reason.kind() }),
            EventBody::AllExhausted { earliest_reset_at } => {
                json!({ "earliestResetAt": earliest_reset_at.map(|at| at.to_string()) })
            }
            EventBody::Error { message, transient } => {
                json!({ "message": message, "transient": transient })
            }
            EventBody::ConfigWarning { message }
            | EventBody::Warning { message }
            | EventBody::Note { message } => json!({ "message": message }),
            EventBody::Sleep { seconds, until } => json!({
                "seconds": round_tenths(*seconds),
                "until": until.strftime("%Y-%m-%dT%H:%M:%SZ").to_string(),
            }),
            EventBody::PaidFallbackEntered {
                number,
                from,
                amount,
                currency,
            } => json!({ "number": number, "from": from, "amount": amount, "currency": currency }),
            EventBody::PaidFallbackLeft {
                number,
                reason,
                to,
                headroom_pct,
            } => {
                let mut fields = Map::new();
                fields.insert("number".to_owned(), Value::from(*number));
                fields.insert("reason".to_owned(), Value::from(*reason));
                if let Some(to) = to {
                    fields.insert("to".to_owned(), Value::from(*to));
                }
                if let Some(headroom_pct) = headroom_pct {
                    fields.insert("headroomPct".to_owned(), Value::from(*headroom_pct));
                }
                Value::Object(fields)
            }
        }
    }
}

/// `seconds` to one decimal place, as Python's `SleepEvent._fields`' `round(self.seconds, 1)` does: Rust's `{:.1}` is
/// correctly rounded on the exact binary double, ties to even, where `(seconds * 10.0).round() / 10.0` would round a
/// tie away from zero (`round(3600.25, 1)` is 3600.2, not 3600.3).
fn round_tenths(seconds: f64) -> f64 {
    format!("{seconds:.1}").parse::<f64>().unwrap_or(seconds)
}

/// cswap's `PollEvent` fields: `fetchErrors` and `windowsPct` only when they hold something.
fn poll_fields(
    active: Option<&AccountRef>,
    headroom: &BTreeMap<u32, Option<f64>>,
    threshold: f64,
    fetch_errors: &BTreeMap<u32, String>,
    windows: &BTreeMap<u32, Vec<(String, f64)>>,
) -> Value {
    let mut fields = Map::new();
    fields.insert(
        "active".to_owned(),
        active.map_or(Value::Null, AccountRef::to_json),
    );
    let headroom: Map<String, Value> = headroom
        .iter()
        .map(|(number, value)| (number.to_string(), json!(value)))
        .collect();
    fields.insert("headroomPct".to_owned(), Value::Object(headroom));
    fields.insert("threshold".to_owned(), json!(threshold));
    if !fetch_errors.is_empty() {
        let errors: Map<String, Value> = fetch_errors
            .iter()
            .map(|(number, error)| (number.to_string(), json!(error)))
            .collect();
        fields.insert("fetchErrors".to_owned(), Value::Object(errors));
    }
    if !windows.is_empty() {
        let pcts: Map<String, Value> = windows
            .iter()
            .map(|(number, windows)| {
                let labelled: Map<String, Value> = windows
                    .iter()
                    .map(|(label, pct)| (label.clone(), json!(pct)))
                    .collect();
                (number.to_string(), Value::Object(labelled))
            })
            .collect();
        fields.insert("windowsPct".to_owned(), Value::Object(pcts));
    }
    Value::Object(fields)
}
