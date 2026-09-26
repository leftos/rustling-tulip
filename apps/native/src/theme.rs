//! The terminal's colour theme: the base palette with every colour nudged
//! away from the background until it keeps a legible contrast ratio.
//!
//! The arithmetic here matches the Tauri client's `terminalTheme.ts`, so both
//! clients paint the same colours; its expected values are pinned in this
//! module's tests.

#![expect(clippy::unreadable_literal, reason = "hex colors read as #rrggbb")]

use alacritty_terminal::vte::ansi::Rgb;

/// The background a pane starts from until it is told to use another one.
pub const DEFAULT_BACKGROUND: Rgb = rgb(0x08090b);

/// The contrast a colour must reach against the background: the default text
/// colour, the caret and the selection tint, the eight base colours, and the
/// eight bright ones.
const MIN_FG_CONTRAST: f64 = 7.0;
const MIN_TINT_CONTRAST: f64 = 3.0;
const MIN_BASE_CONTRAST: f64 = 2.4;

/// The share of a selected cell's background the selection tint covers.
const SELECTION_ALPHA: f64 = 0.3;

/// How far one `ensure_contrast` step moves a colour.
const STEP: f64 = 0.16;

/// The number of steps `ensure_contrast` tries before giving up.
const MAX_STEPS: u32 = 11;

const FOREGROUND_ON_DARK: Rgb = rgb(0xe5e6e8);
const FOREGROUND_ON_LIGHT: Rgb = rgb(0x1a1c22);
const WHITE: Rgb = rgb(0xffffff);
const BLACK: Rgb = rgb(0x000000);
const SELECTION_TINT: Rgb = rgb(0x5b9bff);

/// The base palette, the eight base colours then the eight bright ones.
const BASE_ANSI: [Rgb; 16] = [
    rgb(0x16181d),
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
];

/// Every colour a grid cell resolves against.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    /// The default text colour.
    pub fg: Rgb,
    pub bg: Rgb,
    pub caret: Rgb,
    /// The selection tint, laid over a selected cell's background at
    /// [`Self::selection_alpha`].
    pub selection: Rgb,
    pub selection_alpha: f64,
    /// The colour selected text is drawn in.
    pub selection_fg: Rgb,
    /// The 16 ANSI colours, base colours then bright ones. The 256-colour
    /// cube above index 15 stays the standard xterm one.
    pub ansi: [Rgb; 16],
}

impl Default for Theme {
    fn default() -> Self {
        build_theme(DEFAULT_BACKGROUND)
    }
}

/// The theme for `background`: the base palette, each colour raised toward
/// white (or lowered toward black on a light background) until it reaches its
/// contrast minimum.
pub fn build_theme(background: Rgb) -> Theme {
    let base = if luminance(background) < 0.5 {
        FOREGROUND_ON_DARK
    } else {
        FOREGROUND_ON_LIGHT
    };
    let fg = ensure_contrast(base, background, MIN_FG_CONTRAST);
    let mut ansi = [BLACK; 16];
    for (i, color) in BASE_ANSI.into_iter().enumerate() {
        let min = if i < 8 {
            MIN_BASE_CONTRAST
        } else {
            MIN_TINT_CONTRAST
        };
        ansi[i] = ensure_contrast(color, background, min);
    }
    Theme {
        fg,
        bg: background,
        caret: ensure_contrast(WHITE, background, MIN_TINT_CONTRAST),
        selection: ensure_contrast(SELECTION_TINT, background, MIN_TINT_CONTRAST),
        selection_alpha: SELECTION_ALPHA,
        selection_fg: fg,
        ansi,
    }
}

/// `color` moved away from `background` until it reaches `min_ratio`, or the
/// 11th step, whichever comes first. The direction is away from the
/// background: toward white on a dark one, toward black on a light one.
pub fn ensure_contrast(color: Rgb, background: Rgb, min_ratio: f64) -> Rgb {
    let toward = if luminance(background) < 0.5 {
        WHITE
    } else {
        BLACK
    };
    let mut current = color;
    for _ in 0..MAX_STEPS {
        if contrast_ratio(current, background) >= min_ratio {
            return current;
        }
        current = mix(current, toward, STEP);
    }
    current
}

/// `from` moved `amount` of the way to `toward`, each channel rounded to the
/// nearest 8-bit value.
pub fn mix(from: Rgb, toward: Rgb, amount: f64) -> Rgb {
    let channel = |a: u8, b: u8| {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a weighted average of two channels is a 0-255 value"
        )]
        let v = (f64::from(a) + (f64::from(b) - f64::from(a)) * amount).round() as u8;
        v
    };
    Rgb {
        r: channel(from.r, toward.r),
        g: channel(from.g, toward.g),
        b: channel(from.b, toward.b),
    }
}

/// The WCAG contrast ratio between two colours: 1.0 for identical ones, 21.0
/// for black against white.
pub fn contrast_ratio(a: Rgb, b: Rgb) -> f64 {
    let light = luminance(a).max(luminance(b));
    let dark = luminance(a).min(luminance(b));
    (light + 0.05) / (dark + 0.05)
}

/// `color`'s relative luminance: 0.0 for black, 1.0 for white.
pub fn luminance(color: Rgb) -> f64 {
    let channel = |c: u8| {
        let value = f64::from(c) / 255.0;
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
}

const fn rgb(hex: u32) -> Rgb {
    let [_, r, g, b] = hex.to_be_bytes();
    Rgb { r, g, b }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_BACKGROUND, Theme, build_theme, contrast_ratio, ensure_contrast, luminance, mix,
        rgb,
    };

    #[test]
    fn luminance_of_black_white_and_blue() {
        assert!((luminance(rgb(0x000000)) - 0.0).abs() < f64::EPSILON);
        assert!((luminance(rgb(0xffffff)) - 1.0).abs() < f64::EPSILON);
        // `terminalTheme.ts`'s `luminance(hexToRgb("#5b9bff"))`.
        let blue = luminance(rgb(0x5b9bff));
        assert!((blue - 0.328_868_360_247_807_7).abs() < 1e-12, "got {blue}");
    }

    #[test]
    fn contrast_ratio_of_black_on_white_is_21() {
        let ratio = contrast_ratio(rgb(0x000000), rgb(0xffffff));
        assert!((ratio - 21.0).abs() < f64::EPSILON, "got {ratio}");
    }

    #[test]
    fn ensure_contrast_leaves_a_colour_that_already_passes() {
        let bg = DEFAULT_BACKGROUND;
        // White is 21:1 on this background and base red is above 2.4:1, so
        // neither moves.
        assert_eq!(ensure_contrast(rgb(0xffffff), bg, 3.0), rgb(0xffffff));
        assert_eq!(ensure_contrast(rgb(0xef5c5c), bg, 2.4), rgb(0xef5c5c));
    }

    #[test]
    fn ensure_contrast_moves_a_dark_background_colour_toward_white() {
        let bg = DEFAULT_BACKGROUND;
        let base_black = rgb(0x16181d);
        assert!(contrast_ratio(base_black, bg) < 2.4);
        let raised = ensure_contrast(base_black, bg, 2.4);
        // Two 16% steps toward white, as `ensureContrast` gives in Node.
        assert_eq!(raised, rgb(0x5a5c5f));
        assert!(luminance(raised) > luminance(base_black));
        assert!(contrast_ratio(raised, bg) >= 2.4);
    }

    #[test]
    fn ensure_contrast_gives_up_after_eleven_steps() {
        assert_eq!(mix(rgb(0xffffff), rgb(0x000000), 0.16), rgb(0xd6d6d6));
        // White on white can never reach 21:1, in either direction, so all
        // eleven steps run and the last one's value comes back.
        let white = rgb(0xffffff);
        let reached = ensure_contrast(white, white, 21.0);
        assert_eq!(reached, rgb(0x262626));
        assert!(contrast_ratio(reached, white) < 21.0);
    }

    #[test]
    fn default_theme_matches_the_ported_values() {
        let theme = Theme::default();
        assert_eq!(theme.bg, DEFAULT_BACKGROUND);
        assert_eq!(theme.bg, rgb(0x08090b));
        assert_eq!(theme.fg, rgb(0xe5e6e8));
        assert_eq!(theme.caret, rgb(0xffffff));
        assert_eq!(theme.selection, rgb(0x5b9bff));
        assert!((theme.selection_alpha - 0.3).abs() < f64::EPSILON);
        assert_eq!(theme.selection_fg, theme.fg);
        // `buildTerminalTheme("#08090b")`, as printed by Node. Base black is
        // the one entry that moves — it is below 2.4:1 and so is raised —
        // while base red already clears it and comes through unchanged.
        assert_eq!(
            theme.ansi,
            [
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
            ]
        );
        assert_eq!(
            ensure_contrast(rgb(0x16181d), DEFAULT_BACKGROUND, 2.4),
            theme.ansi[0]
        );
        assert!(contrast_ratio(rgb(0xef5c5c), DEFAULT_BACKGROUND) >= 2.4);
    }

    #[test]
    fn light_background_gets_a_dark_foreground() {
        let paper = rgb(0xf6f4ef);
        let theme = build_theme(paper);
        assert_eq!(theme.bg, paper);
        assert_eq!(theme.fg, rgb(0x1a1c22));
        assert!(luminance(theme.fg) < luminance(paper));
        // White would be unreadable on it, so the caret darkens too.
        assert_eq!(theme.caret, rgb(0x7f7f7f));
    }
}
