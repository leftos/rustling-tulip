//! The extension's principal class, `RTShareViewController`: a
//! `UIViewController` that, once its view has loaded, writes each item the
//! share brings into the App Group's share inbox ([`Inbox`]) and then
//! completes the request. [`classify`] decides from each provider's
//! registered types whether it is a file (copied from the provider's
//! temporary file, or from its file URL), a link or text; a file over
//! [`FILE_MAX_BYTES`](rustling_tulip_ios_share::FILE_MAX_BYTES) is skipped.
//! The request completes once, when every provider has answered or after
//! [`DEADLINE_SECS`], whichever comes first; a later answer writes nothing.
//! Once at least one item is written, it asks iOS to open the app
//! ([`OPEN_APP_URL`](rustling_tulip_ios_share::OPEN_APP_URL)) through the
//! responder chain, as an extension cannot reach `UIApplication` directly;
//! the app reads the inbox when it comes to the foreground.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use block2::Block;
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchTime, MainThreadBound};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObjectProtocol, ProtocolObject};
use objc2::{ClassType, MainThreadOnly, Message, define_class, msg_send};
use objc2_foundation::{
    NSData, NSDictionary, NSError, NSExtensionContext, NSExtensionItem, NSFileManager,
    NSItemProvider, NSObject, NSSecureCoding, NSString, NSURL,
};
use objc2_ui_kit::{UIApplication, UIResponder, UIViewController};
use rustling_tulip_ios_share::{
    ClearOnce, Countdown, DEADLINE_SECS, FILE_URL_TYPE, GROUP_FOLDER, GROUP_ID, Inbox, ItemKind,
    OPEN_APP_URL, TEXT_TYPE, TextKind, URL_TYPE, WriteError, Written, classify, file_type, now_ms,
};

define_class!(
    // SAFETY:
    // - UIViewController asks a subclass that overrides viewDidLoad to call the superclass's
    //   implementation, which this class does first.
    // - RTShareViewController does not implement Drop.
    #[unsafe(super(UIViewController, UIResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RTShareViewController"]
    struct RTShareViewController;

    impl RTShareViewController {
        #[unsafe(method(viewDidLoad))]
        fn view_did_load(&self) {
            // SAFETY: viewDidLoad takes no arguments and returns nothing.
            let () = unsafe { msg_send![super(self), viewDidLoad] };
            self.take_items();
        }
    }

    unsafe impl NSObjectProtocol for RTShareViewController {}
);

impl RTShareViewController {
    /// Writes every shared item into the inbox, then completes the request
    /// once: when the last provider answers or after [`DEADLINE_SECS`],
    /// whichever comes first. With no inbox or no items it completes at once.
    fn take_items(&self) {
        let Some(context) = self.extensionContext() else {
            tracing::warn!("share: the view controller has no extension context");
            return;
        };
        let providers = providers(&context);
        let pending = Arc::new(Pending {
            countdown: Countdown::new(providers.len()),
            written: Written::default(),
            cleared: ClearOnce::default(),
            context: MainThreadBound::new(context, self.mtm()),
            controller: MainThreadBound::new(self.retain(), self.mtm()),
        });
        let Some(dir) = group_inbox() else {
            tracing::warn!("share: the App Group container is not available");
            pending.expire();
            return;
        };
        if providers.is_empty() {
            pending.expire();
            return;
        }
        start_deadline(&pending);
        let inbox = Arc::new(Inbox::new(dir));
        for provider in providers {
            take(&provider, &inbox, &pending);
        }
    }
}

/// Completes the request after [`DEADLINE_SECS`] on the main queue, unless
/// the last provider has already answered.
fn start_deadline(pending: &Arc<Pending>) {
    let timer = Arc::clone(pending);
    let nanos = Duration::from_secs(DEADLINE_SECS).as_nanos();
    let when = DispatchTime::NOW.time(i64::try_from(nanos).unwrap_or(i64::MAX));
    let queued = DispatchQueue::main().after(when, move || {
        if !timer.countdown.is_done() {
            tracing::warn!("share: the deadline passed before every attachment answered");
        }
        timer.expire();
    });
    if queued.is_err() {
        tracing::warn!("share: the deadline could not be queued");
    }
}

/// The request, its [`Countdown`] and the [`ClearOnce`] that empties an
/// earlier share before this share's first write: it completes once, on the
/// main thread, opening the app first when an item was [`Written`].
struct Pending {
    countdown: Countdown,
    written: Written,
    cleared: ClearOnce,
    context: MainThreadBound<Retained<NSExtensionContext>>,
    controller: MainThreadBound<Retained<RTShareViewController>>,
}

impl Pending {
    /// Whether the request is complete, so an answer writes nothing.
    fn is_done(&self) -> bool {
        self.countdown.is_done()
    }

    /// One provider has answered; the last completes the request.
    fn answer(self: &Arc<Self>) {
        if self.countdown.answer() {
            self.complete();
        }
    }

    /// The deadline passed, or there is nothing to wait for.
    fn expire(self: &Arc<Self>) {
        if self.countdown.expire() {
            self.complete();
        }
    }

    /// On a later turn of the main queue, never inside the caller (which may
    /// be `viewDidLoad`, before the view is in a window and its responder
    /// chain reaches the app): asks iOS to open the app when an item was
    /// written, then completes the request, returning no items.
    fn complete(self: &Arc<Self>) {
        let pending = Arc::clone(self);
        DispatchQueue::main().exec_async(move || {
            pending.controller.get_on_main(|controller| {
                if pending.written.any() {
                    open_app(controller);
                }
                let context = pending.context.get(controller.mtm());
                // SAFETY: no items are returned and no completion is asked for.
                unsafe { context.completeRequestReturningItems_completionHandler(None, None) };
            });
        });
    }
}

/// Asks iOS to open [`OPEN_APP_URL`] through the `UIApplication` found up
/// `controller`'s responder chain, as an extension cannot call
/// `sharedApplication`.
fn open_app(controller: &RTShareViewController) {
    let Some(app) = application(controller) else {
        tracing::warn!("share: no UIApplication in the responder chain; the app is not opened");
        return;
    };
    let Some(url) = NSURL::URLWithString(&NSString::from_str(OPEN_APP_URL)) else {
        tracing::warn!("share: the app's URL does not parse; the app is not opened");
        return;
    };
    let options: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::new();
    let handler: Option<&Block<dyn Fn(Bool)>> = None;
    // SAFETY: `app` is a UIApplication, which answers openURL:options:completionHandler:
    // with a URL, an options dictionary (empty, so no key or value can be of the wrong type)
    // and a nullable completion block, and returns nothing; this runs on the main thread.
    unsafe {
        let () = msg_send![&*app, openURL: &*url, options: &*options, completionHandler: handler];
    }
    tracing::info!("share: asked iOS to open the app");
}

/// The first responder up `controller`'s chain that is the `UIApplication`.
fn application(controller: &RTShareViewController) -> Option<Retained<UIResponder>> {
    let mut next = controller.nextResponder();
    while let Some(responder) = next {
        if responder.isKindOfClass(UIApplication::class()) {
            return Some(responder);
        }
        next = responder.nextResponder();
    }
    None
}

/// Every attachment of every input item.
fn providers(context: &NSExtensionContext) -> Vec<Retained<NSItemProvider>> {
    context
        .inputItems()
        .to_vec()
        .into_iter()
        .filter_map(|item| item.downcast::<NSExtensionItem>().ok())
        .filter_map(|item| item.attachments())
        .flat_map(|attachments| attachments.to_vec())
        .collect()
}

/// The share inbox in the App Group container, as the app finds it;
/// `None` while the extension is not signed into the group.
fn group_inbox() -> Option<PathBuf> {
    let container = NSFileManager::defaultManager()
        .containerURLForSecurityApplicationGroupIdentifier(&NSString::from_str(GROUP_ID))?;
    let path = container.path()?;
    Some(PathBuf::from(path.to_string()).join(GROUP_FOLDER))
}

/// Loads `provider`'s item into the inbox as [`classify`] decides from its
/// registered types; one that is none of them just answers.
fn take(provider: &NSItemProvider, inbox: &Arc<Inbox>, pending: &Arc<Pending>) {
    let names: Vec<String> = provider
        .registeredTypeIdentifiers()
        .to_vec()
        .iter()
        .map(ToString::to_string)
        .collect();
    let types: Vec<&str> = names.iter().map(String::as_str).collect();
    let (inbox, pending) = (Arc::clone(inbox), Arc::clone(pending));
    match classify(&types) {
        Some(ItemKind::File) => match file_type(&types) {
            Some(kind) => load_file(provider, kind, inbox, pending),
            None => load_item(provider, FILE_URL_TYPE, TextKind::Url, inbox, pending),
        },
        Some(ItemKind::Url) => load_item(provider, URL_TYPE, TextKind::Url, inbox, pending),
        Some(ItemKind::Text) => load_item(provider, TEXT_TYPE, TextKind::Text, inbox, pending),
        None => {
            tracing::info!("share: an attachment is no file, link or text; it is passed over");
            pending.answer();
        }
    }
}

/// Copies `provider`'s file, loaded as type `kind`, into the inbox inside
/// the load's completion, as the temporary file is removed once it returns.
fn load_file(provider: &NSItemProvider, kind: &str, inbox: Arc<Inbox>, pending: Arc<Pending>) {
    let suggested = provider.suggestedName().map(|name| name.to_string());
    let block = RcBlock::new(move |url: *mut NSURL, error: *mut NSError| {
        // SAFETY: the provider hands the completion a live URL, or null with a live error,
        // for the call.
        let (url, error) = unsafe { (url.as_ref(), error.as_ref()) };
        if pending.is_done() {
            tracing::warn!("share: a file answered after the request completed; it is dropped");
        } else if let Some(path) = url.and_then(NSURL::path) {
            copy_in(
                &inbox,
                Path::new(&path.to_string()),
                suggested.as_deref(),
                &pending,
            );
        } else {
            let why = error.map(|error| error.localizedDescription().to_string());
            tracing::warn!(error = ?why, "share: a shared file could not be loaded");
        }
        pending.answer();
    });
    // SAFETY: the completion touches only the inbox, the countdown and the request, which
    // are safe on the queue it runs on.
    let _progress = unsafe {
        provider.loadFileRepresentationForTypeIdentifier_completionHandler(
            &NSString::from_str(kind),
            &block,
        )
    };
}

/// Clears an earlier share from the inbox before this share's first write;
/// a later write clears nothing, so this share's own items are kept.
fn clear_earlier(inbox: &Inbox, pending: &Pending) {
    match pending.cleared.clear(inbox) {
        Ok(Some(removed)) if removed > 0 => {
            tracing::info!(removed, "share: an earlier pending share is replaced");
        }
        Ok(_) => {}
        Err(err) => {
            tracing::warn!(error = %err, "share: the earlier pending share could not be cleared");
        }
    }
}

/// Writes the file at `path` into the inbox, named as its own file name or
/// else `suggested`, and records it in `pending`; a file over the cap is
/// skipped.
fn copy_in(inbox: &Inbox, path: &Path, suggested: Option<&str>, pending: &Pending) {
    let own = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let name = own.as_deref().or(suggested).unwrap_or_default();
    clear_earlier(inbox, pending);
    match inbox.write_file(path, name, now_ms()) {
        Ok(id) => {
            pending.written.record();
            tracing::info!(id, "share: a file is in the inbox");
        }
        Err(WriteError::TooLarge(bytes)) => {
            tracing::warn!(
                bytes,
                "share: a shared file is over the 64 MiB cap; it is skipped"
            );
        }
        Err(err) => tracing::warn!(error = %err, "share: a shared file could not be written"),
    }
}

/// Loads `provider`'s item of type `type_id` into the inbox as a `kind`
/// item, or as a file when it is a file URL.
fn load_item(
    provider: &NSItemProvider,
    type_id: &str,
    kind: TextKind,
    inbox: Arc<Inbox>,
    pending: Arc<Pending>,
) {
    let block = RcBlock::new(
        move |item: *mut ProtocolObject<dyn NSSecureCoding>, error: *mut NSError| {
            // SAFETY: the provider hands the completion a live item, or null with a live
            // error, for the call.
            let (item, error) = unsafe { (item.cast::<AnyObject>().as_ref(), error.as_ref()) };
            if pending.is_done() {
                tracing::warn!(
                    "share: an item answered after the request completed; it is dropped"
                );
            } else {
                write_loaded(&inbox, kind, item, error, &pending);
            }
            pending.answer();
        },
    );
    // SAFETY: the block is live for the call and the provider copies it; the completion
    // touches only the inbox, the countdown and the request, which are safe on the queue it
    // runs on.
    unsafe {
        provider.loadItemForTypeIdentifier_options_completionHandler(
            &NSString::from_str(type_id),
            None,
            RcBlock::as_ptr(&block),
        );
    }
}

/// Writes what a load gave, `item`, into the inbox: a file URL's file as a
/// file item, anything else holding text as a `kind` item, each recorded in
/// `pending`; with no item, `error` says why.
fn write_loaded(
    inbox: &Inbox,
    kind: TextKind,
    item: Option<&AnyObject>,
    error: Option<&NSError>,
    pending: &Pending,
) {
    let Some(item) = item else {
        let why = error.map(|error| error.localizedDescription().to_string());
        tracing::warn!(error = ?why, "share: a link or text could not be loaded");
        return;
    };
    if let Some(url) = item.downcast_ref::<NSURL>()
        && url.isFileURL()
    {
        if let Some(path) = url.path() {
            copy_in(inbox, Path::new(&path.to_string()), None, pending);
        } else {
            tracing::warn!("share: a shared file URL has no path");
        }
        return;
    }
    let Some(text) = item_text(item) else {
        tracing::warn!("share: a shared link or text holds no string");
        return;
    };
    clear_earlier(inbox, pending);
    match inbox.write_text(kind, &text, now_ms()) {
        Ok(id) => {
            pending.written.record();
            tracing::info!(id, "share: a link or text is in the inbox");
        }
        Err(err) => tracing::warn!(error = %err, "share: a link or text could not be written"),
    }
}

/// The text a loaded item holds: a URL's absolute string, a string, or
/// data that is UTF-8.
fn item_text(item: &AnyObject) -> Option<String> {
    if let Some(url) = item.downcast_ref::<NSURL>() {
        return url.absoluteString().map(|text| text.to_string());
    }
    if let Some(data) = item.downcast_ref::<NSData>() {
        return String::from_utf8(data.to_vec()).ok();
    }
    item.downcast_ref::<NSString>().map(ToString::to_string)
}

/// Registers the class with the Objective-C runtime before `NSExtensionMain`
/// looks the principal class up by name.
extern "C" fn register_class() {
    let _ = RTShareViewController::class();
}

/// Runs [`register_class`] as one of the executable's initializers.
#[used]
#[unsafe(link_section = "__DATA,__mod_init_func")]
static REGISTER_CLASS: extern "C" fn() = register_class;
