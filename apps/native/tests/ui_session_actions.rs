//! Session action specs: the context menu on sidebar leaves and pane
//! headers, the pane header's two-step Stop and exit code, the stopped-pane
//! overlay, and a restart placed only by its own reply.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use gpui::{Modifiers, TestAppContext, px};
use protocol::{BranchCleanup, CleanupAction, ClientMessage, DaemonMessage, SessionSnapshot};
use support::{Fixture, Harness, TestDir, pane, session, tab};

/// Whether the element tagged `selector` has been painted on screen. gpui
/// keeps the bounds of an element that left the tree, so a `false` only
/// holds for an element the spec never showed.
fn painted(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

fn menu_of(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.session_menu().map(str::to_owned))
}

fn updated(session: SessionSnapshot, request_id: Option<&str>) -> DaemonMessage {
    DaemonMessage::SessionUpdated {
        session,
        request_id: request_id.map(str::to_owned),
    }
}

/// The request id of the one message sent, a duplicate of `id`.
fn duplicate_request(sent: &[ClientMessage], id: &str) -> String {
    match sent {
        [
            ClientMessage::DuplicateSession {
                session_id,
                request_id: Some(request_id),
            },
        ] if session_id == id => request_id.clone(),
        other => panic!("expected one DuplicateSession of {id} with an id, sent {other:?}"),
    }
}

/// The messages that place a session or retire one.
fn placements(sent: Vec<ClientMessage>) -> Vec<ClientMessage> {
    sent.into_iter()
        .filter(|m| {
            matches!(
                m,
                ClientMessage::ReplacePaneSession { .. }
                    | ClientMessage::CreateTab { .. }
                    | ClientMessage::DiscardSession { .. }
            )
        })
        .collect()
}

fn is_stop(msg: &ClientMessage, id: &str) -> bool {
    matches!(msg, ClientMessage::StopSession { session_id, cleanup } if session_id == id && cleanup.is_empty())
}

fn is_discard(msg: &ClientMessage, id: &str, cleanup: &[CleanupAction]) -> bool {
    matches!(msg, ClientMessage::DiscardSession { session_id, cleanup: c } if session_id == id && c == cleanup)
}

/// `s1` alone in pane `p1` of tab `t1`, and nothing sent yet.
fn single<'a>(cx: &'a mut TestAppContext, dir: &TestDir, s1: SessionSnapshot) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &Fixture::single(s1));
    h.sent();
    h
}

/// `sessions` in the sidebar and one tab with an empty pane.
fn unplaced<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    sessions: Vec<SessionSnapshot>,
) -> Harness<'a> {
    let fixture = Fixture {
        sessions,
        tabs: vec![tab("t1", &pane("p1", None))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, dir, &fixture);
    h.sent();
    h
}

#[gpui::test]
fn right_click_leaf_shows_running_actions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").worktree("r1").build());

    h.right_click_on("leaf-s1");
    assert_eq!(menu_of(&mut h).as_deref(), Some("s1"));
    assert!(painted(&mut h, "menu-rename"));
    assert!(painted(&mut h, "menu-stop"));
    assert!(!painted(&mut h, "menu-restart"), "running: no Restart");
    assert!(!painted(&mut h, "menu-resume"), "running: no Resume");
    assert!(
        !painted(&mut h, "menu-stop-keep"),
        "the stop choices wait behind Stop session…"
    );

    h.click_on("menu-stop");
    assert!(painted(&mut h, "menu-stop-keep"));
    assert!(painted(&mut h, "menu-stop-delete"));
    assert!(h.sent().is_empty(), "opening the menu sends nothing");

    h.keys("escape");
    assert_eq!(menu_of(&mut h), None);
    h.right_click_on("pane-header-p1");
    assert_eq!(
        menu_of(&mut h).as_deref(),
        Some("s1"),
        "the pane header opens the same menu"
    );
}

#[gpui::test]
fn rename_from_menu_sends_rename_and_blank_restores_default(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").build());

    h.right_click_on("leaf-s1");
    h.click_on("menu-rename");
    h.cx.simulate_input("work");
    h.keys("enter");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::RenameSession { session_id, label: Some(label) }]
            if session_id == "s1" && label == "work"),
        "sent {sent:?}"
    );
    assert_eq!(menu_of(&mut h), None, "Enter closes the menu");

    h.right_click_on("leaf-s1");
    h.click_on("menu-rename");
    h.keys("backspace enter");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::RenameSession { session_id, label: None }]
            if session_id == "s1"),
        "a blank name restores the default: sent {sent:?}"
    );
}

#[gpui::test]
fn menu_stop_keep_worktree_sends_stop_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").worktree("r1").build());

    h.right_click_on("leaf-s1");
    h.click_on("menu-stop");
    h.click_on("menu-stop-keep");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [stop] if is_stop(stop, "s1")),
        "a pane shows it, so nothing but the stop: sent {sent:?}"
    );
    assert_eq!(menu_of(&mut h), None);
}

#[gpui::test]
fn header_stop_is_two_step(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").build());
    let armed = |h: &mut Harness<'_>| h.root(|root, _| root.armed_stop_pane().map(str::to_owned));

    h.click_on("pane-stop-p1");
    assert!(h.sent().is_empty(), "the first click only asks");
    assert_eq!(armed(&mut h).as_deref(), Some("p1"));
    h.click_on("pane-stop-cancel-p1");
    assert_eq!(armed(&mut h), None);
    assert!(h.sent().is_empty(), "Cancel stops nothing");

    h.click_on("pane-stop-p1");
    h.click_on("pane-stop-confirm-p1");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [stop] if is_stop(stop, "s1")),
        "sent {sent:?}"
    );
    assert_eq!(armed(&mut h), None);
}

#[gpui::test]
fn exited_session_shows_exit_code_and_overlay(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").exited(3).build());
    h.answer_scrollback("s1", b"last words");

    assert!(painted(&mut h, "exit-code-p1"));
    assert!(
        !painted(&mut h, "pane-stop-p1"),
        "an exited session has no Stop"
    );
    assert!(painted(&mut h, "exited-p1"));
    assert!(painted(&mut h, "exited-restart-p1"));
    assert!(painted(&mut h, "exited-remove-pane-p1"));
    assert!(
        !painted(&mut h, "exited-park-p1"),
        "no worktree: nothing to keep"
    );
    assert!(
        painted(&mut h, "pane-grid-p1"),
        "the terminal stays under the overlay"
    );
    assert_eq!(h.grid_text("p1")[0], "last words");
}

#[gpui::test]
fn restart_duplicates_then_replaces_pane_and_discards_old(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").exited(0).build());

    h.click_on("exited-restart-p1");
    let id = duplicate_request(&h.sent(), "s1");

    h.send(updated(session("s2").build(), None));
    h.send(updated(session("s3").build(), Some("someone-else")));
    let early = placements(h.sent());
    assert!(
        early.is_empty(),
        "only the reply to this restart is placed: {early:?}"
    );

    h.send(updated(session("s4").build(), Some(&id)));
    let placed = placements(h.sent());
    assert!(
        matches!(placed.as_slice(), [
            ClientMessage::ReplacePaneSession { tab_id, pane_id, session_id: Some(new) },
            discard,
        ] if tab_id == "t1" && pane_id == "p1" && new == "s4" && is_discard(discard, "s1", &[])),
        "sent {placed:?}"
    );
}

#[gpui::test]
fn overlay_remove_keep_worktree_parks(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").worktree("r1").exited(0).build());

    h.click_on("exited-park-p1");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ParkSession { session_id }] if session_id == "s1"),
        "sent {sent:?}"
    );
}

#[gpui::test]
fn remove_and_delete_worktree_requires_confirm_and_sends_cleanup(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").worktree("r1").exited(1).build());

    h.click_on("exited-remove-pane-delete-p1");
    assert!(h.sent().is_empty(), "the first click only asks");
    h.click_on("exited-remove-pane-delete-p1");
    let cleanup = [CleanupAction {
        repo_id: "r1".to_owned(),
        remove_worktree: true,
        branch: BranchCleanup::Auto,
    }];
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [discard] if is_discard(discard, "s1", &cleanup)),
        "sent {sent:?}"
    );
}

#[gpui::test]
fn inactive_session_menu_offers_resume_and_remove(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = unplaced(
        cx,
        &dir,
        vec![session("s1").worktree("r1").inactive().build()],
    );

    h.right_click_on("leaf-s1");
    assert!(painted(&mut h, "menu-resume"));
    assert!(painted(&mut h, "menu-remove"));
    assert!(painted(&mut h, "menu-remove-delete"));
    assert!(
        !painted(&mut h, "menu-stop"),
        "a parked session has no Stop"
    );

    h.click_on("menu-resume");
    let id = duplicate_request(&h.sent(), "s1");
    h.send(updated(session("s2").build(), Some(&id)));
    let placed = placements(h.sent());
    assert!(
        matches!(placed.as_slice(), [
            ClientMessage::CreateTab { name: None, initial_session_id: Some(new) },
            discard,
        ] if new == "s2" && is_discard(discard, "s1", &[])),
        "no pane showed it, so the resumed session opens a tab: sent {placed:?}"
    );
}

#[gpui::test]
fn stop_session_without_pane_parks_or_discards(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = unplaced(
        cx,
        &dir,
        vec![session("s1").worktree("r1").build(), session("s2").build()],
    );

    h.right_click_on("leaf-s1");
    h.click_on("menu-stop");
    h.click_on("menu-stop-keep");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [stop, ClientMessage::ParkSession { session_id }]
            if is_stop(stop, "s1") && session_id == "s1"),
        "a worktree to keep: stop, then park: sent {sent:?}"
    );

    h.right_click_on("leaf-s2");
    h.click_on("menu-stop");
    h.click_on("menu-stop-keep");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [stop, discard] if is_stop(stop, "s2") && is_discard(discard, "s2", &[])),
        "nothing to keep: stop, then discard: sent {sent:?}"
    );
}

#[gpui::test]
fn esc_and_click_outside_close_menu(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").build());

    h.right_click_on("leaf-s1");
    assert_eq!(menu_of(&mut h).as_deref(), Some("s1"));
    h.keys("escape");
    assert_eq!(menu_of(&mut h), None, "Esc closes the menu");

    h.right_click_on("leaf-s1");
    assert_eq!(menu_of(&mut h).as_deref(), Some("s1"));
    let outside = h.center("new-tab");
    h.click(outside, Modifiers::none());
    assert_eq!(menu_of(&mut h), None, "a click outside closes the menu");
    assert!(
        h.sent().is_empty(),
        "the click that closes the menu reaches nothing under it"
    );
}

fn armed_pane(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.armed_stop_pane().map(str::to_owned))
}

fn rows(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.menu_rows())
}

fn pending(h: &mut Harness<'_>, id: &str) -> bool {
    let id = id.to_owned();
    h.root(move |root, _| root.restart_pending(&id))
}

#[gpui::test]
fn overlay_delete_confirm_does_not_survive_restart(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").worktree("r1").exited(1).build());

    h.click_on("exited-remove-pane-delete-p1");
    h.click_on("exited-restart-p1");
    let id = duplicate_request(&h.sent(), "s1");
    h.send(updated(
        session("s2").worktree("r1").exited(1).build(),
        Some(&id),
    ));
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &pane("p1", Some("s2"))),
    });
    h.sent();

    h.click_on("exited-remove-pane-delete-p1");
    let sent = h.sent();
    assert!(
        sent.is_empty(),
        "s2's worktree delete asks first; nothing armed for s1 carries over: sent {sent:?}"
    );
}

#[gpui::test]
fn header_stop_confirm_does_not_survive_session_change(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build(), session("s2").build()],
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();

    h.click_on("pane-stop-p1");
    assert_eq!(armed_pane(&mut h).as_deref(), Some("p1"));
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &pane("p1", Some("s2"))),
    });
    assert_eq!(
        armed_pane(&mut h),
        None,
        "p1 now shows s2, which nobody armed"
    );
    h.click_on("pane-stop-p1");
    let sent = h.sent();
    assert!(
        !sent
            .iter()
            .any(|m| matches!(m, ClientMessage::StopSession { .. })),
        "one click never stops s2: sent {sent:?}"
    );
}

#[gpui::test]
fn armed_confirm_disarms_on_outside_click_and_esc(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").build());

    h.click_on("pane-stop-p1");
    assert_eq!(armed_pane(&mut h).as_deref(), Some("p1"));
    h.click_on("tab-t1");
    assert_eq!(armed_pane(&mut h), None, "a press elsewhere disarms");

    h.click_on("pane-stop-p1");
    assert_eq!(armed_pane(&mut h).as_deref(), Some("p1"));
    h.keys("escape");
    assert_eq!(armed_pane(&mut h), None, "Esc disarms");
    assert!(
        !h.sent()
            .iter()
            .any(|m| matches!(m, ClientMessage::StopSession { .. })),
        "nothing stopped"
    );
}

#[gpui::test]
fn second_restart_click_while_pending_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").exited(0).build());

    h.click_on("exited-restart-p1");
    duplicate_request(&h.sent(), "s1");
    assert!(pending(&mut h, "s1"));
    h.click_on("exited-restart-p1");
    let sent = h.sent();
    assert!(
        sent.is_empty(),
        "one restart in flight at a time: sent {sent:?}"
    );
}

#[gpui::test]
fn failed_restart_clears_pending_and_allows_retry(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").exited(0).build());

    h.click_on("exited-restart-p1");
    let id = duplicate_request(&h.sent(), "s1");
    h.send(DaemonMessage::ActionFailed {
        title: "Couldn't start session".to_owned(),
        detail: "boom".to_owned(),
        hint: None,
        request_id: Some(id),
    });
    assert!(!pending(&mut h, "s1"), "the failure answers the restart");

    h.click_on("exited-restart-p1");
    let id = duplicate_request(&h.sent(), "s1");
    h.send(DaemonMessage::Error {
        message: "unknown session: s1".to_owned(),
        request_id: Some(id),
    });
    assert!(!pending(&mut h, "s1"), "an Error answers it too");

    h.click_on("exited-restart-p1");
    duplicate_request(&h.sent(), "s1");
    h.send(DaemonMessage::Welcome {
        protocol_version: 1,
        supported_versions: vec![1],
    });
    assert!(!pending(&mut h, "s1"), "a reconnect forgets every restart");
}

#[gpui::test]
fn abandoned_session_pane_has_no_exit_overlay(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").abandoned().build());

    assert!(!painted(&mut h, "exited-p1"), "abandoned is not exited");
    assert!(!painted(&mut h, "exited-restart-p1"));

    h.right_click_on("pane-header-p1");
    assert_eq!(rows(&mut h), ["menu-resume-abandoned", "menu-dismiss"]);
    h.click_on("menu-resume-abandoned");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ResumeAbandoned { session_id }] if session_id == "s1"),
        "sent {sent:?}"
    );
}

#[gpui::test]
fn disconnect_closes_menu(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").build());

    h.right_click_on("leaf-s1");
    assert_eq!(menu_of(&mut h).as_deref(), Some("s1"));
    h.lose_connection();
    assert_eq!(menu_of(&mut h), None);
}

#[gpui::test]
fn esc_in_rename_returns_to_rows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").build());

    h.right_click_on("leaf-s1");
    h.click_on("menu-rename");
    assert_eq!(rows(&mut h), ["menu-rename-input"]);
    h.keys("escape");
    assert_eq!(
        menu_of(&mut h).as_deref(),
        Some("s1"),
        "the menu stays open"
    );
    assert_eq!(rows(&mut h), ["menu-rename", "menu-stop"]);
    assert!(h.sent().is_empty());
}

#[gpui::test]
fn menu_of_removed_session_closes_and_refocuses_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build(), session("s2").build()],
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.sent();

    h.right_click_on("leaf-s2");
    h.send(DaemonMessage::SessionRemoved {
        session_id: "s2".to_owned(),
    });
    assert_eq!(menu_of(&mut h), None);
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "the pane has the keyboard again");
}

#[gpui::test]
fn stop_choice_rederives_rows_when_session_stops(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").build());

    h.right_click_on("leaf-s1");
    h.click_on("menu-stop");
    h.send(updated(session("s1").exited(0).build(), None));
    assert_eq!(rows(&mut h), ["menu-restart", "menu-remove-pane"]);
}

#[gpui::test]
fn unplaced_stopped_session_menu_says_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = unplaced(
        cx,
        &dir,
        vec![session("s1").worktree("r1").exited(0).build()],
    );

    h.right_click_on("leaf-s1");
    let labels = h.root(|root, _| root.menu_labels());
    assert_eq!(
        labels,
        [
            "Restart",
            "Remove session, keep worktree",
            "Remove session and delete worktree"
        ]
    );
}

#[gpui::test]
fn menu_delete_worktree_is_two_step(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").worktree("r1").build());

    h.right_click_on("leaf-s1");
    h.click_on("menu-stop");
    h.click_on("menu-stop-delete");
    assert!(h.sent().is_empty(), "the first click only asks");
    assert_eq!(menu_of(&mut h).as_deref(), Some("s1"));
    h.click_on("menu-stop-delete");
    let cleanup = [CleanupAction {
        repo_id: "r1".to_owned(),
        remove_worktree: true,
        branch: BranchCleanup::Auto,
    }];
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [stop, discard] if is_stop(stop, "s1") && is_discard(discard, "s1", &cleanup)),
        "sent {sent:?}"
    );
}

#[gpui::test]
fn restart_focuses_the_replaced_pane_in_its_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").exited(0).build(), session("s9").build()],
        tabs: vec![
            tab("t1", &pane("p1", Some("s1"))),
            tab("t2", &pane("p2", Some("s9"))),
        ],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.click_on("tab-t2");
    h.sent();

    h.right_click_on("leaf-s1");
    h.click_on("menu-restart");
    let id = duplicate_request(&h.sent(), "s1");
    h.send(updated(session("s4").build(), Some(&id)));
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("t1".to_owned())
    );
    assert_eq!(h.root(|root, _| root.focused_pane()), Some("p1".to_owned()));
}
