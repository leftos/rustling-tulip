//! The subagent list: each live agent session's subagents, read with the
//! agent plugin's transcript reader and kept on the session's record, whose
//! snapshot carries them to every client.
//!
//! One task runs for the daemon's life. While a client is connected it makes
//! a pass every [`TICK`], and one at once when a client returns; with none
//! connected it parks and reads nothing. A pass reads every live session with
//! a reader of its own, builds the session's list and writes it only when it
//! differs from the record's. A session that leaves the live set has its list
//! emptied. Each listed dispatch run whose stream named its conversation is
//! read the same way, its list stored on its run entry; one that ends has its
//! list emptied.
//!
//! A first-layer subagent is listed with its whole subtree while any of it
//! runs. One that ended stays listed while it ended after both the host's
//! newest typed prompt and the start of its agent process, up to the host's
//! [`SubagentListing::finished_cap`] most recently finished; with a
//! [`SubagentListing::keep_minutes`] window it stays that many minutes past its
//! finish instead of until the next typed prompt. With no typed prompt read,
//! only the process start and the cap apply.

pub(crate) mod tail;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_api::{ActivityReader, ActivitySnapshot, ActivitySource, SubagentNode};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use protocol::{
    AgentId, AgentState, SessionSnapshot, SessionStatus, SubagentListing, SubagentSnapshot,
};
use tokio::sync::watch;
use tracing::warn;

use crate::dashboard::ReaderFactory;
use crate::dispatch_runs::{DispatchRuns, RunSource};
use crate::session::{SessionRecord, SessionRegistry};
use crate::state::AppState;
use crate::sync::lock;

/// How often the lists are read while a client is connected.
pub(crate) const TICK: Duration = Duration::from_secs(2);

/// The subagent list service, shared by the hub and its tick task.
#[derive(Clone)]
pub(crate) struct SubagentList {
    inner: Arc<Inner>,
}

struct Inner {
    sessions: Arc<SessionRegistry>,
    /// The listed dispatch runs, whose own subagents are read too.
    runs: Option<Arc<DispatchRuns>>,
    /// The host settings the rule reads: its finished-subagent listing.
    state: Arc<AppState>,
    factory: ReaderFactory,
    /// Each live owner's reader between passes.
    readers: Mutex<HashMap<Owner, Cached>>,
}

/// Whose subagents a reader reads: a session, or one listing of a dispatch
/// run, so a session and a run never share a reader and a run listed again
/// gets a fresh one.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Owner {
    Session(String),
    Run { id: String, generation: u64 },
}

/// Which transcript a reader follows; a change needs a fresh reader.
#[derive(Clone, PartialEq, Eq)]
struct ReaderKey {
    conversation_id: String,
    folder: PathBuf,
    process_started_at: DateTime<Utc>,
}

struct Cached {
    key: ReaderKey,
    reader: Box<dyn ActivityReader>,
}

/// What a pass needs of one live session or run, copied out of its record or
/// its run entry.
struct Live {
    owner: Owner,
    agent: AgentId,
    key: ReaderKey,
    extra_env: Vec<(String, String)>,
}

/// One session's read, handed to the blocking pool and back.
struct Job {
    live: Live,
    reader: Option<Box<dyn ActivityReader>>,
    snapshot: Option<ActivitySnapshot>,
}

/// The times an ended subagent must have ended after to stay listed.
struct Window {
    process_started_at: DateTime<Utc>,
    last_typed_prompt_at: Option<DateTime<Utc>>,
}

impl SubagentList {
    /// The service over `sessions`, the dispatch runs `runs` lists, and the
    /// host settings `state` holds, reading with each agent plugin's reader.
    pub(crate) fn new(
        sessions: Arc<SessionRegistry>,
        runs: Option<Arc<DispatchRuns>>,
        state: Arc<AppState>,
    ) -> Self {
        Self::with_reader_factory(sessions, runs, state, Arc::new(plugin_reader))
    }

    /// The service over `sessions`, `runs` and `state`, reading with
    /// `factory`'s readers.
    pub(crate) fn with_reader_factory(
        sessions: Arc<SessionRegistry>,
        runs: Option<Arc<DispatchRuns>>,
        state: Arc<AppState>,
        factory: ReaderFactory,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                sessions,
                runs,
                state,
                factory,
                readers: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Start the tick task for the daemon's life: it parks while `clients`
    /// is 0, makes a pass as soon as it rises, then one every [`TICK`].
    pub(crate) fn spawn(&self, mut clients: watch::Receiver<usize>) -> tokio::task::JoinHandle<()> {
        let list = self.clone();
        tokio::spawn(async move {
            loop {
                if clients.wait_for(|count| *count > 0).await.is_err() {
                    return;
                }
                let mut interval = tokio::time::interval(TICK);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        biased;
                        changed = clients.changed() => {
                            if changed.is_err() {
                                return;
                            }
                            if *clients.borrow_and_update() == 0 {
                                break;
                            }
                        }
                        _ = interval.tick() => {
                            if *clients.borrow() == 0 {
                                break;
                            }
                            list.pass().await;
                        }
                    }
                }
            }
        })
    }

    /// One pass: read every live session and run on the blocking pool, write
    /// each one's list where it changed, and empty the list of every session
    /// that is no longer live and every run no longer read.
    pub(crate) async fn pass(&self) {
        let snapshots = self.inner.sessions.snapshots();
        let mut live = live_sessions(&self.inner.sessions, &snapshots);
        live.extend(self.live_runs(&snapshots));
        let jobs = self.take_jobs(live);
        let factory = Arc::clone(&self.inner.factory);
        let done = match tokio::task::spawn_blocking(move || run_jobs(jobs, &factory)).await {
            Ok(done) => done,
            Err(err) => {
                warn!(
                    ?err,
                    "subagents: the read pass failed; its readers start again next pass"
                );
                return;
            }
        };
        let live: HashSet<String> = done
            .iter()
            .filter_map(|job| match &job.live.owner {
                Owner::Session(id) => Some(id.clone()),
                Owner::Run { .. } => None,
            })
            .collect();
        // One listing and one clock for every owner in this pass, so a session
        // and a run never expire against two different reads of the setting.
        let listing = self.inner.state.subagent_listing();
        let now = Utc::now();
        for job in done {
            self.apply(job, listing, now);
        }
        for snapshot in snapshots {
            if !snapshot.subagents.is_empty() && !live.contains(&snapshot.id) {
                clear_list(&self.inner.sessions, &snapshot.id);
            }
        }
        self.clear_ended_runs();
    }

    /// The listed runs to read: each that named its conversation and has not
    /// ended, read with the rows of the session on the conversation that
    /// dispatched it. The runs lock is released before any record lock.
    fn live_runs(&self, snapshots: &[SessionSnapshot]) -> Vec<Live> {
        let Some(runs) = &self.inner.runs else {
            return Vec::new();
        };
        runs.subagent_sources()
            .into_iter()
            .map(|source| {
                let extra_env = host_env(
                    &self.inner.sessions,
                    snapshots,
                    source.session_id.as_deref(),
                );
                run_live(source, extra_env)
            })
            .collect()
    }

    /// Empty the list of every run no longer read, once.
    fn clear_ended_runs(&self) {
        let Some(runs) = &self.inner.runs else {
            return;
        };
        for (run_id, generation) in runs.ended_with_subagents() {
            runs.set_subagents(&run_id, generation, Vec::new());
        }
    }

    /// Drop the readers of owners no longer live, and take each live owner's
    /// reader out for the pass; a reader of another transcript is dropped,
    /// so the pass builds a fresh one.
    fn take_jobs(&self, live: Vec<Live>) -> Vec<Job> {
        let mut readers = lock(&self.inner.readers);
        let owners: HashSet<&Owner> = live.iter().map(|live| &live.owner).collect();
        readers.retain(|owner, _| owners.contains(owner));
        live.into_iter()
            .map(|live| {
                let reader = readers
                    .remove(&live.owner)
                    .filter(|cached| cached.key == live.key)
                    .map(|cached| cached.reader);
                Job {
                    live,
                    reader,
                    snapshot: None,
                }
            })
            .collect()
    }

    /// Keep a job's reader for the next pass and write its owner's list, read
    /// under this pass's `listing` and `now`.
    fn apply(&self, job: Job, listing: SubagentListing, now: DateTime<Utc>) {
        let Job {
            live,
            reader,
            snapshot,
        } = job;
        let list = snapshot
            .as_ref()
            .map(|snapshot| listed(snapshot, live.key.process_started_at, listing, now))
            .unwrap_or_default();
        match &live.owner {
            Owner::Session(id) => write_list(&self.inner.sessions, id, &live.key, list),
            Owner::Run { id, generation } => {
                if let Some(runs) = &self.inner.runs {
                    runs.set_subagents(id, *generation, list);
                }
            }
        }
        if let Some(reader) = reader {
            lock(&self.inner.readers).insert(
                live.owner,
                Cached {
                    key: live.key,
                    reader,
                },
            );
        }
    }
}

/// The agent plugin's reader for `source`.
fn plugin_reader(agent: &AgentId, source: &ActivitySource) -> Option<Box<dyn ActivityReader>> {
    agent_registry::plugin(agent)?.activity_reader(source)
}

/// The live sessions: each with an agent, a conversation and a folder, and
/// [`is_live`] on its record.
fn live_sessions(sessions: &SessionRegistry, snapshots: &[SessionSnapshot]) -> Vec<Live> {
    snapshots
        .iter()
        .filter_map(|snapshot| {
            let record = sessions.get(&snapshot.id)?;
            let record = lock(&record);
            if !is_live(&record) {
                return None;
            }
            let folder = crate::history::primary_cwd(&record)?;
            Some(Live {
                owner: Owner::Session(snapshot.id.clone()),
                agent: record.agent.clone(),
                key: ReaderKey {
                    conversation_id: record.conversation_id.clone()?,
                    folder: PathBuf::from(folder),
                    process_started_at: record.started_at,
                },
                extra_env: record
                    .spawn_config
                    .as_ref()
                    .map(|config| config.extra_env.clone())
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// What a pass needs of one listed run: its own conversation in its
/// worktree, its start as the process start, and `extra_env`'s rows.
fn run_live(source: RunSource, extra_env: Vec<(String, String)>) -> Live {
    Live {
        owner: Owner::Run {
            id: source.run_id,
            generation: source.generation,
        },
        agent: source.agent,
        key: ReaderKey {
            conversation_id: source.agent_session_id,
            folder: source.worktree,
            process_started_at: source.started_at,
        },
        extra_env,
    }
}

/// The stored env rows of the first listed session on the conversation
/// `session_id`, which a run it dispatched inherits; empty when none is.
pub(crate) fn host_env(
    sessions: &SessionRegistry,
    snapshots: &[SessionSnapshot],
    session_id: Option<&str>,
) -> Vec<(String, String)> {
    let Some(session_id) = session_id else {
        return Vec::new();
    };
    snapshots
        .iter()
        .find_map(|snapshot| {
            let record = sessions.get(&snapshot.id)?;
            let record = lock(&record);
            if record.conversation_id.as_deref() != Some(session_id) {
                return None;
            }
            Some(
                record
                    .spawn_config
                    .as_ref()
                    .map(|config| config.extra_env.clone())
                    .unwrap_or_default(),
            )
        })
        .unwrap_or_default()
}

/// Build the readers the jobs lack and read every one: blocking file IO.
fn run_jobs(jobs: Vec<Job>, factory: &ReaderFactory) -> Vec<Job> {
    let now = Utc::now();
    jobs.into_iter()
        .map(|mut job| {
            if job.reader.is_none() {
                let key = &job.live.key;
                let source = ActivitySource {
                    folder: key.folder.clone(),
                    conversation_id: key.conversation_id.clone(),
                    rows: crate::conversations::lookup_rows(&job.live.extra_env),
                    process_started_at: Some(key.process_started_at),
                };
                job.reader = factory(&job.live.agent, &source);
            }
            job.snapshot = job.reader.as_mut().map(|reader| reader.read(now));
            job
        })
        .collect()
}

/// Whether a session's subagents are read and listed: it runs an agent, is
/// not parked or abandoned, has not stopped or failed, and its agent has not
/// ended inside its shell.
fn is_live(record: &SessionRecord) -> bool {
    record.snapshot_agent().is_some()
        && !record.is_inactive
        && !record.is_abandoned
        && !matches!(record.status, SessionStatus::Stopped | SessionStatus::Error)
        && !agent_ended_in_shell(record)
}

/// Whether the session's agent ended inside its shell, which leaves the
/// shell running under the session's label.
fn agent_ended_in_shell(record: &SessionRecord) -> bool {
    record.agent_in_shell
        && record.shell_label.is_some()
        && record.program_name == record.shell_label
}

/// Whether `record` is still live and still the session `key` was read from:
/// the conversation, process start and folder of the reading unchanged.
fn read_from(record: &SessionRecord, key: &ReaderKey) -> bool {
    is_live(record)
        && record.conversation_id.as_deref() == Some(key.conversation_id.as_str())
        && record.started_at == key.process_started_at
        && crate::history::primary_cwd(record)
            .is_some_and(|folder| Path::new(&folder) == key.folder.as_path())
}

/// Write `list` to session `id`'s record, sending its `SessionUpdated`, only
/// when the record is still the one `key` was read from and `list` differs
/// from the record's.
fn write_list(sessions: &SessionRegistry, id: &str, key: &ReaderKey, list: Vec<SubagentSnapshot>) {
    sessions.update_if_changed(id, |record| {
        if !read_from(record, key) || record.subagents == list {
            return false;
        }
        record.subagents = list;
        true
    });
}

/// Empty session `id`'s list, sending its `SessionUpdated`, once it is no
/// longer live; the write is the one path that does not need the record to
/// still be live.
fn clear_list(sessions: &SessionRegistry, id: &str) {
    sessions.update_if_changed(id, |record| {
        if record.subagents.is_empty() {
            return false;
        }
        record.subagents = Vec::new();
        true
    });
}

/// A session's list from its reading, in tree order: each listed first-layer
/// subagent, oldest first, followed by its subtree, under `listing` read at
/// `now`.
fn listed(
    snapshot: &ActivitySnapshot,
    process_started_at: DateTime<Utc>,
    listing: SubagentListing,
    now: DateTime<Utc>,
) -> Vec<SubagentSnapshot> {
    let window = Window {
        process_started_at,
        last_typed_prompt_at: snapshot.last_typed_prompt_at,
    };
    let kept = kept_ended(&snapshot.subagents, &window, listing, now);
    let mut list = Vec::new();
    for node in &snapshot.subagents {
        if runs(node) || kept.contains(node.agent_id.as_str()) {
            flatten(node, 1, &mut list);
        }
    }
    list
}

/// Whether `node` or any subagent under it runs.
fn runs(node: &SubagentNode) -> bool {
    matches!(node.state, AgentState::Running | AgentState::Queued) || node.children.iter().any(runs)
}

/// Whether `node` ended after `window`'s process start and, with no
/// `keep_minutes` window, after its newest typed prompt too; a window replaces
/// the prompt rule with how long ago it finished at `now`.
fn ended_in(
    node: &SubagentNode,
    window: &Window,
    listing: SubagentListing,
    now: DateTime<Utc>,
) -> bool {
    node.finished_at.is_some_and(|ended| {
        ended > window.process_started_at
            && match listing.keep_minutes {
                None => window
                    .last_typed_prompt_at
                    .is_none_or(|prompt| ended > prompt),
                Some(minutes) => ended + ChronoDuration::minutes(i64::from(minutes)) > now,
            }
    })
}

/// The ids of the ended first-layer subagents that stay listed: the newest
/// [`SubagentListing::finished_cap`] of those that ended inside `window`.
fn kept_ended<'a>(
    nodes: &'a [SubagentNode],
    window: &Window,
    listing: SubagentListing,
    now: DateTime<Utc>,
) -> HashSet<&'a str> {
    let mut ended: Vec<&SubagentNode> = nodes
        .iter()
        .filter(|node| !runs(node) && ended_in(node, window, listing, now))
        .collect();
    ended.sort_by(|left, right| {
        (right.finished_at, &right.agent_id).cmp(&(left.finished_at, &left.agent_id))
    });
    ended
        .into_iter()
        .take(listing.finished_cap as usize)
        .map(|node| node.agent_id.as_str())
        .collect()
}

/// Push `node` at `depth`, then its subtree, onto `list`.
fn flatten(node: &SubagentNode, depth: u8, list: &mut Vec<SubagentSnapshot>) {
    list.push(SubagentSnapshot {
        agent_id: node.agent_id.clone(),
        parent_agent_id: node.parent_agent_id.clone(),
        depth,
        agent_type: node.agent_type.clone(),
        description: node.description.clone(),
        model: node.model.clone(),
        effort: node.effort.clone(),
        state: node.state,
        end_status: node.end_status.clone(),
        started_at: node.started_at,
        finished_at: node.finished_at,
    });
    for child in &node.children {
        flatten(child, depth.saturating_add(1), list);
    }
}
