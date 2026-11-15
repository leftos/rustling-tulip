//! Draws the phone's session page ([`super::session`]): a header with
//! Back, the session's label and status word and, at its right end, the
//! Terminal toggle, over the Chat or Terminal page's body. The page's root
//! takes the back gesture. It only renders and forwards taps.

use gpui::{
    App, BackGestureEvent, ClickEvent, Context, Div, FontWeight, Window, div, prelude::*, px,
};

use super::PhoneRoot;
use super::composer::{self, Dock};
use super::home::{self, Tone};
use super::home_view::{SIDE_PADDING, STATUS_TEXT_SIZE, TOUCH_TARGET, tone_color};
use super::hosts_view::phone_button;
use super::session::{self, BackStep, Header, Page, SessionPage};
use crate::buttons::{ButtonKind, ButtonSize, Layer, button, button_look, field_frame};
use crate::chat_view::chat_available;
use crate::driver_id::DriverId as _;
use crate::fonts::ui_px;
use crate::target::HostKey;
use crate::ui_theme::{UiTheme, theme};

/// The session page's Back button.
const BACK_LABEL: &str = "Back";
/// What the Terminal page says until it holds the terminal.
const TERMINAL_PENDING_TEXT: &str = "The terminal opens here in a later build.";

/// The composer's Send button's label.
const SEND_LABEL: &str = "Send";
/// The composer's Paste button's label.
const PASTE_LABEL: &str = "Paste";
/// The Quick dock's button's label.
const QUICK_LABEL: &str = "Quick";
/// The Keys tray's button's label.
const KEYS_LABEL: &str = "Keys";
/// The room between the composer's rows, and between its buttons.
const COMPOSER_GAP: f32 = 8.0;

impl PhoneRoot {
    /// Whether `host`'s session `session_id` mirrors its chat now.
    pub(super) fn chat_mirrored(&self, host: &HostKey, session_id: &str, cx: &App) -> bool {
        let sidebar = &self.root.read(cx).sidebar;
        sidebar
            .session_on(host, session_id)
            .is_some_and(|session| chat_available(session, sidebar.agents(host)))
    }

    /// The shown session's header, while a session is shown.
    fn session_header(&self, cx: &App) -> Option<Header> {
        let (host, session_id) = self.shown.as_ref()?;
        let ended = self
            .root
            .read(cx)
            .sidebar
            .session_on(host, session_id)
            .is_some_and(home::ended);
        let mirrored = self.chat_mirrored(host, session_id, cx);
        let home = self.home(cx);
        Some(Header::new(
            session_id,
            home.row_of(host, session_id),
            ended,
            mirrored,
        ))
    }

    /// The page the shown session draws while its chat is mirrored or not
    /// ([`SessionPage::shown`]).
    fn shown_page(&self, chat_mirrored: bool) -> Page {
        self.page
            .unwrap_or_else(|| SessionPage::opened(chat_mirrored))
            .shown(chat_mirrored)
    }

    /// Shows the session Home lists as `session_id`, as a tap on its row
    /// does; returns whether Home lists it.
    pub fn open_session_on(&mut self, session_id: &str, cx: &mut Context<Self>) -> bool {
        let host = self
            .home(cx)
            .rows()
            .find(|row| row.session_id == session_id)
            .map(|row| row.host.clone());
        let Some(host) = host else {
            return false;
        };
        self.open_session(host, session_id, cx);
        true
    }

    /// The shown session's label as its header draws it.
    #[must_use]
    pub fn session_label(&self, cx: &App) -> Option<String> {
        self.session_header(cx).map(|header| header.label)
    }

    /// The shown session's status word as its header draws it.
    #[must_use]
    pub fn session_status(&self, cx: &App) -> Option<&'static str> {
        self.session_header(cx)?.status.map(|(word, _)| word)
    }

    /// The Terminal toggle's label, while it shows.
    #[must_use]
    pub fn session_toggle(&self, cx: &App) -> Option<&'static str> {
        let header = self.session_header(cx)?;
        header
            .toggle
            .then(|| self.shown_page(header.chat_mirrored).toggle_label())
    }

    /// One back step on the session page: the composer's open dock closes
    /// first, then the page steps back, from the Terminal page to the Chat
    /// page while the toggle shows, else back to Home.
    pub(super) fn session_back(&mut self, cx: &mut Context<Self>) {
        let (mirrored, toggle) = self.session_header(cx).map_or((false, false), |header| {
            (header.chat_mirrored, header.toggle)
        });
        let drawn = self
            .shown
            .as_ref()
            .is_some_and(|(_, id)| self.dock_drawn(id, self.shown_page(mirrored), cx));
        let step = match self.page.as_mut() {
            Some(page) => session::back_step(drawn, page, mirrored, toggle),
            None => BackStep::Close,
        };
        match step {
            BackStep::CloseDock => {
                self.composer_dock = Dock::None;
                cx.notify();
            }
            BackStep::ToChat => cx.notify(),
            BackStep::Close => self.close_session(cx),
        }
    }

    /// A back gesture over the window while a session is shown: a back step
    /// once it commits.
    pub(super) fn session_back_gesture(
        &mut self,
        event: &BackGestureEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if session::back_on(event.phase) {
            self.session_back(cx);
        }
    }

    /// The toggle's tap: the other page, with the composer's dock closed
    /// since the page it draws on leaves the screen.
    fn toggle_page(&mut self, cx: &mut Context<Self>) {
        if let Some(page) = self.page.as_mut() {
            page.toggle();
            self.composer_dock = Dock::None;
            cx.notify();
        }
    }

    /// While a session is shown: its page, the header over the body of the
    /// page it is on.
    pub(super) fn session_page(&self, cx: &mut Context<Self>) -> Option<Div> {
        let header = self.session_header(cx)?;
        let page = self.shown_page(header.chat_mirrored);
        let t = theme(cx);
        let body = match page {
            Page::Chat => {
                let id = self.shown.as_ref().map(|(_, id)| id.clone())?;
                let chat = self
                    .root
                    .update(cx, |root, cx| root.phone_chat_body(&id, cx));
                let composer = self.composer(&id, cx);
                div()
                    .driver_id(|| "phone-session-chat".to_owned())
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(chat)
                    .children(composer)
            }
            Page::Terminal => terminal_body(&t, cx),
        };
        Some(
            div()
                .driver_id(|| "phone-session".to_owned())
                .size_full()
                .flex()
                .flex_col()
                .child(Self::session_header_row(&header, page, &t, cx))
                .child(body),
        )
    }

    /// The shown session's composer under its chat: the Quick or Keys dock
    /// while one is open, the note saying why it cannot send, the field with
    /// Send, and the Paste, Quick and Keys buttons under them. None before
    /// the chat has loaded.
    fn composer(&self, session_id: &str, cx: &mut Context<Self>) -> Option<Div> {
        let (input, note, enabled, error) = {
            let root = self.root.read(cx);
            (
                root.phone_composer_input(session_id)?,
                root.chat_composer_note(session_id),
                root.phone_composer_send_enabled(session_id, cx),
                root.chat_composer_error(session_id),
            )
        };
        let dock = if self.dock_drawn(session_id, Page::Chat, cx) {
            self.dock(session_id, cx)
        } else {
            None
        };
        let t = theme(cx);
        let id = session_id.to_owned();
        let field = field_frame(false, Layer::Base, &t)
            .driver_id(|| "phone-composer-field".to_owned())
            .flex_1()
            .min_w(px(0.0))
            .py(px(6.0))
            .child(input);
        let send = button(
            "phone-composer-send",
            button_look(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                ButtonKind::Primary,
                ButtonSize::Large,
                enabled,
                Layer::Base,
                &t,
            ),
        )
        .min_h(px(TOUCH_TARGET))
        .flex_none()
        .child(SEND_LABEL)
        .when(enabled, |send| {
            send.on_click(cx.listener({
                let id = id.clone();
                move |this, _: &ClickEvent, _, cx| {
                    this.root.update(cx, |root, cx| root.send_composer(&id, cx));
                }
            }))
        });
        Some(
            div()
                .driver_id(|| "phone-composer".to_owned())
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(COMPOSER_GAP))
                .px(px(SIDE_PADDING))
                .py(px(COMPOSER_GAP))
                .border_t_1()
                .border_color(gpui::rgb(t.line))
                .children(dock)
                .children(note.map(|note| {
                    div()
                        .driver_id(|| "phone-composer-note".to_owned())
                        .text_size(ui_px(cx, STATUS_TEXT_SIZE))
                        .text_color(gpui::rgb(t.muted))
                        .child(note)
                }))
                .child(
                    div()
                        .flex()
                        .items_end()
                        .gap(px(COMPOSER_GAP))
                        .child(field)
                        .child(send),
                )
                .children(error.map(|reason| {
                    div()
                        .driver_id(|| "phone-composer-error".to_owned())
                        .text_size(ui_px(cx, STATUS_TEXT_SIZE))
                        .text_color(gpui::rgb(t.danger))
                        .child(reason)
                }))
                .child(Self::composer_buttons(cx)),
        )
    }

    /// Whether the composer's dock draws now: one is open, the shown page is
    /// the Chat page, and the session's chat has a composer to draw it over.
    /// Both the draw and the back step ask this, so a dock that is set but
    /// off screen never swallows a Back.
    fn dock_drawn(&self, session_id: &str, page: Page, cx: &App) -> bool {
        self.composer_dock != Dock::None
            && page == Page::Chat
            && self
                .root
                .read(cx)
                .phone_composer_input(session_id)
                .is_some()
    }

    /// The three buttons under the composer's field: Paste, and the Quick
    /// and Keys docks' buttons.
    fn composer_buttons(cx: &mut Context<Self>) -> Div {
        let paste = phone_button("phone-composer-paste", ButtonKind::Outlined, cx)
            .min_h(px(TOUCH_TARGET))
            .flex_1()
            .child(PASTE_LABEL)
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.paste_into_composer(window, cx);
            }));
        let quick = phone_button("phone-composer-quick", ButtonKind::Outlined, cx)
            .min_h(px(TOUCH_TARGET))
            .flex_1()
            .child(QUICK_LABEL)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.tap_composer_quick(cx)));
        let keys = phone_button("phone-composer-keys", ButtonKind::Outlined, cx)
            .min_h(px(TOUCH_TARGET))
            .flex_1()
            .child(KEYS_LABEL)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.tap_composer_keys(cx)));
        div()
            .flex()
            .gap(px(COMPOSER_GAP))
            .child(paste)
            .child(quick)
            .child(keys)
    }

    /// Paste's tap: the clipboard's text goes into the composer's field at
    /// its caret. A clipboard with no text changes nothing.
    fn paste_into_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, session_id)) = self.shown.as_ref() else {
            return;
        };
        let session_id = session_id.clone();
        let Some(input) = self.root.read(cx).phone_composer_input(&session_id) else {
            return;
        };
        input.update(cx, |input, cx| input.paste_clipboard(window, cx));
    }

    /// The composer's open dock above its row, while one is.
    fn dock(&self, session_id: &str, cx: &mut Context<Self>) -> Option<Div> {
        let t = theme(cx);
        match self.composer_dock {
            Dock::None => None,
            Dock::Quick => Some(self.quick_dock(session_id, &t, cx)),
            Dock::Keys => Some(Self::keys_dock(session_id, cx)),
        }
    }

    /// The Quick dock: each of the shown session's reply groups under its
    /// title, every reply a tap that types it and Return.
    fn quick_dock(&self, session_id: &str, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let id = session_id.to_owned();
        let mut dock = div()
            .driver_id(|| "phone-dock-quick".to_owned())
            .flex()
            .flex_col()
            .gap(px(COMPOSER_GAP));
        for group in composer::quick_groups(self.shown_has_agent(cx)) {
            let mut row = div().flex().flex_wrap().gap(px(COMPOSER_GAP));
            for (index, text) in group.items.iter().enumerate() {
                let bytes = composer::quick_bytes(text);
                row = row.child(
                    phone_button(
                        &format!("phone-quick-{}-{index}", group.title),
                        ButtonKind::Outlined,
                        cx,
                    )
                    .min_h(px(TOUCH_TARGET))
                    .child(*text)
                    .on_click(cx.listener({
                        let id = id.clone();
                        move |this, _: &ClickEvent, _, cx| {
                            this.root
                                .update(cx, |root, _| root.send_phone_input(&id, &bytes));
                        }
                    })),
                );
            }
            dock = dock.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(ui_px(cx, STATUS_TEXT_SIZE))
                            .text_color(gpui::rgb(t.muted))
                            .child(group.title),
                    )
                    .child(row),
            );
        }
        dock
    }

    /// The Keys tray: every key of the strip, each a tap that types its
    /// bytes.
    fn keys_dock(session_id: &str, cx: &mut Context<Self>) -> Div {
        let id = session_id.to_owned();
        let mut dock = div()
            .driver_id(|| "phone-dock-keys".to_owned())
            .flex()
            .flex_wrap()
            .gap(px(COMPOSER_GAP));
        for (index, key) in composer::KEY_TRAY.iter().enumerate() {
            let bytes = key.bytes;
            dock = dock.child(
                phone_button(&format!("phone-key-{index}"), ButtonKind::Outlined, cx)
                    .min_h(px(TOUCH_TARGET))
                    .child(key.label)
                    .on_click(cx.listener({
                        let id = id.clone();
                        move |this, _: &ClickEvent, _, cx| {
                            this.root
                                .update(cx, |root, _| root.send_phone_input(&id, bytes));
                        }
                    })),
            );
        }
        dock
    }

    /// The header: Back, the label over the status word, and the toggle
    /// while it shows.
    fn session_header_row(header: &Header, page: Page, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let back = phone_button("phone-session-back", ButtonKind::Outlined, cx)
            .min_h(px(TOUCH_TARGET))
            .flex_none()
            .child(BACK_LABEL)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.session_back(cx)));
        let toggle = header.toggle.then(|| {
            phone_button("phone-session-toggle", ButtonKind::Outlined, cx)
                .min_h(px(TOUCH_TARGET))
                .flex_none()
                .child(page.toggle_label())
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_page(cx)))
        });
        div()
            .flex()
            .items_center()
            .gap(px(12.0))
            .min_h(px(TOUCH_TARGET))
            .px(px(SIDE_PADDING))
            .py(px(6.0))
            .border_b_1()
            .border_color(gpui::rgb(t.line))
            .child(back)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .driver_id(|| "phone-session-label".to_owned())
                            .truncate()
                            .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                            .child(header.label.clone()),
                    )
                    .children(
                        header
                            .status
                            .map(|(word, tone)| status_line(word, tone, t, cx)),
                    ),
            )
            .children(toggle)
    }
}

/// The status word in its tone's colour, as Home's row draws it.
fn status_line(word: &'static str, tone: Tone, t: &UiTheme, cx: &App) -> Div {
    div()
        .driver_id(|| "phone-session-status".to_owned())
        .text_size(ui_px(cx, STATUS_TEXT_SIZE))
        .text_color(gpui::rgb(tone_color(tone, t)))
        .child(word)
}

/// The Terminal page's body: one muted line until it holds the terminal.
fn terminal_body(t: &UiTheme, cx: &App) -> Div {
    div()
        .driver_id(|| "phone-session-terminal".to_owned())
        .flex_1()
        .min_h_0()
        .px(px(SIDE_PADDING))
        .py(px(12.0))
        .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
        .text_color(gpui::rgb(t.muted))
        .child(TERMINAL_PENDING_TEXT)
}
