//! Connections specs: the footer's Connections button (and the
//! first-connect overlay's, which covers the footer) opens a modal listing
//! This PC, the saved hosts and the owner's online tailnet peers. Each saved
//! host's row switches its connection on and off, saved in `native-ui.json`,
//! and every held host's row shows its own state. Adding a host saves it and
//! connects it; Forget drops a saved host with its connection and rows. A
//! refused name or a failed scan shows its text.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use daemon_client::device_tokens::DeviceTokenStore as _;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use gpui::{Modifiers, TestAppContext, point, px};
use protocol::{DaemonMessage, WorktreeCleanupFailure};
use rustling_tulip_native::connection_picker::{
    AddMode, PeerSource, RowDetail, RowState, SshTestCancel, SshTester,
};
use rustling_tulip_native::target::{HostKey, SshHost, Target};
use rustling_tulip_native::{Connection, NetCommand, NetEvent, RootView};
use serde_json::{Value, json};
use support::{Fixture, Harness, PROTOCOL, TestDir};
use tailnet::{PeerStatus, Status, TailnetError};

const HOST: &str = "box.tail1234.ts.net";
const DEN: &str = "den.tail1234.ts.net";
const PEER: &str = "peer.tail1234.ts.net";
const OWNER: i64 = 42;
const SLASH_REFUSAL: &str = "That is not a host name: use letters, digits, hyphens and dots.";

/// A Tailscale that answers every scan with the same outcome.
struct FakePeers(Result<Status, TailnetError>);

impl PeerSource for FakePeers {
    fn status(&self) -> Result<Status, TailnetError> {
        self.0.clone()
    }
}

/// An online, untagged, unshared node of the owner named `name`.
fn node(name: &str) -> PeerStatus {
    PeerStatus {
        dns_name: format!("{name}."),
        user_id: OWNER,
        online: true,
        ..PeerStatus::default()
    }
}

/// A tailnet where this PC sees `peers`.
fn tailnet_with(peers: &[&str]) -> FakePeers {
    FakePeers(Ok(Status {
        self_node: Some(node("this-pc.tail1234.ts.net")),
        peer: peers
            .iter()
            .enumerate()
            .map(|(i, name)| (format!("nodekey:{i}"), node(name)))
            .collect(),
    }))
}

/// Writes `native-ui.json` in `dir` before the window opens.
fn seed_ui(dir: &TestDir, ui: &Value) {
    std::fs::write(
        dir.path().join("native-ui.json"),
        serde_json::to_vec_pretty(ui).expect("serialize native-ui.json"),
    )
    .expect("write native-ui.json");
}

/// `native-ui.json` as it stands on disk.
fn saved_ui(dir: &TestDir) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("native-ui.json")).expect("native-ui.json"),
    )
    .expect("native-ui.json is JSON")
}

/// Installs `source` as the picker's Tailscale and opens the picker with
/// the footer's Connections button.
fn open_picker(h: &mut Harness<'_>, source: FakePeers) {
    h.root_update(move |root, _, _| root.set_peer_source(Arc::new(source)));
    h.click_on("footer-connections");
}

fn is_open(h: &mut Harness<'_>) -> bool {
    h.root(|root, _| root.connection_picker_open())
}

/// Each row's label and whether the window holds a connection to its host.
fn rows(h: &mut Harness<'_>) -> Vec<(String, bool)> {
    h.root(|root, _| root.connection_picker_rows())
}

fn focus(h: &mut Harness<'_>) -> Option<String> {
    h.root(RootView::connection_picker_focus)
}

/// Types `text` into the field that has the keyboard.
fn type_text(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    h.keys(&keys.join(" "));
}

/// The remote targets the window opened a connection to, in order.
fn opened(h: &Harness<'_>) -> Vec<Target> {
    h.opened_targets().into_iter().skip(1).collect()
}

/// The hosts the window holds a connection to.
fn held(h: &mut Harness<'_>) -> Vec<HostKey> {
    h.root(|root, _| root.held_hosts())
}

fn tailnet(name: &str) -> Target {
    Target::tailnet(name, None)
}

fn row(label: &str) -> String {
    format!("connections-row-{label}")
}

/// Clicks the row keyed `key` at its left end, clear of its buttons.
fn click_row(h: &mut Harness<'_>, key: &str) {
    let bounds = h.bounds(&row(key));
    h.click(
        point(bounds.left() + px(12.0), bounds.center().y),
        Modifiers::none(),
    );
}

/// Whether the last frame drew what `selector` tags.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.width > px(0.0)
}

/// The selector of the switch on the row keyed `key`.
fn switch(key: &str) -> String {
    format!("connections-row-{key}-enabled")
}

/// The second line and chip of the row keyed `key`.
fn detail(h: &mut Harness<'_>, key: &str) -> Option<RowDetail> {
    let key = key.to_owned();
    h.root(move |root, _| root.connection_picker_row_detail(&key))
}

/// The text of the chip on the row keyed `key`.
fn chip(h: &mut Harness<'_>, key: &str) -> Option<String> {
    detail(h, key)
        .and_then(|detail| detail.state)
        .map(|state| state.label())
}

fn add_mode(h: &mut Harness<'_>) -> Option<AddMode> {
    h.root(|root, _| root.connection_picker_add_mode())
}

/// The focus after each of `presses` Tab presses.
fn tab_walk(h: &mut Harness<'_>, presses: usize) -> Vec<String> {
    (0..presses)
        .map(|_| {
            h.keys("tab");
            focus(h).expect("the picker is open")
        })
        .collect()
}

fn owned_rows(rows: &[(&str, bool)]) -> Vec<(String, bool)> {
    rows.iter()
        .map(|(label, held)| ((*label).to_owned(), *held))
        .collect()
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

/// A connection to `target` that dialled and was refused.
fn closed(target: Target) -> Connection {
    let mut conn = Connection::for_target(target);
    conn.on_connecting();
    let _ = conn.on_closed("connection refused".to_owned());
    conn
}

/// A connection to `target` open and welcomed.
fn open_conn(target: Target) -> Connection {
    let mut conn = Connection::for_target(target);
    conn.on_connecting();
    conn.on_socket_open(8787);
    conn.on_welcome(PROTOCOL, support::daemon_features());
    conn
}

/// A connection to `target` whose daemon does not know this PC.
fn not_approved(target: Target, had_token: bool) -> Connection {
    let mut conn = Connection::for_target(target);
    conn.on_connecting();
    conn.on_socket_open(8787);
    conn.on_not_approved("device not approved".to_owned(), had_token);
    conn
}

/// Has `target`'s daemon say `Welcome`, naming itself `id`.
fn welcome_as(h: &mut Harness<'_>, target: &Target, id: &str) {
    let mut host = h.host(target);
    host.event(NetEvent::State(open_conn(target.clone())));
    host.send(support::welcome_with_id(PROTOCOL, id));
}

#[gpui::test]
fn connections_button_opens_the_picker(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    assert!(!is_open(&mut h));

    open_picker(&mut h, tailnet_with(&[]));

    assert!(is_open(&mut h));
    assert_eq!(rows(&mut h), owned_rows(&[("This PC", true)]));
    assert!(
        h.bounds("connections-panel").size.width > px(0.0),
        "the picker is drawn"
    );
}

#[gpui::test]
fn picker_lists_saved_hosts_and_peers(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 9443 }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());

    open_picker(&mut h, tailnet_with(&[PEER, HOST]));

    assert_eq!(
        rows(&mut h),
        owned_rows(&[("This PC", true), (HOST, true), (PEER, false)]),
        "this PC, the saved host once, then the peer not saved"
    );
    assert_eq!(
        h.root(|root, _| root.connection_picker_scan_text()),
        None,
        "the scan answered"
    );
}

#[gpui::test]
fn picking_a_peer_saves_it_and_connects_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[PEER]));
    h.commands();

    h.click_on(&row(PEER));

    assert_eq!(opened(&h), [Target::tailnet(PEER, Some(8787))]);
    assert_eq!(held(&mut h), [HostKey::Local, HostKey::of(&tailnet(PEER))]);
    assert_eq!(
        saved_ui(&dir)["saved_hosts"],
        json!([{ "name": PEER, "port": 8787 }]),
        "a picked peer is remembered with the port it was dialled on"
    );
    assert!(!is_open(&mut h), "picking a peer closes the picker");
    assert!(h.commands().is_empty(), "This PC is asked for nothing");
}

#[gpui::test]
fn switching_a_host_off_disconnects_it_saves_it_and_keeps_its_rows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 8787 }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let mut host = h.host(&tailnet(HOST));
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![support::session("b1").in_repo("gone").build()],
    });
    host.sent();
    host.commands();
    h.sent();
    h.commands();
    open_picker(&mut h, tailnet_with(&[]));

    h.click_on(&switch(HOST));

    assert_eq!(
        held(&mut h),
        [HostKey::Local],
        "the host's connection is dropped"
    );
    assert_eq!(
        saved_ui(&dir)["saved_hosts"],
        json!([{ "name": HOST, "port": 8787, "enabled": false }])
    );
    assert!(
        listed(&mut h, "b1"),
        "the host's sessions stay in the sidebar"
    );
    assert!(h.sent().is_empty(), "nothing is sent to This PC");
    assert!(h.commands().is_empty(), "nor asked of its network thread");
    let mut host = h.host(&tailnet(HOST));
    assert!(host.sent().is_empty(), "nothing is sent to the host");
    assert!(host.commands().is_empty());
    assert!(is_open(&mut h), "the picker stays open");
    assert_eq!(chip(&mut h, HOST).as_deref(), Some("Off"));
}

#[gpui::test]
fn switching_a_host_on_connects_it_and_saves_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 8787, "enabled": false }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    assert_eq!(
        opened(&h),
        [] as [Target; 0],
        "a host switched off is not dialled"
    );
    open_picker(&mut h, tailnet_with(&[]));
    h.commands();

    h.click_on(&switch(HOST));

    assert_eq!(opened(&h), [tailnet(HOST)]);
    assert_eq!(held(&mut h), [HostKey::Local, HostKey::of(&tailnet(HOST))]);
    assert_eq!(
        saved_ui(&dir)["saved_hosts"],
        json!([{ "name": HOST, "port": 8787 }])
    );
    assert!(h.commands().is_empty(), "This PC is asked for nothing");
    assert!(is_open(&mut h), "the picker stays open");
}

#[gpui::test]
fn this_pc_has_no_switch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 8787 }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[]));
    assert!(!drawn(&mut h, &switch("This PC")));
    assert!(drawn(&mut h, &switch(HOST)), "a saved host has one");
    h.commands();

    h.click_on(&row("This PC"));

    assert!(is_open(&mut h), "picking This PC does nothing");
    assert!(h.commands().is_empty());
    assert_eq!(opened(&h), [tailnet(HOST)]);
    assert_eq!(held(&mut h), [HostKey::Local, HostKey::of(&tailnet(HOST))]);
}

#[gpui::test]
fn picking_a_saved_host_toggles_its_switch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 8787 }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[]));

    click_row(&mut h, HOST);
    assert_eq!(held(&mut h), [HostKey::Local]);
    assert_eq!(saved_ui(&dir)["saved_hosts"][0]["enabled"], json!(false));

    h.keys("enter");
    assert_eq!(held(&mut h), [HostKey::Local, HostKey::of(&tailnet(HOST))]);
    assert_eq!(
        saved_ui(&dir)["saved_hosts"],
        json!([{ "name": HOST, "port": 8787 }])
    );
}

#[gpui::test]
fn every_held_host_shows_its_own_state_chip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [
            { "name": HOST, "port": 8787 },
            { "name": DEN, "port": 8787 },
        ] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.tokens().set(HOST, "t1").expect("seed a token");
    h.tokens().set(DEN, "t2").expect("seed a token");
    h.host(&tailnet(HOST)).connect();
    h.host(&tailnet(DEN))
        .event(NetEvent::State(closed(tailnet(DEN))));

    open_picker(&mut h, tailnet_with(&[]));

    assert_eq!(chip(&mut h, "This PC").as_deref(), Some("Connected"));
    assert_eq!(chip(&mut h, HOST).as_deref(), Some("Connected"));
    assert_eq!(chip(&mut h, DEN).as_deref(), Some("Disconnected"));
    assert!(h.bounds(&format!("connections-state-{DEN}")).size.width > px(0.0));
}

#[gpui::test]
fn a_disabled_host_shows_off(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [{ "name": HOST, "port": 9443, "enabled": false }],
            "host_session_counts": { HOST: 3 },
        }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());

    open_picker(&mut h, tailnet_with(&[]));

    assert_eq!(chip(&mut h, HOST).as_deref(), Some("Off"));
    assert_eq!(
        detail(&mut h, HOST).map(|detail| detail.line),
        Some("Tailnet · port 9443 · 3 sessions last time".to_owned())
    );
    assert!(
        drawn(&mut h, &format!("{}-empty", switch(HOST))),
        "its switch is off"
    );
    assert!(!drawn(&mut h, &format!("connections-row-{HOST}-pair")));
    assert!(!drawn(&mut h, &format!("connections-row-{HOST}-reconnect")));
}

#[gpui::test]
fn a_duplicate_host_shows_same_daemon_as_its_owner(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [
            { "name": HOST, "port": 8787 },
            { "kind": "ssh", "name": "me@devbox" },
        ] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let ssh_box = Target::parse_ssh("me@devbox", None).expect("an SSH host string");
    welcome_as(
        &mut h,
        &tailnet(HOST),
        "6f1e2d3c-0000-4000-8000-000000000001",
    );
    welcome_as(&mut h, &ssh_box, "6f1e2d3c-0000-4000-8000-000000000001");

    open_picker(&mut h, tailnet_with(&[]));

    assert_eq!(
        chip(&mut h, "ssh:me@devbox").as_deref(),
        Some("Same daemon as box")
    );
    assert!(
        drawn(&mut h, &format!("{}-tick", switch("ssh:me@devbox"))),
        "its switch stays on"
    );
}

#[gpui::test]
fn reconnect_now_restarts_only_that_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [
            { "name": HOST, "port": 8787 },
            { "name": DEN, "port": 8787 },
        ] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.tokens().set(HOST, "t1").expect("seed a token");
    h.tokens().set(DEN, "t2").expect("seed a token");
    h.host(&tailnet(HOST))
        .event(NetEvent::State(closed(tailnet(HOST))));
    h.host(&tailnet(DEN)).connect();
    open_picker(&mut h, tailnet_with(&[]));
    assert!(
        !drawn(&mut h, &format!("connections-row-{DEN}-reconnect")),
        "an open host offers no reconnect"
    );
    h.commands();
    h.host(&tailnet(DEN)).commands();

    h.click_on(&format!("connections-row-{HOST}-reconnect"));

    let restarts = h
        .host(&tailnet(HOST))
        .commands()
        .into_iter()
        .filter(|command| matches!(command, NetCommand::Restart))
        .count();
    assert_eq!(restarts, 1, "the host's network thread redials");
    assert!(h.host(&tailnet(DEN)).commands().is_empty());
    assert!(h.commands().is_empty(), "This PC is not restarted");
}

#[gpui::test]
fn typed_name_with_a_slash_shows_the_message_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[]));
    h.commands();

    h.click_on("connections-add-host");
    assert_eq!(focus(&mut h).as_deref(), Some("connections-name"));
    type_text(&mut h, "box.tail1234.ts.net/ws");
    h.keys("enter");

    assert_eq!(
        h.root(|root, _| root.connection_picker_error()).as_deref(),
        Some(SLASH_REFUSAL)
    );
    assert_eq!(opened(&h), [] as [Target; 0]);
    assert!(is_open(&mut h), "a refused name keeps the picker open");
}

#[gpui::test]
fn forget_removes_a_saved_host_and_the_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 9443 }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[]));
    h.commands();

    h.click_on(&format!("connections-forget-{HOST}"));

    assert_eq!(rows(&mut h), owned_rows(&[("This PC", true)]));
    assert!(is_open(&mut h), "the picker stays open after Forget");
    assert_eq!(saved_ui(&dir)["saved_hosts"], json!([]));
    assert!(h.commands().is_empty(), "This PC is asked for nothing");
}

#[gpui::test]
fn forget_disconnects_the_host_and_clears_its_rows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 8787 }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::single(support::session("a1").build()));
    let mut host = h.host(&tailnet(HOST));
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![support::session("b1").in_repo("gone").build()],
    });
    assert!(listed(&mut h, "b1"));
    open_picker(&mut h, tailnet_with(&[]));
    h.commands();

    h.click_on(&format!("connections-forget-{HOST}"));

    assert_eq!(held(&mut h), [HostKey::Local]);
    assert!(!listed(&mut h, "b1"), "the host's rows are gone");
    assert!(listed(&mut h, "a1"), "This PC's rows stay");
    assert_eq!(saved_ui(&dir)["saved_hosts"], json!([]));
    assert!(h.commands().is_empty(), "This PC is asked for nothing");
}

#[gpui::test]
fn forget_drops_a_hosts_session_count_for_good(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [{ "name": HOST, "port": 8787 }],
            "host_session_counts": { HOST: 1 },
        }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let saved = Target::tailnet(HOST, Some(8787));
    let mut host = h.host(&saved);
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![support::session("s1").build()],
    });
    open_picker(&mut h, tailnet_with(&[]));
    h.click_on(&format!("connections-forget-{HOST}"));
    assert!(saved_ui(&dir)["host_session_counts"][HOST].is_null());

    h.host(&saved).send(DaemonMessage::SessionUpdated {
        session: support::session("s2").build(),
        request_id: None,
    });
    h.advance(Duration::from_millis(600));
    let ui = saved_ui(&dir);
    assert!(
        ui["host_session_counts"][HOST].is_null(),
        "a later session does not record it again: {ui}"
    );
}

#[gpui::test]
fn picker_is_reachable_while_the_first_connect_overlay_shows(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 9443 }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.lose_connection();
    assert!(
        h.bounds("connecting-overlay=Starting daemon…").size.width > px(0.0),
        "the first-connect overlay shows"
    );
    h.root_update(|root, _, _| root.set_peer_source(Arc::new(tailnet_with(&[]))));

    h.click_on("overlay-connections");
    assert!(
        is_open(&mut h),
        "the overlay's Connections opens the picker"
    );
    click_row(&mut h, HOST);

    assert_eq!(
        saved_ui(&dir)["saved_hosts"],
        json!([{ "name": HOST, "port": 9443, "enabled": false }]),
        "the picker is drawn above the overlay and takes the click"
    );
}

#[gpui::test]
fn escape_closes_the_picker(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[PEER]));
    h.commands();

    h.keys("escape");

    assert!(!is_open(&mut h));
    assert_eq!(opened(&h), [] as [Target; 0]);
    assert!(h.commands().is_empty());
}

#[gpui::test]
fn adding_a_host_saves_it_enabled_and_connects_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[]));
    h.commands();

    h.click_on("connections-add-host");
    type_text(&mut h, HOST);
    h.keys("enter");

    assert_eq!(opened(&h), [tailnet(HOST)]);
    assert_eq!(held(&mut h), [HostKey::Local, HostKey::of(&tailnet(HOST))]);
    assert_eq!(
        saved_ui(&dir)["saved_hosts"],
        json!([{ "name": HOST, "port": 8787 }]),
        "saved switched on"
    );
    assert!(!is_open(&mut h), "Add closes the picker");
    assert!(h.commands().is_empty(), "This PC is asked for nothing");
}

#[gpui::test]
fn typed_host_with_a_custom_port_dials_that_port(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[]));
    h.commands();

    h.click_on("connections-add-host");
    type_text(&mut h, HOST);
    h.keys("tab");
    assert_eq!(focus(&mut h).as_deref(), Some("connections-port"));
    type_text(&mut h, "9443");
    h.keys("enter");

    assert_eq!(opened(&h), [Target::tailnet(HOST, Some(9443))]);
    assert!(!is_open(&mut h));
    assert_eq!(
        saved_ui(&dir)["saved_hosts"],
        json!([{ "name": HOST, "port": 9443 }])
    );
}

#[gpui::test]
fn picker_shows_tailscale_not_running_text(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 9443 }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());

    open_picker(&mut h, FakePeers(Err(TailnetError::NotRunning)));

    assert_eq!(
        h.root(|root, _| root.connection_picker_scan_text()),
        Some(TailnetError::NotRunning.to_string())
    );
    assert_eq!(
        rows(&mut h),
        owned_rows(&[("This PC", true), (HOST, true)]),
        "this PC and the saved hosts are still offered"
    );
}

#[gpui::test]
fn typed_known_host_on_a_new_port_updates_the_saved_port(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 8787 }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[]));
    h.commands();

    h.click_on("connections-add-host");
    assert_eq!(focus(&mut h).as_deref(), Some("connections-name"));
    type_text(&mut h, HOST);
    h.keys("tab");
    type_text(&mut h, "9443");
    h.keys("enter");

    assert_eq!(
        opened(&h),
        [
            Target::tailnet(HOST, Some(8787)),
            Target::tailnet(HOST, Some(9443))
        ],
        "the host is dialled again on its new port"
    );
    assert_eq!(held(&mut h), [HostKey::Local, HostKey::of(&tailnet(HOST))]);
    let ui = saved_ui(&dir);
    assert_eq!(ui["saved_hosts"], json!([{ "name": HOST, "port": 9443 }]));
}

#[gpui::test]
fn typing_in_the_picker_does_not_reach_the_layers_below(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[]));
    h.click_on("connections-add-host");
    assert_eq!(focus(&mut h).as_deref(), Some("connections-name"));

    h.keys("ctrl-,");

    assert!(
        !h.root(|root, _| root.settings_open()),
        "Settings stays closed behind the picker"
    );
    assert!(is_open(&mut h));
}

#[gpui::test]
fn a_notice_under_the_picker_does_not_take_its_keys(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.send(DaemonMessage::WorktreeCleanupFailed {
        session_id: "s1".to_owned(),
        session_label: "feat/x".to_owned(),
        failures: vec![WorktreeCleanupFailure {
            member_path: "C:/wts/wt.feat-x/a".to_owned(),
            repo_path: "C:/src/a".to_owned(),
            reason: "fs::remove_dir_all: access denied".to_owned(),
            locking_processes: Vec::new(),
        }],
    });
    assert!(h.root(|root, _| root.cleanup_failed_open()));
    h.root_update(|root, window, cx| {
        root.set_peer_source(Arc::new(tailnet_with(&[])));
        root.open_connection_picker(window, cx);
    });
    h.click_on("connections-add-host");
    assert_eq!(focus(&mut h).as_deref(), Some("connections-name"));

    h.keys("space");

    assert!(
        h.root(|root, _| root.cleanup_failed_open()),
        "Space in the Name field does not press the hidden dialog's Ignore"
    );
    assert!(is_open(&mut h));
}

const SSH_HOST: &str = "devbox";
const DENIED: &str = "me@devbox: Permission denied (publickey).";

/// A Test connection that answers every test with `answer`, or, with none,
/// never answers and keeps each test's cancel signal to look at.
#[derive(Default)]
struct FakeSsh {
    answer: Option<Result<(), String>>,
    tested: Mutex<Vec<String>>,
    cancels: Mutex<Vec<SshTestCancel>>,
}

impl FakeSsh {
    fn answering(answer: Result<(), String>) -> Arc<Self> {
        Arc::new(Self {
            answer: Some(answer),
            ..Self::default()
        })
    }

    /// The host strings tested so far.
    fn tested(&self) -> Vec<String> {
        self.tested
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether every test still waiting was told to stop (and one was).
    fn cancelled(&self) -> bool {
        let mut cancels = self.cancels.lock().unwrap_or_else(PoisonError::into_inner);
        !cancels.is_empty()
            && cancels
                .iter_mut()
                .all(|cancel| !matches!(cancel.try_recv(), Ok(None)))
    }
}

impl SshTester for FakeSsh {
    fn test(&self, host: SshHost, cancel: SshTestCancel) -> BoxFuture<'static, Result<(), String>> {
        self.tested
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(host.name().to_owned());
        if let Some(answer) = &self.answer {
            return futures::future::ready(answer.clone()).boxed();
        }
        self.cancels
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(cancel);
        futures::future::pending().boxed()
    }
}

/// Opens the picker with `tester` as its Test connection and no tailnet
/// peers, then the add form, switched to SSH.
fn open_ssh_form(h: &mut Harness<'_>, tester: Arc<FakeSsh>) {
    h.root_update(move |root, _, _| root.set_ssh_tester(tester));
    open_picker(h, tailnet_with(&[]));
    h.commands();
    h.click_on("connections-add-host");
    h.click_on("connections-add-mode-ssh");
    assert!(h.root(|root, _| root.connection_picker_adding_ssh()));
    assert_eq!(
        focus(h).as_deref(),
        Some("connections-ssh-form-host"),
        "the form opens on its Host field"
    );
}

fn ssh_result(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.connection_picker_ssh_result())
}

fn picker_error(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.connection_picker_error())
}

#[gpui::test]
fn picker_lists_a_saved_ssh_host_with_its_ssh_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [
            { "name": HOST, "port": 8787 },
            { "kind": "ssh", "name": "me@devbox", "enabled": false },
        ] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());

    open_picker(&mut h, tailnet_with(&[]));

    assert_eq!(
        rows(&mut h),
        owned_rows(&[("This PC", true), (HOST, true), ("me@devbox", false)])
    );
    assert_eq!(
        h.root(|root, _| root.connection_picker_ssh_rows()),
        ["me@devbox"]
    );
    assert_eq!(
        h.root(|root, _| root.connection_picker_not_paired()),
        [HOST],
        "an SSH host has no device token and is never tagged Not paired"
    );
    assert_eq!(
        detail(&mut h, "ssh:me@devbox").map(|detail| detail.line),
        Some("SSH · rustling-tulipd bridge".to_owned())
    );
    assert!(h.bounds("connections-detail-ssh:me@devbox").size.width > px(0.0));
    assert!(h.bounds("connections-row-ssh:me@devbox").size.width > px(0.0));
}

#[gpui::test]
fn add_ssh_host_refuses_a_bad_host_with_its_text_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let tester = FakeSsh::answering(Ok(()));
    open_ssh_form(&mut h, Arc::clone(&tester));

    h.click_on("connections-ssh-form-test");
    assert_eq!(
        picker_error(&mut h).as_deref(),
        Some("Type an SSH host: an alias from your SSH config, or user@host.")
    );

    h.click_on("connections-ssh-form-host");
    type_text(&mut h, "-x");
    assert_eq!(picker_error(&mut h), None, "an edit clears the refusal");
    h.click_on("connections-add-submit");
    assert_eq!(
        picker_error(&mut h).as_deref(),
        Some("An SSH host cannot start with a dash.")
    );

    assert_eq!(tester.tested(), [] as [String; 0]);
    assert_eq!(opened(&h), [] as [Target; 0]);
    assert!(is_open(&mut h));
}

#[gpui::test]
fn enter_runs_test_connection_and_shows_connected_saving_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let tester = FakeSsh::answering(Ok(()));
    open_ssh_form(&mut h, Arc::clone(&tester));

    type_text(&mut h, "me@devbox");
    h.keys("enter");

    assert_eq!(ssh_result(&mut h).as_deref(), Some("Connected: devbox"));
    assert_eq!(tester.tested(), ["me@devbox"]);
    assert_eq!(opened(&h), [] as [Target; 0]);
    assert!(
        h.root(|root, _| root.connection_picker_adding_ssh()),
        "the form stays open"
    );
    assert!(
        !dir.path().join("native-ui.json").exists()
            || saved_ui(&dir)["saved_hosts"]
                .as_array()
                .is_none_or(Vec::is_empty),
        "a test saves nothing"
    );
}

#[gpui::test]
fn test_connection_failure_shows_ssh_stderr_verbatim(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let tester = FakeSsh::answering(Err(DENIED.to_owned()));
    open_ssh_form(&mut h, Arc::clone(&tester));

    type_text(&mut h, SSH_HOST);
    h.click_on("connections-ssh-form-test");

    assert_eq!(ssh_result(&mut h).as_deref(), Some(DENIED));
    assert_eq!(opened(&h), [] as [Target; 0]);
}

#[gpui::test]
fn cancel_stops_a_running_test_connection(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let tester = Arc::new(FakeSsh::default());
    open_ssh_form(&mut h, Arc::clone(&tester));
    type_text(&mut h, SSH_HOST);

    h.click_on("connections-ssh-form-test");
    assert_eq!(
        ssh_result(&mut h).as_deref(),
        Some("Testing the connection…")
    );
    assert!(!tester.cancelled());

    h.click_on("connections-ssh-form-test");

    assert!(tester.cancelled(), "Cancel tells the test to kill its ssh");
    assert_eq!(ssh_result(&mut h), None);
    assert!(h.root(|root, _| root.connection_picker_adding_ssh()));
}

#[gpui::test]
fn adding_an_ssh_host_without_a_test_saves_it_and_connects_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let tester = FakeSsh::answering(Ok(()));
    open_ssh_form(&mut h, Arc::clone(&tester));

    type_text(&mut h, SSH_HOST);
    h.keys("tab");
    assert_eq!(
        focus(&mut h).as_deref(),
        Some("connections-ssh-form-command")
    );
    type_text(&mut h, "rtd.cmd");
    h.click_on("connections-add-submit");

    assert_eq!(
        opened(&h),
        [Target::parse_ssh(SSH_HOST, Some("rtd.cmd")).expect("an SSH host")]
    );
    assert!(!is_open(&mut h), "Add closes the picker");
    assert_eq!(tester.tested(), [] as [String; 0], "Add runs no test");
    let saved = json!({ "kind": "ssh", "name": SSH_HOST, "remote_command": "rtd.cmd" });
    let ui = saved_ui(&dir);
    assert_eq!(ui["saved_hosts"], json!([saved.clone()]), "no port");
}

#[gpui::test]
fn forget_an_ssh_host_keeps_device_tokens_and_drops_its_count(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [{ "kind": "ssh", "name": SSH_HOST }],
            "host_session_counts": { "ssh:devbox": 2 },
        }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.tokens().set(SSH_HOST, "t1").expect("seed a token");
    open_picker(&mut h, tailnet_with(&[]));
    h.commands();

    h.click_on(&format!("connections-forget-ssh:{SSH_HOST}"));

    assert_eq!(rows(&mut h), owned_rows(&[("This PC", true)]));
    assert_eq!(held(&mut h), [HostKey::Local]);
    let ui = saved_ui(&dir);
    assert_eq!(ui["saved_hosts"], json!([]));
    assert!(
        ui["host_session_counts"]["ssh:devbox"].is_null(),
        "{}",
        ui["host_session_counts"]
    );
    assert_eq!(
        h.tokens().get(SSH_HOST).expect("read the token").as_deref(),
        Some("t1"),
        "an SSH host has no device token to delete"
    );
    assert!(h.commands().is_empty());
}

#[gpui::test]
fn forget_a_tailnet_host_deletes_its_device_token(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 8787 }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.tokens().set(HOST, "t1").expect("seed a token");
    open_picker(&mut h, tailnet_with(&[]));

    h.click_on(&format!("connections-forget-{HOST}"));

    assert_eq!(h.tokens().get(HOST).expect("read the token"), None);
}

#[gpui::test]
fn an_ssh_and_a_tailnet_host_of_one_name_are_listed_and_forgotten_alone(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [
            { "name": HOST, "port": 8787 },
            { "kind": "ssh", "name": HOST },
        ] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.tokens().set(HOST, "t1").expect("seed a token");
    open_picker(&mut h, tailnet_with(&[HOST]));
    assert_eq!(
        rows(&mut h),
        owned_rows(&[("This PC", true), (HOST, true), (HOST, true)])
    );
    assert_eq!(h.root(|root, _| root.connection_picker_ssh_rows()), [HOST]);

    h.click_on(&format!("connections-forget-ssh:{HOST}"));
    assert_eq!(
        saved_ui(&dir)["saved_hosts"],
        json!([{ "name": HOST, "port": 8787 }])
    );
    assert_eq!(
        h.tokens().get(HOST).expect("read the token").as_deref(),
        Some("t1"),
        "the tailnet host keeps its token"
    );
    assert_eq!(held(&mut h), [HostKey::Local, HostKey::of(&tailnet(HOST))]);

    h.click_on(&format!("connections-forget-{HOST}"));
    assert_eq!(saved_ui(&dir)["saved_hosts"], json!([]));
    assert_eq!(h.tokens().get(HOST).expect("read the token"), None);
}

#[gpui::test]
fn escape_leaves_the_ssh_form_then_closes_the_picker(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_ssh_form(&mut h, Arc::new(FakeSsh::default()));

    h.keys("escape");
    assert!(is_open(&mut h), "the first Esc leaves the form");
    assert!(!h.root(|root, _| root.connection_picker_adding_ssh()));

    h.keys("escape");
    assert!(!is_open(&mut h), "the next closes the picker");
}

#[gpui::test]
fn connections_draws_three_sections_in_order(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 9443 }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());

    open_picker(&mut h, tailnet_with(&[PEER]));

    let tops: Vec<_> = [
        "connections-section-this-pc".to_owned(),
        row("This PC"),
        "connections-section-saved".to_owned(),
        row(HOST),
        "connections-section-tailnet".to_owned(),
        row(PEER),
    ]
    .iter()
    .map(|selector| {
        let bounds = h.bounds(selector);
        assert!(bounds.size.width > px(0.0), "{selector} is drawn");
        bounds.origin.y
    })
    .collect();
    assert!(
        tops.windows(2).all(|pair| pair[0] < pair[1]),
        "This PC, Saved hosts and On your tailnet, each over its rows: {tops:?}"
    );
}

#[gpui::test]
fn a_saved_tailnet_host_without_a_token_shows_not_paired(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [
            { "name": HOST, "port": 8787 },
            { "name": PEER, "port": 8787 },
        ] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.tokens().set(PEER, "t1").expect("seed a token");

    open_picker(&mut h, tailnet_with(&[]));

    assert_eq!(
        detail(&mut h, HOST).and_then(|detail| detail.state),
        Some(RowState::NotPaired)
    );
    assert!(
        h.bounds(&format!("connections-not-paired-{HOST}"))
            .size
            .width
            > px(0.0)
    );
    assert_eq!(
        detail(&mut h, PEER).and_then(|detail| detail.state),
        None,
        "a host this PC holds a token for shows no chip before it answers"
    );
}

#[gpui::test]
fn this_pc_row_shows_connected(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::open(cx, &dir);

    open_picker(&mut h, tailnet_with(&[]));

    assert_eq!(
        detail(&mut h, "This PC").and_then(|detail| detail.state),
        Some(RowState::Connected)
    );
    assert!(h.bounds("connections-state-This PC").size.width > px(0.0));
}

#[gpui::test]
fn a_row_shows_its_detail_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [{ "name": HOST, "port": 9443, "enabled": false }],
            "host_session_counts": { HOST: 3 },
        }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());

    open_picker(&mut h, tailnet_with(&[PEER]));

    assert_eq!(
        detail(&mut h, HOST).map(|detail| detail.line),
        Some("Tailnet · port 9443 · 3 sessions last time".to_owned())
    );
    assert_eq!(
        detail(&mut h, PEER).map(|detail| detail.line),
        Some("Online · port 8787 · connecting saves it".to_owned())
    );
    assert!(h.bounds(&format!("connections-detail-{HOST}")).size.width > px(0.0));
}

#[gpui::test]
fn a_held_host_shows_its_live_session_count(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [{ "name": HOST, "port": 9443 }],
            "host_session_counts": { HOST: 3 },
        }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let mut host = h.host(&Target::tailnet(HOST, Some(9443)));
    host.connect();
    host.send(DaemonMessage::Sessions {
        sessions: vec![support::session("b1").build()],
    });

    open_picker(&mut h, tailnet_with(&[]));

    assert_eq!(
        detail(&mut h, HOST).map(|detail| detail.line),
        Some("Tailnet · port 9443 · 1 session".to_owned())
    );
}

#[gpui::test]
fn forget_is_clickable_without_hovering_first(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 9443 }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[]));
    h.commands();
    assert_eq!(
        focus(&mut h),
        Some(row("This PC")),
        "neither the host's row nor its Forget has the focus"
    );

    h.click_on(&format!("connections-forget-{HOST}"));

    assert_eq!(rows(&mut h), owned_rows(&[("This PC", true)]));
    assert!(h.commands().is_empty(), "the row was not picked");
}

fn forget_shown(h: &mut Harness<'_>, key: &str) -> bool {
    let key = key.to_owned();
    h.root(move |root, _| root.connection_picker_forget_shown(&key))
}

#[gpui::test]
fn opening_the_picker_on_a_saved_host_shows_its_state_chip(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [{ "name": HOST, "port": 8787 }],
        }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.host(&tailnet(HOST))
        .event(NetEvent::State(not_approved(tailnet(HOST), true)));

    h.root_update(|root, window, cx| {
        root.set_peer_source(Arc::new(tailnet_with(&[])));
        root.open_connection_picker(window, cx);
    });

    assert_eq!(
        focus(&mut h),
        Some(row("This PC")),
        "This PC's row has the focus"
    );
    assert_eq!(
        detail(&mut h, HOST).and_then(|detail| detail.state),
        Some(RowState::NeedsPairing)
    );
    assert!(h.bounds(&format!("connections-state-{HOST}")).size.width > px(0.0));
    assert!(
        !forget_shown(&mut h, HOST),
        "the row keeps its chip; Forget stays hidden"
    );
}

#[gpui::test]
fn tabbing_to_forget_reveals_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 9443, "enabled": false }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[]));

    assert_eq!(tab_walk(&mut h, 2), [row(HOST), switch(HOST)]);
    assert!(
        !forget_shown(&mut h, HOST),
        "the row's or its switch's focus keeps it hidden"
    );

    h.keys("tab");
    assert_eq!(focus(&mut h), Some(format!("connections-forget-{HOST}")));
    assert!(forget_shown(&mut h, HOST), "Forget's own focus shows it");
}

#[gpui::test]
fn add_host_opens_an_inline_form_on_tailnet(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[]));
    assert_eq!(add_mode(&mut h), None);

    h.click_on("connections-add-host");

    assert_eq!(add_mode(&mut h), Some(AddMode::Tailnet));
    assert_eq!(focus(&mut h).as_deref(), Some("connections-name"));
    let panel = h.bounds("connections-panel");
    for selector in [
        "connections-add-mode-tailnet",
        "connections-add-mode-ssh",
        "connections-name",
        "connections-port",
        "connections-add-cancel",
        "connections-add-submit",
    ] {
        let bounds = h.bounds(selector);
        assert!(
            bounds.size.width > px(0.0) && panel.contains(&bounds.center()),
            "{selector} shows in the panel"
        );
    }
}

#[gpui::test]
fn switching_to_ssh_keeps_the_tailnet_fields_and_cancels_a_running_test(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let tester = Arc::new(FakeSsh::default());
    h.root_update({
        let tester = Arc::clone(&tester);
        move |root, _, _| root.set_ssh_tester(tester)
    });
    open_picker(&mut h, tailnet_with(&[]));
    h.commands();
    h.click_on("connections-add-host");
    type_text(&mut h, HOST);

    h.click_on("connections-add-mode-ssh");
    assert_eq!(focus(&mut h).as_deref(), Some("connections-ssh-form-host"));
    type_text(&mut h, SSH_HOST);
    h.click_on("connections-ssh-form-test");
    assert_eq!(
        ssh_result(&mut h).as_deref(),
        Some("Testing the connection…")
    );

    h.click_on("connections-add-mode-tailnet");
    assert!(tester.cancelled(), "leaving SSH cancels the running test");
    assert_eq!(ssh_result(&mut h), None);

    h.click_on("connections-add-mode-ssh");
    h.click_on("connections-ssh-form-test");
    assert_eq!(
        tester.tested(),
        [SSH_HOST, SSH_HOST],
        "the SSH mode kept its host"
    );

    h.click_on("connections-add-mode-tailnet");
    assert_eq!(focus(&mut h).as_deref(), Some("connections-name"));
    h.keys("enter");
    assert_eq!(
        opened(&h),
        [Target::tailnet(HOST, Some(8787))],
        "the Tailnet mode kept its name"
    );
}

#[gpui::test]
fn cancel_closes_the_form_and_esc_then_closes_the_picker(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[]));
    h.click_on("connections-add-host");

    h.click_on("connections-add-cancel");
    assert_eq!(add_mode(&mut h), None, "Cancel closes the form");
    assert!(is_open(&mut h));
    assert_eq!(focus(&mut h).as_deref(), Some("connections-add-host"));

    h.keys("escape");
    assert!(!is_open(&mut h), "Esc then closes the picker");
}

#[gpui::test]
fn with_fifteen_hosts_and_the_form_open_the_panel_stays_inside_the_window(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let hosts: Vec<Value> = (0..15)
        .map(|n| json!({ "name": format!("host{n}.tail1234.ts.net"), "port": 8787 }))
        .collect();
    seed_ui(&dir, &json!({ "saved_hosts": hosts }));
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.cx.simulate_resize(gpui::size(px(1000.0), px(601.0)));
    open_picker(&mut h, tailnet_with(&[]));

    h.click_on("connections-add-host");

    let height = h.window_height();
    let panel = h.bounds("connections-panel");
    assert!(
        panel.top() >= px(24.0) && panel.bottom() <= px(height - 24.0),
        "the panel {panel:?} keeps 24 px clear of the {height} px window's edges"
    );
    for selector in ["connections-add-submit", "connections-close"] {
        let bounds = h.bounds(selector);
        assert!(
            bounds.size.width > px(0.0)
                && bounds.top() >= panel.top()
                && bounds.bottom() <= panel.bottom(),
            "{selector} {bounds:?} lies inside the panel {panel:?}"
        );
    }
    assert!(
        h.bounds("connections-list").size.height < px(15.0 * 46.0),
        "the list scrolls instead of growing"
    );
}

#[gpui::test]
fn tab_ring_order_with_the_form_closed(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 9443, "enabled": false }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[PEER]));
    assert_eq!(focus(&mut h), Some(row("This PC")));

    assert_eq!(
        tab_walk(&mut h, 7),
        [
            row(HOST),
            switch(HOST),
            format!("connections-forget-{HOST}"),
            row(PEER),
            "connections-add-host".to_owned(),
            "connections-close".to_owned(),
            row("This PC"),
        ]
    );
}

#[gpui::test]
fn tab_ring_walks_a_held_hosts_pair_and_reconnect_after_its_row(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 8787 }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.host(&tailnet(HOST))
        .event(NetEvent::State(not_approved(tailnet(HOST), false)));
    open_picker(&mut h, tailnet_with(&[]));

    assert_eq!(
        tab_walk(&mut h, 5),
        [
            row(HOST),
            switch(HOST),
            format!("connections-row-{HOST}-pair"),
            format!("connections-row-{HOST}-reconnect"),
            format!("connections-forget-{HOST}"),
        ]
    );
}

#[gpui::test]
fn tab_ring_order_with_the_form_open(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 9443, "enabled": false }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[]));
    h.click_on("connections-add-host");
    assert_eq!(focus(&mut h).as_deref(), Some("connections-name"));

    assert_eq!(
        tab_walk(&mut h, 11),
        [
            "connections-port".to_owned(),
            "connections-add-cancel".to_owned(),
            "connections-add-submit".to_owned(),
            "connections-close".to_owned(),
            row("This PC"),
            row(HOST),
            switch(HOST),
            format!("connections-forget-{HOST}"),
            "connections-add-mode-tailnet".to_owned(),
            "connections-add-mode-ssh".to_owned(),
            "connections-name".to_owned(),
        ]
    );

    h.click_on("connections-add-mode-ssh");
    assert_eq!(
        tab_walk(&mut h, 5),
        [
            "connections-ssh-form-command",
            "connections-add-cancel",
            "connections-ssh-form-test",
            "connections-add-submit",
            "connections-close",
        ]
    );
}
