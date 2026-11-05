//! The usage history the fleet forecast reads: one sample per counted
//! account per fetch, kept in memory for as long as the longest rate window.

use claude_accounts::pace::AccountReading;
use jiff::{SignedDuration, Timestamp};

/// How long a sample stays: the longest rate window, 180 minutes, and a
/// quarter hour of slack.
const KEEP: SignedDuration = SignedDuration::from_mins(195);

/// The usage samples the fleet forecast reads, one per counted account per
/// fetch, oldest first.
#[derive(Debug, Default)]
pub(crate) struct UsageHistory {
    pub(crate) samples: Vec<AccountReading>,
}

impl UsageHistory {
    /// Records `readings`, each at the fetch time it carries, as of `now`: a
    /// reading newer than its account's last sample is kept, one whose fetch
    /// went back past that sample clears the account's samples and starts
    /// them again, an account left out of `readings` loses its samples, and
    /// a sample fetched more than [`KEEP`] before `now` is pruned.
    pub(crate) fn record(&mut self, readings: &[AccountReading], now: Timestamp) {
        self.samples.retain(|sample| {
            readings
                .iter()
                .any(|reading| reading.account == sample.account)
        });
        for reading in readings {
            let went_back = self
                .last(reading.account)
                .is_some_and(|last| reading.fetched_at < last);
            if went_back {
                self.samples
                    .retain(|sample| sample.account != reading.account);
            }
        }
        for reading in readings {
            let newer = self
                .last(reading.account)
                .is_none_or(|last| reading.fetched_at > last);
            if newer {
                self.samples.push(*reading);
            }
        }
        if let Ok(cutoff) = now.checked_sub(KEEP) {
            self.samples.retain(|sample| sample.fetched_at >= cutoff);
        }
    }

    /// The newest fetch time `account` has a sample at; `None` when it has
    /// none.
    fn last(&self, account: u32) -> Option<Timestamp> {
        self.samples
            .iter()
            .filter(|sample| sample.account == account)
            .map(|sample| sample.fetched_at)
            .max()
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::{KEEP, UsageHistory};
    use claude_accounts::pace::{AccountReading, WindowReading};
    use jiff::{SignedDuration, Timestamp};

    /// A fixed time, `seconds` past an arbitrary epoch.
    fn at(seconds: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + seconds).expect("the test's time is in range")
    }

    /// Account `account`'s reading, fetched at `fetched`, its 5-hour window
    /// at `pct` with a reset four hours out.
    fn reading(account: u32, fetched: Timestamp, pct: f64) -> AccountReading {
        AccountReading {
            account,
            fetched_at: fetched,
            five_hour: Some(WindowReading {
                pct,
                resets_at: fetched + SignedDuration::from_hours(4),
            }),
            seven_day: None,
            unstarted_five_hour: None,
        }
    }

    #[test]
    fn history_dedupes_a_repeated_fetch() {
        let mut history = UsageHistory::default();
        let now = at(0);
        let sample = reading(1, now, 10.0);

        history.record(&[sample], now);
        history.record(&[sample], now + SignedDuration::from_mins(1));

        assert_eq!(history.samples.len(), 1, "{:?}", history.samples);
        assert_eq!(history.samples[0].fetched_at, now, "{:?}", history.samples);
    }

    #[test]
    fn history_prunes_samples_older_than_195_minutes() {
        let mut history = UsageHistory::default();
        let now = at(0);
        let kept = now - KEEP + SignedDuration::from_mins(1);
        let dropped = now - KEEP - SignedDuration::from_mins(1);

        history.record(&[reading(1, dropped, 5.0)], dropped);
        history.record(&[reading(1, kept, 10.0)], now);

        assert_eq!(history.samples.len(), 1, "{:?}", history.samples);
        assert_eq!(history.samples[0].fetched_at, kept, "{:?}", history.samples);
    }

    #[test]
    fn history_drops_a_departed_account() {
        let mut history = UsageHistory::default();
        let now = at(0);

        history.record(&[reading(1, now, 10.0), reading(2, now, 20.0)], now);
        history.record(&[reading(1, now, 10.0)], now);

        assert_eq!(history.samples.len(), 1, "{:?}", history.samples);
        assert_eq!(history.samples[0].account, 1, "{:?}", history.samples);
    }

    #[test]
    fn history_drops_an_account_whose_fetch_went_back() {
        let mut history = UsageHistory::default();
        let now = at(0);
        let back = now - SignedDuration::from_mins(5);

        history.record(&[reading(1, now, 10.0)], now);
        history.record(&[reading(1, back, 12.0)], now);

        assert_eq!(history.samples.len(), 1, "{:?}", history.samples);
        assert_eq!(
            history.samples[0].fetched_at, back,
            "a fetch that went back starts the account's samples again: {:?}",
            history.samples
        );
    }
}
