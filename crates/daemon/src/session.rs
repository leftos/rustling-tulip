//! In-memory session registry: spawned agent and shell processes plus their state.

use crate::conversations::discovery::Discovery;
use crate::headless::HeadlessHandle;
use crate::history;
use crate::orphan::{self, OrphanMeta};
use crate::paths::Dirs;
use crate::pty::{PtyExit, PtyHandle};
use crate::screen::SharedScreen;
use crate::scrollback;
use crate::sync::{lock, read, write};
use crate::termstate::{self, BracketedPasteTracker};
use chrono::{DateTime, Utc};
use protocol::{
    AgentId, AppearanceOverrides, PendingInput, SessionKind, SessionMember, SessionMetrics,
    SessionMode, SessionSnapshot, SessionStatus, SpawnConfig,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{error, warn};
use uuid::Uuid;

/// In-memory cap on a session's `recent_actions`, matching the tail the
/// clients' headless log draws before it offers the rest.
const RECENT_ACTIONS_CAP: usize = 200;

/// On-disk cap for the persisted `recent_actions` tail. Smaller than the
/// in-memory cap because the sidecar is read on every daemon startup and
/// we don't want it ballooning. Trimmed to fit under [`RECENT_ACTIONS_TAIL_BYTE_CAP`].
const RECENT_ACTIONS_TAIL_ENTRIES: usize = 10;

/// Soft byte cap on the persisted `recent_actions` tail. Roughly 1 KB —
/// keeps sidecars compact. Entries are dropped from the front until the
/// remaining set fits.
const RECENT_ACTIONS_TAIL_BYTE_CAP: usize = 1024;
pub(crate) const EVENT_BROADCAST_CAPACITY: usize = 256;

/// The connection whose request made a session update, and the request's
/// id. Its own connection's forwarder echoes the id; no other client sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOrigin {
    pub connection: u64,
    pub request_id: String,
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Boxed because `SessionSnapshot` is wide; keeping it inline makes
    /// every other variant of this enum that big too. The origin is set
    /// when a client's request made the update.
    Updated(Box<SessionSnapshot>, Option<UpdateOrigin>),
    Removed(String),
    Attention {
        session_id: String,
        reason: protocol::AttentionReason,
    },
    /// The agent running inside the session's shell ended; see
    /// [`SessionRecord::end_agent_in_shell`].
    AgentExited {
        session_id: String,
    },
}

/// Atomic snapshot returned by `attach_lifecycle`'s task when a client requests
/// it via the `scrollback_snapshot_req` channel. `live` receives, from that
/// same task, every PTY chunk it persists after building `data`, so a
/// freshly-spawned forwarder task reading from `live` produces exactly the
/// bytes after the snapshot — no overlap with `data`, no gap. It closes when
/// the session's output ends.
#[derive(Debug)]
pub struct ScrollbackSnapshot {
    pub data: Vec<u8>,
    pub truncated: bool,
    pub live: mpsc::UnboundedReceiver<Vec<u8>>,
}

/// Per-session sender side of the snapshot-request channel. Cloned by the
/// `LoadScrollback` handler each time it needs an atomic snapshot. Stored on
/// the `SessionRecord` so the handler can find it via the registry.
pub type ScrollbackSnapshotReq = mpsc::UnboundedSender<oneshot::Sender<ScrollbackSnapshot>>;

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent runtime flags on one record; a state machine would not describe them"
)]
pub struct SessionRecord {
    pub id: String,
    /// Effective label surfaced to clients. Equals `user_label` when set,
    /// otherwise `default_label`. Kept eagerly synchronized so callers
    /// don't have to recompute on every snapshot.
    pub label: String,
    /// Daemon-generated default label (`<repo>:<branch>` for single,
    /// `<workspace>:<branch>` for workspace). Stable for the lifetime of
    /// the session record — re-applied when the user clears their rename
    /// override.
    pub default_label: String,
    /// Optional user-provided rename override. `None` means "use the
    /// default". Persisted to the orphan sidecar so the override survives
    /// daemon restarts and reattaches.
    pub user_label: Option<String>,
    pub kind: SessionKind,
    pub members: Vec<SessionMember>,
    pub mode: SessionMode,
    pub started_at: DateTime<Utc>,
    pub status: SessionStatus,
    /// When `status` last changed. [`SessionRegistry::update_from`] stamps it
    /// whenever an update moves the status, and every record-creation path
    /// stamps one, so a client can tell how long a session has been waiting.
    pub status_since: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    pub metrics: SessionMetrics,
    pub recent_actions: Vec<String>,
    pub pty: Option<Arc<PtyHandle>>,
    pub headless: Option<Arc<HeadlessHandle>>,
    /// Workspace this session belongs to (`Some` iff `kind == Workspace`).
    /// Used by clients to group sessions in the sidebar tree.
    pub workspace_id: Option<String>,
    /// Which agent is driving this session; a plain shell carries the
    /// default agent, so its status watcher keeps that agent's rules.
    /// Surfaced to clients via the `SessionSnapshot.agent` field, `None`
    /// there for a plain shell.
    pub agent: AgentId,
    /// Latest window title emitted by the agent/shell via OSC 0/1/2. Kept
    /// separate from `label` so explicit app renames can still win and the UI
    /// can filter transient shell titles like `C:\WINDOWS\system32\cmd.exe`.
    /// `None` until the first OSC title arrives.
    pub terminal_title: Option<String>,
    /// Short program name driving this session. For `Mode::PlainShell`
    /// this is the shell label (`"pwsh"`, `"cmd"`, `"bash"`, …); for
    /// `Mode::Interactive` / `Mode::Headless` it's the agent token.
    /// Mirrored on disk via
    /// [`crate::orphan::OrphanMeta::program_name`] so reattach restores
    /// the same chip after a daemon restart.
    pub program_name: Option<String>,
    /// Latest cwd reported by the PTY. Plain shells initialize this from the
    /// spawn cwd and update it from OSC 7 prompt sequences.
    pub current_cwd: Option<String>,
    /// Session-level appearance overrides. Stored after daemon validation.
    pub appearance: AppearanceOverrides,
    /// Persisted spawn-time configuration used to clone this session via
    /// [`protocol::ClientMessage::DuplicateSession`]. Always `Some` for
    /// sessions spawned by daemons that know about this field; `None`
    /// only for orphans reattached from sidecars written by earlier
    /// daemon versions. The duplicate handler rejects clones in the
    /// `None` case and the `GetSpawnConfig` reply surfaces `None` so the
    /// UI can fall back to opening the spawn dialog with defaults.
    pub spawn_config: Option<SpawnConfig>,
    /// True iff this record was reattached as an abandoned session: the
    /// previous daemon crashed mid-run and the agent process is gone.
    /// Surfaced to clients via `SessionSnapshot::is_abandoned`. Distinct
    /// from a normal stopped session (which exited gracefully) and from
    /// an orphan (whose process is still running, just detached).
    pub is_abandoned: bool,
    /// True when the session is parked: process stopped, worktree retained,
    /// session kept in the registry so the user can resume it. Set by the
    /// `ParkSession` handler; cleared by `DiscardSession`.
    pub is_inactive: bool,
    /// Absolute paths of the per-session worktrees created (or reused) at
    /// spawn time. Populated by the spawn pipeline; empty for in-place
    /// sessions (`use_worktree = false`) and for reattached orphans that
    /// pre-date this field. Used by `ListWorktrees` to cross-reference which
    /// worktrees are currently in active use.
    pub worktree_paths: Vec<String>,
    /// User's initial prompt at spawn, retained on the record so it can
    /// be surfaced via `SessionSnapshot::last_prompt` and replayed by the
    /// abandoned-resume handler.
    pub last_prompt: Option<String>,
    /// Notifier feeding user-input byte counts into
    /// `pty_state::watch_plain_shell` so the shell's classifier can tell echo
    /// from command output. `Some` for plain-shell sessions whose status
    /// watcher is running; `None` for agent, headless, orphaned or stopped
    /// sessions. Cleared by the exit watcher when the child dies.
    pub input_notifier: Option<mpsc::UnboundedSender<usize>>,
    /// The rendered screen of an interactive or plain-shell PTY session,
    /// fed from its output broadcast; `None` for headless and orphan
    /// records.
    pub screen: Option<SharedScreen>,
    /// Sender for the `attach_lifecycle` task's snapshot-request channel.
    /// `Some` for PTY sessions whose lifecycle task is running. The
    /// `LoadScrollback` handler uses this to ask the lifecycle task for an
    /// atomic (scrollback bytes, live-receiver) pair, ensuring the per-client
    /// forwarder picks up exactly where the persisted scrollback left off.
    /// `None` for headless, orphaned, or abandoned sessions (no live PTY) —
    /// in those cases `LoadScrollback` falls back to a direct file read with
    /// no live forwarder.
    pub scrollback_snapshot_req: Option<ScrollbackSnapshotReq>,
    /// The request that spawned this session, when it carried an id. Kept
    /// so a connection that lagged past its spawn reply gets it again after
    /// the resync list.
    pub spawn_origin: Option<UpdateOrigin>,
    /// The agent's conversation id: the one its plugin planned at spawn (or
    /// discovery found), moved by the agent's own `SessionStart` hook when a
    /// `/clear` or fork starts a new conversation. `None` for shells,
    /// headless runs and sessions whose conversation is not known yet; a
    /// hook-reported id can fill it in.
    pub conversation_id: Option<String>,
    /// True when the user asked that this session not count as busy. Set by
    /// `SetSessionBusyExcluded`, mirrored into the sidecar and copied into the
    /// session's history entry, so reattach, Resume and recovery keep it.
    pub busy_excluded: bool,
    /// What the session is waiting for while its status is
    /// [`SessionStatus::AwaitingInput`], as the agent CLI's own hooks last
    /// reported it. `None` until a hook reports one; copied into
    /// [`SessionSnapshot::pending_input`]. Mirrored to the sidecar beside the
    /// status, so a session reattached `AwaitingInput` keeps what it was
    /// asking; any other restored status starts with none.
    pub pending_input: Option<PendingInput>,
    /// Time of the last hook event applied to this session, in nanoseconds
    /// since the Unix epoch, from the shim's `X-RT-Hook-Time` header. Async
    /// hooks finish out of order, so an event at or before this time is
    /// dropped. Runtime-only: a daemon restart resets it along with the
    /// hook-driven state.
    pub last_hook_time: Option<u128>,
    /// True once a mapped hook event has been applied to this session. While
    /// true, hook signals own every promotion into `Working` and
    /// `AwaitingInput`, and the PTY classifier only demotes. Set by the first
    /// accepted signal from [`crate::hook_status::apply_signal`]; cleared by a
    /// daemon restart's rebuild of the record (see [`Self::last_hook_time`])
    /// and by a plain shell's own `SessionEnd`, after which the classifier
    /// owns its status again. Runtime-only: neither mirrored into the sidecar
    /// nor sent on the wire.
    pub hook_driven: bool,
    /// The assistant message the last `Stop` ended its turn with, trimmed and
    /// capped at 8 KiB, as the agent CLI's own hook reported it. `None` until
    /// a `Stop` reports one; a `Stop` with no message keeps the previous one.
    /// Runtime-only: neither mirrored into the sidecar nor sent on the wire.
    pub last_assistant_message: Option<String>,
    /// True while the session is `Idle` but the main agent's last `Stop`
    /// reported background work still running; copied into
    /// [`SessionSnapshot::background`]. Set by that `Stop`, cleared by any
    /// status change ([`crate::pty_state::apply_status`]) and by the writers
    /// that set a terminal status directly. Runtime-only: a reattached or
    /// rebuilt record starts with it false.
    pub background: bool,
    /// True when the session's PTY child is the user's shell and its agent
    /// was typed into it at spawn: the record stays an interactive agent
    /// session while its status runs on the shell's watcher. Mirrored into
    /// the sidecar so a reattach picks the same watcher.
    pub agent_in_shell: bool,
    /// The label of the shell an [`Self::agent_in_shell`] session runs, the
    /// leaf's program once the agent has ended. `None` for every other
    /// session. Mirrored into the sidecar.
    pub shell_label: Option<String>,
    /// True once the end of the agent's current run in its shell was
    /// announced to the clients, so one run announces once; a `SessionStart`
    /// hook clears it. Runtime-only.
    pub agent_exit_announced: bool,
    /// The agent whose plugin maps a plain shell's hook events: the agent a
    /// recovered shell typed its resume command into. `None` for every other
    /// session, since an agent session maps with [`Self::agent`]; a plain
    /// shell without one maps no hook event. Mirrored into the sidecar.
    pub hook_agent: Option<AgentId>,
    /// True once a client's `Resize` has sized this session. Until then any
    /// `Resize` applies; after it only a claimed one does, so a client
    /// without recent input cannot take the size from the one typing.
    /// Runtime-only: every new or rebuilt record starts false, and it is
    /// never mirrored into the sidecar.
    pub size_set: bool,
}

/// The agent a record names for a session that runs none: a plain shell, the
/// recovered shell among them.
pub fn default_agent() -> AgentId {
    agent_registry::default_agent()
}

impl SessionRecord {
    /// The agent whose plugin maps this session's hook events: its own agent,
    /// or a plain shell's [`Self::hook_agent`], so `None` for a plain shell
    /// without one.
    pub fn hook_plugin_agent(&self) -> Option<AgentId> {
        if self.mode == SessionMode::PlainShell {
            self.hook_agent.clone()
        } else {
            Some(self.agent.clone())
        }
    }

    /// The agent the wire and the sidecar name for this session: `None` for
    /// a plain shell, which runs none.
    pub fn wire_agent(&self) -> Option<AgentId> {
        (self.mode != SessionMode::PlainShell).then(|| self.agent.clone())
    }

    /// Whether the session's status runs on the shell's watcher and its
    /// hooks end with the agent inside the shell: a plain shell, or an agent
    /// typed into its shell.
    pub fn shell_hosted(&self) -> bool {
        self.mode == SessionMode::PlainShell || self.agent_in_shell
    }

    /// The agent inside this session's shell ended: the leaf names the shell
    /// again. Answers whether the clients are to be told, which is once per
    /// agent run and never for a session whose agent is not typed into its
    /// shell.
    pub fn end_agent_in_shell(&mut self) -> bool {
        if !self.agent_in_shell || self.agent_exit_announced {
            return false;
        }
        self.agent_exit_announced = true;
        if let Some(shell) = &self.shell_label {
            self.program_name = Some(shell.clone());
        }
        true
    }

    /// A new agent run started inside this session's shell (its
    /// `SessionStart` hook): the leaf names the agent, and the run's end is
    /// announced again.
    pub fn start_agent_in_shell(&mut self) {
        if !self.agent_in_shell {
            return;
        }
        self.agent_exit_announced = false;
        if let Some(plugin) = agent_registry::plugin(&self.agent) {
            self.program_name = Some(plugin.descriptor().tag.clone());
        } else {
            warn!(
                session_id = %self.id,
                "no agent plugin `{}` is registered; the leaf keeps its program name",
                self.agent.0
            );
        }
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        // Orphan = session still tracked but with no live stdio handle, and
        // not in a terminal state. After the child exits we keep the record
        // around with status=Stopped and pty=None — that's not an orphan,
        // it's just a finished session.
        let is_orphan = self.pty.is_none()
            && self.headless.is_none()
            && !matches!(self.status, SessionStatus::Stopped | SessionStatus::Error);
        // Surface "this session created a per-session worktree" so the
        // close-context-menu can offer worktree cleanup. Computed from
        // the persisted spawn config; orphans with no stored config
        // conservatively report `false`.
        let has_per_session_worktree =
            self.spawn_config
                .as_ref()
                .is_some_and(|cfg| match &cfg.target {
                    protocol::SpawnTarget::Single { use_worktree, .. }
                    | protocol::SpawnTarget::Workspace { use_worktree, .. } => *use_worktree,
                    protocol::SpawnTarget::Standalone { .. } => false,
                });
        let elevated_authority = self
            .spawn_config
            .as_ref()
            .is_some_and(|cfg| cfg.dangerously_skip_permissions);
        let agent = self.wire_agent();
        let agent_chips = if agent.is_some() {
            self.agent_chips()
        } else {
            Vec::new()
        };
        SessionSnapshot {
            id: self.id.clone(),
            label: self.label.clone(),
            user_label: self.user_label.clone(),
            kind: self.kind.clone(),
            members: self.members.clone(),
            status: self.status,
            status_since: self.status_since,
            mode: self.mode,
            started_at: self.started_at,
            exit_code: self.exit_code,
            metrics: self.metrics.clone(),
            recent_actions: self.recent_actions.clone(),
            is_orphan,
            is_abandoned: self.is_abandoned,
            last_prompt: self.last_prompt.clone(),
            workspace_id: self.workspace_id.clone(),
            agent,
            agent_chips,
            terminal_title: self.terminal_title.clone(),
            program_name: self.program_name.clone(),
            current_cwd: self.current_cwd.clone(),
            appearance: self.appearance.clone(),
            elevated_authority,
            has_per_session_worktree,
            is_inactive: self.is_inactive,
            worktree_paths: self.worktree_paths.clone(),
            conversation_id: self.conversation_id.clone(),
            busy_excluded: self.busy_excluded,
            pending_input: self.pending_input.clone(),
            background: self.background,
        }
    }

    /// The chips the agent's descriptor shows for the options this session
    /// was spawned with (none recorded reads as no options).
    fn agent_chips(&self) -> Vec<protocol::SessionChip> {
        let Some(plugin) = agent_registry::plugin(&self.agent) else {
            warn!(
                session_id = %self.id,
                "no agent plugin `{}` is registered; the session shows no chips",
                self.agent.0
            );
            return Vec::new();
        };
        let empty = protocol::OptionValues::new();
        let options = self
            .spawn_config
            .as_ref()
            .and_then(|cfg| cfg.agent.as_ref())
            .map_or(&empty, |selection| &selection.options);
        agent_api::session_chips(plugin.descriptor(), options)
    }
}

pub struct SessionRegistry {
    by_id: RwLock<HashMap<String, Arc<Mutex<SessionRecord>>>>,
    events: broadcast::Sender<SessionEvent>,
    /// `Dirs` is `Some` once `run()` initializes the registry. Without it
    /// the registry can't sync `recent_actions` back to disk — callers
    /// that construct registries in tests (none today) would pass `None`.
    dirs: Option<Dirs>,
    /// One gate per session, serialising every write and the delete of that
    /// session's sidecar. Two read-modify-writes racing each other would
    /// otherwise be free to leave the older of the two on disk, and a write
    /// racing the delete could put a sidecar back for a session that ended.
    /// Sessions never wait on each other's disk I/O. Lock order: a sidecar
    /// gate, then the registry map, then a record; this map's own lock is
    /// only held to look a gate up, with no other lock held.
    sidecar_gates: Mutex<HashMap<String, Arc<Mutex<SidecarGate>>>>,
    /// The sessions waiting for their conversation ids to be discovered.
    discovery: Discovery,
}

/// The per-session sidecar lock's state.
#[derive(Debug, Default)]
struct SidecarGate {
    /// The sidecar was deleted because the session ended; no write may
    /// bring it back. Session ids are never reused, so this never resets.
    deleted: bool,
}

impl SessionRegistry {
    pub fn new(dirs: Dirs) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        Arc::new(Self {
            by_id: RwLock::new(HashMap::new()),
            events,
            dirs: Some(dirs),
            sidecar_gates: Mutex::new(HashMap::new()),
            discovery: Discovery::default(),
        })
    }

    /// The sessions waiting for their conversation ids to be discovered.
    pub(crate) fn discovery(&self) -> &Discovery {
        &self.discovery
    }

    /// The sidecar gate for `id`, created on first use.
    fn sidecar_gate(&self, id: &str) -> Arc<Mutex<SidecarGate>> {
        Arc::clone(lock(&self.sidecar_gates).entry(id.to_owned()).or_default())
    }

    /// Drop `id`'s map entry if it is still `gate`: a writer that found the
    /// record gone under a gate it made (or kept past `remove`) leaves no
    /// entry behind, and never takes out a newer one.
    fn forget_gate(&self, id: &str, gate: &Arc<Mutex<SidecarGate>>) {
        let mut gates = lock(&self.sidecar_gates);
        if gates.get(id).is_some_and(|held| Arc::ptr_eq(held, gate)) {
            gates.remove(id);
        }
    }

    /// Run `write` with `id`'s record inside the session's sidecar gate,
    /// unless its sidecar was deleted when the session ended or the record
    /// is gone. The record is looked up under the gate, so a write racing
    /// `remove` either sees the record or skips.
    fn with_sidecar_gate(&self, id: &str, write: impl FnOnce(&Arc<Mutex<SessionRecord>>)) {
        let gate = self.sidecar_gate(id);
        let held = lock(&gate);
        if held.deleted {
            return;
        }
        let Some(arc) = self.get(id) else {
            drop(held);
            self.forget_gate(id, &gate);
            return;
        };
        write(&arc);
    }

    /// Whether the gates map holds an entry for `id`.
    #[cfg(test)]
    fn has_sidecar_gate(&self, id: &str) -> bool {
        lock(&self.sidecar_gates).contains_key(id)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }

    pub fn snapshots(&self) -> Vec<SessionSnapshot> {
        let guard = read(&self.by_id);
        guard.values().map(|rec| lock(rec).snapshot()).collect()
    }

    /// Every session's snapshot with the origin of the request that spawned
    /// it, each pair read under one lock.
    pub fn snapshots_with_spawn_origins(&self) -> Vec<(SessionSnapshot, Option<UpdateOrigin>)> {
        let guard = read(&self.by_id);
        guard
            .values()
            .map(|rec| {
                let rec = lock(rec);
                (rec.snapshot(), rec.spawn_origin.clone())
            })
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<Arc<Mutex<SessionRecord>>> {
        let guard = read(&self.by_id);
        guard.get(id).cloned()
    }

    pub fn insert(&self, record: SessionRecord) {
        self.insert_from(record, None);
    }

    /// [`Self::insert`], its broadcast carrying `origin`.
    pub fn insert_from(&self, record: SessionRecord, origin: Option<UpdateOrigin>) {
        self.insert_pending(record).publish_from(origin);
    }

    /// Put `record` into the registry map WITHOUT firing `SessionUpdated`. The
    /// returned [`PendingInsert`] hands back the `Arc<Mutex<SessionRecord>>` so
    /// the caller can patch fields (typically `scrollback_snapshot_req` and
    /// `input_notifier`, which depend on infrastructure spawned after the
    /// record is in the map) before publishing. Calling `publish()` fires the
    /// snapshot exactly once, so clients see the record fully wired up.
    pub fn insert_pending(&self, record: SessionRecord) -> PendingInsert<'_> {
        let id = record.id.clone();
        let arc = Arc::new(Mutex::new(record));
        {
            let mut guard = write(&self.by_id);
            guard.insert(id, arc.clone());
        }
        PendingInsert {
            registry: self,
            arc,
        }
    }

    pub fn remove(&self, id: &str) {
        {
            let mut guard = write(&self.by_id);
            if guard.remove(id).is_some() {
                let _ = self.events.send(SessionEvent::Removed(id.to_string()));
            }
        }
        // Every sidecar write checks the record under the gate, so a write
        // still holding this gate skips, and one that makes a new gate finds
        // the record gone.
        lock(&self.sidecar_gates).remove(id);
    }

    pub fn update<F>(&self, id: &str, f: F)
    where
        F: FnOnce(&mut SessionRecord),
    {
        self.update_from(id, None, f);
    }

    /// Mirror a record's persisted fields (see [`orphan::RecordMirror`]) into
    /// its sidecar. The values are read under the record lock inside the
    /// session's sidecar gate, so the last writer always writes the newest
    /// values. A record that is already gone, or whose sidecar was deleted
    /// when it ended, is skipped rather than resurrected. A session without a
    /// sidecar on disk is a no-op (see [`orphan::try_sync_record_fields`]).
    pub fn sync_sidecar(&self, id: &str) {
        let Some(dirs) = self.dirs.as_ref() else {
            return;
        };
        self.with_sidecar_gate(id, |arc| Self::sync_record(dirs, id, arc));
    }

    /// Write a session's spawn-time sidecar, then mirror the record into it,
    /// both inside the session's sidecar gate. A status tick that lands
    /// between the snapshot `meta` was built from and this write finds no
    /// sidecar to update, so the write alone would leave the stale snapshot
    /// on disk until the session's next change. Writes nothing once the
    /// record is gone or the session has ended and its sidecar was deleted —
    /// a child that exits at once can get there before its spawn does.
    pub fn write_sidecar(&self, meta: &OrphanMeta) {
        let Some(dirs) = self.dirs.as_ref() else {
            return;
        };
        let id = meta.session_id.as_str();
        self.with_sidecar_gate(id, |arc| {
            orphan::try_write_meta(dirs, meta);
            Self::sync_record(dirs, id, arc);
        });
    }

    /// Delete a session's sidecar because the session ended, inside its
    /// sidecar gate, and bar every later write of it: a sync still draining
    /// after the exit would otherwise put back a sidecar that the next daemon
    /// start reads as an abandoned session.
    ///
    /// A session the registry no longer holds has no writer left to bar
    /// (every write checks the record under the gate), so its sidecar is
    /// deleted without making a gate for it.
    pub fn delete_sidecar(&self, id: &str) {
        let Some(dirs) = self.dirs.as_ref() else {
            return;
        };
        if self.get(id).is_none() {
            orphan::try_delete_meta(dirs, id);
            return;
        }
        let gate = self.sidecar_gate(id);
        let mut gate = lock(&gate);
        gate.deleted = true;
        orphan::try_delete_meta(dirs, id);
    }

    /// `id`'s sidecar gate, so a test can hold it and stand in for a write
    /// that is part-way through.
    #[cfg(test)]
    fn hold_sidecar_sync(&self, id: &str) -> Arc<Mutex<SidecarGate>> {
        self.sidecar_gate(id)
    }

    fn sync_record(dirs: &Dirs, id: &str, arc: &Arc<Mutex<SessionRecord>>) {
        orphan::try_sync_record_fields(dirs, id, &Self::record_mirror(arc));
    }

    /// The record's persisted fields, read under its lock.
    fn record_mirror(arc: &Arc<Mutex<SessionRecord>>) -> orphan::RecordMirror {
        let guard = lock(arc);
        orphan::RecordMirror {
            recent_actions_tail: trim_recent_tail(&guard.recent_actions),
            status: guard.status,
            status_since: guard.status_since,
            pending_input: guard.pending_input.clone(),
            terminal_title: guard.terminal_title.clone(),
            current_cwd: guard.current_cwd.clone(),
            default_label: guard.default_label.clone(),
            user_label: guard.user_label.clone(),
            label: guard.label.clone(),
            appearance: guard.appearance.clone(),
            conversation_id: guard.conversation_id.clone(),
            busy_excluded: guard.busy_excluded,
            program_name: guard.program_name.clone(),
            agent_in_shell: guard.agent_in_shell,
            shell_label: guard.shell_label.clone(),
        }
    }

    /// Tell every client that the agent inside session `id`'s shell ended.
    /// Sent after the update that recorded the end, so it follows that
    /// update's `SessionUpdated`.
    pub fn announce_agent_exited(&self, id: &str) {
        let _ = self.events.send(SessionEvent::AgentExited {
            session_id: id.to_owned(),
        });
    }

    /// [`Self::update`], its broadcast carrying `origin`. Returns whether
    /// the registry holds `id`.
    pub fn update_from<F>(&self, id: &str, origin: Option<UpdateOrigin>, f: F) -> bool
    where
        F: FnOnce(&mut SessionRecord),
    {
        self.update_from_if_changed(id, origin, |record| {
            f(record);
            true
        })
    }

    /// [`Self::update`], but `f` answers whether it changed anything. The
    /// sidecar write and the broadcast are skipped when it answers `false`,
    /// so a hot path can test and act in one lock without publishing a no-op
    /// update. Returns whether the registry holds `id`.
    pub fn update_if_changed<F>(&self, id: &str, f: F) -> bool
    where
        F: FnOnce(&mut SessionRecord) -> bool,
    {
        self.update_from_if_changed(id, None, f)
    }

    /// The shared body of [`Self::update_from`] and [`Self::update_if_changed`].
    fn update_from_if_changed<F>(&self, id: &str, origin: Option<UpdateOrigin>, f: F) -> bool
    where
        F: FnOnce(&mut SessionRecord) -> bool,
    {
        let Some(arc) = self.get(id) else {
            return false;
        };
        let (snap, entered_error) = {
            let mut guard = lock(&arc);
            let was_error = guard.status == SessionStatus::Error;
            let was_status = guard.status;
            if !f(&mut guard) {
                return true;
            }
            if guard.status != was_status {
                guard.status_since = Some(Utc::now());
            }
            let entered_error = !was_error && guard.status == SessionStatus::Error;
            (guard.snapshot(), entered_error)
        };
        // Mirror the record into its sidecar so an abandoned session can show
        // "what was this doing right before the daemon died?" and a live-tracer
        // reattach can restore a session that was still waiting for input. The
        // sync re-reads the record instead of using `snap`, inside the
        // session's sidecar gate that every write of its sidecar takes, so
        // the last writer always writes the newest values.
        self.sync_sidecar(id);
        let _ = self
            .events
            .send(SessionEvent::Updated(Box::new(snap), origin));
        if entered_error {
            self.fan_out_attention(id.to_owned(), protocol::AttentionReason::Error);
        }
        true
    }

    pub fn fan_out_attention(&self, session_id: String, reason: protocol::AttentionReason) {
        let _ = self
            .events
            .send(SessionEvent::Attention { session_id, reason });
    }
}

/// Two-stage insert: caller patches the record (via `arc()`) and then calls
/// `publish()` to fire `SessionEvent::Updated`. Required when fields like
/// `scrollback_snapshot_req` need to be set by infrastructure that's only
/// available after the record is reachable in the registry — calling the
/// regular `insert()` would publish a half-wired snapshot first.
pub struct PendingInsert<'a> {
    registry: &'a SessionRegistry,
    arc: Arc<Mutex<SessionRecord>>,
}

impl PendingInsert<'_> {
    pub fn arc(&self) -> &Arc<Mutex<SessionRecord>> {
        &self.arc
    }

    pub fn publish(self) {
        self.publish_from(None);
    }

    /// Takes the unpublished record back out of the registry. No client
    /// heard of it, so nothing is broadcast.
    pub fn discard(self) {
        let id = lock(&self.arc).id.clone();
        write(&self.registry.by_id).remove(&id);
        lock(&self.registry.sidecar_gates).remove(&id);
    }

    /// [`Self::publish`], its broadcast carrying `origin`, which the record
    /// keeps as its spawn origin.
    pub fn publish_from(self, origin: Option<UpdateOrigin>) {
        let snap = {
            let mut rec = lock(&self.arc);
            rec.spawn_origin.clone_from(&origin);
            rec.snapshot()
        };
        let _ = self
            .registry
            .events
            .send(SessionEvent::Updated(Box::new(snap), origin));
    }
}

pub fn new_id() -> String {
    Uuid::new_v4().to_string()
}

/// Build the persistence-bound tail from the in-memory `recent_actions`
/// list. Caps at `RECENT_ACTIONS_TAIL_ENTRIES` newest entries, then
/// further drops from the front until the total UTF-8 byte size fits in
/// `RECENT_ACTIONS_TAIL_BYTE_CAP`. Always returns the most recent
/// entries (FIFO eviction).
pub fn trim_recent_tail(actions: &[String]) -> Vec<String> {
    let start = actions.len().saturating_sub(RECENT_ACTIONS_TAIL_ENTRIES);
    let mut tail: Vec<String> = actions[start..].to_vec();
    while !tail.is_empty()
        && tail.iter().map(String::len).sum::<usize>() > RECENT_ACTIONS_TAIL_BYTE_CAP
    {
        tail.remove(0);
    }
    tail
}

/// Build the scrollback replay handed to a freshly-attaching client: the
/// persisted history plus a leading bracketed-paste enable when the child had
/// that mode on, so xterm's paste flag is correct even if the enable sequence
/// was trimmed from the ring. Returns `(bytes, truncated)`; empty when `dirs`
/// is `None` (no persistence owner).
///
/// Shared with the server's no-live-PTY fallback so every path that hands a
/// client its scrollback re-asserts the same mode — the prefix has to come from
/// wherever the replay is assembled, not just the lifecycle task.
pub fn build_replay_snapshot(
    dirs: Option<&Dirs>,
    session_id: &str,
    bp_enabled: bool,
) -> (Vec<u8>, bool) {
    let Some(dirs) = dirs else {
        return (Vec::new(), false);
    };
    let (bytes, truncated) = scrollback::load(dirs, session_id);
    let prefix = termstate::replay_prefix(bp_enabled);
    if prefix.is_empty() {
        return (bytes, truncated);
    }
    let mut prefixed = Vec::with_capacity(prefix.len() + bytes.len());
    prefixed.extend_from_slice(prefix);
    prefixed.extend_from_slice(&bytes);
    (prefixed, truncated)
}

pub fn push_recent_action(rec: &mut SessionRecord, action: String) {
    rec.recent_actions.push(action);
    if rec.recent_actions.len() > RECENT_ACTIONS_CAP {
        let drop_count = rec.recent_actions.len() - RECENT_ACTIONS_CAP;
        rec.recent_actions.drain(0..drop_count);
    }
    rec.metrics.last_activity_at = Some(Utc::now());
}

/// Wires up the lifecycle tasks for a PTY session and returns the snapshot-
/// request sender the caller must store on the `SessionRecord`. The lifecycle
/// task:
///   - persists each PTY chunk to scrollback when `dirs.is_some()`, then
///     forwards it to every client that has taken a snapshot
///   - handles snapshot requests atomically: reads the scrollback file and
///     registers the client's channel in the same `tokio::select!` arm, so the
///     returned `ScrollbackSnapshot.live` starts at the next chunk NOT in
///     `data` — no overlap, no gap. This lets a freshly-spawned per-client
///     forwarder pick up exactly where the persisted scrollback left off.
///
/// `dirs` is `Some` for normal spawns + reattaches so the orphan-meta sidecar
/// gets cleaned up on exit and scrollback gets persisted; pass `None` for
/// callers that don't own the persistence (no current caller does, but the
/// option is preserved for symmetry with the previous signature).
///
/// Callers must invoke `attach_lifecycle` AFTER `insert_pending` so the exit
/// watcher's `registry.update` finds the session record. The returned
/// `snap_tx` should be patched onto the record's `scrollback_snapshot_req`
/// field before calling `PendingInsert::publish`.
pub fn attach_lifecycle(
    registry: &Arc<SessionRegistry>,
    session_id: String,
    pty: &Arc<PtyHandle>,
    dirs: Option<Dirs>,
) -> ScrollbackSnapshotReq {
    let (snap_tx, mut snap_rx) = mpsc::unbounded_channel::<oneshot::Sender<ScrollbackSnapshot>>();
    let output = pty.take_lifecycle_output();
    let session_for_task = session_id.clone();
    let dirs_for_task = dirs.clone();
    tokio::spawn(async move {
        let Some(mut output) = output else {
            error!(
                session_id = %session_for_task,
                "pty lifecycle output already taken; the session's output reaches no client"
            );
            return;
        };
        // Tracks whether snap_rx is still open. When all senders drop the
        // record's `scrollback_snapshot_req` (typically on session exit), the
        // receiver's `recv()` would return None on every poll — without the
        // guard the select! arm would spin.
        let mut snap_open = true;
        // One sender per client snapshot. This task is the only reader of the
        // session's lossless output queue: it forwards each chunk here, then
        // persists it, so a client sees every chunk after its snapshot in
        // order. Dropped with the task, which closes every client's `live`.
        let mut clients: Vec<mpsc::UnboundedSender<Vec<u8>>> = Vec::new();
        // Sticky bracketed-paste (DEC 2004) state, seeded from disk so it
        // survives across daemon restarts even when the scrollback ring has
        // trimmed the child's original `\e[?2004h`. Re-asserted as a prefix on
        // the replay snapshot below so a freshly-attached xterm's paste flag
        // matches the child. See `termstate` for the full rationale.
        let mut bp_tracker = BracketedPasteTracker::new(
            dirs_for_task
                .as_ref()
                .is_some_and(|d| termstate::load(d, &session_for_task)),
        );
        loop {
            tokio::select! {
                msg = output.recv() => {
                    match msg {
                        Some(bytes) => deliver_chunk(
                            &bytes,
                            &mut clients,
                            dirs_for_task.as_ref(),
                            &session_for_task,
                            &mut bp_tracker,
                        ),
                        None => break,
                    }
                }
                req = snap_rx.recv(), if snap_open => {
                    match req {
                        Some(reply) => {
                            let (data, truncated) = build_replay_snapshot(
                                dirs_for_task.as_ref(),
                                &session_for_task,
                                bp_tracker.enabled(),
                            );
                            // Exact by construction: only this task writes
                            // the scrollback file, between two chunks, so
                            // `data` holds every chunk it has read and none it
                            // has not. The client's sender joins `clients`
                            // before the task reads the next one, and the
                            // output queue is lossless, so every chunk still
                            // queued (sent during the file read) reaches
                            // scrollback, `live` and every other client
                            // together in the output arm.
                            let (tx, live) = mpsc::unbounded_channel::<Vec<u8>>();
                            clients.push(tx);
                            let _ = reply.send(ScrollbackSnapshot { data, truncated, live });
                        }
                        None => {
                            snap_open = false;
                        }
                    }
                }
            }
        }
    });

    // Exit watcher.
    if let Some(rx) = pty.take_exit() {
        tokio::spawn(watch_exit(Arc::clone(registry), session_id, rx, dirs));
    }

    snap_tx
}

/// One PTY chunk on its way through `attach_lifecycle`'s task: every attached
/// client gets it first, so client output never waits on scrollback I/O, then
/// it is appended to scrollback (when `dirs` is set) and the bracketed-paste
/// state is saved if the chunk changed it. Clients whose `live` receiver is
/// gone are dropped from `clients`.
fn deliver_chunk(
    bytes: &[u8],
    clients: &mut Vec<mpsc::UnboundedSender<Vec<u8>>>,
    dirs: Option<&Dirs>,
    session_id: &str,
    bp_tracker: &mut BracketedPasteTracker,
) {
    clients.retain(|tx| tx.send(bytes.to_vec()).is_ok());
    if let Some(d) = dirs {
        scrollback::append(d, session_id, bytes);
        if bp_tracker.observe(bytes) {
            termstate::save(d, session_id, bp_tracker.enabled());
        }
    }
}

/// Exit watcher: mark the session stopped once its PTY exit arrives, record
/// how it ended in the history, then drop its sidecar.
///
/// A tracer lost while the session still holds its pty leaves the session
/// abandoned instead: no exit code, sidecar kept for Resume, and an `Error`
/// attention. A session the user already stopped or parked has released its
/// pty, so its lost tracer ends it like any exit, and any exit after such a
/// stop is the kill's: it sets no exit code and asks for no attention.
async fn watch_exit(
    registry: Arc<SessionRegistry>,
    session_id: String,
    rx: oneshot::Receiver<PtyExit>,
    dirs: Option<Dirs>,
) {
    // A dropped sender means the reader task died without reporting an exit,
    // which is a lost tracer as far as anyone can tell.
    let exit = rx.await.unwrap_or(PtyExit::TracerLost);
    let code = exit.code();
    // Record the end before the `Stopped` update goes out. A client that
    // reacts to that update by discarding the session writes its own
    // `StoppedByUser` entry, and `history::write_if_absent` keeps whichever
    // lands first — so an exit that loses that race is recorded as thrown
    // away by the user instead of as the exit it was. The update below
    // touches nothing the entry is built from (label, members, spawn_config,
    // started_at) and nothing else it reads.
    if let Some(dirs) = dirs.as_ref() {
        history::record_session_end(&registry, dirs, &session_id, history::end_for_exit(exit));
    }
    let mut abandoned = false;
    let mut user_stopped = false;
    registry.update(&session_id, |rec| {
        abandoned = matches!(exit, PtyExit::TracerLost) && rec.pty.is_some();
        // A Stop or a Park claims the session (stopped, pty taken) before the
        // kill, so this exit is the kill's: no code of its own, no attention.
        user_stopped = rec.pty.is_none() && rec.status == SessionStatus::Stopped;
        rec.status = SessionStatus::Stopped;
        // A stopped session has nothing left waiting on it.
        rec.pending_input = None;
        rec.background = false;
        rec.pty = None;
        rec.input_notifier = None;
        rec.scrollback_snapshot_req = None;
        if abandoned {
            rec.exit_code = None;
            rec.is_abandoned = true;
            push_recent_action(rec, "abandoned: tracer lost".to_string());
        } else {
            if !user_stopped {
                rec.exit_code = Some(code);
            }
            push_recent_action(rec, format!("exited with code {code}"));
        }
    });
    if abandoned {
        registry.fan_out_attention(session_id.clone(), protocol::AttentionReason::Error);
        return;
    }
    if !user_stopped {
        registry.fan_out_attention(session_id.clone(), protocol::AttentionReason::Stopped);
    }
    registry.delete_sidecar(&session_id);
}

/// Build a [`SessionRecord`] from a sidecar [`OrphanMeta`] and surface it via
/// `insert` so all attached clients see the reattached session in their next
/// snapshot. The PTY/headless handles are `None` because we missed the spawn
/// moment — the underlying agent is still running but its stdio is detached
/// from us. Status is set conservatively to `Idle` until something proves
/// otherwise.
/// Resolve `(default_label, user_label, effective_label)` from an
/// on-disk sidecar. Legacy sidecars (pre-rename) carry only `label`;
/// treat that as the default with no user override. Newer sidecars
/// carry `default_label` + `user_label` explicitly.
fn resolve_labels_from_meta(meta: &OrphanMeta) -> (String, Option<String>, String) {
    let default_label = meta
        .default_label
        .clone()
        .unwrap_or_else(|| meta.label.clone());
    let user_label = meta.user_label.clone();
    let effective = user_label.clone().unwrap_or_else(|| default_label.clone());
    (default_label, user_label, effective)
}

fn appearance_from_meta(meta: &OrphanMeta) -> AppearanceOverrides {
    let mut appearance = meta.appearance.clone();
    if appearance.accent_color.is_none() {
        appearance.accent_color.clone_from(&meta.accent_color);
    }
    appearance
}

impl SessionRegistry {
    pub fn insert_orphan(&self, meta: &OrphanMeta) {
        let (default_label, user_label, label) = resolve_labels_from_meta(meta);
        let mut record = SessionRecord {
            id: meta.session_id.clone(),
            label,
            default_label,
            user_label,
            kind: meta.kind.clone(),
            members: meta.members.clone(),
            mode: meta.mode,
            started_at: meta.started_at,
            status: SessionStatus::Idle,
            status_since: Some(Utc::now()),
            exit_code: None,
            metrics: SessionMetrics::default(),
            recent_actions: meta.recent_actions_tail.clone(),
            pty: None,
            headless: None,
            workspace_id: meta.workspace_id.clone(),
            agent: meta.agent.clone().unwrap_or_else(default_agent),
            terminal_title: meta.terminal_title.clone(),
            program_name: meta.program_name.clone(),
            current_cwd: meta.current_cwd.clone(),
            appearance: appearance_from_meta(meta),
            spawn_config: meta.spawn_config.clone(),
            is_abandoned: false,
            is_inactive: false,
            worktree_paths: Vec::new(),
            last_prompt: meta.last_prompt.clone(),
            input_notifier: None,
            screen: None,
            scrollback_snapshot_req: None,
            spawn_origin: None,
            conversation_id: meta.conversation_id.clone(),
            pending_input: None,
            last_hook_time: None,
            hook_driven: false,
            background: false,
            last_assistant_message: None,
            busy_excluded: meta.busy_excluded,
            agent_in_shell: meta.agent_in_shell,
            shell_label: meta.shell_label.clone(),
            agent_exit_announced: false,
            size_set: false,
            hook_agent: meta.hook_agent.clone(),
        };
        push_recent_action(&mut record, "reattached after daemon restart".to_string());
        self.insert(record);
    }

    /// Reattach a sidecar whose tracer process is still alive. Unlike
    /// `insert_orphan` this session has a real [`PtyHandle`] wired up to the
    /// tracer's pipe — input, output, resize, and kill all work as if the
    /// daemon had spawned it directly. Surfaced with `is_abandoned = false`
    /// and the same overall shape as a fresh spawn.
    ///
    /// The status comes from the sidecar when the session was waiting for
    /// input or sitting idle when the daemon went away, so it comes back that
    /// way with the stamp it had. Every other stored status is one the
    /// reattach has no evidence for, and starts at `Idle` stamped now. The
    /// status watcher then checks either against the replayed screen once its
    /// startup hold ends ([`crate::pty_state::watch`]): a spinner promotes it
    /// to `Working`, and an idle box clears a restored `AwaitingInput`.
    ///
    /// Returns a [`PendingInsert`] so the caller can patch
    /// `scrollback_snapshot_req` (produced by [`attach_lifecycle`]) onto the
    /// record before publishing the `SessionUpdated` snapshot.
    pub fn insert_reattached(&self, meta: &OrphanMeta, pty: Arc<PtyHandle>) -> PendingInsert<'_> {
        let (default_label, user_label, label) = resolve_labels_from_meta(meta);
        let (status, status_since) = match (meta.status, meta.status_since) {
            (
                Some(restored @ (SessionStatus::Idle | SessionStatus::AwaitingInput)),
                Some(since),
            ) => (restored, since),
            _ => (SessionStatus::Idle, Utc::now()),
        };
        // A prompt is only pending beside the status that waits on it.
        let pending_input = if status == SessionStatus::AwaitingInput {
            meta.pending_input.clone()
        } else {
            None
        };
        let mut record = SessionRecord {
            id: meta.session_id.clone(),
            label,
            default_label,
            user_label,
            kind: meta.kind.clone(),
            members: meta.members.clone(),
            mode: meta.mode,
            started_at: meta.started_at,
            status,
            status_since: Some(status_since),
            exit_code: None,
            metrics: SessionMetrics::default(),
            recent_actions: meta.recent_actions_tail.clone(),
            pty: Some(pty),
            headless: None,
            workspace_id: meta.workspace_id.clone(),
            agent: meta.agent.clone().unwrap_or_else(default_agent),
            terminal_title: meta.terminal_title.clone(),
            program_name: meta.program_name.clone(),
            current_cwd: meta.current_cwd.clone(),
            appearance: appearance_from_meta(meta),
            spawn_config: meta.spawn_config.clone(),
            is_abandoned: false,
            is_inactive: false,
            worktree_paths: Vec::new(),
            last_prompt: meta.last_prompt.clone(),
            input_notifier: None,
            screen: None,
            scrollback_snapshot_req: None,
            spawn_origin: None,
            conversation_id: meta.conversation_id.clone(),
            pending_input,
            last_hook_time: None,
            hook_driven: false,
            background: false,
            last_assistant_message: None,
            busy_excluded: meta.busy_excluded,
            agent_in_shell: meta.agent_in_shell,
            shell_label: meta.shell_label.clone(),
            agent_exit_announced: false,
            size_set: false,
            hook_agent: meta.hook_agent.clone(),
        };
        push_recent_action(
            &mut record,
            "reattached to live tracer after daemon restart".to_string(),
        );
        self.insert_pending(record)
    }

    /// Reattach a sidecar whose recorded pid is no longer alive: a daemon
    /// crash, a reboot or a killed process tree ended the session while no
    /// daemon watched it. Surfaced to clients with
    /// `is_abandoned = true` so the sidebar can offer Resume.
    pub fn insert_abandoned(&self, meta: &OrphanMeta) {
        let (default_label, user_label, label) = resolve_labels_from_meta(meta);
        let mut record = SessionRecord {
            id: meta.session_id.clone(),
            label,
            default_label,
            user_label,
            kind: meta.kind.clone(),
            members: meta.members.clone(),
            mode: meta.mode,
            started_at: meta.started_at,
            status: SessionStatus::Stopped,
            status_since: Some(Utc::now()),
            exit_code: None,
            metrics: SessionMetrics::default(),
            recent_actions: meta.recent_actions_tail.clone(),
            pty: None,
            headless: None,
            workspace_id: meta.workspace_id.clone(),
            agent: meta.agent.clone().unwrap_or_else(default_agent),
            terminal_title: meta.terminal_title.clone(),
            program_name: meta.program_name.clone(),
            current_cwd: meta.current_cwd.clone(),
            appearance: appearance_from_meta(meta),
            spawn_config: meta.spawn_config.clone(),
            is_abandoned: true,
            is_inactive: false,
            worktree_paths: Vec::new(),
            last_prompt: meta.last_prompt.clone(),
            input_notifier: None,
            screen: None,
            scrollback_snapshot_req: None,
            spawn_origin: None,
            conversation_id: meta.conversation_id.clone(),
            pending_input: None,
            last_hook_time: None,
            hook_driven: false,
            background: false,
            last_assistant_message: None,
            busy_excluded: meta.busy_excluded,
            agent_in_shell: meta.agent_in_shell,
            shell_label: meta.shell_label.clone(),
            agent_exit_announced: false,
            size_set: false,
            hook_agent: meta.hook_agent.clone(),
        };
        push_recent_action(
            &mut record,
            "abandoned: a daemon crash, a reboot or a killed process tree ended this session"
                .to_string(),
        );
        self.insert(record);
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::pty::PtyOutput;

    /// Scratch config dir for the replay-snapshot tests.
    fn scratch_dirs(tag: &str) -> Dirs {
        let root = std::env::temp_dir().join(format!("rt-session-{}-{tag}", std::process::id()));
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

    /// A headless record with an empty history.
    fn record(id: &str) -> SessionRecord {
        SessionRecord {
            id: id.to_string(),
            label: id.to_string(),
            default_label: id.to_string(),
            user_label: None,
            kind: SessionKind::Standalone,
            members: Vec::new(),
            mode: SessionMode::Headless,
            started_at: Utc::now(),
            status: SessionStatus::Idle,
            status_since: Some(Utc::now()),
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
            is_abandoned: false,
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
            busy_excluded: false,
            agent_in_shell: false,
            shell_label: None,
            agent_exit_announced: false,
            size_set: false,
            hook_agent: None,
        }
    }

    /// A stored config whose agent is Claude on the `DeepSeek` provider.
    fn deepseek_config(mode: SessionMode) -> SpawnConfig {
        SpawnConfig {
            target: protocol::SpawnTarget::Standalone {
                cwd: None,
                add_dirs: Vec::new(),
            },
            mode,
            dangerously_skip_permissions: false,
            agent: Some(protocol::AgentSelection {
                agent: agent_registry::default_agent(),
                options: protocol::OptionValues::from([(
                    "provider".to_owned(),
                    protocol::OptionValue::Text("deepseek".to_owned()),
                )]),
            }),
            extra_env: Vec::new(),
        }
    }

    #[test]
    fn a_deepseek_snapshot_carries_the_provider_chip() {
        let mut rec = record("s1");
        rec.spawn_config = Some(deepseek_config(SessionMode::Interactive));
        let snapshot = rec.snapshot();
        assert_eq!(snapshot.agent, Some(agent_registry::default_agent()));
        let texts: Vec<&str> = snapshot
            .agent_chips
            .iter()
            .map(|chip| chip.text.as_str())
            .collect();
        assert_eq!(texts, ["DeepSeek"], "the chip shows the choice's label");
        assert!(
            snapshot.agent_chips.iter().all(|chip| chip.tip.is_some()),
            "{:?}",
            snapshot.agent_chips
        );
    }

    #[test]
    fn a_record_without_a_spawn_config_shows_no_provider_chip() {
        assert_eq!(record("s1").snapshot().agent_chips, []);
    }

    #[test]
    fn a_shell_snapshot_has_no_agent_and_no_chips() {
        let mut rec = record("s1");
        rec.mode = SessionMode::PlainShell;
        rec.spawn_config = Some(deepseek_config(SessionMode::PlainShell));
        let snapshot = rec.snapshot();
        assert_eq!(snapshot.agent, None);
        assert_eq!(snapshot.agent_chips, []);
    }

    #[test]
    fn push_recent_action_keeps_the_cap_and_drops_the_oldest() {
        let mut rec = record("s1");
        for i in 0..=RECENT_ACTIONS_CAP {
            push_recent_action(&mut rec, format!("action {i}"));
        }
        assert_eq!(
            RECENT_ACTIONS_CAP, 200,
            "the clients' headless log draws 200 rows"
        );
        assert_eq!(rec.recent_actions.len(), RECENT_ACTIONS_CAP);
        assert_eq!(
            rec.recent_actions.first().map(String::as_str),
            Some("action 1"),
            "the oldest went"
        );
        assert_eq!(
            rec.recent_actions.last().map(String::as_str),
            Some("action 200")
        );
    }

    #[test]
    fn replay_snapshot_prefixes_bracketed_paste_when_enabled() {
        let dirs = scratch_dirs("replay-on");
        crate::scrollback::append(&dirs, "s1", b"history bytes");
        let (data, truncated) = build_replay_snapshot(Some(&dirs), "s1", true);
        assert!(!truncated);
        assert!(
            data.starts_with(b"\x1b[?2004h"),
            "replay must re-assert bracketed paste"
        );
        assert!(data.ends_with(b"history bytes"), "history must survive");
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn replay_snapshot_is_unprefixed_when_disabled() {
        let dirs = scratch_dirs("replay-off");
        crate::scrollback::append(&dirs, "s1", b"history bytes");
        let (data, _) = build_replay_snapshot(Some(&dirs), "s1", false);
        assert_eq!(data, b"history bytes");
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn replay_snapshot_without_dirs_is_empty() {
        assert_eq!(build_replay_snapshot(None, "s1", true), (Vec::new(), false));
    }

    #[test]
    fn trim_recent_tail_keeps_last_n_entries() {
        let actions: Vec<String> = (0..20).map(|i| format!("action {i}")).collect();
        let tail = trim_recent_tail(&actions);
        assert_eq!(tail.len(), RECENT_ACTIONS_TAIL_ENTRIES);
        // The newest entries should survive.
        assert_eq!(tail.last().map(String::as_str), Some("action 19"));
    }

    #[test]
    fn trim_recent_tail_drops_under_byte_cap() {
        // Single huge entry: must exceed the cap on its own.
        let huge = "x".repeat(RECENT_ACTIONS_TAIL_BYTE_CAP * 2);
        let actions = vec![huge.clone(), "small".to_string()];
        let tail = trim_recent_tail(&actions);
        // The huge one gets dropped to fit under the byte cap.
        assert_eq!(tail, vec!["small".to_string()]);
    }

    #[test]
    fn trim_recent_tail_empty_input_empty_output() {
        let tail = trim_recent_tail(&[]);
        assert_eq!(tail, [] as [std::string::String; 0]);
    }

    /// The attention reasons `events` holds, draining it.
    fn drained_attention(
        events: &mut broadcast::Receiver<SessionEvent>,
    ) -> Vec<protocol::AttentionReason> {
        let mut reasons = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let SessionEvent::Attention { reason, .. } = event {
                reasons.push(reason);
            }
        }
        reasons
    }

    #[test]
    fn update_into_error_fans_out_error_attention_once() {
        use crate::history::test_support::{record, scratch_dirs};
        let dirs = scratch_dirs("error-attention");
        let registry = SessionRegistry::new(dirs.clone());
        registry.insert(record("s1", protocol::SessionMode::Headless));
        let mut events = registry.subscribe();

        registry.update("s1", |rec| rec.status = SessionStatus::Error);
        assert_eq!(
            drained_attention(&mut events),
            [protocol::AttentionReason::Error]
        );

        registry.update("s1", |rec| rec.status = SessionStatus::Error);
        assert!(drained_attention(&mut events).is_empty(), "already Error");

        registry.update("s1", |rec| rec.status = SessionStatus::Working);
        registry.update("s1", |rec| rec.status = SessionStatus::Error);
        assert_eq!(
            drained_attention(&mut events),
            [protocol::AttentionReason::Error]
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn status_change_stamps_status_since() {
        use crate::history::test_support::{record, scratch_dirs};
        let dirs = scratch_dirs("status-since-stamp");
        let registry = SessionRegistry::new(dirs.clone());
        let before = Utc::now() - chrono::Duration::seconds(60);
        let mut rec = record("s1", SessionMode::Headless);
        rec.status_since = Some(before);
        registry.insert(rec);

        registry.update("s1", |rec| rec.status = SessionStatus::Working);

        let snapshots = registry.snapshots();
        let since = snapshots.first().and_then(|s| s.status_since);
        assert!(
            since.is_some_and(|t| t > before),
            "a status change stamps a fresh time: {since:?}"
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn same_status_update_keeps_status_since() {
        use crate::history::test_support::{record, scratch_dirs};
        let dirs = scratch_dirs("status-since-keep");
        let registry = SessionRegistry::new(dirs.clone());
        let stamped = DateTime::from_timestamp(1_700_000_000, 0).expect("a fixed stamp");
        let mut rec = record("s1", SessionMode::Headless);
        rec.status = SessionStatus::Working;
        rec.status_since = Some(stamped);
        registry.insert(rec);

        registry.update("s1", |rec| rec.exit_code = Some(0));
        registry.update("s1", |rec| rec.status = SessionStatus::Working);

        let snapshots = registry.snapshots();
        assert_eq!(
            snapshots.first().and_then(|s| s.status_since),
            Some(stamped),
            "an update that leaves the status alone keeps the stamp"
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn new_record_has_status_since() {
        use crate::history::test_support::scratch_dirs;
        let dirs = scratch_dirs("status-since-new");
        let registry = SessionRegistry::new(dirs.clone());
        let meta = crate::orphan::meta_from_record(
            "s1".to_string(),
            1,
            "s1".to_string(),
            SessionKind::Standalone,
            SessionMode::Interactive,
            Vec::new(),
            Utc::now(),
            None,
            None,
            Some(agent_registry::default_agent()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("build meta");
        registry.insert_orphan(&meta);

        let snapshots = registry.snapshots();
        assert_eq!(snapshots.len(), 1);
        assert!(
            snapshots[0].status_since.is_some(),
            "every record-creation path stamps one"
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// Write a sidecar for `id` the way a daemon that synced its status
    /// leaves it. The fixture is literal JSON: it pins the on-disk shape
    /// rather than the struct this build happens to serialize.
    fn write_sidecar_with_status(dirs: &Dirs, id: &str, status: &str, since: DateTime<Utc>) {
        write_sidecar_with_fields(dirs, id, status, since, "");
    }

    /// [`write_sidecar_with_status`] with `extra` (comma-led JSON members)
    /// appended to the object.
    fn write_sidecar_with_fields(
        dirs: &Dirs,
        id: &str,
        status: &str,
        since: DateTime<Utc>,
        extra: &str,
    ) {
        let dir = dirs.sessions_dir.join(id);
        std::fs::create_dir_all(&dir).expect("create session dir");
        let since = since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let json = format!(
            r#"{{"on_disk_version": 2, "session_id": "{id}", "pid": 1234, "label": "{id}",
                "kind": "standalone", "mode": "interactive", "members": [],
                "started_at": "2024-01-01T00:00:00Z",
                "status": "{status}", "status_since": "{since}"{extra}}}"#
        );
        std::fs::write(dir.join("meta.json"), json).expect("write meta.json");
    }

    const PENDING_BASH: &str = r#", "pending_input": {"kind": "permission", "tool_name": "Bash", "summary": "cargo build"}"#;

    /// Reattach `s1` from a sidecar holding `status` and a pending Bash
    /// prompt; answers the restored snapshot's pending input.
    fn reattach_with_pending(tag: &str, status: &str) -> Option<PendingInput> {
        use crate::history::test_support::{fake_pty, scratch_dirs};
        let dirs = scratch_dirs(tag);
        let since = DateTime::from_timestamp(1_700_000_000, 0).expect("a fixed stamp");
        write_sidecar_with_fields(&dirs, "s1", status, since, PENDING_BASH);
        let meta = crate::orphan::load_meta(&dirs, "s1").expect("load meta");
        let registry = SessionRegistry::new(dirs.clone());
        let (pty, _exit_tx) = fake_pty();
        registry.insert_reattached(&meta, pty).publish();
        let snapshots = registry.snapshots();
        let _ = std::fs::remove_dir_all(&dirs.config);
        assert_eq!(snapshots.len(), 1);
        snapshots[0].pending_input.clone()
    }

    #[tokio::test]
    async fn reattach_restores_pending_input_with_awaiting_input() {
        assert_eq!(
            reattach_with_pending("reattach-pending-awaiting", "awaiting_input"),
            Some(PendingInput::Permission {
                tool_name: "Bash".to_owned(),
                summary: "cargo build".to_owned(),
            }),
            "a restored prompt keeps what it was asking"
        );
    }

    #[tokio::test]
    async fn reattach_drops_pending_input_when_not_awaiting_input() {
        for status in ["idle", "working"] {
            assert_eq!(
                reattach_with_pending(&format!("reattach-pending-{status}"), status),
                None,
                "a {status} session restores no prompt"
            );
        }
    }

    /// The exit watcher's history write must land before the `Stopped` update
    /// reaches clients: a client that reacts to that update by discarding the
    /// session writes its own `StoppedByUser` entry, and only the first write
    /// survives. The history write lock is held across the broadcast so the
    /// check cannot race the write it is checking for — the writer parks on
    /// the lock, and the guard is only released afterwards.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[expect(
        clippy::await_holding_lock,
        reason = "holding the history write lock across the broadcast is this test's mechanism: \
                  it parks the writer so the check cannot race the write it is looking for"
    )]
    async fn exit_end_is_recorded_before_stopped_broadcast() {
        use crate::history::test_support::{
            fake_pty, hold_history_write_lock, insert_live, record, scratch_dirs, wait_for_entry,
        };
        use crate::pty::PtyExit;
        use protocol::SessionEnd;
        use std::time::Duration;

        let dirs = scratch_dirs("exit-end-before-broadcast");
        let registry = SessionRegistry::new(dirs.clone());
        let (pty, exit_tx) = fake_pty();
        insert_live(
            &registry,
            &dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        let mut events = registry.subscribe();

        let guard = hold_history_write_lock();
        let _ = exit_tx.send(PtyExit::Code(0));
        let early =
            tokio::time::timeout(Duration::from_millis(200), next_stopped(&mut events)).await;
        assert!(
            early.is_err(),
            "no Stopped update reaches clients while the history write is parked"
        );
        drop(guard);

        tokio::time::timeout(Duration::from_secs(5), next_stopped(&mut events))
            .await
            .expect("the Stopped update follows the history write");
        let entry = crate::history::read_one(&dirs, "s1")
            .expect("the history entry exists when the Stopped update is observed");
        assert_eq!(entry.end, SessionEnd::Exited { code: 0 });
        let entry = wait_for_entry(&dirs, "s1").await;
        assert_eq!(entry.end, SessionEnd::Exited { code: 0 });
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// Wait for the next update that reports the session `Stopped`.
    async fn next_stopped(events: &mut broadcast::Receiver<SessionEvent>) {
        loop {
            match events.recv().await {
                Ok(SessionEvent::Updated(snap, _)) if snap.status == SessionStatus::Stopped => {
                    return;
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => std::future::pending().await,
            }
        }
    }

    /// A session that exits while a hook prompt is pending stops with no
    /// prompt left on it: a stopped session has nothing to answer.
    #[tokio::test]
    async fn an_exited_session_drops_its_pending_input() {
        use crate::history::test_support::{fake_pty, insert_live, record, scratch_dirs};
        use crate::pty::PtyExit;
        use std::time::Duration;

        let dirs = scratch_dirs("exit-drops-pending");
        let registry = SessionRegistry::new(dirs.clone());
        let (pty, exit_tx) = fake_pty();
        let mut rec = record("s1", SessionMode::Interactive);
        rec.status = SessionStatus::AwaitingInput;
        rec.pending_input = Some(PendingInput::Permission {
            tool_name: "Bash".to_owned(),
            summary: "cargo build".to_owned(),
        });
        rec.hook_driven = true;
        rec.background = true;
        insert_live(&registry, &dirs, rec, &pty);
        let mut events = registry.subscribe();

        exit_tx.send(PtyExit::Code(0)).expect("report the exit");
        tokio::time::timeout(Duration::from_secs(5), next_stopped(&mut events))
            .await
            .expect("the Stopped update arrives");

        let rec = registry.get("s1").expect("the session is still listed");
        let record = crate::sync::lock(&rec);
        assert_eq!(record.status, SessionStatus::Stopped);
        assert_eq!(record.pending_input, None, "the prompt left with the exit");
        assert!(!record.background, "no background work outlives the exit");
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// A Stop claims the session (status stopped, pty taken) before the
    /// kill; the child's exit that follows is the kill's, so it sets no exit
    /// code and asks for no attention.
    #[tokio::test]
    async fn a_user_stop_sends_no_attention_and_no_exit_code() {
        use crate::history::test_support::{
            fake_pty, insert_live, record, scratch_dirs, sidecar_gone, write_meta_for,
        };
        use crate::pty::PtyExit;

        let dirs = scratch_dirs("user-stop-quiet");
        let registry = SessionRegistry::new(dirs.clone());
        let (pty, exit_tx) = fake_pty();
        insert_live(
            &registry,
            &dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        write_meta_for(&dirs, "s1");
        let mut events = registry.subscribe();
        registry.update("s1", |rec| {
            rec.status = SessionStatus::Stopped;
            rec.pty = None;
        });

        exit_tx.send(PtyExit::Code(1)).expect("report the exit");
        assert!(sidecar_gone(&dirs, "s1").await, "the watcher ran");

        assert!(
            !drained_attention(&mut events).contains(&protocol::AttentionReason::Stopped),
            "a user stop asks for no attention"
        );
        let rec = registry.get("s1").expect("the session is still listed");
        assert_eq!(crate::sync::lock(&rec).exit_code, None);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// A child that exits on its own still reports its code and asks for
    /// attention.
    #[tokio::test]
    async fn a_self_exit_still_reports_its_code() {
        use crate::history::test_support::{
            fake_pty, insert_live, record, scratch_dirs, sidecar_gone, write_meta_for,
        };
        use crate::pty::PtyExit;

        let dirs = scratch_dirs("self-exit-reports");
        let registry = SessionRegistry::new(dirs.clone());
        let (pty, exit_tx) = fake_pty();
        insert_live(
            &registry,
            &dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        write_meta_for(&dirs, "s1");
        let mut events = registry.subscribe();

        exit_tx.send(PtyExit::Code(1)).expect("report the exit");
        assert!(sidecar_gone(&dirs, "s1").await, "the watcher ran");

        assert!(
            drained_attention(&mut events).contains(&protocol::AttentionReason::Stopped),
            "a self-exit asks for attention"
        );
        let rec = registry.get("s1").expect("the session is still listed");
        assert_eq!(crate::sync::lock(&rec).exit_code, Some(1));
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// Every status change must reach the sidecar, in the same read-modify-
    /// write that carries the recent-actions tail.
    #[tokio::test]
    async fn status_change_is_synced_to_sidecar() {
        use crate::history::test_support::{
            fake_pty, insert_live, record, scratch_dirs, write_meta_for,
        };
        let dirs = scratch_dirs("status-synced-to-sidecar");
        write_meta_for(&dirs, "s1");
        let registry = SessionRegistry::new(dirs.clone());
        let (pty, _exit_tx) = fake_pty();
        insert_live(
            &registry,
            &dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );

        registry.update("s1", |rec| rec.status = SessionStatus::AwaitingInput);
        let stamped = registry
            .snapshots()
            .first()
            .and_then(|s| s.status_since)
            .expect("a status change stamps one");
        let meta = crate::orphan::load_meta(&dirs, "s1").expect("load meta");
        assert_eq!(meta.status, Some(SessionStatus::AwaitingInput));
        assert_eq!(meta.status_since, Some(stamped));

        registry.update("s1", |rec| rec.exit_code = Some(0));
        let meta = crate::orphan::load_meta(&dirs, "s1").expect("load meta");
        assert_eq!(
            meta.status_since,
            Some(stamped),
            "an update that leaves the status alone leaves the stored stamp alone"
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// A title, cwd, rename or appearance change and a status change each
    /// reach the sidecar through the one record sync, so neither can put back
    /// a stale copy of the other's field: the sidecar ends holding the latest
    /// status and stamp alongside the latest title, cwd, labels and look.
    #[test]
    fn title_and_status_updates_both_reach_the_sidecar() {
        use crate::history::test_support::{record, scratch_dirs, write_meta_for};
        let dirs = scratch_dirs("sidecar-title-and-status");
        write_meta_for(&dirs, "s1");
        let registry = SessionRegistry::new(dirs.clone());
        registry.insert(record("s1", SessionMode::Interactive));

        registry.update("s1", |rec| rec.status = SessionStatus::AwaitingInput);
        registry.update("s1", |rec| rec.terminal_title = Some("build".to_string()));
        registry.update("s1", |rec| rec.current_cwd = Some("C:\\work".to_string()));
        registry.update("s1", |rec| {
            rec.user_label = Some("mine".to_string());
            rec.label = "mine".to_string();
        });
        registry.update("s1", |rec| {
            rec.appearance.accent_color = Some("#38bdf8".to_string());
        });

        let snap = registry.snapshots().pop().expect("the session");
        let meta = crate::orphan::load_meta(&dirs, "s1").expect("load meta");
        assert_eq!(meta.status, Some(SessionStatus::AwaitingInput));
        assert_eq!(meta.status_since, snap.status_since);
        assert_eq!(meta.terminal_title.as_deref(), Some("build"));
        assert_eq!(meta.current_cwd.as_deref(), Some("C:\\work"));
        assert_eq!(meta.user_label.as_deref(), Some("mine"));
        assert_eq!(meta.label, "mine");
        assert_eq!(meta.appearance.accent_color.as_deref(), Some("#38bdf8"));
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// A standalone interactive session's sidecar.
    fn standalone_meta() -> OrphanMeta {
        crate::orphan::meta_from_record(
            "s1".to_string(),
            1,
            "s1".to_string(),
            SessionKind::Standalone,
            SessionMode::Interactive,
            Vec::new(),
            Utc::now(),
            None,
            None,
            Some(agent_registry::default_agent()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("build meta")
    }

    #[test]
    fn an_excluded_record_snapshot_carries_the_busy_flag() {
        let mut rec = record("s1");
        assert!(!rec.snapshot().busy_excluded, "a record starts counted");
        rec.busy_excluded = true;
        assert!(rec.snapshot().busy_excluded);
    }

    #[test]
    fn setting_busy_excluded_on_the_record_reaches_the_sidecar() {
        use crate::history::test_support::{record, scratch_dirs, write_meta_for};
        let dirs = scratch_dirs("sidecar-busy-excluded");
        write_meta_for(&dirs, "s1");
        let registry = SessionRegistry::new(dirs.clone());
        registry.insert(record("s1", SessionMode::Interactive));

        registry.update("s1", |rec| rec.busy_excluded = true);
        let meta = crate::orphan::load_meta(&dirs, "s1").expect("load meta");
        assert!(meta.busy_excluded, "the flag is mirrored into the sidecar");

        registry.update("s1", |rec| rec.busy_excluded = false);
        let meta = crate::orphan::load_meta(&dirs, "s1").expect("load meta");
        assert!(!meta.busy_excluded, "clearing it is mirrored too");
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// A sidecar for `s1` whose session does not count as busy.
    fn excluded_meta() -> OrphanMeta {
        let mut meta = standalone_meta();
        meta.busy_excluded = true;
        meta
    }

    fn assert_keeps_busy_excluded(registry: &SessionRegistry) {
        let arc = registry.get("s1").expect("the inserted session");
        assert!(lock(&arc).busy_excluded, "the sidecar's flag is restored");
    }

    #[test]
    fn abandoned_insert_keeps_busy_excluded() {
        use crate::history::test_support::scratch_dirs;
        let dirs = scratch_dirs("abandoned-busy-excluded");
        let registry = SessionRegistry::new(dirs.clone());
        registry.insert_abandoned(&excluded_meta());
        assert_keeps_busy_excluded(&registry);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn orphan_insert_keeps_busy_excluded() {
        use crate::history::test_support::scratch_dirs;
        let dirs = scratch_dirs("orphan-busy-excluded");
        let registry = SessionRegistry::new(dirs.clone());
        registry.insert_orphan(&excluded_meta());
        assert_keeps_busy_excluded(&registry);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[tokio::test]
    async fn reattached_insert_keeps_busy_excluded() {
        use crate::history::test_support::{fake_pty, scratch_dirs};
        let dirs = scratch_dirs("reattached-busy-excluded");
        let registry = SessionRegistry::new(dirs.clone());
        let (pty, _exit_tx) = fake_pty();
        registry.insert_reattached(&excluded_meta(), pty).publish();
        assert_keeps_busy_excluded(&registry);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// A recovered shell reloaded from its sidecar, through every insert a
    /// restart uses, keeps the agent its hooks map with.
    #[tokio::test]
    async fn a_reloaded_shell_sidecar_keeps_its_hook_agent() {
        use crate::history::test_support::{fake_pty, scratch_dirs};
        let dirs = scratch_dirs("reloaded-shell-hook-agent");
        let mut meta = standalone_meta();
        meta.mode = SessionMode::PlainShell;
        meta.hook_agent = Some(agent_registry::default_agent());
        let hook_agent_of = |registry: &SessionRegistry| {
            let arc = registry.get("s1").expect("the inserted session");
            let record = lock(&arc);
            (record.hook_agent.clone(), record.hook_plugin_agent())
        };
        let expected = (
            Some(agent_registry::default_agent()),
            Some(agent_registry::default_agent()),
        );

        let orphan = SessionRegistry::new(dirs.clone());
        orphan.insert_orphan(&meta);
        assert_eq!(hook_agent_of(&orphan), expected, "orphan insert");
        let abandoned = SessionRegistry::new(dirs.clone());
        abandoned.insert_abandoned(&meta);
        assert_eq!(hook_agent_of(&abandoned), expected, "abandoned insert");
        let reattached = SessionRegistry::new(dirs.clone());
        let (pty, _exit_tx) = fake_pty();
        reattached.insert_reattached(&meta, pty).publish();
        assert_eq!(hook_agent_of(&reattached), expected, "reattached insert");

        meta.hook_agent = None;
        let without = SessionRegistry::new(dirs.clone());
        without.insert_orphan(&meta);
        assert_eq!(
            hook_agent_of(&without),
            (None, None),
            "a shell sidecar without one maps no hook event"
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    fn meta_file(dirs: &Dirs, id: &str) -> std::path::PathBuf {
        dirs.sessions_dir.join(id).join("meta.json")
    }

    /// Syncs parked behind the session's sidecar gate while the record moves
    /// on must write the record as it is when they run, not as it was when
    /// they were asked for: each reads the record afresh inside the gate.
    #[test]
    fn syncs_parked_behind_the_gate_write_the_latest_record() {
        use crate::history::test_support::{record, scratch_dirs, write_meta_for};
        let dirs = scratch_dirs("sidecar-gate");
        write_meta_for(&dirs, "s1");
        let registry = SessionRegistry::new(dirs.clone());
        registry.insert(record("s1", SessionMode::Interactive));
        let older = DateTime::from_timestamp(1_600_000_000, 0).expect("a fixed stamp");
        let newer = DateTime::from_timestamp(1_700_000_000, 0).expect("a fixed stamp");
        let rec = registry.get("s1").expect("the record");
        {
            let mut guard = lock(&rec);
            guard.status = SessionStatus::Working;
            guard.status_since = Some(older);
            guard.terminal_title = Some("old".to_string());
        }

        let gate = registry.hold_sidecar_sync("s1");
        let held = lock(&gate);
        let syncs: Vec<_> = (0..2)
            .map(|_| {
                let registry = Arc::clone(&registry);
                std::thread::spawn(move || registry.sync_sidecar("s1"))
            })
            .collect();
        // The map's handle, the test's and one per parked sync.
        wait_until_gate_is_shared(&gate, 4);
        {
            let mut guard = lock(&rec);
            guard.status = SessionStatus::AwaitingInput;
            guard.status_since = Some(newer);
            guard.terminal_title = Some("new".to_string());
        }
        drop(held);
        for sync in syncs {
            sync.join().expect("a sync thread");
        }

        let meta = crate::orphan::load_meta(&dirs, "s1").expect("load meta");
        assert_eq!(meta.status, Some(SessionStatus::AwaitingInput));
        assert_eq!(meta.status_since, Some(newer));
        assert_eq!(meta.terminal_title.as_deref(), Some("new"));
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// Block until `gate` has `handles` owners: every writer and the delete
    /// clone the gate just before they lock it, so a count this high means
    /// that many of them are at the gate.
    fn wait_until_gate_is_shared(gate: &Arc<Mutex<SidecarGate>>, handles: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while Arc::strong_count(gate) < handles {
            assert!(
                std::time::Instant::now() < deadline,
                "only {} of {handles} handles reached the gate",
                Arc::strong_count(gate)
            );
            std::thread::yield_now();
        }
    }

    /// A delete that lands while a sync is part-way through its read-modify-
    /// write waits for it, so the sync cannot put the sidecar back after it:
    /// the next daemon start would read it as an abandoned session.
    #[test]
    fn a_sync_racing_the_delete_leaves_no_sidecar() {
        use crate::history::test_support::{record, scratch_dirs, write_meta_for};
        let dirs = scratch_dirs("sidecar-delete-race");
        write_meta_for(&dirs, "s1");
        let registry = SessionRegistry::new(dirs.clone());
        registry.insert(record("s1", SessionMode::Interactive));

        // Stand in for a sync that has read the sidecar and not yet written.
        let gate = registry.hold_sidecar_sync("s1");
        let held = lock(&gate);
        let read = crate::orphan::load_meta(&dirs, "s1").expect("load meta");
        let deleter = {
            let registry = Arc::clone(&registry);
            std::thread::spawn(move || registry.delete_sidecar("s1"))
        };
        // The map's handle, the test's and the delete's.
        wait_until_gate_is_shared(&gate, 3);
        assert!(
            meta_file(&dirs, "s1").exists(),
            "the delete waits for the write in flight"
        );
        crate::orphan::write_meta(&dirs, &read).expect("the in-flight write lands");
        drop(held);
        deleter.join().expect("the delete thread");
        assert!(!meta_file(&dirs, "s1").exists(), "the delete ran after it");
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// The gates map keeps no entry for a session the registry has let go:
    /// not after `remove`, not after a discarded pending insert, not for a
    /// write that finds its record gone, and not for a delete of an unknown
    /// id.
    #[test]
    fn sidecar_gates_do_not_outlive_their_sessions() {
        use crate::history::test_support::{record, scratch_dirs, write_meta_for};
        let dirs = scratch_dirs("sidecar-gate-cleanup");
        let registry = SessionRegistry::new(dirs.clone());

        write_meta_for(&dirs, "s1");
        registry.insert(record("s1", SessionMode::Interactive));
        registry.update("s1", |rec| rec.status = SessionStatus::AwaitingInput);
        assert!(registry.has_sidecar_gate("s1"), "a write made the gate");
        registry.remove("s1");
        assert!(!registry.has_sidecar_gate("s1"), "remove drops it");

        let pending = registry.insert_pending(record("s2", SessionMode::Interactive));
        registry.sync_sidecar("s2");
        assert!(registry.has_sidecar_gate("s2"));
        pending.discard();
        assert!(!registry.has_sidecar_gate("s2"), "a discard drops it");

        // A late writer that lost the race with `remove` makes a fresh gate.
        registry.sync_sidecar("s1");
        let meta = crate::orphan::load_meta(&dirs, "s1").expect("load meta");
        registry.write_sidecar(&meta);
        assert!(
            !registry.has_sidecar_gate("s1"),
            "a write that finds its record gone drops the gate it made"
        );

        registry.delete_sidecar("s1");
        assert!(
            !registry.has_sidecar_gate("s1"),
            "a delete of an unknown id makes no gate"
        );
        assert!(
            !meta_file(&dirs, "s1").exists(),
            "and still deletes the file"
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// A child that exits at once can end its session before the spawn
    /// writes the sidecar; that late write must not leave a sidecar behind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_spawn_write_after_the_session_ended_writes_nothing() {
        use crate::history::test_support::{
            fake_pty, insert_live, record, scratch_dirs, write_meta_for,
        };
        use crate::pty::PtyExit;
        use std::time::Duration;
        let dirs = scratch_dirs("sidecar-write-after-exit");
        write_meta_for(&dirs, "s1");
        let meta = crate::orphan::load_meta(&dirs, "s1").expect("load meta");
        let registry = SessionRegistry::new(dirs.clone());
        let (pty, exit_tx) = fake_pty();
        insert_live(
            &registry,
            &dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );

        let _ = exit_tx.send(PtyExit::Code(0));
        tokio::time::timeout(Duration::from_secs(5), async {
            while meta_file(&dirs, "s1").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the exit deletes the sidecar");

        registry.write_sidecar(&meta);
        assert!(
            !meta_file(&dirs, "s1").exists(),
            "an ended session's sidecar stays deleted"
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// Title, cwd and status updates racing on three threads must leave the
    /// sidecar matching the record: every write of the sidecar reads the
    /// record afresh inside the session's sidecar gate, so the last writer
    /// always writes the newest value of every field. A stress loop, not a
    /// forced interleaving: a regression fails it often, not every run.
    #[test]
    fn racing_title_cwd_and_status_updates_leave_sidecar_at_latest() {
        use crate::history::test_support::{record, scratch_dirs, write_meta_for};
        const ROUNDS: usize = 100;
        let dirs = scratch_dirs("sidecar-race");
        write_meta_for(&dirs, "s1");
        let registry = SessionRegistry::new(dirs.clone());
        registry.insert(record("s1", SessionMode::Interactive));

        let titles = {
            let registry = Arc::clone(&registry);
            std::thread::spawn(move || {
                for i in 0..ROUNDS {
                    registry.update("s1", |rec| rec.terminal_title = Some(format!("t{i}")));
                }
            })
        };
        let cwds = {
            let registry = Arc::clone(&registry);
            std::thread::spawn(move || {
                for i in 0..ROUNDS {
                    registry.update("s1", |rec| rec.current_cwd = Some(format!("c{i}")));
                }
            })
        };
        let statuses = {
            let registry = Arc::clone(&registry);
            std::thread::spawn(move || {
                for i in 0..ROUNDS {
                    registry.update("s1", |rec| {
                        rec.status = if i % 2 == 0 {
                            SessionStatus::Working
                        } else {
                            SessionStatus::AwaitingInput
                        };
                    });
                }
            })
        };
        for handle in [titles, cwds, statuses] {
            handle.join().expect("an updater thread");
        }

        let snap = registry.snapshots().pop().expect("the session");
        let meta = crate::orphan::load_meta(&dirs, "s1").expect("load meta");
        assert_eq!(meta.status, Some(SessionStatus::AwaitingInput));
        assert_eq!(meta.status_since, snap.status_since);
        let last = ROUNDS - 1;
        assert_eq!(meta.terminal_title, Some(format!("t{last}")));
        assert_eq!(meta.current_cwd, Some(format!("c{last}")));
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[tokio::test]
    async fn reattach_restores_awaiting_input_since() {
        use crate::history::test_support::{fake_pty, scratch_dirs};
        let dirs = scratch_dirs("reattach-awaiting-input");
        let since = DateTime::from_timestamp(1_700_000_000, 0).expect("a fixed stamp");
        write_sidecar_with_status(&dirs, "s1", "awaiting_input", since);
        let meta = crate::orphan::load_meta(&dirs, "s1").expect("load meta");
        let registry = SessionRegistry::new(dirs.clone());
        let (pty, _exit_tx) = fake_pty();
        registry.insert_reattached(&meta, pty).publish();

        let snapshots = registry.snapshots();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(
            snapshots[0].status,
            SessionStatus::AwaitingInput,
            "a session waiting for input comes back waiting"
        );
        assert_eq!(
            snapshots[0].status_since,
            Some(since),
            "with the stamp it had before the restart"
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[tokio::test]
    async fn reattach_does_not_restore_working() {
        use crate::history::test_support::{fake_pty, scratch_dirs};
        let dirs = scratch_dirs("reattach-working");
        let since = DateTime::from_timestamp(1_700_000_000, 0).expect("a fixed stamp");
        write_sidecar_with_status(&dirs, "s1", "working", since);
        let meta = crate::orphan::load_meta(&dirs, "s1").expect("load meta");
        let registry = SessionRegistry::new(dirs.clone());
        let (pty, _exit_tx) = fake_pty();
        registry.insert_reattached(&meta, pty).publish();

        let snapshots = registry.snapshots();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(
            snapshots[0].status,
            SessionStatus::Idle,
            "only a waiting or idle session is restored as it was"
        );
        let since = snapshots[0].status_since.expect("a fresh stamp");
        assert!(
            since > Utc::now() - chrono::Duration::minutes(1),
            "a status that cannot be resumed is stamped now: {since:?}"
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[derive(Debug)]
    struct NoopKiller;

    impl portable_pty::ChildKiller for NoopKiller {
        fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(NoopKiller)
        }
    }

    /// A live session whose output the test feeds, wired to
    /// `attach_lifecycle` the way a spawn is.
    struct FedSession {
        dirs: Dirs,
        _registry: Arc<SessionRegistry>,
        _pty: Arc<PtyHandle>,
        _exit_tx: oneshot::Sender<PtyExit>,
        output: PtyOutput,
        snap_tx: ScrollbackSnapshotReq,
    }

    fn fed_session(tag: &str) -> FedSession {
        let dirs = scratch_dirs(tag);
        let registry = SessionRegistry::new(dirs.clone());
        let (output, lifecycle_output) = PtyOutput::channel(256);
        let (input_tx, _) = mpsc::unbounded_channel();
        let (resize_tx, _) = mpsc::unbounded_channel();
        let (exit_tx, exit_rx) = oneshot::channel();
        let pty = Arc::new(PtyHandle::from_parts(crate::pty::PtyHandleParts {
            output: output.clone(),
            lifecycle_output,
            input_tx,
            resize_tx,
            exit_rx,
            killer: Box::new(NoopKiller),
            pid: None,
        }));
        let snap_tx = attach_lifecycle(&registry, "s".to_owned(), &pty, Some(dirs.clone()));
        FedSession {
            dirs,
            _registry: registry,
            _pty: pty,
            _exit_tx: exit_tx,
            output,
            snap_tx,
        }
    }

    /// Asks the lifecycle task for a snapshot, waiting at most 60 s.
    async fn take_snapshot(snap_tx: &ScrollbackSnapshotReq) -> Result<ScrollbackSnapshot, String> {
        let (tx, rx) = oneshot::channel();
        snap_tx
            .send(tx)
            .map_err(|_| "the lifecycle task is gone".to_owned())?;
        tokio::time::timeout(std::time::Duration::from_secs(60), rx)
            .await
            .map_err(|_| "no snapshot within 60 s".to_owned())?
            .map_err(|_| "the lifecycle task dropped the snapshot reply".to_owned())
    }

    /// A thread broadcasting `<0>`, `<1>`, … on the session's output, one
    /// chunk per millisecond, until `finish`.
    struct NumberedSender {
        stop: Arc<std::sync::atomic::AtomicBool>,
        handle: std::thread::JoinHandle<()>,
    }

    impl NumberedSender {
        fn start(output: &PtyOutput) -> Self {
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::time::{Duration, Instant};
            let stop = Arc::new(AtomicBool::new(false));
            let handle = {
                let stop = Arc::clone(&stop);
                let output = output.clone();
                std::thread::spawn(move || {
                    let mut seq: u64 = 0;
                    while !stop.load(Ordering::Relaxed) {
                        output.blocking_send(format!("<{seq}>").into_bytes());
                        seq += 1;
                        let started = Instant::now();
                        while started.elapsed() < Duration::from_micros(1000) {
                            std::hint::spin_loop();
                        }
                    }
                })
            };
            Self { stop, handle }
        }

        /// Stops the thread and reports whether it ended without panicking.
        fn finish(self) -> bool {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            self.handle.join().is_ok()
        }
    }

    /// The number in the last `<n>` of a numbered chunk or replay.
    fn last_seq(data: &[u8]) -> Option<u64> {
        let text = String::from_utf8_lossy(data);
        let start = text.rfind('<')?;
        let end = text[start..].find('>')? + start;
        text[start + 1..end].parse().ok()
    }

    /// A chunk broadcast while a snapshot is being taken reaches the client:
    /// it is in the reply's `data` or in its `live` receiver, never in
    /// neither. A sender thread broadcasts numbered chunks 1 ms apart, and
    /// every snapshot's first live chunk must follow its data's last.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_chunk_sent_while_a_snapshot_is_taken_reaches_the_client() {
        let session = fed_session("snapshot-race");
        let sender = NumberedSender::start(&session.output);
        let lost = async {
            let mut lost_total = 0_u64;
            for _ in 0..400 {
                let snap = take_snapshot(&session.snap_tx).await?;
                let mut live = snap.live;
                let first = tokio::time::timeout(std::time::Duration::from_secs(60), live.recv())
                    .await
                    .map_err(|_| "no live chunk within 60 s".to_owned())?
                    .ok_or("the live receiver closed")?;
                let first_seq = last_seq(&first).ok_or("a live chunk without a sequence number")?;
                let expected = last_seq(&snap.data).map_or(0, |n| n + 1);
                lost_total += first_seq.saturating_sub(expected);
            }
            Ok::<u64, String>(lost_total)
        }
        .await;
        let sender_ok = sender.finish();
        let _ = std::fs::remove_dir_all(&session.dirs.config);

        assert!(sender_ok, "the sender thread panicked");
        let lost_total = lost.expect("every snapshot and its first live chunk");
        assert_eq!(
            lost_total, 0,
            "{lost_total} chunks sent while snapshots were taken reached neither their data nor their live receiver"
        );
    }

    /// A client already attached misses no chunk while other clients take
    /// snapshots: its live receiver, kept from a first snapshot, holds a
    /// contiguous run of sequence numbers after 200 further snapshots.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_attached_client_misses_no_chunk_when_another_client_takes_a_snapshot() {
        let session = fed_session("attached-client");
        let sender = NumberedSender::start(&session.output);
        let attached = async {
            let attached = take_snapshot(&session.snap_tx).await?.live;
            for _ in 0..200 {
                drop(take_snapshot(&session.snap_tx).await?);
            }
            Ok::<_, String>(attached)
        }
        .await;
        let sender_ok = sender.finish();
        let _ = std::fs::remove_dir_all(&session.dirs.config);

        assert!(sender_ok, "the sender thread panicked");
        let mut attached = attached.expect("every snapshot");
        let mut seqs = Vec::new();
        while let Ok(chunk) = attached.try_recv() {
            seqs.push(last_seq(&chunk));
        }
        let gaps: Vec<(Option<u64>, Option<u64>)> = seqs
            .windows(2)
            .filter(|pair| pair[0].map(|n| n + 1) != pair[1])
            .map(|pair| (pair[0], pair[1]))
            .collect();
        assert!(
            seqs.len() > 1,
            "the attached client received {} chunks",
            seqs.len()
        );
        assert!(
            gaps.is_empty(),
            "{} gaps in the attached client's {} chunks, first: {:?}",
            gaps.len(),
            seqs.len(),
            gaps.first()
        );
    }

    /// An attached client gets every chunk the session sends while its
    /// lifecycle task is held, however many: the task is the only path from
    /// the PTY to clients, and nothing re-sends what it skips. The test runs
    /// on a current-thread runtime, so the lifecycle task runs only when the
    /// test awaits; sending twice the broadcast's capacity, where a send waits
    /// only on a full lifecycle queue, holds it the way a busy worker or a
    /// starved thread does in the daemon.
    #[tokio::test]
    async fn an_attached_client_gets_every_chunk_sent_while_the_lifecycle_task_is_held() {
        // Twice the 256 chunks `fed_session`'s broadcast holds.
        const SENT: u64 = 512;
        let session = fed_session("held-lifecycle");
        let mut attached = take_snapshot(&session.snap_tx)
            .await
            .expect("a first snapshot")
            .live;

        for seq in 0..SENT {
            session.output.send(format!("<{seq}>").into_bytes()).await;
        }

        let mut first_wrong = None;
        for expected in 0..SENT {
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(60), attached.recv())
                .await
                .ok()
                .flatten();
            let got = chunk.as_deref().and_then(last_seq);
            if got != Some(expected) {
                first_wrong = Some((expected, got));
                break;
            }
        }
        let _ = std::fs::remove_dir_all(&session.dirs.config);
        assert_eq!(
            first_wrong, None,
            "(expected chunk, chunk received) at the first chunk the attached client missed of {SENT} sent while its lifecycle task was held"
        );
    }

    /// A full lifecycle queue makes the reader wait instead of dropping a
    /// chunk: with the lifecycle task held, `LIFECYCLE_OUTPUT_CAPACITY` sends
    /// complete at once and the next one does not, then completes once the
    /// task runs, and the attached client gets every chunk in order. Each send
    /// is polled once by hand, outside tokio's cooperative budget, so the
    /// current-thread runtime never runs the lifecycle task in between.
    #[tokio::test]
    async fn a_full_lifecycle_queue_makes_the_reader_wait() {
        use crate::pty::LIFECYCLE_OUTPUT_CAPACITY;
        use std::future::Future;
        use std::task::{Context, Waker};
        let session = fed_session("full-lifecycle-queue");
        let mut attached = take_snapshot(&session.snap_tx)
            .await
            .expect("a first snapshot")
            .live;
        let capacity = u64::try_from(LIFECYCLE_OUTPUT_CAPACITY).expect("the capacity fits u64");
        let mut cx = Context::from_waker(Waker::noop());

        let mut held_sends = 0_u64;
        for seq in 0..capacity {
            let send = std::pin::pin!(tokio::task::unconstrained(
                session.output.send(format!("<{seq}>").into_bytes())
            ));
            if send.poll(&mut cx).is_pending() {
                break;
            }
            held_sends += 1;
        }
        let mut next = Box::pin(tokio::task::unconstrained(
            session.output.send(format!("<{capacity}>").into_bytes()),
        ));
        let next_waited = next.as_mut().poll(&mut cx).is_pending();
        let next_completed = !next_waited
            || tokio::time::timeout(std::time::Duration::from_secs(60), next)
                .await
                .is_ok();

        let mut first_wrong = None;
        for expected in 0..=capacity {
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(60), attached.recv())
                .await
                .ok()
                .flatten();
            let got = chunk.as_deref().and_then(last_seq);
            if got != Some(expected) {
                first_wrong = Some((expected, got));
                break;
            }
        }
        let _ = std::fs::remove_dir_all(&session.dirs.config);
        assert_eq!(
            held_sends, capacity,
            "sends that completed at once while the lifecycle task was held"
        );
        assert!(
            next_waited,
            "the send past a full lifecycle queue completed without waiting"
        );
        assert!(
            next_completed,
            "the waiting send did not complete within 60 s once the lifecycle task ran"
        );
        assert_eq!(
            first_wrong, None,
            "(expected chunk, chunk received) at the first chunk the attached client missed"
        );
    }

    /// A snapshot is answered while output arrives faster than the lifecycle
    /// task persists it: a client attaching to a busy session gets its
    /// scrollback and its live feed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_snapshot_is_answered_while_output_arrives_faster_than_it_is_persisted() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let session = fed_session("flooded");
        let stop = Arc::new(AtomicBool::new(false));
        let flood = {
            let stop = Arc::clone(&stop);
            let output = session.output.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    output.blocking_send(b"x".to_vec());
                    let sent = std::time::Instant::now();
                    while sent.elapsed() < std::time::Duration::from_micros(100) {
                        std::hint::spin_loop();
                    }
                }
            })
        };
        let flowing = async {
            let mut attached = take_snapshot(&session.snap_tx).await?.live;
            tokio::time::timeout(std::time::Duration::from_secs(60), attached.recv())
                .await
                .map_err(|_| "no live chunk within 60 s".to_owned())?
                .ok_or("the live receiver closed")?;
            Ok::<_, String>(attached)
        }
        .await;
        let snap = match flowing {
            Ok(_attached) => take_snapshot(&session.snap_tx).await,
            Err(err) => Err(format!("before the flood was flowing: {err}")),
        };
        stop.store(true, Ordering::Relaxed);
        let flood_ok = flood.join().is_ok();
        let _ = std::fs::remove_dir_all(&session.dirs.config);
        assert!(flood_ok, "the flood thread panicked");
        assert!(snap.is_ok(), "{:?}", snap.err());
    }

    /// A client's live receiver ends once the session's output ends: the
    /// lifecycle task stops on the closed output queue and drops the client's
    /// sender with it, so the per-client forwarder finishes.
    #[tokio::test]
    async fn a_client_live_receiver_closes_when_the_session_output_ends() {
        use crate::history::test_support::fake_pty;
        use std::time::Duration;
        let dirs = scratch_dirs("live-closes");
        let registry = SessionRegistry::new(dirs.clone());
        let (pty, _exit_tx) = fake_pty();
        let snap_tx = attach_lifecycle(&registry, "s".to_owned(), &pty, Some(dirs.clone()));
        let (tx, rx) = oneshot::channel();
        snap_tx.send(tx).expect("the lifecycle task is alive");
        let snap = tokio::time::timeout(Duration::from_secs(60), rx)
            .await
            .expect("a snapshot within 60 s")
            .expect("a snapshot reply");
        let mut live = snap.live;

        drop(pty);

        let end = tokio::time::timeout(Duration::from_secs(60), live.recv())
            .await
            .expect("the live receiver ends within 60 s");
        assert!(
            end.is_none(),
            "no output after the session's output ended: {end:?}"
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }
}
