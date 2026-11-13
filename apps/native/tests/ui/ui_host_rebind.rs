//! Host rebind specs: Unbind and Rebind… on a Connections row, in the jump
//! palette and in a host header's menu; the rebind dialog's count, its
//! layouts asked of a host that lists them, the follow rows, a session that
//! starts while it is open, and the unbind and bind its confirm sends This
//! PC.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::sync::Arc;

use chrono::{DateTime, Utc};
use gpui::{Modifiers, TestAppContext};
use protocol::{
    ClientMessage, DaemonMessage, HostArrangement, HostLayout, RearrangeLayout, SessionSnapshot,
    SplitDirection, TabEntry,
};
use rustling_tulip_native::Connection;
use rustling_tulip_native::NetEvent;
use rustling_tulip_native::connection_picker::PeerSource;
use serde_json::json;
use support::{
    Fixture, Harness, PROTOCOL, REMOTE, TestDir, pane, remote_pane, repo, saved_remote, seed_ui,
    session, split, tab,
};
use tailnet::{Status, TailnetError};

/// The name the remote host's rows and dialog read.
const NAME: &str = "box";

/// A Tailscale that sees no peers.
struct NoPeers;

impl PeerSource for NoPeers {
    fn status(&self) -> Result<Status, TailnetError> {
        Ok(Status {
            self_node: None,
            peer: std::collections::BTreeMap::default(),
        })
    }
}

fn at(minute: u32) -> DateTime<Utc> {
    format!("2026-01-01T00:{minute:02}:00Z")
        .parse()
        .expect("a fixed timestamp")
}

/// Two running sessions of the remote host, `b1` started after `b2`, and a
/// stopped `b3`.
fn remote_sessions() -> Vec<SessionSnapshot> {
    vec![
        session("b1").started_at(at(5)).build(),
        session("b2").started_at(at(1)).build(),
        session("b3").started_at(at(2)).status("stopped").build(),
    ]
}

/// This PC lists `a1` in repo `r1`; [`REMOTE`] is saved and connected,
/// listing `features` beside the usual ones, repo `r2` and `sessions`.
/// Nothing sent is left unread on either host.
fn remote_listing<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    sessions: Vec<SessionSnapshot>,
    features: &[&str],
) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": REMOTE, "port": 8787 }] }),
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
    host.connect_with_features(features);
    host.send(DaemonMessage::Repos {
        repos: vec![repo("r2", "C:/r2")],
    });
    host.send(DaemonMessage::Sessions { sessions });
    h.sent();
    h.host(&saved_remote()).sent();
    h
}

/// Opens the Connections view, dropping what it sent.
fn open_connections(h: &mut Harness<'_>) {
    h.root_update(|root, _, _| root.set_peer_source(Arc::new(NoPeers)));
    h.click_on("footer-connections");
    h.sent();
    h.host(&saved_remote()).sent();
}

/// Opens the rebind dialog of the host whose Connections row is keyed
/// `key`, dropping what went out before it opened.
fn open_rebind(h: &mut Harness<'_>, key: &str) {
    open_connections(h);
    h.click_on(&format!("connections-row-{key}-rebind"));
    assert!(dialog_open(h), "Rebind… opens the dialog");
}

fn dialog_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.host_rebind_open())
}

fn heading(h: &mut Harness<'_>) -> Option<(String, String)> {
    h.root(|root, _| root.host_rebind_heading())
}

fn controls(h: &mut Harness<'_>) -> Vec<(String, String)> {
    h.root(|root, _| root.host_rebind_controls())
}

/// The follow rows the dialog shows, selector and label.
fn follow_rows(h: &mut Harness<'_>) -> Vec<(String, String)> {
    controls(h)
        .into_iter()
        .filter(|(selector, _)| selector.starts_with("host-rebind-follow-"))
        .collect()
}

/// Whether `sent` is exactly one unbind of [`REMOTE`] with a request id.
fn is_remote_unbind(sent: &[ClientMessage]) -> bool {
    matches!(
        sent,
        [ClientMessage::UnbindHostPanes { host: Some(host), request_id: Some(_) }] if host == REMOTE
    )
}

/// The request id of the `ListHostLayouts` among `sent`.
fn layouts_request(sent: &[ClientMessage]) -> Option<String> {
    sent.iter().find_map(|msg| match msg {
        ClientMessage::ListHostLayouts { request_id } => request_id.clone(),
        _ => None,
    })
}

/// A connection to `target` that was open and dropped.
fn dropped(target: rustling_tulip_native::target::Target) -> Connection {
    let mut conn = Connection::for_target(target);
    conn.on_connecting();
    conn.on_socket_open(8787);
    conn.on_welcome(PROTOCOL, Vec::new(), None);
    let _ = conn.on_closed("socket closed".to_owned());
    conn
}

#[gpui::test]
fn connections_unbind_sends_unbind_host_panes_to_this_pc(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_listing(cx, &dir, remote_sessions(), &[]);
    open_connections(&mut h);

    h.click_on(&format!("connections-row-{REMOTE}-unbind"));

    let sent = h.sent();
    assert!(
        is_remote_unbind(&sent),
        "This PC unbinds the host: {sent:?}"
    );
    let remote = h.host(&saved_remote()).sent();
    assert!(remote.is_empty(), "the remote is sent nothing: {remote:?}");
}

#[gpui::test]
fn connections_rebind_opens_the_dialog_with_the_active_session_count(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_listing(cx, &dir, remote_sessions(), &[]);

    open_rebind(&mut h, REMOTE);

    assert!(
        !h.root(|root, _| root.connection_picker_open()),
        "the Connections view closes first"
    );
    assert_eq!(
        heading(&mut h),
        Some((
            format!("Rebind {NAME}'s sessions"),
            "2 active sessions".to_owned()
        )),
        "the stopped session is not counted"
    );
}

#[gpui::test]
fn rebind_confirm_sends_unbind_then_bind_with_the_auto_arrangement(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_listing(cx, &dir, remote_sessions(), &[]);
    open_rebind(&mut h, REMOTE);
    h.sent();
    h.host(&saved_remote()).sent();

    h.click_on("host-rebind-confirm");

    assert!(!dialog_open(&mut h), "the confirm closes the dialog");
    let sent = h.sent();
    assert!(
        matches!(
            sent.as_slice(),
            [
                ClientMessage::UnbindHostPanes { host: Some(unbind_host), request_id: Some(_) },
                ClientMessage::BindHostSessions {
                    host: Some(bind_host),
                    session_ids,
                    arrangement: HostArrangement::Auto {
                        layout: RearrangeLayout::Grid { cols: 0, aspect: Some(_) },
                        max_per_tab: Some(4),
                    },
                    request_id: Some(_),
                },
            ] if unbind_host == REMOTE && bind_host == REMOTE && *session_ids == ["b2", "b1"]
        ),
        "This PC unbinds, then binds the running sessions oldest first: {sent:?}"
    );
    let remote = h.host(&saved_remote()).sent();
    assert!(remote.is_empty(), "the remote is sent nothing: {remote:?}");
}

#[gpui::test]
fn the_dialog_asks_layouts_only_of_a_host_listing_the_feature(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_listing(
        cx,
        &dir,
        remote_sessions(),
        &[protocol::FEATURE_HOST_LAYOUTS],
    );

    open_rebind(&mut h, "This PC");
    let local = h.sent();
    assert_eq!(
        layouts_request(&local),
        None,
        "This PC's daemon does not list the feature: {local:?}"
    );
    assert_eq!(follow_rows(&mut h), Vec::new(), "nothing to follow");
    h.keys("escape");

    open_rebind(&mut h, REMOTE);
    let remote = h.host(&saved_remote()).sent();
    assert!(
        layouts_request(&remote).is_some(),
        "the host listing the feature is asked: {remote:?}"
    );
    assert_eq!(
        follow_rows(&mut h),
        Vec::new(),
        "no follow row before the answer"
    );
}

#[gpui::test]
fn follow_rows_list_the_hosts_own_layout_first_and_confirm_sends_follow(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_listing(
        cx,
        &dir,
        remote_sessions(),
        &[protocol::FEATURE_HOST_LAYOUTS],
    );
    open_rebind(&mut h, REMOTE);
    let id = layouts_request(&h.host(&saved_remote()).sent()).expect("the host is asked");
    let own_tabs = vec![tab("own", &pane("o1", Some("b1")))];
    h.host(&saved_remote()).send(DaemonMessage::HostLayouts {
        host_name: Some("box-host".to_owned()),
        layouts: vec![
            HostLayout {
                client_id: "c1".to_owned(),
                name: Some("lab".to_owned()),
                tabs: vec![tab("lab", &pane("l1", Some("b2")))],
            },
            HostLayout {
                client_id: "c2".to_owned(),
                name: Some("box-host".to_owned()),
                tabs: own_tabs.clone(),
            },
        ],
        request_id: Some(id),
    });

    assert_eq!(
        follow_rows(&mut h),
        [
            (
                "host-rebind-follow-c2".to_owned(),
                "Follow box-host's layout".to_owned()
            ),
            (
                "host-rebind-follow-c1".to_owned(),
                "Follow lab's layout".to_owned()
            ),
        ],
        "the host's own layout comes first"
    );
    h.click_on("host-rebind-follow-c2");
    h.sent();
    h.click_on("host-rebind-confirm");

    let sent = h.sent();
    let follow = sent.iter().find_map(|msg| match msg {
        ClientMessage::BindHostSessions { arrangement, .. } => Some(arrangement.clone()),
        _ => None,
    });
    assert_eq!(
        follow,
        Some(HostArrangement::Follow { tabs: own_tabs }),
        "the bind follows the chosen layout: {sent:?}"
    );
}

#[gpui::test]
fn a_session_spawned_while_open_is_bound_on_confirm(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_listing(cx, &dir, remote_sessions(), &[]);
    open_rebind(&mut h, REMOTE);

    h.host(&saved_remote()).send(DaemonMessage::SessionUpdated {
        session: session("b4").started_at(at(9)).build(),
        request_id: None,
    });

    assert_eq!(
        heading(&mut h).map(|(_, count)| count).as_deref(),
        Some("3 active sessions")
    );
    h.sent();
    h.click_on("host-rebind-confirm");
    let sent = h.sent();
    let bound = sent.iter().find_map(|msg| match msg {
        ClientMessage::BindHostSessions { session_ids, .. } => Some(session_ids.clone()),
        _ => None,
    });
    assert_eq!(
        bound,
        Some(vec!["b2".to_owned(), "b1".to_owned(), "b4".to_owned()]),
        "{sent:?}"
    );
}

#[gpui::test]
fn the_jump_palette_lists_unbind_and_rebind_per_online_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_listing(cx, &dir, remote_sessions(), &[]);
    let rows = |h: &mut Harness<'_>| {
        let at = h.cell_center("p1", 0, 0);
        h.click(at, Modifiers::none());
        h.keys("ctrl-shift-p");
        let rows = h.root(|root, _| root.quick_jump_rows());
        h.keys("escape");
        rows
    };
    let wanted = |name: &str| {
        [
            format!("[SHELL] Unbind {name}'s sessions"),
            format!("[SHELL] Rebind {name}'s sessions…"),
        ]
    };

    let online = rows(&mut h);
    for row in wanted("This PC").iter().chain(wanted(NAME).iter()) {
        assert!(online.contains(row), "{row} is listed: {online:?}");
    }

    h.host(&saved_remote())
        .event(NetEvent::State(dropped(saved_remote())));
    let offline = rows(&mut h);
    for row in &wanted(NAME) {
        assert!(
            !offline.contains(row),
            "an offline host is offered no {row}: {offline:?}"
        );
    }
    for row in &wanted("This PC") {
        assert!(
            offline.contains(row),
            "This PC is always online: {offline:?}"
        );
    }
}

#[gpui::test]
fn the_host_header_menu_unbinds_and_rebinds(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_listing(cx, &dir, remote_sessions(), &[]);

    h.right_click_on(&format!("host-header-{REMOTE}"));
    assert!(h.root(|root, _| root.host_menu_open()), "the menu opens");
    h.click_on("host-menu-unbind");
    assert!(!h.root(|root, _| root.host_menu_open()), "a pick closes it");
    let sent = h.sent();
    assert!(
        is_remote_unbind(&sent),
        "This PC unbinds the host: {sent:?}"
    );

    h.right_click_on(&format!("host-header-{REMOTE}"));
    h.click_on("host-menu-rebind");
    assert!(dialog_open(&mut h), "Rebind… opens the dialog");
    assert_eq!(
        heading(&mut h).map(|(title, _)| title),
        Some(format!("Rebind {NAME}'s sessions"))
    );
}

#[gpui::test]
fn disconnecting_the_host_closes_the_dialog(cx: &mut TestAppContext) {
    const DEN: &str = "den.tail1234.ts.net";
    let dir = TestDir::new();
    let daemon = "6f1e2d3c-0000-4000-8000-0000000000aa";
    // DEN is saved first, so it owns a daemon both reach.
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [
            { "name": DEN, "port": 8787 },
            { "name": REMOTE, "port": 8787 },
        ] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::single(session("a1").build()));
    let mut host = h.host(&saved_remote());
    host.connect();
    host.send(support::welcome_with_id(PROTOCOL, daemon));
    host.send(DaemonMessage::Sessions {
        sessions: remote_sessions(),
    });
    open_rebind(&mut h, REMOTE);

    // DEN answering from the same daemon makes the window let REMOTE go.
    let den = rustling_tulip_native::target::Target::tailnet(DEN, Some(8787));
    let mut other = h.host(&den);
    other.connect();
    other.send(support::welcome_with_id(PROTOCOL, daemon));
    let remote_key = rustling_tulip_native::target::HostKey::of(&saved_remote());
    assert!(
        !h.root(|root, _| root.held_hosts()).contains(&remote_key),
        "the window let the duplicate host go"
    );

    assert!(!dialog_open(&mut h), "the dialog closes with its host");
    if dialog_open(&mut h) {
        h.click_on("host-rebind-confirm");
    }
    let sent = h.sent();
    assert!(
        !sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::UnbindHostPanes { .. } | ClientMessage::BindHostSessions { .. }
        )),
        "nothing is unbound or bound for a host the window let go: {sent:?}"
    );
}

#[gpui::test]
fn escape_closes_the_dialog_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_listing(cx, &dir, remote_sessions(), &[]);
    open_rebind(&mut h, REMOTE);
    h.sent();
    h.host(&saved_remote()).sent();

    h.keys("escape");

    assert!(!dialog_open(&mut h), "Esc closes the dialog");
    assert!(h.sent().is_empty(), "This PC is sent nothing");
    assert!(
        h.host(&saved_remote()).sent().is_empty(),
        "the host is sent nothing"
    );
}

/// The `claim` flags of the `Resize`s in `sent` for session `id`, in order.
fn resize_claims(sent: &[ClientMessage], id: &str) -> Vec<bool> {
    sent.iter()
        .filter_map(|m| match m {
            ClientMessage::Resize {
                session_id, claim, ..
            } if session_id == id => Some(*claim),
            _ => None,
        })
        .collect()
}

/// No claimed `Resize` for `id` in `sent`, and at least one sent.
fn assert_sized_unclaimed(sent: &[ClientMessage], id: &str, why: &str) {
    let claims = resize_claims(sent, id);
    assert!(
        !claims.is_empty() && claims.iter().all(|claim| !claim),
        "{why}: {claims:?}"
    );
}

/// The request id of the `BindHostSessions` among `sent`.
fn bind_request(sent: &[ClientMessage]) -> String {
    sent.iter()
        .find_map(|msg| match msg {
            ClientMessage::BindHostSessions { request_id, .. } => request_id.clone(),
            _ => None,
        })
        .expect("the confirm binds the host's sessions with a request id")
}

/// The tab a rebind of This PC's sessions adds, `a1` beside `a2`.
fn rebound_tab() -> TabEntry {
    tab(
        "t7",
        &split(
            SplitDirection::Horizontal,
            pane("p1", Some("a1")),
            pane("p2", Some("a2")),
        ),
    )
}

/// The tab a rebind of [`REMOTE`]'s sessions adds, `b2` beside `b1`.
fn rebound_remote_tab() -> TabEntry {
    tab(
        "t7",
        &split(
            SplitDirection::Horizontal,
            remote_pane("p1", "b2"),
            remote_pane("p2", "b1"),
        ),
    )
}

/// This PC's rebind dialog open over the Connections view, over `fixture`.
fn this_pc_rebind<'a>(cx: &'a mut TestAppContext, dir: &TestDir, fixture: &Fixture) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, fixture);
    h.sent();
    open_connections(&mut h);
    h.click_on("connections-row-This PC-rebind");
    assert!(dialog_open(&mut h), "Rebind… opens the dialog");
    h
}

/// This PC running `a1` and `a2` with no tabs.
fn two_sessions() -> Fixture {
    Fixture {
        sessions: vec![session("a1").build(), session("a2").build()],
        ..Fixture::default()
    }
}

#[gpui::test]
fn rebind_claims_each_rebound_sessions_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_listing(cx, &dir, remote_sessions(), &[]);
    open_rebind(&mut h, REMOTE);
    h.sent();
    h.host(&saved_remote()).sent();

    h.click_on("host-rebind-confirm");
    let bind_id = bind_request(&h.sent());
    assert!(!bind_id.is_empty(), "the bind carries a request id");

    h.send(DaemonMessage::TabUpdated {
        tab: rebound_remote_tab(),
    });
    // The new tab takes no focus: the panes size the PTY once it has it.
    h.click_on("tab-t7");
    let mut remote = h.host(&saved_remote());
    remote.answer_scrollback("b2", b"");
    remote.answer_scrollback("b1", b"");
    let sent = remote.sent();
    for id in ["b2", "b1"] {
        assert_eq!(
            resize_claims(&sent, id).first(),
            Some(&true),
            "the pane rebound to {id} claims its size: {sent:?}"
        );
    }
}

#[gpui::test]
fn a_refused_rebind_does_not_claim_a_later_bind(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_listing(cx, &dir, remote_sessions(), &[]);
    open_rebind(&mut h, REMOTE);
    h.sent();
    h.host(&saved_remote()).sent();

    h.click_on("host-rebind-confirm");
    let bind_id = bind_request(&h.sent());
    h.send(DaemonMessage::Error {
        message: "refused".to_owned(),
        request_id: Some(bind_id),
        code: None,
    });
    h.send(DaemonMessage::TabUpdated {
        tab: rebound_remote_tab(),
    });
    h.click_on("tab-t7");
    let mut remote = h.host(&saved_remote());
    remote.answer_scrollback("b2", b"");
    remote.answer_scrollback("b1", b"");
    let sent = remote.sent();
    assert_sized_unclaimed(&sent, "b2", "a refused rebind claims nothing");
    assert_sized_unclaimed(&sent, "b1", "a refused rebind claims nothing");
}

#[gpui::test]
fn a_rebind_before_a_reconnect_does_not_claim(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = two_sessions();
    let mut h = this_pc_rebind(cx, &dir, &fixture);
    h.sent();
    h.click_on("host-rebind-confirm");
    h.sent();

    h.send_welcome(1);
    h.sent();
    h.load(&fixture);
    h.send(DaemonMessage::TabUpdated { tab: rebound_tab() });
    h.answer_scrollback("a1", b"");
    h.answer_scrollback("a2", b"");
    let sent = h.sent();
    assert_sized_unclaimed(&sent, "a1", "a reconnect drops the rebind's claim");
    assert_sized_unclaimed(&sent, "a2", "a reconnect drops the rebind's claim");
}

#[gpui::test]
fn rebinding_this_pcs_sessions_claims_their_size(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = two_sessions();
    let mut h = this_pc_rebind(cx, &dir, &fixture);
    h.sent();

    h.click_on("host-rebind-confirm");
    h.send(DaemonMessage::TabUpdated { tab: rebound_tab() });
    h.answer_scrollback("a1", b"");
    h.answer_scrollback("a2", b"");
    let sent = h.sent();
    for id in ["a1", "a2"] {
        assert_eq!(
            resize_claims(&sent, id).first(),
            Some(&true),
            "the pane rebound to {id} claims its size: {sent:?}"
        );
    }
}
