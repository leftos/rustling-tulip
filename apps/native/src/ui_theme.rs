//! The colours the interface draws in, read at run time.
//!
//! A [`UiTheme`] holds one field per [`crate::palette`] token, aliases and
//! derived tokens included. The window's theme is an app global read with
//! [`theme`]; until one is set it is [`UiTheme::CONTRAST`], the built-in
//! look whose values are the palette's. A function with no `cx` takes the
//! theme as `&UiTheme`. Its `terminal` part is the palette a terminal pane
//! paints with when no appearance level overrides its background.

#![expect(clippy::unreadable_literal, reason = "hex colors read as #rrggbb")]

use gpui::{App, Global};
use serde::{Deserialize, Serialize};

use crate::palette;
use crate::theme::{TerminalPalette, rgb};

/// Every UI colour of one look. Each field is the token of the same name
/// in [`crate::palette`]: a `u32` as `0xRRGGBB` or `0xRRGGBBAA` as that
/// token is, or an `f32` share for the diff washes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UiTheme {
    /// [`palette::GROUND`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub ground: u32,
    /// [`palette::SUNKEN`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub sunken: u32,
    /// [`palette::SURFACE`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub surface: u32,
    /// [`palette::RAISED`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub raised: u32,
    /// [`palette::CHIP`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub chip: u32,
    /// [`palette::HOVER`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub hover: u32,
    /// [`palette::PRESSED`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub pressed: u32,
    /// [`palette::POPOVER_HOVER`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub popover_hover: u32,
    /// [`palette::POPOVER_PRESSED`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub popover_pressed: u32,
    /// [`palette::LAUNCHED_TINT`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub launched_tint: u32,
    /// [`palette::ROW_CARD`].
    #[serde(with = "crate::theme_store::hex::rgba")]
    pub row_card: u32,
    /// [`palette::ROW_HOVER`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub row_hover: u32,
    /// [`palette::POPOVER`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub popover: u32,
    /// [`palette::POPOVER_EDGE`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub popover_edge: u32,
    /// [`palette::POPOVER_SHADOW`].
    #[serde(with = "crate::theme_store::hex::rgba")]
    pub popover_shadow: u32,
    /// [`palette::MODAL`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub modal: u32,
    /// [`palette::MODAL_EDGE`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub modal_edge: u32,
    /// [`palette::MODAL_INSET`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub modal_inset: u32,
    /// [`palette::MENU_RULE`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub menu_rule: u32,
    /// [`palette::CARD_EDGE`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub card_edge: u32,
    /// [`palette::LINE`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub line: u32,
    /// [`palette::LINE_STRONG`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub line_strong: u32,
    /// [`palette::TEXT`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub text: u32,
    /// [`palette::TEXT_2`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub text_2: u32,
    /// [`palette::SUBTLE`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub subtle: u32,
    /// [`palette::FAINT`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub faint: u32,
    /// [`palette::ACCENT`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub accent: u32,
    /// [`palette::ON_ACCENT`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub on_accent: u32,
    /// [`palette::PRIMARY`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub primary: u32,
    /// [`palette::PRIMARY_HOVER`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub primary_hover: u32,
    /// [`palette::PRIMARY_PRESSED`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub primary_pressed: u32,
    /// [`palette::SELECTION`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub selection: u32,
    /// [`palette::SCRIM`].
    #[serde(with = "crate::theme_store::hex::rgba")]
    pub scrim: u32,
    /// [`palette::DIALOG_SHADOW`].
    #[serde(with = "crate::theme_store::hex::rgba")]
    pub dialog_shadow: u32,
    /// [`palette::TOAST_SHADOW`].
    #[serde(with = "crate::theme_store::hex::rgba")]
    pub toast_shadow: u32,
    /// [`palette::WORKING`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub working: u32,
    /// [`palette::ASKING`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub asking: u32,
    /// [`palette::ON_ASKING`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub on_asking: u32,
    /// [`palette::WAITING`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub waiting: u32,
    /// [`palette::DANGER`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub danger: u32,
    /// [`palette::DANGER_TEXT`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub danger_text: u32,
    /// [`palette::LILAC`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub lilac: u32,
    /// [`palette::OCHRE`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub ochre: u32,
    /// [`palette::ROSE`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub rose: u32,
    /// [`palette::PERIWINKLE`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub periwinkle: u32,
    /// [`palette::CHANGE_DELETED`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub change_deleted: u32,
    /// [`palette::DIFF_DELETE`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub diff_delete: u32,
    /// [`palette::DIFF_FILLER`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub diff_filler: u32,
    /// [`palette::DIFF_DELETE_WASH_ALPHA`].
    pub diff_delete_wash_alpha: f32,
    /// [`palette::DIFF_INSERT_WASH_ALPHA`].
    pub diff_insert_wash_alpha: f32,
    /// [`palette::DIFF_FILLER_ALPHA`].
    pub diff_filler_alpha: f32,
    /// [`palette::DIFF_WORD_WASH_ALPHA`].
    pub diff_word_wash_alpha: f32,
    /// [`palette::PANEL_BG`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub panel_bg: u32,
    /// [`palette::BAR_BG`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub bar_bg: u32,
    /// `palette::HOVER_BG`.
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub hover_bg: u32,
    /// [`palette::BORDER`].
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub border: u32,
    /// `palette::MUTED`.
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub muted: u32,
    /// `palette::WARNING`.
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub warning: u32,
    /// `palette::SELECTED_BG`.
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub selected_bg: u32,
    /// `palette::INPUT_TEXT`.
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub input_text: u32,
    /// `palette::INPUT_PLACEHOLDER`.
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub input_placeholder: u32,
    /// `palette::INPUT_CURSOR`.
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub input_cursor: u32,
    /// `palette::INPUT_SELECTION`.
    #[serde(with = "crate::theme_store::hex::rgba")]
    pub input_selection: u32,
    /// `palette::ASK_WASH`.
    #[serde(with = "crate::theme_store::hex::rgba")]
    pub ask_wash: u32,
    /// `palette::ASK_BORDER`.
    #[serde(with = "crate::theme_store::hex::rgba")]
    pub ask_border: u32,
    /// `palette::DANGER_WASH`.
    #[serde(with = "crate::theme_store::hex::rgba")]
    pub danger_wash: u32,
    /// `palette::CONTEXT_HOT`.
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub context_hot: u32,
    /// `palette::DANGER_BG`.
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub danger_bg: u32,
    /// `palette::STATUS_OK`.
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub status_ok: u32,
    /// `palette::OK_WASH`.
    #[serde(with = "crate::theme_store::hex::rgba")]
    pub ok_wash: u32,
    /// `palette::STATUS_IDLE`.
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub status_idle: u32,
    /// `palette::STATUS_ERR`.
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub status_err: u32,
    /// `palette::SPAWN_BADGE_OK`.
    #[serde(with = "crate::theme_store::hex::rgb")]
    pub spawn_badge_ok: u32,
    /// `palette::SHELL_MARK_BORDER`.
    #[serde(with = "crate::theme_store::hex::rgba")]
    pub shell_mark_border: u32,
    /// `palette::TRANSPARENT`.
    #[serde(with = "crate::theme_store::hex::rgba")]
    pub transparent: u32,
    /// The terminal's colours, drawn as given: no contrast nudge.
    /// Stored apart from the UI tokens, so a serialized `UiTheme` leaves it out.
    #[serde(skip)]
    pub terminal: TerminalPalette,
}

impl UiTheme {
    /// The built-in look: every field is its [`crate::palette`] token.
    pub const CONTRAST: Self = Self {
        ground: palette::GROUND,
        sunken: palette::SUNKEN,
        surface: palette::SURFACE,
        raised: palette::RAISED,
        chip: palette::CHIP,
        hover: palette::HOVER,
        pressed: palette::PRESSED,
        popover_hover: palette::POPOVER_HOVER,
        popover_pressed: palette::POPOVER_PRESSED,
        launched_tint: palette::LAUNCHED_TINT,
        row_card: palette::ROW_CARD,
        row_hover: palette::ROW_HOVER,
        popover: palette::POPOVER,
        popover_edge: palette::POPOVER_EDGE,
        popover_shadow: palette::POPOVER_SHADOW,
        modal: palette::MODAL,
        modal_edge: palette::MODAL_EDGE,
        modal_inset: palette::MODAL_INSET,
        menu_rule: palette::MENU_RULE,
        card_edge: palette::CARD_EDGE,
        line: palette::LINE,
        line_strong: palette::LINE_STRONG,
        text: palette::TEXT,
        text_2: palette::TEXT_2,
        subtle: palette::SUBTLE,
        faint: palette::FAINT,
        accent: palette::ACCENT,
        on_accent: palette::ON_ACCENT,
        primary: palette::PRIMARY,
        primary_hover: palette::PRIMARY_HOVER,
        primary_pressed: palette::PRIMARY_PRESSED,
        selection: palette::SELECTION,
        scrim: palette::SCRIM,
        dialog_shadow: palette::DIALOG_SHADOW,
        toast_shadow: palette::TOAST_SHADOW,
        working: palette::WORKING,
        asking: palette::ASKING,
        on_asking: palette::ON_ASKING,
        waiting: palette::WAITING,
        danger: palette::DANGER,
        danger_text: palette::DANGER_TEXT,
        lilac: palette::LILAC,
        ochre: palette::OCHRE,
        rose: palette::ROSE,
        periwinkle: palette::PERIWINKLE,
        change_deleted: palette::CHANGE_DELETED,
        diff_delete: palette::DIFF_DELETE,
        diff_filler: palette::DIFF_FILLER,
        diff_delete_wash_alpha: palette::DIFF_DELETE_WASH_ALPHA,
        diff_insert_wash_alpha: palette::DIFF_INSERT_WASH_ALPHA,
        diff_filler_alpha: palette::DIFF_FILLER_ALPHA,
        diff_word_wash_alpha: palette::DIFF_WORD_WASH_ALPHA,
        panel_bg: palette::PANEL_BG,
        bar_bg: palette::BAR_BG,
        hover_bg: palette::HOVER_BG,
        border: palette::BORDER,
        muted: palette::MUTED,
        warning: palette::WARNING,
        selected_bg: palette::SELECTED_BG,
        input_text: palette::INPUT_TEXT,
        input_placeholder: palette::INPUT_PLACEHOLDER,
        input_cursor: palette::INPUT_CURSOR,
        input_selection: palette::INPUT_SELECTION,
        ask_wash: palette::ASK_WASH,
        ask_border: palette::ASK_BORDER,
        danger_wash: palette::DANGER_WASH,
        context_hot: palette::CONTEXT_HOT,
        danger_bg: palette::DANGER_BG,
        status_ok: palette::STATUS_OK,
        ok_wash: palette::OK_WASH,
        status_idle: palette::STATUS_IDLE,
        status_err: palette::STATUS_ERR,
        spawn_badge_ok: palette::SPAWN_BADGE_OK,
        shell_mark_border: palette::SHELL_MARK_BORDER,
        transparent: palette::TRANSPARENT,
        terminal: TerminalPalette {
            fg: rgb(0xe5e6e8),
            bg: rgb(0x070609),
            caret: rgb(0xffffff),
            selection: rgb(0x5b9bff),
            selection_alpha: 0.3,
            selection_fg: rgb(0xe5e6e8),
            ansi: [
                rgb(0x5a5c5f),
                rgb(0xef5c5c),
                rgb(0x3fb96a),
                rgb(0xe8a531),
                rgb(0x5b9bff),
                rgb(0xb787f0),
                rgb(0x5dd5e3),
                rgb(0xe5e6e8),
                rgb(0x656872),
                rgb(0xff8585),
                rgb(0x62d18a),
                rgb(0xf5c267),
                rgb(0x7eb4ff),
                rgb(0xd4abff),
                rgb(0x8feaf3),
                rgb(0xf5f6f8),
            ],
        },
    };
}

/// The theme the window draws in.
struct ActiveTheme(UiTheme);

impl Global for ActiveTheme {}

/// The theme the interface draws in; [`UiTheme::CONTRAST`] until
/// [`set_theme`] has run.
#[must_use]
pub fn theme(cx: &App) -> UiTheme {
    cx.try_global::<ActiveTheme>()
        .map_or(UiTheme::CONTRAST, |active| active.0)
}

/// Makes `theme` the one the interface draws in. The caller refreshes the
/// windows when they are already drawn.
pub fn set_theme(theme: UiTheme, cx: &mut App) {
    cx.set_global(ActiveTheme(theme));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each colour field of `t` beside its palette token, by name.
    fn colour_pairs(t: &UiTheme) -> Vec<(&'static str, u32, u32)> {
        vec![
            ("ground", t.ground, palette::GROUND),
            ("sunken", t.sunken, palette::SUNKEN),
            ("surface", t.surface, palette::SURFACE),
            ("raised", t.raised, palette::RAISED),
            ("chip", t.chip, palette::CHIP),
            ("hover", t.hover, palette::HOVER),
            ("pressed", t.pressed, palette::PRESSED),
            ("popover_hover", t.popover_hover, palette::POPOVER_HOVER),
            (
                "popover_pressed",
                t.popover_pressed,
                palette::POPOVER_PRESSED,
            ),
            ("launched_tint", t.launched_tint, palette::LAUNCHED_TINT),
            ("row_card", t.row_card, palette::ROW_CARD),
            ("row_hover", t.row_hover, palette::ROW_HOVER),
            ("popover", t.popover, palette::POPOVER),
            ("popover_edge", t.popover_edge, palette::POPOVER_EDGE),
            ("popover_shadow", t.popover_shadow, palette::POPOVER_SHADOW),
            ("modal", t.modal, palette::MODAL),
            ("modal_edge", t.modal_edge, palette::MODAL_EDGE),
            ("modal_inset", t.modal_inset, palette::MODAL_INSET),
            ("menu_rule", t.menu_rule, palette::MENU_RULE),
            ("card_edge", t.card_edge, palette::CARD_EDGE),
            ("line", t.line, palette::LINE),
            ("line_strong", t.line_strong, palette::LINE_STRONG),
            ("text", t.text, palette::TEXT),
            ("text_2", t.text_2, palette::TEXT_2),
            ("subtle", t.subtle, palette::SUBTLE),
            ("faint", t.faint, palette::FAINT),
            ("accent", t.accent, palette::ACCENT),
            ("on_accent", t.on_accent, palette::ON_ACCENT),
            ("primary", t.primary, palette::PRIMARY),
            ("primary_hover", t.primary_hover, palette::PRIMARY_HOVER),
            (
                "primary_pressed",
                t.primary_pressed,
                palette::PRIMARY_PRESSED,
            ),
            ("selection", t.selection, palette::SELECTION),
            ("scrim", t.scrim, palette::SCRIM),
            ("dialog_shadow", t.dialog_shadow, palette::DIALOG_SHADOW),
            ("toast_shadow", t.toast_shadow, palette::TOAST_SHADOW),
            ("working", t.working, palette::WORKING),
            ("asking", t.asking, palette::ASKING),
            ("on_asking", t.on_asking, palette::ON_ASKING),
            ("waiting", t.waiting, palette::WAITING),
            ("danger", t.danger, palette::DANGER),
            ("danger_text", t.danger_text, palette::DANGER_TEXT),
            ("lilac", t.lilac, palette::LILAC),
            ("ochre", t.ochre, palette::OCHRE),
            ("rose", t.rose, palette::ROSE),
            ("periwinkle", t.periwinkle, palette::PERIWINKLE),
            ("change_deleted", t.change_deleted, palette::CHANGE_DELETED),
            ("diff_delete", t.diff_delete, palette::DIFF_DELETE),
            ("diff_filler", t.diff_filler, palette::DIFF_FILLER),
            ("panel_bg", t.panel_bg, palette::PANEL_BG),
            ("bar_bg", t.bar_bg, palette::BAR_BG),
            ("hover_bg", t.hover_bg, palette::HOVER_BG),
            ("border", t.border, palette::BORDER),
            ("muted", t.muted, palette::MUTED),
            ("warning", t.warning, palette::WARNING),
            ("selected_bg", t.selected_bg, palette::SELECTED_BG),
            ("input_text", t.input_text, palette::INPUT_TEXT),
            (
                "input_placeholder",
                t.input_placeholder,
                palette::INPUT_PLACEHOLDER,
            ),
            ("input_cursor", t.input_cursor, palette::INPUT_CURSOR),
            (
                "input_selection",
                t.input_selection,
                palette::INPUT_SELECTION,
            ),
            ("ask_wash", t.ask_wash, palette::ASK_WASH),
            ("ask_border", t.ask_border, palette::ASK_BORDER),
            ("danger_wash", t.danger_wash, palette::DANGER_WASH),
            ("context_hot", t.context_hot, palette::CONTEXT_HOT),
            ("danger_bg", t.danger_bg, palette::DANGER_BG),
            ("status_ok", t.status_ok, palette::STATUS_OK),
            ("ok_wash", t.ok_wash, palette::OK_WASH),
            ("status_idle", t.status_idle, palette::STATUS_IDLE),
            ("status_err", t.status_err, palette::STATUS_ERR),
            ("spawn_badge_ok", t.spawn_badge_ok, palette::SPAWN_BADGE_OK),
            (
                "shell_mark_border",
                t.shell_mark_border,
                palette::SHELL_MARK_BORDER,
            ),
            ("transparent", t.transparent, palette::TRANSPARENT),
        ]
    }

    /// The built-in theme's terminal part is the palette `build_theme` makes
    /// for the default ground, so a terminal with no override looks as before.
    #[test]
    fn contrast_terminal_matches_todays_build_theme() {
        use crate::theme::{DEFAULT_BACKGROUND, build_theme};
        assert_eq!(UiTheme::CONTRAST.terminal, build_theme(DEFAULT_BACKGROUND));
    }

    /// The built-in theme is the palette: every field equals its token, so
    /// reading the theme instead of a const changes no colour.
    #[test]
    fn contrast_matches_the_palette() {
        let t = UiTheme::CONTRAST;
        for (name, field, token) in colour_pairs(&t) {
            assert_eq!(field, token, "{name}");
        }
        assert_eq!(
            crate::appearance::packed(t.terminal.bg),
            palette::TERMINAL_GROUND,
            "the terminal's background is the terminal ground"
        );
        let shares = [
            (
                "diff_delete_wash_alpha",
                t.diff_delete_wash_alpha,
                palette::DIFF_DELETE_WASH_ALPHA,
            ),
            (
                "diff_insert_wash_alpha",
                t.diff_insert_wash_alpha,
                palette::DIFF_INSERT_WASH_ALPHA,
            ),
            (
                "diff_filler_alpha",
                t.diff_filler_alpha,
                palette::DIFF_FILLER_ALPHA,
            ),
            (
                "diff_word_wash_alpha",
                t.diff_word_wash_alpha,
                palette::DIFF_WORD_WASH_ALPHA,
            ),
        ];
        for (name, field, token) in shares {
            assert_eq!(field.to_bits(), token.to_bits(), "{name}");
        }
    }
}
