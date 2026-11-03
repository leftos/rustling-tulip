//! What `wait`, `watch` and `kill` conclude from the daemon's messages, with
//! no socket and no clock.
//!
//! The daemon resends the whole `Sessions` list to a connection that lagged
//! behind its broadcast, so these read snapshots and never count events.

use super::is_mine;
use chrono::{DateTime, Utc};
use protocol::{DaemonMessage, SessionSnapshot, SessionStatus};
use regex::Regex;

/// How a wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The session reached the status waited for.
    Reached(SessionStatus),
    /// The first list of sessions does not hold the session.
    Unknown,
    /// The session was there and is gone.
    Ended,
    /// The session's record stays but its status is over, so what the wait
    /// wants will not come.
    Over,
}

/// One wait on one session, reaching `target` when there is one.
pub struct WaitState {
    session_id: String,
    target: Option<SessionStatus>,
    seen: bool,
}

impl WaitState {
    pub fn new(session_id: &str, target: Option<SessionStatus>) -> Self {
        Self {
            session_id: session_id.to_owned(),
            target,
            seen: false,
        }
    }

    /// True once a snapshot has shown the session.
    pub fn seen(&self) -> bool {
        self.seen
    }

    /// Read one message, answering how the wait ended if it did.
    pub fn step(&mut self, message: &DaemonMessage) -> Option<Outcome> {
        match message {
            DaemonMessage::Sessions { sessions } => match find(sessions, &self.session_id) {
                Some(session) => self.observe(session),
                None if self.seen => Some(Outcome::Ended),
                None => Some(Outcome::Unknown),
            },
            DaemonMessage::SessionUpdated { session, .. } if session.id == self.session_id => {
                self.observe(session)
            }
            DaemonMessage::SessionRemoved { session_id } if *session_id == self.session_id => {
                Some(Outcome::Ended)
            }
            _ => None,
        }
    }

    fn observe(&mut self, session: &SessionSnapshot) -> Option<Outcome> {
        self.seen = true;
        if self.target == Some(session.status) {
            Some(Outcome::Reached(session.status))
        } else if is_over(session.status) && !self.target.is_some_and(is_over) {
            Some(Outcome::Over)
        } else {
            None
        }
    }
}

/// One change `watch` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Line {
    pub status_since: Option<DateTime<Utc>>,
    pub status: SessionStatus,
    pub background: bool,
    pub status_seq: u64,
}

/// Why a watch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchEnd {
    /// The first list of sessions does not hold the session.
    Unknown,
    /// The session was there and is gone.
    Removed,
}

/// One watch of one session, diffing each snapshot against the last.
pub struct WatchState {
    session_id: String,
    last: Option<(SessionStatus, bool)>,
    end: Option<WatchEnd>,
}

impl WatchState {
    pub fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_owned(),
            last: None,
            end: None,
        }
    }

    /// Why the watch ended, once it has.
    pub fn end(&self) -> Option<WatchEnd> {
        self.end
    }

    /// Read one message, answering the line it adds if it changed anything.
    pub fn step(&mut self, message: &DaemonMessage) -> Option<Line> {
        match message {
            DaemonMessage::Sessions { sessions } => {
                if let Some(session) = find(sessions, &self.session_id) {
                    return self.observe(session);
                }
                self.end = Some(if self.last.is_some() {
                    WatchEnd::Removed
                } else {
                    WatchEnd::Unknown
                });
                None
            }
            DaemonMessage::SessionUpdated { session, .. } if session.id == self.session_id => {
                self.observe(session)
            }
            DaemonMessage::SessionRemoved { session_id } if *session_id == self.session_id => {
                self.end = Some(WatchEnd::Removed);
                None
            }
            _ => None,
        }
    }

    fn observe(&mut self, session: &SessionSnapshot) -> Option<Line> {
        let state = (session.status, session.background);
        if self.last == Some(state) {
            return None;
        }
        self.last = Some(state);
        Some(Line {
            status_since: session.status_since,
            status: session.status,
            background: session.background,
            status_seq: session.status_seq,
        })
    }
}

/// Keeps at most one `DumpScreen` in flight without losing a change: one that
/// comes while a dump is out asks for another once that dump is answered.
#[derive(Debug, Default)]
struct DumpPacing {
    in_flight: bool,
    stale: bool,
}

impl DumpPacing {
    /// The screen may have changed: true when a dump goes out now.
    fn changed(&mut self) -> bool {
        if self.in_flight {
            self.stale = true;
            return false;
        }
        self.in_flight = true;
        true
    }

    /// The dump was answered: true when another goes out now.
    fn answered(&mut self) -> bool {
        self.in_flight = false;
        let again = self.stale;
        self.stale = false;
        self.in_flight = again;
        again
    }
}

/// True when `message` says `session_id`'s screen may have changed.
fn screen_changed(message: &DaemonMessage, session_id: &str) -> bool {
    match message {
        DaemonMessage::PtyOutput {
            session_id: from, ..
        }
        | DaemonMessage::Scrollback {
            session_id: from, ..
        } => from == session_id,
        DaemonMessage::SessionUpdated { session, .. } => session.id == session_id,
        _ => false,
    }
}

/// What `wait --text` does next.
#[derive(Debug, PartialEq, Eq)]
pub enum TextStep {
    /// Read on.
    Read,
    /// Start the session's output on this connection and dump its screen.
    Follow,
    /// Dump the screen.
    Dump,
    /// The screen matched on this row.
    Matched(String),
    /// The first list of sessions does not hold the session.
    Unknown,
    /// The session is over and its last screen did not match.
    Ended,
    /// The daemon refused a dump: its code and message.
    Refused(Option<String>, String),
}

/// One `wait --text`, keeping at most one dump in flight. Once the session is
/// over it takes one last dump, so text already on screen still matches, and
/// then ends.
pub struct TextWait<'a> {
    session_id: &'a str,
    regex: &'a Regex,
    state: WaitState,
    pacing: DumpPacing,
    following: bool,
    over: bool,
}

impl<'a> TextWait<'a> {
    pub fn new(session_id: &'a str, regex: &'a Regex) -> Self {
        Self {
            session_id,
            regex,
            state: WaitState::new(session_id, None),
            pacing: DumpPacing::default(),
            following: false,
            over: false,
        }
    }

    /// Read one message.
    pub fn step(&mut self, message: &DaemonMessage) -> TextStep {
        let newly_over = match self.state.step(message) {
            Some(Outcome::Unknown) => return TextStep::Unknown,
            Some(Outcome::Ended) => return TextStep::Ended,
            Some(Outcome::Over) => !std::mem::replace(&mut self.over, true),
            Some(Outcome::Reached(_)) | None => false,
        };
        if self.state.seen() && !self.following {
            self.following = true;
            self.pacing.changed();
            return TextStep::Follow;
        }
        if newly_over {
            return self.ask();
        }
        match message {
            DaemonMessage::ScreenDump {
                request_id, grid, ..
            } if is_mine(request_id.as_deref()) => {
                matched_line(self.regex, grid).map_or_else(|| self.answered(), TextStep::Matched)
            }
            DaemonMessage::Error {
                message,
                request_id,
                code,
            } if is_mine(request_id.as_deref()) => self.refused(code.as_deref(), message),
            _ if !self.over && screen_changed(message, self.session_id) => self.ask(),
            _ => TextStep::Read,
        }
    }

    /// The tick: the screen is fed behind the output, so a quiet one is read
    /// again.
    pub fn tick(&mut self) -> TextStep {
        if self.following && !self.over {
            self.ask()
        } else {
            TextStep::Read
        }
    }

    fn ask(&mut self) -> TextStep {
        if self.pacing.changed() {
            TextStep::Dump
        } else {
            TextStep::Read
        }
    }

    fn answered(&mut self) -> TextStep {
        if self.pacing.answered() {
            TextStep::Dump
        } else if self.over {
            TextStep::Ended
        } else {
            TextStep::Read
        }
    }

    fn refused(&mut self, code: Option<&str>, message: &str) -> TextStep {
        match code {
            Some(protocol::error_codes::NOT_FOUND) => TextStep::Ended,
            Some(protocol::error_codes::NO_SCREEN) => self.answered(),
            _ => TextStep::Refused(code.map(str::to_owned), message.to_owned()),
        }
    }
}

/// The row where `regex` first matches `grid`, joined by `\n` so a match may
/// cross rows.
fn matched_line(regex: &Regex, grid: &[String]) -> Option<String> {
    let joined = grid.join("\n");
    let found = regex.find(&joined)?;
    let row = joined[..found.start()].matches('\n').count();
    grid.get(row).cloned()
}

/// True when `message` shows `session_id` stopped or gone.
pub fn shows_stopped(message: &DaemonMessage, session_id: &str) -> bool {
    match message {
        DaemonMessage::Sessions { sessions } => {
            find(sessions, session_id).is_none_or(|session| is_over(session.status))
        }
        DaemonMessage::SessionUpdated { session, .. } if session.id == session_id => {
            is_over(session.status)
        }
        DaemonMessage::SessionRemoved {
            session_id: removed,
        } => removed == session_id,
        _ => false,
    }
}

/// True for a status a session does not leave on its own.
pub fn is_over(status: SessionStatus) -> bool {
    matches!(status, SessionStatus::Stopped | SessionStatus::Error)
}

fn find<'a>(sessions: &'a [SessionSnapshot], session_id: &str) -> Option<&'a SessionSnapshot> {
    sessions.iter().find(|session| session.id == session_id)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::history::test_support::record;
    use protocol::SessionMode;

    fn session(status: SessionStatus, background: bool, seq: u64) -> SessionSnapshot {
        let mut session = record("s1", SessionMode::PlainShell).snapshot();
        session.status = status;
        session.background = background;
        session.status_seq = seq;
        session
    }

    fn list(sessions: Vec<SessionSnapshot>) -> DaemonMessage {
        DaemonMessage::Sessions { sessions }
    }

    fn updated(session: SessionSnapshot) -> DaemonMessage {
        DaemonMessage::SessionUpdated {
            session,
            request_id: None,
        }
    }

    #[test]
    fn wait_returns_on_a_status_already_reached() {
        let mut wait = WaitState::new("s1", Some(SessionStatus::Idle));
        let first = list(vec![session(SessionStatus::Idle, false, 0)]);
        assert_eq!(
            wait.step(&first),
            Some(Outcome::Reached(SessionStatus::Idle))
        );
    }

    #[test]
    fn wait_returns_on_a_later_status() {
        let mut wait = WaitState::new("s1", Some(SessionStatus::Working));
        assert_eq!(
            wait.step(&list(vec![session(SessionStatus::Idle, false, 0)])),
            None
        );
        assert_eq!(
            wait.step(&updated(session(SessionStatus::AwaitingInput, false, 1))),
            None
        );
        assert_eq!(
            wait.step(&updated(session(SessionStatus::Working, false, 2))),
            Some(Outcome::Reached(SessionStatus::Working))
        );
    }

    #[test]
    fn wait_fails_when_the_session_is_removed() {
        let mut unknown = WaitState::new("s1", Some(SessionStatus::Idle));
        assert_eq!(unknown.step(&list(Vec::new())), Some(Outcome::Unknown));

        let removed = DaemonMessage::SessionRemoved {
            session_id: "s1".to_owned(),
        };
        let mut wait = WaitState::new("s1", Some(SessionStatus::Working));
        wait.step(&list(vec![session(SessionStatus::Idle, false, 0)]));
        assert_eq!(wait.step(&removed), Some(Outcome::Ended));
    }

    #[test]
    fn wait_reads_a_lagged_resend_as_a_snapshot() {
        let mut wait = WaitState::new("s1", Some(SessionStatus::Working));
        wait.step(&list(vec![session(SessionStatus::Idle, false, 0)]));
        assert_eq!(
            wait.step(&list(vec![session(SessionStatus::Working, false, 7)])),
            Some(Outcome::Reached(SessionStatus::Working))
        );

        let mut gone = WaitState::new("s1", Some(SessionStatus::Working));
        gone.step(&list(vec![session(SessionStatus::Idle, false, 0)]));
        assert_eq!(gone.step(&list(Vec::new())), Some(Outcome::Ended));
    }

    #[test]
    fn watch_prints_one_line_per_change() {
        let mut watch = WatchState::new("s1");
        let first = watch
            .step(&list(vec![session(SessionStatus::Idle, false, 0)]))
            .expect("the first line is the current state");
        assert_eq!(first.status, SessionStatus::Idle);
        let second = watch
            .step(&updated(session(SessionStatus::Working, false, 1)))
            .expect("a new status is a new line");
        assert_eq!(
            (second.status, second.status_seq),
            (SessionStatus::Working, 1)
        );
        let net = watch
            .step(&list(vec![session(SessionStatus::Stopped, false, 9)]))
            .expect("a lagged resend is one line for the net change");
        assert_eq!((net.status, net.status_seq), (SessionStatus::Stopped, 9));
        assert_eq!(watch.end(), None);
    }

    #[test]
    fn watch_prints_nothing_for_an_unchanged_snapshot() {
        let mut watch = WatchState::new("s1");
        watch.step(&list(vec![session(SessionStatus::Idle, false, 0)]));
        assert_eq!(
            watch.step(&updated(session(SessionStatus::Idle, false, 0))),
            None
        );
        assert_eq!(
            watch.step(&list(vec![session(SessionStatus::Idle, false, 0)])),
            None
        );
    }

    #[test]
    fn watch_prints_a_background_change() {
        let mut watch = WatchState::new("s1");
        watch.step(&list(vec![session(SessionStatus::Idle, false, 0)]));
        let line = watch
            .step(&updated(session(SessionStatus::Idle, true, 0)))
            .expect("the background flag is a change");
        assert!(line.background);
    }

    #[test]
    fn a_resync_scrollback_asks_for_another_dump() {
        let resync = |session_id: &str| DaemonMessage::Scrollback {
            session_id: session_id.to_owned(),
            data_b64: String::new(),
            truncated: false,
            request_id: None,
            forwarder_restarted: false,
        };
        assert!(screen_changed(&resync("s1"), "s1"));
        assert!(!screen_changed(&resync("s2"), "s1"));
    }

    #[test]
    fn text_matches_across_grid_rows() {
        let grid = ["one".to_owned(), "two".to_owned(), "three".to_owned()];
        let across = Regex::new("two\nthr").expect("a valid regex");
        assert_eq!(matched_line(&across, &grid).as_deref(), Some("two"));
        let absent = Regex::new("four").expect("a valid regex");
        assert_eq!(matched_line(&absent, &grid), None);
    }
}
