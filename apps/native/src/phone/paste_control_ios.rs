//! The composer's Paste on iOS 16 and later: a system `UIPasteControl` laid
//! over the GPUI Paste button. iOS takes a paste as the user's own only
//! through its system paste UI; at "Paste from Other Apps: Ask" it refuses a
//! programmatic `UIPasteboard` read outright, while the control's tap pastes
//! with no prompt.
//!
//! Each Paste spot ([`PasteSpot`]) has a control of its own: the composer's
//! Paste button, the Keys dock's Paste key and the composer's edit bar's
//! Paste item (on screen together, all pasting into the composer; the
//! bar's closes it), the terminal key bar's Paste key (text only, into
//! the terminal) and each phone field's Paste button (text only, into that
//! field, on whatever page draws it: [`PasteSpot::Field`]). The edit bar and the selection's handles draw over the
//! window, so a control under them hides while they are drawn: a native
//! control sits above every GPUI view and would take their taps.
//!
//! Each Paste button or key records its bounds each frame
//! ([`record_button`]); at the end of `PhoneRoot`'s frame [`frame_end`]
//! places each drawn spot's control over them, and hides the control of a
//! spot not drawn (or a key scrolled partly out of its bar), or every
//! control when a sheet covers the page. Each control pastes into its own
//! [`PasteTarget`], a `UIView` whose `pasteItemProviders:` loads the first
//! item as an image (any image type) or text; the paste goes where the
//! spot whose control fired pastes. The load completes on a queue of the
//! system's choosing, so its
//! result goes back through a channel to a task on the GPUI side, which
//! drops it when the session page it was pasted on is no longer shown.
//! iOS 15 has no `UIPasteControl` and keeps the GPUI button's own read.

use std::cell::{OnceCell, RefCell};

use block2::RcBlock;
use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{App, Bounds, Context, Focusable as _, ImageFormat, Pixels, WeakEntity};
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
    ImageSource, PasteKind, PasteSpot, PastedImage, TEXT_TYPES, covered_spots, must_unhide,
    paste_kind, pasted_name, spots_to_hide,
};
use super::photos_ios::{as_jpeg, fitted};
use crate::buttons::ButtonSize;
use crate::text_input::normalize_newlines;

/// The first iOS with `UIPasteControl`.
const FIRST_MAJOR: isize = 16;

/// What a control paste loaded, for the session page it was pasted on.
enum Pasted {
    /// The edit bar's control was tapped, before anything it loads
    /// arrives: the composer commits its composition and closes the bar,
    /// whatever the paste yields.
    BarTapped { session: String },
    /// Text, to go in at the composer's caret; `from_bar` when the edit
    /// bar's control pasted it, which pastes as the bar's own Paste does.
    Text {
        session: String,
        text: String,
        from_bar: bool,
    },
    /// An image, to upload as the clipboard's image paste does.
    Image { session: String, image: PastedImage },
    /// Text, to paste into the session's open terminal.
    Terminal { session: String, text: String },
    /// Text, to go in at the caret of the phone field tagged `selector`.
    Field {
        selector: &'static str,
        text: String,
    },
}

impl Pasted {
    /// The session whose page the paste was made on; none for a field's.
    fn session(&self) -> Option<&str> {
        match self {
            Self::BarTapped { session }
            | Self::Text { session, .. }
            | Self::Image { session, .. }
            | Self::Terminal { session, .. } => Some(session),
            Self::Field { .. } => None,
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

/// What `PhoneRoot`'s frame hands the control at its end.
pub(super) struct PasteFrame {
    /// Whether a layer covers the page the Paste button is on.
    pub(super) covered: bool,
    /// The session whose page is shown.
    pub(super) session: Option<String>,
    /// The session the page's open terminal shows.
    pub(super) terminal: Option<String>,
    /// Whether the shown session's host takes uploads, so an image pastes
    /// at the composer.
    pub(super) uploads: bool,
    /// The colours of a control on the page: its Paste buttons and keys.
    pub(super) colors: PasteColors,
    /// The colours of a control on a popover: the edit bar's Paste item
    /// ([`PasteSpot::on_popover`]).
    pub(super) popover: PasteColors,
}

impl PasteFrame {
    /// The colours `spot`'s control takes in this frame.
    const fn colors_of(&self, spot: PasteSpot) -> PasteColors {
        if spot.on_popover() {
            self.popover
        } else {
            self.colors
        }
    }
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
            pasted(self, providers);
        }
    }
);

/// One spot's control and what it pastes into, made on the spot's first
/// placing.
struct Control {
    view: Retained<UIPasteControl>,
    /// The control holds its target weakly, so this keeps it alive.
    target: Retained<PasteTarget>,
    colors: PasteColors,
    /// The spot the control was made for, which decides what it accepts
    /// and where its paste goes.
    spot: PasteSpot,
    /// The session whose Paste the control sits over.
    session: Option<String>,
    /// Whether an image pastes there: the spot takes images and the
    /// session's host takes uploads, as the frame it was placed in found it.
    uploads: bool,
    shown: bool,
}

/// The controls' one owner, on the main thread: at most one control per
/// spot.
#[derive(Default)]
struct Owner {
    controls: Vec<Control>,
    /// Where a paste's result goes to the GPUI side, once its task runs.
    sender: Option<UnboundedSender<Pasted>>,
}

thread_local! {
    /// The Paste spots drawn in the frame being drawn and their bounds,
    /// taken by [`frame_end`].
    static DRAWN: RefCell<Vec<(PasteSpot, Bounds<Pixels>)>> = const { RefCell::new(Vec::new()) };
    /// The areas touch editing drew over the window in the frame being
    /// drawn (the edit bar, the selection's handles), taken by
    /// [`frame_end`]: a control under one hides.
    static COVERS: RefCell<Vec<Bounds<Pixels>>> = const { RefCell::new(Vec::new()) };
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

/// The Paste at `spot` was drawn at `bounds` in this frame, inside the
/// visible `clip`; one the control may not cover there is not recorded.
pub(super) fn record_button(spot: PasteSpot, bounds: Bounds<Pixels>, clip: Bounds<Pixels>) {
    if !spot.shown_in(bounds, clip) {
        return;
    }
    DRAWN.with(|drawn| {
        let mut drawn = drawn.borrow_mut();
        drawn.retain(|(other, _)| *other != spot);
        drawn.push((spot, bounds));
    });
}

/// The composer's edit bar drew its Paste item at `bounds` in this frame,
/// told by its render (`TextInput`'s system paste hook); the bar draws over
/// everything, so nothing clips it.
pub(super) fn record_edit_bar_paste(bounds: Bounds<Pixels>) {
    record_button(PasteSpot::EditBar, bounds, bounds);
}

/// The composer's touch editing drew `cover` over the window in this frame
/// (the edit bar, or a selection handle's touch square), told by its
/// render: a control under it hides for the frame.
pub(super) fn record_touch_cover(cover: Bounds<Pixels>) {
    COVERS.with(|covers| covers.borrow_mut().push(cover));
}

/// The end of `PhoneRoot`'s frame: each Paste drawn in it gets its spot's
/// control, for the session that spot pastes into; the control of a spot
/// not drawn, with no session (a field's needs none) or under what touch editing drew over the
/// window ([`covered_spots`]), hides, and every control hides when a sheet
/// covered the page.
pub(super) fn frame_end(frame: &PasteFrame, root: WeakEntity<PhoneRoot>, cx: &mut App) {
    let drawn = DRAWN.with(RefCell::take);
    let covers = COVERS.with(RefCell::take);
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let mut kept: Vec<(PasteSpot, Bounds<Pixels>)> = if frame.covered || !supported() {
        Vec::new()
    } else {
        drawn
            .into_iter()
            .filter(|(spot, _)| {
                spot.pastes_without_session()
                    || spot
                        .session(frame.session.clone(), frame.terminal.clone())
                        .is_some()
            })
            .collect()
    };
    let under_touch = covered_spots(&kept, &covers);
    kept.retain(|(spot, _)| !under_touch.contains(spot));
    let kept_spots: Vec<PasteSpot> = kept.iter().map(|(spot, _)| *spot).collect();
    let up: Vec<PasteSpot> = OWNER.with(|owner| {
        let owner = owner.borrow();
        owner
            .controls
            .iter()
            .filter(|control| control.shown)
            .map(|control| control.spot)
            .collect()
    });
    for spot in spots_to_hide(&up, &kept_spots) {
        hide(spot);
    }
    if kept.is_empty() {
        return;
    }
    listen(root, cx);
    for (spot, bounds) in kept {
        place(mtm, spot, bounds, frame);
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

/// Puts `spot`'s control over `bounds` and shows it, for the session the
/// spot pastes into in `frame`, making it on the spot's first placing (or
/// anew when the theme's colours changed); no other spot's control is
/// touched.
fn place(mtm: MainThreadMarker, spot: PasteSpot, bounds: Bounds<Pixels>, frame: &PasteFrame) {
    let session = spot.session(frame.session.clone(), frame.terminal.clone());
    let uploads = spot.takes_images(frame.uploads);
    let colors = frame.colors_of(spot);
    OWNER.with(|owner| {
        let mut owner = owner.borrow_mut();
        let controls = &mut owner.controls;
        if let Some(at) = controls
            .iter()
            .position(|control| control.spot == spot && control.colors != colors)
        {
            let old = controls.swap_remove(at);
            old.view.removeFromSuperview();
            old.target.removeFromSuperview();
        }
        let remade = !controls.iter().any(|control| control.spot == spot);
        if remade {
            let Some(made) = make(mtm, colors, spot) else {
                return;
            };
            controls.push(made);
        }
        let Some(control) = controls.iter_mut().find(|control| control.spot == spot) else {
            return;
        };
        control.view.setFrame(rect(bounds));
        if must_unhide(control.shown, remade) {
            control.view.setHidden(false);
            tracing::info!(spot = ?spot, "phone: paste control shown");
        }
        control.shown = true;
        control.session = session;
        control.uploads = uploads;
    });
}

/// Hides `spot`'s control, when it is up.
fn hide(spot: PasteSpot) {
    OWNER.with(|owner| {
        let mut owner = owner.borrow_mut();
        let Some(control) = owner
            .controls
            .iter_mut()
            .find(|control| control.spot == spot && control.shown)
        else {
            return;
        };
        control.view.setHidden(true);
        control.shown = false;
        control.session = None;
        tracing::info!(spot = ?spot, "phone: paste control hidden");
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

/// The control for `spot`, label only in the button's colours, and its
/// target, which accepts what `spot` takes, both added to the key window's
/// root view controller's view; `None` when there is no such view.
fn make(mtm: MainThreadMarker, colors: PasteColors, spot: PasteSpot) -> Option<Control> {
    let Some(view) = root_view(mtm) else {
        tracing::warn!("phone: no root view to put the paste control in");
        return None;
    };
    let this = PasteTarget::alloc(mtm).set_ivars(());
    // SAFETY: `UIView`'s `init` takes no arguments and the class adds no
    // state of its own.
    let target: Retained<PasteTarget> = unsafe { msg_send![super(this), init] };
    let types: Vec<Retained<NSString>> = spot
        .accepts()
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
        spot,
        session: None,
        uploads: false,
        shown: false,
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

/// A control's tap into `target`, on the main thread: the first item is
/// loaded as what [`paste_kind`] reads it as, and the result sent to the
/// GPUI side for the session that control sits over, where its spot pastes.
fn pasted(target: &PasteTarget, providers: &NSArray<NSItemProvider>) {
    let (fired, sender) = OWNER.with(|owner| {
        let owner = owner.borrow();
        let fired = owner
            .controls
            .iter()
            .find(|control| std::ptr::eq(Retained::as_ptr(&control.target), target))
            .map(|control| (control.spot, control.session.clone(), control.uploads));
        (fired, owner.sender.clone())
    });
    let Some((spot, session, uploads)) = fired else {
        tracing::info!("phone: a control paste came from no known control");
        return;
    };
    let Some(sender) = sender else {
        tracing::info!("phone: a control paste had nothing to paste into");
        return;
    };
    if let PasteSpot::Field(selector) = spot {
        pasted_into_field(selector, providers, sender);
        return;
    }
    let Some(session) = session else {
        tracing::info!("phone: a control paste had nothing to paste into");
        return;
    };
    // The edit bar's Paste closes the bar whatever the paste yields, as the
    // bar's own Paste did, and commits the composition before any of it.
    if spot.closes_edit_bar() {
        let session = session.clone();
        send(&sender, Pasted::BarTapped { session });
    }
    let Some(provider) = providers.firstObject() else {
        tracing::info!("phone: a control paste had no item");
        return;
    };
    let offered: Vec<String> = provider
        .registeredTypeIdentifiers()
        .iter()
        .map(|kind| kind.to_string())
        .collect();
    let types: Vec<&str> = offered.iter().map(String::as_str).collect();
    let conforms =
        |kind: &str| provider.hasItemConformingToTypeIdentifier(&NSString::from_str(kind));
    let kind = paste_kind(&types, conforms, uploads);
    tracing::info!(types = ?types, kind = ?kind, uploads, spot = ?spot, "phone: control paste");
    let load_as = match kind {
        PasteKind::Image(source) => source.type_id(),
        PasteKind::Text => TEXT_TYPES
            .into_iter()
            .find(|kind| types.contains(kind))
            .unwrap_or(TEXT_TYPES[0]),
        PasteKind::Nothing => return,
    };
    let from_bar = spot.closes_edit_bar();
    load(&provider, load_as, move |bytes| {
        let session = session.clone();
        let image = match kind {
            PasteKind::Image(ImageSource::Png) => Some(fitted(bytes, ImageFormat::Png)),
            PasteKind::Image(_) => as_jpeg(bytes),
            _ => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let pasted = if spot == PasteSpot::Terminal {
                    Pasted::Terminal { session, text }
                } else {
                    Pasted::Text {
                        session,
                        text,
                        from_bar,
                    }
                };
                send(&sender, pasted);
                return;
            }
        };
        if let Some(image) = image {
            send(&sender, Pasted::Image { session, image });
        }
    });
}

/// A control's tap at the Paste of the field tagged `selector`: the first
/// item's text, when it holds any, is sent to the GPUI side for that field;
/// a field takes no image.
fn pasted_into_field(
    selector: &'static str,
    providers: &NSArray<NSItemProvider>,
    sender: UnboundedSender<Pasted>,
) {
    let Some(provider) = providers.firstObject() else {
        tracing::info!("phone: a control paste had no item");
        return;
    };
    let offered: Vec<String> = provider
        .registeredTypeIdentifiers()
        .iter()
        .map(|kind| kind.to_string())
        .collect();
    let types: Vec<&str> = offered.iter().map(String::as_str).collect();
    let conforms =
        |kind: &str| provider.hasItemConformingToTypeIdentifier(&NSString::from_str(kind));
    if paste_kind(&types, conforms, false) != PasteKind::Text {
        tracing::info!(types = ?types, selector, "phone: a field's control paste held no text");
        return;
    }
    tracing::info!(types = ?types, selector, "phone: control paste into a field");
    let load_as = TEXT_TYPES
        .into_iter()
        .find(|kind| types.contains(kind))
        .unwrap_or(TEXT_TYPES[0]);
    load(&provider, load_as, move |bytes| {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        send(&sender, Pasted::Field { selector, text });
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

/// Puts a control's pasted `text` into `session`'s composer: from the edit
/// bar as the bar's own Paste pastes (composition committed, empty text
/// changing nothing, the bar closed), else at the caret.
fn apply_text(
    this: &PhoneRoot,
    session: &str,
    text: &str,
    from_bar: bool,
    cx: &mut Context<PhoneRoot>,
) {
    let Some(input) = this.root.read(cx).phone_composer_input(session) else {
        return;
    };
    if from_bar {
        input.update(cx, |input, cx| input.paste_from_control(text, cx));
    } else {
        let text = normalize_newlines(text);
        input.update(cx, |input, cx| input.replace_from_app(&text, cx));
    }
}

/// Whether the session page `pasted` was made on is no longer the one
/// shown (or the open terminal's); a field's paste has no page, its field
/// is looked up when it lands ([`apply_field`]).
fn page_left(this: &PhoneRoot, pasted: &Pasted) -> bool {
    let shown = match pasted {
        Pasted::Field { .. } => return false,
        Pasted::Terminal { .. } => this.terminal_session(),
        _ => this.shown.as_ref().map(|(_, session)| session.clone()),
    };
    shown.as_deref() != pasted.session()
}

/// Pastes a control's `text` into the field tagged `selector`, as the
/// edit bar's Paste pastes, after giving it the keyboard; dropped when that
/// field is no longer drawn.
fn apply_field(this: &PhoneRoot, selector: &str, text: &str, cx: &mut Context<PhoneRoot>) {
    let Some(input) = this.field_input(selector) else {
        tracing::info!(
            selector,
            "phone: a paste for a field no longer drawn was dropped"
        );
        return;
    };
    let focus = input.read(cx).focus_handle(cx);
    let window = cx.active_window().or_else(|| cx.windows().first().copied());
    if let Some(window) = window
        && window
            .update(cx, |_, window, cx| focus.focus(window, cx))
            .is_err()
    {
        tracing::debug!("phone: the pasted field's window was gone");
    }
    input.update(cx, |input, cx| input.paste_from_control(text, cx));
}

/// Applies a control paste on the GPUI side, while its session's page (or
/// open terminal) is still the one shown: text goes in at the composer's
/// caret or into the terminal as its Paste key pastes, an image uploads as
/// the clipboard's image paste does, to a host that takes uploads.
fn apply(this: &mut PhoneRoot, pasted: Pasted, cx: &mut Context<PhoneRoot>) {
    if page_left(this, &pasted) {
        tracing::info!("phone: a paste for a page no longer shown was dropped");
        return;
    }
    match pasted {
        Pasted::Field { selector, text } => apply_field(this, selector, &text, cx),
        Pasted::Terminal { session, text } => {
            if text.is_empty() {
                tracing::debug!("phone: a control paste into the terminal had no text");
                return;
            }
            if let Some(pane) = this.terminal_pane(cx) {
                pane.update(cx, |pane, cx| pane.paste_text(&text, &session, cx));
            }
        }
        Pasted::BarTapped { session } => {
            if let Some(input) = this.root.read(cx).phone_composer_input(&session) {
                input.update(cx, crate::text_input::TextInput::control_paste_began);
            }
        }
        Pasted::Text {
            session,
            text,
            from_bar,
        } => apply_text(this, &session, &text, from_bar, cx),
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
