//! A vertical scrollbar drawn over a scrolling container: a thumb only, no
//! track or arrows, shown while the content overflows. The thumb can be
//! dragged, and a press on the strip above or below it pages by one
//! viewport; the wheel still scrolls the container itself.
//!
//! [`Track`] holds the geometry as plain numbers; [`Scrollbar`] reads them
//! from the container's [`ScrollHandle`] and paints the thumb.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    App, Bounds, DispatchPhase, Hitbox, HitboxBehavior, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, ScrollHandle, Styled as _, Window, canvas, point, px,
    size,
};

use crate::ui_theme::theme;

/// The shortest thumb drawn, however long the content.
pub const MIN_THUMB_HEIGHT: f32 = 24.0;
/// The thumb's width.
const THUMB_WIDTH: f32 = 6.0;
/// The gap between the thumb and the container's right edge.
const THUMB_INSET: f32 = 2.0;
/// The width of the strip a press pages in: the thumb and its inset on
/// either side.
const STRIP_WIDTH: f32 = THUMB_WIDTH + 2.0 * THUMB_INSET;

/// The thumb's place, in pixels from the top of the viewport.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thumb {
    /// The distance from the viewport's top to the thumb's.
    pub top: f32,
    /// The thumb's height.
    pub height: f32,
}

impl Thumb {
    fn bottom(self) -> f32 {
        self.top + self.height
    }
}

/// A scrolling container's geometry: how tall its viewport and its content
/// are, and how far down it is scrolled (0 at the top).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Track {
    /// The visible height.
    pub viewport: f32,
    /// The height of everything inside, padding included.
    pub content: f32,
    /// How far the content is scrolled down.
    pub scrolled: f32,
}

impl Track {
    /// The thumb for this geometry, or `None` when the content fits.
    #[must_use]
    pub fn thumb(self) -> Option<Thumb> {
        if self.viewport <= 0.0 || self.content <= self.viewport {
            return None;
        }
        let height = self.thumb_height();
        let travel = self.viewport - height;
        let share = self.scrolled.clamp(0.0, self.max_scroll()) / self.max_scroll();
        Some(Thumb {
            top: travel * share,
            height,
        })
    }

    fn max_scroll(self) -> f32 {
        (self.content - self.viewport).max(0.0)
    }

    fn thumb_height(self) -> f32 {
        (self.viewport * self.viewport / self.content)
            .max(MIN_THUMB_HEIGHT)
            .min(self.viewport)
    }

    /// Where the content is scrolled to after the thumb, pressed while the
    /// content sat at `from`, is dragged by `delta` pixels.
    #[must_use]
    pub fn dragged(self, from: f32, delta: f32) -> f32 {
        let travel = self.viewport - self.thumb_height();
        if self.max_scroll() <= 0.0 || travel <= 0.0 {
            return from;
        }
        (from + delta * self.max_scroll() / travel).clamp(0.0, self.max_scroll())
    }

    /// Where the content is scrolled to after a press `at` pixels from the
    /// viewport's top: one viewport up above the thumb, one down below it,
    /// and unchanged on it.
    #[must_use]
    pub fn paged(self, at: f32) -> f32 {
        let Some(thumb) = self.thumb() else {
            return self.scrolled;
        };
        let target = if at < thumb.top {
            self.scrolled - self.viewport
        } else if at > thumb.bottom() {
            self.scrolled + self.viewport
        } else {
            self.scrolled
        };
        target.clamp(0.0, self.max_scroll())
    }
}

/// A thumb drag in progress: where the pointer was pressed and how far the
/// content was scrolled then.
#[derive(Clone, Copy, Debug)]
struct Drag {
    pointer_y: f32,
    scrolled: f32,
}

/// The scroll handle a container tracks and the drag on its thumb.
#[derive(Default)]
pub struct Scrollbar {
    handle: ScrollHandle,
    drag: Rc<Cell<Option<Drag>>>,
}

impl Scrollbar {
    /// The handle the scrolling container passes to `track_scroll`.
    #[must_use]
    pub fn handle(&self) -> &ScrollHandle {
        &self.handle
    }

    /// The thumb as the container was last laid out, or `None` when its
    /// content fits.
    #[must_use]
    pub fn thumb(&self) -> Option<Thumb> {
        track(&self.handle).thumb()
    }

    /// The strip that paints the thumb, positioned over the container's
    /// right edge; its parent must be `relative` and hold the container
    /// before it, so the container's layout of this frame is read.
    pub fn overlay(&self) -> impl IntoElement {
        let handle = self.handle.clone();
        let paint_handle = self.handle.clone();
        let drag = self.drag.clone();
        canvas(
            move |bounds, window, cx| prepaint(&handle, bounds, window, cx),
            move |bounds, laid: Option<(Thumb, Hitbox, u32)>, window, _| {
                paint(&paint_handle, &drag, bounds, laid, window);
            },
        )
        .absolute()
        .top_0()
        .right_0()
        .h_full()
        .w(px(STRIP_WIDTH))
    }
}

/// The geometry `handle` held after its container's last layout.
fn track(handle: &ScrollHandle) -> Track {
    let viewport = handle.bounds().size.height / px(1.0);
    Track {
        viewport,
        content: viewport + handle.max_offset().height / px(1.0),
        scrolled: -handle.offset().y / px(1.0),
    }
}

/// Moves the content `handle` scrolls to `scrolled` pixels from its top.
fn scroll_to(handle: &ScrollHandle, scrolled: f32) {
    let offset = handle.offset();
    handle.set_offset(point(offset.x, px(-scrolled)));
}

/// Lays the thumb out and, while there is one, claims the strip so the
/// content beneath it takes no presses; the wheel still reaches it. The
/// thumb comes with its fill, as `0xRRGGBB`.
fn prepaint(
    handle: &ScrollHandle,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &App,
) -> Option<(Thumb, Hitbox, u32)> {
    let thumb = track(handle).thumb()?;
    let hitbox = window.insert_hitbox(bounds, HitboxBehavior::BlockMouseExceptScroll);
    Some((thumb, hitbox, theme(cx).line_strong))
}

/// Paints the thumb and listens for presses on the strip and for the drag.
fn paint(
    handle: &ScrollHandle,
    drag: &Rc<Cell<Option<Drag>>>,
    bounds: Bounds<Pixels>,
    laid: Option<(Thumb, Hitbox, u32)>,
    window: &mut Window,
) {
    let Some((thumb, hitbox, thumb_fill)) = laid else {
        drag.set(None);
        return;
    };
    let thumb_bounds = Bounds::new(
        point(
            bounds.right() - px(THUMB_INSET + THUMB_WIDTH),
            bounds.top() + px(thumb.top),
        ),
        size(px(THUMB_WIDTH), px(thumb.height)),
    );
    window.paint_quad(
        gpui::fill(thumb_bounds, gpui::rgb(thumb_fill)).corner_radii(px(THUMB_WIDTH / 2.0)),
    );
    let view = window.current_view();
    let top = bounds.top();
    let (down_handle, down_drag) = (handle.clone(), drag.clone());
    window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx: &mut App| {
        if phase != DispatchPhase::Bubble
            || event.button != MouseButton::Left
            || !hitbox.is_hovered(window)
        {
            return;
        }
        cx.stop_propagation();
        let at = (event.position.y - top) / px(1.0);
        let now = track(&down_handle);
        if (thumb.top..=thumb.bottom()).contains(&at) {
            down_drag.set(Some(Drag {
                pointer_y: event.position.y / px(1.0),
                scrolled: now.scrolled,
            }));
        } else {
            scroll_to(&down_handle, now.paged(at));
            cx.notify(view);
        }
    });
    let (move_handle, move_drag) = (handle.clone(), drag.clone());
    window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx: &mut App| {
        if phase != DispatchPhase::Bubble {
            return;
        }
        let Some(start) = move_drag.get() else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            move_drag.set(None);
            return;
        }
        let delta = event.position.y / px(1.0) - start.pointer_y;
        scroll_to(
            &move_handle,
            track(&move_handle).dragged(start.scrolled, delta),
        );
        cx.notify(view);
    });
    let up_drag = drag.clone();
    window.on_mouse_event(move |_: &MouseUpEvent, phase, _, _| {
        if phase == DispatchPhase::Bubble {
            up_drag.set(None);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{MIN_THUMB_HEIGHT, Thumb, Track};

    fn track(viewport: f32, content: f32, scrolled: f32) -> Track {
        Track {
            viewport,
            content,
            scrolled,
        }
    }

    #[test]
    fn content_that_fits_has_no_thumb() {
        assert_eq!(track(400.0, 400.0, 0.0).thumb(), None, "exactly fits");
        assert_eq!(track(400.0, 300.0, 0.0).thumb(), None, "shorter");
        assert_eq!(track(0.0, 300.0, 0.0).thumb(), None, "nothing laid out");
    }

    #[test]
    fn half_scrolled_content_puts_the_thumb_in_the_middle() {
        let thumb = track(400.0, 800.0, 200.0).thumb();
        assert_eq!(
            thumb,
            Some(Thumb {
                top: 100.0,
                height: 200.0
            })
        );
    }

    #[test]
    fn the_thumb_starts_at_the_top_and_ends_at_the_bottom() {
        assert_eq!(track(400.0, 800.0, 0.0).thumb().map(|t| t.top), Some(0.0));
        assert_eq!(
            track(400.0, 800.0, 400.0).thumb().map(Thumb::bottom),
            Some(400.0)
        );
    }

    #[test]
    fn long_content_keeps_the_minimum_thumb_height() {
        let top = track(400.0, 40_000.0, 0.0).thumb();
        assert_eq!(
            top,
            Some(Thumb {
                top: 0.0,
                height: MIN_THUMB_HEIGHT
            })
        );
        let bottom = track(400.0, 40_000.0, 39_600.0).thumb();
        assert_eq!(
            bottom,
            Some(Thumb {
                top: 400.0 - MIN_THUMB_HEIGHT,
                height: MIN_THUMB_HEIGHT
            })
        );
    }

    #[test]
    fn a_drag_moves_the_content_by_its_share_of_the_travel() {
        // A 200 px thumb travels 200 px over 400 px of scroll.
        let geometry = track(400.0, 800.0, 0.0);
        assert_eq!(geometry.dragged(0.0, 50.0), 100.0, "down");
        assert_eq!(geometry.dragged(200.0, -50.0), 100.0, "up");
        assert_eq!(geometry.dragged(0.0, 1_000.0), 400.0, "stops at the end");
        assert_eq!(geometry.dragged(100.0, -1_000.0), 0.0, "stops at the top");
    }

    #[test]
    fn a_press_above_or_below_the_thumb_pages_by_one_viewport() {
        // An 80 px thumb at 160..240 px.
        let middle = track(400.0, 2_000.0, 800.0);
        assert_eq!(middle.paged(50.0), 400.0, "above pages up");
        assert_eq!(middle.paged(300.0), 1_200.0, "below pages down");
        assert_eq!(middle.paged(200.0), 800.0, "on the thumb stays");
        assert_eq!(
            track(400.0, 2_000.0, 100.0).paged(0.0),
            0.0,
            "stops at the top"
        );
        assert_eq!(
            track(400.0, 2_000.0, 1_500.0).paged(10.0),
            1_100.0,
            "a page up from near the end"
        );
        assert_eq!(
            track(400.0, 2_000.0, 1_500.0).paged(399.0),
            1_600.0,
            "stops at the end"
        );
    }
}
