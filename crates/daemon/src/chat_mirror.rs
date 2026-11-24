//! A session's chat as the connections that loaded it (its loaders) see it:
//! the entries read off the agent's transcript, the answers still arriving as
//! live lines, and the composer and dialog read off the screen. A loader
//! loads the chat, the summary (the chat's stretch lines,
//! [`crate::summary_lines`]), or both, and hears only of what it loaded.
//!
//! A session is mirrored while at least one connection has its chat or
//! summary loaded: its transcript is tailed from the start, its live lines
//! are followed, and its status watcher hands over each screen it reads
//! ([`ChatTap`]). The last unload, the last loader's connection ending, or
//! the session's removal drops the mirror; a session that ended stays
//! mirrored, read-only.
//!
//! One lock orders everything a loader is sent. A loader is registered and
//! sent its `Chat` or `Summary` reply under it, and every append, live merge,
//! screen change and conversation switch is applied and sent under it, so a
//! reply holds exactly what was applied before it and each later message is
//! new to the loader. The lock is taken inside a screen's grid lock, never
//! the reverse, and is never held across an await.

use std::any::Any;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use agent_api::{AgentPlugin, LiveLine, ScreenWork, TranscriptLineFacts};
use chrono::{DateTime, Utc};
use protocol::{
    AgentId, ChatEntry, ChatPrompt, ChatPromptKind, ChatScreen, ComposerState, DaemonMessage,
    PendingInput, PendingQuestion, SessionStatus, StretchLine, SummarySource, ToolKind,
};
use sha2::{Digest as _, Sha256};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::background_work::BackgroundWork;
use crate::chat_work::chat_work;
use crate::hook_status::LiveLines;
use crate::screen::{Screen, SharedScreen, dialogs};
use crate::session::{SessionEvent, SessionRecord, SessionRegistry};
use crate::summarizer::prompt::Trigger;
use crate::summarizer::service::{Outcome, Request, Summarizer};
use crate::summary_lines::{self, TimedEntry};
use crate::sync::lock;
use crate::transcript_tail::{self, TailEvent, TailHandle};

/// The most entries a `Chat` message carries: the newest ones, with its
/// `hidden` counting the older ones left out.
pub const CHAT_REPLY_CAP: usize = 500;

/// How long the open stretch's text must stay the same before the
/// summarizer is asked for its line.
const OPEN_QUIET: Duration = Duration::from_secs(30);

/// How many of the closed stretches, newest first, a whole summary sent at
/// the end of a read asks the summarizer for; older ones keep their
/// template.
const FIRST_READ_LIMIT: usize = 20;

/// The shortest gap between two sends of the live answers still growing.
const LIVE_FLUSH_GAP: Duration = Duration::from_millis(100);

/// The row a chat starts with after the session moved to a new conversation.
const CLEARED_TEXT: &str = "Conversation cleared";

/// The next prompt id to mint, unique within the daemon's run.
static NEXT_PROMPT: AtomicU64 = AtomicU64::new(1);

/// The next epoch a chat switch starts, unique within the daemon's run, so
/// no two tails ever carry the same one.
static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

/// The number in the next pending entry's id, unique within the daemon's
/// run.
static NEXT_PENDING: AtomicU64 = AtomicU64::new(1);

type Outbox = mpsc::UnboundedSender<DaemonMessage>;

/// Every session's chat mirror. Cheap to clone; clones share one mirror.
#[derive(Clone)]
pub struct ChatMirror {
    inner: Arc<Inner>,
}

struct Inner {
    /// The mirrored sessions, by session id: the one lock that orders every
    /// send to a loader.
    sessions: Mutex<HashMap<String, SessionMirror>>,
    registry: Arc<SessionRegistry>,
    live_lines: LiveLines,
    /// The count of connected clients a tail parks on while it is zero.
    client_count: Arc<watch::Sender<usize>>,
    /// Whether the task following the registry's conversation ids and
    /// removals has started.
    following: AtomicBool,
    /// What writes the summary loaders' lines once set; until then they get
    /// the template lines only.
    summarizer: Mutex<Option<Arc<Summarizer>>>,
    /// How many live-line followers are running.
    #[cfg(test)]
    live_tasks: Arc<std::sync::atomic::AtomicUsize>,
    /// How many summary requests are queued or running.
    #[cfg(test)]
    summary_jobs: Arc<std::sync::atomic::AtomicUsize>,
    /// Run once by the next load, between its read of the record and its
    /// locks.
    #[cfg(test)]
    load_gap: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

/// Counts one running live-line follower until it is dropped.
#[cfg(test)]
struct Running(Arc<std::sync::atomic::AtomicUsize>);

#[cfg(test)]
impl Running {
    fn enter(count: &Arc<std::sync::atomic::AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(count))
    }
}

#[cfg(test)]
impl Drop for Running {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What a connection loads of a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoadKind {
    Chat,
    Summary,
}

/// Why a session's chat cannot be loaded, as the `Error` that answers it.
#[derive(Debug)]
pub struct LoadError {
    pub message: String,
    pub code: &'static str,
}

/// The conversation a session's chat reads: its id and, when the agent keeps
/// one for it, its transcript file.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Conversation {
    id: String,
    path: Option<PathBuf>,
}

impl ChatMirror {
    /// A mirror of `registry`'s sessions, merging the lines of `live_lines`;
    /// its tails park while `client_count` is zero.
    pub fn new(
        registry: Arc<SessionRegistry>,
        live_lines: LiveLines,
        client_count: Arc<watch::Sender<usize>>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                sessions: Mutex::new(HashMap::new()),
                registry,
                live_lines,
                client_count,
                following: AtomicBool::new(false),
                summarizer: Mutex::new(None),
                #[cfg(test)]
                live_tasks: Arc::default(),
                #[cfg(test)]
                summary_jobs: Arc::default(),
                #[cfg(test)]
                load_gap: Mutex::new(None),
            }),
        }
    }

    fn upgrade(weak: &Weak<Inner>) -> Option<Self> {
        weak.upgrade().map(|inner| Self { inner })
    }

    /// Have `summarizer` write the summary loaders' lines: each session
    /// mirrored from the next summary load on asks it for its stretches'
    /// lines.
    pub fn set_summarizer(&self, summarizer: Arc<Summarizer>) {
        *lock(&self.inner.summarizer) = Some(summarizer);
    }

    /// The hand-off for `session_id`'s status watcher, whose screen `agent`'s
    /// plugin reads.
    #[must_use]
    pub fn tap(&self, session_id: &str, agent: &AgentId) -> ChatTap {
        ChatTap {
            mirror: self.clone(),
            session_id: session_id.to_owned(),
            agent: agent.clone(),
        }
    }

    /// A guard that unloads every chat `connection` loaded when it drops.
    #[must_use]
    pub fn guard(&self, connection: u64) -> ChatLoadGuard {
        ChatLoadGuard {
            mirror: self.clone(),
            connection,
        }
    }

    /// Make `connection`, whose messages go to `out_tx`, a loader of
    /// `session_id`'s chat and send it the chat as it stands, carrying
    /// `request_id`. A session already mirrored keeps its tail; a new one
    /// starts reading its transcript from the start, so its first reply is
    /// sent before that read and the read's end sends every chat loader the
    /// whole chat as one unsolicited `Chat`.
    ///
    /// # Errors
    ///
    /// The session is unknown, or its agent has no registered plugin; nothing
    /// is registered then.
    pub fn load(
        &self,
        session_id: &str,
        request_id: Option<String>,
        connection: u64,
        out_tx: &Outbox,
    ) -> Result<(), LoadError> {
        self.load_as(LoadKind::Chat, session_id, request_id, connection, out_tx)
    }

    /// Make `connection`, whose messages go to `out_tx`, a summary loader of
    /// `session_id` and send it every stretch line as it stands, carrying
    /// `request_id`, the way [`Self::load`] does the chat; the first read's
    /// end sends every summary loader the lines as one unsolicited `Summary`.
    ///
    /// # Errors
    ///
    /// The session is unknown, or its agent has no registered plugin; nothing
    /// is registered then.
    pub fn load_summary(
        &self,
        session_id: &str,
        request_id: Option<String>,
        connection: u64,
        out_tx: &Outbox,
    ) -> Result<(), LoadError> {
        self.load_as(
            LoadKind::Summary,
            session_id,
            request_id,
            connection,
            out_tx,
        )
    }

    fn load_as(
        &self,
        kind: LoadKind,
        session_id: &str,
        request_id: Option<String>,
        connection: u64,
        out_tx: &Outbox,
    ) -> Result<(), LoadError> {
        self.follow_registry();
        let found = Found::read(&self.inner.registry, session_id).ok_or_else(|| LoadError {
            message: format!("unknown session: {session_id}"),
            code: protocol::error_codes::NOT_FOUND,
        })?;
        let plugin = agent_registry::plugin(&found.agent).ok_or_else(|| LoadError {
            message: format!("chat is not offered for agent `{}`", found.agent.0),
            code: protocol::error_codes::UNSUPPORTED_MESSAGE,
        })?;
        let conversation = found.conversation(plugin);
        #[cfg(test)]
        self.run_load_gap();
        // The grid before the mirror, the tap's order: no check of this
        // screen lands between the reading below and the reply.
        let grid = found.screen.as_ref().map(|screen| lock(screen));
        {
            let mut sessions = lock(&self.inner.sessions);
            let mirror = match sessions.entry(session_id.to_owned()) {
                Entry::Occupied(held) => held.into_mut(),
                Entry::Vacant(slot) => slot.insert(SessionMirror::new(
                    session_id,
                    plugin,
                    self.start_live(session_id),
                )),
            };
            if mirror.conversation != conversation {
                self.switch(session_id, mirror, conversation);
            }
            mirror.take_hook(session_id, found.plan.clone(), found.questions.clone());
            if let Some(screen) = grid.as_deref() {
                if let Some(reading) = dialogs::read(&found.agent, screen) {
                    mirror.observe(session_id, reading);
                }
                if let Some(counted) = dialogs::read_work(&found.agent, screen) {
                    mirror.screen_work = Some(counted);
                }
            }
            mirror.work.clone_from(&found.work);
            mirror.update_work(session_id);
            if kind == LoadKind::Summary {
                self.start_writer(session_id, &mut mirror.writer);
            }
            let reply = mirror.register(kind, session_id, request_id, connection, out_tx);
            // A closed outbox is a connection already ending; its guard
            // unloads. The strip follows the reply, to this loader only.
            let _ = out_tx.send(reply);
            let _ = out_tx.send(mirror.work_shown.clone());
            if let Some(model) = mirror.model_on_load(kind, session_id) {
                let _ = out_tx.send(model);
            }
        }
        drop(grid);
        // The record was read before the lock: a conversation change or a
        // removal handled since found no mirror to act on.
        self.refresh(session_id);
        Ok(())
    }

    /// Show `text`, which a chat send is about to submit in `session_id`'s
    /// terminal, as a pending user entry after the chat's entries until the
    /// transcript's record of it arrives, and send it to the chat loaders.
    /// Returns its id; `None`, adding nothing, when no connection has the
    /// chat loaded.
    #[must_use]
    pub fn note_sent(&self, session_id: &str, text: &str) -> Option<String> {
        let mut sessions = lock(&self.inner.sessions);
        let mirror = sessions.get_mut(session_id)?;
        if !mirror.loaders.values().any(|loader| loader.chat) {
            return None;
        }
        let id = format!("pending:{}", NEXT_PENDING.fetch_add(1, Ordering::Relaxed));
        let entry = mirror.pending.add(&id, text);
        let removed = mirror.pending.take_dropped();
        mirror.tell(&DaemonMessage::ChatAppend {
            session_id: session_id.to_owned(),
            entries: vec![entry],
            removed,
        });
        Some(id)
    }

    /// The id and identity ([`dialogs::prompt_identity`]) of the prompt
    /// `session_id`'s chat shows; `None` when no prompt is up, or when no
    /// connection loads the chat, so no prompt has an id.
    #[must_use]
    pub fn current_prompt(&self, session_id: &str) -> Option<(String, u64)> {
        let sessions = lock(&self.inner.sessions);
        let screen = &sessions.get(session_id)?.screen;
        screen
            .identity
            .map(|identity| (screen.prompt_id.clone(), identity))
    }

    /// Record that the chat answered Rewind's Restore for `session_id`, going
    /// back to before `message` was sent: once the transcript records the
    /// rewind, the chat drops the turns after that one. Nothing when the
    /// session has no mirror.
    pub fn note_restore(&self, session_id: &str, message: &str) {
        let mut sessions = lock(&self.inner.sessions);
        if let Some(mirror) = sessions.get_mut(session_id) {
            mirror.pending_rewind.note(message.to_owned());
        }
    }

    /// `connection` no longer loads `session_id`'s chat; the last loader
    /// leaving drops the session's mirror and stops its tail.
    pub fn unload(&self, session_id: &str, connection: u64) {
        self.unload_as(LoadKind::Chat, session_id, connection);
    }

    /// `connection` no longer loads `session_id`'s summary; the last loader
    /// leaving drops the session's mirror and stops its tail.
    pub fn unload_summary(&self, session_id: &str, connection: u64) {
        self.unload_as(LoadKind::Summary, session_id, connection);
    }

    fn unload_as(&self, kind: LoadKind, session_id: &str, connection: u64) {
        let mut sessions = lock(&self.inner.sessions);
        let Entry::Occupied(mut held) = sessions.entry(session_id.to_owned()) else {
            return;
        };
        let loaders = &mut held.get_mut().loaders;
        if let Entry::Occupied(mut loader) = loaders.entry(connection) {
            match kind {
                LoadKind::Chat => loader.get_mut().chat = false,
                LoadKind::Summary => loader.get_mut().summary = false,
            }
            if !loader.get().loads_any() {
                loader.remove();
            }
        }
        if held.get().loaders.is_empty() {
            held.remove();
        } else {
            held.get_mut().stop_writer_unless_loaded();
        }
    }

    /// `connection` ended: it loads no chat any more.
    fn connection_closed(&self, connection: u64) {
        lock(&self.inner.sessions).retain(|_, mirror| {
            mirror.loaders.remove(&connection);
            mirror.stop_writer_unless_loaded();
            !mirror.loaders.is_empty()
        });
    }

    /// How many sessions have their transcript tailed.
    #[cfg(test)]
    pub fn tail_count(&self) -> usize {
        lock(&self.inner.sessions)
            .values()
            .filter(|mirror| mirror.tail.is_some())
            .count()
    }

    /// How many sessions are mirrored.
    #[cfg(test)]
    pub fn mirror_count(&self) -> usize {
        lock(&self.inner.sessions).len()
    }

    /// How many live-line followers are running.
    #[cfg(test)]
    pub fn live_task_count(&self) -> usize {
        self.inner.live_tasks.load(Ordering::SeqCst)
    }

    /// How many summary requests are queued or running.
    #[cfg(test)]
    pub fn summary_jobs_pending(&self) -> usize {
        self.inner.summary_jobs.load(Ordering::SeqCst)
    }

    /// The epoch `session_id`'s tail events must carry, while it is mirrored.
    #[cfg(test)]
    pub fn epoch(&self, session_id: &str) -> Option<u64> {
        lock(&self.inner.sessions)
            .get(session_id)
            .map(|mirror| mirror.epoch)
    }

    /// Deliver `line` as a tail of `session_id` started on `epoch` would,
    /// while the session is mirrored.
    #[cfg(test)]
    pub fn tail_line(&self, session_id: &str, epoch: u64, line: &str) {
        let plugin = lock(&self.inner.sessions)
            .get(session_id)
            .map(|mirror| mirror.plugin);
        if let Some(plugin) = plugin {
            self.tail_event(session_id, epoch, plugin, TailEvent::Line(line.to_owned()));
        }
    }

    /// Run `hook` in the next load, after it read the session's record and
    /// before it takes any lock.
    #[cfg(test)]
    pub fn before_load_locks(&self, hook: impl FnOnce() + Send + 'static) {
        *lock(&self.inner.load_gap) = Some(Box::new(hook));
    }

    #[cfg(test)]
    fn run_load_gap(&self) {
        let hook = lock(&self.inner.load_gap).take();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Start `session_id`'s chat over on `conversation`: the old tail stops,
    /// the entries and live answers go, and the new transcript is read from
    /// its start; a move from one conversation to another starts with the
    /// row "Conversation cleared". The loaders are sent the fresh chat when
    /// the new transcript's first read ends, or at once when there is none.
    fn switch(
        &self,
        session_id: &str,
        mirror: &mut SessionMirror,
        conversation: Option<Conversation>,
    ) {
        if let Some(tail) = mirror.tail.take() {
            tail.stop();
        }
        mirror.epoch = NEXT_EPOCH.fetch_add(1, Ordering::Relaxed);
        let head = match (&mirror.conversation, &conversation) {
            (Some(_), Some(next)) => Some(ChatEntry::System {
                id: format!("cleared:{}", next.id),
                text: CLEARED_TEXT.to_owned(),
            }),
            _ => None,
        };
        let path = conversation.as_ref().and_then(|c| c.path.clone());
        mirror.conversation = conversation;
        mirror.chat = ChatState::starting_with(head, path.is_some());
        mirror.pending.drop_all();
        mirror.pending_rewind.clear();
        mirror.summary_sent.clear();
        // The new conversation names its model and effort at its first reply.
        mirror.model = None;
        mirror.effort = None;
        mirror.switched = None;
        mirror.update_model(session_id);
        if let Some(writer) = mirror.writer.as_mut() {
            writer.clear();
        }
        // Task ids are unique across conversations, so the ended ones stay.
        if let Some(path) = path {
            mirror.tail = Some(self.start_tail(session_id, path, mirror.epoch, mirror.plugin));
            // The loaders show the old chat until the first read ends.
            mirror.chat.stale = !mirror.loaders.is_empty();
        } else {
            mirror.reset(session_id);
            mirror.update_work(session_id);
        }
    }

    /// Tail `path` for `session_id` from its start; each event is applied
    /// only while the session's chat is still on `epoch`.
    fn start_tail(
        &self,
        session_id: &str,
        path: PathBuf,
        epoch: u64,
        plugin: &'static dyn AgentPlugin,
    ) -> TailHandle {
        debug!(session_id, path = %path.display(), epoch, "chat mirror: tailing the transcript");
        let weak = Arc::downgrade(&self.inner);
        let session_id = session_id.to_owned();
        transcript_tail::spawn(path, 0, self.inner.client_count.subscribe(), move |event| {
            if let Some(mirror) = Self::upgrade(&weak) {
                mirror.tail_event(&session_id, epoch, plugin, event);
            }
        })
    }

    fn tail_event(
        &self,
        session_id: &str,
        epoch: u64,
        plugin: &'static dyn AgentPlugin,
        event: TailEvent,
    ) {
        let facts = match &event {
            TailEvent::Line(line) => plugin.chat_line_facts(line),
            TailEvent::Restarted | TailEvent::CaughtUp => TranscriptLineFacts::default(),
        };
        let mut sessions = lock(&self.inner.sessions);
        let Some(mirror) = sessions
            .get_mut(session_id)
            .filter(|mirror| mirror.epoch == epoch)
        else {
            return;
        };
        match event {
            TailEvent::Line(line) => mirror.apply_line(session_id, plugin, &line, &facts),
            TailEvent::Restarted => {
                mirror.chat.restart();
                mirror.pending.drop_all();
                mirror.pending_rewind.clear();
            }
            TailEvent::CaughtUp => {
                if mirror.chat.catch_up() {
                    mirror.reset(session_id);
                }
                mirror.update_work(session_id);
                mirror.update_model(session_id);
            }
        }
    }

    /// Follow `session_id`'s live lines on a task of their own.
    fn start_live(&self, session_id: &str) -> tokio::task::AbortHandle {
        let receiver = self.inner.live_lines.subscribe(session_id);
        let follower = LiveFollower {
            mirror: Arc::downgrade(&self.inner),
            session_id: session_id.to_owned(),
            last_sent: None,
            flush_at: None,
            #[cfg(test)]
            _running: Running::enter(&self.inner.live_tasks),
        };
        tokio::spawn(follower.run(receiver)).abort_handle()
    }

    /// Merge `line` into `session_id`'s live answers, sending the changed
    /// ones now when `flush`.
    fn merge_live(&self, session_id: &str, line: LiveLine, flush: bool) {
        let mut sessions = lock(&self.inner.sessions);
        if let Some(mirror) = sessions.get_mut(session_id) {
            mirror.chat.merge_live(line);
            if flush {
                mirror.flush_live(session_id);
            }
        }
    }

    /// Send `session_id`'s changed live answers, or every one of them when
    /// `all` (after its live lines lagged).
    fn flush_live(&self, session_id: &str, all: bool) {
        let mut sessions = lock(&self.inner.sessions);
        if let Some(mirror) = sessions.get_mut(session_id) {
            if all {
                mirror.chat.mark_live_changed();
            }
            mirror.flush_live(session_id);
        }
    }

    /// Start the task that asks the summarizer for `session_id`'s stretch
    /// lines into `writer`, when a summarizer is set and none runs; a
    /// stopped writer keeps the lines it stored.
    fn start_writer(&self, session_id: &str, writer: &mut Option<SummaryWriter>) {
        if writer.as_ref().is_some_and(|writer| writer.jobs.is_some()) {
            return;
        }
        let Some(summarizer) = lock(&self.inner.summarizer).clone() else {
            return;
        };
        let (jobs, receiver) = mpsc::unbounded_channel();
        tokio::spawn(write_summaries(
            Arc::downgrade(&self.inner),
            session_id.to_owned(),
            summarizer,
            receiver,
        ));
        let writer = writer.get_or_insert_with(|| SummaryWriter {
            jobs: None,
            memo: HashMap::new(),
            queued: HashMap::new(),
            watched: None,
            run: 0,
            #[cfg(test)]
            pending: Arc::clone(&self.inner.summary_jobs),
        });
        writer.jobs = Some(jobs);
        writer.run += 1;
    }

    /// Whether `job` is still worth a request: `session_id`'s chat is on its
    /// epoch, its stretch's text is still the one it carries, and no line
    /// was stored for that text since it was queued.
    fn summary_due(&self, session_id: &str, job: &SummaryJob) -> bool {
        let mut sessions = lock(&self.inner.sessions);
        let Some(mirror) = sessions
            .get_mut(session_id)
            .filter(|mirror| mirror.epoch == job.epoch)
        else {
            return false;
        };
        let current = mirror.chat.stretch_digest(&job.stretch);
        let Some(writer) = mirror.writer.as_mut() else {
            return false;
        };
        let due = job.run == writer.run
            && writer.jobs.is_some()
            && current.as_deref() == Some(job.digest.as_str())
            && !writer.holds(&job.stretch, &job.digest);
        if !due {
            writer.dequeue(job);
        }
        due
    }

    /// Store `text` (`None` for a miss) as the line of `job`'s stretch and
    /// send the summary loaders the line if it changed.
    fn store_summary(&self, session_id: &str, job: &SummaryJob, text: Option<String>) {
        let mut sessions = lock(&self.inner.sessions);
        let Some(mirror) = sessions
            .get_mut(session_id)
            .filter(|mirror| mirror.epoch == job.epoch)
        else {
            return;
        };
        let Some(writer) = mirror.writer.as_mut() else {
            return;
        };
        writer.dequeue(job);
        writer
            .memo
            .insert(job.stretch.clone(), (job.digest.clone(), text));
        mirror.send_summary_change(session_id);
    }

    /// Start the one task that follows the registry for the mirrored
    /// sessions' conversation ids and removals, if it has not started. It
    /// subscribes before returning, so a change after this call is seen.
    fn follow_registry(&self) {
        if self.inner.following.swap(true, Ordering::SeqCst) {
            return;
        }
        let receiver = self.inner.registry.subscribe();
        tokio::spawn(follow_registry(Arc::downgrade(&self.inner), receiver));
    }

    /// `session_id`'s record now names `conversation_id`: a mirrored session
    /// whose chat reads another conversation switches.
    fn follow_conversation(&self, session_id: &str, conversation_id: Option<&str>) {
        let follows = lock(&self.inner.sessions)
            .get(session_id)
            .is_some_and(|mirror| {
                mirror.conversation.as_ref().map(|held| held.id.as_str()) != conversation_id
            });
        if follows {
            self.refresh(session_id);
        }
    }

    /// `session_id`'s record changed: a mirrored session takes the work and the
    /// hook's plan or question text the record now holds, and its loaders hear
    /// of a changed strip or card.
    fn follow_work(&self, session_id: &str) {
        if !lock(&self.inner.sessions).contains_key(session_id) {
            return;
        }
        let Some(record) = self.inner.registry.get(session_id) else {
            return;
        };
        let (work, plan, questions) = {
            let record = lock(&record);
            (
                record.work.clone(),
                pending_plan(&record),
                pending_questions(&record),
            )
        };
        let mut sessions = lock(&self.inner.sessions);
        if let Some(mirror) = sessions.get_mut(session_id) {
            mirror.take_work(session_id, work);
            mirror.take_hook(session_id, plan, questions);
        }
    }

    /// Read `session_id`'s record again and switch its chat when the record
    /// names another conversation, taking the work it holds; a record that
    /// is gone drops its mirror.
    fn refresh(&self, session_id: &str) {
        let Some(found) = Found::read(&self.inner.registry, session_id) else {
            self.drop_session(session_id);
            return;
        };
        let Some(plugin) = agent_registry::plugin(&found.agent) else {
            return;
        };
        let conversation = found.conversation(plugin);
        let mut sessions = lock(&self.inner.sessions);
        if let Some(mirror) = sessions.get_mut(session_id) {
            if mirror.conversation != conversation {
                self.switch(session_id, mirror, conversation);
            }
            mirror.take_work(session_id, found.work);
            mirror.take_hook(session_id, found.plan, found.questions);
        }
    }

    /// Read every mirrored session's record again, after events were lost.
    fn refresh_all(&self) {
        let session_ids: Vec<String> = lock(&self.inner.sessions).keys().cloned().collect();
        for session_id in session_ids {
            self.refresh(&session_id);
        }
    }

    fn drop_session(&self, session_id: &str) {
        lock(&self.inner.sessions).remove(session_id);
    }
}

/// Follow the registry for the mirror behind `weak` until the mirror is gone:
/// a conversation id change switches a mirrored chat, a removal drops it, and
/// lost events re-read every mirrored record.
async fn follow_registry(weak: Weak<Inner>, mut receiver: broadcast::Receiver<SessionEvent>) {
    loop {
        let received = receiver.recv().await;
        let Some(mirror) = ChatMirror::upgrade(&weak) else {
            return;
        };
        match received {
            Ok(SessionEvent::Updated(snapshot, _)) => {
                mirror.follow_conversation(&snapshot.id, snapshot.conversation_id.as_deref());
                mirror.follow_work(&snapshot.id);
            }
            Ok(SessionEvent::Removed(session_id)) => mirror.drop_session(&session_id),
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    skipped,
                    "chat mirror: session events lagged; re-reading the mirrored sessions"
                );
                // Subscribe before reading: every event after the re-read
                // applies on top of it.
                receiver = mirror.inner.registry.subscribe();
                mirror.refresh_all();
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// One session's live lines, merged into its chat: the first piece after a
/// quiet gap is sent at once, and the pieces after it at most every
/// [`LIVE_FLUSH_GAP`].
struct LiveFollower {
    mirror: Weak<Inner>,
    session_id: String,
    last_sent: Option<Instant>,
    flush_at: Option<Instant>,
    #[cfg(test)]
    _running: Running,
}

impl LiveFollower {
    async fn run(mut self, mut receiver: broadcast::Receiver<LiveLine>) {
        loop {
            let received = match self.flush_at {
                Some(at) => tokio::select! {
                    received = receiver.recv() => received,
                    () = tokio::time::sleep_until(at) => {
                        if !self.flush(false) {
                            return;
                        }
                        continue;
                    }
                },
                None => receiver.recv().await,
            };
            let followed = match received {
                Ok(line) => self.merge(line),
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(
                        session_id = %self.session_id,
                        skipped,
                        "chat mirror: live lines lagged; resending the live answers"
                    );
                    // The receiver goes on from the oldest line the channel
                    // still holds.
                    self.flush(true)
                }
                Err(broadcast::error::RecvError::Closed) => false,
            };
            if !followed {
                return;
            }
        }
    }

    /// Merge `line`, sending it now unless a send went out within the gap;
    /// false once the mirror is gone.
    fn merge(&mut self, line: LiveLine) -> bool {
        let Some(mirror) = ChatMirror::upgrade(&self.mirror) else {
            return false;
        };
        let now = Instant::now();
        let due = self.last_sent.is_none_or(|at| now >= at + LIVE_FLUSH_GAP);
        mirror.merge_live(&self.session_id, line, due);
        if due {
            self.last_sent = Some(now);
            self.flush_at = None;
        } else if let Some(at) = self.last_sent {
            self.flush_at.get_or_insert(at + LIVE_FLUSH_GAP);
        }
        true
    }

    /// Send the changed live answers, or all of them when `all`; false once
    /// the mirror is gone.
    fn flush(&mut self, all: bool) -> bool {
        let Some(mirror) = ChatMirror::upgrade(&self.mirror) else {
            return false;
        };
        mirror.flush_live(&self.session_id, all);
        self.last_sent = Some(Instant::now());
        self.flush_at = None;
        true
    }
}

/// One request for a stretch's line: the chat epoch and the stretch text it
/// was made for, and that text.
struct SummaryJob {
    epoch: u64,
    /// The worker run it was sent to.
    run: u64,
    stretch: String,
    digest: String,
    input: String,
}

/// What a session's summary writer is sent.
enum WriterMsg {
    /// Request this line now.
    Now(SummaryJob),
    /// The open stretch's text changed to this at this time: request it
    /// once it has stayed so for [`OPEN_QUIET`]. It replaces the open
    /// stretch sent before it.
    Quiet(SummaryJob, Instant),
}

/// A session's requests for its stretch lines and the lines that came back.
struct SummaryWriter {
    /// The worker's queue while a summary loader holds the mirror; the
    /// worker ends when it is dropped.
    jobs: Option<mpsc::UnboundedSender<WriterMsg>>,
    /// By stretch id, the digest of the text last requested and the line the
    /// summarizer wrote for it, `None` for a miss.
    memo: HashMap<String, (String, Option<String>)>,
    /// By stretch id, the digest of a request sent and not yet answered.
    queued: HashMap<String, String>,
    /// The open stretch and digest last sent to wait for quiet.
    watched: Option<(String, String)>,
    /// Which run of the worker `jobs` feeds: each start counts one, and a
    /// job of an earlier run is never requested.
    run: u64,
    #[cfg(test)]
    pending: Arc<std::sync::atomic::AtomicUsize>,
}

impl SummaryWriter {
    /// Forget every line and request, for a chat that starts over.
    fn clear(&mut self) {
        self.memo.clear();
        self.queued.clear();
        self.watched = None;
    }

    /// Stop the worker: its queue closes, and the requests waiting in it and
    /// the open stretch's timer go with it. The lines stored stay.
    fn stop(&mut self) {
        self.jobs = None;
        self.queued.clear();
        self.watched = None;
    }

    /// Whether `stretch`'s text with `digest` was requested and answered.
    fn holds(&self, stretch: &str, digest: &str) -> bool {
        self.memo
            .get(stretch)
            .is_some_and(|(held, _)| held == digest)
    }

    /// `job` is no longer waiting for its answer; a job of an earlier run
    /// was forgotten when that run stopped.
    fn dequeue(&mut self, job: &SummaryJob) {
        if job.run == self.run && self.queued.get(&job.stretch) == Some(&job.digest) {
            self.queued.remove(&job.stretch);
        }
    }

    /// Queue the requests `lines` call for, by `stretches` (id to digest and
    /// input) on chat epoch `epoch`: each closed stretch whose text has no
    /// answer yet at once, the open (last) one at once when `turn_ended` and
    /// after [`OPEN_QUIET`] otherwise. A `snapshot` asks only for the newest
    /// [`FIRST_READ_LIMIT`] closed stretches and marks older unanswered ones
    /// a miss.
    fn schedule(
        &mut self,
        epoch: u64,
        lines: &[StretchLine],
        stretches: &HashMap<String, (String, String)>,
        turn_ended: bool,
        snapshot: bool,
    ) {
        let Some((open, closed)) = lines.split_last() else {
            return;
        };
        for (newer, line) in closed.iter().rev().enumerate() {
            let Some((digest, input)) = stretches.get(&line.id) else {
                continue;
            };
            if snapshot && newer >= FIRST_READ_LIMIT {
                // A stretch already answered or queued keeps its entry.
                if !self.memo.contains_key(&line.id) && !self.queued.contains_key(&line.id) {
                    self.memo.insert(line.id.clone(), (digest.clone(), None));
                }
                continue;
            }
            self.request(epoch, &line.id, digest, input);
        }
        if let Some((digest, input)) = stretches.get(&open.id) {
            if turn_ended {
                self.request(epoch, &open.id, digest, input);
            } else {
                self.watch(epoch, &open.id, digest, input);
            }
        }
    }

    /// Send a request for `stretch`'s text unless it was answered, is
    /// waiting, or is empty, or the worker is stopped.
    fn request(&mut self, epoch: u64, stretch: &str, digest: &str, input: &str) {
        let Some(jobs) = &self.jobs else {
            return;
        };
        if input.is_empty()
            || self.holds(stretch, digest)
            || self.queued.get(stretch).is_some_and(|held| held == digest)
        {
            return;
        }
        let job = SummaryJob {
            epoch,
            run: self.run,
            stretch: stretch.to_owned(),
            digest: digest.to_owned(),
            input: input.to_owned(),
        };
        // The worker ends only once this sender is dropped.
        if jobs.send(WriterMsg::Now(job)).is_ok() {
            self.queued.insert(stretch.to_owned(), digest.to_owned());
            #[cfg(test)]
            self.pending.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Have the worker request the open `stretch` once its text has stayed
    /// the same for [`OPEN_QUIET`], unless that text was answered, is
    /// already watched, or is empty, or the worker is stopped.
    fn watch(&mut self, epoch: u64, stretch: &str, digest: &str, input: &str) {
        let Some(jobs) = &self.jobs else {
            return;
        };
        let watched = self
            .watched
            .as_ref()
            .is_some_and(|(id, held)| id == stretch && held == digest);
        if input.is_empty() || watched || self.holds(stretch, digest) {
            return;
        }
        let job = SummaryJob {
            epoch,
            run: self.run,
            stretch: stretch.to_owned(),
            digest: digest.to_owned(),
            input: input.to_owned(),
        };
        // The worker ends only once this sender is dropped.
        let _ = jobs.send(WriterMsg::Quiet(job, Instant::now()));
        self.watched = Some((stretch.to_owned(), digest.to_owned()));
    }

    /// Give `lines` the summarizer's text where it wrote one for the
    /// stretch's text as it stands, by `stretches` (id to digest and input).
    fn overlay(&self, lines: &mut [StretchLine], stretches: &HashMap<String, (String, String)>) {
        for line in lines {
            let Some((digest, _)) = stretches.get(&line.id) else {
                continue;
            };
            if let Some((held, Some(text))) = self.memo.get(&line.id)
                && held == digest
            {
                line.text.clone_from(text);
                line.source = SummarySource::Model;
            }
        }
    }
}

/// Ask `summarizer` for the lines `jobs` brings for `session_id`, one at a
/// time, until the mirror drops the queue; the open stretch's request waits
/// on this task's timer.
async fn write_summaries(
    weak: Weak<Inner>,
    session_id: String,
    summarizer: Arc<Summarizer>,
    mut jobs: mpsc::UnboundedReceiver<WriterMsg>,
) {
    let mut quiet: Option<(SummaryJob, Instant)> = None;
    loop {
        let received = match quiet.as_ref().map(|(_, at)| *at) {
            Some(at) => tokio::select! {
                received = jobs.recv() => received,
                () = tokio::time::sleep_until(at) => {
                    if let Some((job, _)) = quiet.take() {
                        #[cfg(test)]
                        count_job(&weak);
                        write_one(&weak, &session_id, &summarizer, job).await;
                    }
                    continue;
                }
            },
            None => jobs.recv().await,
        };
        match received {
            None => return,
            Some(WriterMsg::Now(job)) => write_one(&weak, &session_id, &summarizer, job).await,
            Some(WriterMsg::Quiet(job, changed_at)) => quiet = Some((job, changed_at + OPEN_QUIET)),
        }
    }
}

/// A fired quiet request is pending until written.
#[cfg(test)]
fn count_job(weak: &Weak<Inner>) {
    if let Some(inner) = weak.upgrade() {
        inner.summary_jobs.fetch_add(1, Ordering::SeqCst);
    }
}

/// Request `job`'s line if it is still due and store what came back: the
/// model's notification, or a miss for any other outcome.
async fn write_one(
    weak: &Weak<Inner>,
    session_id: &str,
    summarizer: &Summarizer,
    mut job: SummaryJob,
) {
    let due = ChatMirror::upgrade(weak).is_some_and(|mirror| mirror.summary_due(session_id, &job));
    if due {
        let request = Request {
            session_id: format!("{session_id}#summary"),
            trigger: Trigger::Stop,
            input: std::mem::take(&mut job.input),
        };
        let text = match summarizer.submit(request).await {
            Outcome::Line { line, .. } if line.source == SummarySource::Model => {
                Some(line.notification)
            }
            outcome => {
                debug!(session_id, stretch = %job.stretch, ?outcome, "chat mirror: no model line for the stretch; its template stays");
                None
            }
        };
        if let Some(mirror) = ChatMirror::upgrade(weak) {
            mirror.store_summary(session_id, &job, text);
        }
    }
    #[cfg(test)]
    if let Some(inner) = weak.upgrade() {
        inner.summary_jobs.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The SHA-256 of a stretch's input, in hex.
fn input_digest(input: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(64);
    for byte in Sha256::digest(input.as_bytes()) {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The plan `record` is waiting for approval on, as its hook's plan text, while
/// the session waits for an answer; `None` while it waits on anything else.
fn pending_plan(record: &SessionRecord) -> Option<String> {
    match (&record.status, &record.pending_input) {
        (SessionStatus::AwaitingInput, Some(PendingInput::PlanApproval { plan: Some(text) })) => {
            Some(text.clone())
        }
        _ => None,
    }
}

/// The questions `record` is waiting for answers to, as its hook carried them,
/// while the session waits; empty while it waits on anything else.
fn pending_questions(record: &SessionRecord) -> Vec<PendingQuestion> {
    match (&record.status, &record.pending_input) {
        (SessionStatus::AwaitingInput, Some(PendingInput::Question { questions })) => {
            questions.clone()
        }
        _ => Vec::new(),
    }
}

/// What a session's record says about its chat, read under the record's lock
/// and used after it is released.
struct Found {
    agent: AgentId,
    conversation_id: Option<String>,
    folder: Option<String>,
    extra_env: Vec<(String, String)>,
    screen: Option<SharedScreen>,
    work: BackgroundWork,
    plan: Option<String>,
    questions: Vec<PendingQuestion>,
}

impl Found {
    fn read(registry: &SessionRegistry, session_id: &str) -> Option<Self> {
        let record = registry.get(session_id)?;
        let record = lock(&record);
        Some(Self {
            agent: record.agent.clone(),
            conversation_id: record.conversation_id.clone(),
            folder: crate::history::primary_cwd(&record),
            // A session spawned with no config set no environment rows.
            extra_env: record
                .spawn_config
                .as_ref()
                .map(|config| config.extra_env.clone())
                .unwrap_or_default(),
            screen: record.screen.clone(),
            work: record.work.clone(),
            plan: pending_plan(&record),
            questions: pending_questions(&record),
        })
    }

    /// The conversation the record names, with the transcript `plugin` keeps
    /// it in.
    fn conversation(&self, plugin: &dyn AgentPlugin) -> Option<Conversation> {
        let id = self.conversation_id.clone()?;
        let path = self.folder.as_deref().and_then(|folder| {
            let rows = crate::conversations::lookup_rows(&self.extra_env);
            plugin.transcript_path(Path::new(folder), &id, &rows)
        });
        Some(Conversation { id, path })
    }
}

/// One connection that loaded a session's chat, its summary, or both.
struct Loader {
    out: Outbox,
    /// The place in the chat's entries of the oldest entry the loader holds:
    /// the first one its last `Chat` carried. A replacement of an entry
    /// before it is not sent to the loader.
    floor: usize,
    /// Whether the loader is sent the chat's messages.
    chat: bool,
    /// Whether the loader is sent the summary's messages.
    summary: bool,
}

impl Loader {
    fn loads_any(&self) -> bool {
        self.chat || self.summary
    }
}

/// A Restore the chat answered and the transcript has not confirmed yet: the
/// message the restore goes back to before, and whether a user prompt arrived
/// since it was set.
#[derive(Default)]
struct PendingRewind {
    /// The message text the restore's choice screen named.
    message: Option<String>,
    /// Whether a user prompt arrived since: the restore's own turn is the
    /// first, and a second one means the turn between them passed without
    /// the rewind's record, so the restore never happened.
    prompted: bool,
}

impl PendingRewind {
    /// Record that a Restore the chat answered goes back to before `message`.
    fn note(&mut self, message: String) {
        self.message = Some(message);
        self.prompted = false;
    }

    /// The pending message, cleared.
    fn take(&mut self) -> Option<String> {
        self.prompted = false;
        self.message.take()
    }

    /// A user prompt arrived: the first since the pending was set is the
    /// restore's own turn. The second means that turn ended with no rewind's
    /// record, so the restore never happened and the pending is dropped.
    /// Returns whether one was dropped.
    fn saw_prompt(&mut self) -> bool {
        if self.message.is_none() {
            return false;
        }
        if self.prompted {
            self.clear();
            return true;
        }
        self.prompted = true;
        false
    }

    /// Forget the pending restore.
    fn clear(&mut self) {
        self.message = None;
        self.prompted = false;
    }
}

/// One mirrored session.
struct SessionMirror {
    plugin: &'static dyn AgentPlugin,
    /// Each loader, by connection.
    loaders: HashMap<u64, Loader>,
    /// The stretch lines as the summary loaders were last sent them, by id.
    summary_sent: HashMap<String, StretchLine>,
    conversation: Option<Conversation>,
    /// Set anew from the daemon-wide count by each switch; a tail event of
    /// any other epoch is dropped. Zero, which no tail carries, until the
    /// first switch.
    epoch: u64,
    tail: Option<TailHandle>,
    chat: ChatState,
    /// The messages chat sends typed whose transcript record has not
    /// arrived.
    pending: PendingSends,
    /// A Restore the chat answered and the transcript has not confirmed yet.
    pending_rewind: PendingRewind,
    screen: ScreenState,
    live_task: tokio::task::AbortHandle,
    /// The work the session's record holds, as its agent's last `Stop`
    /// listed it.
    work: BackgroundWork,
    /// The plan the session's record waits for approval on, from its hook.
    pending_plan: Option<String>,
    /// The questions the session's record waits for answers to, from its
    /// hook.
    pending_questions: Vec<PendingQuestion>,
    /// The ids of the tasks whose task notification the session's
    /// transcripts hold, kept across a conversation switch since task ids
    /// are unique across conversations.
    ended_tasks: HashSet<String>,
    /// The running work the screen counted at its last reading that showed
    /// a count.
    screen_work: Option<ScreenWork>,
    /// The `ChatWork` the loaders were last sent.
    work_shown: DaemonMessage,
    /// The model the transcript's newest main-thread reply names; none
    /// since the last conversation switch until a reply names one.
    model: Option<String>,
    /// The model the chat loaders were last sent in a `ChatModel`.
    model_shown: Option<String>,
    /// The effort the transcript's newest main-thread reply answers with;
    /// none since the last conversation switch until a reply names one.
    effort: Option<String>,
    /// The effort the chat loaders were last sent in a `ChatModel`.
    effort_shown: Option<String>,
    /// The label of the model switch the transcript's newest main-thread
    /// command names, which no reply has replaced yet; the switch names no
    /// model id, so the pill draws the label until a reply names one.
    switched: Option<String>,
    /// The switched label the chat loaders were last sent in a `ChatModel`.
    switched_shown: Option<String>,
    /// The requests for the stretch lines and their answers, from the first
    /// summary load with a summarizer set.
    writer: Option<SummaryWriter>,
}

impl Drop for SessionMirror {
    fn drop(&mut self) {
        self.live_task.abort();
    }
}

impl SessionMirror {
    fn new(
        session_id: &str,
        plugin: &'static dyn AgentPlugin,
        live_task: tokio::task::AbortHandle,
    ) -> Self {
        Self {
            plugin,
            loaders: HashMap::new(),
            summary_sent: HashMap::new(),
            conversation: None,
            epoch: 0,
            tail: None,
            chat: ChatState::default(),
            pending: PendingSends::default(),
            pending_rewind: PendingRewind::default(),
            screen: ScreenState::default(),
            live_task,
            work: BackgroundWork::default(),
            pending_plan: None,
            pending_questions: Vec::new(),
            ended_tasks: HashSet::new(),
            screen_work: None,
            work_shown: DaemonMessage::ChatWork {
                session_id: session_id.to_owned(),
                subagents: Vec::new(),
                shells: Vec::new(),
                wakeup_at: None,
            },
            model: None,
            model_shown: None,
            effort: None,
            effort_shown: None,
            switched: None,
            switched_shown: None,
            writer: None,
        }
    }

    /// Apply one transcript line: send each loader what it changed once the
    /// first read is over, and the strip when the line ended a task.
    fn apply_line(
        &mut self,
        session_id: &str,
        plugin: &dyn AgentPlugin,
        line: &str,
        facts: &TranscriptLineFacts,
    ) {
        self.ended_tasks.extend(facts.ended_tasks.iter().cloned());
        // Kept while the first read catches up too; sent once it ends.
        let model_moved = self.take_model_facts(facts);
        let mut change = self.chat.apply_line(plugin, line, facts);
        if facts.rewound {
            self.apply_rewind(session_id);
        } else if facts.is_user_prompt && self.pending_rewind.saw_prompt() {
            debug!(
                session_id,
                "chat mirror: no rewind's record came; the pending restore is dropped"
            );
        }
        // A line applied while the first read catches up is an old one,
        // which lands no pending entry.
        if !self.chat.catching_up {
            self.pending.clear_landed(&mut change);
        }
        let changed = !(change.entries.is_empty() && change.removed.is_empty());
        if self.chat.catching_up {
            // Sent whole when the read ends, the strip with it.
            self.chat.stale |= changed;
            return;
        }
        if changed {
            self.send_change(session_id, &change);
        }
        if !facts.ended_tasks.is_empty() {
            self.update_work(session_id);
        }
        if model_moved {
            self.update_model(session_id);
        }
    }

    /// The transcript's record confirmed the pending Restore: drop the turns
    /// the restore went back past, and send the loaders the chat whole, since
    /// it is no longer the one they hold. A read still catching up sends it at
    /// its end, as a restart does.
    fn apply_rewind(&mut self, session_id: &str) {
        let Some(message) = self.pending_rewind.take() else {
            return;
        };
        if !self.chat.rewind(&message) {
            return;
        }
        debug!(
            session_id,
            message, "chat mirror: the chat goes back to the restored turn"
        );
        // The loaders are sent the kept stretch lines again with the whole
        // chat; the writer keeps the lines it already wrote for them, so only
        // the stretches a dropped turn touched are asked for again.
        self.summary_sent.clear();
        if self.chat.catching_up {
            return;
        }
        self.chat.stale = false;
        self.reset(session_id);
    }

    /// Take the model facts `facts` carries: a main-thread reply names its
    /// model, the effort it answered with, and replaces any switched label; a
    /// switch names its label and the effort it moved to, with the model id to
    /// come at the next reply. Whether the line moved the model.
    fn take_model_facts(&mut self, facts: &TranscriptLineFacts) -> bool {
        if let Some(model) = &facts.model {
            self.model = Some(model.clone());
            self.effort.clone_from(&facts.effort);
            // The reply's own model replaces a switched label and its effort.
            self.switched = None;
        } else if let Some(switch) = &facts.model_switch {
            // The switch names only a label; the reply after it brings the
            // model id and the effort it answers with.
            self.model = None;
            self.switched = Some(switch.label.clone());
            self.effort.clone_from(&switch.effort);
        }
        facts.model.is_some() || facts.model_switch.is_some()
    }

    /// Send the model, its label and its effort to every chat loader when any
    /// of them changed since they were last sent it.
    fn update_model(&mut self, session_id: &str) {
        if self.model != self.model_shown
            || self.effort != self.effort_shown
            || self.switched != self.switched_shown
        {
            self.model_shown.clone_from(&self.model);
            self.effort_shown.clone_from(&self.effort);
            self.switched_shown.clone_from(&self.switched);
            self.tell(&self.model_message(session_id));
        }
    }

    /// The `ChatModel` a new loader of `kind` is sent after its reply and
    /// the strip: a chat loader's only, once a model or a switch was shown.
    fn model_on_load(&self, kind: LoadKind, session_id: &str) -> Option<DaemonMessage> {
        let shown = self.model_shown.is_some() || self.switched_shown.is_some();
        (kind == LoadKind::Chat && shown).then(|| self.model_message(session_id))
    }

    /// The `ChatModel` of the model, label and effort the chat loaders were
    /// last sent: a switched label as the agent printed it, else the model's
    /// label as the session's agent formats it.
    fn model_message(&self, session_id: &str) -> DaemonMessage {
        let label = match &self.switched_shown {
            Some(label) => Some(label.clone()),
            None => self
                .model_shown
                .as_deref()
                .map(|model| self.plugin.model_label(model)),
        };
        DaemonMessage::ChatModel {
            session_id: session_id.to_owned(),
            model: self.model_shown.clone(),
            label,
            effort: self.effort_shown.clone(),
            switch_command: self.plugin.model_switch_command().map(str::to_owned),
        }
    }

    /// Take `work` as what the record holds, telling the loaders of a
    /// changed strip.
    fn take_work(&mut self, session_id: &str, work: BackgroundWork) {
        if self.work != work {
            self.work = work;
            self.update_work(session_id);
        }
    }

    /// Take `plan` and `questions` as what the record's hook holds, filling a
    /// plan prompt with no body of its own or a question prompt's text, and
    /// telling the loaders of a changed card.
    fn take_hook(
        &mut self,
        session_id: &str,
        plan: Option<String>,
        questions: Vec<PendingQuestion>,
    ) {
        if self.pending_plan == plan && self.pending_questions == questions {
            return;
        }
        self.pending_plan = plan;
        self.pending_questions = questions;
        if self
            .screen
            .refill(self.pending_plan.as_deref(), &self.pending_questions)
        {
            self.tell(&DaemonMessage::ChatScreenChanged {
                session_id: session_id.to_owned(),
                screen: self.screen.shown.clone(),
            });
        }
    }

    /// Take `counted` as the screen's count, telling the loaders of a changed
    /// strip.
    fn take_screen_work(&mut self, session_id: &str, counted: ScreenWork) {
        if self.screen_work != Some(counted) {
            self.screen_work = Some(counted);
            self.update_work(session_id);
        }
    }

    /// Work out the strip again and send it to every loader when it changed.
    fn update_work(&mut self, session_id: &str) {
        let work = chat_work(session_id, &self.work, &self.ended_tasks, self.screen_work);
        if !same_work(&work, &self.work_shown) {
            self.tell_all(&work);
            self.work_shown = work;
        }
    }

    /// Send `msg` once to every loader, whatever it loaded.
    fn tell_all(&self, msg: &DaemonMessage) {
        for loader in self.loaders.values().filter(|loader| loader.loads_any()) {
            // A closed outbox is a connection already ending; its guard
            // unloads it.
            let _ = loader.out.send(msg.clone());
        }
    }

    /// Register `connection`, whose messages go to `out_tx`, as a loader of
    /// what `kind` names, and return its reply carrying `request_id`.
    fn register(
        &mut self,
        kind: LoadKind,
        session_id: &str,
        request_id: Option<String>,
        connection: u64,
        out_tx: &Outbox,
    ) -> DaemonMessage {
        let floor = self.chat.reply_floor(self.pending.len());
        let reply = match kind {
            LoadKind::Chat => self.chat_message(session_id, request_id),
            LoadKind::Summary => self.summary_message(session_id, request_id),
        };
        let loader = self.loaders.entry(connection).or_insert_with(|| Loader {
            out: out_tx.clone(),
            floor,
            chat: false,
            summary: false,
        });
        match kind {
            LoadKind::Chat => {
                loader.chat = true;
                loader.floor = floor;
            }
            LoadKind::Summary => loader.summary = true,
        }
        reply
    }

    /// Send `msg` to every chat loader.
    fn tell(&self, msg: &DaemonMessage) {
        for loader in self.loaders.values().filter(|loader| loader.chat) {
            // A closed outbox is a connection already ending; its guard
            // unloads it.
            let _ = loader.out.send(msg.clone());
        }
    }

    /// Send `msg` to every summary loader.
    fn tell_summary(&self, msg: &DaemonMessage) {
        for loader in self.loaders.values().filter(|loader| loader.summary) {
            // A closed outbox is a connection already ending; its guard
            // unloads it.
            let _ = loader.out.send(msg.clone());
        }
    }

    fn has_summary_loader(&self) -> bool {
        self.loaders.values().any(|loader| loader.summary)
    }

    /// With no summary loader left, stop asking the summarizer for lines;
    /// the next summary load starts again.
    fn stop_writer_unless_loaded(&mut self) {
        if !self.has_summary_loader()
            && let Some(writer) = self.writer.as_mut()
        {
            writer.stop();
        }
    }

    /// Send every chat loader the whole chat and every summary loader every
    /// stretch line as a fresh start, which moves each chat loader's floor
    /// to the reply's first entry.
    fn reset(&mut self, session_id: &str) {
        let msg = self.chat_message(session_id, None);
        // The fresh start leaves out the pending entries already dropped.
        self.pending.take_dropped();
        let floor = self.chat.reply_floor(self.pending.len());
        for loader in self.loaders.values_mut().filter(|loader| loader.chat) {
            loader.floor = floor;
        }
        self.tell(&msg);
        if self.has_summary_loader() {
            let summary = self.summary_message(session_id, None);
            self.tell_summary(&summary);
        }
    }

    /// Send each chat loader what one transcript line changed: its new
    /// entries, its replacements of entries the loader holds, and the live
    /// ids that left; a loader with nothing to hear of it is sent nothing.
    /// Then send the summary loaders the stretch lines it changed.
    fn send_change(&mut self, session_id: &str, change: &Change) {
        self.send_chat_change(session_id, change);
        self.send_summary_change(session_id);
    }

    fn send_chat_change(&self, session_id: &str, change: &Change) {
        for loader in self.loaders.values().filter(|loader| loader.chat) {
            let entries: Vec<ChatEntry> = change
                .entries
                .iter()
                .zip(&change.replaces)
                .filter(|(_, at)| at.is_none_or(|at| at >= loader.floor))
                .map(|(entry, _)| entry.clone())
                .collect();
            if entries.is_empty() && change.removed.is_empty() {
                continue;
            }
            // A closed outbox is a connection already ending; its guard
            // unloads it.
            let _ = loader.out.send(DaemonMessage::ChatAppend {
                session_id: session_id.to_owned(),
                entries,
                removed: change.removed.clone(),
            });
        }
    }

    /// Send the summary loaders the stretch lines whose content changed since
    /// they were last sent, in one `SummaryAppend`; nothing when none did.
    fn send_summary_change(&mut self, session_id: &str) {
        if !self.has_summary_loader() {
            return;
        }
        let changed: Vec<StretchLine> = self
            .shown_lines(false)
            .into_iter()
            .filter(|line| self.summary_sent.get(&line.id) != Some(line))
            .collect();
        if changed.is_empty() {
            return;
        }
        for line in &changed {
            self.summary_sent.insert(line.id.clone(), line.clone());
        }
        self.tell_summary(&DaemonMessage::SummaryAppend {
            session_id: session_id.to_owned(),
            lines: changed,
        });
    }

    /// Every stretch line as one `Summary` message, which the summary
    /// loaders then hold: a reply when `request_id` is set, a fresh start
    /// for the summary loaders when it is not.
    fn summary_message(&mut self, session_id: &str, request_id: Option<String>) -> DaemonMessage {
        let lines = self.shown_lines(true);
        self.summary_sent = lines
            .iter()
            .map(|line| (line.id.clone(), line.clone()))
            .collect();
        DaemonMessage::Summary {
            session_id: session_id.to_owned(),
            lines,
            request_id,
        }
    }

    /// The stretch lines as the summary loaders are shown them, each with
    /// the summarizer's text where it wrote one for the stretch's text as it
    /// stands, after queuing the requests the lines call for (none while a
    /// read is being caught up with). `snapshot` marks a whole summary,
    /// whose requests keep to the newest closed stretches.
    fn shown_lines(&mut self, snapshot: bool) -> Vec<StretchLine> {
        let mut lines = self.chat.stretch_lines();
        let Some(writer) = self.writer.as_mut() else {
            return lines;
        };
        let stretches: HashMap<String, (String, String)> = self
            .chat
            .stretch_inputs()
            .into_iter()
            .map(|(id, input)| (id, (input_digest(&input), input)))
            .collect();
        if !self.chat.catching_up {
            let turn_ended = lines.last().is_some_and(|line| {
                matches!(
                    self.chat.get(&line.to_entry),
                    Some(ChatEntry::TurnEnd { .. })
                )
            });
            writer.schedule(self.epoch, &lines, &stretches, turn_ended, snapshot);
        }
        writer.overlay(&mut lines, &stretches);
        lines
    }

    /// The whole chat as one `Chat` message: a reply when `request_id` is
    /// set, a fresh start for the loaders when it is not.
    fn chat_message(&self, session_id: &str, request_id: Option<String>) -> DaemonMessage {
        let (entries, hidden) = self.chat.reply_with(&self.pending.entries);
        DaemonMessage::Chat {
            session_id: session_id.to_owned(),
            entries,
            hidden,
            screen: self.screen.shown.clone(),
            request_id,
        }
    }

    /// Take `reading` as the screen, telling the loaders when it changed
    /// more than its cursor.
    fn observe(&mut self, session_id: &str, reading: ChatScreen) {
        if self.screen.observe(
            reading,
            self.pending_plan.as_deref(),
            &self.pending_questions,
        ) {
            self.tell(&DaemonMessage::ChatScreenChanged {
                session_id: session_id.to_owned(),
                screen: self.screen.shown.clone(),
            });
        }
    }

    /// Merge the live cards the screen shows, sending them when any
    /// changed.
    fn take_live_cards(&mut self, session_id: &str, cards: Vec<ChatEntry>) {
        if self.chat.merge_cards(cards) {
            self.flush_live(session_id);
        }
    }

    fn flush_live(&mut self, session_id: &str) {
        let entries = self.chat.take_changed_live();
        if !entries.is_empty() {
            let removed = self.pending.take_dropped();
            self.tell(&DaemonMessage::ChatAppend {
                session_id: session_id.to_owned(),
                entries,
                removed,
            });
        }
    }
}

/// The status watcher's hand-off of each screen it reads to the chat mirror.
#[derive(Clone)]
pub struct ChatTap {
    mirror: ChatMirror,
    session_id: String,
    agent: AgentId,
}

impl ChatTap {
    /// Give the mirror `screen`, read under its grid lock: a session nobody
    /// loaded costs a lookup and nothing is parsed; a loaded one's loaders
    /// hear of a change.
    pub fn observe(&self, screen: &Screen) {
        let mut sessions = lock(&self.mirror.inner.sessions);
        let Some(mirror) = sessions.get_mut(&self.session_id) else {
            return;
        };
        if let Some(reading) = dialogs::read(&self.agent, screen) {
            mirror.observe(&self.session_id, reading);
        }
        // A frame that shows no count (a dialog is up) keeps the last one.
        if let Some(counted) = dialogs::read_work(&self.agent, screen) {
            mirror.take_screen_work(&self.session_id, counted);
        }
        let cards = dialogs::read_live_cards(&self.agent, screen);
        // Reading the view costs a parse: only cards the chat would take
        // are worth it.
        if !mirror.chat.would_take(&cards) {
            return;
        }
        // A subagent's view shows its own work, not the main conversation's.
        let subagent_in_view = dialogs::read_view(&self.agent, screen)
            .is_some_and(|view| view.agent_in_view.is_some());
        if !subagent_in_view {
            mirror.take_live_cards(&self.session_id, cards);
        }
    }
}

/// Whether two `ChatWork` messages show the same strip.
fn same_work(a: &DaemonMessage, b: &DaemonMessage) -> bool {
    match (a, b) {
        (
            DaemonMessage::ChatWork {
                subagents,
                shells,
                wakeup_at,
                ..
            },
            DaemonMessage::ChatWork {
                subagents: other_subagents,
                shells: other_shells,
                wakeup_at: other_wakeup,
                ..
            },
        ) => subagents == other_subagents && shells == other_shells && wakeup_at == other_wakeup,
        _ => false,
    }
}

/// Unloads every chat its connection loaded when dropped, on every way out
/// of the connection.
pub struct ChatLoadGuard {
    mirror: ChatMirror,
    connection: u64,
}

impl Drop for ChatLoadGuard {
    fn drop(&mut self) {
        self.mirror.connection_closed(self.connection);
    }
}

/// The screen a session's chat shows, and the id of the prompt on it.
struct ScreenState {
    /// The last reading as the agent's plugin read it, before the plan fill.
    reading: ChatScreen,
    shown: ChatScreen,
    /// What makes the prompt on screen the same prompt, while one is.
    identity: Option<u64>,
    prompt_id: String,
}

impl Default for ScreenState {
    fn default() -> Self {
        Self {
            reading: ChatScreen {
                composer: ComposerState::Unknown,
                prompt: None,
            },
            shown: ChatScreen {
                composer: ComposerState::Unknown,
                prompt: None,
            },
            identity: None,
            prompt_id: String::new(),
        }
    }
}

impl ScreenState {
    /// Take `reading` as the screen, giving its prompt the id the same prompt
    /// had (a new one for a new prompt), and show it with the hook's `plan` and
    /// `questions` filled in; whether what the loaders see changed more than the
    /// prompt's cursor.
    fn observe(
        &mut self,
        mut reading: ChatScreen,
        plan: Option<&str>,
        questions: &[PendingQuestion],
    ) -> bool {
        match reading.prompt.as_mut() {
            Some(prompt) => {
                let identity = dialogs::prompt_identity(prompt);
                if self.identity != Some(identity) {
                    self.identity = Some(identity);
                    self.prompt_id = format!("p{}", NEXT_PROMPT.fetch_add(1, Ordering::Relaxed));
                }
                prompt.id.clone_from(&self.prompt_id);
            }
            None => self.identity = None,
        }
        let changed = self.show_filled(&reading, plan, questions);
        self.reading = reading;
        changed
    }

    /// Show the last reading again with the hook's `plan` and `questions`
    /// filled in; whether what the loaders see changed more than the prompt's
    /// cursor.
    fn refill(&mut self, plan: Option<&str>, questions: &[PendingQuestion]) -> bool {
        let reading = self.reading.clone();
        self.show_filled(&reading, plan, questions)
    }

    /// Show `reading` with the hook's `plan` and `questions` filled in;
    /// whether what the loaders see changed more than the prompt's cursor.
    fn show_filled(
        &mut self,
        reading: &ChatScreen,
        plan: Option<&str>,
        questions: &[PendingQuestion],
    ) -> bool {
        let shown = filled(reading, plan, questions);
        let changed = !same_but_cursor(&shown, &self.shown);
        self.shown = shown;
        changed
    }
}

/// `reading` with the hook's text drawn in: a plan prompt whose own body is
/// empty takes the plan text as its body and, with no subject of its own, the
/// plan's first non-blank line, without a Markdown heading's marks, as its
/// subject; a question prompt takes the hook question it asks ([`card_asks`])
/// whole as its subject, and that question's descriptions
/// ([`fill_descriptions`]). Every other reading is shown as it is.
fn filled(reading: &ChatScreen, plan: Option<&str>, questions: &[PendingQuestion]) -> ChatScreen {
    let mut screen = reading.clone();
    let Some(prompt) = screen.prompt.as_mut() else {
        return screen;
    };
    if prompt.kind == ChatPromptKind::Plan
        && prompt.body.is_empty()
        && let Some(plan) = plan
    {
        plan.clone_into(&mut prompt.body);
        if prompt.subject.trim().is_empty()
            && let Some(line) = plan
                .lines()
                .map(|line| heading_text(line).trim())
                .find(|line| !line.is_empty())
        {
            line.clone_into(&mut prompt.subject);
        }
    }
    if prompt.kind == ChatPromptKind::Question
        && let Some(asked) = questions
            .iter()
            .find(|asked| card_asks(prompt, &asked.question))
    {
        asked.question.clone_into(&mut prompt.subject);
        fill_descriptions(prompt, asked);
    }
    screen
}

/// `line` without the Markdown heading it opens with: a run of `#`s, up to
/// three spaces in (a tab is not one of those spaces), followed by whitespace
/// or by nothing else. A line whose marks begin a word ("#hashtag") or that is
/// indented like code keeps them.
fn heading_text(line: &str) -> &str {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return line;
    }
    let rest = &line[indent..];
    let after = rest.trim_start_matches('#');
    if after.len() == rest.len() {
        return line;
    }
    if after.is_empty() {
        return "";
    }
    if after.starts_with([' ', '\t']) {
        return after.trim_start_matches([' ', '\t']);
    }
    line
}

/// Whether the question card `prompt` asks `asked`, whitespace aside: a card
/// read with its tab bar shows the whole question, one read without it the
/// rows the grid still shows, which are the question's end. The screen joins
/// a token hard-wrapped over two rows with a space, which this ignores.
fn card_asks(prompt: &ChatPrompt, asked: &str) -> bool {
    if prompt.questions.is_empty() {
        return shows_the_end_of(&prompt.subject, asked);
    }
    let shown = without_whitespace(&prompt.subject);
    !shown.is_empty() && shown == without_whitespace(asked)
}

/// Give each of `prompt`'s options that shows a description the description
/// `asked` holds for the option with the same label, when the two match
/// whitespace aside: the hook's is the text unwrapped.
fn fill_descriptions(prompt: &mut ChatPrompt, asked: &PendingQuestion) {
    for option in &mut prompt.options {
        let Some(shown) = option.description.as_mut() else {
            continue;
        };
        let label = without_whitespace(&option.label);
        let hook = asked
            .options
            .iter()
            .zip(&asked.option_descriptions)
            .find(|(asked_label, _)| without_whitespace(asked_label) == label)
            .map(|(_, description)| description);
        if let Some(description) = hook
            && !description.is_empty()
            && without_whitespace(description) == without_whitespace(shown)
        {
            description.clone_into(shown);
        }
    }
}

/// Whether `shown` is the end of `asked` once whitespace is dropped from both:
/// the rows a dialog read without its tab bar still shows are the tail of the
/// question it asks, hard-wrapped mid-token where a token outgrew a row.
pub(crate) fn shows_the_end_of(shown: &str, asked: &str) -> bool {
    let shown = without_whitespace(shown);
    !shown.is_empty() && without_whitespace(asked).ends_with(shown.as_str())
}

/// `text` with all whitespace removed: a token longer than a row is hard-wrapped
/// on screen, so joining the rows puts a space inside it.
pub(crate) fn without_whitespace(text: &str) -> String {
    text.split_whitespace().collect()
}

fn same_but_cursor(a: &ChatScreen, b: &ChatScreen) -> bool {
    // Outside a multi-select question an option is selected where the
    // cursor is, so its mark moves with the cursor. A model picker keeps
    // both: its card fills the highlighted row. So does a question whose
    // options carry previews: its card shows the cursor's option's preview.
    // So does an overlay offering options: its card fills the cursor's row.
    let without_cursor = |prompt: &ChatPrompt| {
        let fills_row = prompt.kind == ChatPromptKind::ModelPicker
            || prompt.preview.is_some()
            || (prompt.kind == ChatPromptKind::Overlay && prompt.options.len() > 1);
        if fills_row {
            return prompt.clone();
        }
        let mut prompt = prompt.clone();
        prompt.cursor = None;
        if !prompt.multi {
            for option in &mut prompt.options {
                option.selected = false;
            }
        }
        prompt
    };
    a.composer == b.composer
        && a.prompt.as_ref().map(without_cursor) == b.prompt.as_ref().map(without_cursor)
}

/// A chat's entries in transcript order, and its answers still arriving.
/// The default is a chat with nothing left to read.
#[derive(Default)]
struct ChatState {
    /// The row the chat starts with, kept when the transcript restarts.
    head: Option<ChatEntry>,
    entries: Vec<ChatEntry>,
    /// Each entry's place in `entries`, by id.
    index: HashMap<String, usize>,
    /// Each entry's time, by id: the time of the transcript line that first
    /// wrote it, kept through its replacements.
    times: HashMap<String, Option<DateTime<Utc>>>,
    /// The ids of the entries that began a turn: each prompt line's first
    /// entry, a user message or a row such as a `!` command's.
    turn_starts: HashSet<String>,
    /// The id of the prompt whose turn the transcript is in.
    turn_prompt: Option<String>,
    /// How many answers the transcript wrote in each prompt's turn.
    answered: HashMap<String, usize>,
    /// Whether the transcript's first read (or the read after a restart) is
    /// still being applied: its lines are not sent one by one, and live
    /// lines wait in `held`, since their place in the turn is known only
    /// once that read's answers are counted.
    catching_up: bool,
    /// The live lines that arrived while catching up, in arrival order.
    held: Vec<LiveLine>,
    /// Whether the loaders show a chat this one no longer extends, so the
    /// end of the read sends it whole.
    stale: bool,
    /// The answers still arriving and the cards of work the screen shows
    /// running before the transcript holds it, in the order each first
    /// came.
    live: Vec<LiveItem>,
    /// The place the next live message of each prompt's turn takes: the
    /// turn's answers already written when its first live message came, then
    /// one more per live message.
    live_next: HashMap<String, usize>,
    /// The keys of live messages whose transcript answer has landed.
    finished: HashSet<String>,
    /// The prompts whose turn a later prompt ended; their live pieces are
    /// dropped.
    ended_prompts: HashSet<String>,
    /// The keys of live messages and cards changed since they were last
    /// sent.
    changed: HashSet<String>,
    /// The eval number and description of each script the transcript has
    /// carried; the screen's card of one is not taken. Both, since a new
    /// agent process numbers its scripts anew.
    landed_scripts: HashSet<(u32, String)>,
    /// The eval number and description of each live card that left; the
    /// screen's card of one is not taken again.
    ended_cards: HashSet<(u32, String)>,
    /// What the plugin keeps between the transcript's lines (such as which
    /// script a later call belongs to); made by the plugin on the first line
    /// read, and gone with the rest of the chat when the transcript restarts
    /// or the session switches conversation.
    context: Option<Box<dyn Any + Send>>,
}

/// One answer still arriving, or one card of work the screen shows running,
/// in its prompt's turn.
struct LiveItem {
    key: String,
    prompt: String,
    body: LiveBody,
}

/// What a live item holds.
enum LiveBody {
    /// An answer's pieces by their place in the message, and its place among
    /// the messages of its prompt's turn.
    Answer {
        ordinal: usize,
        pieces: BTreeMap<u64, String>,
    },
    /// A card the screen shows running, and its script's eval number.
    Card { eval: u32, entry: Box<ChatEntry> },
}

impl LiveItem {
    /// The id the item is sent under: `live:` and its key for an answer,
    /// the card's own id for a card.
    fn id(&self) -> String {
        match &self.body {
            LiveBody::Answer { .. } => format!("live:{}", self.key),
            LiveBody::Card { .. } => self.key.clone(),
        }
    }

    /// The item as it stands: an answer's pieces in index order with nothing
    /// between them, since each piece carries its own line breaks; a card as
    /// the screen showed it last.
    fn entry(&self) -> ChatEntry {
        match &self.body {
            LiveBody::Answer { pieces, .. } => ChatEntry::Answer {
                id: self.id(),
                text: pieces.values().map(String::as_str).collect(),
                live: true,
            },
            LiveBody::Card { entry, .. } => entry.as_ref().clone(),
        }
    }

    /// The eval number of a card's script; `None` for an answer.
    fn card_eval(&self) -> Option<u32> {
        match self.body {
            LiveBody::Card { eval, .. } => Some(eval),
            LiveBody::Answer { .. } => None,
        }
    }

    /// The eval number and description of a card's script; `None` for an
    /// answer.
    fn card_script(&self) -> Option<(u32, String)> {
        match &self.body {
            LiveBody::Card { entry, .. } => card_script(entry),
            LiveBody::Answer { .. } => None,
        }
    }
}

/// What one transcript line changed: the entries to send, new or replacing,
/// and the live entries that left.
#[derive(Default)]
struct Change {
    entries: Vec<ChatEntry>,
    /// For each of `entries`, the place of the entry it replaces; `None` for
    /// a new one.
    replaces: Vec<Option<usize>>,
    removed: Vec<String>,
}

impl ChatState {
    /// A chat opening with `head`, `catching_up` while a transcript's first
    /// read is still to be applied.
    fn starting_with(head: Option<ChatEntry>, catching_up: bool) -> Self {
        let mut chat = Self {
            catching_up,
            ..Self::default()
        };
        if let Some(head) = &head {
            chat.push(head.clone());
        }
        chat.head = head;
        chat
    }

    /// The file started over: everything read from it goes, and the read
    /// that restarted it ends by sending the chat whole.
    fn restart(&mut self) {
        *self = Self::starting_with(self.head.take(), true);
        self.stale = true;
    }

    /// The transcript's record says the conversation was rewound to before
    /// `message` was sent: the last turn that message began and every entry
    /// after it go, except the newest turn's own first entry, which the
    /// transcript wrote before the record. The state of the dropped turns is
    /// pruned, and the chat is stale, so its loaders are sent it whole.
    /// Whether anything went; a chat with no turn start, or a message no turn
    /// before the newest one began with, changes nothing.
    fn rewind(&mut self, message: &str) -> bool {
        let Some(keep) = self.newest_turn() else {
            debug!(message, "chat mirror: the chat began no turn to keep");
            return false;
        };
        let Some(target) = self.last_turn_of(message, keep) else {
            debug!(
                message,
                "chat mirror: no turn before the newest one names the message"
            );
            return false;
        };
        self.entries.drain(target..keep);
        self.prune_turns();
        self.context = None;
        self.stale = true;
        true
    }

    /// The place of the newest entry that began a turn: the turn the rewind
    /// keeps. The user usually re-sends the restored message, so the newest
    /// turn holds the target's own words too.
    fn newest_turn(&self) -> Option<usize> {
        (0..self.entries.len())
            .rev()
            .find(|&at| entry_id(&self.entries[at]).is_some_and(|id| self.turn_starts.contains(id)))
    }

    /// The place of the last entry before `keep` that began a turn the
    /// restored `message` names: the same words, or — when the choice screen
    /// cut the message with `…` — the same opening, with whitespace counted as
    /// one space either way.
    fn last_turn_of(&self, message: &str, keep: usize) -> Option<usize> {
        let target = message.trim();
        let cut = target.strip_suffix('…');
        self.entries[..keep]
            .iter()
            .enumerate()
            .rev()
            .find_map(|(at, entry)| {
                let id = entry_id(entry)?;
                let ChatEntry::User { text, .. } = entry else {
                    return None;
                };
                (self.turn_starts.contains(id) && user_turn_matches(text, target, cut))
                    .then_some(at)
            })
    }

    /// Rebuild `index` from the entries left, and keep only what still names
    /// them or the turn the chat is in: a dropped turn's answers, live
    /// answers and times go with it.
    fn prune_turns(&mut self) {
        self.index = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(at, entry)| Some((entry_id(entry)?.to_owned(), at)))
            .collect();
        self.times.retain(|id, _| self.index.contains_key(id));
        self.turn_starts.retain(|id| self.index.contains_key(id));
        let turn = self.turn_prompt.clone();
        self.answered
            .retain(|prompt, _| Some(prompt) == turn.as_ref());
        self.ended_prompts
            .retain(|prompt| Some(prompt) == turn.as_ref());
        let (kept, dropped): (Vec<LiveItem>, Vec<LiveItem>) = std::mem::take(&mut self.live)
            .into_iter()
            .partition(|live| Some(&live.prompt) == turn.as_ref());
        self.live = kept;
        self.ended_cards
            .extend(dropped.iter().filter_map(LiveItem::card_script));
        self.live_next
            .retain(|prompt, _| Some(prompt) == turn.as_ref());
        self.finished.clear();
        self.changed.clear();
    }

    /// The read being caught up with ended: the held live lines take their
    /// places. Returns whether the loaders must be sent the chat whole, which
    /// then carries every live answer.
    fn catch_up(&mut self) -> bool {
        self.catching_up = false;
        for line in std::mem::take(&mut self.held) {
            self.merge_live(line);
        }
        let stale = std::mem::take(&mut self.stale);
        if stale {
            self.changed.clear();
        }
        stale
    }

    fn get(&self, id: &str) -> Option<ChatEntry> {
        let at = *self.index.get(id)?;
        self.entries.get(at).cloned()
    }

    /// Apply one transcript line: each entry it yields replaces the one with
    /// its id or goes at the end, and a new answer takes the place of the
    /// live message it is the transcript's copy of. A new prompt or an
    /// interrupt notice ends the turn before it and the live answers of the
    /// prompts before it, and a prompt's first entry begins a turn. A new turn
    /// end ends the live answers of its prompt.
    fn apply_line(
        &mut self,
        plugin: &dyn AgentPlugin,
        line: &str,
        facts: &TranscriptLineFacts,
    ) -> Change {
        let mut change = Change::default();
        if facts.is_user_prompt || facts.is_interrupt {
            if let Some(previous) = self.turn_prompt.take()
                && facts.prompt_id.as_ref() != Some(&previous)
            {
                self.ended_prompts.insert(previous);
            }
            self.turn_prompt.clone_from(&facts.prompt_id);
            change.removed = self.end_live_before(facts.prompt_id.as_deref());
        }
        let mut starts_turn = facts.is_user_prompt;
        let mut context = self
            .context
            .take()
            .unwrap_or_else(|| plugin.new_chat_context());
        let produced = plugin.chat_entries(line, &|id| self.get(id), context.as_mut());
        self.context = Some(context);
        for mut entry in produced {
            let Some(id) = entry_id(&entry).map(str::to_owned) else {
                continue;
            };
            self.time_script(&id, &mut entry, facts.timestamp);
            if starts_turn {
                starts_turn = false;
                self.turn_starts.insert(id.clone());
            }
            let had_result = self.has_result(&id);
            let replaces = self.upsert(id, &entry, facts.timestamp, &mut change.removed);
            change
                .removed
                .extend(self.script_landed(&entry, had_result));
            if replaces.is_none()
                && matches!(entry, ChatEntry::TurnEnd { .. })
                && let Some(prompt) = self.turn_prompt.clone()
            {
                change.removed.extend(self.end_turn_live(&prompt));
            }
            change.entries.push(entry);
            change.replaces.push(replaces);
        }
        change
    }

    /// A script that ran at once, its result written at `at`: one with an
    /// outcome but no eval number and no time yet takes the milliseconds
    /// from its call's time to `at` when both are known. A background
    /// script's time comes from the event that settles it.
    fn time_script(&self, id: &str, entry: &mut ChatEntry, at: Option<DateTime<Utc>>) {
        let ChatEntry::Tool {
            kind: ToolKind::Script,
            result: Some(_),
            script: Some(facts),
            ..
        } = entry
        else {
            return;
        };
        if facts.eval.is_some() || facts.ms.is_some() {
            return;
        }
        let called = self.times.get(id).copied().flatten();
        facts.ms = called
            .zip(at)
            .and_then(|(called, at)| u64::try_from((at - called).num_milliseconds()).ok());
    }

    /// Put `entry` in place of the entry with `id`, returning that one's
    /// place, or at the end with the time `at`; a new answer takes the place
    /// of its live message, whose id goes to `removed`.
    fn upsert(
        &mut self,
        id: String,
        entry: &ChatEntry,
        at: Option<DateTime<Utc>>,
        removed: &mut Vec<String>,
    ) -> Option<usize> {
        let replaces = self.index.get(&id).copied();
        if let Some(kept) = replaces.and_then(|place| self.entries.get_mut(place)) {
            kept.clone_from(entry);
        } else {
            if matches!(entry, ChatEntry::Answer { .. }) {
                removed.extend(self.answer_landed());
            }
            self.times.entry(id).or_insert(at);
            self.push(entry.clone());
        }
        replaces
    }

    /// The chat's stretch lines: its turns and their windows of activity.
    fn stretch_lines(&self) -> Vec<StretchLine> {
        summary_lines::stretch_lines(&self.timed())
    }

    /// Each stretch's id and the text the summarizer is sent for it.
    fn stretch_inputs(&self) -> Vec<(String, String)> {
        summary_lines::stretch_inputs(&self.timed())
    }

    /// The digest of `stretch`'s text as it stands; `None` when the chat has
    /// no such stretch.
    fn stretch_digest(&self, stretch: &str) -> Option<String> {
        self.stretch_inputs()
            .into_iter()
            .find(|(id, _)| id == stretch)
            .map(|(_, input)| input_digest(&input))
    }

    /// The entries with their times and turn starts.
    fn timed(&self) -> Vec<TimedEntry<'_>> {
        self.entries
            .iter()
            .map(|entry| {
                let id = entry_id(entry);
                TimedEntry {
                    entry,
                    at: id.and_then(|id| self.times.get(id).copied().flatten()),
                    starts_turn: id.is_some_and(|id| self.turn_starts.contains(id)),
                }
            })
            .collect()
    }

    /// The transcript wrote the next answer of the current turn: the live
    /// message that arrived in the same place of that turn leaves, and its
    /// id is returned.
    fn answer_landed(&mut self) -> Option<String> {
        let prompt = self.turn_prompt.clone()?;
        let count = self.answered.entry(prompt.clone()).or_default();
        let ordinal = *count;
        *count += 1;
        let at = self.live.iter().position(|live| {
            live.prompt == prompt
                && matches!(live.body, LiveBody::Answer { ordinal: at, .. } if at == ordinal)
        })?;
        let live = self.live.remove(at);
        Some(self.finish_live(live))
    }

    /// A prompt other than `prompt` was typed: the live answers of every
    /// other prompt leave, their turns over, and their ids are returned. A
    /// later piece of one of them is dropped.
    fn end_live_before(&mut self, prompt: Option<&str>) -> Vec<String> {
        self.end_live_where(|live| Some(live.prompt.as_str()) != prompt)
    }

    /// `prompt`'s turn ended: its live answers leave, whether or not their
    /// transcript copies matched them, and their ids are returned. A later
    /// piece of one of them is dropped; a piece of a new message under
    /// `prompt` starts a new live answer.
    fn end_turn_live(&mut self, prompt: &str) -> Vec<String> {
        self.end_live_where(|live| live.prompt == prompt)
    }

    /// The live answers `ends` picks leave, finished; their ids.
    fn end_live_where(&mut self, ends: impl Fn(&LiveItem) -> bool) -> Vec<String> {
        let (ended, kept): (Vec<LiveItem>, Vec<LiveItem>) =
            std::mem::take(&mut self.live).into_iter().partition(ends);
        self.live = kept;
        ended
            .into_iter()
            .map(|live| self.finish_live(live))
            .collect()
    }

    /// `live` left: it is no longer sent, a later piece of it is dropped;
    /// its id.
    fn finish_live(&mut self, live: LiveItem) -> String {
        self.changed.remove(&live.key);
        if let Some(script) = live.card_script() {
            self.ended_cards.insert(script);
        }
        let id = live.id();
        self.finished.insert(live.key);
        id
    }

    /// The place in `entries` of the first entry a reply carries, `pending`
    /// pending entries after the live answers; with those filling the whole
    /// reply, the place the next entry takes.
    fn reply_floor(&self, pending: usize) -> usize {
        let hidden =
            (self.entries.len() + self.live.len() + pending).saturating_sub(CHAT_REPLY_CAP);
        hidden.min(self.entries.len())
    }

    /// Put `entry` at the end.
    fn push(&mut self, entry: ChatEntry) {
        if let Some(id) = entry_id(&entry) {
            self.index.insert(id.to_owned(), self.entries.len());
        }
        self.entries.push(entry);
    }

    /// Merge one live piece. A piece with no prompt key has no turn to be
    /// replaced in, so it is not shown, nor is one of a prompt whose turn
    /// ended; one arriving while catching up is held; one of a message whose
    /// transcript answer landed is dropped.
    fn merge_live(&mut self, line: LiveLine) {
        let Some(prompt) = line.prompt_key.clone() else {
            return;
        };
        if self.ended_prompts.contains(&prompt) {
            return;
        }
        if self.catching_up {
            self.held.push(line);
            self.stale = true;
            return;
        }
        let key = live_key(line.message_key.as_deref(), &prompt);
        if self.finished.contains(&key) {
            return;
        }
        if let Some(live) = self.live.iter_mut().find(|live| live.key == key) {
            if let LiveBody::Answer { pieces, .. } = &mut live.body {
                pieces.insert(line.index, line.text);
            }
        } else {
            let answered = &self.answered;
            let next = self
                .live_next
                .entry(prompt.clone())
                .or_insert_with(|| answered.get(&prompt).copied().unwrap_or(0));
            let ordinal = *next;
            *next += 1;
            self.live.push(LiveItem {
                key: key.clone(),
                prompt,
                body: LiveBody::Answer {
                    ordinal,
                    pieces: BTreeMap::from([(line.index, line.text)]),
                },
            });
        }
        self.changed.insert(key);
    }

    /// Mark every live answer changed, so the next send carries them all.
    fn mark_live_changed(&mut self) {
        self.changed = self.live.iter().map(|live| live.key.clone()).collect();
    }

    /// The live answers changed since they were last sent.
    fn take_changed_live(&mut self) -> Vec<ChatEntry> {
        let changed = std::mem::take(&mut self.changed);
        self.live
            .iter()
            .filter(|live| changed.contains(&live.key))
            .map(LiveItem::entry)
            .collect()
    }

    /// Merge the cards the screen shows running, returning whether any is
    /// new or changed. With no turn, or while catching up, none is taken:
    /// the next screen read offers them again.
    fn merge_cards(&mut self, cards: Vec<ChatEntry>) -> bool {
        let Some(prompt) = self.card_turn().cloned() else {
            return false;
        };
        let mut changed = false;
        for card in cards {
            changed |= self.merge_card(&prompt, card);
        }
        changed
    }

    /// Whether merging `cards` would take any: a turn takes cards, and one
    /// of them is new or changed.
    fn would_take(&self, cards: &[ChatEntry]) -> bool {
        self.card_turn().is_some() && cards.iter().any(|card| self.card_wanted(card).is_some())
    }

    /// The prompt whose turn takes live cards: the current one, once the
    /// chat is caught up and while that turn has not ended.
    fn card_turn(&self) -> Option<&String> {
        if self.catching_up {
            return None;
        }
        self.turn_prompt
            .as_ref()
            .filter(|prompt| !self.ended_prompts.contains(*prompt))
    }

    /// The key and eval number of `card` when merging it would push or
    /// change a live card; `None` for a card with no id or script number,
    /// one whose script the transcript carried or whose card left, and one
    /// the live list holds as it is.
    fn card_wanted(&self, card: &ChatEntry) -> Option<(String, u32)> {
        let key = entry_id(card)?;
        let script = card_script(card)?;
        if self.landed_scripts.contains(&script) || self.ended_cards.contains(&script) {
            return None;
        }
        let wanted = (key.to_owned(), script.0);
        match self
            .live
            .iter()
            .find(|live| live.key == key)
            .map(|live| &live.body)
        {
            None => Some(wanted),
            Some(LiveBody::Card { entry, .. }) if **entry != *card => Some(wanted),
            Some(_) => None,
        }
    }

    /// Merge one card into `prompt`'s turn, returning whether it is new or
    /// changed. A card with no id or eval number, one that left, and one
    /// whose script the transcript carried are not taken.
    fn merge_card(&mut self, prompt: &str, card: ChatEntry) -> bool {
        let Some((key, eval)) = self.card_wanted(&card) else {
            return false;
        };
        if let Some(live) = self.live.iter_mut().find(|live| live.key == key) {
            live.body = LiveBody::Card {
                eval,
                entry: Box::new(card),
            };
        } else {
            self.live.push(LiveItem {
                key: key.clone(),
                prompt: prompt.to_owned(),
                body: LiveBody::Card {
                    eval,
                    entry: Box::new(card),
                },
            });
        }
        self.changed.insert(key);
        true
    }

    /// The transcript carried script `entry`, whose earlier copy `had_result`
    /// or not; the ids of the live cards that leave, finished. A dispatched
    /// script's eval number ends every live card of that eval, and a later
    /// card of its eval and description is not taken. A script that ran at
    /// once (an outcome with no eval number, landing for the first time)
    /// ends one live card of the turn.
    fn script_landed(&mut self, entry: &ChatEntry, had_result: bool) -> Vec<String> {
        let ChatEntry::Tool {
            script: Some(facts),
            result,
            ..
        } = entry
        else {
            return Vec::new();
        };
        match facts.eval {
            Some(eval) => {
                self.landed_scripts
                    .insert((eval, facts.description.clone()));
                self.end_live_where(|live| live.card_eval() == Some(eval))
            }
            None if result.is_some() && !had_result => {
                self.ran_at_once(&facts.description).into_iter().collect()
            }
            None => Vec::new(),
        }
    }

    /// A script running `description` ran at once: the oldest live card of
    /// the turn showing that description leaves, else the turn's oldest
    /// card; its id. `None` when the turn has no live card.
    fn ran_at_once(&mut self, description: &str) -> Option<String> {
        let prompt = self.turn_prompt.clone()?;
        let in_turn = |live: &LiveItem| live.prompt == prompt && live.card_eval().is_some();
        let at = self
            .live
            .iter()
            .position(|live| {
                in_turn(live)
                    && live
                        .card_script()
                        .is_some_and(|(_, shown)| shown == description)
            })
            .or_else(|| self.live.iter().position(in_turn))?;
        let live = self.live.remove(at);
        Some(self.finish_live(live))
    }

    /// Whether the entry with `id` already carries an outcome.
    fn has_result(&self, id: &str) -> bool {
        matches!(
            self.get(id),
            Some(ChatEntry::Tool {
                result: Some(_),
                ..
            })
        )
    }

    /// [`Self::reply_with`] no pending entries.
    #[cfg(test)]
    fn reply(&self) -> (Vec<ChatEntry>, u32) {
        self.reply_with(&[])
    }

    /// The newest [`CHAT_REPLY_CAP`] entries, then the live answers, then
    /// `pending`, and how many older ones are left out.
    fn reply_with(&self, pending: &[ChatEntry]) -> (Vec<ChatEntry>, u32) {
        let total = self.entries.len() + self.live.len() + pending.len();
        let hidden = total.saturating_sub(CHAT_REPLY_CAP);
        let entries = self
            .entries
            .iter()
            .cloned()
            .chain(self.live.iter().map(LiveItem::entry))
            .chain(pending.iter().cloned())
            .skip(hidden)
            .collect();
        (entries, u32::try_from(hidden).unwrap_or(u32::MAX))
    }
}

/// Whether the turn `text` is the one the restored `message` names: the same
/// words when the screen had room for it all, else the same opening, since the
/// screen cuts a long message with `…`. Whitespace runs count as one space in
/// both arms, since the recorded message is the screen's wrapped text while
/// the turn's is the prompt as typed.
fn user_turn_matches(text: &str, message: &str, cut: Option<&str>) -> bool {
    match cut {
        None => {
            let whole = collapse_whitespace(message);
            !whole.is_empty() && collapse_whitespace(text) == whole
        }
        Some(opening) => {
            let opening = collapse_whitespace(opening);
            !opening.is_empty() && collapse_whitespace(text).starts_with(opening.as_str())
        }
    }
}

/// `text` with each run of whitespace one space, its ends trimmed.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The messages a chat send typed whose transcript record has not arrived,
/// oldest first, shown as pending user entries after the chat's entries.
/// They start no stretch: the summary reads only the transcript's entries.
#[derive(Default)]
struct PendingSends {
    entries: Vec<ChatEntry>,
    /// The ids of the entries a restart or a conversation switch dropped,
    /// for the next append to remove.
    dropped: Vec<String>,
}

impl PendingSends {
    /// Add `text` as pending entry `id`; the entry.
    fn add(&mut self, id: &str, text: &str) -> ChatEntry {
        let entry = ChatEntry::User {
            id: id.to_owned(),
            text: text.to_owned(),
            pending: true,
        };
        self.entries.push(entry.clone());
        entry
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    /// Drop every pending entry; the next append removes them.
    fn drop_all(&mut self) {
        let ids = self.entries.drain(..).filter_map(|entry| match entry {
            ChatEntry::User { id, .. } => Some(id),
            _ => None,
        });
        self.dropped.extend(ids);
    }

    /// The ids dropped since the last append.
    fn take_dropped(&mut self) -> Vec<String> {
        std::mem::take(&mut self.dropped)
    }

    /// For each user message new in `change`, take away the oldest pending
    /// entry its text settles (see [`settle_key`]), adding its id to the change's
    /// removals, as well as the ids dropped since the last append.
    fn clear_landed(&mut self, change: &mut Change) {
        for (entry, replaces) in change.entries.iter().zip(&change.replaces) {
            if let (ChatEntry::User { text, pending, .. }, None) = (entry, replaces)
                && !pending
                && let Some(id) = self.take_oldest(text)
            {
                change.removed.push(id);
            }
        }
        change.removed.append(&mut self.dropped);
    }

    /// Take away the oldest entry whose text settles against `text` (their
    /// [`settle_key`]s are equal); its id.
    fn take_oldest(&mut self, text: &str) -> Option<String> {
        let key = settle_key(text);
        let at = self.entries.iter().position(
            |entry| matches!(entry, ChatEntry::User { text: held, .. } if settle_key(held) == key),
        )?;
        match self.entries.remove(at) {
            ChatEntry::User { id, .. } => Some(id),
            _ => None,
        }
    }
}

/// The form a pending send and a transcript user message are compared in:
/// the text trimmed, each uploaded file's path (a whitespace-delimited token
/// through an `rt-uploads` folder) as `[Image]`, and each `[Image #<n>]`
/// the agent records for an attachment as `[Image]` too. A pasted image's
/// path so settles against the message the agent recorded for it.
fn settle_key(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text.trim();
    while !rest.is_empty() {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (token, tail) = rest.split_at(end);
        out.push_str(if is_upload_path(token) {
            "[Image]"
        } else {
            token
        });
        let gap_end = tail.len() - tail.trim_start().len();
        let (gap, next) = tail.split_at(gap_end);
        out.push_str(gap);
        rest = next;
    }
    image_markers_as_image(&out)
}

/// Whether `token` is a path to a file in an `rt-uploads` folder: the
/// folder name, a `\` or `/`, then a file name.
fn is_upload_path(token: &str) -> bool {
    const FOLDER: &str = "rt-uploads";
    token.match_indices(FOLDER).any(|(at, _)| {
        let after = &token[at + FOLDER.len()..];
        after
            .strip_prefix(['\\', '/'])
            .is_some_and(|name| !name.is_empty() && !name.ends_with(['\\', '/']))
    })
}

/// `text` with each `[Image #<n>]` as `[Image]`.
fn image_markers_as_image(text: &str) -> String {
    const OPEN: &str = "[Image #";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(OPEN) {
        let (before, from) = rest.split_at(at);
        out.push_str(before);
        let after = &from[OPEN.len()..];
        let number = after.trim_start_matches(|c: char| c.is_ascii_digit());
        match number.strip_prefix(']') {
            Some(next) if number.len() < after.len() => {
                out.push_str("[Image]");
                rest = next;
            }
            _ => {
                out.push_str(OPEN);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The eval number and description of script card `entry`; `None` for
/// any other entry and a script with no number yet.
fn card_script(entry: &ChatEntry) -> Option<(u32, String)> {
    let ChatEntry::Tool {
        script: Some(facts),
        ..
    } = entry
    else {
        return None;
    };
    Some((facts.eval?, facts.description.clone()))
}

/// What groups a live line's pieces: its message, else its prompt.
fn live_key(message_key: Option<&str>, prompt_key: &str) -> String {
    message_key.map_or_else(|| format!("prompt:{prompt_key}"), str::to_owned)
}

/// The id of `entry`; `None` for a kind this build does not know.
pub(crate) fn entry_id(entry: &ChatEntry) -> Option<&str> {
    match entry {
        ChatEntry::User { id, .. }
        | ChatEntry::Answer { id, .. }
        | ChatEntry::Thinking { id, .. }
        | ChatEntry::Tool { id, .. }
        | ChatEntry::System { id, .. }
        | ChatEntry::Agent { id, .. }
        | ChatEntry::Receipt { id, .. }
        | ChatEntry::TurnEnd { id, .. } => Some(id),
        ChatEntry::Unknown => None,
    }
}

/// Transcript lines in the default agent's shape, for the mirror's tests.
#[cfg(test)]
pub(crate) mod test_lines {
    use serde_json::json;

    /// The time of the `n`th second of the test's day.
    pub(crate) fn stamp(n: usize) -> String {
        format!(
            "2026-09-27T{:02}:{:02}:{:02}.000Z",
            n / 3600,
            n / 60 % 60,
            n % 60
        )
    }

    /// A typed prompt `uuid` at second `n`, stamped with `prompt_id`.
    pub(crate) fn prompt(uuid: &str, n: usize, prompt_id: Option<&str>, text: &str) -> String {
        let mut record = json!({
            "type": "user", "uuid": uuid, "timestamp": stamp(n),
            "message": {"role": "user", "content": text},
        });
        if let Some(prompt_id) = prompt_id {
            record["promptId"] = json!(prompt_id);
        }
        record.to_string()
    }

    /// An answer `uuid` at second `n`.
    pub(crate) fn answer(uuid: &str, n: usize, text: &str) -> String {
        json!({
            "type": "assistant", "uuid": uuid, "timestamp": stamp(n),
            "message": {"id": format!("msg-{uuid}"), "role": "assistant", "type": "message",
                "content": [{"type": "text", "text": text}]},
        })
        .to_string()
    }

    /// A message typed mid-turn, `uuid`, written at second `n`.
    pub(crate) fn queued(uuid: &str, n: Option<usize>, text: &str) -> String {
        let mut record = json!({
            "type": "attachment", "uuid": uuid,
            "attachment": {"type": "queued_command", "prompt": text},
        });
        if let Some(n) = n {
            record["timestamp"] = json!(stamp(n));
        }
        record.to_string()
    }

    /// The `last-prompt` record a Restore writes: the leaf of zeros, no
    /// prompt text.
    pub(crate) fn rewound() -> String {
        json!({
            "type": "last-prompt",
            "leafUuid": "00000000-0000-0000-0000-000000000000",
            "explicit": true,
        })
        .to_string()
    }

    /// A `MessageDisplay` hook body: piece `index` of message `message_id` in
    /// the turn of `prompt_id`.
    pub(crate) fn live_body(
        prompt_id: &str,
        message_id: &str,
        index: u64,
        delta: &str,
    ) -> serde_json::Value {
        json!({
            "hook_event_name": "MessageDisplay", "prompt_id": prompt_id,
            "message_id": message_id, "index": index, "final": false, "delta": delta,
        })
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::test_lines::{answer, prompt, queued, rewound, stamp};
    use super::*;

    fn claude() -> &'static dyn AgentPlugin {
        agent_registry::plugin(&agent_registry::default_agent()).expect("Claude is registered")
    }

    fn apply(chat: &mut ChatState, line: &str) -> Change {
        let facts = claude().chat_line_facts(line);
        chat.apply_line(claude(), line, &facts)
    }

    fn ids(entries: &[ChatEntry]) -> Vec<&str> {
        entries.iter().filter_map(entry_id).collect()
    }

    fn live(prompt_id: &str, message_id: &str, index: u64, text: &str) -> LiveLine {
        LiveLine {
            text: text.to_owned(),
            index,
            message_key: Some(message_id.to_owned()),
            prompt_key: Some(prompt_id.to_owned()),
        }
    }

    #[tokio::test]
    async fn a_line_read_while_catching_up_leaves_the_pending_entry() {
        let live_task = tokio::spawn(async {}).abort_handle();
        let mut mirror = SessionMirror::new("s1", claude(), live_task);
        mirror.chat = ChatState::starting_with(None, true);
        mirror.pending.add("pending:1", "same");
        let line = prompt("u-1", 1, None, "same");
        let facts = claude().chat_line_facts(&line);
        mirror.apply_line("s1", claude(), &line, &facts);
        assert_eq!(
            mirror.pending.len(),
            1,
            "an old line with the send's text is not its record"
        );
    }

    /// Whether a pending send of `pending` is settled by a prompt Claude
    /// recorded as `landed`.
    fn settles(pending: &str, landed: &str) -> bool {
        let mut chat = ChatState::starting_with(None, false);
        let mut change = apply(&mut chat, &prompt("u-1", 1, None, landed));
        let mut sends = PendingSends::default();
        sends.add("pending:1", pending);
        sends.clear_landed(&mut change);
        change.removed.iter().any(|id| id == "pending:1")
    }

    #[test]
    fn a_pasted_image_path_settles_against_its_image_message() {
        assert!(settles(
            r"X:\work\.tmp\rt-uploads\20261122-photo-1.jpg ",
            "[Image #1]"
        ));
    }

    #[test]
    fn a_path_inside_a_prompt_settles_against_its_image_message() {
        assert!(settles(
            r"look at X:\work\.tmp\rt-uploads\a.png now",
            "look at [Image #1] now"
        ));
    }

    #[test]
    fn a_unix_upload_path_settles_too() {
        assert!(settles(
            "/Users/me/work/.tmp/rt-uploads/b.jpg",
            "[Image #2]"
        ));
    }

    #[test]
    fn two_pasted_images_settle_against_one_message() {
        let pending = r"X:\work\.tmp\rt-uploads\a.png X:\work\.tmp\rt-uploads\b.jpg";
        assert!(settles(pending, "[Image #1] [Image #2]"));
    }

    #[test]
    fn an_image_message_does_not_settle_plain_text() {
        assert!(!settles("hello", "[Image #1]"));
    }

    #[test]
    fn a_path_outside_rt_uploads_is_compared_as_text() {
        assert!(!settles(r"X:\work\notes\a.png", "[Image #1]"));
    }

    /// A question card read with a tab bar of two questions, asking
    /// `subject`, its options each a label and the description it shows.
    fn question_card(subject: &str, options: &[(&str, Option<&str>)]) -> ChatScreen {
        let tab = |label: &str| protocol::ChatQuestionTab {
            label: label.to_owned(),
            answered: false,
            current: false,
        };
        ChatScreen {
            composer: protocol::ComposerState::Dialog,
            prompt: Some(ChatPrompt {
                id: "p1".to_owned(),
                kind: ChatPromptKind::Question,
                subject: subject.to_owned(),
                body: String::new(),
                options: (1..)
                    .zip(options)
                    .map(|(number, &(label, description))| protocol::ChatOption {
                        number,
                        label: label.to_owned(),
                        description: description.map(str::to_owned),
                        selected: false,
                    })
                    .collect(),
                questions: vec![tab("Notes"), tab("Mocks")],
                can_amend: false,
                always_allow_tip: None,
                multi: false,
                cursor: Some(1),
                adjust: None,
                preview: None,
            }),
        }
    }

    /// A plan card awaiting approval with no subject or body of its own.
    fn plan_card() -> ChatScreen {
        ChatScreen {
            composer: protocol::ComposerState::Dialog,
            prompt: Some(ChatPrompt {
                id: "p1".to_owned(),
                kind: ChatPromptKind::Plan,
                subject: String::new(),
                body: String::new(),
                options: Vec::new(),
                questions: Vec::new(),
                can_amend: false,
                always_allow_tip: None,
                multi: false,
                cursor: None,
                adjust: None,
                preview: None,
            }),
        }
    }

    /// A hook question asking `question`, its options each a label and a
    /// description.
    fn hook_question(question: &str, options: &[(&str, &str)]) -> PendingQuestion {
        PendingQuestion {
            question: question.to_owned(),
            header: String::new(),
            options: options.iter().map(|&(label, _)| label.to_owned()).collect(),
            option_descriptions: options.iter().map(|&(_, text)| text.to_owned()).collect(),
            multi_select: false,
            preview: false,
        }
    }

    fn card_prompt(screen: &ChatScreen) -> &ChatPrompt {
        screen.prompt.as_ref().expect("a question card")
    }

    #[test]
    fn a_plan_subject_filled_from_the_hook_drops_its_leading_hashes() {
        let subject = |plan: &str| {
            card_prompt(&filled(&plan_card(), Some(plan), &[]))
                .subject
                .clone()
        };
        assert_eq!(subject("# Long smoke plan\nstep 1"), "Long smoke plan");
        assert_eq!(subject("## Two\nstep 1"), "Two");
        assert_eq!(
            subject("#hashtag plan\nstep 1"),
            "#hashtag plan",
            "a hash that begins a word is not a heading"
        );
        assert_eq!(subject("#\nThe real plan"), "The real plan");
    }

    #[test]
    fn a_plan_indented_like_code_keeps_its_marks_in_the_subject() {
        let shown = filled(&plan_card(), Some("    # not a heading\nstep 1"), &[]);
        assert_eq!(card_prompt(&shown).subject, "# not a heading");
    }

    #[test]
    fn a_heading_is_read_only_within_three_spaces_of_the_margin() {
        assert_eq!(heading_text("   # Indented"), "Indented");
        assert_eq!(
            heading_text("    # Code"),
            "    # Code",
            "four spaces read as a code block"
        );
        assert_eq!(heading_text("Plan: fix #12"), "Plan: fix #12");
        assert_eq!(
            heading_text("\t# Tab"),
            "\t# Tab",
            "a tab indents like code"
        );
    }

    #[test]
    fn a_tabbed_question_card_takes_the_matching_hooks_question() {
        let asked = "See X:\\dev\\rt\\city-142-ui-mocks.tmp\\x.md now?";
        let card = question_card(
            "See X:\\dev\\rt\\city-142-ui-mo cks.tmp\\x.md now?",
            &[("Yes", None)],
        );
        let hooks = [
            hook_question("Which notes should stay?", &[("Keep", "")]),
            hook_question(asked, &[("Yes", "")]),
        ];
        let shown = filled(&card, None, &hooks);
        assert_eq!(
            card_prompt(&shown).subject,
            asked,
            "the second hook question is the one on screen"
        );
    }

    #[test]
    fn a_cards_option_description_takes_the_hooks_description() {
        let whole = "Open X:\\dev\\rt\\city-1-mocks.tmp\\x.md first";
        let card = question_card(
            "Which mock?",
            &[
                (
                    "City  mocks",
                    Some("Open X:\\dev\\rt\\city-1-mo cks.tmp\\x.md first"),
                ),
                ("None", Some("Skip the mocks")),
            ],
        );
        let hooks = [hook_question(
            "Which mock?",
            &[("City mocks", whole), ("None", "Leave every mock alone")],
        )];
        let shown = filled(&card, None, &hooks);
        let options = &card_prompt(&shown).options;
        assert_eq!(options[0].description.as_deref(), Some(whole));
        assert_eq!(
            options[1].description.as_deref(),
            Some("Skip the mocks"),
            "a description the screen does not show stays the screen's"
        );
    }

    #[test]
    fn a_card_matching_no_hook_question_keeps_its_screen_text() {
        let subject = "See X:\\dev\\rt\\city-142-ui-mo cks.tmp\\x.md now?";
        let description = "Open X:\\dev\\rt\\a- b.md";
        let card = question_card(subject, &[("Yes", Some(description))]);
        let hooks = [hook_question(
            "Where should the next release go?",
            &[("Yes", "Open X:\\dev\\rt\\a-b.md")],
        )];
        let shown = filled(&card, None, &hooks);
        let prompt = card_prompt(&shown);
        assert_eq!(prompt.subject, subject);
        assert_eq!(prompt.options[0].description.as_deref(), Some(description));
    }

    #[test]
    fn a_moved_effort_reads_as_a_changed_prompt() {
        let screen = |kind: protocol::ChatPromptKind, cursor: u32, adjust: &str| ChatScreen {
            composer: protocol::ComposerState::Dialog,
            prompt: Some(ChatPrompt {
                id: "p1".to_owned(),
                kind,
                subject: "Select model".to_owned(),
                body: String::new(),
                options: Vec::new(),
                questions: Vec::new(),
                can_amend: false,
                always_allow_tip: None,
                multi: false,
                cursor: Some(cursor),
                adjust: Some(adjust.to_owned()),
                preview: None,
            }),
        };
        let picker = |cursor: u32, adjust: &str| {
            screen(protocol::ChatPromptKind::ModelPicker, cursor, adjust)
        };
        let permission =
            |cursor: u32| screen(protocol::ChatPromptKind::Permission, cursor, "High effort");
        assert!(
            same_but_cursor(&permission(1), &permission(2)),
            "a moved cursor alone is no change on a permission"
        );
        assert!(
            !same_but_cursor(&picker(1, "High effort"), &picker(2, "High effort")),
            "a moved picker cursor moves the filled row"
        );
        assert!(
            !same_but_cursor(&picker(1, "High effort"), &picker(1, "xHigh effort")),
            "a moved effort redraws the card"
        );
    }

    #[test]
    fn a_moved_cursor_on_a_preview_question_reads_as_a_changed_prompt() {
        let screen = |cursor: u32, preview: Option<&str>| ChatScreen {
            composer: protocol::ComposerState::Dialog,
            prompt: Some(ChatPrompt {
                id: "p1".to_owned(),
                kind: protocol::ChatPromptKind::Question,
                subject: "Which layout?".to_owned(),
                body: String::new(),
                options: Vec::new(),
                questions: Vec::new(),
                can_amend: false,
                always_allow_tip: None,
                multi: false,
                cursor: Some(cursor),
                adjust: None,
                preview: preview.map(str::to_owned),
            }),
        };
        assert!(
            same_but_cursor(&screen(1, None), &screen(2, None)),
            "a moved cursor alone is no change on a question without previews"
        );
        assert!(
            !same_but_cursor(&screen(1, Some("+--+")), &screen(2, Some("+--+"))),
            "a moved cursor on a preview question shows the new option's preview"
        );
        assert!(
            !same_but_cursor(&screen(1, Some("")), &screen(2, Some(""))),
            "a moved cursor on a preview question is a change even with no box drawn"
        );
    }

    #[test]
    fn a_cursor_move_on_a_rewind_prompt_is_a_change() {
        let overlay = |cursor: Option<u32>, count: u32| {
            let options = (1..=count)
                .map(|number| protocol::ChatOption {
                    number,
                    label: if number == count {
                        "Close".to_owned()
                    } else {
                        format!("message {number}")
                    },
                    description: None,
                    selected: Some(number) == cursor,
                })
                .collect();
            ChatScreen {
                composer: protocol::ComposerState::Overlay {
                    name: "rewind".to_owned(),
                },
                prompt: Some(ChatPrompt {
                    id: String::new(),
                    kind: protocol::ChatPromptKind::Overlay,
                    subject: "Rewind".to_owned(),
                    body: "Restore the code and/or conversation to the point before…".to_owned(),
                    options,
                    questions: Vec::new(),
                    can_amend: false,
                    always_allow_tip: None,
                    multi: false,
                    cursor,
                    adjust: None,
                    preview: None,
                }),
            }
        };
        assert!(
            !same_but_cursor(&overlay(Some(2), 3), &overlay(Some(3), 3)),
            "a moved cursor on a Rewind prompt fills another row"
        );
        assert!(
            same_but_cursor(&overlay(None, 1), &overlay(Some(1), 1)),
            "a Close-only overlay keeps the no-push rule"
        );
    }

    #[test]
    fn an_answered_side_question_keeps_its_prompt_id() {
        let side_question = |body: &str| ChatScreen {
            composer: protocol::ComposerState::Overlay {
                name: "btw".to_owned(),
            },
            prompt: Some(ChatPrompt {
                id: String::new(),
                kind: protocol::ChatPromptKind::Overlay,
                subject: "Side question".to_owned(),
                body: body.to_owned(),
                options: vec![protocol::ChatOption {
                    number: 1,
                    label: "Close".to_owned(),
                    description: None,
                    selected: false,
                }],
                questions: Vec::new(),
                can_amend: false,
                always_allow_tip: None,
                multi: false,
                cursor: None,
                adjust: None,
                preview: None,
            }),
        };
        let mut state = ScreenState::default();
        assert!(state.observe(side_question("what is 2+2? one word"), None, &[]));
        let pending = card_prompt(&state.shown).id.clone();
        assert!(
            state.observe(side_question("what is 2+2? one word\n\nFour"), None, &[]),
            "the answer is sent to the clients"
        );
        let answered = card_prompt(&state.shown);
        assert_eq!(answered.id, pending, "the card keeps its id");
        assert_eq!(answered.body, "what is 2+2? one word\n\nFour");
    }

    const REPL: &str = include_str!("../../agents/claude/tests/fixtures/transcripts/repl.jsonl");

    #[test]
    fn a_scripts_calls_name_it_and_its_result_carries_its_run_time() {
        let mut chat = ChatState::default();
        for line in REPL.lines() {
            apply(&mut chat, line);
        }
        let parent = |id: &str| match chat.get(id) {
            Some(ChatEntry::Tool { parent_id, .. }) => parent_id,
            other => unreachable!("{id} is not a tool: {other:?}"),
        };
        for (inner, script) in [
            ("repl_00000000-0000-4000-8000-0000000000a1", "toolu_r1"),
            ("repl_00000000-0000-4000-8000-0000000000a2", "toolu_r1"),
            ("repl_00000000-0000-4000-8000-0000000000b1", "toolu_r2"),
            ("repl_00000000-0000-4000-8000-0000000000c1", "toolu_r3"),
            ("repl_00000000-0000-4000-8000-0000000000c3", "toolu_r4"),
        ] {
            assert_eq!(parent(inner).as_deref(), Some(script), "{inner}");
        }
        assert_eq!(
            parent("toolu_r2"),
            None,
            "a later script is no script's call"
        );
        let script_ms = |id: &str| match chat.get(id) {
            Some(ChatEntry::Tool {
                script: Some(facts),
                result: Some(_),
                ..
            }) => facts.ms,
            other => unreachable!("{id} is not an answered script: {other:?}"),
        };
        assert_eq!(
            script_ms("toolu_r1"),
            Some(7250),
            "10:00:17.250 less 10:00:10.000"
        );
        assert_eq!(script_ms("toolu_r2"), Some(400));
        for (script, ms) in [
            ("toolu_r3", 4194),
            ("toolu_r4", 842),
            ("toolu_r5", 34020),
            ("toolu_r6", 2149),
        ] {
            assert_eq!(script_ms(script), Some(ms), "{script}: the event's time");
        }
    }

    #[test]
    fn a_dispatched_script_runs_untimed_until_its_event_settles_it() {
        let mut chat = ChatState::default();
        let receipt = REPL
            .lines()
            .position(|line| line.contains("(dispatched #156)"))
            .unwrap_or_else(|| unreachable!("no receipt in the fixture"));
        for line in REPL.lines().take(receipt + 1) {
            apply(&mut chat, line);
        }
        assert!(
            matches!(
                chat.get("toolu_r3"),
                Some(ChatEntry::Tool {
                    result: None,
                    script: Some(protocol::ScriptFacts {
                        eval: Some(156),
                        ms: None,
                        ..
                    }),
                    ..
                })
            ),
            "{:?}",
            chat.get("toolu_r3")
        );
    }

    #[test]
    fn a_restarted_transcript_forgets_its_scripts() {
        let mut chat = ChatState::default();
        let lines: Vec<&str> = REPL.lines().collect();
        // The first script's call and its at-once result: its window is open.
        for line in &lines[..3] {
            apply(&mut chat, line);
        }
        chat.restart();
        apply(&mut chat, lines[3]);
        assert!(
            matches!(
                chat.get("repl_00000000-0000-4000-8000-0000000000a1"),
                Some(ChatEntry::Tool {
                    parent_id: None,
                    ..
                })
            ),
            "{:?}",
            chat.entries
        );
    }

    #[test]
    fn the_reply_carries_the_newest_entries_and_counts_the_rest() {
        let mut chat = ChatState::default();
        let total = CHAT_REPLY_CAP + 3;
        for n in 1..=total {
            apply(&mut chat, &prompt(&format!("u-{n}"), n, None, "hi"));
        }
        let (entries, hidden) = chat.reply();
        assert_eq!(entries.len(), CHAT_REPLY_CAP);
        assert_eq!(hidden, 3);
        assert_eq!(entry_id(&entries[0]), Some("u-4"));
        assert_eq!(
            entry_id(&entries[CHAT_REPLY_CAP - 1]),
            Some(format!("u-{total}").as_str())
        );
    }

    #[test]
    fn entries_keep_the_order_the_transcript_wrote_them() {
        let mut chat = ChatState::default();
        apply(&mut chat, &prompt("u-1", 1, None, "first"));
        apply(&mut chat, &answer("a-3", 3, "an answer"));
        let change = apply(&mut chat, &queued("q-2", Some(2), "typed mid-turn"));
        assert_eq!(ids(&change.entries), ["q-2"]);
        apply(&mut chat, &queued("q-untimed", None, "no time"));
        let (entries, hidden) = chat.reply();
        assert_eq!(hidden, 0);
        assert_eq!(
            ids(&entries),
            ["u-1", "a-3", "q-2", "q-untimed"],
            "an append has no place, so a reload must not order otherwise"
        );
    }

    #[test]
    fn a_live_line_without_a_prompt_key_is_not_shown() {
        let mut chat = ChatState::default();
        for message_key in [Some("m1".to_owned()), None] {
            chat.merge_live(LiveLine {
                text: "stray".to_owned(),
                index: 0,
                message_key,
                prompt_key: None,
            });
        }
        assert_eq!(chat.take_changed_live(), Vec::new());
        assert_eq!(chat.reply(), (Vec::new(), 0));
    }

    #[test]
    fn a_live_piece_held_through_the_first_read_is_not_taken_by_its_answers() {
        let mut chat = ChatState::starting_with(None, true);
        chat.merge_live(live("P1", "m2", 0, "two"));
        apply(&mut chat, &prompt("u-1", 1, Some("P1"), "go"));
        let first = apply(&mut chat, &answer("a-1", 2, "one, in full"));
        assert_eq!(
            first.removed,
            Vec::<String>::new(),
            "a-1 was in the file before m2 arrived"
        );
        assert!(chat.catch_up(), "the held piece is sent with the chat");
        assert_eq!(ids(&chat.reply().0), ["u-1", "a-1", "live:m2"]);
        let second = apply(&mut chat, &answer("a-2", 3, "two, in full"));
        assert_eq!(second.removed, ["live:m2"]);
    }

    #[test]
    fn live_pieces_join_in_index_order() {
        let mut chat = ChatState::default();
        chat.merge_live(live("P1", "m1", 1, "second"));
        chat.merge_live(live("P1", "m1", 0, "first\n"));
        assert_eq!(
            chat.take_changed_live(),
            [ChatEntry::Answer {
                id: "live:m1".to_owned(),
                text: "first\nsecond".to_owned(),
                live: true,
            }]
        );
        assert!(chat.take_changed_live().is_empty(), "nothing changed since");
        chat.merge_live(live("P1", "m2", 0, "print(1)\n"));
        chat.merge_live(live("P1", "m2", 1, "print(2)\n"));
        assert_eq!(
            chat.take_changed_live(),
            [ChatEntry::Answer {
                id: "live:m2".to_owned(),
                text: "print(1)\nprint(2)\n".to_owned(),
                live: true,
            }],
            "a piece carries its own line breaks: no blank line between them"
        );
    }

    #[test]
    fn the_answers_of_a_turn_replace_its_live_messages_in_arrival_order() {
        let mut chat = ChatState::default();
        apply(&mut chat, &prompt("u-1", 1, Some("P1"), "go"));
        chat.merge_live(live("P1", "m1", 0, "one"));
        chat.merge_live(live("P1", "m2", 0, "two"));
        let first = apply(&mut chat, &answer("a-1", 2, "one, in full"));
        assert_eq!(first.removed, ["live:m1"]);
        assert_eq!(ids(&first.entries), ["a-1"]);
        let second = apply(&mut chat, &answer("a-2", 3, "two, in full"));
        assert_eq!(second.removed, ["live:m2"]);
        let (entries, _) = chat.reply();
        assert_eq!(ids(&entries), ["u-1", "a-1", "a-2"]);
        chat.merge_live(live("P1", "m1", 1, "late piece"));
        assert!(
            chat.take_changed_live().is_empty(),
            "a finished message stays gone"
        );
        assert_eq!(ids(&chat.reply().0), ["u-1", "a-1", "a-2"]);
    }

    /// The record of a turn's end, `uuid`, at second `n`.
    fn turn_end(uuid: &str, n: usize) -> String {
        serde_json::json!({
            "type": "system", "subtype": "turn_duration", "uuid": uuid,
            "timestamp": stamp(n), "durationMs": 4200,
        })
        .to_string()
    }

    fn has_live(entries: &[ChatEntry]) -> bool {
        ids(entries).iter().any(|id| id.starts_with("live:"))
    }

    /// P1's turn with a live answer `live:m1` its transcript never matched,
    /// ended; the change the turn's end made.
    fn ended_unmatched_turn(chat: &mut ChatState) -> Change {
        apply(chat, &prompt("u-1", 1, Some("P1"), "go"));
        chat.merge_live(live("P1", "m1", 0, "on its way"));
        assert_eq!(ids(&chat.take_changed_live()), ["live:m1"]);
        apply(chat, &turn_end("e-1", 3))
    }

    #[test]
    fn a_turn_end_ends_its_unmatched_live_answers() {
        let mut chat = ChatState::default();
        let change = ended_unmatched_turn(&mut chat);
        assert_eq!(change.removed, ["live:m1"]);
        assert!(!has_live(&chat.reply().0), "no live row outlives its turn");
    }

    #[test]
    fn an_interrupt_ends_the_turns_live_answer_without_starting_a_turn() {
        let mut chat = ChatState::default();
        apply(&mut chat, &prompt("u-1", 1, Some("P1"), "go"));
        chat.merge_live(live("P1", "m1", 0, "on its way"));
        assert_eq!(ids(&chat.take_changed_live()), ["live:m1"]);
        let interrupt = serde_json::json!({
            "type": "user", "uuid": "i-2", "timestamp": stamp(3),
            "message": {"role": "user", "content": "[Request interrupted by user]"},
        })
        .to_string();
        let change = apply(&mut chat, &interrupt);
        assert_eq!(
            change.removed,
            ["live:m1"],
            "the interrupt ends the live answer like a prompt does"
        );
        assert!(
            !has_live(&chat.reply().0),
            "no live row outlives the interrupt"
        );
        assert!(
            !chat.turn_starts.contains("i-2"),
            "an interrupt notice starts no turn"
        );
        let lines = chat.stretch_lines();
        let line_ids: Vec<&str> = lines.iter().map(|line| line.id.as_str()).collect();
        assert_eq!(line_ids, ["u-1"]);
    }

    #[test]
    fn a_peer_message_starts_a_turn() {
        let mut chat = ChatState::default();
        apply(&mut chat, &prompt("u-1", 1, Some("P1"), "go"));
        let body = "The fix is on main.";
        let peer = serde_json::json!({
            "type": "user", "uuid": "g-2", "timestamp": stamp(3), "promptId": "P2",
            "isMeta": true, "promptSource": "system", "turnOrigin": "peer",
            "message": {"role": "user", "content": format!(
                "Another Claude session sent a message:\n<cross-session-message from=\"uds:x\" \
                 from-name=\"yaat-3a\">\n{body}\n</cross-session-message>\n\nThis came from another session."
            )},
            "origin": {"kind": "peer", "from": "uds:x", "name": "yaat-3a", "body": body},
        })
        .to_string();
        apply(&mut chat, &peer);
        assert!(
            chat.turn_starts.contains("g-2"),
            "a peer message starts a turn"
        );
        let lines = chat.stretch_lines();
        let line_ids: Vec<&str> = lines.iter().map(|line| line.id.as_str()).collect();
        assert_eq!(line_ids, ["u-1", "g-2"]);
        assert!(
            lines[1].text.starts_with(body),
            "the turn is named by the message: {:?}",
            lines[1]
        );
    }

    #[test]
    fn a_late_piece_of_an_ended_message_is_dropped() {
        let mut chat = ChatState::default();
        ended_unmatched_turn(&mut chat);
        chat.merge_live(live("P1", "m1", 1, "late piece"));
        assert!(chat.take_changed_live().is_empty(), "m1 is finished");
        assert!(!has_live(&chat.reply().0));
        chat.merge_live(live("P1", "m2", 0, "a later turn's"));
        assert_eq!(
            ids(&chat.take_changed_live()),
            ["live:m2"],
            "a turn's end does not block a later turn under the same prompt"
        );
    }

    #[test]
    fn a_reapplied_turn_end_keeps_the_new_live_answer() {
        let mut chat = ChatState::default();
        ended_unmatched_turn(&mut chat);
        chat.merge_live(live("P1", "m2", 0, "a later turn's"));
        let again = apply(&mut chat, &turn_end("e-1", 3));
        assert!(again.removed.is_empty(), "{:?}", again.removed);
        assert_eq!(ids(&chat.reply().0), ["u-1", "e-1", "live:m2"]);
    }

    #[test]
    fn a_turn_end_keeps_another_prompts_live_answer() {
        let mut chat = ChatState::default();
        apply(&mut chat, &prompt("u-1", 1, Some("P1"), "go"));
        chat.merge_live(live("P2", "m2", 0, "the next turn's"));
        let change = apply(&mut chat, &turn_end("e-1", 3));
        assert!(change.removed.is_empty(), "{:?}", change.removed);
        assert_eq!(ids(&chat.reply().0), ["u-1", "e-1", "live:m2"]);
    }

    #[test]
    fn a_live_message_after_its_turns_landed_answers_takes_the_next_place() {
        let mut chat = ChatState::default();
        apply(&mut chat, &prompt("u-1", 1, Some("P1"), "go"));
        let change = apply(&mut chat, &answer("a-1", 2, "done"));
        assert!(change.removed.is_empty(), "no live message to replace");
        chat.merge_live(live("P1", "m2", 0, "the next one"));
        assert_eq!(
            ids(&chat.take_changed_live()),
            ["live:m2"],
            "m2 follows a-1, it is not a-1's message"
        );
        let second = apply(&mut chat, &answer("a-2", 3, "the next one, in full"));
        assert_eq!(second.removed, ["live:m2"]);
    }

    #[test]
    fn a_piece_of_an_ended_prompt_under_a_new_message_is_not_shown() {
        let mut chat = ChatState::default();
        apply(&mut chat, &prompt("u-1", 1, Some("P1"), "go"));
        apply(&mut chat, &prompt("u-2", 2, Some("P2"), "again"));
        chat.merge_live(live("P1", "m9", 0, "late, under a new key"));
        assert!(chat.take_changed_live().is_empty(), "P1's turn is over");
        assert_eq!(ids(&chat.reply().0), ["u-1", "u-2"]);
    }

    /// The card the screen shows for script `eval` running `Build it`.
    fn live_card(eval: u32) -> ChatEntry {
        live_card_named(eval, "Build it")
    }

    /// The card the screen shows for script `eval` running `description`.
    fn live_card_named(eval: u32, description: &str) -> ChatEntry {
        ChatEntry::Tool {
            id: format!("live-script:{eval}"),
            name: "REPL".to_owned(),
            summary: description.to_owned(),
            input: String::new(),
            result: None,
            parent_id: None,
            tail_paths: Vec::new(),
            kind: ToolKind::Script,
            script: Some(protocol::ScriptFacts {
                description: description.to_owned(),
                eval: Some(eval),
                ms: None,
                error: None,
            }),
        }
    }

    /// A model's `REPL` call `tool_id` running `Build it`, record `uuid` at
    /// second `n`.
    fn repl_call(uuid: &str, n: usize, tool_id: &str) -> String {
        repl_call_named(uuid, n, tool_id, "Build it")
    }

    /// A model's `REPL` call `tool_id` running `description`, record `uuid`
    /// at second `n`.
    fn repl_call_named(uuid: &str, n: usize, tool_id: &str, description: &str) -> String {
        serde_json::json!({
            "type": "assistant", "uuid": uuid, "timestamp": stamp(n), "madeBy": "model",
            "message": {"role": "assistant", "type": "message", "model": "fake-model",
                "content": [{"type": "tool_use", "id": tool_id, "name": "REPL",
                    "input": {"code": "1", "description": description}}],
                "stop_reason": "tool_use"},
        })
        .to_string()
    }

    /// The receipt of `REPL` call `tool_id`, record `uuid` at second `n`: it
    /// runs in the background as `eval`.
    fn repl_receipt(uuid: &str, n: usize, tool_id: &str, eval: u32) -> String {
        let text = format!("(dispatched #{eval})");
        serde_json::json!({
            "type": "user", "uuid": uuid, "timestamp": stamp(n),
            "toolUseResult": {"code": "1", "result": text, "stdout": "", "stderr": "",
                "evalId": eval, "asyncDispatched": true},
            "message": {"role": "user", "content": [{"tool_use_id": tool_id,
                "type": "tool_result", "content": text, "is_error": false}]},
        })
        .to_string()
    }

    /// The result of `REPL` call `tool_id`, record `uuid` at second `n`: the
    /// script ran at once and returned `done`, with no receipt.
    fn repl_done(uuid: &str, n: usize, tool_id: &str) -> String {
        serde_json::json!({
            "type": "user", "uuid": uuid, "timestamp": stamp(n),
            "toolUseResult": {"code": "1", "result": "done", "stdout": "", "stderr": ""},
            "message": {"role": "user", "content": [{"tool_use_id": tool_id,
                "type": "tool_result", "content": "done", "is_error": false}]},
        })
        .to_string()
    }

    /// P1's turn showing the live cards of script 9 (`Build it`) and script
    /// 10 (`Test it`).
    fn two_cards_in_turn(chat: &mut ChatState) {
        apply(chat, &prompt("u-1", 1, Some("P1"), "go"));
        assert!(chat.merge_cards(vec![live_card(9), live_card_named(10, "Test it")]));
    }

    /// P1's turn showing script 9's live card, sent.
    fn card_in_turn(chat: &mut ChatState) {
        apply(chat, &prompt("u-1", 1, Some("P1"), "go"));
        assert!(chat.merge_cards(vec![live_card(9)]));
        assert_eq!(ids(&chat.take_changed_live()), ["live-script:9"]);
    }

    fn has_live_card(entries: &[ChatEntry]) -> bool {
        ids(entries).iter().any(|id| id.starts_with("live-script:"))
    }

    #[test]
    fn a_live_card_follows_the_live_answer_before_it() {
        let mut chat = ChatState::default();
        apply(&mut chat, &prompt("u-1", 1, Some("P1"), "go"));
        chat.merge_live(live("P1", "m1", 0, "building"));
        assert!(chat.merge_cards(vec![live_card(9)]));
        assert_eq!(ids(&chat.reply().0), ["u-1", "live:m1", "live-script:9"]);
        assert_eq!(ids(&chat.take_changed_live()), ["live:m1", "live-script:9"]);
    }

    #[test]
    fn a_live_card_merged_again_changes_nothing() {
        let mut chat = ChatState::default();
        card_in_turn(&mut chat);
        assert!(!chat.merge_cards(vec![live_card(9)]));
        assert_eq!(chat.take_changed_live(), Vec::new());
        assert_eq!(ids(&chat.reply().0), ["u-1", "live-script:9"]);
    }

    #[test]
    fn the_transcripts_script_card_ends_its_live_card() {
        let mut chat = ChatState::default();
        card_in_turn(&mut chat);
        let call = apply(&mut chat, &repl_call("a-2", 2, "toolu_9"));
        assert!(call.removed.is_empty(), "the call has no eval yet");
        let receipt = apply(&mut chat, &repl_receipt("r-3", 3, "toolu_9", 9));
        assert_eq!(receipt.removed, ["live-script:9"]);
        assert_eq!(ids(&receipt.entries), ["toolu_9"]);
        assert!(!chat.merge_cards(vec![live_card(9)]), "script 9 landed");
        assert_eq!(chat.take_changed_live(), Vec::new());
        assert_eq!(ids(&chat.reply().0), ["u-1", "toolu_9"]);
    }

    #[test]
    fn a_live_card_ends_with_its_turn() {
        let interrupt = serde_json::json!({
            "type": "user", "uuid": "i-2", "timestamp": stamp(3),
            "message": {"role": "user", "content": "[Request interrupted by user]"},
        })
        .to_string();
        let ends = [
            turn_end("e-1", 3),
            prompt("u-2", 3, Some("P2"), "again"),
            interrupt,
        ];
        for end in ends {
            let mut chat = ChatState::default();
            card_in_turn(&mut chat);
            let change = apply(&mut chat, &end);
            assert_eq!(change.removed, ["live-script:9"], "{end}");
            assert!(!has_live_card(&chat.reply().0), "{end}");
            assert!(!chat.merge_cards(vec![live_card(9)]), "{end}: card 9 ended");
        }
    }

    #[test]
    fn a_live_card_of_a_landed_script_makes_nothing() {
        let mut chat = ChatState::default();
        apply(&mut chat, &prompt("u-1", 1, Some("P1"), "go"));
        apply(&mut chat, &repl_call("a-2", 2, "toolu_9"));
        apply(&mut chat, &repl_receipt("r-3", 3, "toolu_9", 9));
        assert!(!chat.merge_cards(vec![live_card(9)]));
        assert_eq!(chat.take_changed_live(), Vec::new());
        assert!(!has_live_card(&chat.reply().0));
    }

    #[test]
    fn a_live_card_needs_a_turn_and_a_caught_up_chat() {
        let mut chat = ChatState::default();
        assert!(!chat.merge_cards(vec![live_card(9)]), "no turn");
        assert_eq!(chat.take_changed_live(), Vec::new());
        let mut chat = ChatState::starting_with(None, true);
        apply(&mut chat, &prompt("u-1", 1, Some("P1"), "go"));
        assert!(!chat.merge_cards(vec![live_card(9)]), "catching up");
        chat.catch_up();
        assert!(!has_live_card(&chat.reply().0));
        assert!(chat.merge_cards(vec![live_card(9)]), "offered again");
    }

    #[test]
    fn a_live_card_takes_no_answer_ordinal() {
        let mut chat = ChatState::default();
        apply(&mut chat, &prompt("u-1", 1, Some("P1"), "go"));
        chat.merge_live(live("P1", "m1", 0, "one"));
        assert!(chat.merge_cards(vec![live_card(9)]));
        chat.merge_live(live("P1", "m2", 0, "two"));
        let first = apply(&mut chat, &answer("a-1", 2, "one, in full"));
        assert_eq!(first.removed, ["live:m1"]);
        let second = apply(&mut chat, &answer("a-2", 3, "two, in full"));
        assert_eq!(second.removed, ["live:m2"]);
        assert_eq!(ids(&chat.reply().0), ["u-1", "a-1", "a-2", "live-script:9"]);
    }

    #[test]
    fn a_script_that_ran_at_once_ends_its_live_card() {
        let mut chat = ChatState::default();
        card_in_turn(&mut chat);
        apply(&mut chat, &repl_call("a-2", 2, "toolu_9"));
        let done = apply(&mut chat, &repl_done("r-3", 3, "toolu_9"));
        assert_eq!(done.removed, ["live-script:9"]);
        assert_eq!(ids(&chat.reply().0), ["u-1", "toolu_9"]);
        assert!(chat.merge_cards(vec![live_card_named(10, "Test it")]));
        let again = apply(&mut chat, &repl_done("r-3", 3, "toolu_9"));
        assert!(
            again.removed.is_empty(),
            "script 9's outcome already landed"
        );
        let mut chat = ChatState::default();
        apply(&mut chat, &prompt("u-1", 1, Some("P1"), "go"));
        apply(&mut chat, &repl_call("a-2", 2, "toolu_9"));
        let done = apply(&mut chat, &repl_done("r-3", 3, "toolu_9"));
        assert!(done.removed.is_empty(), "the turn has no live card");
    }

    #[test]
    fn a_script_that_ran_at_once_ends_the_live_card_of_its_description_else_the_oldest() {
        let mut chat = ChatState::default();
        two_cards_in_turn(&mut chat);
        apply(&mut chat, &repl_call_named("a-2", 2, "toolu_10", "Test it"));
        let done = apply(&mut chat, &repl_done("r-3", 3, "toolu_10"));
        assert_eq!(done.removed, ["live-script:10"]);
        assert_eq!(ids(&chat.reply().0), ["u-1", "toolu_10", "live-script:9"]);
        let mut chat = ChatState::default();
        two_cards_in_turn(&mut chat);
        apply(&mut chat, &repl_call_named("a-2", 2, "toolu_11", "Lint it"));
        let done = apply(&mut chat, &repl_done("r-3", 3, "toolu_11"));
        assert_eq!(
            done.removed,
            ["live-script:9"],
            "no card shows its description"
        );
    }

    #[test]
    fn a_live_card_is_known_by_its_eval_and_description() {
        let mut chat = ChatState::default();
        apply(&mut chat, &prompt("u-1", 1, Some("P1"), "go"));
        apply(&mut chat, &repl_call("a-2", 2, "toolu_9"));
        apply(&mut chat, &repl_receipt("r-3", 3, "toolu_9", 9));
        assert!(
            chat.merge_cards(vec![live_card_named(9, "Other script")]),
            "a new agent process numbers its scripts anew"
        );
        let mut chat = ChatState::default();
        card_in_turn(&mut chat);
        apply(&mut chat, &turn_end("e-1", 3));
        assert!(
            !chat.merge_cards(vec![live_card(9)]),
            "card 9 ended with its turn"
        );
        apply(&mut chat, &prompt("u-2", 4, Some("P2"), "again"));
        assert!(!chat.merge_cards(vec![live_card(9)]), "card 9 ended");
        assert!(chat.merge_cards(vec![live_card_named(9, "Other script")]));
        assert_eq!(ids(&chat.take_changed_live()), ["live-script:9"]);
    }

    #[test]
    fn a_rewrapped_live_card_is_sent_again_and_would_take_agrees_with_merging() {
        let mut chat = ChatState::default();
        assert!(!chat.would_take(&[live_card(9)]), "no turn");
        card_in_turn(&mut chat);
        assert!(!chat.would_take(&[live_card(9)]), "unchanged");
        assert!(
            chat.would_take(&[live_card(9), live_card(10)]),
            "card 10 is new"
        );
        let rewrapped = live_card_named(9, "Build it all");
        assert!(chat.would_take(std::slice::from_ref(&rewrapped)));
        assert!(chat.merge_cards(vec![rewrapped.clone()]));
        assert_eq!(chat.take_changed_live(), std::slice::from_ref(&rewrapped));
        assert_eq!(ids(&chat.reply().0), ["u-1", "live-script:9"]);
        apply(
            &mut chat,
            &repl_call_named("a-2", 2, "toolu_9", "Build it all"),
        );
        apply(&mut chat, &repl_receipt("r-3", 3, "toolu_9", 9));
        assert!(!chat.would_take(&[rewrapped]), "script 9 landed");
    }

    #[test]
    fn a_held_piece_of_a_prompt_the_first_read_ended_is_not_shown() {
        let mut chat = ChatState::starting_with(None, true);
        chat.merge_live(live("P1", "m1", 0, "held"));
        apply(&mut chat, &prompt("u-1", 1, Some("P1"), "go"));
        apply(&mut chat, &prompt("u-2", 2, Some("P2"), "again"));
        chat.catch_up();
        assert!(chat.take_changed_live().is_empty(), "P1's turn is over");
        assert_eq!(ids(&chat.reply().0), ["u-1", "u-2"]);
    }

    #[test]
    fn a_replacement_entry_keeps_its_first_time() {
        let call = serde_json::json!({
            "type": "assistant", "uuid": "c-1", "timestamp": stamp(10),
            "message": {"id": "msg-c-1", "role": "assistant", "type": "message",
                "content": [{"type": "tool_use", "id": "t-1", "name": "Bash",
                    "input": {"command": "ls"}}]},
        })
        .to_string();
        let done = serde_json::json!({
            "type": "user", "uuid": "r-1", "timestamp": stamp(70),
            "message": {"role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "t-1", "content": "ok"}]},
        })
        .to_string();
        let mut chat = ChatState::default();
        apply(&mut chat, &call);
        let change = apply(&mut chat, &done);
        assert!(
            ids(&change.entries).contains(&"t-1"),
            "the result replaces the card"
        );
        let first: DateTime<Utc> = stamp(10).parse().expect("a valid time");
        assert_eq!(chat.times.get("t-1"), Some(&Some(first)));
    }

    #[test]
    fn a_system_prompt_row_starts_its_own_stretch() {
        let hour = 3600;
        let bang = serde_json::json!({
            "type": "user", "uuid": "bang", "timestamp": stamp(14 * hour),
            "message": {"role": "user", "content": "<bash-input>git status</bash-input>"},
        })
        .to_string();
        let mut chat = ChatState::default();
        apply(&mut chat, &prompt("p", 10 * hour, Some("P1"), "Fix it"));
        apply(&mut chat, &answer("a", 10 * hour + 120, "Done"));
        apply(&mut chat, &bang);
        let lines = chat.stretch_lines();
        let line_ids: Vec<&str> = lines.iter().map(|line| line.id.as_str()).collect();
        assert_eq!(line_ids, ["p", "bang"]);
        assert_eq!(lines[0].text, "Fix it (2m)");
        assert_eq!(lines[0].to_entry, "a", "nothing after 10:02 joins p");
        let answered: DateTime<Utc> = stamp(10 * hour + 120).parse().expect("a valid time");
        assert_eq!(lines[0].ended_at, Some(answered));
        assert_eq!(
            (lines[1].from_entry.as_str(), lines[1].text.as_str()),
            ("bang", "! git status (<1m)")
        );
    }

    #[test]
    fn a_restart_keeps_only_the_starting_row() {
        let cleared = ChatEntry::System {
            id: "cleared:c2".to_owned(),
            text: CLEARED_TEXT.to_owned(),
        };
        let mut chat = ChatState::starting_with(Some(cleared.clone()), false);
        apply(&mut chat, &prompt("u-1", 1, Some("P1"), "go"));
        chat.merge_live(live("P1", "m1", 0, "on its way"));
        chat.restart();
        assert_eq!(chat.reply(), (vec![cleared], 0));
    }

    /// A chat of the turns `a`, `b` and `c` and the new turn `new`.
    fn three_turns() -> ChatState {
        let mut chat = ChatState::starting_with(None, false);
        for line in [
            prompt("u-1", 1, Some("P1"), "a"),
            answer("m-1", 2, "one"),
            prompt("u-2", 3, Some("P2"), "b"),
            answer("m-2", 4, "two"),
            prompt("u-3", 5, Some("P3"), "c"),
            answer("m-3", 6, "three"),
            prompt("u-4", 7, Some("P4"), "new"),
        ] {
            apply(&mut chat, &line);
        }
        chat
    }

    #[test]
    fn a_rewind_drops_the_turns_after_the_restored_message() {
        let mut chat = three_turns();
        assert!(chat.rewind("b"));
        assert_eq!(ids(&chat.entries), ["u-1", "m-1", "u-4"]);
        assert_eq!(chat.index.len(), 3, "the index follows the entries");
        assert!(
            chat.turn_starts.contains("u-4"),
            "the new turn still begins one"
        );
        assert!(!chat.turn_starts.contains("u-2"));
        assert!(chat.stale, "the loaders are sent the chat whole");
    }

    #[test]
    fn a_rewind_of_a_message_sent_twice_drops_from_its_last_turn() {
        let mut chat = ChatState::starting_with(None, false);
        apply(&mut chat, &prompt("u-1", 1, Some("P1"), "a"));
        apply(&mut chat, &prompt("u-2", 2, Some("P2"), "b"));
        apply(&mut chat, &prompt("u-3", 3, Some("P3"), "b"));
        apply(&mut chat, &prompt("u-4", 4, Some("P4"), "new"));
        assert!(chat.rewind("b"));
        assert_eq!(ids(&chat.entries), ["u-1", "u-2", "u-4"]);
    }

    #[test]
    fn a_rewind_when_the_new_turn_repeats_the_restored_message_drops_the_old_turn() {
        let mut chat = ChatState::starting_with(None, false);
        apply(&mut chat, &prompt("u-1", 1, Some("P1"), "a"));
        apply(&mut chat, &answer("m-1", 2, "one"));
        apply(&mut chat, &prompt("u-2", 3, Some("P2"), "b"));
        apply(&mut chat, &answer("m-2", 4, "two"));
        apply(&mut chat, &prompt("u-3", 5, Some("P3"), "b"));
        assert!(chat.rewind("b"), "the re-sent message names the old turn");
        assert_eq!(ids(&chat.entries), ["u-1", "m-1", "u-3"]);
    }

    /// A chat of the turn `Plan the release notes for v2 and list every
    /// change`, the turn `say ok`, and the new turn `new`.
    fn long_prompt_chat() -> ChatState {
        let mut chat = ChatState::starting_with(None, false);
        for line in [
            prompt(
                "u-1",
                1,
                Some("P1"),
                "Plan the release notes for v2 and list every change",
            ),
            answer("m-1", 2, "one"),
            prompt("u-2", 3, Some("P2"), "say ok"),
            answer("m-2", 4, "two"),
            prompt("u-3", 5, Some("P3"), "new"),
        ] {
            apply(&mut chat, &line);
        }
        chat
    }

    #[test]
    fn a_rewind_matches_a_message_the_screen_cut_short() {
        let mut chat = long_prompt_chat();
        assert!(chat.rewind("Plan the release notes for v2 and…"));
        assert_eq!(ids(&chat.entries), ["u-3"], "the new turn alone stays");
    }

    #[test]
    fn a_rewind_matches_across_whitespace_runs() {
        let mut chat = long_prompt_chat();
        assert!(
            chat.rewind("Plan  the release notes  for v2 and…"),
            "the screen's spaces need not be the turn's"
        );
        assert_eq!(ids(&chat.entries), ["u-3"]);
    }

    #[test]
    fn a_rewind_with_no_cut_matches_a_multi_line_turn_by_its_words() {
        let mut chat = ChatState::starting_with(None, false);
        apply(
            &mut chat,
            &prompt("u-1", 1, Some("P1"), "Plan the release\nnotes  for v2"),
        );
        apply(&mut chat, &answer("m-1", 2, "one"));
        apply(&mut chat, &prompt("u-2", 3, Some("P2"), "new"));
        assert!(
            chat.rewind("Plan the release notes for v2"),
            "a pasted message's words are the same however it wraps"
        );
        assert_eq!(ids(&chat.entries), ["u-2"]);
    }

    #[test]
    fn a_rewind_with_no_cut_matches_the_whole_message_only() {
        let mut chat = long_prompt_chat();
        assert!(!chat.rewind("Plan the release"));
        assert_eq!(
            ids(&chat.entries),
            ["u-1", "m-1", "u-2", "m-2", "u-3"],
            "a cut is what makes a shorter message match"
        );
    }

    #[test]
    fn a_rewind_of_a_message_no_turn_began_with_changes_nothing() {
        let mut chat = three_turns();
        assert!(!chat.rewind("nowhere"));
        assert_eq!(
            ids(&chat.entries),
            ["u-1", "m-1", "u-2", "m-2", "u-3", "m-3", "u-4"]
        );
        assert!(!chat.stale, "nothing is owed a send");
    }

    /// A mirror of `s1` with one chat loader, and what the loader is sent.
    fn loaded_mirror() -> (SessionMirror, mpsc::UnboundedReceiver<DaemonMessage>) {
        let live_task = tokio::spawn(async {}).abort_handle();
        let mut mirror = SessionMirror::new("s1", claude(), live_task);
        let (out, sent) = mpsc::unbounded_channel();
        mirror.loaders.insert(
            7,
            Loader {
                out,
                floor: 0,
                chat: true,
                summary: false,
            },
        );
        (mirror, sent)
    }

    /// Apply `line` to `mirror` as its tail would.
    fn tail(mirror: &mut SessionMirror, line: &str) {
        let facts = claude().chat_line_facts(line);
        mirror.apply_line("s1", claude(), line, &facts);
    }

    /// The entry ids of the last whole chat `sent` holds, `None` when it holds
    /// none.
    fn whole_chat(sent: &mut mpsc::UnboundedReceiver<DaemonMessage>) -> Option<Vec<String>> {
        let mut whole = None;
        while let Ok(msg) = sent.try_recv() {
            if let DaemonMessage::Chat { entries, .. } = msg {
                whole = Some(entries);
            }
        }
        whole.map(|entries| {
            entries
                .iter()
                .filter_map(entry_id)
                .map(str::to_owned)
                .collect()
        })
    }

    /// An attachment line that yields no entry, as the transcript writes
    /// several between a turn's user record and its other records.
    fn quiet_attachment() -> String {
        serde_json::json!({
            "type": "attachment", "uuid": "at-1",
            "attachment": {"type": "file-history-snapshot"},
        })
        .to_string()
    }

    #[tokio::test]
    async fn a_restores_record_drops_the_turns_the_restore_threw_away() {
        let (mut mirror, mut sent) = loaded_mirror();
        for line in [
            prompt("u-1", 1, Some("P1"), "a"),
            answer("m-1", 2, "one"),
            prompt("u-2", 3, Some("P2"), "b"),
            answer("m-2", 4, "two"),
            prompt("u-3", 5, Some("P3"), "c"),
            answer("m-3", 6, "three"),
        ] {
            tail(&mut mirror, &line);
        }
        mirror.pending_rewind.note("b".to_owned());
        // The transcript writes the new turn's user record, its attachments,
        // and then the rewind's record.
        tail(&mut mirror, &prompt("u-4", 7, Some("P4"), "new"));
        tail(&mut mirror, &quiet_attachment());
        tail(&mut mirror, &rewound());
        assert_eq!(
            whole_chat(&mut sent),
            Some(vec!["u-1".to_owned(), "m-1".to_owned(), "u-4".to_owned()]),
            "the chat goes back to the restored turn, and is sent whole"
        );
        assert!(
            mirror.pending_rewind.message.is_none(),
            "the record applied it once"
        );
    }

    #[tokio::test]
    async fn a_restores_record_keeps_the_summary_lines_already_written() {
        let (mut mirror, _sent) = loaded_mirror();
        mirror.writer = Some(SummaryWriter {
            jobs: None,
            memo: HashMap::from([(
                "s-1".to_owned(),
                ("d-1".to_owned(), Some("a line".to_owned())),
            )]),
            queued: HashMap::new(),
            watched: None,
            run: 0,
            pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        for line in [
            prompt("u-1", 1, Some("P1"), "a"),
            answer("m-1", 2, "one"),
            prompt("u-2", 3, Some("P2"), "b"),
            answer("m-2", 4, "two"),
            prompt("u-3", 5, Some("P3"), "c"),
            answer("m-3", 6, "three"),
        ] {
            tail(&mut mirror, &line);
        }
        mirror.pending_rewind.note("b".to_owned());
        tail(&mut mirror, &prompt("u-4", 7, Some("P4"), "new"));
        tail(&mut mirror, &quiet_attachment());
        tail(&mut mirror, &rewound());
        assert!(
            mirror
                .writer
                .as_ref()
                .is_some_and(|writer| writer.holds("s-1", "d-1")),
            "the kept stretches' lines are not asked for again"
        );
    }

    #[tokio::test]
    async fn a_restores_record_with_no_pending_restore_changes_nothing() {
        let (mut mirror, mut sent) = loaded_mirror();
        tail(&mut mirror, &prompt("u-1", 1, Some("P1"), "a"));
        tail(&mut mirror, &answer("m-1", 2, "one"));
        tail(&mut mirror, &rewound());
        assert_eq!(ids(&mirror.chat.entries), ["u-1", "m-1"]);
        assert!(!mirror.chat.stale);
        assert_eq!(whole_chat(&mut sent), None, "no whole chat is sent");
    }

    #[tokio::test]
    async fn a_second_prompt_with_no_restores_record_drops_the_pending_restore() {
        let (mut mirror, mut sent) = loaded_mirror();
        tail(&mut mirror, &prompt("u-1", 1, Some("P1"), "a"));
        mirror.pending_rewind.note("a".to_owned());
        tail(&mut mirror, &prompt("u-2", 2, Some("P2"), "new"));
        assert!(
            mirror.pending_rewind.message.is_some(),
            "the restore's own turn keeps the pending"
        );
        tail(&mut mirror, &prompt("u-3", 3, Some("P3"), "another"));
        assert!(
            mirror.pending_rewind.message.is_none(),
            "the turn between them passed with no record"
        );
        tail(&mut mirror, &rewound());
        assert_eq!(ids(&mirror.chat.entries), ["u-1", "u-2", "u-3"]);
        assert_eq!(whole_chat(&mut sent), None, "no whole chat is sent");
    }
}
