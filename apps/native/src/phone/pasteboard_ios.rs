//! The phone's pasteboard on iOS: `UIPasteboard`'s general pasteboard.
//!
//! `hasImages` and `hasStrings` only probe, so they raise no "Allow Paste"
//! prompt; the bytes are read by [`IosPasteboard::image`] alone, which the
//! composer calls on the Paste tap. A PNG or JPEG is taken as it is; any
//! other image (a HEIC photo, a TIFF) is re-encoded as PNG. An image over
//! the upload cap is scaled down first ([`super::photos_ios::fitted`]).

use gpui::ImageFormat;
use objc2_foundation::NSString;
use objc2_ui_kit::UIPasteboard;

use super::pasteboard::{PastedImage, PhonePasteboard};
use super::photos_ios::fitted;

/// The pasteboard types taken as they are, in order of preference.
const TAKEN_AS_IS: [(&str, ImageFormat); 2] = [
    ("public.png", ImageFormat::Png),
    ("public.jpeg", ImageFormat::Jpeg),
];

/// The iOS pasteboard.
pub struct IosPasteboard;

impl PhonePasteboard for IosPasteboard {
    fn has_image(&self) -> bool {
        // SAFETY: the general pasteboard is read on the app's main thread,
        // where the composer's tap and render run.
        unsafe { UIPasteboard::generalPasteboard().hasImages() }
    }

    fn has_text(&self) -> bool {
        // SAFETY: as in `has_image`.
        unsafe { UIPasteboard::generalPasteboard().hasStrings() }
    }

    fn image(&self) -> Option<PastedImage> {
        let board = UIPasteboard::generalPasteboard();
        for (kind, format) in TAKEN_AS_IS {
            if let Some(data) = board.dataForPasteboardType(&NSString::from_str(kind)) {
                tracing::info!(kind, bytes = data.length(), "phone: pasteboard image read");
                return Some(fitted(data.to_vec(), format));
            }
        }
        // SAFETY: as in `has_image`.
        let image = unsafe { board.image() };
        let Some(png) = image.and_then(|image| image.png_representation()) else {
            tracing::info!("phone: the pasteboard holds no readable image");
            return None;
        };
        tracing::info!(
            bytes = png.length(),
            "phone: pasteboard image re-encoded as PNG"
        );
        Some(fitted(png.to_vec(), ImageFormat::Png))
    }
}
