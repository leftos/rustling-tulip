//! Where the engine may switch to and in which order: cswap's candidate census, `_rank_candidates`, and the no-return bar
//! (`_no_return_account`, `_left_account_recovered` and the release retry of `_rank`), as pure functions. A proactive
//! trigger never targets an account at or over the threshold: when every readable account is, the ranking is empty and
//! the tick holds until the active account reaches its limit. The caller passes the trusted readings, the state file and
//! the time; nothing here reads a file or the clock. Times inside the ranking are seconds since the epoch, as cswap
//! computes them and the state file records them.

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::iter;

use jiff::Timestamp;
use serde_json::{Map, Value};

use crate::engine::AutoswitchState;
use crate::engine::reading::relevant_windows;
use crate::engine::trigger::Trigger;
use crate::store::{AccountKind, AccountRecord, AutoSwitchSettings, Strategy};
use crate::usage::{UsageWindow, secs_from_timestamp};

/// cswap's `RECOVERY_HYSTERESIS_S`: a binding reset this many seconds sooner counts as a meaningful improvement to the
/// no-return bar and its release predicate.
pub const RECOVERY_HYSTERESIS_S: f64 = 300.0;
/// cswap's `RECOVERY_HORIZON_S`: a binding window resetting within this many seconds counts as near enough for the
/// failover release predicate.
pub const RECOVERY_HORIZON_S: f64 = 4.0 * 3600.0;
/// cswap's `HORIZON_HEADROOM_RATIO`: an account the no-return bar released must hold this many times the active
/// account's headroom, and the dominance leg of the release predicate compares headroom at this ratio.
pub const HORIZON_HEADROOM_RATIO: f64 = 2.0;
/// cswap's `SPENT_HEADROOM_PCT`: the headroom, in percent, an account must gain on its departure to count as recovered,
/// and the margin the dominance leg adds.
pub const SPENT_HEADROOM_PCT: f64 = 3.0;

/// One account's usage as the ranking reads it: the windows that gate the account, in cswap's order (the 5-hour, then
/// the 7-day window, then each scoped window the configured models name), and the 7-day window's reset, which
/// consume-first ranks by.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccountUsage {
    /// The windows that gate the account; the fullest one binds it.
    pub windows: Vec<UsageWindow>,
    /// When the 7-day window resets, read even when the window has no utilization.
    pub weekly_reset: Option<Timestamp>,
}

impl AccountUsage {
    /// The usage in a trusted reading, as [`crate::decision_value`] returns it: its windows as cswap's
    /// `relevant_windows` reads them for `models` (the settings' `models`), which is the `five_hour` and `seven_day`
    /// windows with a numeric `pct`, then each `scoped` window with a numeric `pct` whose name `models` holds, and the `seven_day` window's
    /// `resets_at`. A reset without a UTC offset is ignored.
    #[must_use]
    pub fn from_reading(reading: &Value, models: &[String]) -> Self {
        let weekly_reset = reading
            .get("seven_day")
            .and_then(|window| window.get("resets_at"))
            .and_then(Value::as_str)
            .and_then(|text| text.parse().ok());
        Self {
            windows: relevant_windows(reading, models)
                .into_iter()
                .map(|(_, window)| window)
                .collect(),
            weekly_reset,
        }
    }

    /// cswap's `account_headroom`: 100 less the binding window's utilization, so at most 0 means at a limit; `None`
    /// with no windows.
    #[must_use]
    pub fn headroom(&self) -> Option<f64> {
        self.binding().map(|window| 100.0 - window.pct)
    }

    /// The binding window: the fullest, the first of equals, as Python's `max` picks it.
    fn binding(&self) -> Option<&UsageWindow> {
        self.windows
            .iter()
            .fold(None, |best: Option<&UsageWindow>, window| match best {
                Some(best) if best.pct >= window.pct => Some(best),
                _ => Some(window),
            })
    }
}

/// The accessor the ranking reads usage through: given an account, its decision windows.
pub trait WindowSource {
    /// `account`'s usage, `None` when it has no trusted reading this tick.
    fn usage(&self, account: u32) -> Option<AccountUsage>;
}

/// The default accessor: each account's trusted reading (what [`crate::decision_value`] returns), read as
/// [`AccountUsage::from_reading`] reads it for the configured models. An account with no entry has no trusted reading.
#[derive(Debug)]
pub struct TrustedReadings<'a, V> {
    /// Each account's trusted reading.
    pub readings: &'a BTreeMap<u32, V>,
    /// The settings' `models`.
    pub models: &'a [String],
}

// By hand rather than derived: a derive would bound `V: Copy`, and the struct holds only references.
impl<V> Clone for TrustedReadings<'_, V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<V> Copy for TrustedReadings<'_, V> {}

impl<'a, V> TrustedReadings<'a, V> {
    /// The `readings`, each read for `models`.
    #[must_use]
    pub fn new(readings: &'a BTreeMap<u32, V>, models: &'a [String]) -> Self {
        Self { readings, models }
    }
}

impl<V: Borrow<Value>> WindowSource for TrustedReadings<'_, V> {
    fn usage(&self, account: u32) -> Option<AccountUsage> {
        self.readings
            .get(&account)
            .map(|reading| AccountUsage::from_reading(reading.borrow(), self.models))
    }
}

/// An account as the candidate census reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CandidateAccount {
    /// The account's number.
    pub number: u32,
    /// OAuth or a managed API key.
    pub kind: AccountKind,
    /// Held out of automatic switching (`cswap disable`).
    pub disabled: bool,
}

impl CandidateAccount {
    /// Account `number` of `kind`, held out of automatic switching when `disabled`.
    #[must_use]
    pub fn new(number: u32, kind: AccountKind, disabled: bool) -> Self {
        Self {
            number,
            kind,
            disabled,
        }
    }
}

impl From<&AccountRecord> for CandidateAccount {
    fn from(record: &AccountRecord) -> Self {
        Self::new(record.number, record.kind, record.disabled)
    }
}

/// The accounts a tick may switch to, in rotation order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Candidates {
    /// The OAuth accounts, which the ranking orders.
    pub oauth: Vec<u32>,
}

/// What the ranking decides on.
#[derive(Clone, Copy)]
pub struct RankInputs<'a> {
    /// Why the tick looks for another account.
    pub trigger: Trigger,
    /// The active account's number.
    pub active: u32,
    /// The accounts the tick may switch to, from [`candidates`].
    pub candidates: &'a Candidates,
    /// Every account's usage this tick: the active one, the candidates and the account the last switch left.
    pub usage: &'a dyn WindowSource,
    /// The shared state file: the last switch and its departure snapshot.
    pub state: &'a AutoswitchState,
    /// The `autoswitch` settings.
    pub settings: &'a AutoSwitchSettings,
    /// The time the decision is made at.
    pub now: Timestamp,
}

/// The ranking's answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ranking {
    /// The accounts to try, best first; empty when none qualifies.
    pub ordered: Vec<u32>,
    /// Whether any OAuth candidate had a readable headroom, chosen or not.
    pub any_known: bool,
    /// Under consume-first, the active account's 7-day reset when it is known and still ahead.
    pub active_weekly_reset: Option<Timestamp>,
}

/// cswap's candidate census (autoswitch.py:1129-1145): the accounts in `accounts` (rotation order) other than the
/// `active` one that are neither disabled nor quarantined in `state`, OAuth accounts only: an API-key account is never
/// switched to. cswap's census also drops accounts with no usable stored backup, which the caller leaves out of
/// `accounts`. A barred account stays in the census: the bar is about the choice.
#[must_use]
pub fn candidates(
    accounts: &[CandidateAccount],
    active: u32,
    state: &AutoswitchState,
) -> Candidates {
    let mut found = Candidates::default();
    for account in accounts {
        if account.disabled
            || account.number == active
            || state.quarantine.contains_key(&account.number.to_string())
        {
            continue;
        }
        if account.kind == AccountKind::Oauth {
            found.oauth.push(account.number);
        }
    }
    found
}

/// cswap's `_rank` (autoswitch.py:1178-1249): the candidates ranked with the no-return bar; when the bar empties the
/// ranking and the account it bars has recovered since it was left, ranked again without the bar if that finds one.
#[must_use]
pub fn rank(inputs: &RankInputs<'_>) -> Ranking {
    let view = View::new(inputs);
    let recovered = left_recovered(&view);
    let no_return = barred_account(&view, recovered);
    let mut ranking = rank_view(&view, no_return);
    if no_return.is_some() && ranking.ordered.is_empty() && recovered {
        let unbarred = rank_view(&view, None);
        if !unbarred.ordered.is_empty() {
            ranking = unbarred;
        }
    }
    ranking
}

/// cswap's `_rank_candidates` (autoswitch.py:1830-2026) with `no_return` barred: the OAuth candidates that qualify for
/// the trigger, in order. An unreadable candidate, one at its limit, or one at or over the threshold on a proactive
/// trigger never qualifies. `best` needs a target to beat the active account by `hysteresis_pct` and rank by most
/// headroom; `consume-first` ranks by soonest weekly reset and for its own trigger needs that reset strictly sooner
/// than the active account's. An escape trigger ranks every readable candidate with headroom. Ties keep rotation order.
#[must_use]
pub fn rank_candidates(inputs: &RankInputs<'_>, no_return: Option<u32>) -> Ranking {
    rank_view(&View::new(inputs), no_return)
}

/// Whether a proactive tick must hold: every readable account, the active included, is at or over the threshold, with
/// at least one readable candidate. An escape trigger ignores this condition and switches as it does when any account
/// has headroom.
#[must_use]
pub fn all_above_threshold(inputs: &RankInputs<'_>) -> bool {
    let view = View::new(inputs);
    every_account_above_threshold(&view, view.active_headroom())
}

/// cswap's `_no_return_account` (autoswitch.py:1459-1578): the account the last switch left, barred on the proactive
/// triggers while the active account is the one that switch landed on (or the state does not say where it landed).
/// When the account left has `recovered` (see [`left_account_recovered`]), it is released if it now holds
/// [`HORIZON_HEADROOM_RATIO`] times the active account's headroom, or, with the active account unreadable, more than
/// 100 less the threshold.
#[must_use]
pub fn no_return_account(inputs: &RankInputs<'_>, recovered: bool) -> Option<u32> {
    barred_account(&View::new(inputs), recovered)
}

/// cswap's `_left_account_recovered` (autoswitch.py:1580-1828): whether the account the last switch left is a better
/// proposition than when it was left. No departure snapshot releases. After a failover (by `leftTrigger`, or both
/// snapshot values null on a record without it), the account must land healthy (more than 100 less the threshold) or
/// its binding reset must be [`RECOVERY_HYSTERESIS_S`] sooner than the active account's, with that reset known or its
/// own within [`RECOVERY_HORIZON_S`]. Otherwise it must dominate the active account (more than
/// [`HORIZON_HEADROOM_RATIO`] times its headroom plus [`SPENT_HEADROOM_PCT`], or land healthy when the active account
/// is unreadable), have gained [`SPENT_HEADROOM_PCT`] on its departure headroom (capped at 100), or reset
/// [`RECOVERY_HYSTERESIS_S`] sooner than at departure.
#[must_use]
pub fn left_account_recovered(inputs: &RankInputs<'_>) -> bool {
    left_recovered(&View::new(inputs))
}

/// cswap's `_binding_recovery_ts` (autoswitch.py:564): when the account's binding window resets, `None` when it has no
/// windows or the binding window's reset is unknown or not after `now` (where cswap answers infinity). The departure
/// snapshot records this as `leftRecoveryAt`.
#[must_use]
pub fn binding_recovery(usage: Option<&AccountUsage>, now: Timestamp) -> Option<Timestamp> {
    usage?.binding()?.resets_at.filter(|reset| *reset > now)
}

/// A ranking key, compared element by element as cswap compares its key tuples; shorter keys are padded with zeros.
type Key = [f64; 3];

/// The tick's usage as the ranking reads it: the active account's, the candidates' and the barred account's.
struct View<'a> {
    inputs: &'a RankInputs<'a>,
    usage: BTreeMap<u32, AccountUsage>,
    now_s: f64,
}

impl<'a> View<'a> {
    fn new(inputs: &'a RankInputs<'a>) -> Self {
        let accounts = iter::once(inputs.active)
            .chain(inputs.candidates.oauth.iter().copied())
            .chain(came_from(inputs.state));
        let usage = accounts
            .filter_map(|account| Some((account, inputs.usage.usage(account)?)))
            .collect();
        Self {
            inputs,
            usage,
            now_s: secs_from_timestamp(inputs.now),
        }
    }

    fn headroom(&self, account: u32) -> Option<f64> {
        self.usage.get(&account)?.headroom()
    }

    fn active_headroom(&self) -> Option<f64> {
        self.headroom(self.inputs.active)
    }

    /// cswap's `_binding_recovery_ts`: the binding window's reset in seconds, infinity when unknown or past.
    fn recovery(&self, account: u32) -> f64 {
        binding_recovery(self.usage.get(&account), self.inputs.now)
            .map_or(f64::INFINITY, secs_from_timestamp)
    }

    /// cswap's `_seven_day_reset_ts`: the weekly reset while it is still ahead.
    fn weekly_reset(&self, account: u32) -> Option<Timestamp> {
        self.usage
            .get(&account)?
            .weekly_reset
            .filter(|reset| *reset > self.inputs.now)
    }

    /// 100 less the threshold: an account with more headroom than this would be a healthy landing.
    fn landing_floor(&self) -> f64 {
        100.0 - self.inputs.settings.threshold
    }
}

/// Where a candidate goes in the ranking.
enum Placement {
    Qualifies(Key),
    Skip,
}

/// What `_rank_candidates` works out once per ranking before it looks at each candidate (autoswitch.py:1851-1899):
/// the strategy, whether every readable account is at or over the threshold, and the active account's headroom and
/// weekly reset.
struct Gates {
    consume_first: bool,
    all_above: bool,
    active_headroom: Option<f64>,
    active_reset: Option<Timestamp>,
}

impl Gates {
    fn new(view: &View<'_>) -> Self {
        let inputs = view.inputs;
        let consume_first = inputs.settings.strategy == Strategy::ConsumeFirst;
        let active_headroom = view.active_headroom();
        let all_above = every_account_above_threshold(view, active_headroom);
        Self {
            consume_first,
            all_above,
            active_headroom,
            active_reset: if consume_first {
                view.weekly_reset(inputs.active)
            } else {
                None
            },
        }
    }

    /// One readable candidate with `headroom` above 0 that is not barred (autoswitch.py:1913-2022).
    fn place(&self, view: &View<'_>, account: u32, headroom: f64) -> Placement {
        let trigger = view.inputs.trigger;
        let reset = if self.consume_first {
            view.weekly_reset(account)
        } else {
            None
        };
        if !trigger.is_escape() {
            if self.all_above {
                return Placement::Skip;
            }
            if 100.0 - headroom >= view.inputs.settings.threshold
                || !self.beats_active(view, trigger, reset, headroom)
            {
                return Placement::Skip;
            }
        }
        if self.consume_first {
            Placement::Qualifies([
                reset.map_or(f64::INFINITY, secs_from_timestamp),
                -headroom,
                0.0,
            ])
        } else {
            Placement::Qualifies([-headroom, 0.0, 0.0])
        }
    }

    /// The strategy's gate below the threshold: consume-first's strictly sooner weekly reset (for its own trigger
    /// only), or best's hysteresis margin over a readable active account.
    fn beats_active(
        &self,
        view: &View<'_>,
        trigger: Trigger,
        reset: Option<Timestamp>,
        headroom: f64,
    ) -> bool {
        if self.consume_first {
            return trigger != Trigger::ConsumeFirst
                || matches!((reset, self.active_reset), (Some(reset), Some(active)) if reset < active);
        }
        self.active_headroom
            .is_none_or(|active| headroom - active >= view.inputs.settings.hysteresis_pct)
    }
}

/// `_rank_candidates` on a view, with `no_return` barred.
fn rank_view(view: &View<'_>, no_return: Option<u32>) -> Ranking {
    let gates = Gates::new(view);
    let mut qualifying = Vec::new();
    let mut any_known = false;
    for &account in &view.inputs.candidates.oauth {
        let Some(headroom) = view.headroom(account) else {
            continue;
        };
        any_known = true;
        if headroom <= 0.0 || no_return == Some(account) {
            continue;
        }
        match gates.place(view, account, headroom) {
            Placement::Qualifies(key) => qualifying.push((key, account)),
            Placement::Skip => {}
        }
    }
    let mut chosen = qualifying;
    // A stable sort: rotation order breaks ties, as cswap's does.
    chosen.sort_by(|left, right| compare_keys(&left.0, &right.0));
    Ranking {
        ordered: chosen.into_iter().map(|(_, account)| account).collect(),
        any_known,
        active_weekly_reset: gates.active_reset,
    }
}

fn compare_keys(left: &Key, right: &Key) -> Ordering {
    left.iter()
        .zip(right)
        .map(|(left, right)| left.partial_cmp(right).unwrap_or(Ordering::Equal))
        .find(|order| order.is_ne())
        .unwrap_or(Ordering::Equal)
}

fn candidate_headrooms<'v>(view: &'v View<'_>) -> impl Iterator<Item = f64> + 'v {
    view.inputs
        .candidates
        .oauth
        .iter()
        .filter_map(|&account| view.headroom(account))
}

/// cswap's `_every_account_above_threshold` (autoswitch.py:598): the active account readable and at or over the
/// threshold, and every readable candidate too, with at least one readable. This is the condition under which a
/// proactive tick holds: no candidate is a target, and the active account runs to its limit.
fn every_account_above_threshold(view: &View<'_>, active_headroom: Option<f64>) -> bool {
    let threshold = view.inputs.settings.threshold;
    if active_headroom.is_none_or(|active| 100.0 - active < threshold) {
        return false;
    }
    let mut measured = candidate_headrooms(view).peekable();
    measured.peek().is_some() && measured.all(|headroom| 100.0 - headroom >= threshold)
}

/// cswap's `_no_return_account` on a view.
fn barred_account(view: &View<'_>, recovered: bool) -> Option<u32> {
    let inputs = view.inputs;
    if inputs.trigger.is_escape() {
        return None;
    }
    let barred = came_from(inputs.state)?;
    if landed_elsewhere(inputs.state, inputs.active) {
        return None;
    }
    if !recovered {
        return Some(barred);
    }
    let beats_active = view
        .headroom(barred)
        .is_some_and(|left| match view.active_headroom() {
            Some(active) => left >= active * HORIZON_HEADROOM_RATIO,
            None => left > view.landing_floor(),
        });
    (!beats_active).then_some(barred)
}

/// cswap's `_left_account_recovered` on a view.
fn left_recovered(view: &View<'_>) -> bool {
    let state = view.inputs.state;
    let Some(barred) = came_from(state) else {
        return true;
    };
    let left_headroom = Recorded::read(
        state.left_headroom.map(Recorded::nullable),
        &state.extra,
        "leftHeadroom",
    );
    if left_headroom == Recorded::Absent {
        return true;
    }
    let left_recovery = Recorded::read(
        state.left_recovery_at.map(Recorded::nullable),
        &state.extra,
        "leftRecoveryAt",
    );
    let failover = left_trigger_is_failover(state)
        .unwrap_or(left_headroom.is_none() && left_recovery.is_none());
    if failover {
        failover_recovered(view, barred)
    } else {
        ordinary_recovered(view, barred, left_headroom, left_recovery)
    }
}

/// The failover legs (autoswitch.py:1747-1780): a healthy landing now, or a binding reset meaningfully sooner than the
/// active account's, when that reset is known or the barred account's own is within the horizon.
fn failover_recovered(view: &View<'_>, barred: u32) -> bool {
    if view
        .headroom(barred)
        .is_some_and(|headroom| headroom > view.landing_floor())
    {
        return true;
    }
    let peer = view.recovery(barred);
    let active = view.recovery(view.inputs.active);
    (active.is_finite() || peer - view.now_s <= RECOVERY_HORIZON_S)
        && peer < active - RECOVERY_HYSTERESIS_S
}

/// The ordinary legs (autoswitch.py:1809-1828): dominance over the active account, headroom gained since the
/// departure, or a binding reset meaningfully sooner than at the departure.
fn ordinary_recovered(
    view: &View<'_>,
    barred: u32,
    left_headroom: Recorded,
    left_recovery: Recorded,
) -> bool {
    if let Some(headroom) = view.headroom(barred) {
        let dominant = match view.active_headroom() {
            Some(active) => headroom > active * HORIZON_HEADROOM_RATIO + SPENT_HEADROOM_PCT,
            None => headroom > view.landing_floor(),
        };
        let gained = match left_headroom {
            Recorded::Number(left) => headroom >= (left + SPENT_HEADROOM_PCT).min(100.0),
            _ => false,
        };
        if dominant || gained {
            return true;
        }
    }
    let was = match left_recovery {
        Recorded::Number(recovery) => recovery,
        _ => f64::INFINITY,
    };
    view.recovery(barred) < was - RECOVERY_HYSTERESIS_S
}

/// A departure snapshot value as cswap reads it from the state file.
#[derive(Clone, Copy, PartialEq)]
enum Recorded {
    /// The key is not in the file.
    Absent,
    /// The key is null.
    Null,
    Number(f64),
    /// The key holds something other than a number or null.
    Other,
}

impl Recorded {
    /// The value of `key`: `typed` when the state read it as a number or null, else whatever stayed in `extra`.
    fn read(typed: Option<Self>, extra: &Map<String, Value>, key: &str) -> Self {
        typed.unwrap_or(if extra.contains_key(key) {
            Self::Other
        } else {
            Self::Absent
        })
    }

    /// A value the state read as a number or null.
    fn nullable(value: Option<f64>) -> Self {
        value.map_or(Self::Null, Self::Number)
    }

    /// Python's `None`: absent or null.
    fn is_none(self) -> bool {
        matches!(self, Self::Absent | Self::Null)
    }
}

/// The account the last switch left (`lastSwitchFrom`): the number cswap writes, or a numeric string.
fn came_from(state: &AutoswitchState) -> Option<u32> {
    state
        .last_switch_from
        .or_else(|| state.extra.get("lastSwitchFrom")?.as_str()?.parse().ok())
}

/// Whether `lastSwitchTo` names an account other than `active`, compared as text the way cswap compares it. A state
/// without it cannot show the engine moved away, so it keeps the bar.
fn landed_elsewhere(state: &AutoswitchState, active: u32) -> bool {
    if let Some(landed) = &state.last_switch_to {
        return *landed != active.to_string();
    }
    match state.extra.get("lastSwitchTo") {
        None | Some(Value::Null) => false,
        Some(landed) => landed.as_u64() != Some(u64::from(active)),
    }
}

/// Whether `leftTrigger` records a failover; `None` when the state has no `leftTrigger`.
fn left_trigger_is_failover(state: &AutoswitchState) -> Option<bool> {
    if let Some(trigger) = &state.left_trigger {
        return Some(trigger == Trigger::Failover.as_str());
    }
    match state.extra.get("leftTrigger") {
        None | Some(Value::Null) => None,
        Some(_) => Some(false),
    }
}
