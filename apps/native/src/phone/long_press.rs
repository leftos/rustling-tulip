//! A long press on an element, from gpui's own touch recogniser: a touch
//! held [`gpui::GestureTuning`]'s 500 ms within its 8 px slop. The area
//! claims the press on its start, so the lift is not a tap; a shorter
//! touch is still a tap, and a touch that travels is still a pan.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    App, Bounds, DispatchPhase, Hitbox, HitboxBehavior, IntoElement, LongPressEvent, Pixels, Point,
    Styled as _, TouchPhase, Window, canvas,
};

/// Whether a long press's `phase` event, seen in `dispatch`, is one this
/// area claims: its start, over the area, while no other listener has.
fn claims(phase: TouchPhase, dispatch: DispatchPhase, prevented: bool, hovered: bool) -> bool {
    phase == TouchPhase::Started && dispatch.bubble() && !prevented && hovered
}

/// An area over its parent's whole box (the parent is `relative`) that
/// runs `on_press` with where the press started when a long press starts
/// on it. Its hitbox blocks no other hitbox, so the parent's taps and the
/// list's scroll pass through.
pub(super) fn long_press_area(
    on_press: impl Fn(Point<Pixels>, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    canvas(
        |bounds: Bounds<Pixels>, window: &mut Window, _: &mut App| {
            window.insert_hitbox(bounds, HitboxBehavior::Normal)
        },
        move |_, hitbox: Hitbox, window: &mut Window, _: &mut App| {
            window.on_mouse_event(move |event: &LongPressEvent, phase, window, cx| {
                let prevented = window.default_prevented();
                if claims(event.phase, phase, prevented, hitbox.is_hovered(window)) {
                    window.prevent_default();
                    cx.stop_propagation();
                    on_press(event.start_position, window, cx);
                }
            });
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// What a drag area does with one long-press event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hold {
    /// The area claims the press: it starts.
    Press,
    /// The press the area claimed moved.
    Move,
    /// The press the area claimed ended or was cancelled.
    Done,
    /// Not the area's: a press it did not claim, or none at all.
    Ignore,
}

impl Hold {
    /// What a long-press event in `phase` is to the area, given whether
    /// the area holds the current press (`claimed`) and whether it claims
    /// this event ([`claims`], only ever true at a start).
    fn of(claimed: bool, phase: TouchPhase, claim: bool) -> Hold {
        match phase {
            TouchPhase::Started if claim => Hold::Press,
            TouchPhase::Moved if claimed => Hold::Move,
            TouchPhase::Ended | TouchPhase::Cancelled if claimed => Hold::Done,
            _ => Hold::Ignore,
        }
    }

    /// Whether the area holds the press after this event: from its claimed
    /// start until its end; a start it did not claim drops an old one.
    fn holds(self, claimed: bool, phase: TouchPhase) -> bool {
        match self {
            Hold::Press | Hold::Move => true,
            Hold::Done => false,
            Hold::Ignore => claimed && phase != TouchPhase::Started,
        }
    }
}

/// [`long_press_area`] that also follows the press it claimed: `on_press`
/// runs with where the press started, then `on_move` with the finger's
/// position for each of that press's moves, until it ends or is cancelled.
/// Whether it holds the press outlives a frame drawn while the finger is
/// down.
pub(crate) fn long_press_drag_area(
    on_press: impl Fn(Point<Pixels>, &mut Window, &mut App) + 'static,
    on_move: impl Fn(Point<Pixels>, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    canvas(
        |bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App| {
            let held =
                window.use_keyed_state("long-press-drag", cx, |_, _| Rc::new(Cell::new(false)));
            let held = held.read(cx).clone();
            (window.insert_hitbox(bounds, HitboxBehavior::Normal), held)
        },
        move |_, (hitbox, held): (Hitbox, Rc<Cell<bool>>), window: &mut Window, _: &mut App| {
            window.on_mouse_event(move |event: &LongPressEvent, phase, window, cx| {
                let prevented = window.default_prevented();
                let claim = claims(event.phase, phase, prevented, hitbox.is_hovered(window));
                let claimed = held.get();
                let hold = Hold::of(claimed, event.phase, claim);
                if phase.bubble() {
                    held.set(hold.holds(claimed, event.phase));
                }
                match hold {
                    Hold::Press => {
                        window.prevent_default();
                        cx.stop_propagation();
                        on_press(event.start_position, window, cx);
                    }
                    Hold::Move if phase.bubble() => on_move(event.position, window, cx),
                    _ => {}
                }
            });
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The holds after each of `phases`, starting free, each start claimed
    /// when `claim` is.
    fn run(phases: &[TouchPhase], claim: bool) -> Vec<(Hold, bool)> {
        let mut claimed = false;
        phases
            .iter()
            .map(|&phase| {
                let hold = Hold::of(claimed, phase, claim && phase == TouchPhase::Started);
                claimed = hold.holds(claimed, phase);
                (hold, claimed)
            })
            .collect()
    }

    #[test]
    fn a_claimed_press_moves_until_it_ends() {
        let phases = [
            TouchPhase::Started,
            TouchPhase::Moved,
            TouchPhase::Moved,
            TouchPhase::Ended,
            TouchPhase::Moved,
        ];
        assert_eq!(
            run(&phases, true),
            [
                (Hold::Press, true),
                (Hold::Move, true),
                (Hold::Move, true),
                (Hold::Done, false),
                (Hold::Ignore, false),
            ]
        );
    }

    #[test]
    fn a_cancelled_press_stops_moving() {
        let phases = [
            TouchPhase::Started,
            TouchPhase::Cancelled,
            TouchPhase::Moved,
        ];
        assert_eq!(
            run(&phases, true),
            [
                (Hold::Press, true),
                (Hold::Done, false),
                (Hold::Ignore, false),
            ]
        );
    }

    #[test]
    fn a_press_the_area_did_not_claim_never_moves_it() {
        let phases = [TouchPhase::Started, TouchPhase::Moved, TouchPhase::Ended];
        assert_eq!(
            run(&phases, false),
            [
                (Hold::Ignore, false),
                (Hold::Ignore, false),
                (Hold::Ignore, false),
            ]
        );
    }

    #[test]
    fn a_start_it_does_not_claim_drops_a_press_whose_end_it_missed() {
        let hold = Hold::of(true, TouchPhase::Started, false);
        assert_eq!(hold, Hold::Ignore);
        assert!(!hold.holds(true, TouchPhase::Started));
        assert_eq!(Hold::of(false, TouchPhase::Moved, false), Hold::Ignore);
    }
}
