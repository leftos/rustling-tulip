//! Draws the phone's Terminal page: the shown session's live terminal
//! filling the page, its Quick replies while they are open, and the key
//! bar under them. The terminal opens while the page shows and closes once
//! it leaves, through one sync each frame. The page only renders and
//! forwards taps; the bar's logic is [`super::key_bar`]'s.

use std::rc::Rc;

use gpui::{
    App, ClickEvent, ClipboardItem, Context, DispatchPhase, Div, Entity, HitboxBehavior,
    ScrollEvent, Window, canvas, div, prelude::*, px,
};

use super::PhoneRoot;
use super::home_view::{SIDE_PADDING, TOUCH_TARGET};
use super::hosts_view::phone_button;
use super::key_bar::KeyBar;
use super::key_grid::{self, KeyAction};
use super::session::Page;
use super::session_view::{KeySurface, QuickTarget};
use super::terminal_fit::{self, Pan};
use crate::RootView;
use crate::buttons::ButtonKind;
use crate::driver_id::DriverId as _;
use crate::target::HostKey;
use crate::term_view::{TerminalPane, TypedFilter};
use crate::ui_theme::theme;

/// The room between the bar's keys, and around the bar.
const BAR_GAP: f32 = 4.0;

/// The horizontal padding of each of the bar's keys.
const BAR_KEY_PAD: f32 = 6.0;

/// The widest a bar key draws, so a long label truncates rather than runs on.
const BAR_KEY_MAX: f32 = 96.0;

/// The Hide key's label, pinned at the bar's right end.
const HIDE_LABEL: &str = "Hide";

/// The points one line of a line-counted scroll pans the terminal by.
const PAN_LINE: f32 = 20.0;

impl PhoneRoot {
    /// Opens the shown session's terminal while its Terminal page shows and
    /// closes it otherwise; a terminal newly opened gets a fresh key bar,
    /// the keyboard's focus and the bar's sticky Ctrl for typed text. While
    /// it is open, the terminal records whether the keyboard is up, which
    /// decides whether a tap puts it away, and keeps `hidden`, the height
    /// the keyboard covers, for the page to draw it at its keyboard-down
    /// height; the keyboard never re-sizes it. The pan over the hidden rows
    /// starts over when the page opens or closes and while the keyboard is
    /// down.
    pub(super) fn sync_terminal(
        &mut self,
        keyboard_up: bool,
        hidden: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((host, session_id)) = self.wanted_terminal(cx) else {
            self.terminal_hidden = 0.0;
            self.terminal_pan = Pan::default();
            self.terminal_watch = None;
            // Only the terminal this sync opened is closed here.
            if self.terminal_open.take().is_some() {
                self.root.update(cx, RootView::close_phone_terminal);
            }
            return;
        };
        self.root.update(cx, |root, cx| {
            root.open_phone_terminal(&host, &session_id, window, cx);
        });
        if let Some(pane) = self.terminal_pane(cx) {
            pane.update(cx, |pane, _| pane.set_phone_keyboard_up(keyboard_up));
        }
        let wanted = (host, session_id);
        if self.terminal_open.as_ref() != Some(&wanted) {
            tracing::debug!(
                session_id = wanted.1.as_str(),
                "phone: the terminal page opened"
            );
            self.terminal_open = Some(wanted);
            self.terminal_pan = Pan::default();
            *self.key_bar.borrow_mut() = KeyBar::new();
            self.terminal_quick = false;
            if let Some(pane) = self.terminal_pane(cx) {
                pane.read(cx).focus_handle().focus(window, cx);
                let filter = self.typed_ctrl(cx);
                pane.update(cx, |pane, _| pane.set_typed_filter(Some(filter)));
                self.terminal_watch = Some(cx.observe(&pane, |this, _, cx| this.follow_cursor(cx)));
            }
        }
        self.terminal_hidden = hidden;
        self.terminal_pan = if keyboard_up {
            self.terminal_pan.clamped(hidden)
        } else {
            Pan::default()
        };
        self.terminal_need = self.cursor_need(cx);
    }

    /// After the terminal redraws: the page redraws too when the cursor's
    /// row moved what the pan must show, so the row stays in sight.
    fn follow_cursor(&mut self, cx: &mut Context<Self>) {
        if self.terminal_hidden <= 0.0 {
            return;
        }
        let need = self.cursor_need(cx);
        if (need - self.terminal_need).abs() > f32::EPSILON {
            self.terminal_need = need;
            cx.notify();
        }
    }

    /// The session whose terminal shows now: the shown one, while its shown
    /// page is the Terminal page.
    fn wanted_terminal(&self, cx: &App) -> Option<(HostKey, String)> {
        let header = self.session_header(cx)?;
        if self.shown_page(header.chat_mirrored) != Page::Terminal {
            return None;
        }
        self.shown.clone()
    }

    /// The phone's open terminal, if any.
    pub(super) fn terminal_pane(&self, cx: &App) -> Option<Entity<TerminalPane>> {
        self.root.read(cx).phone_terminal_view()
    }

    /// The filter that turns a character typed on the soft keyboard into
    /// its Ctrl keystroke while the bar's Ctrl is armed, and redraws the bar
    /// when it does.
    fn typed_ctrl(&self, cx: &mut Context<Self>) -> TypedFilter {
        let key_bar = Rc::clone(&self.key_bar);
        let phone = cx.weak_entity();
        Box::new(move |text, cx| {
            let keystroke = key_bar.borrow_mut().typed(text)?;
            if phone.update(cx, |_, cx| cx.notify()).is_err() {
                tracing::debug!("phone: a typed Ctrl after the phone's root was dropped");
            }
            Some(keystroke)
        })
    }

    /// The Terminal page's body: the terminal filling it, the Quick replies
    /// while they are open, and the key bar. The terminal sits in a clip
    /// box at its keyboard-down height, so a raised keyboard hides its top
    /// rows rather than shrinking it, and the pan picks the rows shown.
    pub(super) fn terminal_page(
        &self,
        session_id: &str,
        keyboard_up: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let hidden = self.terminal_hidden;
        let (top, bottom) = self
            .terminal_pan
            .effective(hidden, self.cursor_need(cx))
            .edges(hidden);
        let pane = div()
            .absolute()
            .left_0()
            .right_0()
            .top(px(top))
            .bottom(px(bottom))
            .flex()
            .flex_col()
            .children(self.terminal_pane(cx));
        let screen = div()
            .driver_id(|| "phone-session-terminal".to_owned())
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .child(pane)
            .child(Self::terminal_pan_layer(cx));
        let t = theme(cx);
        let quick = self.terminal_quick.then(|| {
            div()
                .flex_none()
                .px(px(SIDE_PADDING))
                .py(px(BAR_GAP))
                .border_t_1()
                .border_color(gpui::rgb(t.line))
                .child(self.quick_dock(session_id, QuickTarget::Terminal, &t, cx))
        });
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(screen)
            .children(quick)
            .child(self.key_bar_row(keyboard_up, cx))
    }

    /// An empty layer over the terminal's clip box that, before the
    /// terminal sees a vertical drag over the box, pans the terminal while
    /// the drag can move the pan; at either bound the drag goes on to the
    /// terminal's own scroll. It acts only where a hit test reaches it, so
    /// a layer over it (the prompt sheet, its scrim) takes its own drags. A
    /// pan sends nothing to the session.
    fn terminal_pan_layer(cx: &Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        canvas(
            |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
            move |_, hitbox, window, _| {
                window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
                    if phase != DispatchPhase::Capture || !hitbox.should_handle_scroll(window) {
                        return;
                    }
                    let delta_y = f32::from(event.delta.pixel_delta(px(PAN_LINE)).y);
                    let consumed = this
                        .update(cx, |this, cx| {
                            let need = this.cursor_need(cx);
                            let (pan, consumed) =
                                this.terminal_pan
                                    .drag_shown(delta_y, this.terminal_hidden, need);
                            this.terminal_pan = pan;
                            if consumed {
                                cx.notify();
                            }
                            // A pan's still start and lift move nothing, and
                            // reaching the terminal they would count as input
                            // and claim its size.
                            let still = delta_y.abs() <= f32::EPSILON;
                            consumed || (still && this.terminal_hidden > 0.0)
                        })
                        .unwrap_or(false);
                    if consumed {
                        cx.stop_propagation();
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    /// The least pan that keeps the open terminal's cursor row in sight
    /// above the raised keyboard; none without a laid-out terminal.
    fn cursor_need(&self, cx: &App) -> f32 {
        if self.terminal_hidden <= 0.0 {
            return 0.0;
        }
        let row_top = self
            .terminal_pane(cx)
            .and_then(|pane| pane.read(cx).cursor_row_top());
        row_top.map_or(0.0, |top| {
            terminal_fit::cursor_need(self.terminal_hidden, top)
        })
    }

    /// The key bar: one sideways-scrolling row of the grid's keys in order,
    /// Ctrl lit while it is armed, then Hide pinned at its end while the
    /// on-screen keyboard is up.
    fn key_bar_row(&self, keyboard_up: bool, cx: &mut Context<Self>) -> Div {
        let armed = self.key_bar.borrow().armed();
        let mut scroll = div()
            .id("phone-key-bar-scroll")
            .driver_id(|| "phone-key-bar-scroll".to_owned())
            .flex()
            .gap(px(BAR_GAP))
            .flex_1()
            .min_w_0()
            .overflow_x_scroll();
        let saved = self.root.read(cx).sidebar.ui_state().phone_keys.clone();
        for (index, key) in key_grid::resolved(saved.as_deref()).iter().enumerate() {
            let kind = if key.action == KeyAction::Ctrl && armed {
                ButtonKind::Primary
            } else {
                ButtonKind::Outlined
            };
            let dimmed = key_grid::is_inert(key);
            scroll = scroll.child(
                phone_button(&format!("phone-bar-{}", key.id), kind, cx)
                    .flex_none()
                    .min_w(px(TOUCH_TARGET))
                    .max_w(px(BAR_KEY_MAX))
                    .min_h(px(TOUCH_TARGET))
                    .px(px(BAR_KEY_PAD))
                    .truncate()
                    .child(key.label.clone())
                    .when(dimmed, |button| {
                        button.opacity(crate::spawn_view::DISABLED_OPACITY)
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.press_grid_key(index, KeySurface::Terminal, window, cx);
                    })),
            );
        }
        let hide = keyboard_up.then(|| {
            phone_button("phone-bar-hide", ButtonKind::Outlined, cx)
                .flex_none()
                .min_h(px(TOUCH_TARGET))
                .px(px(BAR_KEY_PAD))
                .child(HIDE_LABEL)
                .on_click(cx.listener(|_, _: &ClickEvent, window, cx| {
                    crate::keyboard::lower(window);
                    cx.notify();
                }))
        });
        div()
            .driver_id(|| "phone-key-bar".to_owned())
            .flex_none()
            .flex()
            .gap(px(BAR_GAP))
            .px(px(SIDE_PADDING))
            .py(px(BAR_GAP))
            .child(scroll)
            .children(hide)
    }

    /// The session the open terminal shows, as this page opened it.
    pub(super) fn terminal_session(&self) -> Option<String> {
        self.terminal_open.as_ref().map(|(_, id)| id.clone())
    }

    /// Paste's tap: the clipboard's text pasted into `pane`, bracketed when
    /// the program asked; a clipboard with no text sends nothing.
    pub(super) fn paste_into_terminal(
        pane: Option<Entity<TerminalPane>>,
        session_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let (Some(pane), Some(session_id)) = (pane, session_id) else {
            return;
        };
        let text = cx
            .read_from_clipboard()
            .as_ref()
            .and_then(ClipboardItem::text)
            .filter(|text| !text.is_empty());
        let Some(text) = text else {
            tracing::debug!("phone: Paste with no text on the clipboard");
            return;
        };
        pane.update(cx, |pane, cx| pane.paste_text(&text, &session_id, cx));
    }

    /// A Quick reply's tap on the Terminal page: `bytes` typed through the
    /// terminal, and the replies close.
    pub(super) fn quick_into_terminal(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if let Some(pane) = self.terminal_pane(cx) {
            pane.update(cx, |pane, cx| pane.type_bytes(bytes, cx));
        }
        self.terminal_quick = false;
        cx.notify();
    }
}
