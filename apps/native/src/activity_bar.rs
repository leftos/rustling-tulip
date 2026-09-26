//! The activity rail at the window's left edge: Sessions and Source control,
//! which pick the panel beside it and fold it, the badge counting the
//! uncommitted changes, and the Settings gear at the bottom.

use gpui::{ClickEvent, Context, Div, FontWeight, Stateful, Window, div, prelude::*, px, svg};

use crate::assets::{SESSIONS_ICON, SOURCE_CONTROL_ICON};
use crate::sidebar::Activity;
use crate::{BORDER, HOVER_BG, MUTED, PANEL_BG, RootView, TEXT, tooltip};

const RAIL_WIDTH: f32 = 40.0;
const ICON_SIZE: f32 = 18.0;
const ITEM_PADDING: f32 = 9.0;
const ACTIVE_BAR_WIDTH: f32 = 2.0;
const BADGE_HEIGHT: f32 = 16.0;
const BADGE_TEXT_SIZE: f32 = 10.0;
const BADGE_INSET: f32 = 4.0;
/// The badge's text, dark on the accent.
const BADGE_TEXT: u32 = 0x000f_1014;

/// The badge's text for `count`: hidden at 0, `99+` above 99.
#[must_use]
pub fn badge_text(count: usize) -> Option<String> {
    match count {
        0 => None,
        1..=99 => Some(count.to_string()),
        _ => Some("99+".to_owned()),
    }
}

/// A rail item's tooltip, in the Tauri client's words.
fn item_tip(label: &str, badge: usize, active: bool, collapsed: bool) -> String {
    let base = if badge > 0 {
        format!("{label} ({badge} uncommitted)")
    } else {
        label.to_owned()
    };
    let action = match (active, collapsed) {
        (true, false) => "collapse sidebar",
        (true, true) => "expand sidebar",
        (false, true) => "open",
        (false, false) => "show",
    };
    format!("{base} — click to {action} (Ctrl+B)")
}

/// What a rail item shows.
#[derive(Clone, Copy)]
struct Item {
    activity: Activity,
    selector: &'static str,
    label: &'static str,
    icon: &'static str,
    badge: usize,
}

impl RootView {
    /// The panel the rail shows.
    #[must_use]
    pub fn activity(&self) -> Activity {
        self.sidebar.activity()
    }

    /// The Source control item's badge as drawn, when it shows.
    #[must_use]
    pub fn activity_badge(&self) -> Option<String> {
        badge_text(self.sc_badge())
    }

    /// The rail, always shown, left of the panel.
    pub(crate) fn activity_rail(&self, cx: &mut Context<Self>) -> Div {
        let items = [
            Item {
                activity: Activity::Sessions,
                selector: "activity-sessions",
                label: "Sessions",
                icon: SESSIONS_ICON,
                badge: 0,
            },
            Item {
                activity: Activity::SourceControl,
                selector: "activity-source-control",
                label: "Source control",
                icon: SOURCE_CONTROL_ICON,
                badge: self.sc_badge(),
            },
        ];
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(RAIL_WIDTH))
            .h_full()
            .py(px(6.0))
            .gap(px(2.0))
            .debug_selector(|| "activity-rail".to_owned())
            .bg(gpui::rgb(PANEL_BG))
            .border_r_1()
            .border_color(gpui::rgb(BORDER))
            .children(items.into_iter().map(|item| self.rail_item(item, cx)))
            .child(div().flex_1())
            .child(settings_gear(cx))
    }

    fn rail_item(&self, item: Item, cx: &mut Context<Self>) -> Stateful<Div> {
        let active = self.sidebar.activity() == item.activity;
        let collapsed = self.sidebar.is_collapsed();
        let accent = self.sidebar.appearance(None).accent.value;
        let activity = item.activity;
        let selector = item.selector;
        div()
            .id(selector)
            .debug_selector(move || selector.to_owned())
            .relative()
            .flex()
            .justify_center()
            .items_center()
            .py(px(ITEM_PADDING))
            .cursor_pointer()
            .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
            .child(
                svg()
                    .path(item.icon)
                    .size(px(ICON_SIZE))
                    .text_color(gpui::rgb(if active { TEXT } else { MUTED })),
            )
            .when(active, |row| {
                row.child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(ACTIVE_BAR_WIDTH))
                        .bg(gpui::rgb(accent)),
                )
            })
            .when_some(badge_text(item.badge), |row, text| {
                row.child(badge(text, accent))
            })
            .tooltip(tooltip(item_tip(item.label, item.badge, active, collapsed)))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.click_activity(activity, window, cx);
            }))
    }

    /// A rail click: see [`crate::sidebar::SidebarModel::click_activity`].
    /// Folding the panel hands the keyboard back to the active pane.
    fn click_activity(&mut self, item: Activity, window: &mut Window, cx: &mut Context<Self>) {
        self.close_shell_menu(window, cx);
        self.close_sc_picker(window, cx);
        self.sidebar.click_activity(item);
        self.drag = None;
        self.save_ui();
        if self.sidebar.is_collapsed() {
            self.focus_active_pane(window, cx);
        }
        cx.notify();
    }
}

fn badge(text: String, accent: u32) -> Div {
    div()
        .absolute()
        .right(px(BADGE_INSET))
        .bottom(px(BADGE_INSET))
        .flex()
        .items_center()
        .justify_center()
        .min_w(px(BADGE_HEIGHT))
        .h(px(BADGE_HEIGHT))
        .px(px(4.0))
        .rounded_full()
        .bg(gpui::rgb(accent))
        .text_color(gpui::rgb(BADGE_TEXT))
        .text_size(px(BADGE_TEXT_SIZE))
        .font_weight(FontWeight::SEMIBOLD)
        .debug_selector(|| "activity-badge".to_owned())
        .child(text)
}

/// The Settings gear, pinned to the rail's bottom.
fn settings_gear(cx: &mut Context<RootView>) -> Stateful<Div> {
    div()
        .id("settings-open")
        .debug_selector(|| "settings-open".to_owned())
        .flex()
        .justify_center()
        .items_center()
        .py(px(ITEM_PADDING))
        .cursor_pointer()
        .text_size(px(ICON_SIZE))
        .text_color(gpui::rgb(MUTED))
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)).text_color(gpui::rgb(TEXT)))
        .child("⚙")
        .tooltip(tooltip("Settings (Ctrl+,)"))
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.open_settings(window, cx);
        }))
}
