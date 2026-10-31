//! Tab pill drag-and-drop specs: a pill dragged onto another pill's half
//! reorders the strip at once and sends the new order, the insertion mark
//! that shows where it lands, every way a pill drag ends without a reorder,
//! and a pane dragged over a pill, which shows its tab or is refused.

use crate::support;

use gpui::{Pixels, Point, TestAppContext, point};
use protocol::{
    ClientMessage, DaemonMessage, PaneDropEdge, SplitDirection, TabEntry, VscodeWorkspaceFolder,
    VscodeWorkspaceSuggestion,
};
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{RootView, Side, ToastKind};
use serde_json::json;
use support::{Fixture, Harness, REMOTE, TestDir, pane, repo, session, split, tab};

/// Tabs `t1`, `t2` and `t3`, each one empty pane, in that order.
fn three_tabs() -> Fixture {
    Fixture {
        tabs: vec![
            tab("t1", &pane("p1", None)),
            tab("t2", &pane("p2", None)),
            tab("t3", &pane("p3", None)),
        ],
        ..Fixture::default()
    }
}

/// Tab `t1`: `p1` showing `s1` beside `p2` showing `s2`, shown; tab `t2`:
/// `p3` showing `s3` beside `p4` showing `s4`.
fn pane_tabs() -> Fixture {
    Fixture {
        sessions: ["s1", "s2", "s3", "s4"]
            .iter()
            .map(|id| session(id).build())
            .collect(),
        tabs: vec![
            tab(
                "t1",
                &split(
                    SplitDirection::Horizontal,
                    pane("p1", Some("s1")),
                    pane("p2", Some("s2")),
                ),
            ),
            tab(
                "t2",
                &split(
                    SplitDirection::Horizontal,
                    pane("p3", Some("s3")),
                    pane("p4", Some("s4")),
                ),
            ),
        ],
        ..Fixture::default()
    }
}

/// A diff tab `t3` beside [`pane_tabs`]'s two grid tabs.
fn with_diff_tab() -> Fixture {
    let mut fixture = pane_tabs();
    fixture.tabs.push(diff_tab("t3"));
    fixture
}

fn diff_tab(id: &str) -> TabEntry {
    serde_json::from_value(json!({
        "id": id,
        "name": "a.rs",
        "content": { "kind": "diff", "repo_id": "r1", "path": "a.rs", "against": null },
        "created_at": "2026-01-01T00:00:00Z",
    }))
    .expect("diff tab fixture")
}

/// The shown tab's id.
fn active(h: &mut Harness) -> Option<String> {
    h.root(|root, _| root.active_tab_id().map(str::to_owned))
}

/// A point on `pane`'s header, over its title, where a drag starts.
fn header_point(h: &mut Harness, pane: &str) -> Point<Pixels> {
    let title = h.bounds(&format!("pane-title-{pane}"));
    assert!(
        title.origin.x >= Pixels::ZERO,
        "{pane}'s title is on screen"
    );
    title.center()
}

/// The point `fx` across and `fy` down `pane`'s card.
fn at(h: &mut Harness, pane: &str, fx: f32, fy: f32) -> Point<Pixels> {
    let frame = h.bounds(&format!("pane-frame-{pane}"));
    assert!(frame.origin.x >= Pixels::ZERO, "{pane} is on screen");
    point(
        frame.origin.x + frame.size.width * fx,
        frame.origin.y + frame.size.height * fy,
    )
}

/// The pane moves among what the client sent since the last call.
fn moves(h: &mut Harness) -> Vec<ClientMessage> {
    h.sent()
        .into_iter()
        .filter(|msg| matches!(msg, ClientMessage::MovePane { .. }))
        .collect()
}

fn undo_messages(h: &mut Harness) -> Vec<String> {
    h.root(|root, _| {
        root.undo_entries()
            .iter()
            .map(|entry| entry.message.clone())
            .collect()
    })
}

/// The id of the newest undo entry.
fn undo_id(h: &mut Harness) -> u64 {
    h.root(|root, _| root.undo_entries().first().map(|entry| entry.id))
        .expect("an undo entry")
}

/// The pane the drop overlay is drawn on, and the edge it shows.
fn overlay(h: &mut Harness) -> Option<(String, PaneDropEdge)> {
    h.root(RootView::pane_drop_overlay)
}

/// The point `fx` across `tab`'s pill, at its middle height.
fn on_pill(h: &mut Harness, tab: &str, fx: f32) -> Point<Pixels> {
    let pill = h.bounds(&format!("tab-{tab}"));
    assert!(pill.size.width > Pixels::ZERO, "{tab}'s pill is on screen");
    point(pill.origin.x + pill.size.width * fx, pill.center().y)
}

/// The strip's tab ids, in order.
fn strip(h: &mut Harness) -> Vec<String> {
    h.root(|root, _| root.tab_ids())
}

/// The pill the insertion mark stands on, and the side of it.
fn mark(h: &mut Harness) -> Option<(String, Side)> {
    h.root(RootView::tab_drop_mark)
}

/// The tab reorders among what the client sent since the last call.
fn reorders(h: &mut Harness) -> Vec<Vec<String>> {
    h.sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ReorderTabs { ordered_ids, .. } => Some(ordered_ids),
            _ => None,
        })
        .collect()
}

/// The id the tab reorder the client sent carries.
fn reorder_request_id(h: &mut Harness) -> String {
    h.sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::ReorderTabs { request_id, .. } => request_id,
            _ => None,
        })
        .expect("the reorder carries a request id")
}

/// Every toast's kind, title and detail, oldest first.
fn toasts(h: &mut Harness) -> Vec<(ToastKind, String, Option<String>)> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| (toast.kind, toast.title.clone(), toast.detail.clone()))
            .collect()
    })
}

fn order(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| (*id).to_owned()).collect()
}

#[gpui::test]
fn pill_dropped_after_another_reorders_the_tabs(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &three_tabs());
    h.sent();
    let from = on_pill(&mut h, "t1", 0.5);
    let to = on_pill(&mut h, "t3", 0.8);

    h.drag_and_drop(from, to);

    assert_eq!(reorders(&mut h), [order(&["t2", "t3", "t1"])]);
    assert_eq!(mark(&mut h), None, "the mark goes with the drop");
}

#[gpui::test]
fn reorder_shows_before_the_daemon_echo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &three_tabs());
    h.sent();
    let from = on_pill(&mut h, "t3", 0.5);
    let to = on_pill(&mut h, "t1", 0.2);

    h.drag_and_drop(from, to);

    assert_eq!(
        strip(&mut h),
        order(&["t3", "t1", "t2"]),
        "the strip takes the new order before any TabsReordered"
    );
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("t1".to_owned())
    );
    h.send(DaemonMessage::TabsReordered {
        ordered_ids: order(&["t3", "t1", "t2"]),
    });
    assert_eq!(strip(&mut h), order(&["t3", "t1", "t2"]), "the echo agrees");
}

#[gpui::test]
fn refused_reorder_snaps_the_strip_back_with_a_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &three_tabs());
    h.sent();
    let from = on_pill(&mut h, "t1", 0.5);
    let to = on_pill(&mut h, "t3", 0.8);

    h.drag_and_drop(from, to);
    assert_eq!(strip(&mut h), order(&["t2", "t3", "t1"]));
    let id = reorder_request_id(&mut h);

    h.send(DaemonMessage::Error {
        message: "unknown tab".to_owned(),
        request_id: Some(id),
        code: None,
    });

    assert_eq!(
        strip(&mut h),
        order(&["t1", "t2", "t3"]),
        "the refused drop puts the strip back where it was"
    );
    assert_eq!(
        toasts(&mut h),
        [(
            ToastKind::Error,
            "Daemon error".to_owned(),
            Some("unknown tab".to_owned())
        )]
    );
}

#[gpui::test]
fn insertion_mark_follows_the_half(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &three_tabs());
    let from = on_pill(&mut h, "t1", 0.5);

    h.start_drag(from);
    assert!(h.dragging(), "a pill press moved past 2 px starts a drag");

    let left = on_pill(&mut h, "t3", 0.2);
    h.drag_to(left);
    assert_eq!(mark(&mut h), Some(("t3".to_owned(), Side::Before)));

    let right = on_pill(&mut h, "t3", 0.8);
    h.drag_to(right);
    assert_eq!(mark(&mut h), Some(("t3".to_owned(), Side::After)));

    let other = on_pill(&mut h, "t2", 0.2);
    h.drag_to(other);
    assert_eq!(
        mark(&mut h),
        None,
        "before t2 is where t1 already sits: no mark"
    );

    h.release(other);
    assert!(!h.dragging());
    assert_eq!(mark(&mut h), None);
}

#[gpui::test]
fn pill_drop_that_keeps_the_order_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &three_tabs());
    h.sent();
    let from = on_pill(&mut h, "t1", 0.5);

    let itself = on_pill(&mut h, "t1", 0.8);
    let next = on_pill(&mut h, "t2", 0.2);
    for to in [itself, next] {
        h.start_drag(from);
        assert!(h.dragging(), "a pill press moved past 2 px starts a drag");
        h.drag_to(to);
        assert_eq!(mark(&mut h), None, "a drop here keeps the order");
        h.release(to);
        assert!(!h.dragging());
    }

    assert_eq!(reorders(&mut h), Vec::<Vec<String>>::new());
    assert_eq!(strip(&mut h), order(&["t1", "t2", "t3"]));
}

#[gpui::test]
fn removing_the_dragged_tab_ends_the_pill_drag(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &three_tabs());
    h.sent();
    let from = on_pill(&mut h, "t1", 0.5);
    let to = on_pill(&mut h, "t3", 0.8);
    h.start_drag(from);
    assert!(h.dragging(), "a pill press moved past 2 px starts a drag");
    h.drag_to(to);
    assert!(mark(&mut h).is_some(), "the drag is over t3");

    h.send(DaemonMessage::TabRemoved {
        tab_id: "t1".to_owned(),
    });

    assert!(!h.dragging(), "the dragged tab is gone, so is its drag");
    assert_eq!(mark(&mut h), None);
    h.release(to);
    assert_eq!(reorders(&mut h), Vec::<Vec<String>>::new());
    assert_eq!(strip(&mut h), order(&["t2", "t3"]));

    let from = on_pill(&mut h, "t2", 0.5);
    let to = on_pill(&mut h, "t3", 0.8);
    h.start_drag(from);
    assert!(h.dragging(), "a second pill drag is under way");
    h.drag_to(to);
    assert!(mark(&mut h).is_some(), "the drag is over t3");

    h.send(DaemonMessage::TabRemoved {
        tab_id: "t3".to_owned(),
    });

    assert!(h.dragging(), "the dragged tab is still there");
    assert_eq!(mark(&mut h), None, "the marked tab is gone, so is its mark");
    h.keys("escape");
    h.release(to);
    assert_eq!(reorders(&mut h), Vec::<Vec<String>>::new());
}

#[gpui::test]
fn esc_cancels_a_pill_drag(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &three_tabs());
    h.sent();
    let from = on_pill(&mut h, "t1", 0.5);
    let to = on_pill(&mut h, "t3", 0.8);
    h.start_drag(from);
    h.drag_to(to);
    assert!(mark(&mut h).is_some(), "the drag is over t3");

    h.keys("escape");

    assert!(!h.dragging(), "Esc ends the drag");
    assert_eq!(mark(&mut h), None);
    h.release(to);
    assert!(
        reorders(&mut h).is_empty(),
        "a cancelled drag sends nothing"
    );
    assert_eq!(strip(&mut h), order(&["t1", "t2", "t3"]));
}

#[gpui::test]
fn esc_cancels_a_drag_while_a_text_input_has_focus(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &three_tabs());
    h.sent();
    let renamed = on_pill(&mut h, "t1", 0.5);
    h.double_click(renamed);
    let renaming = |h: &mut Harness| h.root(|root, _| root.renaming_tab().map(str::to_owned));
    assert_eq!(
        renaming(&mut h),
        Some("t1".to_owned()),
        "t1's name input has the keyboard"
    );

    let from = on_pill(&mut h, "t2", 0.5);
    let to = on_pill(&mut h, "t3", 0.8);
    h.start_drag(from);
    h.drag_to(to);
    assert!(mark(&mut h).is_some(), "the drag is over t3");
    assert_eq!(
        renaming(&mut h),
        Some("t1".to_owned()),
        "the drag leaves the input open"
    );

    h.keys("escape");

    assert!(!h.dragging(), "Esc ends the drag over the input's own Esc");
    assert_eq!(mark(&mut h), None);
    assert_eq!(
        renaming(&mut h),
        Some("t1".to_owned()),
        "the Esc never reaches the input, so the rename stays open"
    );
    h.release(to);
    assert_eq!(reorders(&mut h), Vec::<Vec<String>>::new());
}

#[gpui::test]
fn pane_over_a_pill_activates_that_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &pane_tabs());
    h.sent();
    let from = header_point(&mut h, "p1");
    let over = on_pill(&mut h, "t2", 0.5);

    h.start_drag(from);
    assert!(h.dragging(), "the header drag is under way");
    h.drag_to(over);

    assert_eq!(
        active(&mut h),
        Some("t2".to_owned()),
        "hovering t2's pill shows it, so the pane can drop on a pane there"
    );
    assert!(h.dragging(), "the drag goes on in the newly shown tab");
    assert_eq!(mark(&mut h), None, "a pane shows no insertion mark");
    assert_eq!(
        overlay(&mut h),
        None,
        "the overlay waits for the next move over the new tab's grid"
    );
    let sent = h.sent();
    assert!(
        !sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::ReorderTabs { .. } | ClientMessage::MovePane { .. }
        )),
        "hovering a pill sends no move: {sent:?}"
    );
    assert_eq!(strip(&mut h), order(&["t1", "t2"]));
}

#[gpui::test]
fn pane_over_its_own_pill_does_not_reactivate(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &pane_tabs());
    h.sent();
    let from = header_point(&mut h, "p1");
    let over = on_pill(&mut h, "t1", 0.8);

    h.start_drag(from);
    h.drag_to(over);

    assert_eq!(active(&mut h), Some("t1".to_owned()), "t1 stays shown");
    assert!(h.dragging(), "the drag goes on");
    assert_eq!(mark(&mut h), None, "a pane shows no insertion mark");
    assert!(moves(&mut h).is_empty(), "no move is sent");
}

#[gpui::test]
fn pane_dropped_on_a_pill_moves_it_balanced_with_undo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &pane_tabs());
    h.sent();
    let from = header_point(&mut h, "p1");
    let over = on_pill(&mut h, "t2", 0.5);
    h.start_drag(from);
    h.drag_to(over);
    assert_eq!(
        active(&mut h),
        Some("t2".to_owned()),
        "the pill's tab shows"
    );

    h.release(over);

    let sent = moves(&mut h);
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::MovePane {
                src_tab_id,
                src_pane_id,
                dst_tab_id,
                dst_pane_id,
                edge,
                touch,
                request_id: Some(_),
            }] if src_tab_id == "t1"
                && src_pane_id == "p1"
                && dst_tab_id == "t2"
                && dst_pane_id == "p3"
                && (*edge == PaneDropEdge::Right || *edge == PaneDropEdge::Bottom)
                && !*touch
        ),
        "t2's two panes are equal, so the balanced target takes the first, p3, and splits it \
         across its longer side: sent {sent:?}"
    );
    assert_eq!(undo_messages(&mut h), ["Moved pane"]);
}

#[gpui::test]
fn pane_dropped_on_its_own_pill_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &pane_tabs());
    h.sent();
    let from = header_point(&mut h, "p1");
    let over = on_pill(&mut h, "t1", 0.8);
    h.start_drag(from);
    h.drag_to(over);

    h.release(over);

    assert!(moves(&mut h).is_empty(), "no move is sent");
    assert_eq!(undo_messages(&mut h), Vec::<String>::new());
}

#[gpui::test]
fn diff_pill_refuses_a_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_diff_tab());
    h.sent();
    let from = header_point(&mut h, "p1");
    let over = on_pill(&mut h, "t3", 0.5);
    h.start_drag(from);
    h.drag_to(over);

    assert_eq!(
        active(&mut h),
        Some("t1".to_owned()),
        "a diff pill does not show its tab"
    );
    assert!(
        h.root(|root, cx| root.pill_no_drop("t3", cx)),
        "the diff pill shows the no-drop state"
    );
    assert!(
        !h.root(|root, cx| root.pill_no_drop("t2", cx)),
        "a grid tab's pill takes a pane"
    );
    assert_eq!(overlay(&mut h), None);

    h.release(over);

    assert!(moves(&mut h).is_empty(), "no move is sent");
    assert_eq!(undo_messages(&mut h), Vec::<String>::new());
    assert!(
        !h.root(|root, cx| root.pill_no_drop("t3", cx)),
        "the no-drop state goes with the drag"
    );
}

#[gpui::test]
fn cross_tab_edge_drop_sends_move_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &pane_tabs());
    h.sent();
    let from = header_point(&mut h, "p1");
    let over = on_pill(&mut h, "t2", 0.5);
    h.start_drag(from);
    h.drag_to(over);
    assert_eq!(
        active(&mut h),
        Some("t2".to_owned()),
        "the pill's tab shows"
    );

    let to = at(&mut h, "p4", 0.8, 0.5);
    h.drag_to(to);
    assert_eq!(
        overlay(&mut h),
        Some(("p4".to_owned(), PaneDropEdge::Right)),
        "the shown tab's drop layer takes a pane from another tab"
    );

    h.release(to);

    let sent = moves(&mut h);
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::MovePane {
                src_tab_id,
                src_pane_id,
                dst_tab_id,
                dst_pane_id,
                edge: PaneDropEdge::Right,
                touch: true,
                request_id: Some(_),
            }] if src_tab_id == "t1"
                && src_pane_id == "p1"
                && dst_tab_id == "t2"
                && dst_pane_id == "p4"
        ),
        "sent {sent:?}"
    );
    assert_eq!(undo_messages(&mut h), ["Moved pane"]);
}

#[gpui::test]
fn cross_tab_centre_drop_sends_swap_panes_with_undo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &pane_tabs());
    h.sent();
    let from = header_point(&mut h, "p1");
    let over = on_pill(&mut h, "t2", 0.5);
    h.start_drag(from);
    h.drag_to(over);

    let centre = at(&mut h, "p4", 0.5, 0.5);
    h.drag_to(centre);
    assert_eq!(
        overlay(&mut h),
        Some(("p4".to_owned(), PaneDropEdge::Replace)),
        "the centre of another tab's pane takes the drop"
    );

    h.release(centre);

    let sent = h.sent();
    let swaps: Vec<&ClientMessage> = sent
        .iter()
        .filter(|msg| matches!(msg, ClientMessage::SwapPanes { .. }))
        .collect();
    assert!(
        matches!(
            swaps.as_slice(),
            [ClientMessage::SwapPanes {
                src_tab_id,
                src_pane_id,
                dst_tab_id,
                dst_pane_id,
                request_id: Some(_),
            }] if src_tab_id == "t1"
                && src_pane_id == "p1"
                && dst_tab_id == "t2"
                && dst_pane_id == "p4"
        ),
        "a cross-tab centre drop swaps the two panes' sessions: {sent:?}"
    );
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::MovePane { .. })),
        "a swap is not also a move: {sent:?}"
    );
    assert_eq!(undo_messages(&mut h), ["Swapped panes"]);

    let focus_panes: Vec<(String, Option<String>)> = h.root(|root, _| {
        root.undo_entries()
            .first()
            .map(|entry| {
                entry
                    .snapshots
                    .iter()
                    .map(|snapshot| (snapshot.tab.id.clone(), snapshot.focus_pane.clone()))
                    .collect()
            })
            .unwrap_or_default()
    });
    assert_eq!(
        focus_panes,
        [
            ("t1".to_owned(), Some("p1".to_owned())),
            ("t2".to_owned(), Some("p4".to_owned())),
        ],
        "each tab comes back focused on the pane its snapshot puts there"
    );

    let id = undo_id(&mut h);
    h.click_on(&format!("undo-action-{id}"));
    let restores = h.sent();
    assert!(
        matches!(
            restores.as_slice(),
            [ClientMessage::RestoreTabSnapshot { tab: first },
             ClientMessage::RestoreTabSnapshot { tab: second }]
            if first.id == "t1" && second.id == "t2"
        ),
        "one snapshot goes back per tab: {restores:?}"
    );
}

#[gpui::test]
fn a_cross_tab_swap_focuses_the_destination_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &pane_tabs());
    h.sent();
    let from = header_point(&mut h, "p1");
    let over = on_pill(&mut h, "t2", 0.5);
    h.start_drag(from);
    h.drag_to(over);
    let centre = at(&mut h, "p4", 0.5, 0.5);
    h.drag_to(centre);

    h.release(centre);

    assert_eq!(
        h.root(|root, _| root.focused_pane()),
        Some("p4".to_owned()),
        "the swapped-in session takes the keyboard in its new pane"
    );
}

/// Tabs `t1` (one pane) and `t2` (`p3` beside `p4`), every pane bound.
fn one_pane_beside_two() -> Fixture {
    Fixture {
        sessions: ["s1", "s3", "s4"]
            .iter()
            .map(|id| session(id).build())
            .collect(),
        tabs: vec![
            tab("t1", &pane("p1", Some("s1"))),
            tab(
                "t2",
                &split(
                    SplitDirection::Horizontal,
                    pane("p3", Some("s3")),
                    pane("p4", Some("s4")),
                ),
            ),
        ],
        ..Fixture::default()
    }
}

/// The id the swap among `sent` carries.
fn swap_request_id(sent: &[ClientMessage]) -> Option<String> {
    sent.iter().find_map(|msg| match msg {
        ClientMessage::SwapPanes { request_id, .. } => request_id.clone(),
        _ => None,
    })
}

#[gpui::test]
fn a_refused_swap_clears_the_in_flight_mark(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &one_pane_beside_two());
    h.sent();
    let from = header_point(&mut h, "p1");
    let over = on_pill(&mut h, "t2", 0.5);
    h.start_drag(from);
    h.drag_to(over);
    let centre = at(&mut h, "p4", 0.5, 0.5);
    h.drag_to(centre);
    h.release(centre);

    let sent = h.sent();
    let request_id = swap_request_id(&sent).expect("the swap carries a request id");
    let id = undo_id(&mut h);

    h.send(DaemonMessage::Error {
        message: "refused".to_owned(),
        request_id: Some(request_id),
        code: None,
    });
    h.sent();
    h.click_on(&format!("undo-action-{id}"));
    let restores = h.sent();
    assert!(
        matches!(
            restores.as_slice(),
            [ClientMessage::RestoreTabSnapshot { tab: first },
             ClientMessage::RestoreTabSnapshot { tab: second }]
            if first.id == "t1" && second.id == "t2"
        ),
        "a swap empties no tab, so neither is closing and both are replaced in place: \
         {restores:?}"
    );
}

#[gpui::test]
fn modal_release_clears_the_drop_overlay(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture {
        sessions: vec![session("s1").in_repo("r1").build(), session("s2").build()],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", Some("s2")),
            ),
        )],
        ..Fixture::default()
    };
    fixture.repos = vec![repo("r1", "C:/r1")];
    let mut h = Harness::with(cx, &dir, &fixture);
    let from = h.bounds("pane-title-p1").center();
    let p2 = h.bounds("pane-frame-p2");
    let over = point(p2.origin.x + p2.size.width * 0.8, p2.center().y);

    h.start_drag(from);
    h.drag_to(over);
    assert!(
        h.root(RootView::pane_drop_overlay).is_some(),
        "the drag is over p2"
    );

    h.send(DaemonMessage::VscodeWorkspaceSuggestion {
        repo_id: "r1".to_owned(),
        suggestion: VscodeWorkspaceSuggestion {
            source_path: "C:/work/team.code-workspace".to_owned(),
            suggested_name: "team".to_owned(),
            folders: vec![VscodeWorkspaceFolder {
                path: "C:/work/r1".to_owned(),
                name: None,
                matched_repo_id: Some("r1".to_owned()),
            }],
        },
    });
    assert!(h.root(|root, _| root.vscode_prompt_open()), "a modal is up");
    h.release(over);
    assert!(!h.dragging(), "the release ends the drag under the modal");
    h.keys("escape");
    assert!(!h.root(|root, _| root.vscode_prompt_open()));

    h.start_drag(from);
    assert!(h.dragging(), "a second drag is under way");
    assert_eq!(
        h.root(RootView::pane_drop_overlay),
        None,
        "the first drag's overlay is gone before the second reaches a pane"
    );
    h.keys("escape");
    h.release(from);
}

#[gpui::test]
fn a_remote_leaf_dropped_on_a_pill_lands_in_that_tab_with_its_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("a1").build()],
        tabs: vec![
            tab("t1", &pane("p1", Some("a1"))),
            tab("t2", &pane("p2", None)),
        ],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    let remote = Target::tailnet(REMOTE, None);
    let mut host = h.host(&remote);
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r2", "C:/r2")],
    });
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").in_repo("r2").build()],
    });
    host.sent();
    h.sent();

    let leaf = h.bounds("leaf-b1");
    assert!(leaf.size.height > Pixels::ZERO, "b1's leaf is on screen");
    let from = point(leaf.origin.x + leaf.size.width * 0.3, leaf.center().y);
    let to = on_pill(&mut h, "t2", 0.5);
    h.start_drag(from);
    assert!(h.dragging(), "b1's leaf drags");
    h.drag_to(to);
    h.release(to);

    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::ReplacePaneSession {
                tab_id,
                pane_id,
                session_id: Some(session),
                host: Some(host),
            }] if tab_id == "t2" && pane_id == "p2" && session == "b1" && host == REMOTE
        ),
        "t2's empty pane takes the remote session with its host: sent {sent:?}"
    );
    assert!(
        h.host(&remote).sent().is_empty(),
        "the layout is This PC's alone"
    );
    assert_eq!(active(&mut h).as_deref(), Some("t1"), "the shown tab stays");
}
