//! Tab pill drag-and-drop specs: a pill dragged onto another pill's half
//! reorders the strip at once and sends the new order, the insertion mark
//! that shows where it lands, and every way a pill drag ends without a
//! reorder.

use crate::support;

use gpui::{Pixels, Point, TestAppContext, point};
use protocol::{
    ClientMessage, DaemonMessage, SplitDirection, VscodeWorkspaceFolder, VscodeWorkspaceSuggestion,
};
use rustling_tulip_native::{RootView, Side};
use support::{Fixture, Harness, TestDir, pane, repo, session, split, tab};

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
            ClientMessage::ReorderTabs { ordered_ids } => Some(ordered_ids),
            _ => None,
        })
        .collect()
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
fn pane_drag_over_a_pill_does_nothing_yet(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build(), session("s2").build()],
        tabs: vec![
            tab(
                "t1",
                &split(
                    SplitDirection::Horizontal,
                    pane("p1", Some("s1")),
                    pane("p2", Some("s2")),
                ),
            ),
            tab("t2", &pane("p3", None)),
        ],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();
    let from = h.bounds("pane-title-p1").center();
    let over = on_pill(&mut h, "t2", 0.8);

    h.start_drag(from);
    assert!(h.dragging(), "the header drag is under way");
    h.drag_to(over);
    assert_eq!(mark(&mut h), None, "a pane shows no insertion mark");
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("t1".to_owned()),
        "hovering t2's pill does not show it"
    );
    h.release(over);

    let sent = h.sent();
    assert!(
        !sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::ReorderTabs { .. } | ClientMessage::MovePane { .. }
        )),
        "a pane dropped on a pill sends nothing: {sent:?}"
    );
    assert_eq!(strip(&mut h), order(&["t1", "t2"]));
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
