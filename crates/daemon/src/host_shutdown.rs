//! Whether the host is shutting down or logging off.

/// Whether a shutdown or logoff is under way. Windows kills the daemon and
/// its tracers without warning once one starts, so a session whose exit the
/// daemon still sees must keep its sidecar and write no history entry: the
/// next start records it lost and auto-resumes it, as it does after a reboot.
#[must_use]
#[cfg(windows)]
pub fn in_progress() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SHUTTINGDOWN};
    // SAFETY: reads a process-wide metric; it takes no pointers and has no
    // preconditions.
    unsafe { GetSystemMetrics(SM_SHUTTINGDOWN) != 0 }
}

/// Whether a shutdown or logoff is under way. Always false off Windows,
/// which has no such system-wide flag.
#[must_use]
#[cfg(not(windows))]
pub fn in_progress() -> bool {
    false
}

#[cfg(test)]
mod tests {
    /// A daemon whose host is up reports no shutdown; the running tests are
    /// the normal session this reads.
    #[cfg(windows)]
    #[test]
    fn host_shutdown_is_not_in_progress_during_tests() {
        assert!(
            !super::in_progress(),
            "no shutdown is under way while the tests run"
        );
    }
}
