//! Multi-host specs: the connections the window opens, one per host, and
//! which host's connection each message goes out on.

use crate::support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use gpui::{Modifiers, Pixels, Point, TestAppContext, point, px};
use protocol::{
    AgentId, AttentionReason, ChatAnswerKind, ChatOption, ChatPrompt, ChatPromptKind, ChatScreen,
    ClientMessage, ComposerState, ContainerRef, DaemonMessage, DeviceInfo, KeepAwakeMode,
    OptionValue, RecoverItemResult, RemoteState, SessionSnapshot, SplitDirection, TabEntry,
    VscodeWorkspaceFolder, VscodeWorkspaceSuggestion, WorktreeCleanupFailure,
    WorktreeLockingProcess,
};
use rustling_tulip_native::ScKey;
use rustling_tulip_native::connection_picker::PeerSource;
use rustling_tulip_native::file_fetch::FetchRequest;
use rustling_tulip_native::target::{HostKey, Target};
use rustling_tulip_native::{
    Connection, HeaderChipKind, NetCommand, NetEvent, toast_action_selector,
};
use support::{
    Fixture, Forget, Harness, PROTOCOL, TestDir, facts, facts_message, forgets, history_item, pane,
    remote_in_pane, remote_lists, remote_pane, repo, saved_remote, saved_remote_over, saved_ui,
    seed_ui, session, split, tab, two_tab_fixture, two_tabs, welcome,
};

#[gpui::test]
fn the_window_opens_one_connection_for_this_pc_with_no_saved_hosts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("s1").build()));

    assert_eq!(
        h.opened_targets(),
        [Target::Local],
        "a default start opens one connection, to this PC"
    );

    h.answer_scrollback("s1", b"");
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());
    h.sent();
    h.keys("a");
    let typed: Vec<String> = h
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::SendInput { session_id, .. } => Some(session_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        typed,
        ["s1"],
        "a key typed into the pane goes out on this PC's connection"
    );
}

/// The tailnet host the specs connect beside This PC.
const REMOTE: &str = "box.tail1234.ts.net";

fn remote() -> Target {
    Target::tailnet(REMOTE, None)
}

/// A window on This PC listing `a1`, with the tailnet host connected
/// beside it listing `remote_sessions`.
fn two_hosts<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    remote_sessions: Vec<SessionSnapshot>,
) -> Harness<'a> {
    let mut h = Harness::with(
        cx,
        dir,
        &Fixture::single(session("a1").in_repo("gone").build()),
    );
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: remote_sessions,
    });
    h.sent();
    h
}

/// Every leaf the sidebar shows, in its order.
fn shown_leaves(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| {
        root.sidebar_containers()
            .into_iter()
            .flat_map(|container| container.leaves.into_iter().map(|leaf| leaf.id))
            .collect()
    })
}

/// Every container key the sidebar shows, with whether it is folded.
fn shown_folds(h: &mut Harness<'_>) -> Vec<(String, bool)> {
    h.root(|root, _| {
        root.sidebar_containers()
            .into_iter()
            .map(|container| (container.key, container.collapsed))
            .collect()
    })
}

#[gpui::test]
fn two_hosts_sessions_show_side_by_side(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, vec![session("b1").in_repo("gone").build()]);

    assert_eq!(
        h.opened_targets(),
        [Target::Local, remote()],
        "the second host has its own connection"
    );
    assert_eq!(
        shown_leaves(&mut h),
        ["a1", "b1"],
        "both hosts' sessions show, This PC's first"
    );
}

#[gpui::test]
fn one_hosts_sessions_list_leaves_the_other_hosts_alone(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, vec![session("b1").in_repo("gone").build()]);

    h.host(&remote()).send(DaemonMessage::Sessions {
        sessions: Vec::new(),
    });
    assert_eq!(
        shown_leaves(&mut h),
        ["a1"],
        "the remote list emptied only its own"
    );

    h.host(&remote()).send(DaemonMessage::Sessions {
        sessions: vec![session("b2").in_repo("gone").build()],
    });
    h.send(DaemonMessage::Sessions {
        sessions: vec![session("a3").in_repo("gone").build()],
    });
    assert_eq!(
        shown_leaves(&mut h),
        ["a3", "b2"],
        "This PC's list replaced only This PC's"
    );
}

#[gpui::test]
fn one_hosts_repos_list_leaves_the_other_hosts_alone(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, Vec::new());
    h.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });
    h.host(&remote()).send(DaemonMessage::Repos {
        repos: vec![repo("r2", "C:/r2")],
    });
    let keys = |h: &mut Harness<'_>| -> Vec<String> {
        shown_folds(h)
            .into_iter()
            .map(|(key, _)| key)
            .filter(|key| key.contains("repo:"))
            .collect()
    };
    assert_eq!(
        keys(&mut h),
        ["repo:r1", format!("{REMOTE}::repo:r2").as_str()]
    );

    h.host(&remote())
        .send(DaemonMessage::Repos { repos: Vec::new() });

    assert_eq!(keys(&mut h), ["repo:r1"], "This PC's repo stays");
}

#[gpui::test]
fn a_session_message_routes_to_the_host_that_owns_the_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, vec![session("b1").in_repo("gone").build()]);
    h.host(&remote()).send(DaemonMessage::AgentExited {
        session_id: "b1".to_owned(),
    });
    let id = h.root(|root, _| root.toasts()[0].id);

    h.click_on(&toast_action_selector(id, "close-session"));

    let remote_sent = h.host(&remote()).sent();
    assert!(
        matches!(remote_sent.as_slice(), [
            ClientMessage::StopSession { session_id: stopped, .. },
            ClientMessage::DiscardSession { session_id: discarded, .. },
        ] if stopped == "b1" && discarded == "b1"),
        "the remote session stops on its own host's connection: {remote_sent:?}"
    );
    let local_sent = h.sent();
    assert!(
        local_sent.is_empty(),
        "nothing goes to This PC: {local_sent:?}"
    );
}

#[gpui::test]
fn a_remote_welcome_resets_only_that_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, vec![session("b1").in_repo("gone").build()]);

    h.host(&remote()).send(welcome(PROTOCOL));

    assert!(
        !h.root(|root, _| root.agents().is_empty()),
        "This PC's agents stay"
    );
    assert_eq!(
        shown_leaves(&mut h),
        ["a1", "b1"],
        "both hosts' sessions stay"
    );
}

#[gpui::test]
fn a_remote_disconnect_keeps_its_sessions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, vec![session("b1").in_repo("gone").build()]);

    h.host(&remote())
        .event(NetEvent::State(Connection::for_target(remote())));

    assert_eq!(
        shown_leaves(&mut h),
        ["a1", "b1"],
        "the offline host's sessions stay as they last stood"
    );
    assert!(
        !h.root(|root, _| root.agents().is_empty()),
        "This PC's agents stay"
    );
}

#[gpui::test]
fn each_host_keeps_its_own_agent_descriptors(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, Vec::new());

    h.host(&remote())
        .send(DaemonMessage::Agents { agents: Vec::new() });

    assert!(
        !h.root(|root, _| root.agents().is_empty()),
        "a remote host's agent list does not replace This PC's"
    );
}

#[gpui::test]
fn a_remote_keep_awake_status_leaves_this_pcs_alone(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, Vec::new());
    h.send(DaemonMessage::KeepAwakeStatus {
        enabled: true,
        mode: KeepAwakeMode::Always,
        active: true,
    });

    h.host(&remote()).send(DaemonMessage::KeepAwakeStatus {
        enabled: false,
        mode: KeepAwakeMode::Off,
        active: false,
    });

    assert_eq!(
        h.root(|root, _| root.keep_awake_mode()),
        Some(KeepAwakeMode::Always),
        "This PC's keep-awake mode stays its own"
    );
}

#[gpui::test]
fn a_remote_remote_status_leaves_this_pcs_alone(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, Vec::new());
    h.send(DaemonMessage::RemoteStatus {
        enabled: true,
        port: 8787,
        state: RemoteState::Starting,
    });
    let before = h.root(|root, _| (root.remote_status_line(), root.remote_toggle()));

    h.host(&remote()).send(DaemonMessage::RemoteStatus {
        enabled: false,
        port: 9999,
        state: RemoteState::Off,
    });

    assert_eq!(
        h.root(|root, _| (root.remote_status_line(), root.remote_toggle())),
        before,
        "This PC's remote access status stays its own"
    );
}

#[gpui::test]
fn a_remote_agent_settings_list_leaves_this_pcs_settings_page_alone(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, Vec::new());
    let agent = AgentId("test".to_owned());
    let local_settings = BTreeMap::from([(
        agent.clone(),
        BTreeMap::from([("sandbox".to_owned(), OptionValue::Bool(true))]),
    )]);
    h.send(DaemonMessage::AgentSettings {
        settings: local_settings.clone(),
    });

    h.host(&remote()).send(DaemonMessage::AgentSettings {
        settings: BTreeMap::from([(agent, BTreeMap::new())]),
    });

    assert_eq!(
        h.root(|root, _| root.agent_settings().clone()),
        local_settings,
        "This PC's Settings values stay its own"
    );
}

#[gpui::test]
fn a_remote_sessions_menu_offers_what_a_remote_host_allows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(
        cx,
        &dir,
        &Fixture::single(session("a1").worktree("r1").build()),
    );
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").worktree("r2").build()],
    });
    h.sent();
    let reveal_tip = |h: &mut Harness<'_>, leaf: &str| {
        h.right_click_on(leaf);
        let tip = h.root(|root, _| root.menu_disabled_tip("session-menu-reveal"));
        h.keys("escape");
        tip
    };

    assert!(
        reveal_tip(&mut h, "leaf-b1").is_some(),
        "the remote session's Reveal is refused: its folder is not on this PC"
    );
    assert_eq!(
        reveal_tip(&mut h, "leaf-a1"),
        None,
        "This PC's session keeps Reveal"
    );
}

/// The point 30% across the row `selector`, `fy` down its height.
fn on_row(h: &mut Harness<'_>, selector: &str, fy: f32) -> Point<Pixels> {
    let row = h.bounds(selector);
    assert!(row.size.height > Pixels::ZERO, "{selector} is on screen");
    point(
        row.origin.x + row.size.width * 0.3,
        row.origin.y + row.size.height * fy,
    )
}

/// The container orders among `sent`.
fn container_orders(sent: Vec<ClientMessage>) -> Vec<Vec<ContainerRef>> {
    sent.into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ReorderContainers { ordered } => Some(ordered),
            _ => None,
        })
        .collect()
}

fn repo_ref(id: &str) -> ContainerRef {
    ContainerRef::Repo(id.to_owned())
}

#[gpui::test]
fn dragging_containers_sends_each_host_only_its_own_order(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![repo("r1", "D:/r1"), repo("r2", "D:/r2")],
        tabs: vec![tab("t1", &pane("p1", None))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r3", "D:/r3"), repo("r4", "D:/r4")],
    });
    h.sent();
    h.host(&remote()).sent();

    let from = on_row(&mut h, "container-repo:r2", 0.5);
    let to = on_row(&mut h, "container-repo:r1", 0.2);
    h.start_drag(from);
    h.drag_to(to);
    h.release(to);

    assert_eq!(
        container_orders(h.sent()),
        [vec![repo_ref("r2"), repo_ref("r1")]],
        "This PC gets only its own containers, in the new order"
    );
    assert_eq!(
        container_orders(h.host(&remote()).sent()),
        Vec::<Vec<ContainerRef>>::new(),
        "the remote host's order did not change: it is sent nothing"
    );
}

#[gpui::test]
fn a_remote_shell_folder_is_scanned_on_its_own_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(
        cx,
        &dir,
        &Fixture::single(session("a1").in_repo("gone").build()),
    );
    h.sent();
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").dir_shell("D:/remote/dir").build()],
    });
    let scans = |sent: Vec<ClientMessage>| -> Vec<String> {
        sent.into_iter()
            .filter_map(|msg| match msg {
                ClientMessage::ScanVscodeWorkspaces { path } => Some(path),
                _ => None,
            })
            .collect()
    };

    assert_eq!(
        scans(h.host(&remote()).sent()),
        ["D:\\remote\\dir"],
        "the remote folder is scanned on the remote host"
    );
    assert!(scans(h.sent()).is_empty(), "This PC is not asked");
}

/// The `.code-workspace` file named `name` in `D:/same/dir`.
fn dir_suggestion(name: &str) -> VscodeWorkspaceSuggestion {
    VscodeWorkspaceSuggestion {
        source_path: format!("D:/same/dir/{name}.code-workspace"),
        suggested_name: name.to_owned(),
        folders: vec![VscodeWorkspaceFolder {
            path: "D:/same/dir".to_owned(),
            name: None,
            matched_repo_id: None,
        }],
    }
}

#[gpui::test]
fn two_hosts_with_the_same_folder_each_get_their_own_vscode_scan(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(
        cx,
        &dir,
        &Fixture::single(session("a1").dir_shell("D:/same/dir").build()),
    );
    h.sent();
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").dir_shell("D:/same/dir").build()],
    });
    let scans = |sent: Vec<ClientMessage>| -> Vec<String> {
        sent.into_iter()
            .filter_map(|msg| match msg {
                ClientMessage::ScanVscodeWorkspaces { path } => Some(path),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        scans(h.host(&remote()).sent()),
        ["D:\\same\\dir"],
        "the remote host scans its own folder at the same path"
    );

    h.send(DaemonMessage::VscodeWorkspacesScanned {
        path: "D:\\same\\dir".to_owned(),
        suggestions: vec![dir_suggestion("ours")],
    });
    h.host(&remote())
        .send(DaemonMessage::VscodeWorkspacesScanned {
            path: "D:\\same\\dir".to_owned(),
            suggestions: vec![dir_suggestion("theirs")],
        });

    let found = |h: &mut Harness<'_>, host: HostKey| {
        h.root(move |root, _| {
            root.cwd_workspace_suggestion_on(&host, "D:/same/dir")
                .map(|suggestion| suggestion.suggested_name.clone())
        })
    };
    assert_eq!(found(&mut h, HostKey::Local).as_deref(), Some("ours"));
    assert_eq!(
        found(&mut h, HostKey::Remote(REMOTE.to_owned())).as_deref(),
        Some("theirs"),
        "each host keeps its own suggestion"
    );
}

#[gpui::test]
fn removing_a_remote_repo_lists_its_own_hosts_sessions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, vec![session("b1").in_repo("r2").build()]);
    h.host(&remote()).send(DaemonMessage::Repos {
        repos: vec![repo("r2", "C:/r2")],
    });
    h.sent();
    h.host(&remote()).sent();

    h.right_click_on(&format!("container-{REMOTE}::repo:r2"));
    h.sent();
    h.host(&remote()).sent();
    h.click_on("container-menu-remove");

    assert_eq!(
        h.root(|root, _| root.repo_remove_session_rows()),
        ["b1 (claude)"],
        "the dialog lists the remote host's live session"
    );
    h.click_on("repo-remove-stop-and-remove");
    let remote_sent = h.host(&remote()).sent();
    assert!(
        matches!(remote_sent.as_slice(), [
            ClientMessage::StopSession { session_id, .. },
            ClientMessage::RemoveRepo { repo_id },
        ] if session_id == "b1" && repo_id == "r2"),
        "the stop and the removal go to the remote host: {remote_sent:?}"
    );
    let local_sent = h.sent();
    assert!(
        local_sent.is_empty(),
        "nothing goes to This PC: {local_sent:?}"
    );
}

#[gpui::test]
fn a_remote_session_asking_raises_a_notification_and_a_needs_you_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(
        cx,
        &dir,
        vec![
            session("b1")
                .label("")
                .agent("test-agent")
                .status("awaiting_input")
                .in_repo("gone")
                .build(),
        ],
    );
    let mut agents = agent_registry::descriptors();
    agents.push(agent_api::test_agent::DESCRIPTOR.clone());
    h.host(&remote()).send(DaemonMessage::Agents { agents });

    h.host(&remote()).send(DaemonMessage::Attention {
        session_id: "b1".to_owned(),
        reason: AttentionReason::AwaitingInput,
    });

    assert_eq!(
        h.notified(),
        [("Needs you".to_owned(), "test: Waiting for input".to_owned())],
        "the toast names the session by its own host's agent"
    );
    let rows: Vec<(String, String)> = h.root(|root, _| {
        root.needs_you_rows()
            .into_iter()
            .map(|row| (row.session_id, row.label))
            .collect()
    });
    assert_eq!(
        rows,
        [("b1".to_owned(), "test".to_owned())],
        "Needs You lists the remote session"
    );
}

/// Whether `sent` holds a `WatchDashboard` turning the watch on.
fn watches(sent: &[ClientMessage]) -> bool {
    sent.iter()
        .any(|msg| matches!(msg, ClientMessage::WatchDashboard { on: true }))
}

#[gpui::test]
fn the_dashboard_watches_every_host_and_lists_both_hosts_rows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let dashboard: TabEntry = serde_json::from_value(serde_json::json!({
        "id": "d1",
        "name": "Dashboard",
        "content": { "kind": "dashboard" },
        "created_at": "2026-01-01T00:00:00Z",
    }))
    .expect("dashboard tab fixture");
    let fixture = Fixture {
        sessions: vec![session("a1").in_repo("gone").status("working").build()],
        tabs: vec![tab("t1", &pane("p1", Some("a1"))), dashboard],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    h.click_on("tab-d1");
    assert!(watches(&h.sent()), "This PC is watched");

    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").in_repo("gone").status("working").build()],
    });
    h.host(&remote())
        .send(facts_message(vec![facts("b1").context(90).build()]));

    let rows: Vec<(String, String)> = h.root(|root, _| {
        root.dashboard_view_rows()
            .into_iter()
            .flat_map(|(_, rows)| rows)
            .map(|row| (row.session_id, row.context))
            .collect()
    });
    let ids: Vec<&str> = rows.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, ["a1", "b1"], "both hosts' rows show together");
    assert_ne!(
        rows[1].1, "—",
        "a host that joins while the Dashboard shows is watched once it welcomes"
    );

    h.click_on("tab-t1");
    assert!(
        h.host(&remote())
            .sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::WatchDashboard { on: false })),
        "leaving the Dashboard ends the remote host's watch too"
    );
}

/// A daemon status for `(repo_id, worktree)` with `changes` unstaged.
fn repo_status(repo_id: &str, worktree: &str, changes: &[&str]) -> DaemonMessage {
    let files: Vec<serde_json::Value> = changes
        .iter()
        .map(|path| serde_json::json!({ "path": path, "status": "M", "from_path": null }))
        .collect();
    serde_json::from_value(serde_json::json!({
        "type": "repo_status",
        "repo_id": repo_id,
        "index_changes": [],
        "worktree_changes": files,
        "worktree_path": worktree,
    }))
    .expect("status fixture")
}

/// The repos of the status requests among `sent`.
fn status_repos(sent: Vec<ClientMessage>) -> Vec<String> {
    sent.into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::RepoStatus { repo_id, .. } => Some(repo_id),
            _ => None,
        })
        .collect()
}

/// The source-control panel's sections as `(id, count)`.
fn sc_sections(h: &mut Harness<'_>) -> Vec<(String, Option<usize>)> {
    h.root(|root, _| {
        root.source_control_panel()
            .sections
            .into_iter()
            .map(|section| (section.id, section.count))
            .collect()
    })
}

#[gpui::test]
fn the_source_control_panel_follows_the_focused_sessions_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        repos: vec![repo("r1", "D:/src/r1")],
        sessions: vec![session("a1").worktree("r1").build()],
        tabs: vec![
            tab("t1", &pane("p1", Some("a1"))),
            tab("t2", &pane("p2", Some("b1"))),
        ],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r2", "D:/src/r2")],
    });
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").worktree("r2").build()],
    });
    h.click_on("activity-source-control");
    h.sent();
    h.host(&remote()).sent();
    assert_eq!(
        sc_sections(&mut h),
        [("r1::C:/wt/x".to_owned(), None)],
        "This PC's session shows This PC's tree"
    );

    h.click_on("tab-t2");

    assert_eq!(
        sc_sections(&mut h),
        [("r2::C:/wt/x".to_owned(), None)],
        "the remote session shows its own host's tree"
    );
    assert!(
        status_repos(h.host(&remote()).sent()).contains(&"r2".to_owned()),
        "the remote tree's status is asked of the remote host"
    );
    assert!(
        status_repos(h.sent()).is_empty(),
        "This PC is asked for nothing"
    );
    h.host(&remote())
        .send(repo_status("r2", "C:/wt/x", &["a.rs"]));
    assert_eq!(sc_sections(&mut h), [("r2::C:/wt/x".to_owned(), Some(1))]);

    h.click_on("tab-t1");
    assert_eq!(sc_sections(&mut h), [("r1::C:/wt/x".to_owned(), None)]);
    h.host(&remote())
        .send(repo_status("r2", "C:/wt/x", &["a.rs", "b.rs"]));
    h.click_on("tab-t2");

    assert_eq!(
        sc_sections(&mut h),
        [("r2::C:/wt/x".to_owned(), Some(2))],
        "a status that came while the host was not shown is kept for it"
    );
    assert!(
        status_repos(h.host(&remote()).sent()).is_empty(),
        "showing the host again asks for nothing it holds"
    );
}

/// A fetch of `rel` from repo `repo_id`.
fn fetch_request(repo_id: &str, rel: &str) -> FetchRequest {
    FetchRequest {
        repo_id: repo_id.to_owned(),
        repo_name: "repo".to_owned(),
        worktree_path: None,
        rel_path: rel.to_owned(),
        line: None,
        column: None,
        open_when_done: false,
    }
}

/// The ids of the fetches among `sent`.
fn fetch_ids(sent: Vec<ClientMessage>) -> Vec<String> {
    sent.into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::FetchFile { id, .. } => Some(id),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn a_remote_hosts_disconnect_ends_only_its_fetches(cx: &mut TestAppContext) {
    const HOME: &str = "home.tail1234.ts.net";
    let dir = TestDir::new();
    let home = Target::tailnet(HOME, None);
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let root = dir.path().join("remote-files");
    h.root_update(move |view, _, _| view.set_remote_files_root(root));
    let mut host = h.host(&home);
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });
    host.sent();
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r2", "C:/r2")],
    });
    h.sent();
    h.root_update(|view, _, cx| view.fetch_remote_file(fetch_request("r1", "a.txt"), cx));
    h.root_update(|view, _, cx| view.fetch_remote_file(fetch_request("r2", "b.txt"), cx));
    let home_fetch = fetch_ids(h.host(&home).sent());
    let remote_fetch = fetch_ids(h.host(&remote()).sent());
    assert_eq!(
        (home_fetch.len(), remote_fetch.len()),
        (1, 1),
        "each fetch goes to the host that holds its repo"
    );

    h.host(&remote())
        .event(NetEvent::State(Connection::for_target(remote())));

    let failed: Vec<String> = h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| toast.title.clone())
            .filter(|title| title.starts_with("Couldn't fetch"))
            .collect()
    });
    assert_eq!(
        failed,
        ["Couldn't fetch b.txt"],
        "only the remote host's fetch ends"
    );
    h.root_update(|view, _, cx| view.fetch_remote_file(fetch_request("r1", "a.txt"), cx));
    assert!(
        h.root(|root, _| root
            .toasts()
            .iter()
            .any(|toast| toast.title == "Already fetching a.txt")),
        "the other host's fetch still runs"
    );
}

#[gpui::test]
fn container_keys_of_two_hosts_do_not_collide(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, vec![session("b1").in_repo("gone").build()]);
    let remote_detached = format!("{REMOTE}::detached");
    assert_eq!(
        shown_folds(&mut h),
        [
            ("detached".to_owned(), false),
            (remote_detached.clone(), false)
        ],
        "each host has its own Detached row"
    );

    h.click_on("container-detached");

    assert_eq!(
        shown_folds(&mut h),
        [("detached".to_owned(), true), (remote_detached, false)],
        "folding This PC's row leaves the remote host's open"
    );
}

/// Whether `sent` asks for the session history.
fn asks_history(sent: &[ClientMessage]) -> bool {
    sent.iter()
        .any(|msg| matches!(msg, ClientMessage::ListSessionHistory { .. }))
}

/// A host's `RecoverSessions` requests: each one's id and the history ids it
/// names.
type Requests = Vec<(String, Vec<String>)>;

/// The `RecoverSessions` requests among `sent`: each one's id and the
/// history ids it names.
fn recover_requests(sent: Vec<ClientMessage>) -> Requests {
    sent.into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::RecoverSessions { request_id, items } => Some((
                request_id.unwrap_or_default(),
                items.into_iter().map(|item| item.history_id).collect(),
            )),
            _ => None,
        })
        .collect()
}

/// The answer to recovery `request_id`: `history_id` came back as
/// `session_id`.
fn recovered(request_id: &str, history_id: &str, session_id: &str) -> DaemonMessage {
    DaemonMessage::RecoverResult {
        request_id: Some(request_id.to_owned()),
        results: vec![RecoverItemResult {
            history_id: history_id.to_owned(),
            session_id: Some(session_id.to_owned()),
            error: None,
        }],
    }
}

/// The messages among `sent` that put a session in a pane or a tab, each as
/// its session id and the host it names (none for This PC).
fn placed(sent: Vec<ClientMessage>) -> Vec<(String, Option<String>)> {
    sent.into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ReplacePaneSession {
                session_id, host, ..
            } => Some((session_id.unwrap_or_default(), host)),
            ClientMessage::SplitPane {
                new_session_id,
                host,
                ..
            } => Some((new_session_id.unwrap_or_default(), host)),
            ClientMessage::CreateTab {
                initial_session_id,
                host,
                ..
            } => Some((initial_session_id.unwrap_or_default(), host)),
            _ => None,
        })
        .collect()
}

/// An approved device named `name`.
fn device(id: &str, name: &str) -> DeviceInfo {
    DeviceInfo {
        device_id: id.to_owned(),
        name: name.to_owned(),
        login: "alice@example.com".to_owned(),
        created_at: chrono::Utc::now(),
        last_seen: None,
        revoked: false,
        connected: 0,
    }
}

#[gpui::test]
fn the_devices_view_acts_on_this_pc_with_a_remote_host_connected(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let mut host = h.host(&remote());
    host.connect();
    h.send(DaemonMessage::Devices {
        devices: vec![device("d1", "laptop")],
    });
    let ours = h.root(|root, _| root.device_rows());
    assert_eq!(ours.len(), 1, "This PC's device is listed");

    h.host(&remote()).send(DaemonMessage::Devices {
        devices: vec![device("d2", "phone"), device("d3", "tablet")],
    });

    assert_eq!(
        h.root(|root, _| root.device_rows()),
        ours,
        "a remote host's device list is not This PC's"
    );
}

#[gpui::test]
fn a_remote_sessions_popout_is_restored_on_its_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let saved = serde_json::json!({
        "key": { "kind": "session", "session_id": "b1", "host": REMOTE },
        "window": null,
    });
    std::fs::write(
        dir.path().join("native-ui.json"),
        serde_json::json!({ "popouts": [saved] }).to_string(),
    )
    .expect("write native-ui.json");
    let mut h = Harness::open(cx, &dir);
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").build()],
    });

    h.load(&Fixture::single(session("a1").build()));

    assert_eq!(h.window_count(), 2, "the remote session's pop-out reopened");
    h.sent();
    h.host(&remote()).sent();
    let mut popout = h.popout_window();
    popout.simulate_keystrokes("a");
    let typed: Vec<String> = h
        .host(&remote())
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::SendInput { session_id, .. } => Some(session_id),
            _ => None,
        })
        .collect();
    assert_eq!(typed, ["b1"], "its terminal types on the remote host");
    h.advance(std::time::Duration::from_millis(500));
    let text = std::fs::read_to_string(dir.path().join("native-ui.json")).expect("a saved layout");
    let ui: serde_json::Value = serde_json::from_str(&text).expect("native-ui.json is JSON");
    assert_eq!(
        ui["popouts"],
        serde_json::json!([saved]),
        "the saved entry keeps its host"
    );
}

#[gpui::test]
fn recover_lists_each_hosts_history_and_recovers_on_that_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.send(DaemonMessage::SessionHistory {
        request_id: None,
        items: vec![history_item("a1").build()],
    });
    let mut host = h.host(&remote());
    host.connect_keeping_sent();
    assert!(
        asks_history(&host.sent()),
        "a host that says Welcome is asked for its history"
    );
    host.send(DaemonMessage::SessionHistory {
        request_id: None,
        items: vec![history_item("b1").build()],
    });
    h.sent();
    assert_eq!(
        h.root(|root, _| root.recover_badge()).as_deref(),
        Some("2"),
        "the badge counts every host's losses"
    );

    h.click_on("activity-recover");

    assert!(asks_history(&h.sent()), "opening asks This PC");
    assert!(
        asks_history(&h.host(&remote()).sent()),
        "and the remote host"
    );
    let rows: Vec<String> =
        h.root(|root, _| root.recover_rows().into_iter().map(|row| row.id).collect());
    assert_eq!(rows, ["a1", "b1"], "This PC's rows, then the remote host's");
    assert_eq!(
        h.root(|root, _| root.recover_host_headers()),
        ["This PC", "box"],
        "each host's rows sit under its name"
    );

    h.click_on("recover-submit");

    let ours = recover_requests(h.sent());
    let theirs = recover_requests(h.host(&remote()).sent());
    assert_eq!(
        ours.iter().map(|(_, ids)| ids.clone()).collect::<Vec<_>>(),
        [["a1"]],
        "This PC recovers only its own row"
    );
    assert_eq!(
        theirs
            .iter()
            .map(|(_, ids)| ids.clone())
            .collect::<Vec<_>>(),
        [["b1"]],
        "the remote host recovers only its own row"
    );
    h.send(recovered(&ours[0].0, "a1", "n1"));
    assert!(
        h.root(|root, _| root.recover_open()),
        "the dialog waits for the other host's answer"
    );
    h.host(&remote()).send(recovered(&theirs[0].0, "b1", "n2"));
    assert!(
        !h.root(|root, _| root.recover_open()),
        "both hosts answered with nothing failed"
    );
}

/// Opens Recover and presses Recover, returning This PC's requests and the
/// remote host's.
fn recover_both(h: &mut Harness<'_>) -> (Requests, Requests) {
    h.click_on("activity-recover");
    h.sent();
    h.host(&remote()).sent();
    h.click_on("recover-submit");
    let ours = recover_requests(h.sent());
    let theirs = recover_requests(h.host(&remote()).sent());
    (ours, theirs)
}

#[gpui::test]
fn a_recovered_remote_session_is_placed_with_its_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, vec![session("b1").in_repo("r9").build()]);
    h.host(&remote()).send(DaemonMessage::SessionHistory {
        request_id: None,
        items: vec![history_item("b1").build()],
    });
    h.sent();
    h.host(&remote()).sent();

    let (ours, theirs) = recover_both(&mut h);
    assert!(ours.is_empty(), "This PC has no row to recover: {ours:?}");
    assert_eq!(
        theirs
            .iter()
            .map(|(_, ids)| ids.clone())
            .collect::<Vec<_>>(),
        [["b1"]],
        "the remote host recovers its own row"
    );
    h.host(&remote()).send(recovered(&theirs[0].0, "b1", "n2"));
    h.host(&remote()).send(DaemonMessage::SessionUpdated {
        session: session("n2").in_repo("r9").build(),
        request_id: None,
    });
    assert_eq!(
        placed(h.sent()),
        [("n2".to_owned(), Some(REMOTE.to_owned()))],
        "the recovered session opens in This PC's layout, in a pane of its own host"
    );
    assert!(
        placed(h.host(&remote()).sent()).is_empty(),
        "the remote host lays out no panes itself"
    );
}

#[gpui::test]
fn an_already_shown_remote_pane_is_not_placed_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("a1")),
        remote_pane("p2", "n2"),
    );
    let mut h = Harness::with(
        cx,
        &dir,
        &Fixture {
            sessions: vec![session("a1").build()],
            tabs: vec![tab("t1", &grid)],
            ..Fixture::default()
        },
    );
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("n2").build()],
    });
    host.send(DaemonMessage::SessionHistory {
        request_id: None,
        items: vec![history_item("b1").build()],
    });
    host.answer_scrollback("n2", b"");
    h.sent();
    h.host(&remote()).sent();

    let (_, theirs) = recover_both(&mut h);
    h.host(&remote()).send(recovered(&theirs[0].0, "b1", "n2"));
    h.host(&remote()).send(DaemonMessage::SessionUpdated {
        session: session("n2").build(),
        request_id: None,
    });
    assert!(
        placed(h.sent()).is_empty(),
        "a pane of the session's own host already shows it"
    );
}

#[gpui::test]
fn a_recovery_batch_waits_for_its_own_hosts_session(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = Fixture::single(session("a1").build());
    fixture.sessions.push(session("n9").build());
    let mut h = Harness::with(cx, &dir, &fixture);
    h.send(DaemonMessage::SessionHistory {
        request_id: None,
        items: Vec::new(),
    });
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").in_repo("r9").build()],
    });
    host.send(DaemonMessage::SessionHistory {
        request_id: None,
        items: vec![history_item("b1").build()],
    });
    h.sent();
    h.host(&remote()).sent();

    let (_, theirs) = recover_both(&mut h);
    h.host(&remote()).send(recovered(&theirs[0].0, "b1", "n9"));
    assert!(
        placed(h.sent()).is_empty(),
        "This PC's own `n9` is not the session its recovery answered with"
    );
    h.host(&remote()).send(DaemonMessage::SessionUpdated {
        session: session("n9").in_repo("r9").build(),
        request_id: None,
    });
    assert_eq!(
        placed(h.sent()),
        [("n9".to_owned(), Some(REMOTE.to_owned()))],
        "the batch places once its own host's snapshot arrives"
    );
}

#[gpui::test]
fn recover_names_a_remote_rows_repo_from_its_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("a1").build()));
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r7", "C:/repos/r7")],
    });
    host.send(DaemonMessage::SessionHistory {
        request_id: None,
        items: vec![
            history_item("b1")
                .folder_only("C:/src/proj", true, Some("r7"))
                .shell_resume(true)
                .build(),
        ],
    });
    h.sent();
    h.host(&remote()).sent();

    h.click_on("activity-recover");
    h.sent();
    h.host(&remote()).sent();
    let rows = h.root(|root, _| root.recover_rows());
    let row = rows
        .iter()
        .find(|row| row.id == "b1")
        .expect("the remote host's row");
    assert_eq!(
        row.recover_as.as_deref(),
        Some("Claude session in r7"),
        "the row's repo is named from the registry of the host that lists it"
    );
}

#[gpui::test]
fn a_remote_reconnect_clears_only_its_recoveries(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, vec![session("b1").in_repo("gone").build()]);
    h.send(DaemonMessage::SessionHistory {
        request_id: None,
        items: vec![history_item("a1").build()],
    });
    h.host(&remote()).send(DaemonMessage::SessionHistory {
        request_id: None,
        items: vec![history_item("b1").build()],
    });
    h.sent();
    h.host(&remote()).sent();

    let (ours, theirs) = recover_both(&mut h);
    assert_eq!(ours.len(), 1, "This PC recovers its row");
    assert_eq!(theirs.len(), 1, "the remote host recovers its own");

    h.host(&remote()).event(NetEvent::State(dropped(remote())));
    assert!(
        h.root(|root, _| root.recover_open()),
        "the dialog stays open for the host still connected"
    );
    h.send(recovered(&ours[0].0, "a1", "n1"));
    assert!(
        !h.root(|root, _| root.recover_open()),
        "the host that went is not waited for"
    );
    h.send(DaemonMessage::SessionUpdated {
        session: session("n1").build(),
        request_id: None,
    });
    assert_eq!(
        placed(h.sent()),
        [("n1".to_owned(), None)],
        "This PC's own recovery still places, in a This PC pane"
    );
}

#[gpui::test]
fn this_pcs_recovery_is_unchanged_with_a_host_connected(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, vec![session("b1").in_repo("gone").build()]);
    h.send(DaemonMessage::SessionHistory {
        request_id: None,
        items: vec![history_item("a1").build()],
    });
    h.sent();
    h.host(&remote()).sent();

    let (ours, theirs) = recover_both(&mut h);
    assert_eq!(
        ours.iter().map(|(_, ids)| ids.clone()).collect::<Vec<_>>(),
        [["a1"]],
        "This PC recovers its own row"
    );
    assert!(
        theirs.is_empty(),
        "the connected host has no row to recover: {theirs:?}"
    );
    h.send(recovered(&ours[0].0, "a1", "n1"));
    assert!(!h.root(|root, _| root.recover_open()), "nothing failed");
    h.send(DaemonMessage::SessionUpdated {
        session: session("n1").build(),
        request_id: None,
    });
    assert_eq!(
        placed(h.sent()),
        [("n1".to_owned(), None)],
        "the recovered session opens in a This PC pane"
    );
    assert!(
        placed(h.host(&remote()).sent()).is_empty(),
        "the other host is sent no placement"
    );
}

#[gpui::test]
fn a_remote_cleanup_failure_retries_on_that_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, vec![session("b1").in_repo("gone").build()]);
    h.host(&remote()).sent();
    // The daemon removes the session before it reports the failure, so
    // the client no longer holds the session it names.
    h.host(&remote())
        .send(DaemonMessage::WorktreeCleanupFailed {
            session_id: "b9".to_owned(),
            session_label: "feat/x".to_owned(),
            failures: vec![WorktreeCleanupFailure {
                member_path: "C:/wts/wt.feat-x/a".to_owned(),
                repo_path: "C:/src/a".to_owned(),
                reason: "fs::remove_dir_all: access denied".to_owned(),
                locking_processes: vec![WorktreeLockingProcess {
                    pid: 10,
                    name: "node.exe".to_owned(),
                    cmdline: None,
                }],
            }],
        });

    h.click_on("cleanup-retry");

    let retries = |sent: Vec<ClientMessage>| -> Vec<(String, Vec<u32>)> {
        sent.into_iter()
            .filter_map(|msg| match msg {
                ClientMessage::RetryWorktreeCleanup {
                    session_id,
                    kill_pids,
                    ..
                } => Some((session_id, kill_pids)),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        retries(h.host(&remote()).sent()),
        [("b9".to_owned(), vec![10])],
        "the retry goes to the host that reported the failure"
    );
    assert!(
        retries(h.sent()).is_empty(),
        "This PC kills nothing for a remote host's failure"
    );
}

/// The commit input of This PC's r1 main tree.
const COMMIT_INPUT: &str = "sc-commit-input-r1::";

/// `repo_id`'s main-tree status with `a.rs` staged.
fn staged_status(repo_id: &str) -> DaemonMessage {
    serde_json::from_value(serde_json::json!({
        "type": "repo_status",
        "repo_id": repo_id,
        "index_changes": [{ "path": "a.rs", "status": "M", "from_path": null }],
        "worktree_changes": [],
        "worktree_path": null,
    }))
    .expect("status fixture")
}

/// This PC's panel on r1 with `a.rs` staged and `fix` typed into its
/// commit input, a pane beside it that could take the keyboard, and the
/// tailnet host connected with r2, which the panel does not show.
fn this_pc_drafting<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut fixture = Fixture::single(session("a1").build());
    fixture.repos = vec![repo("r1", "D:/src/r1")];
    let mut h = Harness::with(cx, dir, &fixture);
    h.answer_scrollback("a1", b"");
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r2", "D:/src/r2")],
    });
    h.click_on("activity-source-control");
    h.send(staged_status("r1"));
    h.click_on(COMMIT_INPUT);
    h.keys("f i x");
    h.sent();
    h.host(&remote()).sent();
    h
}

fn this_pc_draft(h: &mut Harness<'_>) -> Option<String> {
    let key = ScKey {
        repo_id: "r1".to_owned(),
        worktree: None,
    };
    h.root(move |root, cx| root.sc_commit_input(&key, cx))
}

#[gpui::test]
fn a_parked_hosts_status_leaves_the_commit_box_focused(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = this_pc_drafting(cx, &dir);

    h.host(&remote()).send(staged_status("r2"));
    h.keys("y");

    assert_eq!(
        this_pc_draft(&mut h).as_deref(),
        Some("fixy"),
        "typing still goes into This PC's commit box"
    );
}

#[gpui::test]
fn a_parked_hosts_write_error_keeps_this_pcs_commit_draft(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = this_pc_drafting(cx, &dir);

    h.host(&remote()).send(DaemonMessage::GitWriteError {
        repo_id: "r2".to_owned(),
        operation: "commit".to_owned(),
        error: "git commit failed: pre-commit hook rejected".to_owned(),
        worktree_path: None,
    });

    assert_eq!(
        this_pc_draft(&mut h).as_deref(),
        Some("fix"),
        "This PC's draft is left as typed"
    );
}

/// The folders among `sent`'s VS Code workspace scans.
fn scan_paths(sent: Vec<ClientMessage>) -> Vec<String> {
    sent.into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::ScanVscodeWorkspaces { path } => Some(path),
            _ => None,
        })
        .collect()
}

#[gpui::test]
fn a_remote_hosts_pending_scan_is_asked_again_after_it_reconnects(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let shell = || vec![session("b1").dir_shell("D:/remote/dir").build()];
    let mut h = two_hosts(cx, &dir, shell());
    assert_eq!(
        scan_paths(h.host(&remote()).sent()),
        ["D:\\remote\\dir"],
        "the folder is scanned once; its answer never comes"
    );

    h.host(&remote())
        .event(NetEvent::State(Connection::for_target(remote())));
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Sessions { sessions: shell() });

    assert_eq!(
        scan_paths(h.host(&remote()).sent()),
        ["D:\\remote\\dir"],
        "the reconnected host is asked again"
    );
}

/// A second tailnet host the launch specs save.
const OTHER: &str = "den.tail1234.ts.net";

#[gpui::test]
fn launch_connects_this_pc_and_every_enabled_saved_host_in_saved_order(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &serde_json::json!({ "saved_hosts": [
            { "name": OTHER, "port": 9443 },
            { "kind": "ssh", "name": "me@devbox" },
            { "name": REMOTE, "port": 8787 },
        ] }),
    );
    let h = Harness::open(cx, &dir);

    assert_eq!(
        h.opened_targets(),
        [
            Target::Local,
            Target::tailnet(OTHER, Some(9443)),
            Target::parse_ssh("me@devbox", None).expect("an SSH host string"),
            Target::tailnet(REMOTE, Some(8787)),
        ],
        "This PC first, then each saved host in saved order, on its saved port"
    );
}

#[gpui::test]
fn launch_skips_a_disabled_saved_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &serde_json::json!({ "saved_hosts": [
            { "name": OTHER, "port": 8787, "enabled": false },
            { "name": REMOTE, "port": 8787 },
        ] }),
    );
    let h = Harness::open(cx, &dir);

    assert_eq!(
        h.opened_targets(),
        [Target::Local, Target::tailnet(REMOTE, Some(8787))],
        "a host switched off is not dialled"
    );
}

#[gpui::test]
fn launch_with_an_unpaired_saved_host_opens_no_pairing_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &serde_json::json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::open(cx, &dir);
    let mut refused = Connection::for_target(Target::tailnet(REMOTE, Some(8787)));
    refused.on_connecting();
    refused.on_socket_open(8787);
    refused.on_not_approved(protocol::DEVICE_NOT_APPROVED.to_owned(), false);

    let mut host = h.host(&Target::tailnet(REMOTE, Some(8787)));
    host.event(NetEvent::State(refused));
    assert!(host.commands().is_empty(), "nothing asks the host to pair");
    assert!(
        !h.root(|root, _| root.device_pairing_open()),
        "a host that does not know this PC opens nothing at launch"
    );
}

#[gpui::test]
fn a_remote_sessions_saved_popout_reopens_once_its_host_connects_at_launch(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let saved = serde_json::json!({
        "key": { "kind": "session", "session_id": "b1", "host": REMOTE },
        "window": null,
    });
    let unsaved_host = serde_json::json!({
        "key": { "kind": "session", "session_id": "c1", "host": OTHER },
        "window": null,
    });
    seed_ui(
        &dir,
        &serde_json::json!({
            "saved_hosts": [{ "name": REMOTE, "port": 8787 }],
            "popouts": [saved, unsaved_host],
        }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("a1").build()));
    h.advance(std::time::Duration::from_millis(500));
    assert_eq!(h.window_count(), 1, "the host's list has not arrived");
    assert_eq!(
        saved_ui(&dir)["popouts"],
        serde_json::json!([saved]),
        "kept while its host connects; one whose host is not saved is dropped at once"
    );

    let mut host = h.host(&Target::tailnet(REMOTE, Some(8787)));
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").build()],
    });

    assert_eq!(
        h.window_count(),
        2,
        "the pop-out reopened on its host's list"
    );
    h.advance(std::time::Duration::from_millis(500));
    assert_eq!(
        saved_ui(&dir)["popouts"],
        serde_json::json!([saved]),
        "the saved entry stays as it was"
    );
}

/// The daemon id the dedupe specs give a daemon reached twice.
const DAEMON: &str = "6f1e2d3c-0000-4000-8000-000000000001";

fn ssh_box() -> Target {
    Target::parse_ssh("me@devbox", None).expect("an SSH host string")
}

/// Opens `target`'s connection beside This PC's and has its daemon say
/// `Welcome`, naming itself `id` when there is one.
fn welcome_as(h: &mut Harness<'_>, target: &Target, id: Option<&str>) {
    let mut conn = Connection::for_target(target.clone());
    conn.on_connecting();
    conn.on_socket_open(8787);
    conn.on_welcome(PROTOCOL, support::daemon_features());
    let mut host = h.host(target);
    host.event(NetEvent::State(conn));
    host.send(id.map_or_else(
        || welcome(PROTOCOL),
        |id| support::welcome_with_id(PROTOCOL, id),
    ));
}

fn duplicate_of(h: &mut Harness<'_>, target: &Target) -> Option<HostKey> {
    let key = HostKey::of(target);
    h.root(move |root, _| root.duplicate_of(&key).cloned())
}

#[gpui::test]
fn a_second_host_on_the_same_daemon_is_dropped_and_named(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("a1").build()));
    welcome_as(&mut h, &remote(), Some(DAEMON));
    h.host(&remote()).send(DaemonMessage::Sessions {
        sessions: vec![session("b1").build()],
    });

    welcome_as(&mut h, &ssh_box(), Some(DAEMON));
    h.host(&ssh_box()).send(DaemonMessage::Sessions {
        sessions: vec![session("b1").build()],
    });

    assert_eq!(
        duplicate_of(&mut h, &ssh_box()),
        Some(HostKey::of(&remote())),
        "the later host names the one that reached the daemon first"
    );
    assert_eq!(duplicate_of(&mut h, &remote()), None);
    assert_eq!(
        shown_leaves(&mut h),
        ["a1", "b1"],
        "the daemon's session shows once"
    );
    assert_eq!(
        h.root(|root, _| root.held_hosts()),
        [HostKey::Local, HostKey::of(&remote())],
        "the duplicate's connection is gone"
    );
}

#[gpui::test]
fn a_remote_alias_of_this_pc_is_dropped_even_when_it_welcomes_first(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    welcome_as(&mut h, &remote(), Some(DAEMON));
    h.host(&remote()).send(DaemonMessage::Sessions {
        sessions: vec![session("a1").build()],
    });

    h.send(support::welcome_with_id(PROTOCOL, DAEMON));
    h.send(DaemonMessage::Sessions {
        sessions: vec![session("a1").build()],
    });

    assert_eq!(
        duplicate_of(&mut h, &remote()),
        Some(HostKey::Local),
        "This PC owns its own daemon"
    );
    assert_eq!(shown_leaves(&mut h), ["a1"], "This PC's session shows once");
}

#[gpui::test]
fn a_not_approved_duplicate_is_dropped_before_pairing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.send(support::welcome_with_id(PROTOCOL, DAEMON));
    let mut refused = Connection::for_target(remote());
    refused.on_connecting();
    refused.on_socket_open(8787);
    refused.on_not_approved(protocol::DEVICE_NOT_APPROVED.to_owned(), false);

    // What the network thread sends for a tailnet host whose held Welcome
    // is followed by "device not approved".
    let mut host = h.host(&remote());
    host.event(NetEvent::DaemonId(DAEMON.to_owned()));
    host.event(NetEvent::State(refused));
    host.send(DaemonMessage::DevicePairFailed {
        reason: protocol::DEVICE_NOT_APPROVED.to_owned(),
    });

    assert_eq!(duplicate_of(&mut h, &remote()), Some(HostKey::Local));
    assert_eq!(
        h.root(|root, _| root.held_hosts()),
        [HostKey::Local],
        "the duplicate's connection is dropped before its row offers Pair"
    );
    assert!(
        !h.root(|root, _| root.device_pairing_open()),
        "no pairing for a daemon already connected"
    );
}

#[gpui::test]
fn a_welcome_without_a_daemon_id_is_never_a_duplicate(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("a1").build()));
    welcome_as(&mut h, &remote(), None);
    h.host(&remote()).send(DaemonMessage::Sessions {
        sessions: vec![session("b1").build()],
    });
    welcome_as(&mut h, &ssh_box(), None);
    h.host(&ssh_box()).send(DaemonMessage::Sessions {
        sessions: vec![session("c1").build()],
    });

    assert_eq!(duplicate_of(&mut h, &remote()), None);
    assert_eq!(duplicate_of(&mut h, &ssh_box()), None);
    assert_eq!(shown_leaves(&mut h), ["a1", "b1", "c1"]);
}

#[gpui::test]
fn a_paired_event_from_another_host_leaves_the_dialog_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &serde_json::json!({ "saved_hosts": [
            { "name": REMOTE, "port": 8787 },
            { "name": OTHER, "port": 8787 },
        ] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let other = Target::tailnet(OTHER, None);
    h.host(&other).connect();
    let mut refused = Connection::for_target(remote());
    refused.on_connecting();
    refused.on_socket_open(8787);
    refused.on_not_approved(protocol::DEVICE_NOT_APPROVED.to_owned(), false);
    h.host(&remote()).event(NetEvent::State(refused));
    h.root_update(rustling_tulip_native::RootView::open_connection_picker);
    h.click_on(&format!("connections-row-{REMOTE}-pair"));
    assert!(h.root(|root, _| root.device_pairing_open()));

    h.host(&other).event(NetEvent::Paired);

    assert!(
        h.root(|root, _| root.device_pairing_open()),
        "another host's pairing never ends this one's dialog"
    );
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

/// This PC's connection open on the fake daemon's port and welcomed.
fn this_pc_open() -> Connection {
    let mut conn = Connection::for_target(Target::Local);
    conn.on_connecting();
    conn.on_socket_open(4242);
    conn.on_welcome(PROTOCOL, support::daemon_features());
    conn
}

#[gpui::test]
fn same_target_reconnect_keeps_the_rows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &r1_fixture());

    let mut dropped = this_pc_open();
    let _ = dropped.on_closed("socket closed".to_owned());
    h.set_connection(dropped);
    h.set_connection(this_pc_open());
    assert_eq!(
        shown_leaves(&mut h),
        ["s1"],
        "the rows stay until the daemon's lists"
    );
    assert_eq!(h.root(|root, _| root.tab_ids()), ["t1"]);
    assert_eq!(h.root(|root, _| root.active_pane_ids()), ["p1"]);
}

#[gpui::test]
fn quit_sends_shutdown_only_to_this_pc(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &r1_fixture());
    h.host(&remote()).connect();
    h.sent();
    let closed = h.cx.simulate_close();
    h.cx.run_until_parked();
    assert!(!closed, "an active session keeps the window open");
    assert!(h.root(|root, _| root.exit_dialog_open()));
    h.commands();
    h.host(&remote()).commands();

    h.click_on("exit-stop-keep-worktrees");

    let local = h.commands();
    assert!(
        local
            .iter()
            .any(|command| matches!(command, NetCommand::Shutdown { .. } | NetCommand::Stop)),
        "This PC's daemon is stopped: {local:?}"
    );
    let remote_commands = h.host(&remote()).commands();
    assert!(
        !remote_commands
            .iter()
            .any(|command| matches!(command, NetCommand::Shutdown { .. } | NetCommand::Stop)),
        "the remote host's daemon is neither shut down nor stopped: {remote_commands:?}"
    );
}

/// The remote host's repo row, keyed with its host in front.
fn remote_repo_key() -> String {
    format!("{REMOTE}::repo:r2")
}

/// A window on This PC listing `a1` in repo `r1`, with [`REMOTE`] saved,
/// connected and listing `b1` in repo `r2`.
fn saved_remote_listing<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    seed_ui(
        dir,
        &serde_json::json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::with(
        cx,
        dir,
        &Fixture::single(session("a1").in_repo("r1").build()),
    );
    h.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/r1")],
    });
    let mut host = h.host(&saved_remote());
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r2", "C:/r2")],
    });
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").in_repo("r2").build()],
    });
    h.sent();
    assert!(
        shown_folds(&mut h)
            .iter()
            .any(|(key, _)| *key == remote_repo_key()),
        "the remote repo row shows"
    );
    h
}

/// Whether the remote host's repo row and its session's row draw dimmed,
/// as a pair.
fn remote_rows_dimmed(h: &mut Harness<'_>) -> (bool, bool) {
    h.root(|root, _| {
        (
            root.container_dimmed(&remote_repo_key()),
            root.session_dimmed("b1"),
        )
    })
}

/// A connection to `target` that was open and dropped.
fn dropped(target: Target) -> Connection {
    let mut conn = Connection::for_target(target);
    conn.on_connecting();
    conn.on_socket_open(8787);
    conn.on_welcome(PROTOCOL, Vec::new());
    let _ = conn.on_closed("socket closed".to_owned());
    conn
}

#[gpui::test]
fn an_offline_hosts_rows_dim_and_come_back_when_it_reconnects(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_listing(cx, &dir);
    assert_eq!(
        remote_rows_dimmed(&mut h),
        (false, false),
        "open: not dimmed"
    );

    h.host(&saved_remote())
        .event(NetEvent::State(dropped(saved_remote())));
    assert_eq!(
        remote_rows_dimmed(&mut h),
        (true, true),
        "the dropped host's repo and session rows dim"
    );
    assert_eq!(shown_leaves(&mut h), ["a1", "b1"], "and stay listed");

    h.host(&saved_remote()).connect();
    assert_eq!(
        remote_rows_dimmed(&mut h),
        (false, false),
        "they come back when it reconnects"
    );
}

/// A Tailscale that sees no peers: Connections lists only the saved hosts.
struct NoPeers;

impl PeerSource for NoPeers {
    fn status(&self) -> Result<tailnet::Status, tailnet::TailnetError> {
        Ok(tailnet::Status::default())
    }
}

#[gpui::test]
fn a_switched_off_hosts_rows_stay_dimmed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_listing(cx, &dir);
    assert_eq!(remote_rows_dimmed(&mut h), (false, false), "on: not dimmed");
    h.root_update(|root, _, _| root.set_peer_source(Arc::new(NoPeers)));
    h.click_on("footer-connections");

    h.click_on(&format!("connections-row-{REMOTE}-enabled"));

    assert_eq!(h.root(|root, _| root.held_hosts()), [HostKey::Local]);
    assert_eq!(shown_leaves(&mut h), ["a1", "b1"], "its rows stay");
    assert_eq!(
        remote_rows_dimmed(&mut h),
        (true, true),
        "a switched-off host's rows draw dimmed"
    );
}

#[gpui::test]
fn this_pcs_rows_never_dim(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_listing(cx, &dir);
    h.host(&saved_remote())
        .event(NetEvent::State(dropped(saved_remote())));

    h.set_connection(dropped(Target::Local));

    assert_eq!(
        h.root(|root, _| (
            root.host_dimmed(&HostKey::Local),
            root.container_dimmed("repo:r1"),
            root.session_dimmed("a1"),
        )),
        (false, false, false),
        "This PC's rows stay at full strength while its daemon is away"
    );
}

/// Whether the footer drew `text` under `selector`; gpui keeps what it
/// painted, so a spec asks about a text only before it was ever drawn.
fn footer_shows(h: &mut Harness<'_>, selector: &str, text: &str) -> bool {
    h.bounds(&format!("{selector}={text}")).origin.x >= px(0.0)
}

#[gpui::test]
fn the_footer_counts_hosts_online(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &serde_json::json!({ "saved_hosts": [
            { "name": REMOTE, "port": 8787 },
            { "name": OTHER, "port": 8787 },
            { "name": "off.tail1234.ts.net", "port": 8787, "enabled": false },
        ] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    assert!(
        footer_shows(&mut h, "footer-hosts", "1/3 hosts online"),
        "This PC is open, both enabled hosts still connecting; the switched-off host is not counted"
    );

    h.host(&saved_remote()).connect();
    assert!(footer_shows(&mut h, "footer-hosts", "2/3 hosts online"));
    h.host(&Target::tailnet(OTHER, Some(8787))).connect();
    assert!(footer_shows(&mut h, "footer-hosts", "3/3 hosts online"));
    assert!(
        footer_shows(&mut h, "daemon-pill", "daemon · 0 sessions · :4242"),
        "the pill stays This PC's"
    );

    h.click_on("footer-hosts");
    assert!(
        h.root(|root, _| root.connection_picker_open()),
        "the count opens Connections"
    );
}

#[gpui::test]
fn the_footer_hides_the_count_with_one_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &serde_json::json!({ "saved_hosts": [
            { "name": REMOTE, "port": 8787, "enabled": false },
        ] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());

    assert!(
        h.bounds("footer-hosts").origin.x < px(0.0),
        "only This PC is on: the count was never drawn"
    );
}

/// Opens Connections over a Tailscale that sees no peers.
fn open_connections(h: &mut Harness<'_>) {
    h.root_update(|root, _, _| root.set_peer_source(Arc::new(NoPeers)));
    h.click_on("footer-connections");
}

/// The Connected switch on [`REMOTE`]'s Connections row.
fn remote_switch() -> String {
    format!("connections-row-{REMOTE}-enabled")
}

/// Types `text` one key at a time.
fn type_text(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    h.keys(&keys.join(" "));
}

/// A connection to `target` that the host refused as not approved.
fn not_approved(target: Target) -> Connection {
    let mut conn = Connection::for_target(target);
    conn.on_connecting();
    conn.on_socket_open(8787);
    conn.on_not_approved(protocol::DEVICE_NOT_APPROVED.to_owned(), false);
    conn
}

/// The files directly in `dir`; none when it is missing.
fn files_in(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir).map_or(0, Iterator::count)
}

/// Whether a toast says the fetch of `name` failed.
fn fetch_failed(h: &mut Harness<'_>, name: &str) -> bool {
    let title = format!("Couldn't fetch {name}");
    h.root(move |root, _| root.toasts().iter().any(|toast| toast.title == title))
}

#[gpui::test]
fn switching_a_host_off_mid_fetch_ends_its_fetch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_listing(cx, &dir);
    let root = dir.path().join("remote-files");
    let set = root.clone();
    h.root_update(move |view, _, _| view.set_remote_files_root(set));
    h.root_update(|view, _, cx| view.fetch_remote_file(fetch_request("r2", "b.txt"), cx));
    let ids = fetch_ids(h.host(&saved_remote()).sent());
    assert_eq!(ids.len(), 1, "one fetch is sent: {ids:?}");
    let id = &ids[0];
    let mut host = h.host(&saved_remote());
    host.send(DaemonMessage::FileFetchStarted {
        id: id.clone(),
        resolved_path: r"\\?\C:\r2\b.txt".to_owned(),
        size: 10,
    });
    host.send(DaemonMessage::FileChunk {
        id: id.clone(),
        seq: 0,
        data_b64: "aGFsZg==".to_owned(),
    });
    let folder = root.join(REMOTE).join("repo");
    assert_eq!(
        files_in(&folder),
        1,
        "the part file is written while the fetch runs"
    );

    open_connections(&mut h);
    h.click_on(&remote_switch());

    assert!(
        fetch_failed(&mut h, "b.txt"),
        "the fetch ends with the disconnect toast"
    );
    assert_eq!(files_in(&folder), 0, "the part file is deleted");
    h.click_on(&remote_switch());
    h.host(&saved_remote()).connect();
    h.root_update(|view, _, cx| view.fetch_remote_file(fetch_request("r2", "b.txt"), cx));
    assert_eq!(
        fetch_ids(h.host(&saved_remote()).sent()).len(),
        1,
        "the same file is fetched again once the host is back on"
    );
}

#[gpui::test]
fn a_stale_event_from_a_replaced_connection_is_ignored(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_listing(cx, &dir);
    open_connections(&mut h);
    h.click_on(&remote_switch());
    h.click_on(&remote_switch());
    h.host(&saved_remote()).connect();

    h.host(&saved_remote())
        .old_connection()
        .send(DaemonMessage::Sessions {
            sessions: Vec::new(),
        });
    h.host(&saved_remote())
        .old_connection()
        .event(NetEvent::State(dropped(saved_remote())));

    assert_eq!(
        shown_leaves(&mut h),
        ["a1", "b1"],
        "the old connection's empty list empties nothing"
    );
    assert_eq!(
        remote_rows_dimmed(&mut h),
        (false, false),
        "the old connection's close leaves the new one open"
    );
}

#[gpui::test]
fn changing_a_hosts_port_ignores_the_old_connections_late_sessions(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_listing(cx, &dir);
    open_connections(&mut h);
    h.click_on("connections-add-host");
    type_text(&mut h, REMOTE);
    h.keys("tab");
    type_text(&mut h, "9443");
    h.keys("enter");
    let moved = Target::tailnet(REMOTE, Some(9443));
    assert_eq!(
        h.opened_targets().last(),
        Some(&moved),
        "the host is dialled on its new port"
    );
    let mut host = h.host(&moved);
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b2").in_repo("r2").build()],
    });

    h.host(&saved_remote()).send(DaemonMessage::Sessions {
        sessions: vec![
            session("b1").in_repo("r2").build(),
            session("b9").in_repo("r2").build(),
        ],
    });

    assert_eq!(
        shown_leaves(&mut h),
        ["a1", "b2"],
        "the old port's late list is ignored"
    );
}

/// [`REMOTE`] and the SSH host `me@devbox` saved and switched on, both
/// reaching [`DAEMON`]: the SSH host welcomed second and was dropped.
fn two_paths_to_one_daemon<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    seed_ui(
        dir,
        &serde_json::json!({ "saved_hosts": [
            { "name": REMOTE, "port": 8787 },
            { "kind": "ssh", "name": "me@devbox" },
        ] }),
    );
    let mut h = Harness::with(cx, dir, &Fixture::default());
    welcome_as(&mut h, &saved_remote(), Some(DAEMON));
    welcome_as(&mut h, &ssh_box(), Some(DAEMON));
    assert_eq!(
        duplicate_of(&mut h, &ssh_box()),
        Some(HostKey::of(&saved_remote()))
    );
    h
}

/// How many connections the window opened to `target`.
fn times_opened(h: &Harness<'_>, target: &Target) -> usize {
    h.opened_targets()
        .iter()
        .filter(|opened| *opened == target)
        .count()
}

/// Asserts the dropped SSH duplicate was dialled again and, welcoming
/// first this time, now holds the daemon.
fn assert_duplicate_reconnected(h: &mut Harness<'_>) {
    assert_eq!(
        duplicate_of(h, &ssh_box()),
        None,
        "the duplicate no longer names its owner"
    );
    assert_eq!(
        times_opened(h, &ssh_box()),
        2,
        "the duplicate is dialled again"
    );
    welcome_as(h, &ssh_box(), Some(DAEMON));
    assert_eq!(
        duplicate_of(h, &ssh_box()),
        None,
        "welcoming first, it takes the daemon"
    );
    assert!(
        h.root(|root, _| root.held_hosts())
            .contains(&HostKey::of(&ssh_box())),
        "its connection is held"
    );
}

#[gpui::test]
fn a_duplicate_reconnects_when_its_owner_is_switched_off(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_paths_to_one_daemon(cx, &dir);
    open_connections(&mut h);

    h.click_on(&remote_switch());

    assert_duplicate_reconnected(&mut h);
}

#[gpui::test]
fn a_duplicate_reconnects_when_its_owner_drops(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_paths_to_one_daemon(cx, &dir);

    h.host(&saved_remote())
        .event(NetEvent::State(dropped(saved_remote())));

    assert_duplicate_reconnected(&mut h);
}

#[gpui::test]
fn a_pair_armed_while_dialling_is_forgotten_when_the_host_is_switched_off(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &serde_json::json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_connections(&mut h);
    h.click_on(&format!("connections-row-{REMOTE}-pair"));
    assert!(
        !h.root(|root, _| root.device_pairing_open()),
        "the host is still being dialled: the dialog waits for its answer"
    );

    open_connections(&mut h);
    h.click_on(&remote_switch());
    h.click_on(&remote_switch());
    h.host(&saved_remote())
        .event(NetEvent::State(not_approved(saved_remote())));

    assert!(
        !h.root(|root, _| root.device_pairing_open()),
        "the Pair asked before the switch-off is forgotten"
    );
}

/// A window on This PC with [`REMOTE`] saved and connected: tab `t1`
/// splits This PC's `a1` (pane `p1`) beside the remote host's `b1` (pane
/// `p2`), laid out before the remote host's list, and `p2` has the
/// keyboard.
fn saved_remote_mixed<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    seed_ui(
        dir,
        &serde_json::json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::with(cx, dir, &mixed_fixture());
    h.answer_scrollback("a1", b"");
    list_saved_remote(&mut h);
    h
}

/// This PC lists `a1`; tab `t1` splits it (pane `p1`) beside [`REMOTE`]'s
/// `b1` (pane `p2`).
fn mixed_fixture() -> Fixture {
    let grid = split(
        SplitDirection::Horizontal,
        pane("p1", Some("a1")),
        remote_pane("p2", "b1"),
    );
    Fixture {
        sessions: vec![session("a1").build()],
        tabs: vec![tab("t1", &grid)],
        ..Fixture::default()
    }
}

/// Connects [`REMOTE`]'s newest connection, lists `b1` on it, answers its
/// scrollback and clicks `p2`; drains what both hosts were sent.
fn list_saved_remote(h: &mut Harness<'_>) {
    let mut host = h.host(&saved_remote());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").build()],
    });
    host.answer_scrollback("b1", b"");
    let at = h.cell_center("p2", 0, 0);
    h.click(at, Modifiers::none());
    h.sent();
    h.host(&saved_remote()).sent();
}

/// Flips [`REMOTE`]'s Connected switch in Connections, then closes it.
fn flip_remote_switch(h: &mut Harness<'_>) {
    open_connections(h);
    h.click_on(&remote_switch());
    h.keys("escape");
}

#[gpui::test]
fn a_switched_off_hosts_panes_release_its_connection(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_mixed(cx, &dir);

    flip_remote_switch(&mut h);

    assert!(
        h.host(&saved_remote()).is_released(),
        "no pane keeps the switched-off host's connection"
    );
    let at = h.cell_center("p2", 0, 0);
    h.click(at, Modifiers::none());
    h.sent();
    h.keys("x");
    assert!(
        h.sent_input("b1").is_empty(),
        "a key typed in p2 goes nowhere, This PC included"
    );
    assert!(
        h.host(&saved_remote()).sent_input("b1").is_empty(),
        "nor on the switched-off host's connection"
    );

    flip_remote_switch(&mut h);
    list_saved_remote(&mut h);
    h.keys("y");

    assert_eq!(
        h.host(&saved_remote()).sent_input("b1"),
        b"y",
        "keys go out on the newest connection"
    );
    assert!(
        h.host(&saved_remote())
            .old_connection()
            .sent_input("b1")
            .is_empty(),
        "and never on the old one"
    );
    assert!(h.sent_input("b1").is_empty(), "nor to This PC");
}

#[gpui::test]
fn a_reconnecting_host_attaches_nothing_before_its_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_mixed(cx, &dir);
    let before = h.host(&saved_remote()).scrollback_requests("b1").len();
    h.host(&saved_remote())
        .event(NetEvent::State(dropped(saved_remote())));
    h.host(&saved_remote()).connect();

    // In the gap a new pane shows b1, and This PC's session moves.
    let grid = split(
        SplitDirection::Horizontal,
        split(
            SplitDirection::Vertical,
            pane("p1", Some("a1")),
            remote_pane("p4", "b1"),
        ),
        remote_pane("p2", "b1"),
    );
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &grid),
    });
    h.send(DaemonMessage::SessionUpdated {
        session: session("a1").build(),
        request_id: None,
    });
    assert_eq!(
        h.host(&saved_remote()).scrollback_requests("b1").len(),
        before,
        "nothing attaches from the list the host sent before it reconnected"
    );

    h.host(&saved_remote()).send(DaemonMessage::Sessions {
        sessions: vec![session("b1").build()],
    });
    assert_eq!(
        h.host(&saved_remote()).scrollback_requests("b1").len(),
        before + 1,
        "its fresh list attaches both panes with one request"
    );
}

#[gpui::test]
fn a_scrollback_retry_waits_for_the_reconnected_hosts_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &serde_json::json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::with(cx, &dir, &mixed_fixture());
    h.answer_scrollback("a1", b"");
    let mut host = h.host(&saved_remote());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").build()],
    });
    // b1's scrollback reply is lost as the connection blips.
    let before = host.scrollback_requests("b1").len();
    host.event(NetEvent::State(dropped(saved_remote())));
    host.connect();

    h.advance(Duration::from_secs(8));
    h.advance(Duration::from_secs(2));
    assert_eq!(
        h.host(&saved_remote()).scrollback_requests("b1").len(),
        before,
        "the timed-out request is not retried before the host lists again"
    );

    h.host(&saved_remote()).send(DaemonMessage::Sessions {
        sessions: vec![session("b1").build()],
    });
    assert_eq!(
        h.host(&saved_remote()).scrollback_requests("b1").len(),
        before + 1,
        "the fresh list asks for b1's scrollback exactly once"
    );
}

/// The second tailnet host the reconnect specs connect.
const LAB: &str = "lab.tail1234.ts.net";

fn lab() -> Target {
    Target::tailnet(LAB, None)
}

/// A window on This PC listing `a1` in pane `p1`, with [`REMOTE`] listing
/// `b1` (pane `p2`) and `b2` (pane `p3`), and [`LAB`] listing `c1` (pane
/// `p5`), every pane attached.
fn three_hosts<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let lab_pane = protocol::GridNode::Pane {
        pane_id: "p5".to_owned(),
        session_id: Some("c1".to_owned()),
        host: Some(LAB.to_owned()),
    };
    let fixture = Fixture {
        sessions: vec![session("a1").build()],
        tabs: vec![
            tab(
                "t1",
                &split(
                    SplitDirection::Horizontal,
                    pane("p1", Some("a1")),
                    remote_pane("p2", "b1"),
                ),
            ),
            tab(
                "t2",
                &split(
                    SplitDirection::Horizontal,
                    remote_pane("p3", "b2"),
                    lab_pane,
                ),
            ),
        ],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, dir, &fixture);
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").build(), session("b2").build()],
    });
    let mut other = h.host(&lab());
    other.connect();
    other.send(DaemonMessage::Sessions {
        sessions: vec![session("c1").build()],
    });
    h
}

/// How many scrollback requests each pane's session has had: This PC's
/// `a1`, [`REMOTE`]'s `b1` and `b2`, [`LAB`]'s `c1`.
fn scrollback_counts(h: &mut Harness<'_>) -> [usize; 4] {
    let a1 = h.scrollback_requests("a1").len();
    let mut host = h.host(&remote());
    let (b1, b2) = (
        host.scrollback_requests("b1").len(),
        host.scrollback_requests("b2").len(),
    );
    let c1 = h.host(&lab()).scrollback_requests("c1").len();
    [a1, b1, b2, c1]
}

#[gpui::test]
fn a_remote_reconnect_reattaches_only_that_hosts_panes_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = three_hosts(cx, &dir);
    let [a1, b1, b2, c1] = scrollback_counts(&mut h);
    assert!(b1 > 0 && b2 > 0 && c1 > 0, "every remote pane attached");

    let mut host = h.host(&remote());
    host.event(NetEvent::State(dropped(remote())));
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").build(), session("b2").build()],
    });
    assert_eq!(
        scrollback_counts(&mut h),
        [a1, b1 + 1, b2 + 1, c1],
        "one request per session of the reconnected host, none elsewhere"
    );

    h.host(&remote()).send(DaemonMessage::Sessions {
        sessions: vec![session("b1").build(), session("b2").build()],
    });
    assert_eq!(
        scrollback_counts(&mut h),
        [a1, b1 + 1, b2 + 1, c1],
        "a second list on the same connection re-attaches nothing"
    );
}

#[gpui::test]
fn a_this_pc_reconnect_does_not_reattach_remote_panes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_mixed(cx, &dir);
    let a1 = h.scrollback_requests("a1").len();
    let b1 = h.host(&saved_remote()).scrollback_requests("b1").len();

    h.send_welcome(PROTOCOL);
    h.load(&mixed_fixture());

    assert_eq!(
        h.scrollback_requests("a1").len(),
        a1 + 1,
        "This PC's pane attaches again"
    );
    assert_eq!(
        h.host(&saved_remote()).scrollback_requests("b1").len(),
        b1,
        "the remote pane stays attached"
    );
}

#[gpui::test]
fn an_unapproved_reconnect_keeps_a_remote_panes_last_frame(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_mixed(cx, &dir);
    h.host(&saved_remote()).pty("b1", b"hello");
    assert_eq!(h.grid_text("p2")[0], "hello");

    let mut host = h.host(&saved_remote());
    host.send(welcome(PROTOCOL));
    host.event(NetEvent::State(not_approved(saved_remote())));

    assert_eq!(
        h.grid_text("p2")[0],
        "hello",
        "a reconnect that never lists keeps the pane's last frame"
    );
}

/// The forgets This PC was sent since the last look.
fn this_pc_forgets(h: &mut Harness<'_>) -> Vec<Forget> {
    forgets(&h.sent())
}

/// A forget of [`REMOTE`]'s sessions `ids`.
fn forget_of(ids: &[&str]) -> Forget {
    (
        REMOTE.to_owned(),
        ids.iter().map(|id| (*id).to_owned()).collect(),
        false,
    )
}

/// A forget of every pane of `host`.
fn forget_all(host: &str) -> Forget {
    (host.to_owned(), Vec::new(), true)
}

#[gpui::test]
fn a_remote_list_omitting_a_paned_session_forgets_it_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_over(cx, &dir, &two_tab_fixture(), &["b1", "b2"]);

    remote_lists(&mut h, &["b1"]);

    let sent = h.sent();
    assert_eq!(
        forgets(&sent),
        [forget_of(&["b2"])],
        "the pane of the session the list omits is forgotten on This PC"
    );
    assert!(
        sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::ForgetHostSessions {
                request_id: None,
                ..
            }
        )),
        "the forget answers no request"
    );
    assert!(
        forgets(&h.host(&saved_remote()).sent()).is_empty(),
        "nothing goes to the remote host"
    );

    h.send(DaemonMessage::TabUpdated {
        tab: two_tabs().remove(0),
    });
    remote_lists(&mut h, &["b1"]);
    assert!(
        this_pc_forgets(&mut h).is_empty(),
        "a forget in flight is not sent again"
    );
}

#[gpui::test]
fn a_remote_list_before_the_tabs_prunes_when_the_tabs_arrive(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &serde_json::json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::open(cx, &dir);
    h.host(&saved_remote()).connect();
    remote_lists(&mut h, &["b1"]);
    assert!(this_pc_forgets(&mut h).is_empty(), "no layout to prune yet");

    h.load(&two_tab_fixture());

    assert_eq!(
        this_pc_forgets(&mut h),
        [forget_of(&["b2"])],
        "the layout's pane of a session the list omits is forgotten"
    );
}

#[gpui::test]
fn a_session_removed_while_its_drop_is_in_flight_is_forgotten_when_the_pane_lands(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = saved_remote_over(
        cx,
        &dir,
        &Fixture::single(session("a1").build()),
        &["b1", "b2"],
    );

    h.host(&saved_remote()).send(DaemonMessage::SessionRemoved {
        session_id: "b1".to_owned(),
    });
    assert!(this_pc_forgets(&mut h).is_empty(), "no pane shows b1 yet");
    h.send(DaemonMessage::TabUpdated {
        tab: two_tabs().remove(0),
    });
    assert_eq!(
        this_pc_forgets(&mut h),
        [forget_of(&["b1"])],
        "the pane landing on the removed session is forgotten"
    );

    h.send(DaemonMessage::TabUpdated {
        tab: two_tabs().remove(1),
    });
    assert!(this_pc_forgets(&mut h).is_empty(), "b2 is still listed");
    h.host(&saved_remote()).send(DaemonMessage::SessionRemoved {
        session_id: "b2".to_owned(),
    });
    assert_eq!(
        this_pc_forgets(&mut h),
        [forget_of(&["b2"])],
        "a removal of a paned session forgets its pane, and only its"
    );
}

#[gpui::test]
fn a_tab_list_between_a_remote_welcome_and_its_list_forgets_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_over(cx, &dir, &two_tab_fixture(), &["b1", "b2"]);

    h.host(&saved_remote()).send(welcome(PROTOCOL));
    let grid = split(
        SplitDirection::Horizontal,
        split(
            SplitDirection::Vertical,
            pane("p1", Some("a1")),
            remote_pane("p4", "b7"),
        ),
        remote_pane("p2", "b1"),
    );
    h.send(DaemonMessage::TabUpdated {
        tab: tab("t1", &grid),
    });
    assert!(
        this_pc_forgets(&mut h).is_empty(),
        "the host's list from before its Welcome is not compared against"
    );

    remote_lists(&mut h, &["b1", "b2", "b7"]);
    assert!(
        this_pc_forgets(&mut h).is_empty(),
        "its fresh list names every paned session"
    );
}

/// A [`REMOTE`] session that replaced `lineage`, oldest first.
fn successor(id: &str, lineage: &[&str]) -> SessionSnapshot {
    let mut snapshot = session(id).build();
    snapshot.resumed_from = lineage.iter().map(|old| (*old).to_owned()).collect();
    snapshot
}

/// [`REMOTE`]'s daemon lists `sessions`.
fn remote_lists_sessions(h: &mut Harness<'_>, sessions: Vec<SessionSnapshot>) {
    h.host(&saved_remote())
        .send(DaemonMessage::Sessions { sessions });
}

/// A `RebindHostSessions` the client sent: its host and `(old, new)` pairs.
type Rebind = (String, Vec<(String, String)>);

/// The `RebindHostSessions` messages among `sent`, in order.
fn rebinds(sent: &[ClientMessage]) -> Vec<Rebind> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::RebindHostSessions { host, rebinds, .. } => Some((
                host.clone(),
                rebinds
                    .iter()
                    .map(|pair| (pair.old_id.clone(), pair.new_id.clone()))
                    .collect(),
            )),
            _ => None,
        })
        .collect()
}

/// A rebind of [`REMOTE`]'s `pairs`.
fn rebind_of(pairs: &[(&str, &str)]) -> Rebind {
    (
        REMOTE.to_owned(),
        pairs
            .iter()
            .map(|(old, new)| ((*old).to_owned(), (*new).to_owned()))
            .collect(),
    )
}

#[gpui::test]
fn a_link_and_a_removal_send_one_rebind_and_no_forget(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_over(cx, &dir, &two_tab_fixture(), &["b1", "b2"]);

    h.host(&saved_remote()).send(DaemonMessage::SessionUpdated {
        session: successor("b3", &["b2"]),
        request_id: None,
    });
    h.host(&saved_remote()).send(DaemonMessage::SessionRemoved {
        session_id: "b2".to_owned(),
    });

    let sent = h.sent();
    assert_eq!(rebinds(&sent), [rebind_of(&[("b2", "b3")])]);
    assert!(forgets(&sent).is_empty(), "the pane has a successor");
    assert!(
        rebinds(&h.host(&saved_remote()).sent()).is_empty(),
        "nothing goes to the remote host"
    );

    remote_lists_sessions(
        &mut h,
        vec![session("b1").build(), successor("b3", &["b2"])],
    );
    let sent = h.sent();
    assert!(
        rebinds(&sent).is_empty(),
        "a rebind in flight is not resent"
    );
    assert!(forgets(&sent).is_empty(), "a pane with a successor stays");

    h.send(DaemonMessage::TabUpdated {
        tab: tab("t2", &remote_pane("p3", "b3")),
    });
    let sent = h.sent();
    assert!(rebinds(&sent).is_empty(), "the pane shows its successor");
    assert!(forgets(&sent).is_empty(), "a pane with a successor stays");
}

#[gpui::test]
#[expect(clippy::panic, reason = "a wrong message fails the test loudly")]
fn a_refused_rebind_is_retried_by_the_next_prune(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_over(cx, &dir, &two_tab_fixture(), &["b1", "b2"]);
    let resumed = vec![session("b1").build(), successor("b3", &["b2"])];
    remote_lists_sessions(&mut h, resumed.clone());
    let sent = h.sent();
    let Some(ClientMessage::RebindHostSessions {
        request_id: Some(request_id),
        ..
    }) = sent
        .iter()
        .find(|msg| matches!(msg, ClientMessage::RebindHostSessions { .. }))
    else {
        panic!("expected a rebind with a request id, got {sent:?}");
    };

    remote_lists_sessions(&mut h, resumed.clone());
    assert!(rebinds(&h.sent()).is_empty(), "the rebind is in flight");

    h.send(DaemonMessage::Error {
        message: "could not save".to_owned(),
        request_id: Some(request_id.clone()),
        code: Some("tab_refused".to_owned()),
    });
    remote_lists_sessions(&mut h, resumed);

    let sent = h.sent();
    assert_eq!(rebinds(&sent), [rebind_of(&[("b2", "b3")])]);
    assert!(forgets(&sent).is_empty(), "a pane with a successor stays");
}

#[gpui::test]
fn a_rebind_is_sent_again_after_this_pc_reconnects(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_over(cx, &dir, &two_tab_fixture(), &["b1", "b2"]);
    let resumed = vec![session("b1").build(), successor("b3", &["b2"])];
    remote_lists_sessions(&mut h, resumed.clone());
    assert_eq!(rebinds(&h.sent()), [rebind_of(&[("b2", "b3")])]);

    h.set_connection(dropped(Target::Local));
    h.set_connection(this_pc_open());
    h.send_welcome(PROTOCOL);
    h.load(&two_tab_fixture());

    let sent = h.sent();
    assert_eq!(
        rebinds(&sent),
        [rebind_of(&[("b2", "b3")])],
        "what went out on the lost connection goes out again"
    );
    assert!(forgets(&sent).is_empty(), "a pane with a successor stays");
}

#[gpui::test]
fn a_host_list_after_an_absence_rebinds_the_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_over(cx, &dir, &two_tab_fixture(), &["b1", "b2"]);

    h.host(&saved_remote())
        .event(NetEvent::State(dropped(saved_remote())));
    h.host(&saved_remote()).connect();
    remote_lists_sessions(
        &mut h,
        vec![session("b1").build(), successor("b3", &["b2"])],
    );

    let sent = h.sent();
    assert_eq!(rebinds(&sent), [rebind_of(&[("b2", "b3")])]);
    assert!(forgets(&sent).is_empty(), "a pane with a successor stays");
}

#[gpui::test]
fn a_chain_resolves_from_one_list(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_over(cx, &dir, &two_tab_fixture(), &["b1", "b2"]);

    remote_lists_sessions(
        &mut h,
        vec![session("b1").build(), successor("b4", &["b2", "b3"])],
    );

    assert_eq!(
        rebinds(&h.sent()),
        [rebind_of(&[("b2", "b4")])],
        "a client away across two resumes finds the last one"
    );
}

#[gpui::test]
fn a_list_before_tabs_waits_for_tabs(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &serde_json::json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
    );
    let mut h = Harness::open(cx, &dir);
    h.host(&saved_remote()).connect();
    remote_lists_sessions(
        &mut h,
        vec![session("b1").build(), successor("b3", &["b2"])],
    );
    assert!(rebinds(&h.sent()).is_empty(), "no layout to rebind yet");

    h.load(&two_tab_fixture());

    let sent = h.sent();
    assert_eq!(rebinds(&sent), [rebind_of(&[("b2", "b3")])]);
    assert!(forgets(&sent).is_empty(), "a pane with a successor stays");
}

#[gpui::test]
fn the_newest_successor_wins(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_over(cx, &dir, &two_tab_fixture(), &["b1", "b2"]);
    let at = |seconds| chrono::DateTime::from_timestamp(seconds, 0).expect("a valid time");
    let stamped = |id: &str, seconds, lineage: &[&str]| {
        let mut snapshot = session(id).started_at(at(seconds)).build();
        snapshot.resumed_from = lineage.iter().map(|old| (*old).to_owned()).collect();
        snapshot
    };

    remote_lists_sessions(
        &mut h,
        vec![
            stamped("b4", 2_000, &["b1"]),
            stamped("b3", 1_000, &["b1"]),
            stamped("b5", 1_000, &["b2"]),
            stamped("b6", 1_000, &["b2"]),
        ],
    );

    assert_eq!(
        rebinds(&h.sent()),
        [rebind_of(&[("b1", "b4"), ("b2", "b6")])],
        "the newest start, then the larger id"
    );
}

#[gpui::test]
fn a_pane_with_no_successor_is_forgotten_as_before(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_over(cx, &dir, &two_tab_fixture(), &["b1", "b2"]);

    remote_lists_sessions(
        &mut h,
        vec![session("b1").build(), successor("b3", &["b9"])],
    );

    let sent = h.sent();
    assert!(rebinds(&sent).is_empty(), "no pane shows b9");
    assert_eq!(forgets(&sent), [forget_of(&["b2"])]);
}

/// This PC's layout with [`REMOTE`]'s `b2` paned beside the rest.
fn tabs_with_b2() -> DaemonMessage {
    DaemonMessage::Tabs { tabs: two_tabs() }
}

#[gpui::test]
fn nothing_is_pruned_while_a_host_is_offline_switched_off_or_unapproved(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut fixture = two_tab_fixture();
    fixture.tabs.truncate(1);
    let mut h = saved_remote_over(cx, &dir, &fixture, &["b1"]);

    h.host(&saved_remote())
        .event(NetEvent::State(dropped(saved_remote())));
    h.send(tabs_with_b2());
    assert!(this_pc_forgets(&mut h).is_empty(), "offline");

    h.host(&saved_remote()).connect();
    h.send(tabs_with_b2());
    assert!(this_pc_forgets(&mut h).is_empty(), "welcomed, not listed");

    h.host(&saved_remote())
        .event(NetEvent::State(not_approved(saved_remote())));
    h.send(tabs_with_b2());
    assert!(this_pc_forgets(&mut h).is_empty(), "not approved");

    flip_remote_switch(&mut h);
    h.send(tabs_with_b2());
    assert!(this_pc_forgets(&mut h).is_empty(), "switched off");
}

#[gpui::test]
fn a_dropped_duplicates_panes_are_not_pruned(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &serde_json::json!({ "saved_hosts": [
            { "name": REMOTE, "port": 8787 },
            { "kind": "ssh", "name": "me@devbox" },
        ] }),
    );
    let ssh_pane = protocol::GridNode::Pane {
        pane_id: "p9".to_owned(),
        session_id: Some("d1".to_owned()),
        host: HostKey::of(&ssh_box()).wire().map(str::to_owned),
    };
    let fixture = Fixture {
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                remote_pane("p2", "b1"),
                ssh_pane,
            ),
        )],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, &dir, &fixture);
    welcome_as(&mut h, &saved_remote(), Some(DAEMON));
    remote_lists(&mut h, &["b1"]);
    welcome_as(&mut h, &ssh_box(), Some(DAEMON));
    assert_eq!(
        duplicate_of(&mut h, &ssh_box()),
        Some(HostKey::of(&saved_remote()))
    );

    h.send(DaemonMessage::Tabs {
        tabs: fixture.tabs.clone(),
    });

    assert!(
        this_pc_forgets(&mut h).is_empty(),
        "the dropped key's pane stays for the daemon its owner serves"
    );
}

#[gpui::test]
fn a_forget_lost_while_this_pc_reconnects_is_sent_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_over(cx, &dir, &two_tab_fixture(), &["b1", "b2"]);

    h.set_connection(dropped(Target::Local));
    remote_lists(&mut h, &["b1"]);
    assert!(this_pc_forgets(&mut h).is_empty(), "This PC is offline");
    h.set_connection(this_pc_open());
    h.send_welcome(PROTOCOL);
    h.load(&two_tab_fixture());
    assert_eq!(
        this_pc_forgets(&mut h),
        [forget_of(&["b2"])],
        "the prune runs once This PC's layout is back"
    );

    open_connections(&mut h);
    h.set_connection(dropped(Target::Local));
    h.click_on(&format!("connections-forget-{REMOTE}"));
    assert!(this_pc_forgets(&mut h).is_empty(), "This PC is offline");
    h.set_connection(this_pc_open());
    h.send_welcome(PROTOCOL);
    h.load(&two_tab_fixture());
    assert_eq!(
        this_pc_forgets(&mut h),
        [forget_all(REMOTE)],
        "the forgotten host's panes are forgotten once, after This PC's layout"
    );
}

#[gpui::test]
fn forgetting_a_host_forgets_all_its_panes_on_this_pc(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_over(cx, &dir, &two_tab_fixture(), &["b1", "b2"]);
    open_connections(&mut h);

    h.click_on(&format!("connections-forget-{REMOTE}"));

    assert_eq!(this_pc_forgets(&mut h), [forget_all(REMOTE)]);
    assert!(
        forgets(&h.host(&saved_remote()).sent()).is_empty(),
        "nothing goes to the forgotten host"
    );
    h.send(tabs_with_b2());
    assert!(
        this_pc_forgets(&mut h).is_empty(),
        "the host's forget is in flight"
    );
}

#[gpui::test]
fn panes_of_a_host_no_longer_saved_are_forgotten_after_this_pcs_tabs(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &serde_json::json!({ "saved_hosts": [
            { "name": REMOTE, "port": 8787, "enabled": false },
        ] }),
    );
    let other_pane = protocol::GridNode::Pane {
        pane_id: "p5".to_owned(),
        session_id: Some("c1".to_owned()),
        host: Some(OTHER.to_owned()),
    };
    let mut fixture = two_tab_fixture();
    fixture.tabs.push(tab("t3", &other_pane));
    let mut h = Harness::open(cx, &dir);
    h.sent();

    h.load(&fixture);
    assert_eq!(
        this_pc_forgets(&mut h),
        [forget_all(OTHER)],
        "the unsaved host's panes go; the switched-off saved host's stay"
    );

    h.send(DaemonMessage::Tabs {
        tabs: fixture.tabs.clone(),
    });
    assert!(
        this_pc_forgets(&mut h).is_empty(),
        "once per This PC connection"
    );

    h.send_welcome(PROTOCOL);
    h.load(&fixture);
    assert_eq!(
        this_pc_forgets(&mut h),
        [forget_all(OTHER)],
        "a new This PC connection sweeps again"
    );
}

#[gpui::test]
fn a_remote_list_between_this_pcs_reconnect_and_its_welcome_forgets_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = saved_remote_over(cx, &dir, &two_tab_fixture(), &["b1", "b2"]);

    h.set_connection(dropped(Target::Local));
    h.set_connection(this_pc_open());
    remote_lists(&mut h, &["b1"]);
    h.send_welcome(PROTOCOL);
    h.load(&two_tab_fixture());

    assert_eq!(
        this_pc_forgets(&mut h),
        [forget_of(&["b2"])],
        "the layout before This PC's Welcome is the old connection's: b2 is forgotten once, after the new tab list"
    );
}

/// This PC and [`REMOTE`] each list a session `s1`; the only pane, `p1`,
/// shows [`REMOTE`]'s.
fn one_id_on_both_hosts<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    remote_s1: SessionSnapshot,
) -> Harness<'a> {
    let fixture = Fixture {
        sessions: vec![session("s1").build()],
        tabs: vec![tab("t1", &remote_pane("p1", "s1"))],
        ..Fixture::default()
    };
    let mut h = Harness::with(cx, dir, &fixture);
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r1", "C:/repos/r1")],
    });
    host.send(DaemonMessage::Sessions {
        sessions: vec![remote_s1],
    });
    h
}

#[gpui::test]
fn the_pane_header_chip_names_the_panes_host_for_a_session_id_both_hosts_hold(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = one_id_on_both_hosts(cx, &dir, session("s1").build());

    let chips: Vec<String> = h
        .root(|root, _| root.pane_header_parts("p1"))
        .expect("the pane shows a session")
        .chips
        .into_iter()
        .filter(|chip| chip.kind == HeaderChipKind::Host)
        .map(|chip| chip.text)
        .collect();
    assert_eq!(chips, ["box"], "the pane is bound to the remote host's s1");
}

#[gpui::test]
fn a_path_link_in_a_remote_pane_fetches_from_that_host_when_both_hosts_hold_the_id(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let remote_s1 = session("s1").shell("C:/wt/x/sub").worktree("r1").build();
    let mut h = one_id_on_both_hosts(cx, &dir, remote_s1);
    h.host(&remote()).answer_scrollback("s1", b"");
    h.host(&remote())
        .pty("s1", b"wrote C:\\wt\\x\\docs\\notes.md");
    h.host(&remote()).sent();
    h.sent();

    let on = h.cell_center("p1", 9, 0);
    h.click(on, Modifiers::secondary_key());

    let fetched: Vec<String> = h
        .host(&remote())
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::FetchFile { path, .. } => Some(path),
            _ => None,
        })
        .collect();
    assert_eq!(
        fetched,
        ["docs/notes.md"],
        "the link resolves against the remote host's s1 and its repos"
    );
    assert!(
        !h.sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::FetchFile { .. })),
        "nothing is fetched from This PC"
    );
}

// A chat pane on a host.

/// This PC's `a1` in pane `p1` beside the saved host's `b1` in pane `p2`,
/// the host connected and listing `b1`, with nothing unread on either host.
fn remote_chat_beside_local<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    remote_in_pane(cx, dir, &serde_json::json!({}), &[], session("b1").build())
}

/// One entry per `LoadChat` for `session` in `sent`, its request id when it
/// carries one: a request a negative check must still count.
fn load_chats(sent: &[ClientMessage], session: &str) -> Vec<Option<String>> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::LoadChat {
                session_id,
                request_id,
            } if session_id == session => Some(request_id.clone()),
            _ => None,
        })
        .collect()
}

/// One entry per `LoadSummary` for `session` in `sent`, its request id when
/// it carries one: a request a negative check must still count.
fn load_summaries(sent: &[ClientMessage], session: &str) -> Vec<Option<String>> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::LoadSummary {
                session_id,
                request_id,
            } if session_id == session => Some(request_id.clone()),
            _ => None,
        })
        .collect()
}

/// How many `UnloadChat` requests for `session` `sent` holds.
fn unload_chats(sent: &[ClientMessage], session: &str) -> usize {
    sent.iter()
        .filter(
            |msg| matches!(msg, ClientMessage::UnloadChat { session_id } if session_id == session),
        )
        .count()
}

/// The texts of `sent`'s `ChatSend`s for `session`.
fn chat_sends_of(sent: &[ClientMessage], session: &str) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::ChatSend {
                session_id, text, ..
            } if session_id == session => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// The prompt ids and answers of `sent`'s `ChatAnswer`s for `session`.
fn chat_answers_of(sent: &[ClientMessage], session: &str) -> Vec<(String, ChatAnswerKind)> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::ChatAnswer {
                session_id,
                prompt_id,
                answer,
                ..
            } if session_id == session => Some((prompt_id.clone(), answer.clone())),
            _ => None,
        })
        .collect()
}

/// `session`'s chat loaded with no rows, the reply echoing `request_id`.
fn chat_reply(session: &str, request_id: &str) -> DaemonMessage {
    DaemonMessage::Chat {
        session_id: session.to_owned(),
        entries: Vec::new(),
        hidden: 0,
        screen: ChatScreen {
            composer: ComposerState::Ready,
            prompt: None,
        },
        request_id: Some(request_id.to_owned()),
    }
}

/// `session`'s screen now shows `prompt`, or no dialog with the composer
/// ready.
fn chat_screen(session: &str, prompt: Option<ChatPrompt>) -> DaemonMessage {
    DaemonMessage::ChatScreenChanged {
        session_id: session.to_owned(),
        screen: ChatScreen {
            composer: if prompt.is_some() {
                ComposerState::Dialog
            } else {
                ComposerState::Ready
            },
            prompt,
        },
    }
}

/// A permission prompt `id` with its Yes, don't ask again and No buttons.
fn permission_prompt(id: &str) -> ChatPrompt {
    let options = [
        (1, "Yes"),
        (2, "Yes, and don't ask again for: python *"),
        (3, "No"),
    ]
    .into_iter()
    .map(|(number, label)| ChatOption {
        number,
        label: label.to_owned(),
        description: None,
        selected: false,
    })
    .collect();
    ChatPrompt {
        id: id.to_owned(),
        kind: ChatPromptKind::Permission,
        subject: "calc.py".to_owned(),
        body: "Do you want to make this edit?".to_owned(),
        options,
        questions: Vec::new(),
        can_amend: true,
        always_allow_tip: None,
        multi: false,
        cursor: None,
    }
}

/// The answer that picks the card button `number`.
fn pick(number: u32) -> ChatAnswerKind {
    ChatAnswerKind::Option { number }
}

/// Types `text` into the focused composer, one key at a time.
fn chat_type(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text
        .chars()
        .map(|c| {
            if c == ' ' {
                "space".to_owned()
            } else {
                c.to_string()
            }
        })
        .collect();
    h.keys(&keys.join(" "));
}

#[gpui::test]
fn a_remote_chat_pane_loads_from_its_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_chat_beside_local(cx, &dir);

    h.click_on("pane-view-chat-p2");

    let remote_sent = h.host(&saved_remote()).sent();
    assert_eq!(
        load_chats(&remote_sent, "b1").len(),
        1,
        "the pane asks its own host for the chat: {remote_sent:?}"
    );
    let local_sent = h.sent();
    assert!(
        load_chats(&local_sent, "b1").is_empty(),
        "nothing about b1 goes to This PC: {local_sent:?}"
    );
    assert!(
        h.root(|root, _| root.pane_shows_chat("p2")),
        "the pane is on its chat"
    );
}

#[gpui::test]
fn a_remote_summary_pane_loads_from_its_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_chat_beside_local(cx, &dir);

    h.click_on("pane-view-summary-p2");

    let remote_sent = h.host(&saved_remote()).sent();
    assert_eq!(
        load_summaries(&remote_sent, "b1").len(),
        1,
        "the pane asks its own host for the summary: {remote_sent:?}"
    );
    assert_eq!(
        load_chats(&remote_sent, "b1").len(),
        1,
        "and for the chat under it: {remote_sent:?}"
    );
    let local_sent = h.sent();
    assert!(
        load_summaries(&local_sent, "b1").is_empty() && load_chats(&local_sent, "b1").is_empty(),
        "nothing about b1 goes to This PC: {local_sent:?}"
    );
}

#[gpui::test]
fn a_remote_chat_message_goes_to_its_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_chat_beside_local(cx, &dir);
    h.click_on("pane-view-chat-p2");
    let request = load_chats(&h.host(&saved_remote()).sent(), "b1")
        .pop()
        .flatten()
        .expect("the pane asked its host for the chat");
    h.host(&saved_remote()).send(chat_reply("b1", &request));
    h.sent();
    h.host(&saved_remote()).sent();

    h.click_on("chat-composer");
    chat_type(&mut h, "hello there");
    h.keys("ctrl-enter");

    let remote_sent = h.host(&saved_remote()).sent();
    assert_eq!(
        chat_sends_of(&remote_sent, "b1"),
        ["hello there"],
        "the send goes to the host that owns the session: {remote_sent:?}"
    );
    let local_sent = h.sent();
    assert!(
        chat_sends_of(&local_sent, "b1").is_empty(),
        "This PC gets no send for b1: {local_sent:?}"
    );
}

#[gpui::test]
fn a_remote_card_answer_goes_to_its_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_chat_beside_local(cx, &dir);
    h.click_on("pane-view-chat-p2");
    let request = load_chats(&h.host(&saved_remote()).sent(), "b1")
        .pop()
        .flatten()
        .expect("the pane asked its host for the chat");
    h.host(&saved_remote()).send(chat_reply("b1", &request));
    h.host(&saved_remote())
        .send(chat_screen("b1", Some(permission_prompt("p7"))));
    h.sent();
    h.host(&saved_remote()).sent();

    h.click_on("chat-card-1");

    let remote_sent = h.host(&saved_remote()).sent();
    assert_eq!(
        chat_answers_of(&remote_sent, "b1"),
        [("p7".to_owned(), pick(1))],
        "the answer goes to the host that owns the session: {remote_sent:?}"
    );
    let local_sent = h.sent();
    assert!(
        chat_answers_of(&local_sent, "b1").is_empty(),
        "This PC gets no answer for b1: {local_sent:?}"
    );
}

#[gpui::test]
fn unloading_a_remote_chat_after_its_session_left_the_list_reaches_the_host(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = remote_chat_beside_local(cx, &dir);
    h.click_on("pane-view-chat-p2");
    let request = load_chats(&h.host(&saved_remote()).sent(), "b1")
        .pop()
        .flatten()
        .expect("the pane asked its host for the chat");
    h.host(&saved_remote()).send(chat_reply("b1", &request));
    h.sent();
    h.host(&saved_remote()).sent();

    h.host(&saved_remote()).send(DaemonMessage::Sessions {
        sessions: Vec::new(),
    });
    h.send(DaemonMessage::Tabs {
        tabs: vec![tab("t1", &pane("p1", Some("a1")))],
    });

    let remote_sent = h.host(&saved_remote()).sent();
    assert_eq!(
        unload_chats(&remote_sent, "b1"),
        1,
        "the chat unloads on the host that served it, which no longer lists it: {remote_sent:?}"
    );
    let local_sent = h.sent();
    assert_eq!(
        unload_chats(&local_sent, "b1"),
        0,
        "This PC is told nothing of b1's chat: {local_sent:?}"
    );
}

#[gpui::test]
fn a_remote_reconnect_reloads_its_chat(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_chat_beside_local(cx, &dir);
    h.click_on("pane-view-chat-p1");
    let local = load_chats(&h.sent(), "a1")
        .pop()
        .flatten()
        .expect("This PC's pane asked This PC for the chat");
    h.click_on("pane-view-chat-p2");
    let remote = load_chats(&h.host(&saved_remote()).sent(), "b1")
        .pop()
        .flatten()
        .expect("the host's pane asked its host for the chat");
    h.send(chat_reply("a1", &local));
    h.host(&saved_remote()).send(chat_reply("b1", &remote));
    h.sent();
    h.host(&saved_remote()).sent();

    h.host(&saved_remote())
        .event(NetEvent::State(dropped(saved_remote())));
    h.host(&saved_remote()).connect();
    remote_lists(&mut h, &["b1"]);

    let remote_sent = h.host(&saved_remote()).sent();
    assert_eq!(
        load_chats(&remote_sent, "b1").len(),
        1,
        "the new connection asks for the chat again: {remote_sent:?}"
    );
    let local_sent = h.sent();
    assert!(
        load_chats(&local_sent, "a1").is_empty(),
        "This PC's chat is on a connection that did not change: {local_sent:?}"
    );
    assert!(
        h.root(|root, _| root.pane_shows_chat("p2")),
        "the pane stays on its chat"
    );
}

#[gpui::test]
fn a_chat_refusal_from_a_host_reaches_its_pane(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_chat_beside_local(cx, &dir);
    h.click_on("pane-view-chat-p1");
    // This PC's load stays unanswered, so its pane is still waiting when the
    // host's refusal lands: a note on p1 would be one a wrong route left
    // there, not a reply that never matched a pending request.
    let _local = load_chats(&h.sent(), "a1")
        .pop()
        .flatten()
        .expect("This PC's pane asked This PC for the chat");
    h.click_on("pane-view-chat-p2");
    let remote = load_chats(&h.host(&saved_remote()).sent(), "b1")
        .pop()
        .flatten()
        .expect("the host's pane asked its host for the chat");

    h.host(&saved_remote()).send(DaemonMessage::Error {
        message: "session `b1` not found".to_owned(),
        request_id: Some(remote),
        code: Some("not_found".to_owned()),
    });

    assert_eq!(
        h.root(|root, _| root.pane_chat_note("p2").map(str::to_owned))
            .as_deref(),
        Some("Could not load the chat: session `b1` not found"),
        "the host's refusal lands on the pane that asked"
    );
    assert!(
        !h.root(|root, _| root.pane_shows_chat("p2")),
        "that pane is back on its terminal"
    );
    assert!(
        h.root(|root, _| root.pane_chat_note("p1").map(str::to_owned))
            .is_none(),
        "This PC's pane takes no note"
    );
    assert!(
        h.root(|root, _| root.pane_shows_chat("p1")),
        "and stays on its chat"
    );
}

#[gpui::test]
fn a_this_pc_chat_pane_stays_on_this_pc(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_chat_beside_local(cx, &dir);

    h.click_on("pane-view-chat-p1");

    let local_sent = h.sent();
    assert_eq!(
        load_chats(&local_sent, "a1").len(),
        1,
        "This PC's pane asks This PC: {local_sent:?}"
    );
    let remote_sent = h.host(&saved_remote()).sent();
    assert!(
        load_chats(&remote_sent, "a1").is_empty(),
        "the connected host is asked nothing about a1: {remote_sent:?}"
    );
}
