//! The evaluator of an agent's screen rules: the state one rendered session
//! shows.
//!
//! A plugin hands the daemon its rule table as data
//! ([`agent_api::ScreenRules`]); [`CompiledRules::try_compile`] checks the
//! whole table and compiles its patterns once, refusing the table outright
//! when any rule is unsound, and [`CompiledRules::read`] evaluates it over the
//! regions [`Screen`] reads back, by priority: the highest-priority rule whose
//! matcher hits its region, and none of whose guards does, decides the
//! reading. An overlay rule keeps whatever state the session already has,
//! since the overlay is drawn over a turn that may be running or done. A
//! background rule never decides the state: [`CompiledRules::read_background`]
//! reads those rules apart, as a second reading of the same screen. An
//! agent session's status watcher ([`crate::pty_state::watch`]) and the
//! plain-shell watcher ([`crate::pty_state::watch_plain_shell`]) read its
//! screen through the rules of the session's agent ([`rules_for`]).
//! [`CompiledRules::explain`] traces the same evaluation rule by rule.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use agent_api::{LineMatcher, RuleError, ScreenRegion, ScreenRule, ScreenRules, ScreenVerdict};
use protocol::AgentId;
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
    /// Work runs on in the background. Only a background rule gives it, and
    /// [`CompiledRules::read_background`] reads those rules apart from the
    /// state rules, so a [`Reading`] never carries it.
    Background,
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

/// How many characters of a region a rule trace's preview keeps.
const PREVIEW_CHARS: usize = 240;

/// One agent's rule table with its patterns compiled, highest priority first.
pub struct CompiledRules {
    rules: Vec<CompiledRule>,
    /// Why the agent's table was refused, when it was: the set then holds no
    /// rule. Empty for a table that compiled.
    rejected: Vec<RuleError>,
}

/// One evaluation traced rule by rule: what [`CompiledRules::explain`] gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Explanation {
    /// The reading the evaluation gives, the same [`CompiledRules::read`]
    /// gives.
    pub winner: Reading,
    /// Every rule, in the order it is evaluated.
    pub rules: Vec<RuleTrace>,
    /// Why the agent's table was refused, when it was.
    pub rejected: Vec<RuleError>,
}

/// What one rule saw in one evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleTrace {
    pub name: Arc<str>,
    pub priority: u16,
    pub region: ScreenRegion,
    pub verdict: Verdict,
    /// Whether the rule fired: its matcher hit and no guard did.
    pub matched: bool,
    /// The index of the first guard that vetoed a matcher hit.
    pub vetoed_by: Option<usize>,
    /// The region's lines joined with `" | "`, cut to [`PREVIEW_CHARS`]
    /// characters.
    pub preview: String,
}

/// What one rule makes of its region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The matcher hit no line.
    Missed,
    /// The matcher hit and no guard did.
    Matched,
    /// The matcher hit, and the guard at this index vetoed it.
    Vetoed(usize),
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
    /// keep their table order), or refuses the whole table.
    ///
    /// # Errors
    ///
    /// Every problem [`ScreenRules::validate`] finds, then every pattern
    /// that does not compile, in table order; one is enough to refuse the
    /// table.
    pub fn try_compile(rules: &ScreenRules) -> Result<Self, Vec<RuleError>> {
        let mut errors = match rules.validate() {
            Ok(()) => Vec::new(),
            Err(errors) => errors,
        };
        let mut compiled = Vec::with_capacity(rules.rules.len());
        for (index, rule) in rules.rules.iter().enumerate() {
            match compile_rule(rule) {
                Ok(Some(rule)) => compiled.push(rule),
                Ok(None) => {}
                Err(problems) => errors.extend(
                    problems
                        .into_iter()
                        .map(|problem| RuleError::new(index, rule, problem)),
                ),
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        compiled.sort_by_key(|rule| Reverse(rule.priority));
        Ok(Self {
            rules: compiled,
            rejected: Vec::new(),
        })
    }

    /// A set with no rule, standing for a table refused for `rejected`.
    fn refused(rejected: Vec<RuleError>) -> Self {
        Self {
            rules: Vec::new(),
            rejected,
        }
    }

    /// The state the screen shows.
    #[must_use]
    pub fn read(&self, screen: &Screen) -> Reading {
        self.evaluate(screen, None)
    }

    /// The background rule that fires on the screen, by priority: the name of
    /// the first one, or `None` when none does. The state rules play no part.
    #[must_use]
    pub fn read_background(&self, screen: &Screen) -> Option<Arc<str>> {
        let mut regions = RegionLines::default();
        self.rules
            .iter()
            .filter(|rule| rule.verdict == Verdict::Background)
            .find(|rule| rule.outcome(regions.get(screen, rule.region)) == Outcome::Matched)
            .map(|rule| Arc::clone(&rule.name))
    }

    /// The state the screen shows, with what every rule saw on the way, the
    /// background rules among them.
    #[must_use]
    pub fn explain(&self, screen: &Screen) -> Explanation {
        let mut rules = Vec::with_capacity(self.rules.len());
        let winner = self.evaluate(screen, Some(&mut rules));
        Explanation {
            winner,
            rules,
            rejected: self.rejected.clone(),
        }
    }

    /// The one evaluation [`Self::read`] and [`Self::explain`] share: the
    /// first state rule that fires decides the reading. Without a trace to
    /// fill it skips the background rules and stops there; with one it goes on
    /// and traces every rule.
    fn evaluate(&self, screen: &Screen, mut trace: Option<&mut Vec<RuleTrace>>) -> Reading {
        let mut regions = RegionLines::default();
        let mut winner: Option<&CompiledRule> = None;
        for rule in &self.rules {
            let state_rule = rule.verdict != Verdict::Background;
            if !state_rule && trace.is_none() {
                continue;
            }
            let lines = regions.get(screen, rule.region);
            let outcome = rule.outcome(lines);
            if winner.is_none() && state_rule && outcome == Outcome::Matched {
                winner = Some(rule);
            }
            match trace.as_deref_mut() {
                Some(traces) => traces.push(rule.trace(outcome, lines)),
                None if winner.is_some() => break,
                None => {}
            }
        }
        winner.map_or(
            Reading {
                verdict: Verdict::NoMatch,
                rule: None,
                from_title: false,
            },
            |rule| Reading {
                verdict: rule.verdict,
                rule: Some(Arc::clone(&rule.name)),
                from_title: rule.region == ScreenRegion::Title,
            },
        )
    }
}

/// Each agent's compiled rules, compiled on first use.
static BY_AGENT: LazyLock<Mutex<HashMap<AgentId, Arc<CompiledRules>>>> =
    LazyLock::new(Mutex::default);

/// The compiled screen rules of `agent`'s plugin, compiled once per agent.
/// An agent with no registered plugin gets no rules, and so does one whose
/// table is refused ([`compile_or_reject`]).
#[must_use]
pub fn rules_for(agent: &AgentId) -> Arc<CompiledRules> {
    let mut cache = crate::sync::lock(&BY_AGENT);
    let rules = cache.entry(agent.clone()).or_insert_with(|| {
        let compiled = if let Some(plugin) = agent_registry::plugin(agent) {
            compile_or_reject(agent, plugin.screen_rules())
        } else {
            tracing::warn!(
                "no agent plugin `{}` is registered; its screen has no rules",
                agent.0
            );
            CompiledRules::refused(Vec::new())
        };
        Arc::new(compiled)
    });
    Arc::clone(rules)
}

/// `agent`'s table compiled, or, when it is refused, one error naming every
/// bad rule and a set with no rule: the screen then never writes a status.
fn compile_or_reject(agent: &AgentId, rules: &ScreenRules) -> CompiledRules {
    match CompiledRules::try_compile(rules) {
        Ok(compiled) => compiled,
        Err(errors) => {
            let listed: Vec<String> = errors.iter().map(ToString::to_string).collect();
            tracing::error!(
                agent = %agent.0,
                errors = %listed.join("; "),
                "screen rules: the agent's table is refused; its screen has no rules"
            );
            CompiledRules::refused(errors)
        }
    }
}

/// A shell's prompt row, each pattern with whether it is the POSIX form,
/// which a row holding one of an agent's box glyphs never matches: PowerShell's
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

/// Compiles one rule's matchers. A pattern that does not compile is an error
/// naming the matcher; a rule with a part this build does not know gives
/// `None`, since [`ScreenRules::validate`] already reports that part.
fn compile_rule(rule: &ScreenRule) -> Result<Option<CompiledRule>, Vec<String>> {
    let mut problems = Vec::new();
    let matcher = compile_matcher("its matcher", &rule.matcher, &mut problems);
    let mut not = Vec::with_capacity(rule.not.len());
    for (index, guard) in rule.not.iter().enumerate() {
        not.push(compile_matcher(
            &format!("guard {index}"),
            guard,
            &mut problems,
        ));
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    let (Some(verdict), Some(matcher), Some(not)) = (
        verdict_of(rule.verdict),
        matcher,
        not.into_iter().collect::<Option<Vec<_>>>(),
    ) else {
        return Ok(None);
    };
    if rule.region == ScreenRegion::Unknown {
        return Ok(None);
    }
    Ok(Some(CompiledRule {
        name: Arc::from(rule.name.as_str()),
        priority: rule.priority,
        region: rule.region,
        matcher,
        not,
        verdict,
    }))
}

/// The verdict a rule gives, or `None` when this build does not know it.
fn verdict_of(verdict: ScreenVerdict) -> Option<Verdict> {
    match verdict {
        ScreenVerdict::Working => Some(Verdict::State(ScreenState::Working)),
        ScreenVerdict::AwaitingInput => Some(Verdict::State(ScreenState::AwaitingInput)),
        ScreenVerdict::Idle => Some(Verdict::State(ScreenState::Idle)),
        ScreenVerdict::Keep => Some(Verdict::Keep),
        ScreenVerdict::Background => Some(Verdict::Background),
        ScreenVerdict::Unknown => None,
    }
}

/// Compiles one matcher, named `what` in a problem it adds to `problems` when
/// its pattern does not compile; `None` for that and for a matcher this
/// build does not know.
fn compile_matcher(
    what: &str,
    matcher: &LineMatcher,
    problems: &mut Vec<String>,
) -> Option<CompiledMatcher> {
    match matcher {
        LineMatcher::Contains(needle) => Some(CompiledMatcher::Contains(needle.clone())),
        LineMatcher::Regex(pattern) => match Regex::new(pattern) {
            Ok(regex) => Some(CompiledMatcher::Line(regex)),
            Err(error) => {
                problems.push(format!("{what} does not compile: {error}"));
                None
            }
        },
        LineMatcher::Unknown => None,
    }
}

impl CompiledRule {
    /// What the rule makes of `lines`: whether its matcher hits one of them,
    /// and if so the first guard that also does.
    fn outcome(&self, lines: &[String]) -> Outcome {
        if !self.matcher.hits(lines) {
            return Outcome::Missed;
        }
        match self.not.iter().position(|guard| guard.hits(lines)) {
            Some(index) => Outcome::Vetoed(index),
            None => Outcome::Matched,
        }
    }

    /// The trace of this rule's `outcome` over `lines`.
    fn trace(&self, outcome: Outcome, lines: &[String]) -> RuleTrace {
        RuleTrace {
            name: Arc::clone(&self.name),
            priority: self.priority,
            region: self.region,
            verdict: self.verdict,
            matched: outcome == Outcome::Matched,
            vetoed_by: match outcome {
                Outcome::Vetoed(index) => Some(index),
                Outcome::Missed | Outcome::Matched => None,
            },
            preview: lines.join(" | ").chars().take(PREVIEW_CHARS).collect(),
        }
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
/// compiled rule never reads one: [`CompiledRules::try_compile`] refuses it).
fn region_lines(screen: &Screen, region: ScreenRegion) -> Vec<String> {
    match region {
        ScreenRegion::BottomNonEmpty { rows } => screen.bottom_non_empty_lines(rows),
        ScreenRegion::PromptBoxBody => screen.prompt_box_body(),
        ScreenRegion::AbovePromptBox { rows } => screen.above_prompt_box(rows),
        ScreenRegion::Title => screen.title().into_iter().collect(),
        ScreenRegion::AfterLastRule => screen.after_last_horizontal_rule(),
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
        rules_for(&agent_registry::default_agent()).read(screen)
    }

    /// Claude's rule table as its plugin hands it over.
    fn claude_table() -> &'static ScreenRules {
        agent_api::AgentPlugin::screen_rules(&agent_claude::CLAUDE)
    }

    /// The regex Claude's compiled rules hold for the line rule `name`.
    fn line_rule(name: &str) -> Regex {
        rules_for(&agent_registry::default_agent())
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

    /// `rules` compiled, which the test expects to succeed.
    fn compiled(rules: Vec<ScreenRule>) -> CompiledRules {
        CompiledRules::try_compile(&ScreenRules { rules }).expect("the table compiles")
    }

    #[test]
    fn claude_rules_all_compile() {
        let compiled =
            CompiledRules::try_compile(claude_table()).expect("Claude's rule table compiles");
        assert_eq!(compiled.rules.len(), 18, "no Claude rule was dropped");
        let background = compiled
            .rules
            .iter()
            .filter(|rule| rule.verdict == Verdict::Background)
            .count();
        assert_eq!(background, 2, "two of them read background work");
    }

    /// The background rule Claude's rules read on `text`, as a screen.
    fn read_background_claude(text: &str) -> Option<Arc<str>> {
        rules_for(&agent_registry::default_agent()).read_background(&screen_from(text))
    }

    #[test]
    fn background_footer_is_read_from_fixtures() {
        let shown = [
            (
                "claude--idle-background-shell",
                include_str!("fixtures/claude--idle-background-shell.txt"),
                "background_shell",
            ),
            (
                "claude--v2301-done-shell-running--w100",
                include_str!("fixtures/claude--v2301-done-shell-running--w100.txt"),
                "background_shell",
            ),
            (
                "claude--v2302-working-agents-panel--w119",
                include_str!("fixtures/claude--v2302-working-agents-panel--w119.txt"),
                "background_subagents",
            ),
            (
                "v71-agents-running",
                include_str!("fixtures/v71-agents-running.txt"),
                "background_shell",
            ),
        ];
        for (name, text, rule) in shown {
            assert_eq!(
                read_background_claude(text).as_deref(),
                Some(rule),
                "{name}: background work not read"
            );
        }
        let absent = [
            (
                "claude--fresh-idle",
                include_str!("fixtures/claude--fresh-idle.txt"),
            ),
            ("claude--done", include_str!("fixtures/claude--done.txt")),
            ("v03-ready", include_str!("fixtures/v03-ready.txt")),
            ("c11-turn1-end", include_str!("fixtures/c11-turn1-end.txt")),
            (
                "claude--working",
                include_str!("fixtures/claude--working.txt"),
            ),
            (
                "claude--permission-bash",
                include_str!("fixtures/claude--permission-bash.txt"),
            ),
        ];
        for (name, text) in absent {
            assert_eq!(
                read_background_claude(text),
                None,
                "{name}: background work read where none runs"
            );
        }
    }

    #[test]
    fn background_rule_ignores_prose() {
        let capture = include_str!("fixtures/v03-ready.txt");
        let prose = with_rows_above_the_box(
            capture,
            &[
                "  ran 2 shell commands · 1 shell",
                "  ⏵⏵ auto mode on · 1 shell",
                "✻ Worked for 6s · done 12:36 PM · 1 shell still running",
            ],
        );
        assert_eq!(
            read_background_claude(&prose),
            None,
            "a transcript row above the box read as background work"
        );
        let mode_line = "  ⏵⏵ auto mode on (shift+tab to cycle) · ← 2 agents";
        assert!(capture.contains(mode_line));
        let footer = capture.replace(mode_line, "  ⏵⏵ auto mode on · 2 shells · ← 2 agents");
        assert_eq!(
            read_background_claude(&footer).as_deref(),
            Some("background_shell"),
            "the footer's shell count was not read"
        );
        let rule = line_rule("background_shell");
        for other in [
            "  ran 2 shell commands · 1 shell commands",
            "✻ Worked for 6s · done 12:36 PM · 1 shell still running",
            "  ⏵⏵ auto mode on · 1 shell still running",
        ] {
            assert!(!rule.is_match(other), "read as a shell count: {other:?}");
        }
    }

    #[test]
    fn after_last_rule_region_reads_the_footer() {
        let rule_row = "─".repeat(24);
        let footer = compiled(vec![rule(
            "footer_shell",
            100,
            ScreenRegion::AfterLastRule,
            LineMatcher::Contains("1 shell".to_string()),
            ScreenVerdict::Idle,
        )]);
        let needle_above_the_rule = screen_from(&format!(
            "some output\n1 shell running\nmore output\n{rule_row}\n  ? for shortcuts"
        ));
        let miss = footer.read(&needle_above_the_rule);
        assert_eq!(
            miss.verdict,
            Verdict::NoMatch,
            "the footer below the last rule holds no needle"
        );
        let needle_below_the_rule = screen_from(&format!(
            "some output\nmore output\n{rule_row}\n1 shell running"
        ));
        let hit = footer.read(&needle_below_the_rule);
        assert_eq!(hit.rule.as_deref(), Some("footer_shell"));
        assert_eq!(hit.verdict, Verdict::State(ScreenState::Idle));
        let last_four = compiled(vec![rule(
            "last_four",
            100,
            ScreenRegion::BottomNonEmpty { rows: 4 },
            LineMatcher::Contains("1 shell".to_string()),
            ScreenVerdict::Idle,
        )]);
        let hit = last_four.read(&needle_above_the_rule);
        assert_eq!(
            hit.rule.as_deref(),
            Some("last_four"),
            "the text is there; the region is what misses"
        );
    }

    #[test]
    fn explain_lists_every_rule_with_the_winner_marked() {
        let screen = screen_from(include_str!("fixtures/v21-perm-bash.txt"));
        let rules = rules_for(&agent_registry::default_agent());
        let explanation = rules.explain(&screen);
        assert_eq!(
            explanation.winner,
            rules.read(&screen),
            "explain and read agree"
        );
        let names: Vec<&str> = explanation.rules.iter().map(|trace| &*trace.name).collect();
        let table: Vec<&str> = claude_table()
            .rules
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(names, table, "every rule, in priority order");
        let winner = explanation.winner.rule.as_deref().expect("a rule wins");
        let at = names
            .iter()
            .position(|name| *name == winner)
            .expect("the winner is listed");
        assert!(
            explanation.rules[at].matched,
            "the winner is marked matched"
        );
        assert_eq!(explanation.rules[at].vetoed_by, None);
        for above in &explanation.rules[..at] {
            assert!(
                !above.matched,
                "{} sits above the winner yet matched",
                above.name
            );
        }
        assert_eq!(explanation.rejected, Vec::<RuleError>::new());
    }

    #[test]
    fn explain_reports_the_guard_that_vetoed() {
        let guarded = ScreenRule {
            not: vec![
                LineMatcher::Contains("absent".to_string()),
                LineMatcher::Contains("VETO".to_string()),
            ],
            verdict: ScreenVerdict::AwaitingInput,
            ..busy_rule("guarded", 900)
        };
        let rules = compiled(vec![guarded, busy_rule("busy", 100)]);
        let explanation = rules.explain(&screen_from("BUSY VETO"));
        let guarded = &explanation.rules[0];
        assert_eq!(&*guarded.name, "guarded");
        assert!(!guarded.matched);
        assert_eq!(guarded.vetoed_by, Some(1), "the second guard vetoed");
        assert_eq!(guarded.priority, 900);
        assert_eq!(guarded.region, ScreenRegion::BottomNonEmpty { rows: 4 });
        assert_eq!(guarded.verdict, Verdict::State(ScreenState::AwaitingInput));
        let busy = &explanation.rules[1];
        assert!(busy.matched);
        assert_eq!(busy.vetoed_by, None);
        assert_eq!(explanation.winner.rule.as_deref(), Some("busy"));
        let missed = rules.explain(&screen_from("idle"));
        assert!(
            missed
                .rules
                .iter()
                .all(|t| !t.matched && t.vetoed_by.is_none())
        );
        assert_eq!(missed.winner.verdict, Verdict::NoMatch);
    }

    #[test]
    fn explain_preview_is_capped_at_240_chars() {
        let rules = compiled(vec![busy_rule("busy", 100)]);
        let short = rules.explain(&screen_from("one\r\ntwo BUSY"));
        assert_eq!(short.rules[0].preview, "one | two BUSY");
        let long = "x".repeat(200);
        let text = format!("{long}\r\n{long}");
        let preview = &rules.explain(&screen_from(&text)).rules[0].preview;
        assert_eq!(preview.chars().count(), 240);
        assert_eq!(*preview, format!("{long} | {}", "x".repeat(37)));
    }

    #[test]
    fn a_table_with_one_bad_regex_is_rejected_whole() {
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
        let errors = CompiledRules::try_compile(&rules)
            .err()
            .expect("a table with a bad regex is refused");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].rule, "broken");
        assert!(
            errors[0]
                .problem
                .starts_with("its matcher does not compile"),
            "{errors:?}"
        );

        let installed = compile_or_reject(&AgentId("test-agent".to_owned()), &rules);
        let explanation = installed.explain(&screen_from("BUSY"));
        assert!(
            explanation.rules.is_empty(),
            "no rule of a rejected table is used"
        );
        assert_eq!(explanation.winner.verdict, Verdict::NoMatch);
        assert_eq!(
            explanation.rejected, errors,
            "the rejection shows in the trace"
        );
    }

    #[test]
    fn an_unknown_part_rejects_the_whole_table() {
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
        let errors = CompiledRules::try_compile(&rules)
            .err()
            .expect("a table with unknown parts is refused");
        let refused: Vec<&str> = errors.iter().map(|error| error.rule.as_str()).collect();
        assert_eq!(
            refused,
            [
                "unknown_matcher",
                "unknown_guard",
                "unknown_region",
                "unknown_verdict"
            ],
            "every rule with an unknown part is named, and the table is refused"
        );
    }

    #[test]
    fn a_rising_priority_table_is_rejected() {
        let idle_first = ScreenRule {
            verdict: ScreenVerdict::Idle,
            ..busy_rule("low", 100)
        };
        let rules = ScreenRules {
            rules: vec![idle_first, busy_rule("high", 500)],
        };
        let errors = CompiledRules::try_compile(&rules)
            .err()
            .expect("a table whose priorities rise is refused");
        let refused: Vec<&str> = errors.iter().map(|error| error.rule.as_str()).collect();
        assert_eq!(refused, ["high"], "{errors:?}");
    }

    #[test]
    fn a_plugin_without_rules_reads_no_match() {
        let compiled = compiled(Vec::new());
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
            let screen = screen_from(text);
            let reading = read_claude(&screen);
            if reading.verdict != expected {
                let explanation = rules_for(&agent_registry::default_agent()).explain(&screen);
                let hits: Vec<String> = explanation
                    .rules
                    .iter()
                    .filter(|trace| trace.matched || trace.vetoed_by.is_some())
                    .map(|trace| {
                        format!(
                            "  {} [{}] matched {} vetoed by {:?}: {}",
                            trace.name,
                            trace.priority,
                            trace.matched,
                            trace.vetoed_by,
                            trace.preview
                        )
                    })
                    .collect();
                wrong.push(format!(
                    "{name}: wanted {expected:?}, got {:?} by rule {:?}\n{}",
                    reading.verdict,
                    reading.rule,
                    hits.join("\n")
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
            rules_for(&agent_registry::default_agent()).rules.len(),
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
