//! First-connect layout chooser specs: what each option sends, the
//! arrangement applied to the daemon's answer, and the keys and clicks the
//! chooser keeps from everything behind it.

use crate::support;

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
fn nothing_else_to_offer_takes_start_empty_without_asking(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    h.sent();
    h.send(init_required(0, Vec::new()));
    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::InitLayout {
                kind: InitLayoutKind::Empty
            }]
        ),
        "the only choice is taken without a question: {sent:?}"
    );
    assert!(!chooser_open(&mut h), "no chooser opens");
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
    let layout = RearrangeLayout::Grid {
        cols: 2,
        aspect: None,
    };
    let sent = h.sent();
    let extracted: Vec<Vec<String>> = sent
        .iter()
        .filter_map(|m| match m {
            ClientMessage::ExtractToNewTab {
                source_tab_id,
                pane_ids,
                name: None,
                layout: Some(l),
                request_id: None,
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
    h.send_welcome(support::PROTOCOL);
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

#[gpui::test]
fn shortcuts_work_after_start_empty_closes_the_chooser(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);
    h.send(init_required(2, Vec::new()));
    assert!(chooser_open(&mut h));
    h.keys("enter");
    assert!(
        !chooser_open(&mut h),
        "Start empty has the focus and closes it"
    );
    h.keys("ctrl-n");
    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "a shortcut works again once the chooser closes with no tab to give the keyboard to"
    );
}

/// The `claim` flags of the `Resize`s in `sent` for session `id`, in order.
fn resize_claims(sent: &[ClientMessage], id: &str) -> Vec<bool> {
    sent.iter()
        .filter_map(|m| match m {
            ClientMessage::Resize {
                session_id, claim, ..
            } if session_id == id => Some(*claim),
            _ => None,
        })
        .collect()
}

/// No claimed `Resize` for `id` in `sent`, and at least one sent.
fn assert_sized_unclaimed(sent: &[ClientMessage], id: &str, why: &str) {
    let claims = resize_claims(sent, id);
    assert!(
        !claims.is_empty() && claims.iter().all(|claim| !claim),
        "{why}: {claims:?}"
    );
}

#[gpui::test]
fn open_all_claims_each_new_panes_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build(), session("s2").build()],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();
    h.send(init_required(2, Vec::new()));
    h.sent();
    h.click_on("layout-choose-all");
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

    h.send(DaemonMessage::Tabs {
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", Some("s2")),
            ),
        )],
    });
    h.answer_scrollback("s1", b"");
    h.answer_scrollback("s2", b"");
    let sent = h.sent();
    for id in ["s1", "s2"] {
        assert_eq!(
            resize_claims(&sent, id).first(),
            Some(&true),
            "the pane the chooser opened {id} in claims its size: {sent:?}"
        );
    }
}

#[gpui::test]
fn open_all_with_a_max_claims_only_the_panes_the_first_tab_keeps(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build(), session("s2").build()],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();
    h.send(init_required(2, Vec::new()));
    h.sent();
    h.click_on("layout-choose-all");
    h.click_on("chooser-max-up");
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

    h.send(DaemonMessage::Tabs {
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", Some("s2")),
            ),
        )],
    });
    h.answer_scrollback("s1", b"");
    h.answer_scrollback("s2", b"");
    let sent = h.sent();
    assert_eq!(
        resize_claims(&sent, "s1").first(),
        Some(&true),
        "the pane the first tab keeps claims its size: {sent:?}"
    );
    let moved = resize_claims(&sent, "s2");
    assert!(
        !moved.is_empty() && moved.iter().all(|claim| !claim),
        "the pane the arrangement moves to another tab claims nothing here: {sent:?}"
    );
}

#[gpui::test]
fn start_empty_then_a_later_tab_list_does_not_claim(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build()],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();
    h.send(init_required(2, Vec::new()));
    h.sent();
    h.click_on("layout-choose-empty");
    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::InitLayout {
                kind: InitLayoutKind::Empty
            }]
        ),
        "sent {sent:?}"
    );

    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
    });
    h.answer_scrollback("s1", b"");
    assert_sized_unclaimed(&h.sent(), "s1", "Start empty claims nothing");
}
