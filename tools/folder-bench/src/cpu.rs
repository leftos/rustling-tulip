//! CPU time (kernel + user) of this process and of children it started.

use std::process::Child;

/// This process's CPU time so far, in milliseconds.
pub fn own_cpu_ms() -> Option<u64> {
    #[cfg(windows)]
    {
        // SAFETY: returns the current-process pseudo-handle; no preconditions.
        let handle = unsafe { windows::Win32::System::Threading::GetCurrentProcess() };
        handle_cpu_ms(handle)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// `child`'s CPU time, read through its handle (valid after it exited, while
/// `child` is alive).
pub fn child_cpu_ms(child: &Child) -> Option<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle as _;
        handle_cpu_ms(windows::Win32::Foundation::HANDLE(child.as_raw_handle()))
    }
    #[cfg(not(windows))]
    {
        let _ = child;
        None
    }
}

#[cfg(windows)]
fn handle_cpu_ms(handle: windows::Win32::Foundation::HANDLE) -> Option<u64> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::GetProcessTimes;

    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: a process handle the caller keeps alive for this call, and
    // out-pointers to locals.
    unsafe {
        GetProcessTimes(
            handle,
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    }
    .ok()?;
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    // FILETIME counts 100 ns ticks.
    Some((ticks(kernel) + ticks(user)) / 10_000)
}

/// The sum of the parts, `None` when any part is unknown: a partial total
/// would read as a measurement.
pub fn sum(parts: impl IntoIterator<Item = Option<u64>>) -> Option<u64> {
    parts
        .into_iter()
        .try_fold(0u64, |acc, part| part.map(|n| acc + n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sum_with_an_unknown_part_is_unknown() {
        assert_eq!(sum([Some(3), Some(4)]), Some(7));
        assert_eq!(sum([Some(3), None, Some(4)]), None);
        assert_eq!(sum([None]), None);
        assert_eq!(sum([]), Some(0));
    }
}
