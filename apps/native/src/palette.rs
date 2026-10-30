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
//!
//! Mock scaffolding for RT-581: a token that differs between the visual
//! directions is a [`pick`] of four values, today's first, then direction A
//! (Elevation), B (Outline) and C (Contrast). The build's `RT_MOCK_THEME`
//! environment variable (`current`, `a`, `b` or `c`; unset is `current`)
//! chooses which one every token takes.

/// Which column of every [`pick`] this build draws with.
const MOCK: usize = mock_index(option_env!("RT_MOCK_THEME"));

/// The column `RT_MOCK_THEME` names: 0 for `current` or unset, 1 to 3 for
/// directions `a` to `c`. Any other value fails the build.
const fn mock_index(name: Option<&str>) -> usize {
    let Some(name) = name else {
        return 0;
    };
    match name.as_bytes() {
        b"" | b"current" => 0,
        b"a" => 1,
        b"b" => 2,
        b"c" => 3,
        _ => panic!("RT_MOCK_THEME must be current, a, b or c"),
    }
}

/// The value of a token in this build's direction: `values` holds today's
/// value, then direction A's, B's and C's.
const fn pick(values: [u32; 4]) -> u32 {
    values[MOCK]
}

/// The value of a length in this build's direction, in px: `values` holds
/// today's value, then direction A's, B's and C's.
const fn pick_px(values: [f32; 4]) -> f32 {
    values[MOCK]
}

/// The background of the main area, as `0xRRGGBB`.
pub const GROUND: u32 = pick([0x0011_1013, 0x000c_0b0f, 0x0011_1013, 0x0008_070a]);
/// The background of the rail and the footer, as `0xRRGGBB`.
pub const SUNKEN: u32 = pick([0x000b_0a0d, 0x0009_080b, 0x000b_0a0d, 0x0005_0407]);
/// The background of the sidebar, the tab bar and a dialog, as `0xRRGGBB`.
pub const SURFACE: u32 = pick([0x0016_1519, 0x0015_141a, 0x0016_1519, 0x000d_0c10]);
/// The background of a focused pane header and a segmented control, as
/// `0xRRGGBB`.
pub const RAISED: u32 = pick([0x001b_1a1f, 0x001e_1d24, 0x001b_1a1f, 0x0016_151a]);
/// The fill of a chip, as `0xRRGGBB`.
pub const CHIP: u32 = pick([0x0023_2129, 0x0026_242d, 0x0023_2129, 0x001e_1c23]);
/// The fill of a hovered row or button, as `0xRRGGBB`.
pub const HOVER: u32 = pick([0x002c_2a33, 0x004f_4b5b, 0x0046_434f, 0x004a_4756]);
/// The fill of a sidebar leaf a preset launch's Select marked: the accent
/// washed into the ground, as `0xRRGGBB`.
pub const LAUNCHED_TINT: u32 = 0x0030_2122;
/// The built-in terminal ground, as `0xRRGGBB`.
pub const TERMINAL_GROUND: u32 = pick([0x000c_0b0e, 0x000c_0b0e, 0x000c_0b0e, 0x0007_0609]);

/// The fill of a sidebar leaf that is neither selected nor marked, as
/// `0xRRGGBBAA`; none today.
pub const ROW_CARD: u32 = pick([0x0000_0000, 0x1e1d_24ff, 0x0000_0000, 0x1615_1bff]);
/// The edge of a sidebar leaf, as `0xRRGGBBAA`; none today.
pub const ROW_EDGE: u32 = pick([0x0000_0000, 0x0000_0000, 0x3f3c_4aff, 0x0000_0000]);
/// The edge of the selected sidebar leaf where leaves have an edge, as
/// `0xRRGGBBAA`.
pub const SELECTION_EDGE: u32 = pick([0x0000_0000, 0x0000_0000, 0xf07a_62ff, 0x0000_0000]);
/// The fill of a hovered sidebar leaf, as `0xRRGGBB`.
pub const ROW_HOVER: u32 = pick([0x001b_1a1f, 0x0026_242d, 0x001b_1a1f, 0x001e_1d24]);
/// The space above and below each sidebar leaf, in px.
pub const ROW_GAP: f32 = pick_px([1.0, 2.0, 2.0, 2.0]);

/// The fill of a menu and of a popover, as `0xRRGGBB`.
pub const POPOVER: u32 = pick([0x0023_2129, 0x0043_3f4e, 0x003a_3843, 0x003d_3a47]);
/// A menu's and a popover's edge, as `0xRRGGBB`.
pub const POPOVER_EDGE: u32 = pick([0x003a_3742, 0x004a_4757, 0x008e_8a9c, 0x0050_4d5b]);
/// The 1 px line along a menu's and a popover's top edge, as `0xRRGGBBAA`;
/// none outside direction A.
pub const POPOVER_HIGHLIGHT: u32 = pick([0x0000_0000, 0x5a56_67ff, 0x0000_0000, 0x0000_0000]);
/// The shadow a menu and a popover cast, as `0xRRGGBBAA`; none today.
pub const POPOVER_SHADOW: u32 = pick([0x0000_0000, 0x0000_0000, 0x0000_00d9, 0x0000_00b3]);
/// The fill of a modal's card, as `0xRRGGBB`.
pub const MODAL: u32 = pick([0x0016_1519, 0x0034_313d, 0x0016_1519, 0x002a_2832]);
/// A modal card's edge, as `0xRRGGBB`.
pub const MODAL_EDGE: u32 = pick([0x003a_3742, 0x0042_3f4c, 0x008e_8a9c, 0x0046_4350]);
/// The 1 px line along a modal card's top edge, as `0xRRGGBBAA`; none
/// outside direction A.
pub const MODAL_HIGHLIGHT: u32 = pick([0x0000_0000, 0x4a47_57ff, 0x0000_0000, 0x0000_0000]);
/// The fill of a band inside a modal: a dialog's footer, the settings tab
/// list; never darker than [`MODAL`] where the modal is lighter than the
/// sidebar, as `0xRRGGBB`.
pub const MODAL_INSET: u32 = pick([0x001b_1a1f, 0x003a_3743, 0x001b_1a1f, 0x0030_2e38]);
/// The line between groups of menu rows, as `0xRRGGBB`.
pub const MENU_RULE: u32 = pick([0x003a_3742, 0x0055_5263, 0x005a_5668, 0x005e_5a6b]);
/// Whether a menu draws a rule between every group of its rows, not only
/// before its last group.
pub const MENU_GROUP_RULES: bool = MOCK == 3;
/// The edge of a card that sits on a panel: a host row's tile, a settings
/// tab list, as `0xRRGGBB`.
pub const CARD_EDGE: u32 = pick([0x002a_2830, 0x0030_2e37, 0x003f_3c4a, 0x002e_2c35]);

/// A divider, and a panel's or control's edge, as `0xRRGGBB`.
pub const LINE: u32 = pick([0x002a_2830, 0x002a_2830, 0x0032_303a, 0x0024_222a]);
/// An outlined button's or a dialog's border, as `0xRRGGBB`.
pub const LINE_STRONG: u32 = pick([0x003a_3742, 0x0046_4350, 0x0055_5161, 0x0045_4250]);

/// Ordinary text, as `0xRRGGBB`.
pub const TEXT: u32 = 0x00ed_e9e4;
/// Text that stands back: a label, a hint, a secondary value, as `0xRRGGBB`.
pub const TEXT_2: u32 = pick([0x00a7_a2ad, 0x00bb_b6c1, 0x00b0_abb6, 0x00bd_b8c4]);
/// The quietest text: a count, a stamp, a leaf's subline, as `0xRRGGBB`.
pub const SUBTLE: u32 = pick([0x008c_8794, 0x00b0_abb9, 0x00a8_a3b1, 0x00ae_aab8]);
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
/// A primary button's fill, as `0xRRGGBB`: the accent, or in direction C,
/// which keeps the accent for focus and selection, plain light text colour.
pub const PRIMARY: u32 = pick([ACCENT, ACCENT, ACCENT, 0x00e4_e0da]);
/// A hovered primary button's fill, as `0xRRGGBB`.
pub const PRIMARY_HOVER: u32 = pick([ACCENT_HOVER, ACCENT_HOVER, ACCENT_HOVER, 0x00f4_f1ec]);
/// A pressed primary button's fill, as `0xRRGGBB`.
pub const PRIMARY_PRESSED: u32 =
    pick([ACCENT_PRESSED, ACCENT_PRESSED, ACCENT_PRESSED, 0x00c9_c5bf]);

/// The fill of the selected sidebar leaf, settings tab and host row, as
/// `0xRRGGBB`: the chip, a raised step, or the accent washed into the
/// sidebar.
pub const SELECTION: u32 = pick([0x0023_2129, 0x002b_1e21, 0x001b_1a1f, 0x0024_1718]);

/// The dim layer behind a dialog and over a stopped pane, as `0xRRGGBBAA`.
pub const SCRIM: u32 = pick([0x0605_08a8, 0x0605_08a8, 0x0000_00a6, 0x0000_00a6]);
/// The shadow a dialog's card casts, as `0xRRGGBBAA`.
pub const DIALOG_SHADOW: u32 = pick([0x0000_0099, 0x0000_0099, 0x0000_00e6, 0x0000_00cc]);
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
        assert_eq!(GROUND, 0x0011_1013);
        assert_eq!(SUNKEN, 0x000b_0a0d);
        assert_eq!(SURFACE, 0x0016_1519);
        assert_eq!(RAISED, 0x001b_1a1f);
        assert_eq!(CHIP, 0x0023_2129);
        assert_eq!(HOVER, 0x002c_2a33);
        assert_eq!(LAUNCHED_TINT, 0x0030_2122);
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

    /// The Dashboard's washes are their hues at the mock's alphas: the
    /// asking wash at 7%, the chip border at 35%, the stuck flag at 10%, and
    /// the hot context is the danger colour itself.
    #[test]
    fn dashboard_washes_are_their_hues_at_their_alphas() {
        for (name, token, hue, share) in [
            ("ASK_WASH", ASK_WASH, ASKING, 0.07),
            ("ASK_BORDER", ASK_BORDER, ASKING, 0.35),
            ("DANGER_WASH", DANGER_WASH, DANGER, 0.10),
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
        assert_eq!(CONTEXT_HOT, DANGER);
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
            ("TEXT_2 on POPOVER", TEXT_2, POPOVER, 4.5),
            ("SUBTLE on POPOVER", SUBTLE, POPOVER, 4.5),
            ("TEXT_2 on MODAL", TEXT_2, MODAL, 4.5),
            ("SUBTLE on MODAL", SUBTLE, MODAL, 4.5),
            ("TEXT_2 on SELECTION", TEXT_2, SELECTION, 4.5),
            ("SUBTLE on SELECTION", SUBTLE, SELECTION, 4.5),
            ("TEXT on HOVER", TEXT, HOVER, 4.5),
            ("ON_ACCENT on PRIMARY", ON_ACCENT, PRIMARY, 4.5),
        ];
        for (pair, on, under, floor) in pairs {
            let ratio = contrast(on, under);
            assert!(ratio >= floor, "{pair} is {ratio:.2}:1, below {floor}:1");
        }
    }
}
