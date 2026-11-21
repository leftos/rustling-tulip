//! The composer's Paste on iOS 16 and later: a system `UIPasteControl` laid
//! over the GPUI Paste button. iOS takes a paste as the user's own only
//! through its system paste UI; at "Paste from Other Apps: Ask" it refuses a
//! programmatic `UIPasteboard` read outright, while the control's tap pastes
//! with no prompt.
//!
//! The Paste button records its bounds each frame ([`record_button`]); at
//! the end of `PhoneRoot`'s frame [`frame_end`] places the control over
//! them, or hides it when the button was not drawn or a sheet covers the
//! page. The control pastes into [`PasteTarget`], a `UIView` whose
//! `pasteItemProviders:` loads the first item as an image (PNG or JPEG) or
//! text; the load completes on a queue of the system's choosing, so its
//! result goes back through a channel to a task on the GPUI side, which
//! drops it when the session page it was pasted on is no longer shown.
//! iOS 15 has no `UIPasteControl` and keeps the GPUI button's own read.

use std::cell::{Cell, OnceCell, RefCell};

use block2::RcBlock;
use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{App, Bounds, Context, ImageFormat, Pixels, WeakEntity};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{
    NSArray, NSData, NSError, NSItemProvider, NSObject, NSOperatingSystemVersion, NSProcessInfo,
    NSString,
};
use objc2_ui_kit::{
    UIApplication, UIButtonConfigurationCornerStyle, UIColor, UIPasteConfiguration,
    UIPasteConfigurationSupporting as _, UIPasteControl, UIPasteControlConfiguration,
    UIPasteControlDisplayMode, UIResponder, UIView,
};

use super::PhoneRoot;
use super::pasteboard::{
    IMAGE_TYPE, JPEG_TYPE, PNG_TYPE, PasteKind, PastedImage, TEXT_TYPES, paste_kind, pasted_name,
};
use super::photos_ios::{as_jpeg, fitted};
use crate::buttons::ButtonSize;
use crate::text_input::normalize_newlines;

/// The first iOS with `UIPasteControl`.
const FIRST_MAJOR: isize = 16;

/// The type identifiers the control offers to paste.
const ACCEPTED: [&str; 5] = [
    "public.utf8-plain-text",
    "public.plain-text",
    PNG_TYPE,
    JPEG_TYPE,
    IMAGE_TYPE,
];

/// What a control paste loaded, for the session page it was pasted on.
enum Pasted {
    /// Text, to go in at the composer's caret.
    Text { session: String, text: String },
    /// An image, to upload as the clipboard's image paste does.
    Image { session: String, image: PastedImage },
}

impl Pasted {
    /// The session whose page the paste was made on.
    fn session(&self) -> &str {
        match self {
            Self::Text { session, .. } | Self::Image { session, .. } => session,
        }
    }
}

/// The control's colours: the Paste button's text and the ground under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PasteColors {
    /// The label's colour.
    pub(super) text: u32,
    /// The control's fill.
    pub(super) ground: u32,
}

// SAFETY:
// - `pasteItemProviders:` overrides `UIResponder`'s method with its declared
//   signature; `UIKit` calls it on the main thread.
// - `PasteTarget` does not implement `Drop`.
define_class!(
    #[unsafe(super(UIView, UIResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RTPasteTarget"]
    struct PasteTarget;

    impl PasteTarget {
        #[unsafe(method(pasteItemProviders:))]
        fn paste_item_providers(&self, providers: &NSArray<NSItemProvider>) {
            pasted(providers);
        }
    }
);

/// The control and what it pastes into, made on first use.
struct Control {
    view: Retained<UIPasteControl>,
    /// The control holds its target weakly, so this keeps it alive.
    target: Retained<PasteTarget>,
    colors: PasteColors,
}

/// The control's one owner, on the main thread.
#[derive(Default)]
struct Owner {
    control: Option<Control>,
    /// Where a paste's result goes to the GPUI side, once its task runs.
    sender: Option<UnboundedSender<Pasted>>,
    /// The session whose Paste button the control sits over.
    session: Option<String>,
    shown: bool,
}

thread_local! {
    /// The Paste button's bounds in the frame being drawn, taken by
    /// [`frame_end`].
    static DRAWN: Cell<Option<Bounds<Pixels>>> = const { Cell::new(None) };
    static OWNER: RefCell<Owner> = RefCell::new(Owner::default());
    static SUPPORTED: OnceCell<bool> = const { OnceCell::new() };
}

/// Whether this iOS has `UIPasteControl` (16 and later), so the control,
/// not the GPUI button, takes the Paste tap.
pub(super) fn supported() -> bool {
    SUPPORTED.with(|cell| {
        *cell.get_or_init(|| {
            let version = NSOperatingSystemVersion {
                majorVersion: FIRST_MAJOR,
                minorVersion: 0,
                patchVersion: 0,
            };
            NSProcessInfo::processInfo().isOperatingSystemAtLeastVersion(version)
        })
    })
}

/// The Paste button was drawn at `bounds` in this frame.
pub(super) fn record_button(bounds: Bounds<Pixels>) {
    DRAWN.with(|drawn| drawn.set(Some(bounds)));
}

/// The end of `PhoneRoot`'s frame: the control goes over the Paste button
/// drawn in it for `session`, or hides when none was drawn, no session is
/// shown or a sheet `covered` the page.
pub(super) fn frame_end(
    covered: bool,
    session: Option<String>,
    root: WeakEntity<PhoneRoot>,
    colors: PasteColors,
    cx: &mut App,
) {
    let drawn = DRAWN.with(Cell::take);
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    match (drawn, session) {
        (Some(bounds), Some(session)) if !covered && supported() => {
            listen(root, cx);
            place(mtm, bounds, session, colors);
        }
        _ => hide(),
    }
}

/// Starts the task that applies each paste on the GPUI side, once.
fn listen(root: WeakEntity<PhoneRoot>, cx: &mut App) {
    if OWNER.with(|owner| owner.borrow().sender.is_some()) {
        return;
    }
    let (tx, mut rx) = unbounded::<Pasted>();
    OWNER.with(|owner| owner.borrow_mut().sender = Some(tx));
    cx.spawn(async move |cx| {
        while let Some(pasted) = rx.next().await {
            if root.update(cx, |this, cx| apply(this, pasted, cx)).is_err() {
                tracing::debug!("phone: a paste arrived after the phone's root was gone");
                return;
            }
        }
    })
    .detach();
}

/// Puts the control over `bounds` for `session` and shows it, making it on
/// first use (or anew when the theme's colours changed).
fn place(mtm: MainThreadMarker, bounds: Bounds<Pixels>, session: String, colors: PasteColors) {
    OWNER.with(|owner| {
        let mut owner = owner.borrow_mut();
        let stale = owner
            .control
            .as_ref()
            .is_some_and(|control| control.colors != colors);
        if stale && let Some(old) = owner.control.take() {
            old.view.removeFromSuperview();
            old.target.removeFromSuperview();
        }
        if owner.control.is_none() {
            owner.control = make(mtm, colors);
        }
        let Some(control) = owner.control.as_ref() else {
            return;
        };
        control.view.setFrame(rect(bounds));
        if !owner.shown {
            control.view.setHidden(false);
            tracing::info!("phone: paste control shown");
        }
        owner.shown = true;
        owner.session = Some(session);
    });
}

/// Hides the control, when it is up.
fn hide() {
    OWNER.with(|owner| {
        let mut owner = owner.borrow_mut();
        if !owner.shown {
            return;
        }
        if let Some(control) = owner.control.as_ref() {
            control.view.setHidden(true);
        }
        owner.shown = false;
        owner.session = None;
        tracing::info!("phone: paste control hidden");
    });
}

/// `bounds` as a `UIKit` rect: GPUI's logical pixels are points on iOS.
fn rect(bounds: Bounds<Pixels>) -> CGRect {
    let point = |value: Pixels| f64::from(f32::from(value));
    CGRect::new(
        CGPoint::new(point(bounds.origin.x), point(bounds.origin.y)),
        CGSize::new(point(bounds.size.width), point(bounds.size.height)),
    )
}

/// `rgb` (`0xRRGGBB`) as an opaque `UIColor`.
fn color(rgb: u32) -> Retained<UIColor> {
    let channel = |shift: u32| f64::from((rgb >> shift) & 0xFF) / 255.0;
    UIColor::colorWithRed_green_blue_alpha(channel(16), channel(8), channel(0), 1.0)
}

/// The control, label only in the button's colours, and its target, both
/// added to the key window's root view controller's view; `None` when there
/// is no such view.
fn make(mtm: MainThreadMarker, colors: PasteColors) -> Option<Control> {
    let Some(view) = root_view(mtm) else {
        tracing::warn!("phone: no root view to put the paste control in");
        return None;
    };
    let this = PasteTarget::alloc(mtm).set_ivars(());
    // SAFETY: `UIView`'s `init` takes no arguments and the class adds no
    // state of its own.
    let target: Retained<PasteTarget> = unsafe { msg_send![super(this), init] };
    let types: Vec<Retained<NSString>> = ACCEPTED
        .iter()
        .map(|kind| NSString::from_str(kind))
        .collect();
    let accepted = NSArray::from_retained_slice(&types);
    let paste = UIPasteConfiguration::initWithAcceptableTypeIdentifiers(
        UIPasteConfiguration::alloc(mtm),
        &accepted,
    );
    target.setPasteConfiguration(Some(&paste));
    let configuration = UIPasteControlConfiguration::new(mtm);
    configuration.setDisplayMode(UIPasteControlDisplayMode::LabelOnly);
    configuration.setCornerStyle(UIButtonConfigurationCornerStyle::Fixed);
    configuration.setCornerRadius(f64::from(ButtonSize::Large.radius() - 1.0));
    configuration.setBaseForegroundColor(Some(&color(colors.text)));
    configuration.setBaseBackgroundColor(Some(&color(colors.ground)));
    let control = UIPasteControl::initWithConfiguration(UIPasteControl::alloc(mtm), &configuration);
    let responder: &UIResponder = &target;
    control.setTarget(Some(ProtocolObject::from_ref(responder)));
    control.setHidden(true);
    view.addSubview(&target);
    view.addSubview(&control);
    tracing::info!("phone: paste control made");
    Some(Control {
        view: control,
        target,
        colors,
    })
}

/// The key window's root view controller's view (found as the photo
/// picker finds its window).
fn root_view(mtm: MainThreadMarker) -> Option<Retained<UIView>> {
    #[expect(
        deprecated,
        reason = "the app does not hold its window scene; gpui_ios opens one window"
    )]
    let windows = UIApplication::sharedApplication(mtm).windows();
    let window = windows
        .iter()
        .find(|window| window.isKeyWindow())
        .or_else(|| windows.firstObject())?;
    window.rootViewController()?.view()
}

/// The control's tap, on the main thread: the first item is loaded as what
/// [`paste_kind`] reads it as, and the result sent to the GPUI side for the
/// session the control sits over.
fn pasted(providers: &NSArray<NSItemProvider>) {
    let (session, sender) = OWNER.with(|owner| {
        let owner = owner.borrow();
        (owner.session.clone(), owner.sender.clone())
    });
    let (Some(session), Some(sender), Some(provider)) = (session, sender, providers.firstObject())
    else {
        tracing::info!("phone: a control paste had nothing to paste into");
        return;
    };
    let offered: Vec<String> = provider
        .registeredTypeIdentifiers()
        .iter()
        .map(|kind| kind.to_string())
        .collect();
    let types: Vec<&str> = offered.iter().map(String::as_str).collect();
    let kind = paste_kind(&types);
    tracing::info!(types = ?types, kind = ?kind, "phone: control paste");
    let load_as = match kind {
        PasteKind::Image(ImageFormat::Png) => PNG_TYPE,
        PasteKind::Image(_) if types.contains(&JPEG_TYPE) => JPEG_TYPE,
        PasteKind::Image(_) => IMAGE_TYPE,
        PasteKind::Text => TEXT_TYPES
            .into_iter()
            .find(|kind| types.contains(kind))
            .unwrap_or(TEXT_TYPES[0]),
        PasteKind::Nothing => return,
    };
    load(&provider, load_as, move |bytes| {
        let session = session.clone();
        let image = match kind {
            PasteKind::Image(ImageFormat::Png) => Some(fitted(bytes, ImageFormat::Png)),
            PasteKind::Image(_) => as_jpeg(bytes),
            _ => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                send(&sender, Pasted::Text { session, text });
                return;
            }
        };
        if let Some(image) = image {
            send(&sender, Pasted::Image { session, image });
        }
    });
}

/// Loads `provider`'s data as `kind` and hands the bytes to `done`, on the
/// queue the provider completes on; a failed load is logged.
fn load(provider: &NSItemProvider, kind: &str, done: impl Fn(Vec<u8>) + 'static) {
    let block = RcBlock::new(move |data: *mut NSData, error: *mut NSError| {
        // SAFETY: the provider hands the completion a live data object, or
        // null with a live error, for the call.
        let (data, error) = unsafe { (data.as_ref(), error.as_ref()) };
        if let Some(data) = data {
            done(data.to_vec());
        } else {
            let why = error.map(|error| error.localizedDescription().to_string());
            tracing::warn!(error = ?why, "phone: the pasted item could not be loaded");
        }
    });
    // SAFETY: the completion touches only the paste channel, `NSData` and
    // `UIImage`, which are safe on the queue it runs on.
    let _progress = unsafe {
        provider.loadDataRepresentationForTypeIdentifier_completionHandler(
            &NSString::from_str(kind),
            &block,
        )
    };
}

/// Sends a paste's result to the GPUI side; one whose task is gone is
/// dropped.
fn send(sender: &UnboundedSender<Pasted>, pasted: Pasted) {
    if sender.unbounded_send(pasted).is_err() {
        tracing::debug!("phone: a paste loaded after its task was gone");
    }
}

/// Applies a control paste on the GPUI side, while its session's page is
/// still the one shown: text goes in at the composer's caret, an image
/// uploads as the clipboard's image paste does, to a host that takes
/// uploads.
fn apply(this: &mut PhoneRoot, pasted: Pasted, cx: &mut Context<PhoneRoot>) {
    let shown = this.shown.as_ref().map(|(_, session)| session.as_str());
    if shown != Some(pasted.session()) {
        tracing::info!("phone: a paste for a page no longer shown was dropped");
        return;
    }
    match pasted {
        Pasted::Text { session, text } => {
            let Some(input) = this.root.read(cx).phone_composer_input(&session) else {
                return;
            };
            let text = normalize_newlines(&text);
            input.update(cx, |input, cx| input.replace_from_app(&text, cx));
        }
        Pasted::Image {
            session,
            image: (bytes, format),
        } => {
            if !super::session_view::takes_uploads(this.root.read(cx), &session) {
                tracing::info!(session = %session, "phone: the host takes no uploads; the pasted image was dropped");
                return;
            }
            let name = pasted_name(format);
            this.root.update(cx, |root, cx| {
                root.on_chat_paste_image(&session, &name, &bytes, cx);
            });
        }
    }
}
