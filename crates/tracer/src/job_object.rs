//! Windows Job-object grouping for the PTY child + descendants.
//!
//! Why this exists: portable-pty's `Child::kill()` translates to
//! `TerminateProcess`, which only kills the immediate child of the
//! `ConPTY`. Anything claude (or the user's shell) spawned —
//! `node.exe` from `pnpm install`, cargo from a `cargo build`, dev
//! servers, GUI editors opened via `code .` — keeps running with its
//! cwd still pointing inside the worktree. Those open handles wedge
//! the `git worktree remove` / `fs::remove_dir_all` cleanup that
//! runs immediately after.
//!
//! The job-object pattern (used by VS, the GitHub CLI, and most
//! modern launchers) attaches every descendant to one kernel "job"
//! and configures the job to terminate everything when the last
//! handle to it is closed. The tracer holds the only handle; when
//! the tracer exits — normally via the shutdown path, or violently
//! via the daemon killing it — the OS closes that handle, the kill
//! triggers, and the full descendant tree dies in one shot.
//!
//! Best-effort: every step can fail (insufficient privilege, the
//! child already exited, etc.). On failure the caller logs and
//! proceeds without grouping — the legacy "just `TerminateProcess`
//! the immediate child" behavior is what we had before this module.

use std::io;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};
use windows::core::PCWSTR;

/// RAII guard that owns the job-object handle. Drop closes the
/// handle; with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` set, that
/// triggers termination of every process still in the job.
pub struct JobGuard {
    handle: HANDLE,
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        // SAFETY: handle came from CreateJobObjectW and hasn't been
        // closed yet (guard is owned). Failure here is non-actionable;
        // the OS will reap the handle when the process exits anyway.
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.handle) };
    }
}

// Job handles are not Send/Sync by default in the bindings, but a
// HANDLE is just an opaque kernel handle that the OS makes safe to
// pass across threads. The supervisor holds the guard inside a
// long-lived async task, so we manually assert these.
//
// SAFETY: kernel HANDLEs to job objects are usable from any thread —
// CloseHandle, AssignProcessToJobObject, SetInformationJobObject all
// take a HANDLE by value and are documented thread-safe.
unsafe impl Send for JobGuard {}
unsafe impl Sync for JobGuard {}

/// RAII wrapper for a process handle from `OpenProcess`. Used inside
/// [`assign_kill_on_close`] so an early `?` return doesn't leak the
/// transient handle the assignment needed.
struct ChildHandle(HANDLE);

impl Drop for ChildHandle {
    fn drop(&mut self) {
        // SAFETY: handle from OpenProcess, not yet closed.
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// Create an anonymous job object, configure it with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, open the target process for
/// `SET_QUOTA | TERMINATE` (the rights `AssignProcessToJobObject`
/// requires), and bind it. Returns a [`JobGuard`] that owns the job
/// handle for the rest of the tracer's lifetime.
pub fn assign_kill_on_close(child_pid: u32) -> io::Result<JobGuard> {
    // SAFETY: nullable lpJobAttributes + null lpName → anonymous
    // job. The call returns INVALID_HANDLE_VALUE / 0 on failure;
    // we check via Result conversion below.
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
    // SAFETY: info is a stack-allocated valid struct of the documented
    // size; the API copies out of it synchronously.
    unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::from_mut::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(&mut info).cast(),
            info_size,
        )
        .map_err(|e| io::Error::other(format!("SetInformationJobObject: {e}")))?;
    }

    // SAFETY: OpenProcess returns a handle the caller must close; we
    // wrap it in a small RAII shim (defined above so it precedes
    // statements per Clippy's items_after_statements lint) so an
    // early return doesn't leak it.
    let child_handle =
        unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, child_pid) }
            .map_err(|e| io::Error::other(format!("OpenProcess({child_pid}): {e}")))?;
    let child_handle = ChildHandle(child_handle);

    // SAFETY: both handles are live (guard not yet dropped, ChildHandle
    // owns the child handle and won't drop until the end of this fn).
    unsafe {
        AssignProcessToJobObject(job, child_handle.0)
            .map_err(|e| io::Error::other(format!("AssignProcessToJobObject: {e}")))?;
    }

    Ok(guard)
}
