//! Windows ending the session, at a shutdown, a restart or a log-off, is a
//! clean end of the run: the start guard's run marker goes, so the next start
//! does not ask about the tabs. GPUI's window procedure answers neither
//! `WM_QUERYENDSESSION` nor `WM_ENDSESSION`, and Windows sends no `WM_CLOSE`
//! at shutdown, so a subclass on the main window hears it instead.

use std::path::PathBuf;

use gpui::Window;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{WM_ENDSESSION, WM_NCDESTROY};

use crate::start_guard;

/// The subclass's id on the main window.
const SUBCLASS_ID: usize = 0x5254_1277;

/// Whether a `WM_ENDSESSION` with `wparam` ends the run cleanly: the session
/// really ends (a non-zero `wparam`). [`start_guard::mark_clean_end`] then
/// keeps the ask of a safe start still waiting on the prompt.
fn ends_cleanly(wparam: usize) -> bool {
    wparam != 0
}

/// Hooks the end of the Windows session on `window`, the main window, so it
/// clears the run marker in the config dir. A failure logs and leaves the run
/// marked, so the next start asks.
pub(crate) fn install(window: &Window) {
    let dir = match daemon_client::config_dir() {
        Ok(dir) => dir,
        Err(err) => {
            tracing::warn!("no config dir for the session-end hook: {err:#}");
            return;
        }
    };
    let Some(hwnd) = main_hwnd(window) else {
        tracing::warn!("the main window has no Win32 handle; a shutdown will ask about the tabs");
        return;
    };
    let data = Box::into_raw(Box::new(dir));
    // SAFETY: `hwnd` is the live main window, created on this thread; `data`
    // stays valid until `on_message` frees it at `WM_NCDESTROY`.
    let set = unsafe {
        SetWindowSubclass(
            hwnd,
            Some(on_message),
            SUBCLASS_ID,
            data.expose_provenance(),
        )
    };
    if !set.as_bool() {
        // SAFETY: the subclass was not set, so nothing else holds `data`.
        drop(unsafe { Box::from_raw(data) });
        tracing::warn!(
            "hooking the session end: {}",
            windows::core::Error::from_win32()
        );
    }
}

/// The Win32 handle of `window`.
fn main_hwnd(window: &Window) -> Option<HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // `Window::window_handle` is gpui's own handle; the raw one is the trait's.
    match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
        Ok(RawWindowHandle::Win32(handle)) => {
            let address = handle.hwnd.get().cast_unsigned();
            Some(HWND(std::ptr::with_exposed_provenance_mut(address)))
        }
        _ => None,
    }
}

/// The subclass procedure: clears the run marker when the session ends, and
/// unhooks itself as the window goes; every message then goes on unchanged.
unsafe extern "system" fn on_message(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    data: usize,
) -> LRESULT {
    let dir = std::ptr::with_exposed_provenance_mut::<PathBuf>(data);
    if msg == WM_ENDSESSION && ends_cleanly(wparam.0) {
        // SAFETY: `data` is the box `install` leaked, freed only at `WM_NCDESTROY`.
        if let Some(dir) = unsafe { dir.as_ref() } {
            start_guard::mark_clean_end(dir);
        }
    }
    if msg == WM_NCDESTROY {
        // SAFETY: the window is going; the subclass comes off before its box
        // is freed, so no later message reads it.
        unsafe {
            let _ = RemoveWindowSubclass(hwnd, Some(on_message), SUBCLASS_ID);
            drop(Box::from_raw(dir));
        }
    }
    // SAFETY: the arguments are the message this procedure was handed.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::ends_cleanly;

    #[test]
    fn an_ending_session_ends_the_run_cleanly() {
        assert!(ends_cleanly(1));
    }

    #[test]
    fn a_session_that_goes_on_keeps_the_marker() {
        assert!(!ends_cleanly(0), "another app refused the end");
    }
}
