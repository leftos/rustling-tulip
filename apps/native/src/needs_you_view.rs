//! The Needs You panel: a header counting the sessions waiting on the user,
//! then one two-line row each, in the order `needs_you::rows` lists them. A
//! click shows the session; a right-click opens its menu. While the panel
//! shows a wait, a timer repaints it so the waits keep counting, and a list
//! change re-arms that timer so the cycle follows the youngest wait.

use std::time::Duration;

use chrono::{DateTime, Utc};
use gpui::{
    ClickEvent, Context, Div, FontWeight, MouseButton, MouseDownEvent, SharedString, Stateful, div,
    prelude::*, px,
};
use protocol::{DaemonMessage, SessionStatus};

use crate::activity_bar::badge_text;
use crate::needs_you::{self, NeedsYouRow, Reason};
use crate::sidebar::Activity;
use crate::sidebar_view::{ROW_HEIGHT, ROW_PADDING, session_dot};
use crate::{BORDER, HOVER_BG, MUTED, PANEL_BG, RootView, TEXT, UI_TEXT_SIZE, tooltip};

const HEADER_HEIGHT: f32 = 26.0;
const TITLE: &str = "NEEDS YOU";
/// The repaint interval while some wait is under a minute, so its seconds
/// count up.
const FAST_REPAINT: Duration = Duration::from_secs(1);
/// The repaint interval once every wait reads in minutes or more.
const SLOW_REPAINT: Duration = Duration::from_secs(30);
/// A wait younger than this reads in seconds.
const SECONDS_SHOWN_BELOW: i64 = 60;

/// One listed session as the panel draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeedsYouEntry {
    pub session_id: String,
    /// The session ended rather than waiting to be answered.
    pub ended: bool,
    /// The sidebar container it sits in.
    pub container: String,
    pub label: String,
    /// Line two: what the session wants.
    pub detail: String,
    /// How long it has waited, or since it ended; empty when unknown.
    pub waited: String,
}

/// The time slot's text for `since`; empty when the daemon sent no stamp.
fn waited_text(now: DateTime<Utc>, since: Option<DateTime<Utc>>) -> String {
    since.map_or_else(String::new, |since| needs_you::waited(now, since))
}

/// The header's text for `count` listed sessions.
fn header_text(count: usize) -> String {
    format!("{TITLE} · {count}")
}

/// Whether `msg` can change which sessions the panel lists, their order or
/// their waits: the messages the sidebar model folds in.
pub(crate) fn moves_needs_you(msg: &DaemonMessage) -> bool {
    matches!(
        msg,
        DaemonMessage::Repos { .. }
            | DaemonMessage::Workspaces { .. }
            | DaemonMessage::ContainersReordered { .. }
            | DaemonMessage::SessionsReordered { .. }
            | DaemonMessage::Sessions { .. }
            | DaemonMessage::SessionUpdated { .. }
            | DaemonMessage::SessionRemoved { .. }
            | DaemonMessage::Attention { .. }
    )
}

impl RootView {
    /// The rows the panel draws, in list order.
    pub(crate) fn needs_you_list(&self) -> Vec<NeedsYouRow> {
        needs_you::rows(&self.sidebar)
    }

    /// How many sessions the list holds; the rail's badge count.
    pub(crate) fn needs_you_count(&self) -> usize {
        self.needs_you_list().len()
    }

    /// The listed sessions, in list order, as the panel draws them.
    #[must_use]
    pub fn needs_you_rows(&self) -> Vec<NeedsYouEntry> {
        let now = Utc::now();
        self.needs_you_list()
            .into_iter()
            .map(|row| NeedsYouEntry {
                waited: waited_text(now, row.since),
                ended: row.reason == Reason::Ended,
                session_id: row.session_id,
                container: row.container_name,
                label: row.label,
                detail: row.detail,
            })
            .collect()
    }

    /// The Needs You item's badge as drawn, when it shows.
    #[must_use]
    pub fn needs_you_badge(&self) -> Option<String> {
        badge_text(self.needs_you_count())
    }

    /// The panel header's text.
    #[must_use]
    pub fn needs_you_header(&self) -> String {
        header_text(self.needs_you_count())
    }

    /// How many times the panel's timer has repainted it, so the specs can
    /// watch the cycle.
    #[must_use]
    pub fn needs_you_repaints(&self) -> usize {
        self.needs_you_repaints
    }

    /// The panel, `width` wide.
    pub(crate) fn needs_you_view(&self, width: f32, cx: &mut Context<Self>) -> Div {
        let rows = self.needs_you_list();
        let now = Utc::now();
        let body = div()
            .id("needs-you-body")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll();
        let body = if rows.is_empty() {
            body.child(empty_state())
        } else {
            body.children(rows.iter().map(|row| {
                let status = self
                    .sidebar
                    .session(&row.session_id)
                    .map_or(SessionStatus::AwaitingInput, |s| s.status);
                needs_you_row(row, status, now, cx)
            }))
        };
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(width))
            .h_full()
            .debug_selector(|| "needs-you-panel".to_owned())
            .track_focus(&self.sidebar_focus)
            .bg(gpui::rgb(PANEL_BG))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(TEXT))
            .child(header(header_text(rows.len())))
            .child(body)
    }

    /// Whether the Needs You panel is the one showing.
    fn needs_you_shown(&self) -> bool {
        self.sidebar.activity() == Activity::NeedsYou && !self.sidebar.is_collapsed()
    }

    /// Starts the panel's repaint timer when the panel shows a wait, and
    /// drops it when the panel is hidden or empty: an empty panel has no wait
    /// to count, and the list change that adds a row re-arms it.
    pub(crate) fn arm_needs_you_repaint(&mut self, cx: &mut Context<Self>) {
        if !self.needs_you_shown() || self.needs_you_count() == 0 {
            self.needs_you_timer = None;
            return;
        }
        let delay = self.needs_you_repaint_delay();
        self.needs_you_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            // Fails only when the view is gone, and the panel with it.
            this.update(cx, Self::repaint_needs_you).ok();
        }));
    }

    /// The timer fired: repaint the waits and go again while the panel
    /// still shows.
    fn repaint_needs_you(&mut self, cx: &mut Context<Self>) {
        if !self.needs_you_shown() {
            self.needs_you_timer = None;
            return;
        }
        self.needs_you_repaints += 1;
        cx.notify();
        self.arm_needs_you_repaint(cx);
    }

    /// Every second while a wait still reads in seconds, else every 30.
    fn needs_you_repaint_delay(&self) -> Duration {
        let now = Utc::now();
        let fresh = self
            .needs_you_list()
            .iter()
            .filter_map(|row| row.since)
            .any(|since| (now - since).num_seconds() < SECONDS_SHOWN_BELOW);
        if fresh { FAST_REPAINT } else { SLOW_REPAINT }
    }
}

fn header(text: String) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .h(px(HEADER_HEIGHT))
        .px(px(ROW_PADDING))
        .border_b_1()
        .border_color(gpui::rgb(BORDER))
        .text_color(gpui::rgb(MUTED))
        .debug_selector(|| "needs-you-header".to_owned())
        .child(div().font_weight(FontWeight::SEMIBOLD).child(text))
}

fn empty_state() -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .px(px(ROW_PADDING))
        .py(px(6.0))
        .text_color(gpui::rgb(MUTED))
        .debug_selector(|| "needs-you-empty".to_owned())
        .child("Nothing needs you")
        .child(div().child("Sessions waiting on an answer, a permission or a look show here."))
}

/// A listed session: its dot, container, label and wait on line one, what
/// it wants on line two.
fn needs_you_row(
    row: &NeedsYouRow,
    status: SessionStatus,
    now: DateTime<Utc>,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let id = row.session_id.clone();
    let menu_id = row.session_id.clone();
    let name = format!("needs-you-row-{}", row.session_id);
    let waited = row.since.map(|since| needs_you::waited(now, since));
    let first = div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .h(px(ROW_HEIGHT))
        .child(session_dot(
            status,
            format!("needs-you-dot-{}", row.session_id),
        ))
        .child(
            div()
                .flex()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .gap(px(4.0))
                .child(
                    div()
                        .flex_none()
                        .text_color(gpui::rgb(row.accent))
                        .child(row.container_name.clone()),
                )
                .child(div().flex_none().text_color(gpui::rgb(MUTED)).child("·"))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .child(row.label.clone()),
                ),
        )
        .when_some(waited, |line, waited| {
            line.child(div().flex_none().text_color(gpui::rgb(MUTED)).child(waited))
        });
    let second = div()
        .min_w(px(0.0))
        .text_color(gpui::rgb(MUTED))
        .truncate()
        .child(row.detail.clone());
    div()
        .id(SharedString::from(name.clone()))
        .debug_selector(|| name)
        .flex()
        .flex_col()
        .w_full()
        .px(px(ROW_PADDING))
        .pb(px(4.0))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .tooltip(tooltip(row.detail.clone()))
        .child(first)
        .child(second)
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                this.open_session_menu(&menu_id, event.position, window, cx);
                cx.stop_propagation();
            }),
        )
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.select_session(&id, window, cx);
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_you_header_counts_rows() {
        assert_eq!(header_text(0), "NEEDS YOU · 0");
        assert_eq!(header_text(3), "NEEDS YOU · 3");
    }
}
