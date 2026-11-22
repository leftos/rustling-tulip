//! A clock for every ceiling a driver enforces, so that other agents' builds and test runs loading the machine do not
//! turn a fixed timeout into a false failure.
//!
//! It follows the rule `load-adjusted-timeouts` in `docs/DRIVING_CONVENTIONS.md`: the clock keeps wall time on an idle
//! machine and advances by the machine's free share while other work loads it, sampled at most once a second
//! ([`SAMPLE_PERIOD`]) with `GetSystemTimes`, this process's own CPU subtracted and never below [`MIN_FREE`]. A
//! [`Deadline`] also ends at a backstop of [`BACKSTOP_FACTOR`] times its budget in wall time, and a [`StallWatch`]
//! ends a child that neither logs nor uses CPU for [`DEFAULT_STALL_LIMIT`]. An [`Expiry`] says which fired, with the
//! load figures.
//!
//! Off Windows the system source reads nothing, so the clock runs at wall time.

use std::time::Duration;

mod clock;
mod deadline;
mod share;
mod source;
mod stall;

#[cfg(test)]
mod tests;

pub use clock::{LoadClock, Reading, SystemTicker, Ticker};
pub use deadline::{Deadline, Expiry, ExpiryKind, Figures};
pub use share::{CpuCounters, LoadSource, Sample, free_share};
pub use stall::StallWatch;

/// The least share of the machine the clock credits, however loaded it is.
pub const MIN_FREE: f64 = 0.05;

/// How many times its budget a deadline may run in wall time before its backstop ends it.
pub const BACKSTOP_FACTOR: u32 = 5;

/// The least wall time between two samples of the machine's counters.
pub const SAMPLE_PERIOD: Duration = Duration::from_secs(1);

/// How long a child may show neither a log line nor CPU before its stall watch fires.
pub const DEFAULT_STALL_LIMIT: Duration = Duration::from_secs(120);
