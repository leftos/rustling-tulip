//! Connections picker specs: the footer's Connections button (and the
//! first-connect overlay's, which covers the footer) opens a modal listing
//! This PC, the saved hosts and the owner's online tailnet peers. Picking a
//! row or connecting a typed host sends the switch and remembers the host in
//! `native-ui.json`; Forget drops a saved host, and forgetting the current
//! one switches to this PC. A refused name or a failed scan shows its text.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use std::sync::Arc;

use gpui::{TestAppContext, px};
use rustling_tulip_native::connection_picker::PeerSource;
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{NetCommand, RootView};
use serde_json::{Value, json};
use support::{Fixture, Harness, TestDir};
use tailnet::{PeerStatus, Status, TailnetError};

const HOST: &str = "box.tail1234.ts.net";
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

/// The targets of the `SwitchTarget` commands sent since the last look.
fn switches(h: &mut Harness<'_>) -> Vec<Target> {
    h.commands()
        .into_iter()
        .filter_map(|command| match command {
            NetCommand::SwitchTarget(target) => Some(target),
            _ => None,
        })
        .collect()
}

fn row(label: &str) -> String {
    format!("connections-row-{label}")
}

fn owned_rows(rows: &[(&str, bool)]) -> Vec<(String, bool)> {
    rows.iter()
        .map(|(label, current)| ((*label).to_owned(), *current))
        .collect()
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
        owned_rows(&[("This PC", true), (HOST, false), (PEER, false)]),
        "this PC, the saved host once, then the peer not saved"
    );
    assert_eq!(
        h.root(|root, _| root.connection_picker_scan_text()),
        None,
        "the scan answered"
    );
}

#[gpui::test]
fn selecting_a_host_sends_switch_target(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[PEER]));
    h.commands();

    h.click_on(&row(PEER));

    assert_eq!(switches(&mut h), [Target::tailnet(PEER, Some(8787))]);
    assert!(!is_open(&mut h), "picking a host closes the picker");
}

#[gpui::test]
fn typed_name_with_a_slash_shows_the_message_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[]));
    h.commands();

    h.keys("tab");
    assert_eq!(focus(&mut h).as_deref(), Some("connections-name"));
    type_text(&mut h, "box.tail1234.ts.net/ws");
    h.keys("enter");

    assert_eq!(
        h.root(|root, _| root.connection_picker_error()).as_deref(),
        Some(SLASH_REFUSAL)
    );
    assert_eq!(switches(&mut h), [] as [Target; 0]);
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
    assert_eq!(switches(&mut h), [] as [Target; 0]);
}

#[gpui::test]
fn forget_the_current_host_switches_to_local(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [{ "name": HOST, "port": 8787 }],
            "last_target": { "name": HOST, "port": 8787 },
        }),
    );
    let mut h = Harness::on_target(
        cx,
        &dir,
        Target::tailnet(HOST, Some(8787)),
        &Fixture::default(),
    );
    open_picker(&mut h, tailnet_with(&[]));
    assert_eq!(
        rows(&mut h),
        owned_rows(&[("This PC", false), (HOST, true)])
    );
    h.commands();

    h.click_on(&format!("connections-forget-{HOST}"));

    assert_eq!(switches(&mut h), [Target::Local]);
    let ui = saved_ui(&dir);
    assert_eq!(ui["saved_hosts"], json!([]));
    assert!(ui["last_target"].is_null(), "{}", ui["last_target"]);
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
    h.commands();

    h.click_on("overlay-connections");
    assert!(
        is_open(&mut h),
        "the overlay's Connections opens the picker"
    );
    h.click_on(&row(HOST));

    assert_eq!(
        switches(&mut h),
        [Target::tailnet(HOST, Some(9443))],
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
    assert_eq!(switches(&mut h), [] as [Target; 0]);
}

#[gpui::test]
fn switching_host_saves_last_target(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[PEER]));

    h.click_on(&row(PEER));

    let ui = saved_ui(&dir);
    assert_eq!(ui["last_target"], json!({ "name": PEER, "port": 8787 }));
    assert_eq!(
        ui["saved_hosts"],
        json!([{ "name": PEER, "port": 8787 }]),
        "a picked peer is remembered with the port it was dialled on"
    );
}

#[gpui::test]
fn typed_host_with_a_custom_port_dials_that_port(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[]));
    h.commands();

    h.keys("tab");
    type_text(&mut h, HOST);
    h.keys("tab");
    assert_eq!(focus(&mut h).as_deref(), Some("connections-port"));
    type_text(&mut h, "9443");
    h.keys("enter");

    assert_eq!(switches(&mut h), [Target::tailnet(HOST, Some(9443))]);
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
        owned_rows(&[("This PC", true), (HOST, false)]),
        "this PC and the saved hosts are still offered"
    );
}

#[gpui::test]
fn picking_this_pc_switches_to_local(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_ui(
        &dir,
        &json!({
            "saved_hosts": [{ "name": HOST, "port": 8787 }],
            "last_target": { "name": HOST, "port": 8787 },
        }),
    );
    let mut h = Harness::on_target(
        cx,
        &dir,
        Target::tailnet(HOST, Some(8787)),
        &Fixture::default(),
    );
    open_picker(&mut h, tailnet_with(&[]));
    h.commands();

    h.click_on(&row("This PC"));

    assert_eq!(switches(&mut h), [Target::Local]);
    let ui = saved_ui(&dir);
    assert!(ui["last_target"].is_null(), "{}", ui["last_target"]);
    assert_eq!(
        ui["saved_hosts"],
        json!([{ "name": HOST, "port": 8787 }]),
        "the host stays saved"
    );
}

#[gpui::test]
fn picking_the_current_target_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &Fixture::default());
    open_picker(&mut h, tailnet_with(&[PEER]));
    h.commands();

    h.click_on(&row("This PC"));

    assert!(!is_open(&mut h), "the picker closes");
    assert_eq!(switches(&mut h), [] as [Target; 0]);
}
