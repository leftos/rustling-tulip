//! Windows-specific Restart Manager wrapper. See parent module docs.
//!
//! The Restart Manager flow is:
//!   1. `RmStartSession` — get a session handle.
//!   2. `RmRegisterResources` — tell RM which file paths to query.
//!   3. `RmGetList` (twice) — first call returns the buffer size needed;
//!      second call fills the array.
//!   4. `RmEndSession` — release the session.
//!
//! Buffer sizing: RM returns `ERROR_MORE_DATA` from the first
//! `RmGetList` when the caller-provided count is too small, with the
//! actual count in `nProcInfoNeeded`. We allocate and retry up to 4×
//! to handle a brief race where more processes attach a handle between
//! the two calls. After 4 retries we just take what we have.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;

use sysinfo::{ProcessRefreshKind, RefreshKind, System};
use tracing::{debug, warn};
use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::RestartManager::{
    CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources,
    RmShutdown, RmStartSession,
};
use windows::core::{PCWSTR, PWSTR};

use super::LockingProcess;

const MAX_GETLIST_RETRIES: usize = 4;

pub fn processes_holding(paths: &[&Path]) -> Vec<LockingProcess> {
    if paths.is_empty() {
        return Vec::new();
    }

    let mut handle: u32 = 0;
    // RmStartSession fills this buffer with a GUID-shaped session
    // identifier. CCH_RM_SESSION_KEY (32) is the documented length;
    // the +1 reserves space for the trailing NUL.
    let mut session_key = [0u16; (CCH_RM_SESSION_KEY + 1) as usize];
    // SAFETY: out-pointer for session handle; PWSTR over a stack
    // buffer sized to the documented CCH_RM_SESSION_KEY + 1.
    let start_err = unsafe {
        RmStartSession(&raw mut handle, None, PWSTR(session_key.as_mut_ptr()))
    };
    if start_err != ERROR_SUCCESS {
        debug!(err = start_err.0, "RmStartSession failed");
        return Vec::new();
    }

    let infos = match register(handle, paths) {
        Ok(()) => collect(handle),
        Err(err) => Err(err),
    };

    // SAFETY: matched with RmStartSession. Errors here are logged but
    // don't affect the returned list.
    let end_err = unsafe { RmEndSession(handle) };
    if end_err != ERROR_SUCCESS {
        debug!(err = end_err.0, "RmEndSession failed");
    }

    match infos {
        Ok(infos) => enrich(infos),
        Err(err) => {
            debug!(err = err.0, "Restart Manager registration / query failed");
            Vec::new()
        }
    }
}

fn register(handle: u32, paths: &[&Path]) -> Result<(), WIN32_ERROR> {
    // PCWSTR is a C-string-shaped pointer; the wide buffers must
    // outlive the call, so collect them first, then build the pointer
    // slice from them.
    let wide: Vec<Vec<u16>> = paths.iter().map(|p| to_wide(p.as_os_str())).collect();
    let ptrs: Vec<PCWSTR> = wide.iter().map(|w| PCWSTR(w.as_ptr())).collect();

    // SAFETY: ptrs covers the registered file count and remains valid
    // for the duration of the call (wide and ptrs live until function
    // return). No applications/services are registered.
    let err = unsafe { RmRegisterResources(handle, Some(&ptrs), None, None) };
    if err == ERROR_SUCCESS { Ok(()) } else { Err(err) }
}

fn collect(handle: u32) -> Result<Vec<RM_PROCESS_INFO>, WIN32_ERROR> {
    let mut needed: u32 = 0;
    let mut have: u32 = 0;
    let mut reason: u32 = 0; // RmRebootReasonNone

    // First call with a zero-size buffer to get the needed count.
    // SAFETY: needed/have/reason are valid out-pointers; None for the
    // process array is fine when have == 0 (RM returns ERROR_MORE_DATA).
    let err = unsafe {
        RmGetList(
            handle,
            &raw mut needed,
            &raw mut have,
            None,
            &raw mut reason,
        )
    };
    if err == ERROR_SUCCESS {
        return Ok(Vec::new());
    }
    if err != ERROR_MORE_DATA {
        return Err(err);
    }

    let mut infos: Vec<RM_PROCESS_INFO> = Vec::new();
    for _ in 0..MAX_GETLIST_RETRIES {
        if needed == 0 {
            return Ok(Vec::new());
        }
        let needed_usize = needed as usize;
        infos = vec![RM_PROCESS_INFO::default(); needed_usize];
        have = needed;
        // SAFETY: infos is sized for `have` entries; pointer remains
        // valid throughout the call; out-params are stack scalars.
        let err = unsafe {
            RmGetList(
                handle,
                &raw mut needed,
                &raw mut have,
                Some(infos.as_mut_ptr()),
                &raw mut reason,
            )
        };
        if err == ERROR_SUCCESS {
            infos.truncate(have as usize);
            return Ok(infos);
        }
        if err != ERROR_MORE_DATA {
            return Err(err);
        }
        // ERROR_MORE_DATA again — more processes attached a handle in
        // the gap. Loop with the new `needed` value.
    }

    // Hit the retry cap. Return whatever we got rather than failing.
    infos.truncate(have as usize);
    Ok(infos)
}

fn enrich(infos: Vec<RM_PROCESS_INFO>) -> Vec<LockingProcess> {
    if infos.is_empty() {
        return Vec::new();
    }
    let mut sys = System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::everything()),
    );
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    let mut out = Vec::with_capacity(infos.len());
    for info in infos {
        let pid = info.Process.dwProcessId;
        let name = wide_to_string(&info.strAppName);
        let cmdline = sys
            .process(sysinfo::Pid::from_u32(pid))
            .and_then(|proc| {
                let parts: Vec<String> = proc
                    .cmd()
                    .iter()
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect();
                if parts.is_empty() {
                    None
                } else {
                    Some(parts.join(" "))
                }
            });
        out.push(LockingProcess { pid, name, cmdline });
    }
    if out.is_empty() {
        warn!("Restart Manager returned processes but enrichment yielded none");
    }
    out
}

/// `OsStr` → UTF-16 with trailing NUL, as `PCWSTR` expects.
fn to_wide(s: &OsStr) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_wide().collect();
    v.push(0);
    v
}

/// Wide buffer (NUL-terminated) → String. Stops at the first NUL.
fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Phase-1 graceful (via `RmShutdown`) + Phase-2 hard kill (via
/// `TerminateProcess` from sysinfo). See parent module docs.
pub async fn terminate_processes(paths: &[&Path], pids: &[u32]) {
    if !paths.is_empty() {
        graceful_shutdown(paths);
        // RmShutdown is synchronous but the OS still needs a moment to
        // tear down processes that politely accepted the close (e.g.
        // node.exe finishing a flush). 2 s matches the equivalent grace
        // window in the Unix SIGTERM path.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    hard_kill_survivors(pids);
}

fn graceful_shutdown(paths: &[&Path]) {
    let mut handle: u32 = 0;
    let mut session_key = [0u16; (CCH_RM_SESSION_KEY + 1) as usize];
    // SAFETY: out-pointer for handle; PWSTR over a stack buffer sized
    // to CCH_RM_SESSION_KEY + 1.
    let start_err = unsafe {
        RmStartSession(&raw mut handle, None, PWSTR(session_key.as_mut_ptr()))
    };
    if start_err != ERROR_SUCCESS {
        debug!(err = start_err.0, "graceful_shutdown: RmStartSession failed");
        return;
    }

    if register(handle, paths).is_ok() {
        // RmShutdown: 0 = no flags (polite). Pass None for the callback
        // — we don't need progress reporting for this short operation.
        // SAFETY: handle is valid from RmStartSession.
        let err = unsafe { RmShutdown(handle, 0, None) };
        if err != ERROR_SUCCESS {
            debug!(err = err.0, "graceful_shutdown: RmShutdown failed");
        }
    }

    // SAFETY: matched with RmStartSession.
    let _ = unsafe { RmEndSession(handle) };
}

fn hard_kill_survivors(pids: &[u32]) {
    if pids.is_empty() {
        return;
    }
    let mut sys = System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::everything()),
    );
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    for pid in pids {
        if let Some(proc) = sys.process(sysinfo::Pid::from_u32(*pid)) {
            // sysinfo's `kill` calls TerminateProcess on Windows. Returns
            // false when the process is already gone or the daemon
            // doesn't have terminate rights (e.g. admin-only process).
            // Either way, we just continue — the caller's retry loop
            // surfaces the lingering hold.
            proc.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::RestartManager::CCH_RM_MAX_APP_NAME;

    #[test]
    fn wide_to_string_handles_nul_terminator() {
        let mut buf = [0u16; CCH_RM_MAX_APP_NAME as usize + 1];
        for (i, c) in "node.exe\0".encode_utf16().enumerate() {
            buf[i] = c;
        }
        assert_eq!(wide_to_string(&buf), "node.exe");
    }

    #[test]
    fn wide_to_string_handles_unterminated_buffer() {
        // No NUL at all — should consume the entire slice.
        let buf: [u16; 4] = [0x004E, 0x004F, 0x0044, 0x0045];
        assert_eq!(wide_to_string(&buf), "NODE");
    }

    #[test]
    fn empty_paths_returns_empty_list_without_calling_rm() {
        let out = super::super::processes_holding(&[]);
        assert!(out.is_empty());
    }
}
