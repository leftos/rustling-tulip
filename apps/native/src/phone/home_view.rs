//! Draws the phone's Home ([`super::home`]) and the page a row's tap
//! opens: rows and targets at least 44 pt tall, no shortcut hints and
//! nothing that shows only on hover. It only renders and forwards taps.

use gpui::{ClickEvent, Context, Div, FontWeight, SharedString, Stateful, div, prelude::*, px};

use super::PhoneRoot;
use super::home::{HomeGroup, HomeHost, HomeRow, Tone};
use super::hosts_view::phone_button;
use crate::buttons::ButtonKind;
use crate::driver_id::DriverId as _;
use crate::fonts::ui_px;
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
        let mut children: Vec<Div> = Vec::new();
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
            .children(children)
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
