//! The cadence tests that drive a real tick through cswap's engine harness, which the platform-gated engine test
//! support provides. The pure `next_delay` and `sleep_event` tests live in [`crate::cadence`], which is not gated.

use claude_accounts::engine::{
    Engine, EventBody, Lease, LeaseHolder, LeaseState, MAX_SLEEP_S, NO_RESET_FALLBACK_S,
    TickOutcome, next_delay,
};
use claude_accounts::{BalanceReadError, PollResult, UsageEntry};
use proc_table::EmptyTable;
use serde_json::{Value, json};

use crate::cadence::INTERVAL;
use crate::tick::{Bench, reasons};

/// cswap's `_usage`: the 5-hour window at `pct` and an empty 7-day window.
fn reading(pct: f64) -> Value {
    json!({"five_hour": {"pct": pct}, "seven_day": {"pct": 0.0}})
}

// cswap: TestWarmUp.test_warmup_uses_normal_interval
#[test]
fn warmup_uses_normal_interval() {
    let mut bench = Bench::new("cadence_warmup_interval", 3);
    bench.engine = Engine::new("ops-tower", 101, Some(bench.now()));
    bench.set_warm_settled(2);
    bench.set_usage(&[(1, None), (2, Some(reading(10.0)))]);
    bench.set_result(1, PollResult::TokenExpired);
    bench.set_entry(3, UsageEntry::default());
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::NoAction);
    assert_eq!(reasons(&report), ["warming-up"]);
    assert!(
        !report.idle_hold_slow,
        "the warm-up short-circuits before the idle hold"
    );
    let delay = next_delay(&report, INTERVAL, bench.now(), None, 0.5);
    assert!(
        (0.9 * INTERVAL..=1.1 * INTERVAL).contains(&delay),
        "warm-up delay {delay}"
    );
    assert!(
        delay < NO_RESET_FALLBACK_S,
        "a warm-up tick keeps the normal interval, not the idle fallback"
    );
}

// A halted engine stops switching and releases its lease, the state a `RollbackFailed` reaction sets.
#[test]
fn halt_stops_switching_and_releases_the_lease() {
    let mut bench = Bench::new("cadence_halt", 3);
    bench.engine.halt();
    assert!(bench.engine.halted());
    bench.set_usage(&[
        (1, Some(reading(95.0))),
        (2, Some(reading(10.0))),
        (3, Some(reading(50.0))),
    ]);
    let ours = LeaseHolder::new("ops-tower", 101, INTERVAL);
    let claimed = Lease::new(&bench.root())
        .claim_or_heartbeat(&ours, bench.now(), &EmptyTable)
        .expect("the lease can be claimed");
    assert_eq!(claimed, LeaseState::Ours);
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::Halted);
    assert!(
        !bench.root().engine_lease().exists(),
        "the halted tick removed the lease file"
    );
    assert!(bench.engine.halted());
    assert!(
        bench.switch_calls().is_empty(),
        "a halted engine never switches"
    );
    assert_eq!(
        bench.collection_calls(),
        0,
        "a halted engine collects nothing"
    );
    let other = LeaseHolder::new("rustling-tulip", 77, 60.0);
    let seen = Lease::new(&bench.root())
        .observe(&other, bench.now(), &EmptyTable)
        .expect("the lease can be read");
    assert_eq!(seen, LeaseState::Free, "the halted engine holds no lease");
}

// I7: an all-exhausted tick that found a paid target ticks at the interval, even when its balance read failed and the
// entry waits for the next read: no long blocked sleep toward the reset.
#[test]
fn an_all_exhausted_tick_with_a_paid_target_ticks_at_the_interval() {
    let mut bench = Bench::new("cadence_paid_target", 2);
    bench.paid_fleet(2);
    bench.fund(2, 100_000, 1000);
    bench.answer_balance(Err(BalanceReadError::NoRow));
    let report = bench.tick();
    assert_eq!(reasons(&report), ["stale-balance"]);
    assert_eq!(report.sleep_target, None);
    assert!(!report.blocked_wait_long);
    let delay = next_delay(&report, INTERVAL, bench.now(), None, 0.5);
    assert!(
        (0.9 * INTERVAL..=1.1 * INTERVAL).contains(&delay),
        "paid-target delay {delay}"
    );
}

// I7: with no paid target, the all-exhausted tick keeps today's long sleep toward the reset.
#[test]
fn an_all_exhausted_tick_without_one_keeps_the_long_sleep() {
    let mut bench = Bench::new("cadence_no_paid_target", 2);
    bench.paid_fleet(2);
    bench.set_prepaid(2, 100_000, 60);
    let report = bench.tick();
    assert_eq!(report.outcome, TickOutcome::Blocked);
    assert!(report.blocked_wait_long);
    assert!(report.sleep_target.is_some(), "the reset is known");
    let delay = next_delay(&report, INTERVAL, bench.now(), None, 0.5);
    assert!(
        (delay - MAX_SLEEP_S).abs() < f64::EPSILON,
        "all-exhausted delay {delay}"
    );
}

/// Whether `outcome` is the pause of a tick that ran while auto-switch was off.
fn paused_off(outcome: &TickOutcome) -> bool {
    *outcome
        == TickOutcome::Paused {
            reason: "auto-switch is off".to_owned(),
        }
}

// OPS-460 Part 2: while auto-switch is off, an all-exhausted tick keeps the long sleep toward the reset, even with a
// funded paid target an on engine would move to: no paid entry is prepared.
#[test]
fn an_off_all_exhausted_tick_keeps_the_long_sleep() {
    let mut bench = Bench::new("cadence_off_all_exhausted", 2);
    bench.set_auto_switch(false);
    bench.paid_fleet(2);
    bench.fund(2, 100_000, 60);
    let report = bench.tick();
    assert!(paused_off(&report.outcome), "{:?}", report.outcome);
    assert!(report.blocked_wait_long);
    assert!(report.sleep_target.is_some(), "the reset is known");
    assert!(
        bench.switch_calls().is_empty(),
        "no switch: {:?}",
        bench.switch_calls()
    );
    let delay = next_delay(&report, INTERVAL, bench.now(), None, 0.5);
    assert!(
        (delay - MAX_SLEEP_S).abs() < f64::EPSILON,
        "off all-exhausted delay {delay}"
    );
}

// OPS-460 Part 2: while auto-switch is off, an idle hold keeps the slow cadence.
#[test]
fn an_off_idle_hold_keeps_the_slow_cadence() {
    let mut bench = Bench::new("cadence_off_idle_hold", 3);
    bench.set_auto_switch(false);
    bench.set_usage(&[
        (1, None),
        (2, Some(reading(10.0))),
        (3, Some(reading(20.0))),
    ]);
    bench.set_result(1, PollResult::TokenExpired);
    let report = bench.tick();
    assert!(paused_off(&report.outcome), "{:?}", report.outcome);
    assert!(
        report.idle_hold_slow,
        "the idle hold asks for the slow cadence"
    );
    let delay = next_delay(&report, INTERVAL, bench.now(), None, 0.5);
    assert!(
        (delay - NO_RESET_FALLBACK_S).abs() < f64::EPSILON,
        "off idle-hold delay {delay}"
    );
}

// OPS-460 Part 2: while auto-switch is off, paid mode on an account whose live token is expired (Claude Code idle)
// hands on to the idle hold and keeps its slow cadence, as an on engine does.
#[test]
fn an_off_paid_tick_with_an_expired_live_token_keeps_the_idle_hold_cadence() {
    let mut bench = Bench::new("cadence_off_paid_idle", 2);
    bench.paid_fleet(2);
    bench.fund(2, 100_000, 60);
    assert_eq!(bench.tick().outcome, TickOutcome::Switched);

    bench.advance(60);
    bench.set_auto_switch(false);
    bench.paid_fleet(2);
    bench.fund(2, 100_000, 60);
    bench.set_result(2, PollResult::TokenExpired);
    let report = bench.tick();
    assert!(paused_off(&report.outcome), "{:?}", report.outcome);
    assert!(
        report.idle_hold_slow,
        "the idle hold asks for the slow cadence: {:?}",
        report.events
    );
    let delay = next_delay(&report, INTERVAL, bench.now(), None, 0.5);
    assert!(
        (delay - NO_RESET_FALLBACK_S).abs() < f64::EPSILON,
        "off paid idle delay {delay}"
    );
}

// I7: paid mode ticks at the interval, whether it stays on credits or holds, with a warning, on an account that may no
// longer spend them while there is nowhere to go.
#[test]
fn paid_mode_ticks_at_the_interval() {
    let mut bench = Bench::new("cadence_paid_mode", 2);
    bench.paid_fleet(2);
    bench.fund(2, 100_000, 60);
    assert_eq!(bench.tick().outcome, TickOutcome::Switched);

    bench.advance(60);
    bench.paid_fleet(2);
    bench.fund(2, 100_000, 60);
    let report = bench.tick();
    assert_eq!(reasons(&report), ["on-credits"]);
    assert_eq!(
        (report.sleep_target, report.blocked_wait_long),
        (None, false)
    );
    let delay = next_delay(&report, INTERVAL, bench.now(), None, 0.5);
    assert!(
        (0.9 * INTERVAL..=1.1 * INTERVAL).contains(&delay),
        "on-credits delay {delay}"
    );

    bench.advance(60);
    bench.paid_fleet(2);
    bench.fund(2, 100_000, 60);
    bench.set_may_spend(2, false);
    let report = bench.tick();
    assert!(
        report
            .events
            .iter()
            .any(|event| matches!(event.body, EventBody::Warning { .. })),
        "the hold warns: {:?}",
        report.events
    );
    assert_eq!(report.outcome, TickOutcome::Blocked);
    assert_eq!(
        (report.sleep_target, report.blocked_wait_long),
        (None, false)
    );
    let delay = next_delay(&report, INTERVAL, bench.now(), None, 0.5);
    assert!(
        (0.9 * INTERVAL..=1.1 * INTERVAL).contains(&delay),
        "paid hold delay {delay}"
    );
}
