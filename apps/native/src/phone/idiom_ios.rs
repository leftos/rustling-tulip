//! Which kind of iOS device the phone app runs on: `UIDevice`'s interface
//! idiom, read once as the phone's root opens, so the pairing dialog can
//! name an iPad "this iPad".

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};

/// `UIUserInterfaceIdiomPad`.
const IDIOM_PAD: isize = 1;

/// Whether the app runs on an iPad; `false` when `UIKit` cannot say.
pub(super) fn is_pad() -> bool {
    let Some(class) = AnyClass::get(c"UIDevice") else {
        tracing::warn!("phone: no UIDevice class; the device is named a phone");
        return false;
    };
    // SAFETY: `+[UIDevice currentDevice]` takes no arguments and returns the
    // shared device object.
    let device: Option<Retained<AnyObject>> = unsafe { msg_send![class, currentDevice] };
    let Some(device) = device else {
        tracing::warn!("phone: no current UIDevice; the device is named a phone");
        return false;
    };
    // SAFETY: `-[UIDevice userInterfaceIdiom]` takes no arguments and
    // returns a `UIUserInterfaceIdiom`, an `NSInteger`.
    let idiom: isize = unsafe { msg_send![&*device, userInterfaceIdiom] };
    tracing::info!(idiom, "phone: the device's interface idiom");
    idiom == IDIOM_PAD
}
