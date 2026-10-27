//! A pop-out window's root: the popped pane's own terminal, drawn from the
//! main window's [`RootView`] over its one daemon connection. It registers
//! no app shortcuts, so every key goes to the terminal.

use gpui::{
    AnyElement, App, Context, Entity, IntoElement, Render, WeakEntity, Window, div, prelude::*, px,
};

use crate::palette::GROUND;
use crate::popouts::PopoutKey;
use crate::term_view::TerminalPane;
use crate::{MUTED, RootView, UI_TEXT_SIZE, fonts};

/// What a pop-out whose pane has gone shows.
pub(crate) const PANE_GONE_NOTE: &str = "This pane is no longer open";

/// The root view of one pop-out window. Its draw reads the root view, so
/// gpui redraws the window whenever the root view changes.
pub(crate) struct PopoutView {
    root: WeakEntity<RootView>,
    key: PopoutKey,
}

impl PopoutView {
    /// The view for `key`'s window, which docks the pop-out back when the
    /// window is asked to close.
    pub(crate) fn new(
        root: WeakEntity<RootView>,
        key: PopoutKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        watch_close_request(root.clone(), window, cx);
        cx.observe_window_bounds(window, |this, _, cx| this.mark_resized(cx))
            .detach();
        Self { root, key }
    }

    /// The popped pane moved to another tab: the window now holds `key`.
    pub(crate) fn follow(&mut self, key: PopoutKey) {
        self.key = key;
    }

    /// The window was resized: the popped pane's next size claims its
    /// session.
    fn mark_resized(&self, cx: &mut Context<Self>) {
        let PopoutKey::Pane { pane_id, .. } = &self.key else {
            return;
        };
        if let Err(err) = self
            .root
            .update(cx, |root, cx| root.mark_pane_resized(pane_id, cx))
        {
            tracing::debug!("pop-out resized after the root view went: {err:#}");
        }
    }

    /// The popped pane's terminal, while the pop-out is open and the root
    /// view still has the pane. A docked-back pane draws in the main grid
    /// alone: one terminal drawn in two windows at two sizes would resize
    /// itself for ever.
    fn terminal(&self, cx: &App) -> Option<Entity<TerminalPane>> {
        let root = self.root.upgrade()?;
        let root = root.read(cx);
        if !root.popouts.is_open(&self.key) {
            return None;
        }
        root.popout_terminal(&self.key)
    }
}

/// A close request docks the pop-out back and lets the window close; it
/// never reaches the main window's quit flow. The window is found by its id,
/// since its key follows a moved pane.
fn watch_close_request(
    root: WeakEntity<RootView>,
    window: &mut Window,
    cx: &mut Context<PopoutView>,
) {
    window.on_window_should_close(cx, move |window, cx| {
        let window_id = window.window_handle().window_id();
        if let Err(err) = root.update(cx, |root, cx| root.forget_popout_window(window_id, cx)) {
            tracing::debug!(?window_id, "pop-out closed after the root view: {err:#}");
        }
        true
    });
}

impl Render for PopoutView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body: AnyElement = match self.terminal(cx) {
            Some(terminal) => div()
                .flex_1()
                .min_h(px(0.0))
                .size_full()
                .child(terminal)
                .into_any_element(),
            None => div()
                .flex()
                .flex_1()
                .size_full()
                .items_center()
                .justify_center()
                .text_size(px(UI_TEXT_SIZE))
                .text_color(gpui::rgb(MUTED))
                .debug_selector(|| "popout-pane-gone".to_owned())
                .child(PANE_GONE_NOTE)
                .into_any_element(),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(gpui::rgb(GROUND))
            .font_family(fonts::ui_family(cx))
            .child(body)
    }
}
