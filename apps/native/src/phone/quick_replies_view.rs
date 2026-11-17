//! Draws Settings' Quick replies page ([`super::quick_replies`]' data): each
//! set's groups as rows that open a group's page, and that page's Title row
//! over its replies. The page is read-only; the editors come over it.

use gpui::{App, ClickEvent, Context, Div, SharedString, Stateful, div, prelude::*, px};

use super::PhoneRoot;
use super::home_view::{SIDE_PADDING, TOUCH_TARGET};
use super::quick_replies::{self, ReplyGroup, ReplySet};
use super::settings::{Page, Settings, SubPage};
use super::settings_view::{CHEVRON, section_label};
use crate::driver_id::DriverId as _;
use crate::fonts::ui_px;
use crate::ui_theme::UiTheme;

/// The heading over a group page's replies.
const REPLIES_LABEL: &str = "Replies";

/// The two sets, in the order the page draws them.
const SETS: [ReplySet; 2] = [ReplySet::Agent, ReplySet::Shell];

/// The set's name over its groups.
fn set_label(set: ReplySet) -> &'static str {
    match set {
        ReplySet::Agent => "Agent sessions",
        ReplySet::Shell => "Shells",
    }
}

/// The tail of the set's driver ids.
fn set_key(set: ReplySet) -> &'static str {
    match set {
        ReplySet::Agent => "agent",
        ReplySet::Shell => "shell",
    }
}

/// How many replies a group holds, as its row's count reads.
fn reply_count(n: usize) -> String {
    if n == 1 {
        "1 reply".to_owned()
    } else {
        format!("{n} replies")
    }
}

impl PhoneRoot {
    /// Opens Settings on its Quick replies page, as the shot example does.
    pub fn open_settings_replies(&mut self, cx: &mut Context<Self>) {
        self.open_settings_on(Page::QuickReplies, cx);
    }

    /// Opens Settings on `set`'s group `group`, as a row's tap and the shot
    /// example do.
    pub fn open_settings_reply_group(
        &mut self,
        set: ReplySet,
        group: usize,
        cx: &mut Context<Self>,
    ) {
        self.forget_pending_open();
        let mut settings = Settings::default();
        settings.open(Page::QuickReplies);
        settings.push(SubPage::ReplyGroup { set, group });
        self.settings = Some(settings);
        cx.notify();
    }

    /// The Quick replies page: each set's groups under its name.
    pub(super) fn quick_replies_page(&self, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(SIDE_PADDING))
            .children(SETS.map(|set| self.reply_set_section(set, t, cx)))
    }

    /// One set: its name, then a row per group.
    fn reply_set_section(&self, set: ReplySet, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let groups = self.saved_groups(set, cx);
        let rows: Vec<Stateful<Div>> = groups
            .iter()
            .enumerate()
            .map(|(index, group)| Self::group_row(set, group, index, t, cx))
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .driver_id(|| format!("phone-replies-{}", set_key(set)))
            .child(section_label(set_label(set), t, cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .driver_id(|| format!("phone-replies-{}-groups", set_key(set)))
                    .children(rows),
            )
    }

    /// The set's groups as the page draws them: the saved ones, else the
    /// built-in defaults.
    fn saved_groups(&self, set: ReplySet, cx: &App) -> Vec<ReplyGroup> {
        let saved = self.saved_replies(set, cx);
        quick_replies::groups(saved.as_deref(), set)
    }

    /// One group's row: its title over its reply count, a chevron, a touch
    /// target tall; a tap opens the group's page.
    fn group_row(
        set: ReplySet,
        group: &ReplyGroup,
        index: usize,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let id = format!("phone-replies-{}-group-{index}", set_key(set));
        div()
            .id(SharedString::from(id.clone()))
            .driver_id(|| id)
            .flex()
            .items_center()
            .gap(px(10.0))
            .min_h(px(TOUCH_TARGET))
            .px(px(SIDE_PADDING))
            .py(px(6.0))
            .border_b_1()
            .border_color(gpui::rgb(t.line))
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
                            .child(group.title.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
                            .text_color(gpui::rgb(t.muted))
                            .child(reply_count(group.replies.len())),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(gpui::rgb(t.muted))
                    .child(CHEVRON),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.open_settings_reply_group(set, index, cx);
            }))
    }

    /// One group's page: its Title row over its replies.
    pub(super) fn reply_group_page(
        &self,
        set: ReplySet,
        group: usize,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Div {
        let groups = self.saved_groups(set, cx);
        let Some(entry) = groups.get(group) else {
            return div();
        };
        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(SIDE_PADDING))
            .child(Self::group_title_row(entry, t, cx))
            .child(section_label(REPLIES_LABEL, t, cx))
            .child(
                div()
                    .driver_id(|| "phone-replies-replies".to_owned())
                    .flex()
                    .flex_col()
                    .children(
                        entry
                            .replies
                            .iter()
                            .enumerate()
                            .map(|(index, text)| Self::reply_row(text, index, t, cx)),
                    ),
            )
    }

    /// The group page's Title row: the group's title and a chevron.
    fn group_title_row(group: &ReplyGroup, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        div()
            .driver_id(|| "phone-replies-group-title".to_owned())
            .flex()
            .items_center()
            .gap(px(10.0))
            .min_h(px(TOUCH_TARGET))
            .py(px(6.0))
            .border_b_1()
            .border_color(gpui::rgb(t.line))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
                    .child(group.title.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(gpui::rgb(t.muted))
                    .child(CHEVRON),
            )
    }

    /// One reply's row: its text, truncated, and a chevron.
    fn reply_row(text: &str, index: usize, t: &UiTheme, cx: &mut Context<Self>) -> Stateful<Div> {
        let id = format!("phone-replies-reply-{index}");
        div()
            .id(SharedString::from(id.clone()))
            .driver_id(|| id)
            .flex()
            .items_center()
            .gap(px(10.0))
            .min_h(px(TOUCH_TARGET))
            .py(px(6.0))
            .border_b_1()
            .border_color(gpui::rgb(t.line))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
                    .child(text.to_owned()),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(gpui::rgb(t.muted))
                    .child(CHEVRON),
            )
    }
}
