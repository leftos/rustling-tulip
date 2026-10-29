//! The summarizer's spend ledger: what the providers billed on the current
//! local day, kept in `summarizer-spend.json` as `{"date", "spent_usd"}`, and
//! the cap rules over it. A record for another day reads as nothing spent.

use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::summarizer::prompt::Trigger;

/// One local day's spend, as the file holds it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct Record {
    date: NaiveDate,
    spent_usd: f64,
}

/// The day's spend in memory, and the file it is written to.
#[derive(Debug)]
pub(crate) struct Ledger {
    path: PathBuf,
    record: Record,
}

impl Ledger {
    /// The ledger at `path` as of `today`. A missing file, a file that does
    /// not parse (logged) or a record of another day is nothing spent.
    pub(crate) fn load(path: PathBuf, today: NaiveDate) -> Self {
        let record = read(&path)
            .filter(|record| record.date == today)
            .unwrap_or(Record {
                date: today,
                spent_usd: 0.0,
            });
        Self { path, record }
    }

    /// The file the ledger is written to.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// US dollars spent on `today`.
    pub(crate) fn spent(&self, today: NaiveDate) -> f64 {
        if self.record.date == today {
            self.record.spent_usd
        } else {
            0.0
        }
    }

    /// Add `cost_usd` to `today`'s spend, starting the day at nothing when the
    /// record is of another day; a cost that is not a positive number adds
    /// nothing. Returns the day's spend.
    pub(crate) fn add(&mut self, today: NaiveDate, cost_usd: f64) -> f64 {
        if self.record.date != today {
            self.record = Record {
                date: today,
                spent_usd: 0.0,
            };
        }
        if cost_usd.is_finite() && cost_usd > 0.0 {
            self.record.spent_usd += cost_usd;
        }
        self.record.spent_usd
    }

    /// The file's text for the current record.
    ///
    /// # Errors
    ///
    /// The record does not serialize.
    pub(crate) fn to_bytes(&self) -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec_pretty(&self.record)?)
    }
}

/// The record at `path`, or `None` when there is none or it cannot be read
/// (logged).
fn read(path: &Path) -> Option<Record> {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(record) => Some(record),
            Err(err) => {
                warn!(path = %path.display(), %err, "summarizer: the spend ledger does not parse; counting today as nothing spent");
                None
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            warn!(path = %path.display(), %err, "summarizer: could not read the spend ledger; counting today as nothing spent");
            None
        }
    }
}

/// Whether a call for `trigger` may run with `spent_usd` spent today under a
/// cap of `cap_usd`: a working refresh stops at half the cap, a stop summary
/// at the cap.
pub(crate) fn allows(trigger: Trigger, spent_usd: f64, cap_usd: f64) -> bool {
    match trigger {
        Trigger::Stop => spent_usd < cap_usd,
        Trigger::Working => spent_usd < cap_usd / 2.0,
    }
}

/// Whether the day's spend has reached the cap.
pub(crate) fn cap_reached(spent_usd: f64, cap_usd: f64) -> bool {
    spent_usd >= cap_usd
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod tests {
    use super::*;
    use crate::server::test_support::ScratchDir;

    fn day(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").expect("a date")
    }

    fn save(ledger: &Ledger) {
        let bytes = ledger.to_bytes().expect("the record serializes");
        crate::orphan::write_atomic(ledger.path(), &bytes).expect("the ledger writes");
    }

    #[test]
    fn a_new_local_day_resets_the_spend() {
        let scratch = ScratchDir::new("summ-ledger");
        let path = scratch.path().join("summarizer-spend.json");
        let first = day("2026-10-28");
        let second = day("2026-10-29");

        let mut ledger = Ledger::load(path.clone(), first);
        assert!(
            ledger.spent(first).abs() < f64::EPSILON,
            "a missing file is nothing spent"
        );
        ledger.add(first, 0.25);
        ledger.add(first, 0.5);
        save(&ledger);

        let same_day = Ledger::load(path.clone(), first);
        assert!((same_day.spent(first) - 0.75).abs() < 1e-9);
        assert!(
            same_day.spent(second).abs() < f64::EPSILON,
            "another day reads as nothing spent"
        );

        let next_day = Ledger::load(path.clone(), second);
        assert!(next_day.spent(second).abs() < f64::EPSILON);

        let mut rolled = Ledger::load(path.clone(), first);
        assert!(
            (rolled.add(second, 0.1) - 0.1).abs() < 1e-9,
            "the new day starts at nothing"
        );
        save(&rolled);
        let text = std::fs::read_to_string(&path).expect("read the ledger");
        let stored: serde_json::Value = serde_json::from_str(&text).expect("the ledger is JSON");
        assert_eq!(stored["date"], "2026-10-29");
    }
}
