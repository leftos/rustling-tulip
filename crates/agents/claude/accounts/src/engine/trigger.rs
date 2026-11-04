//! What makes the engine look for another account on a tick, and the cooldown that holds the optional moves back: the
//! trigger classification and `_in_cooldown` of cswap's engine, as pure functions of the active account's reading,
//! the engine's memory of earlier ticks, the settings and the time.

use jiff::Timestamp;

use crate::engine::AutoswitchState;
use crate::store::{AutoSwitchSettings, Strategy};
use crate::usage::secs_from_timestamp;

/// cswap's `IDLE_HOLD_MAX_S`: how long an expired active token is read as Claude Code being idle before the engine
/// counts the account as unhealthy again.
pub const IDLE_HOLD_MAX_S: f64 = 30.0 * 60.0;

/// Why the engine looks for another account, as cswap names it in its events and in the state file's `leftTrigger`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Trigger {
    /// The active account is at or over a limit (`at-limit`): an escape.
    AtLimit,
    /// The active account is at or over the threshold (`proactive`).
    Proactive,
    /// The consume-first strategy, below the threshold, looks for an account whose weekly window resets sooner
    /// (`consume-first`).
    ConsumeFirst,
    /// The active account's usage has been unreadable for `unhealthy_ticks` ticks (`failover`): an escape.
    Failover,
}

impl Trigger {
    /// The name cswap writes for the trigger: `at-limit`, `proactive`, `consume-first` or `failover`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AtLimit => "at-limit",
            Self::Proactive => "proactive",
            Self::ConsumeFirst => "consume-first",
            Self::Failover => "failover",
        }
    }

    /// Whether the trigger escapes a blocked or unreadable account (`at-limit`, `failover`), which skips the cooldown,
    /// the landing rule, the hysteresis and the no-return bar.
    #[must_use]
    pub fn is_escape(self) -> bool {
        matches!(self, Self::AtLimit | Self::Failover)
    }
}

/// The active account's reading on this tick, as the trigger classification reads it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ActiveReading {
    /// A trusted reading with this much headroom, in percent (100 less the binding window's utilization).
    Headroom(f64),
    /// The slot's poll answered that its token expired and could not be refreshed this pass (cswap's
    /// `USAGE_TOKEN_EXPIRED`): Claude Code is likely idle and refreshes it on next use.
    TokenExpired,
    /// No trusted reading: a failed or stale fetch, a credential that is another account's, or no windows.
    Unreadable,
}

/// What the engine remembers about the active account between ticks: cswap's `_unhealthy_ticks` and
/// `_idle_hold_since`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActiveHealth {
    /// Ticks in a row without a usable reading of the active account.
    pub unhealthy_ticks: u32,
    /// When the current idle hold (an expired active token) began.
    pub idle_hold_since: Option<Timestamp>,
}

/// Why a tick stays on the active account before any candidate is looked at, as cswap's no-switch reasons name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hold {
    /// The active account is below the threshold and the strategy is `best` (`below-threshold`).
    BelowThreshold,
    /// The active token expired while Claude Code is idle (`active-idle`); cswap slows its cadence while it holds.
    ActiveIdle,
    /// The active account's usage is unknown, `ticks` of the `needed` ticks before a failover (`active-usage-unknown`).
    ActiveUsageUnknown {
        /// Unreadable ticks so far, this one included.
        ticks: u32,
        /// The `unhealthy_ticks` setting.
        needed: u32,
    },
}

/// The trigger classification's answer for a tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerDecision {
    /// Look for another account, for this trigger.
    Switch(Trigger),
    /// Stay, for this reason.
    Hold(Hold),
}

/// A tick's trigger decision and the engine's memory to carry into the next tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Classification {
    /// Whether to look for another account, and why.
    pub decision: TriggerDecision,
    /// The active account's health after this tick.
    pub health: ActiveHealth,
}

/// cswap's trigger classification (autoswitch.py:1050-1122): a trusted reading clears the engine's memory and
/// triggers `at-limit` at no headroom, `proactive` at or over the threshold, and below it `consume-first` under that
/// strategy or a `below-threshold` hold under `best`. An expired token holds as idle for up to [`IDLE_HOLD_MAX_S`]
/// from the hold's start, then counts as unhealthy; an unreadable account ends any idle hold and counts as unhealthy.
/// The `unhealthy_ticks`-th unhealthy tick in a row triggers `failover`. cswap logs a warning when an idle hold
/// lapses; the caller sees that as a hold with `idle_hold_since` set that is no longer [`Hold::ActiveIdle`].
#[must_use]
pub fn classify_trigger(
    reading: ActiveReading,
    health: ActiveHealth,
    settings: &AutoSwitchSettings,
    now: Timestamp,
) -> Classification {
    match reading {
        ActiveReading::Headroom(headroom) => Classification {
            decision: measured(headroom, settings),
            health: ActiveHealth::default(),
        },
        ActiveReading::TokenExpired => token_expired(health, settings, now),
        ActiveReading::Unreadable => count_unhealthy(
            ActiveHealth {
                idle_hold_since: None,
                ..health
            },
            settings,
        ),
    }
}

/// The trigger for a trusted reading with `headroom` (autoswitch.py:1050-1075).
fn measured(headroom: f64, settings: &AutoSwitchSettings) -> TriggerDecision {
    if 100.0 - headroom < settings.threshold {
        return if settings.strategy == Strategy::ConsumeFirst {
            TriggerDecision::Switch(Trigger::ConsumeFirst)
        } else {
            TriggerDecision::Hold(Hold::BelowThreshold)
        };
    }
    TriggerDecision::Switch(if headroom <= 0.0 {
        Trigger::AtLimit
    } else {
        Trigger::Proactive
    })
}

/// The idle hold of an expired active token (autoswitch.py:1077-1107): held while the hold is at most
/// [`IDLE_HOLD_MAX_S`] old, counted as unhealthy after.
fn token_expired(
    health: ActiveHealth,
    settings: &AutoSwitchSettings,
    now: Timestamp,
) -> Classification {
    let since = health.idle_hold_since.unwrap_or(now);
    if now.duration_since(since).as_secs_f64() <= IDLE_HOLD_MAX_S {
        return Classification {
            decision: TriggerDecision::Hold(Hold::ActiveIdle),
            health: ActiveHealth {
                unhealthy_ticks: 0,
                idle_hold_since: Some(since),
            },
        };
    }
    count_unhealthy(
        ActiveHealth {
            idle_hold_since: Some(since),
            ..health
        },
        settings,
    )
}

/// One more unhealthy tick (autoswitch.py:1110-1122): a hold until the `unhealthy_ticks`-th, which fails over.
fn count_unhealthy(health: ActiveHealth, settings: &AutoSwitchSettings) -> Classification {
    let ticks = health.unhealthy_ticks.saturating_add(1);
    let decision = if ticks < settings.unhealthy_ticks {
        TriggerDecision::Hold(Hold::ActiveUsageUnknown {
            ticks,
            needed: settings.unhealthy_ticks,
        })
    } else {
        TriggerDecision::Switch(Trigger::Failover)
    };
    Classification {
        decision,
        health: ActiveHealth {
            unhealthy_ticks: ticks,
            ..health
        },
    }
}

/// cswap's `_in_cooldown` (autoswitch.py:2290) as its callers apply it (:1124, :2241): whether `trigger` is
/// `proactive` or `consume-first` and the last switch recorded in `state` is less than `cooldown_seconds` before
/// `now`. The escapes never wait.
#[must_use]
pub fn in_cooldown(
    trigger: Trigger,
    state: &AutoswitchState,
    settings: &AutoSwitchSettings,
    now: Timestamp,
) -> bool {
    !trigger.is_escape()
        && state
            .last_switch_at
            .is_some_and(|last| secs_from_timestamp(now) - last < settings.cooldown_seconds)
}
