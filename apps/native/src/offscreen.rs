//! Opening a window cloaked and never activated: the mode the smoke tier and
//! the benchmarks run the client in.

use gpui::Window;

/// When set to a non-empty value, the window opens cloaked and is never
/// activated, so the smoke specs never cover or take focus from the user's
/// work. This mode must never call `activate`: gpui's activate path sends a
/// synthetic Alt through `SendInput` and calls `SetForegroundWindow`, which
/// would reach the user's keyboard state and take their focus.
pub const OFFSCREEN_ENV: &str = "RUSTLING_TULIP_OFFSCREEN_WINDOW";

/// Whether the environment asks for a cloaked window, read from
/// [`OFFSCREEN_ENV`].
#[must_use]
pub fn requested() -> bool {
    std::env::var_os(OFFSCREEN_ENV).is_some_and(|value| !value.is_empty())
}

/// The exit code of a process whose window could not be cloaked.
pub const CLOAK_FAILED_EXIT: i32 = 3;

/// The exit code of a driven run that was refused: its environment fails
/// the driver gate, or its driver pipe could not be created.
pub const DRIVER_REFUSED_EXIT: i32 = 4;

/// Places `window` at the primary display's work-area origin at `size` in
/// logical pixels (gpui applies neither to a window it opens hidden), marks
/// it never to be activated, puts it at the bottom of the Z order, cloaks it
/// and shows it without activating it. The compositor still renders a
/// cloaked window, so its pixels can be read, but the user never sees it. A
/// window that cannot be cloaked is never shown: the process exits with
/// [`CLOAK_FAILED_EXIT`], unless the window closed first, which leaves
/// nothing to show.
#[cfg(windows)]
pub fn show_cloaked(window: &Window, size: (u16, u16)) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // `Window::window_handle` is gpui's own handle; the raw one is the trait's.
    let raw = HasWindowHandle::window_handle(window).map(|handle| handle.as_raw());
    let address = match raw {
        Ok(RawWindowHandle::Win32(handle)) => handle.hwnd.get().cast_unsigned(),
        other => {
            tracing::error!("{OFFSCREEN_ENV}: the window has no Win32 handle ({other:?})");
            return;
        }
    };
    // Moving and showing the window sends it messages that gpui handles at
    // once. Sent from here, inside gpui's own callback, they find its state
    // borrowed and are lost, so the window never learns its size. Sent from
    // another thread, they wait for gpui's message loop.
    let spawned = std::thread::Builder::new()
        .name("cloak-window".to_owned())
        .spawn(move || cloak_and_show(address, size));
    if let Err(err) = spawned {
        tracing::error!("{OFFSCREEN_ENV}: starting the thread that shows the window: {err}");
    }
}

/// The work of [`show_cloaked`] on the window whose handle is `address`.
#[cfg(windows)]
fn cloak_and_show(address: usize, size: (u16, u16)) {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::{
        HWND_BOTTOM, SPI_GETWORKAREA, SW_SHOWNOACTIVATE, SWP_NOACTIVATE,
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetWindowPos, ShowWindow, SystemParametersInfoW,
    };
    use windows::core::BOOL;
    let hwnd = HWND(std::ptr::with_exposed_provenance_mut(address));
    let mut work_area = RECT::default();
    let cloak = BOOL::from(true);
    let flags = SWP_NOACTIVATE;
    if !never_activate(hwnd) {
        if window_closed(hwnd) {
            return;
        }
        tracing::error!("{OFFSCREEN_ENV}: the window did not take WS_EX_NOACTIVATE");
    }
    // SAFETY: a plain query on this live window; 0 means it has no DPI.
    let dpi = match unsafe { GetDpiForWindow(hwnd) } {
        0 => 96,
        dpi => i32::try_from(dpi).unwrap_or(96),
    };
    let scaled = |logical: u16| i32::from(logical) * dpi / 96;
    let (width, height) = (scaled(size.0), scaled(size.1));
    // SAFETY: `hwnd` is this live window's handle and the pointers are to
    // locals that outlive the calls; none of the calls activates the window.
    unsafe {
        let area = (&raw mut work_area).cast();
        let none = SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0);
        if let Err(err) = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(area), none) {
            tracing::warn!("{OFFSCREEN_ENV}: reading the primary work area: {err}");
        }
        let (left, top) = (work_area.left, work_area.top);
        let bottom = Some(HWND_BOTTOM);
        if let Err(err) = SetWindowPos(hwnd, bottom, left, top, width, height, flags) {
            if window_closed(hwnd) {
                return;
            }
            tracing::error!(
                "{OFFSCREEN_ENV}: moving the window onto the work area, at the bottom: {err}"
            );
        }
        let cloak_ptr = (&raw const cloak).cast();
        let cloak_size = u32::try_from(size_of::<BOOL>()).unwrap_or(4);
        if let Err(err) = DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, cloak_ptr, cloak_size) {
            match cloak_failure_action(!window_closed(hwnd)) {
                CloakFailure::Exit => exit_uncloaked(&err),
                CloakFailure::Ignore => return,
            }
        }
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    tracing::info!("{OFFSCREEN_ENV}: the window is cloaked, never activated");
}

/// Gives the window `WS_EX_NOACTIVATE`, so neither a click nor the system
/// makes it the foreground window, and returns whether it took it. A window
/// left without it is still kept out of sight by the cloak.
#[cfg(windows)]
pub(crate) fn never_activate(hwnd: windows::Win32::Foundation::HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, SetWindowLongPtrW, WS_EX_NOACTIVATE,
    };
    let noactivate = isize::try_from(WS_EX_NOACTIVATE.0).unwrap_or(0);
    // SAFETY: reads and writes the extended style of this live window; the
    // window is still hidden, so the change activates nothing.
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | noactivate);
        GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & noactivate != 0
    }
}

/// Ends the process before a window that could not be cloaked is shown on
/// the owner's desktop.
#[cfg(windows)]
#[expect(
    clippy::exit,
    reason = "a cloak that fails must not show a window on the owner's desktop"
)]
fn exit_uncloaked(err: &windows::core::Error) -> ! {
    tracing::error!(
        "{OFFSCREEN_ENV}: cloaking the window failed, so it is never shown and the process \
         exits with code {CLOAK_FAILED_EXIT}: {err}"
    );
    std::process::exit(CLOAK_FAILED_EXIT)
}

/// What a failed cloak calls for.
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloakFailure {
    /// The window still exists and would be shown uncloaked: end the process.
    Exit,
    /// The window closed before it could be cloaked, so nothing was shown.
    Ignore,
}

/// What to do when cloaking a window fails, given whether it still exists.
#[cfg(windows)]
fn cloak_failure_action(window_alive: bool) -> CloakFailure {
    if window_alive {
        CloakFailure::Exit
    } else {
        CloakFailure::Ignore
    }
}

/// Whether the window is gone, logged at debug when it is: a window opened
/// and closed at once, or by a quitting app, makes every call on it fail.
#[cfg(windows)]
fn window_closed(hwnd: windows::Win32::Foundation::HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::IsWindow;
    // SAFETY: a plain query; a destroyed handle answers false.
    let closed = !unsafe { IsWindow(Some(hwnd)) }.as_bool();
    if closed {
        tracing::debug!("{OFFSCREEN_ENV}: the window closed before it could be cloaked");
    }
    closed
}

#[cfg(all(test, windows))]
mod tests {
    use super::{CloakFailure, cloak_failure_action};

    #[test]
    fn a_failed_cloak_on_a_live_window_exits() {
        assert_eq!(cloak_failure_action(true), CloakFailure::Exit);
    }

    #[test]
    fn a_failed_cloak_on_a_closed_window_is_ignored() {
        assert_eq!(cloak_failure_action(false), CloakFailure::Ignore);
    }
}

#[cfg(not(windows))]
pub fn show_cloaked(_window: &Window, _size: (u16, u16)) {
    tracing::warn!("{OFFSCREEN_ENV} is only honoured on Windows; the window stays hidden");
}
