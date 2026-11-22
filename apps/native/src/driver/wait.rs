//! `wait_for`: the predicates a wait reads, and the clock-free loop that
//! decides, after each read, whether the wait is over or how long to sleep.

use std::time::Duration;

use load_clock::{BACKSTOP_FACTOR, Figures};
use serde_json::{Map, Value};

use crate::driver::protocol::WaitParams;

/// How long a wait lasts when the params do not say, and its bounds.
const DEFAULT_TIMEOUT_MS: u64 = 5000;
const MIN_TIMEOUT_MS: u64 = 100;
const MAX_TIMEOUT_MS: u64 = 60_000;

/// How often a wait reads its predicate when the params do not say, and
/// its bounds.
const DEFAULT_INTERVAL_MS: u64 = 100;
const MIN_INTERVAL_MS: u64 = 20;
const MAX_INTERVAL_MS: u64 = 1000;

/// What a wait waits on.
#[derive(Clone, Debug, PartialEq)]
pub enum Predicate {
    /// A value in the `state` method's result, at a dotted path whose
    /// numeric segments index arrays (`tabs.0.id`).
    State { path: String, test: StateTest },
    /// Pane `pane`'s visible text holds `contains`.
    PaneText { pane: String, contains: String },
    /// The registry element `name` shows some part of itself, or none.
    Element { name: String, visible: bool },
}

/// What a state predicate asks of the value at its path.
#[derive(Clone, Debug, PartialEq)]
pub enum StateTest {
    /// The value is present and equal to this one.
    Equals(Value),
    /// The value is present and not `null`, or (with `false`) it is not.
    Exists(bool),
}

/// The predicate shapes, as a malformed one's hint lists them.
pub const PREDICATE_SHAPES: &str = r#"until takes {"state": "<path>", "equals": <json>}, {"state": "<path>", "exists": <bool>}, {"pane_text": {"pane": "<id>", "contains": "<text>"}} or {"element": "<name>", "visible": <bool>}"#;

/// The predicate `until` writes.
///
/// # Errors
///
/// What is wrong with `until`: not an object, no predicate key, an unknown
/// key, a missing or wrong-typed field, or both `equals` and `exists`.
pub fn parse_predicate(until: &Value) -> Result<Predicate, String> {
    let Some(fields) = until.as_object() else {
        return Err(format!("until must be an object, not {until}"));
    };
    if fields.contains_key("state") {
        state_predicate(fields)
    } else if fields.contains_key("pane_text") {
        pane_text_predicate(fields)
    } else if fields.contains_key("element") {
        element_predicate(fields)
    } else {
        let keys: Vec<&str> = fields.keys().map(String::as_str).collect();
        Err(format!(
            "until names no predicate (keys: {})",
            keys.join(", ")
        ))
    }
}

fn state_predicate(fields: &Map<String, Value>) -> Result<Predicate, String> {
    only_keys(fields, &["state", "equals", "exists"], "until")?;
    let path = string_field(fields, "state")?;
    if path.is_empty() {
        return Err("state names no path".to_owned());
    }
    let test = match (fields.get("equals"), fields.contains_key("exists")) {
        (Some(_), true) => return Err("give equals or exists, not both".to_owned()),
        (Some(value), false) => StateTest::Equals(value.clone()),
        (None, true) => StateTest::Exists(bool_field(fields, "exists")?),
        (None, false) => return Err("a state predicate needs equals or exists".to_owned()),
    };
    Ok(Predicate::State { path, test })
}

fn pane_text_predicate(fields: &Map<String, Value>) -> Result<Predicate, String> {
    only_keys(fields, &["pane_text"], "until")?;
    let Some(inner) = fields.get("pane_text").and_then(Value::as_object) else {
        return Err("pane_text must be an object with pane and contains".to_owned());
    };
    only_keys(inner, &["pane", "contains"], "pane_text")?;
    Ok(Predicate::PaneText {
        pane: string_field(inner, "pane")?,
        contains: string_field(inner, "contains")?,
    })
}

fn element_predicate(fields: &Map<String, Value>) -> Result<Predicate, String> {
    only_keys(fields, &["element", "visible"], "until")?;
    Ok(Predicate::Element {
        name: string_field(fields, "element")?,
        visible: bool_field(fields, "visible")?,
    })
}

fn only_keys(fields: &Map<String, Value>, allowed: &[&str], within: &str) -> Result<(), String> {
    match fields.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(format!("unknown key {key} in {within}")),
        None => Ok(()),
    }
}

fn string_field(fields: &Map<String, Value>, key: &str) -> Result<String, String> {
    match fields.get(key) {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(other) => Err(format!("{key} must be a string, not {other}")),
        None => Err(format!("missing {key}")),
    }
}

fn bool_field(fields: &Map<String, Value>, key: &str) -> Result<bool, String> {
    match fields.get(key) {
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(other) => Err(format!("{key} must be true or false, not {other}")),
        None => Err(format!("missing {key}")),
    }
}

/// The value at the dotted `path` in `root`, a numeric segment indexing an
/// array; `None` when the path leads nowhere.
#[must_use]
pub fn value_at<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(root, |value, segment| match value {
            Value::Object(fields) => fields.get(segment),
            Value::Array(items) => segment.parse::<usize>().ok().and_then(|at| items.get(at)),
            _ => None,
        })
}

/// What a wait does after a read.
#[derive(Debug, PartialEq)]
pub enum Step {
    /// The wait is over; its reply's result object.
    Done(Value),
    /// Sleep this long, then read again.
    Again(Duration),
}

/// One wait's bounds: how long it lasts in load-adjusted time and how often
/// it reads in wall time, clamped. It reads no clock; each step is given the
/// time spent.
#[derive(Debug, Clone, Copy)]
pub struct WaitLoop {
    timeout: Duration,
    interval: Duration,
}

impl WaitLoop {
    /// The wait `params` ask for, its timeout and interval defaulted and
    /// clamped.
    #[must_use]
    pub fn new(params: &WaitParams) -> Self {
        let timeout = params
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);
        let interval = params
            .interval_ms
            .unwrap_or(DEFAULT_INTERVAL_MS)
            .clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS);
        Self {
            timeout: Duration::from_millis(timeout),
            interval: Duration::from_millis(interval),
        }
    }

    /// The wait's timeout in load-adjusted time, clamped.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// What follows a read that found the predicate `met`, having read
    /// `last`, `spent` into the wait: the reply once it is met, the timeout
    /// has passed in load-adjusted time, or [`BACKSTOP_FACTOR`] times it in
    /// wall time; else a sleep that never runs past the load-adjusted time
    /// left by more than the least interval (20 ms), which it never goes
    /// under: that time left is not wall time, and a shorter sleep would
    /// poll the UI thread at the timer's tick rate.
    #[must_use]
    pub fn step(&self, met: bool, last: Value, spent: Figures) -> Step {
        let ceiling = spent.adjusted >= self.timeout;
        let backstop = spent.wall >= self.timeout.saturating_mul(BACKSTOP_FACTOR);
        if met || ceiling || backstop {
            let mut result = Map::new();
            result.insert("met".to_owned(), Value::Bool(met));
            result.insert("waited_ms".to_owned(), Value::from(millis(spent.wall)));
            result.insert(
                "load_adjusted_ms".to_owned(),
                Value::from(millis(spent.adjusted)),
            );
            result.insert("last".to_owned(), last);
            if !met && !ceiling {
                result.insert("timed_out_by".to_owned(), Value::from("backstop"));
            }
            return Step::Done(Value::Object(result));
        }
        let left = self.timeout.saturating_sub(spent.adjusted);
        Step::Again(
            self.interval
                .min(left)
                .max(Duration::from_millis(MIN_INTERVAL_MS)),
        )
    }
}

/// `span` in whole milliseconds.
fn millis(span: Duration) -> u64 {
    u64::try_from(span.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the step it lost"
)]
mod tests {
    use std::time::Duration;

    use load_clock::Figures;
    use serde_json::{Value, json};

    use super::{Predicate, StateTest, Step, WaitLoop, parse_predicate};
    use crate::driver::protocol::WaitParams;

    fn params(timeout_ms: Option<u64>, interval_ms: Option<u64>) -> WaitParams {
        WaitParams {
            until: json!({"state": "overlay", "equals": "settings"}),
            timeout_ms,
            interval_ms,
        }
    }

    /// The time `wait` has spent: `wall_ms` of wall time, `adjusted_ms` of it
    /// load-adjusted.
    fn spent(wait: &WaitLoop, wall_ms: u64, adjusted_ms: u64) -> Figures {
        Figures {
            budget: wait.timeout(),
            wall: Duration::from_millis(wall_ms),
            adjusted: Duration::from_millis(adjusted_ms),
        }
    }

    fn done(step: Step) -> Value {
        match step {
            Step::Done(result) => Some(result),
            Step::Again(_) => None,
        }
        .expect("the wait is done")
    }

    fn again(step: &Step) -> Duration {
        match step {
            Step::Again(sleep) => Some(*sleep),
            Step::Done(_) => None,
        }
        .expect("the wait goes on")
    }

    #[test]
    fn driver_wait_for_times_out_with_last_value() {
        let wait = WaitLoop::new(&params(Some(300), Some(100)));
        for (at, seen) in [(0, "a"), (100, "b"), (200, "c")] {
            let step = wait.step(false, json!(seen), spent(&wait, at, at));
            assert!(
                matches!(step, Step::Again(_)),
                "not met at {at} ms waits on"
            );
        }

        let result = done(wait.step(false, json!("d"), spent(&wait, 300, 300)));

        assert_eq!(
            result,
            json!({"met": false, "waited_ms": 300, "load_adjusted_ms": 300, "last": "d"})
        );
    }

    #[test]
    fn wait_for_met_on_first_read_answers_at_once() {
        let wait = WaitLoop::new(&params(None, None));

        let result = done(wait.step(true, json!("settings"), spent(&wait, 0, 0)));

        assert_eq!(
            result,
            json!({"met": true, "waited_ms": 0, "load_adjusted_ms": 0, "last": "settings"})
        );
    }

    #[test]
    fn a_loaded_wait_outlasts_its_wall_timeout() {
        let wait = WaitLoop::new(&params(Some(300), Some(100)));

        let sleep = again(&wait.step(false, json!("a"), spent(&wait, 1000, 250)));

        assert_eq!(sleep, Duration::from_millis(50));
    }

    #[test]
    fn a_wait_ends_at_its_timeout_in_load_adjusted_time() {
        let wait = WaitLoop::new(&params(Some(300), Some(100)));

        let result = done(wait.step(false, json!("d"), spent(&wait, 1200, 300)));

        assert_eq!(
            result,
            json!({"met": false, "waited_ms": 1200, "load_adjusted_ms": 300, "last": "d"})
        );
    }

    #[test]
    fn a_wait_ends_at_its_backstop() {
        let wait = WaitLoop::new(&params(Some(300), Some(100)));
        assert!(
            matches!(
                wait.step(false, json!("c"), spent(&wait, 1499, 60)),
                Step::Again(_)
            ),
            "short of 5 x the timeout in wall time it waits on"
        );

        let result = done(wait.step(false, json!("d"), spent(&wait, 1500, 60)));

        assert_eq!(
            result,
            json!({
                "met": false,
                "waited_ms": 1500,
                "load_adjusted_ms": 60,
                "last": "d",
                "timed_out_by": "backstop",
            })
        );
    }

    #[test]
    fn a_met_wait_ends_at_once_with_both_figures() {
        let wait = WaitLoop::new(&params(Some(300), Some(100)));

        let result = done(wait.step(true, json!("settings"), spent(&wait, 400, 120)));

        assert_eq!(
            result,
            json!({"met": true, "waited_ms": 400, "load_adjusted_ms": 120, "last": "settings"})
        );
    }

    #[test]
    fn the_sleep_never_passes_the_adjusted_time_left() {
        let wait = WaitLoop::new(&params(Some(250), Some(100)));
        for (wall, adjusted) in [(0, 0), (500, 100), (900, 160), (1000, 200), (1100, 230)] {
            let sleep = again(&wait.step(false, Value::Null, spent(&wait, wall, adjusted)));
            assert!(
                sleep <= Duration::from_millis(250 - adjusted),
                "at {adjusted} ms adjusted it sleeps {sleep:?}"
            );
            assert!(
                sleep >= Duration::from_millis(20),
                "at {adjusted} ms adjusted it sleeps at least 20 ms"
            );
        }
        assert_eq!(
            again(&wait.step(false, Value::Null, spent(&wait, 1000, 200))),
            Duration::from_millis(50),
            "the last sleep ends at the timeout in load-adjusted time"
        );
        assert_eq!(
            again(&wait.step(false, Value::Null, spent(&wait, 1200, 249))),
            Duration::from_millis(20),
            "under 20 ms left it sleeps the least interval"
        );
    }

    #[test]
    fn wait_for_clamps_timeout_and_interval() {
        let cases = [
            ((Some(0), Some(1)), (100, 20)),
            ((Some(1_000_000_000), Some(5000)), (60_000, 1000)),
            ((None, None), (5000, 100)),
            ((Some(750), Some(250)), (750, 250)),
        ];
        for ((timeout_ms, interval_ms), (timeout, interval)) in cases {
            let wait = WaitLoop::new(&params(timeout_ms, interval_ms));
            assert_eq!(
                (wait.timeout(), wait.interval),
                (
                    Duration::from_millis(timeout),
                    Duration::from_millis(interval)
                ),
                "{timeout_ms:?} / {interval_ms:?}"
            );
        }
    }

    #[test]
    fn wait_for_never_sleeps_past_the_timeout() {
        let wait = WaitLoop::new(&params(Some(250), Some(100)));
        for at in [0, 100, 160, 200, 230] {
            let elapsed = Duration::from_millis(at);
            let sleep = again(&wait.step(false, Value::Null, spent(&wait, at, at)));
            assert!(
                sleep <= Duration::from_millis(250).saturating_sub(elapsed),
                "at {at} ms it sleeps {sleep:?}"
            );
            assert!(sleep > Duration::ZERO, "at {at} ms it sleeps at all");
        }
        assert_eq!(
            again(&wait.step(false, Value::Null, spent(&wait, 200, 200))),
            Duration::from_millis(50),
            "the last sleep ends at the timeout"
        );
        assert_eq!(
            again(&wait.step(false, Value::Null, spent(&wait, 249, 249))),
            Duration::from_millis(20),
            "under 20 ms left it sleeps the least interval"
        );
    }

    #[test]
    fn a_state_predicate_tests_equals_or_exists() {
        assert_eq!(
            parse_predicate(&json!({"state": "overlay", "equals": "settings"})),
            Ok(Predicate::State {
                path: "overlay".to_owned(),
                test: StateTest::Equals(json!("settings")),
            })
        );
        assert_eq!(
            parse_predicate(&json!({"state": "tabs.0.id", "exists": false})),
            Ok(Predicate::State {
                path: "tabs.0.id".to_owned(),
                test: StateTest::Exists(false),
            })
        );
    }

    #[test]
    fn a_pane_text_predicate_names_the_pane_and_the_text() {
        assert_eq!(
            parse_predicate(&json!({"pane_text": {"pane": "p1", "contains": "hi"}})),
            Ok(Predicate::PaneText {
                pane: "p1".to_owned(),
                contains: "hi".to_owned(),
            })
        );
    }

    #[test]
    fn an_element_predicate_names_the_element_and_its_visibility() {
        assert_eq!(
            parse_predicate(&json!({"element": "sidebar-divider", "visible": true})),
            Ok(Predicate::Element {
                name: "sidebar-divider".to_owned(),
                visible: true,
            })
        );
    }

    #[test]
    fn a_predicate_with_an_unknown_key_is_refused() {
        let refused = parse_predicate(&json!({"state": "overlay", "equals": 1, "colour": 2}))
            .expect_err("an unknown key is refused");
        assert!(refused.contains("colour"), "{refused}");
        let none = parse_predicate(&json!({"colour": 2})).expect_err("no predicate");
        assert!(none.contains("colour"), "{none}");
    }

    #[test]
    fn a_state_predicate_with_equals_and_exists_is_refused() {
        let refused = parse_predicate(&json!({"state": "overlay", "equals": 1, "exists": true}))
            .expect_err("both tests are refused");
        assert_eq!(refused, "give equals or exists, not both");
    }

    #[test]
    fn a_pane_text_predicate_without_contains_is_refused() {
        let refused = parse_predicate(&json!({"pane_text": {"pane": "p1"}}))
            .expect_err("contains is required");
        assert!(refused.contains("contains"), "{refused}");
    }
}
