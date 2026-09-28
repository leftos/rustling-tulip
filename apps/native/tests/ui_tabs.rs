//! Tab and split-pane specs: new tab, split, close, rename,
//! divider drags, detach, resize and smart placement.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use gpui::{Modifiers, MouseButton, TestAppContext, point, px, size};
use protocol::{
    ClientMessage, DaemonMessage, MergeLayout, RearrangeLayout, SplitDirection, SplitPlace,
    TabEntry,
};
use serde_json::json;
use std::time::Duration;
use support::{Fixture, Harness, TestDir, pane, session, split, tab};

fn two_tabs() -> Fixture {
    let mut fixture = Fixture::single(session("s1").build());
    fixture.tabs.push(tab("t2", &pane("p2", None)));
    fixture
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

/// Tabs `t1`–`t3`; `t1` holds `s1` and the others an empty pane.
fn three_tabs() -> Fixture {
    let mut fixture = two_tabs();
    fixture.tabs.push(tab("t3", &pane("p3", None)));
    fixture
}

/// Tab `t1` with a pane for each of `sessions`, side by side.
fn grid_of(sessions: &[&str]) -> Fixture {
    let mut panes = sessions
        .iter()
        .enumerate()
        .map(|(i, s)| pane(&format!("p{}", i + 1), Some(s)));
    let first = panes.next().expect("a session at least");
    let grid = panes.fold(first, |grid, next| {
        split(SplitDirection::Horizontal, grid, next)
    });
    Fixture {
        sessions: sessions.iter().map(|s| session(s).build()).collect(),
        tabs: vec![tab("t1", &grid)],
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

fn ctrl_click(h: &mut Harness, selector: &str) {
    let at = h.center(selector);
    h.click(at, Modifiers::control());
}

fn selected(h: &mut Harness) -> Vec<String> {
    h.root(|root, _| root.selected_tabs())
}

fn menu_text(h: &mut Harness) -> Vec<String> {
    h.root(|root, _| root.tab_menu_text())
}

#[gpui::test]
fn badge_shows_busy_over_total_and_hides_at_zero(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        pane("p2", Some("s2")),
    );
    let fixture = Fixture {
        sessions: vec![
            session("s1").status("working").build(),
            session("s2").build(),
        ],
        tabs: vec![tab("t1", &grid), tab("t2", &pane("p3", None))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);

    assert_eq!(h.root(|root, _| root.tab_badge("t1")), Some((1, 2)));
    assert!(h.bounds("tab-badge-t1").origin.x >= px(0.0), "drawn");
    assert_eq!(h.root(|root, _| root.tab_badge("t2")), None, "no session");
    assert!(h.bounds("tab-badge-t2").origin.x < px(0.0), "not drawn");

    h.send(DaemonMessage::TabUpdated {
        tab: diff_tab("t2"),
    });
    assert_eq!(h.root(|root, _| root.tab_badge("t2")), None, "a diff tab");
}

#[gpui::test]
fn ctrl_and_shift_click_select_without_activating(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &three_tabs());
    let active = |h: &mut Harness| h.root(|root, _| root.active_tab_id().map(str::to_owned));
    assert_eq!(active(&mut h).as_deref(), Some("t1"));

    ctrl_click(&mut h, "tab-t2");
    assert_eq!(selected(&mut h), ["t2"]);
    assert_eq!(
        active(&mut h).as_deref(),
        Some("t1"),
        "Ctrl+click only selects"
    );

    let at = h.center("tab-t3");
    h.click(at, Modifiers::shift());
    assert_eq!(
        selected(&mut h),
        ["t2", "t3"],
        "Shift ranges from the anchor"
    );
    assert_eq!(active(&mut h).as_deref(), Some("t1"));

    h.click_on("tab-t3");
    assert!(selected(&mut h).is_empty(), "a plain click clears it");
    assert_eq!(active(&mut h).as_deref(), Some("t3"));
}

#[gpui::test]
fn menu_rename_starts_inline_rename(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_tabs());
    h.sent();
    h.right_click_on("tab-t2");
    h.click_on("tab-menu-rename");
    assert!(!h.in_model("tab-menu"), "the menu closes");
    assert_eq!(
        h.root(|root, _| root.renaming_tab().map(str::to_owned)),
        Some("t2".to_owned())
    );
    h.cx.simulate_input("x");
    h.keys("enter");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::RenameTab { tab_id, name }] if tab_id == "t2" && name == "x"),
        "sent {sent:?}"
    );
}

fn rearranged(sent: &[ClientMessage]) -> Vec<RearrangeLayout> {
    sent.iter()
        .filter_map(|m| match m {
            ClientMessage::RearrangeTab { tab_id, layout } if tab_id == "t1" => Some(*layout),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn menu_rearrange_sends_rearrange_tab_with_aspect_cols(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &grid_of(&["s1", "s2"]));
    h.sent();

    h.right_click_on("tab-t1");
    assert!(
        !h.in_model("tab-menu-grid"),
        "two panes have no shape to pick"
    );
    h.click_on("tab-menu-grid-auto");
    assert_eq!(
        rearranged(&h.sent()),
        [RearrangeLayout::Grid { cols: 2 }],
        "a wide area fits two across"
    );
    assert!(!h.in_model("tab-menu"), "the menu closes");

    h.cx.simulate_resize(size(px(420.0), px(1400.0)));
    h.cx.run_until_parked();
    h.right_click_on("tab-t1");
    h.click_on("tab-menu-grid-auto");
    assert_eq!(
        rearranged(&h.sent()),
        [RearrangeLayout::Grid { cols: 1 }],
        "a tall area stacks them"
    );

    h.right_click_on("tab-t1");
    h.click_on("tab-menu-side-by-side");
    h.right_click_on("tab-t1");
    h.click_on("tab-menu-stacked");
    assert_eq!(
        rearranged(&h.sent()),
        [RearrangeLayout::Horizontal, RearrangeLayout::Vertical]
    );
}

#[gpui::test]
fn menu_grid_shape_sends_its_cols(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &grid_of(&["s1", "s2", "s3", "s4"]));
    h.sent();

    h.right_click_on("tab-t1");
    h.click_on("tab-menu-grid");
    assert!(h.in_model("tab-menu"), "the submenu keeps the menu open");
    assert!(
        !h.in_model("tab-menu-rename"),
        "its rows replace the menu's"
    );
    assert!(
        menu_text(&mut h).contains(&"Auto (2 cols × 2 rows)".to_owned()),
        "rows {:?}",
        menu_text(&mut h)
    );
    h.click_on("tab-menu-grid-back");
    assert!(h.in_model("tab-menu-rename"), "Back restores the rows");
    h.click_on("tab-menu-grid");
    h.click_on("tab-menu-grid-3");
    assert_eq!(rearranged(&h.sent()), [RearrangeLayout::Grid { cols: 3 }]);
    assert!(!h.in_model("tab-menu"));
}

#[gpui::test]
fn menu_hides_rearrange_under_two_panes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("s1")),
        pane("p2", None),
    );
    let fixture = Fixture {
        sessions: vec![session("s1").build()],
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.right_click_on("tab-t1");
    let rows = h.root(|root, _| root.tab_menu_rows());
    assert_eq!(
        rows,
        [
            "tab-menu-rename",
            "tab-menu-increase",
            "tab-menu-decrease",
            "tab-menu-reset",
            "tab-menu-close",
        ],
        "one bound pane: no rearrange rows, and one tab: no Close other tabs"
    );
    assert!(!menu_text(&mut h).contains(&"Rearrange panes".to_owned()));
}

#[gpui::test]
fn close_others_arms_then_sends_close_for_each_other(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &three_tabs());
    h.sent();
    h.right_click_on("tab-t2");
    assert!(menu_text(&mut h).contains(&"Close other tabs".to_owned()));

    h.click_on("tab-menu-close-others");
    assert!(h.sent().is_empty(), "the first press only arms");
    assert!(h.in_model("tab-menu"), "and keeps the menu open");
    assert!(
        menu_text(&mut h).contains(&"Confirm closing 2 other tabs".to_owned()),
        "rows {:?}",
        menu_text(&mut h)
    );

    h.click_on("tab-menu-close-others");
    let closed: Vec<String> = h
        .sent()
        .into_iter()
        .filter_map(|m| match m {
            ClientMessage::CloseTab { tab_id } => Some(tab_id),
            _ => None,
        })
        .collect();
    assert_eq!(closed, ["t1", "t3"]);
    assert!(!h.in_model("tab-menu"));
}

#[gpui::test]
fn merge_selected_sends_merge_tabs_in_strip_order_and_activates_new_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &three_tabs());
    h.sent();
    ctrl_click(&mut h, "tab-t3");
    ctrl_click(&mut h, "tab-t1");

    h.right_click_on("tab-t2");
    assert!(
        !h.in_model("tab-menu-merge-vertical"),
        "t2 is not selected, so its menu has no merge"
    );
    h.keys("escape");

    h.right_click_on("tab-t3");
    assert!(menu_text(&mut h).contains(&"Merge 2 selected into new tab".to_owned()));
    h.click_on("tab-menu-merge-vertical");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::MergeTabs { tab_ids, name: None, layout: MergeLayout::TileVertical }]
            if tab_ids == &["t1".to_owned(), "t3".to_owned()]),
        "sent {sent:?}"
    );
    assert!(selected(&mut h).is_empty(), "the selection clears");
    assert!(!h.in_model("tab-menu"));

    for id in ["t1", "t3"] {
        h.send(DaemonMessage::TabRemoved {
            tab_id: id.to_owned(),
        });
    }
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t4", &pane("p4", None)),
    });
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("t4".to_owned())
    );
}

#[gpui::test]
fn merge_disabled_when_a_diff_tab_is_selected(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.tabs.push(diff_tab("d1"));
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();
    ctrl_click(&mut h, "tab-t1");
    ctrl_click(&mut h, "tab-d1");

    h.right_click_on("tab-t1");
    assert!(menu_text(&mut h).contains(&"Diff tabs can't be merged".to_owned()));
    h.click_on("tab-menu-merge-horizontal");
    assert!(
        !h.sent()
            .iter()
            .any(|m| matches!(m, ClientMessage::MergeTabs { .. })),
        "the rows are inert"
    );
    assert!(h.in_model("tab-menu"), "and the menu stays");
    assert_eq!(selected(&mut h), ["t1", "d1"]);
}

#[gpui::test]
fn close_others_disarms_after_another_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &three_tabs());
    h.sent();
    let closes = |sent: &[ClientMessage]| {
        sent.iter()
            .filter(|m| matches!(m, ClientMessage::CloseTab { .. }))
            .count()
    };

    h.right_click_on("tab-t2");
    h.click_on("tab-menu-close-others");
    assert!(menu_text(&mut h).contains(&"Confirm closing 2 other tabs".to_owned()));
    h.click_on("tab-menu-increase");
    assert!(h.in_model("tab-menu"), "Increase keeps the menu open");
    assert!(
        menu_text(&mut h).contains(&"Close other tabs".to_owned()),
        "another row disarms it: rows {:?}",
        menu_text(&mut h)
    );
    h.click_on("tab-menu-close-others");
    assert_eq!(closes(&h.sent()), 0, "so this press only arms again");

    h.keys("escape");
    h.right_click_on("tab-t2");
    assert!(
        menu_text(&mut h).contains(&"Close other tabs".to_owned()),
        "a closed menu forgets the arm"
    );
    h.click_on("tab-menu-close-others");
    assert_eq!(closes(&h.sent()), 0);
}

/// The undo entries on screen, as `(id, message)`, newest first.
fn undo_entries(h: &mut Harness) -> Vec<(u64, String)> {
    h.root(|root, _| {
        root.undo_entries()
            .iter()
            .map(|entry| (entry.id, entry.message.clone()))
            .collect()
    })
}

/// The first snapshot of the newest entry, as
/// `(tab id, index, was active, focus pane)`.
fn newest_snapshot(h: &mut Harness) -> Option<(String, usize, bool, Option<String>)> {
    h.root(|root, _| {
        let snapshot = root.undo_entries().first()?.snapshots.first()?;
        Some((
            snapshot.tab.id.clone(),
            snapshot.index,
            snapshot.restore_active,
            snapshot.focus_pane.clone(),
        ))
    })
}

#[gpui::test]
fn closing_a_tab_offers_undo_that_sends_restore_tab_at_its_index(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_tabs());
    h.sent();
    h.click_on("tab-close-t2");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::CloseTab { tab_id }] if tab_id == "t2"),
        "sent {sent:?}"
    );

    let (id, message) = undo_entries(&mut h)
        .first()
        .cloned()
        .expect("an undo entry");
    assert_eq!(message, "Closed tab \"t2\"");
    assert_eq!(
        newest_snapshot(&mut h),
        Some(("t2".to_owned(), 1, false, None)),
        "its place in the strip, and no pane to focus"
    );
    assert!(
        h.bounds("undo-shelf").origin.x >= px(0.0),
        "the shelf's column is drawn"
    );
    assert!(h.in_model("undo-shelf"), "and it shows the entry");
    assert!(h.in_model(&format!("undo-entry-{id}")));

    h.send(DaemonMessage::TabRemoved {
        tab_id: "t2".to_owned(),
    });
    h.sent();
    h.click_on(&format!("undo-action-{id}"));
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::RestoreTab { tab, index }] if tab.id == "t2" && *index == 1),
        "sent {sent:?}"
    );
    assert!(undo_entries(&mut h).is_empty(), "the entry is spent");
    assert!(!h.in_model("undo-shelf"), "and the shelf goes with it");
}

#[gpui::test]
fn close_others_offers_no_undo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &three_tabs());
    h.sent();
    h.right_click_on("tab-t2");
    h.click_on("tab-menu-close-others");
    h.click_on("tab-menu-close-others");
    let closed = h
        .sent()
        .iter()
        .filter(|msg| matches!(msg, ClientMessage::CloseTab { .. }))
        .count();
    assert_eq!(closed, 2, "the other tabs close");
    assert!(
        undo_entries(&mut h).is_empty(),
        "Close other tabs cannot be taken back"
    );
}

#[gpui::test]
fn undo_entry_expires_after_8_seconds(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_tabs());
    h.sent();
    h.click_on("tab-close-t2");
    assert_eq!(undo_entries(&mut h).len(), 1);

    h.advance(Duration::from_millis(7_999));
    assert_eq!(undo_entries(&mut h).len(), 1, "still there just before");
    h.advance(Duration::from_millis(1));
    assert!(undo_entries(&mut h).is_empty(), "eight seconds is up");
}

#[gpui::test]
fn shelf_keeps_three_newest(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = two_tabs();
    for id in ["t3", "t4", "t5"] {
        fixture.tabs.push(tab(id, &pane(&format!("p{id}"), None)));
    }
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();
    for id in ["t2", "t3", "t4", "t5"] {
        h.click_on(&format!("tab-close-{id}"));
    }

    let messages: Vec<String> = undo_entries(&mut h)
        .into_iter()
        .map(|(_, message)| message)
        .collect();
    assert_eq!(
        messages,
        [
            "Closed tab \"t5\"",
            "Closed tab \"t4\"",
            "Closed tab \"t3\""
        ],
        "the newest three, newest first"
    );
}

#[gpui::test]
fn dismiss_removes_entry_without_sending(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_tabs());
    h.sent();
    h.click_on("tab-close-t2");
    let (id, _) = undo_entries(&mut h)
        .first()
        .cloned()
        .expect("an undo entry");
    h.sent();

    h.click_on(&format!("undo-dismiss-{id}"));
    assert!(h.sent().is_empty(), "the ✕ sends nothing");
    assert!(undo_entries(&mut h).is_empty(), "the entry is gone");
    assert!(!h.in_model(&format!("undo-entry-{id}")), "and off screen");
    assert!(!h.in_model("undo-shelf"));
}

#[gpui::test]
fn restored_active_tab_becomes_active(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_tabs());
    h.sent();
    h.click_on("tab-close-t1");
    h.click_on("tab-close-t1");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::CloseTab { tab_id }] if tab_id == "t1"),
        "the second click closes the tab holding s1: {sent:?}"
    );

    let (id, message) = undo_entries(&mut h)
        .first()
        .cloned()
        .expect("an undo entry");
    assert_eq!(message, "Closed tab \"t1\"");
    assert_eq!(
        newest_snapshot(&mut h),
        Some(("t1".to_owned(), 0, true, None)),
        "it was the tab shown"
    );

    h.send(DaemonMessage::TabRemoved {
        tab_id: "t1".to_owned(),
    });
    h.sent();
    h.click_on(&format!("undo-action-{id}"));
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::RestoreTab { tab, index }] if tab.id == "t1" && *index == 0),
        "sent {sent:?}"
    );

    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &pane("p1", Some("s1"))),
    });
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("t1".to_owned()),
        "the restored tab shows again"
    );
}

#[gpui::test]
fn shelf_clears_on_welcome(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_tabs());
    h.sent();
    h.click_on("tab-close-t2");
    assert_eq!(undo_entries(&mut h).len(), 1, "the close offers an undo");

    h.send(DaemonMessage::Welcome {
        protocol_version: 1,
        supported_versions: vec![1],
    });
    assert!(
        undo_entries(&mut h).is_empty(),
        "a reconnect forgets what the ids stood for"
    );
}

#[gpui::test]
fn shelf_clears_when_connection_drops(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_tabs());
    h.sent();
    h.click_on("tab-close-t2");
    assert_eq!(undo_entries(&mut h).len(), 1, "the close offers an undo");

    h.lose_connection();
    assert!(
        undo_entries(&mut h).is_empty(),
        "a dropped connection forgets what the ids stood for"
    );
    assert!(!h.in_model("undo-shelf"));
}

#[gpui::test]
fn refused_restore_does_not_activate_the_next_new_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_tabs());
    h.sent();
    h.click_on("tab-close-t1");
    h.click_on("tab-close-t1");
    h.sent();
    h.send(DaemonMessage::TabRemoved {
        tab_id: "t1".to_owned(),
    });
    h.sent();
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("t2".to_owned()),
        "the tab that held s1 is gone"
    );

    let (id, _) = undo_entries(&mut h)
        .first()
        .cloned()
        .expect("an undo entry");
    h.click_on(&format!("undo-action-{id}"));
    h.sent();

    // The daemon refuses it: its own list still holds that tab id.
    h.send(DaemonMessage::Error {
        message: "tab already exists: t1".to_owned(),
        request_id: None,
    });
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t9", &pane("p9", None)),
    });
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("t2".to_owned()),
        "a refused restore arms nothing for the next tab to arrive"
    );
}
