//! Draws the phone's Home ([`super::home`]) and the page a row's tap
//! opens: rows and targets at least 44 pt tall, no shortcut hints and
//! nothing that shows only on hover. It only renders and forwards taps.

use chrono::{DateTime, Utc};
use gpui::{
    App, ClickEvent, Context, Div, FontWeight, ScrollEvent, SharedString, Stateful, StyledText,
    TouchPhase, Window, div, prelude::*, px,
};

use super::PhoneRoot;
use super::home::{HomeGroup, HomeHost, HomeRow, Tone};
use super::hosts_view::phone_button;
use super::tray::{self, Arranged, Buttons, TrayCard};
use crate::buttons::ButtonKind;
use crate::connection::Connection;
use crate::driver_id::DriverId as _;
use crate::ellipsis::truncating;
use crate::fonts::ui_px;
use crate::needs_you;
use crate::sidebar::DETACHED_BANNER;
use crate::sidebar_view::{HostHeaderState, leaf_key};
use crate::target::HostKey;
use crate::ui_theme::{UiTheme, theme};

/// The least height of a row or of anything a finger taps.
const TOUCH_TARGET: f32 = 44.0;
/// The status word's and the banner's text size: the sidebar's subline.
const STATUS_TEXT_SIZE: f32 = 11.0;
/// The mark's dot.
const DOT: f32 = 8.0;
/// The space either side of a row's content.
const SIDE_PADDING: f32 = 16.0;
/// The session page's Back button.
const BACK_LABEL: &str = "Back";
/// A not-paired host's header button.
const PAIR_LABEL: &str = "Pair";
/// What Home says with no session to list.
const EMPTY_TEXT: &str = "No sessions yet";
/// The Needs you card's height, whatever it holds, so the rows under the
/// tray keep their place while cards come, go and change.
const TRAY_CARD_HEIGHT: f32 = 232.0;
/// The button of a card whose prompt is answered on the session's page.
const OPEN_LABEL: &str = "Open";

/// What a card says about its prompt, read before the card is drawn.
struct CardFacts {
    card: TrayCard,
    now: DateTime<Utc>,
    refusal: Option<String>,
}

/// One line of `text`, cut with an ellipsis, tagged `id`.
fn line(id: &str, text: String) -> Div {
    let tag = id.to_owned();
    truncating(id, StyledText::new(text)).driver_id(move || tag)
}

/// The card's text: the session, the prompt, its command, how long it has
/// waited and, after a refused answer, why.
fn card_text(facts: &CardFacts, t: &UiTheme, cx: &Context<PhoneRoot>) -> Div {
    let card = &facts.card;
    let small = ui_px(cx, STATUS_TEXT_SIZE);
    let session = div()
        .flex()
        .gap(px(6.0))
        .child(
            line("phone-tray-label", card.label.clone())
                .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD)),
        )
        .child(
            line("phone-tray-container", card.container_name.clone())
                .flex_1()
                .text_color(gpui::rgb(t.muted)),
        );
    let command = card.command.clone().map(|command| {
        line("phone-tray-command", command)
            .text_size(small)
            .text_color(gpui::rgb(t.text_2))
    });
    let age = card.since.map(|since| {
        line("phone-tray-age", needs_you::waited(facts.now, since))
            .text_size(small)
            .text_color(gpui::rgb(t.muted))
    });
    let refusal = facts.refusal.clone().map(|message| {
        div()
            .driver_id(|| "phone-tray-error".to_owned())
            .child(line("phone-tray-error-text", message))
            .text_size(small)
            .text_color(gpui::rgb(t.danger_text))
    });
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .min_w_0()
        .child(session)
        .child(
            div()
                .driver_id(|| "phone-tray-text".to_owned())
                .line_clamp(2)
                .overflow_hidden()
                .child(card.text.clone()),
        )
        .children(command)
        .children(age)
        .children(refusal)
}

/// The header: `Needs you · {i} of {n}`, and a dot per card while there
/// are several, the shown one lit.
fn tray_header(arranged: &Arranged, t: &UiTheme, cx: &Context<PhoneRoot>) -> Div {
    let text = arranged.header().unwrap_or_default();
    let many = arranged.cards.len() > 1;
    let dots = (0..arranged.cards.len()).filter(|_| many).map(|at| {
        dot(if at == arranged.index {
            t.asking
        } else {
            t.muted
        })
        .size(px(6.0))
    });
    div()
        .driver_id(|| "phone-tray-header".to_owned())
        .flex()
        .items_center()
        .gap(px(4.0))
        .text_size(ui_px(cx, STATUS_TEXT_SIZE))
        .text_color(gpui::rgb(t.asking))
        .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
        .child(line("phone-tray-header-text", text).flex_1())
        .children(dots)
}

/// A dot of `color`.
fn dot(color: u32) -> Div {
    div()
        .flex_none()
        .size(px(DOT))
        .rounded_full()
        .bg(gpui::rgb(color))
}

/// The colour a row's dot and status word take for `tone`.
fn tone_color(tone: Tone, t: &UiTheme) -> u32 {
    match tone {
        Tone::NeedsYou => t.asking,
        Tone::Working => t.working,
        Tone::Quiet => t.muted,
    }
}

/// The key a host's header and status are tagged with: the desktop's
/// host header's.
fn host_tag(host: &HostKey) -> &str {
    host.wire().unwrap_or("local")
}

/// `group`'s heading: its name and, while any of its sessions need the
/// owner, an amber wash, a dot and their count.
fn group_heading(group: &HomeGroup, t: &UiTheme, cx: &Context<PhoneRoot>) -> Div {
    let id = format!("phone-home-group-{}", group.key);
    let marked = group.needs_you > 0;
    let mark = marked.then(|| {
        let mark_id = format!("phone-home-needs-{}", group.key);
        div()
            .driver_id(|| mark_id)
            .flex()
            .items_center()
            .gap(px(4.0))
            .text_color(gpui::rgb(t.asking))
            .child(dot(t.asking))
            .child(group.needs_you.to_string())
    });
    div()
        .driver_id(|| id)
        .flex()
        .items_center()
        .gap(px(8.0))
        .min_h(px(TOUCH_TARGET))
        .px(px(SIDE_PADDING))
        .when(marked, |heading| heading.bg(gpui::rgba(t.ask_wash)))
        .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(group.name.clone()),
        )
        .children(mark)
}

/// What a Detached group says above its rows; it offers no Stop all.
fn detached_banner(t: &UiTheme, cx: &Context<PhoneRoot>) -> Div {
    div()
        .driver_id(|| "detached-banner".to_owned())
        .px(px(SIDE_PADDING))
        .py(px(4.0))
        .text_size(ui_px(cx, STATUS_TEXT_SIZE))
        .text_color(gpui::rgb(t.muted))
        .child(DETACHED_BANNER)
}

/// Home's line with nothing to list.
fn empty_line(t: &UiTheme) -> Div {
    div()
        .driver_id(|| "phone-home-empty".to_owned())
        .flex()
        .items_center()
        .min_h(px(TOUCH_TARGET))
        .px(px(SIDE_PADDING))
        .text_color(gpui::rgb(t.muted))
        .child(EMPTY_TEXT)
}

impl PhoneRoot {
    /// Home: each open host's groups, under a header per host while more
    /// than one is open, scrolling within the height it is given.
    pub(super) fn home_list(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let home = self.home(cx);
        let t = theme(cx);
        let mut children: Vec<Div> = self.needs_you_tray(cx).into_iter().collect();
        for host in &home.hosts {
            if home.host_headers {
                let state = self.root.read(cx).host_header_state(&host.host);
                children.push(Self::host_header(host, state, &t, cx));
            }
            for group in &host.groups {
                children.push(Self::home_group(group, &t, cx));
            }
        }
        if home.rows().next().is_none() {
            children.push(empty_line(&t));
        }
        div()
            .id("phone-home")
            .driver_id(|| "phone-home".to_owned())
            .flex_1()
            .min_h_0()
            .w_full()
            .flex()
            .flex_col()
            .overflow_y_scroll()
            // A sideways swipe on the tray's card must not scroll the list.
            .restrict_scroll_to_axis()
            .children(children)
    }

    /// The Needs you tray: the card the tray shows, drawn off its place by
    /// the swipe in progress; `None` while no session needs the owner.
    fn needs_you_tray(&self, cx: &mut Context<Self>) -> Option<Div> {
        let (arranged, facts) = self.tray_now(cx)?;
        let t = theme(cx);
        let card = div()
            .driver_id(|| "phone-tray-card".to_owned())
            .relative()
            .left(px(self.tray.offset()))
            .h(px(TRAY_CARD_HEIGHT))
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .p(px(12.0))
            .rounded(px(10.0))
            .border_1()
            .border_color(gpui::rgb(t.asking))
            .bg(gpui::rgba(t.ask_wash))
            .child(tray_header(&arranged, &t, cx))
            .child(card_text(&facts, &t, cx))
            .child(div().flex_1())
            .child(Self::tray_buttons(&facts.card, cx));
        Some(
            div()
                .driver_id(|| "phone-tray".to_owned())
                .flex_none()
                .px(px(SIDE_PADDING))
                .py(px(8.0))
                .on_scroll(cx.listener(Self::tray_scrolled))
                .child(card),
        )
    }

    /// The Needs you tray's header as Home draws it now, `Needs you · 1 of
    /// 2`; `None` while the tray is hidden.
    #[must_use]
    pub fn tray_header(&self, cx: &App) -> Option<String> {
        self.tray_now(cx)?.0.header()
    }

    /// Why the daemon refused the last answer to the card the tray shows,
    /// while that card's wait holds.
    #[must_use]
    pub fn tray_refusal(&self, cx: &App) -> Option<String> {
        self.tray_now(cx)?.1.refusal
    }

    /// The tray's cards as drawn now and the shown card's facts; `None`
    /// while no session needs the owner.
    fn tray_now(&self, cx: &App) -> Option<(Arranged, CardFacts)> {
        let root = self.root.read(cx);
        let open = |host: &HostKey| root.conn_of(host).is_some_and(Connection::is_open);
        let cards = tray::cards(&root.sidebar, open, |host, id| root.phone_ask(host, id));
        let arranged = self.tray.arrange(cards);
        let card = arranged.shown()?.clone();
        let refusal = card.since.and_then(|since| {
            root.answer_refusal(&card.key.host, &card.key.session_id, since)
                .map(str::to_owned)
        });
        let now = root.wall_now();
        Some((arranged, CardFacts { card, now, refusal }))
    }

    /// The card's row of buttons: one per answer, or `Open`.
    fn tray_buttons(card: &TrayCard, cx: &mut Context<Self>) -> Div {
        let row = div().flex().gap(px(8.0)).min_h(px(TOUCH_TARGET));
        let Buttons::Answer { since, buttons } = &card.buttons else {
            let host = card.key.host.clone();
            let session_id = card.key.session_id.clone();
            return row.child(
                phone_button("phone-tray-open", ButtonKind::Primary, cx)
                    .flex_1()
                    .min_h(px(TOUCH_TARGET))
                    .child(OPEN_LABEL)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.open_session(host.clone(), &session_id, cx);
                    })),
            );
        };
        let since = *since;
        row.children(buttons.iter().enumerate().map(|(at, button)| {
            let kind = if at == 0 {
                ButtonKind::Primary
            } else {
                ButtonKind::Outlined
            };
            let session_id = card.key.session_id.clone();
            let choice = button.choice.clone();
            let id = format!("phone-tray-choice-{}", at + 1);
            phone_button(&id, kind, cx)
                .flex_1()
                .min_w_0()
                .min_h(px(TOUCH_TARGET))
                .child(line(&format!("{id}-text"), button.label.clone()))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.root.update(cx, |root, cx| {
                        root.answer_from_dashboard(&session_id, since, choice.clone(), cx);
                    });
                }))
        }))
    }

    /// A pan over the tray: its sideways travel feeds the swipe; a vertical
    /// pan travels nothing sideways and scrolls the list as before.
    fn tray_scrolled(&mut self, event: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        let dx = f32::from(event.delta.pixel_delta(window.line_height()).x);
        let at = cx.background_executor().now();
        match event.touch_phase {
            TouchPhase::Started => self.tray.touch_started(dx, at),
            TouchPhase::Moved => self.tray.touch_moved(dx, at),
            TouchPhase::Ended => {
                let width = f32::from(window.viewport_size().width) - 2.0 * SIDE_PADDING;
                self.tray.touch_ended(dx, at, width);
            }
            TouchPhase::Cancelled => self.tray.touch_cancelled(),
        }
        cx.notify();
    }

    /// `host`'s header, drawn only while more than one host is listed: its
    /// state's dot, its name, where its connection stands while not online
    /// and, while it has not approved this phone, `Pair`, as the desktop's
    /// host header says them.
    fn host_header(
        host: &HomeHost,
        state: HostHeaderState,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Div {
        let tag = host_tag(&host.host);
        let id = format!("phone-home-host-{tag}");
        let status = state.label().map(|text| {
            let status_id = format!("phone-home-host-status-{tag}");
            div()
                .driver_id(|| status_id)
                .flex_none()
                .text_color(gpui::rgb(t.muted))
                .child(text)
        });
        let pair = (state == HostHeaderState::NotPaired).then(|| {
            let key = host.host.clone();
            phone_button(
                &format!("phone-home-host-pair-{tag}"),
                ButtonKind::Primary,
                cx,
            )
            .flex_none()
            .min_h(px(TOUCH_TARGET))
            .child(PAIR_LABEL)
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.pair(&key, window, cx);
            }))
        });
        div()
            .driver_id(|| id)
            .flex()
            .items_center()
            .gap(px(8.0))
            .min_h(px(TOUCH_TARGET))
            .px(px(SIDE_PADDING))
            .bg(gpui::rgb(t.raised))
            .child(dot(state.dot(t)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                    .child(host.label.clone()),
            )
            .children(status)
            .children(pair)
    }

    /// `group`'s heading, its banner when Detached, then its rows.
    fn home_group(group: &HomeGroup, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let banner = group.detached.then(|| detached_banner(t, cx));
        let rows: Vec<Stateful<Div>> = group
            .rows
            .iter()
            .map(|row| Self::session_row(row, t, cx))
            .collect();
        div()
            .flex()
            .flex_col()
            .child(group_heading(group, t, cx))
            .children(banner)
            .children(rows)
    }

    /// One session's row: a dot, its label and its status word; a tap
    /// shows the session.
    fn session_row(row: &HomeRow, t: &UiTheme, cx: &mut Context<Self>) -> Stateful<Div> {
        let id = format!("phone-home-row-{}", leaf_key(&row.host, &row.session_id));
        let color = tone_color(row.tone, t);
        let host = row.host.clone();
        let session_id = row.session_id.clone();
        div()
            .id(SharedString::from(id.clone()))
            .driver_id(|| id)
            .flex()
            .items_center()
            .gap(px(10.0))
            .min_h(px(TOUCH_TARGET))
            .px(px(SIDE_PADDING))
            .py(px(6.0))
            .when(row.tone == Tone::NeedsYou, |line| {
                line.bg(gpui::rgba(t.ask_wash))
            })
            .child(dot(color))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .truncate()
                            .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
                            .child(row.label.clone()),
                    )
                    .child(
                        div()
                            .text_size(ui_px(cx, STATUS_TEXT_SIZE))
                            .text_color(gpui::rgb(color))
                            .child(row.status),
                    ),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.open_session(host.clone(), &session_id, cx);
            }))
    }

    /// While a session is shown: its page, a header with Back and its
    /// label over an empty body.
    pub(super) fn session_page(&self, cx: &mut Context<Self>) -> Option<Div> {
        let (host, session_id) = self.shown.clone()?;
        let label = self
            .home(cx)
            .label_of(&host, &session_id)
            .map_or_else(|| session_id.clone(), str::to_owned);
        let t = theme(cx);
        let back = phone_button("phone-session-back", ButtonKind::Outlined, cx)
            .min_h(px(TOUCH_TARGET))
            .flex_none()
            .child(BACK_LABEL)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.close_session(cx)));
        let header = div()
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
                    .driver_id(|| "phone-session-label".to_owned())
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                    .child(label),
            );
        Some(
            div()
                .driver_id(|| "phone-session".to_owned())
                .size_full()
                .flex()
                .flex_col()
                .child(header)
                .child(div().flex_1().min_h_0()),
        )
    }
}
