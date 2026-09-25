//! The session context menu, the pane header's Stop and the stopped-pane
//! overlay: rendering and forwarding to [`crate::session_actions`].

use gpui::{
    AnyElement, ClickEvent, Context, Div, ElementId, Entity, Focusable as _, MouseButton,
    MouseDownEvent, Pixels, Point, SharedString, Stateful, Subscription, Window, anchored,
    deferred, div, prelude::*, px,
};
use protocol::{SessionSnapshot, TabEntry};

use crate::session_actions::{
    MenuEntry, MenuMode, SessionAction, Step, exit_code_label, exited_message,
    header_shows_exit_code, menu_entries, overlay_actions, pane_shows_exit, plan, rename_message,
};
use crate::tabs::{collect_panes, find_tab_containing_session};
use crate::text_input::{TextInput, TextInputEvent};
use crate::{BORDER, DANGER, DANGER_BG, HOVER_BG, MUTED, PANEL_BG, RootView, TEXT, UI_TEXT_SIZE};

const MENU_WIDTH: f32 = 240.0;
/// A worktree-deleting button after its first click.
const ARMED_LABEL: &str = "Confirm delete worktree";
/// The stopped-pane overlay: translucent, so the terminal shows through.
const OVERLAY_TINT: u32 = 0x1e1e_1ecc;

/// The open context menu.
pub(crate) struct SessionMenu {
    session_id: String,
    /// Where the right-click was, in window coordinates.
    at: Point<Pixels>,
    mode: MenuMode,
    /// The name editor, which replaces the rows while renaming.
    rename: Option<(Entity<TextInput>, Subscription)>,
}

/// A menu row as shown: its selector, its text, and what it does.
struct Row {
    selector: String,
    label: &'static str,
    entry: MenuEntry,
}

impl RootView {
    /// The session whose context menu is open.
    #[must_use]
    pub fn session_menu(&self) -> Option<&str> {
        self.menu.as_ref().map(|menu| menu.session_id.as_str())
    }

    /// A pane showing the session whose header Stop waits for its confirm.
    #[must_use]
    pub fn armed_stop_pane(&self) -> Option<&str> {
        let (session_id, action) = self.confirm.armed()?;
        if action != SessionAction::StopKeepWorktree {
            return None;
        }
        self.tabs
            .tabs()
            .iter()
            .filter_map(TabEntry::grid)
            .flat_map(collect_panes)
            .find(|pane| pane.session == Some(session_id))
            .map(|pane| pane.id)
    }

    /// The selectors of the menu's rows as shown now.
    #[must_use]
    pub fn menu_rows(&self) -> Vec<String> {
        match self.menu.as_ref() {
            Some(menu) if menu.rename.is_some() => vec!["menu-rename-input".to_owned()],
            _ => self.rows().into_iter().map(|row| row.selector).collect(),
        }
    }

    /// The labels of the menu's rows as shown now.
    #[must_use]
    pub fn menu_labels(&self) -> Vec<String> {
        self.rows()
            .into_iter()
            .map(|row| row.label.to_owned())
            .collect()
    }

    /// Whether a restart or resume of `session_id` waits for its reply.
    #[must_use]
    pub fn restart_pending(&self, session_id: &str) -> bool {
        self.duplicates.is_pending(session_id)
    }

    /// The active tab's focused pane.
    #[must_use]
    pub fn focused_pane(&self) -> Option<String> {
        self.tabs.focused_pane(self.tabs.active_id()?)
    }

    /// Opens the menu of `session_id` at `at`, taking the keyboard so Esc
    /// reaches it.
    pub(crate) fn open_session_menu(
        &mut self,
        session_id: &str,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sidebar.session(session_id).is_none() {
            return;
        }
        self.menu = Some(SessionMenu {
            session_id: session_id.to_owned(),
            at,
            mode: MenuMode::Actions,
            rename: None,
        });
        self.confirm.disarm();
        self.menu_focus.focus(window);
        cx.notify();
    }

    /// Closes the menu and hands the keyboard back to the active pane.
    pub(crate) fn close_session_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm.disarm();
        if self.menu.take().is_some() {
            self.focus_active_pane(window, cx);
            cx.notify();
        }
    }

    /// Closes the menu of a session the daemon no longer lists.
    pub(crate) fn drop_stale_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .menu
            .as_ref()
            .is_some_and(|menu| self.sidebar.session(&menu.session_id).is_none())
        {
            self.close_session_menu(window, cx);
        }
    }

    fn set_menu_mode(&mut self, mode: MenuMode, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = &mut self.menu else {
            return;
        };
        menu.mode = mode;
        menu.rename = None;
        self.confirm.disarm();
        self.menu_focus.focus(window);
        cx.notify();
    }

    /// Carries out `action` on `session_id`. A worktree delete acts on its
    /// second click only; any other action disarms it. A restart already on
    /// the way does nothing.
    fn choose_action(
        &mut self,
        session_id: &str,
        action: SessionAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sidebar.session(session_id).cloned() else {
            return;
        };
        cx.notify();
        if action.deletes_worktree() {
            if !self.confirm.click(session_id, action) {
                return;
            }
        } else {
            self.confirm.disarm();
        }
        match plan(action, &session, self.is_shown(session_id)) {
            Step::Send(messages) => {
                for msg in messages {
                    self.send(msg);
                }
                self.close_session_menu(window, cx);
            }
            Step::EditName => self.start_session_rename(&session, window, cx),
            Step::Duplicate => {
                let request_id = uuid::Uuid::new_v4().to_string();
                if let Some(request) = self.duplicates.request(session_id, request_id) {
                    self.send(request);
                    self.close_session_menu(window, cx);
                }
            }
        }
    }

    fn is_shown(&self, session_id: &str) -> bool {
        find_tab_containing_session(self.tabs.tabs(), session_id).is_some()
    }

    /// Swaps the menu's rows for a name editor seeded with the name the
    /// sidebar shows. Enter sends the rename; Esc goes back to the rows.
    fn start_session_rename(
        &mut self,
        session: &SessionSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = session
            .user_label
            .clone()
            .unwrap_or_else(|| session.label.clone());
        let input = cx.new(|cx| TextInput::new(name, "Session name", cx));
        let session_id = session.id.clone();
        let events = cx.subscribe_in(
            &input,
            window,
            move |this, input, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => {
                    let text = input.read(cx).text().to_owned();
                    this.send(rename_message(&session_id, &text));
                    this.close_session_menu(window, cx);
                }
                TextInputEvent::Cancel => this.set_menu_mode(MenuMode::Actions, window, cx),
            },
        );
        input.read(cx).focus_handle(cx).focus(window);
        if let Some(menu) = &mut self.menu {
            menu.rename = Some((input, events));
        }
    }

    /// A duplicate that answers one of this client's restarts takes its
    /// original's panes (focusing the first) or a new tab.
    pub(crate) fn place_duplicate(
        &mut self,
        request_id: Option<&str>,
        new_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(request_id) = request_id.filter(|id| self.duplicates.has_request(id)) else {
            return;
        };
        let Some(placed) = self
            .duplicates
            .place(request_id, new_id, &self.tabs.bindings())
        else {
            return;
        };
        self.confirm.disarm();
        if placed.focus.is_none() {
            self.tabs.arm_create();
        }
        for msg in placed.messages {
            self.send(msg);
        }
        if let Some((tab_id, pane_id)) = placed.focus {
            self.tabs.focus_pane(&tab_id, &pane_id);
            self.after_tabs_change(window, cx);
        }
    }

    /// A spawn or duplicate failure: its restart is no longer on the way.
    pub(crate) fn fail_duplicate(&mut self, request_id: Option<&str>) {
        if let Some(id) = request_id {
            self.duplicates.fail(id);
        }
    }

    /// Closes the menu and drops every armed confirm, as when the
    /// connection goes.
    pub(crate) fn reset_session_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_session_menu(window, cx);
    }

    /// The open menu over a layer that keeps a click outside it from
    /// reaching what lies beneath.
    pub(crate) fn session_menu_layer(&self, cx: &mut Context<Self>) -> Option<[AnyElement; 2]> {
        let menu = self.menu.as_ref()?;
        self.sidebar.session(&menu.session_id)?;
        let backdrop = div().absolute().top_0().left_0().size_full().occlude();
        let panel = anchored()
            .position(menu.at)
            .snap_to_window()
            .child(self.menu_panel(menu, cx));
        Some([
            backdrop.into_any_element(),
            deferred(panel).with_priority(1).into_any_element(),
        ])
    }

    fn menu_panel(&self, menu: &SessionMenu, cx: &mut Context<Self>) -> Stateful<Div> {
        let rows: Vec<AnyElement> = match &menu.rename {
            Some((input, _)) => vec![rename_row(input)],
            None => self
                .rows()
                .into_iter()
                .map(|row| self.menu_row(&menu.session_id, &row, cx))
                .collect(),
        };
        let choosing_stop =
            menu.rename.is_none() && self.rows().iter().any(|row| row.entry == MenuEntry::Cancel);
        div()
            .id("session-menu")
            .debug_selector(|| "session-menu".to_owned())
            .track_focus(&self.menu_focus)
            .flex()
            .flex_col()
            .w(px(MENU_WIDTH))
            .p(px(4.0))
            .bg(gpui::rgb(PANEL_BG))
            .border_1()
            .border_color(gpui::rgb(BORDER))
            .rounded(px(6.0))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(TEXT))
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, window, cx| {
                this.close_session_menu(window, cx);
                cx.stop_propagation();
            }))
            .when(choosing_stop, |panel| {
                panel.child(muted_row("Stop session?"))
            })
            .children(rows)
    }

    /// The open menu's rows for its session's current state.
    fn rows(&self) -> Vec<Row> {
        let Some(menu) = &self.menu else {
            return Vec::new();
        };
        let Some(session) = self.sidebar.session(&menu.session_id) else {
            return Vec::new();
        };
        let in_pane = self.is_shown(&session.id);
        menu_entries(session, menu.mode)
            .into_iter()
            .map(|entry| {
                let (selector, label) = match entry {
                    MenuEntry::Action(action) => (
                        format!("menu-{}", action.key()),
                        self.action_label(session, action, in_pane),
                    ),
                    MenuEntry::StopChoice => ("menu-stop".to_owned(), "Stop session…"),
                    MenuEntry::Cancel => ("menu-cancel".to_owned(), "Cancel"),
                };
                Row {
                    selector,
                    label,
                    entry,
                }
            })
            .collect()
    }

    /// An action's text: armed, on the way, or as it stands.
    fn action_label(
        &self,
        session: &SessionSnapshot,
        action: SessionAction,
        in_pane: bool,
    ) -> &'static str {
        if self.confirm.is_armed(&session.id, action) {
            return ARMED_LABEL;
        }
        if self.duplicates.is_pending(&session.id)
            && let Some(pending) = action.pending_label()
        {
            return pending;
        }
        action.label(session.has_per_session_worktree, in_pane)
    }

    fn menu_row(&self, session_id: &str, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        match row.entry {
            MenuEntry::Action(action) => self
                .action_button(session_id, action, &row.selector, row.label, cx)
                .into_any_element(),
            MenuEntry::StopChoice => menu_item(&row.selector, row.label, true, false)
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.set_menu_mode(MenuMode::StopChoice, window, cx);
                }))
                .into_any_element(),
            MenuEntry::Cancel => menu_item(&row.selector, row.label, false, false)
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.set_menu_mode(MenuMode::Actions, window, cx);
                }))
                .into_any_element(),
        }
    }

    /// A button for `action`. It acts on the press, so the root's
    /// press-elsewhere disarm never sees its own confirm; a restart on the
    /// way is inert.
    fn action_button(
        &self,
        session_id: &str,
        action: SessionAction,
        selector: &str,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let armed = self.confirm.is_armed(session_id, action);
        let button = menu_item(selector, label, is_danger(action), armed);
        if action.pending_label().is_some() && self.duplicates.is_pending(session_id) {
            return button.opacity(0.6).cursor_default();
        }
        let session_id = session_id.to_owned();
        button.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.choose_action(&session_id, action, window, cx);
            }),
        )
    }

    /// The pane header's Stop (then "Confirm stop" / "Cancel"), or the exit
    /// code once the session has stopped. Nothing for an empty pane. The
    /// buttons act on the press, as the tab close does.
    pub(crate) fn header_stop(
        &self,
        pane_id: &str,
        session_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(session) = session_id.and_then(|id| self.sidebar.session(id)) else {
            return Vec::new();
        };
        if header_shows_exit_code(session) {
            let name = format!("exit-code-{pane_id}");
            return vec![
                div()
                    .debug_selector(|| name)
                    .flex_none()
                    .px(px(4.0))
                    .child(exit_code_label(session))
                    .into_any_element(),
            ];
        }
        let id = session.id.clone();
        let stop = SessionAction::StopKeepWorktree;
        if !self.confirm.is_armed(&id, stop) {
            let button = header_text_button(&format!("pane-stop-{pane_id}"), "Stop", MUTED);
            return vec![
                on_press(button, cx, move |this, _, _| {
                    this.confirm.click(&id, stop);
                })
                .into_any_element(),
            ];
        }
        let confirm = header_text_button(
            &format!("pane-stop-confirm-{pane_id}"),
            "Confirm stop",
            DANGER,
        );
        let cancel = header_text_button(&format!("pane-stop-cancel-{pane_id}"), "Cancel", TEXT);
        vec![
            on_press(confirm, cx, move |this, window, cx| {
                if this.confirm.click(&id, stop) {
                    this.choose_action(&id, stop, window, cx);
                }
            })
            .into_any_element(),
            on_press(cancel, cx, |this, _, _| {
                this.confirm.disarm();
            })
            .into_any_element(),
        ]
    }

    /// The layer over an exited session's terminal: the exit code and what
    /// to do with the pane.
    pub(crate) fn exited_overlay(
        &self,
        pane_id: &str,
        session_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let session = session_id
            .and_then(|id| self.sidebar.session(id))
            .filter(|session| pane_shows_exit(session))?;
        let buttons: Vec<AnyElement> = overlay_actions(session)
            .into_iter()
            .map(|action| {
                let selector = format!("exited-{}-{pane_id}", action.key());
                let label = self.action_label(session, action, true);
                self.action_button(&session.id, action, &selector, label, cx)
                    .border_1()
                    .border_color(gpui::rgb(BORDER))
                    .into_any_element()
            })
            .collect();
        let name = format!("exited-{pane_id}");
        Some(
            div()
                .id(ElementId::Name(SharedString::from(name.clone())))
                .debug_selector(|| name)
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(10.0))
                .bg(gpui::rgba(OVERLAY_TINT))
                .occlude()
                .text_size(px(UI_TEXT_SIZE))
                .text_color(gpui::rgb(TEXT))
                .child(exited_message(session))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .justify_center()
                        .gap(px(6.0))
                        .children(buttons),
                )
                .into_any_element(),
        )
    }
}

/// Runs `act` on a left press and keeps the press from the root.
fn on_press(
    button: Stateful<Div>,
    cx: &mut Context<RootView>,
    act: impl Fn(&mut RootView, &mut Window, &mut Context<RootView>) + 'static,
) -> Stateful<Div> {
    button.on_mouse_down(
        MouseButton::Left,
        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
            cx.stop_propagation();
            act(this, window, cx);
            cx.notify();
        }),
    )
}

/// Whether an action shows in the danger colour, as in the Tauri menu.
fn is_danger(action: SessionAction) -> bool {
    action.deletes_worktree()
        || matches!(
            action,
            SessionAction::StopKeepWorktree
                | SessionAction::RemovePane
                | SessionAction::DismissAbandoned
        )
}

/// A clickable row of the menu or the overlay.
fn menu_item(selector: &str, label: &'static str, danger: bool, armed: bool) -> Stateful<Div> {
    let name = selector.to_owned();
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .px(px(8.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .cursor_pointer()
        .text_color(gpui::rgb(if danger { DANGER } else { TEXT }))
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .when(armed, |item| item.bg(gpui::rgb(DANGER_BG)))
        .child(label)
}

/// A small text button in a pane header.
fn header_text_button(selector: &str, label: &'static str, color: u32) -> Stateful<Div> {
    let name = selector.to_owned();
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex_none()
        .px(px(4.0))
        .rounded(px(3.0))
        .cursor_pointer()
        .text_color(gpui::rgb(color))
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .child(label)
}

fn muted_row(text: &'static str) -> Div {
    div()
        .px(px(8.0))
        .py(px(3.0))
        .text_color(gpui::rgb(MUTED))
        .child(text)
}

/// The name editor with its hint.
fn rename_row(input: &Entity<TextInput>) -> AnyElement {
    div()
        .debug_selector(|| "menu-rename-input".to_owned())
        .flex()
        .flex_col()
        .gap(px(4.0))
        .p(px(4.0))
        .child(
            div()
                .text_color(gpui::rgb(MUTED))
                .child("Rename session (blank restores the default)"),
        )
        .child(div().w_full().child(input.clone()))
        .into_any_element()
}
