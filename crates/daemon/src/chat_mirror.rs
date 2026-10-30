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

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use agent_api::{AgentPlugin, LiveLine, ScreenWork, TranscriptLineFacts};
use chrono::{DateTime, Utc};
use protocol::{
    AgentId, ChatEntry, ChatPrompt, ChatScreen, ComposerState, DaemonMessage, StretchLine,
};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::background_work::BackgroundWork;
use crate::chat_work::chat_work;
use crate::hook_status::LiveLines;
use crate::screen::{Screen, SharedScreen, dialogs};
use crate::session::{SessionEvent, SessionRegistry};
use crate::summary_lines::{self, TimedEntry};
use crate::sync::lock;
use crate::transcript_tail::{self, TailEvent, TailHandle};

/// The most entries a `Chat` message carries: the newest ones, with its
/// `hidden` counting the older ones left out.
pub const CHAT_REPLY_CAP: usize = 500;

/// The shortest gap between two sends of the live answers still growing.
const LIVE_FLUSH_GAP: Duration = Duration::from_millis(100);

/// The row a chat starts with after the session moved to a new conversation.
const CLEARED_TEXT: &str = "Conversation cleared";

/// The next prompt id to mint, unique within the daemon's run.
static NEXT_PROMPT: AtomicU64 = AtomicU64::new(1);

/// The next epoch a chat switch starts, unique within the daemon's run, so
/// no two tails ever carry the same one.
static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

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
    /// How many live-line followers are running.
    #[cfg(test)]
    live_tasks: Arc<std::sync::atomic::AtomicUsize>,
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
                #[cfg(test)]
                live_tasks: Arc::default(),
                #[cfg(test)]
                load_gap: Mutex::new(None),
            }),
        }
    }

    fn upgrade(weak: &Weak<Inner>) -> Option<Self> {
        weak.upgrade().map(|inner| Self { inner })
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
            let reply = mirror.register(kind, session_id, request_id, connection, out_tx);
            // A closed outbox is a connection already ending; its guard
            // unloads. The strip follows the reply, to this loader only.
            let _ = out_tx.send(reply);
            let _ = out_tx.send(mirror.work_shown.clone());
        }
        drop(grid);
        // The record was read before the lock: a conversation change or a
        // removal handled since found no mirror to act on.
        self.refresh(session_id);
        Ok(())
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
        }
    }

    /// `connection` ended: it loads no chat any more.
    fn connection_closed(&self, connection: u64) {
        lock(&self.inner.sessions).retain(|_, mirror| {
            mirror.loaders.remove(&connection);
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
        mirror.summary_sent.clear();
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
            TailEvent::Restarted => mirror.chat.restart(),
            TailEvent::CaughtUp => {
                if mirror.chat.catch_up() {
                    mirror.reset(session_id);
                }
                mirror.update_work(session_id);
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

    /// `session_id`'s record changed: a mirrored session takes the work the
    /// record now holds, and its loaders hear of a changed strip.
    fn follow_work(&self, session_id: &str) {
        if !lock(&self.inner.sessions).contains_key(session_id) {
            return;
        }
        let Some(record) = self.inner.registry.get(session_id) else {
            return;
        };
        let work = lock(&record).work.clone();
        let mut sessions = lock(&self.inner.sessions);
        if let Some(mirror) = sessions.get_mut(session_id) {
            mirror.take_work(session_id, work);
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

/// What a session's record says about its chat, read under the record's lock
/// and used after it is released.
struct Found {
    agent: AgentId,
    conversation_id: Option<String>,
    folder: Option<String>,
    extra_env: Vec<(String, String)>,
    screen: Option<SharedScreen>,
    work: BackgroundWork,
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
    screen: ScreenState,
    live_task: tokio::task::AbortHandle,
    /// The work the session's record holds, as its agent's last `Stop`
    /// listed it.
    work: BackgroundWork,
    /// The ids of the tasks whose task notification the session's
    /// transcripts hold, kept across a conversation switch since task ids
    /// are unique across conversations.
    ended_tasks: HashSet<String>,
    /// The running work the screen counted at its last reading that showed
    /// a count.
    screen_work: Option<ScreenWork>,
    /// The `ChatWork` the loaders were last sent.
    work_shown: DaemonMessage,
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
            screen: ScreenState::default(),
            live_task,
            work: BackgroundWork::default(),
            ended_tasks: HashSet::new(),
            screen_work: None,
            work_shown: DaemonMessage::ChatWork {
                session_id: session_id.to_owned(),
                subagents: Vec::new(),
                shells: Vec::new(),
                wakeup_at: None,
            },
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
        let change = self.chat.apply_line(plugin, line, facts);
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
    }

    /// Take `work` as what the record holds, telling the loaders of a
    /// changed strip.
    fn take_work(&mut self, session_id: &str, work: BackgroundWork) {
        if self.work != work {
            self.work = work;
            self.update_work(session_id);
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
        let floor = self.chat.reply_floor();
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

    /// Send every chat loader the whole chat and every summary loader every
    /// stretch line as a fresh start, which moves each chat loader's floor
    /// to the reply's first entry.
    fn reset(&mut self, session_id: &str) {
        let msg = self.chat_message(session_id, None);
        let floor = self.chat.reply_floor();
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
            .chat
            .stretch_lines()
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
        let lines = self.chat.stretch_lines();
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

    /// The whole chat as one `Chat` message: a reply when `request_id` is
    /// set, a fresh start for the loaders when it is not.
    fn chat_message(&self, session_id: &str, request_id: Option<String>) -> DaemonMessage {
        let (entries, hidden) = self.chat.reply();
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
        if self.screen.observe(reading) {
            self.tell(&DaemonMessage::ChatScreenChanged {
                session_id: session_id.to_owned(),
                screen: self.screen.shown.clone(),
            });
        }
    }

    fn flush_live(&mut self, session_id: &str) {
        let entries = self.chat.take_changed_live();
        if !entries.is_empty() {
            self.tell(&DaemonMessage::ChatAppend {
                session_id: session_id.to_owned(),
                entries,
                removed: Vec::new(),
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
    shown: ChatScreen,
    /// What makes the prompt on screen the same prompt, while one is.
    identity: Option<u64>,
    prompt_id: String,
}

impl Default for ScreenState {
    fn default() -> Self {
        Self {
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
    /// had (a new one for a new prompt); whether it changed more than the
    /// prompt's cursor.
    fn observe(&mut self, mut reading: ChatScreen) -> bool {
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
        let changed = !same_but_cursor(&reading, &self.shown);
        self.shown = reading;
        changed
    }
}

fn same_but_cursor(a: &ChatScreen, b: &ChatScreen) -> bool {
    // Outside a multi-select question an option is selected where the
    // cursor is, so its mark moves with the cursor.
    let without_cursor = |prompt: &ChatPrompt| {
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
    /// The answers still arriving, in the order their first piece came.
    live: Vec<LiveAnswer>,
    /// The place the next live message of each prompt's turn takes: the
    /// turn's answers already written when its first live message came, then
    /// one more per live message.
    live_next: HashMap<String, usize>,
    /// The keys of live messages whose transcript answer has landed.
    finished: HashSet<String>,
    /// The prompts whose turn a later prompt ended; their live pieces are
    /// dropped.
    ended_prompts: HashSet<String>,
    /// The keys of live messages changed since they were last sent.
    changed: HashSet<String>,
}

/// One answer still arriving: its pieces by their place in the message.
struct LiveAnswer {
    key: String,
    prompt: String,
    /// Its place among the messages of its prompt's turn.
    ordinal: usize,
    pieces: BTreeMap<u64, String>,
}

impl LiveAnswer {
    fn id(&self) -> String {
        format!("live:{}", self.key)
    }

    fn entry(&self) -> ChatEntry {
        let text: Vec<&str> = self.pieces.values().map(String::as_str).collect();
        ChatEntry::Answer {
            id: self.id(),
            text: text.join("\n"),
            live: true,
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
    /// live message it is the transcript's copy of. A new prompt ends the
    /// turn before it and the live answers of the prompts before it, and its
    /// first entry begins a turn.
    fn apply_line(
        &mut self,
        plugin: &dyn AgentPlugin,
        line: &str,
        facts: &TranscriptLineFacts,
    ) -> Change {
        let mut change = Change::default();
        if facts.is_user_prompt {
            if let Some(previous) = self.turn_prompt.take()
                && facts.prompt_id.as_ref() != Some(&previous)
            {
                self.ended_prompts.insert(previous);
            }
            self.turn_prompt.clone_from(&facts.prompt_id);
            change.removed = self.end_live_before(facts.prompt_id.as_deref());
        }
        let mut starts_turn = facts.is_user_prompt;
        let produced = plugin.chat_entries(line, &|id| self.get(id));
        for entry in produced {
            let Some(id) = entry_id(&entry).map(str::to_owned) else {
                continue;
            };
            if starts_turn {
                starts_turn = false;
                self.turn_starts.insert(id.clone());
            }
            let replaces = self.upsert(id, &entry, facts.timestamp, &mut change.removed);
            change.entries.push(entry);
            change.replaces.push(replaces);
        }
        change
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
        let timed: Vec<TimedEntry<'_>> = self
            .entries
            .iter()
            .map(|entry| {
                let id = entry_id(entry);
                TimedEntry {
                    entry,
                    at: id.and_then(|id| self.times.get(id).copied().flatten()),
                    starts_turn: id.is_some_and(|id| self.turn_starts.contains(id)),
                }
            })
            .collect();
        summary_lines::stretch_lines(&timed)
    }

    /// The transcript wrote the next answer of the current turn: the live
    /// message that arrived in the same place of that turn leaves, and its
    /// id is returned.
    fn answer_landed(&mut self) -> Option<String> {
        let prompt = self.turn_prompt.clone()?;
        let count = self.answered.entry(prompt.clone()).or_default();
        let ordinal = *count;
        *count += 1;
        let at = self
            .live
            .iter()
            .position(|live| live.prompt == prompt && live.ordinal == ordinal)?;
        let live = self.live.remove(at);
        self.changed.remove(&live.key);
        let id = live.id();
        self.finished.insert(live.key);
        Some(id)
    }

    /// A prompt other than `prompt` was typed: the live answers of every
    /// other prompt leave, their turns over, and their ids are returned. A
    /// later piece of one of them is dropped.
    fn end_live_before(&mut self, prompt: Option<&str>) -> Vec<String> {
        let (ended, kept): (Vec<LiveAnswer>, Vec<LiveAnswer>) = std::mem::take(&mut self.live)
            .into_iter()
            .partition(|live| Some(live.prompt.as_str()) != prompt);
        self.live = kept;
        ended
            .into_iter()
            .map(|live| {
                self.changed.remove(&live.key);
                let id = live.id();
                self.finished.insert(live.key);
                id
            })
            .collect()
    }

    /// The place in `entries` of the first entry a reply carries; with live
    /// answers filling the whole reply, the place the next entry takes.
    fn reply_floor(&self) -> usize {
        let hidden = (self.entries.len() + self.live.len()).saturating_sub(CHAT_REPLY_CAP);
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
            live.pieces.insert(line.index, line.text);
        } else {
            let answered = &self.answered;
            let next = self
                .live_next
                .entry(prompt.clone())
                .or_insert_with(|| answered.get(&prompt).copied().unwrap_or(0));
            let ordinal = *next;
            *next += 1;
            self.live.push(LiveAnswer {
                key: key.clone(),
                prompt,
                ordinal,
                pieces: BTreeMap::from([(line.index, line.text)]),
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
            .map(LiveAnswer::entry)
            .collect()
    }

    /// The newest [`CHAT_REPLY_CAP`] entries, live answers last, and how many
    /// older ones are left out.
    fn reply(&self) -> (Vec<ChatEntry>, u32) {
        let total = self.entries.len() + self.live.len();
        let hidden = total.saturating_sub(CHAT_REPLY_CAP);
        let entries = self
            .entries
            .iter()
            .cloned()
            .chain(self.live.iter().map(LiveAnswer::entry))
            .skip(hidden)
            .collect();
        (entries, u32::try_from(hidden).unwrap_or(u32::MAX))
    }
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
    use super::test_lines::{answer, prompt, queued, stamp};
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
        chat.merge_live(live("P1", "m1", 0, "first"));
        assert_eq!(
            chat.take_changed_live(),
            [ChatEntry::Answer {
                id: "live:m1".to_owned(),
                text: "first\nsecond".to_owned(),
                live: true,
            }]
        );
        assert!(chat.take_changed_live().is_empty(), "nothing changed since");
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
}
