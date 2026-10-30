//! Every UI colour of the native client, defined once.
//!
//! A view names a token here, never a literal, so recolouring the client is a
//! change to this one file. A value is a `u32` the view hands to `gpui::rgb`
//! (RGB, `0xRRGGBB`) or `gpui::rgba` (RGBA, `0xRRGGBBAA`); the leading byte of
//! an RGB value is padding those conversions ignore.
//!
//! The tokens come first, grouped as grounds, lines, text, the accent, the
//! status colours and the diff washes. Below them are the names the views
//! import, each an alias of one of the tokens; `lib.rs` re-exports the ones
//! that come from the crate root.

/// The background of the main area, as `0xRRGGBB`.
pub const GROUND: u32 = 0x0008_070a;
/// The background of the rail and the footer, as `0xRRGGBB`.
pub const SUNKEN: u32 = 0x0005_0407;
/// The background of the sidebar, the tab bar and a dialog, as `0xRRGGBB`.
pub const SURFACE: u32 = 0x000d_0c10;
/// The background of a focused pane header and of the sidebar's segmented
/// controls, as `0xRRGGBB`.
pub const RAISED: u32 = 0x0016_151a;
/// The fill of a chip, as `0xRRGGBB`.
pub const CHIP: u32 = 0x001e_1c23;
/// The fill of a hovered row or button and of a chosen segment, as
/// `0xRRGGBB`.
pub const HOVER: u32 = 0x004a_4756;
/// The fill of a sidebar leaf a preset launch's Select marked: the accent
/// washed into the ground, as `0xRRGGBB`.
pub const LAUNCHED_TINT: u32 = 0x0030_2122;
/// The built-in terminal ground, as `0xRRGGBB`.
pub const TERMINAL_GROUND: u32 = 0x0007_0609;

/// The card a sidebar leaf that is neither selected nor marked sits on, as
/// `0xRRGGBBAA`.
pub const ROW_CARD: u32 = 0x1615_1bff;
/// The fill of a hovered sidebar leaf, as `0xRRGGBB`.
pub const ROW_HOVER: u32 = 0x001e_1d24;
/// The space above and below each sidebar leaf, in px.
pub const ROW_GAP: f32 = 2.0;

/// The fill of a menu, a popover and a toast, as `0xRRGGBB`.
pub const POPOVER: u32 = 0x003d_3a47;
/// A menu's, a popover's and a toast's edge, as `0xRRGGBB`.
pub const POPOVER_EDGE: u32 = 0x0050_4d5b;
/// The shadow a menu and a popover cast, as `0xRRGGBBAA`.
pub const POPOVER_SHADOW: u32 = 0x0000_00b3;
/// The fill of a modal's card, as `0xRRGGBB`.
pub const MODAL: u32 = 0x002a_2832;
/// A modal card's edge, as `0xRRGGBB`.
pub const MODAL_EDGE: u32 = 0x0046_4350;
/// The fill of a band or a well inside a modal: a dialog's footer, the
/// settings tab list, a segmented control; a step lighter than [`MODAL`], as
/// `0xRRGGBB`.
pub const MODAL_INSET: u32 = 0x0030_2e38;
/// The line between groups of menu rows, as `0xRRGGBB`.
pub const MENU_RULE: u32 = 0x005e_5a6b;
/// The edge of a card that sits on a panel: a host row's tile, a settings
/// tab list, as `0xRRGGBB`.
pub const CARD_EDGE: u32 = 0x002e_2c35;

/// A divider, and a panel's or control's edge, as `0xRRGGBB`.
pub const LINE: u32 = 0x0024_222a;
/// An outlined button's or a dialog's border, as `0xRRGGBB`.
pub const LINE_STRONG: u32 = 0x0045_4250;

/// Ordinary text, as `0xRRGGBB`.
pub const TEXT: u32 = 0x00ed_e9e4;
/// Text that stands back: a label, a hint, a secondary value, and quiet text
/// on a hovered row, as `0xRRGGBB`.
pub const TEXT_2: u32 = 0x00bd_b8c4;
/// The quietest text: a count, a stamp, a leaf's subline, as `0xRRGGBB`;
/// never on [`HOVER`], where it falls below 4.5:1.
pub const SUBTLE: u32 = 0x00ae_aab8;
/// The fill of an idle or spawning glyph, never of text, as `0xRRGGBB`.
pub const FAINT: u32 = 0x006e_6a77;

/// The client's accent, as `0xRRGGBB`.
pub const ACCENT: u32 = 0x00f0_7a62;
/// Text or a glyph drawn on the accent or on a primary button, as
/// `0xRRGGBB`.
pub const ON_ACCENT: u32 = 0x001c_100d;
/// A primary button's fill: a light near-white, which leaves the accent to
/// focus and selection, as `0xRRGGBB`.
pub const PRIMARY: u32 = 0x00e4_e0da;
/// A hovered primary button's fill, a step lighter, as `0xRRGGBB`.
pub const PRIMARY_HOVER: u32 = 0x00f4_f1ec;
/// A pressed primary button's fill, a step darker, as `0xRRGGBB`.
pub const PRIMARY_PRESSED: u32 = 0x00c9_c5bf;

/// The fill of the selected sidebar leaf, settings tab and host row: the
/// accent washed into the sidebar, as `0xRRGGBB`.
pub const SELECTION: u32 = 0x0024_1718;

/// The dim layer behind a dialog and over a stopped pane, as `0xRRGGBBAA`.
pub const SCRIM: u32 = 0x0000_00a6;
/// The shadow a dialog's card casts, as `0xRRGGBBAA`.
pub const DIALOG_SHADOW: u32 = 0x0000_00cc;
/// The shadow a toast casts, as `0xRRGGBBAA`.
pub const TOAST_SHADOW: u32 = 0x0000_0073;

/// A working session's glyph, as `0xRRGGBB`.
pub const WORKING: u32 = 0x006c_a6ff;
/// A session waiting on the user's answer, as `0xRRGGBB`.
pub const ASKING: u32 = 0x00f6_bc4e;
/// A glyph or text drawn on [`ASKING`], as `0xRRGGBB`.
pub const ON_ASKING: u32 = 0x002a_1b00;
/// An unseen finished turn, as `0xRRGGBB`.
pub const WAITING: u32 = 0x004f_c89d;
/// Something that failed, as `0xRRGGBB`.
pub const DANGER: u32 = 0x00f2_6d6d;
/// A workspace's sidebar tag, a renamed file's letter and a diff's strings,
/// as `0xRRGGBB`.
pub const LILAC: u32 = 0x00c9_b8ff;
/// A modified file's letter, a mixed hunk's ruler marker, and a diff's
/// types and numbers, as `0xRRGGBB`.
pub const OCHRE: u32 = 0x00d9_a15c;
/// A diff's keywords, as `0xRRGGBB`.
pub const ROSE: u32 = 0x00e7_849b;
/// A diff's function names, as `0xRRGGBB`.
pub const PERIWINKLE: u32 = 0x008f_b3f0;
/// A deleted file's letter in the source-control tree, as `0xRRGGBB`.
pub const CHANGE_DELETED: u32 = 0x00e5_8a8a;

/// A deleted diff line's wash, as `0xRRGGBB`, at
/// [`DIFF_DELETE_WASH_ALPHA`].
pub const DIFF_DELETE: u32 = 0x00e5_6a6a;
/// The half of a diff row whose side has no line, as `0xRRGGBB`, at
/// [`DIFF_FILLER_ALPHA`].
pub const DIFF_FILLER: u32 = 0x00ff_ffff;
/// The share of a deleted diff line's wash the fill covers.
pub const DIFF_DELETE_WASH_ALPHA: f32 = 0.13;
/// The share of an inserted diff line's wash the fill covers; the wash's
/// colour is [`WAITING`].
pub const DIFF_INSERT_WASH_ALPHA: f32 = 0.12;
/// The share of [`DIFF_FILLER`]'s white the fill covers.
pub const DIFF_FILLER_ALPHA: f32 = 0.025;
/// The share of its line's hue a changed word's wash covers: the delete
/// colour on the old side, [`WAITING`] on the new.
pub const DIFF_WORD_WASH_ALPHA: f32 = 0.30;

/// The sidebar's and panels' background, which is [`SURFACE`], as
/// `0xRRGGBB`.
pub const PANEL_BG: u32 = SURFACE;
/// The pane header's background, which is
/// [`SUNKEN`], as `0xRRGGBB`.
pub const BAR_BG: u32 = SUNKEN;
/// A hovered row's or button's fill, which is [`HOVER`], as `0xRRGGBB`.
pub(crate) const HOVER_BG: u32 = HOVER;
/// A divider, and a panel's or control's edge, which is [`LINE`], as
/// `0xRRGGBB`.
pub const BORDER: u32 = LINE;
/// Text that stands back, which is [`TEXT_2`], as `0xRRGGBB`.
pub(crate) const MUTED: u32 = TEXT_2;
/// Amber, for something the user should look at that is not a failure, which
/// is [`ASKING`], as `0xRRGGBB`.
pub(crate) const WARNING: u32 = ASKING;
/// The fill of a selected sidebar row, layout row or chooser button, which is
/// [`SELECTION`], as `0xRRGGBB`.
pub(crate) const SELECTED_BG: u32 = SELECTION;
/// A text input's text, which is [`TEXT`], as `0xRRGGBB`.
pub(crate) const INPUT_TEXT: u32 = TEXT;
/// A text input's placeholder, which is [`SUBTLE`], as `0xRRGGBB`.
pub(crate) const INPUT_PLACEHOLDER: u32 = SUBTLE;
/// A text input's caret, which is [`ACCENT`], as `0xRRGGBB`.
pub(crate) const INPUT_CURSOR: u32 = ACCENT;
/// A text input's selection, which is [`ACCENT`] at 30% alpha, as
/// `0xRRGGBBAA`.
pub(crate) const INPUT_SELECTION: u32 = (ACCENT << 8) | 0x4d;

/// A Dashboard row waiting on the user: [`ASKING`] at 7% alpha, as
/// `0xRRGGBBAA`.
pub(crate) const ASK_WASH: u32 = (ASKING << 8) | 0x12;
/// A Dashboard queue chip's border: [`ASKING`] at 35% alpha, as
/// `0xRRGGBBAA`.
pub(crate) const ASK_BORDER: u32 = (ASKING << 8) | 0x59;
/// A Dashboard stuck flag's fill: [`DANGER`] at 10% alpha, as `0xRRGGBBAA`.
pub(crate) const DANGER_WASH: u32 = (DANGER << 8) | 0x1a;
/// A Dashboard context figure past 80%, which is [`DANGER`], as `0xRRGGBB`.
pub(crate) const CONTEXT_HOT: u32 = DANGER;

/// The fill behind an armed destructive control, as `0xRRGGBB`.
pub(crate) const DANGER_BG: u32 = 0x003a_1c1f;

/// The footer's dot for a healthy daemon, as `0xRRGGBB`.
pub(crate) const STATUS_OK: u32 = 0x003f_b96a;
/// The disc behind a passed check mark: [`STATUS_OK`] at 16% alpha, as
/// `0xRRGGBBAA`.
pub(crate) const OK_WASH: u32 = (STATUS_OK << 8) | 0x29;
/// The footer's dot for an idle or stopped daemon, as `0xRRGGBB`.
pub(crate) const STATUS_IDLE: u32 = 0x0083_8a96;
/// The footer's dot for a daemon in error, as `0xRRGGBB`.
pub(crate) const STATUS_ERR: u32 = 0x00ef_5c5c;

/// An "ok" badge of the spawn dialog's workspace preview table, as `0xRRGGBB`.
pub(crate) const SPAWN_BADGE_OK: u32 = 0x004e_c9b0;

/// The border of a pane's shell-mark dot, as `0xRRGGBBAA`.
pub(crate) const SHELL_MARK_BORDER: u32 = 0x0000_0059;

/// An invisible border, for a control that keeps its size while unfocused, as
/// `0xRRGGBBAA`.
pub(crate) const TRANSPARENT: u32 = 0x0000_0000;

#[cfg(test)]
mod tests {
    use super::*;

    /// The relative luminance of a `0xRRGGBB` colour: 0.0 for black, 1.0 for
    /// white, as WCAG defines it.
    fn luminance(color: u32) -> f64 {
        let [_, r, g, b] = color.to_be_bytes();
        let channel = |c: u8| {
            let value = f64::from(c) / 255.0;
            if value <= 0.039_28 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    }

    /// The WCAG contrast ratio between two `0xRRGGBB` colours: 1.0 for
    /// identical ones, 21.0 for black against white.
    fn contrast(a: u32, b: u32) -> f64 {
        let (left, right) = (luminance(a), luminance(b));
        (left.max(right) + 0.05) / (left.min(right) + 0.05)
    }

    /// Every token holds the value the palette gives it. These are the only
    /// record of the colours nothing else pins, so a recolour edits both the
    /// token and this test.
    #[test]
    fn every_token_holds_its_value() {
        assert_eq!(GROUND, 0x0008_070a);
        assert_eq!(SUNKEN, 0x0005_0407);
        assert_eq!(SURFACE, 0x000d_0c10);
        assert_eq!(RAISED, 0x0016_151a);
        assert_eq!(CHIP, 0x001e_1c23);
        assert_eq!(HOVER, 0x004a_4756);
        assert_eq!(LAUNCHED_TINT, 0x0030_2122);
        assert_eq!(TERMINAL_GROUND, 0x0007_0609);
        assert_eq!(ROW_CARD, 0x1615_1bff);
        assert_eq!(ROW_HOVER, 0x001e_1d24);
        assert!((ROW_GAP - 2.0).abs() < f32::EPSILON);
        assert_eq!(POPOVER, 0x003d_3a47);
        assert_eq!(POPOVER_EDGE, 0x0050_4d5b);
        assert_eq!(POPOVER_SHADOW, 0x0000_00b3);
        assert_eq!(MODAL, 0x002a_2832);
        assert_eq!(MODAL_EDGE, 0x0046_4350);
        assert_eq!(MODAL_INSET, 0x0030_2e38);
        assert_eq!(MENU_RULE, 0x005e_5a6b);
        assert_eq!(CARD_EDGE, 0x002e_2c35);
        assert_eq!(LINE, 0x0024_222a);
        assert_eq!(LINE_STRONG, 0x0045_4250);
        assert_eq!(TEXT, 0x00ed_e9e4);
        assert_eq!(TEXT_2, 0x00bd_b8c4);
        assert_eq!(SUBTLE, 0x00ae_aab8);
        assert_eq!(FAINT, 0x006e_6a77);
        assert_eq!(ACCENT, 0x00f0_7a62);
        assert_eq!(ON_ACCENT, 0x001c_100d);
        assert_eq!(PRIMARY, 0x00e4_e0da);
        assert_eq!(PRIMARY_HOVER, 0x00f4_f1ec);
        assert_eq!(PRIMARY_PRESSED, 0x00c9_c5bf);
        assert_eq!(SELECTION, 0x0024_1718);
        assert_eq!(SCRIM, 0x0000_00a6);
        assert_eq!(DIALOG_SHADOW, 0x0000_00cc);
        assert_eq!(TOAST_SHADOW, 0x0000_0073);
        assert_eq!(WORKING, 0x006c_a6ff);
        assert_eq!(ASKING, 0x00f6_bc4e);
        assert_eq!(ON_ASKING, 0x002a_1b00);
        assert_eq!(WAITING, 0x004f_c89d);
        assert_eq!(DANGER, 0x00f2_6d6d);
        assert_eq!(LILAC, 0x00c9_b8ff);
        assert_eq!(OCHRE, 0x00d9_a15c);
        assert_eq!(ROSE, 0x00e7_849b);
        assert_eq!(PERIWINKLE, 0x008f_b3f0);
        assert_eq!(CHANGE_DELETED, 0x00e5_8a8a);
        assert_eq!(DIFF_DELETE, 0x00e5_6a6a);
        assert_eq!(DIFF_FILLER, 0x00ff_ffff);
        assert!((DIFF_DELETE_WASH_ALPHA - 0.13).abs() < f32::EPSILON);
        assert!((DIFF_INSERT_WASH_ALPHA - 0.12).abs() < f32::EPSILON);
        assert!((DIFF_FILLER_ALPHA - 0.025).abs() < f32::EPSILON);
        assert!((DIFF_WORD_WASH_ALPHA - 0.30).abs() < f32::EPSILON);
        assert_eq!(DANGER_BG, 0x003a_1c1f);
        assert_eq!(STATUS_OK, 0x003f_b96a);
        assert_eq!(STATUS_IDLE, 0x0083_8a96);
        assert_eq!(STATUS_ERR, 0x00ef_5c5c);
        assert_eq!(SPAWN_BADGE_OK, 0x004e_c9b0);
        assert_eq!(INPUT_SELECTION, 0xf07a_624d);
        assert_eq!(SHELL_MARK_BORDER, 0x0000_0059);
        assert_eq!(TRANSPARENT, 0x0000_0000);
    }

    /// Every name a view imports is one of the tokens.
    #[test]
    fn the_imported_names_alias_the_tokens() {
        assert_eq!(PANEL_BG, SURFACE);
        assert_eq!(BAR_BG, SUNKEN);
        assert_eq!(HOVER_BG, HOVER);
        assert_eq!(BORDER, LINE);
        assert_eq!(MUTED, TEXT_2);
        assert_eq!(WARNING, ASKING);
        assert_eq!(SELECTED_BG, SELECTION);
        assert_eq!(INPUT_TEXT, TEXT);
        assert_eq!(INPUT_PLACEHOLDER, SUBTLE);
        assert_eq!(INPUT_CURSOR, ACCENT);
    }

    /// The scrim is black at about two thirds: `rgba(0, 0, 0, 0.65)`.
    #[test]
    fn scrim_is_black_at_65_percent() {
        let [r, g, b, a] = SCRIM.to_be_bytes();
        assert_eq!((r, g, b), (0x00, 0x00, 0x00));
        let alpha = f64::from(a) / 255.0;
        assert!((alpha - 0.65).abs() < 0.5 / 255.0, "alpha is {alpha:.4}");
    }

    /// A text input's selection is the accent at 30% alpha.
    #[test]
    fn input_selection_is_the_accent_at_30_percent() {
        assert_eq!(INPUT_SELECTION >> 8, ACCENT);
        let alpha = f64::from(INPUT_SELECTION & 0xff) / 255.0;
        assert!((alpha - 0.30).abs() < 0.5 / 255.0, "alpha is {alpha:.4}");
    }

    /// The Dashboard's washes are their hues at the mock's alphas: the
    /// asking wash at 7%, the chip border at 35%, the stuck flag at 10%, and
    /// the hot context is the danger colour itself.
    #[test]
    fn dashboard_washes_are_their_hues_at_their_alphas() {
        for (name, token, hue, share) in [
            ("ASK_WASH", ASK_WASH, ASKING, 0.07),
            ("ASK_BORDER", ASK_BORDER, ASKING, 0.35),
            ("DANGER_WASH", DANGER_WASH, DANGER, 0.10),
            ("OK_WASH", OK_WASH, STATUS_OK, 0.16),
        ] {
            assert_eq!(token >> 8, hue, "{name}'s channels");
            let alpha = f64::from(token & 0xff) / 255.0;
            assert!(
                (alpha - share).abs() < 1.0 / 255.0,
                "{name}'s alpha is {alpha:.4}"
            );
        }
        assert_eq!(ASK_WASH, 0xf6bc_4e12);
        assert_eq!(ASK_BORDER, 0xf6bc_4e59);
        assert_eq!(DANGER_WASH, 0xf26d_6d1a);
        assert_eq!(OK_WASH, 0x3fb9_6a29);
        assert_eq!(CONTEXT_HOT, DANGER);
    }

    /// The floors every token pair the client draws must keep: ordinary text
    /// at 7:1 on the ground, body, secondary and quiet text at 4.5:1 on every
    /// surface they sit on, and the dark text on the fills that carry it. A
    /// hovered row lifts its quiet text to [`TEXT_2`], so [`SUBTLE`] is never
    /// drawn on [`HOVER`] and has no pair there.
    #[test]
    fn the_token_pairs_keep_their_contrast_floors() {
        let surfaces = [
            ("GROUND", GROUND),
            ("SUNKEN", SUNKEN),
            ("SURFACE", SURFACE),
            ("RAISED", RAISED),
            ("CHIP", CHIP),
            ("TERMINAL_GROUND", TERMINAL_GROUND),
            ("ROW_CARD", ROW_CARD >> 8),
            ("ROW_HOVER", ROW_HOVER),
            ("POPOVER", POPOVER),
            ("MODAL", MODAL),
            ("MODAL_INSET", MODAL_INSET),
            ("SELECTION", SELECTION),
        ];
        let texts = [("TEXT", TEXT), ("TEXT_2", TEXT_2), ("SUBTLE", SUBTLE)];
        let mut pairs: Vec<(String, u32, u32, f64)> = Vec::new();
        for (under_name, under) in surfaces {
            for (on_name, on) in texts {
                pairs.push((format!("{on_name} on {under_name}"), on, under, 4.5));
            }
        }
        for (pair, on, under, floor) in [
            ("TEXT on GROUND", TEXT, GROUND, 7.0),
            ("TEXT on HOVER", TEXT, HOVER, 4.5),
            ("TEXT_2 on HOVER", TEXT_2, HOVER, 4.5),
            ("ON_ACCENT on ACCENT", ON_ACCENT, ACCENT, 4.5),
            ("ON_ACCENT on PRIMARY", ON_ACCENT, PRIMARY, 4.5),
            ("ON_ACCENT on PRIMARY_HOVER", ON_ACCENT, PRIMARY_HOVER, 4.5),
            (
                "ON_ACCENT on PRIMARY_PRESSED",
                ON_ACCENT,
                PRIMARY_PRESSED,
                4.5,
            ),
            ("ON_ASKING on ASKING", ON_ASKING, ASKING, 4.5),
        ] {
            pairs.push((pair.to_owned(), on, under, floor));
        }
        for (pair, on, under, floor) in pairs {
            let ratio = contrast(on, under);
            assert!(ratio >= floor, "{pair} is {ratio:.2}:1, below {floor}:1");
        }
    }
}
