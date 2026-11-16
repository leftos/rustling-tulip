//! The tab-close dialog as plain data: which tabs ask, the live sessions a
//! tab shows, what the dialog says, its buttons in the keyboard's order, the
//! keys it takes and the stops its Stop answer sends.

use protocol::{ClientMessage, SessionStatus, TabEntry};

use crate::tabs::collect_panes;
use crate::target::HostKey;

pub(crate) const TITLE: &str = "Close tab?";

/// A button of the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Control {
    Cancel,
    /// Closes the tab and leaves its sessions running in the background.
    Keep,
    /// Stops the tab's live sessions, then closes the tab.
    Stop,
    /// The ✕ in the title row.
    Dismiss,
}

impl Control {
    pub(crate) fn selector(self) -> &'static str {
        match self {
            Self::Cancel => "tab-close-cancel",
            Self::Keep => "tab-close-keep",
            Self::Stop => "tab-close-stop",
            Self::Dismiss => "tab-close-dismiss",
        }
    }

    /// Whether the button shows in the danger colour.
    pub(crate) fn danger(self) -> bool {
        self == Self::Stop
    }
}

/// The buttons in the keyboard's order: the footer's, then ✕.
const CONTROLS: [Control; 4] = [
    Control::Cancel,
    Control::Keep,
    Control::Stop,
    Control::Dismiss,
];

/// A session a tab shows that has not ended, on a host whose connection is
/// open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveSession {
    pub(crate) host: HostKey,
    pub(crate) session_id: String,
}

/// What a key does to the open dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyResult {
    /// The focus moved to another button.
    Moved,
    /// The key presses this button.
    Press(Control),
    /// The dialog takes the key and does nothing with it.
    Ignored,
}

pub(crate) const OTHERS_TITLE: &str = "Close other tabs?";

/// What the dialog closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// The tab itself.
    Tab,
    /// Every tab but the tab, as the tab menu's Close other tabs does.
    OtherTabs,
}

/// The distinct live sessions `tab` shows, in pane order: a pane's session
/// counts when `status_of` knows it on the pane's host and it is neither
/// Stopped nor in Error, and that host's connection is open by
/// `host_open`. A session two panes show counts once. A tab with none, or
/// with no grid, does not ask before it closes.
#[cfg(test)]
pub(crate) fn live_sessions(
    tab: &TabEntry,
    status_of: &dyn Fn(&HostKey, &str) -> Option<SessionStatus>,
    host_open: &dyn Fn(&HostKey) -> bool,
) -> Vec<LiveSession> {
    live_sessions_in(&[tab], status_of, host_open)
}

/// The distinct live sessions across `tabs`, in tab then pane order, by
/// the rule of [`live_sessions`]: a session two of them show counts once.
pub(crate) fn live_sessions_in(
    tabs: &[&TabEntry],
    status_of: &dyn Fn(&HostKey, &str) -> Option<SessionStatus>,
    host_open: &dyn Fn(&HostKey) -> bool,
) -> Vec<LiveSession> {
    let panes = tabs
        .iter()
        .filter_map(|tab| tab.grid())
        .flat_map(collect_panes);
    let mut live: Vec<LiveSession> = Vec::new();
    for pane in panes {
        let Some(id) = pane.session else {
            continue;
        };
        let counted = live
            .iter()
            .any(|s| s.host == pane.host && s.session_id == id);
        if counted || !host_open(&pane.host) {
            continue;
        }
        if status_of(&pane.host, id).is_some_and(is_live) {
            live.push(LiveSession {
                host: pane.host,
                session_id: id.to_owned(),
            });
        }
    }
    live
}

fn is_live(status: SessionStatus) -> bool {
    !matches!(status, SessionStatus::Stopped | SessionStatus::Error)
}

/// The open tab-close dialog: what it closes, the tab, the live sessions
/// and their hosts as they last stood, the first one's label, and the
/// focused button.
#[derive(Debug, Clone)]
pub(crate) struct TabClose {
    scope: Scope,
    tab_id: String,
    sessions: Vec<LiveSession>,
    label: String,
    focus: Control,
}

impl TabClose {
    /// The dialog closing `tab_id` (or, by `scope`, every other tab), which
    /// show `sessions`, the first of them under `label`; Keep running has
    /// the focus. `None` when `sessions` is empty, since such a close does
    /// not ask.
    pub(crate) fn new(
        scope: Scope,
        tab_id: &str,
        sessions: Vec<LiveSession>,
        label: String,
    ) -> Option<Self> {
        if sessions.is_empty() {
            return None;
        }
        Some(Self {
            scope,
            tab_id: tab_id.to_owned(),
            sessions,
            label,
            focus: Control::Keep,
        })
    }

    pub(crate) fn scope(&self) -> Scope {
        self.scope
    }

    pub(crate) fn title(&self) -> &'static str {
        match self.scope {
            Scope::Tab => TITLE,
            Scope::OtherTabs => OTHERS_TITLE,
        }
    }

    /// Takes `sessions`, the tab's live sessions as they stand now, the
    /// first of them under `label`, keeping the focus. Returns `false`, and
    /// keeps nothing, when none is left: the dialog then has nothing to ask.
    pub(crate) fn refresh(&mut self, sessions: Vec<LiveSession>, label: String) -> bool {
        if sessions.is_empty() {
            return false;
        }
        self.sessions = sessions;
        self.label = label;
        true
    }

    pub(crate) fn tab_id(&self) -> &str {
        &self.tab_id
    }

    /// How many live sessions the tab shows.
    pub(crate) fn count(&self) -> usize {
        self.sessions.len()
    }

    /// The line under the title.
    pub(crate) fn body(&self) -> String {
        let shows = match self.scope {
            Scope::Tab => "This tab shows",
            Scope::OtherTabs => "The other tabs show",
        };
        match self.count() {
            1 => format!("{shows} 1 session: {}.", self.label),
            n => format!("{shows} {n} sessions."),
        }
    }

    /// The buttons in the keyboard's order: the footer's, then ✕.
    pub(crate) fn controls() -> [Control; 4] {
        CONTROLS
    }

    pub(crate) fn label(&self, control: Control) -> String {
        let label = match control {
            Control::Cancel => "Cancel",
            Control::Keep => "Keep running",
            Control::Stop if self.count() == 1 => "Stop session",
            Control::Stop => "Stop sessions",
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
        let len = CONTROLS.len();
        let at = CONTROLS.iter().position(|c| *c == self.focus).unwrap_or(0);
        let next = if forward {
            (at + 1) % len
        } else {
            (at + len - 1) % len
        };
        self.focus = CONTROLS[next];
    }

    /// `key` (a keystroke's key name), with Shift held when `shift`: Tab
    /// and Shift+Tab move the focus, Enter and Space press the focused
    /// button, Esc presses Cancel, and every other key is taken and ignored.
    pub(crate) fn key(&mut self, key: &str, shift: bool) -> KeyResult {
        match key {
            "tab" => {
                self.move_focus(!shift);
                KeyResult::Moved
            }
            "enter" | "space" => KeyResult::Press(self.focus),
            "escape" => KeyResult::Press(Control::Cancel),
            _ => KeyResult::Ignored,
        }
    }

    /// The Stop answer's messages, each with the host it goes to: one
    /// `StopSession` per live session, to that session's host.
    pub(crate) fn stop_messages(&self) -> Vec<(HostKey, ClientMessage)> {
        self.sessions
            .iter()
            .map(|session| {
                (
                    session.host.clone(),
                    ClientMessage::StopSession {
                        session_id: session.session_id.clone(),
                        cleanup: Vec::new(),
                    },
                )
            })
            .collect()
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use protocol::{GridNode, SessionStatus, SplitDirection};

    use super::*;
    use crate::tabs::tests::{diff_tab, follow_pane, pane, pane_on, split, tab, wire};

    const REMOTE: &str = "box";

    fn remote() -> HostKey {
        HostKey::Remote(REMOTE.to_owned())
    }

    /// Every session is Working but `s-stopped` (Stopped) and `s-error`
    /// (Error); `s-gone` is unknown.
    fn status_of(_: &HostKey, id: &str) -> Option<SessionStatus> {
        match id {
            "s-stopped" => Some(SessionStatus::Stopped),
            "s-error" => Some(SessionStatus::Error),
            "s-gone" => None,
            _ => Some(SessionStatus::Working),
        }
    }

    fn all_open(_: &HostKey) -> bool {
        true
    }

    fn ids(live: &[LiveSession]) -> Vec<&str> {
        live.iter().map(|s| s.session_id.as_str()).collect()
    }

    fn horizontal(first: GridNode, second: GridNode) -> GridNode {
        split(SplitDirection::Horizontal, 0.5, first, second)
    }

    fn dialog(count: usize) -> TabClose {
        let sessions = (0..count)
            .map(|i| LiveSession {
                host: HostKey::Local,
                session_id: format!("s{i}"),
            })
            .collect();
        TabClose::new(Scope::Tab, "t1", sessions, "work".to_owned())
            .expect("a tab with sessions asks")
    }

    #[test]
    fn tab_close_lists_live_sessions_once_in_pane_order() {
        let grid = horizontal(
            horizontal(pane("p1", Some("a")), pane("p2", Some("b"))),
            horizontal(pane("p3", Some("a")), pane("p4", Some("s-stopped"))),
        );
        let live = live_sessions(&tab("t1", &grid), &status_of, &all_open);
        assert_eq!(ids(&live), ["a", "b"], "deduped, the stopped one left out");
        assert!(live.iter().all(|s| s.host == HostKey::Local));
    }

    #[test]
    fn tab_close_leaves_out_ended_unknown_and_follow_panes() {
        let grid = horizontal(
            horizontal(pane("p1", Some("s-error")), pane("p2", Some("s-gone"))),
            horizontal(pane("p3", None), follow_pane("p4", "run1")),
        );
        let none: Vec<LiveSession> = Vec::new();
        assert_eq!(
            live_sessions(&tab("t1", &grid), &status_of, &all_open),
            none
        );
        assert_eq!(
            live_sessions(&diff_tab("d1"), &status_of, &all_open),
            none,
            "a tab with no grid shows no session"
        );
    }

    #[test]
    fn tab_close_counts_a_session_on_its_own_host_only_while_open() {
        let grid = horizontal(pane("p1", Some("a")), pane_on("p2", Some("a"), REMOTE));
        let live = live_sessions(&tab("t1", &grid), &status_of, &all_open);
        assert_eq!(
            live,
            [
                LiveSession {
                    host: HostKey::Local,
                    session_id: "a".to_owned()
                },
                LiveSession {
                    host: remote(),
                    session_id: "a".to_owned()
                },
            ],
            "the same id on two hosts is two sessions"
        );
        let local_only = |host: &HostKey| *host == HostKey::Local;
        let live = live_sessions(&tab("t1", &grid), &status_of, &local_only);
        assert_eq!(
            live.len(),
            1,
            "a disconnected host's session is not counted"
        );
        assert_eq!(live[0].host, HostKey::Local);
        let remote_only = tab("t2", &pane_on("p1", Some("a"), REMOTE));
        assert_eq!(
            live_sessions(&remote_only, &status_of, &local_only),
            Vec::<LiveSession>::new(),
            "so a tab whose only live session is on it does not ask"
        );
        assert!(TabClose::new(Scope::Tab, "t2", Vec::new(), String::new()).is_none());
    }

    #[test]
    fn tab_close_texts_by_count() {
        let one = dialog(1);
        assert_eq!(one.body(), "This tab shows 1 session: work.");
        let labels: Vec<String> = TabClose::controls().iter().map(|c| one.label(*c)).collect();
        assert_eq!(labels, ["Cancel", "Keep running", "Stop session", "✕"]);
        let two = dialog(2);
        assert_eq!(two.body(), "This tab shows 2 sessions.");
        assert_eq!(two.label(Control::Stop), "Stop sessions");
        let selectors: Vec<&str> = TabClose::controls().iter().map(|c| c.selector()).collect();
        assert_eq!(
            selectors,
            [
                "tab-close-cancel",
                "tab-close-keep",
                "tab-close-stop",
                "tab-close-dismiss"
            ]
        );
        assert!(Control::Stop.danger());
        assert!(!Control::Keep.danger() && !Control::Cancel.danger());
    }

    #[test]
    fn tab_close_focuses_keep_and_tab_wraps_both_ways() {
        let mut d = dialog(1);
        assert_eq!(d.focused(), Control::Keep, "Keep running is the default");
        assert_eq!(d.key("tab", false), KeyResult::Moved);
        assert_eq!(d.focused(), Control::Stop);
        d.key("tab", false);
        assert_eq!(d.focused(), Control::Dismiss);
        d.key("tab", false);
        assert_eq!(d.focused(), Control::Cancel, "wraps to the start");
        assert_eq!(d.key("tab", true), KeyResult::Moved);
        assert_eq!(d.focused(), Control::Dismiss, "and back");
    }

    #[test]
    fn tab_close_refresh_follows_the_live_sessions_now() {
        let mut d = dialog(2);
        d.key("tab", false);
        let left = vec![LiveSession {
            host: HostKey::Local,
            session_id: "s0".to_owned(),
        }];
        assert!(d.refresh(left, "only".to_owned()), "one is left");
        assert_eq!(d.body(), "This tab shows 1 session: only.");
        assert_eq!(d.label(Control::Stop), "Stop session");
        assert_eq!(d.focused(), Control::Stop, "the focus stays");
        let kinds: Vec<String> = d
            .stop_messages()
            .iter()
            .map(|(_, msg)| {
                wire(msg)["session_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        assert_eq!(kinds, ["s0"], "Stop stops only what is live now");

        // A host gone offline drops its sessions from the recompute.
        let grid = horizontal(pane("p1", Some("a")), pane_on("p2", Some("b"), REMOTE));
        let local_only = |host: &HostKey| *host == HostKey::Local;
        let now = live_sessions(&tab("t1", &grid), &status_of, &local_only);
        assert!(d.refresh(now, "a".to_owned()));
        assert_eq!(d.count(), 1);
        assert!(!d.refresh(Vec::new(), String::new()), "none left: close");
        assert_eq!(d.count(), 1, "a refused refresh keeps nothing new");
    }

    #[test]
    fn tab_close_other_tabs_count_each_live_session_once() {
        let x = tab(
            "t1",
            &horizontal(pane("p1", Some("x")), pane("p2", Some("a"))),
        );
        let second = tab(
            "t2",
            &horizontal(pane("p3", Some("a")), pane("p4", Some("b"))),
        );
        let third = tab(
            "t3",
            &horizontal(pane("p5", Some("a")), pane("p6", Some("s-stopped"))),
        );
        let live = live_sessions_in(&[&second, &third], &status_of, &all_open);
        assert_eq!(
            ids(&live),
            ["a", "b"],
            "a once across two tabs; the stopped one left out"
        );
        assert_eq!(ids(&live_sessions(&x, &status_of, &all_open)), ["x", "a"]);

        let d = TabClose::new(Scope::OtherTabs, "t1", live, "a".to_owned()).expect("asks");
        assert_eq!(d.scope(), Scope::OtherTabs);
        assert_eq!(d.title(), "Close other tabs?");
        assert_eq!(d.body(), "The other tabs show 2 sessions.");
        assert_eq!(d.label(Control::Stop), "Stop sessions");
        assert_eq!(dialog(1).title(), "Close tab?");

        let one = vec![LiveSession {
            host: HostKey::Local,
            session_id: "b".to_owned(),
        }];
        let d = TabClose::new(Scope::OtherTabs, "t1", one, "work".to_owned()).expect("asks");
        assert_eq!(d.body(), "The other tabs show 1 session: work.");
        assert_eq!(d.label(Control::Stop), "Stop session");
        assert_eq!(
            live_sessions_in(&[], &status_of, &all_open),
            Vec::<LiveSession>::new(),
            "no other tab: nothing to ask"
        );
    }

    #[test]
    fn tab_close_keys_press_and_cancel() {
        let mut d = dialog(1);
        assert_eq!(d.key("enter", false), KeyResult::Press(Control::Keep));
        assert_eq!(d.key("space", false), KeyResult::Press(Control::Keep));
        assert_eq!(d.key("escape", false), KeyResult::Press(Control::Cancel));
        assert_eq!(d.key("a", false), KeyResult::Ignored);
        assert_eq!(d.focused(), Control::Keep, "no key moved the focus");
    }

    #[test]
    fn tab_close_stops_each_session_on_its_host() {
        let sessions = vec![
            LiveSession {
                host: HostKey::Local,
                session_id: "a".to_owned(),
            },
            LiveSession {
                host: remote(),
                session_id: "b".to_owned(),
            },
        ];
        let d = TabClose::new(Scope::Tab, "t1", sessions, "a".to_owned()).expect("asks");
        assert_eq!(d.tab_id(), "t1");
        let sent: Vec<(HostKey, serde_json::Value)> = d
            .stop_messages()
            .iter()
            .map(|(host, msg)| (host.clone(), wire(msg)))
            .collect();
        assert_eq!(
            sent,
            [
                (
                    HostKey::Local,
                    serde_json::json!({"type": "stop_session", "session_id": "a", "cleanup": []})
                ),
                (
                    remote(),
                    serde_json::json!({"type": "stop_session", "session_id": "b", "cleanup": []})
                ),
            ]
        );
    }
}
