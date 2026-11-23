//! The phone's Photos: picks up to [`PHOTOS_LIMIT`] images from the photo
//! library for the composer to upload, and the size and name rules a phone
//! image goes up by.
//!
//! On iOS the picker is `PHPickerViewController` (`photos_ios.rs`); every
//! other platform logs the request and picks nothing, and [`FakePhotoPicker`]
//! answers as a spec says.

use std::sync::{Mutex, PoisonError};

use chrono::NaiveDateTime;
use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::BoxFuture;
use gpui::ImageFormat;
use protocol::UPLOAD_MAX_BYTES;

use super::pasteboard::PastedImage;
use crate::paste_upload::extension_of;

/// The composer's Photos button's label.
pub const PHOTOS_LABEL: &str = "Photos";

/// The most images one pick takes.
pub const PHOTOS_LIMIT: usize = 10;

/// The longer side, in pixels, an image over the cap is scaled down to.
pub const SCALED_LONG_SIDE: u32 = 2048;

/// The JPEG quality an image over the cap is encoded at.
pub const SCALED_QUALITY: f64 = 0.8;

/// What the phone's platform offers for picking a photo.
pub trait PhotoPicker: Send + Sync {
    /// Shows the platform's photo picker for up to [`PHOTOS_LIMIT`] images;
    /// answers the picked images in the picker's order, none when the pick
    /// was cancelled or nothing could be loaded.
    fn pick(&self) -> BoxFuture<'static, Vec<PastedImage>>;
}

/// A platform with no photo picker: a request is logged and nothing is
/// picked.
pub struct LoggingPhotos;

impl PhotoPicker for LoggingPhotos {
    fn pick(&self) -> BoxFuture<'static, Vec<PastedImage>> {
        tracing::info!("phone: no photo picker on this platform");
        futures::future::ready(Vec::new()).boxed()
    }
}

/// A photo picker for specs: every pick answers what it was made with, and
/// the picks are counted.
pub struct FakePhotoPicker {
    answer: Vec<PastedImage>,
    picks: Mutex<usize>,
}

impl FakePhotoPicker {
    /// A picker whose every pick answers `answer`.
    #[must_use]
    pub fn answering(answer: Vec<PastedImage>) -> Self {
        Self {
            answer,
            picks: Mutex::new(0),
        }
    }

    /// How many picks were asked for.
    #[must_use]
    pub fn picks(&self) -> usize {
        *self.picks.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl PhotoPicker for FakePhotoPicker {
    fn pick(&self) -> BoxFuture<'static, Vec<PastedImage>> {
        *self.picks.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        futures::future::ready(self.answer.clone()).boxed()
    }
}

/// The answer slot of the platform picker's open pick. [`PickSlot::open`]
/// makes a pick's channel; when the user finishes picking, the picker takes
/// the pick's answer out with [`PickSlot::take`] before it loads the images,
/// so the load answers only its own pick, and a pick opened while that load
/// runs gets a fresh slot the old load can never answer.
#[cfg_attr(
    not(any(target_os = "ios", test)),
    expect(dead_code, reason = "only the iOS picker answers through a slot")
)]
pub(super) struct PickSlot<T> {
    waiting: Mutex<Option<oneshot::Sender<T>>>,
}

#[cfg_attr(
    not(any(target_os = "ios", test)),
    expect(dead_code, reason = "only the iOS picker answers through a slot")
)]
impl<T> PickSlot<T> {
    /// An empty slot.
    pub(super) const fn new() -> Self {
        Self {
            waiting: Mutex::new(None),
        }
    }

    /// Opens a pick and gives the end its answer arrives on. A pick still
    /// waiting in the slot is dropped, so it answers nothing.
    pub(super) fn open(&self) -> oneshot::Receiver<T> {
        let (tx, rx) = oneshot::channel();
        *self.waiting.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
        rx
    }

    /// Takes the open pick's answer out of the slot; it answers nothing when
    /// no pick was open.
    pub(super) fn take(&self) -> PickAnswer<T> {
        let sender = self
            .waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        PickAnswer(Mutex::new(sender))
    }
}

/// One pick's answer, taken out of its [`PickSlot`]: answered at most once,
/// from any thread.
pub(super) struct PickAnswer<T>(Mutex<Option<oneshot::Sender<T>>>);

#[cfg_attr(
    not(any(target_os = "ios", test)),
    expect(dead_code, reason = "only the iOS picker answers through a slot")
)]
impl<T> PickAnswer<T> {
    /// Answers the pick with `value`; a second answer, or one whose pick
    /// is gone, is dropped.
    pub(super) fn send(&self, value: T) {
        let sender = self.0.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(tx) = sender
            && tx.send(value).is_err()
        {
            tracing::debug!("phone: a photo pick was answered after its tap was gone");
        }
    }
}

/// How a phone image of `len` bytes goes up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fit {
    /// As it is: it is within the upload cap.
    AsIs,
    /// Scaled so its longer side is at most `long_side` pixels and encoded
    /// as JPEG at `quality`: it is over the cap.
    Scale { long_side: u32, quality: f64 },
}

/// How an image of `len` bytes goes up: as it is within
/// [`UPLOAD_MAX_BYTES`], else scaled to [`SCALED_LONG_SIDE`] at
/// [`SCALED_QUALITY`]. One still over the cap after that is refused by the
/// upload with the desktop's too-large toast.
#[must_use]
pub fn fit(len: usize) -> Fit {
    if len <= UPLOAD_MAX_BYTES {
        Fit::AsIs
    } else {
        Fit::Scale {
            long_side: SCALED_LONG_SIDE,
            quality: SCALED_QUALITY,
        }
    }
}

/// The pixel size an image of `width` by `height` pixels is drawn at so its
/// longer side is exactly `max_long` pixels, keeping its shape: an image
/// already within `max_long` keeps its size rather than growing, and a
/// shorter side never rounds below one pixel.
#[must_use]
pub fn scaled_pixel_size(width: f64, height: f64, max_long: f64) -> (f64, f64) {
    let longer = width.max(height);
    if longer <= max_long || longer <= 0.0 || max_long <= 0.0 {
        return (width, height);
    }
    let ratio = max_long / longer;
    let shorter = |side: f64| (side * ratio).round().max(1.0);
    if width >= height {
        (max_long, shorter(height))
    } else {
        (shorter(width), max_long)
    }
}

/// The name a picked image of `format` uploads under, picked at `at`:
/// `photo-{yyyyMMdd-HHmmss}.<ext>`.
#[must_use]
pub fn picked_name(at: NaiveDateTime, format: ImageFormat) -> String {
    format!(
        "photo-{}.{}",
        at.format("%Y%m%d-%H%M%S"),
        extension_of(format)
    )
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test fixtures")]
mod tests {
    use chrono::NaiveDate;
    use gpui::ImageFormat;
    use protocol::UPLOAD_MAX_BYTES;

    use super::{
        Fit, PickSlot, SCALED_LONG_SIDE, SCALED_QUALITY, fit, picked_name, scaled_pixel_size,
    };

    #[test]
    fn a_taken_answer_answers_only_its_own_pick_and_only_once() {
        let slot: PickSlot<u8> = PickSlot::new();
        let mut first = slot.open();
        let loading = slot.take();
        let mut second = slot.open();

        loading.send(1);
        loading.send(2);

        assert_eq!(
            first.try_recv(),
            Ok(Some(1)),
            "the load answers its own pick"
        );
        assert_eq!(
            second.try_recv(),
            Ok(None),
            "a pick opened while the load ran is not answered by it"
        );
        slot.take().send(3);
        assert_eq!(
            second.try_recv(),
            Ok(Some(3)),
            "the new pick gets its own answer"
        );
    }

    #[test]
    fn a_pick_replaced_before_it_finished_answers_nothing() {
        let slot: PickSlot<u8> = PickSlot::new();
        let mut first = slot.open();
        let _second = slot.open();
        assert!(first.try_recv().is_err(), "the replaced pick is cancelled");
        let empty: PickSlot<u8> = PickSlot::new();
        empty.take().send(4);
    }

    #[test]
    fn an_image_within_the_cap_goes_up_as_it_is() {
        assert_eq!(fit(0), Fit::AsIs);
        assert_eq!(fit(UPLOAD_MAX_BYTES), Fit::AsIs);
    }

    #[test]
    fn an_image_over_the_cap_is_scaled_to_2048_at_quality_0_8() {
        assert_eq!(
            fit(UPLOAD_MAX_BYTES + 1),
            Fit::Scale {
                long_side: SCALED_LONG_SIDE,
                quality: SCALED_QUALITY
            }
        );
        assert_eq!(SCALED_LONG_SIDE, 2048);
        assert!((SCALED_QUALITY - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn a_4032_by_3024_photo_scales_to_2048_by_1536() {
        assert_eq!(scaled_pixel_size(4032.0, 3024.0, 2048.0), (2048.0, 1536.0));
    }

    #[test]
    fn a_portrait_photo_scales_its_long_side() {
        assert_eq!(scaled_pixel_size(3024.0, 4032.0, 2048.0), (1536.0, 2048.0));
        assert_eq!(scaled_pixel_size(8000.0, 10.0, 2048.0), (2048.0, 3.0));
    }

    #[test]
    fn a_small_photo_is_never_upscaled() {
        assert_eq!(scaled_pixel_size(2048.0, 1000.0, 2048.0), (2048.0, 1000.0));
        assert_eq!(scaled_pixel_size(1000.0, 800.0, 2048.0), (1000.0, 800.0));
        assert_eq!(scaled_pixel_size(0.0, 0.0, 2048.0), (0.0, 0.0));
    }

    #[test]
    fn a_picked_photo_is_named_by_its_time() {
        let at = NaiveDate::from_ymd_opt(2026, 11, 20)
            .and_then(|day| day.and_hms_opt(9, 5, 7))
            .expect("a valid time");
        assert_eq!(
            picked_name(at, ImageFormat::Jpeg),
            "photo-20261120-090507.jpg"
        );
    }
}
