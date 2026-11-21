//! The phone's pasteboard on iOS: `UIPasteboard`'s general pasteboard.
//!
//! `hasImages`, `hasStrings` and the board's type list only probe, so they
//! raise no "Allow Paste" alert; the bytes are read by
//! [`IosPasteboard::image`] alone, which the composer calls on the Paste
//! tap, and read once: the type list picks the best image type, and only
//! that type is read. A PNG or JPEG is taken as it is; any other image (a
//! HEIC photo, a TIFF) is read as a `UIImage` and re-encoded as PNG. An
//! image over the upload cap is scaled down first
//! ([`super::photos_ios::fitted`]).

use gpui::ImageFormat;
use objc2_foundation::NSString;
use objc2_ui_kit::UIPasteboard;

use super::pasteboard::{
    ImageSource, JPEG_TYPE, PNG_TYPE, PastedImage, PhonePasteboard, source_from_types,
};
use super::photos_ios::fitted;

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
        // Reading the board's type list raises no "Allow Paste" alert, so
        // the read below is the paste's only one. The list is the first
        // item's types; one naming no image type still reads as any other
        // image, since `has_image` has already seen that the board holds one.
        let types: Vec<String> = {
            // SAFETY: as in `has_image`.
            let listed = unsafe { board.pasteboardTypes() };
            listed.iter().map(|kind| kind.to_string()).collect()
        };
        let Some((kind, format)) = taken_as_is(source_from_types(&types)) else {
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
            return Some(fitted(png.to_vec(), ImageFormat::Png));
        };
        let Some(data) = board.dataForPasteboardType(&NSString::from_str(kind)) else {
            tracing::info!(kind, "phone: the pasteboard's image type holds no data");
            return None;
        };
        tracing::info!(kind, bytes = data.length(), "phone: pasteboard image read");
        Some(fitted(data.to_vec(), format))
    }
}

/// The type identifier and format an image the board takes as it is comes
/// as: the PNG or the JPEG itself; `None` for any other image, which is read
/// as a `UIImage`.
fn taken_as_is(source: ImageSource) -> Option<(&'static str, ImageFormat)> {
    match source {
        ImageSource::Png => Some((PNG_TYPE, ImageFormat::Png)),
        ImageSource::Jpeg => Some((JPEG_TYPE, ImageFormat::Jpeg)),
        ImageSource::Other => None,
    }
}
