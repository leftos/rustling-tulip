//! The client's fonts: the terminal and interface families it bundles, the
//! font settings a pane renders with, the family the interface draws in,
//! and how a requested family resolves against the fonts the text system
//! knows.

use std::borrow::Cow;
use std::ops::RangeInclusive;

use gpui::{App, FontWeight, Global, Pixels, SharedString, TextSystem, px};
use serde::{Deserialize, Serialize};

/// The family a pane renders in when the settings name none.
pub const DEFAULT_FAMILY: &str = "Geist Mono";
/// The family the client's own interface draws in when the user picked
/// none; terminal panes draw in their own.
pub const DEFAULT_UI_FAMILY: &str = "Inter";
/// The proportional families the client ships for its interface, as their
/// fonts name them, in the order the interface font picker lists them; no
/// terminal picker lists them.
pub const BUNDLED_UI_FAMILIES: [&str; 3] = ["Inter", "Geist", "Schibsted Grotesk"];
pub const DEFAULT_SIZE: f32 = 13.0;
/// The smallest size any level may set: the protocol's.
pub const MIN_SIZE: f32 = *protocol::TERMINAL_FONT_SIZES.start() as f32;
/// The largest size any level may set: the protocol's.
pub const MAX_SIZE: f32 = *protocol::TERMINAL_FONT_SIZES.end() as f32;

/// The families the client ships, as their fonts name them.
pub const BUNDLED_FAMILIES: [&str; 4] =
    ["Geist Mono", "Fira Code", "JetBrains Mono", "Cascadia Code"];

/// Tried in order when the requested family is not installed.
const FALLBACK_CHAIN: [&str; 4] = ["Geist Mono", "Cascadia Mono", "Consolas", "Courier New"];

/// A bundled face and the style it is.
struct Face {
    data: &'static [u8],
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the tests check each face's style against it")
    )]
    italic: bool,
}

const fn upright(data: &'static [u8]) -> Face {
    Face {
        data,
        italic: false,
    }
}

const fn italic(data: &'static [u8]) -> Face {
    Face { data, italic: true }
}

/// The bundled faces: Regular and Bold of each family, plus Italic and Bold
/// Italic where the family has them (Fira Code has none).
const BUNDLED_FILES: [Face; 14] = [
    upright(include_bytes!(
        "../assets/fonts/GeistMono/GeistMono-Regular.ttf"
    )),
    upright(include_bytes!(
        "../assets/fonts/GeistMono/GeistMono-Bold.ttf"
    )),
    italic(include_bytes!(
        "../assets/fonts/GeistMono/GeistMono-Italic.ttf"
    )),
    italic(include_bytes!(
        "../assets/fonts/GeistMono/GeistMono-BoldItalic.ttf"
    )),
    upright(include_bytes!(
        "../assets/fonts/FiraCode/FiraCode-Regular.ttf"
    )),
    upright(include_bytes!("../assets/fonts/FiraCode/FiraCode-Bold.ttf")),
    upright(include_bytes!(
        "../assets/fonts/JetBrainsMono/JetBrainsMono-Regular.ttf"
    )),
    upright(include_bytes!(
        "../assets/fonts/JetBrainsMono/JetBrainsMono-Bold.ttf"
    )),
    italic(include_bytes!(
        "../assets/fonts/JetBrainsMono/JetBrainsMono-Italic.ttf"
    )),
    italic(include_bytes!(
        "../assets/fonts/JetBrainsMono/JetBrainsMono-BoldItalic.ttf"
    )),
    upright(include_bytes!(
        "../assets/fonts/CascadiaCode/CascadiaCode-Regular.ttf"
    )),
    upright(include_bytes!(
        "../assets/fonts/CascadiaCode/CascadiaCode-Bold.ttf"
    )),
    italic(include_bytes!(
        "../assets/fonts/CascadiaCode/CascadiaCode-Italic.ttf"
    )),
    italic(include_bytes!(
        "../assets/fonts/CascadiaCode/CascadiaCode-BoldItalic.ttf"
    )),
];

/// A bundled UI face, its family and the weight its `OS/2` table declares.
struct UiFace {
    data: &'static [u8],
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the tests check each face's family and weight against it"
        )
    )]
    family: &'static str,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the tests check each face's weight against it")
    )]
    weight: u16,
}

/// The bundled UI faces of [`BUNDLED_UI_FAMILIES`]: Regular, Medium,
/// `SemiBold` and Bold of each.
const UI_FILES: [UiFace; 12] = [
    UiFace {
        data: include_bytes!("../assets/fonts/Geist/Geist-Regular.ttf"),
        family: "Geist",
        weight: 400,
    },
    UiFace {
        data: include_bytes!("../assets/fonts/Geist/Geist-Medium.ttf"),
        family: "Geist",
        weight: 500,
    },
    UiFace {
        data: include_bytes!("../assets/fonts/Geist/Geist-SemiBold.ttf"),
        family: "Geist",
        weight: 600,
    },
    UiFace {
        data: include_bytes!("../assets/fonts/Geist/Geist-Bold.ttf"),
        family: "Geist",
        weight: 700,
    },
    UiFace {
        data: include_bytes!("../assets/fonts/Inter/Inter-Regular.ttf"),
        family: "Inter",
        weight: 400,
    },
    UiFace {
        data: include_bytes!("../assets/fonts/Inter/Inter-Medium.ttf"),
        family: "Inter",
        weight: 500,
    },
    UiFace {
        data: include_bytes!("../assets/fonts/Inter/Inter-SemiBold.ttf"),
        family: "Inter",
        weight: 600,
    },
    UiFace {
        data: include_bytes!("../assets/fonts/Inter/Inter-Bold.ttf"),
        family: "Inter",
        weight: 700,
    },
    UiFace {
        data: include_bytes!("../assets/fonts/SchibstedGrotesk/SchibstedGrotesk-Regular.ttf"),
        family: "Schibsted Grotesk",
        weight: 400,
    },
    UiFace {
        data: include_bytes!("../assets/fonts/SchibstedGrotesk/SchibstedGrotesk-Medium.ttf"),
        family: "Schibsted Grotesk",
        weight: 500,
    },
    UiFace {
        data: include_bytes!("../assets/fonts/SchibstedGrotesk/SchibstedGrotesk-SemiBold.ttf"),
        family: "Schibsted Grotesk",
        weight: 600,
    },
    UiFace {
        data: include_bytes!("../assets/fonts/SchibstedGrotesk/SchibstedGrotesk-Bold.ttf"),
        family: "Schibsted Grotesk",
        weight: 700,
    },
];

/// The family the interface draws in: the user's pick as resolved against
/// the installed fonts.
struct UiFont(SharedString);

impl Global for UiFont {}

/// The family the interface draws in; [`DEFAULT_UI_FAMILY`] until
/// [`set_ui_family`] has run.
#[must_use]
pub fn ui_family(cx: &App) -> SharedString {
    cx.try_global::<UiFont>().map_or_else(
        || SharedString::new_static(DEFAULT_UI_FAMILY),
        |font| font.0.clone(),
    )
}

/// Makes `requested` (or [`DEFAULT_UI_FAMILY`]) the family the interface
/// draws in, resolved against the fonts the text system knows; they are
/// listed only for a family the client does not bundle.
pub fn set_ui_family(requested: Option<&str>, cx: &mut App) {
    let family = resolve_ui_family(requested, || available_families(cx.text_system()));
    cx.set_global(UiFont(family));
}

/// [`set_ui_family`] against `installed`, the system's families as a font
/// list already has them.
pub fn set_ui_family_among(requested: Option<&str>, installed: &[SharedString], cx: &mut App) {
    let family = resolve_ui_family(requested, || installed.to_vec());
    cx.set_global(UiFont(family));
}

/// The whole pixels the user may add to every UI text size, smallest to
/// largest; 0 draws each at its base.
pub const UI_TEXT_SIZE_OFFSETS: RangeInclusive<i8> = -2..=4;
/// The smallest a UI text size gets, whatever the offset.
const MIN_UI_PX: f32 = 8.0;
/// The weights the user may draw the interface's body text at, lightest
/// first; each place past the first is one shift of every UI weight.
pub const UI_WEIGHTS: [u16; 3] = [400, 500, 600];
/// The body weight the interface draws at when the user picked none.
pub const DEFAULT_UI_WEIGHT: u16 = UI_WEIGHTS[0];
/// The heaviest weight a shift reaches: the heaviest bundled UI face.
const MAX_SHIFTED_WEIGHT: f32 = 700.0;

/// How the interface's text is drawn beyond its family: the whole pixels
/// added to every UI text size and to the fixed heights that hold one line,
/// and the hundreds added to every UI weight.
#[derive(Clone, Copy, Default)]
struct UiText {
    size_offset: i8,
    weight_shift: i8,
}

impl Global for UiText {}

/// How the app draws the interface's text; nothing added until a setter ran.
fn ui_text(cx: &App) -> UiText {
    cx.try_global::<UiText>().copied().unwrap_or_default()
}

/// The offset every UI text size is drawn at; 0 until
/// [`set_ui_text_size`] has run.
#[must_use]
pub fn ui_text_size(cx: &App) -> i8 {
    ui_text(cx).size_offset
}

/// Makes `offset`, clamped to [`UI_TEXT_SIZE_OFFSETS`], the offset every UI
/// text size is drawn at.
pub fn set_ui_text_size(offset: i8, cx: &mut App) {
    cx.set_global(UiText {
        size_offset: clamped_ui_text_size(i64::from(offset)),
        ..ui_text(cx)
    });
}

/// The hundreds added to every UI weight; 0 until [`set_ui_weight`] has run.
#[must_use]
pub fn ui_weight_shift(cx: &App) -> i8 {
    ui_text(cx).weight_shift
}

/// Makes `weight`, one of [`UI_WEIGHTS`], the interface's body weight; any
/// other weight draws as [`DEFAULT_UI_WEIGHT`].
pub fn set_ui_weight(weight: u16, cx: &mut App) {
    let shift = UI_WEIGHTS
        .iter()
        .position(|offered| *offered == weight)
        .and_then(|place| i8::try_from(place).ok())
        .unwrap_or(0);
    cx.set_global(UiText {
        weight_shift: shift,
        ..ui_text(cx)
    });
}

/// The weight mono text draws at: the bundled mono faces are Regular and Bold
/// only, so a shift would draw all source code Bold.
#[must_use]
pub const fn mono_weight() -> FontWeight {
    FontWeight::NORMAL
}

/// Weight `requested` at the app's shift.
#[must_use]
pub fn ui_weight(cx: &App, requested: FontWeight) -> FontWeight {
    ui_weight_at(ui_weight_shift(cx), requested)
}

/// Weight `requested` raised by `shift` hundreds, stopping at 700; a weight
/// already past that stays where it is.
#[must_use]
pub fn ui_weight_at(shift: i8, requested: FontWeight) -> FontWeight {
    let shifted = requested.0 + 100.0 * f32::from(shift);
    FontWeight(shifted.min(MAX_SHIFTED_WEIGHT).max(requested.0))
}

/// `offset` clamped to [`UI_TEXT_SIZE_OFFSETS`].
#[must_use]
pub fn clamped_ui_text_size(offset: i64) -> i8 {
    let low = *UI_TEXT_SIZE_OFFSETS.start();
    let high = *UI_TEXT_SIZE_OFFSETS.end();
    i8::try_from(offset.clamp(i64::from(low), i64::from(high))).unwrap_or(0)
}

/// UI size `base` (a text size, or a fixed height that holds one line of
/// text) at the app's offset.
#[must_use]
pub fn ui_px(cx: &App, base: f32) -> Pixels {
    ui_px_at(ui_text_size(cx), base)
}

/// UI size `base` at `offset`, never below 8 px.
#[must_use]
pub fn ui_px_at(offset: i8, base: f32) -> Pixels {
    px((base + f32::from(offset)).max(MIN_UI_PX))
}

/// The family to draw the interface in for `requested` (or
/// [`DEFAULT_UI_FAMILY`]): a bundled UI family as it is named, which the
/// client registers at startup; else itself, as `available` names it, when
/// the listing has it, else [`DEFAULT_UI_FAMILY`]. `available` is called
/// only for a family not bundled; with nothing to check against, the family
/// is returned unchecked.
#[must_use]
pub fn resolve_ui_family(
    requested: Option<&str>,
    available: impl FnOnce() -> Vec<SharedString>,
) -> SharedString {
    let Some(wanted) = requested else {
        return SharedString::new_static(DEFAULT_UI_FAMILY);
    };
    if let Some(bundled) = BUNDLED_UI_FAMILIES
        .iter()
        .find(|family| family.eq_ignore_ascii_case(wanted))
    {
        return SharedString::new_static(bundled);
    }
    let available = available();
    if available.is_empty() {
        return SharedString::from(wanted.to_owned());
    }
    available
        .iter()
        .find(|family| family.eq_ignore_ascii_case(wanted))
        .cloned()
        .unwrap_or_else(|| {
            tracing::warn!(
                "the interface font {wanted:?} is not installed; drawing in {DEFAULT_UI_FAMILY}"
            );
            SharedString::new_static(DEFAULT_UI_FAMILY)
        })
}

/// How a terminal pane draws its text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FontSettings {
    /// The family asked for; `None` is [`DEFAULT_FAMILY`].
    pub family: Option<String>,
    /// The size in pixels, a whole number from [`MIN_SIZE`] to [`MAX_SIZE`].
    pub size: f32,
    /// Draws normal text at the bold weight; bold cells stay bold.
    pub bold: bool,
}

impl Default for FontSettings {
    fn default() -> Self {
        Self {
            family: None,
            size: DEFAULT_SIZE,
            bold: false,
        }
    }
}

impl FontSettings {
    /// These settings with the size clamped and rounded.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.size = clamp_size(self.size);
        self
    }
}

/// `size` rounded to a whole number of pixels and clamped to
/// [`MIN_SIZE`]..=[`MAX_SIZE`]; a size that is not a number is the default.
#[must_use]
pub fn clamp_size(size: f32) -> f32 {
    if size.is_nan() {
        return DEFAULT_SIZE;
    }
    size.round().clamp(MIN_SIZE, MAX_SIZE)
}

/// The size `size` moves to one step of `delta`, or `None` when the clamp
/// already holds it there: a shortcut at a limit does nothing.
#[must_use]
pub fn stepped(size: f32, delta: f32) -> Option<f32> {
    let size = clamp_size(size);
    let next = clamp_size(size + delta);
    ((next - size).abs() > f32::EPSILON).then_some(next)
}

/// The family to render `requested` (or [`DEFAULT_FAMILY`]) with: itself
/// when `available` has it, else the first of the fallback chain that it
/// has. With nothing to check against, the family is returned unchecked.
#[must_use]
pub fn resolve_family(requested: Option<&str>, available: &[SharedString]) -> SharedString {
    let wanted = requested.unwrap_or(DEFAULT_FAMILY);
    let find = |name: &str| {
        available
            .iter()
            .find(|family| family.eq_ignore_ascii_case(name))
            .cloned()
    };
    if available.is_empty() {
        return SharedString::from(wanted.to_owned());
    }
    find(wanted)
        .or_else(|| FALLBACK_CHAIN.iter().find_map(|name| find(name)))
        .unwrap_or_else(|| SharedString::from(wanted.to_owned()))
}

/// The line height for a font's `ascent` and `descent` (either sign) at
/// `size`: their sum rounded up to whole pixels, or `size × 1.2` when the
/// font gave no usable metrics.
#[must_use]
pub fn line_height(ascent: f32, descent: f32, size: f32) -> f32 {
    let natural = ascent + descent.abs();
    if natural.is_finite() && natural > 0.0 {
        natural.ceil()
    } else {
        (size * 1.2).ceil()
    }
}

/// Registers the bundled fonts with the text system; a failure is logged
/// and the client falls back to installed families.
pub fn register_bundled(cx: &App) {
    let fonts = BUNDLED_FILES
        .iter()
        .map(|face| Cow::Borrowed(face.data))
        .chain(UI_FILES.iter().map(|face| Cow::Borrowed(face.data)))
        .collect();
    if let Err(err) = cx.text_system().add_fonts(fonts) {
        tracing::error!("registering the bundled fonts: {err:#}");
    }
}

/// Every family the text system can render.
#[must_use]
pub fn available_families(text: &TextSystem) -> Vec<SharedString> {
    text.all_font_names()
        .into_iter()
        .map(SharedString::from)
        .collect()
}

/// The installed families that are not bundled, sorted, for a font picker.
#[must_use]
pub fn system_families(cx: &App) -> Vec<SharedString> {
    picker_families(available_families(cx.text_system()))
}

/// `families` as a font picker lists them: sorted, each once, without the
/// bundled ones — the interface's families included, which no terminal
/// draws in — and without the hidden system faces whose names start with `.`.
fn picker_families(families: Vec<SharedString>) -> Vec<SharedString> {
    let mut families: Vec<SharedString> = families
        .into_iter()
        .filter(|family| {
            !BUNDLED_FAMILIES.contains(&family.as_ref())
                && !BUNDLED_UI_FAMILIES.contains(&family.as_ref())
        })
        .filter(|family| !family.starts_with('.'))
        .collect();
    families.sort();
    families.dedup();
    families
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::sidebar::UiState;

    fn names(list: &[&'static str]) -> Vec<SharedString> {
        list.iter().copied().map(SharedString::new_static).collect()
    }

    #[test]
    fn the_picker_drops_bundled_and_dot_named_families() {
        let listed = picker_families(names(&[
            "Consolas",
            ".SF NS Mono",
            "Fira Code",
            "Arial",
            ".Hidden",
            "Consolas",
        ]));
        assert_eq!(listed, names(&["Arial", "Consolas"]));
    }

    #[test]
    fn terminal_picker_lists_no_ui_family() {
        let listed = picker_families(names(&["Geist", "Inter", "Schibsted Grotesk", "Consolas"]));
        assert_eq!(listed, names(&["Consolas"]));
    }

    /// A listing of `families` that counts how often it is asked for.
    fn counted<'a>(
        families: &'a [&'static str],
        calls: &'a std::cell::Cell<u32>,
    ) -> impl FnOnce() -> Vec<SharedString> + 'a {
        move || {
            calls.set(calls.get() + 1);
            names(families)
        }
    }

    #[test]
    fn ui_px_adds_the_offset_and_never_goes_below_8() {
        assert_eq!(ui_px_at(0, 12.0), gpui::px(12.0), "no offset: the base");
        assert_eq!(ui_px_at(3, 12.0), gpui::px(15.0));
        assert_eq!(ui_px_at(-2, 11.5), gpui::px(9.5));
        assert_eq!(ui_px_at(-2, 9.5), gpui::px(8.0), "floored at 8 px");
        assert_eq!(ui_px_at(4, 6.0), gpui::px(10.0), "the floor is on the sum");
        assert_eq!(clamped_ui_text_size(9), 4, "above the range: its top");
        assert_eq!(
            clamped_ui_text_size(-100),
            -2,
            "below the range: its bottom"
        );
        assert_eq!(clamped_ui_text_size(1), 1);
    }

    #[gpui::test]
    fn ui_px_reads_the_offset_the_app_set(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            assert_eq!(ui_text_size(cx), 0, "nothing set: no offset");
            assert_eq!(ui_px(cx, 12.0), gpui::px(12.0));
            set_ui_text_size(2, cx);
            assert_eq!(ui_text_size(cx), 2);
            assert_eq!(ui_px(cx, 12.0), gpui::px(14.0));
            set_ui_text_size(7, cx);
            assert_eq!(ui_text_size(cx), 4, "clamped to the range");
        });
    }

    #[test]
    fn ui_weight_moves_by_the_shift_and_stops_at_700() {
        let at = |shift, requested| ui_weight_at(shift, FontWeight(requested)).0;
        for (requested, moved) in [
            (400.0, [400.0, 500.0, 600.0]),
            (600.0, [600.0, 700.0, 700.0]),
            (700.0, [700.0, 700.0, 700.0]),
        ] {
            assert_eq!(
                [at(0, requested), at(1, requested), at(2, requested)],
                moved,
                "{requested} at shifts 0, 1 and 2"
            );
        }
        assert_eq!(at(2, 900.0), 900.0, "the cap never lowers a weight");
    }

    #[gpui::test]
    fn ui_weight_reads_the_shift_the_app_set(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            assert_eq!(ui_weight(cx, FontWeight::NORMAL), FontWeight::NORMAL);
            set_ui_weight(600, cx);
            assert_eq!(ui_weight(cx, FontWeight::NORMAL), FontWeight::SEMIBOLD);
            set_ui_weight(450, cx);
            assert_eq!(
                ui_weight(cx, FontWeight::NORMAL),
                FontWeight::NORMAL,
                "a weight the setting does not offer: no shift"
            );
        });
    }

    #[gpui::test]
    fn setting_the_weight_keeps_the_text_size(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            set_ui_text_size(3, cx);
            set_ui_weight(500, cx);
            assert_eq!(ui_text_size(cx), 3, "the weight leaves the size alone");
            assert_eq!(ui_weight_shift(cx), 1);
            set_ui_text_size(-1, cx);
            assert_eq!(ui_weight_shift(cx), 1, "the size leaves the weight alone");
        });
    }

    #[test]
    fn ui_family_defaults_to_inter() {
        assert_eq!(DEFAULT_UI_FAMILY, "Inter");
        let calls = std::cell::Cell::new(0);
        assert_eq!(
            resolve_ui_family(None, counted(&["Arial"], &calls)),
            "Inter"
        );
        assert_eq!(calls.get(), 0, "the default lists no fonts");
        assert_eq!(
            UiState::default().ui_font_family,
            None,
            "nothing stored: the default"
        );
    }

    #[test]
    fn a_bundled_ui_family_resolves_without_listing_fonts() {
        let calls = std::cell::Cell::new(0);
        assert_eq!(
            resolve_ui_family(Some("inter"), counted(&["Arial"], &calls)),
            "Inter",
            "matched without regard to case, named as bundled"
        );
        assert_eq!(calls.get(), 0, "the bundled families are registered");
    }

    #[test]
    fn an_unknown_ui_family_falls_back_to_the_default() {
        let calls = std::cell::Cell::new(0);
        assert_eq!(
            resolve_ui_family(Some("Nope Sans"), counted(&["Arial"], &calls)),
            "Inter"
        );
        assert_eq!(calls.get(), 1, "a stored family not bundled is looked up");
        assert_eq!(
            resolve_ui_family(Some("arial"), counted(&["Arial"], &calls)),
            "Arial",
            "matched without regard to case, named as installed"
        );
        assert_eq!(
            resolve_ui_family(Some("Nope Sans"), Vec::new),
            "Nope Sans",
            "nothing to check against: the request unchecked"
        );
    }

    /// The family names (`name` IDs 1 and 16) the face's Windows `name`
    /// records carry.
    fn name_table_families(data: &[u8]) -> Vec<String> {
        let tables = usize::from(be_u16(data, 4));
        let table = (0..tables)
            .map(|i| 12 + i * 16)
            .find(|&record| data.get(record..record + 4) == Some(b"name".as_slice()))
            .map(|record| be_u32(data, record + 8))
            .expect("a name table");
        let count = usize::from(be_u16(data, table + 2));
        let strings = table + usize::from(be_u16(data, table + 4));
        (0..count)
            .map(|i| table + 6 + i * 12)
            .filter(|&record| be_u16(data, record) == 3)
            .filter(|&record| matches!(be_u16(data, record + 6), 1 | 16))
            .map(|record| {
                let len = usize::from(be_u16(data, record + 8));
                let at = strings + usize::from(be_u16(data, record + 10));
                let bytes = data.get(at..at + len).expect("a name string in bounds");
                let units: Vec<u16> = bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| u16::from_be_bytes(*pair))
                    .collect();
                String::from_utf16_lossy(&units)
            })
            .collect()
    }

    #[test]
    fn bundled_ui_families_register() {
        for family in BUNDLED_UI_FAMILIES {
            let faces: Vec<&UiFace> = UI_FILES
                .iter()
                .filter(|face| face.family == family)
                .collect();
            let weights: Vec<u16> = faces.iter().map(|face| face.weight).collect();
            assert_eq!(weights, [400, 500, 600, 700], "{family}'s faces");
            for face in faces {
                assert!(
                    name_table_families(face.data)
                        .iter()
                        .any(|name| name == family),
                    "a {family} face names itself {family}: {:?}",
                    name_table_families(face.data)
                );
            }
        }
        assert!(
            UI_FILES
                .iter()
                .all(|face| BUNDLED_UI_FAMILIES.contains(&face.family)),
            "every UI face belongs to a listed family"
        );
    }

    #[test]
    fn stepping_stops_at_the_clamp_limits() {
        assert_eq!(stepped(MIN_SIZE, -1.0), None);
        assert_eq!(stepped(MAX_SIZE, 1.0), None);
        assert_eq!(stepped(MIN_SIZE, 1.0), Some(9.0));
        assert_eq!(stepped(MAX_SIZE, -1.0), Some(31.0));
        assert_eq!(stepped(40.0, 1.0), None, "clamped first, then stepped");
    }

    #[test]
    fn sizes_clamp_and_round() {
        assert!((clamp_size(13.4) - 13.0).abs() < f32::EPSILON);
        assert!((clamp_size(13.5) - 14.0).abs() < f32::EPSILON);
        assert!((clamp_size(2.0) - MIN_SIZE).abs() < f32::EPSILON);
        assert!((clamp_size(7.6) - MIN_SIZE).abs() < f32::EPSILON);
        assert!((clamp_size(40.0) - MAX_SIZE).abs() < f32::EPSILON);
        assert!((clamp_size(f32::INFINITY) - MAX_SIZE).abs() < f32::EPSILON);
        assert!((clamp_size(f32::NAN) - DEFAULT_SIZE).abs() < f32::EPSILON);
        let settings = FontSettings {
            size: 100.0,
            ..FontSettings::default()
        };
        assert!((settings.normalized().size - MAX_SIZE).abs() < f32::EPSILON);
    }

    #[test]
    fn requested_family_is_used_when_available() {
        let available = names(&["Consolas", "Fira Code", "Geist Mono"]);
        assert_eq!(resolve_family(Some("Fira Code"), &available), "Fira Code");
        assert_eq!(resolve_family(None, &available), "Geist Mono");
    }

    #[test]
    fn missing_family_falls_back_to_geist_then_cascadia() {
        let with_geist = names(&["Cascadia Mono", "Consolas", "Geist Mono"]);
        assert_eq!(resolve_family(Some("Nope"), &with_geist), "Geist Mono");
        let without_geist = names(&["Arial", "Cascadia Mono", "Consolas"]);
        assert_eq!(
            resolve_family(Some("Nope"), &without_geist),
            "Cascadia Mono"
        );
        assert_eq!(resolve_family(None, &without_geist), "Cascadia Mono");
    }

    #[test]
    fn empty_list_returns_the_request_unchecked() {
        assert_eq!(resolve_family(Some("Nope"), &[]), "Nope");
        assert_eq!(resolve_family(None, &[]), "Geist Mono");
    }

    #[test]
    fn ui_state_round_trips_the_terminal_font() {
        let state = UiState {
            terminal_font: FontSettings {
                family: Some("Fira Code".to_owned()),
                size: 16.0,
                bold: true,
            },
            ..UiState::default()
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let back: UiState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, state);

        let older: UiState = serde_json::from_str(
            r#"{ "sidebar_width": 300.0, "sidebar_collapsed": true, "collapsed_containers": [] }"#,
        )
        .expect("a file from before the terminal font was saved");
        assert_eq!(older.terminal_font, FontSettings::default());
        assert!(older.sidebar_collapsed);

        let partial: UiState = serde_json::from_str(r#"{ "terminal_font": { "size": 20.0 } }"#)
            .expect("a font with only its size");
        assert_eq!(partial.terminal_font.family, None);
        assert!(!partial.terminal_font.bold);
    }

    #[test]
    fn ui_state_round_trips_the_tab_font_sizes() {
        let mut state = UiState::default();
        state.tab_font_sizes.insert("t1".to_owned(), 20.0);
        state.tab_font_sizes.insert("t2".to_owned(), 9.0);
        let json = serde_json::to_string(&state).expect("serialize");
        let back: UiState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, state);

        let partial: UiState = serde_json::from_str(r#"{ "tab_font_sizes": { "t9": 15.0 } }"#)
            .expect("a file holding only tab sizes");
        assert_eq!(
            partial,
            UiState {
                tab_font_sizes: [("t9".to_owned(), 15.0)].into(),
                ..UiState::default()
            },
            "only the tab sizes were saved; everything else is the default"
        );
    }

    #[test]
    fn bundled_fonts_are_truetype_or_opentype() {
        for face in BUNDLED_FILES {
            let tag = face.data.get(..4).expect("a font header");
            assert!(
                tag == [0, 1, 0, 0] || tag == b"OTTO" || tag == b"true",
                "not a TrueType/OpenType file: {tag:?}"
            );
        }
    }

    fn be_u16(data: &[u8], at: usize) -> u16 {
        let bytes = data.get(at..at + 2).expect("in bounds");
        u16::from_be_bytes([bytes[0], bytes[1]])
    }

    fn be_u32(data: &[u8], at: usize) -> usize {
        let bytes = data.get(at..at + 4).expect("in bounds");
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize
    }

    /// Whether the face's `OS/2` table sets the ITALIC bit of `fsSelection`.
    fn os2_italic(data: &[u8]) -> bool {
        let tables = usize::from(be_u16(data, 4));
        let os2 = (0..tables)
            .map(|i| 12 + i * 16)
            .find(|&record| data.get(record..record + 4) == Some(b"OS/2".as_slice()))
            .map(|record| be_u32(data, record + 8))
            .expect("an OS/2 table");
        be_u16(data, os2 + 62) & 1 == 1
    }

    /// The face's `OS/2` `usWeightClass`.
    fn os2_weight(data: &[u8]) -> u16 {
        let tables = usize::from(be_u16(data, 4));
        let os2 = (0..tables)
            .map(|i| 12 + i * 16)
            .find(|&record| data.get(record..record + 4) == Some(b"OS/2".as_slice()))
            .map(|record| be_u32(data, record + 8))
            .expect("an OS/2 table");
        be_u16(data, os2 + 4)
    }

    #[test]
    fn bundled_faces_carry_their_italic_style() {
        for (i, face) in BUNDLED_FILES.iter().enumerate() {
            assert_eq!(os2_italic(face.data), face.italic, "face {i}");
        }
    }

    #[test]
    fn ui_faces_are_truetype_or_opentype() {
        for face in UI_FILES {
            let tag = face.data.get(..4).expect("a font header");
            assert!(
                tag == [0, 1, 0, 0] || tag == b"OTTO" || tag == b"true",
                "not a TrueType/OpenType file: {tag:?}"
            );
        }
    }

    #[test]
    fn ui_faces_carry_their_declared_weight() {
        for (i, face) in UI_FILES.iter().enumerate() {
            assert_eq!(os2_weight(face.data), face.weight, "face {i}");
        }
    }
}
