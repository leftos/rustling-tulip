//! The jump palette on screen: opening and closing it, its keys and query
//! field, a click on a row, and its panel. The entries, their order and
//! ranking, the highlight and what Enter does live in [`crate::quick_jump`];
//! the jump itself is [`RootView::jump_to`].

use gpui::{
    AnyElement, Context, ElementId, Entity, Focusable as _, Keystroke, MouseButton, MouseDownEvent,
    ScrollHandle, SharedString, Subscription, Window, div, prelude::*, px,
};

use crate::buttons::{Layer, field_frame};
use crate::driver_id::DriverId as _;
use crate::fonts::{ui_px, ui_text_size};
use crate::quick_jump::{Entry, JumpAction, QuickJump};
use crate::session_menu::popover_frame;
use crate::spawn_view::{DISABLED_OPACITY, SEGMENT_RADIUS, labelled_text};
use crate::target::HostKey;
use crate::text_input::{TextChanged, TextInput, TextInputEvent};
use crate::ui_theme::theme;
use crate::{RootView, is_quick_jump_key};

/// The query field's text while it is empty.
const QUERY_PLACEHOLDER: &str = "Jump to a pane, tab, session or repo";
/// The one row while the query matches nothing.
const NO_MATCH: &str = "No matching pane, tab, session or repo";
/// The panel's width.
const PANEL_WIDTH: f32 = 560.0;
/// How far below the window's top the panel starts.
const PANEL_TOP: f32 = 64.0;
/// A row's height before the UI text offset.
const ROW_HEIGHT: f32 = 26.0;
/// How many rows show before the list scrolls.
const VISIBLE_ROWS: f32 = 12.0;

/// The open palette: its core, its query field, its list's scroll and the
/// field's subscriptions.
pub(crate) struct QuickJumpState {
    core: QuickJump,
    query: Entity<TextInput>,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl QuickJumpState {
    /// Scrolls the list so the highlighted row shows.
    fn reveal(&self) {
        if let Some(at) = self.core.highlight() {
            self.scroll.scroll_to_item(at);
        }
    }
}

/// How an entry reads in the specs: its tag, a space, its name.
fn row_text(entry: &Entry) -> String {
    format!("{} {}", entry.kind.tag(), entry.primary)
}

impl RootView {
    /// Whether the jump palette is open.
    #[must_use]
    pub fn quick_jump_open(&self) -> bool {
        self.quick_jump.is_some()
    }

    /// The palette's rows, best first, each its tag and its name.
    #[must_use]
    pub fn quick_jump_rows(&self) -> Vec<String> {
        self.quick_jump
            .as_ref()
            .map(|state| state.core.filtered().map(row_text).collect())
            .unwrap_or_default()
    }

    /// The highlighted row, as [`Self::quick_jump_rows`] reads it.
    #[must_use]
    pub fn quick_jump_highlighted(&self) -> Option<String> {
        let core = &self.quick_jump.as_ref()?.core;
        core.filtered().nth(core.highlight()?).map(row_text)
    }

    /// Puts `query` in the open palette's field and narrows the list to it,
    /// as typing it does.
    pub fn quick_jump_set_query(
        &mut self,
        query: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = &mut self.quick_jump else {
            return;
        };
        state
            .query
            .update(cx, |input, cx| input.set_text(query.to_owned(), cx));
        state.core.set_query(query);
        state.reveal();
        cx.notify();
    }

    /// Opens the palette over the main window, the keyboard in its query
    /// field; asked from a pop-out, the main window opens it in front. Open
    /// already, it closes.
    pub(crate) fn open_quick_jump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_main_window(window) {
            self.open_in_main_window(cx, Self::open_quick_jump);
            return;
        }
        if self.quick_jump.is_some() {
            self.close_quick_jump(window, cx);
            return;
        }
        // Asked from a pop-out, the main window may hold a dialog, a menu
        // or Settings, which the palette never opens over.
        if self.main_window_held() {
            return;
        }
        let query = cx.new(|cx| TextInput::new("", QUERY_PLACEHOLDER, cx));
        let subscriptions = Self::watch_quick_jump_query(&query, window, cx);
        query.read(cx).focus_handle(cx).focus(window);
        self.quick_jump = Some(QuickJumpState {
            core: QuickJump::new(self.quick_jump_entries()),
            query,
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        });
        cx.notify();
    }

    /// Closes the palette and hands the keyboard back to the active pane.
    pub(crate) fn close_quick_jump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.quick_jump.take().is_some() {
            self.focus_active_pane(window, cx);
            cx.notify();
        }
    }

    /// Closes the palette without handing the keyboard back, then jumps:
    /// the jump puts the keyboard where it goes, and a container jump's
    /// sidebar row must not lose it to a pane.
    fn run_quick_jump(&mut self, action: JumpAction, window: &mut Window, cx: &mut Context<Self>) {
        let query = self
            .quick_jump
            .take()
            .map(|state| state.query.read(cx).focus_handle(cx));
        self.jump_to(action, window, cx);
        // An entry gone stale while the palette was open jumps nowhere; the
        // keyboard must not stay on the closed query field.
        let stuck = query.is_some_and(|handle| handle.is_focused(window));
        if stuck || window.focused(cx).is_none() {
            self.focus_active_pane(window, cx);
        }
        cx.notify();
    }

    /// Enter: jumps to the highlighted row; with no row, nothing happens.
    fn enter_quick_jump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let action = self
            .quick_jump
            .as_ref()
            .and_then(|state| state.core.enter());
        if let Some(action) = action {
            self.run_quick_jump(action, window, cx);
        }
    }

    /// A press on the row at `at`: jumps to it.
    fn press_quick_jump_row(&mut self, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        let action = self
            .quick_jump
            .as_ref()
            .and_then(|state| state.core.filtered().nth(at))
            .map(|entry| entry.action.clone());
        if let Some(action) = action {
            self.run_quick_jump(action, window, cx);
        }
    }

    /// A key while the palette is open; returns whether it took it. Up and
    /// Down move the highlight, Enter jumps, Esc and the palette's key
    /// close it; any other key goes on to the query field.
    pub(crate) fn on_quick_jump_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if is_quick_jump_key(keystroke) || keystroke.key == "escape" {
            self.close_quick_jump(window, cx);
            return true;
        }
        if keystroke.key == "enter" {
            self.enter_quick_jump(window, cx);
            return true;
        }
        let Some(state) = &mut self.quick_jump else {
            return false;
        };
        match keystroke.key.as_str() {
            "up" => state.core.up(),
            "down" => state.core.down(),
            _ => return false,
        }
        state.reveal();
        cx.notify();
        true
    }

    /// Enter and Esc in the query field (its own keys, which the palette's
    /// key handling never sees) jump and close; an edit narrows the list.
    fn watch_quick_jump_query(
        input: &Entity<TextInput>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        let keys = cx.subscribe_in(
            input,
            window,
            |this, _, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => this.enter_quick_jump(window, cx),
                TextInputEvent::Cancel => this.close_quick_jump(window, cx),
            },
        );
        let edits = cx.subscribe_in(input, window, |this, input, _: &TextChanged, _, cx| {
            let text = input.read(cx).text().to_owned();
            if let Some(state) = &mut this.quick_jump {
                state.core.set_query(&text);
                state.reveal();
            }
            cx.notify();
        });
        vec![keys, edits]
    }

    /// The palette's panel, 64 px under the window's top and centred: the
    /// query field over the ranked rows, which scroll past twelve. A press
    /// outside it closes it.
    pub(crate) fn quick_jump_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.quick_jump.as_ref()?;
        let t = theme(cx);
        let row_height = ui_px(cx, ROW_HEIGHT);
        let query = field_frame(true, Layer::Floating, &t)
            .driver_id(|| "quick-jump-query".to_owned())
            .flex_none()
            .w_full()
            .child(state.query.clone());
        let rows = div()
            .id("quick-jump-rows")
            .debug_selector(|| "quick-jump-rows".to_owned())
            .track_scroll(&state.scroll)
            .flex()
            .flex_col()
            .max_h(row_height * VISIBLE_ROWS)
            .overflow_y_scroll()
            .children(self.quick_jump_rows_view(state, cx));
        let panel = popover_frame(ui_text_size(cx), "quick-jump", &t)
            .w(px(PANEL_WIDTH))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .p(px(4.0))
            // A press on the padding or the no-match row keeps the keyboard
            // in the query field.
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, window, cx| {
                this.close_quick_jump(window, cx);
            }))
            .child(query)
            .child(rows);
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(PANEL_TOP))
                .child(panel)
                .into_any_element(),
        )
    }

    /// The list's rows, or its one dimmed no-match row.
    fn quick_jump_rows_view(
        &self,
        state: &QuickJumpState,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let highlight = state.core.highlight();
        let here = self.host_name(&HostKey::Local);
        let rows: Vec<AnyElement> = state
            .core
            .filtered()
            .enumerate()
            .map(|(at, entry)| {
                let elsewhere = (entry.secondary != here).then(|| entry.secondary.clone());
                quick_jump_row(at, entry, highlight == Some(at), elsewhere, cx)
            })
            .collect();
        // With nothing typed there is nothing to miss: an empty palette
        // draws no row.
        if !rows.is_empty() || state.core.query().trim().is_empty() {
            return rows;
        }
        let t = theme(cx);
        vec![
            labelled_text("quick-jump-no-match".to_owned(), NO_MATCH.to_owned())
                .px(px(8.0))
                .py(px(4.0))
                .text_color(gpui::rgb(t.muted))
                .opacity(DISABLED_OPACITY)
                .into_any_element(),
        ]
    }
}

/// Row `at`: the dimmed kind tag, the name, and `elsewhere`, the host of an
/// entry not on This PC, dimmed after a dot; a press jumps to it.
fn quick_jump_row(
    at: usize,
    entry: &Entry,
    highlighted: bool,
    elsewhere: Option<String>,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = theme(cx);
    let name = format!("quick-jump-row-{at}");
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .driver_id(move || name)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.0))
        .h(ui_px(cx, ROW_HEIGHT))
        .px(px(8.0))
        .rounded(px(SEGMENT_RADIUS))
        .whitespace_nowrap()
        .overflow_hidden()
        .cursor_pointer()
        .when(highlighted, move |row| row.bg(gpui::rgb(t.popover_hover)))
        .child(
            div()
                .flex_none()
                .text_color(gpui::rgb(t.muted))
                .child(entry.kind.tag()),
        )
        .child(
            div()
                .min_w(px(0.0))
                .overflow_hidden()
                .text_ellipsis()
                .child(entry.primary.clone()),
        )
        .when_some(elsewhere, |row, host| {
            row.child(
                div()
                    .flex_none()
                    .text_color(gpui::rgb(t.muted))
                    .child(format!("· {host}")),
            )
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
                this.press_quick_jump_row(at, window, cx);
            }),
        )
        .into_any_element()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test fails on a bad fixture")]
mod tests {
    use crate::is_quick_jump_key;
    use gpui::Keystroke;

    fn key(text: &str) -> Keystroke {
        Keystroke::parse(text).expect("a keystroke")
    }

    #[test]
    fn ctrl_shift_p_opens_the_palette() {
        assert!(is_quick_jump_key(&key("ctrl-shift-p")));
    }

    #[test]
    fn ctrl_p_alone_does_not() {
        assert!(!is_quick_jump_key(&key("ctrl-p")));
        assert!(!is_quick_jump_key(&key("shift-p")));
        assert!(!is_quick_jump_key(&key("ctrl-shift-o")));
    }

    #[test]
    fn ctrl_alt_shift_p_does_not() {
        assert!(!is_quick_jump_key(&key("ctrl-alt-shift-p")));
        assert!(!is_quick_jump_key(&key("ctrl-cmd-shift-p")));
    }

    #[test]
    fn cmd_shift_p_counts_only_on_macos() {
        assert_eq!(
            is_quick_jump_key(&key("cmd-shift-p")),
            cfg!(target_os = "macos")
        );
        assert!(!is_quick_jump_key(&key("cmd-alt-shift-p")));
    }
}
