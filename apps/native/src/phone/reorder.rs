//! Drag to reorder the phone's Settings lists: where a finger is in an
//! equal-height list, the items with one moved to another's side, and the
//! one drag area a page draws over the list boxes it recorded each frame.
//! `quick_replies_view` draws the rows and the area.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{App, Bounds, Context, IntoElement, Pixels, Point, Styled as _, Window, canvas, px};

use super::PhoneRoot;
use super::long_press::long_press_drag_area;
use super::quick_replies::{self, ReplyEdit, ReplySet};
use super::settings::SubPage;
use crate::drag::Side;

/// A drawn list's box in the window: `len` rows share its height.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ListBox {
    /// The window y of the list's top.
    pub(crate) top: f32,
    /// Its height: its rows' together.
    pub(crate) height: f32,
}

/// Which list a drag moves, and so which edit its lift makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListId {
    /// A set's groups on the Quick replies page.
    ReplyGroups(ReplySet),
    /// The open group page's replies.
    Replies,
}

/// The drag in progress: the list and the row it lifts, and the slot the
/// finger is over once it has moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReorderDrag {
    /// The list the lifted row belongs to.
    pub(crate) list: ListId,
    /// The lifted row's index in that list.
    pub(crate) from: usize,
    /// The row and half the finger is over, once it has moved.
    pub(crate) over: Option<(usize, Side)>,
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
}

impl RecordedList {
    /// Whether `y` is on one of its rows: inside the box, over a row it
    /// holds.
    fn holds_row(self, y: f32) -> bool {
        y >= self.list.top
            && y <= self.list.top + self.list.height
            && list_slot(y, self.list, self.rows).is_some()
    }
}

/// The lists a page drew in its last frame, one per list.
pub(crate) type ListBoxes = Rc<RefCell<Vec<RecordedList>>>;

/// The row of an equal-height list a finger at `y` points at, and which half
/// of it; none while the list holds no row or drew no height.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a drawn list holds far fewer rows than an f32 counts exactly, and the row is clamped into it first"
)]
pub(crate) fn list_slot(y: f32, list: ListBox, len: usize) -> Option<(usize, Side)> {
    if len == 0 || list.height <= 0.0 {
        return None;
    }
    let pitch = list.height / len as f32;
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

/// Records `list`'s box and its `rows` as it is drawn this frame, replacing
/// the one the list recorded before, for a press on the page to hit-test
/// against.
pub(super) fn list_box_canvas(boxes: &ListBoxes, list: ListId, rows: usize) -> impl IntoElement {
    let boxes = boxes.clone();
    canvas(
        move |bounds: Bounds<Pixels>, _: &mut Window, _: &mut App| {
            let recorded = RecordedList {
                id: list,
                list: ListBox {
                    top: bounds.origin.y / px(1.0),
                    height: bounds.size.height / px(1.0),
                },
                rows,
            };
            let mut boxes = boxes.borrow_mut();
            match boxes.iter_mut().find(|known| known.id == list) {
                Some(entry) => *entry = recorded,
                None => boxes.push(recorded),
            }
        },
        |_, (), _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// The one drag area a page draws over its body: it wants only a press on a
/// recorded list's row (so a hold on anything else still taps and pans), and
/// lifts that row, the finger's moves pick the slot it is over, and the lift
/// drops the row there.
pub(super) fn reorder_area(boxes: &ListBoxes, cx: &mut Context<PhoneRoot>) -> impl IntoElement {
    let (held, press, moved, end_root) = (
        boxes.clone(),
        cx.weak_entity(),
        cx.weak_entity(),
        cx.weak_entity(),
    );
    let wants = move |at: Point<Pixels>| {
        let y = at.y / px(1.0);
        held.borrow().iter().any(|list| list.holds_row(y))
    };
    long_press_drag_area(
        wants,
        move |at, _, cx| {
            // Fails only when the phone's root is gone.
            press.update(cx, |this, cx| this.reorder_press(at, cx)).ok();
        },
        move |at, _, cx| {
            moved.update(cx, |this, cx| this.reorder_move(at, cx)).ok();
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
    /// Clears the recorded list boxes before a page draws its own.
    pub(super) fn forget_list_boxes(&self) {
        self.list_boxes.borrow_mut().clear();
    }

    /// A long press on an editable page: the recorded list under it and the
    /// row there lift for a drag.
    fn reorder_press(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        self.reorder_drag = None;
        let y = at.y / px(1.0);
        let hit = {
            let boxes = self.list_boxes.borrow();
            boxes.iter().find(|list| list.holds_row(y)).copied()
        };
        let Some(list) = hit else {
            return;
        };
        let Some((from, _)) = list_slot(y, list.list, list.rows) else {
            return;
        };
        self.reorder_drag = Some(ReorderDrag {
            list: list.id,
            from,
            over: None,
        });
        cx.notify();
    }

    /// The held finger moved: the slot it is over now.
    fn reorder_move(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(mut drag) = self.reorder_drag else {
            return;
        };
        let y = at.y / px(1.0);
        let recorded = {
            let boxes = self.list_boxes.borrow();
            boxes.iter().find(|list| list.id == drag.list).copied()
        };
        let Some(list) = recorded else {
            return;
        };
        drag.over = list_slot(y, list.list, list.rows);
        self.reorder_drag = Some(drag);
        cx.notify();
    }

    /// The held finger lifted: the row drops where it is over, and the list
    /// saves once. A cancelled touch drops the drag and saves nothing.
    fn reorder_end(&mut self, ended: bool, cx: &mut Context<Self>) {
        let Some(drag) = self.reorder_drag.take() else {
            return;
        };
        if ended
            && let Some((to, side)) = drag.over
            && let Some((set, edit)) = self.reorder_edit(drag, to, side)
        {
            self.save_move(set, edit, cx);
        }
        cx.notify();
    }

    /// The edit a drag's lift makes: a group's move, or a reply's in the
    /// group page that is open.
    fn reorder_edit(
        &self,
        drag: ReorderDrag,
        to: usize,
        side: Side,
    ) -> Option<(ReplySet, ReplyEdit)> {
        match drag.list {
            ListId::ReplyGroups(set) => Some((
                set,
                ReplyEdit::MoveGroup {
                    from: drag.from,
                    to,
                    side,
                },
            )),
            ListId::Replies => {
                let SubPage::ReplyGroup { set, group } = self.settings.as_ref()?.top()? else {
                    return None;
                };
                Some((
                    set,
                    ReplyEdit::MoveReply {
                        group,
                        from: drag.from,
                        to,
                        side,
                    },
                ))
            }
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
        assert_eq!(list_slot(100.0, list(), 4), Some((0, Side::Before)));
        assert_eq!(list_slot(124.9, list(), 4), Some((0, Side::Before)));
    }

    #[test]
    fn list_slot_middle_counts_as_after() {
        assert_eq!(list_slot(125.0, list(), 4), Some((0, Side::After)));
    }

    #[test]
    fn list_slot_second_row() {
        assert_eq!(list_slot(160.0, list(), 4), Some((1, Side::Before)));
    }

    #[test]
    fn list_slot_bottom_of_the_last_row() {
        assert_eq!(list_slot(299.0, list(), 4), Some((3, Side::After)));
    }

    #[test]
    fn list_slot_above_is_before_the_first() {
        assert_eq!(list_slot(40.0, list(), 4), Some((0, Side::Before)));
    }

    #[test]
    fn list_slot_below_is_after_the_last() {
        assert_eq!(list_slot(400.0, list(), 4), Some((3, Side::After)));
    }

    #[test]
    fn list_slot_empty_list_has_no_slot() {
        assert_eq!(list_slot(150.0, list(), 0), None);
        assert_eq!(
            list_slot(
                150.0,
                ListBox {
                    top: 100.0,
                    height: 0.0
                },
                4
            ),
            None
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
