//! Pane drag-and-drop specs: a pane dragged by its header onto a pane of the
//! shown tab, the overlay that shows where it lands, and every way a drag
//! ends without a move.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::{ExternalPaths, FileDropEvent, Modifiers, Pixels, Point, TestAppContext, point, px};
use protocol::{ClientMessage, DaemonMessage, PaneDropEdge, SplitDirection};
use rustling_tulip_native::RootView;
use support::{Fixture, Harness, TestDir, pane, session, split, tab};

/// Tab `t1`: `p1` showing `s1` beside `p2` showing `s2`.
fn pair() -> Fixture {
    Fixture {
        sessions: vec![session("s1").build(), session("s2").build()],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", Some("s2")),
            ),
        )],
        ..Fixture::default()
    }
}

/// Tab `t1`: `p1` beside `p2` stacked over `p3`.
fn trio() -> Fixture {
    Fixture {
        sessions: ["s1", "s2", "s3"]
            .iter()
            .map(|id| session(id).build())
            .collect(),
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                split(
                    SplitDirection::Vertical,
                    pane("p2", Some("s2")),
                    pane("p3", Some("s3")),
                ),
            ),
        )],
        ..Fixture::default()
    }
}

/// [`pair`] in tab `t1`, shown, and tab `t2`: `p3` showing `s3` beside `p4`
/// showing `s4`.
fn two_tabs() -> Fixture {
    let mut fixture = pair();
    fixture
        .sessions
        .extend([session("s3").build(), session("s4").build()]);
    fixture.tabs.push(tab(
        "t2",
        &split(
            SplitDirection::Horizontal,
            pane("p3", Some("s3")),
            pane("p4", Some("s4")),
        ),
    ));
    fixture
}

/// The fixture with every terminal attached and `p1`'s terminal holding the
/// keyboard.
fn attached<'a>(cx: &'a mut TestAppContext, dir: &TestDir, fixture: &Fixture) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, fixture);
    for session in &fixture.sessions {
        h.answer_scrollback(&session.id, b"");
    }
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());
    h.sent();
    h
}

/// A point on `pane`'s header, over its title, where a drag starts.
fn header_point(h: &mut Harness, pane: &str) -> Point<Pixels> {
    let title = h.bounds(&format!("pane-title-{pane}"));
    assert!(title.origin.x >= px(0.0), "{pane}'s title is on screen");
    title.center()
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

/// The pane the drop overlay is drawn on, and the edge it shows.
fn overlay(h: &mut Harness) -> Option<(String, PaneDropEdge)> {
    h.root(RootView::pane_drop_overlay)
}

/// Asserts the only move sent took `p1` of `t1` onto `edge` of `dst` there.
fn assert_moved_onto(h: &mut Harness, dst: &str, edge: PaneDropEdge) {
    let sent = moves(h);
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::MovePane {
                src_tab_id,
                src_pane_id,
                dst_tab_id,
                dst_pane_id,
                edge: sent_edge,
                request_id: Some(_),
            }] if src_tab_id == "t1"
                && src_pane_id == "p1"
                && dst_tab_id == "t1"
                && dst_pane_id == dst
                && *sent_edge == edge
        ),
        "sent {sent:?}"
    );
}

#[gpui::test]
fn dragging_a_header_onto_a_pane_half_sends_move_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &pair());
    let from = header_point(&mut h, "p1");
    let to = at(&mut h, "p2", 0.8, 0.5);

    h.drag_and_drop(from, to);

    assert_moved_onto(&mut h, "p2", PaneDropEdge::Right);
    assert_eq!(undo_messages(&mut h), ["Moved pane"]);
}

#[gpui::test]
fn centre_drop_swaps_panes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &pair());
    let from = header_point(&mut h, "p1");
    let to = at(&mut h, "p2", 0.5, 0.6);

    h.drag_and_drop(from, to);

    assert_moved_onto(&mut h, "p2", PaneDropEdge::Replace);
    assert_eq!(undo_messages(&mut h), ["Swapped panes"]);
}

#[gpui::test]
fn outer_band_drop_splits_at_the_top_level(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &trio());
    let from = header_point(&mut h, "p1");
    let to = at(&mut h, "p3", 0.5, 0.95);

    h.drag_and_drop(from, to);

    assert_moved_onto(&mut h, "p3", PaneDropEdge::OuterBottom);
    assert_eq!(undo_messages(&mut h), ["Moved pane"]);
}

#[gpui::test]
fn overlay_follows_the_pointer_and_clears_on_release(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &pair());
    let from = header_point(&mut h, "p1");
    let frame = h.bounds("pane-frame-p2");

    h.start_drag(from);
    assert!(h.dragging(), "a header press moved past 2 px starts a drag");
    assert_eq!(overlay(&mut h), None, "the drag has not reached a pane yet");

    let right = at(&mut h, "p2", 0.8, 0.5);
    h.drag_to(right);
    assert_eq!(
        overlay(&mut h),
        Some(("p2".to_owned(), PaneDropEdge::Right))
    );
    let tint = h.bounds("pane-drop-overlay-p2");
    let near = |a: Pixels, b: Pixels| (a - b).abs() <= px(2.0);
    assert!(
        near(tint.origin.x, frame.center().x) && near(tint.size.width, frame.size.width / 2.0),
        "the right half is tinted: {tint:?} in {frame:?}"
    );

    let centre = at(&mut h, "p2", 0.5, 0.5);
    h.drag_to(centre);
    assert_eq!(
        overlay(&mut h),
        Some(("p2".to_owned(), PaneDropEdge::Replace))
    );

    h.release(centre);
    assert!(!h.dragging());
    assert_eq!(overlay(&mut h), None, "the overlay goes with the release");
}

#[gpui::test]
fn esc_cancels_the_drag(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &pair());
    let from = header_point(&mut h, "p1");
    let to = at(&mut h, "p2", 0.8, 0.5);
    h.start_drag(from);
    h.drag_to(to);
    assert!(overlay(&mut h).is_some(), "the drag is over p2");

    h.keys("escape");

    assert!(!h.dragging(), "Esc ends the drag");
    assert_eq!(overlay(&mut h), None);
    assert!(
        h.sent_input("s1").is_empty(),
        "the Esc never reaches the focused terminal"
    );
    h.release(to);
    assert!(moves(&mut h).is_empty(), "a cancelled drag sends nothing");
    assert_eq!(undo_messages(&mut h), Vec::<String>::new());
}

#[gpui::test]
fn self_drop_shows_no_overlay_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &pair());
    let from = header_point(&mut h, "p1");
    let to = at(&mut h, "p1", 0.5, 0.6);

    h.start_drag(from);
    h.drag_to(to);
    assert!(h.dragging());
    assert_eq!(overlay(&mut h), None, "a pane cannot drop on itself");

    h.release(to);
    assert!(moves(&mut h).is_empty());
    assert_eq!(undo_messages(&mut h), Vec::<String>::new());
}

#[gpui::test]
fn release_outside_any_pane_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &pair());
    let from = header_point(&mut h, "p1");
    let over_p2 = at(&mut h, "p2", 0.8, 0.5);
    let outside = point(px(2.0), px(2.0));

    h.start_drag(from);
    h.drag_to(over_p2);
    assert!(overlay(&mut h).is_some(), "the drag is over p2");
    h.drag_to(outside);
    assert_eq!(overlay(&mut h), None, "leaving the pane clears its overlay");

    h.release(outside);
    assert!(!h.dragging());
    assert!(moves(&mut h).is_empty());
    assert_eq!(undo_messages(&mut h), Vec::<String>::new());
}

#[gpui::test]
fn header_button_press_never_starts_a_drag(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &pair());

    for button in ["split-right-p1", "close-pane-p1"] {
        let press = h.center(button);
        h.start_drag(press);
        assert!(!h.dragging(), "a wobbling press on {button} is not a drag");
        h.release(point(press.x + px(6.0), press.y));
        h.keys("escape");
    }

    let from = header_point(&mut h, "p1");
    h.start_drag(from);
    assert!(h.dragging(), "the header around the buttons drags");
    h.keys("escape");
    h.release(from);
    assert!(moves(&mut h).is_empty());
}

#[gpui::test]
fn tabs_change_mid_drag_ends_the_drag(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = pair();
    let mut h = attached(cx, &dir, &fixture);
    let from = header_point(&mut h, "p1");
    let to = at(&mut h, "p2", 0.8, 0.5);
    h.start_drag(from);
    h.drag_to(to);
    assert!(overlay(&mut h).is_some(), "the drag is over p2");

    let shown = fixture.tabs.first().expect("the fixture's tab").clone();
    h.send(DaemonMessage::TabUpdated { tab: shown });

    assert!(!h.dragging(), "a tab change ends the drag");
    assert_eq!(overlay(&mut h), None);
    h.release(to);
    assert!(moves(&mut h).is_empty());
}

#[gpui::test]
fn pane_drag_does_not_forward_mouse_to_the_terminal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &pair());
    h.pty("s2", b"\x1b[?1003h\x1b[?1006h");
    h.sent();
    let from = header_point(&mut h, "p1");
    let first = at(&mut h, "p2", 0.3, 0.6);
    let second = at(&mut h, "p2", 0.6, 0.8);

    h.start_drag(from);
    h.drag_to(first);
    h.drag_to(second);
    assert!(
        h.dragging(),
        "the pane drag is under way over p2's terminal"
    );
    h.release(second);

    assert!(
        h.sent_input("s2").is_empty(),
        "no mouse report reaches p2's program during a pane drag"
    );
}

#[gpui::test]
fn the_ghost_sits_at_the_pointer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &pair());
    let header = h.bounds("pane-header-p1");
    let title = h.bounds("pane-title-p1");
    let stop = h.bounds("pane-stop-p1");
    let from = point((title.right() + stop.left()) / 2.0, title.center().y);
    assert!(
        from.x - header.left() > px(100.0),
        "the press is far into the header: {from:?} in {header:?}"
    );
    let to = at(&mut h, "p2", 0.5, 0.8);

    h.start_drag(from);
    assert!(h.dragging(), "the stretch between title and Stop drags");
    h.drag_to(to);

    let ghost = h.bounds("pane-drag-ghost");
    let near = |a: Pixels, b: Pixels| (a - b).abs() <= px(4.0);
    assert!(
        near(ghost.origin.x, to.x) && near(ghost.origin.y, to.y),
        "the ghost {ghost:?} starts at the pointer {to:?}"
    );
    h.release(to);
}

#[gpui::test]
fn esc_and_tab_updates_leave_a_file_drag_alone(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = pair();
    let mut h = attached(cx, &dir, &fixture);
    let over = at(&mut h, "p1", 0.5, 0.6);
    h.cx.simulate_event(FileDropEvent::Entered {
        position: over,
        paths: ExternalPaths::default(),
    });
    assert!(h.dragging(), "files dragged in from the OS are a gpui drag");

    h.keys("escape");
    assert!(h.dragging(), "Esc leaves a drag that is not a pane's");
    assert!(
        !h.sent_input("s1").is_empty(),
        "the Esc reaches the focused terminal"
    );

    let shown = fixture.tabs.first().expect("the fixture's tab").clone();
    h.send(DaemonMessage::TabUpdated { tab: shown });
    assert!(
        h.dragging(),
        "a tab change leaves a drag that is not a pane's"
    );
    assert_eq!(overlay(&mut h), None);
    h.cx.simulate_event(FileDropEvent::Exited);
}

#[gpui::test]
fn only_a_change_to_the_source_or_shown_tab_ends_the_drag(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = two_tabs();
    let mut h = attached(cx, &dir, &fixture);
    let from = header_point(&mut h, "p1");
    let to = at(&mut h, "p2", 0.8, 0.5);
    h.start_drag(from);
    h.drag_to(to);
    assert!(overlay(&mut h).is_some(), "the drag is over p2");

    let other = fixture.tabs.get(1).expect("the fixture's t2").clone();
    h.send(DaemonMessage::TabUpdated { tab: other });
    assert!(h.dragging(), "an update to another tab keeps the drag");
    h.send(DaemonMessage::TabRemoved {
        tab_id: "t2".to_owned(),
    });
    assert!(h.dragging(), "removing another tab keeps the drag");
    let lower = at(&mut h, "p2", 0.8, 0.6);
    h.drag_to(lower);
    assert_eq!(
        overlay(&mut h),
        Some(("p2".to_owned(), PaneDropEdge::Right))
    );

    let source = fixture.tabs.first().expect("the fixture's t1").clone();
    h.send(DaemonMessage::TabUpdated { tab: source });
    assert!(!h.dragging(), "an update to the source tab ends the drag");
    assert_eq!(overlay(&mut h), None);
    h.release(lower);
    assert!(moves(&mut h).is_empty());
}

#[gpui::test]
fn a_drop_in_another_tab_than_the_source_is_refused(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &two_tabs());
    let from = header_point(&mut h, "p1");
    h.start_drag(from);

    h.keys("ctrl-tab");
    let to = at(&mut h, "p4", 0.8, 0.5);
    h.drag_to(to);
    assert!(h.dragging(), "the drag goes on in the newly shown tab");
    assert_eq!(overlay(&mut h), None, "a pane drops only in its own tab");

    h.release(to);
    let sent = moves(&mut h);
    assert!(sent.is_empty(), "sent {sent:?}");
    assert_eq!(undo_messages(&mut h), Vec::<String>::new());
}

#[gpui::test]
fn a_press_on_a_chip_stop_or_exit_code_never_starts_a_drag(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![
            session("s1").trusted().build(),
            session("s2").exited(1).build(),
        ],
        ..pair()
    };
    let mut h = attached(cx, &dir, &fixture);

    for control in ["pane-chip-p1-trusted", "pane-stop-p1", "exit-code-p2"] {
        let bounds = h.bounds(control);
        assert!(bounds.origin.x >= px(0.0), "{control} is on screen");
        let press = bounds.center();
        h.start_drag(press);
        assert!(!h.dragging(), "a wobbling press on {control} is not a drag");
        h.release(point(press.x + px(6.0), press.y));
        h.keys("escape");
    }
    assert!(moves(&mut h).is_empty());
}

#[gpui::test]
fn a_drop_on_the_facing_half_of_its_sibling_shows_nothing_and_sends_nothing(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = attached(cx, &dir, &pair());
    let from = header_point(&mut h, "p1");
    let facing = at(&mut h, "p2", 0.2, 0.5);

    h.start_drag(from);
    h.drag_to(facing);
    assert!(h.dragging());
    assert_eq!(
        overlay(&mut h),
        None,
        "p1 dropped on p2's left half would land where it is"
    );

    h.release(facing);
    assert!(moves(&mut h).is_empty());
    assert_eq!(undo_messages(&mut h), Vec::<String>::new());
}
