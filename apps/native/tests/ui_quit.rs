//! Quit-flow specs: closing the main window quits at once when the daemon
//! has nothing to stop, and otherwise asks through the exit dialog whether to
//! keep the sessions running, stop them (keeping or removing their
//! worktrees, one branch-fate confirm per worktree) or abandon them, then
//! waits for the daemon before quitting.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gpui::TestAppContext;
use protocol::{
    BranchCleanup, BranchFate, CleanupAction, ClientMessage, DaemonHandshake, DaemonMessage,
    MemberBranchFate, SessionSnapshot,
};
use rustling_tulip_native::{Connection, NetCommand, NetDeps, NetEvent};
use serde_json::Value;
use support::{Fixture, Harness, TestDir, pane, repo, session, tab};
use tokio_tungstenite::tungstenite::{self, Message};

const TITLE: &str = "Quit rustling-tulip?";
const BODY: &str = "The background daemon owns your claude sessions. By default it keeps running \
                    after the app closes so sessions survive app restarts.";

fn exit_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.exit_dialog_open())
}

fn exit_text(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.exit_dialog_text())
}

fn exit_buttons(h: &mut Harness<'_>) -> Vec<(String, String, bool)> {
    h.root(|root, _| root.exit_dialog_buttons())
}

fn exit_focus(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.exit_dialog_focus())
}

fn buttons(expected: &[(&str, &str, bool)]) -> Vec<(String, String, bool)> {
    expected
        .iter()
        .map(|(s, l, e)| ((*s).to_owned(), (*l).to_owned(), *e))
        .collect()
}

fn lines(expected: &[&str]) -> Vec<String> {
    expected.iter().map(|line| (*line).to_owned()).collect()
}

/// Asks the window to close, as its X button or Alt+F4 does; returns whether
/// it may close.
fn close(h: &mut Harness<'_>) -> bool {
    let closed = h.cx.simulate_close();
    h.cx.run_until_parked();
    closed
}

/// The commands other than messages the client sent, by name; a shutdown
/// counts the messages it sends first.
fn commands(h: &mut Harness<'_>) -> Vec<String> {
    h.commands()
        .into_iter()
        .map(|command| match command {
            NetCommand::Shutdown { before, drain } if before.is_empty() => {
                format!("shutdown drain={drain}")
            }
            NetCommand::Shutdown { before, drain } => {
                format!("shutdown drain={drain} before={}", before.len())
            }
            NetCommand::Restart => "restart".to_owned(),
            NetCommand::Stop => "stop".to_owned(),
            NetCommand::Send(msg) => format!("send {msg:?}"),
        })
        .collect()
}

/// The messages a shutdown sends first, when a shutdown with `drain` is the
/// one command sent since the last call.
fn shutdown(h: &mut Harness<'_>, drain: bool) -> Option<Vec<ClientMessage>> {
    match h.commands().as_mut_slice() {
        [NetCommand::Shutdown { before, drain: d }] if *d == drain => Some(std::mem::take(before)),
        _ => None,
    }
}

fn json(messages: &[ClientMessage]) -> Value {
    serde_json::to_value(messages).expect("encode messages")
}

fn open_conn() -> Connection {
    let mut conn = Connection::new();
    conn.on_connecting();
    conn.on_socket_open(4242);
    conn.on_welcome(1);
    conn
}

fn closed_conn() -> Connection {
    let mut conn = open_conn();
    let _ = conn.on_closed("dropped".to_owned());
    conn
}

fn reconnecting_conn() -> Connection {
    let mut conn = closed_conn();
    conn.on_connecting();
    conn
}

/// `sessions` loaded, `first` shown in pane `p1`, and nothing sent yet.
fn loaded<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    sessions: Vec<SessionSnapshot>,
) -> Harness<'a> {
    let first = sessions.first().map(|s| s.id.clone());
    let fixture = Fixture {
        tabs: vec![tab("t1", &pane("p1", first.as_deref()))],
        sessions,
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, dir, &fixture);
    h.sent();
    h
}

/// One idle session in `r1`, with the exit dialog open.
fn asking<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = loaded(cx, dir, vec![session("s1").in_repo("r1").build()]);
    assert!(!close(&mut h), "an active session keeps the window open");
    assert!(exit_open(&mut h));
    h
}

/// A workspace session on worktrees of `r1` and `r2`.
fn two_members(id: &str, status: &str) -> SessionSnapshot {
    let mut s = session(id).worktree("r1").status(status).build();
    let mut second = s.members[0].clone();
    "r2".clone_into(&mut second.repo_id);
    "r2".clone_into(&mut second.repo_name);
    s.members.push(second);
    s
}

fn kept(repo: &str, commits: u32) -> MemberBranchFate {
    MemberBranchFate {
        repo_id: repo.to_owned(),
        repo_name: repo.to_owned(),
        branch: "wt/x".to_owned(),
        fate: BranchFate::KeptByDefault {
            unique_commits: Some(commits),
            checked_against: vec!["main".to_owned()],
        },
    }
}

fn preview(h: &mut Harness<'_>, id: &str, members: Vec<MemberBranchFate>) {
    h.send(DaemonMessage::DiscardPreview {
        session_id: id.to_owned(),
        members,
    });
}

fn walk_session(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.delete_dialog_session().map(str::to_owned))
}

fn walk_text(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.delete_dialog_text())
}

fn is_preview(sent: &[ClientMessage], id: &str) -> bool {
    matches!(sent, [ClientMessage::PreviewDiscard { session_id }] if session_id == id)
}

fn cleanup(repos: &[&str], remove_worktree: bool, branch: BranchCleanup) -> Vec<CleanupAction> {
    repos
        .iter()
        .map(|repo| CleanupAction {
            repo_id: (*repo).to_owned(),
            remove_worktree,
            branch,
        })
        .collect()
}

#[gpui::test]
fn close_with_no_active_sessions_quits_without_asking(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(
        cx,
        &dir,
        vec![session("s1").in_repo("r1").exited(0).build()],
    );

    assert!(close(&mut h), "the window may close");
    assert_eq!(h.quit_requests(), 1);
    assert!(!exit_open(&mut h), "nothing to ask about");
    assert!(h.sent().is_empty());
    assert!(commands(&mut h).is_empty());
}

#[gpui::test]
fn close_with_only_orphans_and_stopped_quits_without_asking(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let sessions = vec![
        session("lost").in_repo("r1").orphan().build(),
        session("done").in_repo("r1").exited(1).build(),
        session("parked").in_repo("r1").inactive().build(),
    ];
    let mut h = loaded(cx, &dir, sessions);

    assert!(close(&mut h));
    assert_eq!(h.quit_requests(), 1, "the daemon could stop none of them");
    assert!(!exit_open(&mut h));
    assert!(h.sent().is_empty());
    assert!(commands(&mut h).is_empty());
}

#[gpui::test]
fn close_while_disconnected_quits_without_asking(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir, vec![session("s1").in_repo("r1").build()]);
    h.set_connection(closed_conn());

    assert!(close(&mut h));
    assert_eq!(h.quit_requests(), 1);
    assert!(!exit_open(&mut h));
    assert!(commands(&mut h).is_empty());
}

#[gpui::test]
fn close_with_active_sessions_opens_the_exit_dialog_focused_on_keep_running(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir, vec![session("s1").in_repo("r1").build()]);
    h.answer_scrollback("s1", b"");
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "the pane has the keyboard");

    assert!(!close(&mut h), "the window stays open");
    assert!(exit_open(&mut h));
    assert_eq!(h.quit_requests(), 0);
    assert_eq!(
        exit_text(&mut h),
        lines(&[TITLE, BODY, "1 session is currently active."])
    );
    assert_eq!(
        exit_buttons(&mut h),
        buttons(&[
            ("exit-cancel", "Cancel", true),
            ("exit-stop-keep-worktrees", "Stop sessions and quit", true),
            ("exit-abandon-quit", "Abandon & quit", true),
            (
                "exit-keep-running",
                "Keep sessions running in background",
                true
            ),
        ])
    );
    assert_eq!(exit_focus(&mut h).as_deref(), Some("exit-keep-running"));
    assert!(h.bounds("exit-keep-running").origin.x >= gpui::px(0.0));

    h.keys("a tab");
    assert!(h.sent_input("s1").is_empty(), "the dialog has the keyboard");
    assert_eq!(
        exit_focus(&mut h).as_deref(),
        Some("exit-cancel"),
        "Tab wraps"
    );
    h.keys("shift-tab");
    assert_eq!(exit_focus(&mut h).as_deref(), Some("exit-keep-running"));

    assert!(!close(&mut h), "a second close while asking does nothing");
    assert!(exit_open(&mut h));
    assert_eq!(h.quit_requests(), 0);
    assert!(h.sent().is_empty());
    assert!(commands(&mut h).is_empty());
}

#[gpui::test]
fn keep_running_quits_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking(cx, &dir);

    h.click_on("exit-keep-running");
    assert_eq!(h.quit_requests(), 1);
    assert!(h.sent().is_empty());
    assert!(commands(&mut h).is_empty(), "the daemon keeps running");

    assert!(close(&mut h), "the window may close once quitting");
}

#[gpui::test]
fn stop_and_quit_sends_drain_shutdown_and_quits_on_ack(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking(cx, &dir);

    h.click_on("exit-stop-keep-worktrees");
    assert_eq!(commands(&mut h), ["shutdown drain=true"]);
    assert!(h.sent().is_empty(), "the shutdown is the only request");
    assert_eq!(h.quit_requests(), 0, "waits for the daemon");
    assert_eq!(
        exit_buttons(&mut h),
        buttons(&[
            ("exit-cancel", "Cancel", false),
            ("exit-stop-keep-worktrees", "Stopping…", false),
            ("exit-abandon-quit", "Abandon & quit", false),
            (
                "exit-keep-running",
                "Keep sessions running in background",
                false
            ),
        ])
    );
    assert_eq!(exit_focus(&mut h), None, "no button has the focus");
    h.keys("enter");
    h.keys("space");
    assert!(
        commands(&mut h).is_empty(),
        "nothing takes a press meanwhile"
    );

    h.send(DaemonMessage::ShutdownAck {});
    assert_eq!(h.quit_requests(), 1);
}

#[gpui::test]
fn abandon_sends_non_drain_shutdown_and_quits_when_the_connection_closes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking(cx, &dir);

    h.click_on("exit-abandon-quit");
    assert_eq!(commands(&mut h), ["shutdown drain=false"]);
    assert!(h.sent().is_empty());
    assert_eq!(
        exit_buttons(&mut h)[2],
        (
            "exit-abandon-quit".to_owned(),
            "Stopping…".to_owned(),
            false
        )
    );

    h.event(NetEvent::ShutdownSent);
    h.set_connection(reconnecting_conn());
    assert_eq!(h.quit_requests(), 0, "connecting is still waiting");
    h.set_connection(closed_conn());
    assert_eq!(h.quit_requests(), 1, "the closed connection ends the wait");
}

#[gpui::test]
fn shutdown_while_offline_shows_the_error_and_keeps_the_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking(cx, &dir);

    h.click_on("exit-abandon-quit");
    assert_eq!(commands(&mut h), ["shutdown drain=false"]);
    h.set_connection(closed_conn());
    assert_eq!(
        h.quit_requests(),
        0,
        "a close before the shutdown went out does not end the wait"
    );
    h.event(NetEvent::ShutdownFailed);
    assert_eq!(h.quit_requests(), 0, "the app stays");
    assert!(exit_open(&mut h));
    assert_eq!(
        exit_text(&mut h).last().map(String::as_str),
        Some(
            "The connection to the daemon dropped before anything was stopped. Nothing was \
             changed."
        )
    );
    assert_eq!(
        exit_buttons(&mut h),
        buttons(&[
            ("exit-cancel", "Cancel", true),
            ("exit-stop-keep-worktrees", "Stop sessions and quit", false),
            ("exit-abandon-quit", "Abandon & quit", false),
            (
                "exit-keep-running",
                "Keep sessions running in background",
                true
            ),
        ]),
        "back to the choice, the stops waiting for the connection"
    );
    assert_eq!(exit_focus(&mut h).as_deref(), Some("exit-keep-running"));
    h.advance(Duration::from_secs(6));
    assert!(
        exit_buttons(&mut h).len() > 1,
        "no Force quit for a shutdown that never went out"
    );

    h.set_connection(open_conn());
    assert!(exit_buttons(&mut h).iter().all(|(_, _, enabled)| *enabled));
    h.click_on("exit-stop-keep-worktrees");
    assert_eq!(commands(&mut h), ["shutdown drain=true"]);
    assert!(
        !exit_text(&mut h)
            .iter()
            .any(|line| line.starts_with("The connection")),
        "a new try clears the error"
    );
}

#[gpui::test]
fn a_later_session_vanishing_keeps_the_open_confirm(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let sessions = vec![
        session("a").worktree("r1").build(),
        session("b").worktree("r2").build(),
        session("c").worktree("r3").build(),
    ];
    let mut h = loaded(cx, &dir, sessions);
    assert!(!close(&mut h));
    h.click_on("exit-stop-remove-worktrees");
    assert!(is_preview(&h.sent(), "a"));
    preview(&mut h, "a", vec![kept("r1", 1)]);
    let asked = walk_text(&mut h);

    h.send(DaemonMessage::SessionRemoved {
        session_id: "b".to_owned(),
    });
    assert_eq!(walk_session(&mut h).as_deref(), Some("a"));
    assert_eq!(walk_text(&mut h), asked, "the open confirm is untouched");
    assert!(h.sent().is_empty(), "no second preview");

    h.click_on("delete-worktree-keep-branch");
    assert!(is_preview(&h.sent(), "c"), "b is skipped");
    assert_eq!(walk_text(&mut h)[0], "Session 3 of 3");
}

#[gpui::test]
fn closing_the_window_cancels_an_open_delete_confirm(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir, vec![session("s1").worktree("r1").build()]);
    h.right_click_on("leaf-s1");
    h.click_on("menu-stop");
    h.click_on("menu-stop-delete");
    assert!(is_preview(&h.sent(), "s1"));
    assert_eq!(walk_session(&mut h).as_deref(), Some("s1"));

    assert!(!close(&mut h));
    assert!(exit_open(&mut h));
    assert_eq!(
        walk_session(&mut h),
        None,
        "the menu's confirm is cancelled"
    );
    assert!(h.sent().is_empty(), "and sent nothing");
    assert_eq!(exit_focus(&mut h).as_deref(), Some("exit-keep-running"));
    h.keys("escape");
    assert!(!exit_open(&mut h), "the exit dialog has the keyboard");
    assert_eq!(walk_session(&mut h), None, "the confirm stays closed");
}

#[gpui::test]
fn closing_the_window_closes_the_appearance_editor_settings_and_container_menu(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir, vec![session("s1").in_repo("r1").build()]);
    h.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/repos/r1")],
    });
    // The registry asks for the repo's source-control status; that is not
    // what this spec watches.
    h.sent();
    let editor_open = |h: &mut Harness<'_>| h.root(|root, _| root.appearance_editor_title());

    h.right_click_on("leaf-s1");
    h.click_on("session-menu-appearance");
    assert!(editor_open(&mut h).is_some());
    assert!(!close(&mut h));
    assert!(exit_open(&mut h));
    assert!(editor_open(&mut h).is_none(), "the editor is closed");
    h.keys("escape");
    assert!(!exit_open(&mut h), "the exit dialog has the keyboard");

    h.keys("ctrl-,");
    assert!(h.root(|root, _| root.settings_open()));
    assert!(!close(&mut h));
    assert!(
        !h.root(|root, _| root.settings_open()),
        "Settings is closed"
    );
    h.keys("escape");

    h.right_click_on("container-repo:r1");
    assert!(h.root(|root, _| root.container_menu_open()));
    assert!(!close(&mut h));
    assert!(
        !h.root(|root, _| root.container_menu_open()),
        "the container menu is closed"
    );
    assert!(h.sent().is_empty(), "and nothing was sent");
}

#[gpui::test]
fn closing_the_window_closes_the_shell_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir, vec![session("s1").in_repo("r1").build()]);
    h.click_on("sidebar-shell-dialog");
    assert!(h.root(|root, _| root.shell_dialog_open()));
    h.sent();

    assert!(!close(&mut h));
    assert!(exit_open(&mut h));
    assert!(
        !h.root(|root, _| root.shell_dialog_open()),
        "the Shell… dialog is closed"
    );
    assert!(h.sent().is_empty(), "and sent nothing");
    assert_eq!(exit_focus(&mut h).as_deref(), Some("exit-keep-running"));
    h.keys("escape");
    assert!(!exit_open(&mut h), "the exit dialog has the keyboard");
    assert!(
        !h.root(|root, _| root.shell_dialog_open()),
        "the Shell… dialog stays closed"
    );
}

#[gpui::test]
fn remove_worktrees_walks_each_worktree_session_then_stops_discards_and_shuts_down(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let sessions = vec![
        two_members("ws", "working"),
        session("plain").in_repo("r3").build(),
        session("wt").worktree("r4").status("error").build(),
        session("done").worktree("r5").exited(0).build(),
        session("lost").worktree("r6").orphan().build(),
    ];
    let mut h = loaded(cx, &dir, sessions);
    h.answer_scrollback("ws", b"");
    h.sent();
    assert!(!close(&mut h));
    assert_eq!(
        exit_text(&mut h),
        lines(&[
            TITLE,
            BODY,
            "3 sessions are currently active.",
            "(1 orphan — daemon can't stop these; they'll stay running)",
            "2 active sessions have per-session worktrees. You can stop the sessions and either \
             keep or remove those worktree directories.",
        ])
    );
    assert_eq!(
        exit_buttons(&mut h),
        buttons(&[
            ("exit-cancel", "Cancel", true),
            (
                "exit-stop-keep-worktrees",
                "Stop sessions, keep worktrees",
                true
            ),
            (
                "exit-stop-remove-worktrees",
                "Stop sessions, remove worktrees",
                true
            ),
            (
                "exit-keep-running",
                "Keep sessions running in background",
                true
            ),
        ])
    );

    h.click_on("exit-stop-remove-worktrees");
    assert!(is_preview(&h.sent(), "ws"), "the walk asks about ws first");
    assert_eq!(walk_session(&mut h).as_deref(), Some("ws"));
    assert_eq!(
        walk_text(&mut h)[..2],
        lines(&["Session 1 of 2", "Removing the worktree for ws."])
    );
    assert!(exit_open(&mut h), "the exit dialog stays under the walk");
    preview(&mut h, "ws", vec![kept("r1", 2), kept("r2", 1)]);
    h.click_on("delete-worktree-and-branch");
    assert!(is_preview(&h.sent(), "wt"), "then wt");
    assert!(
        commands(&mut h).is_empty(),
        "no shutdown before the last answer"
    );
    assert_eq!(walk_text(&mut h)[0], "Session 2 of 2");

    preview(&mut h, "wt", vec![kept("r4", 1)]);
    h.keys("enter");
    assert!(h.sent().is_empty(), "nothing goes out on its own");
    let sent = shutdown(&mut h, false).expect("one shutdown carrying the stops");
    let expected = [
        ClientMessage::StopSession {
            session_id: "ws".to_owned(),
            cleanup: cleanup(&["r1", "r2"], false, BranchCleanup::Auto),
        },
        ClientMessage::DiscardSession {
            session_id: "ws".to_owned(),
            cleanup: cleanup(&["r1", "r2"], true, BranchCleanup::Delete),
        },
        ClientMessage::StopSession {
            session_id: "plain".to_owned(),
            cleanup: cleanup(&["r3"], false, BranchCleanup::Auto),
        },
        ClientMessage::DiscardSession {
            session_id: "plain".to_owned(),
            cleanup: cleanup(&["r3"], false, BranchCleanup::Auto),
        },
        ClientMessage::StopSession {
            session_id: "wt".to_owned(),
            cleanup: cleanup(&["r4"], false, BranchCleanup::Auto),
        },
        ClientMessage::DiscardSession {
            session_id: "wt".to_owned(),
            cleanup: cleanup(&["r4"], true, BranchCleanup::Keep),
        },
    ];
    assert_eq!(json(&sent), json(&expected), "sent {sent:?}");
    assert_eq!(walk_session(&mut h), None);
    assert_eq!(
        exit_buttons(&mut h)[2],
        (
            "exit-stop-remove-worktrees".to_owned(),
            "Stopping…".to_owned(),
            false
        )
    );
}

#[gpui::test]
fn cancel_in_the_walk_returns_to_the_exit_dialog_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let sessions = vec![
        session("a").worktree("r1").build(),
        session("b").worktree("r2").build(),
    ];
    let mut h = loaded(cx, &dir, sessions);
    h.sent();
    assert!(!close(&mut h));

    h.click_on("exit-stop-remove-worktrees");
    assert!(is_preview(&h.sent(), "a"));
    h.keys("escape");
    assert_eq!(walk_session(&mut h), None, "Esc cancels the walk");
    assert!(exit_open(&mut h), "and returns to the exit dialog");

    h.click_on("exit-stop-remove-worktrees");
    assert!(is_preview(&h.sent(), "a"), "a new walk starts over");
    preview(&mut h, "a", vec![kept("r1", 1)]);
    h.click_on("delete-worktree-keep-branch");
    assert!(is_preview(&h.sent(), "b"));
    h.click_on("delete-worktree-cancel");
    assert_eq!(walk_session(&mut h), None, "Cancel abandons the whole walk");
    assert!(exit_open(&mut h));
    assert!(h.sent().is_empty(), "nothing stopped or discarded");
    assert!(commands(&mut h).is_empty(), "no shutdown");
    assert_eq!(h.quit_requests(), 0);

    h.keys("escape");
    assert!(!exit_open(&mut h), "the exit dialog has the keyboard back");
}

/// A daemon on a loopback port that answers `Hello` with `Welcome` and
/// `sessions`, closes the connection on the first `Shutdown`, then counts
/// any connection that comes after for 1.5 s. Its thread returns the
/// shutdown's `drain` and that count.
fn fake_daemon(sessions: Vec<SessionSnapshot>) -> (u16, JoinHandle<(Option<bool>, usize)>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let port = listener.local_addr().expect("listener address").port();
    let daemon = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept the client");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set a read timeout");
        let mut ws = tungstenite::accept(stream).expect("websocket upgrade");
        let _hello = ws.read().expect("the client's Hello");
        let replies = [
            DaemonMessage::Welcome {
                protocol_version: 23,
                supported_versions: vec![23],
            },
            DaemonMessage::Sessions { sessions },
        ];
        for reply in replies {
            let text = serde_json::to_string(&reply).expect("encode a reply");
            ws.send(Message::text(text)).expect("send a reply");
        }
        let drain = loop {
            match ws.read() {
                Ok(Message::Text(text)) => {
                    let msg: Value = serde_json::from_str(text.as_str()).expect("client JSON");
                    if msg["type"] == "shutdown" {
                        break msg["drain"].as_bool();
                    }
                }
                Ok(_) => {}
                Err(_) => break None,
            }
        };
        drop(ws);
        listener
            .set_nonblocking(true)
            .expect("poll for later connections");
        let deadline = Instant::now() + Duration::from_millis(1_500);
        let mut later = 0;
        while Instant::now() < deadline {
            if listener.accept().is_ok() {
                later += 1;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        (drain, later)
    });
    (port, daemon)
}

#[gpui::test]
fn shutdown_does_not_reconnect_after_close(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let (port, daemon) = fake_daemon(vec![session("s1").in_repo("r1").build()]);
    let ensures = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&ensures);
    let net = NetDeps {
        ensure: Box::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok::<_, anyhow::Error>(DaemonHandshake {
                    protocol_version: 23,
                    port,
                    auth_token: "spec-token".to_owned(),
                    pid: 0,
                    supported_versions: vec![23],
                })
            })
        }),
        identity: None,
        stop: Box::new(|| Box::pin(async { Ok(()) })),
    };
    let mut h = Harness::open_on_net(cx, &dir, net);
    h.wait_until("the daemon's session list", Duration::from_secs(10), |h| {
        h.root(|root, _| {
            root.sidebar_containers()
                .iter()
                .any(|c| c.leaves.iter().any(|leaf| leaf.id == "s1"))
        })
    });

    assert!(!close(&mut h));
    h.click_on("exit-stop-keep-worktrees");
    h.wait_until(
        "the quit once the daemon closed",
        Duration::from_secs(10),
        |h| h.quit_requests() == 1,
    );
    let (drain, later) = daemon.join().expect("the fake daemon finishes");
    assert_eq!(drain, Some(true), "the daemon was asked to drain");
    assert_eq!(later, 0, "no reconnect after the close");
    assert_eq!(
        ensures.load(Ordering::SeqCst),
        1,
        "no ensure_running, so no respawn"
    );
}

#[gpui::test]
fn stuck_shutdown_offers_force_quit_after_five_seconds(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking(cx, &dir);

    h.click_on("exit-stop-keep-worktrees");
    assert_eq!(commands(&mut h), ["shutdown drain=true"]);
    h.advance(Duration::from_millis(4_900));
    assert_eq!(
        exit_buttons(&mut h)[1],
        (
            "exit-stop-keep-worktrees".to_owned(),
            "Stopping…".to_owned(),
            false
        )
    );
    h.advance(Duration::from_millis(100));
    assert_eq!(
        exit_buttons(&mut h),
        buttons(&[("exit-force-quit", "Force quit", true)])
    );
    assert_eq!(
        exit_text(&mut h).last().map(String::as_str),
        Some("Daemon not responding after 5 seconds.")
    );
    assert_eq!(exit_focus(&mut h).as_deref(), Some("exit-force-quit"));
    assert_eq!(h.quit_requests(), 0);

    h.click_on("exit-force-quit");
    assert_eq!(h.quit_requests(), 1);
    assert!(h.sent().is_empty());
    assert!(commands(&mut h).is_empty(), "force quit sends nothing more");
}

#[gpui::test]
fn escape_cancels_the_exit_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = loaded(cx, &dir, vec![session("s1").in_repo("r1").build()]);
    h.answer_scrollback("s1", b"");
    h.sent();
    assert!(!close(&mut h));

    h.keys("escape");
    assert!(!exit_open(&mut h), "Esc cancels");
    assert_eq!(h.quit_requests(), 0);
    assert!(h.sent().is_empty());
    assert!(commands(&mut h).is_empty());
    h.keys("a");
    assert_eq!(h.sent_input("s1"), b"a", "the pane has the keyboard back");

    assert!(!close(&mut h), "the next close asks again");
    h.click_on("exit-cancel");
    assert!(!exit_open(&mut h), "Cancel closes it too");

    assert!(!close(&mut h));
    h.click_on("exit-stop-keep-worktrees");
    h.keys("escape");
    assert!(exit_open(&mut h), "no Esc while the shutdown is on the way");
}

#[gpui::test]
fn exit_dialog_survives_welcome(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking(cx, &dir);

    h.send(DaemonMessage::Welcome {
        protocol_version: 1,
        supported_versions: vec![1],
    });
    assert!(exit_open(&mut h), "a reconnect leaves the dialog open");

    h.set_connection(closed_conn());
    assert!(exit_open(&mut h), "a disconnect leaves it open too");
    assert_eq!(h.quit_requests(), 0);
    assert_eq!(
        exit_buttons(&mut h),
        buttons(&[
            ("exit-cancel", "Cancel", true),
            ("exit-stop-keep-worktrees", "Stop sessions and quit", false),
            ("exit-abandon-quit", "Abandon & quit", false),
            (
                "exit-keep-running",
                "Keep sessions running in background",
                true
            ),
        ]),
        "stopping needs the daemon"
    );
    h.click_on("exit-stop-keep-worktrees");
    assert!(commands(&mut h).is_empty(), "a disabled stop sends nothing");

    h.set_connection(open_conn());
    h.send(DaemonMessage::Welcome {
        protocol_version: 1,
        supported_versions: vec![1],
    });
    assert!(exit_open(&mut h));
    assert!(exit_buttons(&mut h).iter().all(|(_, _, enabled)| *enabled));
}
