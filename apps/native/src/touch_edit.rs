//! The rules a text field's touch editing follows (`TextInput`'s
//! `with_touch_editing`): the word a press selects, where a dragged
//! selection handle leaves the selection, which items the edit bar offers
//! and where the bar sits. Plain data in and out; the field only reads them.

use std::ops::Range;

use gpui::{Pixels, Point, Size, point, px};

use crate::chat_select::word_at;
use crate::text_input::{next_boundary, previous_boundary};

/// How near the caret, in px, a touch must start to drag it or, as a tap,
/// to open the edit bar.
pub(crate) const CARET_REACH: f32 = 22.0;

/// The gap, in px, between the edit bar and the selection's row: clear of
/// a selection handle's 10 px knob.
pub(crate) const BAR_GAP: f32 = 14.0;

/// The word under byte `offset` of `text`, by the chat selection's word
/// rule ([`word_at`]): letters, digits and underscores, with an inner
/// hyphen or apostrophe kept, or a lone punctuation character; none on
/// white space or at or past the text's end.
pub(crate) fn word_range(text: &str, offset: usize) -> Option<Range<usize>> {
    if offset >= text.len() {
        return None;
    }
    let word = word_at(text, offset);
    let blank = text.get(word.clone())?.chars().all(char::is_whitespace);
    (!word.is_empty() && !blank).then_some(word)
}

/// The selection a handle drag leaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Dragged {
    /// The selected byte range, never empty while the text is not.
    pub(crate) range: Range<usize>,
    /// Whether the dragged end is the range's start.
    pub(crate) reversed: bool,
}

/// `offset` inside `text` and on a character boundary.
fn char_floor(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// The selection once the dragged end moves to byte `to`, the other end
/// staying at `fixed`; both clamped into `text`. Dragged past the fixed
/// end, the ends swap. Dragged onto it, the dragged end stays one
/// character away on the side it came from (`after`: it sat after the
/// fixed end), or on the other side when the text ends there, so the
/// selection never empties.
pub(crate) fn drag_handle(text: &str, fixed: usize, to: usize, after: bool) -> Dragged {
    let fixed = char_floor(text, fixed);
    let mut to = char_floor(text, to);
    if to == fixed {
        let next = next_boundary(text, fixed);
        let previous = previous_boundary(text, fixed);
        let previous = if previous < fixed {
            Some(previous)
        } else {
            None
        };
        let next = if next > fixed { Some(next) } else { None };
        to = if after {
            next.or(previous)
        } else {
            previous.or(next)
        }
        .unwrap_or(fixed);
    }
    if to >= fixed {
        Dragged {
            range: fixed..to,
            reversed: false,
        }
    } else {
        Dragged {
            range: to..fixed,
            reversed: true,
        }
    }
}

/// One button of the edit bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BarItem {
    Cut,
    Copy,
    Paste,
    SelectAll,
}

impl BarItem {
    /// The button's label.
    pub(crate) fn label(self) -> &'static str {
        match self {
            BarItem::Cut => "Cut",
            BarItem::Copy => "Copy",
            BarItem::Paste => "Paste",
            BarItem::SelectAll => "Select All",
        }
    }

    /// The button's driver id.
    pub(crate) fn selector(self) -> &'static str {
        match self {
            BarItem::Cut => "text-edit-cut",
            BarItem::Copy => "text-edit-copy",
            BarItem::Paste => "text-edit-paste",
            BarItem::SelectAll => "text-edit-select-all",
        }
    }
}

/// The edit bar's buttons, in order, for a field holding `len` bytes with
/// `selection` selected: Cut and Copy with a non-empty selection, Paste
/// always (the clipboard is never read to decide), Select All unless the
/// field is empty or all of it is selected.
pub(crate) fn bar_items(len: usize, selection: &Range<usize>) -> Vec<BarItem> {
    let mut items = Vec::new();
    if !selection.is_empty() {
        items.extend([BarItem::Cut, BarItem::Copy]);
    }
    items.push(BarItem::Paste);
    if len > 0 && *selection != (0..len) {
        items.push(BarItem::SelectAll);
    }
    items
}

/// Where the selection the edit bar serves draws, in window px: the top of
/// its first row, the bottom of its last row and the x the bar centres on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SelectionBox {
    pub(crate) top: Pixels,
    pub(crate) bottom: Pixels,
    pub(crate) x: Pixels,
}

/// The top-left of an edit bar of size `bar` in a window of size `window`
/// whose keyboard covers its bottom `keyboard` px, serving `selection`:
/// [`BAR_GAP`] above its top row when the bar fits there clear of the
/// keyboard, else [`BAR_GAP`] below its bottom row, its top never past
/// where the bar still fits above the keyboard (nor above the window's
/// top); with neither spot clear of the keyboard the bar takes the lowest
/// top that is, or the window's top when none is; centred on its x and
/// kept inside the window's width.
pub(crate) fn bar_origin(
    selection: SelectionBox,
    bar: Size<Pixels>,
    window: Size<Pixels>,
    keyboard: Pixels,
) -> Point<Pixels> {
    let unit = px(1.0);
    let (width, height) = (bar.width / unit, bar.height / unit);
    let right = (window.width / unit - width).max(0.0);
    let x = (selection.x / unit - width / 2.0).clamp(0.0, right);
    // The lowest top the bar takes while clear of the keyboard.
    let lowest = (window.height / unit - keyboard / unit - height).max(0.0);
    let above = selection.top / unit - BAR_GAP - height;
    let below = selection.bottom / unit + BAR_GAP;
    let y = if above >= 0.0 && above <= lowest {
        above
    } else if below <= lowest {
        below
    } else {
        lowest
    };
    point(px(x), px(y))
}

/// Whether window point `at` lies within [`CARET_REACH`] of a caret whose
/// stem runs down from `caret` for `line_height`.
pub(crate) fn near_caret(caret: Point<Pixels>, line_height: Pixels, at: Point<Pixels>) -> bool {
    let unit = px(1.0);
    let dx = (at.x - caret.x) / unit;
    let (top, bottom) = (caret.y / unit, (caret.y + line_height) / unit);
    let y = at.y / unit;
    let dy = if y < top {
        top - y
    } else if y > bottom {
        y - bottom
    } else {
        0.0
    };
    dx.hypot(dy) <= CARET_REACH
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::size;

    #[test]
    fn word_range_picks_an_ascii_word() {
        assert_eq!(word_range("hello world", 1), Some(0..5));
        assert_eq!(word_range("hello world", 8), Some(6..11));
    }

    #[test]
    fn word_range_keeps_an_inner_apostrophe() {
        assert_eq!(word_range("we don't stop", 4), Some(3..8));
        assert_eq!(
            word_range("we don't stop", 6),
            Some(3..8),
            "on the apostrophe"
        );
    }

    #[test]
    fn word_range_on_punctuation_is_that_character() {
        assert_eq!(word_range("hi, there", 2), Some(2..3));
    }

    #[test]
    fn word_range_on_a_space_between_words_is_none() {
        assert_eq!(word_range("hello world", 5), None);
        assert_eq!(word_range("hello world", 11), None, "the end");
        assert_eq!(word_range("", 0), None, "an empty text");
    }

    #[test]
    fn word_range_holds_multi_byte_words_whole() {
        assert_eq!(word_range("café olé", 4), Some(0..5), "inside é");
        assert_eq!(word_range("café olé", 7), Some(6..10));
        assert_eq!(word_range("日本 語", 3), Some(0..6));
    }

    #[test]
    fn a_dragged_end_extends_and_shrinks_the_selection() {
        let text = "hello brave world";
        assert_eq!(
            drag_handle(text, 6, 17, true),
            Dragged {
                range: 6..17,
                reversed: false
            },
            "the end extends"
        );
        assert_eq!(
            drag_handle(text, 11, 8, true),
            Dragged {
                range: 8..11,
                reversed: true
            },
            "the start moves in"
        );
        assert_eq!(
            drag_handle(text, 6, 8, true),
            Dragged {
                range: 6..8,
                reversed: false
            },
            "the end shrinks"
        );
    }

    #[test]
    fn a_handle_dragged_past_the_other_end_swaps_the_ends() {
        assert_eq!(
            drag_handle("hello brave world", 6, 2, true),
            Dragged {
                range: 2..6,
                reversed: true
            }
        );
    }

    #[test]
    fn a_handle_dragged_onto_the_other_end_keeps_one_character() {
        assert_eq!(drag_handle("abc", 1, 1, true).range, 1..2, "from after");
        assert_eq!(drag_handle("abc", 1, 1, false).range, 0..1, "from before");
        assert_eq!(
            drag_handle("abc", 3, 3, true).range,
            2..3,
            "no character after the text's end"
        );
        assert_eq!(
            drag_handle("é", 0, 0, true).range,
            0..2,
            "a whole character"
        );
    }

    #[test]
    fn a_handle_drag_clamps_to_the_text() {
        assert_eq!(drag_handle("abc", 1, 99, true).range, 1..3);
        assert_eq!(drag_handle("é!", 2, 1, true).range, 0..2, "a char boundary");
        assert_eq!(
            drag_handle("abc", 99, 0, true).range,
            0..3,
            "the fixed end too"
        );
    }

    #[test]
    fn the_bar_offers_cut_copy_paste_and_select_all_for_a_part_selected() {
        assert_eq!(
            bar_items(11, &(0..5)),
            [
                BarItem::Cut,
                BarItem::Copy,
                BarItem::Paste,
                BarItem::SelectAll
            ]
        );
    }

    #[test]
    fn the_bar_offers_paste_and_select_all_for_a_caret() {
        assert_eq!(bar_items(11, &(3..3)), [BarItem::Paste, BarItem::SelectAll]);
    }

    #[test]
    fn the_bar_offers_only_paste_in_an_empty_field() {
        assert_eq!(bar_items(0, &(0..0)), [BarItem::Paste]);
    }

    #[test]
    fn the_bar_drops_select_all_when_all_is_selected() {
        assert_eq!(
            bar_items(11, &(0..11)),
            [BarItem::Cut, BarItem::Copy, BarItem::Paste]
        );
    }

    #[test]
    fn the_bar_labels_and_selectors() {
        let all = [
            BarItem::Cut,
            BarItem::Copy,
            BarItem::Paste,
            BarItem::SelectAll,
        ];
        let labels: Vec<_> = all.iter().map(|item| item.label()).collect();
        let selectors: Vec<_> = all.iter().map(|item| item.selector()).collect();
        assert_eq!(labels, ["Cut", "Copy", "Paste", "Select All"]);
        assert_eq!(
            selectors,
            [
                "text-edit-cut",
                "text-edit-copy",
                "text-edit-paste",
                "text-edit-select-all"
            ]
        );
    }

    fn sel(top: f32, bottom: f32, x: f32) -> SelectionBox {
        SelectionBox {
            top: px(top),
            bottom: px(bottom),
            x: px(x),
        }
    }

    #[test]
    fn the_bar_sits_above_the_selection_when_it_fits() {
        let origin = bar_origin(
            sel(300.0, 320.0, 200.0),
            size(px(100.0), px(50.0)),
            size(px(400.0), px(800.0)),
            px(0.0),
        );
        assert_eq!(origin, point(px(150.0), px(300.0 - BAR_GAP - 50.0)));
    }

    #[test]
    fn the_bar_sits_below_the_selection_without_room_above() {
        let origin = bar_origin(
            sel(30.0, 50.0, 200.0),
            size(px(100.0), px(50.0)),
            size(px(400.0), px(800.0)),
            px(0.0),
        );
        assert_eq!(origin, point(px(150.0), px(50.0 + BAR_GAP)));
        let low = bar_origin(
            sel(30.0, 790.0, 200.0),
            size(px(100.0), px(50.0)),
            size(px(400.0), px(800.0)),
            px(0.0),
        );
        assert_eq!(low.y, px(750.0), "kept above the window's bottom");
    }

    #[test]
    fn the_edit_bar_stays_above_the_keyboard() {
        let window = size(px(400.0), px(800.0));
        let bar = size(px(100.0), px(50.0));
        // A selection low in the window whose bar has no room above it, and
        // no room below it above a keyboard covering the window's bottom 300.
        let origin = bar_origin(sel(20.0, 760.0, 200.0), bar, window, px(300.0));
        assert!(
            origin.y + bar.height <= px(500.0),
            "clear of the keyboard: {origin:?}"
        );
        // Room below the selection above the keyboard: there.
        let below = bar_origin(sel(20.0, 40.0, 200.0), bar, window, px(300.0));
        assert_eq!(below.y, px(40.0 + BAR_GAP));
        // No keyboard: the window's bottom bounds the bar, as ever.
        let low = bar_origin(sel(30.0, 790.0, 200.0), bar, window, px(0.0));
        assert_eq!(low.y, px(750.0));
    }

    #[test]
    fn the_edit_bar_takes_the_lowest_clear_spot_when_both_reach_under_the_keyboard() {
        let window = size(px(400.0), px(800.0));
        let bar = size(px(100.0), px(50.0));
        // A keyboard over the bottom 300 leaves 450 as the lowest clear top;
        // the spot above a selection at 600..620 and the spot below it both
        // reach under the keyboard.
        let origin = bar_origin(sel(600.0, 620.0, 200.0), bar, window, px(300.0));
        assert_eq!(origin, point(px(150.0), px(450.0)));
    }

    #[test]
    fn the_bar_is_clamped_inside_the_window_width() {
        let window = size(px(400.0), px(800.0));
        let bar = size(px(200.0), px(50.0));
        assert_eq!(
            bar_origin(sel(300.0, 320.0, 10.0), bar, window, px(0.0)).x,
            px(0.0)
        );
        assert_eq!(
            bar_origin(sel(300.0, 320.0, 390.0), bar, window, px(0.0)).x,
            px(200.0)
        );
        let wide = size(px(500.0), px(50.0));
        assert_eq!(
            bar_origin(sel(300.0, 320.0, 200.0), wide, window, px(0.0)).x,
            px(0.0),
            "a bar wider than the window starts at its left"
        );
    }

    #[test]
    fn a_point_near_the_caret_is_within_reach() {
        let caret = point(px(100.0), px(50.0));
        let lh = px(20.0);
        assert!(near_caret(caret, lh, point(px(100.0), px(60.0))), "on it");
        assert!(
            near_caret(caret, lh, point(px(122.0), px(60.0))),
            "at reach"
        );
        assert!(
            !near_caret(caret, lh, point(px(123.0), px(60.0))),
            "past reach"
        );
        assert!(!near_caret(caret, lh, point(px(100.0), px(93.0))), "below");
    }
}
