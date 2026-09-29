//! The worktree cleanup-failed dialog: its modal, its keys, and the retry
//! or ignore it ends with. The model is [`crate::cleanup_failed`].

use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, Keystroke, Stateful, Window, div, prelude::*,
    px,
};
use protocol::{DaemonMessage, WorktreeCleanupFailure};

use crate::buttons::RING_ROOM;
use crate::cleanup_failed::{CleanupFailed, Control, open_folder_target, truncate_cmdline};
use crate::notice_view::modal_panel;
use crate::open::OpenJob;
use crate::open_view::Then;
use crate::session_menu::{backdrop, dialog_button};
use crate::{MUTED, RootView};

const TITLE: &str = "Worktree didn't clean up";
/// The most of the window's height the folder and process list takes.
const LIST_MAX_SHARE: f32 = 0.6;

impl RootView {
    /// Whether the cleanup-failed dialog is open.
    #[must_use]
    pub fn cleanup_failed_open(&self) -> bool {
        !self.cleanup_failed.is_empty()
    }

    /// The session the open dialog is about.
    #[must_use]
    pub fn cleanup_failed_session(&self) -> Option<&str> {
        self.cleanup_failed.head().map(CleanupFailed::session_id)
    }

    /// What the open dialog says under its title.
    #[must_use]
    pub fn cleanup_failed_body(&self) -> Option<String> {
        self.cleanup_failed.head().map(CleanupFailed::body)
    }

    /// The open dialog's controls in the keyboard's order: selector and
    /// label.
    #[must_use]
    pub fn cleanup_failed_controls(&self) -> Vec<(String, String)> {
        let Some(dialog) = self.cleanup_failed.head() else {
            return Vec::new();
        };
        dialog
            .controls()
            .into_iter()
            .map(|control| (control.selector(), dialog.label(control)))
            .collect()
    }

    /// The selector of the open dialog's focused control.
    #[must_use]
    pub fn cleanup_failed_focus(&self) -> Option<String> {
        self.cleanup_failed
            .head()
            .map(|dialog| dialog.focused().selector())
    }

    /// The pids ticked for killing in the open dialog.
    #[must_use]
    pub fn cleanup_failed_ticked(&self) -> Vec<u32> {
        self.cleanup_failed
            .head()
            .map(CleanupFailed::ticked_pids)
            .unwrap_or_default()
    }

    /// The daemon's `WorktreeCleanupFailed`: the failure is queued, and the
    /// dialog takes the keyboard. Every other message is left alone.
    pub(crate) fn on_cleanup_failed_message(
        &mut self,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let DaemonMessage::WorktreeCleanupFailed {
            session_id,
            session_label,
            failures,
        } = msg
        else {
            return;
        };
        tracing::warn!(
            session = %session_id,
            failures = failures.len(),
            "worktree cleanup failed; asking what to do"
        );
        self.cleanup_failed
            .push(session_id.clone(), session_label.clone(), failures.clone());
        self.cleanup_focus.focus(window);
        cx.notify();
    }

    /// Drops every queued failure, as a new connection does.
    pub(crate) fn reset_cleanup_failed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.cleanup_failed.is_empty() {
            return;
        }
        self.cleanup_failed.clear();
        self.after_cleanup_closed(window, cx);
    }

    /// A key while the dialog is open; returns whether it was. Tab and
    /// Shift+Tab move the focus, Enter and Space press the focused control,
    /// Esc ignores, and every other key does nothing.
    pub(crate) fn on_cleanup_failed_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(dialog) = self.cleanup_failed.head_mut() else {
            return false;
        };
        match keystroke.key.as_str() {
            "tab" => {
                dialog.move_focus(!keystroke.modifiers.shift);
                cx.notify();
            }
            "enter" | "space" => {
                let control = dialog.focused();
                self.press_cleanup_control(control, window, cx);
            }
            "escape" => self.press_cleanup_control(Control::Ignore, window, cx),
            _ => {}
        }
        true
    }

    /// Presses `control`: Ignore and ✕ close the entry, the retry sends it
    /// and closes it, a checkbox flips and Open folder reveals the folder.
    fn press_cleanup_control(
        &mut self,
        control: Control,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = self.cleanup_failed.head_mut() else {
            return;
        };
        match control {
            Control::Close | Control::Ignore => {
                tracing::info!(session = %dialog.session_id(), "worktree cleanup failure ignored");
                self.close_cleanup_head(window, cx);
            }
            Control::Retry => {
                let msg = dialog.retry_message();
                tracing::info!(
                    session = %dialog.session_id(),
                    kill = ?dialog.ticked_pids(),
                    "worktree cleanup: retrying"
                );
                self.send(msg);
                self.close_cleanup_head(window, cx);
            }
            Control::Process(pid) => dialog.toggle(pid),
            Control::OpenFolder(index) => {
                let target = dialog
                    .failures()
                    .get(index)
                    .map(|failure| open_folder_target(&failure.member_path));
                if let Some(target) = target {
                    self.dispatch_open(OpenJob::Reveal(target), Then::Nothing, window, cx);
                }
            }
        }
        cx.notify();
    }

    /// Closes the entry on screen; the next one shows, or the keyboard goes
    /// back to what the dialog covered.
    fn close_cleanup_head(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cleanup_failed.pop();
        if self.cleanup_failed.is_empty() {
            self.after_cleanup_closed(window, cx);
        }
        cx.notify();
    }

    /// Hands the keyboard to the dialog under this one, else to the pane.
    fn after_cleanup_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.exit.is_some() {
            if self.exit_walk_confirm_open() {
                self.dialog_focus.focus(window);
            } else {
                self.quit_focus.focus(window);
            }
        } else if self.layout_chooser.is_some() {
            self.chooser_focus.focus(window);
        } else if self.run_confirm.is_some() {
            self.run_focus.focus(window);
        } else {
            self.after_notice_closed(window, cx);
        }
    }

    /// The dialog over a backdrop that takes every click beneath it.
    pub(crate) fn cleanup_failed_layer(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let dialog = self.cleanup_failed.head()?;
        let mut button =
            |control: Control, danger: bool| cleanup_button(dialog, control, danger, cx);
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(TITLE))
            .child(button(Control::Close, false));
        let failures: Vec<Div> = dialog
            .failures()
            .iter()
            .enumerate()
            .map(|(index, failure)| failure_row(failure, button(Control::OpenFolder(index), false)))
            .collect();
        let processes: Vec<Div> = dialog
            .processes()
            .iter()
            .map(|process| {
                let check = button(Control::Process(process.pid), false);
                let cmdline = process
                    .cmdline
                    .as_deref()
                    .filter(|cmdline| !cmdline.is_empty())
                    .map(|cmdline| muted(truncate_cmdline(cmdline)));
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(check)
                            .child(muted(format!("pid {}", process.pid))),
                    )
                    .children(cmdline)
            })
            .collect();
        let footer = div()
            .flex()
            .justify_end()
            .gap(px(8.0))
            .child(button(Control::Ignore, false))
            .child(button(Control::Retry, true));
        // Many folders or lockers scroll inside the panel, so the buttons
        // stay on screen.
        let list = div()
            .id("cleanup-failed-list")
            .max_h(window.viewport_size().height * LIST_MAX_SHARE)
            .overflow_y_scroll()
            .p(px(RING_ROOM))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(div().flex().flex_col().gap(px(8.0)).children(failures))
            .when(!processes.is_empty(), |list| {
                list.child(div().flex().flex_col().gap(px(6.0)).children(processes))
            });
        let panel = modal_panel("cleanup-failed-panel")
            .track_focus(&self.cleanup_focus)
            .child(header)
            .child(div().child(dialog.body()))
            .child(list)
            .children(dialog.no_lockers_note().map(muted))
            .child(footer);
        Some(backdrop("cleanup-failed", panel))
    }
}

/// A button of the dialog, ringed while focused; a checkbox shows its
/// mark before the process name.
fn cleanup_button(
    dialog: &CleanupFailed,
    control: Control,
    danger: bool,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let label = match control {
        Control::Process(pid) => {
            let mark = if dialog.is_ticked(pid) { "☑" } else { "☐" };
            format!("{mark} {}", dialog.label(control))
        }
        _ => dialog.label(control),
    };
    dialog_button(
        &control.selector(),
        label,
        danger,
        control == dialog.focused(),
        true,
    )
    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.press_cleanup_control(control, window, cx);
    }))
}

/// A folder still on disk: its path and Open folder over the reason.
fn failure_row(failure: &WorktreeCleanupFailure, open: Stateful<Div>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .child(div().flex_1().min_w_0().child(failure.member_path.clone()))
                .child(open),
        )
        .child(muted(failure.reason.clone()))
}

fn muted(text: impl Into<gpui::SharedString>) -> Div {
    div().text_color(gpui::rgb(MUTED)).child(text.into())
}
