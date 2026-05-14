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
use protocol::{Agent, AppearanceOverrides, SessionKind, SessionMember, SessionMode, SpawnConfig};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};
use tracing::warn;

/// On-disk schema version for `meta.json`. Bumped only when the shape changes
/// in a way that older daemons can't tolerate (renames, removals, semantic
/// changes). Adding a new `#[serde(default)]` field is NOT a bump.
///
/// v1: A.3 baseline (versioned sidecar, `last_prompt`, `recent_actions_tail`).
/// v2: C.3 — tracer-backed sessions carry `tracer_pid` and `tracer_pipe`. The
/// load path still accepts v1 sidecars (the new fields default to `None`),
/// but v2 daemons that see a v1 sidecar with a non-tracer pid will route the
/// session through the legacy direct-PTY fallback on reattach (i.e. mark
/// abandoned, since there's no direct-PTY path anymore post-C.3).
pub const CURRENT_SIDECAR_VERSION: u32 = 2;

/// Highest sidecar version the current daemon understands.
pub const MAX_KNOWN_SIDECAR_VERSION: u32 = 2;

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
    /// Effective label = `user_label.clone().unwrap_or(default_label)`.
    /// Kept on disk for legacy single-field readers (and as a paper trail
    /// of what the user actually saw at write time). Reattach prefers
    /// `default_label` + `user_label` when present; falls back to this
    /// field when reading sidecars written before the rename feature.
    pub label: String,
    /// Daemon-generated default label captured at spawn time. `None` only
    /// for sidecars written before the rename feature — reattach falls
    /// back to treating `label` as the default in that case.
    #[serde(default)]
    pub default_label: Option<String>,
    /// User-provided rename override. `None` when the user has never
    /// renamed (or has cleared) this session.
    #[serde(default)]
    pub user_label: Option<String>,
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
    /// Legacy user-chosen accent color (`#rrggbb`) for the session UI.
    /// Loaded into `appearance.accent_color` and omitted on future writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent_color: Option<String>,
    /// Session-level appearance overrides.
    #[serde(default)]
    pub appearance: AppearanceOverrides,
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
    /// OS pid of the `rt-tracer.exe` process supervising this session's PTY.
    /// `None` for sidecars written before C.3 (no tracer in the picture) and
    /// for any future spawn path that doesn't go through a tracer. Liveness
    /// checks prefer this over `pid` when set — when the tracer is alive the
    /// session is recoverable via pipe reattach.
    #[serde(default)]
    pub tracer_pid: Option<u32>,
    /// Named-pipe path the daemon reconnects to on startup. Stored alongside
    /// `tracer_pid` so the daemon doesn't have to recompute it (and so a
    /// future ABI shift in pipe naming doesn't break reattach for older
    /// sidecars). `None` whenever `tracer_pid` is `None`.
    #[serde(default)]
    pub tracer_pipe: Option<String>,
    /// Absolute path of the cached tracer binary the supervisor was spawned
    /// from (see [`crate::binary_cache`]). Persisted so the daemon's startup
    /// GC sweep can tell which cached entries are still in use by live
    /// tracers. `None` for sidecars written before the binary cache existed
    /// or for a future spawn path that bypasses the cache; the GC sweep
    /// treats absent entries as "unknown — keep" by including only
    /// known-in-use paths in its retain set.
    #[serde(default)]
    pub tracer_exe_path: Option<String>,
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

/// Returns true iff the recorded pid exists *and* its executable name matches
/// what we expected to spawn. The name check guards against PID reuse: if the
/// pid was recycled to an unrelated process, the meta is stale and should be
/// dropped.
///
/// Post-C.3, tracer-backed sessions carry `tracer_pid` + `tracer_pipe`; we
/// check the tracer for liveness in that case (the underlying agent process
/// lives behind the tracer's PTY and isn't a direct daemon child anymore).
/// Pre-C.3 sidecars use the legacy `pid` + `program_name` path.
pub fn is_session_alive(meta: &OrphanMeta) -> bool {
    if let Some(tracer_pid) = meta.tracer_pid {
        return pid_matches(tracer_pid, Some("rt-tracer"));
    }
    pid_matches(meta.pid, meta.program_name.as_deref())
}

fn pid_matches(pid: u32, program_token: Option<&str>) -> bool {
    let mut sys =
        System::new_with_specifics(RefreshKind::new().with_processes(ProcessRefreshKind::new()));
    let pid = sysinfo::Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::new(),
    );
    let Some(process) = sys.process(pid) else {
        return false;
    };
    let name = process.name().to_string_lossy().to_lowercase();
    match program_token {
        Some(token) => name.contains(&token.to_lowercase()),
        // Legacy fallback for the very oldest sidecars (pre plain-shell
        // support): `claude` (Unix) or `claude.exe` / `node.exe` shim on
        // Windows. The TUI is a Node script invoked via a shim; allow either.
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
    tracer_pid: Option<u32>,
    tracer_pipe: Option<String>,
    tracer_exe_path: Option<String>,
) -> anyhow::Result<OrphanMeta> {
    if pid == 0 {
        return Err(anyhow!("refusing to write orphan meta with pid=0"));
    }
    Ok(OrphanMeta {
        on_disk_version: CURRENT_SIDECAR_VERSION,
        session_id,
        pid,
        // At spawn time, the effective label IS the daemon-generated
        // default. Subsequent renames write through via `update_labels`
        // (which reads + mutates + writes back the sidecar).
        default_label: Some(label.clone()),
        user_label: None,
        label,
        kind,
        mode,
        members,
        started_at,
        workspace_id,
        program_name,
        agent: Some(agent),
        terminal_title: None,
        accent_color: None,
        appearance: AppearanceOverrides::default(),
        spawn_config,
        last_prompt,
        recent_actions_tail: Vec::new(),
        tracer_pid,
        tracer_pipe,
        tracer_exe_path,
    })
}

/// Best-effort sync of the rename fields into the sidecar. Mirrors the
/// `update_terminal_title` / `update_recent_actions_tail` pattern:
/// read the sidecar, mutate three fields, write back. Silently no-ops
/// for sessions whose sidecar doesn't exist yet (the next spawn-time
/// write will pick the latest values up).
pub fn update_labels(
    dirs: &Dirs,
    session_id: &str,
    default_label: &str,
    user_label: Option<&str>,
    effective_label: &str,
) -> anyhow::Result<()> {
    let path = meta_path(dirs, session_id);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).context("reading meta for rename update"),
    };
    let mut meta = load_meta_from_bytes(&bytes).context("loading meta for rename update")?;
    let already_synced = meta.default_label.as_deref() == Some(default_label)
        && meta.user_label.as_deref() == user_label
        && meta.label == effective_label;
    if already_synced {
        return Ok(());
    }
    meta.default_label = Some(default_label.to_string());
    meta.user_label = user_label.map(str::to_string);
    meta.label = effective_label.to_string();
    write_meta(dirs, &meta)
}

pub fn try_update_labels(
    dirs: &Dirs,
    session_id: &str,
    default_label: &str,
    user_label: Option<&str>,
    effective_label: &str,
) {
    if let Err(err) = update_labels(dirs, session_id, default_label, user_label, effective_label) {
        warn!(?err, %session_id, "failed to update orphan meta labels");
    }
}

/// Best-effort: read the meta sidecar, mutate `terminal_title`, write it back.
/// Used by the OSC-title parser when the agent emits an OSC 0/1/2 sequence.
/// Updating the sidecar preserves the terminal-title hint without changing the
/// canonical repo/workspace label. Returns `Ok(())` for both "meta did not
/// exist" and "meta updated" — only true I/O errors surface.
pub fn update_terminal_title(dirs: &Dirs, session_id: &str, new_title: &str) -> anyhow::Result<()> {
    let path = meta_path(dirs, session_id);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).context("reading meta for terminal_title update"),
    };
    let mut meta =
        load_meta_from_bytes(&bytes).context("loading meta for terminal_title update")?;
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

pub fn update_appearance(
    dirs: &Dirs,
    session_id: &str,
    appearance: &AppearanceOverrides,
) -> anyhow::Result<()> {
    let path = meta_path(dirs, session_id);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).context("reading meta for appearance update"),
    };
    let mut meta = load_meta_from_bytes(&bytes).context("loading meta for appearance update")?;
    if meta.appearance == *appearance && meta.accent_color.is_none() {
        return Ok(());
    }
    meta.accent_color = None;
    meta.appearance = appearance.clone();
    write_meta(dirs, &meta)
}

pub fn try_update_appearance(dirs: &Dirs, session_id: &str, appearance: &AppearanceOverrides) {
    if let Err(err) = update_appearance(dirs, session_id, appearance) {
        warn!(?err, %session_id, "failed to update orphan meta appearance");
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
            default_label: Some("test".to_string()),
            user_label: None,
            kind: SessionKind::Single,
            mode: SessionMode::Interactive,
            members: vec![],
            started_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            workspace_id: None,
            program_name: Some("rt-tracer".to_string()),
            agent: Some(Agent::Claude),
            terminal_title: None,
            accent_color: Some("#2f81f7".to_string()),
            appearance: AppearanceOverrides {
                accent_color: Some("#3fb950".to_string()),
                ..AppearanceOverrides::default()
            },
            spawn_config: None,
            last_prompt: None,
            recent_actions_tail: Vec::new(),
            tracer_pid: Some(1234),
            tracer_pipe: Some(r"\\.\pipe\rt-tracer-s1".to_string()),
            tracer_exe_path: Some(r"C:\cache\rt-tracer-aaaaaaaaaaaaaaaa.exe".to_string()),
        };
        let bytes = serde_json::to_vec(&original).expect("serialize");
        let decoded = load_meta_from_bytes(&bytes).expect("decode");
        assert_eq!(decoded.on_disk_version, CURRENT_SIDECAR_VERSION);
        assert_eq!(decoded.session_id, "s1");
        assert_eq!(decoded.tracer_pid, Some(1234));
        assert_eq!(
            decoded.tracer_pipe.as_deref(),
            Some(r"\\.\pipe\rt-tracer-s1")
        );
        assert_eq!(
            decoded.tracer_exe_path.as_deref(),
            Some(r"C:\cache\rt-tracer-aaaaaaaaaaaaaaaa.exe")
        );
        assert_eq!(decoded.accent_color.as_deref(), Some("#2f81f7"));
        assert_eq!(decoded.appearance.accent_color.as_deref(), Some("#3fb950"));
    }

    #[test]
    fn v2_sidecar_without_tracer_exe_path_loads_as_none() {
        // A sidecar written before the binary-cache feature carries
        // tracer_pid + tracer_pipe but no tracer_exe_path. Serde defaults
        // it to None; the GC sweep simply doesn't get a hint and treats
        // the cache entry as unknown (excluded from the retain set —
        // unsafe to delete only if it matches the live daemon's own
        // exe, which is also in the retain set).
        let v2 = br#"{
            "on_disk_version": 2,
            "session_id": "old-cache",
            "pid": 4321,
            "label": "legacy",
            "kind": "single",
            "mode": "interactive",
            "members": [],
            "started_at": "2024-01-01T00:00:00Z",
            "program_name": "rt-tracer",
            "tracer_pid": 4321,
            "tracer_pipe": "\\\\.\\pipe\\rt-tracer-old"
        }"#;
        let meta = load_meta_from_bytes(v2).expect("parse v2 without exe path");
        assert_eq!(meta.tracer_pid, Some(4321));
        assert_eq!(meta.tracer_exe_path, None);
    }

    #[test]
    fn v1_sidecar_without_tracer_fields_loads_as_none() {
        // A sidecar written by a v1 (pre-C.3) daemon doesn't carry tracer
        // fields; serde defaults them to `None`. Reattach treats this as
        // "no tracer to reconnect to" → route via legacy pid liveness.
        let v1 = br#"{
            "on_disk_version": 1,
            "session_id": "old",
            "pid": 9001,
            "label": "legacy",
            "kind": "single",
            "mode": "interactive",
            "members": [],
            "started_at": "2024-01-01T00:00:00Z",
            "program_name": "claude"
        }"#;
        let meta = load_meta_from_bytes(v1).expect("parse v1");
        assert_eq!(meta.tracer_pid, None);
        assert_eq!(meta.tracer_pipe, None);
        assert_eq!(meta.program_name.as_deref(), Some("claude"));
    }
}
