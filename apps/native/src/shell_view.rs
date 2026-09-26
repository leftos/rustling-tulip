//! The Shell… dialog on screen — its folder field, Browse, the checkbox, the
//! clear link and its buttons — and the quick "+ Shell" spawn. Its state and
//! every rule live in [`crate::shell_dialog`]; spawning goes through
//! [`RootView::spawn`].

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, Focusable as _, FontWeight, Keystroke,
    PathPromptOptions, Stateful, Subscription, Window, div, prelude::*, px,
};

use crate::session_menu::{backdrop, dialog_button};
use crate::shell_dialog::{
    FOLDER_PLACEHOLDER, PATH_HINT, ShellControl, ShellForm, standalone_shell_request,
};
use crate::spawns::OpenIn;
use crate::text_input::{TextChanged, TextInput, TextInputEvent};
use crate::{BORDER, HOVER_BG, MUTED, PANEL_BG, RootView, TEXT, UI_TEXT_SIZE};

const DIALOG_WIDTH: f32 = 440.0;
const TITLE: &str = "Open a shell";
const SAVE_DEFAULT_LABEL: &str = "Use as quick shell default";
const CLEAR_DEFAULT_LABEL: &str = "Use home folder for + Shell";

/// The open Shell… dialog: its form and its folder field.
pub(crate) struct ShellDialog {
    form: ShellForm,
    folder_input: Entity<TextInput>,
    _subscriptions: Vec<Subscription>,
}

impl RootView {
    /// Whether the Shell… dialog is open.
    #[must_use]
    pub fn shell_dialog_open(&self) -> bool {
        self.shell_dialog.is_some()
    }

    /// The text of the folder field, while the dialog is open.
    #[must_use]
    pub fn shell_dialog_folder(&self, cx: &App) -> Option<String> {
        let dialog = self.shell_dialog.as_ref()?;
        Some(shell_folder(dialog, cx).to_owned())
    }

    /// The selector of the dialog's focused control.
    #[must_use]
    pub fn shell_dialog_focus(&self, cx: &App) -> Option<&'static str> {
        let dialog = self.shell_dialog.as_ref()?;
        let folder = shell_folder(dialog, cx);
        let focused = dialog.form.focused(folder);
        Some(focused.selector())
    }

    /// Whether Open shell can go, while the dialog is open.
    #[must_use]
    pub fn shell_dialog_can_submit(&self, cx: &App) -> bool {
        let Some(dialog) = self.shell_dialog.as_ref() else {
            return false;
        };
        ShellForm::can_submit(shell_folder(dialog, cx))
    }

    /// The hint under the folder field, while the folder is something but
    /// not a full path.
    #[must_use]
    pub fn shell_dialog_hint(&self, cx: &App) -> Option<&'static str> {
        let dialog = self.shell_dialog.as_ref()?;
        ShellForm::hint_shown(shell_folder(dialog, cx)).then_some(PATH_HINT)
    }

    /// Whether the dialog would remember its folder for the quick shell.
    #[must_use]
    pub fn shell_dialog_saves_default(&self) -> bool {
        self.shell_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.form.save_default())
    }

    /// Whether the dialog offers to go back to the home folder.
    #[must_use]
    pub fn shell_dialog_clears_default(&self) -> bool {
        self.shell_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.form.remembered())
    }

    /// "+ Shell": a plain shell in the remembered folder, else in the
    /// daemon's own default, in the tab on screen.
    pub(crate) fn quick_shell(&mut self, cx: &mut Context<Self>) {
        let cwd = self.sidebar.quick_shell_dir().map(str::to_owned);
        let open_in = self.open_in_active_tab();
        self.spawn(standalone_shell_request(cwd), open_in, cx);
    }

    /// "Shell…": opens the dialog, unless the connection is down or another
    /// dialog, menu or modal notice is up. No repo is needed.
    pub(crate) fn open_shell_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let blocked = self.shell_dialog.is_some()
            || self.exit.is_some()
            || self.spawn_dialog.is_some()
            || self.delete_dialog.is_some()
            || self.menu.is_some()
            || self.notices.has_modal()
            || self.conn.overlay().is_some();
        if blocked {
            return;
        }
        let remembered = self.sidebar.quick_shell_dir().map(str::to_owned);
        let folder = remembered.clone().unwrap_or_default();
        self.shell_generation += 1;
        let form = ShellForm::new(self.shell_generation, remembered.as_deref());
        let folder_input = cx.new(|cx| TextInput::new(folder, FOLDER_PLACEHOLDER, cx));
        let subscriptions = Self::watch_shell_field(&folder_input, window, cx);
        self.renaming = None;
        self.close_flyout();
        self.close_tab_menu(window, cx);
        self.shell_dialog = Some(ShellDialog {
            form,
            folder_input,
            _subscriptions: subscriptions,
        });
        self.apply_shell_focus(window, cx);
    }

    /// Closes the dialog and hands the keyboard back to the active pane.
    pub(crate) fn close_shell_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.shell_dialog.take().is_some() {
            self.focus_active_pane(window, cx);
            cx.notify();
        }
    }

    /// Enter submits (through the field's own event), Esc closes, an edit
    /// redraws the dialog (the folder is what Submit and the hint are judged
    /// by) and a click in the field moves the form's focus there.
    fn watch_shell_field(
        input: &Entity<TextInput>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        let keys = cx.subscribe_in(
            input,
            window,
            |this, _, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => this.submit_shell_dialog(window, cx),
                TextInputEvent::Cancel => this.close_shell_dialog(window, cx),
            },
        );
        let edits = cx.subscribe_in(input, window, |_, _, _: &TextChanged, _, cx| {
            cx.notify();
        });
        let handle = input.read(cx).focus_handle(cx);
        let focus = cx.on_focus(&handle, window, |this, _, cx| {
            if let Some(dialog) = &mut this.shell_dialog {
                dialog.form.set_focus(ShellControl::Folder);
            }
            cx.notify();
        });
        vec![keys, edits, focus]
    }

    /// Open one: the shell goes out under a request id, and the folder is
    /// remembered only once that spawn lands.
    fn submit_shell_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(folder) = self.shell_dialog_folder(cx) else {
            return;
        };
        let Some(submission) = self
            .shell_dialog
            .as_mut()
            .and_then(|dialog| dialog.form.submit(&folder))
        else {
            return;
        };
        self.close_shell_dialog(window, cx);
        let open_in = self.open_in_active_tab();
        let request_id = self.spawn(
            standalone_shell_request(Some(submission.folder.clone())),
            open_in,
            cx,
        );
        if submission.save_default {
            self.quick_shell_saves.arm(&request_id, &submission.folder);
        }
    }

    /// "Use home folder for + Shell": forgets the remembered folder and
    /// closes, spawning nothing.
    fn clear_quick_shell_default(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar.set_quick_shell_dir(None) {
            self.save_ui();
        }
        self.close_shell_dialog(window, cx);
    }

    /// Where a spawn asked for the current tab opens: the tab on screen when
    /// it can hold panes, else a tab of its own — the spawn dialog's own
    /// rule, read from the tab itself.
    fn open_in_active_tab(&self) -> OpenIn {
        match self.tabs.active_tab().filter(|tab| tab.grid().is_some()) {
            Some(tab) => OpenIn::CurrentTab(tab.id.clone()),
            None => OpenIn::NewTab,
        }
    }

    /// A key while the dialog is open; returns whether it was the dialog's
    /// (every key but typing in the folder field is). Tab and Shift+Tab move
    /// the focus, Esc closes, Space presses the focused button.
    pub(crate) fn on_shell_dialog_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = keystroke.key.as_str();
        let Some(folder) = self.shell_dialog_folder(cx) else {
            return false;
        };
        let Some(focused) = self
            .shell_dialog
            .as_ref()
            .map(|dialog| dialog.form.focused(&folder))
        else {
            return false;
        };
        match key {
            "tab" => {
                if let Some(dialog) = &mut self.shell_dialog {
                    dialog.form.move_focus(!keystroke.modifiers.shift, &folder);
                }
                self.apply_shell_focus(window, cx);
                true
            }
            "escape" => {
                self.close_shell_dialog(window, cx);
                true
            }
            _ if self.shell_field_focused(window, cx) => false,
            "enter" | "space" => {
                self.press_shell_control(focused, window, cx);
                true
            }
            _ => true,
        }
    }

    /// A click on `control`, or Space while it has the focus; a click moves
    /// the keyboard to that control, as the spawn dialog's does.
    fn press_shell_control(
        &mut self,
        control: ShellControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(dialog) = &mut self.shell_dialog {
            dialog.form.set_focus(control);
        }
        match control {
            ShellControl::Close | ShellControl::Cancel => self.close_shell_dialog(window, cx),
            ShellControl::Submit => self.submit_shell_dialog(window, cx),
            ShellControl::ClearDefault => self.clear_quick_shell_default(window, cx),
            ShellControl::Browse => self.browse_for_folder(cx),
            ShellControl::SaveDefault => {
                if let Some(dialog) = &mut self.shell_dialog {
                    dialog.form.toggle_save_default();
                }
                self.apply_shell_focus(window, cx);
            }
            ShellControl::Folder => self.apply_shell_focus(window, cx),
        }
    }

    /// "Browse…": the folder the OS picker returns replaces the field, which
    /// happens only while this dialog is the one that asked and the field
    /// still holds what it held then. Browse is disabled meanwhile.
    fn browse_for_folder(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.shell_dialog else {
            return;
        };
        if dialog.form.browsing() {
            return;
        }
        let generation = dialog.form.generation();
        let folder = shell_folder(dialog, cx).to_owned();
        dialog.form.begin_browse(&folder);
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            let picked = match picked.await {
                Ok(Ok(Some(paths))) => paths
                    .into_iter()
                    .next()
                    .map(|path| path.to_string_lossy().into_owned()),
                Ok(Err(err)) => {
                    tracing::warn!("the folder picker failed: {err:#}");
                    None
                }
                // Cancelled, or the picker's sender went before it answered.
                Ok(Ok(None)) | Err(_) => None,
            };
            this.update(cx, |this, cx| {
                this.finish_browse(generation, picked.as_deref(), cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// The folder picker's answer for dialog `generation`, applied when it is
    /// still the dialog on screen with its folder unchanged.
    fn finish_browse(&mut self, generation: u64, picked: Option<&str>, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.shell_dialog else {
            return;
        };
        let current = shell_folder(dialog, cx).to_owned();
        let applied = dialog.form.finish_browse(generation, &current, picked);
        if let Some(folder) = applied {
            let input = dialog.folder_input.clone();
            input.update(cx, |input, cx| input.set_text(folder, cx));
        }
        cx.notify();
    }

    /// The keyboard goes where the form's focus is: the folder field, else
    /// the dialog itself.
    pub(crate) fn apply_shell_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.shell_dialog else {
            return;
        };
        let focus = dialog.form.focused(shell_folder(dialog, cx));
        match focus {
            ShellControl::Folder => dialog.folder_input.read(cx).focus_handle(cx).focus(window),
            _ => self.shell_focus.focus(window),
        }
        cx.notify();
    }

    /// Whether the folder field holds the keyboard.
    fn shell_field_focused(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.shell_dialog.as_ref().is_some_and(|dialog| {
            dialog
                .folder_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        })
    }

    /// The dialog over a backdrop that takes every click beneath it; a click
    /// on the backdrop itself does nothing.
    pub(crate) fn shell_dialog_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.shell_dialog.as_ref()?;
        let form = &dialog.form;
        let folder = shell_folder(dialog, cx).to_owned();
        let focus = form.focused(&folder);
        let close = dialog_button(
            ShellControl::Close.selector(),
            "✕".to_owned(),
            false,
            focus == ShellControl::Close,
        )
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.close_shell_dialog(window, cx);
        }));
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(TITLE))
            .child(close);
        let browse_enabled = !form.browsing();
        let browse = dialog_button(
            ShellControl::Browse.selector(),
            "Browse…".to_owned(),
            false,
            focus == ShellControl::Browse,
        )
        .when(browse_enabled, |button| {
            button.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.press_shell_control(ShellControl::Browse, window, cx);
            }))
        })
        .when(!browse_enabled, |button| button.opacity(0.5));
        let field = div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(div().text_color(gpui::rgb(MUTED)).child("Folder"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(input_box(dialog, focus))
                    .child(browse),
            )
            .children(hint_row(&folder));
        let clear = form.remembered().then(|| {
            link_button(
                ShellControl::ClearDefault.selector(),
                CLEAR_DEFAULT_LABEL,
                focus == ShellControl::ClearDefault,
            )
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.press_shell_control(ShellControl::ClearDefault, window, cx);
            }))
        });
        let panel = div()
            .id("shell-panel")
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
            .track_focus(&self.shell_focus)
            .child(header)
            .child(field)
            .child(save_default_row(form, focus, cx))
            .children(clear)
            .child(footer(&folder, focus, cx));
        Some(backdrop("shell-dialog", panel))
    }
}

/// The folder field's text, while the dialog is open.
fn shell_folder<'a>(dialog: &'a ShellDialog, cx: &'a App) -> &'a str {
    dialog.folder_input.read(cx).text()
}

/// The folder field in its box.
fn input_box(dialog: &ShellDialog, focus: ShellControl) -> Div {
    div()
        .debug_selector(|| ShellControl::Folder.selector().to_owned())
        .flex()
        .flex_1()
        .px(px(6.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(if focus == ShellControl::Folder {
            TEXT
        } else {
            BORDER
        }))
        .child(dialog.folder_input.clone())
}

/// What the folder field says about a text that is not yet a full path.
fn hint_row(folder: &str) -> Option<Div> {
    ShellForm::hint_shown(folder).then(|| {
        div()
            .debug_selector(|| "shell-path-hint".to_owned())
            .text_color(gpui::rgb(MUTED))
            .child(PATH_HINT)
    })
}

/// The "Use as quick shell default" checkbox, ticked or not.
fn save_default_row(
    form: &ShellForm,
    focus: ShellControl,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let mark = if form.save_default() { "☑" } else { "☐" };
    div()
        .id(ShellControl::SaveDefault.selector())
        .debug_selector(|| ShellControl::SaveDefault.selector().to_owned())
        .flex()
        .items_center()
        .gap(px(6.0))
        .px(px(6.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(if focus == ShellControl::SaveDefault {
            TEXT
        } else {
            BORDER
        }))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .child(mark)
        .child(SAVE_DEFAULT_LABEL)
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.press_shell_control(ShellControl::SaveDefault, window, cx);
        }))
}

/// A link-style button: plain text that lights up under the pointer.
fn link_button(selector: &'static str, label: &'static str, focused: bool) -> Stateful<Div> {
    div()
        .id(selector)
        .debug_selector(move || selector.to_owned())
        .px(px(4.0))
        .py(px(2.0))
        .rounded(px(4.0))
        .text_color(gpui::rgb(if focused { TEXT } else { MUTED }))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)).text_color(gpui::rgb(TEXT)))
        .child(label)
}

/// Cancel, and Open shell while a folder is there to open.
fn footer(folder: &str, focus: ShellControl, cx: &mut Context<RootView>) -> Div {
    let cancel = dialog_button(
        ShellControl::Cancel.selector(),
        "Cancel".to_owned(),
        false,
        focus == ShellControl::Cancel,
    )
    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
        this.press_shell_control(ShellControl::Cancel, window, cx);
    }));
    let can_submit = ShellForm::can_submit(folder);
    let submit = dialog_button(
        ShellControl::Submit.selector(),
        "Open shell".to_owned(),
        false,
        focus == ShellControl::Submit,
    )
    .when(can_submit, |button| {
        button.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.press_shell_control(ShellControl::Submit, window, cx);
        }))
    })
    .when(!can_submit, |button| button.opacity(0.5));
    div()
        .flex()
        .justify_end()
        .gap(px(6.0))
        .child(cancel)
        .child(submit)
}
