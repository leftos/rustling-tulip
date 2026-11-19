//! The extension's principal class, `RTNotificationService`: a
//! `UNNotificationServiceExtension` that reads the push's `e` ciphertext and
//! the install's push keys from the Keychain, and hands the content handler
//! the decrypted title, body and badge, or the content as it came on any
//! failure. A Needs you alert that names the host's sessions still waiting
//! first removes the delivered Needs you notifications whose wait ended
//! ([`clearing::ends`]), so a later alert repairs what a lost background
//! clear left. The handler is called exactly once. Nothing is logged.

use std::ptr::NonNull;
use std::sync::Mutex;

use block2::{DynBlock, RcBlock};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2::{ClassType, DefinedClass, Message, define_class, msg_send};
use objc2_foundation::{
    NSArray, NSDictionary, NSMutableCopying, NSMutableDictionary, NSNumber, NSObject, NSString,
    ns_string,
};
use objc2_user_notifications::{
    UNMutableNotificationContent, UNNotification, UNNotificationContent, UNNotificationRequest,
    UNNotificationServiceExtension, UNUserNotificationCenter,
};
use protocol::{PushNotice, PushNoticeKind};
use push_crypto::{KEYCHAIN_ACCOUNT, KEYCHAIN_SERVICE};
use rustling_tulip_ios_notify::clearing::{self, Drawn};
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
            Ok(alert) => self.prune_then_deliver(rewritten(&original, &alert), alert.notice),
            Err(_) => self.deliver_original(),
        }
    }

    /// For a Needs you `notice` that names the host's sessions still
    /// waiting, removes the delivered Needs you notifications it ends, then
    /// hands the handler `content` from the same completion; any other
    /// notice hands it over at once. Should the completion never run,
    /// `serviceExtensionTimeWillExpire` delivers the content as it came.
    fn prune_then_deliver(
        &self,
        content: Retained<UNMutableNotificationContent>,
        notice: PushNotice,
    ) {
        if notice.kind != PushNoticeKind::NeedsYou || notice.waiting.is_none() {
            self.deliver(&content);
            return;
        }
        let this = self.retain();
        let block = RcBlock::new(move |shown: NonNull<NSArray<UNNotification>>| {
            // SAFETY: the system hands the completion a live array for the call.
            let shown = unsafe { shown.as_ref() };
            let ended: Vec<Retained<NSString>> = shown
                .to_vec()
                .iter()
                .filter_map(|delivered| ended_id(delivered, &notice))
                .collect();
            if !ended.is_empty() {
                UNUserNotificationCenter::currentNotificationCenter()
                    .removeDeliveredNotificationsWithIdentifiers(&NSArray::from_retained_slice(
                        &ended,
                    ));
            }
            this.deliver(&content);
        });
        UNUserNotificationCenter::currentNotificationCenter()
            .getDeliveredNotificationsWithCompletionHandler(&block);
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

/// The identifier of `delivered` when `notice` ends its wait, read from the
/// ids this extension put in its user info.
fn ended_id(delivered: &UNNotification, notice: &PushNotice) -> Option<Retained<NSString>> {
    let request = delivered.request();
    let info = request.content().userInfo();
    let kind = info_string(&info, "kind");
    let host = info_string(&info, "host");
    let session_id = info_string(&info, "session_id");
    let waited_since = info_string(&info, "waited_since");
    let shown = Drawn {
        kind: kind.as_deref(),
        host: host.as_deref(),
        session_id: session_id.as_deref(),
        waited_since: waited_since.as_deref(),
    };
    clearing::ends(notice, shown).then(|| request.identifier())
}

/// The string `info` holds under `key`, when it holds one.
fn info_string(info: &NSDictionary<AnyObject, AnyObject>, key: &str) -> Option<String> {
    let value = info.objectForKey(&NSString::from_str(key))?;
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
