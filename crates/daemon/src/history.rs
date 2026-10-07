//! Ended-session history, kept so a session can be recovered after it ends.
//!
//! Every end path (the child exiting, the user stopping or discarding a
//! session, a drained shutdown) writes one entry to
//! `<config>/history/<session-id>.json` before the session's `meta.json`
//! sidecar is deleted. The first write for a session wins, so a Stop followed
//! by the exit watcher keeps `StoppedByUser` rather than `Exited`. Headless
//! sessions are never written: a `claude --print` run has nothing to resume.
//! Startup prunes entries older than [`HISTORY_RETENTION`].

use crate::codex_rollout;
use crate::orphan;
use crate::paths::{Dirs, normalize_path_key};
use crate::pty::PtyExit;
use crate::session::{SessionRecord, SessionRegistry};
use crate::sync::lock;
use crate::tracer_log::{self, TracerLogEnd, TracerLogSummary};
use crate::transcripts;
use anyhow::{Context as _, anyhow};
use chrono::{DateTime, TimeDelta, Utc};
use protocol::{
    Agent, AgentOptions, ConversationCandidate, HistoryEntry, HistorySource, InjectorStartup,
    InjectorStep, PinnedMemberWorktree, PromptInjector, RecoverAs, RecoverItem, RepoEntry,
    SessionEnd, SessionHistoryItem, SessionKind, SessionMember, SessionMode, SpawnConfig,
    SpawnRequest, SpawnTarget, WorkspaceEntry, WorktreeReusePolicy,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use tokio::sync::watch;
use tracing::{info, warn};

/// How long an ended session stays in the history.
pub const HISTORY_RETENTION: TimeDelta = TimeDelta::days(7);

/// Most conversations offered for one history entry.
const CANDIDATE_LIMIT: usize = 5;
/// How long after a session's end its conversation may still have been written.
const CANDIDATE_GRACE: TimeDelta = TimeDelta::minutes(2);
/// How far before its end a session with no known start is searched from.
const UNKNOWN_START_LOOKBACK: TimeDelta = TimeDelta::days(1);
/// The longest a recovered shell waits for its prompt before typing
/// `claude --resume <id>`; it types sooner once the prompt has printed.
const SHELL_RESUME_DELAY_MS: u32 = 2000;
/// The tracer-log importer's revision, stamped on every entry it writes. An
/// unrecovered import from an older revision is imported again.
pub const IMPORT_REV: u32 = 1;
/// Program file stems an imported plain-shell session may have run.
const SHELL_STEMS: [&str; 6] = ["pwsh", "powershell", "cmd", "bash", "zsh", "sh"];
/// The branch name a recovered repo or workspace spawn asks for when its
/// session recorded none. Its folders are pinned, so nothing checks it out.
const PINNED_BRANCH_FALLBACK: &str = "HEAD";

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
    if let Some(first) = rec.members.first() {
        return Some(first.worktree_path.clone());
    }
    match rec.spawn_config.as_ref().map(|cfg| &cfg.target) {
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
        skip_permissions: None,
        model: None,
        end_time_known: true,
        import_rev: 0,
        agent_conversation_id: rec.agent_conversation_id.clone(),
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
    orphan::write_atomic(&entry_path(dirs, &entry.session_id), &bytes)?;
    CHANGES.send_modify(|n| *n = n.wrapping_add(1));
    Ok(())
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
pub fn record_session_end(
    registry: &SessionRegistry,
    dirs: &Dirs,
    session_id: &str,
    end: SessionEnd,
) {
    let Some(rec) = registry.get(session_id) else {
        return;
    };
    capture_codex_conversation(registry, dirs, session_id, &rec);
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

/// A last look for an interactive Codex session's rollout before its entry
/// is written, so a first message sent after the watch's last poll still
/// gives the entry its conversation id.
fn capture_codex_conversation(
    registry: &SessionRegistry,
    dirs: &Dirs,
    session_id: &str,
    rec: &Mutex<SessionRecord>,
) {
    let Some(look) = codex_rollout::uncaptured(&lock(rec)) else {
        return;
    };
    let scan = || {
        let Some(home) = codex_rollout::codex_home(&look.extra_env) else {
            return;
        };
        codex_rollout::capture_once(
            registry,
            dirs,
            session_id,
            &home,
            &look.cwd,
            look.since,
            &mut HashSet::new(),
        );
    };
    // The end paths run on the async workers and the scan's directory walk
    // blocks, so a multi-thread runtime is told before it runs.
    let on_worker = tokio::runtime::Handle::try_current()
        .is_ok_and(|handle| handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread);
    if on_worker {
        tokio::task::block_in_place(scan);
    } else {
        scan();
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
pub fn read_all(dirs: &Dirs) -> Vec<HistoryEntry> {
    let mut entries: Vec<HistoryEntry> = scan(dirs).into_iter().map(|(_, e)| e).collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.ended_at));
    entries
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

/// The registered repos and workspaces an imported session is matched against.
pub struct Registered<'a> {
    pub repos: &'a [RepoEntry],
    pub workspaces: &'a [WorkspaceEntry],
}

/// Import the sessions that ended before the daemon kept a history, from
/// their `<config>/logs/tracer-<id>.log` files modified within
/// [`HISTORY_RETENTION`] of `now`. A session in `skip` (a live or abandoned
/// sidecar) is left alone, and so is one already in the history unless it is
/// an unrecovered import from before [`IMPORT_REV`], which is imported again.
/// A log naming no program or one that is neither Claude nor a known shell is
/// skipped. A session with no end line in the daemon logs gets the log's
/// modified time as `ended_at` and `end_time_known: false`. Returns how many
/// entries were written.
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
/// or it holds an unrecovered import from before [`IMPORT_REV`]. A recorded
/// entry, a recovered one, a current import and an unreadable file are kept.
fn is_importable(path: &Path) -> bool {
    if !path.exists() {
        return true;
    }
    match read_entry(path) {
        Ok(existing) => {
            existing.source == HistorySource::TracerLog
                && existing.import_rev < IMPORT_REV
                && existing.recovered_at.is_none()
        }
        Err(err) => {
            warn!(?err, "keeping unreadable session history file");
            false
        }
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

/// Each session's earliest end time in `daemon.log` and `daemon.log.old`.
fn daemon_end_times(logs_dir: &Path) -> HashMap<String, DateTime<Utc>> {
    let mut merged: HashMap<String, DateTime<Utc>> = HashMap::new();
    for name in ["daemon.log", "daemon.log.old"] {
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

/// Every `--add-dir <path>` value in `args`, in order.
fn add_dir_args(args: &[String]) -> Vec<String> {
    args.windows(2)
        .filter(|pair| pair[0] == "--add-dir")
        .map(|pair| pair[1].clone())
        .collect()
}

fn end_from_log(end: TracerLogEnd) -> SessionEnd {
    match end {
        TracerLogEnd::StoppedByUser => SessionEnd::StoppedByUser,
        TracerLogEnd::Exited { code } => SessionEnd::Exited { code },
        TracerLogEnd::Lost => SessionEnd::TracerLost,
    }
}

/// The history entry a tracer log describes. Claude sessions are matched to
/// the workspace (cwd its first member, `--add-dir`s its other members) or
/// the repo (cwd its path, no `--add-dir`) they ran in, but never get a
/// `spawn_config`: the log doesn't say how the session was spawned. Recovery
/// runs Claude in the same folders, under the matched workspace or repo with
/// each member pinned to its folder, else with the same `--add-dir`s.
fn entry_from_tracer_log(
    id: &str,
    summary: &TracerLogSummary,
    end: ImportEnd,
    registered: &Registered<'_>,
) -> Option<HistoryEntry> {
    let stem = program_stem(&summary.program)?;
    let is_claude = stem.starts_with("claude");
    if !is_claude && !SHELL_STEMS.contains(&stem.as_str()) {
        return None;
    }
    let cwd = summary.cwd.clone();
    let (mode, program_name, kind, workspace_id, members) = if is_claude {
        let add_dirs = add_dir_args(&summary.args);
        let (kind, workspace_id) = match_target(&cwd, &add_dirs, registered);
        let members = folder_members(&cwd, &add_dirs, registered.repos);
        let name = "claude".to_owned();
        (SessionMode::Interactive, name, kind, workspace_id, members)
    } else {
        let kind = SessionKind::Standalone;
        (SessionMode::PlainShell, stem, kind, None, Vec::new())
    };
    Some(HistoryEntry {
        session_id: id.to_owned(),
        label: String::new(),
        kind,
        mode,
        agent: Agent::Claude,
        spawn_config: None,
        members,
        workspace_id,
        primary_cwd: Some(cwd.clone()),
        current_cwd: (!is_claude).then_some(cwd),
        program_name: Some(program_name),
        started_at: Some(summary.started_at),
        ended_at: end.at,
        end: end_from_log(summary.end),
        claude_session_id: None,
        source: HistorySource::TracerLog,
        recovered_at: None,
        skip_permissions: is_claude.then(|| {
            summary
                .args
                .iter()
                .any(|a| a == "--dangerously-skip-permissions")
        }),
        model: if is_claude {
            model_arg(&summary.args)
        } else {
            None
        },
        end_time_known: end.known,
        import_rev: IMPORT_REV,
        agent_conversation_id: None,
    })
}

/// The value of the last `--model <m>` or `--model=<m>` in `args`.
fn model_arg(args: &[String]) -> Option<String> {
    let mut model = None;
    for (i, arg) in args.iter().enumerate() {
        if let Some(value) = arg.strip_prefix("--model=") {
            model = Some(value.to_owned());
        } else if arg == "--model"
            && let Some(value) = args.get(i + 1)
        {
            model = Some(value.clone());
        }
    }
    model.filter(|m| !m.is_empty())
}

fn repo_at<'a>(repos: &'a [RepoEntry], path: &str) -> Option<&'a RepoEntry> {
    let key = normalize_path_key(path);
    repos.iter().find(|r| normalize_path_key(&r.path) == key)
}

/// The workspace a Claude session in `cwd` with `add_dirs` ran for, else the
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

/// The folder a history entry is about: the shell's last folder, else where
/// the session started, else its first member's worktree.
#[must_use]
pub fn entry_folder(entry: &HistoryEntry) -> Option<&str> {
    entry
        .current_cwd
        .as_deref()
        .or(entry.primary_cwd.as_deref())
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
/// first, with the conversations it may be recovered into and what its folder
/// is. `claude_home` is `None` when Claude Code's home can't be found, and
/// then no Claude entry has candidates. `codex_home` gives a Codex entry's
/// home from its recorded environment rows.
#[must_use]
pub fn history_items(
    dirs: &Dirs,
    repos: &[RepoEntry],
    claude_home: Option<&Path>,
    codex_home: impl Fn(&[(String, String)]) -> Option<PathBuf>,
) -> Vec<SessionHistoryItem> {
    let now = Utc::now();
    read_all(dirs)
        .into_iter()
        .filter(|entry| entry.mode != SessionMode::Headless)
        .map(|entry| history_item(entry, repos, claude_home, &codex_home, now))
        .collect()
}

fn history_item(
    entry: HistoryEntry,
    repos: &[RepoEntry],
    claude_home: Option<&Path>,
    codex_home: &impl Fn(&[(String, String)]) -> Option<PathBuf>,
    now: DateTime<Utc>,
) -> SessionHistoryItem {
    let folder = entry_folder(&entry).map(str::to_owned);
    // A Codex or Cursor session recovers as its own agent, never into a
    // Claude conversation.
    let candidates = match (entry.agent, claude_home, folder.as_deref()) {
        (Agent::Claude, Some(home), Some(folder)) => {
            conversation_candidates(&entry, home, folder, now)
        }
        _ => Vec::new(),
    };
    let own_agent_resumable = own_agent_resumable(&entry, codex_home);
    let folder_is_git_repo = folder
        .as_deref()
        .is_some_and(|folder| Path::new(folder).join(".git").exists());
    let folder_repo_id = folder
        .as_deref()
        .and_then(|folder| repo_at(repos, folder))
        .map(|repo| repo.id.clone());
    SessionHistoryItem {
        entry,
        candidates,
        folder_is_git_repo,
        folder_repo_id,
        own_agent_resumable,
    }
}

/// Whether recovering `entry` as its own agent resumes its conversation: a
/// Codex entry whose recorded rollout is still under its Codex home, or a
/// Cursor entry with a recorded chat.
fn own_agent_resumable(
    entry: &HistoryEntry,
    codex_home: &impl Fn(&[(String, String)]) -> Option<PathBuf>,
) -> bool {
    let Some(id) = entry.agent_conversation_id.as_deref() else {
        return false;
    };
    match entry.agent {
        Agent::Claude => false,
        Agent::Codex => codex_home(recorded_env(entry))
            .is_some_and(|home| codex_rollout::rollout_exists(&home, id)),
        Agent::Cursor => true,
    }
}

/// The entry's own conversation when it knows one and its transcript is
/// still there; otherwise the transcripts written in `folder` while the
/// session ran, up to `now` when its end time is unknown.
fn conversation_candidates(
    entry: &HistoryEntry,
    claude_home: &Path,
    folder: &str,
    now: DateTime<Utc>,
) -> Vec<ConversationCandidate> {
    let found = if let Some(id) = &entry.claude_session_id {
        transcripts::known_conversation(claude_home, folder, id)
            .into_iter()
            .collect()
    } else {
        let (start, end) = candidate_window(entry, now);
        transcripts::candidates(claude_home, folder, start, end, CANDIDATE_LIMIT)
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
/// when set, then spawn `request`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryPlan {
    pub request: SpawnRequest,
    pub register_repo: Option<String>,
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
/// worktree. A Claude session recovers as Claude (or a shell); a Codex or
/// Cursor session only as its own agent. `conversation_exists(agent, folder,
/// id)` says whether `agent` still holds conversation `id` for `folder`. The
/// error is the message the client shows for the item.
pub fn plan_recovery(
    entry: &HistoryEntry,
    item: &RecoverItem,
    registered: &Registered<'_>,
    conversation_exists: impl Fn(Agent, &str, &str) -> bool,
) -> Result<RecoveryPlan, String> {
    if entry.recovered_at.is_some() {
        return Err("already recovered".to_owned());
    }
    if entry.mode == SessionMode::Headless {
        return Err("a headless session can't be recovered".to_owned());
    }
    if item.how == RecoverAs::Unknown {
        return Err("unsupported recovery kind".to_owned());
    }
    let folder = entry_folder(entry).unwrap_or_default();
    if entry.agent != Agent::Claude || item.how == RecoverAs::OwnAgent {
        return own_agent_plan(entry, item, folder, registered, conversation_exists);
    }
    if let Some(id) = &item.conversation_id
        && !conversation_exists(Agent::Claude, folder, id)
    {
        return Err(format!("conversation {id} not found for {folder}"));
    }
    let plan = match &item.how {
        RecoverAs::Claude => RecoveryPlan {
            request: claude_request(entry, folder, required_conversation(item)?, registered),
            register_repo: None,
        },
        RecoverAs::RegisterRepoThenClaude { path } => {
            if normalize_path_key(path) != normalize_path_key(folder) {
                return Err(format!("{path} is not the session's folder {folder}"));
            }
            let conversation = required_conversation(item)?;
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
                request: folder_claude(entry, target, conversation),
                register_repo: Some(path.clone()),
            }
        }
        RecoverAs::Shell => RecoveryPlan {
            request: shell_request(folder, item.conversation_id.as_deref()),
            register_repo: None,
        },
        RecoverAs::OwnAgent | RecoverAs::Unknown => {
            return Err("unsupported recovery kind".to_owned());
        }
    };
    Ok(plan)
}

/// A Codex or Cursor `entry` recovered as its own agent from its recorded
/// spawn settings, pinned like a Claude recovery: resuming the item's
/// conversation, which must be the one the entry recorded and still exist,
/// else a fresh run with no first prompt. Refuses every other pairing of
/// agent and kind.
fn own_agent_plan(
    entry: &HistoryEntry,
    item: &RecoverItem,
    folder: &str,
    registered: &Registered<'_>,
    conversation_exists: impl Fn(Agent, &str, &str) -> bool,
) -> Result<RecoveryPlan, String> {
    if entry.agent == Agent::Claude || item.how != RecoverAs::OwnAgent {
        let name = agent_name(entry.agent);
        return Err(format!("a {name} session recovers as {name}"));
    }
    let Some(config) = &entry.spawn_config else {
        return Err("no spawn settings recorded".to_owned());
    };
    if let Some(id) = item.conversation_id.as_deref() {
        if entry.agent_conversation_id.as_deref() != Some(id) {
            return Err("not this session's conversation".to_owned());
        }
        if !conversation_exists(entry.agent, folder, id) {
            return Err(format!("conversation {id} not found for {folder}"));
        }
    }
    let mut request = recorded_request(entry, config, folder, registered);
    request
        .resume_conversation
        .clone_from(&item.conversation_id);
    Ok(RecoveryPlan {
        request,
        register_repo: None,
    })
}

/// The agent's name as a recovery refusal says it.
fn agent_name(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "Claude",
        Agent::Codex => "Codex",
        Agent::Cursor => "Cursor",
    }
}

fn required_conversation(item: &RecoverItem) -> Result<&str, String> {
    item.conversation_id
        .as_deref()
        .ok_or_else(|| "no conversation to resume".to_owned())
}

/// Claude resuming `conversation` where the session ran: its own spawn config
/// when it has one, with every repo member pinned to its recorded folder;
/// else the registered workspace or repo its folders are, pinned the same
/// way; else its folder with its other members as `--add-dir`s.
fn claude_request(
    entry: &HistoryEntry,
    folder: &str,
    conversation: &str,
    registered: &Registered<'_>,
) -> SpawnRequest {
    let Some(config) = &entry.spawn_config else {
        let members = entry_members(entry, folder, registered.repos);
        let target = folder_target(entry, &members, registered);
        return folder_claude(entry, target, conversation);
    };
    let mut request = recorded_request(entry, config, folder, registered);
    // A plain shell's record says Claude while its stored config may carry
    // the spawn dialog's options for another agent.
    if request.mode == SessionMode::PlainShell {
        request.agent_options = AgentOptions::Claude {
            permission_mode: None,
        };
    }
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
fn pin_recorded(
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

/// Claude resuming `conversation` in `target`, with the permission and model
/// flags `entry` recorded.
fn folder_claude(entry: &HistoryEntry, target: SpawnTarget, conversation: &str) -> SpawnRequest {
    SpawnRequest {
        label: None,
        target,
        mode: SessionMode::Interactive,
        initial_prompt: None,
        dangerously_skip_permissions: entry.skip_permissions.unwrap_or(false),
        agent_options: AgentOptions::Claude {
            permission_mode: None,
        },
        model: entry.model.clone(),
        extra_env: Vec::new(),
        prompt_injector: None,
        request_id: None,
        resume_conversation: Some(conversation.to_owned()),
    }
}

/// A plain shell in `folder` that types `claude --resume <id>` once it is up,
/// when a conversation was chosen.
fn shell_request(folder: &str, conversation: Option<&str>) -> SpawnRequest {
    let prompt_injector = conversation.map(|id| PromptInjector {
        steps: vec![
            InjectorStep::Delay {
                ms: SHELL_RESUME_DELAY_MS,
            },
            InjectorStep::Text {
                content: format!("claude --resume {id}"),
                newline: true,
            },
        ],
        verify_mode_marker: None,
        startup: InjectorStartup::ShellPrompt,
    });
    SpawnRequest {
        label: None,
        target: SpawnTarget::Standalone {
            cwd: (!folder.is_empty()).then(|| folder.to_owned()),
            add_dirs: Vec::new(),
        },
        mode: SessionMode::PlainShell,
        initial_prompt: None,
        dangerously_skip_permissions: false,
        agent_options: AgentOptions::Claude {
            permission_mode: None,
        },
        model: None,
        extra_env: Vec::new(),
        prompt_injector,
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

    /// Hold the history write lock until the returned guard drops. While a
    /// test holds it, an end path that comes after the lock is parked, so the
    /// test decides which of two competing writes lands first.
    pub fn hold_history_write_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::sync::lock(&super::WRITE_LOCK)
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
            status_since: Some(Utc::now()),
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
            agent_conversation_id: None,
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
            Agent::Claude,
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
    fn entry_from_record_copies_agent_conversation_id() {
        let mut rec = record("s1", SessionMode::Interactive);
        rec.agent = Agent::Codex;
        rec.agent_conversation_id = Some("019a2b3c-codex".to_owned());
        let entry = entry_from_record(&rec, SessionEnd::TracerLost, Utc::now());
        assert_eq!(
            entry.agent_conversation_id.as_deref(),
            Some("019a2b3c-codex")
        );
        assert_eq!(entry.claude_session_id, None);
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
    fn codex_session_end_writes_its_rollout_id_to_the_entry() {
        let dirs = scratch_dirs("codex-final-scan");
        let home = dirs.config.join("codex-home");
        let cwd = r"X:\dev\codex-final-scan";
        let id = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a60";
        let now = Utc::now();
        let local = now.with_timezone(&chrono::Local).naive_local();
        let day = home
            .join("sessions")
            .join(local.format("%Y").to_string())
            .join(local.format("%m").to_string())
            .join(local.format("%d").to_string());
        std::fs::create_dir_all(&day).expect("create day folder");
        let first_line = serde_json::json!({
            "type": "session_meta",
            "payload": { "id": id, "session_id": id, "cwd": cwd, "source": "cli" },
        });
        let name = format!("rollout-{}-{id}.jsonl", local.format("%Y-%m-%dT%H-%M-%S"));
        std::fs::write(day.join(name), format!("{first_line}\n")).expect("write rollout");

        let mut rec = record("c1", SessionMode::Interactive);
        rec.agent = Agent::Codex;
        rec.started_at = now - TimeDelta::minutes(1);
        rec.spawn_config = Some(protocol::SpawnConfig {
            target: SpawnTarget::Standalone {
                cwd: Some(cwd.to_owned()),
                add_dirs: Vec::new(),
            },
            mode: SessionMode::Interactive,
            dangerously_skip_permissions: false,
            agent_options: AgentOptions::Codex { sandbox: None },
            model: None,
            extra_env: vec![("CODEX_HOME".to_owned(), home.to_string_lossy().into_owned())],
        });
        let registry = SessionRegistry::new(dirs.clone());
        registry.insert(rec);

        record_session_end(&registry, &dirs, "c1", SessionEnd::StoppedByUser);

        let entry = read_one(&dirs, "c1").expect("history entry");
        assert_eq!(entry.agent_conversation_id.as_deref(), Some(id));
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
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod import_tests {
    use super::test_support::scratch_dirs;
    use super::*;
    use serde_json::json;
    use std::fs::File;
    use std::time::SystemTime;

    const YAAT_ID: &str = "f6216fc2-54b1-497c-a257-7c297f7d9859";
    const YAAT_LOG: [&str; 3] = [
        r"2026-09-27T11:09:41.935397Z  INFO rt_tracer: rt-tracer starting session_id=f6216fc2-54b1-497c-a257-7c297f7d9859 cwd=D:\yaat cols=120 rows=32 argc=6",
        r#"2026-09-27T11:09:41.940609Z  INFO rt_tracer::supervisor: supervisor: about to spawn child program=C:\Users\lefto\.local\bin\claude.exe argc=5 args=["--add-dir", "D:\\yaat-server", "--append-system-prompt", "Workspace member paths for this session:\n  yaat         ->  D:\\yaat\n  yaat-server  ->  D:\\yaat-server\n", "--dangerously-skip-permissions"] cwd=D:\yaat"#,
        r"2026-09-27T11:09:41.982317Z  INFO rt_tracer::supervisor: supervisor: client connected iteration=1 session_id=f6216fc2-54b1-497c-a257-7c297f7d9859",
    ];
    const STOP_ID: &str = "22b5a96d-8d81-40e3-a594-498fc639c7cc";
    const STOP_LOG: [&str; 3] = [
        r"2026-09-26T03:53:21.116126Z  INFO rt_tracer: rt-tracer starting session_id=22b5a96d-8d81-40e3-a594-498fc639c7cc cwd=D:\rustling-tulip cols=120 rows=32 argc=2",
        r#"2026-09-26T03:53:21.120000Z  INFO rt_tracer::supervisor: supervisor: about to spawn child program=C:\Users\lefto\.local\bin\claude.exe argc=1 args=["--dangerously-skip-permissions"] cwd=D:\rustling-tulip"#,
        r"2026-09-26T05:09:44.845855Z  INFO rt_tracer::supervisor: supervisor: Stop request received",
    ];
    const PWSH_ID: &str = "0b8f4c1e-1111-4222-8333-944455556666";
    const PWSH_LOG: [&str; 3] = [
        r"2026-09-27T08:00:00.000000Z  INFO rt_tracer: rt-tracer starting session_id=0b8f4c1e-1111-4222-8333-944455556666 cwd=D:\ cols=120 rows=32 argc=4",
        r#"2026-09-27T08:00:00.010000Z  INFO rt_tracer::supervisor: supervisor: about to spawn child program=pwsh.exe argc=3 args=["-NoExit", "-Command", "$global:__rt_original_prompt = $function:prompt; function global:prompt { \"PS $($PWD.Path)> \" }"] cwd=D:\"#,
        r"2026-09-27T08:05:00.000000Z  INFO rt_tracer::supervisor: supervisor: child exited; shutting down exit_code=0",
    ];

    fn ts(stamp: &str) -> DateTime<Utc> {
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

    fn write_log(dirs: &Dirs, name: &str, lines: &[&str]) -> PathBuf {
        let logs = dirs.config.join("logs");
        std::fs::create_dir_all(&logs).expect("create logs dir");
        let path = logs.join(name);
        std::fs::write(&path, lines.join("\n")).expect("write log");
        path
    }

    fn set_mtime(path: &Path, at: DateTime<Utc>) {
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
        assert_eq!(entry.agent, Agent::Claude);
        assert_eq!(entry.program_name.as_deref(), Some("claude"));
        assert_eq!(entry.primary_cwd.as_deref(), Some(r"D:\yaat"));
        assert_eq!(entry.current_cwd, None);
        assert_eq!(entry.started_at, Some(ts("2026-09-27T11:09:41.935397Z")));
        assert_eq!(entry.end, SessionEnd::TracerLost);
        assert_eq!(entry.source, HistorySource::TracerLog);
        assert_eq!(entry.label, "");
        assert_eq!(entry.claude_session_id, None);
        assert_eq!(entry.skip_permissions, Some(true));
        assert_eq!(entry.model, None);
        let _ = std::fs::remove_dir_all(&dirs.config);
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
                Some(false),
                Some("opus"),
            ),
            (
                "aaaaaaaa-0000-4000-8000-000000000002",
                r#"["--model=sonnet", "--dangerously-skip-permissions"]"#,
                Some(true),
                Some("sonnet"),
            ),
            (
                "aaaaaaaa-0000-4000-8000-000000000003",
                "[]",
                Some(false),
                None,
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
            assert_eq!(entry.skip_permissions, skip, "{id}");
            assert_eq!(entry.model.as_deref(), model, "{id}");
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
        assert_eq!(entry.skip_permissions, None);
        assert_eq!(entry.model, None);
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
        assert_eq!(entry.import_rev, IMPORT_REV);
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
        assert_eq!(entry.import_rev, IMPORT_REV);
        assert_eq!(
            entry.label, "",
            "the importer's entry replaced the stale one"
        );
        assert!(!entry.end_time_known);
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
        let current = imported(STOP_ID, IMPORT_REV, "current");
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
    fn codex_and_programless_logs_are_skipped() {
        let dirs = scratch_dirs("import-codex");
        let codex_id = "aaaaaaaa-1111-4222-8333-944455556666";
        write_log(
            &dirs,
            &format!("tracer-{codex_id}.log"),
            &[
                &format!(
                    "2026-09-27T08:00:00.000000Z  INFO rt_tracer: rt-tracer starting session_id={codex_id} cwd=D:\\yaat cols=120 rows=32 argc=1"
                ),
                r#"2026-09-27T08:00:00.010000Z  INFO rt_tracer::supervisor: supervisor: about to spawn child program=C:\bin\codex.exe argc=1 args=["--yolo"] cwd=D:\yaat"#,
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
    fn program_stems_and_add_dirs_are_read_from_any_path_form() {
        assert_eq!(
            program_stem(r"C:\Users\x\.local\bin\claude.exe").as_deref(),
            Some("claude")
        );
        assert_eq!(program_stem("/usr/bin/bash").as_deref(), Some("bash"));
        assert_eq!(program_stem("PWSH.EXE").as_deref(), Some("pwsh"));
        assert_eq!(program_stem(""), None);
        let args: Vec<String> = ["--add-dir", "a", "-x", "--add-dir", "b", "--add-dir"]
            .map(str::to_owned)
            .into();
        assert_eq!(add_dir_args(&args), ["a", "b"]);
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod recovery_tests {
    use super::test_support::{record, scratch_dirs};
    use super::*;
    use protocol::{SpawnConfig, WorktreeReusePolicy};
    use serde_json::json;
    use std::fs::File;
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
            agent_options: AgentOptions::Claude {
                permission_mode: None,
            },
            model: Some("opus".to_owned()),
            extra_env: Vec::new(),
        }
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
        plan_recovery(entry, item, &registered, |_, _, _| true)
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
        for how in [RecoverAs::Claude, RecoverAs::Shell] {
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
            &item(RecoverAs::Shell, Some("gone")),
            &Registered {
                repos: &[],
                workspaces: &[],
            },
            |agent, folder, id| {
                asked
                    .borrow_mut()
                    .push((agent, folder.to_owned(), id.to_owned()));
                false
            },
        );
        assert_eq!(
            result,
            Err(r"conversation gone not found for D:\yaat".to_owned())
        );
        assert_eq!(
            *asked.borrow(),
            [(Agent::Claude, r"D:\yaat".to_owned(), "gone".to_owned())]
        );
    }

    #[test]
    fn claude_needs_a_conversation() {
        let entry = folder_only(vec![member("", r"D:\yaat")]);
        assert_eq!(
            plan(&entry, &item(RecoverAs::Claude, None)),
            Err("no conversation to resume".to_owned())
        );
        let register = RecoverAs::RegisterRepoThenClaude {
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
            &item(RecoverAs::Claude, Some(CONV)),
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
        assert_eq!(req.agent(), Agent::Claude);
        assert!(req.dangerously_skip_permissions);
        assert_eq!(req.model.as_deref(), Some("opus"));
        assert_eq!(req.initial_prompt, None);
        assert_eq!(req.prompt_injector, None);
    }

    #[test]
    fn claude_from_a_shell_config_is_forced_to_interactive_claude() {
        let mut entry = folder_only(vec![member("r1", r"D:\repo")]);
        let mut config = workspace_config();
        config.mode = SessionMode::PlainShell;
        config.agent_options = AgentOptions::Codex { sandbox: None };
        entry.spawn_config = Some(config);

        let req = plan(&entry, &item(RecoverAs::Claude, Some(CONV)))
            .expect("plan")
            .request;

        assert_eq!(req.mode, SessionMode::Interactive);
        assert_eq!(req.agent(), Agent::Claude);
        assert_eq!(req.resume_conversation.as_deref(), Some(CONV));
    }

    const CODEX_ID: &str = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
    const CODEX_FOLDER: &str = r"C:\wt\repo";

    fn codex_config() -> SpawnConfig {
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
            agent_options: AgentOptions::Codex {
                sandbox: Some(protocol::CodexSandbox::WorkspaceWrite),
            },
            model: Some("gpt-5-codex".to_owned()),
            extra_env: vec![("CODEX_HOME".to_owned(), r"D:\codex-home".to_owned())],
        }
    }

    /// A Codex session in one repo's worktree, with its spawn config and
    /// the conversation id `id` when one was captured.
    fn codex_entry(id: Option<&str>) -> HistoryEntry {
        let mut entry = folder_only(vec![member("r1", CODEX_FOLDER)]);
        entry.agent = Agent::Codex;
        entry.agent_conversation_id = id.map(str::to_owned);
        entry.spawn_config = Some(codex_config());
        entry
    }

    fn codex_plan(entry: &HistoryEntry, item: &RecoverItem) -> Result<RecoveryPlan, String> {
        plan_in(entry, item, &[repo("r1", r"D:\repo")], &[])
    }

    #[test]
    fn claude_recovery_of_a_removed_single_repo_entry_runs_standalone() {
        let mut entry = folder_only(vec![member("r1", CODEX_FOLDER)]);
        entry.spawn_config = Some(SpawnConfig {
            agent_options: AgentOptions::Claude {
                permission_mode: None,
            },
            ..codex_config()
        });
        let recover = item(RecoverAs::Claude, Some(CONV));

        let removed = plan(&entry, &recover).expect("plan").request;
        assert_eq!(
            removed.target,
            SpawnTarget::Standalone {
                cwd: Some(CODEX_FOLDER.to_owned()),
                add_dirs: Vec::new(),
            }
        );
        assert_eq!(removed.agent(), Agent::Claude);
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
    fn own_agent_resumes_a_codex_entry_pinned_to_its_folder() {
        let entry = codex_entry(Some(CODEX_ID));

        let plan = codex_plan(&entry, &item(RecoverAs::OwnAgent, Some(CODEX_ID))).expect("plan");

        assert_eq!(plan.register_repo, None);
        let req = plan.request;
        assert_eq!(
            req.target,
            SpawnTarget::Single {
                repo_id: "r1".to_owned(),
                branch_name: "feat/x".to_owned(),
                base_branch: Some("main".to_owned()),
                use_worktree: true,
                checkout_strategy: None,
                worktree_reuse: WorktreeReusePolicy::Reuse,
                existing_worktree: Some(CODEX_FOLDER.to_owned()),
            }
        );
        assert_every_member_pinned(&req.target, 1);
        assert_eq!(req.agent_options, codex_config().agent_options);
        assert_eq!(req.mode, SessionMode::Interactive);
        assert_eq!(req.model.as_deref(), Some("gpt-5-codex"));
        assert!(!req.dangerously_skip_permissions);
        assert_eq!(req.extra_env, codex_config().extra_env);
        assert_eq!(req.resume_conversation.as_deref(), Some(CODEX_ID));
        assert_eq!(req.initial_prompt, None);
        assert_eq!(req.prompt_injector, None);
    }

    #[test]
    fn own_agent_without_id_is_a_fresh_run() {
        for recorded in [None, Some(CODEX_ID)] {
            let entry = codex_entry(recorded);
            let req = plan_recovery(
                &entry,
                &item(RecoverAs::OwnAgent, None),
                &Registered {
                    repos: &[repo("r1", r"D:\repo")],
                    workspaces: &[],
                },
                |_, _, _| false,
            )
            .expect("plan")
            .request;

            assert_eq!(req.agent(), Agent::Codex);
            assert_eq!(req.resume_conversation, None, "recorded {recorded:?}");
            assert_eq!(req.initial_prompt, None);
            assert_eq!(req.prompt_injector, None);
            assert_every_member_pinned(&req.target, 1);
        }
    }

    #[test]
    fn own_agent_with_another_id_fails() {
        for recorded in [None, Some(CODEX_ID)] {
            assert_eq!(
                codex_plan(
                    &codex_entry(recorded),
                    &item(RecoverAs::OwnAgent, Some("0199ffff-other"))
                ),
                Err("not this session's conversation".to_owned()),
                "recorded {recorded:?}"
            );
        }
    }

    #[test]
    fn own_agent_with_a_gone_rollout_fails() {
        let entry = codex_entry(Some(CODEX_ID));
        let result = plan_recovery(
            &entry,
            &item(RecoverAs::OwnAgent, Some(CODEX_ID)),
            &Registered {
                repos: &[],
                workspaces: &[],
            },
            |_, _, _| false,
        );
        assert_eq!(
            result,
            Err(format!(
                "conversation {CODEX_ID} not found for {CODEX_FOLDER}"
            ))
        );
    }

    #[test]
    fn own_agent_without_spawn_config_fails() {
        let mut entry = codex_entry(Some(CODEX_ID));
        entry.spawn_config = None;
        for conversation in [None, Some(CODEX_ID)] {
            assert_eq!(
                codex_plan(&entry, &item(RecoverAs::OwnAgent, conversation)),
                Err("no spawn settings recorded".to_owned())
            );
        }
    }

    #[test]
    fn own_agent_on_a_claude_entry_fails() {
        let mut entry = folder_only(vec![member("r1", CODEX_FOLDER)]);
        entry.spawn_config = Some(workspace_config());
        for conversation in [None, Some(CONV)] {
            assert_eq!(
                plan(&entry, &item(RecoverAs::OwnAgent, conversation)),
                Err("a Claude session recovers as Claude".to_owned())
            );
        }
    }

    #[test]
    fn a_codex_entry_refuses_claude_recovery() {
        let register = RecoverAs::RegisterRepoThenClaude {
            path: CODEX_FOLDER.to_owned(),
        };
        for how in [RecoverAs::Claude, register, RecoverAs::Shell] {
            assert_eq!(
                codex_plan(&codex_entry(Some(CODEX_ID)), &item(how.clone(), Some(CONV))),
                Err("a Codex session recovers as Codex".to_owned()),
                "{how:?}"
            );
            let mut cursor = codex_entry(None);
            cursor.agent = Agent::Cursor;
            assert_eq!(
                codex_plan(&cursor, &item(how.clone(), Some(CONV))),
                Err("a Cursor session recovers as Cursor".to_owned()),
                "{how:?}"
            );
        }
    }

    #[test]
    fn a_headless_entry_is_refused() {
        let register = RecoverAs::RegisterRepoThenClaude {
            path: CODEX_FOLDER.to_owned(),
        };
        let mut claude = folder_only(vec![member("r1", CODEX_FOLDER)]);
        claude.mode = SessionMode::Headless;
        let mut codex = codex_entry(Some(CODEX_ID));
        codex.mode = SessionMode::Headless;
        for entry in [&claude, &codex] {
            for how in [
                RecoverAs::Claude,
                register.clone(),
                RecoverAs::Shell,
                RecoverAs::OwnAgent,
                RecoverAs::Unknown,
            ] {
                assert_eq!(
                    codex_plan(entry, &item(how.clone(), Some(CODEX_ID))),
                    Err("a headless session can't be recovered".to_owned()),
                    "{:?} {how:?}",
                    entry.agent
                );
            }
        }
    }

    #[test]
    fn own_agent_is_planned_before_any_claude_transcript_lookup() {
        let asked = std::cell::RefCell::new(Vec::new());
        let lookup = |agent: Agent, folder: &str, id: &str| {
            asked
                .borrow_mut()
                .push((agent, folder.to_owned(), id.to_owned()));
            true
        };
        let registered = Registered {
            repos: &[],
            workspaces: &[],
        };
        let entry = codex_entry(Some(CODEX_ID));

        plan_recovery(
            &entry,
            &item(RecoverAs::OwnAgent, Some(CODEX_ID)),
            &registered,
            lookup,
        )
        .expect("plan");
        let refused = plan_recovery(
            &entry,
            &item(RecoverAs::Claude, Some(CONV)),
            &registered,
            lookup,
        );

        assert!(refused.is_err());
        assert_eq!(
            *asked.borrow(),
            [(Agent::Codex, CODEX_FOLDER.to_owned(), CODEX_ID.to_owned())],
            "only the Codex rollout is looked up"
        );
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

        let req = plan_in(&entry, &item(RecoverAs::Claude, Some(CONV)), &repos, &grown)
            .expect("plan")
            .request;

        assert_eq!(
            req.target,
            SpawnTarget::Standalone {
                cwd: Some(r"C:\wt\yaat".to_owned()),
                add_dirs: vec![r"C:\wt\yaat-server".to_owned()],
            }
        );
        assert_eq!(
            req.model.as_deref(),
            Some("opus"),
            "the config's flags stay"
        );
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
            &item(RecoverAs::Claude, Some(CONV)),
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
                &item(RecoverAs::Claude, Some(CONV)),
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

        let req = plan_in(&entry, &item(RecoverAs::Claude, Some(CONV)), &repos, &[])
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

        let plan =
            plan_in(&entry, &item(RecoverAs::Claude, Some(CONV)), &repos, &[]).expect("plan");

        assert_eq!(plan.register_repo, None);
        assert_eq!(
            plan.request.target,
            SpawnTarget::Standalone {
                cwd: Some(r"D:\yaat".to_owned()),
                add_dirs: vec![r"D:\yaat-server".to_owned()],
            }
        );
        assert_eq!(plan.request.mode, SessionMode::Interactive);
        assert_eq!(plan.request.agent(), Agent::Claude);
        assert_eq!(plan.request.resume_conversation.as_deref(), Some(CONV));
        assert!(!plan.request.dangerously_skip_permissions);
    }

    #[test]
    fn folder_only_and_register_paths_replay_the_recorded_flags() {
        let mut entry = folder_only(vec![member("", r"D:\foo")]);
        entry.skip_permissions = Some(true);
        entry.model = Some("opus".to_owned());
        let register = RecoverAs::RegisterRepoThenClaude {
            path: r"D:\foo".to_owned(),
        };
        for how in [RecoverAs::Claude, register] {
            let req = plan(&entry, &item(how.clone(), Some(CONV)))
                .expect("plan")
                .request;
            assert!(req.dangerously_skip_permissions, "{how:?}");
            assert_eq!(req.model.as_deref(), Some("opus"), "{how:?}");
        }
    }

    #[test]
    fn folder_only_claude_without_members_runs_in_the_folder() {
        let mut entry = folder_only(Vec::new());
        entry.primary_cwd = Some(r"D:\scratch".to_owned());

        let req = plan(&entry, &item(RecoverAs::Claude, Some(CONV)))
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
        let how = RecoverAs::RegisterRepoThenClaude {
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
    fn register_repo_then_claude_runs_pinned_in_the_new_repo() {
        let entry = folder_only(vec![imported("", r"D:\foo")]);
        let how = RecoverAs::RegisterRepoThenClaude {
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
            &item(RecoverAs::Claude, Some(CONV)),
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
            &item(RecoverAs::Claude, Some(CONV)),
            &repos,
            &workspaces,
        )
        .expect("plan");
        assert_eq!(kept.placeholder_branch_folder(), None);
        let standalone = plan(&entry, &item(RecoverAs::Claude, Some(CONV))).expect("plan");
        assert_eq!(standalone.placeholder_branch_folder(), None);
    }

    #[test]
    fn register_repo_then_claude_refuses_another_folder() {
        let entry = folder_only(vec![member("", r"D:\foo")]);
        let how = RecoverAs::RegisterRepoThenClaude {
            path: r"D:\elsewhere".to_owned(),
        };
        assert_eq!(
            plan(&entry, &item(how, Some(CONV))),
            Err(r"D:\elsewhere is not the session's folder D:\foo".to_owned())
        );
    }

    #[test]
    fn shell_types_the_resume_command_in_the_last_folder() {
        let mut entry = folder_only(vec![member("", r"D:\yaat")]);
        entry.current_cwd = Some(r"D:\yaat\src".to_owned());
        entry.spawn_config = Some(workspace_config());

        let req = plan(&entry, &item(RecoverAs::Shell, Some(CONV)))
            .expect("plan")
            .request;

        assert_eq!(req.mode, SessionMode::PlainShell);
        assert_eq!(
            req.target,
            SpawnTarget::Standalone {
                cwd: Some(r"D:\yaat\src".to_owned()),
                add_dirs: Vec::new(),
            }
        );
        assert_eq!(req.resume_conversation, None);
        let injector = req.prompt_injector.expect("injector");
        assert_eq!(injector.startup, InjectorStartup::ShellPrompt);
        assert_eq!(
            injector.steps,
            [
                InjectorStep::Delay { ms: 2000 },
                InjectorStep::Text {
                    content: format!("claude --resume {CONV}"),
                    newline: true,
                },
            ]
        );
    }

    #[test]
    fn shell_without_a_conversation_is_a_bare_shell() {
        let entry = folder_only(vec![member("", r"D:\yaat")]);
        let req = plan(&entry, &item(RecoverAs::Shell, None))
            .expect("plan")
            .request;
        assert_eq!(req.mode, SessionMode::PlainShell);
        assert_eq!(req.prompt_injector, None);
    }

    #[test]
    fn unknown_kind_is_refused() {
        let entry = folder_only(vec![member("", r"D:\yaat")]);
        assert_eq!(
            plan(&entry, &item(RecoverAs::Unknown, Some(CONV))),
            Err("unsupported recovery kind".to_owned())
        );
    }

    fn write_transcript(home: &Path, cwd: &str, id: &str, at: DateTime<Utc>) {
        let dir = home
            .join("projects")
            .join(transcripts::encode_project_dir(cwd));
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
        known.claude_session_id = Some("conv-known".to_owned());
        write_transcript(&home, &git_dir, "conv-known", ended - TimeDelta::days(3));
        write_transcript(&home, &git_dir, "conv-other", ended);
        let mut gone = history_entry(
            "gone",
            SessionMode::Interactive,
            &git_dir,
            ended - TimeDelta::seconds(1),
        );
        gone.claude_session_id = Some("conv-missing".to_owned());
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

        let items = history_items(&dirs, &repos, Some(&home), |_| None);

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

        let no_home = history_items(&dirs, &repos, None, |_| None);
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

        let items = history_items(&dirs, &[], Some(&home), |_| None);

        let ids: Vec<&str> = items[0].candidates.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["within-a-day"]);
        assert!(!items[0].folder_is_git_repo);
        let _ = std::fs::remove_dir_all(&dirs.config);
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
        let items = history_items(&dirs, &[], Some(&home), |_| None);
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

    /// An interactive `agent` entry in `folder` whose spawn recorded
    /// `CODEX_HOME=<codex_home>`, with its conversation id when given, and a
    /// Claude transcript written in its window (a Claude entry would get it
    /// as a candidate).
    fn agent_entry(
        dirs: &Dirs,
        agent: Agent,
        session_id: &str,
        conversation: Option<&str>,
        codex_home: &Path,
    ) -> HistoryEntry {
        let folder = dirs.config.join("agent-folder");
        let folder = folder.to_string_lossy();
        let ended = DateTime::from_timestamp(Utc::now().timestamp() - 600, 0).expect("time");
        let mut entry = history_entry(session_id, SessionMode::Interactive, &folder, ended);
        entry.agent = agent;
        entry.agent_conversation_id = conversation.map(str::to_owned);
        let mut config = codex_config();
        config.extra_env = vec![(
            "CODEX_HOME".to_owned(),
            codex_home.to_string_lossy().into_owned(),
        )];
        entry.spawn_config = Some(config);
        write_transcript(
            &dirs.config.join("claude-home"),
            &folder,
            &format!("claude-{session_id}"),
            ended - TimeDelta::minutes(5),
        );
        write_if_absent(dirs, &entry).expect("write entry");
        entry
    }

    /// Items for `dirs`, the Codex home read from each entry's own
    /// `CODEX_HOME` row.
    fn agent_items(dirs: &Dirs) -> HashMap<String, SessionHistoryItem> {
        let codex_home = |env: &[(String, String)]| {
            env.iter()
                .find(|(name, _)| name == "CODEX_HOME")
                .map(|(_, value)| PathBuf::from(value))
        };
        history_items(
            dirs,
            &[],
            Some(&dirs.config.join("claude-home")),
            codex_home,
        )
        .into_iter()
        .map(|item| (item.entry.session_id.clone(), item))
        .collect()
    }

    #[test]
    fn codex_history_item_has_no_candidates_and_resumable_follows_rollout() {
        let dirs = scratch_dirs("items-codex");
        let codex_home = dirs.config.join("codex-home");
        let day = codex_home
            .join("sessions")
            .join("2026")
            .join("09")
            .join("28");
        std::fs::create_dir_all(&day).expect("day folder");
        std::fs::write(
            day.join(format!("rollout-2026-09-28T10-00-00-{CODEX_ID}.jsonl")),
            "{}\n",
        )
        .expect("rollout");
        agent_entry(&dirs, Agent::Codex, "kept", Some(CODEX_ID), &codex_home);
        agent_entry(
            &dirs,
            Agent::Codex,
            "gone",
            Some("0199ffff-gone"),
            &codex_home,
        );
        agent_entry(&dirs, Agent::Codex, "none", None, &codex_home);
        agent_entry(
            &dirs,
            Agent::Codex,
            "other-home",
            Some(CODEX_ID),
            &dirs.config.join("elsewhere"),
        );
        agent_entry(&dirs, Agent::Claude, "claude", None, &codex_home);

        let items = agent_items(&dirs);

        assert_eq!(items.len(), 5);
        for id in ["kept", "gone", "none", "other-home"] {
            assert!(
                items[id].candidates.is_empty(),
                "{id} gets no Claude transcripts"
            );
        }
        assert!(
            !items["claude"].candidates.is_empty(),
            "a Claude entry still does"
        );
        assert!(items["kept"].own_agent_resumable);
        assert!(!items["gone"].own_agent_resumable, "its rollout is gone");
        assert!(!items["none"].own_agent_resumable, "no id recorded");
        assert!(
            !items["other-home"].own_agent_resumable,
            "looked up under the entry's own Codex home"
        );
        assert!(!items["claude"].own_agent_resumable);
        let _ = std::fs::remove_dir_all(&dirs.config);
    }

    #[test]
    fn cursor_history_item_resumable_when_id_recorded() {
        let dirs = scratch_dirs("items-cursor");
        let nowhere = dirs.config.join("no-codex-home");
        agent_entry(&dirs, Agent::Cursor, "chat", Some(CODEX_ID), &nowhere);
        agent_entry(&dirs, Agent::Cursor, "no-chat", None, &nowhere);

        let items = agent_items(&dirs);

        assert!(items["chat"].own_agent_resumable);
        assert!(!items["no-chat"].own_agent_resumable);
        assert!(items.values().all(|item| item.candidates.is_empty()));
        let _ = std::fs::remove_dir_all(&dirs.config);
    }
}
