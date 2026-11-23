//! The phone's Camera: takes one photo for the composer to upload as a
//! picked photo goes up.
//!
//! On iOS the camera is `UIImagePickerController` (`camera_ios.rs`); every
//! other platform has none, so the Attach sheet offers no Camera, and
//! [`FakeCamera`] answers as a spec says.

use std::sync::{Mutex, PoisonError};

use futures::FutureExt as _;
use futures::future::BoxFuture;

use super::pasteboard::PastedImage;

/// The Attach sheet's Camera row's label.
pub const CAMERA_LABEL: &str = "Camera";

/// What the phone's platform offers for taking a photo.
pub trait CameraPicker: Send + Sync {
    /// Whether the phone has a camera to take a photo with; the Attach sheet
    /// offers Camera only then.
    fn available(&self) -> bool;

    /// Shows the platform's camera for one photo; answers the photo taken,
    /// or `None` when the capture was cancelled or failed.
    fn capture(&self) -> BoxFuture<'static, Option<PastedImage>>;
}

/// A platform with no camera: it is not available, and a capture is logged
/// and takes nothing.
pub struct LoggingCamera;

impl CameraPicker for LoggingCamera {
    fn available(&self) -> bool {
        false
    }

    fn capture(&self) -> BoxFuture<'static, Option<PastedImage>> {
        tracing::info!("phone: no camera on this platform");
        futures::future::ready(None).boxed()
    }
}

/// A camera for specs: it is available as it was made, every capture answers
/// what it was made with, and the captures are counted.
pub struct FakeCamera {
    available: bool,
    answer: Option<PastedImage>,
    captures: Mutex<usize>,
}

impl FakeCamera {
    /// A camera that is `available` or not, whose every capture answers
    /// `answer`.
    #[must_use]
    pub fn new(available: bool, answer: Option<PastedImage>) -> Self {
        Self {
            available,
            answer,
            captures: Mutex::new(0),
        }
    }

    /// How many captures were asked for.
    #[must_use]
    pub fn captures(&self) -> usize {
        *self.captures.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl CameraPicker for FakeCamera {
    fn available(&self) -> bool {
        self.available
    }

    fn capture(&self) -> BoxFuture<'static, Option<PastedImage>> {
        *self.captures.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        futures::future::ready(self.answer.clone()).boxed()
    }
}

#[cfg(test)]
mod tests {
    use futures::FutureExt as _;
    use gpui::ImageFormat;

    use super::{CameraPicker, FakeCamera, LoggingCamera};

    #[test]
    fn a_fake_camera_counts_captures_and_says_whether_it_is_there() {
        let jpeg = vec![0xFF, 0xD8, 0xFF, 1];
        let camera = FakeCamera::new(true, Some((jpeg.clone(), ImageFormat::Jpeg)));
        assert!(camera.available(), "a camera made available is there");
        assert_eq!(camera.captures(), 0, "nothing captured yet");

        let taken = camera.capture().now_or_never().flatten();

        assert_eq!(
            taken,
            Some((jpeg, ImageFormat::Jpeg)),
            "the capture answers its photo"
        );
        assert_eq!(camera.captures(), 1, "the capture is counted");
        assert!(
            !FakeCamera::new(false, None).available(),
            "a camera made unavailable is not there"
        );
        assert!(!LoggingCamera.available(), "no platform camera off iOS");
    }
}
