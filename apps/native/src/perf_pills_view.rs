//! The status bar's CPU and RAM pills, one per tracked machine, or a
//! placeholder while none is tracked, and the right-click menu that picks
//! the tracked machines. Each pill's text, level and tip come from
//! [`crate::perf_pills`]; this module only draws them.

use gpui::{
    AnyElement, Context, ElementId, FontWeight, MouseButton, MouseDownEvent, Pixels, Point,
    SharedString, anchored, deferred, div, prelude::*, px,
};

use crate::driver_id::DriverId as _;
use crate::fonts::{self, ui_px};
use crate::perf_pills::{Pill, PillLevel};
use crate::session_menu::{disabled_row, menu_item, muted_row, popover_frame};
use crate::sidebar::{LOCAL_HOST_KEY, session_view_key};
use crate::target::HostKey;
use crate::ui_theme::theme;
use crate::{FOOTER_PILL_HEIGHT, FOOTER_PILL_TEXT_SIZE, RootView, tagged_text, tooltip};

/// The placeholder pill's key, in its selector `footer-perf-none`.
const PLACEHOLDER_KEY: &str = "none";
/// The placeholder pill's text and tip, while no machine is tracked.
const PLACEHOLDER_TEXT: &str = "CPU · RAM";
const PLACEHOLDER_TIP: &str = "Right-click to choose machines to track";
/// The menu's first line, above its machine rows.
const MENU_HEADER: &str = "Track CPU and RAM on";
/// The tip of a connected host's row whose daemon reports no CPU and RAM.
const UNSUPPORTED_TIP: &str = "This host's daemon does not report CPU and RAM";
/// A row's mark before its machine's name: tracked, or not.
const TRACKED_MARK: &str = "✓ ";
const UNTRACKED_MARK: &str = "   ";
/// The menu's width in logical pixels.
const MENU_WIDTH: f32 = 240.0;

impl RootView {
    /// The footer's CPU and RAM pills, one per tracked machine that has
    /// one, else the placeholder that keeps the menu a right-click away.
    pub(crate) fn perf_pills_view(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut pills = self.perf_pills();
        if pills.is_empty() {
            pills.push((
                PLACEHOLDER_KEY.to_owned(),
                Pill {
                    text: PLACEHOLDER_TEXT.to_owned(),
                    level: PillLevel::Muted,
                    tip: PLACEHOLDER_TIP.to_owned(),
                },
            ));
        }
        pills
            .into_iter()
            .map(|(key, pill)| perf_pill(&key, pill, cx))
            .collect()
    }

    /// How host `key`'s pill draws; `None` while it has no pill.
    #[must_use]
    pub fn perf_pill_level(&self, key: &str) -> Option<PillLevel> {
        self.perf_pills()
            .into_iter()
            .find(|(kept, _)| kept == key)
            .map(|(_, pill)| pill.level)
    }

    /// Whether the CPU and RAM pills' menu shows.
    #[must_use]
    pub fn perf_menu_open(&self) -> bool {
        self.perf_menu.is_some()
    }

    /// Opens the CPU and RAM pills' menu at `at`.
    pub fn open_perf_menu(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        self.perf_menu = Some(at);
        cx.notify();
    }

    pub(crate) fn close_perf_menu(&mut self, cx: &mut Context<Self>) {
        if self.perf_menu.take().is_some() {
            cx.notify();
        }
    }

    /// The menu's machine rows, in order: each one's host key, its label
    /// (the machine's name after its tracked mark) and whether it can be
    /// pressed, which an untracked connected host whose daemon reports no
    /// CPU and RAM cannot; a tracked one can, so it can be untracked.
    #[must_use]
    pub fn perf_menu_rows(&self) -> Vec<(String, String, bool)> {
        self.perf_menu_entries()
            .into_iter()
            .map(|row| {
                let enabled = row.enabled();
                (row.key, row.label, enabled)
            })
            .collect()
    }

    /// The menu's machine rows, in order.
    fn perf_menu_entries(&self) -> Vec<PerfMenuRow> {
        let tracked =
            crate::perf_pills::tracked_keys(self.sidebar.ui_state().perf_tracked.as_deref());
        self.perf_menu_hosts(&tracked)
            .into_iter()
            .map(|(host, known)| {
                let key = session_view_key(&host);
                let name = match host {
                    HostKey::Local => "This PC".to_owned(),
                    HostKey::Remote(_) if known => self.host_name(&host),
                    HostKey::Remote(_) => key.clone(),
                };
                let is_tracked = tracked.contains(&key);
                let mark = if is_tracked {
                    TRACKED_MARK
                } else {
                    UNTRACKED_MARK
                };
                let unsupported = self.conn_of(&host).is_some_and(|conn| {
                    conn.is_open() && !conn.supports(protocol::FEATURE_PERF_MONITOR)
                });
                PerfMenuRow {
                    key,
                    label: format!("{mark}{name}"),
                    tracked: is_tracked,
                    unsupported,
                }
            })
            .collect()
    }

    /// The machines the menu lists, each once: This PC, the saved hosts in
    /// saved order, any other connected host, then any `tracked` key none of
    /// these is. Each comes with whether the window knows the host, so an
    /// unknown one is named by its key.
    fn perf_menu_hosts(&self, tracked: &[String]) -> Vec<(HostKey, bool)> {
        let saved = self
            .sidebar
            .ui_state()
            .saved_hosts
            .iter()
            .filter_map(|saved| saved.target().ok())
            .map(|target| HostKey::of(&target));
        let known = saved.chain(self.connected_hosts()).map(|host| (host, true));
        let unknown = tracked.iter().map(|key| (host_of_key(key), false));
        let mut hosts = vec![(HostKey::Local, true)];
        for (host, known) in known.chain(unknown) {
            if hosts.iter().all(|(kept, _)| *kept != host) {
                hosts.push((host, known));
            }
        }
        hosts
    }

    /// The open CPU and RAM menu over a layer that closes it on a press
    /// outside.
    pub(crate) fn perf_menu_layer(&self, cx: &mut Context<Self>) -> Option<[AnyElement; 2]> {
        let at = self.perf_menu?;
        let t = theme(cx);
        let backdrop = div()
            .id("perf-menu-backdrop")
            .driver_id(|| "perf-menu-backdrop".to_owned())
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.close_perf_menu(cx);
                    cx.stop_propagation();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.close_perf_menu(cx);
                    cx.stop_propagation();
                }),
            );
        let rows = self
            .perf_menu_entries()
            .into_iter()
            .map(|row| perf_menu_row(row, cx))
            .collect::<Vec<_>>();
        let panel = popover_frame(fonts::ui_text_size(cx), "perf-menu", &t)
            .flex()
            .flex_col()
            .w(px(MENU_WIDTH))
            .p(px(4.0))
            .child(muted_row(MENU_HEADER, &t))
            .children(rows);
        let anchored = anchored().position(at).snap_to_window().child(panel);
        Some([
            backdrop.into_any_element(),
            deferred(anchored).with_priority(1).into_any_element(),
        ])
    }
}

/// The host a tracked key names.
fn host_of_key(key: &str) -> HostKey {
    if key == LOCAL_HOST_KEY {
        HostKey::Local
    } else {
        HostKey::Remote(key.to_owned())
    }
}

/// Host `key`'s footer pill, tagged `footer-perf-{key}`; a right-click
/// opens the menu.
fn perf_pill(key: &str, pill: Pill, cx: &mut Context<RootView>) -> AnyElement {
    let t = theme(cx);
    let selector = format!("footer-perf-{key}");
    let probe = selector.clone();
    let color = match pill.level {
        PillLevel::Normal => t.text_2,
        PillLevel::Warn => t.asking,
        PillLevel::Muted => t.faint,
    };
    let hover = t.hover_bg;
    div()
        .id(ElementId::Name(SharedString::from(selector.clone())))
        .driver_id(move || probe)
        .flex()
        .flex_none()
        .items_center()
        .h(ui_px(cx, FOOTER_PILL_HEIGHT))
        .px(px(9.0))
        .rounded(ui_px(cx, FOOTER_PILL_HEIGHT) / 2.0)
        .border_1()
        .border_color(gpui::rgb(t.line_strong))
        .text_size(ui_px(cx, FOOTER_PILL_TEXT_SIZE))
        .text_color(gpui::rgb(color))
        .when(pill.level == PillLevel::Warn, |pill| {
            pill.font_weight(FontWeight::BOLD)
        })
        .whitespace_nowrap()
        .hover(move |style| style.bg(gpui::rgb(hover)))
        .child(tagged_text(selector, pill.text))
        .tooltip(tooltip(pill.tip))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|this, event: &MouseDownEvent, _, cx| {
                this.open_perf_menu(event.position, cx);
                cx.stop_propagation();
            }),
        )
        .into_any_element()
}

/// One machine's row of the CPU and RAM menu.
struct PerfMenuRow {
    /// The host's key.
    key: String,
    /// The machine's name after its tracked mark.
    label: String,
    tracked: bool,
    /// Connected to a daemon that reports no CPU and RAM.
    unsupported: bool,
}

impl PerfMenuRow {
    /// Whether a press acts: always but on an untracked host whose daemon
    /// reports no CPU and RAM, since tracking it would show nothing.
    fn enabled(&self) -> bool {
        self.tracked || !self.unsupported
    }
}

/// The menu row `row`, tagged `perf-menu-{key}`: a press tracks or
/// untracks its host and leaves the menu open; a disabled row says why,
/// and so does a tracked host's whose daemon reports no CPU and RAM.
fn perf_menu_row(row: PerfMenuRow, cx: &mut Context<RootView>) -> AnyElement {
    let t = theme(cx);
    let selector = format!("perf-menu-{}", row.key);
    if !row.enabled() {
        return disabled_row(&selector, row.label, UNSUPPORTED_TIP, &t).into_any_element();
    }
    let host = host_of_key(&row.key);
    menu_item(&selector, row.label, false, &t)
        .when(row.unsupported, |item| {
            item.tooltip(tooltip(UNSUPPORTED_TIP))
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                this.toggle_perf_tracked(&host, cx);
            }),
        )
        .into_any_element()
}
