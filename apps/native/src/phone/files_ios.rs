//! The phone's Files on iOS: `UIDocumentPickerViewController` for one file,
//! opened as a copy, presented from the topmost view controller of the app's
//! key window on the main thread, as the photo picker is (`photos_ios.rs`).
//! Opening a copy hands back a URL the app may read without security-scoped
//! access.
//!
//! The picked file's size is read first: one over the upload cap is
//! answered by name alone, never read into memory. Otherwise its name and
//! bytes are read out of that copy and answered
//! through a oneshot channel, since the delegate runs on the main thread while
//! the pick's tap may not.

use std::cell::OnceCell;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_foundation::{NSArray, NSData, NSObject, NSURL};
use objc2_ui_kit::{
    UIApplication, UIDocumentPickerDelegate, UIDocumentPickerViewController, UIViewController,
};
use objc2_uniform_type_identifiers::UTTypeItem;

use protocol::UPLOAD_MAX_BYTES;

use super::files_picker::{FilePick, FilePicker};
use super::photos::PickSlot;

/// The open pick's answer, until the picker finishes and takes it out; a
/// newer pick replaces one still open, which answers it nothing.
static WAITING: PickSlot<Option<FilePick>> = PickSlot::new();

// SAFETY:
// - `UIDocumentPickerDelegate` asks this class only for its two pick methods,
//   with their declared signatures.
// - `PickerDelegate` does not implement `Drop`.
define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RTFilePickerDelegate"]
    struct PickerDelegate;

    unsafe impl NSObjectProtocol for PickerDelegate {}

    unsafe impl UIDocumentPickerDelegate for PickerDelegate {
        #[unsafe(method(documentPicker:didPickDocumentsAtURLs:))]
        fn did_pick_documents(
            &self,
            picker: &UIDocumentPickerViewController,
            urls: &NSArray<NSURL>,
        ) {
            picked(picker, urls);
        }

        #[unsafe(method(documentPickerWasCancelled:))]
        fn was_cancelled(&self, picker: &UIDocumentPickerViewController) {
            cancelled(picker);
        }
    }
);

thread_local! {
    /// The picker's delegate, made once on the main thread; the picker holds
    /// its delegate weak, so this keeps it alive.
    static DELEGATE: OnceCell<Retained<PickerDelegate>> = const { OnceCell::new() };
}

/// The iOS file picker.
pub struct IosFilePicker;

impl FilePicker for IosFilePicker {
    fn pick(&self) -> BoxFuture<'static, Option<FilePick>> {
        let rx = WAITING.open();
        if let Some(mtm) = MainThreadMarker::new() {
            present_on_main(mtm);
        } else {
            DispatchQueue::main().exec_async(|| {
                if let Some(mtm) = MainThreadMarker::new() {
                    present_on_main(mtm);
                } else {
                    tracing::warn!("phone: the main queue ran off the main thread; no file picker");
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

/// Presents the picker for any document, opened as a copy, from the topmost
/// view controller; logs `phone: file picker presented` once `UIKit` has
/// shown it.
fn present_on_main(mtm: MainThreadMarker) {
    let Some(presenter) = top_view_controller(mtm) else {
        tracing::warn!("phone: no view controller to present the file picker from");
        WAITING.take().send(None);
        return;
    };
    // SAFETY: `UTTypeItem` is the framework's `public.item` constant, read as
    // the immutable object every document type conforms to.
    let types = NSArray::from_slice(&[unsafe { UTTypeItem }]);
    let picker = UIDocumentPickerViewController::initForOpeningContentTypes_asCopy(
        UIDocumentPickerViewController::alloc(mtm),
        &types,
        true,
    );
    let delegate = delegate(mtm);
    picker.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    let done = RcBlock::new(|| tracing::info!("phone: file picker presented"));
    presenter.presentViewController_animated_completion(&picker, true, Some(&done));
}

/// The picker finished: it is dismissed, its pick's answer is taken out of
/// [`WAITING`] here, and its first file's name and bytes are read from the
/// copy the picker made and answered through that answer alone, its name
/// alone when it is over the upload cap, or nothing when the pick was
/// cancelled.
fn picked(picker: &UIDocumentPickerViewController, urls: &NSArray<NSURL>) {
    picker.dismissViewControllerAnimated_completion(true, None);
    let answer = WAITING.take();
    let Some(url) = urls.firstObject() else {
        tracing::info!("phone: the file pick was cancelled");
        answer.send(None);
        return;
    };
    let name = url
        .lastPathComponent()
        .map_or_else(|| "file".to_owned(), |name| name.to_string());
    if over_cap(&url, &name) {
        answer.send(Some(FilePick::TooLarge(name)));
        return;
    }
    let Some(bytes) = NSData::dataWithContentsOfURL(&url) else {
        tracing::warn!(name = %name, "phone: the picked file could not be read");
        answer.send(None);
        return;
    };
    tracing::info!(
        name = %name,
        bytes = bytes.length(),
        "phone: a file was picked"
    );
    answer.send(Some(FilePick::File((name, bytes.to_vec()))));
}

/// Whether the copy at `url`, named `name`, is over the upload cap, by its
/// size on disk; a size that cannot be read is logged and counts as within
/// it, so the copy is read as before.
fn over_cap(url: &NSURL, name: &str) -> bool {
    let Some(path) = url.path() else {
        tracing::warn!(name = %name, "phone: the picked file has no path to size");
        return false;
    };
    match std::fs::metadata(path.to_string()) {
        Ok(meta) => {
            let bytes = meta.len();
            let over = bytes > u64::try_from(UPLOAD_MAX_BYTES).unwrap_or(u64::MAX);
            if over {
                tracing::info!(name = %name, bytes, "phone: the picked file is over the upload cap");
            }
            over
        }
        Err(err) => {
            tracing::warn!(name = %name, error = %err, "phone: the picked file's size could not be read");
            false
        }
    }
}

/// The picker was dismissed without a file: the pick is answered nothing.
fn cancelled(picker: &UIDocumentPickerViewController) {
    picker.dismissViewControllerAnimated_completion(true, None);
    tracing::info!("phone: the file pick was cancelled");
    WAITING.take().send(None);
}

/// The view controller at the top of the key window's presentation chain,
/// so a sheet already up does not refuse the picker (as `photos_ios.rs`
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
