//! Connection lifecycle rules for the native client: the connection state, the
//! reconnect backoff, what Restart does, the standby watchdog, and the footer
//! and overlay the view shows for each state.
//!
//! Plain Rust with no GPUI or tokio in its API: `net.rs` feeds it events and
//! does what it says, and the view only reads [`Connection::footer`] and
//! [`Connection::overlay`].

use std::time::{Duration, SystemTime};

use crate::target::Target;

/// How often the standby watchdog ticks.
pub const TICK: Duration = Duration::from_secs(30);
/// A gap between watchdog ticks at least this long means the machine slept.
pub const RESUME_GAP: Duration = Duration::from_secs(90);
/// How long the post-resume probe waits for any inbound message.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

const BACKOFF_BASE: Duration = Duration::from_millis(500);
const BACKOFF_CAP: Duration = Duration::from_secs(10);

/// Where the connection to the daemon stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Boot, through the first `ensure_running`.
    Init,
    /// A WebSocket connect is under way, or the socket is open and the
    /// daemon has not yet answered `Hello`.
    Connecting,
    /// The daemon accepted `Hello`, negotiated this protocol version and
    /// listed these features in its `Welcome`.
    Open {
        negotiated: u32,
        features: Vec<String>,
    },
    /// The socket closed or could not be opened.
    Closed { reason: String },
    /// The daemon rejected the auth token. Terminal until a Restart.
    AuthFailed { reason: String },
    /// The tailnet host has not approved this device: its daemon answered
    /// `Hello` with `Welcome`, then `DevicePairFailed { reason }`, and keeps
    /// the socket open as a pairing connection. `had_token` says whether the
    /// `Hello` carried a stored device token (one the host no longer
    /// accepts) or none. Terminal until a Restart.
    NotApproved { reason: String, had_token: bool },
    /// `ensure_running` or the handshake file failed.
    Error { reason: String },
}

/// What a Restart request turns into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartAction {
    /// Connect again from scratch now (the machine is already reset).
    FreshConnect,
    /// Send `Shutdown { drain: false }` on the live socket and let the normal
    /// reconnect respawn the daemon.
    SendShutdown,
}

/// The colour class of the footer's status dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotKind {
    /// Connected.
    Ok,
    /// Starting or connecting.
    Pending,
    /// Disconnected, reconnect pending.
    Idle,
    /// Stopped by the user.
    Stopped,
    /// Auth failed or an error.
    Err,
}

/// What the footer shows for the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footer {
    /// The status dot's colour class.
    pub dot: DotKind,
    /// The short state text ("3 sessions", "disconnected", …).
    pub label: String,
    /// The local daemon's port, only while the connection is open; never
    /// set on a remote host.
    pub port: Option<u16>,
    /// A remote host's short name; `None` on the local daemon.
    pub host: Option<String>,
    /// The label, followed by " — <reason>" when the state carries one.
    pub tooltip: String,
}

impl Footer {
    /// The footer pill's text: `daemon · <label>`, with ` · :<port>` while
    /// open, on the local daemon; `<host> · <label>` on a remote host.
    #[must_use]
    pub fn pill(&self) -> String {
        match (&self.host, self.port) {
            (Some(host), _) => format!("{host} · {}", self.label),
            (None, Some(port)) => format!("daemon · {} · :{port}", self.label),
            (None, None) => format!("daemon · {}", self.label),
        }
    }
}

/// Reconnect delay before attempt `attempt` (0-based): 0.5 s doubling per
/// attempt, capped at 10 s.
#[must_use]
pub fn backoff_delay(attempt: u32) -> Duration {
    let factor = 1_u32.checked_shl(attempt).unwrap_or(u32::MAX);
    BACKOFF_BASE.saturating_mul(factor).min(BACKOFF_CAP)
}

/// The connection state machine: the single owner of the reconnect, restart
/// and stop rules.
#[derive(Debug, Clone)]
pub struct Connection {
    state: State,
    stop_requested: bool,
    has_ever_connected: bool,
    attempt: u32,
    socket_live: bool,
    port: Option<u16>,
    target: Target,
}

impl Default for Connection {
    fn default() -> Self {
        Self::new()
    }
}

impl Connection {
    /// A machine at boot on the local daemon: [`State::Init`], never
    /// connected, no stop request.
    #[must_use]
    pub fn new() -> Self {
        Self::for_target(Target::Local)
    }

    /// A machine at boot on `target`.
    #[must_use]
    pub fn for_target(target: Target) -> Self {
        Self {
            state: State::Init,
            stop_requested: false,
            has_ever_connected: false,
            attempt: 0,
            socket_live: false,
            port: None,
            target,
        }
    }

    #[must_use]
    pub fn state(&self) -> &State {
        &self.state
    }

    /// The daemon this machine connects to.
    #[must_use]
    pub fn target(&self) -> &Target {
        &self.target
    }

    /// Whether Stop is offered: only the local daemon's process is ever
    /// stopped from here.
    #[must_use]
    pub fn can_stop(&self) -> bool {
        self.target.is_local()
    }

    #[must_use]
    pub fn is_open(&self) -> bool {
        matches!(self.state, State::Open { .. })
    }

    /// Whether the connected daemon listed `feature` in its `Welcome`; false
    /// whenever the connection is not open.
    #[must_use]
    pub fn supports(&self, feature: &str) -> bool {
        match &self.state {
            State::Open { features, .. } => features.iter().any(|listed| listed == feature),
            _ => false,
        }
    }

    /// A WebSocket connect is starting.
    pub fn on_connecting(&mut self) {
        self.state = State::Connecting;
    }

    /// The WebSocket to the daemon on `port` opened; `Hello` goes next.
    pub fn on_socket_open(&mut self, port: u16) {
        self.socket_live = true;
        self.port = Some(port);
    }

    /// The daemon answered `Hello` with `Welcome`, listing `features`.
    pub fn on_welcome(&mut self, negotiated: u32, features: Vec<String>) {
        self.state = State::Open {
            negotiated,
            features,
        };
        self.attempt = 0;
        self.has_ever_connected = true;
    }

    /// The daemon rejected the auth token.
    pub fn on_auth_failed(&mut self, reason: String) {
        self.state = State::AuthFailed { reason };
    }

    /// The tailnet host has not approved this device (see
    /// [`State::NotApproved`]). Never counts as a connect.
    pub fn on_not_approved(&mut self, reason: String, had_token: bool) {
        self.state = State::NotApproved { reason, had_token };
    }

    /// Whether the daemon refused this client (auth failed, or not
    /// approved): nothing reconnects until a Restart.
    fn refused(&self) -> bool {
        matches!(
            self.state,
            State::AuthFailed { .. } | State::NotApproved { .. }
        )
    }

    /// The socket closed (or never opened). Returns the delay before the next
    /// attempt, or `None` when no reconnect is due (stopped, auth failed, not
    /// approved).
    pub fn on_closed(&mut self, reason: String) -> Option<Duration> {
        self.socket_live = false;
        if self.refused() {
            return None;
        }
        self.state = State::Closed { reason };
        self.schedule_reconnect()
    }

    /// `ensure_running` or the handshake file failed. Returns the delay
    /// before the next attempt, as [`Connection::on_closed`] does.
    pub fn on_error(&mut self, reason: String) -> Option<Duration> {
        self.socket_live = false;
        if self.refused() {
            return None;
        }
        self.state = State::Error { reason };
        self.schedule_reconnect()
    }

    fn schedule_reconnect(&mut self) -> Option<Duration> {
        if self.stop_requested {
            return None;
        }
        let delay = backoff_delay(self.attempt);
        self.attempt = self.attempt.saturating_add(1);
        Some(delay)
    }

    /// Decide what Restart does. A fresh connect resets the machine to
    /// [`State::Connecting`] with the stop request cleared and the backoff
    /// reset; a shutdown leaves the state alone for the close to drive. On a
    /// remote host Restart is always a fresh connect (Reconnect now): the
    /// host's daemon is never shut down from here.
    pub fn restart(&mut self) -> RestartAction {
        let dead = self.refused() || matches!(self.state, State::Error { .. });
        if dead || self.stop_requested || !self.socket_live || !self.target.is_local() {
            self.stop_requested = false;
            self.state = State::Connecting;
            self.attempt = 0;
            RestartAction::FreshConnect
        } else {
            RestartAction::SendShutdown
        }
    }

    /// The user stopped the daemon: no reconnect until a Restart. Ignored on
    /// a remote host, which [`Connection::can_stop`] never offers.
    pub fn stop(&mut self) {
        if self.can_stop() {
            self.stop_requested = true;
        }
    }

    /// Why the connection is closed, failed or rejected; `None` otherwise.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match &self.state {
            State::Closed { reason }
            | State::AuthFailed { reason }
            | State::NotApproved { reason, .. }
            | State::Error { reason } => Some(reason),
            State::Init | State::Connecting | State::Open { .. } => None,
        }
    }

    fn label_and_dot(&self, session_count: usize) -> (String, DotKind) {
        let (label, dot) = match &self.state {
            State::Open { .. } if session_count == 1 => ("1 session", DotKind::Ok),
            State::Open { .. } => return (format!("{session_count} sessions"), DotKind::Ok),
            _ if self.stop_requested => ("stopped", DotKind::Stopped),
            State::Init => ("starting…", DotKind::Pending),
            State::Connecting => ("connecting…", DotKind::Pending),
            State::Closed { .. } => ("disconnected", DotKind::Idle),
            State::AuthFailed { .. } => ("auth failed", DotKind::Err),
            State::NotApproved { .. } => ("not paired", DotKind::Err),
            State::Error { .. } => ("error", DotKind::Err),
        };
        (label.to_owned(), dot)
    }

    /// The footer's dot, label, port and tooltip for `session_count` sessions.
    #[must_use]
    pub fn footer(&self, session_count: usize) -> Footer {
        let (label, dot) = self.label_and_dot(session_count);
        let tooltip = match self.reason() {
            Some(reason) => format!("{label} — {reason}"),
            None => label.clone(),
        };
        Footer {
            dot,
            port: self
                .port
                .filter(|_| self.is_open() && self.target.is_local()),
            host: self.target.short_name().map(str::to_owned),
            label,
            tooltip,
        }
    }

    /// What the Restart action is called: Restart on the local daemon,
    /// Reconnect on a remote host (see [`Connection::restart`]).
    #[must_use]
    pub fn restart_label(&self) -> &'static str {
        if self.target.is_local() {
            "Restart daemon"
        } else {
            "Reconnect now"
        }
    }

    /// The full-window overlay text shown until the first connect, naming a
    /// remote host by its short name, or `None` when [`Connection::overlay`]
    /// shows none.
    #[must_use]
    pub fn overlay_text(&self) -> Option<String> {
        let local = self.overlay()?;
        let Some(host) = self.target.short_name() else {
            return Some(local.to_owned());
        };
        Some(match self.state {
            State::Closed { .. } => format!("{host} dropped — reconnecting…"),
            State::NotApproved { .. } => format!("Pair with {host}"),
            _ => format!("Connecting to {host}…"),
        })
    }

    /// The local daemon's full-window overlay text shown until the first
    /// connect, or `None` once connected or when the footer carries a
    /// failure instead. [`Connection::overlay_text`] is the text for any
    /// target.
    #[must_use]
    pub fn overlay(&self) -> Option<&'static str> {
        if self.has_ever_connected {
            return None;
        }
        match self.state {
            State::AuthFailed { .. } | State::Error { .. } => None,
            State::NotApproved { .. } => Some("Not paired"),
            _ if self.stop_requested => Some("Waiting for daemon…"),
            State::Init => Some("Starting daemon…"),
            State::Connecting => Some("Connecting to daemon…"),
            State::Closed { .. } => Some("Daemon dropped — reconnecting…"),
            State::Open { .. } => Some("Waiting for daemon…"),
        }
    }
}

/// Detects a system resume: wall-clock time jumps across a suspend, so a gap
/// of [`RESUME_GAP`] or more between two [`TICK`]s means the machine slept.
#[derive(Debug, Clone, Copy)]
pub struct Watchdog {
    last: SystemTime,
}

impl Watchdog {
    /// A watchdog whose last tick was at `now`.
    #[must_use]
    pub fn new(now: SystemTime) -> Self {
        Self { last: now }
    }

    /// Record a tick at `now`; true when the gap since the last tick means the
    /// machine resumed from sleep. A clock that went backwards is no resume.
    pub fn tick(&mut self, now: SystemTime) -> bool {
        let resumed = now
            .duration_since(self.last)
            .is_ok_and(|gap| gap >= RESUME_GAP);
        self.last = now;
        resumed
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Connection, DotKind, RESUME_GAP, RestartAction, State, TICK, Watchdog, backoff_delay,
    };
    use crate::target::Target;
    use protocol::FEATURE_AGENT_COMMANDS;
    use std::time::{Duration, SystemTime};

    const HALF_SECOND: Duration = Duration::from_millis(500);

    fn open(negotiated: u32) -> Connection {
        let mut conn = Connection::new();
        conn.on_connecting();
        conn.on_socket_open(51418);
        conn.on_welcome(negotiated, Vec::new());
        conn
    }

    #[test]
    fn backoff_doubles_from_half_second_and_caps_at_ten() {
        let delays: Vec<u64> = (0..8)
            .map(|attempt| u64::try_from(backoff_delay(attempt).as_millis()).unwrap_or(u64::MAX))
            .collect();
        assert_eq!(
            delays,
            [500, 1_000, 2_000, 4_000, 8_000, 10_000, 10_000, 10_000]
        );
        assert_eq!(backoff_delay(u32::MAX), Duration::from_secs(10));
    }

    #[test]
    fn open_resets_backoff() {
        let mut conn = Connection::new();
        for _ in 0..3 {
            conn.on_closed("refused".to_owned());
        }
        conn.on_socket_open(51418);
        conn.on_welcome(20, Vec::new());
        assert_eq!(conn.on_closed("dropped".to_owned()), Some(HALF_SECOND));
    }

    #[test]
    fn close_schedules_reconnect() {
        let mut conn = open(20);
        assert_eq!(conn.on_closed("dropped".to_owned()), Some(HALF_SECOND));
        assert_eq!(
            conn.state(),
            &State::Closed {
                reason: "dropped".to_owned()
            }
        );
        assert_eq!(
            conn.on_closed("refused".to_owned()),
            Some(Duration::from_secs(1))
        );
    }

    #[test]
    fn error_schedules_reconnect() {
        let mut conn = Connection::new();
        assert_eq!(conn.on_error("no binary".to_owned()), Some(HALF_SECOND));
        assert_eq!(
            conn.state(),
            &State::Error {
                reason: "no binary".to_owned()
            }
        );
    }

    #[test]
    fn auth_failed_is_terminal_and_survives_close() {
        let mut conn = Connection::new();
        conn.on_connecting();
        conn.on_socket_open(51418);
        conn.on_auth_failed("bad token".to_owned());
        assert_eq!(conn.on_closed("closed".to_owned()), None);
        assert_eq!(conn.on_error("later".to_owned()), None);
        assert_eq!(
            conn.state(),
            &State::AuthFailed {
                reason: "bad token".to_owned()
            }
        );
    }

    #[test]
    fn stop_suppresses_reconnect() {
        let mut conn = open(20);
        conn.stop();
        assert_eq!(conn.on_closed("killed".to_owned()), None);
        assert_eq!(conn.on_error("gone".to_owned()), None);
    }

    #[test]
    fn restart_from_auth_failed_is_fresh_connect() {
        let mut conn = Connection::new();
        conn.on_connecting();
        conn.on_socket_open(51418);
        conn.on_auth_failed("bad token".to_owned());
        assert_eq!(conn.restart(), RestartAction::FreshConnect);
        assert_eq!(conn.state(), &State::Connecting);
        assert_eq!(conn.on_closed("dropped".to_owned()), Some(HALF_SECOND));
    }

    #[test]
    fn restart_when_open_sends_shutdown() {
        let mut conn = open(20);
        assert_eq!(conn.restart(), RestartAction::SendShutdown);
        assert_eq!(
            conn.state(),
            &State::Open {
                negotiated: 20,
                features: Vec::new()
            }
        );

        let mut closed = open(20);
        closed.on_closed("dropped".to_owned());
        assert_eq!(closed.restart(), RestartAction::FreshConnect);

        let mut stopped = open(20);
        stopped.stop();
        assert_eq!(stopped.restart(), RestartAction::FreshConnect);
        assert_eq!(stopped.on_closed("dropped".to_owned()), Some(HALF_SECOND));
    }

    fn remote() -> Connection {
        Connection::for_target(Target::tailnet("box.tail1234.ts.net", None))
    }

    fn open_remote() -> Connection {
        let mut conn = remote();
        conn.on_connecting();
        conn.on_socket_open(8787);
        conn.on_welcome(23, Vec::new());
        conn
    }

    #[test]
    fn remote_restart_reconnects_without_shutdown() {
        assert_eq!(open_remote().restart_label(), "Reconnect now");
        assert_eq!(open(20).restart_label(), "Restart daemon");
        assert_eq!(open_remote().restart(), RestartAction::FreshConnect);

        let mut conn = remote();
        for _ in 0..3 {
            conn.on_closed("refused".to_owned());
        }
        conn.on_connecting();
        conn.on_socket_open(8787);
        assert_eq!(conn.restart(), RestartAction::FreshConnect);
        assert_eq!(conn.state(), &State::Connecting);
        assert_eq!(
            conn.on_closed("dropped".to_owned()),
            Some(HALF_SECOND),
            "Reconnect resets the backoff"
        );
    }

    #[test]
    fn restart_after_a_drop_on_a_tailnet_target_fresh_connects_to_it() {
        let mut conn = open_remote();
        conn.on_closed("dropped".to_owned());
        let host = conn.target().clone();
        assert_eq!(conn.restart(), RestartAction::FreshConnect);
        assert_eq!(conn.target(), &host, "the redial keeps the host it was on");
    }

    #[test]
    fn remote_auth_failed_left_by_reconnect() {
        let mut conn = remote();
        conn.on_connecting();
        conn.on_socket_open(8787);
        conn.on_auth_failed("device not approved".to_owned());
        assert_eq!(conn.on_closed("closed".to_owned()), None);
        assert_eq!(conn.restart(), RestartAction::FreshConnect);
        assert_eq!(conn.state(), &State::Connecting);
        assert_eq!(conn.on_closed("dropped".to_owned()), Some(HALF_SECOND));
    }

    #[test]
    fn remote_pill_shows_short_name_without_port() {
        let footer = open_remote().footer(3);
        assert_eq!(footer.port, None);
        assert_eq!(footer.pill(), "box · 3 sessions");
        let mut closed = open_remote();
        closed.on_closed("dropped".to_owned());
        assert_eq!(closed.footer(3).pill(), "box · disconnected");

        assert_eq!(open(20).footer(3).pill(), "daemon · 3 sessions · :51418");
        assert_eq!(Connection::new().footer(0).pill(), "daemon · starting…");
    }

    #[test]
    fn remote_overlay_says_connecting_to_short_name() {
        let mut conn = remote();
        assert_eq!(conn.overlay_text().as_deref(), Some("Connecting to box…"));
        conn.on_connecting();
        assert_eq!(conn.overlay_text().as_deref(), Some("Connecting to box…"));
        conn.on_closed("refused".to_owned());
        assert_eq!(
            conn.overlay_text().as_deref(),
            Some("box dropped — reconnecting…")
        );
        assert_eq!(open_remote().overlay_text(), None);

        let mut local = Connection::new();
        assert_eq!(local.overlay_text().as_deref(), Some("Starting daemon…"));
        local.on_connecting();
        assert_eq!(
            local.overlay_text().as_deref(),
            Some("Connecting to daemon…")
        );
    }

    /// A tailnet connection whose daemon said this device is not approved.
    fn not_approved(had_token: bool) -> Connection {
        let mut conn = remote();
        conn.on_connecting();
        conn.on_socket_open(8787);
        conn.on_not_approved(protocol::DEVICE_NOT_APPROVED.to_owned(), had_token);
        conn
    }

    /// A host that has not approved this device is dialled again only on a
    /// command: neither its close nor a later error schedules a reconnect.
    #[test]
    fn not_approved_survives_close() {
        for had_token in [false, true] {
            let mut conn = not_approved(had_token);
            assert_eq!(conn.on_closed("closed".to_owned()), None, "close");
            assert_eq!(conn.on_error("later".to_owned()), None, "error");
            assert_eq!(
                conn.state(),
                &State::NotApproved {
                    reason: protocol::DEVICE_NOT_APPROVED.to_owned(),
                    had_token,
                }
            );
        }
    }

    #[test]
    fn not_approved_footer_says_not_paired() {
        let conn = not_approved(false);
        let footer = conn.footer(0);
        assert_eq!(footer.label, "not paired");
        assert_eq!(footer.dot, DotKind::Err);
        assert_eq!(footer.pill(), "box · not paired");
        assert_eq!(footer.tooltip, "not paired — device not approved");
        assert!(!conn.is_open(), "a pairing connection is not open");
    }

    #[test]
    fn not_approved_overlay_names_the_host() {
        let mut conn = not_approved(true);
        assert_eq!(conn.overlay_text().as_deref(), Some("Pair with box"));
        assert_eq!(
            conn.on_closed("daemon closed the connection".to_owned()),
            None,
            "the pairing socket's close schedules no reconnect"
        );
        assert_eq!(conn.on_error("later".to_owned()), None);
        assert_eq!(
            conn.overlay_text().as_deref(),
            Some("Pair with box"),
            "the close keeps the not-approved state"
        );
        assert_eq!(conn.restart(), RestartAction::FreshConnect);
        assert_eq!(conn.state(), &State::Connecting);

        assert_eq!(
            open_remote().overlay_text(),
            None,
            "a served host shows no overlay"
        );
    }

    #[test]
    fn remote_stop_is_unavailable() {
        assert!(
            Connection::new().can_stop(),
            "Stop stays on the local daemon"
        );
        let mut conn = open_remote();
        assert!(!conn.can_stop());
        conn.stop();
        assert_eq!(
            conn.on_closed("dropped".to_owned()),
            Some(HALF_SECOND),
            "a Stop must not end the reconnects to a remote host"
        );
    }

    #[test]
    fn watchdog_detects_resume_after_gap() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut dog = Watchdog::new(start);
        assert!(dog.tick(start + RESUME_GAP));
        assert!(dog.tick(start + RESUME_GAP + Duration::from_secs(3_600)));
    }

    #[test]
    fn watchdog_ignores_normal_ticks() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut dog = Watchdog::new(start);
        let mut now = start;
        for _ in 0..10 {
            now += TICK;
            assert!(!dog.tick(now));
        }
        assert!(!dog.tick(now + RESUME_GAP - Duration::from_millis(1)));
    }

    #[test]
    fn watchdog_survives_backwards_clock() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut dog = Watchdog::new(start);
        let earlier = start - Duration::from_secs(3_600);
        assert!(!dog.tick(earlier));
        assert!(!dog.tick(earlier + TICK));
    }

    #[test]
    fn footer_labels_per_state() {
        let mut conn = Connection::new();
        assert_eq!(conn.footer(0).label, "starting…");
        assert_eq!(conn.footer(0).dot, DotKind::Pending);
        conn.on_connecting();
        assert_eq!(conn.footer(0).label, "connecting…");
        assert_eq!(conn.footer(0).dot, DotKind::Pending);
        conn.on_socket_open(51418);
        conn.on_welcome(20, Vec::new());
        assert_eq!(conn.footer(0).label, "0 sessions");
        assert_eq!(conn.footer(1).label, "1 session");
        assert_eq!(conn.footer(3).label, "3 sessions");
        assert_eq!(conn.footer(3).dot, DotKind::Ok);
        conn.on_closed("dropped".to_owned());
        assert_eq!(conn.footer(3).label, "disconnected");
        assert_eq!(conn.footer(3).dot, DotKind::Idle);
        conn.on_error("no binary".to_owned());
        assert_eq!(conn.footer(3).label, "error");
        assert_eq!(conn.footer(3).dot, DotKind::Err);
        conn.on_auth_failed("bad token".to_owned());
        assert_eq!(conn.footer(3).label, "auth failed");
        assert_eq!(conn.footer(3).dot, DotKind::Err);

        let mut stopped = open(20);
        stopped.stop();
        assert_eq!(stopped.footer(2).label, "2 sessions");
        stopped.on_closed("killed".to_owned());
        assert_eq!(stopped.footer(2).label, "stopped");
        assert_eq!(stopped.footer(2).dot, DotKind::Stopped);
    }

    #[test]
    fn footer_port_only_when_open() {
        let mut conn = Connection::new();
        conn.on_connecting();
        conn.on_socket_open(51418);
        assert_eq!(conn.footer(0).port, None);
        conn.on_welcome(20, Vec::new());
        assert_eq!(conn.footer(0).port, Some(51418));
        conn.on_closed("dropped".to_owned());
        assert_eq!(conn.footer(0).port, None);
    }

    #[test]
    fn footer_tooltip_includes_reason() {
        let mut conn = open(20);
        assert_eq!(conn.footer(1).tooltip, "1 session");
        conn.on_closed("connection reset".to_owned());
        assert_eq!(conn.footer(1).tooltip, "disconnected — connection reset");
        conn.on_auth_failed("bad token".to_owned());
        assert_eq!(conn.footer(1).tooltip, "auth failed — bad token");
    }

    #[test]
    fn overlay_hidden_after_first_connect() {
        let mut conn = open(20);
        assert_eq!(conn.overlay(), None);
        conn.on_closed("dropped".to_owned());
        assert_eq!(conn.overlay(), None);
        conn.on_connecting();
        assert_eq!(conn.overlay(), None);

        let mut failed = Connection::new();
        failed.on_error("no binary".to_owned());
        assert_eq!(failed.overlay(), None);
        failed.on_auth_failed("bad token".to_owned());
        assert_eq!(failed.overlay(), None);
    }

    #[test]
    fn overlay_labels_before_first_connect() {
        let mut conn = Connection::new();
        assert_eq!(conn.overlay(), Some("Starting daemon…"));
        conn.on_connecting();
        assert_eq!(conn.overlay(), Some("Connecting to daemon…"));
        conn.on_closed("refused".to_owned());
        assert_eq!(conn.overlay(), Some("Daemon dropped — reconnecting…"));
        conn.stop();
        assert_eq!(conn.overlay(), Some("Waiting for daemon…"));
    }

    fn welcome(conn: &mut Connection) {
        conn.on_socket_open(51418);
        conn.on_welcome(
            20,
            vec![FEATURE_AGENT_COMMANDS.to_owned(), "later".to_owned()],
        );
    }

    #[test]
    fn features_come_with_welcome() {
        let mut conn = Connection::new();
        assert!(
            !conn.supports(FEATURE_AGENT_COMMANDS),
            "none before Welcome"
        );
        conn.on_connecting();
        conn.on_socket_open(51418);
        assert!(
            !conn.supports(FEATURE_AGENT_COMMANDS),
            "none before Welcome"
        );
        welcome(&mut conn);
        assert!(conn.supports(FEATURE_AGENT_COMMANDS));
        assert!(conn.supports("later"), "unknown features are kept");
        assert!(!conn.supports("unlisted"));

        let bare = open(20);
        assert!(
            !bare.supports(FEATURE_AGENT_COMMANDS),
            "an older daemon lists none"
        );
    }

    #[test]
    fn features_clear_when_the_connection_drops() {
        let mut conn = Connection::new();
        welcome(&mut conn);
        conn.on_closed("dropped".to_owned());
        assert!(!conn.supports(FEATURE_AGENT_COMMANDS), "cleared on close");

        welcome(&mut conn);
        conn.on_error("no binary".to_owned());
        assert!(!conn.supports(FEATURE_AGENT_COMMANDS), "cleared on error");

        welcome(&mut conn);
        conn.on_connecting();
        assert!(
            !conn.supports(FEATURE_AGENT_COMMANDS),
            "cleared on reconnect"
        );

        welcome(&mut conn);
        conn.on_auth_failed("bad token".to_owned());
        assert!(
            !conn.supports(FEATURE_AGENT_COMMANDS),
            "cleared on auth failure"
        );
        assert_eq!(conn.restart(), RestartAction::FreshConnect);

        welcome(&mut conn);
        conn.stop();
        assert_eq!(conn.restart(), RestartAction::FreshConnect);
        assert!(
            !conn.supports(FEATURE_AGENT_COMMANDS),
            "cleared on a fresh connect"
        );
    }
}
