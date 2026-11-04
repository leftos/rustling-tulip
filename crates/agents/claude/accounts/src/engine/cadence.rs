//! The engine's cadence between ticks: how long a tick's outcome earns
//! before the next one, obeying the usage store's own poll plan, and the `sleep` event a long wait reports.

use jiff::{SignedDuration, Timestamp};

use crate::engine::{Event, EventBody, MAX_SLEEP_S, NO_RESET_FALLBACK_S, TickReport};
use crate::poll::URGENT_INTERVAL_S;
use crate::usage::secs_from_timestamp;

/// The seconds to wait before the next tick.
///
/// `interval_s` is the configured interval, `now` the time the tick ran at, `active_next_poll_at` the usage store's
/// planned next poll for the active account (`None` when there is no active account, no row, no plan, or the usage
/// store could not be read — an unreadable store is no error: the unshortened delay is always safe), and `jitter_unit` the caller's
/// draw in `[0, 1)`, so the function is pure.
///
/// Whatever the outcome (a tick paused because auto-switch is off included): a known reset the tick sleeps toward is
/// waited for, capped at [`MAX_SLEEP_S`]; a block that will not resolve soon, and an idle hold, wait at least
/// [`NO_RESET_FALLBACK_S`]; else the interval with ±10 % jitter, shortened to the store's plan — never lengthened — when
/// the plan is due sooner.
#[must_use]
pub fn next_delay(
    report: &TickReport,
    interval_s: f64,
    now: Timestamp,
    active_next_poll_at: Option<Timestamp>,
    jitter_unit: f64,
) -> f64 {
    if let Some(target) = report.sleep_target {
        let until = secs_from_timestamp(target) - secs_from_timestamp(now);
        return until.max(interval_s).min(MAX_SLEEP_S);
    }
    if report.blocked_wait_long || report.idle_hold_slow {
        return interval_s.max(NO_RESET_FALLBACK_S);
    }
    respect_poll_plan(
        interval_s * (0.9 + 0.2 * jitter_unit),
        now,
        active_next_poll_at,
    )
}

/// A normal-cadence sleep shortened to the store's next poll for the active account,
/// never below [`URGENT_INTERVAL_S`] (the planner's floor, and the rate budget's) and never lengthened.
fn respect_poll_plan(delay: f64, now: Timestamp, active_next_poll_at: Option<Timestamp>) -> f64 {
    let Some(next) = active_next_poll_at else {
        return delay;
    };
    let due_in = secs_from_timestamp(next) - secs_from_timestamp(now);
    delay.min(due_in.max(URGENT_INTERVAL_S))
}

/// The `sleep` event of a wait longer than 1.5 intervals: `None` when the wait is short
/// enough that the loop stays quiet, and `None` when the delay is not finite or lies outside the timestamp range, so a
/// nonsensical delay reports no sleep rather than a bogus wake time.
#[must_use]
pub fn sleep_event(delay_s: f64, interval_s: f64, now: Timestamp) -> Option<Event> {
    if delay_s <= interval_s * 1.5 {
        return None;
    }
    let until = now
        .checked_add(SignedDuration::try_from_secs_f64(delay_s).ok()?)
        .ok()?;
    Some(Event {
        at: now,
        body: EventBody::Sleep {
            seconds: delay_s,
            until,
        },
    })
}
