//! The engine's cadence between ticks, and the `sleep` event a long
//! wait reports, in the tests that need neither the run loop nor the
//! engine harness. Nothing here touches the filesystem, so the module is not platform-gated and runs on every runner;
//! the tests that drive a real tick live in `cadence_tick`, which needs the platform-gated harness.

#![expect(
    clippy::expect_used,
    reason = "a test that cannot build its fixed clock, raise its sleep event or read its event's JSON must fail loudly"
)]

use claude_accounts::engine::{
    Event, EventBody, MAX_SLEEP_S, NO_RESET_FALLBACK_S, TickOutcome, TickReport, URGENT_INTERVAL_S,
    next_delay, sleep_event,
};
use jiff::{SignedDuration, Timestamp};

/// The default `interval_seconds`, which the harness's engine also uses.
pub(crate) const INTERVAL: f64 = 60.0;

/// The tests' fixed clock: 2026-09-21 14:13:20 UTC, `offset_ms` from it.
fn at(offset_ms: i64) -> Timestamp {
    Timestamp::from_millisecond(1_790_000_000_000 + offset_ms).expect("the test clock is in range")
}

/// `now` plus `seconds`.
fn in_secs(now: Timestamp, seconds: i64) -> Timestamp {
    now + SignedDuration::from_secs(seconds)
}

/// A report with `outcome` and the cadence inputs, and no events.
fn report(
    outcome: TickOutcome,
    sleep_target: Option<Timestamp>,
    blocked_wait_long: bool,
    idle_hold_slow: bool,
) -> TickReport {
    let mut report = TickReport::new(outcome);
    report.sleep_target = sleep_target;
    report.blocked_wait_long = blocked_wait_long;
    report.idle_hold_slow = idle_hold_slow;
    report
}

/// The two delays are the same float to within rounding (the Python tests assert equality of exactly computed values).
#[track_caller]
fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "expected {expected}, got {actual}"
    );
}

#[test]
fn blocked_with_reset_rechecks_at_exhausted_cadence() {
    let now = at(0);
    let blocked = report(TickOutcome::Blocked, Some(in_secs(now, 1800)), false, false);
    assert_close(next_delay(&blocked, INTERVAL, now, None, 0.0), MAX_SLEEP_S);
}

#[test]
fn blocked_exhausted_without_reset_uses_fallback() {
    let now = at(0);
    let blocked = report(TickOutcome::Blocked, None, true, false);
    assert_close(
        next_delay(&blocked, INTERVAL, now, None, 0.0),
        NO_RESET_FALLBACK_S,
    );
}

#[test]
fn blocked_on_resolvable_condition_keeps_normal_cadence() {
    let now = at(0);
    let blocked = report(TickOutcome::Blocked, None, false, false);
    assert_close(
        next_delay(&blocked, INTERVAL, now, None, 0.0),
        0.9 * INTERVAL,
    );
    let high = next_delay(&blocked, INTERVAL, now, None, 0.999_999);
    assert!(
        high < 1.1 * INTERVAL && high > 1.09 * INTERVAL,
        "jitter 0.999… gives just under 1.1x, got {high}"
    );
}

#[test]
fn normal_delay_is_jittered_interval() {
    let now = at(0);
    let outcomes = [
        TickOutcome::Error,
        TickOutcome::Switched,
        TickOutcome::NoAction,
        TickOutcome::Halted,
        TickOutcome::Paused {
            reason: String::new(),
        },
    ];
    for outcome in outcomes {
        let case = report(outcome.clone(), None, false, false);
        for jitter in [0.0, 0.25, 0.5, 0.999_999] {
            let delay = next_delay(&case, INTERVAL, now, None, jitter);
            assert!(
                (0.9 * INTERVAL..=1.1 * INTERVAL).contains(&delay),
                "{outcome:?} at jitter {jitter} gave {delay}"
            );
        }
    }
}

#[test]
fn sleep_cap() {
    let now = at(0);
    let blocked = report(
        TickOutcome::Blocked,
        Some(in_secs(now, 50 * 3600)),
        false,
        false,
    );
    assert_close(next_delay(&blocked, INTERVAL, now, None, 0.0), MAX_SLEEP_S);
}

// The delay half of the idle-hold cadence.
#[test]
fn idle_hold_slows_cadence() {
    let now = at(0);
    let holding = report(TickOutcome::NoAction, None, false, true);
    assert!(next_delay(&holding, INTERVAL, now, None, 0.0) >= NO_RESET_FALLBACK_S);
    let released = report(TickOutcome::NoAction, None, false, false);
    assert!(
        next_delay(&released, INTERVAL, now, None, 0.5) <= 1.1 * INTERVAL,
        "a released hold keeps the normal cadence"
    );
}

/// The pause of a tick that ran while auto-switch was off.
fn paused_off() -> TickOutcome {
    TickOutcome::Paused {
        reason: "auto-switch is off".to_owned(),
    }
}

// OPS-460: a tick paused because auto-switch is off, with every account exhausted and a known reset, waits toward it.
#[test]
fn paused_with_a_sleep_target_waits_toward_it() {
    let now = at(0);
    let far = report(paused_off(), Some(in_secs(now, 1800)), true, false);
    assert_close(next_delay(&far, INTERVAL, now, None, 0.0), MAX_SLEEP_S);
    let near = report(paused_off(), Some(in_secs(now, 200)), true, false);
    assert_close(next_delay(&near, INTERVAL, now, None, 0.0), 200.0);
}

// OPS-460: a paused tick blocked on something that will not change soon waits the fallback.
#[test]
fn paused_with_blocked_wait_long_waits_the_fallback() {
    let now = at(0);
    let paused = report(paused_off(), None, true, false);
    assert_close(
        next_delay(&paused, INTERVAL, now, None, 0.0),
        NO_RESET_FALLBACK_S,
    );
}

// OPS-460: a paused tick whose active account idle-holds waits the fallback.
#[test]
fn paused_with_idle_hold_slow_waits_the_fallback() {
    let now = at(0);
    let paused = report(paused_off(), None, false, true);
    assert_close(
        next_delay(&paused, INTERVAL, now, None, 0.0),
        NO_RESET_FALLBACK_S,
    );
}

// OPS-460: the cadence flags decide the wait whatever the outcome, in order: the sleep target, then the long block,
// then the idle hold; none of them ever cut to the poll plan.
#[test]
fn a_flag_decides_the_wait_whatever_the_outcome() {
    let now = at(0);
    let outcomes = [
        TickOutcome::Switched,
        TickOutcome::Error,
        TickOutcome::NoAction,
        TickOutcome::Blocked,
        paused_off(),
        TickOutcome::Halted,
    ];
    let plan = Some(in_secs(now, 60));
    for outcome in outcomes {
        let target = report(outcome.clone(), Some(in_secs(now, 400)), true, true);
        assert_close(next_delay(&target, INTERVAL, now, plan, 0.5), 400.0);
        let long = report(outcome.clone(), None, true, false);
        assert_close(
            next_delay(&long, INTERVAL, now, plan, 0.5),
            NO_RESET_FALLBACK_S,
        );
        let idle = report(outcome.clone(), None, false, true);
        assert_close(
            next_delay(&idle, INTERVAL, now, plan, 0.5),
            NO_RESET_FALLBACK_S,
        );
    }
}

#[test]
fn poll_plan_sleep_is_cut_to_the_rows_next_poll() {
    let now = at(0);
    let normal = report(TickOutcome::NoAction, None, false, false);
    assert_close(
        next_delay(&normal, 360.0, now, Some(in_secs(now, 60)), 0.5),
        60.0,
    );
}

#[test]
fn poll_plan_never_sleeps_below_the_planners_own_floor() {
    let now = at(0);
    let normal = report(TickOutcome::NoAction, None, false, false);
    let overdue = now - SignedDuration::from_secs(500);
    assert_close(
        next_delay(&normal, 360.0, now, Some(overdue), 0.5),
        URGENT_INTERVAL_S,
    );
}

#[test]
fn poll_plan_a_relaxed_plan_never_lengthens_the_sleep() {
    let now = at(0);
    let normal = report(TickOutcome::NoAction, None, false, false);
    let delay = next_delay(&normal, INTERVAL, now, Some(in_secs(now, 3600)), 0.999_999);
    assert!(
        delay <= 1.1 * INTERVAL,
        "a distant plan must not stretch the cadence, got {delay}"
    );
}

#[test]
fn poll_plan_a_short_interval_is_never_lengthened() {
    let now = at(0);
    let normal = report(TickOutcome::NoAction, None, false, false);
    let delay = next_delay(&normal, 15.0, now, Some(in_secs(now, 5)), 0.999_999);
    assert!(delay <= 15.0 * 1.1, "a 15s interval slept {delay}s");
}

// The loop reports a `sleep` event only when it waits past 1.5 intervals (autoswitch.py:2468).
#[test]
fn sleep_event_only_past_one_and_a_half_intervals() {
    let now = at(0);
    assert!(
        sleep_event(90.0, INTERVAL, now).is_none(),
        "exactly 1.5 intervals stays quiet"
    );
    let event =
        sleep_event(90.001, INTERVAL, now).expect("past 1.5 intervals the loop reports a sleep");
    assert_eq!(event.kind(), "sleep");
    assert_eq!(event.at, now);
    assert!(
        sleep_event(f64::INFINITY, INTERVAL, now).is_none(),
        "a non-finite delay reports no sleep"
    );
}

// The `sleep` event's JSON shape.
#[test]
fn sleep_event_json_has_its_shape() {
    let now = at(0);
    let event = sleep_event(3600.06, INTERVAL, now).expect("a long wait reports a sleep");
    assert_eq!(event.kind(), "sleep");
    let EventBody::Sleep { seconds, .. } = &event.body else {
        unreachable!("the sleep event carries the sleep body");
    };
    assert_close(*seconds, 3600.06);
    let until_text = in_secs(now, 3600)
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let event_json = event.to_json();
    assert_eq!(event_json["event"].as_str(), Some("sleep"));
    let rounded = event_json["seconds"].as_f64().expect("seconds is a number");
    assert!(
        (rounded - 3600.1).abs() < 1e-9,
        "seconds is rounded to one decimal, got {rounded}"
    );
    assert_eq!(event_json["until"].as_str(), Some(until_text.as_str()));
}

// Rounding seconds to one decimal is correctly rounded on the exact binary double, ties to even — not half away from zero.
#[test]
fn sleep_seconds_round_like_python() {
    let now = at(0);
    for (seconds, expected) in [
        (3600.25, 3600.2),
        (5400.45, 5400.4),
        (90.25, 90.2),
        (0.35, 0.3),
    ] {
        let event = Event {
            at: now,
            body: EventBody::Sleep {
                seconds,
                until: now,
            },
        };
        let rounded = event.to_json()["seconds"]
            .as_f64()
            .expect("seconds is a number");
        assert_close(rounded, expected);
    }
}
