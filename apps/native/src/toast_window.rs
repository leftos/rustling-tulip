//! The Win32 half of the app's own toasts: a window placed on a monitor's
//! work area, above every other window and never activated.
//!
//! Only the pixel conversion and the serial queue are testable here; the
//! calls that touch a window run in the app and are proven when a toast is
//! drawn. The rules they follow are in `docs/native-client.md` "The app's
//! own toasts".

use crate::toast_stack::WorkArea;
#[cfg(windows)]
use windows::Win32::Foundation::HWND;

/// Where a toast window goes on screen. Windows that are not the platform's
/// have no handle, so every address here is an `Option`.
pub trait ToastPlacer: Send + Sync {
    /// The work area, in logical px, and the DPI of the monitor holding the
    /// window at `near`, else of the primary monitor.
    fn work_area(&self, near: Option<usize>) -> Option<(WorkArea, u32)>;

    /// Shows a newly opened toast window on `rect`, above every other window
    /// and never activated.
    fn show(&self, hwnd_address: Option<usize>, rect: PhysicalRect);

    /// Moves a shown toast window to `rect`.
    fn restack(&self, hwnd_address: Option<usize>, rect: PhysicalRect);

    /// The handle of `window`, the address the other calls take.
    fn address_of(&self, window: &gpui::Window) -> Option<usize>;
}

/// The placer the client runs with: real windows on the real monitors.
pub struct SystemToastPlacer;

#[cfg(windows)]
impl ToastPlacer for SystemToastPlacer {
    fn work_area(&self, near: Option<usize>) -> Option<(WorkArea, u32)> {
        work_area_logical(near.map(hwnd_at))
    }

    fn show(&self, hwnd_address: Option<usize>, rect: PhysicalRect) {
        if let Some(address) = hwnd_address {
            show_topmost(address, rect);
        } else {
            tracing::warn!("a toast window with no handle cannot be shown");
        }
    }

    fn restack(&self, hwnd_address: Option<usize>, rect: PhysicalRect) {
        if let Some(address) = hwnd_address {
            move_topmost(address, rect);
        } else {
            tracing::warn!("a toast window with no handle cannot be moved");
        }
    }

    fn address_of(&self, window: &gpui::Window) -> Option<usize> {
        window_address(window)
    }
}

#[cfg(not(windows))]
impl ToastPlacer for SystemToastPlacer {
    fn work_area(&self, _near: Option<usize>) -> Option<(WorkArea, u32)> {
        None
    }

    fn show(&self, _hwnd_address: Option<usize>, _rect: PhysicalRect) {}

    fn restack(&self, _hwnd_address: Option<usize>, _rect: PhysicalRect) {}

    fn address_of(&self, _window: &gpui::Window) -> Option<usize> {
        None
    }
}

/// A rectangle in physical pixels, the unit every Win32 call here takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalRect {
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
/// own toasts keep clear of the taskbar and docks. The DPI beside it is that
/// monitor's, the one [`to_physical`] wants for a toast placed there.
///
/// `None`, logged at warn, when a call that reads the monitor fails.
#[cfg(windows)]
pub(crate) fn work_area_logical(hwnd_near: Option<HWND>) -> Option<(WorkArea, u32)> {
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
    let area = WorkArea {
        left: logical(work.left),
        top: logical(work.top),
        right: logical(work.right),
        bottom: logical(work.bottom),
    };
    Some((area, dpi_x))
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

/// The window at `address`.
#[cfg(windows)]
fn hwnd_at(address: usize) -> HWND {
    HWND(std::ptr::with_exposed_provenance_mut(address))
}

/// One call on a toast window, as the worker takes them.
#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaceCall {
    /// Show the window at the address on the rectangle.
    Show(usize, PhysicalRect),
    /// Move the shown window at the address to the rectangle.
    Move(usize, PhysicalRect),
}

/// Applies every call `calls` delivers, in the order they were sent, until
/// each sender is gone.
#[cfg(windows)]
fn serve(calls: &std::sync::mpsc::Receiver<PlaceCall>, mut apply: impl FnMut(PlaceCall)) {
    while let Ok(call) = calls.recv() {
        apply(call);
    }
}

/// Queues `call` for the one `toast-window` thread, started on first use.
///
/// The calls send window messages, and with gpui's state borrowed on the UI
/// thread those messages are lost (as in `offscreen.rs`). One queue also
/// keeps a later move from being overwritten by an earlier show.
#[cfg(windows)]
fn send_to_worker(call: PlaceCall) {
    use std::sync::OnceLock;
    use std::sync::mpsc::{Sender, channel};

    static WORKER: OnceLock<Option<Sender<PlaceCall>>> = OnceLock::new();
    let worker = WORKER.get_or_init(|| {
        let (tx, rx) = channel();
        let spawned = std::thread::Builder::new()
            .name("toast-window".to_owned())
            .spawn(move || serve(&rx, apply_call));
        match spawned {
            Ok(_) => Some(tx),
            Err(err) => {
                tracing::warn!("starting the thread that places toast windows: {err}");
                None
            }
        }
    });
    let Some(worker) = worker else {
        tracing::warn!("no thread places toast windows: {call:?} is dropped");
        return;
    };
    if let Err(err) = worker.send(call) {
        tracing::warn!("the thread that places toast windows has gone: {err}");
    }
}

/// Runs `call` on the worker's thread.
#[cfg(windows)]
fn apply_call(call: PlaceCall) {
    match call {
        PlaceCall::Show(address, rect) => show_and_raise(address, rect),
        PlaceCall::Move(address, rect) => move_window(address, rect),
    }
}

/// Shows the window whose handle is `hwnd_address` on `rect` in physical px,
/// above every other window and never activated.
#[cfg(windows)]
pub(crate) fn show_topmost(hwnd_address: usize, rect: PhysicalRect) {
    send_to_worker(PlaceCall::Show(hwnd_address, rect));
}

/// How far a window's outer rectangle reaches past its client area on each
/// side, in physical px.
#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Insets {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

/// The outer rectangle that gives a window with `insets` the client area
/// `rect`.
#[cfg(windows)]
fn outer_rect(rect: PhysicalRect, insets: Insets) -> PhysicalRect {
    PhysicalRect {
        x: rect.x - insets.left,
        y: rect.y - insets.top,
        width: rect.width + insets.left + insets.right,
        height: rect.height + insets.top + insets.bottom,
    }
}

/// The window's insets as it stands now.
#[cfg(windows)]
fn window_insets(hwnd: HWND) -> windows::core::Result<Insets> {
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, GetWindowRect};

    let (mut outer, mut client) = (RECT::default(), RECT::default());
    let mut origin = POINT::default();
    // SAFETY: each call only writes its own local out parameter, and Windows
    // rejects the handle of a window that has closed.
    unsafe {
        GetWindowRect(hwnd, &raw mut outer)?;
        GetClientRect(hwnd, &raw mut client)?;
        ClientToScreen(hwnd, &raw mut origin).ok()?;
    }
    let left = origin.x - outer.left;
    let top = origin.y - outer.top;
    Ok(Insets {
        left,
        top,
        right: outer.right - outer.left - client.right - left,
        bottom: outer.bottom - outer.top - client.bottom - top,
    })
}

/// Puts the window's outer rectangle at `rect`, topmost and not activated.
#[cfg(windows)]
fn set_topmost_rect(hwnd: HWND, rect: PhysicalRect) -> windows::core::Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::{HWND_TOPMOST, SWP_NOACTIVATE, SetWindowPos};

    // SAFETY: the flags keep the window from being activated, and Windows
    // rejects the handle of one that has closed. `HWND_TOPMOST` is what sets
    // `WS_EX_TOPMOST`: Windows ignores that bit in a plain style write.
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            SWP_NOACTIVATE,
        )
    }
}

/// Gives the window the client area `rect`, in two placings. The first takes
/// it to `rect`'s monitor; that call waits for the window's thread, so any
/// DPI change has landed before the insets are read. gpui hides the frame
/// inside the outer rectangle, so the second placing reaches past `rect` by
/// those insets.
#[cfg(windows)]
fn fit_client_area(hwnd: HWND, rect: PhysicalRect) -> windows::core::Result<()> {
    set_topmost_rect(hwnd, rect)?;
    let insets = window_insets(hwnd)?;
    set_topmost_rect(hwnd, outer_rect(rect, insets))
}

/// The work of [`show_topmost`], on the window whose handle is `address`. A
/// window that has closed by now fails here, logged at warn.
#[cfg(windows)]
fn show_and_raise(address: usize, rect: PhysicalRect) {
    use windows::Win32::UI::WindowsAndMessaging::{SW_SHOWNOACTIVATE, ShowWindow};

    let hwnd = hwnd_at(address);
    if !crate::offscreen::never_activate(hwnd) {
        tracing::warn!("the toast window did not take WS_EX_NOACTIVATE");
        return;
    }
    if let Err(err) = fit_client_area(hwnd, rect) {
        tracing::warn!("placing the toast window on the work area: {err}");
        return;
    }
    // SAFETY: showing this live window without activating it.
    let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
}

/// Moves the shown toast whose handle is `hwnd_address` to `rect` in physical
/// px, keeping it above every other window and never activating it.
#[cfg(windows)]
pub(crate) fn move_topmost(hwnd_address: usize, rect: PhysicalRect) {
    send_to_worker(PlaceCall::Move(hwnd_address, rect));
}

/// The work of [`move_topmost`]. A window that has closed by now fails here,
/// logged at warn.
#[cfg(windows)]
fn move_window(address: usize, rect: PhysicalRect) {
    if let Err(err) = fit_client_area(hwnd_at(address), rect) {
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

    #[cfg(windows)]
    #[test]
    fn outer_rect_adds_every_inset() {
        use super::{Insets, outer_rect};

        let rect = PhysicalRect {
            x: 1544,
            y: 948,
            width: 360,
            height: 116,
        };
        let insets = Insets {
            left: 8,
            top: 1,
            right: 7,
            bottom: 9,
        };
        let outer = PhysicalRect {
            x: 1536,
            y: 947,
            width: 375,
            height: 126,
        };
        assert_eq!(outer_rect(rect, insets), outer);
    }

    #[cfg(windows)]
    #[test]
    fn outer_rect_with_no_insets_is_the_rect() {
        use super::{Insets, outer_rect};

        let rect = PhysicalRect {
            x: -1544,
            y: 948,
            width: 360,
            height: 116,
        };
        let insets = Insets {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        assert_eq!(outer_rect(rect, insets), rect);
    }

    #[cfg(windows)]
    #[test]
    fn the_worker_applies_calls_in_the_order_they_were_sent() {
        use super::{PlaceCall, serve};

        let rect = |x| PhysicalRect {
            x,
            y: 0,
            width: 360,
            height: 116,
        };
        let sent = [
            PlaceCall::Show(1, rect(10)),
            PlaceCall::Move(1, rect(20)),
            PlaceCall::Show(2, rect(30)),
            PlaceCall::Move(1, rect(40)),
        ];
        let (tx, rx) = std::sync::mpsc::channel();
        for call in sent {
            assert!(tx.send(call).is_ok(), "the queue is open");
        }
        drop(tx);
        let mut applied = Vec::new();
        serve(&rx, |call| applied.push(call));
        assert_eq!(applied, sent);
    }

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
