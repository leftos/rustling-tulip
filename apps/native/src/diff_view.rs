//! A read-only side-by-side diff: one uniform list whose every item holds a
//! row's old and new halves, so both sides scroll together. Each half has a
//! line-number gutter and its line in the terminal font; long lines are not
//! wrapped but scroll sideways together, and only the part of a line the
//! text column shows is laid out. Tokens take their syntax class's colour
//! once the tab hands the view each side's classes. An overview ruler down
//! the right edge marks where each hunk sits in the whole diff.

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    Context, DispatchPhase, Div, FocusHandle, HighlightStyle, Hsla, KeyDownEvent, ScrollStrategy,
    ScrollWheelEvent, SharedString, StyledText, UniformListScrollHandle, Window, canvas,
    combine_highlights, div, font, prelude::*, px, rgb, uniform_list,
};

use crate::diff_model::{DiffModel, Row, RowKind, Side};
use crate::fonts::{self, FontSettings};
use crate::palette::{
    ACCENT, DIFF_DELETE, DIFF_DELETE_WASH_ALPHA, DIFF_FILLER, DIFF_FILLER_ALPHA,
    DIFF_INSERT_WASH_ALPHA, DIFF_WORD_WASH_ALPHA, LINE, OCHRE, SUBTLE, TERMINAL_GROUND, TEXT,
    WAITING,
};
use crate::syntax::{Highlighted, TokenClass};

const CURRENT_BAR_WIDTH: f32 = 2.0;
/// The least width of a half's line-number gutter.
const GUTTER_MIN_WIDTH: f32 = 48.0;
/// The space between a gutter's number and its line.
const GUTTER_PAD: f32 = 12.0;
/// The size of a gutter's line numbers.
const GUTTER_TEXT_SIZE: f32 = 11.0;
const DIVIDER_WIDTH: f32 = 1.0;
/// The overview ruler's width, its left border included.
const RULER_WIDTH: f32 = 10.0;
/// How far a ruler marker sits in from the ruler's sides.
const RULER_MARK_INSET: f32 = 2.0;
/// A ruler marker's least height, so a one-row hunk in a long diff shows.
const RULER_MARK_MIN_HEIGHT: f32 = 2.0;
const RULER_MARK_RADIUS: f32 = 2.0;

/// What a hunk changes, as its overview-ruler marker shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HunkKind {
    /// Lines both removed and added.
    Mixed,
    /// Only added lines.
    Insert,
    /// Only removed lines.
    Delete,
}

impl HunkKind {
    /// The kind of a hunk made of `rows`; `None` when none of them changes.
    fn of(rows: &[Row]) -> Option<Self> {
        let removes = rows
            .iter()
            .any(|row| matches!(row.kind, RowKind::Delete | RowKind::Modify));
        let adds = rows
            .iter()
            .any(|row| matches!(row.kind, RowKind::Insert | RowKind::Modify));
        match (removes, adds) {
            (true, true) => Some(Self::Mixed),
            (false, true) => Some(Self::Insert),
            (true, false) => Some(Self::Delete),
            (false, false) => None,
        }
    }

    /// The marker's colour, as `0xRRGGBB`.
    const fn color(self) -> u32 {
        match self {
            Self::Mixed => OCHRE,
            Self::Insert => WAITING,
            Self::Delete => DIFF_DELETE,
        }
    }
}

/// One hunk's marker on the overview ruler, in pixels from the ruler's top.
#[derive(Debug, Clone, Copy, PartialEq)]
struct RulerMark {
    top: f32,
    height: f32,
    kind: HunkKind,
}

/// The overview ruler's markers for `model`'s hunks on a ruler `height`
/// pixels tall. Each sits at its first row's share of all the rows and is as
/// tall as its rows' share, but at least [`RULER_MARK_MIN_HEIGHT`] and never
/// past the ruler's foot. None for an empty diff or a ruler not yet laid out.
fn ruler_marks(model: &DiffModel, height: f32) -> Vec<RulerMark> {
    let rows = model.rows();
    if rows.is_empty() || height <= 0.0 {
        return Vec::new();
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "a diff's row count is far below f32's exact range"
    )]
    let row_height = height / rows.len() as f32;
    model
        .hunks()
        .iter()
        .filter_map(|hunk| {
            let kind = HunkKind::of(rows.get(hunk.clone())?)?;
            #[expect(
                clippy::cast_precision_loss,
                reason = "a diff's row count is far below f32's exact range"
            )]
            let (start, len) = (hunk.start as f32, hunk.len() as f32);
            let mark_height = (len * row_height).max(RULER_MARK_MIN_HEIGHT);
            let top = (start * row_height).min(height - mark_height).max(0.0);
            Some(RulerMark {
                top,
                height: mark_height,
                kind,
            })
        })
        .collect()
}

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
    /// The advance of one digit of a gutter's line number, drawn in the
    /// same family at [`GUTTER_TEXT_SIZE`].
    gutter_digit_width: f32,
}

/// The width of a half's line-number gutter for line numbers of `digits`
/// digits: room for them in the gutter's text and the pad after them, but
/// at least [`GUTTER_MIN_WIDTH`].
fn gutter_width(digits: usize, metrics: &Metrics) -> f32 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "a line number has a handful of digits"
    )]
    let digits = digits as f32;
    (digits * metrics.gutter_digit_width + GUTTER_PAD).max(GUTTER_MIN_WIDTH)
}

/// Which side of a row a half draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Half {
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
    /// The overview ruler's height, in pixels, recorded at paint.
    ruler_height: Rc<Cell<f32>>,
    /// The digits of the largest line number.
    gutter_digits: usize,
    /// The characters of the longest line on either side.
    longest_line: usize,
    /// Each side's syntax classes by line, when they have landed.
    old_syntax: Option<Arc<Highlighted>>,
    new_syntax: Option<Arc<Highlighted>>,
    /// Each token class's colour, in [`TokenClass::ALL`] order.
    class_colors: [Hsla; 6],
}

impl DiffView {
    /// A view of `model` in `font`, the app's terminal font.
    pub fn new(model: DiffModel, font: FontSettings, cx: &mut Context<Self>) -> Self {
        let (gutter_digits, longest_line) = measure(&model);
        let class_colors = TokenClass::ALL.map(|class| Hsla::from(rgb(class.color())));
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
            ruler_height: Rc::new(Cell::new(0.0)),
            gutter_digits,
            longest_line,
            old_syntax: None,
            new_syntax: None,
            class_colors,
        }
    }

    /// Colours the sides' lines by the syntax classes `old` and `new` give
    /// them, line 1 first; `None` leaves a side uncoloured. The scroll and
    /// the current hunk stay.
    pub fn set_syntax(
        &mut self,
        old: Option<Arc<Highlighted>>,
        new: Option<Arc<Highlighted>>,
        cx: &mut Context<Self>,
    ) {
        self.old_syntax = old;
        self.new_syntax = new;
        cx.notify();
    }

    /// The syntax colours of row `row`'s `half`: byte ranges into its whole
    /// line and the colour each is drawn in. None for a filler half or an
    /// uncoloured side.
    #[must_use]
    pub fn syntax_colors(&self, row: usize, half: Half) -> Vec<(Range<usize>, Hsla)> {
        let Some(side) = self
            .model
            .rows()
            .get(row)
            .and_then(|row| side_of(row, half))
        else {
            return Vec::new();
        };
        self.line_syntax(side, half)
            .iter()
            .map(|(range, class)| (range.clone(), self.class_colors[class.index()]))
            .collect()
    }

    /// The syntax classes of `side`'s line on `half`.
    fn line_syntax(&self, side: &Side, half: Half) -> &[(Range<usize>, TokenClass)] {
        let lines = match half {
            Half::Old => self.old_syntax.as_deref(),
            Half::New => self.new_syntax.as_deref(),
        };
        usize::try_from(side.line_no)
            .ok()
            .and_then(|line_no| line_no.checked_sub(1))
            .and_then(|index| lines?.get(index))
            .map_or(&[], Vec::as_slice)
    }

    /// Shows `model` in place of the current one. The row at the top stays
    /// at the top, clamped to the new rows, and no hunk is current.
    pub fn set_model(&mut self, model: DiffModel, cx: &mut Context<Self>) {
        let top = self.top_row();
        (self.gutter_digits, self.longest_line) = measure(&model);
        self.model = model;
        self.current = None;
        if let Some(last) = self.model.rows().len().checked_sub(1) {
            self.scroll
                .scroll_to_item_strict(top.min(last), ScrollStrategy::Top);
        }
        cx.notify();
    }

    /// The row at the top of the viewport.
    fn top_row(&self) -> usize {
        let line_height = self
            .metrics
            .as_ref()
            .map_or(16.0, |m| m.line_height)
            .max(1.0);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a non-negative row index far below usize::MAX"
        )]
        let row = (self.scroll_top() / line_height).floor().max(0.0) as usize;
        row
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

    /// The width of a half's line-number gutter, for the largest line
    /// number; see [`gutter_width`].
    fn gutter_width(&self, metrics: &Metrics) -> f32 {
        gutter_width(self.gutter_digits, metrics)
    }

    /// The overview ruler down the right edge, a marker for each hunk.
    fn ruler(&self) -> Div {
        let marks = ruler_marks(&self.model, self.ruler_height.get());
        div()
            .debug_selector(|| "diff-ruler".to_owned())
            .relative()
            .flex_none()
            .w(px(RULER_WIDTH))
            .h_full()
            .border_l_1()
            .border_color(rgb(LINE))
            .children(marks.into_iter().enumerate().map(|(index, mark)| {
                div()
                    .debug_selector(move || format!("diff-ruler-mark-{index}"))
                    .absolute()
                    .left(px(RULER_MARK_INSET))
                    .right(px(RULER_MARK_INSET))
                    .top(px(mark.top))
                    .h(px(mark.height))
                    .rounded(px(RULER_MARK_RADIUS))
                    .bg(rgb(mark.kind.color()))
            }))
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
        let gutter_digit_width = text
            .advance(font_id, px(GUTTER_TEXT_SIZE), '0')
            .map_or(GUTTER_TEXT_SIZE * 0.6, |s| s.width / px(1.0));
        let ascent = text.ascent(font_id, size) / px(1.0);
        let descent = text.descent(font_id, size) / px(1.0);
        let metrics = Metrics {
            family,
            char_width,
            line_height: fonts::line_height(ascent, descent, self.font.size),
            gutter_digit_width,
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
                            .when(current, |bar| bar.bg(rgb(ACCENT))),
                    )
                    .child(old)
                    .child(
                        div()
                            .w(px(DIVIDER_WIDTH))
                            .h_full()
                            .flex_none()
                            .bg(rgb(LINE)),
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
        let Some(side) = side_of(row, half) else {
            return (cell.bg(wash(DIFF_FILLER, DIFF_FILLER_ALPHA)), None);
        };
        let changed = row.kind != RowKind::Equal;
        let (line_text, hue, line_alpha) = match half {
            Half::Old => (
                self.model.old_text(side),
                DIFF_DELETE,
                DIFF_DELETE_WASH_ALPHA,
            ),
            Half::New => (self.model.new_text(side), WAITING, DIFF_INSERT_WASH_ALPHA),
        };
        let words = self.model.inline(index).map(|spans| match half {
            Half::Old => spans.left.as_slice(),
            Half::New => spans.right.as_slice(),
        });
        let shown = visible_range(line_text, columns.start, columns.len);
        let word_style = HighlightStyle {
            background_color: Some(Hsla::from(wash(hue, DIFF_WORD_WASH_ALPHA))),
            ..HighlightStyle::default()
        };
        let words = words
            .unwrap_or_default()
            .iter()
            .filter_map(|span| clip(line_text, &shown, span))
            .map(|range| (range, word_style));
        let classes = self
            .line_syntax(side, half)
            .iter()
            .filter_map(|(span, class)| {
                let style = HighlightStyle {
                    color: Some(self.class_colors[class.index()]),
                    ..HighlightStyle::default()
                };
                Some((clip(line_text, &shown, span)?, style))
            })
            .collect::<Vec<_>>();
        let text = SharedString::from(line_text.get(shown.clone()).unwrap_or_default().to_owned());
        let line =
            StyledText::new(text.clone()).with_highlights(combine_highlights(classes, words));
        let cell = cell
            .when(changed, |cell| cell.bg(wash(hue, line_alpha)))
            .child(
                div()
                    .w(px(self.gutter_width(metrics)))
                    .h_full()
                    .flex_none()
                    .pr(px(GUTTER_PAD))
                    .text_right()
                    .text_size(px(GUTTER_TEXT_SIZE))
                    .text_color(rgb(SUBTLE))
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

/// The digits of `model`'s largest line number, and the characters of its
/// longest line on either side.
fn measure(model: &DiffModel) -> (usize, usize) {
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
    (largest.max(1).to_string().len(), longest)
}

/// The side of `row` that `half` draws; `None` for a filler.
fn side_of(row: &Row, half: Half) -> Option<&Side> {
    match half {
        Half::Old => row.left.as_ref(),
        Half::New => row.right.as_ref(),
    }
}

/// The bytes of `text` that its `len` characters from character `start`
/// cover, on character boundaries.
fn visible_range(text: &str, start: usize, len: usize) -> Range<usize> {
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
    from..to
}

/// `span`, a byte range into `text`, clipped to `shown` and rebased onto
/// it; `None` when it lies wholly outside. The result stays on character
/// boundaries, widened to them when `span` is not.
fn clip(text: &str, shown: &Range<usize>, span: &Range<usize>) -> Option<Range<usize>> {
    let mut span_start = span.start.max(shown.start);
    let mut span_end = span.end.min(shown.end);
    if span_start >= span_end {
        return None;
    }
    while !text.is_char_boundary(span_start) {
        span_start -= 1;
    }
    while !text.is_char_boundary(span_end) {
        span_end += 1;
    }
    Some(span_start - shown.start..span_end - shown.start)
}

/// The `0xRRGGBB` `color` at `alpha`, as a gpui colour.
fn wash(color: u32, alpha: f32) -> gpui::Rgba {
    gpui::Rgba {
        a: alpha,
        ..rgb(color)
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
        let ruler_height = self.ruler_height.clone();
        // Registered in the capture phase, so a Shift+wheel reaches this
        // view before the list turns it into a vertical scroll. Painting it
        // records the width of a half's text column, which bounds how far
        // the lines scroll sideways, and the ruler's height, which places
        // its markers; a change to either redraws the view, so its render
        // clamps the scroll and moves the markers. gpui drops a notify sent
        // while a frame is drawn, so the redraw is deferred past it.
        let wheel = canvas(
            |_, _, _| {},
            move |bounds, (), window, cx| {
                let fixed = CURRENT_BAR_WIDTH + DIVIDER_WIDTH + RULER_WIDTH;
                let half = (bounds.size.width / px(1.0) - fixed) / 2.0;
                let width = (half - gutter).max(0.0);
                let height = bounds.size.height / px(1.0);
                let resized = (column_width.get() - width).abs() > f32::EPSILON
                    || (ruler_height.get() - height).abs() > f32::EPSILON;
                if resized {
                    column_width.set(width);
                    ruler_height.set(height);
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
            .flex()
            .flex_row()
            .bg(rgb(TERMINAL_GROUND))
            .text_color(rgb(TEXT))
            .font_family(metrics.family)
            .text_size(px(self.font.size))
            .line_height(px(metrics.line_height))
            .child(div().flex_1().min_w(px(0.0)).h_full().child(list))
            .child(self.ruler())
            .child(wheel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mono font's metrics at `size` px: a 0.6 em advance, as the
    /// gutter's text has at [`GUTTER_TEXT_SIZE`].
    fn mono(size: f32) -> Metrics {
        Metrics {
            family: SharedString::from(fonts::DEFAULT_FAMILY),
            char_width: size * 0.6,
            line_height: size * 1.3,
            gutter_digit_width: GUTTER_TEXT_SIZE * 0.6,
        }
    }

    #[test]
    fn the_gutter_fits_its_digits_at_the_gutter_size_under_a_small_terminal_font() {
        let small = mono(10.0);
        let needed = 6.0 * small.gutter_digit_width + GUTTER_PAD;
        let width = gutter_width(6, &small);
        assert!(
            width >= needed,
            "six 11 px digits and the pad need {needed}, got {width}"
        );
    }

    #[test]
    fn the_gutter_follows_its_own_text_not_the_terminal_font() {
        assert!(
            (gutter_width(3, &mono(24.0)) - GUTTER_MIN_WIDTH).abs() < f32::EPSILON,
            "a large terminal font leaves three digits at the least width"
        );
        assert!((gutter_width(1, &mono(10.0)) - GUTTER_MIN_WIDTH).abs() < f32::EPSILON);
    }

    fn slice(
        text: &str,
        spans: &[Range<usize>],
        start: usize,
        len: usize,
    ) -> (String, Vec<Range<usize>>) {
        let shown = visible_range(text, start, len);
        let spans = spans
            .iter()
            .filter_map(|span| clip(text, &shown, span))
            .collect();
        (text[shown].to_owned(), spans)
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

    fn model(old: &str, new: &str) -> DiffModel {
        DiffModel::build(old, new, crate::diff_model::DiffOptions::default())
    }

    /// Each marker's (top, height, kind), with the pixels rounded to tenths.
    fn marks(model: &DiffModel, height: f32) -> Vec<(f32, f32, HunkKind)> {
        ruler_marks(model, height)
            .into_iter()
            .map(|mark| {
                let tenths = |value: f32| (value * 10.0).round() / 10.0;
                (tenths(mark.top), tenths(mark.height), mark.kind)
            })
            .collect()
    }

    #[test]
    fn ruler_marks_sit_at_each_hunks_share_of_the_rows() {
        // Rows: a, b|B, c, d, e|-, f, g, -|h: eight rows, 10 px each.
        let diff = model("a\nb\nc\nd\ne\nf\ng\n", "a\nB\nc\nd\nf\ng\nh\n");
        assert_eq!(diff.rows().len(), 8, "{:?}", diff.rows());
        assert_eq!(
            marks(&diff, 80.0),
            [
                (10.0, 10.0, HunkKind::Mixed),
                (40.0, 10.0, HunkKind::Delete),
                (70.0, 10.0, HunkKind::Insert),
            ]
        );
    }

    #[test]
    fn a_hunk_of_several_rows_is_as_tall_as_its_share() {
        let diff = model("a\nb\nc\nd\n", "a\nb\nc\nd\nx\ny\nz\nw\n");
        assert_eq!(marks(&diff, 160.0), [(80.0, 80.0, HunkKind::Insert)]);
    }

    #[test]
    fn a_tiny_hunk_keeps_the_least_height_inside_the_ruler() {
        let lines: Vec<String> = (0..1000).map(|i| format!("line {i}")).collect();
        let old = lines.join("\n") + "\n";
        let new = format!("{old}added\n");
        let diff = model(&old, &new);
        let shown = marks(&diff, 100.0);
        assert_eq!(
            shown,
            [(98.0, 2.0, HunkKind::Insert)],
            "at the foot, not past it"
        );
        let first = format!(
            "changed\n{}",
            old.split_once('\n').map_or("", |(_, rest)| rest)
        );
        assert_eq!(
            marks(&model(&old, &first), 100.0),
            [(0.0, 2.0, HunkKind::Mixed)]
        );
    }

    #[test]
    fn no_marks_for_an_empty_diff_or_an_unlaid_ruler() {
        assert!(ruler_marks(&model("", ""), 100.0).is_empty());
        assert!(ruler_marks(&model("a\n", "b\n"), 0.0).is_empty());
        assert!(
            ruler_marks(&model("a\n", "a\n"), 100.0).is_empty(),
            "no hunks"
        );
    }

    #[test]
    fn a_zero_offset_leaves_a_short_line_as_it_is() {
        assert_eq!(
            slice("abc", &one(1..2), 0, 80),
            ("abc".to_owned(), one(1..2))
        );
    }
}
