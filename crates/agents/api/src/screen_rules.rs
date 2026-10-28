//! The screen rules a plugin hands the daemon: one table of rules over the
//! regions a rendered terminal screen is read by, evaluated by priority to
//! decide whether an agent session is working, waits for an answer or is
//! idle.
//!
//! The rules are data, so a plugin carries its own table and a rule file can
//! later load one. Compiling the regex patterns and reading a screen stay in
//! the daemon: `regex` and the terminal grid are its dependencies, never a
//! plugin's, so the patterns travel as strings.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The most rules one table may hold.
pub const MAX_RULES: usize = 128;

/// The most characters one matcher or guard pattern may hold.
pub const MAX_PATTERN_CHARS: usize = 512;

/// The most guards one rule may carry.
pub const MAX_GUARDS: usize = 8;

/// The screen rules of one agent, highest priority first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenRules {
    /// The rules, in descending priority order.
    #[serde(default)]
    pub rules: Vec<ScreenRule>,
}

/// One problem with one rule of a table, found before the table is used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleError {
    /// The rule's name, or `#<index>` when it has none.
    pub rule: String,
    /// What is wrong with it.
    pub problem: String,
}

impl RuleError {
    /// A problem with `rule`, the table's rule at `index`.
    #[must_use]
    pub fn new(index: usize, rule: &ScreenRule, problem: impl Into<String>) -> Self {
        let rule = if rule.name.is_empty() {
            format!("#{index}")
        } else {
            rule.name.clone()
        };
        Self {
            rule,
            problem: problem.into(),
        }
    }
}

impl std::fmt::Display for RuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "screen rule {:?}: {}", self.rule, self.problem)
    }
}

impl ScreenRules {
    /// Checks the table's structure: every rule named once, priorities that
    /// never rise, no part this build does not know, no empty pattern (it
    /// would match every row), and the caps on rules, pattern length and
    /// guards.
    ///
    /// # Errors
    ///
    /// Every problem found, in table order.
    pub fn validate(&self) -> Result<(), Vec<RuleError>> {
        let mut errors = Vec::new();
        let mut names: Vec<&str> = Vec::new();
        let mut last_priority: Option<u16> = None;
        for (index, rule) in self.rules.iter().enumerate() {
            let mut fail = |problem: String| errors.push(RuleError::new(index, rule, problem));
            if index == MAX_RULES {
                fail(format!(
                    "the table holds {} rules; at most {MAX_RULES} are allowed",
                    self.rules.len()
                ));
            }
            if rule.name.is_empty() {
                fail("the rule has no name".to_owned());
            } else if names.contains(&rule.name.as_str()) {
                fail("the name is used twice".to_owned());
            }
            names.push(&rule.name);
            if let Some(previous) = last_priority
                && rule.priority > previous
            {
                fail(format!(
                    "priority {} follows priority {previous}; priorities must not rise",
                    rule.priority
                ));
            }
            last_priority = Some(rule.priority);
            for problem in part_problems(rule) {
                fail(problem);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// The problems with one rule's region, verdict, matcher and guards.
fn part_problems(rule: &ScreenRule) -> Vec<String> {
    let mut problems = Vec::new();
    if rule.region == ScreenRegion::Unknown {
        problems.push("it reads a region this build does not know".to_owned());
    }
    if rule.verdict == ScreenVerdict::Unknown {
        problems.push("it gives a verdict this build does not know".to_owned());
    }
    if rule.not.len() > MAX_GUARDS {
        problems.push(format!(
            "it has {} guards; at most {MAX_GUARDS} are allowed",
            rule.not.len()
        ));
    }
    problems.extend(pattern_problem("its matcher", &rule.matcher));
    for (index, guard) in rule.not.iter().enumerate() {
        problems.extend(pattern_problem(&format!("guard {index}"), guard));
    }
    problems
}

/// What is wrong with one matcher, named `what` in the message, if anything.
fn pattern_problem(what: &str, matcher: &LineMatcher) -> Option<String> {
    let pattern = match matcher {
        LineMatcher::Contains(pattern) | LineMatcher::Regex(pattern) => pattern,
        LineMatcher::Unknown => {
            return Some(format!("{what} is a kind this build does not know"));
        }
    };
    let chars = pattern.chars().count();
    if chars == 0 {
        Some(format!("{what} is empty and would match every row"))
    } else if chars > MAX_PATTERN_CHARS {
        Some(format!(
            "{what} is {chars} characters; at most {MAX_PATTERN_CHARS} are allowed"
        ))
    } else {
        None
    }
}

/// One rule: where to look, what for, what to ignore, and what a hit means.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenRule {
    /// The rule's name, unique within its table and quoted in the daemon's
    /// log line for the reading it decided.
    pub name: String,
    /// Higher wins. The table is stored in descending order, and evaluation
    /// takes the first rule that matches.
    pub priority: u16,
    /// The part of the screen the matcher reads.
    pub region: ScreenRegion,
    /// What the rule looks for in its region.
    pub matcher: LineMatcher,
    /// Matchers that veto the rule when any of them hits the same region.
    #[serde(default)]
    pub not: Vec<LineMatcher>,
    /// What a hit means.
    pub verdict: ScreenVerdict,
}

/// The part of the screen a rule reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScreenRegion {
    /// The last `rows` non-empty rows.
    BottomNonEmpty {
        /// How many rows to read.
        rows: usize,
    },
    /// The rows between the prompt box's two rules.
    PromptBoxBody,
    /// The last `rows` non-empty rows above the prompt box.
    AbovePromptBox {
        /// How many rows to read.
        rows: usize,
    },
    /// The last title the stream set with OSC 0/2.
    Title,
    /// Every row below the last horizontal rule row (the footer under the
    /// prompt box), trailing empty rows dropped.
    AfterLastRule,
    /// A region this build does not know, carried from a newer plugin.
    #[serde(other)]
    Unknown,
}

/// How a rule looks for a hit in its region.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "pattern", rename_all = "snake_case")]
pub enum LineMatcher {
    /// A literal substring of a row.
    Contains(String),
    /// A regex matched against each row.
    Regex(String),
    /// A matcher this build does not know, carried from a newer plugin.
    Unknown,
}

/// The fields of a [`LineMatcher`] in its tagged JSON form; `pattern` is
/// whatever a newer kind carries.
#[derive(Deserialize)]
struct MatcherFields {
    kind: String,
    #[serde(default)]
    pattern: Option<serde_json::Value>,
}

/// An unknown `kind` decodes as [`LineMatcher::Unknown`] whatever its
/// `pattern` holds: serde's own `#[serde(other)]` on an adjacently tagged enum
/// refuses a content field it does not have a variant for, which would fail
/// the whole rule set a newer plugin sends.
impl<'de> Deserialize<'de> for LineMatcher {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let fields = MatcherFields::deserialize(deserializer)?;
        Ok(match (fields.kind.as_str(), fields.pattern) {
            ("contains", Some(serde_json::Value::String(pattern))) => Self::Contains(pattern),
            ("regex", Some(serde_json::Value::String(pattern))) => Self::Regex(pattern),
            _ => Self::Unknown,
        })
    }
}

/// What a rule says about the session's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", from = "String", into = "String")]
pub enum ScreenVerdict {
    /// A turn is in progress.
    Working,
    /// A dialog or a prompt waits for an answer.
    AwaitingInput,
    /// Nothing is running.
    Idle,
    /// An overlay is open: leave the status as it is.
    Keep,
    /// Work runs on in the background beside the session's state. Such a
    /// rule is a second reading of the screen, evaluated apart from the
    /// first-match state rules, and never decides the state.
    Background,
    /// A verdict this build does not know, carried from a newer plugin.
    Unknown,
}

impl From<String> for ScreenVerdict {
    fn from(raw: String) -> Self {
        match raw.as_str() {
            "working" => Self::Working,
            "awaiting_input" => Self::AwaitingInput,
            "idle" => Self::Idle,
            "keep" => Self::Keep,
            "background" => Self::Background,
            _ => Self::Unknown,
        }
    }
}

impl From<ScreenVerdict> for String {
    fn from(verdict: ScreenVerdict) -> Self {
        match verdict {
            ScreenVerdict::Working => "working".to_string(),
            ScreenVerdict::AwaitingInput => "awaiting_input".to_string(),
            ScreenVerdict::Idle => "idle".to_string(),
            ScreenVerdict::Keep => "keep".to_string(),
            ScreenVerdict::Background => "background".to_string(),
            ScreenVerdict::Unknown => "unknown".to_string(),
        }
    }
}

/// How much output the daemon must see, and how long it must then stay
/// quiet, before an agent's TUI counts as up and a prompt is typed into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadySignal {
    /// Cumulative PTY output bytes the stream must have printed first.
    pub min_output_bytes: usize,
    /// Then this long without output: the banner is painted and the keystroke
    /// handler is bound.
    pub quiet_for: Duration,
}

impl Default for ReadySignal {
    /// A banner of about 1 KB printed, then 1.5 s of silence.
    fn default() -> Self {
        Self {
            min_output_bytes: 1024,
            quiet_for: Duration::from_millis(1500),
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{
        LineMatcher, MAX_GUARDS, MAX_PATTERN_CHARS, MAX_RULES, ReadySignal, ScreenRegion,
        ScreenRule, ScreenRules, ScreenVerdict,
    };
    use crate::test_agent::TestAgent;
    use std::time::Duration;

    fn rule(
        name: &str,
        priority: u16,
        region: ScreenRegion,
        matcher: LineMatcher,
        verdict: ScreenVerdict,
    ) -> ScreenRule {
        ScreenRule {
            name: name.to_string(),
            priority,
            region,
            matcher,
            not: Vec::new(),
            verdict,
        }
    }

    #[test]
    fn screen_rules_round_trip_through_json() {
        let rules = ScreenRules {
            rules: vec![
                rule(
                    "dialog",
                    900,
                    ScreenRegion::BottomNonEmpty { rows: 24 },
                    LineMatcher::Contains("Esc to cancel".to_string()),
                    ScreenVerdict::AwaitingInput,
                ),
                ScreenRule {
                    not: vec![LineMatcher::Regex(r"^\s*$".to_string())],
                    ..rule(
                        "spinner",
                        800,
                        ScreenRegion::AbovePromptBox { rows: 12 },
                        LineMatcher::Regex(r"^[·] ".to_string()),
                        ScreenVerdict::Working,
                    )
                },
                rule(
                    "box",
                    500,
                    ScreenRegion::PromptBoxBody,
                    LineMatcher::Contains("❯".to_string()),
                    ScreenVerdict::Idle,
                ),
                rule(
                    "footer",
                    300,
                    ScreenRegion::AfterLastRule,
                    LineMatcher::Contains("1 shell".to_string()),
                    ScreenVerdict::Idle,
                ),
                rule(
                    "title",
                    100,
                    ScreenRegion::Title,
                    LineMatcher::Contains("✳".to_string()),
                    ScreenVerdict::Keep,
                ),
            ],
        };
        assert!(
            json_has_footer_kind(&rules),
            "the footer region serializes as after_last_rule"
        );
        let json = serde_json::to_string(&rules).expect("the rules serialize");
        let back: ScreenRules = serde_json::from_str(&json).expect("the rules decode");
        assert_eq!(back, rules);
        let empty: ScreenRules = serde_json::from_str("{}").expect("an absent rule list decodes");
        assert_eq!(empty, ScreenRules::default());
    }

    fn json_has_footer_kind(rules: &ScreenRules) -> bool {
        serde_json::to_string(rules)
            .expect("the rules serialize")
            .contains(r#""region":{"kind":"after_last_rule"}"#)
    }

    /// A rule that reads `BUSY` in the last four rows as working.
    fn busy(name: &str, priority: u16) -> ScreenRule {
        rule(
            name,
            priority,
            ScreenRegion::BottomNonEmpty { rows: 4 },
            LineMatcher::Contains("BUSY".to_string()),
            ScreenVerdict::Working,
        )
    }

    /// The problems `validate` reports for `rules`, each as its display text.
    fn problems(rules: Vec<ScreenRule>) -> Vec<String> {
        ScreenRules { rules }
            .validate()
            .expect_err("the table is refused")
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn validate_rejects_an_empty_matcher() {
        let empty_contains = ScreenRule {
            matcher: LineMatcher::Contains(String::new()),
            ..busy("empty_contains", 900)
        };
        let empty_regex = ScreenRule {
            matcher: LineMatcher::Regex(String::new()),
            ..busy("empty_regex", 800)
        };
        let empty_guard = ScreenRule {
            not: vec![
                LineMatcher::Contains("x".to_string()),
                LineMatcher::Regex(String::new()),
            ],
            ..busy("empty_guard", 700)
        };
        assert_eq!(
            problems(vec![
                empty_contains,
                empty_regex,
                empty_guard,
                busy("fine", 100)
            ]),
            [
                r#"screen rule "empty_contains": its matcher is empty and would match every row"#,
                r#"screen rule "empty_regex": its matcher is empty and would match every row"#,
                r#"screen rule "empty_guard": guard 1 is empty and would match every row"#,
            ]
        );
    }

    #[test]
    fn validate_rejects_a_duplicate_name() {
        assert_eq!(
            problems(vec![busy("twice", 500), busy("twice", 100)]),
            [r#"screen rule "twice": the name is used twice"#]
        );
        assert_eq!(
            problems(vec![busy("", 500)]),
            [r##"screen rule "#0": the rule has no name"##]
        );
    }

    #[test]
    fn validate_rejects_a_rising_priority() {
        assert_eq!(
            problems(vec![busy("low", 100), busy("high", 500)]),
            [r#"screen rule "high": priority 500 follows priority 100; priorities must not rise"#]
        );
    }

    #[test]
    fn validate_rejects_too_many_rules() {
        let at_cap: Vec<ScreenRule> = (0..MAX_RULES)
            .map(|i| busy(&format!("r{i}"), 100))
            .collect();
        assert_eq!(
            ScreenRules {
                rules: at_cap.clone()
            }
            .validate(),
            Ok(())
        );
        let mut over = at_cap;
        over.push(busy("one_too_many", 100));
        assert_eq!(
            problems(over),
            [format!(
                r#"screen rule "one_too_many": the table holds {} rules; at most {MAX_RULES} are allowed"#,
                MAX_RULES + 1
            )]
        );
    }

    #[test]
    fn validate_rejects_an_overlong_matcher() {
        let at_cap = ScreenRule {
            matcher: LineMatcher::Regex("a".repeat(MAX_PATTERN_CHARS)),
            ..busy("at_cap", 900)
        };
        assert_eq!(
            ScreenRules {
                rules: vec![at_cap]
            }
            .validate(),
            Ok(())
        );
        let long_matcher = ScreenRule {
            matcher: LineMatcher::Contains("é".repeat(MAX_PATTERN_CHARS + 1)),
            ..busy("long_matcher", 900)
        };
        let long_guard = ScreenRule {
            not: vec![LineMatcher::Regex("a".repeat(MAX_PATTERN_CHARS + 1))],
            ..busy("long_guard", 800)
        };
        assert_eq!(
            problems(vec![long_matcher, long_guard]),
            [
                format!(
                    r#"screen rule "long_matcher": its matcher is {} characters; at most {MAX_PATTERN_CHARS} are allowed"#,
                    MAX_PATTERN_CHARS + 1
                ),
                format!(
                    r#"screen rule "long_guard": guard 0 is {} characters; at most {MAX_PATTERN_CHARS} are allowed"#,
                    MAX_PATTERN_CHARS + 1
                ),
            ]
        );
    }

    #[test]
    fn validate_rejects_too_many_guards() {
        let guards = |n: usize| {
            (0..n)
                .map(|i| LineMatcher::Contains(format!("g{i}")))
                .collect()
        };
        let at_cap = ScreenRule {
            not: guards(MAX_GUARDS),
            ..busy("at_cap", 900)
        };
        assert_eq!(
            ScreenRules {
                rules: vec![at_cap]
            }
            .validate(),
            Ok(())
        );
        let over = ScreenRule {
            not: guards(MAX_GUARDS + 1),
            ..busy("over", 900)
        };
        assert_eq!(
            problems(vec![over]),
            [format!(
                r#"screen rule "over": it has {} guards; at most {MAX_GUARDS} are allowed"#,
                MAX_GUARDS + 1
            )]
        );
    }

    #[test]
    fn an_unknown_matcher_kind_decodes_as_unknown() {
        let matcher: LineMatcher =
            serde_json::from_str(r#"{"kind": "all_of", "pattern": ["a", "b"]}"#)
                .expect("an unknown matcher decodes");
        assert_eq!(matcher, LineMatcher::Unknown);
    }

    #[test]
    fn an_unknown_region_and_verdict_decode_as_unknown() {
        let region: ScreenRegion =
            serde_json::from_str(r#"{"kind": "sidebar"}"#).expect("an unknown region decodes");
        assert_eq!(region, ScreenRegion::Unknown);
        let verdict: ScreenVerdict =
            serde_json::from_str(r#""paused""#).expect("an unknown verdict decodes");
        assert_eq!(verdict, ScreenVerdict::Unknown);
    }

    #[test]
    fn a_plugin_without_rules_has_an_empty_rule_set() {
        let plugin: &dyn crate::AgentPlugin = &TestAgent::default();
        assert_eq!(plugin.screen_rules().rules, [] as [ScreenRule; 0]);
    }

    #[test]
    fn a_plugin_without_a_chat_reader_reads_nothing_and_answers_nothing() {
        let plugin: &dyn crate::AgentPlugin = &TestAgent::default();
        let rows = vec!["❯".to_string()];
        assert_eq!(plugin.read_chat_screen(&rows), None);
        let prompt = protocol::ChatPrompt {
            id: String::new(),
            kind: protocol::ChatPromptKind::Permission,
            subject: "notes.txt".to_string(),
            body: String::new(),
            options: Vec::new(),
            questions: Vec::new(),
            can_amend: false,
            always_allow_tip: None,
            multi: false,
            cursor: None,
        };
        let answer = protocol::ChatAnswerKind::Option { number: 1 };
        assert_eq!(
            plugin.answer_recipe(&prompt, &answer),
            Err(crate::AnswerRefusal::Unsupported)
        );
    }

    #[test]
    fn the_default_file_reference_is_the_bare_path() {
        let plugin: &dyn crate::AgentPlugin = &TestAgent::default();
        assert_eq!(plugin.file_reference("prompts/a.md"), "prompts/a.md");
    }

    #[test]
    fn the_default_ready_signal_is_the_banner_tuning() {
        let plugin: &dyn crate::AgentPlugin = &TestAgent::default();
        let signal = plugin.ready_signal();
        assert_eq!(signal, ReadySignal::default());
        assert_eq!(signal.min_output_bytes, 1024);
        assert_eq!(signal.quiet_for, Duration::from_millis(1500));
    }
}
