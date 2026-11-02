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

use crate::buttons::{ButtonSize, Layer, outlined_button, primary_button};
use crate::dashboard::{self, DashboardRow, GroupKind, HeaderCounts};
use crate::dashboard_rows::{
    AskBlock, AskChoice, Columns, GO_TO_PANE, HeaderFigures, NestedRow, QUEUE_HINT, QueueChip,
    RowCells, RowInput, agent_glyph, header_figures, header_pills, queue_chips, row_cells,
    visible_columns,
};
use crate::driver_id::DriverId as _;
use crate::ellipsis::truncating;
use crate::fonts::DEFAULT_FAMILY;
use crate::status_glyph::{GlyphSize, Shape, glyph, glyph_view};
use crate::target::HostKey;
use crate::ui_theme::{UiTheme, theme};
use crate::{RootView, UI_TEXT_SIZE};

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
            agents: self.sidebar.session_agents(&row.session_id),
            unseen: self.sidebar.is_unseen(&row.session_id),
            prompt: self.chat.held_prompt(&row.session_id),
        })
    }

    /// The By status view's groups in display order, each as its heading and
    /// the cells of its rows, as the view draws them.
    #[must_use]
    pub fn dashboard_view_rows(&self) -> Vec<(&'static str, Vec<RowCells>)> {
        let model = dashboard::model(&self.sidebar, &self.dashboard_facts.facts());
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
        let model = dashboard::model(&self.sidebar, &self.dashboard_facts.facts());
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
        self.dashboard_facts
            .store(&HostKey::Local)
            .and_then(|store| header_figures(store, Utc::now()))
    }

    /// The columns the view's last laid-out width shows; every one before
    /// the first layout.
    #[must_use]
    pub fn dashboard_columns(&self) -> Columns {
        visible_columns(self.dashboard_width.unwrap_or(f32::INFINITY))
    }

    /// The Dashboard tab's body.
    pub(crate) fn dashboard_tab_element(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = theme(cx);
        let columns = self.dashboard_columns();
        let counts = dashboard::model(&self.sidebar, &self.dashboard_facts.facts()).counts();
        let accents = self.sidebar.session_accents();
        let mut body: Vec<AnyElement> = Vec::new();
        let mut row_at: HashMap<String, usize> = HashMap::new();
        for (heading, rows) in self.dashboard_view_rows() {
            body.push(group_heading(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                heading,
                rows.len(),
                &t,
            ));
            for cells in &rows {
                row_at.insert(cells.session_id.clone(), body.len());
                let accent = accents
                    .get(cells.session_id.as_str())
                    .copied()
                    .unwrap_or(t.text_2);
                body.push(self.dashboard_row(cells, columns, accent, cx));
                if let Some(ask) = &cells.ask {
                    let answer_out = self
                        .sidebar
                        .session(&ask.session_id)
                        .is_some_and(|session| self.dashboard_answers.is_out(session));
                    body.push(ask_block_element(ask, answer_out, cx));
                }
                body.extend(cells.nested.iter().map(|nested| {
                    nested_row(
                        crate::fonts::ui_text_size(cx),
                        crate::fonts::ui_weight_shift(cx),
                        &cells.session_id,
                        nested,
                        columns,
                        &theme(cx),
                    )
                }));
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
            .bg(gpui::rgb(t.ground))
            .text_size(crate::fonts::ui_px(cx, UI_TEXT_SIZE))
            .text_color(gpui::rgb(t.text))
            .driver_id(|| "dashboard-view".to_owned())
            .child(width_probe(cx))
            .child(header_strip(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                counts,
                self.dashboard_figures(),
                &theme(cx),
            ))
            .when(!chips.is_empty(), |view| {
                view.child(queue_strip(&chips, &row_at, cx))
            })
            .child(column_header(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                columns,
                &t,
            ))
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
        let t = theme(cx);
        let offset = crate::fonts::ui_text_size(cx);
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
            .driver_id(move || selector)
            .relative()
            .flex_none()
            .min_h(px(ROW_MIN_H))
            .py(px(5.0))
            .border_t_1()
            .border_color(gpui::rgb(t.line))
            .cursor_pointer()
            .hover(|style| style.bg(gpui::rgb(t.surface)))
            .when(cells.asking, |row| {
                row.bg(gpui::rgba(t.ask_wash)).child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(2.0))
                        .bg(gpui::rgb(t.asking))
                        .driver_id(move || asking),
                )
            })
            .child(fixed(GLYPH_W).child(glyph_view(
                mark,
                GlyphSize::Leaf,
                &format!("{name}-glyph"),
                &theme(cx),
            )))
            .child(fixed(SESSION_W).child(session_cell(
                offset,
                crate::fonts::ui_weight_shift(cx),
                cells,
                accent,
                &name,
                &t,
            )))
            .when(columns.linear, |row| {
                row.child(
                    fixed(LINEAR_W).child(
                        line_text(&format!("{name}-issue"), &cells.issue)
                            .font_family(DEFAULT_FAMILY)
                            .font_weight(FontWeight::BOLD),
                    ),
                )
            })
            .child(flexible().child(now_cell(
                offset,
                crate::fonts::ui_weight_shift(cx),
                cells,
                mark.shape,
                &name,
                &t,
            )))
            .when(columns.summary, |row| {
                row.child(flexible().child(summary_cell(offset, cells, &name, &t)))
            })
            .when(columns.agents, |row| {
                row.child(
                    fixed(AGENTS_W)
                        .text_color(gpui::rgb(t.text_2))
                        .child(line_text(&format!("{name}-agents"), &cells.agents)),
                )
            })
            .child(fixed(CONTEXT_W).child(context_cell(offset, cells, &t)))
            .when(columns.activity, |row| {
                row.child(fixed(ACTIVITY_W).child(activity_cell(cells, &t)))
            })
            .child(fixed(RUN_W).child(numbers(offset, &cells.run, &cells.last, &t)))
            .child(fixed(COST_W).child(numbers(offset, &cells.cost, &cells.tokens, &t)))
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
        let model = dashboard::model(&self.sidebar, &self.dashboard_facts.facts());
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

/// The ask block under a waiting row, tagged `dash-ask-{id}`: its label and
/// the prompt's text, a permission's command in a mono box
/// (`dash-ask-{id}-command`), then a button per choice
/// (`dash-ask-choice-{id}-{n}`, `n` from 1) with its key hint, the first
/// primary, and `Go to pane` last (`dash-ask-go-{id}`). While an answer is
/// out for this wait (`answer_out`) the choice buttons draw disabled.
pub(crate) fn ask_block_element(
    block: &AskBlock,
    answer_out: bool,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = theme(cx);
    let offset = crate::fonts::ui_text_size(cx);
    let shift = crate::fonts::ui_weight_shift(cx);
    let id = block.session_id.clone();
    let selector = format!("dash-ask-{id}");
    let mut buttons: Vec<AnyElement> = Vec::with_capacity(block.choices.len() + 1);
    if let Some(since) = block.since {
        let answer_at = (!answer_out).then_some(since);
        for (index, choice) in block.choices.iter().enumerate() {
            buttons.push(ask_choice_button(&id, index, choice, answer_at, cx));
        }
    }
    let go = id.clone();
    buttons.push(
        outlined_button(
            offset,
            shift,
            &format!("dash-ask-go-{id}"),
            ButtonSize::Compact,
            true,
            Layer::Base,
            &t,
        )
        .text_color(gpui::rgb(t.text_2))
        .child(GO_TO_PANE)
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.select_session(&go, window, cx);
        }))
        .into_any_element(),
    );
    div()
        .relative()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .pl(px(PAD_LEFT + GLYPH_W + GAP))
        .pr(px(PAD_RIGHT))
        .pb(px(8.0))
        .bg(gpui::rgba(t.ask_wash))
        .driver_id(move || selector)
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(px(2.0))
                .bg(gpui::rgb(t.asking)),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .min_w(px(0.0))
                .child(
                    div()
                        .flex_none()
                        .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
                        .text_color(gpui::rgb(t.asking))
                        .child(block.label.clone()),
                )
                .child(line_text(&format!("dash-ask-{id}-text"), &block.text).flex_1()),
        )
        .when_some(block.command.clone(), |ask, command| {
            let name = format!("dash-ask-{id}-command");
            ask.child(
                div()
                    .flex_none()
                    .max_w_full()
                    .px(px(8.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(gpui::rgb(t.line))
                    .bg(gpui::rgb(t.surface))
                    .font_family(DEFAULT_FAMILY)
                    .font_weight(crate::fonts::mono_weight())
                    .text_size(crate::fonts::ui_px_at(offset, SMALL_TEXT))
                    .child(line_text(&name, &command))
                    .driver_id(move || name),
            )
        })
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(6.0))
                .children(buttons),
        )
        .into_any_element()
}

/// One answer button of an ask block: its label and key hint, primary when
/// it is the first; a press sends its choice against `answer_at`, and with
/// none (an answer is out) it draws disabled and takes no press.
fn ask_choice_button(
    id: &str,
    index: usize,
    choice: &AskChoice,
    answer_at: Option<chrono::DateTime<Utc>>,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = theme(cx);
    let offset = crate::fonts::ui_text_size(cx);
    let shift = crate::fonts::ui_weight_shift(cx);
    let selector = format!("dash-ask-choice-{id}-{}", index + 1);
    let make = if index == 0 {
        primary_button
    } else {
        outlined_button
    };
    let session = id.to_owned();
    let answer = choice.choice;
    make(
        offset,
        shift,
        &selector,
        ButtonSize::Compact,
        answer_at.is_some(),
        Layer::Base,
        &t,
    )
    .gap(px(6.0))
    .child(choice.label.clone())
    .when_some(choice.hint.clone(), |button, hint| {
        button.child(
            div()
                .opacity(0.65)
                .text_size(crate::fonts::ui_px_at(offset, CAPS_TEXT))
                .child(hint),
        )
    })
    .when_some(answer_at, |button, since| {
        button.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.answer_from_dashboard(&session, since, answer, cx);
        }))
    })
    .into_any_element()
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
fn two_lines(offset: i8, first: impl IntoElement, second: Option<Div>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .min_w(px(0.0))
        .child(first)
        .when_some(second, |cell, second| {
            cell.child(second.text_size(crate::fonts::ui_px_at(offset, SMALL_TEXT)))
        })
}

fn session_cell(
    offset: i8,
    shift: i8,
    cells: &RowCells,
    accent: u32,
    name: &str,
    t: &UiTheme,
) -> Div {
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
                .text_color(gpui::rgb(t.text_2))
                .child(cells.container.clone()),
        )
        .child(
            line_text(&format!("{name}-branch"), &cells.branch)
                .flex_1()
                .font_family(DEFAULT_FAMILY)
                .font_weight(crate::fonts::mono_weight())
                .text_color(gpui::rgb(t.subtle)),
        );
    two_lines(
        offset,
        line_text(&format!("{name}-label"), &cells.label)
            .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::SEMIBOLD)),
        Some(second),
    )
}

/// The state word's colour: the glyph's hue, quieter for an idle, starting
/// or ended session.
fn state_colour(shape: Shape, t: &UiTheme) -> u32 {
    match shape {
        Shape::Working | Shape::Background => t.working,
        Shape::Asking => t.asking,
        Shape::Waiting => t.waiting,
        Shape::Error => t.danger,
        Shape::Idle => t.text_2,
        Shape::Spawning | Shape::Stopped => t.subtle,
    }
}

fn now_cell(offset: i8, shift: i8, cells: &RowCells, shape: Shape, name: &str, t: &UiTheme) -> Div {
    let first = div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .min_w(px(0.0))
        .overflow_hidden()
        .child(
            div()
                .flex_none()
                .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
                .text_color(gpui::rgb(state_colour(shape, t)))
                .child(cells.state_word.clone()),
        )
        .when_some(cells.age.clone(), |line, age| {
            line.child(
                div()
                    .flex_none()
                    .text_color(gpui::rgb(t.subtle))
                    .child(format!("· {age}")),
            )
        })
        .children(cells.stuck.iter().enumerate().map(|(index, flag)| {
            stuck_flag(offset, shift, &format!("{name}-flag-{index}"), flag, t)
        }));
    two_lines(
        offset,
        first,
        Some(line_text(&format!("{name}-now"), &cells.now).text_color(gpui::rgb(t.text_2))),
    )
}

fn summary_cell(offset: i8, cells: &RowCells, name: &str, t: &UiTheme) -> Div {
    two_lines(
        offset,
        line_text(&format!("{name}-summary"), &cells.summary),
        cells.summary_older.as_ref().map(|older| {
            line_text(&format!("{name}-summary-older"), older).text_color(gpui::rgb(t.subtle))
        }),
    )
}

/// One stuck flag: a tag that shrinks with its line and ends in `…` rather
/// than spilling into the next column.
fn stuck_flag(offset: i8, shift: i8, id: &str, text: &str, t: &UiTheme) -> Div {
    let selector = id.to_owned();
    truncating(id, StyledText::new(text.to_owned()))
        .driver_id(move || selector.clone())
        .flex_shrink()
        .min_w(px(0.0))
        .px(px(5.0))
        .rounded(px(3.0))
        .bg(gpui::rgba(t.danger_wash))
        .text_size(crate::fonts::ui_px_at(offset, 11.0))
        .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
        .text_color(gpui::rgb(t.danger))
}

/// The context bar and its figure, tagged `dashboard-ctx-{id}`, and
/// `dashboard-ctx-{id}-hot` on the bar's fill past 80%.
fn context_cell(offset: i8, cells: &RowCells, t: &UiTheme) -> Div {
    let base = format!("dashboard-ctx-{}", cells.session_id);
    let hot = format!("{base}-hot");
    let colour = if cells.context_hot {
        t.context_hot
    } else {
        t.text_2
    };
    let fill = cells
        .context_percent
        .map(|percent| BAR_W * f32::from(percent.min(100)) / 100.0);
    div()
        .flex()
        .items_center()
        .gap(px(5.0))
        .text_size(crate::fonts::ui_px_at(offset, SMALL_TEXT))
        .text_color(gpui::rgb(colour))
        .driver_id(move || base)
        .child(
            div()
                .flex_none()
                .w(px(BAR_W))
                .h(px(BAR_H))
                .rounded(px(3.0))
                .overflow_hidden()
                .bg(gpui::rgb(t.chip))
                .when_some(fill, |bar, fill| {
                    bar.child(
                        div()
                            .h_full()
                            .w(px(fill))
                            .bg(gpui::rgb(colour))
                            .when(cells.context_hot, |fill| fill.driver_id(move || hot)),
                    )
                }),
        )
        .child(cells.context.clone())
}

/// The ten one-minute bars, faint while the window is quiet.
fn activity_cell(cells: &RowCells, t: &UiTheme) -> Div {
    let name = format!("dashboard-spark-{}", cells.session_id);
    let (colour, opacity) = if cells.activity_quiet {
        (t.faint, 1.0)
    } else {
        (t.working, 0.75)
    };
    div()
        .flex()
        .items_end()
        .gap(px(1.5))
        .h(px(18.0))
        .driver_id(move || name)
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
fn numbers(offset: i8, first: &str, second: &str, t: &UiTheme) -> Div {
    div()
        .flex()
        .flex_col()
        .items_end()
        .gap(px(2.0))
        .whitespace_nowrap()
        .child(first.to_owned())
        .child(
            div()
                .text_size(crate::fonts::ui_px_at(offset, SMALL_TEXT))
                .text_color(gpui::rgb(t.subtle))
                .child(second.to_owned()),
        )
}

/// A node of a Working row's agent tree: its name across the Session and
/// Linear columns, its task across Now and Summary, its stuck flag, and its
/// age under Run.
fn nested_row(
    offset: i8,
    shift: i8,
    session_id: &str,
    nested: &NestedRow,
    columns: Columns,
    t: &UiTheme,
) -> AnyElement {
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
    let task = line_text(&format!("{name}-task"), &nested.task).text_color(gpui::rgb(t.text_2));
    let task = if nested.mono {
        task.font_family(DEFAULT_FAMILY)
            .font_weight(crate::fonts::mono_weight())
            .text_size(crate::fonts::ui_px_at(offset, SMALL_TEXT))
    } else {
        task
    };
    let selector = name.clone();
    grid_line(div())
        .flex_none()
        .h(crate::fonts::ui_px_at(offset, SUBROW_H))
        .text_size(crate::fonts::ui_px_at(offset, 12.0))
        .driver_id(move || selector)
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
                            .font_weight(crate::fonts::mono_weight())
                            .text_color(gpui::rgb(t.line_strong))
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
                cell.child(stuck_flag(offset, shift, &format!("{name}-flag"), flag, t))
            }),
        )
        .child(
            fixed(RUN_W)
                .flex()
                .justify_end()
                .text_size(crate::fonts::ui_px_at(offset, SMALL_TEXT))
                .text_color(gpui::rgb(t.subtle))
                .child(nested.age.clone()),
        )
        .child(fixed(COST_W))
        .into_any_element()
}

/// A group's heading and its row count; the Needs you heading in amber.
fn group_heading(
    offset: i8,
    shift: i8,
    heading: &'static str,
    count: usize,
    t: &UiTheme,
) -> AnyElement {
    let colour = if heading == GroupKind::NeedsYou.heading() {
        t.asking
    } else {
        t.text_2
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
        .driver_id(move || name)
        .child(
            div()
                .text_size(crate::fonts::ui_px_at(offset, CAPS_TEXT))
                .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
                .text_color(gpui::rgb(colour))
                .child(heading.to_uppercase()),
        )
        .child(
            div()
                .text_size(crate::fonts::ui_px_at(offset, 11.0))
                .text_color(gpui::rgb(t.subtle))
                .child(count.to_string()),
        )
        .into_any_element()
}

/// The column names over the rows, outside the scrolled body so they stay.
fn column_header(offset: i8, shift: i8, columns: Columns, t: &UiTheme) -> Div {
    let caps = |text: &'static str| {
        div()
            .text_size(crate::fonts::ui_px_at(offset, CAPS_TEXT))
            .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
            .text_color(gpui::rgb(t.subtle))
            .child(text)
    };
    grid_line(div())
        .flex_none()
        .h(crate::fonts::ui_px_at(offset, COLUMNS_H))
        .border_b_1()
        .border_color(gpui::rgb(t.line))
        .driver_id(|| "dashboard-columns".to_owned())
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
fn header_strip(
    offset: i8,
    shift: i8,
    counts: HeaderCounts,
    figures: Option<HeaderFigures>,
    t: &UiTheme,
) -> Div {
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
                    .text_color(gpui::rgb(if asking { t.asking } else { t.text_2 }))
                    .when(asking, |pill| {
                        pill.bg(gpui::rgba(t.ask_wash))
                            .border_1()
                            .border_color(gpui::rgba(t.ask_border))
                    })
                    .when(!asking, |pill| pill.bg(gpui::rgb(t.chip)))
                    .child(glyph_view(
                        mark,
                        GlyphSize::Footer,
                        &format!("{name}-glyph"),
                        t,
                    ))
                    .child(
                        div()
                            .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
                            .text_color(gpui::rgb(if asking { t.asking } else { t.text }))
                            .child(pill.count.to_string()),
                    )
                    .child(pill.label)
                    .driver_id(move || name)
            });
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(14.0))
        .px(px(18.0))
        .py(px(10.0))
        .border_b_1()
        .border_color(gpui::rgb(t.line))
        .bg(gpui::rgb(t.surface))
        .driver_id(|| "dashboard-header".to_owned())
        .child(
            div()
                .text_size(crate::fonts::ui_px_at(offset, 15.0))
                .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
                .child("Dashboard"),
        )
        .child(
            div()
                .flex()
                .gap(px(4.0))
                .text_size(crate::fonts::ui_px_at(offset, 12.0))
                .children(pills),
        )
        .child(div().flex_1())
        .when_some(figures, |strip, figures| {
            strip.child(figures_view(offset, shift, figures, t))
        })
}

fn figure(shift: i8, value: String, after: &'static str, t: &UiTheme) -> Div {
    div()
        .flex()
        .gap(px(4.0))
        .child(
            div()
                .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
                .text_color(gpui::rgb(t.text))
                .child(value),
        )
        .child(after)
}

fn figures_view(offset: i8, shift: i8, figures: HeaderFigures, t: &UiTheme) -> Div {
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
            .text_size(crate::fonts::ui_px_at(offset, SMALL_TEXT))
            .child(release.name)
            .child(
                div()
                    .w(px(44.0))
                    .h(px(4.0))
                    .rounded(px(2.0))
                    .overflow_hidden()
                    .bg(gpui::rgb(t.chip))
                    .child(div().h_full().w(px(fill)).bg(gpui::rgb(t.lilac))),
            )
            .child(format!("{}/{}", release.done, release.total))
    });
    div()
        .flex()
        .items_center()
        .gap(px(16.0))
        .text_size(crate::fonts::ui_px_at(offset, 12.0))
        .text_color(gpui::rgb(t.text_2))
        .driver_id(|| "dashboard-figures".to_owned())
        .child(div().flex().gap(px(4.0)).child("Today").child(figure(
            shift,
            figures.cost,
            "API-equiv",
            t,
        )))
        .child(figure(shift, figures.tokens, "tokens", t))
        .child(figure(shift, figures.commits.to_string(), "commits", t))
        .when_some(figures.issues_done, |strip, done| {
            strip.child(figure(shift, done.to_string(), "issues Done", t))
        })
        .children(releases)
        .when_some(figures.synced, |strip, synced| {
            strip.child(
                div()
                    .text_size(crate::fonts::ui_px_at(offset, 11.0))
                    .text_color(gpui::rgb(t.subtle))
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
    let t = theme(cx);
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
            .driver_id(move || selector)
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
            .border_color(gpui::rgba(t.ask_border))
            .bg(gpui::rgb(t.raised))
            .text_size(crate::fonts::ui_px(cx, 12.0))
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
                    .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                    .text_color(gpui::rgb(t.asking))
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
        .border_color(gpui::rgb(t.line))
        .bg(gpui::rgba(t.ask_wash))
        .driver_id(|| "dashboard-queue-strip".to_owned())
        .child(
            div()
                .flex_none()
                .text_size(crate::fonts::ui_px(cx, CAPS_TEXT))
                .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                .text_color(gpui::rgb(t.asking))
                .child("QUEUE"),
        )
        .children(chips)
        .child(div().flex_1())
        .child(
            div()
                .flex_none()
                .text_size(crate::fonts::ui_px(cx, SMALL_TEXT))
                .text_color(gpui::rgb(t.subtle))
                .child(QUEUE_HINT),
        )
}
