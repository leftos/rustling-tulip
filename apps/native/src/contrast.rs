//! The WCAG 2.x contrast of a theme's text and fill pairs: the ratio of
//! two opaque colours and the table of token pairs a theme is held to.

use crate::appearance::packed;
use crate::ui_theme::UiTheme;

/// A foreground token drawn on a background token, and the contrast ratio
/// the pair must reach. `terminal_fg` and `terminal_bg` name the terminal
/// palette's default text and background.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pair {
    pub fg: &'static str,
    pub bg: &'static str,
    pub minimum: f32,
}

/// One pair of a theme as measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PairResult {
    pub fg: &'static str,
    pub bg: &'static str,
    pub ratio: f32,
    pub minimum: f32,
    pub passes: bool,
}

/// Normal text's minimum.
const TEXT: f32 = 4.5;
/// A glyph fill's minimum.
const GLYPH: f32 = 3.0;

const fn pair(fg: &'static str, bg: &'static str, minimum: f32) -> Pair {
    Pair { fg, bg, minimum }
}

/// Every pair a theme is checked on. Each names an opaque token: a token
/// carrying alpha has no single colour to measure.
pub const PAIRS: &[Pair] = &[
    pair("text", "ground", TEXT),
    pair("text", "surface", TEXT),
    pair("text", "raised", TEXT),
    pair("text", "row_hover", TEXT),
    pair("text", "popover", TEXT),
    pair("text", "modal", TEXT),
    pair("text_2", "ground", TEXT),
    pair("text_2", "surface", TEXT),
    pair("text_2", "raised", TEXT),
    pair("text_2", "row_hover", TEXT),
    pair("text_2", "popover", TEXT),
    pair("text_2", "modal", TEXT),
    pair("subtle", "ground", TEXT),
    pair("subtle", "surface", TEXT),
    pair("subtle", "raised", TEXT),
    pair("subtle", "row_hover", TEXT),
    pair("subtle", "popover", TEXT),
    pair("subtle", "modal", TEXT),
    pair("on_accent", "accent", TEXT),
    pair("on_asking", "asking", TEXT),
    pair("danger_text", "ground", TEXT),
    pair("faint", "ground", GLYPH),
    pair("faint", "surface", GLYPH),
    pair("terminal_fg", "terminal_bg", TEXT),
];

/// The value of the token `name` in `t`, as `0xRRGGBB`, for the names
/// [`PAIRS`] uses.
#[must_use]
pub fn token(t: &UiTheme, name: &str) -> Option<u32> {
    let value = match name {
        "ground" => t.ground,
        "surface" => t.surface,
        "raised" => t.raised,
        "row_hover" => t.row_hover,
        "popover" => t.popover,
        "modal" => t.modal,
        "text" => t.text,
        "text_2" => t.text_2,
        "subtle" => t.subtle,
        "faint" => t.faint,
        "accent" => t.accent,
        "on_accent" => t.on_accent,
        "asking" => t.asking,
        "on_asking" => t.on_asking,
        "danger_text" => t.danger_text,
        "terminal_fg" => packed(t.terminal.fg),
        "terminal_bg" => packed(t.terminal.bg),
        _ => return None,
    };
    Some(value)
}

/// `rgb`'s relative luminance, `0xRRGGBB`: 0.0 for black, 1.0 for white.
#[must_use]
pub fn relative_luminance(rgb: u32) -> f32 {
    let [_, r, g, b] = rgb.to_be_bytes();
    let channel = |c: u8| {
        let value = f32::from(c) / 255.0;
        if value <= 0.040_45 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

/// The contrast ratio of two opaque `0xRRGGBB` colours: 1.0 for equal
/// ones, 21.0 for black on white, whichever is lighter.
#[must_use]
pub fn ratio(fg: u32, bg: u32) -> f32 {
    let a = relative_luminance(fg);
    let b = relative_luminance(bg);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// Every pair of [`PAIRS`] measured in `t`, in table order.
#[must_use]
pub fn check(t: &UiTheme) -> Vec<PairResult> {
    PAIRS
        .iter()
        .filter_map(|p| {
            let measured = ratio(token(t, p.fg)?, token(t, p.bg)?);
            Some(PairResult {
                fg: p.fg,
                bg: p.bg,
                ratio: measured,
                minimum: p.minimum,
                passes: measured >= p.minimum,
            })
        })
        .collect()
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
#[expect(clippy::unreadable_literal, reason = "hex colors read as #rrggbb")]
mod tests {
    use super::*;

    #[test]
    fn black_on_white_is_21() {
        let r = ratio(0x000000, 0xffffff);
        assert!((r - 21.0).abs() < 1e-4, "got {r}");
        assert!((ratio(0xffffff, 0x000000) - r).abs() < f32::EPSILON);
    }

    #[test]
    fn equal_colours_are_1() {
        for c in [0x000000, 0x777777, 0xf07a62, 0xffffff] {
            let r = ratio(c, c);
            assert!((r - 1.0).abs() < 1e-6, "{c:06x}: {r}");
        }
    }

    #[test]
    fn a_known_mid_pair() {
        let r = ratio(0x777777, 0xffffff);
        assert!((r - 4.48).abs() < 0.01, "got {r}");
        assert!(r < 4.5, "#777777 on white just misses 4.5:1");
    }

    #[test]
    fn every_pair_names_an_opaque_token() {
        let t = UiTheme::CONTRAST;
        let ui = serde_json::to_value(t).expect("the theme serializes");
        for p in PAIRS {
            for name in [p.fg, p.bg] {
                assert!(token(&t, name).is_some(), "{name} names no token");
                if name.starts_with("terminal_") {
                    continue;
                }
                let stored = ui[name].as_str().expect("a colour token");
                assert_eq!(stored.len(), 7, "{name} is {stored}, not #rrggbb");
            }
        }
        assert_eq!(check(&t).len(), PAIRS.len(), "every pair is measured");
    }
}
