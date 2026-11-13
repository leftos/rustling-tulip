//! The pure core of the performance sample: the machine's CPU share between
//! two reads, and its memory. No I/O.

use chrono::{DateTime, Utc};
use protocol::{MachineSample, PerfSample};

/// The machine's cumulative CPU times, all cores together, in the source's
/// own unit (100 ns on Windows, clock ticks on macOS); only the ratio of two
/// deltas is used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CpuTimes {
    /// Time spent running anything but the idle process.
    pub(crate) busy: u64,
    /// Busy and idle time together.
    pub(crate) total: u64,
}

/// One read of the machine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RawTable {
    /// `None` when the machine's times could not be read.
    pub(crate) machine_cpu: Option<CpuTimes>,
    pub(crate) mem_used_bytes: u64,
    pub(crate) mem_total_bytes: u64,
}

/// `part` as a percent of `whole`, clamped to 0..=100.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "a CPU percent needs no more precision than an f32 holds"
)]
fn percent(part: u64, whole: u64) -> f32 {
    let share = part as f64 * 100.0 / whole as f64;
    share.clamp(0.0, 100.0) as f32
}

/// The machine's busy share between `prev` and `now`: `None` without a
/// previous reading, when either read lacks the times, or when a counter
/// stood still or went backwards.
fn machine_cpu(now: &RawTable, prev: Option<&RawTable>) -> Option<f32> {
    let now = now.machine_cpu?;
    let prev = prev?.machine_cpu?;
    let total = now
        .total
        .checked_sub(prev.total)
        .filter(|total| *total > 0)?;
    let busy = now.busy.checked_sub(prev.busy).filter(|busy| *busy > 0)?;
    Some(percent(busy, total))
}

/// One sample from this read and the one before it.
pub(crate) fn build_sample(
    now: &RawTable,
    prev: Option<&RawTable>,
    at: DateTime<Utc>,
    interval_ms: u32,
) -> PerfSample {
    PerfSample {
        at,
        interval_ms,
        machine: MachineSample {
            cpu_percent: machine_cpu(now, prev),
            mem_used_bytes: now.mem_used_bytes,
            mem_total_bytes: now.mem_total_bytes,
        },
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use chrono::{TimeZone as _, Utc};

    use super::{CpuTimes, RawTable, build_sample};

    fn table(machine: Option<(u64, u64)>) -> RawTable {
        RawTable {
            machine_cpu: machine.map(|(busy, total)| CpuTimes { busy, total }),
            mem_used_bytes: 6_000,
            mem_total_bytes: 16_000,
        }
    }

    fn cpu(now: &RawTable, prev: Option<&RawTable>) -> Option<f32> {
        sample(now, prev).machine.cpu_percent
    }

    fn sample(now: &RawTable, prev: Option<&RawTable>) -> protocol::PerfSample {
        let at = Utc
            .with_ymd_and_hms(2026, 11, 12, 10, 0, 0)
            .single()
            .expect("a valid time");
        build_sample(now, prev, at, 2000)
    }

    #[test]
    fn cpu_is_the_share_of_machine_time_between_two_reads() {
        let prev = table(Some((1_000, 10_000)));
        assert_eq!(cpu(&table(Some((3_000, 14_000))), Some(&prev)), Some(50.0));
        assert_eq!(cpu(&table(Some((2_000, 14_000))), Some(&prev)), Some(25.0));
        assert_eq!(
            cpu(&table(Some((9_000, 14_000))), Some(&prev)),
            Some(100.0),
            "clamped"
        );
        assert_eq!(
            cpu(&table(Some((1_000, 10_000))), Some(&prev)),
            None,
            "a zero delta"
        );
        assert_eq!(
            cpu(&table(Some((1_000, 14_000))), Some(&prev)),
            None,
            "a zero busy delta"
        );
        assert_eq!(
            cpu(&table(Some((500, 5_000))), Some(&prev)),
            None,
            "a counter reset"
        );
        assert_eq!(
            cpu(&table(Some((3_000, 9_000))), Some(&prev)),
            None,
            "a negative total delta"
        );
    }

    #[test]
    fn the_first_read_has_no_cpu() {
        let now = table(Some((3_000, 14_000)));
        assert_eq!(cpu(&now, None), None, "no previous reading");
        assert_eq!(cpu(&table(None), Some(&now)), None, "no times now");
        assert_eq!(cpu(&now, Some(&table(None))), None, "no times before");
    }

    #[test]
    fn machine_cpu_and_memory_come_from_the_table() {
        let got = sample(&table(Some((10, 40))), Some(&table(Some((0, 0)))));
        assert_eq!(got.machine.cpu_percent, Some(25.0));
        assert_eq!(got.machine.mem_used_bytes, 6_000);
        assert_eq!(got.machine.mem_total_bytes, 16_000);
        assert_eq!(got.interval_ms, 2000);
    }
}
