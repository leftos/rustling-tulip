//! Terminal font-size specs: the resolution order every pane applies, the
//! session and tab shortcuts, the tab context menu and pruning.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use std::time::Duration;

use gpui::{Modifiers, TestAppContext};
use protocol::{
    AppearanceOverrides, ClientMessage, DaemonMessage, RepoEntry, SessionSnapshot, WorkspaceEntry,
};
use support::{Fixture, Harness, TestDir, pane, repo, session, tab, workspace};

/// The harness on `fixture`, with pane `p1`'s scrollback answered, the pane
/// focused and everything sent so far drained.
fn focused<'a>(cx: &'a mut TestAppContext, dir: &TestDir, fixture: &Fixture) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, fixture);
    h.answer_scrollback("s1", b"");
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());
    h.sent();
    h
}

/// The size pane `p1` draws at.
fn pane_size(h: &mut Harness<'_>) -> u16 {
    let size = h
        .root(|root, cx| root.pane_font("p1", cx).map(|font| font.size))
        .expect("the pane has a font");
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a font size is a whole number of pixels"
    )]
    let size = size as u16;
    size
}

/// The appearances of the `SetSessionAppearance`s sent for `session`.
fn session_appearances(sent: &[ClientMessage], session: &str) -> Vec<AppearanceOverrides> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetSessionAppearance {
                session_id,
                appearance,
                ..
            } if session_id == session => Some(appearance.clone()),
            _ => None,
        })
        .collect()
}

/// The `request_id`s of the `SetSessionAppearance`s sent for `session`.
fn appearance_request_ids(sent: &[ClientMessage], session: &str) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetSessionAppearance {
                session_id,
                request_id,
                ..
            } if session_id == session => request_id.clone(),
            _ => None,
        })
        .collect()
}

/// The column counts of the `Resize`s in `sent` for session `id`, in order.
fn resize_cols(sent: &[ClientMessage], id: &str) -> Vec<u16> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::Resize {
                session_id, cols, ..
            } if session_id == id => Some(*cols),
            _ => None,
        })
        .collect()
}

/// A session whose own appearance sets its size to `size`.
fn sized(size: u16) -> SessionSnapshot {
    let mut s = session("s1").build();
    s.appearance.terminal_font_size = Some(size);
    s
}

/// Repo `r1`, whose own appearance sets its size to `size`.
fn sized_repo(size: u16) -> RepoEntry {
    let mut container = repo("r1", "C:/repos/r1");
    container.appearance.terminal_font_size = Some(size);
    container
}

/// Workspace `w1` over `r1`, whose own appearance sets its size to `size`.
fn sized_workspace(size: u16) -> WorkspaceEntry {
    let mut container = workspace("w1", &["r1"]);
    container.appearance.terminal_font_size = Some(size);
    container
}

/// The saved layout as JSON.
fn saved(dir: &TestDir) -> serde_json::Value {
    serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("native-ui.json")).expect("native-ui.json"),
    )
    .expect("native-ui.json is JSON")
}

#[gpui::test]
fn ctrl_plus_sends_session_size_from_the_resolved_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut container = repo("r1", "C:/repos/r1");
    container.appearance.terminal_font_size = Some(20);
    let mut s = session("s1").in_repo("r1").build();
    s.appearance.accent_color = Some("#ff8800".to_owned());
    let fixture = Fixture {
        repos: vec![container],
        sessions: vec![s],
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
        ..Fixture::default()
    };
    let mut h = focused(cx, &dir, &fixture);
    assert_eq!(pane_size(&mut h), 20, "the repo's size, not the app's");

    h.keys("ctrl-=");

    let sent = h.sent();
    let appearances = session_appearances(&sent, "s1");
    assert_eq!(appearances.len(), 1, "sent {sent:?}");
    assert_eq!(
        appearances[0].terminal_font_size,
        Some(21),
        "one above the resolved 20"
    );
    assert_eq!(
        appearances[0].accent_color.as_deref(),
        Some("#ff8800"),
        "the session's other appearance fields ride along"
    );
}

#[gpui::test]
fn ctrl_minus_at_eight_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = focused(cx, &dir, &Fixture::single(sized(8)));
    assert_eq!(pane_size(&mut h), 8, "the session's own size");

    h.keys("ctrl--");
    assert!(h.sent().is_empty(), "the clamp holds: nothing goes out");

    h.keys("ctrl-=");
    let sent = h.sent();
    let appearances = session_appearances(&sent, "s1");
    assert_eq!(
        appearances.first().map(|a| a.terminal_font_size),
        Some(Some(9)),
        "the other direction still steps: sent {sent:?}"
    );
}

#[gpui::test]
fn ctrl_shift_plus_sets_the_tab_override_and_resizes_the_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"");
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());
    let before = *resize_cols(&h.sent(), "s1")
        .last()
        .expect("the pane sized its PTY");

    h.keys("ctrl-shift-=");

    assert_eq!(
        h.root(|root, _| root.tab_font_override("t1")),
        Some(14.0),
        "one above the resolved 13"
    );
    let sent = h.sent();
    assert!(
        session_appearances(&sent, "s1").is_empty(),
        "the tab level leaves the session alone: {sent:?}"
    );
    let after = *resize_cols(&sent, "s1")
        .last()
        .expect("the new size resizes the PTY");
    assert!(
        after < before,
        "a larger font fits fewer columns: {before} -> {after}"
    );
}

#[gpui::test]
fn tab_override_wins_over_session_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = focused(cx, &dir, &Fixture::single(sized(20)));
    assert_eq!(pane_size(&mut h), 20, "the session's size");

    h.keys("ctrl-shift--");

    assert_eq!(h.root(|root, _| root.tab_font_override("t1")), Some(19.0));
    assert_eq!(pane_size(&mut h), 19, "the tab beats the session's 20");
    assert!(
        session_appearances(&h.sent(), "s1").is_empty(),
        "the session is untouched"
    );
}

#[gpui::test]
fn ctrl_zero_clears_session_size_and_tab_override(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = focused(cx, &dir, &Fixture::single(sized(20)));
    h.keys("ctrl-shift--");
    assert_eq!(pane_size(&mut h), 19);
    h.sent();

    h.keys("ctrl-0");

    let sent = h.sent();
    let appearances = session_appearances(&sent, "s1");
    assert_eq!(
        appearances.first().map(|a| a.terminal_font_size),
        Some(None),
        "the session's size is cleared: {sent:?}"
    );
    assert_eq!(
        h.root(|root, _| root.tab_font_override("t1")),
        None,
        "the tab override is gone"
    );
    assert_eq!(
        pane_size(&mut h),
        20,
        "the session's own size returns until the daemon echoes the clear"
    );

    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").build(),
        request_id: None,
    });
    assert_eq!(pane_size(&mut h), 13, "both fall back to the app default");
}

#[gpui::test]
fn tab_menu_shows_the_size_and_its_rows_work(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = focused(cx, &dir, &Fixture::single(session("s1").build()));

    h.right_click_on("tab-t1");
    assert!(h.in_model("tab-menu"), "the right-click opened the menu");
    assert_eq!(
        h.root(|root, _| root.tab_menu().map(str::to_owned)),
        Some("t1".to_owned())
    );
    assert_eq!(
        h.root(|root, _| root.tab_menu_size()),
        Some(13.0),
        "the header shows the resolved size"
    );

    h.click_on("tab-menu-reset");
    assert_eq!(
        h.root(|root, _| root.tab_font_override("t1")),
        None,
        "Reset is inert with no override to clear"
    );

    h.click_on("tab-menu-increase");
    assert_eq!(h.root(|root, _| root.tab_font_override("t1")), Some(14.0));
    assert_eq!(pane_size(&mut h), 14);
    assert_eq!(
        h.root(|root, _| root.tab_menu_size()),
        Some(14.0),
        "the header follows the change"
    );

    h.click_on("tab-menu-decrease");
    assert_eq!(h.root(|root, _| root.tab_font_override("t1")), Some(13.0));

    h.click_on("tab-menu-reset");
    assert_eq!(
        h.root(|root, _| root.tab_font_override("t1")),
        None,
        "Reset clears the override"
    );
    assert!(!h.in_model("tab-menu"), "and closes the menu");
    assert_eq!(pane_size(&mut h), 13, "the pane falls back");
}

#[gpui::test]
fn overrides_for_closed_tabs_are_pruned(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "tab_font_sizes": { "t1": 20.0, "t2": 24.0 } }"#,
    )
    .expect("write the saved layout");

    let mut h = focused(cx, &dir, &Fixture::single(session("s1").build()));

    assert_eq!(
        h.root(|root, _| root.tab_font_override("t1")),
        Some(20.0),
        "the live tab keeps its override"
    );
    assert_eq!(
        h.root(|root, _| root.tab_font_override("t2")),
        None,
        "the tab the daemon did not list lost its"
    );
    assert_eq!(pane_size(&mut h), 20, "the override still draws");

    let saved = saved(&dir);
    assert!(
        saved["tab_font_sizes"].get("t2").is_none(),
        "and it is not saved back: {saved}"
    );
}

#[gpui::test]
fn session_size_from_the_daemon_applies_live(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = focused(cx, &dir, &Fixture::single(session("s1").build()));
    assert_eq!(pane_size(&mut h), 13, "the app default");

    h.send(DaemonMessage::SessionUpdated {
        session: sized(18),
        request_id: None,
    });
    assert_eq!(pane_size(&mut h), 18, "the pane follows the snapshot");

    h.keys("ctrl-shift-=");
    assert_eq!(pane_size(&mut h), 19, "one above the session's 18");

    h.send(DaemonMessage::SessionUpdated {
        session: sized(11),
        request_id: None,
    });
    assert_eq!(pane_size(&mut h), 19, "the tab override still wins");
}

#[gpui::test]
fn session_step_ignores_the_tab_override(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "tab_font_sizes": { "t1": 20.0 } }"#,
    )
    .expect("write the saved layout");
    let mut h = focused(cx, &dir, &Fixture::single(session("s1").build()));
    assert_eq!(pane_size(&mut h), 20, "the tab's override draws");

    h.keys("ctrl-=");

    let sent = h.sent();
    let appearances = session_appearances(&sent, "s1");
    assert_eq!(appearances.len(), 1, "sent {sent:?}");
    assert_eq!(
        appearances[0].terminal_font_size,
        Some(14),
        "one above the size the session resolves to without the tab's override"
    );
    assert_eq!(
        pane_size(&mut h),
        20,
        "the pane still draws the tab's override"
    );
}

#[gpui::test]
fn repo_size_change_applies_live(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![sized_repo(20)],
        sessions: vec![session("s1").in_repo("r1").build()],
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
        ..Fixture::default()
    };
    let mut h = focused(cx, &dir, &fixture);
    assert_eq!(pane_size(&mut h), 20, "the repo's size");

    h.send(DaemonMessage::Repos {
        repos: vec![sized_repo(18)],
    });

    assert_eq!(pane_size(&mut h), 18, "the repo's new size applies live");
}

#[gpui::test]
fn workspace_size_change_applies_live(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![sized_repo(18)],
        workspaces: vec![sized_workspace(20)],
        sessions: vec![session("s1").in_repo("r1").in_workspace("w1").build()],
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
    };
    let mut h = focused(cx, &dir, &fixture);
    assert_eq!(pane_size(&mut h), 20, "the workspace beats the repo's 18");

    h.send(DaemonMessage::Workspaces {
        workspaces: vec![sized_workspace(16)],
    });

    assert_eq!(pane_size(&mut h), 16, "its new size applies live");
}

#[gpui::test]
fn dropping_the_connection_closes_the_tab_menu(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = focused(cx, &dir, &Fixture::single(session("s1").build()));
    h.right_click_on("tab-t1");
    assert!(h.in_model("tab-menu"), "the right-click opened the menu");

    h.lose_connection();

    assert!(!h.in_model("tab-menu"), "the connecting overlay closes it");
}

#[gpui::test]
fn held_keys_step_from_the_pending_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = focused(cx, &dir, &Fixture::single(session("s1").build()));
    assert_eq!(pane_size(&mut h), 13, "the app default");

    h.keys("ctrl-=");
    h.keys("ctrl-=");

    let sent = h.sent();
    let sizes: Vec<Option<u16>> = session_appearances(&sent, "s1")
        .iter()
        .map(|appearance| appearance.terminal_font_size)
        .collect();
    assert_eq!(
        sizes,
        vec![Some(14), Some(15)],
        "the second press steps from the first, un-echoed: {sent:?}"
    );

    h.send(DaemonMessage::SessionUpdated {
        session: sized(14),
        request_id: None,
    });
    h.keys("ctrl-=");
    let sent = h.sent();
    assert_eq!(
        session_appearances(&sent, "s1")
            .first()
            .and_then(|appearance| appearance.terminal_font_size),
        Some(16),
        "a broadcast answers no send, so the press steps from 15 still on its way"
    );

    let last = appearance_request_ids(&sent, "s1")
        .pop()
        .expect("the send carries a request id");
    h.send(DaemonMessage::SessionUpdated {
        session: sized(16),
        request_id: Some(last),
    });
    h.send(DaemonMessage::SessionUpdated {
        session: sized(20),
        request_id: None,
    });
    h.keys("ctrl-=");
    assert_eq!(
        session_appearances(&h.sent(), "s1")
            .first()
            .and_then(|appearance| appearance.terminal_font_size),
        Some(21),
        "answering the last send answers every send before it"
    );
}

#[gpui::test]
fn ctrl_shift_minus_takes_the_underscore_key(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = focused(cx, &dir, &Fixture::single(session("s1").build()));

    h.keys("ctrl-shift-_");

    assert_eq!(
        h.root(|root, _| root.tab_font_override("t1")),
        Some(12.0),
        "the key a shifted - types steps the tab's override"
    );
    assert_eq!(pane_size(&mut h), 12);
}

#[gpui::test]
fn ctrl_zero_with_nothing_to_clear_sends_and_saves_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = focused(cx, &dir, &Fixture::single(session("s1").build()));
    let before = saved(&dir);

    h.keys("ctrl-0");

    assert!(h.sent().is_empty(), "nothing to clear, nothing sent");
    assert_eq!(saved(&dir), before, "and the layout is not written");
}

#[gpui::test]
fn tab_steps_debounce_the_save(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = focused(cx, &dir, &Fixture::single(session("s1").build()));
    let before = saved(&dir);

    h.keys("ctrl-shift-=");
    h.keys("ctrl-shift-=");
    h.advance(Duration::from_millis(499));

    assert_eq!(saved(&dir), before, "under the debounce nothing is written");

    h.advance(Duration::from_millis(1));

    assert_eq!(
        saved(&dir)["tab_font_sizes"]["t1"].as_f64(),
        Some(15.0),
        "the save that fires writes the size the steps reached"
    );
}

#[gpui::test]
fn quitting_flushes_the_pending_tab_save(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = focused(cx, &dir, &Fixture::single(session("s1").exited(0).build()));
    h.keys("ctrl-shift-=");
    assert!(
        saved(&dir)["tab_font_sizes"].get("t1").is_none(),
        "the save is still pending"
    );

    assert!(
        h.cx.simulate_close(),
        "nothing to ask about, so the window closes"
    );
    h.cx.run_until_parked();

    assert_eq!(
        saved(&dir)["tab_font_sizes"]["t1"].as_f64(),
        Some(14.0),
        "the quit path writes it"
    );
}

#[gpui::test]
fn overrides_survive_a_tab_update_before_the_first_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "tab_font_sizes": { "t1": 20.0 } }"#,
    )
    .expect("write the saved layout");

    let mut h = Harness::open(cx, &dir);
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t2", &pane("p2", None)),
    });

    assert_eq!(
        h.root(|root, _| root.tab_font_override("t1")),
        Some(20.0),
        "an update for one tab prunes nothing before the full list"
    );
}
