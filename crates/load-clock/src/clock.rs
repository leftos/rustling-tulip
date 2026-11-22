//! The load-adjusted clock.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::SAMPLE_PERIOD;
use crate::share::{LoadSource, Sample, free_share};
use crate::source::NoLoad;

/// Where a clock reads wall time from; tests drive a manual one.
pub trait Ticker: Send + Sync {
    /// The current instant.
    fn now(&self) -> Instant;
}

/// The system's monotonic clock, [`Instant::now`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemTicker;

impl Ticker for SystemTicker {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Wall and load-adjusted time since a clock was made.
#[derive(Debug, Clone, Copy)]
pub struct Reading {
    /// Wall time since the clock was made.
    pub wall: Duration,
    /// Load-adjusted time since the clock was made.
    pub adjusted: Duration,
}

/// What a clock has integrated so far.
struct State {
    sample: Sample,
    sampled_at: Instant,
    /// Adjusted time up to `sampled_at`.
    integral: Duration,
    /// The share measured over the last span, 1 before the first.
    share: f64,
    /// The last adjusted time read, which no later read goes below.
    reported: Duration,
}

/// A clock that keeps wall time on an idle machine and advances by the machine's free share while other work loads it.
///
/// Sampling is read-driven: each [`LoadClock::read`] takes a new sample when at least [`SAMPLE_PERIOD`] has passed
/// since the last one, and credits that span, however long, at the share measured over it. Between samples it
/// extrapolates at the last share.
pub struct LoadClock {
    source: Box<dyn LoadSource>,
    ticker: Arc<dyn Ticker>,
    start: Instant,
    state: Mutex<State>,
}

impl LoadClock {
    /// The machine's clock: on Windows its CPU counters and [`Instant::now`]; wall time elsewhere.
    ///
    /// Returns:
    ///     A shared clock started now.
    #[must_use]
    pub fn system() -> Arc<LoadClock> {
        #[cfg(windows)]
        {
            LoadClock::new(
                Box::new(crate::source::WindowsSource),
                Arc::new(SystemTicker),
            )
        }
        #[cfg(not(windows))]
        {
            LoadClock::wall()
        }
    }

    /// A clock that always reads the machine as free, so its adjusted time is its wall time.
    ///
    /// Returns:
    ///     A shared clock started now.
    #[must_use]
    pub fn wall() -> Arc<LoadClock> {
        LoadClock::new(Box::new(NoLoad), Arc::new(SystemTicker))
    }

    /// A clock over a given load source and ticker, sampling the source once now.
    ///
    /// Args:
    ///     source: Where the machine's counters and this process's CPU are read.
    ///     ticker: Where wall time is read.
    ///
    /// Returns:
    ///     A shared clock started at the ticker's current instant.
    #[must_use]
    pub fn new(source: Box<dyn LoadSource>, ticker: Arc<dyn Ticker>) -> Arc<LoadClock> {
        let start = ticker.now();
        let sample = Sample::take(source.as_ref());
        let state = State {
            sample,
            sampled_at: start,
            integral: Duration::ZERO,
            share: 1.0,
            reported: Duration::ZERO,
        };
        Arc::new(LoadClock {
            source,
            ticker,
            start,
            state: Mutex::new(state),
        })
    }

    /// Wall and load-adjusted time since the clock was made, sampling first if due.
    ///
    /// Returns:
    ///     The reading; its adjusted time never goes below an earlier read's.
    pub fn read(&self) -> Reading {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let now = self.ticker.now();
        let mut since = now.saturating_duration_since(state.sampled_at);
        if since >= SAMPLE_PERIOD {
            let sample = Sample::take(self.source.as_ref());
            let measured = free_share(state.sample, sample);
            // The counters are cumulative, so the share measured over a span of any length is exact for it.
            state.integral += scale(since, measured);
            state.share = measured;
            state.sample = sample;
            state.sampled_at = now;
            since = Duration::ZERO;
        }
        let adjusted = (state.integral + scale(since, state.share)).max(state.reported);
        state.reported = adjusted;
        Reading {
            wall: now.saturating_duration_since(self.start),
            adjusted,
        }
    }
}

/// `span` times `share`, exact when the share is 1.
fn scale(span: Duration, share: f64) -> Duration {
    if share >= 1.0 {
        span
    } else {
        span.mul_f64(share)
    }
}
