//! Find processes holding open handles inside a directory.
//!
//! Used by the worktree cleanup path when `git worktree remove --force`
//! and the `fs::remove_dir_all` fallback both fail: the dir is wedged
//! by a still-running grandchild (a `node.exe` from `pnpm install`, a
//! `cargo` background indexer, the user's editor, etc.). Surfacing the
//! offending process names + pids lets the user decide whether to kill
//! them and retry.
//!
//! Windows: uses the Restart Manager API (`Rm*` family), the same path
//! Microsoft's installers use. No admin required. Cmdline is enriched
//! per pid via the existing `sysinfo` dep so the UI can show "node.exe
//! — pnpm run dev" instead of just "node.exe".
//!
//! Non-Windows: returns an empty list. The protocol message still
//! surfaces in the UI as a "worktree didn't clean up" notice; the
//! locking-process list is just empty there.

#[cfg(windows)]
mod windows_impl;

/// One process holding an open handle inside the queried path.
#[derive(Debug, Clone)]
pub struct LockingProcess {
    pub pid: u32,
    /// Friendly name reported by Restart Manager (typically the
    /// executable name without path).
    pub name: String,
    /// Full command line (program + args) joined with spaces. `None`
    /// when sysinfo can't enrich (the process exited between the RM
    /// snapshot and the sysinfo refresh).
    pub cmdline: Option<String>,
}

/// Enumerate processes locking any file inside `paths`. Best-effort —
/// returns an empty list on API failures (which the caller treats as
/// "we couldn't find any locker, surface the path as stuck anyway").
#[cfg(windows)]
pub fn processes_holding(paths: &[&std::path::Path]) -> Vec<LockingProcess> {
    windows_impl::processes_holding(paths)
}

#[cfg(not(windows))]
pub fn processes_holding(_paths: &[&std::path::Path]) -> Vec<LockingProcess> {
    Vec::new()
}

/// Two-phase termination for processes holding a worktree open.
///
/// Windows path: open a Restart Manager session against `paths` and ask
/// `RmShutdown` for a polite close (an editor with unsaved buffers
/// gets to prompt to save) — then after a short grace period, anything
/// in `pids` that's still alive is hard-killed via `TerminateProcess`.
///
/// Non-Windows path: `SIGTERM` via sysinfo for the listed pids, then a
/// hard `SIGKILL` after the grace period.
///
/// `paths` is only used to drive `RmShutdown`'s graceful close; pid-level
/// kills always go through `pids`. The split exists because the user's
/// modal lets them deselect specific pids to spare (e.g. their editor)
/// while still wanting the polite-close to fire for the worktree as a
/// whole.
pub async fn terminate_processes(paths: &[&std::path::Path], pids: &[u32]) {
    #[cfg(windows)]
    {
        windows_impl::terminate_processes(paths, pids).await;
    }
    #[cfg(not(windows))]
    {
        let _ = paths; // not used outside Windows
        terminate_unix(pids).await;
    }
}

#[cfg(not(windows))]
async fn terminate_unix(pids: &[u32]) {
    use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, Signal, System};
    if pids.is_empty() {
        return;
    }
    let mut sys = System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::everything()),
    );
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    for pid in pids {
        if let Some(proc) = sys.process(Pid::from_u32(*pid)) {
            let _ = proc.kill_with(Signal::Term);
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    for pid in pids {
        if let Some(proc) = sys.process(Pid::from_u32(*pid)) {
            proc.kill();
        }
    }
}
