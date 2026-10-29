//! The Dashboard's facts service: a session facts record per listed session,
//! read from the agent plugin's transcript reader and sent only to the
//! connections that watch a Dashboard.
//!
//! A connection is a watcher while it is open and its last `WatchDashboard`
//! said on. While at least one watcher exists a task ticks every
//! [`TICK`]: it reads each listed session's transcript, rebuilds its facts,
//! and sends each watcher the entries that changed since that watcher's last
//! send. With no watcher the task parks and reads nothing. A new watcher's
//! first message holds every listed session's facts.

pub(crate) mod now;
pub(crate) mod pr;
pub(crate) mod work;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_api::{
    ActivityReader, ActivitySnapshot, ActivitySource, BackgroundTask, EndedTask, SubagentNode,
};
use chrono::{DateTime, Utc};
use protocol::{
    AgentId, AgentKind, AgentNode, AgentState, DaemonMessage, DayTotals, RecentWork, SessionFacts,
    SessionMember, SessionStatus, StuckReason,
};
use tokio::sync::{mpsc, watch};
use tracing::warn;

use crate::session::SessionRegistry;
use crate::sync::lock;

use self::now::{NowInput, now_line, one_line, running_text, subagent_name};

/// How often the facts are read while a Dashboard is watched.
pub(crate) const TICK: Duration = Duration::from_secs(2);

/// How many failed tool results in a row make a session or a node stuck.
const STUCK_FAILURES: u32 = 3;

/// The context share past which a session is stuck on a full context.
const CONTEXT_HIGH_PERCENT: u8 = 80;

/// Builds an agent's transcript reader for one session, `None` when the
/// agent has none or the transcript cannot be located.
pub(crate) type ReaderFactory =
    Arc<dyn Fn(&AgentId, &ActivitySource) -> Option<Box<dyn ActivityReader>> + Send + Sync>;

/// The Dashboard's facts service, shared by the hub, every connection and
/// the tick task.
#[derive(Clone)]
pub(crate) struct Dashboard {
    inner: Arc<Inner>,
}

struct Inner {
    sessions: Arc<SessionRegistry>,
    factory: ReaderFactory,
    state: Mutex<State>,
    /// The number of watchers, which wakes and parks the tick task.
    watcher_count: watch::Sender<usize>,
    /// Held across a whole read pass and its sends, so a tick and a new
    /// watcher's first pass never read at once or interleave their sends.
    pass: tokio::sync::Mutex<()>,
    /// Each worktree's recent-work read, reused for [`work::TTL`].
    work: work::WorkCache,
    /// Each branch's pull-request read, reused for [`pr::TTL`].
    pr: pr::PrCache,
}

#[derive(Default)]
struct State {
    watchers: HashMap<u64, Watcher>,
    /// One entry per listed session, by session id.
    entries: BTreeMap<String, Entry>,
}

/// A connection that watches a Dashboard.
struct Watcher {
    out_tx: mpsc::UnboundedSender<DaemonMessage>,
    /// The facts last sent to it, by session id.
    sent: HashMap<String, SessionFacts>,
    /// Whether it has had its first message.
    greeted: bool,
}

/// Which transcript a reader follows; a change needs a fresh reader.
#[derive(Clone, PartialEq, Eq)]
struct ReaderKey {
    conversation_id: String,
    folder: PathBuf,
    process_started_at: DateTime<Utc>,
}

/// One listed session's cached reading.
struct Entry {
    key: Option<ReaderKey>,
    /// Taken out while a read pass runs.
    reader: Option<Box<dyn ActivityReader>>,
    /// The newest reading of the session's transcript.
    snapshot: Option<ActivitySnapshot>,
    /// The status seen on the previous pass, to tell when a turn starts.
    last_status: Option<SessionStatus>,
    /// When the session's current or latest turn started: finished nodes from
    /// before it are no longer listed.
    turn_started_at: Option<DateTime<Utc>>,
    facts: SessionFacts,
}

/// What a read pass needs of one listed session, copied out of its record.
struct Listed {
    id: String,
    agent: AgentId,
    status: SessionStatus,
    status_since: Option<DateTime<Utc>>,
    background: bool,
    started_at: DateTime<Utc>,
    key: Option<ReaderKey>,
    extra_env: Vec<(String, String)>,
}

/// One session's read, handed to the blocking pool and back.
struct Job {
    id: String,
    agent: AgentId,
    key: Option<ReaderKey>,
    extra_env: Vec<(String, String)>,
    reader: Option<Box<dyn ActivityReader>>,
    snapshot: Option<ActivitySnapshot>,
}

impl Dashboard {
    /// The service over `sessions`, reading with each agent plugin's reader.
    pub(crate) fn new(sessions: Arc<SessionRegistry>) -> Self {
        Self::with_reader_factory(sessions, Arc::new(plugin_reader))
    }

    /// The service over `sessions`, reading with `factory`'s readers.
    pub(crate) fn with_reader_factory(
        sessions: Arc<SessionRegistry>,
        factory: ReaderFactory,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                sessions,
                factory,
                state: Mutex::new(State::default()),
                watcher_count: watch::Sender::new(0),
                pass: tokio::sync::Mutex::new(()),
                work: work::WorkCache::default(),
                pr: pr::PrCache::default(),
            }),
        }
    }

    /// The service with its pull-request reads running `program` instead of
    /// `gh`: a test's stand-in, so no test spawns the real CLI.
    #[cfg(test)]
    #[expect(
        clippy::expect_used,
        reason = "a freshly built service has no other holder"
    )]
    pub(crate) fn with_pr_program(mut self, program: &str) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("a Dashboard no one else holds retargets its pull-request reads")
            .pr = pr::PrCache::with_program(program);
        self
    }

    /// Make `connection` a watcher sending on `out_tx`, and send it every
    /// listed session's facts. The first watcher gets a read pass at once,
    /// so it never waits for a tick or sees a reading from before it came.
    pub(crate) async fn watch(
        &self,
        connection: u64,
        out_tx: mpsc::UnboundedSender<DaemonMessage>,
    ) {
        let _pass = self.inner.pass.lock().await;
        let first = {
            let mut state = lock(&self.inner.state);
            let first = state.watchers.is_empty();
            state.watchers.insert(
                connection,
                Watcher {
                    out_tx,
                    sent: HashMap::new(),
                    greeted: false,
                },
            );
            self.watchers_changed(&mut state);
            first
        };
        if first {
            self.read_pass(Utc::now()).await;
        }
        self.send_changes();
    }

    /// Stop `connection` watching; a connection that was not is left alone.
    pub(crate) fn unwatch(&self, connection: u64) {
        let mut state = lock(&self.inner.state);
        if state.watchers.remove(&connection).is_some() {
            self.watchers_changed(&mut state);
        }
    }

    /// A guard that stops `connection` watching when dropped, so a closed
    /// connection is never left a watcher.
    pub(crate) fn watch_guard(&self, connection: u64) -> WatchGuard {
        WatchGuard {
            dashboard: self.clone(),
            connection,
        }
    }

    /// The first prompt of `session_id`'s conversation, as last read.
    pub(crate) fn prompt(&self, session_id: &str) -> Option<String> {
        let state = lock(&self.inner.state);
        state
            .entries
            .get(session_id)
            .and_then(|entry| entry.snapshot.as_ref())
            .and_then(|snapshot| snapshot.first_prompt.clone())
    }

    /// The recent work of a session's `members`, each worktree's read reused
    /// for [`work::TTL`] and each branch's pull request for [`pr::TTL`].
    pub(crate) async fn recent_work(
        &self,
        members: &[SessionMember],
        started_at: DateTime<Utc>,
    ) -> RecentWork {
        work::recent_work(&self.inner.work, &self.inner.pr, members, started_at).await
    }

    /// One tick: read every listed session's transcript as of `now` and send
    /// each watcher what changed. Reads nothing with no watcher.
    pub(crate) async fn tick(&self, now: DateTime<Utc>) {
        let _pass = self.inner.pass.lock().await;
        if self.read_pass(now).await {
            self.send_changes();
        }
    }

    /// Start the tick task for the daemon's life: it parks while no
    /// connection watches and ticks every [`TICK`] while one does.
    pub(crate) fn spawn_ticker(&self) -> tokio::task::JoinHandle<()> {
        let dashboard = self.clone();
        let mut count = self.inner.watcher_count.subscribe();
        tokio::spawn(async move {
            loop {
                if count.wait_for(|watchers| *watchers > 0).await.is_err() {
                    return;
                }
                let mut interval = tokio::time::interval(TICK);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                // The first tick completes at once; the new watcher's own
                // pass has just read.
                interval.tick().await;
                loop {
                    tokio::select! {
                        _ = interval.tick() => dashboard.tick(Utc::now()).await,
                        changed = count.changed() => {
                            if changed.is_err() {
                                return;
                            }
                            if *count.borrow_and_update() == 0 {
                                break;
                            }
                        }
                    }
                }
            }
        })
    }

    /// The number of watchers, as the tick task sees it.
    #[cfg(test)]
    pub(crate) fn watcher_count(&self) -> watch::Receiver<usize> {
        self.inner.watcher_count.subscribe()
    }

    /// Publish the watcher count after the watchers changed. With none left
    /// the cache empties, so no session's facts outlive the watch; the next
    /// first watcher's pass rebuilds it.
    fn watchers_changed(&self, state: &mut State) {
        if state.watchers.is_empty() {
            state.entries.clear();
        }
        let watchers = state.watchers.len();
        self.inner.watcher_count.send_if_modified(|count| {
            let changed = *count != watchers;
            *count = watchers;
            changed
        });
    }

    /// Read every listed session's transcript and rebuild its facts; a
    /// session no longer listed leaves the cache. Answers false, having read
    /// nothing, when no connection watches.
    async fn read_pass(&self, now: DateTime<Utc>) -> bool {
        let listed = listed_sessions(&self.inner.sessions);
        let jobs = {
            let mut state = lock(&self.inner.state);
            if state.watchers.is_empty() {
                return false;
            }
            take_jobs(&mut state, &listed)
        };
        let factory = Arc::clone(&self.inner.factory);
        let done = match tokio::task::spawn_blocking(move || run_jobs(jobs, &factory, now)).await {
            Ok(done) => done,
            Err(err) => {
                warn!(
                    ?err,
                    "dashboard: the transcript read pass failed; its readers start again next pass"
                );
                return true;
            }
        };
        let mut state = lock(&self.inner.state);
        // The last watcher may have left while the pass read; its emptied
        // cache stays empty.
        if state.watchers.is_empty() {
            return false;
        }
        apply_jobs(&mut state, &listed, done, now);
        true
    }

    /// Send each watcher the entries that changed since its last send, and
    /// a first message to one that has had none. A watcher whose connection
    /// is gone is dropped.
    fn send_changes(&self) {
        let mut state = lock(&self.inner.state);
        let State { watchers, entries } = &mut *state;
        watchers.retain(|connection, watcher| {
            watcher.sent.retain(|id, _| entries.contains_key(id));
            let changed: Vec<SessionFacts> = entries
                .iter()
                .filter(|(id, entry)| watcher.sent.get(*id) != Some(&entry.facts))
                .map(|(_, entry)| entry.facts.clone())
                .collect();
            if changed.is_empty() && watcher.greeted {
                return true;
            }
            let message = DaemonMessage::DashboardFacts {
                facts: changed.clone(),
                totals: DayTotals::default(),
                linear_synced_at: None,
            };
            if watcher.out_tx.send(message).is_err() {
                warn!(
                    connection,
                    "dashboard: a watcher's connection is gone; it stops watching"
                );
                return false;
            }
            watcher.greeted = true;
            for facts in changed {
                watcher.sent.insert(facts.session_id.clone(), facts);
            }
            true
        });
        self.watchers_changed(&mut state);
    }
}

/// Stops its connection watching when dropped.
pub(crate) struct WatchGuard {
    dashboard: Dashboard,
    connection: u64,
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        self.dashboard.unwatch(self.connection);
    }
}

/// The agent plugin's reader for `source`.
fn plugin_reader(agent: &AgentId, source: &ActivitySource) -> Option<Box<dyn ActivityReader>> {
    agent_registry::plugin(agent)?.activity_reader(source)
}

/// The sessions the Dashboard lists: every one with an agent, headless runs
/// included, but no plain shell and no parked or abandoned session.
fn listed_sessions(sessions: &SessionRegistry) -> Vec<Listed> {
    sessions
        .snapshots()
        .into_iter()
        .filter(|snapshot| {
            snapshot.agent.is_some() && !snapshot.is_inactive && !snapshot.is_abandoned
        })
        .filter_map(|snapshot| {
            let record = sessions.get(&snapshot.id)?;
            let record = lock(&record);
            let folder = crate::history::primary_cwd(&record);
            let key =
                record
                    .conversation_id
                    .clone()
                    .zip(folder)
                    .map(|(conversation_id, folder)| ReaderKey {
                        conversation_id,
                        folder: PathBuf::from(folder),
                        process_started_at: record.started_at,
                    });
            Some(Listed {
                id: snapshot.id,
                agent: record.agent.clone(),
                status: record.status,
                status_since: record.status_since,
                background: record.background,
                started_at: record.started_at,
                key,
                extra_env: record
                    .spawn_config
                    .as_ref()
                    .map(|config| config.extra_env.clone())
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// Drop the entries of sessions no longer listed, and take each listed
/// session's reader out for the pass; a reader of another transcript is
/// left behind, so the pass builds a fresh one.
fn take_jobs(state: &mut State, listed: &[Listed]) -> Vec<Job> {
    let ids: HashSet<&str> = listed.iter().map(|session| session.id.as_str()).collect();
    state.entries.retain(|id, _| ids.contains(id.as_str()));
    listed
        .iter()
        .map(|session| {
            let reader = state
                .entries
                .get_mut(&session.id)
                .filter(|entry| entry.key == session.key)
                .and_then(|entry| entry.reader.take());
            Job {
                id: session.id.clone(),
                agent: session.agent.clone(),
                key: session.key.clone(),
                extra_env: session.extra_env.clone(),
                reader,
                snapshot: None,
            }
        })
        .collect()
}

/// Build the readers the jobs lack and read every one: blocking file IO.
fn run_jobs(jobs: Vec<Job>, factory: &ReaderFactory, now: DateTime<Utc>) -> Vec<Job> {
    jobs.into_iter()
        .map(|mut job| {
            if job.reader.is_none()
                && let Some(key) = &job.key
            {
                let source = ActivitySource {
                    folder: key.folder.clone(),
                    conversation_id: key.conversation_id.clone(),
                    rows: crate::conversations::lookup_rows(&job.extra_env),
                    process_started_at: Some(key.process_started_at),
                };
                job.reader = factory(&job.agent, &source);
            }
            job.snapshot = job.reader.as_mut().map(|reader| reader.read(now));
            job
        })
        .collect()
}

/// Put a pass's readings into the cache and rebuild each session's facts.
fn apply_jobs(state: &mut State, listed: &[Listed], done: Vec<Job>, now: DateTime<Utc>) {
    let mut done: HashMap<String, Job> =
        done.into_iter().map(|job| (job.id.clone(), job)).collect();
    for session in listed {
        let Some(job) = done.remove(&session.id) else {
            continue;
        };
        let entry = state
            .entries
            .entry(session.id.clone())
            .or_insert_with(|| Entry {
                key: None,
                reader: None,
                snapshot: None,
                last_status: None,
                turn_started_at: None,
                facts: empty_facts(&session.id),
            });
        if entry.key != job.key || job.reader.is_none() {
            entry.snapshot = None;
        }
        entry.key = job.key;
        entry.reader = job.reader;
        if job.snapshot.is_some() {
            entry.snapshot = job.snapshot;
        }
        note_status(entry, session);
        entry.facts = build_facts(session, entry.snapshot.as_ref(), entry.turn_started_at, now);
    }
}

/// Mark a turn's start: the session moved into `Working`, or was first seen
/// outside it (its latest turn's start is then unknown, so it counts from
/// when that status began).
fn note_status(entry: &mut Entry, session: &Listed) {
    let starts_turn = match entry.last_status {
        None => true,
        Some(previous) => {
            session.status == SessionStatus::Working && previous != SessionStatus::Working
        }
    };
    if starts_turn {
        entry.turn_started_at = session.status_since;
    }
    entry.last_status = Some(session.status);
}

fn empty_facts(session_id: &str) -> SessionFacts {
    SessionFacts {
        session_id: session_id.to_owned(),
        now: protocol::NowLine {
            kind: protocol::NowKind::Unknown,
            text: String::new(),
            since: None,
        },
        agents: Vec::new(),
        context_percent: None,
        tokens_in: 0,
        tokens_out: 0,
        cost_usd: 0.0,
        activity: Vec::new(),
        stuck: Vec::new(),
        last_activity_at: None,
        issue: None,
        model: None,
    }
}

/// One session's facts from its registry status and its newest reading.
fn build_facts(
    session: &Listed,
    snapshot: Option<&ActivitySnapshot>,
    turn_started_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> SessionFacts {
    let input = NowInput {
        status: session.status,
        status_since: session.status_since,
        background: session.background,
        snapshot,
    };
    let mut facts = empty_facts(&session.id);
    facts.now = now_line(&input, now);
    let Some(snapshot) = snapshot else {
        return facts;
    };
    let tree = TreeContext {
        running: crate::idle_exit::blocks_exit(session.status, false),
        turn_started_at,
        fallback_start: snapshot.last_record_at.unwrap_or(session.started_at),
        now,
    };
    facts.agents = agent_tree(snapshot, &tree);
    facts.context_percent = snapshot
        .context
        .as_ref()
        .and_then(|context| context.percent);
    facts.tokens_in = snapshot.tokens_in;
    facts.tokens_out = snapshot.tokens_out;
    facts.cost_usd = snapshot.cost_usd;
    facts.activity = snapshot.minute_counts.to_vec();
    facts.stuck = stuck_reasons(snapshot);
    facts.last_activity_at = snapshot.last_record_at;
    facts.model.clone_from(&snapshot.model);
    facts
}

/// Why a session looks stuck: a run of failed tool results, a full context.
fn stuck_reasons(snapshot: &ActivitySnapshot) -> Vec<StuckReason> {
    let mut reasons = Vec::new();
    if let Some(reason) = failures(snapshot.failure_run) {
        reasons.push(reason);
    }
    if snapshot
        .context
        .as_ref()
        .and_then(|context| context.percent)
        .is_some_and(|percent| percent > CONTEXT_HIGH_PERCENT)
    {
        reasons.push(StuckReason::ContextHigh);
    }
    reasons
}

fn failures(failure_run: u32) -> Option<StuckReason> {
    (failure_run >= STUCK_FAILURES).then(|| StuckReason::ToolFailures {
        count: u16::try_from(failure_run).unwrap_or(u16::MAX),
    })
}

/// What mapping a session's agent tree needs beyond the nodes.
struct TreeContext {
    /// False for a stopped or failed session: none of its nodes still runs.
    running: bool,
    turn_started_at: Option<DateTime<Utc>>,
    /// The start a node with no time of its own takes.
    fallback_start: DateTime<Utc>,
    now: DateTime<Utc>,
}

impl TreeContext {
    /// A node's state once a session that is not running has had its
    /// running and queued nodes finished.
    fn state(&self, state: AgentState) -> AgentState {
        match state {
            AgentState::Running | AgentState::Queued if !self.running => AgentState::Finished,
            other => other,
        }
    }

    /// Whether a node that ended at `ended_at` ended before the current turn.
    fn before_turn(&self, ended_at: Option<DateTime<Utc>>) -> bool {
        matches!((ended_at, self.turn_started_at), (Some(ended), Some(turn)) if ended < turn)
    }
}

/// The session's agent tree: its subagents, then its background shells in
/// launch order, running ones before ended ones.
fn agent_tree(snapshot: &ActivitySnapshot, tree: &TreeContext) -> Vec<AgentNode> {
    let subagents = snapshot
        .subagents
        .iter()
        .filter_map(|node| subagent_node(node, tree));
    let running = snapshot
        .background
        .iter()
        .map(|task| running_shell_node(task, tree));
    let ended = snapshot
        .ended_background
        .iter()
        .filter(|ended| !tree.before_turn(ended.ended_at))
        .map(|ended| ended_shell_node(ended, tree));
    subagents.chain(running).chain(ended).collect()
}

/// A subagent and the ones it launched, or `None` for one that ended before
/// the current turn and keeps no listed child.
fn subagent_node(node: &SubagentNode, tree: &TreeContext) -> Option<AgentNode> {
    let children: Vec<AgentNode> = node
        .children
        .iter()
        .filter_map(|child| subagent_node(child, tree))
        .collect();
    let state = tree.state(node.state);
    let ended = !matches!(state, AgentState::Running | AgentState::Queued);
    if ended && children.is_empty() && tree.before_turn(node.finished_at) {
        return None;
    }
    Some(AgentNode {
        key: format!("subagent:{}", node.agent_id),
        kind: AgentKind::Subagent,
        name: subagent_name(node).to_owned(),
        // A node rule 1 finished never ended on its own: it has no result.
        task: if state == node.state {
            subagent_task(node, state, tree.now)
        } else {
            "finished".to_owned()
        },
        activity: Vec::new(),
        state,
        started_at: node
            .started_at
            .or(node.finished_at)
            .unwrap_or(tree.fallback_start),
        stuck: failures(node.failure_run),
        children,
    })
}

/// What a subagent runs now, or how it ended.
fn subagent_task(node: &SubagentNode, state: AgentState, now: DateTime<Utc>) -> String {
    let last_line = node
        .last_text
        .as_deref()
        .and_then(|text| text.lines().find(|line| !line.trim().is_empty()))
        .map(one_line);
    match state {
        AgentState::Running | AgentState::Queued => match &node.open_call {
            Some(call) => running_text(call, now),
            None => last_line.unwrap_or_default(),
        },
        AgentState::Finished => match last_line {
            Some(line) => format!("finished · {line}"),
            None => "finished".to_owned(),
        },
        AgentState::Failed => with_run_time(
            node.end_status.as_deref().unwrap_or("failed"),
            node.started_at,
            node.finished_at,
        ),
        AgentState::Unknown => String::new(),
    }
}

/// `word · <run time>`, or the word alone when either end is unknown.
fn with_run_time(
    word: &str,
    started: Option<DateTime<Utc>>,
    ended: Option<DateTime<Utc>>,
) -> String {
    match started.zip(ended) {
        Some((started, ended)) => format!("{word} · {}", now::age(started, ended)),
        None => word.to_owned(),
    }
}

fn shell_name(task: &BackgroundTask) -> String {
    format!("bg shell {}", task.id)
}

fn shell_start(task: &BackgroundTask, tree: &TreeContext) -> DateTime<Utc> {
    task.started.unwrap_or(tree.fallback_start)
}

/// A background shell still running, or finished with its stopped session.
fn running_shell_node(task: &BackgroundTask, tree: &TreeContext) -> AgentNode {
    let state = tree.state(AgentState::Running);
    let task_text = if state == AgentState::Running {
        task.command.as_deref().map(one_line).unwrap_or_default()
    } else {
        "finished".to_owned()
    };
    AgentNode {
        key: format!("bg:{}", task.id),
        kind: AgentKind::Shell,
        name: shell_name(task),
        task: task_text,
        activity: Vec::new(),
        state,
        started_at: shell_start(task, tree),
        stuck: None,
        children: Vec::new(),
    }
}

/// A background shell a task notification ended: its summary, else its
/// status word, with its run time.
fn ended_shell_node(ended: &EndedTask, tree: &TreeContext) -> AgentNode {
    let word = ended
        .summary
        .as_deref()
        .or(ended.status.as_deref())
        .map_or_else(|| "ended".to_owned(), one_line);
    AgentNode {
        key: format!("bg:{}", ended.task.id),
        kind: AgentKind::Shell,
        name: shell_name(&ended.task),
        task: with_run_time(&word, ended.task.started, ended.ended_at),
        activity: Vec::new(),
        state: tree.state(ended.state),
        started_at: shell_start(&ended.task, tree),
        stuck: None,
        children: Vec::new(),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A scripted reader factory: each conversation id reads the snapshot the
    //! test last set for it, and every read is counted.

    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use agent_api::{ActivityReader, ActivitySnapshot};
    use chrono::{DateTime, Utc};

    use super::ReaderFactory;
    use crate::sync::lock;

    /// The snapshots a scripted reader returns, and its read and build counts.
    #[derive(Clone, Default)]
    pub(crate) struct Script {
        snapshots: Arc<Mutex<HashMap<String, ActivitySnapshot>>>,
        reads: Arc<AtomicUsize>,
        builds: Arc<AtomicUsize>,
    }

    impl Script {
        /// Make `conversation_id`'s reader return `snapshot` from now on.
        pub(crate) fn set(&self, conversation_id: &str, snapshot: ActivitySnapshot) {
            lock(&self.snapshots).insert(conversation_id.to_owned(), snapshot);
        }

        /// How many reads the readers made.
        pub(crate) fn reads(&self) -> usize {
            self.reads.load(Ordering::SeqCst)
        }

        /// How many readers were built.
        pub(crate) fn builds(&self) -> usize {
            self.builds.load(Ordering::SeqCst)
        }

        /// A factory building readers over this script.
        pub(crate) fn factory(&self) -> ReaderFactory {
            let script = self.clone();
            Arc::new(move |_, source| {
                script.builds.fetch_add(1, Ordering::SeqCst);
                Some(Box::new(ScriptedReader {
                    script: script.clone(),
                    conversation_id: source.conversation_id.clone(),
                }) as Box<dyn ActivityReader>)
            })
        }
    }

    struct ScriptedReader {
        script: Script,
        conversation_id: String,
    }

    impl ActivityReader for ScriptedReader {
        fn read(&mut self, _now: DateTime<Utc>) -> ActivitySnapshot {
            self.script.reads.fetch_add(1, Ordering::SeqCst);
            lock(&self.script.snapshots)
                .get(&self.conversation_id)
                .cloned()
                .unwrap_or_default()
        }
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use agent_api::{
        ActivitySnapshot, BackgroundTask, ContextReading, EndedTask, OpenCall, SubagentNode,
    };
    use chrono::{DateTime, Duration, TimeZone as _, Utc};
    use protocol::{AgentKind, AgentState, SessionStatus, StuckReason};

    use super::{Listed, TreeContext, agent_tree, build_facts};

    fn at(minute: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 28, 12, 0, 0)
            .single()
            .expect("a valid time")
            + Duration::minutes(minute)
    }

    fn node(id: &str, state: AgentState) -> SubagentNode {
        SubagentNode {
            agent_id: id.to_owned(),
            parent_agent_id: None,
            agent_type: Some("Explore".to_owned()),
            description: Some(format!("look at {id}")),
            started_at: Some(at(1)),
            finished_at: (state != AgentState::Running).then(|| at(4)),
            state,
            end_status: None,
            open_call: None,
            last_text: None,
            failure_run: 0,
            activity: [0; agent_api::MINUTE_COLUMNS],
            children: Vec::new(),
        }
    }

    fn tree(running: bool, turn_started_at: Option<DateTime<Utc>>) -> TreeContext {
        TreeContext {
            running,
            turn_started_at,
            fallback_start: at(0),
            now: at(10),
        }
    }

    fn listed(status: SessionStatus) -> Listed {
        Listed {
            id: "s1".to_owned(),
            agent: agent_registry::default_agent(),
            status,
            status_since: Some(at(0)),
            background: false,
            started_at: at(0),
            key: None,
            extra_env: Vec::new(),
        }
    }

    #[test]
    fn a_subagent_maps_to_a_keyed_agent_node_with_its_children() {
        let mut launcher = node("a1", AgentState::Running);
        launcher.open_call = Some(OpenCall {
            id: "c1".to_owned(),
            name: "Bash".to_owned(),
            command: Some("cargo   build".to_owned()),
            started: at(8),
        });
        launcher.children = vec![node("a2", AgentState::Finished)];
        launcher.children[0].last_text = Some("\n2 findings, both fixed\nmore".to_owned());
        let snapshot = ActivitySnapshot {
            subagents: vec![launcher],
            ..ActivitySnapshot::default()
        };
        let nodes = agent_tree(&snapshot, &tree(true, None));
        assert_eq!(nodes.len(), 1);
        let top = &nodes[0];
        assert_eq!(top.key, "subagent:a1");
        assert_eq!(top.kind, AgentKind::Subagent);
        assert_eq!(top.name, "look at a1");
        assert_eq!(top.task, "Running Bash · 2m: cargo build");
        assert_eq!(top.state, AgentState::Running);
        assert_eq!(top.started_at, at(1));
        assert_eq!(top.activity, Vec::<u16>::new());
        assert_eq!(top.children.len(), 1);
        assert_eq!(top.children[0].key, "subagent:a2");
        assert_eq!(top.children[0].task, "finished · 2 findings, both fixed");
    }

    #[test]
    fn a_killed_subagent_reads_its_end_status_as_the_task() {
        let mut killed = node("a1", AgentState::Failed);
        killed.end_status = Some("killed".to_owned());
        killed.finished_at = Some(at(13));
        let snapshot = ActivitySnapshot {
            subagents: vec![killed],
            ..ActivitySnapshot::default()
        };
        let nodes = agent_tree(&snapshot, &tree(true, None));
        assert_eq!(nodes[0].task, "killed · 12m");
        assert_eq!(nodes[0].state, AgentState::Failed);
    }

    #[test]
    fn three_failed_results_in_a_row_mark_a_node_stuck() {
        let mut two = node("a1", AgentState::Running);
        two.failure_run = 2;
        let mut three = node("a2", AgentState::Running);
        three.failure_run = 3;
        let snapshot = ActivitySnapshot {
            subagents: vec![two, three],
            ..ActivitySnapshot::default()
        };
        let nodes = agent_tree(&snapshot, &tree(true, None));
        assert_eq!(nodes[0].stuck, None);
        assert_eq!(nodes[1].stuck, Some(StuckReason::ToolFailures { count: 3 }));
    }

    #[test]
    fn context_over_80_percent_is_context_high_and_three_failures_tool_failures() {
        let reading = |percent| ContextReading {
            model: "m".to_owned(),
            tokens: 1,
            percent: Some(percent),
        };
        let mut snapshot = ActivitySnapshot {
            context: Some(reading(80)),
            failure_run: 2,
            ..ActivitySnapshot::default()
        };
        let facts = |snapshot: &ActivitySnapshot| {
            build_facts(
                &listed(SessionStatus::Working),
                Some(snapshot),
                None,
                at(10),
            )
            .stuck
        };
        assert_eq!(facts(&snapshot), Vec::<StuckReason>::new());
        snapshot.context = Some(reading(81));
        snapshot.failure_run = 4;
        assert_eq!(
            facts(&snapshot),
            vec![
                StuckReason::ToolFailures { count: 4 },
                StuckReason::ContextHigh
            ]
        );
    }

    #[test]
    fn rule_1_finishes_running_nodes_of_a_stopped_session() {
        let mut launcher = node("a1", AgentState::Running);
        launcher.last_text = Some("reading the tests".to_owned());
        launcher.children = vec![node("a2", AgentState::Running)];
        launcher.children[0].last_text = Some("half way".to_owned());
        let mut failed = node("a3", AgentState::Failed);
        failed.end_status = Some("killed".to_owned());
        let snapshot = ActivitySnapshot {
            subagents: vec![launcher, failed],
            background: vec![BackgroundTask {
                id: "b1".to_owned(),
                tool_use_id: None,
                started: Some(at(2)),
                command: Some("npm run dev".to_owned()),
            }],
            ..ActivitySnapshot::default()
        };
        for status in [SessionStatus::Stopped, SessionStatus::Error] {
            let facts = build_facts(&listed(status), Some(&snapshot), None, at(10));
            let states: Vec<AgentState> = facts.agents.iter().map(|node| node.state).collect();
            assert_eq!(
                states,
                vec![
                    AgentState::Finished,
                    AgentState::Failed,
                    AgentState::Finished
                ],
                "{status:?}"
            );
            assert_eq!(facts.agents[0].children[0].state, AgentState::Finished);
            assert_eq!(facts.agents[0].task, "finished");
            assert_eq!(facts.agents[0].children[0].task, "finished");
            assert_eq!(facts.agents[2].task, "finished");
        }
        let running = build_facts(&listed(SessionStatus::Idle), Some(&snapshot), None, at(10));
        assert_eq!(running.agents[0].state, AgentState::Running);
        assert_eq!(running.agents[2].state, AgentState::Running);
    }

    #[test]
    fn finished_nodes_from_before_the_turn_are_dropped() {
        let mut old = node("old", AgentState::Finished);
        old.finished_at = Some(at(2));
        let mut old_launcher = node("launcher", AgentState::Finished);
        old_launcher.finished_at = Some(at(2));
        old_launcher.children = vec![node("child", AgentState::Running)];
        let mut recent = node("recent", AgentState::Finished);
        recent.finished_at = Some(at(6));
        let task = BackgroundTask {
            id: "b1".to_owned(),
            tool_use_id: None,
            started: Some(at(1)),
            command: None,
        };
        let snapshot = ActivitySnapshot {
            subagents: vec![old, old_launcher, recent],
            ended_background: vec![EndedTask {
                task,
                state: AgentState::Finished,
                status: Some("completed".to_owned()),
                summary: None,
                ended_at: Some(at(3)),
            }],
            ..ActivitySnapshot::default()
        };
        let keys: Vec<String> = agent_tree(&snapshot, &tree(true, Some(at(5))))
            .into_iter()
            .map(|node| node.key)
            .collect();
        assert_eq!(keys, vec!["subagent:launcher", "subagent:recent"]);
        let every = agent_tree(&snapshot, &tree(true, None));
        assert_eq!(every.len(), 4);
        assert_eq!(every[3].task, "completed · 2m");
    }
}
