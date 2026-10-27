//! Pop-out window specs: a pane's terminal opened in a second window over
//! the same connection takes keys there, and closing the window docks the
//! pane back into its grid.

use crate::support;

use gpui::{
    App, EntityId, InputHandler, Modifiers, MouseButton, MouseDownEvent, Pixels, Point,
    TestAppContext, VisualTestContext, Window, point, px, size,
};
use protocol::{AttentionReason, ClientMessage, DaemonMessage, SplitDirection, TabEntry};
use rustling_tulip_native::{
    OpenOutcome, POPPED_OUT_NOTE, PopoutKey, RootView, ToastKind, ToastSpec,
};
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
        root.open_session_menu("s1", Some(&pane), point(px(40.0), px(12.0)), window, cx);
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
        root.open_session_menu("s1", Some(&pane), point(px(40.0), px(12.0)), window, cx);
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
