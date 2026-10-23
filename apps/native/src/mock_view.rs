//! A session's mock page: the HTML file its Claude showed with `show_mock`,
//! drawn live by a `WebView2` inside the session's pane.
//!
//! GPUI's `DirectComposition` target is topmost over its window's children,
//! so the webview lives in a borderless popup the GPUI window owns, placed
//! in screen coordinates over the panel's body. The popup paints above
//! every GPUI layer, so the view holding the panel hides it whenever
//! something is drawn over the panes.
//!
//! Building the webview runs a nested Win32 message pump, and creating,
//! activating or destroying windows sends messages to the GPUI window.
//! Each of those runs in a foreground task holding no GPUI borrow, never
//! inside a frame or an event handler, where the re-entered window
//! procedure would find GPUI's state borrowed.

use std::path::{Path, PathBuf};

use gpui::{
    App, AppContext, Bounds, Context, DispatchPhase, Element, ElementId, Entity, EventEmitter,
    GlobalElementId, HitboxBehavior, InspectorElementId, InteractiveElement, IntoElement, LayoutId,
    MouseDownEvent, ParentElement, Pixels, Render, SharedString, StatefulInteractiveElement, Style,
    Styled, Window, div, px,
};
use protocol::{DaemonMessage, SessionSnapshot};

use crate::RootView;
use crate::palette::{HOVER, LINE, RAISED, TEXT, TEXT_2};

/// What a mock panel asks of the view holding it.
pub(crate) enum MockPanelEvent {
    /// The user closed the panel.
    Close,
}

/// A session's mock page and the webview showing it.
pub(crate) struct MockPanel {
    session_id: String,
    title: SharedString,
    path: PathBuf,
    /// `WebView2`'s user-data folder; `None` leaves wry's default.
    data_dir: Option<PathBuf>,
    #[cfg(windows)]
    host: win32::Host,
    hidden: bool,
}

impl EventEmitter<MockPanelEvent> for MockPanel {}

impl MockPanel {
    pub(crate) fn new(
        session_id: String,
        title: String,
        path: PathBuf,
        data_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            session_id,
            title: title.into(),
            path,
            data_dir,
            #[cfg(windows)]
            host: win32::Host::default(),
            hidden: false,
        }
    }

    /// Shows `path` under `title`, loading it at once when the webview
    /// exists.
    pub(crate) fn show(&mut self, title: String, path: PathBuf) {
        self.title = title.into();
        self.path = path;
        #[cfg(windows)]
        self.load_current();
    }

    pub(crate) fn is_hidden(&self) -> bool {
        self.hidden
    }

    /// Hides the webview while something is drawn over it, handing the
    /// keyboard back to the window, or shows it again.
    pub(crate) fn set_hidden(&mut self, hidden: bool, cx: &mut Context<Self>) {
        self.hidden = hidden;
        #[cfg(windows)]
        self.sync_visibility(cx);
        #[cfg(not(windows))]
        let _ = cx;
    }

    /// Hides and destroys the webview and its popup after the current
    /// frame; the panel is being closed.
    pub(crate) fn release(&mut self, cx: &mut Context<Self>) {
        #[cfg(windows)]
        self.host.release(cx);
        #[cfg(not(windows))]
        let _ = cx;
    }

    /// Gives the keyboard back to the GPUI window after the current event.
    fn focus_parent_later(&self, cx: &mut Context<Self>) {
        #[cfg(windows)]
        self.host.focus_owner_later(cx);
        #[cfg(not(windows))]
        let _ = cx;
    }
}

#[cfg(windows)]
impl MockPanel {
    fn load_current(&mut self) {
        let Some(webview) = &self.host.webview else {
            return;
        };
        let url = file_url(&self.path);
        if let Err(err) = webview.load_url(&url) {
            tracing::warn!(
                "mock panel for session {}: loading {url} failed: {err}",
                self.session_id
            );
        }
        // A failed load is not retried every frame; the next `show` tries again.
        self.host.loaded_url = Some(url);
    }

    /// Records the body's bounds each frame; on the first frame with a real
    /// window, follows the window's moves and schedules the build.
    fn place(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        self.host.bounds = bounds;
        let first = !self.host.build_tried;
        if first {
            self.host.build_tried = true;
            self.host.owner = win32::window_handle(window);
            if self.host.owner.is_some() {
                self.host.moves = Some(cx.observe_window_bounds(window, |panel, window, cx| {
                    panel.reposition(window, cx);
                }));
            }
        }
        self.reposition(window, cx);
        if first && self.host.owner.is_some() {
            Self::schedule_build(cx);
        }
        if self.host.webview.is_some()
            && self.host.loaded_url.as_deref() != Some(&file_url(&self.path))
        {
            self.load_current();
        }
        self.sync_visibility(cx);
    }

    /// Works out where the body is on screen and moves the popup there
    /// after the current frame when that changed.
    fn reposition(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(owner) = self.host.owner else {
            return;
        };
        self.host.target = win32::screen_rect(owner, self.host.bounds, window.scale_factor());
        self.host.move_if_needed(cx);
    }

    fn sync_visibility(&mut self, cx: &mut Context<Self>) {
        self.host.set_shown(!self.hidden, cx);
    }

    fn schedule_build(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let job = this.update(cx, |panel, _| panel.build_job());
            let Ok(Some(job)) = job else {
                tracing::debug!(
                    "mock panel: closed, or not on screen, before its webview was built"
                );
                return;
            };
            let built = job.run();
            if let Err(err) = this.update(cx, |panel, cx| panel.finish_build(built, cx)) {
                tracing::debug!("mock panel: closed while its webview was built: {err}");
            }
        })
        .detach();
    }

    fn build_job(&mut self) -> Option<win32::BuildJob> {
        let data_dir = self.data_dir.clone();
        Some(win32::BuildJob {
            session_id: self.session_id.clone(),
            url: file_url(&self.path),
            rect: self.host.target?,
            owner: self.host.owner?,
            context: self
                .host
                .web_context
                .take()
                .unwrap_or_else(|| wry::WebContext::new(data_dir)),
        })
    }

    fn finish_build(&mut self, built: win32::Built, cx: &mut Context<Self>) {
        self.host.web_context = Some(built.context);
        let Some((window, webview, rect)) = built.host else {
            return;
        };
        self.host.window = Some(window);
        self.host.webview = Some(std::rc::Rc::new(webview));
        self.host.placed = Some(rect);
        self.host.loaded_url = Some(built.url);
        // The popup is created hidden; showing it, and any move asked for
        // during the build, go out now.
        self.host.move_if_needed(cx);
        self.sync_visibility(cx);
        cx.notify();
    }
}

#[cfg(windows)]
mod win32 {
    use std::num::NonZeroIsize;
    use std::rc::Rc;

    use gpui::{Bounds, Context, Pixels, Subscription, Window};
    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetForegroundWindow, SW_HIDE, SW_SHOWNOACTIVATE,
        SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER, SetForegroundWindow,
        SetWindowPos, ShowWindow, WS_CLIPCHILDREN, WS_EX_NOPARENTNOTIFY, WS_EX_TOOLWINDOW,
        WS_POPUP,
    };
    use windows::core::w;

    use super::MockPanel;

    /// A rectangle in screen device pixels.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(super) struct ScreenRect {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    }

    impl ScreenRect {
        /// The webview's bounds inside the popup: all of it.
        fn fill(self) -> wry::Rect {
            use wry::dpi::{PhysicalPosition, PhysicalSize, Position, Size};
            wry::Rect {
                position: Position::Physical(PhysicalPosition::new(0, 0)),
                size: Size::Physical(PhysicalSize::new(
                    self.width.unsigned_abs(),
                    self.height.unsigned_abs(),
                )),
            }
        }
    }

    /// The popup holding a panel's webview, and what it needs to follow
    /// the panel's body.
    #[derive(Default)]
    pub(super) struct Host {
        /// The GPUI window, which owns the popup.
        pub(super) owner: Option<NonZeroIsize>,
        /// The popup, once built.
        pub(super) window: Option<NonZeroIsize>,
        /// Shared with the tasks that call it outside a frame.
        pub(super) webview: Option<Rc<wry::WebView>>,
        /// Lent to the build task while it runs.
        pub(super) web_context: Option<wry::WebContext>,
        /// The URL the webview last tried to load; a frame whose page
        /// differs (the page changed during the build) loads it.
        pub(super) loaded_url: Option<String>,
        /// The build was scheduled, so a failed one is not retried every
        /// frame.
        pub(super) build_tried: bool,
        /// The visibility last sent to the popup.
        shown: bool,
        /// The body's bounds at the last frame, in window pixels.
        pub(super) bounds: Bounds<Pixels>,
        /// Where the popup belongs on screen.
        pub(super) target: Option<ScreenRect>,
        /// Where the popup was last sent.
        pub(super) placed: Option<ScreenRect>,
        /// Re-places the popup when the GPUI window moves.
        pub(super) moves: Option<Subscription>,
    }

    impl Host {
        pub(super) fn move_if_needed(&mut self, cx: &mut Context<MockPanel>) {
            let (Some(window), Some(webview), Some(target)) =
                (self.window, self.webview.clone(), self.target)
            else {
                return;
            };
            if self.placed == Some(target) {
                return;
            }
            self.placed = Some(target);
            cx.spawn(async move |_, _| move_popup(window, &webview, target))
                .detach();
        }

        pub(super) fn set_shown(&mut self, visible: bool, cx: &mut Context<MockPanel>) {
            let (Some(window), Some(owner)) = (self.window, self.owner) else {
                return;
            };
            if self.shown == visible {
                return;
            }
            self.shown = visible;
            cx.spawn(async move |_, _| {
                if visible {
                    // SAFETY: `window` is this panel's live popup.
                    let _was_visible = unsafe { ShowWindow(hwnd(window), SW_SHOWNOACTIVATE) };
                } else {
                    hide_popup(window, owner);
                }
            })
            .detach();
        }

        pub(super) fn focus_owner_later(&self, cx: &mut Context<MockPanel>) {
            let (Some(window), Some(owner)) = (self.window, self.owner) else {
                return;
            };
            cx.spawn(async move |_, _| give_back_foreground(window, owner))
                .detach();
        }

        pub(super) fn release(&mut self, cx: &mut Context<MockPanel>) {
            let webview = self.webview.take();
            let window = self.window.take();
            let context = self.web_context.take();
            let owner = self.owner;
            self.moves = None;
            cx.spawn(async move |_, _| {
                if let (Some(window), Some(owner)) = (window, owner) {
                    hide_popup(window, owner);
                }
                // The webview's teardown messages its parent, the popup, so
                // it goes before the popup does.
                drop(webview);
                drop(context);
                if let Some(window) = window {
                    destroy_popup(window);
                }
            })
            .detach();
        }
    }

    /// What the build task needs, taken out of the panel so the build
    /// holds no GPUI borrow.
    pub(super) struct BuildJob {
        pub(super) session_id: String,
        pub(super) url: String,
        pub(super) rect: ScreenRect,
        pub(super) owner: NonZeroIsize,
        pub(super) context: wry::WebContext,
    }

    /// What the build task hands back.
    pub(super) struct Built {
        pub(super) context: wry::WebContext,
        pub(super) url: String,
        pub(super) host: Option<(NonZeroIsize, wry::WebView, ScreenRect)>,
    }

    impl BuildJob {
        /// Creates the hidden popup over `rect` and builds the webview
        /// filling it; `build_as_child` pumps messages until `WebView2`'s
        /// environment and controller exist.
        pub(super) fn run(mut self) -> Built {
            let host = match create_popup(self.owner, self.rect) {
                Ok(window) => self.build_in(window),
                Err(err) => {
                    tracing::warn!(
                        "mock panel for session {}: the popup could not be created: {err}",
                        self.session_id
                    );
                    None
                }
            };
            Built {
                context: self.context,
                url: self.url,
                host,
            }
        }

        fn build_in(
            &mut self,
            window: NonZeroIsize,
        ) -> Option<(NonZeroIsize, wry::WebView, ScreenRect)> {
            let built = wry::WebViewBuilder::new_with_web_context(&mut self.context)
                .with_url(self.url.as_str())
                .with_bounds(self.rect.fill())
                .build_as_child(&RawWindow(window));
            match built {
                Ok(webview) => {
                    tracing::info!(
                        "mock panel for session {}: webview built for {}",
                        self.session_id,
                        self.url
                    );
                    Some((window, webview, self.rect))
                }
                Err(err) => {
                    tracing::warn!(
                        "mock panel for session {}: the webview could not be built: {err}",
                        self.session_id
                    );
                    destroy_popup(window);
                    None
                }
            }
        }
    }

    /// The handle of `window` when it is a real Win32 window. The test
    /// platform's window has no GPU and panics when asked for its handle,
    /// so a window without GPU specs is never asked.
    pub(super) fn window_handle(window: &Window) -> Option<NonZeroIsize> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if window.gpu_specs().is_none() {
            tracing::debug!("mock panel: the window has no GPU-backed platform window");
            return None;
        }
        // `Window::window_handle` is gpui's own handle; the raw one is the trait's.
        match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
            Ok(RawWindowHandle::Win32(handle)) => Some(handle.hwnd),
            other => {
                tracing::warn!("mock panel: the window has no Win32 handle ({other:?})");
                None
            }
        }
    }

    /// `bounds`, in the window's logical pixels, as a screen rectangle in
    /// device pixels: scaled by `scale`, then offset by where the window's
    /// client area starts on screen. `ClientToScreen` only maps the point;
    /// it sends no message.
    pub(super) fn screen_rect(
        owner: NonZeroIsize,
        bounds: Bounds<Pixels>,
        scale: f32,
    ) -> Option<ScreenRect> {
        let mut origin = POINT::default();
        // SAFETY: `owner` is the live GPUI window and `origin` a local POINT.
        if !unsafe { ClientToScreen(hwnd(owner), &raw mut origin) }.as_bool() {
            tracing::debug!("mock panel: the window's screen position is unavailable");
            return None;
        }
        let left = device(f32::from(bounds.left()) * scale);
        let top = device(f32::from(bounds.top()) * scale);
        let right = device(f32::from(bounds.right()) * scale);
        let bottom = device(f32::from(bounds.bottom()) * scale);
        Some(ScreenRect {
            x: origin.x + left,
            y: origin.y + top,
            width: (right - left).max(0),
            height: (bottom - top).max(0),
        })
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "a window coordinate in device pixels is far inside i32"
    )]
    fn device(value: f32) -> i32 {
        value.round() as i32
    }

    fn hwnd(handle: NonZeroIsize) -> HWND {
        HWND(std::ptr::with_exposed_provenance_mut(
            handle.get().cast_unsigned(),
        ))
    }

    /// An owned, borderless, hidden popup over `rect`: it stays above its
    /// owner, hides while the owner is minimised and has no taskbar button.
    fn create_popup(owner: NonZeroIsize, rect: ScreenRect) -> windows::core::Result<NonZeroIsize> {
        // SAFETY: "STATIC" is a system window class; `owner` is the live
        // GPUI window.
        let window = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOPARENTNOTIFY,
                w!("STATIC"),
                None,
                WS_POPUP | WS_CLIPCHILDREN,
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                Some(hwnd(owner)),
                None,
                None,
                None,
            )
        }?;
        NonZeroIsize::new(window.0.expose_provenance().cast_signed())
            .ok_or_else(windows::core::Error::from_win32)
    }

    fn move_popup(window: NonZeroIsize, webview: &wry::WebView, rect: ScreenRect) {
        // SAFETY: `window` is this panel's live popup.
        let moved = unsafe {
            SetWindowPos(
                hwnd(window),
                None,
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_ASYNCWINDOWPOS,
            )
        };
        if let Err(err) = moved {
            tracing::debug!("mock panel: moving the popup failed: {err}");
        }
        if let Err(err) = webview.set_bounds(rect.fill()) {
            tracing::debug!("mock panel: sizing the webview failed: {err}");
        }
    }

    /// Hides the popup, giving the foreground back to the GPUI window when
    /// the popup had it.
    fn hide_popup(window: NonZeroIsize, owner: NonZeroIsize) {
        give_back_foreground(window, owner);
        // SAFETY: `window` is this panel's live popup.
        let _was_visible = unsafe { ShowWindow(hwnd(window), SW_HIDE) };
    }

    fn give_back_foreground(window: NonZeroIsize, owner: NonZeroIsize) {
        // SAFETY: plain Win32 calls on this panel's popup and its owner.
        unsafe {
            if GetForegroundWindow() == hwnd(window) && !SetForegroundWindow(hwnd(owner)).as_bool()
            {
                tracing::debug!("mock panel: the window could not take the foreground back");
            }
        }
    }

    fn destroy_popup(window: NonZeroIsize) {
        // SAFETY: `window` is this panel's popup, destroyed once.
        if let Err(err) = unsafe { DestroyWindow(hwnd(window)) } {
            tracing::warn!("mock panel: destroying the popup failed: {err}");
        }
    }

    /// A raw HWND as a `HasWindowHandle`, for wry.
    struct RawWindow(NonZeroIsize);

    impl raw_window_handle::HasWindowHandle for RawWindow {
        fn window_handle(
            &self,
        ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
            let handle = raw_window_handle::Win32WindowHandle::new(self.0);
            let raw = raw_window_handle::RawWindowHandle::Win32(handle);
            // SAFETY: the HWND is the panel's live popup; the borrow ends
            // with the `build_as_child` call that takes it.
            Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(raw) })
        }
    }
}

impl Render for MockPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let close = div()
            .id("mock-close")
            .px(px(8.0))
            .py(px(2.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .hover(|style| style.bg(gpui::rgb(HOVER)).text_color(gpui::rgb(TEXT)))
            .child("Close")
            .on_click(cx.listener(|_, _, _, cx| cx.emit(MockPanelEvent::Close)));
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(8.0))
            .py(px(4.0))
            .bg(gpui::rgb(RAISED))
            .border_b_1()
            .border_color(gpui::rgb(LINE))
            .text_color(gpui::rgb(TEXT_2))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .child(self.title.clone()),
            )
            .child(close);
        let body = div()
            .flex()
            .flex_1()
            .min_h(px(0.0))
            .child(MockBody { panel: cx.entity() });
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(header)
            .child(body)
    }
}

/// The panel's body: records its bounds for the webview every frame and
/// blocks the mouse from what is drawn behind it.
struct MockBody {
    panel: Entity<MockPanel>,
}

impl IntoElement for MockBody {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for MockBody {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = Style {
            size: gpui::Size::full(),
            flex_shrink: 1.0,
            ..Style::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        #[cfg(windows)]
        self.panel
            .update(cx, |panel, cx| panel.place(bounds, window, cx));
        #[cfg(not(windows))]
        let _ = cx;
        window.insert_hitbox(bounds, HitboxBehavior::BlockMouse);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        (): &mut Self::PrepaintState,
        window: &mut Window,
        _: &mut App,
    ) {
        let panel = self.panel.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
            if phase == DispatchPhase::Capture && !bounds.contains(&event.position) {
                panel.update(cx, |panel, cx| panel.focus_parent_later(cx));
            }
        });
    }
}

/// The `file://` URL of an absolute path: forward slashes, and every byte
/// outside the unreserved set, `/` and `:` percent-encoded.
pub(crate) fn file_url(path: &Path) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let text = path.to_string_lossy().replace('\\', "/");
    let mut url = String::from(if text.starts_with('/') {
        "file://"
    } else {
        "file:///"
    });
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/:".contains(&byte) {
            url.push(char::from(byte));
        } else {
            url.push('%');
            url.push(char::from(HEX[usize::from(byte >> 4)]));
            url.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    url
}

impl RootView {
    /// Shows a session's mock in its panel, opening the panel on the first.
    pub(crate) fn on_mock_shown(
        &mut self,
        session_id: String,
        title: String,
        path: String,
        cx: &mut Context<Self>,
    ) {
        tracing::info!("mock shown for session {session_id}: {title} ({path})");
        let path = PathBuf::from(path);
        if let Some(panel) = self.mock_panels.get(&session_id) {
            panel.update(cx, |panel, _| panel.show(title, path));
        } else {
            let data_dir = self.ui_dir.as_ref().map(|dir| dir.join("webview2"));
            let id = session_id.clone();
            let panel = cx.new(|_| MockPanel::new(id, title, path, data_dir));
            let key = session_id.clone();
            cx.subscribe(
                &panel,
                move |this, _, event: &MockPanelEvent, cx| match event {
                    MockPanelEvent::Close => {
                        if let Some(panel) = this.mock_panels.remove(&key) {
                            panel.update(cx, MockPanel::release);
                        }
                        cx.notify();
                    }
                },
            )
            .detach();
            self.mock_panels.insert(session_id, panel);
        }
        cx.notify();
    }

    /// Folds a `MockShown` in; returns whether `msg` was one.
    pub(crate) fn on_mock_message(&mut self, msg: &DaemonMessage, cx: &mut Context<Self>) -> bool {
        let DaemonMessage::MockShown {
            session_id,
            title,
            path,
        } = msg
        else {
            return false;
        };
        self.on_mock_shown(session_id.clone(), title.clone(), path.clone(), cx);
        true
    }

    /// Shows `RUSTLING_TULIP_MOCK_DEMO`'s page beside the first session of
    /// the first non-empty session list.
    pub(crate) fn start_mock_demo(&mut self, sessions: &[SessionSnapshot], cx: &mut Context<Self>) {
        let Some(first) = sessions.first() else {
            return;
        };
        if let Some(path) = self.mock_demo.take() {
            self.on_mock_shown(first.id.clone(), "demo".to_owned(), path, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_url_handles_drive_paths_and_spaces() {
        assert_eq!(
            file_url(Path::new(r"X:\a b\mock.html")),
            "file:///X:/a%20b/mock.html"
        );
    }
}
