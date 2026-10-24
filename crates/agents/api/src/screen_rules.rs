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

/// The screen rules of one agent, highest priority first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenRules {
    /// The rules, in descending priority order.
    #[serde(default)]
    pub rules: Vec<ScreenRule>,
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
    /// Claude's banner tuning: about 1 KB printed, then 1.5 s of silence.
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
    use super::{LineMatcher, ReadySignal, ScreenRegion, ScreenRule, ScreenRules, ScreenVerdict};
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
                    "title",
                    100,
                    ScreenRegion::Title,
                    LineMatcher::Contains("✳".to_string()),
                    ScreenVerdict::Keep,
                ),
            ],
        };
        let json = serde_json::to_string(&rules).expect("the rules serialize");
        let back: ScreenRules = serde_json::from_str(&json).expect("the rules decode");
        assert_eq!(back, rules);
        let empty: ScreenRules = serde_json::from_str("{}").expect("an absent rule list decodes");
        assert_eq!(empty, ScreenRules::default());
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
