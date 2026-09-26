//! The session context menu, the pane header's Stop and the stopped-pane
//! overlay: rendering and forwarding to [`crate::session_actions`].

use gpui::{
    AnyElement, ClickEvent, Context, Div, ElementId, Entity, Focusable as _, FontWeight, Keystroke,
    MouseButton, MouseDownEvent, Pixels, Point, SharedString, Stateful, Subscription, Task, Window,
    anchored, deferred, div, prelude::*, px,
};
use protocol::{MemberBranchFate, SessionSnapshot, TabEntry};

use crate::branch_fate::{DeleteWorktreeConfirm, DialogButton, confirm_messages};
use crate::grid_view::{NO_REPOS_TIP, PANE_PENDING_TIP};
use crate::session_actions::{
    MenuEntry, MenuMode, SessionAction, Step, exit_code_label, exited_message,
    header_shows_exit_code, menu_entries, overlay_actions, pane_shows_exit, plan, rename_message,
};
use crate::tabs::{collect_panes, find_tab_containing_session};
use crate::text_input::{TextInput, TextInputEvent};
use crate::{
    BORDER, DANGER, DANGER_BG, HOVER_BG, MUTED, PANEL_BG, RootView, TEXT, UI_TEXT_SIZE, tooltip,
};

const MENU_WIDTH: f32 = 240.0;
const NEW_SESSION_TIP: &str = "Open the spawn dialog; the new session takes over this pane";
/// The stopped-pane overlay: translucent, so the terminal shows through.
const OVERLAY_TINT: u32 = 0x1e1e_1ecc;
/// The dim layer behind the delete-worktree confirm.
pub(crate) const BACKDROP_TINT: u32 = 0x0000_0099;
const DIALOG_WIDTH: f32 = 440.0;

/// `panel` centred over a tinted layer that takes every click beneath it, and
/// tags it `selector`; a click on the layer itself does nothing.
pub(crate) fn backdrop(selector: &'static str, panel: Stateful<Div>) -> AnyElement {
    div()
        .id(selector)
        .debug_selector(|| selector.to_owned())
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::rgba(BACKDROP_TINT))
        .occlude()
        .child(panel)
        .into_any_element()
}

/// Who opened the delete-worktree confirm, and so what its answer does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfirmOwner {
    /// The session menu or the exited overlay: a delete answer sends the
    /// stop and the discard.
    Menu,
    /// The quit's branch-fate walk, at session `index` of `total`: the
    /// answer is recorded and the walk moves on; Cancel ends the walk.
    ExitWalk { index: usize, total: usize },
}

impl ConfirmOwner {
    /// The walk's "Session n of m" line under the heading.
    fn progress(self) -> Option<String> {
        match self {
            Self::Menu => None,
            Self::ExitWalk { index, total } => Some(format!("Session {index} of {total}")),
        }
    }
}

/// The open delete-worktree confirm.
pub(crate) struct DeleteDialog {
    confirm: DeleteWorktreeConfirm,
    owner: ConfirmOwner,
    /// Wakes the dialog at the preview's deadline; replacing or dropping
    /// it cancels the wake-up.
    timer: Option<Task<()>>,
}

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
        let session_id = self.confirm.armed()?;
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

    /// Closes the menu and the menu's delete-worktree confirm of a session
    /// the daemon no longer lists. The quit's walk skips such a session
    /// itself.
    pub(crate) fn drop_stale_session_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .menu
            .as_ref()
            .is_some_and(|menu| self.sidebar.session(&menu.session_id).is_none())
        {
            self.close_session_menu(window, cx);
        }
        if self.delete_dialog.as_ref().is_some_and(|dialog| {
            dialog.owner == ConfirmOwner::Menu
                && self.sidebar.session(dialog.confirm.session_id()).is_none()
        }) {
            self.close_delete_dialog(window, cx);
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

    /// Carries out `action` on `session_id`, disarming the header Stop. A
    /// worktree delete opens the delete-worktree confirm in the menu's
    /// place. A restart already on the way does nothing.
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
        self.confirm.disarm();
        match plan(action, &session, self.is_shown(session_id)) {
            Step::ConfirmWorktreeDelete => {
                self.close_session_menu(window, cx);
                self.open_delete_dialog(session_id, ConfirmOwner::Menu, window, cx);
            }
            Step::Send(messages) => {
                for msg in messages {
                    self.send(msg);
                }
                self.close_session_menu(window, cx);
            }
            Step::EditName => self.start_session_rename(&session, window, cx),
            Step::Duplicate => {
                let request_id = crate::new_request_id();
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

    /// Closes the menu and the delete-worktree confirm and drops every
    /// armed confirm, as when the connection goes.
    pub(crate) fn reset_session_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_session_menu(window, cx);
        self.close_delete_dialog(window, cx);
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

    /// An action's text: on the way, or as it stands.
    fn action_label(
        &self,
        session: &SessionSnapshot,
        action: SessionAction,
        in_pane: bool,
    ) -> &'static str {
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
            MenuEntry::StopChoice => menu_item(&row.selector, row.label, true)
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.set_menu_mode(MenuMode::StopChoice, window, cx);
                }))
                .into_any_element(),
            MenuEntry::Cancel => menu_item(&row.selector, row.label, false)
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
        let button = menu_item(selector, label, is_danger(action));
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
        if !self.confirm.is_armed(&id) {
            let button = header_text_button(&format!("pane-stop-{pane_id}"), "Stop", MUTED);
            return vec![
                on_press(button, cx, move |this, _, _| {
                    this.confirm.click(&id);
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
                if this.confirm.click(&id) {
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

    /// The overlay's "New session…": the spawn dialog aimed at this pane,
    /// whose new session replaces the stopped one. Disabled with no repo,
    /// and while a spawn aimed at the pane is on its way.
    fn overlay_new_session(
        &self,
        tab_id: &str,
        pane_id: &str,
        session_id: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pending = self.spawns.aims_at(tab_id, pane_id);
        let tip = if !self.has_repos() {
            NO_REPOS_TIP
        } else if pending {
            PANE_PENDING_TIP
        } else {
            NEW_SESSION_TIP
        };
        let button = BorderedButton {
            selector: format!("exited-new-session-{pane_id}"),
            label: "New session…",
            tip,
            enabled: self.has_repos() && !pending,
        };
        let (tab_id, pane_id, session_id) =
            (tab_id.to_owned(), pane_id.to_owned(), session_id.to_owned());
        bordered_button(button, cx, move |this, window, cx| {
            this.new_session_in_pane(&tab_id, &pane_id, Some(&session_id), window, cx);
        })
        .into_any_element()
    }

    /// The selectors of the stopped-pane overlays of the tab on screen:
    /// each overlay and its buttons, as [`Self::exited_overlay`] draws them.
    #[must_use]
    pub fn exited_overlay_selectors(&self) -> Vec<String> {
        let Some(grid) = self.tabs.active_tab().and_then(TabEntry::grid) else {
            return Vec::new();
        };
        let mut selectors = Vec::new();
        for pane in collect_panes(grid) {
            let Some(session) = pane
                .session
                .and_then(|id| self.sidebar.session(id))
                .filter(|session| pane_shows_exit(session))
            else {
                continue;
            };
            let mut keys: Vec<&str> = overlay_actions(session)
                .into_iter()
                .map(SessionAction::key)
                .collect();
            keys.insert(1, "new-session");
            selectors.push(format!("exited-{}", pane.id));
            selectors.extend(keys.iter().map(|key| format!("exited-{key}-{}", pane.id)));
        }
        selectors
    }

    /// The layer over an exited session's terminal in `tab_id`: the exit
    /// code and what to do with the pane.
    pub(crate) fn exited_overlay(
        &self,
        tab_id: &str,
        pane_id: &str,
        session_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let session = session_id
            .and_then(|id| self.sidebar.session(id))
            .filter(|session| pane_shows_exit(session))?;
        let mut buttons: Vec<AnyElement> = overlay_actions(session)
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
        buttons.insert(
            1,
            self.overlay_new_session(tab_id, pane_id, &session.id, cx),
        );
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

impl RootView {
    /// The session whose delete-worktree confirm is open.
    #[must_use]
    pub fn delete_dialog_session(&self) -> Option<&str> {
        self.delete_dialog
            .as_ref()
            .map(|dialog| dialog.confirm.session_id())
    }

    /// The confirm's footer buttons as shown: selector and label.
    #[must_use]
    pub fn delete_dialog_buttons(&self) -> Vec<(String, String)> {
        let Some(dialog) = &self.delete_dialog else {
            return Vec::new();
        };
        dialog
            .confirm
            .buttons()
            .into_iter()
            .map(|button| (button.selector().to_owned(), dialog.confirm.label(button)))
            .collect()
    }

    /// The confirm's text: the intro, the status note, then one
    /// "repo branch fate" line per member.
    #[must_use]
    pub fn delete_dialog_text(&self) -> Vec<String> {
        let Some(dialog) = &self.delete_dialog else {
            return Vec::new();
        };
        let confirm = &dialog.confirm;
        let label = self.session_label(confirm.session_id());
        let mut lines: Vec<String> = dialog.owner.progress().into_iter().collect();
        lines.push(format!("Removing the worktree for {label}."));
        lines.extend(confirm.status_note().map(str::to_owned));
        lines.extend(
            confirm
                .member_rows()
                .into_iter()
                .map(|row| format!("{} {} {}", row.repo, row.branch, row.fate)),
        );
        lines
    }

    /// The selector of the confirm's focused button.
    #[must_use]
    pub fn delete_dialog_focus(&self) -> Option<String> {
        self.delete_dialog
            .as_ref()
            .map(|dialog| dialog.confirm.focused().selector().to_owned())
    }

    /// Whether the open confirm is the quit's branch-fate walk.
    pub(crate) fn exit_walk_confirm_open(&self) -> bool {
        self.delete_dialog
            .as_ref()
            .is_some_and(|dialog| matches!(dialog.owner, ConfirmOwner::ExitWalk { .. }))
    }

    /// Opens the confirm for `session_id` on behalf of `owner`, replacing
    /// any confirm open: asks the daemon for its branch fates, arms the
    /// fallback at the preview's deadline and takes the keyboard.
    pub(crate) fn open_delete_dialog(
        &mut self,
        session_id: &str,
        owner: ConfirmOwner,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let confirm = DeleteWorktreeConfirm::new(session_id, (self.now)());
        self.send(confirm.request());
        self.delete_dialog = Some(DeleteDialog {
            confirm,
            owner,
            timer: None,
        });
        self.schedule_delete_dialog_tick(cx);
        self.dialog_focus.focus(window);
        cx.notify();
    }

    /// Arms a timer for the rest of the wait for the preview, or disarms it
    /// once the wait is over.
    fn schedule_delete_dialog_tick(&mut self, cx: &mut Context<Self>) {
        let now = (self.now)();
        let Some(dialog) = &mut self.delete_dialog else {
            return;
        };
        dialog.timer = dialog.confirm.deadline().map(|deadline| {
            let delay = deadline.saturating_duration_since(now);
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                // Fails only when the view is gone, and the dialog with it.
                this.update(cx, Self::tick_delete_dialog).ok();
            })
        });
    }

    /// The timer fired: fall back when the deadline has passed, else wait
    /// out the rest, as the clock may lag the timer.
    fn tick_delete_dialog(&mut self, cx: &mut Context<Self>) {
        let now = (self.now)();
        let Some(dialog) = &mut self.delete_dialog else {
            return;
        };
        if dialog.confirm.tick(now) {
            cx.notify();
        }
        self.schedule_delete_dialog_tick(cx);
    }

    /// The daemon's branch fates for the open confirm's session.
    pub(crate) fn on_discard_preview(
        &mut self,
        session_id: &str,
        members: &[MemberBranchFate],
        cx: &mut Context<Self>,
    ) {
        if let Some(dialog) = &mut self.delete_dialog
            && dialog.confirm.on_preview(session_id, members)
        {
            cx.notify();
        }
    }

    /// Closes the confirm and hands the keyboard back to the active pane;
    /// closing the quit's walk ends the walk and returns to the exit dialog.
    pub(crate) fn close_delete_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.delete_dialog.take() else {
            return;
        };
        match dialog.owner {
            ConfirmOwner::Menu => self.focus_active_pane(window, cx),
            ConfirmOwner::ExitWalk { .. } => self.cancel_exit_walk(window, cx),
        }
        cx.notify();
    }

    /// The user's answer. For the menu, a delete sends the confirm's
    /// messages for the session as it stands now; Cancel sends nothing. For
    /// the quit's walk, a delete records the branch choice and Cancel ends
    /// the walk.
    fn answer_delete_dialog(
        &mut self,
        button: DialogButton,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = self.delete_dialog.take() else {
            return;
        };
        if let ConfirmOwner::ExitWalk { .. } = dialog.owner {
            match button.branch() {
                Some(branch) => {
                    self.answer_exit_walk(dialog.confirm.session_id(), branch, window, cx);
                }
                None => self.cancel_exit_walk(window, cx),
            }
            cx.notify();
            return;
        }
        let session = self.sidebar.session(dialog.confirm.session_id()).cloned();
        if let (Some(branch), Some(session)) = (button.branch(), session) {
            for msg in confirm_messages(&session, branch) {
                self.send(msg);
            }
        }
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    /// A key while the confirm is open: Esc cancels, Enter and Space press
    /// the focused button, Tab and Shift+Tab move the focus.
    pub(crate) fn on_delete_dialog_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match keystroke.key.as_str() {
            "escape" => self.close_delete_dialog(window, cx),
            "enter" | "space" => {
                if let Some(button) = self.delete_dialog.as_ref().map(|d| d.confirm.focused()) {
                    self.answer_delete_dialog(button, window, cx);
                }
            }
            "tab" => {
                if let Some(dialog) = &mut self.delete_dialog {
                    dialog.confirm.move_focus(!keystroke.modifiers.shift);
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    /// The confirm over a backdrop that takes every click beneath it; a
    /// click on the backdrop itself does nothing.
    pub(crate) fn delete_dialog_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.delete_dialog.as_ref()?;
        let confirm = &dialog.confirm;
        let close = dialog_button("delete-worktree-dialog-close", "✕".to_owned(), false, false)
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.close_delete_dialog(window, cx);
            }));
        let progress = dialog
            .owner
            .progress()
            .map(|line| div().text_color(gpui::rgb(MUTED)).child(line));
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Delete worktree?"),
                    )
                    .children(progress),
            )
            .child(close);
        let buttons: Vec<AnyElement> = confirm
            .buttons()
            .into_iter()
            .map(|button| {
                let focused = confirm.focused() == button;
                dialog_button(
                    button.selector(),
                    confirm.label(button),
                    button.is_danger(),
                    focused,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.answer_delete_dialog(button, window, cx);
                }))
                .into_any_element()
            })
            .collect();
        let panel = div()
            .id("delete-worktree-panel")
            .track_focus(&self.dialog_focus)
            .flex()
            .flex_col()
            .gap(px(10.0))
            .w(px(DIALOG_WIDTH))
            .p(px(14.0))
            .bg(gpui::rgb(PANEL_BG))
            .border_1()
            .border_color(gpui::rgb(BORDER))
            .rounded(px(6.0))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(TEXT))
            .child(header)
            .child(self.delete_dialog_body(confirm))
            .child(div().flex().justify_end().gap(px(6.0)).children(buttons));
        Some(backdrop("delete-worktree-dialog", panel))
    }

    /// Whose worktree goes, the wait or its failure, and each member's
    /// branch with its fate.
    fn delete_dialog_body(&self, confirm: &DeleteWorktreeConfirm) -> Div {
        let intro = div()
            .flex()
            .flex_wrap()
            .child("Removing the worktree for ")
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .child(self.session_label(confirm.session_id())),
            )
            .child(".");
        let note = confirm
            .status_note()
            .map(|note| div().text_color(gpui::rgb(MUTED)).child(note));
        let members = confirm.member_rows().into_iter().map(|row| {
            div()
                .flex()
                .flex_wrap()
                .gap(px(8.0))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(row.repo))
                .child(div().text_color(gpui::rgb(MUTED)).child(row.branch))
                .child(row.fate)
        });
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(intro)
            .children(note)
            .children(members)
    }
}

/// A button of the delete-worktree confirm; the focused one is outlined.
pub(crate) fn dialog_button(
    selector: &str,
    label: String,
    danger: bool,
    focused: bool,
) -> Stateful<Div> {
    let name = selector.to_owned();
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .px(px(10.0))
        .py(px(4.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(if focused { TEXT } else { BORDER }))
        .cursor_pointer()
        .text_color(gpui::rgb(if danger { DANGER } else { TEXT }))
        .when(danger, |button| button.bg(gpui::rgb(DANGER_BG)))
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .child(label)
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

/// A bordered text button of an empty pane, the empty main area or the
/// stopped-pane overlay.
pub(crate) struct BorderedButton {
    pub selector: String,
    pub label: &'static str,
    pub tip: &'static str,
    pub enabled: bool,
}

/// `button`, which runs `act` on a left press as [`on_press`] does. A
/// disabled one is dimmed, has no hover and the default cursor, and its
/// press only stops at it.
pub(crate) fn bordered_button(
    button: BorderedButton,
    cx: &mut Context<RootView>,
    act: impl Fn(&mut RootView, &mut Window, &mut Context<RootView>) + 'static,
) -> Stateful<Div> {
    let name = button.selector;
    let base = div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .px(px(8.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(BORDER))
        .text_color(gpui::rgb(TEXT))
        .tooltip(tooltip(button.tip))
        .child(button.label);
    if button.enabled {
        let base = base
            .cursor_pointer()
            .hover(|style| style.bg(gpui::rgb(HOVER_BG)));
        on_press(base, cx, act)
    } else {
        base.opacity(0.6)
            .cursor_default()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
    }
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
fn menu_item(selector: &str, label: &'static str, danger: bool) -> Stateful<Div> {
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
