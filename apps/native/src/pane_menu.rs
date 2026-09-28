//! The pane actions' state and the empty pane's menu: Move to ▸ (a new tab,
//! or another grid tab at its balanced drop target) and Close pane. Also the
//! header's move-to-a-new-tab button and the keys of the pane dialogs.

use gpui::{
    AnyElement, ClickEvent, Context, Div, FocusHandle, Keystroke, MouseDownEvent, Pixels, Point,
    Stateful, Window, anchored, deferred, div, prelude::*,
};
use protocol::{ClientMessage, TabContent};

use crate::move_panes_view::MovePanesDialog;
use crate::pane_close::PaneClose;
use crate::session_menu::{menu_frame, menu_item, menu_separator};
use crate::tabs::{collect_panes, pick_balanced_drop_target};
use crate::undo;
use crate::{RootView, tooltip};

/// The pane dialogs and the empty pane's menu, while open, and the focus
/// each takes.
pub(crate) struct PaneUi {
    pub(crate) close: Option<PaneClose>,
    pub(crate) move_panes: Option<MovePanesDialog>,
    pub(crate) menu: Option<EmptyPaneMenu>,
    /// The pane dialogs' keyboard focus when no text field of theirs has it.
    pub(crate) dialog_focus: FocusHandle,
    pub(crate) menu_focus: FocusHandle,
}

impl PaneUi {
    pub(crate) fn new(dialog_focus: FocusHandle, menu_focus: FocusHandle) -> Self {
        Self {
            close: None,
            move_panes: None,
            menu: None,
            dialog_focus,
            menu_focus,
        }
    }
}

/// The open menu of an empty pane.
pub(crate) struct EmptyPaneMenu {
    tab_id: String,
    pane_id: String,
    /// Where the right-click was, in window coordinates.
    at: Point<Pixels>,
    /// Whether the Move to submenu replaces the rows.
    move_open: bool,
}

/// What a row of the empty pane's menu does.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MenuAction {
    OpenMove,
    CloseMove,
    NewTab,
    MoveTo(String),
    Close,
}

/// A row of the empty pane's menu.
struct MenuRow {
    selector: String,
    label: String,
    tip: Option<String>,
    action: MenuAction,
}

impl MenuRow {
    fn new(selector: &str, label: &str, action: MenuAction) -> Self {
        Self {
            selector: selector.to_owned(),
            label: label.to_owned(),
            tip: None,
            action,
        }
    }
}

impl RootView {
    /// The empty pane whose menu is open.
    #[must_use]
    pub fn empty_pane_menu(&self) -> Option<&str> {
        self.pane_ui.menu.as_ref().map(|menu| menu.pane_id.as_str())
    }

    /// The selectors of the open empty-pane menu's rows.
    #[must_use]
    pub fn empty_pane_menu_rows(&self) -> Vec<String> {
        self.pane_ui
            .menu
            .as_ref()
            .map(|menu| {
                self.empty_pane_menu_lines(menu)
                    .into_iter()
                    .flatten()
                    .map(|row| row.selector)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The rows of `menu`, `None` for a separator: the Move to submenu
    /// while open (Back, New tab, then every other grid tab), else Move to
    /// ▸ and Close pane.
    fn empty_pane_menu_lines(&self, menu: &EmptyPaneMenu) -> Vec<Option<MenuRow>> {
        if !menu.move_open {
            return vec![
                Some(MenuRow::new(
                    "empty-pane-menu-move",
                    "Move to ▸",
                    MenuAction::OpenMove,
                )),
                Some(MenuRow::new(
                    "empty-pane-close",
                    "Close pane",
                    MenuAction::Close,
                )),
            ];
        }
        let mut lines = vec![
            Some(MenuRow::new(
                "empty-pane-menu-back",
                "‹ Back",
                MenuAction::CloseMove,
            )),
            Some(MenuRow {
                tip: Some("Move this pane into a fresh tab".to_owned()),
                ..MenuRow::new("empty-pane-move-new", "New tab", MenuAction::NewTab)
            }),
        ];
        let targets: Vec<Option<MenuRow>> = self
            .tabs
            .tabs()
            .iter()
            .filter(|tab| tab.id != menu.tab_id && matches!(tab.content, TabContent::Grid { .. }))
            .map(|tab| {
                Some(MenuRow {
                    selector: format!("empty-pane-move-{}", tab.id),
                    label: tab.name.clone(),
                    tip: Some(format!("Move this pane into \"{}\"", tab.name)),
                    action: MenuAction::MoveTo(tab.id.clone()),
                })
            })
            .collect();
        if !targets.is_empty() {
            lines.push(None);
            lines.extend(targets);
        }
        lines
    }

    /// Opens the menu of empty pane `pane_id` of `tab_id` at `at`, taking
    /// the keyboard so Esc reaches it. Every other context menu gives way.
    pub(crate) fn open_empty_pane_menu(
        &mut self,
        tab_id: &str,
        pane_id: &str,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu = None;
        self.tab_menu = None;
        self.pane_ui.menu = Some(EmptyPaneMenu {
            tab_id: tab_id.to_owned(),
            pane_id: pane_id.to_owned(),
            at,
            move_open: false,
        });
        self.pane_ui.menu_focus.focus(window);
        cx.notify();
    }

    /// Closes the menu and hands the keyboard back to the active pane.
    pub(crate) fn close_empty_pane_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pane_ui.menu.take().is_some() {
            self.focus_active_pane(window, cx);
            cx.notify();
        }
    }

    /// Moves `pane_id` of `tab_id` into a new tab, which shows once it
    /// arrives.
    pub(crate) fn move_pane_to_new_tab(&mut self, tab_id: &str, pane_id: &str) {
        self.tabs.arm_create();
        self.send(ClientMessage::ExtractToNewTab {
            source_tab_id: tab_id.to_owned(),
            pane_ids: vec![pane_id.to_owned()],
            name: None,
            layout: None,
        });
    }

    /// Runs a press on a row of the open empty-pane menu.
    fn run_empty_pane_action(
        &mut self,
        action: &MenuAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = &mut self.pane_ui.menu else {
            return;
        };
        let (tab_id, pane_id) = (menu.tab_id.clone(), menu.pane_id.clone());
        match action {
            MenuAction::OpenMove | MenuAction::CloseMove => {
                menu.move_open = *action == MenuAction::OpenMove;
                self.pane_ui.menu_focus.focus(window);
                cx.notify();
                return;
            }
            MenuAction::NewTab => self.move_pane_to_new_tab(&tab_id, &pane_id),
            MenuAction::MoveTo(dst_tab_id) => {
                let target = self
                    .tabs
                    .tab(dst_tab_id)
                    .and_then(|tab| tab.grid())
                    .and_then(pick_balanced_drop_target);
                match target {
                    Some((dst_pane_id, edge)) => {
                        let snapshots = [&tab_id, dst_tab_id]
                            .into_iter()
                            .filter_map(|id| self.tab_snapshot(id, Some(&pane_id)))
                            .collect();
                        self.record_undo(undo::MOVED_PANE.to_owned(), snapshots, cx);
                        self.send(ClientMessage::MovePane {
                            src_tab_id: tab_id,
                            src_pane_id: pane_id,
                            dst_tab_id: dst_tab_id.clone(),
                            dst_pane_id,
                            edge,
                        });
                    }
                    None => {
                        tracing::warn!(tab = %dst_tab_id, "move pane: the tab has no pane to drop on");
                    }
                }
            }
            MenuAction::Close => {
                self.record_pane_close(&tab_id, &pane_id, None, cx);
                self.send(ClientMessage::ClosePane { tab_id, pane_id });
            }
        }
        self.close_empty_pane_menu(window, cx);
    }

    /// The open empty-pane menu over a layer that keeps a click outside it
    /// from reaching what lies beneath.
    pub(crate) fn empty_pane_menu_layer(&self, cx: &mut Context<Self>) -> Option<[AnyElement; 2]> {
        let menu = self.pane_ui.menu.as_ref()?;
        let rows: Vec<AnyElement> = self
            .empty_pane_menu_lines(menu)
            .into_iter()
            .map(|line| match line {
                None => menu_separator().into_any_element(),
                Some(row) => empty_pane_row(row, cx).into_any_element(),
            })
            .collect();
        let frame = menu_frame(
            "empty-pane-menu",
            &self.pane_ui.menu_focus,
            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                this.close_empty_pane_menu(window, cx);
                cx.stop_propagation();
            }),
        )
        .children(rows);
        let backdrop = div().absolute().top_0().left_0().size_full().occlude();
        let panel = anchored().position(menu.at).snap_to_window().child(frame);
        Some([
            backdrop.into_any_element(),
            deferred(panel).with_priority(1).into_any_element(),
        ])
    }

    /// The ids of `tab_id`'s panes with the session each shows; empty for
    /// a tab the daemon no longer lists or a diff tab.
    pub(crate) fn tab_pane_sessions(&self, tab_id: &str) -> Vec<(String, Option<String>)> {
        self.tabs
            .tab(tab_id)
            .and_then(|tab| tab.grid())
            .map(|grid| {
                collect_panes(grid)
                    .into_iter()
                    .map(|p| (p.id.to_owned(), p.session.map(str::to_owned)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Drops what the daemon's tab list has outrun: the menu of a pane that
    /// is gone, the pane-close dialog of a pane that is gone or shows
    /// another session, and the Move panes dialog's gone panes (the dialog
    /// with them once none is left). Returns whether a dialog or the menu
    /// closed, so the keyboard can go back.
    pub(crate) fn drop_stale_pane_ui(&mut self) -> bool {
        let mut dropped = false;
        if let Some(menu) = &self.pane_ui.menu {
            let panes = self.tab_pane_sessions(&menu.tab_id);
            if !panes.iter().any(|(id, _)| *id == menu.pane_id) {
                self.pane_ui.menu = None;
                dropped = true;
            }
        }
        if let Some(close) = &self.pane_ui.close {
            let panes = self.tab_pane_sessions(close.tab_id());
            let still = panes.iter().any(|(id, session)| {
                id == close.pane_id() && session.as_deref() == Some(close.session_id())
            });
            if !still {
                self.pane_ui.close = None;
                dropped = true;
            }
        }
        if let Some(tab_id) = self
            .pane_ui
            .move_panes
            .as_ref()
            .map(|d| d.model.tab_id().to_owned())
        {
            let panes = self.tab_pane_sessions(&tab_id);
            let left = self.pane_ui.move_panes.as_mut().map_or(0, |dialog| {
                dialog
                    .model
                    .retain_panes(|pane| panes.iter().any(|(id, _)| id == pane))
            });
            if left == 0 {
                self.pane_ui.move_panes = None;
                dropped = true;
            }
        }
        dropped
    }

    /// A key while the pane-close dialog or the empty pane's menu is open;
    /// returns whether it was taken. The menu takes only Esc. The Move
    /// panes dialog's keys go through [`Self::on_move_panes_key`].
    pub(crate) fn on_pane_ui_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.pane_ui.close.is_some() {
            self.on_pane_close_key(keystroke, window, cx);
            return true;
        }
        if self.pane_ui.menu.is_some() && keystroke.key == "escape" {
            self.close_empty_pane_menu(window, cx);
            return true;
        }
        false
    }
}

/// A row of the empty pane's menu, with its hover text when it has one.
fn empty_pane_row(row: MenuRow, cx: &mut Context<RootView>) -> Stateful<Div> {
    let item = menu_item(&row.selector, row.label, false);
    let item = match row.tip {
        Some(tip) => item.tooltip(tooltip(tip)),
        None => item,
    };
    let action = row.action;
    item.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.run_empty_pane_action(&action, window, cx);
    }))
}
