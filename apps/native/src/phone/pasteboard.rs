//! The phone's pasteboard, for the image the composer's Paste uploads.
//!
//! On iOS it is `UIPasteboard` (`pasteboard_ios.rs`): [`PhonePasteboard::has_image`]
//! and [`PhonePasteboard::has_text`] are the probes that raise no "Allow
//! Paste" prompt, and [`PhonePasteboard::image`] reads the bytes, only on
//! the tap. Every other platform's pasteboard holds no image, so Paste pastes
//! text as it always did, and [`FakePasteboard`] lets the UI specs hold one.

use std::sync::{Mutex, PoisonError};

use gpui::ImageFormat;

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
    /// An image, loaded as the type of `format`.
    Image(ImageFormat),
    /// Text.
    Text,
    /// Neither: the paste changes nothing.
    Nothing,
}

/// What an item offering `type_ids` pastes as: an image wins over text (a
/// Photos copy offers its JPEG beside other types), PNG over JPEG; a bare
/// `public.image` is loaded as JPEG. Text is any of the plain text types.
#[cfg_attr(
    not(any(target_os = "ios", test)),
    expect(dead_code, reason = "only the iOS paste control reads an item's types")
)]
pub(crate) fn paste_kind(type_ids: &[&str]) -> PasteKind {
    let offers = |kind: &str| type_ids.contains(&kind);
    if offers(PNG_TYPE) {
        PasteKind::Image(ImageFormat::Png)
    } else if offers(JPEG_TYPE) || offers(IMAGE_TYPE) {
        PasteKind::Image(ImageFormat::Jpeg)
    } else if TEXT_TYPES.iter().any(|kind| offers(kind)) {
        PasteKind::Text
    } else {
        PasteKind::Nothing
    }
}

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
    use gpui::ImageFormat;

    use super::{PasteKind, paste_kind, pasted_name};

    #[test]
    fn a_pasted_image_takes_the_desktop_paste_name() {
        assert_eq!(pasted_name(ImageFormat::Png), "paste.png");
        assert_eq!(pasted_name(ImageFormat::Jpeg), "paste.jpg");
    }

    #[test]
    fn a_photos_copy_pastes_as_its_jpeg() {
        assert_eq!(
            paste_kind(&[
                "public.jpeg",
                "com.apple.mobileslideshow.asset.localidentifier"
            ]),
            PasteKind::Image(ImageFormat::Jpeg)
        );
    }

    #[test]
    fn an_image_wins_over_text() {
        assert_eq!(
            paste_kind(&["public.png", "public.utf8-plain-text"]),
            PasteKind::Image(ImageFormat::Png)
        );
    }

    #[test]
    fn the_plain_text_types_paste_as_text() {
        assert_eq!(
            paste_kind(&["public.utf8-plain-text", "public.plain-text", "public.text"]),
            PasteKind::Text
        );
    }

    #[test]
    fn a_bare_image_is_loaded_as_jpeg() {
        assert_eq!(
            paste_kind(&["public.image"]),
            PasteKind::Image(ImageFormat::Jpeg)
        );
    }

    #[test]
    fn an_item_with_no_known_type_pastes_nothing() {
        assert_eq!(paste_kind(&[]), PasteKind::Nothing);
        assert_eq!(paste_kind(&["com.example.other"]), PasteKind::Nothing);
    }
}
