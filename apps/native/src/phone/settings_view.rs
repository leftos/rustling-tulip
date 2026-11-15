//! Draws the phone's Settings ([`super::settings`]): the header row over
//! Home and the hosts screen with its `Settings` button, and, while
//! Settings is open, its own header (Back and the title) over the category
//! list or a category's page. It only renders and forwards taps.

use gpui::{
    BackGestureEvent, ClickEvent, Context, Div, FontWeight, SharedString, Stateful, Window, div,
    prelude::*, px,
};

use super::PhoneRoot;
use super::home_view::{SIDE_PADDING, TOUCH_TARGET};
use super::hosts_view::phone_button;
use super::session;
use super::settings::{BackStep, Page, Settings};
use crate::buttons::ButtonKind;
use crate::driver_id::DriverId as _;
use crate::fonts::ui_px;
use crate::ui_theme::{UiTheme, theme};

/// The header's button that opens Settings.
const OPEN_LABEL: &str = "Settings";
/// Settings' Back button.
const BACK_LABEL: &str = "Back";
/// What the Notifications page says while no host draws a switch.
const NO_NOTIFICATIONS_TEXT: &str = "No host offers notifications yet.";
/// The mark at a category row's right end.
const CHEVRON: &str = "›";

/// A header row's frame, the session page's: at least a touch target tall,
/// the side padding, a line under it.
fn header_frame(t: &UiTheme) -> Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(12.0))
        .min_h(px(TOUCH_TARGET))
        .px(px(SIDE_PADDING))
        .py(px(6.0))
        .border_b_1()
        .border_color(gpui::rgb(t.line))
}

impl PhoneRoot {
    /// Opens Settings on its category list, as the header's button does.
    pub fn open_settings(&mut self, cx: &mut Context<Self>) {
        tracing::debug!("phone: opening Settings");
        self.forget_pending_open();
        self.settings = Some(Settings::default());
        cx.notify();
    }

    /// Opens `page` over Settings' list, as its row's tap does.
    fn open_settings_page(&mut self, page: Page, cx: &mut Context<Self>) {
        self.forget_pending_open();
        if let Some(settings) = self.settings.as_mut() {
            settings.open(page);
        }
        cx.notify();
    }

    /// One Back step in Settings: a page goes back to the list, the list
    /// closes Settings.
    pub(super) fn settings_back(&mut self, cx: &mut Context<Self>) {
        self.forget_pending_open();
        let step = self.settings.as_mut().map(Settings::back);
        if step == Some(BackStep::Close) {
            self.settings = None;
        }
        cx.notify();
    }

    /// A back gesture over the window while a session or Settings shows:
    /// one step of the screen shown once it commits.
    pub(super) fn screen_back_gesture(
        &mut self,
        event: &BackGestureEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !session::back_on(event.phase) {
            return;
        }
        if self.shown.is_some() {
            self.session_back(cx);
        } else if self.settings.is_some() {
            self.settings_back(cx);
        }
    }

    /// The header over Home and the hosts screen: the `Settings` button at
    /// its right end.
    pub(super) fn home_header_row(t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let open = phone_button("phone-settings-open", ButtonKind::Outlined, cx)
            .min_h(px(TOUCH_TARGET))
            .flex_none()
            .child(OPEN_LABEL)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.open_settings(cx)));
        header_frame(t).justify_end().child(open)
    }

    /// While Settings is open: its header over the category list or the
    /// page it is on.
    pub(super) fn settings_page(&self, cx: &mut Context<Self>) -> Option<Div> {
        let settings = self.settings?;
        let t = theme(cx);
        let body = match settings.page {
            None => Self::category_list(&t, cx),
            Some(Page::Notifications) => self.notifications_page(&t, cx),
        };
        Some(
            div()
                .driver_id(|| "phone-settings".to_owned())
                .size_full()
                .flex()
                .flex_col()
                .child(Self::settings_header_row(settings.title(), &t, cx))
                .child(
                    div()
                        .id("phone-settings-body")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .child(body),
                ),
        )
    }

    /// Settings' header: Back, then the title.
    fn settings_header_row(title: &'static str, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let back = phone_button("phone-settings-back", ButtonKind::Outlined, cx)
            .min_h(px(TOUCH_TARGET))
            .flex_none()
            .child(BACK_LABEL)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.settings_back(cx)));
        header_frame(t).child(back).child(
            div()
                .driver_id(|| "phone-settings-title".to_owned())
                .flex_1()
                .min_w_0()
                .truncate()
                .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                .child(title),
        )
    }

    /// The category list: one row per category, in [`Page::ALL`]'s order.
    fn category_list(t: &UiTheme, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .children(Page::ALL.map(|page| Self::category_row(page, t, cx)))
    }

    /// `page`'s row: its name and a chevron, a touch target tall; a tap
    /// opens its page.
    fn category_row(page: Page, t: &UiTheme, cx: &mut Context<Self>) -> Stateful<Div> {
        let id = format!("phone-settings-row-{}", page.key());
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
                    .truncate()
                    .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
                    .child(page.label()),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(gpui::rgb(t.muted))
                    .child(CHEVRON),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.open_settings_page(page, cx);
            }))
    }

    /// The Notifications page: each open host's switch and hint as Home's
    /// footer drew them, or a line saying no host offers one.
    fn notifications_page(&self, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let lines = self.push_lines(t, cx);
        let empty = lines.is_empty().then(|| {
            div()
                .driver_id(|| "phone-settings-notifications-empty".to_owned())
                .text_color(gpui::rgb(t.muted))
                .child(NO_NOTIFICATIONS_TEXT)
        });
        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(SIDE_PADDING))
            .children(lines)
            .children(empty)
    }
}
