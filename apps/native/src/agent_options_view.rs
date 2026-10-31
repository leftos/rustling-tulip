//! Draws an agent's spawn options: the fields of one placement, each as the
//! control its kind takes. A press goes to the form as its [`Control`], and
//! a text field's edits reach it through the dialog's per-field inputs.

use gpui::{AnyElement, Context, Div, IntoElement, ParentElement, Styled, div, prelude::*, px};
use protocol::{ChoiceStyle, OptionValue, SpawnPlacement};

use crate::RootView;
use crate::agent_options::{FieldEditor, FieldView};
use crate::spawn_form::{Control, SpawnForm, field_selector};
use crate::spawn_view::{
    DISABLED_OPACITY, Look, OptionInput, check_label, checkbox_row, field, input_box, muted, note,
    option_button, press_control, reveal_when_focused, segmented,
};
use crate::ui_theme::theme;

/// The note under a field a trusted launch locks, in the spawn dialog and on
/// an agent's Settings page.
pub(crate) const LOCKED: &str = "Ignored while trusted launch is on. The chosen value is kept for when you turn trusted launch off.";

/// The row of a field whose kind this client does not know, in the spawn
/// dialog and on an agent's Settings page.
pub(crate) const UPDATE_CLIENT: &str = "Update the client to change this";

/// The rows of the form's agent fields placed at `placement`, in the
/// descriptor's order; `inputs` are the dialog's text fields, one per text
/// or model field.
pub(crate) fn rows(
    form: &SpawnForm,
    inputs: &[OptionInput],
    placement: SpawnPlacement,
    focus: &Control,
    cx: &mut Context<RootView>,
) -> Vec<AnyElement> {
    form.option_fields()
        .iter()
        .filter(|view| view.placement == placement)
        .map(|view| row(view, inputs, focus, cx))
        .collect()
}

fn row(
    view: &FieldView,
    inputs: &[OptionInput],
    focus: &Control,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let locked = view.disabled && view.editor != FieldEditor::Unknown;
    let body = match &view.editor {
        FieldEditor::Toggle => return toggle_row(view, focus, cx),
        FieldEditor::Choice { choices, style } => {
            let buttons = choices
                .iter()
                .map(|choice| {
                    let control = Control::FieldChoice {
                        field: view.id.clone(),
                        choice: choice.selector.clone(),
                    };
                    let look = Look {
                        selected: choice.value == view.value,
                        focused: focus == &control,
                        enabled: !view.disabled,
                    };
                    option_button(&control, choice.label.clone(), look, cx).into_any_element()
                })
                .collect();
            match style {
                ChoiceStyle::Segmented => div().flex().child(segmented(buttons, &theme(cx))),
                ChoiceStyle::Dropdown | ChoiceStyle::Unknown => segmented(buttons, &theme(cx)),
            }
        }
        FieldEditor::Text { .. } => text_row(view, inputs, &[], focus, cx),
        FieldEditor::Model { suggestions, .. } => text_row(view, inputs, suggestions, focus, cx),
        FieldEditor::Unknown => div()
            .debug_selector({
                let selector = field_selector(&view.id);
                move || selector
            })
            .opacity(DISABLED_OPACITY)
            .child(muted(UPDATE_CLIENT, &theme(cx))),
    };
    field(
        crate::fonts::ui_text_size(cx),
        view.label.clone(),
        &theme(cx),
    )
    .child(body)
    .children(
        view.help
            .clone()
            .map(|text| note(crate::fonts::ui_text_size(cx), text, &theme(cx))),
    )
    .children(locked.then(|| muted(LOCKED, &theme(cx))))
    .into_any_element()
}

/// A toggle field: a checkbox with the field's help under its label.
fn toggle_row(view: &FieldView, focus: &Control, cx: &mut Context<RootView>) -> AnyElement {
    let control = Control::FieldToggle(view.id.clone());
    let focused = focus == &control;
    let checked = view.value == OptionValue::Bool(true);
    let label = check_label(
        view.label.clone(),
        view.help
            .clone()
            .map(|text| note(crate::fonts::ui_text_size(cx), text, &theme(cx))),
    );
    let row = checkbox_row(
        crate::fonts::ui_text_size(cx),
        &control.selector(),
        checked,
        focused,
        label,
        &theme(cx),
    );
    let row = if view.disabled {
        row.opacity(DISABLED_OPACITY)
    } else {
        row.on_click(press_control(&control, cx))
    };
    div()
        .flex()
        .child(reveal_when_focused(row, &control, focused, cx))
        .into_any_element()
}

/// A text or model field's input, and a chip per suggestion beside it.
fn text_row(
    view: &FieldView,
    inputs: &[OptionInput],
    suggestions: &[String],
    focus: &Control,
    cx: &mut Context<RootView>,
) -> Div {
    let control = Control::FieldText(view.id.clone());
    let input = inputs
        .iter()
        .find(|input| input.field == view.id)
        .map(|input| input_box(&control, &input.input, focus, cx));
    let chips: Vec<AnyElement> = suggestions
        .iter()
        .map(|suggestion| {
            let control = Control::FieldSuggestion {
                field: view.id.clone(),
                value: suggestion.clone(),
            };
            let look = Look {
                selected: view.value == OptionValue::Text(suggestion.clone()),
                focused: focus == &control,
                enabled: !view.disabled,
            };
            option_button(&control, suggestion.clone(), look, cx).into_any_element()
        })
        .collect();
    div()
        .flex()
        .gap(px(8.0))
        .when(view.disabled, |row| row.opacity(DISABLED_OPACITY))
        .children(input)
        .when(!chips.is_empty(), |row| {
            row.child(div().flex().flex_none().child(segmented(chips, &theme(cx))))
        })
}
