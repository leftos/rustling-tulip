//! The quit flow's rules: which sessions count as active, what the exit
//! dialog offers and says, the branch-fate walk over the worktree sessions
//! before a "remove worktrees" quit, the messages that quit sends, and the
//! wait for the daemon's answer. Mirrors the Tauri app's
//! `ExitConfirmDialog.tsx`, `utils/exitWorktreeQueue.ts` and the exit half of
//! `App.tsx`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use protocol::{BranchCleanup, CleanupAction, ClientMessage, SessionSnapshot, SessionStatus};

/// How long after a shutdown is sent the dialog waits for the daemon's
/// answer before it offers Force quit.
pub(crate) const STUCK_AFTER: Duration = Duration::from_secs(5);

pub(crate) const TITLE: &str = "Quit rustling-tulip?";
pub(crate) const BODY: &str = "The background daemon owns your claude sessions. By default it keeps \
     running after the app closes so sessions survive app restarts.";
/// The error line when a stop could not reach the daemon at all.
pub(crate) const SHUTDOWN_FAILED: &str =
    "The connection to the daemon dropped before anything was stopped. Nothing was changed.";
const STOPPING: &str = "Stopping…";

/// The warning once the daemon has not answered in [`STUCK_AFTER`].
pub(crate) fn stuck_warning() -> String {
    format!(
        "Daemon not responding after {} seconds.",
        STUCK_AFTER.as_secs()
    )
}

/// A session the daemon can still stop: not stopped, and not an orphan
/// whose PTY the daemon lost. An errored or spawning session counts.
pub(crate) fn is_active(session: &SessionSnapshot) -> bool {
    session.status != SessionStatus::Stopped && !session.is_orphan
}

/// What the exit dialog counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Counts {
    /// Active sessions.
    pub(crate) active: usize,
    /// Active sessions on per-session worktrees.
    pub(crate) worktrees: usize,
    /// Orphans that have not stopped: the daemon cannot stop them.
    pub(crate) orphans: usize,
}

impl Counts {
    pub(crate) fn of(sessions: &[SessionSnapshot]) -> Self {
        let mut counts = Self::default();
        for session in sessions {
            if is_active(session) {
                counts.active += 1;
                if session.has_per_session_worktree {
                    counts.worktrees += 1;
                }
            } else if session.status != SessionStatus::Stopped {
                counts.orphans += 1;
            }
        }
        counts
    }

    /// "N session is currently active.", or "No active sessions." once none
    /// are.
    pub(crate) fn active_line(self) -> String {
        match self.active {
            0 => "No active sessions.".to_owned(),
            1 => "1 session is currently active.".to_owned(),
            n => format!("{n} sessions are currently active."),
        }
    }

    /// The orphan aside, when any orphan is still running.
    pub(crate) fn orphan_note(self) -> Option<String> {
        let plural = if self.orphans == 1 { "" } else { "s" };
        (self.orphans > 0).then(|| {
            format!(
                "({} orphan{plural} — daemon can't stop these; they'll stay running)",
                self.orphans
            )
        })
    }

    /// The worktree note, when any active session has a worktree of its own.
    pub(crate) fn worktree_note(self) -> Option<String> {
        let has = if self.worktrees == 1 {
            "session has"
        } else {
            "sessions have"
        };
        (self.worktrees > 0).then(|| {
            format!(
                "{} active {has} per-session worktrees. You can stop the sessions and either keep \
                 or remove those worktree directories.",
                self.worktrees
            )
        })
    }
}

/// A button of the exit dialog's footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitButton {
    Cancel,
    /// "Stop sessions, keep worktrees" / "Stop sessions and quit".
    StopKeep,
    /// "Stop sessions, remove worktrees".
    StopRemove,
    /// "Abandon & quit".
    Abandon,
    /// "Keep sessions running in background", the primary action.
    KeepRunning,
    /// The only button once the daemon has not answered in time.
    ForceQuit,
}

impl ExitButton {
    /// The button's debug selector, the Tauri dialog's test id.
    pub(crate) fn selector(self) -> &'static str {
        match self {
            Self::Cancel => "exit-cancel",
            Self::StopKeep => "exit-stop-keep-worktrees",
            Self::StopRemove => "exit-stop-remove-worktrees",
            Self::Abandon => "exit-abandon-quit",
            Self::KeepRunning => "exit-keep-running",
            Self::ForceQuit => "exit-force-quit",
        }
    }

    pub(crate) fn is_danger(self) -> bool {
        matches!(self, Self::StopKeep | Self::StopRemove | Self::ForceQuit)
    }

    /// Whether pressing it sends to the daemon, so it needs a connection.
    fn stops(self) -> bool {
        matches!(self, Self::StopKeep | Self::StopRemove | Self::Abandon)
    }
}

/// The sessions a "remove worktrees" quit covers, as they stand when it
/// starts: every active session, in session-list order.
pub(crate) fn covered(sessions: &[SessionSnapshot]) -> Vec<SessionSnapshot> {
    sessions.iter().filter(|s| is_active(s)).cloned().collect()
}

/// The branch-fate walk: one delete-worktree confirm per worktree session,
/// in session-list order, before the removal goes out.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Walk {
    /// The sessions the removal covers, snapshotted when the walk started.
    covered: Vec<SessionSnapshot>,
    /// Sessions still to ask about, in prompt order.
    pending: Vec<String>,
    /// Answers so far; a session with none is discarded under `Auto`.
    choices: HashMap<String, BranchCleanup>,
    /// How many sessions the walk started with, fixed so "Session n of m"
    /// never renumbers when one vanishes.
    total: usize,
}

impl Walk {
    /// A walk over every active session on a per-session worktree, with the
    /// [`covered`] sessions snapshotted; `None` when there is none to ask
    /// about.
    pub(crate) fn start(sessions: &[SessionSnapshot]) -> Option<Self> {
        let covered = covered(sessions);
        let pending: Vec<String> = covered
            .iter()
            .filter(|s| s.has_per_session_worktree)
            .map(|s| s.id.clone())
            .collect();
        (!pending.is_empty()).then(|| Self {
            covered,
            total: pending.len(),
            pending,
            choices: HashMap::new(),
        })
    }

    /// The sessions the removal covers, as they stood when the walk started.
    pub(crate) fn covered(&self) -> &[SessionSnapshot] {
        &self.covered
    }

    /// The session being asked about.
    pub(crate) fn current(&self) -> Option<&str> {
        self.pending.first().map(String::as_str)
    }

    /// The 1-based position of the current session, and the walk's size.
    pub(crate) fn progress(&self) -> (usize, usize) {
        (self.total - self.pending.len() + 1, self.total)
    }

    /// Records `branch` for `session_id` and moves past it.
    pub(crate) fn record(&mut self, session_id: &str, branch: BranchCleanup) {
        self.pending.retain(|id| id != session_id);
        self.choices.insert(session_id.to_owned(), branch);
    }

    /// Drops the pending sessions `is_live` no longer knows; returns whether
    /// any went. They keep no choice, so they go under `Auto`.
    pub(crate) fn skip_vanished(&mut self, is_live: impl Fn(&str) -> bool) -> bool {
        let before = self.pending.len();
        self.pending.retain(|id| is_live(id));
        self.pending.len() != before
    }

    pub(crate) fn choices(&self) -> &HashMap<String, BranchCleanup> {
        &self.choices
    }
}

/// What a "remove worktrees" quit sends before its shutdown, for every
/// `covered` session in order that `live` still lists: a stop that leaves
/// the worktrees alone while it is still active, then a discard that
/// removes the session's own worktrees with the walk's branch choice
/// (`Auto` without one). A covered session `live` no longer lists is
/// skipped, and a session only `live` lists is left to the shutdown.
/// Tauri's `sendShutdown` shape.
pub(crate) fn removal_messages(
    covered: &[SessionSnapshot],
    choices: &HashMap<String, BranchCleanup>,
    live: &[SessionSnapshot],
) -> Vec<ClientMessage> {
    let mut messages = Vec::new();
    for session in covered {
        let Some(now) = live.iter().find(|s| s.id == session.id) else {
            continue;
        };
        let branch = choices
            .get(&session.id)
            .copied()
            .unwrap_or(BranchCleanup::Auto);
        let cleanup = |remove_worktree: bool, branch: BranchCleanup| -> Vec<CleanupAction> {
            session
                .members
                .iter()
                .map(|member| CleanupAction {
                    repo_id: member.repo_id.clone(),
                    remove_worktree,
                    branch,
                })
                .collect()
        };
        if is_active(now) {
            messages.push(ClientMessage::StopSession {
                session_id: session.id.clone(),
                cleanup: cleanup(false, BranchCleanup::Auto),
            });
        }
        messages.push(ClientMessage::DiscardSession {
            session_id: session.id.clone(),
            cleanup: cleanup(session.has_per_session_worktree, branch),
        });
    }
    messages
}

/// Where the exit dialog stands.
#[derive(Debug, Clone, PartialEq)]
enum Phase {
    /// Waiting for the user's choice.
    Choosing,
    /// The branch-fate walk is on top of the dialog.
    Walking(Walk),
    /// A shutdown was handed to the network thread at `sent_at`; `sent`
    /// once it reported the shutdown went out, `stuck` once the daemon has
    /// not answered in [`STUCK_AFTER`].
    Stopping {
        pressed: ExitButton,
        sent_at: Instant,
        sent: bool,
        stuck: bool,
    },
}

/// The open exit dialog: its phase, the button the keyboard is on, and
/// whether the last shutdown failed to reach the daemon.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExitDialog {
    phase: Phase,
    focused: ExitButton,
    failed: bool,
}

impl ExitDialog {
    /// A fresh dialog, focused on Keep running.
    pub(crate) fn new() -> Self {
        Self {
            phase: Phase::Choosing,
            focused: ExitButton::KeepRunning,
            failed: false,
        }
    }

    /// The footer's buttons, left to right, for `counts`.
    pub(crate) fn buttons(&self, counts: Counts) -> Vec<ExitButton> {
        if self.is_stuck() {
            return vec![ExitButton::ForceQuit];
        }
        let mut buttons = vec![ExitButton::Cancel, ExitButton::StopKeep];
        if counts.worktrees > 0 {
            buttons.push(ExitButton::StopRemove);
        } else if counts.active > 0 {
            buttons.push(ExitButton::Abandon);
        }
        buttons.push(ExitButton::KeepRunning);
        buttons
    }

    pub(crate) fn label(&self, button: ExitButton, counts: Counts) -> String {
        if let Phase::Stopping { pressed, .. } = self.phase
            && pressed == button
        {
            return STOPPING.to_owned();
        }
        let label = match button {
            ExitButton::Cancel => "Cancel",
            ExitButton::StopKeep if counts.worktrees > 0 => "Stop sessions, keep worktrees",
            ExitButton::StopKeep => "Stop sessions and quit",
            ExitButton::StopRemove => "Stop sessions, remove worktrees",
            ExitButton::Abandon => "Abandon & quit",
            ExitButton::KeepRunning => "Keep sessions running in background",
            ExitButton::ForceQuit => "Force quit",
        };
        label.to_owned()
    }

    /// Whether `button` takes a press: nothing does while a shutdown waits
    /// (but Force quit), and the stop buttons need a connection.
    pub(crate) fn enabled(&self, button: ExitButton, connected: bool) -> bool {
        match self.phase {
            Phase::Stopping { stuck, .. } => stuck && button == ExitButton::ForceQuit,
            Phase::Choosing | Phase::Walking(_) => connected || !button.stops(),
        }
    }

    /// The button the keyboard is on: the one placed there while it is
    /// still shown and enabled, else the primary one; `None` while no
    /// button takes a press.
    pub(crate) fn focused(&self, counts: Counts, connected: bool) -> Option<ExitButton> {
        let shown = self.buttons(counts);
        let primary = if self.is_stuck() {
            ExitButton::ForceQuit
        } else {
            ExitButton::KeepRunning
        };
        [self.focused, primary]
            .into_iter()
            .find(|button| shown.contains(button) && self.enabled(*button, connected))
    }

    /// Moves the focus to the next enabled button (or the previous one),
    /// wrapping around the footer.
    pub(crate) fn move_focus(&mut self, forward: bool, counts: Counts, connected: bool) {
        let enabled: Vec<ExitButton> = self
            .buttons(counts)
            .into_iter()
            .filter(|b| self.enabled(*b, connected))
            .collect();
        let Some(current) = self.focused(counts, connected) else {
            return;
        };
        let Some(at) = enabled.iter().position(|b| *b == current) else {
            return;
        };
        let count = enabled.len();
        let next = if forward {
            (at + 1) % count
        } else {
            (at + count - 1) % count
        };
        self.focused = enabled[next];
    }

    /// A shutdown is on the way.
    pub(crate) fn in_flight(&self) -> bool {
        matches!(self.phase, Phase::Stopping { .. })
    }

    pub(crate) fn is_stuck(&self) -> bool {
        matches!(self.phase, Phase::Stopping { stuck: true, .. })
    }

    pub(crate) fn walk(&self) -> Option<&Walk> {
        match &self.phase {
            Phase::Walking(walk) => Some(walk),
            Phase::Choosing | Phase::Stopping { .. } => None,
        }
    }

    pub(crate) fn walk_mut(&mut self) -> Option<&mut Walk> {
        match &mut self.phase {
            Phase::Walking(walk) => Some(walk),
            Phase::Choosing | Phase::Stopping { .. } => None,
        }
    }

    pub(crate) fn start_walk(&mut self, walk: Walk) {
        self.failed = false;
        self.phase = Phase::Walking(walk);
    }

    /// Back to the choice, as when the walk is cancelled.
    pub(crate) fn cancel_walk(&mut self) {
        if matches!(self.phase, Phase::Walking(_)) {
            self.phase = Phase::Choosing;
        }
    }

    /// `pressed` handed a shutdown to the network thread at `now`.
    pub(crate) fn start_stopping(&mut self, pressed: ExitButton, now: Instant) {
        self.failed = false;
        self.phase = Phase::Stopping {
            pressed,
            sent_at: now,
            sent: false,
            stuck: false,
        };
    }

    /// The network thread sent the shutdown: from now on a closed
    /// connection ends the wait.
    pub(crate) fn mark_sent(&mut self) {
        if let Phase::Stopping { sent, .. } = &mut self.phase {
            *sent = true;
        }
    }

    /// Whether the shutdown went out to the daemon.
    pub(crate) fn is_sent(&self) -> bool {
        matches!(self.phase, Phase::Stopping { sent: true, .. })
    }

    /// The shutdown could not go out: back to the choice, with the error
    /// line shown until the next press.
    pub(crate) fn fail_shutdown(&mut self) {
        if matches!(self.phase, Phase::Stopping { .. }) {
            self.phase = Phase::Choosing;
            self.failed = true;
        }
    }

    /// When the wait for the daemon gives up; `None` unless it is waiting.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        match self.phase {
            Phase::Stopping {
                sent_at,
                stuck: false,
                ..
            } => Some(sent_at + STUCK_AFTER),
            Phase::Choosing | Phase::Walking(_) | Phase::Stopping { .. } => None,
        }
    }

    /// The clock at `now`: past the deadline, the dialog offers Force quit.
    /// Returns whether it just did.
    pub(crate) fn tick(&mut self, now: Instant) -> bool {
        match self.deadline() {
            Some(deadline) if now >= deadline => {
                if let Phase::Stopping { stuck, .. } = &mut self.phase {
                    *stuck = true;
                }
                self.focused = ExitButton::ForceQuit;
                true
            }
            _ => false,
        }
    }

    /// The warning above the footer: once the daemon has not answered, or
    /// once a shutdown could not reach it.
    pub(crate) fn warning(&self) -> Option<String> {
        if self.is_stuck() {
            Some(stuck_warning())
        } else if self.failed {
            Some(SHUTDOWN_FAILED.to_owned())
        } else {
            None
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests build fixtures with expect; failure messages aid debugging"
)]
mod tests {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use protocol::{BranchCleanup, CleanupAction, ClientMessage, SessionSnapshot};
    use serde_json::json;

    use super::{Counts, ExitButton, ExitDialog, Walk, covered, removal_messages};

    /// A session `id` with `status`, on `repos`, with its own worktrees when
    /// `worktree`.
    fn session(id: &str, status: &str, repos: &[&str], worktree: bool) -> SessionSnapshot {
        let members: Vec<_> = repos
            .iter()
            .map(|repo| json!({ "repo_id": repo, "repo_name": repo, "branch": "wt/a", "worktree_path": "" }))
            .collect();
        serde_json::from_value(json!({
            "id": id,
            "label": id,
            "kind": "single",
            "members": members,
            "status": status,
            "mode": "interactive",
            "started_at": "2026-01-01T00:00:00Z",
            "exit_code": null,
            "metrics": { "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0, "last_activity_at": null },
            "recent_actions": [],
            "agent": "claude",
            "has_per_session_worktree": worktree,
        }))
        .expect("session fixture")
    }

    fn orphan(id: &str) -> SessionSnapshot {
        let mut s = session(id, "working", &["r1"], true);
        s.is_orphan = true;
        s
    }

    fn counts(active: usize, worktrees: usize, orphans: usize) -> Counts {
        Counts {
            active,
            worktrees,
            orphans,
        }
    }

    fn labelled(dialog: &ExitDialog, counts: Counts) -> Vec<(&'static str, String)> {
        dialog
            .buttons(counts)
            .into_iter()
            .map(|b| (b.selector(), dialog.label(b, counts)))
            .collect()
    }

    fn owned(pairs: &[(&'static str, &str)]) -> Vec<(&'static str, String)> {
        pairs.iter().map(|(s, l)| (*s, (*l).to_owned())).collect()
    }

    #[test]
    fn counts_leave_out_stopped_and_orphans_and_count_errors() {
        let sessions = [
            session("idle", "idle", &["r1"], false),
            session("err", "error", &["r1"], true),
            session("spawning", "spawning", &["r1"], false),
            session("asking", "awaiting_input", &["r1"], true),
            session("done", "stopped", &["r1"], true),
            orphan("lost"),
        ];
        assert_eq!(Counts::of(&sessions), counts(4, 2, 1));
        let mut stopped_orphan = orphan("gone");
        stopped_orphan.status = protocol::SessionStatus::Stopped;
        assert_eq!(Counts::of(&[stopped_orphan]), counts(0, 0, 0));
    }

    #[test]
    fn buttons_without_worktrees_offer_stop_and_abandon() {
        let dialog = ExitDialog::new();
        assert_eq!(
            labelled(&dialog, counts(2, 0, 0)),
            owned(&[
                ("exit-cancel", "Cancel"),
                ("exit-stop-keep-worktrees", "Stop sessions and quit"),
                ("exit-abandon-quit", "Abandon & quit"),
                ("exit-keep-running", "Keep sessions running in background"),
            ])
        );
    }

    #[test]
    fn buttons_with_worktrees_offer_keep_or_remove_and_no_abandon() {
        let dialog = ExitDialog::new();
        assert_eq!(
            labelled(&dialog, counts(2, 1, 0)),
            owned(&[
                ("exit-cancel", "Cancel"),
                ("exit-stop-keep-worktrees", "Stop sessions, keep worktrees"),
                (
                    "exit-stop-remove-worktrees",
                    "Stop sessions, remove worktrees"
                ),
                ("exit-keep-running", "Keep sessions running in background"),
            ])
        );
    }

    #[test]
    fn orphans_only_add_their_note_and_no_active_drops_abandon() {
        let dialog = ExitDialog::new();
        let with_orphan = counts(1, 0, 2);
        assert_eq!(
            dialog.buttons(with_orphan),
            [
                ExitButton::Cancel,
                ExitButton::StopKeep,
                ExitButton::Abandon,
                ExitButton::KeepRunning
            ]
        );
        assert_eq!(
            dialog.buttons(counts(0, 0, 1)),
            [
                ExitButton::Cancel,
                ExitButton::StopKeep,
                ExitButton::KeepRunning
            ],
            "Abandon only while a session is active"
        );
    }

    #[test]
    fn lines_use_the_singular_and_the_plural() {
        assert_eq!(
            counts(1, 1, 1).active_line(),
            "1 session is currently active."
        );
        assert_eq!(
            counts(3, 0, 0).active_line(),
            "3 sessions are currently active."
        );
        assert_eq!(counts(0, 0, 0).active_line(), "No active sessions.");
        assert_eq!(
            counts(1, 0, 1).orphan_note().as_deref(),
            Some("(1 orphan — daemon can't stop these; they'll stay running)")
        );
        assert_eq!(
            counts(1, 0, 2).orphan_note().as_deref(),
            Some("(2 orphans — daemon can't stop these; they'll stay running)")
        );
        assert_eq!(counts(1, 0, 0).orphan_note(), None);
        assert_eq!(
            counts(1, 1, 0).worktree_note().as_deref(),
            Some(
                "1 active session has per-session worktrees. You can stop the sessions and either \
                 keep or remove those worktree directories."
            )
        );
        assert_eq!(
            counts(2, 2, 0).worktree_note().as_deref(),
            Some(
                "2 active sessions have per-session worktrees. You can stop the sessions and \
                 either keep or remove those worktree directories."
            )
        );
        assert_eq!(counts(2, 0, 0).worktree_note(), None);
    }

    #[test]
    fn focus_starts_on_keep_running_and_cycles_the_enabled_buttons() {
        let mut dialog = ExitDialog::new();
        let c = counts(1, 0, 0);
        assert_eq!(dialog.focused(c, true), Some(ExitButton::KeepRunning));
        dialog.move_focus(true, c, true);
        assert_eq!(
            dialog.focused(c, true),
            Some(ExitButton::Cancel),
            "wraps around"
        );
        dialog.move_focus(true, c, true);
        assert_eq!(dialog.focused(c, true), Some(ExitButton::StopKeep));
        dialog.move_focus(false, c, true);
        dialog.move_focus(false, c, true);
        assert_eq!(dialog.focused(c, true), Some(ExitButton::KeepRunning));

        dialog.move_focus(false, c, false);
        assert_eq!(
            dialog.focused(c, false),
            Some(ExitButton::Cancel),
            "disconnected, the stop buttons are skipped"
        );
        assert!(!dialog.enabled(ExitButton::StopKeep, false));
        assert!(!dialog.enabled(ExitButton::Abandon, false));
        assert!(dialog.enabled(ExitButton::KeepRunning, false));
        dialog.move_focus(false, c, false);
        assert_eq!(dialog.focused(c, false), Some(ExitButton::KeepRunning));
    }

    #[test]
    fn no_button_has_the_focus_while_every_button_is_disabled() {
        let t0 = Instant::now();
        let mut dialog = ExitDialog::new();
        let c = counts(1, 0, 0);
        dialog.move_focus(false, c, true);
        dialog.start_stopping(ExitButton::StopKeep, t0);
        assert_eq!(dialog.focused(c, true), None, "the shutdown is on the way");
        dialog.move_focus(true, c, true);
        assert_eq!(
            dialog.focused(c, true),
            None,
            "Tab finds nothing to move to"
        );
        dialog.tick(t0 + Duration::from_secs(5));
        assert_eq!(dialog.focused(c, true), Some(ExitButton::ForceQuit));
    }

    #[test]
    fn in_flight_labels_the_pressed_button_and_disables_every_button() {
        let mut dialog = ExitDialog::new();
        let c = counts(1, 1, 0);
        dialog.start_stopping(ExitButton::StopRemove, Instant::now());
        assert!(dialog.in_flight());
        assert_eq!(dialog.label(ExitButton::StopRemove, c), "Stopping…");
        assert_eq!(
            dialog.label(ExitButton::StopKeep, c),
            "Stop sessions, keep worktrees"
        );
        for button in dialog.buttons(c) {
            assert!(!dialog.enabled(button, true), "{button:?} is disabled");
        }
    }

    #[test]
    fn stuck_timer_offers_force_quit_after_five_seconds() {
        let t0 = Instant::now();
        let mut dialog = ExitDialog::new();
        let c = counts(1, 0, 0);
        assert_eq!(dialog.deadline(), None, "no timer before a shutdown");
        dialog.start_stopping(ExitButton::StopKeep, t0);
        assert_eq!(dialog.deadline(), Some(t0 + Duration::from_secs(5)));
        assert!(!dialog.tick(t0 + Duration::from_millis(4_999)));
        assert_eq!(dialog.warning(), None);
        assert!(dialog.tick(t0 + Duration::from_secs(5)));
        assert_eq!(
            dialog.warning().as_deref(),
            Some("Daemon not responding after 5 seconds.")
        );
        assert_eq!(
            labelled(&dialog, c),
            owned(&[("exit-force-quit", "Force quit")])
        );
        assert!(dialog.enabled(ExitButton::ForceQuit, false));
        assert_eq!(dialog.focused(c, true), Some(ExitButton::ForceQuit));
        assert_eq!(dialog.deadline(), None);
        assert!(!dialog.tick(t0 + Duration::from_secs(9)), "once");
    }

    #[test]
    fn walk_asks_each_worktree_session_in_order_and_skips_vanished_ones() {
        let sessions = [
            session("a", "working", &["r1"], true),
            session("plain", "idle", &["r1"], false),
            session("done", "stopped", &["r1"], true),
            orphan("lost"),
            session("b", "error", &["r1"], true),
            session("c", "idle", &["r1"], true),
        ];
        assert_eq!(Walk::start(&sessions[1..4]), None, "nothing to ask about");
        let mut walk = Walk::start(&sessions).expect("three worktree sessions");
        assert_eq!(walk.current(), Some("a"));
        assert_eq!(walk.progress(), (1, 3));
        walk.record("a", BranchCleanup::Delete);
        assert_eq!(walk.current(), Some("b"));
        assert_eq!(walk.progress(), (2, 3));

        assert!(walk.skip_vanished(|id| id != "b"));
        assert!(!walk.skip_vanished(|_| true));
        assert_eq!(walk.current(), Some("c"));
        assert_eq!(walk.progress(), (3, 3), "the total stays fixed");
        walk.record("c", BranchCleanup::Keep);
        assert_eq!(walk.current(), None, "every answer is in");
        assert_eq!(
            walk.choices(),
            &HashMap::from([
                ("a".to_owned(), BranchCleanup::Delete),
                ("c".to_owned(), BranchCleanup::Keep),
            ]),
            "the vanished session has no choice: it goes under auto"
        );
    }

    fn cleanups(
        repos: &[&str],
        remove_worktree: bool,
        branch: BranchCleanup,
    ) -> Vec<CleanupAction> {
        repos
            .iter()
            .map(|repo| CleanupAction {
                repo_id: (*repo).to_owned(),
                remove_worktree,
                branch,
            })
            .collect()
    }

    #[test]
    fn removal_stops_then_discards_every_active_session_in_order() {
        let sessions = [
            session("ws", "working", &["r1", "r2"], true),
            session("done", "stopped", &["r1"], true),
            orphan("lost"),
            session("single", "error", &["r3"], false),
        ];
        let choices = HashMap::from([("ws".to_owned(), BranchCleanup::Delete)]);
        let sent = removal_messages(&covered(&sessions), &choices, &sessions);
        let expected = [
            ClientMessage::StopSession {
                session_id: "ws".to_owned(),
                cleanup: cleanups(&["r1", "r2"], false, BranchCleanup::Auto),
            },
            ClientMessage::DiscardSession {
                session_id: "ws".to_owned(),
                cleanup: cleanups(&["r1", "r2"], true, BranchCleanup::Delete),
            },
            ClientMessage::StopSession {
                session_id: "single".to_owned(),
                cleanup: cleanups(&["r3"], false, BranchCleanup::Auto),
            },
            ClientMessage::DiscardSession {
                session_id: "single".to_owned(),
                cleanup: cleanups(&["r3"], false, BranchCleanup::Auto),
            },
        ];
        assert_eq!(
            serde_json::to_value(&sent).expect("encode sent"),
            serde_json::to_value(expected).expect("encode expected")
        );
    }

    #[test]
    fn a_session_that_stops_mid_walk_is_still_discarded_with_its_fate() {
        let at_start = [
            session("a", "working", &["r1"], true),
            session("b", "idle", &["r2"], true),
            session("gone", "idle", &["r3"], true),
        ];
        let mut walk = Walk::start(&at_start).expect("three worktree sessions");
        walk.record("a", BranchCleanup::Delete);
        walk.record("b", BranchCleanup::Keep);
        walk.record("gone", BranchCleanup::Delete);
        let now = [
            session("a", "stopped", &["r1"], true),
            session("b", "idle", &["r2"], true),
        ];
        let sent = removal_messages(walk.covered(), walk.choices(), &now);
        let expected = [
            ClientMessage::DiscardSession {
                session_id: "a".to_owned(),
                cleanup: cleanups(&["r1"], true, BranchCleanup::Delete),
            },
            ClientMessage::StopSession {
                session_id: "b".to_owned(),
                cleanup: cleanups(&["r2"], false, BranchCleanup::Auto),
            },
            ClientMessage::DiscardSession {
                session_id: "b".to_owned(),
                cleanup: cleanups(&["r2"], true, BranchCleanup::Keep),
            },
        ];
        assert_eq!(
            serde_json::to_value(&sent).expect("encode sent"),
            serde_json::to_value(expected).expect("encode expected"),
            "a stopped session is only discarded, a vanished one is skipped"
        );
    }

    #[test]
    fn a_session_created_mid_walk_is_not_discarded() {
        let at_start = [session("a", "working", &["r1"], true)];
        let mut walk = Walk::start(&at_start).expect("one worktree session");
        walk.record("a", BranchCleanup::Keep);
        let now = [
            session("new", "working", &["r2"], true),
            session("a", "working", &["r1"], true),
        ];
        let sent = removal_messages(walk.covered(), walk.choices(), &now);
        let expected = [
            ClientMessage::StopSession {
                session_id: "a".to_owned(),
                cleanup: cleanups(&["r1"], false, BranchCleanup::Auto),
            },
            ClientMessage::DiscardSession {
                session_id: "a".to_owned(),
                cleanup: cleanups(&["r1"], true, BranchCleanup::Keep),
            },
        ];
        assert_eq!(
            serde_json::to_value(&sent).expect("encode sent"),
            serde_json::to_value(expected).expect("encode expected"),
            "the shutdown covers the new session"
        );
    }
}
