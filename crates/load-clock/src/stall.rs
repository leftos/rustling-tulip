//! The stall watch: a child that neither logs nor uses CPU.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::clock::Ticker;
use crate::deadline::{Expiry, ExpiryKind, Figures};

/// Watches a child's log length and CPU, and fires once neither has grown for its limit of wall time.
///
/// It counts wall time, since a stalled child uses no CPU whatever the load.
pub struct StallWatch {
    ticker: Arc<dyn Ticker>,
    limit: Duration,
    last_growth: Instant,
    log_len: u64,
    cpu: Duration,
}

impl StallWatch {
    /// A watch that starts counting now.
    ///
    /// Args:
    ///     ticker: Where wall time is read.
    ///     limit: How long the child may show no growth; [`crate::DEFAULT_STALL_LIMIT`] as a rule.
    ///
    /// Returns:
    ///     The watch, its first growth window starting at the ticker's current instant.
    #[must_use]
    pub fn new(ticker: Arc<dyn Ticker>, limit: Duration) -> StallWatch {
        let last_growth = ticker.now();
        StallWatch {
            ticker,
            limit,
            last_growth,
            log_len: 0,
            cpu: Duration::ZERO,
        }
    }

    /// Record what the child showed: its log length and its CPU so far.
    ///
    /// Either one larger than last seen restarts the window; a smaller one (a rotated log) only moves the baseline.
    ///
    /// Args:
    ///     `log_len`: The child's log length in bytes.
    ///     cpu: The child's kernel + user CPU so far.
    pub fn observe(&mut self, log_len: u64, cpu: Duration) {
        if log_len > self.log_len || cpu > self.cpu {
            self.last_growth = self.ticker.now();
        }
        self.log_len = log_len;
        self.cpu = cpu;
    }

    /// Some(Expiry{kind: Stall, ..}) once neither has grown for `limit` of wall time.
    ///
    /// Returns:
    ///     The stall, its figures' budget the limit and its wall and adjusted time the time since the last growth.
    #[must_use]
    pub fn stalled(&self) -> Option<Expiry> {
        let quiet = self
            .ticker
            .now()
            .saturating_duration_since(self.last_growth);
        (quiet >= self.limit).then_some(Expiry {
            kind: ExpiryKind::Stall,
            figures: Figures {
                budget: self.limit,
                wall: quiet,
                adjusted: quiet,
            },
        })
    }
}
