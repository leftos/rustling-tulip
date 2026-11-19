//! The phone's host browser page: a host folder listed through the host's
//! listing ([`super::browser`]), drawn over the page shown under a header
//! with Back, the host, the folder's path and Show hidden. A parent row
//! goes up (none at a root), a folder row enters the folder, and a file row
//! fetches the file and opens it in the viewer, as a kept copy opens. The
//! next page is asked for once the list is drawn near its last entry; Back
//! or a committed back gesture closes the page. A long press on a folder row
//! opens a sheet whose Fetch folder fetches the folder whole into the
//! copies folder; the line under the header says how that fetch went.

use chrono::{DateTime, Local};
use gpui::{
    BackGestureEvent, ClickEvent, Context, Div, FontWeight, Pixels, ScrollEvent, ScrollHandle,
    SharedString, Stateful, Window, div, prelude::*, px,
};
use protocol::DirEntry;

use super::PhoneRoot;
use super::browser::{self, Answer, Browser, Event, Taken};
use super::home_view::{SIDE_PADDING, STATUS_TEXT_SIZE, TOUCH_TARGET};
use super::hosts_view::{host_open, phone_button};
use super::long_press::long_press_area;
use super::settings_view::{BACK_LABEL, CHEVRON, header_frame};
use super::sheet::CANCEL_LABEL;
use crate::buttons::ButtonKind;
use crate::driver_id::DriverId as _;
use crate::fonts::{ui_px, ui_weight};
use crate::spawn_form::human_size;
use crate::target::HostKey;
use crate::ui_theme::{UiTheme, theme};

/// The header's toggle that shows the hidden entries.
const SHOW_HIDDEN_LABEL: &str = "Show hidden";

/// The parent row's name.
const PARENT_LABEL: &str = "..";

/// Why a host without the host-browse feature lists nothing.
const NO_BROWSE_ERROR: &str = "this host does not list folders";

/// The folder sheet's row that fetches the folder whole.
const FETCH_FOLDER_LABEL: &str = "Fetch folder";

/// The browser page: its state and its list's scroll.
pub(super) struct BrowserPage {
    pub(super) state: Browser,
    /// The list's scroll, fresh for every folder, so a folder opens at its
    /// top.
    scroll: ScrollHandle,
    /// The folder a long press opened the sheet on, by its name in the
    /// folder shown, while the sheet is open.
    sheet: Option<String>,
}

impl PhoneRoot {
    /// Opens the browser on `host`'s folder `path` over the page shown. A
    /// host that offers browsing is asked for its info once per connection,
    /// then the folder is listed; any other is asked nothing.
    pub fn open_browser(&mut self, host: HostKey, path: String, cx: &mut Context<Self>) {
        tracing::debug!(?host, %path, "phone: opening the host browser");
        self.close_viewer(cx);
        let watched = host.clone();
        self.root
            .update(cx, |root, _| root.phone_browse.watch(Some(watched)));
        self.browser = Some(BrowserPage {
            state: Browser::new(host, path),
            scroll: ScrollHandle::new(),
            sheet: None,
        });
        self.browser_start(cx);
        cx.notify();
    }

    /// Lists the folder shown with the host's info this connection gave, or
    /// asks for that info first.
    fn browser_start(&mut self, cx: &mut Context<Self>) {
        let Some(page) = self.browser.as_mut() else {
            return;
        };
        let host = page.state.host().clone();
        let (open, serves, facts) = {
            let root = self.root.read(cx);
            (
                host_open(root, &host),
                root.host_serves_browse(&host),
                root.phone_browse.facts(&host).cloned(),
            )
        };
        if !open {
            // The page waits; the host's Welcome resets the browser, which
            // starts again here.
            tracing::debug!(?host, "phone: the browsed host is not connected; waiting");
            return;
        }
        if !serves {
            tracing::debug!(?host, "phone: the host does not list folders");
            page.state.fail(NO_BROWSE_ERROR.to_owned());
            return;
        }
        page.state.clear_error();
        let msg = if let Some(facts) = facts {
            page.state.set_facts(facts);
            let path = page.state.path().to_owned();
            page.state.list(path, crate::new_request_id())
        } else {
            page.state.ask_info(crate::new_request_id())
        };
        self.root.update(cx, |root, _| root.send_to(&host, msg));
    }

    /// Takes what the root view kept for the browser: each answer it still
    /// waits for, and its host's restarts.
    pub(super) fn take_browse_answers(&mut self, cx: &mut Context<Self>) {
        let events = self.root.update(cx, |root, _| root.phone_browse.drain());
        for event in events {
            match event {
                Event::Reset(host) => self.browser_reset(&host, cx),
                Event::Answer { host, id, answer } => self.browser_answer(&host, &id, answer, cx),
            }
        }
    }

    /// `host` started over: the browser on it asks again.
    fn browser_reset(&mut self, host: &HostKey, cx: &mut Context<Self>) {
        let Some(page) = self.browser.as_mut() else {
            return;
        };
        if page.state.host() != host {
            return;
        }
        tracing::debug!(?host, "phone: the browsed host started over; asking again");
        page.state.reset();
        self.browser_start(cx);
    }

    /// `host` answered the browser's request `id`: its info lists the
    /// folder, a page shows; an answer no longer waited for is dropped.
    fn browser_answer(&mut self, host: &HostKey, id: &str, answer: Answer, cx: &mut Context<Self>) {
        let Some(page) = self.browser.as_mut() else {
            return;
        };
        if page.state.host() != host {
            return;
        }
        match page.state.take(id, answer) {
            Taken::Dropped => {
                tracing::debug!(%id, "phone: dropping a browse answer no longer waited for");
            }
            Taken::Info(facts) => {
                let path = page.state.path().to_owned();
                let msg = page.state.list(path, crate::new_request_id());
                let host = host.clone();
                self.root.update(cx, |root, _| {
                    root.phone_browse.remember(&host, facts);
                    root.send_to(&host, msg);
                });
            }
            Taken::Listing => {}
        }
        cx.notify();
    }

    /// Lists `path` in place of the folder shown, from its top.
    fn browser_enter(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(page) = self.browser.as_mut() else {
            return;
        };
        tracing::debug!(%path, "phone: browsing a host folder");
        let host = page.state.host().clone();
        let msg = page.state.list(path, crate::new_request_id());
        page.scroll = ScrollHandle::new();
        page.sheet = None;
        // A host that cannot be asked now is asked once its Welcome resets
        // the browser.
        if self.root.read(cx).host_serves_browse(&host) {
            self.root.update(cx, |root, _| root.send_to(&host, msg));
        }
        cx.notify();
    }

    /// The parent row: lists the folder holding the one shown.
    fn browser_up(&mut self, cx: &mut Context<Self>) {
        let Some(parent) = self.browser.as_ref().and_then(|page| page.state.parent()) else {
            return;
        };
        self.browser_enter(parent, cx);
    }

    /// A row's tap: a folder is entered, a file opens as a kept copy does,
    /// fetched first when the phone keeps none.
    fn browser_open_row(&mut self, name: &str, folder: bool, cx: &mut Context<Self>) {
        let Some(page) = self.browser.as_ref() else {
            return;
        };
        let path = page.state.child(name);
        if folder {
            self.browser_enter(path, cx);
        } else {
            let host = page.state.host().clone();
            self.open_kept(host, path, cx);
        }
    }

    /// A long press on the folder row `name`: its sheet opens.
    fn browser_open_sheet(&mut self, name: String, cx: &mut Context<Self>) {
        if let Some(page) = self.browser.as_mut() {
            tracing::debug!(%name, "phone: opening a browsed folder's sheet");
            page.sheet = Some(name);
            cx.notify();
        }
    }

    /// The folder sheet's Cancel or scrim: the sheet closes, asking nothing.
    fn browser_close_sheet(&mut self, cx: &mut Context<Self>) {
        if let Some(page) = self.browser.as_mut() {
            page.sheet = None;
            cx.notify();
        }
    }

    /// Fetch folder: the sheet closes and its folder is fetched whole; the
    /// result line says it is fetching, or why the fetch was refused.
    fn browser_fetch_sheet_folder(&mut self, cx: &mut Context<Self>) {
        let Some(page) = self.browser.as_mut() else {
            return;
        };
        let Some(name) = page.sheet.take() else {
            return;
        };
        let host = page.state.host().clone();
        let path = page.state.child(&name);
        page.state.fetch_started(path.clone(), name);
        if let Err(reason) = self.fetch_host_folder(host, path.clone(), cx) {
            tracing::info!(%path, %reason, "phone: a folder fetch was refused");
            if let Some(page) = self.browser.as_mut() {
                page.state.fetch_failed(&path, reason);
            }
        }
        cx.notify();
    }

    /// Show hidden: shows or hides the hidden entries.
    fn browser_toggle_hidden(&mut self, cx: &mut Context<Self>) {
        if let Some(page) = self.browser.as_mut() {
            page.state.toggle_hidden();
            cx.notify();
        }
    }

    /// Asks for the next page once the list is drawn near its last entry.
    pub(super) fn browser_next_page(&mut self, cx: &mut Context<Self>) {
        let Some(page) = self.browser.as_mut() else {
            return;
        };
        if !self.root.read(cx).host_serves_browse(page.state.host()) {
            return;
        }
        let first_entry = usize::from(page.state.parent().is_some());
        let drawn = page.scroll.bottom_item().saturating_sub(first_entry);
        let Some(msg) = page.state.next_page(drawn, crate::new_request_id()) else {
            return;
        };
        tracing::debug!(path = page.state.path(), "phone: asking for the next page");
        let host = page.state.host().clone();
        self.root.update(cx, |root, _| root.send_to(&host, msg));
    }

    /// Back on the browser: closes it, showing the page under it.
    fn browser_back(&mut self, cx: &mut Context<Self>) {
        self.close_browser(cx);
        cx.notify();
    }

    /// Closes the browser, if open; the root view stops keeping its host's
    /// answers.
    pub(super) fn close_browser(&mut self, cx: &mut Context<Self>) {
        if self.browser.take().is_some() {
            self.root
                .update(cx, |root, _| root.phone_browse.watch(None));
        }
    }

    /// A back gesture over the window: the browser's Back while it is the
    /// page on top, else the screen's own step back.
    pub(super) fn browser_back_gesture(
        &mut self,
        event: &BackGestureEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings.is_none() && self.viewer.is_none() && self.browser.is_some() {
            if super::session::back_on(event.phase) {
                self.browser_back(cx);
            }
            return;
        }
        self.screen_back_gesture(event, window, cx);
    }

    /// The folder the browser shows, while it is open.
    #[must_use]
    pub fn browser_path(&self) -> Option<&str> {
        self.browser.as_ref().map(|page| page.state.path())
    }

    /// The names of the rows the browser lists, in order, while it is open.
    #[must_use]
    pub fn browser_rows(&self) -> Vec<String> {
        self.browser.as_ref().map_or_else(Vec::new, |page| {
            page.state
                .rows()
                .iter()
                .map(|entry| entry.name.clone())
                .collect()
        })
    }

    /// The line drawn in the rows' place, while the folder could not be
    /// listed.
    #[must_use]
    pub fn browser_error(&self) -> Option<String> {
        self.browser.as_ref()?.state.error_line()
    }

    /// The result line under the browser's header, while it shows one.
    #[must_use]
    pub fn browser_result(&self) -> Option<String> {
        self.browser.as_ref()?.state.fetch_line()
    }

    /// The folder sheet over the browser, while a long press opened one:
    /// the folder's name, Fetch folder and Cancel over a scrim, its bottom
    /// clear of `bottom`'s home indicator.
    pub(super) fn browser_sheet_layer(
        &self,
        bottom: Pixels,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let name = self.browser.as_ref()?.sheet.clone()?;
        let t = theme(cx);
        let scrim = div()
            .id("phone-browser-sheet-scrim")
            .driver_id(|| "phone-browser-sheet-scrim".to_owned())
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(gpui::rgba(t.scrim))
            .occlude()
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.browser_close_sheet(cx)));
        let fetch = Self::sheet_button("phone-browser-sheet-fetch", ButtonKind::Outlined, cx)
            .child(FETCH_FOLDER_LABEL)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                this.browser_fetch_sheet_folder(cx);
            }));
        let cancel = Self::sheet_button("phone-browser-sheet-cancel", ButtonKind::Outlined, cx)
            .child(CANCEL_LABEL)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                this.browser_close_sheet(cx);
            }));
        let title = div()
            .driver_id(|| "phone-browser-sheet-title".to_owned())
            .flex()
            .items_center()
            .min_h(px(TOUCH_TARGET))
            .px(px(4.0))
            .truncate()
            .font_weight(ui_weight(cx, FontWeight::SEMIBOLD))
            .child(name);
        let panel = div()
            .id("phone-browser-sheet")
            .driver_id(|| "phone-browser-sheet".to_owned())
            .occlude()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .px(px(16.0))
            .pt(px(16.0))
            .pb(bottom + px(16.0))
            .rounded_t(px(14.0))
            .border_t_1()
            .border_color(gpui::rgb(t.line))
            .bg(gpui::rgb(t.raised))
            .child(title)
            .child(fetch)
            .child(cancel);
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .flex_col()
                .justify_end()
                .child(scrim)
                .child(panel),
        )
    }

    /// While the browser is open: its header over its list.
    pub(super) fn browser_page(&self, cx: &mut Context<Self>) -> Option<Div> {
        let page = self.browser.as_ref()?;
        let t = theme(cx);
        let (host_label, open) = {
            let root = self.root.read(cx);
            (
                root.host_label_of(page.state.host()),
                host_open(root, page.state.host()),
            )
        };
        let waiting = (!open).then(|| format!("Waiting for {host_label}…"));
        Some(
            div()
                .driver_id(|| "phone-browser".to_owned())
                .size_full()
                .flex()
                .flex_col()
                .child(Self::browser_header_row(page, &host_label, &t, cx))
                .children(result_line(&page.state, &t))
                .child(Self::browser_list(page, waiting, &t, cx)),
        )
    }

    /// The browser's header: Back, the host over the folder's path, and
    /// Show hidden, filled while it is on.
    fn browser_header_row(
        page: &BrowserPage,
        host_label: &str,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Div {
        let back = phone_button("phone-browser-back", ButtonKind::Outlined, cx)
            .min_h(px(TOUCH_TARGET))
            .flex_none()
            .child(BACK_LABEL)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.browser_back(cx)));
        let names = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .driver_id(|| "phone-browser-host".to_owned())
                    .truncate()
                    .font_weight(ui_weight(cx, FontWeight::SEMIBOLD))
                    .child(host_label.to_owned()),
            )
            .child(
                div()
                    .driver_id(|| "phone-browser-path".to_owned())
                    .truncate()
                    .text_size(ui_px(cx, STATUS_TEXT_SIZE))
                    .text_color(gpui::rgb(t.muted))
                    .child(page.state.path().to_owned()),
            );
        let kind = if page.state.show_hidden() {
            ButtonKind::Primary
        } else {
            ButtonKind::Outlined
        };
        let hidden = phone_button("phone-browser-hidden", kind, cx)
            .min_h(px(TOUCH_TARGET))
            .flex_none()
            .child(SHOW_HIDDEN_LABEL)
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.browser_toggle_hidden(cx)));
        header_frame(t).child(back).child(names).child(hidden)
    }

    /// The browser's list: `waiting`, the line drawn alone while the host is
    /// not connected; else the parent row, then a row per entry or the line
    /// saying why the folder could not be listed.
    fn browser_list(
        page: &BrowserPage,
        waiting: Option<String>,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let parent = page
            .state
            .parent()
            .filter(|_| waiting.is_none())
            .map(|_| Self::parent_row(t, cx).into_any_element());
        let rows: Vec<gpui::AnyElement> = if let Some(line) = waiting {
            vec![note_line("phone-browser-waiting", line, t)]
        } else if let Some(line) = page.state.error_line() {
            vec![note_line("phone-browser-error", line, t)]
        } else {
            let now = Local::now();
            page.state
                .rows()
                .into_iter()
                .map(|entry| Self::entry_row(entry, &now, t, cx).into_any_element())
                .collect()
        };
        div()
            .id("phone-browser-list")
            .driver_id(|| "phone-browser-list".to_owned())
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .track_scroll(&page.scroll)
            .on_scroll(cx.listener(|this, _: &ScrollEvent, _, cx| this.browser_next_page(cx)))
            .children(parent)
            .children(rows)
    }

    /// The parent row, which lists the folder holding the one shown.
    fn parent_row(t: &UiTheme, cx: &mut Context<Self>) -> Stateful<Div> {
        row_frame("phone-browser-parent", t)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
                    .child(PARENT_LABEL),
            )
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.browser_up(cx)))
    }

    /// `entry`'s row: its name, then for a file its size, then when it last
    /// changed, and a folder's chevron.
    fn entry_row(
        entry: &DirEntry,
        now: &DateTime<Local>,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let name = entry.name.clone();
        let folder = browser::is_folder(entry);
        let size_id = format!("phone-browser-size-{name}");
        let size = (!folder).then(|| {
            div()
                .driver_id(move || size_id)
                .child(human_size(entry.size))
        });
        let modified = entry.modified.and_then(|ms| browser::short_date(ms, now));
        let meta = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_size(ui_px(cx, STATUS_TEXT_SIZE))
            .text_color(gpui::rgb(t.muted))
            .children(size)
            .children(modified)
            .children(folder.then_some(CHEVRON));
        let tapped = name.clone();
        // A long press on a folder offers its sheet; a file's offers nothing.
        let long_press = folder.then(|| {
            let phone = cx.weak_entity();
            let pressed = name.clone();
            long_press_area(move |_, _, cx| {
                // Fails only when the phone's root is gone.
                phone
                    .update(cx, |this, cx| this.browser_open_sheet(pressed.clone(), cx))
                    .ok();
            })
        });
        row_frame(&format!("phone-browser-row-{name}"), t)
            .relative()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(ui_px(cx, crate::UI_TEXT_SIZE))
                    .child(name),
            )
            .child(meta)
            .children(long_press)
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.browser_open_row(&tapped, folder, cx);
            }))
    }
}

/// The result line under the header, while `state` shows one: muted, or in
/// the danger colour when it says why a fetch failed.
fn result_line(state: &Browser, t: &UiTheme) -> Option<Div> {
    let line = state.fetch_line()?;
    let color = if state.fetch_failed_line() {
        t.danger_text
    } else {
        t.muted
    };
    Some(
        div()
            .driver_id(|| "phone-browser-result".to_owned())
            .flex_none()
            .px(px(SIDE_PADDING))
            .py(px(8.0))
            .border_b_1()
            .border_color(gpui::rgb(t.line))
            .text_color(gpui::rgb(color))
            .child(line),
    )
}

/// A muted line tagged `selector` drawn in the rows' place.
fn note_line(selector: &'static str, line: String, t: &UiTheme) -> gpui::AnyElement {
    div()
        .driver_id(|| selector.to_owned())
        .flex_none()
        .px(px(SIDE_PADDING))
        .py(px(12.0))
        .text_color(gpui::rgb(t.muted))
        .child(line)
        .into_any_element()
}

/// A list row tagged `selector`: at least a touch target tall, the side
/// padding, a line under it.
fn row_frame(selector: &str, t: &UiTheme) -> Stateful<Div> {
    let tag = selector.to_owned();
    div()
        .id(SharedString::from(selector.to_owned()))
        .driver_id(move || tag)
        .flex_none()
        .flex()
        .items_center()
        .gap(px(10.0))
        .min_h(px(TOUCH_TARGET))
        .px(px(SIDE_PADDING))
        .border_b_1()
        .border_color(gpui::rgb(t.line))
}
