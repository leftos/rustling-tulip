//! The Needs You list: which sessions need the user, why, and for how long.
//!
//! Plain Rust over the sidebar model, so the list is unit-testable without
//! GPUI; the view in `needs_you_view` draws these rows.

use crate::appearance;
use crate::sidebar::{Leaf, LeafState, SidebarModel, session_title};
use chrono::{DateTime, Utc};
use protocol::{
    AgentDescriptor, AlertKind, PendingInput, PendingQuestion, SessionSnapshot, SessionStatus,
};

/// Why a session is listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reason {
    /// The session is waiting for the user to answer.
    Asking,
    /// The session ended with its leaf still flagged for the user.
    Ended,
}

/// One row of the list.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NeedsYouRow {
    pub(crate) session_id: String,
    /// The sidebar container the session sits in.
    pub(crate) container_name: String,
    /// The session's resolved accent, `0xRRGGBB`: the colour the session's
    /// sidebar leaf draws, so its own override wins over its container's.
    pub(crate) accent: u32,
    /// The label the sidebar leaf shows.
    pub(crate) label: String,
    pub(crate) reason: Reason,
    /// When the status last changed; `None` from a daemon that predates it.
    pub(crate) since: Option<DateTime<Utc>>,
    /// Line two: what the session wants, as specifically as this build knows.
    pub(crate) detail: String,
}

/// The sessions needing the user, in list order: the longest wait first, the
/// rows with no stamp after those, the ended ones last. Ties keep the
/// sidebar's order. Parked and abandoned sessions are never listed.
pub(crate) fn rows(model: &SidebarModel) -> Vec<NeedsYouRow> {
    let accents = model.session_accents();
    let mut asking = Vec::new();
    let mut ended = Vec::new();
    for container in model.containers() {
        for leaf in &container.leaves {
            let Some(session) = model.session(&leaf.id) else {
                continue;
            };
            let Some(reason) = reason_of(leaf) else {
                continue;
            };
            let row = NeedsYouRow {
                session_id: session.id.clone(),
                container_name: container.name.clone(),
                accent: accents
                    .get(session.id.as_str())
                    .copied()
                    .unwrap_or(appearance::BUILTIN_ACCENT),
                label: leaf.label.clone(),
                reason,
                since: session.status_since,
                detail: detail_of(reason, session, model.agents()),
            };
            match reason {
                Reason::Asking => asking.push(row),
                Reason::Ended => ended.push(row),
            }
        }
    }
    asking.sort_by_key(|row| (row.since.is_none(), row.since));
    asking.extend(ended);
    asking
}

/// Whether the session of `leaf` is in the list; the sidebar's Needs you on
/// top sort lifts a container by this same test.
pub(crate) fn is_listed(leaf: &Leaf) -> bool {
    reason_of(leaf).is_some()
}

/// The Answer rule: the session is idle and its summary says it waits for the
/// user's answer, made at or after the stop it is about. A session with no
/// status stamp never matches, since its summary cannot be tied to this stop.
pub(crate) fn is_answer(session: &SessionSnapshot) -> bool {
    let Some(since) = session.status_since else {
        return false;
    };
    session.status == SessionStatus::Idle
        && session.summary.as_ref().is_some_and(|summary| {
            summary.kind == Some(AlertKind::NeedsAnswer) && summary.updated_at >= since
        })
}

/// Why the session of `leaf` is listed, or `None` when it is not. A parked or
/// abandoned session never is. Asking comes from the status, so a cleared
/// attention leaves the row; an ended session is listed only while its leaf
/// is still flagged.
fn reason_of(leaf: &Leaf) -> Option<Reason> {
    if matches!(
        leaf.state,
        LeafState::Inactive { .. } | LeafState::Abandoned { .. }
    ) {
        return None;
    }
    match leaf.status {
        SessionStatus::AwaitingInput => Some(Reason::Asking),
        SessionStatus::Error | SessionStatus::Stopped if leaf.attention => Some(Reason::Ended),
        _ => None,
    }
}

/// Line two for a listed session; `agents` says how its terminal title reads.
fn detail_of(reason: Reason, session: &SessionSnapshot, agents: &[AgentDescriptor]) -> String {
    match reason {
        Reason::Asking => session
            .pending_input
            .as_ref()
            .and_then(pending_line)
            .unwrap_or_else(|| waiting_line(session, agents)),
        Reason::Ended if session.status == SessionStatus::Error => "Error".to_owned(),
        Reason::Ended => session.exit_code.map_or_else(
            || "Stopped".to_owned(),
            |code| format!("Exited with code {code}"),
        ),
    }
}

/// Line two when no hook has said what the session wants: the waiting text,
/// with the terminal title when it names something (see [`session_title`]).
fn waiting_line(session: &SessionSnapshot, agents: &[AgentDescriptor]) -> String {
    session_title(session, agents).map_or_else(
        || "Waiting for input".to_owned(),
        |title| format!("Waiting for input · {title}"),
    )
}

/// Line two from the prompt the agent's hooks last reported, or `None` when
/// the prompt cannot be said in one line.
fn pending_line(pending: &PendingInput) -> Option<String> {
    match pending {
        PendingInput::Question { questions } => question_line(questions),
        PendingInput::Permission { tool_name, summary } => permission_line(tool_name, summary),
        PendingInput::PlanApproval { plan } => Some(plan_line(plan.as_deref())),
        PendingInput::Other { message } => {
            let message = one_line(message);
            (!message.is_empty()).then_some(message)
        }
        PendingInput::Unknown => None,
    }
}

/// The first question that has text, as `header: question`, with a count of
/// the questions after it when the prompt asks several.
fn question_line(questions: &[PendingQuestion]) -> Option<String> {
    let (header, question) = questions
        .iter()
        .map(|question| (one_line(&question.header), one_line(&question.question)))
        .find(|(_, question)| !question.is_empty())?;
    let line = if header.is_empty() {
        question
    } else {
        format!("{header}: {question}")
    };
    let rest = questions.len() - 1;
    if rest == 0 {
        Some(line)
    } else {
        Some(format!("{line} +{rest} more"))
    }
}

/// What one tool call would do, named by the tool.
fn permission_line(tool_name: &str, summary: &str) -> Option<String> {
    let tool = one_line(tool_name);
    if tool.is_empty() {
        return None;
    }
    let summary = one_line(summary);
    if summary.is_empty() {
        Some(format!("Allow {tool}"))
    } else {
        Some(format!("Allow {tool}: {summary}"))
    }
}

/// A plan approval, named by the plan's first line.
fn plan_line(plan: Option<&str>) -> String {
    plan.and_then(first_line).map_or_else(
        || "Approve plan".to_owned(),
        |line| format!("Approve plan: {line}"),
    )
}

/// The first line of `text` that holds anything but whitespace, as one line.
fn first_line(text: &str) -> Option<String> {
    text.lines().map(one_line).find(|line| !line.is_empty())
}

/// `text` with every run of whitespace, newlines included, as one space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// How long a wait reads: whole seconds below a minute, whole minutes below
/// an hour, then hours and minutes ("1h 5m", "2h" on the hour).
pub(crate) fn waited(now: DateTime<Utc>, since: DateTime<Utc>) -> String {
    let seconds = (now - since).num_seconds().max(0);
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    let rest = minutes % 60;
    if rest == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h {rest}m")
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use protocol::{AttentionReason, DaemonMessage, PendingInput, PendingQuestion, RepoEntry};
    use serde_json::json;

    /// The repo every fixture session belongs to.
    const REPO: &str = "r1";

    fn repo() -> RepoEntry {
        serde_json::from_value(json!({ "id": REPO, "name": REPO, "path": "C:/r1" }))
            .expect("repo fixture")
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

    fn asking(id: &str) -> SessionSnapshot {
        let mut s = session(id);
        s.status = SessionStatus::AwaitingInput;
        s
    }

    /// A session waiting for an answer, with `pending` as the prompt its
    /// agent's hooks last reported.
    fn asking_with(id: &str, pending: PendingInput) -> SessionSnapshot {
        let mut s = asking(id);
        s.pending_input = Some(pending);
        s
    }

    fn question(header: &str, text: &str) -> PendingQuestion {
        PendingQuestion {
            question: text.to_owned(),
            header: header.to_owned(),
            options: Vec::new(),
            multi_select: false,
        }
    }

    fn ended(id: &str, status: SessionStatus, exit_code: Option<i32>) -> SessionSnapshot {
        let mut s = session(id);
        s.status = status;
        s.exit_code = exit_code;
        s
    }

    /// A fixed stamp `offset` seconds from mid-November 2023, so the tests
    /// never read the clock.
    fn stamp(offset: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + offset, 0).expect("a fixed stamp")
    }

    fn list(sessions: Vec<SessionSnapshot>) -> SidebarModel {
        let mut model = SidebarModel::default();
        model.apply(&DaemonMessage::Repos {
            repos: vec![repo()],
        });
        model.apply(&DaemonMessage::Sessions { sessions });
        model
    }

    fn attend(model: &mut SidebarModel, id: &str) {
        model.apply(&DaemonMessage::Attention {
            session_id: id.to_owned(),
            reason: AttentionReason::Error,
        });
    }

    fn ids(listed: &[NeedsYouRow]) -> Vec<&str> {
        listed.iter().map(|row| row.session_id.as_str()).collect()
    }

    #[test]
    fn awaiting_input_is_listed_as_asking() {
        let listed = rows(&list(vec![asking("s1")]));
        assert_eq!(ids(&listed), ["s1"]);
        let row = &listed[0];
        assert_eq!(row.container_name, REPO);
        assert_eq!(row.accent, appearance::BUILTIN_ACCENT);
        assert_eq!(row.label, "s1");
        assert_eq!(row.reason, Reason::Asking);
        assert_eq!(row.detail, "Waiting for input");
        assert_eq!(row.since, None, "the fixture carries no stamp");
    }

    #[test]
    #[expect(clippy::unreadable_literal, reason = "hex colors read as #rrggbb")]
    fn accent_follows_the_session_override() {
        let mut accented = asking("s1");
        accented.appearance.accent_color = Some("#38bdf8".to_owned());
        let listed = rows(&list(vec![accented]));
        assert_eq!(
            listed[0].accent, 0x38bdf8,
            "the row wears the session's own colour, as its leaf does"
        );
    }

    #[test]
    fn asking_row_survives_cleared_attention() {
        let mut model = list(vec![asking("s1")]);
        attend(&mut model, "s1");
        model.clear_attention("s1");
        assert_eq!(ids(&rows(&model)), ["s1"], "the question still needs you");
    }

    #[test]
    fn error_in_attention_is_listed_as_ended() {
        let mut model = list(vec![ended("s1", SessionStatus::Error, None)]);
        attend(&mut model, "s1");
        let listed = rows(&model);
        assert_eq!(ids(&listed), ["s1"]);
        assert_eq!(listed[0].reason, Reason::Ended);
        assert_eq!(listed[0].detail, "Error");
    }

    #[test]
    fn an_excluded_ended_session_is_still_listed() {
        let mut excluded = ended("s1", SessionStatus::Error, None);
        excluded.busy_excluded = true;
        let mut model = list(vec![excluded]);
        attend(&mut model, "s1");
        let listed = rows(&model);
        assert_eq!(ids(&listed), ["s1"]);
        assert_eq!(listed[0].reason, Reason::Ended);
    }

    #[test]
    fn an_excluded_asking_session_is_still_listed() {
        let mut excluded = asking("s1");
        excluded.busy_excluded = true;
        let listed = rows(&list(vec![excluded]));
        assert_eq!(ids(&listed), ["s1"]);
        assert_eq!(listed[0].reason, Reason::Asking);
    }

    #[test]
    fn stopped_without_attention_is_not_listed() {
        let model = list(vec![ended("s1", SessionStatus::Stopped, Some(0))]);
        assert_eq!(rows(&model), [] as [crate::needs_you::NeedsYouRow; 0]);
    }

    #[test]
    fn parked_and_abandoned_are_never_listed() {
        let mut parked = asking("s1");
        parked.is_inactive = true;
        let mut abandoned = asking("s2");
        abandoned.is_abandoned = true;
        let mut parked_error = ended("s3", SessionStatus::Error, None);
        parked_error.is_inactive = true;
        let mut model = list(vec![parked, abandoned, parked_error]);
        attend(&mut model, "s3");
        assert_eq!(rows(&model), [] as [crate::needs_you::NeedsYouRow; 0]);
    }

    #[test]
    fn working_and_idle_are_not_listed() {
        let mut working = session("s1");
        working.status = SessionStatus::Working;
        let idle = session("s2");
        let mut model = list(vec![working, idle]);
        attend(&mut model, "s2");
        assert!(
            rows(&model).is_empty(),
            "attention alone does not list a session that is neither waiting nor ended"
        );
    }

    #[test]
    fn a_background_session_never_needs_you() {
        let mut bg = session("s1");
        bg.background = true;
        let mut model = list(vec![bg]);
        assert!(rows(&model).is_empty(), "background work is not a question");
        attend(&mut model, "s1");
        assert!(
            rows(&model).is_empty(),
            "attention alone does not list an idle session with background work"
        );
    }

    #[test]
    fn asking_rows_oldest_first_then_unknown_since_then_ended() {
        let mut oldest = asking("s1");
        oldest.status_since = Some(stamp(-300));
        let mut recent = asking("s2");
        recent.status_since = Some(stamp(-60));
        let unknown = asking("s3");
        let mut errored = ended("s4", SessionStatus::Error, None);
        errored.status_since = Some(stamp(-600));
        let mut model = list(vec![oldest, recent, unknown, errored]);
        attend(&mut model, "s4");

        let listed = rows(&model);
        assert_eq!(ids(&listed), ["s1", "s2", "s3", "s4"]);
        assert_eq!(listed[0].since, Some(stamp(-300)));
        assert_eq!(listed[2].since, None, "an unstamped wait sorts last");
        assert_eq!(listed[3].reason, Reason::Ended);
    }

    #[test]
    fn ended_row_carries_status_since() {
        let mut stopped = ended("s1", SessionStatus::Stopped, Some(1));
        stopped.status_since = Some(stamp(-90));
        let mut model = list(vec![stopped]);
        attend(&mut model, "s1");
        let listed = rows(&model);
        assert_eq!(listed[0].reason, Reason::Ended);
        assert_eq!(
            listed[0].since,
            Some(stamp(-90)),
            "an ended row reads the time since it ended"
        );
    }

    #[test]
    fn ties_keep_sidebar_order() {
        let mut later = asking("b");
        later.status_since = Some(stamp(0));
        let mut earlier = asking("a");
        earlier.status_since = Some(stamp(0));
        let listed = rows(&list(vec![later, earlier]));
        assert_eq!(ids(&listed), ["a", "b"]);
    }

    #[test]
    fn asking_detail_with_and_without_terminal_title() {
        let bare = asking("s1");
        let mut titled = asking("s2");
        titled.terminal_title = Some("Running the migration".to_owned());
        let listed = rows(&list(vec![bare, titled]));
        assert_eq!(listed[0].detail, "Waiting for input");
        assert_eq!(
            listed[1].detail,
            "Waiting for input · Running the migration"
        );
    }

    #[test]
    fn waiting_line_drops_the_glyph() {
        let mut working = asking("s1");
        working.terminal_title = Some("◑ Running the migration".to_owned());
        let mut idle = asking("s2");
        idle.terminal_title = Some("✳ Claude Code".to_owned());
        let mut model = list(vec![working, idle]);
        model.set_agents(agent_registry::descriptors());
        let listed = rows(&model);
        assert_eq!(
            listed[0].detail,
            "Waiting for input · Running the migration"
        );
        assert_eq!(
            listed[1].detail, "Waiting for input",
            "a placeholder title says nothing"
        );
    }

    #[test]
    fn question_detail_is_header_and_first_question() {
        let pending = PendingInput::Question {
            questions: vec![question("Migration", "keep newest, merge oldest, or skip?")],
        };
        let listed = rows(&list(vec![asking_with("s1", pending)]));
        assert_eq!(
            listed[0].detail,
            "Migration: keep newest, merge oldest, or skip?"
        );
    }

    #[test]
    fn question_without_header_shows_the_bare_question() {
        let pending = PendingInput::Question {
            questions: vec![question("", "keep newest, merge oldest, or skip?")],
        };
        let listed = rows(&list(vec![asking_with("s1", pending)]));
        assert_eq!(listed[0].detail, "keep newest, merge oldest, or skip?");
    }

    #[test]
    fn several_questions_add_a_plus_n_more_suffix() {
        let pending = PendingInput::Question {
            questions: vec![
                question("Migration", "keep newest, merge oldest, or skip?"),
                question("Backup", "keep a copy first?"),
                question("Downtime", "is a restart acceptable?"),
            ],
        };
        let listed = rows(&list(vec![asking_with("s1", pending)]));
        assert_eq!(
            listed[0].detail,
            "Migration: keep newest, merge oldest, or skip? +2 more"
        );
    }

    #[test]
    fn question_with_no_questions_falls_back_to_waiting_for_input() {
        let listed = rows(&list(vec![asking_with(
            "s1",
            PendingInput::Question {
                questions: Vec::new(),
            },
        )]));
        assert_eq!(listed[0].detail, "Waiting for input");
    }

    #[test]
    fn permission_detail_names_the_tool_and_what_it_runs() {
        let pending = PendingInput::Permission {
            tool_name: "Bash".to_owned(),
            summary: "cargo build".to_owned(),
        };
        let listed = rows(&list(vec![asking_with("s1", pending)]));
        assert_eq!(listed[0].detail, "Allow Bash: cargo build");
    }

    #[test]
    fn permission_with_an_empty_summary_names_only_the_tool() {
        let pending = PendingInput::Permission {
            tool_name: "Write".to_owned(),
            summary: String::new(),
        };
        let listed = rows(&list(vec![asking_with("s1", pending)]));
        assert_eq!(listed[0].detail, "Allow Write");
    }

    #[test]
    fn plan_approval_detail_is_the_plans_first_line() {
        let pending = PendingInput::PlanApproval {
            plan: Some("\n\n# Plan\nstep 1".to_owned()),
        };
        let listed = rows(&list(vec![asking_with("s1", pending)]));
        assert_eq!(listed[0].detail, "Approve plan: # Plan");
    }

    #[test]
    fn plan_approval_without_a_plan_reads_approve_plan() {
        let blank = rows(&list(vec![asking_with(
            "s1",
            PendingInput::PlanApproval {
                plan: Some(" \n\t\n".to_owned()),
            },
        )]));
        assert_eq!(blank[0].detail, "Approve plan");
        let absent = rows(&list(vec![asking_with(
            "s2",
            PendingInput::PlanApproval { plan: None },
        )]));
        assert_eq!(absent[0].detail, "Approve plan");
    }

    #[test]
    fn other_detail_is_the_message() {
        let pending = PendingInput::Other {
            message: "An MCP server asks for input".to_owned(),
        };
        let listed = rows(&list(vec![asking_with("s1", pending)]));
        assert_eq!(listed[0].detail, "An MCP server asks for input");
        let blank = rows(&list(vec![asking_with(
            "s2",
            PendingInput::Other {
                message: "  ".to_owned(),
            },
        )]));
        assert_eq!(blank[0].detail, "Waiting for input");
    }

    #[test]
    fn unknown_pending_input_falls_back_to_the_degraded_text() {
        let bare = asking_with("s1", PendingInput::Unknown);
        let mut titled = asking_with("s2", PendingInput::Unknown);
        titled.terminal_title = Some("Running the migration".to_owned());
        let listed = rows(&list(vec![bare, titled]));
        assert_eq!(listed[0].detail, "Waiting for input");
        assert_eq!(
            listed[1].detail,
            "Waiting for input · Running the migration"
        );
    }

    #[test]
    fn pending_input_text_is_one_line() {
        let pending = PendingInput::Question {
            questions: vec![question(
                "Migration",
                "keep newest,\nmerge oldest,\tor\nskip?",
            )],
        };
        let listed = rows(&list(vec![asking_with("s1", pending)]));
        assert_eq!(
            listed[0].detail,
            "Migration: keep newest, merge oldest, or skip?"
        );
    }

    #[test]
    fn pending_input_is_ignored_for_ended_rows() {
        let mut stale = ended("s1", SessionStatus::Error, None);
        stale.pending_input = Some(PendingInput::Other {
            message: "stale question".to_owned(),
        });
        let mut model = list(vec![stale]);
        attend(&mut model, "s1");
        let listed = rows(&model);
        assert_eq!(listed[0].reason, Reason::Ended);
        assert_eq!(listed[0].detail, "Error");
    }

    #[test]
    fn ended_detail_forms() {
        let mut model = list(vec![
            ended("s1", SessionStatus::Error, None),
            ended("s2", SessionStatus::Stopped, Some(3)),
            ended("s3", SessionStatus::Stopped, None),
        ]);
        for id in ["s1", "s2", "s3"] {
            attend(&mut model, id);
        }
        let listed = rows(&model);
        assert_eq!(ids(&listed), ["s1", "s2", "s3"]);
        assert_eq!(listed[0].detail, "Error");
        assert_eq!(listed[1].detail, "Exited with code 3");
        assert_eq!(listed[2].detail, "Stopped");
    }

    #[test]
    fn waited_formats_boundaries() {
        let now = stamp(0);
        assert_eq!(waited(now, now), "0s");
        assert_eq!(waited(now, stamp(-59)), "59s");
        assert_eq!(waited(now, stamp(-60)), "1m");
        assert_eq!(waited(now, stamp(-59 * 60)), "59m");
        assert_eq!(waited(now, stamp(-(60 * 60 + 5 * 60))), "1h 5m");
        assert_eq!(waited(now, stamp(-7200)), "2h");
    }
}
