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

/// Places `window` at the primary display's work-area origin at its full
/// size (gpui applies neither to a window it opens hidden), cloaks it and
/// shows it without activating it. The compositor still renders a cloaked
/// window, so its pixels can be read, but the user never sees it. A window
/// that cannot be cloaked stays hidden.
#[cfg(windows)]
pub fn show_cloaked(window: &Window) {
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
        .spawn(move || cloak_and_show(address));
    if let Err(err) = spawned {
        tracing::error!("{OFFSCREEN_ENV}: starting the thread that shows the window: {err}");
    }
}

/// The work of [`show_cloaked`] on the window whose handle is `address`.
#[cfg(windows)]
fn cloak_and_show(address: usize) {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::{
        SPI_GETWORKAREA, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOZORDER,
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetWindowPos, ShowWindow, SystemParametersInfoW,
    };
    use windows::core::BOOL;
    let hwnd = HWND(std::ptr::with_exposed_provenance_mut(address));
    let mut work_area = RECT::default();
    let cloak = BOOL::from(true);
    let flags = SWP_NOZORDER | SWP_NOACTIVATE;
    // SAFETY: a plain query on this live window; 0 means it has no DPI.
    let dpi = match unsafe { GetDpiForWindow(hwnd) } {
        0 => 96,
        dpi => i32::try_from(dpi).unwrap_or(96),
    };
    let scaled = |logical: u16| i32::from(logical) * dpi / 96;
    let (width, height) = (scaled(crate::WINDOW_SIZE.0), scaled(crate::WINDOW_SIZE.1));
    // SAFETY: `hwnd` is this live window's handle and the pointers are to
    // locals that outlive the calls; none of the calls activates the window.
    unsafe {
        let area = (&raw mut work_area).cast();
        let none = SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0);
        if let Err(err) = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(area), none) {
            tracing::warn!("{OFFSCREEN_ENV}: reading the primary work area: {err}");
        }
        let (left, top) = (work_area.left, work_area.top);
        if let Err(err) = SetWindowPos(hwnd, None, left, top, width, height, flags) {
            tracing::error!("{OFFSCREEN_ENV}: moving the window onto the work area: {err}");
        }
        let cloak_ptr = (&raw const cloak).cast();
        let cloak_size = u32::try_from(size_of::<BOOL>()).unwrap_or(4);
        if let Err(err) = DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, cloak_ptr, cloak_size) {
            tracing::error!("{OFFSCREEN_ENV}: cloaking the window, so it stays hidden: {err}");
            return;
        }
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    tracing::info!("{OFFSCREEN_ENV}: the window is cloaked, never activated");
}

#[cfg(not(windows))]
pub fn show_cloaked(_window: &Window) {
    tracing::warn!("{OFFSCREEN_ENV} is only honoured on Windows; the window stays hidden");
}
