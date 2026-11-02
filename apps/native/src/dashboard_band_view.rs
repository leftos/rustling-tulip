//! The need-you band: the one `Need you` column of the sessions waiting on
//! the user, each with its ask block, washed in the asking colour while any
//! wait exists and calm with none.
//!
//! By repo has no home yet, so nothing in the product draws the band: RT-501
//! mounts it, and the specs switch it on for the root view through
//! [`enable_band_probe`]. The column's texts and numbers come from
//! `dashboard`; this file only lays them out.

use std::cell::Cell;

use chrono::Utc;
use gpui::{
    AnyElement, Context, FontWeight, IntoElement, ParentElement, Styled, div, prelude::*, px,
};

use crate::RootView;
use crate::UI_TEXT_SIZE;
use crate::dashboard::{self, BandWait, band as model_band, needs_you_count};
use crate::dashboard_rows::RowInput;
use crate::dashboard_view::ask_block_element;
use crate::driver_id::DriverId as _;
use crate::status_glyph::{GlyphSize, glyph_view};
use crate::ui_theme::theme;

/// The column's heading and the ask block's label capitals.
const CAPS_TEXT: f32 = 10.5;

thread_local! {
    /// Whether this thread draws the band; only a spec switches it on.
    static BAND_PROBE: Cell<bool> = const { Cell::new(false) };
}

/// Switches the band's drawing on for this thread, so the UI specs can read
/// and press it while nothing mounts it yet. Nothing else calls this.
pub fn enable_band_probe() {
    BAND_PROBE.with(|probe| probe.set(true));
}

/// Whether the band draws here.
fn band_probe_on() -> bool {
    BAND_PROBE.with(Cell::get)
}

impl RootView {
    /// The band over the root view while a spec has switched the probe on;
    /// nothing draws it in the product until RT-501 mounts it.
    pub(crate) fn dashboard_band_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !band_probe_on() {
            return None;
        }
        let model = dashboard::model(&self.sidebar, &self.dashboard_facts.facts());
        let inputs: Vec<RowInput<'_>> = model
            .waits()
            .iter()
            .filter_map(|row| {
                Some(RowInput {
                    row,
                    session: self.sidebar.session(&row.session_id)?,
                    facts: self.dashboard_facts.facts_for(&row.session_id),
                    agents: self.sidebar.session_agents(&row.session_id),
                    unseen: self.sidebar.is_unseen(&row.session_id),
                    prompt: self.chat.held_prompt(&row.session_id),
                })
            })
            .collect();
        let waits = model_band(&inputs, &theme(cx), Utc::now());
        let answers = |id: &str| {
            self.sidebar
                .session(id)
                .is_some_and(|session| self.dashboard_answers.is_out(session))
        };
        Some(
            div()
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .right(px(0.0))
                .child(band(&waits.waits, &answers, cx))
                .into_any_element(),
        )
    }
}

/// The band: one `Need you` column, washed in the asking colour while the
/// wait count is above zero and calm otherwise. `answer_out` says whether an
/// answer is already out for a wait, whose buttons then draw disabled.
pub(crate) fn band(
    waits: &[BandWait],
    answer_out: &dyn Fn(&str) -> bool,
    cx: &mut Context<RootView>,
) -> impl IntoElement {
    let t = theme(cx);
    let offset = crate::fonts::ui_text_size(cx);
    let shift = crate::fonts::ui_weight_shift(cx);
    let washed = needs_you_count(waits.len(), false) > 0;
    let state = if washed {
        "dashboard-band-washed"
    } else {
        "dashboard-band-calm"
    };
    let rows: Vec<AnyElement> = waits
        .iter()
        .map(|wait| wait_row(wait, answer_out, cx))
        .collect();
    div()
        .flex()
        .gap(px(10.0))
        .p(px(10.0))
        .when(washed, |view| view.bg(gpui::rgba(t.ask_wash)))
        .driver_id(|| "dashboard-band".to_owned())
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .gap(px(8.0))
                .min_w(px(0.0))
                .driver_id(move || state.to_owned())
                .child(
                    div()
                        .flex_none()
                        .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
                        .text_size(crate::fonts::ui_px_at(offset, CAPS_TEXT))
                        .text_color(gpui::rgb(t.text_2))
                        .child("Need you"),
                )
                .children(rows)
                .when(waits.is_empty(), |column| {
                    column.child(
                        div()
                            .flex_none()
                            .text_size(crate::fonts::ui_px(cx, UI_TEXT_SIZE))
                            .text_color(gpui::rgb(t.text_2))
                            .driver_id(|| "dashboard-band-empty".to_owned())
                            .child("No agent is waiting."),
                    )
                }),
        )
}

/// One wait's row: its glyph, label, repo, state and age, then the ask block
/// that answers it. The ask block's own selectors are the Dashboard rows'.
fn wait_row(
    wait: &BandWait,
    answer_out: &dyn Fn(&str) -> bool,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = theme(cx);
    let offset = crate::fonts::ui_text_size(cx);
    let shift = crate::fonts::ui_weight_shift(cx);
    let id = wait.session_id.clone();
    let selector = format!("dashboard-band-wait={id}");
    let glyph_id = format!("dashboard-band-glyph-{id}");
    let answer = wait
        .ask
        .as_ref()
        .map(|ask| ask_block_element(ask, answer_out(&ask.session_id), cx));
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .driver_id(move || selector)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .min_w(px(0.0))
                .child(glyph_view(wait.glyph, GlyphSize::Leaf, &glyph_id, &t))
                .child(
                    div()
                        .flex_none()
                        .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
                        .text_size(crate::fonts::ui_px_at(offset, UI_TEXT_SIZE))
                        .child(wait.label.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(crate::fonts::ui_px_at(offset, UI_TEXT_SIZE))
                        .text_color(gpui::rgb(t.text_2))
                        .child(wait.repo.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(crate::fonts::ui_px_at(offset, UI_TEXT_SIZE))
                        .text_color(gpui::rgb(t.asking))
                        .child(wait.state.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(crate::fonts::ui_px_at(offset, UI_TEXT_SIZE))
                        .text_color(gpui::rgb(t.text_2))
                        .child(wait.age.clone()),
                ),
        )
        .children(answer)
        .into_any_element()
}
