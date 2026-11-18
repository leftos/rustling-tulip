//! The phone's viewer page: the last file a fetch saved on the phone, drawn
//! over the page shown under a header with Back, the file's name, its host
//! and its path on that host. Its body is the desktop viewer's, read under
//! the reserved tab [`PHONE_VIEWER_ID`]; Back or a committed back gesture
//! closes it and shows the page under it again.

use gpui::{ClickEvent, Context, Div, FontWeight, div, prelude::*, px};

use super::PhoneRoot;
use super::home_view::{STATUS_TEXT_SIZE, TOUCH_TARGET};
use super::hosts_view::phone_button;
use super::settings_view::{BACK_LABEL, header_frame};
use crate::PhoneFetched;
use crate::buttons::ButtonKind;
use crate::driver_id::DriverId as _;
use crate::fonts::{ui_px, ui_weight};
use crate::ui_theme::{UiTheme, theme};
use crate::viewer_tab_view::{PHONE_VIEWER_ID, viewer_body_id};

/// The line under the header while the copy may be out of date.
const STALE_TEXT: &str = "This copy may be out of date: the host did not answer.";

/// The file the viewer page shows, as its header names it.
pub(super) struct PhoneViewer {
    /// The copy's file name.
    pub(super) name: String,
    /// The host as the fetch's toasts name it.
    pub(super) host_label: String,
    /// The path the fetch asked the host for.
    pub(super) host_path: String,
    /// Whether the copy may be out of date.
    pub(super) stale: bool,
}

impl PhoneRoot {
    /// Opens the viewer page on the copy `saved` names, over the page shown.
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
        self.root
            .update(cx, |root, cx| root.open_phone_viewer(path, cx));
        self.viewer = Some(PhoneViewer {
            name,
            host_label: saved.host_label,
            host_path: saved.host_path,
            stale: false,
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

    /// While the viewer page is open: its header over the file's body.
    pub(super) fn viewer_page(&self, cx: &mut Context<Self>) -> Option<Div> {
        let viewer = self.viewer.as_ref()?;
        let t = theme(cx);
        let (banner, lines) = self
            .root
            .update(cx, |root, cx| root.viewer_body(PHONE_VIEWER_ID, false, cx))
            .unwrap_or((None, None));
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
        Some(
            div()
                .driver_id(|| "phone-viewer".to_owned())
                .size_full()
                .flex()
                .flex_col()
                .child(Self::viewer_header_row(viewer, &t, cx))
                .children(stale)
                .child(
                    div()
                        .driver_id(|| viewer_body_id(PHONE_VIEWER_ID))
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
                        .children(banner)
                        .children(lines),
                ),
        )
    }

    /// The viewer page's header: Back, then the file's name over its host
    /// and its path on that host.
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
        header_frame(t).child(back).child(names)
    }
}
