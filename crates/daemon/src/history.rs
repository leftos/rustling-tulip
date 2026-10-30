//! Ended-session history, kept so a session can be recovered after it ends.
//!
//! Every end path (the child exiting, the user stopping or discarding a
//! session, a drained shutdown) writes one entry to
//! `<config>/history/<session-id>.json` before the session's `meta.json`
//! sidecar is deleted. The first write for a session wins, so a Stop followed
//! by the exit watcher keeps `StoppedByUser` rather than `Exited`. Headless
//! sessions are never written: a headless run has nothing to resume.
//! Startup prunes entries older than [`HISTORY_RETENTION`].

use crate::conversations;
use crate::env_secrets::{self, SealOutcome};
use crate::hook_status::PluginLookup;
use crate::orphan;
use crate::paths::{Dirs, normalize_path_key};
use crate::pty::PtyExit;
use crate::session::{SessionRecord, SessionRegistry};
use crate::sync::lock;
use crate::tracer_log::{self, AgentNote, TracerLogEnd, TracerLogSummary};
use agent_api::{AgentPlugin, RecoveredRun, ResolvedRows, TracerRun};
use anyhow::{Context as _, anyhow};
use chrono::{DateTime, TimeDelta, Utc};
use protocol::{
    AgentId, AgentSelection, ConversationCandidate, HistoryEntry, HistorySource, OptionValues,
    PinnedMemberWorktree, RecoverAs, RecoverItem, RepoEntry, SessionEnd, SessionHistoryItem,
    SessionKind, SessionMember, SessionMode, SpawnConfig, SpawnRequest, SpawnTarget,
    WorkspaceEntry, WorktreeReusePolicy,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use tokio::sync::watch;
use tracing::{debug, info, warn};

/// How long an ended session stays in the history.
pub const HISTORY_RETENTION: TimeDelta = TimeDelta::days(7);

/// How long after a session's end its conversation may still have been written.
const CANDIDATE_GRACE: TimeDelta = TimeDelta::minutes(2);
/// How far before its end a session with no known start is searched from.
const UNKNOWN_START_LOOKBACK: TimeDelta = TimeDelta::days(1);
/// The tracer-log importer's revision for a plain-shell import. An agent's
/// import is stamped with its plugin's `import_rev`; an older import is
/// imported again from its tracer log.
pub const SHELL_IMPORT_REV: u32 = 3;
/// Program file stems an imported plain-shell session may have run.
const SHELL_STEMS: [&str; 6] = ["pwsh", "powershell", "cmd", "bash", "zsh", "sh"];
/// The branch name a recovered repo or workspace spawn asks for when its
/// session recorded none. Its folders are pinned, so nothing checks it out.
const PINNED_BRANCH_FALLBACK: &str = "HEAD";
/// The exit code Windows gives a console child it ends at shutdown or logoff
/// (`0xC000013A`, `STATUS_CONTROL_C_EXIT`).
pub(crate) const STATUS_CONTROL_C_EXIT: i32 = -1_073_741_510;

/// Bumped after every history write, so the server can broadcast the new list.
static CHANGES: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::channel(0).0);

/// A receiver that wakes whenever the history is written.
#[must_use]
pub fn subscribe_changes() -> watch::Receiver<u64> {
    CHANGES.subscribe()
}

/// Serializes history writes inside this daemon, so the exists-check in
/// [`write_if_absent`] and the read-modify-write in [`mark_recovered`] are not
/// interleaved by two end paths racing on one session.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// How many [`write_if_absent`] calls have reached [`WRITE_LOCK`], counted
/// just before each takes it, so a test holding the lock can see a writer
/// parked on it.
#[cfg(test)]
static WRITERS_AT_LOCK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

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
pub(crate) fn primary_cwd(rec: &SessionRecord) -> Option<String> {
    start_folder(&rec.members, rec.spawn_config.as_ref())
}

/// The folder a session with `members` and `config` started in.
pub(crate) fn start_folder(
    members: &[SessionMember],
    config: Option<&SpawnConfig>,
) -> Option<String> {
    if let Some(first) = members.first() {
        return Some(first.worktree_path.clone());
    }
    match config.map(|cfg| &cfg.target) {
        Some(SpawnTarget::Standalone { cwd, .. }) => cwd
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
        agent: rec.wire_agent(),
        spawn_config: rec.spawn_config.clone(),
        members: rec.members.clone(),
        workspace_id: rec.workspace_id.clone(),
        primary_cwd: primary_cwd(rec),
        current_cwd: rec.current_cwd.clone(),
        program_name: rec.program_name.clone(),
        started_at: Some(rec.started_at),
        ended_at,
        end,
        conversation_id: rec.conversation_id.clone(),
        source: HistorySource::Record,
        recovered_at: None,
        end_time_known: true,
        import_rev: 0,
        busy_excluded: rec.busy_excluded,
        // The options and the trusted flag ride in the spawn config.
        options: None,
        trusted: false,
    }
}

/// Build the history entry for the session a dead `meta.json` sidecar
/// describes, ended for `end` at `ended_at`. A `TracerLost` end read at start
/// has no recorded time, so its `ended_at` is a guess and `end_time_known` is
/// false; any other end comes with the time its tracer log gives.
#[must_use]
pub fn entry_from_meta(
    meta: &orphan::OrphanMeta,
    end: SessionEnd,
    ended_at: DateTime<Utc>,
) -> HistoryEntry {
    HistoryEntry {
        session_id: meta.session_id.clone(),
        label: meta.label.clone(),
        kind: meta.kind.clone(),
        mode: meta.mode,
        agent: meta.agent.clone(),
        spawn_config: meta.spawn_config.clone(),
        members: meta.members.clone(),
        workspace_id: meta.workspace_id.clone(),
        primary_cwd: start_folder(&meta.members, meta.spawn_config.as_ref()),
        current_cwd: meta.current_cwd.clone(),
        program_name: meta.program_name.clone(),
        started_at: Some(meta.started_at),
        ended_at,
        end,
        conversation_id: meta.conversation_id.clone(),
        source: HistorySource::Record,
        recovered_at: None,
        end_time_known: !matches!(end, SessionEnd::TracerLost),
        import_rev: 0,
        busy_excluded: meta.busy_excluded,
        // The options and the trusted flag ride in the spawn config.
        options: None,
        trusted: false,
    }
}

/// Record an end for every dead, non-headless sidecar that has no history
/// entry yet: a session the daemon never saw end, because a reboot, a killed
/// process tree or a daemon crash took it. A tracer log that says the session
/// exited or was stopped gives that end and its time, except an exit with
/// [`STATUS_CONTROL_C_EXIT`], which counts as lost. A lost session is written
/// as `TracerLost`, ended at the later of its sidecar's `status_since` and its
/// tracer log's last line, else at its sidecar's modified time, else `now`.
/// An existing entry is kept untouched, and a session that ended more than
/// [`HISTORY_RETENTION`] before `now` is not written, since the prune would
/// delete it in the same start.
///
/// Returns the ids written as `TracerLost`: the sessions lost with no end.
pub fn record_lost_at_start(
    dirs: &Dirs,
    dead: &[orphan::OrphanMeta],
    now: DateTime<Utc>,
) -> Vec<String> {
    let mut lost = Vec::new();
    for meta in dead.iter().filter(|m| m.mode != SessionMode::Headless) {
        let entry = lost_entry(dirs, meta, now);
        if now - entry.ended_at > HISTORY_RETENTION {
            debug!(
                session_id = %meta.session_id,
                ended_at = %entry.ended_at,
                "session ended before the history retention; not recorded"
            );
            continue;
        }
        match write_if_absent(dirs, &entry) {
            Ok(true) if entry.end == SessionEnd::TracerLost => lost.push(entry.session_id),
            Ok(_) => {}
            Err(err) => warn!(
                ?err,
                session_id = %meta.session_id,
                path = %entry_path(dirs, &meta.session_id).display(),
                "failed to record a session lost with no end"
            ),
        }
    }
    lost
}

/// The entry [`record_lost_at_start`] writes for `meta`.
fn lost_entry(dirs: &Dirs, meta: &orphan::OrphanMeta, now: DateTime<Utc>) -> HistoryEntry {
    // A session the daemon kept as the host shut down was lost whatever its
    // tracer log ends with: the shell may have handled the signal and exited
    // with a code of its own.
    let log = if meta.ended_during_host_shutdown {
        None
    } else {
        session_tracer_log(dirs, &meta.session_id)
    };
    if let Some(summary) = &log
        && !is_lost(summary.end)
    {
        return entry_from_meta(meta, end_from_log(summary.end), summary.last_line_at);
    }
    // A tracer logs only lifecycle lines, so its last line can be days older
    // than the loss; the sidecar's status stamp is rewritten on every change.
    let last_seen = [meta.status_since, log.map(|summary| summary.last_line_at)]
        .into_iter()
        .flatten()
        .max();
    let ended_at = last_seen
        .or_else(|| sidecar_modified(dirs, &meta.session_id))
        .unwrap_or(now);
    entry_from_meta(meta, SessionEnd::TracerLost, ended_at)
}

/// Whether a tracer log's end means the session was lost rather than ended:
/// no end line, or the exit Windows forces on a console child at shutdown.
fn is_lost(end: TracerLogEnd) -> bool {
    matches!(
        end,
        TracerLogEnd::Lost
            | TracerLogEnd::Exited {
                code: STATUS_CONTROL_C_EXIT
            }
    )
}

/// Rewrite the start-up loss entry of the abandoned session `session_id` as
/// `StoppedByUser`, keeping every other field: the user dismissed it, so it no
/// longer counts as a session lost with no end. Only an unrecovered
/// `TracerLost` entry [`record_lost_at_start`] wrote (its end time unknown) is
/// rewritten. Returns whether this call rewrote it.
///
/// # Errors
///
/// The entry could not be written back.
pub fn dismiss_start_up_loss(dirs: &Dirs, session_id: &str) -> anyhow::Result<bool> {
    let _guard = lock(&WRITE_LOCK);
    let Some(mut entry) = read_one(dirs, session_id) else {
        return Ok(false);
    };
    let start_up_loss = entry.end == SessionEnd::TracerLost
        && !entry.end_time_known
        && entry.source == HistorySource::Record
        && entry.recovered_at.is_none();
    if !start_up_loss {
        return Ok(false);
    }
    entry.end = SessionEnd::StoppedByUser;
    write_entry(dirs, &entry)?;
    Ok(true)
}

/// The summary of `logs/tracer-<id>.log`, when it exists and has a starting
/// line.
fn session_tracer_log(dirs: &Dirs, session_id: &str) -> Option<TracerLogSummary> {
    let path = dirs
        .config
        .join("logs")
        .join(format!("tracer-{session_id}.log"));
    match std::fs::read(&path) {
        Ok(bytes) => tracer_log::parse_tracer_log(&String::from_utf8_lossy(&bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            warn!(?err, path = %path.display(), "failed to read tracer log");
            None
        }
    }
}

/// When `sessions/<id>/meta.json` was last written.
fn sidecar_modified(dirs: &Dirs, session_id: &str) -> Option<DateTime<Utc>> {
    let path = dirs.sessions_dir.join(session_id).join("meta.json");
    std::fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .map(DateTime::<Utc>::from)
        .map_err(
            |err| warn!(?err, path = %path.display(), "failed to read the sidecar's modified time"),
        )
        .ok()
}

/// Write `entry` unless the session already has one. Returns `Ok(true)` when
/// this call wrote it and `Ok(false)` when an earlier end path got there first.
pub fn write_if_absent(dirs: &Dirs, entry: &HistoryEntry) -> anyhow::Result<bool> {
    #[cfg(test)]
    WRITERS_AT_LOCK.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let _guard = lock(&WRITE_LOCK);
    let path = entry_path(dirs, &entry.session_id);
    if path.exists() {
        return Ok(false);
    }
    write_entry(dirs, entry)?;
    Ok(true)
}

fn write_entry(dirs: &Dirs, entry: &HistoryEntry) -> anyhow::Result<()> {
    write_entry_to(dirs, &entry_path(dirs, &entry.session_id), entry)
}

/// Write `entry` to `path` and wake the history watch. The startup seal pass
/// writes back the exact file it read, so a history file under a name the
/// session id doesn't derive stays where it was.
fn write_entry_to(dirs: &Dirs, path: &Path, entry: &HistoryEntry) -> anyhow::Result<()> {
    let dir = dirs.history_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let bytes = serde_json::to_vec_pretty(entry).context("serializing history entry")?;
    crate::atomic_file::write_atomic(path, &bytes)?;
    CHANGES.send_modify(|n| *n = n.wrapping_add(1));
    Ok(())
}

/// Seal every secret literal row in every history entry's spawn config,
/// rewriting each entry that changed and leaving the rest byte-identical.
///
/// A file that cannot be read or parsed is skipped with a warning, as the
/// history loader treats it; a store refusal leaves that entry as it was and
/// the next start tries again.
pub fn seal_history_secrets(dirs: &Dirs) {
    for (path, mut entry) in scan(dirs).entries {
        let Some(config) = entry.spawn_config.as_mut() else {
            continue;
        };
        let SealOutcome::Sealed(rows) = env_secrets::seal_stored_rows(&config.extra_env, &path)
        else {
            continue;
        };
        config.extra_env = rows;
        if let Err(err) = write_entry_to(dirs, &path, &entry) {
            warn!(?err, path = %path.display(), "history secret seal: could not rewrite the entry");
        }
    }
}

/// A session id that is safe to use as a file stem: never a path.
fn is_plain_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The history entry for `session_id`, if it has a readable one.
#[must_use]
pub fn read_one(dirs: &Dirs, session_id: &str) -> Option<HistoryEntry> {
    if !is_plain_id(session_id) {
        return None;
    }
    let path = entry_path(dirs, session_id);
    if !path.is_file() {
        return None;
    }
    read_entry(&path)
        .map_err(|err| warn!(?err, %session_id, "unreadable session history entry"))
        .ok()
}

/// Record that the session `session_id` ended for `end`, unless it is headless
/// or already recorded. Failures are logged and never block the end path.
/// A session still waiting for its conversation to be discovered gets a final
/// look first, so an id found now is in the entry.
pub fn record_session_end(
    registry: &SessionRegistry,
    dirs: &Dirs,
    session_id: &str,
    end: SessionEnd,
) {
    let Some(rec) = registry.get(session_id) else {
        return;
    };
    conversations::discovery::final_look(registry, dirs, session_id, Utc::now());
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

/// What one pass over the history dir saw.
struct Scan {
    /// The entries it could read, with their paths.
    entries: Vec<(PathBuf, HistoryEntry)>,
    /// Files it could not turn into an entry, already warned about.
    unreadable: Vec<PathBuf>,
    /// Whether the history dir itself could not be listed. A missing dir is
    /// not a failure: a fresh install has none.
    dir_unreadable: bool,
}

/// Every `.json` file in the history dir with the entry it holds. Unreadable
/// and unparseable files are skipped with a warning.
fn scan(dirs: &Dirs) -> Scan {
    let dir = dirs.history_dir();
    let mut out = Scan {
        entries: Vec::new(),
        unreadable: Vec::new(),
        dir_unreadable: false,
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return out,
        Err(err) => {
            warn!(?err, dir = %dir.display(), "failed to read session history dir");
            out.dir_unreadable = true;
            return out;
        }
    };
    for dir_entry in entries {
        let path = match dir_entry {
            Ok(dir_entry) => dir_entry.path(),
            Err(err) => {
                warn!(?err, "skipping unreadable session history entry");
                out.unreadable.push(dir.clone());
                continue;
            }
        };
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        match read_entry(&path) {
            Ok(entry) => out.entries.push((path, entry)),
            Err(err) => {
                warn!(?err, "skipping unreadable session history file");
                out.unreadable.push(path);
            }
        }
    }
    out
}

/// Every history entry, newest end first.
#[must_use]
pub fn read_all(dirs: &Dirs) -> Vec<HistoryEntry> {
    let mut entries: Vec<HistoryEntry> = scan(dirs).entries.into_iter().map(|(_, e)| e).collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.ended_at));
    entries
}

/// Every history entry the dir holds, or the path the scan could not read —
/// the folder itself, or a file it could not parse, which [`prune`] leaves in
/// place on purpose. A startup sweep that must not miss a secret reference
/// declines to act rather than read a skipped file as "nothing".
///
/// # Errors
///
/// The path of the folder or file that could not be read.
pub fn read_all_for_references(dirs: &Dirs) -> Result<Vec<HistoryEntry>, PathBuf> {
    let scan = scan(dirs);
    match scan.unreadable.into_iter().next() {
        Some(path) => Err(path),
        None if scan.dir_unreadable => Err(dirs.history_dir()),
        None => Ok(scan.entries.into_iter().map(|(_, entry)| entry).collect()),
    }
}

/// Stamp the entry for `session_id` as recovered at `at`.
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
    for (path, entry) in scan(dirs).entries {
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

/// The registered repos and workspaces an imported session is matched against.
pub struct Registered<'a> {
    pub repos: &'a [RepoEntry],
    pub workspaces: &'a [WorkspaceEntry],
}

/// Import the sessions that ended before the daemon kept a history, from
/// their `<config>/logs/tracer-<id>.log` files modified within
/// [`HISTORY_RETENTION`] of `now`. A session in `skip` (a live or abandoned
/// sidecar) is left alone, and so is one already in the history unless it is
/// an unrecovered import from before its importer's revision, which is
/// imported again. A log naming no program or one that is neither
/// an agent's nor a known shell is skipped. A session with no end line in the daemon logs gets
/// the log's modified time as `ended_at` and `end_time_known: false`. Returns
/// how many entries were written.
pub fn import_tracer_logs(
    dirs: &Dirs,
    registered: &Registered<'_>,
    skip: &HashSet<String>,
    now: DateTime<Utc>,
) -> usize {
    let logs_dir = dirs.config.join("logs");
    let listing = match std::fs::read_dir(&logs_dir) {
        Ok(listing) => listing,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(err) => {
            warn!(?err, dir = %logs_dir.display(), "failed to list tracer logs");
            return 0;
        }
    };
    let mut end_times: Option<HashMap<String, DateTime<Utc>>> = None;
    let mut imported = 0;
    for path in listing.filter_map(Result::ok).map(|e| e.path()) {
        let Some(id) = tracer_log_id(&path) else {
            continue;
        };
        if skip.contains(&id) || !is_importable(&entry_path(dirs, &id)) {
            continue;
        }
        let Some((modified, summary)) = read_recent_tracer_log(&path, now) else {
            continue;
        };
        let ends = end_times.get_or_insert_with(|| daemon_end_times(&logs_dir));
        let end = match ends.get(&id) {
            Some(at) => ImportEnd {
                at: *at,
                known: true,
            },
            None => ImportEnd {
                at: modified,
                known: false,
            },
        };
        let Some(entry) = entry_from_tracer_log(&id, &summary, end, registered) else {
            continue;
        };
        match write_import(dirs, &entry) {
            Ok(true) => imported += 1,
            Ok(false) => {}
            Err(err) => warn!(?err, session_id = %id, "failed to import tracer log"),
        }
    }
    if imported > 0 {
        info!(imported, "imported ended sessions from tracer logs");
    }
    imported
}

/// Whether the importer may write the history file at `path`: there is none,
/// or it holds an unrecovered import from before its importer's revision
/// (its plugin's `import_rev`, or [`SHELL_IMPORT_REV`] for a plain shell). A
/// recorded entry, a recovered one, a current import, an import naming an
/// agent this daemon has no plugin for and an unreadable file are kept.
fn is_importable(path: &Path) -> bool {
    if !path.exists() {
        return true;
    }
    match read_entry(path) {
        Ok(existing) => {
            existing.source == HistorySource::TracerLog
                && existing.recovered_at.is_none()
                && current_import_rev(&existing).is_some_and(|rev| existing.import_rev < rev)
        }
        Err(err) => {
            warn!(?err, "keeping unreadable session history file");
            false
        }
    }
}

/// The importer revision an import of `entry`'s agent is stamped with now:
/// its plugin's `import_rev`, or [`SHELL_IMPORT_REV`] for a plain shell.
/// `None` when no plugin is registered for its agent.
fn current_import_rev(entry: &HistoryEntry) -> Option<u32> {
    match &entry.agent {
        None => Some(SHELL_IMPORT_REV),
        Some(agent) => agent_registry::plugin(agent).map(agent_api::AgentPlugin::import_rev),
    }
}

/// Write an imported `entry` when [`is_importable`] still allows it, checked
/// under the write lock. Returns `Ok(true)` when this call wrote it.
fn write_import(dirs: &Dirs, entry: &HistoryEntry) -> anyhow::Result<bool> {
    let _guard = lock(&WRITE_LOCK);
    if !is_importable(&entry_path(dirs, &entry.session_id)) {
        return Ok(false);
    }
    write_entry(dirs, entry)?;
    Ok(true)
}

/// When an imported session ended, and whether a daemon log said so.
#[derive(Debug, Clone, Copy)]
struct ImportEnd {
    at: DateTime<Utc>,
    known: bool,
}

/// `<id>` of a `tracer-<id>.log` file name.
fn tracer_log_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let id = name.strip_prefix("tracer-")?.strip_suffix(".log")?;
    is_plain_id(id).then(|| id.to_owned())
}

/// The log's modified time and summary, when it was modified within
/// [`HISTORY_RETENTION`] of `now` and has a starting line.
fn read_recent_tracer_log(
    path: &Path,
    now: DateTime<Utc>,
) -> Option<(DateTime<Utc>, TracerLogSummary)> {
    let modified: DateTime<Utc> = std::fs::metadata(path).ok()?.modified().ok()?.into();
    if now - modified > HISTORY_RETENTION {
        return None;
    }
    let bytes = std::fs::read(path)
        .map_err(|err| warn!(?err, path = %path.display(), "failed to read tracer log"))
        .ok()?;
    let summary = tracer_log::parse_tracer_log(&String::from_utf8_lossy(&bytes))?;
    Some((modified, summary))
}

/// Each session's earliest end time in `daemon.log`, `daemon.log.1` and
/// `daemon.log.old`.
fn daemon_end_times(logs_dir: &Path) -> HashMap<String, DateTime<Utc>> {
    let mut merged: HashMap<String, DateTime<Utc>> = HashMap::new();
    for name in ["daemon.log", "daemon.log.1", "daemon.log.old"] {
        let Ok(bytes) = std::fs::read(logs_dir.join(name)) else {
            continue;
        };
        for (id, at) in tracer_log::session_end_times(&String::from_utf8_lossy(&bytes)) {
            merged
                .entry(id)
                .and_modify(|end| *end = (*end).min(at))
                .or_insert(at);
        }
    }
    merged
}

/// The lowercase file stem of `program`, whichever separator its path uses.
fn program_stem(program: &str) -> Option<String> {
    let leaf = program.rsplit(['/', '\\']).next()?;
    let stem = Path::new(leaf).file_stem()?.to_str()?.to_lowercase();
    (!stem.is_empty()).then_some(stem)
}

fn end_from_log(end: TracerLogEnd) -> SessionEnd {
    match end {
        TracerLogEnd::StoppedByUser => SessionEnd::StoppedByUser,
        TracerLogEnd::Exited { code } => SessionEnd::Exited { code },
        TracerLogEnd::Lost => SessionEnd::TracerLost,
    }
}

/// The history entry a tracer log describes. A log whose agent note names a
/// registered agent is that agent's session (see [`entry_from_agent_note`]);
/// a note naming no registered agent is ignored. Otherwise the run is offered to every
/// agent's plugin in order, and the first that recognises it gives the
/// entry's program name, extra folders, options and trust. Such a session is
/// matched to the workspace (cwd its first member, extra folders its other
/// members) or the repo (cwd its path, no extra folder) it ran in, but never
/// gets a `spawn_config`: the log doesn't say how the session was spawned.
/// Recovery runs the agent in the same folders, under the matched workspace
/// or repo with each member pinned to its folder, else with the same extra
/// folders. A run no plugin recognises is a plain shell when its program is
/// one, with no agent.
fn entry_from_tracer_log(
    id: &str,
    summary: &TracerLogSummary,
    end: ImportEnd,
    registered: &Registered<'_>,
) -> Option<HistoryEntry> {
    let cwd = summary.cwd.clone();
    let shell = HistoryEntry {
        session_id: id.to_owned(),
        label: String::new(),
        kind: SessionKind::Standalone,
        mode: SessionMode::PlainShell,
        agent: None,
        spawn_config: None,
        members: Vec::new(),
        workspace_id: None,
        primary_cwd: Some(cwd.clone()),
        current_cwd: Some(cwd.clone()),
        program_name: None,
        started_at: Some(summary.started_at),
        ended_at: end.at,
        end: end_from_log(summary.end),
        conversation_id: None,
        source: HistorySource::TracerLog,
        recovered_at: None,
        end_time_known: end.known,
        import_rev: SHELL_IMPORT_REV,
        // A tracer log does not record the flag; an imported session counts.
        busy_excluded: false,
        options: None,
        trusted: false,
    };
    if let Some(note) = &summary.agent_note {
        if let Some(plugin) = agent_registry::plugin(&note.agent) {
            return Some(entry_from_agent_note(note, plugin, shell, registered));
        }
        warn!(
            session_id = id,
            agent = %note.agent.0,
            "the tracer log's agent note names an unregistered agent; importing the log by its program"
        );
    }
    let run = TracerRun {
        program: &summary.program,
        args: &summary.args,
        cwd: &summary.cwd,
    };
    for plugin in agent_registry::all() {
        let Some(imported) = plugin.import_tracer_run(&run) else {
            continue;
        };
        let (kind, workspace_id) = match_target(&cwd, &imported.extra_dirs, registered);
        return Some(HistoryEntry {
            kind,
            mode: SessionMode::Interactive,
            agent: Some(plugin.descriptor().id.clone()),
            members: folder_members(&cwd, &imported.extra_dirs, registered.repos),
            workspace_id,
            current_cwd: None,
            program_name: Some(imported.program_name),
            import_rev: plugin.import_rev(),
            options: Some(imported.options),
            trusted: imported.trusted,
            ..shell
        });
    }
    let stem = program_stem(&summary.program)?;
    SHELL_STEMS
        .contains(&stem.as_str())
        .then_some(HistoryEntry {
            program_name: Some(stem),
            ..shell
        })
}

/// The agent entry a shell-hosted session's agent note describes, built on
/// the log's `shell` entry: the note's agent, options, trust and
/// conversation, matched to its folders like a recognised run.
fn entry_from_agent_note(
    note: &AgentNote,
    plugin: &dyn AgentPlugin,
    shell: HistoryEntry,
    registered: &Registered<'_>,
) -> HistoryEntry {
    let cwd = shell.primary_cwd.clone().unwrap_or_default();
    let (kind, workspace_id) = match_target(&cwd, &note.extra_dirs, registered);
    HistoryEntry {
        kind,
        mode: SessionMode::Interactive,
        agent: Some(note.agent.clone()),
        members: folder_members(&cwd, &note.extra_dirs, registered.repos),
        workspace_id,
        current_cwd: None,
        program_name: Some(plugin.descriptor().tag.clone()),
        import_rev: plugin.import_rev(),
        options: Some(note.options.clone()),
        trusted: note.trusted,
        conversation_id: note.conversation_id.clone(),
        ..shell
    }
}

fn repo_at<'a>(repos: &'a [RepoEntry], path: &str) -> Option<&'a RepoEntry> {
    let key = normalize_path_key(path);
    repos.iter().find(|r| normalize_path_key(&r.path) == key)
}

/// The workspace an agent session in `cwd` with `add_dirs` ran for, else the
/// repo, else neither.
fn match_target(
    cwd: &str,
    add_dirs: &[String],
    registered: &Registered<'_>,
) -> (SessionKind, Option<String>) {
    let cwd_key = normalize_path_key(cwd);
    let mut add_keys: Vec<String> = add_dirs.iter().map(|d| normalize_path_key(d)).collect();
    add_keys.sort();
    for ws in registered.workspaces {
        let member_keys: Option<Vec<String>> = ws
            .member_repo_ids
            .iter()
            .map(|id| {
                registered
                    .repos
                    .iter()
                    .find(|r| &r.id == id)
                    .map(|r| normalize_path_key(&r.path))
            })
            .collect();
        let Some((first, rest)) = member_keys.as_deref().and_then(<[String]>::split_first) else {
            continue;
        };
        let mut rest = rest.to_vec();
        rest.sort();
        if *first == cwd_key && rest == add_keys {
            return (SessionKind::Workspace, Some(ws.id.clone()));
        }
    }
    if add_dirs.is_empty() && repo_at(registered.repos, cwd).is_some() {
        return (SessionKind::Single, None);
    }
    (SessionKind::Standalone, None)
}

/// One member per folder, `cwd` first: the registered repo there when there
/// is one, else just the path.
fn folder_members(cwd: &str, add_dirs: &[String], repos: &[RepoEntry]) -> Vec<SessionMember> {
    std::iter::once(cwd)
        .chain(add_dirs.iter().map(String::as_str))
        .map(|path| {
            let repo = repo_at(repos, path);
            SessionMember {
                repo_id: repo.map(|r| r.id.clone()).unwrap_or_default(),
                repo_name: repo.map(|r| r.name.clone()).unwrap_or_default(),
                branch: String::new(),
                worktree_path: path.to_owned(),
            }
        })
        .collect()
}

/// The folder a history entry is about: a plain shell's last folder, else
/// where the session started, else its first member's worktree. An
/// interactive session's agent keeps the folder it started in, wherever its
/// shell later went.
#[must_use]
pub fn entry_folder(entry: &HistoryEntry) -> Option<&str> {
    let last = entry
        .current_cwd
        .as_deref()
        .filter(|_| entry.mode != SessionMode::Interactive);
    last.or(entry.primary_cwd.as_deref())
        .or(entry.current_cwd.as_deref())
        .or_else(|| entry.members.first().map(|m| m.worktree_path.as_str()))
        .filter(|folder| !folder.is_empty())
}

/// The environment rows `entry`'s spawn recorded, or none without a config.
#[must_use]
pub fn recorded_env(entry: &HistoryEntry) -> &[(String, String)] {
    entry
        .spawn_config
        .as_ref()
        .map_or(&[], |config| config.extra_env.as_slice())
}

/// The history as the client lists it: every non-headless entry, newest end
/// first, with the conversations it may be recovered into, what its folder is
/// and whether a shell can resume its conversation. `lookup_rows` gives the
/// rows an entry's conversations are looked up with; `plugins` finds an
/// agent's plugin.
#[must_use]
pub fn history_items(
    dirs: &Dirs,
    repos: &[RepoEntry],
    lookup_rows: &dyn Fn(&HistoryEntry) -> ResolvedRows,
    plugins: PluginLookup,
) -> Vec<SessionHistoryItem> {
    let now = Utc::now();
    read_all(dirs)
        .into_iter()
        .filter(|entry| entry.mode != SessionMode::Headless)
        .map(|entry| {
            let rows = lookup_rows(&entry);
            history_item(entry, repos, &rows, now, plugins)
        })
        .collect()
}

fn history_item(
    entry: HistoryEntry,
    repos: &[RepoEntry],
    rows: &ResolvedRows,
    now: DateTime<Utc>,
    plugins: PluginLookup,
) -> SessionHistoryItem {
    let folder = entry_folder(&entry).map(str::to_owned);
    let candidates = match folder.as_deref() {
        Some(folder) => conversation_candidates(&entry, rows, folder, now),
        None => Vec::new(),
    };
    let folder_is_git_repo = folder
        .as_deref()
        .is_some_and(|folder| Path::new(folder).join(".git").exists());
    let folder_repo_id = folder
        .as_deref()
        .and_then(|folder| repo_at(repos, folder))
        .map(|repo| repo.id.clone());
    // The row's first conversation is the one its shell would resume.
    let shell_resume = shell_resume_applies(
        &entry,
        candidates.first().map(|candidate| candidate.id.as_str()),
        plugins,
    );
    SessionHistoryItem {
        entry,
        candidates,
        folder_is_git_repo,
        folder_repo_id,
        shell_resume,
    }
}

/// The entry's own conversation when it knows one and its transcript is
/// still there; otherwise the transcripts written in `folder` while the
/// session ran, up to `now` when its end time is unknown.
fn conversation_candidates(
    entry: &HistoryEntry,
    rows: &ResolvedRows,
    folder: &str,
    now: DateTime<Utc>,
) -> Vec<ConversationCandidate> {
    let agent = recovery_agent(entry);
    let Some(plugin) = agent_registry::plugin(&agent) else {
        warn!(
            session_id = %entry.session_id,
            "no agent plugin `{}` is registered; the session has no conversation candidates",
            agent.0
        );
        return Vec::new();
    };
    let folder = Path::new(folder);
    let found = if let Some(id) = &entry.conversation_id {
        conversations::known(plugin, folder, id, rows)
            .into_iter()
            .collect()
    } else {
        conversations::candidates(plugin, folder, candidate_window(entry, now), rows)
    };
    found
        .into_iter()
        .map(|c| ConversationCandidate {
            id: c.id,
            last_active: c.last_active,
            title: c.title,
        })
        .collect()
}

/// The span a session's conversation may have been written in. A known end
/// reaches [`CANDIDATE_GRACE`] past it; an unknown one reaches `now`, from the
/// start or, with no start either, from [`HISTORY_RETENTION`] before `now`.
fn candidate_window(entry: &HistoryEntry, now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    if entry.end_time_known {
        let start = entry
            .started_at
            .unwrap_or(entry.ended_at - UNKNOWN_START_LOOKBACK);
        (start, entry.ended_at + CANDIDATE_GRACE)
    } else {
        let start = entry.started_at.unwrap_or(now - HISTORY_RETENTION);
        (start, now)
    }
}

/// What recovering one history entry does: register `register_repo` first
/// when set, then spawn `request`. A shell that resumes a conversation also
/// carries the agent it resumes in, `hook_agent`, and the conversation,
/// which the spawn takes beside the request and never on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryPlan {
    pub request: SpawnRequest,
    pub register_repo: Option<String>,
    /// The agent a recovered shell resumes its conversation in, whose hooks
    /// map its events; `None` for every other plan.
    pub hook_agent: Option<AgentId>,
    /// The conversation a recovered shell resumes in `hook_agent`.
    pub resume_conversation: Option<String>,
    /// The options of the selection a recovered shell resumes in
    /// `hook_agent`, which its hook plan and resume command take; empty for
    /// every other plan.
    pub hook_options: OptionValues,
    /// Whether the run a recovered shell resumes in `hook_agent` was trusted;
    /// false for every other plan.
    pub hook_trusted: bool,
}

impl RecoveryPlan {
    /// Point a plan that registers its folder as a repo at the id that
    /// registration gave it: the planner pins the folder in a Single target
    /// before the repo has an id.
    pub fn bind_registered_repo(&mut self, registered_id: &str) {
        if self.register_repo.is_some()
            && let SpawnTarget::Single { repo_id, .. } = &mut self.request.target
        {
            registered_id.clone_into(repo_id);
        }
    }

    /// The folder whose checked-out branch should name a pinned repo or
    /// workspace target that asks for the [`PINNED_BRANCH_FALLBACK`]
    /// placeholder: its first pinned member's. `None` for any other plan.
    #[must_use]
    pub fn placeholder_branch_folder(&self) -> Option<&str> {
        match &self.request.target {
            SpawnTarget::Single {
                branch_name,
                existing_worktree: Some(path),
                ..
            } if branch_name == PINNED_BRANCH_FALLBACK => Some(path),
            SpawnTarget::Workspace {
                branch_name,
                existing_worktrees,
                ..
            } if branch_name == PINNED_BRANCH_FALLBACK => {
                existing_worktrees.first().map(|pin| pin.path.as_str())
            }
            _ => None,
        }
    }

    /// Name the plan's repo or workspace target's branch `name`.
    pub fn name_branch(&mut self, name: String) {
        match &mut self.request.target {
            SpawnTarget::Single { branch_name, .. }
            | SpawnTarget::Workspace { branch_name, .. } => {
                *branch_name = name;
            }
            SpawnTarget::Standalone { .. } => {}
        }
    }
}

/// The branch a pinned target is named for, given what reading the pinned
/// folder's current branch returned: that branch, else
/// [`PINNED_BRANCH_FALLBACK`] for a detached HEAD or a failed read.
#[must_use]
pub fn branch_or_placeholder(current: anyhow::Result<Option<String>>) -> String {
    match current {
        Ok(Some(branch)) => branch,
        Ok(None) => PINNED_BRANCH_FALLBACK.to_owned(),
        Err(err) => {
            warn!(
                ?err,
                "reading a recovered folder's branch failed; naming it HEAD"
            );
            PINNED_BRANCH_FALLBACK.to_owned()
        }
    }
}

/// Plan how `item` recovers `entry`. A session goes back under the repo or
/// workspace in `registered` it ran in, with every member pinned to the
/// folder it ran in, so recovery never checks out a branch or creates a
/// worktree. An agent session recovers as its agent, or as a shell that
/// resumes its conversation when the agent's plugin can build that command,
/// or as a bare shell. `conversation_exists(agent, folder, id)` says whether
/// `agent` still holds conversation `id` for `folder`; `plugins` finds an
/// agent's plugin. The error is the message the client shows for the item.
pub fn plan_recovery(
    entry: &HistoryEntry,
    item: &RecoverItem,
    registered: &Registered<'_>,
    plugins: PluginLookup,
    conversation_exists: impl Fn(&AgentId, &str, &str) -> bool,
) -> Result<RecoveryPlan, String> {
    if entry.recovered_at.is_some() {
        return Err("already recovered".to_owned());
    }
    if entry.mode == SessionMode::Headless {
        return Err("a headless session can't be recovered".to_owned());
    }
    let folder = entry_folder(entry).unwrap_or_default();
    let plan = match &item.how {
        RecoverAs::Unknown => return Err("unsupported recovery kind".to_owned()),
        // A bare shell runs no agent and resumes nothing, so it gets no
        // hooks.
        RecoverAs::Shell => RecoveryPlan {
            request: shell_request(entry, folder),
            register_repo: None,
            hook_agent: None,
            resume_conversation: None,
            hook_options: OptionValues::new(),
            hook_trusted: false,
        },
        RecoverAs::Agent => {
            let spawn = recovered_spawn(entry, item, plugins)?;
            let conversation = existing_conversation(item, &spawn, folder, &conversation_exists)?;
            RecoveryPlan {
                request: agent_request(entry, folder, conversation, registered, spawn),
                register_repo: None,
                hook_agent: None,
                resume_conversation: None,
                hook_options: OptionValues::new(),
                hook_trusted: false,
            }
        }
        RecoverAs::RegisterRepoThenAgent { path } => {
            if normalize_path_key(path) != normalize_path_key(folder) {
                return Err(format!("{path} is not the session's folder {folder}"));
            }
            let spawn = recovered_spawn(entry, item, plugins)?;
            let conversation = existing_conversation(item, &spawn, folder, &conversation_exists)?;
            let add_dirs: Vec<String> = entry
                .members
                .iter()
                .skip(1)
                .map(|m| m.worktree_path.clone())
                .collect();
            // A Single target has no `--add-dir`s, so a session that had some
            // keeps them in the folder instead of joining the new repo.
            let target = if add_dirs.is_empty() {
                let branch = recorded_branch(&entry.members);
                pinned_single(String::new(), path, branch)
            } else {
                SpawnTarget::Standalone {
                    cwd: Some(path.clone()),
                    add_dirs,
                }
            };
            RecoveryPlan {
                request: folder_agent(entry, target, conversation, spawn),
                register_repo: Some(path.clone()),
                hook_agent: None,
                resume_conversation: None,
                hook_options: OptionValues::new(),
                hook_trusted: false,
            }
        }
        RecoverAs::ShellResume => {
            let spawn = recovered_spawn(entry, item, plugins)?;
            let conversation = existing_conversation(item, &spawn, folder, &conversation_exists)?;
            RecoveryPlan {
                request: shell_request(entry, folder),
                register_repo: None,
                hook_agent: Some(spawn.selection.agent),
                resume_conversation: Some(conversation.to_owned()),
                hook_options: spawn.selection.options,
                hook_trusted: spawn.dangerously_skip_permissions,
            }
        }
    };
    Ok(plan)
}

/// The conversation `item` resumes, once `conversation_exists` says
/// `spawn`'s agent still holds it for `folder`.
fn existing_conversation<'a>(
    item: &'a RecoverItem,
    spawn: &RecoveredSpawn,
    folder: &str,
    conversation_exists: &impl Fn(&AgentId, &str, &str) -> bool,
) -> Result<&'a str, String> {
    let id = required_conversation(item)?;
    if !conversation_exists(&spawn.selection.agent, folder, id) {
        return Err(format!("conversation {id} not found for {folder}"));
    }
    Ok(id)
}

/// The agent and options a recovery of an entry spawns with, and its trust.
struct RecoveredSpawn {
    selection: AgentSelection,
    dangerously_skip_permissions: bool,
}

/// The agent a recovery of `entry` runs: the one it ran, or the default agent
/// for a plain shell, whose user may have run that agent in it.
fn recovery_agent(entry: &HistoryEntry) -> AgentId {
    entry
        .agent
        .clone()
        .unwrap_or_else(agent_registry::default_agent)
}

/// What an agent recovery of `entry` spawns with: the options its plugin's
/// `recovery_options` gives for what the entry recorded. A shell that resumes
/// the conversation is planned only when the plugin can type that command
/// into a shell (`caps.shell_resume`) and its `shell_resume` builds one; its
/// refusal is the error.
fn recovered_spawn(
    entry: &HistoryEntry,
    item: &RecoverItem,
    plugins: PluginLookup,
) -> Result<RecoveredSpawn, String> {
    let (agent, plugin, options, trusted) = recovery_setup(entry, plugins)?;
    if item.how == RecoverAs::ShellResume {
        shell_resume_command(plugin, &options, required_conversation(item)?)?;
    }
    Ok(RecoveredSpawn {
        selection: AgentSelection { agent, options },
        dangerously_skip_permissions: trusted,
    })
}

/// The agent a recovery of `entry` runs, its plugin, the options the plugin
/// gives for what the entry recorded, and whether it ran trusted. The error
/// is what the client shows when the recovery cannot start.
fn recovery_setup(
    entry: &HistoryEntry,
    plugins: PluginLookup,
) -> Result<(AgentId, &'static dyn AgentPlugin, OptionValues, bool), String> {
    let agent = recovery_agent(entry);
    let plugin =
        plugins(&agent).ok_or_else(|| format!("no agent plugin `{}` is registered", agent.0))?;
    let (recorded, trusted) = recorded_options(entry);
    let options = plugin
        .recovery_options(&RecoveredRun {
            recorded: recorded.as_ref(),
            trusted,
        })
        .map_err(|err| err.to_string())?;
    Ok((agent, plugin, options, trusted))
}

/// The command a shell types to resume `conversation` with `options`; the
/// error says why the agent cannot, which is the refusal the client shows. A
/// plugin without `caps.shell_resume` cannot, whatever its `shell_resume`
/// builds.
fn shell_resume_command(
    plugin: &dyn AgentPlugin,
    options: &OptionValues,
    conversation: &str,
) -> Result<String, String> {
    let descriptor = plugin.descriptor();
    let command = if descriptor.caps.shell_resume {
        plugin
            .shell_resume(options, conversation, &[])
            .map_err(|err| err.to_string())?
    } else {
        None
    };
    command.ok_or_else(|| {
        format!(
            "{} cannot resume a conversation in a shell",
            descriptor.label
        )
    })
}

/// Whether recovering `entry` as a shell that resumes `conversation` can
/// work: its agent's plugin has the cap, gives options for what the entry
/// recorded, and builds that resume command. With no conversation it cannot.
fn shell_resume_applies(
    entry: &HistoryEntry,
    conversation: Option<&str>,
    plugins: PluginLookup,
) -> bool {
    let Some(conversation) = conversation else {
        return false;
    };
    recovery_setup(entry, plugins)
        .and_then(|(_, plugin, options, _)| shell_resume_command(plugin, &options, conversation))
        .is_ok()
}

/// The options `entry` recorded and whether it ran trusted. An agent spawn
/// config gives its selection's options and its skip flag. A plain shell's
/// config, whose flags never reached an agent, or no config gives the options
/// and trust an import read from its tracer log, `None` when it read none.
fn recorded_options(entry: &HistoryEntry) -> (Option<OptionValues>, bool) {
    if let Some(config) = &entry.spawn_config
        && config.mode != SessionMode::PlainShell
    {
        let options = config
            .agent
            .as_ref()
            .map(|selection| selection.options.clone());
        return (options, config.dangerously_skip_permissions);
    }
    (entry.options.clone(), entry.trusted)
}

fn required_conversation(item: &RecoverItem) -> Result<&str, String> {
    item.conversation_id
        .as_deref()
        .ok_or_else(|| "no conversation to resume".to_owned())
}

/// The agent resuming `conversation` where the session ran: its own spawn
/// config when it has one, with every repo member pinned to its recorded
/// folder; else the registered workspace or repo its folders are, pinned the
/// same way; else its folder with its other members as `--add-dir`s.
fn agent_request(
    entry: &HistoryEntry,
    folder: &str,
    conversation: &str,
    registered: &Registered<'_>,
    spawn: RecoveredSpawn,
) -> SpawnRequest {
    let Some(config) = &entry.spawn_config else {
        let members = entry_members(entry, folder, registered.repos);
        let target = folder_target(entry, &members, registered);
        return folder_agent(entry, target, conversation, spawn);
    };
    let mut request = recorded_request(entry, config, folder, registered);
    request.agent = Some(spawn.selection);
    request.dangerously_skip_permissions = spawn.dangerously_skip_permissions;
    request.mode = SessionMode::Interactive;
    request.resume_conversation = Some(conversation.to_owned());
    request
}

/// `config` as a request with its recorded target pinned to the folders the
/// session ran in, else the registered workspace or repo its folders are,
/// else its folder; no first prompt and no injector.
fn recorded_request(
    entry: &HistoryEntry,
    config: &SpawnConfig,
    folder: &str,
    registered: &Registered<'_>,
) -> SpawnRequest {
    let members = entry_members(entry, folder, registered.repos);
    let mut request = config.to_clone_request();
    request.target = pin_recorded(&config.target, &members, registered)
        .unwrap_or_else(|| folder_target(entry, &members, registered));
    request
}

/// The entry's members, or one for `folder` when it recorded none.
fn entry_members(entry: &HistoryEntry, folder: &str, repos: &[RepoEntry]) -> Vec<SessionMember> {
    if entry.members.is_empty() {
        folder_members(folder, &[], repos)
    } else {
        entry.members.clone()
    }
}

/// The recorded `target` with every member pinned to the folder it ran in,
/// or `None` when some member can't be: a Single with other than one member
/// or whose repo is no longer registered, or a workspace that is gone or
/// whose members no longer match.
pub(crate) fn pin_recorded(
    target: &SpawnTarget,
    members: &[SessionMember],
    registered: &Registered<'_>,
) -> Option<SpawnTarget> {
    match target {
        SpawnTarget::Single {
            repo_id,
            branch_name,
            base_branch,
            worktree_reuse,
            ..
        } => {
            let [only] = members else {
                return None;
            };
            let registered_repo = registered.repos.iter().any(|r| r.id == *repo_id);
            (registered_repo && only.repo_id == *repo_id).then(|| SpawnTarget::Single {
                repo_id: repo_id.clone(),
                branch_name: branch_name.clone(),
                base_branch: base_branch.clone(),
                use_worktree: true,
                checkout_strategy: None,
                worktree_reuse: *worktree_reuse,
                existing_worktree: Some(only.worktree_path.clone()),
            })
        }
        SpawnTarget::Workspace {
            workspace_id,
            branch_name,
            base_branch,
            worktree_reuse,
            ..
        } => {
            let ws = registered
                .workspaces
                .iter()
                .find(|w| &w.id == workspace_id)?;
            Some(SpawnTarget::Workspace {
                workspace_id: workspace_id.clone(),
                branch_name: branch_name.clone(),
                base_branch: base_branch.clone(),
                use_worktree: true,
                worktree_reuse: *worktree_reuse,
                existing_worktrees: workspace_pins(ws, members, registered.repos)?,
            })
        }
        SpawnTarget::Standalone { .. } => Some(target.clone()),
    }
}

/// Where a session with no spawn config goes back to: the registered
/// workspace its members are, else the registered repo its one member is,
/// each pinned to the recorded folders; else the first folder with the
/// others as `--add-dir`s.
fn folder_target(
    entry: &HistoryEntry,
    members: &[SessionMember],
    registered: &Registered<'_>,
) -> SpawnTarget {
    if let Some(target) = workspace_target(entry.workspace_id.as_deref(), members, registered) {
        return target;
    }
    if let [only] = members
        && let Some(repo) = member_repo(only, registered.repos)
    {
        return pinned_single(
            repo.id.clone(),
            &only.worktree_path,
            recorded_branch(members),
        );
    }
    let (cwd, add_dirs) = match members.split_first() {
        Some((first, rest)) => (
            first.worktree_path.clone(),
            rest.iter().map(|m| m.worktree_path.clone()).collect(),
        ),
        None => (String::new(), Vec::new()),
    };
    SpawnTarget::Standalone {
        cwd: Some(cwd),
        add_dirs,
    }
}

/// The registered workspace `members` ran as, named by `workspace_id` or
/// matched from their folders, with every member pinned.
fn workspace_target(
    workspace_id: Option<&str>,
    members: &[SessionMember],
    registered: &Registered<'_>,
) -> Option<SpawnTarget> {
    let (first, rest) = members.split_first()?;
    let id = if let Some(id) = workspace_id {
        id.to_owned()
    } else {
        let add_dirs: Vec<String> = rest.iter().map(|m| m.worktree_path.clone()).collect();
        let (SessionKind::Workspace, Some(id)) =
            match_target(&first.worktree_path, &add_dirs, registered)
        else {
            return None;
        };
        id
    };
    let ws = registered.workspaces.iter().find(|w| w.id == id)?;
    Some(SpawnTarget::Workspace {
        workspace_id: ws.id.clone(),
        branch_name: recorded_branch(members),
        base_branch: None,
        use_worktree: true,
        worktree_reuse: WorktreeReusePolicy::Reuse,
        existing_worktrees: workspace_pins(ws, members, registered.repos)?,
    })
}

/// One pin per member of `ws`, at the folder the matching recorded member ran
/// in. `None` unless every workspace member has exactly one recorded member
/// and the session's first folder is the workspace's first member, where a
/// workspace session runs: an unpinned member would get a new worktree.
fn workspace_pins(
    ws: &WorkspaceEntry,
    members: &[SessionMember],
    repos: &[RepoEntry],
) -> Option<Vec<PinnedMemberWorktree>> {
    if ws.member_repo_ids.len() != members.len() {
        return None;
    }
    let mut pins = Vec::with_capacity(members.len());
    for (i, repo_id) in ws.member_repo_ids.iter().enumerate() {
        let at = members
            .iter()
            .position(|m| member_repo(m, repos).is_some_and(|r| &r.id == repo_id))?;
        if (i == 0) != (at == 0) {
            return None;
        }
        pins.push(PinnedMemberWorktree {
            repo_id: repo_id.clone(),
            path: members[at].worktree_path.clone(),
        });
    }
    Some(pins)
}

/// The registered repo a member ran in: the one it recorded, else the one
/// at its folder.
fn member_repo<'a>(member: &SessionMember, repos: &'a [RepoEntry]) -> Option<&'a RepoEntry> {
    repos
        .iter()
        .find(|r| !member.repo_id.is_empty() && r.id == member.repo_id)
        .or_else(|| repo_at(repos, &member.worktree_path))
}

/// The branch the first member recorded, else [`PINNED_BRANCH_FALLBACK`].
/// A pinned spawn records the branch the folder has checked out; this name
/// only labels a workspace session and stands in for a detached HEAD.
fn recorded_branch(members: &[SessionMember]) -> String {
    members
        .first()
        .map(|m| m.branch.as_str())
        .filter(|b| !b.is_empty())
        .unwrap_or(PINNED_BRANCH_FALLBACK)
        .to_owned()
}

/// A Single target on `repo_id` that runs in `path` on whatever it has
/// checked out. The pin needs `use_worktree`, and it skips both the checkout
/// and the worktree creation.
fn pinned_single(repo_id: String, path: &str, branch_name: String) -> SpawnTarget {
    SpawnTarget::Single {
        repo_id,
        branch_name,
        base_branch: None,
        use_worktree: true,
        checkout_strategy: None,
        worktree_reuse: WorktreeReusePolicy::Reuse,
        existing_worktree: Some(path.to_owned()),
    }
}

/// The agent resuming `conversation` in `target` with `spawn`'s options and
/// the env rows `entry`'s spawn recorded.
fn folder_agent(
    entry: &HistoryEntry,
    target: SpawnTarget,
    conversation: &str,
    spawn: RecoveredSpawn,
) -> SpawnRequest {
    SpawnRequest {
        label: None,
        target,
        mode: SessionMode::Interactive,
        initial_prompt: None,
        dangerously_skip_permissions: spawn.dangerously_skip_permissions,
        agent: Some(spawn.selection),
        extra_env: recorded_env(entry).to_vec(),
        secret_env_keys: Vec::new(),
        prompt_injector: None,
        request_id: None,
        resume_conversation: Some(conversation.to_owned()),
    }
}

/// A plain shell in `folder` carrying the env rows `entry` recorded. The
/// resume command a chosen conversation adds is the spawn's to build, from
/// the plan's hook agent and conversation.
fn shell_request(entry: &HistoryEntry, folder: &str) -> SpawnRequest {
    SpawnRequest {
        label: None,
        target: SpawnTarget::Standalone {
            cwd: (!folder.is_empty()).then(|| folder.to_owned()),
            add_dirs: Vec::new(),
        },
        mode: SessionMode::PlainShell,
        initial_prompt: None,
        dangerously_skip_permissions: false,
        agent: None,
        extra_env: recorded_env(entry).to_vec(),
        secret_env_keys: Vec::new(),
        prompt_injector: None,
        request_id: None,
        resume_conversation: None,
    }
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
    use crate::pty::{PtyExit, PtyHandle, PtyHandleParts, PtyProducer};
    use crate::session::{SessionRecord, SessionRegistry, attach_lifecycle};
    use chrono::Utc;
    use portable_pty::ChildKiller;
    use protocol::{
        AppearanceOverrides, HistoryEntry, SessionKind, SessionMetrics, SessionMode, SessionStatus,
    };
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;

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

    /// The tracer reader's side of a [`fake_pty`]: it owns the session's
    /// producer, sends its output and reports how the session ended. Dropping
    /// it without an exit is a reader gone without one, a lost tracer.
    pub struct FakeExit {
        producer: PtyProducer,
    }

    impl FakeExit {
        /// Reports `exit` without waiting for it to be queued, the way a
        /// oneshot send does: [`PtyProducer::finish`] runs on its own task.
        /// Gives the exit back when there is no runtime to run it on.
        pub fn send(self, exit: PtyExit) -> Result<(), PtyExit> {
            let Ok(runtime) = tokio::runtime::Handle::try_current() else {
                return Err(exit);
            };
            runtime.spawn(self.producer.finish(exit));
            Ok(())
        }

        /// Sends one output chunk, as the tracer reader does.
        pub async fn chunk(&self, bytes: Vec<u8>) {
            self.producer.send(bytes).await;
        }

        /// Queues `exit` behind every chunk sent before it.
        pub async fn finish(self, exit: PtyExit) {
            self.producer.finish(exit).await;
        }
    }

    /// A PTY handle with no process behind it: kills do nothing, and the
    /// returned [`FakeExit`] plays the tracer reporting how the session ended.
    pub fn fake_pty() -> (Arc<PtyHandle>, FakeExit) {
        let (producer, lifecycle_output) = PtyProducer::channel(16);
        let (input_tx, _) = mpsc::unbounded_channel();
        let (resize_tx, _) = mpsc::unbounded_channel();
        let handle = PtyHandle::from_parts(PtyHandleParts {
            output: producer.output(),
            lifecycle_output,
            input_tx,
            resize_tx,
            killer: Box::new(NoopKiller),
            pid: None,
        });
        (Arc::new(handle), FakeExit { producer })
    }

    /// A [`fake_pty`] that keeps what is written to it: every
    /// [`PtyHandle::write_input`] arrives on the returned receiver.
    pub fn recording_pty() -> (Arc<PtyHandle>, mpsc::UnboundedReceiver<Vec<u8>>, FakeExit) {
        let (producer, lifecycle_output) = PtyProducer::channel(16);
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (resize_tx, _) = mpsc::unbounded_channel();
        let handle = PtyHandle::from_parts(PtyHandleParts {
            output: producer.output(),
            lifecycle_output,
            input_tx,
            resize_tx,
            killer: Box::new(NoopKiller),
            pid: None,
        });
        (Arc::new(handle), input_rx, FakeExit { producer })
    }

    /// Hold the history write lock until the returned guard drops. While a
    /// test holds it, an end path that comes after the lock is parked, so the
    /// test decides which of two competing writes lands first.
    pub fn hold_history_write_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::sync::lock(&super::WRITE_LOCK)
    }

    /// How many history writers have reached the write lock so far; read it
    /// before an end path starts, then [`wait_for_writer_at_lock`] past it.
    pub fn writers_at_lock() -> usize {
        super::WRITERS_AT_LOCK.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Poll until more than `seen` history writers have reached the write
    /// lock, failing as hung when none has after [`HANG_GUARD`].
    pub async fn wait_for_writer_at_lock(seen: usize) {
        tokio::time::timeout(HANG_GUARD, async {
            while writers_at_lock() <= seen {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("hung: no history writer reached the write lock"));
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
            remote_config_file: root.join("remote.json"),
            sessions_dir: root.join("sessions"),
            worktrees_dir: root.join("worktrees"),
            binaries_dir: root.join("binaries"),
            bin_templates: None,
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
            status_since: Some(Utc::now()),
            status_seq: 0,
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
            summary: None,
            busy_excluded: false,
            agent_in_shell: false,
            shell_label: None,
            agent_exit_announced: false,
            applied_size: None,
            work: crate::background_work::BackgroundWork::default(),
            hook_agent: None,
            hook_options: protocol::OptionValues::new(),
            hook_trusted: false,
        }
    }

    /// Write a minimal `meta.json` sidecar for `id`, as a spawn does.
    pub fn write_meta_for(dirs: &Dirs, id: &str) {
        let meta = crate::orphan::meta_from_record(
            id.to_string(),
            1,
            id.to_string(),
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
        crate::orphan::write_meta(dirs, &meta).expect("write meta");
    }

    /// The bound on these waits: a hang guard a passing run never comes
    /// near, failing as "hung".
    const HANG_GUARD: Duration = Duration::from_secs(120);

    /// Poll until `id`'s sidecar is gone, an exit watcher's last step,
    /// failing as hung when it is still on disk after [`HANG_GUARD`].
    pub async fn sidecar_gone(dirs: &Dirs, id: &str) {
        tokio::time::timeout(HANG_GUARD, async {
            while crate::orphan::load_meta(dirs, id).is_ok() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("hung: {id}'s sidecar is still on disk"));
    }

    /// Poll until `session_id`'s history entry appears, failing as hung when
    /// it has not after [`HANG_GUARD`].
    pub async fn wait_for_entry(dirs: &Dirs, session_id: &str) -> HistoryEntry {
        tokio::time::timeout(HANG_GUARD, async {
            loop {
                if let Some(entry) = super::read_all(dirs)
                    .into_iter()
                    .find(|e| e.session_id == session_id)
                {
                    return entry;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("hung: no history entry for {session_id}"))
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod tests {
    use super::import_tests::{PWSH_LOG, STOP_LOG, YAAT_LOG, set_mtime, ts, write_log};
    use super::test_support::{record, scratch_dirs, write_meta_for};
    use super::*;
    use crate::session::SessionRegistry;

    fn entry(id: &str, end: SessionEnd, ended_at: DateTime<Utc>) -> HistoryEntry {
        entry_from_record(&record(id, SessionMode::Interactive), end, ended_at)
    }

    /// A sidecar that names no agent ends as an entry that names none.
    #[test]
    fn history_entry_from_meta_without_an_agent_names_none() {
        let mut meta = crate::orphan::meta_from_record(
            "s-no-agent".to_string(),
            1,
            "s-no-agent".to_string(),
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
        meta.agent = None;
        let entry = entry_from_meta(&meta, SessionEnd::TracerLost, Utc::now());
        assert_eq!(entry.agent, None);
    }

    /// The hook path moves the record to a `/clear`'s conversation, and the
    /// entry written at the end carries it.
    #[test]
    fn entry_from_record_carries_the_hook_updated_conversation_id() {
        let mut rec = record("s1", SessionMode::Interactive);
        rec.conversation_id = Some("old-conv".to_owned());
        let body = serde_json::json!({
            "hook_event_name": "SessionStart",
            "source": "clear",
            "session_id": "new-conv",
        });
        let report = agent_registry::plugin(&agent_registry::default_agent())
            .expect("Claude is registered")
            .map_hook_event(&body, None, false)
            .expect("a start is mapped");
        crate::hook_status::apply_details(&mut rec, &report, 1_700_000_000_000_000_000);
        assert_eq!(
            rec.conversation_id.as_deref(),
            Some("new-conv"),
            "the hook path moves the record"
        );
        let entry = entry_from_record(&rec, SessionEnd::TracerLost, Utc::now());
        assert_eq!(
            entry.conversation_id.as_deref(),
            Some("new-conv"),
            "the ended session keeps the conversation it really was in"
        );
    }

    #[test]
    fn entry_from_record_copies_busy_excluded() {
        let mut rec = record("s1", SessionMode::Interactive);
        assert!(!entry_from_record(&rec, SessionEnd::TracerLost, Utc::now()).busy_excluded);
        rec.busy_excluded = true;
        let entry = entry_from_record(&rec, SessionEnd::TracerLost, Utc::now());
        assert!(entry.busy_excluded, "a recovered session keeps the flag");
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

    #[test]
    fn read_one_finds_an_entry_and_refuses_path_ids() {
        let dirs = scratch_dirs("read-one");
        write_if_absent(&dirs, &entry("s1", SessionEnd::TracerLost, Utc::now())).expect("write");
        assert_eq!(
            read_one(&dirs, "s1").map(|e| e.session_id).as_deref(),
            Some("s1")
        );
        assert_eq!(read_one(&dirs, "missing"), None);
        assert_eq!(read_one(&dirs, r"..\history\s1"), None);
        assert_eq!(read_one(&dirs, ""), None);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// A dead sidecar for `id` on disk, carrying a conversation id.
    fn dead_meta(dirs: &Dirs, id: &str) -> orphan::OrphanMeta {
        write_meta_for(dirs, id);
        let mut meta = orphan::load_meta(dirs, id).expect("load meta");
        meta.conversation_id = Some(format!("conv-{id}"));
        meta
    }

    /// The start every lost-at-start test runs at: a day after its logs.
    fn now() -> DateTime<Utc> {
        ts("2026-09-28T00:00:00Z")
    }

    fn entry_bytes(dirs: &Dirs, id: &str) -> Vec<u8> {
        std::fs::read(dirs.history_dir().join(format!("{id}.json"))).expect("read entry")
    }

    #[test]
    fn lost_at_start_writes_tracer_lost_without_entry() {
        let dirs = scratch_dirs("lost-writes");
        let meta = dead_meta(&dirs, "lost");
        write_log(&dirs, "tracer-lost.log", &YAAT_LOG);
        let ids = record_lost_at_start(&dirs, &[meta], now());
        assert_eq!(ids, vec!["lost".to_owned()]);
        let written = read_one(&dirs, "lost").expect("entry written");
        assert_eq!(written.end, SessionEnd::TracerLost);
        assert!(!written.end_time_known, "the time of a lost end is a guess");
        assert_eq!(written.conversation_id.as_deref(), Some("conv-lost"));
        assert_eq!(written.source, HistorySource::Record);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn lost_at_start_keeps_existing_entry() {
        let dirs = scratch_dirs("lost-keeps");
        let meta = dead_meta(&dirs, "kept");
        write_if_absent(&dirs, &entry("kept", SessionEnd::StoppedByUser, Utc::now()))
            .expect("pre-write");
        let before = entry_bytes(&dirs, "kept");
        let ids = record_lost_at_start(&dirs, &[meta], now());
        assert!(ids.is_empty(), "a session with an entry is not offered");
        assert_eq!(entry_bytes(&dirs, "kept"), before);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn lost_at_start_skips_headless() {
        let dirs = scratch_dirs("lost-headless");
        let mut meta = dead_meta(&dirs, "headless");
        meta.mode = SessionMode::Headless;
        let ids = record_lost_at_start(&dirs, &[meta], now());
        assert_eq!(ids, Vec::<String>::new());
        assert_eq!(read_one(&dirs, "headless"), None);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn lost_at_start_end_time_from_tracer_log_else_sidecar() {
        let dirs = scratch_dirs("lost-end-time");
        let logged = dead_meta(&dirs, "logged");
        write_log(&dirs, "tracer-logged.log", &YAAT_LOG);
        let unlogged = dead_meta(&dirs, "unlogged");
        let sidecar_time = ts("2026-09-27T12:00:00Z");
        set_mtime(
            &dirs.sessions_dir.join("unlogged").join("meta.json"),
            sidecar_time,
        );
        let ids = record_lost_at_start(&dirs, &[logged, unlogged], now());
        assert_eq!(ids, vec!["logged".to_owned(), "unlogged".to_owned()]);
        let from_log = read_one(&dirs, "logged").expect("logged entry");
        assert_eq!(from_log.ended_at, ts("2026-09-27T11:09:41.982317Z"));
        assert!(!from_log.end_time_known);
        let from_sidecar = read_one(&dirs, "unlogged").expect("unlogged entry");
        assert_eq!(from_sidecar.ended_at, sidecar_time);
        assert!(!from_sidecar.end_time_known);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn lost_at_start_records_exited_or_stopped_from_tracer_log() {
        let dirs = scratch_dirs("lost-true-end");
        let exited = dead_meta(&dirs, "exited");
        write_log(&dirs, "tracer-exited.log", &PWSH_LOG);
        let stopped = dead_meta(&dirs, "stopped");
        write_log(&dirs, "tracer-stopped.log", &STOP_LOG);
        let ids = record_lost_at_start(&dirs, &[exited, stopped], now());
        assert!(
            ids.is_empty(),
            "a session that ended on its own is not offered"
        );
        let exited = read_one(&dirs, "exited").expect("exited entry");
        assert_eq!(exited.end, SessionEnd::Exited { code: 0 });
        assert_eq!(exited.ended_at, ts("2026-09-27T08:05:00Z"));
        assert!(exited.end_time_known);
        let stopped = read_one(&dirs, "stopped").expect("stopped entry");
        assert_eq!(stopped.end, SessionEnd::StoppedByUser);
        assert_eq!(stopped.ended_at, ts("2026-09-26T05:09:44.845855Z"));
        assert!(stopped.end_time_known);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn lost_at_start_twice_writes_once() {
        let dirs = scratch_dirs("lost-twice");
        let meta = dead_meta(&dirs, "twice");
        let first = record_lost_at_start(&dirs, std::slice::from_ref(&meta), now());
        assert_eq!(first, vec!["twice".to_owned()]);
        let before = entry_bytes(&dirs, "twice");
        let second = record_lost_at_start(&dirs, &[meta], now());
        assert!(second.is_empty(), "a second start offers nothing");
        assert_eq!(entry_bytes(&dirs, "twice"), before);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn lost_at_start_end_time_is_the_later_of_status_since_and_log() {
        let dirs = scratch_dirs("lost-later-time");
        let mut status_later = dead_meta(&dirs, "status-later");
        status_later.status_since = Some(ts("2026-09-27T15:00:00Z"));
        write_log(&dirs, "tracer-status-later.log", &YAAT_LOG);
        let mut log_later = dead_meta(&dirs, "log-later");
        log_later.status_since = Some(ts("2026-09-27T09:00:00Z"));
        write_log(&dirs, "tracer-log-later.log", &YAAT_LOG);
        let ids = record_lost_at_start(&dirs, &[status_later, log_later], now());
        assert_eq!(ids, vec!["status-later".to_owned(), "log-later".to_owned()]);
        let from_status = read_one(&dirs, "status-later").expect("status-later entry");
        assert_eq!(from_status.ended_at, ts("2026-09-27T15:00:00Z"));
        let from_log = read_one(&dirs, "log-later").expect("log-later entry");
        assert_eq!(from_log.ended_at, ts("2026-09-27T11:09:41.982317Z"));
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn lost_at_start_skips_a_loss_older_than_retention() {
        let dirs = scratch_dirs("lost-too-old");
        let meta = dead_meta(&dirs, "old");
        write_log(&dirs, "tracer-old.log", &YAAT_LOG);
        let later = ts("2026-09-27T11:09:41.982317Z") + HISTORY_RETENTION + TimeDelta::minutes(1);
        let ids = record_lost_at_start(&dirs, &[meta], later);
        assert_eq!(ids, Vec::<String>::new());
        assert_eq!(read_one(&dirs, "old"), None);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn lost_at_start_counts_a_shutdown_exit_as_lost() {
        let dirs = scratch_dirs("lost-shutdown-exit");
        let meta = dead_meta(&dirs, "shutdown");
        write_log(
            &dirs,
            "tracer-shutdown.log",
            &[
                YAAT_LOG[0],
                YAAT_LOG[1],
                r"2026-09-27T11:10:00.000000Z  INFO rt_tracer::supervisor: supervisor: child exited; shutting down exit_code=-1073741510",
            ],
        );
        let ids = record_lost_at_start(&dirs, &[meta], now());
        assert_eq!(ids, vec!["shutdown".to_owned()]);
        let written = read_one(&dirs, "shutdown").expect("shutdown entry");
        assert_eq!(written.end, SessionEnd::TracerLost);
        assert!(!written.end_time_known);
        assert_eq!(written.ended_at, ts("2026-09-27T11:10:00Z"));
        let _ = std::fs::remove_dir_all(&dirs.config);
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod import_tests {
    use super::test_support::scratch_dirs;
    use super::*;
    use protocol::OptionValue;
    use serde_json::json;
    use std::fs::File;
    use std::time::SystemTime;

    const YAAT_ID: &str = "f6216fc2-54b1-497c-a257-7c297f7d9859";
    pub(super) const YAAT_LOG: [&str; 3] = [
        r"2026-09-27T11:09:41.935397Z  INFO rt_tracer: rt-tracer starting session_id=f6216fc2-54b1-497c-a257-7c297f7d9859 cwd=D:\yaat cols=120 rows=32 argc=6",
        r#"2026-09-27T11:09:41.940609Z  INFO rt_tracer::supervisor: supervisor: about to spawn child program=C:\Users\lefto\.local\bin\claude.exe argc=5 args=["--add-dir", "D:\\yaat-server", "--append-system-prompt", "Workspace member paths for this session:\n  yaat         ->  D:\\yaat\n  yaat-server  ->  D:\\yaat-server\n", "--dangerously-skip-permissions"] cwd=D:\yaat"#,
        r"2026-09-27T11:09:41.982317Z  INFO rt_tracer::supervisor: supervisor: client connected iteration=1 session_id=f6216fc2-54b1-497c-a257-7c297f7d9859",
    ];
    const STOP_ID: &str = "22b5a96d-8d81-40e3-a594-498fc639c7cc";
    pub(super) const STOP_LOG: [&str; 3] = [
        r"2026-09-26T03:53:21.116126Z  INFO rt_tracer: rt-tracer starting session_id=22b5a96d-8d81-40e3-a594-498fc639c7cc cwd=D:\rustling-tulip cols=120 rows=32 argc=2",
        r#"2026-09-26T03:53:21.120000Z  INFO rt_tracer::supervisor: supervisor: about to spawn child program=C:\Users\lefto\.local\bin\claude.exe argc=1 args=["--dangerously-skip-permissions"] cwd=D:\rustling-tulip"#,
        r"2026-09-26T05:09:44.845855Z  INFO rt_tracer::supervisor: supervisor: Stop request received",
    ];
    const PWSH_ID: &str = "0b8f4c1e-1111-4222-8333-944455556666";
    pub(super) const PWSH_LOG: [&str; 3] = [
        r"2026-09-27T08:00:00.000000Z  INFO rt_tracer: rt-tracer starting session_id=0b8f4c1e-1111-4222-8333-944455556666 cwd=D:\ cols=120 rows=32 argc=4",
        r#"2026-09-27T08:00:00.010000Z  INFO rt_tracer::supervisor: supervisor: about to spawn child program=pwsh.exe argc=3 args=["-NoExit", "-Command", "$global:__rt_original_prompt = $function:prompt; function global:prompt { \"PS $($PWD.Path)> \" }"] cwd=D:\"#,
        r"2026-09-27T08:05:00.000000Z  INFO rt_tracer::supervisor: supervisor: child exited; shutting down exit_code=0",
    ];

    pub(super) fn ts(stamp: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(stamp)
            .expect("valid timestamp")
            .with_timezone(&Utc)
    }

    fn repo(id: &str, path: &str) -> RepoEntry {
        serde_json::from_value(json!({"id": id, "name": id, "path": path, "default_branch": null}))
            .expect("repo fixture")
    }

    fn workspace(id: &str, members: &[&str]) -> WorkspaceEntry {
        serde_json::from_value(json!({"id": id, "name": id, "member_repo_ids": members}))
            .expect("workspace fixture")
    }

    pub(super) fn write_log(dirs: &Dirs, name: &str, lines: &[&str]) -> PathBuf {
        let logs = dirs.config.join("logs");
        std::fs::create_dir_all(&logs).expect("create logs dir");
        let path = logs.join(name);
        std::fs::write(&path, lines.join("\n")).expect("write log");
        path
    }

    pub(super) fn set_mtime(path: &Path, at: DateTime<Utc>) {
        File::options()
            .write(true)
            .open(path)
            .expect("reopen log")
            .set_modified(SystemTime::from(at))
            .expect("set log mtime");
    }

    fn import(dirs: &Dirs, repos: &[RepoEntry], workspaces: &[WorkspaceEntry]) -> usize {
        import_skipping(dirs, repos, workspaces, &HashSet::new())
    }

    fn import_skipping(
        dirs: &Dirs,
        repos: &[RepoEntry],
        workspaces: &[WorkspaceEntry],
        skip: &HashSet<String>,
    ) -> usize {
        let registered = Registered { repos, workspaces };
        import_tracer_logs(dirs, &registered, skip, Utc::now())
    }

    fn only(dirs: &Dirs, id: &str) -> HistoryEntry {
        read_one(dirs, id).expect("imported entry")
    }

    fn member_ids(entry: &HistoryEntry) -> Vec<(&str, &str)> {
        entry
            .members
            .iter()
            .map(|m| (m.repo_id.as_str(), m.worktree_path.as_str()))
            .collect()
    }

    #[test]
    fn workspace_log_matches_the_registered_workspace() {
        let dirs = scratch_dirs("import-ws");
        write_log(&dirs, &format!("tracer-{YAAT_ID}.log"), &YAAT_LOG);
        let repos = [
            repo("r-yaat", r"D:\yaat"),
            repo("r-srv", r"d:/Yaat-Server/"),
        ];
        let workspaces = [workspace("ws1", &["r-yaat", "r-srv"])];

        assert_eq!(import(&dirs, &repos, &workspaces), 1);

        let entry = only(&dirs, YAAT_ID);
        assert_eq!(entry.kind, SessionKind::Workspace);
        assert_eq!(entry.workspace_id.as_deref(), Some("ws1"));
        assert_eq!(
            entry.spawn_config, None,
            "no in-place target avoids a checkout"
        );
        assert_eq!(
            member_ids(&entry),
            [("r-yaat", r"D:\yaat"), ("r-srv", r"D:\yaat-server")]
        );
        assert_eq!(entry.mode, SessionMode::Interactive);
        assert_eq!(entry.agent, Some(agent_registry::default_agent()));
        assert_eq!(entry.program_name.as_deref(), Some("claude"));
        assert_eq!(entry.primary_cwd.as_deref(), Some(r"D:\yaat"));
        assert_eq!(entry.current_cwd, None);
        assert_eq!(entry.started_at, Some(ts("2026-09-27T11:09:41.935397Z")));
        assert_eq!(entry.end, SessionEnd::TracerLost);
        assert_eq!(entry.source, HistorySource::TracerLog);
        assert_eq!(entry.label, "");
        assert_eq!(entry.conversation_id, None);
        assert!(entry.trusted);
        assert_eq!(option_of(&entry, "model"), Some(&OptionValue::Null));
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// The revision Claude's importer stamps an import with.
    fn claude_import_rev() -> u32 {
        agent_registry::plugin(&agent_registry::default_agent())
            .expect("Claude is registered")
            .import_rev()
    }

    /// The value an import recorded for its agent's option `key`.
    fn option_of<'a>(entry: &'a HistoryEntry, key: &str) -> Option<&'a OptionValue> {
        entry.options.as_ref()?.get(key)
    }

    fn text(value: &str) -> OptionValue {
        OptionValue::Text(value.to_owned())
    }

    fn claude_log(id: &str, args: &str) -> [String; 2] {
        [
            format!(
                "2026-09-27T08:00:00.000000Z  INFO rt_tracer: rt-tracer starting session_id={id} cwd=D:\\proj cols=120 rows=32 argc=3"
            ),
            format!(
                r"2026-09-27T08:00:00.010000Z  INFO rt_tracer::supervisor: supervisor: about to spawn child program=C:\bin\claude.exe argc=2 args={args} cwd=D:\proj"
            ),
        ]
    }

    #[test]
    fn importer_reads_the_permission_and_model_flags() {
        let dirs = scratch_dirs("import-flags");
        let cases = [
            (
                "aaaaaaaa-0000-4000-8000-000000000001",
                r#"["--model", "opus"]"#,
                false,
                text("opus"),
            ),
            (
                "aaaaaaaa-0000-4000-8000-000000000002",
                r#"["--model=sonnet", "--dangerously-skip-permissions"]"#,
                true,
                text("sonnet"),
            ),
            (
                "aaaaaaaa-0000-4000-8000-000000000003",
                "[]",
                false,
                OptionValue::Null,
            ),
        ];
        for (id, args, _, _) in &cases {
            let lines = claude_log(id, args);
            let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
            write_log(&dirs, &format!("tracer-{id}.log"), &lines);
        }

        assert_eq!(import(&dirs, &[], &[]), 3);

        for (id, _, skip, model) in cases {
            let entry = only(&dirs, id);
            assert_eq!(entry.trusted, skip, "{id}");
            assert_eq!(option_of(&entry, "model"), Some(&model), "{id}");
        }
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn workspace_log_without_a_workspace_is_folder_only() {
        let dirs = scratch_dirs("import-folder");
        write_log(&dirs, &format!("tracer-{YAAT_ID}.log"), &YAAT_LOG);

        assert_eq!(import(&dirs, &[], &[]), 1);

        let entry = only(&dirs, YAAT_ID);
        assert_eq!(entry.kind, SessionKind::Standalone);
        assert_eq!(entry.workspace_id, None);
        assert_eq!(entry.spawn_config, None);
        assert_eq!(
            member_ids(&entry),
            [("", r"D:\yaat"), ("", r"D:\yaat-server")]
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn workspace_whose_members_differ_from_the_add_dirs_is_not_matched() {
        let dirs = scratch_dirs("import-ws-mismatch");
        write_log(&dirs, &format!("tracer-{YAAT_ID}.log"), &YAAT_LOG);
        let repos = [repo("r-yaat", r"D:\yaat"), repo("r-other", r"D:\other")];
        let workspaces = [workspace("ws1", &["r-yaat", "r-other"])];

        import(&dirs, &repos, &workspaces);

        let entry = only(&dirs, YAAT_ID);
        assert_eq!(entry.kind, SessionKind::Standalone);
        assert_eq!(entry.workspace_id, None);
        assert_eq!(
            member_ids(&entry),
            [("r-yaat", r"D:\yaat"), ("", r"D:\yaat-server")]
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn single_repo_log_is_matched_to_its_repo() {
        let dirs = scratch_dirs("import-single");
        write_log(&dirs, &format!("tracer-{STOP_ID}.log"), &STOP_LOG);
        let repos = [repo("r-rt", r"D:\rustling-tulip")];

        import(&dirs, &repos, &[]);

        let entry = only(&dirs, STOP_ID);
        assert_eq!(entry.kind, SessionKind::Single);
        assert_eq!(entry.spawn_config, None);
        assert_eq!(member_ids(&entry), [("r-rt", r"D:\rustling-tulip")]);
        assert_eq!(entry.end, SessionEnd::StoppedByUser);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn pwsh_log_is_a_folder_only_plain_shell() {
        let dirs = scratch_dirs("import-pwsh");
        write_log(&dirs, &format!("tracer-{PWSH_ID}.log"), &PWSH_LOG);

        import(&dirs, &[repo("r-root", r"D:\")], &[]);

        let entry = only(&dirs, PWSH_ID);
        assert_eq!(entry.mode, SessionMode::PlainShell);
        assert_eq!(entry.kind, SessionKind::Standalone);
        assert_eq!(entry.program_name.as_deref(), Some("pwsh"));
        assert_eq!(entry.current_cwd.as_deref(), Some(r"D:\"));
        assert_eq!(entry.primary_cwd.as_deref(), Some(r"D:\"));
        assert_eq!(entry.members, [] as [protocol::SessionMember; 0]);
        assert_eq!(entry.spawn_config, None);
        assert_eq!(entry.end, SessionEnd::Exited { code: 0 });
        assert!(!entry.trusted);
        assert_eq!(entry.options, None);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn an_import_is_stamped_with_the_plugins_import_rev() {
        let dirs = scratch_dirs("import-plugin-rev");
        write_log(&dirs, &format!("tracer-{YAAT_ID}.log"), &YAAT_LOG);

        assert_eq!(import(&dirs, &[], &[]), 1);

        let entry = only(&dirs, YAAT_ID);
        assert_eq!(entry.agent, Some(agent_registry::default_agent()));
        assert_eq!(entry.import_rev, claude_import_rev());
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn a_shell_import_names_no_agent() {
        let dirs = scratch_dirs("import-shell-agent");
        write_log(&dirs, &format!("tracer-{PWSH_ID}.log"), &PWSH_LOG);

        assert_eq!(import(&dirs, &[], &[]), 1);

        let entry = only(&dirs, PWSH_ID);
        assert_eq!(entry.mode, SessionMode::PlainShell);
        assert_eq!(entry.agent, None);
        assert_eq!(entry.import_rev, SHELL_IMPORT_REV);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn shell_import_rev_is_3() {
        assert_eq!(SHELL_IMPORT_REV, 3);
    }

    const SHELL_HOSTED_CONV: &str = "85573bb1-c581-489e-baaa-94d5a384744c";

    /// The tracer log of a pwsh in `D:\yaat` that Claude was typed into, with
    /// `note` on its agent note line, or no such line.
    fn shell_hosted_log(id: &str, note: Option<&serde_json::Value>) -> Vec<String> {
        let mut lines = vec![format!(
            r"2026-09-27T08:00:00.000000Z  INFO rt_tracer: rt-tracer starting session_id={id} cwd=D:\yaat cols=120 rows=32 argc=4"
        )];
        if let Some(note) = note {
            lines.push(format!(
                "2026-09-27T08:00:00.005000Z  INFO rt_tracer: rt-tracer agent note agent_note={note}"
            ));
        }
        lines.push(
            r#"2026-09-27T08:00:00.010000Z  INFO rt_tracer::supervisor: supervisor: about to spawn child program=pwsh.exe argc=1 args=["-NoExit"] cwd=D:\yaat"#
                .to_owned(),
        );
        lines
    }

    fn import_shell_hosted(dirs: &Dirs, id: &str, note: Option<&serde_json::Value>) {
        let lines = shell_hosted_log(id, note);
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        write_log(dirs, &format!("tracer-{id}.log"), &lines);
        let repos = [repo("r-yaat", r"D:\yaat"), repo("r-srv", r"D:\yaat-server")];
        let workspaces = [workspace("ws1", &["r-yaat", "r-srv"])];
        assert_eq!(import(dirs, &repos, &workspaces), 1);
    }

    fn deepseek_options() -> OptionValues {
        OptionValues::from([
            ("model".to_owned(), text("deepseek-v4-pro")),
            ("provider".to_owned(), text("deepseek")),
            ("permission_mode".to_owned(), OptionValue::Null),
        ])
    }

    /// A trusted Claude note on the `deepseek` provider with `conversation_id` and the
    /// second workspace member as its extra folder.
    fn claude_note(conversation_id: Option<&str>) -> serde_json::Value {
        json!({
            "agent": agent_registry::default_agent(),
            "options": deepseek_options(),
            "trusted": true,
            "conversation_id": conversation_id,
            "extra_dirs": [r"D:\yaat-server"],
        })
    }

    /// Asserts `entry` is the plain pwsh entry a log without a note gives.
    fn assert_plain_shell(entry: &HistoryEntry) {
        assert_eq!(entry.mode, SessionMode::PlainShell);
        assert_eq!(entry.agent, None);
        assert_eq!(entry.kind, SessionKind::Standalone);
        assert_eq!(entry.workspace_id, None);
        assert_eq!(entry.program_name.as_deref(), Some("pwsh"));
        assert_eq!(entry.members, [] as [protocol::SessionMember; 0]);
        assert_eq!(entry.current_cwd.as_deref(), Some(r"D:\yaat"));
        assert_eq!(entry.options, None);
        assert!(!entry.trusted);
        assert_eq!(entry.conversation_id, None);
        assert_eq!(entry.import_rev, SHELL_IMPORT_REV);
    }

    #[test]
    fn a_shell_hosted_agent_log_imports_as_the_agent_with_its_options_and_conversation() {
        let dirs = scratch_dirs("import-shell-hosted-note");
        let id = "aaaaaaaa-0000-4000-8000-0000000003a1";

        import_shell_hosted(&dirs, id, Some(&claude_note(Some(SHELL_HOSTED_CONV))));

        let entry = only(&dirs, id);
        let plugin =
            agent_registry::plugin(&agent_registry::default_agent()).expect("Claude is registered");
        assert_eq!(entry.mode, SessionMode::Interactive);
        assert_eq!(entry.agent, Some(agent_registry::default_agent()));
        assert_eq!(entry.options, Some(deepseek_options()));
        assert!(entry.trusted);
        assert_eq!(entry.conversation_id.as_deref(), Some(SHELL_HOSTED_CONV));
        assert_eq!(
            entry.program_name.as_deref(),
            Some(plugin.descriptor().tag.as_str())
        );
        assert_eq!(entry.import_rev, claude_import_rev());
        assert_eq!(entry.kind, SessionKind::Workspace);
        assert_eq!(entry.workspace_id.as_deref(), Some("ws1"));
        assert_eq!(
            member_ids(&entry),
            [("r-yaat", r"D:\yaat"), ("r-srv", r"D:\yaat-server")]
        );
        assert_eq!(entry.primary_cwd.as_deref(), Some(r"D:\yaat"));
        assert_eq!(entry.current_cwd, None);
        assert_eq!(entry.spawn_config, None);
        assert_eq!(entry.source, HistorySource::TracerLog);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn a_shell_hosted_log_without_a_note_stays_a_plain_shell() {
        let dirs = scratch_dirs("import-shell-hosted-no-note");
        let id = "aaaaaaaa-0000-4000-8000-0000000003a2";

        import_shell_hosted(&dirs, id, None);

        assert_plain_shell(&only(&dirs, id));
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn a_note_with_no_conversation_id_imports_with_none() {
        let dirs = scratch_dirs("import-shell-hosted-no-conv");
        let id = "aaaaaaaa-0000-4000-8000-0000000003a3";

        import_shell_hosted(&dirs, id, Some(&claude_note(None)));

        let entry = only(&dirs, id);
        assert_eq!(entry.mode, SessionMode::Interactive);
        assert_eq!(entry.agent, Some(agent_registry::default_agent()));
        assert_eq!(entry.options, Some(deepseek_options()));
        assert_eq!(entry.conversation_id, None);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn a_note_naming_an_unregistered_agent_falls_back_to_the_shell_entry() {
        let dirs = scratch_dirs("import-shell-hosted-unknown");
        let id = "aaaaaaaa-0000-4000-8000-0000000003a4";
        let mut note = claude_note(Some(SHELL_HOSTED_CONV));
        note["agent"] = json!("no-such-agent");

        import_shell_hosted(&dirs, id, Some(&note));

        assert_plain_shell(&only(&dirs, id));
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn an_import_keeps_its_options_and_trust() {
        let dirs = scratch_dirs("import-keeps-options");
        let id = "aaaaaaaa-0000-4000-8000-0000000000b7";
        let lines = claude_log(
            id,
            r#"["--model=sonnet", "--dangerously-skip-permissions"]"#,
        );
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        write_log(&dirs, &format!("tracer-{id}.log"), &lines);

        assert_eq!(import(&dirs, &[], &[]), 1);

        let entry = only(&dirs, id);
        assert_eq!(entry.spawn_config, None, "an import stays config-less");
        assert!(entry.trusted);
        assert_eq!(option_of(&entry, "model"), Some(&text("sonnet")));
        assert_eq!(option_of(&entry, "provider"), Some(&text("anthropic")));
        let item = RecoverItem {
            history_id: id.to_owned(),
            conversation_id: Some("85573bb1-c581-489e-baaa-94d5a384744c".to_owned()),
            how: RecoverAs::Agent,
        };
        let registered = Registered {
            repos: &[],
            workspaces: &[],
        };
        let request = plan_recovery(
            &entry,
            &item,
            &registered,
            agent_registry::plugin,
            |_, _, _| true,
        )
        .expect("plan")
        .request;
        assert!(
            request.dangerously_skip_permissions,
            "the trust is replayed"
        );
        assert_eq!(
            request
                .agent
                .and_then(|selection| selection.options.get("model").cloned()),
            Some(text("sonnet")),
            "the options are replayed"
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn end_time_comes_from_the_daemon_logs_before_the_mtime() {
        let dirs = scratch_dirs("import-end-time");
        let log = write_log(&dirs, &format!("tracer-{YAAT_ID}.log"), &YAAT_LOG);
        set_mtime(&log, Utc::now() - TimeDelta::hours(1));
        write_log(
            &dirs,
            "daemon.log",
            &[&format!(
                "2026-09-27T18:40:00.000000Z  INFO rustling_tulipd::server: discard_session: begin session_id={YAAT_ID} cleanup_targets=0"
            )],
        );
        write_log(
            &dirs,
            "daemon.log.old",
            &[&format!(
                "2026-09-27T18:29:06.000000Z  INFO rustling_tulipd::tracer_client: tracer_client: child exited session_id={YAAT_ID} code=1"
            )],
        );

        import(&dirs, &[], &[]);

        let entry = only(&dirs, YAAT_ID);
        assert_eq!(entry.ended_at, ts("2026-09-27T18:29:06Z"));
        assert!(entry.end_time_known, "a daemon-log end line is a known end");
        assert_eq!(entry.import_rev, claude_import_rev());
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn daemon_end_times_reads_the_rolled_generation() {
        let dirs = scratch_dirs("daemon-end-rolled");
        write_log(
            &dirs,
            "daemon.log.1",
            &[&format!(
                "2026-09-27T18:29:06.000000Z  INFO rustling_tulipd::tracer_client: tracer_client: child exited session_id={YAAT_ID} code=1"
            )],
        );

        let ends = daemon_end_times(&dirs.config.join("logs"));

        assert_eq!(ends.get(YAAT_ID), Some(&ts("2026-09-27T18:29:06Z")));
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn end_time_falls_back_to_the_log_mtime() {
        let dirs = scratch_dirs("import-mtime");
        let log = write_log(&dirs, &format!("tracer-{YAAT_ID}.log"), &YAAT_LOG);
        let mtime = ts("2026-09-27T12:00:00Z").max(Utc::now() - TimeDelta::days(1));
        let mtime = DateTime::from_timestamp(mtime.timestamp(), 0).expect("whole seconds");
        set_mtime(&log, mtime);
        write_log(&dirs, "daemon.log", &["unrelated line"]);

        import(&dirs, &[], &[]);

        let entry = only(&dirs, YAAT_ID);
        assert_eq!(entry.ended_at, mtime);
        assert!(!entry.end_time_known, "no end line leaves the end unknown");
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// An imported entry for `id` as an importer of revision `rev` wrote it.
    fn imported(id: &str, rev: u32, label: &str) -> HistoryEntry {
        let mut e = entry_from_record(
            &super::test_support::record(id, SessionMode::Interactive),
            SessionEnd::TracerLost,
            Utc::now(),
        );
        e.source = HistorySource::TracerLog;
        e.import_rev = rev;
        e.label = label.to_owned();
        e
    }

    #[test]
    fn stale_unrecovered_import_is_replaced() {
        let dirs = scratch_dirs("reimport-stale");
        write_log(&dirs, &format!("tracer-{YAAT_ID}.log"), &YAAT_LOG);
        write_if_absent(&dirs, &imported(YAAT_ID, 0, "stale")).expect("write stale");

        assert_eq!(import(&dirs, &[], &[]), 1);

        let entry = only(&dirs, YAAT_ID);
        assert_eq!(entry.import_rev, claude_import_rev());
        assert_eq!(
            entry.label, "",
            "the importer's entry replaced the stale one"
        );
        assert!(!entry.end_time_known);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn imported_entry_with_deepseek_model_recovers_on_deepseek() {
        let dirs = scratch_dirs("import-deepseek");
        let deepseek = "aaaaaaaa-0000-4000-8000-0000000000d5";
        let anthropic = "aaaaaaaa-0000-4000-8000-0000000000a1";
        for (id, args) in [
            (deepseek, r#"["--model", "deepseek-flash"]"#),
            (anthropic, r#"["--model", "opus"]"#),
        ] {
            let lines = claude_log(id, args);
            let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
            write_log(&dirs, &format!("tracer-{id}.log"), &lines);
        }

        assert_eq!(import(&dirs, &[], &[]), 2);

        let entry = only(&dirs, deepseek);
        assert_eq!(option_of(&entry, "provider"), Some(&text("deepseek")));
        assert_eq!(
            option_of(&only(&dirs, anthropic), "provider"),
            Some(&text("anthropic"))
        );
        let item = RecoverItem {
            history_id: deepseek.to_owned(),
            conversation_id: Some("85573bb1-c581-489e-baaa-94d5a384744c".to_owned()),
            how: RecoverAs::Agent,
        };
        let registered = Registered {
            repos: &[],
            workspaces: &[],
        };
        let request = plan_recovery(
            &entry,
            &item,
            &registered,
            agent_registry::plugin,
            |_, _, _| true,
        )
        .expect("plan")
        .request;
        let options = request.agent.map(|selection| selection.options);
        assert_eq!(
            options.as_ref().and_then(|options| options.get("provider")),
            Some(&text("deepseek"))
        );
        assert_eq!(
            options.as_ref().and_then(|options| options.get("model")),
            Some(&text("deepseek-flash"))
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn recovered_record_and_current_entries_are_not_reimported() {
        let dirs = scratch_dirs("reimport-kept");
        let mut recovered = imported(YAAT_ID, 0, "recovered");
        recovered.recovered_at = Some(Utc::now());
        let recorded = {
            let mut e = entry_from_record(
                &super::test_support::record(PWSH_ID, SessionMode::Interactive),
                SessionEnd::TracerLost,
                Utc::now(),
            );
            e.label = "recorded".to_owned();
            e
        };
        let current = imported(STOP_ID, claude_import_rev(), "current");
        for (id, e) in [
            (YAAT_ID, &recovered),
            (PWSH_ID, &recorded),
            (STOP_ID, &current),
        ] {
            write_log(&dirs, &format!("tracer-{id}.log"), &YAAT_LOG);
            write_if_absent(&dirs, e).expect("write existing");
        }

        assert_eq!(import(&dirs, &[], &[]), 0);

        assert_eq!(only(&dirs, YAAT_ID).label, "recovered");
        assert_eq!(only(&dirs, PWSH_ID).label, "recorded");
        assert_eq!(only(&dirs, PWSH_ID).source, HistorySource::Record);
        assert_eq!(only(&dirs, STOP_ID).label, "current");
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn session_already_in_the_history_is_not_imported_again() {
        let dirs = scratch_dirs("import-existing");
        write_log(&dirs, &format!("tracer-{YAAT_ID}.log"), &YAAT_LOG);
        let mut recorded = entry_from_record(
            &super::test_support::record(YAAT_ID, SessionMode::Interactive),
            SessionEnd::StoppedByUser,
            Utc::now(),
        );
        recorded.label = "kept".to_owned();
        write_if_absent(&dirs, &recorded).expect("write recorded");

        assert_eq!(import(&dirs, &[], &[]), 0);

        let entry = only(&dirs, YAAT_ID);
        assert_eq!(entry.source, HistorySource::Record);
        assert_eq!(entry.label, "kept");
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn live_or_abandoned_sidecar_session_is_skipped() {
        let dirs = scratch_dirs("import-live");
        write_log(&dirs, &format!("tracer-{YAAT_ID}.log"), &YAAT_LOG);
        let skip: HashSet<String> = [YAAT_ID.to_owned()].into();

        assert_eq!(import_skipping(&dirs, &[], &[], &skip), 0);
        assert_eq!(read_one(&dirs, YAAT_ID), None);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn other_program_and_programless_logs_are_skipped() {
        let dirs = scratch_dirs("import-other-program");
        let other_id = "aaaaaaaa-1111-4222-8333-944455556666";
        write_log(
            &dirs,
            &format!("tracer-{other_id}.log"),
            &[
                &format!(
                    "2026-09-27T08:00:00.000000Z  INFO rt_tracer: rt-tracer starting session_id={other_id} cwd=D:\\yaat cols=120 rows=32 argc=1"
                ),
                r#"2026-09-27T08:00:00.010000Z  INFO rt_tracer::supervisor: supervisor: about to spawn child program=C:\bin\aider.exe argc=1 args=["--yes"] cwd=D:\yaat"#,
            ],
        );
        write_log(&dirs, &format!("tracer-{YAAT_ID}.log"), &YAAT_LOG[..1]);
        write_log(&dirs, "tracer-garbage.log", &["not a tracer log"]);

        assert_eq!(import(&dirs, &[], &[]), 0);
        assert_eq!(read_all(&dirs), [] as [protocol::HistoryEntry; 0]);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn log_older_than_the_retention_is_skipped() {
        let dirs = scratch_dirs("import-old");
        let log = write_log(&dirs, &format!("tracer-{YAAT_ID}.log"), &YAAT_LOG);
        set_mtime(&log, Utc::now() - TimeDelta::days(8));

        assert_eq!(import(&dirs, &[], &[]), 0);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn program_stems_are_read_from_any_path_form() {
        assert_eq!(
            program_stem(r"C:\Users\x\.local\bin\claude.exe").as_deref(),
            Some("claude")
        );
        assert_eq!(program_stem("/usr/bin/bash").as_deref(), Some("bash"));
        assert_eq!(program_stem("PWSH.EXE").as_deref(), Some("pwsh"));
        assert_eq!(program_stem(""), None);
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod recovery_tests {
    use super::test_support::{record, scratch_dirs};
    use super::*;
    use agent_api::AgentPlugin;
    use agent_api::test_agent::{TestAgent, TestOptions};
    use protocol::{AgentDescriptor, SpawnConfig, WorktreeReusePolicy};
    use serde_json::json;
    use std::fs::File;
    use std::sync::LazyLock;
    use std::time::SystemTime;

    const CONV: &str = "85573bb1-c581-489e-baaa-94d5a384744c";

    fn member(repo_id: &str, path: &str) -> SessionMember {
        SessionMember {
            repo_id: repo_id.to_owned(),
            repo_name: repo_id.to_owned(),
            branch: "main".to_owned(),
            worktree_path: path.to_owned(),
        }
    }

    fn folder_only(members: Vec<SessionMember>) -> HistoryEntry {
        let mut entry = entry_from_record(
            &record("h1", SessionMode::Interactive),
            SessionEnd::TracerLost,
            Utc::now(),
        );
        entry.primary_cwd = members.first().map(|m| m.worktree_path.clone());
        entry.members = members;
        entry
    }

    fn workspace_config() -> SpawnConfig {
        SpawnConfig {
            target: SpawnTarget::Workspace {
                workspace_id: "ws1".to_owned(),
                branch_name: "feat/x".to_owned(),
                base_branch: None,
                use_worktree: true,
                worktree_reuse: WorktreeReusePolicy::Reuse,
                existing_worktrees: Vec::new(),
            },
            mode: SessionMode::Interactive,
            dangerously_skip_permissions: true,
            agent: Some(claude(&[("model", "opus")])),
            extra_env: Vec::new(),
        }
    }

    /// Claude with the text options `options`, every other field at its
    /// default, as a spawn stores them.
    fn claude(options: &[(&str, &str)]) -> AgentSelection {
        AgentSelection {
            agent: agent_registry::default_agent(),
            options: crate::server::test_support::validated_claude(&raw_options(options)),
        }
    }

    /// The text options `options`, as named.
    fn raw_options(options: &[(&str, &str)]) -> OptionValues {
        options
            .iter()
            .map(|(key, value)| {
                (
                    (*key).to_owned(),
                    protocol::OptionValue::Text((*value).to_owned()),
                )
            })
            .collect()
    }

    /// The agent a request selects.
    fn selected_agent(request: &SpawnRequest) -> Option<AgentId> {
        request
            .agent
            .as_ref()
            .map(|selection| selection.agent.clone())
    }

    /// The option `key` a request selects.
    fn option(request: &SpawnRequest, key: &str) -> Option<protocol::OptionValue> {
        request.agent.as_ref()?.options.get(key).cloned()
    }

    /// The model a request selects, when it names one.
    fn model_of(request: &SpawnRequest) -> Option<&str> {
        match request.agent.as_ref()?.options.get("model")? {
            protocol::OptionValue::Text(model) => Some(model),
            _ => None,
        }
    }

    /// The env rows a recovered session's spawn recorded: a plain value and a
    /// secret reference, which stays a reference until the spawn resolves it.
    fn recorded_env_rows() -> Vec<(String, String)> {
        vec![
            ("RT_PLAIN".to_owned(), "1".to_owned()),
            (
                "EXAMPLE_API_KEY".to_owned(),
                "${secret:0123456789abcdef0123456789abcdef}".to_owned(),
            ),
        ]
    }

    fn item(how: RecoverAs, conversation: Option<&str>) -> RecoverItem {
        RecoverItem {
            history_id: "h1".to_owned(),
            conversation_id: conversation.map(str::to_owned),
            how,
        }
    }

    fn plan(entry: &HistoryEntry, item: &RecoverItem) -> Result<RecoveryPlan, String> {
        plan_in(entry, item, &[], &[])
    }

    fn plan_in(
        entry: &HistoryEntry,
        item: &RecoverItem,
        repos: &[RepoEntry],
        workspaces: &[WorkspaceEntry],
    ) -> Result<RecoveryPlan, String> {
        let registered = Registered { repos, workspaces };
        plan_recovery(
            entry,
            item,
            &registered,
            agent_registry::plugin,
            |_, _, _| true,
        )
    }

    fn repo(id: &str, path: &str) -> RepoEntry {
        serde_json::from_value(json!({"id": id, "name": id, "path": path, "default_branch": null}))
            .expect("repo fixture")
    }

    fn workspace(id: &str, members: &[&str]) -> WorkspaceEntry {
        serde_json::from_value(json!({"id": id, "name": id, "member_repo_ids": members}))
            .expect("workspace fixture")
    }

    /// The yaat workspace: `D:\yaat` first, then `D:\yaat-server`.
    fn yaat_registry() -> (Vec<RepoEntry>, Vec<WorkspaceEntry>) {
        let repos = vec![repo("r-yaat", r"D:\yaat"), repo("r-srv", r"D:\yaat-server")];
        (repos, vec![workspace("ws1", &["r-yaat", "r-srv"])])
    }

    /// An imported member: a folder, its repo id when it is a registered
    /// repo's path, and no branch.
    fn imported(repo_id: &str, path: &str) -> SessionMember {
        SessionMember {
            branch: String::new(),
            ..member(repo_id, path)
        }
    }

    /// The recovery invariant: a repo or workspace target runs every member
    /// in a pinned folder, so the spawn neither checks out nor creates.
    fn assert_every_member_pinned(target: &SpawnTarget, members: usize) {
        match target {
            SpawnTarget::Single {
                use_worktree,
                existing_worktree,
                checkout_strategy,
                ..
            } => {
                assert_eq!(members, 1, "a Single target has one member");
                assert!(*use_worktree, "a pin needs use_worktree: {target:?}");
                assert!(existing_worktree.is_some(), "unpinned: {target:?}");
                assert_eq!(*checkout_strategy, None);
            }
            SpawnTarget::Workspace {
                use_worktree,
                existing_worktrees,
                ..
            } => {
                assert!(*use_worktree, "a pin needs use_worktree: {target:?}");
                assert_eq!(existing_worktrees.len(), members, "unpinned: {target:?}");
            }
            SpawnTarget::Standalone { .. } => {}
        }
    }

    #[test]
    fn recovered_entry_is_refused() {
        let mut entry = folder_only(vec![member("", r"D:\yaat")]);
        entry.recovered_at = Some(Utc::now());
        for how in [RecoverAs::Agent, RecoverAs::ShellResume, RecoverAs::Shell] {
            assert_eq!(
                plan(&entry, &item(how, Some(CONV))),
                Err("already recovered".to_owned())
            );
        }
    }

    #[test]
    fn missing_conversation_is_refused_with_its_folder() {
        let entry = folder_only(vec![member("", r"D:\yaat")]);
        let asked = std::cell::RefCell::new(Vec::new());
        let result = plan_recovery(
            &entry,
            &item(RecoverAs::ShellResume, Some("gone")),
            &Registered {
                repos: &[],
                workspaces: &[],
            },
            agent_registry::plugin,
            |agent, folder, id| {
                asked
                    .borrow_mut()
                    .push((agent.clone(), folder.to_owned(), id.to_owned()));
                false
            },
        );
        assert_eq!(
            result,
            Err(r"conversation gone not found for D:\yaat".to_owned())
        );
        assert_eq!(
            *asked.borrow(),
            [(
                agent_registry::default_agent(),
                r"D:\yaat".to_owned(),
                "gone".to_owned()
            )]
        );
    }

    #[test]
    fn claude_needs_a_conversation() {
        let entry = folder_only(vec![member("", r"D:\yaat")]);
        assert_eq!(
            plan(&entry, &item(RecoverAs::Agent, None)),
            Err("no conversation to resume".to_owned())
        );
        let register = RecoverAs::RegisterRepoThenAgent {
            path: r"D:\yaat".to_owned(),
        };
        assert_eq!(
            plan(&entry, &item(register, None)),
            Err("no conversation to resume".to_owned())
        );
    }

    #[test]
    fn claude_with_a_workspace_config_clones_it_pinned_and_resumes() {
        let mut entry = folder_only(vec![
            member("r-yaat", r"C:\wt\yaat"),
            member("r-srv", r"C:\wt\yaat-server"),
        ]);
        entry.spawn_config = Some(workspace_config());
        let (repos, workspaces) = yaat_registry();

        let plan = plan_in(
            &entry,
            &item(RecoverAs::Agent, Some(CONV)),
            &repos,
            &workspaces,
        )
        .expect("plan");

        assert_eq!(plan.register_repo, None);
        let req = plan.request;
        assert_eq!(
            req.target,
            SpawnTarget::Workspace {
                workspace_id: "ws1".to_owned(),
                branch_name: "feat/x".to_owned(),
                base_branch: None,
                use_worktree: true,
                worktree_reuse: WorktreeReusePolicy::Reuse,
                existing_worktrees: vec![
                    PinnedMemberWorktree {
                        repo_id: "r-yaat".to_owned(),
                        path: r"C:\wt\yaat".to_owned(),
                    },
                    PinnedMemberWorktree {
                        repo_id: "r-srv".to_owned(),
                        path: r"C:\wt\yaat-server".to_owned(),
                    },
                ],
            }
        );
        assert_every_member_pinned(&req.target, 2);
        assert_eq!(req.resume_conversation.as_deref(), Some(CONV));
        assert_eq!(req.mode, SessionMode::Interactive);
        assert_eq!(selected_agent(&req), Some(agent_registry::default_agent()));
        assert!(req.dangerously_skip_permissions);
        assert_eq!(model_of(&req), Some("opus"));
        assert_eq!(req.initial_prompt, None);
        assert_eq!(req.prompt_injector, None);
    }

    #[test]
    fn claude_from_a_shell_config_is_forced_to_interactive_claude() {
        let mut entry = folder_only(vec![member("r1", r"D:\repo")]);
        let mut config = workspace_config();
        config.mode = SessionMode::PlainShell;
        entry.spawn_config = Some(config);

        let req = plan(&entry, &item(RecoverAs::Agent, Some(CONV)))
            .expect("plan")
            .request;

        assert_eq!(req.mode, SessionMode::Interactive);
        assert_eq!(selected_agent(&req), Some(agent_registry::default_agent()));
        assert_eq!(req.resume_conversation.as_deref(), Some(CONV));
    }

    /// Claude on `DeepSeek` in plan mode.
    fn deepseek_options() -> [(&'static str, &'static str); 2] {
        [("permission_mode", "plan"), ("provider", "deepseek")]
    }

    /// A recorded `DeepSeek` entry: a workspace config on `DeepSeek`.
    fn recorded_deepseek(members: Vec<SessionMember>) -> HistoryEntry {
        let mut entry = folder_only(members);
        let mut config = workspace_config();
        config.agent = Some(claude(&deepseek_options()));
        entry.spawn_config = Some(config);
        entry
    }

    /// An imported `DeepSeek` entry: no config, the provider its `--model` named.
    fn imported_deepseek(members: Vec<SessionMember>) -> HistoryEntry {
        let mut entry = folder_only(members);
        entry.options =
            Some(claude(&[("provider", "deepseek"), ("model", "deepseek-flash")]).options);
        entry
    }

    /// What [`option`] answers for a text value.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "compared with what `option` answers"
    )]
    fn text(value: &str) -> Option<protocol::OptionValue> {
        Some(protocol::OptionValue::Text(value.to_owned()))
    }

    #[test]
    fn recover_keeps_a_deepseek_provider() {
        let (repos, workspaces) = yaat_registry();
        let members = vec![
            member("r-yaat", r"C:\wt\yaat"),
            member("r-srv", r"C:\wt\yaat-server"),
        ];
        let recorded = plan_in(
            &recorded_deepseek(members.clone()),
            &item(RecoverAs::Agent, Some(CONV)),
            &repos,
            &workspaces,
        )
        .expect("plan")
        .request;
        assert_eq!(option(&recorded, "provider"), text("deepseek"));
        assert_eq!(option(&recorded, "permission_mode"), text("plan"));

        for registry in [(repos.as_slice(), workspaces.as_slice()), (&[], &[])] {
            let imported = plan_in(
                &imported_deepseek(members.clone()),
                &item(RecoverAs::Agent, Some(CONV)),
                registry.0,
                registry.1,
            )
            .expect("plan")
            .request;
            assert_eq!(
                option(&imported, "provider"),
                text("deepseek"),
                "{:?}",
                imported.target
            );
            assert_eq!(model_of(&imported), Some("deepseek-flash"));
        }
    }

    #[test]
    fn shell_recovery_of_a_deepseek_entry_is_refused() {
        let members = vec![member("", r"D:\yaat")];
        for entry in [
            recorded_deepseek(members.clone()),
            imported_deepseek(members.clone()),
        ] {
            assert_eq!(
                plan(&entry, &item(RecoverAs::ShellResume, Some(CONV))),
                Err("DeepSeek sessions recover as Claude.".to_owned()),
                "{:?}",
                entry.spawn_config
            );
            // A bare shell runs no agent, so no provider refuses it.
            for conversation in [Some(CONV), None] {
                assert!(
                    plan(&entry, &item(RecoverAs::Shell, conversation)).is_ok(),
                    "{conversation:?} {:?}",
                    entry.spawn_config
                );
            }
        }
        let anthropic = folder_only(members);
        assert!(plan(&anthropic, &item(RecoverAs::ShellResume, Some(CONV))).is_ok());
        assert!(plan(&anthropic, &item(RecoverAs::Shell, None)).is_ok());
    }

    #[test]
    fn shell_recovery_of_an_unknown_provider_entry_is_refused() {
        let mut entry = folder_only(vec![member("", r"D:\yaat")]);
        entry.options = Some(raw_options(&[("provider", "unknown")]));
        assert_eq!(
            plan(&entry, &item(RecoverAs::ShellResume, Some(CONV))),
            Err("Sessions on this Claude provider recover as Claude.".to_owned())
        );
    }

    #[test]
    fn plan_recovery_refuses_a_shell_through_the_plugins_shell_resume() {
        let entry = imported_deepseek(vec![member("", r"D:\yaat")]);
        let options = claude(&[("provider", "deepseek"), ("model", "deepseek-flash")]).options;
        let refusal = agent_registry::plugin(&agent_registry::default_agent())
            .expect("Claude is registered")
            .shell_resume(&options, "", &[])
            .expect_err("the plugin refuses a DeepSeek shell");
        let registered = Registered {
            repos: &[],
            workspaces: &[],
        };
        // The refusal comes before the conversation's existence check, so a
        // missing conversation does not hide it.
        assert_eq!(
            plan_recovery(
                &entry,
                &item(RecoverAs::ShellResume, Some(CONV)),
                &registered,
                agent_registry::plugin,
                |_, _, _| false
            ),
            Err(refusal.to_string())
        );
    }

    /// The two Claude recoveries of an entry known by its folder `D:\foo`.
    fn claude_paths() -> [RecoverAs; 2] {
        [
            RecoverAs::Agent,
            RecoverAs::RegisterRepoThenAgent {
                path: r"D:\foo".to_owned(),
            },
        ]
    }

    #[test]
    fn a_plain_shell_record_recovers_as_claude_without_its_stale_flags() {
        let mut entry = folder_only(vec![member("", r"D:\foo")]);
        let mut config = workspace_config();
        config.mode = SessionMode::PlainShell;
        assert!(config.dangerously_skip_permissions && config.agent.is_some());
        entry.spawn_config = Some(config);
        for how in claude_paths() {
            let request = plan(&entry, &item(how.clone(), Some(CONV)))
                .expect("plan")
                .request;
            assert_eq!(option(&request, "provider"), text("anthropic"), "{how:?}");
            assert!(!request.dangerously_skip_permissions, "{how:?}");
            assert_eq!(model_of(&request), None, "{how:?}");
        }
    }

    #[test]
    fn a_deepseek_claude_config_keeps_its_flags_through_every_claude_path() {
        let mut entry = recorded_deepseek(vec![member("", r"D:\foo")]);
        if let Some(config) = entry.spawn_config.as_mut() {
            config.agent = Some(claude(&[
                ("permission_mode", "plan"),
                ("provider", "deepseek"),
                ("model", "deepseek-pro"),
            ]));
            config.dangerously_skip_permissions = true;
        }
        for how in claude_paths() {
            let request = plan(&entry, &item(how.clone(), Some(CONV)))
                .expect("plan")
                .request;
            assert_eq!(option(&request, "provider"), text("deepseek"), "{how:?}");
            assert_eq!(option(&request, "permission_mode"), text("plan"), "{how:?}");
            assert!(request.dangerously_skip_permissions, "{how:?}");
            assert_eq!(model_of(&request), Some("deepseek-pro"), "{how:?}");
        }
    }

    #[test]
    fn register_repo_then_claude_keeps_the_provider_of_a_config() {
        let register = RecoverAs::RegisterRepoThenAgent {
            path: r"D:\foo".to_owned(),
        };
        let members = vec![member("", r"D:\foo")];
        let mut entry = recorded_deepseek(members.clone());
        if let Some(config) = entry.spawn_config.as_mut() {
            config.agent = Some(claude(&[
                ("permission_mode", "plan"),
                ("provider", "deepseek"),
                ("model", "deepseek-pro"),
            ]));
            config.dangerously_skip_permissions = true;
        }
        let recorded = plan(&entry, &item(register.clone(), Some(CONV)))
            .expect("plan")
            .request;
        assert_eq!(option(&recorded, "provider"), text("deepseek"));
        assert_eq!(option(&recorded, "permission_mode"), text("plan"));
        assert_eq!(model_of(&recorded), Some("deepseek-pro"));
        assert!(recorded.dangerously_skip_permissions);

        let imported = plan(&imported_deepseek(members), &item(register, Some(CONV)))
            .expect("plan")
            .request;
        assert_eq!(option(&imported, "provider"), text("deepseek"));
    }

    /// [`TestAgent`]'s schema with `caps.shell_resume` on.
    static SHELL_RESUME_CAP: LazyLock<AgentDescriptor> = LazyLock::new(|| {
        let mut descriptor = agent_api::test_agent::DESCRIPTOR.clone();
        descriptor.caps.shell_resume = true;
        descriptor
    });

    /// The test agent with `descriptor` in place of its own, whose
    /// `shell_resume` answers `resume_command`.
    struct ShellResumeAgent {
        inner: TestAgent,
        descriptor: &'static LazyLock<AgentDescriptor>,
        resume_command: Option<&'static str>,
    }

    impl ShellResumeAgent {
        fn new(
            descriptor: &'static LazyLock<AgentDescriptor>,
            resume_command: Option<&'static str>,
        ) -> Self {
            Self {
                inner: TestAgent::new(agent_api::ConversationPlan::None),
                descriptor,
                resume_command,
            }
        }
    }

    impl agent_api::Agent for ShellResumeAgent {
        type Options = TestOptions;

        fn descriptor(&self) -> &AgentDescriptor {
            LazyLock::force(self.descriptor)
        }

        fn program_env_var(&self) -> &'static str {
            agent_api::Agent::program_env_var(&self.inner)
        }

        fn default_program(&self) -> &'static str {
            agent_api::Agent::default_program(&self.inner)
        }

        fn liveness_tokens(&self) -> &'static [&'static str] {
            agent_api::Agent::liveness_tokens(&self.inner)
        }

        fn recovery_options(&self, run: &agent_api::RecoveredRun<'_>) -> TestOptions {
            agent_api::Agent::recovery_options(&self.inner, run)
        }

        fn shell_resume(
            &self,
            _opts: &TestOptions,
            _conversation_id: &str,
            _hook_files: &[std::path::PathBuf],
        ) -> Result<Option<String>, agent_api::AgentError> {
            Ok(self.resume_command.map(str::to_owned))
        }

        fn check_request(
            &self,
            opts: &TestOptions,
            req: &agent_api::SpawnCheck<'_>,
        ) -> Result<(), agent_api::Refusal> {
            agent_api::Agent::check_request(&self.inner, opts, req)
        }

        fn spawn_env(
            &self,
            opts: &TestOptions,
            rows: &agent_api::ResolvedRows,
            keys: &dyn agent_api::KeyLookup,
        ) -> Result<agent_api::EnvPlan, agent_api::Refusal> {
            agent_api::Agent::spawn_env(&self.inner, opts, rows, keys)
        }

        fn conversation_plan(
            &self,
            opts: &TestOptions,
            resume: Option<&str>,
        ) -> agent_api::ConversationPlan {
            agent_api::Agent::conversation_plan(&self.inner, opts, resume)
        }

        fn interactive_args(
            &self,
            opts: &TestOptions,
            ctx: &agent_api::SpawnContext<'_>,
            members: &[SessionMember],
            initial_prompt: Option<&str>,
        ) -> Vec<String> {
            agent_api::Agent::interactive_args(&self.inner, opts, ctx, members, initial_prompt)
        }
    }

    static TEST_AGENT: LazyLock<TestAgent> =
        LazyLock::new(|| TestAgent::new(agent_api::ConversationPlan::None));

    /// Claims `caps.shell_resume` while its `shell_resume` builds no command.
    static SHELL_RESUME_NONE: LazyLock<ShellResumeAgent> =
        LazyLock::new(|| ShellResumeAgent::new(&SHELL_RESUME_CAP, None));

    /// Lacks `caps.shell_resume` while its `shell_resume` builds a command.
    static SHELL_RESUME_WITHOUT_CAP: LazyLock<ShellResumeAgent> = LazyLock::new(|| {
        ShellResumeAgent::new(
            &agent_api::test_agent::DESCRIPTOR,
            Some("test-agent --resume"),
        )
    });

    #[expect(clippy::unnecessary_wraps, reason = "a PluginLookup answers an Option")]
    fn test_agent_plugin(_agent: &AgentId) -> Option<&'static dyn AgentPlugin> {
        Some(&*TEST_AGENT)
    }

    #[expect(clippy::unnecessary_wraps, reason = "a PluginLookup answers an Option")]
    fn shell_resume_none_plugin(_agent: &AgentId) -> Option<&'static dyn AgentPlugin> {
        Some(&*SHELL_RESUME_NONE)
    }

    #[expect(clippy::unnecessary_wraps, reason = "a PluginLookup answers an Option")]
    fn shell_resume_without_cap_plugin(_agent: &AgentId) -> Option<&'static dyn AgentPlugin> {
        Some(&*SHELL_RESUME_WITHOUT_CAP)
    }

    /// A folder entry the test agent ran.
    fn test_agent_entry() -> HistoryEntry {
        let mut entry = folder_only(vec![member("", r"D:\foo")]);
        entry.agent = Some(agent_api::test_agent::DESCRIPTOR.id.clone());
        entry
    }

    fn plan_with(plugins: PluginLookup, how: RecoverAs) -> Result<RecoveryPlan, String> {
        let registered = Registered {
            repos: &[],
            workspaces: &[],
        };
        plan_recovery(
            &test_agent_entry(),
            &item(how, Some(CONV)),
            &registered,
            plugins,
            |_, _, _| true,
        )
    }

    #[test]
    fn plan_recovery_offers_shell_resume_only_with_the_cap() {
        assert_eq!(
            plan_with(shell_resume_without_cap_plugin, RecoverAs::ShellResume),
            Err("Test agent cannot resume a conversation in a shell".to_owned()),
            "an agent without the cap is refused although its shell_resume builds a command"
        );
        assert!(
            plan_with(test_agent_plugin, RecoverAs::Shell).is_ok(),
            "a bare shell needs no cap"
        );
        let claude = plan(
            &folder_only(vec![member("", r"D:\foo")]),
            &item(RecoverAs::ShellResume, Some(CONV)),
        )
        .expect("Claude has the cap");
        assert_eq!(claude.hook_agent, Some(agent_registry::default_agent()));
        assert_eq!(claude.resume_conversation.as_deref(), Some(CONV));
    }

    #[test]
    fn shell_resume_none_refuses_the_shell_resume_recovery() {
        assert_eq!(
            plan_with(shell_resume_none_plugin, RecoverAs::ShellResume),
            Err("Test agent cannot resume a conversation in a shell".to_owned())
        );
        assert!(plan_with(shell_resume_none_plugin, RecoverAs::Shell).is_ok());
    }

    /// Claims `caps.shell_resume` and builds a command.
    static SHELL_RESUME_SOME: LazyLock<ShellResumeAgent> =
        LazyLock::new(|| ShellResumeAgent::new(&SHELL_RESUME_CAP, Some("test-agent --resume")));

    #[expect(clippy::unnecessary_wraps, reason = "a PluginLookup answers an Option")]
    fn shell_resume_some_plugin(_agent: &AgentId) -> Option<&'static dyn AgentPlugin> {
        Some(&*SHELL_RESUME_SOME)
    }

    #[test]
    fn plan_recovery_shell_resume_carries_the_recovered_options() {
        let mut entry = test_agent_entry();
        let recorded = OptionValues::from([
            (
                "mode".to_owned(),
                protocol::OptionValue::Text("recovered-mode".to_owned()),
            ),
            ("sandbox".to_owned(), protocol::OptionValue::Bool(true)),
        ]);
        entry.options = Some(recorded.clone());
        entry.trusted = true;
        let plugin: &dyn AgentPlugin = &*SHELL_RESUME_SOME;
        let expected = plugin
            .recovery_options(&RecoveredRun {
                recorded: Some(&recorded),
                trusted: true,
            })
            .expect("the recorded options decode");
        assert_eq!(expected.get("mode").cloned(), text("recovered-mode"));
        let registered = Registered {
            repos: &[],
            workspaces: &[],
        };

        let plan = plan_recovery(
            &entry,
            &item(RecoverAs::ShellResume, Some(CONV)),
            &registered,
            shell_resume_some_plugin,
            |_, _, _| true,
        )
        .expect("plan");

        assert_eq!(plan.hook_options, expected);
        assert!(plan.hook_trusted, "the recovered run's trust is carried");
        for how in [RecoverAs::Shell, RecoverAs::Agent] {
            let other = plan_recovery(
                &entry,
                &item(how.clone(), Some(CONV)),
                &registered,
                shell_resume_some_plugin,
                |_, _, _| true,
            )
            .expect("plan");
            assert!(other.hook_options.is_empty(), "{how:?}");
            assert!(!other.hook_trusted, "{how:?}");
        }
    }

    const SINGLE_FOLDER: &str = r"C:\wt\repo";

    fn single_config() -> SpawnConfig {
        SpawnConfig {
            target: SpawnTarget::Single {
                repo_id: "r1".to_owned(),
                branch_name: "feat/x".to_owned(),
                base_branch: Some("main".to_owned()),
                use_worktree: true,
                checkout_strategy: None,
                worktree_reuse: WorktreeReusePolicy::Reuse,
                existing_worktree: None,
            },
            mode: SessionMode::Interactive,
            dangerously_skip_permissions: false,
            agent: Some(claude(&[])),
            extra_env: Vec::new(),
        }
    }

    #[test]
    fn claude_recovery_of_a_removed_single_repo_entry_runs_standalone() {
        let mut entry = folder_only(vec![member("r1", SINGLE_FOLDER)]);
        entry.spawn_config = Some(single_config());
        let recover = item(RecoverAs::Agent, Some(CONV));

        let removed = plan(&entry, &recover).expect("plan").request;
        assert_eq!(
            removed.target,
            SpawnTarget::Standalone {
                cwd: Some(SINGLE_FOLDER.to_owned()),
                add_dirs: Vec::new(),
            }
        );
        assert_eq!(
            selected_agent(&removed),
            Some(agent_registry::default_agent())
        );
        assert_eq!(removed.resume_conversation.as_deref(), Some(CONV));

        let registered = plan_in(&entry, &recover, &[repo("r1", r"D:\repo")], &[])
            .expect("plan")
            .request;
        assert!(
            matches!(&registered.target, SpawnTarget::Single { repo_id, .. } if repo_id == "r1"),
            "{:?}",
            registered.target
        );
        assert_every_member_pinned(&registered.target, 1);
    }

    #[test]
    fn a_headless_entry_is_refused() {
        let register = RecoverAs::RegisterRepoThenAgent {
            path: SINGLE_FOLDER.to_owned(),
        };
        let mut entry = folder_only(vec![member("r1", SINGLE_FOLDER)]);
        entry.mode = SessionMode::Headless;
        for how in [
            RecoverAs::Agent,
            register,
            RecoverAs::ShellResume,
            RecoverAs::Shell,
            RecoverAs::Unknown,
        ] {
            assert_eq!(
                plan_in(
                    &entry,
                    &item(how.clone(), Some(CONV)),
                    &[repo("r1", r"D:\repo")],
                    &[]
                ),
                Err("a headless session can't be recovered".to_owned()),
                "{how:?}"
            );
        }
    }

    #[test]
    fn a_recorded_workspace_whose_members_changed_is_not_replayed_unpinned() {
        let mut entry = folder_only(vec![
            member("r-yaat", r"C:\wt\yaat"),
            member("r-srv", r"C:\wt\yaat-server"),
        ]);
        entry.spawn_config = Some(workspace_config());
        let (mut repos, _) = yaat_registry();
        repos.push(repo("r-new", r"D:\new"));
        let grown = [workspace("ws1", &["r-yaat", "r-srv", "r-new"])];

        let req = plan_in(&entry, &item(RecoverAs::Agent, Some(CONV)), &repos, &grown)
            .expect("plan")
            .request;

        assert_eq!(
            req.target,
            SpawnTarget::Standalone {
                cwd: Some(r"C:\wt\yaat".to_owned()),
                add_dirs: vec![r"C:\wt\yaat-server".to_owned()],
            }
        );
        assert_eq!(model_of(&req), Some("opus"), "the config's flags stay");
    }

    #[test]
    fn a_recorded_in_place_single_is_pinned_to_its_folder() {
        let mut entry = folder_only(vec![member("r1", r"D:\repo")]);
        let mut config = workspace_config();
        config.target = SpawnTarget::Single {
            repo_id: "r1".to_owned(),
            branch_name: "main".to_owned(),
            base_branch: Some("main".to_owned()),
            use_worktree: false,
            checkout_strategy: Some(protocol::CheckoutStrategy::Stash),
            worktree_reuse: WorktreeReusePolicy::Reuse,
            existing_worktree: None,
        };
        entry.spawn_config = Some(config);

        let req = plan_in(
            &entry,
            &item(RecoverAs::Agent, Some(CONV)),
            &[repo("r1", r"D:\repo")],
            &[],
        )
        .expect("plan")
        .request;

        assert_eq!(
            req.target,
            SpawnTarget::Single {
                repo_id: "r1".to_owned(),
                branch_name: "main".to_owned(),
                base_branch: Some("main".to_owned()),
                use_worktree: true,
                checkout_strategy: None,
                worktree_reuse: WorktreeReusePolicy::Reuse,
                existing_worktree: Some(r"D:\repo".to_owned()),
            }
        );
        assert_every_member_pinned(&req.target, 1);
        assert_eq!(req.resume_conversation.as_deref(), Some(CONV));
    }

    #[test]
    fn an_imported_workspace_entry_recovers_into_its_pinned_workspace() {
        let (repos, workspaces) = yaat_registry();
        let named = {
            let mut entry = folder_only(vec![
                imported("r-yaat", r"D:\yaat"),
                imported("r-srv", r"D:\yaat-server"),
            ]);
            entry.workspace_id = Some("ws1".to_owned());
            entry
        };
        // Matched now, by folder: imported before the workspace existed.
        let matched = folder_only(vec![
            imported("", r"d:/YAAT/"),
            imported("", r"D:\yaat-server"),
        ]);
        for entry in [named, matched] {
            let req = plan_in(
                &entry,
                &item(RecoverAs::Agent, Some(CONV)),
                &repos,
                &workspaces,
            )
            .expect("plan")
            .request;

            let (workspace_id, pins): (Option<&str>, Vec<(&str, &str)>) = match &req.target {
                SpawnTarget::Workspace {
                    workspace_id,
                    existing_worktrees,
                    ..
                } => (
                    Some(workspace_id.as_str()),
                    existing_worktrees
                        .iter()
                        .map(|p| (p.repo_id.as_str(), p.path.as_str()))
                        .collect(),
                ),
                _ => (None, Vec::new()),
            };
            assert_eq!(workspace_id, Some("ws1"), "{:?}", req.target);
            assert_eq!(
                pins,
                [
                    ("r-yaat", entry.members[0].worktree_path.as_str()),
                    ("r-srv", r"D:\yaat-server"),
                ]
            );
            assert_every_member_pinned(&req.target, 2);
            assert_eq!(req.resume_conversation.as_deref(), Some(CONV));
            assert_eq!(req.mode, SessionMode::Interactive);
        }
    }

    #[test]
    fn an_imported_single_repo_entry_recovers_into_its_pinned_repo() {
        let entry = folder_only(vec![imported("", r"D:\foo")]);
        let repos = [repo("r-foo", "d:/FOO/")];

        let req = plan_in(&entry, &item(RecoverAs::Agent, Some(CONV)), &repos, &[])
            .expect("plan")
            .request;

        assert_eq!(
            req.target,
            SpawnTarget::Single {
                repo_id: "r-foo".to_owned(),
                branch_name: PINNED_BRANCH_FALLBACK.to_owned(),
                base_branch: None,
                use_worktree: true,
                checkout_strategy: None,
                worktree_reuse: WorktreeReusePolicy::Reuse,
                existing_worktree: Some(r"D:\foo".to_owned()),
            }
        );
        assert_every_member_pinned(&req.target, 1);
        assert_eq!(req.resume_conversation.as_deref(), Some(CONV));
    }

    #[test]
    fn folder_only_claude_replays_its_add_dirs_in_a_standalone_target() {
        let entry = folder_only(vec![
            member("r-yaat", r"D:\yaat"),
            member("", r"D:\yaat-server"),
        ]);
        // Only one of the two folders is a registered repo, and no workspace
        // has both.
        let repos = [repo("r-yaat", r"D:\yaat")];

        let plan = plan_in(&entry, &item(RecoverAs::Agent, Some(CONV)), &repos, &[]).expect("plan");

        assert_eq!(plan.register_repo, None);
        assert_eq!(
            plan.request.target,
            SpawnTarget::Standalone {
                cwd: Some(r"D:\yaat".to_owned()),
                add_dirs: vec![r"D:\yaat-server".to_owned()],
            }
        );
        assert_eq!(plan.request.mode, SessionMode::Interactive);
        assert_eq!(
            selected_agent(&plan.request),
            Some(agent_registry::default_agent())
        );
        assert_eq!(plan.request.resume_conversation.as_deref(), Some(CONV));
        assert!(!plan.request.dangerously_skip_permissions);
    }

    #[test]
    fn folder_only_and_register_paths_replay_the_recorded_flags() {
        let mut entry = folder_only(vec![member("", r"D:\foo")]);
        entry.trusted = true;
        entry.options = Some(claude(&[("model", "opus")]).options);
        let register = RecoverAs::RegisterRepoThenAgent {
            path: r"D:\foo".to_owned(),
        };
        for how in [RecoverAs::Agent, register] {
            let req = plan(&entry, &item(how.clone(), Some(CONV)))
                .expect("plan")
                .request;
            assert!(req.dangerously_skip_permissions, "{how:?}");
            assert_eq!(model_of(&req), Some("opus"), "{how:?}");
        }
    }

    #[test]
    fn folder_only_claude_without_members_runs_in_the_folder() {
        let mut entry = folder_only(Vec::new());
        entry.primary_cwd = Some(r"D:\scratch".to_owned());

        let req = plan(&entry, &item(RecoverAs::Agent, Some(CONV)))
            .expect("plan")
            .request;

        assert_eq!(
            req.target,
            SpawnTarget::Standalone {
                cwd: Some(r"D:\scratch".to_owned()),
                add_dirs: Vec::new(),
            }
        );
    }

    #[test]
    fn register_repo_then_claude_registers_the_folder_and_resumes_there() {
        let entry = folder_only(vec![member("", r"D:\foo"), member("", r"D:\bar")]);
        let how = RecoverAs::RegisterRepoThenAgent {
            path: "d:/FOO/".to_owned(),
        };

        let plan = plan(&entry, &item(how, Some(CONV))).expect("plan");

        assert_eq!(plan.register_repo.as_deref(), Some("d:/FOO/"));
        assert_eq!(
            plan.request.target,
            SpawnTarget::Standalone {
                cwd: Some("d:/FOO/".to_owned()),
                add_dirs: vec![r"D:\bar".to_owned()],
            }
        );
        assert_eq!(plan.request.resume_conversation.as_deref(), Some(CONV));
    }

    #[test]
    fn register_repo_then_claude_carries_the_recorded_env_rows() {
        let mut entry = folder_only(vec![member("", r"D:\foo"), member("", r"D:\bar")]);
        let mut config = workspace_config();
        config.extra_env = recorded_env_rows();
        entry.spawn_config = Some(config);
        let how = RecoverAs::RegisterRepoThenAgent {
            path: "d:/FOO/".to_owned(),
        };

        let plan = plan(&entry, &item(how, Some(CONV))).expect("plan");

        assert_eq!(plan.request.extra_env, recorded_env_rows());
    }

    #[test]
    fn register_repo_then_claude_runs_pinned_in_the_new_repo() {
        let entry = folder_only(vec![imported("", r"D:\foo")]);
        let how = RecoverAs::RegisterRepoThenAgent {
            path: r"D:\foo".to_owned(),
        };

        let mut plan = plan(&entry, &item(how, Some(CONV))).expect("plan");
        plan.bind_registered_repo("r-new");

        assert_eq!(plan.register_repo.as_deref(), Some(r"D:\foo"));
        assert_eq!(
            plan.request.target,
            SpawnTarget::Single {
                repo_id: "r-new".to_owned(),
                branch_name: PINNED_BRANCH_FALLBACK.to_owned(),
                base_branch: None,
                use_worktree: true,
                checkout_strategy: None,
                worktree_reuse: WorktreeReusePolicy::Reuse,
                existing_worktree: Some(r"D:\foo".to_owned()),
            }
        );
        assert_every_member_pinned(&plan.request.target, 1);
        assert_eq!(plan.request.resume_conversation.as_deref(), Some(CONV));
    }

    #[test]
    fn a_placeholder_branch_is_named_for_the_first_pinned_folder() {
        assert_eq!(branch_or_placeholder(Ok(Some("main".to_owned()))), "main");
        assert_eq!(branch_or_placeholder(Ok(None)), PINNED_BRANCH_FALLBACK);
        assert_eq!(
            branch_or_placeholder(Err(anyhow!("not a repo"))),
            PINNED_BRANCH_FALLBACK
        );

        let (repos, workspaces) = yaat_registry();
        let entry = folder_only(vec![
            imported("", r"D:\yaat"),
            imported("", r"D:\yaat-server"),
        ]);
        let mut named = plan_in(
            &entry,
            &item(RecoverAs::Agent, Some(CONV)),
            &repos,
            &workspaces,
        )
        .expect("plan");
        assert_eq!(named.placeholder_branch_folder(), Some(r"D:\yaat"));
        named.name_branch("main".to_owned());
        assert_eq!(named.placeholder_branch_folder(), None);
        assert!(
            matches!(&named.request.target, SpawnTarget::Workspace { branch_name, .. } if branch_name == "main"),
            "{:?}",
            named.request.target
        );

        // A recorded branch is kept, and a standalone plan has no branch.
        let mut recorded = entry.clone();
        recorded.members[0].branch = "feat/x".to_owned();
        let kept = plan_in(
            &recorded,
            &item(RecoverAs::Agent, Some(CONV)),
            &repos,
            &workspaces,
        )
        .expect("plan");
        assert_eq!(kept.placeholder_branch_folder(), None);
        let standalone = plan(&entry, &item(RecoverAs::Agent, Some(CONV))).expect("plan");
        assert_eq!(standalone.placeholder_branch_folder(), None);
    }

    #[test]
    fn register_repo_then_claude_refuses_another_folder() {
        let entry = folder_only(vec![member("", r"D:\foo")]);
        let how = RecoverAs::RegisterRepoThenAgent {
            path: r"D:\elsewhere".to_owned(),
        };
        assert_eq!(
            plan(&entry, &item(how, Some(CONV))),
            Err(r"D:\elsewhere is not the session's folder D:\foo".to_owned())
        );
    }

    #[test]
    fn shell_types_the_resume_command_in_the_start_folder() {
        // An interactive session's shell followed a `cd`; its conversation
        // lives under the folder the session started in.
        let mut entry = folder_only(vec![member("", r"D:\yaat")]);
        entry.current_cwd = Some(r"D:\yaat\src".to_owned());
        entry.spawn_config = Some(workspace_config());

        let plan = plan(&entry, &item(RecoverAs::ShellResume, Some(CONV))).expect("plan");
        let req = &plan.request;

        assert_eq!(req.mode, SessionMode::PlainShell);
        assert_eq!(
            req.target,
            SpawnTarget::Standalone {
                cwd: Some(r"D:\yaat".to_owned()),
                add_dirs: Vec::new(),
            }
        );
        assert_eq!(req.resume_conversation, None);
        assert_eq!(
            req.prompt_injector, None,
            "the spawn types the plugin's command"
        );
        assert_eq!(plan.hook_agent, Some(agent_registry::default_agent()));
        assert_eq!(plan.resume_conversation.as_deref(), Some(CONV));
    }

    #[test]
    fn shell_recovery_carries_the_recorded_env_rows() {
        let mut entry = folder_only(vec![member("", r"D:\yaat")]);
        let mut config = workspace_config();
        config.extra_env = recorded_env_rows();
        entry.spawn_config = Some(config);

        let req = plan(&entry, &item(RecoverAs::ShellResume, Some(CONV)))
            .expect("plan")
            .request;

        assert_eq!(req.extra_env, recorded_env_rows());
    }

    #[test]
    fn a_recovery_without_a_spawn_config_has_no_env_rows() {
        let entry = folder_only(vec![member("", r"D:\yaat")]);

        let req = plan(&entry, &item(RecoverAs::ShellResume, Some(CONV)))
            .expect("plan")
            .request;

        assert!(req.extra_env.is_empty(), "{:?}", req.extra_env);
    }

    #[test]
    fn a_shell_entry_recovers_through_the_default_agent() {
        let mut entry = folder_only(vec![member("", r"D:\yaat")]);
        entry.mode = SessionMode::PlainShell;
        entry.agent = None;

        let resumed = plan(&entry, &item(RecoverAs::ShellResume, Some(CONV))).expect("plan");
        assert_eq!(resumed.hook_agent, Some(agent_registry::default_agent()));
        assert_eq!(resumed.resume_conversation.as_deref(), Some(CONV));

        let agent = plan(&entry, &item(RecoverAs::Agent, Some(CONV)))
            .expect("plan")
            .request;
        assert_eq!(
            selected_agent(&agent),
            Some(agent_registry::default_agent())
        );
        assert_eq!(agent.mode, SessionMode::Interactive);
    }

    #[test]
    fn sidecar_and_history_round_trip_conversation_id() {
        let dirs = scratch_dirs("conversation-id-round-trip");
        let mut rec = record("s-conv", SessionMode::Interactive);
        rec.conversation_id = Some(CONV.to_owned());
        let meta = crate::orphan::meta_from_record(
            rec.id.clone(),
            1,
            rec.label.clone(),
            SessionKind::Standalone,
            SessionMode::Interactive,
            Vec::new(),
            Utc::now(),
            None,
            None,
            rec.wire_agent(),
            None,
            None,
            None,
            None,
            None,
            None,
            rec.conversation_id.clone(),
        )
        .expect("build meta");
        crate::orphan::write_meta(&dirs, &meta).expect("write meta");
        let loaded = crate::orphan::load_meta(&dirs, "s-conv").expect("load meta");
        assert_eq!(loaded.conversation_id.as_deref(), Some(CONV));

        let ended = entry_from_meta(&loaded, SessionEnd::TracerLost, Utc::now());
        write_if_absent(&dirs, &ended).expect("write the entry");
        let read = read_one(&dirs, "s-conv").expect("read the entry");
        assert_eq!(read.conversation_id.as_deref(), Some(CONV));
        let from_record = entry_from_record(&rec, SessionEnd::TracerLost, Utc::now());
        assert_eq!(from_record.conversation_id.as_deref(), Some(CONV));
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn shell_without_a_conversation_is_a_bare_shell() {
        let entry = folder_only(vec![member("", r"D:\yaat")]);
        let plan = plan(&entry, &item(RecoverAs::Shell, None)).expect("plan");
        assert_eq!(plan.request.mode, SessionMode::PlainShell);
        assert_eq!(plan.request.prompt_injector, None);
        assert_eq!(plan.hook_agent, None, "a bare shell gets no hooks");
        assert_eq!(plan.resume_conversation, None);
    }

    #[test]
    fn unknown_kind_is_refused() {
        let entry = folder_only(vec![member("", r"D:\yaat")]);
        assert_eq!(
            plan(&entry, &item(RecoverAs::Unknown, Some(CONV))),
            Err("unsupported recovery kind".to_owned())
        );
    }

    /// Lookup rows pointing every entry's Claude home at `home`.
    fn home_rows(home: &Path) -> impl Fn(&HistoryEntry) -> ResolvedRows + use<> {
        let home = home.display().to_string();
        move |_| ResolvedRows::new(vec![("CLAUDE_CONFIG_DIR".to_owned(), home.clone())])
    }

    fn write_transcript(home: &Path, cwd: &str, id: &str, at: DateTime<Utc>) {
        let dir = home
            .join("projects")
            .join(agent_claude::encode_project_dir(cwd));
        std::fs::create_dir_all(&dir).expect("create project dir");
        let path = dir.join(format!("{id}.jsonl"));
        let line =
            json!({"type": "user", "cwd": cwd, "message": {"content": format!("about {id}")}});
        std::fs::write(&path, format!("{line}\n")).expect("write transcript");
        File::options()
            .write(true)
            .open(&path)
            .expect("reopen transcript")
            .set_modified(SystemTime::from(at))
            .expect("set mtime");
    }

    fn history_entry(
        id: &str,
        mode: SessionMode,
        folder: &str,
        ended_at: DateTime<Utc>,
    ) -> HistoryEntry {
        let mut entry = entry_from_record(&record(id, mode), SessionEnd::TracerLost, ended_at);
        entry.primary_cwd = Some(folder.to_owned());
        entry.started_at = Some(ended_at - TimeDelta::hours(1));
        entry
    }

    /// An interactive session's shell follows the user's `cd`; its agent's
    /// conversation lives under the folder the session started in.
    #[test]
    fn an_interactive_entry_recovers_from_its_start_folder_after_a_cd() {
        let mut entry = history_entry("s", SessionMode::Interactive, r"D:\start", Utc::now());
        entry.current_cwd = Some(r"D:\elsewhere".to_owned());
        assert_eq!(entry_folder(&entry), Some(r"D:\start"));
        let mut shell = history_entry("t", SessionMode::PlainShell, r"D:\start", Utc::now());
        shell.current_cwd = Some(r"D:\elsewhere".to_owned());
        assert_eq!(
            entry_folder(&shell),
            Some(r"D:\elsewhere"),
            "a plain shell recovers where it last was"
        );
    }

    #[test]
    fn history_items_describe_folders_and_candidates() {
        let dirs = scratch_dirs("items");
        let home = dirs.config.join("claude-home");
        let git_dir = dirs.config.join("git-folder");
        let git_file = dirs.config.join("git-file-folder");
        let plain = dirs.config.join("plain-folder");
        std::fs::create_dir_all(git_dir.join(".git")).expect("git dir");
        std::fs::create_dir_all(&git_file).expect("git file folder");
        std::fs::write(git_file.join(".git"), "gitdir: elsewhere").expect("git file");
        std::fs::create_dir_all(&plain).expect("plain folder");
        let (git_dir, git_file, plain) = (
            git_dir.to_string_lossy().into_owned(),
            git_file.to_string_lossy().into_owned(),
            plain.to_string_lossy().into_owned(),
        );
        let ended = DateTime::from_timestamp(Utc::now().timestamp() - 600, 0).expect("time");

        let mut known = history_entry("known", SessionMode::Interactive, &git_dir, ended);
        known.conversation_id = Some("conv-known".to_owned());
        write_transcript(&home, &git_dir, "conv-known", ended - TimeDelta::days(3));
        write_transcript(&home, &git_dir, "conv-other", ended);
        let mut gone = history_entry(
            "gone",
            SessionMode::Interactive,
            &git_dir,
            ended - TimeDelta::seconds(1),
        );
        gone.conversation_id = Some("conv-missing".to_owned());
        let mut shell = history_entry(
            "shell",
            SessionMode::PlainShell,
            &plain,
            ended - TimeDelta::seconds(2),
        );
        shell.current_cwd = Some(git_file.clone());
        write_transcript(
            &home,
            &git_file,
            "conv-in-window",
            ended - TimeDelta::minutes(5),
        );
        write_transcript(
            &home,
            &git_file,
            "conv-after",
            ended + TimeDelta::minutes(3),
        );
        write_transcript(&home, &git_file, "conv-before", ended - TimeDelta::hours(2));
        let headless = history_entry("headless", SessionMode::Headless, &plain, ended);
        for e in [&known, &gone, &shell, &headless] {
            write_if_absent(&dirs, e).expect("write entry");
        }
        let repos: Vec<RepoEntry> = vec![
            serde_json::from_value(json!({"id": "r-git", "name": "git", "path": git_dir.to_uppercase(), "default_branch": null}))
                .expect("repo"),
        ];

        let items = history_items(&dirs, &repos, &home_rows(&home), agent_registry::plugin);

        let ids: Vec<&str> = items.iter().map(|i| i.entry.session_id.as_str()).collect();
        assert_eq!(ids, ["known", "gone", "shell"], "headless is left out");
        let cands =
            |i: usize| -> Vec<&str> { items[i].candidates.iter().map(|c| c.id.as_str()).collect() };
        assert_eq!(
            cands(0),
            ["conv-known"],
            "a known conversation is the only candidate"
        );
        assert_eq!(
            items[0].candidates[0].last_active,
            ended - TimeDelta::days(3)
        );
        assert_eq!(
            items[0].candidates[0].title.as_deref(),
            Some("about conv-known")
        );
        assert!(
            cands(1).is_empty(),
            "a known conversation whose file is gone"
        );
        assert_eq!(
            cands(2),
            ["conv-in-window"],
            "the shell's last folder is searched"
        );
        assert!(items[0].folder_is_git_repo);
        assert_eq!(items[0].folder_repo_id.as_deref(), Some("r-git"));
        assert!(items[2].folder_is_git_repo, "a .git file counts");
        assert_eq!(items[2].folder_repo_id, None);

        let no_home = history_items(
            &dirs,
            &repos,
            &home_rows(&dirs.config.join("no-claude-home")),
            agent_registry::plugin,
        );
        assert!(no_home.iter().all(|i| i.candidates.is_empty()));
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn entry_with_no_start_searches_a_day_back() {
        let dirs = scratch_dirs("items-no-start");
        let home = dirs.config.join("claude-home");
        let folder = r"D:\nowhere-real";
        let ended = DateTime::from_timestamp(Utc::now().timestamp() - 600, 0).expect("time");
        let mut entry = history_entry("s", SessionMode::Interactive, folder, ended);
        entry.started_at = None;
        write_if_absent(&dirs, &entry).expect("write entry");
        write_transcript(&home, folder, "within-a-day", ended - TimeDelta::hours(20));
        write_transcript(&home, folder, "too-old", ended - TimeDelta::hours(25));

        let items = history_items(&dirs, &[], &home_rows(&home), agent_registry::plugin);

        let ids: Vec<&str> = items[0].candidates.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["within-a-day"]);
        assert!(!items[0].folder_is_git_repo);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    /// Whether the one history item `entry` builds carries `shell_resume`,
    /// with a transcript `conversation` in its folder when one is given.
    fn shell_resume_flag(
        tag: &str,
        entry: &HistoryEntry,
        plugins: PluginLookup,
        conversation: Option<&str>,
    ) -> bool {
        let dirs = scratch_dirs(tag);
        let home = dirs.config.join("claude-home");
        let folder = entry.primary_cwd.clone().expect("folder");
        if let Some(conversation) = conversation {
            let at = entry.ended_at - TimeDelta::minutes(5);
            write_transcript(&home, &folder, conversation, at);
        }
        write_if_absent(&dirs, entry).expect("write entry");
        let items = history_items(&dirs, &[], &home_rows(&home), plugins);
        let flag = items[0].shell_resume;
        let _ = std::fs::remove_dir_all(&dirs.config);
        flag
    }

    #[test]
    fn history_item_flags_shell_resume_for_an_anthropic_claude_entry() {
        let ended = Utc::now() - TimeDelta::minutes(10);
        let mut entry = history_entry("s", SessionMode::Interactive, r"D:\proj", ended);
        entry.agent = Some(agent_registry::default_agent());
        entry.options = Some(claude(&[("provider", "anthropic")]).options);
        assert!(shell_resume_flag(
            "shell-resume-anthropic",
            &entry,
            agent_registry::plugin,
            Some("conv-resume"),
        ));
    }

    #[test]
    fn history_item_does_not_flag_shell_resume_on_deepseek() {
        let ended = Utc::now() - TimeDelta::minutes(10);
        let mut entry = history_entry("s", SessionMode::Interactive, r"D:\proj", ended);
        entry.agent = Some(agent_registry::default_agent());
        entry.options =
            Some(claude(&[("provider", "deepseek"), ("model", "deepseek-flash")]).options);
        assert!(!shell_resume_flag(
            "shell-resume-deepseek",
            &entry,
            agent_registry::plugin,
            Some("conv-resume"),
        ));
    }

    #[test]
    fn history_item_does_not_flag_shell_resume_without_the_cap() {
        let ended = Utc::now() - TimeDelta::minutes(10);
        let mut entry = history_entry("s", SessionMode::Interactive, r"D:\proj", ended);
        entry.agent = Some(agent_registry::default_agent());
        assert!(!shell_resume_flag(
            "shell-resume-without-cap",
            &entry,
            shell_resume_without_cap_plugin,
            Some("conv-resume"),
        ));
    }

    #[test]
    fn history_item_does_not_flag_shell_resume_without_a_candidate() {
        let ended = Utc::now() - TimeDelta::minutes(10);
        let mut entry = history_entry("s", SessionMode::Interactive, r"D:\proj", ended);
        entry.agent = Some(agent_registry::default_agent());
        assert!(!shell_resume_flag(
            "shell-resume-no-candidate",
            &entry,
            agent_registry::plugin,
            None,
        ));
    }

    /// Candidate ids for one entry `e` alone in a fresh history.
    fn candidate_ids(
        tag: &str,
        e: &HistoryEntry,
        transcripts: &[(&str, DateTime<Utc>)],
    ) -> Vec<String> {
        let dirs = scratch_dirs(tag);
        let home = dirs.config.join("claude-home");
        let folder = e.primary_cwd.clone().expect("folder");
        for (id, at) in transcripts {
            write_transcript(&home, &folder, id, *at);
        }
        write_if_absent(&dirs, e).expect("write entry");
        let items = history_items(&dirs, &[], &home_rows(&home), agent_registry::plugin);
        let _ = std::fs::remove_dir_all(&dirs.config);
        items[0].candidates.iter().map(|c| c.id.clone()).collect()
    }

    #[test]
    fn unknown_end_searches_up_to_now() {
        let now = DateTime::from_timestamp(Utc::now().timestamp(), 0).expect("time");
        let started = now - TimeDelta::hours(6);
        let mut e = history_entry("s", SessionMode::PlainShell, r"D:\unknown-end", started);
        e.started_at = Some(started);
        e.ended_at = started + TimeDelta::seconds(5);
        e.end_time_known = false;
        let transcripts = [
            ("hours-later", now - TimeDelta::hours(2)),
            ("before-start", started - TimeDelta::hours(1)),
        ];

        assert_eq!(
            candidate_ids("items-unknown-end", &e, &transcripts),
            ["hours-later"]
        );
    }

    #[test]
    fn unknown_end_without_a_start_searches_the_retention() {
        let now = DateTime::from_timestamp(Utc::now().timestamp(), 0).expect("time");
        let mut e = history_entry("s", SessionMode::PlainShell, r"D:\unknown-both", now);
        e.started_at = None;
        e.ended_at = now - TimeDelta::days(3);
        e.end_time_known = false;
        let transcripts = [
            ("six-days-ago", now - TimeDelta::days(6)),
            ("eight-days-ago", now - TimeDelta::days(8)),
        ];

        assert_eq!(
            candidate_ids("items-unknown-both", &e, &transcripts),
            ["six-days-ago"]
        );
    }

    #[test]
    fn known_end_window_stops_at_the_grace() {
        let now = DateTime::from_timestamp(Utc::now().timestamp(), 0).expect("time");
        let ended = now - TimeDelta::hours(6);
        let e = history_entry("s", SessionMode::PlainShell, r"D:\known-end", ended);
        assert!(e.end_time_known);
        let transcripts = [
            ("in-session", ended - TimeDelta::minutes(30)),
            ("in-grace", ended + TimeDelta::minutes(1)),
            ("hours-later", now - TimeDelta::hours(2)),
        ];

        assert_eq!(
            candidate_ids("items-known-end", &e, &transcripts),
            ["in-grace", "in-session"]
        );
    }
}
