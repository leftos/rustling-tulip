//! The Themes modal on screen: the list of themes under its Built-in and
//! Yours headings beside a miniature of the app in the previewed theme, the
//! theme's light or dark word, contrast summary and terminal source; and its
//! second state, the theme editor, the tokens under their group headings
//! beside a preview of the working copy. Its rows and rules live in
//! [`crate::themes`]. The window takes a theme only on Use and on a save of
//! the theme in use, so no way out of the modal has anything to restore.

use std::path::Path;

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, ElementId, Entity, Focusable, FontWeight,
    KeyDownEvent, KeyUpEvent, MouseButton, MouseDownEvent, ScrollHandle, SharedString, Stateful,
    Subscription, WeakEntity, Window, div, prelude::*, px, relative,
};

use crate::appearance::packed;
use crate::appearance_stops::AppearanceStop;
use crate::buttons::{ButtonSize, Layer, field_frame_with_ring, focus_ring, primary_button};
use crate::contrast;
use crate::diff_view::wash;
use crate::driver_id::DriverId as _;
use crate::fonts::{self, DEFAULT_FAMILY};
use crate::session_menu::{backdrop, dialog_button, dialog_look};
use crate::spawn_view::{close_button, dialog_card, dialog_footer, dialog_title_bar};
use crate::text_input::{SelectAll, TextChanged, TextInput, TextInputEvent};
use crate::theme_store::{self, ThemeError, ThemeStore};
use crate::theme_tokens::{self, TokenGroup, TokenKind, TokenRow, TokenValue};
use crate::themes::{ContrastBadge, ThemeEditor, ThemeRow, ThemesDialog, badge_in};
use crate::ui_theme::{UiTheme, theme};
use crate::{FileFilter, RootView};

const WIDTH: f32 = 860.0;
const HEIGHT: f32 = 600.0;
const EDIT_WIDTH: f32 = 1040.0;
const EDIT_HEIGHT: f32 = 680.0;
/// The share of the window the modal may fill, each way.
const WINDOW_SHARE: f32 = 0.9;
const LIST_WIDTH: f32 = 248.0;
const TOKEN_LIST_WIDTH: f32 = 340.0;
const TOKEN_ROW_HEIGHT: f32 = 24.0;
const TOKEN_SWATCH_SIZE: f32 = 14.0;
const TOKEN_BADGE_WIDTH: f32 = 44.0;
const ROW_RADIUS: f32 = 6.0;
const MINIATURE_WIDTH: f32 = 582.0;
const MINIATURE_HEIGHT: f32 = 300.0;
const MINIATURE_SIDEBAR_WIDTH: f32 = 148.0;
/// The note a blank name shows.
const BLANK_NAME: &str = "Give the theme a name";
/// What the error row says before a failed export's error.
const EXPORT_FAILED: &str = "Couldn't export the theme";
/// What the error row says before a refused import's error.
const IMPORT_FAILED: &str = "Couldn't import the theme";

/// A control of the modal that Tab reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThemesControl {
    List,
    Cancel,
    Use,
    Name,
    Hex,
    Revert,
    Save,
    Delete,
}

impl ThemesControl {
    const fn selector(self) -> &'static str {
        match self {
            Self::List => "themes-list",
            Self::Cancel => "themes-cancel",
            Self::Use => "themes-use",
            Self::Name => "themes-name",
            Self::Hex => "themes-hex",
            Self::Revert => "themes-revert",
            Self::Save => "themes-save",
            Self::Delete => "themes-delete",
        }
    }
}

/// What the modal shows: the list of themes, or the editor of one.
enum Mode {
    List,
    Edit(Box<ThemeEditor>),
}

/// A theme file the modal offers to delete.
#[derive(Debug, Clone)]
struct DeleteTarget {
    id: String,
    name: String,
    /// The file is named after a built-in, which the id still names.
    shadow: bool,
}

/// A theme as Export writes it.
struct ExportSource {
    name: String,
    theme: UiTheme,
    base: Option<String>,
    terminal_source: Option<String>,
}

/// The file type Export saves and Import opens.
fn theme_file_filter() -> FileFilter {
    FileFilter::new("Theme", &["json"])
}

/// Saves the theme file at `path` into `store` as a new user theme, returning
/// its id. A file over [`theme_store::MAX_THEME_BYTES`] is refused unread,
/// and a parse error names the file.
fn import_file(store: &ThemeStore, path: &Path) -> Result<String, ThemeError> {
    let io = |source| ThemeError::Io {
        path: path.to_path_buf(),
        source,
    };
    let len = std::fs::metadata(path).map_err(io)?.len();
    // A length past usize is past the cap too.
    let bytes = usize::try_from(len).unwrap_or(usize::MAX);
    if bytes > theme_store::MAX_THEME_BYTES {
        return Err(ThemeError::TooLarge { bytes });
    }
    let text = std::fs::read_to_string(path).map_err(io)?;
    store.import(&text).map_err(|err| match err {
        ThemeError::Parse { source, .. } => ThemeError::Parse {
            path: path.to_path_buf(),
            source,
        },
        other => other,
    })
}

/// The question the modal asks in a row above its footer.
#[derive(Debug, Clone)]
enum Ask {
    Discard,
    Delete(DeleteTarget),
}

impl Ask {
    fn question(&self) -> String {
        match self {
            Self::Discard => "Discard your changes?".to_owned(),
            Self::Delete(target) => format!("Delete {}?", target.name),
        }
    }

    /// The answers: the one that goes ahead, then the one that keeps.
    const fn answers(&self) -> (&'static str, &'static str) {
        match self {
            Self::Discard => ("Discard", "Keep editing"),
            Self::Delete(_) => ("Delete", "Keep"),
        }
    }
}

/// The stops Tab walks: Use only while the keyboard's row can be used.
fn control_ring(can_use: bool) -> &'static [ThemesControl] {
    if can_use {
        &[
            ThemesControl::List,
            ThemesControl::Cancel,
            ThemesControl::Use,
        ]
    } else {
        &[ThemesControl::List, ThemesControl::Cancel]
    }
}

/// The open modal: its model, the control with the keyboard, why the last
/// store write or Use failed, where the list is scrolled to, and the
/// editor's state: its mode, the fields, their notes and the Pick toggle.
pub(crate) struct ThemesModal {
    dialog: ThemesDialog,
    focus: ThemesControl,
    error: Option<String>,
    scroll: ScrollHandle,
    mode: Mode,
    ask: Option<Ask>,
    /// The question's going-ahead answer has the keyboard, not the safe one.
    ask_go: bool,
    /// The Enter that opened the hex input has not been lifted yet.
    enter_down: bool,
    name_input: Entity<TextInput>,
    hex_input: Entity<TextInput>,
    name_note: Option<String>,
    hex_note: Option<&'static str>,
    pick: bool,
    token_scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

/// The place of row `index` among the list's children: the rows follow their
/// heading, so a row of Yours has two headings before it.
fn list_child(dialog: &ThemesDialog, index: usize) -> usize {
    if index < dialog.yours_start() {
        index + 1
    } else {
        index + 2
    }
}

/// What `failing` pairs below their floor come to.
fn contrast_verdict(failing: usize) -> String {
    match failing {
        0 => "All pairs pass AA".to_owned(),
        1 => "1 pair below AA".to_owned(),
        count => format!("{count} pairs below AA"),
    }
}

/// What a row's contrast summary says: its verdict, then each failing pair.
fn contrast_lines(row: &ThemeRow) -> Vec<String> {
    let verdict = contrast_verdict(row.failing.len());
    let pairs = row.failing.iter().map(|pair| {
        format!(
            "{} on {}: {:.1}:1, needs {:.1}:1",
            pair.fg, pair.bg, pair.ratio, pair.minimum
        )
    });
    std::iter::once(verdict).chain(pairs).collect()
}

/// What a row's terminal source line says, when it has a source.
fn terminal_line(row: &ThemeRow) -> Option<String> {
    (!row.terminal_source.is_empty()).then(|| format!("Terminal: {}", row.terminal_source))
}

const fn light_word(row: &ThemeRow) -> &'static str {
    if row.light { "Light" } else { "Dark" }
}

impl ThemesModal {
    /// The row the preview draws.
    fn previewed(&self) -> Option<&ThemeRow> {
        self.dialog.rows.get(self.dialog.preview())
    }

    /// The keyboard's row.
    fn cursor_row(&self) -> Option<&ThemeRow> {
        self.dialog.rows.get(self.dialog.cursor)
    }

    fn editor(&self) -> Option<&ThemeEditor> {
        match &self.mode {
            Mode::Edit(editor) => Some(editor),
            Mode::List => None,
        }
    }

    fn editor_mut(&mut self) -> Option<&mut ThemeEditor> {
        match &mut self.mode {
            Mode::Edit(editor) => Some(editor),
            Mode::List => None,
        }
    }

    /// What Export writes: the editor's working copy under the name field's
    /// trimmed text, else the keyboard's row when it has a theme.
    fn export_source(&self, cx: &App) -> Option<ExportSource> {
        match &self.mode {
            Mode::Edit(editor) => Some(ExportSource {
                name: self.name_input.read(cx).text().trim().to_owned(),
                theme: editor.working,
                base: editor.base.clone(),
                terminal_source: editor.terminal_source.clone(),
            }),
            Mode::List => {
                let row = self.cursor_row()?;
                Some(ExportSource {
                    name: row.entry.name.clone(),
                    theme: row.theme?,
                    base: row.entry.base.clone(),
                    terminal_source: row.entry.terminal_source.clone(),
                })
            }
        }
    }

    /// Fills the hex input from the keyboard's row and drops its note.
    fn sync_hex(&mut self, cx: &mut Context<RootView>) {
        let Some(editor) = self.editor() else {
            return;
        };
        let text = value_text(&editor.working, editor.row());
        self.hex_input
            .update(cx, |input, cx| input.set_text(text, cx));
        self.hex_note = None;
    }
}

impl RootView {
    /// Whether the Themes modal is open.
    #[must_use]
    pub fn themes_modal_open(&self) -> bool {
        self.themes_modal.is_some()
    }

    /// The open modal's model.
    #[must_use]
    pub fn themes_modal(&self) -> Option<&ThemesDialog> {
        Some(&self.themes_modal.as_ref()?.dialog)
    }

    /// The selector of the control with the modal's keyboard.
    #[must_use]
    pub fn themes_modal_focus(&self) -> Option<&'static str> {
        Some(self.themes_modal.as_ref()?.focus.selector())
    }

    /// Whether the modal holds the window's keyboard.
    #[must_use]
    pub fn themes_modal_has_keyboard(&self, window: &Window) -> bool {
        self.themes_modal.is_some() && self.themes_focus.is_focused(window)
    }

    /// Why the last Use failed, while the modal shows it.
    #[must_use]
    pub fn themes_modal_error(&self) -> Option<String> {
        self.themes_modal.as_ref()?.error.clone()
    }

    /// The previewed row's contrast summary, line by line.
    #[must_use]
    pub fn themes_modal_contrast(&self) -> Vec<String> {
        self.themes_modal
            .as_ref()
            .and_then(ThemesModal::previewed)
            .filter(|row| row.theme.is_some())
            .map_or_else(Vec::new, contrast_lines)
    }

    /// The previewed row's terminal source line.
    #[must_use]
    pub fn themes_modal_terminal_source(&self) -> Option<String> {
        terminal_line(self.themes_modal.as_ref()?.previewed()?)
    }

    /// The store of the user's themes; with no config dir it holds none.
    fn theme_store(&self) -> ThemeStore {
        ThemeStore::new(self.themes_dir.clone().unwrap_or_default())
    }

    /// The name of the theme in use, as the boot resolves it: a built-in's
    /// from the table, a user theme's from its one file, else the default's.
    pub(crate) fn theme_in_use_name(&self) -> String {
        let built_in = |id: &str| {
            theme_store::BUILT_INS
                .iter()
                .find(|(known, _, _)| *known == id)
                .map(|(_, name, _)| (*name).to_owned())
        };
        let user = |id: &str| {
            let dir = self.themes_dir.clone()?;
            ThemeStore::new(dir).name_of(id)
        };
        self.sidebar
            .ui_state()
            .theme
            .as_deref()
            .and_then(|id| built_in(id).or_else(|| user(id)))
            .or_else(|| built_in(theme_store::DEFAULT_ID))
            .unwrap_or_default()
    }

    /// Opens the modal on the theme in use, scrolled to it. Every theme file
    /// is read here, once.
    pub fn open_themes_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.themes_modal.is_some() {
            return;
        }
        let selected = self.sidebar.ui_state().theme.clone();
        let dialog = ThemesDialog::new(&self.theme_store(), selected.as_deref());
        let scroll = ScrollHandle::new();
        scroll.scroll_to_item(list_child(&dialog, dialog.cursor));
        let name_input = cx.new(|cx| TextInput::new("", "Theme name", cx));
        let hex_input = cx.new(|cx| TextInput::new("", "#rrggbb", cx));
        let subscriptions = Self::watch_editor_fields(&name_input, &hex_input, window, cx);
        self.themes_modal = Some(ThemesModal {
            dialog,
            focus: ThemesControl::List,
            error: None,
            scroll,
            mode: Mode::List,
            ask: None,
            ask_go: false,
            enter_down: false,
            name_input,
            hex_input,
            name_note: None,
            hex_note: None,
            pick: false,
            token_scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        });
        self.themes_focus.focus(window);
        cx.notify();
    }

    /// Closes the modal; the keyboard goes back to Settings' Theme stop while
    /// Settings is open.
    pub(crate) fn close_themes_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.themes_modal.take().is_none() {
            return;
        }
        if self.settings_open() {
            self.focus_appearance_stop(AppearanceStop::Theme, window, cx);
        }
        cx.notify();
    }

    /// A key for the Themes modal, else for Settings or the appearance editor
    /// under it; returns whether one of them is open. A key held on `Choose…`
    /// does nothing, so holding Enter cannot strobe the modal.
    pub(crate) fn on_settings_layer_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let ks = &event.keystroke;
        let taken = if self.themes_modal.is_some() {
            self.on_themes_key(event, window, cx)
        } else if self.appearance_editor.is_none() {
            return false;
        } else if event.is_held
            && matches!(ks.key.as_str(), "enter" | "space")
            && self.focused_appearance_stop(window, cx) == Some(AppearanceStop::Theme)
        {
            true
        } else {
            let typing = self.settings_typing(window, cx);
            self.on_settings_key(ks, window, cx)
                || self.on_editor_key(ks, window, cx)
                || (!typing && self.on_appearance_key(ks, window, cx))
        };
        if taken {
            cx.stop_propagation();
        }
        true
    }

    /// A key while the modal is open; returns whether it was the modal's: a
    /// chord is not. A question open takes the key, and so does the editor;
    /// otherwise Esc closes, Tab and Shift+Tab walk the ring, Up and Down
    /// move the list's cursor while the list has the keyboard, Enter presses
    /// the focused control and Space Cancel or Use. A held key presses nothing.
    fn on_themes_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.reconcile_themes_focus(window, cx);
        let Some(modal) = self.themes_modal.as_ref() else {
            return false;
        };
        let focus = modal.focus;
        let asking = modal.ask.is_some();
        let editing = modal.editor().is_some();
        let keystroke = &event.keystroke;
        let mods = &keystroke.modifiers;
        if mods.control || mods.alt || mods.platform || mods.function {
            return false;
        }
        if asking {
            self.on_themes_ask_key(event, window, cx);
            return true;
        }
        if editing {
            return self.on_edit_key(event, focus, window, cx);
        }
        let on_list = focus == ThemesControl::List;
        match keystroke.key.as_str() {
            "escape" => self.close_themes_modal(window, cx),
            "tab" => self.step_themes_focus(mods.shift, cx),
            "up" if on_list => self.move_themes_cursor(-1, cx),
            "down" if on_list => self.move_themes_cursor(1, cx),
            "enter" if !event.is_held => self.press_themes_control(focus, window, cx),
            "space" if !event.is_held && !on_list => self.press_themes_control(focus, window, cx),
            _ => {}
        }
        true
    }

    /// Tab (Shift+Tab with `back`): on to the next control, wrapping.
    fn step_themes_focus(&mut self, back: bool, cx: &mut Context<Self>) {
        let Some(modal) = self.themes_modal.as_mut() else {
            return;
        };
        let ring = control_ring(modal.dialog.can_use());
        let at = ring
            .iter()
            .position(|control| *control == modal.focus)
            .unwrap_or(0);
        let next = if back { at + ring.len() - 1 } else { at + 1 };
        if let Some(control) = ring.get(next % ring.len()) {
            modal.focus = *control;
        }
        cx.notify();
    }

    /// Moves the list's cursor `delta` rows, keeping it in view.
    fn move_themes_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(modal) = self.themes_modal.as_mut() else {
            return;
        };
        modal.dialog.move_cursor(delta);
        modal.error = None;
        modal
            .scroll
            .scroll_to_item(list_child(&modal.dialog, modal.dialog.cursor));
        cx.notify();
    }

    /// A click on row `index`: the cursor moves to it, the hover clears and
    /// the list takes the keyboard.
    fn click_theme_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(modal) = self.themes_modal.as_mut() else {
            return;
        };
        modal.focus = ThemesControl::List;
        let delta = index.cast_signed() - modal.dialog.cursor.cast_signed();
        self.move_themes_cursor(delta, cx);
    }

    /// The pointer entered row `index`, or left the list (`None`).
    fn hover_theme_row(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        if let Some(modal) = self.themes_modal.as_mut() {
            modal.dialog.set_hover(index);
            cx.notify();
        }
    }

    /// A click on `control`, or Enter or Space while it has the keyboard.
    fn press_themes_control(
        &mut self,
        control: ThemesControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match control {
            ThemesControl::Cancel => self.close_themes_modal(window, cx),
            ThemesControl::List | ThemesControl::Use => self.use_theme(window, cx),
            ThemesControl::Name
            | ThemesControl::Hex
            | ThemesControl::Revert
            | ThemesControl::Save
            | ThemesControl::Delete => {}
        }
    }

    /// Use: the keyboard's row becomes the theme in use and the modal closes;
    /// a theme that fails to load keeps it open, saying why. A row that
    /// cannot be used does nothing, and the row in use only closes the
    /// modal, so no selection is saved for it.
    fn use_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(modal) = self.themes_modal.as_ref() else {
            return;
        };
        if modal.dialog.cursor == modal.dialog.in_use {
            self.close_themes_modal(window, cx);
            return;
        }
        let Some(id) = modal.dialog.chosen_id().map(str::to_owned) else {
            return;
        };
        let name = modal
            .dialog
            .rows
            .get(modal.dialog.cursor)
            .map(|row| row.entry.name.clone());
        match self.select_theme(&id, cx) {
            Ok(()) => {
                if let Some(name) = name {
                    self.set_appearance_theme_name(name);
                }
                self.close_themes_modal(window, cx);
            }
            Err(err) => {
                tracing::warn!(id, "using the theme: {err}");
                if let Some(modal) = self.themes_modal.as_mut() {
                    modal.error = Some(err.to_string());
                }
                cx.notify();
            }
        }
    }

    /// The modal over a backdrop that takes every click beneath it.
    pub(crate) fn themes_layer(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.reconcile_themes_focus(window, cx);
        let modal = self.themes_modal.as_ref()?;
        let t = theme(cx);
        let offset = fonts::ui_text_size(cx);
        let close = close_button("themes-close", false, &t).on_click(cx.listener(
            |this, _: &ClickEvent, window, cx| {
                this.press_themes_close(window, cx);
            },
        ));
        let (title, width, height, body, foot) = match &modal.mode {
            Mode::List => (
                "Themes",
                WIDTH,
                HEIGHT,
                list_body(modal, offset, cx),
                footer(modal, cx),
            ),
            Mode::Edit(editor) => (
                "Edit theme",
                EDIT_WIDTH,
                EDIT_HEIGHT,
                edit_body(modal, editor, cx),
                edit_footer(modal, editor, cx),
            ),
        };
        let ask = modal.ask.as_ref().map(|ask| ask_row(ask, modal.ask_go, cx));
        let asking = ask.is_some();
        let body = body.relative().when(asking, |body| body.child(ask_cover()));
        let foot = foot.relative().when(asking, |foot| foot.child(ask_cover()));
        let error = modal.error.clone().map(|error| {
            div()
                .driver_id(|| "themes-error".to_owned())
                .flex_none()
                .px(px(14.0))
                .py(px(6.0))
                .bg(gpui::rgb(t.danger_bg))
                .text_size(fonts::ui_px_at(offset, 12.0))
                .text_color(gpui::rgb(t.danger_text))
                .child(error)
        });
        let panel = dialog_card(offset, "themes-panel", width, &t)
            .h(px(height))
            .max_w(relative(WINDOW_SHARE))
            .max_h(relative(WINDOW_SHARE))
            .track_focus(&self.themes_focus)
            .on_key_up(cx.listener(|this, event: &KeyUpEvent, _, _| {
                if event.keystroke.key == "enter" {
                    this.clear_enter_down();
                }
            }))
            .child(dialog_title_bar(
                offset,
                crate::fonts::ui_weight_shift(cx),
                title,
                close,
                &t,
            ))
            .child(body)
            .children(error)
            .children(ask)
            .child(foot);
        let frame = div()
            .id("themes-frame")
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(panel);
        Some(backdrop("themes-modal", frame, &t))
    }
}

/// The list of themes beside what it previews.
fn list_body(modal: &ThemesModal, offset: i8, cx: &mut Context<RootView>) -> Div {
    let t = theme(cx);
    div()
        .flex()
        .flex_1()
        .min_h_0()
        .child(theme_list(modal, cx))
        .child(details(
            modal,
            offset,
            crate::fonts::ui_weight_shift(cx),
            &t,
        ))
}

/// A heading of the list.
fn heading(selector: &'static str, label: &'static str, offset: i8, shift: i8, t: &UiTheme) -> Div {
    div()
        .driver_id(move || selector.to_owned())
        .flex_none()
        .pt(px(6.0))
        .px(px(8.0))
        .pb(px(3.0))
        .text_size(fonts::ui_px_at(offset, 11.5))
        .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
        .text_color(gpui::rgb(t.text_2))
        .child(label)
}

/// The list: the headings and their rows, then the Import button. The pointer
/// leaving it hands the preview back to the keyboard's row.
fn theme_list(modal: &ThemesModal, cx: &mut Context<RootView>) -> Stateful<Div> {
    let t = theme(cx);
    let offset = fonts::ui_text_size(cx);
    let yours = modal.dialog.yours_start();
    let mut list = div()
        .id("themes-list")
        .debug_selector(|| ThemesControl::List.selector().to_owned())
        .flex_none()
        .w(px(LIST_WIDTH))
        .p(px(8.0))
        .flex()
        .flex_col()
        .gap(px(2.0))
        .overflow_y_scroll()
        .track_scroll(&modal.scroll)
        .border_r_1()
        .border_color(gpui::rgb(t.line))
        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
            if !*hovered {
                this.hover_theme_row(None, cx);
            }
        }))
        .child(heading(
            "themes-built-in",
            "Built-in",
            offset,
            crate::fonts::ui_weight_shift(cx),
            &t,
        ));
    for (index, row) in modal.dialog.rows.iter().enumerate() {
        if index == yours {
            list = list.child(heading(
                "themes-yours",
                "Yours",
                offset,
                crate::fonts::ui_weight_shift(cx),
                &t,
            ));
        }
        list = list.child(theme_row(index, row, modal, cx));
    }
    list.child(
        div().flex_none().pt(px(10.0)).px(px(4.0)).child(
            dialog_button(
                "themes-import",
                "Import".to_owned(),
                dialog_look(offset, crate::fonts::ui_weight_shift(cx), false, true, &t),
                false,
            )
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.import_theme(cx);
            })),
        ),
    )
}

/// A row's selector and element id: a file shadowing a built-in has the
/// built-in's id, so the id alone would not tell them apart.
fn row_selector(row: &ThemeRow) -> String {
    let group = if row.entry.built_in {
        "builtin"
    } else {
        "user"
    };
    format!("themes-row-{group}-{}", row.entry.id)
}

/// One theme of the list: its name, the `In use` mark, and the error of a
/// row that cannot be used.
fn theme_row(
    index: usize,
    row: &ThemeRow,
    modal: &ThemesModal,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let t = theme(cx);
    let offset = fonts::ui_text_size(cx);
    let selector = row_selector(row);
    let selected = modal.dialog.cursor == index;
    let name = div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(row.entry.name.clone()),
        )
        .when(modal.dialog.in_use == index, |line| {
            line.child(
                div()
                    .driver_id(|| "themes-in-use".to_owned())
                    .flex_none()
                    .text_size(fonts::ui_px_at(offset, 11.0))
                    .text_color(gpui::rgb(t.text_2))
                    .child("In use"),
            )
        });
    let error = row.entry.error.clone().map(|error| {
        div()
            .driver_id(|| "themes-row-error".to_owned())
            .text_size(fonts::ui_px_at(offset, 11.0))
            .text_color(gpui::rgb(t.danger_text))
            .child(error)
    });
    div()
        .id(ElementId::Name(SharedString::from(selector.clone())))
        .driver_id(move || selector)
        .relative()
        .flex()
        .flex_col()
        .flex_none()
        .min_w_0()
        .px(px(8.0))
        .py(px(5.0))
        .rounded(px(ROW_RADIUS))
        .cursor_pointer()
        .text_size(fonts::ui_px_at(offset, 12.5))
        .text_color(gpui::rgb(t.text))
        .when(selected, |row| row.bg(gpui::rgb(t.selection)))
        .hover(|style| style.bg(gpui::rgb(t.popover_hover)))
        .child(name)
        .children(error)
        .when(selected && modal.focus == ThemesControl::List, |row| {
            row.child(focus_ring(ROW_RADIUS, &t))
        })
        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            if *hovered {
                this.hover_theme_row(Some(index), cx);
            }
        }))
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.click_theme_row(index, cx);
        }))
}

/// The right column: the preview and what it tells of the previewed theme.
fn details(modal: &ThemesModal, offset: i8, shift: i8, t: &UiTheme) -> Stateful<Div> {
    let column = div()
        .id("themes-details")
        .debug_selector(|| "themes-details".to_owned())
        .flex_1()
        .min_w_0()
        .p(px(14.0))
        .flex()
        .flex_col()
        .gap(px(8.0))
        .overflow_y_scroll()
        .text_size(fonts::ui_px_at(offset, 12.0))
        .text_color(gpui::rgb(t.text_2));
    let Some(row) = modal.previewed() else {
        return column;
    };
    let Some(previewed) = row.theme else {
        return column.children(row.entry.error.clone().map(|error| {
            div()
                .driver_id(|| "themes-preview-error".to_owned())
                .text_color(gpui::rgb(t.danger_text))
                .child(error)
        }));
    };
    let mut lines = contrast_lines(row).into_iter();
    let verdict = lines.next().unwrap_or_default();
    column
        .child(miniature(&previewed, &Picker(None)))
        .child(
            div()
                .driver_id(|| "themes-light-word".to_owned())
                .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
                .text_color(gpui::rgb(t.text))
                .child(light_word(row)),
        )
        .child(
            div()
                .driver_id(|| "themes-contrast".to_owned())
                .when(!row.failing.is_empty(), |line| {
                    line.text_color(gpui::rgb(t.danger_text))
                })
                .child(verdict),
        )
        .children(lines.map(|line| div().flex_none().child(line)))
        .children(terminal_line(row).map(|line| {
            div()
                .driver_id(|| "themes-terminal-source".to_owned())
                .child(line)
        }))
}

/// The footer's left side: Duplicate and edit for a row with a theme, Edit
/// for a user's, Delete for any user row, then Export for a row with a
/// theme. None of them is a Tab stop.
fn row_actions(modal: &ThemesModal, cx: &mut Context<RootView>) -> Div {
    let shift = fonts::ui_weight_shift(cx);
    let t = theme(cx);
    let offset = fonts::ui_text_size(cx);
    let can_duplicate = modal.dialog.can_use();
    let user = modal.cursor_row().is_some_and(|row| !row.entry.built_in);
    let duplicate = dialog_button(
        "themes-duplicate",
        "Duplicate and edit".to_owned(),
        dialog_look(offset, shift, false, can_duplicate, &t),
        false,
    )
    .when(can_duplicate, |button| {
        button.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.duplicate_theme(window, cx);
        }))
    });
    let edit = (user && can_duplicate).then(|| {
        dialog_button(
            "themes-edit",
            "Edit".to_owned(),
            dialog_look(offset, shift, false, true, &t),
            false,
        )
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.edit_theme(window, cx);
        }))
    });
    let delete = user.then(|| {
        dialog_button(
            ThemesControl::Delete.selector(),
            "Delete".to_owned(),
            dialog_look(offset, shift, true, true, &t),
            false,
        )
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.ask_delete_theme(window, cx);
        }))
    });
    let export = dialog_button(
        "themes-export",
        "Export".to_owned(),
        dialog_look(offset, shift, false, can_duplicate, &t),
        false,
    )
    .when(can_duplicate, |button| {
        button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            this.export_theme(cx);
        }))
    });
    div()
        .flex()
        .gap(px(10.0))
        .child(duplicate)
        .children(edit)
        .children(delete)
        .child(export)
}

/// The footer: what the keyboard's row offers, then Cancel and Use.
fn footer(modal: &ThemesModal, cx: &mut Context<RootView>) -> Div {
    let t = theme(cx);
    let offset = fonts::ui_text_size(cx);
    let cancel = dialog_button(
        ThemesControl::Cancel.selector(),
        "Cancel".to_owned(),
        dialog_look(offset, crate::fonts::ui_weight_shift(cx), false, true, &t),
        modal.focus == ThemesControl::Cancel,
    )
    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
        this.press_themes_control(ThemesControl::Cancel, window, cx);
    }));
    let can_use = modal.dialog.can_use();
    let use_theme = primary_button(
        offset,
        crate::fonts::ui_weight_shift(cx),
        ThemesControl::Use.selector(),
        ButtonSize::Regular,
        can_use,
        Layer::Floating,
        &t,
    )
    .relative()
    .when(modal.focus == ThemesControl::Use, |button| {
        button.child(focus_ring(ButtonSize::Regular.radius(), &t))
    })
    .child("Use")
    .when(can_use, |button| {
        button.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.press_themes_control(ThemesControl::Use, window, cx);
        }))
    });
    dialog_footer(&t)
        .justify_between()
        .child(row_actions(modal, cx))
        .child(div().flex().gap(px(10.0)).child(cancel).child(use_theme))
}

/// The editor's flows: entering and leaving it, its keys and fields, Save,
/// Revert and Delete.
impl RootView {
    /// The open editor's model.
    #[must_use]
    pub fn themes_editor(&self) -> Option<&ThemeEditor> {
        self.themes_modal.as_ref()?.editor()
    }

    /// The text in the editor's hex input.
    #[must_use]
    pub fn themes_editor_hex(&self, cx: &App) -> Option<String> {
        let modal = self.themes_modal.as_ref()?;
        modal.editor()?;
        Some(modal.hex_input.read(cx).text().to_owned())
    }

    /// Why the hex input's text was refused, while the editor shows it.
    #[must_use]
    pub fn themes_editor_hex_note(&self) -> Option<String> {
        self.themes_modal.as_ref()?.hex_note.map(str::to_owned)
    }

    /// Why the name was refused, while the editor shows it.
    #[must_use]
    pub fn themes_editor_name_note(&self) -> Option<String> {
        self.themes_modal.as_ref()?.name_note.clone()
    }

    /// The editor's contrast verdict.
    #[must_use]
    pub fn themes_editor_summary(&self) -> Option<String> {
        Some(contrast_verdict(self.themes_editor()?.failing().len()))
    }

    /// The badge the token `name` shows in the editor's list.
    #[must_use]
    pub fn themes_badge(&self, name: &str) -> Option<String> {
        self.themes_editor()?.badge(name).map(badge_text)
    }

    /// The question the modal is asking, while it asks one.
    #[must_use]
    pub fn themes_ask(&self) -> Option<String> {
        Some(self.themes_modal.as_ref()?.ask.as_ref()?.question())
    }

    /// Whether a text field of the editor has the keyboard.
    fn themes_typing(&self, window: &Window, cx: &App) -> bool {
        self.themes_modal.as_ref().is_some_and(|modal| {
            modal
                .name_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
                || modal.hex_input.read(cx).focus_handle(cx).is_focused(window)
        })
    }

    /// A field that lost the window's keyboard, to a press outside it, a
    /// question or a notice, hands the ring back to the list.
    fn reconcile_themes_focus(&mut self, window: &Window, cx: &App) {
        let typing = self.themes_typing(window, cx);
        if let Some(modal) = self.themes_modal.as_mut()
            && !typing
            && matches!(modal.focus, ThemesControl::Name | ThemesControl::Hex)
        {
            modal.focus = ThemesControl::List;
        }
    }

    /// Enter submits and Esc returns to the list in both fields; an edit of
    /// the name is the editor's, an edit of the hex input only redraws.
    fn watch_editor_fields(
        name: &Entity<TextInput>,
        hex: &Entity<TextInput>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        let name_keys = cx.subscribe_in(name, window, |this, _, _: &TextInputEvent, window, cx| {
            this.focus_themes_control(ThemesControl::List, window, cx);
        });
        let name_edits = cx.subscribe_in(name, window, |this, _, _: &TextChanged, _, cx| {
            this.rename_editor(cx);
        });
        let hex_keys = cx.subscribe_in(
            hex,
            window,
            |this, _, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => this.submit_hex(window, cx),
                TextInputEvent::Cancel => this.cancel_hex(window, cx),
            },
        );
        let hex_edits = cx.subscribe_in(hex, window, |this, _, _: &TextChanged, _, cx| {
            this.clear_enter_down();
            cx.notify();
        });
        vec![name_keys, name_edits, hex_keys, hex_edits]
    }

    /// Gives `control` the keyboard: a field takes it with its text selected,
    /// so typing replaces the text, and any other control leaves it on the
    /// modal.
    fn focus_themes_control(
        &mut self,
        control: ThemesControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.place_themes_focus(control, true, window, cx);
    }

    /// Gives `control` the keyboard, a field's text selected with `select`.
    fn place_themes_focus(
        &mut self,
        control: ThemesControl,
        select: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(modal) = self.themes_modal.as_mut() else {
            return;
        };
        modal.focus = control;
        let input = match control {
            ThemesControl::Name => Some(&modal.name_input),
            ThemesControl::Hex => Some(&modal.hex_input),
            _ => None,
        };
        if let Some(input) = input {
            input.read(cx).focus_handle(cx).focus(window);
            if select {
                window.dispatch_action(Box::new(SelectAll), cx);
            }
        } else {
            self.themes_focus.focus(window);
        }
        cx.notify();
    }

    /// Opens `editor` in the modal; nothing is written yet.
    fn open_theme_editor(
        &mut self,
        editor: ThemeEditor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(modal) = self.themes_modal.as_mut() else {
            return;
        };
        let name = editor.name.clone();
        modal
            .name_input
            .update(cx, |input, cx| input.set_text(name, cx));
        modal.mode = Mode::Edit(Box::new(editor));
        modal.error = None;
        modal.name_note = None;
        modal.ask = None;
        modal.pick = false;
        modal.token_scroll = ScrollHandle::new();
        modal.sync_hex(cx);
        self.focus_themes_control(ThemesControl::List, window, cx);
    }

    /// Duplicate and edit: a copy of the keyboard's row under a free name.
    pub fn duplicate_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.themes_modal.as_ref().and_then(ThemesModal::cursor_row) else {
            return;
        };
        let Some(theme) = row.theme else {
            return;
        };
        let name = self.theme_store().copy_name(&row.entry.name);
        let base = Some(row.entry.id.clone());
        let source = row.entry.terminal_source.clone();
        let editor = ThemeEditor::new(None, name, theme, base, source);
        self.open_theme_editor(editor, window, cx);
    }

    /// Edit: the keyboard's row, a user theme that loads, as it stands.
    fn edit_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.themes_modal.as_ref().and_then(ThemesModal::cursor_row) else {
            return;
        };
        let Some(theme) = row.theme.filter(|_| !row.entry.built_in) else {
            return;
        };
        let entry = row.entry.clone();
        let editor = ThemeEditor::new(
            Some(entry.id),
            entry.name,
            theme,
            entry.base,
            entry.terminal_source,
        );
        self.open_theme_editor(editor, window, cx);
    }

    /// A key while the modal asks: Tab moves between the two answers, the
    /// safe one first, Enter or Space presses the focused one and Esc keeps
    /// what stands. A held key presses nothing.
    fn on_themes_ask_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.is_held {
            return;
        }
        let Some(go) = self.themes_modal.as_ref().map(|modal| modal.ask_go) else {
            return;
        };
        match event.keystroke.key.as_str() {
            "tab" => {
                if let Some(modal) = self.themes_modal.as_mut() {
                    modal.ask_go = !go;
                }
                cx.notify();
            }
            "enter" | "space" if go => self.answer_themes_ask(window, cx),
            "escape" | "enter" | "space" => self.dismiss_themes_ask(cx),
            _ => {}
        }
    }

    fn dismiss_themes_ask(&mut self, cx: &mut Context<Self>) {
        if let Some(modal) = self.themes_modal.as_mut() {
            modal.ask = None;
            cx.notify();
        }
    }

    /// Asks `ask`, the keyboard on the modal so Enter and Esc answer it.
    fn set_themes_ask(&mut self, ask: Ask, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(modal) = self.themes_modal.as_mut() {
            modal.ask = Some(ask);
            modal.ask_go = false;
            self.themes_focus.focus(window);
            cx.notify();
        }
    }

    /// The asked question's first answer: discard the edits, or delete.
    fn answer_themes_ask(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self
            .themes_modal
            .as_mut()
            .and_then(|modal| modal.ask.take())
        {
            Some(Ask::Discard) => self.exit_theme_editor(window, cx),
            Some(Ask::Delete(target)) => self.delete_theme(&target, window, cx),
            None => {}
        }
    }

    /// A key in the editor; returns whether it was the editor's. Tab walks
    /// the ring from a field too; every other key of a focused field is the
    /// field's.
    fn on_edit_key(
        &mut self,
        event: &KeyDownEvent,
        focus: ThemesControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let keystroke = &event.keystroke;
        if keystroke.key == "tab" {
            self.step_edit_focus(keystroke.modifiers.shift, window, cx);
            return true;
        }
        if self.themes_typing(window, cx) {
            return false;
        }
        let focus = match focus {
            ThemesControl::Name | ThemesControl::Hex => ThemesControl::List,
            other => other,
        };
        let on_list = focus == ThemesControl::List;
        match keystroke.key.as_str() {
            "escape" if !event.is_held => self.leave_theme_editor(window, cx),
            "up" if on_list => self.move_token_cursor(-1, cx),
            "down" if on_list => self.move_token_cursor(1, cx),
            "enter" if !event.is_held => {
                if let Some(modal) = self.themes_modal.as_mut() {
                    modal.enter_down = on_list;
                }
                self.press_edit_control(focus, window, cx);
            }
            "space" if !event.is_held && !on_list => self.press_edit_control(focus, window, cx),
            _ => {}
        }
        true
    }

    /// Tab (Shift+Tab with `back`) in the editor: on to the next control,
    /// wrapping; the hex input stands where the list does.
    fn step_edit_focus(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(modal) = self.themes_modal.as_ref() else {
            return;
        };
        let Some(editor) = modal.editor() else {
            return;
        };
        let ring = edit_ring(editor);
        let from = match modal.focus {
            ThemesControl::Hex => ThemesControl::List,
            other => other,
        };
        let at = ring
            .iter()
            .position(|control| *control == from)
            .unwrap_or(0);
        let next = if back { at + ring.len() - 1 } else { at + 1 };
        let control = ring[next % ring.len()];
        self.focus_themes_control(control, window, cx);
    }

    /// Enter or Space on `control`: the list hands over to the hex input.
    fn press_edit_control(
        &mut self,
        control: ThemesControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match control {
            ThemesControl::List => self.focus_themes_control(ThemesControl::Hex, window, cx),
            ThemesControl::Revert => self.revert_editor(window, cx),
            ThemesControl::Save => self.save_editor(cx),
            ThemesControl::Delete => self.ask_delete_theme(window, cx),
            ThemesControl::Name
            | ThemesControl::Hex
            | ThemesControl::Cancel
            | ThemesControl::Use => {}
        }
    }

    fn move_token_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(editor) = self.themes_modal.as_mut().and_then(ThemesModal::editor_mut) {
            editor.move_cursor(delta);
        }
        self.after_token_move(cx);
    }

    /// The hex input follows the keyboard's row, and the list keeps it in
    /// view.
    fn after_token_move(&mut self, cx: &mut Context<Self>) {
        let Some(modal) = self.themes_modal.as_mut() else {
            return;
        };
        modal.sync_hex(cx);
        if let Some(cursor) = modal.editor().map(|editor| editor.cursor) {
            modal.token_scroll.scroll_to_item(token_child(cursor));
        }
        cx.notify();
    }

    /// A click on the token at `index`: its value is the hex input's to edit.
    fn click_token_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.themes_modal.as_mut().and_then(ThemesModal::editor_mut) {
            editor.cursor = index;
        }
        self.after_token_move(cx);
        self.focus_themes_control(ThemesControl::Hex, window, cx);
    }

    /// A press on a part of the preview that shows `token`, while Pick is on.
    fn pick_token(&mut self, token: &str, cx: &mut Context<Self>) {
        let Some(modal) = self.themes_modal.as_mut().filter(|modal| modal.pick) else {
            return;
        };
        if let Some(editor) = modal.editor_mut() {
            editor.jump_to(token);
        }
        self.after_token_move(cx);
    }

    fn toggle_pick(&mut self, cx: &mut Context<Self>) {
        if let Some(modal) = self.themes_modal.as_mut() {
            modal.pick = !modal.pick;
            cx.notify();
        }
    }

    /// The Enter that opened the hex input is up, or typing has begun.
    fn clear_enter_down(&mut self) {
        if let Some(modal) = self.themes_modal.as_mut() {
            modal.enter_down = false;
        }
    }

    /// Enter in the hex input: a value of the row's kind applies and the list
    /// takes the keyboard back; anything else shows the kind's note. The
    /// repeats of the Enter that opened the input submit nothing: a binding
    /// carries no held flag, so the opening press is remembered until it
    /// lifts. A value equal to the row's writes nothing, since a write would
    /// re-derive the tokens that follow it.
    fn submit_hex(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(modal) = self.themes_modal.as_mut() else {
            return;
        };
        if modal.enter_down {
            return;
        }
        let text = modal.hex_input.read(cx).text().to_owned();
        let Mode::Edit(editor) = &mut modal.mode else {
            return;
        };
        let row = editor.row();
        let Some(value) = theme_tokens::parse_value(row, &text) else {
            modal.hex_note = Some(value_hint(row.kind));
            cx.notify();
            return;
        };
        if theme_tokens::get(&editor.working, row.name) != Some(value) {
            editor.set_token(row.name, value);
        }
        modal.sync_hex(cx);
        self.focus_themes_control(ThemesControl::List, window, cx);
    }

    /// Esc in the hex input: its text goes back to the row's value.
    fn cancel_hex(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(modal) = self.themes_modal.as_mut() {
            modal.sync_hex(cx);
        }
        self.focus_themes_control(ThemesControl::List, window, cx);
    }

    fn rename_editor(&mut self, cx: &mut Context<Self>) {
        let Some(modal) = self.themes_modal.as_mut() else {
            return;
        };
        let name = modal.name_input.read(cx).text().to_owned();
        if let Some(editor) = modal.editor_mut() {
            editor.name = name;
        }
        modal.name_note = None;
        cx.notify();
    }

    /// Revert: the tokens and the name go back to those last saved, and a
    /// keyboard on Revert, now disabled, moves to the list.
    fn revert_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(modal) = self.themes_modal.as_mut() else {
            return;
        };
        let Some(editor) = modal.editor_mut() else {
            return;
        };
        editor.revert();
        let name = editor.name.clone();
        modal
            .name_input
            .update(cx, |input, cx| input.set_text(name, cx));
        modal.name_note = None;
        modal.sync_hex(cx);
        if modal.focus == ThemesControl::Revert {
            self.focus_themes_control(ThemesControl::List, window, cx);
        }
        cx.notify();
    }

    /// Save: a refused name shows why and writes nothing; otherwise a new
    /// theme takes its file and the editor stays on it.
    fn save_editor(&mut self, cx: &mut Context<Self>) {
        let store = self.theme_store();
        let Some(modal) = self.themes_modal.as_mut() else {
            return;
        };
        let Mode::Edit(editor) = &mut modal.mode else {
            return;
        };
        let name = editor.name.trim().to_owned();
        modal.name_note = save_refusal(&store, &name, editor.id.as_deref());
        if modal.name_note.is_some() {
            cx.notify();
            return;
        }
        editor.name.clone_from(&name);
        let base = editor.base.as_deref();
        let source = editor.terminal_source.as_deref();
        let written = match editor.id.clone() {
            None => store.save(&name, &editor.working, base, source),
            Some(id) => store
                .update(&id, &name, &editor.working, base, source)
                .map(|()| id),
        };
        match written {
            Ok(id) => {
                editor.mark_saved(id.clone());
                modal.error = None;
                modal
                    .name_input
                    .update(cx, |input, cx| input.set_text(name, cx));
                self.after_theme_saved(&id, cx);
            }
            Err(err) => self.show_themes_error("saving the theme", &err, cx),
        }
    }

    /// The window redraws when `id` is the theme in use, and the list and
    /// the Settings row follow the store.
    fn after_theme_saved(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.sidebar.ui_state().theme.as_deref() == Some(id)
            && let Err(err) = self.select_theme(id, cx)
        {
            self.show_themes_error("applying the saved theme", &err, cx);
        }
        self.rebuild_themes_dialog();
        cx.notify();
    }

    /// Lists the store's themes again, the cursor on its place clamped to
    /// the new rows, and refreshes the name on the Settings row.
    fn rebuild_themes_dialog(&mut self) {
        let selected = self.sidebar.ui_state().theme.clone();
        let mut dialog = ThemesDialog::new(&self.theme_store(), selected.as_deref());
        let name = self.theme_in_use_name();
        self.set_appearance_theme_name(name);
        if let Some(modal) = self.themes_modal.as_mut() {
            let last = dialog.rows.len().saturating_sub(1);
            dialog.cursor = modal.dialog.cursor.min(last);
            modal.dialog = dialog;
        }
    }

    /// Export: the theme's JSON goes on the clipboard, then the save picker
    /// asks where to write it, off the UI thread; a cancel keeps the
    /// clipboard. This PC's picker and file, whatever host the window is on.
    fn export_theme(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self
            .themes_modal
            .as_ref()
            .and_then(|modal| modal.export_source(cx))
        else {
            return;
        };
        if let Some(modal) = self.themes_modal.as_mut() {
            modal.error = None;
        }
        let json = match theme_store::export_json(
            &source.name,
            &source.theme,
            source.base.as_deref(),
            source.terminal_source.as_deref(),
        ) {
            Ok(json) => json,
            Err(err) => {
                self.show_themes_failure(EXPORT_FAILED, &err.to_string(), cx);
                return;
            }
        };
        self.copy_to_clipboard(&json, cx);
        let suggested = format!("{}.json", theme_store::slug(&source.name));
        let picked = (self.pick_save)(cx, None, suggested, theme_file_filter());
        cx.spawn(async move |this, cx| {
            let written = match picked.await {
                Ok(Some(path)) => {
                    cx.background_executor()
                        .spawn(async move {
                            std::fs::write(&path, json)
                                .map_err(|err| format!("{}: {err}", path.display()))
                        })
                        .await
                }
                Ok(None) => Ok(()),
                Err(err) => Err(err),
            };
            if let Err(err) = written {
                // Fails only when the view is gone, and the modal with it.
                this.update(cx, |this, cx| {
                    this.show_themes_failure(EXPORT_FAILED, &err, cx);
                })
                .ok();
            }
        })
        .detach();
    }

    /// Import: the theme file this PC's file picker returns is saved as a new
    /// user theme off the UI thread, and the list comes back with the cursor
    /// on it. Nothing is applied; Use does that.
    fn import_theme(&mut self, cx: &mut Context<Self>) {
        if let Some(modal) = self.themes_modal.as_mut() {
            modal.error = None;
        }
        let store = self.theme_store();
        let picked = (self.pick_file)(cx, None, theme_file_filter());
        cx.spawn(async move |this, cx| {
            let Some(path) = picked.await else {
                return;
            };
            let imported = cx
                .background_executor()
                .spawn(async move { import_file(&store, &path) })
                .await;
            // Fails only when the view is gone, and the modal with it.
            this.update(cx, |this, cx| match imported {
                Ok(id) => this.show_imported_theme(&id, cx),
                Err(err) => this.show_themes_failure(IMPORT_FAILED, &err.to_string(), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Lists the store again with the cursor on the imported theme `id`.
    fn show_imported_theme(&mut self, id: &str, cx: &mut Context<Self>) {
        self.rebuild_themes_dialog();
        if let Some(modal) = self.themes_modal.as_mut() {
            let at = modal
                .dialog
                .rows
                .iter()
                .position(|row| row.entry.id == id && !row.entry.built_in);
            if let Some(at) = at {
                modal.dialog.cursor = at;
                modal.dialog.hover = None;
                modal.scroll.scroll_to_item(list_child(&modal.dialog, at));
            }
            modal.error = None;
        }
        cx.notify();
    }

    /// Shows `what: err` in the modal's error row.
    fn show_themes_failure(&mut self, what: &str, err: &str, cx: &mut Context<Self>) {
        tracing::warn!("{what}: {err}");
        if let Some(modal) = self.themes_modal.as_mut() {
            modal.error = Some(format!("{what}: {err}"));
        }
        cx.notify();
    }

    fn show_themes_error(&mut self, doing: &str, err: &ThemeError, cx: &mut Context<Self>) {
        tracing::warn!("{doing}: {err}");
        if let Some(modal) = self.themes_modal.as_mut() {
            modal.error = Some(err.to_string());
        }
        cx.notify();
    }

    /// Esc or the close button in the editor: unsaved changes ask first.
    fn leave_theme_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = self.themes_editor().is_some_and(ThemeEditor::is_dirty);
        if dirty {
            self.set_themes_ask(Ask::Discard, window, cx);
        } else {
            self.exit_theme_editor(window, cx);
        }
    }

    /// Back to the list, on the theme the editor saved when it saved one.
    fn exit_theme_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(modal) = self.themes_modal.as_mut() else {
            return;
        };
        let id = match std::mem::replace(&mut modal.mode, Mode::List) {
            Mode::Edit(editor) => editor.id,
            Mode::List => None,
        };
        modal.error = None;
        modal.hex_note = None;
        modal.name_note = None;
        modal.pick = false;
        let saved_row = modal
            .dialog
            .rows
            .iter()
            .position(|row| !row.entry.built_in && Some(&row.entry.id) == id.as_ref());
        if let Some(at) = saved_row {
            modal.dialog.cursor = at;
        }
        modal.dialog.set_hover(None);
        modal
            .scroll
            .scroll_to_item(list_child(&modal.dialog, modal.dialog.cursor));
        self.focus_themes_control(ThemesControl::List, window, cx);
    }

    /// The close button: the editor's way out, else the modal's.
    fn press_themes_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.themes_editor().is_some() {
            self.leave_theme_editor(window, cx);
        } else {
            self.close_themes_modal(window, cx);
        }
    }

    /// Delete: the list's user row, or the editor's saved theme, once asked.
    fn ask_delete_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = self
            .themes_modal
            .as_ref()
            .and_then(|modal| match &modal.mode {
                Mode::List => modal.cursor_row().and_then(delete_target),
                Mode::Edit(editor) => editor.id.clone().map(|id| DeleteTarget {
                    id,
                    name: editor.name.clone(),
                    shadow: false,
                }),
            });
        if let Some(target) = target {
            self.set_themes_ask(Ask::Delete(target), window, cx);
        }
    }

    /// Deletes `target`; the theme in use falls to the default, and the modal
    /// comes back to the list rebuilt from the store.
    fn delete_theme(&mut self, target: &DeleteTarget, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.theme_store();
        let deleted = if target.shadow {
            store.delete_file(&target.id)
        } else {
            store.delete(&target.id)
        };
        if let Err(err) = deleted {
            self.show_themes_error("deleting the theme", &err, cx);
            return;
        }
        let in_use = self.sidebar.ui_state().theme.as_deref() == Some(target.id.as_str());
        let selected = if in_use && !target.shadow {
            self.select_theme(theme_store::DEFAULT_ID, cx)
        } else {
            Ok(())
        };
        self.rebuild_themes_dialog();
        if let Some(modal) = self.themes_modal.as_mut() {
            modal.mode = Mode::List;
        }
        self.exit_theme_editor(window, cx);
        if let Err(err) = selected {
            self.show_themes_error("selecting the default theme", &err, cx);
        }
    }
}

/// What keeps `name` from being saved over the theme `id` names, when
/// something does.
fn save_refusal(store: &ThemeStore, name: &str, id: Option<&str>) -> Option<String> {
    if name.is_empty() {
        return Some(BLANK_NAME.to_owned());
    }
    store
        .name_taken(name, id)
        .then(|| format!("A theme is already called {name}"))
}

/// The row as a thing to delete, when it is a user's.
fn delete_target(row: &ThemeRow) -> Option<DeleteTarget> {
    let id = &row.entry.id;
    (!row.entry.built_in).then(|| DeleteTarget {
        id: id.clone(),
        name: row.entry.name.clone(),
        shadow: theme_store::BUILT_INS
            .iter()
            .any(|(known, _, _)| known == id),
    })
}

/// The stops Tab walks in the editor: Revert only while there is a change,
/// Delete only for a saved theme.
fn edit_ring(editor: &ThemeEditor) -> Vec<ThemesControl> {
    let mut ring = vec![ThemesControl::Name, ThemesControl::List];
    if editor.is_dirty() {
        ring.push(ThemesControl::Revert);
    }
    ring.push(ThemesControl::Save);
    if editor.id.is_some() {
        ring.push(ThemesControl::Delete);
    }
    ring
}

/// The note under the hex input for a value that is not `kind`'s.
const fn value_hint(kind: TokenKind) -> &'static str {
    match kind {
        TokenKind::Opaque => "Use #RRGGBB",
        TokenKind::Alpha => "Use #RRGGBBAA",
        TokenKind::Share => "Use a number from 0 to 1",
    }
}

/// A group's heading: its selector and its words.
const fn group_heading(group: TokenGroup) -> (&'static str, &'static str) {
    match group {
        TokenGroup::Surfaces => ("themes-group-surfaces", "Surfaces"),
        TokenGroup::Text => ("themes-group-text", "Text"),
        TokenGroup::Lines => ("themes-group-lines", "Lines"),
        TokenGroup::AccentAndSelection => ("themes-group-accent", "Accent and selection"),
        TokenGroup::Status => ("themes-group-status", "Status"),
        TokenGroup::DiffAndSyntax => ("themes-group-diff", "Diff and syntax"),
        TokenGroup::Terminal => ("themes-group-terminal", "Terminal"),
    }
}

/// The place of the token at `cursor` among the list's children: each group's
/// heading comes before its rows.
fn token_child(cursor: usize) -> usize {
    let breaks = theme_tokens::ROWS[..=cursor]
        .windows(2)
        .filter(|pair| pair[0].group != pair[1].group)
        .count();
    cursor + breaks + 1
}

fn badge_text(badge: ContrastBadge) -> String {
    format!("{:.1}:1", badge.ratio)
}

/// A row's value as the editor shows it.
fn value_text(t: &UiTheme, row: &TokenRow) -> String {
    theme_tokens::get(t, row.name)
        .map_or_else(String::new, |value| theme_tokens::format_value(row, value))
}

/// What lies over the modal's body and footer while it asks: a press stops
/// here.
fn ask_cover() -> Div {
    div().absolute().top_0().left_0().size_full().occlude()
}

/// The question the modal asks above its footer, with its two answers.
fn ask_row(ask: &Ask, go: bool, cx: &mut Context<RootView>) -> Div {
    let shift = fonts::ui_weight_shift(cx);
    let t = theme(cx);
    let offset = fonts::ui_text_size(cx);
    let (yes, no) = ask.answers();
    let keep = dialog_button(
        "themes-ask-no",
        no.to_owned(),
        dialog_look(offset, shift, false, true, &t),
        !go,
    )
    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.dismiss_themes_ask(cx)));
    let answer = dialog_button(
        "themes-ask-yes",
        yes.to_owned(),
        dialog_look(offset, shift, true, true, &t),
        go,
    )
    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
        this.answer_themes_ask(window, cx);
    }));
    div()
        .driver_id(|| "themes-ask".to_owned())
        .font_weight(fonts::ui_weight_at(shift, FontWeight::NORMAL))
        .flex()
        .flex_none()
        .items_center()
        .justify_between()
        .px(px(14.0))
        .py(px(8.0))
        .bg(gpui::rgb(t.danger_bg))
        .text_size(fonts::ui_px_at(offset, 12.5))
        .text_color(gpui::rgb(t.text))
        .child(ask.question())
        .child(div().flex().gap(px(10.0)).child(keep).child(answer))
}

/// A field of the editor in its frame, tagged with its control's selector.
/// A press in the frame gives it the keyboard; the press is kept from the
/// modal's own focus.
fn editor_field(
    control: ThemesControl,
    input: &Entity<TextInput>,
    focused: bool,
    cx: &mut Context<RootView>,
) -> Div {
    let selector = control.selector();
    field_frame_with_ring(focused, format!("{selector}-ring"), &theme(cx))
        .driver_id(move || selector.to_owned())
        .flex_1()
        .min_w(px(0.0))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                window.prevent_default();
                this.place_themes_focus(control, false, window, cx);
            }),
        )
        .child(input.clone())
}

/// A note under a field, in the danger colour.
fn field_note(selector: &'static str, text: String, offset: i8, t: &UiTheme) -> Div {
    div()
        .driver_id(move || selector.to_owned())
        .flex_none()
        .text_size(fonts::ui_px_at(offset, 11.5))
        .text_color(gpui::rgb(t.danger_text))
        .child(text)
}

/// The editor between the title and the footer: the name over the tokens and
/// the preview.
fn edit_body(modal: &ThemesModal, editor: &ThemeEditor, cx: &mut Context<RootView>) -> Div {
    div()
        .font_weight(fonts::ui_weight(cx, FontWeight::NORMAL))
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .child(name_row(modal, cx))
        .child(
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .child(token_column(modal, editor, cx))
                .child(preview_column(modal, editor, cx)),
        )
}

fn name_row(modal: &ThemesModal, cx: &mut Context<RootView>) -> Div {
    let t = theme(cx);
    let offset = fonts::ui_text_size(cx);
    let focused = modal.focus == ThemesControl::Name;
    let note = modal
        .name_note
        .clone()
        .map(|note| field_note("themes-name-note", note, offset, &t));
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(10.0))
        .px(px(14.0))
        .py(px(8.0))
        .border_b_1()
        .border_color(gpui::rgb(t.line))
        .text_size(fonts::ui_px_at(offset, 12.5))
        .text_color(gpui::rgb(t.text_2))
        .child("Name")
        .child(editor_field(ThemesControl::Name, &modal.name_input, focused, cx).max_w(px(320.0)))
        .children(note)
}

/// The left column: the tokens under their group headings, scrolling, over
/// the one input that edits the keyboard's row.
fn token_column(modal: &ThemesModal, editor: &ThemeEditor, cx: &mut Context<RootView>) -> Div {
    let shift = fonts::ui_weight_shift(cx);
    let t = theme(cx);
    let offset = fonts::ui_text_size(cx);
    let pairs = contrast::check(&editor.working);
    let mut list = div()
        .id("themes-tokens")
        .debug_selector(|| ThemesControl::List.selector().to_owned())
        .flex_1()
        .min_h_0()
        .p(px(8.0))
        .flex()
        .flex_col()
        .gap(px(1.0))
        .overflow_y_scroll()
        .track_scroll(&modal.token_scroll);
    let mut group = None;
    for (index, row) in theme_tokens::ROWS.iter().enumerate() {
        if group != Some(row.group) {
            group = Some(row.group);
            let (selector, label) = group_heading(row.group);
            list = list.child(heading(selector, label, offset, shift, &t));
        }
        let badge = token_badge(row, badge_in(&pairs, row.name), offset, &t);
        list = list.child(token_row(index, row, modal, editor, cx).child(badge));
    }
    div()
        .flex_none()
        .flex()
        .flex_col()
        .w(px(TOKEN_LIST_WIDTH))
        .min_h_0()
        .border_r_1()
        .border_color(gpui::rgb(t.line))
        .child(list)
        .child(hex_panel(modal, editor, cx))
}

/// One token of the list: its swatch, label, value and contrast badge.
fn token_row(
    index: usize,
    row: &'static TokenRow,
    modal: &ThemesModal,
    editor: &ThemeEditor,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let t = theme(cx);
    let offset = fonts::ui_text_size(cx);
    let selector = format!("themes-token-{}", row.name);
    let selected = editor.cursor == index;
    div()
        .id(ElementId::Name(SharedString::from(selector.clone())))
        .driver_id(move || selector)
        .relative()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.0))
        .min_h(px(TOKEN_ROW_HEIGHT))
        .px(px(8.0))
        .rounded(px(ROW_RADIUS))
        .cursor_pointer()
        .text_size(fonts::ui_px_at(offset, 12.0))
        .text_color(gpui::rgb(t.text))
        .when(selected, |line| line.bg(gpui::rgb(t.selection)))
        .hover(|style| style.bg(gpui::rgb(t.popover_hover)))
        .child(token_swatch(row, &editor.working, &t))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(row.label),
        )
        .child(
            div()
                .flex_none()
                .font_family(DEFAULT_FAMILY)
                .font_weight(fonts::mono_weight())
                .text_color(gpui::rgb(t.text_2))
                .child(value_text(&editor.working, row)),
        )
        .when(selected && modal.focus == ThemesControl::List, |line| {
            line.child(focus_ring(ROW_RADIUS, &t))
        })
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.click_token_row(index, window, cx);
        }))
}

/// A row's colour, drawn from the working theme; a share keeps the place
/// empty so the labels line up.
fn token_swatch(row: &TokenRow, working: &UiTheme, t: &UiTheme) -> Div {
    let swatch = div()
        .flex_none()
        .size(px(TOKEN_SWATCH_SIZE))
        .rounded(px(3.0));
    match (row.kind, theme_tokens::get(working, row.name)) {
        (TokenKind::Opaque, Some(TokenValue::Color(color))) => swatch
            .bg(gpui::rgb(color))
            .border_1()
            .border_color(gpui::rgb(t.line_strong)),
        (TokenKind::Alpha, Some(TokenValue::Color(color))) => swatch
            .bg(gpui::rgba(color))
            .border_1()
            .border_color(gpui::rgb(t.line_strong)),
        _ => swatch,
    }
}

/// A row's badge, its place kept when it has none so the values line up.
fn token_badge(row: &TokenRow, badge: Option<ContrastBadge>, offset: i8, t: &UiTheme) -> Div {
    let place = div()
        .flex_none()
        .w(px(TOKEN_BADGE_WIDTH))
        .flex()
        .justify_end();
    let Some(badge) = badge else {
        return place;
    };
    let selector = format!("themes-badge-{}", row.name);
    let color = if badge.below { t.danger_text } else { t.text_2 };
    place.child(
        div()
            .driver_id(move || selector)
            .text_size(fonts::ui_px_at(offset, 11.0))
            .text_color(gpui::rgb(color))
            .child(badge_text(badge)),
    )
}

/// The one input for the keyboard's row, under its label, with its note.
fn hex_panel(modal: &ThemesModal, editor: &ThemeEditor, cx: &mut Context<RootView>) -> Div {
    let t = theme(cx);
    let offset = fonts::ui_text_size(cx);
    let focused = modal.focus == ThemesControl::Hex;
    let note = modal
        .hex_note
        .map(|note| field_note("themes-hex-note", note.to_owned(), offset, &t));
    div()
        .flex()
        .flex_col()
        .flex_none()
        .gap(px(4.0))
        .p(px(10.0))
        .border_t_1()
        .border_color(gpui::rgb(t.line))
        .text_size(fonts::ui_px_at(offset, 12.0))
        .text_color(gpui::rgb(t.text_2))
        .child(editor.row().label)
        .child(div().flex().child(editor_field(
            ThemesControl::Hex,
            &modal.hex_input,
            focused,
            cx,
        )))
        .children(note)
}

/// The right column: the Pick toggle over the preview of the working theme.
fn preview_column(
    modal: &ThemesModal,
    editor: &ThemeEditor,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let t = theme(cx);
    let offset = fonts::ui_text_size(cx);
    let shift = fonts::ui_weight_shift(cx);
    let pick = if modal.pick {
        primary_button(
            offset,
            shift,
            "themes-pick",
            ButtonSize::Regular,
            true,
            Layer::Floating,
            &t,
        )
        .child("Pick")
    } else {
        dialog_button(
            "themes-pick",
            "Pick".to_owned(),
            dialog_look(offset, shift, false, true, &t),
            false,
        )
    }
    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_pick(cx)));
    let picker = Picker(Some(cx.weak_entity()));
    div()
        .id("themes-editor-details")
        .flex_1()
        .min_w_0()
        .p(px(14.0))
        .flex()
        .flex_col()
        .gap(px(8.0))
        .overflow_y_scroll()
        .child(div().flex().flex_none().child(pick))
        .child(editor_preview(&editor.working, &picker))
}

/// The editor's footer: the contrast verdict, then Delete for a saved theme,
/// Revert, Export (not a Tab stop) and Save.
fn edit_footer(modal: &ThemesModal, editor: &ThemeEditor, cx: &mut Context<RootView>) -> Div {
    let shift = fonts::ui_weight_shift(cx);
    let t = theme(cx);
    let offset = fonts::ui_text_size(cx);
    let failing = editor.failing().len();
    let dirty = editor.is_dirty();
    let delete = editor.id.is_some().then(|| {
        dialog_button(
            ThemesControl::Delete.selector(),
            "Delete".to_owned(),
            dialog_look(offset, shift, true, true, &t),
            modal.focus == ThemesControl::Delete,
        )
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.ask_delete_theme(window, cx);
        }))
    });
    let revert = dialog_button(
        ThemesControl::Revert.selector(),
        "Revert".to_owned(),
        dialog_look(offset, shift, false, dirty, &t),
        modal.focus == ThemesControl::Revert,
    )
    .when(dirty, |button| {
        button.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.revert_editor(window, cx);
        }))
    });
    let export = dialog_button(
        "themes-export",
        "Export".to_owned(),
        dialog_look(offset, shift, false, true, &t),
        false,
    )
    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
        this.export_theme(cx);
    }));
    let save = primary_button(
        offset,
        shift,
        ThemesControl::Save.selector(),
        ButtonSize::Regular,
        true,
        Layer::Floating,
        &t,
    )
    .relative()
    .when(modal.focus == ThemesControl::Save, |button| {
        button.child(focus_ring(ButtonSize::Regular.radius(), &t))
    })
    .child("Save")
    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save_editor(cx)));
    let summary = div()
        .driver_id(|| "themes-edit-summary".to_owned())
        .font_weight(fonts::ui_weight_at(shift, FontWeight::NORMAL))
        .text_size(fonts::ui_px_at(offset, 12.5))
        .text_color(gpui::rgb(if failing == 0 {
            t.text_2
        } else {
            t.danger_text
        }))
        .child(contrast_verdict(failing));
    dialog_footer(&t).justify_between().child(summary).child(
        div()
            .flex()
            .gap(px(10.0))
            .children(delete)
            .child(revert)
            .child(export)
            .child(save),
    )
}

/// Every part of the editor's preview that Pick can press: its region's
/// selector, and the token that region shows.
const PICK_REGIONS: &[(&str, &str)] = &[
    ("pick-ground", "ground"),
    ("pick-sidebar", "surface"),
    ("pick-repo", "text"),
    ("pick-leaf", "selection"),
    ("pick-leaf-text", "text"),
    ("pick-leaf-text-quiet", "text_2"),
    ("pick-dot-working", "working"),
    ("pick-dot-asking", "asking"),
    ("pick-dot-idle", "status_idle"),
    ("pick-dot-ok", "status_ok"),
    ("pick-dot-waiting", "waiting"),
    ("pick-tabs", "sunken"),
    ("pick-tab", "ground"),
    ("pick-tab-edge", "accent"),
    ("pick-tab-quiet", "subtle"),
    ("pick-pane-header", "raised"),
    ("pick-pane-state", "primary"),
    ("pick-terminal", "terminal_bg"),
    ("pick-terminal-fg", "terminal_fg"),
    ("pick-caret", "terminal_caret"),
    ("pick-ansi-1", "ansi_1"),
    ("pick-ansi-2", "ansi_2"),
    ("pick-ansi-3", "ansi_3"),
    ("pick-ansi-4", "ansi_4"),
    ("pick-ansi-5", "ansi_5"),
    ("pick-ansi-6", "ansi_6"),
    ("pick-diff-delete", "diff_delete"),
    ("pick-diff-insert", "waiting"),
    ("pick-footer", "sunken"),
    ("pick-menu-edge", "popover_edge"),
    ("pick-menu", "popover"),
    ("pick-menu-hover", "popover_hover"),
    ("pick-menu-rule", "menu_rule"),
    ("pick-modal-edge", "modal_edge"),
    ("pick-modal", "modal"),
    ("pick-modal-inset", "modal_inset"),
    ("pick-modal-button", "primary"),
    ("pick-modal-label", "on_accent"),
    ("pick-chip-working", "working"),
    ("pick-chip-asking", "asking"),
    ("pick-chip-danger", "danger"),
    ("pick-syntax-lilac", "lilac"),
    ("pick-syntax-ochre", "ochre"),
    ("pick-syntax-rose", "rose"),
    ("pick-syntax-periwinkle", "periwinkle"),
];

/// Who hears a press on a part of a preview: the editor's preview tells the
/// root which token the part shows, and the list's preview tells no one.
struct Picker(Option<WeakEntity<RootView>>);

impl Picker {
    /// `part`, tagged `region` and telling the root its [`PICK_REGIONS`]
    /// token on a press; the press goes no further, so the innermost region
    /// answers.
    fn tag(&self, region: &'static str, part: Div) -> Div {
        let Some(root) = self.0.clone() else {
            return part;
        };
        let Some((_, token)) = PICK_REGIONS.iter().find(|(id, _)| *id == region) else {
            return part;
        };
        part.driver_id(move || region.to_owned()).on_mouse_down(
            MouseButton::Left,
            move |_: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                if let Err(err) = root.update(cx, |this, cx| this.pick_token(token, cx)) {
                    tracing::warn!("picking a token: {err}");
                }
            },
        )
    }
}

/// The editor's preview, drawn from `t` alone at fixed sizes: the miniature,
/// then samples of a menu, a modal, chips and the syntax letters. Each part
/// tells `picker` the token it shows.
fn editor_preview(t: &UiTheme, picker: &Picker) -> impl IntoElement {
    div()
        .driver_id(|| "themes-editor-preview".to_owned())
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(miniature(t, picker))
        .child(
            div()
                .flex()
                .items_start()
                .gap(px(12.0))
                .font_weight(FontWeight::NORMAL)
                .text_size(px(10.5))
                .text_color(gpui::rgb(t.text))
                .child(sample_menu(t, picker))
                .child(sample_modal(t, picker))
                .child(sample_chips(t, picker)),
        )
}

fn sample_menu(t: &UiTheme, picker: &Picker) -> Div {
    let item = |label: &'static str| div().px(px(8.0)).py(px(3.0)).rounded(px(3.0)).child(label);
    let menu = div()
        .flex()
        .flex_col()
        .w(px(136.0))
        .p(px(4.0))
        .rounded(px(5.0))
        .bg(gpui::rgb(t.popover))
        .child(picker.tag(
            "pick-menu-hover",
            item("Rename").bg(gpui::rgb(t.popover_hover)),
        ))
        .child(item("Duplicate"))
        .child(picker.tag(
            "pick-menu-rule",
            div().h(px(2.0)).my(px(3.0)).bg(gpui::rgb(t.menu_rule)),
        ))
        .child(item("Appearance"));
    picker.tag(
        "pick-menu-edge",
        div()
            .flex_none()
            .p(px(2.0))
            .rounded(px(6.0))
            .bg(gpui::rgb(t.popover_edge))
            .child(picker.tag("pick-menu", menu)),
    )
}

fn sample_modal(t: &UiTheme, picker: &Picker) -> Div {
    let button = div()
        .px(px(10.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .bg(gpui::rgb(t.primary))
        .child(picker.tag(
            "pick-modal-label",
            div().text_color(gpui::rgb(t.on_accent)).child("Rename"),
        ));
    let inset = div()
        .flex()
        .justify_end()
        .p(px(6.0))
        .rounded_b(px(5.0))
        .bg(gpui::rgb(t.modal_inset))
        .child(picker.tag("pick-modal-button", button));
    let modal = div()
        .flex()
        .flex_col()
        .w(px(176.0))
        .rounded(px(5.0))
        .bg(gpui::rgb(t.modal))
        .child(div().px(px(10.0)).py(px(8.0)).child("Rename session"))
        .child(picker.tag("pick-modal-inset", inset));
    picker.tag(
        "pick-modal-edge",
        div()
            .flex_none()
            .p(px(2.0))
            .rounded(px(6.0))
            .bg(gpui::rgb(t.modal_edge))
            .child(picker.tag("pick-modal", modal)),
    )
}

/// The status chips and the syntax letters.
fn sample_chips(t: &UiTheme, picker: &Picker) -> Div {
    let chip = |region: &'static str, label: &'static str, color: u32| {
        picker.tag(
            region,
            div()
                .flex_none()
                .px(px(8.0))
                .py(px(2.0))
                .rounded(px(9.0))
                .bg(gpui::rgb(t.chip))
                .text_color(gpui::rgb(color))
                .child(label),
        )
    };
    let letter = |region: &'static str, label: &'static str, color: u32| {
        picker.tag(
            region,
            div()
                .flex_none()
                .w(px(16.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(gpui::rgb(color))
                .child(label),
        )
    };
    div()
        .flex()
        .flex_col()
        .flex_none()
        .gap(px(6.0))
        .child(chip("pick-chip-working", "1 working", t.working))
        .child(chip("pick-chip-asking", "2 need you", t.asking))
        .child(chip("pick-chip-danger", "Failed", t.danger))
        .child(
            div()
                .flex()
                .gap(px(4.0))
                .child(letter("pick-syntax-lilac", "M", t.lilac))
                .child(letter("pick-syntax-ochre", "R", t.ochre))
                .child(letter("pick-syntax-rose", "C", t.rose))
                .child(letter("pick-syntax-periwinkle", "U", t.periwinkle)),
        )
}

/// A scale model of the app drawn from `t` alone, its sizes fixed: a sidebar,
/// a tab bar, a pane header, a terminal, a diff and a footer.
fn miniature(t: &UiTheme, picker: &Picker) -> impl IntoElement {
    let main = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .child(miniature_tabs(t, picker))
        .child(miniature_pane_header(t, picker))
        .child(miniature_terminal(t, picker))
        .child(miniature_diff(t, picker));
    picker.tag(
        "pick-ground",
        div()
            .driver_id(|| "themes-miniature".to_owned())
            .font_weight(FontWeight::NORMAL)
            .flex_none()
            .flex()
            .flex_col()
            .w(px(MINIATURE_WIDTH))
            .max_w(relative(1.0))
            .h(px(MINIATURE_HEIGHT))
            .overflow_hidden()
            .rounded(px(8.0))
            .border_1()
            .border_color(gpui::rgb(t.line_strong))
            .bg(gpui::rgb(t.ground))
            .text_size(px(10.5))
            .text_color(gpui::rgb(t.text))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(miniature_sidebar(t, picker))
                    .child(main),
            )
            .child(miniature_footer(t, picker)),
    )
}

/// A round status dot.
fn miniature_dot(color: u32) -> Div {
    div()
        .flex_none()
        .size(px(7.0))
        .rounded_full()
        .bg(gpui::rgb(color))
}

fn miniature_sidebar(t: &UiTheme, picker: &Picker) -> Div {
    let leaf = |label: &'static str, dot: u32, region: &'static str, selected: bool| {
        let line = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .h(px(22.0))
            .px(px(6.0))
            .rounded(px(4.0))
            .when(selected, |leaf| {
                leaf.bg(gpui::rgb(t.selection))
                    .border_l_2()
                    .border_color(gpui::rgb(t.accent))
            })
            .child(picker.tag(region, miniature_dot(dot)))
            .child(
                picker.tag(
                    if selected {
                        "pick-leaf-text"
                    } else {
                        "pick-leaf-text-quiet"
                    },
                    div()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_color(gpui::rgb(if selected { t.text } else { t.text_2 }))
                        .child(label),
                ),
            );
        if selected {
            picker.tag("pick-leaf", line)
        } else {
            line
        }
    };
    let repo = |label: &'static str| {
        picker.tag(
            "pick-repo",
            div()
                .pt(px(6.0))
                .pb(px(2.0))
                .px(px(6.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(gpui::rgb(t.text))
                .child(label),
        )
    };
    picker.tag(
        "pick-sidebar",
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .w(px(MINIATURE_SIDEBAR_WIDTH))
            .p(px(6.0))
            .bg(gpui::rgb(t.surface))
            .border_r_1()
            .border_color(gpui::rgb(t.line))
            .child(repo("rustling-tulip"))
            .child(leaf(
                "feat/petal-footer",
                t.working,
                "pick-dot-working",
                true,
            ))
            .child(leaf("fix/recover-keys", t.asking, "pick-dot-asking", false))
            .child(leaf("Shell", t.status_idle, "pick-dot-idle", false))
            .child(repo("flight-deck"))
            .child(leaf("deck-server", t.status_ok, "pick-dot-ok", false))
            .child(leaf("deck-client", t.waiting, "pick-dot-waiting", false)),
    )
}

fn miniature_tabs(t: &UiTheme, picker: &Picker) -> Div {
    let tab = |label: &'static str, active: bool| {
        let tab = div().flex().items_center().h_full().px(px(12.0));
        if active {
            picker.tag(
                "pick-tab",
                tab.relative()
                    .bg(gpui::rgb(t.ground))
                    .text_color(gpui::rgb(t.text))
                    .child(label)
                    .child(
                        picker.tag(
                            "pick-tab-edge",
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .right_0()
                                .h(px(2.0))
                                .bg(gpui::rgb(t.accent)),
                        ),
                    ),
            )
        } else {
            picker.tag(
                "pick-tab-quiet",
                tab.text_color(gpui::rgb(t.subtle)).child(label),
            )
        }
    };
    picker.tag(
        "pick-tabs",
        div()
            .flex_none()
            .flex()
            .h(px(26.0))
            .bg(gpui::rgb(t.bar_bg))
            .border_b_1()
            .border_color(gpui::rgb(t.line))
            .child(tab("Petal", true))
            .child(tab("Review", false))
            .child(tab("footer.rs", false)),
    )
}

fn miniature_pane_header(t: &UiTheme, picker: &Picker) -> Div {
    picker.tag(
        "pick-pane-header",
        div()
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .h(px(22.0))
            .px(px(8.0))
            .bg(gpui::rgb(t.raised))
            .border_b_1()
            .border_color(gpui::rgb(t.line))
            .text_color(gpui::rgb(t.text_2))
            .child("feat/petal-footer")
            .child(picker.tag(
                "pick-pane-state",
                div().text_color(gpui::rgb(t.primary)).child("Working"),
            )),
    )
}

/// A sample of the terminal: spans in its foreground and ANSI colours.
fn miniature_terminal(t: &UiTheme, picker: &Picker) -> Div {
    const FG: &str = "pick-terminal-fg";
    const RED: &str = "pick-ansi-1";
    const GREEN: &str = "pick-ansi-2";
    const YELLOW: &str = "pick-ansi-3";
    const BLUE: &str = "pick-ansi-4";
    const MAGENTA: &str = "pick-ansi-5";
    const CYAN: &str = "pick-ansi-6";
    let palette = t.terminal;
    let fg = packed(palette.fg);
    let [_, red, green, yellow, blue, magenta, cyan, ..] = palette.ansi.map(packed);
    let lines: [&[(&'static str, u32, &'static str)]; 6] = [
        &[
            ("~/rustling-tulip", blue, BLUE),
            (" on ", fg, FG),
            ("main", magenta, MAGENTA),
        ],
        &[("$ ", green, GREEN), ("cargo nextest run", fg, FG)],
        &[
            ("   Compiling ", green, GREEN),
            ("rustling-tulip-native", fg, FG),
        ],
        &[
            ("warning", yellow, YELLOW),
            (": unused import ", fg, FG),
            ("`px`", cyan, CYAN),
        ],
        &[
            ("error", red, RED),
            ("[E0382]: borrow of moved value", fg, FG),
        ],
        &[("    PASS ", green, GREEN), ("[0.12s] ui_theme", fg, FG)],
    ];
    picker.tag(
        "pick-terminal",
        div()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .p(px(8.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .bg(gpui::rgb(packed(palette.bg)))
            .font_family(DEFAULT_FAMILY)
            .font_weight(crate::fonts::mono_weight())
            .children(lines.into_iter().map(|spans| {
                div().flex().whitespace_nowrap().children(spans.iter().map(
                    |&(text, color, region)| {
                        picker.tag(region, div().text_color(gpui::rgb(color)).child(text))
                    },
                ))
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(div().text_color(gpui::rgb(green)).child("$"))
                    .child(
                        picker.tag(
                            "pick-caret",
                            div()
                                .w(px(6.0))
                                .h(px(12.0))
                                .bg(gpui::rgb(packed(palette.caret))),
                        ),
                    ),
            ),
    )
}

/// Two lines of a diff on the theme's washes.
fn miniature_diff(t: &UiTheme, picker: &Picker) -> Div {
    let line = |region: &'static str, text: &'static str, fill: gpui::Rgba| {
        picker.tag(
            region,
            div()
                .h(px(18.0))
                .px(px(8.0))
                .flex()
                .items_center()
                .whitespace_nowrap()
                .bg(fill)
                .child(text),
        )
    };
    div()
        .flex_none()
        .flex()
        .flex_col()
        .font_family(DEFAULT_FAMILY)
        .font_weight(crate::fonts::mono_weight())
        .text_color(gpui::rgb(t.text))
        .bg(gpui::rgb(t.ground))
        .border_t_1()
        .border_color(gpui::rgb(t.line))
        .child(line(
            "pick-diff-delete",
            "- let petal = old_footer();",
            wash(t.diff_delete, t.diff_delete_wash_alpha),
        ))
        .child(line(
            "pick-diff-insert",
            "+ let petal = footer(theme);",
            wash(t.waiting, t.diff_insert_wash_alpha),
        ))
}

fn miniature_footer(t: &UiTheme, picker: &Picker) -> Div {
    picker.tag(
        "pick-footer",
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.0))
            .h(px(22.0))
            .px(px(8.0))
            .bg(gpui::rgb(t.bar_bg))
            .border_t_1()
            .border_color(gpui::rgb(t.line))
            .text_color(gpui::rgb(t.subtle))
            .child(picker.tag("pick-dot-ok", miniature_dot(t.status_ok)))
            .child("Connected")
            .child("·")
            .child("3 sessions"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contrast::PairResult;
    use crate::theme_store::ThemeEntry;

    fn pair(fg: &'static str, ratio: f32, minimum: f32) -> PairResult {
        PairResult {
            fg,
            bg: "ground",
            ratio,
            minimum,
            passes: false,
        }
    }

    fn row(failing: Vec<PairResult>) -> ThemeRow {
        ThemeRow {
            entry: ThemeEntry {
                id: "mine".to_owned(),
                name: "Mine".to_owned(),
                built_in: false,
                error: None,
                base: None,
                terminal_source: None,
            },
            theme: Some(UiTheme::CONTRAST),
            failing,
            light: false,
            terminal_source: String::new(),
        }
    }

    #[test]
    fn one_failing_pair_reads_in_the_singular() {
        assert_eq!(
            contrast_lines(&row(vec![pair("faint", 2.84, 3.0)])),
            ["1 pair below AA", "faint on ground: 2.8:1, needs 3.0:1"]
        );
    }

    #[test]
    fn an_empty_terminal_source_shows_no_line() {
        assert_eq!(terminal_line(&row(Vec::new())), None);
    }
}
