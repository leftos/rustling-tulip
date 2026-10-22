//! The repo/workspace Remove flow's plain state: the armed row of the
//! container menu and the dialog that lists the container's live sessions.
//!
//! The view ([`crate::repo_remove_view`]) only renders what this holds. The
//! rules — which sessions count as live, which belong to the level, the
//! dialog's title, rows, buttons and focus, and the messages an answer sends
//! — are decided and unit-tested here.

use protocol::{ClientMessage, SessionMode, SessionSnapshot, SessionStatus};

use crate::appearance_view::Level;
use crate::sidebar::SidebarModel;

/// The plain label of a repo's Remove row.
const REPO_ROW: &str = "Remove repo";
/// The plain label of a workspace's Remove row.
const WORKSPACE_ROW: &str = "Remove workspace";
/// The armed label of a repo's Remove row, after its first click.
const ARMED_REPO_ROW: &str = "Confirm: remove repo…";
/// The armed label of a workspace's Remove row.
const ARMED_WORKSPACE_ROW: &str = "Confirm: remove workspace…";
/// The dialog's body line.
pub(crate) const DIALOG_BODY: &str = "Running sessions move to Detached.";

/// The three buttons of the dialog, in footer and Tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepoRemoveButton {
    Cancel,
    /// Unregister the container and leave its sessions running.
    Anyway,
    /// Stop every listed session, then unregister the container.
    StopAndRemove,
}

impl RepoRemoveButton {
    pub(crate) const ALL: [Self; 3] = [Self::Cancel, Self::Anyway, Self::StopAndRemove];

    pub(crate) const fn selector(self) -> &'static str {
        match self {
            Self::Cancel => "repo-remove-cancel",
            Self::Anyway => "repo-remove-anyway",
            Self::StopAndRemove => "repo-remove-stop-and-remove",
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Cancel => "Cancel",
            Self::Anyway => "Remove anyway",
            Self::StopAndRemove => "Stop and remove",
        }
    }

    /// Only the button that ends sessions wears the danger colour.
    pub(crate) const fn is_danger(self) -> bool {
        matches!(self, Self::StopAndRemove)
    }
}

/// One session row of the dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoRemoveSession {
    pub(crate) id: String,
    /// The leaf's label.
    pub(crate) label: String,
    /// The agent driving it, drawn in parentheses beside the label; `None`
    /// for a leaf with no agent to name.
    pub(crate) agent: Option<String>,
}

impl RepoRemoveSession {
    /// The row's text: the leaf's label, then its agent in parentheses.
    pub(crate) fn row_text(&self) -> String {
        match &self.agent {
            Some(agent) => format!("{} ({agent})", self.label),
            None => self.label.clone(),
        }
    }
}

/// The open dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoRemoveDialog {
    level: Level,
    name: String,
    sessions: Vec<RepoRemoveSession>,
    /// The focused button's index into [`RepoRemoveButton::ALL`].
    focus: usize,
}

impl RepoRemoveDialog {
    pub(crate) fn level(&self) -> &Level {
        &self.level
    }

    /// The container's name, drawn under the title.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn sessions(&self) -> &[RepoRemoveSession] {
        &self.sessions
    }

    /// The heading: `Remove repo?` or `Remove workspace?`.
    pub(crate) fn title(&self) -> &'static str {
        match self.level {
            Level::Repo(_) => "Remove repo?",
            Level::Workspace(_) => "Remove workspace?",
            Level::Session(_) | Level::App => "",
        }
    }

    /// The footer's buttons as shown: selector and label, in order.
    pub(crate) fn buttons() -> Vec<(String, String)> {
        RepoRemoveButton::ALL
            .into_iter()
            .map(|button| (button.selector().to_owned(), button.label().to_owned()))
            .collect()
    }

    pub(crate) fn focused(&self) -> RepoRemoveButton {
        RepoRemoveButton::ALL
            .get(self.focus)
            .copied()
            .unwrap_or(RepoRemoveButton::Cancel)
    }

    /// Tab and Shift+Tab: the next or previous button, wrapping.
    pub(crate) fn move_focus(&mut self, forward: bool) {
        let count = RepoRemoveButton::ALL.len();
        self.focus = if forward {
            (self.focus + 1) % count
        } else {
            (self.focus + count - 1) % count
        };
    }
}

/// The Remove flow's state: the one armed row app-wide, keyed by container,
/// and the dialog while it is open.
#[derive(Debug, Clone, Default)]
pub(crate) struct RepoRemove {
    armed: Option<String>,
    dialog: Option<RepoRemoveDialog>,
}

impl RepoRemove {
    /// The container key whose row is armed.
    pub(crate) fn armed(&self) -> Option<&str> {
        self.armed.as_deref()
    }

    /// Arms the row of container `key`; returns whether it changed.
    pub(crate) fn arm(&mut self, key: &str) -> bool {
        self.armed.replace(key.to_owned()).as_deref() != Some(key)
    }

    /// Drops the arm; returns whether one was there.
    pub(crate) fn disarm(&mut self) -> bool {
        self.armed.take().is_some()
    }

    pub(crate) fn dialog(&self) -> Option<&RepoRemoveDialog> {
        self.dialog.as_ref()
    }

    pub(crate) fn dialog_mut(&mut self) -> Option<&mut RepoRemoveDialog> {
        self.dialog.as_mut()
    }

    /// Opens the dialog for `level`, dropping any arm: a click that opens the
    /// dialog did not land on the armed row.
    pub(crate) fn open_dialog(
        &mut self,
        level: Level,
        name: String,
        sessions: Vec<RepoRemoveSession>,
    ) {
        self.armed = None;
        self.dialog = Some(RepoRemoveDialog {
            level,
            name,
            sessions,
            focus: 0,
        });
    }

    /// Takes the open dialog, closing it.
    pub(crate) fn close_dialog(&mut self) -> Option<RepoRemoveDialog> {
        self.dialog.take()
    }
}

/// The label the Remove row of `level` shows, plain or armed after its first
/// click.
pub(crate) fn row_label(level: &Level, armed: bool) -> String {
    match (level, armed) {
        (Level::Repo(_), false) => REPO_ROW.to_owned(),
        (Level::Repo(_), true) => ARMED_REPO_ROW.to_owned(),
        (Level::Workspace(_), false) => WORKSPACE_ROW.to_owned(),
        (Level::Workspace(_), true) => ARMED_WORKSPACE_ROW.to_owned(),
        (Level::Session(_) | Level::App, _) => String::new(),
    }
}

/// The container key [`RepoRemove`] arms by.
pub(crate) fn container_key(level: &Level) -> String {
    match level {
        Level::Repo(id) => format!("repo:{id}"),
        Level::Workspace(id) => format!("ws:{id}"),
        Level::Session(id) => format!("session:{id}"),
        Level::App => "app".to_owned(),
    }
}

/// The message that unregisters `level`.
pub(crate) fn remove_message(level: &Level) -> Option<ClientMessage> {
    match level {
        Level::Repo(repo_id) => Some(ClientMessage::RemoveRepo {
            repo_id: repo_id.clone(),
        }),
        Level::Workspace(workspace_id) => Some(ClientMessage::RemoveWorkspace {
            workspace_id: workspace_id.clone(),
        }),
        Level::Session(_) | Level::App => None,
    }
}

/// The message that stops `session_id` and keeps its record: stop only, so
/// the session moves to Detached rather than vanishing.
pub(crate) fn stop_message(session_id: &str) -> ClientMessage {
    ClientMessage::StopSession {
        session_id: session_id.to_owned(),
        cleanup: Vec::new(),
    }
}

/// Whether removing the level would end `s`: a live leaf, so a session that
/// is not parked, abandoned, stopped or errored. An orphan still runs, so it
/// counts as live.
pub(crate) fn is_live(s: &SessionSnapshot) -> bool {
    let state_is_live = !s.is_inactive && !s.is_abandoned;
    state_is_live && !matches!(s.status, SessionStatus::Stopped | SessionStatus::Error)
}

/// Whether `s` is one of `level`'s own sessions, by the rule the sidebar
/// places a leaf by: its workspace id for a workspace, its first member repo
/// for a repo. A repo a workspace holds has no container of its own, and a
/// plain shell placed in the folder is not owned.
pub(crate) fn owned_by(level: &Level, s: &SessionSnapshot, model: &SidebarModel) -> bool {
    if s.mode == SessionMode::PlainShell {
        return false;
    }
    match level {
        Level::Workspace(id) => s.workspace_id.as_deref() == Some(id.as_str()),
        Level::Repo(id) => {
            let in_a_workspace = model
                .workspaces()
                .iter()
                .any(|workspace| workspace.member_repo_ids.iter().any(|member| member == id));
            s.workspace_id.is_none()
                && !in_a_workspace
                && s.members
                    .first()
                    .is_some_and(|member| member.repo_id == *id)
        }
        Level::Session(_) | Level::App => false,
    }
}

/// The live sessions `level` owns, as the dialog lists them, by label as the
/// container draws its leaves.
pub(crate) fn live_owned_sessions(
    level: &Level,
    sessions: &[SessionSnapshot],
    model: &SidebarModel,
) -> Vec<RepoRemoveSession> {
    let mut listed: Vec<RepoRemoveSession> = sessions
        .iter()
        .filter(|session| is_live(session) && owned_by(level, session, model))
        .map(|session| RepoRemoveSession {
            id: session.id.clone(),
            label: crate::sidebar::display_label(session),
            agent: (session.mode != SessionMode::PlainShell)
                .then(|| session.agent.as_label().to_owned()),
        })
        .collect();
    listed.sort_by_cached_key(|session| session.label.to_lowercase());
    listed
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use protocol::{Agent, DaemonMessage, RepoEntry, SessionKind, WorkspaceEntry};
    use serde_json::json;

    fn repo(id: &str) -> RepoEntry {
        serde_json::from_value(json!({ "id": id, "name": id, "path": format!("C:/{id}") }))
            .expect("repo fixture")
    }

    fn workspace(id: &str, members: &[&str]) -> WorkspaceEntry {
        serde_json::from_value(json!({ "id": id, "name": id, "member_repo_ids": members }))
            .expect("workspace fixture")
    }

    /// A sidebar holding `repos` and `workspaces`, so the ownership rule reads
    /// the registry it has to read.
    fn model(repos: &[&str], workspaces: &[(&str, &[&str])]) -> SidebarModel {
        let mut model = SidebarModel::default();
        model.apply(&DaemonMessage::Repos {
            repos: repos.iter().map(|id| repo(id)).collect(),
        });
        model.apply(&DaemonMessage::Workspaces {
            workspaces: workspaces
                .iter()
                .map(|(id, members)| workspace(id, members))
                .collect(),
        });
        model
    }

    fn session(id: &str) -> SessionSnapshot {
        serde_json::from_value(json!({
            "id": id,
            "label": id,
            "kind": "single",
            "members": [],
            "status": "idle",
            "mode": "interactive",
            "started_at": "2026-01-01T00:00:00Z",
            "exit_code": null,
            "metrics": { "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0, "last_activity_at": null },
            "recent_actions": [],
            "agent": "claude",
        }))
        .expect("session fixture")
    }

    fn in_repo(id: &str, repo_id: &str) -> SessionSnapshot {
        let mut s = session(id);
        s.members = serde_json::from_value(json!([{
            "repo_id": repo_id,
            "repo_name": repo_id,
            "branch": "main",
            "worktree_path": "",
        }]))
        .expect("member fixture");
        s
    }

    fn in_workspace(id: &str, workspace_id: &str) -> SessionSnapshot {
        let mut s = in_repo(id, "r1");
        s.kind = SessionKind::Workspace;
        s.workspace_id = Some(workspace_id.to_owned());
        s
    }

    fn shell(id: &str, cwd: &str) -> SessionSnapshot {
        let mut s = session(id);
        s.mode = SessionMode::PlainShell;
        s.current_cwd = Some(cwd.to_owned());
        s
    }

    fn row(id: &str) -> RepoRemoveSession {
        RepoRemoveSession {
            id: id.to_owned(),
            label: id.to_owned(),
            agent: Some("claude".to_owned()),
        }
    }

    fn open(remove: &mut RepoRemove, level: Level, sessions: Vec<RepoRemoveSession>) {
        remove.open_dialog(level, "name".to_owned(), sessions);
    }

    #[test]
    fn a_row_arms_once_and_disarms() {
        let mut remove = RepoRemove::default();
        assert_eq!(remove.armed(), None);
        assert!(remove.arm("repo:r1"), "the first click arms");
        assert_eq!(remove.armed(), Some("repo:r1"));
        assert!(!remove.arm("repo:r1"), "the second click is the removal's");
        assert_eq!(remove.armed(), Some("repo:r1"), "and it stays armed");
        assert!(remove.disarm());
        assert_eq!(remove.armed(), None);
    }

    #[test]
    fn disarming_an_unarmed_row_reports_nothing() {
        let mut remove = RepoRemove::default();
        assert!(!remove.disarm());
    }

    #[test]
    fn live_counts_an_orphan_and_skips_the_rest() {
        let mut stopped = session("s");
        stopped.status = SessionStatus::Stopped;
        let mut errored = session("s");
        errored.status = SessionStatus::Error;
        let mut abandoned = session("s");
        abandoned.is_abandoned = true;
        abandoned.status = SessionStatus::Stopped;
        let mut inactive = session("s");
        inactive.is_inactive = true;
        inactive.status = SessionStatus::Stopped;
        let mut orphan = session("s");
        orphan.is_orphan = true;

        assert!(is_live(&session("s")), "idle");
        assert!(is_live(&orphan), "an orphan still runs");
        assert!(!is_live(&stopped), "stopped");
        assert!(!is_live(&errored), "errored");
        assert!(!is_live(&abandoned), "abandoned");
        assert!(!is_live(&inactive), "parked");
    }

    #[test]
    fn only_the_levels_own_live_sessions_are_listed() {
        let model = model(&["r1", "r2"], &[("w1", &["r2"])]);
        let sessions = [
            in_repo("own", "r1"),
            in_repo("elsewhere", "r2"),
            shell("shell", "C:/r1"),
            in_workspace("ws", "w1"),
        ];
        let level = Level::Repo("r1".to_owned());
        let listed: Vec<String> = live_owned_sessions(&level, &sessions, &model)
            .into_iter()
            .map(|session| session.id)
            .collect();
        assert_eq!(
            listed,
            ["own"],
            "a plain shell in the folder and a workspace session are not the repo's"
        );
    }

    #[test]
    fn a_workspace_lists_the_sessions_that_name_it() {
        let model = model(&["r1", "r2"], &[("w1", &["r1"])]);
        let sessions = [
            in_workspace("ws", "w1"),
            in_repo("single", "r1"),
            in_workspace("other", "w2"),
        ];
        let level = Level::Workspace("w1".to_owned());
        let listed: Vec<String> = live_owned_sessions(&level, &sessions, &model)
            .into_iter()
            .map(|session| session.id)
            .collect();
        assert_eq!(listed, ["ws"]);
    }

    #[test]
    fn a_repo_a_workspace_holds_owns_nothing() {
        let model = model(&["r1"], &[("w1", &["r1"])]);
        let sessions = [in_repo("s1", "r1")];
        let level = Level::Repo("r1".to_owned());
        assert!(
            live_owned_sessions(&level, &sessions, &model).is_empty(),
            "the sidebar places it in the workspace, not in a repo container"
        );
    }

    #[test]
    fn a_stopped_owned_session_is_not_listed() {
        let model = model(&["r1"], &[]);
        let mut stopped = in_repo("gone", "r1");
        stopped.status = SessionStatus::Stopped;
        let level = Level::Repo("r1".to_owned());
        assert!(
            live_owned_sessions(&level, &[stopped], &model).is_empty(),
            "a stopped session is not live"
        );
    }

    #[test]
    fn a_listed_session_carries_its_label_and_agent() {
        let model = model(&["r1"], &[]);
        let mut s = in_repo("s1", "r1");
        s.label = "feature work".to_owned();
        s.agent = Agent::Claude;
        let level = Level::Repo("r1".to_owned());
        let listed = live_owned_sessions(&level, &[s], &model);
        assert_eq!(
            listed,
            [RepoRemoveSession {
                id: "s1".to_owned(),
                label: "feature work".to_owned(),
                agent: Some("claude".to_owned()),
            }]
        );
        assert_eq!(listed[0].row_text(), "feature work (claude)");
    }

    #[test]
    fn a_listed_session_with_no_agent_draws_no_parenthesis() {
        let row = RepoRemoveSession {
            id: "s1".to_owned(),
            label: "scratch".to_owned(),
            agent: None,
        };
        assert_eq!(row.row_text(), "scratch");
    }

    #[test]
    fn the_rows_are_listed_by_label() {
        let model = model(&["r1"], &[]);
        let mut beta = in_repo("beta", "r1");
        beta.label = "Beta".to_owned();
        let mut alpha = in_repo("alpha", "r1");
        alpha.label = "alpha".to_owned();
        let level = Level::Repo("r1".to_owned());
        let listed: Vec<String> = live_owned_sessions(&level, &[beta, alpha], &model)
            .into_iter()
            .map(|session| session.id)
            .collect();
        assert_eq!(listed, ["alpha", "beta"]);
    }

    #[test]
    fn focus_moves_along_the_buttons_and_wraps() {
        let mut remove = RepoRemove::default();
        open(&mut remove, Level::Repo("r1".to_owned()), vec![row("s1")]);
        let mut dialog = remove.dialog().expect("the dialog opened").clone();
        assert_eq!(dialog.focused(), RepoRemoveButton::Cancel, "Cancel first");
        dialog.move_focus(true);
        assert_eq!(dialog.focused(), RepoRemoveButton::Anyway);
        dialog.move_focus(true);
        assert_eq!(dialog.focused(), RepoRemoveButton::StopAndRemove);
        dialog.move_focus(true);
        assert_eq!(dialog.focused(), RepoRemoveButton::Cancel, "it wraps");
        dialog.move_focus(false);
        assert_eq!(
            dialog.focused(),
            RepoRemoveButton::StopAndRemove,
            "backwards too"
        );
    }

    #[test]
    fn the_dialog_names_its_level_and_buttons() {
        let mut remove = RepoRemove::default();
        remove.open_dialog(
            Level::Workspace("w1".to_owned()),
            "frontend".to_owned(),
            vec![row("s1")],
        );
        let dialog = remove.dialog().expect("the dialog opened");
        assert_eq!(dialog.title(), "Remove workspace?");
        assert_eq!(dialog.name(), "frontend");
        assert_eq!(dialog.level(), &Level::Workspace("w1".to_owned()));
        assert_eq!(dialog.sessions().to_vec(), vec![row("s1")]);
        assert_eq!(
            RepoRemoveDialog::buttons(),
            [
                ("repo-remove-cancel".to_owned(), "Cancel".to_owned()),
                ("repo-remove-anyway".to_owned(), "Remove anyway".to_owned()),
                (
                    "repo-remove-stop-and-remove".to_owned(),
                    "Stop and remove".to_owned()
                ),
            ]
        );
        assert!(RepoRemoveButton::StopAndRemove.is_danger());
        assert!(!RepoRemoveButton::Cancel.is_danger());
        assert!(!RepoRemoveButton::Anyway.is_danger());
    }

    #[test]
    fn a_repo_dialog_asks_about_the_repo() {
        let mut remove = RepoRemove::default();
        open(&mut remove, Level::Repo("r1".to_owned()), vec![row("s1")]);
        assert_eq!(
            remove.dialog().expect("open").title(),
            "Remove repo?",
            "the heading follows the level"
        );
    }

    #[test]
    fn the_row_label_says_what_it_removes_and_confirms_second() {
        let repo_level = Level::Repo("r1".to_owned());
        assert_eq!(row_label(&repo_level, false), "Remove repo");
        assert_eq!(row_label(&repo_level, true), "Confirm: remove repo…");
        let ws_level = Level::Workspace("w1".to_owned());
        assert_eq!(row_label(&ws_level, false), "Remove workspace");
        assert_eq!(row_label(&ws_level, true), "Confirm: remove workspace…");
        assert_eq!(container_key(&repo_level), "repo:r1");
        assert_eq!(container_key(&ws_level), "ws:w1");
    }

    #[test]
    fn the_messages_name_the_level_and_stop_without_cleanup() {
        assert!(matches!(
            remove_message(&Level::Repo("r1".to_owned())),
            Some(ClientMessage::RemoveRepo { repo_id }) if repo_id == "r1"
        ));
        assert!(matches!(
            remove_message(&Level::Workspace("w1".to_owned())),
            Some(ClientMessage::RemoveWorkspace { workspace_id }) if workspace_id == "w1"
        ));
        assert!(matches!(
            stop_message("s1"),
            ClientMessage::StopSession { session_id, cleanup }
                if session_id == "s1" && cleanup.is_empty()
        ));
        assert!(
            remove_message(&Level::App).is_none(),
            "no level, no removal"
        );
    }

    #[test]
    fn opening_a_dialog_drops_the_arm_and_closing_hands_it_back() {
        let mut remove = RepoRemove::default();
        assert!(remove.arm("repo:r1"));
        open(&mut remove, Level::Repo("r1".to_owned()), Vec::new());
        assert_eq!(remove.armed(), None, "the click did not land on the row");
        assert!(remove.dialog().is_some());
        let closed = remove.close_dialog().expect("the dialog was open");
        assert_eq!(closed.name(), "name");
        assert!(remove.dialog().is_none(), "and it is closed");
        assert!(remove.close_dialog().is_none(), "closing twice is nothing");
    }
}
