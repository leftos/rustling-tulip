//! The colours the interface draws in, read at run time.
//!
//! A [`UiTheme`] holds one field per [`crate::palette`] token, aliases and
//! derived tokens included. The window's theme is an app global read with
//! [`theme`]; until one is set it is [`UiTheme::CONTRAST`], the built-in
//! look whose values are the palette's. A function with no `cx` takes the
//! theme as `&UiTheme`. The terminal's colours are not here: they are
//! `theme.rs`'s.

use gpui::{App, Global};

use crate::palette;

/// Every UI colour of one look. Each field is the token of the same name
/// in [`crate::palette`]: a `u32` as `0xRRGGBB` or `0xRRGGBBAA` as that
/// token is, or an `f32` share for the diff washes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UiTheme {
    /// [`palette::GROUND`].
    pub ground: u32,
    /// [`palette::SUNKEN`].
    pub sunken: u32,
    /// [`palette::SURFACE`].
    pub surface: u32,
    /// [`palette::RAISED`].
    pub raised: u32,
    /// [`palette::CHIP`].
    pub chip: u32,
    /// [`palette::HOVER`].
    pub hover: u32,
    /// [`palette::PRESSED`].
    pub pressed: u32,
    /// [`palette::POPOVER_HOVER`].
    pub popover_hover: u32,
    /// [`palette::POPOVER_PRESSED`].
    pub popover_pressed: u32,
    /// [`palette::LAUNCHED_TINT`].
    pub launched_tint: u32,
    /// [`palette::TERMINAL_GROUND`].
    pub terminal_ground: u32,
    /// [`palette::ROW_CARD`].
    pub row_card: u32,
    /// [`palette::ROW_HOVER`].
    pub row_hover: u32,
    /// [`palette::POPOVER`].
    pub popover: u32,
    /// [`palette::POPOVER_EDGE`].
    pub popover_edge: u32,
    /// [`palette::POPOVER_SHADOW`].
    pub popover_shadow: u32,
    /// [`palette::MODAL`].
    pub modal: u32,
    /// [`palette::MODAL_EDGE`].
    pub modal_edge: u32,
    /// [`palette::MODAL_INSET`].
    pub modal_inset: u32,
    /// [`palette::MENU_RULE`].
    pub menu_rule: u32,
    /// [`palette::CARD_EDGE`].
    pub card_edge: u32,
    /// [`palette::LINE`].
    pub line: u32,
    /// [`palette::LINE_STRONG`].
    pub line_strong: u32,
    /// [`palette::TEXT`].
    pub text: u32,
    /// [`palette::TEXT_2`].
    pub text_2: u32,
    /// [`palette::SUBTLE`].
    pub subtle: u32,
    /// [`palette::FAINT`].
    pub faint: u32,
    /// [`palette::ACCENT`].
    pub accent: u32,
    /// [`palette::ON_ACCENT`].
    pub on_accent: u32,
    /// [`palette::PRIMARY`].
    pub primary: u32,
    /// [`palette::PRIMARY_HOVER`].
    pub primary_hover: u32,
    /// [`palette::PRIMARY_PRESSED`].
    pub primary_pressed: u32,
    /// [`palette::SELECTION`].
    pub selection: u32,
    /// [`palette::SCRIM`].
    pub scrim: u32,
    /// [`palette::DIALOG_SHADOW`].
    pub dialog_shadow: u32,
    /// [`palette::TOAST_SHADOW`].
    pub toast_shadow: u32,
    /// [`palette::WORKING`].
    pub working: u32,
    /// [`palette::ASKING`].
    pub asking: u32,
    /// [`palette::ON_ASKING`].
    pub on_asking: u32,
    /// [`palette::WAITING`].
    pub waiting: u32,
    /// [`palette::DANGER`].
    pub danger: u32,
    /// [`palette::DANGER_TEXT`].
    pub danger_text: u32,
    /// [`palette::LILAC`].
    pub lilac: u32,
    /// [`palette::OCHRE`].
    pub ochre: u32,
    /// [`palette::ROSE`].
    pub rose: u32,
    /// [`palette::PERIWINKLE`].
    pub periwinkle: u32,
    /// [`palette::CHANGE_DELETED`].
    pub change_deleted: u32,
    /// [`palette::DIFF_DELETE`].
    pub diff_delete: u32,
    /// [`palette::DIFF_FILLER`].
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
    pub panel_bg: u32,
    /// [`palette::BAR_BG`].
    pub bar_bg: u32,
    /// `palette::HOVER_BG`.
    pub hover_bg: u32,
    /// [`palette::BORDER`].
    pub border: u32,
    /// `palette::MUTED`.
    pub muted: u32,
    /// `palette::WARNING`.
    pub warning: u32,
    /// `palette::SELECTED_BG`.
    pub selected_bg: u32,
    /// `palette::INPUT_TEXT`.
    pub input_text: u32,
    /// `palette::INPUT_PLACEHOLDER`.
    pub input_placeholder: u32,
    /// `palette::INPUT_CURSOR`.
    pub input_cursor: u32,
    /// `palette::INPUT_SELECTION`.
    pub input_selection: u32,
    /// `palette::ASK_WASH`.
    pub ask_wash: u32,
    /// `palette::ASK_BORDER`.
    pub ask_border: u32,
    /// `palette::DANGER_WASH`.
    pub danger_wash: u32,
    /// `palette::CONTEXT_HOT`.
    pub context_hot: u32,
    /// `palette::DANGER_BG`.
    pub danger_bg: u32,
    /// `palette::STATUS_OK`.
    pub status_ok: u32,
    /// `palette::OK_WASH`.
    pub ok_wash: u32,
    /// `palette::STATUS_IDLE`.
    pub status_idle: u32,
    /// `palette::STATUS_ERR`.
    pub status_err: u32,
    /// `palette::SPAWN_BADGE_OK`.
    pub spawn_badge_ok: u32,
    /// `palette::SHELL_MARK_BORDER`.
    pub shell_mark_border: u32,
    /// `palette::TRANSPARENT`.
    pub transparent: u32,
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
        terminal_ground: palette::TERMINAL_GROUND,
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
            (
                "terminal_ground",
                t.terminal_ground,
                palette::TERMINAL_GROUND,
            ),
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

    /// The built-in theme is the palette: every field equals its token, so
    /// reading the theme instead of a const changes no colour.
    #[test]
    fn contrast_matches_the_palette() {
        let t = UiTheme::CONTRAST;
        for (name, field, token) in colour_pairs(&t) {
            assert_eq!(field, token, "{name}");
        }
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
