//! The preset wizard on screen: the prompt source stage, the variables
//! stage, and Cancel, Back and Next. Its state and every rule live in
//! [`crate::preset_wizard`]; this view renders it and forwards clicks,
//! edits, picks and Esc.

use std::path::PathBuf;

use gpui::{
    AnyElement, ClickEvent, Context, Div, Entity, FontWeight, Keystroke, Stateful, Subscription,
    Window, div, prelude::*, px,
};
use protocol::{PresetEntry, PresetPromptSource, PresetTarget, PresetVariable, PresetVariableKind};

use crate::notice_view::modal_panel;
use crate::preset_wizard::{
    CLEAR_LABEL, FILE_LABEL, FILE_PLACEHOLDER, FOLDER_LABEL, FOLDER_PLACEHOLDER, INLINE_LABEL,
    INLINE_PLACEHOLDER, ISSUES_HINT, ISSUES_LABEL, ISSUES_NOTE, ISSUES_PLACEHOLDER, IssuesFeedback,
    NextButton, PICK_LABEL, Pick, PresetWizard, REQUIRED_HINT, REQUIRED_NOTE, SOURCE_LEGEND,
    SourceKind, Stage, shows_required, source_label, takes_text, variable_placeholder,
};
use crate::session_menu::{backdrop, dialog_button};
use crate::spawn_view::checkbox_row;
use crate::text_input::{TextChanged, TextInput, TextInputEvent};
use crate::{BORDER, DANGER, MUTED, RootView, TEXT, tooltip};

/// The open wizard: its model and the entities of its text fields.
pub(crate) struct PresetWizardDialog {
    wizard: PresetWizard,
    inline_input: Entity<TextInput>,
    issues_input: Entity<TextInput>,
    /// The text field of each prompted variable typed as text, by name.
    text_inputs: Vec<(String, Entity<TextInput>)>,
    _subscriptions: Vec<Subscription>,
}

/// A text field whose edits the model takes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Field {
    Inline,
    Issues,
    Variable(String),
}

/// The selector of a variable's field; its Pick… and Clear add `-pick` and
/// `-clear`.
fn variable_selector(name: &str) -> String {
    format!("preset-variable-{name}")
}

impl RootView {
    /// Opens the wizard for `entry` launched on `target`, unless the
    /// connection is down.
    pub(crate) fn open_preset_wizard(
        &mut self,
        entry: PresetEntry,
        target: PresetTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.conn.overlay().is_some() {
            return;
        }
        let repo_path = self
            .sidebar
            .repos()
            .iter()
            .find(|repo| repo.id == entry.source_repo_id)
            .map(|repo| repo.path.clone());
        let wizard = PresetWizard::new(entry, target, repo_path.as_deref());
        let inline_input = cx
            .new(|cx| TextInput::multi_line(String::new(), INLINE_PLACEHOLDER, cx).with_rows(4, 8));
        let issues_input = cx.new(|cx| TextInput::new(String::new(), ISSUES_PLACEHOLDER, cx));
        let mut subscriptions = Self::watch_preset_field(&inline_input, Field::Inline, window, cx);
        subscriptions.extend(Self::watch_preset_field(
            &issues_input,
            Field::Issues,
            window,
            cx,
        ));
        let mut text_inputs = Vec::new();
        for variable in wizard.prompted_variables() {
            if !takes_text(&variable.kind) {
                continue;
            }
            let value = wizard.value(&variable.name).to_owned();
            let placeholder = variable_placeholder(variable);
            let input = cx.new(|cx| TextInput::new(value, placeholder, cx));
            let field = Field::Variable(variable.name.clone());
            subscriptions.extend(Self::watch_preset_field(&input, field, window, cx));
            text_inputs.push((variable.name.clone(), input));
        }
        self.preset_wizard = Some(PresetWizardDialog {
            wizard,
            inline_input,
            issues_input,
            text_inputs,
            _subscriptions: subscriptions,
        });
        self.preset_wizard_focus.focus(window);
        cx.notify();
    }

    /// Closes the wizard and hands the keyboard back to the active pane.
    pub(crate) fn close_preset_wizard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preset_wizard.take().is_some() {
            self.focus_active_pane(window, cx);
            cx.notify();
        }
    }

    /// An edit goes to the model; Esc in the field closes the wizard.
    fn watch_preset_field(
        input: &Entity<TextInput>,
        field: Field,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        let edits = cx.subscribe_in(input, window, move |this, input, _: &TextChanged, _, cx| {
            let text = input.read(cx).text().to_owned();
            if let Some(dialog) = &mut this.preset_wizard {
                match &field {
                    Field::Inline => dialog.wizard.set_inline_text(text),
                    Field::Issues => dialog.wizard.set_issues_spec(text),
                    Field::Variable(name) => dialog.wizard.set_value(name, text),
                }
            }
            cx.notify();
        });
        let keys = cx.subscribe_in(
            input,
            window,
            |this, _, event: &TextInputEvent, window, cx| {
                if *event == TextInputEvent::Cancel {
                    this.close_preset_wizard(window, cx);
                }
            },
        );
        vec![edits, keys]
    }

    /// A key while the wizard is open: Esc closes it and is the wizard's;
    /// every other key belongs to the field that has the keyboard.
    pub(crate) fn on_preset_wizard_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.preset_wizard.is_none() || keystroke.key != "escape" {
            return false;
        }
        self.close_preset_wizard(window, cx);
        true
    }

    fn with_preset_wizard(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut PresetWizard),
    ) {
        if let Some(dialog) = &mut self.preset_wizard {
            change(&mut dialog.wizard);
            cx.notify();
        }
    }

    /// Asks the file or folder picker for `pick`; the answer fills it when
    /// the wizard is still open. A path goes as picked: the daemon reads it
    /// on its own host.
    fn pick_for_preset(&mut self, pick: Pick, cx: &mut Context<Self>) {
        let Some(dialog) = &self.preset_wizard else {
            return;
        };
        let start = dialog.wizard.pick_start(&pick);
        let picked = match dialog.wizard.pick_filter(&pick) {
            Some(filter) => (self.pick_file)(cx, start, filter),
            None => (self.pick_folder)(cx, start),
        };
        cx.spawn(async move |this, cx| {
            let picked = picked.await;
            // Fails only when the view is gone, and its window with it.
            this.update(cx, |this, cx| this.finish_preset_pick(pick, picked, cx))
                .ok();
        })
        .detach();
    }

    fn finish_preset_pick(&mut self, pick: Pick, picked: Option<PathBuf>, cx: &mut Context<Self>) {
        let Some(picked) = picked else {
            return;
        };
        let path = picked.to_string_lossy().into_owned();
        self.with_preset_wizard(cx, |wizard| wizard.apply_pick(pick, path));
    }

    /// The wizard over a backdrop that takes every click beneath it.
    pub(crate) fn preset_wizard_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.preset_wizard.as_ref()?;
        let wizard = &dialog.wizard;
        let description = wizard.entry().description.clone().map(|text| {
            div()
                .debug_selector(|| "preset-wizard-description".to_owned())
                .text_color(gpui::rgb(MUTED))
                .child(text)
        });
        let body = match wizard.stage() {
            Stage::Source => source_body(dialog, cx),
            Stage::Variables => variables_body(dialog, cx),
        };
        let panel = modal_panel("preset-wizard-panel")
            .track_focus(&self.preset_wizard_focus)
            .child(
                div()
                    .debug_selector(|| "preset-wizard-title".to_owned())
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(wizard.title()),
            )
            .children(description)
            .child(body)
            .child(wizard_footer(wizard, cx));
        Some(backdrop("preset-wizard", panel))
    }

    /// Whether the preset wizard is open.
    #[must_use]
    pub fn preset_wizard_open(&self) -> bool {
        self.preset_wizard.is_some()
    }

    /// The wizard's heading.
    #[must_use]
    pub fn preset_wizard_title(&self) -> Option<String> {
        self.preset_wizard.as_ref().map(|d| d.wizard.title())
    }

    /// The stage on screen: `source` or `variables`.
    #[must_use]
    pub fn preset_wizard_stage(&self) -> Option<&'static str> {
        self.preset_wizard.as_ref().map(|d| d.wizard.stage().name())
    }

    /// The source radio rows as drawn: selector, label and whether picked.
    #[must_use]
    pub fn preset_wizard_sources(&self) -> Vec<(String, String, bool)> {
        let Some(dialog) = &self.preset_wizard else {
            return Vec::new();
        };
        let picked = dialog.wizard.source_kind();
        dialog
            .wizard
            .entry()
            .prompt_sources
            .iter()
            .map(|source| {
                let kind = SourceKind::of(source);
                (
                    kind.selector().to_owned(),
                    source_label(source),
                    kind == picked,
                )
            })
            .collect()
    }

    /// The picked prompts file, while one is.
    #[must_use]
    pub fn preset_wizard_file_path(&self) -> Option<String> {
        self.preset_wizard
            .as_ref()
            .and_then(|d| d.wizard.file_path().map(str::to_owned))
    }

    /// The prompts folder, while one is set.
    #[must_use]
    pub fn preset_wizard_folder_path(&self) -> Option<String> {
        self.preset_wizard
            .as_ref()
            .and_then(|d| d.wizard.folder_path().map(str::to_owned))
    }

    /// Next as drawn: whether it can be pressed, and its tooltip.
    #[must_use]
    pub fn preset_wizard_next(&self) -> Option<NextButton> {
        self.preset_wizard.as_ref().map(|d| d.wizard.next_button())
    }

    /// The value variable `name` holds.
    #[must_use]
    pub fn preset_wizard_value(&self, name: &str) -> Option<String> {
        self.preset_wizard
            .as_ref()
            .map(|d| d.wizard.value(name).to_owned())
    }

    /// The line under the issue spec.
    #[must_use]
    pub fn preset_wizard_issues_line(&self) -> Option<String> {
        self.preset_wizard
            .as_ref()
            .map(|d| match d.wizard.issues_feedback() {
                IssuesFeedback::Hint => ISSUES_HINT.to_owned(),
                IssuesFeedback::Ok(line) | IssuesFeedback::Error(line) => line,
            })
    }
}

/// A muted label line.
fn muted(text: impl Into<gpui::SharedString>) -> Div {
    div().text_color(gpui::rgb(MUTED)).child(text.into())
}

/// A field's label with an optional muted note after it.
fn field_label(label: String, note: Option<String>) -> Div {
    div()
        .flex()
        .child(label)
        .children(note.map(|note| div().text_color(gpui::rgb(MUTED)).child(note)))
}

/// A text input in its box, tagged `selector`.
fn input_box(selector: String, input: &Entity<TextInput>) -> Div {
    div()
        .debug_selector(move || selector)
        .flex()
        .flex_1()
        .px(px(6.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(BORDER))
        .child(input.clone())
}

/// A radio row: a filled dot while picked.
fn radio_row(selector: &'static str, picked: bool, label: String) -> Stateful<Div> {
    div()
        .id(selector)
        .debug_selector(move || selector.to_owned())
        .flex()
        .items_center()
        .gap(px(6.0))
        .cursor_pointer()
        .child(if picked { "◉" } else { "○" })
        .child(label)
}

/// What a row's button does to the root view.
type ViewAction = Box<dyn Fn(&mut RootView, &mut Context<RootView>)>;

/// A read-only path with its Pick… button, and Clear when `clear` is
/// given and the path is set.
fn path_row(
    selector: &str,
    path: Option<&str>,
    placeholder: &str,
    on_pick: impl Fn(&mut RootView, &mut Context<RootView>) + 'static,
    on_clear: Option<ViewAction>,
    cx: &mut Context<RootView>,
) -> Div {
    let shown = div()
        .debug_selector({
            let selector = selector.to_owned();
            move || selector
        })
        .flex_1()
        .px(px(6.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(BORDER))
        .text_color(gpui::rgb(if path.is_some() { TEXT } else { MUTED }))
        .child(path.unwrap_or(placeholder).to_owned());
    let pick = dialog_button(
        &format!("{selector}-pick"),
        PICK_LABEL.to_owned(),
        false,
        false,
        true,
    )
    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| on_pick(this, cx)));
    let clear = on_clear.filter(|_| path.is_some()).map(|on_clear| {
        dialog_button(
            &format!("{selector}-clear"),
            CLEAR_LABEL.to_owned(),
            false,
            false,
            true,
        )
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| on_clear(this, cx)))
    });
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(shown)
        .child(pick)
        .children(clear)
}

/// The Source stage: the allowed sources, then the picked one's field.
fn source_body(dialog: &PresetWizardDialog, cx: &mut Context<RootView>) -> Div {
    let wizard = &dialog.wizard;
    let picked = wizard.source_kind();
    let radios: Vec<Stateful<Div>> = wizard
        .entry()
        .prompt_sources
        .iter()
        .map(|source| {
            let kind = SourceKind::of(source);
            radio_row(kind.selector(), kind == picked, source_label(source)).on_click(cx.listener(
                move |this, _: &ClickEvent, _, cx| {
                    this.with_preset_wizard(cx, |wizard| {
                        wizard.set_source_kind(kind);
                    });
                },
            ))
        })
        .collect();
    let field = match picked {
        SourceKind::File => file_field(wizard, cx),
        SourceKind::Folder => folder_field(wizard, cx),
        SourceKind::Inline => div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(INLINE_LABEL)
            .child(input_box(
                "preset-source-inline-text".to_owned(),
                &dialog.inline_input,
            )),
        SourceKind::GithubIssues => issues_field(dialog),
    };
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(muted(SOURCE_LEGEND))
        .children(radios)
        .child(field)
}

fn file_field(wizard: &PresetWizard, cx: &mut Context<RootView>) -> Div {
    let row = path_row(
        "preset-source-file",
        wizard.file_path(),
        FILE_PLACEHOLDER,
        |this, cx| this.pick_for_preset(Pick::SourceFile, cx),
        None,
        cx,
    );
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(FILE_LABEL)
        .child(row)
}

fn folder_field(wizard: &PresetWizard, cx: &mut Context<RootView>) -> Div {
    let note = wizard
        .entry()
        .prompt_sources
        .iter()
        .find_map(|source| match source {
            PresetPromptSource::Folder { relative_path } => {
                Some(format!(" · default: {relative_path}"))
            }
            _ => None,
        });
    let row = path_row(
        "preset-source-folder",
        wizard.folder_path(),
        FOLDER_PLACEHOLDER,
        |this, cx| this.pick_for_preset(Pick::SourceFolder, cx),
        None,
        cx,
    );
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(field_label(FOLDER_LABEL.to_owned(), note))
        .child(row)
}

fn issues_field(dialog: &PresetWizardDialog) -> Div {
    let line = match dialog.wizard.issues_feedback() {
        IssuesFeedback::Hint => muted(ISSUES_HINT),
        IssuesFeedback::Ok(summary) => {
            muted(summary).debug_selector(|| "preset-source-github-issues-ok".to_owned())
        }
        IssuesFeedback::Error(err) => div()
            .debug_selector(|| "preset-source-github-issues-error".to_owned())
            .text_color(gpui::rgb(DANGER))
            .child(err),
    };
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(field_label(
            ISSUES_LABEL.to_owned(),
            Some(ISSUES_NOTE.to_owned()),
        ))
        .child(input_box(
            "preset-source-github-issues-spec".to_owned(),
            &dialog.issues_input,
        ))
        .child(line)
}

/// The Variables stage: one field per prompted variable.
fn variables_body(dialog: &PresetWizardDialog, cx: &mut Context<RootView>) -> Div {
    let wizard = &dialog.wizard;
    let fields: Vec<Div> = wizard
        .prompted_variables()
        .into_iter()
        .map(|variable| variable_field(dialog, variable, cx))
        .collect();
    div().flex().flex_col().gap(px(8.0)).children(fields)
}

fn variable_field(
    dialog: &PresetWizardDialog,
    variable: &PresetVariable,
    cx: &mut Context<RootView>,
) -> Div {
    let wizard = &dialog.wizard;
    let selector = variable_selector(&variable.name);
    let name = variable.name.clone();
    let value = wizard.value(&variable.name);
    let path = (!value.is_empty()).then_some(value);
    let input: AnyElement = if let Some(pick) = Pick::for_variable(variable) {
        path_row(
            &selector,
            path,
            &variable_placeholder(variable),
            move |this, cx| this.pick_for_preset(pick.clone(), cx),
            Some(Box::new(move |this, cx| {
                this.with_preset_wizard(cx, |wizard| wizard.set_value(&name, String::new()));
            })),
            cx,
        )
        .into_any_element()
    } else if matches!(variable.kind, PresetVariableKind::Toggle { .. }) {
        checkbox_row(&selector, wizard.toggle_on(&name), false, "")
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.with_preset_wizard(cx, |wizard| wizard.toggle(&name));
            }))
            .into_any_element()
    } else {
        match dialog.text_inputs.iter().find(|(n, _)| *n == variable.name) {
            Some((_, input)) => input_box(selector.clone(), input).into_any_element(),
            None => div().into_any_element(),
        }
    };
    let note = shows_required(variable).then(|| REQUIRED_NOTE.to_owned());
    let missing = wizard.is_missing(variable).then(|| {
        let hint = format!("{selector}-required");
        div()
            .debug_selector(move || hint)
            .text_color(gpui::rgb(DANGER))
            .child(REQUIRED_HINT)
    });
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(field_label(variable.label.clone(), note))
        .child(input)
        .children(missing)
}

/// Cancel, Back after the first stage, and Next with its tooltip.
fn wizard_footer(wizard: &PresetWizard, cx: &mut Context<RootView>) -> Div {
    let cancel = dialog_button("preset-cancel", "Cancel".to_owned(), false, false, true).on_click(
        cx.listener(|this, _: &ClickEvent, window, cx| this.close_preset_wizard(window, cx)),
    );
    let back = (wizard.stage() != Stage::Source).then(|| {
        dialog_button("preset-back", "Back".to_owned(), false, false, true).on_click(cx.listener(
            |this, _: &ClickEvent, _, cx| {
                this.with_preset_wizard(cx, |wizard| {
                    wizard.back();
                });
            },
        ))
    });
    let state = wizard.next_button();
    let next = dialog_button(
        "preset-next",
        "Next".to_owned(),
        false,
        false,
        state.enabled,
    )
    .when_some(state.tip, |button, tip| button.tooltip(tooltip(tip)))
    .when(state.enabled, |button| {
        button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            this.with_preset_wizard(cx, |wizard| {
                wizard.advance();
            });
        }))
    });
    div()
        .flex()
        .justify_end()
        .gap(px(6.0))
        .child(cancel)
        .children(back)
        .child(next)
}
