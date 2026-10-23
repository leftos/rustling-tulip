//! A session's mock page: the HTML file its Claude showed with `show_mock`,
//! drawn live by a `WebView2` child window inside the session's pane.
//!
//! The child window paints above every GPUI layer, so the view holding the
//! panel hides it whenever something is drawn over the panes.
//!
//! Building the webview runs a nested Win32 message pump, and creating,
//! focusing away from or destroying the child sends messages to the GPUI
//! window. Each of those runs in a foreground task holding no GPUI borrow,
//! never inside a frame or an event handler, where the re-entered window
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
    /// Shared with the tasks that call it outside a frame.
    #[cfg(windows)]
    webview: Option<std::rc::Rc<wry::WebView>>,
    /// Lent to the build task while it runs.
    #[cfg(windows)]
    web_context: Option<wry::WebContext>,
    /// The URL the webview last tried to load; a frame whose page differs
    /// (the page changed during the build) loads it.
    #[cfg(windows)]
    loaded_url: Option<String>,
    /// The webview build was scheduled, so a failed one is not retried
    /// every frame.
    #[cfg(windows)]
    build_tried: bool,
    /// The visibility last sent to the webview.
    #[cfg(windows)]
    shown: bool,
    /// The body's bounds at the last frame.
    #[cfg(windows)]
    bounds: Bounds<Pixels>,
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
            webview: None,
            #[cfg(windows)]
            web_context: None,
            #[cfg(windows)]
            loaded_url: None,
            #[cfg(windows)]
            build_tried: false,
            #[cfg(windows)]
            shown: false,
            #[cfg(windows)]
            bounds: Bounds::default(),
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

    /// Hides and destroys the webview after the current frame; the panel
    /// is being closed.
    pub(crate) fn release(&mut self, cx: &mut Context<Self>) {
        #[cfg(windows)]
        if let Some(webview) = self.webview.take() {
            let context = self.web_context.take();
            cx.spawn(async move |_, _| {
                hide(&webview);
                drop(webview);
                drop(context);
            })
            .detach();
        }
        #[cfg(not(windows))]
        let _ = cx;
    }

    /// Gives the keyboard back to the window after the current event.
    fn focus_parent_later(&self, cx: &mut Context<Self>) {
        #[cfg(windows)]
        if let Some(webview) = self.webview.clone() {
            cx.spawn(async move |_, _| {
                if let Err(err) = webview.focus_parent() {
                    tracing::debug!("mock panel: returning focus to the window failed: {err}");
                }
            })
            .detach();
        }
        #[cfg(not(windows))]
        let _ = cx;
    }
}

/// What the build task needs, taken out of the panel so the build holds
/// no GPUI borrow.
#[cfg(windows)]
struct BuildJob {
    session_id: String,
    url: String,
    rect: wry::Rect,
    parent: ParentWindow,
    context: wry::WebContext,
}

#[cfg(windows)]
impl BuildJob {
    /// Builds the webview; `build_as_child` pumps messages until `WebView2`'s
    /// environment and controller exist.
    fn run(mut self) -> (wry::WebContext, String, Option<wry::WebView>) {
        let built = wry::WebViewBuilder::new_with_web_context(&mut self.context)
            .with_url(self.url.as_str())
            .with_bounds(self.rect)
            .build_as_child(&self.parent);
        let webview = match built {
            Ok(webview) => {
                tracing::info!(
                    "mock panel for session {}: webview built for {}",
                    self.session_id,
                    self.url
                );
                Some(webview)
            }
            Err(err) => {
                tracing::warn!(
                    "mock panel for session {}: the webview could not be built: {err}",
                    self.session_id
                );
                None
            }
        };
        (self.context, self.url, webview)
    }
}

#[cfg(windows)]
impl MockPanel {
    fn load_current(&mut self) {
        let Some(webview) = &self.webview else {
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
        self.loaded_url = Some(url);
    }

    /// Records the body's bounds each frame, schedules the build on the
    /// first frame with a real window, and fits a built webview to the
    /// bounds.
    fn place(&mut self, bounds: Bounds<Pixels>, window: &Window, cx: &mut Context<Self>) {
        self.bounds = bounds;
        if self.webview.is_none() && !self.build_tried {
            self.build_tried = true;
            if let Some(parent) = ParentWindow::of(window) {
                Self::schedule_build(parent, cx);
            }
        }
        if self.webview.is_some() && self.loaded_url.as_deref() != Some(&file_url(&self.path)) {
            self.load_current();
        }
        let Some(webview) = &self.webview else {
            return;
        };
        if let Err(err) = webview.set_bounds(logical_rect(bounds)) {
            tracing::debug!("mock panel: setting the webview's bounds failed: {err}");
        }
        self.sync_visibility(cx);
    }

    fn schedule_build(parent: ParentWindow, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let job = this.update(cx, |panel, _| panel.build_job(parent));
            let Ok(job) = job else {
                tracing::debug!("mock panel: closed before its webview was built");
                return;
            };
            let built = job.run();
            if let Err(err) = this.update(cx, |panel, cx| panel.finish_build(built, cx)) {
                tracing::debug!("mock panel: closed while its webview was built: {err}");
            }
        })
        .detach();
    }

    fn build_job(&mut self, parent: ParentWindow) -> BuildJob {
        let data_dir = self.data_dir.clone();
        BuildJob {
            session_id: self.session_id.clone(),
            url: file_url(&self.path),
            rect: logical_rect(self.bounds),
            parent,
            context: self
                .web_context
                .take()
                .unwrap_or_else(|| wry::WebContext::new(data_dir)),
        }
    }

    fn finish_build(
        &mut self,
        (context, url, webview): (wry::WebContext, String, Option<wry::WebView>),
        cx: &mut Context<Self>,
    ) {
        self.web_context = Some(context);
        let Some(webview) = webview else {
            return;
        };
        self.webview = Some(std::rc::Rc::new(webview));
        // Built visible; a hide asked for during the build goes out now.
        self.shown = true;
        self.loaded_url = Some(url);
        self.sync_visibility(cx);
        cx.notify();
    }

    /// Sends a change of visibility to the webview after the current
    /// frame: hiding gives the keyboard back to the window, which sends
    /// it a focus message.
    fn sync_visibility(&mut self, cx: &mut Context<Self>) {
        let visible = !self.hidden;
        let Some(webview) = self.webview.clone() else {
            return;
        };
        if self.shown == visible {
            return;
        }
        self.shown = visible;
        cx.spawn(async move |_, _| {
            if visible {
                if let Err(err) = webview.set_visible(true) {
                    tracing::warn!("mock panel: showing the webview failed: {err}");
                }
            } else {
                hide(&webview);
            }
        })
        .detach();
    }
}

#[cfg(windows)]
fn hide(webview: &wry::WebView) {
    if let Err(err) = webview.focus_parent() {
        tracing::debug!("mock panel: returning focus to the window failed: {err}");
    }
    if let Err(err) = webview.set_visible(false) {
        tracing::warn!("mock panel: hiding the webview failed: {err}");
    }
}

#[cfg(windows)]
fn logical_rect(bounds: Bounds<Pixels>) -> wry::Rect {
    use wry::dpi::{LogicalPosition, LogicalSize, Position, Size};
    wry::Rect {
        position: Position::Logical(LogicalPosition::new(
            f64::from(f32::from(bounds.origin.x)),
            f64::from(f32::from(bounds.origin.y)),
        )),
        size: Size::Logical(LogicalSize::new(
            f64::from(f32::from(bounds.size.width)),
            f64::from(f32::from(bounds.size.height)),
        )),
    }
}

/// The GPUI window's raw HWND, taken during a frame and handed to the
/// build task, which builds the webview from it without the window.
#[cfg(windows)]
struct ParentWindow(std::num::NonZeroIsize);

#[cfg(windows)]
impl ParentWindow {
    /// The handle of `window` when it is a real Win32 window. The test
    /// platform's window has no GPU and panics when asked for its handle,
    /// so a window without GPU specs is never asked.
    fn of(window: &Window) -> Option<Self> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if window.gpu_specs().is_none() {
            tracing::debug!("mock panel: the window has no GPU-backed platform window");
            return None;
        }
        // `Window::window_handle` is gpui's own handle; the raw one is the trait's.
        match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
            Ok(RawWindowHandle::Win32(handle)) => Some(Self(handle.hwnd)),
            other => {
                tracing::warn!("mock panel: the window has no Win32 handle ({other:?})");
                None
            }
        }
    }
}

#[cfg(windows)]
impl raw_window_handle::HasWindowHandle for ParentWindow {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let handle = raw_window_handle::Win32WindowHandle::new(self.0);
        let raw = raw_window_handle::RawWindowHandle::Win32(handle);
        // SAFETY: the HWND is the GPUI window's, which lives as long as the
        // app's only window; the borrow ends with the `build_as_child` call
        // that takes it.
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(raw) })
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
