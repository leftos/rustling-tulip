//! Orphan-session recovery sidecar.
//!
//! At spawn time the daemon writes `<sessions_dir>/<id>/meta.json` recording
//! the OS pid plus enough state to rebuild a [`SessionRecord`] without a live
//! PTY/headless handle. On startup the daemon walks the sessions dir, drops
//! stale entries whose pid no longer points at a `claude` process, and
//! reattaches the survivors so they appear in the dashboard with their
//! scrollback intact (PTY stream is gone, but state and history are not).
//!
//! Decoupled from Claude Code's own `~/.claude/projects/<encoded-cwd>/*.jsonl`
//! files so we don't depend on the undocumented log layout.

use crate::paths::Dirs;
use anyhow::{Context as _, anyhow};
use chrono::{DateTime, Utc};
use protocol::{Agent, SessionKind, SessionMember, SessionMode, SpawnConfig};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};
use tracing::warn;

/// On-disk schema version for `meta.json`. Bumped only when the shape changes
/// in a way that older daemons can't tolerate (renames, removals, semantic
/// changes). Adding a new `#[serde(default)]` field is NOT a bump.
///
/// Always written by post-A.3 daemons; sidecars written before this field
/// existed deserialize with the default-shim value (1).
pub const CURRENT_SIDECAR_VERSION: u32 = 1;

/// Highest sidecar version the current daemon understands. Today equal to
/// [`CURRENT_SIDECAR_VERSION`]; when a future daemon learns to load a v2
/// sidecar this bumps to 2.
pub const MAX_KNOWN_SIDECAR_VERSION: u32 = 1;

fn default_sidecar_version() -> u32 {
    CURRENT_SIDECAR_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrphanMeta {
    /// Schema version of this sidecar. Defaults to `CURRENT_SIDECAR_VERSION`
    /// when missing so pre-A.3 sidecars load unchanged.
    #[serde(default = "default_sidecar_version")]
    pub on_disk_version: u32,
    pub session_id: String,
    pub pid: u32,
    pub label: String,
    pub kind: SessionKind,
    pub mode: SessionMode,
    pub members: Vec<SessionMember>,
    pub started_at: DateTime<Utc>,
    /// Workspace id for `kind == Workspace`. `None` for single-repo sessions
    /// and for metas written by earlier daemon versions.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Process name substring to match against during orphan-recovery's
    /// liveness check (case-insensitive). `claude` sessions store the
    /// program-name token (e.g. `"claude"`); `plain_shell` sessions store the
    /// shell label (e.g. `"pwsh"`, `"cmd"`, `"bash"`). `None` for metas
    /// written by earlier daemon versions: the check falls back to the
    /// legacy `claude`/`node` heuristic.
    #[serde(default)]
    pub program_name: Option<String>,
    /// Which CLI spawned this session. `None` for metas written by daemon
    /// versions that pre-date codex support; reattach defaults to
    /// `Agent::Claude` in that case (the only agent that existed then).
    #[serde(default)]
    pub agent: Option<Agent>,
    /// Latest OSC-emitted window title. Stored alongside the canonical label
    /// so a daemon restart reattaches the orphan with the same annotation it
    /// had. `None` for metas written before this field existed; the OSC
    /// watcher will repopulate it on the next title broadcast.
    #[serde(default)]
    pub terminal_title: Option<String>,
    /// Spawn-time configuration used to clone this session via
    /// [`protocol::ClientMessage::DuplicateSession`]. `None` for sidecars
    /// written by daemon versions before this field existed; reattached
    /// orphans in that state can't be duplicated (the daemon rejects the
    /// request) and `GetSpawnConfig` surfaces `None` so the UI falls back
    /// to opening the spawn dialog with defaults.
    #[serde(default)]
    pub spawn_config: Option<SpawnConfig>,
    /// The user's initial prompt at spawn time. Captured so the
    /// "Resume abandoned" UX can replay it after a daemon crash. `None`
    /// for sessions spawned without a prompt (interactive, no prefill)
    /// and for sidecars written before this field existed.
    #[serde(default)]
    pub last_prompt: Option<String>,
    /// Most recent `recent_actions` from the live session, bounded to a
    /// small set that fits in ~1 KB. Synced on every registry update so
    /// the abandoned-bucket UI can show "what was this session doing
    /// right before the daemon crashed?". Empty for sidecars written
    /// before B.1.
    #[serde(default)]
    pub recent_actions_tail: Vec<String>,
}

fn meta_path(dirs: &Dirs, session_id: &str) -> PathBuf {
    dirs.sessions_dir.join(session_id).join("meta.json")
}

pub fn write_meta(dirs: &Dirs, meta: &OrphanMeta) -> anyhow::Result<()> {
    let dir = dirs.sessions_dir.join(&meta.session_id);
    std::fs::create_dir_all(&dir).context("creating session sidecar dir")?;
    let path = dir.join("meta.json");
    let bytes = serde_json::to_vec_pretty(meta).context("serializing orphan meta")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes).context("writing meta tmp")?;
    std::fs::rename(&tmp, &path).context("renaming meta")?;
    Ok(())
}

/// Read a single session's meta sidecar. Used by `stop_session` to recover
/// the pid for orphan kills. Returns `Err` if the file is missing or
/// malformed — callers should treat both as "no usable sidecar".
pub fn load_meta(dirs: &Dirs, session_id: &str) -> anyhow::Result<OrphanMeta> {
    let path = meta_path(dirs, session_id);
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    load_meta_from_bytes(&bytes).with_context(|| format!("loading {}", path.display()))
}

pub fn read_all_metas(dirs: &Dirs) -> anyhow::Result<Vec<OrphanMeta>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dirs.sessions_dir) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(err) => return Err(err).context("reading sessions dir"),
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                warn!(?err, "skipping unreadable session dir entry");
                continue;
            }
        };
        if !entry.file_type().is_ok_and(|ft| ft.is_dir()) {
            continue;
        }
        let path = entry.path().join("meta.json");
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                warn!(?err, path = %path.display(), "skipping unreadable meta.json");
                continue;
            }
        };
        match load_meta_from_bytes(&bytes) {
            Ok(meta) => out.push(meta),
            Err(err) => {
                warn!(?err, path = %path.display(), "skipping malformed meta.json");
            }
        }
    }
    Ok(out)
}

/// Decode a sidecar's bytes and route through [`migrate_sidecar`]. Rejects
/// sidecars written by a future daemon version so a downgrade scenario (v2
/// daemon writes the file, user rolls back to v1) doesn't corrupt live state.
fn load_meta_from_bytes(bytes: &[u8]) -> anyhow::Result<OrphanMeta> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).context("parsing meta.json as Value")?;
    migrate_sidecar(value)
}

/// Migration seam for sidecar shape changes. Today every accepted on-disk
/// version maps 1:1 to `OrphanMeta` (the only shape we know). When a future
/// version introduces an actual migration, branch on `on_disk_version` here.
fn migrate_sidecar(value: serde_json::Value) -> anyhow::Result<OrphanMeta> {
    let on_disk = value
        .get("on_disk_version")
        .and_then(serde_json::Value::as_u64)
        .map_or(CURRENT_SIDECAR_VERSION, |n| u32::try_from(n).unwrap_or(0));
    if on_disk > MAX_KNOWN_SIDECAR_VERSION {
        return Err(anyhow!(
            "sidecar from future daemon version: on_disk_version={on_disk}, max known={MAX_KNOWN_SIDECAR_VERSION}"
        ));
    }
    serde_json::from_value::<OrphanMeta>(value).context("deserializing migrated meta")
}

/// Remove the meta sidecar for a session that has stopped or errored. Leaves
/// the rest of the session dir (e.g. `scrollback.bin`) in place — the caller
/// decides whether to nuke the dir entirely.
pub fn delete_meta(dirs: &Dirs, session_id: &str) -> anyhow::Result<()> {
    let path = meta_path(dirs, session_id);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
    }
}

/// Remove the entire `<sessions_dir>/<id>/` directory. Use when stopping a
/// session for good (cleanup) so scrollback doesn't leak across recreations.
pub fn delete_session_dir(dirs: &Dirs, session_id: &str) -> anyhow::Result<()> {
    let path = dirs.sessions_dir.join(session_id);
    match std::fs::remove_dir_all(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
    }
}

/// Returns true iff `pid` exists *and* its executable name matches what we
/// expected to spawn. The name check guards against PID reuse: if the pid was
/// recycled to an unrelated process, the meta is stale and should be dropped.
///
/// When `meta.program_name` is set (post-upgrade metas) the match is
/// case-insensitive substring against that token. When it is `None` (legacy
/// metas written before plain-shell support landed) we fall back to the
/// original `claude`/`node` heuristic so existing sidecars keep working.
pub fn is_session_alive(meta: &OrphanMeta) -> bool {
    let mut sys =
        System::new_with_specifics(RefreshKind::new().with_processes(ProcessRefreshKind::new()));
    let pid = sysinfo::Pid::from_u32(meta.pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::new(),
    );
    let Some(process) = sys.process(pid) else {
        return false;
    };
    let name = process.name().to_string_lossy().to_lowercase();
    match meta.program_name.as_deref() {
        Some(token) => name.contains(&token.to_lowercase()),
        // Legacy fallback: `claude` (Unix) or `claude.exe` / `node.exe` shim
        // on Windows. The TUI is a Node script invoked via a shim; allow
        // either.
        None => name.contains("claude") || name.contains("node"),
    }
}

/// Send SIGKILL / `TerminateProcess` to the recorded pid, but only if it still
/// matches the recorded program name. The name-match check guards against
/// PID reuse (we don't want to kill an unrelated process that happens to
/// have the same id). Used by `stop_session` when the in-memory PTY/headless
/// handles are absent (i.e. orphan sessions reattached after daemon restart).
///
/// Returns `true` when the process existed and was signalled; `false` when
/// the pid is gone, the name no longer matches, or the kill call itself
/// failed (e.g. permission denied).
pub fn kill_pid(meta: &OrphanMeta) -> bool {
    if !is_session_alive(meta) {
        return false;
    }
    let mut sys =
        System::new_with_specifics(RefreshKind::new().with_processes(ProcessRefreshKind::new()));
    let pid = sysinfo::Pid::from_u32(meta.pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::new(),
    );
    let Some(process) = sys.process(pid) else {
        return false;
    };
    process.kill()
}

/// Filter a list of metas down to those whose pid is still a live process
/// matching the recorded program name. Returns `(live, dead)` so callers can
/// clean up dead entries.
pub fn partition_live(metas: Vec<OrphanMeta>) -> (Vec<OrphanMeta>, Vec<OrphanMeta>) {
    let mut live = Vec::new();
    let mut dead = Vec::new();
    for m in metas {
        if is_session_alive(&m) {
            live.push(m);
        } else {
            dead.push(m);
        }
    }
    (live, dead)
}

/// Best-effort wrapper that logs and swallows non-fatal sidecar errors. Used
/// in lifecycle paths where a missing or unwritable meta should never abort
/// session handling.
pub fn try_write_meta(dirs: &Dirs, meta: &OrphanMeta) {
    if let Err(err) = write_meta(dirs, meta) {
        warn!(?err, session_id = %meta.session_id, "failed to write orphan meta");
    }
}

pub fn try_delete_meta(dirs: &Dirs, session_id: &str) {
    if let Err(err) = delete_meta(dirs, session_id) {
        warn!(?err, %session_id, "failed to delete orphan meta");
    }
}

pub fn try_delete_session_dir(dirs: &Dirs, session_id: &str) {
    if let Err(err) = delete_session_dir(dirs, session_id) {
        warn!(?err, %session_id, "failed to delete session dir");
    }
}

/// Convenience constructor used by the spawn paths.
#[expect(
    clippy::too_many_arguments,
    reason = "Spawn paths construct the meta from scattered locals; bundling adds noise"
)]
pub fn meta_from_record(
    session_id: String,
    pid: u32,
    label: String,
    kind: SessionKind,
    mode: SessionMode,
    members: Vec<SessionMember>,
    started_at: DateTime<Utc>,
    workspace_id: Option<String>,
    program_name: Option<String>,
    agent: Agent,
    spawn_config: Option<SpawnConfig>,
    last_prompt: Option<String>,
) -> anyhow::Result<OrphanMeta> {
    if pid == 0 {
        return Err(anyhow!("refusing to write orphan meta with pid=0"));
    }
    Ok(OrphanMeta {
        on_disk_version: CURRENT_SIDECAR_VERSION,
        session_id,
        pid,
        label,
        kind,
        mode,
        members,
        started_at,
        workspace_id,
        program_name,
        agent: Some(agent),
        terminal_title: None,
        spawn_config,
        last_prompt,
        recent_actions_tail: Vec::new(),
    })
}

/// Best-effort: read the meta sidecar, mutate `terminal_title`, write it back.
/// Used by the OSC-title parser when the agent emits an OSC 0/2 sequence — we
/// want that annotation to survive a daemon restart so a reattached orphan
/// keeps its terminal-title hint. The canonical `label` is intentionally not
/// touched (it stays whatever the spawn pipeline chose). Returns `Ok(())` for
/// both "meta did not exist" and "meta updated" — only true I/O errors
/// surface.
pub fn update_terminal_title(dirs: &Dirs, session_id: &str, new_title: &str) -> anyhow::Result<()> {
    let path = meta_path(dirs, session_id);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).context("reading meta for terminal_title update"),
    };
    let mut meta = load_meta_from_bytes(&bytes).context("loading meta for terminal_title update")?;
    if meta.terminal_title.as_deref() == Some(new_title) {
        return Ok(());
    }
    meta.terminal_title = Some(new_title.to_string());
    write_meta(dirs, &meta)
}

pub fn try_update_terminal_title(dirs: &Dirs, session_id: &str, new_title: &str) {
    if let Err(err) = update_terminal_title(dirs, session_id, new_title) {
        warn!(?err, %session_id, "failed to update orphan meta terminal_title");
    }
}

/// Best-effort sync of `recent_actions_tail` into the sidecar. Called by
/// the registry after every `update()` so abandoned sessions retain the
/// most recent operational context. Silently no-ops if the sidecar
/// doesn't exist (e.g. for a brand-new session whose spawn pipeline
/// hasn't called `try_write_meta` yet, or a session that never had a
/// sidecar — plain shells in some configurations).
///
/// Returns `Ok(())` for both "meta did not exist" and "meta updated";
/// real I/O errors surface as `Err` and the caller logs them.
pub fn update_recent_actions_tail(
    dirs: &Dirs,
    session_id: &str,
    new_tail: &[String],
) -> anyhow::Result<()> {
    let path = meta_path(dirs, session_id);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).context("reading meta for recent_actions update"),
    };
    let mut meta =
        load_meta_from_bytes(&bytes).context("loading meta for recent_actions update")?;
    if meta.recent_actions_tail == new_tail {
        return Ok(());
    }
    meta.recent_actions_tail = new_tail.to_vec();
    write_meta(dirs, &meta)
}

pub fn try_update_recent_actions_tail(dirs: &Dirs, session_id: &str, new_tail: &[String]) {
    if let Err(err) = update_recent_actions_tail(dirs, session_id, new_tail) {
        warn!(?err, %session_id, "failed to update orphan meta recent_actions_tail");
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "tests assert preconditions with expect/unwrap; failure messages aid debugging"
)]
mod tests {
    use super::*;

    #[test]
    fn legacy_sidecar_without_version_loads_with_default_v1() {
        // Pre-A.3 sidecars don't carry `on_disk_version`. The serde default
        // shim must produce CURRENT_SIDECAR_VERSION so they keep loading.
        let legacy = br#"{
            "session_id": "s1",
            "pid": 1234,
            "label": "test",
            "kind": "single",
            "mode": "interactive",
            "members": [],
            "started_at": "2024-01-01T00:00:00Z"
        }"#;
        let meta = load_meta_from_bytes(legacy).expect("parse legacy");
        assert_eq!(meta.on_disk_version, CURRENT_SIDECAR_VERSION);
        assert_eq!(meta.session_id, "s1");
    }

    #[test]
    fn future_version_sidecar_is_rejected() {
        // A downgrade scenario: v(N+1) daemon wrote the file with a future
        // version, user rolled back to v(N). Refuse to load so we don't
        // misinterpret the shape.
        let future = br#"{
            "on_disk_version": 999,
            "session_id": "s1",
            "pid": 1234,
            "label": "test",
            "kind": "single",
            "mode": "interactive",
            "members": [],
            "started_at": "2024-01-01T00:00:00Z"
        }"#;
        let res = load_meta_from_bytes(future);
        assert!(res.is_err(), "expected future-version rejection");
        let err = format!("{:?}", res.err().unwrap());
        assert!(err.contains("future daemon version"), "got: {err}");
    }

    #[test]
    fn current_version_sidecar_round_trips() {
        let original = OrphanMeta {
            on_disk_version: CURRENT_SIDECAR_VERSION,
            session_id: "s1".to_string(),
            pid: 1234,
            label: "test".to_string(),
            kind: SessionKind::Single,
            mode: SessionMode::Interactive,
            members: vec![],
            started_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            workspace_id: None,
            program_name: Some("claude".to_string()),
            agent: Some(Agent::Claude),
            terminal_title: None,
            spawn_config: None,
            last_prompt: None,
            recent_actions_tail: Vec::new(),
        };
        let bytes = serde_json::to_vec(&original).expect("serialize");
        let decoded = load_meta_from_bytes(&bytes).expect("decode");
        assert_eq!(decoded.on_disk_version, CURRENT_SIDECAR_VERSION);
        assert_eq!(decoded.session_id, "s1");
    }
}
