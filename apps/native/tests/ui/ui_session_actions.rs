//! Session action specs: the context menu on sidebar leaves and pane
//! headers, the pane header's two-step Stop and exit code, the stopped-pane
//! overlay, and a restart placed only by its own reply.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::path::PathBuf;

use gpui::{Modifiers, TestAppContext, px};
use protocol::{
    CleanupAction, ClientMessage, DaemonMessage, PaneDropEdge, SessionSnapshot, SplitDirection,
};
use rustling_tulip_native::RootView;
use support::{Fixture, Harness, Opened, TestDir, pane, repo, session, split, tab};

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

fn dialog_of(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.delete_dialog_session().map(str::to_owned))
}

/// Whether `sent` is the delete-worktree confirm's preview request for
/// `id` and nothing else: no discard goes out on the click.
fn only_preview(sent: &[ClientMessage], id: &str) -> bool {
    matches!(sent, [ClientMessage::PreviewDiscard { session_id }] if session_id == id)
}

#[gpui::test]
fn remove_and_delete_worktree_opens_the_confirm(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").worktree("r1").exited(1).build());

    h.click_on("exited-remove-pane-delete-p1");
    let sent = h.sent();
    assert!(
        only_preview(&sent, "s1"),
        "the click only asks: sent {sent:?}"
    );
    assert_eq!(dialog_of(&mut h).as_deref(), Some("s1"));
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
    h.click_on("action-failed-dismiss");

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
    assert!(painted(&mut h, "abandoned-p1"), "its own overlay covers it");
    assert_eq!(
        notice_of(&mut h, "p1"),
        ["Session abandoned during daemon restart."],
        "no prompt line without a prompt"
    );

    h.right_click_on("pane-header-p1");
    assert_eq!(rows(&mut h), ["menu-resume-abandoned", "menu-dismiss"]);
    h.click_on("menu-resume-abandoned");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ResumeAbandoned { session_id }] if session_id == "s1"),
        "sent {sent:?}"
    );
}

fn notice_of(h: &mut Harness<'_>, pane_id: &str) -> Vec<String> {
    let pane_id = pane_id.to_owned();
    h.root(move |root, _| root.pane_notice(&pane_id))
}

#[gpui::test]
fn abandoned_pane_shows_overlay_with_last_prompt(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(
        cx,
        &dir,
        session("s1")
            .abandoned()
            .last_prompt("fix the build")
            .build(),
    );

    assert!(painted(&mut h, "abandoned-p1"));
    assert_eq!(
        notice_of(&mut h, "p1"),
        [
            "Session abandoned during daemon restart.",
            "Last prompt: fix the build",
        ]
    );
    assert!(painted(&mut h, "abandoned-resume-p1"));
    assert!(painted(&mut h, "abandoned-dismiss-p1"));
    h.click_on("abandoned-dismiss-p1");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::DiscardAbandoned { session_id }] if session_id == "s1"),
        "sent {sent:?}"
    );
}

#[gpui::test]
fn abandoned_overlay_resume_sends_resume_abandoned(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").abandoned().build());

    h.click_on("abandoned-resume-p1");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ResumeAbandoned { session_id }] if session_id == "s1"),
        "sent {sent:?}"
    );
}

#[gpui::test]
fn orphan_pane_shows_banner_naming_runtime(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").orphan().agent("codex").build());

    assert!(painted(&mut h, "orphan-banner-p1"));
    assert_eq!(
        notice_of(&mut h, "p1"),
        [
            "PTY stream lost across daemon restart. The underlying codex process is still running, \
             but live input/output is not available. Use Stop to kill the recorded PID and clean up, \
             then spawn a new session."
        ]
    );
    assert!(!h.in_model("abandoned-p1"), "an orphan is not abandoned");
}

/// The discards among the messages sent since the last look.
fn discards(h: &mut Harness<'_>) -> Vec<ClientMessage> {
    h.sent()
        .into_iter()
        .filter(|m| matches!(m, ClientMessage::DiscardSession { .. }))
        .collect()
}

#[gpui::test]
fn worktreeless_session_that_exits_by_itself_is_discarded(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").status("working").build());

    h.send(updated(session("s1").exited(0).build(), None));

    let sent = discards(&mut h);
    assert!(
        matches!(sent.as_slice(), [m] if is_discard(m, "s1", &[])),
        "sent {sent:?}"
    );
    h.send(updated(session("s1").exited(0).build(), None));
    assert!(
        discards(&mut h).is_empty(),
        "a repeat snapshot is no new exit"
    );
}

#[gpui::test]
fn exit_update_between_welcome_and_sessions_list_is_not_discarded(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").status("working").build());

    h.send(DaemonMessage::Welcome {
        protocol_version: 1,
        supported_versions: vec![1],
    });
    h.send(updated(session("s1").exited(0).build(), None));

    assert!(
        discards(&mut h).is_empty(),
        "the working status is from before the reconnect"
    );
}

#[gpui::test]
fn stopped_by_user_is_not_auto_discarded(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").status("working").build());

    h.send(updated(session("s1").status("stopped").build(), None));
    h.send(updated(session("s1").exited(1).build(), None));

    assert!(discards(&mut h).is_empty());
}

#[gpui::test]
fn worktree_session_that_exits_is_kept(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").worktree("r1").build());

    h.send(updated(
        session("s1").worktree("r1").exited(0).build(),
        None,
    ));

    assert!(discards(&mut h).is_empty());
}

#[gpui::test]
fn headless_session_that_finishes_is_kept(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").headless().status("working").build());

    h.send(updated(session("s1").headless().exited(0).build(), None));

    assert!(discards(&mut h).is_empty(), "its stats stay to be read");
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

fn groups(h: &mut Harness<'_>) -> Vec<Vec<String>> {
    h.root(|root, _| root.menu_groups())
}

fn diff_tab(id: &str) -> protocol::TabEntry {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "name": "a.rs",
        "content": { "kind": "diff", "repo_id": "r1", "path": "a.rs", "against": null },
        "created_at": "2026-01-01T00:00:00Z",
    }))
    .expect("diff tab fixture")
}

/// Opens `leaf`'s menu and its Duplicate submenu.
fn open_duplicate(h: &mut Harness<'_>, leaf: &str) {
    h.right_click_on(leaf);
    h.click_on("session-menu-duplicate");
}

const APPEARANCE_GROUP: [&str; 2] = ["session-menu-appearance", "session-menu-accent"];

#[gpui::test]
fn duplicate_new_tab_sends_duplicate_then_create_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").build());

    open_duplicate(&mut h, "leaf-s1");
    h.click_on("duplicate-new-tab");
    let id = duplicate_request(&h.sent(), "s1");
    assert_eq!(menu_of(&mut h), None, "a pick closes the menu");

    h.send(updated(session("s2").build(), Some(&id)));
    let placed = placements(h.sent());
    assert!(
        matches!(placed.as_slice(), [
            ClientMessage::CreateTab { name: None, initial_session_id: Some(new) },
        ] if new == "s2"),
        "sent {placed:?}"
    );
}

/// `s1` in repo `in_repo` of two registered repos, alone in pane `p1` of
/// tab `t1`, beside tab `t2` with an empty pane; nothing sent yet.
fn two_repos<'a>(cx: &'a mut TestAppContext, dir: &TestDir, in_repo: &str) -> Harness<'a> {
    let fixture = Fixture {
        repos: vec![repo("r1", "C:/r1"), repo("r2", "C:/r2")],
        sessions: vec![session("s1").in_repo(in_repo).build()],
        tabs: vec![
            tab("t1", &pane("p1", Some("s1"))),
            tab("t2", &pane("p2", None)),
        ],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, dir, &fixture);
    h.sent();
    h
}

/// Shift-clicks the Duplicate choice tagged `selector` in `s1`'s menu.
fn shift_duplicate(h: &mut Harness<'_>, selector: &str) {
    open_duplicate(h, "leaf-s1");
    let at = h.center(selector);
    h.click(
        at,
        Modifiers {
            shift: true,
            ..Modifiers::default()
        },
    );
}

fn config_reply(session_id: &str, config: Option<serde_json::Value>) -> DaemonMessage {
    DaemonMessage::SpawnConfigReply {
        session_id: session_id.to_owned(),
        config: config.map(|value| serde_json::from_value(value).expect("spawn config fixture")),
    }
}

fn dialog_selected(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.spawn_dialog_selected())
}

#[gpui::test]
fn shift_duplicate_asks_for_the_spawn_config_and_sends_no_duplicate(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_repos(cx, &dir, "r1");

    shift_duplicate(&mut h, "duplicate-new-tab");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::GetSpawnConfig { session_id }] if session_id == "s1"),
        "sent {sent:?}"
    );
    assert_eq!(menu_of(&mut h), None, "a pick closes the menu");
    assert!(
        !h.root(|root, _| root.spawn_dialog_open()),
        "the dialog waits for the reply"
    );
}

#[gpui::test]
fn shift_duplicate_reply_opens_the_locked_prefilled_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_repos(cx, &dir, "r1");
    shift_duplicate(&mut h, "duplicate-tab-t2");
    h.sent();

    h.send(config_reply("s9", None));
    assert!(
        !h.root(|root, _| root.spawn_dialog_open()),
        "another session's reply opens nothing"
    );
    h.send(config_reply(
        "s1",
        Some(serde_json::json!({
            "target": { "kind": "single", "repo_id": "r1", "branch_name": "feat/dup",
                "base_branch": "origin/dev", "use_worktree": true },
            "mode": "interactive",
            "dangerously_skip_permissions": false,
            "agent_options": { "kind": "claude", "permission_mode": "plan" },
            "model": "claude-sonnet-9",
            "extra_env": [["LEVEL", "3"]],
        })),
    ));
    assert!(h.root(|root, _| root.spawn_dialog_open()));
    assert_eq!(
        h.root(RootView::spawn_dialog_model).as_deref(),
        Some("claude-sonnet-9")
    );
    assert_ne!(
        h.root(RootView::spawn_dialog_branch).as_deref(),
        Some("feat/dup"),
        "a duplicate's worktree runs on a fresh branch"
    );
    assert_eq!(
        h.root(RootView::spawn_dialog_base).as_deref(),
        Some("origin/dev")
    );
    assert_eq!(
        h.root(RootView::spawn_dialog_env),
        Some(vec![("LEVEL".to_owned(), "3".to_owned())])
    );
    let selected = dialog_selected(&mut h);
    for chosen in [
        "spawn-target-repo-r1",
        "spawn-placement-tab-t2",
        "spawn-approval-plan",
    ] {
        assert!(
            selected.contains(&chosen.to_owned()),
            "{chosen} in {selected:?}"
        );
    }
    assert!(
        !selected.contains(&"spawn-skip-perms".to_owned()),
        "the source was not trusted: {selected:?}"
    );
    assert!(painted(&mut h, "spawn-target-repo-r1"));
    assert!(
        !painted(&mut h, "spawn-target-repo-r2"),
        "the locked target shows alone"
    );
}

#[gpui::test]
fn shift_duplicate_with_no_stored_config_opens_defaults_on_the_source_repo(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = two_repos(cx, &dir, "r2");
    shift_duplicate(&mut h, "duplicate-new-tab");
    h.sent();

    h.send(config_reply("s1", None));
    assert!(h.root(|root, _| root.spawn_dialog_open()));
    let selected = dialog_selected(&mut h);
    for chosen in ["spawn-target-repo-r2", "spawn-placement-new-tab"] {
        assert!(
            selected.contains(&chosen.to_owned()),
            "{chosen} in {selected:?}"
        );
    }
    assert_eq!(h.root(RootView::spawn_dialog_model).as_deref(), Some(""));
    assert!(painted(&mut h, "spawn-target-repo-r2"));
    assert!(
        !painted(&mut h, "spawn-target-repo-r1"),
        "the dialog is held on the source's repo"
    );
}

#[gpui::test]
fn shift_duplicate_on_a_removed_repo_says_why(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_repos(cx, &dir, "r1");
    shift_duplicate(&mut h, "duplicate-new-tab");
    h.sent();

    h.send(DaemonMessage::Repos {
        repos: vec![repo("r2", "C:/r2")],
    });
    h.send(config_reply("s1", None));
    assert!(!h.root(|root, _| root.spawn_dialog_open()));
    let notice = h.root(|root, _| {
        root.action_failed()
            .map(|n| (n.title.clone(), n.detail.clone()))
    });
    assert_eq!(
        notice,
        Some((
            "Duplicate failed".to_owned(),
            "Couldn't open the spawn dialog: the repo is no longer registered".to_owned()
        ))
    );
}

#[gpui::test]
fn shift_duplicate_of_a_standalone_session_duplicates_it_plainly(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").shell("C:/tmp").build());

    shift_duplicate(&mut h, "duplicate-new-tab");
    let id = duplicate_request(&h.sent(), "s1");
    assert!(!id.is_empty());
    assert!(!h.root(|root, _| root.spawn_dialog_open()));
}

#[gpui::test]
fn duplicate_into_tab_places_the_copy_and_switches_to_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build()],
        tabs: vec![
            tab("t1", &pane("p1", Some("s1"))),
            tab("t2", &pane("p2", None)),
        ],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("t1".to_owned())
    );

    open_duplicate(&mut h, "leaf-s1");
    h.click_on("duplicate-tab-t2");
    let id = duplicate_request(&h.sent(), "s1");
    h.send(updated(session("s2").build(), Some(&id)));
    let placed = placements(h.sent());
    assert!(
        matches!(placed.as_slice(), [
            ClientMessage::ReplacePaneSession { tab_id, pane_id, session_id: Some(new) },
        ] if tab_id == "t2" && pane_id == "p2" && new == "s2"),
        "the copy takes t2's empty pane: sent {placed:?}"
    );
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        Some("t2".to_owned()),
        "its tab comes to the front"
    );
    assert_eq!(h.root(|root, _| root.focused_pane()), Some("p2".to_owned()));
}

#[gpui::test]
fn duplicate_lists_new_tab_then_grid_tabs_only(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build()],
        tabs: vec![
            tab("t1", &pane("p1", Some("s1"))),
            diff_tab("d1"),
            tab("t2", &pane("p2", None)),
        ],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();

    open_duplicate(&mut h, "leaf-s1");
    assert_eq!(
        groups(&mut h),
        [
            vec!["duplicate-back", "duplicate-new-tab"],
            vec!["duplicate-tab-t1", "duplicate-tab-t2"],
        ],
        "every grid tab, the one showing the session too; no diff tab"
    );
    assert!(
        rows(&mut h).is_empty(),
        "the submenu replaces the state actions"
    );
    assert!(
        h.root(|root, _| root.accent_menu_rows()).is_empty(),
        "and the appearance rows"
    );
    assert!(h.sent().is_empty(), "opening the submenu sends nothing");
}

#[gpui::test]
fn duplicate_back_returns_to_actions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").build());

    open_duplicate(&mut h, "leaf-s1");
    h.click_on("duplicate-back");
    assert_eq!(
        menu_of(&mut h).as_deref(),
        Some("s1"),
        "the menu stays open"
    );
    assert_eq!(
        groups(&mut h),
        [
            vec!["menu-rename", "menu-stop"],
            vec!["session-menu-duplicate", "session-menu-move"],
            APPEARANCE_GROUP.to_vec(),
        ]
    );
    assert!(h.sent().is_empty());
}

#[gpui::test]
fn duplicate_does_not_discard_the_original(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").exited(0).build());

    h.right_click_on("pane-header-p1");
    h.click_on("session-menu-duplicate");
    h.click_on("duplicate-tab-t1");
    let id = duplicate_request(&h.sent(), "s1");
    assert!(
        !pending(&mut h, "s1"),
        "a copy is no restart: Restart stays offered"
    );
    h.send(updated(session("s2").build(), Some(&id)));
    let sent = h.sent();
    assert!(
        placements(sent.clone()).is_empty(),
        "the original keeps its pane and is not discarded: sent {sent:?}"
    );
    assert!(
        sent.iter().any(|m| matches!(m,
            ClientMessage::SplitPane { tab_id, pane_id, new_session_id: Some(new), .. }
                if tab_id == "t1" && pane_id == "p1" && new == "s2")),
        "the copy splits in beside the original: sent {sent:?}"
    );
}

#[gpui::test]
fn duplicate_of_headless_session_is_disabled(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").headless().build());

    h.right_click_on("leaf-s1");
    assert!(
        groups(&mut h)
            .concat()
            .contains(&"session-menu-duplicate".to_owned()),
        "offered, dimmed: {:?}",
        groups(&mut h)
    );
    assert_eq!(
        h.root(|root, _| root.menu_disabled_tip("session-menu-duplicate")),
        Some("Headless sessions are one-shot kickoffs; spawn a new one instead")
    );
    let before = groups(&mut h);
    h.click_on("session-menu-duplicate");
    assert_eq!(groups(&mut h), before, "the click opens no submenu");
    assert!(h.sent().is_empty());
}

#[gpui::test]
fn new_rows_sit_between_state_actions_and_appearance(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = unplaced(
        cx,
        &dir,
        vec![session("s1").build(), session("s2").exited(0).build()],
    );

    h.right_click_on("leaf-s1");
    assert_eq!(
        groups(&mut h),
        [
            vec!["menu-rename", "menu-stop"],
            ADD_GROUP.to_vec(),
            APPEARANCE_GROUP.to_vec(),
        ]
    );
    assert_eq!(
        h.root(|root, _| root.menu_disabled_tip("session-menu-duplicate")),
        None
    );
    let top = |h: &mut Harness<'_>, selector: &str| h.bounds(selector).origin.y;
    let (stop, duplicate, appearance) = (
        top(&mut h, "menu-stop"),
        top(&mut h, "session-menu-duplicate"),
        top(&mut h, "session-menu-appearance"),
    );
    assert!(
        stop < duplicate && duplicate < appearance,
        "drawn in that order: {stop:?} {duplicate:?} {appearance:?}"
    );

    h.keys("escape");
    h.right_click_on("leaf-s2");
    assert_eq!(
        groups(&mut h),
        [
            vec!["menu-restart", "menu-remove-pane"],
            ADD_GROUP.to_vec(),
            APPEARANCE_GROUP.to_vec(),
        ],
        "a stopped session offers Duplicate too"
    );
}

const ADD_GROUP: [&str; 3] = [
    "session-menu-duplicate",
    "session-menu-add-current",
    "session-menu-add-new",
];

fn active_tab(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.active_tab_id().map(str::to_owned))
}

fn row_label(h: &mut Harness<'_>, selector: &str) -> Option<&'static str> {
    let selector = selector.to_owned();
    h.root(move |root, _| root.menu_row_label(&selector))
}

/// The daemon's answer to a new tab: tab `t9` showing `session_id`.
fn new_tab_arrives(h: &mut Harness<'_>, session_id: &str) {
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t9", &pane("p9", Some(session_id))),
    });
}

fn undo_messages(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| {
        root.undo_entries()
            .iter()
            .map(|entry| entry.message.clone())
            .collect()
    })
}

#[gpui::test]
fn move_to_new_tab_sends_extract_and_activates_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").build());

    h.right_click_on("leaf-s1");
    h.click_on("session-menu-move");
    assert_eq!(
        groups(&mut h),
        [vec!["move-back", "move-new-tab"]],
        "no other tab to list"
    );
    assert!(h.sent().is_empty(), "opening the submenu sends nothing");
    h.click_on("move-new-tab");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ExtractToNewTab { source_tab_id, pane_ids, name: None, layout: None }]
            if source_tab_id == "t1" && pane_ids == &["p1"]),
        "sent {sent:?}"
    );
    assert_eq!(menu_of(&mut h), None, "a pick closes the menu");
    assert!(undo_messages(&mut h).is_empty(), "a new tab has no undo");
    new_tab_arrives(&mut h, "s1");
    assert_eq!(
        active_tab(&mut h).as_deref(),
        Some("t9"),
        "the new tab shows"
    );
}

#[gpui::test]
fn move_to_tab_sends_move_pane_to_drop_target_with_undo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build(), session("s2").build()],
        tabs: vec![
            tab("t1", &pane("p1", Some("s1"))),
            tab("t2", &pane("p2", Some("s2"))),
            diff_tab("d1"),
            tab(
                "t3",
                &split(
                    SplitDirection::Vertical,
                    pane("p3", Some("s2")),
                    pane("p4", None),
                ),
            ),
            tab("t4", &pane("p5", Some("s1"))),
        ],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();

    h.right_click_on("leaf-s1");
    h.click_on("session-menu-move");
    assert_eq!(
        groups(&mut h),
        [
            vec!["move-back", "move-new-tab"],
            vec!["move-tab-t2", "move-tab-t3"],
        ],
        "grid tabs not showing s1; no diff tab"
    );
    h.click_on("move-tab-t3");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::MovePane { src_tab_id, src_pane_id, dst_tab_id, dst_pane_id, edge: PaneDropEdge::Replace }]
            if src_tab_id == "t1" && src_pane_id == "p1" && dst_tab_id == "t3" && dst_pane_id == "p4"),
        "the first binding goes to t3's empty pane: sent {sent:?}"
    );
    assert_eq!(undo_messages(&mut h), ["Moved pane"]);
    assert_eq!(menu_of(&mut h), None, "a pick closes the menu");
}

#[gpui::test]
fn move_from_a_pane_header_moves_that_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build()],
        tabs: vec![
            tab("t1", &pane("p1", Some("s1"))),
            tab("t2", &pane("p2", Some("s1"))),
            tab("t3", &pane("p3", None)),
        ],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.click_on("tab-t2");
    h.sent();

    h.right_click_on("pane-header-p2");
    h.click_on("session-menu-move");
    assert_eq!(
        groups(&mut h),
        [vec!["move-back", "move-new-tab"], vec!["move-tab-t3"]]
    );
    h.click_on("move-tab-t3");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::MovePane { src_tab_id, src_pane_id, dst_tab_id, .. }]
            if src_tab_id == "t2" && src_pane_id == "p2" && dst_tab_id == "t3"),
        "the right-clicked pane moves, not the first binding: sent {sent:?}"
    );

    h.right_click_on("leaf-s1");
    h.click_on("session-menu-move");
    h.click_on("move-new-tab");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ExtractToNewTab { source_tab_id, pane_ids, .. }]
            if source_tab_id == "t1" && pane_ids == &["p1"]),
        "from the sidebar the first binding moves: sent {sent:?}"
    );
}

#[gpui::test]
fn unbound_session_add_to_current_places_it_in_the_active_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = unplaced(cx, &dir, vec![session("s1").build()]);

    h.right_click_on("leaf-s1");
    assert_eq!(
        row_label(&mut h, "session-menu-add-current"),
        Some("Add to current tab")
    );
    h.click_on("session-menu-add-current");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ReplacePaneSession { tab_id, pane_id, session_id: Some(id) }]
            if tab_id == "t1" && pane_id == "p1" && id == "s1"),
        "the active tab's empty pane takes it: sent {sent:?}"
    );
    assert_eq!(menu_of(&mut h), None, "a pick closes the menu");
    assert_eq!(h.root(|root, _| root.focused_pane()), Some("p1".to_owned()));
}

#[gpui::test]
fn add_to_current_with_diff_tab_active_reads_add_to_new_tab_and_opens_one(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build()],
        tabs: vec![tab("t1", &pane("p1", None)), diff_tab("d1")],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.click_on("tab-d1");
    h.sent();

    h.right_click_on("leaf-s1");
    assert_eq!(
        row_label(&mut h, "session-menu-add-current"),
        Some("Add to new tab")
    );
    assert!(
        !h.in_model("session-menu-add-new"),
        "one Add to new tab row, not two: {:?}",
        groups(&mut h)
    );
    h.click_on("session-menu-add-current");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::CreateTab { name: None, initial_session_id: Some(id) }]
            if id == "s1"),
        "sent {sent:?}"
    );
    new_tab_arrives(&mut h, "s1");
    assert_eq!(active_tab(&mut h).as_deref(), Some("t9"));
}

#[gpui::test]
fn add_to_new_tab_sends_create_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = unplaced(cx, &dir, vec![session("s1").build()]);

    h.right_click_on("leaf-s1");
    assert_eq!(
        row_label(&mut h, "session-menu-add-new"),
        Some("Add to new tab")
    );
    h.click_on("session-menu-add-new");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::CreateTab { name: None, initial_session_id: Some(id) }]
            if id == "s1"),
        "the empty pane is left alone: sent {sent:?}"
    );
    assert_eq!(menu_of(&mut h), None, "a pick closes the menu");
    new_tab_arrives(&mut h, "s1");
    assert_eq!(active_tab(&mut h).as_deref(), Some("t9"));
}

#[gpui::test]
fn reveal_worktree_opens_the_worktree_folder(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = single(cx, &dir, session("s1").worktree("r1").build());

    h.right_click_on("leaf-s1");
    assert_eq!(
        row_label(&mut h, "session-menu-reveal"),
        Some("Reveal worktree")
    );
    h.click_on("session-menu-reveal");
    assert_eq!(h.opened(), [Opened::DefaultApp(PathBuf::from("C:/wt/x"))]);
    assert_eq!(menu_of(&mut h), None, "a pick closes the menu");
    assert!(h.sent().is_empty(), "the daemon is not asked");
}

#[gpui::test]
fn reveal_hidden_without_own_worktree(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![
            session("s1").build(),
            session("s2")
                .members(&[("r1", "main", "C:/src/r1")])
                .build(),
        ],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", Some("s2")),
            ),
        )],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();

    for leaf in ["leaf-s1", "leaf-s2"] {
        h.right_click_on(leaf);
        let shown = groups(&mut h).concat();
        assert!(
            shown.contains(&"session-menu-move".to_owned())
                && !shown.contains(&"session-menu-reveal".to_owned()),
            "{leaf}: {shown:?}"
        );
        h.keys("escape");
    }
}

#[gpui::test]
fn menu_groups_order_is_state_then_location_then_appearance(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![
            session("s1").worktree("r1").build(),
            session("s2").worktree("r1").build(),
        ],
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();

    h.right_click_on("leaf-s1");
    let located = [
        "session-menu-duplicate",
        "session-menu-move",
        "session-menu-reveal",
    ];
    assert_eq!(
        groups(&mut h),
        [
            vec!["menu-rename", "menu-stop"],
            located.to_vec(),
            APPEARANCE_GROUP.to_vec(),
        ]
    );
    let order = [
        "menu-stop",
        "session-menu-duplicate",
        "session-menu-move",
        "session-menu-reveal",
        "session-menu-appearance",
    ];
    let tops: Vec<_> = order
        .iter()
        .map(|selector| h.bounds(selector).origin.y)
        .collect();
    assert!(
        tops.windows(2).all(|pair| pair[0] < pair[1]),
        "drawn in {order:?} order: {tops:?}"
    );

    h.keys("escape");
    h.right_click_on("leaf-s2");
    assert_eq!(
        groups(&mut h),
        [
            vec!["menu-rename", "menu-stop"],
            vec![
                "session-menu-duplicate",
                "session-menu-add-current",
                "session-menu-add-new",
                "session-menu-reveal",
            ],
            APPEARANCE_GROUP.to_vec(),
        ],
        "an unbound session offers the add rows in Move to's place"
    );
}

#[gpui::test]
fn duplicate_into_tab_split_brings_the_tab_forward_and_focuses_the_copy(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build(), session("s9").build()],
        tabs: vec![
            tab("t1", &pane("p1", Some("s1"))),
            tab("t2", &pane("p2", Some("s9"))),
        ],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();
    assert_eq!(active_tab(&mut h).as_deref(), Some("t1"));

    open_duplicate(&mut h, "leaf-s1");
    h.click_on("duplicate-tab-t2");
    let id = duplicate_request(&h.sent(), "s1");
    h.send(updated(session("s2").build(), Some(&id)));
    let sent = h.sent();
    assert!(
        sent.iter().any(|m| matches!(m,
            ClientMessage::SplitPane { tab_id, pane_id, new_session_id: Some(new), .. }
                if tab_id == "t2" && pane_id == "p2" && new == "s2")),
        "the copy splits t2's pane: sent {sent:?}"
    );
    assert_eq!(
        active_tab(&mut h).as_deref(),
        Some("t2"),
        "its tab comes to the front"
    );
    h.send(DaemonMessage::TabUpdated {
        tab: tab(
            "t2",
            &split(
                SplitDirection::Horizontal,
                pane("p2", Some("s9")),
                pane("p6", Some("s2")),
            ),
        ),
    });
    assert_eq!(active_tab(&mut h).as_deref(), Some("t2"));
    assert_eq!(
        h.root(|root, _| root.focused_pane()),
        Some("p6".to_owned()),
        "the copy's new pane takes the focus"
    );
}
