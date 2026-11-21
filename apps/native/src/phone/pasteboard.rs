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

#[cfg(test)]
mod tests {
    use gpui::ImageFormat;

    use super::pasted_name;

    #[test]
    fn a_pasted_image_takes_the_desktop_paste_name() {
        assert_eq!(pasted_name(ImageFormat::Png), "paste.png");
        assert_eq!(pasted_name(ImageFormat::Jpeg), "paste.jpg");
    }
}
