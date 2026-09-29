//! Every UI colour of the native client, defined once.
//!
//! A view names a token here, never a literal, so recolouring the client is a
//! change to this one file. A value is a `u32` the view hands to `gpui::rgb`
//! (RGB, `0xRRGGBB`) or `gpui::rgba` (RGBA, `0xRRGGBBAA`); the leading byte of
//! an RGB value is padding those conversions ignore.
//!
//! `lib.rs` re-exports the names the views already import from the crate root
//! (`BAR_BG`, `PANEL_BG`, `BORDER`, `TEXT`, …), so their imports keep working.

/// Background of the footer and the tab bar, as `0xRRGGBB`.
pub const BAR_BG: u32 = 0x0025_2526;
/// Background of the sidebar and panels, as `0xRRGGBB`.
pub const PANEL_BG: u32 = 0x000f_1014;
/// Background of a flyout's panel, as `0xRRGGBB`.
pub(crate) const OVERLAY_BG: u32 = 0x001e_1e1e;
/// Background of a hovered row or button, as `0xRRGGBB`.
pub(crate) const HOVER_BG: u32 = 0x002d_2f36;
/// A divider, and a panel's or control's edge, as `0xRRGGBB`.
pub(crate) const BORDER: u32 = 0x0020_222a;
/// Ordinary text, as `0xRRGGBB`.
pub(crate) const TEXT: u32 = 0x00cc_cccc;
/// Text that stands back: a label, a hint, a secondary value, as `0xRRGGBB`.
pub(crate) const MUTED: u32 = 0x009a_9a9a;
/// Red, for something that failed, as `0xRRGGBB`.
pub(crate) const DANGER: u32 = 0x00ef_5c5c;
/// The fill behind an armed destructive control, as `0xRRGGBB`.
pub(crate) const DANGER_BG: u32 = 0x003a_1c1f;
/// Amber, for something the user should look at that is not a failure, as
/// `0xRRGGBB`.
pub(crate) const WARNING: u32 = 0x00e8_a531;

/// The footer's dot for a healthy daemon, as `0xRRGGBB`.
pub(crate) const STATUS_OK: u32 = 0x003f_b96a;
/// The footer's dot for an idle or stopped daemon, as `0xRRGGBB`.
pub(crate) const STATUS_IDLE: u32 = 0x0083_8a96;
/// The footer's dot for a daemon in error, as `0xRRGGBB`.
pub(crate) const STATUS_ERR: u32 = 0x00ef_5c5c;

/// The fill of a selected sidebar row, layout row or chooser button, as
/// `0xRRGGBB`.
pub(crate) const SELECTED_BG: u32 = 0x0037_3a44;

/// An "ok" badge of the spawn dialog's workspace preview table, as `0xRRGGBB`.
pub(crate) const SPAWN_BADGE_OK: u32 = 0x004e_c9b0;

/// A text input's text, as `0xRRGGBB`.
pub(crate) const INPUT_TEXT: u32 = 0x00cc_cccc;
/// A text input's placeholder, as `0xRRGGBB`.
pub(crate) const INPUT_PLACEHOLDER: u32 = 0x006a_6a6a;
/// A text input's caret, as `0xRRGGBB`.
pub(crate) const INPUT_CURSOR: u32 = 0x00cc_cccc;
/// A text input's selection, as `0xRRGGBBAA`.
pub(crate) const INPUT_SELECTION: u32 = 0x264f_78ff;

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

/// The stopped-pane overlay, translucent so the terminal shows through, as
/// `0xRRGGBBAA`.
pub(crate) const OVERLAY_TINT: u32 = 0x1e1e_1ecc;
/// The dim layer behind the delete-worktree confirm, as `0xRRGGBBAA`.
pub(crate) const BACKDROP_TINT: u32 = 0x0000_0099;

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

    /// Every token holds the value it had before it moved here. These are the
    /// only record of the colours nothing else pins, so a recolour edits both
    /// the token and this test.
    #[test]
    fn every_token_keeps_its_pre_palette_value() {
        assert_eq!(BAR_BG, 0x0025_2526);
        assert_eq!(PANEL_BG, 0x000f_1014);
        assert_eq!(OVERLAY_BG, 0x001e_1e1e);
        assert_eq!(HOVER_BG, 0x002d_2f36);
        assert_eq!(BORDER, 0x0020_222a);
        assert_eq!(TEXT, 0x00cc_cccc);
        assert_eq!(MUTED, 0x009a_9a9a);
        assert_eq!(DANGER, 0x00ef_5c5c);
        assert_eq!(DANGER_BG, 0x003a_1c1f);
        assert_eq!(WARNING, 0x00e8_a531);
        assert_eq!(STATUS_OK, 0x003f_b96a);
        assert_eq!(STATUS_IDLE, 0x0083_8a96);
        assert_eq!(STATUS_ERR, 0x00ef_5c5c);
        assert_eq!(SELECTED_BG, 0x0037_3a44);
        assert_eq!(SPAWN_BADGE_OK, 0x004e_c9b0);
        assert_eq!(INPUT_TEXT, 0x00cc_cccc);
        assert_eq!(INPUT_PLACEHOLDER, 0x006a_6a6a);
        assert_eq!(INPUT_CURSOR, 0x00cc_cccc);
        assert_eq!(INPUT_SELECTION, 0x264f_78ff);
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
        assert_eq!(OVERLAY_TINT, 0x1e1e_1ecc);
        assert_eq!(BACKDROP_TINT, 0x0000_0099);
        assert_eq!(SHELL_MARK_BORDER, 0x0000_0059);
        assert_eq!(SHELL_MARK_OK, 0x004e_c9b0);
        assert_eq!(SHELL_MARK_FAIL, 0x00f4_8771);
        assert_eq!(TRANSPARENT, 0x0000_0000);
    }
}
