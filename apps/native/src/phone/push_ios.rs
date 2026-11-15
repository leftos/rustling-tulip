//! The phone's push on iOS: the notification permission from
//! `UNUserNotificationCenter`, the APNs device token from `UIKit`'s remote
//! notification registration, and the install's push keys in the Keychain.
//!
//! `gpui_ios`'s app delegate answers no remote-notification callback, so the
//! first device token asked adds `didRegister...` and `didFailToRegister...`
//! to its class at run time; each logs that `UIKit` called it. Every
//! completion runs on a thread of the system's choosing and answers through a
//! oneshot channel.
//!
//! A background push wakes the app through
//! `didReceiveRemoteNotification:fetchCompletionHandler:`, added by
//! [`add_wake_method`] in [`crate::phone::run`] before the app starts, once the
//! app has registered the platform's delegate classes, with the launch
//! callback's call as a second try: a clear notice removes its session's Needs
//! you notification and sets the badge.

use std::ffi::CStr;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use block2::{DynBlock, RcBlock};
use dispatch2::{DispatchQueue, MainThreadBound};
use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::BoxFuture;
use gpui::App;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
use objc2::{available, sel};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSError, NSString};
use objc2_ui_kit::{UIApplication, UIApplicationState, UIBackgroundFetchResult};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNAuthorizationStatus, UNNotification, UNNotificationSettings,
    UNUserNotificationCenter,
};
use protocol::{ApnsEnvironment, PushNotice, PushNoticeKind};
use push_crypto::{InstallKeys, KEYCHAIN_ACCOUNT, KEYCHAIN_SERVICE};
use rustling_tulip_ios_notify::notice;
use security_framework::access_control::{ProtectionMode, SecAccessControl};
use security_framework::passwords::{
    PasswordOptions, delete_generic_password_options, generic_password,
    set_generic_password_options,
};

use super::push::{Authorization, Delivered, PushPlatform, build_environment};
use super::sweep::{self, Sweep};

/// The launch argument that asks for provisional notification permission at
/// start, with no prompt, then registers for the device token: the
/// simulator's hands-off push check (`tools/ios/sim-notify.ps1`) passes it.
pub const PUSH_PROVISIONAL_ARG: &str = "--push-provisional";

/// The class `gpui_ios` registers as the app's delegate.
const DELEGATE_CLASS: &std::ffi::CStr = c"GPUIIosAppDelegate";

/// The Keychain's `errSecItemNotFound`.
const ITEM_NOT_FOUND: i32 = -25300;

/// What a device token request answers: the token hex, or why not.
type TokenAnswer = Result<String, String>;

/// Every device token request waiting on `UIKit`'s callback; one callback
/// answers them all.
static WAITING: Mutex<Vec<oneshot::Sender<TokenAnswer>>> = Mutex::new(Vec::new());

/// Whether the delegate methods were added, made once on first use.
static HOOK: OnceLock<Result<(), String>> = OnceLock::new();

/// The iOS push platform.
pub struct IosPushPlatform;

impl PushPlatform for IosPushPlatform {
    fn supported(&self) -> bool {
        true
    }

    fn environment(&self) -> ApnsEnvironment {
        build_environment()
    }

    fn authorization(&self) -> BoxFuture<'static, Authorization> {
        let (tx, rx) = oneshot::channel();
        let tx = Mutex::new(Some(tx));
        let block = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
            // SAFETY: the system hands the completion a live settings object for the call.
            let status = unsafe { settings.as_ref() }.authorizationStatus();
            answer_once(&tx, from_status(status));
        });
        UNUserNotificationCenter::currentNotificationCenter()
            .getNotificationSettingsWithCompletionHandler(&block);
        rx.map(|answer| answer.unwrap_or(Authorization::NotDetermined))
            .boxed()
    }

    fn request_authorization(&self) -> BoxFuture<'static, Authorization> {
        request(
            UNAuthorizationOptions::Alert
                | UNAuthorizationOptions::Sound
                | UNAuthorizationOptions::Badge,
        )
    }

    fn device_token(&self) -> BoxFuture<'static, Result<String, String>> {
        let Some(mtm) = MainThreadMarker::new() else {
            return futures::future::ready(Err(
                "the device token was asked off the main thread".to_owned()
            ))
            .boxed();
        };
        add_wake_method();
        if let Err(err) = HOOK.get_or_init(add_delegate_methods) {
            return futures::future::ready(Err(err.clone())).boxed();
        }
        let (tx, rx) = oneshot::channel();
        let first = {
            let mut waiting = waiting();
            waiting.push(tx);
            waiting.len() == 1
        };
        // A request while another waits joins its wait.
        if first {
            UIApplication::sharedApplication(mtm).registerForRemoteNotifications();
        }
        rx.map(|answer| {
            answer.unwrap_or_else(|_| Err("the device token request was dropped".to_owned()))
        })
        .boxed()
    }

    fn install_keys(&self) -> Result<InstallKeys, String> {
        match generic_password(keys_item()) {
            Ok(bytes) => match InstallKeys::from_bytes(&bytes) {
                Ok(keys) => return Ok(keys),
                Err(err) => {
                    tracing::warn!(error = %err, "phone: the install's push keys item is malformed; making new keys");
                    // A malformed item is replaced; one already gone needs no delete.
                    let _gone = delete_generic_password_options(keys_item());
                }
            },
            Err(err) if err.code() == ITEM_NOT_FOUND => {}
            Err(err) => return Err(format!("the install's push keys could not be read: {err}")),
        }
        make_keys()
    }

    fn delivered(&self) -> BoxFuture<'static, Vec<Delivered>> {
        let (tx, rx) = oneshot::channel();
        let tx = Mutex::new(Some(tx));
        let block = RcBlock::new(move |shown: NonNull<NSArray<UNNotification>>| {
            // SAFETY: the system hands the completion a live array for the call.
            let shown = unsafe { shown.as_ref() };
            answer_once(
                &tx,
                shown.to_vec().iter().map(|n| delivered_of(n)).collect(),
            );
        });
        UNUserNotificationCenter::currentNotificationCenter()
            .getDeliveredNotificationsWithCompletionHandler(&block);
        rx.map(|answer| {
            answer.unwrap_or_else(|_| {
                tracing::debug!("phone: the delivered notifications were never given");
                Vec::new()
            })
        })
        .boxed()
    }

    fn remove_delivered(&self, ids: Vec<String>) {
        let ids: Vec<Retained<NSString>> = ids.iter().map(|id| NSString::from_str(id)).collect();
        UNUserNotificationCenter::currentNotificationCenter()
            .removeDeliveredNotificationsWithIdentifiers(&NSArray::from_retained_slice(&ids));
    }

    fn set_badge(&self, count: u32) {
        let count = isize::try_from(count).unwrap_or(isize::MAX);
        if available!(ios = 16.0) {
            let block = RcBlock::new(|error: *mut NSError| {
                // SAFETY: the system passes a live NSError for the call, or nil.
                if let Some(error) = unsafe { error.as_ref() } {
                    tracing::debug!(code = error.code(), "phone: the badge could not be set");
                }
            });
            UNUserNotificationCenter::currentNotificationCenter()
                .setBadgeCount_withCompletionHandler(count, Some(&block));
            return;
        }
        let Some(mtm) = MainThreadMarker::new() else {
            tracing::debug!("phone: the badge was set off the main thread");
            return;
        };
        #[expect(deprecated, reason = "iOS 15 has no setBadgeCount")]
        UIApplication::sharedApplication(mtm).setApplicationIconBadgeNumber(count);
    }
}

/// `shown` as the sweep reads it: its request's identifier and the `host`,
/// `session_id` and `kind` strings the notification extension put in its user
/// info.
fn delivered_of(shown: &UNNotification) -> Delivered {
    let request = shown.request();
    let info = request.content().userInfo();
    Delivered {
        id: request.identifier().to_string(),
        host: info_string(&info, "host"),
        session_id: info_string(&info, "session_id"),
        kind: info_string(&info, "kind"),
        waited_since: info_string(&info, "waited_since"),
    }
}

/// The string `info` holds under `key`, when it holds one.
fn info_string(info: &NSDictionary, key: &str) -> Option<String> {
    let value = info.objectForKey(&NSString::from_str(key))?;
    let text = value.downcast::<NSString>().ok()?;
    Some(text.to_string())
}

/// The Keychain item that holds the install's push keys, no access group
/// named: on a device the app's one entitled group, which the extension
/// reads; the simulator has none.
fn keys_item() -> PasswordOptions {
    PasswordOptions::new_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
}

/// New install keys, written to the Keychain readable after the first
/// unlock, on this device only.
fn make_keys() -> Result<InstallKeys, String> {
    let keys = InstallKeys::generate().map_err(|err| format!("{err:#}"))?;
    let access = SecAccessControl::create_with_protection(
        Some(ProtectionMode::AccessibleAfterFirstUnlockThisDeviceOnly),
        0,
    )
    .map_err(|err| format!("the push keys' access control could not be made: {err}"))?;
    let mut options = keys_item();
    options.set_access_control(access);
    set_generic_password_options(&keys.to_bytes(), options)
        .map_err(|err| format!("the install's push keys could not be written: {err}"))?;
    Ok(keys)
}

/// Asks for the notification permission with `options` and gives the answer.
fn request(options: UNAuthorizationOptions) -> BoxFuture<'static, Authorization> {
    let (tx, rx) = oneshot::channel();
    let tx = Mutex::new(Some(tx));
    let block = RcBlock::new(move |granted: Bool, _error: *mut NSError| {
        let answer = if granted.as_bool() {
            Authorization::Granted
        } else {
            Authorization::Denied
        };
        answer_once(&tx, answer);
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .requestAuthorizationWithOptions_completionHandler(options, &block);
    rx.map(|answer| answer.unwrap_or(Authorization::NotDetermined))
        .boxed()
}

/// Sends `answer` through the sender in `slot`, the first time only.
fn answer_once<T>(slot: &Mutex<Option<oneshot::Sender<T>>>, answer: T) {
    let sender = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(sender) = sender {
        // The receiver is gone only when nobody waits any more.
        let _unheard = sender.send(answer);
    }
}

/// The permission a settings status means: provisional and ephemeral count
/// as granted; a status this build does not know reads as not asked, so the
/// switch asks and the system answers.
fn from_status(status: UNAuthorizationStatus) -> Authorization {
    match status {
        UNAuthorizationStatus::Denied => Authorization::Denied,
        UNAuthorizationStatus::Authorized
        | UNAuthorizationStatus::Provisional
        | UNAuthorizationStatus::Ephemeral => Authorization::Granted,
        _ => Authorization::NotDetermined,
    }
}

/// The device token requests waiting on `UIKit`.
fn waiting() -> MutexGuard<'static, Vec<oneshot::Sender<TokenAnswer>>> {
    WAITING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Answers every waiting device token request with `answer`.
fn answer_all(answer: &TokenAnswer) {
    let senders = std::mem::take(&mut *waiting());
    for sender in senders {
        // A receiver gone means its request was dropped.
        let _unheard = sender.send(answer.clone());
    }
}

/// `bytes` as lowercase hex.
fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        for nibble in [byte >> 4, byte & 0x0f] {
            // A nibble is always a hex digit.
            text.push(char::from_digit(u32::from(nibble), 16).unwrap_or('0'));
        }
    }
    text
}

/// The Objective-C signature of [`did_register`].
type Registered = extern "C-unwind" fn(*mut AnyObject, Sel, *mut UIApplication, *mut NSData);

/// The Objective-C signature of [`did_fail`].
type Failed = extern "C-unwind" fn(*mut AnyObject, Sel, *mut UIApplication, *mut NSError);

/// `application:didRegisterForRemoteNotificationsWithDeviceToken:`.
extern "C-unwind" fn did_register(
    _this: *mut AnyObject,
    _cmd: Sel,
    _app: *mut UIApplication,
    token: *mut NSData,
) {
    // SAFETY: UIKit passes the token as a live NSData for the call, or nil.
    let token = unsafe { token.as_ref() };
    tracing::info!("phone: the app delegate received a device token");
    answer_all(
        &token
            .map(|data| hex(&data.to_vec()))
            .ok_or_else(|| "UIKit gave no device token".to_owned()),
    );
}

/// `application:didFailToRegisterForRemoteNotificationsWithError:`.
extern "C-unwind" fn did_fail(
    _this: *mut AnyObject,
    _cmd: Sel,
    _app: *mut UIApplication,
    error: *mut NSError,
) {
    // SAFETY: UIKit passes the error as a live NSError for the call, or nil.
    let error = unsafe { error.as_ref() };
    let domain = error.map(|error| error.domain().to_string());
    let code = error.map(NSError::code);
    tracing::info!(
        domain = ?domain,
        code = ?code,
        "phone: the app delegate failed to register for pushes"
    );
    let text = error.map_or_else(
        || "the registration failed with no error".to_owned(),
        |error| error.localizedDescription().to_string(),
    );
    answer_all(&Err(text));
}

/// Adds the two remote-notification callbacks to `gpui_ios`'s delegate class;
/// adding a method flushes the runtime's method caches, so `UIKit` finds them.
///
/// # Errors
///
/// The class is not registered, or already answers either selector.
fn add_delegate_methods() -> Result<(), String> {
    let class = AnyClass::get(DELEGATE_CLASS).ok_or("the app delegate class is not registered")?;
    // SAFETY: both are C-ABI functions; the runtime calls each with the `v@:@@` signature it
    // is added under, which is the one each declares.
    let (registered, failed) = unsafe {
        (
            std::mem::transmute::<Registered, Imp>(did_register as Registered),
            std::mem::transmute::<Failed, Imp>(did_fail as Failed),
        )
    };
    add_method(
        class,
        sel!(application:didRegisterForRemoteNotificationsWithDeviceToken:),
        registered,
        c"v@:@@",
    )?;
    add_method(
        class,
        sel!(application:didFailToRegisterForRemoteNotificationsWithError:),
        failed,
        c"v@:@@",
    )
}

/// Adds `imp` to `class` as `name`, under the Objective-C type string
/// `types`.
///
/// # Errors
///
/// The class already answers `name`, so its own method would take the call
/// and ours would never run.
fn add_method(class: &AnyClass, name: Sel, imp: Imp, types: &CStr) -> Result<(), String> {
    // SAFETY: `class` is a registered class, every caller passes an `imp` that takes the
    // signature `types` names, and the runtime copies the type string.
    let added = unsafe {
        objc2::ffi::class_addMethod(
            std::ptr::from_ref(class).cast_mut(),
            name,
            imp,
            types.as_ptr(),
        )
    };
    if added.as_bool() {
        Ok(())
    } else {
        Err(format!("the app delegate already answers {name:?}"))
    }
}

/// The block `application:didReceiveRemoteNotification:fetchCompletionHandler:`
/// is handed, which must be called once.
type FetchHandler = DynBlock<dyn Fn(UIBackgroundFetchResult)>;

/// [`FetchHandler`], copied off the stack so it outlives the call.
type HeldFetchHandler = RcBlock<dyn Fn(UIBackgroundFetchResult)>;

/// The Objective-C signature of [`did_wake`].
type Woken = extern "C-unwind" fn(
    *mut AnyObject,
    Sel,
    *mut UIApplication,
    *mut NSDictionary,
    *mut FetchHandler,
);

/// Whether the background-push callback was added, tried once.
static WAKE_HOOK: OnceLock<()> = OnceLock::new();

/// Adds `application:didReceiveRemoteNotification:fetchCompletionHandler:`
/// to `gpui_ios`'s delegate class, once. [`crate::phone::run`] calls it before
/// the app starts, after the app has registered the delegate classes, so a
/// background push that cold-launches the app finds it; `push_platform`'s call
/// inside the launch callback is a second try for a run where the class was
/// still missing. A failure is logged, and the foreground sweep still clears.
pub fn add_wake_method() {
    let Some(class) = AnyClass::get(DELEGATE_CLASS) else {
        tracing::warn!(
            "phone: the app delegate class is not registered yet; the background-push method waits"
        );
        return;
    };
    WAKE_HOOK.get_or_init(|| match add_wake(class) {
        Ok(()) => tracing::info!("phone: background wake method added"),
        Err(err) => {
            tracing::warn!(error = %err, "phone: a background push will not clear notifications");
        }
    });
}

/// [`add_wake_method`]'s one try.
///
/// # Errors
///
/// The class already answers the selector.
fn add_wake(class: &AnyClass) -> Result<(), String> {
    // SAFETY: a C-ABI function; the runtime calls it with the `v@:@@@?` signature it is added
    // under, which is the one it declares.
    let woken = unsafe { std::mem::transmute::<Woken, Imp>(did_wake as Woken) };
    add_method(
        class,
        sel!(application:didReceiveRemoteNotification:fetchCompletionHandler:),
        woken,
        c"v@:@@@?",
    )
}

/// `application:didReceiveRemoteNotification:fetchCompletionHandler:`: a
/// background push woke the app. A clear removes its session's Needs you
/// notification and sets the badge; anything else does nothing, and so does
/// any push while the app is in front, where the foreground sweep owns the
/// drawer and the badge. The completion runs once on every path, with no
/// network I/O before it.
extern "C-unwind" fn did_wake(
    _this: *mut AnyObject,
    _cmd: Sel,
    _app: *mut UIApplication,
    info: *mut NSDictionary,
    handler: *mut FetchHandler,
) {
    // SAFETY: UIKit passes the completion as a live block for the call, or nil.
    let Some(handler) = (unsafe { handler.as_ref() }) else {
        tracing::debug!("phone: a background push came with no completion");
        return;
    };
    let handler = handler.copy();
    // UIKit calls the delegate on the main thread.
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::debug!("phone: a background push came off the main thread; nothing done");
        handler.call((UIBackgroundFetchResult::NoData,));
        return;
    };
    if UIApplication::sharedApplication(mtm).applicationState() == UIApplicationState::Active {
        tracing::debug!("phone: a background clear while in front; the sweep handles it");
        handler.call((UIBackgroundFetchResult::NoData,));
        return;
    }
    // SAFETY: UIKit passes the user info as a live dictionary for the call, or nil.
    let info = unsafe { info.as_ref() };
    match cleared_notice(info) {
        Some(notice) => clear_delivered(notice, MainThreadBound::new(handler, mtm)),
        None => handler.call((UIBackgroundFetchResult::NoData,)),
    }
}

/// The clear notice in a background push's user info `info`, decrypted with
/// the install's push keys as the extension does; `None`, logged, when the
/// push does not decrypt or is not a clear.
fn cleared_notice(info: Option<&NSDictionary>) -> Option<PushNotice> {
    let e = info.and_then(|info| info_string(info, "e"));
    // Read only: a wake never makes keys, which would orphan this install's subscription.
    let keys = generic_password(keys_item()).ok();
    let read = match notice::decrypted(e.as_deref(), keys.as_deref()) {
        Ok(read) => read,
        Err(why) => {
            tracing::debug!(why = ?why, "phone: a background push did not decrypt; nothing cleared");
            return None;
        }
    };
    if read.notice.kind != PushNoticeKind::Cleared {
        tracing::debug!("phone: a background push is not a clear; nothing done");
        return None;
    }
    Some(read.notice)
}

/// Reads the delivered notifications, removes those the clear `notice`
/// covers and sets the badge on the main queue (iOS 15 sets a badge only
/// there), then calls `handler` from that same block, once.
fn clear_delivered(notice: PushNotice, handler: MainThreadBound<HeldFetchHandler>) {
    let handler = Mutex::new(Some(handler));
    let block = RcBlock::new(move |shown: NonNull<NSArray<UNNotification>>| {
        // SAFETY: the system hands the completion a live array for the call.
        let shown = unsafe { shown.as_ref() };
        let delivered: Vec<Delivered> = shown.to_vec().iter().map(|n| delivered_of(n)).collect();
        let cleared = sweep::clear(&notice, &delivered);
        let held = handler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(held) = held {
            DispatchQueue::main().exec_async(move || {
                let result = cleared.map_or(UIBackgroundFetchResult::NoData, apply_clear);
                held.get_on_main(|handler| handler.call((result,)));
            });
        }
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .getDeliveredNotificationsWithCompletionHandler(&block);
}

/// Does what a clear decided and says what the wake fetched; runs on the
/// main queue.
fn apply_clear(cleared: Sweep) -> UIBackgroundFetchResult {
    tracing::debug!(
        removed = cleared.remove.len(),
        "phone: a background clear ran"
    );
    let platform = IosPushPlatform;
    let acted = !cleared.remove.is_empty() || cleared.badge.is_some();
    if !cleared.remove.is_empty() {
        platform.remove_delivered(cleared.remove);
    }
    if let Some(badge) = cleared.badge {
        platform.set_badge(badge);
    }
    if acted {
        UIBackgroundFetchResult::NewData
    } else {
        UIBackgroundFetchResult::NoData
    }
}

/// With [`PUSH_PROVISIONAL_ARG`]: asks for provisional notification
/// permission now, with no prompt, logs the answer, then asks for the device
/// token and logs that it came, never the token.
pub fn start_provisional(platform: Arc<IosPushPlatform>, cx: &mut App) {
    let asked = request(
        UNAuthorizationOptions::Alert
            | UNAuthorizationOptions::Sound
            | UNAuthorizationOptions::Badge
            | UNAuthorizationOptions::Provisional,
    );
    cx.spawn(async move |_cx| {
        let granted = asked.await == Authorization::Granted;
        let answer = if granted { "granted" } else { "refused" };
        tracing::info!("phone: provisional push permission {answer}");
        if !granted {
            return;
        }
        match platform.device_token().await {
            Ok(_) => tracing::info!("phone: device token received"),
            Err(err) => tracing::warn!(error = %err, "phone: no device token"),
        }
    })
    .detach();
}
