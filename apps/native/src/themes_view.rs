//! The Themes modal on screen: the list of themes under its Built-in and
//! Yours headings beside a miniature of the app in the previewed theme, the
//! theme's light or dark word, contrast summary and terminal source. Its rows
//! and rules live in [`crate::themes`]. The window takes a theme only on Use,
//! so no way out of the modal has anything to restore.

use gpui::{
    AnyElement, ClickEvent, Context, Div, ElementId, FontWeight, KeyDownEvent, ScrollHandle,
    SharedString, Stateful, Window, div, prelude::*, px, relative,
};

use crate::RootView;
use crate::appearance::packed;
use crate::appearance_stops::AppearanceStop;
use crate::buttons::{ButtonSize, Layer, focus_ring, primary_button};
use crate::diff_view::wash;
use crate::fonts::{self, DEFAULT_FAMILY};
use crate::session_menu::{backdrop, dialog_button, dialog_look};
use crate::spawn_view::{close_button, dialog_card, dialog_footer, dialog_title_bar};
use crate::theme_store::{self, ThemeStore};
use crate::themes::{ThemeRow, ThemesDialog};
use crate::ui_theme::{UiTheme, theme};

const WIDTH: f32 = 860.0;
const HEIGHT: f32 = 600.0;
/// The share of the window the modal may fill, each way.
const WINDOW_SHARE: f32 = 0.9;
const LIST_WIDTH: f32 = 248.0;
const ROW_RADIUS: f32 = 6.0;
const MINIATURE_WIDTH: f32 = 582.0;
const MINIATURE_HEIGHT: f32 = 300.0;
const MINIATURE_SIDEBAR_WIDTH: f32 = 148.0;

/// A control of the modal that Tab reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThemesControl {
    List,
    Cancel,
    Use,
}

impl ThemesControl {
    const fn selector(self) -> &'static str {
        match self {
            Self::List => "themes-list",
            Self::Cancel => "themes-cancel",
            Self::Use => "themes-use",
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
/// Use failed, and where the list is scrolled to.
pub(crate) struct ThemesModal {
    dialog: ThemesDialog,
    focus: ThemesControl,
    error: Option<String>,
    scroll: ScrollHandle,
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

/// What a row's contrast summary says: its verdict, then each failing pair.
fn contrast_lines(row: &ThemeRow) -> Vec<String> {
    let verdict = match row.failing.len() {
        0 => "All pairs pass AA".to_owned(),
        1 => "1 pair below AA".to_owned(),
        count => format!("{count} pairs below AA"),
    };
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
        self.themes_modal = Some(ThemesModal {
            dialog,
            focus: ThemesControl::List,
            error: None,
            scroll,
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
    /// chord is not. Esc closes, Tab and Shift+Tab walk the ring, Up and Down
    /// move the list's cursor while the list has the keyboard, Enter presses
    /// the focused control and Space Cancel or Use. A held key presses nothing.
    fn on_themes_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(focus) = self.themes_modal.as_ref().map(|modal| modal.focus) else {
            return false;
        };
        let keystroke = &event.keystroke;
        let mods = &keystroke.modifiers;
        if mods.control || mods.alt || mods.platform || mods.function {
            return false;
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
    pub(crate) fn themes_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let modal = self.themes_modal.as_ref()?;
        let t = theme(cx);
        let offset = fonts::ui_text_size(cx);
        let close = close_button("themes-close", false, &t).on_click(cx.listener(
            |this, _: &ClickEvent, window, cx| {
                this.close_themes_modal(window, cx);
            },
        ));
        let body = div()
            .flex()
            .flex_1()
            .min_h_0()
            .child(theme_list(modal, cx))
            .child(details(modal, offset, &t));
        let error = modal.error.clone().map(|error| {
            div()
                .debug_selector(|| "themes-error".to_owned())
                .flex_none()
                .px(px(14.0))
                .py(px(6.0))
                .bg(gpui::rgb(t.danger_bg))
                .text_size(fonts::ui_px_at(offset, 12.0))
                .text_color(gpui::rgb(t.danger_text))
                .child(error)
        });
        let panel = dialog_card(offset, "themes-panel", WIDTH, &t)
            .h(px(HEIGHT))
            .max_w(relative(WINDOW_SHARE))
            .max_h(relative(WINDOW_SHARE))
            .track_focus(&self.themes_focus)
            .child(dialog_title_bar(offset, "Themes", close, &t))
            .child(body)
            .children(error)
            .child(footer(modal, cx));
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

/// A heading of the list.
fn heading(selector: &'static str, label: &'static str, offset: i8, t: &UiTheme) -> Div {
    div()
        .debug_selector(move || selector.to_owned())
        .flex_none()
        .pt(px(6.0))
        .px(px(8.0))
        .pb(px(3.0))
        .text_size(fonts::ui_px_at(offset, 11.5))
        .font_weight(FontWeight::SEMIBOLD)
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
        .child(heading("themes-built-in", "Built-in", offset, &t));
    for (index, row) in modal.dialog.rows.iter().enumerate() {
        if index == yours {
            list = list.child(heading("themes-yours", "Yours", offset, &t));
        }
        list = list.child(theme_row(index, row, modal, cx));
    }
    list.child(
        div()
            .flex_none()
            .pt(px(10.0))
            .px(px(4.0))
            .child(dialog_button(
                "themes-import",
                "Import".to_owned(),
                dialog_look(offset, false, false, &t),
                false,
            )),
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
                    .debug_selector(|| "themes-in-use".to_owned())
                    .flex_none()
                    .text_size(fonts::ui_px_at(offset, 11.0))
                    .text_color(gpui::rgb(t.text_2))
                    .child("In use"),
            )
        });
    let error = row.entry.error.clone().map(|error| {
        div()
            .debug_selector(|| "themes-row-error".to_owned())
            .text_size(fonts::ui_px_at(offset, 11.0))
            .text_color(gpui::rgb(t.danger_text))
            .child(error)
    });
    div()
        .id(ElementId::Name(SharedString::from(selector.clone())))
        .debug_selector(move || selector)
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
fn details(modal: &ThemesModal, offset: i8, t: &UiTheme) -> Stateful<Div> {
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
                .debug_selector(|| "themes-preview-error".to_owned())
                .text_color(gpui::rgb(t.danger_text))
                .child(error)
        }));
    };
    let mut lines = contrast_lines(row).into_iter();
    let verdict = lines.next().unwrap_or_default();
    column
        .child(miniature(&previewed))
        .child(
            div()
                .debug_selector(|| "themes-light-word".to_owned())
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(gpui::rgb(t.text))
                .child(light_word(row)),
        )
        .child(
            div()
                .debug_selector(|| "themes-contrast".to_owned())
                .when(!row.failing.is_empty(), |line| {
                    line.text_color(gpui::rgb(t.danger_text))
                })
                .child(verdict),
        )
        .children(lines.map(|line| div().flex_none().child(line)))
        .children(terminal_line(row).map(|line| {
            div()
                .debug_selector(|| "themes-terminal-source".to_owned())
                .child(line)
        }))
}

/// The footer: Duplicate and edit and Export, both disabled, then Cancel and
/// Use.
fn footer(modal: &ThemesModal, cx: &mut Context<RootView>) -> Div {
    let t = theme(cx);
    let offset = fonts::ui_text_size(cx);
    let disabled = |selector: &str, label: &str| {
        dialog_button(
            selector,
            label.to_owned(),
            dialog_look(offset, false, false, &t),
            false,
        )
    };
    let cancel = dialog_button(
        ThemesControl::Cancel.selector(),
        "Cancel".to_owned(),
        dialog_look(offset, false, true, &t),
        modal.focus == ThemesControl::Cancel,
    )
    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
        this.press_themes_control(ThemesControl::Cancel, window, cx);
    }));
    let can_use = modal.dialog.can_use();
    let use_theme = primary_button(
        offset,
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
        .child(
            div()
                .flex()
                .gap(px(10.0))
                .child(disabled("themes-duplicate", "Duplicate and edit"))
                .child(disabled("themes-export", "Export")),
        )
        .child(div().flex().gap(px(10.0)).child(cancel).child(use_theme))
}

/// A scale model of the app drawn from `t` alone, its sizes fixed: a sidebar,
/// a tab bar, a pane header, a terminal, a diff and a footer.
fn miniature(t: &UiTheme) -> impl IntoElement {
    let main = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .child(miniature_tabs(t))
        .child(miniature_pane_header(t))
        .child(miniature_terminal(t))
        .child(miniature_diff(t));
    div()
        .debug_selector(|| "themes-miniature".to_owned())
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
                .child(miniature_sidebar(t))
                .child(main),
        )
        .child(miniature_footer(t))
}

/// A round status dot.
fn miniature_dot(color: u32) -> Div {
    div()
        .flex_none()
        .size(px(7.0))
        .rounded_full()
        .bg(gpui::rgb(color))
}

fn miniature_sidebar(t: &UiTheme) -> Div {
    let leaf = |label: &'static str, dot: u32, selected: bool| {
        div()
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
            .child(miniature_dot(dot))
            .child(
                div()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(gpui::rgb(if selected { t.text } else { t.text_2 }))
                    .child(label),
            )
    };
    let repo = |label: &'static str| {
        div()
            .pt(px(6.0))
            .pb(px(2.0))
            .px(px(6.0))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(gpui::rgb(t.text))
            .child(label)
    };
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
        .child(leaf("feat/petal-footer", t.working, true))
        .child(leaf("fix/recover-keys", t.asking, false))
        .child(leaf("Shell", t.status_idle, false))
        .child(repo("flight-deck"))
        .child(leaf("deck-server", t.status_ok, false))
        .child(leaf("deck-client", t.waiting, false))
}

fn miniature_tabs(t: &UiTheme) -> Div {
    let tab = |label: &'static str, active: bool| {
        let tab = div().flex().items_center().h_full().px(px(12.0));
        if active {
            tab.bg(gpui::rgb(t.ground))
                .border_t_2()
                .border_color(gpui::rgb(t.accent))
                .text_color(gpui::rgb(t.text))
                .child(label)
        } else {
            tab.text_color(gpui::rgb(t.subtle)).child(label)
        }
    };
    div()
        .flex_none()
        .flex()
        .h(px(26.0))
        .bg(gpui::rgb(t.bar_bg))
        .border_b_1()
        .border_color(gpui::rgb(t.line))
        .child(tab("Petal", true))
        .child(tab("Review", false))
        .child(tab("footer.rs", false))
}

fn miniature_pane_header(t: &UiTheme) -> Div {
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
        .child(div().text_color(gpui::rgb(t.primary)).child("Working"))
}

/// A sample of the terminal: spans in its foreground and ANSI colours.
fn miniature_terminal(t: &UiTheme) -> Div {
    let palette = t.terminal;
    let fg = packed(palette.fg);
    let [_, red, green, yellow, blue, magenta, cyan, ..] = palette.ansi.map(packed);
    let lines: [&[(&'static str, u32)]; 6] = [
        &[("~/rustling-tulip", blue), (" on ", fg), ("main", magenta)],
        &[("$ ", green), ("cargo nextest run", fg)],
        &[("   Compiling ", green), ("rustling-tulip-native", fg)],
        &[
            ("warning", yellow),
            (": unused import ", fg),
            ("`px`", cyan),
        ],
        &[("error", red), ("[E0382]: borrow of moved value", fg)],
        &[("    PASS ", green), ("[0.12s] ui_theme", fg)],
    ];
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
        .children(lines.into_iter().map(|spans| {
            div().flex().whitespace_nowrap().children(
                spans
                    .iter()
                    .map(|&(text, color)| div().text_color(gpui::rgb(color)).child(text)),
            )
        }))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .child(div().text_color(gpui::rgb(green)).child("$"))
                .child(
                    div()
                        .w(px(6.0))
                        .h(px(12.0))
                        .bg(gpui::rgb(packed(palette.caret))),
                ),
        )
}

/// Two lines of a diff on the theme's washes.
fn miniature_diff(t: &UiTheme) -> Div {
    let line = |text: &'static str, fill: gpui::Rgba| {
        div()
            .h(px(18.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .whitespace_nowrap()
            .bg(fill)
            .child(text)
    };
    div()
        .flex_none()
        .flex()
        .flex_col()
        .font_family(DEFAULT_FAMILY)
        .text_color(gpui::rgb(t.text))
        .bg(gpui::rgb(t.ground))
        .border_t_1()
        .border_color(gpui::rgb(t.line))
        .child(line(
            "- let petal = old_footer();",
            wash(t.diff_delete, t.diff_delete_wash_alpha),
        ))
        .child(line(
            "+ let petal = footer(theme);",
            wash(t.waiting, t.diff_insert_wash_alpha),
        ))
}

fn miniature_footer(t: &UiTheme) -> Div {
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
        .child(miniature_dot(t.status_ok))
        .child("Connected")
        .child("·")
        .child("3 sessions")
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
