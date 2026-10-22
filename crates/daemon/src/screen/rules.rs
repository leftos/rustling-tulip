//! Claude Code's screen rules: the state one rendered session shows.
//!
//! One table of rules over the regions [`Screen`] reads back, evaluated by
//! priority: the highest-priority rule whose matcher hits its region, and none
//! of whose guards does, decides the reading. An overlay rule keeps whatever
//! state the session already has, since the overlay is drawn over a turn that
//! may be running or done. Nothing calls this yet; the status watchers read it
//! in a later change.

use std::cmp::Reverse;
use std::sync::LazyLock;

use regex::Regex;

use super::Screen;

/// The spinner line of a turn in progress: a cycling glyph, a capitalised
/// verb, an ellipsis, then the optional phase, the elapsed time and the token
/// details.
const SPINNER: &str = r"^\s*[·✢*✶✻✽]\s+\p{Lu}[\p{L}'’-]*…(?:\s+·\s+[\p{L} ]+?)?(?:\s+\(.*\))?\s*$";

/// The spinner a turn keeps painted while its hooks run: Claude leaves it up
/// across a hook-reported stop or tool start, so the line is not a turn of its
/// own.
const HOOKS_RUNNING: &str = r"^\s*[·✢*✶✻✽]\s+\S.*\(running \p{L}+ hooks…";

/// The state a Claude session's screen shows.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    pub verdict: Verdict,
    pub rule: Option<&'static str>,
    /// Whether the winning rule read the OSC title rather than the screen
    /// body, so a caller can refuse a state known only from the title.
    pub from_title: bool,
}

/// The part of the screen a rule reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Region {
    /// The last `n` non-empty rows.
    BottomNonEmpty(usize),
    /// The rows below the last rule row.
    #[cfg_attr(
        test,
        expect(
            dead_code,
            reason = "no rule reads this region yet; the reader is there for the rules a later table adds"
        )
    )]
    AfterLastRule,
    /// The rows between the prompt box's two rules.
    PromptBoxBody,
    /// The last `n` non-empty rows above the prompt box.
    AbovePromptBox(usize),
    /// The last OSC 0/2 title the stream set.
    Title,
}

/// How a rule looks for a hit in its region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Matcher {
    /// A literal substring of a row.
    Contains(&'static str),
    /// A regex matched against each row.
    Line(&'static str),
}

/// One rule: where to look, what for, what to ignore, and what a hit means.
struct Rule {
    name: &'static str,
    /// Higher wins. The table is stored in descending order, and evaluation
    /// takes the first rule that matches.
    priority: u16,
    region: Region,
    matcher: Matcher,
    /// Matchers that veto the rule when any of them hits the same region.
    not: &'static [Matcher],
    verdict: Verdict,
}

/// The Claude screen rules, in descending priority order.
static CLAUDE_RULES: &[Rule] = &[
    Rule {
        name: "overlay_detailed_transcript",
        priority: 990,
        region: Region::BottomNonEmpty(4),
        matcher: Matcher::Contains("Showing detailed transcript"),
        not: &[],
        verdict: Verdict::Keep,
    },
    Rule {
        name: "overlay_model_picker",
        priority: 990,
        region: Region::BottomNonEmpty(4),
        matcher: Matcher::Contains("Enter to set as default"),
        not: &[],
        verdict: Verdict::Keep,
    },
    Rule {
        name: "overlay_btw",
        priority: 990,
        region: Region::BottomNonEmpty(4),
        matcher: Matcher::Contains("f to fork · Esc to close"),
        not: &[],
        verdict: Verdict::Keep,
    },
    Rule {
        name: "overlay_rewind",
        priority: 990,
        region: Region::BottomNonEmpty(4),
        matcher: Matcher::Contains("Enter to continue · Esc to cancel"),
        not: &[],
        verdict: Verdict::Keep,
    },
    Rule {
        name: "dialog_permission",
        priority: 980,
        region: Region::BottomNonEmpty(24),
        matcher: Matcher::Contains("Esc to cancel · Tab to amend"),
        not: &[],
        verdict: Verdict::State(ScreenState::AwaitingInput),
    },
    Rule {
        name: "dialog_question",
        priority: 980,
        region: Region::BottomNonEmpty(24),
        matcher: Matcher::Line(r"Enter to select ·.*Esc to cancel"),
        not: &[],
        verdict: Verdict::State(ScreenState::AwaitingInput),
    },
    Rule {
        name: "dialog_question_review",
        priority: 980,
        region: Region::BottomNonEmpty(24),
        matcher: Matcher::Contains("Ready to submit your answers?"),
        not: &[],
        verdict: Verdict::State(ScreenState::AwaitingInput),
    },
    Rule {
        name: "dialog_plan",
        priority: 980,
        region: Region::BottomNonEmpty(24),
        matcher: Matcher::Contains("Would you like to proceed?"),
        not: &[],
        verdict: Verdict::State(ScreenState::AwaitingInput),
    },
    Rule {
        name: "dialog_trust",
        priority: 980,
        region: Region::BottomNonEmpty(24),
        matcher: Matcher::Contains("Enter to confirm · Esc to cancel"),
        not: &[],
        verdict: Verdict::State(ScreenState::AwaitingInput),
    },
    Rule {
        name: "turn_hooks_running",
        priority: 975,
        region: Region::AbovePromptBox(12),
        matcher: Matcher::Line(HOOKS_RUNNING),
        not: &[],
        verdict: Verdict::Keep,
    },
    Rule {
        name: "turn_spinner",
        priority: 970,
        region: Region::AbovePromptBox(12),
        matcher: Matcher::Line(SPINNER),
        not: &[],
        verdict: Verdict::State(ScreenState::Working),
    },
    Rule {
        name: "overlay_heads_up",
        priority: 960,
        region: Region::AbovePromptBox(16),
        matcher: Matcher::Contains("✦ Heads up ·"),
        not: &[],
        verdict: Verdict::Keep,
    },
    Rule {
        name: "idle_interrupted",
        priority: 950,
        region: Region::AbovePromptBox(12),
        matcher: Matcher::Contains("⎿  Interrupted ·"),
        not: &[],
        verdict: Verdict::State(ScreenState::Idle),
    },
    Rule {
        name: "idle_prompt_box",
        priority: 940,
        region: Region::PromptBoxBody,
        matcher: Matcher::Line(r"^\s*❯"),
        not: &[],
        verdict: Verdict::State(ScreenState::Idle),
    },
    Rule {
        name: "title_working",
        priority: 200,
        region: Region::Title,
        matcher: Matcher::Line(r"^[\u{2800}-\u{28FF}\u{25D0}-\u{25D3}] "),
        not: &[],
        verdict: Verdict::State(ScreenState::Working),
    },
    Rule {
        name: "title_idle",
        priority: 100,
        region: Region::Title,
        matcher: Matcher::Line("^✳ "),
        not: &[],
        verdict: Verdict::State(ScreenState::Idle),
    },
];

/// One rule with its matchers compiled.
struct Compiled {
    name: &'static str,
    priority: u16,
    region: Region,
    matcher: CompiledMatcher,
    not: Vec<CompiledMatcher>,
    verdict: Verdict,
}

/// One matcher, compiled.
enum CompiledMatcher {
    Contains(&'static str),
    Line(Regex),
}

/// The table with its patterns compiled once, highest priority first; a rule
/// whose pattern does not compile is left out.
static COMPILED: LazyLock<Vec<Compiled>> = LazyLock::new(|| {
    let mut compiled: Vec<Compiled> = CLAUDE_RULES.iter().filter_map(compile_rule).collect();
    compiled.sort_by_key(|rule| Reverse(rule.priority));
    compiled
});

/// The state the screen shows.
#[must_use]
pub fn read_claude(screen: &Screen) -> Reading {
    let mut regions = RegionLines::default();
    for rule in COMPILED.iter() {
        if rule.matches(regions.get(screen, rule.region)) {
            return Reading {
                verdict: rule.verdict,
                rule: Some(rule.name),
                from_title: rule.region == Region::Title,
            };
        }
    }
    Reading {
        verdict: Verdict::NoMatch,
        rule: None,
        from_title: false,
    }
}

/// The regions read so far, so each one is read once per evaluation.
#[derive(Default)]
struct RegionLines {
    read: Vec<(Region, Vec<String>)>,
}

impl RegionLines {
    /// The lines of `region`, read now when this evaluation has not needed it
    /// yet.
    fn get(&mut self, screen: &Screen, region: Region) -> &[String] {
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
/// not compile.
fn compile_rule(rule: &Rule) -> Option<Compiled> {
    let matcher = compile_matcher(rule.name, rule.matcher)?;
    let mut not = Vec::with_capacity(rule.not.len());
    for guard in rule.not {
        not.push(compile_matcher(rule.name, *guard)?);
    }
    Some(Compiled {
        name: rule.name,
        priority: rule.priority,
        region: rule.region,
        matcher,
        not,
        verdict: rule.verdict,
    })
}

/// Compiles one matcher, logging the rule that named a pattern which did not
/// compile.
fn compile_matcher(rule: &str, matcher: Matcher) -> Option<CompiledMatcher> {
    match matcher {
        Matcher::Contains(needle) => Some(CompiledMatcher::Contains(needle)),
        Matcher::Line(pattern) => match Regex::new(pattern) {
            Ok(regex) => Some(CompiledMatcher::Line(regex)),
            Err(error) => {
                tracing::error!(
                    rule = rule,
                    pattern = pattern,
                    %error,
                    "screen rules: a pattern did not compile; the rule is skipped"
                );
                None
            }
        },
    }
}

impl Compiled {
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
            Self::Contains(needle) => line.contains(*needle),
            Self::Line(regex) => regex.is_match(line),
        })
    }
}

/// The lines of one region.
fn region_lines(screen: &Screen, region: Region) -> Vec<String> {
    match region {
        Region::BottomNonEmpty(n) => screen.bottom_non_empty_lines(n),
        Region::AfterLastRule => screen.after_last_horizontal_rule(),
        Region::PromptBoxBody => screen.prompt_box_body(),
        Region::AbovePromptBox(n) => screen.above_prompt_box(n),
        Region::Title => screen.title().into_iter().collect(),
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

    /// The regex the table compiled for the line rule `name`.
    fn line_rule(name: &str) -> &'static Regex {
        COMPILED
            .iter()
            .find(|rule| rule.name == name)
            .and_then(|rule| match &rule.matcher {
                CompiledMatcher::Line(regex) => Some(regex),
                CompiledMatcher::Contains(_) => None,
            })
            .expect("a compiled line rule")
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
        assert_eq!(reading.rule, Some("turn_spinner"));
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

    #[test]
    fn rules_are_in_priority_order() {
        for pair in CLAUDE_RULES.windows(2) {
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
            COMPILED.len(),
            CLAUDE_RULES.len(),
            "every rule compiled, none skipped"
        );
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
            assert_eq!(reading.rule, Some("title_working"));
            assert!(reading.from_title);
        }
        let idle = reading("✳ Claude Code");
        assert_eq!(idle.verdict, Verdict::State(ScreenState::Idle));
        assert_eq!(idle.rule, Some("title_idle"));
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
