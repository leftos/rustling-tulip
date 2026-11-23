//! The phone's Camera on iOS: `UIImagePickerController` with the camera as
//! its source, for one photo, presented from the topmost view controller of
//! the app's key window on the main thread, as the photo picker is
//! (`photos_ios.rs`). It runs in process, so the app's Info.plist carries
//! `NSCameraUsageDescription`.
//!
//! The photo taken is encoded as JPEG and, over the upload cap, scaled down
//! as a picked photo is ([`super::photos_ios::fitted`]); it is answered
//! through a oneshot channel, since the delegate runs on the main thread
//! while the capture's tap may not.

use std::cell::OnceCell;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use gpui::ImageFormat;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_foundation::{NSDictionary, NSObject};
use objc2_ui_kit::{
    UIApplication, UIImage, UIImagePickerController, UIImagePickerControllerDelegate,
    UIImagePickerControllerInfoKey, UIImagePickerControllerOriginalImage,
    UIImagePickerControllerSourceType, UINavigationControllerDelegate, UIViewController,
};

use super::camera::CameraPicker;
use super::pasteboard::PastedImage;
use super::photos::{PickSlot, SCALED_QUALITY};
use super::photos_ios::fitted;

/// The open capture's answer, until the camera finishes and takes it out; a
/// newer capture replaces one still open, which answers it nothing.
static WAITING: PickSlot<Option<PastedImage>> = PickSlot::new();

// SAFETY:
// - `UIImagePickerControllerDelegate` asks this class only for its finish
//   and cancel methods, with their declared signatures, and
//   `UINavigationControllerDelegate`'s methods are all optional.
// - `CameraDelegate` does not implement `Drop`.
define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RTCameraPickerDelegate"]
    struct CameraDelegate;

    unsafe impl NSObjectProtocol for CameraDelegate {}

    unsafe impl UINavigationControllerDelegate for CameraDelegate {}

    unsafe impl UIImagePickerControllerDelegate for CameraDelegate {
        #[unsafe(method(imagePickerController:didFinishPickingMediaWithInfo:))]
        fn did_finish_picking(
            &self,
            picker: &UIImagePickerController,
            info: &NSDictionary<UIImagePickerControllerInfoKey, AnyObject>,
        ) {
            taken(picker, info);
        }

        #[unsafe(method(imagePickerControllerDidCancel:))]
        fn did_cancel(&self, picker: &UIImagePickerController) {
            cancelled(picker);
        }
    }
);

thread_local! {
    /// The camera's delegate, made once on the main thread; the picker holds
    /// its delegate weak, so this keeps it alive.
    static DELEGATE: OnceCell<Retained<CameraDelegate>> = const { OnceCell::new() };
}

/// The iOS camera.
pub struct IosCamera;

impl CameraPicker for IosCamera {
    fn available(&self) -> bool {
        MainThreadMarker::new().is_some_and(|mtm| {
            UIImagePickerController::isSourceTypeAvailable(
                UIImagePickerControllerSourceType::Camera,
                mtm,
            )
        })
    }

    fn capture(&self) -> BoxFuture<'static, Option<PastedImage>> {
        let rx = WAITING.open();
        if let Some(mtm) = MainThreadMarker::new() {
            present_on_main(mtm);
        } else {
            DispatchQueue::main().exec_async(|| {
                if let Some(mtm) = MainThreadMarker::new() {
                    present_on_main(mtm);
                } else {
                    tracing::warn!("phone: the main queue ran off the main thread; no camera");
                    WAITING.take().send(None);
                }
            });
        }
        rx.map(|taken| taken.ok().flatten()).boxed()
    }
}

/// The camera's delegate, made on first use.
fn delegate(mtm: MainThreadMarker) -> Retained<CameraDelegate> {
    DELEGATE.with(|cell| {
        cell.get_or_init(|| {
            let this = CameraDelegate::alloc(mtm).set_ivars(());
            // SAFETY: `NSObject`'s `init` takes no arguments and the class
            // adds no state of its own.
            unsafe { msg_send![super(this), init] }
        })
        .clone()
    })
}

/// Presents the camera from the topmost view controller; logs
/// `phone: camera presented` once `UIKit` has shown it.
fn present_on_main(mtm: MainThreadMarker) {
    let Some(presenter) = top_view_controller(mtm) else {
        tracing::warn!("phone: no view controller to present the camera from");
        WAITING.take().send(None);
        return;
    };
    let picker = UIImagePickerController::new(mtm);
    picker.setSourceType(UIImagePickerControllerSourceType::Camera);
    let delegate = delegate(mtm);
    let object: &AnyObject = &delegate;
    // SAFETY: the delegate conforms to both protocols the picker asks of it,
    // and lives for the process (`DELEGATE`), so the picker's weak reference
    // stays valid.
    unsafe { picker.setDelegate(Some(object)) };
    let done = RcBlock::new(|| tracing::info!("phone: camera presented"));
    presenter.presentViewController_animated_completion(&picker, true, Some(&done));
}

/// The camera took a photo: it is dismissed, its capture's answer is taken
/// out of [`WAITING`], and the original image is encoded as JPEG, fitted to
/// the upload cap and answered; nothing when it cannot be read.
fn taken(
    picker: &UIImagePickerController,
    info: &NSDictionary<UIImagePickerControllerInfoKey, AnyObject>,
) {
    picker.dismissViewControllerAnimated_completion(true, None);
    let answer = WAITING.take();
    // SAFETY: the key is UIKit's immutable string constant.
    let key = unsafe { UIImagePickerControllerOriginalImage };
    let image = info
        .objectForKey(key)
        .and_then(|object| object.downcast::<UIImage>().ok());
    let Some(jpeg) = image.and_then(|image| image.jpeg_representation(SCALED_QUALITY)) else {
        tracing::warn!("phone: the photo taken could not be read as an image");
        answer.send(None);
        return;
    };
    tracing::info!(bytes = jpeg.length(), "phone: a photo was taken");
    answer.send(Some(fitted(jpeg.to_vec(), ImageFormat::Jpeg)));
}

/// The camera was dismissed without a photo: the capture is answered nothing.
fn cancelled(picker: &UIImagePickerController) {
    picker.dismissViewControllerAnimated_completion(true, None);
    tracing::info!("phone: the camera was cancelled");
    WAITING.take().send(None);
}

/// The view controller at the top of the key window's presentation chain,
/// so a sheet already up does not refuse the camera (as `photos_ios.rs`
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
