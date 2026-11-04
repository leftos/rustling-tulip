//! cswap's candidate census, ranking, cooldown and no-return bar, ported from its `test_autoswitch.py` one test per
//! cswap test. A cswap test that drives whole ticks runs through [`Fleet::tick`], a tick built from the pure functions:
//! classification, cooldown, census, ranking, and the departure snapshot cswap's `_perform` records on a switch.

use std::collections::BTreeMap;

use claude_accounts::engine::{
    AccountUsage, ActiveHealth, ActiveReading, CandidateAccount, Candidates, RankInputs, Ranking,
    Trigger, TriggerDecision, TrustedReadings, WindowSource, binding_recovery, candidates,
    classify_trigger, in_cooldown, left_account_recovered, no_return_account, rank,
    rank_candidates,
};
use claude_accounts::{
    AccountKind, AutoSwitchSettings, AutoswitchState, QuarantineEntry, QuarantineRecord, Strategy,
};
use jiff::Timestamp;
use serde_json::{Value, json};

use crate::support::at;

const H: i64 = 3600;

type Readings = BTreeMap<u32, Value>;

fn rows<const N: usize>(rows: [(u32, Value); N]) -> Readings {
    BTreeMap::from(rows)
}

/// `readings` as the ranking reads them, with no `models` in the settings.
fn source(readings: &Readings) -> TrustedReadings<'_, Value> {
    TrustedReadings::new(readings, &[])
}

/// cswap's `_usage`: the 5-hour window at `pct` and an empty 7-day window.
fn five(pct: f64) -> Value {
    json!({"five_hour": {"pct": pct}, "seven_day": {"pct": 0.0}})
}

/// cswap's `_usage` with the 5-hour window resetting at `reset`.
fn five_at(pct: f64, reset: &str) -> Value {
    json!({"five_hour": {"pct": pct, "resets_at": reset}, "seven_day": {"pct": 0.0}})
}

/// cswap's `_usage7`: both windows, the 7-day one resetting at `reset` when given.
fn weekly(pct5: f64, pct7: f64, reset: Option<&str>) -> Value {
    let mut seven_day = json!({"pct": pct7});
    if let Some(reset) = reset {
        seven_day["resets_at"] = json!(reset);
    }
    json!({"five_hour": {"pct": pct5}, "seven_day": seven_day})
}

fn seconds(timestamp: Timestamp) -> f64 {
    timestamp.as_duration().as_secs_f64()
}

fn switched(outcomes: &[Option<u32>]) -> bool {
    outcomes.iter().any(Option::is_some)
}

/// A departure snapshot value as the state file holds it.
#[derive(Clone, Copy)]
enum Snap {
    Null,
    At(f64),
}

/// A state whose last switch left account `from`, with its departure snapshot.
fn left(from: u32, headroom: Snap, recovery: Snap, trigger: Option<&str>) -> AutoswitchState {
    let field = |snap: Snap| match snap {
        Snap::Null => Some(None),
        Snap::At(value) => Some(Some(value)),
    };
    let mut state = AutoswitchState::default();
    state.last_switch_from = Some(from);
    state.left_headroom = field(headroom);
    state.left_recovery_at = field(recovery);
    state.left_trigger = trigger.map(str::to_owned);
    state
}

/// cswap's engine harness, reduced to what the decision reads: the accounts, the settings, the state file, the active
/// account, the engine's memory of it and a clock in seconds from the tests' fixed time.
struct Fleet {
    accounts: Vec<CandidateAccount>,
    settings: AutoSwitchSettings,
    state: AutoswitchState,
    active: u32,
    health: ActiveHealth,
    clock_s: i64,
}

impl Fleet {
    /// OAuth accounts 1 to `count`, account 1 active, cswap's default settings.
    fn new(count: u32) -> Self {
        Self {
            accounts: (1..=count)
                .map(|number| CandidateAccount::new(number, AccountKind::Oauth, false))
                .collect(),
            settings: AutoSwitchSettings::default(),
            state: AutoswitchState::default(),
            active: 1,
            health: ActiveHealth::default(),
            clock_s: 0,
        }
    }

    fn consume_first(count: u32) -> Self {
        let mut fleet = Self::new(count);
        fleet.settings.strategy = Strategy::ConsumeFirst;
        fleet
    }

    fn now(&self) -> Timestamp {
        at(self.clock_s * 1000)
    }

    fn advance(&mut self, seconds: i64) {
        self.clock_s += seconds;
    }

    /// A `resets_at` `offset` seconds from the clock now: cswap's `_at`, or its `_days_out` when kept across ticks.
    fn reset_in(&self, offset: i64) -> String {
        at((self.clock_s + offset) * 1000).to_string()
    }

    /// The epoch seconds `offset` seconds from the clock now.
    fn secs_in(&self, offset: i64) -> f64 {
        seconds(at((self.clock_s + offset) * 1000))
    }

    fn census(&self) -> Candidates {
        candidates(&self.accounts, self.active, &self.state)
    }

    fn inputs<'a>(
        &'a self,
        trigger: Trigger,
        census: &'a Candidates,
        usage: &'a dyn WindowSource,
    ) -> RankInputs<'a> {
        RankInputs {
            trigger,
            active: self.active,
            candidates: census,
            usage,
            state: &self.state,
            settings: &self.settings,
            now: self.now(),
        }
    }

    /// The ranking with the no-return bar and its release, as a tick for `trigger` asks for it.
    fn ranking(&self, trigger: Trigger, readings: &Readings) -> Ranking {
        let census = self.census();
        rank(&self.inputs(trigger, &census, &source(readings)))
    }

    fn recovered(&self, readings: &Readings) -> bool {
        let census = self.census();
        left_account_recovered(&self.inputs(Trigger::Proactive, &census, &source(readings)))
    }

    fn barred(&self, trigger: Trigger, readings: &Readings, recovered: bool) -> Option<u32> {
        let census = self.census();
        no_return_account(&self.inputs(trigger, &census, &source(readings)), recovered)
    }

    fn decision(&self, readings: &Readings) -> TriggerDecision {
        classify_trigger(
            active_reading(self.active, readings),
            self.health,
            &self.settings,
            self.now(),
        )
        .decision
    }

    /// One tick on `readings` (an account missing from them is unreadable): the account it switched to, if any.
    fn tick(&mut self, readings: &Readings) -> Option<u32> {
        let now = self.now();
        let classification = classify_trigger(
            active_reading(self.active, readings),
            self.health,
            &self.settings,
            now,
        );
        self.health = classification.health;
        let TriggerDecision::Switch(trigger) = classification.decision else {
            return None;
        };
        if in_cooldown(trigger, &self.state, &self.settings, now) {
            return None;
        }
        let target = *self.ranking(trigger, readings).ordered.first()?;
        let departed = source(readings).usage(self.active);
        self.state.last_switch_at = Some(seconds(now));
        self.state.last_switch_to = Some(target.to_string());
        self.state.last_switch_from = Some(self.active);
        self.state.left_headroom = Some(departed.as_ref().and_then(AccountUsage::headroom));
        self.state.left_recovery_at = Some(binding_recovery(departed.as_ref(), now).map(seconds));
        self.state.left_trigger = Some(trigger.as_str().to_owned());
        self.active = target;
        Some(target)
    }

    /// `ticks` ticks, the clock advancing `step` seconds after each, on the readings `readings` gives at each tick.
    fn walk(
        &mut self,
        ticks: usize,
        step: i64,
        readings: impl Fn(&Self) -> Readings,
    ) -> Vec<Option<u32>> {
        let mut outcomes = Vec::new();
        for _ in 0..ticks {
            let current = readings(self);
            outcomes.push(self.tick(&current));
            self.advance(step);
        }
        outcomes
    }

    /// cswap's failover setup: account 1 unreadable for `unhealthy_ticks` ticks a minute apart, account 2 at 4 % with
    /// its 5-hour window resetting at `reset` (or 400 h from each tick).
    fn fail_over_to_2(&mut self, reset: Option<&str>) {
        let outcomes = self.walk(3, 60, |fleet| {
            rows([(
                2,
                five_at(
                    4.0,
                    &reset.map_or_else(|| fleet.reset_in(400 * H), str::to_owned),
                ),
            )])
        });
        assert_eq!(
            outcomes,
            [None, None, Some(2)],
            "premise: the third unreadable tick fails over to account 2"
        );
        assert_eq!(
            self.state.left_headroom,
            Some(None),
            "premise: a failover records a null departure headroom"
        );
        assert_eq!(self.state.left_recovery_at, Some(None));
    }
}

fn active_reading(active: u32, readings: &Readings) -> ActiveReading {
    source(readings)
        .usage(active)
        .and_then(|usage| usage.headroom())
        .map_or(ActiveReading::Unreadable, ActiveReading::Headroom)
}

// cswap: TestDecisionTable.test_over_threshold_switches_to_max_headroom
#[test]
fn over_threshold_switches_to_max_headroom() {
    let mut fleet = Fleet::new(3);
    assert_eq!(
        fleet.tick(&rows([(1, five(95.0)), (2, five(40.0)), (3, five(20.0))])),
        Some(3)
    );
    assert_eq!(fleet.state.left_trigger.as_deref(), Some("proactive"));
    assert_eq!(fleet.state.last_switch_to.as_deref(), Some("3"));
}

// cswap: TestDecisionTable.test_hysteresis_margin_blocks_marginal_candidates (the ranking; the all-exhausted report and
// the cadence are the tick's)
#[test]
fn hysteresis_margin_blocks_marginal_candidates() {
    let mut fleet = Fleet::new(3);
    let readings = rows([(1, five(95.0)), (2, five(86.0)), (3, five(88.0))]);
    let ranking = fleet.ranking(Trigger::Proactive, &readings);
    assert!(
        ranking.ordered.is_empty(),
        "9 points better is under the 10-point margin: {ranking:?}"
    );
    assert!(ranking.any_known);
    assert_eq!(fleet.tick(&readings), None);
}

// cswap: TestDecisionTable.test_issue_115_strictly_better_candidate_switches
#[test]
fn issue_115_strictly_better_candidate_switches() {
    let mut fleet = Fleet::new(3);
    let readings = rows([
        (
            1,
            json!({"five_hour": {"pct": 99.0}, "seven_day": {"pct": 24.0}}),
        ),
        (
            2,
            json!({"five_hour": {"pct": 3.0}, "seven_day": {"pct": 89.0}}),
        ),
        (
            3,
            json!({"five_hour": {"pct": 95.0}, "seven_day": {"pct": 10.0}}),
        ),
    ]);
    assert_eq!(fleet.tick(&readings), Some(2));
    assert_eq!(fleet.state.left_trigger.as_deref(), Some("proactive"));
}

// cswap: TestDecisionTable.test_proactive_never_lands_at_or_over_threshold
#[test]
fn proactive_never_lands_at_or_over_threshold() {
    let mut fleet = Fleet::new(2);
    fleet.settings.threshold = 80.0;
    fleet.settings.hysteresis_pct = 5.0;
    assert_eq!(fleet.tick(&rows([(1, five(90.0)), (2, five(85.0))])), None);
}

// cswap: TestDecisionTable.test_stable_landing_does_not_switch_back
#[test]
fn stable_landing_does_not_switch_back() {
    let mut fleet = Fleet::new(2);
    fleet.settings.cooldown_seconds = 0.0;
    let readings = rows([
        (
            1,
            json!({"five_hour": {"pct": 99.0}, "seven_day": {"pct": 24.0}}),
        ),
        (
            2,
            json!({"five_hour": {"pct": 3.0}, "seven_day": {"pct": 89.0}}),
        ),
    ]);
    assert_eq!(fleet.tick(&readings), Some(2));
    fleet.advance(60);
    assert_eq!(
        fleet.decision(&readings),
        TriggerDecision::Hold(claude_accounts::engine::Hold::BelowThreshold)
    );
    assert_eq!(fleet.tick(&readings), None);
}

// cswap: TestDecisionTable.test_mixed_unknown_and_exhausted_is_not_all_exhausted (the ranking; the report is the tick's)
#[test]
fn mixed_unknown_and_exhausted_is_not_all_exhausted() {
    let fleet = Fleet::new(3);
    let reset = fleet.reset_in(2 * H);
    let ranking = fleet.ranking(
        Trigger::Proactive,
        &rows([(1, five(95.0)), (2, five_at(100.0, &reset))]),
    );
    assert_eq!(ranking.ordered, Vec::<u32>::new());
    assert!(ranking.any_known, "the account at its limit is readable");
}

// cswap: TestDecisionTable.test_cooldown_suppresses_proactive
#[test]
fn cooldown_suppresses_proactive() {
    let mut fleet = Fleet::new(3);
    fleet.state.last_switch_at = Some(fleet.secs_in(-10));
    let readings = rows([(1, five(95.0)), (2, five(10.0)), (3, five(10.0))]);
    assert_eq!(
        fleet.decision(&readings),
        TriggerDecision::Switch(Trigger::Proactive)
    );
    assert!(in_cooldown(
        Trigger::Proactive,
        &fleet.state,
        &fleet.settings,
        fleet.now()
    ));
    assert_eq!(fleet.tick(&readings), None);
}

// cswap: TestDecisionTable.test_at_limit_bypasses_cooldown
#[test]
fn at_limit_bypasses_cooldown() {
    let mut fleet = Fleet::new(3);
    fleet.state.last_switch_at = Some(fleet.secs_in(-10));
    assert_eq!(
        fleet.tick(&rows([(1, five(100.0)), (2, five(10.0)), (3, five(50.0))])),
        Some(2)
    );
    assert_eq!(fleet.state.left_trigger.as_deref(), Some("at-limit"));
}

// cswap: TestDecisionTable.test_cooldown_expires
#[test]
fn cooldown_expires() {
    let mut fleet = Fleet::new(3);
    fleet.state.last_switch_at = Some(fleet.secs_in(0));
    fleet.advance(400);
    assert!(
        fleet
            .tick(&rows([(1, five(95.0)), (2, five(10.0)), (3, five(50.0))]))
            .is_some()
    );
}

// cswap: TestDecisionTable.test_all_candidates_unknown_is_no_comparison
#[test]
fn all_candidates_unknown_is_no_comparison() {
    let fleet = Fleet::new(3);
    let ranking = fleet.ranking(Trigger::Proactive, &rows([(1, five(95.0))]));
    assert_eq!(ranking.ordered, Vec::<u32>::new());
    assert!(!ranking.any_known);
}

// cswap: TestDecisionTable.test_tie_resolves_to_earliest_slot
#[test]
fn tie_resolves_to_earliest_slot() {
    let mut fleet = Fleet::new(3);
    assert_eq!(
        fleet.tick(&rows([(1, five(95.0)), (2, five(30.0)), (3, five(30.0))])),
        Some(2)
    );
}

// cswap: TestDecisionTable.test_candidate_not_better_than_active_is_skipped
#[test]
fn candidate_not_better_than_active_is_skipped() {
    let mut fleet = Fleet::new(3);
    assert_eq!(
        fleet.tick(&rows([(1, five(91.0)), (2, five(89.0)), (3, five(88.0))])),
        None
    );
}

// cswap: TestDecisionTable.test_at_limit_escapes_hysteresis_bar
#[test]
fn at_limit_escapes_hysteresis_bar() {
    let mut fleet = Fleet::new(3);
    assert_eq!(
        fleet.tick(&rows([(1, five(100.0)), (2, five(85.0)), (3, five(97.0))])),
        Some(2)
    );
    assert_eq!(fleet.state.left_trigger.as_deref(), Some("at-limit"));
}

// cswap: TestDecisionTable.test_at_limit_never_targets_another_at_limit_account
#[test]
fn at_limit_never_targets_another_at_limit_account() {
    let mut fleet = Fleet::new(3);
    assert_eq!(
        fleet.tick(&rows([
            (1, five(100.0)),
            (2, five(100.0)),
            (3, five(100.0))
        ])),
        None
    );
}

// cswap: TestDecisionTable.test_failover_ignores_hysteresis_bar
#[test]
fn failover_ignores_hysteresis_bar() {
    let mut fleet = Fleet::new(3);
    let outcomes = fleet.walk(3, 0, |_| rows([(2, five(85.0)), (3, five(100.0))]));
    assert_eq!(outcomes, [None, None, Some(2)]);
    assert_eq!(fleet.state.left_trigger.as_deref(), Some("failover"));
}

#[test]
fn the_census_excludes_disabled_accounts() {
    let mut fleet = Fleet::new(3);
    fleet.accounts[1].disabled = true;
    assert_eq!(fleet.census().oauth, [3]);
}

#[test]
fn the_census_excludes_quarantined_accounts() {
    let mut fleet = Fleet::new(4);
    let entry = QuarantineEntry::new(
        "b@example.test",
        "invalid_grant",
        "2026-09-21T14:13:20Z",
        None,
    );
    fleet
        .state
        .quarantine
        .insert("2".to_owned(), QuarantineRecord::Entry(entry));
    fleet.state.quarantine.insert(
        "3".to_owned(),
        QuarantineRecord::Malformed(json!("unreadable")),
    );
    assert_eq!(
        fleet.census().oauth,
        [4],
        "every key of `quarantine` counts, as cswap counts it"
    );
}

#[test]
fn the_census_excludes_the_active_account() {
    let mut fleet = Fleet::new(3);
    fleet.active = 2;
    assert_eq!(fleet.census().oauth, [1, 3]);
}

/// Consume-first's weekly resets: soonest, later, latest, and one already past.
fn resets(fleet: &Fleet) -> [String; 4] {
    [
        fleet.reset_in(48 * H),
        fleet.reset_in(120 * H),
        fleet.reset_in(168 * H),
        fleet.reset_in(-720 * H),
    ]
}

// cswap: TestConsumeFirstStrategy.test_below_threshold_switches_to_soonest_weekly_reset
#[test]
fn below_threshold_switches_to_soonest_weekly_reset() {
    let mut fleet = Fleet::consume_first(3);
    let [soon, later, latest, _] = resets(&fleet);
    let readings = rows([
        (1, weekly(20.0, 20.0, Some(&later))),
        (2, weekly(10.0, 10.0, Some(&soon))),
        (3, weekly(10.0, 10.0, Some(&latest))),
    ]);
    assert_eq!(fleet.tick(&readings), Some(2));
    assert_eq!(fleet.state.left_trigger.as_deref(), Some("consume-first"));
}

// cswap: TestConsumeFirstStrategy.test_stays_when_active_already_resets_soonest
#[test]
fn stays_when_active_already_resets_soonest() {
    let mut fleet = Fleet::consume_first(3);
    let [soon, later, latest, _] = resets(&fleet);
    let readings = rows([
        (1, weekly(20.0, 20.0, Some(&soon))),
        (2, weekly(10.0, 10.0, Some(&later))),
        (3, weekly(10.0, 10.0, Some(&latest))),
    ]);
    let ranking = fleet.ranking(Trigger::ConsumeFirst, &readings);
    assert_eq!(ranking.ordered, Vec::<u32>::new());
    assert!(ranking.any_known);
    assert!(
        ranking.active_weekly_reset.is_some(),
        "the hold is already-consuming-soonest, not reset-unknown"
    );
    assert_eq!(fleet.tick(&readings), None);
}

// cswap: TestConsumeFirstStrategy.test_over_threshold_prefers_soonest_reset_over_max_headroom
#[test]
fn over_threshold_prefers_soonest_reset_over_max_headroom() {
    let mut fleet = Fleet::consume_first(3);
    let [soon, later, latest, _] = resets(&fleet);
    let readings = rows([
        (1, weekly(95.0, 20.0, Some(&later))),
        (2, weekly(50.0, 40.0, Some(&soon))),
        (3, weekly(10.0, 10.0, Some(&latest))),
    ]);
    assert_eq!(fleet.tick(&readings), Some(2));
}

// cswap: TestConsumeFirstStrategy.test_a_consume_first_target_must_still_be_healthy
#[test]
fn a_consume_first_target_must_still_be_healthy() {
    let mut fleet = Fleet::consume_first(2);
    let [soon, _, latest, _] = resets(&fleet);
    let readings = rows([
        (1, weekly(40.0, 40.0, Some(&latest))),
        (2, weekly(96.0, 96.0, Some(&soon))),
    ]);
    assert_eq!(
        fleet.tick(&readings),
        None,
        "a 96 % account re-triggers on the next tick"
    );
}

// cswap: TestConsumeFirstStrategy.test_respects_cooldown
#[test]
fn respects_cooldown() {
    let mut fleet = Fleet::consume_first(3);
    let [soon, later, latest, _] = resets(&fleet);
    let first = rows([
        (1, weekly(20.0, 20.0, Some(&later))),
        (2, weekly(10.0, 10.0, Some(&soon))),
        (3, weekly(10.0, 10.0, Some(&latest))),
    ]);
    assert_eq!(fleet.tick(&first), Some(2));
    let second = rows([
        (2, weekly(20.0, 20.0, Some(&later))),
        (1, weekly(10.0, 10.0, Some(&latest))),
        (3, weekly(10.0, 10.0, Some(&soon))),
    ]);
    assert_eq!(
        fleet.decision(&second),
        TriggerDecision::Switch(Trigger::ConsumeFirst)
    );
    assert!(in_cooldown(
        Trigger::ConsumeFirst,
        &fleet.state,
        &fleet.settings,
        fleet.now()
    ));
    assert_eq!(fleet.tick(&second), None);
    assert_eq!(fleet.active, 2);
}

// cswap: TestConsumeFirstStrategy.test_reset_unknown_when_active_reset_missing
#[test]
fn reset_unknown_when_active_reset_missing() {
    let mut fleet = Fleet::consume_first(3);
    let [soon, _, latest, _] = resets(&fleet);
    let readings = rows([
        (1, weekly(20.0, 20.0, None)),
        (2, weekly(10.0, 10.0, Some(&soon))),
        (3, weekly(10.0, 10.0, Some(&latest))),
    ]);
    let ranking = fleet.ranking(Trigger::ConsumeFirst, &readings);
    assert_eq!(ranking.ordered, Vec::<u32>::new());
    assert!(ranking.any_known);
    assert_eq!(
        ranking.active_weekly_reset, None,
        "the hold is reset-unknown"
    );
    assert_eq!(fleet.tick(&readings), None);
}

// cswap: TestConsumeFirstStrategy.test_unreadable_candidates_stay_no_comparison
#[test]
fn unreadable_candidates_stay_no_comparison() {
    let fleet = Fleet::consume_first(3);
    let [_, later, _, _] = resets(&fleet);
    let ranking = fleet.ranking(
        Trigger::ConsumeFirst,
        &rows([(1, weekly(20.0, 20.0, Some(&later)))]),
    );
    assert_eq!(ranking.ordered, Vec::<u32>::new());
    assert!(!ranking.any_known);
}

// cswap: TestConsumeFirstStrategy.test_exhausted_candidates_hold_without_false_reset_claim
#[test]
fn exhausted_candidates_hold_without_false_reset_claim() {
    let fleet = Fleet::consume_first(3);
    let [soon, later, latest, _] = resets(&fleet);
    let readings = rows([
        (1, weekly(20.0, 20.0, Some(&later))),
        (2, weekly(100.0, 100.0, Some(&soon))),
        (3, weekly(100.0, 100.0, Some(&latest))),
    ]);
    let ranking = fleet.ranking(Trigger::ConsumeFirst, &readings);
    assert_eq!(ranking.ordered, Vec::<u32>::new());
    assert!(ranking.any_known);
    assert!(ranking.active_weekly_reset.is_some());
}

// cswap: TestConsumeFirstStrategy.test_skips_sooner_account_that_is_exhausted
#[test]
fn skips_sooner_account_that_is_exhausted() {
    let mut fleet = Fleet::consume_first(3);
    let [soon, later, latest, _] = resets(&fleet);
    let readings = rows([
        (1, weekly(20.0, 20.0, Some(&latest))),
        (2, weekly(100.0, 100.0, Some(&soon))),
        (3, weekly(10.0, 10.0, Some(&later))),
    ]);
    assert_eq!(fleet.tick(&readings), Some(3));
}

// cswap: TestConsumeFirstStrategy.test_candidate_with_past_reset_is_not_selected
#[test]
fn candidate_with_past_reset_is_not_selected() {
    let mut fleet = Fleet::consume_first(3);
    let [soon, later, _, past] = resets(&fleet);
    let readings = rows([
        (1, weekly(20.0, 20.0, Some(&later))),
        (2, weekly(10.0, 10.0, Some(&past))),
        (3, weekly(10.0, 10.0, Some(&soon))),
    ]);
    assert_eq!(fleet.tick(&readings), Some(3));
}

// cswap: TestConsumeFirstStrategy.test_active_past_reset_holds_reset_unknown
#[test]
fn active_past_reset_holds_reset_unknown() {
    let mut fleet = Fleet::consume_first(3);
    let [soon, later, _, past] = resets(&fleet);
    let readings = rows([
        (1, weekly(20.0, 20.0, Some(&past))),
        (2, weekly(10.0, 10.0, Some(&soon))),
        (3, weekly(10.0, 10.0, Some(&later))),
    ]);
    let ranking = fleet.ranking(Trigger::ConsumeFirst, &readings);
    assert!(ranking.any_known);
    assert_eq!(ranking.active_weekly_reset, None);
    assert_eq!(fleet.tick(&readings), None);
}

/// cswap's `TestConsumeFirstDepartureRecordsItsOwnTrigger` fleet: the account left (1) on 11 points, the active one
/// (2) on 5, no resets anywhere.
fn eleven_against_five(state: AutoswitchState) -> Fleet {
    let mut fleet = Fleet::new(2);
    fleet.active = 2;
    fleet.state = state;
    fleet
}

// cswap: TestConsumeFirstDepartureRecordsItsOwnTrigger.test_the_null_snapshot_answers_differently_by_recorded_trigger
#[test]
fn the_null_snapshot_answers_differently_by_recorded_trigger() {
    let readings = rows([(1, five(89.0)), (2, five(95.0))]);
    let failover = eleven_against_five(left(1, Snap::Null, Snap::Null, Some("failover")));
    let consume_first = eleven_against_five(left(1, Snap::Null, Snap::Null, Some("consume-first")));
    assert!(
        failover.recovered(&readings),
        "a failover departure's landing floor (h > 10) releases at 11 points"
    );
    assert!(
        !consume_first.recovered(&readings),
        "an ordinary departure needs h > 5 x 2 + 3 = 13"
    );
}

// cswap: TestConsumeFirstDepartureRecordsItsOwnTrigger.test_pre_upgrade_null_snapshot_without_leftTrigger_still_infers_failover
#[test]
fn pre_upgrade_null_snapshot_without_left_trigger_still_infers_failover() {
    let fleet = eleven_against_five(left(1, Snap::Null, Snap::Null, None));
    assert!(fleet.recovered(&rows([(1, five(89.0)), (2, five(95.0))])));
}

// cswap: TestConsumeFirstDepartureRecordsItsOwnTrigger
// .test_a_legacy_record_with_a_real_leftHeadroom_is_never_forced_through_the_failover_legs
#[test]
fn a_legacy_record_with_a_real_left_headroom_is_never_forced_through_the_failover_legs() {
    let fleet = eleven_against_five(left(1, Snap::At(10.0), Snap::Null, None));
    assert!(!fleet.recovered(&rows([(1, five(89.0)), (2, five(95.0))])));
}

// cswap: TestEveryAccountAboveThreshold.test_a_single_healthy_peer_still_wins_normally
#[test]
fn a_single_healthy_peer_still_wins_normally() {
    let mut fleet = Fleet::new(3);
    let readings = rows([
        (1, five_at(99.0, &fleet.reset_in(H))),
        (2, five_at(95.0, &fleet.reset_in(60))),
        (3, five_at(20.0, &fleet.reset_in(5 * H))),
    ]);
    assert_eq!(fleet.tick(&readings), Some(3));
}

// cswap: TestEveryAccountAboveThreshold.test_below_threshold_is_untouched
#[test]
fn below_threshold_is_untouched() {
    let mut fleet = Fleet::new(3);
    assert_eq!(
        fleet.tick(&rows([(1, five(50.0)), (2, five(10.0)), (3, five(10.0))])),
        None
    );
}

// cswap: TestEveryAccountAboveThreshold.test_all_at_limit_still_reports_exhausted
#[test]
fn all_at_limit_still_reports_exhausted() {
    let mut fleet = Fleet::new(3);
    let readings = rows([
        (1, five_at(100.0, &fleet.reset_in(600))),
        (2, five_at(100.0, &fleet.reset_in(300))),
        (3, five_at(100.0, &fleet.reset_in(900))),
    ]);
    assert_eq!(fleet.tick(&readings), None);
}

// cswap: TestEveryAccountAboveThreshold.test_at_limit_trigger_still_ignores_the_landing_rule
#[test]
fn at_limit_trigger_still_ignores_the_landing_rule() {
    let mut fleet = Fleet::new(3);
    let readings = rows([
        (1, five_at(100.0, &fleet.reset_in(60))),
        (2, five_at(30.0, &fleet.reset_in(86_400))),
        (3, five_at(95.0, &fleet.reset_in(120))),
    ]);
    assert_eq!(fleet.tick(&readings), Some(2));
    assert_eq!(fleet.state.left_trigger.as_deref(), Some("at-limit"));
}

// cswap: TestHorizonAxisDoesNotFlap.test_the_no_return_filter_does_not_block_the_at_limit_escape
#[test]
fn the_no_return_filter_does_not_block_the_at_limit_escape() {
    let mut fleet = Fleet::new(3);
    fleet.state.last_switch_from = Some(2);
    assert_eq!(
        fleet.tick(&rows([(1, five(100.0)), (2, five(0.0))])),
        Some(2)
    );
}

/// cswap's opening move of the bar tests: account 1 at 92 % (500 h out) left for account 2 at 10 % (400 h out), with a
/// third account at `third` (300 h out) when there is one; the clock then advances past the cooldown.
fn leave_one_for_two(fleet: &mut Fleet, third: Option<f64>, resets: &[String; 3]) {
    let mut readings = rows([
        (1, five_at(92.0, &resets[0])),
        (2, five_at(10.0, &resets[1])),
    ]);
    if let Some(pct) = third {
        readings.insert(3, five_at(pct, &resets[2]));
    }
    assert_eq!(
        fleet.tick(&readings),
        Some(2),
        "premise: the opening proactive move"
    );
    fleet.advance(301);
}

fn days_out(fleet: &Fleet) -> [String; 3] {
    [
        fleet.reset_in(500 * H),
        fleet.reset_in(400 * H),
        fleet.reset_in(300 * H),
    ]
}

// cswap: TestHorizonAxisDoesNotFlap.test_a_proactive_move_does_not_lock_out_the_next_one
#[test]
fn a_proactive_move_does_not_lock_out_the_next_one() {
    let mut fleet = Fleet::new(3);
    let resets = days_out(&fleet);
    leave_one_for_two(&mut fleet, None, &resets);
    let outcomes = fleet.walk(20, 1801, |_| {
        rows([
            (1, five_at(0.0, &resets[1])),
            (2, five_at(97.0, &resets[0])),
        ])
    });
    assert!(
        switched(&outcomes),
        "{outcomes:?}: one proactive move disabled proactive switching"
    );
}

// cswap: TestHorizonAxisDoesNotFlap.test_an_ordinary_departure_does_not_stall_on_a_weekly_bound_peer
#[test]
fn an_ordinary_departure_does_not_stall_on_a_weekly_bound_peer() {
    let mut fleet = Fleet::consume_first(2);
    let (r20, r10) = (fleet.reset_in(20 * H), fleet.reset_in(10 * H));
    let opening = rows([
        (1, weekly(30.0, 30.0, Some(&r20))),
        (2, weekly(50.0, 50.0, Some(&r10))),
    ]);
    assert_eq!(fleet.tick(&opening), Some(2));
    fleet.advance(301);
    let r400 = fleet.reset_in(400 * H);
    let mut outcomes = Vec::new();
    for pct in [50.0, 80.0, 90.0, 95.0, 98.0, 99.5, 100.0] {
        outcomes.push(fleet.tick(&rows([
            (1, weekly(30.0, 30.0, Some(&r20))),
            (2, weekly(pct, pct, Some(&r400))),
        ])));
        fleet.advance(301);
    }
    assert_eq!(
        outcomes[0], None,
        "{outcomes:?}: an absolute floor would release at once"
    );
    assert!(
        switched(&outcomes[..6]),
        "{outcomes:?}: the engine returned only at a hard limit"
    );
}

/// Whether some account in the census holds headroom, the evidence cswap's all-exhausted report must not lose.
fn census_has_headroom(fleet: &Fleet, readings: &Readings) -> bool {
    fleet.census().oauth.iter().any(|account| {
        source(readings)
            .usage(*account)
            .and_then(|usage| usage.headroom())
            .is_some_and(|headroom| headroom > 0.0)
    })
}

// cswap: TestHorizonAxisDoesNotFlap.test_a_filtered_candidate_does_not_forge_an_all_exhausted_claim (the census the
// report reads; the report is the tick's)
#[test]
fn a_filtered_candidate_does_not_forge_an_all_exhausted_claim() {
    let mut fleet = Fleet::new(3);
    let resets = days_out(&fleet);
    leave_one_for_two(&mut fleet, Some(100.0), &resets);
    let readings = rows([
        (1, five_at(0.0, &resets[1])),
        (2, five_at(97.0, &resets[0])),
        (3, five_at(100.0, &resets[2])),
    ]);
    assert!(
        census_has_headroom(&fleet, &readings),
        "account 1 is at 0 %; the fleet is not exhausted"
    );
}

// cswap: TestHorizonAxisDoesNotFlap.test_the_bar_does_not_hide_the_account_from_the_census
#[test]
fn the_bar_does_not_hide_the_account_from_the_census() {
    let mut fleet = Fleet::new(3);
    let resets = days_out(&fleet);
    leave_one_for_two(&mut fleet, Some(100.0), &resets);
    let readings = rows([
        (1, five_at(85.0, &resets[1])),
        (2, five_at(90.0, &resets[0])),
        (3, five_at(100.0, &resets[2])),
    ]);
    let recovered = fleet.recovered(&readings);
    assert!(
        recovered,
        "premise: 15 points is 3 more than the 8 it was left on"
    );
    assert_eq!(
        fleet.barred(Trigger::Proactive, &readings, recovered),
        Some(1),
        "premise: 15 points does not reach twice the active's 10, so the bar is in effect"
    );
    assert_eq!(fleet.census().oauth, [1, 3]);
    assert!(census_has_headroom(&fleet, &readings));
}

// cswap: TestHorizonAxisDoesNotFlap.test_the_bar_lifts_when_it_would_leave_nothing
#[test]
fn the_bar_lifts_when_it_would_leave_nothing() {
    let mut fleet = Fleet::consume_first(2);
    let (r500, r400) = (fleet.reset_in(500 * H), fleet.reset_in(400 * H));
    assert_eq!(
        fleet.tick(&rows([
            (1, weekly(95.0, 95.0, Some(&r500))),
            (2, weekly(5.0, 5.0, Some(&r400)))
        ])),
        Some(2)
    );
    fleet.advance(301);
    let r10 = fleet.reset_in(10 * H);
    let outcomes = fleet.walk(20, 1801, |_| {
        rows([
            (1, weekly(0.0, 0.0, Some(&r10))),
            (2, weekly(30.0, 30.0, Some(&r500))),
        ])
    });
    assert!(
        switched(&outcomes),
        "{outcomes:?}: the only peer was barred with no way to lift it"
    );
}

// cswap: TestHorizonAxisDoesNotFlap.test_the_bar_never_applies_to_an_escape
#[test]
fn the_bar_never_applies_to_an_escape() {
    let mut fleet = Fleet::new(3);
    fleet.active = 2;
    fleet.state.last_switch_from = Some(1);
    let readable = rows([(1, five(85.0)), (2, five(90.0)), (3, five(99.0))]);
    let unreadable = rows([(1, five(85.0)), (3, five(99.0))]);
    for trigger in [Trigger::AtLimit, Trigger::Failover] {
        for readings in [&readable, &unreadable] {
            assert_eq!(
                fleet.barred(trigger, readings, true),
                None,
                "{trigger:?} barred the account we left"
            );
        }
    }
    assert_eq!(
        fleet.barred(Trigger::Proactive, &readable, true),
        Some(1),
        "premise: a proactive tick bars it"
    );
}

// cswap: TestHorizonAxisDoesNotFlap.test_the_bar_actually_removes_the_account_from_the_ranking
#[test]
fn the_bar_actually_removes_the_account_from_the_ranking() {
    let mut fleet = Fleet::new(3);
    fleet.active = 2;
    let census = Candidates { oauth: vec![1, 3] };
    let readings = rows([(1, five(40.0)), (2, five(96.0)), (3, five(99.0))]);
    let usage = source(&readings);
    let inputs = fleet.inputs(Trigger::Proactive, &census, &usage);
    assert_eq!(
        rank_candidates(&inputs, None).ordered,
        [1],
        "premise: unbarred, account 1 is the pick"
    );
    assert_eq!(rank_candidates(&inputs, Some(1)).ordered, Vec::<u32>::new());
}

// cswap: TestHorizonAxisDoesNotFlap.test_an_unreadable_barred_account_does_not_crash_the_tick
#[test]
fn an_unreadable_barred_account_does_not_crash_the_tick() {
    let mut fleet = Fleet::new(3);
    fleet.active = 2;
    fleet.state.last_switch_from = Some(1);
    let readings = rows([(2, five(90.0)), (3, five(60.0))]);
    assert_eq!(
        fleet.barred(Trigger::Proactive, &readings, true),
        Some(1),
        "unknown headroom is no evidence it beats us"
    );
}

/// cswap's flap fleet: account 1 on 8 points (500 h out) left for account 2 on 30 (400 h out), then an hour on.
fn left_at_eight_for_thirty(fleet: &mut Fleet) -> [String; 2] {
    let resets = [fleet.reset_in(500 * H), fleet.reset_in(400 * H)];
    assert_eq!(
        fleet.tick(&rows([
            (1, five_at(92.0, &resets[0])),
            (2, five_at(70.0, &resets[1]))
        ])),
        Some(2)
    );
    fleet.advance(3612);
    resets
}

// cswap: TestHorizonAxisDoesNotFlap.test_the_release_fires_on_this_same_fleet_once_the_peer_actually_improves
#[test]
fn the_release_fires_on_this_same_fleet_once_the_peer_actually_improves() {
    let mut fleet = Fleet::new(2);
    let resets = left_at_eight_for_thirty(&mut fleet);
    let outcomes = fleet.walk(10, 301, |_| {
        rows([
            (1, five_at(0.0, &resets[0])),
            (2, five_at(98.0, &resets[1])),
        ])
    });
    assert!(
        switched(&outcomes),
        "{outcomes:?}: a bar that never releases is the lockout"
    );
}

// cswap: TestHorizonAxisDoesNotFlap.test_a_departure_at_full_quota_is_immediately_eligible
#[test]
fn a_departure_at_full_quota_is_immediately_eligible() {
    let mut fleet = Fleet::consume_first(2);
    let (r500, r100) = (fleet.reset_in(500 * H), fleet.reset_in(100 * H));
    assert_eq!(
        fleet.tick(&rows([
            (1, weekly(0.0, 0.0, Some(&r500))),
            (2, weekly(0.0, 0.0, Some(&r100)))
        ])),
        Some(2)
    );
    assert!(
        matches!(fleet.state.left_headroom, Some(Some(headroom)) if (headroom - 100.0).abs() < f64::EPSILON),
        "premise: a full-quota departure"
    );
    fleet.advance(301);
    let outcomes = fleet.walk(10, 301, |_| {
        rows([
            (1, weekly(0.0, 0.0, Some(&r500))),
            (2, weekly(90.0, 0.0, Some(&r100))),
        ])
    });
    assert!(
        switched(&outcomes),
        "{outcomes:?}: refusing a full account is the unsatisfiable-above-97 lockout"
    );
}

// cswap: TestHorizonAxisDoesNotFlap.test_the_clamp_stays_load_bearing_when_dominance_does_not_fire
#[test]
fn the_clamp_stays_load_bearing_when_dominance_does_not_fire() {
    let mut fleet = Fleet::new(2);
    fleet.active = 2;
    fleet.state = left(1, Snap::At(98.0), Snap::Null, None);
    assert!(
        fleet.recovered(&rows([(1, five(0.0)), (2, five(40.0))])),
        "100 >= min(98 + 3, 100) releases"
    );
}

// cswap: TestHorizonAxisDoesNotFlap.test_the_dominance_leg_does_not_silently_read_an_unreadable_active_as_no_dominance
#[test]
fn the_dominance_leg_does_not_silently_read_an_unreadable_active_as_no_dominance() {
    let mut fleet = Fleet::new(2);
    fleet.state = left(2, Snap::At(40.0), Snap::Null, None);
    assert!(
        fleet.recovered(&rows([(1, five(98.0)), (2, five(60.0))])),
        "premise: a readable active is dominated"
    );
    assert!(
        fleet.recovered(&rows([(2, five(60.0))])),
        "an unreadable active must not read as no dominance"
    );
}

// cswap: TestHorizonAxisDoesNotFlap
// .test_no_return_account_does_not_re_bar_an_already_recovered_peer_when_the_active_is_unreadable
#[test]
fn no_return_account_does_not_re_bar_an_already_recovered_peer_when_the_active_is_unreadable() {
    let mut fleet = Fleet::new(2);
    fleet.state = left(2, Snap::At(40.0), Snap::Null, None);
    for trigger in [Trigger::Proactive, Trigger::ConsumeFirst] {
        assert_eq!(
            fleet.barred(trigger, &rows([(1, five(98.0)), (2, five(60.0))]), true),
            None,
            "premise: {trigger:?}"
        );
        assert_eq!(
            fleet.barred(trigger, &rows([(2, five(60.0))]), true),
            None,
            "{trigger:?} re-barred it"
        );
    }
}

// cswap: TestHorizonAxisDoesNotFlap.test_the_all_spent_recovery_leg_carries_its_own_hysteresis
#[test]
fn the_all_spent_recovery_leg_carries_its_own_hysteresis() {
    let mut fleet = Fleet::new(2);
    fleet.active = 2;
    fleet.state = left(1, Snap::Null, Snap::Null, None);
    let readings = rows([
        (1, five_at(95.0, &fleet.reset_in(3600))),
        (2, five_at(98.0, &fleet.reset_in(3660))),
    ]);
    assert!(
        !fleet.recovered(&readings),
        "60 s sooner is inside RECOVERY_HYSTERESIS_S"
    );
}

// cswap: TestHorizonAxisDoesNotFlap.test_the_dominance_fallback_does_not_fire_below_its_own_floor
#[test]
fn the_dominance_fallback_does_not_fire_below_its_own_floor() {
    let mut fleet = Fleet::new(2);
    fleet.state = left(2, Snap::At(40.0), Snap::Null, None);
    assert!(
        !fleet.recovered(&rows([(2, five(95.0))])),
        "5 points is below the landing floor of 10"
    );
}

// cswap: TestHorizonAxisDoesNotFlap.test_no_return_accounts_unreadable_active_fallback_has_a_floor_too
#[test]
fn no_return_accounts_unreadable_active_fallback_has_a_floor_too() {
    let mut fleet = Fleet::new(2);
    fleet.state.last_switch_from = Some(2);
    assert_eq!(
        fleet.barred(Trigger::Proactive, &rows([(2, five(95.0))]), true),
        Some(2)
    );
}

// cswap: TestHorizonAxisDoesNotFlap.test_a_switch_that_recorded_no_snapshot_still_releases
#[test]
fn a_switch_that_recorded_no_snapshot_still_releases() {
    let mut fleet = Fleet::new(2);
    fleet.state.last_switch_from = Some(2);
    let (r500, r400) = (fleet.reset_in(500 * H), fleet.reset_in(400 * H));
    let outcomes = fleet.walk(20, 1801, |_| {
        rows([(1, five_at(97.0, &r500)), (2, five_at(0.0, &r400))])
    });
    assert!(
        switched(&outcomes),
        "{outcomes:?}: a record without a snapshot barred the only peer forever"
    );
}

// cswap: TestHorizonAxisDoesNotFlap.test_absence_of_a_snapshot_releases_even_a_poor_or_unreadable_peer
#[test]
fn absence_of_a_snapshot_releases_even_a_poor_or_unreadable_peer() {
    let mut fleet = Fleet::new(2);
    fleet.state.last_switch_from = Some(2);
    assert!(
        fleet.recovered(&rows([(1, five(98.0)), (2, five(96.0))])),
        "a poor peer"
    );
    assert!(
        fleet.recovered(&rows([(1, five(98.0))])),
        "an unreadable peer"
    );
}

/// cswap's frozen-peer walk after a failover: account 1 frozen on 4 points while account 2 burns through `pcts`.
fn after_failover(fleet: &mut Fleet, pcts: &[f64]) -> Vec<Option<u32>> {
    let (frozen, r400) = (fleet.reset_in(500 * H), fleet.reset_in(400 * H));
    fleet.fail_over_to_2(Some(&r400));
    fleet.advance(301);
    let mut outcomes = Vec::new();
    for pct in pcts {
        outcomes.push(fleet.tick(&rows([
            (1, five_at(96.0, &frozen)),
            (2, five_at(*pct, &r400)),
        ])));
        fleet.advance(301);
    }
    outcomes
}

// cswap: TestHorizonAxisDoesNotFlap.test_a_failover_departure_still_unreadable_does_not_crash_or_release
#[test]
fn a_failover_departure_still_unreadable_does_not_crash_or_release() {
    let mut fleet = Fleet::new(2);
    let r400 = fleet.reset_in(400 * H);
    fleet.fail_over_to_2(Some(&r400));
    fleet.advance(301);
    let outcomes = fleet.walk(10, 301, |_| rows([(2, five_at(98.0, &r400))]));
    assert!(
        !switched(&outcomes),
        "{outcomes:?}: still unmeasurable is not a recovery"
    );
}

/// cswap's failover-then-release walks: after the failover, ten ticks with account 1 at `peer` (10 h out) and account
/// 2 at `active` (400 h out).
fn release_after_failover(peer: f64, active: f64) -> Vec<Option<u32>> {
    let mut fleet = Fleet::new(2);
    let r400 = fleet.reset_in(400 * H);
    fleet.fail_over_to_2(Some(&r400));
    fleet.advance(301);
    let r10 = fleet.reset_in(10 * H);
    fleet.walk(10, 301, |_| {
        rows([(1, five_at(peer, &r10)), (2, five_at(active, &r400))])
    })
}

// cswap: TestHorizonAxisDoesNotFlap.test_a_failover_departure_releases_once_the_peer_is_readable_again
#[test]
fn a_failover_departure_releases_once_the_peer_is_readable_again() {
    let outcomes = release_after_failover(0.0, 95.0);
    assert!(
        switched(&outcomes),
        "{outcomes:?}: a permanent proactive lockout"
    );
}

// cswap: TestHorizonAxisDoesNotFlap.test_a_failover_departure_releases_a_healthy_but_not_near_full_peer
#[test]
fn a_failover_departure_releases_a_healthy_but_not_near_full_peer() {
    let outcomes = release_after_failover(30.0, 98.0);
    assert!(
        switched(&outcomes),
        "{outcomes:?}: a 70-point peer was held by an absolute floor"
    );
}

// cswap: TestHorizonAxisDoesNotFlap.test_a_failover_hold_still_escapes_at_limit
#[test]
fn a_failover_hold_still_escapes_at_limit() {
    let mut fleet = Fleet::new(2);
    let (frozen, r400) = (fleet.reset_in(500 * H), fleet.reset_in(400 * H));
    let held = after_failover(&mut fleet, &[98.0, 98.0, 98.0]);
    assert!(
        !switched(&held),
        "premise: the all-above hold blocks the proactive return"
    );
    assert_eq!(
        fleet.tick(&rows([
            (1, five_at(0.0, &frozen)),
            (2, five_at(100.0, &r400))
        ])),
        Some(1)
    );
}

// cswap: TestHorizonAxisDoesNotFlap.test_the_release_fires_when_the_barred_account_recovered
#[test]
fn the_release_fires_when_the_barred_account_recovered() {
    let mut fleet = Fleet::new(2);
    let resets = left_at_eight_for_thirty(&mut fleet);
    let outcomes = fleet.walk(10, 301, |_| {
        rows([
            (1, five_at(0.0, &resets[0])),
            (2, five_at(98.0, &resets[1])),
        ])
    });
    assert!(
        switched(&outcomes),
        "{outcomes:?}: account 1 came back to full quota"
    );
}

// cswap: TestHorizonAxisDoesNotFlap.test_the_ratio_release_changes_where_the_engine_lands
#[test]
fn the_ratio_release_changes_where_the_engine_lands() {
    let mut fleet = Fleet::new(3);
    let resets = days_out(&fleet);
    leave_one_for_two(&mut fleet, Some(70.0), &resets);
    let readings = rows([
        (1, five_at(20.0, &resets[0])),
        (2, five_at(90.0, &resets[1])),
        (3, five_at(70.0, &resets[2])),
    ]);
    assert_eq!(
        fleet.tick(&readings),
        Some(1),
        "the account left holds 8 times the active's headroom"
    );
}

// cswap: TestHorizonAxisDoesNotFlap.test_the_bar_is_recomputed_on_the_phase_two_snapshot (the bar is read from the
// snapshot the ranking is given; the phase-two refetch is the tick's)
#[test]
fn the_bar_is_recomputed_on_the_phase_two_snapshot() {
    let mut fleet = Fleet::consume_first(3);
    let (r500, r10, r400) = (
        fleet.reset_in(500 * H),
        fleet.reset_in(10 * H),
        fleet.reset_in(400 * H),
    );
    let opening = rows([
        (1, weekly(20.0, 20.0, Some(&r500))),
        (2, weekly(5.0, 5.0, Some(&r10))),
        (3, weekly(5.0, 5.0, Some(&r400))),
    ]);
    assert_eq!(fleet.tick(&opening), Some(2));
    fleet.advance(301);
    let stale = rows([
        (1, weekly(80.0, 80.0, Some(&r10))),
        (2, weekly(70.0, 70.0, Some(&r500))),
        (3, weekly(60.0, 60.0, Some(&r400))),
    ]);
    let fresh = rows([
        (1, weekly(10.0, 10.0, Some(&r10))),
        (2, weekly(85.0, 85.0, Some(&r500))),
        (3, weekly(60.0, 60.0, Some(&r400))),
    ]);
    assert_eq!(
        fleet.ranking(Trigger::ConsumeFirst, &stale).ordered.first(),
        Some(&3),
        "premise: the stale snapshot bars 1"
    );
    assert_eq!(
        fleet.tick(&fresh),
        Some(1),
        "on the fresh snapshot the account left is released and resets soonest"
    );
}

// cswap: TestTheReleasePredicateOneStateShapePerTest.test_failover_peer_readable_dominant_and_changed_releases
#[test]
fn failover_peer_readable_dominant_and_changed_releases() {
    let mut fleet = Fleet::new(2);
    fleet.state = left(2, Snap::Null, Snap::Null, None);
    assert!(
        fleet.recovered(&rows([(1, five(98.0)), (2, five(85.0))])),
        "15 points clears the floor of 10"
    );
}

// cswap: TestTheReleasePredicateOneStateShapePerTest.test_the_failover_floor_moves_with_the_users_threshold
#[test]
fn the_failover_floor_moves_with_the_users_threshold() {
    let mut fleet = Fleet::new(2);
    fleet.state = left(2, Snap::Null, Snap::Null, None);
    let readings = rows([(1, five(98.0)), (2, five(65.0))]);
    fleet.settings.threshold = 60.0;
    assert!(
        !fleet.recovered(&readings),
        "threshold 60: the floor is 40, above the peer's 35"
    );
    fleet.settings.threshold = 71.0;
    assert!(
        fleet.recovered(&readings),
        "threshold 71: the floor is 29, below the peer's 35"
    );
}

// cswap: TestTheReleasePredicateOneStateShapePerTest.test_failover_peer_still_poor_holds
#[test]
fn failover_peer_still_poor_holds() {
    let mut fleet = Fleet::new(2);
    fleet.state = left(2, Snap::Null, Snap::Null, None);
    assert!(!fleet.recovered(&rows([(1, five(98.0)), (2, five(95.0))])));
}

// cswap: TestTheReleasePredicateOneStateShapePerTest.test_failover_peer_still_unreadable_holds
#[test]
fn failover_peer_still_unreadable_holds() {
    let mut fleet = Fleet::new(2);
    fleet.state = left(2, Snap::Null, Snap::Null, None);
    assert!(!fleet.recovered(&rows([(1, five(98.0))])));
}

// cswap: TestTheReleasePredicateOneStateShapePerTest.test_ordinary_departure_weekly_bound_peer_that_recovered_releases
#[test]
fn ordinary_departure_weekly_bound_peer_that_recovered_releases() {
    let mut fleet = Fleet::new(2);
    fleet.state = left(2, Snap::At(4.0), Snap::At(fleet.secs_in(3600)), None);
    let readings = rows([(1, five(92.0)), (2, five_at(96.0, &fleet.reset_in(60)))]);
    assert!(
        fleet.recovered(&readings),
        "the binding reset moved an hour nearer"
    );
}

// cswap: TestTheReleasePredicateOneStateShapePerTest.test_pre_upgrade_record_keys_absent_releases
#[test]
fn pre_upgrade_record_keys_absent_releases() {
    let mut fleet = Fleet::new(2);
    fleet.state.last_switch_from = Some(2);
    assert!(fleet.recovered(&rows([(1, five(98.0)), (2, five(96.0))])));
}

// cswap: TestTheReleasePredicateOneStateShapePerTest.test_at_limit_escape_still_works
#[test]
fn at_limit_escape_still_works() {
    let mut fleet = Fleet::new(2);
    fleet.state.last_switch_from = Some(2);
    for readings in [
        rows([(1, five(100.0)), (2, five(0.0))]),
        rows([(2, five(0.0))]),
    ] {
        for recovered in [true, false] {
            assert_eq!(fleet.barred(Trigger::AtLimit, &readings, recovered), None);
        }
    }
}

// cswap: TestEscapeBeforeTheLimitLands.test_at_99_the_proactive_path_already_escapes
#[test]
fn at_99_the_proactive_path_already_escapes() {
    let mut fleet = Fleet::new(3);
    let readings = rows([
        (1, five_at(99.0, &fleet.reset_in(109 * H))),
        (2, five_at(70.0, &fleet.reset_in(80 * H))),
        (3, five_at(100.0, &fleet.reset_in(50 * H))),
    ]);
    assert_eq!(fleet.tick(&readings), Some(2));
    assert_eq!(fleet.state.left_trigger.as_deref(), Some("proactive"));
}

// cswap: TestEscapeBeforeTheLimitLands.test_below_the_brink_the_ordinary_rules_still_decide
#[test]
fn below_the_brink_the_ordinary_rules_still_decide() {
    let mut fleet = Fleet::new(3);
    let readings = rows([
        (1, five_at(50.0, &fleet.reset_in(109 * H))),
        (2, five_at(45.0, &fleet.reset_in(80 * H))),
        (3, five_at(40.0, &fleet.reset_in(50 * H))),
    ]);
    assert_eq!(fleet.tick(&readings), None);
}

// cswap: TestReviewFindings202.test_recovery_reads_the_binding_windows_reset
#[test]
fn recovery_reads_the_binding_windows_reset() {
    let fleet = Fleet::new(1);
    let reading = json!({"five_hour": {"pct": 40.0, "resets_at": fleet.reset_in(3600)}, "seven_day": {"pct": 95.0}});
    assert_eq!(
        binding_recovery(
            Some(&AccountUsage::from_reading(&reading, &[])),
            fleet.now()
        ),
        None
    );
}

// cswap: TestReviewFindings202.test_at_limit_still_ranks_by_headroom_when_all_are_above
#[test]
fn at_limit_still_ranks_by_headroom_when_all_are_above() {
    let mut fleet = Fleet::new(3);
    let readings = rows([
        (1, five_at(100.0, &fleet.reset_in(60))),
        (2, five_at(91.0, &fleet.reset_in(86_400))),
        (3, five_at(97.0, &fleet.reset_in(120))),
    ]);
    assert_eq!(fleet.tick(&readings), Some(2));
    assert_eq!(fleet.state.left_trigger.as_deref(), Some("at-limit"));
}
