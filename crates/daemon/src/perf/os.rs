//! This host's machine source: the CPU's cumulative times and the memory in
//! use, read from the OS. Windows and macOS have one; other hosts have none
//! and list no performance monitor.

use std::sync::Arc;

use super::PerfSource;

/// This host's machine source, or `None` on a platform without one.
#[cfg(windows)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the same signature returns None on a host without a source"
)]
pub(crate) fn machine_source() -> Option<Arc<dyn PerfSource>> {
    Some(Arc::new(windows_source::WindowsSource))
}

/// This host's machine source, or `None` on a platform without one.
#[cfg(target_os = "macos")]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the same signature returns None on a host without a source"
)]
pub(crate) fn machine_source() -> Option<Arc<dyn PerfSource>> {
    Some(Arc::new(mac_source::MacSource::new()))
}

/// This host's machine source, or `None` on a platform without one.
#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn machine_source() -> Option<Arc<dyn PerfSource>> {
    None
}

#[cfg(windows)]
mod windows_source {
    use tracing::warn;
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows::Win32::System::Threading::GetSystemTimes;

    use super::super::PerfSource;
    use super::super::sample::{CpuTimes, RawTable};

    /// The machine read through `GetSystemTimes` and `GlobalMemoryStatusEx`.
    pub(super) struct WindowsSource;

    impl PerfSource for WindowsSource {
        fn read(&self) -> Option<RawTable> {
            let mut status = MEMORYSTATUSEX {
                dwLength: u32::try_from(size_of::<MEMORYSTATUSEX>()).ok()?,
                ..MEMORYSTATUSEX::default()
            };
            // SAFETY: `status` is a live, writable local whose `dwLength`
            // holds its own size, as the call requires, for the call's whole
            // duration.
            if let Err(error) = unsafe { GlobalMemoryStatusEx(&raw mut status) } {
                warn!(%error, "perf: reading the machine's memory failed; skipping this tick");
                return None;
            }
            Some(RawTable {
                machine_cpu: cpu_times(),
                mem_used_bytes: status.ullTotalPhys.saturating_sub(status.ullAvailPhys),
                mem_total_bytes: status.ullTotalPhys,
            })
        }
    }

    /// A `FILETIME` as one 64-bit count of 100 ns units.
    fn filetime_units(time: FILETIME) -> u64 {
        (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
    }

    /// The machine's cumulative times, `None` when the call failed. Kernel
    /// time includes idle time, so kernel and user together are the total.
    fn cpu_times() -> Option<CpuTimes> {
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
            warn!(%error, "perf: reading the machine's CPU times failed; this sample has no CPU");
            return None;
        }
        let total = filetime_units(kernel).checked_add(filetime_units(user))?;
        Some(CpuTimes {
            busy: total.saturating_sub(filetime_units(idle)),
            total,
        })
    }
}

#[cfg(target_os = "macos")]
mod mac_source {
    use std::sync::Mutex;

    use super::super::PerfSource;
    use super::super::sample::{CpuTimes, RawTable};
    use crate::sync::lock;

    /// The machine read through `host_statistics` and sysinfo's memory.
    pub(super) struct MacSource {
        /// This host's port, taken once: each `mach_host_self` call adds a
        /// send right.
        host: libc::mach_port_t,
        system: Mutex<sysinfo::System>,
    }

    impl MacSource {
        pub(super) fn new() -> Self {
            // SAFETY: `mach_host_self` takes no arguments and only returns a
            // send right to this host's port.
            #[expect(
                deprecated,
                reason = "libc's mach_host_self is the call this source needs; mach2 is not a dependency"
            )]
            let host = unsafe { libc::mach_host_self() };
            Self {
                host,
                system: Mutex::new(sysinfo::System::new()),
            }
        }

        /// The machine's cumulative ticks, `None` when the call failed.
        fn cpu_times(&self) -> Option<CpuTimes> {
            let mut info = libc::host_cpu_load_info {
                cpu_ticks: Default::default(),
            };
            let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
            // SAFETY: `info` is a live, writable `host_cpu_load_info` and
            // `count` holds its size in `integer_t` units, so the kernel
            // writes no more than `info` holds; both outlive the call.
            let result = unsafe {
                libc::host_statistics(
                    self.host,
                    libc::HOST_CPU_LOAD_INFO,
                    (&raw mut info).cast(),
                    &raw mut count,
                )
            };
            if result != libc::KERN_SUCCESS {
                return None;
            }
            let ticks = |state: libc::c_int| {
                usize::try_from(state)
                    .ok()
                    .and_then(|index| info.cpu_ticks.get(index))
                    .map(|ticks| u64::from(*ticks))
            };
            let busy = ticks(libc::CPU_STATE_USER)?
                .checked_add(ticks(libc::CPU_STATE_SYSTEM)?)?
                .checked_add(ticks(libc::CPU_STATE_NICE)?)?;
            let total = busy.checked_add(ticks(libc::CPU_STATE_IDLE)?)?;
            Some(CpuTimes { busy, total })
        }
    }

    impl PerfSource for MacSource {
        fn read(&self) -> Option<RawTable> {
            let (used, total) = {
                let mut system = lock(&self.system);
                system.refresh_memory();
                (system.used_memory(), system.total_memory())
            };
            if total == 0 {
                return None;
            }
            Some(RawTable {
                machine_cpu: self.cpu_times(),
                mem_used_bytes: used,
                mem_total_bytes: total,
            })
        }
    }
}

#[cfg(test)]
#[cfg(any(windows, target_os = "macos"))]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use super::machine_source;

    #[test]
    fn the_machine_source_reads_memory_and_rising_cpu_times() {
        let source = machine_source().expect("this host has a source");
        let first = source.read().expect("the first read");
        let second = source.read().expect("the second read");
        let mut totals = Vec::new();
        for table in [&first, &second] {
            assert!(table.mem_total_bytes > 0, "{table:?}");
            assert!(table.mem_used_bytes > 0, "{table:?}");
            assert!(table.mem_used_bytes <= table.mem_total_bytes, "{table:?}");
            let times = table.machine_cpu.expect("the CPU times were read");
            assert!(times.busy <= times.total, "{times:?}");
            totals.push(times.total);
        }
        assert!(
            totals[1] >= totals[0],
            "the totals went backwards: {totals:?}"
        );
    }
}
