//! How the phone's terminal fits above a raised keyboard: the terminal
//! keeps its keyboard-down height, so its rows never change, and the page
//! shows its bottom part above the keyboard; a vertical drag pans up to the
//! rows the keyboard hides. Plain points, no layout types, so the rules test
//! without a window.

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

/// The least pan that shows the cursor's row above the raised keyboard.
///
/// Args:
///     hidden: The hidden height in points.
///     `row_top`: The top of the cursor's row, in points from the
///         terminal's top.
///
/// Returns:
///     `hidden - row_top`, kept inside `[0, hidden]`.
pub(super) fn cursor_need(hidden: f32, row_top: f32) -> f32 {
    (hidden - row_top).clamp(0.0, hidden.max(0.0))
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
    /// needs, so a drag can show more but never pans the cursor away.
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

    #[test]
    fn a_cursor_on_the_bottom_row_needs_no_pan() {
        let need = cursor_need(HIDDEN, 600.0);
        assert!(need.abs() < f32::EPSILON, "{need}");
    }

    #[test]
    fn a_cursor_on_row_zero_needs_the_whole_hidden_height() {
        let need = cursor_need(HIDDEN, 0.0);
        assert!((need - HIDDEN).abs() < f32::EPSILON, "{need}");
    }

    #[test]
    fn a_cursor_in_the_hidden_band_needs_the_rest_of_it() {
        let need = cursor_need(HIDDEN, 102.0);
        assert!((need - 200.0).abs() < f32::EPSILON, "{need}");
    }

    #[test]
    fn a_cursor_needs_nothing_with_the_keyboard_down() {
        let need = cursor_need(0.0, 0.0);
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
