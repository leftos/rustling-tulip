//! The daemon's part of a Dashboard card's now line: what a session is doing
//! this second, highest rule first. The daemon words the rules its own data
//! decides (a running subagent, an open tool call, background work, thinking);
//! the cases only the client knows (a pending prompt, an unseen turn, the
//! summary, an ended session) go out as [`NowKind::Unknown`] with no text.
//!
//! Every function takes `now`, and ages are worded in whole minutes, so a
//! line's text changes at most once a minute.

use agent_api::{ActivitySnapshot, OpenCall, SubagentNode};
use chrono::{DateTime, Utc};
use protocol::{AgentState, NowKind, NowLine, SessionStatus};

/// The most characters a command keeps on a now line or a node's task.
pub(crate) const COMMAND_CHARS: usize = 80;

/// What the now line is read from: the session's registry status and the
/// transcript's latest snapshot, if one was read.
pub(crate) struct NowInput<'a> {
    pub(crate) status: SessionStatus,
    pub(crate) status_since: Option<DateTime<Utc>>,
    /// The session snapshot's `background` flag: an idle session whose last
    /// stop left background work running.
    pub(crate) background: bool,
    pub(crate) snapshot: Option<&'a ActivitySnapshot>,
}

/// The now line for `input` as of `now`.
pub(crate) fn now_line(input: &NowInput<'_>, now: DateTime<Utc>) -> NowLine {
    match input.status {
        SessionStatus::Working => working_line(input, now),
        SessionStatus::Idle if input.background => waiting_on_background(
            input
                .snapshot
                .and_then(|snapshot| snapshot.background.first())
                .and_then(|task| task.started)
                .or(input.status_since),
        ),
        _ => client_worded(input.status_since),
    }
}

/// A working session: a running subagent, else its oldest open call, else
/// background work, else thinking.
fn working_line(input: &NowInput<'_>, now: DateTime<Utc>) -> NowLine {
    let Some(snapshot) = input.snapshot else {
        return thinking(input.status_since);
    };
    if let Some(node) = first_running(&snapshot.subagents) {
        return NowLine {
            kind: NowKind::WaitingOnSubagent,
            text: format!("Waiting on subagent \"{}\"", subagent_name(node)),
            since: node.started_at.or(input.status_since),
        };
    }
    if let Some(call) = snapshot.open_calls.first() {
        return NowLine {
            kind: NowKind::RunningTool,
            text: running_text(call, now),
            since: Some(call.started),
        };
    }
    if let Some(task) = snapshot.background.first() {
        return waiting_on_background(task.started.or(input.status_since));
    }
    thinking(input.status_since)
}

fn waiting_on_background(since: Option<DateTime<Utc>>) -> NowLine {
    NowLine {
        kind: NowKind::WaitingOnBackground,
        text: "Waiting on a background task".to_owned(),
        since,
    }
}

fn thinking(since: Option<DateTime<Utc>>) -> NowLine {
    NowLine {
        kind: NowKind::Thinking,
        text: "Thinking".to_owned(),
        since,
    }
}

/// A rule the client words: waiting on the user, idle, ended.
fn client_worded(since: Option<DateTime<Utc>>) -> NowLine {
    NowLine {
        kind: NowKind::Unknown,
        text: String::new(),
        since,
    }
}

/// The first running subagent of the tree, depth first, at any depth: a
/// running subagent under a finished launcher still counts.
pub(crate) fn first_running(nodes: &[SubagentNode]) -> Option<&SubagentNode> {
    nodes.iter().find_map(|node| {
        if node.state == AgentState::Running {
            Some(node)
        } else {
            first_running(&node.children)
        }
    })
}

/// A subagent's name: its description, else its type, else its id.
pub(crate) fn subagent_name(node: &SubagentNode) -> &str {
    node.description
        .as_deref()
        .or(node.agent_type.as_deref())
        .unwrap_or(&node.agent_id)
}

/// `Running <tool> · <age>: <command>`, or without the command for a call
/// that takes none.
pub(crate) fn running_text(call: &OpenCall, now: DateTime<Utc>) -> String {
    let age = age(call.started, now);
    match call.command.as_deref().map(one_line) {
        Some(command) if !command.is_empty() => {
            format!("Running {} · {age}: {command}", call.name)
        }
        _ => format!("Running {} · {age}", call.name),
    }
}

/// How long ago `since` was, in whole minutes: `<1m`, `5m`, `1h05m`. A time
/// after `now` reads `<1m`.
pub(crate) fn age(since: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let minutes = (now - since).num_minutes();
    if minutes < 1 {
        return "<1m".to_owned();
    }
    if minutes < 60 {
        return format!("{minutes}m");
    }
    format!("{}h{:02}m", minutes / 60, minutes % 60)
}

/// `text` with every run of whitespace made one space, trimmed, and cut at
/// [`COMMAND_CHARS`] characters.
pub(crate) fn one_line(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(COMMAND_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use agent_api::{ActivitySnapshot, BackgroundTask, OpenCall, SubagentNode};
    use chrono::{DateTime, Duration, TimeZone as _, Utc};
    use protocol::{AgentState, NowKind, SessionStatus};

    use super::{NowInput, age, now_line, one_line};

    fn at(minute: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 28, 12, 0, 0)
            .single()
            .unwrap_or_default()
            + Duration::minutes(minute)
    }

    fn call(name: &str, command: Option<&str>, started: DateTime<Utc>) -> OpenCall {
        OpenCall {
            id: format!("call-{name}"),
            name: name.to_owned(),
            command: command.map(str::to_owned),
            started,
        }
    }

    fn subagent(id: &str, state: AgentState) -> SubagentNode {
        SubagentNode {
            agent_id: id.to_owned(),
            parent_agent_id: None,
            agent_type: Some("Explore".to_owned()),
            description: Some(format!("look at {id}")),
            started_at: Some(at(1)),
            finished_at: None,
            state,
            end_status: None,
            open_call: None,
            last_text: None,
            failure_run: 0,
            activity: [0; agent_api::MINUTE_COLUMNS],
            children: Vec::new(),
        }
    }

    fn shell(id: &str) -> BackgroundTask {
        BackgroundTask {
            id: id.to_owned(),
            tool_use_id: None,
            started: Some(at(2)),
            command: Some("cargo test".to_owned()),
        }
    }

    fn line_for(
        status: SessionStatus,
        background: bool,
        snapshot: &ActivitySnapshot,
    ) -> protocol::NowLine {
        let input = NowInput {
            status,
            status_since: Some(at(0)),
            background,
            snapshot: Some(snapshot),
        };
        now_line(&input, at(10))
    }

    fn assert_client_worded(line: &protocol::NowLine) {
        assert_eq!(line.kind, NowKind::Unknown, "{line:?}");
        assert!(line.text.is_empty(), "{line:?}");
        assert_eq!(line.since, Some(at(0)), "{line:?}");
    }

    #[test]
    fn an_asking_session_leaves_its_now_line_to_the_client() {
        let snapshot = ActivitySnapshot {
            open_calls: vec![call("Bash", Some("ls"), at(1))],
            ..ActivitySnapshot::default()
        };
        assert_client_worded(&line_for(SessionStatus::AwaitingInput, false, &snapshot));
    }

    #[test]
    fn an_idle_session_without_background_leaves_its_now_line_to_the_client() {
        let snapshot = ActivitySnapshot {
            background: vec![shell("b1")],
            ..ActivitySnapshot::default()
        };
        assert_client_worded(&line_for(SessionStatus::Idle, false, &snapshot));
    }

    #[test]
    fn an_ended_session_leaves_its_now_line_to_the_client() {
        let snapshot = ActivitySnapshot::default();
        assert_client_worded(&line_for(SessionStatus::Stopped, false, &snapshot));
        assert_client_worded(&line_for(SessionStatus::Error, true, &snapshot));
    }

    #[test]
    fn a_working_session_with_a_running_subagent_waits_on_it_by_description() {
        let snapshot = ActivitySnapshot {
            subagents: vec![
                subagent("a0", AgentState::Finished),
                subagent("a1", AgentState::Running),
            ],
            open_calls: vec![call("Agent", None, at(1))],
            ..ActivitySnapshot::default()
        };
        let line = line_for(SessionStatus::Working, false, &snapshot);
        assert_eq!(line.kind, NowKind::WaitingOnSubagent);
        assert_eq!(line.text, "Waiting on subagent \"look at a1\"");
        assert_eq!(line.since, Some(at(1)));
    }

    #[test]
    fn a_subagent_with_no_description_is_worded_by_its_type() {
        let mut node = subagent("a1", AgentState::Running);
        node.description = None;
        let snapshot = ActivitySnapshot {
            subagents: vec![node],
            ..ActivitySnapshot::default()
        };
        let line = line_for(SessionStatus::Working, false, &snapshot);
        assert_eq!(line.text, "Waiting on subagent \"Explore\"");
    }

    #[test]
    fn a_nested_running_subagent_counts_when_its_launcher_is_finished() {
        let mut launcher = subagent("a1", AgentState::Finished);
        launcher.children = vec![subagent("a2", AgentState::Running)];
        let snapshot = ActivitySnapshot {
            subagents: vec![launcher],
            ..ActivitySnapshot::default()
        };
        let line = line_for(SessionStatus::Working, false, &snapshot);
        assert_eq!(line.text, "Waiting on subagent \"look at a2\"");
    }

    #[test]
    fn a_finished_subagent_does_not_hold_a_working_session() {
        let snapshot = ActivitySnapshot {
            subagents: vec![
                subagent("a1", AgentState::Finished),
                subagent("a2", AgentState::Failed),
            ],
            ..ActivitySnapshot::default()
        };
        let line = line_for(SessionStatus::Working, false, &snapshot);
        assert_eq!(line.kind, NowKind::Thinking);
    }

    #[test]
    fn a_working_session_with_an_open_call_runs_the_oldest_with_its_age_and_command() {
        let snapshot = ActivitySnapshot {
            open_calls: vec![
                call("Bash", Some("cargo test"), at(3)),
                call("Read", None, at(9)),
            ],
            ..ActivitySnapshot::default()
        };
        let line = line_for(SessionStatus::Working, false, &snapshot);
        assert_eq!(line.kind, NowKind::RunningTool);
        assert_eq!(line.text, "Running Bash · 7m: cargo test");
        assert_eq!(line.since, Some(at(3)));
    }

    #[test]
    fn the_command_collapses_whitespace_and_is_cut_at_80_characters() {
        let long = format!("echo   {}\n\tdone", "x".repeat(100));
        let cut = one_line(&long);
        assert_eq!(cut.chars().count(), 80);
        assert!(cut.starts_with("echo xxx"), "{cut}");
        assert_eq!(one_line("  a \n\t b  "), "a b");
    }

    #[test]
    fn an_open_call_with_no_command_reads_running_tool_and_age() {
        let snapshot = ActivitySnapshot {
            open_calls: vec![call("WebFetch", None, at(10))],
            ..ActivitySnapshot::default()
        };
        let line = line_for(SessionStatus::Working, false, &snapshot);
        assert_eq!(line.text, "Running WebFetch · <1m");
    }

    #[test]
    fn a_working_session_with_only_a_background_shell_waits_on_a_background_task() {
        let snapshot = ActivitySnapshot {
            background: vec![shell("b1")],
            ..ActivitySnapshot::default()
        };
        let line = line_for(SessionStatus::Working, false, &snapshot);
        assert_eq!(line.kind, NowKind::WaitingOnBackground);
        assert_eq!(line.text, "Waiting on a background task");
        assert_eq!(line.since, Some(at(2)));
    }

    #[test]
    fn an_idle_session_with_the_background_flag_waits_on_a_background_task() {
        let line = line_for(SessionStatus::Idle, true, &ActivitySnapshot::default());
        assert_eq!(line.kind, NowKind::WaitingOnBackground);
        assert_eq!(line.since, Some(at(0)));
    }

    #[test]
    fn an_idle_background_line_dates_from_the_oldest_background_task() {
        let mut later = shell("b2");
        later.started = Some(at(4));
        let snapshot = ActivitySnapshot {
            background: vec![shell("b1"), later],
            ..ActivitySnapshot::default()
        };
        let line = line_for(SessionStatus::Idle, true, &snapshot);
        assert_eq!(line.kind, NowKind::WaitingOnBackground);
        assert_eq!(line.since, Some(at(2)));
    }

    #[test]
    fn an_ended_background_shell_does_not_count() {
        let snapshot = ActivitySnapshot {
            ended_background: vec![agent_api::EndedTask {
                task: shell("b1"),
                state: AgentState::Finished,
                status: Some("completed".to_owned()),
                summary: None,
                ended_at: Some(at(5)),
            }],
            ..ActivitySnapshot::default()
        };
        let line = line_for(SessionStatus::Working, false, &snapshot);
        assert_eq!(line.kind, NowKind::Thinking);
    }

    #[test]
    fn a_working_session_with_nothing_open_is_thinking() {
        let line = line_for(SessionStatus::Working, false, &ActivitySnapshot::default());
        assert_eq!(line.kind, NowKind::Thinking);
        assert_eq!(line.text, "Thinking");
        assert_eq!(line.since, Some(at(0)));
        let unread = NowInput {
            status: SessionStatus::Working,
            status_since: Some(at(0)),
            background: false,
            snapshot: None,
        };
        assert_eq!(now_line(&unread, at(10)).kind, NowKind::Thinking);
    }

    #[test]
    fn a_running_subagent_outranks_an_open_call_which_outranks_background_work() {
        let mut snapshot = ActivitySnapshot {
            subagents: vec![subagent("a1", AgentState::Running)],
            open_calls: vec![call("Bash", Some("ls"), at(3))],
            background: vec![shell("b1")],
            ..ActivitySnapshot::default()
        };
        let kind =
            |snapshot: &ActivitySnapshot| line_for(SessionStatus::Working, false, snapshot).kind;
        assert_eq!(kind(&snapshot), NowKind::WaitingOnSubagent);
        snapshot.subagents.clear();
        assert_eq!(kind(&snapshot), NowKind::RunningTool);
        snapshot.open_calls.clear();
        assert_eq!(kind(&snapshot), NowKind::WaitingOnBackground);
    }

    #[test]
    fn age_reads_under_a_minute_in_minutes_and_hours_after_an_hour() {
        assert_eq!(age(at(0), at(0) + Duration::seconds(59)), "<1m");
        assert_eq!(age(at(5), at(0)), "<1m");
        assert_eq!(age(at(0), at(1)), "1m");
        assert_eq!(age(at(0), at(59)), "59m");
        assert_eq!(age(at(0), at(65)), "1h05m");
        assert_eq!(age(at(0), at(185)), "3h05m");
    }

    #[test]
    fn the_since_of_each_rule_is_stable_across_reads() {
        let snapshot = ActivitySnapshot {
            open_calls: vec![call("Bash", Some("ls"), at(3))],
            ..ActivitySnapshot::default()
        };
        let input = NowInput {
            status: SessionStatus::Working,
            status_since: Some(at(0)),
            background: false,
            snapshot: Some(&snapshot),
        };
        let first = now_line(&input, at(4));
        let later = now_line(&input, at(4) + Duration::seconds(30));
        assert_eq!(first, later, "within a minute nothing on the line changes");
        let next_minute = now_line(&input, at(5));
        assert_eq!(next_minute.since, first.since);
        assert_ne!(next_minute.text, first.text);
    }
}
