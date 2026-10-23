//! A session's mock page: the HTML file its Claude showed with `show_mock`,
//! drawn live by a `WebView2` child window inside the session's pane.
//!
//! The child window paints above every GPUI layer, so the view holding the
//! panel hides it whenever something is drawn over the panes.

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
    /// Dropped before the context it was built with.
    #[cfg(windows)]
    webview: Option<wry::WebView>,
    #[cfg(windows)]
    web_context: Option<wry::WebContext>,
    /// A page load failed; the next frame tries it again.
    #[cfg(windows)]
    pending_reload: bool,
    /// The webview build was tried, so a failed one is not retried every
    /// frame.
    #[cfg(windows)]
    build_tried: bool,
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
            pending_reload: false,
            #[cfg(windows)]
            build_tried: false,
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
    /// keyboard back to the window; the next frame shows it again.
    pub(crate) fn set_hidden(&mut self, hidden: bool) {
        self.hidden = hidden;
        #[cfg(windows)]
        if hidden && let Some(webview) = &self.webview {
            hide(webview);
        }
    }
}

#[cfg(windows)]
impl MockPanel {
    fn load_current(&mut self) {
        let Some(webview) = &self.webview else {
            return;
        };
        self.pending_reload = false;
        if let Err(err) = webview.load_url(&file_url(&self.path)) {
            tracing::warn!(
                "mock panel for session {}: loading {} failed: {err}",
                self.session_id,
                self.path.display()
            );
            self.pending_reload = true;
        }
    }

    /// Builds the webview on the first frame with a real window, then fits
    /// it to `bounds` every frame.
    fn place(&mut self, bounds: Bounds<Pixels>, window: &Window) {
        let rect = logical_rect(bounds);
        if self.webview.is_none() && !self.build_tried {
            self.build_tried = true;
            self.webview = self.build(rect, window);
        }
        if self.pending_reload {
            self.load_current();
        }
        let Some(webview) = &self.webview else {
            return;
        };
        if let Err(err) = webview.set_bounds(rect) {
            tracing::debug!("mock panel: setting the webview's bounds failed: {err}");
        }
        if let Err(err) = webview.set_visible(!self.hidden) {
            tracing::debug!("mock panel: setting the webview's visibility failed: {err}");
        }
    }

    fn build(&mut self, rect: wry::Rect, window: &Window) -> Option<wry::WebView> {
        let parent = ParentWindow::of(window)?;
        let data_dir = self.data_dir.clone();
        let context = self
            .web_context
            .get_or_insert_with(|| wry::WebContext::new(data_dir));
        let built = wry::WebViewBuilder::new_with_web_context(context)
            .with_url(file_url(&self.path))
            .with_bounds(rect)
            .build_as_child(&parent);
        match built {
            Ok(webview) => {
                tracing::info!(
                    "mock panel for session {}: webview built for {}",
                    self.session_id,
                    self.path.display()
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
        }
    }

    fn focus_parent(&self) {
        if let Some(webview) = &self.webview
            && let Err(err) = webview.focus_parent()
        {
            tracing::debug!("mock panel: returning focus to the window failed: {err}");
        }
    }
}

#[cfg(windows)]
impl Drop for MockPanel {
    fn drop(&mut self) {
        if let Some(webview) = self.webview.take() {
            hide(&webview);
        }
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

/// The GPUI window's Win32 handle, owned so the webview can be built from
/// it without borrowing the window.
#[cfg(windows)]
struct ParentWindow(raw_window_handle::Win32WindowHandle);

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
            Ok(RawWindowHandle::Win32(handle)) => Some(Self(handle)),
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
        let raw = raw_window_handle::RawWindowHandle::Win32(self.0);
        // SAFETY: the handle is the live GPUI window's, read this frame, and
        // the borrow ends with the `build_as_child` call that takes it.
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

/// The panel's body: places the webview over its bounds every frame and
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
            .update(cx, |panel, _| panel.place(bounds, window));
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
                #[cfg(windows)]
                panel.read(cx).focus_parent();
                #[cfg(not(windows))]
                let _ = (&panel, cx);
            }
        });
    }
}

/// The `file://` URL of an absolute path: forward slashes, and every byte
/// outside the unreserved set, `/` and `:` percent-encoded.
pub(crate) fn file_url(path: &Path) -> String {
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
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
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
                        this.mock_panels.remove(&key);
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
