//! The Add repo prompt on a remote host on screen: its title, the path
//! field, the hint and refusal under it, and Cancel and Add. Its state and
//! every rule live in [`crate::host_path_prompt`]; Add sends `AddRepo` and
//! closes, and a daemon refusal arrives as the usual error toast.

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, Focusable as _, FontWeight, Keystroke,
    Subscription, Window, div, prelude::*, px,
};

use crate::RootView;
use crate::host_path_prompt::{HostPathPrompt, PromptControl};
use crate::notice_view::modal_panel;
use crate::session_menu::{backdrop, dialog_button, dialog_look};
use crate::target::HostKey;
use crate::text_input::{TextChanged, TextInput, TextInputEvent};
use crate::ui_theme::{UiTheme, theme};

/// The open prompt: its state and its path field.
pub(crate) struct HostPathDialog {
    prompt: HostPathPrompt,
    path_input: Entity<TextInput>,
    _subscriptions: Vec<Subscription>,
}

impl RootView {
    /// Whether the Add repo prompt is open.
    #[must_use]
    pub fn host_path_prompt_open(&self) -> bool {
        self.host_path_prompt.is_some()
    }

    /// The prompt's title, while it is open.
    #[must_use]
    pub fn host_path_prompt_title(&self) -> Option<String> {
        self.host_path_prompt
            .as_ref()
            .map(|dialog| dialog.prompt.title())
    }

    /// The text of the path field, while the prompt is open.
    #[must_use]
    pub fn host_path_prompt_text(&self, cx: &App) -> Option<String> {
        let dialog = self.host_path_prompt.as_ref()?;
        Some(dialog.path_input.read(cx).text().to_owned())
    }

    /// The refusal under the path field.
    #[must_use]
    pub fn host_path_prompt_error(&self) -> Option<String> {
        self.host_path_prompt
            .as_ref()?
            .prompt
            .error()
            .map(str::to_owned)
    }

    /// The selector of the prompt's focused control.
    #[must_use]
    pub fn host_path_prompt_focus(&self) -> Option<String> {
        self.host_path_prompt
            .as_ref()
            .map(|dialog| dialog.prompt.focus().selector().to_owned())
    }

    /// Whether the spawn dialog's own focus holds the keyboard: where it
    /// goes back when the prompt opened over the no-repos dialog closes.
    #[must_use]
    pub fn spawn_dialog_holds_keyboard(&self, window: &Window) -> bool {
        self.spawn_dialog.is_some() && self.spawn_focus.is_focused(window)
    }

    /// Whether pane `pane_id`'s terminal holds the keyboard.
    #[must_use]
    pub fn pane_holds_keyboard(&self, pane_id: &str, window: &Window, cx: &App) -> bool {
        self.panes
            .get(pane_id)
            .is_some_and(|slot| slot.view().read(cx).is_focused(window))
    }

    /// Add repo on a remote host: opens the prompt naming the host, its
    /// field empty, since this PC's remembered folder means nothing there.
    pub(crate) fn open_host_path_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.host_path_prompt.is_some() {
            return;
        }
        let Some(host) = self.conn().target().short_name().map(str::to_owned) else {
            tracing::warn!("the Add repo prompt opens only on a remote host");
            return;
        };
        let prompt = HostPathPrompt::new(host);
        let placeholder = prompt.placeholder();
        let path_input = cx.new(|cx| TextInput::new(String::new(), placeholder, cx));
        let subscriptions = Self::watch_host_path_field(&path_input, window, cx);
        self.renaming = None;
        self.close_flyout();
        self.host_path_prompt = Some(HostPathDialog {
            prompt,
            path_input,
            _subscriptions: subscriptions,
        });
        self.apply_host_path_focus(window, cx);
    }

    /// Closes the prompt and hands the keyboard back to the spawn dialog it
    /// opened over, else to the active pane.
    pub(crate) fn close_host_path_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.host_path_prompt.take().is_some() {
            if self.spawn_dialog.is_some() {
                self.apply_spawn_focus(window, cx);
            } else {
                self.focus_active_pane(window, cx);
            }
            cx.notify();
        }
    }

    /// Enter adds (through the field's own event), Esc closes, an edit
    /// clears the refusal, and a click in the field moves the focus there.
    fn watch_host_path_field(
        input: &Entity<TextInput>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        let keys = cx.subscribe_in(
            input,
            window,
            |this, _, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => this.submit_host_path_prompt(window, cx),
                TextInputEvent::Cancel => this.close_host_path_prompt(window, cx),
            },
        );
        let edits = cx.subscribe_in(input, window, |this, _, _: &TextChanged, _, cx| {
            if let Some(dialog) = &mut this.host_path_prompt {
                dialog.prompt.edited();
            }
            cx.notify();
        });
        let handle = input.read(cx).focus_handle(cx);
        let focus = cx.on_focus(&handle, window, |this, _, cx| {
            if let Some(dialog) = &mut this.host_path_prompt {
                dialog.prompt.set_focus(PromptControl::Path);
            }
            cx.notify();
        });
        vec![keys, edits, focus]
    }

    /// Add: the typed folder is registered and the prompt closes, or the
    /// refusal shows under the field.
    fn submit_host_path_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.host_path_prompt_text(cx) else {
            return;
        };
        let add = self
            .host_path_prompt
            .as_mut()
            .and_then(|dialog| dialog.prompt.submit(&text));
        match add {
            Some(add) => {
                tracing::info!("adding repo {} on the host as {}", add.path, add.name);
                self.send_to(&HostKey::Local, add.message());
                self.close_host_path_prompt(window, cx);
            }
            None => self.apply_host_path_focus(window, cx),
        }
    }

    /// A key while the Add repo prompt or the spawn dialog is open: whether
    /// it was theirs; `None` while neither is open. The prompt goes first,
    /// since the spawn dialog's no-repos Add repo opens it over that dialog.
    pub(crate) fn on_add_repo_dialogs_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<bool> {
        if self.host_path_prompt.is_some() {
            Some(self.on_host_path_prompt_key(keystroke, window, cx))
        } else if self.spawn_dialog.is_some() {
            Some(self.on_spawn_dialog_key(keystroke, window, cx))
        } else {
            None
        }
    }

    /// A key while the prompt is open; returns whether it was the prompt's
    /// (every key but typing in the path field is). Tab and Shift+Tab move
    /// the focus, Esc closes, and Enter or Space press the focused button.
    pub(crate) fn on_host_path_prompt_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(focused) = self
            .host_path_prompt
            .as_ref()
            .map(|dialog| dialog.prompt.focus())
        else {
            return false;
        };
        match keystroke.key.as_str() {
            "tab" => {
                if let Some(dialog) = &mut self.host_path_prompt {
                    dialog.prompt.move_focus(!keystroke.modifiers.shift);
                }
                self.apply_host_path_focus(window, cx);
                true
            }
            "escape" => {
                self.close_host_path_prompt(window, cx);
                true
            }
            _ if self.host_path_field_focused(window, cx) => false,
            "enter" | "space" => {
                self.press_host_path_control(focused, window, cx);
                true
            }
            _ => true,
        }
    }

    /// A click on `control`, or Enter or Space while it has the focus.
    fn press_host_path_control(
        &mut self,
        control: PromptControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(dialog) = &mut self.host_path_prompt {
            dialog.prompt.set_focus(control);
        }
        match control {
            PromptControl::Path => self.apply_host_path_focus(window, cx),
            PromptControl::Cancel => self.close_host_path_prompt(window, cx),
            PromptControl::Add => self.submit_host_path_prompt(window, cx),
        }
    }

    /// The keyboard goes where the prompt's focus is: the path field, else
    /// the prompt itself.
    fn apply_host_path_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.host_path_prompt else {
            return;
        };
        match dialog.prompt.focus() {
            PromptControl::Path => dialog.path_input.read(cx).focus_handle(cx).focus(window),
            PromptControl::Cancel | PromptControl::Add => self.host_path_focus.focus(window),
        }
        cx.notify();
    }

    /// Whether the path field holds the keyboard.
    fn host_path_field_focused(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.host_path_prompt.as_ref().is_some_and(|dialog| {
            dialog
                .path_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        })
    }

    /// The prompt over a backdrop that takes every click beneath it; a click
    /// on the backdrop itself does nothing.
    pub(crate) fn host_path_prompt_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = theme(cx);
        let dialog = self.host_path_prompt.as_ref()?;
        let prompt = &dialog.prompt;
        let focus = prompt.focus();
        let field = div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(path_box(dialog, focus, &t))
            .child(
                div()
                    .debug_selector(|| "host-path-hint".to_owned())
                    .text_color(gpui::rgb(t.muted))
                    .child(prompt.hint()),
            )
            .children(prompt.error().map(|error| {
                div()
                    .debug_selector(|| "host-path-error".to_owned())
                    .px(px(6.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .bg(gpui::rgb(t.danger_bg))
                    .text_color(gpui::rgb(t.danger))
                    .child(error.to_owned())
            }));
        let panel = modal_panel(
            crate::fonts::ui_text_size(cx),
            "host-path-panel",
            &theme(cx),
        )
        .track_focus(&self.host_path_focus)
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child(prompt.title()),
        )
        .child(field)
        .child(footer(focus, cx));
        Some(backdrop("host-path-prompt", panel, &theme(cx)))
    }
}

/// The path field in its box.
fn path_box(dialog: &HostPathDialog, focus: PromptControl, t: &UiTheme) -> Div {
    div()
        .debug_selector(|| PromptControl::Path.selector().to_owned())
        .flex()
        .px(px(6.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(if focus == PromptControl::Path {
            t.text
        } else {
            t.border
        }))
        .child(dialog.path_input.clone())
}

/// Cancel and Add; Add always takes a press, and a path it refuses says
/// why under the field.
fn footer(focus: PromptControl, cx: &mut Context<RootView>) -> Div {
    let button = |control: PromptControl, label: &str, cx: &mut Context<RootView>| {
        dialog_button(
            control.selector(),
            label.to_owned(),
            dialog_look(crate::fonts::ui_text_size(cx), false, true, &theme(cx)),
            focus == control,
        )
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.press_host_path_control(control, window, cx);
        }))
    };
    div()
        .flex()
        .justify_end()
        .gap(px(6.0))
        .child(button(PromptControl::Cancel, "Cancel", cx))
        .child(button(PromptControl::Add, "Add", cx))
}
