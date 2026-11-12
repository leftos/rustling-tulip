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
//!
//! The list follows the child alone: a run whose child is dead leaves it at
//! the next liveness tick or scan, and is not listed again while the daemon runs. The files follow
//! the runner: the daemon deletes a dead child's live-run file only once the
//! file's runner is dead or not recorded, since a runner that may be alive
//! still reads the run's stop marker, which the scan deletes only once no
//! live-run file is beside it.

pub(crate) mod guard;
mod links;
mod parse;
mod tail;
#[cfg(test)]
mod tests;
mod watch;

use crate::session::SessionRegistry;
use crate::stream_state::StreamState;
use agent_api::HeadlessEvent;
use chrono::{DateTime, Utc};
pub(crate) use links::DispatchLinks;
use parse::LiveRunFile;
use protocol::{DaemonMessage, DispatchRunSnapshot, DispatchRunStatus, SessionStatus};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// The broadcast channel's capacity; a receiver further behind lags.
pub(crate) const EVENT_CAPACITY: usize = 256;

/// What the server holds of the live runs it follows: the list and its
/// change feed, and the live-run folder a Stop writes its marker into.
#[derive(Clone)]
pub(crate) struct DispatchRunsHub {
    /// The listed runs.
    pub runs: Arc<DispatchRuns>,
    /// The live-run folder the watcher follows.
    pub folder: PathBuf,
}

/// What [`DispatchRuns::mark_stopping`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkStopping {
    /// The run is now stopping, and its `Updated` was sent.
    Marked,
    /// The run was already stopping; nothing changed.
    AlreadyStopping,
    /// The run is not listed, or is listed again under another generation.
    Gone,
}

/// What a Stop reads of one listed run, under the lock.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StopTarget {
    /// The listing the Stop acts on.
    pub generation: u64,
    /// The run's child, whose tree a Stop kills.
    pub child_pid: u32,
    /// The child's recorded creation time, when the tool recorded one.
    pub child_start: Option<u64>,
    /// When the file says the child started.
    pub started_at: DateTime<Utc>,
    /// Whether a Stop already marked the run.
    pub stopping: bool,
}

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
    /// A Stop marked the run; its status shows `Stopping` until it goes.
    stopping: bool,
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
            status: if self.stopping {
                DispatchRunStatus::Stopping
            } else {
                status_of(self.stream.status)
            },
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

/// Every run in `map`, oldest first.
fn sorted_snapshots(map: &RunMap) -> Vec<DispatchRunSnapshot> {
    let mut snapshots: Vec<DispatchRunSnapshot> = map
        .entries
        .iter()
        .map(|(run_id, entry)| entry.snapshot(run_id))
        .collect();
    snapshots.sort_by_key(|snapshot| snapshot.started_at);
    snapshots
}

/// What the liveness tick checks of one run, read under the lock.
struct GuardTarget {
    run_id: String,
    generation: u64,
    child_pid: u32,
    child_start: Option<u64>,
    /// The dispatch tool's process that waits on the child, when recorded.
    runner_pid: Option<u32>,
    /// The runner's recorded creation time, when the tool recorded one.
    runner_start: Option<u64>,
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
    /// The sessions whose conversations the runs link, set once by
    /// [`Self::attach_sessions`] under the runs lock.
    sessions: OnceLock<Arc<SessionRegistry>>,
}

impl DispatchRuns {
    pub(crate) fn new() -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Arc::new(Self {
            runs: Mutex::new(RunMap::default()),
            events,
            sessions: OnceLock::new(),
        })
    }

    /// Count every listed run, and every run listed from now on, as
    /// background work of the sessions on the conversation its `session_id`
    /// names, and send those sessions' updates. A second call does nothing.
    pub(crate) fn attach_sessions(&self, sessions: Arc<SessionRegistry>) {
        let map = self.lock();
        if self.sessions.set(sessions).is_err() {
            return;
        }
        let mut flipped = Vec::new();
        for entry in map.entries.values() {
            flipped.extend(self.relink(entry.file.session_id.as_deref(), None));
        }
        drop(map);
        self.notify(&flipped);
    }

    /// Link the conversation `added` names and unlink the one `removed`
    /// names, returning the ids whose link came or went. Nothing before the
    /// sessions are attached. Called under the runs lock; the links lock is
    /// a leaf, so this takes no other lock.
    fn relink(&self, added: Option<&str>, removed: Option<&str>) -> Vec<String> {
        let Some(sessions) = self.sessions.get() else {
            return Vec::new();
        };
        if added == removed {
            return Vec::new();
        }
        let links = sessions.dispatch_links();
        let mut flipped = Vec::new();
        if let Some(id) = added
            && links.link(id)
        {
            flipped.push(id.to_owned());
        }
        if let Some(id) = removed
            && links.unlink(id)
        {
            flipped.push(id.to_owned());
        }
        flipped
    }

    /// Send an update of every session on each of `flipped`'s conversations.
    /// Called with neither the runs lock nor the links lock held.
    fn notify(&self, flipped: &[String]) {
        let Some(sessions) = self.sessions.get() else {
            return;
        };
        for conversation_id in flipped {
            sessions.notify_conversation(conversation_id);
        }
    }

    /// Every listed run, oldest first.
    pub(crate) fn snapshots(&self) -> Vec<DispatchRunSnapshot> {
        sorted_snapshots(&self.lock())
    }

    /// Send the whole run list on `out_tx` while holding the runs lock, so no
    /// change is broadcast between the list's read and its send: a connection
    /// whose forwarder subscribed first never sees a change land before the
    /// list it already applies to.
    pub(crate) fn send_list(&self, out_tx: &mpsc::UnboundedSender<DaemonMessage>) {
        let map = self.lock();
        let _ = out_tx.send(DaemonMessage::DispatchRuns {
            runs: sorted_snapshots(&map),
        });
        drop(map);
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
                runner_pid: entry.file.runner_pid,
                runner_start: entry.file.runner_start,
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
            stopping: false,
        };
        let snapshot = entry.snapshot(run_id);
        let added = entry.file.session_id.clone();
        let old = map.entries.insert(run_id.to_owned(), entry);
        if let Some(old) = &old {
            old.cancel.cancel();
        }
        let removed = old.and_then(|old| old.file.session_id);
        let flipped = self.relink(added.as_deref(), removed.as_deref());
        self.send(DispatchRunEvent::Updated(snapshot));
        drop(map);
        self.notify(&flipped);
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
        let flipped = self.relink(file.session_id.as_deref(), entry.file.session_id.as_deref());
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
        drop(map);
        self.notify(&flipped);
        restart.then_some(next_generation)
    }

    /// Remove a listed run, stop its tailer and send `Removed`. A run id that
    /// is not listed sends nothing, so each listing is removed once.
    fn remove(&self, run_id: &str) -> bool {
        let mut map = self.lock();
        let Some(entry) = map.entries.remove(run_id) else {
            return false;
        };
        self.removed(map, run_id, &entry);
        true
    }

    /// Finish removing `entry`, already taken out of `map`: stop its tailer,
    /// unlink its conversation and send `Removed`, then release the lock and
    /// send the sessions' updates.
    fn removed(&self, map: MutexGuard<'_, RunMap>, run_id: &str, entry: &RunEntry) {
        entry.cancel.cancel();
        let flipped = self.relink(None, entry.file.session_id.as_deref());
        self.send(DispatchRunEvent::Removed(run_id.to_owned()));
        drop(map);
        self.notify(&flipped);
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
        self.removed(map, run_id, &entry);
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

    /// What a Stop needs of the listed run `run_id`; `None` when it is not
    /// listed.
    pub(crate) fn stop_target(&self, run_id: &str) -> Option<StopTarget> {
        self.lock().entries.get(run_id).map(|entry| StopTarget {
            generation: entry.generation,
            child_pid: entry.file.child_pid,
            child_start: entry.file.child_start,
            started_at: entry.file.started_at,
            stopping: entry.stopping,
        })
    }

    /// Mark the run listed at `generation` as stopping and send its
    /// `Updated`, unless it is already stopping or no longer that listing.
    /// Two Stops racing here see one `Marked`.
    pub(crate) fn mark_stopping(&self, run_id: &str, generation: u64) -> MarkStopping {
        self.set_stopping(run_id, generation, true)
    }

    /// Clear the stopping mark of the run listed at `generation` (its kill
    /// failed) and send its `Updated`; nothing when it is not marked.
    pub(crate) fn clear_stopping(&self, run_id: &str, generation: u64) {
        let _ = self.set_stopping(run_id, generation, false);
    }

    /// Set the run's stopping mark to `stopping` under the lock, sending its
    /// `Updated` when the mark changed.
    fn set_stopping(&self, run_id: &str, generation: u64, stopping: bool) -> MarkStopping {
        let mut map = self.lock();
        let Some(entry) = map
            .entries
            .get_mut(run_id)
            .filter(|entry| entry.generation == generation)
        else {
            return MarkStopping::Gone;
        };
        if entry.stopping == stopping {
            return MarkStopping::AlreadyStopping;
        }
        entry.stopping = stopping;
        let snapshot = entry.snapshot(run_id);
        self.send(DispatchRunEvent::Updated(snapshot));
        MarkStopping::Marked
    }

    /// List `run_id` as a run of `child_pid` without a watcher, for the
    /// server's tests; returns its generation.
    #[cfg(test)]
    pub(crate) fn list_for_test(
        &self,
        run_id: &str,
        child_pid: u32,
        child_start: Option<u64>,
        started_at: DateTime<Utc>,
    ) -> u64 {
        let file = test_file(run_id, child_pid, child_start, started_at);
        self.insert(run_id, file, None, CancellationToken::new())
    }

    /// [`Self::list_for_test`] for a run whose file names the conversation
    /// `session_id`.
    #[cfg(test)]
    pub(crate) fn list_linked_for_test(&self, run_id: &str, session_id: Option<&str>) -> u64 {
        let mut file = test_file(run_id, 4242, Some(7), Utc::now());
        file.session_id = session_id.map(str::to_owned);
        self.insert(run_id, file, None, CancellationToken::new())
    }
}

/// The live-run file of a run of `child_pid` that names no conversation.
#[cfg(test)]
fn test_file(
    run_id: &str,
    child_pid: u32,
    child_start: Option<u64>,
    started_at: DateTime<Utc>,
) -> LiveRunFile {
    LiveRunFile {
        schema: parse::SCHEMA,
        run_id: run_id.to_owned(),
        title: "a run".to_owned(),
        backend: "a backend".to_owned(),
        tier: None,
        session_id: None,
        agent: None,
        worktree: PathBuf::from("worktree"),
        run_dir: PathBuf::from("run"),
        stream: PathBuf::from("stream.jsonl"),
        child_pid,
        child_start,
        runner_pid: None,
        runner_start: None,
        started_at,
        resumed_from: None,
        auto_resume: false,
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
