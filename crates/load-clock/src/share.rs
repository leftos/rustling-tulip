//! The free-share arithmetic and the counters it reads.

use std::time::Duration;

use crate::MIN_FREE;

/// Cumulative CPU counters, summed over every processor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuCounters {
    /// CPU time spent on any work, idle excluded.
    pub busy: Duration,
    /// CPU time in total, idle included.
    pub total: Duration,
}

/// Where the clock reads the machine's load from.
pub trait LoadSource: Send + Sync {
    /// The machine's counters, None when the read failed (logged).
    fn machine(&self) -> Option<CpuCounters>;
    /// This process's own kernel + user CPU so far, None when the read failed.
    fn own_cpu(&self) -> Option<Duration>;
}

/// One read of a [`LoadSource`].
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    /// The machine's counters, None when the read failed.
    pub machine: Option<CpuCounters>,
    /// This process's own CPU, None when the read failed.
    pub own: Option<Duration>,
}

impl Sample {
    /// Reads both counters from `source`.
    pub(crate) fn take(source: &dyn LoadSource) -> Sample {
        Sample {
            machine: source.machine(),
            own: source.own_cpu(),
        }
    }
}

/// The share of the machine free for this process over one span, in `MIN_FREE..=1.0`.
///
/// It is `1 - (machine busy - own cpu) / machine total` over the span; counters that did not move, or a missing read,
/// read as a free machine (1.0). A missing own-CPU read counts as no own work.
///
/// Args:
///     before: The sample at the start of the span.
///     after: The sample at its end.
///
/// Returns:
///     The free share, never below [`MIN_FREE`].
#[must_use]
pub fn free_share(before: Sample, after: Sample) -> f64 {
    let (Some(start), Some(end)) = (before.machine, after.machine) else {
        return 1.0;
    };
    let total = end.total.saturating_sub(start.total);
    if total.is_zero() {
        return 1.0;
    }
    let busy = end.busy.saturating_sub(start.busy);
    let own = match (before.own, after.own) {
        (Some(start), Some(end)) => end.saturating_sub(start),
        _ => Duration::ZERO,
    };
    let others = busy.saturating_sub(own);
    (1.0 - others.as_secs_f64() / total.as_secs_f64()).clamp(MIN_FREE, 1.0)
}
