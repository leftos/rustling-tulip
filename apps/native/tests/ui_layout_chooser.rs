//! First-connect layout chooser specs: what each option sends, the
//! arrangement applied to the daemon's answer, and the keys and clicks the
//! chooser keeps from everything behind it.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use gpui::{Modifiers, TestAppContext};
use protocol::{
    ClientMessage, ClonableLayout, DaemonMessage, InitLayoutKind, RearrangeLayout, SplitDirection,
};
use support::{Fixture, Harness, TestDir, pane, session, split, tab};

fn init_required(count: u32, clonable: Vec<ClonableLayout>) -> DaemonMessage {
    DaemonMessage::LayoutInitRequired {
        has_legacy: false,
        active_session_count: count,
        clonable,
    }
}

fn chooser_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.layout_chooser_open())
}

#[gpui::test]
fn layout_init_required_opens_the_chooser_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    h.sent();
    h.send(init_required(2, Vec::new()));
    assert!(chooser_open(&mut h));
    assert!(h.sent().is_empty(), "the chooser waits for a choice");
    let labels: Vec<String> = h.root(|root, _| {
        root.layout_chooser_controls()
            .into_iter()
            .map(|(_, label)| label)
            .collect()
    });
    assert_eq!(labels, ["Start empty", "Open all active sessions"]);
}

#[gpui::test]
fn start_empty_sends_empty_and_closes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    h.send(init_required(2, Vec::new()));
    h.sent();
    h.keys("enter");
    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::InitLayout {
                kind: InitLayoutKind::Empty
            }]
        ),
        "Start empty has the focus: {sent:?}"
    );
    assert!(!chooser_open(&mut h));
}

#[gpui::test]
fn open_all_grid_with_max_extracts_then_rearranges_on_tabs(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    h.send(init_required(5, Vec::new()));
    h.sent();
    h.click_on("layout-choose-all");
    h.click_on("chooser-grid-shape-2");
    h.click_on("chooser-max-up");
    h.click_on("chooser-max-up");
    let confirm = h.root(|root, _| {
        root.layout_chooser_controls()
            .into_iter()
            .find(|(selector, _)| selector == "chooser-confirm-all")
            .map(|(_, label)| label)
    });
    assert_eq!(confirm.as_deref(), Some("Open 5 sessions, 3 tabs"));
    h.click_on("chooser-confirm-all");
    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::InitLayout {
                kind: InitLayoutKind::AllSessions
            }]
        ),
        "sent {sent:?}"
    );
    assert!(!chooser_open(&mut h));

    let row = |a: &str, b: &str| split(SplitDirection::Horizontal, pane(a, None), pane(b, None));
    let grid = split(
        SplitDirection::Vertical,
        row("p1", "p2"),
        split(SplitDirection::Vertical, row("p3", "p4"), pane("p5", None)),
    );
    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &grid)],
    });
    let layout = RearrangeLayout::Grid { cols: 2 };
    let sent = h.sent();
    let extracted: Vec<Vec<String>> = sent
        .iter()
        .filter_map(|m| match m {
            ClientMessage::ExtractToNewTab {
                source_tab_id,
                pane_ids,
                name: None,
                layout: Some(l),
            } if source_tab_id == "t1" && *l == layout => Some(pane_ids.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        extracted,
        [
            vec!["p3".to_owned(), "p4".to_owned()],
            vec!["p5".to_owned()]
        ],
        "sent {sent:?}"
    );
    assert_eq!(sent.len(), 3, "sent {sent:?}");
    assert!(
        matches!(
            sent.last(),
            Some(ClientMessage::RearrangeTab { tab_id, layout: l }) if tab_id == "t1" && *l == layout
        ),
        "sent {sent:?}"
    );

    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &grid)],
    });
    assert!(h.sent().is_empty(), "the arrangement applies once");
}

#[gpui::test]
fn copy_client_sends_clone_client(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    let other = ClonableLayout {
        client_id: "abcdef123456".to_owned(),
        name: None,
    };
    h.send(init_required(0, vec![other]));
    h.sent();
    let controls = h.root(|root, _| root.layout_chooser_controls());
    assert!(
        controls.contains(&(
            "layout-choose-clone-abcdef123456".to_owned(),
            "Copy another window's layout (abcdef)".to_owned()
        )),
        "{controls:?}"
    );
    h.click_on("layout-choose-clone-abcdef123456");
    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::InitLayout {
                kind: InitLayoutKind::CloneClient { client_id }
            }] if client_id == "abcdef123456"
        ),
        "sent {sent:?}"
    );
    assert!(!chooser_open(&mut h));
}

#[gpui::test]
fn esc_does_nothing_and_keys_do_not_reach_the_pty(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"");
    let cell = h.cell_center("p1", 0, 0);
    h.click(cell, Modifiers::none());
    h.sent();
    h.send(init_required(1, Vec::new()));
    h.keys("escape a b ctrl-b");
    assert!(chooser_open(&mut h), "Esc does not dismiss the chooser");
    assert!(h.sent_input("s1").is_empty(), "no key reached the pane");
    assert!(h.sent().is_empty());
    assert!(
        !h.root(|root, _| root.sidebar_collapsed()),
        "Ctrl+B did not reach the root's shortcuts"
    );
}

#[gpui::test]
fn clicks_behind_the_chooser_do_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    let new_tab = h.center("new-tab");
    h.sent();
    h.send(init_required(1, Vec::new()));
    h.click(new_tab, Modifiers::none());
    let sent = h.sent();
    assert!(
        !sent
            .iter()
            .any(|m| matches!(m, ClientMessage::CreateTab { .. })),
        "sent {sent:?}"
    );
    assert!(chooser_open(&mut h));
}

#[gpui::test]
fn welcome_closes_it_and_the_next_request_reopens_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    h.send(init_required(1, Vec::new()));
    h.lose_connection();
    assert!(
        chooser_open(&mut h),
        "it stays under the connecting overlay"
    );
    h.send(DaemonMessage::Welcome {
        protocol_version: support::PROTOCOL,
        supported_versions: vec![support::PROTOCOL],
    });
    assert!(!chooser_open(&mut h));
    h.send(init_required(1, Vec::new()));
    assert!(chooser_open(&mut h));
}

#[gpui::test]
fn tabs_closes_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    h.send(init_required(1, Vec::new()));
    h.sent();
    h.send(DaemonMessage::Tabs { tabs: Vec::new() });
    assert!(!chooser_open(&mut h));
    assert!(h.sent().is_empty(), "no arrangement was chosen");
}
