//! The phone's Photos on iOS: `PHPickerViewController` for one image,
//! presented from the topmost view controller of the app's key window on the
//! main thread, as the share sheet is (`share_ios.rs`). The picker runs out
//! of process, so it needs no photo library permission.
//!
//! The picked item is loaded as JPEG: taken as it is when its provider offers
//! `public.jpeg`, else loaded as `public.image` and re-encoded through
//! `UIImage`. An image over the upload cap is scaled down ([`fitted`]). The
//! answer goes back through a oneshot channel, since the item provider's
//! completion runs on a queue of the system's choosing.
//!
//! `objc2-photos-ui` declares `PHPickerViewController` for macOS alone, so
//! the iOS class (a `UIViewController`) is declared here; the configuration,
//! filter, result and delegate protocol come from the crate, which links
//! `PhotosUI`.

use std::cell::OnceCell;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use gpui::ImageFormat;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2::{
    MainThreadMarker, MainThreadOnly, define_class, extern_class, extern_methods, msg_send,
};
use objc2_core_foundation::CGSize;
use objc2_foundation::{NSArray, NSData, NSError, NSObject, NSString};
use objc2_photos_ui::{
    PHPickerConfiguration, PHPickerConfigurationAssetRepresentationMode, PHPickerFilter,
    PHPickerResult, PHPickerViewControllerDelegate,
};
use objc2_ui_kit::{UIApplication, UIImage, UIResponder, UIViewController};

use super::pasteboard::PastedImage;
use super::photos::{Fit, PhotoPicker, PickSlot, SCALED_QUALITY, fit, scaled_size};

/// The type a picked item is taken as it is under.
const JPEG_TYPE: &str = "public.jpeg";

/// The type a picked item is loaded as when it offers no JPEG.
const IMAGE_TYPE: &str = "public.image";

/// The open pick's answer, until the picker finishes and takes it out for
/// its load; a newer pick replaces one still open, which answers it nothing.
static WAITING: PickSlot<Option<PastedImage>> = PickSlot::new();

extern_class!(
    /// `PhotosUI`'s picker on iOS, a `UIViewController`.
    #[unsafe(super(UIViewController, UIResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "PHPickerViewController"]
    struct PickerController;
);

#[expect(
    non_snake_case,
    reason = "the methods keep their Objective-C selector names, as objc2's own bindings do"
)]
impl PickerController {
    extern_methods!(
        #[unsafe(method(initWithConfiguration:))]
        #[unsafe(method_family = init)]
        unsafe fn initWithConfiguration(
            this: Allocated<Self>,
            configuration: &PHPickerConfiguration,
        ) -> Retained<Self>;

        #[unsafe(method(setDelegate:))]
        #[unsafe(method_family = none)]
        unsafe fn setDelegate(
            &self,
            delegate: Option<&ProtocolObject<dyn PHPickerViewControllerDelegate>>,
        );
    );
}

// SAFETY:
// - `PHPickerViewControllerDelegate` asks this class only for
//   `picker:didFinishPicking:`, with its declared signature.
// - `PickerDelegate` does not implement `Drop`.
define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RTPhotoPickerDelegate"]
    struct PickerDelegate;

    unsafe impl NSObjectProtocol for PickerDelegate {}

    unsafe impl PHPickerViewControllerDelegate for PickerDelegate {
        #[unsafe(method(picker:didFinishPicking:))]
        fn did_finish_picking(&self, picker: &UIViewController, results: &NSArray<PHPickerResult>) {
            finished(picker, results);
        }
    }
);

thread_local! {
    /// The picker's delegate, made once on the main thread; the picker holds
    /// its delegate unretained, so this keeps it alive.
    static DELEGATE: OnceCell<Retained<PickerDelegate>> = const { OnceCell::new() };
}

/// The iOS photo picker.
pub struct IosPhotoPicker;

impl PhotoPicker for IosPhotoPicker {
    fn pick(&self) -> BoxFuture<'static, Option<PastedImage>> {
        let rx = WAITING.open();
        if let Some(mtm) = MainThreadMarker::new() {
            present_on_main(mtm);
        } else {
            DispatchQueue::main().exec_async(|| match MainThreadMarker::new() {
                Some(mtm) => present_on_main(mtm),
                None => {
                    tracing::warn!(
                        "phone: the main queue ran off the main thread; no photo picker"
                    );
                    WAITING.take().send(None);
                }
            });
        }
        rx.map(|picked| picked.ok().flatten()).boxed()
    }
}

/// The picker's delegate, made on first use.
fn delegate(mtm: MainThreadMarker) -> Retained<PickerDelegate> {
    DELEGATE.with(|cell| {
        cell.get_or_init(|| {
            let this = PickerDelegate::alloc(mtm).set_ivars(());
            // SAFETY: `NSObject`'s `init` takes no arguments and the class
            // adds no state of its own.
            unsafe { msg_send![super(this), init] }
        })
        .clone()
    })
}

/// Presents the picker, images only and one at most, from the topmost view
/// controller; logs `phone: photo picker presented` once `UIKit` has shown
/// it.
fn present_on_main(mtm: MainThreadMarker) {
    let Some(presenter) = top_view_controller(mtm) else {
        tracing::warn!("phone: no view controller to present the photo picker from");
        WAITING.take().send(None);
        return;
    };
    // SAFETY: a fresh configuration is set up on the main thread before the
    // picker is made from it; the limit and filter are the documented ones.
    let picker = unsafe {
        let configuration = PHPickerConfiguration::new();
        configuration.setSelectionLimit(1);
        configuration.setFilter(Some(&PHPickerFilter::imagesFilter()));
        configuration.setPreferredAssetRepresentationMode(
            PHPickerConfigurationAssetRepresentationMode::Compatible,
        );
        PickerController::initWithConfiguration(PickerController::alloc(mtm), &configuration)
    };
    let delegate = delegate(mtm);
    // SAFETY: the delegate lives for the process (`DELEGATE`), so the
    // picker's unretained reference stays valid.
    unsafe { picker.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
    let done = RcBlock::new(|| tracing::info!("phone: photo picker presented"));
    presenter.presentViewController_animated_completion(&picker, true, Some(&done));
}

/// The picker finished: it is dismissed, its pick's answer is taken out of
/// [`WAITING`] here on the main thread, and its first result is loaded and
/// answered through that answer alone, or nothing when the pick was
/// cancelled. A pick opened while the load runs gets a fresh slot, which this
/// load never answers.
fn finished(picker: &UIViewController, results: &NSArray<PHPickerResult>) {
    picker.dismissViewControllerAnimated_completion(true, None);
    let answer = WAITING.take();
    let Some(result) = results.firstObject() else {
        tracing::info!("phone: the photo pick was cancelled");
        answer.send(None);
        return;
    };
    // SAFETY: a result's item provider is read on the main thread the
    // delegate is called on.
    let provider = unsafe { result.itemProvider() };
    let jpeg = provider.hasItemConformingToTypeIdentifier(&NSString::from_str(JPEG_TYPE));
    let kind = NSString::from_str(if jpeg { JPEG_TYPE } else { IMAGE_TYPE });
    let block = RcBlock::new(move |data: *mut NSData, error: *mut NSError| {
        // SAFETY: the provider hands the completion a live data object, or
        // null with a live error, for the call.
        let (data, error) = unsafe { (data.as_ref(), error.as_ref()) };
        match data {
            Some(data) => answer.send(as_jpeg(data.to_vec())),
            None => {
                let why = error.map(|error| error.localizedDescription().to_string());
                tracing::warn!(error = ?why, "phone: the picked photo could not be loaded");
                answer.send(None);
            }
        }
    });
    // SAFETY: the completion touches only the answer channel, `NSData` and
    // `UIImage`, which are safe on the queue it runs on.
    let _progress = unsafe {
        provider.loadDataRepresentationForTypeIdentifier_completionHandler(&kind, &block)
    };
}

/// `bytes` as a JPEG within the cap: a JPEG is kept, any other image
/// re-encoded at [`SCALED_QUALITY`]; `None` when it cannot be read.
pub(super) fn as_jpeg(bytes: Vec<u8>) -> Option<PastedImage> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(fitted(bytes, ImageFormat::Jpeg));
    }
    let image = UIImage::imageWithData(&NSData::with_bytes(&bytes));
    let Some(jpeg) = image.and_then(|image| image.jpeg_representation(SCALED_QUALITY)) else {
        tracing::warn!(
            bytes = bytes.len(),
            "phone: the picked photo could not be read as an image"
        );
        return None;
    };
    Some(fitted(jpeg.to_vec(), ImageFormat::Jpeg))
}

/// `bytes` of `format` as they go up: as they are within the cap, else
/// scaled and encoded as JPEG ([`super::photos::fit`]). An image that cannot
/// be scaled goes as it is, which the upload refuses with its too-large
/// toast.
pub(super) fn fitted(bytes: Vec<u8>, format: ImageFormat) -> PastedImage {
    match fit(bytes.len()) {
        Fit::AsIs => (bytes, format),
        Fit::Scale { long_side, quality } => match scaled(&bytes, long_side, quality) {
            Some(jpeg) => (jpeg, ImageFormat::Jpeg),
            None => (bytes, format),
        },
    }
}

/// `bytes` scaled so the longer side is at most `long_side` and encoded as
/// JPEG at `quality`; `None` when they cannot be read as an image.
fn scaled(bytes: &[u8], long_side: u32, quality: f64) -> Option<Vec<u8>> {
    let image = UIImage::imageWithData(&NSData::with_bytes(bytes))?;
    // SAFETY: an image's size is read-only state, safe off the main thread.
    let size = unsafe { image.size() };
    let (width, height) = scaled_size(size.width, size.height, long_side);
    let thumbnail = image.imageByPreparingThumbnailOfSize(CGSize::new(width, height))?;
    let jpeg = thumbnail.jpeg_representation(quality)?;
    tracing::info!(
        from = bytes.len(),
        to = jpeg.length(),
        "phone: image scaled to fit the upload cap"
    );
    Some(jpeg.to_vec())
}

/// The view controller at the top of the key window's presentation chain,
/// so a sheet already up does not refuse the picker (as `share_ios.rs`
/// finds it).
fn top_view_controller(mtm: MainThreadMarker) -> Option<Retained<UIViewController>> {
    #[expect(
        deprecated,
        reason = "the app does not hold its window scene; gpui_ios opens one window"
    )]
    let windows = UIApplication::sharedApplication(mtm).windows();
    let window = windows
        .iter()
        .find(|window| window.isKeyWindow())
        .or_else(|| windows.firstObject())?;
    let mut top = window.rootViewController()?;
    while let Some(presented) = top.presentedViewController() {
        top = presented;
    }
    Some(top)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test fails loudly")]
mod tests {
    use objc2::{ClassType, ProtocolType, sel};
    use objc2_photos_ui::PHPickerViewControllerDelegate;

    use super::PickerDelegate;

    #[test]
    fn the_picker_delegate_registers_with_its_protocol_method() {
        let class = PickerDelegate::class();
        let protocol = <dyn PHPickerViewControllerDelegate>::protocol()
            .expect("PhotosUI registers the picker's delegate protocol");
        assert!(
            class.conforms_to(protocol),
            "the class conforms to the protocol"
        );
        assert!(
            class
                .instance_method(sel!(picker:didFinishPicking:))
                .is_some(),
            "the class answers picker:didFinishPicking:"
        );
    }
}
