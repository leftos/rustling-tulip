//! The footer's state as plain Rust: the status counts, and the flyout's
//! detail rows, two-click stop, and the log and config paths it opens. The
//! GPUI view only renders these and forwards clicks.

use std::path::{Path, PathBuf};

use protocol::{SessionMode, SessionSnapshot, SessionStatus};

use crate::LOG_FILE;
use crate::connection::Connection;
use crate::net::HandshakeInfo;
use crate::status_glyph::{Glyph, Shape, glyph};

/// How many sessions show each of the glyphs the footer counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatusCounts {
    /// Sessions waiting on the user's answer (`AwaitingInput`).
    pub need_you: usize,
    /// Sessions working.
    pub working: usize,
    /// Agent sessions whose finished turn this client has not seen.
    pub waiting: usize,
}

/// Which count a footer span shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountKind {
    NeedYou,
    Working,
    Waiting,
}

impl CountKind {
    /// The name the span's debug selectors carry.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::NeedYou => "need-you",
            Self::Working => "working",
            Self::Waiting => "waiting",
        }
    }

    /// The glyph the span draws before its text: the one a session it
    /// counts shows in its leaf.
    #[must_use]
    pub fn glyph(self) -> Glyph {
        let (status, unseen) = match self {
            Self::NeedYou => (SessionStatus::AwaitingInput, false),
            Self::Working => (SessionStatus::Working, false),
            Self::Waiting => (SessionStatus::Idle, true),
        };
        glyph(status, SessionMode::Interactive, unseen)
    }
}

/// One count the footer shows: its kind and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountSpan {
    pub kind: CountKind,
    pub text: String,
}

impl StatusCounts {
    /// The spans the footer shows, in order need you, working, waiting; a
    /// zero count has none.
    #[must_use]
    pub fn spans(self) -> Vec<CountSpan> {
        [
            (CountKind::NeedYou, self.need_you, "need you"),
            (CountKind::Working, self.working, "working"),
            (CountKind::Waiting, self.waiting, "waiting"),
        ]
        .into_iter()
        .filter(|(_, count, _)| *count > 0)
        .map(|(kind, count, words)| CountSpan {
            kind,
            text: format!("{count} {words}"),
        })
        .collect()
    }
}

/// Counts `sessions` by the glyph each shows, `is_unseen` telling whether a
/// session's finished turn is in the client's unseen set. A plain shell
/// shows the idle dot whatever the set says, so it is never waiting.
#[must_use]
pub fn status_counts(
    sessions: &[SessionSnapshot],
    is_unseen: impl Fn(&str) -> bool,
) -> StatusCounts {
    let mut counts = StatusCounts::default();
    for session in sessions {
        match glyph(session.status, session.mode, is_unseen(&session.id)).shape {
            Shape::Asking => counts.need_you += 1,
            Shape::Working => counts.working += 1,
            Shape::Waiting => counts.waiting += 1,
            Shape::Idle | Shape::Spawning | Shape::Stopped | Shape::Error => {}
        }
    }
    counts
}

/// The flyout's detail rows as (label, value), in display order: State, then
/// Port, PID and Protocol when a handshake is known, Sessions while open, and
/// Reason when the state carries one.
#[must_use]
pub fn flyout_rows(
    conn: &Connection,
    handshake: Option<&HandshakeInfo>,
    session_count: usize,
) -> Vec<(&'static str, String)> {
    let mut rows = vec![("State", conn.footer(session_count).label)];
    if let Some(info) = handshake {
        rows.push(("Port", info.port.to_string()));
        rows.push(("PID", info.pid.to_string()));
        rows.push(("Protocol", format!("v{}", info.protocol_version)));
    }
    if conn.is_open() {
        rows.push(("Sessions", session_count.to_string()));
    }
    if let Some(reason) = conn.reason() {
        rows.push(("Reason", reason.to_owned()));
    }
    rows
}

/// The flyout's two-click stop: the first click arms it, the second confirms.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StopConfirm {
    /// True after the first click, until the confirming click or a reset.
    pub armed: bool,
}

impl StopConfirm {
    /// Register a click on the stop button; true when this click confirms the
    /// stop (the button disarms again).
    pub fn click(&mut self) -> bool {
        let confirmed = self.armed;
        self.armed = !self.armed;
        confirmed
    }

    /// Disarm, as when the flyout closes.
    pub fn reset(&mut self) {
        self.armed = false;
    }

    /// The stop button's label for the current arming.
    #[must_use]
    pub fn label(self) -> &'static str {
        if self.armed {
            "Confirm: stop daemon"
        } else {
            "Stop daemon"
        }
    }
}

/// The files the flyout opens or copies, all under the config dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogPaths {
    /// `<config dir>/logs/daemon.log`.
    pub daemon_log: PathBuf,
    /// `<config dir>/logs/native.log`.
    pub native_log: PathBuf,
    /// The config dir itself.
    pub config_dir: PathBuf,
    /// `<config dir>/daemon.json`, from `daemon_client::handshake_file_in`.
    pub handshake: PathBuf,
}

/// The flyout's paths under `config_dir`.
#[must_use]
pub fn log_paths(config_dir: &Path) -> LogPaths {
    let logs = config_dir.join("logs");
    LogPaths {
        daemon_log: logs.join("daemon.log"),
        native_log: logs.join(LOG_FILE),
        config_dir: config_dir.to_path_buf(),
        handshake: daemon_client::handshake_file_in(config_dir),
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{
        CountKind, CountSpan, LogPaths, StatusCounts, StopConfirm, flyout_rows, log_paths,
        status_counts,
    };
    use crate::connection::Connection;
    use crate::net::HandshakeInfo;
    use protocol::{SessionMode, SessionSnapshot, SessionStatus};
    use serde_json::json;
    use std::collections::HashSet;
    use std::path::Path;

    fn session(id: &str, status: SessionStatus, mode: SessionMode) -> SessionSnapshot {
        let mut s: SessionSnapshot = serde_json::from_value(json!({
            "id": id,
            "label": id,
            "kind": "single",
            "members": [],
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
        .expect("session fixture");
        s.status = status;
        s.mode = mode;
        s
    }

    fn agent(id: &str, status: SessionStatus) -> SessionSnapshot {
        session(id, status, SessionMode::Interactive)
    }

    fn counts(sessions: &[SessionSnapshot], unseen: &[&str]) -> StatusCounts {
        let unseen: HashSet<&str> = unseen.iter().copied().collect();
        status_counts(sessions, |id| unseen.contains(id))
    }

    fn span(kind: CountKind, text: &str) -> CountSpan {
        CountSpan {
            kind,
            text: text.to_owned(),
        }
    }

    #[test]
    fn counts_need_you_working_and_unseen_turns() {
        let sessions = [
            agent("ask-1", SessionStatus::AwaitingInput),
            agent("ask-2", SessionStatus::AwaitingInput),
            agent("work-1", SessionStatus::Working),
            agent("work-2", SessionStatus::Working),
            agent("work-3", SessionStatus::Working),
            agent("done", SessionStatus::Idle),
            agent("seen", SessionStatus::Idle),
            agent("new", SessionStatus::Spawning),
            agent("gone", SessionStatus::Stopped),
            agent("broken", SessionStatus::Error),
        ];
        let counts = counts(&sessions, &["done"]);
        assert_eq!(
            counts,
            StatusCounts {
                need_you: 2,
                working: 3,
                waiting: 1,
            }
        );
        assert_eq!(
            counts.spans(),
            vec![
                span(CountKind::NeedYou, "2 need you"),
                span(CountKind::Working, "3 working"),
                span(CountKind::Waiting, "1 waiting"),
            ]
        );
    }

    #[test]
    fn a_zero_count_has_no_span() {
        let sessions = [
            agent("ask", SessionStatus::AwaitingInput),
            agent("done", SessionStatus::Idle),
        ];
        assert_eq!(
            counts(&sessions, &["done"]).spans(),
            vec![
                span(CountKind::NeedYou, "1 need you"),
                span(CountKind::Waiting, "1 waiting"),
            ],
            "nothing is working, so no working span"
        );
    }

    #[test]
    fn all_zero_counts_have_no_spans() {
        let sessions = [
            agent("idle", SessionStatus::Idle),
            agent("gone", SessionStatus::Stopped),
        ];
        assert_eq!(counts(&sessions, &[]), StatusCounts::default());
        assert!(counts(&sessions, &[]).spans().is_empty());
        assert!(counts(&[], &[]).spans().is_empty(), "no sessions at all");
    }

    #[test]
    fn a_shell_is_never_waiting_even_if_marked_unseen() {
        let sessions = [
            session("sh", SessionStatus::Idle, SessionMode::PlainShell),
            session("hl", SessionStatus::Idle, SessionMode::Headless),
        ];
        assert_eq!(
            counts(&sessions, &["sh", "hl"]),
            StatusCounts {
                waiting: 1,
                ..StatusCounts::default()
            },
            "the headless agent counts, the shell does not"
        );
    }

    #[test]
    fn an_unseen_mark_on_a_working_session_counts_it_working() {
        let sessions = [agent("w", SessionStatus::Working)];
        assert_eq!(
            counts(&sessions, &["w"]),
            StatusCounts {
                working: 1,
                ..StatusCounts::default()
            }
        );
    }

    #[test]
    fn each_span_draws_the_glyph_its_sessions_show() {
        assert_eq!(CountKind::NeedYou.glyph().shape.name(), "asking");
        assert_eq!(CountKind::Working.glyph().shape.name(), "working");
        assert_eq!(CountKind::Waiting.glyph().shape.name(), "waiting");
    }

    const HANDSHAKE: HandshakeInfo = HandshakeInfo {
        port: 51418,
        pid: 4242,
        protocol_version: 18,
    };

    fn open() -> Connection {
        let mut conn = Connection::new();
        conn.on_connecting();
        conn.on_socket_open(HANDSHAKE.port);
        conn.on_welcome(HANDSHAKE.protocol_version);
        conn
    }

    fn row(label: &'static str, value: &str) -> (&'static str, String) {
        (label, value.to_owned())
    }

    #[test]
    fn flyout_rows_when_open() {
        assert_eq!(
            flyout_rows(&open(), Some(&HANDSHAKE), 3),
            vec![
                row("State", "3 sessions"),
                row("Port", "51418"),
                row("PID", "4242"),
                row("Protocol", "v18"),
                row("Sessions", "3"),
            ]
        );
    }

    #[test]
    fn flyout_rows_without_handshake() {
        let mut conn = Connection::new();
        conn.on_connecting();
        assert_eq!(
            flyout_rows(&conn, None, 0),
            vec![row("State", "connecting…")]
        );
    }

    #[test]
    fn flyout_rows_show_reason() {
        let mut conn = open();
        let _ = conn.on_closed("connection reset".to_owned());
        assert_eq!(
            flyout_rows(&conn, Some(&HANDSHAKE), 3),
            vec![
                row("State", "disconnected"),
                row("Port", "51418"),
                row("PID", "4242"),
                row("Protocol", "v18"),
                row("Reason", "connection reset"),
            ]
        );
    }

    #[test]
    fn stop_needs_two_clicks() {
        let mut stop = StopConfirm::default();
        assert_eq!(stop.label(), "Stop daemon");
        assert!(!stop.click(), "the first click only arms");
        assert!(stop.armed);
        assert_eq!(stop.label(), "Confirm: stop daemon");
        assert!(stop.click(), "the second click confirms");
        assert!(!stop.armed, "confirming disarms");
        assert!(!stop.click(), "a third click arms again");
    }

    #[test]
    fn closing_flyout_disarms_stop() {
        let mut stop = StopConfirm::default();
        assert!(!stop.click());
        stop.reset();
        assert!(!stop.armed);
        assert_eq!(stop.label(), "Stop daemon");
        assert!(!stop.click(), "after a reset the next click only arms");
    }

    #[test]
    fn log_paths_under_config_dir() {
        let cfg = Path::new("C:/cfg/rustling-tulip");
        assert_eq!(
            log_paths(cfg),
            LogPaths {
                daemon_log: cfg.join("logs").join("daemon.log"),
                native_log: cfg.join("logs").join("native.log"),
                config_dir: cfg.to_path_buf(),
                handshake: cfg.join("daemon.json"),
            }
        );
    }
}
