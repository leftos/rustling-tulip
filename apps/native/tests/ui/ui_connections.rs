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
use protocol::ClientMessage;
use protocol::{DaemonMessage, WorktreeCleanupFailure};
use rustling_tulip_native::connection_picker::{
    AddMode, PeerSource, RowDetail, RowState, SshTestCancel, SshTester,
};
use rustling_tulip_native::target::{HostKey, SshHost, Target};
use rustling_tulip_native::{Connection, NetCommand, NetEvent, RootView};
use serde_json::{Value, json};
use support::{Fixture, Harness, PROTOCOL, TestDir, session, shell_spawns};
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
    conn.on_welcome(PROTOCOL, support::daemon_features(), None);
    conn
}

/// A connection to `target` still dialling.
fn dialling(target: Target) -> Connection {
    let mut conn = Connection::for_target(target);
    conn.on_connecting();
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
fn a_host_on_an_older_release_shows_its_version_and_behind(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 8787 }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.host(&tailnet(HOST)).connect_with_version("0.9.0");

    open_picker(&mut h, tailnet_with(&[]));

    let detail = detail(&mut h, HOST).expect("the saved host's row");
    assert_eq!(
        detail.line, "Tailnet · port 8787 · v0.9.0",
        "the row names the release its host runs"
    );
    assert_eq!(
        detail.state,
        Some(RowState::Connected),
        "the Behind tag sits beside the state chip, not in its place"
    );
    assert!(
        drawn(&mut h, &format!("connections-behind-{HOST}")),
        "a host older than this client wears the Behind tag"
    );
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

/// A release older than this client's.
const OLD_RELEASE: &str = "0.0.1";
/// What a host says when it will not update.
const REFUSAL: &str = "This host is already on the newest release.";

/// Connects `target` on release `version`, its daemon listing
/// `remote_update`, and drains what the client sent on `Welcome`.
fn connect_updatable(h: &mut Harness<'_>, target: &Target, version: &str) {
    let mut features = support::daemon_features();
    features.push(protocol::FEATURE_REMOTE_UPDATE.to_owned());
    let mut conn = Connection::for_target(target.clone());
    conn.on_connecting();
    conn.on_socket_open(8787);
    conn.on_welcome(PROTOCOL, features.clone(), None);
    conn.set_build_version(Some(version.to_owned()));
    let mut host = h.host(target);
    host.event(NetEvent::State(conn));
    host.send(DaemonMessage::Welcome {
        protocol_version: PROTOCOL,
        supported_versions: vec![PROTOCOL],
        features,
        daemon_id: None,
        origin: None,
        build_version: Some(version.to_owned()),
    });
    host.sent();
}

/// A window holding `HOST` on an older release that can update on request,
/// running a working and an idle session, with the picker open.
fn updatable_host<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    seed_ui(
        dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 8787 }] }),
    );
    let mut h = Harness::with(cx, dir, &Fixture::default());
    h.tokens().set(HOST, "t1").expect("seed a token");
    connect_updatable(&mut h, &tailnet(HOST), OLD_RELEASE);
    h.host(&tailnet(HOST)).send(DaemonMessage::Sessions {
        sessions: vec![
            session("b1").status("working").build(),
            session("b2").build(),
        ],
    });
    h.sent();
    open_picker(&mut h, tailnet_with(&[]));
    h
}

/// The `RemoteUpdate` requests among `sent`, by id.
fn remote_updates(sent: &[ClientMessage]) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::RemoteUpdate { request_id } => Some(request_id.clone()),
            _ => None,
        })
        .collect()
}

/// Every toast's title and detail, oldest first.
fn toasts(h: &mut Harness<'_>) -> Vec<(String, Option<String>)> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| (toast.title.clone(), toast.detail.clone()))
            .collect()
    })
}

/// Opens the dialog from `HOST`'s row and presses Update: the id the
/// request went with.
fn ask_update(h: &mut Harness<'_>) -> String {
    h.click_on(&format!("connections-row-{HOST}-update"));
    h.click_on("host-update-confirm");
    let ids = remote_updates(&h.host(&tailnet(HOST)).sent());
    ids.into_iter()
        .next()
        .expect("one RemoteUpdate went to the host")
}

#[gpui::test]
fn update_host_confirms_listing_the_hosts_sessions_then_sends_remote_update(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = updatable_host(cx, &dir);
    assert!(
        drawn(&mut h, &format!("connections-row-{HOST}-update")),
        "the row of a host on an older release offers Update host"
    );

    h.click_on(&format!("connections-row-{HOST}-update"));

    assert!(!is_open(&mut h), "the picker gives way to the dialog");
    let dialog = h
        .root(|root, _| root.host_update_dialog())
        .expect("the confirm dialog is open");
    assert_eq!(
        dialog.title,
        format!("Update {HOST} to the newest release?")
    );
    assert_eq!(
        dialog.body,
        format!(
            "{HOST} will download and check the newest official release itself, then restart its \
             daemon. Its sessions keep running."
        )
    );
    assert_eq!(
        dialog
            .sessions
            .iter()
            .map(|line| (line.title.as_str(), line.status, line.working))
            .collect::<Vec<_>>(),
        [("b1", "working", true), ("b2", "idle", false)],
        "each session with its status, the working one marked"
    );
    assert_eq!(dialog.no_sessions, None);
    assert_eq!(dialog.warnings, Vec::<String>::new());
    assert_eq!(dialog.confirm, format!("Update {HOST}"));
    assert!(drawn(&mut h, "host-update-session-0"));
    h.keys("enter");
    assert!(
        h.root(|root, _| root.host_update_dialog()).is_some(),
        "Enter does not update"
    );
    assert_eq!(
        remote_updates(&h.host(&tailnet(HOST)).sent()),
        Vec::<String>::new()
    );
    h.sent();

    h.click_on("host-update-confirm");

    assert!(h.root(|root, _| root.host_update_dialog()).is_none());
    assert_eq!(
        remote_updates(&h.host(&tailnet(HOST)).sent()).len(),
        1,
        "exactly one request goes to the host"
    );
    assert!(
        remote_updates(&h.sent()).is_empty(),
        "This PC is asked nothing"
    );
    assert!(
        toasts(&mut h).contains(&(format!("Asking {HOST} to update…"), None)),
        "{:?}",
        toasts(&mut h)
    );
    h.root_update(RootView::open_connection_picker);
    assert!(is_open(&mut h), "the picker opens again");
    // The row's controls, as Tab walks them: the drawn ones.
    let ring = tab_walk(&mut h, 5);
    assert!(
        !ring.contains(&format!("connections-row-{HOST}-update")),
        "a host with an update pending is not offered another: {ring:?}"
    );
}

#[gpui::test]
fn cancel_in_the_update_dialog_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = updatable_host(cx, &dir);
    h.click_on(&format!("connections-row-{HOST}-update"));
    h.click_on("host-update-cancel");
    assert!(h.root(|root, _| root.host_update_dialog()).is_none());

    h.click_on("footer-connections");
    h.click_on(&format!("connections-row-{HOST}-update"));
    h.keys("escape");
    assert!(
        h.root(|root, _| root.host_update_dialog()).is_none(),
        "Esc cancels"
    );

    assert_eq!(
        remote_updates(&h.host(&tailnet(HOST)).sent()),
        Vec::<String>::new()
    );
    assert!(
        !toasts(&mut h)
            .iter()
            .any(|(title, _)| title.starts_with("Asking")),
        "no request, no toast"
    );
}

#[gpui::test]
fn a_refused_remote_update_shows_the_hosts_message_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = updatable_host(cx, &dir);
    let id = ask_update(&mut h);

    h.host(&tailnet(HOST)).send(DaemonMessage::Error {
        message: REFUSAL.to_owned(),
        request_id: Some(id),
        code: Some("update_refused".to_owned()),
    });

    let shown = toasts(&mut h);
    assert_eq!(
        shown
            .iter()
            .filter(|(title, detail)| title.contains(REFUSAL) || detail.as_deref() == Some(REFUSAL))
            .collect::<Vec<_>>(),
        [&(format!("{HOST} didn't update"), Some(REFUSAL.to_owned()))],
        "one toast carries the host's text: {shown:?}"
    );
}

#[gpui::test]
fn a_host_back_on_the_new_version_shows_updated(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = updatable_host(cx, &dir);
    let id = ask_update(&mut h);
    let version = protocol::BUILD_VERSION;
    h.host(&tailnet(HOST))
        .send(DaemonMessage::RemoteUpdateStarted {
            request_id: id,
            version: version.to_owned(),
        });
    assert!(
        toasts(&mut h).contains(&(format!("{HOST} is updating to {version}…"), None)),
        "{:?}",
        toasts(&mut h)
    );

    h.host(&tailnet(HOST))
        .event(NetEvent::State(closed(tailnet(HOST))));
    connect_updatable(&mut h, &tailnet(HOST), version);

    let shown = toasts(&mut h);
    assert!(
        shown.contains(&(format!("{HOST} updated to {version}"), None)),
        "{shown:?}"
    );
    assert!(
        !shown.iter().any(|(title, _)| title.contains("is updating")),
        "the progress toast became the outcome: {shown:?}"
    );
}

#[gpui::test]
fn a_host_that_never_returns_says_so_after_the_bound(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = updatable_host(cx, &dir);
    let id = ask_update(&mut h);
    h.host(&tailnet(HOST))
        .send(DaemonMessage::RemoteUpdateStarted {
            request_id: id,
            version: protocol::BUILD_VERSION.to_owned(),
        });
    h.host(&tailnet(HOST))
        .event(NetEvent::State(closed(tailnet(HOST))));
    let gone = format!("{HOST} hasn't come back; it may still be updating.");

    h.advance(Duration::from_secs(110));
    assert!(!toasts(&mut h).iter().any(|(title, _)| *title == gone));

    h.advance(Duration::from_secs(11));
    assert!(
        toasts(&mut h).contains(&(gone, None)),
        "{:?}",
        toasts(&mut h)
    );
}

#[gpui::test]
fn the_update_action_is_hidden_for_this_pc_ssh_and_an_up_to_date_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [
            { "name": HOST, "port": 8787 },
            { "kind": "ssh", "name": "me@devbox" },
        ] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.tokens().set(HOST, "t1").expect("seed a token");
    let ssh_box = Target::parse_ssh("me@devbox", None).expect("an SSH host string");
    connect_updatable(&mut h, &tailnet(HOST), protocol::BUILD_VERSION);
    connect_updatable(&mut h, &ssh_box, OLD_RELEASE);

    open_picker(&mut h, tailnet_with(&[]));

    assert!(!drawn(&mut h, "connections-row-This PC-update"), "This PC");
    assert!(
        !drawn(&mut h, "connections-row-ssh:me@devbox-update"),
        "an SSH host, even behind"
    );
    assert!(
        detail(&mut h, "ssh:me@devbox").is_some_and(|detail| detail.behind),
        "the SSH host is behind"
    );
    assert!(
        !drawn(&mut h, &format!("connections-row-{HOST}-update")),
        "a host on this client's release"
    );
}

#[gpui::test]
fn update_connected_clients_is_offered_on_this_pc_when_supported(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.reconnect_with_features(&[protocol::FEATURE_CLIENT_UPDATE]);

    open_picker(&mut h, tailnet_with(&[]));

    assert!(
        drawn(&mut h, "connections-row-This PC-update-clients"),
        "This PC's daemon can tell its clients to update"
    );
}

#[gpui::test]
fn pressing_it_sends_update_connected_clients_and_shows_the_counts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.reconnect_with_features(&[protocol::FEATURE_CLIENT_UPDATE]);
    open_picker(&mut h, tailnet_with(&[]));
    h.sent();

    h.click_on("connections-row-This PC-update-clients");

    assert!(!is_open(&mut h), "the dialog closes");
    let sent = h.sent();
    let ids: Vec<String> = sent
        .iter()
        .filter_map(|msg| match msg {
            ClientMessage::UpdateConnectedClients { request_id } => request_id.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 1, "one request, for This PC: {sent:?}");
    assert!(!ids[0].is_empty(), "a fresh request id");

    h.send(DaemonMessage::ConnectedClientsNotified {
        request_id: Some(ids[0].clone()),
        native: 2,
        phones: 1,
        too_old: 1,
    });
    assert!(
        toasts(&mut h).contains(&(
            "Told 2 clients to update; 1 phones reload; 1 too old to update this way".to_owned(),
            None
        )),
        "{:?}",
        toasts(&mut h)
    );

    h.send(DaemonMessage::ConnectedClientsNotified {
        request_id: Some(ids[0].clone()),
        native: 1,
        phones: 0,
        too_old: 0,
    });
    assert!(
        toasts(&mut h).contains(&("Told 1 client to update".to_owned(), None)),
        "{:?}",
        toasts(&mut h)
    );

    h.send(DaemonMessage::ConnectedClientsNotified {
        request_id: Some(ids[0].clone()),
        native: 0,
        phones: 0,
        too_old: 0,
    });
    assert!(
        toasts(&mut h).contains(&("No other clients are connected.".to_owned(), None)),
        "{:?}",
        toasts(&mut h)
    );
}

#[gpui::test]
fn it_is_not_offered_without_the_feature(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());

    open_picker(&mut h, tailnet_with(&[]));

    assert!(
        !drawn(&mut h, "connections-row-This PC-update-clients"),
        "a daemon that cannot tell its clients offers no such action"
    );
}

#[gpui::test]
fn open_shell_on_a_connections_row_spawns_there(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 8787 }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.tokens().set(HOST, "t1").expect("seed a token");
    h.host(&tailnet(HOST)).connect();
    open_picker(&mut h, tailnet_with(&[]));
    h.sent();
    h.host(&tailnet(HOST)).sent();

    h.click_on(&format!("connections-row-{HOST}-shell"));

    assert!(!is_open(&mut h), "the picker closes");
    assert_eq!(
        shell_spawns(&h.host(&tailnet(HOST)).sent()),
        [None],
        "the host runs one plain shell, with no folder"
    );
    assert_eq!(
        shell_spawns(&h.sent()),
        Vec::<Option<String>>::new(),
        "This PC is sent none"
    );
}

#[gpui::test]
fn an_offline_row_offers_no_open_shell(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 8787 }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.tokens().set(HOST, "t1").expect("seed a token");
    h.host(&tailnet(HOST))
        .event(NetEvent::State(dialling(tailnet(HOST))));
    open_picker(&mut h, tailnet_with(&[]));
    assert!(
        !drawn(&mut h, &format!("connections-row-{HOST}-shell")),
        "a connecting host offers no shell"
    );

    h.host(&tailnet(HOST))
        .event(NetEvent::State(closed(tailnet(HOST))));
    assert!(
        !drawn(&mut h, &format!("connections-row-{HOST}-shell")),
        "a dropped host offers no shell"
    );

    h.host(&tailnet(HOST))
        .event(NetEvent::State(not_approved(tailnet(HOST), true)));
    assert!(
        !drawn(&mut h, &format!("connections-row-{HOST}-shell")),
        "a not-approved host offers no shell"
    );
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
fn a_long_tailscale_error_wraps_inside_the_panel(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 9443 }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let message = "[fake-tailscale] RUSTLING_TULIP_FAKE_TAILSCALE_STATE points at a file \
                   that does not exist, so the fake CLI has no tailnet state to report back";

    open_picker(
        &mut h,
        FakePeers(Err(TailnetError::Failed(message.to_owned()))),
    );

    let panel = h.bounds("connections-panel");
    let scan = h.bounds("connections-scan");
    let one_line = h.bounds("connections-detail-This PC").size.height;
    assert!(
        scan.right() <= panel.right(),
        "the error {scan:?} ends inside the panel {panel:?}"
    );
    assert!(
        scan.size.height > one_line,
        "the error {scan:?} wraps past one {one_line:?} line"
    );
}

#[gpui::test]
fn a_failed_scan_shows_its_error_under_the_title(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{ "name": HOST, "port": 9443 }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    let message = "Tailscale could not report this PC's tailnet: the local tailscaled \
                   service did not answer. Start Tailscale and sign in, then open Connections again.";

    open_picker(
        &mut h,
        FakePeers(Err(TailnetError::Failed(message.to_owned()))),
    );

    let panel = h.bounds("connections-panel");
    let title = h.bounds("connections-section-tailnet-title");
    let scan = h.bounds("connections-scan");
    assert!(
        scan.top() >= title.bottom(),
        "the error {scan:?} starts under the title {title:?}"
    );
    assert_eq!(
        scan.left(),
        title.left(),
        "the error {scan:?} starts at the section's left, as the title {title:?} does"
    );
    assert!(
        scan.left() >= panel.left() && scan.right() <= panel.right(),
        "the error {scan:?} stays inside the panel {panel:?}"
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
fn a_saved_rows_detail_line_ends_before_its_on_switch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [{ "kind": "ssh", "name": SSH_HOST, "enabled": false }],
            "host_session_counts": { "ssh:devbox": 2 },
        }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());

    open_picker(&mut h, tailnet_with(&[]));

    let key = format!("ssh:{SSH_HOST}");
    let line = h.bounds(&format!("connections-detail-{key}"));
    let on = h.bounds(&switch(&key));
    let one_line = h.bounds("connections-detail-This PC").size.height;
    assert!(
        detail(&mut h, &key).is_some_and(|detail| detail.line.ends_with("2 sessions last time")),
        "the row's line names its sessions"
    );
    assert!(
        line.right() <= on.left(),
        "the detail line {line:?} ends before the switch {on:?}"
    );
    assert!(
        line.size.height > one_line,
        "the detail line {line:?} wraps past one {one_line:?} line rather than being cut"
    );
}

#[gpui::test]
fn a_squeezed_detail_wraps_between_whole_pieces(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [{ "name": HOST, "port": 8787, "enabled": false }],
            "host_session_counts": { HOST: 4 },
        }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[]));

    h.root_update(|root, _, cx| root.set_ui_text_size(4, cx));
    h.cx.run_until_parked();

    assert_eq!(
        detail(&mut h, HOST).map(|detail| detail.line),
        Some("Tailnet · port 8787 · 4 sessions last time".to_owned())
    );
    let line = h.bounds(&format!("connections-detail-{HOST}"));
    let on = h.bounds(&switch(HOST));
    let one_line = h.bounds("connections-detail-This PC").size.height;
    let first = h.bounds(&format!("connections-piece-0-{HOST}"));
    let last = h.bounds(&format!("connections-piece-2-{HOST}"));
    assert!(
        line.size.height > one_line && last.top() >= first.bottom(),
        "the detail line {line:?} wraps past one {one_line:?} line, its last piece \
         {last:?} under its first {first:?}"
    );
    for n in 0..3 {
        let piece = h.bounds(&format!("connections-piece-{n}-{HOST}"));
        assert!(piece.size.width > px(0.0), "piece {n} is drawn");
        assert!(
            // Half a line of room for subpixel rounding: a second line
            // would double the height.
            piece.size.height < one_line * 1.5,
            "piece {n} {piece:?} keeps its text on one {one_line:?} line"
        );
        assert!(
            piece.right() <= on.left(),
            "piece {n} {piece:?} ends before the switch {on:?}"
        );
    }
    assert!(
        line.right() <= on.left(),
        "the detail line {line:?} ends before the switch {on:?}"
    );
}

#[gpui::test]
fn a_detail_piece_wider_than_the_column_wraps_inside_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let command = "wsl -d Ubuntu -- /home/me/.cargo/bin/rustling-tulipd bridge";
    seed_ui(
        &dir,
        &json!({ "saved_hosts": [{
            "kind": "ssh",
            "name": SSH_HOST,
            "remote_command": command,
            "enabled": false,
        }] }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());

    open_picker(&mut h, tailnet_with(&[]));

    let key = format!("ssh:{SSH_HOST}");
    assert!(
        detail(&mut h, &key).is_some_and(|detail| detail.line.contains(command)),
        "the row's line names its remote command"
    );
    let on = h.bounds(&switch(&key));
    let mut pieces = 0;
    loop {
        let piece = h.bounds(&format!("connections-piece-{pieces}-{key}"));
        if piece.size.width <= px(0.0) {
            break;
        }
        assert!(
            piece.right() <= on.left(),
            "piece {pieces} {piece:?} ends before the switch {on:?}"
        );
        pieces += 1;
    }
    assert!(pieces >= 2, "the line draws its pieces, {pieces} found");
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

    assert_eq!(
        tab_walk(&mut h, 3),
        [
            "connections-row-This PC-shell".to_owned(),
            row(HOST),
            switch(HOST),
        ]
    );
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

/// The `connections-ssh` shot's own tree in a 984x601 window, with its SSH
/// form at its tallest after a passed Test connection: every heading and
/// row the list draws keeps the height it has with the form closed, and no
/// two consecutive ones overlap.
#[gpui::test]
fn the_tallest_ssh_form_keeps_rows_and_headings_apart(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [
                { "name": "box.tail1234.ts.net", "port": 8787 },
                { "kind": "ssh", "name": "me@buildbox", "enabled": false },
            ],
            "host_session_counts": { "box.tail1234.ts.net": 4 },
        }),
    );
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    h.cx.simulate_resize(gpui::size(px(984.0), px(601.0)));
    h.root_update(move |root, _, _| root.set_ssh_tester(FakeSsh::answering(Ok(()))));
    open_picker(&mut h, tailnet_with(&["laptop.tail1234.ts.net"]));
    h.commands();

    let headings: Vec<String> = ["this-pc", "saved", "tailnet"]
        .iter()
        .map(|slug| format!("connections-section-{slug}"))
        .collect();
    let ssh = h.root(|root, _| root.connection_picker_ssh_rows());
    let rows: Vec<String> = h
        .root(|root, _| root.connection_picker_rows())
        .into_iter()
        .map(|(label, _)| {
            let key = if ssh.contains(&label) {
                format!("ssh:{label}")
            } else {
                label
            };
            row(&key)
        })
        .collect();
    let mut selectors = headings.clone();
    selectors.extend(rows.iter().cloned());
    let height = |h: &mut Harness<'_>, selector: &str| h.bounds(selector).size.height / px(1.0);
    let closed: Vec<f32> = selectors.iter().map(|s| height(&mut h, s)).collect();

    h.click_on("connections-add-host");
    h.click_on("connections-add-mode-ssh");
    assert!(h.root(|root, _| root.connection_picker_adding_ssh()));
    assert_eq!(focus(&mut h).as_deref(), Some("connections-ssh-form-host"));
    type_text(&mut h, "me@devbox");
    h.keys("enter");
    assert_eq!(ssh_result(&mut h).as_deref(), Some("Connected: devbox"));

    for (selector, closed_height) in selectors.iter().zip(&closed) {
        let open = height(&mut h, selector);
        assert!(
            (open - closed_height).abs() <= 0.5,
            "{selector} keeps the {closed_height} px height it has with the form closed, not {open} px"
        );
    }
    for selector in &rows {
        let open = height(&mut h, selector);
        assert!(
            open >= 46.0,
            "the row at {selector} keeps its 46 px floor, not {open} px"
        );
    }
    let mut drawn: Vec<(String, f32, f32)> = selectors
        .iter()
        .map(|selector| {
            let bounds = h.bounds(selector);
            (
                selector.clone(),
                bounds.top() / px(1.0),
                bounds.bottom() / px(1.0),
            )
        })
        .collect();
    drawn.sort_by(|a, b| a.1.total_cmp(&b.1));
    for pair in drawn.windows(2) {
        assert!(
            pair[0].2 <= pair[1].1,
            "{} ends at {} px, above where {} starts at {} px",
            pair[0].0,
            pair[0].2,
            pair[1].0,
            pair[1].1
        );
    }
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
        tab_walk(&mut h, 8),
        [
            "connections-row-This PC-shell".to_owned(),
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
        tab_walk(&mut h, 6),
        [
            "connections-row-This PC-shell".to_owned(),
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
        tab_walk(&mut h, 12),
        [
            "connections-port".to_owned(),
            "connections-add-cancel".to_owned(),
            "connections-add-submit".to_owned(),
            "connections-close".to_owned(),
            row("This PC"),
            "connections-row-This PC-shell".to_owned(),
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
