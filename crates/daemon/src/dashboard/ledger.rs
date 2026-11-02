//! The Dashboard's day ledger: what each listed session spent today in tokens
//! and dollars, and the commits its worktrees made, kept in `day-ledger.json`
//! as one record per local calendar day.
//!
//! A session's own figures only rise, so a day's figure is what it holds now
//! minus the base it started the day at. A session first seen today that ran
//! yesterday takes yesterday's last figures as its base, so a long session's
//! spend is not counted twice; one that started today starts at nothing; one
//! already running before the daemon first saw it today starts at its current
//! figures, because what it spent unseen is unknown.
//!
//! The commits are a set of shas per day, so a commit two sessions' worktrees
//! both contain counts once.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, NaiveDate, NaiveTime, TimeZone as _, Utc};
use protocol::DayTotals;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// How long one worktree folder's commits are reused before they are read
/// again.
pub(crate) const COMMIT_TTL: Duration = Duration::from_secs(60);

/// The shortest gap between two writes of the file.
pub(crate) const WRITE_INTERVAL: Duration = Duration::from_secs(10);

/// How many days back the file keeps; an older one is dropped on a write.
const KEEP_DAYS: i64 = 30;

/// How many commits one folder's read takes: well past a day's worth, since
/// the day's figure is the set's size and a cut would undercount it.
pub(crate) const COMMIT_LIMIT: usize = 1000;

/// The whole file: one record per local calendar day.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Days {
    #[serde(default)]
    days: BTreeMap<NaiveDate, Day>,
}

/// One local calendar day: what its sessions spent and the commits it saw.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Day {
    #[serde(default)]
    sessions: BTreeMap<String, SessionDay>,
    /// Every commit sha a listed session's worktree showed today.
    #[serde(default)]
    commits: BTreeSet<String>,
}

/// One session's figures on one day: what it holds now and the base it
/// started the day at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
struct SessionDay {
    #[serde(default)]
    base_cost_usd: f64,
    #[serde(default)]
    base_tokens: u64,
    #[serde(default)]
    cost_usd: f64,
    #[serde(default)]
    tokens: u64,
}

impl SessionDay {
    /// What the session spent today: its figures now, less its base.
    fn spent_cost(self) -> f64 {
        (self.cost_usd - self.base_cost_usd).max(0.0)
    }

    /// The tokens the session spent today: its figures now, less its base.
    fn spent_tokens(self) -> u64 {
        self.tokens.saturating_sub(self.base_tokens)
    }
}

/// One session's figures as a read pass sees them.
pub(crate) struct Seen<'a> {
    pub(crate) id: &'a str,
    /// When the session's process started, which decides its base on the day
    /// it is first seen.
    pub(crate) started_at: DateTime<Utc>,
    /// Whether it replaced abandoned sessions in place: a resumed conversation
    /// was already spending before it started, so a resume gets no zero base.
    pub(crate) resumed: bool,
    pub(crate) tokens: u64,
    pub(crate) cost_usd: f64,
}

/// The day ledger in memory and the file it is written to.
#[derive(Debug)]
pub(crate) struct Ledger {
    path: PathBuf,
    /// False when the file on disk could not be moved aside: its bytes are
    /// then never overwritten, so the ledger stays in memory for the run.
    writable: bool,
    days: Days,
    /// When each folder's commits were last read.
    commits_read: HashMap<PathBuf, Instant>,
    /// The failure last logged for each folder, so one that keeps failing
    /// logs once.
    failures: HashMap<PathBuf, String>,
    /// Whether the day records changed since the last write.
    dirty: bool,
    /// When the file was last written, and the day it was written for.
    written_at: Option<Instant>,
    written_day: Option<NaiveDate>,
}

impl Ledger {
    /// The ledger in `path`, read as of `now`: a missing file is empty, one
    /// that does not parse is moved aside and the ledger starts empty, and one
    /// that cannot be read for another reason starts empty too. A file that
    /// cannot be moved aside is never written over in the run that found it.
    pub(crate) fn open(path: PathBuf, now: DateTime<Utc>) -> Self {
        let (days, writable) = read(&path, now);
        Self {
            days,
            path,
            writable,
            commits_read: HashMap::new(),
            failures: HashMap::new(),
            dirty: false,
            written_at: None,
            written_day: Some(local_date(now)),
        }
    }

    /// The file the ledger is written to.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Record `seen`'s figures on `today`, raising its stored figures and
    /// setting its base the first time it is seen that day. Answers whether
    /// its stored figures changed.
    pub(crate) fn note(&mut self, today: NaiveDate, seen: &Seen<'_>) -> bool {
        let cost_usd = if seen.cost_usd.is_finite() {
            seen.cost_usd.max(0.0)
        } else {
            0.0
        };
        let base = match self.stored(today, seen.id) {
            Some(stored) => (stored.base_cost_usd, stored.base_tokens),
            None => self.base_for(today, seen, cost_usd),
        };
        let day = self.days.days.entry(today).or_default();
        let stored = day
            .sessions
            .entry(seen.id.to_owned())
            .or_insert(SessionDay {
                base_cost_usd: base.0,
                base_tokens: base.1,
                cost_usd: 0.0,
                tokens: 0,
            });
        let before = (stored.cost_usd, stored.tokens);
        stored.cost_usd = stored.cost_usd.max(cost_usd);
        stored.tokens = stored.tokens.max(seen.tokens);
        let changed = (stored.cost_usd, stored.tokens) != before;
        self.dirty |= changed;
        changed
    }

    /// The session's record of `today`, when it has one.
    fn stored(&self, today: NaiveDate, id: &str) -> Option<SessionDay> {
        self.days.days.get(&today)?.sessions.get(id).copied()
    }

    /// The base a session starts `today` at: yesterday's last figures when
    /// that day holds it, nothing when it started today without resuming
    /// another conversation, else its own figures now, since what it spent
    /// before it was seen today is unknown.
    fn base_for(&self, today: NaiveDate, seen: &Seen<'_>, cost_usd: f64) -> (f64, u64) {
        if let Some(previous) = today.pred_opt().and_then(|day| self.days.days.get(&day))
            && let Some(stored) = previous.sessions.get(seen.id)
        {
            return (stored.cost_usd, stored.tokens);
        }
        if !seen.resumed && local_date(seen.started_at) == today {
            return (0.0, 0);
        }
        (cost_usd, seen.tokens)
    }

    /// The folders whose commits `now` is due to read again, in path order.
    #[must_use]
    pub(crate) fn due_folders(&self, folders: &BTreeSet<PathBuf>, now: Instant) -> Vec<PathBuf> {
        folders
            .iter()
            .filter(|folder| {
                self.commits_read
                    .get(folder.as_path())
                    .is_none_or(|read| now.duration_since(*read) >= COMMIT_TTL)
            })
            .cloned()
            .collect()
    }

    /// Mark `folder`'s commits as read at `now`, whether the read worked.
    pub(crate) fn note_commit_read(&mut self, folder: &Path, now: Instant) {
        self.commits_read.insert(folder.to_path_buf(), now);
    }

    /// Add `shas` to `today`'s commits. Answers whether any was new.
    pub(crate) fn record_commits(
        &mut self,
        today: NaiveDate,
        shas: impl IntoIterator<Item = String>,
    ) -> bool {
        let day = self.days.days.entry(today).or_default();
        let mut changed = false;
        for sha in shas {
            changed |= day.commits.insert(sha);
        }
        self.dirty |= changed;
        changed
    }

    /// Log `folder`'s failed commit read at warn, once per failure text.
    pub(crate) fn record_failure(&mut self, folder: &Path, error: &str) {
        if self.failures.get(folder).is_some_and(|last| last == error) {
            return;
        }
        warn!(
            folder = %folder.display(),
            error,
            "dashboard: a worktree's commits could not be read"
        );
        self.failures.insert(folder.to_path_buf(), error.to_owned());
    }

    /// `today`'s totals: its sessions' figures after each one's base, and how
    /// many commits it saw.
    #[must_use]
    pub(crate) fn totals(&self, today: NaiveDate) -> DayTotals {
        let Some(day) = self.days.days.get(&today) else {
            return DayTotals::default();
        };
        DayTotals {
            cost_usd: day
                .sessions
                .values()
                .map(|session| session.spent_cost())
                .sum(),
            tokens: day
                .sessions
                .values()
                .map(|session| session.spent_tokens())
                .sum(),
            commits: u32::try_from(day.commits.len()).unwrap_or(u32::MAX),
            issues_done: 0,
            releases: Vec::new(),
        }
    }

    /// The file's bytes when a write is due at `now` on `today`, marking the
    /// write made: the day's figures changed, at most once every
    /// [`WRITE_INTERVAL`], or the first write of a new day.
    pub(crate) fn take_write(&mut self, today: NaiveDate, now: Instant) -> Option<Vec<u8>> {
        if !self.writable || !self.write_due(today, now) {
            return None;
        }
        prune(&mut self.days, today);
        let bytes = match serde_json::to_vec_pretty(&self.days) {
            Ok(bytes) => bytes,
            Err(err) => {
                warn!(
                    %err,
                    "dashboard: the day ledger does not serialize; the next pass writes it again"
                );
                return None;
            }
        };
        self.dirty = false;
        self.written_at = Some(now);
        self.written_day = Some(today);
        Some(bytes)
    }

    /// Whether the file is to be written at `now` on `today`: a new day at
    /// once, a changed day at most once every [`WRITE_INTERVAL`].
    fn write_due(&self, today: NaiveDate, now: Instant) -> bool {
        if self.written_day != Some(today) {
            return true;
        }
        if !self.dirty {
            return false;
        }
        self.written_at
            .is_none_or(|written| now.duration_since(written) >= WRITE_INTERVAL)
    }
}

/// Drop the days more than [`KEEP_DAYS`] before `today`.
fn prune(days: &mut Days, today: NaiveDate) {
    days.days
        .retain(|day, _| today.signed_duration_since(*day).num_days() <= KEEP_DAYS);
}

/// The local calendar date of `at`.
pub(crate) fn local_date(at: DateTime<Utc>) -> NaiveDate {
    at.with_timezone(&Local).date_naive()
}

/// The UTC instant local midnight starting `today` falls on.
#[must_use]
pub(crate) fn local_midnight_utc(today: NaiveDate) -> DateTime<Utc> {
    let midnight = today.and_time(NaiveTime::MIN);
    Local
        .from_local_datetime(&midnight)
        .earliest()
        .map_or_else(|| midnight.and_utc(), |local| local.with_timezone(&Utc))
}

/// The records at `path` and whether the file may be written: a missing file
/// is empty; one that does not parse is moved aside, so its bytes are never
/// overwritten in place; one that cannot be read for another reason counts as
/// empty.
fn read(path: &Path, now: DateTime<Utc>) -> (Days, bool) {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(days) => (days, true),
            Err(err) => {
                warn!(
                    path = %path.display(),
                    %err,
                    "dashboard: the day ledger does not parse; it is moved aside and starts empty"
                );
                (Days::default(), move_aside(path, now))
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (Days::default(), true),
        Err(err) => {
            warn!(
                path = %path.display(),
                %err,
                "dashboard: could not read the day ledger; it starts empty"
            );
            (Days::default(), true)
        }
    }
}

/// Rename a ledger that does not parse to `day-ledger.json.corrupt-<stamp>`,
/// answering whether the old bytes left the ledger's path.
fn move_aside(path: &Path, now: DateTime<Utc>) -> bool {
    let name = path.file_name().map_or_else(
        || "day-ledger.json".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let moved = path.with_file_name(format!("{name}.corrupt-{}", now.format("%Y%m%dT%H%M%SZ")));
    match std::fs::rename(path, &moved) {
        Ok(()) => true,
        Err(err) => {
            warn!(
                path = %path.display(),
                to = %moved.display(),
                %err,
                "dashboard: the day ledger could not be moved aside; it is not written this run"
            );
            false
        }
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod tests {
    use super::*;
    use crate::server::test_support::ScratchDir;

    fn day(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").expect("a date")
    }

    /// Noon UTC on `text`: the local calendar date is `text` in every zone the
    /// tests can run in.
    fn noon(text: &str) -> DateTime<Utc> {
        day(text)
            .and_hms_opt(12, 0, 0)
            .expect("a valid time")
            .and_utc()
    }

    fn seen(id: &str, started_at: DateTime<Utc>, tokens: u64, cost_usd: f64) -> Seen<'_> {
        Seen {
            id,
            started_at,
            resumed: false,
            tokens,
            cost_usd,
        }
    }

    /// A session that resumed another in place.
    fn resumed(id: &str, started_at: DateTime<Utc>, tokens: u64, cost_usd: f64) -> Seen<'_> {
        Seen {
            resumed: true,
            ..seen(id, started_at, tokens, cost_usd)
        }
    }

    /// A ledger over a scratch folder's `day-ledger.json`.
    fn ledger(scratch: &ScratchDir, opened_on: &str) -> Ledger {
        Ledger::open(scratch.path().join("day-ledger.json"), noon(opened_on))
    }

    #[test]
    fn a_figure_only_rises() {
        let scratch = ScratchDir::new("day-ledger-rises");
        let mut ledger = ledger(&scratch, "2026-10-28");
        let today = day("2026-10-28");
        let started = noon("2026-10-28");

        assert!(ledger.note(today, &seen("s1", started, 100, 1.0)));
        assert!(!ledger.note(today, &seen("s1", started, 80, 0.5)));

        let totals = ledger.totals(today);
        assert_eq!(totals.tokens, 100);
        assert!((totals.cost_usd - 1.0).abs() < 1e-9);
    }

    #[test]
    fn an_ended_session_still_counts() {
        let scratch = ScratchDir::new("day-ledger-ended");
        let mut ledger = ledger(&scratch, "2026-10-28");
        let today = day("2026-10-28");
        let started = noon("2026-10-28");

        ledger.note(today, &seen("s1", started, 100, 2.0));
        ledger.note(today, &seen("s1", started, 0, 0.0));

        let totals = ledger.totals(today);
        assert_eq!(totals.tokens, 100);
        assert!((totals.cost_usd - 2.0).abs() < 1e-9);
    }

    #[test]
    fn a_new_local_date_starts_empty() {
        let scratch = ScratchDir::new("day-ledger-new-day");
        let mut ledger = ledger(&scratch, "2026-10-28");
        let first = day("2026-10-28");
        let second = day("2026-10-29");

        ledger.note(first, &seen("s1", noon("2026-10-28"), 100, 1.0));
        assert_eq!(ledger.totals(second), DayTotals::default());

        ledger.note(second, &seen("s1", noon("2026-10-28"), 140, 1.4));
        assert_eq!(
            ledger.totals(second).tokens,
            40,
            "the second day pays only what it added"
        );
        assert_eq!(ledger.totals(first).tokens, 100, "the first day is kept");
    }

    #[test]
    fn a_session_started_today_has_a_zero_base() {
        let scratch = ScratchDir::new("day-ledger-started-today");
        let mut ledger = ledger(&scratch, "2026-10-28");
        let today = day("2026-10-28");

        ledger.note(today, &seen("s1", noon("2026-10-28"), 50, 0.5));

        assert_eq!(ledger.totals(today).tokens, 50);
    }

    #[test]
    fn a_session_seen_first_today_but_started_yesterday_keeps_yesterdays_last_as_its_base() {
        let scratch = ScratchDir::new("day-ledger-carried");
        let mut ledger = ledger(&scratch, "2026-10-27");
        let yesterday = day("2026-10-27");
        let today = day("2026-10-28");
        let started = noon("2026-10-27");

        ledger.note(yesterday, &seen("s1", started, 40, 0.4));
        assert_eq!(ledger.totals(today).tokens, 0, "nothing on today yet");

        ledger.note(today, &seen("s1", started, 70, 0.7));
        let totals = ledger.totals(today);
        assert_eq!(totals.tokens, 30);
        assert!((totals.cost_usd - 0.3).abs() < 1e-9);
    }

    #[test]
    fn a_session_first_seen_today_with_no_record_starts_at_its_current_figures() {
        let scratch = ScratchDir::new("day-ledger-unseen");
        let mut ledger = ledger(&scratch, "2026-10-28");
        let today = day("2026-10-28");

        ledger.note(today, &seen("s1", noon("2026-10-24"), 70, 0.7));

        assert_eq!(
            ledger.totals(today),
            DayTotals::default(),
            "what it spent before the daemon saw it is unknown"
        );
    }

    #[test]
    fn a_resumed_session_started_today_takes_its_current_figures_as_base() {
        let scratch = ScratchDir::new("day-ledger-resumed");
        let mut ledger = ledger(&scratch, "2026-10-28");
        let today = day("2026-10-28");

        ledger.note(today, &resumed("s1", noon("2026-10-28"), 70, 0.7));

        assert_eq!(
            ledger.totals(today),
            DayTotals::default(),
            "the conversation it resumed had already spent what it shows"
        );
    }

    #[test]
    fn commits_are_deduped_by_sha() {
        let scratch = ScratchDir::new("day-ledger-commits");
        let mut ledger = ledger(&scratch, "2026-10-28");
        let today = day("2026-10-28");

        let first = ledger.record_commits(today, ["a".to_owned(), "b".to_owned()]);
        let second = ledger.record_commits(today, ["b".to_owned(), "c".to_owned()]);
        let third = ledger.record_commits(today, ["a".to_owned()]);

        assert!(first, "two new commits");
        assert!(second, "one new commit");
        assert!(!third, "a sha already counted is not new");
        assert_eq!(ledger.totals(today).commits, 3);
    }

    #[test]
    fn days_older_than_thirty_are_pruned() {
        let scratch = ScratchDir::new("day-ledger-prune");
        let path = scratch.path().join("day-ledger.json");
        let mut ledger = Ledger::open(path.clone(), noon("2026-10-30"));
        let today = day("2026-10-30");

        let too_old = day("2026-09-28");
        let kept = day("2026-09-30");
        ledger.note(too_old, &seen("s1", noon("2026-09-28"), 10, 0.1));
        ledger.note(kept, &seen("s2", noon("2026-09-30"), 20, 0.2));
        let now = Instant::now();
        let bytes = ledger
            .take_write(today, now)
            .expect("the first write is due");
        crate::atomic_file::write_atomic(&path, &bytes).expect("the ledger writes");

        let reopened = Ledger::open(path, noon("2026-10-30"));
        assert!(
            reopened.days.days.contains_key(&kept),
            "a day 30 days back is kept"
        );
        assert!(
            !reopened.days.days.contains_key(&too_old),
            "a day older than 30 days is dropped"
        );
    }

    #[test]
    fn a_corrupt_file_is_moved_aside_and_starts_empty() {
        let scratch = ScratchDir::new("day-ledger-corrupt");
        let path = scratch.path().join("day-ledger.json");
        std::fs::write(&path, b"not json").expect("seed a corrupt ledger");

        let ledger = Ledger::open(path.clone(), noon("2026-10-28"));

        assert!(ledger.days.days.is_empty(), "it starts empty");
        assert!(!path.exists(), "the old bytes leave the ledger's path");
        let moved: Vec<PathBuf> = std::fs::read_dir(scratch.path())
            .expect("read the scratch folder")
            .map(|entry| entry.expect("a folder entry").path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("day-ledger.json.corrupt-"))
            })
            .collect();
        assert_eq!(moved.len(), 1, "{moved:?}");
        assert_eq!(
            std::fs::read(&moved[0]).expect("read the moved file"),
            b"not json"
        );
    }

    #[test]
    fn a_corrupt_file_that_cannot_be_moved_is_never_overwritten() {
        let scratch = ScratchDir::new("day-ledger-unmovable");
        let path = scratch.path().join("day-ledger.json");
        std::fs::write(&path, b"not json").expect("seed a corrupt ledger");
        let now = noon("2026-10-28");
        let blocked = path.with_file_name(format!(
            "day-ledger.json.corrupt-{}",
            now.format("%Y%m%dT%H%M%SZ")
        ));
        std::fs::create_dir(&blocked).expect("block the corrupt target with a folder");

        let mut ledger = Ledger::open(path.clone(), now);
        assert!(ledger.days.days.is_empty(), "it starts empty");
        let today = day("2026-10-28");
        ledger.note(today, &seen("s1", noon("2026-10-28"), 10, 0.1));

        assert!(
            ledger.take_write(today, Instant::now()).is_none(),
            "a ledger that could not move its corrupt file writes nothing"
        );
        assert_eq!(
            std::fs::read(&path).expect("read the ledger"),
            b"not json",
            "the bytes it could not move aside are never overwritten"
        );
    }

    #[test]
    fn a_missing_file_starts_empty() {
        let scratch = ScratchDir::new("day-ledger-missing");
        let path = scratch.path().join("day-ledger.json");

        let ledger = Ledger::open(path.clone(), noon("2026-10-28"));

        assert!(ledger.days.days.is_empty());
        assert!(!path.exists(), "opening one writes nothing");
    }
}
