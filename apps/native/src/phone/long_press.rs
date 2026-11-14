//! A long press on an element, from gpui's own touch recogniser: a touch
//! held [`gpui::GestureTuning`]'s 500 ms within its 8 px slop. The area
//! claims the press on its start, so the lift is not a tap; a shorter
//! touch is still a tap, and a touch that travels is still a pan.

use gpui::{
    App, Bounds, DispatchPhase, Hitbox, HitboxBehavior, IntoElement, LongPressEvent, Pixels,
    Styled as _, TouchPhase, Window, canvas,
};

/// Whether a long press's `phase` event, seen in `dispatch`, is one this
/// area claims: its start, over the area, while no other listener has.
fn claims(phase: TouchPhase, dispatch: DispatchPhase, prevented: bool, hovered: bool) -> bool {
    phase == TouchPhase::Started && dispatch.bubble() && !prevented && hovered
}

/// An area over its parent's whole box (the parent is `relative`) that
/// runs `on_press` when a long press starts on it. Its hitbox blocks no
/// other hitbox, so the parent's taps and the list's scroll pass through.
pub(super) fn long_press_area(
    on_press: impl Fn(&mut Window, &mut App) + 'static,
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
                    on_press(window, cx);
                }
            });
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}
