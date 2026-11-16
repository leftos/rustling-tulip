//! A long press on an element, from gpui's own touch recogniser: a touch
//! held [`gpui::GestureTuning`]'s 500 ms within its 8 px slop. The area
//! claims the press on its start, so the lift is not a tap; a shorter
//! touch is still a tap, and a touch that travels is still a pan.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, DispatchPhase, GestureTuning, Hitbox, HitboxBehavior, IntoElement, LongPressEvent,
    Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Pixels, PlatformInput, Point,
    Styled as _, TouchDragEvent, TouchPhase, Window, canvas, px,
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

/// What one event in `phase` is to a drag area that holds the current touch
/// when `claimed` and claims this event when `claim`, and whether it holds
/// the touch after it ([`Hold::of`], [`Hold::holds`]).
fn step(claimed: bool, phase: TouchPhase, claim: bool) -> (Hold, bool) {
    let hold = Hold::of(claimed, phase, claim);
    (hold, hold.holds(claimed, phase))
}

/// The area's hitbox and its keyed "holds the touch" flag, which outlives
/// a frame drawn while the finger is down.
fn drag_hitbox(
    key: &'static str,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) -> (Hitbox, Rc<Cell<bool>>) {
    let held = window.use_keyed_state(key, cx, |_, _| Rc::new(Cell::new(false)));
    let held = held.read(cx).clone();
    (window.insert_hitbox(bounds, HitboxBehavior::Normal), held)
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
            drag_hitbox("long-press-drag", bounds, window, cx)
        },
        move |_, (hitbox, held): (Hitbox, Rc<Cell<bool>>), window: &mut Window, _: &mut App| {
            window.on_mouse_event(move |event: &LongPressEvent, phase, window, cx| {
                let prevented = window.default_prevented();
                let claim = claims(event.phase, phase, prevented, hitbox.is_hovered(window));
                let (hold, holds) = step(held.get(), event.phase, claim);
                if phase.bubble() {
                    held.set(holds);
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

/// Whether window point `at` lies within `tuning`'s tap slop of `start`.
fn within_slop(start: Point<Pixels>, at: Point<Pixels>, tuning: &GestureTuning) -> bool {
    let (dx, dy) = ((at.x - start.x) / px(1.0), (at.y - start.y) / px(1.0));
    dx.hypot(dy) <= tuning.touch_slop / px(1.0)
}

/// Whether a touch a drag area claimed, started at `start` and lifted at
/// `end` after `elapsed`, was a tap: it `ended` (was not cancelled), ended
/// within `tuning`'s tap slop of its start, and lasted less than its long
/// press time. The caller also checks that no move strayed past the slop.
fn is_tap(
    start: Point<Pixels>,
    end: Point<Pixels>,
    elapsed: Duration,
    ended: bool,
    tuning: &GestureTuning,
) -> bool {
    ended && within_slop(start, end, tuning) && elapsed < tuning.long_press_duration
}

/// A touch-drag area's state for the touch it holds, which outlives a
/// frame drawn while the finger is down.
#[derive(Default)]
struct TouchDrag {
    /// Whether the area holds the current touch.
    held: Cell<bool>,
    /// When the held touch started.
    since: Cell<Option<Instant>>,
    /// Whether the held touch moved past the tap slop.
    strayed: Cell<bool>,
}

impl TouchDrag {
    /// Whether the held touch, ending or cancelled with `event` at `now`,
    /// was a tap ([`is_tap`]) that never strayed past the slop; forgets
    /// when it started.
    fn was_tap(&self, event: &TouchDragEvent, now: Instant, tuning: &GestureTuning) -> bool {
        let elapsed = self
            .since
            .take()
            .map_or(tuning.long_press_duration, |since| {
                now.saturating_duration_since(since)
            });
        let ended = event.phase == TouchPhase::Ended;
        !self.strayed.get() && is_tap(event.start_position, event.position, elapsed, ended, tuning)
    }
}

/// Replays a tap at window point `at` once the current dispatch is over:
/// one left-button press and release, as gpui makes of a tap, through the
/// window's own event path, so what lies under the point takes it.
fn replay_tap(at: Point<Pixels>, window: &Window, cx: &mut App) {
    window.defer(cx, move |window, cx| {
        let down = MouseDownEvent {
            button: MouseButton::Left,
            position: at,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        };
        let up = MouseUpEvent {
            button: MouseButton::Left,
            position: at,
            modifiers: Modifiers::default(),
            click_count: 1,
        };
        window.dispatch_event(PlatformInput::MouseDown(down), cx);
        window.dispatch_event(PlatformInput::MouseUp(up), cx);
    });
}

/// An area over its parent's whole box (the parent is `relative`) that
/// claims a touch starting on it as a direct drag ([`TouchDragEvent`]), so
/// the touch neither pans nor long-presses: `on_start` runs with where the
/// touch started, `on_move` with the finger's position for each of its
/// moves once it has strayed past the tap slop (none before), and `on_end`
/// when it ends or is cancelled. A claimed touch that
/// was a tap ([`is_tap`], never moving past the slop) is then replayed as
/// a tap for what lies under the area. A touch that starts elsewhere is
/// never claimed. The area's state lives in the window's state under `key`,
/// one per area, and outlives a frame drawn while the finger is down.
pub(crate) fn touch_drag_area(
    key: &'static str,
    on_start: impl Fn(Point<Pixels>, &mut Window, &mut App) + 'static,
    on_move: impl Fn(Point<Pixels>, &mut Window, &mut App) + 'static,
    on_end: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    canvas(
        move |bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App| {
            let state = window.use_keyed_state(key, cx, |_, _| Rc::new(TouchDrag::default()));
            let state = state.read(cx).clone();
            (window.insert_hitbox(bounds, HitboxBehavior::Normal), state)
        },
        move |_, (hitbox, state): (Hitbox, Rc<TouchDrag>), window: &mut Window, _: &mut App| {
            window.on_mouse_event(move |event: &TouchDragEvent, phase, window, cx| {
                let prevented = window.default_prevented();
                let claim = claims(event.phase, phase, prevented, hitbox.is_hovered(window));
                let (hold, holds) = step(state.held.get(), event.phase, claim);
                if !phase.bubble() {
                    return;
                }
                state.held.set(holds);
                let tuning = GestureTuning::default();
                match hold {
                    Hold::Press => {
                        window.prevent_default();
                        cx.stop_propagation();
                        state.since.set(Some(cx.background_executor().now()));
                        state.strayed.set(false);
                        on_start(event.start_position, window, cx);
                    }
                    Hold::Move => {
                        if !within_slop(event.start_position, event.position, &tuning) {
                            state.strayed.set(true);
                        }
                        // A move inside the slop may still be a tap: it moves nothing.
                        if state.strayed.get() {
                            on_move(event.position, window, cx);
                        }
                    }
                    Hold::Done => {
                        on_end(window, cx);
                        if state.was_tap(event, cx.background_executor().now(), &tuning) {
                            replay_tap(event.position, window, cx);
                        }
                    }
                    Hold::Ignore => {}
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

    /// The (hold, claimed) after each of `phases` through [`step`], as a
    /// touch-drag area sees them, starting free, each start claimed when
    /// `claim` is.
    fn drag(phases: &[TouchPhase], claim: bool) -> Vec<(Hold, bool)> {
        let mut claimed = false;
        phases
            .iter()
            .map(|&phase| {
                let (hold, next) = step(claimed, phase, claim && phase == TouchPhase::Started);
                claimed = next;
                (hold, claimed)
            })
            .collect()
    }

    #[test]
    fn a_claimed_touch_drag_moves_until_it_ends() {
        let phases = [
            TouchPhase::Started,
            TouchPhase::Moved,
            TouchPhase::Ended,
            TouchPhase::Moved,
        ];
        assert_eq!(
            drag(&phases, true),
            [
                (Hold::Press, true),
                (Hold::Move, true),
                (Hold::Done, false),
                (Hold::Ignore, false),
            ]
        );
    }

    #[test]
    fn a_touch_drag_the_area_did_not_claim_never_moves_it() {
        let phases = [TouchPhase::Started, TouchPhase::Moved, TouchPhase::Ended];
        assert_eq!(
            drag(&phases, false),
            [
                (Hold::Ignore, false),
                (Hold::Ignore, false),
                (Hold::Ignore, false),
            ]
        );
    }

    #[test]
    fn a_cancel_ends_a_touch_drag() {
        let phases = [
            TouchPhase::Started,
            TouchPhase::Cancelled,
            TouchPhase::Moved,
        ];
        assert_eq!(
            drag(&phases, true),
            [
                (Hold::Press, true),
                (Hold::Done, false),
                (Hold::Ignore, false),
            ]
        );
    }

    /// A touch from (100, 100) to `end`, lasting `ms` milliseconds, as a
    /// claimed touch's tap check sees it.
    fn tap(end: (f32, f32), ms: u64, ended: bool) -> bool {
        let tuning = GestureTuning::default();
        let start = gpui::point(px(100.0), px(100.0));
        let end = gpui::point(px(end.0), px(end.1));
        is_tap(start, end, Duration::from_millis(ms), ended, &tuning)
    }

    #[test]
    fn a_claimed_touch_within_the_slop_and_time_is_a_tap() {
        assert!(tap((100.0, 100.0), 0, true), "still");
        let slop = GestureTuning::default().touch_slop / px(1.0);
        assert!(tap((100.0 + slop, 100.0), 100, true), "at the slop's edge");
    }

    #[test]
    fn a_claimed_touch_moved_past_the_slop_is_no_tap() {
        let slop = GestureTuning::default().touch_slop / px(1.0);
        assert!(!tap((100.0, 100.0 + slop + 1.0), 100, true));
    }

    #[test]
    fn a_claimed_touch_held_past_the_long_press_time_is_no_tap() {
        let held = GestureTuning::default().long_press_duration;
        let ms = u64::try_from(held.as_millis()).unwrap_or(u64::MAX);
        assert!(!tap((100.0, 100.0), ms, true));
    }

    #[test]
    fn a_cancelled_claimed_touch_is_no_tap() {
        assert!(!tap((100.0, 100.0), 0, false));
    }

    #[test]
    fn a_start_it_does_not_claim_drops_a_press_whose_end_it_missed() {
        let hold = Hold::of(true, TouchPhase::Started, false);
        assert_eq!(hold, Hold::Ignore);
        assert!(!hold.holds(true, TouchPhase::Started));
        assert_eq!(Hold::of(false, TouchPhase::Moved, false), Hold::Ignore);
    }
}
