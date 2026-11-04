//! The per-account usage table of the account store, `cache/usage.json` (schema [`USAGE_SCHEMA_VERSION`]).
//!
//! Each row belongs to an account slot and names the account it describes (`email`, `organizationUuid`): a row whose
//! identity differs from the slot's current account reads as an empty entry, so a reused slot never shows the previous
//! account's usage. Times are seconds since the epoch. Reads take no lock: every writer replaces the file whole. A JSON
//! object whose `schemaVersion` is not the integer [`USAGE_SCHEMA_VERSION`] (missing, a string, a float or another
//! number) reads as empty and is never written; a file that is not a JSON object (torn, empty, garbage) reads as empty
//! and is rewritten as a fresh table by the next write.

use std::collections::BTreeMap;
use std::io;

use serde_json::{Map, Value};

use crate::paths::AccountsRoot;
use crate::store::{AccountRecord, read_optional_unlinked};

/// The only `cache/usage.json` schema this crate reads or writes.
pub const USAGE_SCHEMA_VERSION: u64 = 3;

/// The largest minor-unit scale a usage or prepaid answer may name, and a stored balance may hold.
pub(crate) const MAX_MINOR_EXPONENT: u32 = 9;

/// Whether `code` is a currency code as a prepaid answer names one: exactly three ASCII uppercase letters.
pub(crate) fn is_currency_code(code: &str) -> bool {
    code.len() == 3 && code.bytes().all(|byte| byte.is_ascii_uppercase())
}

/// The row keys [`UsageEntry`] models; every other key lands in [`UsageEntry::extra`].
const MODELLED_KEYS: [&str; 15] = [
    "email",
    "organizationUuid",
    "lastGood",
    "fetchedAt",
    "lastAttemptAt",
    "consecutiveFailures",
    "lastError",
    "backoffUntil",
    "nextPollAt",
    "pollIntervalS",
    "last429At",
    "claimId",
    "claimUntil",
    "heldUntil",
    "prepaid",
];

/// The account a usage row describes: its email and organization UUID (empty when it has none).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub email: String,
    pub org_uuid: String,
}

impl Identity {
    /// The identity of `email` in organization `org_uuid` (empty for none).
    #[must_use]
    pub fn new(email: &str, org_uuid: &str) -> Self {
        Self {
            email: email.to_owned(),
            org_uuid: org_uuid.to_owned(),
        }
    }

    /// The identity `accounts.json` records for an account.
    #[must_use]
    pub fn of(record: &AccountRecord) -> Self {
        Self::new(record.email.as_str(), &record.org_uuid)
    }

    /// Whether `row` describes this account: the same `email`, and the same
    /// `organizationUuid`, a missing one counting as empty.
    pub(crate) fn matches(&self, row: &Map<String, Value>) -> bool {
        let org_matches = match row.get("organizationUuid") {
            None => self.org_uuid.is_empty(),
            Some(Value::String(org)) => *org == self.org_uuid,
            Some(_) => false,
        };
        row.get("email").and_then(Value::as_str) == Some(self.email.as_str()) && org_matches
    }

    /// The row started for an account with no row of its own.
    pub(crate) fn fresh_row(&self) -> Map<String, Value> {
        let mut row = Map::new();
        row.insert("email".to_owned(), Value::String(self.email.clone()));
        row.insert(
            "organizationUuid".to_owned(),
            Value::String(self.org_uuid.clone()),
        );
        row
    }
}

/// One rate-limit window of a usage reading: how much of it is used, and when it resets.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageWindow {
    /// Utilization in percent; 100 or more means the window is exhausted.
    pub pct: f64,
    /// When the window resets, if the reading said and it parsed.
    pub resets_at: Option<jiff::Timestamp>,
}

/// One account's row of the usage table as read: the last good reading, the fetch and backoff state, the fetch lease,
/// and every key this crate does not model. An account with no row, or a row naming another account, reads as
/// `UsageEntry::default()`.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct UsageEntry {
    /// The account the row describes; `None` on an empty entry.
    pub identity: Option<Identity>,
    /// The last successful reading (`lastGood`), raw, as the poll shapes it.
    pub last_good: Option<Value>,
    /// The account-wide 5-hour window of `last_good`.
    pub five_hour: Option<UsageWindow>,
    /// The account-wide 7-day window of `last_good`.
    pub seven_day: Option<UsageWindow>,
    /// When `last_good` was fetched (`fetchedAt`).
    pub fetched_at: Option<jiff::Timestamp>,
    /// When a fetch was last claimed or recorded (`lastAttemptAt`).
    pub last_attempt_at: Option<jiff::Timestamp>,
    /// Failed fetches since the last success (`consecutiveFailures`).
    pub consecutive_failures: u32,
    /// The last failure's kind, such as `http-429` or `timeout` (`lastError`).
    pub last_error: Option<String>,
    /// No fetch before this time (`backoffUntil`).
    pub backoff_until: Option<jiff::Timestamp>,
    /// The next scheduled fetch (`nextPollAt`).
    pub next_poll_at: Option<jiff::Timestamp>,
    /// The poll interval the schedule last chose, in seconds (`pollIntervalS`).
    pub poll_interval_s: Option<f64>,
    /// When the usage endpoint last answered 429 for this account (`last429At`).
    pub last_429_at: Option<jiff::Timestamp>,
    /// The fencing id of the fetch lease, if one is held (`claimId`).
    pub claim_id: Option<String>,
    /// When the fetch lease lapses (`claimUntil`).
    pub claim_until: Option<jiff::Timestamp>,
    /// No collector fetches before this time: another machine hands its readings over (`heldUntil`).
    pub held_until: Option<jiff::Timestamp>,
    /// The organization's prepaid extra-usage balance as last read (`prepaid`), with its own fetch time and failure;
    /// `None` when the row has none.
    pub prepaid: Option<PrepaidEntry>,
    /// Every other key of the row, as written, in file order.
    pub extra: Map<String, Value>,
}

/// An account's prepaid extra-usage balance as its usage row holds it (`prepaid`): the last good figure and when it was
/// read, and the last attempt's failure, which leaves the figure in place. A balance read never touches the row's
/// usage failure state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PrepaidEntry {
    /// The balance in minor units (`amountMinor`); 0 for an organization with no prepaid balance. `None` when no read
    /// has succeeded yet.
    pub amount_minor: Option<i64>,
    /// How many decimal places the minor unit is (`exponent`): the balance is `amount_minor / 10^exponent`.
    pub exponent: Option<u32>,
    /// The balance's currency (`currency`); `None` for an organization with no prepaid balance.
    pub currency: Option<String>,
    /// Whether the organization tops its balance up on its own (`autoReload`): `None` when the answer did not say.
    pub auto_reload: Option<bool>,
    /// When the figure was read (`fetchedAt`).
    pub fetched_at: Option<jiff::Timestamp>,
    /// The last attempt's failure kind, such as `http-429` or `timeout` (`error`); `None` after a success.
    pub error: Option<String>,
    /// When a balance read was last attempted, success or failure (`attemptedAt`).
    pub attempted_at: Option<jiff::Timestamp>,
}

impl PrepaidEntry {
    /// The row's `prepaid` object as an entry; `None` when it is not an object, names an exponent above
    /// [`MAX_MINOR_EXPONENT`], or names a currency that is not [`is_currency_code`]'s, none of which a read writes.
    pub(crate) fn from_value(value: &Value) -> Option<Self> {
        let prepaid = value.as_object()?;
        let exponent = match prepaid.get("exponent").and_then(Value::as_u64) {
            Some(exponent) => Some(
                u32::try_from(exponent)
                    .ok()
                    .filter(|exponent| *exponent <= MAX_MINOR_EXPONENT)?,
            ),
            None => None,
        };
        let currency = match prepaid.get("currency") {
            None | Some(Value::Null) => None,
            Some(Value::String(code)) if is_currency_code(code) => Some(code.clone()),
            Some(_) => return None,
        };
        Some(Self {
            amount_minor: prepaid.get("amountMinor").and_then(Value::as_i64),
            exponent,
            currency,
            auto_reload: prepaid.get("autoReload").and_then(Value::as_bool),
            fetched_at: row_time(prepaid, "fetchedAt"),
            error: prepaid
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned),
            attempted_at: row_time(prepaid, "attemptedAt"),
        })
    }
}

impl UsageEntry {
    fn from_row(identity: &Identity, row: &Map<String, Value>) -> Self {
        let last_good = row
            .get("lastGood")
            .filter(|value| value.is_object())
            .cloned();
        let (five_hour, seven_day) = last_good.as_ref().map_or((None, None), account_windows);
        let extra = row
            .iter()
            .filter(|(key, _)| !MODELLED_KEYS.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        Self {
            identity: Some(identity.clone()),
            last_good,
            five_hour,
            seven_day,
            fetched_at: row_time(row, "fetchedAt"),
            last_attempt_at: row_time(row, "lastAttemptAt"),
            consecutive_failures: failure_count(row.get("consecutiveFailures")),
            last_error: row
                .get("lastError")
                .and_then(Value::as_str)
                .map(str::to_owned),
            backoff_until: row_time(row, "backoffUntil"),
            next_poll_at: row_time(row, "nextPollAt"),
            poll_interval_s: row.get("pollIntervalS").and_then(Value::as_f64),
            last_429_at: row_time(row, "last429At"),
            claim_id: row
                .get("claimId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            claim_until: row_time(row, "claimUntil"),
            held_until: row_time(row, "heldUntil"),
            prepaid: row.get("prepaid").and_then(PrepaidEntry::from_value),
            extra,
        }
    }

    /// Whether the reading was fetched at most `ttl_s` seconds before `now`.
    #[must_use]
    pub fn fresh(&self, now: jiff::Timestamp, ttl_s: f64) -> bool {
        self.fetched_at
            .is_some_and(|fetched| secs_from_timestamp(now) - secs_from_timestamp(fetched) <= ttl_s)
    }

    /// Whether a failure's backoff still keeps collectors off at `now`.
    #[must_use]
    pub fn in_backoff(&self, now: jiff::Timestamp) -> bool {
        self.backoff_until.is_some_and(|until| now < until)
    }

    /// Whether a reading adopted from another machine still keeps collectors off at `now`.
    #[must_use]
    pub fn held(&self, now: jiff::Timestamp) -> bool {
        self.held_until.is_some_and(|until| now < until)
    }

    /// Whether another collector's fetch lease is live at `now`: `claimUntil` when the row
    /// has one, else an attempt in the last 10 s.
    #[must_use]
    pub fn claimed(&self, now: jiff::Timestamp) -> bool {
        live_claim(
            self.claim_until.map(secs_from_timestamp),
            self.last_attempt_at.map(secs_from_timestamp),
            secs_from_timestamp(now),
        )
    }
}

/// How long a row with no lease (`claimUntil`) counts as claimed after its last attempt.
const LEGACY_CLAIM_TTL_S: f64 = 10.0;

/// Whether a fetch lease is live at `now`: before `claim_until` when the row has one, else
/// within 10 s of `last_attempt_at`. Times are seconds since the epoch.
pub(crate) fn live_claim(claim_until: Option<f64>, last_attempt_at: Option<f64>, now: f64) -> bool {
    match claim_until {
        Some(claim_until) => now < claim_until,
        None => last_attempt_at.is_some_and(|attempt| now - attempt < LEGACY_CLAIM_TTL_S),
    }
}

/// The account store's usage table, `cache/usage.json`.
pub struct UsageTable;

impl UsageTable {
    /// One entry per account in `identities` (slot number to the account it holds now), read without a lock. A
    /// missing, unreadable or corrupt file, one whose schema is not [`USAGE_SCHEMA_VERSION`], or a root that is a link
    /// or open to another principal, gives every account an empty entry, as does a row naming another account.
    #[must_use]
    pub fn entries(
        root: &AccountsRoot,
        identities: &BTreeMap<u32, Identity>,
    ) -> BTreeMap<u32, UsageEntry> {
        let rows = match read_document(root) {
            CacheDocument::Rows(document) => document,
            CacheDocument::Missing
            | CacheDocument::Corrupt
            | CacheDocument::Unsupported
            | CacheDocument::Unreadable(_) => Map::new(),
        };
        let accounts = rows.get("accounts").and_then(Value::as_object);
        identities
            .iter()
            .map(|(number, identity)| {
                let row = accounts.and_then(|accounts| accounts.get(&number.to_string()));
                let entry = match row {
                    Some(Value::Object(row)) if identity.matches(row) => {
                        UsageEntry::from_row(identity, row)
                    }
                    Some(_) | None => UsageEntry::default(),
                };
                (*number, entry)
            })
            .collect()
    }
}

/// What `cache/usage.json` holds.
pub(crate) enum CacheDocument {
    /// No file yet.
    Missing,
    /// A file that is not a JSON object: torn, empty, or garbage. Read as empty, and rewritten by the next write.
    Corrupt,
    /// A JSON object whose `schemaVersion` is not the integer [`USAGE_SCHEMA_VERSION`]: missing, a string, a float or
    /// another number. Read as empty, never written.
    Unsupported,
    /// The file exists but could not be read, or the root or a folder on the way was refused.
    Unreadable(io::Error),
    /// A table of [`USAGE_SCHEMA_VERSION`], every top-level key kept.
    Rows(Map<String, Value>),
}

/// `cache/usage.json` as read; a root that is a link or open to another principal, or a folder on the way that is a
/// link ([`AccountsRoot::check_folders`]), is unreadable.
///
/// It holds no secret (each row's `email`, `organizationUuid`, `rejectedFingerprint` and `claimId`, beside usage
/// percentages and reset times), so the parsed tree is dropped without scrubbing.
pub(crate) fn read_document(root: &AccountsRoot) -> CacheDocument {
    // The read takes no lock, so it verifies the root itself, as every lock in the root does.
    if let Err(error) = crate::store::check_root(root) {
        return CacheDocument::Unreadable(io::Error::new(io::ErrorKind::InvalidInput, error));
    }
    let path = root.usage_json();
    if let Err(error) = root.check_folders(&path) {
        return CacheDocument::Unreadable(error);
    }
    let bytes = match read_optional_unlinked(&path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return CacheDocument::Missing,
        Err(error) => return CacheDocument::Unreadable(error),
    };
    let Ok(Value::Object(document)) = serde_json::from_slice::<Value>(&bytes) else {
        return CacheDocument::Corrupt;
    };
    // Only the JSON integer is this store's version: `3.0`, `"3"` and a missing key are some other writer's table.
    if document.get("schemaVersion").and_then(Value::as_u64) == Some(USAGE_SCHEMA_VERSION) {
        CacheDocument::Rows(document)
    } else {
        CacheDocument::Unsupported
    }
}

/// The 5-hour and 7-day windows of a reading: `five_hour` and `seven_day` objects with a numeric `pct`.
pub(crate) fn account_windows(reading: &Value) -> (Option<UsageWindow>, Option<UsageWindow>) {
    (window(reading, "five_hour"), window(reading, "seven_day"))
}

fn window(reading: &Value, key: &str) -> Option<UsageWindow> {
    let window = reading.get(key)?.as_object()?;
    let pct = window.get("pct")?.as_f64()?;
    let resets_at = window
        .get("resets_at")
        .and_then(Value::as_str)
        .and_then(|text| text.parse().ok());
    Some(UsageWindow { pct, resets_at })
}

/// A row's time field: a JSON number of seconds since the epoch.
fn row_time(row: &Map<String, Value>, key: &str) -> Option<jiff::Timestamp> {
    row.get(key)
        .and_then(Value::as_f64)
        .and_then(timestamp_from_secs)
}

/// Seconds since the epoch as a timestamp; `None` when out of range or not finite.
pub(crate) fn timestamp_from_secs(seconds: f64) -> Option<jiff::Timestamp> {
    let duration = jiff::SignedDuration::try_from_secs_f64(seconds).ok()?;
    jiff::Timestamp::from_duration(duration).ok()
}

/// A timestamp as seconds since the epoch, as the usage table holds its times.
pub(crate) fn secs_from_timestamp(timestamp: jiff::Timestamp) -> f64 {
    timestamp.as_duration().as_secs_f64()
}

/// `consecutiveFailures` as a count: a number, truncated; anything else is 0.
pub(crate) fn failure_count(value: Option<&Value>) -> u32 {
    let Some(count) = value.and_then(Value::as_f64) else {
        return 0;
    };
    whole_count(count)
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the count is clamped into u32's range first"
)]
fn whole_count(count: f64) -> u32 {
    if count.is_nan() {
        return 0;
    }
    count.clamp(0.0, f64::from(u32::MAX)).trunc() as u32
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test that cannot set up its store root must fail loudly"
)]
mod tests {
    use serde_json::json;

    use super::{CacheDocument, PrepaidEntry};

    #[test]
    fn a_stored_prepaid_with_a_bad_currency_reads_as_none() {
        for currency in [
            json!("usd"),
            json!("US"),
            json!("USDX"),
            json!("U$D"),
            json!(840),
        ] {
            let stored = json!({
                "amountMinor": 102_003, "exponent": 2, "currency": currency,
                "fetchedAt": 1_790_000_000.0, "attemptedAt": 1_790_000_000.0,
            });

            assert_eq!(
                PrepaidEntry::from_value(&stored),
                None,
                "a stored currency that is not three ASCII uppercase letters reads as no prepaid entry: {currency}"
            );
        }
        let stored = json!({
            "amountMinor": 102_003, "exponent": 2, "currency": "USD",
            "fetchedAt": 1_790_000_000.0, "attemptedAt": 1_790_000_000.0,
        });
        assert!(
            PrepaidEntry::from_value(&stored).is_some(),
            "the canonical currency still reads"
        );
    }

    /// A usage cache whose root the store's check refuses keeps the check's own error as the unreadable document's
    /// source.
    #[test]
    fn an_unreadable_usage_cache_keeps_the_check_error_as_its_source() {
        let folder = std::env::temp_dir()
            .join("claude-accounts-unit")
            .join("an_unreadable_usage_cache_keeps_the_check_error_as_its_source");
        let root = crate::paths::AccountsRoot::new(folder.join("store"));
        std::fs::create_dir_all(&folder).expect("the test's folder can be made");
        std::fs::write(root.root(), b"not a folder")
            .expect("the test's file stands where the store root would be");
        let refusal =
            crate::store::check_root(&root).expect_err("a file at the store root is refused");
        let text = refusal.to_string();

        let CacheDocument::Unreadable(error) = super::read_document(&root) else {
            unreachable!("a root the store's check refuses is unreadable");
        };

        assert_eq!(
            error.kind(),
            std::io::ErrorKind::InvalidInput,
            "the I/O kind is unchanged"
        );
        assert_eq!(error.to_string(), text, "the refusal's text is unchanged");
        let kept = error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<crate::store::StoreError>())
            .expect("the check's own error is kept as the source");
        assert_eq!(
            kept.kind(),
            crate::store::StoreErrorKind::RootNotDirectory,
            "the source is the check's own error"
        );
    }
}
