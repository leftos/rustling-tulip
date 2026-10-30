//! The repo/workspace Remove flow's view: the container menu's danger row
//! (armed on its first click, a two-click confirm) and the dialog that lists
//! the container's live sessions, over [`crate::repo_remove`]'s state.

use gpui::{AnyElement, ClickEvent, Context, FontWeight, Keystroke, Window, div, prelude::*, px};

use crate::appearance_view::Level;
use crate::notice_view::modal_panel;
use crate::repo_remove::{
    DIALOG_BODY, RepoRemoveButton, RepoRemoveDialog, RepoRemoveSession, container_key,
    live_owned_sessions, remove_message, row_label, stop_message,
};
use crate::session_menu::{backdrop, dialog_button};
use crate::{MUTED, RootView};

impl RootView {
    /// The name `level`'s container shows.
    fn container_name(&self, level: &Level) -> String {
        match level {
            Level::Repo(id) => self
                .sidebar
                .repos()
                .iter()
                .find(|repo| &repo.id == id)
                .map(|repo| repo.name.clone()),
            Level::Workspace(id) => self
                .sidebar
                .workspaces()
                .iter()
                .find(|workspace| &workspace.id == id)
                .map(|workspace| workspace.name.clone()),
            Level::Session(_) | Level::App => None,
        }
        .unwrap_or_default()
    }

    /// The Remove row of `level`'s container menu was clicked: with live
    /// sessions it opens the dialog; without, the first click arms the row
    /// and the second sends the removal.
    pub(crate) fn click_remove_container(
        &mut self,
        level: &Level,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = container_key(level);
        let sessions = live_owned_sessions(level, self.sidebar.sessions(), &self.sidebar);
        if sessions.is_empty() {
            if self.repo_remove.armed() == Some(key.as_str()) {
                self.repo_remove.disarm();
                if let Some(msg) = remove_message(level) {
                    self.send_to(&self.host_of_container(&key), msg);
                }
                self.close_container_menu(window, cx);
                cx.notify();
            } else {
                self.repo_remove.arm(&key);
                cx.notify();
            }
            return;
        }
        let name = self.container_name(level);
        self.close_container_menu(window, cx);
        self.repo_remove.open_dialog(level.clone(), name, sessions);
        self.repo_remove_focus.focus(window);
        cx.notify();
    }

    /// A key while the dialog is open: Esc cancels, Enter and Space press the
    /// focused button, Tab and Shift+Tab move the focus.
    pub(crate) fn on_repo_remove_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match keystroke.key.as_str() {
            "escape" => self.answer_repo_remove(RepoRemoveButton::Cancel, window, cx),
            "enter" | "space" => {
                if let Some(button) = self.repo_remove.dialog().map(RepoRemoveDialog::focused) {
                    self.answer_repo_remove(button, window, cx);
                }
            }
            "tab" => {
                if let Some(dialog) = self.repo_remove.dialog_mut() {
                    dialog.move_focus(!keystroke.modifiers.shift);
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    /// The user's answer. Cancel closes without sending; both removals send
    /// what they say and leave the dialog open, until the daemon's repo or
    /// workspace list no longer holds the level.
    fn answer_repo_remove(
        &mut self,
        button: RepoRemoveButton,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if button == RepoRemoveButton::Cancel {
            self.close_repo_remove_dialog(window, cx);
            return;
        }
        let Some(dialog) = self.repo_remove.dialog() else {
            return;
        };
        if button == RepoRemoveButton::StopAndRemove {
            for session in dialog.sessions() {
                self.send_to(
                    &self.host_of_session(&session.id),
                    stop_message(&session.id),
                );
            }
        }
        if let Some(msg) = remove_message(dialog.level()) {
            self.send_to(&self.host_of_container(&container_key(dialog.level())), msg);
        }
        cx.notify();
    }

    /// Closes the dialog and hands the keyboard back to the active pane.
    pub(crate) fn close_repo_remove_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.repo_remove.close_dialog().is_some() {
            self.focus_active_pane(window, cx);
            cx.notify();
        }
    }

    /// The Remove dialog over a backdrop that takes every click beneath it; a
    /// click on the backdrop itself does nothing.
    pub(crate) fn repo_remove_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.repo_remove.dialog()?;
        let header = div()
            .flex()
            .flex_col()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(dialog.title()),
            )
            .child(
                div()
                    .text_color(gpui::rgb(MUTED))
                    .child(dialog.name().to_owned()),
            );
        let body = div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(DIALOG_BODY)
            .children(
                dialog
                    .sessions()
                    .iter()
                    .map(|session| div().child(session.row_text())),
            );
        let buttons: Vec<AnyElement> = RepoRemoveButton::ALL
            .into_iter()
            .map(|button| {
                dialog_button(
                    button.selector(),
                    button.label().to_owned(),
                    button.is_danger(),
                    dialog.focused() == button,
                    true,
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.answer_repo_remove(button, window, cx);
                }))
                .into_any_element()
            })
            .collect();
        let panel = modal_panel("repo-remove-panel")
            .track_focus(&self.repo_remove_focus)
            .child(header)
            .child(body)
            .child(div().flex().justify_end().gap(px(6.0)).children(buttons));
        Some(backdrop("repo-remove-dialog", panel))
    }

    /// Whether the Remove dialog is open.
    #[must_use]
    pub fn repo_remove_open(&self) -> bool {
        self.repo_remove.dialog().is_some()
    }

    /// The container key whose Remove row is armed, while one is.
    #[must_use]
    pub fn repo_remove_armed(&self) -> Option<String> {
        self.repo_remove.armed().map(str::to_owned)
    }

    /// The armed Remove row's label, while the container menu that draws it
    /// is open.
    #[must_use]
    pub fn repo_remove_armed_label(&self) -> Option<String> {
        let level = self.container_menu.as_ref()?.level()?;
        let armed = self.repo_remove.armed()?;
        (armed == container_key(&level)).then(|| row_label(&level, true))
    }

    /// The open dialog's heading.
    #[must_use]
    pub fn repo_remove_title(&self) -> Option<String> {
        self.repo_remove
            .dialog()
            .map(|dialog| dialog.title().to_owned())
    }

    /// The open dialog's subtitle: the container's name.
    #[must_use]
    pub fn repo_remove_subject(&self) -> Option<String> {
        self.repo_remove
            .dialog()
            .map(|dialog| dialog.name().to_owned())
    }

    /// The open dialog's session rows as drawn.
    #[must_use]
    pub fn repo_remove_session_rows(&self) -> Vec<String> {
        self.repo_remove.dialog().map_or_else(Vec::new, |dialog| {
            dialog
                .sessions()
                .iter()
                .map(RepoRemoveSession::row_text)
                .collect()
        })
    }

    /// The open dialog's footer buttons as shown: selector and label.
    #[must_use]
    pub fn repo_remove_buttons(&self) -> Vec<(String, String)> {
        self.repo_remove
            .dialog()
            .map_or_else(Vec::new, |_| RepoRemoveDialog::buttons())
    }

    /// The selector of the open dialog's focused button.
    #[must_use]
    pub fn repo_remove_focus(&self) -> Option<String> {
        self.repo_remove
            .dialog()
            .map(|dialog| dialog.focused().selector().to_owned())
    }
}
