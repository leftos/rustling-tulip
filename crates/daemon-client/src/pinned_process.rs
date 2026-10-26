//! A process pinned by a Windows handle: once [`PinnedProcess::open`] holds
//! the handle, it names that one process object, so a kill through it can
//! never reach another process that later reuses the pid.

use anyhow::Context as _;
use std::time::Duration;
use windows::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
};
use windows::core::HRESULT;

/// How often [`PinnedProcess::wait`] looks whether the process has exited.
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Why [`PinnedProcess::open`] could not pin a pid.
#[derive(Debug)]
pub(crate) enum OpenFailure {
    /// No process has the pid, or the one that has it already exited.
    NotRunning,
    /// A process may have the pid, but this client may not open it to
    /// terminate it (a daemon an elevated client spawned, for one).
    NoRights(windows::core::Error),
}

/// What an `OpenProcess` error says about the pid: `ERROR_INVALID_PARAMETER`
/// means no process has it. Any other error (`ERROR_ACCESS_DENIED` above all)
/// cannot rule out a live process, so it is never read as "not running".
fn open_failure(err: windows::core::Error) -> OpenFailure {
    if err.code() == HRESULT::from_win32(ERROR_INVALID_PARAMETER.0) {
        OpenFailure::NotRunning
    } else {
        OpenFailure::NoRights(err)
    }
}

/// A handle to a running process, opened for terminate and synchronize, and
/// closed on drop.
pub(crate) struct PinnedProcess {
    handle: HANDLE,
    pid: u32,
}

// SAFETY: a process handle is a kernel object reference that any thread may
// use; this type only passes it to the thread-safe Win32 calls below and
// closes it once, on drop.
unsafe impl Send for PinnedProcess {}
// SAFETY: as above; no method mutates the handle.
unsafe impl Sync for PinnedProcess {}

impl PinnedProcess {
    /// Pins `pid`.
    ///
    /// # Errors
    ///
    /// [`OpenFailure::NotRunning`] when no process has `pid` or the one that
    /// has it already exited; [`OpenFailure::NoRights`] when a process has it
    /// but this client may not open it to terminate it.
    pub(crate) fn open(pid: u32) -> Result<Self, OpenFailure> {
        // SAFETY: OpenProcess takes no pointers; the handle it returns is
        // owned by `Self` from here on and closed exactly once, on drop.
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, pid) }
            .map_err(open_failure)?;
        let pinned = Self { handle, pid };
        if pinned.has_exited() {
            return Err(OpenFailure::NotRunning);
        }
        Ok(pinned)
    }

    /// Force-terminates the pinned process. A process that has already exited
    /// counts as terminated.
    ///
    /// # Errors
    ///
    /// Fails when `TerminateProcess` does and the process is still running.
    pub(crate) fn terminate(&self) -> anyhow::Result<()> {
        // SAFETY: `self.handle` is a live process handle opened with
        // PROCESS_TERMINATE and not yet closed.
        match unsafe { TerminateProcess(self.handle, 1) } {
            Ok(()) => Ok(()),
            Err(_) if self.has_exited() => Ok(()),
            Err(err) => Err(err).with_context(|| format!("terminating pid {}", self.pid)),
        }
    }

    /// Waits up to `timeout` for the pinned process to exit; `true` once it
    /// has.
    pub(crate) async fn wait(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if self.has_exited() {
                return true;
            }
            tokio::time::sleep(EXIT_POLL_INTERVAL).await;
        }
        self.has_exited()
    }

    fn has_exited(&self) -> bool {
        // SAFETY: `self.handle` is a live process handle opened with
        // PROCESS_SYNCHRONIZE; a zero timeout only samples its state.
        let state = unsafe { WaitForSingleObject(self.handle, 0) };
        state == WAIT_OBJECT_0
    }
}

impl Drop for PinnedProcess {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenProcess and is closed only here.
        if let Err(err) = unsafe { CloseHandle(self.handle) } {
            tracing::debug!(pid = self.pid, %err, "closing the pinned process handle failed");
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{OpenFailure, PinnedProcess, open_failure};
    use crate::test_fakes::stand_in;
    use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER};
    use windows::core::{Error, HRESULT};

    #[test]
    fn open_errors_say_whether_the_pid_runs_or_may_not_be_stopped() {
        let denied = open_failure(Error::from_hresult(HRESULT::from_win32(
            ERROR_ACCESS_DENIED.0,
        )));
        assert!(matches!(denied, OpenFailure::NoRights(_)), "{denied:?}");

        let missing = open_failure(Error::from_hresult(HRESULT::from_win32(
            ERROR_INVALID_PARAMETER.0,
        )));
        assert!(matches!(missing, OpenFailure::NotRunning), "{missing:?}");
    }

    #[test]
    fn terminating_an_already_exited_pinned_process_counts_as_stopped() {
        let mut child = stand_in();
        let pinned = PinnedProcess::open(child.id()).expect("pin the running stand-in");
        child.kill().expect("kill the stand-in");
        child.wait().expect("reap the stand-in");

        pinned
            .terminate()
            .expect("a process that already exited counts as stopped");
    }
}
