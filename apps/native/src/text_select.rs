//! Selectable text drawn by a GPUI view: each text a frame draws registered
//! with its layout, the hit test that maps a window point to a byte of one,
//! and the drawing that paints a selection's bytes on the highlight colour.
//! The phone's Chat and a followed run's or subagent's transcript share it.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    Bounds, HighlightStyle, Hsla, IntoElement, Pixels, Point, SharedString, Styled as _,
    StyledText, TextAlign, TextLayout, canvas, point, px,
};

use crate::chat_select::{self, SelectedSpan, TextPos, UnitSource, Units};
use crate::chat_view::HandleSpots;
use crate::viewer_md_view::select_highlights;

/// The selectable texts the phone's chat drew in its latest frame, each
/// with its layout, for the hit test, and the selected bytes it drew.
#[derive(Default)]
pub(crate) struct TextRegistry {
    /// Each drawn unit, in drawing order.
    pub(crate) texts: Vec<DrawnText>,
    /// Whether the frame that registered `texts` has painted, so that each
    /// layout has been measured and placed; until then none is read.
    pub(crate) painted: bool,
    /// Each unit's entry id, unit index and the bytes drawn selected.
    pub(crate) painted_ranges: Vec<(String, u32, Range<usize>)>,
    /// Where the selection's handles go, as the latest frame measured them;
    /// the next frame draws its handles there. [`Self::forget`] keeps them.
    pub(crate) handles: Option<HandleSpots>,
}

impl TextRegistry {
    /// Forgets the last frame's texts: before a new frame registers its
    /// own, and while the phone draws no chat, so no hit reads stale ones.
    pub(crate) fn forget(&mut self) {
        self.texts.clear();
        self.painted = false;
        self.painted_ranges.clear();
    }

    /// The first text whose bounds hold window point `at`, while it is
    /// inside `viewport`; `None` before the frame has painted.
    pub(crate) fn inside(
        &self,
        at: Point<Pixels>,
        viewport: Option<Bounds<Pixels>>,
    ) -> Option<&DrawnText> {
        if !self.painted || !viewport.is_none_or(|view| view.contains(&at)) {
            return None;
        }
        self.texts
            .iter()
            .find(|text| text.layout.bounds().contains(&at))
    }

    /// The (entry id, unit index, byte) of the glyph under window point
    /// `at`, only when it is on a text inside `viewport` and on one of its
    /// glyphs ([`glyph_at`]): not the nearest text, nor the blank beside a
    /// line; `None` before the frame has painted. A hold starts here; the
    /// held finger's moves read [`Self::hit`], which takes the nearest byte.
    pub(crate) fn hit_inside(
        &self,
        at: Point<Pixels>,
        viewport: Option<Bounds<Pixels>>,
    ) -> Option<(String, u32, usize)> {
        let text = self.inside(at, viewport)?;
        Some((text.entry.clone(), text.unit, glyph_at(text, at)?))
    }

    /// The (entry id, unit index, byte) window point `at` falls on: the
    /// first text whose bounds hold it, while it is inside `viewport` (the
    /// chat's scroll area), else the text nearest it vertically (the earlier
    /// on a tie), at byte 0 above that text, at its end below it; the byte
    /// at a character's start. `None` before the frame has painted or with
    /// no text drawn.
    pub(crate) fn hit(
        &self,
        at: Point<Pixels>,
        viewport: Option<Bounds<Pixels>>,
    ) -> Option<(String, u32, usize)> {
        if !self.painted {
            return None;
        }
        let text = match self.inside(at, viewport) {
            Some(text) => text,
            None => self.nearest(at)?,
        };
        Some((text.entry.clone(), text.unit, byte_at(text, at)))
    }

    /// The text nearest window point `at` vertically, the earlier on a tie.
    pub(crate) fn nearest(&self, at: Point<Pixels>) -> Option<&DrawnText> {
        let mut best: Option<(Pixels, &DrawnText)> = None;
        for text in &self.texts {
            let distance = y_distance(text.layout.bounds(), at.y);
            if best.is_none_or(|(least, _)| distance < least) {
                best = Some((distance, text));
            }
        }
        best.map(|(_, text)| text)
    }
}

/// How far `y` is above or below `bounds`; zero inside its rows.
fn y_distance(bounds: Bounds<Pixels>, y: Pixels) -> Pixels {
    if y < bounds.top() {
        bounds.top() - y
    } else if y > bounds.bottom() {
        y - bounds.bottom()
    } else {
        px(0.0)
    }
}

/// One selectable text a frame drew: its entry id, unit index, layout, and
/// how its element aligns it.
pub(crate) struct DrawnText {
    pub(crate) entry: String,
    pub(crate) unit: u32,
    pub(crate) layout: TextLayout,
    pub(crate) align: TextAlign,
}

/// The byte of `text` at window point `at`: 0 above it, its end below it,
/// else the character under or nearest the point, measured from where the
/// text's line is drawn; moved back to a character's start.
fn byte_at(text: &DrawnText, at: Point<Pixels>) -> usize {
    let layout = &text.layout;
    let bounds = layout.bounds();
    let drawn = layout.text();
    let byte = if at.y < bounds.top() {
        0
    } else if at.y > bounds.bottom() {
        drawn.len()
    } else {
        let shifted = point(at.x - align_shift(layout, text.align), at.y);
        match layout.index_for_position(shifted) {
            Ok(index) | Err(index) => index,
        }
    };
    chat_select::char_floor(&drawn, byte)
}

/// The byte of the glyph of `text` under window point `at`, moved back to
/// a character's start; `None` where the point is on no glyph: above or
/// below the text's lines, or in the blank beside a line (the layout's
/// position lookup answers `Err` there, which [`byte_at`] takes as the
/// nearest byte).
fn glyph_at(text: &DrawnText, at: Point<Pixels>) -> Option<usize> {
    let layout = &text.layout;
    let bounds = layout.bounds();
    if at.y < bounds.top() || at.y > bounds.bottom() {
        return None;
    }
    let shifted = point(at.x - align_shift(layout, text.align), at.y);
    let index = layout.index_for_position(shifted).ok()?;
    Some(chat_select::char_floor(&layout.text(), index))
}

/// How far right of its bounds' left edge a centred or right-aligned text
/// draws its line, which the layout's own position lookup does not apply;
/// zero for a left-aligned text and for one that takes more than one line,
/// which keeps the left-edge measure.
pub(crate) fn align_shift(layout: &TextLayout, align: TextAlign) -> Pixels {
    if matches!(align, TextAlign::Left) {
        return px(0.0);
    }
    let lines = layout.line_layouts();
    let [line] = lines.as_slice() else {
        return px(0.0);
    };
    if !line.wrap_boundaries.is_empty() {
        return px(0.0);
    }
    let room = (layout.bounds().size.width - line.unwrapped_layout.width).max(px(0.0));
    match align {
        TextAlign::Right => room,
        TextAlign::Center => room / 2.0,
        TextAlign::Left => px(0.0),
    }
}

/// What a phone chat's selectable texts are drawn with: its units, the
/// selection's ends as positions in the units, the highlight colour, and
/// the registry each drawn text's layout goes into.
#[derive(Clone)]
pub(crate) struct SelectDraw {
    pub(crate) units: Rc<Units>,
    pub(crate) selected: Option<SelectedSpan>,
    /// The selection's (start, end) in reading order, which the handles
    /// mark.
    pub(crate) ends: Option<(TextPos, TextPos)>,
    pub(crate) background: Hsla,
    pub(crate) registry: Rc<RefCell<TextRegistry>>,
}

impl SelectDraw {
    /// `text`, entry `entry`'s text drawn from `source` and aligned by
    /// `align`, styled with `highlights` and the selected bytes on the
    /// highlight colour; when it is a unit, its layout is registered and its
    /// selected bytes recorded.
    pub(crate) fn text(
        &self,
        (entry, source): (&str, UnitSource),
        align: TextAlign,
        text: SharedString,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
    ) -> StyledText {
        let Some((at, unit)) = self.units.locate(entry, source) else {
            return styled_text(text, highlights);
        };
        let selected = self
            .selected
            .as_ref()
            .and_then(|span| self.units.range_at(span, at));
        let mut registry = self.registry.borrow_mut();
        if let Some(range) = &selected {
            registry
                .painted_ranges
                .push((entry.to_owned(), unit, range.clone()));
        }
        let styled = styled_text(
            text,
            select_highlights(highlights, selected, self.background),
        );
        registry.texts.push(DrawnText {
            entry: entry.to_owned(),
            unit,
            layout: styled.layout().clone(),
            align,
        });
        styled
    }
}

/// `text` styled with `highlights`, none set when there are none.
fn styled_text(text: SharedString, highlights: Vec<(Range<usize>, HighlightStyle)>) -> StyledText {
    if highlights.is_empty() {
        StyledText::new(text)
    } else {
        StyledText::new(text).with_highlights(highlights)
    }
}

/// A chat text: through `select` when the pane draws selectable text
/// ([`SelectDraw::text`]), else `text` styled with `highlights`.
pub(crate) fn select_text(
    select: Option<&SelectDraw>,
    entry: &str,
    source: UnitSource,
    text: impl Into<SharedString>,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
) -> StyledText {
    match select {
        Some(select) => select.text((entry, source), TextAlign::Left, text.into(), highlights),
        None => styled_text(text.into(), highlights),
    }
}

/// A mark painted after the phone chat's rows, telling `registry` that
/// every text it holds has been laid out.
pub(crate) fn painted_mark(registry: Rc<RefCell<TextRegistry>>) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |_, (), _, _| registry.borrow_mut().painted = true,
    )
    .absolute()
    .size_0()
}
