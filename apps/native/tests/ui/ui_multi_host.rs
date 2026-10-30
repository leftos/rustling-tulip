//! Multi-host specs: the connections the window opens, one per host, and
//! which host's connection each message goes out on.

use crate::support;

use gpui::{Modifiers, TestAppContext};
use protocol::{ClientMessage, DaemonMessage, SessionSnapshot};
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{Connection, NetEvent, toast_action_selector};
use support::{Fixture, Harness, PROTOCOL, TestDir, repo, session, welcome};

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
