//! The Manage worktrees modal on screen: its rows and confirms, its keys,
//! and the Launch hand-off to the spawn dialog. The model is
//! [`crate::worktrees_manager`].

use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, Keystroke, Stateful, Window, div, prelude::*,
    px,
};
use protocol::{ClientMessage, DaemonMessage, RootWorktreeEntry, RootWorktreeStatus};

use crate::notice_view::modal_panel;
use crate::notices::ToastKind;
use crate::session_menu::{backdrop, dialog_button};
use crate::settings_view::SettingsControl;
use crate::spawn_form::Lock;
use crate::spawn_view::{SpawnEntry, now_unix};
use crate::worktrees_manager::{
    Action, Confirm, ConfirmButton, Control, EMPTY, SCANNING, TITLE, WorktreesManager,
};
use crate::{BORDER, MUTED, RootView, tooltip};

/// The most of the window's height the rows take before they scroll.
const LIST_MAX_SHARE: f32 = 0.6;

/// A button of the manager or of its open confirm, as drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreesButton {
    pub selector: String,
    pub label: String,
    pub enabled: bool,
    pub tooltip: Option<String>,
}

/// A row of the manager as it reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreesRow {
    pub title: String,
    pub lines: Vec<String>,
    /// Each member's line while the members are expanded.
    pub members: Option<Vec<String>>,
}

impl RootView {
    /// Whether the Manage worktrees modal is open.
    #[must_use]
    pub fn worktrees_manager_open(&self) -> bool {
        self.worktrees_manager.is_some()
    }

    /// The open confirm's buttons, else the manager's, enabled or not, in
    /// the keyboard's order.
    #[must_use]
    pub fn worktrees_manager_buttons(&self) -> Vec<WorktreesButton> {
        let Some(manager) = &self.worktrees_manager else {
            return Vec::new();
        };
        if let Some(confirm) = manager.confirm() {
            return [ConfirmButton::Cancel, ConfirmButton::Ok]
                .map(|button| WorktreesButton {
                    selector: confirm.selector(button).to_owned(),
                    label: confirm.label(button).to_owned(),
                    enabled: true,
                    tooltip: None,
                })
                .to_vec();
        }
        manager
            .all_controls()
            .iter()
            .map(|control| WorktreesButton {
                selector: manager.selector(control),
                label: manager.label(control),
                enabled: manager.enabled(control),
                tooltip: manager.tooltip(control),
            })
            .collect()
    }

    /// The selector of the focused button, the open confirm's first.
    #[must_use]
    pub fn worktrees_manager_focus(&self) -> Option<String> {
        let manager = self.worktrees_manager.as_ref()?;
        Some(match manager.confirm() {
            Some(confirm) => confirm.selector(confirm.focused()).to_owned(),
            None => manager.selector(&manager.focused()),
        })
    }

    /// The rows, in the snapshot's order.
    #[must_use]
    pub fn worktrees_manager_rows(&self) -> Vec<WorktreesRow> {
        let Some(manager) = &self.worktrees_manager else {
            return Vec::new();
        };
        let now = now_unix();
        manager
            .entries()
            .iter()
            .map(|entry| {
                let row = manager.row(entry, now);
                WorktreesRow {
                    title: row.title,
                    lines: row.lines,
                    members: row.members,
                }
            })
            .collect()
    }

    /// The lines over the rows: the root, then the scanning or empty note
    /// when one shows.
    #[must_use]
    pub fn worktrees_manager_lines(&self) -> Vec<String> {
        let Some(manager) = &self.worktrees_manager else {
            return Vec::new();
        };
        let known = self
            .worktrees_root
            .as_ref()
            .map(|(root, is_override)| (root.as_str(), *is_override));
        let mut lines = vec![manager.root_line(known)];
        if manager.scanning() {
            lines.push(SCANNING.to_owned());
        } else if manager.empty() {
            lines.push(EMPTY.to_owned());
        }
        lines
    }

    /// The open confirm's title, body and hint.
    #[must_use]
    pub fn worktrees_manager_confirm(&self) -> Option<(String, String, String)> {
        let confirm = self.worktrees_manager.as_ref()?.confirm()?;
        Some((
            confirm.title().to_owned(),
            confirm.body(),
            confirm.hint().to_owned(),
        ))
    }

    /// Opens the manager, which scans the worktrees root.
    pub(crate) fn open_worktrees_manager(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.worktrees_manager.is_some() {
            return;
        }
        let (manager, msg) = WorktreesManager::open();
        tracing::info!("worktrees manager: scanning the worktrees root");
        self.send(msg);
        self.worktrees_manager = Some(manager);
        self.worktrees_focus.focus(window);
        cx.notify();
    }

    /// Closes the manager; the keyboard goes back to Settings' Manage
    /// worktrees… button.
    fn close_worktrees_manager(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.worktrees_manager.take().is_none() {
            return;
        }
        if self.settings_open() {
            self.focus_settings_control(SettingsControl::WorktreesManage, window);
        } else {
            self.focus_active_pane(window, cx);
        }
        cx.notify();
    }

    /// A new connection: the root is heard afresh and the manager closes.
    pub(crate) fn reset_worktrees(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reset_worktrees_tab();
        if self.worktrees_manager.take().is_some() {
            self.focus_active_pane(window, cx);
            cx.notify();
        }
    }

    /// The daemon's worktrees root, and the snapshots and errors that
    /// answer the manager's requests. Every other message is left alone.
    pub(crate) fn on_worktrees_message(&mut self, msg: &DaemonMessage, cx: &mut Context<Self>) {
        match msg {
            DaemonMessage::WorktreesRootChanged { root, is_override } => {
                self.on_worktrees_root_changed(root, *is_override, cx);
                // An idle open manager follows the new root.
                if let Some(manager) = self
                    .worktrees_manager
                    .as_mut()
                    .filter(|manager| !manager.pending())
                {
                    manager.request_scan();
                    self.send(ClientMessage::InspectWorktreesRoot);
                    cx.notify();
                }
            }
            DaemonMessage::WorktreesRootSnapshot {
                root,
                is_override,
                entries,
            } => {
                if let Some(manager) = &mut self.worktrees_manager {
                    manager.on_snapshot(root, *is_override, entries);
                    cx.notify();
                }
            }
            DaemonMessage::Error { .. } => {
                if let Some(manager) = &mut self.worktrees_manager {
                    manager.on_error();
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    /// A key while the manager is open; it takes every key. With a confirm
    /// open, Tab and Shift+Tab switch its buttons, Enter and Space press
    /// the focused one and Esc cancels. Otherwise Tab and Shift+Tab move
    /// the focus, Enter and Space press, and Esc closes the manager.
    pub(crate) fn on_worktrees_manager_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(manager) = &mut self.worktrees_manager else {
            return;
        };
        let key = keystroke.key.as_str();
        if let Some(confirm) = manager.confirm_mut() {
            match key {
                "tab" => confirm.toggle_focus(),
                "enter" | "space" => {
                    let button = confirm.focused();
                    return self.answer_worktrees_confirm(button, window, cx);
                }
                "escape" => {
                    return self.answer_worktrees_confirm(ConfirmButton::Cancel, window, cx);
                }
                _ => {}
            }
            cx.notify();
            return;
        }
        match key {
            "tab" => manager.move_focus(!keystroke.modifiers.shift),
            "enter" | "space" => {
                let control = manager.focused();
                return self.press_worktrees_control(&control, window, cx);
            }
            "escape" => return self.close_worktrees_manager(window, cx),
            _ => {}
        }
        cx.notify();
    }

    fn press_worktrees_control(
        &mut self,
        control: &Control,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(manager) = &mut self.worktrees_manager else {
            return;
        };
        let action = manager.press(control);
        self.apply_worktrees_action(action, window, cx);
    }

    fn answer_worktrees_confirm(
        &mut self,
        button: ConfirmButton,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(manager) = &mut self.worktrees_manager else {
            return;
        };
        let action = manager.answer(button);
        self.apply_worktrees_action(action, window, cx);
    }

    fn apply_worktrees_action(
        &mut self,
        action: Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            Action::Nothing => {}
            Action::Send(messages) => {
                tracing::info!(requests = messages.len(), "worktrees manager: sending");
                for msg in messages {
                    self.send(msg);
                }
            }
            Action::Close => return self.close_worktrees_manager(window, cx),
            Action::Launch(entry) => return self.launch_into_worktree(&entry, window, cx),
        }
        cx.notify();
    }

    /// Closes the manager and Settings, then opens the spawn dialog held on
    /// the group's repo or workspace and pinned to its worktree.
    fn launch_into_worktree(
        &mut self,
        entry: &RootWorktreeEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(lock) = Lock::from_group(entry) else {
            tracing::warn!(path = %entry.path, "worktrees manager: the group has no launch target");
            return;
        };
        // An Active group's launch came through the manager's own share
        // confirm, so the dialog does not ask for the pin again.
        let lock = if entry.status == RootWorktreeStatus::Active {
            lock.share_confirmed()
        } else {
            lock
        };
        // Tear the manager and Settings down only once the dialog can
        // really open: a target that left the registry keeps both up.
        if let Some(reason) = self.spawn_dialog_blocker(Some(&lock), true) {
            tracing::warn!(path = %entry.path, reason, "worktrees manager: cannot launch here");
            let title = format!("Can't launch here: {reason}");
            self.push_toast(ToastKind::Warning, &title, None, cx);
            return;
        }
        tracing::info!(path = %entry.path, "worktrees manager: launching into the group");
        self.worktrees_manager = None;
        self.close_appearance_editor(window, cx);
        self.open_spawn_dialog(SpawnEntry::Locked(lock), window, cx);
        cx.notify();
    }

    /// The manager, and its open confirm over it, each over a backdrop that
    /// takes every click beneath it.
    pub(crate) fn worktrees_manager_layer(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let manager = self.worktrees_manager.as_ref()?;
        let focused = manager.confirm().is_none().then(|| manager.focused());
        let focused = focused.as_ref();
        let mut lines = self.worktrees_manager_lines().into_iter();
        let root = lines.next().unwrap_or_default();
        let toolbar = div()
            .flex()
            .gap(px(8.0))
            .child(manager_button(
                manager,
                Control::Refresh,
                false,
                focused,
                cx,
            ))
            .child(manager_button(
                manager,
                Control::DeleteStale,
                true,
                focused,
                cx,
            ));
        let now = now_unix();
        let rows: Vec<Div> = manager
            .entries()
            .iter()
            .map(|entry| manager_row(manager, entry, now, focused, cx))
            .collect();
        let list = div()
            .id("worktrees-manager-list")
            .max_h(window.viewport_size().height * LIST_MAX_SHARE)
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .children(rows);
        let footer = div().flex().justify_end().child(manager_button(
            manager,
            Control::Close,
            false,
            focused,
            cx,
        ));
        let panel = modal_panel("worktrees-manager-panel")
            .track_focus(&self.worktrees_focus)
            .child(div().font_weight(FontWeight::SEMIBOLD).child(TITLE))
            .child(muted(root))
            .child(toolbar)
            .children(lines.map(|line| div().child(line)))
            .child(list)
            .child(footer);
        let confirm = manager.confirm().map(|confirm| confirm_layer(confirm, cx));
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .child(backdrop("worktrees-manager", panel))
                .children(confirm)
                .into_any_element(),
        )
    }
}

/// One group: its title, the lines under it, its members while expanded,
/// and its buttons.
fn manager_row(
    manager: &WorktreesManager,
    entry: &RootWorktreeEntry,
    now: i64,
    focused: Option<&Control>,
    cx: &mut Context<RootView>,
) -> Div {
    let row = manager.row(entry, now);
    let mut lines = row.lines.into_iter();
    let status = lines.next().unwrap_or_default();
    let meta: Vec<String> = lines.collect();
    let members = (!entry.members.is_empty()).then(|| {
        manager_button(
            manager,
            Control::Members(entry.path.clone()),
            false,
            focused,
            cx,
        )
    });
    let member_lines = row
        .members
        .unwrap_or_default()
        .into_iter()
        .map(|line| muted(line).pl(px(12.0)));
    let actions = div()
        .flex()
        .gap(px(8.0))
        .child(manager_button(
            manager,
            Control::Launch(entry.path.clone()),
            false,
            focused,
            cx,
        ))
        .child(manager_button(
            manager,
            Control::Delete(entry.path.clone()),
            true,
            focused,
            cx,
        ));
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .p(px(6.0))
        .border_1()
        .border_color(gpui::rgb(BORDER))
        .rounded(px(4.0))
        .child(div().font_weight(FontWeight::SEMIBOLD).child(row.title))
        .child(div().child(status))
        .children(meta.into_iter().map(muted))
        .children(members)
        .children(member_lines)
        .child(actions)
}

/// A button of the manager, outlined while focused; a disabled one is
/// dimmed and inert, and a tooltip says why when the model gives one.
fn manager_button(
    manager: &WorktreesManager,
    control: Control,
    danger: bool,
    focused: Option<&Control>,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let button = dialog_button(
        &manager.selector(&control),
        manager.label(&control),
        danger,
        focused == Some(&control),
    );
    let button = match manager.tooltip(&control) {
        Some(tip) => button.tooltip(tooltip(tip)),
        None => button,
    };
    if !manager.enabled(&control) {
        return button.opacity(0.5).cursor_default();
    }
    button.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.press_worktrees_control(&control, window, cx);
    }))
}

/// A confirm over the manager: its title, body and hint over Cancel and
/// the button that goes ahead.
fn confirm_layer(confirm: &Confirm, cx: &mut Context<RootView>) -> AnyElement {
    let buttons = [ConfirmButton::Cancel, ConfirmButton::Ok].map(|button| {
        dialog_button(
            confirm.selector(button),
            confirm.label(button).to_owned(),
            button == ConfirmButton::Ok,
            confirm.focused() == button,
        )
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.answer_worktrees_confirm(button, window, cx);
        }))
    });
    let panel = modal_panel("worktrees-manager-confirm-panel")
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child(confirm.title()),
        )
        .child(div().child(confirm.body()))
        .child(muted(confirm.hint()))
        .child(div().flex().justify_end().gap(px(8.0)).children(buttons));
    backdrop("worktrees-manager-confirm", panel)
}

fn muted(text: impl Into<gpui::SharedString>) -> Div {
    div().text_color(gpui::rgb(MUTED)).child(text.into())
}
