//! In-memory session registry: spawned `claude` processes plus their state.

use crate::headless::HeadlessHandle;
use crate::orphan::{self, OrphanMeta};
use crate::paths::Dirs;
use crate::pty::PtyHandle;
use crate::scrollback;
use crate::sync::{lock, read, write};
use chrono::{DateTime, Utc};
use protocol::{
    SessionKind, SessionMember, SessionMetrics, SessionMode, SessionSnapshot, SessionStatus,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::broadcast;
use tracing::warn;
use uuid::Uuid;

const RECENT_ACTIONS_CAP: usize = 32;
const EVENT_BROADCAST_CAPACITY: usize = 256;

#[derive(Debug, Clone)]
pub enum SessionEvent {
    Updated(SessionSnapshot),
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
    pub label: String,
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
        }
    }
}

pub struct SessionRegistry {
    by_id: RwLock<HashMap<String, Arc<Mutex<SessionRecord>>>>,
    events: broadcast::Sender<SessionEvent>,
}

impl SessionRegistry {
    pub fn new() -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        Arc::new(Self {
            by_id: RwLock::new(HashMap::new()),
            events,
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
        let _ = self.events.send(SessionEvent::Updated(snap));
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
        let snap = {
            let mut guard = lock(&arc);
            f(&mut guard);
            guard.snapshot()
        };
        let _ = self.events.send(SessionEvent::Updated(snap));
    }

    pub fn fan_out_pty(&self, session_id: &str, data: Vec<u8>) {
        let _ = self.events.send(SessionEvent::PtyOutput {
            session_id: session_id.to_string(),
            data,
        });
    }

    pub fn fan_out_attention(&self, session_id: String, reason: protocol::AttentionReason) {
        let _ = self.events.send(SessionEvent::Attention { session_id, reason });
    }
}

pub fn new_id() -> String {
    Uuid::new_v4().to_string()
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
                push_recent_action(rec, format!("exited with code {code}"));
            });
            registry_for_exit.fan_out_attention(
                session_for_exit.clone(),
                protocol::AttentionReason::Stopped,
            );
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
impl SessionRegistry {
    pub fn insert_orphan(&self, meta: &OrphanMeta) {
        let mut record = SessionRecord {
            id: meta.session_id.clone(),
            label: meta.label.clone(),
            kind: meta.kind.clone(),
            members: meta.members.clone(),
            mode: meta.mode,
            started_at: meta.started_at,
            status: SessionStatus::Idle,
            exit_code: None,
            metrics: SessionMetrics::default(),
            recent_actions: Vec::new(),
            pty: None,
            headless: None,
        };
        push_recent_action(&mut record, "reattached after daemon restart".to_string());
        self.insert(record);
    }
}
