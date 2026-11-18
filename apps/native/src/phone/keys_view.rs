//! Draws the phone's Settings Keys page and the key editor over it
//! ([`super::key_grid`]'s data): a muted hint, the key grid in fixed equal
//! columns, Add key and Restore defaults with its confirm row, then the
//! editor one key at a time — its label, its presses, and the picker that
//! sets one press's key. It only renders and forwards taps.

use gpui::{
    App, ClickEvent, Context, Div, Entity, Focusable, SharedString, Stateful, Subscription, Window,
    div, prelude::*, px,
};

use super::PhoneRoot;
use super::home_view::{SIDE_PADDING, TOUCH_TARGET};
use super::hosts_view::{field_box, phone_button};
use super::key_grid::{self, DraftStep, GridKey, KeyAction, KeyDraft, KeyEdit, NamedKey, StepKey};
use super::reorder::{self, ListId};
use super::settings::{Page, Settings, SubPage};
use super::settings_view::section_label;
use crate::buttons::{ButtonKind, ButtonSize, Layer, outlined_button};
use crate::drag::Side;
use crate::driver_id::DriverId as _;
use crate::fonts::ui_px;
use crate::text_input::{TextInput, TextInputEvent};
use crate::ui_theme::UiTheme;

/// The line over the grid, naming the two gestures the page answers.
const HINT: &str = "Long-press a key to move it; tap one to change it.";
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
/// The least width of a grid column, in points.
const CELL_MIN: f32 = 72.0;
/// The gap between two cells, in points.
const CELL_GAP: f32 = 8.0;
/// A cell's horizontal padding, its button's own: an empty place of the last
/// row takes it too, so every column is one width.
const CELL_PAD_X: f32 = 16.0;

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

    /// Keeps the Keys page's Restore confirm only while that page tops
    /// Settings: leaving the page, opening anything over it or closing
    /// Settings drops it.
    pub(super) fn keep_keys_restore_confirm(&mut self) {
        let on_page = self
            .settings
            .as_ref()
            .is_some_and(|settings| settings.top().is_none() && settings.page == Some(Page::Keys));
        if !on_page {
            self.keys_restore = KeysRestore::Idle;
        }
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
    /// an index names nothing.
    fn write_keys(&mut self, edit: KeyEdit, cx: &mut Context<Self>) -> bool {
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
    /// writes nothing.
    pub(super) fn move_key(&mut self, from: usize, to: usize, side: Side, cx: &mut Context<Self>) {
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

    /// The Keys page: the hint, the grid in as many equal columns as the
    /// window fits, then Add key and Restore defaults, with the page's drag
    /// area over the grid.
    pub(super) fn keys_page(
        &self,
        window: &mut Window,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Div {
        self.forget_list_boxes();
        let saved = self.saved_keys(cx);
        let keys = key_grid::resolved(saved.as_deref());
        let width = f32::from(window.viewport_size().width) - 2.0 * SIDE_PADDING;
        let columns = key_grid::grid_columns(width, CELL_MIN, CELL_GAP);
        div()
            .relative()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(SIDE_PADDING))
            .child(
                div()
                    .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
                    .text_color(gpui::rgb(t.muted))
                    .child(HINT),
            )
            .child(self.keys_grid(&keys, columns, cx))
            .child(Self::add_key_row(cx))
            .child(self.keys_restore_row(t, cx))
            .child(reorder::reorder_area(&self.list_boxes, cx))
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

    /// The grid: `columns` equal cells a row, the last row's empty places
    /// kept so every cell is the same width, and its box recorded for a
    /// press to hit-test.
    fn keys_grid(&self, keys: &[GridKey], columns: usize, cx: &mut Context<Self>) -> Div {
        let mut grid = div()
            .driver_id(|| "phone-keys-grid".to_owned())
            .relative()
            .flex()
            .flex_col()
            .gap(px(CELL_GAP));
        for (row, chunk) in keys.chunks(columns).enumerate() {
            let mut line = div().flex().gap(px(CELL_GAP));
            for (offset, key) in chunk.iter().enumerate() {
                let index = row * columns + offset;
                line = line.child(Self::key_cell(index, key, cx));
            }
            for _ in chunk.len()..columns {
                line = line.child(Self::grid_spacer());
            }
            grid = grid.child(line);
        }
        grid.child(reorder::grid_box_canvas(
            &self.list_boxes,
            ListId::KeyGrid,
            columns,
            CELL_GAP,
            keys.len(),
        ))
    }

    /// An empty place of the grid's last row: as wide as a cell and drawing
    /// nothing, so every column keeps one width.
    fn grid_spacer() -> Div {
        div()
            .flex_1()
            .min_w_0()
            .px(px(CELL_PAD_X))
            .border_1()
            .border_color(gpui::transparent_black())
    }

    /// One cell: the key's label on one line, truncated so its row is one
    /// height, dimmed while the key does nothing; a tap opens a send or inert
    /// key's editor, and Paste, Quick and Ctrl take none.
    fn key_cell(index: usize, key: &GridKey, cx: &mut Context<Self>) -> Stateful<Div> {
        let dimmed = key_grid::is_inert(key);
        let editable = !key_grid::is_protected(key);
        let cell = phone_button(
            &format!("phone-keys-cell-{index}"),
            ButtonKind::Outlined,
            cx,
        )
        .flex_1()
        .min_w_0()
        .min_h(px(TOUCH_TARGET))
        .truncate()
        .child(key.label.clone())
        .when(dimmed, |cell| {
            cell.opacity(crate::spawn_view::DISABLED_OPACITY)
        });
        if editable {
            cell.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open_key_editor(Some(index), window, cx);
            }))
        } else {
            cell
        }
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
