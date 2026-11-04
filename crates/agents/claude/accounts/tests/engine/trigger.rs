//! The engine's trigger classification and idle hold.

use claude_accounts::engine::{
    ActiveHealth, ActiveReading, Hold, IDLE_HOLD_MAX_S, Trigger, TriggerDecision, classify_trigger,
};
use claude_accounts::{AutoSwitchSettings, Strategy};

use crate::support::at;

/// A run of ticks on one engine: each tick carries the health the last one left, at the engine's clock.
struct Ticks {
    settings: AutoSwitchSettings,
    health: ActiveHealth,
    clock_s: i64,
}

impl Ticks {
    fn new() -> Self {
        Self {
            settings: AutoSwitchSettings::default(),
            health: ActiveHealth::default(),
            clock_s: 0,
        }
    }

    fn tick(&mut self, reading: ActiveReading) -> TriggerDecision {
        let classification = classify_trigger(
            reading,
            self.health,
            &self.settings,
            at(self.clock_s * 1000),
        );
        self.health = classification.health;
        classification.decision
    }
}

const HELD: ActiveReading = ActiveReading::TokenExpired;
const UNKNOWN: ActiveReading = ActiveReading::Unreadable;
const IDLE: TriggerDecision = TriggerDecision::Hold(Hold::ActiveIdle);
const FAILOVER: TriggerDecision = TriggerDecision::Switch(Trigger::Failover);

fn unknown(ticks: u32) -> TriggerDecision {
    TriggerDecision::Hold(Hold::ActiveUsageUnknown { ticks, needed: 3 })
}

/// The idle-hold cap in whole seconds, as the clock advances in them.
fn idle_cap_s() -> i64 {
    assert!(
        (IDLE_HOLD_MAX_S - 1800.0).abs() < f64::EPSILON,
        "the idle-hold cap is 30 minutes"
    );
    1800
}

#[test]
fn below_threshold_is_no_action() {
    assert_eq!(
        Ticks::new().tick(ActiveReading::Headroom(50.0)),
        TriggerDecision::Hold(Hold::BelowThreshold)
    );
}

#[test]
fn unknown_active_usage_waits_then_fails_over() {
    let mut ticks = Ticks::new();
    assert_eq!(ticks.tick(UNKNOWN), unknown(1));
    assert_eq!(ticks.tick(UNKNOWN), unknown(2));
    assert_eq!(ticks.tick(UNKNOWN), FAILOVER);
}

#[test]
fn known_active_usage_resets_unhealthy_counter() {
    let mut ticks = Ticks::new();
    ticks.tick(UNKNOWN);
    ticks.tick(UNKNOWN);
    assert_eq!(
        ticks.tick(ActiveReading::Headroom(50.0)),
        TriggerDecision::Hold(Hold::BelowThreshold)
    );
    assert_eq!(
        ticks.tick(UNKNOWN),
        unknown(1),
        "a trusted reading resets the counter"
    );
}

#[test]
fn token_expired_holds_instead_of_failover() {
    let mut ticks = Ticks::new();
    for _ in 0..6 {
        assert_eq!(ticks.tick(HELD), IDLE);
        ticks.clock_s += 60;
    }
    assert_eq!(ticks.health.unhealthy_ticks, 0);
}

#[test]
fn idle_hold_cap_escalates_to_failover() {
    let mut ticks = Ticks::new();
    assert_eq!(ticks.tick(HELD), IDLE);
    ticks.clock_s += idle_cap_s() + 1;
    assert_eq!(ticks.tick(HELD), unknown(1));
    assert_eq!(ticks.tick(HELD), unknown(2));
    assert_eq!(ticks.tick(HELD), FAILOVER);
}

#[test]
fn recovery_resets_the_hold_clock() {
    let mut ticks = Ticks::new();
    ticks.tick(HELD);
    ticks.clock_s += idle_cap_s() - 60;
    ticks.tick(ActiveReading::Headroom(50.0));
    ticks.clock_s += 120;
    assert_eq!(
        ticks.tick(HELD),
        IDLE,
        "a new expiry after a trusted reading starts a new hold"
    );
    assert_eq!(ticks.health.unhealthy_ticks, 0);
}

#[test]
fn plain_fetch_failure_still_counts_unhealthy() {
    let mut ticks = Ticks::new();
    ticks.tick(HELD);
    assert_eq!(ticks.tick(UNKNOWN), unknown(1));
    assert_eq!(ticks.health.unhealthy_ticks, 1);
    assert_eq!(ticks.health.idle_hold_since, None);
}

#[test]
fn foreign_credential_sentinel_fails_over_instead_of_holding() {
    // A credential proven to be another account's is no trusted reading: the caller passes it as unreadable.
    let mut ticks = Ticks::new();
    assert_eq!(ticks.tick(UNKNOWN), unknown(1));
    assert_eq!(ticks.tick(UNKNOWN), unknown(2));
    assert_eq!(ticks.tick(UNKNOWN), FAILOVER);
    assert_eq!(ticks.health.idle_hold_since, None);
}

#[test]
fn best_strategy_unaffected_below_threshold() {
    let mut ticks = Ticks::new();
    assert_eq!(
        ticks.tick(ActiveReading::Headroom(80.0)),
        TriggerDecision::Hold(Hold::BelowThreshold)
    );
    ticks.settings.strategy = Strategy::ConsumeFirst;
    assert_eq!(
        ticks.tick(ActiveReading::Headroom(80.0)),
        TriggerDecision::Switch(Trigger::ConsumeFirst),
        "control: consume-first moves below the threshold"
    );
}
