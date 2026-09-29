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
pub const GROUND: u32 = 0x0011_1013;
/// The background of the rail and the footer, as `0xRRGGBB`.
pub const SUNKEN: u32 = 0x000b_0a0d;
/// The background of the sidebar, the tab bar and a dialog, as `0xRRGGBB`.
pub const SURFACE: u32 = 0x0016_1519;
/// The background of a focused pane header and a segmented control, as
/// `0xRRGGBB`.
pub const RAISED: u32 = 0x001b_1a1f;
/// The fill of a selected row, of a chip and of a popover, as `0xRRGGBB`.
pub const CHIP: u32 = 0x0023_2129;
/// The fill of a hovered row or button, as `0xRRGGBB`.
pub const HOVER: u32 = 0x002c_2a33;
/// The built-in terminal ground, as `0xRRGGBB`.
pub const TERMINAL_GROUND: u32 = 0x000c_0b0e;

/// A divider, and a panel's or control's edge, as `0xRRGGBB`.
pub const LINE: u32 = 0x002a_2830;
/// An outlined button's or a dialog's border, as `0xRRGGBB`.
pub const LINE_STRONG: u32 = 0x003a_3742;

/// Ordinary text, as `0xRRGGBB`.
pub const TEXT: u32 = 0x00ed_e9e4;
/// Text that stands back: a label, a hint, a secondary value, as `0xRRGGBB`.
pub const TEXT_2: u32 = 0x00a7_a2ad;
/// The quietest text: a count, a stamp, a leaf's subline, as `0xRRGGBB`.
pub const SUBTLE: u32 = 0x008c_8794;
/// The fill of an idle or spawning glyph, never of text, as `0xRRGGBB`.
pub const FAINT: u32 = 0x006e_6a77;

/// The client's accent, as `0xRRGGBB`.
pub const ACCENT: u32 = 0x00f0_7a62;
/// Text or a glyph drawn on the accent, as `0xRRGGBB`.
pub const ON_ACCENT: u32 = 0x001c_100d;
/// A hovered primary button's fill: the accent 12% of the way to [`TEXT`],
/// as `0xRRGGBB`.
pub const ACCENT_HOVER: u32 = 0x00f0_8772;
/// A pressed primary button's fill: the accent 12% of the way to
/// [`ON_ACCENT`], as `0xRRGGBB`.
pub const ACCENT_PRESSED: u32 = 0x00d7_6d58;

/// The dim layer behind a dialog and over a stopped pane, as `0xRRGGBBAA`.
pub const SCRIM: u32 = 0x0605_08a8;
/// The shadow a dialog's card casts, as `0xRRGGBBAA`.
pub const DIALOG_SHADOW: u32 = 0x0000_0099;
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
/// The Codex runtime tag, as `0xRRGGBB`.
pub const LILAC: u32 = 0x00c9_b8ff;

/// A deleted diff line's wash, as `0xRRGGBB`, at
/// [`DIFF_DELETE_WASH_ALPHA`].
pub const DIFF_DELETE: u32 = 0x00e5_6a6a;
/// The text of a removed diff line, as `0xRRGGBB`.
pub const DIFF_REMOVED_TEXT: u32 = 0x00e5_8a8a;
/// The text of an added diff line, as `0xRRGGBB`.
pub const DIFF_ADDED_TEXT: u32 = 0x007f_d3ae;
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

/// The sidebar's and panels' background, which is [`SURFACE`], as
/// `0xRRGGBB`.
pub const PANEL_BG: u32 = SURFACE;
/// The footer's and the tab bar's background, which is [`SUNKEN`], as
/// `0xRRGGBB`.
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
/// [`CHIP`], as `0xRRGGBB`.
pub(crate) const SELECTED_BG: u32 = CHIP;
/// A text input's text, which is [`TEXT`], as `0xRRGGBB`.
pub(crate) const INPUT_TEXT: u32 = TEXT;
/// A text input's placeholder, which is [`SUBTLE`], as `0xRRGGBB`.
pub(crate) const INPUT_PLACEHOLDER: u32 = SUBTLE;
/// A text input's caret, which is [`ACCENT`], as `0xRRGGBB`.
pub(crate) const INPUT_CURSOR: u32 = ACCENT;
/// A text input's selection, which is [`ACCENT`] at 30% alpha, as
/// `0xRRGGBBAA`.
pub(crate) const INPUT_SELECTION: u32 = (ACCENT << 8) | 0x4d;

/// The fill behind an armed destructive control, as `0xRRGGBB`.
pub(crate) const DANGER_BG: u32 = 0x003a_1c1f;

/// The footer's dot for a healthy daemon, as `0xRRGGBB`.
pub(crate) const STATUS_OK: u32 = 0x003f_b96a;
/// The footer's dot for an idle or stopped daemon, as `0xRRGGBB`.
pub(crate) const STATUS_IDLE: u32 = 0x0083_8a96;
/// The footer's dot for a daemon in error, as `0xRRGGBB`.
pub(crate) const STATUS_ERR: u32 = 0x00ef_5c5c;

/// An "ok" badge of the spawn dialog's workspace preview table, as `0xRRGGBB`.
pub(crate) const SPAWN_BADGE_OK: u32 = 0x004e_c9b0;

/// The rail's badge text, dark on the accent, as `0xRRGGBB`.
pub(crate) const RAIL_BADGE_TEXT: u32 = 0x000f_1014;

/// A modified file in the source-control tree, as `0xRRGGBB`.
pub(crate) const CHANGES_MODIFIED: u32 = 0x00d4_a72c;
/// An added file in the source-control tree, as `0xRRGGBB`.
pub(crate) const CHANGES_ADDED: u32 = 0x004e_c9b0;
/// A renamed file in the source-control tree, as `0xRRGGBB`.
pub(crate) const CHANGES_RENAMED: u32 = 0x0056_9cd6;
/// An untracked file in the source-control tree, as `0xRRGGBB`.
pub(crate) const CHANGES_UNTRACKED: u32 = 0x006a_9955;

/// A deleted line's half, and the old half of a modified row, as
/// `0xRRGGBBAA`.
pub(crate) const DIFF_DELETE_BG: u32 = 0xf851_4926;
/// An inserted line's half, and the new half of a modified row, as
/// `0xRRGGBBAA`.
pub(crate) const DIFF_INSERT_BG: u32 = 0x3fb9_5026;
/// A changed word on the old side, as `0xRRGGBBAA`.
pub(crate) const DIFF_DELETE_WORD_BG: u32 = 0xf851_4959;
/// A changed word on the new side, as `0xRRGGBBAA`.
pub(crate) const DIFF_INSERT_WORD_BG: u32 = 0x3fb9_5059;
/// The half of a diff row whose side has no line, as `0xRRGGBBAA`.
pub(crate) const DIFF_FILLER_BG: u32 = 0x8080_800f;
/// A diff line's text, as `0xRRGGBBAA`.
pub(crate) const DIFF_TEXT: u32 = 0xcccc_ccff;
/// A diff gutter's line number, as `0xRRGGBBAA`.
pub(crate) const DIFF_GUTTER_TEXT: u32 = 0x6e76_81ff;
/// The bar left of the current hunk's rows, as `0xRRGGBBAA`.
pub(crate) const DIFF_CURRENT_BAR: u32 = 0x4c8d_ffcc;
/// The line between a diff's two halves, as `0xRRGGBBAA`.
pub(crate) const DIFF_DIVIDER: u32 = 0x2022_2aff;

/// The border of a pane's shell-mark dot, as `0xRRGGBBAA`.
pub(crate) const SHELL_MARK_BORDER: u32 = 0x0000_0059;
/// A shell-mark dot for a command that exited 0, as `0xRRGGBB`.
pub(crate) const SHELL_MARK_OK: u32 = 0x004e_c9b0;
/// A shell-mark dot for a command that exited non-zero, as `0xRRGGBB`.
pub(crate) const SHELL_MARK_FAIL: u32 = 0x00f4_8771;

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
        assert_eq!(GROUND, 0x0011_1013);
        assert_eq!(SUNKEN, 0x000b_0a0d);
        assert_eq!(SURFACE, 0x0016_1519);
        assert_eq!(RAISED, 0x001b_1a1f);
        assert_eq!(CHIP, 0x0023_2129);
        assert_eq!(HOVER, 0x002c_2a33);
        assert_eq!(TERMINAL_GROUND, 0x000c_0b0e);
        assert_eq!(LINE, 0x002a_2830);
        assert_eq!(LINE_STRONG, 0x003a_3742);
        assert_eq!(TEXT, 0x00ed_e9e4);
        assert_eq!(TEXT_2, 0x00a7_a2ad);
        assert_eq!(SUBTLE, 0x008c_8794);
        assert_eq!(FAINT, 0x006e_6a77);
        assert_eq!(ACCENT, 0x00f0_7a62);
        assert_eq!(ON_ACCENT, 0x001c_100d);
        assert_eq!(ACCENT_HOVER, 0x00f0_8772);
        assert_eq!(ACCENT_PRESSED, 0x00d7_6d58);
        assert_eq!(SCRIM, 0x0605_08a8);
        assert_eq!(DIALOG_SHADOW, 0x0000_0099);
        assert_eq!(TOAST_SHADOW, 0x0000_0073);
        assert_eq!(WORKING, 0x006c_a6ff);
        assert_eq!(ASKING, 0x00f6_bc4e);
        assert_eq!(ON_ASKING, 0x002a_1b00);
        assert_eq!(WAITING, 0x004f_c89d);
        assert_eq!(DANGER, 0x00f2_6d6d);
        assert_eq!(LILAC, 0x00c9_b8ff);
        assert_eq!(DIFF_DELETE, 0x00e5_6a6a);
        assert_eq!(DIFF_REMOVED_TEXT, 0x00e5_8a8a);
        assert_eq!(DIFF_ADDED_TEXT, 0x007f_d3ae);
        assert_eq!(DIFF_FILLER, 0x00ff_ffff);
        assert!((DIFF_DELETE_WASH_ALPHA - 0.13).abs() < f32::EPSILON);
        assert!((DIFF_INSERT_WASH_ALPHA - 0.12).abs() < f32::EPSILON);
        assert!((DIFF_FILLER_ALPHA - 0.025).abs() < f32::EPSILON);
        assert_eq!(DANGER_BG, 0x003a_1c1f);
        assert_eq!(STATUS_OK, 0x003f_b96a);
        assert_eq!(STATUS_IDLE, 0x0083_8a96);
        assert_eq!(STATUS_ERR, 0x00ef_5c5c);
        assert_eq!(SPAWN_BADGE_OK, 0x004e_c9b0);
        assert_eq!(INPUT_SELECTION, 0xf07a_624d);
        assert_eq!(RAIL_BADGE_TEXT, 0x000f_1014);
        assert_eq!(CHANGES_MODIFIED, 0x00d4_a72c);
        assert_eq!(CHANGES_ADDED, 0x004e_c9b0);
        assert_eq!(CHANGES_RENAMED, 0x0056_9cd6);
        assert_eq!(CHANGES_UNTRACKED, 0x006a_9955);
        assert_eq!(DIFF_DELETE_BG, 0xf851_4926);
        assert_eq!(DIFF_INSERT_BG, 0x3fb9_5026);
        assert_eq!(DIFF_DELETE_WORD_BG, 0xf851_4959);
        assert_eq!(DIFF_INSERT_WORD_BG, 0x3fb9_5059);
        assert_eq!(DIFF_FILLER_BG, 0x8080_800f);
        assert_eq!(DIFF_TEXT, 0xcccc_ccff);
        assert_eq!(DIFF_GUTTER_TEXT, 0x6e76_81ff);
        assert_eq!(DIFF_CURRENT_BAR, 0x4c8d_ffcc);
        assert_eq!(DIFF_DIVIDER, 0x2022_2aff);
        assert_eq!(SHELL_MARK_BORDER, 0x0000_0059);
        assert_eq!(SHELL_MARK_OK, 0x004e_c9b0);
        assert_eq!(SHELL_MARK_FAIL, 0x00f4_8771);
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
        assert_eq!(SELECTED_BG, CHIP);
        assert_eq!(INPUT_TEXT, TEXT);
        assert_eq!(INPUT_PLACEHOLDER, SUBTLE);
        assert_eq!(INPUT_CURSOR, ACCENT);
    }

    /// The scrim is near-black at two thirds: `rgba(6, 5, 8, 0.66)`.
    #[test]
    fn scrim_is_060508_at_66_percent() {
        let [r, g, b, a] = SCRIM.to_be_bytes();
        assert_eq!((r, g, b), (0x06, 0x05, 0x08));
        let alpha = f64::from(a) / 255.0;
        assert!((alpha - 0.66).abs() < 0.5 / 255.0, "alpha is {alpha:.4}");
    }

    /// A text input's selection is the accent at 30% alpha.
    #[test]
    fn input_selection_is_the_accent_at_30_percent() {
        assert_eq!(INPUT_SELECTION >> 8, ACCENT);
        let alpha = f64::from(INPUT_SELECTION & 0xff) / 255.0;
        assert!((alpha - 0.30).abs() < 0.5 / 255.0, "alpha is {alpha:.4}");
    }

    /// The floors every token pair the client draws must keep: ordinary text
    /// on the ground, secondary and subtle text on the surfaces they sit on,
    /// and the dark text on the two fills that carry it.
    #[test]
    fn the_token_pairs_keep_their_contrast_floors() {
        let pairs = [
            ("TEXT on GROUND", TEXT, GROUND, 7.0),
            ("TEXT_2 on SURFACE", TEXT_2, SURFACE, 4.5),
            ("TEXT_2 on CHIP", TEXT_2, CHIP, 4.5),
            ("SUBTLE on SURFACE", SUBTLE, SURFACE, 4.5),
            ("SUBTLE on CHIP", SUBTLE, CHIP, 4.5),
            ("ON_ACCENT on ACCENT", ON_ACCENT, ACCENT, 4.5),
            ("ON_ACCENT on ACCENT_HOVER", ON_ACCENT, ACCENT_HOVER, 4.5),
            (
                "ON_ACCENT on ACCENT_PRESSED",
                ON_ACCENT,
                ACCENT_PRESSED,
                4.5,
            ),
            ("ON_ASKING on ASKING", ON_ASKING, ASKING, 4.5),
        ];
        for (pair, on, under, floor) in pairs {
            let ratio = contrast(on, under);
            assert!(ratio >= floor, "{pair} is {ratio:.2}:1, below {floor}:1");
        }
    }
}
