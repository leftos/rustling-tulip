//! How the phone's terminal fits its clip box: the terminal keeps its rest
//! height, the clip box's height with the keyboard down, the Quick dock
//! closed and no Show prompt, so its rows never change; the box shows its
//! bottom part, the cursor's row kept in sight, and a vertical drag pans up
//! to the rows the box hides. Plain points, no layout types, so the rules
//! test without a window.

/// The height of the terminal the raised keyboard covers: how far the
/// keyboard reaches above the safe area's bottom, none while it is down.
///
/// Args:
///     `safe_bottom`: The safe area's bottom inset, in points.
///     `ime_bottom`: The on-screen keyboard's bottom inset, in points.
///
/// Returns:
///     The covered height in points, never below zero.
pub(super) fn hidden_height(safe_bottom: f32, ime_bottom: f32) -> f32 {
    (ime_bottom - safe_bottom).max(0.0)
}

/// The height of the terminal its clip box cannot show: how much shorter
/// the box is than the terminal's rest height.
///
/// Args:
///     rest: The terminal's rest height, in points.
///     clip: The clip box's measured height, in points.
///
/// Returns:
///     `rest - clip`, never below zero.
pub(super) fn hidden_below(rest: f32, clip: f32) -> f32 {
    (rest - clip).max(0.0)
}

/// The terminal's rest height worked out from a frame not at rest: the
/// clip box's height and back what took room from it.
///
/// Args:
///     clip: The clip box's measured height, in points.
///     `keyboard_cover`: The height the raised keyboard covers, from
///         [`hidden_height`].
///     dock: The Quick dock's measured height, zero while it is closed.
///     prompt: The Show prompt row's measured height, zero while it is
///         not drawn.
///
/// Returns:
///     The sum of the four, in points.
pub(super) fn seed_rest(clip: f32, keyboard_cover: f32, dock: f32, prompt: f32) -> f32 {
    clip + keyboard_cover + dock + prompt
}

/// The least pan that shows the cursor's row on the bottom row of the
/// window over the terminal, with the rows above it showing; a cursor in
/// the first rows shows the terminal from row 0.
///
/// Args:
///     hidden: The hidden height in points.
///     rest: The terminal's rest height, in points.
///     `row_bottom`: The bottom of the cursor's row, in points from the
///         terminal's top.
///
/// Returns:
///     `rest - row_bottom`, kept inside `[0, hidden]`.
pub(super) fn cursor_need(hidden: f32, rest: f32, row_bottom: f32) -> f32 {
    (rest - row_bottom).clamp(0.0, hidden.max(0.0))
}

/// How far the terminal is panned down to show rows the keyboard hides:
/// zero shows its bottom rows, the hidden height shows its top row.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Pan {
    /// The pan in points, from zero to the hidden height.
    pub(super) offset: f32,
}

impl Pan {
    /// This pan kept inside what the keyboard now hides.
    ///
    /// Args:
    ///     hidden: The hidden height in points.
    ///
    /// Returns:
    ///     The pan with its offset clamped to `[0, hidden]`.
    pub(super) fn clamped(self, hidden: f32) -> Self {
        Self {
            offset: self.offset.clamp(0.0, hidden.max(0.0)),
        }
    }

    /// The pan in effect: this drag pan, raised to what the cursor's row
    /// needs, so the rows above the cursor's row show; a larger drag pans
    /// on toward the top rows, which can take the cursor's row below the
    /// window.
    ///
    /// Args:
    ///     hidden: The hidden height in points.
    ///     need: The cursor's need, from [`cursor_need`].
    ///
    /// Returns:
    ///     `max(offset, need)` clamped to `[0, hidden]`.
    pub(super) fn effective(self, hidden: f32, need: f32) -> Self {
        Self {
            offset: self.offset.max(need),
        }
        .clamped(hidden)
    }

    /// A drag over the terminal with the cursor's need applied: it starts
    /// from the pan in effect, and is consumed only when it moves that.
    ///
    /// Args:
    ///     `delta_y`: The drag's vertical delta, signed as [`Pan::drag`]'s.
    ///     hidden: The hidden height in points.
    ///     need: The cursor's need, from [`cursor_need`].
    ///
    /// Returns:
    ///     The new drag pan, and whether the pan in effect moved.
    pub(super) fn drag_shown(self, delta_y: f32, hidden: f32, need: f32) -> (Self, bool) {
        let from = self.effective(hidden, need);
        let (to, _) = from.drag(delta_y, hidden);
        let moved = (to.effective(hidden, need).offset - from.offset).abs() > f32::EPSILON;
        if moved { (to, true) } else { (self, false) }
    }

    /// A vertical drag over the terminal: it pans while it can move the
    /// offset, and is left to the terminal's own scroll at either bound.
    ///
    /// Args:
    ///     `delta_y`: The drag's vertical delta in points, signed as GPUI's
    ///         `ScrollDelta` y that the terminal's own scroll reads: positive
    ///         when the content moves down (a finger dragging down, a wheel
    ///         turned up), which reveals the hidden top rows and grows the
    ///         offset.
    ///     hidden: The hidden height in points.
    ///
    /// Returns:
    ///     The new pan, and whether the drag moved it (consumed), so the
    ///     terminal does not also scroll.
    pub(super) fn drag(self, delta_y: f32, hidden: f32) -> (Self, bool) {
        let from = self.clamped(hidden);
        let to = Self {
            offset: from.offset + delta_y,
        }
        .clamped(hidden);
        let consumed = (to.offset - from.offset).abs() > f32::EPSILON;
        (to, consumed)
    }

    /// The terminal's top and bottom edges inside its clip box, so it keeps
    /// its keyboard-down height and shows the rows this pan picks.
    ///
    /// Args:
    ///     hidden: The hidden height in points.
    ///
    /// Returns:
    ///     `(top, bottom)` in points: `(-(hidden - offset), -offset)`.
    pub(super) fn edges(self, hidden: f32) -> (f32, f32) {
        let offset = self.clamped(hidden).offset;
        (-(hidden - offset), -offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAFE: f32 = 34.0;
    const KEYBOARD: f32 = 336.0;
    const HIDDEN: f32 = 302.0;

    #[test]
    fn keyboard_down_hides_nothing() {
        let hidden = hidden_height(SAFE, 0.0);
        assert!(hidden.abs() < f32::EPSILON, "{hidden}");
        assert_eq!(Pan::default().edges(hidden), (0.0, 0.0));
    }

    #[test]
    fn keyboard_up_shows_the_bottom_rows() {
        let hidden = hidden_height(SAFE, KEYBOARD);
        assert!((hidden - HIDDEN).abs() < f32::EPSILON, "{hidden}");
        assert_eq!(Pan::default().edges(hidden), (-HIDDEN, 0.0));
    }

    #[test]
    fn an_ime_below_the_safe_area_hides_nothing() {
        let hidden = hidden_height(SAFE, 20.0);
        assert!(hidden.abs() < f32::EPSILON, "{hidden}");
    }

    #[test]
    fn a_drag_reveals_the_top() {
        let (pan, consumed) = Pan::default().drag(100.0, HIDDEN);
        assert_eq!(pan, Pan { offset: 100.0 });
        assert!(consumed);
        assert_eq!(pan.edges(HIDDEN), (-202.0, -100.0));
    }

    #[test]
    fn a_drag_past_the_top_clamps_then_passes_on() {
        let (pan, consumed) = Pan { offset: 250.0 }.drag(100.0, HIDDEN);
        assert_eq!(pan, Pan { offset: HIDDEN });
        assert!(consumed, "it moved");

        let (pan, consumed) = pan.drag(50.0, HIDDEN);
        assert_eq!(pan, Pan { offset: HIDDEN });
        assert!(!consumed, "at the top the pane scrolls");
    }

    #[test]
    fn a_drag_back_down_at_the_bottom_passes_on() {
        let (pan, consumed) = Pan::default().drag(-40.0, HIDDEN);
        assert_eq!(pan, Pan::default());
        assert!(!consumed);
    }

    #[test]
    fn a_falling_keyboard_clamps_the_pan_to_zero() {
        assert_eq!(Pan { offset: 100.0 }.clamped(0.0), Pan::default());
    }

    const REST: f32 = 600.0;

    #[test]
    fn the_hidden_height_is_what_the_clip_box_lost() {
        assert!((hidden_below(REST, 298.0) - 302.0).abs() < f32::EPSILON);
    }

    #[test]
    fn a_clip_box_with_no_height_hides_the_whole_terminal() {
        assert!((hidden_below(REST, 0.0) - REST).abs() < f32::EPSILON);
    }

    #[test]
    fn a_clip_box_taller_than_the_rest_height_hides_nothing() {
        let hidden = hidden_below(500.0, 600.0);
        assert!(hidden.abs() < f32::EPSILON, "{hidden}");
    }

    #[test]
    fn the_seeded_rest_height_adds_back_what_took_the_room() {
        let rest = seed_rest(100.0, 302.0, 120.0, 60.0);
        assert!((rest - 582.0).abs() < f32::EPSILON, "{rest}");
    }

    #[test]
    fn a_cursor_on_the_bottom_row_needs_no_pan() {
        let need = cursor_need(HIDDEN, REST, 600.0);
        assert!(need.abs() < f32::EPSILON, "{need}");
    }

    #[test]
    fn a_cursor_in_the_first_rows_needs_the_whole_hidden_height() {
        let need = cursor_need(HIDDEN, REST, 20.0);
        assert!((need - HIDDEN).abs() < f32::EPSILON, "{need}");
    }

    #[test]
    fn a_cursor_in_the_middle_sits_on_the_window_bottom_row() {
        let need = cursor_need(HIDDEN, REST, 330.0);
        assert!((need - 270.0).abs() < f32::EPSILON, "{need}");
    }

    #[test]
    fn a_cursor_needs_nothing_with_the_keyboard_down() {
        let need = cursor_need(0.0, REST, 20.0);
        assert!(need.abs() < f32::EPSILON, "{need}");
    }

    #[test]
    fn the_cursor_need_raises_a_smaller_drag_pan() {
        let pan = Pan::default().effective(HIDDEN, 100.0);
        assert_eq!(pan, Pan { offset: 100.0 });
    }

    #[test]
    fn a_larger_drag_pan_beats_the_cursor_need() {
        let pan = Pan { offset: 200.0 }.effective(HIDDEN, 100.0);
        assert_eq!(pan, Pan { offset: 200.0 });
    }

    #[test]
    fn the_pan_in_effect_stays_inside_the_hidden_height() {
        let pan = Pan { offset: 400.0 }.effective(HIDDEN, 0.0);
        assert_eq!(pan, Pan { offset: HIDDEN });
    }

    #[test]
    fn a_drag_below_the_cursor_need_passes_on() {
        let (pan, consumed) = Pan::default().drag_shown(-40.0, HIDDEN, 100.0);
        assert_eq!(pan, Pan::default());
        assert!(!consumed, "the cursor holds the pan");

        let (pan, consumed) = Pan::default().drag_shown(40.0, HIDDEN, 100.0);
        assert_eq!(pan, Pan { offset: 140.0 });
        assert!(consumed, "from the pan in effect");
    }
}
