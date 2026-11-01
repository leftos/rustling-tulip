//! Pop-out window specs: a pane's terminal opened in a second window over
//! the same connection takes keys there, and closing the window docks the
//! pane back into its grid.

use crate::support;

use gpui::{
    App, AppContext as _, EntityId, InputHandler, Modifiers, MouseButton, MouseDownEvent, Pixels,
    Point, TestAppContext, VisualContext as _, VisualTestContext, Window, WindowId, point, px,
    size,
};
use protocol::{
    AttentionReason, ClientMessage, DaemonMessage, PaneDropEdge, SplitDirection, TabEntry,
};
use rustling_tulip_native::target::{HostKey, Target};
use rustling_tulip_native::{
    Connection, NetEvent, OpenOutcome, POPPED_OUT_NOTE, PopoutKey, RootView, ToastKind, ToastSpec,
};
use serde_json::json;
use support::{
    Fixture, Harness, PROTOCOL, REMOTE, TestDir, mixed_tabs, pane, remote_lists, remote_pane, repo,
    saved_remote, saved_remote_over, saved_ui, seed_ui, session, split, tab, two_tab_fixture,
    two_tabs,
};

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
    let header = h.center("pane-header-p1");

    pop_out(&mut h, "t1", "p1");
    assert_eq!(
        h.root(|root, _| root.pane_note("p1")),
        Some(POPPED_OUT_NOTE)
    );
    assert_eq!(POPPED_OUT_NOTE, "Popped out to its own window");
    assert!(
        h.bounds("pane-popped-p1").origin.x >= px(0.0),
        "the card is drawn"
    );
    for part in ["popped-label-p1", "popped-focus-p1", "popped-dock-p1"] {
        assert!(h.bounds(part).origin.x >= px(0.0), "{part} is drawn");
    }
    assert!(
        !header_takes_a_right_click(&mut h, header),
        "the card replaces the pane, header and all"
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

    // The daemon extracted p1 out of t1 into a tab of its own: the new tab
    // first, then the emptied source.
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t2", &pane("p1", Some("s1"))),
    });
    h.send(DaemonMessage::TabRemoved {
        tab_id: "t1".to_owned(),
    });

    let mut popout = h.popout_window();
    popout.run_until_parked();
    assert!(
        popout.debug_bounds("popout-pane-gone").is_none(),
        "the pop-out still draws p1's terminal"
    );
    assert!(h.root(|root, _| root.is_popped_out("t2", "p1")));
    assert_eq!(
        h.root(|root, _| root.pane_note("p1")),
        Some(POPPED_OUT_NOTE),
        "the main grid shows the card, not a second terminal"
    );
    h.sent();
    popout.simulate_keystrokes("b");
    assert_eq!(
        h.sent_input("s1"),
        b"b",
        "the pop-out's terminal takes the keys"
    );
}

#[gpui::test]
fn moved_to_another_tab_popped_pane_keeps_its_popout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    fixture.tabs.push(tab("t2", &pane("p2", Some("s2"))));
    let mut h = Harness::with(cx, &dir, &fixture);
    pop_out(&mut h, "t1", "p1");

    move_p1_into_t2(&mut h);

    assert!(
        h.root(|root, _| root.is_popped_out("t2", "p1")),
        "the pop-out follows its pane to t2"
    );
    assert_eq!(h.window_count(), 2, "the pop-out window stays open");
    assert_eq!(h.root(|root, _| root.popout_count()), 1);
    let mut popout = h.popout_window();
    popout.run_until_parked();
    assert!(popout.debug_bounds("popout-pane-gone").is_none());
    assert_eq!(popout.window_title().as_deref(), Some("s1"));
    h.sent();
    popout.simulate_keystrokes("c");
    assert_eq!(h.sent_input("s1"), b"c", "keys still reach the session");

    assert!(h.close_popout(), "the re-keyed pop-out may close");
    assert_eq!(
        h.root(|root, _| root.popout_count()),
        0,
        "its close request docks the moved pane back"
    );
    assert_eq!(h.root(|root, _| root.pane_note("p1")), None);
}

/// The daemon's events for moving `p1`, alone in `t1`, into `t2` beside
/// `p2`: the destination first, then the emptied source.
fn move_p1_into_t2(h: &mut Harness<'_>) {
    h.send(DaemonMessage::TabUpdated {
        tab: tab(
            "t2",
            &split(
                SplitDirection::Horizontal,
                pane("p2", Some("s2")),
                pane("p1", Some("s1")),
            ),
        ),
    });
    h.send(DaemonMessage::TabRemoved {
        tab_id: "t1".to_owned(),
    });
}

#[gpui::test]
fn a_resync_that_moved_a_popped_pane_keeps_its_popout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    fixture.tabs.push(tab("t2", &pane("p2", Some("s2"))));
    let mut h = Harness::with(cx, &dir, &fixture);
    pop_out(&mut h, "t1", "p1");

    // A resync delivers the move as one tab list.
    h.send(DaemonMessage::Tabs {
        tabs: vec![tab(
            "t2",
            &split(
                SplitDirection::Horizontal,
                pane("p2", Some("s2")),
                pane("p1", Some("s1")),
            ),
        )],
    });

    assert!(h.root(|root, _| root.is_popped_out("t2", "p1")));
    assert_eq!(h.window_count(), 2, "the pop-out window stays open");
    let mut popout = h.popout_window();
    popout.run_until_parked();
    assert!(popout.debug_bounds("popout-pane-gone").is_none());
    h.sent();
    popout.simulate_keystrokes("c");
    assert_eq!(h.sent_input("s1"), b"c", "keys still reach the session");
}

/// The size of the last claimed `Resize` `sent` holds for `session`, as
/// `(cols, rows)`.
fn claimed_size(sent: &[ClientMessage], session: &str) -> Option<(u16, u16)> {
    sent.iter().rev().find_map(|msg| match msg {
        ClientMessage::Resize {
            session_id,
            cols,
            rows,
            claim: true,
        } if session_id == session => Some((*cols, *rows)),
        _ => None,
    })
}

#[gpui::test]
fn popped_pane_off_the_active_tab_drives_its_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &three_tabs());
    pop_out(&mut h, "t1", "p1");
    let popout = h.popout_window();
    h.click_on("tab-t2");
    h.sent();

    popout.simulate_resize(size(px(900.0), px(600.0)));
    popout.run_until_parked();

    let sent = h.sent();
    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::Resize { session_id, .. } if session_id == "s1"
        )),
        "resizing the pop-out resizes its session while t2 is on screen: {sent:?}"
    );
}

#[gpui::test]
fn popped_pane_answers_queries_off_the_active_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.tabs.push(tab("t2", &pane("p2", Some("s1"))));
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.sent();
    pop_out(&mut h, "t1", "p1");
    let _ = h.popout_window();
    let (cols, rows) = claimed_size(&h.sent(), "s1").expect("the pop-out claims s1's size");
    h.click_on("tab-t2");
    h.sent();

    // The cursor parks in the bottom-right cell, so its report names the
    // answering pane's grid size.
    h.pty("s1", b"\x1b[999;999H\x1b[6n");

    assert_eq!(
        String::from_utf8_lossy(&h.sent_input("s1")),
        format!("\x1b[{rows};{cols}R"),
        "only the pop-out's terminal answers, from its own grid"
    );
}

#[gpui::test]
fn main_window_never_focuses_a_popped_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &three_tabs());
    pop_out(&mut h, "t1", "p1");
    let _ = h.popout_window();

    h.click_on("tab-t2");
    h.click_on("tab-t1");

    let root = h.root.clone();
    let in_main =
        h.cx.update(|window, cx| root.read(cx).pane_terminal_focused("p1", window, cx));
    assert!(
        !in_main,
        "the main window gives the popped pane's terminal no focus"
    );
}

/// Drives `pane`'s text input in the pop-out window.
fn popout_input<R>(
    h: &Harness<'_>,
    popout: &mut VisualTestContext,
    pane: &str,
    drive: impl FnOnce(&mut dyn InputHandler, &mut Window, &mut App) -> R,
) -> R {
    let root = h.root.clone();
    let result = popout.update(|window, cx| {
        let mut handler = root
            .read(cx)
            .pane_input_handler(pane)
            .expect("the pane has a terminal");
        drive(&mut handler, window, cx)
    });
    popout.run_until_parked();
    result
}

fn popout_marked_text(h: &Harness<'_>, popout: &mut VisualTestContext) -> Option<String> {
    popout_input(h, popout, "p1", |handler, window, cx| {
        let range = handler.marked_text_range(window, cx)?;
        handler.text_for_range(range, &mut None, window, cx)
    })
}

#[gpui::test]
fn blur_in_the_popout_drops_marked_text(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"");
    pop_out(&mut h, "t1", "p1");
    let mut popout = h.popout_window();
    // A test window starts inactive, and gpui reports no focus change in one.
    popout.update(|window, _| window.activate_window());
    popout_input(&h, &mut popout, "p1", |handler, window, cx| {
        handler.replace_and_mark_text_in_range(None, "´", None, window, cx);
    });
    assert_eq!(popout_marked_text(&h, &mut popout).as_deref(), Some("´"));

    popout.update(|window, _| window.blur());
    popout.run_until_parked();

    assert_eq!(
        popout_marked_text(&h, &mut popout),
        None,
        "a blur in the pop-out drops its composition"
    );
}

#[gpui::test]
fn popout_title_follows_a_rename(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture::single(session("s1").user_label("Build").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    pop_out(&mut h, "t1", "p1");
    let mut popout = h.popout_window();
    assert_eq!(popout.window_title().as_deref(), Some("Build"));

    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").user_label("Deploy").build(),
        request_id: None,
    });
    popout.run_until_parked();

    assert_eq!(
        popout.window_title().as_deref(),
        Some("Deploy"),
        "the pop-out's title follows the session's new name"
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

#[gpui::test]
fn a_drop_on_a_popped_card_changes_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    fixture.sessions.push(session("s3").build());
    fixture.tabs = vec![tab(
        "t1",
        &split(
            SplitDirection::Horizontal,
            pane("p1", Some("s1")),
            pane("p2", Some("s2")),
        ),
    )];
    let mut h = Harness::with(cx, &dir, &fixture);
    pop_out(&mut h, "t1", "p1");
    h.sent();
    let card = h.center("pane-popped-p1");

    let header = h.center("pane-header-p2");
    h.drag_and_drop(header, card);
    let leaf = h.center("leaf-s3");
    h.drag_and_drop(leaf, card);

    let sent = h.sent();
    assert!(
        !sent.iter().any(|msg| changes_layout_or_sessions(msg)
            || matches!(msg, ClientMessage::ReplacePaneSession { .. })),
        "a drop on the card asks the daemon for nothing: {sent:?}"
    );
    assert!(popped(&mut h), "the pop-out stays open");
    let mut popout = h.popout_window();
    popout.run_until_parked();
    assert_eq!(
        popout.window_title().as_deref(),
        Some("s1"),
        "the pop-out still shows its session"
    );
}

#[gpui::test]
fn focus_window_after_a_cross_tab_move_focuses_no_hidden_terminal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    fixture.tabs.push(tab("t2", &pane("p2", Some("s2"))));
    let mut h = Harness::with(cx, &dir, &fixture);
    pop_out(&mut h, "t1", "p1");
    move_p1_into_t2(&mut h);
    let mut popout = h.popout_window();
    popout.update(|window, _| window.blur());
    h.cx.update(|window, _| window.blur());

    let key = pane_key("t1", "p1");
    let outcome = h.root_update(move |root, window, cx| root.open_popout(key, window, cx));

    assert_ne!(outcome, OpenOutcome::Opened, "the old key opens nothing");
    assert_eq!(h.window_count(), 2, "one pop-out window in all");
    assert_eq!(h.root(|root, _| root.popout_count()), 1);
    let root = h.root.clone();
    let in_popout =
        popout.update(|window, cx| root.read(cx).pane_terminal_focused("p1", window, cx));
    let root = h.root.clone();
    let in_main =
        h.cx.update(|window, cx| root.read(cx).pane_terminal_focused("p1", window, cx));
    assert!(
        !in_popout && !in_main,
        "the old key focuses no terminal in either window \
         (pop-out {in_popout}, main {in_main})"
    );
}

/// Whether a right-click where a pane header was opens a session menu, as a
/// drawn header does; a stale debug bound cannot say whether it still is.
fn header_takes_a_right_click(h: &mut Harness<'_>, at: Point<Pixels>) -> bool {
    h.right_click(at);
    let opened = !h.root(|root, _| root.menu_groups()).is_empty();
    if opened {
        h.keys("escape");
    }
    opened
}

/// Whether `window` is the active window.
fn is_active(window: &mut VisualTestContext) -> bool {
    window.run_until_parked();
    window.update(|window, _| window.is_window_active())
}

#[gpui::test]
fn header_pop_out_opens_a_window_and_leaves_a_card(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.sent();
    let header = h.center("pane-header-p1");
    assert!(
        header_takes_a_right_click(&mut h, header),
        "a drawn header opens the session menu"
    );

    h.click_on("pane-popout-p1");

    assert_eq!(h.window_count(), 2, "the pane has a window of its own");
    assert!(popped(&mut h));
    let sent = h.sent();
    assert!(
        !sent.iter().any(changes_layout_or_sessions),
        "popping out asks the daemon for nothing: {sent:?}"
    );
    for part in [
        "pane-popped-p1",
        "popped-label-p1",
        "popped-focus-p1",
        "popped-dock-p1",
    ] {
        assert!(h.bounds(part).origin.x >= px(0.0), "{part} is drawn");
    }
    assert_eq!(
        h.root(|root, _| root.pane_note("p1")),
        Some(POPPED_OUT_NOTE)
    );
    assert!(
        !header_takes_a_right_click(&mut h, header),
        "the popped pane's header is not drawn in the main grid"
    );
}

#[gpui::test]
fn dock_back_from_the_card_closes_the_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    pop_out(&mut h, "t1", "p1");
    assert_eq!(h.window_count(), 2);
    h.sent();

    h.click_on("popped-dock-p1");

    assert_eq!(h.window_count(), 1, "the pop-out window closed");
    assert!(!popped(&mut h));
    assert_eq!(h.root(|root, _| root.popout_count()), 0);
    assert_eq!(
        h.root(|root, _| root.pane_note("p1")),
        None,
        "the terminal is back in the grid"
    );
    let sent = h.sent();
    assert!(
        !sent.iter().any(changes_layout_or_sessions),
        "docking back asks the daemon for nothing: {sent:?}"
    );
}

#[gpui::test]
fn focus_window_raises_the_open_popout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    pop_out(&mut h, "t1", "p1");
    let mut popout = h.popout_window();
    popout.update(|window, _| window.blur());
    h.cx.update(|window, _| window.activate_window());
    assert!(!is_active(&mut popout), "the main window is in front");

    h.click_on("popped-focus-p1");

    assert!(is_active(&mut popout), "the pop-out window is raised");
    let root = h.root.clone();
    let focused = popout.update(|window, cx| root.read(cx).pane_terminal_focused("p1", window, cx));
    assert!(focused, "its terminal has the keyboard");
    assert_eq!(h.window_count(), 2, "no second pop-out window");
    assert_eq!(h.root(|root, _| root.popout_count()), 1);
}

#[gpui::test]
fn session_menu_pops_the_active_tab_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.tabs.push(tab("t2", &pane("p2", Some("s1"))));
    let mut h = Harness::with(cx, &dir, &fixture);
    h.click_on("tab-t2");

    h.right_click_on("leaf-s1");
    assert_eq!(
        h.root(|root, _| root.menu_row_label("session-menu-popout")),
        Some("Pop out")
    );
    h.click_on("session-menu-popout");

    assert!(
        h.root(|root, _| root.is_popped_out("t2", "p2")),
        "the pane in the tab on screen pops"
    );
    assert!(!popped(&mut h), "t1's pane stays in its grid");
    assert_eq!(h.window_count(), 2);

    h.right_click_on("leaf-s1");
    assert_eq!(
        h.root(|root, _| root.menu_disabled_tip("session-menu-popout")),
        Some("Already in a pop-out")
    );
    h.click_on("session-menu-popout");
    assert_eq!(
        h.root(|root, _| root.popout_count()),
        1,
        "the dimmed row does nothing"
    );
    h.keys("escape");

    h.click_on("tab-t1");
    h.click_on("pane-popout-p1");
    assert_eq!(
        h.root(|root, _| root.popout_count()),
        1,
        "the header's Pop out is dimmed while a pop-out shows its session"
    );
    assert!(!popped(&mut h));
}

#[gpui::test]
fn moved_popped_pane_keeps_its_popout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    fixture.tabs = vec![tab(
        "t1",
        &split(
            SplitDirection::Horizontal,
            pane("p1", Some("s1")),
            pane("p3", Some("s2")),
        ),
    )];
    let mut h = Harness::with(cx, &dir, &fixture);
    pop_out(&mut h, "t1", "p1");

    // The daemon swapped p1 and p3 within t1: p1 keeps its id.
    h.send(DaemonMessage::Tabs {
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p3", Some("s2")),
                pane("p1", Some("s1")),
            ),
        )],
    });

    assert!(popped(&mut h), "the pop-out stays open");
    assert_eq!(h.window_count(), 2);
    let mut popout = h.popout_window();
    popout.run_until_parked();
    assert!(
        popout.debug_bounds("popout-pane-gone").is_none(),
        "the pop-out still draws the pane's terminal"
    );
    assert!(
        h.bounds("popped-dock-p1").origin.x >= px(0.0),
        "the main grid still shows the card"
    );
    h.sent();
    popout.simulate_keystrokes("c");
    assert_eq!(h.sent_input("s1"), b"c", "keys still reach the session");
}

/// Whether pane `pane_id`'s terminal has the main window's keyboard.
fn focused_in_main(h: &mut Harness<'_>, pane_id: &str) -> bool {
    let root = h.root.clone();
    h.cx.update(|window, cx| root.read(cx).pane_terminal_focused(pane_id, window, cx))
}

/// Whether pane `pane_id`'s terminal has the pop-out window's keyboard.
fn focused_in_popout(h: &Harness<'_>, popout: &mut VisualTestContext, pane_id: &str) -> bool {
    let root = h.root.clone();
    popout.update(|window, cx| root.read(cx).pane_terminal_focused(pane_id, window, cx))
}

/// Clicks `p1`'s first cell, giving its terminal the main window's keyboard.
fn focus_p1_in_main(h: &mut Harness<'_>) {
    let cell = h.cell_center("p1", 0, 0);
    h.click(cell, Modifiers::none());
    assert!(
        focused_in_main(h, "p1"),
        "p1 has the main window's keyboard"
    );
}

/// `s1` of `r1` in `t1`: with a repo registered, Ctrl+N opens the spawn
/// dialog itself.
fn with_a_repo() -> Fixture {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    fixture
}

/// The main window no longer types into the popped `p1`: its app shortcuts
/// work there again.
fn assert_main_lets_go_of_p1(h: &mut Harness<'_>) {
    assert!(popped(h), "p1 is popped out");
    assert!(
        !focused_in_main(h, "p1"),
        "the main window's keyboard left the popped pane"
    );
    h.keys("ctrl-n");
    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "Ctrl+N in the main window opens the spawn dialog"
    );
    h.keys("escape");
    assert!(!h.root(|root, _| root.spawn_dialog_open()));

    let panel = h.bounds("sidebar-panel");
    h.click(
        point(panel.center().x, panel.bottom() - px(10.0)),
        Modifiers::none(),
    );
    let root = h.root.clone();
    let in_sidebar =
        h.cx.update(|window, cx| root.read(cx).sidebar_focused(window));
    assert!(in_sidebar, "a click on the sidebar still focuses it");
}

#[gpui::test]
fn popping_out_the_focused_pane_from_the_session_menu_unfocuses_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_a_repo());
    focus_p1_in_main(&mut h);

    h.right_click_on("leaf-s1");
    h.click_on("session-menu-popout");

    assert_main_lets_go_of_p1(&mut h);
}

#[gpui::test]
fn popping_out_the_focused_pane_from_its_header_unfocuses_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_a_repo());
    focus_p1_in_main(&mut h);

    h.click_on("pane-popout-p1");

    assert_main_lets_go_of_p1(&mut h);
}

fn detached(sent: &[ClientMessage], session: &str) -> bool {
    sent.iter()
        .any(|msg| matches!(msg, ClientMessage::Detach { session_id } if session_id == session))
}

/// After a cross-tab move: one pop-out window, keyed to `tab_id`, drawing
/// the terminal `before` named, with the keyboard, and `s1` never detached.
fn assert_popout_kept_p1(
    h: &mut Harness<'_>,
    popout: &mut VisualTestContext,
    tab_id: &str,
    before: Option<EntityId>,
) {
    assert_eq!(h.window_count(), 2, "one pop-out window");
    assert_eq!(h.root(|root, _| root.popout_count()), 1);
    assert!(
        h.root(|root, _| root.is_popped_out(tab_id, "p1")),
        "the pop-out follows p1 to {tab_id}"
    );
    assert_eq!(
        h.root(|root, _| root.pane_terminal_id("p1")),
        before,
        "p1 keeps its terminal"
    );
    let sent = h.sent();
    assert!(!detached(&sent, "s1"), "s1 stays attached: {sent:?}");
    popout.run_until_parked();
    assert!(popout.debug_bounds("popout-pane-gone").is_none());
    assert!(
        focused_in_popout(h, popout, "p1"),
        "the pop-out's terminal keeps the keyboard"
    );
}

#[gpui::test]
fn a_popped_pane_moved_by_two_tab_events_keeps_its_terminal_and_focus(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    fixture.sessions.push(session("s3").build());
    fixture.tabs = vec![
        tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p3", Some("s3")),
            ),
        ),
        tab("t2", &pane("p2", Some("s2"))),
    ];
    let mut h = Harness::with(cx, &dir, &fixture);
    pop_out(&mut h, "t1", "p1");
    let mut popout = h.popout_window();
    assert!(focused_in_popout(&h, &mut popout, "p1"));
    let before = h.root(|root, _| root.pane_terminal_id("p1"));
    assert!(before.is_some());
    h.sent();

    // The daemon's order for a cross-tab move: the destination, then the
    // source.
    h.send(DaemonMessage::TabUpdated {
        tab: tab(
            "t2",
            &split(
                SplitDirection::Horizontal,
                pane("p2", Some("s2")),
                pane("p1", Some("s1")),
            ),
        ),
    });
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &pane("p3", Some("s3"))),
    });

    assert_popout_kept_p1(&mut h, &mut popout, "t2", before);
}

#[gpui::test]
fn a_popped_pane_extracted_to_a_new_tab_keeps_its_terminal_and_focus(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    pop_out(&mut h, "t1", "p1");
    let mut popout = h.popout_window();
    assert!(focused_in_popout(&h, &mut popout, "p1"));
    let before = h.root(|root, _| root.pane_terminal_id("p1"));
    assert!(before.is_some());
    h.sent();

    // The daemon's order for an extract: the new tab, then the source.
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t3", &pane("p1", Some("s1"))),
    });
    h.send(DaemonMessage::TabRemoved {
        tab_id: "t1".to_owned(),
    });

    assert_popout_kept_p1(&mut h, &mut popout, "t3", before);
}

/// Whether `selector` is drawn in the pop-out window.
fn drawn_in_popout(popout: &mut VisualTestContext, selector: &'static str) -> bool {
    popout.run_until_parked();
    popout.debug_bounds(selector).is_some()
}

/// Presses `selector` in the pop-out window.
fn click_in_popout(popout: &mut VisualTestContext, selector: &'static str) {
    popout.run_until_parked();
    let bounds = popout
        .debug_bounds(selector)
        .expect("the control is drawn in the pop-out");
    popout.simulate_click(bounds.center(), Modifiers::none());
    popout.run_until_parked();
}

#[gpui::test]
fn popout_toolbar_shows_the_header_parts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture::single(session("s1").user_label("Build").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    let main_parts = h.root(|root, _| root.pane_header_parts("p1"));
    assert!(main_parts.is_some(), "the main header shows s1");

    pop_out(&mut h, "t1", "p1");
    let mut popout = h.popout_window();

    assert_eq!(
        h.root(|root, _| root.popout_header_parts("p1")),
        main_parts,
        "the toolbar shows what the main header showed"
    );
    assert_eq!(
        h.root(|root, _| root.popout_docked_in("p1")).as_deref(),
        Some("in t1")
    );
    for part in [
        "pane-glyph-p1-idle",
        "pane-title-p1",
        "popout-tab-p1",
        "pane-chip-p1-runtime",
        "popout-stop-p1",
        "popout-dock-p1",
    ] {
        assert!(drawn_in_popout(&mut popout, part), "{part} is drawn");
    }
    assert!(
        !drawn_in_popout(&mut popout, "pane-popout-p1"),
        "a pop-out has no Pop out of its own"
    );
}

#[gpui::test]
fn popout_stop_asks_twice(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    pop_out(&mut h, "t1", "p1");
    let mut popout = h.popout_window();
    h.sent();

    click_in_popout(&mut popout, "popout-stop-p1");

    assert!(h.sent().is_empty(), "the first press only asks");
    assert!(drawn_in_popout(&mut popout, "popout-stop-confirm-p1"));
    assert!(drawn_in_popout(&mut popout, "popout-stop-cancel-p1"));
    assert_eq!(
        h.root(|root, _| root.armed_stop_pane().map(str::to_owned)),
        None,
        "the main window's Stop is not armed"
    );

    click_in_popout(&mut popout, "popout-stop-confirm-p1");
    let sent = h.sent();
    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::StopSession { session_id, .. } if session_id == "s1"
        )),
        "the second press stops s1: {sent:?}"
    );
}

#[gpui::test]
fn popout_dock_back_closes_the_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    pop_out(&mut h, "t1", "p1");
    let mut popout = h.popout_window();
    h.sent();

    popout.run_until_parked();
    let dock = popout
        .debug_bounds("popout-dock-p1")
        .expect("Dock back is drawn in the pop-out");
    // Dock back acts on the press and closes the window it lands in, so no
    // release can follow it there.
    popout.simulate_event(MouseDownEvent {
        position: dock.center(),
        modifiers: Modifiers::none(),
        button: MouseButton::Left,
        click_count: 1,
        first_mouse: false,
    });

    assert_eq!(h.window_count(), 1, "the pop-out window closed");
    assert!(!popped(&mut h));
    assert_eq!(h.root(|root, _| root.pane_note("p1")), None);
    let sent = h.sent();
    assert!(
        !sent.iter().any(changes_layout_or_sessions),
        "docking back asks the daemon for nothing: {sent:?}"
    );
}

#[gpui::test]
fn popout_shows_the_exited_overlay(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"last words");
    pop_out(&mut h, "t1", "p1");
    let mut popout = h.popout_window();

    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").exited(3).build(),
        request_id: None,
    });

    for part in [
        "exited-p1",
        "exited-restart-p1",
        "exited-remove-pane-p1",
        "popout-exit-code-p1",
    ] {
        assert!(drawn_in_popout(&mut popout, part), "{part} is drawn");
    }
    assert_eq!(h.window_count(), 2, "an exited session keeps its pop-out");
}

#[gpui::test]
fn popout_closes_when_its_session_is_removed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = s1_working_beside_s2(cx, &dir);
    let mut popout = h.popout_window();

    h.send(DaemonMessage::SessionRemoved {
        session_id: "s1".to_owned(),
    });
    assert_eq!(
        h.window_count(),
        2,
        "the removal alone leaves the pane, so the pop-out stays"
    );
    assert!(
        !drawn_in_popout(&mut popout, "popout-pane-gone"),
        "the pop-out still draws the pane, as the main grid would"
    );

    // The tab update that drops the pane.
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &pane("p2", Some("s2"))),
    });
    assert_eq!(h.window_count(), 1, "the pop-out window closed");
    assert_eq!(h.root(|root, _| root.popout_count()), 0);
}

#[gpui::test]
fn resume_in_a_popout_keeps_the_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = s1_working_beside_s2(cx, &dir);

    // A resume's removal of the old session can come before the tab update
    // that rebinds its pane.
    h.send(DaemonMessage::SessionRemoved {
        session_id: "s1".to_owned(),
    });
    h.send(DaemonMessage::SessionUpdated {
        session: session("s1b").build(),
        request_id: None,
    });
    h.send(DaemonMessage::TabUpdated {
        tab: tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1b")),
                pane("p2", Some("s2")),
            ),
        ),
    });

    assert_eq!(h.window_count(), 2, "the pop-out window stays open");
    assert!(h.root(|root, _| root.is_popped_out("t1", "p1")));
    let mut popout = h.popout_window();
    popout.run_until_parked();
    assert_eq!(popout.window_title().as_deref(), Some("s1b"));
}

#[gpui::test]
fn new_session_from_a_popout_overlay_docks_and_opens_the_dialog_in_the_main_window(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_a_repo());
    pop_out(&mut h, "t1", "p1");
    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").in_repo("r1").exited(3).build(),
        request_id: None,
    });
    let mut popout = h.popout_window();
    popout.run_until_parked();
    let button = popout
        .debug_bounds("exited-new-session-p1")
        .expect("the overlay's New session is drawn in the pop-out");

    // The press closes the window it lands in, so no release follows it.
    popout.simulate_event(MouseDownEvent {
        position: button.center(),
        modifiers: Modifiers::none(),
        button: MouseButton::Left,
        click_count: 1,
        first_mouse: false,
    });

    assert_eq!(h.window_count(), 1, "the pane docked back");
    assert!(!popped(&mut h));
    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "the spawn dialog opens in the main window"
    );
    assert!(
        h.cx.update(|window, _| window.is_window_active()),
        "the main window is in front"
    );
}

#[gpui::test]
fn popout_closes_when_its_pane_is_closed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    fixture.tabs = vec![tab(
        "t1",
        &split(
            SplitDirection::Horizontal,
            pane("p1", Some("s1")),
            pane("p2", Some("s2")),
        ),
    )];
    let mut h = Harness::with(cx, &dir, &fixture);
    pop_out(&mut h, "t1", "p1");
    assert_eq!(h.window_count(), 2);

    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &pane("p2", Some("s2"))),
    });

    assert_eq!(h.window_count(), 1, "the pop-out window closed");
    assert_eq!(h.root(|root, _| root.popout_count()), 0);
}

#[gpui::test]
fn a_moved_popped_pane_keeps_its_popout_through_prune(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    fixture.sessions.push(session("s3").build());
    fixture.tabs = vec![
        tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p3", Some("s3")),
            ),
        ),
        tab("t2", &pane("p2", Some("s2"))),
    ];
    let mut h = Harness::with(cx, &dir, &fixture);
    pop_out(&mut h, "t1", "p1");

    h.send(DaemonMessage::TabUpdated {
        tab: tab(
            "t2",
            &split(
                SplitDirection::Horizontal,
                pane("p2", Some("s2")),
                pane("p1", Some("s1")),
            ),
        ),
    });
    assert_eq!(h.window_count(), 2, "the destination's fold keeps it");
    assert_eq!(h.root(|root, _| root.popout_count()), 1);

    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &pane("p3", Some("s3"))),
    });
    assert_eq!(h.window_count(), 2, "the source's fold keeps it");
    assert!(h.root(|root, _| root.is_popped_out("t2", "p1")));
}

/// Whether leaf `id` holds an unseen turn.
fn leaf_unseen(h: &mut Harness<'_>, id: &str) -> Option<bool> {
    h.root(|root, _| {
        root.sidebar_containers()
            .into_iter()
            .flat_map(|c| c.leaves)
            .find(|leaf| leaf.id == id)
            .map(|leaf| leaf.unseen)
    })
}

/// `s1` working in `p1` beside `s2` in `p2`, with `p1` popped out.
fn s1_working_beside_s2<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut fixture = Fixture::single(session("s1").status("working").build());
    fixture.sessions.push(session("s2").build());
    fixture.tabs = vec![tab(
        "t1",
        &split(
            SplitDirection::Horizontal,
            pane("p1", Some("s1")),
            pane("p2", Some("s2")),
        ),
    )];
    let mut h = Harness::with(cx, dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.answer_scrollback("s2", b"");
    pop_out(&mut h, "t1", "p1");
    h
}

/// Ends `s1`'s turn.
fn end_s1_turn(h: &mut Harness<'_>) {
    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").build(),
        request_id: None,
    });
}

/// Whether `window` is the active window, its frames settled.
fn window_active(window: &mut VisualTestContext) -> bool {
    window.run_until_parked();
    window.update(|window, _| window.is_window_active())
}

#[gpui::test]
fn a_deactivated_popout_stops_clearing_unseen_turns(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = s1_working_beside_s2(cx, &dir);
    let mut popout = h.popout_window();
    assert!(focused_in_popout(&h, &mut popout, "p1"));

    h.cx.update(|window, _| window.activate_window());
    assert!(!window_active(&mut popout), "the main window is in front");
    end_s1_turn(&mut h);

    assert_eq!(
        leaf_unseen(&mut h, "s1"),
        Some(true),
        "an inactive pop-out does not show s1, though its terminal keeps its focus"
    );
}

#[gpui::test]
fn raising_a_focused_popout_clears_its_turn(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = s1_working_beside_s2(cx, &dir);
    h.cx.update(|window, _| window.activate_window());
    end_s1_turn(&mut h);
    assert_eq!(leaf_unseen(&mut h, "s1"), Some(true));
    let mut popout = h.popout_window();
    assert!(
        focused_in_popout(&h, &mut popout, "p1"),
        "the terminal kept its window's keyboard"
    );

    popout.update(|window, _| window.activate_window());

    assert!(window_active(&mut popout));
    assert_eq!(
        leaf_unseen(&mut h, "s1"),
        Some(false),
        "raising the pop-out shows s1"
    );
}

#[gpui::test]
fn a_popped_single_pane_tab_lets_turns_end_unseen(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture::single(session("s1").status("working").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    pop_out(&mut h, "t1", "p1");
    assert_eq!(
        h.root(|root, _| root.focused_pane()).as_deref(),
        Some("p1"),
        "the main tab still names the popped pane focused"
    );
    h.cx.update(|window, _| window.activate_window());

    end_s1_turn(&mut h);

    assert_eq!(
        leaf_unseen(&mut h, "s1"),
        Some(true),
        "a popped pane shows nothing in the main window"
    );
}

/// [`s1_working_beside_s2`] with the main window in front, `p2` focused
/// there, and `s1`'s turn ended while nothing showed it.
fn s1_ends_unseen_beside_a_popout<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = s1_working_beside_s2(cx, dir);
    h.cx.update(|window, _| window.activate_window());
    let cell = h.cell_center("p2", 0, 0);
    h.click(cell, Modifiers::none());
    assert!(focused_in_main(&mut h, "p2"));
    assert_eq!(h.root(|root, _| root.focused_pane()).as_deref(), Some("p2"));

    end_s1_turn(&mut h);
    assert_eq!(leaf_unseen(&mut h, "s1"), Some(true), "s1 ended unseen");
    h
}

#[gpui::test]
fn focused_popout_clears_the_unseen_turn(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = s1_ends_unseen_beside_a_popout(cx, &dir);

    h.click_on("popped-focus-p1");
    let mut popout = h.popout_window();
    assert!(focused_in_popout(&h, &mut popout, "p1"));
    assert_eq!(
        leaf_unseen(&mut h, "s1"),
        Some(false),
        "the focused pop-out shows s1"
    );

    for status in ["working", "idle"] {
        h.send(DaemonMessage::SessionUpdated {
            session: session("s1").status(status).build(),
            request_id: None,
        });
    }
    assert_eq!(
        leaf_unseen(&mut h, "s1"),
        Some(false),
        "a turn ending in the focused pop-out is seen"
    );
}

#[gpui::test]
fn an_open_but_unfocused_popout_leaves_the_unseen_turn(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = s1_ends_unseen_beside_a_popout(cx, &dir);

    let cell = h.cell_center("p2", 0, 0);
    h.click(cell, Modifiers::none());

    assert_eq!(
        leaf_unseen(&mut h, "s1"),
        Some(true),
        "neither the open pop-out nor focus elsewhere in the main window saw s1"
    );
    assert_eq!(h.window_count(), 2);
}

#[gpui::test]
fn popout_terminal_takes_focus_on_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));

    pop_out(&mut h, "t1", "p1");

    let mut popout = h.popout_window();
    assert!(
        focused_in_popout(&h, &mut popout, "p1"),
        "the pop-out's terminal has its window's keyboard"
    );
    assert!(!focused_in_main(&mut h, "p1"));
}

#[gpui::test]
fn leaf_click_raises_a_popped_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    pop_out(&mut h, "t1", "p1");
    let mut popout = h.popout_window();
    popout.update(|window, _| window.blur());
    h.cx.update(|window, _| window.activate_window());
    assert!(!is_active(&mut popout), "the main window is in front");

    h.click_on("leaf-s1");

    assert!(is_active(&mut popout), "the pop-out window is raised");
    assert!(
        focused_in_popout(&h, &mut popout, "p1"),
        "its terminal has the keyboard"
    );
    assert!(!focused_in_main(&mut h, "p1"));
    assert_eq!(h.window_count(), 2);
}

#[gpui::test]
fn a_pane_in_two_tabs_for_one_fold_draws_no_gone_frame(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    fixture.tabs.push(tab("t2", &pane("p2", Some("s2"))));
    let mut h = Harness::with(cx, &dir, &fixture);
    h.click_on("tab-t2");
    pop_out(&mut h, "t1", "p1");

    // The destination's event alone: p1 is in t1 and in t2 for this fold.
    h.send(DaemonMessage::TabUpdated {
        tab: tab(
            "t2",
            &split(
                SplitDirection::Horizontal,
                pane("p2", Some("s2")),
                pane("p1", Some("s1")),
            ),
        ),
    });

    let mut popout = h.popout_window();
    popout.run_until_parked();
    assert!(
        popout.debug_bounds("popout-pane-gone").is_none(),
        "the pop-out still draws p1's terminal"
    );
    assert_eq!(
        h.root(|root, _| root.pane_note("p1")),
        Some(POPPED_OUT_NOTE),
        "the main grid draws the card, not p1's terminal"
    );
}

/// The terminal a session pop-out of `s2` draws, outside every grid.
const S2_SLOT: &str = "popout:s2";

/// `s1` in `t1`'s `p1`, beside `s2`, which no pane shows.
fn s2_without_a_pane() -> Fixture {
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    fixture
}

/// Pops session `id` out into a window of its own.
fn pop_out_session(h: &mut Harness<'_>, id: &str) -> OpenOutcome {
    let key = PopoutKey::Session {
        session_id: id.to_owned(),
        host: None,
    };
    h.root_update(move |root, window, cx| root.open_popout(key, window, cx))
}

/// Clicks the pop-out window at `at`.
fn click_in_popout_at(popout: &mut VisualTestContext, at: Point<Pixels>) {
    popout.simulate_click(at, Modifiers::none());
    popout.run_until_parked();
}

/// Presses `selector` in the pop-out window with no release, as a press
/// that closes its own window must be sent.
fn press_in_popout(popout: &mut VisualTestContext, selector: &'static str) {
    popout.run_until_parked();
    let bounds = popout
        .debug_bounds(selector)
        .expect("the control is drawn in the pop-out");
    popout.simulate_event(MouseDownEvent {
        position: bounds.center(),
        modifiers: Modifiers::none(),
        button: MouseButton::Left,
        click_count: 1,
        first_mouse: false,
    });
}

#[gpui::test]
fn session_popout_loads_scrollback_and_takes_keys(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &s2_without_a_pane());
    h.sent();

    h.right_click_on("leaf-s2");
    assert_eq!(
        h.root(|root, _| root.menu_row_label("session-menu-popout")),
        Some("Pop out")
    );
    assert_eq!(
        h.root(|root, _| root.menu_disabled_tip("session-menu-popout")),
        None,
        "Pop out is enabled for a session no pane shows"
    );
    h.click_on("session-menu-popout");

    assert_eq!(h.window_count(), 2, "the session has a window of its own");
    assert_eq!(h.root(|root, _| root.popout_count()), 1);
    assert_eq!(
        h.scrollback_requests("s2").len(),
        1,
        "the pop-out's terminal asks for s2's scrollback"
    );
    h.answer_scrollback("s2", b"earlier output");
    let mut popout = h.popout_window();
    popout.run_until_parked();
    assert!(
        h.grid_text(S2_SLOT)
            .iter()
            .any(|row| row.contains("earlier output")),
        "the scrollback fills the pop-out's terminal"
    );
    assert_eq!(popout.window_title().as_deref(), Some("s2"));
    let sent = h.sent();
    assert!(
        !sent.iter().any(changes_layout_or_sessions),
        "popping a session out places it in no tab: {sent:?}"
    );
    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::Resize { session_id, .. } if session_id == "s2"
        )),
        "the pop-out's terminal drives s2's size: {sent:?}"
    );
    assert!(
        focused_in_popout(&h, &mut popout, S2_SLOT),
        "its terminal has the window's keyboard"
    );

    popout.simulate_keystrokes("a");
    assert_eq!(h.sent_input("s2"), b"a", "the key reached the session");
}

#[gpui::test]
fn session_popout_stays_after_stop(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &s2_without_a_pane());
    assert_eq!(pop_out_session(&mut h, "s2"), OpenOutcome::Opened);
    h.answer_scrollback("s2", b"");
    let mut popout = h.popout_window();
    h.sent();

    click_in_popout(&mut popout, "popout-stop-popout:s2");
    assert!(h.sent().is_empty(), "the first press only asks");
    click_in_popout(&mut popout, "popout-stop-confirm-popout:s2");
    let sent = h.sent();
    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::StopSession { session_id, .. } if session_id == "s2"
        )),
        "the second press stops s2: {sent:?}"
    );
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::DiscardSession { .. })),
        "a session its pop-out shows is stopped, not removed: {sent:?}"
    );

    h.send(DaemonMessage::SessionUpdated {
        session: session("s2").exited(3).build(),
        request_id: None,
    });

    assert_eq!(h.window_count(), 2, "a stopped session keeps its pop-out");
    for part in [
        "exited-popout:s2",
        "exited-restart-popout:s2",
        "popout-exit-code-popout:s2",
        "popout-close-popout:s2",
    ] {
        assert!(drawn_in_popout(&mut popout, part), "{part} is drawn");
    }
    assert!(
        !drawn_in_popout(&mut popout, "exited-new-session-popout:s2"),
        "no pane waits for a new session"
    );
    assert_eq!(popout.window_title().as_deref(), Some("s2"));
}

#[gpui::test]
fn session_popout_shows_not_found_when_removed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &s2_without_a_pane());
    pop_out_session(&mut h, "s2");
    h.answer_scrollback("s2", b"");
    let mut popout = h.popout_window();
    assert!(drawn_in_popout(&mut popout, "popout-stop-popout:s2"));
    let stop = popout
        .debug_bounds("popout-stop-popout:s2")
        .expect("Stop is drawn while s2 is listed");

    h.send(DaemonMessage::SessionRemoved {
        session_id: "s2".to_owned(),
    });

    assert_eq!(h.window_count(), 2, "the window stays open");
    assert!(drawn_in_popout(&mut popout, "popout-not-found-popout:s2"));
    assert!(drawn_in_popout(&mut popout, "popout-close-popout:s2"));
    assert_eq!(popout.window_title().as_deref(), Some("Session not found"));
    h.sent();
    // A frame's debug bounds outlive it, so Stop's absence shows as two
    // presses where it stood that ask nothing.
    click_in_popout_at(&mut popout, stop.center());
    click_in_popout_at(&mut popout, stop.center());
    let sent = h.sent();
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::StopSession { .. })),
        "nothing is left to stop: {sent:?}"
    );

    press_in_popout(&mut popout, "popout-close-popout:s2");

    assert_eq!(h.window_count(), 1, "Close window closed it");
    assert_eq!(h.root(|root, _| root.popout_count()), 0);
    let sent = h.sent();
    assert!(
        !sent.iter().any(changes_layout_or_sessions) && !detached(&sent, "s2"),
        "closing asks the daemon nothing of a removed session: {sent:?}"
    );
}

/// `s2` popped out on its own, then stopped with exit code 3.
fn stopped_s2_popped_out<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &s2_without_a_pane());
    pop_out_session(&mut h, "s2");
    h.answer_scrollback("s2", b"");
    h.send(DaemonMessage::SessionUpdated {
        session: session("s2").exited(3).build(),
        request_id: None,
    });
    h
}

#[gpui::test]
fn session_menu_labels_a_popped_out_session_without_a_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = stopped_s2_popped_out(cx, &dir);

    h.right_click_on("leaf-s2");

    let labels = h.root(|root, _| root.menu_labels());
    assert!(
        labels.iter().any(|label| label == "Remove session"),
        "no tab pane shows s2: {labels:?}"
    );
    assert!(
        !labels.iter().any(|label| label.starts_with("Remove pane")),
        "a session pop-out is no pane: {labels:?}"
    );
}

#[gpui::test]
fn restart_from_a_session_popout_reopens_in_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = stopped_s2_popped_out(cx, &dir);
    let mut popout = h.popout_window();
    h.sent();

    click_in_popout(&mut popout, "exited-restart-popout:s2");
    let restart = h
        .sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::DuplicateSession {
                session_id,
                request_id,
                ..
            } if session_id == "s2" => request_id,
            _ => None,
        })
        .expect("a restart of s2 with a request id");
    h.send(DaemonMessage::SessionUpdated {
        session: session("s2b").build(),
        request_id: Some(restart),
    });

    let sent = h.sent();
    assert!(
        !sent.iter().any(|msg| changes_layout_or_sessions(msg)
            || matches!(msg, ClientMessage::ReplacePaneSession { .. })),
        "the copy is placed in no tab: {sent:?}"
    );
    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::DiscardSession { session_id, .. } if session_id == "s2"
        )),
        "the restarted session is discarded: {sent:?}"
    );
    assert_eq!(h.window_count(), 2, "one pop-out window, the same one");
    assert_eq!(h.root(|root, _| root.popout_count()), 1);
    popout.run_until_parked();
    assert_eq!(popout.window_title().as_deref(), Some("s2b"));
    assert_eq!(
        h.scrollback_requests("s2b").len(),
        1,
        "the pop-out's terminal attaches the copy"
    );
    h.answer_scrollback("s2b", b"");
    popout.simulate_keystrokes("a");
    assert_eq!(h.sent_input("s2b"), b"a", "keys reach the copy");

    h.send(DaemonMessage::SessionRemoved {
        session_id: "s2".to_owned(),
    });
    assert!(
        !drawn_in_popout(&mut popout, "popout-not-found-popout:s2b"),
        "the old session's removal leaves the copy shown"
    );
}

#[gpui::test]
fn restart_from_the_sidebar_reopens_in_the_sessions_popout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = stopped_s2_popped_out(cx, &dir);
    let mut popout = h.popout_window();
    h.sent();

    h.right_click_on("leaf-s2");
    h.click_on("menu-restart");
    let restart = h
        .sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::DuplicateSession {
                session_id,
                request_id,
                ..
            } if session_id == "s2" => request_id,
            _ => None,
        })
        .expect("a restart of s2 with a request id");
    h.send(DaemonMessage::SessionUpdated {
        session: session("s2b").build(),
        request_id: Some(restart),
    });

    let sent = h.sent();
    assert!(
        !sent.iter().any(|msg| changes_layout_or_sessions(msg)
            || matches!(msg, ClientMessage::ReplacePaneSession { .. })),
        "the copy is placed in no tab: {sent:?}"
    );
    assert_eq!(h.window_count(), 2, "one pop-out window, the same one");
    assert_eq!(h.root(|root, _| root.popout_count()), 1);
    popout.run_until_parked();
    assert_eq!(popout.window_title().as_deref(), Some("s2b"));
    h.answer_scrollback("s2b", b"");
    h.send(DaemonMessage::SessionRemoved {
        session_id: "s2".to_owned(),
    });
    assert!(
        !drawn_in_popout(&mut popout, "popout-not-found-popout:s2b"),
        "the pop-out shows the copy, not a missing session"
    );
}

#[gpui::test]
fn leaf_click_on_a_popped_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &s2_without_a_pane());
    pop_out_session(&mut h, "s2");
    let mut popout = h.popout_window();
    popout.update(|window, _| window.blur());
    h.cx.update(|window, _| window.activate_window());
    assert!(!is_active(&mut popout), "the main window is in front");
    h.sent();

    h.click_on("leaf-s2");

    assert!(is_active(&mut popout), "the pop-out window is raised");
    assert!(
        focused_in_popout(&h, &mut popout, S2_SLOT),
        "its terminal has the keyboard"
    );
    assert_eq!(h.window_count(), 2);
    let sent = h.sent();
    assert!(
        !sent.iter().any(|msg| changes_layout_or_sessions(msg)
            || matches!(msg, ClientMessage::ReplacePaneSession { .. })),
        "the click places s2 in no pane: {sent:?}"
    );
}

/// Runs `change` on the root view in the pop-out window, as a control drawn
/// there would, then lets the window settle.
fn in_popout<R>(
    h: &Harness<'_>,
    popout: &mut VisualTestContext,
    change: impl FnOnce(&mut RootView, &mut Window, &mut gpui::Context<RootView>) -> R,
) -> R {
    let root = h.root.clone();
    let out = popout.update(|window, cx| root.update(cx, |root, cx| change(root, window, cx)));
    popout.run_until_parked();
    out
}

/// Whether `selector` was ever drawn in the main window. gpui never clears
/// a selector's bounds, so this only says "never" for a selector new to it.
fn ever_drawn_in_main(h: &mut Harness<'_>, selector: &str) -> bool {
    h.cx.run_until_parked();
    h.cx.debug_bounds(named(selector)).is_some()
}

/// `selector`, built at run time, as the `'static` name gpui's debug
/// bounds take; a spec leaks a handful of these.
fn named(selector: &str) -> &'static str {
    Box::leak(selector.to_owned().into_boxed_str())
}

/// Presses `selector` in the pop-out window, a selector built at run time.
fn click_in_popout_named(popout: &mut VisualTestContext, selector: &str) {
    popout.run_until_parked();
    let bounds = popout
        .debug_bounds(named(selector))
        .expect("the control is drawn in the pop-out");
    popout.simulate_click(bounds.center(), Modifiers::none());
    popout.run_until_parked();
}

/// A shell command's prompt, typed command, output and exit marks.
fn marked_command(line: &str) -> String {
    format!("\x1b]133;A\x07$ {line}\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07")
}

#[gpui::test]
fn pane_close_dialog_opens_in_the_window_that_asked(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"");
    pop_out(&mut h, "t1", "p1");
    let mut popout = h.popout_window();

    in_popout(&h, &mut popout, |root, window, cx| {
        root.close_pane_clicked("t1", "p1", Some("s1"), window, cx);
    });

    assert!(h.root(|root, _| root.pane_close_open()));
    assert!(
        drawn_in_popout(&mut popout, "pane-close"),
        "the dialog draws in the pop-out that asked"
    );
    assert!(
        !ever_drawn_in_main(&mut h, "pane-close"),
        "the main window draws no copy of it"
    );
    popout.simulate_keystrokes("escape");
    assert!(
        !h.root(|root, _| root.pane_close_open()),
        "Esc in the pop-out cancels it"
    );
    assert!(
        focused_in_popout(&h, &mut popout, "p1"),
        "the pop-out's terminal has the keyboard back"
    );

    in_popout(&h, &mut popout, |root, window, cx| {
        root.close_pane_clicked("t1", "p1", Some("s1"), window, cx);
    });
    assert!(h.root(|root, _| root.pane_close_open()));
    h.root_update(|root, _, cx| root.dock_back(&pane_key("t1", "p1"), cx));
    assert!(
        !h.root(|root, _| root.pane_close_open()),
        "docking the pop-out back closes the dialog it opened"
    );
}

#[gpui::test]
fn session_menu_opens_in_the_popout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"");
    pop_out(&mut h, "t1", "p1");
    let mut popout = h.popout_window();
    h.sent();

    let pane = ("t1".to_owned(), "p1".to_owned());
    in_popout(&h, &mut popout, |root, window, cx| {
        root.open_session_menu(
            &rustling_tulip_native::target::HostKey::Local,
            "s1",
            Some(&pane),
            point(px(40.0), px(12.0)),
            window,
            cx,
        );
    });

    assert!(
        !h.root(|root, _| root.menu_labels()).is_empty(),
        "the menu is open"
    );
    assert!(
        drawn_in_popout(&mut popout, "session-menu"),
        "the menu draws in the pop-out"
    );
    assert!(
        !ever_drawn_in_main(&mut h, "session-menu"),
        "the main window draws no copy of it"
    );
    popout.simulate_keystrokes("escape");
    assert!(
        h.root(|root, _| root.menu_labels()).is_empty(),
        "Esc in the pop-out closes it"
    );
    assert!(
        focused_in_popout(&h, &mut popout, "p1"),
        "the pop-out's terminal has the keyboard back"
    );
    popout.simulate_keystrokes("a");
    assert_eq!(h.sent_input("s1"), b"a", "keys reach the session again");
}

#[gpui::test]
fn gutter_dot_menu_opens_in_the_popout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture::single(session("s1").shell("C:/work").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    pop_out(&mut h, "t1", "p1");
    let mut popout = h.popout_window();
    h.pty("s1", marked_command("make").as_bytes());

    click_in_popout_named(&mut popout, "shell-dot-p1-0");

    assert!(h.root(|root, _| root.shell_menu_open()));
    assert!(
        drawn_in_popout(&mut popout, "shell-menu"),
        "the gutter menu draws in the pop-out"
    );
    assert!(
        !ever_drawn_in_main(&mut h, "shell-menu"),
        "the main window draws no copy of it"
    );
    popout.simulate_keystrokes("escape");
    assert!(
        !h.root(|root, _| root.shell_menu_open()),
        "Esc in the pop-out closes it"
    );
    assert!(focused_in_popout(&h, &mut popout, "p1"));
}

/// A toast that stays until closed, titled `title`.
fn sticky_toast(title: &str) -> ToastSpec {
    ToastSpec {
        kind: ToastKind::Info,
        title: title.to_owned(),
        detail: None,
        key: None,
        sticky: true,
        actions: Vec::new(),
    }
}

#[gpui::test]
fn toast_shows_in_the_focused_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"");
    pop_out(&mut h, "t1", "p1");
    let mut popout = h.popout_window();

    click_in_popout_named(&mut popout, "popout-toolbar-p1");
    let first = h.root_update(|root, _, cx| root.push_toast_spec(sticky_toast("first"), cx));
    let first_close = format!("toast-close-{first}");
    popout.run_until_parked();
    assert!(
        popout.debug_bounds(named(&first_close)).is_some(),
        "a toast raised after a press in the pop-out shows there"
    );
    assert!(!ever_drawn_in_main(&mut h, &first_close));

    h.cx.simulate_keystrokes("escape");
    let second = h.root_update(|root, _, cx| root.push_toast_spec(sticky_toast("second"), cx));
    assert!(
        ever_drawn_in_main(&mut h, &format!("toast-close-{second}")),
        "a toast raised after a key in the main window shows there"
    );
    assert!(
        !ever_drawn_in_main(&mut h, &first_close),
        "the shown toast stays in the pop-out"
    );

    assert!(h.close_popout(), "the pop-out closes");
    assert!(
        ever_drawn_in_main(&mut h, &first_close),
        "a closed window's toast moves to the main window"
    );
}

#[gpui::test]
fn gutter_dot_menu_works_after_a_popout_restart(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture
        .sessions
        .push(session("s2").shell("C:/work").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    pop_out_session(&mut h, "s2");
    h.answer_scrollback("s2", b"");
    h.send(DaemonMessage::SessionUpdated {
        session: session("s2").shell("C:/work").exited(3).build(),
        request_id: None,
    });
    let mut popout = h.popout_window();
    h.sent();

    click_in_popout(&mut popout, "exited-restart-popout:s2");
    let restart = h
        .sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::DuplicateSession {
                session_id,
                request_id,
                ..
            } if session_id == "s2" => request_id,
            _ => None,
        })
        .expect("a restart of s2 with a request id");
    h.send(DaemonMessage::SessionUpdated {
        session: session("s2b").shell("C:/work").build(),
        request_id: Some(restart),
    });
    h.answer_scrollback("s2b", b"");
    h.pty("s2b", marked_command("make").as_bytes());

    click_in_popout_named(&mut popout, "shell-dot-popout:s2b-0");

    assert!(
        h.root(|root, _| root.shell_menu_open()),
        "the restarted terminal's gutter dot opens its menu"
    );
    assert!(drawn_in_popout(&mut popout, "shell-menu"));
}

#[gpui::test]
fn delete_worktree_confirm_from_a_popout_menu_takes_the_main_windows_keys(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture::single(session("s1").worktree("r1").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    pop_out(&mut h, "t1", "p1");
    let mut popout = h.popout_window();
    popout.update(|window, _| window.activate_window());
    in_popout(&h, &mut popout, |root, window, cx| {
        root.close_pane_clicked("t1", "p1", Some("s1"), window, cx);
    });

    click_in_popout(&mut popout, "pane-close-delete");

    assert_eq!(
        h.root(|root, _| root.delete_dialog_session().map(str::to_owned))
            .as_deref(),
        Some("s1"),
        "the delete-worktree confirm opens"
    );
    assert!(
        window_active(h.cx),
        "the main window, which draws the confirm, comes to the front"
    );
    h.cx.simulate_keystrokes("escape");
    assert_eq!(
        h.root(|root, _| root.delete_dialog_session().map(str::to_owned)),
        None,
        "Esc in the main window cancels it"
    );
}

#[gpui::test]
fn opening_a_menu_in_main_hands_the_popouts_keys_back(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"");
    pop_out(&mut h, "t1", "p1");
    let mut popout = h.popout_window();
    let pane = ("t1".to_owned(), "p1".to_owned());
    in_popout(&h, &mut popout, |root, window, cx| {
        root.open_session_menu(
            &rustling_tulip_native::target::HostKey::Local,
            "s1",
            Some(&pane),
            point(px(40.0), px(12.0)),
            window,
            cx,
        );
    });
    assert!(!focused_in_popout(&h, &mut popout, "p1"), "the menu has it");

    h.right_click_on("leaf-s1");

    assert!(
        !h.root(|root, _| root.menu_labels()).is_empty(),
        "the menu opened in the main window"
    );
    popout.run_until_parked();
    assert!(
        focused_in_popout(&h, &mut popout, "p1"),
        "the pop-out's terminal has its window's keyboard back"
    );
}

#[gpui::test]
fn an_overlay_in_a_popout_does_not_block_the_main_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture
        .sessions
        .push(session("s2").status("working").build());
    fixture.tabs = vec![tab(
        "t1",
        &split(
            SplitDirection::Horizontal,
            pane("p1", Some("s1")),
            pane("p2", Some("s2")),
        ),
    )];
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.answer_scrollback("s2", b"");
    pop_out(&mut h, "t1", "p1");
    let mut popout = h.popout_window();
    in_popout(&h, &mut popout, |root, window, cx| {
        root.close_pane_clicked("t1", "p1", Some("s1"), window, cx);
    });
    assert!(h.root(|root, _| root.pane_close_open()));
    h.cx.update(|window, _| window.activate_window());
    assert!(window_active(h.cx), "the main window has the keyboard");
    h.notified();

    h.send(DaemonMessage::Attention {
        session_id: "s2".to_owned(),
        reason: AttentionReason::AwaitingInput,
    });

    assert_eq!(
        h.notified(),
        Vec::new(),
        "s2's pane is on screen in the main window, which no dialog covers"
    );
}

/// `t1` holding `p1` on `s1` beside `p2` on `s2`, then `t2` with `s3` in
/// `p3`.
fn split_tabs() -> Fixture {
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    fixture.sessions.push(session("s3").build());
    fixture.tabs = vec![
        tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", Some("s2")),
            ),
        ),
        tab("t2", &pane("p3", Some("s3"))),
    ];
    fixture
}

fn tab_key(tab_id: &str) -> PopoutKey {
    PopoutKey::Tab {
        tab_id: tab_id.to_owned(),
    }
}

/// Pops tab `tab_id` out, as its menu's Pop out does.
fn pop_out_tab(h: &mut Harness<'_>, tab_id: &str) -> OpenOutcome {
    let key = tab_key(tab_id);
    h.root_update(move |root, window, cx| root.open_popout(key, window, cx))
}

fn tab_popped(h: &mut Harness<'_>, tab_id: &str) -> bool {
    let tab_id = tab_id.to_owned();
    h.root(move |root, _| root.is_tab_popped(&tab_id))
}

/// Whether the element tagged `selector` is on screen in the main window.
fn on_screen(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

/// The id of the window `window` tests.
fn window_id(window: &mut VisualTestContext) -> WindowId {
    window.update(|window, _| window.window_handle().window_id())
}

#[gpui::test]
fn tab_menu_pops_out_a_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &split_tabs());
    h.sent();

    h.right_click_on("tab-t1");
    let rows = h.root(|root, _| root.tab_menu_rows());
    assert_eq!(
        &rows[..2],
        ["tab-menu-rename", "tab-menu-popout"],
        "Pop out follows Rename"
    );
    assert!(
        h.root(|root, _| root.tab_menu_text())
            .contains(&"Pop out".to_owned())
    );
    h.click_on("tab-menu-popout");

    assert_eq!(h.window_count(), 2, "the tab has a window of its own");
    assert!(tab_popped(&mut h, "t1"));
    assert!(
        h.root(|root, _| root.tab_menu().is_none()),
        "the menu closed"
    );
    let sent = h.sent();
    assert!(
        !sent.iter().any(changes_layout_or_sessions),
        "popping a tab out asks the daemon for nothing: {sent:?}"
    );
    let mut popout = h.popout_window();
    assert_eq!(
        popout.window_title().as_deref(),
        Some("t1"),
        "titled by its tab"
    );
    assert_eq!(
        popout.update(|window, _| window.bounds().size),
        size(px(1100.0), px(720.0))
    );

    h.right_click_on("tab-t1");
    assert!(
        h.root(|root, _| root.tab_menu_text())
            .contains(&"Focus window".to_owned()),
        "the row reads Focus window once the tab is out"
    );
    h.click_on("tab-menu-popout");
    assert_eq!(h.window_count(), 2, "no second window");
    assert_eq!(h.root(|root, _| root.popout_count()), 1);
}

#[gpui::test]
fn tab_popout_shows_its_grid_and_splits(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &split_tabs());
    assert_eq!(pop_out_tab(&mut h, "t1"), OpenOutcome::Opened);
    let mut popout = h.popout_window();
    h.sent();

    for part in [
        "popout-tab-title-t1",
        "popout-tab-dock-t1",
        "tab-grid-t1",
        "pane-header-p1",
        "pane-header-p2",
        "divider-t1-[]",
    ] {
        assert!(drawn_in_popout(&mut popout, part), "{part} is drawn");
    }
    assert!(
        !drawn_in_popout(&mut popout, "tab-t1"),
        "the pop-out has no tab strip"
    );

    click_in_popout(&mut popout, "split-right-p2");
    let sent = h.sent();
    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::SplitPane { tab_id, pane_id, .. } if tab_id == "t1" && pane_id == "p2"
        )),
        "a header's split acts on the popped tab: {sent:?}"
    );

    in_popout(&h, &mut popout, |root, window, cx| {
        root.close_pane_clicked("t1", "p2", Some("s2"), window, cx);
    });
    assert!(
        drawn_in_popout(&mut popout, "pane-close"),
        "a pane's close asks in the pop-out"
    );
}

#[gpui::test]
fn a_divider_drag_in_a_tab_popout_does_not_highlight_the_main_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &split_tabs());
    pop_out_tab(&mut h, "t1");
    let mut popout = h.popout_window();
    let (main, there) = (window_id(h.cx), window_id(&mut popout));
    popout.run_until_parked();
    let divider = popout
        .debug_bounds("divider-t1-[]")
        .expect("the divider is drawn in the pop-out")
        .center();
    h.sent();

    popout.simulate_mouse_down(divider, MouseButton::Left, Modifiers::none());
    popout.simulate_mouse_move(
        point(divider.x + px(60.0), divider.y),
        MouseButton::Left,
        Modifiers::none(),
    );
    popout.run_until_parked();

    let lit = |h: &mut Harness<'_>, window: WindowId| {
        h.root(move |root, _| root.divider_highlighted("t1", &[], window))
    };
    assert!(
        lit(&mut h, there),
        "the drag lights its divider in the pop-out"
    );
    assert!(
        !lit(&mut h, main),
        "the main window's copy of the divider stays unlit"
    );

    popout.simulate_mouse_up(
        point(divider.x + px(60.0), divider.y),
        MouseButton::Left,
        Modifiers::none(),
    );
    popout.run_until_parked();
    let sent = h.sent();
    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::SetPaneRatio { tab_id, ratio, .. } if tab_id == "t1" && *ratio > 0.5
        )),
        "the drag sets the ratio from the pop-out's own grid: {sent:?}"
    );
    assert!(!lit(&mut h, there), "the release ends the drag");
}

#[gpui::test]
fn spawn_is_disabled_in_a_tab_popout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").in_repo("r1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    fixture.tabs = vec![tab(
        "t1",
        &split(
            SplitDirection::Horizontal,
            pane("p1", Some("s1")),
            pane("p2", None),
        ),
    )];
    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(h.root(|root, _| root.pane_spawn_refusal("t1")), None);
    pop_out_tab(&mut h, "t1");
    let mut popout = h.popout_window();
    h.sent();

    assert_eq!(
        h.root(|root, _| root.pane_spawn_refusal("t1")),
        Some("Spawn from the main window")
    );
    click_in_popout(&mut popout, "empty-pane-new-session-p2");
    assert!(
        !h.root(|root, _| root.spawn_dialog_open()),
        "New session… opens no dialog"
    );
    click_in_popout(&mut popout, "empty-pane-shell-p2");
    let sent = h.sent();
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::SpawnSession(_))),
        "Shell here spawns nothing: {sent:?}"
    );

    popout.run_until_parked();
    let header = popout
        .debug_bounds("pane-header-p2")
        .expect("the empty pane's header is drawn")
        .center();
    popout.simulate_mouse_down(header, MouseButton::Right, Modifiers::none());
    popout.simulate_mouse_up(header, MouseButton::Right, Modifiers::none());
    popout.run_until_parked();
    let rows = h.root(|root, _| root.empty_pane_menu_rows());
    assert!(
        rows.contains(&"empty-pane-close".to_owned())
            && rows.contains(&"empty-pane-menu-move".to_owned()),
        "the rows that spawn nothing stay: {rows:?}"
    );
    click_in_popout(&mut popout, "empty-pane-close");
    let sent = h.sent();
    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::ClosePane { tab_id, pane_id, .. } if tab_id == "t1" && pane_id == "p2"
        )),
        "Close pane still works there: {sent:?}"
    );
}

/// Opens a shell in `C:/work` through the main window's More menu's
/// Shell… dialog, and replies to its spawn with standalone shell `new`;
/// returns what the client sent from the submit on.
fn shell_from_the_main_window(h: &mut Harness<'_>) -> Vec<ClientMessage> {
    h.open_shell_dialog_from_more();
    assert!(
        h.root(|root, _| root.shell_dialog_open()),
        "the dialog opened"
    );
    h.keys("C : / w o r k");
    h.click_on("shell-submit");
    let mut sent = h.sent();
    let request_id = sent
        .iter()
        .find_map(|msg| match msg {
            ClientMessage::SpawnSession(request) => Some(request.request_id.clone()),
            _ => None,
        })
        .expect("the dialog sent its spawn");
    h.send(DaemonMessage::SessionUpdated {
        session: session("new").shell("C:/work").build(),
        request_id,
    });
    sent.extend(h.sent());
    sent
}

/// The tabs `msg` asks the daemon to change.
fn tabs_named(msg: &ClientMessage) -> Vec<&str> {
    match msg {
        ClientMessage::SplitPane { tab_id, .. }
        | ClientMessage::ReplacePaneSession { tab_id, .. }
        | ClientMessage::ClosePane { tab_id, .. }
        | ClientMessage::CloseTab { tab_id, .. } => vec![tab_id.as_str()],
        ClientMessage::MovePane {
            src_tab_id,
            dst_tab_id,
            ..
        } => vec![src_tab_id.as_str(), dst_tab_id.as_str()],
        _ => Vec::new(),
    }
}

#[gpui::test]
fn a_shell_from_the_main_window_lands_in_its_active_popped_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &split_tabs());
    pop_out_tab(&mut h, "t1");
    h.sent();

    let sent = shell_from_the_main_window(&mut h);

    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::SplitPane { tab_id, new_session_id: Some(id), .. }
                if tab_id == "t1" && id == "new"
        )),
        "the shell joins the popped tab the main window has active: {sent:?}"
    );
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::CreateTab { .. })),
        "no tab of its own: {sent:?}"
    );
    assert!(tab_popped(&mut h, "t1"), "the tab stays popped out");
}

#[gpui::test]
fn a_shell_from_the_main_window_leaves_its_other_tabs_alone(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &split_tabs());
    pop_out_tab(&mut h, "t1");
    h.sent();

    let sent = shell_from_the_main_window(&mut h);

    assert!(
        !sent
            .iter()
            .flat_map(tabs_named)
            .any(|tab_id| tab_id == "t2"),
        "t2 is not the active tab, so nothing touches it: {sent:?}"
    );
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned))
            .as_deref(),
        Some("t1"),
        "the popped tab stays the active one"
    );
}

/// The point `fx` across and `fy` down `selector`'s bounds in the pop-out.
fn popout_point(
    popout: &mut VisualTestContext,
    selector: &'static str,
    fx: f32,
    fy: f32,
) -> Point<Pixels> {
    popout.run_until_parked();
    let bounds = popout
        .debug_bounds(selector)
        .expect("the element is drawn in the pop-out");
    point(
        bounds.origin.x + bounds.size.width * fx,
        bounds.origin.y + bounds.size.height * fy,
    )
}

#[gpui::test]
fn a_pane_dragged_inside_a_tab_popout_moves_within_that_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &split_tabs());
    pop_out_tab(&mut h, "t1");
    h.click_on("tab-t2");
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned))
            .as_deref(),
        Some("t2"),
        "the main window shows another tab"
    );
    let mut popout = h.popout_window();
    let from = popout_point(&mut popout, "pane-title-p1", 0.5, 0.5);
    let to = popout_point(&mut popout, "pane-frame-p2", 0.8, 0.5);
    h.sent();

    popout.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    popout.simulate_mouse_move(
        point(from.x + px(6.0), from.y),
        MouseButton::Left,
        Modifiers::none(),
    );
    popout.run_until_parked();
    popout.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
    popout.run_until_parked();
    popout.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    popout.run_until_parked();

    let sent = h.sent();
    let moves: Vec<_> = sent
        .iter()
        .filter(|msg| matches!(msg, ClientMessage::MovePane { .. }))
        .collect();
    assert!(
        matches!(
            moves.as_slice(),
            [ClientMessage::MovePane {
                src_tab_id,
                src_pane_id,
                dst_tab_id,
                dst_pane_id,
                edge: PaneDropEdge::Right,
                ..
            }] if src_tab_id == "t1"
                && src_pane_id == "p1"
                && dst_tab_id == "t1"
                && dst_pane_id == "p2"
        ),
        "p1 moves beside p2 in the popped tab, not into the main window's t2: {sent:?}"
    );
}

#[gpui::test]
fn pane_pop_out_is_disabled_in_a_tab_popout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &split_tabs());
    pop_out_tab(&mut h, "t1");
    let mut popout = h.popout_window();

    assert!(
        drawn_in_popout(&mut popout, "pane-popout-p1"),
        "the header keeps its Pop out, dimmed"
    );
    for part in ["split-right-p1", "split-down-p1", "close-pane-p1"] {
        assert!(drawn_in_popout(&mut popout, part), "{part} is drawn");
    }
    click_in_popout(&mut popout, "pane-popout-p1");
    assert_eq!(h.root(|root, _| root.popout_count()), 1, "nothing opened");
    assert!(!popped(&mut h), "p1 stays in its tab's window");
    assert_eq!(h.window_count(), 2);

    h.right_click_on("leaf-s1");
    assert_eq!(
        h.root(|root, _| root.menu_disabled_tip("session-menu-popout")),
        Some("Already in a pop-out"),
        "a pane of a popped tab overlaps it"
    );
}

#[gpui::test]
fn main_strip_marks_a_popped_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &split_tabs());
    assert!(!on_screen(&mut h, "tab-pill-popped-t1"), "no mark yet");

    pop_out_tab(&mut h, "t1");

    assert!(
        on_screen(&mut h, "tab-pill-popped-t1"),
        "the popped tab's pill carries the mark"
    );
    assert!(
        !on_screen(&mut h, "tab-pill-popped-t2"),
        "a tab still in the main window has none"
    );
    h.click_on("tab-t2");
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned))
            .as_deref(),
        Some("t2")
    );
    h.click_on("tab-t1");
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned))
            .as_deref(),
        Some("t1"),
        "the marked pill still selects its tab"
    );
    assert!(tab_popped(&mut h, "t1"), "selecting it keeps it popped out");
}

#[gpui::test]
fn main_window_shows_a_card_for_a_popped_active_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &split_tabs());
    let header = h.center("pane-header-p1");
    focus_p1_in_main(&mut h);

    pop_out_tab(&mut h, "t1");

    for part in [
        "popped-tab-t1",
        "popped-tab-label-t1",
        "popped-tab-focus-t1",
        "popped-tab-dock-t1",
    ] {
        assert!(on_screen(&mut h, part), "{part} is drawn");
    }
    assert!(
        !header_takes_a_right_click(&mut h, header),
        "the card replaces the grid, headers and all"
    );
    assert!(!focused_in_main(&mut h, "p1"), "the card holds no focus");
    h.sent();
    h.keys("a");
    assert!(
        h.sent_input("s1").is_empty() && h.sent_input("s2").is_empty(),
        "keys typed at the card go nowhere"
    );
}

#[gpui::test]
fn focus_window_on_the_tab_card_raises_the_popout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &split_tabs());
    pop_out_tab(&mut h, "t1");
    let mut popout = h.popout_window();
    popout.update(|window, _| window.blur());
    h.cx.update(|window, _| window.activate_window());
    assert!(!is_active(&mut popout), "the main window is in front");

    h.click_on("popped-tab-focus-t1");

    assert!(is_active(&mut popout), "the pop-out window is raised");
    assert!(
        focused_in_popout(&h, &mut popout, "p1"),
        "its focused pane's terminal has the keyboard"
    );
    assert_eq!(h.window_count(), 2, "no second pop-out window");
    assert_eq!(h.root(|root, _| root.popout_count()), 1);
}

#[gpui::test]
fn dock_back_unmarks_the_pill_and_redraws_the_grid(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &split_tabs());
    let header = h.center("pane-header-p1");
    pop_out_tab(&mut h, "t1");
    h.sent();

    h.click_on("popped-tab-dock-t1");

    assert_eq!(h.window_count(), 1, "the pop-out window closed");
    assert!(!tab_popped(&mut h, "t1"), "the pill loses its mark");
    assert!(
        header_takes_a_right_click(&mut h, header),
        "the main window draws the grid again"
    );
    let cell = h.cell_center("p1", 0, 0);
    h.click(cell, Modifiers::none());
    h.sent();
    h.keys("b");
    assert_eq!(
        h.sent_input("s1"),
        b"b",
        "the terminal types in the main grid"
    );

    pop_out_tab(&mut h, "t1");
    let mut popout = h.popout_window();
    press_in_popout(&mut popout, "popout-tab-dock-t1");
    assert_eq!(
        h.window_count(),
        1,
        "the toolbar's Dock back closes the window"
    );
    assert!(!tab_popped(&mut h, "t1"));
    let sent = h.sent();
    assert!(
        !sent.iter().any(changes_layout_or_sessions),
        "docking back asks the daemon for nothing: {sent:?}"
    );
}

#[gpui::test]
fn closing_a_tab_popout_docks_it_back(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &split_tabs());
    pop_out_tab(&mut h, "t1");
    h.sent();

    assert!(h.close_popout(), "the pop-out may close");

    assert_eq!(h.window_count(), 1);
    assert!(!tab_popped(&mut h, "t1"), "closing docks the tab back");
    assert_eq!(h.root(|root, _| root.popout_count()), 0);
    assert_eq!(h.quit_requests(), 0, "the app stays");
    let sent = h.sent();
    assert!(
        !sent.iter().any(changes_layout_or_sessions),
        "closing stops no session and removes no tab: {sent:?}"
    );
    assert!(on_screen(&mut h, "pane-header-p1"), "the grid is back");
}

#[gpui::test]
fn removing_the_tab_closes_its_popout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &split_tabs());
    pop_out_tab(&mut h, "t1");
    assert_eq!(h.window_count(), 2);

    h.send(DaemonMessage::TabRemoved {
        tab_id: "t1".to_owned(),
    });

    assert_eq!(
        h.window_count(),
        1,
        "the pop-out window closed with its tab"
    );
    assert_eq!(h.root(|root, _| root.popout_count()), 0);
    assert!(!tab_popped(&mut h, "t1"));
}

#[gpui::test]
fn a_popped_tab_panes_are_sized_from_the_popout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &split_tabs());
    h.answer_scrollback("s1", b"");
    h.answer_scrollback("s2", b"");
    pop_out_tab(&mut h, "t1");
    let popout = h.popout_window();
    h.click_on("tab-t2");
    h.sent();

    popout.simulate_resize(size(px(900.0), px(600.0)));
    popout.run_until_parked();

    let sent = h.sent();
    for session in ["s1", "s2"] {
        assert!(
            claims_size(&sent, session),
            "resizing the pop-out resizes {session} while t2 is on screen: {sent:?}"
        );
    }

    h.click_on("tab-t1");
    h.sent();
    h.cx.simulate_resize(size(px(700.0), px(500.0)));
    h.cx.run_until_parked();
    let sent = h.sent();
    assert!(
        !sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::Resize { session_id, .. } if session_id == "s1" || session_id == "s2"
        )),
        "the main window never resizes a popped tab's panes: {sent:?}"
    );
}

/// [`split_tabs`] with both of `t1`'s sessions' scrollback answered and `t1`
/// popped out.
fn t1_popped<'a>(cx: &'a mut TestAppContext, dir: &TestDir, fixture: &Fixture) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, fixture);
    h.answer_scrollback("s1", b"");
    h.answer_scrollback("s2", b"");
    assert_eq!(pop_out_tab(&mut h, "t1"), OpenOutcome::Opened);
    h
}

/// The fill of pane `pane_id`'s header, which shows its tab's focus.
fn header_fill(h: &mut Harness<'_>, pane_id: &'static str) -> Option<u32> {
    h.root(move |root, cx| root.pane_header_fill(pane_id, cx))
}

#[gpui::test]
fn clicking_a_pane_in_a_tab_popout_focuses_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = t1_popped(cx, &dir, &split_tabs());
    let mut popout = h.popout_window();
    popout.update(|window, _| window.activate_window());
    assert!(window_active(&mut popout), "the pop-out is in front");
    let (lit, unlit) = (header_fill(&mut h, "p1"), header_fill(&mut h, "p2"));
    assert_ne!(lit, unlit, "p1 starts with its tab's focus");
    assert!(focused_in_popout(&h, &mut popout, "p1"));

    let root = h.root.clone();
    let at = popout
        .update(|_, cx| root.read(cx).pane_cell_center("p2", 0, 0, cx))
        .expect("p2 is laid out in the pop-out");
    click_in_popout_at(&mut popout, at);

    assert!(
        focused_in_popout(&h, &mut popout, "p2"),
        "the click gives p2 the pop-out's keyboard"
    );
    assert_eq!(
        h.root(|root, _| root.focused_pane()).as_deref(),
        Some("p2"),
        "the tab now focuses p2"
    );
    assert_eq!(header_fill(&mut h, "p2"), lit, "the focus ring moved to p2");
    assert_eq!(header_fill(&mut h, "p1"), unlit);

    popout.update(|window, _| window.blur());
    h.cx.update(|window, _| window.activate_window());
    h.click_on("popped-tab-focus-t1");
    assert!(is_active(&mut popout), "the pop-out window is raised");
    assert!(
        focused_in_popout(&h, &mut popout, "p2"),
        "Focus window gives the keyboard back to p2"
    );
}

#[gpui::test]
fn a_turn_finishing_in_a_focused_tab_popout_is_seen(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = split_tabs();
    fixture.sessions[0] = session("s1").status("working").build();
    let mut h = t1_popped(cx, &dir, &fixture);
    let mut popout = h.popout_window();
    h.cx.update(|window, _| window.activate_window());
    popout.update(|window, _| window.activate_window());
    assert!(window_active(&mut popout), "the pop-out is in front");
    assert!(focused_in_popout(&h, &mut popout, "p1"));

    end_s1_turn(&mut h);

    assert_eq!(
        leaf_unseen(&mut h, "s1"),
        Some(false),
        "the focused tab pop-out shows s1"
    );
    let notified = h.notified();
    assert!(
        !notified.iter().any(|(title, _)| title == "Finished"),
        "a turn the owner watched finish raises no Finished: {notified:?}"
    );
}

#[gpui::test]
fn showing_a_session_in_a_popped_tab_raises_the_popout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = t1_popped(cx, &dir, &split_tabs());
    let mut popout = h.popout_window();
    h.click_on("tab-t2");
    popout.update(|window, _| window.blur());
    h.cx.update(|window, _| window.activate_window());
    assert!(!is_active(&mut popout), "the main window is in front");

    h.click_on("leaf-s2");

    assert!(is_active(&mut popout), "the tab's pop-out window is raised");
    assert!(
        focused_in_popout(&h, &mut popout, "p2"),
        "s2's pane has the pop-out's keyboard"
    );
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned))
            .as_deref(),
        Some("t2"),
        "the main window keeps its tab"
    );
    assert!(tab_popped(&mut h, "t1"));
    assert_eq!(h.window_count(), 2);
}

#[gpui::test]
fn split_in_a_tab_popout_focuses_the_new_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = t1_popped(cx, &dir, &split_tabs());
    let mut popout = h.popout_window();
    click_in_popout(&mut popout, "split-right-p2");

    h.send(DaemonMessage::TabUpdated {
        tab: tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                split(
                    SplitDirection::Horizontal,
                    pane("p2", Some("s2")),
                    pane("p4", None),
                ),
            ),
        ),
    });
    popout.run_until_parked();

    assert!(
        focused_in_popout(&h, &mut popout, "p4"),
        "the new pane has the pop-out's keyboard"
    );
    assert!(!focused_in_main(&mut h, "p4"));

    h.click_on("tab-t2");
    h.send(DaemonMessage::TabUpdated {
        tab: tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                split(
                    SplitDirection::Horizontal,
                    pane("p2", Some("s2")),
                    split(SplitDirection::Vertical, pane("p4", None), pane("p5", None)),
                ),
            ),
        ),
    });
    popout.run_until_parked();

    assert!(
        focused_in_popout(&h, &mut popout, "p5"),
        "a new pane takes the pop-out's keyboard while another tab shows in the main window"
    );
}

#[gpui::test]
fn a_main_window_dialog_from_a_tab_popout_keeps_the_tab_popped(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = split_tabs();
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    fixture.sessions[0] = session("s1").in_repo("r1").build();
    let mut h = t1_popped(cx, &dir, &fixture);
    h.send(DaemonMessage::SessionUpdated {
        session: session("s1").in_repo("r1").exited(3).build(),
        request_id: None,
    });
    let mut popout = h.popout_window();

    press_in_popout(&mut popout, "exited-new-session-p1");
    h.cx.run_until_parked();

    assert_eq!(h.window_count(), 2, "the tab stays in its pop-out");
    assert!(tab_popped(&mut h, "t1"));
    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "the spawn dialog opens in the main window"
    );
    assert!(
        h.cx.update(|window, _| window.is_window_active()),
        "the main window is in front"
    );
}

#[gpui::test]
fn dock_back_focuses_the_tab_pane_in_the_main_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &split_tabs());
    h.answer_scrollback("s1", b"");
    h.answer_scrollback("s2", b"");
    h.cx.update(|window, _| window.activate_window());
    let cell = h.cell_center("p2", 0, 0);
    h.click(cell, Modifiers::none());
    assert!(focused_in_main(&mut h, "p2"));
    assert_eq!(h.root(|root, _| root.focused_pane()).as_deref(), Some("p2"));
    pop_out_tab(&mut h, "t1");
    assert!(!focused_in_main(&mut h, "p2"), "the card holds no focus");

    h.click_on("popped-tab-dock-t1");
    assert!(
        focused_in_main(&mut h, "p2"),
        "the card's Dock back gives the tab's focused pane the keyboard"
    );

    pop_out_tab(&mut h, "t1");
    let mut popout = h.popout_window();
    press_in_popout(&mut popout, "popout-tab-dock-t1");
    h.cx.run_until_parked();
    assert_eq!(h.window_count(), 1);
    assert!(
        focused_in_main(&mut h, "p2"),
        "the toolbar's Dock back gives it the keyboard"
    );

    pop_out_tab(&mut h, "t1");
    assert!(h.close_popout(), "the pop-out may close");
    assert!(
        focused_in_main(&mut h, "p2"),
        "closing the window gives it the keyboard"
    );
}

#[gpui::test]
fn closing_a_tab_popout_mid_drag_sends_its_ratio(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &split_tabs());
    pop_out_tab(&mut h, "t1");
    let mut popout = h.popout_window();
    let (main, there) = (window_id(h.cx), window_id(&mut popout));
    popout.run_until_parked();
    let divider = popout
        .debug_bounds("divider-t1-[]")
        .expect("the divider is drawn in the pop-out")
        .center();
    popout.simulate_mouse_down(divider, MouseButton::Left, Modifiers::none());
    popout.simulate_mouse_move(
        point(divider.x + px(60.0), divider.y),
        MouseButton::Left,
        Modifiers::none(),
    );
    popout.run_until_parked();
    let lit = |h: &mut Harness<'_>, window: WindowId| {
        h.root(move |root, _| root.divider_highlighted("t1", &[], window))
    };
    assert!(lit(&mut h, there), "the drag is under way");
    h.sent();

    assert!(h.close_popout(), "the pop-out may close");

    let sent = h.sent();
    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::SetPaneRatio { tab_id, ratio, .. } if tab_id == "t1" && *ratio > 0.5
        )),
        "the close ends the drag with the ratio it set: {sent:?}"
    );
    assert!(!lit(&mut h, there) && !lit(&mut h, main), "no drag is left");
}

#[gpui::test]
fn font_shortcuts_in_the_main_window_skip_a_popped_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        r#"{ "tab_font_sizes": { "t1": 20.0 } }"#,
    )
    .expect("write the saved layout");
    let mut h = t1_popped(cx, &dir, &split_tabs());
    h.sent();

    for keys in ["ctrl-=", "ctrl-shift-=", "ctrl-0"] {
        h.keys(keys);
    }

    let sent = h.sent();
    assert!(
        !sent
            .iter()
            .any(|msg| matches!(msg, ClientMessage::SetSessionAppearance { .. })),
        "no session's size changes: {sent:?}"
    );
    assert_eq!(
        h.root(|root, _| root.tab_font_override("t1")),
        Some(20.0),
        "the popped tab's own size stays"
    );
}

// --- Diff tab pop-outs -----------------------------------------------------

/// `s1` in `t1`, then a diff tab `d1` of `r1`'s `src/main.rs`.
fn with_a_diff_tab() -> Fixture {
    let mut fixture = Fixture::single(session("s1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    fixture.tabs.push(diff_tab("d1", "src/main.rs"));
    fixture
}

/// The ids of the `GetFileSnapshot`s in `sent`.
fn snapshot_ids(sent: &[ClientMessage]) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::GetFileSnapshot { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

/// `count` numbered lines, with the lines in `changed` (1-based) edited.
fn numbered(count: usize, changed: &[usize]) -> String {
    (1..=count)
        .map(|n| {
            if changed.contains(&n) {
                format!("line {n} edited\n")
            } else {
                format!("line {n}\n")
            }
        })
        .collect()
}

/// [`with_a_diff_tab`]'s client with `d1`'s one snapshot answered with
/// `old` and `new`, and what it sent so far drained.
fn diff_landed<'a>(cx: &'a mut TestAppContext, dir: &TestDir, old: &str, new: &str) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &with_a_diff_tab());
    let ids = snapshot_ids(&h.sent());
    assert_eq!(ids.len(), 1, "d1 fetches its snapshot once: {ids:?}");
    h.send(DaemonMessage::FileSnapshot {
        id: ids[0].clone(),
        repo_id: "r1".to_owned(),
        path: "src/main.rs".to_owned(),
        against: None,
        old: old.to_owned(),
        new: new.to_owned(),
        language: "rust".to_owned(),
        unavailable: None,
        worktree_path: None,
    });
    h.sent();
    h
}

/// Whether diff tab `d1`'s view has the keyboard of the window `window`
/// tests.
fn diff_focused_in(h: &Harness<'_>, window: &mut VisualTestContext) -> bool {
    let root = h.root.clone();
    window.update(|window, cx| root.read(cx).diff_tab_focused("d1", window, cx))
}

fn diff_focused_in_main(h: &mut Harness<'_>) -> bool {
    let root = h.root.clone();
    h.cx.update(|window, cx| root.read(cx).diff_tab_focused("d1", window, cx))
}

/// `d1`'s header as text.
fn d1_header(h: &mut Harness<'_>) -> rustling_tulip_native::DiffTabHeader {
    h.root(|root, cx| root.diff_tab_header("d1", cx))
        .expect("d1 has a view")
}

/// The id of `d1`'s side-by-side view, once its diff is built.
fn d1_view(h: &mut Harness<'_>) -> EntityId {
    h.root(|root, cx| root.diff_tab_view("d1", cx))
        .expect("d1's diff is built")
        .entity_id()
}

fn d1_current_change(h: &mut Harness<'_>) -> Option<usize> {
    h.root(|root, cx| {
        root.diff_tab_view("d1", cx)
            .and_then(|view| view.read(cx).current_hunk())
    })
}

/// The whitespace setting saved in `native-ui.json`.
fn saved_include_whitespace(dir: &TestDir) -> serde_json::Value {
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).expect("native-ui.json");
    let saved: serde_json::Value = serde_json::from_str(&text).expect("native-ui.json is JSON");
    saved["diff_include_whitespace"].clone()
}

#[gpui::test]
fn tab_menu_pops_out_a_diff_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = diff_landed(cx, &dir, "a\n", "b\n");
    let view = d1_view(&mut h);

    h.right_click_on("tab-d1");
    assert!(
        h.root(|root, _| root.tab_menu_text())
            .contains(&"Pop out".to_owned())
    );
    h.click_on("tab-menu-popout");

    assert_eq!(h.window_count(), 2, "the diff tab has a window of its own");
    assert!(tab_popped(&mut h, "d1"));
    let sent = h.sent();
    assert!(
        snapshot_ids(&sent).is_empty(),
        "the pop-out shows the snapshot held: {sent:?}"
    );
    assert!(
        !sent.iter().any(changes_layout_or_sessions),
        "popping a diff tab out asks the daemon for nothing: {sent:?}"
    );
    let mut popout = h.popout_window();
    assert_eq!(
        popout.window_title().as_deref(),
        Some("src/main.rs"),
        "titled by its tab"
    );
    for part in [
        "popout-tab-title-d1",
        "popout-tab-dock-d1",
        "diff-path",
        "diff-whitespace",
    ] {
        assert!(drawn_in_popout(&mut popout, part), "{part} is drawn");
    }
    assert_eq!(d1_view(&mut h), view, "the tab's own view, not a copy");
    assert!(
        diff_focused_in(&h, &mut popout),
        "the diff has the pop-out's keyboard"
    );

    h.right_click_on("tab-d1");
    assert!(
        h.root(|root, _| root.tab_menu_text())
            .contains(&"Focus window".to_owned()),
        "the row reads Focus window once the tab is out"
    );
}

#[gpui::test]
fn main_window_shows_a_card_for_a_popped_diff_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = diff_landed(cx, &dir, "a\n", "b\n");
    h.cx.update(|window, _| window.activate_window());
    pop_out_tab(&mut h, "d1");
    assert!(
        on_screen(&mut h, "tab-pill-popped-d1"),
        "the popped diff tab's pill carries the mark"
    );

    h.click_on("tab-d1");

    for part in [
        "popped-tab-d1",
        "popped-tab-label-d1",
        "popped-tab-focus-d1",
        "popped-tab-dock-d1",
    ] {
        assert!(on_screen(&mut h, part), "{part} is drawn");
    }
    assert!(
        !ever_drawn_in_main(&mut h, "diff-path"),
        "the card stands in for the diff"
    );
    assert!(!diff_focused_in_main(&mut h), "the card holds no focus");

    let mut popout = h.popout_window();
    popout.update(|window, _| window.blur());
    h.click_on("popped-tab-focus-d1");
    assert!(is_active(&mut popout), "Focus window raises the pop-out");
    assert!(diff_focused_in(&h, &mut popout));
    assert_eq!(h.root(|root, _| root.popout_count()), 1);
}

#[gpui::test]
fn diff_toggles_work_in_a_diff_popout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = diff_landed(cx, &dir, "a\n  b\nc\n", "a\nb\nc\n");
    assert_eq!(d1_header(&mut h).count, "1 change");
    pop_out_tab(&mut h, "d1");
    let mut popout = h.popout_window();
    h.sent();

    click_in_popout(&mut popout, "diff-whitespace");

    let shown = d1_header(&mut h);
    assert!(!shown.include_whitespace, "the toggle flips the view");
    assert_eq!(shown.count, "no changes");
    assert_eq!(
        saved_include_whitespace(&dir),
        json!(false),
        "the setting flips"
    );
    let highlight = d1_header(&mut h).highlight;
    click_in_popout(&mut popout, "diff-highlight");
    assert_eq!(d1_header(&mut h).highlight, !highlight);
    let sent = h.sent();
    assert!(
        snapshot_ids(&sent).is_empty(),
        "rebuilt from the texts held: {sent:?}"
    );
}

#[gpui::test]
fn docking_a_diff_tab_back_keeps_its_view(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (old, new) = (numbered(40, &[]), numbered(40, &[3, 20, 37]));
    let mut h = diff_landed(cx, &dir, &old, &new);
    h.cx.update(|window, _| window.activate_window());
    h.click_on("tab-d1");
    let view = d1_view(&mut h);
    pop_out_tab(&mut h, "d1");
    let mut popout = h.popout_window();
    click_in_popout(&mut popout, "diff-last");
    assert_eq!(d1_current_change(&mut h), Some(2));
    h.sent();

    press_in_popout(&mut popout, "popout-tab-dock-d1");
    h.cx.run_until_parked();

    assert_eq!(
        h.window_count(),
        1,
        "the toolbar's Dock back closes the window"
    );
    assert!(!tab_popped(&mut h, "d1"));
    assert_eq!(d1_view(&mut h), view, "the same view comes back");
    assert_eq!(
        d1_current_change(&mut h),
        Some(2),
        "its place in the diff stays"
    );
    assert!(
        diff_focused_in_main(&mut h),
        "the diff takes the main window's keyboard"
    );
    h.click_on("diff-whitespace");
    assert!(
        !d1_header(&mut h).include_whitespace,
        "the toggles work in the main window again"
    );
    let sent = h.sent();
    assert!(
        snapshot_ids(&sent).is_empty(),
        "nothing refetched: {sent:?}"
    );
}

#[gpui::test]
fn closing_a_diff_popout_docks_it_back(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = diff_landed(cx, &dir, "a\n  b\nc\n", "a\nb\nc\n");
    pop_out_tab(&mut h, "d1");
    h.sent();

    assert!(h.close_popout(), "the pop-out may close");

    assert_eq!(h.window_count(), 1);
    assert!(!tab_popped(&mut h, "d1"), "closing docks the tab back");
    assert_eq!(h.root(|root, _| root.popout_count()), 0);
    assert_eq!(h.quit_requests(), 0, "the app stays");
    let sent = h.sent();
    assert!(
        !sent.iter().any(changes_layout_or_sessions) && snapshot_ids(&sent).is_empty(),
        "closing removes no tab and refetches nothing: {sent:?}"
    );
    h.click_on("tab-d1");
    assert!(
        on_screen(&mut h, "diff-path"),
        "the diff draws in the main window"
    );
    h.click_on("diff-whitespace");
    assert!(
        !d1_header(&mut h).include_whitespace,
        "the toggles work in the main window"
    );
}

/// `r1`'s main tree with `c.rs` changed, as a `RepoStatus`.
fn c_rs_changed() -> DaemonMessage {
    serde_json::from_value(json!({
        "type": "repo_status",
        "repo_id": "r1",
        "index_changes": [],
        "worktree_changes": [{ "path": "c.rs", "status": "M", "from_path": null }],
        "worktree_path": null,
    }))
    .expect("status fixture")
}

#[gpui::test]
fn reopening_a_popped_diff_tab_raises_its_popout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![repo("r1", "D:/src/r1")],
        tabs: vec![tab("t1", &pane("p1", None)), diff_tab("d1", "c.rs")],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.click_on("activity-source-control");
    h.send(c_rs_changed());
    pop_out_tab(&mut h, "d1");
    let mut popout = h.popout_window();
    popout.update(|window, _| window.blur());
    h.cx.update(|window, _| window.activate_window());
    assert!(!is_active(&mut popout), "the main window is in front");
    let active = |h: &mut Harness<'_>| h.root(|root, _| root.active_tab_id().map(str::to_owned));
    assert_eq!(active(&mut h).as_deref(), Some("t1"));
    h.sent();

    h.click_on("sc-row-changes-r1::|c.rs");
    let open_id = h
        .sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::OpenDiffTab { id, .. } => Some(id),
            _ => None,
        })
        .expect("the row asks to open its diff");
    h.send(DaemonMessage::DiffTabOpened {
        id: open_id,
        tab_id: "d1".to_owned(),
    });

    assert!(is_active(&mut popout), "the diff's pop-out is raised");
    assert!(
        diff_focused_in(&h, &mut popout),
        "the diff has the pop-out's keyboard"
    );
    assert_eq!(
        active(&mut h).as_deref(),
        Some("t1"),
        "the main window keeps its tab"
    );
}

#[gpui::test]
fn switching_the_main_window_to_a_popped_diff_tab_takes_keys_from_the_terminal(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = diff_landed(cx, &dir, "a\n", "b\n");
    h.answer_scrollback("s1", b"");
    h.cx.update(|window, _| window.activate_window());
    focus_p1_in_main(&mut h);
    pop_out_tab(&mut h, "d1");
    assert!(
        focused_in_main(&mut h, "p1"),
        "t1's pane keeps the keyboard"
    );

    h.keys("ctrl-tab");

    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned))
            .as_deref(),
        Some("d1")
    );
    assert!(!focused_in_main(&mut h, "p1"), "the card takes it from p1");
    h.sent();
    h.keys("a");
    assert!(
        h.sent_input("s1").is_empty(),
        "keys typed at the card reach no terminal"
    );
}

#[gpui::test]
fn closing_an_overlay_in_a_diff_popout_hands_the_diff_its_keyboard(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = diff_landed(cx, &dir, "a\n", "b\n");
    pop_out_tab(&mut h, "d1");
    let mut popout = h.popout_window();
    assert!(diff_focused_in(&h, &mut popout));

    // No overlay opens from a diff pop-out's own controls; a pane close
    // asked for from its window draws there, as a gutter menu's would.
    in_popout(&h, &mut popout, |root, window, cx| {
        root.close_pane_clicked("t1", "p1", Some("s1"), window, cx);
    });
    assert!(drawn_in_popout(&mut popout, "pane-close"));
    assert!(
        !diff_focused_in(&h, &mut popout),
        "the dialog has the keyboard"
    );
    popout.simulate_keystrokes("escape");
    popout.run_until_parked();

    assert!(!h.root(|root, _| root.pane_close_open()));
    assert!(
        diff_focused_in(&h, &mut popout),
        "the diff has the pop-out's keyboard back"
    );
}

#[gpui::test]
fn toggles_work_after_a_quick_pop_and_dock(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = diff_landed(cx, &dir, "a\n  b\nc\n", "a\nb\nc\n");
    h.click_on("tab-d1");
    let key = tab_key("d1");

    h.root_update(move |root, window, cx| {
        root.open_popout(key.clone(), window, cx);
        root.dock_back(&key, cx);
    });
    pop_out_tab(&mut h, "d1");
    h.root_update(|root, _, cx| root.dock_back(&tab_key("d1"), cx));

    assert_eq!(h.window_count(), 1);
    h.click_on("diff-whitespace");
    assert!(
        !d1_header(&mut h).include_whitespace,
        "the toggles work in the main window"
    );
}

#[gpui::test]
fn removing_a_diff_tab_closes_its_popout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = diff_landed(cx, &dir, "a\n", "b\n");
    pop_out_tab(&mut h, "d1");
    assert_eq!(h.window_count(), 2);

    h.send(DaemonMessage::TabRemoved {
        tab_id: "d1".to_owned(),
    });

    assert_eq!(
        h.window_count(),
        1,
        "the pop-out window closed with its tab"
    );
    assert_eq!(h.root(|root, _| root.popout_count()), 0);
    assert!(!tab_popped(&mut h, "d1"));
}

/// What `native-ui.json` keeps for pane `pane_id` of tab `tab_id` popped out
/// before its window has moved.
fn saved_pane(tab_id: &str, pane_id: &str) -> serde_json::Value {
    json!({
        "key": { "kind": "pane", "tab_id": tab_id, "pane_id": pane_id },
        "window": null,
    })
}

/// Lets the layout's save debounce run out, so a pending save is written.
fn let_the_save_run(h: &mut Harness<'_>) {
    h.advance(std::time::Duration::from_millis(500));
}

#[gpui::test]
fn opening_a_popout_saves_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));

    pop_out(&mut h, "t1", "p1");
    let_the_save_run(&mut h);

    assert_eq!(saved_ui(&dir)["popouts"], json!([saved_pane("t1", "p1")]));
}

#[gpui::test]
fn docking_back_forgets_a_saved_popout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    pop_out(&mut h, "t1", "p1");
    let_the_save_run(&mut h);
    assert_eq!(saved_ui(&dir)["popouts"], json!([saved_pane("t1", "p1")]));

    let key = pane_key("t1", "p1");
    h.root_update(move |root, _, cx| root.dock_back(&key, cx));
    let_the_save_run(&mut h);

    assert_eq!(saved_ui(&dir)["popouts"], json!([]));
}

#[gpui::test]
fn closing_a_popout_window_forgets_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    pop_out(&mut h, "t1", "p1");
    let_the_save_run(&mut h);
    assert_eq!(saved_ui(&dir)["popouts"], json!([saved_pane("t1", "p1")]));

    assert!(h.close_popout());
    let_the_save_run(&mut h);

    assert_eq!(saved_ui(&dir)["popouts"], json!([]));
}

#[gpui::test]
fn quitting_keeps_the_saved_popouts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture::single(session("s1").in_repo("r1").exited(0).build());
    let mut h = Harness::with(cx, &dir, &fixture);
    pop_out(&mut h, "t1", "p1");

    assert!(h.cx.simulate_close(), "nothing to ask about: it may close");
    h.cx.run_until_parked();

    assert_eq!(h.quit_requests(), 1);
    assert_eq!(h.window_count(), 1, "the pop-out closed with the app");
    assert_eq!(
        saved_ui(&dir)["popouts"],
        json!([saved_pane("t1", "p1")]),
        "the open set is written before the app goes, with no debounce wait"
    );
    let_the_save_run(&mut h);
    assert_eq!(
        saved_ui(&dir)["popouts"],
        json!([saved_pane("t1", "p1")]),
        "closing with the app forgets no pop-out"
    );
}

#[gpui::test]
fn popouts_reopen_once_tabs_and_sessions_arrive(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(&dir, &json!({ "popouts": [saved_pane("t1", "p1")] }));
    let fixture = Fixture::single(session("s1").build());
    let mut h = Harness::open(cx, &dir);

    h.send(DaemonMessage::Tabs {
        tabs: fixture.tabs.clone(),
    });
    assert_eq!(
        h.window_count(),
        1,
        "nothing reopens before the session list"
    );
    assert!(!popped(&mut h));

    h.send(DaemonMessage::Sessions {
        sessions: fixture.sessions.clone(),
    });
    assert_eq!(h.window_count(), 2, "the saved pop-out reopened");
    assert!(popped(&mut h));
    h.sent();
    let mut popout = h.popout_window();
    popout.simulate_keystrokes("a");
    assert_eq!(h.sent_input("s1"), b"a", "its terminal takes keys");
}

#[gpui::test]
fn a_saved_popout_whose_tab_is_gone_is_dropped(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "popouts": [saved_pane("t9", "p9"), saved_pane("t1", "p1")] }),
    );

    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));

    assert_eq!(
        h.window_count(),
        2,
        "only the pane that still exists reopens"
    );
    assert!(popped(&mut h));
    let_the_save_run(&mut h);
    assert_eq!(
        saved_ui(&dir)["popouts"],
        json!([saved_pane("t1", "p1")]),
        "the gone entry is dropped from the saved list"
    );
}

#[gpui::test]
fn a_popout_saved_on_a_missing_monitor_opens_at_its_size_on_the_primary(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let saved_place = json!({
        "x": 5000.0,
        "y": 100.0,
        "width": 900.0,
        "height": 500.0,
        "maximized": false,
        "display_uuid": "unplugged",
    });
    seed_ui(
        &dir,
        &json!({ "popouts": [{
            "key": { "kind": "pane", "tab_id": "t1", "pane_id": "p1" },
            "window": saved_place,
        }] }),
    );

    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));

    let mut popout = h.popout_window();
    let bounds = popout.update(|window, _| window.bounds());
    let primary =
        h.cx.update(|_, cx| cx.primary_display().expect("a display").bounds());
    assert_eq!(bounds.size, size(px(900.0), px(500.0)), "its saved size");
    assert_eq!(
        bounds.center(),
        primary.center(),
        "centred on the primary monitor"
    );
}

#[gpui::test]
fn a_restored_popout_leaves_the_keyboard_in_the_main_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "popouts": [{ "key": { "kind": "session", "session_id": "s2" } }] }),
    );
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    // Named on the command line, s1's pane takes the main window's keyboard
    // as the layout arrives, just before the saved pop-out reopens.
    let mut h = Harness::open_wanting(cx, &dir, Some("s1"));
    h.cx.update(|window, _| window.activate_window());

    h.load(&fixture);

    assert_eq!(h.window_count(), 2, "the session pop-out reopened");
    let mut popout = h.popout_window();
    assert!(!is_active(&mut popout), "the pop-out opened behind");
    assert!(
        h.cx.update(|window, _| window.is_window_active()),
        "the main window stays in front"
    );
    assert!(
        focused_in_popout(&h, &mut popout, "popout:s2"),
        "the pop-out's terminal has its own window's keyboard"
    );
    assert!(
        focused_in_main(&mut h, "p1"),
        "p1 keeps the main window's keyboard"
    );
    h.sent();
    h.keys("x");
    assert_eq!(
        h.sent_input("s1"),
        b"x",
        "keys still reach the main window's pane"
    );
}

#[gpui::test]
fn a_reconnect_restores_nothing_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(&dir, &json!({ "popouts": [saved_pane("t1", "p1")] }));
    let fixture = Fixture::single(session("s1").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(h.window_count(), 2);

    h.lose_connection();
    h.welcome_with_features();
    h.send_welcome(support::PROTOCOL);
    h.load(&fixture);
    let_the_save_run(&mut h);

    assert_eq!(
        h.window_count(),
        2,
        "the open pop-out stays, and no second opens"
    );
    assert_eq!(h.root(|root, _| root.popout_count()), 1);
    assert_eq!(
        saved_ui(&dir)["popouts"],
        json!([saved_pane("t1", "p1")]),
        "the open pop-out is still saved"
    );
}

/// `s1` in `t1` and `s2` in `t2`, both stopped, so the main window's close
/// asks nothing.
fn two_stopped_tabs() -> Fixture {
    let mut fixture = Fixture::single(session("s1").in_repo("r1").exited(0).build());
    fixture
        .sessions
        .push(session("s2").in_repo("r1").exited(0).build());
    fixture.tabs.push(tab("t2", &pane("p2", Some("s2"))));
    fixture
}

/// What `native-ui.json` keeps for a session pop-out of `session_id` before
/// its window has moved.
fn saved_session(session_id: &str) -> serde_json::Value {
    json!({
        "key": { "kind": "session", "session_id": session_id },
        "window": null,
    })
}

/// `s1` in `t1`, and `s2` listed in no pane.
fn s1_and_s2() -> Fixture {
    let mut fixture = Fixture::single(session("s1").build());
    fixture.sessions.push(session("s2").build());
    fixture
}

#[gpui::test]
fn closing_all_windows_keeps_the_saved_popouts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_stopped_tabs());
    pop_out(&mut h, "t1", "p1");
    pop_out(&mut h, "t2", "p2");
    let_the_save_run(&mut h);
    let both = json!([saved_pane("t1", "p1"), saved_pane("t2", "p2")]);
    assert_eq!(saved_ui(&dir)["popouts"], both);

    // Windows' "Close all windows": each pop-out's close request, then the
    // main window's, with no save in between.
    assert!(h.close_popout());
    assert!(h.close_popout());
    assert!(h.cx.simulate_close(), "nothing to ask about: it may close");
    h.cx.run_until_parked();

    assert_eq!(h.quit_requests(), 1);
    assert_eq!(
        saved_ui(&dir)["popouts"],
        both,
        "the pop-outs closed on the way out are saved again, in their order"
    );
}

#[gpui::test]
fn a_closed_popout_stays_forgotten_once_saved(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &two_stopped_tabs());
    pop_out(&mut h, "t1", "p1");
    let_the_save_run(&mut h);
    assert!(h.close_popout());
    let_the_save_run(&mut h);
    assert_eq!(saved_ui(&dir)["popouts"], json!([]));

    assert!(h.cx.simulate_close(), "nothing to ask about: it may close");
    h.cx.run_until_parked();

    assert_eq!(h.quit_requests(), 1);
    assert_eq!(
        saved_ui(&dir)["popouts"],
        json!([]),
        "the quit does not save again a pop-out forgotten by an earlier write"
    );
}

#[gpui::test]
fn a_popout_opened_before_the_restore_stays_saved(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(&dir, &json!({ "popouts": [saved_session("s2")] }));
    let fixture = s1_and_s2();
    let mut h = Harness::open(cx, &dir);
    h.send(DaemonMessage::Tabs {
        tabs: fixture.tabs.clone(),
    });
    assert_eq!(
        pop_out(&mut h, "t1", "p1"),
        OpenOutcome::Opened,
        "p1 pops out before the session list arrives"
    );

    h.send(DaemonMessage::Sessions {
        sessions: fixture.sessions.clone(),
    });

    assert_eq!(
        h.window_count(),
        3,
        "the saved pop-out reopened beside p1's"
    );
    assert!(popped(&mut h));
    let_the_save_run(&mut h);
    assert_eq!(
        saved_ui(&dir)["popouts"],
        json!([saved_pane("t1", "p1"), saved_session("s2")]),
        "every open pop-out is saved, in the order they opened"
    );
}

#[gpui::test]
fn a_saved_session_that_cannot_attach_does_not_block_its_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "popouts": [saved_session("s1"), saved_pane("t1", "p1")] }),
    );

    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").headless().build()));

    assert_eq!(h.window_count(), 2, "p1's pop-out reopened");
    assert!(popped(&mut h));
    let_the_save_run(&mut h);
    assert_eq!(
        saved_ui(&dir)["popouts"],
        json!([saved_pane("t1", "p1")]),
        "the session pop-out that could not open is dropped"
    );
}

#[gpui::test]
fn a_restore_does_not_raise_an_inactive_main_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(&dir, &json!({ "popouts": [saved_pane("t1", "p1")] }));
    let mut h = Harness::open(cx, &dir);
    h.cx.deactivate_window();

    h.load(&Fixture::single(session("s1").build()));

    assert_eq!(h.window_count(), 2, "the saved pop-out reopened");
    let mut popout = h.popout_window();
    assert!(!is_active(&mut popout), "the pop-out opened behind");
    assert!(
        !h.cx.update(|window, _| window.is_window_active()),
        "the main window, not in front when the restore began, is not raised"
    );
}

#[gpui::test]
fn a_restore_triggered_from_a_popout_uses_the_main_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(&dir, &json!({ "popouts": [saved_session("s2")] }));
    let fixture = s1_and_s2();
    let mut h = Harness::open(cx, &dir);
    h.send(DaemonMessage::Tabs {
        tabs: fixture.tabs.clone(),
    });
    assert_eq!(pop_out(&mut h, "t1", "p1"), OpenOutcome::Opened);
    let popped = h.popout_window().window_handle();
    let main = h.cx.update(|window, _| {
        window.activate_window();
        window.window_handle()
    });
    let main_display = h.cx.update(|window, cx| window.display(cx).map(|d| d.id()));

    h.send(DaemonMessage::Sessions {
        sessions: fixture.sessions.clone(),
    });

    assert_eq!(h.window_count(), 3, "the saved session pop-out reopened");
    let restored =
        h.cx.windows()
            .into_iter()
            .find(|window| *window != main && *window != popped)
            .expect("the restored pop-out's window");
    let restored_display =
        h.cx.update_window(restored, |_, window, cx| window.display(cx).map(|d| d.id()))
            .expect("the restored window is open");
    assert_eq!(
        restored_display, main_display,
        "it opens on the main window's display"
    );
    assert!(
        h.cx.update(|window, _| window.is_window_active()),
        "the main window is the one in front"
    );
}

#[gpui::test]
fn a_pane_popout_over_a_remote_session_is_titled_by_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = mixed_tabs(cx, &dir);

    assert_eq!(pop_out(&mut h, "t1", "p2"), OpenOutcome::Opened);

    let mut popout = h.popout_window();
    assert_eq!(
        popout.window_title().as_deref(),
        Some("b1"),
        "titled by the remote session the pane shows, not as an empty pane"
    );
}

/// Seeds [`REMOTE`] as a saved host and `popouts` as the pop-outs saved at
/// the last quit.
fn seed_saved_popouts(dir: &TestDir, popouts: &serde_json::Value) {
    seed_ui(
        dir,
        &json!({
            "saved_hosts": [{ "name": REMOTE, "port": 8787 }],
            "popouts": popouts,
        }),
    );
}

#[gpui::test]
fn a_saved_tab_popout_waits_for_every_host_it_names(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let saved = json!({ "key": { "kind": "tab", "tab_id": "t1" }, "window": null });
    seed_saved_popouts(&dir, &json!([saved]));
    let mut h = Harness::with(cx, &dir, &two_tab_fixture());
    assert_eq!(
        h.window_count(),
        1,
        "t1 shows a session of the remote host, whose list has not arrived"
    );

    h.host(&saved_remote()).connect();
    remote_lists(&mut h, &["b1", "b2"]);

    assert_eq!(h.window_count(), 2, "the tab reopens on the host's list");
    h.advance(std::time::Duration::from_millis(500));
    assert_eq!(saved_ui(&dir)["popouts"], json!([saved]));
}

#[gpui::test]
fn a_saved_pane_popout_whose_session_the_remote_list_omits_never_opens(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let saved = json!({
        "key": { "kind": "pane", "tab_id": "t2", "pane_id": "p3" },
        "window": null,
    });
    seed_saved_popouts(&dir, &json!([saved]));
    let mut h = Harness::with(cx, &dir, &two_tab_fixture());
    assert_eq!(h.window_count(), 1, "p3 waits for the remote host's list");

    h.host(&saved_remote()).connect();
    remote_lists(&mut h, &["b1"]);

    assert_eq!(
        h.window_count(),
        1,
        "the pane's session is being forgotten, so its pop-out never opens"
    );
    h.advance(std::time::Duration::from_millis(500));
    assert_eq!(
        saved_ui(&dir)["popouts"],
        json!([]),
        "the saved entry is gone"
    );
}

#[gpui::test]
fn a_saved_pane_popout_survives_a_resume(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_saved_popouts(&dir, &json!([saved_p3()]));
    let mut h = Harness::with(cx, &dir, &two_tab_fixture());
    assert_eq!(h.window_count(), 1, "p3 waits for the remote host's list");

    h.host(&saved_remote()).connect();
    h.host(&saved_remote()).send(DaemonMessage::Sessions {
        sessions: vec![session("b1").build(), resumed_b2()],
    });

    assert_eq!(h.window_count(), 2, "p3 has a successor, so it reopens");
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t2", &remote_pane("p3", "b3")),
    });
    assert_eq!(h.window_count(), 2, "and stays through the rebind");
    h.advance(std::time::Duration::from_millis(500));
    assert_eq!(
        saved_ui(&dir)["popouts"][0]["key"],
        saved_p3()["key"],
        "the saved entry stays"
    );
}

#[gpui::test]
fn a_stale_list_does_not_save_a_pane_popout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_saved_popouts(&dir, &json!([saved_p3()]));
    let mut h = Harness::open(cx, &dir);
    h.send(DaemonMessage::Tabs { tabs: two_tabs() });
    h.host(&saved_remote()).connect();
    remote_lists(&mut h, &["b1"]);
    h.host(&saved_remote()).send(DaemonMessage::Sessions {
        sessions: vec![session("b1").build(), resumed_b2()],
    });
    h.host(&saved_remote()).send(support::welcome(PROTOCOL));

    h.send(DaemonMessage::Sessions {
        sessions: vec![session("a1").build()],
    });

    assert_eq!(
        h.window_count(),
        1,
        "p3's forget is on its way, and the list naming a successor predates the Welcome"
    );
}

/// [`REMOTE`]'s session `b3`, which replaced `b2`.
fn resumed_b2() -> protocol::SessionSnapshot {
    let mut snapshot = session("b3").build();
    snapshot.resumed_from = vec!["b2".to_owned()];
    snapshot
}

#[gpui::test]
fn a_remote_resume_in_a_popout_keeps_the_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_over(cx, &dir, &two_tab_fixture(), &["b1", "b2"]);
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t2", &remote_pane("p3", "b2")),
    });
    pop_out(&mut h, "t2", "p3");
    assert_eq!(h.window_count(), 2, "the pop-out has a window of its own");
    h.sent();

    h.host(&saved_remote()).send(DaemonMessage::SessionUpdated {
        session: resumed_b2(),
        request_id: None,
    });
    h.host(&saved_remote()).send(DaemonMessage::SessionRemoved {
        session_id: "b2".to_owned(),
    });
    assert!(
        h.sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::RebindHostSessions { .. })),
        "This PC is asked to rebind the pane"
    );
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t2", &remote_pane("p3", "b3")),
    });

    assert_eq!(h.window_count(), 2, "the pop-out window stays open");
    assert!(h.root(|root, _| root.is_popped_out("t2", "p3")));
    let mut popout = h.popout_window();
    popout.run_until_parked();
    assert_eq!(popout.window_title().as_deref(), Some("b3"));
}

/// The saved pane pop-out of `t2`'s `p3`, which shows [`REMOTE`]'s `b2`.
fn saved_p3() -> serde_json::Value {
    json!({
        "key": { "kind": "pane", "tab_id": "t2", "pane_id": "p3" },
        "window": null,
    })
}

#[gpui::test]
fn a_saved_pane_popout_whose_session_the_remote_list_omits_never_opens_when_the_list_comes_first(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    seed_saved_popouts(&dir, &json!([saved_p3()]));
    let mut h = Harness::open(cx, &dir);
    h.host(&saved_remote()).connect();
    remote_lists(&mut h, &["b1"]);

    h.load(&two_tab_fixture());

    assert_eq!(
        h.window_count(),
        1,
        "the remote host's list omits b2, so p3's pop-out never opens"
    );
    h.advance(std::time::Duration::from_millis(500));
    assert_eq!(
        saved_ui(&dir)["popouts"],
        json!([]),
        "the saved entry is gone"
    );
}

#[gpui::test]
fn a_saved_pane_popout_whose_session_the_remote_list_omits_never_opens_while_this_pc_is_offline(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    seed_saved_popouts(&dir, &json!([saved_p3()]));
    let mut h = Harness::with(cx, &dir, &two_tab_fixture());
    h.lose_connection();

    h.host(&saved_remote()).connect();
    remote_lists(&mut h, &["b1"]);
    assert_eq!(
        h.window_count(),
        1,
        "nothing opens while This PC is offline"
    );

    h.welcome_with_features();
    h.send_welcome(PROTOCOL);
    h.load(&two_tab_fixture());
    assert_eq!(h.window_count(), 1, "nor once This PC's layout is back");
    h.advance(std::time::Duration::from_millis(500));
    assert_eq!(
        saved_ui(&dir)["popouts"],
        json!([]),
        "the saved entry is gone"
    );
}

#[gpui::test]
fn a_moved_saved_pane_popout_waits_for_its_own_host(cx: &mut TestAppContext) {
    const OTHER: &str = "den.tail1234.ts.net";
    let dir = TestDir::new();
    let saved_pane = |pane_id: &str| json!({ "key": { "kind": "pane", "tab_id": "t2", "pane_id": pane_id }, "window": null });
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [
                { "name": REMOTE, "port": 8787 },
                { "name": OTHER, "port": 8787 },
            ],
            "popouts": [saved_pane("p3"), saved_pane("p4")],
        }),
    );
    let mut fixture = two_tab_fixture();
    fixture.tabs[1] = tab(
        "t2",
        &split(
            SplitDirection::Horizontal,
            support::remote_pane("p3", "b2"),
            support::remote_pane("p4", "b3"),
        ),
    );
    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(h.window_count(), 1, "both wait for the remote host's list");

    // Each pane moves to a tab of its own before the host lists.
    let mut moved = fixture.tabs.clone();
    moved[1] = tab("t5", &support::remote_pane("p3", "b2"));
    moved.push(tab("t6", &support::remote_pane("p4", "b3")));
    h.send(DaemonMessage::Tabs { tabs: moved });
    let mut other = h.host(&Target::tailnet(OTHER, Some(8787)));
    other.connect();
    other.send(DaemonMessage::Sessions {
        sessions: Vec::new(),
    });
    assert_eq!(
        h.window_count(),
        1,
        "another host's list is not the moved panes' host's"
    );

    remote_lists(&mut h, &["b1", "b3"]);

    assert_eq!(
        h.window_count(),
        2,
        "p4's session is listed and its pop-out opens; p3's is not and never opens"
    );
    h.advance(std::time::Duration::from_millis(500));
    assert_eq!(
        saved_ui(&dir)["popouts"],
        json!([{ "key": { "kind": "pane", "tab_id": "t6", "pane_id": "p4" }, "window": null }]),
        "p3's entry is gone; p4's follows its pane to its new tab"
    );
}

#[gpui::test]
fn a_pruned_pane_closes_its_popout(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_over(cx, &dir, &two_tab_fixture(), &["b1", "b2"]);
    assert_eq!(pop_out(&mut h, "t2", "p3"), OpenOutcome::Opened);
    assert_eq!(h.window_count(), 2);

    h.send(DaemonMessage::TabRemoved {
        tab_id: "t2".to_owned(),
    });

    assert_eq!(h.window_count(), 1, "the pruned pane's pop-out closed");
    h.advance(std::time::Duration::from_millis(500));
    assert_eq!(
        saved_ui(&dir)["popouts"],
        json!([]),
        "the saved entry is gone"
    );
}

/// The open window titled `title`.
fn window_titled(h: &mut Harness<'_>, title: &str) -> VisualTestContext {
    h.cx.run_until_parked();
    h.cx.windows()
        .into_iter()
        .map(|window| VisualTestContext::from_window(window, &h.cx.cx))
        .find_map(|mut window| (window.window_title().as_deref() == Some(title)).then_some(window))
        .expect("a window with the title is open")
}

#[gpui::test]
fn rekeying_a_remote_session_popout_keeps_its_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("a1").build());
    fixture
        .sessions
        .push(session("b1").user_label("Here").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    let remote = Target::tailnet(REMOTE, None);
    let mut host = h.host(&remote);
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").build()],
    });
    let here = PopoutKey::session(&HostKey::Local, "b1");
    let there = PopoutKey::session(&HostKey::Remote(REMOTE.to_owned()), "b1");
    for key in [here, there] {
        assert_eq!(
            h.root_update(move |root, window, cx| root.open_popout(key, window, cx)),
            OpenOutcome::Opened,
            "each host's b1 has a pop-out of its own"
        );
    }
    // This PC's b1 goes; its pop-out stays open on its key, beside the
    // remote host's b1 that is restarted.
    h.send(DaemonMessage::SessionRemoved {
        session_id: "b1".to_owned(),
    });
    h.host(&remote).answer_scrollback("b1", b"");
    h.host(&remote).send(DaemonMessage::SessionUpdated {
        session: session("b1").exited(3).build(),
        request_id: None,
    });
    h.host(&remote).sent();

    let mut popout = window_titled(&mut h, "b1");
    click_in_popout(&mut popout, "exited-restart-popout:box.tail1234.ts.net:b1");
    let restart = h
        .host(&remote)
        .sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::DuplicateSession {
                session_id,
                request_id,
                ..
            } if session_id == "b1" => request_id,
            _ => None,
        })
        .expect("a restart of the remote b1 with a request id");
    h.host(&remote).send(DaemonMessage::SessionUpdated {
        session: session("b1b").build(),
        request_id: Some(restart),
    });

    assert_eq!(h.window_count(), 3, "both pop-outs stay open");
    window_titled(&mut h, "b1b");
    window_titled(&mut h, "Session not found");
    h.advance(std::time::Duration::from_millis(500));
    assert_eq!(
        saved_ui(&dir)["popouts"],
        json!([
            { "key": { "kind": "session", "session_id": "b1" }, "window": null },
            {
                "key": { "kind": "session", "session_id": "b1b", "host": REMOTE },
                "window": null,
            },
        ]),
        "the remote pop-out follows the copy on its host; This PC's b1 keeps its own"
    );
}

/// The daemon id both of [`seed_two_paths`]' hosts reach.
const DAEMON: &str = "6f1e2d3c-0000-4000-8000-000000000001";

/// The SSH host `me@devbox`'s key as the wire's `host` value.
const SSH_KEY: &str = "ssh:me@devbox";

fn ssh_box() -> Target {
    Target::parse_ssh("me@devbox", None).expect("an SSH host string")
}

/// Saves [`REMOTE`] first and the SSH host `me@devbox` second, both
/// switched on, with `popouts` as the saved pop-outs.
fn seed_two_paths(dir: &TestDir, popouts: &serde_json::Value) {
    seed_ui(
        dir,
        &json!({
            "saved_hosts": [
                { "name": REMOTE, "port": 8787 },
                { "kind": "ssh", "name": "me@devbox" },
            ],
            "popouts": popouts,
        }),
    );
}

/// Opens `target`'s connection beside This PC's and has its daemon say
/// `Welcome`, naming itself `id`.
fn welcome_as(h: &mut Harness<'_>, target: &Target, id: &str) {
    let mut conn = Connection::for_target(target.clone());
    conn.on_connecting();
    conn.on_socket_open(8787);
    conn.on_welcome(PROTOCOL, support::daemon_features());
    let mut host = h.host(target);
    host.event(NetEvent::State(conn));
    host.send(support::welcome_with_id(PROTOCOL, id));
}

/// A saved session pop-out of `id` on the host whose wire key is `host`.
fn saved_session_popout(id: &str, host: &str) -> serde_json::Value {
    json!({
        "key": { "kind": "session", "session_id": id, "host": host },
        "window": null,
    })
}

#[gpui::test]
fn a_saved_session_popout_on_a_dropped_key_reopens_under_its_owner(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_two_paths(&dir, &json!([saved_session_popout("d1", SSH_KEY)]));
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("a1").build()));
    welcome_as(&mut h, &saved_remote(), DAEMON);
    welcome_as(&mut h, &ssh_box(), DAEMON);
    assert_eq!(h.window_count(), 1, "no host has listed d1 yet");

    remote_lists(&mut h, &["d1"]);

    assert_eq!(
        h.window_count(),
        2,
        "the pop-out reopens on the host that owns the dropped key's daemon"
    );
    h.advance(std::time::Duration::from_millis(500));
    assert_eq!(
        saved_ui(&dir)["popouts"],
        json!([saved_session_popout("d1", REMOTE)]),
        "the saved entry names the owner"
    );
}

#[gpui::test]
fn an_open_session_popout_follows_its_session_to_the_new_owner(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_two_paths(&dir, &json!([]));
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("a1").build()));
    welcome_as(&mut h, &ssh_box(), DAEMON);
    h.host(&ssh_box()).send(DaemonMessage::Sessions {
        sessions: vec![session("b1").build()],
    });
    let on_ssh = PopoutKey::session(&HostKey::of(&ssh_box()), "b1");
    assert_eq!(
        h.root_update(move |root, window, cx| root.open_popout(on_ssh, window, cx)),
        OpenOutcome::Opened
    );
    assert_eq!(h.window_count(), 2);

    welcome_as(&mut h, &saved_remote(), DAEMON);
    remote_lists(&mut h, &["b1"]);

    assert_eq!(h.window_count(), 2, "the same window is kept");
    let on_tailnet = PopoutKey::session(&HostKey::of(&saved_remote()), "b1");
    let opened = on_tailnet.clone();
    assert_eq!(
        h.root_update(move |root, window, cx| root.open_popout(opened, window, cx)),
        OpenOutcome::Existing(on_tailnet),
        "the pop-out is keyed on the owner"
    );
    assert_eq!(
        h.host(&saved_remote()).scrollback_requests("b1").len(),
        1,
        "its terminal attaches on the owner's connection"
    );
}
