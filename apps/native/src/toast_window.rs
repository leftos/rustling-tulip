//! The Win32 half of the app's own toasts: a window placed on a monitor's
//! work area, above every other window and never activated.
//!
//! Only the pixel conversion is testable here; the calls that touch a window
//! run in the app and are proven when a toast is drawn. The rules they
//! follow are in `docs/plans/custom-toasts.md`.
#![expect(dead_code, reason = "the toast view has no caller in the client yet")]

#[cfg(windows)]
use crate::toast_stack::WorkArea;
#[cfg(windows)]
use windows::Win32::Foundation::HWND;

/// A rectangle in physical pixels, the unit every Win32 call here takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PhysicalRect {
    /// The left edge.
    pub x: i32,
    /// The top edge.
    pub y: i32,
    /// The width.
    pub width: i32,
    /// The height.
    pub height: i32,
}

/// `left`, `top`, `width` and `height` in logical px as the physical px a
/// window on a monitor at `dpi` wants: each scaled by `dpi / 96` and
/// rounded to the nearest pixel.
pub(crate) fn to_physical(left: f32, top: f32, width: f32, height: f32, dpi: u32) -> PhysicalRect {
    PhysicalRect {
        x: scale(left, dpi),
        y: scale(top, dpi),
        width: scale(width, dpi),
        height: scale(height, dpi),
    }
}

/// `value` in logical px scaled to physical px at `dpi` and rounded.
fn scale(value: f32, dpi: u32) -> i32 {
    #[expect(clippy::cast_precision_loss, reason = "a DPI is a small whole number")]
    let dpi = dpi as f32;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a window coordinate fits in an i32"
    )]
    let physical = (value * dpi / 96.0).round() as i32;
    physical
}

/// The work area, in logical px, of the monitor holding `hwnd_near`, or of
/// the primary monitor when there is none: the part of the screen Windows'
/// own toasts keep clear of the taskbar and docks.
///
/// `None`, logged at warn, when a call that reads the monitor fails.
#[cfg(windows)]
pub(crate) fn work_area_logical(hwnd_near: Option<HWND>) -> Option<WorkArea> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint, MonitorFromWindow,
    };
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    let size = u32::try_from(size_of::<MONITORINFO>()).unwrap_or_default();
    let mut info = MONITORINFO {
        cbSize: size,
        ..Default::default()
    };
    // SAFETY: both calls only query the monitor and write their out
    // parameters; a handle Windows rejects answers the primary monitor.
    let monitor = unsafe {
        match hwnd_near {
            Some(hwnd) => MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY),
            None => MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY),
        }
    };
    // SAFETY: `info` is one local `MONITORINFO` whose `cbSize` names its own
    // length, and `monitor` is a handle from this process's own screen.
    if !unsafe { GetMonitorInfoW(monitor, &raw mut info) }.as_bool() {
        tracing::warn!("reading a monitor's work area: the call failed");
        return None;
    }
    let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
    // SAFETY: both out parameters are locals this call writes.
    if let Err(err) =
        unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &raw mut dpi_x, &raw mut dpi_y) }
    {
        tracing::warn!("reading a monitor's DPI: {err}");
        return None;
    }
    if dpi_x == 0 {
        tracing::warn!("reading a monitor's DPI: it reports none");
        return None;
    }
    let logical = |value: i32| logical_px(value, dpi_x);
    let work = info.rcWork;
    Some(WorkArea {
        left: logical(work.left),
        top: logical(work.top),
        right: logical(work.right),
        bottom: logical(work.bottom),
    })
}

/// `value` in physical px as the logical px a monitor at `dpi` uses.
#[cfg(windows)]
fn logical_px(value: i32, dpi: u32) -> f32 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "a screen coordinate is far below 2^24"
    )]
    let physical = value as f32;
    #[expect(clippy::cast_precision_loss, reason = "a DPI is a small whole number")]
    let scale = dpi as f32;
    physical * 96.0 / scale
}

/// Shows the window whose handle is `hwnd_address` on `rect` in physical px,
/// above every other window and never activated.
///
/// The calls run on a short thread of their own: they send the window
/// messages, and from inside the callback that opens a window gpui's state is
/// borrowed, so those messages would be lost (as in `offscreen.rs`).
#[cfg(windows)]
pub(crate) fn show_topmost(hwnd_address: usize, rect: PhysicalRect) {
    let spawned = std::thread::Builder::new()
        .name("toast-window".to_owned())
        .spawn(move || show_and_raise(hwnd_address, rect));
    if let Err(err) = spawned {
        tracing::warn!("starting the thread that shows the toast window: {err}");
    }
}

/// The work of [`show_topmost`], on the window whose handle is `address`.
#[cfg(windows)]
fn show_and_raise(address: usize, rect: PhysicalRect) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, HWND_TOPMOST, SW_SHOWNOACTIVATE, SWP_NOACTIVATE,
        SetWindowLongPtrW, SetWindowPos, ShowWindow, WS_EX_TOPMOST,
    };

    let hwnd = HWND(std::ptr::with_exposed_provenance_mut(address));
    if !crate::offscreen::never_activate(hwnd) {
        tracing::warn!("the toast window did not take WS_EX_NOACTIVATE");
        return;
    }
    let topmost = isize::try_from(WS_EX_TOPMOST.0).unwrap_or_default();
    // SAFETY: reads and writes the extended style of this live window, which
    // is still hidden, so neither call activates it.
    let raised = unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | topmost);
        GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & topmost
    } != 0;
    if !raised {
        tracing::warn!("the toast window did not take WS_EX_TOPMOST");
        return;
    }
    // SAFETY: `hwnd` is this live window's handle and the flags keep it from
    // being activated.
    if let Err(err) = unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            SWP_NOACTIVATE,
        )
    } {
        tracing::warn!("placing the toast window on the work area: {err}");
        return;
    }
    // SAFETY: showing this live window without activating it.
    let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
}

/// Moves the shown toast whose handle is `hwnd_address` to `rect` in physical
/// px, keeping it above every other window and never activating it. Called
/// from the caller's thread to restack the toasts already shown.
#[cfg(windows)]
pub(crate) fn move_topmost(hwnd_address: usize, rect: PhysicalRect) {
    use windows::Win32::UI::WindowsAndMessaging::{HWND_TOPMOST, SWP_NOACTIVATE, SetWindowPos};

    let hwnd = HWND(std::ptr::with_exposed_provenance_mut(hwnd_address));
    // SAFETY: `hwnd` is a live window's handle and the flags keep it from
    // being activated.
    if let Err(err) = unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            SWP_NOACTIVATE,
        )
    } {
        tracing::warn!("moving the toast window: {err}");
    }
}

/// The raw Win32 handle of `window`, or `None`, logged at warn, when it has
/// none.
#[cfg(windows)]
pub(crate) fn window_address(window: &gpui::Window) -> Option<usize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let raw = HasWindowHandle::window_handle(window).map(|handle| handle.as_raw());
    match raw {
        Ok(RawWindowHandle::Win32(handle)) => Some(handle.hwnd.get().cast_unsigned()),
        other => {
            tracing::warn!("the toast window has no Win32 handle ({other:?})");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PhysicalRect, to_physical};

    #[test]
    fn to_physical_scales_by_dpi() {
        let unscaled = PhysicalRect {
            x: 0,
            y: 0,
            width: 360,
            height: 120,
        };
        assert_eq!(to_physical(0.0, 0.0, 360.0, 120.0, 96), unscaled);
        let halfway = PhysicalRect {
            x: 15,
            y: 30,
            width: 540,
            height: 180,
        };
        assert_eq!(to_physical(10.0, 20.0, 360.0, 120.0, 144), halfway);
        let doubled = PhysicalRect {
            x: 20,
            y: 40,
            width: 720,
            height: 240,
        };
        assert_eq!(to_physical(10.0, 20.0, 360.0, 120.0, 192), doubled);
    }

    #[test]
    fn to_physical_rounds_half_pixels() {
        let rounded = PhysicalRect {
            x: 0,
            y: 1,
            width: 2,
            height: 3,
        };
        assert_eq!(to_physical(0.4, 0.5, 1.5, 2.5, 96), rounded);
        let negative = PhysicalRect {
            x: -1,
            y: -1,
            width: 0,
            height: 0,
        };
        assert_eq!(to_physical(-0.5, -1.4, 0.0, 0.0, 96), negative);
    }
}
