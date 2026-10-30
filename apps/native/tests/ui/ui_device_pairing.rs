//! Device pairing specs: this PC pairing itself with a tailnet host that
//! answers "not approved". The dialog opens from the Pair button on the
//! host's Connections row, never by itself at launch; its buttons and keys
//! send the pairing commands to that host's network thread; the host picker
//! tags a saved host this PC holds no token for, and Forget deletes the
//! host's token.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use daemon_client::device_tokens::DeviceTokenStore as _;
use gpui::TestAppContext;
use protocol::{DEVICE_NOT_APPROVED, DaemonMessage, DevicePairOutcome};
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{Connection, NetCommand, NetEvent, RootView};
use serde_json::json;
use support::{Harness, TestDir};

const HOST: &str = "box.tail1234.ts.net";
const OTHER: &str = "den.tail1234.ts.net";

fn host() -> Target {
    Target::tailnet(HOST, None)
}

/// A connection to [`HOST`] still dialling.
fn connecting() -> Connection {
    let mut conn = Connection::for_target(host());
    conn.on_connecting();
    conn
}

/// A pairing connection to [`HOST`]: its daemon does not know this PC.
fn not_approved(had_token: bool) -> Connection {
    let mut conn = connecting();
    conn.on_socket_open(8787);
    conn.on_not_approved(DEVICE_NOT_APPROVED.to_owned(), had_token);
    conn
}

/// Saves `hosts` as the picker's hosts, each switched on.
fn seed_saved_hosts(dir: &TestDir, hosts: &[serde_json::Value]) {
    std::fs::write(
        dir.path().join("native-ui.json"),
        json!({ "saved_hosts": hosts }).to_string(),
    )
    .expect("seed native-ui.json");
}

fn tailnet_entry(name: &str) -> serde_json::Value {
    json!({ "name": name, "port": 8787 })
}

/// Whether the last frame drew what `selector` tags.
fn drawn(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).size.width > gpui::px(0.0)
}

fn open_picker(h: &mut Harness<'_>) {
    h.root_update(RootView::open_connection_picker);
}

/// Reports `conn` on [`HOST`]'s connection.
fn host_state(h: &mut Harness<'_>, conn: Connection) {
    h.host(&host()).event(NetEvent::State(conn));
}

/// The commands sent to [`HOST`]'s network thread since the last look.
fn host_commands(h: &mut Harness<'_>) -> Vec<NetCommand> {
    h.host(&host()).commands()
}

/// The window connected to the saved [`HOST`] at launch, which answers "not
/// approved", the owner pressing its row's Pair: the dialog is open in its
/// Intro phase.
fn picked<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    seed_saved_hosts(dir, &[tailnet_entry(HOST)]);
    let mut h = Harness::open(cx, dir);
    host_state(&mut h, connecting());
    host_state(&mut h, not_approved(false));
    open_picker(&mut h);
    h.click_on(&format!("connections-row-{HOST}-pair"));
    assert!(h.root(|root, _| root.device_pairing_open()));
    h
}

fn line(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.device_pairing_line())
}

fn buttons(h: &mut Harness<'_>) -> Vec<&'static str> {
    h.root(|root, _| root.device_pairing_buttons())
}

/// The dialog past Ask to pair, the host's request pending.
fn waiting<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = picked(cx, dir);
    h.click_on("device-pairing-ask");
    h.host(&host()).send(DaemonMessage::DevicePairPending {
        request_id: "r1".to_owned(),
        expires_in_secs: 300,
    });
    h
}

/// The dialog after the owner approved on the host: the code field shows.
fn code_entry<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = waiting(cx, dir);
    h.host(&host()).send(DaemonMessage::DevicePairDecided {
        request_id: "r1".to_owned(),
        outcome: DevicePairOutcome::Approved,
    });
    host_commands(&mut h);
    h
}

#[gpui::test]
fn pair_on_an_unpaired_row_opens_the_dialog_for_that_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_saved_hosts(&dir, &[tailnet_entry(HOST), tailnet_entry(OTHER)]);
    let mut h = Harness::open(cx, &dir);
    h.tokens()
        .set(OTHER, "device-token")
        .expect("store the token");
    h.host(&Target::tailnet(OTHER, None)).connect();
    host_state(&mut h, connecting());
    open_picker(&mut h);
    assert!(
        !drawn(&mut h, &format!("connections-row-{OTHER}-pair")),
        "a paired, connected host offers no Pair"
    );
    h.commands();

    h.click_on(&format!("connections-row-{HOST}-pair"));
    assert!(!h.root(|root, _| root.connection_picker_open()));
    assert!(
        !h.root(|root, _| root.device_pairing_open()),
        "nothing opens while the host is still being dialled"
    );
    host_state(&mut h, not_approved(false));

    assert!(h.root(|root, _| root.device_pairing_open()));
    assert_eq!(
        h.root(|root, _| root.device_pairing_title()).as_deref(),
        Some("Pair with box.tail1234.ts.net")
    );
    assert_eq!(
        line(&mut h).as_deref(),
        Some("box.tail1234.ts.net does not know this PC.")
    );
    assert_eq!(buttons(&mut h), ["Ask to pair", "Close"]);
    assert!(host_commands(&mut h).is_empty(), "opening sends nothing");
    assert!(h.commands().is_empty(), "nor asks This PC for anything");
}

#[gpui::test]
fn pair_on_a_not_approved_row_opens_the_dialog_at_once(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_saved_hosts(&dir, &[tailnet_entry(HOST)]);
    let mut h = Harness::open(cx, &dir);
    h.tokens()
        .set(HOST, "device-token")
        .expect("store the token");
    let mut refused = connecting();
    refused.on_socket_open(8787);
    refused.on_not_approved(DEVICE_NOT_APPROVED.to_owned(), true);
    host_state(&mut h, refused);
    open_picker(&mut h);

    h.click_on(&format!("connections-row-{HOST}-pair"));

    assert!(h.root(|root, _| root.device_pairing_open()));
    assert_eq!(
        line(&mut h).as_deref(),
        Some("box.tail1234.ts.net no longer accepts this PC's pairing.")
    );
}

#[gpui::test]
fn an_ssh_row_has_no_pair_button(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_saved_hosts(&dir, &[json!({ "kind": "ssh", "name": "devbox" })]);
    let mut h = Harness::open(cx, &dir);

    open_picker(&mut h);

    assert!(drawn(&mut h, "connections-row-ssh:devbox"));
    assert!(!drawn(&mut h, "connections-row-ssh:devbox-pair"));
    assert!(
        !drawn(&mut h, "connections-row-This PC-pair"),
        "nor does This PC's"
    );
}

#[gpui::test]
fn ask_to_pair_sends_pair_request(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = picked(cx, &dir);

    h.click_on("device-pairing-ask");
    assert!(
        host_commands(&mut h)
            .iter()
            .any(|command| matches!(command, NetCommand::PairRequest)),
        "Ask to pair asks the host's network thread"
    );
    assert!(h.commands().is_empty(), "This PC is asked for nothing");
    assert_eq!(line(&mut h).as_deref(), Some("Asking box.tail1234.ts.net…"));
    assert_eq!(buttons(&mut h), ["Cancel"]);

    h.host(&host()).send(DaemonMessage::DevicePairPending {
        request_id: "r1".to_owned(),
        expires_in_secs: 300,
    });
    assert_eq!(
        line(&mut h).as_deref(),
        Some("Approve this PC on box.tail1234.ts.net.")
    );
    assert_eq!(
        h.root(|root, _| root.device_pairing_countdown()).as_deref(),
        Some("expires in 5:00")
    );
    h.advance(std::time::Duration::from_secs(61));
    assert_eq!(
        h.root(|root, _| root.device_pairing_countdown()).as_deref(),
        Some("expires in 3:59")
    );
}

#[gpui::test]
fn enter_submits_the_code(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = code_entry(cx, &dir);
    assert_eq!(
        line(&mut h).as_deref(),
        Some("Type the code box.tail1234.ts.net shows.")
    );
    assert_eq!(buttons(&mut h), ["Pair", "Cancel"]);

    h.keys("1 2 x 3 4 5 6 7");
    assert_eq!(
        h.root(RootView::device_pairing_code).as_deref(),
        Some("123456"),
        "only digits, at most six, stay in the field"
    );
    h.keys("enter");
    let redeemed: Vec<String> = host_commands(&mut h)
        .into_iter()
        .filter_map(|command| match command {
            NetCommand::PairRedeem { code } => Some(code),
            _ => None,
        })
        .collect();
    assert_eq!(redeemed, ["123456"]);
    assert_eq!(
        h.root(RootView::device_pairing_code).as_deref(),
        Some(""),
        "the field clears once the code is sent"
    );
    assert!(h.root(|root, _| root.device_pairing_open()));
}

#[gpui::test]
fn esc_while_waiting_sends_pair_cancel_and_closes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = waiting(cx, &dir);
    host_commands(&mut h);

    h.keys("escape");
    let cancels = host_commands(&mut h)
        .iter()
        .filter(|command| matches!(command, NetCommand::PairCancel))
        .count();
    assert_eq!(cancels, 1, "Esc withdraws the request once");
    assert!(!h.root(|root, _| root.device_pairing_open()));
}

#[gpui::test]
fn paired_closes_the_dialog(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = code_entry(cx, &dir);

    h.host(&host()).event(NetEvent::Paired);
    assert!(!h.root(|root, _| root.device_pairing_open()));
    assert!(
        !host_commands(&mut h)
            .iter()
            .any(|command| matches!(command, NetCommand::PairCancel)),
        "a pairing that succeeded is not withdrawn"
    );
}

#[gpui::test]
fn token_save_failed_keeps_the_dialog_with_its_line(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = code_entry(cx, &dir);
    h.keys("1 2 3 4 5 6 enter");

    h.host(&host()).event(NetEvent::TokenSaveFailed);
    assert!(h.root(|root, _| root.device_pairing_open()));
    assert_eq!(
        line(&mut h).as_deref(),
        Some("Could not save this device's token; pair again.")
    );
    assert_eq!(buttons(&mut h), ["Ask again", "Close"]);
}

/// The dialog ended by a lost socket, then Ask again pressed: the host's
/// network thread redials to ask.
fn asked_again(h: &mut Harness<'_>) {
    h.host(&host()).event(NetEvent::PairingLost);
    assert_eq!(buttons(h), ["Ask again", "Close"]);
    h.click_on("device-pairing-ask-again");
    assert!(
        host_commands(h)
            .iter()
            .any(|command| matches!(command, NetCommand::PairRequest)),
        "Ask again asks the host's network thread"
    );
}

#[gpui::test]
fn a_failed_redial_after_ask_again_withdraws_the_request(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = waiting(cx, &dir);
    asked_again(&mut h);

    host_state(&mut h, connecting());
    assert!(
        h.root(|root, _| root.device_pairing_open()),
        "the redial is under way"
    );
    let mut dropped = connecting();
    let _ = dropped.on_closed("connection refused".to_owned());
    host_state(&mut h, dropped);
    assert!(!h.root(|root, _| root.device_pairing_open()));
    assert!(
        host_commands(&mut h)
            .iter()
            .any(|command| matches!(command, NetCommand::PairCancel)),
        "the request the next redial would send is withdrawn"
    );
}

#[gpui::test]
fn ask_again_shows_an_empty_code_field(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = code_entry(cx, &dir);
    h.keys("1 2 3 4 5 6");
    asked_again(&mut h);

    let mut host = h.host(&host());
    host.send(DaemonMessage::DevicePairPending {
        request_id: "r2".to_owned(),
        expires_in_secs: 300,
    });
    host.send(DaemonMessage::DevicePairDecided {
        request_id: "r2".to_owned(),
        outcome: DevicePairOutcome::Approved,
    });
    assert_eq!(
        h.root(RootView::device_pairing_code).as_deref(),
        Some(""),
        "digits typed for the old request are gone"
    );
}

#[gpui::test]
fn a_refused_code_returns_focus_to_the_field(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = code_entry(cx, &dir);
    h.keys("1 2 3 4 5 6 tab enter");
    assert!(
        host_commands(&mut h)
            .iter()
            .any(|command| matches!(command, NetCommand::PairRedeem { .. })),
        "Enter on the focused Pair sends the code"
    );
    h.host(&host()).send(DaemonMessage::DevicePairFailed {
        reason: "That code is not right.".to_owned(),
    });

    h.keys("6 5 4 3 2 1");
    assert_eq!(
        h.root(RootView::device_pairing_code).as_deref(),
        Some("654321"),
        "the next code goes into the field"
    );
}

#[gpui::test]
fn waiting_dialog_takes_a_cli_minted_code(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = waiting(cx, &dir);
    host_commands(&mut h);
    assert_eq!(h.root(RootView::device_pairing_code), None);

    h.click_on("device-pairing-have-code");
    assert_eq!(
        line(&mut h).as_deref(),
        Some("Approve this PC on box.tail1234.ts.net."),
        "the request still waits for the host"
    );
    assert_eq!(buttons(&mut h), ["Pair", "Cancel"]);
    h.keys("1 2 3 4 5 6 enter");
    let redeemed: Vec<String> = host_commands(&mut h)
        .into_iter()
        .filter_map(|command| match command {
            NetCommand::PairRedeem { code } => Some(code),
            _ => None,
        })
        .collect();
    assert_eq!(redeemed, ["123456"], "the code goes to the host");

    h.host(&host()).send(DaemonMessage::DevicePairFailed {
        reason: "That code is not right.".to_owned(),
    });
    assert_eq!(
        h.root(|root, _| root.device_pairing_refusal()).as_deref(),
        Some("That code is not right.")
    );

    h.keys("4 2");
    h.host(&host()).send(DaemonMessage::DevicePairDecided {
        request_id: "r1".to_owned(),
        outcome: DevicePairOutcome::Approved,
    });
    assert_eq!(
        line(&mut h).as_deref(),
        Some("Type the code box.tail1234.ts.net shows.")
    );
    assert_eq!(
        h.root(RootView::device_pairing_code).as_deref(),
        Some(""),
        "the approval clears what was typed for a CLI code"
    );
}

#[gpui::test]
fn the_picker_tags_an_unpaired_saved_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_saved_hosts(&dir, &[tailnet_entry(HOST), tailnet_entry(OTHER)]);
    let mut h = Harness::open(cx, &dir);
    h.tokens()
        .set(HOST, "device-token")
        .expect("store the token");

    open_picker(&mut h);
    assert_eq!(
        h.root(|root, _| root.connection_picker_not_paired()),
        [OTHER],
        "only the host this PC holds no token for is tagged"
    );
}

#[gpui::test]
fn forget_deletes_the_hosts_token(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    seed_saved_hosts(&dir, &[tailnet_entry(HOST)]);
    let mut h = Harness::open(cx, &dir);
    h.tokens()
        .set(HOST, "device-token")
        .expect("store the token");
    open_picker(&mut h);

    h.click_on(&format!("connections-forget-{HOST}"));
    assert_eq!(
        h.tokens().get(HOST).expect("read the token"),
        None,
        "Forget deletes the host's device token"
    );
    assert!(
        h.root(|root, _| root.toasts().is_empty()),
        "a delete that worked raises no toast"
    );
}
