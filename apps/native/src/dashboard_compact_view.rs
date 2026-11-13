//! The Dashboard tab's Compact view and the header's view toggle: stacked
//! lanes with no scroll container at all, built from `dashboard_compact`'s
//! blocks. Each block is a repo cell, then a row per session (its session
//! cell, then its agent tree as chips that wrap), then the fold row. A row
//! click shows the session's pane, as a By status row's does; a waiting row
//! does not answer from here.

use std::collections::HashMap;

use chrono::Utc;
use gpui::{
    AnyElement, App, ClickEvent, Context, Div, FontWeight, SharedString, StyledText, div, font,
    prelude::*, px, svg,
};
use protocol::{AgentState, SessionMode, SessionStatus};

use crate::assets::{
    SIDEBAR_BRANCH_ICON, SIDEBAR_FOLDER_ICON, SIDEBAR_SHELL_ICON, SIDEBAR_WORKSPACE_ICON,
};
use crate::dashboard::{self, DashboardRow, GroupKind};
use crate::dashboard_compact::{
    BLOCK_GAP, BODY_PAD_X, BODY_PAD_Y, CHIP_GAP_X, CHIP_GAP_Y, CHIP_H, CHIPS_PAD_X, CHIPS_PAD_Y,
    COMPACT_EMPTY_TEXT, ChipKind, CompactBlock, CompactChip, CompactLayout, CompactRow,
    REPO_LINE_GAP, REPO_LINES, REPO_PAD_Y, REPO_W, ROW_H, TEXT_PX, block_h, chips_room, fit,
    layout,
};
use crate::dashboard_rows::agent_glyph;
use crate::dashboard_view::{state_colour, stuck_flag};
use crate::driver_id::DriverId as _;
use crate::ellipsis::truncating;
use crate::fonts::{DEFAULT_FAMILY, ui_family, ui_px, ui_text_size, ui_weight};
use crate::sidebar::{ContainerKind, DashboardView};
use crate::status_glyph::{Glyph, GlyphSize, glyph, glyph_view};
use crate::ui_theme::{UiTheme, theme};
use crate::{RootView, tooltip};

/// The session cell's fixed columns, as the mock's grid sizes them, and the
/// gap between them; the now line takes what is left, and the state word's
/// column is measured ([`state_column_w`]). Fixed so the rows' columns line
/// up and nothing in the view scrolls.
const LABEL_W: f32 = 196.0;
const ISSUE_W: f32 = 64.0;
const AGE_W: f32 = 32.0;
const CELL_GAP: f32 = 8.0;
/// The longest state word, which sizes the state column.
const WIDEST_STATE: &str = "Permission";
/// A dispatch chip's fill and border: the workspace colour at 10% and 30%.
const DISPATCH_FILL_ALPHA: u32 = 0x1a;
const DISPATCH_EDGE_ALPHA: u32 = 0x4d;
/// The arrow a dispatch chip draws before its name.
const DISPATCH_ARROW: &str = "↗";

/// The toggle's two views: the value, label, hover text and selector.
const VIEW_CHOICES: [(DashboardView, &str, &str, &str); 2] = [
    (
        DashboardView::Compact,
        "Compact",
        "Every agent at a glance, no scrolling",
        "dashboard-view-compact",
    ),
    (
        DashboardView::ByStatus,
        "By status",
        "Grouped rows, with the answers to what agents ask",
        "dashboard-view-by-status",
    ),
];

impl RootView {
    fn dashboard_compact_layout(&self, cx: &App) -> CompactLayout {
        layout(
            self.dashboard_width.unwrap_or(f32::INFINITY),
            ui_text_size(cx),
        )
    }

    /// The height the Compact view's fit counts `block` at, in the view's
    /// measured width and the UI text size.
    #[must_use]
    pub fn dashboard_compact_block_h(&self, block: &CompactBlock, cx: &App) -> f32 {
        block_h(block, self.dashboard_compact_layout(cx))
    }

    /// The Compact view's blocks in the sidebar's order, folded to fit the
    /// view's measured height, as the view draws them.
    #[must_use]
    pub fn dashboard_compact_blocks(&self, cx: &App) -> Vec<CompactBlock> {
        let model = dashboard::model(&self.sidebar, &self.dashboard_facts.facts());
        let mut listed: HashMap<&str, (GroupKind, &DashboardRow)> = HashMap::new();
        for kind in GroupKind::ALL {
            for row in model.rows(kind) {
                listed.insert(row.session_id.as_str(), (kind, row));
            }
        }
        let now = Utc::now();
        let mut blocks = Vec::new();
        for (index, container) in self.sidebar.containers().into_iter().enumerate() {
            let mut block = CompactBlock::new(index, container.name, container.kind);
            for leaf in &container.leaves {
                let Some(&(kind, row)) = listed.get(leaf.id.as_str()) else {
                    continue;
                };
                if let Some(input) = self.dashboard_row_input(row) {
                    block.add(&input, kind, now);
                }
            }
            if !block.is_empty() {
                blocks.push(block);
            }
        }
        fit(
            &mut blocks,
            self.dashboard_compact_layout(cx),
            self.dashboard_height.unwrap_or(f32::INFINITY),
        );
        blocks
    }

    /// The Compact view: its blocks, clipped, never scrolled.
    pub(crate) fn dashboard_compact_element(&self, cx: &mut Context<Self>) -> AnyElement {
        let layout = self.dashboard_compact_layout(cx);
        let blocks = self.dashboard_compact_blocks(cx);
        let empty = blocks.is_empty();
        let state_w = state_column_w(cx);
        let drawn: Vec<AnyElement> = blocks
            .iter()
            .map(|block| self.compact_block(block, layout, state_w, cx))
            .collect();
        let t = theme(cx);
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .flex_col()
            .gap(px(BLOCK_GAP))
            .px(px(BODY_PAD_X))
            .py(px(BODY_PAD_Y))
            .overflow_hidden()
            .text_size(ui_px(cx, TEXT_PX))
            .driver_id(|| "dashboard-compact".to_owned())
            .when(empty, |view| {
                view.child(
                    div()
                        .text_color(gpui::rgb(t.subtle))
                        .driver_id(|| "dashboard-compact-empty".to_owned())
                        .child(COMPACT_EMPTY_TEXT),
                )
            })
            .children(drawn)
            .into_any_element()
    }

    /// One container's block: its repo cell, its rows and its fold row, the
    /// state words in columns `state_w` px wide.
    fn compact_block(
        &self,
        block: &CompactBlock,
        layout: CompactLayout,
        state_w: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = theme(cx);
        let rows: Vec<AnyElement> = block
            .rows
            .iter()
            .enumerate()
            .map(|(at, row)| self.compact_row(row, layout, state_w, at == 0, cx))
            .collect();
        let name = format!("dashboard-compact-repo-{}", block.index);
        div()
            .flex()
            .flex_none()
            .min_w(px(0.0))
            .overflow_hidden()
            .rounded(px(6.0))
            .border_1()
            .border_color(gpui::rgb(t.card_edge))
            .bg(gpui::rgb(t.sunken))
            .driver_id(move || name)
            .child(repo_cell(block, cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.0))
                    .children(rows)
                    .when(block.has_fold_row(), |cell| cell.child(fold_row(block, cx))),
            )
            .into_any_element()
    }

    /// One session's row: the session cell, then the chips cell, beside it
    /// or, in a narrow view, under it.
    fn compact_row(
        &self,
        row: &CompactRow,
        layout: CompactLayout,
        state_w: f32,
        first: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = theme(cx);
        let name = format!("dashboard-compact-row-{}", row.session_id);
        let id = row.session_id.clone();
        let selector = name.clone();
        let session = self.session_cell(row, layout, state_w, cx);
        let chips_name = format!("dashboard-compact-chips-{}", row.session_id);
        let chip_w = chips_room(layout.chips_w);
        let chips: Vec<AnyElement> = row
            .chips
            .iter()
            .enumerate()
            .map(|(n, chip)| chip_element(&row.session_id, n, chip, chip_w, cx))
            .collect();
        let chips_cell = div()
            .flex()
            .flex_wrap()
            .content_start()
            .gap_x(px(CHIP_GAP_X))
            .gap_y(px(CHIP_GAP_Y))
            .px(px(CHIPS_PAD_X))
            .when(!row.chips.is_empty(), |cell| cell.py(px(CHIPS_PAD_Y)))
            .min_w(px(0.0))
            .when(layout.stacked, Styled::w_full)
            .when(!layout.stacked, Styled::flex_1)
            .driver_id(move || chips_name)
            .children(chips);
        div()
            .id(SharedString::from(name))
            .driver_id(move || selector)
            .flex()
            .flex_none()
            .when(layout.stacked, Styled::flex_col)
            .min_h(ui_px(cx, ROW_H))
            .min_w(px(0.0))
            .cursor_pointer()
            .when(!first, |line| {
                line.border_t_1().border_color(gpui::rgb(t.card_edge))
            })
            .when(row.asking, |line| line.bg(gpui::rgba(t.ask_wash)))
            .hover(|style| style.bg(gpui::rgb(t.surface)))
            .child(session)
            .child(chips_cell)
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.select_session(&id, window, cx);
            }))
            .into_any_element()
    }

    /// The session cell's one line: glyph, label, state word (in a column
    /// `state_w` px wide), stuck flags and now line, then the issue key and
    /// the age while the width keeps them. With the chips under it, the label
    /// shares the room with the now line rather than taking its fixed column.
    fn session_cell(
        &self,
        row: &CompactRow,
        layout: CompactLayout,
        state_w: f32,
        cx: &mut Context<Self>,
    ) -> Div {
        let t = theme(cx);
        let offset = crate::fonts::ui_text_size(cx);
        let shift = crate::fonts::ui_weight_shift(cx);
        let name = format!("dashboard-compact-row-{}", row.session_id);
        let part = |suffix: &str| format!("{name}-{suffix}");
        let mark = self.row_glyph(&row.session_id, &t);
        let now = div()
            .flex()
            .flex_1()
            .items_center()
            .gap(px(6.0))
            .min_w(px(0.0))
            .overflow_hidden()
            .driver_id({
                let selector = part("now");
                move || selector
            })
            .children(row.stuck.iter().enumerate().map(|(index, flag)| {
                stuck_flag(offset, shift, &part(&format!("flag-{index}")), flag, &t)
            }))
            .when(!row.now.is_empty(), |now| {
                now.child(tagged(
                    cell_text(&part("now-text"), &row.now)
                        .flex_1()
                        .text_color(gpui::rgb(t.text_2)),
                    part("now-text"),
                ))
            });
        let label = if layout.stacked {
            div().flex_1().min_w(px(0.0))
        } else {
            fixed(LABEL_W)
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(CELL_GAP))
            .h(ui_px(cx, ROW_H))
            .pl(px(9.0))
            .pr(px(10.0))
            .min_w(px(0.0))
            .overflow_hidden()
            .when(layout.stacked, Styled::w_full)
            .when(!layout.stacked, |cell| cell.w(px(layout.session_w)))
            .child(tagged(div().flex_none(), part("glyph")).child(glyph_view(
                mark,
                GlyphSize::Leaf,
                &part("glyph"),
                &t,
            )))
            .child(
                tagged(label, part("label")).child(
                    cell_text(&part("label-text"), &row.label)
                        .text_size(ui_px(cx, 12.5))
                        .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                        .text_color(gpui::rgb(t.text)),
                ),
            )
            .child(
                tagged(fixed(state_w), part("state")).child(
                    cell_text(&part("state-text"), &row.state)
                        .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                        .text_color(gpui::rgb(state_colour(mark.shape, &t))),
                ),
            )
            .child(now)
            .when(layout.issue, |cell| {
                cell.child(tagged(fixed(ISSUE_W), part("issue")).child(
                    cell_text(&part("issue-text"), &row.issue).text_color(gpui::rgb(t.subtle)),
                ))
            })
            .when(layout.age, |cell| {
                cell.child(
                    tagged(fixed(AGE_W).flex().justify_end(), part("age")).child(
                        cell_text(&part("age-text"), row.age.as_deref().unwrap_or_default())
                            .text_color(gpui::rgb(t.subtle)),
                    ),
                )
            })
    }

    /// The header's `Compact | By status` control. It is one keyboard stop
    /// (`on_dashboard_toggle_key`), so the accent border follows the focus.
    pub(crate) fn dashboard_view_toggle(&self, cx: &mut Context<Self>) -> Div {
        let t = theme(cx);
        let current = self.sidebar.dashboard_view();
        let segments = VIEW_CHOICES.iter().map(|&(view, label, tip, selector)| {
            let active = view == current;
            div()
                .id(selector)
                .driver_id(move || selector.to_owned())
                .flex()
                .items_center()
                .h(ui_px(cx, 22.0))
                .px(px(9.0))
                .rounded(px(4.0))
                .text_size(ui_px(cx, 11.5))
                .cursor_pointer()
                .tooltip(tooltip(tip))
                .when(active, |segment| {
                    segment
                        .bg(gpui::rgb(t.hover))
                        .font_weight(crate::fonts::ui_weight(cx, FontWeight::MEDIUM))
                        .text_color(gpui::rgb(t.text))
                })
                .when(!active, |segment| {
                    segment
                        .text_color(gpui::rgb(t.text_2))
                        .hover(move |style| style.text_color(gpui::rgb(t.text)))
                })
                .child(label)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.set_dashboard_view(view, cx);
                }))
        });
        div()
            .track_focus(&self.dashboard_toggle_focus)
            .flex()
            .flex_none()
            .p(px(2.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(gpui::rgba(0))
            .focus(move |style| style.border_color(gpui::rgb(t.accent)))
            .bg(gpui::rgb(t.raised))
            .children(segments)
    }
}

/// `element` tagged `selector` for the specs.
fn tagged(element: Div, selector: String) -> Div {
    element.driver_id(move || selector)
}

fn fixed(width: f32) -> Div {
    div().flex_none().w(px(width)).min_w(px(0.0))
}

/// One line of text that ends in `…` when its cell is too narrow.
fn cell_text(id: &str, text: &str) -> Div {
    truncating(id, StyledText::new(text.to_owned()))
}

/// The state column's width: [`WIDEST_STATE`] measured in the interface's
/// family at the cell's weight and the view's text size, rounded up with a
/// pixel to spare, so no state word ends in `…`.
#[expect(
    clippy::cast_precision_loss,
    reason = "a state word is far shorter than f32's exact integer range"
)]
fn state_column_w(cx: &App) -> f32 {
    let size = ui_px(cx, TEXT_PX);
    let face = gpui::Font {
        weight: ui_weight(cx, FontWeight::SEMIBOLD),
        ..font(ui_family(cx))
    };
    let text = cx.text_system();
    let id = text.resolve_font(&face);
    let measured: anyhow::Result<f32> = WIDEST_STATE
        .chars()
        .map(|ch| {
            text.advance(id, size, ch)
                .map(|advance| advance.width / px(1.0))
        })
        .sum();
    match measured {
        Ok(width) => width.ceil() + 1.0,
        Err(err) => {
            // A face without one of the word's glyphs: a full em a letter
            // is wider than any glyph it falls back to.
            tracing::warn!("measuring the Dashboard's state column: {err:#}");
            WIDEST_STATE.chars().count() as f32 * (size / px(1.0))
        }
    }
}

/// A status glyph for a session in `status`, for the repo cell's tally.
fn tally_glyph(status: SessionStatus, unseen: bool, t: &UiTheme) -> Glyph {
    glyph(status, false, SessionMode::Interactive, unseen, t)
}

/// The block's left cell: the container's icon and name, its kind and
/// session count, and its sessions by state.
fn repo_cell(block: &CompactBlock, cx: &mut Context<RootView>) -> Div {
    let t = theme(cx);
    let name = format!("dashboard-compact-repo-{}", block.index);
    let (icon, colour) = match block.kind {
        ContainerKind::Workspace => (SIDEBAR_WORKSPACE_ICON, t.lilac),
        ContainerKind::Repo => (SIDEBAR_BRANCH_ICON, t.text_2),
        _ => (SIDEBAR_FOLDER_ICON, t.text_2),
    };
    let name_colour = if block.is_workspace() {
        t.lilac
    } else {
        t.text
    };
    let tally = [
        (
            "asking",
            block.tally.asking,
            SessionStatus::AwaitingInput,
            false,
        ),
        (
            "working",
            block.tally.working,
            SessionStatus::Working,
            false,
        ),
        ("done", block.tally.done, SessionStatus::Idle, true),
        ("idle", block.tally.idle, SessionStatus::Idle, false),
    ];
    let counts = tally.into_iter().filter(|&(_, count, _, _)| count > 0).map(
        |(state, count, status, unseen)| {
            let selector = format!("{name}-tally-{state}");
            div()
                .flex()
                .items_center()
                .gap(px(3.0))
                .child(glyph_view(
                    tally_glyph(status, unseen, &t),
                    GlyphSize::Footer,
                    &selector,
                    &t,
                ))
                .child(count.to_string())
                .driver_id(move || selector)
        },
    );
    let name_id = format!("{name}-name");
    // Each line's height is set, so the cell is as tall as the estimate
    // counts it.
    let line = |at: usize| {
        let height = ui_px(cx, REPO_LINES[at]);
        div().flex_none().h(height).line_height(height)
    };
    div()
        .flex()
        .flex_col()
        .flex_none()
        .gap(px(REPO_LINE_GAP))
        .w(px(REPO_W))
        .min_w(px(0.0))
        .px(px(10.0))
        .py(px(REPO_PAD_Y))
        .overflow_hidden()
        .bg(gpui::rgb(t.surface))
        .border_r_1()
        .border_color(gpui::rgb(t.card_edge))
        .child(
            line(0)
                .flex()
                .items_center()
                .gap(px(5.0))
                .min_w(px(0.0))
                .child(kind_icon(icon, 13.0, colour))
                .child(
                    cell_text(&name_id, &block.name)
                        .flex_1()
                        .text_size(ui_px(cx, 13.0))
                        .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                        .text_color(gpui::rgb(name_colour))
                        .driver_id(move || name_id),
                ),
        )
        .child(
            line(1)
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_size(ui_px(cx, 10.5))
                .text_color(gpui::rgb(t.subtle))
                .child(block.tag())
                .child(block.sessions().to_string()),
        )
        .child(
            line(2)
                .flex()
                .items_center()
                .gap(px(8.0))
                .text_color(gpui::rgb(t.subtle))
                .children(counts),
        )
}

fn kind_icon(path: &'static str, size: f32, colour: u32) -> impl IntoElement {
    svg()
        .path(path)
        .flex_none()
        .size(px(size))
        .text_color(gpui::rgb(colour))
}

/// A `rgb` colour at `alpha`, as `gpui::rgba` reads it.
fn faded(colour: u32, alpha: u32) -> u32 {
    (colour << 8) | alpha
}

/// One node's chip: its glyph, its kind's icon, its name and what it does,
/// at most `max_w` wide; a finished node muted, a failed one in danger.
fn chip_element(
    session_id: &str,
    n: usize,
    chip: &CompactChip,
    max_w: f32,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = theme(cx);
    let name = format!("dashboard-compact-chip-{session_id}-{n}");
    let finished = chip.state == AgentState::Finished;
    let failed = chip.state == AgentState::Failed;
    let (fill, edge) = match chip.kind {
        _ if failed => (gpui::rgba(t.danger_wash), gpui::rgb(t.danger)),
        ChipKind::Agent => (gpui::rgb(t.chip), gpui::rgba(0)),
        ChipKind::Shell => (gpui::rgba(0), gpui::rgb(t.line_strong)),
        ChipKind::Dispatch => (
            gpui::rgba(faded(t.lilac, DISPATCH_FILL_ALPHA)),
            gpui::rgba(faded(t.lilac, DISPATCH_EDGE_ALPHA)),
        ),
    };
    let name_colour = if finished { t.subtle } else { t.text };
    let what_colour = if failed {
        t.danger
    } else if finished {
        t.subtle
    } else {
        t.text_2
    };
    let mono = chip.kind != ChipKind::Agent;
    let label = cell_text(&format!("{name}-name"), &chip.name)
        .flex_shrink(1.0)
        .text_color(gpui::rgb(name_colour))
        .when(mono, |text| {
            text.font_family(DEFAULT_FAMILY)
                .font_weight(crate::fonts::mono_weight())
                .text_size(ui_px(cx, 11.5))
        })
        .when(!mono && !finished, |text| {
            text.font_weight(crate::fonts::ui_weight(cx, FontWeight::MEDIUM))
        });
    let selector = name.clone();
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(5.0))
        .h(ui_px(cx, CHIP_H))
        .pl(px(6.0))
        .pr(px(7.0))
        .min_w(px(0.0))
        .when(max_w.is_finite(), |chip| chip.max_w(px(max_w.max(0.0))))
        .overflow_hidden()
        .rounded(px(4.0))
        .border_1()
        .border_color(edge)
        .bg(fill)
        .driver_id(move || selector)
        .child(glyph_view(
            agent_glyph(chip.state, &t),
            GlyphSize::Footer,
            &format!("{name}-glyph"),
            &t,
        ))
        .when(chip.kind == ChipKind::Shell, |line| {
            line.child(kind_icon(SIDEBAR_SHELL_ICON, 11.0, t.subtle))
        })
        .when(chip.kind == ChipKind::Dispatch, |line| {
            line.child(
                div()
                    .flex_none()
                    .text_color(gpui::rgb(t.lilac))
                    .child(DISPATCH_ARROW),
            )
        })
        .child(label)
        .child(
            cell_text(&format!("{name}-what"), &chip.what)
                .flex_shrink(1.0)
                .text_color(gpui::rgb(what_colour)),
        )
        .into_any_element()
}

/// The block's last row: its ended count, then the idle sessions a fold
/// hides, as a count and their labels.
fn fold_row(block: &CompactBlock, cx: &App) -> AnyElement {
    let t = &theme(cx);
    let weight = ui_weight(cx, FontWeight::SEMIBOLD);
    let name = format!("dashboard-compact-fold-{}", block.index);
    let count_chip = |suffix: &str, text: String, mark: Option<Glyph>| {
        let selector = format!("{name}-{suffix}");
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(5.0))
            .h(px(20.0))
            .px(px(7.0))
            .rounded(px(4.0))
            .bg(gpui::rgb(t.chip))
            .font_weight(weight)
            .text_color(gpui::rgb(t.text_2))
            .when_some(mark, |chip, mark| {
                chip.child(glyph_view(mark, GlyphSize::Footer, &selector, t))
            })
            .child(text)
            .driver_id(move || selector)
    };
    let selector = name.clone();
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.0))
        .h(ui_px(cx, ROW_H))
        .px(px(9.0))
        .min_w(px(0.0))
        .overflow_hidden()
        .when(!block.rows.is_empty(), |row| {
            row.border_t_1().border_color(gpui::rgb(t.card_edge))
        })
        .driver_id(move || selector)
        .when_some(block.ended_text(), |row, text| {
            row.child(count_chip("ended", text, None))
        })
        .when_some(block.idle_text(), |row, text| {
            row.child(count_chip(
                "idle",
                text,
                Some(tally_glyph(SessionStatus::Idle, false, t)),
            ))
            .child(
                cell_text(&format!("{name}-names"), &block.folded.join(" · "))
                    .flex_1()
                    .text_color(gpui::rgb(t.subtle)),
            )
        })
        .into_any_element()
}
