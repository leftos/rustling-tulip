//! Multi-host routing specs: each action on a remote host's row, pane or
//! message goes to that host, never to a host found by a first match.

use crate::support;

use gpui::TestAppContext;
use protocol::{
    ClientMessage, DaemonMessage, InitLayoutKind, VscodeWorkspaceFolder, VscodeWorkspaceSuggestion,
};
use rustling_tulip_native::target::{HostKey, Target};
use rustling_tulip_native::{Connection, NetEvent};
use support::{Fixture, Harness, PROTOCOL, TestDir, repo, saved_remote, seed_ui, session};

/// The tailnet host the specs connect beside This PC.
const REMOTE: &str = "box.tail1234.ts.net";

/// The daemon id the dedupe spec gives a daemon reached twice.
const DAEMON: &str = "6f1e2d3c-0000-4000-8000-000000000002";

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
        active_session_count: 0,
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
