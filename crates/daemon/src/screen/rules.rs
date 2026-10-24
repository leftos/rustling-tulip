//! The evaluator of an agent's screen rules: the state one rendered session
//! shows.
//!
//! A plugin hands the daemon its rule table as data
//! ([`agent_api::ScreenRules`]); [`CompiledRules::compile`] compiles its
//! patterns once, and [`CompiledRules::read`] evaluates the table over the
//! regions [`Screen`] reads back, by priority: the highest-priority rule whose
//! matcher hits its region, and none of whose guards does, decides the
//! reading. An overlay rule keeps whatever state the session already has,
//! since the overlay is drawn over a turn that may be running or done. An
//! agent session's status watcher ([`crate::pty_state::watch`]) and the
//! plain-shell watcher ([`crate::pty_state::watch_plain_shell`]) read its
//! screen through the rules of the session's agent ([`rules_for`]).

use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use agent_api::{LineMatcher, ScreenRegion, ScreenRule, ScreenRules, ScreenVerdict};
use protocol::Agent;
use regex::Regex;

use super::Screen;

/// The state a session's screen shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenState {
    /// A turn is in progress.
    Working,
    /// A dialog or a prompt waits for an answer.
    AwaitingInput,
    /// Nothing is running.
    Idle,
}

/// What a rule says about the session's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The screen shows this state.
    State(ScreenState),
    /// An overlay is open: leave the status as it is.
    Keep,
    /// No rule matched.
    NoMatch,
}

/// One evaluation: the verdict, and the rule that gave it (`None` when no rule
/// matched).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub verdict: Verdict,
    pub rule: Option<Arc<str>>,
    /// Whether the winning rule read the OSC title rather than the screen
    /// body, so a caller can refuse a state known only from the title.
    pub from_title: bool,
}

/// One agent's rule table with its patterns compiled, highest priority first.
pub struct CompiledRules {
    rules: Vec<CompiledRule>,
}

/// One rule with its matchers compiled.
struct CompiledRule {
    name: Arc<str>,
    priority: u16,
    region: ScreenRegion,
    matcher: CompiledMatcher,
    not: Vec<CompiledMatcher>,
    verdict: Verdict,
}

/// One matcher, compiled.
enum CompiledMatcher {
    Contains(String),
    Line(Regex),
}

impl CompiledRules {
    /// Compiles `rules`, sorted by descending priority (rules of one priority
    /// keep their table order). A rule whose pattern does not compile is left
    /// out with an error, and one with a region, a matcher, a guard or a
    /// verdict this build does not know is left out with a warning.
    #[must_use]
    pub fn compile(rules: &ScreenRules) -> Self {
        let mut compiled: Vec<CompiledRule> = rules.rules.iter().filter_map(compile_rule).collect();
        compiled.sort_by_key(|rule| Reverse(rule.priority));
        Self { rules: compiled }
    }

    /// The state the screen shows.
    #[must_use]
    pub fn read(&self, screen: &Screen) -> Reading {
        let mut regions = RegionLines::default();
        for rule in &self.rules {
            if rule.matches(regions.get(screen, rule.region)) {
                return Reading {
                    verdict: rule.verdict,
                    rule: Some(Arc::clone(&rule.name)),
                    from_title: rule.region == ScreenRegion::Title,
                };
            }
        }
        Reading {
            verdict: Verdict::NoMatch,
            rule: None,
            from_title: false,
        }
    }
}

/// Each agent's compiled rules, compiled on first use.
static BY_AGENT: LazyLock<Mutex<HashMap<Agent, Arc<CompiledRules>>>> =
    LazyLock::new(Mutex::default);

/// The compiled screen rules of `agent`'s plugin, compiled once per agent.
#[must_use]
pub fn rules_for(agent: Agent) -> Arc<CompiledRules> {
    let mut cache = crate::sync::lock(&BY_AGENT);
    let rules = cache.entry(agent).or_insert_with(|| {
        Arc::new(CompiledRules::compile(
            agent_registry::plugin(agent).screen_rules(),
        ))
    });
    Arc::clone(rules)
}

/// A shell's prompt row, each pattern with whether it is the POSIX form,
/// which a row holding one of Claude's box glyphs never matches: PowerShell's
/// `PS <path>>`, cmd's `<drive>:\<path>>`, and a POSIX shell's row ending in
/// `$`, `#` or `%`.
const SHELL_PROMPTS: [(&str, bool); 3] = [
    (r"^PS [^>]*>$", false),
    (r"^[A-Za-z]:\\[^>]*>$", false),
    (r"^\S.*[$#%]$", true),
];

/// [`SHELL_PROMPTS`] compiled once; a pattern that does not compile is left
/// out.
static COMPILED_PROMPTS: LazyLock<Vec<(Regex, bool)>> = LazyLock::new(|| {
    SHELL_PROMPTS
        .iter()
        .filter_map(|&(pattern, posix)| match Regex::new(pattern) {
            Ok(regex) => Some((regex, posix)),
            Err(error) => {
                tracing::error!(
                    pattern = pattern,
                    %error,
                    "screen rules: a shell prompt pattern did not compile; it is skipped"
                );
                None
            }
        })
        .collect()
});

/// Whether the last non-empty row of the grid is a shell's prompt: the shell
/// is back at its own prompt, whatever an ended program left painted above.
#[must_use]
pub fn last_row_is_shell_prompt(screen: &Screen) -> bool {
    screen
        .bottom_non_empty_lines(1)
        .first()
        .is_some_and(|row| is_shell_prompt(row.trim_end()))
}

fn is_shell_prompt(row: &str) -> bool {
    let box_glyph = row.contains(['│', '─', '❯']);
    COMPILED_PROMPTS
        .iter()
        .any(|(regex, posix)| !(*posix && box_glyph) && regex.is_match(row))
}

/// The regions read so far, so each one is read once per evaluation.
#[derive(Default)]
struct RegionLines {
    read: Vec<(ScreenRegion, Vec<String>)>,
}

impl RegionLines {
    /// The lines of `region`, read now unless this evaluation has already read
    /// them.
    fn get(&mut self, screen: &Screen, region: ScreenRegion) -> &[String] {
        let index = if let Some(index) = self.read.iter().position(|(key, _)| *key == region) {
            index
        } else {
            self.read.push((region, region_lines(screen, region)));
            self.read.len() - 1
        };
        &self.read[index].1
    }
}

/// Compiles one rule's matchers, or gives `None` when one of its patterns does
/// not compile or one of its parts is unknown to this build.
fn compile_rule(rule: &ScreenRule) -> Option<CompiledRule> {
    let verdict = verdict_of(rule)?;
    if rule.region == ScreenRegion::Unknown {
        tracing::warn!(
            rule = %rule.name,
            "screen rules: the rule reads a region this build does not know; it is skipped"
        );
        return None;
    }
    let matcher = compile_matcher(&rule.name, &rule.matcher)?;
    let mut not = Vec::with_capacity(rule.not.len());
    for guard in &rule.not {
        not.push(compile_matcher(&rule.name, guard)?);
    }
    Some(CompiledRule {
        name: Arc::from(rule.name.as_str()),
        priority: rule.priority,
        region: rule.region,
        matcher,
        not,
        verdict,
    })
}

/// The verdict a rule gives, or `None`, logged, when this build does not know
/// it.
fn verdict_of(rule: &ScreenRule) -> Option<Verdict> {
    match rule.verdict {
        ScreenVerdict::Working => Some(Verdict::State(ScreenState::Working)),
        ScreenVerdict::AwaitingInput => Some(Verdict::State(ScreenState::AwaitingInput)),
        ScreenVerdict::Idle => Some(Verdict::State(ScreenState::Idle)),
        ScreenVerdict::Keep => Some(Verdict::Keep),
        ScreenVerdict::Unknown => {
            tracing::warn!(
                rule = %rule.name,
                "screen rules: the rule gives a verdict this build does not know; it is skipped"
            );
            None
        }
    }
}

/// Compiles one matcher, logging the rule that named a pattern which did not
/// compile or a matcher this build does not know.
fn compile_matcher(rule: &str, matcher: &LineMatcher) -> Option<CompiledMatcher> {
    match matcher {
        LineMatcher::Contains(needle) => Some(CompiledMatcher::Contains(needle.clone())),
        LineMatcher::Regex(pattern) => match Regex::new(pattern) {
            Ok(regex) => Some(CompiledMatcher::Line(regex)),
            Err(error) => {
                tracing::error!(
                    rule = %rule,
                    pattern = %pattern,
                    %error,
                    "screen rules: a pattern did not compile; the rule is skipped"
                );
                None
            }
        },
        LineMatcher::Unknown => {
            tracing::warn!(
                rule = %rule,
                "screen rules: the rule uses a matcher this build does not know; it is skipped"
            );
            None
        }
    }
}

impl CompiledRule {
    /// Whether the rule fires over `lines`: its matcher hits one of them and
    /// none of its guards does.
    fn matches(&self, lines: &[String]) -> bool {
        self.matcher.hits(lines) && !self.not.iter().any(|guard| guard.hits(lines))
    }
}

impl CompiledMatcher {
    /// Whether the matcher hits any of `lines`.
    fn hits(&self, lines: &[String]) -> bool {
        lines.iter().any(|line| match self {
            Self::Contains(needle) => line.contains(needle.as_str()),
            Self::Line(regex) => regex.is_match(line),
        })
    }
}

/// The lines of one region; a region this build does not know has none (a
/// compiled rule never reads one: [`compile_rule`] drops it).
fn region_lines(screen: &Screen, region: ScreenRegion) -> Vec<String> {
    match region {
        ScreenRegion::BottomNonEmpty { rows } => screen.bottom_non_empty_lines(rows),
        ScreenRegion::PromptBoxBody => screen.prompt_box_body(),
        ScreenRegion::AbovePromptBox { rows } => screen.above_prompt_box(rows),
        ScreenRegion::Title => screen.title().into_iter().collect(),
        ScreenRegion::Unknown => Vec::new(),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    /// The CSI sequences a capture carries, for measuring a row's visible
    /// width without them.
    static CSI: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").expect("the CSI pattern compiles"));

    /// One capture as a screen: its lines are the rows, in a grid wide enough
    /// for the widest of them.
    fn screen_from(text: &str) -> Screen {
        let lines: Vec<&str> = text.lines().collect();
        let widest = lines
            .iter()
            .map(|line| visible_width(line))
            .max()
            .unwrap_or(0);
        let columns = u16::try_from(widest + 4).expect("a capture fits a grid");
        let rows = u16::try_from(lines.len() + 2).expect("a capture fits a grid");
        let mut screen = Screen::new(columns, rows);
        screen.feed(lines.join("\r\n").as_bytes());
        screen
    }

    /// How many cells a line shows, its escape sequences aside.
    fn visible_width(line: &str) -> usize {
        CSI.replace_all(line, "").chars().count()
    }

    /// The state the screen shows by Claude's rules.
    fn read_claude(screen: &Screen) -> Reading {
        rules_for(Agent::Claude).read(screen)
    }

    /// Claude's rule table as its plugin hands it over.
    fn claude_table() -> &'static ScreenRules {
        agent_registry::plugin(Agent::Claude).screen_rules()
    }

    /// The regex Claude's compiled rules hold for the line rule `name`.
    fn line_rule(name: &str) -> Regex {
        rules_for(Agent::Claude)
            .rules
            .iter()
            .find(|rule| &*rule.name == name)
            .and_then(|rule| match &rule.matcher {
                CompiledMatcher::Line(regex) => Some(regex.clone()),
                CompiledMatcher::Contains(_) => None,
            })
            .expect("a compiled line rule")
    }

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

    /// A rule that reads `BUSY` anywhere in the last four rows as working.
    fn busy_rule(name: &str, priority: u16) -> ScreenRule {
        rule(
            name,
            priority,
            ScreenRegion::BottomNonEmpty { rows: 4 },
            LineMatcher::Contains("BUSY".to_string()),
            ScreenVerdict::Working,
        )
    }

    #[test]
    fn claude_rules_all_compile() {
        let compiled = CompiledRules::compile(claude_table());
        assert_eq!(compiled.rules.len(), 16, "no Claude rule was dropped");
    }

    #[test]
    fn a_rule_whose_pattern_does_not_compile_is_skipped() {
        let rules = ScreenRules {
            rules: vec![
                rule(
                    "broken",
                    900,
                    ScreenRegion::BottomNonEmpty { rows: 4 },
                    LineMatcher::Regex("(unclosed".to_string()),
                    ScreenVerdict::AwaitingInput,
                ),
                busy_rule("busy", 100),
            ],
        };
        let compiled = CompiledRules::compile(&rules);
        assert_eq!(compiled.rules.len(), 1, "only the good rule is kept");
        let reading = compiled.read(&screen_from("BUSY"));
        assert_eq!(reading.verdict, Verdict::State(ScreenState::Working));
        assert_eq!(reading.rule.as_deref(), Some("busy"));
    }

    #[test]
    fn an_unknown_matcher_rule_is_skipped() {
        let unknown_guard = ScreenRule {
            not: vec![LineMatcher::Unknown],
            ..busy_rule("unknown_guard", 800)
        };
        let unknown_region = ScreenRule {
            region: ScreenRegion::Unknown,
            ..busy_rule("unknown_region", 700)
        };
        let unknown_verdict = ScreenRule {
            verdict: ScreenVerdict::Unknown,
            ..busy_rule("unknown_verdict", 600)
        };
        let rules = ScreenRules {
            rules: vec![
                rule(
                    "unknown_matcher",
                    900,
                    ScreenRegion::BottomNonEmpty { rows: 4 },
                    LineMatcher::Unknown,
                    ScreenVerdict::AwaitingInput,
                ),
                unknown_guard,
                unknown_region,
                unknown_verdict,
                busy_rule("busy", 100),
            ],
        };
        let compiled = CompiledRules::compile(&rules);
        assert_eq!(
            compiled.rules.len(),
            1,
            "every rule with an unknown part is dropped"
        );
        let reading = compiled.read(&screen_from("BUSY"));
        assert_eq!(reading.rule.as_deref(), Some("busy"));
    }

    #[test]
    fn rule_order_follows_priority_not_input_order() {
        let idle_first = ScreenRule {
            verdict: ScreenVerdict::Idle,
            ..busy_rule("low", 100)
        };
        let rules = ScreenRules {
            rules: vec![idle_first, busy_rule("high", 500)],
        };
        let reading = CompiledRules::compile(&rules).read(&screen_from("BUSY"));
        assert_eq!(reading.verdict, Verdict::State(ScreenState::Working));
        assert_eq!(reading.rule.as_deref(), Some("high"));
    }

    #[test]
    fn a_plugin_without_rules_reads_no_match() {
        let compiled = CompiledRules::compile(&ScreenRules::default());
        let reading = compiled.read(&screen_from(include_str!(
            "fixtures/v71-agents-running.txt"
        )));
        assert_eq!(
            reading,
            Reading {
                verdict: Verdict::NoMatch,
                rule: None,
                from_title: false
            }
        );
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one row per capture; the table is the test"
    )]
    fn every_fixture_reads_its_state() {
        use ScreenState::{AwaitingInput, Idle, Working};
        use Verdict::{Keep, State};
        let cases: [(&str, &str, Verdict); 37] = [
            (
                "v03-ready",
                include_str!("fixtures/v03-ready.txt"),
                State(Idle),
            ),
            (
                "v11-turn1-end",
                include_str!("fixtures/v11-turn1-end.txt"),
                State(Idle),
            ),
            (
                "c11-turn1-end",
                include_str!("fixtures/c11-turn1-end.txt"),
                State(Idle),
            ),
            (
                "v26-deny",
                include_str!("fixtures/v26-deny.txt"),
                State(Idle),
            ),
            ("c24-esc", include_str!("fixtures/c24-esc.txt"), State(Idle)),
            (
                "c130-ask-esc",
                include_str!("fixtures/c130-ask-esc.txt"),
                State(Idle),
            ),
            (
                "p98-compacted",
                include_str!("fixtures/p98-compacted.txt"),
                State(Idle),
            ),
            (
                "claude--fresh-idle",
                include_str!("fixtures/claude--fresh-idle.txt"),
                State(Idle),
            ),
            (
                "claude--done",
                include_str!("fixtures/claude--done.txt"),
                State(Idle),
            ),
            (
                "claude--idle-background-shell",
                include_str!("fixtures/claude--idle-background-shell.txt"),
                State(Idle),
            ),
            (
                "claude--v2301-done-shell-running--w100",
                include_str!("fixtures/claude--v2301-done-shell-running--w100.txt"),
                State(Idle),
            ),
            (
                "v71-agents-running",
                include_str!("fixtures/v71-agents-running.txt"),
                State(Working),
            ),
            (
                "v81-bang-out",
                include_str!("fixtures/v81-bang-out.txt"),
                State(Working),
            ),
            (
                "v54-plan-feedback-sent",
                include_str!("fixtures/v54-plan-feedback-sent.txt"),
                Keep,
            ),
            (
                "c80-bang-out",
                include_str!("fixtures/c80-bang-out.txt"),
                Keep,
            ),
            (
                "claude--working",
                include_str!("fixtures/claude--working.txt"),
                State(Working),
            ),
            (
                "claude--working-esc-to-interrupt",
                include_str!("fixtures/claude--working-esc-to-interrupt.txt"),
                State(Working),
            ),
            (
                "claude--v2301-spinner-no-timer--w100",
                include_str!("fixtures/claude--v2301-spinner-no-timer--w100.txt"),
                State(Working),
            ),
            (
                "claude--v2301-working-quiet--w100",
                include_str!("fixtures/claude--v2301-working-quiet--w100.txt"),
                State(Working),
            ),
            (
                "claude--v2302-working-agents-panel--w119",
                include_str!("fixtures/claude--v2302-working-agents-panel--w119.txt"),
                State(Working),
            ),
            (
                "v01-start",
                include_str!("fixtures/v01-start.txt"),
                State(AwaitingInput),
            ),
            (
                "v21-perm-bash",
                include_str!("fixtures/v21-perm-bash.txt"),
                State(AwaitingInput),
            ),
            (
                "c23-perm-bash",
                include_str!("fixtures/c23-perm-bash.txt"),
                State(AwaitingInput),
            ),
            (
                "c52-edit-perm",
                include_str!("fixtures/c52-edit-perm.txt"),
                State(AwaitingInput),
            ),
            (
                "v31-ask-q1",
                include_str!("fixtures/v31-ask-q1.txt"),
                State(AwaitingInput),
            ),
            (
                "c31-ask-q1",
                include_str!("fixtures/c31-ask-q1.txt"),
                State(AwaitingInput),
            ),
            (
                "v42-ask-review",
                include_str!("fixtures/v42-ask-review.txt"),
                State(AwaitingInput),
            ),
            (
                "v51-plan-dialog",
                include_str!("fixtures/v51-plan-dialog.txt"),
                State(AwaitingInput),
            ),
            (
                "claude--permission-bash",
                include_str!("fixtures/claude--permission-bash.txt"),
                State(AwaitingInput),
            ),
            (
                "claude--trust-prompt",
                include_str!("fixtures/claude--trust-prompt.txt"),
                State(AwaitingInput),
            ),
            ("v95-rewind", include_str!("fixtures/v95-rewind.txt"), Keep),
            ("v96-ctrl-o", include_str!("fixtures/v96-ctrl-o.txt"), Keep),
            (
                "v140-model-picker",
                include_str!("fixtures/v140-model-picker.txt"),
                Keep,
            ),
            ("v141-btw", include_str!("fixtures/v141-btw.txt"), Keep),
            (
                "v152-learn-more",
                include_str!("fixtures/v152-learn-more.txt"),
                Keep,
            ),
            (
                "claude--menu-model-picker",
                include_str!("fixtures/claude--menu-model-picker.txt"),
                Keep,
            ),
            (
                "claude--v2302-turn-ending-stop-hooks--w119",
                include_str!("fixtures/claude--v2302-turn-ending-stop-hooks--w119.txt"),
                Keep,
            ),
        ];
        let mut wrong = Vec::new();
        for (name, text, expected) in cases {
            let reading = read_claude(&screen_from(text));
            if reading.verdict != expected {
                wrong.push(format!(
                    "{name}: wanted {expected:?}, got {:?} by rule {:?}",
                    reading.verdict, reading.rule
                ));
            }
        }
        assert!(
            wrong.is_empty(),
            "captures read the wrong state:\n{}",
            wrong.join("\n")
        );
    }

    #[test]
    fn a_turn_without_a_timer_is_working() {
        let capture = include_str!("fixtures/v71-agents-running.txt");
        assert!(capture.contains("· Cooking… (21s · ↓ 1.1k tokens)"));
        let text = capture.replace("· Cooking… (21s · ↓ 1.1k tokens)", "✶ Cooking…");
        let reading = read_claude(&screen_from(&text));
        assert_eq!(reading.verdict, Verdict::State(ScreenState::Working));
        assert_eq!(reading.rule.as_deref(), Some("turn_spinner"));
        assert!(!reading.from_title);
    }

    #[test]
    fn spinner_examples_and_non_examples() {
        let spinner = line_rule("turn_spinner");
        let spinners = [
            "· Cooking… (21s · ↓ 1.1k tokens)",
            "✽ Crafting… (4s · ↓ 215 tokens · thinking with high effort)",
            "· Onioning… (running PostToolUse hooks… 4/5 · 22s · ↓ 1.0k tokens · thought for 1s)",
            "· Deciphering… · tool use (running Stop hooks… 5/6 · 5s · ↓ 236 tokens · thought for 1s)",
            "✶ Wibbling… ",
            "✢ Puzzling… (3m 3s · ↓ 9.2k tokens)",
            "* Wibbling… (4s · ↓ 304 tokens)",
            "✳ Cooking… (3s · ↓ 120 tokens)",
            "· Cooking… (21s · ↓ 1.1k tokens · thinking with hi…",
            "✻ Brewing… (3s · ↓",
        ];
        for line in spinners {
            assert!(spinner.is_match(line), "read as no spinner: {line:?}");
        }
        let others = [
            "✻ Baked for 22s · done 5:13 PM",
            "● Running 1 shell command…",
            "  Running 1 shell command…",
            "* Loading the config…",
            "❯ Cooking… now",
            "",
        ];
        for line in others {
            assert!(!spinner.is_match(line), "read as a spinner: {line:?}");
        }
    }

    #[test]
    fn hooks_running_examples() {
        let hooks = line_rule("turn_hooks_running");
        let running = [
            "· Onioning… (running PostToolUse hooks… 4/5 · 22s · ↓ 1.0k tokens · thought for 1s)",
            "· Deciphering… · tool use (running Stop hooks… 5/6 · 5s · ↓ 236 tokens · thought for 1s)",
        ];
        for line in running {
            assert!(hooks.is_match(line), "read as no running hooks: {line:?}");
        }
        let others = ["· Cooking… (21s · ↓ 1.1k tokens)", "✶ Wibbling… "];
        for line in others {
            assert!(!hooks.is_match(line), "read as running hooks: {line:?}");
        }
    }

    /// The index of a capture's box prompt row: the last row that starts with
    /// the prompt glyph.
    fn prompt_row<S: AsRef<str>>(lines: &[S]) -> usize {
        lines
            .iter()
            .rposition(|line| line.as_ref().trim_start().starts_with('❯'))
            .expect("the capture's prompt row")
    }

    /// A capture with `rows` inserted above the box's top rule.
    fn with_rows_above_the_box(capture: &str, rows: &[&str]) -> String {
        let lines: Vec<&str> = capture.lines().collect();
        let prompt = prompt_row(&lines);
        let top = lines[..prompt]
            .iter()
            .rposition(|line| line.trim_start().starts_with('─'))
            .expect("the box's top rule");
        let mut built: Vec<&str> = lines[..top].to_vec();
        built.extend_from_slice(rows);
        built.extend_from_slice(&lines[top..]);
        built.join("\r\n")
    }

    /// A capture whose box row reads the bash-mode prompt.
    fn with_bash_prompt(capture: &str) -> String {
        let mut lines: Vec<String> = capture.lines().map(str::to_owned).collect();
        let prompt = prompt_row(&lines);
        lines[prompt] = "! ".to_owned();
        lines.join("\r\n")
    }

    #[test]
    fn reply_prose_is_not_a_dialog() {
        let frame = with_rows_above_the_box(
            include_str!("fixtures/v03-ready.txt"),
            &[
                "● I've drafted the change. Would you like to proceed?",
                "Ready to submit your answers?",
            ],
        );
        let reading = read_claude(&screen_from(&frame));
        assert_eq!(reading.verdict, Verdict::State(ScreenState::Idle));
        assert_eq!(reading.rule.as_deref(), Some("idle_prompt_box"));
    }

    #[test]
    fn a_long_todo_list_keeps_the_spinner_in_view() {
        let mut rows = vec!["· Cooking… (21s · ↓ 1.1k tokens)".to_owned()];
        for task in 1..=15 {
            rows.push(format!("  ⎿  ◻ task {task}"));
        }
        rows.push("─".repeat(40));
        rows.push("❯ ".to_owned());
        rows.push("─".repeat(40));
        rows.push("  ? for shortcuts".to_owned());
        let reading = read_claude(&screen_from(&rows.join("\r\n")));
        assert_eq!(reading.verdict, Verdict::State(ScreenState::Working));
        assert_eq!(reading.rule.as_deref(), Some("turn_spinner"));
    }

    #[test]
    fn a_bash_mode_box_reads_idle() {
        let frame = with_bash_prompt(include_str!("fixtures/v03-ready.txt"));
        let reading = read_claude(&screen_from(&frame));
        assert_eq!(reading.verdict, Verdict::State(ScreenState::Idle));
        assert_eq!(reading.rule.as_deref(), Some("idle_prompt_box"));
        assert!(!reading.from_title);
    }

    #[test]
    fn rules_are_in_priority_order() {
        for pair in claude_table().rules.windows(2) {
            let [above, below] = pair else { continue };
            assert!(
                above.priority >= below.priority,
                "{} ({}) sits above {} ({})",
                above.name,
                above.priority,
                below.name,
                below.priority
            );
        }
    }

    #[test]
    fn every_pattern_compiles() {
        assert_eq!(
            rules_for(Agent::Claude).rules.len(),
            claude_table().rules.len(),
            "every rule compiled, none skipped"
        );
        assert_eq!(
            COMPILED_PROMPTS.len(),
            SHELL_PROMPTS.len(),
            "every shell prompt pattern compiled, none skipped"
        );
    }

    #[test]
    fn shell_prompt_examples_and_non_examples() {
        let last_row_is_prompt = |row: &str| {
            let mut screen = Screen::new(80, 5);
            screen.feed(format!("some output\r\n{row}").as_bytes());
            last_row_is_shell_prompt(&screen)
        };
        let prompts = [
            r"PS C:\work>",
            r"PS C:\Users\me\src\rt>",
            r"C:\work>",
            "me@box:~/rt$",
            "root@box:/#",
        ];
        for row in prompts {
            assert!(last_row_is_prompt(row), "read as no prompt: {row:?}");
        }
        let others = [
            "❯",
            "  ⏵⏵ auto mode on · 1 shell",
            "  ctx:  11%  |  ds: $  17.40  |  Opus 5.5",
            "· Cooking… (21s · ↓ 1.1k tokens)",
            "",
        ];
        for row in others {
            assert!(!last_row_is_prompt(row), "read as a prompt: {row:?}");
        }
    }

    #[test]
    fn title_rules() {
        let reading = |title: &str| {
            let mut screen = Screen::new(40, 5);
            screen.feed(format!("\x1b]0;{title}\x07").as_bytes());
            read_claude(&screen)
        };
        for working in ["⠋ Fix tests", "◐ Fix tests"] {
            let reading = reading(working);
            assert_eq!(reading.verdict, Verdict::State(ScreenState::Working));
            assert_eq!(reading.rule.as_deref(), Some("title_working"));
            assert!(reading.from_title);
        }
        let idle = reading("✳ Claude Code");
        assert_eq!(idle.verdict, Verdict::State(ScreenState::Idle));
        assert_eq!(idle.rule.as_deref(), Some("title_idle"));
        assert!(idle.from_title);
        let blank = read_claude(&Screen::new(40, 5));
        assert_eq!(
            blank,
            Reading {
                verdict: Verdict::NoMatch,
                rule: None,
                from_title: false
            }
        );
    }
}
