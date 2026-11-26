//! The phone's viewer page: the last file a fetch saved on the phone, drawn
//! over the page shown under a header with Back, the file's name, its host
//! and its path on that host. Its body is the desktop viewer's, read under
//! the reserved tab [`PHONE_VIEWER_ID`]; Back or a committed back gesture
//! closes it and shows the page under it again.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{
    App, Bounds, ClickEvent, Context, Div, Entity, FontWeight, MouseButton, MouseDownEvent,
    ObjectFit, PinchEvent, Pixels, Point, RetainAllImageCache, Rgba, ScrollDelta, ScrollEvent,
    Window, canvas, div, prelude::*, px,
};

use super::PhoneRoot;
use super::home_view::{STATUS_TEXT_SIZE, TOUCH_TARGET};
use super::hosts_view::phone_button;
use super::image_zoom::ImageZoom;
use super::settings_view::{BACK_LABEL, header_frame};
use super::share::SHARE_LABEL;
use crate::PhoneFetched;
use crate::buttons::ButtonKind;
use crate::driver_id::DriverId as _;
use crate::fonts::{ui_px, ui_weight};
use crate::html_media::is_html_page;
use crate::spawn_form::human_size;
use crate::ui_theme::{UiTheme, theme};
use crate::viewer_tab::{MarkdownMode, ViewerLoad};
use crate::viewer_tab_view::{PHONE_VIEWER_ID, viewer_body_id};
use crate::viewer_text::is_image_path;

/// The line under the header while the copy may be out of date.
const STALE_TEXT: &str = "This copy may be out of date: the host did not answer.";

/// The card's line for a file the text view cannot show.
const NOT_SHOWN_TEXT: &str = "This file can't be shown here.";

/// The file the viewer page shows, as its header names it.
pub(super) struct PhoneViewer {
    /// The copy's absolute path.
    pub(super) path: PathBuf,
    /// The copy's length on disk, in bytes.
    pub(super) size: u64,
    /// The copy's file name.
    pub(super) name: String,
    /// The host as the fetch's toasts name it.
    pub(super) host_label: String,
    /// The path the fetch asked the host for.
    pub(super) host_path: String,
    /// Whether the copy may be out of date.
    pub(super) stale: bool,
    /// The image cache this open draws through: its own, so a re-fetched
    /// copy at the same path loads afresh.
    pub(super) images: Entity<RetainAllImageCache>,
    /// The image page's zoom, back at the fit size on every open, and `None`
    /// for a copy whose file names no pixel size: its page neither zooms nor
    /// moves, so its card stays centred.
    pub(super) zoom: Option<ImageZoom>,
    /// The image area's bounds in the last frame it drew, which the zoom's
    /// gestures measure against; empty until it first draws.
    pub(super) bounds: Rc<Cell<Bounds<Pixels>>>,
}

impl PhoneRoot {
    /// Opens the viewer page on the copy `saved` names, over the page shown;
    /// a web page, which the phone cannot render, also goes to the share
    /// sheet.
    pub(super) fn open_viewer(&mut self, saved: PhoneFetched, cx: &mut Context<Self>) {
        let copies = match self.root.read(cx).remote_files_root() {
            Ok(copies) => copies.to_path_buf(),
            Err(err) => {
                tracing::warn!(error = %err, "phone: no copies folder; the saved fetch not shown");
                return;
            }
        };
        let path = copies.join(&saved.local_path);
        let name = path.file_name().map_or_else(
            || saved.local_path.clone(),
            |name| name.to_string_lossy().into_owned(),
        );
        let size = std::fs::metadata(&path).map_or(saved.size, |meta| meta.len());
        let zoom = is_image_path(&path).then(|| open_zoom(&path)).flatten();
        let images = RetainAllImageCache::new(cx);
        self.root
            .update(cx, |root, cx| root.open_phone_viewer(path.clone(), cx));
        if is_html_page(&path.to_string_lossy()) {
            self.share.present(&path);
        }
        self.viewer = Some(PhoneViewer {
            path,
            size,
            name,
            host_label: saved.host_label,
            host_path: saved.host_path,
            stale: false,
            images,
            zoom,
            bounds: Rc::new(Cell::new(Bounds::default())),
        });
    }

    /// Back on the viewer page: closes it, showing the page under it.
    pub(super) fn viewer_back(&mut self, cx: &mut Context<Self>) {
        self.close_viewer(cx);
        cx.notify();
    }

    /// Closes the viewer page, if open, and drops its reserved tab.
    pub(super) fn close_viewer(&mut self, cx: &mut Context<Self>) {
        if self.viewer.take().is_some() {
            self.root.update(cx, |root, _| root.close_phone_viewer());
        }
    }

    /// The viewer page's header, as name, host label, host path and whether
    /// the copy may be out of date; `None` while the page is closed.
    #[must_use]
    pub fn viewer_header(&self) -> Option<(String, String, String, bool)> {
        self.viewer.as_ref().map(|viewer| {
            (
                viewer.name.clone(),
                viewer.host_label.clone(),
                viewer.host_path.clone(),
                viewer.stale,
            )
        })
    }

    /// The image cache the viewer page draws through, while it is open: a
    /// fresh one for every open.
    #[doc(hidden)]
    #[must_use]
    pub fn viewer_image_cache(&self) -> Option<Entity<RetainAllImageCache>> {
        self.viewer.as_ref().map(|viewer| viewer.images.clone())
    }

    /// While the viewer page is open: its header over the file's body, the
    /// text body or, for a file it cannot show, the image or the card.
    pub(super) fn viewer_page(&self, cx: &mut Context<Self>) -> Option<Div> {
        let viewer = self.viewer.as_ref()?;
        let t = theme(cx);
        let (banner, lines, not_text) = self.root.update(cx, |root, cx| {
            let not_text = root.viewer_tab(PHONE_VIEWER_ID).is_some_and(|tab| {
                matches!(
                    tab.load(),
                    ViewerLoad::TooLarge | ViewerLoad::Binary | ViewerLoad::NotDecodable
                )
            });
            let rendered = root
                .viewer_tab(PHONE_VIEWER_ID)
                .is_some_and(|tab| tab.markdown() == Some(MarkdownMode::Rendered));
            let (banner, lines) = root
                .viewer_body(PHONE_VIEWER_ID, false, cx)
                .unwrap_or_default();
            // The numbered lines sit inside the side inset; rendered
            // Markdown and the banner keep their own padding.
            let lines = lines.map(|lines| {
                if rendered {
                    lines
                } else {
                    lines.pl(px(super::home_view::SIDE_PADDING))
                }
            });
            (banner, lines, not_text)
        });
        let stale = viewer.stale.then(|| {
            div()
                .driver_id(|| "phone-viewer-stale".to_owned())
                .flex_none()
                .px(px(super::home_view::SIDE_PADDING))
                .py(px(6.0))
                .text_size(ui_px(cx, STATUS_TEXT_SIZE))
                .text_color(gpui::rgb(t.muted))
                .child(STALE_TEXT)
        });
        let look = Self::card_look(&t, cx);
        let body = if is_image_path(&viewer.path) {
            Self::viewer_image(viewer, look, cx)
        } else if not_text {
            Self::viewer_card(viewer, look)
        } else {
            div()
                .driver_id(|| viewer_body_id(PHONE_VIEWER_ID))
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
                .children(banner)
                .children(lines)
        };
        Some(
            div()
                .driver_id(|| "phone-viewer".to_owned())
                .size_full()
                .flex()
                .flex_col()
                .child(Self::viewer_header_row(viewer, &t, cx))
                .children(stale)
                .child(body),
        )
    }

    /// The viewer page's body for an image: the copy drawn scaled to fit the
    /// area and keeping its aspect, or the card when the copy cannot be
    /// decoded. A pinch zooms it about the fingers, a drag pans it while
    /// zoomed, and a double tap switches it between the fit size and twice
    /// it; the area the picture is drawn in never moves. A copy whose file
    /// names no pixel size draws as before the zoom existed: across the area
    /// with its card centred, and neither gesture does anything.
    fn viewer_image(viewer: &PhoneViewer, look: CardLook, cx: &mut Context<Self>) -> Div {
        let Some(zoom) = viewer.zoom else {
            return div()
                .driver_id(|| "phone-viewer-image".to_owned())
                .flex_1()
                .min_h_0()
                .flex()
                .child(image_element(viewer, look));
        };
        let area = area_of(viewer.bounds.get());
        let mut shown = zoom;
        shown.clamp(area);
        let (width, height) = shown.box_size(area);
        let (offset_x, offset_y) = shown.offset();
        div()
            .driver_id(|| "phone-viewer-image".to_owned())
            .relative()
            .overflow_hidden()
            .flex_1()
            .min_h_0()
            .on_pinch(cx.listener(|this, event: &PinchEvent, _, cx| {
                this.zoom_image_pinch(event.position, event.delta, cx);
            }))
            .on_scroll(cx.listener(|this, event: &ScrollEvent, _, cx| {
                this.zoom_image_pan(event, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    if event.click_count == 2 {
                        this.zoom_image_double_tap(event.position, cx);
                    }
                }),
            )
            .child(
                div()
                    .driver_id(|| "phone-viewer-image-zoom".to_owned())
                    .absolute()
                    .top(px(offset_y))
                    .left(px(offset_x))
                    .w(px(width))
                    .h(px(height))
                    .child(image_element(viewer, look)),
            )
            .child(area_canvas(viewer.bounds.clone(), cx.entity()))
    }

    /// The viewer page's body for a file the text view cannot show: the card
    /// filling the page's remaining space.
    fn viewer_card(viewer: &PhoneViewer, look: CardLook) -> Div {
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .p(px(super::home_view::SIDE_PADDING))
            .child(file_card(&viewer.name, viewer.size, look))
    }

    /// The look the viewer page's file card draws with.
    fn card_look(t: &UiTheme, cx: &mut Context<Self>) -> CardLook {
        CardLook {
            text_size: ui_px(cx, STATUS_TEXT_SIZE),
            weight: ui_weight(cx, FontWeight::SEMIBOLD),
            muted: gpui::rgb(t.muted),
            border: gpui::rgb(t.border),
            surface: gpui::rgb(t.surface),
        }
    }

    /// The viewer page's header: Back, then the file's name over its host
    /// and its path on that host, then Share.
    fn viewer_header_row(viewer: &PhoneViewer, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let back = phone_button("phone-viewer-back", ButtonKind::Outlined, cx)
            .min_h(px(TOUCH_TARGET))
            .flex_none()
            .child(BACK_LABEL)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.viewer_back(cx)));
        let small = ui_px(cx, STATUS_TEXT_SIZE);
        let muted = gpui::rgb(t.muted);
        let names = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .driver_id(|| "phone-viewer-title".to_owned())
                    .truncate()
                    .font_weight(ui_weight(cx, FontWeight::SEMIBOLD))
                    .child(viewer.name.clone()),
            )
            .child(
                div()
                    .driver_id(|| "phone-viewer-host".to_owned())
                    .truncate()
                    .text_size(small)
                    .text_color(muted)
                    .child(viewer.host_label.clone()),
            )
            .child(
                div()
                    .driver_id(|| "phone-viewer-path".to_owned())
                    .truncate()
                    .text_size(small)
                    .text_color(muted)
                    .child(viewer.host_path.clone()),
            );
        let share = phone_button("phone-viewer-share", ButtonKind::Outlined, cx)
            .min_h(px(TOUCH_TARGET))
            .flex_none()
            .child(SHARE_LABEL)
            .on_click(cx.listener(|this, _: &ClickEvent, _, _| this.share_viewer()));
        header_frame(t).child(back).child(names).child(share)
    }

    /// A pinch on the image page: zooms the image about `position`, the
    /// window point the fingers hold.
    fn zoom_image_pinch(&mut self, position: Point<Pixels>, delta: f32, cx: &mut Context<Self>) {
        if let Some(viewer) = self.viewer.as_mut() {
            let bounds = viewer.bounds.get();
            if let Some(zoom) = viewer.zoom.as_mut() {
                zoom.pinch(delta, in_area(bounds, position), area_of(bounds));
            }
        }
        cx.notify();
    }

    /// A drag on the image page: pans the zoomed image by the scroll's pixel
    /// delta, and moves nothing at the fit size.
    fn zoom_image_pan(&mut self, event: &ScrollEvent, cx: &mut Context<Self>) {
        let ScrollDelta::Pixels(delta) = event.delta else {
            return;
        };
        if let Some(viewer) = self.viewer.as_mut() {
            let area = area_of(viewer.bounds.get());
            if let Some(zoom) = viewer.zoom.as_mut() {
                zoom.pan(in_pixels(delta.x), in_pixels(delta.y), area);
            }
        }
        cx.notify();
    }

    /// A double tap on the image page: switches the image between the fit
    /// size and twice it about the tapped point.
    fn zoom_image_double_tap(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if let Some(viewer) = self.viewer.as_mut() {
            let bounds = viewer.bounds.get();
            if let Some(zoom) = viewer.zoom.as_mut() {
                zoom.double_tap(in_area(bounds, position), area_of(bounds));
            }
        }
        cx.notify();
    }

    /// Draws the open image again after its area drew at new bounds: the
    /// picture follows the area's size, and its zoom is kept inside it.
    fn redraw_image(&mut self, cx: &mut Context<Self>) {
        let Some(viewer) = self.viewer.as_mut() else {
            return;
        };
        if let Some(zoom) = viewer.zoom.as_mut() {
            zoom.clamp(area_of(viewer.bounds.get()));
        }
        cx.notify();
    }
}

/// The colours and text metrics the viewer page's file card draws with.
#[derive(Clone, Copy)]
struct CardLook {
    /// The card's smaller lines.
    text_size: Pixels,
    /// The file name's weight.
    weight: FontWeight,
    /// The smaller lines' colour.
    muted: Rgba,
    /// The card's border.
    border: Rgba,
    /// The card's fill.
    surface: Rgba,
}

/// The card saying `name` cannot be shown here, with its `size` under it.
fn file_card(name: &str, size: u64, look: CardLook) -> Div {
    div()
        .driver_id(|| "phone-viewer-card".to_owned())
        .flex()
        .flex_col()
        .items_center()
        .gap(px(6.0))
        .max_w_full()
        .p(px(16.0))
        .rounded(px(10.0))
        .border_1()
        .border_color(look.border)
        .bg(look.surface)
        .child(
            div()
                .driver_id(|| "phone-viewer-card-name".to_owned())
                .max_w_full()
                .truncate()
                .font_weight(look.weight)
                .child(name.to_owned()),
        )
        .child(
            div()
                .driver_id(|| "phone-viewer-card-size".to_owned())
                .text_size(look.text_size)
                .text_color(look.muted)
                .child(human_size(size)),
        )
        .child(
            div()
                .driver_id(|| "phone-viewer-card-note".to_owned())
                .text_size(look.text_size)
                .text_color(look.muted)
                .child(NOT_SHOWN_TEXT),
        )
}

/// `card` centred in the space the page gave it, as the image's fallback
/// draws it.
fn card_frame(card: Div) -> Div {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p(px(super::home_view::SIDE_PADDING))
        .child(card)
}

/// `p` as a number of pixels, for the zoom's arithmetic.
fn in_pixels(p: Pixels) -> f32 {
    p / px(1.0)
}

/// `bounds`' size, as the zoom takes its area.
fn area_of(bounds: Bounds<Pixels>) -> (f32, f32) {
    (in_pixels(bounds.size.width), in_pixels(bounds.size.height))
}

/// `position`, measured from `bounds`' top-left corner, in pixels.
fn in_area(bounds: Bounds<Pixels>, position: Point<Pixels>) -> (f32, f32) {
    (
        in_pixels(position.x - bounds.origin.x),
        in_pixels(position.y - bounds.origin.y),
    )
}

/// The copy's element, or the card when it cannot be decoded.
fn image_element(viewer: &PhoneViewer, look: CardLook) -> impl IntoElement {
    let name = viewer.name.clone();
    let size = viewer.size;
    gpui::img(viewer.path.clone())
        .image_cache(&viewer.images)
        .object_fit(ObjectFit::Contain)
        .size_full()
        .with_fallback(move || card_frame(file_card(&name, size, look)).into_any_element())
}

/// The zoom the image page opens with over `path`, at the fit size, or
/// `None` when the copy's file names no pixel size: its page neither zooms
/// nor moves, so its card stays centred.
fn open_zoom(path: &Path) -> Option<ImageZoom> {
    match image::image_dimensions(path) {
        Ok(dimensions) => Some(ImageZoom::new(image_size(dimensions))),
        Err(err) => {
            tracing::debug!(
                error = %err,
                "phone: the copy names no image size; its page does not zoom"
            );
            None
        }
    }
}

/// `(width, height)` in pixels, as the zoom's floats.
#[expect(
    clippy::cast_precision_loss,
    reason = "an image's sides are far below 2^24"
)]
fn image_size((width, height): (u32, u32)) -> (f32, f32) {
    (width as f32, height as f32)
}

/// The image area's own bounds, written where the zoom reads them each
/// frame they change, so a gesture measures against the box the image fills
/// and the page draws the image again at the area's new size.
fn area_canvas(bounds: Rc<Cell<Bounds<Pixels>>>, phone: Entity<PhoneRoot>) -> impl IntoElement {
    canvas(
        move |drawn: Bounds<Pixels>, window: &mut Window, cx: &mut App| {
            if bounds.get() != drawn {
                bounds.set(drawn);
                let phone = phone.clone();
                window.defer(cx, move |_, cx| {
                    phone.update(cx, PhoneRoot::redraw_image);
                });
            }
        },
        |_, (), _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}
