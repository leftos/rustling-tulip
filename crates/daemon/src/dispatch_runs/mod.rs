//! The dispatch tool's live runs: the live-run file format, and the watcher,
//! tailer and liveness check that keep the daemon's list of them
//! (`docs/plans/dispatch-follow.md` sections 3, 6, 9 and 10).
//!
//! One watcher task owns every listing and removal: debounced file events,
//! a full scan at startup and every [`Timing::scan`], and the liveness tick.
//! Each listed run has one tailer task that folds its stream into a
//! [`StreamState`]. The runs map's lock is never held across file I/O or an
//! await: callers read first, then lock, update and send, then unlock. Every
//! `Updated` and `Removed` is sent while the lock is held (`broadcast::send`
//! never blocks or awaits), so receivers see them in the map's own order: no
//! `Updated` lands after its run's `Removed`, and no older fold overtakes a
//! newer one.

// The runtime tests are Windows-only, so a test build elsewhere leaves the
// runtime unused too, until DF.6 starts it from the server.
#![cfg_attr(
    any(not(test), not(windows)),
    expect(dead_code, reason = "started by the server in DF.6")
)]

pub(crate) mod guard;
mod parse;
mod tail;
#[cfg(test)]
mod tests;
mod watch;

use crate::stream_state::StreamState;
use agent_api::HeadlessEvent;
use chrono::{DateTime, Utc};
use parse::LiveRunFile;
use protocol::{DispatchRunSnapshot, DispatchRunStatus, SessionStatus};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// The broadcast channel's capacity; a receiver further behind lags.
const EVENT_CAPACITY: usize = 256;

/// A change to the live-run list.
#[expect(
    clippy::large_enum_variant,
    reason = "a snapshot goes straight to every receiver; boxing it would allocate per event"
)]
#[derive(Debug, Clone)]
pub(crate) enum DispatchRunEvent {
    /// A run appeared, or its file or stream changed.
    Updated(DispatchRunSnapshot),
    /// A run went: its file was removed or its process died. Sent at most
    /// once per listed run.
    Removed(String),
}

/// The intervals the watcher and the tailers run on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    /// The full folder scan that covers missed events.
    pub scan: Duration,
    /// The process guard check of every listed run.
    pub liveness: Duration,
    /// Each tailer's poll of its stream file.
    pub tail: Duration,
    /// The file events' debounce.
    pub debounce: Duration,
}

impl Timing {
    /// The design's intervals (`docs/plans/dispatch-follow.md` section 6).
    #[cfg_attr(
        test,
        expect(
            dead_code,
            reason = "used by the server wiring (DF.6); tests use short intervals"
        )
    )]
    pub(crate) const DEFAULT: Self = Self {
        scan: Duration::from_secs(30),
        liveness: Duration::from_secs(2),
        tail: Duration::from_millis(250),
        debounce: Duration::from_millis(100),
    };
}

/// One listed run.
struct RunEntry {
    file: LiveRunFile,
    stream: StreamState,
    main_checkout: Option<PathBuf>,
    /// Stops the run's tailer.
    cancel: CancellationToken,
    /// Which listing of this run id this is, so a stale tailer's fold or a
    /// stale liveness verdict never touches a later listing.
    generation: u64,
}

impl RunEntry {
    fn snapshot(&self, run_id: &str) -> DispatchRunSnapshot {
        let file = &self.file;
        DispatchRunSnapshot {
            run_id: run_id.to_owned(),
            title: file.title.clone(),
            backend: file.backend.clone(),
            tier: file.tier,
            worktree: file.worktree.to_string_lossy().into_owned(),
            main_checkout: self
                .main_checkout
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            run_dir: file.run_dir.to_string_lossy().into_owned(),
            started_at: file.started_at,
            status: status_of(self.stream.status),
            metrics: self.stream.metrics.clone(),
            recent_actions: self.stream.recent_actions.clone(),
            agent_session_id: self.stream.agent_session_id.clone(),
            session_id: file.session_id.clone(),
            resumed_from: file.resumed_from.clone(),
            auto_resume: file.auto_resume,
        }
    }
}

/// A run's status from its stream's last status: `Starting` before the first,
/// `Ended` after a final one, `Working` otherwise.
fn status_of(stream: Option<SessionStatus>) -> DispatchRunStatus {
    match stream {
        None => DispatchRunStatus::Starting,
        Some(SessionStatus::Stopped | SessionStatus::Error) => DispatchRunStatus::Ended,
        Some(_) => DispatchRunStatus::Working,
    }
}

/// What the liveness tick checks of one run, read under the lock.
struct GuardTarget {
    run_id: String,
    generation: u64,
    child_pid: u32,
    child_start: Option<u64>,
    started_at: DateTime<Utc>,
}

/// The runs map and the counter of listings.
#[derive(Default)]
struct RunMap {
    entries: HashMap<String, RunEntry>,
    next_generation: u64,
}

/// The daemon's list of live dispatch runs and its change feed.
pub(crate) struct DispatchRuns {
    runs: Mutex<RunMap>,
    events: broadcast::Sender<DispatchRunEvent>,
}

impl DispatchRuns {
    pub(crate) fn new() -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Arc::new(Self {
            runs: Mutex::new(RunMap::default()),
            events,
        })
    }

    /// Every listed run, oldest first.
    pub(crate) fn snapshots(&self) -> Vec<DispatchRunSnapshot> {
        let mut snapshots: Vec<DispatchRunSnapshot> = self
            .lock()
            .entries
            .iter()
            .map(|(run_id, entry)| entry.snapshot(run_id))
            .collect();
        snapshots.sort_by_key(|snapshot| snapshot.started_at);
        snapshots
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<DispatchRunEvent> {
        self.events.subscribe()
    }

    fn lock(&self) -> MutexGuard<'_, RunMap> {
        self.runs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn send(&self, event: DispatchRunEvent) {
        // No receiver is fine: nobody is listening yet.
        let _ = self.events.send(event);
    }

    /// The listed run's file.
    fn listed_file(&self, run_id: &str) -> Option<LiveRunFile> {
        self.lock()
            .entries
            .get(run_id)
            .map(|entry| entry.file.clone())
    }

    fn run_ids(&self) -> Vec<String> {
        self.lock().entries.keys().cloned().collect()
    }

    fn guard_targets(&self) -> Vec<GuardTarget> {
        self.lock()
            .entries
            .iter()
            .map(|(run_id, entry)| GuardTarget {
                run_id: run_id.clone(),
                generation: entry.generation,
                child_pid: entry.file.child_pid,
                child_start: entry.file.child_start,
                started_at: entry.file.started_at,
            })
            .collect()
    }

    /// List a new run and send its first `Updated`, returning its generation.
    fn insert(
        &self,
        run_id: &str,
        file: LiveRunFile,
        main_checkout: Option<PathBuf>,
        cancel: CancellationToken,
    ) -> u64 {
        let mut map = self.lock();
        let generation = map.next_generation;
        map.next_generation += 1;
        let entry = RunEntry {
            file,
            stream: StreamState::default(),
            main_checkout,
            cancel,
            generation,
        };
        let snapshot = entry.snapshot(run_id);
        if let Some(old) = map.entries.insert(run_id.to_owned(), entry) {
            old.cancel.cancel();
        }
        self.send(DispatchRunEvent::Updated(snapshot));
        generation
    }

    /// Replace a listed run's file and send `Updated` when it changed. A new
    /// stream path or agent restarts the run's stream state: the old tailer
    /// is cancelled, `fresh` becomes the run's token, and the new generation
    /// is returned for the new tailer.
    fn replace(&self, run_id: &str, file: LiveRunFile, fresh: CancellationToken) -> Option<u64> {
        let mut map = self.lock();
        let next_generation = map.next_generation;
        let entry = map.entries.get_mut(run_id)?;
        if entry.file == file {
            return None;
        }
        let restart = entry.file.stream != file.stream || entry.file.agent != file.agent;
        entry.file = file;
        if restart {
            entry.cancel.cancel();
            entry.cancel = fresh;
            entry.stream = StreamState::default();
            entry.generation = next_generation;
        }
        let snapshot = entry.snapshot(run_id);
        if restart {
            map.next_generation += 1;
        }
        self.send(DispatchRunEvent::Updated(snapshot));
        restart.then_some(next_generation)
    }

    /// Remove a listed run, stop its tailer and send `Removed`. A run id that
    /// is not listed sends nothing, so each listing is removed once.
    fn remove(&self, run_id: &str) -> bool {
        let mut map = self.lock();
        let Some(entry) = map.entries.remove(run_id) else {
            return false;
        };
        entry.cancel.cancel();
        self.send(DispatchRunEvent::Removed(run_id.to_owned()));
        true
    }

    /// [`Self::remove`], only while the run is still the listing at
    /// `generation`.
    fn remove_generation(&self, run_id: &str, generation: u64) -> bool {
        let mut map = self.lock();
        let removed = match map.entries.get(run_id) {
            Some(entry) if entry.generation == generation => map.entries.remove(run_id),
            _ => None,
        };
        let Some(entry) = removed else {
            return false;
        };
        entry.cancel.cancel();
        self.send(DispatchRunEvent::Removed(run_id.to_owned()));
        true
    }

    /// Apply one tailer tick's events to the run listed at `generation` and
    /// send its `Updated`; a run removed or listed again meanwhile is left
    /// alone. `restart` (the stream shrank and is read again from its start)
    /// clears the run's stream state first, so the replay adds nothing twice.
    fn fold(&self, run_id: &str, generation: u64, restart: bool, events: Vec<HeadlessEvent>) {
        let mut map = self.lock();
        let Some(entry) = map
            .entries
            .get_mut(run_id)
            .filter(|entry| entry.generation == generation)
        else {
            return;
        };
        if restart {
            entry.stream = StreamState::default();
        }
        for event in events {
            entry.stream.apply(event);
        }
        let snapshot = entry.snapshot(run_id);
        self.send(DispatchRunEvent::Updated(snapshot));
    }
}

/// Create `folder` and watch it until `shutdown`: the startup scan, then file
/// events, scans every `timing.scan` and the liveness tick every
/// `timing.liveness`, with one tailer per listed run.
pub(crate) fn spawn(
    folder: PathBuf,
    runs: Arc<DispatchRuns>,
    timing: Timing,
    shutdown: CancellationToken,
) -> anyhow::Result<JoinHandle<()>> {
    use anyhow::Context as _;
    std::fs::create_dir_all(&folder)
        .with_context(|| format!("creating the live-run folder {}", folder.display()))?;
    watch::start(folder, runs, timing, shutdown)
}
