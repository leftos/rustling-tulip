//! Ended-session history, kept so a session can be recovered after it ends.
//!
//! Every end path (the child exiting, the user stopping or discarding a
//! session, a drained shutdown) writes one entry to
//! `<config>/history/<session-id>.json` before the session's `meta.json`
//! sidecar is deleted. The first write for a session wins, so a Stop followed
//! by the exit watcher keeps `StoppedByUser` rather than `Exited`. Headless
//! sessions are never written: a `claude --print` run has nothing to resume.
//! Startup prunes entries older than [`HISTORY_RETENTION`].

use crate::orphan;
use crate::paths::Dirs;
use crate::pty::PtyExit;
use crate::session::{SessionRecord, SessionRegistry};
use crate::sync::lock;
use anyhow::{Context as _, anyhow};
use chrono::{DateTime, TimeDelta, Utc};
use protocol::{HistoryEntry, HistorySource, SessionEnd, SessionMode, SpawnTarget};
use std::path::PathBuf;
use std::sync::Mutex;
use tracing::{info, warn};

/// How long an ended session stays in the history.
pub const HISTORY_RETENTION: TimeDelta = TimeDelta::days(7);

/// Serializes history writes inside this daemon, so the exists-check in
/// [`write_if_absent`] and the read-modify-write in [`mark_recovered`] are not
/// interleaved by two end paths racing on one session.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

fn entry_path(dirs: &Dirs, session_id: &str) -> PathBuf {
    dirs.history_dir().join(format!("{session_id}.json"))
}

/// The end reason for a PTY exit the exit watcher saw.
#[must_use]
pub fn end_for_exit(exit: PtyExit) -> SessionEnd {
    match exit {
        PtyExit::Code(code) => SessionEnd::Exited { code },
        PtyExit::TracerLost => SessionEnd::TracerLost,
    }
}

/// The folder a session started in: its first member's worktree, or a
/// standalone shell's requested directory.
fn primary_cwd(rec: &SessionRecord) -> Option<String> {
    if let Some(first) = rec.members.first() {
        return Some(first.worktree_path.clone());
    }
    match rec.spawn_config.as_ref().map(|cfg| &cfg.target) {
        Some(SpawnTarget::Standalone { cwd }) => cwd
            .as_deref()
            .map(str::trim)
            .filter(|cwd| !cwd.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

/// Build the history entry for `rec`, ended for `end` at `ended_at`.
#[must_use]
pub fn entry_from_record(
    rec: &SessionRecord,
    end: SessionEnd,
    ended_at: DateTime<Utc>,
) -> HistoryEntry {
    HistoryEntry {
        session_id: rec.id.clone(),
        label: rec.label.clone(),
        kind: rec.kind.clone(),
        mode: rec.mode,
        agent: rec.agent,
        spawn_config: rec.spawn_config.clone(),
        members: rec.members.clone(),
        workspace_id: rec.workspace_id.clone(),
        primary_cwd: primary_cwd(rec),
        current_cwd: rec.current_cwd.clone(),
        program_name: rec.program_name.clone(),
        started_at: Some(rec.started_at),
        ended_at,
        end,
        claude_session_id: rec.claude_session_id.clone(),
        source: HistorySource::Record,
        recovered_at: None,
    }
}

/// Write `entry` unless the session already has one. Returns `Ok(true)` when
/// this call wrote it and `Ok(false)` when an earlier end path got there first.
pub fn write_if_absent(dirs: &Dirs, entry: &HistoryEntry) -> anyhow::Result<bool> {
    let _guard = lock(&WRITE_LOCK);
    let path = entry_path(dirs, &entry.session_id);
    if path.exists() {
        return Ok(false);
    }
    write_entry(dirs, entry)?;
    Ok(true)
}

fn write_entry(dirs: &Dirs, entry: &HistoryEntry) -> anyhow::Result<()> {
    let dir = dirs.history_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let bytes = serde_json::to_vec_pretty(entry).context("serializing history entry")?;
    orphan::write_atomic(&entry_path(dirs, &entry.session_id), &bytes)
}

/// Record that the session `session_id` ended for `end`, unless it is headless
/// or already recorded. Failures are logged and never block the end path.
pub fn record_session_end(
    registry: &SessionRegistry,
    dirs: &Dirs,
    session_id: &str,
    end: SessionEnd,
) {
    let Some(rec) = registry.get(session_id) else {
        return;
    };
    let entry = {
        let guard = lock(&rec);
        if guard.mode == SessionMode::Headless {
            return;
        }
        entry_from_record(&guard, end, Utc::now())
    };
    if let Err(err) = write_if_absent(dirs, &entry) {
        warn!(?err, %session_id, ?end, "failed to write session history entry");
    }
}

fn read_entry(path: &std::path::Path) -> anyhow::Result<HistoryEntry> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

/// Every `.json` file in the history dir with the entry it holds. Unreadable
/// and unparseable files are skipped with a warning.
fn scan(dirs: &Dirs) -> Vec<(PathBuf, HistoryEntry)> {
    let dir = dirs.history_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            warn!(?err, dir = %dir.display(), "failed to read session history dir");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for dir_entry in entries {
        let path = match dir_entry {
            Ok(dir_entry) => dir_entry.path(),
            Err(err) => {
                warn!(?err, "skipping unreadable session history entry");
                continue;
            }
        };
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        match read_entry(&path) {
            Ok(entry) => out.push((path, entry)),
            Err(err) => warn!(?err, "skipping unreadable session history file"),
        }
    }
    out
}

/// Every history entry, newest end first.
#[must_use]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the history has no reader outside tests yet")
)]
pub fn read_all(dirs: &Dirs) -> Vec<HistoryEntry> {
    let mut entries: Vec<HistoryEntry> = scan(dirs).into_iter().map(|(_, e)| e).collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.ended_at));
    entries
}

/// Stamp the entry for `session_id` as recovered at `at`.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "no session is recovered outside tests yet")
)]
pub fn mark_recovered(dirs: &Dirs, session_id: &str, at: DateTime<Utc>) -> anyhow::Result<()> {
    let _guard = lock(&WRITE_LOCK);
    let path = entry_path(dirs, session_id);
    if !path.exists() {
        return Err(anyhow!("no history entry for session {session_id}"));
    }
    let mut entry = read_entry(&path)?;
    entry.recovered_at = Some(at);
    write_entry(dirs, &entry)
}

/// Delete entries that ended more than `max_age` before `now`. Returns how
/// many were deleted. Unparseable files are left alone.
pub fn prune(dirs: &Dirs, now: DateTime<Utc>, max_age: TimeDelta) -> usize {
    let _guard = lock(&WRITE_LOCK);
    let mut removed = 0;
    for (path, entry) in scan(dirs) {
        if now - entry.ended_at <= max_age {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(err) => {
                warn!(?err, path = %path.display(), "failed to prune session history entry");
            }
        }
    }
    if removed > 0 {
        info!(removed, "pruned old session history entries");
    }
    removed
}

/// Fixtures shared by the history tests here and the end-path tests in
/// `session`, `tracer_client` and `server`.
#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "test fixtures fail loudly on setup errors and timeouts"
)]
pub(crate) mod test_support {
    use crate::paths::Dirs;
    use crate::pty::{PtyExit, PtyHandle, PtyHandleParts};
    use crate::session::{SessionRecord, SessionRegistry, attach_lifecycle};
    use chrono::Utc;
    use portable_pty::ChildKiller;
    use protocol::{
        Agent, AppearanceOverrides, HistoryEntry, SessionKind, SessionMetrics, SessionMode,
        SessionStatus,
    };
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{broadcast, mpsc, oneshot};

    #[derive(Debug)]
    struct NoopKiller;

    impl ChildKiller for NoopKiller {
        fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(NoopKiller)
        }
    }

    /// A PTY handle with no process behind it: kills do nothing, and the
    /// returned sender plays the tracer reporting how the session ended.
    pub fn fake_pty() -> (Arc<PtyHandle>, oneshot::Sender<PtyExit>) {
        let (output, _) = broadcast::channel(16);
        let (input_tx, _) = mpsc::unbounded_channel();
        let (resize_tx, _) = mpsc::unbounded_channel();
        let (exit_tx, exit_rx) = oneshot::channel();
        let handle = PtyHandle::from_parts(PtyHandleParts {
            output,
            input_tx,
            resize_tx,
            exit_rx,
            killer: Box::new(NoopKiller),
            pid: None,
        });
        (Arc::new(handle), exit_tx)
    }

    /// Insert `rec` wired to `pty` the way a spawn does, exit watcher included.
    pub fn insert_live(
        registry: &Arc<SessionRegistry>,
        dirs: &Dirs,
        mut rec: SessionRecord,
        pty: &Arc<PtyHandle>,
    ) {
        let id = rec.id.clone();
        rec.pty = Some(Arc::clone(pty));
        let pending = registry.insert_pending(rec);
        let snap_tx = attach_lifecycle(registry, id, pty, Some(dirs.clone()));
        crate::sync::lock(pending.arc()).scrollback_snapshot_req = Some(snap_tx);
        pending.publish();
    }

    /// A fresh scratch config dir for `tag`.
    pub fn scratch_dirs(tag: &str) -> Dirs {
        let root = std::env::temp_dir().join(format!("rt-history-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sessions")).expect("create scratch dir");
        Dirs {
            config: root.clone(),
            state_file: root.join("state.json"),
            handshake_file: root.join("daemon.json"),
            lan_config_file: root.join("lan.json"),
            lan_cert_file: root.join("lan-cert.pem"),
            lan_key_file: root.join("lan-key.pem"),
            sessions_dir: root.join("sessions"),
            worktrees_dir: root.join("worktrees"),
            binaries_dir: root.join("binaries"),
        }
    }

    /// A standalone record `id` in `mode` with no process behind it.
    pub fn record(id: &str, mode: SessionMode) -> SessionRecord {
        SessionRecord {
            id: id.to_string(),
            label: id.to_string(),
            default_label: id.to_string(),
            user_label: None,
            kind: SessionKind::Standalone,
            members: Vec::new(),
            mode,
            started_at: Utc::now(),
            status: SessionStatus::Idle,
            exit_code: None,
            metrics: SessionMetrics::default(),
            recent_actions: Vec::new(),
            pty: None,
            headless: None,
            workspace_id: None,
            agent: Agent::default(),
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
            scrollback_snapshot_req: None,
            spawn_origin: None,
            claude_session_id: None,
        }
    }

    /// Wait up to five seconds for `session_id`'s history entry to appear.
    pub async fn wait_for_entry(dirs: &Dirs, session_id: &str) -> HistoryEntry {
        for _ in 0..500 {
            if let Some(entry) = super::read_all(dirs)
                .into_iter()
                .find(|e| e.session_id == session_id)
            {
                return entry;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no history entry for {session_id} within five seconds");
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod tests {
    use super::test_support::{record, scratch_dirs};
    use super::*;
    use crate::session::SessionRegistry;

    fn entry(id: &str, end: SessionEnd, ended_at: DateTime<Utc>) -> HistoryEntry {
        entry_from_record(&record(id, SessionMode::Interactive), end, ended_at)
    }

    #[test]
    fn write_if_absent_keeps_first_write() {
        let dirs = scratch_dirs("first-write");
        let now = Utc::now();
        let first = entry("s1", SessionEnd::StoppedByUser, now);
        let second = entry("s1", SessionEnd::Exited { code: 0 }, now);
        assert!(write_if_absent(&dirs, &first).expect("first write"));
        assert!(!write_if_absent(&dirs, &second).expect("second write"));
        let all = read_all(&dirs);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].end, SessionEnd::StoppedByUser);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn read_all_sorts_newest_first_and_skips_garbage() {
        let dirs = scratch_dirs("read-all");
        let now = Utc::now();
        for (id, age_mins) in [("old", 30), ("newest", 1), ("middle", 10)] {
            let e = entry(
                id,
                SessionEnd::TracerLost,
                now - TimeDelta::minutes(age_mins),
            );
            write_if_absent(&dirs, &e).expect("write");
        }
        std::fs::write(dirs.history_dir().join("garbage.json"), b"{ not json")
            .expect("write garbage");
        std::fs::write(dirs.history_dir().join("stray.json.tmp"), b"{}").expect("write stray");
        let ids: Vec<String> = read_all(&dirs).into_iter().map(|e| e.session_id).collect();
        assert_eq!(ids, vec!["newest", "middle", "old"]);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn prune_removes_only_old_entries() {
        let dirs = scratch_dirs("prune");
        let now = Utc::now();
        let old = entry("old", SessionEnd::TracerLost, now - TimeDelta::days(8));
        let recent = entry("recent", SessionEnd::TracerLost, now - TimeDelta::days(6));
        write_if_absent(&dirs, &old).expect("write old");
        write_if_absent(&dirs, &recent).expect("write recent");
        std::fs::write(dirs.history_dir().join("garbage.json"), b"nope").expect("write garbage");
        assert_eq!(prune(&dirs, now, HISTORY_RETENTION), 1);
        let ids: Vec<String> = read_all(&dirs).into_iter().map(|e| e.session_id).collect();
        assert_eq!(ids, vec!["recent"]);
        assert!(
            dirs.history_dir().join("garbage.json").exists(),
            "an unparseable file is left alone"
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn mark_recovered_sets_timestamp() {
        let dirs = scratch_dirs("recovered");
        let now = Utc::now();
        write_if_absent(&dirs, &entry("s1", SessionEnd::TracerLost, now)).expect("write");
        let at = now + TimeDelta::minutes(5);
        mark_recovered(&dirs, "s1", at).expect("mark recovered");
        let all = read_all(&dirs);
        assert_eq!(all[0].recovered_at, Some(at));
        assert_eq!(all[0].end, SessionEnd::TracerLost);
        assert!(mark_recovered(&dirs, "missing", at).is_err());
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn headless_record_is_not_written() {
        let dirs = scratch_dirs("headless");
        let registry = SessionRegistry::new(dirs.clone());
        registry.insert(record("headless", SessionMode::Headless));
        registry.insert(record("shell", SessionMode::PlainShell));
        record_session_end(&registry, &dirs, "headless", SessionEnd::StoppedByUser);
        record_session_end(&registry, &dirs, "shell", SessionEnd::StoppedByUser);
        let ids: Vec<String> = read_all(&dirs).into_iter().map(|e| e.session_id).collect();
        assert_eq!(ids, vec!["shell"]);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }
}
