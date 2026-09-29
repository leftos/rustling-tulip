//! The Needs You list: which sessions need the user, why, and for how long.
//!
//! Plain Rust over the sidebar model, so the list is unit-testable without
//! GPUI; the view in `needs_you_view` draws these rows.

#![cfg_attr(
    not(test),
    expect(dead_code, reason = "wired into the Needs You view in NY.3")
)]

use crate::appearance;
use crate::sidebar::{Leaf, SidebarModel};
use chrono::{DateTime, Utc};
use protocol::{SessionSnapshot, SessionStatus};

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
            if session.is_inactive || session.is_abandoned {
                continue;
            }
            let Some(reason) = reason_of(leaf, session) else {
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
                detail: detail_of(reason, session),
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

/// Why `session` is listed, or `None` when it is not. Asking comes from the
/// snapshot, so a cleared attention leaves the row; an ended session is
/// listed only while its leaf is still flagged.
fn reason_of(leaf: &Leaf, session: &SessionSnapshot) -> Option<Reason> {
    match session.status {
        SessionStatus::AwaitingInput => Some(Reason::Asking),
        SessionStatus::Error | SessionStatus::Stopped if leaf.attention => Some(Reason::Ended),
        _ => None,
    }
}

/// Line two for a listed session.
fn detail_of(reason: Reason, session: &SessionSnapshot) -> String {
    match reason {
        Reason::Asking => terminal_title(session).map_or_else(
            || "Waiting for input".to_owned(),
            |title| format!("Waiting for input · {title}"),
        ),
        Reason::Ended if session.status == SessionStatus::Error => "Error".to_owned(),
        Reason::Ended => session.exit_code.map_or_else(
            || "Stopped".to_owned(),
            |code| format!("Exited with code {code}"),
        ),
    }
}

/// The session's terminal title, when it has a non-blank one.
fn terminal_title(session: &SessionSnapshot) -> Option<&str> {
    session
        .terminal_title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
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
    use protocol::{AttentionReason, DaemonMessage, RepoEntry};
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
    fn stopped_without_attention_is_not_listed() {
        let model = list(vec![ended("s1", SessionStatus::Stopped, Some(0))]);
        assert!(rows(&model).is_empty());
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
        assert!(rows(&model).is_empty());
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
