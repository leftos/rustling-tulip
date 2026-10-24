//! Target-switch specs: when the client moves from one daemon to another
//! (this PC's and a tailnet host's), nothing of the old daemon stays on
//! screen or takes a click, the exit dialog opened for it closes, and the
//! switch itself sends nothing to either daemon and saves nothing to
//! `native-ui.json`. A new connection on the same target keeps the rows.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use futures::channel::mpsc::unbounded;
use gpui::{Modifiers, MouseButton, TestAppContext, point, px};
use protocol::{ClientMessage, DaemonMessage, SplitDirection, TabEntry};
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{Connection, FilePicker, FolderPicker, NetCommand, RootDeps, RootView};
use serde_json::json;
use support::{
    AutostartRecorder, Fixture, Harness, NotifyRecorder, OpenRecorder, TestDir, history_item, pane,
    repo, session, split, tab,
};

const HOST: &str = "box.tail1234.ts.net";

/// A connection to `target` open on `port`, welcomed with the fake daemon's
/// features.
fn open_on(target: Target, port: u16) -> Connection {
    let mut conn = Connection::for_target(target);
    conn.on_connecting();
    conn.on_socket_open(port);
    conn.on_welcome(support::PROTOCOL, support::daemon_features());
    conn
}

/// An open connection to the tailnet host [`HOST`].
fn remote() -> Connection {
    open_on(Target::tailnet(HOST, None), 8787)
}

/// An open connection to the local daemon, on the fake daemon's port.
fn local() -> Connection {
    open_on(Target::Local, 4242)
}

/// A switch back to this PC whose start of the local daemon failed.
fn local_error() -> Connection {
    let mut conn = Connection::for_target(Target::Local);
    conn.on_connecting();
    let _ = conn.on_error("could not start the daemon".to_owned());
    conn
}

/// `s1` in repo `r1` (registered at `D:/src/r1`, the session on its main
/// tree), shown in pane `p1` of tab `t1`.
fn r1_fixture() -> Fixture {
    let mut fixture = Fixture::single(
        session("s1")
            .members(&[("r1", "main", "D:/src/r1")])
            .build(),
    );
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    fixture
}

/// A diff tab of `a.rs` in `r1`'s main tree.
fn diff_tab(id: &str) -> TabEntry {
    serde_json::from_value(json!({
        "id": id,
        "name": "a.rs",
        "content": { "kind": "diff", "repo_id": "r1", "path": "a.rs", "against": null },
        "created_at": "2026-01-01T00:00:00Z",
    }))
    .expect("diff tab fixture")
}

/// A daemon status of `r1`'s main tree with `changes` unstaged.
fn r1_status(changes: &[&str]) -> DaemonMessage {
    let files: Vec<serde_json::Value> = changes
        .iter()
        .map(|path| json!({ "path": path, "status": "M", "from_path": null }))
        .collect();
    serde_json::from_value(json!({
        "type": "repo_status",
        "repo_id": "r1",
        "index_changes": [],
        "worktree_changes": files,
        "worktree_path": null,
    }))
    .expect("status fixture")
}

/// Whether any sidebar container lists `id` as a leaf.
fn listed(h: &mut Harness<'_>, id: &str) -> bool {
    let id = id.to_owned();
    h.root(move |root, _| {
        root.sidebar_containers()
            .iter()
            .any(|c| c.leaves.iter().any(|leaf| leaf.id == id))
    })
}

fn tab_ids(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.tab_ids())
}

fn pane_ids(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.active_pane_ids())
}

/// The active tab `native-ui.json` holds on disk.
fn saved_active_tab(dir: &TestDir) -> Option<String> {
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).expect("a saved layout");
    let ui: serde_json::Value = serde_json::from_str(&text).expect("native-ui.json is JSON");
    ui["active_tab_id"].as_str().map(str::to_owned)
}

fn toast_titles(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.toasts().iter().map(|t| t.title.clone()).collect())
}

#[gpui::test]
fn target_switch_clears_the_old_hosts_sessions_repos_and_tabs(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &r1_fixture());
    assert!(listed(&mut h, "s1"), "the local daemon's session is listed");
    assert_eq!(tab_ids(&mut h), ["t1"]);

    h.set_connection(remote());
    assert!(!listed(&mut h, "s1"), "the local daemon's session is gone");
    assert!(
        h.root(|root, _| root.sidebar_containers().is_empty()),
        "no repo, workspace or session row of the local daemon is left"
    );
    assert!(
        tab_ids(&mut h).is_empty(),
        "the local daemon's tabs are gone"
    );
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned)),
        None
    );
    assert!(
        pane_ids(&mut h).is_empty(),
        "no pane of the old tab is left"
    );
}

#[gpui::test]
fn remote_to_local_switch_with_ensure_error_leaves_no_clickable_rows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::on_target(cx, &dir, Target::tailnet(HOST, None), &r1_fixture());
    let leaf = h.center("leaf-s1");

    h.set_connection(local_error());
    assert!(!listed(&mut h, "s1"), "the host's session is gone");
    assert!(!h.in_model("leaf-s1"));
    assert!(tab_ids(&mut h).is_empty(), "the host's tabs are gone");
    h.right_click(leaf);
    assert!(
        h.root(|root, _| root.session_menu().is_none()),
        "where the host's row was, no session menu opens"
    );
}

#[gpui::test]
fn reveal_and_path_ctrl_click_are_gone_after_remote_to_local_error(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture::single(session("s1").worktree("r1").build());
    let mut h = Harness::on_target(cx, &dir, Target::tailnet(HOST, None), &fixture);
    h.answer_scrollback("s1", b"");
    h.pty("s1", br"wrote C:\wt\x\docs\notes.md");
    let link = h.cell_center("p1", 9, 0);
    let leaf = h.center("leaf-s1");
    h.sent();

    h.set_connection(local_error());
    assert!(pane_ids(&mut h).is_empty(), "the host's pane is gone");
    h.click(link, Modifiers::secondary_key());
    h.right_click(leaf);
    assert!(
        h.root(|root, _| root.session_menu().is_none()),
        "no menu offers Reveal on the host's worktree"
    );
    assert_eq!(h.opened(), [], "no host path opens on this PC");
    assert_eq!(
        toast_titles(&mut h),
        [] as [String; 0],
        "nothing is even tried"
    );
    let sent = h.sent();
    assert!(sent.is_empty(), "and nothing is fetched: sent {sent:?}");
}

#[gpui::test]
fn target_switch_sends_nothing_to_the_daemon(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = r1_fixture();
    fixture.tabs.push(diff_tab("d1"));
    let mut h = Harness::with(cx, &dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.send(DaemonMessage::SessionHistory {
        request_id: None,
        items: vec![history_item("old").build()],
    });
    h.sent();
    h.commands();

    h.set_connection(remote());
    let sent = h.sent();
    assert!(sent.is_empty(), "the switch sends nothing: {sent:?}");
    assert!(
        h.commands().is_empty(),
        "nor asks the network thread for anything"
    );

    h.load(&Fixture::default());
    let sent = h.sent();
    assert!(
        !sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::Detach { .. }
                | ClientMessage::GetFileSnapshot { .. }
                | ClientMessage::ListSessionHistory { .. }
        )),
        "the host is not told about the local daemon's panes: sent {sent:?}"
    );
}

#[gpui::test]
fn target_switch_does_not_save_native_ui_json(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &r1_fixture());
    assert_eq!(saved_active_tab(&dir).as_deref(), Some("t1"));

    h.set_connection(remote());
    assert_eq!(
        saved_active_tab(&dir).as_deref(),
        Some("t1"),
        "the switch saves nothing"
    );
    h.load(&Fixture {
        sessions: vec![session("h1").build()],
        tabs: vec![tab("ht1", &pane("hp1", Some("h1")))],
        ..Fixture::default()
    });
    assert_eq!(
        h.root(|root, _| root.active_tab_id().map(str::to_owned))
            .as_deref(),
        Some("ht1"),
        "the host's tab shows"
    );
    assert_eq!(
        saved_active_tab(&dir).as_deref(),
        Some("t1"),
        "the host's tab id is not saved as this PC's active tab"
    );
}

#[gpui::test]
fn same_target_reconnect_keeps_the_rows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &r1_fixture());

    let mut dropped = local();
    let _ = dropped.on_closed("socket closed".to_owned());
    h.set_connection(dropped);
    h.set_connection(local());
    assert!(
        listed(&mut h, "s1"),
        "the rows stay until the daemon's lists"
    );
    assert_eq!(tab_ids(&mut h), ["t1"]);
    assert_eq!(pane_ids(&mut h), ["p1"]);
}

#[gpui::test]
fn target_switch_clears_sc_history_diff_tabs_and_worktrees_root(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = r1_fixture();
    fixture.tabs.push(diff_tab("d1"));
    let mut h = Harness::with(cx, &dir, &fixture);
    h.send(r1_status(&["a.rs"]));
    h.send(DaemonMessage::WorktreesRootChanged {
        root: "E:/trees".to_owned(),
        is_override: true,
    });
    h.send(DaemonMessage::SessionHistory {
        request_id: None,
        items: vec![history_item("old").build()],
    });
    assert_eq!(h.root(|root, _| root.sc_badge()), 1);
    assert!(
        h.root(|root, _| root.worktrees_active_line())
            .contains("E:/trees")
    );
    assert!(h.root(|root, _| root.recover_badge()).is_some());
    assert!(h.root(|root, cx| root.diff_tab_header("d1", cx)).is_some());

    h.set_connection(remote());
    assert!(
        !h.root(|root, _| root.worktrees_active_line())
            .contains("E:/trees"),
        "the local daemon's worktrees root is gone"
    );
    assert_eq!(
        h.root(|root, _| root.recover_badge()),
        None,
        "the local daemon's ended sessions are gone"
    );
    assert!(
        h.root(|root, cx| root.diff_tab_header("d1", cx)).is_none(),
        "the local daemon's diff tab view is gone"
    );

    // The host happens to list a repo and session under the same ids.
    h.load(&r1_fixture());
    assert_eq!(
        h.root(|root, _| root.sc_badge()),
        0,
        "the local daemon's status does not count for the host's repo"
    );
}

#[gpui::test]
fn target_switch_closes_the_pane_close_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        sessions: vec![session("s1").build()],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", None),
            ),
        )],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.sent();
    h.click_on("close-pane-p1");
    assert!(
        h.root(|root, _| root.pane_close_open()),
        "the dialog asks first"
    );

    h.set_connection(remote());
    assert!(
        !h.root(|root, _| root.pane_close_open()),
        "the dialog asked about the local daemon's pane"
    );
    let sent = h.sent();
    assert!(
        sent.is_empty(),
        "nothing is closed on either daemon: sent {sent:?}"
    );
}

#[gpui::test]
fn target_switch_ends_a_divider_drag_without_sending(cx: &mut TestAppContext) {
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
    let to = point(from.x, from.y + px(60.0));
    h.cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    h.cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
    h.cx.run_until_parked();

    h.set_connection(remote());
    h.cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    let sent = h.sent();
    assert!(
        sent.is_empty(),
        "the old tab's ratio goes to neither daemon: sent {sent:?}"
    );
}

#[gpui::test]
fn switch_back_to_local_restores_the_saved_active_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    std::fs::write(
        dir.path().join("native-ui.json"),
        json!({ "active_tab_id": "t2" }).to_string(),
    )
    .expect("seed native-ui.json");
    let fixture = Fixture {
        sessions: vec![session("s1").build(), session("s2").build()],
        tabs: vec![
            tab("t1", &pane("p1", Some("s1"))),
            tab("t2", &pane("p2", Some("s2"))),
        ],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    let active = |h: &mut Harness<'_>| h.root(|root, _| root.active_tab_id().map(str::to_owned));
    assert_eq!(active(&mut h).as_deref(), Some("t2"), "the saved tab shows");

    h.set_connection(remote());
    h.load(&Fixture {
        sessions: vec![session("h1").build()],
        tabs: vec![tab("ht1", &pane("hp1", Some("h1")))],
        ..Fixture::default()
    });
    h.set_connection(local());
    h.load(&fixture);
    assert_eq!(
        active(&mut h).as_deref(),
        Some("t2"),
        "back on this PC, its saved tab shows again"
    );
    assert_eq!(saved_active_tab(&dir).as_deref(), Some("t2"));
}

#[gpui::test]
fn boot_on_a_saved_host_starts_on_that_target(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let host = json!({ "name": HOST, "port": 9443 });
    std::fs::write(
        dir.path().join("native-ui.json"),
        json!({ "saved_hosts": [host.clone()], "last_target": host }).to_string(),
    )
    .expect("seed native-ui.json");
    let (tx, mut commands) = unbounded();
    let (_events, events) = unbounded();
    let quits = Rc::new(Cell::new(0));
    let pick_folder: FolderPicker = Rc::new(|_, _| Box::pin(async { None }));
    let pick_file: FilePicker = Rc::new(|_, _, _| Box::pin(async { None }));
    let deps = RootDeps {
        tx,
        events,
        ui_dir: Some(dir.path().to_path_buf()),
        paths: Err("specs have no config dir".to_owned()),
        wanted: None,
        now: Arc::new(Instant::now),
        quit: support::quit_recorder(&quits),
        open: Arc::new(OpenRecorder::default()),
        notify: Arc::new(NotifyRecorder::default()),
        pick_folder,
        pick_file,
        autostart: Arc::new(AutostartRecorder::default()),
    };

    let (root, cx) =
        cx.add_window_view(move |window, cx| RootView::with_transport(deps, window, cx));
    cx.run_until_parked();
    assert_eq!(
        root.read_with(cx, |root, _| root.connection_target().clone()),
        Target::tailnet(HOST, Some(9443)),
        "the window starts on the saved host, on its saved port"
    );
    assert!(
        commands.try_recv().is_err(),
        "nothing asks the network thread to switch"
    );
}

/// `s1` in repo `r1` loaded from the local daemon, with the exit dialog
/// open.
fn asking<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &r1_fixture());
    h.sent();
    let closed = h.cx.simulate_close();
    h.cx.run_until_parked();
    assert!(!closed, "an active session keeps the window open");
    assert!(h.root(|root, _| root.exit_dialog_open()));
    h
}

#[gpui::test]
fn target_switch_closes_the_exit_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking(cx, &dir);

    h.set_connection(remote());
    assert!(
        !h.root(|root, _| root.exit_dialog_open()),
        "the dialog asked about the local daemon's sessions"
    );
    assert_eq!(h.quit_requests(), 0);
}

#[gpui::test]
fn stop_keep_after_a_switch_to_remote_sends_no_shutdown(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = asking(cx, &dir);
    let stop = h.center("exit-stop-keep-worktrees");
    h.commands();

    h.set_connection(remote());
    h.click(stop, Modifiers::none());
    let commands = h.commands();
    assert!(
        !commands
            .iter()
            .any(|command| matches!(command, NetCommand::Shutdown { .. } | NetCommand::Stop)),
        "the host's daemon is neither shut down nor stopped"
    );
    assert_eq!(h.quit_requests(), 0);
}
