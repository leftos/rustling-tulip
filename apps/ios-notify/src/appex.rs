//! The extension's principal class, `RTNotificationService`: a
//! `UNNotificationServiceExtension` that reads the push's `e` ciphertext and
//! the install's push keys from the Keychain, and hands the content handler
//! the decrypted title, body and badge, or the content as it came on any
//! failure. The handler is called exactly once. Nothing is logged.

use std::ptr::NonNull;
use std::sync::Mutex;

use block2::{DynBlock, RcBlock};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2::{ClassType, DefinedClass, define_class, msg_send};
use objc2_foundation::{
    NSMutableCopying, NSMutableDictionary, NSNumber, NSObject, NSString, ns_string,
};
use objc2_user_notifications::{
    UNMutableNotificationContent, UNNotificationContent, UNNotificationRequest,
    UNNotificationServiceExtension,
};
use push_crypto::{KEYCHAIN_ACCOUNT, KEYCHAIN_SERVICE};
use rustling_tulip_ios_notify::notice::{self, Alert};
use security_framework::passwords::{PasswordOptions, generic_password};

/// The block the system hands `didReceiveNotificationRequest:withContentHandler:`.
type ContentHandler = DynBlock<dyn Fn(NonNull<UNNotificationContent>)>;

/// The handler, copied off the stack so it outlives the call that brought it.
type HeldHandler = RcBlock<dyn Fn(NonNull<UNNotificationContent>)>;

/// The request's handler and its content as it came, kept until the handler
/// runs so `serviceExtensionTimeWillExpire` can still deliver the content.
#[derive(Default)]
struct Ivars {
    handler: Mutex<Option<HeldHandler>>,
    original: Mutex<Option<Retained<UNNotificationContent>>>,
}

define_class!(
    // SAFETY:
    // - UNNotificationServiceExtension asks a subclass only to override its two methods,
    //   which this class does with their declared signatures, and to call the handler once.
    // - RTNotificationService does not implement Drop.
    #[unsafe(super(UNNotificationServiceExtension, NSObject))]
    #[name = "RTNotificationService"]
    #[ivars = Ivars]
    struct RTNotificationService;

    impl RTNotificationService {
        #[unsafe(method_id(init))]
        fn init(this: Allocated<Self>) -> Option<Retained<Self>> {
            let this = this.set_ivars(Ivars::default());
            // SAFETY: `init` is the superclass's designated initializer, called once on the
            // allocated object whose ivars are now set.
            unsafe { msg_send![super(this), init] }
        }

        #[unsafe(method(didReceiveNotificationRequest:withContentHandler:))]
        fn did_receive(&self, request: &UNNotificationRequest, handler: &ContentHandler) {
            self.receive(request, handler);
        }

        #[unsafe(method(serviceExtensionTimeWillExpire))]
        fn time_will_expire(&self) {
            self.deliver_original();
        }
    }

    unsafe impl NSObjectProtocol for RTNotificationService {}
);

impl RTNotificationService {
    fn receive(&self, request: &UNNotificationRequest, handler: &ContentHandler) {
        let original = request.content();
        if let Ok(mut slot) = self.ivars().handler.lock() {
            *slot = Some(handler.copy());
        }
        if let Ok(mut slot) = self.ivars().original.lock() {
            *slot = Some(original.clone());
        }
        let e = ciphertext(&original);
        let keys = install_keys();
        match notice::alert(e.as_deref(), keys.as_deref()) {
            Ok(alert) => self.deliver(&rewritten(&original, &alert)),
            Err(_) => self.deliver_original(),
        }
    }

    /// Hands the handler the request's content as it came, if it has not run.
    fn deliver_original(&self) {
        let original = self
            .ivars()
            .original
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Some(original) = original {
            self.deliver(&original);
        }
    }

    /// Hands the handler `content`, once: a second call finds no handler.
    fn deliver(&self, content: &UNNotificationContent) {
        let handler = self
            .ivars()
            .handler
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Ok(mut slot) = self.ivars().original.lock() {
            *slot = None;
        }
        if let Some(handler) = handler {
            handler.call((NonNull::from(content),));
        }
    }
}

/// The push's `e` ciphertext, when its user info holds one as a string.
fn ciphertext(content: &UNNotificationContent) -> Option<String> {
    let value = content.userInfo().objectForKey(ns_string!("e"))?;
    let text = value.downcast::<NSString>().ok()?;
    Some(text.to_string())
}

/// The install's push keys item, read with no access group named: the
/// extension's one entitled group is the one the app writes into.
fn install_keys() -> Option<Vec<u8>> {
    generic_password(PasswordOptions::new_generic_password(
        KEYCHAIN_SERVICE,
        KEYCHAIN_ACCOUNT,
    ))
    .ok()
}

/// A copy of `original` showing `alert`, with its ids and wait start in the
/// user info.
fn rewritten(
    original: &UNNotificationContent,
    alert: &Alert,
) -> Retained<UNMutableNotificationContent> {
    let shown = &alert.shown;
    let content = original.mutableCopy();
    content.setTitle(&NSString::from_str(&shown.title));
    content.setBody(&NSString::from_str(&shown.body));
    content.setBadge(Some(&NSNumber::numberWithUnsignedInt(shown.badge)));
    let info =
        NSMutableDictionary::<AnyObject, AnyObject>::dictionaryWithDictionary(&original.userInfo());
    if let Some(host) = &shown.host {
        put(&info, ns_string!("host"), &NSString::from_str(host));
    }
    put(
        &info,
        ns_string!("session_id"),
        &NSString::from_str(&shown.session_id),
    );
    put(&info, ns_string!("kind"), &NSString::from_str(&shown.kind));
    if let Some(since) = &alert.waited_since {
        put(
            &info,
            ns_string!("waited_since"),
            &NSString::from_str(since),
        );
    }
    // SAFETY: the dictionary holds only property-list objects: the push's own user info and
    // strings.
    unsafe { content.setUserInfo(&info) };
    content
}

fn put(info: &NSMutableDictionary<AnyObject, AnyObject>, key: &NSString, value: &NSString) {
    // SAFETY: the key is a string, which the dictionary copies, and both the key and object
    // types of the user info are any object.
    unsafe { info.setObject_forKey(value, ProtocolObject::from_ref(key)) };
}

/// Registers the class with the Objective-C runtime before `NSExtensionMain`
/// looks the principal class up by name.
extern "C" fn register_class() {
    let _ = RTNotificationService::class();
}

/// Runs [`register_class`] as one of the executable's initializers.
#[used]
#[unsafe(link_section = "__DATA,__mod_init_func")]
static REGISTER_CLASS: extern "C" fn() = register_class;
