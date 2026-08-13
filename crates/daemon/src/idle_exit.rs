//! Idle self-exit: shut the daemon down when it has nothing left to do.
//!
//! The daemon deliberately outlives the app so live sessions survive app
//! restarts. But once the last client disconnects AND no session has (or may
//! still have) a live child process, staying resident only pins the
//! executable on disk — the next app launch with an updated binary has to go
//! through the `/shutdown` + respawn dance instead of just starting fresh.
//! This watcher flips the daemon-wide shutdown watch once that idle state has
//! held continuously for [`GRACE_PERIOD`].
//!
//! What keeps the daemon alive:
//! - any attached WS client (`Hub::client_count` > 0), local or LAN;
//! - any session whose child is (or may be) running — see [`blocks_exit`];
//! - an in-flight preset launch job, which is about to spawn sessions.
//!
//! What does NOT keep it alive: `Stopped`/`Error` records (inert — the child
//! is gone) and abandoned records (rebuilt from their on-disk sidecars by the
//! next daemon start, so nothing is lost by exiting).
//!
//! The shutdown mirrors the HTTP `/shutdown` path: no drain, sidecars and
//! scrollback stay on disk.

use crate::session::{SessionEvent, SessionRegistry};
use protocol::SessionStatus;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Mutex as AsyncMutex, broadcast, watch};
use tracing::info;

/// How long the zero-clients / zero-live-sessions state must hold before the
/// daemon exits. Long enough for an app restart, update install, or pop-out
/// reload to reconnect without killing the daemon out from under itself.
pub const GRACE_PERIOD: Duration = Duration::from_secs(30);

/// Cancellation handles for in-flight preset launch jobs — the same map as
/// `Hub::preset_cancellations`. A non-empty map means sessions are about to
/// be spawned, so the watcher holds off even though the registry is idle.
type PresetJobs = Arc<AsyncMutex<HashMap<String, watch::Sender<bool>>>>;

/// Spawn the idle-exit watcher task. Runs for the daemon's lifetime; flips
/// `shutdown_tx` to `true` (and exits) once the idle condition has held for
/// `grace`, or exits silently if either input channel closes (the daemon is
/// already tearing down some other way).
pub fn spawn(
    sessions: Arc<SessionRegistry>,
    client_count: watch::Receiver<usize>,
    shutdown_tx: watch::Sender<bool>,
    preset_jobs: PresetJobs,
    grace: Duration,
) {
    tokio::spawn(run(sessions, client_count, shutdown_tx, preset_jobs, grace));
}

/// A session keeps the daemon alive while its child is — or may still be —
/// running. `Stopped`/`Error` records are inert, and abandoned records come
/// back from their sidecars on the next daemon start, so neither blocks.
/// Orphans (live-but-detached children) conservatively DO block.
fn blocks_exit(status: SessionStatus, is_abandoned: bool) -> bool {
    !is_abandoned && !matches!(status, SessionStatus::Stopped | SessionStatus::Error)
}

/// True when no client is attached and no session blocks the exit. Reads the
/// latest client count (marking it seen, so a subsequent `changed()` only
/// wakes on a genuinely new value) and sweeps the full registry — the watcher
/// never trusts event payloads, only current state.
fn is_idle(client_count: &mut watch::Receiver<usize>, sessions: &SessionRegistry) -> bool {
    *client_count.borrow_and_update() == 0
        && !sessions
            .snapshots()
            .iter()
            .any(|s| blocks_exit(s.status, s.is_abandoned))
}

/// Block until the client count or any session changes. Returns `false` when
/// a channel is gone (daemon teardown) — the watcher should exit. A `Lagged`
/// session-event receiver is fine: the caller re-derives state from registry
/// snapshots, so dropped events only mean a spurious wake-up.
async fn wait_for_change(
    client_count: &mut watch::Receiver<usize>,
    session_events: &mut broadcast::Receiver<SessionEvent>,
) -> bool {
    tokio::select! {
        res = client_count.changed() => res.is_ok(),
        evt = session_events.recv() => !matches!(evt, Err(RecvError::Closed)),
    }
}

enum GraceOutcome {
    /// The idle state held for the whole grace period.
    Expired,
    /// A client attached or a session came alive before the timer fired.
    ActivityResumed,
    /// An input channel closed — the daemon is tearing down already.
    Closed,
}

/// Wait out the grace period, bailing early if the idle condition breaks.
async fn idle_held_for(
    grace: Duration,
    client_count: &mut watch::Receiver<usize>,
    session_events: &mut broadcast::Receiver<SessionEvent>,
    sessions: &SessionRegistry,
) -> GraceOutcome {
    let deadline = tokio::time::sleep(grace);
    tokio::pin!(deadline);
    loop {
        let alive = tokio::select! {
            () = &mut deadline => return GraceOutcome::Expired,
            res = client_count.changed() => res.is_ok(),
            evt = session_events.recv() => !matches!(evt, Err(RecvError::Closed)),
        };
        if !alive {
            return GraceOutcome::Closed;
        }
        if !is_idle(client_count, sessions) {
            return GraceOutcome::ActivityResumed;
        }
    }
}

async fn run(
    sessions: Arc<SessionRegistry>,
    mut client_count: watch::Receiver<usize>,
    shutdown_tx: watch::Sender<bool>,
    preset_jobs: PresetJobs,
    grace: Duration,
) {
    let mut session_events = sessions.subscribe();
    loop {
        if !is_idle(&mut client_count, &sessions) {
            if !wait_for_change(&mut client_count, &mut session_events).await {
                return;
            }
            continue;
        }
        info!(
            grace_secs = grace.as_secs(),
            "idle-exit: no clients and no live sessions; grace timer started"
        );
        match idle_held_for(grace, &mut client_count, &mut session_events, &sessions).await {
            GraceOutcome::ActivityResumed => {
                info!("idle-exit: activity resumed; grace timer cancelled");
            }
            GraceOutcome::Closed => return,
            GraceOutcome::Expired => {
                if !preset_jobs.lock().await.is_empty() {
                    info!("idle-exit: preset launch still in flight; grace timer restarted");
                    continue;
                }
                // The preset-lock await above could have raced a spawn.
                if !is_idle(&mut client_count, &sessions) {
                    continue;
                }
                info!(
                    grace_secs = grace.as_secs(),
                    "idle-exit: idle for the full grace period; requesting daemon shutdown"
                );
                let _ = shutdown_tx.send(true);
                return;
            }
        }
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
    use tokio::time::timeout;

    fn scratch_dirs(tag: &str) -> Dirs {
        let root = std::env::temp_dir().join(format!("rt-idle-exit-{}-{tag}", std::process::id()));
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
        }
    }

    struct Harness {
        sessions: Arc<SessionRegistry>,
        count_tx: watch::Sender<usize>,
        shutdown_rx: watch::Receiver<bool>,
        preset_jobs: PresetJobs,
    }

    const GRACE: Duration = Duration::from_secs(30);

    fn start(tag: &str, initial_clients: usize) -> Harness {
        let sessions = SessionRegistry::new(scratch_dirs(tag));
        let (count_tx, count_rx) = watch::channel(initial_clients);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let preset_jobs: PresetJobs = Arc::new(AsyncMutex::new(HashMap::new()));
        spawn(
            Arc::clone(&sessions),
            count_rx,
            shutdown_tx,
            Arc::clone(&preset_jobs),
            GRACE,
        );
        Harness {
            sessions,
            count_tx,
            shutdown_rx,
            preset_jobs,
        }
    }

    async fn assert_shutdown_within(h: &mut Harness, window: Duration) {
        timeout(window, h.shutdown_rx.changed())
            .await
            .expect("watcher should have requested shutdown within the window")
            .expect("shutdown channel closed unexpectedly");
        assert!(*h.shutdown_rx.borrow());
    }

    async fn assert_no_shutdown_for(h: &mut Harness, window: Duration) {
        assert!(
            timeout(window, h.shutdown_rx.changed()).await.is_err(),
            "watcher must not request shutdown while activity blocks it"
        );
    }

    #[test]
    fn live_statuses_block_exit_terminal_and_abandoned_do_not() {
        for status in [
            SessionStatus::Spawning,
            SessionStatus::Idle,
            SessionStatus::Working,
            SessionStatus::AwaitingInput,
        ] {
            assert!(blocks_exit(status, false), "{status:?} must block exit");
        }
        assert!(!blocks_exit(SessionStatus::Stopped, false));
        assert!(!blocks_exit(SessionStatus::Error, false));
        // Abandoned sessions are rebuilt from sidecars on the next start.
        assert!(!blocks_exit(SessionStatus::Working, true));
    }

    #[tokio::test(start_paused = true)]
    async fn exits_after_grace_when_no_clients_and_no_sessions() {
        let mut h = start("empty", 0);
        assert_shutdown_within(&mut h, GRACE * 2).await;
    }

    #[tokio::test(start_paused = true)]
    async fn attached_client_blocks_exit_until_disconnect() {
        let mut h = start("client", 1);
        assert_no_shutdown_for(&mut h, GRACE * 4).await;
        h.count_tx.send(0).expect("watcher holds the receiver");
        assert_shutdown_within(&mut h, GRACE * 2).await;
    }

    #[tokio::test(start_paused = true)]
    async fn live_session_blocks_exit_until_it_stops() {
        let mut h = start("session", 0);
        h.sessions
            .insert(test_record("s1", SessionStatus::Idle, false));
        assert_no_shutdown_for(&mut h, GRACE * 4).await;
        h.sessions
            .update("s1", |r| r.status = SessionStatus::Stopped);
        assert_shutdown_within(&mut h, GRACE * 2).await;
    }

    #[tokio::test(start_paused = true)]
    async fn abandoned_session_does_not_block_exit() {
        let mut h = start("abandoned", 0);
        h.sessions
            .insert(test_record("s1", SessionStatus::Working, true));
        assert_shutdown_within(&mut h, GRACE * 2).await;
    }

    #[tokio::test(start_paused = true)]
    async fn preset_launch_in_flight_defers_exit() {
        let mut h = start("preset", 0);
        {
            let (job_tx, _job_rx) = watch::channel(false);
            h.preset_jobs
                .lock()
                .await
                .insert("job-1".to_string(), job_tx);
        }
        assert_no_shutdown_for(&mut h, GRACE * 4).await;
        h.preset_jobs.lock().await.remove("job-1");
        // The watcher only re-checks the job map at grace expiry, so allow a
        // couple of cycles.
        assert_shutdown_within(&mut h, GRACE * 3).await;
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_during_grace_cancels_the_timer() {
        let mut h = start("reconnect", 1);
        h.count_tx.send(0).expect("watcher holds the receiver");
        // Reconnect halfway through the grace period.
        tokio::time::sleep(GRACE / 2).await;
        h.count_tx.send(1).expect("watcher holds the receiver");
        assert_no_shutdown_for(&mut h, GRACE * 4).await;
        h.count_tx.send(0).expect("watcher holds the receiver");
        assert_shutdown_within(&mut h, GRACE * 2).await;
    }
}
