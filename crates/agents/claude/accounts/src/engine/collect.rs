//! Collecting the usage readings an auto-switch decision runs on, cswap's `_collect_warmup_usage` and
//! `_collect_scheduled_usage`: the start-up warm-up forces one fetch of every account not polled since the engine
//! started, and the scheduled collection fetches the active account and one due candidate, then every candidate when a
//! switch could be near. Each collection polls through [`poll_with_refresh`] and reads the usage table back after it,
//! except the exit target's re-read ([`collect_recheck`]), which never refreshes.
//!
//! A poll's [`PollResult`] stands in for cswap's per-entry sentinel and dead-token verdict: an API key, no or unreadable
//! credentials, an expired or session-held token, a struck lineage, a foreign live credential, a deferred refresh, a
//! failed live write, or a disabled account has nothing a fetch could change this pass.

use std::collections::{BTreeMap, BTreeSet};
use std::iter;

use jiff::Timestamp;

use crate::engine::{AccountUsage, decision_value};
use crate::poll::{
    ACTIVE_MAX_INTERVAL_S, ESCALATION_MARGIN_PCT, EXHAUSTED_INTERVAL_S, FetchMode, FetchScope,
    MIN_INTERVAL_S, PollError, PollOutcome, PollResult, plan_oversleeps, poll_read_only,
    poll_with_refresh,
};
use crate::refresh::RefreshContext;
use crate::store::AutoSwitchSettings;
use crate::transport::UsageTransport;
use crate::usage::{Identity, UsageEntry, UsageTable, secs_from_timestamp};

/// What one collection left behind: each account's usage table entry, read after its fetches, and what the poll showed
/// for it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Collected {
    /// Each account of `accounts.json`'s usage table entry.
    pub entries: BTreeMap<u32, UsageEntry>,
    /// Each account's poll result; a later phase's result for an account replaces an earlier one.
    /// A later phase's `NotDue` can hide an earlier `Failed`; the entry keeps the failure.
    pub results: BTreeMap<u32, PollResult>,
}

impl Collected {
    /// Takes `later`'s entries and results over this collection's, account by account.
    fn merge(&mut self, later: Self) {
        self.entries.extend(later.entries);
        self.results.extend(later.results);
    }
}

/// A warm-up collection, and how many of its accounts the warm-up gate has nothing left to wait for ([`warm_settled`]).
#[derive(Clone, Debug, PartialEq)]
pub struct WarmUp {
    /// The entries and results the warm-up left behind.
    pub collected: Collected,
    /// The members settled: polled since the start, or unpollable for now.
    pub settled: usize,
    /// The members: the active account and the candidates.
    pub total: usize,
}

/// What a scheduled collection needs beyond the poll's own inputs.
#[derive(Clone, Copy, Debug)]
pub struct ScheduleInputs<'a> {
    /// The active account.
    pub current: u32,
    /// The accounts a switch could move to, quarantined and non-switchable ones already left out; the active account is
    /// skipped if listed.
    pub candidates: &'a [u32],
    /// Whether the engine idle-holds: no candidate is polled, only the active account.
    pub idle_hold: bool,
    /// The auto-switch threshold in percent, as the tick read it.
    pub threshold: f64,
    /// The settings' `models`, as [`decision_value`] reads them.
    pub models: &'a [String],
    /// Whether the engine spends on the active account's prepaid credits (its paid fallback): phase B then also leaves
    /// out an exhausted account whose valid plan is exactly `EXHAUSTED_INTERVAL_S`, which already has a planned fetch.
    pub paid_mode: bool,
}

/// Whether a fetch of the account concluded at or after `since`, cswap's `_polled_since`: a success then, or a recorded
/// failure then whose row no longer carries a live lease (a live one is a concurrent collector mid-fetch).
#[must_use]
pub fn polled_since(entry: Option<&UsageEntry>, since: Timestamp, now: Timestamp) -> bool {
    let Some(entry) = entry else {
        return false;
    };
    if entry.fetched_at.is_some_and(|fetched| fetched >= since) {
        return true;
    }
    entry.consecutive_failures > 0
        && entry
            .last_attempt_at
            .is_some_and(|attempt| attempt >= since)
        && !entry.claimed(now)
}

/// Whether the warm-up gate has nothing left to wait for on the account, cswap's `_warm_settled`: no entry at all,
/// polled since `since` ([`polled_since`]), or unpollable for now: a poll `result` that stands for a cswap sentinel or a
/// dead token, a failure backoff, or another machine's hold.
#[must_use]
pub fn warm_settled(
    entry: Option<&UsageEntry>,
    result: Option<&PollResult>,
    since: Timestamp,
    now: Timestamp,
) -> bool {
    let Some(entry) = entry else {
        return true;
    };
    polled_since(Some(entry), since, now)
        || result.is_some_and(is_sentinel)
        || entry.in_backoff(now)
        || entry.held(now)
}

/// The due candidate with the stalest reading, cswap's `due_candidate`: never fetched first, then the oldest fetch. Due
/// means its poll `results` show no sentinel or dead token, it is out of failure backoff and not held, and it is past its
/// planned poll, has none, or has one no planner could have written. Ties go to the lower account number, compared as a
/// number: cswap compares the numbers as strings, so it would pick 10 before 9.
#[must_use]
pub fn due_candidate(
    candidates: &[u32],
    entries: &BTreeMap<u32, UsageEntry>,
    results: &BTreeMap<u32, PollResult>,
    now: Timestamp,
) -> Option<u32> {
    let mut due: Vec<(bool, Timestamp, u32)> = Vec::new();
    for &number in candidates {
        if results.get(&number).is_some_and(is_sentinel) {
            continue;
        }
        let Some(entry) = entries.get(&number) else {
            due.push((false, Timestamp::MIN, number));
            continue;
        };
        let planned = entry.next_poll_at.is_some_and(|next| now < next) && !oversleeps(entry, now);
        if entry.in_backoff(now) || entry.held(now) || planned {
            continue;
        }
        due.push(match entry.fetched_at {
            None => (false, Timestamp::MIN, number),
            Some(fetched) => (true, fetched, number),
        });
    }
    due.into_iter().min().map(|(_, _, number)| number)
}

/// The start-up warm-up, cswap's `_collect_warmup_usage`: the members are `current` then `candidates` (the caller has
/// left out quarantined and non-switchable accounts). Each member with no fetch concluded since `warm_since` is fetched
/// once with [`FetchMode::Force`], past the serve TTL and its poll plan; strikes, backoff, holds and live leases still
/// refuse. No idle-hold or escalation runs here.
///
/// # Errors
///
/// As [`poll_with_refresh`].
pub fn collect_warmup<T: UsageTransport + ?Sized>(
    transport: &T,
    settings: &AutoSwitchSettings,
    context: &RefreshContext<'_>,
    current: u32,
    candidates: &[u32],
    warm_since: Timestamp,
) -> Result<WarmUp, PollError> {
    let members: Vec<u32> = iter::once(current)
        .chain(others(current, candidates))
        .collect();
    let before = read_entries(context)?;
    let now = (context.clock)();
    let pending: BTreeSet<u32> = members
        .iter()
        .copied()
        .filter(|number| !polled_since(before.get(number), warm_since, now))
        .collect();
    let collected = collect(
        transport,
        settings,
        context,
        FetchScope::Only {
            accounts: &pending,
            mode: FetchMode::Force,
        },
    )?;
    let now = (context.clock)();
    let settled = members
        .iter()
        .filter(|number| {
            warm_settled(
                collected.entries.get(number),
                collected.results.get(number),
                warm_since,
                now,
            )
        })
        .count();
    Ok(WarmUp {
        collected,
        settled,
        total: members.len(),
    })
}

/// The scheduled two-phase collection, cswap's `_collect_scheduled_usage`. Phase A fetches the active account when its
/// own row nominates it (never fetched, a candidate-cadence plan older than the active cap, a plan no planner could have
/// written, past its plan, or no plan and at least 180 s old) and, unless the engine idle-holds, the one
/// [`due_candidate`]; it runs with [`FetchMode::Escalation`] when the active account's candidate-cadence plan is being
/// overridden, else [`FetchMode::Scheduled`]. Phase B fetches the active account and every candidate with
/// [`FetchMode::Escalation`] when a switch could be near: there are candidates, and the active account's headroom is
/// unknown while its token is not merely expired, or its utilization is within 15 points of the threshold. Phase B leaves
/// out an account at its limit whose valid plan is wider than the exhausted cadence, so a post-429 plan survives.
///
/// Phase B's check and the readings it trusts are judged at a fresh read of the clock taken after phase A: cswap judges
/// its plans at the clock it read before phase A, so this one ages the readings phase A just fetched by no more than the
/// fetches took.
///
/// # Errors
///
/// As [`poll_with_refresh`].
pub fn collect_scheduled<T: UsageTransport + ?Sized>(
    transport: &T,
    settings: &AutoSwitchSettings,
    context: &RefreshContext<'_>,
    inputs: &ScheduleInputs<'_>,
) -> Result<Collected, PollError> {
    let candidates = others(inputs.current, inputs.candidates);
    let nothing = BTreeSet::new();
    let pre = collect(
        transport,
        settings,
        context,
        FetchScope::Only {
            accounts: &nothing,
            mode: FetchMode::Scheduled,
        },
    )?;
    let now = (context.clock)();
    let active = pre.entries.get(&inputs.current);
    let stale_candidate_plan =
        active.is_some_and(|entry| stale_candidate_plan(entry, inputs.models, now));
    let mut plan = BTreeSet::new();
    if active.is_none_or(|entry| stale_candidate_plan || active_due(entry, now)) {
        plan.insert(inputs.current);
    }
    if !inputs.idle_hold
        && let Some(pick) = due_candidate(&candidates, &pre.entries, &pre.results, now)
    {
        plan.insert(pick);
    }
    let mode = if stale_candidate_plan {
        FetchMode::Escalation
    } else {
        FetchMode::Scheduled
    };
    let mut collected = collect(
        transport,
        settings,
        context,
        FetchScope::Only {
            accounts: &plan,
            mode,
        },
    )?;
    let decided_at = (context.clock)();
    if candidates.is_empty() || !escalates(&collected, inputs, decided_at) {
        return Ok(collected);
    }
    let escalation = escalation_set(&collected, &candidates, inputs, decided_at);
    let escalated = collect(
        transport,
        settings,
        context,
        FetchScope::Only {
            accounts: &escalation,
            mode: FetchMode::Escalation,
        },
    )?;
    collected.merge(escalated);
    Ok(collected)
}

/// The consume-first phase-2 refetch, cswap's `usage_entries_by_account(fetch={current, *candidates})` in its
/// two-phase commit: `members` (the active account and the candidates) are fetched with [`FetchMode::Escalation`], so a
/// reading past the serve TTL is fetched again while strikes, backoff, holds and live leases still refuse and serve the
/// stored entry. Unlike the warm-up it never forces.
///
/// # Errors
///
/// As [`poll_with_refresh`].
pub fn collect_refetch<T: UsageTransport + ?Sized>(
    transport: &T,
    settings: &AutoSwitchSettings,
    context: &RefreshContext<'_>,
    members: &[u32],
) -> Result<Collected, PollError> {
    let accounts: BTreeSet<u32> = members.iter().copied().collect();
    collect(
        transport,
        settings,
        context,
        FetchScope::Only {
            accounts: &accounts,
            mode: FetchMode::Escalation,
        },
    )
}

/// The exit target's re-read: account `number` alone, fetched with [`FetchMode::Escalation`] on the read-only path
/// ([`poll_once`](crate::poll::poll_once)'s), with its stored access token only. Nothing is refreshed and no grant is
/// spent: an expired token is refused unsent and a 401 is recorded as a failure, so neither leaves the entry fetched
/// anew. Strikes, backoff, holds and live leases still refuse and serve the stored entry.
///
/// # Errors
///
/// As [`poll_once`](crate::poll::poll_once).
pub fn collect_recheck<T: UsageTransport + ?Sized>(
    transport: &T,
    settings: &AutoSwitchSettings,
    context: &RefreshContext<'_>,
    number: u32,
) -> Result<Collected, PollError> {
    let accounts = BTreeSet::from([number]);
    let scope = FetchScope::Only {
        accounts: &accounts,
        mode: FetchMode::Escalation,
    };
    let outcomes = poll_read_only(transport, settings, context, scope)?;
    collected(context, outcomes)
}

/// Whether a poll result stands for one of cswap's usage sentinels or its dead-token verdict: nothing a fetch could
/// change this pass.
pub(crate) fn is_sentinel(result: &PollResult) -> bool {
    match result {
        PollResult::Disabled
        | PollResult::NoCredentials
        | PollResult::CredentialsUnreadable(_)
        | PollResult::AuthDead
        | PollResult::ForeignCredential
        | PollResult::TokenExpired
        | PollResult::Deferred(_)
        | PollResult::LiveWriteFailed => true,
        PollResult::NotDue
        | PollResult::Fetched { .. }
        | PollResult::Failed(_)
        | PollResult::Fenced
        | PollResult::StoreUnmirrored
        | PollResult::ConsumeBusy
        | PollResult::StashUnreadable
        | PollResult::InvalidClient
        | PollResult::Draining => false,
    }
}

/// Whether a poll result is one cswap shows as its `token expired` sentinel: an expired token left unsent, a refresh
/// put off before anything was spent, or a successor the live file could not take.
pub(crate) fn shows_expired(result: &PollResult) -> bool {
    match result {
        PollResult::TokenExpired | PollResult::Deferred(_) | PollResult::LiveWriteFailed => true,
        PollResult::Disabled
        | PollResult::NoCredentials
        | PollResult::CredentialsUnreadable(_)
        | PollResult::AuthDead
        | PollResult::ForeignCredential
        | PollResult::NotDue
        | PollResult::Fetched { .. }
        | PollResult::Failed(_)
        | PollResult::Fenced
        | PollResult::StoreUnmirrored
        | PollResult::ConsumeBusy
        | PollResult::StashUnreadable
        | PollResult::InvalidClient
        | PollResult::Draining => false,
    }
}

/// Polls within `scope` under `settings`, then reads every account's usage table entry back.
fn collect<T: UsageTransport + ?Sized>(
    transport: &T,
    settings: &AutoSwitchSettings,
    context: &RefreshContext<'_>,
    scope: FetchScope<'_>,
) -> Result<Collected, PollError> {
    let outcomes = poll_with_refresh(transport, settings, context, scope)?;
    collected(context, outcomes)
}

/// The poll's `outcomes`, with every account's usage table entry read back after it.
fn collected(
    context: &RefreshContext<'_>,
    outcomes: Vec<PollOutcome>,
) -> Result<Collected, PollError> {
    let results = outcomes
        .into_iter()
        .map(|outcome| (outcome.number, outcome.result))
        .collect();
    Ok(Collected {
        entries: read_entries(context)?,
        results,
    })
}

/// Each account of `accounts.json`'s usage table entry, read without a lock; none when there is no `accounts.json`.
fn read_entries(context: &RefreshContext<'_>) -> Result<BTreeMap<u32, UsageEntry>, PollError> {
    let Some(sequence) = context.store.read_sequence()? else {
        return Ok(BTreeMap::new());
    };
    let identities = sequence
        .records()
        .map(|record| (record.number, Identity::of(record)))
        .collect();
    Ok(UsageTable::entries(context.store.root(), &identities))
}

/// `candidates` without `current`.
fn others(current: u32, candidates: &[u32]) -> Vec<u32> {
    candidates
        .iter()
        .copied()
        .filter(|number| *number != current)
        .collect()
}

/// The account's headroom on its trusted reading, `None` when unknown: a sentinel result, or no reading to trust.
fn headroom(collected: &Collected, number: u32, models: &[String], now: Timestamp) -> Option<f64> {
    if collected.results.get(&number).is_some_and(is_sentinel) {
        return None;
    }
    let reading = decision_value(collected.entries.get(&number)?, models, now)?;
    AccountUsage::from_reading(reading, models).headroom()
}

/// Whether the active account's own row nominates it: never fetched, a plan no planner could have written, past its
/// plan, or no plan and at least [`MIN_INTERVAL_S`] old.
fn active_due(entry: &UsageEntry, now: Timestamp) -> bool {
    let Some(fetched_at) = entry.fetched_at else {
        return true;
    };
    if oversleeps(entry, now) {
        return true;
    }
    match entry.next_poll_at {
        Some(next) => now >= next,
        None => seconds_between(now, fetched_at) >= MIN_INTERVAL_S,
    }
}

/// Whether the active account carries a plan written while it was a candidate (wider than the active cadence's cap)
/// and its reading is at least that cap old, while its binding window is not exhausted: the plan is overridden. The
/// binding window is the fullest of the 5-hour and 7-day windows and the scoped windows `models` names, as cswap's
/// `relevant_windows` gives them.
fn stale_candidate_plan(entry: &UsageEntry, models: &[String], now: Timestamp) -> bool {
    let old = entry
        .fetched_at
        .is_some_and(|fetched| seconds_between(now, fetched) >= ACTIVE_MAX_INTERVAL_S);
    let wide = entry.poll_interval_s.unwrap_or(0.0) > ACTIVE_MAX_INTERVAL_S;
    let binding_pct = entry
        .last_good
        .as_ref()
        .and_then(|reading| AccountUsage::from_reading(reading, models).headroom())
        .map_or(0.0, |headroom| 100.0 - headroom);
    old && wide && binding_pct < 100.0
}

/// Whether phase B runs: the active account's headroom is unknown while it does not show an expired token, or its
/// utilization is within [`ESCALATION_MARGIN_PCT`] of the threshold.
fn escalates(collected: &Collected, inputs: &ScheduleInputs<'_>, now: Timestamp) -> bool {
    match headroom(collected, inputs.current, inputs.models, now) {
        None => !collected
            .results
            .get(&inputs.current)
            .is_some_and(shows_expired),
        Some(headroom) => 100.0 - headroom >= inputs.threshold - ESCALATION_MARGIN_PCT,
    }
}

/// The active account and every candidate, less those at their limit on a trusted reading whose valid plan is wider
/// than [`EXHAUSTED_INTERVAL_S`], or at least that wide in paid mode.
fn escalation_set(
    collected: &Collected,
    candidates: &[u32],
    inputs: &ScheduleInputs<'_>,
    now: Timestamp,
) -> BTreeSet<u32> {
    let planned_wide = |interval: f64| {
        if inputs.paid_mode {
            interval >= EXHAUSTED_INTERVAL_S
        } else {
            interval > EXHAUSTED_INTERVAL_S
        }
    };
    let mut fetch = BTreeSet::new();
    for number in iter::once(inputs.current).chain(candidates.iter().copied()) {
        let wide_plan = collected.entries.get(&number).is_some_and(|entry| {
            entry.next_poll_at.is_some_and(|next| now < next)
                && planned_wide(entry.poll_interval_s.unwrap_or(0.0))
        });
        let exhausted =
            headroom(collected, number, inputs.models, now).is_some_and(|headroom| headroom <= 0.0);
        if !(wide_plan && exhausted) {
            fetch.insert(number);
        }
    }
    fetch
}

/// Whether the entry's planned poll lies further out than any plan the planner could make from its interval.
fn oversleeps(entry: &UsageEntry, now: Timestamp) -> bool {
    plan_oversleeps(
        entry.next_poll_at.map(secs_from_timestamp),
        entry.poll_interval_s,
        secs_from_timestamp(now),
    )
}

fn seconds_between(later: Timestamp, earlier: Timestamp) -> f64 {
    secs_from_timestamp(later) - secs_from_timestamp(earlier)
}
