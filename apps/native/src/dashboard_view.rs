//! The Dashboard tab's By status view: the header strip with its count pills
//! and day figures, the queue strip while a session waits, the column
//! header, then each group's heading and its two-line rows, a Working row's
//! agent tree nested below it. Every text and number comes from
//! `dashboard_rows`; this file only lays them out. A row click shows the
//! session's pane, a queue chip click scrolls its row into view, and while
//! the tab is active a timer repaints the ages.

use std::collections::HashMap;
use std::time::Duration;

use chrono::Utc;
use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, SharedString, StyledText, canvas, div,
    prelude::*, px,
};
use protocol::{SessionMode, SessionStatus};

use crate::dashboard::{self, DashboardRow, GroupKind, HeaderCounts};
use crate::dashboard_rows::{
    Columns, HeaderFigures, NestedRow, QUEUE_HINT, QueueChip, RowCells, RowInput, agent_glyph,
    header_figures, header_pills, queue_chips, row_cells, visible_columns,
};
use crate::ellipsis::truncating;
use crate::fonts::DEFAULT_FAMILY;
use crate::palette::{
    ASK_BORDER, ASK_WASH, ASKING, CHIP, CONTEXT_HOT, DANGER, DANGER_WASH, FAINT, GROUND, LILAC,
    LINE, LINE_STRONG, RAISED, SUBTLE, SURFACE, TEXT_2, WAITING, WORKING,
};
use crate::status_glyph::{GlyphSize, Shape, glyph, glyph_view};
use crate::ui_theme::{UiTheme, theme};
use crate::{RootView, TEXT, UI_TEXT_SIZE};

/// The grid's columns, as the mock sizes them; Now and Summary share what is
/// left.
const GLYPH_W: f32 = 30.0;
const SESSION_W: f32 = 232.0;
const LINEAR_W: f32 = 248.0;
const AGENTS_W: f32 = 104.0;
const CONTEXT_W: f32 = 74.0;
const ACTIVITY_W: f32 = 70.0;
const RUN_W: f32 = 72.0;
const COST_W: f32 = 66.0;
const GAP: f32 = 12.0;
const PAD_LEFT: f32 = 14.0;
const PAD_RIGHT: f32 = 18.0;
const ROW_MIN_H: f32 = 46.0;
const SUBROW_H: f32 = 23.0;
const COLUMNS_H: f32 = 26.0;
/// A row's second line and the smaller figures.
const SMALL_TEXT: f32 = 11.5;
/// The column header's and the group headings' capitals.
const CAPS_TEXT: f32 = 10.5;
/// The context bar's size.
const BAR_W: f32 = 38.0;
const BAR_H: f32 = 5.0;
/// A finished nested node draws this opaque.
const DIMMED: f32 = 0.55;
/// The repaint interval while some age is under a minute.
const FAST_REPAINT: Duration = Duration::from_secs(1);
/// The repaint interval once every age reads in minutes.
const SLOW_REPAINT: Duration = Duration::from_secs(30);
const FRESH_BELOW_SECONDS: i64 = 60;

impl RootView {
    /// The facts and snapshot a listed row reads.
    fn dashboard_row_input<'a>(&'a self, row: &'a DashboardRow) -> Option<RowInput<'a>> {
        let session = self.sidebar.session(&row.session_id)?;
        Some(RowInput {
            row,
            session,
            facts: self.dashboard_facts.facts_for(&row.session_id),
            agents: self.sidebar.agents(),
            unseen: self.sidebar.is_unseen(&row.session_id),
        })
    }

    /// The By status view's groups in display order, each as its heading and
    /// the cells of its rows, as the view draws them.
    #[must_use]
    pub fn dashboard_view_rows(&self) -> Vec<(&'static str, Vec<RowCells>)> {
        let model = dashboard::model(&self.sidebar, self.dashboard_facts.facts());
        let now = Utc::now();
        GroupKind::ALL
            .into_iter()
            .map(|kind| {
                let rows = model
                    .rows(kind)
                    .iter()
                    .filter_map(|row| self.dashboard_row_input(row))
                    .map(|input| row_cells(&input, kind, now))
                    .collect();
                (kind.heading(), rows)
            })
            .collect()
    }

    /// The queue strip's chips, longest wait first; empty hides the strip.
    #[must_use]
    pub fn dashboard_queue(&self) -> Vec<QueueChip> {
        let model = dashboard::model(&self.sidebar, self.dashboard_facts.facts());
        let waits: Vec<RowInput<'_>> = model
            .waits()
            .iter()
            .filter_map(|row| self.dashboard_row_input(row))
            .collect();
        queue_chips(&waits, Utc::now())
    }

    /// The header's day figures, `None` while they are not known.
    #[must_use]
    pub fn dashboard_figures(&self) -> Option<HeaderFigures> {
        header_figures(&self.dashboard_facts, Utc::now())
    }

    /// The columns the view's last laid-out width shows; every one before
    /// the first layout.
    #[must_use]
    pub fn dashboard_columns(&self) -> Columns {
        visible_columns(self.dashboard_width.unwrap_or(f32::INFINITY))
    }

    /// The Dashboard tab's body.
    pub(crate) fn dashboard_tab_element(&self, cx: &mut Context<Self>) -> AnyElement {
        let columns = self.dashboard_columns();
        let counts = dashboard::model(&self.sidebar, self.dashboard_facts.facts()).counts();
        let accents = self.sidebar.session_accents();
        let mut body: Vec<AnyElement> = Vec::new();
        let mut row_at: HashMap<String, usize> = HashMap::new();
        for (heading, rows) in self.dashboard_view_rows() {
            body.push(group_heading(heading, rows.len()));
            for cells in &rows {
                row_at.insert(cells.session_id.clone(), body.len());
                let accent = accents
                    .get(cells.session_id.as_str())
                    .copied()
                    .unwrap_or(TEXT_2);
                body.push(self.dashboard_row(cells, columns, accent, cx));
                body.extend(
                    cells
                        .nested
                        .iter()
                        .map(|nested| nested_row(&cells.session_id, nested, columns, &theme(cx))),
                );
            }
        }
        let chips = self.dashboard_queue();
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .size_full()
            .min_w(px(0.0))
            .bg(gpui::rgb(GROUND))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(TEXT))
            .debug_selector(|| "dashboard-view".to_owned())
            .child(width_probe(cx))
            .child(header_strip(counts, self.dashboard_figures(), &theme(cx)))
            .when(!chips.is_empty(), |view| {
                view.child(queue_strip(&chips, &row_at, cx))
            })
            .child(column_header(columns))
            .child(
                div()
                    .id("dashboard-body")
                    .debug_selector(|| "dashboard-body".to_owned())
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.0))
                    .pb(px(20.0))
                    .overflow_y_scroll()
                    .track_scroll(&self.dashboard_scroll)
                    .children(body),
            )
            .into_any_element()
    }

    /// One listed session's two-line row.
    fn dashboard_row(
        &self,
        cells: &RowCells,
        columns: Columns,
        accent: u32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = cells.session_id.clone();
        let name = format!("dashboard-row-{id}");
        let mark = self.sidebar.session(&id).map_or_else(
            || {
                glyph(
                    SessionStatus::Idle,
                    false,
                    SessionMode::Interactive,
                    false,
                    &theme(cx),
                )
            },
            |s| {
                glyph(
                    s.status,
                    s.background,
                    s.mode,
                    self.sidebar.is_unseen(&s.id),
                    &theme(cx),
                )
            },
        );
        let asking = format!("{name}-asking");
        let selector = name.clone();
        grid_line(div().id(SharedString::from(name.clone())))
            .debug_selector(move || selector)
            .relative()
            .flex_none()
            .min_h(px(ROW_MIN_H))
            .py(px(5.0))
            .border_t_1()
            .border_color(gpui::rgb(LINE))
            .cursor_pointer()
            .hover(|style| style.bg(gpui::rgb(SURFACE)))
            .when(cells.asking, |row| {
                row.bg(gpui::rgba(ASK_WASH)).child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(2.0))
                        .bg(gpui::rgb(ASKING))
                        .debug_selector(move || asking),
                )
            })
            .child(fixed(GLYPH_W).child(glyph_view(
                mark,
                GlyphSize::Leaf,
                &format!("{name}-glyph"),
                &theme(cx),
            )))
            .child(fixed(SESSION_W).child(session_cell(cells, accent, &name)))
            .when(columns.linear, |row| {
                row.child(
                    fixed(LINEAR_W).child(
                        line_text(&format!("{name}-issue"), &cells.issue)
                            .font_family(DEFAULT_FAMILY)
                            .font_weight(FontWeight::BOLD),
                    ),
                )
            })
            .child(flexible().child(now_cell(cells, mark.shape, &name)))
            .when(columns.summary, |row| {
                row.child(flexible().child(summary_cell(cells, &name)))
            })
            .when(columns.agents, |row| {
                row.child(
                    fixed(AGENTS_W)
                        .text_color(gpui::rgb(TEXT_2))
                        .child(line_text(&format!("{name}-agents"), &cells.agents)),
                )
            })
            .child(fixed(CONTEXT_W).child(context_cell(cells)))
            .when(columns.activity, |row| {
                row.child(fixed(ACTIVITY_W).child(activity_cell(cells)))
            })
            .child(fixed(RUN_W).child(numbers(&cells.run, &cells.last)))
            .child(fixed(COST_W).child(numbers(&cells.cost, &cells.tokens)))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.select_session(&id, window, cx);
            }))
            .into_any_element()
    }

    /// Starts the ages' repaint when the Dashboard tab becomes active and
    /// drops it when another tab does.
    pub(crate) fn sync_dashboard_repaint(&mut self, cx: &mut Context<Self>) {
        if !self.dashboard_active() {
            self.dashboard_timer = None;
            return;
        }
        if self.dashboard_timer.is_none() {
            self.arm_dashboard_repaint(cx);
        }
    }

    fn arm_dashboard_repaint(&mut self, cx: &mut Context<Self>) {
        let delay = self.dashboard_repaint_delay();
        self.dashboard_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            // Fails only when the view is gone, and the tab with it.
            this.update(cx, Self::repaint_dashboard).ok();
        }));
    }

    /// The timer fired: repaint the ages and go again while the tab is
    /// still the active one.
    fn repaint_dashboard(&mut self, cx: &mut Context<Self>) {
        if !self.dashboard_active() {
            self.dashboard_timer = None;
            return;
        }
        cx.notify();
        self.arm_dashboard_repaint(cx);
    }

    /// Every second while a listed session's status stamp, start or last
    /// activity is under a minute old, else every 30.
    fn dashboard_repaint_delay(&self) -> Duration {
        let now = Utc::now();
        let model = dashboard::model(&self.sidebar, self.dashboard_facts.facts());
        let fresh = GroupKind::ALL
            .into_iter()
            .flat_map(|kind| model.rows(kind))
            .filter_map(|row| self.sidebar.session(&row.session_id))
            .flat_map(|session| {
                let last = self
                    .dashboard_facts
                    .facts_for(&session.id)
                    .and_then(|facts| facts.last_activity_at);
                [session.status_since, Some(session.started_at), last]
            })
            .flatten()
            .any(|at| (now - at).num_seconds() < FRESH_BELOW_SECONDS);
        if fresh { FAST_REPAINT } else { SLOW_REPAINT }
    }
}

/// Notes the view's laid-out width and redraws when it changed, so the
/// columns follow a resize.
fn width_probe(cx: &mut Context<RootView>) -> impl IntoElement {
    let root = cx.entity();
    canvas(
        move |bounds, _, cx| {
            let width = bounds.size.width / px(1.0);
            root.update(cx, |this, cx| {
                if this
                    .dashboard_width
                    .is_none_or(|held| (held - width).abs() > 0.5)
                {
                    this.dashboard_width = Some(width);
                    cx.notify();
                }
            });
        },
        |_, (), _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// A line of the grid: the row's padding and the gap between its cells.
fn grid_line<E: Styled>(element: E) -> E {
    element
        .flex()
        .items_center()
        .gap(px(GAP))
        .pl(px(PAD_LEFT))
        .pr(px(PAD_RIGHT))
}

fn fixed(width: f32) -> Div {
    div().flex_none().w(px(width)).min_w(px(0.0))
}

fn flexible() -> Div {
    div().flex_1().min_w(px(0.0))
}

/// One line of text that ends in `…` when its cell is too narrow.
fn line_text(id: &str, text: &str) -> Div {
    truncating(id, StyledText::new(text.to_owned()))
}

/// A cell's two lines, the second smaller and quieter.
fn two_lines(first: impl IntoElement, second: Option<Div>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .min_w(px(0.0))
        .child(first)
        .when_some(second, |cell, second| {
            cell.child(second.text_size(px(SMALL_TEXT)))
        })
}

fn session_cell(cells: &RowCells, accent: u32, name: &str) -> Div {
    let second = div()
        .flex()
        .items_center()
        .gap(px(5.0))
        .min_w(px(0.0))
        .child(
            div()
                .flex_none()
                .size(px(7.0))
                .rounded(px(2.0))
                .bg(gpui::rgb(accent)),
        )
        .child(
            div()
                .flex_none()
                .text_color(gpui::rgb(TEXT_2))
                .child(cells.container.clone()),
        )
        .child(
            line_text(&format!("{name}-branch"), &cells.branch)
                .flex_1()
                .font_family(DEFAULT_FAMILY)
                .text_color(gpui::rgb(SUBTLE)),
        );
    two_lines(
        line_text(&format!("{name}-label"), &cells.label).font_weight(FontWeight::SEMIBOLD),
        Some(second),
    )
}

/// The state word's colour: the glyph's hue, quieter for an idle, starting
/// or ended session.
fn state_colour(shape: Shape) -> u32 {
    match shape {
        Shape::Working | Shape::Background => WORKING,
        Shape::Asking => ASKING,
        Shape::Waiting => WAITING,
        Shape::Error => DANGER,
        Shape::Idle => TEXT_2,
        Shape::Spawning | Shape::Stopped => SUBTLE,
    }
}

fn now_cell(cells: &RowCells, shape: Shape, name: &str) -> Div {
    let first = div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .min_w(px(0.0))
        .overflow_hidden()
        .child(
            div()
                .flex_none()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(gpui::rgb(state_colour(shape)))
                .child(cells.state_word.clone()),
        )
        .when_some(cells.age.clone(), |line, age| {
            line.child(
                div()
                    .flex_none()
                    .text_color(gpui::rgb(SUBTLE))
                    .child(format!("· {age}")),
            )
        })
        .children(
            cells
                .stuck
                .iter()
                .enumerate()
                .map(|(index, flag)| stuck_flag(&format!("{name}-flag-{index}"), flag)),
        );
    two_lines(
        first,
        Some(line_text(&format!("{name}-now"), &cells.now).text_color(gpui::rgb(TEXT_2))),
    )
}

fn summary_cell(cells: &RowCells, name: &str) -> Div {
    two_lines(
        line_text(&format!("{name}-summary"), &cells.summary),
        cells.summary_older.as_ref().map(|older| {
            line_text(&format!("{name}-summary-older"), older).text_color(gpui::rgb(SUBTLE))
        }),
    )
}

/// One stuck flag: a tag that shrinks with its line and ends in `…` rather
/// than spilling into the next column.
fn stuck_flag(id: &str, text: &str) -> Div {
    let selector = id.to_owned();
    truncating(id, StyledText::new(text.to_owned()))
        .debug_selector(move || selector.clone())
        .flex_shrink()
        .min_w(px(0.0))
        .px(px(5.0))
        .rounded(px(3.0))
        .bg(gpui::rgba(DANGER_WASH))
        .text_size(px(11.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(gpui::rgb(DANGER))
}

/// The context bar and its figure, tagged `dashboard-ctx-{id}`, and
/// `dashboard-ctx-{id}-hot` on the bar's fill past 80%.
fn context_cell(cells: &RowCells) -> Div {
    let base = format!("dashboard-ctx-{}", cells.session_id);
    let hot = format!("{base}-hot");
    let colour = if cells.context_hot {
        CONTEXT_HOT
    } else {
        TEXT_2
    };
    let fill = cells
        .context_percent
        .map(|percent| BAR_W * f32::from(percent.min(100)) / 100.0);
    div()
        .flex()
        .items_center()
        .gap(px(5.0))
        .text_size(px(SMALL_TEXT))
        .text_color(gpui::rgb(colour))
        .debug_selector(move || base)
        .child(
            div()
                .flex_none()
                .w(px(BAR_W))
                .h(px(BAR_H))
                .rounded(px(3.0))
                .overflow_hidden()
                .bg(gpui::rgb(CHIP))
                .when_some(fill, |bar, fill| {
                    bar.child(
                        div()
                            .h_full()
                            .w(px(fill))
                            .bg(gpui::rgb(colour))
                            .when(cells.context_hot, |fill| fill.debug_selector(move || hot)),
                    )
                }),
        )
        .child(cells.context.clone())
}

/// The ten one-minute bars, faint while the window is quiet.
fn activity_cell(cells: &RowCells) -> Div {
    let name = format!("dashboard-spark-{}", cells.session_id);
    let (colour, opacity) = if cells.activity_quiet {
        (FAINT, 1.0)
    } else {
        (WORKING, 0.75)
    };
    div()
        .flex()
        .items_end()
        .gap(px(1.5))
        .h(px(18.0))
        .debug_selector(move || name)
        .children(cells.activity.iter().map(|height| {
            div()
                .flex_none()
                .w(px(5.0))
                .h(px(*height))
                .bg(gpui::rgb(colour))
                .opacity(opacity)
        }))
}

/// A right-aligned pair of figures, the second smaller and quieter.
fn numbers(first: &str, second: &str) -> Div {
    div()
        .flex()
        .flex_col()
        .items_end()
        .gap(px(2.0))
        .whitespace_nowrap()
        .child(first.to_owned())
        .child(
            div()
                .text_size(px(SMALL_TEXT))
                .text_color(gpui::rgb(SUBTLE))
                .child(second.to_owned()),
        )
}

/// A node of a Working row's agent tree: its name across the Session and
/// Linear columns, its task across Now and Summary, its stuck flag, and its
/// age under Run.
fn nested_row(session_id: &str, nested: &NestedRow, columns: Columns, t: &UiTheme) -> AnyElement {
    let name = format!("dashboard-sub-{session_id}-{}", nested.key);
    let name_w = SESSION_W + if columns.linear { GAP + LINEAR_W } else { 0.0 };
    let stuck_w = CONTEXT_W
        + if columns.agents { AGENTS_W + GAP } else { 0.0 }
        + if columns.activity {
            GAP + ACTIVITY_W
        } else {
            0.0
        };
    let indent = 12.0 + if nested.depth > 0 { 18.0 } else { 0.0 };
    let task = line_text(&format!("{name}-task"), &nested.task).text_color(gpui::rgb(TEXT_2));
    let task = if nested.mono {
        task.font_family(DEFAULT_FAMILY).text_size(px(SMALL_TEXT))
    } else {
        task
    };
    let selector = name.clone();
    grid_line(div())
        .flex_none()
        .h(px(SUBROW_H))
        .text_size(px(12.0))
        .debug_selector(move || selector)
        .when(nested.dimmed, |row| row.opacity(DIMMED))
        .child(fixed(GLYPH_W))
        .child(
            fixed(name_w).child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .pl(px(indent))
                    .min_w(px(0.0))
                    .child(
                        div()
                            .flex_none()
                            .w(px(14.0))
                            .font_family(DEFAULT_FAMILY)
                            .text_color(gpui::rgb(LINE_STRONG))
                            .child("└"),
                    )
                    .child(glyph_view(
                        agent_glyph(nested.state, t),
                        GlyphSize::Footer,
                        &format!("{name}-glyph"),
                        t,
                    ))
                    .child(line_text(&format!("{name}-name"), &nested.name).flex_1()),
            ),
        )
        .child(flexible().child(task))
        .child(
            fixed(stuck_w).when_some(nested.stuck.as_deref(), |cell, flag| {
                cell.child(stuck_flag(&format!("{name}-flag"), flag))
            }),
        )
        .child(
            fixed(RUN_W)
                .flex()
                .justify_end()
                .text_size(px(SMALL_TEXT))
                .text_color(gpui::rgb(SUBTLE))
                .child(nested.age.clone()),
        )
        .child(fixed(COST_W))
        .into_any_element()
}

/// A group's heading and its row count; the Needs you heading in amber.
fn group_heading(heading: &'static str, count: usize) -> AnyElement {
    let colour = if heading == GroupKind::NeedsYou.heading() {
        ASKING
    } else {
        TEXT_2
    };
    let name = format!(
        "dashboard-group-{}",
        heading.to_lowercase().replace(' ', "-")
    );
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.0))
        .pt(px(12.0))
        .pb(px(5.0))
        .pl(px(PAD_LEFT))
        .pr(px(PAD_RIGHT))
        .debug_selector(move || name)
        .child(
            div()
                .text_size(px(CAPS_TEXT))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(gpui::rgb(colour))
                .child(heading.to_uppercase()),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(SUBTLE))
                .child(count.to_string()),
        )
        .into_any_element()
}

/// The column names over the rows, outside the scrolled body so they stay.
fn column_header(columns: Columns) -> Div {
    let caps = |text: &'static str| {
        div()
            .text_size(px(CAPS_TEXT))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(gpui::rgb(SUBTLE))
            .child(text)
    };
    grid_line(div())
        .flex_none()
        .h(px(COLUMNS_H))
        .border_b_1()
        .border_color(gpui::rgb(LINE))
        .debug_selector(|| "dashboard-columns".to_owned())
        .child(fixed(GLYPH_W))
        .child(fixed(SESSION_W).child(caps("SESSION")))
        .when(columns.linear, |line| {
            line.child(fixed(LINEAR_W).child(caps("LINEAR")))
        })
        .child(flexible().child(caps("NOW")))
        .when(columns.summary, |line| {
            line.child(flexible().child(caps("SUMMARY")))
        })
        .when(columns.agents, |line| {
            line.child(fixed(AGENTS_W).child(caps("AGENTS")))
        })
        .child(fixed(CONTEXT_W).child(caps("CONTEXT")))
        .when(columns.activity, |line| {
            line.child(fixed(ACTIVITY_W).child(caps("10 MIN")))
        })
        .child(fixed(RUN_W).flex().justify_end().child(caps("RUN · LAST")))
        .child(fixed(COST_W).flex().justify_end().child(caps("COST")))
}

/// The title, the count pills and, once known, the day's figures.
fn header_strip(counts: HeaderCounts, figures: Option<HeaderFigures>, t: &UiTheme) -> Div {
    let marks = [
        glyph(
            SessionStatus::AwaitingInput,
            false,
            SessionMode::Interactive,
            false,
            t,
        ),
        glyph(
            SessionStatus::Working,
            false,
            SessionMode::Interactive,
            false,
            t,
        ),
        glyph(
            SessionStatus::Idle,
            false,
            SessionMode::Interactive,
            false,
            t,
        ),
        glyph(
            SessionStatus::Idle,
            false,
            SessionMode::Interactive,
            true,
            t,
        ),
    ];
    let pills =
        header_pills(counts)
            .into_iter()
            .zip(marks)
            .enumerate()
            .map(|(index, (pill, mark))| {
                let asking = index == 0;
                let name = format!("dashboard-pill-{}", pill.label.replace(' ', "-"));
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(9.0))
                    .py(px(3.0))
                    .rounded(px(5.0))
                    .text_color(gpui::rgb(if asking { ASKING } else { TEXT_2 }))
                    .when(asking, |pill| {
                        pill.bg(gpui::rgba(ASK_WASH))
                            .border_1()
                            .border_color(gpui::rgba(ASK_BORDER))
                    })
                    .when(!asking, |pill| pill.bg(gpui::rgb(CHIP)))
                    .child(glyph_view(
                        mark,
                        GlyphSize::Footer,
                        &format!("{name}-glyph"),
                        t,
                    ))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(gpui::rgb(if asking { ASKING } else { TEXT }))
                            .child(pill.count.to_string()),
                    )
                    .child(pill.label)
                    .debug_selector(move || name)
            });
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(14.0))
        .px(px(18.0))
        .py(px(10.0))
        .border_b_1()
        .border_color(gpui::rgb(LINE))
        .bg(gpui::rgb(SURFACE))
        .debug_selector(|| "dashboard-header".to_owned())
        .child(
            div()
                .text_size(px(15.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child("Dashboard"),
        )
        .child(
            div()
                .flex()
                .gap(px(4.0))
                .text_size(px(12.0))
                .children(pills),
        )
        .child(div().flex_1())
        .when_some(figures, |strip, figures| strip.child(figures_view(figures)))
}

fn figure(value: String, after: &'static str) -> Div {
    div()
        .flex()
        .gap(px(4.0))
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(gpui::rgb(TEXT))
                .child(value),
        )
        .child(after)
}

fn figures_view(figures: HeaderFigures) -> Div {
    let releases = figures.releases.into_iter().map(|release| {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a share between 0 and 1 of a 44 px bar"
        )]
        let fill = (44.0 * release.share) as f32;
        div()
            .flex()
            .items_center()
            .gap(px(5.0))
            .text_size(px(SMALL_TEXT))
            .child(release.name)
            .child(
                div()
                    .w(px(44.0))
                    .h(px(4.0))
                    .rounded(px(2.0))
                    .overflow_hidden()
                    .bg(gpui::rgb(CHIP))
                    .child(div().h_full().w(px(fill)).bg(gpui::rgb(LILAC))),
            )
            .child(format!("{}/{}", release.done, release.total))
    });
    div()
        .flex()
        .items_center()
        .gap(px(16.0))
        .text_size(px(12.0))
        .text_color(gpui::rgb(TEXT_2))
        .debug_selector(|| "dashboard-figures".to_owned())
        .child(
            div()
                .flex()
                .gap(px(4.0))
                .child("Today")
                .child(figure(figures.cost, "API-equiv")),
        )
        .child(figure(figures.tokens, "tokens"))
        .child(figure(figures.commits.to_string(), "commits"))
        .child(figure(figures.issues_done.to_string(), "issues Done"))
        .children(releases)
        .when_some(figures.synced, |strip, synced| {
            strip.child(
                div()
                    .text_size(px(11.0))
                    .text_color(gpui::rgb(SUBTLE))
                    .child(synced),
            )
        })
}

/// The sessions waiting on the user, longest first, each chip scrolling its
/// row into view, then the order's hint.
fn queue_strip(
    chips: &[QueueChip],
    row_at: &HashMap<String, usize>,
    cx: &mut Context<RootView>,
) -> Div {
    let mark = glyph(
        SessionStatus::AwaitingInput,
        false,
        SessionMode::Interactive,
        false,
        &theme(cx),
    );
    let chips = chips.iter().map(|chip| {
        let name = format!("dashboard-queue-{}", chip.session_id);
        let index = row_at.get(&chip.session_id).copied();
        let selector = name.clone();
        div()
            .id(SharedString::from(name.clone()))
            .debug_selector(move || selector)
            .flex()
            .flex_shrink()
            .items_center()
            .gap(px(7.0))
            .min_w(px(0.0))
            .max_w(px(560.0))
            .pl(px(7.0))
            .pr(px(9.0))
            .py(px(3.0))
            .rounded(px(5.0))
            .border_1()
            .border_color(gpui::rgba(ASK_BORDER))
            .bg(gpui::rgb(RAISED))
            .text_size(px(12.0))
            .cursor_pointer()
            .child(glyph_view(
                mark,
                GlyphSize::Leaf,
                &format!("{name}-glyph"),
                &theme(cx),
            ))
            .child(
                div()
                    .flex_none()
                    .font_family(DEFAULT_FAMILY)
                    .font_weight(FontWeight::BOLD)
                    .child(chip.title.clone()),
            )
            .child(line_text(&format!("{name}-prompt"), &chip.prompt).flex_1())
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(gpui::rgb(ASKING))
                    .child(chip.age.clone()),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                if let Some(index) = index {
                    this.dashboard_scroll.scroll_to_item(index);
                    cx.notify();
                }
            }))
    });
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.0))
        .px(px(18.0))
        .py(px(7.0))
        .border_b_1()
        .border_color(gpui::rgb(LINE))
        .bg(gpui::rgba(ASK_WASH))
        .debug_selector(|| "dashboard-queue-strip".to_owned())
        .child(
            div()
                .flex_none()
                .text_size(px(CAPS_TEXT))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(gpui::rgb(ASKING))
                .child("QUEUE"),
        )
        .children(chips)
        .child(div().flex_1())
        .child(
            div()
                .flex_none()
                .text_size(px(SMALL_TEXT))
                .text_color(gpui::rgb(SUBTLE))
                .child(QUEUE_HINT),
        )
}
