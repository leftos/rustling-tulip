//! What one Dashboard row shows: its now line, state word, stuck flags, agent
//! counts, context figure, activity bars, summary lines, ages, cost and
//! tokens; the rows a session's agent tree adds below it; and the header's
//! pills and the queue strip's chips.
//!
//! Plain Rust over the model's [`DashboardRow`], its snapshot and the daemon's
//! `SessionFacts`, so every wording and every number is unit-testable without
//! GPUI; the Dashboard view draws these.

use chrono::{DateTime, Utc};
use protocol::{
    AgentDescriptor, AgentKind, AgentNode, AgentState, NowKind, PendingInput, SessionFacts,
    SessionMode, SessionSnapshot, SessionStatus, StuckReason,
};

use crate::dashboard::{DashboardRow, GroupKind, HeaderCounts};
use crate::dashboard_facts::FactsStore;
use crate::needs_you::{self, Reason};
use crate::status_glyph::{Glyph, glyph};
use crate::ui_theme::UiTheme;

/// The ten-minute window of a session nothing was counted for yet.
const QUIET_WINDOW: [u16; 10] = [0; 10];

/// The queue strip's trailing hint: the waits it lists run longest first.
pub(crate) const QUEUE_HINT: &str = "Oldest first";

/// The daemon's wording for a working session with nothing read yet.
const THINKING: &str = "Thinking";
/// The wording for a session whose turn left background work running.
const WAITING_ON_BACKGROUND: &str = "Waiting on a background task";
/// A session the daemon has not started yet.
const STARTING: &str = "Starting";
/// An idle session whose last turn ended unseen.
const TURN_FINISHED: &str = "Turn finished · not looked at yet";
/// An idle session with nothing to say about itself.
const IDLE: &str = "Idle";
/// A session that ended in an error.
const ERROR: &str = "Error";
/// What a column shows when nothing is known about it.
const DASH: &str = "—";

/// What a row's text is read from: the listed session, its snapshot, the
/// daemon's facts for it, the client's unseen flag and the agent plugins'
/// descriptors.
pub(crate) struct RowInput<'a> {
    /// The model's row: its label, container and id.
    pub(crate) row: &'a DashboardRow,
    /// The session's snapshot, from the sidebar's model.
    pub(crate) session: &'a SessionSnapshot,
    /// The daemon's facts for the session; `None` until they arrive.
    pub(crate) facts: Option<&'a SessionFacts>,
    /// Every agent plugin's descriptor, for the terminal-title fallback.
    pub(crate) agents: &'a [AgentDescriptor],
    /// Whether the sidebar leaf marks the session's last turn unseen.
    pub(crate) unseen: bool,
}

/// The line under a row's state word: the highest now-line rule that applies.
///
/// The daemon words rules 2 to 6; the client words the rest, from the pending
/// prompt, the unseen turn, the summary or the end.
pub(crate) fn now_text(input: &RowInput<'_>) -> String {
    let session = input.session;
    match session.status {
        SessionStatus::AwaitingInput => needs_you::detail_of(Reason::Asking, session, input.agents),
        SessionStatus::Working => daemon_line(input.facts).unwrap_or_else(|| THINKING.to_owned()),
        SessionStatus::Spawning => STARTING.to_owned(),
        SessionStatus::Idle if session.background => {
            daemon_line(input.facts).unwrap_or_else(|| WAITING_ON_BACKGROUND.to_owned())
        }
        SessionStatus::Idle if needs_you::is_answer(session) => {
            headline(session).unwrap_or_else(|| IDLE.to_owned())
        }
        SessionStatus::Idle if input.unseen => TURN_FINISHED.to_owned(),
        SessionStatus::Idle => headline(session).unwrap_or_else(|| IDLE.to_owned()),
        SessionStatus::Stopped => session.exit_code.map_or_else(
            || "Session ended".to_owned(),
            |code| format!("Session ended · exit {code}"),
        ),
        SessionStatus::Error => ERROR.to_owned(),
    }
}

/// The word beside the status glyph, from the status, the prompt's kind and
/// the now line.
pub(crate) fn state_word(input: &RowInput<'_>) -> String {
    let session = input.session;
    match session.status {
        SessionStatus::AwaitingInput => match session.pending_input.as_ref() {
            Some(PendingInput::Question { .. }) => "Question",
            Some(PendingInput::Permission { .. }) => "Permission",
            Some(PendingInput::PlanApproval { .. }) => "Plan",
            _ => "Waiting",
        }
        .to_owned(),
        SessionStatus::Working if in_gate(input.facts) => "In gate".to_owned(),
        SessionStatus::Working => "Working".to_owned(),
        SessionStatus::Spawning => STARTING.to_owned(),
        SessionStatus::Idle if session.background => "Working".to_owned(),
        SessionStatus::Idle if needs_you::is_answer(session) => IDLE.to_owned(),
        SessionStatus::Idle if input.unseen => "Finished".to_owned(),
        SessionStatus::Idle => IDLE.to_owned(),
        SessionStatus::Stopped => session
            .exit_code
            .map_or_else(|| "Stopped".to_owned(), |code| format!("Exited {code}")),
        SessionStatus::Error => ERROR.to_owned(),
    }
}

/// The flags beside the state word that say the session looks stuck.
pub(crate) fn stuck_flags(stuck: &[StuckReason]) -> Vec<String> {
    stuck
        .iter()
        .filter_map(|reason| match reason {
            StuckReason::ToolFailures { count } => {
                Some(format!("⚑ {count} tool calls failed in a row"))
            }
            StuckReason::ContextHigh => Some("⚑ context high".to_owned()),
            StuckReason::Unknown => None,
        })
        .collect()
}

/// How many of each kind the session's agent tree holds, below its main
/// agent: subagents, how many of them run, dispatch runs and background work.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct AgentCounts {
    subs: usize,
    live: usize,
    dispatch: usize,
    bg: usize,
}

impl AgentCounts {
    fn add(&mut self, nodes: &[AgentNode]) {
        for node in nodes {
            match node.kind {
                AgentKind::Subagent => {
                    self.subs += 1;
                    if matches!(node.state, AgentState::Running | AgentState::Queued) {
                        self.live += 1;
                    }
                }
                AgentKind::Dispatch => self.dispatch += 1,
                AgentKind::Shell | AgentKind::Monitor | AgentKind::Gate => self.bg += 1,
                AgentKind::Unknown => {}
            }
            self.add(&node.children);
        }
    }
}

/// The agent counts as one line, e.g. `1 sub (1 live) · 1 dispatch · 2 bg`,
/// or `—` for a session running nothing below its main agent.
pub(crate) fn agents_summary(agents: &[AgentNode]) -> String {
    let mut counts = AgentCounts::default();
    counts.add(agents);
    let mut parts = Vec::new();
    if counts.subs > 0 {
        parts.push(if counts.live > 0 {
            format!("{} sub ({} live)", counts.subs, counts.live)
        } else {
            format!("{} sub", counts.subs)
        });
    }
    if counts.dispatch > 0 {
        parts.push(format!("{} dispatch", counts.dispatch));
    }
    if counts.bg > 0 {
        parts.push(format!("{} bg", counts.bg));
    }
    if parts.is_empty() {
        DASH.to_owned()
    } else {
        parts.join(" · ")
    }
}

/// A row's context figure: its text and whether it reads hot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContextFigure {
    /// The percentage, or `—` before the first assistant record.
    pub(crate) text: String,
    /// True past 80 percent, the point the bar turns to the danger colour.
    pub(crate) hot: bool,
}

/// The context column: a percentage that reads hot past 80, else `—`.
pub(crate) fn context(percent: Option<u8>) -> ContextFigure {
    percent.map_or_else(
        || ContextFigure {
            text: DASH.to_owned(),
            hot: false,
        },
        |percent| ContextFigure {
            text: format!("{percent}%"),
            hot: percent > 80,
        },
    )
}

/// A row's ten-minute bars: their heights and whether the session is quiet.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ActivityBars {
    /// One height in pixels per minute counted, oldest first.
    pub(crate) heights: Vec<f32>,
    /// True when nothing was counted in the window, or none was read yet.
    pub(crate) quiet: bool,
}

/// The ten-minute column's bars: heights scaled to a floor of 6 calls, at
/// least 2 pixels tall each.
pub(crate) fn activity_bars(activity: &[u16]) -> ActivityBars {
    let top = activity.iter().copied().max().unwrap_or(0);
    let scale = f32::from(top.max(6));
    ActivityBars {
        heights: activity
            .iter()
            .map(|count| (f32::from(*count) / scale * 18.0).max(2.0))
            .collect(),
        quiet: activity.iter().all(|count| *count == 0),
    }
}

/// How long ago `since` was, in the daemon's own wording: `<1m`, `5m`,
/// `1h05m`. A time after `now` reads `<1m`.
pub(crate) fn format_age(since: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let minutes = (now - since).num_minutes();
    if minutes < 1 {
        return "<1m".to_owned();
    }
    if minutes < 60 {
        return format!("{minutes}m");
    }
    format!("{}h{:02}m", minutes / 60, minutes % 60)
}

/// A cost in dollars, to the cent: `$0.46`, `$0.00` for a known zero, and
/// `<$0.01` for a cost under a cent that is not zero; `—` when unknown.
pub(crate) fn format_cost(cost: Option<f64>) -> String {
    cost.map_or_else(
        || DASH.to_owned(),
        |cost| {
            if cost > 0.0 && cost < 0.01 {
                "<$0.01".to_owned()
            } else {
                format!("${cost:.2}")
            }
        },
    )
}

/// Input tokens: plain under a thousand, then `842k`, then `3.1M`.
pub(crate) fn format_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }
    if tokens < 1_000_000 {
        return format!("{}k", tokens / 1_000);
    }
    let tenths = tokens / 100_000;
    if tenths.is_multiple_of(10) {
        format!("{}M", tenths / 10)
    } else {
        format!("{}.{}M", tenths / 10, tenths % 10)
    }
}

/// The cost column's two lines: the cost, then the input tokens.
pub(crate) fn cost_and_tokens(facts: Option<&SessionFacts>) -> (String, String) {
    facts.map_or_else(
        || (DASH.to_owned(), DASH.to_owned()),
        |facts| {
            (
                format_cost(Some(facts.cost_usd)),
                format_tokens(facts.tokens_in),
            )
        },
    )
}

/// The Linear column's issue key, or `—` when none was detected.
pub(crate) fn issue_key(facts: Option<&SessionFacts>) -> String {
    facts
        .and_then(|facts| facts.issue.clone())
        .filter(|issue| !issue.trim().is_empty())
        .unwrap_or_else(|| DASH.to_owned())
}

/// The Run and last columns: the runtime since the session started, and how
/// long ago it last did anything. An ended session's run stops at its end,
/// falling back to now when it carries no status stamp.
pub(crate) fn run_and_last(input: &RowInput<'_>, now: DateTime<Utc>) -> (String, String) {
    let session = input.session;
    let end = match session.status {
        SessionStatus::Stopped | SessionStatus::Error => session.status_since.unwrap_or(now),
        _ => now,
    };
    let run = format_age(session.started_at, end);
    let last = input
        .facts
        .and_then(|facts| facts.last_activity_at)
        .map_or_else(
            || DASH.to_owned(),
            |at| format!("{} ago", format_age(at, now)),
        );
    (run, last)
}

/// The summary column's two lines: the newest summary line, then the one
/// before it prefixed by its age (`15m · …`). A session with no summary reads
/// `—` on its first line and nothing on its second.
pub(crate) fn summary_lines(input: &RowInput<'_>, now: DateTime<Utc>) -> (String, Option<String>) {
    let Some(summary) = input.session.summary.as_ref() else {
        return (DASH.to_owned(), None);
    };
    let mut lines = summary.lines.iter();
    let first = lines
        .next()
        .map_or_else(|| DASH.to_owned(), |line| line.text.clone());
    let second = lines
        .next()
        .map(|line| format!("{} · {}", format_age(line.at, now), line.text));
    (first, second)
}

/// The session column's second line: the first member's branch, then the
/// count of the other members of a workspace.
pub(crate) fn branch_label(session: &SessionSnapshot) -> String {
    let Some(first) = session.members.first() else {
        return DASH.to_owned();
    };
    let rest = session.members.len() - 1;
    if rest == 0 {
        first.branch.clone()
    } else {
        format!("{} +{rest}", first.branch)
    }
}

/// One row the session's agent tree adds below it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NestedRow {
    /// The node's key, stable across updates.
    pub key: String,
    /// How far below the tree's root the node sits.
    pub depth: usize,
    /// Where the node is in its run.
    pub state: AgentState,
    /// A subagent's description, a dispatch run's title, `bg shell <id>`.
    pub name: String,
    /// What it runs now, or how it ended.
    pub task: String,
    /// True for a task that runs something now, drawn in the monospace face.
    pub mono: bool,
    /// Its own stuck flag, worded as the row's flags are.
    pub stuck: Option<String>,
    /// How long ago it started.
    pub age: String,
    /// True for a node that finished: its row draws dimmed.
    pub dimmed: bool,
}

/// The glyph a nested row's node shows for where it is in its run: a
/// running node as working, a queued one as the faint ring, a finished one
/// as the stopped dot, a failed one as the error dot.
pub(crate) fn agent_glyph(state: AgentState, t: &UiTheme) -> Glyph {
    let status = match state {
        AgentState::Running => SessionStatus::Working,
        AgentState::Queued => SessionStatus::Spawning,
        AgentState::Finished => SessionStatus::Stopped,
        AgentState::Failed => SessionStatus::Error,
        AgentState::Unknown => SessionStatus::Idle,
    };
    glyph(status, false, SessionMode::Interactive, false, t)
}

/// Everything one Dashboard row draws, column by column.
#[derive(Debug, Clone, PartialEq)]
pub struct RowCells {
    pub session_id: String,
    /// The Session column: the label, then the container and the branch.
    pub label: String,
    pub container: String,
    pub branch: String,
    /// The Linear column's issue key, or `—`.
    pub issue: String,
    /// The Now column's first line: the state word, how long the status has
    /// held (`None` without a stamp) and the stuck flags.
    pub state_word: String,
    pub age: Option<String>,
    pub stuck: Vec<String>,
    /// The Now column's second line.
    pub now: String,
    /// The Summary column: the newest line, then the one before with its
    /// age.
    pub summary: String,
    pub summary_older: Option<String>,
    /// The Agents column.
    pub agents: String,
    /// The Context column's figure, whether it reads hot, and the bar's
    /// fill in percent.
    pub context: String,
    pub context_hot: bool,
    pub context_percent: Option<u8>,
    /// The ten-minute bars' heights in pixels, and whether they read quiet.
    pub activity: Vec<f32>,
    pub activity_quiet: bool,
    /// The Run · last column.
    pub run: String,
    pub last: String,
    /// The Cost column: the cost, then the input tokens.
    pub cost: String,
    pub tokens: String,
    /// A row waiting on the user: washed and striped.
    pub asking: bool,
    /// The rows its agent tree adds below it: only under a Working row.
    pub nested: Vec<NestedRow>,
}

/// What the row of `input`, listed in group `kind`, draws at `now`. A
/// session without facts reads dashes and quiet bars, and only a Working
/// row carries nested rows.
pub(crate) fn row_cells(input: &RowInput<'_>, kind: GroupKind, now: DateTime<Utc>) -> RowCells {
    let facts = input.facts;
    let figure = context(facts.and_then(|facts| facts.context_percent));
    let window = facts
        .map(|facts| facts.activity.as_slice())
        .filter(|activity| !activity.is_empty())
        .unwrap_or(&QUIET_WINDOW);
    let bars = activity_bars(window);
    let (summary, summary_older) = summary_lines(input, now);
    let (run, last) = run_and_last(input, now);
    let (cost, tokens) = cost_and_tokens(facts);
    RowCells {
        session_id: input.row.session_id.clone(),
        label: input.row.label.clone(),
        container: input.row.container_name.clone(),
        branch: branch_label(input.session),
        issue: issue_key(facts),
        state_word: state_word(input),
        age: input
            .session
            .status_since
            .map(|since| format_age(since, now)),
        stuck: facts.map_or_else(Vec::new, |facts| stuck_flags(&facts.stuck)),
        now: now_text(input),
        summary,
        summary_older,
        agents: facts.map_or_else(|| DASH.to_owned(), |facts| agents_summary(&facts.agents)),
        context: figure.text,
        context_hot: figure.hot,
        context_percent: facts.and_then(|facts| facts.context_percent),
        activity: bars.heights,
        activity_quiet: bars.quiet,
        run,
        last,
        cost,
        tokens,
        asking: kind == GroupKind::NeedsYou,
        nested: match (kind, facts) {
            (GroupKind::Working, Some(facts)) => nested_rows(&facts.agents, now),
            _ => Vec::new(),
        },
    }
}

/// The header's day figures, shown only once they are known.
#[derive(Debug, Clone, PartialEq)]
pub struct HeaderFigures {
    /// `$4.10`, the day's API-equivalent cost.
    pub cost: String,
    /// `8.6M`, the day's tokens.
    pub tokens: String,
    pub commits: u32,
    pub issues_done: u32,
    pub releases: Vec<ReleaseFigure>,
    /// `Linear · 1m ago`, once the issue cache has synced.
    pub synced: Option<String>,
}

/// One open release's bar in the header.
#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseFigure {
    pub name: String,
    pub done: u32,
    pub total: u32,
    /// How much of the bar is filled, 0.0 to 1.0.
    pub share: f64,
}

/// The header's figures from the facts held, or `None` while the totals are
/// all zero and the issue cache never synced.
pub(crate) fn header_figures(store: &FactsStore, now: DateTime<Utc>) -> Option<HeaderFigures> {
    if !store.figures_known() {
        return None;
    }
    let totals = store.totals();
    Some(HeaderFigures {
        cost: format_cost(Some(totals.cost_usd)),
        tokens: format_tokens(totals.tokens),
        commits: totals.commits,
        issues_done: totals.issues_done,
        releases: totals
            .releases
            .iter()
            .map(|release| ReleaseFigure {
                name: release.name.clone(),
                done: release.done,
                total: release.total,
                share: if release.total == 0 {
                    0.0
                } else {
                    (f64::from(release.done) / f64::from(release.total)).min(1.0)
                },
            })
            .collect(),
        synced: store
            .linear_synced_at()
            .map(|at| format!("Linear · {} ago", format_age(at, now))),
    })
}

/// The rows a session's agent tree adds below it, depth first, the tree's
/// roots at depth 0. The main agent is the row itself and gets no row here.
pub(crate) fn nested_rows(agents: &[AgentNode], now: DateTime<Utc>) -> Vec<NestedRow> {
    let mut rows = Vec::new();
    push_nested(agents, 0, now, &mut rows);
    rows
}

fn push_nested(nodes: &[AgentNode], depth: usize, now: DateTime<Utc>, rows: &mut Vec<NestedRow>) {
    for node in nodes {
        rows.push(NestedRow {
            key: node.key.clone(),
            depth,
            state: node.state,
            name: node.name.clone(),
            task: node.task.clone(),
            mono: node.task.starts_with("Running"),
            stuck: node
                .stuck
                .and_then(|reason| stuck_flags(&[reason]).into_iter().next()),
            age: format_age(node.started_at, now),
            dimmed: node.state == AgentState::Finished,
        });
        push_nested(&node.children, depth + 1, now, rows);
    }
}

/// One of the header's count pills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HeaderPill {
    /// The word after the count: `need you`, `working`, `idle`, `done`.
    pub(crate) label: &'static str,
    pub(crate) count: usize,
}

/// The header's count pills, in group order.
pub(crate) fn header_pills(counts: HeaderCounts) -> [HeaderPill; 4] {
    [
        HeaderPill {
            label: "need you",
            count: counts.need_you,
        },
        HeaderPill {
            label: "working",
            count: counts.working,
        },
        HeaderPill {
            label: "idle",
            count: counts.idle,
        },
        HeaderPill {
            label: "done",
            count: counts.done,
        },
    ]
}

/// One chip of the queue strip: a session waiting on the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueChip {
    pub session_id: String,
    /// The issue key, else the session's label.
    pub title: String,
    /// The prompt on one line.
    pub prompt: String,
    /// How long it has waited.
    pub age: String,
}

/// The queue strip's chips, in the order the waits come: longest wait first.
pub(crate) fn queue_chips(waits: &[RowInput<'_>], now: DateTime<Utc>) -> Vec<QueueChip> {
    waits
        .iter()
        .map(|input| QueueChip {
            session_id: input.session.id.clone(),
            title: {
                let key = issue_key(input.facts);
                if key == DASH {
                    input.row.label.clone()
                } else {
                    key
                }
            },
            prompt: chip_prompt(input),
            age: input
                .session
                .status_since
                .map_or_else(|| DASH.to_owned(), |since| format_age(since, now)),
        })
        .collect()
}

/// A chip's prompt: a permission reads `<tool>: <command>`, anything else as
/// the row's own asking wording.
fn chip_prompt(input: &RowInput<'_>) -> String {
    let Some(PendingInput::Permission { tool_name, summary }) =
        input.session.pending_input.as_ref()
    else {
        return now_text(input);
    };
    let tool = one_line(tool_name);
    let summary = one_line(summary);
    if tool.is_empty() {
        now_text(input)
    } else if summary.is_empty() {
        tool
    } else {
        format!("{tool}: {summary}")
    }
}

/// The columns a view `view_width_px` wide shows: they drop in the order
/// Summary, Linear, the ten-minute bars, Agents as the view narrows, so the
/// view never scrolls sideways.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each column is dropped on its own width, so no two of them move together"
)]
pub struct Columns {
    pub summary: bool,
    pub linear: bool,
    pub activity: bool,
    pub agents: bool,
}

/// Which columns fit a view `view_width_px` wide.
pub(crate) fn visible_columns(view_width_px: f32) -> Columns {
    Columns {
        summary: view_width_px >= 1400.0,
        linear: view_width_px >= 1200.0,
        activity: view_width_px >= 1050.0,
        agents: view_width_px >= 950.0,
    }
}

/// The daemon's own wording of a now-line rule, when it sent one.
fn daemon_line(facts: Option<&SessionFacts>) -> Option<String> {
    facts
        .map(|facts| &facts.now)
        .filter(|now| now.kind != NowKind::Unknown && !now.text.trim().is_empty())
        .map(|now| now.text.clone())
}

/// Whether the daemon holds the session in a gate.
fn in_gate(facts: Option<&SessionFacts>) -> bool {
    facts.is_some_and(|facts| facts.now.kind == NowKind::InGate)
}

/// The newest summary's headline, when it has one.
fn headline(session: &SessionSnapshot) -> Option<String> {
    session
        .summary
        .as_ref()
        .map(|summary| summary.headline.trim())
        .filter(|headline| !headline.is_empty())
        .map(str::to_owned)
}

/// `text` with every run of whitespace as one space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use protocol::{AlertKind, SessionMember, SessionSummary, SummaryLine, SummarySource};
    use serde_json::json;

    const REPO: &str = "r1";

    fn at(minute: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + minute * 60, 0).expect("a fixed stamp")
    }

    fn session(id: &str) -> SessionSnapshot {
        serde_json::from_value(json!({
            "id": id,
            "label": id,
            "kind": "single",
            "members": [{
                "repo_id": REPO,
                "repo_name": REPO,
                "branch": "main",
                "worktree_path": "",
            }],
            "status": "idle",
            "mode": "interactive",
            "started_at": "2026-01-01T00:00:00Z",
            "exit_code": null,
            "metrics": {
                "input_tokens": 0,
                "output_tokens": 0,
                "cost_usd": 0.0,
                "last_activity_at": null,
            },
            "recent_actions": [],
            "agent": "claude",
        }))
        .expect("session fixture")
    }

    fn in_status(id: &str, status: SessionStatus) -> SessionSnapshot {
        let mut session = session(id);
        session.status = status;
        session
    }

    fn row(id: &str) -> DashboardRow {
        DashboardRow {
            session_id: id.to_owned(),
            container_name: REPO.to_owned(),
            label: id.to_owned(),
            busy_excluded: false,
        }
    }

    fn input<'a>(
        row: &'a DashboardRow,
        session: &'a SessionSnapshot,
        facts: Option<&'a SessionFacts>,
    ) -> RowInput<'a> {
        RowInput {
            row,
            session,
            facts,
            agents: &[],
            unseen: false,
        }
    }

    fn facts(id: &str) -> SessionFacts {
        serde_json::from_value(json!({
            "session_id": id,
            "now": { "kind": "unknown", "text": "" },
        }))
        .expect("facts fixture")
    }

    fn with_now(kind: &str, text: &str) -> SessionFacts {
        serde_json::from_value(json!({
            "session_id": "s1",
            "now": { "kind": kind, "text": text },
        }))
        .expect("facts fixture")
    }

    fn summary(headline: &str, lines: Vec<SummaryLine>) -> SessionSummary {
        SessionSummary {
            kind: None,
            headline: headline.to_owned(),
            lines,
            updated_at: at(0),
            source: SummarySource::Model,
        }
    }

    fn line(text: &str, minute: i64) -> SummaryLine {
        SummaryLine {
            text: text.to_owned(),
            at: at(minute),
            source: SummarySource::Model,
            provisional: false,
        }
    }

    fn node(key: &str, kind: AgentKind, state: AgentState) -> AgentNode {
        AgentNode {
            key: key.to_owned(),
            kind,
            name: key.to_owned(),
            task: String::new(),
            activity: Vec::new(),
            state,
            started_at: at(0),
            stuck: None,
            children: Vec::new(),
        }
    }

    fn member(branch: &str) -> SessionMember {
        SessionMember {
            repo_id: REPO.to_owned(),
            repo_name: REPO.to_owned(),
            branch: branch.to_owned(),
            worktree_path: String::new(),
        }
    }

    fn assert_heights(actual: &[f32], expected: &[f32], why: &str) {
        assert_eq!(actual.len(), expected.len(), "{why}: {actual:?}");
        for (got, want) in actual.iter().zip(expected) {
            assert!((got - want).abs() < 0.001, "{why}: {got} is not {want}");
        }
    }

    #[test]
    fn asking_row_uses_the_pending_prompt_line() {
        let row = row("s1");
        let mut session = in_status("s1", SessionStatus::AwaitingInput);
        session.pending_input = Some(PendingInput::Question {
            questions: vec![protocol::PendingQuestion {
                question: "Deploy it?".to_owned(),
                header: "Release".to_owned(),
                options: Vec::new(),
                multi_select: false,
            }],
        });
        assert_eq!(
            now_text(&input(&row, &session, None)),
            "Release: Deploy it?"
        );
        session.pending_input = Some(PendingInput::Permission {
            tool_name: "Bash".to_owned(),
            summary: "cargo build".to_owned(),
        });
        assert_eq!(
            now_text(&input(&row, &session, None)),
            "Allow Bash: cargo build"
        );
        session.pending_input = None;
        assert_eq!(now_text(&input(&row, &session, None)), "Waiting for input");
    }

    #[test]
    fn daemon_worded_rules_pass_through_verbatim() {
        let row = row("s1");
        let session = in_status("s1", SessionStatus::Working);
        for (kind, text) in [
            ("in_gate", "In gate · heavy 1: cargo test"),
            ("queued_for_gate", "Queued for gate · #3"),
            ("waiting_on_subagent", "Waiting on subagent \"look at it\""),
            ("running_tool", "Running Bash · 1m: cargo test"),
            ("waiting_on_background", "Waiting on a background task"),
            ("thinking", "Thinking"),
        ] {
            let facts = with_now(kind, text);
            assert_eq!(
                now_text(&input(&row, &session, Some(&facts))),
                text,
                "{kind}"
            );
        }
    }

    #[test]
    fn idle_unseen_reads_turn_finished_not_looked_at_yet() {
        let row = row("s1");
        let session = session("s1");
        let mut input = input(&row, &session, None);
        input.unseen = true;
        assert_eq!(now_text(&input), "Turn finished · not looked at yet");
        assert_eq!(state_word(&input), "Finished");
    }

    #[test]
    fn an_unseen_answer_rule_row_reads_idle_and_its_headline() {
        let row = row("s1");
        let mut session = session("s1");
        session.status_since = Some(at(0));
        let answer = |headline: &str| {
            let mut summary = summary(headline, Vec::new());
            summary.kind = Some(AlertKind::NeedsAnswer);
            summary.updated_at = at(1);
            summary
        };
        session.summary = Some(answer("Which branch should I rebase onto?"));
        {
            let mut unseen = input(&row, &session, None);
            unseen.unseen = true;
            assert_eq!(
                now_text(&unseen),
                "Which branch should I rebase onto?",
                "the Answer rule outranks the unseen turn and reads the headline"
            );
            assert_eq!(state_word(&unseen), "Idle", "an answer row reads idle");
        }
        session.summary = Some(answer("  "));
        let mut unseen = input(&row, &session, None);
        unseen.unseen = true;
        assert_eq!(
            now_text(&unseen),
            "Idle",
            "an answer row with no headline reads idle"
        );
        assert_eq!(state_word(&unseen), "Idle");
    }

    #[test]
    fn idle_reads_the_summary_headline_else_idle() {
        let row = row("s1");
        let mut session = session("s1");
        assert_eq!(now_text(&input(&row, &session, None)), "Idle");
        session.summary = Some(summary("Refactoring auth", Vec::new()));
        assert_eq!(now_text(&input(&row, &session, None)), "Refactoring auth");
        session.summary = Some(summary("  ", Vec::new()));
        assert_eq!(now_text(&input(&row, &session, None)), "Idle");
    }

    #[test]
    fn ended_reads_session_ended_exit_n() {
        let row = row("s1");
        let mut stopped = in_status("s1", SessionStatus::Stopped);
        stopped.exit_code = Some(3);
        assert_eq!(
            now_text(&input(&row, &stopped, None)),
            "Session ended · exit 3"
        );
        stopped.exit_code = None;
        assert_eq!(now_text(&input(&row, &stopped, None)), "Session ended");
        let error = in_status("s1", SessionStatus::Error);
        assert_eq!(now_text(&input(&row, &error, None)), "Error");
    }

    #[test]
    fn idle_with_background_keeps_the_daemons_background_line() {
        let row = row("s1");
        let mut session = session("s1");
        session.background = true;
        let facts = with_now("waiting_on_background", "Waiting on a background task");
        assert_eq!(
            now_text(&input(&row, &session, Some(&facts))),
            "Waiting on a background task",
            "rule 5 outranks the unseen and summary rules"
        );
        let mut unseen = input(&row, &session, Some(&facts));
        unseen.unseen = true;
        assert_eq!(now_text(&unseen), "Waiting on a background task");
        assert_eq!(
            now_text(&input(&row, &session, None)),
            "Waiting on a background task"
        );
    }

    #[test]
    fn working_without_facts_reads_thinking() {
        let row = row("s1");
        let session = in_status("s1", SessionStatus::Working);
        assert_eq!(now_text(&input(&row, &session, None)), "Thinking");
        let empty = facts("s1");
        assert_eq!(
            now_text(&input(&row, &session, Some(&empty))),
            "Thinking",
            "facts with no daemon rule read as thinking"
        );
    }

    #[test]
    fn spawning_reads_starting() {
        let row = row("s1");
        let session = in_status("s1", SessionStatus::Spawning);
        let input = input(&row, &session, None);
        assert_eq!(now_text(&input), "Starting");
        assert_eq!(state_word(&input), "Starting");
    }

    #[test]
    fn state_word_follows_status_and_prompt_kind() {
        let row = row("s1");
        let mut asking = in_status("s1", SessionStatus::AwaitingInput);
        let cases = [
            (
                Some(PendingInput::Question {
                    questions: Vec::new(),
                }),
                "Question",
            ),
            (
                Some(PendingInput::Permission {
                    tool_name: "Bash".to_owned(),
                    summary: "cargo test".to_owned(),
                }),
                "Permission",
            ),
            (Some(PendingInput::PlanApproval { plan: None }), "Plan"),
            (
                Some(PendingInput::Other {
                    message: "pick one".to_owned(),
                }),
                "Waiting",
            ),
            (None, "Waiting"),
        ];
        for (pending, want) in cases {
            asking.pending_input = pending;
            assert_eq!(state_word(&input(&row, &asking, None)), want);
        }
        let working = in_status("s1", SessionStatus::Working);
        assert_eq!(state_word(&input(&row, &working, None)), "Working");
        let gate = with_now("in_gate", "In gate · heavy 1: cargo test");
        assert_eq!(state_word(&input(&row, &working, Some(&gate))), "In gate");
        let mut background = session("s1");
        background.background = true;
        assert_eq!(state_word(&input(&row, &background, None)), "Working");
        assert_eq!(state_word(&input(&row, &session("s1"), None)), "Idle");
        let mut exited = in_status("s1", SessionStatus::Stopped);
        exited.exit_code = Some(0);
        assert_eq!(state_word(&input(&row, &exited, None)), "Exited 0");
        exited.exit_code = None;
        assert_eq!(state_word(&input(&row, &exited, None)), "Stopped");
        let error = in_status("s1", SessionStatus::Error);
        assert_eq!(state_word(&input(&row, &error, None)), "Error");
    }

    #[test]
    fn stuck_flags_word_failures_and_context_high_and_skip_unknown() {
        let flags = stuck_flags(&[
            StuckReason::ToolFailures { count: 3 },
            StuckReason::ContextHigh,
            StuckReason::Unknown,
        ]);
        assert_eq!(flags, ["⚑ 3 tool calls failed in a row", "⚑ context high"]);
        assert_eq!(stuck_flags(&[]).len(), 0);
    }

    #[test]
    fn agents_summary_counts_subs_live_dispatch_and_bg() {
        let mut running = node("subagent:1", AgentKind::Subagent, AgentState::Running);
        running.children = vec![node(
            "subagent:2",
            AgentKind::Subagent,
            AgentState::Finished,
        )];
        let tree = vec![
            running,
            node("dispatch:1", AgentKind::Dispatch, AgentState::Running),
            node("bg:1", AgentKind::Shell, AgentState::Finished),
            node("bg:2", AgentKind::Monitor, AgentState::Running),
            node("gate:1", AgentKind::Gate, AgentState::Queued),
        ];
        assert_eq!(agents_summary(&tree), "2 sub (1 live) · 1 dispatch · 3 bg");
        assert_eq!(agents_summary(&[]), "—");
        let quiet = vec![node(
            "subagent:1",
            AgentKind::Subagent,
            AgentState::Finished,
        )];
        assert_eq!(agents_summary(&quiet), "1 sub");
    }

    #[test]
    fn context_is_hot_only_above_80_and_none_reads_a_dash() {
        assert_eq!(
            context(None),
            ContextFigure {
                text: "—".to_owned(),
                hot: false
            }
        );
        assert_eq!(context(Some(80)).text, "80%");
        assert!(!context(Some(80)).hot);
        assert_eq!(context(Some(81)).text, "81%");
        assert!(context(Some(81)).hot);
    }

    #[test]
    fn activity_bar_heights_scale_with_a_floor_of_6_and_min_2px() {
        assert_heights(
            &activity_bars(&[6, 3, 0, 12]).heights,
            &[9.0, 4.5, 2.0, 18.0],
            "scaled to the tallest bar",
        );
        assert_heights(
            &activity_bars(&[1, 3]).heights,
            &[3.0, 9.0],
            "a quiet window still scales to a floor of 6",
        );
    }

    #[test]
    fn quiet_activity_is_flagged() {
        assert!(activity_bars(&[]).quiet);
        assert!(activity_bars(&[0; 10]).quiet);
        assert!(!activity_bars(&[0, 1]).quiet);
        assert_heights(
            &activity_bars(&[0; 10]).heights,
            &[2.0; 10],
            "no bar under 2px",
        );
    }

    #[test]
    fn cost_and_tokens_format() {
        assert_eq!(format_cost(None), "—");
        assert_eq!(format_cost(Some(0.0)), "$0.00", "a known zero is a cost");
        assert_eq!(format_cost(Some(0.46)), "$0.46");
        assert_eq!(format_cost(Some(0.004)), "<$0.01");
        assert_eq!(format_tokens(842), "842");
        assert_eq!(format_tokens(842_000), "842k");
        assert_eq!(format_tokens(3_100_000), "3.1M");
        assert_eq!(format_tokens(3_000_000), "3M");
        let mut facts = facts("s1");
        facts.cost_usd = 0.46;
        facts.tokens_in = 3_100_000;
        assert_eq!(
            cost_and_tokens(Some(&facts)),
            ("$0.46".to_owned(), "3.1M".to_owned())
        );
        assert_eq!(cost_and_tokens(None), ("—".to_owned(), "—".to_owned()));
    }

    #[test]
    fn ages_use_the_daemons_format() {
        assert_eq!(format_age(at(0), at(0)), "<1m");
        assert_eq!(format_age(at(0), at(1)), "1m");
        assert_eq!(format_age(at(0), at(-5)), "<1m", "a future stamp reads <1m");
        assert_eq!(format_age(at(0), at(65)), "1h05m");

        let row = row("s1");
        let mut session = session("s1");
        session.started_at = at(-70);
        let mut facts = facts("s1");
        facts.last_activity_at = Some(at(5));
        let (run, last) = run_and_last(&input(&row, &session, Some(&facts)), at(10));
        assert_eq!(run, "1h20m");
        assert_eq!(last, "5m ago");
        let (_, last) = run_and_last(&input(&row, &session, None), at(10));
        assert_eq!(last, "—", "nothing read yet");
    }

    #[test]
    fn an_ended_sessions_run_time_stops_at_its_end() {
        let row = row("s1");
        let ended = |status: SessionStatus, since: Option<DateTime<Utc>>| {
            let mut session = in_status("s1", status);
            session.started_at = at(-70);
            session.status_since = since;
            session
        };
        let stopped = ended(SessionStatus::Stopped, Some(at(-10)));
        let (run, _) = run_and_last(&input(&row, &stopped, None), at(0));
        assert_eq!(run, "1h00m", "the run stops when the session ended");

        let unstamped = ended(SessionStatus::Stopped, None);
        let (run, _) = run_and_last(&input(&row, &unstamped, None), at(0));
        assert_eq!(run, "1h10m", "with no stamp the run goes to now");

        let throttled = ended(SessionStatus::Error, Some(at(-70)));
        let (run, _) = run_and_last(&input(&row, &throttled, None), at(0));
        assert_eq!(run, "<1m", "an error stamped at its start ran no time");

        let working = ended(SessionStatus::Working, Some(at(-10)));
        let (run, _) = run_and_last(&input(&row, &working, None), at(0));
        assert_eq!(run, "1h10m", "a running session keeps counting to now");
    }

    #[test]
    fn summary_second_line_carries_its_age() {
        let row = row("s1");
        let mut session = session("s1");
        session.summary = Some(summary(
            "Refactoring",
            vec![line("Refactoring auth", 6), line("Read the module", -5)],
        ));
        let (first, second) = summary_lines(&input(&row, &session, None), at(10));
        assert_eq!(first, "Refactoring auth");
        assert_eq!(second, Some("15m · Read the module".to_owned()));
        session.summary = Some(summary("Refactoring", vec![line("Only one", 0)]));
        assert_eq!(
            summary_lines(&input(&row, &session, None), at(10)),
            ("Only one".to_owned(), None)
        );
        session.summary = None;
        assert_eq!(
            summary_lines(&input(&row, &session, None), at(10)),
            ("—".to_owned(), None)
        );
    }

    #[test]
    fn workspace_branch_shows_the_other_member_count() {
        let mut session = session("s1");
        assert_eq!(branch_label(&session), "main");
        session.members = vec![member("feat/x"), member("main")];
        assert_eq!(branch_label(&session), "feat/x +1");
        session.members = vec![member("feat/x"), member("main"), member("other")];
        assert_eq!(branch_label(&session), "feat/x +2");
        session.members.clear();
        assert_eq!(branch_label(&session), "—");
    }

    #[test]
    fn nested_rows_are_depth_first_skip_the_main_agent_and_dim_finished() {
        let mut parent = node("subagent:1", AgentKind::Subagent, AgentState::Running);
        parent.name = "look at the parser".to_owned();
        parent.task = "Running Bash · 1m: cargo test".to_owned();
        parent.stuck = Some(StuckReason::ContextHigh);
        let mut child = node("subagent:2", AgentKind::Subagent, AgentState::Finished);
        child.task = "finished · 2 findings".to_owned();
        parent.children = vec![child];
        let tree = vec![parent, node("bg:1", AgentKind::Shell, AgentState::Finished)];
        let rows = nested_rows(&tree, at(3));
        assert_eq!(rows.len(), 3, "no row for the main agent or the session");
        assert_eq!(
            rows.iter().map(|row| row.key.as_str()).collect::<Vec<_>>(),
            ["subagent:1", "subagent:2", "bg:1"]
        );
        assert_eq!(
            rows.iter().map(|row| row.depth).collect::<Vec<_>>(),
            [0, 1, 0]
        );
        assert_eq!(rows[0].name, "look at the parser");
        assert_eq!(rows[0].task, "Running Bash · 1m: cargo test");
        assert!(
            rows[0].mono,
            "a task that runs something now is drawn in mono"
        );
        assert!(!rows[1].mono, "a finished result line is not mono");
        assert!(!rows[2].mono, "a node with no task is not mono");
        assert_eq!(rows[0].stuck, Some("⚑ context high".to_owned()));
        assert_eq!(rows[0].age, "3m");
        assert!(!rows[0].dimmed, "a running node is not dimmed");
        assert!(rows[1].dimmed, "a finished node draws dimmed");
        assert!(rows[2].dimmed);
        assert_eq!(nested_rows(&[], at(0)).len(), 0);
    }

    #[test]
    fn queue_chips_follow_waits_order_and_word_permission_as_tool_colon_command() {
        assert_eq!(QUEUE_HINT, "Oldest first");
        let first_row = row("s1");
        let mut first = in_status("s1", SessionStatus::AwaitingInput);
        first.status_since = Some(at(-5));
        first.pending_input = Some(PendingInput::Permission {
            tool_name: "Bash".to_owned(),
            summary: "cargo  build".to_owned(),
        });
        let second_row = row("s2");
        let mut second = in_status("s2", SessionStatus::AwaitingInput);
        second.status_since = Some(at(-62));
        let mut second_facts = facts("s2");
        second_facts.issue = Some("RT-444".to_owned());
        let chips = queue_chips(
            &[
                input(&first_row, &first, None),
                input(&second_row, &second, Some(&second_facts)),
            ],
            at(0),
        );
        assert_eq!(
            chips
                .iter()
                .map(|chip| chip.session_id.as_str())
                .collect::<Vec<_>>(),
            ["s1", "s2"],
            "the waits' own order is kept"
        );
        assert_eq!(chips[0].prompt, "Bash: cargo build");
        assert_eq!(chips[0].title, "s1", "no issue key reads as the label");
        assert_eq!(chips[0].age, "5m");
        assert_eq!(chips[1].title, "RT-444");
        assert_eq!(chips[1].age, "1h02m");
    }

    #[test]
    fn header_pills_count_the_groups_in_order() {
        let pills = header_pills(HeaderCounts {
            need_you: 2,
            working: 3,
            idle: 1,
            done: 4,
        });
        assert_eq!(
            pills.map(|pill| (pill.label, pill.count)),
            [("need you", 2), ("working", 3), ("idle", 1), ("done", 4)]
        );
    }

    #[test]
    fn columns_drop_in_order_as_the_view_narrows() {
        let wide = visible_columns(1600.0);
        assert_eq!(
            wide,
            Columns {
                summary: true,
                linear: true,
                activity: true,
                agents: true
            }
        );
        let narrow = visible_columns(1399.0);
        assert!(!narrow.summary);
        assert!(narrow.linear && narrow.activity && narrow.agents);
        let narrower = visible_columns(1199.0);
        assert!(!narrower.summary && !narrower.linear);
        assert!(narrower.activity && narrower.agents);
        let tight = visible_columns(1049.0);
        assert!(!tight.summary && !tight.linear && !tight.activity);
        assert!(tight.agents);
        let minimal = visible_columns(900.0);
        assert!(!minimal.summary && !minimal.linear && !minimal.activity && !minimal.agents);
    }

    #[test]
    fn row_cells_fill_every_column_and_nest_only_under_working_rows() {
        let row = row("s1");
        let mut session = in_status("s1", SessionStatus::Working);
        session.started_at = at(-70);
        session.status_since = Some(at(-2));
        let mut facts = with_now("running_tool", "Running Bash · 1m: cargo test");
        facts.context_percent = Some(85);
        facts.cost_usd = 0.46;
        facts.tokens_in = 3_100_000;
        facts.activity = vec![6, 0, 12];
        facts.stuck = vec![StuckReason::ContextHigh];
        facts.issue = Some("RT-444".to_owned());
        facts.last_activity_at = Some(at(-5));
        facts.agents = vec![node("subagent:1", AgentKind::Subagent, AgentState::Running)];
        let cells = row_cells(
            &input(&row, &session, Some(&facts)),
            GroupKind::Working,
            at(0),
        );
        assert_eq!(cells.branch, "main");
        assert_eq!(cells.issue, "RT-444");
        assert_eq!(cells.state_word, "Working");
        assert_eq!(cells.age.as_deref(), Some("2m"));
        assert_eq!(cells.stuck, ["⚑ context high"]);
        assert_eq!(cells.now, "Running Bash · 1m: cargo test");
        assert_eq!(cells.agents, "1 sub (1 live)");
        assert_eq!((cells.context.as_str(), cells.context_hot), ("85%", true));
        assert_eq!(cells.context_percent, Some(85));
        assert_heights(&cells.activity, &[9.0, 2.0, 18.0], "the facts' window");
        assert_eq!(
            (cells.run.as_str(), cells.last.as_str()),
            ("1h10m", "5m ago")
        );
        assert_eq!(
            (cells.cost.as_str(), cells.tokens.as_str()),
            ("$0.46", "3.1M")
        );
        assert!(!cells.asking);
        assert_eq!(cells.nested.len(), 1, "a Working row nests its agents");

        let idle = session_with_facts_in(GroupKind::Idle, &facts);
        assert_eq!(idle.nested.len(), 0, "only a Working row nests");
        let asking = session_with_facts_in(GroupKind::NeedsYou, &facts);
        assert!(asking.asking, "a Needs you row is washed");
    }

    fn session_with_facts_in(kind: GroupKind, facts: &SessionFacts) -> RowCells {
        let row = row("s1");
        let session = session("s1");
        row_cells(&input(&row, &session, Some(facts)), kind, at(0))
    }

    #[test]
    fn row_cells_without_facts_read_dashes_and_quiet_bars() {
        let row = row("s1");
        let session = in_status("s1", SessionStatus::Working);
        let cells = row_cells(&input(&row, &session, None), GroupKind::Working, at(0));
        assert_eq!(cells.context, "—");
        assert_eq!(cells.agents, "—");
        assert_eq!((cells.cost.as_str(), cells.tokens.as_str()), ("—", "—"));
        assert_eq!(cells.issue, "—");
        assert_eq!(cells.age, None);
        assert_eq!(cells.now, "Thinking");
        assert!(cells.activity_quiet);
        assert_heights(&cells.activity, &[2.0; 10], "ten quiet bars");
        assert_eq!(cells.nested.len(), 0);
    }

    #[test]
    fn header_figures_hide_until_known() {
        let mut store = FactsStore::default();
        assert_eq!(header_figures(&store, at(0)), None);
        store.apply(&protocol::DaemonMessage::DashboardFacts {
            facts: Vec::new(),
            totals: protocol::DayTotals {
                cost_usd: 4.1,
                tokens: 8_600_000,
                commits: 14,
                issues_done: 5,
                releases: vec![protocol::ReleaseProgress {
                    name: "vNext".to_owned(),
                    done: 11,
                    total: 22,
                }],
            },
            linear_synced_at: Some(at(-1)),
        });
        let figures = header_figures(&store, at(0)).expect("known figures");
        assert_eq!(figures.cost, "$4.10");
        assert_eq!(figures.tokens, "8.6M");
        assert_eq!((figures.commits, figures.issues_done), (14, 5));
        assert_eq!(figures.releases[0].name, "vNext");
        assert!((figures.releases[0].share - 0.5).abs() < 1e-9);
        assert_eq!(figures.synced.as_deref(), Some("Linear · 1m ago"));
    }

    #[test]
    fn agent_glyphs_follow_the_nodes_state() {
        use crate::status_glyph::Shape;
        let shapes = [
            AgentState::Running,
            AgentState::Queued,
            AgentState::Finished,
            AgentState::Failed,
            AgentState::Unknown,
        ]
        .map(|state| agent_glyph(state, &UiTheme::CONTRAST).shape);
        assert_eq!(
            shapes,
            [
                Shape::Working,
                Shape::Spawning,
                Shape::Stopped,
                Shape::Error,
                Shape::Idle
            ]
        );
    }
}
