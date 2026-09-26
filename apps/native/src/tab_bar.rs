//! The tab strip: a pill per tab (click to show it, double-click to rename,
//! middle-click or × to close, twice when the tab holds something), then "+"
//! for a new tab.

use gpui::{
    AnyElement, ClickEvent, Context, Div, ElementId, Entity, Focusable as _, Keystroke,
    MouseButton, MouseDownEvent, Pixels, Point, SharedString, Stateful, Subscription, Window,
    anchored, deferred, div, prelude::*, px,
};
use protocol::{ClientMessage, RearrangeLayout, TabContent, TabEntry};

use crate::fonts;
use crate::session_menu::{menu_frame, menu_item, muted_row};
use crate::tabs::collect_panes;
use crate::text_input::{TextInput, TextInputEvent};
use crate::{
    BAR_BG, BORDER, DANGER, HOVER_BG, MUTED, PANEL_BG, RootView, TEXT, UI_TEXT_SIZE, tooltip,
};

const TAB_BAR_HEIGHT: f32 = 26.0;
const RENAME_WIDTH: f32 = 140.0;

/// A tab shortcut, before the check whether it has anything to do.
#[derive(Debug, PartialEq, Eq)]
enum TabKey {
    /// Ctrl+Tab, or Ctrl+Shift+Tab going `back`.
    Cycle { back: bool },
    /// Ctrl+1–9: the tab at this index.
    Nth(usize),
    /// Ctrl+T, or Ctrl+Shift+T that acts `anywhere`, the terminals included.
    New { anywhere: bool },
    /// Ctrl+Shift+G.
    Grid,
}

/// Which tab shortcut `ks` is, if any; Alt or the platform key rules all
/// of them out.
fn tab_key(ks: &Keystroke) -> Option<TabKey> {
    let mods = &ks.modifiers;
    if !mods.control || mods.alt || mods.platform {
        return None;
    }
    let key = ks.key.to_ascii_lowercase();
    match key.as_str() {
        "tab" => Some(TabKey::Cycle { back: mods.shift }),
        "t" => Some(TabKey::New {
            anywhere: mods.shift,
        }),
        "g" if mods.shift => Some(TabKey::Grid),
        _ if !mods.shift => key
            .parse::<usize>()
            .ok()
            .filter(|digit| (1..=9).contains(digit))
            .map(|digit| TabKey::Nth(digit - 1)),
        _ => None,
    }
}

/// The open tab context menu: the tab's font size.
pub(crate) struct TabMenu {
    /// The tab it acts on.
    tab_id: String,
    /// Where the right-click was, in window coordinates.
    at: Point<Pixels>,
}

impl RootView {
    /// The tab whose context menu is open.
    #[must_use]
    pub fn tab_menu(&self) -> Option<&str> {
        self.tab_menu.as_ref().map(|menu| menu.tab_id.as_str())
    }

    /// The size the open menu's header shows.
    #[must_use]
    pub fn tab_menu_size(&self) -> Option<f32> {
        let menu = self.tab_menu.as_ref()?;
        Some(self.resolved_tab_font_size(&menu.tab_id))
    }

    /// The selectors of the open menu's rows.
    #[must_use]
    pub fn tab_menu_rows(&self) -> Vec<String> {
        if self.tab_menu.is_none() {
            return Vec::new();
        }
        ["tab-menu-increase", "tab-menu-decrease", "tab-menu-reset"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    /// The size stored as `tab_id`'s override, if the tab has one.
    #[must_use]
    pub fn tab_font_override(&self, tab_id: &str) -> Option<f32> {
        self.sidebar.tab_font_size(tab_id)
    }

    /// The size a pane of `tab_id` draws at: the tab's override, else the
    /// size its focused pane's session resolves to.
    #[must_use]
    pub fn resolved_tab_font_size(&self, tab_id: &str) -> f32 {
        let session = self
            .tabs
            .focused_pane(tab_id)
            .and_then(|pane_id| self.pane_session(&pane_id));
        self.sidebar
            .resolved_font_size(self.sidebar.tab_font_size(tab_id), session.as_deref())
    }

    /// Opens `tab_id`'s context menu at `at`, taking the keyboard so Esc
    /// reaches it. The session menu gives way to it.
    pub(crate) fn open_tab_menu(
        &mut self,
        tab_id: &str,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tabs.tab(tab_id).is_none() {
            return;
        }
        self.menu = None;
        self.tab_menu = Some(TabMenu {
            tab_id: tab_id.to_owned(),
            at,
        });
        self.tab_menu_focus.focus(window);
        cx.notify();
    }

    /// Drops the menu of a tab the daemon no longer lists.
    pub(crate) fn drop_stale_tab_menu(&mut self) {
        if self
            .tab_menu
            .as_ref()
            .is_some_and(|menu| self.tabs.tab(&menu.tab_id).is_none())
        {
            self.tab_menu = None;
        }
    }

    /// Closes the menu and hands the keyboard back to the active pane.
    pub(crate) fn close_tab_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tab_menu.take().is_some() {
            self.focus_active_pane(window, cx);
            cx.notify();
        }
    }

    /// Steps `tab_id`'s override one `delta` from the size its panes resolve
    /// to; at a clamp limit nothing changes. The panes follow at once; the
    /// layout is written once the steps settle.
    pub(crate) fn bump_tab_font(&mut self, tab_id: &str, delta: f32, cx: &mut Context<Self>) {
        let Some(next) = fonts::stepped(self.resolved_tab_font_size(tab_id), delta) else {
            return;
        };
        self.sidebar.set_tab_font_size(tab_id, next);
        self.after_tab_font_change(cx);
    }

    /// Drops `tab_id`'s override.
    pub(crate) fn reset_tab_font(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        if self.sidebar.clear_tab_font_size(tab_id) {
            self.after_tab_font_change(cx);
        }
    }

    /// Re-applies the panes' fonts and schedules the layout write, so a run
    /// of tab font changes saves at most once per debounce.
    fn after_tab_font_change(&mut self, cx: &mut Context<Self>) {
        self.apply_pane_fonts(cx);
        self.schedule_ui_save(cx);
        cx.notify();
    }

    /// The open tab menu over a layer that keeps a click outside it from
    /// reaching what lies beneath.
    pub(crate) fn tab_menu_layer(&self, cx: &mut Context<Self>) -> Option<[AnyElement; 2]> {
        let menu = self.tab_menu.as_ref()?;
        self.tabs.tab(&menu.tab_id)?;
        let backdrop = div().absolute().top_0().left_0().size_full().occlude();
        let panel = anchored()
            .position(menu.at)
            .snap_to_window()
            .child(self.tab_menu_panel(menu, cx));
        Some([
            backdrop.into_any_element(),
            deferred(panel).with_priority(1).into_any_element(),
        ])
    }

    fn tab_menu_panel(&self, menu: &TabMenu, cx: &mut Context<Self>) -> Stateful<Div> {
        let size = self.resolved_tab_font_size(&menu.tab_id);
        let overridden = self.sidebar.tab_font_size(&menu.tab_id).is_some();
        let can_grow = fonts::stepped(size, 1.0).is_some();
        let can_shrink = fonts::stepped(size, -1.0).is_some();
        let (grow, shrink, reset) = (
            menu.tab_id.clone(),
            menu.tab_id.clone(),
            menu.tab_id.clone(),
        );
        menu_frame(
            "tab-menu",
            &self.tab_menu_focus,
            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                this.close_tab_menu(window, cx);
                cx.stop_propagation();
            }),
        )
        .child(muted_row(format!("Font size: {size}")))
        .child(
            tab_menu_row("tab-menu-increase", "Increase", can_grow).when(can_grow, |row| {
                row.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.bump_tab_font(&grow, 1.0, cx);
                }))
            }),
        )
        .child(
            tab_menu_row("tab-menu-decrease", "Decrease", can_shrink).when(can_shrink, |row| {
                row.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.bump_tab_font(&shrink, -1.0, cx);
                }))
            }),
        )
        .child(
            tab_menu_row("tab-menu-reset", "Reset", overridden).when(overridden, |row| {
                row.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.reset_tab_font(&reset, cx);
                    this.close_tab_menu(window, cx);
                }))
            }),
        )
    }
}

/// A row of the tab menu; a disabled one is inert, as an action still on the
/// way is in the session menu.
fn tab_menu_row(selector: &str, label: &'static str, enabled: bool) -> Stateful<Div> {
    let row = menu_item(selector, label, false);
    if enabled {
        row
    } else {
        row.opacity(0.6).cursor_default()
    }
}

/// A tab name being edited in place.
pub(crate) struct Rename {
    tab_id: String,
    input: Entity<TextInput>,
    _subscriptions: [Subscription; 2],
}

impl Rename {
    pub(crate) fn tab_id(&self) -> &str {
        &self.tab_id
    }
}

/// How a rename ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenameEnd {
    Submit,
    Cancel,
    /// The input lost the keyboard to something the user chose.
    Blur,
}

impl RootView {
    pub(crate) fn tab_bar(&self, cx: &mut Context<Self>) -> Div {
        let pills: Vec<AnyElement> = self
            .tabs
            .tabs()
            .iter()
            .map(|tab| self.tab_pill(tab, cx))
            .collect();
        div()
            .flex()
            .flex_none()
            .items_center()
            .h(px(TAB_BAR_HEIGHT))
            .bg(gpui::rgb(BAR_BG))
            .border_b_1()
            .border_color(gpui::rgb(BORDER))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(MUTED))
            .children(pills)
            .child(new_tab_button(cx))
    }

    fn tab_pill(&self, tab: &TabEntry, cx: &mut Context<Self>) -> AnyElement {
        let active = self.tabs.active_id() == Some(tab.id.as_str());
        let armed = self.tabs.close_confirm.armed() == Some(tab.id.as_str());
        let rename = self.renaming.as_ref().filter(|r| r.tab_id == tab.id);
        let label = match rename {
            Some(rename) => div().w(px(RENAME_WIDTH)).child(rename.input.clone()),
            None => div().whitespace_nowrap().child(pill_label(tab)),
        };
        let (click_id, middle_id, menu_id) = (tab.id.clone(), tab.id.clone(), tab.id.clone());
        let on_click = cx.listener(move |this, event: &ClickEvent, window, cx| {
            this.click_tab(&click_id, event.click_count(), window, cx);
        });
        let on_middle = cx.listener(move |this, _: &MouseDownEvent, _, cx| {
            this.close_tab_click(&middle_id);
            cx.stop_propagation();
            cx.notify();
        });
        let name = format!("tab-{}", tab.id);
        div()
            .id(ElementId::Name(SharedString::from(name.clone())))
            .debug_selector(|| name)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.open_tab_menu(&menu_id, event.position, window, cx);
                    cx.stop_propagation();
                }),
            )
            .flex()
            .items_center()
            .gap(px(6.0))
            .h_full()
            .px(px(10.0))
            .border_r_1()
            .border_color(gpui::rgb(BORDER))
            .cursor_pointer()
            .when(active, |pill| {
                pill.bg(gpui::rgb(PANEL_BG)).text_color(gpui::rgb(TEXT))
            })
            .when(!active, |pill| {
                pill.hover(|style| style.bg(gpui::rgb(HOVER_BG)))
            })
            .when(rename.is_none(), move |pill| {
                pill.on_click(on_click)
                    .on_mouse_down(MouseButton::Middle, on_middle)
            })
            .child(label)
            .child(close_button(&tab.id, armed, cx))
            .into_any_element()
    }

    fn click_tab(
        &mut self,
        tab_id: &str,
        click_count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if click_count >= 2 {
            self.start_rename(tab_id, window, cx);
        } else {
            self.tabs.activate(tab_id);
            self.after_tabs_change(window, cx);
        }
    }

    /// The tab shortcuts; returns whether `ks` was one with something to
    /// do. Ctrl+(Shift+)Tab cycles the tabs, Ctrl+1–9 shows the Nth and
    /// Ctrl+Shift+G rearranges the active grid, from the terminals too;
    /// Ctrl+T opens a tab outside the terminals and Ctrl+Shift+T anywhere.
    /// None fires while a menu or a popup is open.
    pub(crate) fn on_tab_shortcut(
        &mut self,
        ks: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.menu_open() {
            return false;
        }
        match tab_key(ks) {
            Some(TabKey::Cycle { back }) => self.cycle_tab(back, window, cx),
            Some(TabKey::Nth(index)) => self.show_nth_tab(index, window, cx),
            Some(TabKey::New { anywhere }) => {
                if !anywhere && !self.outside_terminal(window, cx) {
                    return false;
                }
                self.new_tab();
                true
            }
            Some(TabKey::Grid) => self.rearrange_active_grid(),
            None => false,
        }
    }

    /// Shows the tab after the active one, or before it when `back`,
    /// wrapping; with fewer than two tabs there is nothing to do.
    fn cycle_tab(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let tabs = self.tabs.tabs();
        let count = tabs.len();
        if count < 2 {
            return false;
        }
        let current = self
            .tabs
            .active_id()
            .and_then(|id| tabs.iter().position(|tab| tab.id == id));
        let next = match (current, back) {
            (Some(at), false) => (at + 1) % count,
            (Some(at), true) => (at + count - 1) % count,
            (None, _) => 0,
        };
        let tab_id = tabs[next].id.clone();
        self.show_tab(&tab_id, window, cx);
        true
    }

    /// Shows the tab at `index` in strip order, when there is one.
    fn show_nth_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(tab_id) = self.tabs.tabs().get(index).map(|tab| tab.id.clone()) else {
            return false;
        };
        self.show_tab(&tab_id, window, cx);
        true
    }

    /// A context menu or a popup Esc closes is open.
    fn menu_open(&self) -> bool {
        self.tab_menu.is_some()
            || self.menu.is_some()
            || self.container_menu.is_some()
            || self.shell_menu.is_some()
            || self.sc_picker_open
            || self.changes.file_menu.is_some()
            || self.flyout_open
    }

    /// Shows `tab_id` as a click on its pill does.
    fn show_tab(&mut self, tab_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.tabs.activate(tab_id);
        self.after_tabs_change(window, cx);
    }

    /// Asks the daemon to lay the active tab out as a grid, when it is a
    /// grid of two panes or more.
    fn rearrange_active_grid(&self) -> bool {
        let Some(tab) = self.tabs.active_tab() else {
            return false;
        };
        let Some(grid) = tab.grid() else {
            return false;
        };
        if collect_panes(grid).len() < 2 {
            return false;
        }
        self.send(ClientMessage::RearrangeTab {
            tab_id: tab.id.clone(),
            layout: RearrangeLayout::Grid { cols: 0 },
        });
        true
    }

    fn new_tab(&mut self) {
        self.tabs.arm_create();
        self.send(ClientMessage::CreateTab {
            name: None,
            initial_session_id: None,
        });
    }

    /// A close click: closes the tab, or arms the close when the tab holds
    /// a session or a split.
    fn close_tab_click(&mut self, tab_id: &str) {
        let Some(tab) = self.tabs.tab(tab_id).cloned() else {
            return;
        };
        if self.tabs.close_confirm.click(&tab) {
            self.send(ClientMessage::CloseTab { tab_id: tab.id });
        }
    }

    fn start_rename(&mut self, tab_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.tabs.tab(tab_id).map(|tab| tab.name.clone()) else {
            return;
        };
        let input = cx.new(|cx| TextInput::new(name, "Tab name", cx));
        let events = cx.subscribe_in(
            &input,
            window,
            |this, _, event: &TextInputEvent, window, cx| {
                let end = match event {
                    TextInputEvent::Submit => RenameEnd::Submit,
                    TextInputEvent::Cancel => RenameEnd::Cancel,
                };
                this.finish_rename(end, window, cx);
            },
        );
        let handle = input.read(cx).focus_handle(cx);
        let blur = cx.on_blur(&handle, window, |this, window, cx| {
            this.finish_rename(RenameEnd::Blur, window, cx);
        });
        handle.focus(window);
        self.renaming = Some(Rename {
            tab_id: tab_id.to_owned(),
            input,
            _subscriptions: [events, blur],
        });
        cx.notify();
    }

    /// Ends a rename. Enter and a blur keep a changed, non-blank name; Enter
    /// and Esc hand the keyboard back to the active tab's pane.
    fn finish_rename(&mut self, end: RenameEnd, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rename) = self.renaming.take() else {
            return;
        };
        let name = rename.input.read(cx).text().trim().to_owned();
        let changed = self
            .tabs
            .tab(&rename.tab_id)
            .is_some_and(|tab| tab.name != name);
        if end != RenameEnd::Cancel && changed && !name.is_empty() {
            self.send(ClientMessage::RenameTab {
                tab_id: rename.tab_id,
                name,
            });
        }
        if end != RenameEnd::Blur {
            self.focus_active_pane(window, cx);
        }
        cx.notify();
    }
}

/// A tab's name; a diff tab's is marked Δ.
fn pill_label(tab: &TabEntry) -> String {
    match tab.content {
        TabContent::Diff { .. } => format!("Δ {}", tab.name),
        TabContent::Grid { .. } => tab.name.clone(),
    }
}

/// The ×, or ✓ while a close waits for its second click. It acts on the
/// press so the root's click-elsewhere reset never sees it.
fn close_button(tab_id: &str, armed: bool, cx: &mut Context<RootView>) -> Stateful<Div> {
    let id = tab_id.to_owned();
    let tip = if armed {
        "Click again to confirm closing this tab"
    } else {
        "Close tab"
    };
    let name = format!("tab-close-{tab_id}");
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .px(px(3.0))
        .rounded(px(3.0))
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .when(armed, |button| button.text_color(gpui::rgb(DANGER)))
        .tooltip(tooltip(tip))
        .child(if armed { "✓" } else { "×" })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                this.close_tab_click(&id);
                cx.stop_propagation();
                cx.notify();
            }),
        )
}

fn new_tab_button(cx: &mut Context<RootView>) -> Stateful<Div> {
    div()
        .id("new-tab")
        .debug_selector(|| "new-tab".to_owned())
        .px(px(10.0))
        .h_full()
        .flex()
        .items_center()
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)).text_color(gpui::rgb(TEXT)))
        .tooltip(tooltip("New tab"))
        .child("+")
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            this.new_tab();
            cx.notify();
        }))
}
