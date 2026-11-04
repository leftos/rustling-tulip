//! The usage reading a switch decision may trust: cswap's `UsageEntry.decision_value` less the sentinel, with the
//! `trust_extended` that `UsageStore.entries` computes for it.

use jiff::Timestamp;
use serde_json::Value;

use crate::usage::{UsageEntry, UsageWindow, account_windows};

/// cswap's `STALE_OK_S`: a reading this young is trusted for decisions.
const STALE_OK_S: f64 = 300.0;
/// cswap's `TRUST_MAX_AGE_S`: deliberate staleness keeps a reading trusted at most this long after its fetch.
const TRUST_MAX_AGE_S: f64 = 3600.0;
/// cswap's `RATE_LIMIT_TRUST_MAX_AGE_S`: after a 429, the ceiling when no window reset comes sooner.
const RATE_LIMIT_TRUST_MAX_AGE_S: f64 = 7200.0;

/// cswap's `decision_value` less the sentinel, which the caller checks first: `entry`'s last good reading while its
/// age at `now` is at most cswap's `STALE_OK_S` (300 s), or while its staleness is deliberate (cswap's
/// `trust_extended`): within the age ceiling (after a 429, until the earliest relevant window resets and at most
/// 7200 s; otherwise 3600 s), and failing, before its planned poll, under a live fetch lease, or held for another
/// machine's hand-over. `models` names the per-model scoped windows whose resets count after a 429 (`all` for every
/// one): the settings' `models` as written, matched to a window's name case-insensitively. A `resets_at`
/// without a UTC offset is ignored, where cswap would read it as local time. `None` when the reading is not to be
/// trusted: the switch decision treats the account's headroom as unknown. The caller checks the slot's sentinel (a
/// poll outcome such as an expired token) first, as cswap's `decision_value` does before this.
#[must_use]
pub fn decision_value<'a>(
    entry: &'a UsageEntry,
    models: &[String],
    now: Timestamp,
) -> Option<&'a Value> {
    let reading = entry.last_good.as_ref()?;
    let age = seconds_between(now, entry.fetched_at?);
    (age <= STALE_OK_S || trust_extended(entry, age, models, now)).then_some(reading)
}

/// cswap's `trust_extended` from `UsageStore.entries`: within the age ceiling (after a 429, until the earliest window
/// reset; otherwise [`TRUST_MAX_AGE_S`]), and stale on purpose: failing, before its planned poll, under a live fetch
/// lease, or held for another machine's hand-over.
fn trust_extended(entry: &UsageEntry, age: f64, models: &[String], now: Timestamp) -> bool {
    let within_ceiling = if entry.last_error.as_deref() == Some("http-429") {
        rate_limited_trust_ok(entry.last_good.as_ref(), age, models, now)
    } else {
        age <= TRUST_MAX_AGE_S
    };
    let scheduled = entry.next_poll_at.is_some_and(|next| now < next);
    let held = entry.held_until.is_some_and(|until| now < until);
    within_ceiling && (entry.consecutive_failures > 0 || scheduled || entry.claimed(now) || held)
}

/// cswap's `_rate_limited_trust_ok`: a reading frozen by 429s stays trusted until the earliest relevant window resets,
/// and never past [`RATE_LIMIT_TRUST_MAX_AGE_S`] after its fetch.
fn rate_limited_trust_ok(
    reading: Option<&Value>,
    age: f64,
    models: &[String],
    now: Timestamp,
) -> bool {
    let before_reset = reading
        .and_then(|reading| earliest_reset(reading, models))
        .is_none_or(|reset| now < reset);
    age < RATE_LIMIT_TRUST_MAX_AGE_S && before_reset
}

/// cswap's `_earliest_reset` over `oauth.relevant_windows`: the soonest `resets_at` among the windows
/// [`relevant_windows`] gives for `models`, counting only a reset that parses as an RFC 3339 time.
fn earliest_reset(reading: &Value, models: &[String]) -> Option<Timestamp> {
    relevant_windows(reading, models)
        .into_iter()
        .filter_map(|(_, window)| window.resets_at)
        .min()
}

/// cswap's `relevant_windows`: every window that gates an account, under the label cswap's `_window_pcts` shows it by.
/// The `five_hour` (`5h`) and `seven_day` (`7d`) windows with a numeric `pct`, then each `scoped` window with a numeric
/// `pct` and a name `models` holds (matched case-insensitively; `all` holds every name), under that name, in the
/// reading's order.
pub(crate) fn relevant_windows(reading: &Value, models: &[String]) -> Vec<(String, UsageWindow)> {
    let (five_hour, seven_day) = account_windows(reading);
    let mut windows: Vec<(String, UsageWindow)> = [("5h", five_hour), ("7d", seven_day)]
        .into_iter()
        .filter_map(|(label, window)| Some((label.to_owned(), window?)))
        .collect();
    let scoped = reading
        .get("scoped")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    windows.extend(
        scoped
            .filter(|window| scoped_window_counts(window, models))
            .filter_map(scoped_window),
    );
    windows
}

/// A `scoped` window with a string `name` and a numeric `pct`, under its name.
fn scoped_window(window: &Value) -> Option<(String, UsageWindow)> {
    let name = window.get("name")?.as_str()?;
    let pct = window.get("pct")?.as_f64()?;
    let resets_at = window
        .get("resets_at")
        .and_then(Value::as_str)
        .and_then(|text| text.parse().ok());
    Some((name.to_owned(), UsageWindow { pct, resets_at }))
}

/// cswap's `parse_model_names`: the `autoswitch.model` setting split on commas, each name trimmed, empty names dropped,
/// and each name kept once in its first spelling, compared case-insensitively. `all` stays a name; the matching gives
/// it its meaning.
#[must_use]
pub fn parse_model_names(setting: Option<&str>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for name in setting.unwrap_or_default().split(',').map(str::trim) {
        let low = name.to_lowercase();
        if !name.is_empty() && !names.iter().any(|known| known.to_lowercase() == low) {
            names.push(name.to_owned());
        }
    }
    names
}

/// Whether a reading's `scoped` window counts toward an account's decision windows: its string `name` is one of
/// `models` (the settings' `models`), matched case-insensitively, or
/// `models` holds `all`. A window without a string `name` never counts, and nothing counts when `models` is empty.
#[must_use]
pub fn scoped_window_counts(window: &Value, models: &[String]) -> bool {
    let Some(name) = window.get("name").and_then(Value::as_str) else {
        return false;
    };
    let name = name.to_lowercase();
    models.iter().any(|model| {
        let model = model.to_lowercase();
        model == "all" || model == name
    })
}

fn seconds_between(later: Timestamp, earlier: Timestamp) -> f64 {
    later.duration_since(earlier).as_secs_f64()
}
