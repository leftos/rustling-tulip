//! The load sources: the Windows counters, and the one that reads nothing.

use std::time::Duration;

use crate::share::{CpuCounters, LoadSource};

/// A source that reads nothing, so the clock runs at wall time.
pub(crate) struct NoLoad;

impl LoadSource for NoLoad {
    fn machine(&self) -> Option<CpuCounters> {
        None
    }

    fn own_cpu(&self) -> Option<Duration> {
        None
    }
}

#[cfg(windows)]
pub(crate) use windows_source::WindowsSource;

#[cfg(windows)]
mod windows_source {
    use std::time::Duration;

    use tracing::warn;
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes, GetSystemTimes};

    use crate::share::{CpuCounters, LoadSource};

    /// The machine read through `GetSystemTimes`, this process through `GetProcessTimes`.
    pub(crate) struct WindowsSource;

    impl LoadSource for WindowsSource {
        fn machine(&self) -> Option<CpuCounters> {
            let mut idle = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            // SAFETY: each pointer is to a live, writable local `FILETIME` that
            // outlives the call, and no two of them alias.
            let read = unsafe {
                GetSystemTimes(
                    Some(&raw mut idle),
                    Some(&raw mut kernel),
                    Some(&raw mut user),
                )
            };
            if let Err(error) = read {
                warn!(%error, "load clock: reading the machine's CPU times failed; this span reads as free");
                return None;
            }
            // Kernel time includes idle time, so kernel and user together are the total.
            let total = filetime_units(kernel).checked_add(filetime_units(user))?;
            Some(CpuCounters {
                busy: units_to_duration(total.saturating_sub(filetime_units(idle))),
                total: units_to_duration(total),
            })
        }

        fn own_cpu(&self) -> Option<Duration> {
            let mut creation = FILETIME::default();
            let mut exit = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            // SAFETY: `GetCurrentProcess` returns a pseudo-handle that is always
            // valid for this process and needs no closing; each pointer is to a
            // live, writable local `FILETIME` that outlives the call, and no two
            // of them alias.
            let read = unsafe {
                GetProcessTimes(
                    GetCurrentProcess(),
                    &raw mut creation,
                    &raw mut exit,
                    &raw mut kernel,
                    &raw mut user,
                )
            };
            if let Err(error) = read {
                warn!(%error, "load clock: reading this process's CPU times failed; this span counts no own work");
                return None;
            }
            let units = filetime_units(kernel).checked_add(filetime_units(user))?;
            Some(units_to_duration(units))
        }
    }

    /// A `FILETIME` as one 64-bit count of 100 ns units.
    fn filetime_units(time: FILETIME) -> u64 {
        (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
    }

    /// A count of 100 ns units as a duration, without overflow.
    fn units_to_duration(units: u64) -> Duration {
        const PER_SECOND: u64 = 10_000_000;
        Duration::from_secs(units / PER_SECOND) + Duration::from_nanos((units % PER_SECOND) * 100)
    }
}
