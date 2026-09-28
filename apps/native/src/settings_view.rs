//! The Settings modal: a tab list on the left and the chosen tab's rows
//! beside it. General holds keep-awake and copy on select, Appearance the
//! appearance editor at the app level, App title the window title's
//! settings; every change applies at once. The main window's title, which
//! the App title tab controls, is kept here too.

use std::time::Duration;

use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, Keystroke, Stateful, Window, div, prelude::*,
    px,
};
use protocol::ClientMessage;

use crate::appearance_view::{Level, close_footer};
use crate::mouse::CopyOnSelect;
use crate::session_menu::{backdrop, dialog_button};
use crate::tabs::tab_session_counts;
use crate::window_title::compute_title;
use crate::{BORDER, HOVER_BG, MUTED, PANEL_BG, RootView, TEXT, UI_TEXT_SIZE};

const TAB_LIST_WIDTH: f32 = 120.0;
/// As wide as the Appearance tab's two columns, so the modal keeps its size
/// from tab to tab.
const CONTENT_WIDTH: f32 = 656.0;
/// How long the window title waits for its inputs to settle, so a session
/// flickering between working and idle does not flicker the taskbar.
const TITLE_DEBOUNCE: Duration = Duration::from_millis(350);
/// What the tabs no item fills yet show.
const COMING_SOON: &str = "Coming soon.";

/// A tab of the Settings modal, in the order the list shows them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum SettingsTab {
    #[default]
    General,
    Notifications,
    SpawnDefaults,
    Worktrees,
    Appearance,
    AppTitle,
}

impl SettingsTab {
    const ALL: [Self; 6] = [
        Self::General,
        Self::Notifications,
        Self::SpawnDefaults,
        Self::Worktrees,
        Self::Appearance,
        Self::AppTitle,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Notifications => "Notifications",
            Self::SpawnDefaults => "Spawn defaults",
            Self::Worktrees => "Worktrees",
            Self::Appearance => "Appearance",
            Self::AppTitle => "App title",
        }
    }

    const fn selector(self) -> &'static str {
        match self {
            Self::General => "settings-tab-general",
            Self::Notifications => "settings-tab-notifications",
            Self::SpawnDefaults => "settings-tab-spawn-defaults",
            Self::Worktrees => "settings-tab-worktrees",
            Self::Appearance => "settings-tab-appearance",
            Self::AppTitle => "settings-tab-app-title",
        }
    }

    /// The tab above (`down` false) or below this one, stopping at the ends.
    fn step(self, down: bool) -> Self {
        let at = Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0);
        let next = if down {
            (at + 1).min(Self::ALL.len() - 1)
        } else {
            at.saturating_sub(1)
        };
        Self::ALL[next]
    }
}

/// A control of a Settings tab that Tab can reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsControl {
    KeepAwake,
    CopyOnSelect,
    TitleCount,
    TitleSuffix,
}

impl SettingsControl {
    const fn selector(self) -> &'static str {
        match self {
            Self::KeepAwake => "settings-keep-awake-toggle",
            Self::CopyOnSelect => "settings-terminal-copy-on-selection",
            Self::TitleCount => "settings-title-busy-count",
            Self::TitleSuffix => "settings-title-product-suffix",
        }
    }
}

/// The daemon's keep-awake setting and whether it holds the machine awake
/// now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeepAwake {
    pub(crate) enabled: bool,
    pub(crate) active: bool,
}

/// What the keep-awake button offers.
const fn keep_awake_label(state: Option<KeepAwake>) -> &'static str {
    match state {
        Some(KeepAwake { enabled: true, .. }) => "Allow sleep while sessions run",
        _ => "Keep this machine awake while sessions run",
    }
}

/// The line beside the keep-awake button.
const fn keep_awake_status(state: Option<KeepAwake>) -> &'static str {
    match state {
        None => "(daemon not connected)",
        Some(KeepAwake {
            enabled: true,
            active: true,
        }) => "Holding the machine awake — a session is live.",
        Some(KeepAwake {
            enabled: true,
            active: false,
        }) => "Sleep is allowed until a session starts.",
        Some(KeepAwake { enabled: false, .. }) => "The machine may sleep while sessions run.",
    }
}

impl RootView {
    /// Whether the Settings modal is open.
    #[must_use]
    pub fn settings_open(&self) -> bool {
        self.appearance_editor
            .as_ref()
            .is_some_and(|editor| editor.level == Level::App)
    }

    /// The Settings tabs' labels, in the list's order.
    #[must_use]
    pub fn settings_tab_labels(&self) -> Vec<&'static str> {
        SettingsTab::ALL.iter().map(|tab| tab.label()).collect()
    }

    /// The label of the Settings tab shown.
    #[must_use]
    pub fn settings_tab(&self) -> &'static str {
        self.settings_tab.label()
    }

    /// The keep-awake button's label and whether it can be pressed.
    #[must_use]
    pub fn keep_awake_button(&self) -> (&'static str, bool) {
        (keep_awake_label(self.keep_awake), self.keep_awake.is_some())
    }

    /// The keep-awake status line.
    #[must_use]
    pub fn keep_awake_status(&self) -> &'static str {
        keep_awake_status(self.keep_awake)
    }

    /// The App title tab's preview: the title for a tab named "Tab name"
    /// with one of three terminals busy.
    #[must_use]
    pub fn title_preview(&self) -> String {
        let title = &self.sidebar.ui_state().title;
        let preview = compute_title(
            Some((1, 3)),
            Some("Tab name"),
            title.show_count,
            title.suffix,
        );
        if preview.is_empty() {
            "(empty)".to_owned()
        } else {
            preview
        }
    }

    /// What in the Settings frame has the keyboard: `"list"` for the tab
    /// list, else the focused control's selector.
    #[must_use]
    pub fn settings_focus(&self, window: &Window) -> Option<&'static str> {
        if self.settings_tabs_focus.is_focused(window) {
            return Some("list");
        }
        self.focused_settings_control(window)
            .map(SettingsControl::selector)
    }

    /// The shown tab's controls Tab reaches, in order; a disabled one is
    /// left out.
    fn settings_controls(&self) -> Vec<SettingsControl> {
        match self.settings_tab {
            SettingsTab::General if self.keep_awake.is_some() => {
                vec![SettingsControl::KeepAwake, SettingsControl::CopyOnSelect]
            }
            SettingsTab::General => vec![SettingsControl::CopyOnSelect],
            SettingsTab::AppTitle => {
                vec![SettingsControl::TitleCount, SettingsControl::TitleSuffix]
            }
            SettingsTab::Notifications
            | SettingsTab::SpawnDefaults
            | SettingsTab::Worktrees
            | SettingsTab::Appearance => Vec::new(),
        }
    }

    /// The control holding the keyboard, if one does.
    fn focused_settings_control(&self, window: &Window) -> Option<SettingsControl> {
        self.settings_control.filter(|control| {
            self.appearance_focus.is_focused(window) && self.settings_controls().contains(control)
        })
    }

    /// Moves the keyboard one step along the ring the tab list and the
    /// shown tab's controls make, backwards with `back`. From anywhere off
    /// the ring it goes to the tab list.
    fn step_settings_focus(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        let controls = self.settings_controls();
        let at = if self.settings_tabs_focus.is_focused(window) {
            Some(0)
        } else {
            self.focused_settings_control(window)
                .and_then(|control| controls.iter().position(|c| *c == control))
                .map(|index| index + 1)
        };
        let len = controls.len() + 1;
        let next = match at {
            None => 0,
            Some(at) if back => (at + len - 1) % len,
            Some(at) => (at + 1) % len,
        };
        if next == 0 {
            self.settings_control = None;
            self.settings_tabs_focus.focus(window);
        } else {
            self.settings_control = Some(controls[next - 1]);
            self.appearance_focus.focus(window);
        }
        cx.notify();
    }

    fn press_settings_control(
        &mut self,
        control: SettingsControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings_control = Some(control);
        self.appearance_focus.focus(window);
        match control {
            SettingsControl::KeepAwake => self.toggle_keep_awake(),
            SettingsControl::CopyOnSelect => self.toggle_copy_on_select(cx),
            SettingsControl::TitleCount => self.toggle_title_count(window, cx),
            SettingsControl::TitleSuffix => self.toggle_title_suffix(window, cx),
        }
        cx.notify();
    }

    /// The title last set on the window; empty until the first is.
    #[must_use]
    pub fn window_title(&self) -> &str {
        &self.window_title
    }

    /// Ctrl+, or the gear: the Settings modal on its General tab, with the
    /// keyboard on the tab list.
    pub(crate) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_appearance_editor(Level::App, window, cx);
        if self.settings_open() {
            self.settings_tab = SettingsTab::General;
            self.settings_control = None;
            self.settings_tabs_focus.focus(window);
        }
    }

    fn select_settings_tab(
        &mut self,
        tab: SettingsTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings_tab = tab;
        self.settings_control = None;
        self.settings_tabs_focus.focus(window);
        cx.notify();
    }

    /// A key while Settings is open; returns whether it was the frame's.
    /// Tab and Shift+Tab walk the ring of the tab list and the shown tab's
    /// controls, Up and Down on the tab list change the tab, and Space or
    /// Enter presses the focused control. On Appearance, a key anywhere
    /// but the tab list is the appearance editor's.
    pub(crate) fn on_settings_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mods = &keystroke.modifiers;
        if !self.settings_open() || mods.control || mods.alt || mods.platform || mods.function {
            return false;
        }
        let on_list = self.settings_tabs_focus.is_focused(window);
        if self.settings_tab == SettingsTab::Appearance && !on_list {
            return false;
        }
        match (keystroke.key.as_str(), mods.shift) {
            ("tab", back) => {
                self.step_settings_focus(back, window, cx);
                true
            }
            ("up" | "down", false) if on_list => {
                let tab = self.settings_tab.step(keystroke.key == "down");
                self.select_settings_tab(tab, window, cx);
                true
            }
            ("space" | "enter", false) => match self.focused_settings_control(window) {
                Some(control) => {
                    self.press_settings_control(control, window, cx);
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    /// The daemon's keep-awake state, or `None` when this connection has
    /// not reported it.
    pub(crate) fn set_keep_awake(&mut self, state: Option<KeepAwake>) {
        self.keep_awake = state;
    }

    fn toggle_keep_awake(&mut self) {
        if let Some(state) = self.keep_awake {
            self.send(ClientMessage::SetKeepAwake {
                enabled: !state.enabled,
            });
        }
    }

    fn toggle_copy_on_select(&mut self, cx: &mut Context<Self>) {
        let on = !self.sidebar.ui_state().general.copy_on_select;
        self.sidebar.set_copy_on_select(on);
        cx.set_global(CopyOnSelect(on));
        self.save_ui();
        cx.notify();
    }

    fn toggle_title_count(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let on = !self.sidebar.ui_state().title.show_count;
        self.sidebar.set_title_show_count(on);
        self.after_title_setting(window, cx);
    }

    fn toggle_title_suffix(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let on = !self.sidebar.ui_state().title.suffix;
        self.sidebar.set_title_suffix(on);
        self.after_title_setting(window, cx);
    }

    fn after_title_setting(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_ui();
        self.refresh_window_title(window, cx);
        cx.notify();
    }

    /// The title the window should show now: the active tab's name and
    /// counts as the App title settings shape them.
    fn wanted_window_title(&self) -> String {
        let title = &self.sidebar.ui_state().title;
        let tab = self.tabs.active_tab();
        let counts = tab.and_then(|tab| tab_session_counts(tab, self.sidebar.sessions()));
        compute_title(
            counts,
            tab.map(|tab| tab.name.as_str()),
            title.show_count,
            title.suffix,
        )
    }

    /// Sets the window title [`TITLE_DEBOUNCE`] after it last changed; a
    /// change inside the wait restarts it.
    pub(crate) fn refresh_window_title(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wanted = self.wanted_window_title();
        let waiting = self.title_pending.as_ref().unwrap_or(&self.window_title);
        if *waiting == wanted {
            return;
        }
        if wanted == self.window_title {
            self.title_pending = None;
            self.title_timer = None;
            return;
        }
        self.title_pending = Some(wanted);
        self.title_timer = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(TITLE_DEBOUNCE).await;
            // Fails only when the view is gone, and its window with it.
            this.update_in(cx, |this, window, _| this.apply_window_title(window))
                .ok();
        }));
    }

    fn apply_window_title(&mut self, window: &mut Window) {
        self.title_timer = None;
        if let Some(title) = self.title_pending.take() {
            window.set_window_title(&title);
            self.window_title = title;
        }
    }

    /// The Settings modal over a backdrop that takes every click beneath
    /// it; a click on the backdrop itself does nothing.
    pub(crate) fn settings_layer(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
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
        let body = div()
            .flex()
            .gap(px(12.0))
            .child(self.settings_tab_list(window, cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(CONTENT_WIDTH))
                    .children(self.settings_content(window, cx)),
            );
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

    /// The tab list; the shown tab is filled, and outlined while the list
    /// has the keyboard.
    fn settings_tab_list(&self, window: &Window, cx: &mut Context<Self>) -> Stateful<Div> {
        let list_focused = self.settings_tabs_focus.is_focused(window);
        let tabs = SettingsTab::ALL.map(|tab| {
            let shown = tab == self.settings_tab;
            let selector = tab.selector();
            div()
                .id(selector)
                .debug_selector(|| selector.to_owned())
                .px(px(8.0))
                .py(px(4.0))
                .rounded(px(4.0))
                .border_1()
                .border_color(gpui::rgb(if shown && list_focused {
                    TEXT
                } else {
                    PANEL_BG
                }))
                .cursor_pointer()
                .when(shown, |row| row.bg(gpui::rgb(HOVER_BG)))
                .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.select_settings_tab(tab, window, cx);
                }))
                .child(tab.label())
        });
        div()
            .id("settings-tabs")
            .track_focus(&self.settings_tabs_focus)
            .flex()
            .flex_col()
            .flex_none()
            .w(px(TAB_LIST_WIDTH))
            .pr(px(8.0))
            .border_r_1()
            .border_color(gpui::rgb(BORDER))
            .children(tabs)
    }

    fn settings_content(&self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let focused = self.focused_settings_control(window);
        match self.settings_tab {
            SettingsTab::General => self.general_tab(focused, cx),
            SettingsTab::Appearance => self
                .appearance_body(cx)
                .map(IntoElement::into_any_element)
                .into_iter()
                .collect(),
            SettingsTab::AppTitle => self.app_title_tab(focused, cx),
            SettingsTab::Notifications | SettingsTab::SpawnDefaults | SettingsTab::Worktrees => {
                vec![hint(COMING_SOON).into_any_element()]
            }
        }
    }

    fn general_tab(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let (label, enabled) = self.keep_awake_button();
        let keep_awake = SettingsControl::KeepAwake;
        let button = dialog_button(
            keep_awake.selector(),
            label.to_owned(),
            false,
            focused == Some(keep_awake),
        );
        let button = if enabled {
            button.on_click(press(keep_awake, cx))
        } else {
            button.opacity(0.5).cursor_default()
        };
        let status = hint(self.keep_awake_status())
            .debug_selector(|| "settings-keep-awake-status".to_owned());
        let power = section("Power").child(
            div()
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(button)
                .child(status),
        );
        let copy = toggle(
            SettingsControl::CopyOnSelect,
            self.sidebar.ui_state().general.copy_on_select,
            "Copy selection to clipboard automatically",
            focused,
            cx,
        );
        let terminal = section("Terminal behavior").child(copy).child(hint(
            "Off: select to highlight, copy explicitly with Ctrl+C \
             (when there's a selection) or Ctrl+Shift+C.",
        ));
        vec![power.into_any_element(), terminal.into_any_element()]
    }

    fn app_title_tab(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let title = &self.sidebar.ui_state().title;
        let count = toggle(
            SettingsControl::TitleCount,
            title.show_count,
            "Show busy/total terminal count",
            focused,
            cx,
        );
        let suffix = toggle(
            SettingsControl::TitleSuffix,
            title.suffix,
            "Append “ — rustling-tulip” suffix",
            focused,
            cx,
        );
        let preview = div().flex().gap(px(10.0)).child("Preview").child(
            div()
                .debug_selector(|| "settings-title-preview".to_owned())
                .px(px(6.0))
                .rounded(px(3.0))
                .bg(gpui::rgb(HOVER_BG))
                .child(self.title_preview()),
        );
        let section = section("App title")
            .child(hint(
                "Controls what the OS window title bar shows for the main window.",
            ))
            .child(count)
            .child(hint(
                "Renders (M/N) before the tab name where M is the number of non-idle \
                 terminals in the active tab and N is the total terminal count. Hidden \
                 when the tab has no terminals.",
            ))
            .child(suffix)
            .child(hint(
                "Keeps OS taskbars grouping rustling-tulip windows together. Turn off \
                 for a tighter title.",
            ))
            .child(preview);
        vec![section.into_any_element()]
    }
}

/// A titled group of rows.
fn section(title: &'static str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .pb(px(12.0))
        .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
}

/// A muted line under or beside a control.
fn hint(text: &'static str) -> Div {
    div().text_color(gpui::rgb(MUTED)).child(text)
}

/// A click handler that presses `control`.
fn press(
    control: SettingsControl,
    cx: &mut Context<RootView>,
) -> impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static {
    cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.press_settings_control(control, window, cx);
    })
}

/// `control`'s checkbox row, ticked when `checked` and outlined when it
/// has the keyboard.
fn toggle(
    control: SettingsControl,
    checked: bool,
    label: &'static str,
    focused: Option<SettingsControl>,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let selector = control.selector();
    div()
        .id(selector)
        .debug_selector(|| selector.to_owned())
        .flex()
        .items_center()
        .gap(px(6.0))
        .px(px(4.0))
        .py(px(2.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(if focused == Some(control) {
            TEXT
        } else {
            PANEL_BG
        }))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .on_click(press(control, cx))
        .child(if checked { "☑" } else { "☐" })
        .child(label)
}
