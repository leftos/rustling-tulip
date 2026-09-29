//! The Move panes dialog: its modal, its keys and the extract it sends. The
//! model is [`crate::move_panes`].

use gpui::{
    AnyElement, ClickEvent, Context, Div, Entity, Focusable as _, FontWeight, Keystroke,
    MouseButton, MouseDownEvent, Stateful, Subscription, Window, div, prelude::*, px,
};

use protocol::ClientMessage;

use crate::move_panes::{Control, LayoutChoice, MovePanes, NAME_LABEL, NAME_PLACEHOLDER, TITLE};
use crate::notice_view::modal_panel;
use crate::session_menu::{backdrop, dialog_button};
use crate::tabs::collect_panes;
use crate::text_input::{TextInput, TextInputEvent};
use crate::{BORDER, HOVER_BG, MUTED, RootView};

/// The open dialog: its model and its name field.
pub(crate) struct MovePanesDialog {
    pub(crate) model: MovePanes,
    name: Entity<TextInput>,
    _events: Subscription,
}

impl RootView {
    /// Whether the Move panes dialog is open.
    #[must_use]
    pub fn move_panes_open(&self) -> bool {
        self.pane_ui.move_panes.is_some()
    }

    /// The open dialog's controls in the keyboard's order: selector and
    /// label.
    #[must_use]
    pub fn move_panes_controls(&self) -> Vec<(String, String)> {
        let Some(dialog) = &self.pane_ui.move_panes else {
            return Vec::new();
        };
        let model = &dialog.model;
        model
            .controls(self.pane_area_aspect())
            .into_iter()
            .map(|control| {
                (
                    model.selector(control),
                    self.move_panes_label(model, control),
                )
            })
            .collect()
    }

    /// The selector of the open dialog's focused control.
    #[must_use]
    pub fn move_panes_focus(&self) -> Option<String> {
        let dialog = self.pane_ui.move_panes.as_ref()?;
        Some(dialog.model.selector(dialog.model.focused()))
    }

    fn move_panes_label(&self, model: &MovePanes, control: Control) -> String {
        match control {
            Control::Pane(index) => model
                .panes()
                .get(index)
                .map(|(_, label)| label.clone())
                .unwrap_or_default(),
            Control::Layout(layout) => layout.label().to_owned(),
            Control::Shape(cols) => model
                .shapes(self.pane_area_aspect())
                .into_iter()
                .find(|(c, _)| *c == cols)
                .map(|(_, label)| label)
                .unwrap_or_default(),
            Control::Name => NAME_LABEL.to_owned(),
            Control::Cancel => "Cancel".to_owned(),
            Control::Confirm => model.confirm_label(),
            Control::Dismiss => "✕".to_owned(),
        }
    }

    /// Opens the dialog over `tab_id`'s panes that show a session, in grid
    /// order, labelled with their sessions' names.
    pub(crate) fn open_move_panes(
        &mut self,
        tab_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.tab(tab_id) else {
            return;
        };
        let panes: Vec<(String, String)> = tab
            .grid()
            .map(|grid| {
                collect_panes(grid)
                    .into_iter()
                    .filter_map(|pane| {
                        pane.session
                            .map(|session| (pane.id.to_owned(), self.session_label(session)))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let model = MovePanes::new(tab_id, &tab.name, panes);
        let name = cx.new(|cx| TextInput::new("", NAME_PLACEHOLDER, cx));
        let events = cx.subscribe_in(
            &name,
            window,
            |this, _, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => this.confirm_move_panes(window, cx),
                TextInputEvent::Cancel => this.close_move_panes(window, cx),
            },
        );
        self.pane_ui.move_panes = Some(MovePanesDialog {
            model,
            name,
            _events: events,
        });
        self.pane_ui.dialog_focus.focus(window);
        cx.notify();
    }

    /// Closes the dialog and hands the keyboard back to the active pane.
    fn close_move_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pane_ui.move_panes.take().is_some() {
            self.focus_active_pane(window, cx);
            cx.notify();
        }
    }

    /// Sends the extract and closes the dialog; the new tab shows once it
    /// arrives. Does nothing while no pane is ticked.
    fn confirm_move_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.pane_ui.move_panes else {
            return;
        };
        let name = dialog.name.read(cx).text().to_owned();
        let Some(mut msg) = dialog.model.message(&name, self.pane_area_aspect()) else {
            return;
        };
        // Only panes the tab still holds go; a move of none is no move.
        let live = self.tab_pane_sessions(dialog.model.tab_id());
        if let ClientMessage::ExtractToNewTab { pane_ids, .. } = &mut msg {
            pane_ids.retain(|pane| live.iter().any(|(id, _)| id == pane));
            if pane_ids.is_empty() {
                self.close_move_panes(window, cx);
                return;
            }
        }
        tracing::info!(tab = %dialog.model.tab_id(), "moving panes to a new tab");
        self.tabs.arm_create();
        self.send(msg);
        self.close_move_panes(window, cx);
    }

    /// A key while the dialog is open; returns whether it was taken. Tab
    /// and Shift+Tab move the focus; while the name field has it every
    /// other key is the field's (Enter confirms, Esc closes). Otherwise
    /// Space and Enter press the focused control, Esc closes, and every
    /// other key does nothing.
    pub(crate) fn on_move_panes_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let aspect = self.pane_area_aspect();
        let Some(dialog) = &mut self.pane_ui.move_panes else {
            return false;
        };
        let focused = dialog.model.focused();
        match keystroke.key.as_str() {
            "tab" => {
                dialog.model.move_focus(!keystroke.modifiers.shift, aspect);
                self.sync_move_panes_focus(window, cx);
            }
            _ if focused == Control::Name => return false,
            "enter" | "space" => self.press_move_panes(focused, window, cx),
            "escape" => self.close_move_panes(window, cx),
            _ => {}
        }
        true
    }

    /// Gives the keyboard to the name field while it is focused, else to
    /// the dialog.
    fn sync_move_panes_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.pane_ui.move_panes else {
            return;
        };
        if dialog.model.focused() == Control::Name {
            dialog.name.read(cx).focus_handle(cx).focus(window);
        } else {
            self.pane_ui.dialog_focus.focus(window);
        }
        cx.notify();
    }

    /// Presses `control`, which takes the focus: a checkbox flips, a layout
    /// or a shape is chosen, the name field takes the keyboard, and the
    /// buttons confirm or close.
    fn press_move_panes(&mut self, control: Control, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.pane_ui.move_panes else {
            return;
        };
        if control == Control::Confirm && !dialog.model.confirm_enabled() {
            return;
        }
        dialog.model.set_focus(control);
        match control {
            Control::Pane(index) => dialog.model.toggle(index),
            Control::Layout(layout) => dialog.model.set_layout(layout),
            Control::Shape(cols) => dialog.model.set_shape(cols),
            Control::Name => {}
            Control::Cancel | Control::Dismiss => {
                self.close_move_panes(window, cx);
                return;
            }
            Control::Confirm => {
                self.confirm_move_panes(window, cx);
                return;
            }
        }
        self.sync_move_panes_focus(window, cx);
    }

    /// The dialog over a backdrop that takes every click beneath it.
    pub(crate) fn move_panes_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.pane_ui.move_panes.as_ref()?;
        let model = &dialog.model;
        let aspect = self.pane_area_aspect();
        let mut button = |control: Control, selected: bool| {
            let label = match control {
                Control::Pane(index) => {
                    let mark = if model.is_ticked(index) { "☑" } else { "☐" };
                    format!("{mark} {}", self.move_panes_label(model, control))
                }
                _ => self.move_panes_label(model, control),
            };
            move_panes_button(model, control, label, selected, cx)
        };
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(TITLE))
            .child(button(Control::Dismiss, false));
        let checks: Vec<Stateful<Div>> = (0..model.panes().len())
            .map(|index| button(Control::Pane(index), false))
            .collect();
        let layouts: Vec<Stateful<Div>> = LayoutChoice::ALL
            .into_iter()
            .map(|layout| button(Control::Layout(layout), layout == model.layout()))
            .collect();
        let shapes: Vec<Stateful<Div>> = if model.layout() == LayoutChoice::Grid {
            model
                .shapes(aspect)
                .into_iter()
                .map(|(cols, _)| button(Control::Shape(cols), cols == model.shape()))
                .collect()
        } else {
            Vec::new()
        };
        let footer = div()
            .flex()
            .justify_end()
            .gap(px(8.0))
            .child(button(Control::Cancel, false))
            .child(button(Control::Confirm, false));
        let row = || div().flex().flex_wrap().gap(px(6.0));
        let panel = modal_panel("move-panes-panel")
            .track_focus(&self.pane_ui.dialog_focus)
            .child(header)
            .child(muted(model.hint()))
            .child(div().flex().flex_col().gap(px(4.0)).children(checks))
            .child(muted("Layout"))
            .child(row().children(layouts))
            .when(!shapes.is_empty(), |panel| {
                panel.child(row().children(shapes))
            })
            .child(muted(NAME_LABEL))
            .child(Self::move_panes_name_field(dialog, cx))
            .child(footer);
        Some(backdrop("move-panes", panel))
    }

    /// The name field, outlined while focused; a press gives it the focus.
    fn move_panes_name_field(dialog: &MovePanesDialog, cx: &mut Context<Self>) -> Div {
        let focused = dialog.model.focused() == Control::Name;
        div()
            .debug_selector(|| "move-panes-name".to_owned())
            .px(px(6.0))
            .py(px(2.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(gpui::rgb(if focused { crate::TEXT } else { BORDER }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.press_move_panes(Control::Name, window, cx);
                }),
            )
            .child(dialog.name.clone())
    }
}

/// A control of the dialog, ringed while focused and shaded while chosen;
/// a disabled Move is dimmed.
fn move_panes_button(
    model: &MovePanes,
    control: Control,
    label: String,
    selected: bool,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let enabled = control != Control::Confirm || model.confirm_enabled();
    let button = dialog_button(
        &model.selector(control),
        label,
        false,
        control == model.focused(),
        enabled,
    )
    .when(selected, |button| button.bg(gpui::rgb(HOVER_BG)));
    button.when(enabled, |button| {
        button.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.press_move_panes(control, window, cx);
        }))
    })
}

fn muted(text: impl Into<gpui::SharedString>) -> Div {
    div().text_color(gpui::rgb(MUTED)).child(text.into())
}
