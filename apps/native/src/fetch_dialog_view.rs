//! The Fetch file… dialog on screen — the repo or worktree it fetches from,
//! its path field, the fetch's progress and its buttons. Its state and every
//! rule live in [`crate::fetch_dialog`]; the fetch goes through
//! [`RootView::try_start_fetch`].

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, Focusable as _, FontWeight, Keystroke,
    Subscription, Window, div, prelude::*, px, relative,
};

use crate::RootView;
use crate::fetch_dialog::{FetchControl, FetchForm, PLACEHOLDER, progress_fraction, progress_text};
use crate::fetch_link;
use crate::file_fetch::FetchEntry;
use crate::notice_view::modal_panel;
use crate::session_menu::{backdrop, dialog_button, dialog_look};
use crate::text_input::{TextChanged, TextInput, TextInputEvent};
use crate::ui_theme::{UiTheme, theme};

const TITLE: &str = "Fetch file";

/// The open Fetch file… dialog: its form and its path field.
pub(crate) struct FetchDialog {
    form: FetchForm,
    path_input: Entity<TextInput>,
    _subscriptions: Vec<Subscription>,
}

impl RootView {
    /// Whether the Fetch file… dialog is open.
    #[must_use]
    pub fn fetch_dialog_open(&self) -> bool {
        self.fetch_dialog.is_some()
    }

    /// The text of the path field, while the dialog is open.
    #[must_use]
    pub fn fetch_dialog_path(&self, cx: &App) -> Option<String> {
        let dialog = self.fetch_dialog.as_ref()?;
        Some(fetch_path(dialog, cx).to_owned())
    }

    /// The repo id of the root the path is read against.
    #[must_use]
    pub fn fetch_dialog_scope(&self) -> Option<String> {
        let dialog = self.fetch_dialog.as_ref()?;
        dialog.form.scope().map(|root| root.repo_id.clone())
    }

    /// The line naming the root, for a session with no root or one.
    #[must_use]
    pub fn fetch_dialog_caption(&self) -> Option<String> {
        self.fetch_dialog.as_ref()?.form.caption()
    }

    /// The error under the path field.
    #[must_use]
    pub fn fetch_dialog_error(&self) -> Option<String> {
        self.fetch_dialog.as_ref()?.form.error().map(str::to_owned)
    }

    /// The fetch's progress line and the share of the bar filled, while the
    /// dialog's fetch is in flight.
    #[must_use]
    pub fn fetch_dialog_progress(&self) -> Option<(String, f32)> {
        let entry = self.fetch_dialog_entry()?;
        Some((
            progress_text(entry.bytes_received(), entry.size(), entry.host()),
            progress_fraction(entry.bytes_received(), entry.size()),
        ))
    }

    /// The selector of the dialog's focused control.
    #[must_use]
    pub fn fetch_dialog_focus(&self, cx: &App) -> Option<String> {
        let dialog = self.fetch_dialog.as_ref()?;
        Some(dialog.form.focused(fetch_path(dialog, cx)).selector())
    }

    /// Whether Fetch can go, while the dialog is open.
    #[must_use]
    pub fn fetch_dialog_can_submit(&self, cx: &App) -> bool {
        self.fetch_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.form.can_submit(fetch_path(dialog, cx)))
    }

    /// "Fetch file…": opens the dialog over the focused session's repos and
    /// worktrees, read once now, unless this is the local daemon, no pane is
    /// focused, or another dialog, menu or modal notice is up.
    pub(crate) fn open_fetch_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let blocked = self.fetch_dialog.is_some()
            || self.shell_dialog.is_some()
            || self.exit.is_some()
            || self.spawn_dialog.is_some()
            || self.appearance_editor.is_some()
            || self.delete_dialog.is_some()
            || self.menu.is_some()
            || self.container_menu.is_some()
            || self.sidebar_more_open()
            || self.shell_menu.is_some()
            || self.sc_picker_open
            || self.changes.file_menu.is_some()
            || self.changes.discard.is_some()
            || self.stash.drop.is_some()
            || self.notices.has_modal()
            || self.conn.overlay().is_some();
        if blocked || self.capabilities().opens_local_files() {
            return;
        }
        let Some(session_id) = self.focused_session() else {
            return;
        };
        let session = self.sidebar.session(&session_id);
        let cwd = session.and_then(|session| session.current_cwd.clone());
        let roots = session
            .map(|session| {
                fetch_link::roots(&session.members, cwd.as_deref(), self.sidebar.repos())
            })
            .unwrap_or_default();
        let form = FetchForm::new(roots, cwd.as_deref());
        let path_input = cx.new(|cx| TextInput::new(String::new(), PLACEHOLDER, cx));
        let subscriptions = Self::watch_fetch_field(&path_input, window, cx);
        self.renaming = None;
        self.close_flyout();
        self.close_tab_menu(window, cx);
        self.fetch_dialog = Some(FetchDialog {
            form,
            path_input,
            _subscriptions: subscriptions,
        });
        self.apply_fetch_focus(window, cx);
    }

    /// Closes the dialog, cancelling its fetch unless that is finishing,
    /// and hands the keyboard back to the active pane.
    pub(crate) fn close_fetch_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.fetch_dialog.take() else {
            return;
        };
        if let Some(id) = dialog.form.fetch_id() {
            self.cancel_fetch(id);
        }
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    /// Esc, Cancel or ✕: cancels the fetch and closes, except once the
    /// fetch is finishing, when the save closes the dialog.
    fn dismiss_fetch_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.fetch_dialog_finishing() {
            return;
        }
        self.close_fetch_dialog(window, cx);
    }

    /// Whether the dialog's fetch is `id`.
    pub(crate) fn fetch_dialog_follows(&self, id: &str) -> bool {
        self.fetch_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.form.fetch_id() == Some(id))
    }

    /// The dialog's fetch failed for `message`, which it shows under the
    /// field, back to editing.
    pub(crate) fn fail_fetch_dialog(
        &mut self,
        message: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(dialog) = &mut self.fetch_dialog {
            dialog.form.fail(message);
        }
        self.apply_fetch_focus(window, cx);
    }

    /// The dialog's fetch, while it is in flight.
    fn fetch_dialog_entry(&self) -> Option<&FetchEntry> {
        let id = self.fetch_dialog.as_ref()?.form.fetch_id()?;
        self.fetches.get(id)
    }

    /// Whether the dialog's fetch has its last chunk with the writer.
    fn fetch_dialog_finishing(&self) -> bool {
        self.fetch_dialog_entry()
            .is_some_and(FetchEntry::is_finishing)
    }

    /// Enter fetches (through the field's own event), Esc dismisses, an
    /// edit clears the error and redraws the dialog, and a click in the
    /// field moves the form's focus there.
    fn watch_fetch_field(
        input: &Entity<TextInput>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        let keys = cx.subscribe_in(
            input,
            window,
            |this, _, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => this.submit_fetch_dialog(window, cx),
                TextInputEvent::Cancel => this.dismiss_fetch_dialog(window, cx),
            },
        );
        let edits = cx.subscribe_in(input, window, |this, _, _: &TextChanged, _, cx| {
            if let Some(dialog) = &mut this.fetch_dialog {
                dialog.form.edited();
            }
            cx.notify();
        });
        let handle = input.read(cx).focus_handle(cx);
        let focus = cx.on_focus(&handle, window, |this, _, cx| {
            if let Some(dialog) = &mut this.fetch_dialog {
                dialog.form.set_focus(FetchControl::Path);
            }
            cx.notify();
        });
        vec![keys, edits, focus]
    }

    /// Fetch: the file goes out as the dialog's fetch, or the reason it
    /// cannot shows under the field.
    fn submit_fetch_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.fetch_dialog_path(cx) else {
            return;
        };
        let request = self
            .fetch_dialog
            .as_mut()
            .and_then(|dialog| dialog.form.submit(&text));
        if let Some(request) = request {
            let started = self.try_start_fetch(request, None, true, cx);
            if let Some(dialog) = &mut self.fetch_dialog {
                match started {
                    Ok(id) => dialog.form.started(id),
                    Err(refusal) => dialog.form.fail(refusal.message().to_owned()),
                }
            }
        }
        self.apply_fetch_focus(window, cx);
    }

    /// A key while the Shell… or the Fetch file… dialog is open: whether it
    /// was the dialog's; `None` while neither is open.
    pub(crate) fn on_path_dialog_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<bool> {
        if self.shell_dialog.is_some() {
            Some(self.on_shell_dialog_key(keystroke, window, cx))
        } else if self.fetch_dialog.is_some() {
            Some(self.on_fetch_dialog_key(keystroke, window, cx))
        } else {
            None
        }
    }

    /// A key while the dialog is open; returns whether it was the dialog's
    /// (every key but typing in the path field is). Tab and Shift+Tab move
    /// the focus, Esc dismisses, Left and Right step through the scope row,
    /// and Enter or Space press the focused button.
    pub(crate) fn on_fetch_dialog_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(text) = self.fetch_dialog_path(cx) else {
            return false;
        };
        let Some((focused, busy)) = self
            .fetch_dialog
            .as_ref()
            .map(|dialog| (dialog.form.focused(&text), dialog.form.is_busy()))
        else {
            return false;
        };
        match keystroke.key.as_str() {
            "tab" => {
                if let Some(dialog) = &mut self.fetch_dialog {
                    dialog.form.move_focus(!keystroke.modifiers.shift, &text);
                }
                self.apply_fetch_focus(window, cx);
                true
            }
            "escape" => {
                self.dismiss_fetch_dialog(window, cx);
                true
            }
            _ if self.fetch_field_focused(window, cx) => false,
            key @ ("left" | "right") => {
                if matches!(focused, FetchControl::Scope(_))
                    && !busy
                    && let Some(dialog) = &mut self.fetch_dialog
                {
                    dialog.form.step_scope(key == "right");
                }
                self.apply_fetch_focus(window, cx);
                true
            }
            "enter" | "space" => {
                self.press_fetch_control(focused, window, cx);
                true
            }
            _ => true,
        }
    }

    /// A click on `control`, or Enter or Space while it has the focus; a
    /// click moves the keyboard to that control.
    fn press_fetch_control(
        &mut self,
        control: FetchControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(dialog) = &mut self.fetch_dialog {
            dialog.form.set_focus(control);
            if let FetchControl::Scope(index) = control
                && !dialog.form.is_busy()
            {
                dialog.form.select_scope(index);
            }
        }
        match control {
            FetchControl::Close | FetchControl::Cancel => self.dismiss_fetch_dialog(window, cx),
            FetchControl::Submit => self.submit_fetch_dialog(window, cx),
            FetchControl::Scope(_) | FetchControl::Path => self.apply_fetch_focus(window, cx),
        }
    }

    /// The keyboard goes where the form's focus is: the path field, else
    /// the dialog itself.
    pub(crate) fn apply_fetch_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.fetch_dialog else {
            return;
        };
        match dialog.form.focused(fetch_path(dialog, cx)) {
            FetchControl::Path => dialog.path_input.read(cx).focus_handle(cx).focus(window),
            _ => self.fetch_focus.focus(window),
        }
        cx.notify();
    }

    /// Whether the path field holds the keyboard.
    fn fetch_field_focused(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.fetch_dialog.as_ref().is_some_and(|dialog| {
            dialog
                .path_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        })
    }

    /// The dialog over a backdrop that takes every click beneath it; a click
    /// on the backdrop itself does nothing.
    pub(crate) fn fetch_dialog_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = theme(cx);
        let dialog = self.fetch_dialog.as_ref()?;
        let form = &dialog.form;
        let text = fetch_path(dialog, cx).to_owned();
        let focus = form.focused(&text);
        let dismissable = !self.fetch_dialog_finishing();
        let close = dialog_button(
            &FetchControl::Close.selector(),
            "✕".to_owned(),
            dialog_look(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                false,
                dismissable,
                &theme(cx),
            ),
            focus == FetchControl::Close,
        )
        .when(dismissable, |button| {
            button.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.press_fetch_control(FetchControl::Close, window, cx);
            }))
        });
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                    .child(TITLE),
            )
            .child(close);
        let field = div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(path_box(dialog, focus, &t))
            .children(form.hint().map(|hint| {
                div()
                    .debug_selector(|| "fetch-hint".to_owned())
                    .text_color(gpui::rgb(t.muted))
                    .child(hint)
            }))
            .children(form.error().map(|error| {
                div()
                    .debug_selector(|| "fetch-error".to_owned())
                    .px(px(6.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .bg(gpui::rgb(t.danger_bg))
                    .text_color(gpui::rgb(t.danger))
                    .child(error.to_owned())
            }));
        let panel = modal_panel(crate::fonts::ui_text_size(cx), "fetch-panel", &theme(cx))
            .track_focus(&self.fetch_focus)
            .child(header)
            .child(scope_row(form, focus, cx))
            .child(field)
            .children(
                self.fetch_dialog_progress()
                    .map(|progress| progress_row(progress, &t)),
            )
            .child(footer(form.can_submit(&text), dismissable, focus, cx));
        Some(backdrop("fetch-dialog", panel, &theme(cx)))
    }
}

/// The path field's text, while the dialog is open.
fn fetch_path<'a>(dialog: &'a FetchDialog, cx: &'a App) -> &'a str {
    dialog.path_input.read(cx).text()
}

/// The root the path is read against: a muted caption for no root or one,
/// else a button per root, the chosen one filled.
fn scope_row(form: &FetchForm, focus: FetchControl, cx: &mut Context<RootView>) -> Div {
    let t = theme(cx);
    let row = div().flex().flex_wrap().items_center().gap(px(6.0));
    if let Some(caption) = form.caption() {
        return row.child(
            div()
                .debug_selector(|| "fetch-caption".to_owned())
                .text_color(gpui::rgb(t.muted))
                .child(caption),
        );
    }
    let chosen = form.scope().map(|root| root.repo_id.clone());
    let enabled = !form.is_busy();
    row.children(form.roots().iter().enumerate().map(|(index, root)| {
        let control = FetchControl::Scope(index);
        dialog_button(
            &control.selector(),
            root.repo_name.clone(),
            dialog_look(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                false,
                enabled,
                &theme(cx),
            ),
            focus == control,
        )
        .when(chosen.as_deref() == Some(root.repo_id.as_str()), |button| {
            button.bg(gpui::rgb(t.selected_bg))
        })
        .when(enabled, |button| {
            button.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.press_fetch_control(control, window, cx);
            }))
        })
    }))
}

/// The path field in its box.
fn path_box(dialog: &FetchDialog, focus: FetchControl, t: &UiTheme) -> Div {
    div()
        .debug_selector(|| FetchControl::Path.selector())
        .flex()
        .px(px(6.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(if focus == FetchControl::Path {
            t.text
        } else {
            t.border
        }))
        .child(dialog.path_input.clone())
}

/// The fetch's bar and the line saying how far it is.
fn progress_row((text, fraction): (String, f32), t: &UiTheme) -> Div {
    let fill = div()
        .debug_selector(|| "fetch-progress-fill".to_owned())
        .h_full()
        .w(relative(fraction))
        .rounded(px(3.0))
        .bg(gpui::rgb(t.accent));
    let track = div()
        .debug_selector(|| "fetch-progress".to_owned())
        .w_full()
        .h(px(6.0))
        .rounded(px(3.0))
        .bg(gpui::rgb(t.border))
        .child(fill);
    div().flex().flex_col().gap(px(4.0)).child(track).child(
        div()
            .debug_selector(|| "fetch-progress-text".to_owned())
            .text_color(gpui::rgb(t.muted))
            .child(text),
    )
}

/// Cancel, while the fetch can still be cancelled, and Fetch while the path
/// can be fetched.
fn footer(
    can_submit: bool,
    dismissable: bool,
    focus: FetchControl,
    cx: &mut Context<RootView>,
) -> Div {
    let cancel = dialog_button(
        &FetchControl::Cancel.selector(),
        "Cancel".to_owned(),
        dialog_look(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            false,
            dismissable,
            &theme(cx),
        ),
        focus == FetchControl::Cancel,
    )
    .when(dismissable, |button| {
        button.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.press_fetch_control(FetchControl::Cancel, window, cx);
        }))
    });
    let submit = dialog_button(
        &FetchControl::Submit.selector(),
        "Fetch".to_owned(),
        dialog_look(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            false,
            can_submit,
            &theme(cx),
        ),
        focus == FetchControl::Submit,
    )
    .when(can_submit, |button| {
        button.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.press_fetch_control(FetchControl::Submit, window, cx);
        }))
    });
    div()
        .flex()
        .justify_end()
        .gap(px(6.0))
        .child(cancel)
        .child(submit)
}
