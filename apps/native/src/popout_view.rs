//! A pop-out window's root: the popped pane's toolbar and terminal, drawn
//! from the main window's [`RootView`] over its one daemon connection. It
//! registers no app shortcuts, so every key goes to the terminal.

use gpui::{
    AnyElement, App, Context, FocusHandle, IntoElement, MouseDownEvent, Render, Subscription,
    WeakEntity, Window, div, prelude::*, px,
};

use crate::palette::GROUND;
use crate::popouts::PopoutKey;
use crate::{MUTED, RootView, UI_TEXT_SIZE, fonts};

/// What a pop-out whose pane has gone shows.
pub(crate) const PANE_GONE_NOTE: &str = "This pane is no longer open";

/// The root view of one pop-out window. Its draw reads the root view, so
/// gpui redraws the window whenever the root view changes.
pub(crate) struct PopoutView {
    root: WeakEntity<RootView>,
    key: PopoutKey,
    /// Tells the root view when the popped terminal gains and loses this
    /// window's keyboard.
    _focus: Vec<Subscription>,
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
        let focus = watch_terminal_focus(&root, &key, window, cx);
        Self {
            root,
            key,
            _focus: focus,
        }
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

    /// The window's activation or the popped terminal's focus changed: its
    /// session counts as shown while the window is active and the terminal
    /// has its keyboard.
    fn report_shown(&self, handle: &FocusHandle, window: &Window, cx: &mut Context<Self>) {
        let PopoutKey::Pane { pane_id, .. } = &self.key else {
            return;
        };
        let focused = window.is_window_active() && handle.is_focused(window);
        if let Err(err) = self.root.update(cx, |root, cx| {
            root.popout_terminal_focus(pane_id, focused, cx);
        }) {
            tracing::debug!("pop-out focus changed after the root view went: {err:#}");
        }
    }

    /// The popped pane's toolbar and terminal, while the pop-out is open and
    /// the root view still has the pane. A docked-back pane draws in the
    /// main grid alone: one terminal drawn in two windows at two sizes
    /// would resize itself for ever.
    fn pane(&self, cx: &mut App) -> Option<AnyElement> {
        let root = self.root.upgrade()?;
        if !root.read(cx).popouts.is_open(&self.key) {
            return None;
        }
        root.update(cx, |root, cx| root.popout_pane(&self.key, cx))
    }
}

/// Follows the pop-out window's activation and the popped terminal's focus
/// there: a session shown in the active window with the keyboard counts as
/// seen. gpui sends no focus event when a window is only deactivated or
/// reactivated, so activation is observed too.
fn watch_terminal_focus(
    root: &WeakEntity<RootView>,
    key: &PopoutKey,
    window: &mut Window,
    cx: &mut Context<PopoutView>,
) -> Vec<Subscription> {
    let handle = root.upgrade().and_then(|root| {
        let terminal = root.read(cx).popout_terminal(key)?;
        Some(terminal.read(cx).focus_handle())
    });
    let Some(handle) = handle else {
        tracing::debug!(?key, "a pop-out opened with no terminal to follow");
        return Vec::new();
    };
    let (on_in, on_blur, on_active) = (handle.clone(), handle.clone(), handle.clone());
    vec![
        cx.on_focus_in(&handle, window, move |this, window, cx| {
            this.report_shown(&on_in, window, cx);
        }),
        cx.on_blur(&handle, window, move |this, window, cx| {
            this.report_shown(&on_blur, window, cx);
        }),
        cx.observe_window_activation(window, move |this, window, cx| {
            this.report_shown(&on_active, window, cx);
        }),
    ]
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
        let body: AnyElement = self.pane(cx).unwrap_or_else(|| {
            div()
                .flex()
                .flex_1()
                .size_full()
                .items_center()
                .justify_center()
                .text_size(px(UI_TEXT_SIZE))
                .text_color(gpui::rgb(MUTED))
                .debug_selector(|| "popout-pane-gone".to_owned())
                .child(PANE_GONE_NOTE)
                .into_any_element()
        });
        let root = self.root.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(gpui::rgb(GROUND))
            .font_family(fonts::ui_family(cx))
            // A press anywhere that did not stop at a control drops what
            // the press would drop in the main window, an armed Stop too.
            .on_any_mouse_down(move |event: &MouseDownEvent, window, cx| {
                if let Err(err) =
                    root.update(cx, |root, cx| root.on_any_mouse_down(event, window, cx))
                {
                    tracing::debug!("a pop-out press after the root view went: {err:#}");
                }
            })
            .child(body)
    }
}
