//! Multi-host routing specs: each action on a remote host's row, pane or
//! message goes to that host, never to a host found by a first match.

use crate::support;

use gpui::{Modifiers, Pixels, Point, TestAppContext, point, px};
use protocol::{
    AgentDescriptor, AgentId, AttentionReason, BranchFate, CheckoutStrategy, ClientMessage,
    DaemonMessage, InitLayoutKind, MemberBranchFate, MergeEvidence, PresetEntry, PresetTarget,
    SessionMode, SessionSnapshot, SpawnConfig, SpawnRequest, SpawnTarget, SplitDirection,
    VscodeWorkspaceFolder, VscodeWorkspaceSuggestion, WorktreeCleanupFailure, WorktreeReusePolicy,
};
use rustling_tulip_native::target::{HostKey, Target};
use rustling_tulip_native::{Connection, HeaderChipKind, NetEvent, OpenIn, OpenOutcome, PopoutKey};
use support::{
    Fixture, Harness, PROTOCOL, TestDir, pane, remote_in_pane, remote_pane, repo, saved_remote,
    seed_ui, session, split, tab,
};

/// The tailnet host the specs connect beside This PC.
const REMOTE: &str = "box.tail1234.ts.net";

/// The daemon id the dedupe spec gives a daemon reached twice.
const DAEMON: &str = "6f1e2d3c-0000-4000-8000-000000000002";

/// What a row says on a host whose files this PC cannot open.
const LOCAL_FILES: &str = "Not available on a remote host: this opens files on this PC";

fn remote() -> Target {
    Target::tailnet(REMOTE, None)
}

fn ssh_box() -> Target {
    Target::parse_ssh("me@devbox", None).expect("an SSH host string")
}

/// The key of the first sidebar container of `host` that has a folder.
fn folder_container_key(h: &mut Harness<'_>, host: HostKey) -> String {
    h.root(move |root, _| {
        root.sidebar_containers()
            .into_iter()
            .find(|container| container.host == host && container.path.is_some())
            .map(|container| container.key)
            .expect("the host's folder container shows")
    })
}

/// A window on This PC listing `a1`, with the tailnet host connected
/// beside it listing `b1`, a shell in `D:/remote/dir`.
fn remote_folder<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(
        cx,
        dir,
        &Fixture::single(session("a1").in_repo("gone").build()),
    );
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").dir_shell("D:/remote/dir").build()],
    });
    host.sent();
    h.sent();
    h
}

#[gpui::test]
fn a_remote_folders_add_repo_banner_adds_it_on_that_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_folder(cx, &dir);
    let key = folder_container_key(&mut h, HostKey::of(&remote()));

    h.click_on(&format!("container-{key}-add-repo"));

    let adds = |sent: Vec<ClientMessage>| -> usize {
        sent.iter()
            .filter(|msg| matches!(msg, ClientMessage::AddRepo { .. }))
            .count()
    };
    assert_eq!(
        adds(h.host(&remote()).sent()),
        1,
        "the folder is added on its own host"
    );
    assert_eq!(adds(h.sent()), 0, "This PC is not asked");
}

#[gpui::test]
fn a_remote_folders_add_workspace_banner_accepts_that_hosts_scan(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_folder(cx, &dir);
    let suggestion = VscodeWorkspaceSuggestion {
        source_path: "D:/remote/dir/theirs.code-workspace".to_owned(),
        suggested_name: "theirs".to_owned(),
        folders: vec![VscodeWorkspaceFolder {
            path: "D:/remote/dir".to_owned(),
            name: None,
            matched_repo_id: None,
        }],
    };
    h.host(&remote())
        .send(DaemonMessage::VscodeWorkspacesScanned {
            path: "D:\\remote\\dir".to_owned(),
            suggestions: vec![suggestion],
        });
    let key = folder_container_key(&mut h, HostKey::of(&remote()));

    h.click_on(&format!("container-{key}-add-workspace"));

    let accepted = |sent: Vec<ClientMessage>| -> Vec<String> {
        sent.into_iter()
            .filter_map(|msg| match msg {
                ClientMessage::AcceptVscodeWorkspaceSuggestion { suggestion, .. } => {
                    Some(suggestion.suggested_name)
                }
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        accepted(h.host(&remote()).sent()),
        ["theirs"],
        "the remote host's own scan is accepted there"
    );
    assert!(accepted(h.sent()).is_empty(), "This PC is not asked");
}

#[gpui::test]
fn an_ssh_host_whose_name_holds_double_colons_owns_its_rows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let ipv6 = Target::parse_ssh("me@fe80::1", None).expect("an SSH host string");
    let mut host = h.host(&ipv6);
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r2", "C:/r2")],
    });
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").in_repo("r2").build()],
    });
    let mut dropped = Connection::for_target(ipv6.clone());
    dropped.on_connecting();
    dropped.on_socket_open(8787);
    dropped.on_welcome(PROTOCOL, Vec::new());
    let _ = dropped.on_closed("socket closed".to_owned());
    h.host(&ipv6).event(NetEvent::State(dropped));

    let key = h.root(|root, _| {
        root.sidebar_containers()
            .into_iter()
            .find(|container| container.key.ends_with("repo:r2"))
            .map(|container| container.key)
            .expect("the SSH host's repo row shows")
    });
    assert!(
        h.root(move |root, _| root.container_dimmed(&key)),
        "the offline SSH host's repo row dims with its host"
    );
}

#[gpui::test]
fn a_remote_hosts_layout_question_is_answered_to_that_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("a1").build()));
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::LayoutInitRequired {
        has_legacy: false,
        active_session_count: 1,
        clonable: Vec::new(),
    });
    h.sent();
    let start_empty = h.root(|root, _| {
        root.layout_chooser_controls()
            .into_iter()
            .find(|(_, label)| label == "Start empty")
            .map(|(selector, _)| selector)
            .expect("the chooser offers Start empty")
    });

    h.click_on(&start_empty);

    let inits = |sent: Vec<ClientMessage>| -> usize {
        sent.iter()
            .filter(|msg| {
                matches!(
                    msg,
                    ClientMessage::InitLayout {
                        kind: InitLayoutKind::Empty
                    }
                )
            })
            .count()
    };
    assert_eq!(
        inits(h.host(&remote()).sent()),
        1,
        "the choice goes to the host that asked"
    );
    assert_eq!(inits(h.sent()), 0, "This PC is not answered");
}

#[gpui::test]
fn a_remote_host_with_nothing_to_offer_is_answered_to_that_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("a1").build()));
    let mut host = h.host(&remote());
    host.connect();
    h.host(&remote()).send(DaemonMessage::LayoutInitRequired {
        has_legacy: false,
        active_session_count: 0,
        clonable: Vec::new(),
    });
    h.sent();
    assert!(
        !h.root(|root, _| root.layout_chooser_open()),
        "nothing to choose, so nothing is asked"
    );
    assert_eq!(
        init_count(&h.host(&remote()).sent()),
        1,
        "the only choice goes to the host that asked"
    );
    assert_eq!(init_count(&h.sent()), 0, "This PC is not answered");
}

#[gpui::test]
fn a_question_with_nothing_to_offer_does_not_wait_behind_another_hosts_chooser(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("a1").build()));
    h.host(&remote()).connect();
    h.send(layout_question(1));
    assert!(
        h.root(|root, _| root.layout_chooser_open()),
        "This PC's question waits for an answer"
    );
    h.host(&remote()).sent();

    h.host(&remote()).send(layout_question(0));

    assert_eq!(
        init_count(&h.host(&remote()).sent()),
        1,
        "the remote host's only choice is taken at once"
    );
    assert!(
        h.root(|root, _| root.layout_chooser_open()),
        "This PC's chooser still shows"
    );
}

/// How many connections the window opened to `target`.
fn dials(h: &Harness<'_>, target: &Target) -> usize {
    h.opened_targets()
        .iter()
        .filter(|opened| *opened == target)
        .count()
}

/// Has `target`'s daemon say `Welcome`, naming itself [`DAEMON`].
fn welcome_as_daemon(h: &mut Harness<'_>, target: &Target) {
    let mut conn = Connection::for_target(target.clone());
    conn.on_connecting();
    conn.on_socket_open(8787);
    conn.on_welcome(PROTOCOL, support::daemon_features());
    let mut host = h.host(target);
    host.event(NetEvent::State(conn));
    host.send(support::welcome_with_id(PROTOCOL, DAEMON));
}

#[gpui::test]
fn a_dropped_duplicate_is_dialled_again_only_once_its_owner_closes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &serde_json::json!({ "saved_hosts": [
            { "name": REMOTE, "port": 8787 },
            { "kind": "ssh", "name": "me@devbox" },
        ] }),
    );
    let mut h = Harness::open(cx, &dir);
    welcome_as_daemon(&mut h, &saved_remote());
    welcome_as_daemon(&mut h, &ssh_box());
    assert_eq!(
        h.root(|root, _| root.duplicate_of(&HostKey::of(&ssh_box())).cloned()),
        Some(HostKey::of(&saved_remote())),
        "the SSH host is dropped as the tailnet host's duplicate"
    );
    assert_eq!(dials(&h, &ssh_box()), 1);

    let mut retrying = Connection::for_target(saved_remote());
    retrying.on_connecting();
    h.host(&saved_remote()).event(NetEvent::State(retrying));
    assert_eq!(
        dials(&h, &ssh_box()),
        1,
        "the owner's own retry does not free its duplicate"
    );

    let mut closed = Connection::for_target(saved_remote());
    closed.on_connecting();
    let _ = closed.on_closed("socket closed".to_owned());
    h.host(&saved_remote()).event(NetEvent::State(closed));
    assert_eq!(
        dials(&h, &ssh_box()),
        2,
        "once the owner's connection closes, its duplicate is dialled again"
    );
}

#[gpui::test]
fn a_host_dropped_as_a_duplicate_does_not_redial_its_own_duplicates(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &serde_json::json!({ "saved_hosts": [
            { "name": REMOTE, "port": 8787 },
            { "kind": "ssh", "name": "me@devbox" },
        ] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    welcome_as_daemon(&mut h, &saved_remote());
    welcome_as_daemon(&mut h, &ssh_box());
    assert_eq!(
        h.root(|root, _| root.duplicate_of(&HostKey::of(&ssh_box())).cloned()),
        Some(HostKey::of(&saved_remote())),
        "the tailnet host owns the daemon, the SSH host is its duplicate"
    );
    let mut retrying = Connection::for_target(saved_remote());
    retrying.on_connecting();
    h.host(&saved_remote()).event(NetEvent::State(retrying));
    h.send(support::welcome_with_id(PROTOCOL, DAEMON));

    welcome_as_daemon(&mut h, &saved_remote());

    assert_eq!(
        h.root(|root, _| root.duplicate_of(&HostKey::of(&saved_remote())).cloned()),
        Some(HostKey::Local),
        "the tailnet host is dropped as This PC's duplicate"
    );
    assert_eq!(
        dials(&h, &ssh_box()),
        1,
        "the dropped host's own duplicate is not dialled again"
    );
    assert_eq!(
        h.root(|root, _| root.duplicate_of(&HostKey::of(&ssh_box())).cloned()),
        Some(HostKey::Local),
        "the SSH host now names This PC, which holds the daemon"
    );
}

/// Every toast's title, oldest first.
fn toast_titles(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| toast.title.clone())
            .collect()
    })
}

#[gpui::test]
fn each_hosts_resume_report_keeps_its_own_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("a1").build()));
    h.host(&remote()).connect();

    h.send(DaemonMessage::ResumeReport {
        resumed: 2,
        left_in_recover: 0,
    });
    h.host(&remote()).send(DaemonMessage::ResumeReport {
        resumed: 1,
        left_in_recover: 0,
    });

    let reports: Vec<String> = toast_titles(&mut h)
        .into_iter()
        .filter(|title| title.ends_with("after restart"))
        .collect();
    assert_eq!(
        reports,
        [
            "Resumed 2 sessions after restart",
            "Resumed 1 session after restart"
        ],
        "the remote host's report does not replace This PC's"
    );
}

#[gpui::test]
fn an_agent_exit_on_either_host_raises_no_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("s1").build()],
    });
    let before = toast_titles(&mut h);

    h.host(&remote()).send(DaemonMessage::AgentExited {
        session_id: "s1".to_owned(),
    });
    h.send(DaemonMessage::AgentExited {
        session_id: "s1".to_owned(),
    });

    assert_eq!(
        toast_titles(&mut h),
        before,
        "each host's pane shows its shell, no toast says so"
    );
}

/// This PC lists `s1` live; [`REMOTE`] lists `remote_s1` under the same id.
/// Tab `t1` splits This PC's `s1` (pane `p1`) beside the remote one (pane
/// `p2`).
fn shared_id<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    remote_s1: protocol::SessionSnapshot,
) -> Harness<'a> {
    shared_id_with(cx, dir, session("s1").build(), remote_s1)
}

/// As [`shared_id`], with This PC's `s1` given too.
fn shared_id_with<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    local_s1: protocol::SessionSnapshot,
    remote_s1: protocol::SessionSnapshot,
) -> Harness<'a> {
    let mut fixture = Fixture::single(local_s1);
    fixture.tabs = vec![tab(
        "t1",
        &split(
            SplitDirection::Horizontal,
            pane("p1", Some("s1")),
            remote_pane("p2", "s1"),
        ),
    )];
    let mut h = Harness::with(cx, dir, &fixture);
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![remote_s1],
    });
    host.sent();
    h.sent();
    h
}

#[gpui::test]
fn a_remote_panes_exit_overlay_reads_its_own_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shared_id(cx, &dir, session("s1").exited(1).build());

    let selectors = h.root(|root, _| root.exited_overlay_selectors());
    assert!(
        selectors.iter().any(|s| s == "exited-p2"),
        "the remote pane's exited session shows its overlay: {selectors:?}"
    );
    assert!(
        !selectors.iter().any(|s| s == "exited-p1"),
        "This PC's live s1 shows none: {selectors:?}"
    );
    assert!(
        h.in_model("exited-p2"),
        "the overlay draws on the remote pane"
    );
    assert!(!h.in_model("exited-p1"));
}

#[gpui::test]
fn a_duplicate_from_a_remote_pane_of_a_shared_id_goes_to_that_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shared_id(cx, &dir, session("s1").build());

    h.right_click_on("pane-header-p2");
    h.click_on("session-menu-duplicate");
    h.click_on("duplicate-new-tab");

    let duplicates = |sent: Vec<ClientMessage>| -> usize {
        sent.iter()
            .filter(|msg| matches!(msg, ClientMessage::DuplicateSession { .. }))
            .count()
    };
    assert_eq!(
        duplicates(h.host(&remote()).sent()),
        1,
        "the remote pane's session is duplicated on its host"
    );
    assert_eq!(duplicates(h.sent()), 0, "This PC is not asked");
}

#[gpui::test]
fn add_to_new_tab_from_a_remote_sessions_menu_binds_that_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    // Neither host's s1 shows in a pane, and the tab's empty pane has room,
    // so the menu offers Add to new tab.
    let mut fixture = Fixture::single(session("s1").build());
    fixture.tabs = vec![tab("t1", &pane("p1", None))];
    let mut h = Harness::with(cx, &dir, &fixture);
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("s1").build()],
    });
    host.sent();
    h.sent();
    let remote_key = HostKey::of(&remote());
    h.root_update(move |root, window, cx| {
        root.open_session_menu(
            &remote_key,
            "s1",
            None,
            point(px(40.0), px(12.0)),
            window,
            cx,
        );
    });

    h.click_on("session-menu-add-new");

    let new_tabs: Vec<(Option<String>, Option<String>)> = h
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::CreateTab {
                initial_session_id,
                host,
                ..
            } => Some((initial_session_id, host)),
            _ => None,
        })
        .collect();
    assert_eq!(
        new_tabs,
        [(Some("s1".to_owned()), Some(REMOTE.to_owned()))],
        "the new tab, kept in This PC's layout, binds its pane to the remote host's s1"
    );
}

#[gpui::test]
fn a_remote_panes_link_fetches_from_that_host_when_both_hold_the_repo_id(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let ui = serde_json::json!({});
    let mut h = remote_in_pane(cx, &dir, &ui, &["r1"], session("s1").worktree("r1").build());
    let root = dir.path().join("remote-files");
    h.root_update(move |view, _, _| view.set_remote_files_root(root));
    h.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/local/r1")],
    });
    h.sent();

    h.host(&saved_remote()).pty("s1", b"see C:/wt/x/src/a.rs");
    let on = h.cell_center("p2", 6, 0);
    h.click(on, Modifiers::secondary_key());

    let fetches = |sent: Vec<ClientMessage>| -> Vec<String> {
        sent.into_iter()
            .filter_map(|msg| match msg {
                ClientMessage::FetchFile { path, .. } => Some(path),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        fetches(h.host(&saved_remote()).sent()),
        ["src/a.rs"],
        "the link is fetched from the pane's host"
    );
    assert!(fetches(h.sent()).is_empty(), "This PC is not asked");
}

/// A point `fy` of the way down the row tagged `selector`.
fn on_row(h: &mut Harness<'_>, selector: &str, fy: f32) -> Point<Pixels> {
    let row = h.bounds(selector);
    assert!(row.size.height > Pixels::ZERO, "{selector} is on screen");
    point(
        row.origin.x + row.size.width * 0.3,
        row.origin.y + row.size.height * fy,
    )
}

#[gpui::test]
fn a_leaf_reorder_in_a_remote_repo_goes_to_that_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![repo("r1", "C:/r1")],
        sessions: vec![
            session("a").in_repo("r1").build(),
            session("b").in_repo("r1").build(),
        ],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "D:/remote/r1")],
    });
    host.send(DaemonMessage::Sessions {
        sessions: vec![
            session("c").in_repo("r1").build(),
            session("d").in_repo("r1").build(),
        ],
    });
    host.sent();
    h.sent();

    let from = on_row(&mut h, &remote_leaf("", "d"), 0.5);
    let to = on_row(&mut h, &remote_leaf("", "c"), 0.2);
    h.drag_and_drop(from, to);

    let reorders = |sent: Vec<ClientMessage>| -> Vec<(String, Vec<String>)> {
        sent.into_iter()
            .filter_map(|msg| match msg {
                ClientMessage::ReorderSessions {
                    container_id,
                    ordered_ids,
                } => Some((container_id, ordered_ids)),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        reorders(h.host(&remote()).sent()),
        [("r1".to_owned(), vec!["d".to_owned(), "c".to_owned()])],
        "the remote repo's order goes to its host"
    );
    assert!(reorders(h.sent()).is_empty(), "This PC is not asked");
}

/// A `LayoutInitRequired` for a client with `count` active sessions.
fn layout_question(count: u32) -> DaemonMessage {
    DaemonMessage::LayoutInitRequired {
        has_legacy: false,
        active_session_count: count,
        clonable: Vec::new(),
    }
}

/// How many `InitLayout`s are in `sent`.
fn init_count(sent: &[ClientMessage]) -> usize {
    sent.iter()
        .filter(|msg| matches!(msg, ClientMessage::InitLayout { .. }))
        .count()
}

/// Reports `target`'s connection closed.
fn close_host(h: &mut Harness<'_>, target: &Target) {
    let mut closed = Connection::for_target(target.clone());
    closed.on_connecting();
    let _ = closed.on_closed("socket closed".to_owned());
    h.host(target).event(NetEvent::State(closed));
}

#[gpui::test]
fn a_remote_hosts_layout_chooser_closes_with_its_connection(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("a1").build()));
    h.host(&remote()).connect();
    h.host(&remote()).send(layout_question(1));
    assert!(h.root(|root, _| root.layout_chooser_open()));

    close_host(&mut h, &remote());

    assert!(
        !h.root(|root, _| root.layout_chooser_open()),
        "the chooser goes with its host's connection"
    );
}

#[gpui::test]
fn two_hosts_layout_questions_are_each_answered(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("a1").build()));
    h.host(&remote()).connect();
    h.send(layout_question(1));
    h.host(&remote()).send(layout_question(1));
    h.sent();
    h.host(&remote()).sent();

    h.click_on("layout-choose-empty");
    assert!(
        h.root(|root, _| root.layout_chooser_open()),
        "the second host's question shows once the first is answered"
    );
    h.click_on("layout-choose-empty");

    assert_eq!(init_count(&h.sent()), 1, "This PC is answered once");
    assert_eq!(
        init_count(&h.host(&remote()).sent()),
        1,
        "the remote host is answered once"
    );
    assert!(!h.root(|root, _| root.layout_chooser_open()));
}

#[gpui::test]
fn this_pcs_tab_list_leaves_a_remote_hosts_chooser_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("a1").build()));
    h.host(&remote()).connect();
    h.host(&remote()).send(layout_question(1));

    h.send(DaemonMessage::Tabs { tabs: Vec::new() });

    assert!(
        h.root(|root, _| root.layout_chooser_open()),
        "only the remote host's own answer closes its chooser"
    );
}

/// A failed cleanup of session `id`, one worktree left on disk.
fn cleanup_failure(id: &str) -> DaemonMessage {
    DaemonMessage::WorktreeCleanupFailed {
        session_id: id.to_owned(),
        session_label: format!("feat/{id}"),
        failures: vec![WorktreeCleanupFailure {
            member_path: format!("C:/wts/wt.{id}/a"),
            repo_path: "C:/src/a".to_owned(),
            reason: "fs::remove_dir_all: access denied".to_owned(),
            locking_processes: Vec::new(),
        }],
    }
}

/// The session whose failed cleanup is on screen.
fn cleanup_on_screen(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.cleanup_failed_session().map(str::to_owned))
}

#[gpui::test]
fn a_remote_hosts_reset_drops_only_its_cleanup_failures(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("a1").build()));
    h.host(&remote()).connect();
    h.send(cleanup_failure("a1"));
    h.host(&remote()).send(cleanup_failure("b1"));
    assert_eq!(cleanup_on_screen(&mut h).as_deref(), Some("a1"));

    h.host(&remote()).connect();
    assert_eq!(
        cleanup_on_screen(&mut h).as_deref(),
        Some("a1"),
        "This PC's failure stays on screen"
    );
    h.click_on("cleanup-ignore");

    assert_eq!(
        cleanup_on_screen(&mut h),
        None,
        "the remote host's failure went with its reset"
    );
}

#[gpui::test]
fn this_pcs_reset_keeps_a_remote_hosts_cleanup_failure(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("a1").build()));
    h.host(&remote()).connect();
    h.host(&remote()).send(cleanup_failure("b1"));
    h.send(cleanup_failure("a1"));

    this_pc_reconnects(&mut h);

    assert_eq!(
        cleanup_on_screen(&mut h).as_deref(),
        Some("b1"),
        "the remote host's failure outlives This PC's reset"
    );
}

#[gpui::test]
fn this_pcs_reset_leaves_the_keyboard_on_a_remote_hosts_cleanup_failure(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    // A repo, so the spawn dialog has something to open on.
    let mut fixture = Fixture::single(session("a1").build());
    fixture.repos = vec![repo("r1", "C:/r1")];
    let mut h = Harness::with(cx, &dir, &fixture);
    h.host(&remote()).connect();
    h.click_on("sidebar-add-session");
    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "This PC's spawn dialog opened"
    );
    h.host(&remote()).send(cleanup_failure("b1"));

    this_pc_reconnects(&mut h);

    assert!(
        h.root(|root, _| root.cleanup_failed_focus()).is_some(),
        "the remote host's failure is still up"
    );
    let root = h.root.clone();
    let holds =
        h.cx.update(|window, cx| root.read(cx).cleanup_failed_holds_keyboard(window));
    assert!(holds, "the dialog on screen keeps the keyboard");
    h.keys("escape");
    assert_eq!(cleanup_on_screen(&mut h), None, "Esc ignores it");
}

#[gpui::test]
fn a_remote_hosts_tab_list_closes_its_chooser(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("a1").build()));
    h.host(&remote()).connect();
    h.host(&remote()).send(layout_question(1));
    assert!(h.root(|root, _| root.layout_chooser_open()));

    h.host(&remote())
        .send(DaemonMessage::Tabs { tabs: Vec::new() });

    assert!(
        !h.root(|root, _| root.layout_chooser_open()),
        "the remote host's tab list ends its question"
    );
}

#[gpui::test]
fn a_remote_hosts_tab_list_leaves_this_pcs_question(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("a1").build()));
    h.host(&remote()).connect();
    h.send(layout_question(1));
    h.host(&remote()).send(layout_question(1));
    h.sent();
    h.host(&remote()).sent();

    h.host(&remote())
        .send(DaemonMessage::Tabs { tabs: Vec::new() });
    assert!(
        h.root(|root, _| root.layout_chooser_open()),
        "This PC's question still shows"
    );
    h.click_on("layout-choose-empty");

    assert_eq!(init_count(&h.sent()), 1, "This PC is answered once");
    assert!(
        !h.root(|root, _| root.layout_chooser_open()),
        "the remote host's question went with its tab list"
    );
}

#[gpui::test]
fn a_layout_picked_for_a_remote_host_is_not_laid_over_this_pcs_tabs(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    // No session of This PC's, so its tab list owes it nothing else.
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.host(&remote()).connect();
    h.host(&remote()).send(layout_question(5));
    h.sent();
    h.click_on("layout-choose-all");
    h.click_on("chooser-grid-shape-2");
    h.click_on("chooser-confirm-all");
    assert_eq!(
        init_count(&h.host(&remote()).sent()),
        1,
        "the choice goes to the remote host"
    );

    let row = |a: &str, b: &str| split(SplitDirection::Horizontal, pane(a, None), pane(b, None));
    let grid = split(SplitDirection::Vertical, row("p1", "p2"), row("p3", "p4"));
    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &grid)],
    });

    let sent = h.sent();
    assert!(
        sent.is_empty(),
        "no arrangement goes to This PC: sent {sent:?}"
    );
}

/// A descriptor with `id` and `tag`, otherwise the test agent's own.
fn described(id: &str, tag: &str) -> AgentDescriptor {
    let mut descriptor = agent_api::test_agent::DESCRIPTOR.clone();
    descriptor.id = AgentId(id.to_owned());
    tag.clone_into(&mut descriptor.tag);
    descriptor
}

/// The text of the runtime chip `pane_id`'s header carries, if any.
fn runtime_chip(h: &mut Harness<'_>, pane_id: &str) -> Option<String> {
    h.root(|root, _| root.pane_header_parts(pane_id))
        .and_then(|parts| {
            parts
                .chips
                .into_iter()
                .find(|chip| chip.kind == HeaderChipKind::Runtime)
                .map(|chip| chip.text)
        })
}

#[gpui::test]
fn a_remote_panes_header_names_the_agent_its_own_host_lists(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    // Both hosts list `s1`, each tagging its agent its own way: a header
    // resolved by the first host listing the id would read This PC's tag.
    let fixture = Fixture {
        sessions: vec![session("s1").agent("agent-x").build()],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                remote_pane("p2", "s1"),
            ),
        )],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.send(DaemonMessage::Agents {
        agents: vec![described("agent-x", "local-x")],
    });
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Agents {
        agents: vec![described("agent-x", "remote-x")],
    });
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("s1").agent("agent-x").build()],
    });
    host.sent();
    h.sent();

    assert_eq!(
        runtime_chip(&mut h, "p1").as_deref(),
        Some("local-x"),
        "This PC's pane names the agent its own host lists"
    );
    assert_eq!(
        runtime_chip(&mut h, "p2").as_deref(),
        Some("remote-x"),
        "the remote pane names the descriptor its own host lists, not This PC's"
    );
}

/// The point `fx` across pane `pane_id`'s frame, `fy` down it.
fn at(h: &mut Harness<'_>, pane_id: &str, fx: f32, fy: f32) -> Point<Pixels> {
    let frame = h.bounds(&format!("pane-frame-{pane_id}"));
    assert!(frame.origin.x >= px(0.0), "{pane_id} is on screen");
    point(
        frame.origin.x + frame.size.width * fx,
        frame.origin.y + frame.size.height * fy,
    )
}

#[gpui::test]
fn an_offline_hosts_leaf_neither_drags_nor_binds(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    // This PC's tab holds one empty pane; the tailnet host lists `b1` in a
    // repo of its own, shown in no pane.
    let fixture = Fixture {
        sessions: vec![session("a1").build()],
        tabs: vec![tab("t1", &pane("p2", None))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r2", "C:/r2")],
    });
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").in_repo("r2").build()],
    });
    host.sent();
    h.sent();
    close_host(&mut h, &remote());

    let key = h.root(|root, _| {
        root.sidebar_containers()
            .into_iter()
            .find(|container| container.key.ends_with("repo:r2"))
            .map(|container| container.key)
            .expect("the remote host's repo row shows while it is offline")
    });
    assert!(
        h.root(move |root, _| root.container_dimmed(&key)),
        "the offline host's rows dim"
    );

    let from = on_row(&mut h, &remote_leaf("", "b1"), 0.5);
    h.start_drag(from);
    assert!(!h.dragging(), "the offline host's leaf does not drag");
    let to = at(&mut h, "p2", 0.5, 0.5);
    h.drag_to(to);
    assert_eq!(
        h.root(rustling_tulip_native::RootView::pane_drop_overlay),
        None,
        "no drop overlay shows over the pane"
    );
    h.release(to);
    let sent = h.sent();
    assert!(
        sent.is_empty(),
        "the offline leaf binds nothing and sends nothing: sent {sent:?}"
    );
    assert!(
        h.root(|root, _| root.pane_header_parts("p2")).is_none(),
        "the pane stays empty"
    );
}

#[gpui::test]
fn a_remote_session_popouts_orphan_banner_names_its_own_hosts_agent(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    // Both hosts list `s1`, tagged apart; only the remote one's is an
    // orphan, shown in a window of its own.
    let fixture = Fixture {
        sessions: vec![session("s1").agent("agent-x").build()],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.send(DaemonMessage::Agents {
        agents: vec![described("agent-x", "local-x")],
    });
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Agents {
        agents: vec![described("agent-x", "remote-x")],
    });
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("s1").agent("agent-x").orphan().build()],
    });
    host.sent();
    h.sent();

    let key = PopoutKey::session(&HostKey::Remote(REMOTE.to_owned()), "s1");
    assert_eq!(
        h.root_update(move |root, window, cx| root.open_popout(key, window, cx)),
        OpenOutcome::Opened,
        "the remote session's window opens"
    );

    let line = h.root(|root, _| root.orphan_banner_line(&HostKey::Remote(REMOTE.to_owned()), "s1"));
    let line = line.expect("the remote host's orphan shows its banner");
    assert!(
        line.contains("remote-x"),
        "the window's orphan banner names the remote host's agent: {line}"
    );
    assert!(
        !line.contains("local-x"),
        "This PC's descriptor stays off it: {line}"
    );
}

/// This PC lists `a1` in pane `p1` beside [`REMOTE`]'s `b1` in pane `p2`;
/// [`REMOTE`] lists repo `r2` and `remote_session`. Nothing sent is left
/// unread.
fn two_hosts<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    remote_session: SessionSnapshot,
) -> Harness<'a> {
    let fixture = Fixture {
        sessions: vec![session("a1").build()],
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("a1")),
                remote_pane("p2", "b1"),
            ),
        )],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, dir, &fixture);
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r2", "C:/r2")],
    });
    host.send(DaemonMessage::Sessions {
        sessions: vec![remote_session],
    });
    host.sent();
    h.sent();
    h
}

/// Reports This PC's connection lost and back: the connecting overlay, then
/// its `Welcome` and its own session list again.
fn this_pc_reconnects(h: &mut Harness<'_>) {
    h.lose_connection();
    h.welcome_with_features();
    h.send_welcome(PROTOCOL);
    h.send(DaemonMessage::Sessions {
        sessions: vec![session("a1").build()],
    });
    h.sent();
}

/// A claude spawn on branch `feature` of [`REMOTE`]'s repo `r2`.
fn remote_request(use_worktree: bool) -> SpawnRequest {
    SpawnRequest {
        label: None,
        target: SpawnTarget::Single {
            repo_id: "r2".to_owned(),
            branch_name: "feature".to_owned(),
            base_branch: None,
            use_worktree,
            checkout_strategy: None,
            worktree_reuse: WorktreeReusePolicy::default(),
            existing_worktree: None,
        },
        mode: SessionMode::Interactive,
        initial_prompt: None,
        dangerously_skip_permissions: false,
        agent: Some(
            serde_json::from_value(serde_json::json!({ "agent": "claude", "options": {} }))
                .expect("agent selection"),
        ),
        extra_env: Vec::new(),
        secret_env_keys: Vec::new(),
        prompt_injector: None,
        request_id: None,
        resume_conversation: None,
    }
}

/// The request ids of the spawns among `sent`.
fn spawn_ids(sent: &[ClientMessage]) -> Vec<Option<String>> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SpawnSession(request) => Some(request.request_id.clone()),
            _ => None,
        })
        .collect()
}

/// `session` placed with [`REMOTE`] as its host.
fn placed_on_remote(session: &str) -> (Option<String>, Option<String>) {
    (Some(session.to_owned()), Some(REMOTE.to_owned()))
}

/// The session placements among `sent`: every `CreateTab`,
/// `SplitPane` and `ReplacePaneSession`, as the session and host it names.
fn placements(sent: &[ClientMessage]) -> Vec<(Option<String>, Option<String>)> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::CreateTab {
                initial_session_id,
                host,
                ..
            }
            | ClientMessage::SplitPane {
                new_session_id: initial_session_id,
                host,
                ..
            }
            | ClientMessage::ReplacePaneSession {
                session_id: initial_session_id,
                host,
                ..
            } => Some((initial_session_id.clone(), host.clone())),
            _ => None,
        })
        .collect()
}

/// The sessions `sent` discards.
fn discards(sent: &[ClientMessage]) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::DiscardSession { session_id, .. } => Some(session_id.clone()),
            _ => None,
        })
        .collect()
}

/// Whether a spawning toast is on screen.
fn spawning_shown(h: &mut Harness<'_>) -> bool {
    toast_titles(h)
        .iter()
        .any(|title| title == "Spawning session…")
}

#[gpui::test]
fn a_remote_restart_survives_this_pcs_reconnect(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, session("b1").exited(1).build());

    h.click_on("exited-restart-p2");
    let restart = h
        .host(&remote())
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
        .expect("a restart of b1 under a request id");

    this_pc_reconnects(&mut h);

    h.host(&remote()).send(DaemonMessage::SessionUpdated {
        session: session("b1b").build(),
        request_id: Some(restart),
    });
    let replaced: Vec<(String, Option<String>, Option<String>)> = h
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ReplacePaneSession {
                pane_id,
                session_id,
                host,
                ..
            } => Some((pane_id, session_id, host)),
            _ => None,
        })
        .collect();
    assert_eq!(
        replaced,
        [(
            "p2".to_owned(),
            Some("b1b".to_owned()),
            Some(REMOTE.to_owned())
        )],
        "the copy takes the original's pane after This PC's reconnect"
    );
    assert_eq!(
        discards(&h.host(&remote()).sent()),
        ["b1"],
        "the original is discarded on the remote host that answered"
    );
}

#[gpui::test]
fn a_remote_spawns_toast_survives_this_pcs_reconnect(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, session("b1").build());

    let id = h.root_update(|root, _, cx| root.spawn(remote_request(true), OpenIn::NewTab, cx));
    assert_eq!(
        spawn_ids(&h.host(&remote()).sent()),
        [Some(id.clone())],
        "the spawn goes to the remote host"
    );
    assert!(spawning_shown(&mut h), "the spawn shows its toast");

    this_pc_reconnects(&mut h);

    assert!(
        spawning_shown(&mut h),
        "the remote host's spawning toast stays across This PC's reconnect"
    );

    h.host(&remote()).send(DaemonMessage::SessionUpdated {
        session: session("b2").in_repo("r2").build(),
        request_id: Some(id.clone()),
    });
    assert!(
        !spawning_shown(&mut h),
        "the remote host's reply ends the toast"
    );
    assert_eq!(
        placements(&h.sent()),
        [placed_on_remote("b2")],
        "the spawn is placed though This PC reconnected"
    );
}

/// The request id and checkout strategy of every spawn among `sent`.
fn resends(sent: &[ClientMessage]) -> Vec<(Option<String>, Option<CheckoutStrategy>)> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SpawnSession(request) => {
                let strategy = match &request.target {
                    SpawnTarget::Single {
                        checkout_strategy, ..
                    } => *checkout_strategy,
                    _ => None,
                };
                Some((request.request_id.clone(), strategy))
            }
            _ => None,
        })
        .collect()
}

/// The selector of the shown checkout prompt's focused button, if one shows.
fn checkout_focus(h: &mut Harness<'_>) -> Option<&'static str> {
    h.root(|root, _| root.checkout_focus())
}

/// The dirty-tree prompt of [`REMOTE`]'s in-place spawn `request_id`.
fn dirty_remote(request_id: &str) -> DaemonMessage {
    DaemonMessage::CheckoutConfirmRequired {
        repo_id: "r2".to_owned(),
        branch: "feature".to_owned(),
        dirty_count: 1,
        request_id: Some(request_id.to_owned()),
    }
}

#[gpui::test]
fn a_remote_checkout_prompt_survives_this_pcs_reconnect(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, session("b1").build());

    let id = h.root_update(|root, _, cx| root.spawn(remote_request(false), OpenIn::NewTab, cx));
    h.host(&remote()).sent();
    h.host(&remote()).send(dirty_remote(&id));
    assert_eq!(
        checkout_focus(&mut h),
        Some("checkout-cancel"),
        "the remote spawn's prompt shows"
    );

    this_pc_reconnects(&mut h);

    assert_eq!(
        checkout_focus(&mut h),
        Some("checkout-cancel"),
        "the prompt stays shown across This PC's reconnect"
    );

    h.click_on("checkout-stash");
    assert_eq!(
        resends(&h.host(&remote()).sent()),
        [(Some(id.clone()), Some(CheckoutStrategy::Stash))],
        "the answer resends the spawn to the remote host"
    );
    assert!(
        !h.sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::SpawnSession(_))),
        "This PC is not asked"
    );
    assert_eq!(checkout_focus(&mut h), None, "the answer closes the prompt");
}

#[gpui::test]
fn a_remote_hosts_disconnect_closes_its_checkout_prompt(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, session("b1").build());

    let id = h.root_update(|root, _, cx| root.spawn(remote_request(false), OpenIn::NewTab, cx));
    h.host(&remote()).sent();
    h.host(&remote()).send(dirty_remote(&id));
    assert!(checkout_focus(&mut h).is_some(), "the prompt shows");

    close_host(&mut h, &remote());
    h.sent();

    assert_eq!(
        checkout_focus(&mut h),
        None,
        "the dropped host's prompt closes"
    );

    h.host(&remote()).connect();
    h.host(&remote()).send(DaemonMessage::SessionUpdated {
        session: session("b2").in_repo("r2").build(),
        request_id: Some(id),
    });
    assert_eq!(
        placements(&h.sent()),
        [],
        "the reply to the ended spawn is placed nowhere"
    );
}

#[gpui::test]
fn a_remote_sessions_label_uses_its_hosts_agents(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let remote_session = session("b1").agent("agent-y").label("").build();
    let mut h = two_hosts(cx, &dir, remote_session);
    h.host(&remote()).send(DaemonMessage::Agents {
        agents: vec![described("agent-y", "remote-y")],
    });

    h.click_on("close-pane-p2");
    h.click_on("pane-close-only");
    let message = h.root(|root, _| {
        root.undo_entries()
            .first()
            .map(|entry| entry.message.clone())
    });

    assert_eq!(
        message.as_deref(),
        Some("Closed pane \"remote-y\""),
        "the remote session's label reads the agent its own host describes, not This PC's id"
    );
}

#[gpui::test]
fn a_remote_repos_container_menu_gives_the_remote_refusal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, session("b1").in_repo("r2").build());

    h.right_click_on(&format!("container-{REMOTE}::repo:r2"));

    assert_eq!(
        h.root(|root, _| root.container_menu_disabled_tip("container-menu-explorer")),
        Some(LOCAL_FILES.to_owned()),
        "a remote repo's Explorer row gives the remote refusal, not the missing folder"
    );
    assert_eq!(
        h.root(|root, _| root.container_menu_disabled_tip("container-menu-vscode")),
        Some(LOCAL_FILES.to_owned()),
        "and so does its VS Code row"
    );
}

/// How many of `sent` are `DuplicateSession`s.
fn duplicate_count(sent: &[ClientMessage]) -> usize {
    sent.iter()
        .filter(|msg| matches!(msg, ClientMessage::DuplicateSession { .. }))
        .count()
}

/// How many of `sent` are `SpawnSession`s.
fn spawn_count(sent: &[ClientMessage]) -> usize {
    sent.iter()
        .filter(|msg| matches!(msg, ClientMessage::SpawnSession(_)))
        .count()
}

/// The sessions `sent` stops.
fn stops(sent: &[ClientMessage]) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::StopSession { session_id, .. } => Some(session_id.clone()),
            _ => None,
        })
        .collect()
}

/// The sessions whose branch fates `sent` asks for.
fn previews(sent: &[ClientMessage]) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::PreviewDiscard { session_id } => Some(session_id.clone()),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn a_remote_repo_menu_spawns_on_its_own_host_when_this_pc_shares_the_id(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, session("b1").in_repo("r2").build());
    h.send(DaemonMessage::Repos {
        repos: vec![repo("r2", "C:/local/r2")],
    });
    h.sent();

    h.right_click_on(&format!("container-{REMOTE}::repo:r2"));
    h.click_on("container-menu-spawn");
    h.click_on("container-spawn-shell");

    let remote_sent = h.host(&remote()).sent();
    assert_eq!(
        spawn_count(&remote_sent),
        1,
        "the remote repo's shell spawns on its host: {remote_sent:?}"
    );
    assert_eq!(spawn_count(&h.sent()), 0, "This PC's r2 is not asked");
}

#[gpui::test]
fn copy_path_on_a_remote_repo_copies_that_hosts_path(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, session("b1").in_repo("r2").build());

    h.right_click_on(&format!("container-{REMOTE}::repo:r2"));
    assert_eq!(
        h.root(|root, _| root.container_menu_disabled_tip("container-menu-copy-path")),
        None,
        "Copy path is offered for the remote repo's path"
    );
    h.click_on("container-menu-copy-path");

    assert_eq!(
        h.clipboard().as_deref(),
        Some("C:/r2"),
        "the remote host's path is copied"
    );
}

#[gpui::test]
fn restarting_a_remote_session_proceeds_while_this_pc_restarts_the_same_id(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = shared_id_with(
        cx,
        &dir,
        session("s1").exited(1).build(),
        session("s1").exited(1).build(),
    );

    h.click_on("exited-restart-p1");
    assert_eq!(duplicate_count(&h.sent()), 1, "This PC's s1 restarts first");
    h.click_on("exited-restart-p2");

    assert_eq!(
        duplicate_count(&h.host(&remote()).sent()),
        1,
        "the remote host's s1 restarts though This PC's restart of the same id is in flight"
    );
    assert_eq!(duplicate_count(&h.sent()), 0, "This PC is not asked again");
}

#[gpui::test]
fn header_stop_stops_the_panes_own_host_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shared_id_with(
        cx,
        &dir,
        session("s1").exited(1).build(),
        session("s1").build(),
    );

    assert!(
        h.in_model("pane-stop-p2"),
        "the remote pane's live s1 offers Stop, not This PC's exit code"
    );
    h.click_on("pane-stop-p2");
    h.click_on("pane-stop-confirm-p2");

    assert_eq!(
        stops(&h.host(&remote()).sent()),
        ["s1"],
        "the remote host's s1 is stopped"
    );
    assert!(stops(&h.sent()).is_empty(), "This PC is not asked");
}

#[gpui::test]
fn delete_worktree_dialog_acts_on_its_own_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shared_id_with(
        cx,
        &dir,
        session("s1").build(),
        session("s1").worktree("r1").build(),
    );
    let remote_key = HostKey::of(&remote());
    h.root_update(move |root, window, cx| {
        root.open_session_menu(
            &remote_key,
            "s1",
            None,
            point(px(40.0), px(12.0)),
            window,
            cx,
        );
    });

    h.click_on("menu-stop");
    h.click_on("menu-stop-delete");
    assert_eq!(
        previews(&h.host(&remote()).sent()),
        ["s1"],
        "the remote host is asked for its s1's branch fates"
    );
    assert!(previews(&h.sent()).is_empty(), "This PC is not asked");

    h.host(&remote()).send(DaemonMessage::DiscardPreview {
        session_id: "s1".to_owned(),
        members: vec![MemberBranchFate {
            repo_id: "r1".to_owned(),
            repo_name: "r1".to_owned(),
            branch: "wt/x".to_owned(),
            fate: BranchFate::WillDelete {
                into: "origin/main".to_owned(),
                via: MergeEvidence::Ancestry,
            },
        }],
    });
    h.click_on("delete-worktree-and-branch");

    assert_eq!(
        discards(&h.host(&remote()).sent()),
        ["s1"],
        "the delete goes to the remote host's s1"
    );
    assert!(discards(&h.sent()).is_empty(), "This PC's s1 is left alone");
}

#[gpui::test]
fn env_names_are_asked_of_the_spawn_targets_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, session("b1").in_repo("r2").build());
    h.host(&remote()).send(DaemonMessage::Agents {
        agents: vec![described("agent-y", "remote-y")],
    });
    h.host(&remote()).sent();

    h.right_click_on(&format!("container-{REMOTE}::repo:r2"));
    h.click_on("container-menu-spawn");
    h.click_on("container-spawn-agent-agent-y");

    let env_names = |sent: Vec<ClientMessage>| -> usize {
        sent.iter()
            .filter(|msg| matches!(msg, ClientMessage::ListEnvNames { .. }))
            .count()
    };
    assert_eq!(
        env_names(h.host(&remote()).sent()),
        1,
        "the remote target's env names are asked of its host"
    );
    assert_eq!(env_names(h.sent()), 0, "This PC is not asked");
}

/// Whether `sent` holds a `RemoveRepo` of `repo_id`.
fn removes_repo(sent: &[ClientMessage], repo_id: &str) -> bool {
    sent.iter()
        .any(|msg| matches!(msg, ClientMessage::RemoveRepo { repo_id: id } if id == repo_id))
}

#[gpui::test]
fn removing_a_remote_repo_unregisters_it_on_its_own_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, session("b1").build());
    h.send(DaemonMessage::Repos {
        repos: vec![repo("r2", "C:/local/r2")],
    });
    h.sent();

    h.right_click_on(&format!("container-{REMOTE}::repo:r2"));
    h.click_on("container-menu-remove");
    h.click_on("container-menu-remove");

    assert!(
        removes_repo(&h.host(&remote()).sent(), "r2"),
        "the remote host unregisters its r2"
    );
    assert!(!removes_repo(&h.sent(), "r2"), "This PC's r2 stays");
}

#[gpui::test]
fn a_remote_container_menu_closes_when_its_host_drops_the_repo(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, session("b1").build());
    h.send(DaemonMessage::Repos {
        repos: vec![repo("r2", "C:/local/r2")],
    });
    h.sent();

    h.right_click_on(&format!("container-{REMOTE}::repo:r2"));
    assert!(h.root(|root, _| root.container_menu_open()));
    h.host(&remote())
        .send(DaemonMessage::Repos { repos: Vec::new() });

    assert!(
        !h.root(|root, _| root.container_menu_open()),
        "the remote repo's menu closes once its host drops r2, though This PC lists an r2"
    );
}

#[gpui::test]
fn the_delete_dialog_names_its_own_hosts_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shared_id_with(
        cx,
        &dir,
        session("s1").worktree("r1").label("local-one").build(),
        session("s1").worktree("r1").label("remote-one").build(),
    );
    let remote_key = HostKey::of(&remote());
    h.root_update(move |root, window, cx| {
        root.open_session_menu(
            &remote_key,
            "s1",
            None,
            point(px(40.0), px(12.0)),
            window,
            cx,
        );
    });
    h.click_on("menu-stop");
    h.click_on("menu-stop-delete");

    let text = h.root(|root, _| root.delete_dialog_text());
    assert_eq!(
        text.first().map(String::as_str),
        Some("Removing the worktree for remote-one."),
        "the dialog names the remote host's session"
    );
}

#[gpui::test]
fn removing_a_remote_repo_with_sessions_confirms_on_its_own_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, session("b1").in_repo("r2").build());
    h.send(DaemonMessage::Repos {
        repos: vec![repo("r2", "C:/local/r2")],
    });
    h.sent();

    h.right_click_on(&format!("container-{REMOTE}::repo:r2"));
    h.click_on("container-menu-remove");
    assert!(
        h.root(|root, _| root.repo_remove_open()),
        "the remote r2's live session asks for the confirm"
    );
    h.click_on("repo-remove-stop-and-remove");

    let remote_sent = h.host(&remote()).sent();
    assert_eq!(
        stops(&remote_sent),
        ["b1"],
        "the remote host stops its session"
    );
    assert!(
        removes_repo(&remote_sent, "r2"),
        "and unregisters its r2: {remote_sent:?}"
    );
    let local_sent = h.sent();
    assert!(
        stops(&local_sent).is_empty() && !removes_repo(&local_sent, "r2"),
        "This PC's r2 stays: {local_sent:?}"
    );
}

/// A stored plain-shell launch of `r2` in place on `branch`.
fn shell_launch(branch: &str) -> SpawnConfig {
    SpawnConfig {
        target: SpawnTarget::Single {
            repo_id: "r2".to_owned(),
            branch_name: branch.to_owned(),
            base_branch: None,
            use_worktree: false,
            checkout_strategy: None,
            worktree_reuse: WorktreeReusePolicy::default(),
            existing_worktree: None,
        },
        mode: SessionMode::PlainShell,
        dangerously_skip_permissions: false,
        agent: None,
        extra_env: Vec::new(),
    }
}

/// [`two_hosts`], with This PC listing an `r2` of its own at
/// `C:/local/r2`: This PC's `r2` last launched `local`, the remote host's
/// `remote_last`. Nothing sent is left unread.
fn shared_repo<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    local: Option<SpawnConfig>,
    remote_last: Option<SpawnConfig>,
) -> Harness<'a> {
    let mut h = two_hosts(cx, dir, session("b1").in_repo("r2").build());
    let mut local_r2 = repo("r2", "C:/local/r2");
    local_r2.last_spawn_config = local;
    h.send(DaemonMessage::Repos {
        repos: vec![local_r2],
    });
    let mut remote_r2 = repo("r2", "C:/r2");
    remote_r2.last_spawn_config = remote_last;
    h.host(&remote()).send(DaemonMessage::Repos {
        repos: vec![remote_r2],
    });
    h.host(&remote()).sent();
    h.sent();
    h
}

#[gpui::test]
fn launch_last_on_a_remote_repo_spawns_on_its_own_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shared_repo(cx, &dir, None, Some(shell_launch("remote-x")));

    h.right_click_on(&format!("container-{REMOTE}::repo:r2"));
    h.click_on("container-launch-last");
    h.click_on("container-launch-last-current");

    let remote_sent = h.host(&remote()).sent();
    assert_eq!(
        spawn_count(&remote_sent),
        1,
        "the remote repo's last launch replays on its host: {remote_sent:?}"
    );
    assert_eq!(spawn_count(&h.sent()), 0, "This PC's r2 is not asked");
}

#[gpui::test]
fn a_remote_repos_launch_summary_is_its_own_hosts_last_config(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shared_repo(
        cx,
        &dir,
        Some(shell_launch("local-x")),
        Some(shell_launch("remote-x")),
    );

    h.right_click_on(&format!("container-{REMOTE}::repo:r2"));
    h.click_on("container-menu-spawn");

    let labels = h.root(|root, _| root.container_menu_labels());
    assert!(
        labels.iter().any(|label| label.contains("remote-x")),
        "Last sums up the remote host's stored launch: {labels:?}"
    );
    assert!(
        !labels.iter().any(|label| label.contains("local-x")),
        "not This PC's r2's: {labels:?}"
    );
}

/// A preset of `r2` with a prompts folder under it and no variables.
fn folder_preset() -> PresetEntry {
    serde_json::from_value(serde_json::json!({
        "id": "triage", "name": "Triage", "source_repo_id": "r2",
        "prompt_sources": [{"kind": "folder", "relative_path": "docs/prompts"}],
        "variables": [],
        "prompt_template": "{prompt}", "branch_template": "tmp/{index}",
        "tab_grouping": {"kind": "none"},
        "injector": {"startup_delay_ms": 0, "pre_input": [], "post_input": []}
    }))
    .expect("preset fixture")
}

/// The preset messages among `sent`, by kind.
fn preset_messages(sent: &[ClientMessage]) -> Vec<&'static str> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::ListPresets { .. } => Some("list"),
            ClientMessage::PreviewPreset { .. } => Some("preview"),
            ClientMessage::ResolvePresetScripts { .. } => Some("scripts"),
            ClientMessage::LaunchPreset { .. } => Some("launch"),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn a_preset_from_a_remote_repo_menu_opens_on_its_own_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shared_repo(cx, &dir, None, None);
    let target = PresetTarget::Repo {
        repo_id: "r2".to_owned(),
    };

    h.right_click_on(&format!("container-{REMOTE}::repo:r2"));
    let request_id = h
        .host(&remote())
        .sent()
        .into_iter()
        .find_map(|msg| match msg {
            ClientMessage::ListPresets {
                target: asked,
                request_id,
            } if asked == target => request_id,
            _ => None,
        });
    assert!(
        request_id.is_some(),
        "the remote repo's presets are asked of its host"
    );
    h.host(&remote()).send(DaemonMessage::Presets {
        target,
        entries: vec![folder_preset()],
        request_id,
    });
    h.click_on("container-menu-launch-preset-0");

    assert_eq!(
        h.root(|root, cx| root.preset_wizard_path_text("preset-source-folder", cx))
            .as_deref(),
        Some("C:/r2\\docs\\prompts"),
        "the wizard opens on the remote host, its folder under that host's r2"
    );
    h.click_on("preset-next");
    assert_eq!(
        preset_messages(&h.host(&remote()).sent()),
        ["preview"],
        "the preview is asked of the remote host"
    );
    assert!(
        preset_messages(&h.sent()).is_empty(),
        "This PC is asked for no preset of the remote repo"
    );
}

#[gpui::test]
fn arming_stop_on_a_remote_pane_leaves_this_pcs_same_id_pane_unarmed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shared_id_with(cx, &dir, session("s1").build(), session("s1").build());

    h.click_on("pane-stop-p2");

    assert_eq!(
        h.root(|root, _| root.armed_stop_pane().map(str::to_owned))
            .as_deref(),
        Some("p2"),
        "the remote pane's Stop is the armed one"
    );
    assert!(
        h.in_model("pane-stop-confirm-p2"),
        "the remote pane asks for the confirm"
    );
    assert!(
        !h.in_model("pane-stop-confirm-p1"),
        "This PC's s1 pane is not armed"
    );
    assert!(h.in_model("pane-stop-p1"), "it keeps its plain Stop");
}

/// A branch fate of `r1`'s `wt/x` that deletes the branch.
fn will_delete() -> MemberBranchFate {
    MemberBranchFate {
        repo_id: "r1".to_owned(),
        repo_name: "r1".to_owned(),
        branch: "wt/x".to_owned(),
        fate: BranchFate::WillDelete {
            into: "origin/main".to_owned(),
            via: MergeEvidence::Ancestry,
        },
    }
}

#[gpui::test]
fn a_late_preview_from_another_host_does_not_fill_the_delete_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shared_id_with(
        cx,
        &dir,
        session("s1").worktree("r1").build(),
        session("s1").worktree("r1").build(),
    );
    let remote_key = HostKey::of(&remote());
    h.root_update(move |root, window, cx| {
        root.open_session_menu(
            &remote_key,
            "s1",
            None,
            point(px(40.0), px(12.0)),
            window,
            cx,
        );
    });
    h.click_on("menu-stop");
    h.click_on("menu-stop-delete");

    h.send(DaemonMessage::DiscardPreview {
        session_id: "s1".to_owned(),
        members: vec![will_delete()],
    });

    let buttons: Vec<String> = h
        .root(|root, _| root.delete_dialog_buttons())
        .into_iter()
        .map(|(selector, _)| selector)
        .collect();
    assert_eq!(
        buttons,
        ["delete-worktree-cancel"],
        "This PC's preview of its own s1 leaves the remote s1's confirm waiting"
    );
}

/// How many of `sent` ask for the env names.
fn env_name_asks(sent: &[ClientMessage]) -> usize {
    sent.iter()
        .filter(|msg| matches!(msg, ClientMessage::ListEnvNames { .. }))
        .count()
}

#[gpui::test]
fn changing_the_spawn_target_host_asks_its_env_names(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, session("b1").in_repo("r2").build());
    h.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/local/r1")],
    });
    h.click_on("sidebar-add-session");
    h.click_on("spawn-target");
    h.click_on("spawn-target-repo-r1");
    h.sent();
    h.host(&remote()).sent();

    h.click_on("spawn-target");
    h.click_on(&format!("spawn-target-repo-r2@{REMOTE}"));

    assert_eq!(
        env_name_asks(&h.host(&remote()).sent()),
        1,
        "the new target's host is asked for its env names"
    );
    assert_eq!(env_name_asks(&h.sent()), 0, "This PC is not asked again");
}

/// Remote leaf selector `leaf-{part}{REMOTE}::{id}`: the host part a
/// remote host's leaf selectors carry.
fn remote_leaf(part: &str, id: &str) -> String {
    format!("leaf-{part}{REMOTE}::{id}")
}

#[gpui::test]
fn a_remote_leaf_has_host_qualified_selectors(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shared_id(cx, &dir, session("s1").build());

    assert!(h.in_model("leaf-s1"), "This PC's leaf keeps its selector");
    let remote = remote_leaf("", "s1");
    assert!(h.in_model(&remote), "the remote leaf is addressable");
    assert!(
        h.in_model(&format!("{remote}-label")),
        "its label moves with it"
    );
    assert_ne!(
        h.bounds("leaf-s1").origin.y,
        h.bounds(&remote).origin.y,
        "the two leaves are distinct rows"
    );
}

/// The sessions among `sent` an abandoned session's Resume goes out for.
fn abandoned_resumes(sent: &[ClientMessage]) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::ResumeAbandoned { session_id } => Some(session_id.clone()),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn resume_on_a_remote_leaf_resumes_on_its_own_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shared_id_with(
        cx,
        &dir,
        session("s1").abandoned().build(),
        session("s1").abandoned().build(),
    );

    h.click_on(&remote_leaf("resume-", "s1"));

    assert_eq!(
        abandoned_resumes(&h.host(&remote()).sent()),
        ["s1"],
        "the remote leaf's Resume goes to its host"
    );
    assert!(
        abandoned_resumes(&h.sent()).is_empty(),
        "This PC is not asked"
    );
}

#[gpui::test]
fn a_remote_leafs_pill_shows_its_own_hosts_tab_state(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    // Only This PC's s1 shows in a pane; the remote host's s1 is in none.
    let mut fixture = Fixture::single(session("s1").build());
    fixture.tabs = vec![tab("t1", &pane("p1", Some("s1")))];
    let mut h = Harness::with(cx, &dir, &fixture);
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("s1").build()],
    });
    host.sent();
    h.sent();

    // An unbound pill opens its session in a new tab; a bound one does not.
    h.click_on(&remote_leaf("pill-", "s1"));

    assert_eq!(
        placements(&h.sent()),
        [placed_on_remote("s1")],
        "the remote leaf's pill is unbound, as its own s1 shows in no pane"
    );
}

#[gpui::test]
fn clicking_a_remote_leaf_selects_its_own_hosts_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("s1").build());
    fixture.tabs = vec![
        tab("t1", &pane("p1", Some("s1"))),
        tab("t2", &remote_pane("p2", "s1")),
    ];
    let mut h = Harness::with(cx, &dir, &fixture);
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("s1").build()],
    });
    assert_eq!(h.root(|root, _| root.active_pane_ids()), ["p1"]);

    h.click_on(&remote_leaf("", "s1"));

    assert_eq!(
        h.root(|root, _| root.active_pane_ids()),
        ["p2"],
        "the remote leaf shows the pane of its own host's s1"
    );
}

#[gpui::test]
fn a_remote_session_notice_reads_its_own_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shared_id_with(
        cx,
        &dir,
        session("s1").no_agent().build(),
        session("s1").agent("agent-x").build(),
    );
    let mut labelled = described("agent-x", "remote-x");
    "Remote X".clone_into(&mut labelled.label);
    h.host(&remote()).send(DaemonMessage::Agents {
        agents: vec![labelled],
    });

    h.host(&remote()).send(DaemonMessage::SessionNotice {
        session_id: "s1".to_owned(),
        notice: protocol::SessionNoticeKind::WaitingInMain,
    });
    h.send(DaemonMessage::SessionNotice {
        session_id: "s1".to_owned(),
        notice: protocol::SessionNoticeKind::WaitingInMain,
    });

    let titles = toast_titles(&mut h);
    assert!(
        titles
            .iter()
            .any(|title| title == "Remote X is waiting in the main conversation."),
        "the remote notice names the agent its own host lists: {titles:?}"
    );
    assert!(
        titles
            .iter()
            .any(|title| title == "The agent is waiting in the main conversation."),
        "This PC's notice keeps a toast of its own: {titles:?}"
    );
}

/// The font sizes `sent` sets on session `id`.
fn font_sizes(sent: &[ClientMessage], id: &str) -> Vec<Option<u16>> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SetSessionAppearance {
                session_id,
                appearance,
                ..
            } if session_id == id => Some(appearance.terminal_font_size),
            _ => None,
        })
        .collect()
}

/// Focuses pane `pane_id` and presses Ctrl+=.
fn step_font_in(h: &mut Harness<'_>, pane_id: &str) {
    let on = h.cell_center(pane_id, 0, 0);
    h.click(on, Modifiers::none());
    h.keys("ctrl-=");
}

#[gpui::test]
fn a_stale_remote_session_drops_only_its_own_ui(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shared_id(cx, &dir, session("s1").build());
    // A click focuses a pane only in an active window.
    h.cx.update(|window, _| window.activate_window());

    step_font_in(&mut h, "p2");
    assert_eq!(
        h.root(|root, _| root.focused_pane()),
        Some("p2".to_owned()),
        "the click focuses the remote pane"
    );
    let remote_first = font_sizes(&h.host(&remote()).sent(), "s1");
    assert_eq!(
        remote_first.len(),
        1,
        "the remote pane's step goes to its host"
    );
    assert!(
        font_sizes(&h.sent(), "s1").is_empty(),
        "This PC is not asked"
    );
    step_font_in(&mut h, "p1");
    let local_first = font_sizes(&h.sent(), "s1");
    assert_eq!(local_first.len(), 1, "This PC's step stays in flight");

    // The remote host drops its s1, then lists it again unchanged.
    h.host(&remote()).send(DaemonMessage::Sessions {
        sessions: Vec::new(),
    });
    h.host(&remote()).send(DaemonMessage::Sessions {
        sessions: vec![session("s1").build()],
    });

    step_font_in(&mut h, "p2");
    assert_eq!(
        font_sizes(&h.host(&remote()).sent(), "s1"),
        remote_first,
        "the remote host's stale send was dropped with its session"
    );
    step_font_in(&mut h, "p1");
    assert_eq!(
        font_sizes(&h.sent(), "s1"),
        [local_first[0].map(|size| size + 1)],
        "This PC's send, still in flight, is kept"
    );
}

#[gpui::test]
fn needs_you_reads_the_remote_leafs_own_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shared_id(
        cx,
        &dir,
        session("s1")
            .status("awaiting_input")
            .pending_input(serde_json::json!({
                "kind": "question",
                "questions": [{
                    "question": "ship it?",
                    "header": "Deploy",
                    "options": [],
                    "multi_select": false,
                }],
            }))
            .build(),
    );

    let details: Vec<String> = h
        .root(|root, _| root.needs_you_rows())
        .into_iter()
        .map(|row| row.detail)
        .collect();
    assert_eq!(
        details,
        ["Deploy: ship it?"],
        "the remote row reads what its own host's s1 asks"
    );
}

/// The sessions among `sent` whose discard is previewed.
fn discard_previews(sent: &[ClientMessage]) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::PreviewDiscard { session_id } => Some(session_id.clone()),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn closing_a_remote_pane_deletes_on_its_own_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shared_id_with(
        cx,
        &dir,
        session("s1").worktree("r1").build(),
        session("s1").worktree("r1").build(),
    );
    h.root_update(|root, window, cx| {
        root.close_pane_clicked("t1", "p2", Some("s1"), window, cx);
    });

    h.click_on("pane-close-delete");

    assert_eq!(
        discard_previews(&h.host(&remote()).sent()),
        ["s1"],
        "the remote pane's delete asks its own host"
    );
    assert!(
        discard_previews(&h.sent()).is_empty(),
        "This PC is not asked"
    );
}

#[gpui::test]
fn popping_out_a_remote_pane_uses_its_own_hosts_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    // This PC's s1 cannot show in a pane; the remote host's can.
    let mut h = shared_id_with(
        cx,
        &dir,
        session("s1").headless().build(),
        session("s1").build(),
    );

    h.click_on("pane-popout-p2");
    assert_eq!(
        h.window_count(),
        2,
        "the remote pane pops out on its own host's s1"
    );
}

#[gpui::test]
fn removing_a_remote_repo_counts_its_own_hosts_workspace_sessions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, session("b1").in_repo("r2").build());
    // This PC's workspace holds an r2; the remote host's r2 is in none.
    h.send(DaemonMessage::Workspaces {
        workspaces: vec![support::workspace("w1", &["r2"])],
    });
    h.sent();

    h.right_click_on(&format!("container-{REMOTE}::repo:r2"));
    h.click_on("container-menu-remove");

    assert_eq!(
        h.root(|root, _| root.repo_remove_session_rows()),
        ["b1 (claude)"],
        "the remote r2's own session is counted"
    );
}

/// This PC lists `a1` (pane `p0`, tab `t0`, in front) and `s1` (pane `p1`,
/// tab `t1`); [`REMOTE`] lists `remote_s1` under the same id (pane `p2`,
/// tab `t2`). Nothing sent is left unread.
fn shared_id_tabs<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    remote_s1: SessionSnapshot,
) -> Harness<'a> {
    let fixture = Fixture {
        sessions: vec![session("a1").build(), session("s1").build()],
        tabs: vec![
            tab("t0", &pane("p0", Some("a1"))),
            tab("t1", &pane("p1", Some("s1"))),
            tab("t2", &remote_pane("p2", "s1")),
        ],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, dir, &fixture);
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![remote_s1],
    });
    host.sent();
    h.sent();
    h
}

/// Session `id` waits on the user.
fn awaiting(id: &str) -> DaemonMessage {
    DaemonMessage::Attention {
        session_id: id.to_owned(),
        reason: AttentionReason::AwaitingInput,
    }
}

#[gpui::test]
fn clicking_a_remote_needs_you_row_shows_its_own_hosts_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shared_id_tabs(cx, &dir, session("s1").status("awaiting_input").build());
    assert_eq!(h.root(|root, _| root.active_pane_ids()), ["p0"]);

    h.click_on("activity-needs-you");
    h.click_on("needs-you-row-s1");

    assert_eq!(
        h.root(|root, _| root.active_pane_ids()),
        ["p2"],
        "the remote row shows the pane of its own host's s1, not This PC's"
    );
}

#[gpui::test]
fn a_remote_reconnect_drops_its_unanswered_appearance_sends(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shared_id(cx, &dir, session("s1").build());
    // A click focuses a pane only in an active window.
    h.cx.update(|window, _| window.activate_window());
    step_font_in(&mut h, "p2");
    let remote_first = font_sizes(&h.host(&remote()).sent(), "s1");
    assert_eq!(
        remote_first.len(),
        1,
        "the remote pane's step goes to its host"
    );
    step_font_in(&mut h, "p1");
    let local_first = font_sizes(&h.sent(), "s1");
    assert_eq!(local_first.len(), 1, "This PC's step stays in flight");

    // The remote host reconnects and lists its s1 again unchanged.
    h.host(&remote()).connect();
    h.host(&remote()).send(DaemonMessage::Sessions {
        sessions: vec![session("s1").build()],
    });

    step_font_in(&mut h, "p2");
    assert_eq!(
        font_sizes(&h.host(&remote()).sent(), "s1"),
        remote_first,
        "the reconnect dropped the remote host's unanswered send"
    );
    step_font_in(&mut h, "p1");
    assert_eq!(
        font_sizes(&h.sent(), "s1"),
        [local_first[0].map(|size| size + 1)],
        "This PC's send, still in flight, is kept"
    );
}

#[gpui::test]
fn pane_close_discards_on_the_host_it_opened_for(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shared_id(cx, &dir, session("s1").build());
    h.root_update(|root, window, cx| {
        root.close_pane_clicked("t1", "p2", Some("s1"), window, cx);
    });

    // While the dialog is open, This PC's layout binds p2 to its own s1.
    h.send(DaemonMessage::Tabs {
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("s1")),
                pane("p2", Some("s1")),
            ),
        )],
    });
    h.sent();
    assert!(
        h.root(|root, _| root.pane_close_open()),
        "precondition: the dialog stays, its pane still showing an s1"
    );

    h.click_on("pane-close-discard");

    assert_eq!(
        discards(&h.host(&remote()).sent()),
        ["s1"],
        "the discard goes to the host the dialog opened for"
    );
    assert!(discards(&h.sent()).is_empty(), "This PC's s1 is left alone");
}

/// Whether `host`'s leaf `s1` shows in the sidebar needing attention.
fn s1_attention(h: &mut Harness<'_>, host: HostKey) -> Option<bool> {
    h.root(move |root, _| {
        root.sidebar_containers()
            .into_iter()
            .filter(|container| container.host == host)
            .flat_map(|container| container.leaves)
            .find(|leaf| leaf.id == "s1")
            .map(|leaf| leaf.attention)
    })
}

#[gpui::test]
fn opening_a_remote_leaf_keeps_this_pcs_same_id_attention(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = shared_id(cx, &dir, session("s1").build());
    h.send(awaiting("s1"));
    h.host(&remote()).send(awaiting("s1"));
    assert_eq!(s1_attention(&mut h, HostKey::of(&remote())), Some(true));

    h.click_on(&remote_leaf("", "s1"));

    assert_eq!(
        s1_attention(&mut h, HostKey::of(&remote())),
        Some(false),
        "the opened remote s1 no longer needs you"
    );
    assert_eq!(
        s1_attention(&mut h, HostKey::Local),
        Some(true),
        "This PC's s1 still does"
    );
}

#[gpui::test]
fn a_remote_toasts_go_to_pane_shows_its_own_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &serde_json::json!({ "notifications": { "toast_style": "app" } }),
    );
    let mut h = shared_id_tabs(cx, &dir, session("s1").build());
    h.host(&remote()).send(awaiting("s1"));
    assert_eq!(h.root(|root, _| root.active_pane_ids()), ["p0"]);

    let mut toast = h.popout_window();
    toast.run_until_parked();
    let go = toast
        .debug_bounds("toast-go-to-pane")
        .expect("the remote s1's toast offers Go to pane");
    toast.simulate_click(go.center(), Modifiers::none());
    toast.run_until_parked();

    assert_eq!(
        h.root(|root, _| root.active_pane_ids()),
        ["p2"],
        "Go to pane shows the pane of the toast's own host's s1"
    );
}
