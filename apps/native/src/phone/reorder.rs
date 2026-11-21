//! Drag to reorder the phone's Settings lists: where a finger is in an
//! equal-height list, the items with one moved to another's side, the one
//! drag area a page draws over the boxes it recorded each frame, and how a
//! dragged list draws its lifted row and its drop slot. `quick_replies_view`
//! and `keys_view` draw them.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, Context, Div, IntoElement, ParentElement as _, Pixels, Point,
    ScrollHandle, Stateful, Styled as _, Window, canvas, div, point, px,
};

use super::PhoneRoot;
use super::autoscroll::{self, Visible};
use super::long_press::long_press_drag_area;
use super::quick_replies::{self, ReplyEdit, ReplySet};
use super::settings::{Settings, SubPage};
use crate::drag::Side;
use crate::driver_id::DriverId as _;
use crate::ui_theme::UiTheme;

/// How faintly a dragged row draws while it is lifted.
const LIFTED_OPACITY: f32 = 0.4;
/// The accent line marking a drop slot, in points.
const SLOT_LINE: f32 = 2.0;
/// The driver id of the mark over the lifted row, which draws nothing.
const LIFTED_ID: &str = "phone-drag-lifted";
/// The driver id of the accent line at the drop slot.
const SLOT_ID: &str = "phone-drag-slot";

/// A drawn list's box in the window: `len` rows share its height, while the
/// drop slot's accent line is not drawn there.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ListBox {
    /// The window y of the list's top.
    pub(crate) top: f32,
    /// Its height: its rows' together, and the drop slot's line's while that
    /// is drawn.
    pub(crate) height: f32,
}

/// Which list a drag moves, and so which edit its lift makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListId {
    /// A set's groups on the Quick replies page.
    ReplyGroups(ReplySet),
    /// The open group page's replies.
    Replies,
    /// The Keys page's keys.
    KeyList,
    /// The open key editor's presses.
    KeySteps,
}

/// The drag in progress: the list and the row it lifts, and the slot the
/// finger is over and where the finger is once it has moved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ReorderDrag {
    /// The list the lifted row belongs to.
    pub(crate) list: ListId,
    /// The lifted row's index in that list.
    pub(crate) from: usize,
    /// The row and half the finger is over, once it has moved.
    pub(crate) over: Option<(usize, Side)>,
    /// The finger's last window position, once it has moved: a finger held
    /// still sends no moves, so each auto-scroll frame reads it here.
    pub(crate) finger: Option<Point<Pixels>>,
}

/// A list as its page drew it in the last frame: its box and how many rows
/// it holds beside it, so a press can hit-test the list without an entity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RecordedList {
    /// Which list it is.
    pub(crate) id: ListId,
    /// Its box in the window.
    pub(crate) list: ListBox,
    /// How many rows it holds.
    pub(crate) rows: usize,
    /// Whether it draws its drop slot's accent line, which the box's height
    /// includes and no row shares.
    pub(crate) slot_shown: bool,
}

impl RecordedList {
    /// Whether `at` is on one of its rows: inside the box, over a row it
    /// holds.
    fn holds(self, at: Point<Pixels>) -> bool {
        let y = at.y / px(1.0);
        y >= self.list.top
            && y <= self.list.top + self.list.height
            && list_slot(y, self.list, self.rows, self.slot_shown).is_some()
    }

    /// The row `at` points at, and which half of it.
    fn slot(self, at: Point<Pixels>) -> Option<(usize, Side)> {
        list_slot(at.y / px(1.0), self.list, self.rows, self.slot_shown)
    }
}

/// What the open Settings page keeps for its drags: the boxes its lists
/// drew in the last frame, one per list, the Settings body's scroll, which
/// a drag held near its edge moves, and whether a frame is asked for to
/// move it.
#[derive(Default)]
pub(crate) struct DragPage {
    /// The boxes the page's lists drew in its last frame.
    boxes: RefCell<Vec<RecordedList>>,
    /// The Settings body's scroll.
    pub(crate) scroll: ScrollHandle,
    /// Whether an auto-scroll frame is asked for and not yet run.
    stepping: Cell<bool>,
}

/// The open page's drag record, shared with the canvases that record it.
pub(crate) type ListBoxes = Rc<DragPage>;

/// The row of an equal-height list a finger at `y` points at, and which half
/// of it; none while the list holds no row or drew no height. `slot_shown`
/// says the list draws its drop slot's accent line, whose height the box
/// also holds and which is no row's, so the pitch is the rows' alone.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a drawn list holds far fewer rows than an f32 counts exactly, and the row is clamped into it first"
)]
pub(crate) fn list_slot(
    y: f32,
    list: ListBox,
    len: usize,
    slot_shown: bool,
) -> Option<(usize, Side)> {
    let height = list.height - if slot_shown { SLOT_LINE } else { 0.0 };
    if len == 0 || height <= 0.0 {
        return None;
    }
    let pitch = height / len as f32;
    let row = ((y - list.top) / pitch)
        .floor()
        .clamp(0.0, (len - 1) as f32);
    Some((
        row as usize,
        crate::drag::side_of(y, list.top + row * pitch, pitch),
    ))
}

/// `items` with the one at `from` moved to `side` of the one at `to`; an
/// index that names nothing leaves them as they are.
#[must_use]
pub(crate) fn reordered<T: Clone>(items: &[T], from: usize, to: usize, side: Side) -> Vec<T> {
    let order: Vec<usize> = (0..items.len()).collect();
    crate::drag::reorder(&order, &from, &to, side)
        .into_iter()
        .map(|index| items[index].clone())
        .collect()
}

/// Records `drawn` as its page drew it this frame, replacing the box it
/// recorded before, for a press on the page to hit-test against.
fn record(boxes: &ListBoxes, drawn: RecordedList) {
    let mut boxes = boxes.boxes.borrow_mut();
    match boxes.iter_mut().find(|known| known.id == drawn.id) {
        Some(entry) => *entry = drawn,
        None => boxes.push(drawn),
    }
}

/// Records `list`'s box and its `rows` as it is drawn this frame, replacing
/// the one the list recorded before, for a press on the page to hit-test
/// against; `slot_shown` is whether the box this frame holds the drop slot's
/// accent line.
pub(super) fn list_box_canvas(
    boxes: &ListBoxes,
    list: ListId,
    rows: usize,
    slot_shown: bool,
) -> impl IntoElement {
    let boxes = boxes.clone();
    canvas(
        move |bounds: Bounds<Pixels>, _: &mut Window, _: &mut App| {
            record(
                &boxes,
                RecordedList {
                    id: list,
                    list: ListBox {
                        top: bounds.origin.y / px(1.0),
                        height: bounds.size.height / px(1.0),
                    },
                    rows,
                    slot_shown,
                },
            );
        },
        |_, (), _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// The drop slot `drag` marks on the row at `index`, if any.
pub(super) fn slot_of(drag: Option<ReorderDrag>, index: usize) -> Option<Side> {
    drag.and_then(|drag| drag.over)
        .filter(|(at, _)| *at == index)
        .map(|(_, side)| side)
}

/// One row of a dragged list as its page draws it: the accent line where the
/// row above would drop it, the row itself, at reduced opacity and marked
/// while it is the lifted one, and the line where it would drop below it.
pub(super) fn dragged_row(
    row: Stateful<Div>,
    lifted: bool,
    slot: Option<Side>,
    t: &UiTheme,
) -> Vec<AnyElement> {
    let line = || {
        div()
            .driver_id(|| SLOT_ID.to_owned())
            .flex_none()
            .h(px(SLOT_LINE))
            .w_full()
            .bg(gpui::rgb(t.accent))
            .into_any_element()
    };
    let mut out = Vec::new();
    if slot == Some(Side::Before) {
        out.push(line());
    }
    out.push(if lifted {
        row.relative()
            .opacity(LIFTED_OPACITY)
            .child(
                div()
                    .driver_id(|| LIFTED_ID.to_owned())
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
            )
            .into_any_element()
    } else {
        row.into_any_element()
    });
    if slot == Some(Side::After) {
        out.push(line());
    }
    out
}

/// The one drag area a page draws over its body: it wants only a press on a
/// recorded box's row or cell (so a hold on anything else still taps and
/// pans), lifts that row, the finger's moves pick the slot it is over, and
/// the lift drops the row there.
pub(super) fn reorder_area(boxes: &ListBoxes, cx: &mut Context<PhoneRoot>) -> impl IntoElement {
    let (held, press, moved, end_root) = (
        boxes.clone(),
        cx.weak_entity(),
        cx.weak_entity(),
        cx.weak_entity(),
    );
    let wants = move |at: Point<Pixels>| held.boxes.borrow().iter().any(|drawn| drawn.holds(at));
    long_press_drag_area(
        wants,
        move |at, _, cx| {
            // Fails only when the phone's root is gone.
            press.update(cx, |this, cx| this.reorder_press(at, cx)).ok();
        },
        move |at, window, cx| {
            moved
                .update(cx, |this, cx| this.reorder_move(at, window, cx))
                .ok();
        },
        move |ended, _, cx| {
            end_root
                .update(cx, |this, cx| this.reorder_end(ended, cx))
                .ok();
        },
    )
}

// How a page's drag works: the boxes its lists recorded, and the one area
// over them that lifts a row and drops it.
impl PhoneRoot {
    /// Clears the recorded boxes before a page draws its own.
    pub(super) fn forget_list_boxes(&self) {
        self.list_boxes.boxes.borrow_mut().clear();
    }

    /// A long press on an editable page: the recorded list under it and the
    /// row there lift for a drag.
    fn reorder_press(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        self.reorder_drag = None;
        let hit = {
            let boxes = self.list_boxes.boxes.borrow();
            boxes.iter().find(|drawn| drawn.holds(at)).copied()
        };
        let Some(hit) = hit else {
            return;
        };
        let Some((from, _)) = hit.slot(at) else {
            return;
        };
        if hit.id == ListId::KeyList {
            self.disarm_keys();
        }
        self.reorder_drag = Some(ReorderDrag {
            list: hit.id,
            from,
            over: None,
            finger: None,
        });
        cx.notify();
    }

    /// A drag that lifts the row at `from` of `list` at once, as a row's drag
    /// handle does.
    pub(super) fn reorder_lift(&mut self, list: ListId, from: usize, cx: &mut Context<Self>) {
        self.reorder_drag = Some(ReorderDrag {
            list,
            from,
            over: None,
            finger: None,
        });
        cx.notify();
    }

    /// The held finger moved: the slot it is over now, and, when it is in an
    /// edge zone of the Settings body, a frame asked for to scroll it.
    pub(super) fn reorder_move(
        &mut self,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mut drag) = self.reorder_drag else {
            return;
        };
        let Some(recorded) = self.recorded(drag.list) else {
            return;
        };
        drag.over = recorded.slot(at);
        drag.finger = Some(at);
        self.reorder_drag = Some(drag);
        cx.notify();
        if self.autoscroll_step(at) != 0.0 {
            self.ask_scroll_frame(window, cx);
        }
    }

    /// The box `list` recorded, as the page last drew it or as an
    /// auto-scroll frame moved it since.
    fn recorded(&self, list: ListId) -> Option<RecordedList> {
        let boxes = self.list_boxes.boxes.borrow();
        boxes.iter().find(|drawn| drawn.id == list).copied()
    }

    /// How far the Settings body scrolls this frame with the finger at `at`
    /// ([`autoscroll::autoscroll_step`] over its visible bounds).
    fn autoscroll_step(&self, at: Point<Pixels>) -> f32 {
        let bounds = self.list_boxes.scroll.bounds();
        let visible = Visible {
            top: bounds.top() / px(1.0),
            bottom: bounds.bottom() / px(1.0),
        };
        autoscroll::autoscroll_step(at.y / px(1.0), visible, autoscroll::SETTINGS)
    }

    /// Asks for one auto-scroll frame, unless one is already asked for.
    fn ask_scroll_frame(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.list_boxes.stepping.replace(true) {
            return;
        }
        cx.on_next_frame(window, Self::scroll_frame);
    }

    /// One auto-scroll frame: while a drag is held in an edge zone, the
    /// Settings body moves one step toward that edge, the drop slot follows
    /// the finger in the moved list, and the next frame is asked for. A
    /// drag that ended, a list no longer drawn, a finger out of the zones or
    /// a page at its end asks for none.
    fn scroll_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.list_boxes.stepping.set(false);
        let Some(mut drag) = self.reorder_drag else {
            return;
        };
        let Some(finger) = drag.finger else {
            return;
        };
        if self.recorded(drag.list).is_none() {
            return;
        }
        if self.scroll_page_by(self.autoscroll_step(finger)) == 0.0 {
            return;
        }
        if let Some(recorded) = self.recorded(drag.list) {
            drag.over = recorded.slot(finger);
        }
        self.reorder_drag = Some(drag);
        cx.notify();
        self.ask_scroll_frame(window, cx);
    }

    /// Scrolls the Settings body by `step` points (positive toward its
    /// bottom), never past either end, and moves the recorded boxes with
    /// its content; how far it moved.
    fn scroll_page_by(&self, step: f32) -> f32 {
        let scroll = &self.list_boxes.scroll;
        let offset = scroll.offset();
        let (now, max) = (offset.y / px(1.0), scroll.max_offset().y / px(1.0));
        let next = (now - step).clamp(-max.max(0.0), 0.0);
        let moved = now - next;
        if moved == 0.0 {
            return 0.0;
        }
        scroll.set_offset(point(offset.x, px(next)));
        for drawn in self.list_boxes.boxes.borrow_mut().iter_mut() {
            drawn.list.top -= moved;
        }
        moved
    }

    /// The held finger lifted: the row drops where it is over, and
    /// the list saves once. A cancelled touch drops the drag and saves
    /// nothing.
    pub(super) fn reorder_end(&mut self, ended: bool, cx: &mut Context<Self>) {
        let Some(drag) = self.reorder_drag.take() else {
            return;
        };
        if ended && let Some((to, side)) = drag.over {
            self.apply_drag(drag, to, side, cx);
        }
        cx.notify();
    }

    /// The edit a lifted drag makes on the list it came from.
    fn apply_drag(&mut self, drag: ReorderDrag, to: usize, side: Side, cx: &mut Context<Self>) {
        match drag.list {
            ListId::ReplyGroups(set) => {
                let edit = ReplyEdit::MoveGroup {
                    from: drag.from,
                    to,
                    side,
                };
                self.save_move(set, edit, cx);
            }
            ListId::Replies => {
                let Some(SubPage::ReplyGroup { set, group }) =
                    self.settings.as_ref().and_then(Settings::top)
                else {
                    return;
                };
                let edit = ReplyEdit::MoveReply {
                    group,
                    from: drag.from,
                    to,
                    side,
                };
                self.save_move(set, edit, cx);
            }
            ListId::KeyList => self.move_key(drag.from, to, side, cx),
            ListId::KeySteps => self.move_press(drag.from, to, side, cx),
        }
    }

    /// Saves `set` with `edit` made when it changes the order; a move that
    /// changes nothing writes nothing.
    fn save_move(&mut self, set: ReplySet, edit: ReplyEdit, cx: &mut Context<Self>) {
        let groups = self.saved_groups(set, cx);
        let Some(next) = quick_replies::apply(&groups, edit) else {
            return;
        };
        if next == groups {
            return;
        }
        self.root
            .update(cx, |root, cx| root.set_phone_replies(set, Some(next), cx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list box the slot tests hit-test: rows 50 tall from y 100.
    fn list() -> ListBox {
        ListBox {
            top: 100.0,
            height: 200.0,
        }
    }

    #[test]
    fn list_slot_top_half_of_the_first_row() {
        assert_eq!(list_slot(100.0, list(), 4, false), Some((0, Side::Before)));
        assert_eq!(list_slot(124.9, list(), 4, false), Some((0, Side::Before)));
    }

    #[test]
    fn list_slot_middle_counts_as_after() {
        assert_eq!(list_slot(125.0, list(), 4, false), Some((0, Side::After)));
    }

    #[test]
    fn list_slot_second_row() {
        assert_eq!(list_slot(160.0, list(), 4, false), Some((1, Side::Before)));
    }

    #[test]
    fn list_slot_bottom_of_the_last_row() {
        assert_eq!(list_slot(299.0, list(), 4, false), Some((3, Side::After)));
    }

    #[test]
    fn list_slot_above_is_before_the_first() {
        assert_eq!(list_slot(40.0, list(), 4, false), Some((0, Side::Before)));
    }

    #[test]
    fn list_slot_below_is_after_the_last() {
        assert_eq!(list_slot(400.0, list(), 4, false), Some((3, Side::After)));
    }

    #[test]
    fn list_slot_empty_list_has_no_slot() {
        assert_eq!(list_slot(150.0, list(), 0, false), None);
        assert_eq!(
            list_slot(
                150.0,
                ListBox {
                    top: 100.0,
                    height: 0.0
                },
                4,
                false
            ),
            None
        );
    }

    /// Twenty 44 pt rows from y 100, with a drop slot's accent line drawn
    /// under one of them.
    fn slot_list() -> ListBox {
        ListBox {
            top: 100.0,
            height: 20.0 * 44.0 + SLOT_LINE,
        }
    }

    #[test]
    fn list_slot_centre_of_a_late_row_is_after_with_the_slot_line() {
        // The twentieth row's exact centre: the line's height belongs to no
        // row, so every row keeps the 44 pt it was drawn at.
        assert_eq!(
            list_slot(100.0 + 19.0 * 44.0 + 22.0, slot_list(), 20, true),
            Some((19, Side::After))
        );
    }

    /// The items `reordered` moves: a, b, c and d.
    fn items() -> Vec<char> {
        vec!['a', 'b', 'c', 'd']
    }

    #[test]
    fn reordered_moves_down() {
        assert_eq!(
            reordered(&items(), 0, 2, Side::After),
            vec!['b', 'c', 'a', 'd']
        );
    }

    #[test]
    fn reordered_moves_to_the_top() {
        assert_eq!(
            reordered(&items(), 3, 0, Side::Before),
            vec!['d', 'a', 'b', 'c']
        );
    }

    #[test]
    fn reordered_before_the_next_changes_nothing() {
        assert_eq!(reordered(&items(), 0, 1, Side::Before), items());
    }

    #[test]
    fn reordered_onto_itself_changes_nothing() {
        assert_eq!(reordered(&items(), 1, 1, Side::After), items());
    }

    #[test]
    fn reordered_out_of_range_changes_nothing() {
        assert_eq!(
            reordered(&['a', 'b', 'c'], 5, 0, Side::Before),
            vec!['a', 'b', 'c']
        );
    }
}
