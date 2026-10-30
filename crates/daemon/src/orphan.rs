//! Orphan-session recovery sidecar.
//!
//! At spawn time the daemon writes `<sessions_dir>/<id>/meta.json` recording
//! the OS pid plus enough state to rebuild a [`SessionRecord`] without a live
//! PTY/headless handle. On startup the daemon walks the sessions dir, drops
//! stale entries whose pid no longer points at the session's program, and
//! reattaches the survivors so they appear in the dashboard with their
//! scrollback intact (PTY stream is gone, but state and history are not).
//!
//! Decoupled from the agent CLI's own transcript files so we don't depend on
//! their undocumented layout.

use crate::env_secrets::{self, SealOutcome};
use crate::paths::Dirs;
use anyhow::{Context as _, anyhow};
use chrono::{DateTime, Utc};
use protocol::{
    AgentId, AppearanceOverrides, PendingInput, SessionKind, SessionMember, SessionMode,
    SessionStatus, SpawnConfig,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};
use tracing::{debug, warn};

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
    /// The program the user runs, the label the leaf shows: the agent token
    /// or the shell label (e.g. `"pwsh"`, `"cmd"`,
    /// `"bash"`). A tracer-backed sidecar's liveness goes by `tracer_pid`;
    /// only a sidecar without tracer fields matches its `pid` against this
    /// name (case-insensitive substring), falling back to the
    /// liveness tokens of the session's agent (`agent`, else the default
    /// agent) when it is `None`.
    #[serde(default)]
    pub program_name: Option<String>,
    /// Which agent this session runs; `None` for a plain shell. Reattach
    /// uses the default agent for a shell's record.
    #[serde(default)]
    pub agent: Option<AgentId>,
    /// Latest OSC-emitted window title. Stored alongside the canonical label
    /// so a daemon restart reattaches the orphan with the same annotation it
    /// had. `None` for metas written before this field existed; the OSC
    /// watcher will repopulate it on the next title broadcast.
    #[serde(default)]
    pub terminal_title: Option<String>,
    /// Latest cwd reported by the session. Used to restore plain-shell
    /// sidebar grouping after a daemon restart. `None` for older sidecars or
    /// sessions that have not emitted a cwd yet.
    #[serde(default)]
    pub current_cwd: Option<String>,
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
    /// right before a daemon crash, a reboot or a killed process tree ended
    /// it?". Empty for sidecars written
    /// before B.1.
    #[serde(default)]
    pub recent_actions_tail: Vec<String>,
    /// The session's status when the sidecar was last written. A reattach
    /// restores a session that was waiting for input (or idle) as it was, and
    /// treats every other stored status as one it has no evidence for. `None`
    /// for sidecars written before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SessionStatus>,
    /// When `status` last changed, so a reattached session shows how long it
    /// has been waiting instead of starting that clock over. `None` whenever
    /// `status` is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_since: Option<DateTime<Utc>>,
    /// What the session was waiting for when the sidecar was last written, as
    /// its hooks reported it. A reattach restores it only beside a restored
    /// `AwaitingInput`. Absent when nothing was pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_input: Option<PendingInput>,
    /// OS pid of the `rt-tracer.exe` process supervising this session's PTY.
    /// `None` for sidecars written before C.3 (no tracer in the picture) and
    /// for any future spawn path that doesn't go through a tracer. Liveness
    /// checks prefer this over `pid` when set — when the tracer is alive the
    /// session is recoverable via pipe reattach.
    #[serde(default)]
    pub tracer_pid: Option<u32>,
    /// The tracer process's start time in epoch seconds, as
    /// [`sysinfo::Process::start_time`] reports it, recorded so a recycled
    /// `tracer_pid` naming an unrelated `rt-tracer` does not pass for this
    /// session's tracer. `None` when the sidecar predates this field or the
    /// start time could not be read; either falls back to the name check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracer_started_at: Option<u64>,
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
    /// The agent conversation the session runs, as its plugin planned or
    /// discovered it. `None` for shells, headless runs and sessions whose
    /// conversation is not known yet.
    #[serde(default)]
    pub conversation_id: Option<String>,
    /// True when the user asked that this session not count as busy, so a
    /// reattached session keeps it. False for sidecars written before this
    /// field existed.
    #[serde(default)]
    pub busy_excluded: bool,
    /// True when the session's PTY child is the shell its agent was typed
    /// into, so a reattach runs the shell's status watcher. False for
    /// sidecars written before this field existed.
    #[serde(default)]
    pub agent_in_shell: bool,
    /// The label of the shell such a session runs, which the leaf shows once
    /// its agent has ended. `None` for every other session.
    #[serde(default)]
    pub shell_label: Option<String>,
    /// The agent whose plugin maps a recovered shell's hook events (see
    /// [`crate::session::SessionRecord::hook_agent`]), set at spawn. `None`
    /// for every other session and for sidecars written before this field
    /// existed: such a shell maps no hook event.
    #[serde(default)]
    pub hook_agent: Option<AgentId>,
    /// The recovered selection's options that `hook_agent`'s plugin maps a
    /// recovered shell's hook events with, set at spawn. Empty for every
    /// other session and for sidecars written before this field existed.
    #[serde(default)]
    pub hook_options: protocol::OptionValues,
    /// Whether the run a recovered shell resumes in `hook_agent` was trusted,
    /// which its hook events are mapped with, set at spawn. False for every
    /// other session and for sidecars written before this field existed.
    #[serde(default)]
    pub hook_trusted: bool,
}

fn meta_path(dirs: &Dirs, session_id: &str) -> PathBuf {
    dirs.sessions_dir.join(session_id).join("meta.json")
}

pub fn write_meta(dirs: &Dirs, meta: &OrphanMeta) -> anyhow::Result<()> {
    let dir = dirs.sessions_dir.join(&meta.session_id);
    std::fs::create_dir_all(&dir).context("creating session sidecar dir")?;
    let path = dir.join("meta.json");
    let bytes = serde_json::to_vec_pretty(meta).context("serializing orphan meta")?;
    crate::atomic_file::write_atomic(&path, &bytes).context("writing meta")
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
    Ok(scan_sidecars(dirs)?.0)
}

/// Every sidecar the sessions dir holds, plus the paths the scan could not turn
/// into a meta — already warned about by the scan itself.
fn scan_sidecars(dirs: &Dirs) -> anyhow::Result<(Vec<OrphanMeta>, Vec<PathBuf>)> {
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    let entries = match std::fs::read_dir(&dirs.sessions_dir) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok((out, skipped)),
        Err(err) => return Err(err).context("reading sessions dir"),
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                warn!(?err, "skipping unreadable session dir entry");
                skipped.push(dirs.sessions_dir.clone());
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
                skipped.push(path);
                continue;
            }
        };
        // Two failure classes, handled differently:
        //   - Bytes that aren't JSON at all (torn write, e.g. power loss or
        //     standby mid-rewrite): permanently unreadable. Quarantine to
        //     `meta.json.corrupt` so the bytes survive for post-mortem but
        //     the scan stops re-warning about the same file on every boot.
        //   - Valid JSON that fails migration (notably a future-version
        //     sidecar after a downgrade): left in place on purpose — the
        //     next upgrade can still recover that session.
        match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Err(err) => {
                warn!(?err, path = %path.display(), "quarantining corrupt meta.json (not valid JSON)");
                quarantine_meta(&path);
                skipped.push(path);
            }
            Ok(value) => match migrate_sidecar(value) {
                Ok(meta) => out.push(meta),
                Err(err) => {
                    warn!(?err, path = %path.display(), "skipping malformed meta.json");
                    skipped.push(path);
                }
            },
        }
    }
    Ok((out, skipped))
}

/// Every sidecar the sessions dir holds, or the path the scan could not read —
/// the folder itself, or a sidecar it could not turn into a meta (a corrupt
/// one, or one from a future daemon version, which is kept on purpose for a
/// later upgrade). A startup sweep that must not miss a secret reference
/// declines to act rather than read a skipped file as "nothing".
///
/// # Errors
///
/// The path of the folder or sidecar that could not be read.
pub fn read_all_metas_for_references(dirs: &Dirs) -> Result<Vec<OrphanMeta>, PathBuf> {
    let (metas, skipped) = scan_sidecars(dirs).map_err(|_| dirs.sessions_dir.clone())?;
    match skipped.into_iter().next() {
        Some(path) => Err(path),
        None => Ok(metas),
    }
}

/// Seal every secret literal row in every session sidecar's spawn config,
/// rewriting each sidecar that changed and leaving the rest byte-identical.
///
/// Run at startup before orphan recovery reads the sidecars, so a reattached
/// or abandoned session's config already carries references. A sidecar that
/// cannot be read or parsed is skipped with a warning, as orphan recovery
/// treats it; a store refusal leaves that sidecar as it was and the next start
/// tries again.
pub fn seal_sidecar_secrets(dirs: &Dirs) {
    let metas = read_all_metas(dirs).unwrap_or_else(|err| {
        warn!(?err, "sidecar secret seal: failed to read session sidecars");
        Vec::new()
    });
    for mut meta in metas {
        let path = meta_path(dirs, &meta.session_id);
        let Some(config) = meta.spawn_config.as_mut() else {
            continue;
        };
        let SealOutcome::Sealed(rows) = env_secrets::seal_stored_rows(&config.extra_env, &path)
        else {
            continue;
        };
        config.extra_env = rows;
        if let Err(err) = write_meta(dirs, &meta) {
            warn!(?err, path = %path.display(), "sidecar secret seal: could not rewrite the sidecar");
        }
    }
}

/// Move an undecodable sidecar aside as `meta.json.corrupt`, replacing any
/// earlier quarantined generation. Best-effort: a rename failure just means
/// the warning repeats next boot.
fn quarantine_meta(path: &std::path::Path) {
    let mut target = path.as_os_str().to_owned();
    target.push(".corrupt");
    let target = PathBuf::from(target);
    let _ = std::fs::remove_file(&target);
    if let Err(err) = std::fs::rename(path, &target) {
        warn!(?err, path = %path.display(), "failed to quarantine corrupt meta.json");
    }
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

/// Write `summary` as the session's `summary.json`, atomically, only while its
/// `meta.json` sidecar exists: the session dir is never made here, so a late
/// summary cannot bring back the dir of a session that ended.
pub fn write_summary(
    dirs: &Dirs,
    session_id: &str,
    summary: &protocol::SessionSummary,
) -> anyhow::Result<()> {
    if !meta_path(dirs, session_id).exists() {
        return Ok(());
    }
    let bytes = serde_json::to_vec_pretty(summary).context("serializing the session summary")?;
    crate::atomic_file::write_atomic(
        &crate::paths::session_summary_file(dirs, session_id),
        &bytes,
    )
    .context("writing the session summary")
}

pub fn try_write_summary(dirs: &Dirs, session_id: &str, summary: &protocol::SessionSummary) {
    if let Err(err) = write_summary(dirs, session_id, summary) {
        warn!(error = %format!("{err:#}"), %session_id, "failed to write the session summary");
    }
}

/// The session's saved summary. A missing file is `None`; an unreadable or
/// undecodable one is `None` with a warning, so the session still restores.
pub fn load_summary(dirs: &Dirs, session_id: &str) -> Option<protocol::SessionSummary> {
    let path = crate::paths::session_summary_file(dirs, session_id);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            warn!(%err, path = %path.display(), "could not read the session summary; its ring starts empty");
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(summary) => Some(summary),
        Err(err) => {
            warn!(%err, path = %path.display(), "the session summary does not decode; its ring starts empty");
            None
        }
    }
}

/// Remove the session's `summary.json`; a missing file is not an error.
pub fn delete_summary(dirs: &Dirs, session_id: &str) -> anyhow::Result<()> {
    let path = crate::paths::session_summary_file(dirs, session_id);
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

/// Returns true iff the recorded process still exists *and* is the one the
/// sidecar recorded. The name check alone guards against PID reuse only until
/// a pid is recycled to another session's `rt-tracer`, so a tracer-backed
/// sidecar carrying `tracer_started_at` also has to match the tracer's start
/// time exactly. A sidecar without a stamp (written by an older build, or a
/// start time that could not be read) falls back to the name check, so an
/// upgrade never abandons a session whose tracer is still running.
///
/// Post-C.3, tracer-backed sessions carry `tracer_pid` + `tracer_pipe`; we
/// check the tracer for liveness in that case (the underlying agent process
/// lives behind the tracer's PTY and isn't a direct daemon child anymore).
/// Pre-C.3 sidecars use the legacy `pid` + `program_name` path.
pub fn is_session_alive(meta: &OrphanMeta) -> bool {
    let (sys, pid) = system_with_process(meta.tracer_pid.unwrap_or(meta.pid));
    sys.process(pid)
        .is_some_and(|process| recorded_process_matches(meta, process))
}

/// One `System` holding only `pid`, refreshed so its name and start time are
/// the process's current ones. A single refresh feeds both the check and the
/// kill in [`kill_pid`], so no pid can be recycled between them.
fn system_with_process(pid: u32) -> (System, sysinfo::Pid) {
    let mut sys =
        System::new_with_specifics(RefreshKind::new().with_processes(ProcessRefreshKind::new()));
    let pid = sysinfo::Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::new(),
    );
    (sys, pid)
}

/// Whether `process` is the process `meta` recorded: a tracer-backed sidecar
/// needs the `rt-tracer` name and the tracer's recorded start time, and a
/// pre-C.3 sidecar the recorded program name.
fn recorded_process_matches(meta: &OrphanMeta, process: &sysinfo::Process) -> bool {
    let name = process.name().to_string_lossy().to_lowercase();
    if meta.tracer_pid.is_none() {
        let agent = meta
            .agent
            .clone()
            .unwrap_or_else(agent_registry::default_agent);
        let fallback =
            agent_registry::plugin(&agent).map_or(&[][..], |plugin| plugin.liveness_tokens());
        return name_matches(&name, meta.program_name.as_deref(), fallback);
    }
    let Some(recorded) = meta.tracer_started_at else {
        debug!(
            session_id = %meta.session_id,
            tracer_pid = meta.tracer_pid,
            "sidecar has no tracer start time; falling back to the name check"
        );
        return name_matches(&name, Some("rt-tracer"), &[]);
    };
    let started_at = process.start_time();
    started_at != 0 && started_at == recorded && name_matches(&name, Some("rt-tracer"), &[])
}

/// The start time, in epoch seconds, of the live process `pid`, or `None`
/// when no such process exists or the OS would not open it. `sysinfo` reports
/// `0` for a process it could not open, which reads as unknown here.
pub fn process_start_time(pid: u32) -> Option<u64> {
    let mut sys =
        System::new_with_specifics(RefreshKind::new().with_processes(ProcessRefreshKind::new()));
    let pid = sysinfo::Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::new(),
    );
    let started_at = sys.process(pid)?.start_time();
    (started_at != 0).then_some(started_at)
}

/// Whether the lowercase process `name` is the program a sidecar recorded:
/// it contains `program_token`, or, for the very oldest sidecars that named
/// none, any of the agent's `fallback` liveness tokens.
fn name_matches(name: &str, program_token: Option<&str>, fallback: &[&str]) -> bool {
    match program_token {
        Some(token) => name.contains(&token.to_lowercase()),
        None => fallback.iter().any(|token| name.contains(token)),
    }
}

/// Send SIGKILL / `TerminateProcess` to the recorded process, but only if the
/// one refresh that finds it still matches the sidecar: a tracer-backed
/// sidecar's tracer name and recorded start time (the name alone when the
/// sidecar has no stamp), a pre-C.3 sidecar's recorded program name.
/// Checking and killing the same process object guards
/// against PID reuse (an unrelated process that happens to have the id) and
/// against a pid recycled between the check and the kill. Used by
/// `stop_session` when the in-memory PTY/headless handles are absent (i.e.
/// orphan sessions reattached after daemon restart).
///
/// Returns `true` when the process existed and was signalled; `false` when
/// the pid is gone, the process no longer matches, or the kill call itself
/// failed (e.g. permission denied).
pub fn kill_pid(meta: &OrphanMeta) -> bool {
    let (sys, pid) = system_with_process(meta.tracer_pid.unwrap_or(meta.pid));
    let Some(process) = sys.process(pid) else {
        return false;
    };
    if !recorded_process_matches(meta, process) {
        return false;
    }
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
    agent: Option<AgentId>,
    spawn_config: Option<SpawnConfig>,
    last_prompt: Option<String>,
    current_cwd: Option<String>,
    tracer_pid: Option<u32>,
    tracer_pipe: Option<String>,
    tracer_exe_path: Option<String>,
    conversation_id: Option<String>,
) -> anyhow::Result<OrphanMeta> {
    if pid == 0 {
        return Err(anyhow!("refusing to write orphan meta with pid=0"));
    }
    let tracer_started_at = tracer_pid.and_then(|tracer_pid| {
        let started_at = process_start_time(tracer_pid);
        if started_at.is_none() {
            warn!(
                %session_id,
                tracer_pid,
                "could not read the tracer's start time; its liveness check falls back to the name"
            );
        }
        started_at
    });
    Ok(OrphanMeta {
        on_disk_version: CURRENT_SIDECAR_VERSION,
        session_id,
        pid,
        // At spawn time, the effective label IS the daemon-generated
        // default. Subsequent renames reach the sidecar through the
        // registry's record sync.
        default_label: Some(label.clone()),
        user_label: None,
        label,
        kind,
        mode,
        members,
        started_at,
        workspace_id,
        program_name,
        agent,
        terminal_title: None,
        current_cwd,
        accent_color: None,
        appearance: AppearanceOverrides::default(),
        spawn_config,
        last_prompt,
        recent_actions_tail: Vec::new(),
        // The spawn pipeline fills these from the record it just published;
        // `meta_from_record` never sees that record.
        status: None,
        status_since: None,
        pending_input: None,
        tracer_pid,
        tracer_started_at,
        tracer_pipe,
        tracer_exe_path,
        conversation_id,
        // Mirrored from the record, like the status.
        busy_excluded: false,
        agent_in_shell: false,
        shell_label: None,
        // A recovered shell's spawn sets these on the meta this returns.
        hook_agent: None,
        hook_options: protocol::OptionValues::new(),
        hook_trusted: false,
    })
}

/// The sidecar fields that mirror the live [`crate::session::SessionRecord`].
/// Every change to one of them reaches disk through
/// [`crate::session::SessionRegistry::sync_sidecar`], which reads them from
/// the record inside one critical section, so no writer can put back a stale
/// copy of a field another writer has already moved on.
#[derive(Debug, Clone)]
pub struct RecordMirror {
    pub recent_actions_tail: Vec<String>,
    pub status: SessionStatus,
    pub status_since: Option<DateTime<Utc>>,
    pub pending_input: Option<PendingInput>,
    pub terminal_title: Option<String>,
    pub current_cwd: Option<String>,
    pub default_label: String,
    pub user_label: Option<String>,
    pub label: String,
    pub appearance: AppearanceOverrides,
    pub conversation_id: Option<String>,
    pub busy_excluded: bool,
    pub program_name: Option<String>,
    pub agent_in_shell: bool,
    pub shell_label: Option<String>,
}

impl RecordMirror {
    fn matches(&self, meta: &OrphanMeta) -> bool {
        meta.recent_actions_tail == self.recent_actions_tail
            && meta.status == Some(self.status)
            && meta.status_since == self.status_since
            && meta.pending_input == self.pending_input
            && meta.terminal_title == self.terminal_title
            && meta.current_cwd == self.current_cwd
            && meta.default_label.as_deref() == Some(self.default_label.as_str())
            && meta.user_label == self.user_label
            && meta.label == self.label
            && meta.appearance == self.appearance
            && meta.accent_color.is_none()
            && meta.conversation_id == self.conversation_id
            && meta.busy_excluded == self.busy_excluded
            && meta.program_name == self.program_name
            && meta.agent_in_shell == self.agent_in_shell
            && meta.shell_label == self.shell_label
    }

    fn apply(&self, meta: &mut OrphanMeta) {
        meta.recent_actions_tail
            .clone_from(&self.recent_actions_tail);
        meta.status = Some(self.status);
        meta.status_since = self.status_since;
        meta.pending_input.clone_from(&self.pending_input);
        meta.terminal_title.clone_from(&self.terminal_title);
        meta.current_cwd.clone_from(&self.current_cwd);
        meta.default_label = Some(self.default_label.clone());
        meta.user_label.clone_from(&self.user_label);
        meta.label.clone_from(&self.label);
        // The legacy top-level accent was folded into `appearance` when the
        // record was built, so it is dropped from every rewrite.
        meta.accent_color = None;
        meta.appearance = self.appearance.clone();
        meta.conversation_id.clone_from(&self.conversation_id);
        meta.busy_excluded = self.busy_excluded;
        meta.program_name.clone_from(&self.program_name);
        meta.agent_in_shell = self.agent_in_shell;
        meta.shell_label.clone_from(&self.shell_label);
    }
}

/// Best-effort sync of the record's on-disk mirror into the sidecar: the
/// recent-actions tail, the status/stamp pair, the terminal title and cwd,
/// the labels, the appearance, the two conversation ids and the busy
/// exclusion. Called by the registry
/// after every `update()` so abandoned sessions retain the most recent
/// operational context and a reattach can bring back a session that was still
/// waiting for input. Silently no-ops if the sidecar doesn't exist (e.g. for a
/// brand-new session whose spawn pipeline hasn't written it yet, or a session
/// that never had a sidecar — plain shells in some configurations).
///
/// Returns `Ok(())` for both "meta did not exist" and "meta updated"; real
/// I/O errors surface as `Err` and the caller logs them.
pub fn sync_record_fields(
    dirs: &Dirs,
    session_id: &str,
    mirror: &RecordMirror,
) -> anyhow::Result<()> {
    let path = meta_path(dirs, session_id);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).context("reading meta for record sync"),
    };
    let mut meta = load_meta_from_bytes(&bytes).context("loading meta for record sync")?;
    if mirror.matches(&meta) {
        return Ok(());
    }
    mirror.apply(&mut meta);
    write_meta(dirs, &meta)
}

pub fn try_sync_record_fields(dirs: &Dirs, session_id: &str, mirror: &RecordMirror) {
    if let Err(err) = sync_record_fields(dirs, session_id, mirror) {
        warn!(?err, %session_id, "failed to sync orphan meta record fields");
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

    fn scratch_dirs(tag: &str) -> Dirs {
        let root = std::env::temp_dir().join(format!("rt-orphan-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sessions")).expect("create sessions dir");
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

    /// A sidecar naming no program matches on its agent's liveness tokens;
    /// a recorded program token is matched as before.
    #[test]
    fn name_matches_falls_back_to_the_agents_liveness_tokens() {
        let tokens = agent_registry::plugin(&agent_registry::default_agent())
            .expect("Claude is registered")
            .liveness_tokens();
        assert!(name_matches("node.exe", None, tokens));
        assert!(!name_matches("pwsh.exe", None, tokens));
        assert!(name_matches("rt-tracer.exe", Some("RT-Tracer"), tokens));
        assert!(!name_matches("node.exe", Some("rt-tracer"), tokens));
    }

    fn write_sidecar(dirs: &Dirs, session_id: &str, bytes: &[u8]) -> PathBuf {
        let dir = dirs.sessions_dir.join(session_id);
        std::fs::create_dir_all(&dir).expect("create session dir");
        let path = dir.join("meta.json");
        std::fs::write(&path, bytes).expect("write meta.json");
        path
    }

    #[test]
    fn scan_quarantines_non_json_sidecars_but_keeps_future_versions() {
        let dirs = scratch_dirs("quarantine");
        let valid = br#"{
            "session_id": "ok",
            "pid": 1234,
            "label": "test",
            "kind": "single",
            "mode": "interactive",
            "members": [],
            "started_at": "2024-01-01T00:00:00Z"
        }"#;
        let valid_path = write_sidecar(&dirs, "ok", valid);
        let corrupt_path = write_sidecar(&dirs, "corrupt", b"\x00\x00not json");
        let future = br#"{
            "on_disk_version": 999,
            "session_id": "future",
            "pid": 1234,
            "label": "test",
            "kind": "single",
            "mode": "interactive",
            "members": [],
            "started_at": "2024-01-01T00:00:00Z"
        }"#;
        let future_path = write_sidecar(&dirs, "future", future);

        let metas = read_all_metas(&dirs).expect("scan sessions dir");
        assert_eq!(metas.len(), 1, "only the valid sidecar loads");
        assert_eq!(metas[0].session_id, "ok");

        // Torn-write garbage: moved aside so the next boot doesn't re-warn.
        assert!(!corrupt_path.exists(), "corrupt meta.json must be renamed");
        let quarantined = corrupt_path.with_extension("json.corrupt");
        assert_eq!(
            std::fs::read(&quarantined).expect("read quarantined bytes"),
            b"\x00\x00not json",
            "quarantine must preserve the original bytes"
        );

        // Future-version sidecar: preserved in place for a later upgrade.
        assert!(future_path.exists(), "future-version sidecar must survive");
        // Valid sidecar untouched.
        assert!(valid_path.exists());

        // A second scan is quiet about the quarantined file and still loads
        // the valid one.
        let metas = read_all_metas(&dirs).expect("rescan sessions dir");
        assert_eq!(metas.len(), 1);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

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
    fn sidecar_round_trips_status_and_since() {
        let dirs = scratch_dirs("status-round-trip");
        let since = DateTime::from_timestamp(1_700_000_000, 0).expect("a fixed stamp");
        let mut meta = meta_from_record(
            "s1".to_string(),
            1,
            "s1".to_string(),
            SessionKind::Standalone,
            SessionMode::Interactive,
            Vec::new(),
            since,
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
        meta.status = Some(SessionStatus::AwaitingInput);
        meta.status_since = Some(since);
        write_meta(&dirs, &meta).expect("write meta");

        let loaded = load_meta(&dirs, "s1").expect("load meta");
        assert_eq!(loaded.status, Some(SessionStatus::AwaitingInput));
        assert_eq!(loaded.status_since, Some(since));

        // A sidecar written before these fields existed loads with both unset
        // rather than being rejected.
        let legacy = br#"{
            "on_disk_version": 2,
            "session_id": "legacy",
            "pid": 1234,
            "label": "legacy",
            "kind": "standalone",
            "mode": "interactive",
            "members": [],
            "started_at": "2024-01-01T00:00:00Z"
        }"#;
        write_sidecar(&dirs, "legacy", legacy);
        let loaded = load_meta(&dirs, "legacy").expect("load legacy meta");
        assert_eq!(loaded.status, None);
        assert_eq!(loaded.status_since, None);
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
            agent: Some(agent_registry::default_agent()),
            terminal_title: None,
            current_cwd: Some(r"C:\work\repo".to_string()),
            accent_color: Some("#2f81f7".to_string()),
            appearance: AppearanceOverrides {
                accent_color: Some("#3fb950".to_string()),
                ..AppearanceOverrides::default()
            },
            spawn_config: None,
            last_prompt: None,
            recent_actions_tail: Vec::new(),
            status: None,
            status_since: None,
            pending_input: None,
            tracer_pid: Some(1234),
            tracer_started_at: Some(1_700_000_000),
            tracer_pipe: Some(r"\\.\pipe\rt-tracer-s1".to_string()),
            tracer_exe_path: Some(r"C:\cache\rt-tracer-aaaaaaaaaaaaaaaa.exe".to_string()),
            conversation_id: None,
            busy_excluded: false,
            agent_in_shell: false,
            shell_label: None,
            hook_agent: None,
            hook_options: protocol::OptionValues::new(),
            hook_trusted: false,
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
        assert_eq!(decoded.current_cwd.as_deref(), Some(r"C:\work\repo"));
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
    fn a_sidecar_without_the_busy_flag_loads_counted() {
        let legacy = br#"{
            "on_disk_version": 2,
            "session_id": "legacy",
            "pid": 1234,
            "label": "legacy",
            "kind": "standalone",
            "mode": "interactive",
            "members": [],
            "started_at": "2024-01-01T00:00:00Z"
        }"#;
        let meta = load_meta_from_bytes(legacy).expect("parse a sidecar without the flag");
        assert!(
            !meta.busy_excluded,
            "an older sidecar's session counts as busy"
        );

        let mut excluded = meta;
        excluded.busy_excluded = true;
        let bytes = serde_json::to_vec(&excluded).expect("serialize");
        let decoded = load_meta_from_bytes(&bytes).expect("decode");
        assert!(
            decoded.busy_excluded,
            "the flag round-trips through the sidecar"
        );
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

    #[test]
    fn conversation_id_round_trips_and_defaults_on_old_meta() {
        let dirs = scratch_dirs("claude-session-id");
        let meta = meta_from_record(
            "s-claude".to_string(),
            4242,
            "repo:main".to_string(),
            SessionKind::Single,
            SessionMode::Interactive,
            vec![],
            chrono::DateTime::from_timestamp(0, 0).unwrap(),
            None,
            Some("rt-tracer".to_string()),
            Some(agent_registry::default_agent()),
            None,
            None,
            None,
            Some(4242),
            Some(r"\\.\pipe\rt-tracer-s-claude".to_string()),
            None,
            Some("0b6f4c7e-1111-4222-8333-944455556666".to_string()),
        )
        .expect("build meta");
        write_meta(&dirs, &meta).expect("write meta");
        let loaded = load_meta(&dirs, "s-claude").expect("load meta");
        assert_eq!(
            loaded.conversation_id.as_deref(),
            Some("0b6f4c7e-1111-4222-8333-944455556666")
        );

        // A sidecar written before the field existed decodes with `None`.
        let old = br#"{
            "on_disk_version": 2,
            "session_id": "old",
            "pid": 9001,
            "label": "legacy",
            "kind": "single",
            "mode": "interactive",
            "members": [],
            "started_at": "2024-01-01T00:00:00Z",
            "program_name": "rt-tracer",
            "tracer_pid": 9001,
            "tracer_pipe": "\\\\.\\pipe\\rt-tracer-old"
        }"#;
        let old_meta = load_meta_from_bytes(old).expect("parse old meta");
        assert_eq!(old_meta.conversation_id, None);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// A session whose agent runs inside its shell keeps that, the shell's
    /// name and the program the leaf shows through its sidecar; a sidecar
    /// written before the fields existed loads as an agent that is the child.
    #[test]
    fn agent_in_shell_round_trips_through_the_sidecar() {
        let dirs = scratch_dirs("agent-in-shell-round-trip");
        write_meta(&dirs, &s1_meta(None)).expect("write meta");
        let mut mirror = mirror_for(None);
        mirror.agent_in_shell = true;
        mirror.shell_label = Some("pwsh".to_owned());
        mirror.program_name = Some("pwsh".to_owned());
        sync_record_fields(&dirs, "s1", &mirror).expect("sync the record");
        let loaded = load_meta(&dirs, "s1").expect("load meta");
        assert!(loaded.agent_in_shell, "the flag reached the sidecar");
        assert_eq!(loaded.shell_label.as_deref(), Some("pwsh"));
        assert_eq!(
            loaded.program_name.as_deref(),
            Some("pwsh"),
            "the program the leaf shows follows the record"
        );

        let old = br#"{
            "on_disk_version": 2,
            "session_id": "old",
            "pid": 9001,
            "label": "legacy",
            "kind": "single",
            "mode": "interactive",
            "members": [],
            "started_at": "2024-01-01T00:00:00Z"
        }"#;
        let old_meta = load_meta_from_bytes(old).expect("parse old meta");
        assert!(!old_meta.agent_in_shell);
        assert_eq!(old_meta.shell_label, None);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// A recovered shell's hook agent survives the sidecar; a sidecar
    /// without the field loads as a shell with no hook agent.
    #[test]
    fn hook_agent_round_trips_through_the_sidecar() {
        let dirs = scratch_dirs("hook-agent-round-trip");
        let mut meta = s1_meta(None);
        meta.mode = SessionMode::PlainShell;
        meta.hook_agent = Some(agent_registry::default_agent());
        write_meta(&dirs, &meta).expect("write meta");
        let loaded = load_meta(&dirs, "s1").expect("load meta");
        assert_eq!(loaded.hook_agent, Some(agent_registry::default_agent()));

        let old = br#"{
            "on_disk_version": 2,
            "session_id": "old",
            "pid": 9001,
            "label": "legacy",
            "kind": "single",
            "mode": "plain_shell",
            "members": [],
            "started_at": "2024-01-01T00:00:00Z"
        }"#;
        let old_meta = load_meta_from_bytes(old).expect("parse old meta");
        assert_eq!(old_meta.hook_agent, None);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// A recovered shell's hook options and trust survive the sidecar; a
    /// sidecar without the fields loads as empty options, untrusted.
    #[test]
    fn hook_options_and_trust_round_trip_through_the_sidecar() {
        let dirs = scratch_dirs("hook-options-round-trip");
        let mut meta = s1_meta(None);
        meta.mode = SessionMode::PlainShell;
        meta.hook_agent = Some(agent_registry::default_agent());
        meta.hook_options = protocol::OptionValues::from([(
            "mode".to_owned(),
            protocol::OptionValue::Text("fast".to_owned()),
        )]);
        meta.hook_trusted = true;
        write_meta(&dirs, &meta).expect("write meta");
        let loaded = load_meta(&dirs, "s1").expect("load meta");
        assert_eq!(loaded.hook_options, meta.hook_options);
        assert!(loaded.hook_trusted, "the trust reached the sidecar");

        let old = br#"{
            "on_disk_version": 2,
            "session_id": "old",
            "pid": 9001,
            "label": "legacy",
            "kind": "single",
            "mode": "plain_shell",
            "members": [],
            "started_at": "2024-01-01T00:00:00Z",
            "hook_agent": "claude"
        }"#;
        let old_meta = load_meta_from_bytes(old).expect("parse old meta");
        assert!(old_meta.hook_options.is_empty());
        assert!(!old_meta.hook_trusted);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// A mirror over the fields `meta_from_record` writes for `s1`, carrying
    /// the given conversation id.
    fn mirror_for(conversation_id: Option<&str>) -> RecordMirror {
        RecordMirror {
            recent_actions_tail: Vec::new(),
            status: SessionStatus::Idle,
            status_since: None,
            pending_input: None,
            terminal_title: None,
            current_cwd: None,
            default_label: "s1".to_owned(),
            user_label: None,
            label: "s1".to_owned(),
            appearance: AppearanceOverrides::default(),
            conversation_id: conversation_id.map(str::to_owned),
            busy_excluded: false,
            program_name: Some("rt-tracer".to_owned()),
            agent_in_shell: false,
            shell_label: None,
        }
    }

    /// The spawn-time sidecar `meta_from_record` writes for `s1`, carrying
    /// `conversation_id`.
    fn s1_meta(conversation_id: Option<&str>) -> OrphanMeta {
        meta_from_record(
            "s1".to_string(),
            4242,
            "s1".to_string(),
            SessionKind::Standalone,
            SessionMode::Interactive,
            vec![],
            chrono::DateTime::from_timestamp(0, 0).unwrap(),
            None,
            Some("rt-tracer".to_string()),
            Some(agent_registry::default_agent()),
            None,
            None,
            None,
            Some(4242),
            Some(r"\\.\pipe\rt-tracer-s1".to_string()),
            None,
            conversation_id.map(str::to_owned),
        )
        .expect("build meta")
    }

    /// A pending prompt reaches the sidecar with its status and loads back;
    /// once nothing is pending the field leaves the file.
    #[test]
    fn orphan_meta_round_trips_pending_input() {
        let dirs = scratch_dirs("pending-input-round-trip");
        write_meta(&dirs, &s1_meta(None)).expect("write meta");
        let pending = PendingInput::Permission {
            tool_name: "Bash".to_owned(),
            summary: "cargo build".to_owned(),
        };
        let mut mirror = mirror_for(None);
        mirror.status = SessionStatus::AwaitingInput;
        mirror.pending_input = Some(pending.clone());
        sync_record_fields(&dirs, "s1", &mirror).expect("sync the prompt");
        let loaded = load_meta(&dirs, "s1").expect("load meta");
        assert_eq!(loaded.status, Some(SessionStatus::AwaitingInput));
        assert_eq!(loaded.pending_input, Some(pending));

        sync_record_fields(&dirs, "s1", &mirror_for(None)).expect("sync the answer");
        let text = std::fs::read_to_string(meta_path(&dirs, "s1")).expect("read sidecar");
        assert!(
            !text.contains("pending_input"),
            "nothing pending writes no field: {text}"
        );
        assert_eq!(
            load_meta(&dirs, "s1").expect("load meta").pending_input,
            None
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// Only the conversation id differing is still a mismatch: the mirror
    /// must not skip the write, and applying it moves the id.
    #[test]
    fn record_mirror_matches_on_the_conversation_id() {
        let mirror = mirror_for(Some("new-conv"));
        let mut meta = s1_meta(Some("old-conv"));
        mirror.apply(&mut meta);
        // Every other field now mirrors; only the id differs.
        meta.conversation_id = Some("old-conv".to_owned());
        assert!(
            !mirror.matches(&meta),
            "a different conversation id is not a match"
        );
        mirror.apply(&mut meta);
        assert_eq!(meta.conversation_id.as_deref(), Some("new-conv"));
    }

    #[test]
    fn record_mirror_applies_a_changed_conversation_id_and_skips_an_unchanged_one() {
        let dirs = scratch_dirs("mirror-conversation-id");
        let meta = s1_meta(Some("old-conv"));
        write_meta(&dirs, &meta).expect("write meta");

        // A changed id reaches disk.
        sync_record_fields(&dirs, "s1", &mirror_for(Some("new-conv"))).expect("sync changed id");
        assert_eq!(
            load_meta(&dirs, "s1")
                .expect("load meta")
                .conversation_id
                .as_deref(),
            Some("new-conv"),
            "a /clear's conversation survives a reattach"
        );

        // A matching id leaves the file byte-for-byte as it was; the marker
        // would not survive a rewrite.
        let path = meta_path(&dirs, "s1");
        let mut marked = std::fs::read(&path).expect("read sidecar");
        marked.push(b'\n');
        std::fs::write(&path, &marked).expect("write the marker");
        sync_record_fields(&dirs, "s1", &mirror_for(Some("new-conv"))).expect("sync same id");
        assert_eq!(
            std::fs::read(&path).expect("read sidecar"),
            marked,
            "a matching mirror does not rewrite the sidecar"
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn a_deepseek_spawn_config_survives_the_sidecar() {
        let dirs = scratch_dirs("deepseek-provider");
        let options = protocol::AgentSelection {
            agent: agent_registry::default_agent(),
            options: protocol::OptionValues::from([(
                "provider".to_owned(),
                protocol::OptionValue::Text("deepseek".to_owned()),
            )]),
        };
        let config = SpawnConfig {
            target: protocol::SpawnTarget::Standalone {
                cwd: Some(r"D:\proj".to_owned()),
                add_dirs: Vec::new(),
            },
            mode: SessionMode::Interactive,
            dangerously_skip_permissions: false,
            agent: Some(options.clone()),
            extra_env: Vec::new(),
        };
        let meta = meta_from_record(
            "s-deepseek".to_string(),
            4242,
            "proj".to_string(),
            SessionKind::Standalone,
            SessionMode::Interactive,
            vec![],
            chrono::DateTime::from_timestamp(0, 0).unwrap(),
            None,
            Some("rt-tracer".to_string()),
            Some(agent_registry::default_agent()),
            Some(config),
            None,
            None,
            Some(4242),
            Some(r"\\.\pipe\rt-tracer-s-deepseek".to_string()),
            None,
            None,
        )
        .expect("build meta");
        write_meta(&dirs, &meta).expect("write meta");
        let loaded = load_meta(&dirs, "s-deepseek").expect("load meta");
        assert_eq!(
            loaded.spawn_config.map(|config| config.agent),
            Some(Some(options)),
            "a reattached session keeps its provider"
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// A live process whose executable name contains `rt-tracer`, standing in
    /// for a tracer supervisor. It copies `cmd.exe` under a tracer-shaped name
    /// and parks it in `pause`, holding the piped stdin open until the process
    /// is killed or the guard drops.
    #[cfg(windows)]
    struct TracerLikeProcess {
        child: std::process::Child,
        dirs: Dirs,
    }

    #[cfg(windows)]
    impl TracerLikeProcess {
        fn spawn(tag: &str) -> Self {
            Self::spawn_named(tag, "rt-tracer-test.exe")
        }

        /// Copies `cmd.exe` under `exe_name` in its own scratch dir, so a test
        /// can stand up a process the tracer name check accepts and one it
        /// rejects.
        fn spawn_named(tag: &str, exe_name: &str) -> Self {
            let dirs = scratch_dirs(tag);
            let exe = dirs.config.join(exe_name);
            let system_root = std::env::var_os("SystemRoot").expect("SystemRoot is set on Windows");
            let cmd = std::path::Path::new(&system_root)
                .join("System32")
                .join("cmd.exe");
            std::fs::copy(&cmd, &exe).expect("copy cmd.exe");
            let child = std::process::Command::new(&exe)
                .args(["/c", "pause"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn rt-tracer-test.exe");
            Self { child, dirs }
        }

        fn pid(&self) -> u32 {
            self.child.id()
        }

        fn started_at(&self) -> u64 {
            process_start_time(self.pid()).expect("a live process reports a start time")
        }

        /// The spawn-time sidecar naming this process as its tracer.
        fn meta(&self) -> OrphanMeta {
            meta_from_record(
                "s-tracer".to_string(),
                self.pid(),
                "s-tracer".to_string(),
                SessionKind::Standalone,
                SessionMode::Interactive,
                vec![],
                chrono::DateTime::from_timestamp(0, 0).unwrap(),
                None,
                Some("rt-tracer".to_string()),
                Some(agent_registry::default_agent()),
                None,
                None,
                None,
                Some(self.pid()),
                Some(r"\\.\pipe\rt-tracer-s-tracer".to_string()),
                None,
                None,
            )
            .expect("build meta")
        }

        fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
            self.child.try_wait()
        }

        fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
            self.child.wait()
        }
    }

    #[cfg(windows)]
    impl Drop for TracerLikeProcess {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.dirs.config);
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_tracer_sidecar_whose_start_time_differs_is_not_alive() {
        let tracer = TracerLikeProcess::spawn("tracer-start-time-differs");
        let mut meta = tracer.meta();
        let real = meta
            .tracer_started_at
            .expect("the spawn path stamps the tracer's start time");
        meta.tracer_started_at = Some(real + 5);
        assert!(
            !is_session_alive(&meta),
            "a recycled tracer pid must not pass for this session's tracer"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_tracer_sidecar_without_a_start_time_falls_back_to_the_name() {
        let tracer = TracerLikeProcess::spawn("tracer-no-start-time");
        let mut meta = tracer.meta();
        meta.tracer_started_at = None;
        assert!(
            is_session_alive(&meta),
            "an older sidecar's running tracer is kept on the name check"
        );

        let impostor =
            TracerLikeProcess::spawn_named("tracer-no-start-time-impostor", "plain-cmd-test.exe");
        let mut meta = impostor.meta();
        meta.tracer_started_at = None;
        assert!(
            !is_session_alive(&meta),
            "without a stamp the name check still has to pass"
        );
    }

    #[cfg(windows)]
    #[test]
    fn kill_pid_kills_a_matching_tracer_without_a_start_time() {
        let mut tracer = TracerLikeProcess::spawn("kill-pid-no-start-time");
        let mut meta = tracer.meta();
        meta.tracer_started_at = None;
        assert!(kill_pid(&meta), "the name check still kills the tracer");
        tracer.wait().expect("the killed process is reaped");
    }

    #[cfg(windows)]
    #[test]
    fn kill_pid_spares_a_process_whose_start_time_differs() {
        let mut tracer = TracerLikeProcess::spawn("kill-pid-start-time");
        let mut meta = tracer.meta();
        let real = meta
            .tracer_started_at
            .expect("the spawn path stamps the tracer's start time");

        meta.tracer_started_at = Some(real + 5);
        assert!(!kill_pid(&meta), "a mismatched stamp must not be killed");
        assert!(
            matches!(tracer.try_wait(), Ok(None)),
            "the process is still running"
        );

        meta.tracer_started_at = Some(real);
        assert!(
            matches!(tracer.try_wait(), Ok(None)),
            "the process is still running right before the matching kill"
        );
        assert!(kill_pid(&meta), "the matching tracer is killed");
        tracer.wait().expect("the killed process is reaped");
    }

    #[cfg(windows)]
    #[test]
    fn a_tracer_sidecar_with_its_start_time_is_alive() {
        let tracer = TracerLikeProcess::spawn("tracer-start-time-match");
        let meta = tracer.meta();
        assert_eq!(
            meta.tracer_started_at,
            Some(tracer.started_at()),
            "the stamp is the live process's own start time"
        );
        assert!(is_session_alive(&meta), "the live tracer keeps the session");
    }

    #[test]
    fn tracer_started_at_round_trips_and_is_absent_for_a_pid_less_sidecar() {
        let dirs = scratch_dirs("tracer-started-at");
        let mut meta = s1_meta(None);
        meta.tracer_started_at = Some(1_700_000_123);
        write_meta(&dirs, &meta).expect("write meta");
        let loaded = load_meta(&dirs, "s1").expect("load meta");
        assert_eq!(loaded.tracer_pid, Some(4242));
        assert_eq!(loaded.tracer_started_at, Some(1_700_000_123));

        let mut shell = s1_meta(None);
        shell.tracer_pid = None;
        shell.tracer_pipe = None;
        shell.tracer_started_at = None;
        let json = serde_json::to_value(&shell).expect("serialize");
        assert!(
            json.get("tracer_started_at").is_none(),
            "a sidecar with no tracer writes no start time: {json}"
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }
}
