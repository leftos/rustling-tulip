//! Subagent tails: one subagent's transcript followed read-only by the
//! connections that sent `FollowTranscript` for it.
//!
//! One tail runs per `(owner, agent id)` while that key has a follower; an
//! owner is a session or a dispatch run. Every follower operation, every tail
//! event and every removal runs under one tails lock, a leaf: the record's
//! fields are copied out and its lock released before it is taken, and
//! nothing awaits under it. A follow joins the running tail or starts one from
//! byte 0; the last unfollow, or the last following connection closing, drops
//! the entry and with it the tail. Removing the session from the registry, or
//! the run from the run list, ends each of its tails, sending
//! `TranscriptAppend { ended: true }`; a shutdown ends none.

#[cfg(test)]
#[path = "tail_tests.rs"]
pub(crate) mod tests;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use agent_api::AgentPlugin;
use protocol::{AgentId, DaemonMessage, TranscriptSource, error_codes};
use tokio::sync::{broadcast, watch};
use tracing::{debug, warn};

use crate::dispatch_runs::{DispatchRunEvent, DispatchRuns};
use crate::session::{SessionEvent, SessionRegistry};
use crate::sync::lock;
use crate::transcript_log::{ConnectionId, Outbox, TranscriptLog};
use crate::transcript_tail::{self, TailEvent, TailHandle};

/// Whose subagent a tail follows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Owner {
    /// A session's, by session id.
    Session(String),
    /// A dispatch run's, by run id.
    Run(String),
}

impl Owner {
    /// The session or run id the owner names.
    fn id(&self) -> &str {
        match self {
            Owner::Session(id) | Owner::Run(id) => id,
        }
    }
}

/// A followed subagent: its owner and its agent id.
pub(crate) type Key = (Owner, String);

type Map = HashMap<Key, Entry>;

/// A spec's hook, run once between a follow's record read and its tails lock.
#[cfg(test)]
type LockGap = Box<dyn FnOnce() + Send>;

/// The next generation a started tail carries, unique within the daemon's
/// run, so a stopped tail's late event never lands in a newer entry.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// One followed subagent's tail, log and followers.
struct Entry {
    log: TranscriptLog,
    /// Stops the tail when the entry drops.
    _tail: TailHandle,
    plugin: &'static dyn AgentPlugin,
    /// The transcript the tail reads; a join never swaps it.
    path: PathBuf,
    /// The generation the tail's events carry.
    generation: u64,
    /// Whether the tail's first read has ended.
    caught_up: bool,
    /// The first read's lines, folded at once when it ends.
    buffer: Vec<String>,
}

/// The subagent tails, shared by the hub, the connections and the removal
/// watcher.
#[derive(Clone)]
pub(crate) struct SubagentTails {
    tails: Arc<Mutex<Map>>,
    registry: Arc<SessionRegistry>,
    client_count: Arc<watch::Sender<usize>>,
    /// The listed runs, whose own subagents a run follow reads; `None` when
    /// the daemon follows no live-run folder.
    runs: Option<Arc<DispatchRuns>>,
    /// A spec's hook, run between the record read and the tails lock.
    #[cfg(test)]
    lock_gap: Arc<Mutex<Option<LockGap>>>,
}

/// Unfollows its connection from every subagent tail when it drops.
pub(crate) struct SubagentFollowGuard {
    tails: SubagentTails,
    connection: ConnectionId,
}

impl Drop for SubagentFollowGuard {
    fn drop(&mut self) {
        self.tails.unfollow_all(self.connection);
    }
}

impl SubagentTails {
    /// The tails over `registry`'s sessions and `runs`' runs; each tail parks
    /// while `client_count` is 0.
    pub(crate) fn new(
        registry: Arc<SessionRegistry>,
        client_count: Arc<watch::Sender<usize>>,
        runs: Option<Arc<DispatchRuns>>,
    ) -> Self {
        Self {
            tails: Arc::default(),
            registry,
            client_count,
            runs,
            #[cfg(test)]
            lock_gap: Arc::default(),
        }
    }

    /// Make `connection` follow subagent `key`, sending the backfill
    /// answering `request_id` on `outbox`, or a `NOT_FOUND` `Error` when the
    /// owner is not held or its plugin names no transcript for the agent.
    pub(crate) fn follow(
        &self,
        key: &Key,
        request_id: Option<String>,
        connection: ConnectionId,
        outbox: &Outbox,
    ) {
        let Some((path, plugin)) = self.resolve(key) else {
            let message = format!("unknown subagent: {}/{}", key.0.id(), key.1);
            let _ = outbox.send(DaemonMessage::Error {
                message,
                request_id,
                code: Some(error_codes::NOT_FOUND.to_owned()),
            });
            return;
        };
        self.run_lock_gap();
        {
            let mut map = lock(&self.tails);
            let entry = match map.entry(key.clone()) {
                std::collections::hash_map::Entry::Occupied(held) => held.into_mut(),
                std::collections::hash_map::Entry::Vacant(free) => {
                    free.insert(self.start(key, path, plugin))
                }
            };
            entry
                .log
                .follow(connection, outbox.clone(), &source_of(key), request_id);
            sweep(&mut map);
        }
        // A removal between the record read and the lock found no entry to
        // end: end it now.
        if self.owner_gone(&key.0) {
            end_where(&self.tails, |held| held == key);
        }
    }

    /// Stop sending `key`'s transcript to `connection`, dropping each entry
    /// left with no follower.
    pub(crate) fn unfollow(&self, key: &Key, connection: ConnectionId) {
        let mut map = lock(&self.tails);
        if let Some(entry) = map.get_mut(key) {
            entry.log.unfollow(connection);
        }
        sweep(&mut map);
    }

    /// Stop sending every transcript to `connection`, dropping each entry
    /// left with no follower.
    fn unfollow_all(&self, connection: ConnectionId) {
        let mut map = lock(&self.tails);
        for entry in map.values_mut() {
            entry.log.unfollow(connection);
        }
        sweep(&mut map);
    }

    /// A guard that unfollows `connection` everywhere when it drops.
    #[must_use]
    pub(crate) fn follow_guard(&self, connection: ConnectionId) -> SubagentFollowGuard {
        SubagentFollowGuard {
            tails: self.clone(),
            connection,
        }
    }

    /// Start the tasks that end a removed session's tails and a removed run's,
    /// for the daemon's life. Each subscribes before returning, so a removal
    /// after this call is seen.
    pub(crate) fn spawn_removal_watcher(&self) -> tokio::task::JoinHandle<()> {
        let receiver = self.registry.subscribe();
        if let Some(runs) = &self.runs {
            let run_events = runs.subscribe();
            tokio::spawn(follow_run_removals(self.clone(), run_events));
        }
        tokio::spawn(follow_removals(self.clone(), receiver))
    }

    /// The transcript the plugin keeps for `key`'s agent, and the plugin; the
    /// record or the run list is read and released on return.
    fn resolve(&self, key: &Key) -> Option<(PathBuf, &'static dyn AgentPlugin)> {
        match &key.0 {
            Owner::Session(session_id) => self.resolve_session(session_id, &key.1),
            Owner::Run(run_id) => self.resolve_run(run_id, &key.1),
        }
    }

    /// The transcript of `agent_id` in the session `session_id`'s record.
    fn resolve_session(
        &self,
        session_id: &str,
        agent_id: &str,
    ) -> Option<(PathBuf, &'static dyn AgentPlugin)> {
        let (agent, conversation_id, folder, extra_env) = {
            let record = self.registry.get(session_id)?;
            let record = lock(&record);
            (
                record.agent.clone(),
                record.conversation_id.clone()?,
                crate::history::primary_cwd(&record)?,
                record
                    .spawn_config
                    .as_ref()
                    .map(|config| config.extra_env.clone())
                    .unwrap_or_default(),
            )
        };
        transcript_path(
            &agent,
            Path::new(&folder),
            &conversation_id,
            agent_id,
            &extra_env,
        )
    }

    /// The transcript of `agent_id` in the run `run_id`'s entry, read with the
    /// env rows of the session that dispatched it.
    fn resolve_run(
        &self,
        run_id: &str,
        agent_id: &str,
    ) -> Option<(PathBuf, &'static dyn AgentPlugin)> {
        let runs = self.runs.as_ref()?;
        let source = runs.subagent_source(run_id)?;
        let extra_env = crate::subagents::host_env(
            &self.registry,
            &self.registry.snapshots(),
            source.session_id.as_deref(),
        );
        transcript_path(
            &source.agent,
            &source.worktree,
            &source.agent_session_id,
            agent_id,
            &extra_env,
        )
    }

    /// Whether `owner` is gone: its session removed from the registry, or its
    /// run no longer listed.
    fn owner_gone(&self, owner: &Owner) -> bool {
        match owner {
            Owner::Session(session_id) => self.registry.get(session_id).is_none(),
            Owner::Run(run_id) => self
                .runs
                .as_ref()
                .is_none_or(|runs| !runs.is_listed(run_id)),
        }
    }

    /// A fresh entry for `key`, tailing `path` from its start.
    fn start(&self, key: &Key, path: PathBuf, plugin: &'static dyn AgentPlugin) -> Entry {
        let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
        debug!(owner = ?key.0, agent_id = %key.1, path = %path.display(), generation, "subagent tail: started");
        let weak: Weak<Mutex<Map>> = Arc::downgrade(&self.tails);
        let event_key = key.clone();
        let tail = transcript_tail::spawn(
            path.clone(),
            0,
            self.client_count.subscribe(),
            move |event| {
                if let Some(tails) = weak.upgrade() {
                    on_event(&tails, &event_key, generation, event);
                }
            },
        );
        Entry {
            log: TranscriptLog::default(),
            _tail: tail,
            plugin,
            path,
            generation,
            caught_up: false,
            buffer: Vec::new(),
        }
    }

    /// End and drop every tail whose owner is gone, after removals were
    /// lost. The registry and the run list are read outside the tails lock.
    fn end_missing(&self) {
        let owners: HashSet<Owner> = lock(&self.tails)
            .keys()
            .map(|(owner, _)| owner.clone())
            .collect();
        let gone: HashSet<Owner> = owners
            .into_iter()
            .filter(|owner| self.owner_gone(owner))
            .collect();
        if !gone.is_empty() {
            end_where(&self.tails, |(owner, _)| gone.contains(owner));
        }
    }

    #[cfg(test)]
    fn run_lock_gap(&self) {
        let hook = lock(&self.lock_gap).take();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Nothing to run in a build without the specs' hook.
    #[cfg(not(test))]
    #[expect(
        clippy::unused_self,
        reason = "the test build's twin reads the hook on self"
    )]
    fn run_lock_gap(&self) {}

    /// Run `hook` in the next follow, after it read the session's record and
    /// before it takes the tails lock.
    #[cfg(test)]
    pub(crate) fn before_tail_lock(&self, hook: impl FnOnce() + Send + 'static) {
        *lock(&self.lock_gap) = Some(Box::new(hook));
    }

    /// How many subagents are tailed.
    #[cfg(test)]
    pub(crate) fn tail_count(&self) -> usize {
        lock(&self.tails).len()
    }

    /// The generation `key`'s tail carries, while it is tailed.
    #[cfg(test)]
    pub(crate) fn generation(&self, key: &Key) -> Option<u64> {
        lock(&self.tails).get(key).map(|entry| entry.generation)
    }

    /// Deliver `event` as a tail of `key` started on `generation` would.
    #[cfg(test)]
    pub(crate) fn deliver(&self, key: &Key, generation: u64, event: TailEvent) {
        on_event(&self.tails, key, generation, event);
    }
}

/// The transcript `agent`'s plugin keeps in `folder` for `conversation_id`,
/// with the env rows `extra_env` names.
fn transcript_path(
    agent: &AgentId,
    folder: &Path,
    conversation_id: &str,
    agent_id: &str,
    extra_env: &[(String, String)],
) -> Option<(PathBuf, &'static dyn AgentPlugin)> {
    let plugin = agent_registry::plugin(agent)?;
    let rows = crate::conversations::lookup_rows(extra_env);
    let path = plugin.subagent_transcript_path(folder, conversation_id, agent_id, &rows)?;
    Some((path, plugin))
}

/// The wire source of subagent `key`.
fn source_of(key: &Key) -> TranscriptSource {
    let (owner, agent_id) = key;
    match owner {
        Owner::Session(session_id) => TranscriptSource::Subagent {
            session_id: session_id.clone(),
            agent_id: agent_id.clone(),
        },
        Owner::Run(run_id) => TranscriptSource::RunSubagent {
            run_id: run_id.clone(),
            agent_id: agent_id.clone(),
        },
    }
}

/// Drop every entry left with no follower, which stops its tail.
fn sweep(map: &mut Map) {
    map.retain(|key, entry| {
        let keep = entry.log.has_followers();
        if !keep {
            debug!(owner = ?key.0, agent_id = %key.1, path = %entry.path.display(), "subagent tail: stopped");
        }
        keep
    });
}

/// Tell the followers of every entry whose key `ends` that the transcript
/// ended, and drop those entries; nothing for a key no longer held.
fn end_where(tails: &Mutex<Map>, ends: impl Fn(&Key) -> bool) {
    let mut map = lock(tails);
    map.retain(|key, entry| {
        if !ends(key) {
            return true;
        }
        entry.log.end(&source_of(key));
        debug!(owner = ?key.0, agent_id = %key.1, path = %entry.path.display(), "subagent tail: ended");
        false
    });
}

/// Apply one tail event to `key`'s entry, while it is still on `generation`.
/// The first read's lines fold in one append when it ends; each later line
/// folds on its own. A blank line yields nothing.
fn on_event(tails: &Mutex<Map>, key: &Key, generation: u64, event: TailEvent) {
    let mut map = lock(tails);
    let Some(entry) = map
        .get_mut(key)
        .filter(|entry| entry.generation == generation)
    else {
        return;
    };
    let source = source_of(key);
    let plugin = entry.plugin;
    match event {
        TailEvent::Line(line) if line.trim().is_empty() => {}
        TailEvent::Line(line) if entry.caught_up => {
            entry.log.fold(&source, false, &[line], plugin);
        }
        TailEvent::Line(line) => entry.buffer.push(line),
        TailEvent::CaughtUp if !entry.caught_up => {
            entry.caught_up = true;
            let lines = std::mem::take(&mut entry.buffer);
            entry.log.fold(&source, false, &lines, plugin);
        }
        TailEvent::CaughtUp => {}
        TailEvent::Restarted => {
            entry.buffer.clear();
            entry.log.restarted(&source);
        }
    }
}

/// End the tails of each session the registry removes, until the registry
/// closes; lost events end every tail whose session is gone.
async fn follow_removals(tails: SubagentTails, mut receiver: broadcast::Receiver<SessionEvent>) {
    loop {
        match receiver.recv().await {
            Ok(SessionEvent::Removed(session_id)) => {
                end_where(&tails.tails, |(owner, _)| {
                    *owner == Owner::Session(session_id.clone())
                });
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    skipped,
                    "subagent tails: session events lagged; ending the tails of removed sessions"
                );
                // Subscribe before reading: a removal after the sweep is seen.
                receiver = tails.registry.subscribe();
                tails.end_missing();
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// End the tails of each run the run list removes, until the run list closes;
/// lost events end every tail whose run is no longer listed.
async fn follow_run_removals(
    tails: SubagentTails,
    mut receiver: broadcast::Receiver<DispatchRunEvent>,
) {
    loop {
        match receiver.recv().await {
            Ok(DispatchRunEvent::Removed(run_id)) => {
                end_where(&tails.tails, |(owner, _)| {
                    *owner == Owner::Run(run_id.clone())
                });
            }
            Ok(DispatchRunEvent::Updated(_)) => {}
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    skipped,
                    "subagent tails: run events lagged; ending the tails of removed runs"
                );
                // Subscribe before reading: a removal after the sweep is seen.
                let Some(runs) = tails.runs.as_ref() else {
                    return;
                };
                receiver = runs.subscribe();
                tails.end_missing();
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}
