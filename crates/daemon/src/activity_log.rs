//! The status-change log: `<config>/activity/<key>.jsonl`, one file per
//! session and one `{at, status}` line per status change, so the Dashboard's
//! two-hour activity track is whole when one opens (design in
//! `docs/plans/dashboard.md`).
//!
//! Every function takes the folder and the clock as arguments, so tests drive
//! it with fixed times. A log write never fails or delays a status update
//! beyond the write itself: an I/O error is logged and swallowed.

use chrono::{DateTime, Utc};
use protocol::SessionStatus;
use serde::{Deserialize, Serialize};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::warn;

/// How far back the activity track draws: a line older than this is pruned,
/// and an ended session's file is deleted once its newest line passes it.
pub const WINDOW: chrono::Duration = chrono::Duration::hours(2);

/// How often a running daemon refreshes `activity/daemon.json`'s alive stamp
/// and prunes the log.
pub const ALIVE_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// The daemon's own start and alive record, in the activity folder.
const DAEMON_FILE: &str = "daemon.json";

/// Serialises every append against another append and against a prune's
/// read-through-rename, so two writers never interleave inside one file and a
/// prune never rewrites over a line that landed while it ran.
static FILE_LOCK: Mutex<()> = Mutex::new(());

/// One line of a status-change log.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
struct Line {
    at: DateTime<Utc>,
    status: SessionStatus,
}

/// A stretch with no record: the daemon was not running, or the key had no
/// line yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gap {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

/// This daemon's start time and last alive stamp, with the stretches no
/// daemon recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonRecord {
    pub started_at: Option<DateTime<Utc>>,
    pub alive_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub gaps: Vec<Gap>,
}

/// The log key of session `id`.
#[must_use]
pub fn session_key(id: &str) -> String {
    format!("session-{id}")
}

/// Append one status change to `key`'s log, creating the folder if missing.
pub fn append(dir: &Path, key: &str, at: DateTime<Utc>, status: SessionStatus) {
    let mut line = match serde_json::to_vec(&Line { at, status }) {
        Ok(line) => line,
        Err(err) => {
            warn!(key, ?err, "serializing a status-change line failed");
            return;
        }
    };
    line.push(b'\n');
    if let Err(err) = std::fs::create_dir_all(dir) {
        warn!(key, ?err, dir = %dir.display(), "creating the activity dir failed");
        return;
    }
    let path = line_path(dir, key);
    // Held across the open and the one write, so no other append can land
    // between this line's bytes and its newline.
    let _guard = crate::sync::lock(&FILE_LOCK);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path);
    let mut file = match file {
        Ok(file) => file,
        Err(err) => {
            warn!(key, ?err, path = %path.display(), "opening an activity log failed");
            return;
        }
    };
    if let Err(err) = file.write_all(&line) {
        warn!(key, ?err, path = %path.display(), "writing an activity log failed");
    }
}

/// `key`'s lines sorted by their stamp, the newest last: two changes appended
/// in the opposite order of their stamps still read in time order. The sort
/// is stable, so equal stamps keep their file order. A line that does not
/// parse is skipped with one warning for the whole file; a missing file is
/// empty.
#[must_use]
pub fn read_lines(dir: &Path, key: &str) -> Vec<(DateTime<Utc>, SessionStatus)> {
    let path = line_path(dir, key);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            warn!(key, ?err, path = %path.display(), "reading an activity log failed");
            return Vec::new();
        }
    };
    let mut lines = Vec::new();
    let mut skipped = 0_usize;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        match serde_json::from_str::<Line>(line) {
            Ok(parsed) => lines.push((parsed.at, parsed.status)),
            Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        warn!(
            key,
            path = %path.display(),
            skipped,
            "skipped unreadable status-change lines"
        );
    }
    lines.sort_by_key(|(at, _)| *at);
    lines
}

/// Drop every line older than [`WINDOW`], except the newest of them, whose
/// status the window's start is read from. A key no longer live (see
/// [`sidecar_is_live`]) whose newest line passed the window has its file
/// deleted.
pub fn prune(dir: &Path, now: DateTime<Utc>, is_live: impl Fn(&str) -> bool) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return,
        Err(err) => {
            warn!(?err, dir = %dir.display(), "reading the activity dir failed");
            return;
        }
    };
    let cutoff = now - WINDOW;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let Some(key) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        prune_file(&path, key, cutoff, is_live(key));
    }
}

/// [`prune`] for one file. Holds [`FILE_LOCK`] from the read through the
/// rename, so a line appended meanwhile is never read as absent and then
/// written out of existence.
fn prune_file(path: &Path, key: &str, cutoff: DateTime<Utc>, live: bool) {
    let _guard = crate::sync::lock(&FILE_LOCK);
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            warn!(key, ?err, path = %path.display(), "reading an activity log failed");
            return;
        }
    };
    let entries: Vec<(&str, Option<Line>)> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| (line, serde_json::from_str::<Line>(line).ok()))
        .collect();
    let newest = entries
        .iter()
        .filter_map(|(_, line)| line.map(|line| line.at))
        .max();
    if !live && newest.is_none_or(|at| at < cutoff) {
        if let Err(err) = std::fs::remove_file(path) {
            warn!(key, ?err, path = %path.display(), "deleting an activity log failed");
        }
        return;
    }
    // The newest line older than the cutoff stays, so the status at the
    // window's start is known.
    let mut newest_old: Option<(usize, DateTime<Utc>)> = None;
    for (index, (_, line)) in entries.iter().enumerate() {
        if let Some(line) = *line
            && line.at < cutoff
            && newest_old.is_none_or(|(_, at)| line.at > at)
        {
            newest_old = Some((index, line.at));
        }
    }
    let kept_old = newest_old.map(|(index, _)| index);
    let mut kept = String::new();
    let mut dropped = false;
    for (index, (raw, line)) in entries.iter().enumerate() {
        let keep = line.is_some_and(|line| line.at >= cutoff) || Some(index) == kept_old;
        if keep {
            kept.push_str(raw);
            kept.push('\n');
        } else {
            dropped = true;
        }
    }
    if !dropped {
        return;
    }
    if let Err(err) = crate::atomic_file::write_atomic(path, kept.as_bytes()) {
        warn!(key, ?err, path = %path.display(), "rewriting an activity log failed");
    }
}

/// Record this daemon's start, and the stretch since the previous daemon's
/// last alive stamp as a gap.
#[must_use]
pub fn record_start(dir: &Path, now: DateTime<Utc>) -> DaemonRecord {
    let mut record = read_daemon_record(dir);
    if let Some(alive_at) = record.alive_at {
        record.gaps.push(Gap {
            from: alive_at,
            to: now,
        });
    }
    let cutoff = now - WINDOW;
    record.gaps.retain(|gap| gap.to > cutoff);
    record.started_at = Some(now);
    record.alive_at = Some(now);
    write_daemon_record(dir, &record);
    record
}

/// Refresh the alive stamp of the stored record.
pub fn stamp_alive(dir: &Path, now: DateTime<Utc>) {
    let mut record = read_daemon_record(dir);
    record.alive_at = Some(now);
    write_daemon_record(dir, &record);
}

/// The stretches to draw as gaps for a key whose earliest line is
/// `first_line_at`, in time order and never overlapping: the record's own
/// gaps still inside the window, plus the stretch from the window's start to
/// this daemon's start when the key has no line from before it.
#[must_use]
pub fn gaps_for(
    record: &DaemonRecord,
    first_line_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Vec<Gap> {
    let window_start = now - WINDOW;
    let mut gaps: Vec<Gap> = record
        .gaps
        .iter()
        .copied()
        .filter(|gap| gap.to > window_start)
        .collect();
    if let Some(started_at) = record.started_at
        && started_at > window_start
        && first_line_at.is_none_or(|at| at > started_at)
    {
        gaps.push(Gap {
            from: window_start,
            to: started_at,
        });
    }
    gaps.sort_by_key(|gap| gap.from);
    let mut merged: Vec<Gap> = Vec::with_capacity(gaps.len());
    for gap in gaps {
        match merged.last_mut() {
            Some(last) if gap.from <= last.to => last.to = last.to.max(gap.to),
            _ => merged.push(gap),
        }
    }
    merged
}

/// A liveness test for [`prune`]: a `session-<id>` key is live while its
/// session's sidecar exists (live and abandoned sessions both have one; a
/// graceful stop deletes it). No other key is ever live.
pub fn sidecar_is_live(sessions_dir: &Path) -> impl Fn(&str) -> bool {
    let sessions_dir = sessions_dir.to_path_buf();
    move |key: &str| {
        key.strip_prefix("session-")
            .is_some_and(|id| sessions_dir.join(id).join("meta.json").exists())
    }
}

fn line_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.jsonl"))
}

fn daemon_path(dir: &Path) -> PathBuf {
    dir.join(DAEMON_FILE)
}

/// The stored record; a missing file is the default, and an unreadable one is
/// logged and read as the default.
pub fn read_daemon_record(dir: &Path) -> DaemonRecord {
    let path = daemon_path(dir);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return DaemonRecord::default(),
        Err(err) => {
            warn!(?err, path = %path.display(), "reading the activity daemon record failed");
            return DaemonRecord::default();
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(record) => record,
        Err(err) => {
            warn!(?err, path = %path.display(), "an unreadable activity daemon record reads as none");
            DaemonRecord::default()
        }
    }
}

/// Write the record through a temp file and a rename; a failure is logged and
/// swallowed like every other write here.
fn write_daemon_record(dir: &Path, record: &DaemonRecord) {
    let path = daemon_path(dir);
    let bytes = match serde_json::to_vec(record) {
        Ok(bytes) => bytes,
        Err(err) => {
            warn!(?err, "serializing the activity daemon record failed");
            return;
        }
    };
    if let Err(err) = std::fs::create_dir_all(dir) {
        warn!(?err, dir = %dir.display(), "creating the activity dir failed");
        return;
    }
    if let Err(err) = crate::atomic_file::write_atomic(&path, &bytes) {
        warn!(?err, path = %path.display(), "writing the activity daemon record failed");
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;

    /// A fresh scratch activity folder for `tag`.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rt-activity-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    /// A fixed clock reading `offset` seconds after a base instant.
    fn at(offset: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + offset, 0).expect("a fixed time")
    }

    #[test]
    fn prune_drops_old_lines_but_keeps_the_status_at_the_window_start() {
        let dir = scratch_dir("prune-old-lines");
        let now = at(0);
        let key = session_key("s1");
        append(
            &dir,
            &key,
            now - chrono::Duration::hours(3),
            SessionStatus::Idle,
        );
        append(
            &dir,
            &key,
            now - chrono::Duration::minutes(150),
            SessionStatus::Working,
        );
        append(
            &dir,
            &key,
            now - chrono::Duration::minutes(60),
            SessionStatus::AwaitingInput,
        );
        append(
            &dir,
            &key,
            now - chrono::Duration::minutes(10),
            SessionStatus::Idle,
        );

        prune(&dir, now, |_| true);

        assert_eq!(
            read_lines(&dir, &key),
            vec![
                (now - chrono::Duration::minutes(150), SessionStatus::Working),
                (
                    now - chrono::Duration::minutes(60),
                    SessionStatus::AwaitingInput
                ),
                (now - chrono::Duration::minutes(10), SessionStatus::Idle),
            ],
            "the newest line before the window stays, with its own time"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_deletes_a_file_two_hours_after_its_session() {
        let dir = scratch_dir("prune-dead-key");
        let now = at(0);
        let dead = session_key("dead");
        let live = session_key("live");
        append(
            &dir,
            &dead,
            now - chrono::Duration::hours(3),
            SessionStatus::Stopped,
        );
        append(
            &dir,
            &live,
            now - chrono::Duration::hours(3),
            SessionStatus::Working,
        );

        prune(&dir, now, |key| key == live.as_str());

        assert!(
            !line_path(&dir, &dead).exists(),
            "an ended session's log goes two hours after its last line"
        );
        assert!(
            line_path(&dir, &live).exists(),
            "a live session's log stays"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_restart_after_a_stale_alive_stamp_yields_one_gap() {
        let dir = scratch_dir("restart-gap");
        let first = at(0);
        let alive = at(30 * 60);
        let second = at(60 * 60);

        assert!(record_start(&dir, first).gaps.is_empty(), "a first start");
        stamp_alive(&dir, alive);
        let record = record_start(&dir, second);

        assert_eq!(
            record.gaps,
            vec![Gap {
                from: alive,
                to: second
            }]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_key_first_seen_after_a_start_is_gapped_from_the_window_start() {
        let dir = scratch_dir("first-seen");
        let start = at(0);
        let record = record_start(&dir, start);
        let now = start + chrono::Duration::minutes(20);

        let gaps = gaps_for(&record, Some(start + chrono::Duration::minutes(10)), now);

        assert_eq!(
            gaps,
            vec![Gap {
                from: now - WINDOW,
                to: start
            }],
            "no line before the start leaves the stretch before it blank"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_line_is_skipped() {
        let dir = scratch_dir("bad-line");
        let key = session_key("s1");
        append(&dir, &key, at(0), SessionStatus::Idle);
        let path = line_path(&dir, &key);
        let mut text = std::fs::read_to_string(&path).expect("read the log");
        text.push_str("not json\n");
        let good = serde_json::to_string(&Line {
            at: at(1),
            status: SessionStatus::Working,
        })
        .expect("serialize a line");
        text.push_str(&good);
        text.push('\n');
        std::fs::write(&path, text).expect("write the log");

        assert_eq!(
            read_lines(&dir, &key),
            vec![
                (at(0), SessionStatus::Idle),
                (at(1), SessionStatus::Working)
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_lines_orders_lines_by_time() {
        let dir = scratch_dir("read-order");
        let key = session_key("s1");
        let later = Line {
            at: at(10),
            status: SessionStatus::Idle,
        };
        let earlier = Line {
            at: at(0),
            status: SessionStatus::Working,
        };
        let mut text = serde_json::to_string(&later).expect("serialize a line");
        text.push('\n');
        text.push_str(&serde_json::to_string(&earlier).expect("serialize a line"));
        text.push('\n');
        std::fs::write(line_path(&dir, &key), text).expect("write the log");

        assert_eq!(
            read_lines(&dir, &key),
            vec![
                (at(0), SessionStatus::Working),
                (at(10), SessionStatus::Idle)
            ],
            "a line that lands after one with a later stamp still reads newest last"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gaps_for_adds_no_window_gap_when_the_daemon_started_before_the_window() {
        let now = at(0);
        let record = DaemonRecord {
            started_at: Some(now - chrono::Duration::hours(5)),
            alive_at: None,
            gaps: Vec::new(),
        };

        let gaps = gaps_for(&record, Some(now - chrono::Duration::hours(1)), now);

        assert!(
            gaps.is_empty(),
            "a daemon already running at the window's start leaves no blank stretch: {gaps:?}"
        );
        assert!(
            gaps.iter().all(|gap| gap.from < gap.to),
            "no gap runs backwards"
        );
    }

    #[test]
    fn concurrent_appends_keep_every_line() {
        let dir = scratch_dir("concurrent-appends");
        let key = session_key("s1");
        std::thread::scope(|scope| {
            for thread in 0..8_i64 {
                let dir = &dir;
                let key = key.as_str();
                scope.spawn(move || {
                    for index in 0..50_i64 {
                        append(dir, key, at(thread * 1000 + index), SessionStatus::Working);
                    }
                });
            }
        });

        let lines = read_lines(&dir, &key);
        assert_eq!(lines.len(), 400, "every appended line survives whole");
        assert!(
            lines.windows(2).all(|pair| pair[0].0 <= pair[1].0),
            "the lines read in time order"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
