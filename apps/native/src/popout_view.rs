//! A pop-out window's root: the popped pane's or session's toolbar and
//! terminal, or a popped tab's toolbar and whole grid or diff, drawn from the main
//! window's [`RootView`] over its one daemon connection. It registers no app
//! shortcuts, so every key goes to the terminal.

use crate::fonts::ui_px;
use gpui::{
    AnyElement, AnyWindowHandle, App, Context, FocusHandle, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Render, Subscription, WeakEntity, Window,
    WindowId, div, prelude::*,
};

use crate::overlay_owner::Overlay;
use crate::popouts::{self, PopoutKey};
use crate::term_view::PaneEvent;
use crate::ui_theme::theme;
use crate::{RootView, UI_TEXT_SIZE, fonts};

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
        cx.observe_window_bounds(window, |this, window, cx| {
            this.mark_resized(cx);
            this.remember_place(window, cx);
        })
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

    /// The window was resized: its terminal's next size, or every one of a
    /// popped tab's, claims its session.
    fn mark_resized(&self, cx: &mut Context<Self>) {
        let key = self.key.clone();
        let marked = self.root.update(cx, |root, cx| match &key {
            PopoutKey::Tab { tab_id } => root.mark_tab_panes_resized(tab_id, cx),
            PopoutKey::Pane { .. } | PopoutKey::Session { .. } => {
                if let Some(slot_id) = popouts::terminal_slot(&key) {
                    root.mark_pane_resized(&slot_id, cx);
                }
            }
        });
        if let Err(err) = marked {
            tracing::debug!("pop-out resized after the root view went: {err:#}");
        }
    }

    /// The window moved or was resized: the root view keeps its place for
    /// the next launch.
    fn remember_place(&self, window: &Window, cx: &mut Context<Self>) {
        let key = self.key.clone();
        let remembered = self
            .root
            .update(cx, |root, cx| root.remember_popout_place(&key, window, cx));
        if let Err(err) = remembered {
            tracing::debug!("pop-out moved after the root view went: {err:#}");
        }
    }

    /// The window's activation or the popped terminal's focus changed: its
    /// session counts as shown while the window is active and the terminal
    /// has its keyboard. A popped tab's terminal is its focused pane's.
    fn report_shown(&self, handle: &FocusHandle, window: &Window, cx: &mut Context<Self>) {
        let followed = match &self.key {
            PopoutKey::Tab { .. } => self.tab_terminal(cx),
            PopoutKey::Pane { .. } | PopoutKey::Session { .. } => {
                popouts::terminal_slot(&self.key).map(|slot_id| (slot_id, handle.clone()))
            }
        };
        let Some((slot_id, handle)) = followed else {
            return;
        };
        let focused = window.is_window_active() && handle.is_focused(window);
        if let Err(err) = self.root.update(cx, |root, cx| {
            root.popout_terminal_focus(&slot_id, focused, cx);
        }) {
            tracing::debug!("pop-out focus changed after the root view went: {err:#}");
        }
    }

    /// A popped tab's focused pane, by slot id, and its terminal's focus
    /// handle.
    fn tab_terminal(&self, cx: &App) -> Option<(String, FocusHandle)> {
        let root = self.root.upgrade()?;
        let terminal = root.read(cx).popout_terminal(&self.key)?;
        let terminal = terminal.read(cx);
        Some((terminal.pane_id().to_owned(), terminal.focus_handle()))
    }

    /// The popped pane's toolbar and terminal, or the popped tab's toolbar
    /// and grid, as window `here` draws them, while the pop-out is open and
    /// the root view still has its content. Docked-back content draws in
    /// the main window alone: one terminal drawn in two windows at two
    /// sizes would resize itself for ever.
    fn pane(&self, here: WindowId, cx: &mut App) -> Option<AnyElement> {
        let root = self.root.upgrade()?;
        if !root.read(cx).popouts.is_open(&self.key) {
            return None;
        }
        root.update(cx, |root, cx| root.popout_pane(&self.key, here, cx))
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let here = window.window_handle().window_id();
        let body: AnyElement = self.pane(here, cx).unwrap_or_else(|| {
            div()
                .flex()
                .flex_1()
                .size_full()
                .items_center()
                .justify_center()
                .text_size(ui_px(cx, UI_TEXT_SIZE))
                .text_color(gpui::rgb(t.muted))
                .debug_selector(|| "popout-pane-gone".to_owned())
                .child(PANE_GONE_NOTE)
                .into_any_element()
        });
        let overlays = self.root.upgrade().map_or_else(Vec::new, |root| {
            root.update(cx, |root, cx| root.window_overlays(here, cx))
        });
        let (root, on_press, on_key) = (self.root.clone(), self.root.clone(), self.root.clone());
        let (on_move, on_up) = (self.root.clone(), self.root.clone());
        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(gpui::rgb(t.ground))
            .font_family(fonts::ui_family(cx))
            .capture_any_mouse_down(move |_: &MouseDownEvent, window, cx| {
                if let Err(err) = on_press.update(cx, |root, _| root.note_interaction(window)) {
                    tracing::debug!("a pop-out press after the root view went: {err:#}");
                }
            })
            // Only the keys of an overlay this window draws and the
            // Terminal / Chat key; the app's other shortcuts stay with the
            // main window.
            .capture_key_down(move |event: &KeyDownEvent, window, cx| {
                if let Err(err) =
                    on_key.update(cx, |root, cx| root.on_popout_key(event, window, cx))
                {
                    tracing::debug!("a pop-out key after the root view went: {err:#}");
                }
            })
            // A press anywhere that did not stop at a control drops what
            // the press would drop in the main window, an armed Stop too.
            .on_any_mouse_down(move |event: &MouseDownEvent, window, cx| {
                if let Err(err) =
                    root.update(cx, |root, cx| root.on_any_mouse_down(event, window, cx))
                {
                    tracing::debug!("a pop-out press after the root view went: {err:#}");
                }
            })
            // A divider drag a popped tab's grid started follows the
            // pointer in this window only.
            .on_mouse_move(move |event: &MouseMoveEvent, window, cx| {
                if let Err(err) =
                    on_move.update(cx, |root, cx| root.on_drag_move(event, window, cx))
                {
                    tracing::debug!("a pop-out drag after the root view went: {err:#}");
                }
            })
            .on_mouse_up(
                MouseButton::Left,
                move |event: &MouseUpEvent, window, cx| {
                    if let Err(err) =
                        on_up.update(cx, |root, cx| root.on_drag_end(event, window, cx))
                    {
                        tracing::debug!("a pop-out release after the root view went: {err:#}");
                    }
                },
            )
            .child(body)
            .children(overlays)
    }
}

/// The overlays drawn in whichever window opened them: what a window draws,
/// takes keys for, and hands the keyboard back to once one closes.
impl RootView {
    /// Whether `overlay` is open and the main window draws it: the main
    /// window's own checks count only what covers it.
    pub(crate) fn open_in_main(&self, overlay: Overlay) -> bool {
        let open = match overlay {
            Overlay::SessionMenu => self.menu.is_some(),
            Overlay::ShellMenu => self.shell_menu.is_some(),
            Overlay::EmptyPaneMenu => self.pane_ui.menu.is_some(),
            Overlay::PaneClose => self.pane_ui.close.is_some(),
        };
        open && self.overlays.shows(overlay, self.layouts.main())
    }

    /// Whether `window` is the main window.
    pub(crate) fn is_main_window(&self, window: &Window) -> bool {
        window.window_handle().window_id() == self.layouts.main()
    }

    /// Opens a dialog that only the main window draws, asked for from a
    /// pop-out: once this update is over the main window comes to the front
    /// (a cloaked client never activates one) and `open` runs with its
    /// window, so the dialog takes the main window's keys.
    pub(crate) fn open_in_main_window(
        &self,
        cx: &mut Context<Self>,
        open: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        Self::in_window(self.layouts.main(), cx, move |root, window, cx| {
            if !crate::offscreen::requested() {
                window.activate_window();
            }
            open(root, window, cx);
        });
    }

    /// The user pressed a mouse button or a key in `window`.
    pub(crate) fn note_interaction(&mut self, window: &Window) {
        self.overlays.interacted(window.window_handle().window_id());
    }

    /// The menus, the pane-close dialog and the toasts pop-out window
    /// `here` draws, in the main window's stacking order.
    pub(crate) fn window_overlays(
        &self,
        here: WindowId,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut layers: Vec<AnyElement> = Vec::new();
        layers.extend(self.session_menu_layer(here, cx).into_iter().flatten());
        layers.extend(self.shell_menu_layer(here, cx).into_iter().flatten());
        layers.extend(self.empty_pane_menu_layer(here, cx).into_iter().flatten());
        layers.extend(self.pane_close_layer(here, cx));
        layers.extend(self.toast_layer(here, cx));
        layers
    }

    /// A key in a pop-out window: the pane-close dialog it draws takes
    /// every key, Esc closes a menu it draws, and Ctrl+Shift+M switches its
    /// pane between terminal and chat. Anything else goes on to its
    /// terminal.
    pub(crate) fn on_popout_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.note_interaction(window);
        let ks = &event.keystroke;
        let here = window.window_handle().window_id();
        let escape = ks.key == "escape";
        let taken = if self.on_pane_ui_key(ks, window, cx) {
            true
        } else if escape && self.menu.is_some() && self.overlays.shows(Overlay::SessionMenu, here) {
            self.close_session_menu(window, cx);
            true
        } else if escape
            && self.shell_menu.is_some()
            && self.overlays.shows(Overlay::ShellMenu, here)
        {
            self.close_shell_menu(window, cx);
            true
        } else if crate::keys::is_pane_view_key(ks) {
            // Swallowed even with nothing to switch: Ctrl+M is Enter to
            // the terminal.
            self.toggle_window_pane_view(here, window, cx);
            true
        } else {
            false
        };
        if taken {
            cx.stop_propagation();
        }
    }

    /// Window `window_id` closed or docked back: the overlays it drew close
    /// with it, and its toasts move to the main window.
    pub(crate) fn drop_window_overlays(&mut self, window_id: WindowId) {
        for overlay in self.overlays.forget_window(window_id) {
            match overlay {
                Overlay::SessionMenu => {
                    self.confirm.disarm();
                    self.menu = None;
                }
                Overlay::ShellMenu => self.shell_menu = None,
                Overlay::EmptyPaneMenu => self.pane_ui.menu = None,
                Overlay::PaneClose => self.pane_ui.close = None,
            }
        }
    }

    /// An overlay that `owner` drew closed: that window's terminal takes
    /// its keyboard back, the main window's active pane in the main window.
    /// From another window the hand-back waits for this update to end.
    pub(crate) fn hand_back_focus(
        &self,
        owner: WindowId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if owner == window.window_handle().window_id() {
            self.focus_window_terminal(window, cx);
        } else {
            Self::in_window(owner, cx, |root, window, cx| {
                root.focus_window_terminal(window, cx);
            });
        }
    }

    /// Gives `window`'s keyboard to its terminal: the active pane in the
    /// main window, the popped terminal or diff in a pop-out.
    fn focus_window_terminal(&self, window: &mut Window, cx: &Context<Self>) {
        let here = window.window_handle().window_id();
        if here == self.layouts.main() {
            self.focus_active_pane(window, cx);
        } else if let Some(focus) = self.popout_focus_in(here, cx) {
            focus.focus(window);
        }
    }

    /// A terminal's event, made in the window its slot was created in, acts
    /// for the window that draws the terminal now: a popped terminal's
    /// gutter menu opens in its pop-out.
    pub(crate) fn route_pane_event(
        &mut self,
        slot_id: &str,
        event: &PaneEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.slot_window(slot_id) {
            Some(drawn_in) if drawn_in != window.window_handle().window_id() => {
                let event = event.clone();
                Self::in_window(drawn_in, cx, move |root, window, cx| {
                    root.on_pane_event(&event, window, cx);
                });
            }
            _ => self.on_pane_event(event, window, cx),
        }
    }

    /// The pop-out window that draws terminal `slot_id`, if one does: its
    /// session's, its pane's, or its tab's.
    pub(crate) fn slot_window(&self, slot_id: &str) -> Option<WindowId> {
        let key = match popouts::slot_session(slot_id) {
            Some(session_id) => PopoutKey::Session {
                session_id: session_id.to_owned(),
            },
            None => match self.popouts.popped_key(slot_id) {
                Some(key) => key.clone(),
                None => self.tab_popout_of_pane(slot_id)?,
            },
        };
        self.popout_windows
            .get(&key)
            .map(|popout| AnyWindowHandle::from(popout.handle).window_id())
    }

    /// What takes the keyboard in pop-out window `window_id`: its terminal,
    /// or a popped diff tab's view.
    fn popout_focus_in(&self, window_id: WindowId, cx: &App) -> Option<FocusHandle> {
        let key = self
            .popout_windows
            .iter()
            .find(|(_, popout)| AnyWindowHandle::from(popout.handle).window_id() == window_id)
            .map(|(key, _)| key)?;
        self.popout_focus(key, cx)
    }

    /// Runs `act` for window `window_id` once this update is over; a window
    /// closed by then is passed over.
    pub(crate) fn in_window(
        window_id: WindowId,
        cx: &mut Context<Self>,
        act: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let root = cx.weak_entity();
        cx.defer(move |cx| {
            let handle = cx
                .windows()
                .into_iter()
                .find(|handle| handle.window_id() == window_id);
            let Some(handle) = handle else {
                tracing::debug!(?window_id, "the window to act for has closed");
                return;
            };
            let done = handle.update(cx, |_, window, cx| {
                root.update(cx, |root, cx| act(root, window, cx))
            });
            if let Err(err) = done.and_then(std::convert::identity) {
                tracing::debug!(?window_id, "acting for a window: {err:#}");
            }
        });
    }
}
