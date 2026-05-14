//! In-memory session registry: spawned `claude` processes plus their state.

use crate::headless::HeadlessHandle;
use crate::orphan::{self, OrphanMeta};
use crate::paths::Dirs;
use crate::pty::PtyHandle;
use crate::scrollback;
use crate::sync::{lock, read, write};
use chrono::{DateTime, Utc};
use protocol::{
    Agent, AppearanceOverrides, SessionKind, SessionMember, SessionMetrics, SessionMode,
    SessionSnapshot, SessionStatus, SpawnConfig,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::{broadcast, mpsc};
use tracing::warn;
use uuid::Uuid;

const RECENT_ACTIONS_CAP: usize = 32;

/// On-disk cap for the persisted `recent_actions` tail. Smaller than the
/// in-memory cap because the sidecar is read on every daemon startup and
/// we don't want it ballooning. Trimmed to fit under [`RECENT_ACTIONS_TAIL_BYTE_CAP`].
const RECENT_ACTIONS_TAIL_ENTRIES: usize = 10;

/// Soft byte cap on the persisted `recent_actions` tail. Roughly 1 KB —
/// keeps sidecars compact. Entries are dropped from the front until the
/// remaining set fits.
const RECENT_ACTIONS_TAIL_BYTE_CAP: usize = 1024;
const EVENT_BROADCAST_CAPACITY: usize = 256;

#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Boxed because `SessionSnapshot` is wide; keeping it inline makes
    /// every other variant of this enum that big too.
    Updated(Box<SessionSnapshot>),
    Removed(String),
    PtyOutput {
        session_id: String,
        data: Vec<u8>,
    },
    Attention {
        session_id: String,
        reason: protocol::AttentionReason,
    },
}

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
    pub exit_code: Option<i32>,
    pub metrics: SessionMetrics,
    pub recent_actions: Vec<String>,
    pub pty: Option<Arc<PtyHandle>>,
    pub headless: Option<Arc<HeadlessHandle>>,
    /// Workspace this session belongs to (`Some` iff `kind == Workspace`).
    /// Used by clients to group sessions in the sidebar tree.
    pub workspace_id: Option<String>,
    /// Which CLI is driving this session. Surfaced to clients via the
    /// `SessionSnapshot.agent` field.
    pub agent: Agent,
    /// Latest window title emitted by the agent/shell via OSC 0/2. Kept
    /// separate from `label` so the canonical name (set at spawn time, or by
    /// the user) is not overwritten by transient shell titles like
    /// `C:\WINDOWS\system32\cmd.exe`. `None` until the first OSC title arrives.
    pub terminal_title: Option<String>,
    /// Short program name driving this session. For `Mode::PlainShell`
    /// this is the shell label (`"pwsh"`, `"cmd"`, `"bash"`, …); for
    /// `Mode::Interactive` / `Mode::Headless` it's the agent token
    /// (`"claude"`, `"codex"`). Mirrored on disk via
    /// [`crate::orphan::OrphanMeta::program_name`] so reattach restores
    /// the same chip after a daemon restart.
    pub program_name: Option<String>,
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
    /// previous daemon crashed mid-run and the `claude` process is gone.
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
    /// Notifier feeding user-input byte counts into `pty_state::watch` so
    /// the status watcher can distinguish echo from genuine model output.
    /// `Some` for interactive sessions whose watcher is running; `None`
    /// for headless, plain-shell, orphaned, or stopped sessions. Cleared
    /// by the exit watcher when the child dies.
    pub input_notifier: Option<mpsc::UnboundedSender<usize>>,
}

impl SessionRecord {
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
        SessionSnapshot {
            id: self.id.clone(),
            label: self.label.clone(),
            kind: self.kind.clone(),
            members: self.members.clone(),
            status: self.status,
            mode: self.mode,
            started_at: self.started_at,
            exit_code: self.exit_code,
            metrics: self.metrics.clone(),
            recent_actions: self.recent_actions.clone(),
            is_orphan,
            is_abandoned: self.is_abandoned,
            last_prompt: self.last_prompt.clone(),
            workspace_id: self.workspace_id.clone(),
            agent: self.agent,
            terminal_title: self.terminal_title.clone(),
            program_name: self.program_name.clone(),
            appearance: self.appearance.clone(),
            elevated_authority,
            has_per_session_worktree,
            is_inactive: self.is_inactive,
            worktree_paths: self.worktree_paths.clone(),
        }
    }
}

pub struct SessionRegistry {
    by_id: RwLock<HashMap<String, Arc<Mutex<SessionRecord>>>>,
    events: broadcast::Sender<SessionEvent>,
    /// `Dirs` is `Some` once `run()` initializes the registry. Without it
    /// the registry can't sync `recent_actions` back to disk — callers
    /// that construct registries in tests (none today) would pass `None`.
    dirs: Option<Dirs>,
}

impl SessionRegistry {
    pub fn new(dirs: Dirs) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        Arc::new(Self {
            by_id: RwLock::new(HashMap::new()),
            events,
            dirs: Some(dirs),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }

    pub fn snapshots(&self) -> Vec<SessionSnapshot> {
        let guard = read(&self.by_id);
        guard.values().map(|rec| lock(rec).snapshot()).collect()
    }

    pub fn get(&self, id: &str) -> Option<Arc<Mutex<SessionRecord>>> {
        let guard = read(&self.by_id);
        guard.get(id).cloned()
    }

    pub fn insert(&self, record: SessionRecord) {
        let id = record.id.clone();
        let arc = Arc::new(Mutex::new(record));
        {
            let mut guard = write(&self.by_id);
            guard.insert(id, arc.clone());
        }
        let snap = lock(&arc).snapshot();
        let _ = self.events.send(SessionEvent::Updated(Box::new(snap)));
    }

    pub fn remove(&self, id: &str) {
        let mut guard = write(&self.by_id);
        if guard.remove(id).is_some() {
            let _ = self.events.send(SessionEvent::Removed(id.to_string()));
        }
    }

    pub fn update<F>(&self, id: &str, f: F)
    where
        F: FnOnce(&mut SessionRecord),
    {
        let Some(arc) = self.get(id) else { return };
        let (snap, recent_tail) = {
            let mut guard = lock(&arc);
            f(&mut guard);
            let tail = trim_recent_tail(&guard.recent_actions);
            (guard.snapshot(), tail)
        };
        // Sync the persisted recent_actions tail so an abandoned session
        // can show "what was this doing right before the daemon died?".
        // Skipped if the sidecar doesn't exist (e.g. for brand-new sessions
        // where the spawn pipeline hasn't called try_write_meta yet, or
        // for sessions that never had a sidecar in the first place).
        if let Some(dirs) = &self.dirs {
            orphan::try_update_recent_actions_tail(dirs, id, &recent_tail);
        }
        let _ = self.events.send(SessionEvent::Updated(Box::new(snap)));
    }

    pub fn fan_out_pty(&self, session_id: &str, data: Vec<u8>) {
        let _ = self.events.send(SessionEvent::PtyOutput {
            session_id: session_id.to_string(),
            data,
        });
    }

    pub fn fan_out_attention(&self, session_id: String, reason: protocol::AttentionReason) {
        let _ = self
            .events
            .send(SessionEvent::Attention { session_id, reason });
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

pub fn push_recent_action(rec: &mut SessionRecord, action: String) {
    rec.recent_actions.push(action);
    if rec.recent_actions.len() > RECENT_ACTIONS_CAP {
        let drop_count = rec.recent_actions.len() - RECENT_ACTIONS_CAP;
        rec.recent_actions.drain(0..drop_count);
    }
    rec.metrics.last_activity_at = Some(Utc::now());
}

/// Wires up the lifecycle tasks for a freshly-spawned PTY session: copies
/// output to the registry's broadcast and updates state when the child exits.
/// `dirs` is `Some` for normal spawns so the orphan-meta sidecar gets cleaned
/// up on exit; pass `None` for reattached orphans where the sidecar is owned
/// by the original spawner.
pub fn attach_lifecycle(
    registry: &Arc<SessionRegistry>,
    session_id: String,
    pty: &Arc<PtyHandle>,
    dirs: Option<Dirs>,
) {
    // Output forwarder. Also persists each chunk to scrollback when dirs is
    // provided (i.e. for normal spawns, not reattached orphans which don't
    // own a stream to capture from).
    let mut output = pty.output.subscribe();
    let registry_for_output = Arc::clone(registry);
    let session_for_output = session_id.clone();
    let dirs_for_output = dirs.clone();
    tokio::spawn(async move {
        loop {
            match output.recv().await {
                Ok(bytes) => {
                    if let Some(d) = &dirs_for_output {
                        scrollback::append(d, &session_for_output, &bytes);
                    }
                    registry_for_output.fan_out_pty(&session_for_output, bytes);
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(session_id = %session_for_output, lagged = n, "pty output lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Exit watcher.
    if let Some(rx) = pty.take_exit() {
        let registry_for_exit = Arc::clone(registry);
        let session_for_exit = session_id;
        tokio::spawn(async move {
            let code = rx.await.unwrap_or(-1);
            registry_for_exit.update(&session_for_exit, |rec| {
                rec.status = SessionStatus::Stopped;
                rec.exit_code = Some(code);
                rec.pty = None;
                rec.input_notifier = None;
                push_recent_action(rec, format!("exited with code {code}"));
            });
            registry_for_exit
                .fan_out_attention(session_for_exit.clone(), protocol::AttentionReason::Stopped);
            if let Some(dirs) = dirs {
                orphan::try_delete_meta(&dirs, &session_for_exit);
            }
        });
    }
}

/// Build a [`SessionRecord`] from a sidecar [`OrphanMeta`] and surface it via
/// `insert` so all attached clients see the reattached session in their next
/// snapshot. The PTY/headless handles are `None` because we missed the spawn
/// moment — the underlying `claude` is still running but its stdio is detached
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
            exit_code: None,
            metrics: SessionMetrics::default(),
            recent_actions: meta.recent_actions_tail.clone(),
            pty: None,
            headless: None,
            workspace_id: meta.workspace_id.clone(),
            agent: meta.agent.unwrap_or_default(),
            terminal_title: meta.terminal_title.clone(),
            program_name: meta.program_name.clone(),
            appearance: appearance_from_meta(meta),
            spawn_config: meta.spawn_config.clone(),
            is_abandoned: false,
            is_inactive: false,
            worktree_paths: Vec::new(),
            last_prompt: meta.last_prompt.clone(),
            input_notifier: None,
        };
        push_recent_action(&mut record, "reattached after daemon restart".to_string());
        self.insert(record);
    }

    /// Reattach a sidecar whose tracer process is still alive. Unlike
    /// `insert_orphan` this session has a real [`PtyHandle`] wired up to the
    /// tracer's pipe — input, output, resize, and kill all work as if the
    /// daemon had spawned it directly. Surfaced with `is_abandoned = false`
    /// and the same overall shape as a fresh spawn.
    pub fn insert_reattached(&self, meta: &OrphanMeta, pty: Arc<PtyHandle>) {
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
            exit_code: None,
            metrics: SessionMetrics::default(),
            recent_actions: meta.recent_actions_tail.clone(),
            pty: Some(pty),
            headless: None,
            workspace_id: meta.workspace_id.clone(),
            agent: meta.agent.unwrap_or_default(),
            terminal_title: meta.terminal_title.clone(),
            program_name: meta.program_name.clone(),
            appearance: appearance_from_meta(meta),
            spawn_config: meta.spawn_config.clone(),
            is_abandoned: false,
            is_inactive: false,
            worktree_paths: Vec::new(),
            last_prompt: meta.last_prompt.clone(),
            input_notifier: None,
        };
        push_recent_action(
            &mut record,
            "reattached to live tracer after daemon restart".to_string(),
        );
        self.insert(record);
    }

    /// Reattach a sidecar whose recorded pid is no longer alive: the previous
    /// daemon crashed mid-session. Surfaced to clients with
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
            exit_code: None,
            metrics: SessionMetrics::default(),
            recent_actions: meta.recent_actions_tail.clone(),
            pty: None,
            headless: None,
            workspace_id: meta.workspace_id.clone(),
            agent: meta.agent.unwrap_or_default(),
            terminal_title: meta.terminal_title.clone(),
            program_name: meta.program_name.clone(),
            appearance: appearance_from_meta(meta),
            spawn_config: meta.spawn_config.clone(),
            is_abandoned: true,
            is_inactive: false,
            worktree_paths: Vec::new(),
            last_prompt: meta.last_prompt.clone(),
            input_notifier: None,
        };
        push_recent_action(
            &mut record,
            "abandoned: daemon crashed before this session finished".to_string(),
        );
        self.insert(record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(tail.is_empty());
    }
}
