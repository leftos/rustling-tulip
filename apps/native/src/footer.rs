//! The footer flyout's state as plain Rust: the detail rows, the two-click
//! stop, and the log and config paths it opens. The GPUI view only renders
//! these and forwards clicks.

use std::path::{Path, PathBuf};

use crate::LOG_FILE;
use crate::connection::Connection;
use crate::net::HandshakeInfo;

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
mod tests {
    use super::{LogPaths, StopConfirm, flyout_rows, log_paths};
    use crate::connection::Connection;
    use crate::net::HandshakeInfo;
    use std::path::Path;

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
