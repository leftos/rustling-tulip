//! What the user can do to a session from its context menu, its pane header
//! and the stopped-pane overlay, and the daemon messages each choice sends.
//! Mirrors the Tauri app's `SessionContextMenu.tsx` and `SessionPane.tsx`.

use std::collections::HashMap;

use protocol::{BranchCleanup, CleanupAction, ClientMessage, SessionSnapshot, SessionStatus};

use crate::tabs::PaneBinding;

/// Which set of actions a session offers. Parked, stopped and running
/// follow `SessionContextMenu.tsx`'s state buckets. An abandoned session
/// gets the Resume and Dismiss of `SessionPane.tsx`'s abandoned view
/// instead of Stop or Restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActionState {
    Running,
    Stopped,
    Inactive,
    Abandoned,
}

/// Something the user can do to a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SessionAction {
    Rename,
    StopKeepWorktree,
    StopDeleteWorktree,
    Restart,
    /// "Remove pane, keep worktree": the session moves to inactive.
    Park,
    RemovePane,
    RemovePaneDeleteWorktree,
    Resume,
    RemoveFromSidebar,
    RemoveFromSidebarDeleteWorktree,
    ResumeAbandoned,
    DismissAbandoned,
}

/// What the context menu shows besides the name editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuMode {
    /// The session's actions, the stop choices folded under one entry.
    Actions,
    /// "Stop session…" was chosen: the stop choices and Cancel.
    StopChoice,
}

/// A row of the context menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuEntry {
    Action(SessionAction),
    /// "Stop session…", which opens the stop choices.
    StopChoice,
    /// Back from the stop choices to the actions.
    Cancel,
}

/// What choosing an action does.
#[derive(Debug, Clone)]
pub(crate) enum Step {
    /// Send these, in order.
    Send(Vec<ClientMessage>),
    /// Open the name editor; nothing is sent until it is submitted.
    EditName,
    /// Duplicate the session under a fresh request id, then place the
    /// duplicate and discard the original ([`Duplicates`]).
    Duplicate,
}

/// The one action waiting for its confirming second click, keyed by the
/// session it acts on, so it never carries over to another session.
#[derive(Debug, Default)]
pub(crate) struct ActionConfirm {
    armed: Option<(String, SessionAction)>,
}

/// The sessions being restarted or resumed, by the request id of their
/// `DuplicateSession`.
#[derive(Debug, Default)]
pub(crate) struct Duplicates {
    pending: HashMap<String, String>,
}

/// How a duplicate that arrived takes its original's place.
#[derive(Debug, Clone)]
pub(crate) struct Placed {
    /// Where the duplicate goes, then the original's discard.
    pub(crate) messages: Vec<ClientMessage>,
    /// The first pane that showed the original, which the duplicate takes
    /// over and which gets the focus; `None` when it opens a new tab.
    pub(crate) focus: Option<(String, String)>,
}

pub(crate) fn action_state(session: &SessionSnapshot) -> ActionState {
    if session.is_inactive {
        ActionState::Inactive
    } else if session.is_abandoned {
        ActionState::Abandoned
    } else if matches!(
        session.status,
        SessionStatus::Stopped | SessionStatus::Error
    ) {
        ActionState::Stopped
    } else {
        ActionState::Running
    }
}

/// Whether the pane header gives the exit code instead of Stop: any
/// stopped session, as in `SessionPane.tsx`'s header.
pub(crate) fn header_shows_exit_code(session: &SessionSnapshot) -> bool {
    session.status == SessionStatus::Stopped
}

/// Whether the stopped-pane overlay covers the terminal: a stopped session
/// that was not abandoned.
pub(crate) fn pane_shows_exit(session: &SessionSnapshot) -> bool {
    header_shows_exit_code(session) && !session.is_abandoned
}

fn exit_code(session: &SessionSnapshot) -> String {
    session
        .exit_code
        .map_or_else(|| "?".to_owned(), |code| code.to_string())
}

/// The pane header's note for an exited session.
pub(crate) fn exit_code_label(session: &SessionSnapshot) -> String {
    format!("exit code {}", exit_code(session))
}

/// The stopped-pane overlay's message.
pub(crate) fn exited_message(session: &SessionSnapshot) -> String {
    format!("Session exited (code {})", exit_code(session))
}

/// The actions a session offers, by its state; the worktree variants only
/// when it has a worktree of its own.
pub(crate) fn menu_actions(session: &SessionSnapshot) -> Vec<SessionAction> {
    let worktree = session.has_per_session_worktree;
    match action_state(session) {
        ActionState::Running => {
            let mut actions = vec![SessionAction::Rename, SessionAction::StopKeepWorktree];
            if worktree {
                actions.push(SessionAction::StopDeleteWorktree);
            }
            actions
        }
        ActionState::Stopped => overlay_actions(session),
        ActionState::Inactive => {
            let mut actions = vec![SessionAction::Resume, SessionAction::RemoveFromSidebar];
            if worktree {
                actions.push(SessionAction::RemoveFromSidebarDeleteWorktree);
            }
            actions
        }
        ActionState::Abandoned => vec![
            SessionAction::ResumeAbandoned,
            SessionAction::DismissAbandoned,
        ],
    }
}

/// The stopped-pane overlay's buttons: Restart, then keep the worktree
/// (when there is one), then remove the pane, deleting the worktree when
/// there is one.
pub(crate) fn overlay_actions(session: &SessionSnapshot) -> Vec<SessionAction> {
    if session.has_per_session_worktree {
        vec![
            SessionAction::Restart,
            SessionAction::Park,
            SessionAction::RemovePaneDeleteWorktree,
        ]
    } else {
        vec![SessionAction::Restart, SessionAction::RemovePane]
    }
}

/// The context menu's rows in `mode`. The stop choices list the worktree
/// delete first, as the Tauri confirm does; a session with no stop choices
/// left (it stopped meanwhile) shows its actions instead.
pub(crate) fn menu_entries(session: &SessionSnapshot, mode: MenuMode) -> Vec<MenuEntry> {
    let actions = menu_actions(session);
    let stops: Vec<MenuEntry> = actions
        .iter()
        .rev()
        .filter(|action| action.is_stop())
        .map(|&action| MenuEntry::Action(action))
        .collect();
    if mode == MenuMode::StopChoice && !stops.is_empty() {
        let mut entries = stops;
        entries.push(MenuEntry::Cancel);
        return entries;
    }
    let mut entries: Vec<MenuEntry> = actions
        .iter()
        .filter(|action| !action.is_stop())
        .map(|&action| MenuEntry::Action(action))
        .collect();
    if !stops.is_empty() {
        entries.push(MenuEntry::StopChoice);
    }
    entries
}

/// The rename a submitted name sends; a blank name restores the default.
pub(crate) fn rename_message(session_id: &str, text: &str) -> ClientMessage {
    let label = text.trim();
    ClientMessage::RenameSession {
        session_id: session_id.to_owned(),
        label: (!label.is_empty()).then(|| label.to_owned()),
    }
}

/// What `action` does to `session`; `shown` says whether a pane shows it.
/// A stop that leaves no pane behind also parks the session when it has a
/// worktree to keep, else discards it, as `SessionContextMenu.tsx`'s
/// `sendStop` does. Deleting a worktree discards with one cleanup per
/// member repo.
pub(crate) fn plan(action: SessionAction, session: &SessionSnapshot, shown: bool) -> Step {
    let id = session.id.clone();
    let stop = || ClientMessage::StopSession {
        session_id: id.clone(),
        cleanup: Vec::new(),
    };
    let discard = |cleanup| ClientMessage::DiscardSession {
        session_id: id.clone(),
        cleanup,
    };
    let park = || ClientMessage::ParkSession {
        session_id: id.clone(),
    };
    let messages = match action {
        SessionAction::Rename => return Step::EditName,
        SessionAction::Restart | SessionAction::Resume => return Step::Duplicate,
        SessionAction::StopKeepWorktree if shown => vec![stop()],
        SessionAction::StopKeepWorktree if session.has_per_session_worktree => vec![stop(), park()],
        SessionAction::StopKeepWorktree => vec![stop(), discard(Vec::new())],
        SessionAction::StopDeleteWorktree => vec![stop(), discard(delete_worktrees(session))],
        SessionAction::Park => vec![park()],
        SessionAction::RemovePane | SessionAction::RemoveFromSidebar => {
            vec![discard(Vec::new())]
        }
        SessionAction::RemovePaneDeleteWorktree
        | SessionAction::RemoveFromSidebarDeleteWorktree => {
            vec![discard(delete_worktrees(session))]
        }
        SessionAction::ResumeAbandoned => vec![ClientMessage::ResumeAbandoned {
            session_id: id.clone(),
        }],
        SessionAction::DismissAbandoned => vec![ClientMessage::DiscardAbandoned {
            session_id: id.clone(),
        }],
    };
    Step::Send(messages)
}

fn delete_worktrees(session: &SessionSnapshot) -> Vec<CleanupAction> {
    session
        .members
        .iter()
        .map(|member| CleanupAction {
            repo_id: member.repo_id.clone(),
            remove_worktree: true,
            branch: BranchCleanup::Auto,
        })
        .collect()
}

impl SessionAction {
    /// The action's part of its debug selector.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Rename => "rename",
            Self::StopKeepWorktree => "stop-keep",
            Self::StopDeleteWorktree => "stop-delete",
            Self::Restart => "restart",
            Self::Park => "park",
            Self::RemovePane => "remove-pane",
            Self::RemovePaneDeleteWorktree => "remove-pane-delete",
            Self::Resume => "resume",
            Self::RemoveFromSidebar => "remove",
            Self::RemoveFromSidebarDeleteWorktree => "remove-delete",
            Self::ResumeAbandoned => "resume-abandoned",
            Self::DismissAbandoned => "dismiss",
        }
    }

    /// The action's text. A stop that keeps nothing says so plainly, and a
    /// stopped session no pane shows is removed as a session, not a pane.
    pub(crate) fn label(self, has_worktree: bool, in_pane: bool) -> &'static str {
        match self {
            Self::Rename => "Rename…",
            Self::StopKeepWorktree if has_worktree => "Stop, keep worktree",
            Self::StopKeepWorktree => "Stop session",
            Self::StopDeleteWorktree => "Stop and delete worktree",
            Self::Restart => "Restart",
            Self::Park if in_pane => "Remove pane, keep worktree",
            Self::Park => "Remove session, keep worktree",
            Self::RemovePane if in_pane => "Remove pane",
            Self::RemovePane => "Remove session",
            Self::RemovePaneDeleteWorktree if in_pane => "Remove pane and delete worktree",
            Self::RemovePaneDeleteWorktree => "Remove session and delete worktree",
            Self::Resume | Self::ResumeAbandoned => "Resume",
            Self::RemoveFromSidebar => "Remove from sidebar",
            Self::RemoveFromSidebarDeleteWorktree => "Remove from sidebar and delete worktree",
            Self::DismissAbandoned => "Dismiss",
        }
    }

    /// The text of a restart or resume while its duplicate is on the way.
    pub(crate) fn pending_label(self) -> Option<&'static str> {
        match self {
            Self::Restart => Some("Restarting…"),
            Self::Resume => Some("Resuming…"),
            _ => None,
        }
    }

    /// Whether the action deletes a worktree, and so asks twice.
    pub(crate) fn deletes_worktree(self) -> bool {
        matches!(
            self,
            Self::StopDeleteWorktree
                | Self::RemovePaneDeleteWorktree
                | Self::RemoveFromSidebarDeleteWorktree
        )
    }

    fn is_stop(self) -> bool {
        matches!(self, Self::StopKeepWorktree | Self::StopDeleteWorktree)
    }
}

impl ActionConfirm {
    /// A click on `action` for `session_id`: true when exactly that was
    /// armed (and disarms it), else arms it and returns false.
    pub(crate) fn click(&mut self, session_id: &str, action: SessionAction) -> bool {
        if self.is_armed(session_id, action) {
            self.armed = None;
            true
        } else {
            self.armed = Some((session_id.to_owned(), action));
            false
        }
    }

    pub(crate) fn is_armed(&self, session_id: &str, action: SessionAction) -> bool {
        self.armed
            .as_ref()
            .is_some_and(|(id, armed)| id == session_id && *armed == action)
    }

    pub(crate) fn armed(&self) -> Option<(&str, SessionAction)> {
        self.armed
            .as_ref()
            .map(|(id, action)| (id.as_str(), *action))
    }

    /// Returns whether anything was armed.
    pub(crate) fn disarm(&mut self) -> bool {
        self.armed.take().is_some()
    }
}

impl Duplicates {
    /// Records a restart of `session_id` under `request_id` and returns the
    /// request to send; `None` while one for the session is on the way.
    pub(crate) fn request(
        &mut self,
        session_id: &str,
        request_id: String,
    ) -> Option<ClientMessage> {
        if self.is_pending(session_id) {
            return None;
        }
        self.pending
            .insert(request_id.clone(), session_id.to_owned());
        Some(ClientMessage::DuplicateSession {
            session_id: session_id.to_owned(),
            request_id: Some(request_id),
        })
    }

    /// Whether a restart or resume of `session_id` waits for its reply.
    pub(crate) fn is_pending(&self, session_id: &str) -> bool {
        self.pending.values().any(|original| original == session_id)
    }

    /// Whether `request_id` is one of this client's restarts.
    pub(crate) fn has_request(&self, request_id: &str) -> bool {
        self.pending.contains_key(request_id)
    }

    /// A failure reply for `request_id`; returns whether it was pending.
    pub(crate) fn fail(&mut self, request_id: &str) -> bool {
        self.pending.remove(request_id).is_some()
    }

    /// Forgets every restart, as a new connection must.
    pub(crate) fn clear(&mut self) {
        self.pending.clear();
    }

    /// When `request_id` names a pending restart: the messages that put
    /// `new_id` where the original was (every pane that showed it, or a new
    /// tab when none did) and then discard the original. The discard goes
    /// last so the daemon has rebound the panes before it closes the
    /// original's, as `App.tsx`'s restart handler orders it.
    pub(crate) fn place(
        &mut self,
        request_id: &str,
        new_id: &str,
        bindings: &[PaneBinding],
    ) -> Option<Placed> {
        let original = self.pending.remove(request_id)?;
        let panes: Vec<&PaneBinding> = bindings
            .iter()
            .filter(|binding| binding.session_id.as_deref() == Some(original.as_str()))
            .collect();
        let mut messages: Vec<ClientMessage> = panes
            .iter()
            .map(|binding| ClientMessage::ReplacePaneSession {
                tab_id: binding.tab_id.clone(),
                pane_id: binding.pane_id.clone(),
                session_id: Some(new_id.to_owned()),
            })
            .collect();
        if panes.is_empty() {
            messages.push(ClientMessage::CreateTab {
                name: None,
                initial_session_id: Some(new_id.to_owned()),
            });
        }
        messages.push(ClientMessage::DiscardSession {
            session_id: original,
            cleanup: Vec::new(),
        });
        Some(Placed {
            messages,
            focus: panes
                .first()
                .map(|binding| (binding.tab_id.clone(), binding.pane_id.clone())),
        })
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert preconditions with expect and panic; failure messages aid debugging"
)]
mod tests {
    use super::{
        ActionConfirm, ActionState, Duplicates, MenuEntry, MenuMode, SessionAction, Step,
        action_state, exit_code_label, exited_message, header_shows_exit_code, menu_actions,
        menu_entries, overlay_actions, pane_shows_exit, plan, rename_message,
    };
    use crate::tabs::PaneBinding;
    use protocol::{BranchCleanup, CleanupAction, ClientMessage, SessionSnapshot};
    use serde_json::json;

    use SessionAction as A;

    fn session(status: &str) -> SessionSnapshot {
        serde_json::from_value(json!({
            "id": "s1",
            "label": "repo:main",
            "kind": "single",
            "members": [
                { "repo_id": "r1", "repo_name": "r1", "branch": "wt/a", "worktree_path": "" },
                { "repo_id": "r2", "repo_name": "r2", "branch": "wt/a", "worktree_path": "" },
            ],
            "status": status,
            "mode": "interactive",
            "started_at": "2026-01-01T00:00:00Z",
            "exit_code": null,
            "metrics": { "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0, "last_activity_at": null },
            "recent_actions": [],
            "agent": "claude",
        }))
        .expect("session fixture")
    }

    fn with_worktree(mut s: SessionSnapshot) -> SessionSnapshot {
        s.has_per_session_worktree = true;
        s
    }

    fn inactive(mut s: SessionSnapshot) -> SessionSnapshot {
        s.is_inactive = true;
        s
    }

    fn abandoned() -> SessionSnapshot {
        let mut s = session("stopped");
        s.is_abandoned = true;
        s
    }

    fn sent(step: Step) -> Vec<ClientMessage> {
        match step {
            Step::Send(messages) => messages,
            other => panic!("expected messages, got {other:?}"),
        }
    }

    fn delete_all() -> Vec<CleanupAction> {
        ["r1", "r2"]
            .into_iter()
            .map(|repo_id| CleanupAction {
                repo_id: repo_id.to_owned(),
                remove_worktree: true,
                branch: BranchCleanup::Auto,
            })
            .collect()
    }

    fn is_discard(msg: &ClientMessage, cleanup: &[CleanupAction]) -> bool {
        matches!(msg, ClientMessage::DiscardSession { session_id, cleanup: c } if session_id == "s1" && c == cleanup)
    }

    fn is_stop(msg: &ClientMessage) -> bool {
        matches!(msg, ClientMessage::StopSession { session_id, cleanup } if session_id == "s1" && cleanup.is_empty())
    }

    #[test]
    fn actions_state_follows_the_tauri_buckets() {
        for status in ["spawning", "idle", "working", "awaiting_input"] {
            assert_eq!(
                action_state(&session(status)),
                ActionState::Running,
                "{status}"
            );
        }
        assert_eq!(action_state(&session("stopped")), ActionState::Stopped);
        assert_eq!(action_state(&session("error")), ActionState::Stopped);
        assert_eq!(
            action_state(&inactive(session("stopped"))),
            ActionState::Inactive,
            "parked wins over stopped"
        );
        assert_eq!(action_state(&abandoned()), ActionState::Abandoned);
    }

    #[test]
    fn actions_by_state_offer_worktree_variants_only_with_a_worktree() {
        assert_eq!(
            menu_actions(&session("idle")),
            [A::Rename, A::StopKeepWorktree]
        );
        assert_eq!(
            menu_actions(&with_worktree(session("idle"))),
            [A::Rename, A::StopKeepWorktree, A::StopDeleteWorktree]
        );
        assert_eq!(
            menu_actions(&session("stopped")),
            [A::Restart, A::RemovePane]
        );
        assert_eq!(
            menu_actions(&with_worktree(session("stopped"))),
            [A::Restart, A::Park, A::RemovePaneDeleteWorktree]
        );
        assert_eq!(
            menu_actions(&inactive(session("stopped"))),
            [A::Resume, A::RemoveFromSidebar]
        );
        assert_eq!(
            menu_actions(&inactive(with_worktree(session("stopped")))),
            [
                A::Resume,
                A::RemoveFromSidebar,
                A::RemoveFromSidebarDeleteWorktree
            ]
        );
        assert_eq!(
            overlay_actions(&with_worktree(session("stopped"))),
            [A::Restart, A::Park, A::RemovePaneDeleteWorktree]
        );
        assert_eq!(
            menu_actions(&with_worktree(abandoned())),
            [A::ResumeAbandoned, A::DismissAbandoned],
            "no Restart and no worktree delete for an abandoned session"
        );
    }

    #[test]
    fn actions_menu_folds_the_stop_choices_under_one_entry() {
        let running = with_worktree(session("idle"));
        assert_eq!(
            menu_entries(&running, MenuMode::Actions),
            [MenuEntry::Action(A::Rename), MenuEntry::StopChoice]
        );
        assert_eq!(
            menu_entries(&running, MenuMode::StopChoice),
            [
                MenuEntry::Action(A::StopDeleteWorktree),
                MenuEntry::Action(A::StopKeepWorktree),
                MenuEntry::Cancel
            ]
        );
        assert_eq!(
            menu_entries(&session("idle"), MenuMode::StopChoice),
            [MenuEntry::Action(A::StopKeepWorktree), MenuEntry::Cancel]
        );
        assert_eq!(
            menu_entries(&session("stopped"), MenuMode::Actions),
            [
                MenuEntry::Action(A::Restart),
                MenuEntry::Action(A::RemovePane)
            ]
        );
    }

    #[test]
    fn actions_stop_choice_of_a_session_that_stopped_shows_its_actions() {
        assert_eq!(
            menu_entries(&session("stopped"), MenuMode::StopChoice),
            [
                MenuEntry::Action(A::Restart),
                MenuEntry::Action(A::RemovePane)
            ]
        );
    }

    #[test]
    fn actions_labels_and_confirm_flags() {
        assert_eq!(A::StopKeepWorktree.label(true, true), "Stop, keep worktree");
        assert_eq!(A::StopKeepWorktree.label(false, true), "Stop session");
        assert_eq!(A::Park.label(true, true), "Remove pane, keep worktree");
        assert_eq!(A::Park.label(true, false), "Remove session, keep worktree");
        assert_eq!(A::RemovePane.label(false, false), "Remove session");
        assert_eq!(
            A::RemovePaneDeleteWorktree.label(true, true),
            "Remove pane and delete worktree"
        );
        assert_eq!(
            A::RemovePaneDeleteWorktree.label(true, false),
            "Remove session and delete worktree"
        );
        assert_eq!(A::Restart.pending_label(), Some("Restarting…"));
        assert_eq!(A::Park.pending_label(), None);
        let deleting: Vec<SessionAction> = [
            A::Rename,
            A::StopKeepWorktree,
            A::StopDeleteWorktree,
            A::Restart,
            A::Park,
            A::RemovePane,
            A::RemovePaneDeleteWorktree,
            A::Resume,
            A::RemoveFromSidebar,
            A::RemoveFromSidebarDeleteWorktree,
            A::ResumeAbandoned,
            A::DismissAbandoned,
        ]
        .into_iter()
        .filter(|a| a.deletes_worktree())
        .collect();
        assert_eq!(
            deleting,
            [
                A::StopDeleteWorktree,
                A::RemovePaneDeleteWorktree,
                A::RemoveFromSidebarDeleteWorktree
            ]
        );
    }

    #[test]
    fn actions_exit_code_texts() {
        let mut s = session("stopped");
        assert!(pane_shows_exit(&s));
        assert_eq!(exit_code_label(&s), "exit code ?");
        s.exit_code = Some(3);
        assert_eq!(exit_code_label(&s), "exit code 3");
        assert_eq!(exited_message(&s), "Session exited (code 3)");
        assert!(
            !pane_shows_exit(&session("error")),
            "only stopped, as in SessionPane"
        );
        assert!(!pane_shows_exit(&session("idle")));
        assert!(!pane_shows_exit(&abandoned()), "abandoned gets no overlay");
        assert!(header_shows_exit_code(&abandoned()));
    }

    #[test]
    fn actions_rename_trims_and_blank_restores_the_default() {
        assert!(matches!(
            rename_message("s1", "  work  "),
            ClientMessage::RenameSession { session_id, label: Some(l) } if session_id == "s1" && l == "work"
        ));
        assert!(matches!(
            rename_message("s1", "   "),
            ClientMessage::RenameSession { label: None, .. }
        ));
    }

    #[test]
    fn actions_stop_parks_or_discards_only_without_a_pane() {
        let shown = sent(plan(
            A::StopKeepWorktree,
            &with_worktree(session("idle")),
            true,
        ));
        assert!(
            matches!(shown.as_slice(), [stop] if is_stop(stop)),
            "{shown:?}"
        );

        let parked = sent(plan(
            A::StopKeepWorktree,
            &with_worktree(session("idle")),
            false,
        ));
        assert!(
            matches!(parked.as_slice(), [stop, ClientMessage::ParkSession { session_id }] if is_stop(stop) && session_id == "s1"),
            "{parked:?}"
        );

        let discarded = sent(plan(A::StopKeepWorktree, &session("idle"), false));
        assert!(
            matches!(discarded.as_slice(), [stop, discard] if is_stop(stop) && is_discard(discard, &[])),
            "{discarded:?}"
        );
    }

    #[test]
    fn actions_worktree_deletes_send_one_cleanup_per_member() {
        let running = with_worktree(session("idle"));
        let stop = sent(plan(A::StopDeleteWorktree, &running, true));
        assert!(
            matches!(stop.as_slice(), [s, d] if is_stop(s) && is_discard(d, &delete_all())),
            "{stop:?}"
        );
        let stopped = with_worktree(session("stopped"));
        for action in [
            A::RemovePaneDeleteWorktree,
            A::RemoveFromSidebarDeleteWorktree,
        ] {
            let msgs = sent(plan(action, &stopped, true));
            assert!(
                matches!(msgs.as_slice(), [d] if is_discard(d, &delete_all())),
                "{msgs:?}"
            );
        }
        for action in [A::RemovePane, A::RemoveFromSidebar] {
            let msgs = sent(plan(action, &stopped, false));
            assert!(
                matches!(msgs.as_slice(), [d] if is_discard(d, &[])),
                "{msgs:?}"
            );
        }
        let park = sent(plan(A::Park, &stopped, true));
        assert!(
            matches!(park.as_slice(), [ClientMessage::ParkSession { session_id }] if session_id == "s1")
        );
    }

    #[test]
    fn actions_abandoned_resume_and_dismiss_use_the_abandoned_messages() {
        let resume = sent(plan(A::ResumeAbandoned, &abandoned(), true));
        assert!(
            matches!(resume.as_slice(), [ClientMessage::ResumeAbandoned { session_id }] if session_id == "s1"),
            "{resume:?}"
        );
        let dismiss = sent(plan(A::DismissAbandoned, &abandoned(), true));
        assert!(
            matches!(dismiss.as_slice(), [ClientMessage::DiscardAbandoned { session_id }] if session_id == "s1"),
            "{dismiss:?}"
        );
    }

    #[test]
    fn actions_rename_restart_and_resume_are_not_plain_sends() {
        let s = session("stopped");
        assert!(matches!(
            plan(A::Rename, &session("idle"), true),
            Step::EditName
        ));
        assert!(matches!(plan(A::Restart, &s, true), Step::Duplicate));
        assert!(matches!(
            plan(A::Resume, &inactive(s), false),
            Step::Duplicate
        ));
    }

    #[test]
    fn actions_confirm_is_keyed_by_session_and_action() {
        let mut confirm = ActionConfirm::default();
        assert!(!confirm.click("s1", A::RemovePaneDeleteWorktree));
        assert!(confirm.is_armed("s1", A::RemovePaneDeleteWorktree));
        assert!(
            !confirm.click("s2", A::RemovePaneDeleteWorktree),
            "another session arms its own"
        );
        assert!(!confirm.is_armed("s1", A::RemovePaneDeleteWorktree));
        assert!(
            !confirm.click("s2", A::StopDeleteWorktree),
            "another action too"
        );
        assert!(confirm.click("s2", A::StopDeleteWorktree));
        assert_eq!(confirm.armed(), None, "acting disarms");
        assert!(!confirm.disarm());
        confirm.click("s1", A::StopKeepWorktree);
        assert!(confirm.disarm());
        assert!(
            !confirm.click("s1", A::StopKeepWorktree),
            "a disarm needs two clicks again"
        );
    }

    fn binding(tab: &str, pane: &str, session: Option<&str>) -> PaneBinding {
        PaneBinding {
            tab_id: tab.to_owned(),
            pane_id: pane.to_owned(),
            session_id: session.map(str::to_owned),
        }
    }

    #[test]
    fn actions_duplicate_replaces_every_pane_of_the_original_then_discards_it() {
        let mut dups = Duplicates::default();
        let request = dups.request("s1", "req-1".to_owned());
        assert!(matches!(
            request,
            Some(ClientMessage::DuplicateSession { session_id, request_id: Some(id) }) if session_id == "s1" && id == "req-1"
        ));
        let bindings = [
            binding("t1", "p1", Some("s1")),
            binding("t1", "p2", Some("s9")),
            binding("t2", "p3", Some("s1")),
        ];
        assert!(dups.place("other", "s2", &bindings).is_none());
        let placed = dups
            .place("req-1", "s2", &bindings)
            .expect("the matching reply places the duplicate");
        assert_eq!(placed.focus, Some(("t1".to_owned(), "p1".to_owned())));
        let replaced: Vec<(&str, &str)> = placed
            .messages
            .iter()
            .filter_map(|m| match m {
                ClientMessage::ReplacePaneSession {
                    tab_id,
                    pane_id,
                    session_id,
                } if session_id.as_deref() == Some("s2") => {
                    Some((tab_id.as_str(), pane_id.as_str()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(replaced, [("t1", "p1"), ("t2", "p3")]);
        assert!(
            placed.messages.last().is_some_and(|m| is_discard(m, &[])),
            "the discard goes last: {:?}",
            placed.messages
        );
        assert!(
            dups.place("req-1", "s3", &bindings).is_none(),
            "each request is placed once"
        );
    }

    #[test]
    fn actions_duplicate_of_a_session_no_pane_shows_opens_a_tab() {
        let mut dups = Duplicates::default();
        dups.request("s1", "req-1".to_owned());
        let placed = dups
            .place("req-1", "s2", &[binding("t1", "p1", None)])
            .expect("placed");
        assert_eq!(placed.focus, None);
        assert!(
            matches!(placed.messages.as_slice(), [
                ClientMessage::CreateTab { name: None, initial_session_id: Some(id) },
                discard,
            ] if id == "s2" && is_discard(discard, &[])),
            "{:?}",
            placed.messages
        );
    }

    #[test]
    fn actions_duplicate_is_one_at_a_time_until_answered_or_cleared() {
        let mut dups = Duplicates::default();
        assert!(dups.request("s1", "req-1".to_owned()).is_some());
        assert!(dups.is_pending("s1"));
        assert!(dups.has_request("req-1"));
        assert!(
            dups.request("s1", "req-2".to_owned()).is_none(),
            "a second restart of s1 while the first is on the way"
        );
        assert!(dups.request("s2", "req-3".to_owned()).is_some());
        assert!(!dups.fail("unknown"));
        assert!(dups.fail("req-1"));
        assert!(!dups.is_pending("s1"));
        assert!(dups.request("s1", "req-4".to_owned()).is_some());
        dups.clear();
        assert!(!dups.is_pending("s1") && !dups.is_pending("s2"));
    }
}
