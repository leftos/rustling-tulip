//! The Settings modal: a tab list on the left, whose one tab, Appearance,
//! holds the appearance editor at the app level.

use gpui::{AnyElement, ClickEvent, Context, FontWeight, Window, div, prelude::*, px};

use crate::appearance_view::{Level, close_footer};
use crate::session_menu::{backdrop, dialog_button};
use crate::{BORDER, HOVER_BG, PANEL_BG, RootView, TEXT, UI_TEXT_SIZE};

const TAB_LIST_WIDTH: f32 = 120.0;

impl RootView {
    /// Whether the Settings modal is open.
    #[must_use]
    pub fn settings_open(&self) -> bool {
        self.appearance_editor
            .as_ref()
            .is_some_and(|editor| editor.level == Level::App)
    }

    /// Ctrl+, or the gear: the Settings modal on its Appearance tab.
    pub(crate) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_appearance_editor(Level::App, window, cx);
    }

    /// The Settings modal over a backdrop that takes every click beneath
    /// it; a click on the backdrop itself does nothing.
    pub(crate) fn settings_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.settings_open() {
            return None;
        }
        let close =
            dialog_button("settings-close", "×".to_owned(), false, false).on_click(cx.listener(
                |this, _: &ClickEvent, window, cx| this.close_appearance_editor(window, cx),
            ));
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Settings"))
            .child(close);
        let tab = div()
            .id("settings-tab-appearance")
            .debug_selector(|| "settings-tab-appearance".to_owned())
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(4.0))
            .bg(gpui::rgb(HOVER_BG))
            .child("Appearance");
        let tabs = div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(TAB_LIST_WIDTH))
            .pr(px(8.0))
            .border_r_1()
            .border_color(gpui::rgb(BORDER))
            .child(tab);
        let body = div()
            .flex()
            .gap(px(12.0))
            .child(tabs)
            .children(self.appearance_body(cx));
        let panel = div()
            .id("settings-panel")
            .track_focus(&self.appearance_focus)
            .flex()
            .flex_col()
            .gap(px(10.0))
            .p(px(14.0))
            .bg(gpui::rgb(PANEL_BG))
            .border_1()
            .border_color(gpui::rgb(BORDER))
            .rounded(px(6.0))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(TEXT))
            .child(header)
            .child(body)
            .child(close_footer("settings-footer-close", cx));
        Some(backdrop("settings", panel))
    }
}
