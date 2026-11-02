//! The Dashboard's two-hour activity track: each listed session's status log
//! as stretches of `working`, `asking` and `idle`, plus the stretches no
//! daemon recorded. A stretch never covers time no daemon was running, and it
//! resumes after such a gap when the log shows the same status again.
//!
//! Only the reads under the activity folder touch the disk; everything else
//! is a function of the log lines and `now`, so tests drive it with fixed
//! times.

use std::path::Path;

use chrono::{DateTime, Utc};
use protocol::{ActivitySegment, SegmentActivity, SegmentKind, SessionStatus};

use crate::activity_log;

/// The hours an [`protocol::DaemonMessage::ActivityTracks`] message says its
/// track covers: the span of [`activity_log::WINDOW`].
pub(crate) const TRACK_HOURS: u32 = 2;

/// One session the Dashboard lists, as its track needs it.
pub(crate) struct TrackedSession {
    pub(crate) id: String,
    /// The lane it draws in: its workspace's name, else its repo's name.
    pub(crate) repo: String,
    /// Its display label.
    pub(crate) label: String,
}

/// The start of the window `now` closes: every stretch or gap beginning
/// before it is clipped to it.
#[must_use]
pub(crate) fn window_start(now: DateTime<Utc>) -> DateTime<Utc> {
    now - activity_log::WINDOW
}

/// Every listed session's stretches and gaps over the last
/// [`activity_log::WINDOW`], sorted by lane, session key and start.
#[must_use]
pub(crate) fn build(
    dir: &Path,
    sessions: &[TrackedSession],
    now: DateTime<Utc>,
) -> Vec<ActivitySegment> {
    // Read once for the whole pass: every session's gaps come from it.
    let record = activity_log::read_daemon_record(dir);
    let window_start = window_start(now);
    let mut segments = Vec::new();
    for session in sessions {
        let key = activity_log::session_key(&session.id);
        let lines = activity_log::read_lines(dir, &key);
        let gaps = activity_log::gaps_for(&record, lines.first().map(|(at, _)| *at), now);
        segments.extend(stretches(session, &key, &lines, &gaps, window_start));
        segments.extend(gap_segments(session, &key, &gaps, window_start));
    }
    segments.sort_by(|a, b| {
        (a.repo.as_str(), a.key.as_str(), a.from).cmp(&(b.repo.as_str(), b.key.as_str(), b.from))
    });
    segments
}

/// Whether two passes draw the same track: a `from` at or before its pass's
/// window start counts as equal there, so a stretch carried in from before
/// the window compares the same at any later time.
#[must_use]
pub(crate) fn same_tracks(
    a: &[ActivitySegment],
    b: &[ActivitySegment],
    a_window_start: DateTime<Utc>,
    b_window_start: DateTime<Utc>,
) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(a, b)| {
            a.repo == b.repo
                && a.key == b.key
                && a.kind == b.kind
                && a.label == b.label
                && a.activity == b.activity
                && clipped_start(a.from, a_window_start) == clipped_start(b.from, b_window_start)
                && a.to == b.to
        })
}

/// A `from` at or before its pass's window start is that window's own edge,
/// so two passes agree on it however far the window has moved.
fn clipped_start(from: DateTime<Utc>, window_start: DateTime<Utc>) -> Option<DateTime<Utc>> {
    (from > window_start).then_some(from)
}

/// The stretches one session's `lines` draw over the window starting at
/// `window_start`, cut where a `gaps` entry means no daemon recorded.
fn stretches(
    session: &TrackedSession,
    key: &str,
    lines: &[(DateTime<Utc>, SessionStatus)],
    gaps: &[activity_log::Gap],
    window_start: DateTime<Utc>,
) -> Vec<ActivitySegment> {
    // The newest line from before the window carries the status the window
    // opens in; every older one is outside it.
    let carried = lines.iter().rposition(|(at, _)| *at < window_start);
    let first = carried.unwrap_or(0);
    let mut segments: Vec<ActivitySegment> = Vec::new();
    for (index, (at, status)) in lines.iter().enumerate().skip(first) {
        let Some(activity) = activity(*status) else {
            continue;
        };
        let from = if index == first && carried.is_some() {
            window_start
        } else {
            *at
        };
        let to = lines.get(index + 1).map(|(next, _)| *next);
        for (from, to) in cut(from, to, gaps) {
            match segments.last_mut() {
                Some(last) if last.activity == Some(activity) && last.to == Some(from) => {
                    last.to = to;
                }
                _ => segments.push(ActivitySegment {
                    repo: session.repo.clone(),
                    key: key.to_owned(),
                    kind: SegmentKind::Session,
                    label: session.label.clone(),
                    activity: Some(activity),
                    from,
                    to,
                }),
            }
        }
    }
    segments
}

/// `(from, to)` with every `gap` removed, in order: a stretch ends where the
/// gap starts, and the same activity resumes where the gap ends.
fn cut(
    mut from: DateTime<Utc>,
    to: Option<DateTime<Utc>>,
    gaps: &[activity_log::Gap],
) -> Vec<(DateTime<Utc>, Option<DateTime<Utc>>)> {
    let mut pieces = Vec::new();
    for gap in gaps {
        if to.is_some_and(|to| to <= gap.from) || from >= gap.to {
            continue;
        }
        if from < gap.from {
            pieces.push((from, Some(gap.from)));
        }
        from = gap.to;
        if to.is_some_and(|to| to <= gap.to) {
            return pieces;
        }
    }
    pieces.push((from, to));
    pieces
}

/// The stretches no daemon recorded, clipped to the window.
fn gap_segments(
    session: &TrackedSession,
    key: &str,
    gaps: &[activity_log::Gap],
    window_start: DateTime<Utc>,
) -> Vec<ActivitySegment> {
    gaps.iter()
        .map(|gap| ActivitySegment {
            repo: session.repo.clone(),
            key: key.to_owned(),
            kind: SegmentKind::Gap,
            label: session.label.clone(),
            activity: None,
            from: gap.from.max(window_start),
            to: Some(gap.to),
        })
        .collect()
}

/// What a status draws as; `None` for one that only ends the stretch before
/// it, and starts none of its own.
fn activity(status: SessionStatus) -> Option<SegmentActivity> {
    match status {
        SessionStatus::Working => Some(SegmentActivity::Working),
        SessionStatus::AwaitingInput => Some(SegmentActivity::Asking),
        SessionStatus::Idle | SessionStatus::Spawning => Some(SegmentActivity::Idle),
        SessionStatus::Stopped | SessionStatus::Error => None,
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use chrono::Duration;
    use std::path::PathBuf;

    /// A fresh scratch activity folder for `tag`.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rt-tracks-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    /// A fixed clock reading `offset` seconds after a base instant.
    fn at(offset: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + offset, 0).expect("a fixed time")
    }

    fn minutes(count: i64) -> Duration {
        Duration::minutes(count)
    }

    fn hours(count: i64) -> Duration {
        Duration::hours(count)
    }

    /// The one listed session the tests draw.
    fn session(id: &str) -> TrackedSession {
        TrackedSession {
            id: id.to_owned(),
            repo: "repo".to_owned(),
            label: "repo: main".to_owned(),
        }
    }

    /// The segments of `kind` in `segments`.
    fn of_kind(segments: &[ActivitySegment], kind: SegmentKind) -> Vec<&ActivitySegment> {
        segments
            .iter()
            .filter(|segment| segment.kind == kind)
            .collect()
    }

    /// Panics when a session stretch of `key` covers time its gap does.
    fn assert_no_overlap(segments: &[ActivitySegment], key: &str) {
        let gaps = of_kind(segments, SegmentKind::Gap);
        for stretch in of_kind(segments, SegmentKind::Session) {
            for gap in &gaps {
                let end = gap.to.expect("a gap always has an end");
                let overlaps = stretch.from < end && stretch.to.is_none_or(|to| to > gap.from);
                assert!(
                    !overlaps,
                    "{key}: the stretch from {:?} spans the gap {:?}..{end}",
                    stretch.from, gap.from
                );
            }
        }
    }

    /// `id`'s log, with each `(age, status)` line written `age` before `now`.
    fn log(
        dir: &Path,
        id: &str,
        now: DateTime<Utc>,
        lines: &[(Duration, SessionStatus)],
    ) -> String {
        let key = activity_log::session_key(id);
        for (age, status) in lines {
            activity_log::append(dir, &key, now - *age, *status);
        }
        key
    }

    /// Write `record` as the daemon's own activity record.
    fn write_record(dir: &Path, record: &activity_log::DaemonRecord) {
        let bytes = serde_json::to_vec(record).expect("serialize the record");
        std::fs::write(dir.join("daemon.json"), bytes).expect("write the record");
    }

    #[test]
    fn an_open_stretch_has_no_to() {
        let dir = scratch_dir("open");
        let now = at(0);
        let key = log(&dir, "s1", now, &[(minutes(10), SessionStatus::Working)]);

        let segments = build(&dir, &[session("s1")], now);

        assert_eq!(
            segments,
            vec![ActivitySegment {
                repo: "repo".to_owned(),
                key,
                kind: SegmentKind::Session,
                label: "repo: main".to_owned(),
                activity: Some(SegmentActivity::Working),
                from: now - minutes(10),
                to: None,
            }]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stretch_before_the_window_starts_at_the_window() {
        let dir = scratch_dir("window");
        let now = at(0);
        let key = log(
            &dir,
            "s1",
            now,
            &[
                (hours(3), SessionStatus::Idle),
                (hours(1), SessionStatus::Working),
            ],
        );

        let segments = build(&dir, &[session("s1")], now);
        let stretches = of_kind(&segments, SegmentKind::Session);

        assert_eq!(stretches.len(), 2, "{segments:?}");
        assert_eq!(stretches[0].key, key);
        assert_eq!(stretches[0].activity, Some(SegmentActivity::Idle));
        assert_eq!(stretches[0].from, now - activity_log::WINDOW);
        assert_eq!(stretches[0].to, Some(now - hours(1)));
        assert_eq!(stretches[1].activity, Some(SegmentActivity::Working));
        assert_eq!(stretches[1].from, now - hours(1));
        assert_eq!(stretches[1].to, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn neighbouring_same_activity_stretches_merge() {
        let dir = scratch_dir("merge");
        let now = at(0);
        log(
            &dir,
            "s1",
            now,
            &[
                (minutes(50), SessionStatus::Working),
                (minutes(40), SessionStatus::Working),
            ],
        );

        let segments = build(&dir, &[session("s1")], now);
        let stretches = of_kind(&segments, SegmentKind::Session);

        assert_eq!(stretches.len(), 1, "{segments:?}");
        assert_eq!(stretches[0].from, now - minutes(50));
        assert_eq!(stretches[0].to, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stopped_line_ends_the_stretch() {
        let dir = scratch_dir("stopped");
        let now = at(0);
        log(
            &dir,
            "s1",
            now,
            &[
                (minutes(50), SessionStatus::Working),
                (minutes(20), SessionStatus::Stopped),
            ],
        );

        let segments = build(&dir, &[session("s1")], now);
        let stretches = of_kind(&segments, SegmentKind::Session);

        assert_eq!(stretches.len(), 1, "{segments:?}");
        assert_eq!(stretches[0].activity, Some(SegmentActivity::Working));
        assert_eq!(stretches[0].from, now - minutes(50));
        assert_eq!(stretches[0].to, Some(now - minutes(20)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn awaiting_input_is_asking() {
        let dir = scratch_dir("asking");
        let now = at(0);
        log(
            &dir,
            "s1",
            now,
            &[(minutes(10), SessionStatus::AwaitingInput)],
        );

        let segments = build(&dir, &[session("s1")], now);

        assert_eq!(segments.len(), 1, "{segments:?}");
        assert_eq!(segments[0].activity, Some(SegmentActivity::Asking));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_restart_gap_is_a_gap_segment() {
        let dir = scratch_dir("gap");
        let now = at(0);
        let before = now - minutes(100);
        let _ = activity_log::record_start(&dir, before);
        let key = log(&dir, "s1", now, &[(minutes(100), SessionStatus::Idle)]);
        let _ = activity_log::record_start(&dir, now - minutes(30));

        let segments = build(&dir, &[session("s1")], now);
        let gaps = of_kind(&segments, SegmentKind::Gap);

        assert_eq!(gaps.len(), 1, "{segments:?}");
        assert_eq!(gaps[0].key, key);
        assert_eq!(gaps[0].label, "repo: main");
        assert_eq!(gaps[0].activity, None);
        assert_eq!(gaps[0].from, before);
        assert_eq!(gaps[0].to, Some(now - minutes(30)));
        assert_no_overlap(&segments, &key);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stretch_resumes_after_a_restart_gap() {
        let dir = scratch_dir("resume");
        let now = at(0);
        write_record(
            &dir,
            &activity_log::DaemonRecord {
                started_at: Some(now - minutes(30)),
                alive_at: Some(now - minutes(30)),
                gaps: vec![activity_log::Gap {
                    from: now - minutes(60),
                    to: now - minutes(30),
                }],
            },
        );
        let key = log(&dir, "s1", now, &[(minutes(100), SessionStatus::Idle)]);

        let segments = build(&dir, &[session("s1")], now);
        let stretches = of_kind(&segments, SegmentKind::Session);
        let gaps = of_kind(&segments, SegmentKind::Gap);

        assert_eq!(stretches.len(), 2, "{segments:?}");
        assert_eq!(stretches[0].activity, Some(SegmentActivity::Idle));
        assert_eq!(stretches[0].from, now - minutes(100));
        assert_eq!(stretches[0].to, Some(now - minutes(60)));
        assert_eq!(stretches[1].activity, Some(SegmentActivity::Idle));
        assert_eq!(stretches[1].from, now - minutes(30));
        assert_eq!(stretches[1].to, None);
        assert_eq!(gaps.len(), 1, "{segments:?}");
        assert_eq!(gaps[0].from, now - minutes(60));
        assert_eq!(gaps[0].to, Some(now - minutes(30)));
        assert_no_overlap(&segments, &key);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_with_no_lines_yields_nothing() {
        let dir = scratch_dir("none");

        let segments = build(&dir, &[session("s1")], at(0));

        assert!(segments.is_empty(), "{segments:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
