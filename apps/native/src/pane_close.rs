//! The pane-close dialog as plain data: what it says, its buttons in the
//! keyboard's order, and the messages each answer sends.

use protocol::{BranchCleanup, CleanupAction, ClientMessage, SessionSnapshot, SessionStatus};

pub(crate) const TITLE: &str = "Close pane?";
pub(crate) const NOTE: &str = "Closing the pane only removes it from this tab. Keeping the session leaves it running in the sidebar so you can re-bind it later. Closing the session stops the underlying process and removes it from the sidebar.";

/// A button of the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Control {
    Cancel,
    /// Closes the pane and leaves the session alone.
    PaneOnly,
    /// Closes the pane, stops the session and discards it, keeping its
    /// worktree.
    Discard,
    /// Hands over to the delete-worktree confirm.
    Delete,
    /// The ✕ in the title row.
    Dismiss,
}

impl Control {
    pub(crate) fn selector(self) -> &'static str {
        match self {
            Self::Cancel => "pane-close-cancel",
            Self::PaneOnly => "pane-close-only",
            Self::Discard => "pane-close-discard",
            Self::Delete => "pane-close-delete",
            Self::Dismiss => "pane-close-dismiss",
        }
    }

    /// Whether the button shows in the danger colour.
    pub(crate) fn danger(self) -> bool {
        matches!(self, Self::Discard | Self::Delete)
    }
}

/// The open pane-close dialog: the pane, the session it shows and the
/// focused button.
#[derive(Debug, Clone)]
pub(crate) struct PaneClose {
    tab_id: String,
    pane_id: String,
    session_id: String,
    label: String,
    status: SessionStatus,
    has_worktree: bool,
    repo_ids: Vec<String>,
    focus: Control,
}

impl PaneClose {
    /// The dialog for `pane_id` of `tab_id`, which shows `session` under
    /// `label`; the pane-only button has the focus.
    pub(crate) fn new(
        tab_id: &str,
        pane_id: &str,
        session: &SessionSnapshot,
        label: String,
    ) -> Self {
        Self {
            tab_id: tab_id.to_owned(),
            pane_id: pane_id.to_owned(),
            session_id: session.id.clone(),
            label,
            status: session.status,
            has_worktree: session.has_per_session_worktree,
            repo_ids: session.members.iter().map(|m| m.repo_id.clone()).collect(),
            focus: Control::PaneOnly,
        }
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn tab_id(&self) -> &str {
        &self.tab_id
    }

    pub(crate) fn pane_id(&self) -> &str {
        &self.pane_id
    }

    fn stopped(&self) -> bool {
        self.status == SessionStatus::Stopped
    }

    /// The question under the title.
    pub(crate) fn body(&self) -> String {
        let stopped = if self.stopped() {
            " (already stopped)"
        } else {
            ""
        };
        format!(
            "This pane is showing {}{stopped}. What would you like to do?",
            self.label
        )
    }

    /// The buttons in the keyboard's order: the footer's, then ✕.
    pub(crate) fn controls(&self) -> Vec<Control> {
        let mut controls = vec![Control::Cancel, Control::PaneOnly, Control::Discard];
        if self.has_worktree {
            controls.push(Control::Delete);
        }
        controls.push(Control::Dismiss);
        controls
    }

    pub(crate) fn label(&self, control: Control) -> String {
        let label = match control {
            Control::Cancel => "Cancel",
            Control::PaneOnly if self.has_worktree => "Close pane, keep session, keep worktree",
            Control::PaneOnly => "Close pane but keep session in sidebar",
            Control::Discard if self.has_worktree => {
                "Close pane, don't keep session, keep worktree"
            }
            Control::Discard if self.stopped() => "Close pane and remove session",
            Control::Discard => "Close pane and close session",
            Control::Delete => "Close pane, don't keep session, delete worktree",
            Control::Dismiss => "✕",
        };
        label.to_owned()
    }

    pub(crate) fn focused(&self) -> Control {
        self.focus
    }

    /// Moves the focus to the next button, or the previous one when not
    /// `forward`, wrapping at either end.
    pub(crate) fn move_focus(&mut self, forward: bool) {
        let controls = self.controls();
        let at = controls.iter().position(|c| *c == self.focus).unwrap_or(0);
        let len = controls.len();
        let next = if forward {
            (at + 1) % len
        } else {
            (at + len - 1) % len
        };
        self.focus = controls[next];
    }

    pub(crate) fn close_pane_message(&self) -> ClientMessage {
        ClientMessage::ClosePane {
            tab_id: self.tab_id.clone(),
            pane_id: self.pane_id.clone(),
        }
    }

    /// Close the pane, stop the session unless it has already ended, and
    /// discard it keeping every member's worktree, the branch left to the
    /// daemon's automatic choice.
    pub(crate) fn discard_keep_messages(&self) -> Vec<ClientMessage> {
        let mut messages = vec![self.close_pane_message()];
        if !matches!(self.status, SessionStatus::Stopped | SessionStatus::Error) {
            messages.push(ClientMessage::StopSession {
                session_id: self.session_id.clone(),
                cleanup: Vec::new(),
            });
        }
        let cleanup = self
            .repo_ids
            .iter()
            .map(|repo_id| CleanupAction {
                repo_id: repo_id.clone(),
                remove_worktree: false,
                branch: BranchCleanup::Auto,
            })
            .collect();
        messages.push(ClientMessage::DiscardSession {
            session_id: self.session_id.clone(),
            cleanup,
        });
        messages
    }
}

#[cfg(test)]
mod tests {
    use protocol::{BranchCleanup, ClientMessage, SessionStatus};

    use super::*;
    use crate::tabs::tests::{session, wire};

    fn dialog(status: SessionStatus, worktree: bool, repos: &[&str]) -> PaneClose {
        let mut s = session("s1", repos.first().copied(), None);
        s.status = status;
        s.has_per_session_worktree = worktree;
        s.members = repos
            .iter()
            .map(|repo| protocol::SessionMember {
                repo_id: (*repo).to_owned(),
                repo_name: (*repo).to_owned(),
                branch: "wt/x".to_owned(),
                worktree_path: String::new(),
            })
            .collect();
        PaneClose::new("t1", "p1", &s, "work".to_owned())
    }

    fn labels(dialog: &PaneClose) -> Vec<String> {
        dialog
            .controls()
            .into_iter()
            .map(|control| dialog.label(control))
            .collect()
    }

    #[test]
    fn pane_close_labels_by_worktree_and_stopped() {
        let plain = dialog(SessionStatus::Idle, false, &["r1"]);
        assert_eq!(
            labels(&plain),
            [
                "Cancel",
                "Close pane but keep session in sidebar",
                "Close pane and close session",
                "✕"
            ]
        );
        assert_eq!(plain.focused(), Control::PaneOnly, "pane-only is focused");
        assert_eq!(
            plain.body(),
            "This pane is showing work. What would you like to do?"
        );

        let stopped = dialog(SessionStatus::Stopped, false, &["r1"]);
        assert_eq!(
            stopped.label(Control::Discard),
            "Close pane and remove session"
        );
        assert_eq!(
            stopped.body(),
            "This pane is showing work (already stopped). What would you like to do?"
        );

        let worktree = dialog(SessionStatus::Stopped, true, &["r1"]);
        assert_eq!(
            labels(&worktree),
            [
                "Cancel",
                "Close pane, keep session, keep worktree",
                "Close pane, don't keep session, keep worktree",
                "Close pane, don't keep session, delete worktree",
                "✕"
            ]
        );
        assert_eq!(
            worktree
                .controls()
                .into_iter()
                .map(Control::selector)
                .collect::<Vec<_>>(),
            [
                "pane-close-cancel",
                "pane-close-only",
                "pane-close-discard",
                "pane-close-delete",
                "pane-close-dismiss"
            ]
        );
    }

    #[test]
    fn pane_close_focus_cycles_both_ways() {
        let mut plain = dialog(SessionStatus::Idle, false, &["r1"]);
        plain.move_focus(true);
        assert_eq!(plain.focused(), Control::Discard);
        plain.move_focus(true);
        assert_eq!(plain.focused(), Control::Dismiss);
        plain.move_focus(true);
        assert_eq!(plain.focused(), Control::Cancel, "wraps to the start");
        plain.move_focus(false);
        assert_eq!(plain.focused(), Control::Dismiss, "and back");
    }

    #[test]
    fn discard_keep_skips_stop_when_stopped_or_error() {
        let kinds = |dialog: &PaneClose| -> Vec<String> {
            dialog
                .discard_keep_messages()
                .iter()
                .map(|msg| wire(msg)["type"].as_str().unwrap_or_default().to_owned())
                .collect()
        };
        let live = dialog(SessionStatus::Working, false, &["r1"]);
        assert_eq!(
            kinds(&live),
            ["close_pane", "stop_session", "discard_session"]
        );
        for status in [SessionStatus::Stopped, SessionStatus::Error] {
            let ended = dialog(status, false, &["r1"]);
            assert_eq!(
                kinds(&ended),
                ["close_pane", "discard_session"],
                "{status:?}"
            );
        }
    }

    #[test]
    fn discard_keep_sends_keep_worktree_cleanup_per_member() {
        let two = dialog(SessionStatus::Stopped, true, &["r1", "r2"]);
        let messages = two.discard_keep_messages();
        assert!(
            matches!(&messages[0], ClientMessage::ClosePane { tab_id, pane_id } if tab_id == "t1" && pane_id == "p1"),
            "{messages:?}"
        );
        let Some(ClientMessage::DiscardSession {
            session_id,
            cleanup,
        }) = messages.last()
        else {
            unreachable!("no discard in {messages:?}");
        };
        assert_eq!(session_id, "s1");
        let repos: Vec<&str> = cleanup.iter().map(|c| c.repo_id.as_str()).collect();
        assert_eq!(repos, ["r1", "r2"]);
        assert!(
            cleanup
                .iter()
                .all(|c| !c.remove_worktree && c.branch == BranchCleanup::Auto),
            "{cleanup:?}"
        );
        assert!(matches!(
            two.close_pane_message(),
            ClientMessage::ClosePane { .. }
        ));
    }
}
