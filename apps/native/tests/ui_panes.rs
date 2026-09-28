//! Pane action specs: the pane-close dialog, the header's move button, the
//! empty pane's menu and the Move panes dialog.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use gpui::TestAppContext;
use protocol::{
    ClientMessage, DaemonMessage, GridNode, PaneDropEdge, RearrangeLayout, SessionSnapshot,
    SplitDirection, TabEntry,
};
use serde_json::json;
use support::{Fixture, Harness, TestDir, pane, session, split, tab};

/// Tab `t1`: pane `p1` showing `s1` beside the empty pane `p2`.
fn with_empty(s1: SessionSnapshot) -> Fixture {
    Fixture {
        sessions: vec![s1],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", None),
            ),
        )],
        ..Fixture::default()
    }
}

/// Tab `t1` with a pane for each of `sessions`, side by side.
fn grid_of(sessions: &[&str]) -> GridNode {
    let mut panes = sessions
        .iter()
        .enumerate()
        .map(|(i, s)| pane(&format!("p{}", i + 1), Some(s)));
    let first = panes.next().expect("a session at least");
    panes.fold(first, |grid, next| {
        split(SplitDirection::Horizontal, grid, next)
    })
}

fn grid_fixture(sessions: &[&str]) -> Fixture {
    Fixture {
        sessions: sessions.iter().map(|s| session(s).build()).collect(),
        tabs: vec![tab("t1", &grid_of(sessions))],
        ..Fixture::default()
    }
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

fn kinds(sent: &[ClientMessage]) -> Vec<String> {
    sent.iter()
        .map(|msg| {
            serde_json::to_value(msg).expect("a client message serializes")["type"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

fn active(h: &mut Harness) -> Option<String> {
    h.root(|root, _| root.active_tab_id().map(str::to_owned))
}

/// The daemon answers an extract with the new tab; it becomes active.
fn assert_new_tab_activates(h: &mut Harness) {
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t9", &pane("p9", Some("s1"))),
    });
    assert_eq!(active(h).as_deref(), Some("t9"), "the new tab shows");
}

#[gpui::test]
fn pane_close_on_empty_pane_sends_close_immediately(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_empty(session("s1").build()));
    h.sent();
    h.click_on("close-pane-p2");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ClosePane { tab_id, pane_id }] if tab_id == "t1" && pane_id == "p2"),
        "sent {sent:?}"
    );
    assert!(!h.in_model("pane-close"), "no dialog for an empty pane");
}

#[gpui::test]
fn pane_close_with_session_opens_dialog_pane_only_focused(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_empty(session("s1").label("work").build()));
    h.sent();
    h.click_on("close-pane-p1");
    assert!(h.sent().is_empty(), "the dialog asks first");
    assert!(h.in_model("pane-close"));
    assert_eq!(
        h.root(|root, _| root.pane_close_focus()).as_deref(),
        Some("pane-close-only")
    );
    let labels: Vec<String> = h
        .root(|root, _| root.pane_close_controls())
        .into_iter()
        .map(|(_, label)| label)
        .collect();
    assert_eq!(
        labels,
        [
            "Cancel",
            "Close pane but keep session in sidebar",
            "Close pane and close session",
            "✕"
        ]
    );
    assert!(!h.in_model("pane-close-delete"), "no worktree, no delete");
    h.keys("tab");
    assert_eq!(
        h.root(|root, _| root.pane_close_focus()).as_deref(),
        Some("pane-close-discard"),
        "Tab moves on"
    );
}

#[gpui::test]
fn pane_only_sends_close_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_empty(session("s1").build()));
    h.sent();
    h.click_on("close-pane-p1");
    h.click_on("pane-close-only");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ClosePane { tab_id, pane_id }] if tab_id == "t1" && pane_id == "p1"),
        "sent {sent:?}"
    );
    assert!(!h.in_model("pane-close"), "the dialog closes");
}

#[gpui::test]
fn discard_keep_sends_close_stop_discard(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_empty(session("s1").in_repo("r1").build()));
    h.sent();
    h.click_on("close-pane-p1");
    h.click_on("pane-close-discard");
    let sent = h.sent();
    assert_eq!(
        kinds(&sent),
        ["close_pane", "stop_session", "discard_session"],
        "sent {sent:?}"
    );
    assert!(
        matches!(&sent[2], ClientMessage::DiscardSession { session_id, cleanup }
            if session_id == "s1" && cleanup.len() == 1 && !cleanup[0].remove_worktree),
        "sent {sent:?}"
    );
    assert!(!h.in_model("pane-close"));
}

#[gpui::test]
fn delete_worktree_opens_branch_fate_confirm(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_empty(session("s1").worktree("r1").build()));
    h.sent();
    h.click_on("close-pane-p1");
    assert_eq!(
        h.root(|root, _| root.pane_close_controls())
            .into_iter()
            .map(|(_, label)| label)
            .collect::<Vec<_>>(),
        [
            "Cancel",
            "Close pane, keep session, keep worktree",
            "Close pane, don't keep session, keep worktree",
            "Close pane, don't keep session, delete worktree",
            "✕"
        ]
    );
    h.click_on("pane-close-delete");
    assert!(!h.in_model("pane-close"), "the pane dialog gives way");
    assert_eq!(
        h.root(|root, _| root.delete_dialog_session().map(str::to_owned))
            .as_deref(),
        Some("s1"),
        "the delete-worktree confirm opens"
    );
    let sent = h.sent();
    assert_eq!(kinds(&sent), ["preview_discard"], "sent {sent:?}");
}

#[gpui::test]
fn pane_close_esc_cancels(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_empty(session("s1").build()));
    h.sent();
    h.click_on("close-pane-p1");
    h.keys("escape");
    assert!(!h.in_model("pane-close"), "Esc closes it");
    assert!(h.sent().is_empty(), "and sends nothing");

    h.click_on("close-pane-p1");
    h.keys("shift-tab");
    assert_eq!(
        h.root(|root, _| root.pane_close_focus()).as_deref(),
        Some("pane-close-cancel")
    );
    h.keys("enter");
    assert!(!h.in_model("pane-close"), "Enter presses Cancel");
    assert!(h.sent().is_empty());
}

#[gpui::test]
fn header_move_sends_extract_and_activates_new_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_empty(session("s1").build()));
    h.sent();
    assert!(
        !h.in_model("pane-move-new-tab-p2"),
        "an empty pane has no move button"
    );
    h.click_on("pane-move-new-tab-p1");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ExtractToNewTab { source_tab_id, pane_ids, name: None, layout: None }]
            if source_tab_id == "t1" && pane_ids == &["p1"]),
        "sent {sent:?}"
    );
    assert_new_tab_activates(&mut h);
}

#[gpui::test]
fn empty_pane_menu_move_to_tab_sends_move_pane_to_drop_target(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = with_empty(session("s1").build());
    fixture.sessions.push(session("s2").build());
    fixture.tabs.push(tab(
        "t2",
        &split(
            SplitDirection::Vertical,
            pane("p3", Some("s2")),
            pane("p4", None),
        ),
    ));
    fixture.tabs.push(diff_tab("t3"));
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();

    h.right_click_on("pane-header-p2");
    assert!(h.in_model("empty-pane-menu"));
    assert!(h.in_model("empty-pane-close"));
    h.click_on("empty-pane-menu-move");
    let rows = h.root(|root, _| root.empty_pane_menu_rows());
    assert_eq!(
        rows,
        [
            "empty-pane-menu-back",
            "empty-pane-move-new",
            "empty-pane-move-t2"
        ],
        "the diff tab and the pane's own tab are not targets"
    );
    h.click_on("empty-pane-move-t2");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::MovePane { src_tab_id, src_pane_id, dst_tab_id, dst_pane_id, edge: PaneDropEdge::Replace }]
            if src_tab_id == "t1" && src_pane_id == "p2" && dst_tab_id == "t2" && dst_pane_id == "p4"),
        "sent {sent:?}"
    );
    assert!(!h.in_model("empty-pane-menu"), "the menu closes");
}

#[gpui::test]
fn empty_pane_menu_new_tab_sends_extract(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_empty(session("s1").build()));
    h.sent();
    h.right_click_on("pane-header-p1");
    assert!(
        !h.in_model("empty-pane-menu"),
        "a bound pane has the session menu"
    );
    h.keys("escape");

    h.right_click_on("empty-pane-shell-p2");
    assert!(h.in_model("empty-pane-menu"), "the body opens it too");
    h.click_on("empty-pane-menu-move");
    h.click_on("empty-pane-move-new");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ExtractToNewTab { source_tab_id, pane_ids, name: None, layout: None }]
            if source_tab_id == "t1" && pane_ids == &["p2"]),
        "sent {sent:?}"
    );
    assert_new_tab_activates(&mut h);

    h.click_on("tab-t1");
    h.right_click_on("pane-header-p2");
    h.click_on("empty-pane-close");
    let sent = h.sent();
    assert!(
        sent.iter()
            .any(|m| matches!(m, ClientMessage::ClosePane { pane_id, .. } if pane_id == "p2")),
        "sent {sent:?}"
    );
}

#[gpui::test]
fn move_panes_row_hidden_under_three_bound_panes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &grid_fixture(&["s1", "s2"]));
    h.right_click_on("tab-t1");
    assert!(h.in_model("tab-menu"));
    assert!(!h.in_model("tab-menu-move-panes"), "two panes: no row");
    h.keys("escape");

    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &grid_of(&["s1", "s2", "s3"])),
    });
    h.right_click_on("tab-t1");
    assert!(
        h.in_model("tab-menu-move-panes"),
        "three panes: the row shows"
    );
}

#[gpui::test]
fn move_panes_dialog_sends_extract_with_layout_and_name(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &grid_fixture(&["s1", "s2", "s3", "s4"]));
    h.sent();
    h.right_click_on("tab-t1");
    h.click_on("tab-menu-move-panes");
    assert!(!h.in_model("tab-menu"), "the menu gives way");
    assert!(h.in_model("move-panes"));
    assert_eq!(
        h.root(|root, _| root.move_panes_focus()).as_deref(),
        Some("move-panes-pane-p1"),
        "the first box has the focus"
    );
    h.click_on("move-panes-confirm");
    assert!(h.sent().is_empty(), "nothing ticked: Move is disabled");
    assert!(h.in_model("move-panes"));

    h.keys("space");
    h.click_on("move-panes-pane-p3");
    h.click_on("move-panes-layout-stacked");
    h.click_on("move-panes-name");
    h.cx.simulate_input("Work");
    h.keys("enter");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ExtractToNewTab { source_tab_id, pane_ids, name: Some(name), layout: Some(RearrangeLayout::Vertical) }]
            if source_tab_id == "t1" && pane_ids == &["p1", "p3"] && name == "Work"),
        "sent {sent:?}"
    );
    assert!(!h.in_model("move-panes"), "the dialog closes");
    assert_new_tab_activates(&mut h);
}

#[gpui::test]
fn move_panes_esc_cancels(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &grid_fixture(&["s1", "s2", "s3"]));
    h.sent();
    h.right_click_on("tab-t1");
    h.click_on("tab-menu-move-panes");
    h.keys("space escape");
    assert!(!h.in_model("move-panes"), "Esc closes it");

    h.right_click_on("tab-t1");
    h.click_on("tab-menu-move-panes");
    h.click_on("move-panes-name");
    h.keys("escape");
    assert!(!h.in_model("move-panes"), "Esc from the name field too");
    assert!(h.sent().is_empty(), "nothing sent");
}

#[gpui::test]
fn move_panes_prunes_a_pane_removed_while_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &grid_fixture(&["s1", "s2", "s3", "s4"]));
    h.sent();
    h.right_click_on("tab-t1");
    h.click_on("tab-menu-move-panes");
    h.keys("space");
    h.click_on("move-panes-pane-p4");

    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &grid_of(&["s1", "s2", "s3"])),
    });
    assert!(h.in_model("move-panes"), "three panes are left to move");
    assert!(
        !h.in_model("move-panes-pane-p4"),
        "the gone pane is off the list"
    );
    h.click_on("move-panes-confirm");
    let extracts: Vec<ClientMessage> = h
        .sent()
        .into_iter()
        .filter(|m| matches!(m, ClientMessage::ExtractToNewTab { .. }))
        .collect();
    assert!(
        matches!(extracts.as_slice(), [ClientMessage::ExtractToNewTab { pane_ids, .. }] if pane_ids == &["p1"]),
        "sent {extracts:?}"
    );

    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &grid_of(&["s1", "s2", "s3"])),
    });
    h.right_click_on("tab-t1");
    h.click_on("tab-menu-move-panes");
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &pane("p9", None)),
    });
    assert!(!h.in_model("move-panes"), "no listed pane left: it closes");
}

#[gpui::test]
fn app_shortcuts_do_nothing_while_move_panes_name_has_focus(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &grid_fixture(&["s1", "s2", "s3"]));
    h.sent();
    h.right_click_on("tab-t1");
    h.click_on("tab-menu-move-panes");
    h.click_on("move-panes-name");
    h.keys("ctrl-t ctrl-shift-t ctrl-shift-n");
    let sent = h.sent();
    assert!(
        !sent
            .iter()
            .any(|m| matches!(m, ClientMessage::CreateTab { .. })),
        "sent {sent:?}"
    );
    assert!(h.in_model("move-panes"), "the dialog stays");
    assert!(
        !h.root(|root, _| root.spawn_dialog_open()),
        "no spawn dialog"
    );
}

#[gpui::test]
fn pane_close_dropped_when_pane_rebinds(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = with_empty(session("s1").build());
    fixture.sessions.push(session("s2").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    h.click_on("close-pane-p1");
    assert!(h.in_model("pane-close"));
    h.send(DaemonMessage::TabUpdated {
        tab: tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", Some("s2")),
            ),
        ),
    });
    assert!(h.in_model("pane-close"), "another pane's change keeps it");
    h.send(DaemonMessage::TabUpdated {
        tab: tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s2")),
                pane("p2", None),
            ),
        ),
    });
    assert!(
        !h.in_model("pane-close"),
        "its pane shows another session now"
    );
    h.keys("escape");
    let closes = kinds(&h.sent())
        .into_iter()
        .filter(|kind| ["close_pane", "stop_session", "discard_session"].contains(&kind.as_str()))
        .count();
    assert_eq!(closes, 0, "nothing closed or stopped");
}
