//! Renders the sidebar model beside the terminal: a header with the hide
//! button, container rows and session leaves, and the drag divider.

use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, MouseButton, SharedString, Stateful, Window,
    div, prelude::*, px,
};
use protocol::SessionStatus;

use crate::connection::DotKind;
use crate::sidebar::{Container, Leaf};
use crate::{
    BORDER, Drag, HOVER_BG, MUTED, PANEL_BG, RootView, TEXT, UI_TEXT_SIZE, dot_color, drag_handle,
    status_dot, tooltip,
};

const ROW_HEIGHT: f32 = 22.0;
const ROW_PADDING: f32 = 8.0;
const LEAF_INDENT: f32 = 22.0;
const SELECTED_BG: u32 = 0x0037_3a44;
const TAG_TEXT_SIZE: f32 = 10.0;
const COLLAPSED_STRIP_WIDTH: f32 = 16.0;

impl RootView {
    /// The sidebar, the divider and the terminal pane side by side; a hidden
    /// sidebar leaves only a slim strip to show it again.
    pub(crate) fn main_row(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let row = div().flex().flex_row().flex_1().min_h(px(0.0));
        let row = if self.sidebar.is_collapsed() {
            row.child(collapsed_strip(cx))
        } else {
            let width = self.sidebar.width(window.viewport_size().width / px(1.0));
            let active = matches!(self.drag, Some(Drag::Sidebar));
            row.child(self.sidebar_panel(width, cx))
                .child(divider(active, cx))
        };
        row.child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .h_full()
                .child(self.tab_bar(cx))
                .child(self.grid_area(cx)),
        )
    }

    fn sidebar_panel(&self, width: f32, cx: &mut Context<Self>) -> Div {
        let attached = self.focused_session();
        let containers = self.sidebar.containers();
        let body = div()
            .id("sidebar-body")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll();
        let body = if containers.is_empty() {
            body.child(
                div()
                    .px(px(ROW_PADDING))
                    .py(px(6.0))
                    .text_color(gpui::rgb(MUTED))
                    .child("No sessions"),
            )
        } else {
            let mut rows = Vec::new();
            for container in &containers {
                rows.extend(container_rows(container, attached.as_deref(), cx));
            }
            body.children(rows)
        };
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(width))
            .h_full()
            .debug_selector(|| "sidebar-panel".to_owned())
            .track_focus(&self.sidebar_focus)
            .bg(gpui::rgb(PANEL_BG))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(TEXT))
            .child(header(cx))
            .child(body)
    }
}

/// "Sessions" and the button that hides the sidebar.
fn header(cx: &mut Context<RootView>) -> Div {
    let hide = div()
        .id("sidebar-hide")
        .px(px(6.0))
        .rounded(px(4.0))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)).text_color(gpui::rgb(TEXT)))
        .child("«")
        .tooltip(tooltip("Hide sidebar (Ctrl+B outside the terminal)"))
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.toggle_sidebar(window, cx);
        }));
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_between()
        .h(px(ROW_HEIGHT + 4.0))
        .px(px(ROW_PADDING))
        .border_b_1()
        .border_color(gpui::rgb(BORDER))
        .text_color(gpui::rgb(MUTED))
        .child(div().font_weight(FontWeight::SEMIBOLD).child("Sessions"))
        .child(hide)
}

/// The slim strip a hidden sidebar leaves at the left edge, with the button
/// that shows it again.
fn collapsed_strip(cx: &mut Context<RootView>) -> Div {
    let show = div()
        .id("sidebar-show")
        .debug_selector(|| "sidebar-show".to_owned())
        .flex()
        .justify_center()
        .w_full()
        .py(px(4.0))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)).text_color(gpui::rgb(TEXT)))
        .child("»")
        .tooltip(tooltip("Show sidebar"))
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.toggle_sidebar(window, cx);
        }));
    div()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(COLLAPSED_STRIP_WIDTH))
        .h_full()
        .bg(gpui::rgb(PANEL_BG))
        .border_r_1()
        .border_color(gpui::rgb(BORDER))
        .text_size(px(UI_TEXT_SIZE))
        .text_color(gpui::rgb(MUTED))
        .child(show)
}

/// The drag handle; a press starts a resize the root follows until release.
fn divider(active: bool, cx: &mut Context<RootView>) -> Stateful<Div> {
    drag_handle("sidebar-divider", true, active)
        .debug_selector(|| "sidebar-divider".to_owned())
        .on_mouse_down(MouseButton::Left, cx.listener(RootView::start_drag))
}

/// A container row, then its leaves unless it is collapsed.
fn container_rows(
    container: &Container,
    attached: Option<&str>,
    cx: &mut Context<RootView>,
) -> Vec<AnyElement> {
    let mut rows = vec![container_row(container, cx).into_any_element()];
    if !container.collapsed {
        for leaf in &container.leaves {
            let selected = attached == Some(leaf.id.as_str());
            rows.push(leaf_row(leaf, selected, cx).into_any_element());
        }
    }
    rows
}

fn container_row(container: &Container, cx: &mut Context<RootView>) -> Stateful<Div> {
    let key = container.key.clone();
    let chip = if container.collapsed { "▸" } else { "▾" };
    let name = format!("container-{}", container.key);
    div()
        .id(SharedString::from(name.clone()))
        .debug_selector(|| name)
        .flex()
        .items_center()
        .gap(px(6.0))
        .h(px(ROW_HEIGHT))
        .px(px(ROW_PADDING))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .child(
            div()
                .flex_none()
                .w(px(10.0))
                .text_color(gpui::rgb(MUTED))
                .child(chip),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(TAG_TEXT_SIZE))
                .text_color(gpui::rgb(MUTED))
                .child(container.kind.tag()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .font_weight(FontWeight::SEMIBOLD)
                .child(container.name.clone()),
        )
        .when(container.attention, |row| row.child(attention_mark()))
        .child(
            div()
                .flex_none()
                .text_color(gpui::rgb(MUTED))
                .child(container.leaves.len().to_string()),
        )
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.toggle_container(&key);
            cx.notify();
        }))
}

fn leaf_row(leaf: &Leaf, selected: bool, cx: &mut Context<RootView>) -> Stateful<Div> {
    let id = leaf.id.clone();
    let name = format!("leaf-{}", leaf.id);
    div()
        .id(SharedString::from(name.clone()))
        .debug_selector(|| name)
        .flex()
        .items_center()
        .gap(px(6.0))
        .h(px(ROW_HEIGHT))
        .pl(px(LEAF_INDENT))
        .pr(px(ROW_PADDING))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .when(selected, |row| row.bg(gpui::rgb(SELECTED_BG)))
        .child(session_dot(leaf.status, &leaf.id))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .child(leaf.label.clone()),
        )
        .when_some(leaf.runtime.clone(), |row, runtime| {
            row.child(
                div()
                    .flex_none()
                    .text_size(px(TAG_TEXT_SIZE))
                    .text_color(gpui::rgb(MUTED))
                    .child(runtime),
            )
        })
        .when(leaf.attention, |row| row.child(attention_mark()))
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.select_session(&id, window, cx);
        }))
}

/// A session's status dot: pulsing while working, hollow while spawning,
/// amber while awaiting input.
fn session_dot(status: SessionStatus, session_id: &str) -> AnyElement {
    let id = SharedString::from(format!("leaf-dot-{session_id}"));
    match status {
        SessionStatus::Working => status_dot(DotKind::Pending, id),
        SessionStatus::Idle => status_dot(DotKind::Ok, id),
        SessionStatus::Stopped => status_dot(DotKind::Stopped, id),
        SessionStatus::Error => status_dot(DotKind::Err, id),
        SessionStatus::AwaitingInput => plain_dot()
            .bg(gpui::rgb(dot_color(DotKind::Pending)))
            .into_any_element(),
        SessionStatus::Spawning => plain_dot()
            .border_1()
            .border_color(gpui::rgb(dot_color(DotKind::Pending)))
            .into_any_element(),
    }
}

fn plain_dot() -> Div {
    div().flex_none().size(px(8.0)).rounded_full()
}

fn attention_mark() -> Div {
    div()
        .flex_none()
        .font_weight(FontWeight::BOLD)
        .text_color(gpui::rgb(dot_color(DotKind::Pending)))
        .child("!")
}
