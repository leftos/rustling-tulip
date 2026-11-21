//! The phone's pasteboard, for the image the composer's Paste uploads.
//!
//! On iOS it is `UIPasteboard` (`pasteboard_ios.rs`): [`PhonePasteboard::has_image`]
//! and [`PhonePasteboard::has_text`] are the probes that raise no "Allow
//! Paste" prompt, and [`PhonePasteboard::image`] reads the bytes, only on
//! the tap. Every other platform's pasteboard holds no image, so Paste pastes
//! text as it always did, and [`FakePasteboard`] lets the UI specs hold one.

use std::sync::{Mutex, PoisonError};

use gpui::{Bounds, ImageFormat, Pixels, px};

use crate::paste_upload::extension_of;

/// An image off the pasteboard: its bytes and their format.
pub type PastedImage = (Vec<u8>, ImageFormat);

/// What the phone's platform offers of its pasteboard beyond text.
pub trait PhonePasteboard: Send + Sync {
    /// Whether the pasteboard holds an image, asked without reading it.
    fn has_image(&self) -> bool;
    /// Whether the pasteboard holds text, asked without reading it.
    fn has_text(&self) -> bool;
    /// The pasteboard's image, read now; `None` when it holds none.
    fn image(&self) -> Option<PastedImage>;
}

/// A platform whose pasteboard holds no image: Paste pastes text.
pub struct LoggingPasteboard;

impl PhonePasteboard for LoggingPasteboard {
    fn has_image(&self) -> bool {
        false
    }

    fn has_text(&self) -> bool {
        true
    }

    fn image(&self) -> Option<PastedImage> {
        tracing::info!("phone: no pasteboard image on this platform");
        None
    }
}

/// A pasteboard for specs: holds the image and the text it was made with,
/// and counts the image's reads.
pub struct FakePasteboard {
    image: Option<PastedImage>,
    text: bool,
    reads: Mutex<usize>,
}

impl FakePasteboard {
    /// A pasteboard holding `image` and, when `text`, some text.
    #[must_use]
    pub fn new(image: Option<PastedImage>, text: bool) -> Self {
        Self {
            image,
            text,
            reads: Mutex::new(0),
        }
    }

    /// How many times the image was read.
    #[must_use]
    pub fn reads(&self) -> usize {
        *self.reads.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl PhonePasteboard for FakePasteboard {
    fn has_image(&self) -> bool {
        self.image.is_some()
    }

    fn has_text(&self) -> bool {
        self.text
    }

    fn image(&self) -> Option<PastedImage> {
        *self.reads.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        self.image.clone()
    }
}

/// The name a pasted image of `format` uploads under: the desktop paste's,
/// `paste.<ext>`.
pub(super) fn pasted_name(format: ImageFormat) -> String {
    format!("paste.{}", extension_of(format))
}

/// What the system paste control's item offers, read from its type
/// identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(any(target_os = "ios", test)),
    expect(dead_code, reason = "only the iOS paste control reads an item's types")
)]
pub(crate) enum PasteKind {
    /// An image, loaded as the type its source names.
    Image(ImageSource),
    /// Text.
    Text,
    /// Neither: the paste changes nothing.
    Nothing,
}

/// The type an item's image is loaded as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(any(target_os = "ios", test)),
    expect(dead_code, reason = "only the iOS paste control reads an item's types")
)]
pub(crate) enum ImageSource {
    /// A PNG, uploaded as itself.
    Png,
    /// A JPEG, uploaded as itself.
    Jpeg,
    /// Any other image (HEIC, TIFF, GIF, ...), loaded as `public.image` and
    /// re-encoded to JPEG.
    Other,
}

#[cfg_attr(
    not(any(target_os = "ios", test)),
    expect(dead_code, reason = "only the iOS paste control reads an item's types")
)]
impl ImageSource {
    /// The type identifier the image is loaded as.
    pub(crate) const fn type_id(self) -> &'static str {
        match self {
            Self::Png => PNG_TYPE,
            Self::Jpeg => JPEG_TYPE,
            Self::Other => IMAGE_TYPE,
        }
    }
}

/// What an item offering `offered`, whose types `conforms` to the type
/// identifiers it is asked about, pastes as on a host that takes uploads
/// when `uploads`. An image is any item conforming to `public.image`: PNG
/// over JPEG over any other. It wins over text (a Photos copy offers its
/// JPEG beside other types) where the host takes uploads; where it takes
/// none, an item with text pastes its text and an image alone pastes
/// nothing. Text is any of the plain text types, offered or conformed to.
#[cfg_attr(
    not(any(target_os = "ios", test)),
    expect(dead_code, reason = "only the iOS paste control reads an item's types")
)]
pub(crate) fn paste_kind(
    offered: &[&str],
    conforms: impl Fn(&str) -> bool,
    uploads: bool,
) -> PasteKind {
    let image = if conforms(PNG_TYPE) {
        Some(ImageSource::Png)
    } else if conforms(JPEG_TYPE) {
        Some(ImageSource::Jpeg)
    } else if conforms(IMAGE_TYPE) {
        Some(ImageSource::Other)
    } else {
        None
    };
    let text = TEXT_TYPES
        .iter()
        .any(|kind| offered.contains(kind) || conforms(kind));
    match image {
        Some(source) if uploads => PasteKind::Image(source),
        _ if text => PasteKind::Text,
        _ => PasteKind::Nothing,
    }
}

/// Where a system paste control sits, and so where its paste goes: each
/// spot has a control of its own, so the composer's Paste and the Keys
/// dock's, on screen together, each have one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PasteSpot {
    /// The composer's Paste button: text in at the caret, or an image
    /// uploaded.
    Composer,
    /// The Keys dock's Paste key: pastes as the composer's Paste does.
    Dock,
    /// The terminal key bar's Paste key: text pasted into the terminal.
    Terminal,
}

#[cfg_attr(
    not(any(target_os = "ios", test)),
    expect(dead_code, reason = "only the iOS paste control places itself")
)]
impl PasteSpot {
    /// The type identifiers the control offers to paste here: text and
    /// images at the composer and the dock, text only at the terminal.
    pub(crate) const fn accepts(self) -> &'static [&'static str] {
        match self {
            Self::Composer | Self::Dock => &COMPOSER_ACCEPTS,
            Self::Terminal => &TERMINAL_ACCEPTS,
        }
    }

    /// The session a paste here goes to: the shown session's composer (from
    /// its Paste or the dock's), or the session the open terminal shows.
    pub(crate) fn session(self, shown: Option<String>, terminal: Option<String>) -> Option<String> {
        match self {
            Self::Composer | Self::Dock => shown,
            Self::Terminal => terminal,
        }
    }

    /// Whether an image pastes here, the host taking uploads when
    /// `uploads`: only into the composer.
    pub(crate) const fn takes_images(self, uploads: bool) -> bool {
        matches!(self, Self::Composer | Self::Dock) && uploads
    }

    /// Whether the control goes over this spot drawn at `bounds` inside the
    /// visible `clip`: the composer's button and the dock's key always,
    /// the key bar's key only while [`spot_fully_visible`] (its bar scrolls
    /// sideways).
    pub(crate) fn shown_in(self, bounds: Bounds<Pixels>, clip: Bounds<Pixels>) -> bool {
        match self {
            Self::Composer | Self::Dock => true,
            Self::Terminal => spot_fully_visible(bounds, clip),
        }
    }
}

/// How far, in points, a spot's edge may sit outside its clip and still
/// count as inside. The key's bounds and its clip are the same rectangle
/// reached two ways in floats (the canvas inside the key's border, and the
/// key's own mask inset by that border), which at a fractional scale differ
/// by a float step; a key scrolled partly out is out by far more.
const CLIP_SLACK: f32 = 0.1;

/// Whether `spot` lies wholly inside `clip`, edges included and within
/// [`CLIP_SLACK`]: any part further outside, a spot touching the clip's
/// edge from outside among them, is not.
#[cfg_attr(
    not(any(target_os = "ios", test)),
    expect(dead_code, reason = "only the iOS paste control places itself")
)]
pub(crate) fn spot_fully_visible(spot: Bounds<Pixels>, clip: Bounds<Pixels>) -> bool {
    let slack = px(CLIP_SLACK);
    spot.origin.x >= clip.origin.x - slack
        && spot.origin.y >= clip.origin.y - slack
        && spot.right() <= clip.right() + slack
        && spot.bottom() <= clip.bottom() + slack
}

/// Whether placing the control must unhide it, it being up already when
/// `shown` and made anew (hidden, as every new control is) when `remade`.
#[cfg_attr(
    not(any(target_os = "ios", test)),
    expect(dead_code, reason = "only the iOS paste control places itself")
)]
pub(crate) const fn must_unhide(shown: bool, remade: bool) -> bool {
    !shown || remade
}

/// The spots of `up`, whose control is up, that were not `drawn` (with a
/// session to paste into) this frame: each hides its control, while every
/// drawn spot keeps its own.
#[cfg_attr(
    not(any(target_os = "ios", test)),
    expect(dead_code, reason = "only the iOS paste control places itself")
)]
pub(crate) fn spots_to_hide(up: &[PasteSpot], drawn: &[PasteSpot]) -> Vec<PasteSpot> {
    up.iter()
        .copied()
        .filter(|spot| !drawn.contains(spot))
        .collect()
}

/// The type identifiers the composer's and the dock's controls offer to
/// paste.
const COMPOSER_ACCEPTS: [&str; 5] = [
    TEXT_TYPES[0],
    TEXT_TYPES[1],
    PNG_TYPE,
    JPEG_TYPE,
    IMAGE_TYPE,
];
/// The type identifiers the terminal key bar's control offers to paste.
const TERMINAL_ACCEPTS: [&str; 2] = [TEXT_TYPES[0], TEXT_TYPES[1]];

/// A PNG's type identifier.
pub(crate) const PNG_TYPE: &str = "public.png";
/// A JPEG's type identifier.
pub(crate) const JPEG_TYPE: &str = "public.jpeg";
/// Any image's type identifier.
pub(crate) const IMAGE_TYPE: &str = "public.image";
/// The text type identifiers an item's text is read as, the loaded one first.
pub(crate) const TEXT_TYPES: [&str; 3] =
    ["public.utf8-plain-text", "public.plain-text", "public.text"];

#[cfg(test)]
mod tests {
    use gpui::{Bounds, ImageFormat, Pixels, point, px, size};

    use super::{
        IMAGE_TYPE, ImageSource, PasteKind, PasteSpot, must_unhide, paste_kind, pasted_name,
        spot_fully_visible, spots_to_hide,
    };

    /// The image types the tests' items offer, each conforming to
    /// `public.image` as the system's type tree has it.
    const IMAGES: [&str; 6] = [
        "public.png",
        "public.jpeg",
        "public.heic",
        "public.tiff",
        "com.compuserve.gif",
        "public.image",
    ];

    /// What an item offering `offered` pastes as on a host that takes
    /// uploads when `uploads`: each offered type conforms to itself, and an
    /// image type to `public.image`.
    fn kind_of(offered: &[&str], uploads: bool) -> PasteKind {
        let conforms = |kind: &str| {
            offered.contains(&kind)
                || (kind == IMAGE_TYPE && offered.iter().any(|offer| IMAGES.contains(offer)))
        };
        paste_kind(offered, conforms, uploads)
    }

    #[test]
    fn a_pasted_image_takes_the_desktop_paste_name() {
        assert_eq!(pasted_name(ImageFormat::Png), "paste.png");
        assert_eq!(pasted_name(ImageFormat::Jpeg), "paste.jpg");
    }

    #[test]
    fn a_photos_copy_pastes_as_its_jpeg() {
        assert_eq!(
            kind_of(
                &[
                    "public.jpeg",
                    "com.apple.mobileslideshow.asset.localidentifier"
                ],
                true
            ),
            PasteKind::Image(ImageSource::Jpeg)
        );
    }

    #[test]
    fn an_image_wins_over_text_when_the_host_takes_uploads() {
        assert_eq!(
            kind_of(&["public.png", "public.utf8-plain-text"], true),
            PasteKind::Image(ImageSource::Png)
        );
    }

    #[test]
    fn the_plain_text_types_paste_as_text() {
        assert_eq!(
            kind_of(
                &["public.utf8-plain-text", "public.plain-text", "public.text"],
                true
            ),
            PasteKind::Text
        );
    }

    #[test]
    fn a_bare_image_is_loaded_as_the_bare_image_type() {
        assert_eq!(
            kind_of(&["public.image"], true),
            PasteKind::Image(ImageSource::Other)
        );
    }

    #[test]
    fn a_heic_item_loads_as_the_bare_image_type() {
        assert_eq!(
            kind_of(&["public.heic"], true),
            PasteKind::Image(ImageSource::Other)
        );
        assert_eq!(ImageSource::Other.type_id(), "public.image");
        assert_eq!(ImageSource::Png.type_id(), "public.png");
        assert_eq!(ImageSource::Jpeg.type_id(), "public.jpeg");
    }

    #[test]
    fn a_tiff_and_gif_item_paste_as_images() {
        assert_eq!(
            kind_of(&["public.tiff"], true),
            PasteKind::Image(ImageSource::Other)
        );
        assert_eq!(
            kind_of(&["com.compuserve.gif"], true),
            PasteKind::Image(ImageSource::Other)
        );
    }

    #[test]
    fn an_image_and_text_item_pastes_text_when_the_host_takes_no_uploads() {
        assert_eq!(
            kind_of(&["public.png", "public.utf8-plain-text"], false),
            PasteKind::Text
        );
    }

    #[test]
    fn an_image_only_item_pastes_nothing_when_the_host_takes_no_uploads() {
        assert_eq!(kind_of(&["public.png"], false), PasteKind::Nothing);
        assert_eq!(kind_of(&["public.heic"], false), PasteKind::Nothing);
    }

    /// A spot `width` wide at `x`, a key bar's key tall.
    fn key_at(x: f32, width: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(0.0)), size(px(width), px(44.0)))
    }

    #[test]
    fn a_paste_key_scrolled_partly_out_of_the_bar_is_not_covered() {
        let clip = key_at(0.0, 300.0);
        let rows = [
            ("fully inside", key_at(10.0, 44.0), true),
            ("filling the clip", key_at(0.0, 300.0), true),
            ("partly out on the left", key_at(-10.0, 44.0), false),
            ("partly out on the right", key_at(280.0, 44.0), false),
            ("wholly out", key_at(310.0, 44.0), false),
            (
                "touching the left edge from outside",
                key_at(-44.0, 44.0),
                false,
            ),
            (
                "touching the right edge from outside",
                key_at(300.0, 44.0),
                false,
            ),
            (
                "the clip off by a float step on the left and top",
                Bounds::new(point(px(-4e-6), px(-4e-6)), size(px(300.0), px(44.0))),
                true,
            ),
            (
                "the clip off by a float step on the right and bottom",
                Bounds::new(
                    point(px(0.0), px(0.0)),
                    size(px(300.0_f32.next_up()), px(44.0 + 4e-6)),
                ),
                true,
            ),
            ("cut by 1 pt on the right", key_at(0.0, 301.0), false),
        ];
        for (row, spot, shown) in rows {
            assert_eq!(spot_fully_visible(spot, clip), shown, "{row}");
            assert_eq!(PasteSpot::Terminal.shown_in(spot, clip), shown, "{row}");
        }
        assert!(PasteSpot::Composer.shown_in(key_at(-10.0, 44.0), clip));
    }

    #[test]
    fn the_terminal_spot_accepts_text_only() {
        assert_eq!(
            PasteSpot::Terminal.accepts(),
            ["public.utf8-plain-text", "public.plain-text"]
        );
        assert_eq!(
            PasteSpot::Composer.accepts(),
            [
                "public.utf8-plain-text",
                "public.plain-text",
                "public.png",
                "public.jpeg",
                "public.image"
            ]
        );
    }

    #[test]
    fn each_spot_pastes_into_its_own_page() {
        let shown = Some("chat".to_owned());
        let terminal = Some("term".to_owned());
        assert_eq!(
            PasteSpot::Composer.session(shown.clone(), terminal.clone()),
            shown
        );
        assert_eq!(
            PasteSpot::Terminal.session(shown.clone(), terminal.clone()),
            terminal
        );
        assert!(PasteSpot::Composer.takes_images(true));
        assert!(!PasteSpot::Composer.takes_images(false));
        assert!(!PasteSpot::Terminal.takes_images(true));
        assert!(!PasteSpot::Terminal.takes_images(false));
    }

    #[test]
    fn an_image_and_text_item_pastes_text_into_the_terminal() {
        let uploads = PasteSpot::Terminal.takes_images(true);
        assert_eq!(
            kind_of(&["public.png", "public.utf8-plain-text"], uploads),
            PasteKind::Text
        );
        assert_eq!(kind_of(&["public.png"], uploads), PasteKind::Nothing);
    }

    #[test]
    fn a_remade_paste_control_is_unhidden_while_shown() {
        assert!(must_unhide(false, false), "shown anew");
        assert!(must_unhide(false, true), "made and shown anew");
        assert!(!must_unhide(true, false), "still shown, same control");
        assert!(
            must_unhide(true, true),
            "still shown, control remade hidden"
        );
    }

    #[test]
    fn the_dock_spot_pastes_like_the_composer() {
        let dock = PasteSpot::Dock;
        let composer = PasteSpot::Composer;
        assert_eq!(dock.accepts(), composer.accepts());
        let shown = Some("chat".to_owned());
        let terminal = Some("term".to_owned());
        assert_eq!(
            dock.session(shown.clone(), terminal.clone()),
            composer.session(shown, terminal)
        );
        for uploads in [true, false] {
            assert_eq!(
                dock.takes_images(uploads),
                composer.takes_images(uploads),
                "{uploads}"
            );
        }
        let clip = Bounds::new(point(px(0.0), px(0.0)), size(px(100.0), px(50.0)));
        let past_clip = Bounds::new(point(px(-10.0), px(0.0)), size(px(44.0), px(44.0)));
        assert_eq!(
            dock.shown_in(past_clip, clip),
            composer.shown_in(past_clip, clip)
        );
    }

    #[test]
    fn two_spots_drawn_keep_two_controls() {
        let both = [PasteSpot::Composer, PasteSpot::Dock];
        assert_eq!(spots_to_hide(&both, &both), Vec::<PasteSpot>::new());
        assert_eq!(
            spots_to_hide(&[PasteSpot::Composer], &both),
            Vec::<PasteSpot>::new(),
            "a spot drawn anew hides no other"
        );
    }

    #[test]
    fn a_spot_not_drawn_hides_its_control() {
        let up = [PasteSpot::Composer, PasteSpot::Dock];
        assert_eq!(
            spots_to_hide(&up, &[PasteSpot::Composer]),
            [PasteSpot::Dock]
        );
        assert_eq!(
            spots_to_hide(&up, &[]),
            up,
            "nothing drawn hides every control"
        );
    }

    #[test]
    fn an_item_with_no_known_type_pastes_nothing() {
        assert_eq!(kind_of(&[], true), PasteKind::Nothing);
        assert_eq!(kind_of(&["com.example.other"], true), PasteKind::Nothing);
    }
}
