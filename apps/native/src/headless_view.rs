//! The body a pane showing a headless session draws where a terminal would
//! be: the session's stats over its recent-actions log, as the Tauri app's
//! `HeadlessView` does.

use gpui::{
    AnyElement, ClickEvent, Context, Div, ElementId, FocusHandle, MouseButton, SharedString,
    Stateful, div, prelude::*, px,
};
use protocol::SessionSnapshot;

use crate::fonts::DEFAULT_FAMILY;
use crate::headless::{self, EMPTY_LOG};
use crate::tabs;
use crate::{BAR_BG, BORDER, HOVER_BG, MUTED, RootView, TEXT};

/// The stats' label and value sizes, and the log's text size.
const STAT_LABEL_SIZE: f32 = 10.0;
const STAT_VALUE_SIZE: f32 = 13.0;
const LOG_TEXT_SIZE: f32 = 12.0;
/// The gap between the stats, and the bar's padding.
const STAT_GAP: f32 = 16.0;
/// The log rows' number column and the gap between a number and its text.
const LOG_NUMBER_WIDTH: f32 = 40.0;
const LOG_ROW_GAP: f32 = 8.0;
/// The stat selectors, in draw order, matching `HeadlessBody::stats`.
const STAT_KEYS: [&str; 4] = ["status", "in", "out", "cost"];

/// What a pane showing a headless session draws, as the body renders it and
/// the specs read it back.
pub struct HeadlessBody {
    pub pane_id: String,
    /// The four stats in draw order, each (label, value).
    pub stats: [(&'static str, String); 4],
    /// The show-all button's label, while the cap hides earlier entries.
    pub show_all: Option<String>,
    /// The log's rows in draw order, each (its number in the full list, its
    /// text).
    pub rows: Vec<(usize, String)>,
    /// The muted note the log shows instead of rows, when it has none.
    pub empty_note: Option<&'static str>,
}

impl HeadlessBody {
    fn of(pane_id: &str, session: &SessionSnapshot, show_all: bool) -> Self {
        let (hidden, shown) = headless::visible(&session.recent_actions, show_all);
        Self {
            pane_id: pane_id.to_owned(),
            stats: [
                ("STATUS", headless::status_label(session.status).to_owned()),
                (
                    "IN TOKENS",
                    headless::thousands(session.metrics.input_tokens),
                ),
                (
                    "OUT TOKENS",
                    headless::thousands(session.metrics.output_tokens),
                ),
                ("COST", headless::cost(session.metrics.cost_usd)),
            ],
            show_all: (hidden > 0)
                .then(|| headless::show_all_label(session.recent_actions.len(), hidden)),
            rows: shown
                .iter()
                .enumerate()
                .map(|(i, text)| (hidden + i + 1, text.clone()))
                .collect(),
            empty_note: session.recent_actions.is_empty().then_some(EMPTY_LOG),
        }
    }
}

impl RootView {
    /// Whether `pane_id`'s log shows every action rather than the tail.
    pub(crate) fn headless_expanded(&self, pane_id: &str) -> bool {
        self.headless_show_all.contains(pane_id)
    }

    /// What every pane showing a headless session draws, in tab order.
    #[must_use]
    pub fn headless_bodies(&self) -> Vec<HeadlessBody> {
        let mut bodies = Vec::new();
        for tab in self.tabs.tabs() {
            let Some(grid) = tab.grid() else {
                continue;
            };
            for pane in tabs::collect_panes(grid) {
                let Some(session) = pane.session.and_then(|id| self.sidebar.session(id)) else {
                    continue;
                };
                if !headless::is_headless(session) {
                    continue;
                }
                bodies.push(HeadlessBody::of(
                    pane.id,
                    session,
                    self.headless_expanded(pane.id),
                ));
            }
        }
        bodies
    }

    /// The body of a pane showing a headless session: the stats bar over the
    /// session's log, and a click anywhere in it focuses the pane through
    /// the pane's own focus handle, as an empty pane does, so the app's
    /// shortcuts stay live.
    pub(crate) fn headless_body(
        &self,
        pane_id: &str,
        session: &SessionSnapshot,
        focus: Option<FocusHandle>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = HeadlessBody::of(pane_id, session, self.headless_expanded(pane_id));
        let name = format!("headless-{pane_id}");
        let body = div()
            .debug_selector(move || name.clone())
            .flex()
            .flex_col()
            .flex_1()
            .size_full()
            .min_h(px(0.0))
            .child(self.headless_stats(&body))
            .child(self.headless_log(pane_id, &body, cx));
        match focus {
            Some(handle) => body
                .track_focus(&handle)
                .on_mouse_down(MouseButton::Left, move |_, window, _| {
                    handle.focus(window);
                })
                .into_any_element(),
            None => body.into_any_element(),
        }
    }

    /// The stats bar: each stat a small muted label over its monospace
    /// value.
    fn headless_stats(&self, body: &HeadlessBody) -> Div {
        let family = self.mono_family();
        let pane_id = body.pane_id.clone();
        div()
            .flex_none()
            .flex()
            .gap(px(STAT_GAP))
            .px(px(14.0))
            .py(px(8.0))
            .border_b_1()
            .border_color(gpui::rgb(BORDER))
            .bg(gpui::rgb(BAR_BG))
            .children(STAT_KEYS.into_iter().zip(body.stats.iter()).map(
                move |(key, (label, value))| {
                    let selector = format!("headless-stat-{key}-{pane_id}");
                    let (family, value) = (family.clone(), value.clone());
                    div()
                        .debug_selector(move || selector.clone())
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .text_size(px(STAT_LABEL_SIZE))
                                .text_color(gpui::rgb(MUTED))
                                .child(*label),
                        )
                        .child(
                            div()
                                .font_family(family)
                                .text_size(px(STAT_VALUE_SIZE))
                                .text_color(gpui::rgb(TEXT))
                                .child(value),
                        )
                },
            ))
    }

    /// The log: the show-all button while the cap hides earlier entries,
    /// then one numbered row per drawn action; the muted note when the
    /// session has reported none.
    fn headless_log(
        &self,
        pane_id: &str,
        body: &HeadlessBody,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = format!("headless-log-{pane_id}");
        let mut log = div()
            .id(ElementId::Name(SharedString::from(name.clone())))
            .debug_selector(move || name.clone())
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .px(px(14.0))
            .py(px(12.0))
            .font_family(self.mono_family())
            .text_size(px(LOG_TEXT_SIZE))
            .text_color(gpui::rgb(TEXT));
        if let Some(note) = body.empty_note {
            let name = format!("headless-empty-{pane_id}");
            return log
                .child(
                    div()
                        .debug_selector(move || name)
                        .text_color(gpui::rgb(MUTED))
                        .child(note),
                )
                .into_any_element();
        }
        if let Some(label) = &body.show_all {
            log = log.child(show_all_button(pane_id, label, cx));
        }
        log.children(
            body.rows.iter().map(|(number, text)| {
                row(*number, format!("headless-row-{number}-{pane_id}"), text)
            }),
        )
        .into_any_element()
    }

    /// The monospace family the stats' values and the log draw in: the
    /// app's terminal font, or the bundled default.
    fn mono_family(&self) -> SharedString {
        SharedString::from(
            self.sidebar
                .ui_state()
                .terminal_font
                .family
                .clone()
                .unwrap_or_else(|| DEFAULT_FAMILY.to_owned()),
        )
    }
}

/// The button that reveals the entries the cap hid; there is no way back,
/// as in the Tauri app.
fn show_all_button(pane_id: &str, label: &str, cx: &mut Context<RootView>) -> Stateful<Div> {
    let selector = format!("headless-show-all-{pane_id}");
    let pane = pane_id.to_owned();
    div()
        .id(ElementId::Name(SharedString::from(selector.clone())))
        .debug_selector(move || selector.clone())
        .w_full()
        .px(px(8.0))
        .py(px(6.0))
        .mb(px(6.0))
        .rounded(px(4.0))
        .border_1()
        .border_dashed()
        .border_color(gpui::rgb(BORDER))
        .text_center()
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .child(label.to_owned())
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.headless_show_all.insert(pane.clone());
            cx.notify();
        }))
}

/// One log row, tagged `name`: its number in the full list, then its text.
fn row(number: usize, name: String, text: &str) -> Div {
    div()
        .debug_selector(move || name)
        .flex()
        .gap(px(LOG_ROW_GAP))
        .child(
            div()
                .flex_none()
                .flex()
                .justify_end()
                .w(px(LOG_NUMBER_WIDTH))
                .text_color(gpui::rgb(MUTED))
                .child(number.to_string()),
        )
        .child(div().flex_1().min_w(px(0.0)).child(text.to_owned()))
}
