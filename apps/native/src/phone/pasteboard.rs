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

/// The image source among the types an item offers, `conforms` answering
/// whether it conforms to a type identifier: PNG over JPEG over any other
/// image, and `None` when it offers no image. `conforms` sees the type tree,
/// so a HEIC conforms to `public.image`.
#[cfg_attr(
    not(any(target_os = "ios", test)),
    expect(dead_code, reason = "only the iOS paste paths choose an image source")
)]
pub(crate) fn image_source(conforms: impl Fn(&str) -> bool) -> Option<ImageSource> {
    if conforms(PNG_TYPE) {
        Some(ImageSource::Png)
    } else if conforms(JPEG_TYPE) {
        Some(ImageSource::Jpeg)
    } else if conforms(IMAGE_TYPE) {
        Some(ImageSource::Other)
    } else {
        None
    }
}

/// The image source a pasteboard that lists `types` reads as: PNG over JPEG
/// over any other image, and any list naming neither as
/// [`ImageSource::Other`], the read that accepts whatever image the board
/// holds. The caller has already seen that the board holds an image.
#[cfg_attr(
    not(any(target_os = "ios", test)),
    expect(dead_code, reason = "only the iOS paste paths choose an image source")
)]
pub(crate) fn source_from_types(types: &[String]) -> ImageSource {
    image_source(|kind| types.iter().any(|listed| listed == kind)).unwrap_or(ImageSource::Other)
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
    let image = image_source(&conforms);
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
    /// The composer's edit bar's Paste item: pastes as the composer's
    /// Paste does, over the selection, and closes the bar.
    #[cfg_attr(
        not(any(target_os = "ios", test)),
        expect(dead_code, reason = "only the iOS paste control places itself")
    )]
    EditBar,
    /// The terminal key bar's Paste key: text pasted into the terminal.
    Terminal,
}

#[cfg_attr(
    not(any(target_os = "ios", test)),
    expect(dead_code, reason = "only the iOS paste control places itself")
)]
impl PasteSpot {
    /// The type identifiers the control offers to paste here: text and
    /// images at the composer, the dock and the edit bar, text only at the
    /// terminal.
    pub(crate) const fn accepts(self) -> &'static [&'static str] {
        match self {
            Self::Composer | Self::Dock | Self::EditBar => &COMPOSER_ACCEPTS,
            Self::Terminal => &TERMINAL_ACCEPTS,
        }
    }

    /// The session a paste here goes to: the shown session's composer (from
    /// its Paste, the dock's or its edit bar's), or the session the open
    /// terminal shows.
    pub(crate) fn session(self, shown: Option<String>, terminal: Option<String>) -> Option<String> {
        match self {
            Self::Composer | Self::Dock | Self::EditBar => shown,
            Self::Terminal => terminal,
        }
    }

    /// Whether an image pastes here, the host taking uploads when
    /// `uploads`: only into the composer.
    pub(crate) const fn takes_images(self, uploads: bool) -> bool {
        matches!(self, Self::Composer | Self::Dock | Self::EditBar) && uploads
    }

    /// Whether the control goes over this spot drawn at `bounds` inside the
    /// visible `clip`: the composer's button, the dock's key and the edit
    /// bar's item always, the key bar's key only while
    /// [`spot_fully_visible`] (its bar scrolls sideways).
    pub(crate) fn shown_in(self, bounds: Bounds<Pixels>, clip: Bounds<Pixels>) -> bool {
        match self {
            Self::Composer | Self::Dock | Self::EditBar => true,
            Self::Terminal => spot_fully_visible(bounds, clip),
        }
    }

    /// Whether a paste here closes the composer's edit bar, as the bar's own
    /// Paste does: only from the bar.
    pub(crate) const fn closes_edit_bar(self) -> bool {
        matches!(self, Self::EditBar)
    }

    /// Whether the control here takes the popover's colours, as the edit
    /// bar it sits in draws on one; every other spot takes the page's.
    pub(crate) const fn on_popover(self) -> bool {
        matches!(self, Self::EditBar)
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

/// The spots of `controls`, each placed at its bounds, whose control lies
/// under one of `covers`, the areas touch editing draws over the window
/// (the edit bar and each selection handle's touch square): the native
/// control sits above every GPUI view and would take the cover's taps, so
/// each hides for the frame. The edit bar's own control, over its Paste
/// item, is never hidden; edges that only touch do not cover.
#[cfg_attr(
    not(any(target_os = "ios", test)),
    expect(dead_code, reason = "only the iOS paste control places itself")
)]
pub(crate) fn covered_spots(
    controls: &[(PasteSpot, Bounds<Pixels>)],
    covers: &[Bounds<Pixels>],
) -> Vec<PasteSpot> {
    let overlaps = |a: &Bounds<Pixels>, b: &Bounds<Pixels>| {
        a.origin.x < b.right()
            && b.origin.x < a.right()
            && a.origin.y < b.bottom()
            && b.origin.y < a.bottom()
    };
    controls
        .iter()
        .filter(|(spot, bounds)| {
            *spot != PasteSpot::EditBar && covers.iter().any(|cover| overlaps(bounds, cover))
        })
        .map(|(spot, _)| *spot)
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
        IMAGE_TYPE, ImageSource, PasteKind, PasteSpot, covered_spots, image_source, must_unhide,
        paste_kind, pasted_name, source_from_types, spot_fully_visible, spots_to_hide,
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

    /// Whether an item offering `offered` conforms to the type identifier
    /// `kind`: each offered type to itself, and an image type to
    /// `public.image`, as the system's type tree has it.
    fn conforms_of<'a>(offered: &'a [&str]) -> impl Fn(&str) -> bool + 'a {
        move |kind: &str| {
            offered.contains(&kind)
                || (kind == IMAGE_TYPE && offered.iter().any(|offer| IMAGES.contains(offer)))
        }
    }

    /// The image source an item offering `offered` reads as.
    fn source_of(offered: &[&str]) -> Option<ImageSource> {
        image_source(conforms_of(offered))
    }

    /// What an item offering `offered` pastes as on a host that takes
    /// uploads when `uploads`.
    fn kind_of(offered: &[&str], uploads: bool) -> PasteKind {
        paste_kind(offered, conforms_of(offered), uploads)
    }

    #[test]
    fn image_source_prefers_png_then_jpeg_then_any_image() {
        assert_eq!(
            source_of(&["public.png", "public.jpeg"]),
            Some(ImageSource::Png)
        );
        assert_eq!(source_of(&["public.jpeg"]), Some(ImageSource::Jpeg));
        assert_eq!(source_of(&[IMAGE_TYPE]), Some(ImageSource::Other));
        assert_eq!(
            source_of(&["public.tiff", "public.png"]),
            Some(ImageSource::Png)
        );
    }

    #[test]
    fn image_source_is_none_without_an_image_type() {
        assert_eq!(source_of(&["public.utf8-plain-text"]), None);
        assert_eq!(source_of(&[]), None);
    }

    #[test]
    fn heic_reads_through_the_image_path() {
        let source = source_of(&["public.heic"]);
        assert_eq!(source, Some(ImageSource::Other));
        assert_eq!(source.map(ImageSource::type_id), Some(IMAGE_TYPE));
    }

    #[test]
    fn source_from_types_falls_back_when_no_listed_type_is_known() {
        assert_eq!(
            source_from_types(&["com.compuserve.gif".to_owned()]),
            ImageSource::Other
        );
        assert_eq!(
            source_from_types(&["public.heic".to_owned()]),
            ImageSource::Other
        );
        assert_eq!(source_from_types(&[]), ImageSource::Other);
        assert_eq!(
            source_from_types(&["public.jpeg".to_owned(), "public.png".to_owned()]),
            ImageSource::Png
        );
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
    fn the_edit_bar_spot_pastes_like_the_composer() {
        let bar = PasteSpot::EditBar;
        let composer = PasteSpot::Composer;
        assert_eq!(bar.accepts(), composer.accepts());
        let shown = Some("chat".to_owned());
        let terminal = Some("term".to_owned());
        assert_eq!(
            bar.session(shown.clone(), terminal.clone()),
            composer.session(shown, terminal)
        );
        for uploads in [true, false] {
            assert_eq!(
                bar.takes_images(uploads),
                composer.takes_images(uploads),
                "{uploads}"
            );
        }
        let clip = Bounds::new(point(px(0.0), px(0.0)), size(px(100.0), px(50.0)));
        let past_clip = Bounds::new(point(px(-10.0), px(0.0)), size(px(44.0), px(44.0)));
        assert!(
            bar.shown_in(past_clip, clip),
            "the bar draws over every clip"
        );
        assert!(bar.closes_edit_bar(), "a paste from the bar closes it");
        for other in [PasteSpot::Composer, PasteSpot::Dock, PasteSpot::Terminal] {
            assert!(!other.closes_edit_bar(), "{other:?}");
        }
    }

    #[test]
    fn only_the_edit_bar_control_takes_the_popover_colours() {
        assert!(
            PasteSpot::EditBar.on_popover(),
            "the bar draws on a popover"
        );
        for page in [PasteSpot::Composer, PasteSpot::Dock, PasteSpot::Terminal] {
            assert!(!page.on_popover(), "{page:?} keeps the page's ground");
        }
    }

    /// A rectangle at `x`, `y` of `width` by `height`, in px.
    fn rect(x: f32, y: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
    }

    /// The edit bar, drawn over the dock's last row in these cases.
    fn edit_bar() -> Bounds<Pixels> {
        rect(40.0, 500.0, 300.0, 54.0)
    }

    /// The start handle's touch square.
    fn start_handle() -> Bounds<Pixels> {
        rect(100.0, 580.0, 44.0, 44.0)
    }

    #[test]
    fn a_dock_control_under_the_edit_bar_is_hidden() {
        let controls = [(PasteSpot::Dock, rect(250.0, 520.0, 80.0, 44.0))];
        assert_eq!(
            covered_spots(&controls, &[edit_bar(), start_handle()]),
            [PasteSpot::Dock]
        );
    }

    #[test]
    fn a_dock_control_under_a_handle_is_hidden() {
        let controls = [(PasteSpot::Dock, rect(60.0, 590.0, 80.0, 44.0))];
        assert_eq!(
            covered_spots(&controls, &[edit_bar(), start_handle()]),
            [PasteSpot::Dock]
        );
    }

    #[test]
    fn a_control_clear_of_the_bar_stays() {
        let controls = [
            (PasteSpot::Dock, rect(250.0, 400.0, 80.0, 44.0)),
            // The composer's Paste, below the field, touching the bar's
            // bottom edge only.
            (PasteSpot::Composer, rect(16.0, 700.0, 80.0, 44.0)),
            (PasteSpot::Terminal, rect(40.0, 554.0, 80.0, 20.0)),
        ];
        assert_eq!(
            covered_spots(&controls, &[edit_bar(), start_handle()]),
            Vec::<PasteSpot>::new()
        );
    }

    #[test]
    fn the_edit_bar_control_is_never_hidden_by_its_own_bar() {
        let controls = [
            (PasteSpot::EditBar, rect(200.0, 505.0, 60.0, 44.0)),
            (PasteSpot::Composer, rect(16.0, 700.0, 80.0, 44.0)),
        ];
        assert_eq!(
            covered_spots(&controls, &[edit_bar(), start_handle()]),
            Vec::<PasteSpot>::new()
        );
    }

    #[test]
    fn an_item_with_no_known_type_pastes_nothing() {
        assert_eq!(kind_of(&[], true), PasteKind::Nothing);
        assert_eq!(kind_of(&["com.example.other"], true), PasteKind::Nothing);
    }
}
