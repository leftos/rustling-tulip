//! Multi-host specs: the connections the window opens, one per host, and
//! which host's connection each message goes out on.

use crate::support;

use std::collections::BTreeMap;

use gpui::{Modifiers, Pixels, Point, TestAppContext, point};
use protocol::{
    AgentId, AttentionReason, ClientMessage, ContainerRef, DaemonMessage, KeepAwakeMode,
    OptionValue, RemoteState, SessionSnapshot, TabEntry,
};
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{Connection, NetEvent, toast_action_selector};
use support::{
    Fixture, Harness, PROTOCOL, TestDir, facts, facts_message, pane, repo, session, tab, welcome,
};

#[gpui::test]
fn the_window_opens_one_connection_for_the_boot_target(cx: &mut TestAppContext) {
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
        [vec![repo_ref("r3"), repo_ref("r4")]],
        "the remote host gets only its own, in their order"
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
fn a_remote_sessions_list_never_counts_as_this_pcs_import(cx: &mut TestAppContext) {
    const HOME: &str = "home.tail1234.ts.net";
    let dir = TestDir::new();
    let ui = serde_json::json!({
        "saved_hosts": [{ "name": HOME, "port": 8787 }],
        "host_session_counts": { HOME: 3 },
    });
    std::fs::write(
        dir.path().join("native-ui.json"),
        serde_json::to_vec_pretty(&ui).expect("serialize native-ui.json"),
    )
    .expect("write native-ui.json");
    let fixture = Fixture {
        sessions: (1..=3).map(|i| session(&format!("s{i}")).build()).collect(),
        tabs: vec![tab("t1", &pane("p1", Some("s1")))],
        ..Fixture::default()
    };
    let mut h = Harness::on_target(cx, &dir, Target::tailnet(HOME, None), &fixture);
    assert!(
        !h.root(|root, _| root.import_modal_open()),
        "3 recorded, 3 arrived"
    );

    h.send_welcome(PROTOCOL);
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![session("b1").build()],
    });
    h.send(DaemonMessage::Sessions {
        sessions: fixture.sessions.clone(),
    });
    h.send(DaemonMessage::Tabs {
        tabs: fixture.tabs.clone(),
    });

    assert!(
        !h.root(|root, _| root.import_modal_open()),
        "the other host's list is not the layout home's first list"
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
