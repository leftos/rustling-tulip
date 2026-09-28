//! The pane-close dialog: its modal, its keys, and what each answer sends.
//! The model is [`crate::pane_close`].

use gpui::{AnyElement, ClickEvent, Context, FontWeight, Keystroke, Window, div, prelude::*, px};
use protocol::ClientMessage;

use crate::notice_view::modal_panel;
use crate::pane_close::{Control, NOTE, PaneClose, TITLE};
use crate::session_menu::{ConfirmOwner, backdrop, dialog_button};
use crate::{MUTED, RootView};

impl RootView {
    /// Whether the pane-close dialog is open.
    #[must_use]
    pub fn pane_close_open(&self) -> bool {
        self.pane_ui.close.is_some()
    }

    /// The open dialog's buttons in the keyboard's order: selector and
    /// label.
    #[must_use]
    pub fn pane_close_controls(&self) -> Vec<(String, String)> {
        let Some(dialog) = &self.pane_ui.close else {
            return Vec::new();
        };
        dialog
            .controls()
            .into_iter()
            .map(|control| (control.selector().to_owned(), dialog.label(control)))
            .collect()
    }

    /// The selector of the open dialog's focused button.
    #[must_use]
    pub fn pane_close_focus(&self) -> Option<String> {
        self.pane_ui
            .close
            .as_ref()
            .map(|dialog| dialog.focused().selector().to_owned())
    }

    /// The header's ×: an empty pane closes at once; a pane showing a
    /// session asks what becomes of the session first.
    pub(crate) fn close_pane_clicked(
        &mut self,
        tab_id: &str,
        pane_id: &str,
        session_id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session = session_id.and_then(|id| self.sidebar.session(id));
        let Some(session) = session else {
            if let Some(id) = session_id {
                tracing::warn!(session = %id, "closing a pane whose session is unknown");
            }
            self.record_pane_close(tab_id, pane_id, None, cx);
            self.send(ClientMessage::ClosePane {
                tab_id: tab_id.to_owned(),
                pane_id: pane_id.to_owned(),
            });
            return;
        };
        let label = self.session_label(&session.id);
        self.pane_ui.close = Some(PaneClose::new(tab_id, pane_id, session, label));
        self.pane_ui.dialog_focus.focus(window);
        cx.notify();
    }

    /// A key while the dialog is open. Tab and Shift+Tab move the focus,
    /// Enter and Space press the focused button, Esc cancels, and every
    /// other key does nothing.
    pub(crate) fn on_pane_close_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = &mut self.pane_ui.close else {
            return;
        };
        match keystroke.key.as_str() {
            "tab" => {
                dialog.move_focus(!keystroke.modifiers.shift);
                cx.notify();
            }
            "enter" | "space" => {
                let control = dialog.focused();
                self.press_pane_close(control, window, cx);
            }
            "escape" => self.press_pane_close(Control::Cancel, window, cx),
            _ => {}
        }
    }

    /// Presses `control`: Cancel and ✕ close the dialog, the pane-only and
    /// discard answers send and close it, and the delete answer hands over
    /// to the delete-worktree confirm, whose discard closes the pane.
    fn press_pane_close(&mut self, control: Control, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.pane_ui.close.take() else {
            return;
        };
        match control {
            Control::Cancel | Control::Dismiss => {}
            Control::PaneOnly => {
                self.record_pane_close(
                    dialog.tab_id(),
                    dialog.pane_id(),
                    Some(dialog.session_label()),
                    cx,
                );
                tracing::info!(pane = %dialog.pane_id(), "closing the pane, keeping its session");
                self.send(dialog.close_pane_message());
            }
            Control::Discard => {
                tracing::info!(
                    pane = %dialog.pane_id(),
                    session = %dialog.session_id(),
                    "closing the pane and discarding its session, keeping the worktree"
                );
                for msg in dialog.discard_keep_messages() {
                    self.send(msg);
                }
            }
            Control::Delete => {
                self.open_delete_dialog(dialog.session_id(), ConfirmOwner::Menu, window, cx);
                return;
            }
        }
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    /// The dialog over a backdrop that takes every click beneath it.
    pub(crate) fn pane_close_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.pane_ui.close.as_ref()?;
        let mut button = |control: Control| {
            dialog_button(
                control.selector(),
                dialog.label(control),
                control.danger(),
                control == dialog.focused(),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.press_pane_close(control, window, cx);
            }))
        };
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(TITLE))
            .child(button(Control::Dismiss));
        let footer: Vec<_> = dialog
            .controls()
            .into_iter()
            .filter(|control| *control != Control::Dismiss)
            .map(&mut button)
            .collect();
        let panel = modal_panel("pane-close-panel")
            .track_focus(&self.pane_ui.dialog_focus)
            .child(header)
            .child(div().child(dialog.body()))
            .child(div().text_color(gpui::rgb(MUTED)).child(NOTE))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap(px(8.0))
                    .children(footer),
            );
        Some(backdrop("pane-close", panel))
    }
}
