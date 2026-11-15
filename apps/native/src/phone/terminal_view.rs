//! Draws the phone's Terminal page: the shown session's live terminal
//! filling the page, its Quick replies while they are open, and the key
//! bar under them. The terminal opens while the page shows and closes once
//! it leaves, through one sync each frame. The page only renders and
//! forwards taps; the bar's logic is [`super::key_bar`]'s.

use std::rc::Rc;

use gpui::{
    App, ClickEvent, ClipboardItem, Context, Div, Entity, Pixels, Window, div, prelude::*, px,
};

use super::PhoneRoot;
use super::home_view::{SIDE_PADDING, TOUCH_TARGET};
use super::hosts_view::phone_button;
use super::key_bar::{self, Action, Key, KeyBar};
use super::session::Page;
use super::session_view::QuickTarget;
use crate::RootView;
use crate::buttons::ButtonKind;
use crate::driver_id::DriverId as _;
use crate::target::HostKey;
use crate::term_view::{TerminalPane, TypedFilter};
use crate::ui_theme::theme;

/// The room between the bar's keys, and around the bar.
const BAR_GAP: f32 = 4.0;

impl PhoneRoot {
    /// Opens the shown session's terminal while its Terminal page shows and
    /// closes it otherwise; a terminal newly opened gets a fresh key bar,
    /// the keyboard's focus and the bar's sticky Ctrl for typed text. While
    /// it is open, a changed bottom inset (the keyboard rose or fell) has
    /// its next size claim the PTY.
    pub(super) fn sync_terminal(
        &mut self,
        inset_bottom: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((host, session_id)) = self.wanted_terminal(cx) else {
            // Only the terminal this sync opened is closed here.
            if self.terminal_open.take().is_some() {
                self.terminal_inset = None;
                self.root.update(cx, RootView::close_phone_terminal);
            }
            return;
        };
        self.root.update(cx, |root, cx| {
            root.open_phone_terminal(&host, &session_id, window, cx);
        });
        let wanted = (host, session_id);
        if self.terminal_open.as_ref() != Some(&wanted) {
            tracing::debug!(
                session_id = wanted.1.as_str(),
                "phone: the terminal page opened"
            );
            self.terminal_open = Some(wanted);
            self.terminal_inset = None;
            *self.key_bar.borrow_mut() = KeyBar::new();
            self.terminal_quick = false;
            if let Some(pane) = self.terminal_pane(cx) {
                pane.read(cx).focus_handle().focus(window, cx);
                let filter = self.typed_ctrl(cx);
                pane.update(cx, |pane, _| pane.set_typed_filter(Some(filter)));
            }
        }
        if self
            .terminal_inset
            .is_some_and(|stored| stored != inset_bottom)
        {
            self.root
                .update(cx, |root, cx| root.phone_terminal_resized(cx));
        }
        self.terminal_inset = Some(inset_bottom);
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
    fn terminal_pane(&self, cx: &App) -> Option<Entity<TerminalPane>> {
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
    /// while they are open, and the key bar.
    pub(super) fn terminal_page(&self, session_id: &str, cx: &mut Context<Self>) -> Div {
        let screen = div()
            .driver_id(|| "phone-session-terminal".to_owned())
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .children(self.terminal_pane(cx));
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
            .child(self.key_bar_row(cx))
    }

    /// The key bar: one row of the bar's keys in order, Ctrl lit while it
    /// is armed.
    fn key_bar_row(&self, cx: &mut Context<Self>) -> Div {
        let armed = self.key_bar.borrow().armed();
        let mut bar = div()
            .driver_id(|| "phone-key-bar".to_owned())
            .flex_none()
            .flex()
            .gap(px(BAR_GAP))
            .px(px(SIDE_PADDING))
            .py(px(BAR_GAP));
        for &key in key_bar::KEYS {
            let kind = if key == Key::Ctrl && armed {
                ButtonKind::Primary
            } else {
                ButtonKind::Outlined
            };
            bar = bar.child(
                phone_button(&format!("phone-bar-{}", key.id()), kind, cx)
                    .flex_1()
                    .min_w_0()
                    .min_h(px(TOUCH_TARGET))
                    .child(key.label())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.tap_bar(key, cx);
                    })),
            );
        }
        bar
    }

    /// A tap on the bar's `key`: its keystroke through the terminal, the
    /// clipboard's text pasted, the Quick replies opened or closed, or
    /// Ctrl armed or disarmed.
    fn tap_bar(&mut self, key: Key, cx: &mut Context<Self>) {
        let action = self.key_bar.borrow_mut().press(key);
        let pane = self.terminal_pane(cx);
        match action {
            Action::Key(keystroke) => {
                if let Some(pane) = pane {
                    pane.update(cx, |pane, cx| pane.press(&keystroke, cx));
                }
            }
            Action::Paste => Self::paste_into_terminal(pane, self.terminal_session(), cx),
            Action::Quick => self.terminal_quick = !self.terminal_quick,
            Action::None => {}
        }
        cx.notify();
    }

    /// The session the open terminal shows, as this page opened it.
    fn terminal_session(&self) -> Option<String> {
        self.terminal_open.as_ref().map(|(_, id)| id.clone())
    }

    /// Paste's tap: the clipboard's text pasted into `pane`, bracketed when
    /// the program asked; a clipboard with no text sends nothing.
    fn paste_into_terminal(
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
