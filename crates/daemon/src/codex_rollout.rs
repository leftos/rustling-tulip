//! Codex rollout files: the thread id a Codex session writes to disk.
//!
//! Codex names each thread's transcript
//! `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<local time>-<thread id>.jsonl`
//! once the first message is sent, and resumes it with `codex resume <id>`.
//! There is no flag to choose the id up front, so the daemon finds the file a
//! session wrote by its first line's `payload.cwd` and records its id.

use crate::history;
use crate::orphan;
use crate::paths::{Dirs, normalize_path_key};
use crate::session::{SessionRecord, SessionRegistry};
use crate::sync::lock;
use crate::user_env;
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeDelta, TimeZone, Utc};
use directories::UserDirs;
use protocol::{Agent, SessionMode, SessionStatus};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// How often [`watch`] looks for the rollout during its first [`FAST_PHASE`].
const FAST_POLL: Duration = Duration::from_secs(2);
/// How often [`watch`] looks once [`FAST_PHASE`] is over.
const SLOW_POLL: Duration = Duration::from_secs(10);
/// How long [`watch`] polls every [`FAST_POLL`].
const FAST_PHASE: Duration = Duration::from_mins(5);
/// How long [`watch`] polls at most every [`SLOW_POLL`].
const SLOW_PHASE: Duration = Duration::from_hours(1);
/// How often [`watch`] looks once [`SLOW_PHASE`] is over.
const IDLE_POLL: Duration = Duration::from_mins(1);
/// How much earlier than the spawn a rollout's name may be stamped.
const NAME_SLACK: TimeDelta = TimeDelta::seconds(5);
/// Length of the local time in a rollout's name, `YYYY-MM-DDTHH-MM-SS`.
const NAME_TIME_LEN: usize = 19;
const NAME_TIME_FORMAT: &str = "%Y-%m-%dT%H-%M-%S";

/// The file under the config dir holding the contested rollouts.
const CONTESTED_FILE: &str = "codex-contested.json";

/// Held across one whole claim (the waiting check, the scan and the record
/// write) and every read-modify-write of the contested file, so two sessions
/// never claim or contest the same rollout at once.
static CLAIM_LOCK: Mutex<()> = Mutex::new(());

/// A rollout found while another Codex session waited in the same folder.
/// Neither session can tell whose it is, so no session claims it, even after
/// the other one ends or the daemon restarts. Kept for
/// [`history::HISTORY_RETENTION`].
#[derive(Debug, Serialize, Deserialize)]
struct ContestedRollout {
    id: String,
    contested_at: DateTime<Utc>,
}

/// Codex's home for a spawn: the spawn's own `CODEX_HOME` env row (an env
/// reference resolved as the spawn resolves it), else the daemon's
/// `CODEX_HOME` (an interactive child inherits the daemon's environment),
/// else `<home>/.codex`. An empty value counts as unset.
#[must_use]
pub fn codex_home(extra_env: &[(String, String)]) -> Option<PathBuf> {
    codex_home_with(extra_env, user_env::resolve)
}

fn codex_home_with(
    extra_env: &[(String, String)],
    lookup: impl FnMut(&str) -> Option<(user_env::Secret, user_env::Origin)>,
) -> Option<PathBuf> {
    let row = extra_env
        .iter()
        .rev()
        .find(|(name, _)| name.eq_ignore_ascii_case("CODEX_HOME"));
    let resolved = row.and_then(|row| {
        match crate::server::resolve_env_refs(std::slice::from_ref(row), lookup) {
            Ok(mut rows) => rows.pop().map(|(_, value)| value),
            Err(failure) => {
                warn!(
                    detail = %failure.detail,
                    "the spawn's CODEX_HOME row does not resolve; using the daemon's CODEX_HOME or the default"
                );
                None
            }
        }
    });
    let home = UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    codex_home_from(resolved.as_deref(), std::env::var_os("CODEX_HOME"), home)
}

fn contested_path(dirs: &Dirs) -> PathBuf {
    dirs.config.join(CONTESTED_FILE)
}

/// The contested rollouts on disk, without those contested longer ago than
/// [`history::HISTORY_RETENTION`]. A missing or unreadable file is empty.
fn load_contested(dirs: &Dirs) -> Vec<ContestedRollout> {
    let path = contested_path(dirs);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            warn!(?err, path = %path.display(), "failed to read the contested codex rollouts");
            return Vec::new();
        }
    };
    let mut entries: Vec<ContestedRollout> = match serde_json::from_slice(&bytes) {
        Ok(entries) => entries,
        Err(err) => {
            warn!(?err, path = %path.display(), "ignoring an unparseable contested codex rollouts file");
            return Vec::new();
        }
    };
    let cutoff = Utc::now() - history::HISTORY_RETENTION;
    entries.retain(|entry| entry.contested_at >= cutoff);
    entries
}

/// Add `id` to the contested rollouts on disk. Call with [`CLAIM_LOCK`] held.
fn contest(dirs: &Dirs, id: &str) {
    let mut entries = load_contested(dirs);
    if entries.iter().any(|entry| entry.id == id) {
        return;
    }
    entries.push(ContestedRollout {
        id: id.to_string(),
        contested_at: Utc::now(),
    });
    let path = contested_path(dirs);
    let written = serde_json::to_vec_pretty(&entries)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| orphan::write_atomic(&path, &bytes));
    if let Err(err) = written {
        warn!(?err, path = %path.display(), %id, "failed to save a contested codex rollout");
    }
}

/// The conversation ids of history entries that ended within
/// [`history::HISTORY_RETENTION`].
fn ended_ids(dirs: &Dirs) -> HashSet<String> {
    let cutoff = Utc::now() - history::HISTORY_RETENTION;
    history::read_all(dirs)
        .into_iter()
        .filter(|entry| entry.ended_at >= cutoff)
        .filter_map(|entry| entry.agent_conversation_id)
        .collect()
}

fn codex_home_from(
    row: Option<&str>,
    process: Option<OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(row) = row.filter(|row| !row.is_empty()) {
        return Some(PathBuf::from(row));
    }
    match process {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => home.map(|home| home.join(".codex")),
    }
}

/// The thread id in a rollout file name: the text after `rollout-`, the
/// 19-character local time and `-`, up to `_` or `.jsonl`. Compressed
/// (`.jsonl.zst`) names count.
#[must_use]
pub fn id_from_file_name(name: &str) -> Option<&str> {
    name_parts(name).map(|(_, id)| id)
}

/// A rollout file name's local creation time and thread id.
fn name_parts(name: &str) -> Option<(NaiveDateTime, &str)> {
    let rest = name.strip_prefix("rollout-")?;
    let time = NaiveDateTime::parse_from_str(rest.get(..NAME_TIME_LEN)?, NAME_TIME_FORMAT).ok()?;
    let tail = rest.get(NAME_TIME_LEN..)?.strip_prefix('-')?;
    let stem = tail
        .strip_suffix(".jsonl")
        .or_else(|| tail.strip_suffix(".jsonl.zst"))?;
    let id = stem.split('_').next()?;
    (!id.is_empty()).then_some((time, id))
}

/// The first line of a rollout: `{"type":"session_meta","payload":{...}}`.
#[derive(Deserialize)]
struct FirstLine {
    #[serde(rename = "type")]
    kind: String,
    payload: MetaPayload,
}

#[derive(Deserialize)]
struct MetaPayload {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    source: Option<serde_json::Value>,
}

/// What a rollout's first line says about its thread.
enum Head {
    /// The first line is not complete yet; the file is read again later.
    Unfinished,
    /// An interactive (`source: "cli"`) thread started in `cwd`.
    Interactive { cwd: String },
    /// Anything else: an `exec` run, another source, an unparseable line.
    Other,
}

fn read_head(path: &Path) -> Head {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) => {
            debug!(?err, path = %path.display(), "codex rollout not readable yet");
            return Head::Unfinished;
        }
    };
    let mut line = String::new();
    match BufReader::new(file).read_line(&mut line) {
        Ok(_) if line.ends_with('\n') => {}
        Ok(_) => return Head::Unfinished,
        Err(err) => {
            debug!(?err, path = %path.display(), "codex rollout's first line not readable yet");
            return Head::Unfinished;
        }
    }
    match serde_json::from_str::<FirstLine>(&line) {
        Ok(head)
            if head.kind == "session_meta"
                && head
                    .payload
                    .source
                    .as_ref()
                    .and_then(serde_json::Value::as_str)
                    == Some("cli") =>
        {
            head.payload
                .cwd
                .map_or(Head::Other, |cwd| Head::Interactive { cwd })
        }
        Ok(_) => Head::Other,
        Err(err) => {
            debug!(?err, path = %path.display(), "codex rollout's first line is not session_meta");
            Head::Other
        }
    }
}

/// `<home>/sessions/YYYY/MM/DD` for `day`.
fn day_dir(home: &Path, day: NaiveDate) -> PathBuf {
    home.join("sessions")
        .join(day.format("%Y").to_string())
        .join(day.format("%m").to_string())
        .join(day.format("%d").to_string())
}

/// The paths in `dir`; none when it is missing or unreadable.
fn list_dir(dir: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|entry| {
                entry
                    .map_err(|err| debug!(?err, dir = %dir.display(), "skipping unreadable entry"))
                    .ok()
            })
            .map(|entry| entry.path())
            .collect(),
        Err(err) if err.kind() == ErrorKind::NotFound => Vec::new(),
        Err(err) => {
            debug!(?err, dir = %dir.display(), "failed to list codex rollout folder");
            Vec::new()
        }
    }
}

/// The id of a rollout an interactive Codex thread in `cwd` started at or
/// after `since`, the earliest-named when several match.
///
/// Lists the day folders for every local date from `since`'s through today,
/// and keeps `rollout-*.jsonl` files whose name time, read as local time and
/// taken to UTC, is no earlier than `since` minus [`NAME_SLACK`] and whose id
/// is not in `taken`. Only a file's first line is
/// read; its `payload.cwd` must equal `cwd` under [`normalize_path_key`]
/// (slash direction, a trailing separator and, on Windows, case aside) and its
/// `payload.source` must be `"cli"`. Files read in full that do not match are
/// added to `seen` and skipped on later calls with the same set.
pub fn find_rollout(
    home: &Path,
    cwd: &str,
    since: DateTime<Utc>,
    taken: &HashSet<String>,
    seen: &mut HashSet<PathBuf>,
) -> Option<String> {
    let earliest = since - NAME_SLACK;
    let cwd_key = normalize_path_key(cwd);
    let today = Local::now().date_naive();
    let mut day = since.with_timezone(&Local).date_naive();
    let mut days = Vec::new();
    while day <= today {
        days.push(day);
        let Some(next) = day.succ_opt() else {
            break;
        };
        day = next;
    }
    let mut best: Option<(DateTime<Utc>, String)> = None;
    for day in days {
        for path in list_dir(&day_dir(home, day)) {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some((local_time, id)) = name_parts(name) else {
                continue;
            };
            let Some(time) = Local
                .from_local_datetime(&local_time)
                .latest()
                .map(|time| time.with_timezone(&Utc))
            else {
                continue;
            };
            if time < earliest
                || !path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
                || taken.contains(id)
                || seen.contains(&path)
            {
                continue;
            }
            match read_head(&path) {
                Head::Unfinished => {}
                Head::Interactive { cwd: found } if normalize_path_key(&found) == cwd_key => {
                    if best.as_ref().is_none_or(|(best_time, _)| time < *best_time) {
                        best = Some((time, id.to_string()));
                    }
                }
                Head::Interactive { .. } | Head::Other => {
                    seen.insert(path.clone());
                }
            }
        }
    }
    best.map(|(_, id)| id)
}

/// Whether any day folder under `<home>/sessions` holds a rollout for `id`.
/// Reads file names only.
#[must_use]
pub fn rollout_exists(home: &Path, id: &str) -> bool {
    list_dir(&home.join("sessions"))
        .iter()
        .flat_map(|year| list_dir(year))
        .flat_map(|month| list_dir(&month))
        .flat_map(|day| list_dir(&day))
        .any(|file| {
            file.file_name()
                .and_then(|name| name.to_str())
                .and_then(id_from_file_name)
                == Some(id)
        })
}

fn is_running(rec: &SessionRecord) -> bool {
    !matches!(rec.status, SessionStatus::Stopped | SessionStatus::Error)
}

/// Whether `rec` is a running interactive Codex session in `cwd` that has no
/// conversation id yet.
fn waits_in(rec: &SessionRecord, cwd: &str) -> bool {
    rec.agent == Agent::Codex
        && rec.mode == SessionMode::Interactive
        && rec.agent_conversation_id.is_none()
        && is_running(rec)
        && history::primary_cwd(rec).as_deref() == Some(cwd)
}

/// The ids no session may claim (every id already on a record, and every
/// contested one), and whether a session other than `session_id` waits in
/// `cwd`.
fn claim_view(
    registry: &SessionRegistry,
    dirs: &Dirs,
    session_id: &str,
    cwd: &str,
) -> (HashSet<String>, bool) {
    let mut taken: HashSet<String> = load_contested(dirs)
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    let mut others_wait = false;
    for snapshot in registry.snapshots() {
        let Some(arc) = registry.get(&snapshot.id) else {
            continue;
        };
        let rec = lock(&arc);
        if let Some(id) = &rec.agent_conversation_id {
            taken.insert(id.clone());
        }
        if rec.id != session_id && waits_in(&rec, cwd) {
            others_wait = true;
        }
    }
    (taken, others_wait)
}

/// One look for `session_id`'s rollout. Ids already on a record, contested,
/// or recorded by a history entry within [`history::HISTORY_RETENTION`] are
/// skipped. The id found is claimed, set on the record through
/// [`SessionRegistry::update_from`] (which syncs the sidecar), only when no
/// other Codex session waits in `cwd`; otherwise it is saved as contested and
/// no session claims it. The whole look runs under [`CLAIM_LOCK`]. Blocking
/// file I/O: call it off the async workers. Returns the claimed id.
pub fn capture_once(
    registry: &SessionRegistry,
    dirs: &Dirs,
    session_id: &str,
    home: &Path,
    cwd: &str,
    since: DateTime<Utc>,
    seen: &mut HashSet<PathBuf>,
) -> Option<String> {
    let _claim = lock(&CLAIM_LOCK);
    let (mut taken, others_wait) = claim_view(registry, dirs, session_id, cwd);
    let mut id = find_rollout(home, cwd, since, &taken, seen)?;
    let ended = ended_ids(dirs);
    if ended.contains(&id) {
        taken.extend(ended);
        id = find_rollout(home, cwd, since, &taken, seen)?;
    }
    if others_wait {
        debug!(%session_id, conversation = %id, cwd, "codex rollout left unclaimed: another codex session waits in its folder");
        contest(dirs, &id);
        return None;
    }
    let mut claimed = false;
    registry.update_from(session_id, None, |rec| {
        if rec.agent_conversation_id.is_none() {
            rec.agent_conversation_id = Some(id.clone());
            claimed = true;
        }
    });
    if !claimed {
        return None;
    }
    info!(%session_id, conversation = %id, "captured codex conversation id");
    Some(id)
}

/// Whether `session_id` is still in the registry, running, and without a
/// conversation id.
fn still_waiting(registry: &SessionRegistry, session_id: &str) -> bool {
    registry.get(session_id).is_some_and(|arc| {
        let rec = lock(&arc);
        rec.agent_conversation_id.is_none() && is_running(&rec)
    })
}

/// What one [`watch`] task polls for, shared with each blocking poll.
struct WatchJob {
    registry: Arc<SessionRegistry>,
    dirs: Dirs,
    session_id: String,
    cwd: String,
    home: PathBuf,
}

/// What a rollout look needs for an interactive Codex session that has no
/// conversation id yet.
pub struct Uncaptured {
    /// The session's primary folder, which the rollout's `payload.cwd` names.
    pub cwd: String,
    /// When the session started; older rollouts are not its own.
    pub since: DateTime<Utc>,
    /// The spawn's env rows, which may name its `CODEX_HOME`.
    pub extra_env: Vec<(String, String)>,
}

/// The rollout look for `rec` when it is an interactive Codex session with
/// no conversation id and a primary folder; `None` for anything else.
#[must_use]
pub fn uncaptured(rec: &SessionRecord) -> Option<Uncaptured> {
    if rec.agent != Agent::Codex
        || rec.mode != SessionMode::Interactive
        || rec.agent_conversation_id.is_some()
    {
        return None;
    }
    Some(Uncaptured {
        cwd: history::primary_cwd(rec)?,
        since: rec.started_at,
        extra_env: rec
            .spawn_config
            .as_ref()
            .map(|cfg| cfg.extra_env.clone())
            .unwrap_or_default(),
    })
}

/// Start [`watch`] for `session_id` when it is an interactive Codex session
/// without a conversation id (see [`uncaptured`]): from its `started_at`, in
/// its primary folder, under the Codex home its spawn settings name. Starts
/// nothing for any other session.
pub fn watch_session(registry: &Arc<SessionRegistry>, dirs: &Dirs, session_id: &str) {
    let Some(look) = registry
        .get(session_id)
        .and_then(|arc| uncaptured(&lock(&arc)))
    else {
        return;
    };
    let Some(home) = codex_home(&look.extra_env) else {
        warn!(%session_id, "no codex home found; the session's rollout is not watched");
        return;
    };
    debug!(%session_id, cwd = %look.cwd, home = %home.display(), "watching for the codex rollout");
    watch(
        Arc::clone(registry),
        dirs.clone(),
        session_id.to_owned(),
        look.cwd,
        look.since,
        home,
    );
}

/// How long [`watch`] waits before its next look, `elapsed` after it
/// started: [`FAST_POLL`] for the first [`FAST_PHASE`], [`SLOW_POLL`] until
/// [`SLOW_PHASE`], then [`IDLE_POLL`].
fn poll_pause(elapsed: Duration) -> Duration {
    if elapsed < FAST_PHASE {
        FAST_POLL
    } else if elapsed < SLOW_PHASE {
        SLOW_POLL
    } else {
        IDLE_POLL
    }
}

/// Poll for `session_id`'s rollout with [`capture_once`], each poll on the
/// blocking pool, pausing [`poll_pause`] between looks. Stops once the id is
/// claimed, or the session leaves the registry, stops running or gets an id
/// another way.
pub fn watch(
    registry: Arc<SessionRegistry>,
    dirs: Dirs,
    session_id: String,
    cwd: String,
    since: DateTime<Utc>,
    home: PathBuf,
) -> JoinHandle<()> {
    let job = Arc::new(WatchJob {
        registry,
        dirs,
        session_id,
        cwd,
        home,
    });
    tokio::spawn(async move {
        let started = tokio::time::Instant::now();
        let mut seen = HashSet::new();
        while still_waiting(&job.registry, &job.session_id) {
            let poll = Arc::clone(&job);
            let looked = tokio::task::spawn_blocking(move || {
                let found = capture_once(
                    &poll.registry,
                    &poll.dirs,
                    &poll.session_id,
                    &poll.home,
                    &poll.cwd,
                    since,
                    &mut seen,
                );
                (found, seen)
            })
            .await;
            match looked {
                Ok((Some(_), _)) => return,
                Ok((None, kept)) => seen = kept,
                Err(err) => {
                    warn!(?err, session_id = %job.session_id, "codex rollout poll failed; watch stopped");
                    return;
                }
            }
            tokio::time::sleep(poll_pause(started.elapsed())).await;
        }
        debug!(session_id = %job.session_id, "codex rollout watch ended without an id");
    })
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod tests {
    use super::*;
    use crate::history::test_support::{record, scratch_dirs, write_meta_for};
    use chrono::{Local, NaiveDate, NaiveDateTime, TimeDelta};
    use protocol::{Agent, SessionMember, SessionMode};
    use std::time::Duration;

    const CWD: &str = r"X:\dev\proj";

    fn scratch_home(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rt-codex-home-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create scratch codex home");
        root
    }

    fn meta_line(id: &str, cwd: &str, source: &str) -> String {
        serde_json::json!({
            "timestamp": "2026-09-28T21:46:12.000Z",
            "ordinal": 0,
            "type": "session_meta",
            "payload": { "id": id, "session_id": id, "cwd": cwd, "source": source },
        })
        .to_string()
    }

    fn rollout_name(at: NaiveDateTime, id: &str) -> String {
        format!("rollout-{}-{id}.jsonl", at.format("%Y-%m-%dT%H-%M-%S"))
    }

    fn write_rollout_named(home: &Path, at: NaiveDateTime, name: &str, first_line: &str) {
        let dir = home
            .join("sessions")
            .join(at.format("%Y/%m/%d").to_string());
        std::fs::create_dir_all(&dir).expect("create day folder");
        std::fs::write(dir.join(name), format!("{first_line}\n")).expect("write rollout");
    }

    fn write_rollout(home: &Path, at: NaiveDateTime, id: &str, cwd: &str) {
        write_rollout_named(home, at, &rollout_name(at, id), &meta_line(id, cwd, "cli"));
    }

    fn local(at: DateTime<Utc>) -> NaiveDateTime {
        at.with_timezone(&Local).naive_local()
    }

    fn codex_record(id: &str, cwd: &str) -> SessionRecord {
        let mut rec = record(id, SessionMode::Interactive);
        rec.agent = Agent::Codex;
        rec.members = vec![SessionMember {
            repo_id: "r".to_string(),
            repo_name: "proj".to_string(),
            branch: "main".to_string(),
            worktree_path: cwd.to_string(),
        }];
        rec
    }

    fn conversation_id(registry: &SessionRegistry, id: &str) -> Option<String> {
        registry
            .get(id)
            .and_then(|rec| crate::sync::lock(&rec).agent_conversation_id.clone())
    }

    #[test]
    fn codex_id_comes_from_plain_reverted_and_compressed_names() {
        let id = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
        let rollout = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5c";
        let plain = format!("rollout-2026-09-28T23-46-12-{id}.jsonl");
        let reverted = format!("rollout-2026-09-28T23-46-12-{id}_{rollout}.jsonl");
        let compressed = format!("rollout-2026-09-28T23-46-12-{id}.jsonl.zst");
        assert_eq!(id_from_file_name(&plain), Some(id));
        assert_eq!(id_from_file_name(&reverted), Some(id));
        assert_eq!(id_from_file_name(&compressed), Some(id));
        assert_eq!(id_from_file_name("rollout-2026-09-28T23-46-12.jsonl"), None);
        assert_eq!(id_from_file_name(&format!("notes-{id}.jsonl")), None);
        assert_eq!(
            id_from_file_name(&format!("rollout-2026-09-28T23-46-12-{id}.txt")),
            None
        );
    }

    #[test]
    fn codex_find_takes_a_new_rollout_in_the_cwd() {
        let home = scratch_home("find-new");
        let now = Utc::now();
        write_rollout(&home, local(now), "id-new", CWD);
        let found = find_rollout(
            &home,
            CWD,
            now - TimeDelta::minutes(1),
            &HashSet::new(),
            &mut HashSet::new(),
        );
        assert_eq!(found.as_deref(), Some("id-new"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn codex_find_skips_another_cwd_an_older_file_a_taken_id_and_an_exec_run() {
        let home = scratch_home("find-skips");
        let now = Utc::now();
        let since = now - TimeDelta::minutes(1);
        write_rollout(&home, local(now), "id-other-cwd", r"X:\dev\other");
        write_rollout(&home, local(since - TimeDelta::minutes(1)), "id-old", CWD);
        write_rollout(&home, local(now), "id-taken", CWD);
        write_rollout_named(
            &home,
            local(now),
            &rollout_name(local(now), "id-exec"),
            &meta_line("id-exec", CWD, "exec"),
        );
        let taken = HashSet::from(["id-taken".to_string()]);
        let found = find_rollout(&home, CWD, since, &taken, &mut HashSet::new());
        assert_eq!(found, None);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn codex_find_reads_yesterdays_folder_for_a_spawn_before_midnight() {
        let home = scratch_home("find-yesterday");
        let yesterday = Local::now().date_naive().pred_opt().expect("yesterday");
        let spawned = yesterday.and_hms_opt(23, 59, 58).expect("spawn time");
        let since = spawned
            .and_local_timezone(Local)
            .earliest()
            .expect("local spawn time")
            .with_timezone(&Utc);
        let created = yesterday.and_hms_opt(23, 59, 59).expect("thread time");
        write_rollout(&home, created, "id-yesterday", CWD);
        let found = find_rollout(&home, CWD, since, &HashSet::new(), &mut HashSet::new());
        assert_eq!(found.as_deref(), Some("id-yesterday"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn codex_two_waiting_sessions_on_one_cwd_claim_nothing() {
        let dirs = scratch_dirs("codex-two-waiting");
        let home = scratch_home("two-waiting");
        let cwd = r"X:\dev\shared";
        let registry = SessionRegistry::new(dirs.clone());
        registry.insert(codex_record("a1", cwd));
        registry.insert(codex_record("a2", cwd));
        let since = Utc::now() - TimeDelta::minutes(1);
        write_rollout(&home, local(Utc::now()), "id-contested", cwd);

        let mut seen = HashSet::new();
        assert_eq!(
            capture_once(&registry, &dirs, "a1", &home, cwd, since, &mut seen),
            None
        );
        assert_eq!(
            capture_once(
                &registry,
                &dirs,
                "a2",
                &home,
                cwd,
                since,
                &mut HashSet::new()
            ),
            None
        );
        registry.remove("a2");
        assert_eq!(
            capture_once(
                &registry,
                &dirs,
                "a1",
                &home,
                cwd,
                since,
                &mut HashSet::new()
            ),
            None,
            "a rollout found while two sessions waited stays unclaimed"
        );
        assert_eq!(conversation_id(&registry, "a1"), None);
        let _ = std::fs::remove_dir_all(&dirs.config);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn codex_rollout_exists_finds_a_file_in_any_day_folder() {
        let home = scratch_home("exists");
        let id = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5d";
        let old = NaiveDate::from_ymd_opt(2025, 1, 2)
            .and_then(|d| d.and_hms_opt(3, 4, 5))
            .expect("fixture time");
        write_rollout_named(
            &home,
            old,
            &format!("rollout-2025-01-02T03-04-05-{id}.jsonl.zst"),
            "compressed",
        );
        assert!(rollout_exists(&home, id));
        assert!(!rollout_exists(
            &home,
            "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5e"
        ));
        let empty = scratch_home("exists-empty");
        assert!(!rollout_exists(&empty, id));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[test]
    fn codex_home_prefers_the_env_row() {
        let rows = vec![("CODEX_HOME".to_string(), r"D:\codex-row".to_string())];
        assert_eq!(codex_home(&rows), Some(PathBuf::from(r"D:\codex-row")));
        let user = Some(PathBuf::from(r"C:\Users\me"));
        let process = Some(OsString::from(r"D:\codex-process"));
        assert_eq!(
            codex_home_from(Some(r"D:\codex-row"), process.clone(), user.clone()),
            Some(PathBuf::from(r"D:\codex-row"))
        );
        assert_eq!(
            codex_home_from(None, process, user.clone()),
            Some(PathBuf::from(r"D:\codex-process"))
        );
        assert_eq!(
            codex_home_from(None, None, user),
            Some(PathBuf::from(r"C:\Users\me\.codex"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn codex_watch_sets_the_record_and_sidecar_once_the_file_appears() {
        let dirs = scratch_dirs("codex-watch-finds");
        let home = scratch_home("watch-finds");
        let cwd = r"X:\dev\watched";
        write_meta_for(&dirs, "w1");
        let registry = SessionRegistry::new(dirs.clone());
        registry.insert(codex_record("w1", cwd));
        let since = Utc::now() - TimeDelta::minutes(1);
        let handle = watch(
            Arc::clone(&registry),
            dirs.clone(),
            "w1".to_string(),
            cwd.to_string(),
            since,
            home.clone(),
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!handle.is_finished(), "nothing to find yet");

        write_rollout(&home, local(Utc::now()), "id-watched", cwd);
        tokio::time::timeout(Duration::from_secs(30), handle)
            .await
            .expect("watch stops once it finds the rollout")
            .expect("watch task");

        assert_eq!(
            conversation_id(&registry, "w1").as_deref(),
            Some("id-watched")
        );
        let meta = crate::orphan::load_meta(&dirs, "w1").expect("load meta");
        assert_eq!(meta.agent_conversation_id.as_deref(), Some("id-watched"));
        let _ = std::fs::remove_dir_all(&dirs.config);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn watch_backs_off_after_an_hour() {
        assert_eq!(poll_pause(Duration::ZERO), FAST_POLL);
        assert_eq!(poll_pause(Duration::from_secs(299)), FAST_POLL);
        assert_eq!(poll_pause(Duration::from_mins(5)), SLOW_POLL);
        assert_eq!(poll_pause(Duration::from_secs(3599)), SLOW_POLL);
        assert_eq!(poll_pause(Duration::from_hours(1)), Duration::from_mins(1));
        assert_eq!(poll_pause(Duration::from_hours(30)), Duration::from_mins(1));
    }

    /// Slash direction and case count only on Windows, as in
    /// [`normalize_path_key`].
    #[cfg(windows)]
    #[test]
    fn codex_find_matches_the_cwd_under_path_normalization() {
        let now = Utc::now();
        let since = now - TimeDelta::minutes(1);
        for (tag, written) in [
            ("slashes", "X:/dev/proj"),
            ("trailing", r"X:\dev\proj\"),
            ("drive-case", r"x:\dev\proj"),
        ] {
            let home = scratch_home(&format!("find-normalized-{tag}"));
            write_rollout(&home, local(now), "id-normalized", written);
            let found = find_rollout(&home, CWD, since, &HashSet::new(), &mut HashSet::new());
            assert_eq!(found.as_deref(), Some("id-normalized"), "cwd {written}");
            let _ = std::fs::remove_dir_all(&home);
        }
        let home = scratch_home("find-normalized-other");
        write_rollout(&home, local(now), "id-sibling", r"X:\dev\proj2");
        let found = find_rollout(&home, CWD, since, &HashSet::new(), &mut HashSet::new());
        assert_eq!(found, None, "another folder is not matched");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test(start_paused = true)]
    async fn codex_watch_stops_when_the_record_goes() {
        let dirs = scratch_dirs("codex-watch-gone");
        let home = scratch_home("watch-gone");
        let registry = SessionRegistry::new(dirs.clone());
        registry.insert(codex_record("w2", CWD));
        let handle = watch(
            Arc::clone(&registry),
            dirs.clone(),
            "w2".to_string(),
            CWD.to_string(),
            Utc::now(),
            home.clone(),
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!handle.is_finished(), "the session is still waiting");

        registry.remove("w2");
        tokio::time::timeout(Duration::from_secs(30), handle)
            .await
            .expect("watch stops once the record is gone")
            .expect("watch task");
        let _ = std::fs::remove_dir_all(&dirs.config);
        let _ = std::fs::remove_dir_all(&home);
    }

    fn utc_at(day: NaiveDate, hour: u32) -> DateTime<Utc> {
        day.and_hms_opt(hour, 0, 0)
            .expect("fixture time")
            .and_local_timezone(Local)
            .earliest()
            .expect("local fixture time")
            .with_timezone(&Utc)
    }

    #[test]
    fn codex_contested_rollout_stays_unclaimed_after_a_restart() {
        let dirs = scratch_dirs("codex-contested-restart");
        let home = scratch_home("contested-restart");
        let cwd = r"X:\dev\restart";
        let since = Utc::now() - TimeDelta::minutes(1);
        let before = SessionRegistry::new(dirs.clone());
        before.insert(codex_record("a", cwd));
        before.insert(codex_record("b", cwd));
        write_rollout(&home, local(Utc::now()), "id-of-a", cwd);
        assert_eq!(
            capture_once(&before, &dirs, "a", &home, cwd, since, &mut HashSet::new()),
            None
        );
        let stored = std::fs::read_to_string(dirs.config.join("codex-contested.json"))
            .expect("contested set on disk");
        assert!(stored.contains("id-of-a"), "{stored}");

        let after = SessionRegistry::new(dirs.clone());
        after.insert(codex_record("b", cwd));
        assert_eq!(
            capture_once(&after, &dirs, "b", &home, cwd, since, &mut HashSet::new()),
            None
        );
        assert_eq!(conversation_id(&after, "b"), None);
        let _ = std::fs::remove_dir_all(&dirs.config);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn codex_contested_entries_older_than_the_history_window_are_dropped() {
        let dirs = scratch_dirs("codex-contested-prune");
        let home = scratch_home("contested-prune");
        let cwd = r"X:\dev\prune";
        std::fs::write(
            dirs.config.join("codex-contested.json"),
            r#"[{"id":"id-long-ago","contested_at":"2020-01-01T00:00:00Z"}]"#,
        )
        .expect("write contested set");
        let registry = SessionRegistry::new(dirs.clone());
        registry.insert(codex_record("p", cwd));
        write_rollout(&home, local(Utc::now()), "id-long-ago", cwd);
        let since = Utc::now() - TimeDelta::minutes(1);
        assert_eq!(
            capture_once(
                &registry,
                &dirs,
                "p",
                &home,
                cwd,
                since,
                &mut HashSet::new()
            )
            .as_deref(),
            Some("id-long-ago")
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn codex_id_of_an_ended_session_in_the_history_is_not_claimed() {
        let dirs = scratch_dirs("codex-history-taken");
        let home = scratch_home("history-taken");
        let cwd = r"X:\dev\ended";
        let mut ended = codex_record("ended", cwd);
        ended.agent_conversation_id = Some("id-ended".to_string());
        let entry =
            history::entry_from_record(&ended, protocol::SessionEnd::StoppedByUser, Utc::now());
        history::write_if_absent(&dirs, &entry).expect("write history entry");
        let registry = SessionRegistry::new(dirs.clone());
        registry.insert(codex_record("next", cwd));
        write_rollout(&home, local(Utc::now()), "id-ended", cwd);
        let since = Utc::now() - TimeDelta::minutes(1);
        assert_eq!(
            capture_once(
                &registry,
                &dirs,
                "next",
                &home,
                cwd,
                since,
                &mut HashSet::new()
            ),
            None
        );
        let _ = std::fs::remove_dir_all(&dirs.config);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn codex_find_scans_every_day_since_the_spawn() {
        let home = scratch_home("find-every-day");
        let yesterday = Local::now().date_naive().pred_opt().expect("yesterday");
        let two_days_ago = yesterday.pred_opt().expect("two days ago");
        let created = yesterday.and_hms_opt(12, 0, 0).expect("thread time");
        write_rollout(&home, created, "id-mid-session", CWD);
        let found = find_rollout(
            &home,
            CWD,
            utc_at(two_days_ago, 12),
            &HashSet::new(),
            &mut HashSet::new(),
        );
        assert_eq!(found.as_deref(), Some("id-mid-session"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn codex_home_resolves_an_env_reference_row() {
        let rows = vec![("CODEX_HOME".to_string(), "${env:RT_CODEX_HOME}".to_string())];
        let resolved = codex_home_with(&rows, |name| {
            crate::user_env::resolve_with(
                name,
                |n| (n == "RT_CODEX_HOME").then(|| r"D:\resolved-codex".to_owned()),
                |_| None,
            )
        });
        assert_eq!(resolved, Some(PathBuf::from(r"D:\resolved-codex")));
        let unresolved = codex_home_with(&rows, |_| None);
        assert_ne!(unresolved, Some(PathBuf::from("${env:RT_CODEX_HOME}")));
    }

    #[test]
    fn codex_home_resolves_a_secret_reference_row() {
        let (_lock, _dir) = crate::env_secrets::test_support::scratch("codex-secret-home");
        let stored = crate::env_secrets::seal("CODEX_HOME", r"D:\sealed-codex").expect("seals");
        let rows = vec![("CODEX_HOME".to_string(), format!("${{secret:{stored}}}"))];
        // The lookup is the env one; a secret row is resolved from the store.
        assert_eq!(
            codex_home_with(&rows, |_| None),
            Some(PathBuf::from(r"D:\sealed-codex"))
        );
        let unknown = vec![(
            "CODEX_HOME".to_string(),
            "${secret:0123456789abcdef0123456789abcdef}".to_string(),
        )];
        assert_ne!(
            codex_home_with(&unknown, |_| None),
            Some(PathBuf::from("${secret:0123456789abcdef0123456789abcdef}"))
        );
    }
}
