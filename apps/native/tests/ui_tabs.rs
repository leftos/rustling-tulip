//! Tab and split-pane specs: layout init, new tab, split, close, rename,
//! divider drags, detach, resize and smart placement.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use gpui::{Modifiers, MouseButton, TestAppContext, point, px};
use protocol::{ClientMessage, DaemonMessage, InitLayoutKind, SplitDirection, SplitPlace};
use support::{Fixture, Harness, TestDir, pane, session, split, tab};

fn two_tabs() -> Fixture {
    let mut fixture = Fixture::single(session("s1").build());
    fixture.tabs.push(tab("t2", &pane("p2", None)));
    fixture
}

#[gpui::test]
fn layout_init_required_is_answered_with_all_sessions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    h.send(DaemonMessage::LayoutInitRequired {
        has_legacy: false,
        active_session_count: 0,
        clonable: Vec::new(),
    });
    let sent = h.sent();
    assert!(
        sent.iter().any(|m| matches!(
            m,
            ClientMessage::InitLayout {
                kind: InitLayoutKind::AllSessions
            }
        )),
        "sent {sent:?}"
    );
}

#[gpui::test]
fn new_tab_button_creates_a_tab_that_becomes_active(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.sent();
    h.click_on("new-tab");
    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::CreateTab { name: None, .. }]
        ),
        "sent {sent:?}"
    );
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t2", &pane("p2", None)),
    });
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("t2".to_owned())
    );
}

#[gpui::test]
fn split_right_sends_a_horizontal_split_and_shift_places_first(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.sent();
    let split_place = |sent: Vec<ClientMessage>| match sent.as_slice() {
        [
            ClientMessage::SplitPane {
                direction: SplitDirection::Horizontal,
                place,
                pane_id,
                ..
            },
        ] if pane_id == "p1" => Some(*place),
        _ => None,
    };

    let at = h.center("split-right-p1");
    h.click(at, Modifiers::none());
    assert_eq!(split_place(h.sent()), Some(SplitPlace::Second));
    h.click(at, Modifiers::shift());
    assert_eq!(split_place(h.sent()), Some(SplitPlace::First));
}

#[gpui::test]
fn tab_close_arms_first_and_esc_disarms_and_middle_click_closes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_tabs());
    h.answer_scrollback("s1", b"");
    let cell = h.cell_center("p1", 0, 0);
    h.click(cell, Modifiers::none());
    h.sent();
    let closes = |sent: &[ClientMessage], id: &str| {
        sent.iter()
            .filter(|m| matches!(m, ClientMessage::CloseTab { tab_id } if tab_id == id))
            .count()
    };

    h.click_on("tab-close-t1");
    assert!(h.sent().is_empty(), "the first click only arms");
    h.keys("escape");
    h.click_on("tab-close-t1");
    assert_eq!(
        closes(&h.sent(), "t1"),
        0,
        "Esc disarmed, so this click arms again"
    );
    h.click_on("tab-close-t1");
    assert_eq!(closes(&h.sent(), "t1"), 1, "the second click closes");

    let pill = h.center("tab-t2");
    h.cx.simulate_mouse_down(pill, MouseButton::Middle, Modifiers::none());
    h.cx.simulate_mouse_up(pill, MouseButton::Middle, Modifiers::none());
    assert_eq!(
        closes(&h.sent(), "t2"),
        1,
        "middle-click closes an empty tab"
    );
}

#[gpui::test]
fn double_click_renames_a_tab_and_esc_or_blank_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_tabs());
    h.sent();
    let pill = h.center("tab-t1");

    h.double_click(pill);
    h.cx.simulate_input("work");
    h.keys("enter");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::RenameTab { tab_id, name }] if tab_id == "t1" && name == "work"),
        "sent {sent:?}"
    );

    let renames = |sent: &[ClientMessage]| {
        sent.iter()
            .filter(|m| matches!(m, ClientMessage::RenameTab { .. }))
            .count()
    };
    h.double_click(pill);
    assert_eq!(
        h.root(|root, _| root.renaming_tab().map(str::to_owned)),
        Some("t1".to_owned())
    );
    h.cx.simulate_input("zzz");
    h.keys("escape");
    assert_eq!(
        h.root(|root, _| root.renaming_tab().map(str::to_owned)),
        None,
        "Esc closes the editor"
    );
    h.keys("enter");
    assert_eq!(
        renames(&h.sent()),
        0,
        "Esc cancels: a later Enter renames nothing"
    );

    h.double_click(pill);
    h.keys("backspace enter");
    assert!(h.sent().is_empty(), "a blank name is not sent");
}

#[gpui::test]
fn dragging_a_split_divider_sends_one_ratio_on_release(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        split(SplitDirection::Vertical, pane("p2", None), pane("p3", None)),
    );
    let fixture = Fixture {
        sessions: vec![session("s1").build()],
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();

    let from = h.center("divider-t1-[1]");
    h.cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    for dy in [20.0, 40.0, 60.0] {
        let to = point(from.x, from.y + px(dy));
        h.cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
    }
    assert!(h.sent().is_empty(), "nothing while dragging");
    h.cx.simulate_mouse_up(
        point(from.x, from.y + px(60.0)),
        MouseButton::Left,
        Modifiers::none(),
    );

    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::SetPaneRatio { tab_id, split_path, ratio }]
            if tab_id == "t1" && split_path == &[1] && *ratio > 0.5),
        "sent {sent:?}"
    );
}

#[gpui::test]
fn same_session_in_two_panes_detaches_only_with_the_last(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        pane("p2", Some("s1")),
    );
    let fixture = Fixture {
        sessions: vec![session("s1").build()],
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();
    let detaches = |sent: &[ClientMessage]| {
        sent.iter()
            .filter(|m| matches!(m, ClientMessage::Detach { session_id } if session_id == "s1"))
            .count()
    };

    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &pane("p1", Some("s1"))),
    });
    assert_eq!(detaches(&h.sent()), 0, "p1 still shows s1");
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &pane("p1", None)),
    });
    assert_eq!(detaches(&h.sent()), 1);
}

#[gpui::test]
fn pane_in_an_inactive_tab_resizes_only_once_shown(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build(), session("s2").build()],
        tabs: vec![
            tab("t1", &pane("p1", Some("s1"))),
            tab("t2", &pane("p2", Some("s2"))),
        ],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    let resizes = |sent: &[ClientMessage], id: &str| {
        sent.iter()
            .filter(|m| matches!(m, ClientMessage::Resize { session_id, .. } if session_id == id))
            .count()
    };
    let sent = h.sent();
    assert!(
        resizes(&sent, "s1") >= 1,
        "the active tab's pane sizes its PTY"
    );
    assert_eq!(resizes(&sent, "s2"), 0, "the hidden pane does not");

    h.click_on("tab-t2");
    assert_eq!(resizes(&h.sent(), "s2"), 1, "shown, it sends its size once");
}

#[gpui::test]
fn sidebar_click_on_an_unbound_session_fills_the_empty_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build()],
        tabs: vec![tab("t1", &pane("p1", None))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();
    h.click_on("leaf-s1");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ReplacePaneSession { tab_id, pane_id, session_id }]
            if tab_id == "t1" && pane_id == "p1" && session_id.as_deref() == Some("s1")),
        "sent {sent:?}"
    );
}
