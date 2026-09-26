//! App shortcut specs: Ctrl+(Shift+)Tab and Ctrl+1–9 switch tabs,
//! Ctrl+(Shift+)T opens a tab, Ctrl+Shift+G rearranges the active grid; a key
//! with nothing to do reaches the terminal, and none fire under a menu.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use gpui::{Modifiers, TestAppContext, point, px};
use protocol::{ClientMessage, RearrangeLayout, SplitDirection, TabEntry};
use serde_json::json;
use support::{Fixture, Harness, TestDir, pane, repo, session, split, tab};

/// A diff tab of `path` in `r1`'s main tree.
fn diff_tab(id: &str, path: &str) -> TabEntry {
    serde_json::from_value(json!({
        "id": id,
        "name": path,
        "content": {
            "kind": "diff",
            "repo_id": "r1",
            "path": path,
            "against": null,
            "worktree_path": null,
        },
        "created_at": "2026-01-01T00:00:00Z",
    }))
    .expect("diff tab fixture")
}

/// `s1` in `t1`, `s2` in `t2`, then a diff tab `d1`.
fn three_tabs() -> Fixture {
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    fixture.tabs.push(tab("t2", &pane("p2", Some("s2"))));
    fixture.tabs.push(diff_tab("d1", "src/main.rs"));
    fixture
}

fn active(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.active_tab_id().map(str::to_owned))
}

/// Gives `s1`'s terminal in `t1` the keyboard and drops what went out.
fn focus_terminal(h: &mut Harness<'_>) {
    let cell = h.cell_center("p1", 0, 0);
    h.click(cell, Modifiers::none());
    h.sent();
}

/// Moves the keyboard off the terminals, onto the sidebar panel.
fn focus_sidebar(h: &mut Harness<'_>) {
    let panel = h.bounds("sidebar-panel");
    h.click(
        point(panel.center().x, panel.bottom() - px(10.0)),
        Modifiers::none(),
    );
    h.sent();
}

fn creates_tab(sent: &[ClientMessage]) -> bool {
    matches!(
        sent,
        [ClientMessage::CreateTab {
            name: None,
            initial_session_id: None,
        }]
    )
}

#[gpui::test]
fn ctrl_tab_cycles_tabs_from_the_terminal_and_wraps(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &three_tabs());
    focus_terminal(&mut h);
    assert_eq!(active(&mut h).as_deref(), Some("t1"));

    h.keys("ctrl-tab");
    assert_eq!(active(&mut h).as_deref(), Some("t2"));
    h.keys("ctrl-tab");
    assert_eq!(
        active(&mut h).as_deref(),
        Some("d1"),
        "diff tabs are in the cycle"
    );
    h.keys("ctrl-tab");
    assert_eq!(active(&mut h).as_deref(), Some("t1"), "wraps to the first");
    assert!(
        h.sent_input("s1").is_empty(),
        "the terminal never saw a tab"
    );
    assert!(h.sent_input("s2").is_empty());
}

#[gpui::test]
fn ctrl_shift_tab_goes_back_and_wraps(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &three_tabs());
    focus_terminal(&mut h);

    h.keys("ctrl-shift-tab");
    assert_eq!(active(&mut h).as_deref(), Some("d1"), "wraps to the last");
    h.keys("ctrl-shift-tab");
    assert_eq!(active(&mut h).as_deref(), Some("t2"));
    assert!(h.sent_input("s1").is_empty());
}

#[gpui::test]
fn ctrl_tab_with_one_tab_reaches_the_pty(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    focus_terminal(&mut h);

    h.keys("ctrl-tab");
    assert_eq!(h.sent_input("s1"), b"\t");
    assert_eq!(active(&mut h).as_deref(), Some("t1"));
}

#[gpui::test]
fn ctrl_digit_jumps_to_that_tab_and_missing_tab_falls_through(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &three_tabs());
    focus_terminal(&mut h);

    h.keys("ctrl-3");
    assert_eq!(active(&mut h).as_deref(), Some("d1"));
    h.keys("ctrl-1");
    assert_eq!(active(&mut h).as_deref(), Some("t1"));
    h.keys("ctrl-2");
    assert_eq!(active(&mut h).as_deref(), Some("t2"));
    assert!(h.sent_input("s1").is_empty());
    assert!(
        h.sent_input("s2").is_empty(),
        "the terminal never saw a digit"
    );

    h.keys("ctrl-5");
    assert_eq!(active(&mut h).as_deref(), Some("t2"), "there is no 5th tab");
    assert!(
        h.sent_input("s2").is_empty(),
        "Ctrl+5 has no terminal bytes"
    );
}

#[gpui::test]
fn ctrl_t_outside_terminal_creates_a_tab_and_in_terminal_reaches_the_pty(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    focus_terminal(&mut h);

    h.keys("ctrl-t");
    assert_eq!(h.sent_input("s1"), [0x14]);
    assert!(!creates_tab(&h.sent()), "no tab from inside a terminal");

    focus_sidebar(&mut h);
    h.keys("ctrl-t");
    let sent = h.sent();
    assert!(creates_tab(&sent), "sent {sent:?}");
    assert!(h.sent_input("s1").is_empty());
}

#[gpui::test]
fn ctrl_shift_t_creates_a_tab_from_the_terminal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    focus_terminal(&mut h);

    h.keys("ctrl-shift-t");
    let sent = h.sent();
    assert!(creates_tab(&sent), "sent {sent:?}");
    assert!(h.sent_input("s1").is_empty(), "the terminal never saw it");
}

#[gpui::test]
fn ctrl_shift_g_rearranges_a_grid_of_two_panes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        pane("p2", Some("s2")),
    );
    let fixture = Fixture {
        sessions: vec![session("s1").build(), session("s2").build()],
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    focus_terminal(&mut h);

    h.keys("ctrl-shift-g");
    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [ClientMessage::RearrangeTab {
                tab_id,
                layout: RearrangeLayout::Grid { cols: 0 },
            }] if tab_id == "t1"
        ),
        "sent {sent:?}"
    );
    assert!(h.sent_input("s1").is_empty(), "the terminal never saw it");
}

#[gpui::test]
fn ctrl_shift_g_with_one_pane_reaches_the_pty(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    focus_terminal(&mut h);

    h.keys("ctrl-shift-g");
    assert_eq!(h.sent_input("s1"), [0x07]);
    let sent = h.sent();
    assert!(
        !sent
            .iter()
            .any(|m| matches!(m, ClientMessage::RearrangeTab { .. })),
        "sent {sent:?}"
    );
}

#[gpui::test]
fn shortcuts_are_ignored_while_a_menu_is_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &three_tabs());
    focus_terminal(&mut h);
    h.right_click_on("leaf-s1");
    assert!(
        h.root(|root, _| root.session_menu().is_some()),
        "the session menu is open"
    );

    h.keys("ctrl-tab");
    h.keys("ctrl-2");
    h.keys("ctrl-shift-t");
    assert_eq!(active(&mut h).as_deref(), Some("t1"));
    let sent = h.sent();
    assert!(!creates_tab(&sent), "sent {sent:?}");
}
