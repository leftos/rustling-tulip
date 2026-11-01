//! The main window's taskbar badge: the App title count over every tab,
//! drawn on Windows as a disc in the palette's danger colour with white
//! digits on the window's taskbar button. Pop-out windows carry none, and
//! other platforms draw none yet.

use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::{App, Context, Window};
use protocol::DaemonMessage;

use crate::settings_view::TITLE_DEBOUNCE;
use crate::tabs::{TabBadge, window_session_counts};
use crate::ui_theme::theme;
use crate::{NetEvent, RootView};

/// What the badge shows: its text, what a screen reader reads for it, and
/// the disc's colour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadgeOverlay {
    /// The digits drawn on the disc: `1` to `9`, or `9+`.
    pub text: String,
    /// The overlay's accessible description, as `1 needs you` or `3 need
    /// you`.
    pub description: String,
    /// The disc's colour, `0xRRGGBB`.
    pub colour: u32,
}

/// The badge's text for a count of `n`: none at 0, the digits up to 9, and
/// `9+` above.
#[must_use]
pub fn overlay_text(n: usize) -> Option<String> {
    match n {
        0 => None,
        1..=9 => Some(n.to_string()),
        _ => Some("9+".to_owned()),
    }
}

/// Puts the badge on a window's taskbar button.
pub trait TaskbarBadge {
    /// Shows `overlay` on `window`'s taskbar button, or clears it with
    /// `None`. Called on the UI thread, only when the badge changes; a
    /// failure is logged and answered `false`, so the next change tries
    /// again.
    fn show(&self, window: &Window, overlay: Option<&BadgeOverlay>) -> bool;

    /// A stream that yields each time the OS (re)creates `window`'s taskbar
    /// button: at launch and after Explorer restarts, which drops the
    /// overlay. `None` when nothing is watched.
    fn button_created(&self, _window: &Window) -> Option<UnboundedReceiver<()>> {
        None
    }
}

/// A badge that draws nothing: the cloaked smoke window's and the shots'.
pub struct NoTaskbarBadge;

impl TaskbarBadge for NoTaskbarBadge {
    fn show(&self, _window: &Window, _overlay: Option<&BadgeOverlay>) -> bool {
        true
    }
}

/// The badge the client runs with: the taskbar button's overlay icon on
/// Windows, nothing elsewhere.
#[derive(Default)]
pub struct SystemTaskbarBadge {
    /// The taskbar list, created at the first change and kept; dropped
    /// after a failure so the next change creates it afresh.
    #[cfg(windows)]
    taskbar: std::cell::RefCell<Option<windows::Win32::UI::Shell::ITaskbarList3>>,
}

impl TaskbarBadge for SystemTaskbarBadge {
    #[cfg(windows)]
    fn show(&self, window: &Window, overlay: Option<&BadgeOverlay>) -> bool {
        match win::show(&self.taskbar, window, overlay) {
            Ok(()) => true,
            Err(err) => {
                self.taskbar.borrow_mut().take();
                let text = overlay.map_or("none", |overlay| overlay.text.as_str());
                tracing::warn!("the taskbar badge ({text}) was not shown: {err:#}");
                false
            }
        }
    }

    #[cfg(windows)]
    fn button_created(&self, window: &Window) -> Option<UnboundedReceiver<()>> {
        win::watch_button_created(window)
            .inspect_err(|err| {
                tracing::warn!("the taskbar badge will not follow a new taskbar button: {err:#}");
            })
            .ok()
    }

    #[cfg(not(windows))]
    fn show(&self, _window: &Window, _overlay: Option<&BadgeOverlay>) -> bool {
        true
    }
}

/// Whether `event` can move the badge's count: the connection, the session
/// list and the tabs.
pub(crate) fn moves_badge(event: &NetEvent) -> bool {
    match event {
        NetEvent::State(_) => true,
        NetEvent::Message(msg) => matches!(
            **msg,
            DaemonMessage::Sessions { .. }
                | DaemonMessage::SessionUpdated { .. }
                | DaemonMessage::SessionRemoved { .. }
                | DaemonMessage::Tabs { .. }
                | DaemonMessage::TabUpdated { .. }
                | DaemonMessage::TabRemoved { .. }
        ),
        _ => false,
    }
}

impl RootView {
    /// The badge the main window wants now: the App title count over every
    /// tab, each session once, none while the count is off or counts
    /// nothing.
    fn wanted_badge(&self, cx: &App) -> Option<BadgeOverlay> {
        let title = &self.sidebar.ui_state().title;
        if !title.show_count {
            return None;
        }
        let counts = window_session_counts(self.tabs.tabs(), &self.sidebar.sessions_by_host());
        let (n, what) = match counts.badge(title.count)? {
            TabBadge::NeedsYou(1) => (1, "needs you"),
            TabBadge::NeedsYou(n) => (n, "need you"),
            TabBadge::Busy { busy, .. } => (busy, "busy"),
            TabBadge::Idle { idle, .. } => (idle, "idle"),
        };
        Some(BadgeOverlay {
            text: overlay_text(n)?,
            description: format!("{n} {what}"),
            colour: theme(cx).danger,
        })
    }

    /// Whether `wanted` is what the taskbar button already shows.
    fn badge_settled(&self, wanted: Option<&BadgeOverlay>) -> bool {
        !self.badge_unsure && wanted == self.badge_shown.as_ref()
    }

    /// Puts the wanted badge on the main window's taskbar button
    /// [`TITLE_DEBOUNCE`] after it last changed, so a session flipping
    /// between states redraws nothing. Called from what moves the count
    /// (the connection, the sessions, the tabs, the App title settings),
    /// never from a frame: a minimized window draws none.
    pub(crate) fn schedule_taskbar_badge(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wanted = self.wanted_badge(cx);
        if self.badge_settled(wanted.as_ref()) {
            self.badge_timer = None;
            self.badge_pending = None;
            return;
        }
        if self.badge_timer.is_some() && self.badge_pending == wanted {
            return;
        }
        self.badge_pending = wanted;
        self.badge_timer = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(TITLE_DEBOUNCE).await;
            // Fails only when the view is gone, and its window with it.
            this.update_in(cx, |this, window, cx| this.apply_taskbar_badge(window, cx))
                .ok();
        }));
    }

    /// Shows the badge wanted now, recording it only once the taskbar took
    /// it.
    fn apply_taskbar_badge(&mut self, window: &Window, cx: &App) {
        self.badge_timer = None;
        self.badge_pending = None;
        let wanted = self.wanted_badge(cx);
        if self.badge_settled(wanted.as_ref()) {
            return;
        }
        if self.taskbar_badge.show(window, wanted.as_ref()) {
            self.badge_shown = wanted;
            self.badge_unsure = false;
        } else {
            self.badge_unsure = true;
        }
    }

    /// Puts the badge back each time the OS (re)creates the main window's
    /// taskbar button.
    pub(crate) fn watch_taskbar_button(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut created) = self.taskbar_badge.button_created(window) else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            while created.next().await.is_some() {
                let rescheduled = this.update_in(cx, |this, window, cx| {
                    this.badge_unsure = true;
                    this.schedule_taskbar_badge(window, cx);
                });
                if rescheduled.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Takes the badge off the main window's taskbar button as it closes.
    pub(crate) fn clear_taskbar_badge(&mut self, window: &Window) {
        self.badge_timer = None;
        self.badge_pending = None;
        if (self.badge_shown.is_some() || self.badge_unsure)
            && self.taskbar_badge.show(window, None)
        {
            self.badge_shown = None;
            self.badge_unsure = false;
        }
    }
}

/// Paints the badge into `pixels`, `side` by `side` 32-bit BGRA pixels
/// holding white-on-black text: a disc of `colour` with its edge smoothed,
/// the text in white over it, transparent outside it. Alpha is straight, as
/// an icon's is.
#[cfg(windows)]
fn paint_disc(pixels: &mut [u32], side: usize, colour: u32) {
    #[expect(
        clippy::cast_precision_loss,
        reason = "an icon side is a small number of pixels"
    )]
    let (radius, to_f32) = (side as f32 / 2.0, |n: usize| n as f32);
    let [_, red, green, blue] = colour.to_be_bytes().map(f32::from);
    for (index, pixel) in pixels.iter_mut().enumerate() {
        let dx = to_f32(index % side) + 0.5 - radius;
        let dy = to_f32(index / side) + 0.5 - radius;
        let cover = (radius - dx.hypot(dy) + 0.5).clamp(0.0, 1.0);
        // GDI drew the text in grey levels: its red byte is its coverage.
        let text = f32::from(pixel.to_be_bytes()[1]) / 255.0;
        let blend = |base: f32| to_byte(base + (255.0 - base) * text);
        *pixel =
            (to_byte(cover * 255.0) << 24) | (blend(red) << 16) | (blend(green) << 8) | blend(blue);
    }
}

/// `value`, 0 to 255, as a whole byte in a `u32`.
#[cfg(windows)]
fn to_byte(value: f32) -> u32 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to 0..=255 first"
    )]
    let byte = value.round().clamp(0.0, 255.0) as u32;
    byte
}

#[cfg(windows)]
mod win {
    use std::cell::RefCell;

    use anyhow::{Context as _, anyhow, bail};
    use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
    use gpui::Window;
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS,
        CreateBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW, DEFAULT_CHARSET,
        DEFAULT_PITCH, DIB_RGB_COLORS, DT_CENTER, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteDC,
        DeleteObject, DrawTextW, FF_SWISS, FW_BOLD, GdiFlush, HDC, HGDIOBJ, OUT_DEFAULT_PRECIS,
        SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
    };
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
    use windows::Win32::UI::Shell::{
        DefSubclassProc, ITaskbarList3, RemoveWindowSubclass, SetWindowSubclass, TaskbarList,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateIconIndirect, DestroyIcon, GetSystemMetrics, HICON, ICONINFO, RegisterWindowMessageW,
        SM_CXSMICON, WM_NCDESTROY,
    };
    use windows::core::{HSTRING, PCWSTR, w};

    use super::{BadgeOverlay, paint_disc};

    /// The taskbar list kept in `slot`, created and kept there the first
    /// time.
    fn taskbar_list(slot: &RefCell<Option<ITaskbarList3>>) -> anyhow::Result<ITaskbarList3> {
        if let Some(taskbar) = slot.borrow().as_ref() {
            return Ok(taskbar.clone());
        }
        // SAFETY: COM is initialised on this thread: GPUI's Windows platform
        // calls `OleInitialize(None)` on the UI thread when it starts.
        let taskbar: ITaskbarList3 =
            unsafe { CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER) }
                .context("creating the taskbar list")?;
        // SAFETY: `taskbar` is a live object this thread created.
        unsafe { taskbar.HrInit() }.context("initialising the taskbar list")?;
        *slot.borrow_mut() = Some(taskbar.clone());
        Ok(taskbar)
    }

    /// Sets or clears `window`'s taskbar overlay through the list in `slot`.
    pub(super) fn show(
        slot: &RefCell<Option<ITaskbarList3>>,
        window: &Window,
        overlay: Option<&BadgeOverlay>,
    ) -> anyhow::Result<()> {
        let hwnd = hwnd_of(window)?;
        let taskbar = taskbar_list(slot)?;
        let Some(overlay) = overlay else {
            // SAFETY: a null icon clears the overlay of this live window.
            return unsafe { taskbar.SetOverlayIcon(hwnd, HICON::default(), PCWSTR::null()) }
                .context("clearing the overlay");
        };
        let icon = draw_icon(overlay)?;
        let description = HSTRING::from(overlay.description.as_str());
        // SAFETY: `icon` is alive until after the call; the taskbar keeps a
        // copy of its own, so `icon` is destroyed when it drops.
        unsafe { taskbar.SetOverlayIcon(hwnd, icon.0, &description) }.context("setting the overlay")
    }

    /// What the subclass procedure needs: the registered message's number
    /// and where to report it.
    struct ButtonWatch {
        message: u32,
        created: UnboundedSender<()>,
    }

    /// This module's subclass id on the main window.
    const BUTTON_WATCH_ID: usize = 0x5254_4247;

    /// Subclasses `window` narrowly to hear `TaskbarButtonCreated`, which
    /// gpui's window procedure passes by with no hook; every message goes
    /// on to the window's own procedure unchanged.
    pub(super) fn watch_button_created(window: &Window) -> anyhow::Result<UnboundedReceiver<()>> {
        let hwnd = hwnd_of(window)?;
        // SAFETY: a plain registration of a string the shell also registers.
        let message = unsafe { RegisterWindowMessageW(w!("TaskbarButtonCreated")) };
        if message == 0 {
            bail!("registering TaskbarButtonCreated failed");
        }
        let (created, receiver) = unbounded();
        let watch = Box::into_raw(Box::new(ButtonWatch { message, created }));
        // SAFETY: `hwnd` is this thread's live window; `watch` stays alive
        // until the procedure frees it at `WM_NCDESTROY`.
        let set = unsafe {
            SetWindowSubclass(
                hwnd,
                Some(button_watch_proc),
                BUTTON_WATCH_ID,
                watch.expose_provenance(),
            )
        };
        if !set.as_bool() {
            // SAFETY: the subclass was not set, so nothing else holds it.
            drop(unsafe { Box::from_raw(watch) });
            bail!("subclassing the main window failed");
        }
        Ok(receiver)
    }

    /// Reports each `TaskbarButtonCreated`, frees the watch as the window
    /// goes, and passes every message on.
    unsafe extern "system" fn button_watch_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        data: usize,
    ) -> LRESULT {
        let watch = std::ptr::with_exposed_provenance_mut::<ButtonWatch>(data);
        // SAFETY: `data` is the watch `watch_button_created` leaked, alive
        // until `WM_NCDESTROY` below.
        if msg == unsafe { (*watch).message }
            && unsafe { (*watch).created.unbounded_send(()) }.is_err()
        {
            tracing::debug!("a new taskbar button, but the root view is gone");
        }
        if msg == WM_NCDESTROY {
            // SAFETY: the window is going; the subclass and its watch go
            // with it, once.
            unsafe {
                if !RemoveWindowSubclass(hwnd, Some(button_watch_proc), BUTTON_WATCH_ID).as_bool() {
                    tracing::debug!("removing the taskbar button watch failed");
                }
                drop(Box::from_raw(watch));
            }
        }
        // SAFETY: passes the message on to the window's own procedure.
        unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
    }

    fn hwnd_of(window: &Window) -> anyhow::Result<HWND> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        // `Window::window_handle` is gpui's own handle; the raw one is the trait's.
        match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
            Ok(RawWindowHandle::Win32(handle)) => {
                let address = handle.hwnd.get().cast_unsigned();
                Ok(HWND(std::ptr::with_exposed_provenance_mut(address)))
            }
            other => Err(anyhow!("the window has no Win32 handle ({other:?})")),
        }
    }

    /// An icon this module created, destroyed when dropped.
    struct Icon(HICON);

    impl Drop for Icon {
        fn drop(&mut self) {
            // SAFETY: the icon was created by `CreateIconIndirect` and is
            // destroyed once.
            if let Err(err) = unsafe { DestroyIcon(self.0) } {
                tracing::debug!("destroying the badge icon: {err}");
            }
        }
    }

    /// A GDI bitmap or font this module created, deleted when dropped.
    struct GdiObject(HGDIOBJ);

    impl Drop for GdiObject {
        fn drop(&mut self) {
            // SAFETY: the object is selected into no context by now.
            if !unsafe { DeleteObject(self.0) }.as_bool() {
                tracing::debug!("deleting a badge GDI object failed");
            }
        }
    }

    /// A memory drawing context, deleted when dropped.
    struct MemoryDc(HDC);

    impl Drop for MemoryDc {
        fn drop(&mut self) {
            // SAFETY: the context was created by `CreateCompatibleDC`.
            if !unsafe { DeleteDC(self.0) }.as_bool() {
                tracing::debug!("deleting the badge drawing context failed");
            }
        }
    }

    /// The badge as a small icon: `overlay`'s text in white bold over a
    /// disc of its colour.
    fn draw_icon(overlay: &BadgeOverlay) -> anyhow::Result<Icon> {
        // SAFETY: a plain query.
        let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
        let side = usize::try_from(size).context("the small icon size")?;
        // SAFETY: a context compatible with the screen, deleted on drop.
        let dc = MemoryDc(unsafe { CreateCompatibleDC(None) });
        if dc.0.is_invalid() {
            bail!("creating a drawing context failed");
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).context("the header size")?,
                biWidth: size,
                // Negative: rows run top-down.
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        // SAFETY: `info` describes a 32-bit top-down bitmap; `bits` receives
        // its pixels, owned by the bitmap.
        let colour_bitmap = unsafe {
            CreateDIBSection(
                Some(dc.0),
                &raw const info,
                DIB_RGB_COLORS,
                &raw mut bits,
                None,
                0,
            )
        }
        .context("creating the badge bitmap")?;
        let colour = GdiObject(colour_bitmap.into());
        if bits.is_null() {
            bail!("the badge bitmap has no pixels");
        }
        let pixels = bits.cast::<u32>();
        // SAFETY: the bitmap holds `side * side` 32-bit pixels and lives
        // as long as `colour`; nothing else touches them now.
        unsafe { std::slice::from_raw_parts_mut(pixels, side * side) }.fill(0);
        draw_text(&dc, &colour, &overlay.text, size)?;
        // SAFETY: as above; GDI finished drawing at `GdiFlush`.
        paint_disc(
            unsafe { std::slice::from_raw_parts_mut(pixels, side * side) },
            side,
            overlay.colour,
        );
        // SAFETY: a monochrome mask of the icon's size, all zeros: the colour
        // bitmap's alpha is what shows.
        let mask_bitmap = unsafe { CreateBitmap(size, size, 1, 1, None) };
        if mask_bitmap.is_invalid() {
            bail!("creating the badge mask failed");
        }
        let _mask = GdiObject(mask_bitmap.into());
        let icon_info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask_bitmap,
            hbmColor: colour_bitmap,
        };
        // SAFETY: both bitmaps are live and selected into no context; the
        // icon copies them, so they are deleted when they drop.
        let icon = unsafe { CreateIconIndirect(&raw const icon_info) }
            .context("creating the badge icon")?;
        Ok(Icon(icon))
    }

    /// Draws `text` in white bold, centred, onto `bitmap` through `dc`,
    /// smoothed in grey levels so its coverage reads back from the red
    /// channel.
    fn draw_text(dc: &MemoryDc, bitmap: &GdiObject, text: &str, size: i32) -> anyhow::Result<()> {
        let height = if text.chars().count() > 1 {
            size * 9 / 16
        } else {
            size * 11 / 16
        };
        // SAFETY: a font this function deletes on drop.
        let font = unsafe {
            CreateFontW(
                -height,
                0,
                0,
                0,
                FW_BOLD.0.cast_signed(),
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                u32::from(DEFAULT_PITCH.0 | FF_SWISS.0),
                w!("Segoe UI"),
            )
        };
        if font.is_invalid() {
            bail!("creating the badge font failed");
        }
        let font = GdiObject(font.into());
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: size,
            bottom: size,
        };
        // SAFETY: `dc`, `bitmap` and `font` are live; the previous objects
        // are selected back before either is deleted.
        let drawn = unsafe {
            let old_bitmap = SelectObject(dc.0, bitmap.0);
            let old_font = SelectObject(dc.0, font.0);
            SetTextColor(dc.0, COLORREF(0x00FF_FFFF));
            SetBkMode(dc.0, TRANSPARENT);
            let height = DrawTextW(
                dc.0,
                &mut wide,
                &raw mut rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
            let flushed = GdiFlush().as_bool();
            SelectObject(dc.0, old_font);
            SelectObject(dc.0, old_bitmap);
            height != 0 && flushed
        };
        if !drawn {
            bail!("drawing the badge text failed");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::overlay_text;

    #[test]
    fn the_badge_text_is_none_at_zero_digits_to_nine_and_nine_plus_above() {
        assert_eq!(overlay_text(0), None);
        for n in 1..=9 {
            assert_eq!(overlay_text(n), Some(n.to_string()));
        }
        assert_eq!(overlay_text(10), Some("9+".to_owned()));
        assert_eq!(overlay_text(250), Some("9+".to_owned()));
    }
}
