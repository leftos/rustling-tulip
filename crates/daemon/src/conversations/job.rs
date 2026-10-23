//! A Windows job object that kills a process and every process it starts
//! when its guard drops.
//!
//! Killing a child kills only that process: a `cmd.exe /d /c <shim>` wrapper's
//! grandchild, or anything the agent's program starts, keeps running. Every
//! descendant of a process assigned to the job joins the job, and the job is
//! set to terminate all of them once its last handle closes. The guard holds
//! the only handle.

use std::io;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};
use windows::core::PCWSTR;

/// Owns the job's only handle. Dropping it closes the handle, and with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` set that terminates every process
/// still in the job.
pub(crate) struct JobGuard {
    handle: HANDLE,
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateJobObjectW and the guard owns
        // it, so it is still open. A failed close is not actionable; the OS
        // closes the handle when the daemon exits.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

// SAFETY: a job-object HANDLE is an opaque kernel handle usable from any
// thread; CloseHandle, AssignProcessToJobObject and SetInformationJobObject
// take it by value and are thread-safe. The runner's future holds the guard
// across awaits, so it must be Send.
unsafe impl Send for JobGuard {}
// SAFETY: as for Send; the guard exposes no method that touches the handle.
unsafe impl Sync for JobGuard {}

/// A process handle from `OpenProcess`, closed on drop so an early return
/// from [`assign_kill_on_close`] does not leak it.
struct ChildHandle(HANDLE);

impl Drop for ChildHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenProcess and is not yet closed.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Create an anonymous job set to kill its processes when its last handle
/// closes, and assign the process `child_pid` to it.
pub(crate) fn assign_kill_on_close(child_pid: u32) -> io::Result<JobGuard> {
    // SAFETY: no attributes and a null name make an anonymous job; failure
    // comes back as an Err.
    let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
        .map_err(|e| io::Error::other(format!("CreateJobObjectW: {e}")))?;
    let guard = JobGuard { handle: job };

    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
            LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            ..Default::default()
        },
        ..Default::default()
    };
    let info_size = u32::try_from(std::mem::size_of_val(&info))
        .map_err(|_| io::Error::other("JOBOBJECT_EXTENDED_LIMIT_INFORMATION too large for u32"))?;
    // SAFETY: info is a valid struct of the size passed; the call copies out
    // of it before returning.
    unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::from_mut::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(&mut info).cast(),
            info_size,
        )
        .map_err(|e| io::Error::other(format!("SetInformationJobObject: {e}")))?;
    }

    // SAFETY: OpenProcess returns a handle the caller must close, which
    // ChildHandle does.
    let child_handle =
        unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, child_pid) }
            .map_err(|e| io::Error::other(format!("OpenProcess({child_pid}): {e}")))?;
    let child_handle = ChildHandle(child_handle);

    // SAFETY: both handles are open: the guard and ChildHandle own them until
    // the end of this function at the earliest.
    unsafe {
        AssignProcessToJobObject(job, child_handle.0)
            .map_err(|e| io::Error::other(format!("AssignProcessToJobObject: {e}")))?;
    }

    Ok(guard)
}
