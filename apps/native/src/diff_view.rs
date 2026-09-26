//! A read-only side-by-side diff: one uniform list whose every item holds a
//! row's old and new halves, so both sides scroll together. Each half has a
//! line-number gutter and its line in the terminal font; long lines are not
//! wrapped but scroll sideways together, and only the part of a line the
//! text column shows is laid out.

#![expect(clippy::unreadable_literal, reason = "colours read as #rrggbbaa")]

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    Context, DispatchPhase, Div, FocusHandle, HighlightStyle, Hsla, KeyDownEvent, ScrollStrategy,
    ScrollWheelEvent, SharedString, StyledText, UniformListScrollHandle, Window, canvas, div, font,
    prelude::*, px, rgba, uniform_list,
};

use crate::diff_model::{DiffModel, Row, RowKind};
use crate::fonts::{self, FontSettings};
use crate::theme;

/// A deleted line's half, and the old half of a modified row.
const DELETE_BG: u32 = 0xf8514926;
/// An inserted line's half, and the new half of a modified row.
const INSERT_BG: u32 = 0x3fb95026;
/// A changed word on the old side.
const DELETE_WORD_BG: u32 = 0xf8514959;
/// A changed word on the new side.
const INSERT_WORD_BG: u32 = 0x3fb95059;
/// The half of a row whose side has no line.
const FILLER_BG: u32 = 0x8080800f;
const TEXT: u32 = 0xccccccff;
const GUTTER_TEXT: u32 = 0x6e7681ff;
/// The bar left of the current hunk's rows.
const CURRENT_BAR: u32 = 0x4c8dffcc;
const CURRENT_BAR_WIDTH: f32 = 2.0;
/// The space between a gutter's number and its line.
const GUTTER_PAD: f32 = 8.0;
/// The line between the two halves.
const DIVIDER: u32 = 0x20222aff;
const DIVIDER_WIDTH: f32 = 1.0;

/// Where [`DiffView::go`] moves the current hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    First,
    Prev,
    Next,
    Last,
}

/// The font the rows draw in, resolved once.
#[derive(Debug, Clone)]
struct Metrics {
    family: SharedString,
    char_width: f32,
    line_height: f32,
}

/// Which side of a row a half draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Half {
    Old,
    New,
}

/// The character columns every line shows in a frame: `len` of them from
/// `start`, drawn `shift` pixels left of the text column's edge.
#[derive(Debug, Clone, Copy)]
struct Columns {
    start: usize,
    len: usize,
    shift: f32,
}

impl Columns {
    /// The columns a text column scrolled `h_offset` pixels right shows,
    /// for a text column no wider than `max_width`.
    fn new(h_offset: f32, max_width: f32, char_width: f32) -> Self {
        let char_width = char_width.max(1.0);
        let start = (h_offset / char_width).floor().max(0.0);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "both are non-negative column counts far below usize::MAX"
        )]
        let (first, len) = (
            start as usize,
            (max_width / char_width).ceil().max(0.0) as usize + 2,
        );
        Self {
            start: first,
            len,
            shift: h_offset - start * char_width,
        }
    }
}

/// A side-by-side view of a [`DiffModel`].
pub struct DiffView {
    model: DiffModel,
    font: FontSettings,
    metrics: Option<Metrics>,
    scroll: UniformListScrollHandle,
    /// How far both halves' text is scrolled right, in pixels.
    h_offset: f32,
    /// The hunk the change keys last moved to.
    current: Option<usize>,
    focus: FocusHandle,
    /// The rows the list drew last.
    rendered: Range<usize>,
    /// The text each half of the rows the list drew last shows.
    shown: Vec<(Option<SharedString>, Option<SharedString>)>,
    /// The width of one half's text column, in pixels, recorded at paint.
    column_width: Rc<Cell<f32>>,
    /// The digits of the largest line number.
    gutter_digits: usize,
    /// The characters of the longest line on either side.
    longest_line: usize,
}

impl DiffView {
    /// A view of `model` in `font`, the app's terminal font.
    pub fn new(model: DiffModel, font: FontSettings, cx: &mut Context<Self>) -> Self {
        let (mut largest, mut longest) = (0, 0);
        for row in model.rows() {
            for (side, text) in [
                (
                    row.left.as_ref(),
                    row.left.as_ref().map(|s| model.old_text(s)),
                ),
                (
                    row.right.as_ref(),
                    row.right.as_ref().map(|s| model.new_text(s)),
                ),
            ] {
                largest = largest.max(side.map_or(0, |s| s.line_no));
                longest = longest.max(text.map_or(0, |t| t.chars().count()));
            }
        }
        Self {
            model,
            font: font.normalized(),
            metrics: None,
            scroll: UniformListScrollHandle::new(),
            h_offset: 0.0,
            current: None,
            focus: cx.focus_handle(),
            rendered: 0..0,
            shown: Vec::new(),
            column_width: Rc::new(Cell::new(0.0)),
            gutter_digits: largest.max(1).to_string().len(),
            longest_line: longest,
        }
    }

    #[must_use]
    pub fn model(&self) -> &DiffModel {
        &self.model
    }

    #[must_use]
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// The list's scroll handle.
    #[must_use]
    pub fn scroll_handle(&self) -> &UniformListScrollHandle {
        &self.scroll
    }

    /// How far the list is scrolled down, in pixels.
    #[must_use]
    pub fn scroll_top(&self) -> f32 {
        -(self.scroll.0.borrow().base_handle.offset().y / px(1.0))
    }

    /// How far the lines are scrolled right, in pixels.
    #[must_use]
    pub fn h_offset(&self) -> f32 {
        self.h_offset
    }

    /// The character columns a half's text column shows, the partly shown
    /// ones included.
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "both are non-negative column counts far below usize::MAX"
    )]
    pub fn visible_columns(&self) -> Range<usize> {
        let char_width = self.char_width().max(1.0);
        let start = (self.h_offset / char_width).floor().max(0.0);
        let end = ((self.h_offset + self.column_width.get()) / char_width)
            .ceil()
            .max(0.0);
        start as usize..end as usize
    }

    /// The hunk the change keys last moved to.
    #[must_use]
    pub fn current_hunk(&self) -> Option<usize> {
        self.current
    }

    /// The rows the list drew last.
    #[must_use]
    pub fn rendered_rows(&self) -> Range<usize> {
        self.rendered.clone()
    }

    /// The (old, new) text of the rows the list drew last: the part of each
    /// line the text column's scroll position covers. A filler half has
    /// none.
    #[must_use]
    pub fn shown_text(&self) -> &[(Option<SharedString>, Option<SharedString>)] {
        &self.shown
    }

    /// The (old, new) line numbers of the rows the list drew last; a filler
    /// half has none.
    #[must_use]
    pub fn shown_line_numbers(&self) -> Vec<(Option<u32>, Option<u32>)> {
        self.model
            .rows()
            .get(self.rendered.clone())
            .unwrap_or_default()
            .iter()
            .map(|row| {
                (
                    row.left.as_ref().map(|s| s.line_no),
                    row.right.as_ref().map(|s| s.line_no),
                )
            })
            .collect()
    }

    /// Moves the current hunk and scrolls it into view: a hunk that fits the
    /// viewport has its middle row centred, a taller one its first row at
    /// the top. Next and previous wrap; see [`Self::step`] for where they
    /// start from.
    pub fn go(&mut self, nav: Nav, cx: &mut Context<Self>) {
        let target = match nav {
            Nav::First => self.model.first_hunk(),
            Nav::Last => self.model.last_hunk(),
            Nav::Next | Nav::Prev => self.step(nav),
        };
        let Some(hunk) = target else {
            return;
        };
        self.current = Some(hunk);
        if let Some(rows) = self.model.hunks().get(hunk) {
            if rows.len() <= self.viewport_rows() {
                let middle = rows.start + rows.len() / 2;
                self.scroll
                    .scroll_to_item_strict(middle, ScrollStrategy::Center);
            } else {
                self.scroll
                    .scroll_to_item_strict(rows.start, ScrollStrategy::Top);
            }
        }
        cx.notify();
    }

    /// The hunk Next or Prev moves to. From the current hunk while any of
    /// its rows is drawn; otherwise Next goes to the first hunk starting at
    /// or below the first drawn row and Prev to the last one starting above
    /// it. Both wrap.
    fn step(&self, nav: Nav) -> Option<usize> {
        let hunks = self.model.hunks();
        let drawn = &self.rendered;
        let current = self
            .current
            .and_then(|hunk| hunks.get(hunk))
            .filter(|hunk| hunk.start < drawn.end && drawn.start < hunk.end);
        match (nav, current) {
            (Nav::Prev, Some(hunk)) => self.model.prev_hunk(hunk.start),
            (Nav::Prev, None) => self.model.prev_hunk(drawn.start),
            (_, Some(hunk)) => self.model.next_hunk(hunk.start),
            (_, None) => hunks
                .iter()
                .position(|hunk| hunk.start >= drawn.start)
                .or_else(|| self.model.first_hunk()),
        }
    }

    /// How many whole rows the list's viewport holds; none before the
    /// first draw.
    fn viewport_rows(&self) -> usize {
        let height = self.scroll.0.borrow().base_handle.bounds().size.height / px(1.0);
        let line_height = self
            .metrics
            .as_ref()
            .map_or(16.0, |m| m.line_height)
            .max(1.0);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a non-negative row count far below usize::MAX"
        )]
        let rows = (height / line_height).floor().max(0.0) as usize;
        rows
    }

    /// F7 goes to the next change and Shift+F7 to the previous one.
    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let mods = ks.modifiers;
        if ks.key != "f7" || mods.control || mods.alt || mods.platform {
            return;
        }
        self.go(if mods.shift { Nav::Prev } else { Nav::Next }, cx);
        cx.stop_propagation();
    }

    /// Shift+wheel, and a wheel's sideways part, scroll the lines sideways.
    /// A sideways-only or Shift event stops here, so the list does not turn
    /// it into a vertical scroll.
    fn on_wheel(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let line_height = px(self.metrics.as_ref().map_or(16.0, |m| m.line_height));
        let delta = event.delta.pixel_delta(line_height);
        let sideways_only = delta.y == px(0.0);
        let shift = event.modifiers.shift;
        let dx = if shift && delta.x == px(0.0) {
            delta.y
        } else {
            delta.x
        };
        if dx != px(0.0) {
            let max = self.max_h_offset();
            self.h_offset = (self.h_offset - dx / px(1.0)).clamp(0.0, max);
            cx.notify();
        }
        if shift || sideways_only {
            cx.stop_propagation();
        }
    }

    /// The width of one character, before the first draw a guess.
    fn char_width(&self) -> f32 {
        self.metrics.as_ref().map_or(8.0, |m| m.char_width)
    }

    /// The farthest the lines scroll right: far enough that the longest
    /// line's last character reaches the text column's right edge, and not
    /// at all when every line fits.
    fn max_h_offset(&self) -> f32 {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a line's character count is far below f32's exact range"
        )]
        let chars = self.longest_line as f32;
        (chars * self.char_width() - self.column_width.get()).max(0.0)
    }

    /// The width of a half's line-number gutter.
    fn gutter_width(&self, metrics: &Metrics) -> f32 {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a line number has a handful of digits"
        )]
        let digits = self.gutter_digits as f32;
        digits * metrics.char_width + GUTTER_PAD
    }

    /// The font metrics, resolved on the first draw.
    fn metrics(&mut self, window: &Window) -> Metrics {
        if let Some(metrics) = &self.metrics {
            return metrics.clone();
        }
        let text = window.text_system();
        let available = fonts::available_families(text);
        let family = fonts::resolve_family(self.font.family.as_deref(), &available);
        let font_id = text.resolve_font(&font(family.clone()));
        let size = px(self.font.size);
        let char_width = text
            .advance(font_id, size, 'm')
            .map_or(self.font.size * 0.6, |s| s.width / px(1.0));
        let ascent = text.ascent(font_id, size) / px(1.0);
        let descent = text.descent(font_id, size) / px(1.0);
        let metrics = Metrics {
            family,
            char_width,
            line_height: fonts::line_height(ascent, descent, self.font.size),
        };
        self.metrics = Some(metrics.clone());
        metrics
    }

    /// The rows `range` in a window `viewport_width` pixels wide, which no
    /// text column is wider than.
    fn render_rows(&mut self, range: Range<usize>, viewport_width: f32) -> Vec<Div> {
        self.rendered = range.clone();
        let Some(metrics) = self.metrics.clone() else {
            self.shown.clear();
            return Vec::new();
        };
        let columns = Columns::new(self.h_offset, viewport_width, metrics.char_width);
        let mut shown = Vec::with_capacity(range.len());
        let rows: Vec<Div> = range
            .filter_map(|index| Some((index, self.model.rows().get(index)?)))
            .map(|(index, row)| {
                let current = self
                    .current
                    .and_then(|hunk| self.model.hunks().get(hunk))
                    .is_some_and(|hunk| hunk.contains(&index));
                let (old, old_text) = self.half(index, row, Half::Old, columns, &metrics);
                let (new, new_text) = self.half(index, row, Half::New, columns, &metrics);
                shown.push((old_text, new_text));
                div()
                    .w_full()
                    .h(px(metrics.line_height))
                    .flex()
                    .flex_row()
                    .child(
                        div()
                            .w(px(CURRENT_BAR_WIDTH))
                            .h_full()
                            .flex_none()
                            .when(current, |bar| bar.bg(rgba(CURRENT_BAR))),
                    )
                    .child(old)
                    .child(
                        div()
                            .w(px(DIVIDER_WIDTH))
                            .h_full()
                            .flex_none()
                            .bg(rgba(DIVIDER)),
                    )
                    .child(new)
            })
            .collect();
        self.shown = shown;
        rows
    }

    /// One half of row `index`: its gutter and the part of its line
    /// `columns` covers, or a filler; and the text it shows.
    fn half(
        &self,
        index: usize,
        row: &Row,
        half: Half,
        columns: Columns,
        metrics: &Metrics,
    ) -> (Div, Option<SharedString>) {
        let cell = div().flex_1().min_w(px(0.0)).h_full().flex().flex_row();
        let side = match half {
            Half::Old => row.left.as_ref(),
            Half::New => row.right.as_ref(),
        };
        let Some(side) = side else {
            return (cell.bg(rgba(FILLER_BG)), None);
        };
        let changed = row.kind != RowKind::Equal;
        let (text, tint, word_tint) = match half {
            Half::Old => (self.model.old_text(side), DELETE_BG, DELETE_WORD_BG),
            Half::New => (self.model.new_text(side), INSERT_BG, INSERT_WORD_BG),
        };
        let words = self.model.inline(index).map(|spans| match half {
            Half::Old => spans.left.as_slice(),
            Half::New => spans.right.as_slice(),
        });
        let (text, words) =
            visible_slice(text, words.unwrap_or_default(), columns.start, columns.len);
        let word_style = HighlightStyle {
            background_color: Some(Hsla::from(rgba(word_tint))),
            ..HighlightStyle::default()
        };
        let text = SharedString::from(text.to_owned());
        let line = StyledText::new(text.clone())
            .with_highlights(words.into_iter().map(|range| (range, word_style)));
        let cell = cell
            .when(changed, |cell| cell.bg(rgba(tint)))
            .child(
                div()
                    .w(px(self.gutter_width(metrics)))
                    .h_full()
                    .flex_none()
                    .pr(px(GUTTER_PAD / 2.0))
                    .text_right()
                    .text_color(rgba(GUTTER_TEXT))
                    .child(side.line_no.to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .h_full()
                    .relative()
                    .overflow_hidden()
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left(px(-columns.shift))
                            .whitespace_nowrap()
                            .child(line),
                    ),
            );
        (cell, Some(text))
    }
}

/// The `len` characters of `text` from character `start`, with `spans` (byte
/// ranges into `text`) clipped to them and rebased onto the slice. Spans
/// wholly outside the slice are dropped, and every range stays on character
/// boundaries.
fn visible_slice<'a>(
    text: &'a str,
    spans: &[Range<usize>],
    start: usize,
    len: usize,
) -> (&'a str, Vec<Range<usize>>) {
    let from = text
        .char_indices()
        .nth(start)
        .map_or(text.len(), |(at, _)| at);
    let rest = text.get(from..).unwrap_or_default();
    let to = from
        + rest
            .char_indices()
            .nth(len)
            .map_or(rest.len(), |(at, _)| at);
    let shown = text.get(from..to).unwrap_or_default();
    let clipped = spans
        .iter()
        .filter_map(|span| {
            let mut span_start = span.start.max(from);
            let mut span_end = span.end.min(to);
            if span_start >= span_end {
                return None;
            }
            while !text.is_char_boundary(span_start) {
                span_start -= 1;
            }
            while !text.is_char_boundary(span_end) {
                span_end += 1;
            }
            Some(span_start - from..span_end - from)
        })
        .collect();
    (shown, clipped)
}

/// The theme's background as a gpui colour.
fn background() -> gpui::Rgba {
    let bg = theme::DEFAULT_BACKGROUND;
    gpui::Rgba {
        r: f32::from(bg.r) / 255.0,
        g: f32::from(bg.g) / 255.0,
        b: f32::from(bg.b) / 255.0,
        a: 1.0,
    }
}

impl Render for DiffView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let metrics = self.metrics(window);
        // A wider window since the last wheel may leave less to scroll.
        self.h_offset = self.h_offset.min(self.max_h_offset());
        let view = cx.entity();
        let list = uniform_list(
            "diff-rows",
            self.model.rows().len(),
            cx.processor(|this, range, window: &mut Window, _cx| {
                this.render_rows(range, window.viewport_size().width / px(1.0))
            }),
        )
        .track_scroll(self.scroll.clone())
        .size_full();
        let gutter = self.gutter_width(&metrics);
        let column_width = self.column_width.clone();
        // Registered in the capture phase, so a Shift+wheel reaches this
        // view before the list turns it into a vertical scroll. Painting it
        // records the width of a half's text column, which bounds how far
        // the lines scroll sideways; a new width redraws the view, so its
        // render clamps the scroll against it. gpui drops a notify sent
        // while a frame is drawn, so the redraw is deferred past it.
        let wheel = canvas(
            |_, _, _| {},
            move |bounds, (), window, cx| {
                let half = (bounds.size.width / px(1.0) - CURRENT_BAR_WIDTH - DIVIDER_WIDTH) / 2.0;
                let width = (half - gutter).max(0.0);
                if (column_width.get() - width).abs() > f32::EPSILON {
                    column_width.set(width);
                    let redraw = view.clone();
                    cx.defer(move |cx| redraw.update(cx, |_, cx| cx.notify()));
                }
                window.on_mouse_event(move |event: &ScrollWheelEvent, phase, _, cx| {
                    if phase == DispatchPhase::Capture && bounds.contains(&event.position) {
                        view.update(cx, |this, cx| this.on_wheel(event, cx));
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        div()
            .id("diff-view")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .relative()
            .size_full()
            .bg(background())
            .text_color(rgba(TEXT))
            .font_family(metrics.family)
            .text_size(px(self.font.size))
            .line_height(px(metrics.line_height))
            .child(list)
            .child(wheel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice(
        text: &str,
        spans: &[Range<usize>],
        start: usize,
        len: usize,
    ) -> (String, Vec<Range<usize>>) {
        let (text, spans) = visible_slice(text, spans, start, len);
        (text.to_owned(), spans)
    }

    /// A list of just `range`.
    fn one(range: Range<usize>) -> Vec<Range<usize>> {
        vec![range]
    }

    #[test]
    fn an_ascii_slice_from_mid_line() {
        assert_eq!(slice("abcdefghij", &[], 3, 4), ("defg".to_owned(), vec![]));
    }

    #[test]
    fn spans_cut_by_the_slice_are_clipped_and_rebased() {
        let (text, spans) = slice("abcdefghij", &[1..4, 6..9], 2, 6);
        assert_eq!(text, "cdefgh");
        assert_eq!(spans, [0..2, 4..6], "cd and gh");
        let (_, spans) = slice("abcdefghij", &one(0..10), 2, 6);
        assert_eq!(spans, one(0..6), "a span over the whole slice");
    }

    #[test]
    fn spans_outside_the_slice_are_dropped() {
        let (text, spans) = slice("abcdefghij", &[0..2, 8..10], 3, 4);
        assert_eq!(text, "defg");
        assert!(spans.is_empty(), "{spans:?}");
    }

    #[test]
    fn multi_byte_characters_are_never_split() {
        let text = "aé日本😀z";
        assert_eq!(slice(text, &[], 1, 3).0, "é日本");
        assert_eq!(slice(text, &[], 3, 2).0, "本😀");
        assert_eq!(slice(text, &[], 5, 9).0, "z");
        let whole = 0..text.len();
        let (shown, spans) = slice(text, &one(whole), 2, 3);
        assert_eq!(shown, "日本😀");
        assert_eq!(spans, one(0..shown.len()));
        let (shown, spans) = slice(text, &one(2..7), 1, 3);
        assert_eq!(shown, "é日本");
        assert_eq!(
            spans,
            one(0..8),
            "a span off char boundaries widens to them"
        );
    }

    #[test]
    fn an_offset_past_the_end_shows_nothing() {
        assert_eq!(slice("abc", &one(0..3), 10, 5), (String::new(), vec![]));
        assert_eq!(slice("abc", &one(0..3), 3, 5), (String::new(), vec![]));
    }

    #[test]
    fn a_zero_offset_leaves_a_short_line_as_it_is() {
        assert_eq!(
            slice("abc", &one(1..2), 0, 80),
            ("abc".to_owned(), one(1..2))
        );
    }
}
