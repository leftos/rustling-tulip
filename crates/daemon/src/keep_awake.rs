//! Keep the host awake while any session has a live child.
//!
//! A `claude` session mid-task — or parked at its prompt waiting for the user
//! to come back — is silently killed when the machine idle-sleeps. The daemon
//! owns this hold rather than the app window because the daemon is the process
//! whose lifetime matches "rustling-tulip is running": it outlives the window
//! and self-exits when idle (`idle_exit.rs`). The predicate is shared with that
//! watcher ([`crate::idle_exit::blocks_exit`]) so the OS hold covers exactly
//! the sessions that keep the daemon resident. Busy-vs-idle status is
//! deliberately not consulted: status detection is heuristic, and a misread
//! idle would let the box sleep mid-task.
//!
//! System sleep only — the display is left to its own timer.

use crate::idle_exit::blocks_exit;
use crate::server::StateEvent;
use crate::session::{SessionEvent, SessionRegistry};
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, watch};
use tracing::info;

/// What the UI shows: `enabled` is the persisted user setting, `active` is
/// whether the OS hold is engaged right now (`enabled` and a session is live).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub enabled: bool,
    pub active: bool,
}

/// The OS boundary. Injectable so the watcher's transition logic can be tested
/// without touching the host's power state.
pub trait Inhibitor: Send {
    /// Engage (`hold = true`) or release the platform sleep inhibitor.
    /// Implementations are best-effort: a failure is logged, never fatal.
    fn set(&mut self, hold: bool);
}

/// The inhibitor for the platform this daemon was built for.
#[cfg(windows)]
pub fn native() -> Box<dyn Inhibitor> {
    Box::new(windows_impl::ThreadExecutionState::new())
}

/// The inhibitor for the platform this daemon was built for.
#[cfg(target_os = "macos")]
pub fn native() -> Box<dyn Inhibitor> {
    Box::new(macos_impl::Caffeinate::default())
}

/// The inhibitor for the platform this daemon was built for.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn native() -> Box<dyn Inhibitor> {
    Box::new(unsupported::Unsupported::default())
}

#[cfg(windows)]
mod windows_impl {
    use super::Inhibitor;
    use std::sync::mpsc::{Sender, channel};
    use tracing::warn;
    use windows::Win32::System::Power::{
        ES_CONTINUOUS, ES_SYSTEM_REQUIRED, EXECUTION_STATE, SetThreadExecutionState,
    };

    /// `SetThreadExecutionState` sets the flag on the *calling* thread and the
    /// OS clears it when that thread exits. Tokio tasks migrate between worker
    /// threads, so they must never call the API directly — the hold would land
    /// on an arbitrary worker and be dropped at random. Hence a dedicated
    /// thread driven over a channel: it is the one thread whose lifetime we
    /// control, and dropping the sender ends it, which releases the hold.
    pub struct ThreadExecutionState {
        tx: Sender<bool>,
    }

    impl ThreadExecutionState {
        pub fn new() -> Self {
            let (tx, rx) = channel::<bool>();
            let spawned = std::thread::Builder::new()
                .name("rt-keepawake".into())
                .spawn(move || {
                    for hold in rx {
                        let flags = if hold {
                            ES_CONTINUOUS | ES_SYSTEM_REQUIRED
                        } else {
                            ES_CONTINUOUS
                        };
                        // SAFETY: no pointer arguments and no preconditions;
                        // the call just sets a flag on the current thread.
                        let prev = unsafe { SetThreadExecutionState(flags) };
                        if prev == EXECUTION_STATE(0) {
                            warn!(
                                hold,
                                "SetThreadExecutionState failed; keep-awake hold not applied"
                            );
                        }
                    }
                });
            if let Err(err) = spawned {
                warn!(
                    ?err,
                    "failed to start the keep-awake thread; hold unavailable"
                );
            }
            Self { tx }
        }
    }

    impl Inhibitor for ThreadExecutionState {
        fn set(&mut self, hold: bool) {
            if let Err(err) = self.tx.send(hold) {
                warn!(?err, hold, "keep-awake thread is gone; hold not applied");
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod macos_impl {
    use super::Inhibitor;
    use std::process::{Child, Command, Stdio};
    use tracing::warn;

    /// `caffeinate -i` asserts "no idle system sleep" for as long as it runs;
    /// `-w <daemon pid>` makes it exit on its own if the daemon dies without
    /// releasing, so a crash can't leave the machine pinned awake forever.
    #[derive(Default)]
    pub struct Caffeinate {
        child: Option<Child>,
    }

    impl Inhibitor for Caffeinate {
        fn set(&mut self, hold: bool) {
            if hold {
                if self.child.is_some() {
                    return;
                }
                match Command::new("/usr/bin/caffeinate")
                    .args(["-i", "-w", &std::process::id().to_string()])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    Ok(child) => self.child = Some(child),
                    Err(err) => warn!(
                        ?err,
                        "failed to spawn caffeinate; keep-awake hold not applied"
                    ),
                }
            } else if let Some(mut child) = self.child.take() {
                if let Err(err) = child.kill() {
                    warn!(
                        ?err,
                        "failed to stop caffeinate; keep-awake hold may persist"
                    );
                }
                if let Err(err) = child.wait() {
                    warn!(?err, "failed to reap caffeinate");
                }
            }
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod unsupported {
    use super::Inhibitor;
    use tracing::warn;

    /// No inhibitor API is wired up for this platform. Warns once so the log
    /// explains why sessions still get slept, then stays quiet.
    #[derive(Default)]
    pub struct Unsupported {
        warned: bool,
    }

    impl Inhibitor for Unsupported {
        fn set(&mut self, hold: bool) {
            if hold && !self.warned {
                self.warned = true;
                warn!("keep-awake is not supported on this platform");
            }
        }
    }
}

/// Spawn the keep-awake watcher task. Runs for the daemon's lifetime, holding
/// the OS awake while `enabled_rx` is on and any session blocks the idle exit,
/// and publishing every change on `status_tx` (for the initial-state push) and
/// `state_events` (for live clients).
pub fn spawn(
    sessions: Arc<SessionRegistry>,
    enabled_rx: watch::Receiver<bool>,
    status_tx: watch::Sender<Status>,
    state_events: broadcast::Sender<StateEvent>,
    inhibitor: Box<dyn Inhibitor>,
) {
    tokio::spawn(run(
        sessions,
        enabled_rx,
        status_tx,
        state_events,
        inhibitor,
    ));
}

/// Block until the setting or any session changes. Returns `false` when a
/// channel is gone (daemon teardown) — the watcher should exit. A `Lagged`
/// session-event receiver is fine: the caller re-derives state from registry
/// snapshots, so dropped events only mean a spurious wake-up.
async fn wait_for_change(
    enabled: &mut watch::Receiver<bool>,
    session_events: &mut broadcast::Receiver<SessionEvent>,
) -> bool {
    tokio::select! {
        res = enabled.changed() => res.is_ok(),
        evt = session_events.recv() => !matches!(evt, Err(RecvError::Closed)),
    }
}

async fn run(
    sessions: Arc<SessionRegistry>,
    mut enabled_rx: watch::Receiver<bool>,
    status_tx: watch::Sender<Status>,
    state_events: broadcast::Sender<StateEvent>,
    mut inhibitor: Box<dyn Inhibitor>,
) {
    let mut session_events = sessions.subscribe();
    let mut held = false;
    let mut published: Option<Status> = None;
    loop {
        // Never trust event payloads: re-derive from the setting and the
        // current registry contents on every wake-up.
        let enabled = *enabled_rx.borrow_and_update();
        let active = enabled
            && sessions
                .snapshots()
                .iter()
                .any(|s| blocks_exit(s.status, s.is_abandoned));
        if active != held {
            inhibitor.set(active);
            held = active;
            if active {
                info!("keep-awake: hold engaged");
            } else {
                info!("keep-awake: hold released");
            }
        }
        let status = Status { enabled, active };
        if published != Some(status) {
            published = Some(status);
            status_tx.send_replace(status);
            let _ = state_events.send(StateEvent::KeepAwakeStatus { enabled, active });
        }
        if !wait_for_change(&mut enabled_rx, &mut session_events).await {
            break;
        }
    }
    if held {
        inhibitor.set(false);
        info!("keep-awake: watcher stopping; hold released");
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::paths::Dirs;
    use crate::session::SessionRecord;
    use chrono::Utc;
    use protocol::{
        Agent, AppearanceOverrides, SessionKind, SessionMetrics, SessionMode, SessionStatus,
    };
    use std::sync::mpsc::Receiver as StdReceiver;
    use std::time::Duration;
    use tokio::time::timeout;

    /// Generous upper bound for "the watcher reacted"; the assertions return
    /// as soon as the value arrives, so a healthy run never waits this long.
    const WAIT: Duration = Duration::from_secs(5);
    /// How long to watch for a hold that must never come.
    const QUIET: Duration = Duration::from_millis(300);

    fn scratch_dirs(tag: &str) -> Dirs {
        let root = std::env::temp_dir().join(format!("rt-keep-awake-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create scratch dir");
        Dirs {
            config: root.clone(),
            state_file: root.join("state.json"),
            handshake_file: root.join("daemon.json"),
            lan_config_file: root.join("lan.json"),
            lan_cert_file: root.join("lan-cert.pem"),
            lan_key_file: root.join("lan-key.pem"),
            sessions_dir: root.join("sessions"),
            worktrees_dir: root.join("worktrees"),
            binaries_dir: root.join("binaries"),
        }
    }

    fn test_record(id: &str, status: SessionStatus, is_abandoned: bool) -> SessionRecord {
        SessionRecord {
            id: id.to_string(),
            label: id.to_string(),
            default_label: id.to_string(),
            user_label: None,
            kind: SessionKind::Standalone,
            members: Vec::new(),
            mode: SessionMode::Interactive,
            started_at: Utc::now(),
            status,
            exit_code: None,
            metrics: SessionMetrics::default(),
            recent_actions: Vec::new(),
            pty: None,
            headless: None,
            workspace_id: None,
            agent: Agent::default(),
            terminal_title: None,
            program_name: None,
            current_cwd: None,
            appearance: AppearanceOverrides::default(),
            spawn_config: None,
            is_abandoned,
            is_inactive: false,
            worktree_paths: Vec::new(),
            last_prompt: None,
            input_notifier: None,
            scrollback_snapshot_req: None,
            spawn_origin: None,
        }
    }

    /// Records every `set` call instead of touching the host's power state.
    struct Recorder(std::sync::mpsc::Sender<bool>);

    impl Inhibitor for Recorder {
        fn set(&mut self, hold: bool) {
            let _ = self.0.send(hold);
        }
    }

    struct Harness {
        sessions: Arc<SessionRegistry>,
        enabled_tx: watch::Sender<bool>,
        status_rx: watch::Receiver<Status>,
        events_rx: broadcast::Receiver<StateEvent>,
        holds: StdReceiver<bool>,
    }

    fn status(enabled: bool, active: bool) -> Status {
        Status { enabled, active }
    }

    fn start(tag: &str, enabled: bool) -> Harness {
        let sessions = SessionRegistry::new(scratch_dirs(tag));
        let (enabled_tx, enabled_rx) = watch::channel(enabled);
        let (status_tx, status_rx) = watch::channel(status(enabled, false));
        let (state_events, events_rx) = broadcast::channel(16);
        let (hold_tx, holds) = std::sync::mpsc::channel();
        spawn(
            Arc::clone(&sessions),
            enabled_rx,
            status_tx,
            state_events,
            Box::new(Recorder(hold_tx)),
        );
        Harness {
            sessions,
            enabled_tx,
            status_rx,
            events_rx,
            holds,
        }
    }

    /// Drain state events until the keep-awake status matches `want`. `false`
    /// means the broadcast closed before it showed up.
    async fn saw_status_event(rx: &mut broadcast::Receiver<StateEvent>, want: Status) -> bool {
        while let Ok(evt) = rx.recv().await {
            if let StateEvent::KeepAwakeStatus { enabled, active } = evt
                && enabled == want.enabled
                && active == want.active
            {
                return true;
            }
        }
        false
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hold_tracks_live_sessions() {
        let h = start("live", true);
        assert!(
            h.holds.recv_timeout(QUIET).is_err(),
            "no sessions means nothing to keep the machine awake for"
        );

        h.sessions
            .insert(test_record("s1", SessionStatus::Working, false));
        assert!(
            h.holds.recv_timeout(WAIT).expect("hold engages"),
            "a live session must engage the hold"
        );

        h.sessions
            .update("s1", |r| r.status = SessionStatus::Stopped);
        assert!(
            !h.holds.recv_timeout(WAIT).expect("hold releases"),
            "the last session stopping must release the hold"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn setting_gates_the_hold() {
        let h = start("setting", false);
        h.sessions
            .insert(test_record("s1", SessionStatus::Working, false));
        assert!(
            h.holds.recv_timeout(QUIET).is_err(),
            "a live session must not engage the hold while the setting is off"
        );

        h.enabled_tx.send_replace(true);
        assert!(
            h.holds.recv_timeout(WAIT).expect("hold engages"),
            "turning the setting on with a live session engages the hold"
        );

        h.enabled_tx.send_replace(false);
        assert!(
            !h.holds.recv_timeout(WAIT).expect("hold releases"),
            "turning the setting off releases the hold immediately"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn abandoned_sessions_do_not_engage_the_hold() {
        let h = start("abandoned", true);
        h.sessions
            .insert(test_record("s1", SessionStatus::Working, true));
        assert!(
            h.holds.recv_timeout(QUIET).is_err(),
            "an abandoned session has no live child to protect"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_is_published_on_the_watch_and_the_broadcast() {
        let mut h = start("status", true);
        h.sessions
            .insert(test_record("s1", SessionStatus::Working, false));
        assert!(h.holds.recv_timeout(WAIT).expect("hold engages"));

        assert!(
            timeout(WAIT, saw_status_event(&mut h.events_rx, status(true, true)))
                .await
                .expect("status broadcast within the window"),
            "clients must be told the hold engaged"
        );

        timeout(WAIT, async {
            while *h.status_rx.borrow_and_update() != status(true, true) {
                if h.status_rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .expect("status watch reflects the engaged hold");
        assert_eq!(*h.status_rx.borrow(), status(true, true));
    }
}
