//! Draws the phone's Settings Keys page and the key editor over it
//! ([`super::key_grid`]'s data): a read-only preview of the Terminal bar, a
//! muted hint, a row per key in the saved order, Add key and Restore
//! defaults with its confirm row, then the editor one key at a time — its
//! label, its presses, and the picker that sets one press's key. It only
//! renders and forwards taps.

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, Focusable, SharedString, Stateful,
    Subscription, Window, div, prelude::*, px,
};

use super::PhoneRoot;
use super::home_view::{SIDE_PADDING, TOUCH_TARGET};
use super::hosts_view::{field_box, phone_button};
use super::key_grid::{
    self, DraftStep, GridKey, KeyAction, KeyDraft, KeyEdit, KeysEditMode, NamedKey, StepKey,
};
use super::long_press::touch_drag_area_ending;
use super::reorder::{self, ListId};
use super::settings::{Page, Settings, SubPage};
use super::settings_view::{CHEVRON, section_label};
use super::terminal_view::BarKeys;
use crate::buttons::{ButtonKind, ButtonSize, Layer, outlined_button};
use crate::drag::Side;
use crate::driver_id::DriverId as _;
use crate::fonts::ui_px;
use crate::text_input::{TextInput, TextInputEvent};
use crate::ui_theme::UiTheme;

/// The line over the keys, naming the two gestures the page answers.
const HINT: &str = "Long-press a key to move it; tap one to change it.";
/// The line over the keys in edit mode.
const EDIT_HINT: &str = "Drag a handle to move a key; tap a minus to remove it.";
/// The button at the hint's right that turns edit mode on.
const EDIT_LABEL: &str = "Edit";
/// The same button while edit mode is on, which turns it off.
const DONE_LABEL: &str = "Done";
/// The armed row's button that removes its key.
const DELETE_LABEL: &str = "Delete";
/// What a removable row's minus disc draws in edit mode.
const MINUS: &str = "\u{2212}";
/// What a row's drag handle draws in edit mode.
const HANDLE: &str = "\u{2261}";
/// The minus disc's diameter, in points.
const MINUS_DISC: f32 = 22.0;
/// The Restore defaults button's label.
const RESTORE_ROW_LABEL: &str = "Restore defaults";
/// The confirm row's Restore.
const RESTORE_LABEL: &str = "Restore";
/// The question the Restore defaults button turns into.
const RESTORE_ASK: &str = "Replace these with the defaults?";
/// The confirm row's Cancel.
const CANCEL_LABEL: &str = "Cancel";
/// The row that opens a new key's editor.
const ADD_KEY_LABEL: &str = "Add key";
/// The heading over the editor's presses.
const PRESSES_LABEL: &str = "Presses";
/// The row that appends a press to the key being edited.
const ADD_PRESS_LABEL: &str = "Add press";
/// The editor's Save.
const SAVE_LABEL: &str = "Save";
/// The editor's button that drops the key it edits.
const REMOVE_KEY_LABEL: &str = "Remove key";
/// A press's Remove.
const REMOVE_LABEL: &str = "Remove";
/// A press's key button before a key is chosen.
const CHOOSE_KEY_LABEL: &str = "Choose key";
/// The picker's field says what to type.
const PICKER_PLACEHOLDER: &str = "Type one character";
/// What the picker says when its field does not hold one character.
const PICKER_REFUSAL: &str = "Type one character.";
/// The picker's button that takes its field's character.
const USE_LABEL: &str = "Use";
/// What an editor says when the key it edits is gone.
const STALE_ITEM: &str = "That item no longer exists.";
/// The least height of a key's row, in points.
const ROW_HEIGHT: f32 = 48.0;
/// The widest a row's "what it sends" text draws before it truncates, in
/// points.
const SUMMARY_MAX: f32 = 160.0;

/// What the Keys page's Restore defaults draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeysRestore {
    /// The button.
    Idle,
    /// The question row, with its Restore and Cancel.
    Asking,
}

/// One press's modifier, as its toggle button names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Modifier {
    Ctrl,
    Alt,
    Shift,
}

/// What a key's row draws at its right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowEnd {
    /// Nothing: a protected key outside edit mode.
    Nothing,
    /// The chevron of a key whose tap opens its editor.
    Chevron,
    /// The armed row's Delete.
    Delete,
    /// The drag handle every other row shows in edit mode.
    Handle,
}

impl RowEnd {
    /// The end of the row at `index`, whose key is `protected` or not, while
    /// the page is in `mode`.
    fn of(index: usize, protected: bool, mode: KeysEditMode) -> Self {
        match mode {
            KeysEditMode::Off if protected => Self::Nothing,
            KeysEditMode::Off => Self::Chevron,
            KeysEditMode::On { .. } if mode.armed() == Some(index) => Self::Delete,
            KeysEditMode::On { .. } => Self::Handle,
        }
    }
}

/// The press and modifier one modifier button turns on or off.
#[derive(Debug, Clone, Copy)]
struct Toggle {
    step: usize,
    modifier: Modifier,
}

/// The open key editor: the sub-page it belongs to, the label field, the
/// presses being built, and the picker's field while the picker shows.
pub(super) struct KeyEditor {
    /// The editor sub-page the field and the draft belong to.
    sub: SubPage,
    label: Entity<TextInput>,
    draft: KeyDraft,
    /// The picker's one-character field, while the picker shows.
    picker: Option<Entity<TextInput>>,
    error: Option<String>,
    picker_error: Option<String>,
    _keys: Subscription,
}

impl KeyEditor {
    /// How many presses the draft holds.
    pub(super) fn step_count(&self) -> usize {
        self.draft.steps.len()
    }
}

/// Whether `sub` still names the Keys page's item: its key is in a grid of
/// `keys`, and its press is in the open editor's draft of `steps`. A
/// sub-page of another page is not this page's to close.
pub(super) fn key_sub_page_stands(sub: SubPage, keys: usize, steps: Option<usize>) -> bool {
    match sub {
        SubPage::KeyEditor { key } => key.is_none_or(|key| key < keys),
        SubPage::KeyPicker { step } => steps.is_some_and(|steps| step < steps),
        _ => true,
    }
}

/// A button tagged `selector`, drawn dimmed and taking no tap while `enabled`
/// is false.
fn dimmable_button(
    selector: &str,
    enabled: bool,
    label: &'static str,
    t: &UiTheme,
    cx: &Context<PhoneRoot>,
) -> Stateful<Div> {
    outlined_button(
        crate::fonts::ui_text_size(cx),
        crate::fonts::ui_weight_shift(cx),
        selector,
        ButtonSize::Large,
        enabled,
        Layer::Base,
        t,
    )
    .min_h(px(TOUCH_TARGET))
    .child(label)
}

/// The editor's refusal of a save, in the danger colour.
fn error_line(selector: &'static str, message: &str, t: &UiTheme, cx: &Context<PhoneRoot>) -> Div {
    div()
        .driver_id(|| selector.to_owned())
        .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
        .text_color(gpui::rgb(t.danger_text))
        .child(message.to_owned())
}

impl PhoneRoot {
    /// Opens Settings on its Keys page, as the shot example does.
    pub fn open_settings_keys(&mut self, cx: &mut Context<Self>) {
        self.open_settings_on(Page::Keys, cx);
    }

    /// Opens Settings on its Keys page with the editor over it, on the key
    /// at `key`, or on a new key while it is `None`, as the shot example
    /// does.
    pub fn open_settings_key_editor(
        &mut self,
        key: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_settings_on(Page::Keys, cx);
        self.open_key_editor(key, window, cx);
    }

    /// Keeps the Keys page's Restore confirm and its edit mode only while
    /// that page tops Settings: leaving the page, opening anything over it or
    /// closing Settings drops both.
    pub(super) fn keep_keys_restore_confirm(&mut self) {
        let on_page = self
            .settings
            .as_ref()
            .is_some_and(|settings| settings.top().is_none() && settings.page == Some(Page::Keys));
        if !on_page {
            self.keys_restore = KeysRestore::Idle;
            self.keys_edit = KeysEditMode::Off;
        }
    }

    /// A tap on Edit or Done: edit mode turns on with no row armed, or off.
    fn toggle_keys_edit(&mut self, cx: &mut Context<Self>) {
        self.keys_edit = self.keys_edit.toggled();
        cx.notify();
    }

    /// A tap on the minus disc of the row at `index`: arms that row, or
    /// disarms it while it is the armed one.
    fn tap_key_minus(&mut self, index: usize, cx: &mut Context<Self>) {
        let keys = self.resolved_keys(cx);
        self.keys_edit = self.keys_edit.tap_minus(index, &keys);
        cx.notify();
    }

    /// A tap on the armed row's Delete: its key leaves the list, which saves.
    fn delete_armed_key(&mut self, cx: &mut Context<Self>) {
        let (edit, next) = self.keys_edit.confirm_delete();
        self.keys_edit = next;
        if let Some(edit) = edit {
            self.write_keys(edit, cx);
        }
        cx.notify();
    }

    /// A tap on the row at `index`: in edit mode it only disarms the armed
    /// row; otherwise it opens the key's editor.
    fn tap_key_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.keys_edit.is_on() {
            self.keys_edit = self.keys_edit.disarmed();
            cx.notify();
        } else {
            self.open_key_editor(Some(index), window, cx);
        }
    }

    /// A touch on the drag handle of the row at `index`: the row lifts at
    /// once, and no row stays armed.
    fn lift_key_by_handle(&mut self, index: usize, cx: &mut Context<Self>) {
        self.keys_edit = self.keys_edit.disarmed();
        self.reorder_lift(ListId::KeyList, index, cx);
    }

    /// Drops the editor once its sub-page left the stack or Settings closed,
    /// and the picker's field once the picker left.
    pub(super) fn keep_key_editor(&mut self) {
        let Some(editor) = self.key_editor.as_ref() else {
            return;
        };
        let sub = editor.sub;
        let open = self
            .settings
            .as_ref()
            .is_some_and(|settings| settings.stack.contains(&sub));
        if !open {
            self.key_editor = None;
            return;
        }
        let picking = self
            .settings
            .as_ref()
            .is_some_and(|settings| matches!(settings.top(), Some(SubPage::KeyPicker { .. })));
        if !picking && let Some(editor) = self.key_editor.as_mut() {
            editor.picker = None;
            editor.picker_error = None;
        }
    }

    /// The grid the page draws: the saved one, else the built-in grid.
    fn saved_keys(&self, cx: &App) -> Option<Vec<GridKey>> {
        self.root.read(cx).sidebar.ui_state().phone_keys.clone()
    }

    /// The grid the page draws and edits: the saved one, else the built-in
    /// grid.
    fn resolved_keys(&self, cx: &Context<Self>) -> Vec<GridKey> {
        key_grid::resolved(self.saved_keys(cx).as_deref())
    }

    /// How many keys the grid holds.
    pub(super) fn key_count(&self, cx: &App) -> usize {
        key_grid::resolved(self.saved_keys(cx).as_deref()).len()
    }

    /// A tap on Restore defaults: the confirm row asks.
    fn ask_keys_restore(&mut self, cx: &mut Context<Self>) {
        self.keys_restore = KeysRestore::Asking;
        cx.notify();
    }

    /// A tap on the confirm row's Cancel: nothing changes.
    fn cancel_keys_restore(&mut self, cx: &mut Context<Self>) {
        self.keys_restore = KeysRestore::Idle;
        cx.notify();
    }

    /// A tap on the confirm row's Restore: the grid goes back to its built-in
    /// keys.
    fn restore_keys_defaults(&mut self, cx: &mut Context<Self>) {
        self.keys_restore = KeysRestore::Idle;
        self.keys_edit = self.keys_edit.disarmed();
        self.root
            .update(cx, |root, cx| root.set_phone_keys(None, cx));
        cx.notify();
    }

    /// Opens the editor of the key at `index`, or of a new key while it is
    /// `None`. A protected key opens none.
    fn open_key_editor(&mut self, key: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let keys = self.resolved_keys(cx);
        let (label, draft) = match key {
            None => (String::new(), KeyDraft::default()),
            Some(index) => {
                let Some(saved) = keys.get(index) else {
                    return;
                };
                if key_grid::is_protected(saved) {
                    return;
                }
                (saved.label.clone(), KeyDraft::of(saved))
            }
        };
        let sub = SubPage::KeyEditor { key };
        self.key_editor = Some(Self::make_key_editor(sub, label, draft, window, cx));
        self.push_sub_page(sub, cx);
    }

    /// The editor over `sub`: its label field holding `label` and focused,
    /// which saves on Enter.
    fn make_key_editor(
        sub: SubPage,
        label: String,
        draft: KeyDraft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> KeyEditor {
        let input = cx.new(|cx| TextInput::new(label, draft.label_placeholder(), cx));
        input.read(cx).focus_handle(cx).focus(window, cx);
        let keys = cx.subscribe(&input, |this, _, event: &TextInputEvent, cx| {
            if *event == TextInputEvent::Submit {
                this.save_key_editor(cx);
            }
        });
        KeyEditor {
            sub,
            label: input,
            draft,
            picker: None,
            error: None,
            picker_error: None,
            _keys: keys,
        }
    }

    /// Opens `sub` over the page.
    fn push_sub_page(&mut self, sub: SubPage, cx: &mut Context<Self>) {
        if let Some(settings) = self.settings.as_mut() {
            settings.push(sub);
        }
        self.keep_keys_restore_confirm();
        self.keep_key_editor();
        cx.notify();
    }

    /// Saves the open editor: a refused draft keeps it open with its message,
    /// a saved one pops it.
    fn save_key_editor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.key_editor.as_ref() else {
            return;
        };
        let SubPage::KeyEditor { key } = editor.sub else {
            return;
        };
        let typed = editor.label.read(cx).text().to_owned();
        let checked = key_grid::check_draft(&typed, &editor.draft);
        let (label, steps) = match checked {
            Ok(checked) => checked,
            Err(error) => {
                self.set_key_editor_error(&error.message(), cx);
                return;
            }
        };
        let keys = self.resolved_keys(cx);
        let id = match key {
            Some(index) => keys.get(index).map(|saved| saved.id.clone()),
            None => Some(key_grid::next_id(&keys)),
        };
        let Some(id) = id else {
            self.set_key_editor_error(STALE_ITEM, cx);
            return;
        };
        let built = GridKey {
            id,
            label,
            action: KeyAction::Send { steps },
        };
        let edit = match key {
            Some(index) => KeyEdit::Replace(index, built),
            None => KeyEdit::Add(built),
        };
        if self.write_keys(edit, cx) {
            self.close_saved_key_editor(cx);
        } else {
            self.set_key_editor_error(STALE_ITEM, cx);
        }
    }

    /// Writes the whole grid with `edit` made; false, writing nothing, while
    /// an index names nothing. No row stays armed, since the indices move.
    fn write_keys(&mut self, edit: KeyEdit, cx: &mut Context<Self>) -> bool {
        self.keys_edit = self.keys_edit.disarmed();
        let keys = self.resolved_keys(cx);
        let Some(next) = key_grid::apply_key_edit(&keys, edit) else {
            return false;
        };
        self.root
            .update(cx, |root, cx| root.set_phone_keys(Some(next), cx));
        true
    }

    /// A drag's lift on the grid: saves the grid with the key at `from`
    /// moved to `side` of the one at `to`; a move that changes nothing
    /// writes nothing. No row stays armed, since the indices move.
    pub(super) fn move_key(&mut self, from: usize, to: usize, side: Side, cx: &mut Context<Self>) {
        self.keys_edit = self.keys_edit.disarmed();
        let keys = self.resolved_keys(cx);
        let Some(next) = key_grid::apply_key_edit(&keys, KeyEdit::Move { from, to, side }) else {
            return;
        };
        if next == keys {
            return;
        }
        self.root
            .update(cx, |root, cx| root.set_phone_keys(Some(next), cx));
    }

    /// A drag's lift on the editor's presses: the press at `from` moves to
    /// `side` of the one at `to`, in the draft only.
    pub(super) fn move_press(
        &mut self,
        from: usize,
        to: usize,
        side: Side,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = self.key_editor.as_mut() {
            editor.draft.move_step(from, to, side);
        }
        self.refresh_key_placeholder(cx);
        cx.notify();
    }

    /// Pops the editor a save just came from, dropping its draft.
    fn close_saved_key_editor(&mut self, cx: &mut Context<Self>) {
        if let Some(settings) = self.settings.as_mut() {
            settings.back();
        }
        self.key_editor = None;
        cx.notify();
    }

    /// A tap on the editor's Remove key: the key leaves the grid.
    fn remove_key(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.key_editor.as_ref() else {
            return;
        };
        let SubPage::KeyEditor { key: Some(index) } = editor.sub else {
            return;
        };
        if self.write_keys(KeyEdit::Remove(index), cx) {
            self.close_saved_key_editor(cx);
        } else {
            self.set_key_editor_error(STALE_ITEM, cx);
        }
    }

    /// Says what stopped the open editor from saving, keeping it open.
    fn set_key_editor_error(&mut self, message: &str, cx: &mut Context<Self>) {
        if let Some(editor) = self.key_editor.as_mut() {
            editor.error = Some(message.to_owned());
        }
        cx.notify();
    }

    /// Says why the open picker cannot take its field, keeping it open.
    fn set_picker_error(&mut self, message: &str, cx: &mut Context<Self>) {
        if let Some(editor) = self.key_editor.as_mut() {
            editor.picker_error = Some(message.to_owned());
        }
        cx.notify();
    }

    /// Opens the picker over the editor, choosing press `step`'s key.
    fn open_key_picker(&mut self, step: usize, window: &mut Window, cx: &mut Context<Self>) {
        let open = self
            .key_editor
            .as_ref()
            .is_some_and(|editor| step < editor.draft.steps.len());
        if !open {
            return;
        }
        let input = cx.new(|cx| TextInput::new("", PICKER_PLACEHOLDER, cx));
        input.read(cx).focus_handle(cx).focus(window, cx);
        if let Some(editor) = self.key_editor.as_mut() {
            editor.picker = Some(input);
            editor.picker_error = None;
        }
        self.push_sub_page(SubPage::KeyPicker { step }, cx);
    }

    /// A tap on the picker's Use: its one character becomes the press's key,
    /// and the editor shows again.
    fn use_picker_char(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.key_editor.as_ref() else {
            return;
        };
        let Some(input) = editor.picker.as_ref() else {
            return;
        };
        let typed = input.read(cx).text().to_owned();
        let mut chars = typed.chars();
        let (Some(value), None) = (chars.next(), chars.next()) else {
            self.set_picker_error(PICKER_REFUSAL, cx);
            return;
        };
        self.set_picker_key(StepKey::Char { value }, cx);
    }

    /// A tap on a named key's picker row: it becomes the press's key, and the
    /// editor shows again.
    fn pick_named_key(&mut self, name: NamedKey, cx: &mut Context<Self>) {
        self.set_picker_key(StepKey::Named { name }, cx);
    }

    /// Sets the press the picker is over to `key` and pops back to the
    /// editor.
    fn set_picker_key(&mut self, key: StepKey, cx: &mut Context<Self>) {
        let Some(SubPage::KeyPicker { step }) = self.settings.as_ref().and_then(Settings::top)
        else {
            return;
        };
        let Some(press) = self
            .key_editor
            .as_mut()
            .and_then(|editor| editor.draft.steps.get_mut(step))
        else {
            return;
        };
        press.key = Some(key);
        if let Some(editor) = self.key_editor.as_mut() {
            editor.picker = None;
            editor.picker_error = None;
        }
        if let Some(settings) = self.settings.as_mut() {
            settings.back();
        }
        self.refresh_key_placeholder(cx);
        cx.notify();
    }

    /// A tap on Add press: an empty press joins the draft.
    fn add_step(&mut self, cx: &mut Context<Self>) {
        if let Some(editor) = self.key_editor.as_mut() {
            editor.draft.steps.push(DraftStep {
                ctrl: false,
                alt: false,
                shift: false,
                key: None,
            });
        }
        self.refresh_key_placeholder(cx);
        cx.notify();
    }

    /// A tap on a press's Remove: the press leaves the draft.
    fn remove_step(&mut self, step: usize, cx: &mut Context<Self>) {
        if let Some(editor) = self.key_editor.as_mut()
            && step < editor.draft.steps.len()
        {
            editor.draft.steps.remove(step);
        }
        self.refresh_key_placeholder(cx);
        cx.notify();
    }

    /// A tap on a press's Ctrl, Alt or Shift: it turns that modifier on or
    /// off.
    fn toggle_step(&mut self, toggle: Toggle, cx: &mut Context<Self>) {
        let Some(press) = self
            .key_editor
            .as_mut()
            .and_then(|editor| editor.draft.steps.get_mut(toggle.step))
        else {
            return;
        };
        let held = match toggle.modifier {
            Modifier::Ctrl => &mut press.ctrl,
            Modifier::Alt => &mut press.alt,
            Modifier::Shift => &mut press.shift,
        };
        *held = !*held;
        self.refresh_key_placeholder(cx);
        cx.notify();
    }

    /// Redraws the label field's placeholder from the draft's presses.
    fn refresh_key_placeholder(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.key_editor.as_ref() else {
            return;
        };
        let placeholder = editor.draft.label_placeholder();
        editor
            .label
            .update(cx, |input, cx| input.set_placeholder(placeholder, cx));
    }

    /// The Keys page: a read-only preview of the Terminal bar, the hint, a
    /// row per key in the saved order, then Add key and Restore defaults,
    /// with the page's drag area over the rows.
    pub(super) fn keys_page(&self, _: &mut Window, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        self.forget_list_boxes();
        let keys = self.resolved_keys(cx);
        div()
            .relative()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(SIDE_PADDING))
            .child(self.bar_keys_row(BarKeys::Preview, cx))
            .child(self.keys_hint_row(t, cx))
            .child(self.keys_list(&keys, t, cx))
            .child(Self::add_key_row(cx))
            .child(self.keys_restore_row(t, cx))
            .child(reorder::reorder_area(&self.list_boxes, cx))
    }

    /// The muted hint, with Edit (Done in edit mode) at its right.
    fn keys_hint_row(&self, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let (hint, label) = if self.keys_edit.is_on() {
            (EDIT_HINT, DONE_LABEL)
        } else {
            (HINT, EDIT_LABEL)
        };
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
                    .text_color(gpui::rgb(t.muted))
                    .child(hint),
            )
            .child(
                phone_button("phone-keys-edit", ButtonKind::Outlined, cx)
                    .min_h(px(TOUCH_TARGET))
                    .flex_none()
                    .child(label)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_keys_edit(cx))),
            )
    }

    /// The row that opens a new key's editor.
    fn add_key_row(cx: &mut Context<Self>) -> Stateful<Div> {
        phone_button("phone-keys-add", ButtonKind::Outlined, cx)
            .min_h(px(TOUCH_TARGET))
            .child(ADD_KEY_LABEL)
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.open_key_editor(None, window, cx);
            }))
    }

    /// The keys as one list, a row each in order, the lifted row dimmed and
    /// the drop slot drawn while a drag moves one, and its box recorded for
    /// a press to hit-test.
    fn keys_list(&self, keys: &[GridKey], t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let drag = self
            .reorder_drag
            .filter(|drag| drag.list == ListId::KeyList);
        let mode = self.keys_edit;
        let rows: Vec<AnyElement> = keys
            .iter()
            .enumerate()
            .flat_map(|(index, key)| {
                reorder::dragged_row(
                    Self::key_row(index, key, mode, t, cx),
                    drag.is_some_and(|drag| drag.from == index),
                    reorder::slot_of(drag, index),
                    t,
                )
            })
            .collect();
        div()
            .driver_id(|| "phone-keys-list".to_owned())
            .relative()
            .flex()
            .flex_col()
            .children(rows)
            .child(reorder::list_box_canvas(
                &self.list_boxes,
                ListId::KeyList,
                keys.len(),
            ))
    }

    /// One key's row: in edit mode its minus disc (a blank slot for Ctrl,
    /// Paste and Quick), its label, truncated, then what it sends in muted
    /// text when that differs from the label, one line so every row is one
    /// height, dimmed while the key does nothing, and its end
    /// ([`Self::key_row_end`]). Outside edit mode a tap opens a removable
    /// key's editor; in edit mode a tap only disarms the armed row.
    fn key_row(
        index: usize,
        key: &GridKey,
        mode: KeysEditMode,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let id = format!("phone-keys-row-{index}");
        let protected = key_grid::is_protected(key);
        let summary = key_grid::row_summary(key);
        let summary = (summary != key.label).then_some(summary);
        let row = div()
            .id(SharedString::from(id.clone()))
            .driver_id(|| id.clone())
            .flex()
            .items_center()
            .gap(px(10.0))
            .min_h(px(ROW_HEIGHT))
            .px(px(SIDE_PADDING))
            .border_b_1()
            .border_color(gpui::rgb(t.line))
            .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
            .when(mode.is_on(), |row| {
                row.child(Self::minus_slot(&id, index, protected, t, cx))
            })
            .child(div().flex_1().min_w_0().truncate().child(key.label.clone()))
            .when_some(summary, |row, summary| {
                row.child(
                    div()
                        .driver_id(|| format!("{id}-summary"))
                        .flex_none()
                        .max_w(px(SUMMARY_MAX))
                        .truncate()
                        .text_color(gpui::rgb(t.muted))
                        .child(summary),
                )
            })
            .when(key_grid::is_inert(key), |row| {
                row.opacity(crate::spawn_view::DISABLED_OPACITY)
            })
            .children(Self::key_row_end(
                &id,
                index,
                RowEnd::of(index, protected, mode),
                t,
                cx,
            ));
        if protected && !mode.is_on() {
            return row;
        }
        row.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.tap_key_row(index, window, cx);
        }))
    }

    /// A row's end: outside edit mode a chevron on a removable key and
    /// nothing on Ctrl, Paste and Quick; in edit mode the drag handle, or
    /// Delete on the armed row.
    fn key_row_end(
        id: &str,
        index: usize,
        end: RowEnd,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        match end {
            RowEnd::Nothing => None,
            RowEnd::Chevron => Some(
                div()
                    .driver_id(|| format!("{id}-chevron"))
                    .flex_none()
                    .text_color(gpui::rgb(t.muted))
                    .child(CHEVRON)
                    .into_any_element(),
            ),
            RowEnd::Delete => Some(
                phone_button(&format!("{id}-delete"), ButtonKind::Danger, cx)
                    .min_h(px(TOUCH_TARGET))
                    .flex_none()
                    .child(DELETE_LABEL)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.delete_armed_key(cx);
                    }))
                    .into_any_element(),
            ),
            RowEnd::Handle => Some(Self::key_handle(id, index, t, cx).into_any_element()),
        }
    }

    /// A row's leading slot in edit mode, a touch target wide: the minus
    /// disc on a removable key, whose tap arms or disarms the row, or a
    /// blank slot on Ctrl, Paste and Quick, so the labels line up.
    fn minus_slot(
        id: &str,
        index: usize,
        protected: bool,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let slot = div().flex_none().size(px(TOUCH_TARGET));
        if protected {
            return slot.into_any_element();
        }
        let minus = format!("{id}-minus");
        slot.id(SharedString::from(minus.clone()))
            .driver_id(|| minus.clone())
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .size(px(MINUS_DISC))
                    .rounded_full()
                    .bg(gpui::rgba(t.danger_wash))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(gpui::rgb(t.danger))
                    .child(MINUS),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                this.tap_key_minus(index, cx);
            }))
            .into_any_element()
    }

    /// A row's drag handle in edit mode, a touch target square: a touch that
    /// starts on it lifts the row at once, its moves pick the drop slot, and
    /// its lift drops the row there through the page's reorder path.
    fn key_handle(id: &str, index: usize, t: &UiTheme, cx: &mut Context<Self>) -> Stateful<Div> {
        let handle = format!("{id}-handle");
        let (lift, moved, end) = (cx.weak_entity(), cx.weak_entity(), cx.weak_entity());
        // Each update fails only when the phone's root is gone.
        let area = touch_drag_area_ending(
            "phone-keys-handle",
            move |_, _, cx| {
                lift.update(cx, |this, cx| this.lift_key_by_handle(index, cx))
                    .ok();
            },
            move |at, _, cx| {
                moved.update(cx, |this, cx| this.reorder_move(at, cx)).ok();
            },
            move |ended, _, cx| {
                end.update(cx, |this, cx| this.reorder_end(ended, cx)).ok();
            },
        );
        div()
            .id(SharedString::from(handle.clone()))
            .driver_id(|| handle.clone())
            .relative()
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .size(px(TOUCH_TARGET))
            .text_color(gpui::rgb(t.muted))
            .child(HANDLE)
            .child(area)
    }

    /// Restore defaults: the button, or the row that asks first. It is dimmed
    /// and takes no tap while the built-in grid already shows.
    fn keys_restore_row(&self, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        if self.keys_restore == KeysRestore::Asking {
            return Self::keys_restore_confirm_row(t, cx);
        }
        let saved = self.saved_keys(cx).is_some();
        let button = dimmable_button("phone-keys-restore", saved, RESTORE_ROW_LABEL, t, cx);
        let button = if saved {
            button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.ask_keys_restore(cx)))
        } else {
            button
        };
        div().flex().items_center().child(button)
    }

    /// The question Restore defaults turns into, with its Restore and Cancel.
    fn keys_restore_confirm_row(t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let confirm = dimmable_button("phone-keys-restore-confirm", true, RESTORE_LABEL, t, cx)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.restore_keys_defaults(cx);
            }));
        let cancel = dimmable_button("phone-keys-restore-cancel", true, CANCEL_LABEL, t, cx)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.cancel_keys_restore(cx)));
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(div().flex_1().min_w_0().child(RESTORE_ASK))
            .child(confirm)
            .child(cancel)
    }

    /// The sub-page the Keys page opened: the editor, or the picker over it.
    pub(super) fn key_editor_page(&self, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let Some(top) = self.settings.as_ref().and_then(Settings::top) else {
            return div();
        };
        self.forget_list_boxes();
        match top {
            SubPage::KeyPicker { .. } => self.key_picker_page(t, cx),
            SubPage::KeyEditor { .. } => self.key_editor_body(t, cx),
            _ => div(),
        }
    }

    /// The open editor: its label field, a row per press, Add press, its
    /// refusal and Save, at the top of its sub-page, and Remove key for a
    /// saved key.
    fn key_editor_body(&self, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let Some(editor) = self.key_editor.as_ref() else {
            return div();
        };
        let saved = matches!(editor.sub, SubPage::KeyEditor { key: Some(_) });
        let rows: Vec<Stateful<Div>> = editor
            .draft
            .steps
            .iter()
            .enumerate()
            .map(|(step, press)| Self::press_row(step, press, cx))
            .collect();
        let mut page = div()
            .relative()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(SIDE_PADDING))
            .child(field_box("phone-key-editor-label", &editor.label, t, cx))
            .child(section_label(PRESSES_LABEL, t, cx))
            .child(
                div()
                    .driver_id(|| "phone-key-editor-steps".to_owned())
                    .relative()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .children(rows)
                    .child(reorder::list_box_canvas(
                        &self.list_boxes,
                        ListId::KeySteps,
                        editor.draft.steps.len(),
                    )),
            )
            .child(
                phone_button("phone-key-editor-add-step", ButtonKind::Outlined, cx)
                    .min_h(px(TOUCH_TARGET))
                    .child(ADD_PRESS_LABEL)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.add_step(cx))),
            );
        if let Some(error) = editor.error.as_ref() {
            page = page.child(error_line("phone-key-editor-error", error, t, cx));
        }
        page = page.child(
            dimmable_button("phone-key-editor-save", true, SAVE_LABEL, t, cx)
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save_key_editor(cx))),
        );
        if saved {
            page = page.child(
                phone_button("phone-key-editor-remove", ButtonKind::Danger, cx)
                    .min_h(px(TOUCH_TARGET))
                    .child(REMOVE_KEY_LABEL)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.remove_key(cx))),
            );
        }
        page.child(reorder::reorder_area(&self.list_boxes, cx))
    }

    /// One press of the editor: its Ctrl, Alt and Shift toggles, its key
    /// button and its Remove.
    fn press_row(step: usize, press: &DraftStep, cx: &mut Context<Self>) -> Stateful<Div> {
        let id = format!("phone-key-editor-step-{step}");
        let remove = format!("{id}-remove");
        div()
            .id(SharedString::from(id.clone()))
            .driver_id(|| id.clone())
            .flex()
            .items_center()
            .gap(px(6.0))
            .min_h(px(TOUCH_TARGET))
            .child(Self::modifier_button(
                &format!("{id}-ctrl"),
                "Ctrl",
                press.ctrl,
                Toggle {
                    step,
                    modifier: Modifier::Ctrl,
                },
                cx,
            ))
            .child(Self::modifier_button(
                &format!("{id}-alt"),
                "Alt",
                press.alt,
                Toggle {
                    step,
                    modifier: Modifier::Alt,
                },
                cx,
            ))
            .child(Self::modifier_button(
                &format!("{id}-shift"),
                "Shift",
                press.shift,
                Toggle {
                    step,
                    modifier: Modifier::Shift,
                },
                cx,
            ))
            .child(Self::press_key_button(step, press, cx))
            .child(
                phone_button(&remove, ButtonKind::Outlined, cx)
                    .min_h(px(TOUCH_TARGET))
                    .flex_none()
                    .child(REMOVE_LABEL)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.remove_step(step, cx);
                    })),
            )
    }

    /// One press's modifier toggle, Primary while the modifier is held.
    fn modifier_button(
        selector: &str,
        label: &'static str,
        held: bool,
        toggle: Toggle,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let kind = if held {
            ButtonKind::Primary
        } else {
            ButtonKind::Outlined
        };
        phone_button(selector, kind, cx)
            .min_h(px(TOUCH_TARGET))
            .flex_none()
            .child(label)
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.toggle_step(toggle, cx);
            }))
    }

    /// One press's key button: the key's name, or Choose key while it has
    /// none; a tap opens the picker.
    fn press_key_button(step: usize, press: &DraftStep, cx: &mut Context<Self>) -> Stateful<Div> {
        let selector = format!("phone-key-editor-step-{step}-key");
        let label = match press.key {
            Some(key) => key_grid::press_key_label(key, press.shift),
            None => CHOOSE_KEY_LABEL.to_owned(),
        };
        phone_button(&selector, ButtonKind::Outlined, cx)
            .min_h(px(TOUCH_TARGET))
            .min_w_0()
            .flex_1()
            .truncate()
            .child(label)
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open_key_picker(step, window, cx);
            }))
    }

    /// The open picker: its one-character field with Use, its refusal, then
    /// a row per named key.
    fn key_picker_page(&self, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let Some(editor) = self.key_editor.as_ref() else {
            return div();
        };
        let Some(input) = editor.picker.as_ref() else {
            return div();
        };
        let mut page = div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(SIDE_PADDING))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(field_box("phone-key-picker-char", input, t, cx))
                    .child(
                        dimmable_button("phone-key-picker-char-use", true, USE_LABEL, t, cx)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.use_picker_char(cx);
                            })),
                    ),
            );
        if let Some(error) = editor.picker_error.as_ref() {
            page = page.child(error_line("phone-key-picker-error", error, t, cx));
        }
        let rows: Vec<Stateful<Div>> = NamedKey::ALL
            .iter()
            .map(|name| Self::picker_row(*name, cx))
            .collect();
        page.child(div().flex().flex_col().children(rows))
    }

    /// One named key's picker row: its name, a touch target tall; a tap sets
    /// the press's key.
    fn picker_row(name: NamedKey, cx: &mut Context<Self>) -> Stateful<Div> {
        phone_button(
            &format!("phone-key-picker-{}", name.id()),
            ButtonKind::Outlined,
            cx,
        )
        .min_h(px(TOUCH_TARGET))
        .child(name.label())
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.pick_named_key(name, cx);
        }))
    }
}
