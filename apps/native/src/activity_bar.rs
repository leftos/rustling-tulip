//! The activity rail at the window's left edge: Sessions, Needs You and
//! Source control, which pick the panel beside it and fold it, the badges
//! counting the sessions waiting on the user and the uncommitted changes,
//! the Dashboard button, which opens or shows the Dashboard tab, and at the
//! bottom the Recover sessions button with its badge and the Settings
//! button.

use gpui::{
    ClickEvent, Context, DispatchPhase, Div, FontWeight, MouseButton, MouseDownEvent,
    MouseExitEvent, MouseUpEvent, SharedString, Stateful, Window, canvas, div, prelude::*, px, svg,
};

use crate::assets::{
    DASHBOARD_ICON, NEEDS_YOU_ICON, RECOVER_ICON, SESSIONS_ICON, SETTINGS_ICON, SOURCE_CONTROL_ICON,
};
use crate::palette::{ACCENT, CHIP, HOVER, SUBTLE, SUNKEN};
use crate::sidebar::{Activity, SidebarModel};
use crate::{BORDER, RootView, TEXT, tooltip};

/// The rail's width in logical pixels, its right border included.
pub const RAIL_WIDTH: f32 = 52.0;
/// One rail button's height in logical pixels; it spans the rail's width.
/// The buttons stack from the rail's top with no gap.
pub const ITEM_HEIGHT: f32 = 48.0;
/// The rounded tile a button's icon sits on.
const TILE_SIZE: f32 = 36.0;
const TILE_RADIUS: f32 = 9.0;
const ICON_SIZE: f32 = 19.0;
const ACTIVE_BAR_WIDTH: f32 = 2.0;
/// The active bar's distance from its button's top and bottom.
const ACTIVE_BAR_INSET: f32 = 10.0;
const BADGE_HEIGHT: f32 = 16.0;
const BADGE_TEXT_SIZE: f32 = 10.0;
const BADGE_INSET: f32 = 4.0;
/// The ring in the rail's colour that sets a badge off its tile.
const BADGE_RING: f32 = 2.0;
/// Space under the Settings button.
const RAIL_BOTTOM: f32 = 8.0;

/// The badge's text for `count`: hidden at 0, `99+` above 99.
#[must_use]
pub fn badge_text(count: usize) -> Option<String> {
    match count {
        0 => None,
        1..=99 => Some(count.to_string()),
        _ => Some("99+".to_owned()),
    }
}

/// A rail item's tooltip, in the Tauri client's words; `noun` says what the
/// badge counts.
fn item_tip(label: &str, badge: usize, noun: &str, active: bool, collapsed: bool) -> String {
    let base = if badge > 0 {
        format!("{label} ({badge} {noun})")
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

/// The Dashboard button's tooltip, naming the sessions waiting on the user.
fn dashboard_tip(waiting: usize) -> String {
    if waiting > 0 {
        format!("Dashboard ({waiting} waiting on you)")
    } else {
        "Dashboard".to_owned()
    }
}

/// The rail button a press put the tooltip away on: it offers no tooltip
/// until the pointer leaves it with no button held, or leaves the window,
/// so the tooltip cannot cover the panel the press opened.
#[derive(Debug, Default)]
pub(crate) struct RailTipSuppression {
    pressed: Option<&'static str>,
    /// Whether the press is still held. gpui reports a held element as
    /// unhovered, so a leave while held is no leave.
    held: bool,
}

impl RailTipSuppression {
    /// The button tagged `selector` was pressed.
    pub(crate) fn press(&mut self, selector: &'static str) {
        self.pressed = Some(selector);
        self.held = true;
    }

    /// The press on the button tagged `selector` was released, over the
    /// button (`inside`) or off it; whether that brought a put-away tooltip
    /// back, which a release off the button does.
    pub(crate) fn release(&mut self, selector: &'static str, inside: bool) -> bool {
        if self.pressed != Some(selector) {
            return false;
        }
        self.held = false;
        if inside {
            return false;
        }
        self.pressed = None;
        true
    }

    /// The pointer entered (`hovered`) or left the button tagged `selector`;
    /// whether that brought a put-away tooltip back.
    pub(crate) fn hover_changed(&mut self, selector: &'static str, hovered: bool) -> bool {
        let leaves_pressed = !hovered && !self.held && self.pressed == Some(selector);
        if leaves_pressed {
            self.pressed = None;
        }
        leaves_pressed
    }

    /// The pointer left the window; whether that brought a put-away tooltip
    /// back.
    pub(crate) fn pointer_left_window(&mut self) -> bool {
        self.held = false;
        self.pressed.take().is_some()
    }

    /// Whether the button tagged `selector` offers its tooltip.
    pub(crate) fn offers_tip(&self, selector: &'static str) -> bool {
        self.pressed != Some(selector)
    }
}

/// What a rail item shows.
#[derive(Clone, Copy)]
struct Item {
    activity: Activity,
    selector: &'static str,
    label: &'static str,
    icon: &'static str,
    badge: usize,
    badge_selector: &'static str,
    /// What the badge counts, in the tooltip.
    noun: &'static str,
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
                badge_selector: "activity-sessions-badge",
                noun: "",
            },
            Item {
                activity: Activity::NeedsYou,
                selector: "activity-needs-you",
                label: "Needs You",
                icon: NEEDS_YOU_ICON,
                badge: self.needs_you_count(),
                badge_selector: "needs-you-badge",
                noun: "waiting on you",
            },
            Item {
                activity: Activity::SourceControl,
                selector: "activity-source-control",
                label: "Source control",
                icon: SOURCE_CONTROL_ICON,
                badge: self.sc_badge(),
                badge_selector: "activity-badge",
                noun: "uncommitted",
            },
        ];
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(RAIL_WIDTH))
            .h_full()
            .pb(px(RAIL_BOTTOM))
            .debug_selector(|| "activity-rail".to_owned())
            .bg(gpui::rgb(SUNKEN))
            .border_r_1()
            .border_color(gpui::rgb(BORDER))
            .children(items.into_iter().map(|item| self.rail_item(item, cx)))
            .child(self.rail_dashboard_button(cx))
            .child(Self::rail_spacer(cx))
            .child(self.rail_recover_button(cx))
            .child(self.rail_settings_button(cx))
    }

    fn rail_item(&self, item: Item, cx: &mut Context<Self>) -> Stateful<Div> {
        let active = self.sidebar.activity() == item.activity;
        let collapsed = self.sidebar.is_collapsed();
        let activity = item.activity;
        let button = rail_button(item.selector, item.icon, active)
            .when_some(badge_text(item.badge), |row, text| {
                row.child(badge(item.badge_selector, text))
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.click_activity(activity, window, cx);
            }));
        let tip = item_tip(item.label, item.badge, item.noun, active, collapsed);
        self.with_rail_tip(button, item.selector, tip, cx)
    }

    /// The Dashboard button, under the panel items: it opens the Dashboard
    /// tab or shows the listed one, lit while that tab is the active one,
    /// with the Needs You badge's count.
    fn rail_dashboard_button(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let selector = "activity-dashboard";
        let count = self.needs_you_count();
        let button = rail_button(selector, DASHBOARD_ICON, self.dashboard_active())
            .when_some(badge_text(count), |row, text| {
                row.child(badge("dashboard-badge", text))
            })
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.open_or_focus_dashboard(window, cx);
            }));
        self.with_rail_tip(button, selector, dashboard_tip(count), cx)
    }

    /// The Recover sessions button, above Settings, with its badge counting
    /// the sessions lost and not yet recovered.
    fn rail_recover_button(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let selector = "activity-recover";
        let button = rail_button(selector, RECOVER_ICON, false)
            .when_some(
                badge_text(self.session_history.badge_count()),
                |row, text| row.child(badge("recover-badge", text)),
            )
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.toggle_recover(window, cx);
            }));
        self.with_rail_tip(button, selector, "Recover sessions", cx)
    }

    /// The Settings button, pinned to the rail's bottom.
    fn rail_settings_button(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let selector = "settings-open";
        let button = rail_button(selector, SETTINGS_ICON, false).on_click(cx.listener(
            |this, _: &ClickEvent, window, cx| {
                this.open_settings(window, cx);
            },
        ));
        self.with_rail_tip(button, selector, "Settings (Ctrl+,)", cx)
    }

    /// Gives the rail button tagged `selector` its tooltip `tip`, unless a
    /// press put the tooltip away and the pointer has not left the button
    /// since: see [`RailTipSuppression`].
    fn with_rail_tip(
        &self,
        button: Stateful<Div>,
        selector: &'static str,
        tip: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        button
            .when(self.rail_tip.offers_tip(selector), |button| {
                button.tooltip(tooltip(tip))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.rail_tip.press(selector);
                    cx.notify();
                }),
            )
            // Releases are caught in the capture phase, which no click
            // handler can stop.
            .capture_any_mouse_up(cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                if event.button == MouseButton::Left && this.rail_tip.release(selector, true) {
                    cx.notify();
                }
            }))
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseUpEvent, _, cx| {
                    if this.rail_tip.release(selector, false) {
                        cx.notify();
                    }
                }),
            )
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if this.rail_tip.hover_changed(selector, *hovered) {
                    cx.notify();
                }
            }))
    }

    /// The rail's spacer, which also listens for the pointer leaving the
    /// window: that sends no move, so no button hears it leave.
    fn rail_spacer(cx: &mut Context<Self>) -> impl IntoElement {
        let root = cx.entity();
        canvas(
            |_, _, _| {},
            move |_, (), window, _| {
                window.on_mouse_event(move |_: &MouseExitEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble {
                        root.update(cx, |this, cx| {
                            if this.rail_tip.pointer_left_window() {
                                cx.notify();
                            }
                        });
                    }
                });
            },
        )
        .flex_1()
    }

    /// A rail click: see [`crate::sidebar::SidebarModel::click_activity`].
    /// Folding the panel hands the keyboard back to the active pane.
    fn click_activity(&mut self, item: Activity, window: &mut Window, cx: &mut Context<Self>) {
        self.change_activity(|sidebar| sidebar.click_activity(item), window, cx);
    }

    /// Shows the sessions panel, unfolding it, as a rail click on Sessions
    /// would but without folding an open panel; the keyboard stays put.
    pub(crate) fn show_sessions_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.change_activity(
            |sidebar| sidebar.show_activity(Activity::Sessions),
            window,
            cx,
        );
    }

    /// Changes the rail's panel by `change`, closing the menus the change
    /// would strand.
    fn change_activity(
        &mut self,
        change: impl FnOnce(&mut SidebarModel),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_shell_menu(window, cx);
        self.close_sc_picker(window, cx);
        self.close_sc_file_menu(window, cx);
        self.close_more_menu(window, cx);
        let was_collapsed = self.sidebar.is_collapsed();
        change(&mut self.sidebar);
        // Only a fold or an unfold gives the panes a new width; a switch
        // between two open panels leaves it alone.
        if self.sidebar.is_collapsed() != was_collapsed {
            self.mark_active_panes_resized(cx);
        }
        // The Stop all chip and the container rows live only in the sessions
        // panel, so a switch away from it, or a fold, takes any arm and the
        // row focus and hover with it.
        if self.sidebar.is_collapsed() || self.sidebar.activity() != Activity::Sessions {
            self.detached_stop_all.disarm();
            self.row_focus = None;
            self.hovered_container = None;
        }
        self.drag = None;
        self.save_ui();
        if self.sidebar.is_collapsed() {
            self.focus_active_pane(window, cx);
        } else {
            self.seed_history();
        }
        self.arm_needs_you_repaint(cx);
        cx.notify();
    }
}

/// A rail button tagged `selector`: `icon` on a rounded tile, which is
/// filled and led by the accent bar while `active`, and lit on hover.
fn rail_button(selector: &'static str, icon: &'static str, active: bool) -> Stateful<Div> {
    let tile = div()
        .flex()
        .items_center()
        .justify_center()
        .size(px(TILE_SIZE))
        .rounded(px(TILE_RADIUS))
        .when(active, |tile| tile.bg(gpui::rgb(CHIP)))
        .when(!active, |tile| {
            tile.group_hover(selector, |style| style.bg(gpui::rgb(HOVER)))
        })
        .child(
            svg()
                .path(icon)
                .size(px(ICON_SIZE))
                .text_color(gpui::rgb(if active { TEXT } else { SUBTLE })),
        );
    div()
        .id(selector)
        .group(selector)
        .debug_selector(move || selector.to_owned())
        .relative()
        .flex()
        .flex_none()
        .justify_center()
        .items_center()
        .w_full()
        .h(px(ITEM_HEIGHT))
        .cursor_pointer()
        .when(active, |button| {
            button.child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(ACTIVE_BAR_INSET))
                    .bottom(px(ACTIVE_BAR_INSET))
                    .w(px(ACTIVE_BAR_WIDTH))
                    .rounded(px(ACTIVE_BAR_WIDTH))
                    .bg(gpui::rgb(ACCENT)),
            )
        })
        .child(tile)
}

fn badge(selector: &'static str, text: String) -> Div {
    div()
        .absolute()
        .right(px(BADGE_INSET))
        .top(px(BADGE_INSET))
        .flex()
        .items_center()
        .justify_center()
        .min_w(px(BADGE_HEIGHT))
        .h(px(BADGE_HEIGHT))
        .px(px(4.0))
        .rounded_full()
        .bg(gpui::rgb(HOVER))
        .border(px(BADGE_RING))
        .border_color(gpui::rgb(SUNKEN))
        .text_color(gpui::rgb(TEXT))
        .text_size(px(BADGE_TEXT_SIZE))
        .font_weight(FontWeight::BOLD)
        .debug_selector(move || selector.to_owned())
        .child(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressed_rail_item_suppresses_its_tooltip_until_hover_leaves() {
        let mut tips = RailTipSuppression::default();
        assert!(
            tips.offers_tip("a"),
            "every button offers its tooltip at first"
        );
        tips.press("a");
        assert!(!tips.offers_tip("a"), "a press puts the tooltip away");
        assert!(tips.offers_tip("b"), "only the pressed button's");
        assert!(!tips.release("a", true), "a release on the button");
        assert!(
            !tips.hover_changed("a", true),
            "hovering it again changes nothing"
        );
        assert!(
            !tips.hover_changed("b", false),
            "leaving another button changes nothing"
        );
        assert!(!tips.offers_tip("a"));
        assert!(tips.hover_changed("a", false), "leaving the pressed button");
        assert!(tips.offers_tip("a"), "brings its tooltip back");
        assert!(
            !tips.hover_changed("a", false),
            "a second leave changes nothing"
        );
    }

    #[test]
    fn a_held_press_is_lifted_only_by_a_leave_after_release_or_the_window() {
        let mut tips = RailTipSuppression::default();
        tips.press("a");
        assert!(!tips.hover_changed("a", false), "gpui's unhover while held");
        assert!(!tips.release("b", true), "another button's release");
        assert!(!tips.release("a", true), "a release on the button");
        assert!(!tips.offers_tip("a"), "keeps the tooltip away");
        assert!(tips.hover_changed("a", false), "until the pointer leaves");

        tips.press("a");
        assert!(
            tips.release("a", false),
            "a release off the button lifts it"
        );
        assert!(tips.offers_tip("a"));

        tips.press("a");
        assert!(
            tips.pointer_left_window(),
            "so does leaving the window, held"
        );
        assert!(tips.offers_tip("a"));
        assert!(
            !tips.pointer_left_window(),
            "with nothing put away, nothing"
        );
    }

    #[test]
    fn item_tip_uses_each_items_noun() {
        assert_eq!(
            item_tip("Needs You", 3, "waiting on you", false, false),
            "Needs You (3 waiting on you) — click to show (Ctrl+B)"
        );
        assert_eq!(
            item_tip("Source control", 2, "uncommitted", true, false),
            "Source control (2 uncommitted) — click to collapse sidebar (Ctrl+B)"
        );
        assert_eq!(
            item_tip("Sessions", 0, "", false, true),
            "Sessions — click to open (Ctrl+B)",
            "no badge, no count"
        );
    }
}
