//! Draws the phone's Home ([`super::home`]): rows and targets at least
//! 44 pt tall, no shortcut hints and nothing that shows only on hover. A
//! row's tap opens the session page `session_view` draws. It only renders
//! and forwards taps.

use std::cell::Cell;
use std::rc::Rc;

use chrono::{DateTime, Utc};
use gpui::{
    App, ClickEvent, Context, Div, FontWeight, Pixels, ScrollEvent, ScrollHandle, SharedString,
    Stateful, StyledText, TouchPhase, Window, div, prelude::*, px,
};

use super::PhoneRoot;
use super::home::{HomeGroup, HomeHost, HomeRow, Tone};
use super::hosts_view::{RowParts, phone_button, row_parts, saved_name};
use super::long_press::long_press_area;
use super::root_view::LIST_PANE_WIDTH;
use super::settings_view::{CHECK, CHEVRON};
use super::sheet::CANCEL_LABEL;
use super::tray::{self, Arranged, Buttons, TrayCard};
use crate::buttons::ButtonKind;
use crate::chat_cards;
use crate::connection::Connection;
use crate::driver_id::DriverId as _;
use crate::ellipsis::truncating;
use crate::fonts::{ui_px, ui_text_size, ui_weight_shift};
use crate::needs_you;
use crate::sidebar::{DETACHED_BANNER, PhoneOrder};
use crate::sidebar_view::{HostHeaderState, leaf_key};
use crate::target::HostKey;
use crate::ui_theme::{UiTheme, theme};

/// The least height of a row or of anything a finger taps.
pub(super) const TOUCH_TARGET: f32 = 44.0;
/// The status word's and the banner's text size: the sidebar's subline.
pub(super) const STATUS_TEXT_SIZE: f32 = 11.0;
/// The mark's dot.
const DOT: f32 = 8.0;
/// The space either side of a row's content.
pub(super) const SIDE_PADDING: f32 = 16.0;
/// A not-paired host's header button.
const PAIR_LABEL: &str = "Pair";

/// The width, in points, of the accent edge on the selected row.
const SELECTED_EDGE_WIDTH: f32 = 3.0;
/// What Home says with no session to list.
const EMPTY_TEXT: &str = "No sessions yet";
/// The Needs you card's height, whatever it holds, so the rows under the
/// tray keep their place while cards come, go and change: room for the
/// session's line and three described answer rows, each a 44 pt target,
/// under the header and above `Open`, with anything longer scrolling
/// inside.
const TRAY_CARD_HEIGHT: f32 = 356.0;
/// The button of a card whose prompt is answered on the session's page.
const OPEN_LABEL: &str = "Open";
/// Home's header button that opens the sort sheet, and the sheet's title.
const SORT_LABEL: &str = "Sort";
/// The orders the sort sheet offers, in its order: each with the tail of
/// its row's selector and its label.
const ORDERS: [(PhoneOrder, &str, &str); 4] = [
    (PhoneOrder::Manual, "manual", "Manual"),
    (PhoneOrder::Alphabetical, "alphabetical", "A–Z"),
    (PhoneOrder::NeedsYouFirst, "needs-you", "Needs you first"),
    (PhoneOrder::DesktopTabs, "desktop-tabs", "Desktop tabs"),
];

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

/// The card's text: the session, the prompt while the card says it, its
/// command, how long it has waited and, after a refused answer, why.
fn card_text(facts: &CardFacts, prompt: bool, t: &UiTheme, cx: &Context<PhoneRoot>) -> Div {
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
        .when(prompt, |text| {
            text.child(
                div()
                    .driver_id(|| "phone-tray-text".to_owned())
                    .line_clamp(2)
                    .overflow_hidden()
                    .child(card.text.clone()),
            )
        })
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

/// Whether `handle`'s body has room to scroll by `delta`: the offset runs
/// from 0 at the top to `-max` at the bottom, so a negative delta heads for
/// the bottom. A pan moves a body that has room that way, and passes at its
/// top or bottom.
fn has_room(handle: &ScrollHandle, delta: Pixels) -> bool {
    if delta == px(0.0) {
        return false;
    }
    let offset = handle.offset().y;
    let max = handle.max_offset().y;
    if delta < px(0.0) {
        offset > -max
    } else {
        offset < px(0.0)
    }
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
pub(super) fn tone_color(tone: Tone, t: &UiTheme) -> u32 {
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

/// What a host says under its header while the order picked cannot apply
/// to it.
fn note_line(host: &HostKey, note: &'static str, t: &UiTheme, cx: &Context<PhoneRoot>) -> Div {
    let id = format!("phone-home-note-{}", host_tag(host));
    div()
        .driver_id(|| id)
        .px(px(SIDE_PADDING))
        .py(px(4.0))
        .text_size(ui_px(cx, STATUS_TEXT_SIZE))
        .text_color(gpui::rgb(t.muted))
        .child(note)
}

/// The line between two rows of a group, inset by the side padding.
fn hairline(t: &UiTheme) -> Div {
    div()
        .flex_none()
        .h(px(1.0))
        .mx(px(SIDE_PADDING))
        .bg(gpui::rgb(t.line))
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
                let (state, parts) = {
                    let root = self.root.read(cx);
                    (
                        root.host_header_state(&host.host),
                        row_parts(root, &host.host),
                    )
                };
                children.push(Self::host_header(host, state, &parts, &t, cx));
            }
            if let Some(note) = host.note {
                children.push(note_line(&host.host, note, &t, cx));
            }
            for group in &host.groups {
                let selected = self.selected_in(group);
                children.push(Self::home_group(group, selected.as_deref(), &t, cx));
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
            .child(self.tray_body(&facts, &t, cx))
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

    /// The card's scrolling body: the session and its wait, then the answer
    /// rows of a card the tray can answer, or every question and option of
    /// one it cannot. Its scroll is the shown card's own, and a vertical
    /// pan in it moves it alone while it has room.
    fn tray_body(&self, facts: &CardFacts, t: &UiTheme, cx: &mut Context<Self>) -> Stateful<Div> {
        let card = &facts.card;
        let listed = matches!(card.buttons, Buttons::Open) && !card.questions.is_empty();
        let handle = self.tray.body_scroll(&card.key);
        let id = format!(
            "phone-tray-body-{}-{}",
            host_tag(&card.key.host),
            card.key.session_id
        );
        let scrolled = handle.clone();
        let sideways = Rc::new(Cell::new(false));
        let seen = Rc::clone(&sideways);
        let body = div()
            .id(id)
            .driver_id(|| "phone-tray-body".to_owned())
            .flex_1()
            .min_h(px(0.0))
            .w_full()
            .overflow_y_scroll()
            .restrict_scroll_to_axis()
            .track_scroll(&handle)
            .on_scroll(cx.listener(move |this, event: &ScrollEvent, window, cx| {
                this.tray_body_scrolled(&scrolled, &seen, event, window, cx);
            }))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(card_text(facts, !listed, t, cx));
        if listed {
            body.children(chat_cards::tray_questions(
                ui_text_size(cx),
                ui_weight_shift(cx),
                &card.questions,
                TOUCH_TARGET,
                t,
            ))
        } else {
            body.children(Self::tray_answers(card, t, cx))
        }
    }

    /// A pan over the card's body: a vertical move with the body's room left
    /// in that direction scrolls the body alone, so Home's list holds still;
    /// at the body's top or bottom the move goes on to the list, as does any
    /// sideways travel, so the tray's swipe still sees the whole gesture. A
    /// gesture that never travelled sideways ends the tray's swipe here when
    /// the lift carries the last of its vertical travel, so the list takes
    /// none of it.
    fn tray_body_scrolled(
        &mut self,
        handle: &ScrollHandle,
        sideways: &Cell<bool>,
        event: &ScrollEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = event.delta.pixel_delta(window.line_height());
        match event.touch_phase {
            TouchPhase::Started => {
                sideways.set(false);
                if delta.x == px(0.0) && has_room(handle, delta.y) {
                    // The recogniser folds the gesture's first travel into
                    // its start: the tray still begins the gesture, but the
                    // list takes none of it.
                    let at = cx.background_executor().now();
                    self.tray.touch_started(0.0, at);
                    cx.stop_propagation();
                }
            }
            TouchPhase::Moved => {
                if delta.x != px(0.0) {
                    sideways.set(true);
                } else if has_room(handle, delta.y) {
                    cx.stop_propagation();
                }
            }
            TouchPhase::Ended => {
                if !sideways.get() && delta.x == px(0.0) && has_room(handle, delta.y) {
                    self.tray.touch_cancelled();
                    cx.stop_propagation();
                }
            }
            TouchPhase::Cancelled => sideways.set(false),
        }
    }

    /// A card's answer rows: one per button the tray can send, a full-width
    /// row at the phone's touch target, its description under its label.
    fn tray_answers(card: &TrayCard, t: &UiTheme, cx: &mut Context<Self>) -> Vec<Stateful<Div>> {
        let Buttons::Answer { since, buttons } = &card.buttons else {
            return Vec::new();
        };
        let since = *since;
        buttons
            .iter()
            .enumerate()
            .map(|(at, button)| {
                let id = format!("phone-tray-choice-{}", at + 1);
                let session_id = card.key.session_id.clone();
                let choice = button.choice.clone();
                let description_selector = button
                    .description
                    .as_ref()
                    .map(|_| format!("phone-tray-choice-desc-{}", at + 1));
                let row = chat_cards::option_row(
                    chat_cards::OptionRow {
                        offset: ui_text_size(cx),
                        shift: ui_weight_shift(cx),
                        selector: &id,
                        label: &button.label,
                        description: button.description.as_deref(),
                        description_selector,
                        selected: false,
                        enabled: true,
                        min_height: Some(TOUCH_TARGET),
                    },
                    t,
                );
                row.w_full()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.root.update(cx, |root, cx| {
                            root.answer_from_dashboard(&session_id, since, choice.clone(), cx);
                        });
                    }))
            })
            .collect()
    }

    /// The card's pinned footer: `Open`, which shows the session's page.
    fn tray_buttons(card: &TrayCard, cx: &mut Context<Self>) -> Div {
        let host = card.key.host.clone();
        let session_id = card.key.session_id.clone();
        div().flex().justify_end().min_h(px(TOUCH_TARGET)).child(
            phone_button("phone-tray-open", ButtonKind::Primary, cx)
                .min_h(px(TOUCH_TARGET))
                .child(OPEN_LABEL)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.open_session(host.clone(), &session_id, cx);
                })),
        )
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
                // The tray spans the list pane in two panes, else the window.
                let list = if self.layout.two_pane() {
                    LIST_PANE_WIDTH
                } else {
                    f32::from(window.viewport_size().width)
                };
                self.tray.touch_ended(dx, at, list - 2.0 * SIDE_PADDING);
            }
            TouchPhase::Cancelled => self.tray.touch_cancelled(),
        }
        cx.notify();
    }

    /// `host`'s header, drawn only while more than one host is listed: its
    /// state's dot, its name, where its connection stands while not online,
    /// `Pair` while it has not approved this phone and `Retry` while it is
    /// offline, as the desktop's host header says them; under it the parts
    /// its connection calls for, drawn as its row on the hosts screen draws
    /// them.
    fn host_header(
        host: &HomeHost,
        state: HostHeaderState,
        parts: &RowParts,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Div {
        let tag = host_tag(&host.host);
        let id = format!("phone-home-host-{tag}");
        let name = saved_name(&host.host);
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
        let redial = parts.redial.then(|| {
            Self::retry_button(name, &host.host, cx)
                .flex_none()
                .min_h(px(TOUCH_TARGET))
        });
        let head = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .min_h(px(TOUCH_TARGET))
            .px(px(SIDE_PADDING))
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
            .children(redial);
        let mut extra = div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .px(px(SIDE_PADDING))
            .pb(px(6.0));
        if let Some(fingerprint) = parts.prompt.as_deref() {
            extra = extra.child(Self::host_key_prompt(name, &host.host, fingerprint, t, cx));
        }
        if let Some(reason) = parts.reason.as_deref() {
            extra = extra.child(Self::reason_line(name, reason, t));
        }
        if parts.retry {
            extra = extra.child(Self::retry_line(name, &host.host, cx));
        }
        div()
            .driver_id(|| id)
            .flex()
            .flex_col()
            .bg(gpui::rgb(t.raised))
            .child(head)
            .when(
                parts.prompt.is_some() || parts.reason.is_some() || parts.retry,
                |header| header.child(extra),
            )
    }

    /// The id of the session in `group` selected beside the list, in two
    /// panes.
    fn selected_in(&self, group: &HomeGroup) -> Option<String> {
        group
            .rows
            .iter()
            .find(|row| self.is_selected(&row.host, &row.session_id))
            .map(|row| row.session_id.clone())
    }

    /// `group`'s heading, its banner when Detached, then its rows with a
    /// hairline between each two, the one whose session is `selected`
    /// marked.
    fn home_group(
        group: &HomeGroup,
        selected: Option<&str>,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Div {
        let banner = group.detached.then(|| detached_banner(t, cx));
        let mut body = div()
            .flex()
            .flex_col()
            .child(group_heading(group, t, cx))
            .children(banner);
        for (at, row) in group.rows.iter().enumerate() {
            if at > 0 {
                body = body.child(hairline(t));
            }
            let chosen = selected == Some(row.session_id.as_str());
            body = body.child(Self::session_row(row, chosen, t, cx));
        }
        body
    }

    /// Home's header button that opens the sort sheet.
    pub(super) fn sort_button(cx: &mut Context<Self>) -> Stateful<Div> {
        phone_button("phone-home-sort", ButtonKind::Outlined, cx)
            .min_h(px(TOUCH_TARGET))
            .flex_none()
            .child(SORT_LABEL)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.sort_menu = true;
                cx.notify();
            }))
    }

    /// Picks `order` for Home: saved, and the sheet closes.
    fn choose_order(&mut self, order: PhoneOrder, cx: &mut Context<Self>) {
        tracing::debug!(?order, "phone: Home's order picked");
        self.sort_menu = false;
        self.root
            .update(cx, |root, cx| root.set_phone_order(order, cx));
        cx.notify();
    }

    /// The sort sheet over a scrim, as the long-press sheet draws: its
    /// title, a row per order with the current one checked, then Cancel,
    /// its bottom clear of `bottom`'s home indicator. `None` while closed.
    pub(super) fn sort_sheet_layer(&self, bottom: Pixels, cx: &mut Context<Self>) -> Option<Div> {
        if !self.sort_menu {
            return None;
        }
        let t = theme(cx);
        let current = self.root.read(cx).sidebar.ui_state().phone_order;
        let close = |this: &mut Self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>| {
            this.sort_menu = false;
            cx.notify();
        };
        let scrim = div()
            .id("phone-home-sort-scrim")
            .driver_id(|| "phone-home-sort-scrim".to_owned())
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(gpui::rgba(t.scrim))
            .occlude()
            .on_click(cx.listener(close));
        let rows: Vec<Stateful<Div>> = ORDERS
            .iter()
            .map(|&entry| Self::sort_row(entry, entry.0 == current, &t, cx))
            .collect();
        let panel = div()
            .id("phone-home-sort-sheet")
            .driver_id(|| "phone-home-sort-sheet".to_owned())
            .occlude()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .px(px(16.0))
            .pt(px(16.0))
            .pb(bottom + px(16.0))
            .rounded_t(px(14.0))
            .border_t_1()
            .border_color(gpui::rgb(t.line))
            .bg(gpui::rgb(t.raised))
            .child(
                div()
                    .flex()
                    .items_center()
                    .min_h(px(TOUCH_TARGET))
                    .px(px(4.0))
                    .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                    .child(SORT_LABEL),
            )
            .child(div().flex().flex_col().children(rows))
            .child(
                Self::sheet_button("phone-home-sort-cancel", ButtonKind::Outlined, cx)
                    .child(CANCEL_LABEL)
                    .on_click(cx.listener(close)),
            );
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .flex_col()
                .justify_end()
                .child(scrim)
                .child(panel),
        )
    }

    /// One order's row in the sort sheet (`entry` as [`ORDERS`] lists it),
    /// as Appearance's theme rows draw: the check while it is `current`,
    /// then its label, a touch target tall; a tap picks it.
    fn sort_row(
        entry: (PhoneOrder, &str, &'static str),
        current: bool,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let (order, key, label) = entry;
        let id = format!("phone-home-sort-{key}");
        let check_id = format!("{id}-check");
        let mark = div()
            .flex_none()
            .w(px(18.0))
            .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
            .when(current, |mark| mark.driver_id(|| check_id).child(CHECK));
        div()
            .id(SharedString::from(id.clone()))
            .driver_id(|| id)
            .flex()
            .items_center()
            .gap(px(10.0))
            .min_h(px(TOUCH_TARGET))
            .px(px(4.0))
            .py(px(6.0))
            .border_b_1()
            .border_color(gpui::rgb(t.line))
            .child(mark)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
                    .child(label),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.choose_order(order, cx)))
    }

    /// One session's row: a dot, its label and its status word, then a
    /// chevron at its right edge, on the raised ground with an accent edge
    /// while `selected` and while a finger is down on it; a tap anywhere on
    /// it shows the session (nothing while it is the one selected), a long
    /// press opens its sheet.
    fn session_row(
        row: &HomeRow,
        selected: bool,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let leaf = leaf_key(&row.host, &row.session_id);
        let id = format!("phone-home-row-{leaf}");
        let chevron_id = format!("{id}-chevron");
        let raised = t.raised;
        let mark = selected.then(|| {
            let mark_id = format!("phone-home-row-selected-{leaf}");
            div()
                .driver_id(|| mark_id)
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .w(px(SELECTED_EDGE_WIDTH))
                .bg(gpui::rgb(t.accent))
        });
        let color = tone_color(row.tone, t);
        let host = row.host.clone();
        let session_id = row.session_id.clone();
        let phone = cx.weak_entity();
        let (press_host, press_id) = (row.host.clone(), row.session_id.clone());
        let long_press = long_press_area(move |_, _, cx| {
            // Fails only when the phone's root is gone.
            phone
                .update(cx, |this, cx| {
                    this.open_sheet(press_host.clone(), &press_id, cx);
                })
                .ok();
        });
        div()
            .id(SharedString::from(id.clone()))
            .driver_id(|| id)
            .relative()
            .flex()
            .items_center()
            .gap(px(10.0))
            .min_h(px(TOUCH_TARGET))
            .px(px(SIDE_PADDING))
            .py(px(6.0))
            .when(row.tone == Tone::NeedsYou, |line| {
                line.bg(gpui::rgba(t.ask_wash))
            })
            .when(selected, |line| line.bg(gpui::rgb(t.raised)))
            .active(move |line| line.bg(gpui::rgb(raised)))
            .children(mark)
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
            .child(
                div()
                    .driver_id(|| chevron_id)
                    .flex_none()
                    .text_color(gpui::rgb(t.muted))
                    .child(CHEVRON),
            )
            .child(long_press)
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                if !this.is_selected(&host, &session_id) {
                    this.open_session(host.clone(), &session_id, cx);
                }
            }))
    }
}
