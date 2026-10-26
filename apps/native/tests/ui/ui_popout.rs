//! Pop-out window specs: a pane's terminal opened in a second window over
//! the same connection takes keys there, and closing the window docks the
//! pane back into its grid.

use crate::support;

use gpui::{Modifiers, TestAppContext, px, size};
use protocol::{ClientMessage, DaemonMessage, TabEntry};
use rustling_tulip_native::{OpenOutcome, POPPED_OUT_NOTE, PopoutKey};
use serde_json::json;
use support::{Fixture, Harness, TestDir, pane, repo, session, tab};

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

/// Whether the client asked the daemon to close, stop or rearrange
/// anything: a pop-out's open and close never do.
fn changes_layout_or_sessions(msg: &ClientMessage) -> bool {
    matches!(
        msg,
        ClientMessage::ClosePane { .. }
            | ClientMessage::CloseTab { .. }
            | ClientMessage::StopSession { .. }
            | ClientMessage::Shutdown { .. }
            | ClientMessage::CreateTab { .. }
            | ClientMessage::MovePane { .. }
            | ClientMessage::ExtractToNewTab { .. }
            | ClientMessage::SplitPane { .. }
            | ClientMessage::SwapPanes { .. }
            | ClientMessage::MergeTabs { .. }
            | ClientMessage::ReorderTabs { .. }
    )
}

fn popped(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.is_popped_out("t1", "p1"))
}

fn pane_key(tab_id: &str, pane_id: &str) -> PopoutKey {
    PopoutKey::Pane {
        tab_id: tab_id.to_owned(),
        pane_id: pane_id.to_owned(),
    }
}

/// Pops pane `pane_id` of tab `tab_id` out, as a menu entry would.
fn pop_out(h: &mut Harness<'_>, tab_id: &str, pane_id: &str) -> OpenOutcome {
    let key = pane_key(tab_id, pane_id);
    h.root_update(move |root, window, cx| root.open_popout(key, window, cx))
}

#[gpui::test]
fn popped_pane_takes_keys_in_its_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.sent();

    assert_eq!(pop_out(&mut h, "t1", "p1"), OpenOutcome::Opened);
    assert_eq!(h.window_count(), 2, "the pop-out has a window of its own");
    let mut popout = h.popout_window();
    let root = h.root.clone();
    let focused = popout.update(|window, cx| root.read(cx).pane_terminal_focused("p1", window, cx));
    assert!(focused, "the terminal has the keyboard in the pop-out");

    popout.simulate_keystrokes("a");
    assert_eq!(h.sent_input("s1"), b"a", "the key reached the session");
}

#[gpui::test]
fn popout_opens_at_its_size_and_is_titled(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture::single(session("s1").user_label("Build").build());
    let mut h = Harness::with(cx, &dir, &fixture);

    pop_out(&mut h, "t1", "p1");
    let mut popout = h.popout_window();

    assert_eq!(
        popout.update(|window, _| window.bounds().size),
        size(px(1100.0), px(720.0))
    );
    assert_eq!(
        popout.window_title().as_deref(),
        Some("Build"),
        "titled by the session bound to the pane"
    );
}

#[gpui::test]
fn main_grid_shows_a_placeholder_for_a_popped_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    assert_eq!(h.root(|root, _| root.pane_note("p1")), None);

    pop_out(&mut h, "t1", "p1");
    assert_eq!(
        h.root(|root, _| root.pane_note("p1")),
        Some(POPPED_OUT_NOTE)
    );
    assert_eq!(POPPED_OUT_NOTE, "Popped out to its own window");
    assert!(
        h.bounds("pane-popped-p1").origin.x >= px(0.0),
        "the placeholder is drawn"
    );

    let key = pane_key("t1", "p1");
    h.root_update(move |root, _, cx| root.dock_back(&key, cx));
    assert_eq!(
        h.root(|root, _| root.pane_note("p1")),
        None,
        "the terminal is back in the grid"
    );
    assert_eq!(h.window_count(), 1, "docking back closes the window");
}

#[gpui::test]
fn closing_the_popout_docks_the_pane_back(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    pop_out(&mut h, "t1", "p1");
    let before_close = h.sent();
    assert!(
        !before_close.iter().any(changes_layout_or_sessions),
        "opening sent nothing to the daemon: {before_close:?}"
    );

    assert!(h.close_popout(), "the pop-out may close");
    let on_close = h.sent();
    assert!(
        !on_close.iter().any(changes_layout_or_sessions),
        "closing sent nothing to the daemon: {on_close:?}"
    );
    assert_eq!(h.window_count(), 1);
    assert!(!popped(&mut h));
    assert_eq!(h.root(|root, _| root.popout_count()), 0);
    assert_eq!(h.root(|root, _| root.pane_note("p1")), None);

    let cell = h.cell_center("p1", 0, 0);
    h.click(cell, Modifiers::none());
    h.sent();
    h.keys("b");
    assert_eq!(
        h.sent_input("s1"),
        b"b",
        "the terminal draws and types in the main grid again"
    );
}

#[gpui::test]
fn opening_a_popped_pane_again_keeps_one_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    assert_eq!(pop_out(&mut h, "t1", "p1"), OpenOutcome::Opened);

    assert_eq!(
        pop_out(&mut h, "t1", "p1"),
        OpenOutcome::Existing(pane_key("t1", "p1"))
    );
    assert_eq!(h.window_count(), 2, "one pop-out window, not two");
    assert_eq!(h.root(|root, _| root.popout_count()), 1);
}

#[gpui::test]
fn app_shortcuts_do_nothing_in_a_popout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &three_tabs());
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned))
            .as_deref(),
        Some("t1")
    );
    pop_out(&mut h, "t1", "p1");

    let mut popout = h.popout_window();
    popout.simulate_keystrokes("ctrl-tab");
    h.cx.run_until_parked();

    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned))
            .as_deref(),
        Some("t1"),
        "Ctrl+Tab in a pop-out switches no tab"
    );
}

#[gpui::test]
fn quit_flow_ignores_a_popout_close(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture::single(session("s1").in_repo("r1").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    pop_out(&mut h, "t1", "p1");

    assert!(h.close_popout());

    assert!(
        !h.root(|root, _| root.exit_dialog_open()),
        "a pop-out's close never asks about the sessions"
    );
    assert_eq!(h.quit_requests(), 0, "the app stays");
    assert_eq!(h.window_count(), 1, "the main window stays open");
}

#[gpui::test]
fn main_window_close_closes_the_popouts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture::single(session("s1").in_repo("r1").exited(0).build());
    let mut h = Harness::with(cx, &dir, &fixture);
    pop_out(&mut h, "t1", "p1");
    assert_eq!(h.window_count(), 2);

    assert!(h.cx.simulate_close(), "nothing to ask about: it may close");
    h.cx.run_until_parked();

    assert_eq!(h.quit_requests(), 1);
    assert_eq!(h.window_count(), 1, "the pop-out window closed first");
    assert_eq!(h.root(|root, _| root.popout_count()), 0);
}

/// Whether `sent` holds a claimed `Resize` for `session`.
fn claims_size(sent: &[ClientMessage], session: &str) -> bool {
    sent.iter().any(|msg| {
        matches!(
            msg,
            ClientMessage::Resize { session_id, claim: true, .. } if session_id == session
        )
    })
}

#[gpui::test]
fn a_moved_popped_pane_draws_in_one_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    pop_out(&mut h, "t1", "p1");

    // The daemon moved p1 out of t1 into a tab of its own.
    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t2", &pane("p1", Some("s1")))],
    });

    let mut popout = h.popout_window();
    popout.run_until_parked();
    assert!(
        popout.debug_bounds("popout-pane-gone").is_some(),
        "the pop-out shows the pane is no longer open"
    );
    assert!(!h.root(|root, _| root.is_popped_out("t2", "p1")));
    assert_eq!(h.root(|root, _| root.pane_note("p1")), None);
    let cell = h.cell_center("p1", 0, 0);
    h.click(cell, Modifiers::none());
    h.sent();
    h.keys("b");
    assert_eq!(
        h.sent_input("s1"),
        b"b",
        "the main grid draws p1's terminal"
    );
}

#[gpui::test]
fn popping_out_sends_a_claimed_resize(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.sent();

    pop_out(&mut h, "t1", "p1");
    let _ = h.popout_window();
    let opened = h.sent();
    assert!(
        claims_size(&opened, "s1"),
        "the pop-out's size claims the session: {opened:?}"
    );

    let key = pane_key("t1", "p1");
    h.root_update(move |root, _, cx| root.dock_back(&key, cx));
    let _ = h.bounds("pane-grid-p1");
    let docked = h.sent();
    assert!(
        claims_size(&docked, "s1"),
        "the grid's size claims it back: {docked:?}"
    );
}

#[gpui::test]
fn docking_back_before_the_window_opens_leaves_no_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));

    let key = pane_key("t1", "p1");
    h.root_update(move |root, window, cx| {
        root.open_popout(key.clone(), window, cx);
        root.dock_back(&key, cx);
    });

    assert_eq!(
        h.window_count(),
        1,
        "no pop-out window outlives its dock-back"
    );
    assert_eq!(h.root(|root, _| root.popout_count()), 0);
}
