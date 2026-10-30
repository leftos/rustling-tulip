//! The tab strip: a pill per tab (click to show it, double-click to rename,
//! middle-click or × to close, twice when the tab holds something), then "+"
//! for a new tab.

use gpui::{
    AnyElement, App, ClickEvent, Context, CursorStyle, Div, DragMoveEvent, ElementId, Entity,
    Focusable as _, Keystroke, MouseButton, MouseDownEvent, Pixels, Point, SharedString, Stateful,
    Subscription, Window, anchored, deferred, div, prelude::*, px, svg,
};
use protocol::{
    ClientMessage, MergeLayout, RearrangeLayout, SessionMode, SessionStatus, TabContent, TabEntry,
};

use crate::activity_bar::badge_text;
use crate::assets::{DASHBOARD_ICON, TAB_CLOSE_ICON, TAB_NEW_ICON};
use crate::drag::{self, DragPayload, Side};
use crate::fonts;
use crate::overlay_owner::Overlay;
use crate::palette::{ACCENT, CHIP, GROUND, LINE_STRONG, RAISED, SUBTLE, SURFACE, WORKING};
use crate::popouts::PopoutKey;
use crate::session_menu::{MENU_MAX_SHARE, menu_frame, menu_item, menu_separator, muted_row};
use crate::status_glyph::{GlyphSize, glyph, glyph_view};
use crate::tab_menu::{MenuLine, TabAction, head_lines, merge_lines, rearrange_lines};
use crate::tabs::{PillClick, bound_pane_count, collect_panes, tab_session_counts};
use crate::text_input::{TextInput, TextInputEvent};
use crate::undo;
use crate::viewer_tab::display_name;
use crate::{BORDER, DANGER, HOVER_BG, MUTED, RootView, TEXT, UI_TEXT_SIZE, tooltip};

const TAB_BAR_HEIGHT: f32 = 38.0;
const RENAME_WIDTH: f32 = 140.0;
/// The busy pill's height and text size.
const BUSY_PILL_HEIGHT: f32 = 18.0;
const BUSY_PILL_TEXT_SIZE: f32 = 10.5;
/// The size of a pill's close icon and of the new-tab icon.
const CLOSE_ICON_SIZE: f32 = 12.0;
const NEW_TAB_ICON_SIZE: f32 = 15.0;
/// The size of the kind icon before a Dashboard tab's name.
const TAB_ICON_SIZE: f32 = 13.0;
/// The alpha of the accent wash on a selected pill, about 14%.
const SELECTED_WASH_ALPHA: u32 = 0x24;

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

/// The open tab context menu: rename, rearrange, font size, close and
/// merge.
pub(crate) struct TabMenu {
    /// The tab it acts on.
    tab_id: String,
    /// Where the right-click was, in window coordinates.
    at: Point<Pixels>,
    /// Whether the Grid submenu replaces the rows.
    grid_open: bool,
    /// Whether Close other tabs waits for its second press.
    close_others_armed: bool,
    /// The most height its frame may take, from the window it opened in.
    max_height: Pixels,
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
        self.tab_menu
            .as_ref()
            .map(|menu| {
                self.tab_menu_lines(menu)
                    .iter()
                    .filter_map(MenuLine::selector)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The text of the open menu's labels and rows, in order.
    #[must_use]
    pub fn tab_menu_text(&self) -> Vec<String> {
        let Some(menu) = &self.tab_menu else {
            return Vec::new();
        };
        self.tab_menu_lines(menu)
            .into_iter()
            .filter_map(|line| match line {
                MenuLine::Label(text) | MenuLine::Row { label: text, .. } => Some(text),
                MenuLine::Separator => None,
            })
            .collect()
    }

    /// The selected tabs, in strip order.
    #[must_use]
    pub fn selected_tabs(&self) -> Vec<String> {
        self.tabs
            .selection
            .in_order(self.tabs.tabs())
            .into_iter()
            .map(|tab| tab.id.clone())
            .collect()
    }

    /// The `(busy, total)` badge `tab_id`'s pill shows; `None` when it
    /// shows none: a diff tab, or no live session in its panes.
    #[must_use]
    pub fn tab_badge(&self, tab_id: &str) -> Option<(usize, usize)> {
        let tab = self.tabs.tab(tab_id)?;
        tab_session_counts(tab, self.sidebar.sessions()).filter(|(_, total)| *total > 0)
    }

    /// Every line of `menu`: the Grid submenu alone while it is open, else
    /// rename (not for a viewer tab, whose pill shows its file's name), the
    /// rearrange section, the font section, the close rows and the merge
    /// section.
    fn tab_menu_lines(&self, menu: &TabMenu) -> Vec<MenuLine> {
        let tabs = self.tabs.tabs();
        let Some(tab) = self.tabs.tab(&menu.tab_id) else {
            return Vec::new();
        };
        let bound = tab.grid().map_or(0, bound_pane_count);
        let rearrange = rearrange_lines(bound, self.pane_area_aspect(), menu.grid_open);
        let submenu = rearrange
            .iter()
            .any(|line| line.selector() == Some("tab-menu-grid-back"));
        if menu.grid_open && submenu {
            return rearrange;
        }
        let mut lines = head_lines(tab, tabs, &self.popouts);
        if !rearrange.is_empty() {
            if !lines.is_empty() {
                lines.push(MenuLine::Separator);
            }
            lines.extend(rearrange);
        }
        if bound >= 3 {
            lines.push(MenuLine::row(
                "tab-menu-move-panes",
                "Move panes to new tab…",
                TabAction::MovePanes,
            ));
        }
        lines.push(MenuLine::Separator);
        lines.extend(self.font_lines(&menu.tab_id));
        lines.push(MenuLine::Separator);
        let close_armed = self.tabs.close_confirm.armed() == Some(tab.id.as_str());
        lines.push(close_line(close_armed));
        if tabs.len() >= 2 {
            lines.push(close_others_line(tabs.len() - 1, menu.close_others_armed));
        }
        let merge = merge_lines(tabs, &self.tabs.selection, &menu.tab_id);
        if !merge.is_empty() {
            lines.push(MenuLine::Separator);
            lines.extend(merge);
        }
        lines
    }

    /// The font section: the size, then Increase, Decrease and Reset, each
    /// inert when it would change nothing.
    fn font_lines(&self, tab_id: &str) -> Vec<MenuLine> {
        let size = self.resolved_tab_font_size(tab_id);
        let overridden = self.sidebar.tab_font_size(tab_id).is_some();
        let row = |selector: &str, label: &str, action: TabAction, enabled: bool| {
            if enabled {
                MenuLine::row(selector, label, action)
            } else {
                MenuLine::inert(selector, label)
            }
        };
        vec![
            MenuLine::Label(format!("Font size: {size}")),
            row(
                "tab-menu-increase",
                "Increase",
                TabAction::FontUp,
                fonts::stepped(size, 1.0).is_some(),
            ),
            row(
                "tab-menu-decrease",
                "Decrease",
                TabAction::FontDown,
                fonts::stepped(size, -1.0).is_some(),
            ),
            row("tab-menu-reset", "Reset", TabAction::FontReset, overridden),
        ]
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
        self.close_more_menu(window, cx);
        self.menu = None;
        self.pane_ui.menu = None;
        self.tab_menu = Some(TabMenu {
            tab_id: tab_id.to_owned(),
            at,
            grid_open: false,
            close_others_armed: false,
            max_height: window.viewport_size().height * MENU_MAX_SHARE,
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
        self.apply_pane_fonts(true, cx);
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
        let lines: Vec<AnyElement> = self
            .tab_menu_lines(menu)
            .into_iter()
            .map(|line| tab_menu_line(line, &menu.tab_id, cx))
            .collect();
        menu_frame(
            "tab-menu",
            &self.tab_menu_focus,
            menu.max_height,
            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                this.close_tab_menu(window, cx);
                cx.stop_propagation();
            }),
        )
        .children(lines)
    }

    /// Runs a press on a row of `tab_id`'s menu.
    fn run_tab_action(
        &mut self,
        tab_id: &str,
        action: TabAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if action != TabAction::CloseOthers
            && let Some(menu) = &mut self.tab_menu
        {
            menu.close_others_armed = false;
        }
        match action {
            TabAction::Rename => {
                self.tab_menu = None;
                self.start_rename(tab_id, window, cx);
            }
            TabAction::PopOut => {
                self.close_tab_menu(window, cx);
                let key = PopoutKey::Tab {
                    tab_id: tab_id.to_owned(),
                };
                self.open_popout(key, window, cx);
            }
            TabAction::OpenGrid | TabAction::CloseGrid => {
                self.show_grid_menu(action == TabAction::OpenGrid, window, cx);
            }
            TabAction::Rearrange(layout) => {
                self.send(ClientMessage::RearrangeTab {
                    tab_id: tab_id.to_owned(),
                    layout,
                });
                self.close_tab_menu(window, cx);
            }
            TabAction::MovePanes => {
                self.tab_menu = None;
                self.open_move_panes(tab_id, window, cx);
            }
            TabAction::FontUp => self.bump_tab_font(tab_id, 1.0, cx),
            TabAction::FontDown => self.bump_tab_font(tab_id, -1.0, cx),
            TabAction::FontReset => {
                self.reset_tab_font(tab_id, cx);
                self.close_tab_menu(window, cx);
            }
            TabAction::Close => {
                if self.close_tab_click(tab_id, cx) {
                    self.close_tab_menu(window, cx);
                }
                cx.notify();
            }
            TabAction::CloseOthers => self.close_other_tabs(tab_id, window, cx),
            TabAction::Merge(layout) => self.merge_selected(layout, window, cx),
        }
    }

    /// Swaps the menu's rows for the Grid submenu (`open`), or back.
    fn show_grid_menu(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(menu) = &mut self.tab_menu {
            menu.grid_open = open;
            self.tab_menu_focus.focus(window);
            cx.notify();
        }
    }

    /// The first press arms Close other tabs; the second closes every tab
    /// but `tab_id`.
    fn close_other_tabs(&mut self, tab_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = &mut self.tab_menu else {
            return;
        };
        if !menu.close_others_armed {
            menu.close_others_armed = true;
            cx.notify();
            return;
        }
        let others: Vec<String> = self
            .tabs
            .tabs()
            .iter()
            .filter(|tab| tab.id != tab_id)
            .map(|tab| tab.id.clone())
            .collect();
        for other in others {
            self.tabs.mark_closing(&other);
            self.send(ClientMessage::CloseTab { tab_id: other });
        }
        self.close_tab_menu(window, cx);
    }

    /// Merges the selected tabs, in strip order, into a new tab that shows
    /// once it arrives, and clears the selection.
    fn merge_selected(&mut self, layout: MergeLayout, window: &mut Window, cx: &mut Context<Self>) {
        let tab_ids = self.selected_tabs();
        let request_id = crate::new_request_id();
        self.tabs.arm_create(&request_id);
        self.send(ClientMessage::MergeTabs {
            tab_ids,
            name: None,
            layout,
            request_id: Some(request_id),
        });
        self.tabs.selection.clear();
        self.close_tab_menu(window, cx);
    }
}

/// The mark after a popped-out tab's name on its pill, in the strip's muted
/// text.
fn popped_mark(tab_id: &str) -> Stateful<Div> {
    let name = format!("tab-pill-popped-{tab_id}");
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex_none()
        .text_color(gpui::rgb(MUTED))
        .tooltip(tooltip("Popped out"))
        .child("↗")
}

/// `Close tab`, or its confirm while the tab's close waits for a second
/// press.
fn close_line(armed: bool) -> MenuLine {
    let label = if armed {
        "Confirm closing this tab"
    } else {
        "Close tab"
    };
    MenuLine::Row {
        selector: "tab-menu-close".to_owned(),
        label: label.to_owned(),
        action: Some(TabAction::Close),
        danger: armed,
    }
}

/// `Close other tabs` for `others` tabs, or its confirm once armed.
fn close_others_line(others: usize, armed: bool) -> MenuLine {
    let label = if armed {
        let s = if others == 1 { "" } else { "s" };
        format!("Confirm closing {others} other tab{s}")
    } else {
        "Close other tabs".to_owned()
    };
    MenuLine::Row {
        selector: "tab-menu-close-others".to_owned(),
        label,
        action: Some(TabAction::CloseOthers),
        danger: armed,
    }
}

/// One line of `tab_id`'s menu. A row with no action is inert, as an
/// action still on the way is in the session menu. Close acts on the press,
/// as the pill's × does, so the root's click-elsewhere reset never disarms
/// it.
fn tab_menu_line(line: MenuLine, tab_id: &str, cx: &mut Context<RootView>) -> AnyElement {
    let (selector, label, action, danger) = match line {
        MenuLine::Label(text) => return muted_row(text).into_any_element(),
        MenuLine::Separator => return menu_separator().into_any_element(),
        MenuLine::Row {
            selector,
            label,
            action,
            danger,
        } => (selector, label, action, danger),
    };
    let row = menu_item(&selector, label, danger);
    let Some(action) = action else {
        return row.opacity(0.6).cursor_default().into_any_element();
    };
    let tab_id = tab_id.to_owned();
    if action == TabAction::Close {
        return row
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.run_tab_action(&tab_id, action, window, cx);
                    cx.stop_propagation();
                }),
            )
            .into_any_element();
    }
    row.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.run_tab_action(&tab_id, action, window, cx);
    }))
    .into_any_element()
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
        let accent = self.sidebar.appearance(None).accent.value;
        let pills: Vec<AnyElement> = self
            .tabs
            .tabs()
            .iter()
            .map(|tab| self.tab_pill(tab, accent, cx))
            .collect();
        div()
            .flex()
            .flex_none()
            .items_center()
            .h(px(TAB_BAR_HEIGHT))
            .bg(gpui::rgb(SURFACE))
            .border_b_1()
            .border_color(gpui::rgb(BORDER))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(MUTED))
            .children(pills)
            .child(new_tab_button(cx))
    }

    /// `tab`'s pill; selection shows in `accent`, `0xRRGGBB`.
    fn tab_pill(&self, tab: &TabEntry, accent: u32, cx: &mut Context<Self>) -> AnyElement {
        let active = self.tabs.active_id() == Some(tab.id.as_str());
        let look = PillLook::of(active, self.tabs.selection.contains(&tab.id));
        let armed = self.tabs.close_confirm.armed() == Some(tab.id.as_str());
        let badge = self
            .tab_badge(&tab.id)
            .map(|(busy, total)| busy_badge(&tab.id, busy, total));
        let dashboard = matches!(tab.content, TabContent::Dashboard);
        let icon = dashboard.then(|| dashboard_icon(&tab.id));
        let need_badge = dashboard
            .then(|| need_you_badge(&tab.id, self.needs_you_count()))
            .flatten();
        let rename = self.renaming.as_ref().filter(|r| r.tab_id == tab.id);
        let label = match rename {
            Some(rename) => div().w(px(RENAME_WIDTH)).child(rename.input.clone()),
            None => div().whitespace_nowrap().child(pill_label(tab)),
        };
        let (click_id, middle_id, menu_id) = (tab.id.clone(), tab.id.clone(), tab.id.clone());
        let on_click = cx.listener(move |this, event: &ClickEvent, window, cx| {
            this.click_tab(&click_id, event, window, cx);
        });
        let on_middle = cx.listener(move |this, _: &MouseDownEvent, _, cx| {
            this.close_tab_click(&middle_id, cx);
            cx.stop_propagation();
            cx.notify();
        });
        let name = format!("tab-{}", tab.id);
        let dragged = self.active_tab_drag(cx) == Some(tab.id.as_str());
        let no_drop = self.pill_no_drop(&tab.id, cx);
        let mark = self
            .tab_drop_mark(cx)
            .filter(|(id, _)| *id == tab.id)
            .map(|(_, side)| insertion_mark(&tab.id, side));
        let (move_id, drop_id) = (tab.id.clone(), tab.id.clone());
        let ghost = self.pill_ghost(tab, cx);
        let popped = self.is_tab_popped(&tab.id).then(|| popped_mark(&tab.id));
        div()
            .id(ElementId::Name(SharedString::from(name.clone())))
            .debug_selector(|| name)
            .relative()
            .on_drag_move(cx.listener(
                move |this, event: &DragMoveEvent<DragPayload>, window, cx| {
                    this.on_pill_drag_move(&move_id, event, window, cx);
                },
            ))
            .on_drop(cx.listener(move |this, payload: &DragPayload, _, cx| {
                this.on_pill_drop(&drop_id, payload, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.open_tab_menu(&menu_id, event.position, window, cx);
                    cx.stop_propagation();
                }),
            )
            .flex()
            .items_center()
            .gap(px(8.0))
            .h_full()
            .pl(px(14.0))
            .pr(px(12.0))
            .border_r_1()
            .border_color(gpui::rgb(BORDER))
            .cursor_pointer()
            .when(no_drop, |pill| {
                pill.cursor(CursorStyle::OperationNotAllowed)
            })
            .when(active, |pill| pill.text_color(gpui::rgb(TEXT)))
            .when(!active, |pill| {
                pill.hover(|style| style.bg(gpui::rgb(HOVER_BG)))
            })
            .when_some(look.background(accent), |pill, bg| pill.bg(gpui::rgba(bg)))
            .when_some(look.outline(accent), |pill, outline| {
                pill.border_1().border_color(gpui::rgb(outline))
            })
            .when(dragged, |pill| pill.opacity(DRAGGED_PILL_OPACITY))
            .when(rename.is_none(), move |pill| {
                pill.on_click(on_click)
                    .on_mouse_down(MouseButton::Middle, on_middle)
                    .on_drag(DragPayload::Tab(tab.id.clone()), ghost)
            })
            .children(badge)
            .children(icon)
            .child(label)
            .children(need_badge)
            .children(popped)
            .child(close_button(&tab.id, armed, cx))
            .children(mark)
            .into_any_element()
    }

    /// What starts `tab`'s pill drag: it records the tab as the one being
    /// dragged and builds the small copy of the pill that follows the
    /// pointer.
    fn pill_ghost(
        &self,
        tab: &TabEntry,
        cx: &mut Context<Self>,
    ) -> impl Fn(&DragPayload, Point<Pixels>, &mut Window, &mut App) -> Entity<PillGhost> + 'static
    {
        let root = cx.entity().downgrade();
        let tab_id = tab.id.clone();
        let label: SharedString = pill_label(tab).into();
        let badge = self.tab_badge(&tab.id);
        move |_, offset, _, cx| {
            let dragged = tab_id.clone();
            if let Err(err) = root.update(cx, |this, _| this.tab_drag = Some(dragged)) {
                tracing::warn!(%err, "pill drag: the window's root view is gone");
            }
            let (tab_id, label) = (tab_id.clone(), label.clone());
            cx.new(|_| PillGhost {
                tab_id,
                label,
                badge,
                offset,
            })
        }
    }

    /// The tab being dragged by its pill, while gpui's drag under way is a
    /// pill drag.
    pub(crate) fn active_tab_drag(&self, cx: &App) -> Option<&str> {
        self.tab_drag.as_deref().filter(|_| cx.has_active_drag())
    }

    /// After a tab message: a pill drag whose tab left the strip ends as
    /// Esc ends it, and a mark on a tab that left the strip goes.
    pub(crate) fn drop_stale_pill_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let gone = |tabs: &crate::tabs::TabsModel, id: &str| tabs.tab(id).is_none();
        if self
            .active_tab_drag(cx)
            .is_some_and(|id| gone(&self.tabs, id))
        {
            self.end_drag(window, cx);
            return;
        }
        if self
            .tab_drop
            .as_ref()
            .is_some_and(|(id, _)| gone(&self.tabs, id))
        {
            self.tab_drop = None;
            cx.notify();
        }
    }

    /// The pill the insertion mark stands on and the side of it, while a
    /// pill drag is under way and a drop there would change the order.
    #[must_use]
    pub fn tab_drop_mark(&self, cx: &App) -> Option<(String, Side)> {
        self.tab_drop
            .clone()
            .filter(|_| self.active_tab_drag(cx).is_some())
    }

    /// What `tab_id`'s pill reads while it is not being renamed.
    #[must_use]
    pub fn tab_pill_label(&self, tab_id: &str) -> Option<String> {
        self.tabs.tab(tab_id).map(pill_label)
    }

    /// Whether `tab_id`'s pill shows the no-drop state: a pane drag is under
    /// way and that pill's tab takes no pane, or a sidebar leaf drag is and
    /// that pill's tab takes no leaf.
    #[must_use]
    pub fn pill_no_drop(&self, tab_id: &str, cx: &App) -> bool {
        let pane_refused = self.active_pane_drag(cx).is_some()
            && drag::pill_pane_hover(self.tabs.tabs(), self.tabs.active_id(), tab_id)
                == drag::PillPaneHover::Inert;
        let leaf_refused = matches!(self.active_sidebar_drag(cx), Some(DragPayload::Leaf { .. }))
            && !drag::pill_takes_leaf(&self.tabs, tab_id);
        pane_refused || leaf_refused
    }

    /// Follows a drag over `tab_id`'s pill: a pill dragged over its half
    /// that would change the order marks that side; a pane dragged over a
    /// grid tab's pill shows that tab, so the pane can drop on a pane in it;
    /// anything else takes the mark off this pill. gpui calls this for every
    /// move of the drag, over the pill or not.
    fn on_pill_drag_move(
        &mut self,
        tab_id: &str,
        event: &DragMoveEvent<DragPayload>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let at = event.event.position;
        let bounds = event.bounds;
        let over_pill = bounds.contains(&at);
        let target = match event.drag(cx) {
            DragPayload::Tab(dragged) if over_pill => {
                let side = drag::side_of(
                    at.x / px(1.0),
                    bounds.origin.x / px(1.0),
                    bounds.size.width / px(1.0),
                );
                drag::reorder_tabs(&self.tab_ids(), dragged, tab_id, side, None)
                    .map(|_| (tab_id.to_owned(), side))
            }
            _ => None,
        };
        let marked_here = self.tab_drop.as_ref().is_some_and(|(id, _)| id == tab_id);
        if target.is_some() && target != self.tab_drop {
            self.tab_drop = target;
            cx.notify();
        } else if target.is_none() && marked_here {
            self.tab_drop = None;
            cx.notify();
        }
        let activate = over_pill
            && matches!(event.drag(cx), DragPayload::Pane { .. })
            && drag::pill_pane_hover(self.tabs.tabs(), self.tabs.active_id(), tab_id)
                == drag::PillPaneHover::Activate;
        if activate {
            self.tabs.activate(tab_id);
            self.pane_drop = None;
            self.after_tabs_change(window, cx);
        }
    }

    /// A drop on `tab_id`'s pill: a pill dropped where the mark stands takes
    /// the new order at once and sends it; the daemon's echo then agrees.
    /// A pane dropped on another grid tab's pill moves there at that tab's
    /// balanced drop target. A sidebar leaf dropped on a pill goes where
    /// [`drag::leaf_pill_drop`] says. A drop on the pane's own tab's pill, on
    /// a diff tab's pill, or one that keeps the order, does nothing.
    fn on_pill_drop(&mut self, tab_id: &str, payload: &DragPayload, cx: &mut Context<Self>) {
        let mark = self.tab_drop.take();
        self.tab_drag = None;
        self.pane_drop = None;
        self.pane_drag = None;
        self.pane_drag_session = None;
        self.sidebar_drag = None;
        self.sidebar_drop = None;
        cx.notify();
        match payload {
            DragPayload::Pane {
                tab_id: src_tab,
                pane_id,
            } => {
                if src_tab != tab_id && drag::pill_takes_pane(self.tabs.tabs(), tab_id) {
                    self.move_pane_to_tab(src_tab, pane_id, tab_id, cx);
                }
            }
            DragPayload::Tab(dragged) => {
                let Some((target, side)) = mark else {
                    return;
                };
                if target != tab_id {
                    return;
                }
                if let Some(ClientMessage::ReorderTabs { ordered_ids, .. }) =
                    drag::reorder_tabs(&self.tab_ids(), dragged, &target, side, None)
                {
                    self.reorder_tabs_by_drop(ordered_ids);
                }
            }
            DragPayload::Leaf { session_id, .. } => {
                let drop = drag::leaf_pill_drop(
                    &self.tabs,
                    self.sidebar.sessions(),
                    session_id,
                    tab_id,
                    Some(crate::new_request_id()),
                );
                if let Some(drop) = drop {
                    self.send_leaf_drop(drop, cx);
                }
            }
            DragPayload::Container(_) => {}
        }
    }

    /// A tab dropped on a pill or on a tab container: the strip takes
    /// `ordered_ids` at once and sends it with a request id, so a refusal
    /// puts the strip back.
    pub(crate) fn reorder_tabs_by_drop(&mut self, ordered_ids: Vec<String>) {
        let request_id = crate::new_request_id();
        self.tabs.note_reorder(&request_id, &ordered_ids);
        if !self.tabs.reorder_local(&ordered_ids) {
            self.tabs.fail_reorder(&request_id);
            tracing::warn!(
                ?ordered_ids,
                "tab drop: the new order is not the strip's tabs"
            );
            return;
        }
        self.send(ClientMessage::ReorderTabs {
            ordered_ids,
            request_id: Some(request_id),
        });
    }

    /// A click on `tab_id`'s pill. Ctrl toggles the tab in the selection
    /// and Shift selects a range, neither showing it; a plain click shows
    /// it, and a plain double-click renames it.
    fn click_tab(
        &mut self,
        tab_id: &str,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mods = event.modifiers();
        let kind = if mods.control {
            PillClick::Toggle
        } else if mods.shift {
            PillClick::Range
        } else {
            PillClick::Plain
        };
        let renames = self.tabs.tab(tab_id).is_some_and(|tab| {
            !matches!(
                tab.content,
                TabContent::Viewer { .. } | TabContent::Dashboard
            )
        });
        if kind == PillClick::Plain && event.click_count() >= 2 && renames {
            self.start_rename(tab_id, window, cx);
            return;
        }
        let ids = self.tab_ids();
        let order: Vec<&str> = ids.iter().map(String::as_str).collect();
        let active = self.tabs.active_id().map(str::to_owned);
        if self
            .tabs
            .selection
            .click(&order, active.as_deref(), tab_id, kind)
        {
            self.tabs.activate(tab_id);
            self.after_tabs_change(window, cx);
        } else {
            cx.notify();
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
            || self.open_in_main(Overlay::EmptyPaneMenu)
            || self.open_in_main(Overlay::SessionMenu)
            || self.container_menu.is_some()
            || self.open_in_main(Overlay::ShellMenu)
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
            layout: RearrangeLayout::Grid {
                cols: 0,
                aspect: Some(self.pane_area_aspect()),
            },
        });
        true
    }

    fn new_tab(&mut self) {
        let request_id = crate::new_request_id();
        self.tabs.arm_create(&request_id);
        self.send(ClientMessage::CreateTab {
            name: None,
            initial_session_id: None,
            host: None,
            request_id: Some(request_id),
        });
    }

    /// A close click: closes the tab, or arms the close when the tab holds
    /// a session or a split. Returns whether it asked to close the tab.
    fn close_tab_click(&mut self, tab_id: &str, cx: &mut Context<Self>) -> bool {
        let Some(tab) = self.tabs.tab(tab_id).cloned() else {
            return false;
        };
        let close = self.tabs.close_confirm.click(&tab);
        if close {
            if let Some(snapshot) = self.tab_snapshot(&tab.id, None) {
                self.record_undo(undo::closed_tab_message(&tab.name), vec![snapshot], cx);
            }
            self.tabs.mark_closing(&tab.id);
            self.send(ClientMessage::CloseTab { tab_id: tab.id });
        }
        close
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

/// How a pill shows whether it is active and selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PillLook {
    Plain,
    /// The tab shown: the main area's ground.
    Active,
    /// Selected, not shown: a light accent wash.
    Washed,
    /// Shown and selected: the main area's ground in an accent outline.
    ActiveOutlined,
}

impl PillLook {
    fn of(active: bool, selected: bool) -> Self {
        match (active, selected) {
            (false, false) => Self::Plain,
            (true, false) => Self::Active,
            (false, true) => Self::Washed,
            (true, true) => Self::ActiveOutlined,
        }
    }

    /// The background, `0xRRGGBBAA`, for an `accent` of `0xRRGGBB`.
    fn background(self, accent: u32) -> Option<u32> {
        match self {
            Self::Plain => None,
            Self::Active | Self::ActiveOutlined => Some((GROUND << 8) | 0xff),
            Self::Washed => Some((accent << 8) | SELECTED_WASH_ALPHA),
        }
    }

    /// The outline's colour, `0xRRGGBB`, if it has one.
    fn outline(self, accent: u32) -> Option<u32> {
        (self == Self::ActiveOutlined).then_some(accent)
    }
}

/// A tab's name; a diff tab's is marked Δ, and a viewer tab's is its file's
/// name.
fn pill_label(tab: &TabEntry) -> String {
    match &tab.content {
        TabContent::Diff { .. } => format!("Δ {}", tab.name),
        TabContent::Viewer { path } => display_name(path).to_owned(),
        TabContent::Grid { .. } | TabContent::Dashboard | TabContent::Unknown => tab.name.clone(),
    }
}

/// What a grid tab's busy pill shows.
#[derive(Debug, PartialEq, Eq)]
struct BusyPill {
    /// `{busy}/{total}`.
    text: String,
    /// Whether the working glyph leads the text: only while a pane is busy.
    glyph: bool,
    /// The text's colour, `0xRRGGBB`.
    color: u32,
}

impl BusyPill {
    /// The working colour with its glyph while a pane is busy, else a grey
    /// `0/N`.
    fn of(busy: usize, total: usize) -> Self {
        Self {
            text: format!("{busy}/{total}"),
            glyph: busy > 0,
            color: if busy > 0 { WORKING } else { SUBTLE },
        }
    }
}

/// The busy pill after a grid tab's name, tagged `tab-badge-<id>`; its glyph
/// is tagged `tab-badge-glyph-<id>-working`.
fn busy_badge(tab_id: &str, busy: usize, total: usize) -> Stateful<Div> {
    let pill = BusyPill::of(busy, total);
    let name = format!("tab-badge-{tab_id}");
    let glyph_id = format!("tab-badge-glyph-{tab_id}");
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(4.0))
        .h(px(BUSY_PILL_HEIGHT))
        .px(px(6.0))
        .rounded(px(BUSY_PILL_HEIGHT / 2.0))
        .bg(gpui::rgb(CHIP))
        .font_family(fonts::DEFAULT_FAMILY)
        .text_size(px(BUSY_PILL_TEXT_SIZE))
        .text_color(gpui::rgb(pill.color))
        .tooltip(tooltip(format!("{busy} of {total} panes busy")))
        .when(pill.glyph, |row| {
            row.child(glyph_view(
                glyph(
                    SessionStatus::Working,
                    false,
                    SessionMode::Interactive,
                    false,
                ),
                GlyphSize::Pill,
                &glyph_id,
            ))
        })
        .child(pill.text)
}

/// The Dashboard icon before a Dashboard tab's name, tagged
/// `tab-dashboard-icon-<id>`.
fn dashboard_icon(tab_id: &str) -> Div {
    let name = format!("tab-dashboard-icon-{tab_id}");
    div().debug_selector(|| name).flex().flex_none().child(
        svg()
            .path(DASHBOARD_ICON)
            .size(px(TAB_ICON_SIZE))
            .text_color(gpui::rgb(SUBTLE)),
    )
}

/// The pill after a Dashboard tab's name counting the sessions waiting on
/// the user, tagged `tab-need-badge-<id>`; none at 0.
fn need_you_badge(tab_id: &str, waiting: usize) -> Option<Stateful<Div>> {
    let text = badge_text(waiting)?;
    let name = format!("tab-need-badge-{tab_id}");
    Some(
        div()
            .id(ElementId::Name(SharedString::from(name.clone())))
            .debug_selector(|| name)
            .flex()
            .flex_none()
            .items_center()
            .h(px(BUSY_PILL_HEIGHT))
            .px(px(6.0))
            .rounded(px(BUSY_PILL_HEIGHT / 2.0))
            .bg(gpui::rgb(CHIP))
            .font_family(fonts::DEFAULT_FAMILY)
            .text_size(px(BUSY_PILL_TEXT_SIZE))
            .text_color(gpui::rgb(TEXT))
            .tooltip(tooltip(format!("{waiting} waiting on you")))
            .child(text),
    )
}

/// How opaque a pill stays in its slot while it is dragged.
const DRAGGED_PILL_OPACITY: f32 = 0.4;
/// The insertion mark's width, and how far it stands in from the pill's top
/// and bottom.
const MARK_WIDTH: f32 = 2.0;
const MARK_INSET: f32 = 5.0;
/// How far the mark sits out past the pill's edge, so it straddles the
/// border between two pills.
const MARK_OUTSET: f32 = 1.5;
/// Where the pill ghost sits, right of and below the pointer.
const GHOST_NUDGE: Point<Pixels> = Point {
    x: px(10.0),
    y: px(14.0),
};
const GHOST_HEIGHT: f32 = 26.0;
const GHOST_TEXT_SIZE: f32 = 11.0;
const GHOST_OPACITY: f32 = 0.85;
const GHOST_BADGE_HEIGHT: f32 = 15.0;
const GHOST_BADGE_TEXT_SIZE: f32 = 9.5;

/// The accent bar at `side` of `tab_id`'s pill, where a dragged pill lands.
fn insertion_mark(tab_id: &str, side: Side) -> Div {
    let selector = format!("tab-drop-mark-{tab_id}");
    let bar = div()
        .debug_selector(move || selector)
        .absolute()
        .top(px(MARK_INSET))
        .bottom(px(MARK_INSET))
        .w(px(MARK_WIDTH))
        .rounded(px(MARK_WIDTH / 2.0))
        .bg(gpui::rgb(ACCENT));
    match side {
        Side::Before => bar.left(px(-MARK_OUTSET)),
        Side::After => bar.right(px(-MARK_OUTSET)),
    }
}

/// The small copy of a dragged pill that follows the pointer: its busy
/// badge, when it has one, and its label.
pub(crate) struct PillGhost {
    tab_id: String,
    label: SharedString,
    /// The pill's busy and total pane counts.
    badge: Option<(usize, usize)>,
    /// Where the pointer sat in the pill when the drag started. gpui draws
    /// the ghost that far up and left of the pointer; the ghost moves back
    /// by it.
    offset: Point<Pixels>,
}

impl Render for PillGhost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let badge = self.badge.map(|(busy, total)| {
            let pill = BusyPill::of(busy, total);
            let glyph_id = format!("tab-ghost-glyph-{}", self.tab_id);
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(4.0))
                .h(px(GHOST_BADGE_HEIGHT))
                .px(px(5.0))
                .rounded(px(GHOST_BADGE_HEIGHT / 2.0))
                .bg(gpui::rgb(CHIP))
                .font_family(fonts::DEFAULT_FAMILY)
                .text_size(px(GHOST_BADGE_TEXT_SIZE))
                .text_color(gpui::rgb(pill.color))
                .when(pill.glyph, |row| {
                    row.child(glyph_view(
                        glyph(
                            SessionStatus::Working,
                            false,
                            SessionMode::Interactive,
                            false,
                        ),
                        GlyphSize::Pill,
                        &glyph_id,
                    ))
                })
                .child(pill.text)
        });
        div().child(
            div()
                .debug_selector(|| "tab-drag-ghost".to_owned())
                .absolute()
                .left(self.offset.x + GHOST_NUDGE.x)
                .top(self.offset.y + GHOST_NUDGE.y)
                .flex()
                .items_center()
                .gap(px(8.0))
                .h(px(GHOST_HEIGHT))
                .px(px(10.0))
                .whitespace_nowrap()
                .bg(gpui::rgb(RAISED))
                .border_1()
                .border_color(gpui::rgb(LINE_STRONG))
                .rounded(px(4.0))
                .shadow_lg()
                .opacity(GHOST_OPACITY)
                .font_family(fonts::ui_family(cx))
                .text_size(px(GHOST_TEXT_SIZE))
                .text_color(gpui::rgb(TEXT))
                .children(badge)
                .child(self.label.clone()),
        )
    }
}

/// The close icon, or ✓ while a close waits for its second click. It acts
/// on the press so the root's click-elsewhere reset never sees it.
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
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .min_w(px(CLOSE_ICON_SIZE + 4.0))
        .h(px(CLOSE_ICON_SIZE + 4.0))
        .rounded(px(3.0))
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .tooltip(tooltip(tip))
        .map(|button| {
            if armed {
                button.text_color(gpui::rgb(DANGER)).child("✓")
            } else {
                button.child(
                    svg()
                        .path(TAB_CLOSE_ICON)
                        .size(px(CLOSE_ICON_SIZE))
                        .text_color(gpui::rgb(SUBTLE)),
                )
            }
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                this.close_tab_click(&id, cx);
                cx.stop_propagation();
                cx.notify();
            }),
        )
}

fn new_tab_button(cx: &mut Context<RootView>) -> Stateful<Div> {
    div()
        .id("new-tab")
        .debug_selector(|| "new-tab".to_owned())
        .w(px(TAB_BAR_HEIGHT))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .tooltip(tooltip("New tab"))
        .child(
            svg()
                .path(TAB_NEW_ICON)
                .size(px(NEW_TAB_ICON_SIZE))
                .text_color(gpui::rgb(MUTED)),
        )
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            this.new_tab();
            cx.notify();
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCENT: u32 = 0x005b_9bff;

    #[test]
    fn active_selected_tab_keeps_active_background() {
        let active = PillLook::of(true, false);
        let both = PillLook::of(true, true);
        assert_eq!(both, PillLook::ActiveOutlined);
        assert_eq!(both.background(ACCENT), active.background(ACCENT));
        assert_eq!(both.background(ACCENT), Some((GROUND << 8) | 0xff));
        assert_eq!(both.outline(ACCENT), Some(ACCENT));
        assert_eq!(active.outline(ACCENT), None);

        let washed = PillLook::of(false, true);
        assert_eq!(washed.background(ACCENT), Some((ACCENT << 8) | 0x24));
        assert_eq!(washed.outline(ACCENT), None);
        assert_eq!(PillLook::of(false, false).background(ACCENT), None);
    }

    #[test]
    fn busy_pill_shows_the_working_glyph_while_a_pane_is_busy() {
        assert_eq!(
            BusyPill::of(1, 2),
            BusyPill {
                text: "1/2".to_owned(),
                glyph: true,
                color: WORKING,
            }
        );
    }

    #[test]
    fn idle_busy_pill_is_a_grey_zero_over_total_with_no_glyph() {
        assert_eq!(
            BusyPill::of(0, 3),
            BusyPill {
                text: "0/3".to_owned(),
                glyph: false,
                color: SUBTLE,
            }
        );
    }
}
