//! Keep the host awake while sessions need it, as the user's mode says.
//!
//! An agent session mid-task — or parked at its prompt waiting for the user
//! to come back — is silently killed when the machine idle-sleeps. The daemon
//! owns this hold rather than the app window because the daemon is the process
//! whose lifetime matches "rustling-tulip is running": it outlives the window
//! and self-exits when idle (`idle_exit.rs`).
//!
//! The persisted [`KeepAwakeMode`] picks the predicate. `Off` never holds.
//! `WhenBusy` holds while a session is busy by the client's own busy set:
//! working (a plain shell counts while its output classifier marks it
//! working), waiting on the user, spawning, or idle with background work; a
//! session marked "Don't count as busy" never holds. It is released as soon
//! as the last busy session settles, after only the status classifiers' own
//! hysteresis. `Always` holds while any session has a live child, the
//! predicate shared with the idle-exit watcher
//! ([`crate::idle_exit::blocks_exit`]), so the hold covers exactly the
//! sessions that keep the daemon resident.
//!
//! System sleep only — the display is left to its own timer.

use crate::idle_exit::blocks_exit;
use crate::server::StateEvent;
use crate::session::{SessionEvent, SessionRegistry};
use protocol::{KeepAwakeMode, SessionSnapshot, SessionStatus};
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, watch};
use tracing::info;

/// What the UI shows: `mode` is the persisted user setting, `active` is
/// whether the OS hold is engaged right now (a session meets the mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub mode: KeepAwakeMode,
    pub active: bool,
}

/// Whether `session` holds the machine awake under `mode`. A mode this
/// daemon does not know holds nothing.
fn holds_awake(mode: KeepAwakeMode, session: &SessionSnapshot) -> bool {
    match mode {
        KeepAwakeMode::Off | KeepAwakeMode::Unknown => false,
        KeepAwakeMode::WhenBusy => is_busy(session),
        KeepAwakeMode::Always => blocks_exit(session.status, session.is_abandoned),
    }
}

/// The native client's busy set over a live session the user has not
/// marked "Don't count as busy".
fn is_busy(session: &SessionSnapshot) -> bool {
    if session.is_abandoned || session.busy_excluded {
        return false;
    }
    match session.status {
        SessionStatus::Working | SessionStatus::AwaitingInput | SessionStatus::Spawning => true,
        SessionStatus::Idle => session.background,
        SessionStatus::Stopped | SessionStatus::Error => false,
    }
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
/// the OS awake while any session meets the mode on `mode_rx`, and
/// publishing every change on `status_tx` (for the initial-state push) and
/// `state_events` (for live clients).
pub fn spawn(
    sessions: Arc<SessionRegistry>,
    mode_rx: watch::Receiver<KeepAwakeMode>,
    status_tx: watch::Sender<Status>,
    state_events: broadcast::Sender<StateEvent>,
    inhibitor: Box<dyn Inhibitor>,
) {
    tokio::spawn(run(sessions, mode_rx, status_tx, state_events, inhibitor));
}

/// Block until the setting or any session changes. Returns `false` when a
/// channel is gone (daemon teardown) — the watcher should exit. A `Lagged`
/// session-event receiver is fine: the caller re-derives state from registry
/// snapshots, so dropped events only mean a spurious wake-up.
async fn wait_for_change(
    mode: &mut watch::Receiver<KeepAwakeMode>,
    session_events: &mut broadcast::Receiver<SessionEvent>,
) -> bool {
    tokio::select! {
        res = mode.changed() => res.is_ok(),
        evt = session_events.recv() => !matches!(evt, Err(RecvError::Closed)),
    }
}

async fn run(
    sessions: Arc<SessionRegistry>,
    mut mode_rx: watch::Receiver<KeepAwakeMode>,
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
        let mode = *mode_rx.borrow_and_update();
        let active = sessions.snapshots().iter().any(|s| holds_awake(mode, s));
        if active != held {
            inhibitor.set(active);
            held = active;
            if active {
                info!("keep-awake: hold engaged");
            } else {
                info!("keep-awake: hold released");
            }
        }
        let status = Status { mode, active };
        if published != Some(status) {
            published = Some(status);
            status_tx.send_replace(status);
            let _ = state_events.send(StateEvent::KeepAwakeStatus { mode, active });
        }
        if !wait_for_change(&mut mode_rx, &mut session_events).await {
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
    use protocol::{AppearanceOverrides, SessionKind, SessionMetrics, SessionMode, SessionStatus};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    /// The hang guard on "the watcher reacted", failing as "hung"; the waits
    /// return as soon as the value arrives.
    const HANG_GUARD: Duration = Duration::from_secs(120);
    /// A virtual sleep on the paused clock. It ends only once every task is
    /// idle, so after it the watcher has handled everything sent before it.
    const SETTLE: Duration = Duration::from_millis(300);

    fn scratch_dirs(tag: &str) -> Dirs {
        let root = std::env::temp_dir().join(format!("rt-keep-awake-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create scratch dir");
        Dirs {
            config: root.clone(),
            state_file: root.join("state.json"),
            handshake_file: root.join("daemon.json"),
            remote_config_file: root.join("remote.json"),
            sessions_dir: root.join("sessions"),
            worktrees_dir: root.join("worktrees"),
            binaries_dir: root.join("binaries"),
            bin_templates: None,
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
            status_since: Some(Utc::now()),
            status_seq: 0,
            exit_code: None,
            metrics: SessionMetrics::default(),
            recent_actions: Vec::new(),
            pty: None,
            headless: None,
            workspace_id: None,
            agent: agent_registry::default_agent(),
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
            screen: None,
            scrollback_snapshot_req: None,
            spawn_origin: None,
            conversation_id: None,
            pending_input: None,
            last_hook_time: None,
            hook_driven: false,
            background: false,
            last_assistant_message: None,
            summary: None,
            busy_excluded: false,
            agent_in_shell: false,
            shell_label: None,
            agent_exit_announced: false,
            applied_size: None,
            work: crate::background_work::BackgroundWork::default(),
            hook_agent: None,
            hook_options: protocol::OptionValues::new(),
            hook_trusted: false,
            resumed_from: Vec::new(),
        }
    }

    /// Records every `set` call instead of touching the host's power state.
    struct Recorder(mpsc::UnboundedSender<bool>);

    impl Inhibitor for Recorder {
        fn set(&mut self, hold: bool) {
            let _ = self.0.send(hold);
        }
    }

    struct Harness {
        sessions: Arc<SessionRegistry>,
        mode_tx: watch::Sender<KeepAwakeMode>,
        status_rx: watch::Receiver<Status>,
        events_rx: broadcast::Receiver<StateEvent>,
        holds: mpsc::UnboundedReceiver<bool>,
    }

    impl Harness {
        /// The watcher's next hold change, failing as hung when none comes.
        async fn next_hold(&mut self) -> bool {
            timeout(HANG_GUARD, self.holds.recv())
                .await
                .expect("hung: the hold never changed")
                .expect("the watcher holds the recorder")
        }

        /// True when the watcher, once idle, has made no hold change.
        async fn stays_put(&mut self) -> bool {
            tokio::time::sleep(SETTLE).await;
            matches!(self.holds.try_recv(), Err(mpsc::error::TryRecvError::Empty))
        }
    }

    fn status(mode: KeepAwakeMode, active: bool) -> Status {
        Status { mode, active }
    }

    fn start(tag: &str, mode: KeepAwakeMode) -> Harness {
        let sessions = SessionRegistry::new(scratch_dirs(tag));
        let (mode_tx, mode_rx) = watch::channel(mode);
        let (status_tx, status_rx) = watch::channel(status(mode, false));
        let (state_events, events_rx) = broadcast::channel(16);
        let (hold_tx, holds) = mpsc::unbounded_channel();
        spawn(
            Arc::clone(&sessions),
            mode_rx,
            status_tx,
            state_events,
            Box::new(Recorder(hold_tx)),
        );
        Harness {
            sessions,
            mode_tx,
            status_rx,
            events_rx,
            holds,
        }
    }

    /// Drain state events until the keep-awake status matches `want`. `false`
    /// means the broadcast closed before it showed up.
    async fn saw_status_event(rx: &mut broadcast::Receiver<StateEvent>, want: Status) -> bool {
        while let Ok(evt) = rx.recv().await {
            if let StateEvent::KeepAwakeStatus { mode, active } = evt
                && mode == want.mode
                && active == want.active
            {
                return true;
            }
        }
        false
    }

    #[tokio::test(start_paused = true)]
    async fn always_holds_while_any_session_is_live() {
        let mut h = start("always", KeepAwakeMode::Always);
        assert!(
            h.stays_put().await,
            "no sessions means nothing to keep the machine awake for"
        );

        h.sessions
            .insert(test_record("s1", SessionStatus::Idle, false));
        assert!(h.next_hold().await, "an idle live session engages the hold");

        h.sessions
            .update("s1", |r| r.status = SessionStatus::Stopped);
        assert!(
            !h.next_hold().await,
            "the last session stopping must release the hold"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn off_never_holds() {
        let mut h = start("off", KeepAwakeMode::Off);
        h.sessions
            .insert(test_record("s1", SessionStatus::Working, false));
        h.sessions
            .insert(test_record("s2", SessionStatus::AwaitingInput, false));
        assert!(h.stays_put().await, "Off never holds the machine awake");
    }

    #[tokio::test(start_paused = true)]
    async fn when_busy_holds_only_while_a_session_is_busy() {
        let mut h = start("busy", KeepAwakeMode::WhenBusy);
        h.sessions
            .insert(test_record("s1", SessionStatus::Idle, false));
        assert!(h.stays_put().await, "an idle session is not busy");

        h.sessions
            .update("s1", |r| r.status = SessionStatus::Working);
        assert!(h.next_hold().await, "a working session engages the hold");

        h.sessions.update("s1", |r| r.status = SessionStatus::Idle);
        assert!(!h.next_hold().await, "going idle releases the hold");

        h.sessions
            .update("s1", |r| r.status = SessionStatus::Stopped);
        assert!(h.stays_put().await, "a stopped session is not busy either");
    }

    #[tokio::test(start_paused = true)]
    async fn when_busy_counts_a_printing_shell_and_awaiting_input() {
        let mut h = start("busy-shell", KeepAwakeMode::WhenBusy);
        h.sessions.insert(SessionRecord {
            mode: SessionMode::PlainShell,
            ..test_record("sh", SessionStatus::Idle, false)
        });
        assert!(h.stays_put().await, "a quiet shell is not busy");

        h.sessions
            .update("sh", |r| r.status = SessionStatus::Working);
        assert!(h.next_hold().await, "a shell printing output is busy");
        h.sessions.update("sh", |r| r.status = SessionStatus::Idle);
        assert!(
            !h.next_hold().await,
            "the shell going quiet releases the hold"
        );

        h.sessions
            .insert(test_record("s2", SessionStatus::AwaitingInput, false));
        assert!(h.next_hold().await, "a session waiting on the user is busy");
        h.sessions.update("s2", |r| {
            r.status = SessionStatus::Idle;
            r.background = true;
        });
        assert!(
            h.stays_put().await,
            "idle with background work is still busy"
        );
        h.sessions.update("s2", |r| r.background = false);
        assert!(
            !h.next_hold().await,
            "the background work ending releases it"
        );

        h.sessions
            .insert(test_record("s3", SessionStatus::Spawning, false));
        assert!(h.next_hold().await, "a spawning session is busy");
    }

    #[tokio::test(start_paused = true)]
    async fn when_busy_ignores_busy_excluded_sessions() {
        let mut h = start("busy-excluded", KeepAwakeMode::WhenBusy);
        h.sessions.insert(SessionRecord {
            busy_excluded: true,
            ..test_record("dev-server", SessionStatus::Working, false)
        });
        assert!(
            h.stays_put().await,
            "a session marked Don't count as busy never holds the machine"
        );

        h.sessions
            .insert(test_record("s2", SessionStatus::Working, false));
        assert!(h.next_hold().await, "an ordinary busy session still does");
    }

    #[tokio::test(start_paused = true)]
    async fn setting_gates_the_hold() {
        let mut h = start("setting", KeepAwakeMode::Off);
        h.sessions
            .insert(test_record("s1", SessionStatus::Idle, false));
        assert!(
            h.stays_put().await,
            "a live session must not engage the hold while the setting is off"
        );

        h.mode_tx.send_replace(KeepAwakeMode::Always);
        assert!(
            h.next_hold().await,
            "switching to Always with a live session engages the hold"
        );

        h.mode_tx.send_replace(KeepAwakeMode::WhenBusy);
        assert!(
            !h.next_hold().await,
            "switching to When busy with only an idle session releases it"
        );

        h.mode_tx.send_replace(KeepAwakeMode::Always);
        assert!(h.next_hold().await, "back to Always engages it again");

        h.mode_tx.send_replace(KeepAwakeMode::Off);
        assert!(
            !h.next_hold().await,
            "turning the setting off releases the hold immediately"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn abandoned_sessions_do_not_engage_the_hold() {
        let mut h = start("abandoned", KeepAwakeMode::Always);
        h.sessions
            .insert(test_record("s1", SessionStatus::Working, true));
        assert!(
            h.stays_put().await,
            "an abandoned session has no live child to protect"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn status_is_published_on_the_watch_and_the_broadcast() {
        let mut h = start("status", KeepAwakeMode::Always);
        h.sessions
            .insert(test_record("s1", SessionStatus::Working, false));
        assert!(h.next_hold().await);

        let engaged = status(KeepAwakeMode::Always, true);
        assert!(
            timeout(HANG_GUARD, saw_status_event(&mut h.events_rx, engaged))
                .await
                .expect("hung: the status was never broadcast"),
            "clients must be told the hold engaged"
        );

        timeout(HANG_GUARD, async {
            while *h.status_rx.borrow_and_update() != engaged {
                if h.status_rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .expect("hung: the status watch never reflected the engaged hold");
        assert_eq!(*h.status_rx.borrow(), engaged);
    }
}
