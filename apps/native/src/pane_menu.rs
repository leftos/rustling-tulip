//! The pane actions' state and the empty pane's menu: Move to ▸ (a new tab,
//! or another grid tab at its balanced drop target) and Close pane. Also the
//! session pane menu's Show chat / Show terminal row, the header's
//! move-to-a-new-tab button and the keys of the pane dialogs.

use gpui::{
    AnyElement, ClickEvent, Context, Div, FocusHandle, Keystroke, MouseDownEvent, Pixels, Point,
    Stateful, Window, WindowId, anchored, deferred, div, prelude::*,
};
use protocol::{ClientMessage, TabContent};

use crate::drag::PaneDrop;
use crate::move_panes_view::MovePanesDialog;
use crate::overlay_owner::Overlay;
use crate::pane_close::PaneClose;
use crate::session_menu::{MENU_MAX_SHARE, menu_frame, menu_item, menu_separator};
use crate::tabs::{collect_panes, pick_balanced_drop_target};
use crate::ui_theme::theme;
use crate::undo;
use crate::{RootView, tooltip};

/// The selector of the session pane menu's row that switches the pane
/// between its terminal and its chat.
pub(crate) const PANE_VIEW_ROW: &str = "session-menu-pane-view";

/// The label of the Terminal / Chat row: the view a click switches to.
fn pane_view_label(chat_shown: bool) -> &'static str {
    if chat_shown {
        "Show terminal"
    } else {
        "Show chat"
    }
}

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
    /// The most height its frame may take, from the window it opened in.
    max_height: Pixels,
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
    /// The label of the open session menu's Terminal / Chat row; `None`
    /// when the menu did not open from a pane header or that pane's session
    /// offers no chat, the same rule as the header's toggle.
    #[must_use]
    pub fn pane_view_menu_label(&self) -> Option<&'static str> {
        self.pane_view_menu_pane()
            .map(|pane_id| pane_view_label(self.pane_shows_chat(pane_id)))
    }

    /// The pane whose header opened the session menu, while it offers the
    /// Terminal / Chat row.
    fn pane_view_menu_pane(&self) -> Option<&str> {
        self.session_menu_pane()
            .filter(|pane_id| self.pane_offers_chat(pane_id))
    }

    /// The session menu's Terminal / Chat row, while it offers one: a click
    /// closes the menu and switches the pane, as its header toggle does.
    pub(crate) fn pane_view_menu_row(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pane_id = self.pane_view_menu_pane()?.to_owned();
        let label = pane_view_label(self.pane_shows_chat(&pane_id));
        let row = menu_item(PANE_VIEW_ROW, label, false).on_click(cx.listener(
            move |this, _: &ClickEvent, window, cx| {
                this.close_session_menu(window, cx);
                this.toggle_pane_view(&pane_id, window, cx);
            },
        ));
        Some(row.into_any_element())
    }

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
        // Another window's menu is not under this one's backdrop: only one
        // menu is open at a time across the windows.
        self.close_session_menu(window, cx);
        self.close_shell_menu(window, cx);
        self.tab_menu = None;
        self.overlays
            .opened(Overlay::EmptyPaneMenu, window.window_handle().window_id());
        self.pane_ui.menu = Some(EmptyPaneMenu {
            tab_id: tab_id.to_owned(),
            pane_id: pane_id.to_owned(),
            at,
            move_open: false,
            max_height: window.viewport_size().height * MENU_MAX_SHARE,
        });
        self.pane_ui.menu_focus.focus(window);
        cx.notify();
    }

    /// Closes the menu and hands the keyboard back to the terminal of the
    /// window it was open in.
    pub(crate) fn close_empty_pane_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pane_ui.menu.take().is_some() {
            let owner = self.overlays.owner(Overlay::EmptyPaneMenu);
            self.hand_back_focus(owner, window, cx);
            cx.notify();
        }
    }

    /// Moves `pane_id` of `tab_id` into a new tab, which shows once it
    /// arrives. A tab left with no pane counts as closing until the daemon
    /// answers.
    pub(crate) fn move_pane_to_new_tab(&mut self, tab_id: &str, pane_id: &str) {
        let request_id = crate::new_request_id();
        self.tabs.arm_create(&request_id);
        self.tabs
            .mark_closing_if_last_pane(tab_id, Some(&request_id));
        let msg = ClientMessage::ExtractToNewTab {
            source_tab_id: tab_id.to_owned(),
            pane_ids: vec![pane_id.to_owned()],
            name: None,
            layout: None,
            request_id: Some(request_id),
        };
        self.tabs.note_layout_request(&msg);
        self.send(msg);
    }

    /// Moves `pane_id` of `tab_id` into grid tab `dst_tab_id` at its
    /// balanced drop target, recording both tabs for the undo.
    pub(crate) fn move_pane_to_tab(
        &mut self,
        tab_id: &str,
        pane_id: &str,
        dst_tab_id: &str,
        cx: &mut Context<Self>,
    ) {
        let target = self
            .tabs
            .tab(dst_tab_id)
            .and_then(|tab| tab.grid())
            .and_then(pick_balanced_drop_target);
        let Some((dst_pane_id, edge)) = target else {
            tracing::warn!(tab = %dst_tab_id, "move pane: the tab has no pane to drop on");
            return;
        };
        self.send_pane_move(
            &PaneDrop {
                src_tab_id: tab_id.to_owned(),
                src_pane_id: pane_id.to_owned(),
                dst_tab_id: dst_tab_id.to_owned(),
                dst_pane_id,
                edge,
                touch: false,
                swap: false,
                request_id: Some(crate::new_request_id()),
                undo_label: undo::MOVED_PANE,
            },
            cx,
        );
    }

    /// Sends a pane move, a menu's or a drop's: both tabs as they stand go
    /// on the undo shelf under the move's label, a source tab the move
    /// leaves with no pane counts as closing until the daemon answers (a
    /// swap empties no tab), and the request is noted as in flight. A swap
    /// gives the keyboard to the destination pane, which comes to show the
    /// dragged session, and its undo snapshots each tab on the pane holding
    /// the session that tab put there.
    pub(crate) fn send_pane_move(&mut self, drop: &PaneDrop, cx: &mut Context<Self>) {
        let across_tabs = drop.dst_tab_id != drop.src_tab_id;
        let mut tab_ids = vec![drop.src_tab_id.as_str()];
        if across_tabs {
            tab_ids.push(drop.dst_tab_id.as_str());
        }
        let snapshots = tab_ids
            .into_iter()
            .filter_map(|id| {
                let focus = if drop.swap && id == drop.dst_tab_id {
                    drop.dst_pane_id.as_str()
                } else {
                    drop.src_pane_id.as_str()
                };
                self.tab_snapshot(id, Some(focus))
            })
            .collect();
        self.record_undo(drop.undo_label.to_owned(), snapshots, cx);
        if across_tabs && !drop.swap {
            self.tabs
                .mark_closing_if_last_pane(&drop.src_tab_id, drop.request_id.as_deref());
        }
        let message = drop.message();
        self.tabs.note_layout_request(&message);
        self.send(message);
        if drop.swap {
            self.tabs.focus_pane(&drop.dst_tab_id, &drop.dst_pane_id);
        }
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
                self.move_pane_to_tab(&tab_id, &pane_id, dst_tab_id, cx);
            }
            MenuAction::Close => {
                self.record_pane_close(&tab_id, &pane_id, None, cx);
                let request_id = crate::new_request_id();
                self.tabs
                    .mark_closing_if_last_pane(&tab_id, Some(&request_id));
                self.send(ClientMessage::ClosePane {
                    tab_id,
                    pane_id,
                    request_id: Some(request_id),
                });
            }
        }
        self.close_empty_pane_menu(window, cx);
    }

    /// The open empty-pane menu over a layer that keeps a click outside it
    /// from reaching what lies beneath, while window `here` draws it.
    pub(crate) fn empty_pane_menu_layer(
        &self,
        here: WindowId,
        cx: &mut Context<Self>,
    ) -> Option<[AnyElement; 2]> {
        if !self.overlays.shows(Overlay::EmptyPaneMenu, here) {
            return None;
        }
        let menu = self.pane_ui.menu.as_ref()?;
        let rows: Vec<AnyElement> = self
            .empty_pane_menu_lines(menu)
            .into_iter()
            .map(|line| match line {
                None => menu_separator(&theme(cx)).into_any_element(),
                Some(row) => empty_pane_row(row, cx).into_any_element(),
            })
            .collect();
        let frame = menu_frame(
            "empty-pane-menu",
            &self.pane_ui.menu_focus,
            menu.max_height,
            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                this.close_empty_pane_menu(window, cx);
                cx.stop_propagation();
            }),
            &theme(cx),
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
    /// returns whether it was taken. Only the window that draws the dialog
    /// or the menu hands it keys. The menu takes only Esc. The Move panes
    /// dialog's keys go through [`Self::on_move_panes_key`].
    pub(crate) fn on_pane_ui_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let here = window.window_handle().window_id();
        if self.pane_ui.close.is_some() && self.overlays.shows(Overlay::PaneClose, here) {
            self.on_pane_close_key(keystroke, window, cx);
            return true;
        }
        if self.pane_ui.menu.is_some()
            && self.overlays.shows(Overlay::EmptyPaneMenu, here)
            && keystroke.key == "escape"
        {
            self.close_empty_pane_menu(window, cx);
            return true;
        }
        false
    }
}

/// A row of the empty pane's menu, with its hover text when it has one.
fn empty_pane_row(row: MenuRow, cx: &mut Context<RootView>) -> Stateful<Div> {
    let item = menu_item(&row.selector, row.label, false, &theme(cx));
    let item = match row.tip {
        Some(tip) => item.tooltip(tooltip(tip)),
        None => item,
    };
    let action = row.action;
    item.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.run_empty_pane_action(&action, window, cx);
    }))
}
