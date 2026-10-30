//! The "VS Code workspace detected" prompt on screen: one suggested
//! `.code-workspace` file at a time, its folder rows, and the three answers.
//! The queue, its order, the body text and the messages live in
//! [`crate::vscode_suggestion`].

use gpui::{AnyElement, ClickEvent, Context, FontWeight, Keystroke, Window, div, prelude::*, px};

use protocol::{DaemonMessage, VscodeWorkspaceSuggestion};

use crate::fonts;
use crate::notice_view::modal_panel;
use crate::scan_key;
use crate::session_menu::{backdrop, dialog_button, dialog_look};
use crate::target::HostKey;
use crate::ui_theme::theme;
use crate::vscode_suggestion::{
    BodyStyle, DISMISS_LABEL, SuggestionButton, TITLE, VscodeSuggestions, WATCH_TIP,
};
use crate::{RootView, tooltip};

impl RootView {
    /// The two VS Code messages: a suggestion queues and shows, a parsed
    /// file fills the creator's pending parse. Returns whether it was one.
    pub(crate) fn on_vscode_message(
        &mut self,
        host: &HostKey,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match msg {
            DaemonMessage::VscodeWorkspaceSuggestion {
                repo_id,
                suggestion,
            } => {
                self.on_vscode_suggestion(repo_id.clone(), suggestion.clone(), window, cx);
                true
            }
            DaemonMessage::VscodeWorkspaceParsed { suggestion } => {
                self.on_vscode_parsed(suggestion.clone(), cx);
                true
            }
            DaemonMessage::VscodeWorkspacesScanned { path, suggestions } => {
                self.on_vscode_workspaces_scanned(host, path, suggestions, cx);
                true
            }
            _ => false,
        }
    }

    /// `host`'s answer to a DIR or SH folder scan: keep its first
    /// suggestion, or none, under the host and the folder's normalized path.
    pub(crate) fn on_vscode_workspaces_scanned(
        &mut self,
        host: &HostKey,
        path: &str,
        suggestions: &[VscodeWorkspaceSuggestion],
        cx: &mut Context<Self>,
    ) {
        self.cwd_workspace_scans
            .insert((host.clone(), scan_key(path)), suggestions.first().cloned());
        cx.notify();
    }

    /// The daemon pushed a suggestion: queue it, and show it when nothing
    /// else is.
    pub(crate) fn on_vscode_suggestion(
        &mut self,
        repo_id: String,
        suggestion: VscodeWorkspaceSuggestion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_open = !self.vscode_suggestions.is_empty();
        self.vscode_suggestions.push(repo_id, suggestion);
        if !was_open {
            self.vscode_prompt_focus.focus(window);
        }
        cx.notify();
    }

    /// A key while the prompt is open: Esc dismisses, Tab and Shift+Tab move
    /// the focus, Enter and Space press the focused button.
    pub(crate) fn on_vscode_prompt_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.vscode_suggestions.is_empty() {
            return false;
        }
        match keystroke.key.as_str() {
            "escape" => {
                self.vscode_suggestions.dismiss();
                self.after_vscode_prompt(window, cx);
                true
            }
            "tab" => {
                self.vscode_suggestions
                    .move_focus(!keystroke.modifiers.shift);
                cx.notify();
                true
            }
            "enter" | "space" => {
                let choice = self.vscode_suggestions.focus();
                self.press_vscode_choice(choice, window, cx);
                true
            }
            _ => true,
        }
    }

    /// Answers the shown suggestion: an acceptance sends its message, and
    /// every answer dismisses the prompt, showing the next queued one.
    fn press_vscode_choice(
        &mut self,
        choice: SuggestionButton,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let message = self.vscode_suggestions.message(choice);
        self.vscode_suggestions.dismiss();
        if let Some(message) = message {
            self.send_to(&HostKey::Local, message);
        }
        self.after_vscode_prompt(window, cx);
    }

    /// Hands the keyboard to the next prompt; else back to the workspace
    /// creator when it is open (a suggestion can arrive over it); else to the
    /// active pane.
    fn after_vscode_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.vscode_suggestions.is_empty() {
            self.vscode_prompt_focus.focus(window);
        } else if self.workspace_creator.is_some() {
            self.apply_workspace_creator_focus(window, cx);
        } else {
            self.focus_active_pane(window, cx);
        }
        cx.notify();
    }

    /// The prompt over a backdrop that takes every click beneath it; a click
    /// on the backdrop itself does nothing.
    pub(crate) fn vscode_prompt_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = theme(cx);
        if self.vscode_suggestions.is_empty() {
            return None;
        }
        let focus = self.vscode_suggestions.focus();
        let dismiss = dialog_button(
            "vscode-prompt-dismiss",
            "✕".to_owned(),
            dialog_look(crate::fonts::ui_text_size(cx), false, true, &theme(cx)),
            false,
        )
        .tooltip(tooltip(DISMISS_LABEL))
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.vscode_suggestions.dismiss();
            this.after_vscode_prompt(window, cx);
        }));
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(TITLE))
            .child(dismiss);
        // The drawn sentence is the model's own parts, so a spec that reads
        // them reads what is on screen: the path in code styling, the name
        // bold, the rest plain.
        let body = div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .debug_selector(|| "vscode-prompt-body".to_owned())
                    .flex()
                    .flex_wrap()
                    .items_baseline()
                    .gap(px(3.0))
                    .children(self.vscode_suggestions.body_parts().into_iter().map(
                        |(text, style)| {
                            let part = div().child(text);
                            match style {
                                BodyStyle::Text => part,
                                BodyStyle::Code => part.font_family(fonts::DEFAULT_FAMILY),
                                BodyStyle::Bold => part.font_weight(FontWeight::BOLD),
                            }
                        },
                    )),
            )
            .children(
                self.vscode_suggestions
                    .folder_rows()
                    .into_iter()
                    .enumerate()
                    .map(|(index, row)| {
                        let badge = row.badge();
                        div()
                            .debug_selector(move || format!("vscode-prompt-folder-{index}"))
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(row.label)
                            .child(div().text_color(gpui::rgb(t.muted)).child(badge))
                    }),
            );
        let buttons: Vec<AnyElement> = SuggestionButton::ALL
            .into_iter()
            .map(|button| {
                let tip = (button == SuggestionButton::CreateWatch).then_some(WATCH_TIP);
                let element = dialog_button(
                    button.selector(),
                    button.label().to_owned(),
                    dialog_look(crate::fonts::ui_text_size(cx), false, true, &theme(cx)),
                    focus == button,
                )
                .when_some(tip, |element, tip| element.tooltip(tooltip(tip)))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.press_vscode_choice(button, window, cx);
                }));
                element.into_any_element()
            })
            .collect();
        let panel = modal_panel(
            crate::fonts::ui_text_size(cx),
            "vscode-prompt-panel",
            &theme(cx),
        )
        .track_focus(&self.vscode_prompt_focus)
        .child(header)
        .child(body)
        .child(div().flex().justify_end().gap(px(6.0)).children(buttons));
        Some(backdrop("vscode-prompt", panel, &theme(cx)))
    }

    /// Whether a suggestion is on screen.
    #[must_use]
    pub fn vscode_prompt_open(&self) -> bool {
        !self.vscode_suggestions.is_empty()
    }

    /// How many suggestions are queued.
    #[must_use]
    pub fn vscode_prompt_queued(&self) -> usize {
        self.vscode_suggestions.len()
    }

    /// The prompt's title.
    #[must_use]
    pub fn vscode_prompt_title(&self) -> Option<String> {
        self.vscode_prompt_open().then(|| TITLE.to_owned())
    }

    /// The prompt's body as the model words it.
    #[must_use]
    pub fn vscode_prompt_body(&self) -> Option<String> {
        self.vscode_prompt_open()
            .then(|| self.vscode_suggestions.body())
    }

    /// The prompt's body parts as drawn: the text and its style name.
    #[must_use]
    pub fn vscode_prompt_body_parts(&self) -> Vec<(String, &'static str)> {
        self.vscode_suggestions
            .body_parts()
            .into_iter()
            .map(|(text, style)| (text, style.label()))
            .collect()
    }

    /// The shown file's folder rows as drawn: label and badge.
    #[must_use]
    pub fn vscode_prompt_folders(&self) -> Vec<(String, String)> {
        self.vscode_suggestions
            .folder_rows()
            .into_iter()
            .map(|row| {
                let badge = row.badge().to_owned();
                (row.label, badge)
            })
            .collect()
    }

    /// The prompt's footer buttons as shown: selector and label.
    #[must_use]
    pub fn vscode_prompt_buttons(&self) -> Vec<(String, String)> {
        VscodeSuggestions::buttons()
    }

    /// The selector of the focused button.
    #[must_use]
    pub fn vscode_prompt_focus(&self) -> Option<String> {
        self.vscode_prompt_open()
            .then(|| self.vscode_suggestions.focus().selector().to_owned())
    }

    /// Every selector the open prompt draws, for the spec harness.
    #[must_use]
    pub fn vscode_prompt_controls(&self) -> Vec<(String, String)> {
        if self.vscode_suggestions.is_empty() {
            return Vec::new();
        }
        let mut controls = vec![("vscode-prompt-dismiss".to_owned(), DISMISS_LABEL.to_owned())];
        controls.extend(VscodeSuggestions::buttons());
        controls.extend(
            self.vscode_suggestions
                .folder_rows()
                .into_iter()
                .enumerate()
                .map(|(index, row)| {
                    (
                        format!("vscode-prompt-folder-{index}"),
                        format!("{} {}", row.label, row.badge()),
                    )
                }),
        );
        controls
    }
}
