//! Draws the phone's session page ([`super::session`]): a header with
//! Back, the session's label and status word and, at its right end, the
//! Terminal toggle, over the Chat or Terminal page's body. The page's root
//! takes the back gesture. It only renders and forwards taps.

use gpui::{
    App, BackGestureEvent, ClickEvent, Context, Div, FontWeight, Window, div, prelude::*, px,
};

use super::PhoneRoot;
use super::home::{self, Tone};
use super::home_view::{SIDE_PADDING, STATUS_TEXT_SIZE, TOUCH_TARGET, tone_color};
use super::hosts_view::phone_button;
use super::session::{self, BackStep, Header, Page, SessionPage};
use crate::buttons::ButtonKind;
use crate::chat_view::chat_available;
use crate::driver_id::DriverId as _;
use crate::fonts::ui_px;
use crate::target::HostKey;
use crate::ui_theme::{UiTheme, theme};

/// The session page's Back button.
const BACK_LABEL: &str = "Back";
/// What the Terminal page says until it holds the terminal.
const TERMINAL_PENDING_TEXT: &str = "The terminal opens here in a later build.";

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

    /// One back step on the session page: to the Chat page from the
    /// Terminal page while the toggle shows, else back to Home.
    pub(super) fn session_back(&mut self, cx: &mut Context<Self>) {
        let (mirrored, toggle) = self.session_header(cx).map_or((false, false), |header| {
            (header.chat_mirrored, header.toggle)
        });
        let step = self
            .page
            .as_mut()
            .map_or(BackStep::Close, |page| page.back(mirrored, toggle));
        match step {
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

    /// The toggle's tap: the other page.
    fn toggle_page(&mut self, cx: &mut Context<Self>) {
        if let Some(page) = self.page.as_mut() {
            page.toggle();
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
            Page::Chat => div()
                .driver_id(|| "phone-session-chat".to_owned())
                .flex_1()
                .min_h_0(),
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
