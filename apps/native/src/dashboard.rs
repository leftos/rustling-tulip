//! The Dashboard's model: which sessions it lists, the four groups they fall
//! in, the order within each group, the header's counts and the waits.
//!
//! Plain Rust over the sidebar model, so the rules are unit-testable without
//! GPUI; nothing here draws.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use protocol::{SessionFacts, SessionSnapshot, SessionStatus};

use crate::RootView;
use crate::needs_you;
use crate::sidebar::{Leaf, LeafState, SidebarModel};

/// The Dashboard's groups, in the order it shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GroupKind {
    /// Waiting on the user: asking, or idle under the Answer rule.
    NeedsYou,
    /// Working, spawning, or idle with background work still running.
    Working,
    /// Idle with nothing left to look at.
    Idle,
    /// An unseen turn, or ended.
    Done,
}

impl GroupKind {
    /// Every group, in display order.
    pub(crate) const ALL: [Self; 4] = [Self::NeedsYou, Self::Working, Self::Idle, Self::Done];

    /// The group's heading.
    pub(crate) fn heading(self) -> &'static str {
        match self {
            Self::NeedsYou => "Needs you",
            Self::Working => "Working",
            Self::Idle => "Idle",
            Self::Done => "Done",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::NeedsYou => 0,
            Self::Working => 1,
            Self::Idle => 2,
            Self::Done => 3,
        }
    }
}

/// One listed session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DashboardRow {
    pub(crate) session_id: String,
    /// The sidebar container the session sits in.
    pub(crate) container_name: String,
    /// The label the sidebar leaf shows.
    pub(crate) label: String,
    /// Marked "Don't count as busy": listed, but not counted as working.
    pub(crate) busy_excluded: bool,
}

/// The header's count pills.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct HeaderCounts {
    pub(crate) need_you: usize,
    /// Working rows, busy-excluded ones left out.
    pub(crate) working: usize,
    pub(crate) idle: usize,
    pub(crate) done: usize,
}

/// The listed sessions, grouped and ordered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DashboardModel {
    groups: [Vec<DashboardRow>; 4],
}

impl DashboardModel {
    /// The rows of `kind`, in order.
    pub(crate) fn rows(&self, kind: GroupKind) -> &[DashboardRow] {
        &self.groups[kind.index()]
    }

    /// The sessions waiting on the user, longest wait first: the Needs you
    /// group. Ended sessions are never waits; they are in Done.
    pub(crate) fn waits(&self) -> &[DashboardRow] {
        self.rows(GroupKind::NeedsYou)
    }

    pub(crate) fn counts(&self) -> HeaderCounts {
        HeaderCounts {
            need_you: self.waits().len(),
            working: self
                .rows(GroupKind::Working)
                .iter()
                .filter(|row| !row.busy_excluded)
                .count(),
            idle: self.rows(GroupKind::Idle).len(),
            done: self.rows(GroupKind::Done).len(),
        }
    }
}

/// A row with the times it sorts by.
struct Placed {
    row: DashboardRow,
    /// When the status last changed; the wait's start.
    since: Option<DateTime<Utc>>,
    /// The latest activity known for the session.
    activity: DateTime<Utc>,
}

/// The Dashboard's groups over `sidebar`. `facts` holds the daemon's facts
/// by session id; a session without facts sorts by its snapshot alone.
///
/// Needs you runs longest wait first, the sessions with no stamp after
/// those; the other groups run latest activity first. Ties keep the
/// sidebar's order.
pub(crate) fn model(
    sidebar: &SidebarModel,
    facts: &BTreeMap<String, SessionFacts>,
) -> DashboardModel {
    let mut placed: [Vec<Placed>; 4] = Default::default();
    for container in sidebar.containers() {
        for leaf in &container.leaves {
            if !is_listed(leaf) {
                continue;
            }
            let Some(session) = sidebar.session(&leaf.id) else {
                continue;
            };
            let kind = group_of(session, leaf.unseen);
            placed[kind.index()].push(Placed {
                row: DashboardRow {
                    session_id: session.id.clone(),
                    container_name: container.name.clone(),
                    label: leaf.label.clone(),
                    busy_excluded: session.busy_excluded,
                },
                since: session.status_since,
                activity: latest_activity(session, facts),
            });
        }
    }
    for (kind, group) in GroupKind::ALL.into_iter().zip(placed.iter_mut()) {
        if kind == GroupKind::NeedsYou {
            group.sort_by_key(|placed| (placed.since.is_none(), placed.since));
        } else {
            group.sort_by_key(|placed| Reverse(placed.activity));
        }
    }
    DashboardModel {
        groups: placed.map(|group| group.into_iter().map(|placed| placed.row).collect()),
    }
}

/// Whether the Dashboard lists the session of `leaf`: it has an agent, and
/// it is neither parked nor abandoned.
fn is_listed(leaf: &Leaf) -> bool {
    leaf.agent.is_some()
        && !matches!(
            leaf.state,
            LeafState::Inactive { .. } | LeafState::Abandoned { .. }
        )
}

/// The group `session` falls in; `unseen` says its last turn ended unseen.
fn group_of(session: &SessionSnapshot, unseen: bool) -> GroupKind {
    match session.status {
        SessionStatus::Stopped | SessionStatus::Error => GroupKind::Done,
        SessionStatus::AwaitingInput => GroupKind::NeedsYou,
        SessionStatus::Working | SessionStatus::Spawning => GroupKind::Working,
        SessionStatus::Idle if needs_you::is_answer(session) => GroupKind::NeedsYou,
        SessionStatus::Idle if session.background => GroupKind::Working,
        SessionStatus::Idle if unseen => GroupKind::Done,
        SessionStatus::Idle => GroupKind::Idle,
    }
}

/// The session's latest activity: its facts' stamp, else its status stamp,
/// else its start.
fn latest_activity(
    session: &SessionSnapshot,
    facts: &BTreeMap<String, SessionFacts>,
) -> DateTime<Utc> {
    facts
        .get(&session.id)
        .and_then(|facts| facts.last_activity_at)
        .or(session.status_since)
        .unwrap_or(session.started_at)
}

/// One listed session as the Dashboard's groups hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardEntry {
    pub session_id: String,
    /// The sidebar container it sits in.
    pub container: String,
    pub label: String,
}

impl RootView {
    fn dashboard_model(&self) -> DashboardModel {
        model(&self.sidebar, &BTreeMap::new())
    }

    /// The Dashboard's groups in display order, each as its heading and its
    /// rows in order; an empty group is kept.
    #[must_use]
    pub fn dashboard_groups(&self) -> Vec<(&'static str, Vec<DashboardEntry>)> {
        let model = self.dashboard_model();
        GroupKind::ALL
            .into_iter()
            .map(|kind| {
                let entries = model
                    .rows(kind)
                    .iter()
                    .map(|row| DashboardEntry {
                        session_id: row.session_id.clone(),
                        container: row.container_name.clone(),
                        label: row.label.clone(),
                    })
                    .collect();
                (kind.heading(), entries)
            })
            .collect()
    }

    /// The header's counts in group order: need you, working, idle, done.
    #[must_use]
    pub fn dashboard_counts(&self) -> [usize; 4] {
        let counts = self.dashboard_model().counts();
        [counts.need_you, counts.working, counts.idle, counts.done]
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use protocol::{
        AlertKind, AttentionReason, DaemonMessage, RepoEntry, SessionMode, SessionSummary,
        SummarySource,
    };
    use serde_json::json;

    /// The repo every fixture session belongs to.
    const REPO: &str = "r1";

    fn repo() -> RepoEntry {
        serde_json::from_value(json!({ "id": REPO, "name": REPO, "path": "C:/r1" }))
            .expect("repo fixture")
    }

    /// An idle interactive agent session in [`REPO`].
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

    fn with_status(id: &str, status: SessionStatus) -> SessionSnapshot {
        let mut s = session(id);
        s.status = status;
        s
    }

    /// `id` in `status` since `since`.
    fn stamped(id: &str, status: SessionStatus, since: DateTime<Utc>) -> SessionSnapshot {
        let mut s = with_status(id, status);
        s.status_since = Some(since);
        s
    }

    /// An idle session stamped at `since`, with a summary of `kind` made at
    /// `updated_at`.
    fn summarized(
        id: &str,
        since: Option<DateTime<Utc>>,
        kind: AlertKind,
        updated_at: DateTime<Utc>,
    ) -> SessionSnapshot {
        let mut s = session(id);
        s.status_since = since;
        s.summary = Some(SessionSummary {
            kind: Some(kind),
            headline: String::new(),
            lines: Vec::new(),
            updated_at,
            source: SummarySource::Model,
        });
        s
    }

    /// A fixed stamp `offset` seconds from mid-November 2023, so the tests
    /// never read the clock.
    fn stamp(offset: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + offset, 0).expect("a fixed stamp")
    }

    fn sidebar(sessions: Vec<SessionSnapshot>) -> SidebarModel {
        let mut model = SidebarModel::default();
        model.apply(&DaemonMessage::Repos {
            repos: vec![repo()],
        });
        model.apply(&DaemonMessage::Sessions { sessions });
        model
    }

    fn build(model: &SidebarModel) -> DashboardModel {
        super::model(model, &BTreeMap::new())
    }

    fn ids(rows: &[DashboardRow]) -> Vec<&str> {
        rows.iter().map(|row| row.session_id.as_str()).collect()
    }

    /// The group `id` falls in, or `None` when it is not listed.
    fn group(model: &DashboardModel, id: &str) -> Option<GroupKind> {
        GroupKind::ALL
            .into_iter()
            .find(|kind| model.rows(*kind).iter().any(|row| row.session_id == id))
    }

    fn group_of_one(session: SessionSnapshot) -> Option<GroupKind> {
        let id = session.id.clone();
        group(&build(&sidebar(vec![session])), &id)
    }

    fn facts(id: &str, last_activity_at: DateTime<Utc>) -> SessionFacts {
        let mut facts: SessionFacts = serde_json::from_value(json!({
            "session_id": id,
            "now": { "kind": "thinking", "text": "" },
        }))
        .expect("facts fixture");
        facts.last_activity_at = Some(last_activity_at);
        facts
    }

    #[test]
    fn lists_every_agent_session_headless_included() {
        let mut headless = with_status("s2", SessionStatus::Working);
        headless.mode = SessionMode::Headless;
        let model = build(&sidebar(vec![session("s1"), headless]));
        assert_eq!(group(&model, "s1"), Some(GroupKind::Idle));
        assert_eq!(group(&model, "s2"), Some(GroupKind::Working));
    }

    #[test]
    fn plain_shells_are_not_listed() {
        let mut shell = session("s1");
        shell.mode = SessionMode::PlainShell;
        shell.agent = None;
        assert_eq!(group_of_one(shell), None);
    }

    #[test]
    fn parked_and_abandoned_agent_sessions_are_not_listed() {
        let mut parked = with_status("s1", SessionStatus::Stopped);
        parked.is_inactive = true;
        let mut abandoned = with_status("s2", SessionStatus::Stopped);
        abandoned.is_abandoned = true;
        let model = build(&sidebar(vec![parked, abandoned]));
        assert_eq!(group(&model, "s1"), None);
        assert_eq!(group(&model, "s2"), None);
    }

    #[test]
    fn an_orphan_agent_session_is_listed() {
        let mut orphan = session("s1");
        orphan.is_orphan = true;
        assert_eq!(group_of_one(orphan), Some(GroupKind::Idle));
    }

    #[test]
    fn awaiting_input_is_in_needs_you() {
        let asking = with_status("s1", SessionStatus::AwaitingInput);
        assert_eq!(group_of_one(asking), Some(GroupKind::NeedsYou));
    }

    #[test]
    fn idle_with_a_fresh_needs_answer_summary_is_in_needs_you() {
        let later = summarized("s1", Some(stamp(0)), AlertKind::NeedsAnswer, stamp(5));
        let same = summarized("s2", Some(stamp(0)), AlertKind::NeedsAnswer, stamp(0));
        let model = build(&sidebar(vec![later, same]));
        assert_eq!(group(&model, "s1"), Some(GroupKind::NeedsYou));
        assert_eq!(
            group(&model, "s2"),
            Some(GroupKind::NeedsYou),
            "a summary stamped with the stop itself is about that stop"
        );
    }

    #[test]
    fn idle_with_a_needs_answer_summary_older_than_status_since_is_idle() {
        let stale = summarized("s1", Some(stamp(0)), AlertKind::NeedsAnswer, stamp(-1));
        assert_eq!(group_of_one(stale), Some(GroupKind::Idle));
    }

    #[test]
    fn idle_without_a_stamp_and_a_needs_answer_summary_is_idle() {
        let unstamped = summarized("s1", None, AlertKind::NeedsAnswer, stamp(0));
        assert_eq!(group_of_one(unstamped), Some(GroupKind::Idle));
    }

    #[test]
    fn done_waiting_and_working_update_summaries_do_not_raise_needs_you() {
        let done = summarized("s1", Some(stamp(0)), AlertKind::DoneWaiting, stamp(5));
        let update = summarized("s2", Some(stamp(0)), AlertKind::WorkingUpdate, stamp(5));
        let model = build(&sidebar(vec![done, update]));
        assert_eq!(group(&model, "s1"), Some(GroupKind::Idle));
        assert_eq!(group(&model, "s2"), Some(GroupKind::Idle));
    }

    #[test]
    fn a_working_session_with_a_needs_answer_summary_stays_working() {
        let mut working = summarized("s1", Some(stamp(0)), AlertKind::NeedsAnswer, stamp(5));
        working.status = SessionStatus::Working;
        assert_eq!(group_of_one(working), Some(GroupKind::Working));
    }

    #[test]
    fn working_and_idle_with_background_are_working() {
        let working = with_status("s1", SessionStatus::Working);
        let mut background = session("s2");
        background.background = true;
        let model = build(&sidebar(vec![working, background]));
        assert_eq!(group(&model, "s1"), Some(GroupKind::Working));
        assert_eq!(group(&model, "s2"), Some(GroupKind::Working));
    }

    #[test]
    fn spawning_is_working() {
        let spawning = with_status("s1", SessionStatus::Spawning);
        assert_eq!(group_of_one(spawning), Some(GroupKind::Working));
    }

    #[test]
    fn plain_idle_is_idle() {
        assert_eq!(group_of_one(session("s1")), Some(GroupKind::Idle));
    }

    #[test]
    fn an_unseen_turn_is_done() {
        let mut model = sidebar(vec![with_status("s1", SessionStatus::Working)]);
        model.apply(&DaemonMessage::SessionUpdated {
            session: session("s1"),
            request_id: None,
        });
        assert!(
            model.is_unseen("s1"),
            "the turn ended while nothing was focused"
        );
        assert_eq!(group(&build(&model), "s1"), Some(GroupKind::Done));
    }

    #[test]
    fn stopped_and_error_are_done_with_or_without_attention() {
        let mut model = sidebar(vec![
            with_status("s1", SessionStatus::Stopped),
            with_status("s2", SessionStatus::Error),
        ]);
        model.apply(&DaemonMessage::Attention {
            session_id: "s2".to_owned(),
            reason: AttentionReason::Error,
        });
        let built = build(&model);
        assert_eq!(group(&built, "s1"), Some(GroupKind::Done));
        assert_eq!(group(&built, "s2"), Some(GroupKind::Done));
    }

    #[test]
    fn ended_rows_are_not_waits() {
        let mut model = sidebar(vec![
            with_status("s1", SessionStatus::AwaitingInput),
            with_status("s2", SessionStatus::Error),
        ]);
        model.apply(&DaemonMessage::Attention {
            session_id: "s2".to_owned(),
            reason: AttentionReason::Error,
        });
        assert_eq!(
            needs_you::rows(&model).len(),
            2,
            "the Needs You panel still lists the ended row"
        );
        assert_eq!(ids(build(&model).waits()), ["s1"]);
    }

    #[test]
    fn groups_run_needs_you_working_idle_done() {
        let headings = GroupKind::ALL.map(GroupKind::heading);
        assert_eq!(headings, ["Needs you", "Working", "Idle", "Done"]);
    }

    #[test]
    fn needs_you_runs_longest_wait_first_unstamped_last_ties_in_sidebar_order() {
        let recent = stamped("s1", SessionStatus::AwaitingInput, stamp(-60));
        let unstamped = with_status("s2", SessionStatus::AwaitingInput);
        let answer = summarized("s3", Some(stamp(-300)), AlertKind::NeedsAnswer, stamp(-200));
        let tie_b = stamped("b", SessionStatus::AwaitingInput, stamp(-120));
        let tie_a = stamped("a", SessionStatus::AwaitingInput, stamp(-120));
        let model = build(&sidebar(vec![recent, unstamped, answer, tie_b, tie_a]));
        assert_eq!(ids(model.waits()), ["s3", "a", "b", "s1", "s2"]);
    }

    #[test]
    fn waits_match_needs_you_rows_order() {
        let sidebar = sidebar(vec![
            stamped("s1", SessionStatus::AwaitingInput, stamp(-60)),
            with_status("s2", SessionStatus::AwaitingInput),
            stamped("s3", SessionStatus::AwaitingInput, stamp(-600)),
        ]);
        let panel: Vec<String> = needs_you::rows(&sidebar)
            .into_iter()
            .map(|row| row.session_id)
            .collect();
        assert_eq!(ids(build(&sidebar).waits()), panel);
    }

    #[test]
    fn other_groups_run_latest_activity_first() {
        let old = stamped("s1", SessionStatus::Working, stamp(-600));
        let new = stamped("s2", SessionStatus::Working, stamp(-60));
        let sidebar = sidebar(vec![old, new]);
        assert_eq!(
            ids(build(&sidebar).rows(GroupKind::Working)),
            ["s2", "s1"],
            "without facts the status stamp orders them"
        );
        let mut by_id = BTreeMap::new();
        by_id.insert("s1".to_owned(), facts("s1", stamp(-5)));
        let model = super::model(&sidebar, &by_id);
        assert_eq!(
            ids(model.rows(GroupKind::Working)),
            ["s1", "s2"],
            "the facts' activity stamp wins over the status stamp"
        );
    }

    #[test]
    fn latest_activity_falls_back_to_the_start() {
        let mut earlier = session("s1");
        earlier.started_at = stamp(-600);
        let mut later = session("s2");
        later.started_at = stamp(-60);
        let stamped_old = stamped("s3", SessionStatus::Idle, stamp(-300));
        let model = build(&sidebar(vec![earlier, later, stamped_old]));
        assert_eq!(ids(model.rows(GroupKind::Idle)), ["s2", "s3", "s1"]);
    }

    #[test]
    fn header_counts_follow_the_groups() {
        let mut model = sidebar(vec![
            with_status("s1", SessionStatus::AwaitingInput),
            summarized("s2", Some(stamp(0)), AlertKind::NeedsAnswer, stamp(1)),
            with_status("s3", SessionStatus::Working),
            session("s4"),
            with_status("s5", SessionStatus::Stopped),
            with_status("s6", SessionStatus::Working),
        ]);
        model.apply(&DaemonMessage::SessionUpdated {
            session: session("s6"),
            request_id: None,
        });
        assert_eq!(
            build(&model).counts(),
            HeaderCounts {
                need_you: 2,
                working: 1,
                idle: 1,
                done: 2,
            }
        );
    }

    #[test]
    fn busy_excluded_working_is_listed_but_not_counted_working() {
        let mut excluded = with_status("s1", SessionStatus::Working);
        excluded.busy_excluded = true;
        let model = build(&sidebar(vec![
            excluded,
            with_status("s2", SessionStatus::Working),
        ]));
        assert_eq!(ids(model.rows(GroupKind::Working)), ["s1", "s2"]);
        assert_eq!(model.counts().working, 1);
    }

    #[test]
    fn busy_excluded_background_is_not_counted_working() {
        let mut excluded = session("s1");
        excluded.background = true;
        excluded.busy_excluded = true;
        let model = build(&sidebar(vec![excluded]));
        assert_eq!(ids(model.rows(GroupKind::Working)), ["s1"]);
        assert_eq!(model.counts().working, 0);
    }

    #[test]
    fn busy_excluded_idle_asking_and_ended_still_count() {
        let mut asking = with_status("s1", SessionStatus::AwaitingInput);
        asking.busy_excluded = true;
        let mut idle = session("s2");
        idle.busy_excluded = true;
        let mut ended = with_status("s3", SessionStatus::Stopped);
        ended.busy_excluded = true;
        let counts = build(&sidebar(vec![asking, idle, ended])).counts();
        assert_eq!(
            counts,
            HeaderCounts {
                need_you: 1,
                working: 0,
                idle: 1,
                done: 1,
            }
        );
    }
}
