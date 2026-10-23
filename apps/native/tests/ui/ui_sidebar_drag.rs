//! Sidebar drag specs: workspace and repo containers reorder by their
//! header's half and send every shown one in the new order, SH, DIR and
//! Detached container rows neither drag nor take drops, tab containers and
//! pills drop on each other, leaves reorder within their own container only,
//! the local order holds until the daemon's broadcast, and Esc cancels. Any
//! leaf drags onto a pane, where the pane drag's overlay shows where it
//! lands, or onto a tab pill, which places it in that tab.

use crate::support;

use gpui::{Pixels, Point, TestAppContext, point, px};
use protocol::{
    ClientMessage, ContainerRef, DaemonMessage, PaneDropEdge, SplitDirection, SplitPlace,
};
use rustling_tulip_native::{RootView, Side, SidebarRow};
use support::{Fixture, Harness, TestDir, pane, repo, session, split, tab, workspace};

/// Workspace `w1` (session `u`, shown in no pane); repos `r1` (sessions
/// `a`, `b`) and `r2` (session `c`); the standalone shell `sh`, the folder
/// shell `d` and the detached session `x`. Tab `t1` shows `a` beside `b`;
/// tabs `t2` and `t3` are one empty pane each.
fn fixture() -> Fixture {
    Fixture {
        repos: vec![repo("r1", "D:\\r1"), repo("r2", "D:\\r2")],
        workspaces: vec![workspace("w1", &[])],
        sessions: vec![
            session("a").in_repo("r1").build(),
            session("b").in_repo("r1").build(),
            session("c").in_repo("r2").build(),
            session("sh").shell("C:\\elsewhere").build(),
            session("d").dir_shell("C:\\other\\proj").build(),
            session("x").in_repo("gone").build(),
            session("u").in_workspace("w1").build(),
        ],
        tabs: vec![
            tab(
                "t1",
                &split(
                    SplitDirection::Horizontal,
                    pane("p1", Some("a")),
                    pane("p2", Some("b")),
                ),
            ),
            tab("t2", &pane("p3", None)),
            tab("t3", &pane("p4", None)),
        ],
    }
}

/// The point a third of the way across the row tagged `selector` and `fy`
/// down it.
fn on_row(h: &mut Harness, selector: &str, fy: f32) -> Point<Pixels> {
    let row = h.bounds(selector);
    assert!(row.size.height > Pixels::ZERO, "{selector} is on screen");
    point(
        row.origin.x + row.size.width * 0.3,
        row.origin.y + row.size.height * fy,
    )
}

/// The point `fx` across `tab`'s pill, at its middle height.
fn on_pill(h: &mut Harness, tab: &str, fx: f32) -> Point<Pixels> {
    let pill = h.bounds(&format!("tab-{tab}"));
    assert!(pill.size.width > Pixels::ZERO, "{tab}'s pill is on screen");
    point(pill.origin.x + pill.size.width * fx, pill.center().y)
}

/// The shown containers' keys, in order.
fn shown(h: &mut Harness) -> Vec<String> {
    h.root(|root, _| {
        root.sidebar_containers()
            .iter()
            .map(|c| c.key.clone())
            .collect()
    })
}

/// The shown leaves of the container `key`, in order.
fn leaves(h: &mut Harness, key: &str) -> Vec<String> {
    let key = key.to_owned();
    h.root(move |root, _| {
        root.sidebar_containers()
            .iter()
            .find(|c| c.key == key)
            .map(|c| c.leaves.iter().map(|l| l.id.clone()).collect())
            .unwrap_or_default()
    })
}

/// The key of the first shown container whose key starts with `prefix`.
fn key_like(h: &mut Harness, prefix: &str) -> String {
    shown(h)
        .into_iter()
        .find(|key| key.starts_with(prefix))
        .expect("a container with that key")
}

/// The row the insertion line stands at, and the side of it.
fn mark(h: &mut Harness) -> Option<(SidebarRow, Side)> {
    h.root(RootView::sidebar_drop_mark)
}

/// `messages` as they go over the wire.
fn wire(messages: &[ClientMessage]) -> serde_json::Value {
    serde_json::to_value(messages).expect("client messages serialize")
}

/// The reorder messages among what the client sent since the last call.
fn reorders(h: &mut Harness) -> Vec<ClientMessage> {
    h.sent()
        .into_iter()
        .filter(|msg| {
            matches!(
                msg,
                ClientMessage::ReorderContainers { .. }
                    | ClientMessage::ReorderSessions { .. }
                    | ClientMessage::ReorderTabs { .. }
            )
        })
        .collect()
}

/// The tab orders among what the client sent since the last call.
fn tab_orders(h: &mut Harness) -> Vec<Vec<String>> {
    reorders(h)
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ReorderTabs { ordered_ids, .. } => Some(ordered_ids),
            _ => None,
        })
        .collect()
}

/// The point `fx` across and `fy` down `pane`'s card.
fn at(h: &mut Harness, pane: &str, fx: f32, fy: f32) -> Point<Pixels> {
    let frame = h.bounds(&format!("pane-frame-{pane}"));
    assert!(frame.origin.x >= px(0.0), "{pane} is on screen");
    point(
        frame.origin.x + frame.size.width * fx,
        frame.origin.y + frame.size.height * fy,
    )
}

fn undo_messages(h: &mut Harness) -> Vec<String> {
    h.root(|root, _| {
        root.undo_entries()
            .iter()
            .map(|entry| entry.message.clone())
            .collect()
    })
}

/// The pane the drop overlay is drawn on, and the edge it shows.
fn overlay(h: &mut Harness) -> Option<(String, PaneDropEdge)> {
    h.root(RootView::pane_drop_overlay)
}

/// The layout requests a leaf drop sends, among what the client sent since
/// the last call.
fn placements(h: &mut Harness) -> Vec<ClientMessage> {
    h.sent()
        .into_iter()
        .filter(|msg| {
            matches!(
                msg,
                ClientMessage::SplitPane { .. }
                    | ClientMessage::ReplacePaneSession { .. }
                    | ClientMessage::MovePane { .. }
                    | ClientMessage::SwapPanes { .. }
                    | ClientMessage::CreateTab { .. }
            )
        })
        .collect()
}

/// Drags the leaf `leaf` to `to`, checks the overlay shows `shown` there,
/// and drops it.
fn drop_leaf(h: &mut Harness, leaf: &str, to: Point<Pixels>, shown: (&str, PaneDropEdge)) {
    let from = on_row(h, &format!("leaf-{leaf}"), 0.5);
    h.start_drag(from);
    assert!(h.dragging(), "{leaf}'s leaf drags");
    h.drag_to(to);
    assert_eq!(
        overlay(h),
        Some((shown.0.to_owned(), shown.1)),
        "the overlay shows where {leaf} lands"
    );
    h.release(to);
    assert!(!h.dragging(), "the drop ends the drag");
    assert_eq!(overlay(h), None, "the overlay goes with the drop");
}

/// Asserts the only request sent split `p2` of `t1` at `place` of a
/// `direction` split with `u` in the new pane.
fn assert_split_p2(h: &mut Harness, direction: SplitDirection, place: SplitPlace) {
    let sent = placements(h);
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::SplitPane {
                tab_id,
                pane_id,
                direction: sent_direction,
                place: sent_place,
                new_session_id: Some(session),
                request_id: Some(_),
            }] if tab_id == "t1"
                && pane_id == "p2"
                && *sent_direction == direction
                && *sent_place == place
                && session == "u"
        ),
        "sent {sent:?}"
    );
}

fn header(key: &str) -> SidebarRow {
    SidebarRow::Container(key.to_owned())
}

fn ids(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| (*id).to_owned()).collect()
}

fn repo_ref(id: &str) -> ContainerRef {
    ContainerRef::Repo(id.to_owned())
}

fn workspace_ref(id: &str) -> ContainerRef {
    ContainerRef::Workspace(id.to_owned())
}

/// Drags the `r2` container onto the top half of the `w1` container and
/// holds it there.
fn drag_r2_over_w1(h: &mut Harness) -> Point<Pixels> {
    let from = on_row(h, "container-repo:r2", 0.5);
    let to = on_row(h, "container-ws:w1", 0.2);
    h.start_drag(from);
    assert!(h.dragging(), "a repo container drags");
    h.drag_to(to);
    to
}

#[gpui::test]
fn container_dropped_before_another_sends_the_full_order(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.sent();
    assert_eq!(&shown(&mut h)[..3], ["ws:w1", "repo:r1", "repo:r2"]);

    let to = drag_r2_over_w1(&mut h);
    assert_eq!(mark(&mut h), Some((header("ws:w1"), Side::Before)));
    let line = h.bounds("sidebar-drop-mark-ws:w1");
    assert!(line.size.width > Pixels::ZERO, "the line is drawn");
    h.release(to);

    assert_eq!(
        wire(&reorders(&mut h)),
        wire(&[ClientMessage::ReorderContainers {
            ordered: vec![repo_ref("r2"), workspace_ref("w1"), repo_ref("r1")],
        }]),
        "every shown workspace and repo, in the new order"
    );
    assert_eq!(
        &shown(&mut h)[..3],
        ["repo:r2", "ws:w1", "repo:r1"],
        "the new order shows at once"
    );
    assert_eq!(mark(&mut h), None, "the line goes with the drop");

    let from = on_row(&mut h, "container-repo:r1", 0.5);
    h.start_drag(from);
    assert!(h.dragging(), "a fresh drag starts");
    assert_eq!(mark(&mut h), None, "a fresh drag shows no stale line");
    h.release(from);
}

#[gpui::test]
fn shell_and_dir_containers_do_not_drag_but_their_leaves_do(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.sent();
    let still = [
        key_like(&mut h, "standalone:"),
        key_like(&mut h, "cwd:"),
        "detached".to_owned(),
    ];

    let from = on_row(&mut h, "leaf-d", 0.5);
    h.start_drag(from);
    assert!(h.dragging(), "a leaf of a DIR container drags");
    h.release(from);
    for key in &still {
        let from = on_row(&mut h, &format!("container-{key}"), 0.5);
        h.start_drag(from);
        assert!(!h.dragging(), "{key} does not drag");
        h.release(from);
    }

    for key in &still {
        let from = on_row(&mut h, "container-repo:r1", 0.5);
        let to = on_row(&mut h, &format!("container-{key}"), 0.2);
        h.start_drag(from);
        assert!(h.dragging(), "a repo container drags");
        h.drag_to(to);
        assert_eq!(mark(&mut h), None, "{key} takes no drop");
        h.release(to);
    }
    assert!(reorders(&mut h).is_empty(), "nothing reordered");
}

#[gpui::test]
fn tab_container_reorders_the_strip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.click_on("sidebar-view-tabs");
    h.sent();

    let from = on_row(&mut h, "container-tab:t3", 0.5);
    let to = on_row(&mut h, "container-tab:t1", 0.2);
    h.start_drag(from);
    h.drag_to(to);
    assert_eq!(mark(&mut h), Some((header("tab:t1"), Side::Before)));
    h.release(to);
    assert_eq!(tab_orders(&mut h), [ids(&["t3", "t1", "t2"])]);
    assert_eq!(h.root(|root, _| root.tab_ids()), ids(&["t3", "t1", "t2"]));
    assert_eq!(
        &shown(&mut h)[..3],
        ["tab:t3", "tab:t1", "tab:t2"],
        "the tab containers follow the strip"
    );

    let from = on_pill(&mut h, "t2", 0.5);
    let to = on_row(&mut h, "container-tab:t3", 0.2);
    h.drag_and_drop(from, to);
    assert_eq!(
        tab_orders(&mut h),
        [ids(&["t2", "t3", "t1"])],
        "a pill drops on a tab container"
    );

    let from = on_row(&mut h, "container-tab:t1", 0.5);
    let to = on_pill(&mut h, "t2", 0.2);
    h.start_drag(from);
    h.drag_to(to);
    assert_eq!(
        h.root(RootView::tab_drop_mark),
        Some(("t2".to_owned(), Side::Before)),
        "the pill marks a tab container dragged over it"
    );
    h.release(to);
    assert_eq!(
        tab_orders(&mut h),
        [ids(&["t1", "t2", "t3"])],
        "a tab container drops on a pill"
    );
}

#[gpui::test]
fn leaf_reorders_within_its_repo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.sent();
    assert_eq!(leaves(&mut h, "repo:r1"), ids(&["a", "b"]));

    let from = on_row(&mut h, "leaf-b", 0.5);
    let to = on_row(&mut h, "leaf-a", 0.2);
    h.start_drag(from);
    assert!(h.dragging(), "a repo's leaf drags");
    h.drag_to(to);
    assert!(mark(&mut h).is_some(), "the line stands at a");
    h.release(to);
    assert_eq!(
        wire(&reorders(&mut h)),
        wire(&[ClientMessage::ReorderSessions {
            container_id: "r1".to_owned(),
            ordered_ids: ids(&["b", "a"]),
        }])
    );
    assert_eq!(leaves(&mut h, "repo:r1"), ids(&["b", "a"]));

    h.click_on("sidebar-view-tabs");
    assert_eq!(leaves(&mut h, "tab:t1"), ids(&["a", "b"]), "pane order");
    let from = on_row(&mut h, "leaf-b", 0.5);
    let to = on_row(&mut h, "leaf-a", 0.2);
    h.drag_and_drop(from, to);
    assert_eq!(
        wire(&reorders(&mut h)),
        wire(&[ClientMessage::ReorderSessions {
            container_id: "t1".to_owned(),
            ordered_ids: ids(&["b", "a"]),
        }]),
        "a tab container's leaves reorder too"
    );
    assert_eq!(leaves(&mut h, "tab:t1"), ids(&["b", "a"]));
}

#[gpui::test]
fn leaf_dropped_in_another_container_does_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.sent();

    let from = on_row(&mut h, "leaf-a", 0.5);
    let to = on_row(&mut h, "leaf-c", 0.2);
    h.start_drag(from);
    assert!(h.dragging(), "a repo's leaf drags");
    h.drag_to(to);
    assert_eq!(mark(&mut h), None, "no line in another container");
    h.release(to);

    assert!(reorders(&mut h).is_empty());
    assert_eq!(leaves(&mut h, "repo:r1"), ids(&["a", "b"]));
    assert_eq!(leaves(&mut h, "repo:r2"), ids(&["c"]));
}

#[gpui::test]
fn leaf_dropped_on_a_pane_edge_places_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.sent();

    let to = at(&mut h, "p2", 0.1, 0.5);
    drop_leaf(&mut h, "u", to, ("p2", PaneDropEdge::Left));
    assert_split_p2(&mut h, SplitDirection::Horizontal, SplitPlace::First);
    assert!(
        undo_messages(&mut h).is_empty(),
        "placing a session records no undo"
    );

    let to = at(&mut h, "p2", 0.5, 0.5);
    drop_leaf(&mut h, "u", to, ("p2", PaneDropEdge::Replace));
    let sent = placements(&mut h);
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::ReplacePaneSession {
                tab_id,
                pane_id,
                session_id: Some(session),
            }] if tab_id == "t1" && pane_id == "p2" && session == "u"
        ),
        "the centre replaces what the pane shows: sent {sent:?}"
    );

    let to = at(&mut h, "p2", 0.97, 0.5);
    drop_leaf(&mut h, "u", to, ("p2", PaneDropEdge::Right));
    assert_split_p2(&mut h, SplitDirection::Horizontal, SplitPlace::Second);
    assert_eq!(undo_messages(&mut h), Vec::<String>::new());

    let to = at(&mut h, "p2", 0.8, 0.5);
    drop_leaf(&mut h, "a", to, ("p2", PaneDropEdge::Right));
    let sent = placements(&mut h);
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::MovePane {
                src_tab_id,
                src_pane_id,
                dst_tab_id,
                dst_pane_id,
                edge: PaneDropEdge::Right,
                request_id: Some(_),
                ..
            }] if src_tab_id == "t1"
                && src_pane_id == "p1"
                && dst_tab_id == "t1"
                && dst_pane_id == "p2"
        ),
        "a bound leaf moves its pane: sent {sent:?}"
    );
    assert_eq!(undo_messages(&mut h), ["Moved pane"]);
}

#[gpui::test]
fn leaf_dropped_on_a_pill_places_it_in_that_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.sent();
    let shown = h.root(|root, _| root.active_tab_id().map(str::to_owned));
    assert_eq!(shown.as_deref(), Some("t1"));

    let from = on_row(&mut h, "leaf-u", 0.5);
    let to = on_pill(&mut h, "t2", 0.5);
    h.start_drag(from);
    h.drag_to(to);
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        shown,
        "a leaf over a pill does not show that tab"
    );
    h.release(to);

    let sent = placements(&mut h);
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::ReplacePaneSession {
                tab_id,
                pane_id,
                session_id: Some(session),
            }] if tab_id == "t2" && pane_id == "p3" && session == "u"
        ),
        "the leaf fills t2's empty pane: sent {sent:?}"
    );
    assert!(!h.dragging());
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        shown,
        "the drop does not change the shown tab"
    );
    assert_eq!(undo_messages(&mut h), Vec::<String>::new());
}

#[gpui::test]
fn a_leaf_released_off_any_target_leaves_no_stale_drag(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.click_on("sidebar-view-tabs");
    h.sent();

    let from = on_row(&mut h, "leaf-d", 0.5);
    let off = h.bounds("footer-status").center();
    h.start_drag(from);
    assert!(h.dragging(), "d's leaf drags");
    h.drag_to(off);
    h.release(off);
    assert!(!h.dragging(), "the release ends the drag");
    assert!(placements(&mut h).is_empty(), "nothing takes the drop");

    let from = on_pill(&mut h, "t3", 0.5);
    let to = on_row(&mut h, "container-tab:t1", 0.2);
    h.start_drag(from);
    h.drag_to(to);
    assert_eq!(
        mark(&mut h),
        Some((header("tab:t1"), Side::Before)),
        "a later pill drag is its own drag, not the leaf's"
    );
    h.release(to);
    assert_eq!(tab_orders(&mut h), [ids(&["t3", "t1", "t2"])]);
}

#[gpui::test]
fn local_order_holds_until_the_echo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.sent();

    let to = drag_r2_over_w1(&mut h);
    h.release(to);
    assert_eq!(&shown(&mut h)[..3], ["repo:r2", "ws:w1", "repo:r1"]);

    h.send(DaemonMessage::SessionUpdated {
        session: session("a").in_repo("r1").status("working").build(),
        request_id: None,
    });
    assert_eq!(
        &shown(&mut h)[..3],
        ["repo:r2", "ws:w1", "repo:r1"],
        "an unrelated message leaves the local order"
    );

    h.send(DaemonMessage::ContainersReordered {
        ordered: vec![repo_ref("r2"), workspace_ref("w1"), repo_ref("r1")],
    });
    assert_eq!(
        &shown(&mut h)[..3],
        ["repo:r2", "ws:w1", "repo:r1"],
        "the echo of the same order changes nothing"
    );

    h.send(DaemonMessage::ContainersReordered {
        ordered: vec![repo_ref("r1"), workspace_ref("w1")],
    });
    assert_eq!(
        &shown(&mut h)[..3],
        ["repo:r1", "ws:w1", "repo:r2"],
        "a broadcast that disagrees wins"
    );
}

#[gpui::test]
fn esc_cancels_a_sidebar_drag(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &fixture());
    h.sent();

    let to = drag_r2_over_w1(&mut h);
    assert!(mark(&mut h).is_some(), "the drag is over w1");

    h.keys("escape");

    assert!(!h.dragging(), "Esc ends the drag");
    assert_eq!(mark(&mut h), None);
    h.release(to);
    assert!(
        reorders(&mut h).is_empty(),
        "a cancelled drag sends nothing"
    );
    assert_eq!(&shown(&mut h)[..3], ["ws:w1", "repo:r1", "repo:r2"]);

    let from = on_row(&mut h, "container-repo:r1", 0.5);
    h.start_drag(from);
    assert!(h.dragging(), "a fresh drag starts");
    assert_eq!(mark(&mut h), None, "a fresh drag shows no stale line");
    h.release(from);
}
