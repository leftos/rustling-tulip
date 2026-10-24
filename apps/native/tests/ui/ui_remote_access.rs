//! Remote access page specs: the Settings tab after Worktrees, its switch
//! and port sending `ConfigureRemote`, its lines following the daemon's
//! `RemoteStatus`, its login-launch toggle over the autostart seam, its Tab
//! ring, its Waiting devices rows with the tab's badge, and its absence
//! while the target is a remote host.

use std::time::Duration;

use crate::support;

use chrono::{TimeDelta, Utc};
use gpui::{TestAppContext, px};
use protocol::{
    ClientMessage, DaemonMessage, DeviceInfo, FEATURE_DEVICE_BLOCK, PendingDevice, RemoteState,
};
use rustling_tulip_native::Connection;
use rustling_tulip_native::remote_access::state_line;
use rustling_tulip_native::target::Target;
use support::{Fixture, Harness, TestDir, session};

const HOST: &str = "box.tail1234.ts.net";
const TAB: &str = "settings-tab-remote-access";
const TOGGLE: &str = "settings-remote-toggle";
const PORT: &str = "settings-remote-port-input";
const APPLY: &str = "settings-remote-port-apply";
const AUTOSTART: &str = "settings-remote-autostart-toggle";
const NOT_CONNECTED: &str = "(daemon not connected)";

/// A connection to `target` open on `port`, welcomed with the fake daemon's
/// features.
fn open_on(target: Target, port: u16) -> Connection {
    let mut conn = Connection::for_target(target);
    conn.on_connecting();
    conn.on_socket_open(port);
    conn.on_welcome(support::PROTOCOL, support::daemon_features());
    conn
}

/// An open connection to the tailnet host [`HOST`].
fn remote() -> Connection {
    open_on(Target::tailnet(HOST, None), 8787)
}

/// An open connection to the local daemon, on the fake daemon's port.
fn local() -> Connection {
    open_on(Target::Local, 4242)
}

fn status(enabled: bool, port: u16, state: RemoteState) -> DaemonMessage {
    DaemonMessage::RemoteStatus {
        enabled,
        port,
        state,
    }
}

fn serving(port: u16) -> RemoteState {
    RemoteState::Serving {
        name: HOST.to_owned(),
        port,
        cert_expires_at: None,
    }
}

/// The window on session `s1` alone, its scrollback answered and what it
/// sent at connect drained.
fn opened<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &Fixture::single(session("s1").build()));
    h.answer_scrollback("s1", b"");
    h.sent();
    h
}

/// Delivers the page's fixture again: a spec that switches the target does
/// not count on the rows outliving the switch.
fn reload(h: &mut Harness<'_>) {
    h.load(&Fixture::single(session("s1").build()));
}

/// Settings open on the Remote access page.
fn open_page(h: &mut Harness<'_>) {
    h.keys("ctrl-,");
    h.click_on(TAB);
    assert_eq!(shown_tab(h), "Remote access");
}

fn shown_tab(h: &mut Harness<'_>) -> &'static str {
    h.root(|root, _| root.settings_tab())
}

fn settings_focus(h: &mut Harness<'_>) -> Option<&'static str> {
    let root = h.root.clone();
    h.cx.update(|window, cx| root.read(cx).settings_focus(window, cx))
}

fn type_text(h: &mut Harness<'_>, text: &str) {
    let keys: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    h.keys(&keys.join(" "));
}

/// The switch and port of every `ConfigureRemote` the client sent since the
/// last read; any other message fails the spec.
fn configures(h: &mut Harness<'_>) -> Vec<(bool, Option<u16>)> {
    let sent = h.sent();
    let configures: Vec<(bool, Option<u16>)> = sent
        .iter()
        .filter_map(|msg| match msg {
            ClientMessage::ConfigureRemote { enabled, port } => Some((*enabled, *port)),
            _ => None,
        })
        .collect();
    assert_eq!(
        configures.len(),
        sent.len(),
        "only ConfigureRemote is sent: {sent:?}"
    );
    configures
}

fn status_line(h: &mut Harness<'_>) -> String {
    h.root(|root, _| root.remote_status_line())
}

/// The switch's (ticked, can be pressed).
fn toggle(h: &mut Harness<'_>) -> (bool, bool) {
    h.root(|root, _| root.remote_toggle())
}

/// Whether `selector` was ever painted; gpui keeps an element's bounds
/// after it leaves, so a spec asks before the element was first drawn.
fn painted(h: &mut Harness<'_>, selector: &str) -> bool {
    h.bounds(selector).origin.x >= px(0.0)
}

/// A request from `phone` that asked 14 seconds ago.
fn request(id: &str, approved: bool) -> PendingDevice {
    PendingDevice {
        request_id: id.to_owned(),
        name: "phone".to_owned(),
        login: "alice@example.com".to_owned(),
        asked_at: Utc::now() - TimeDelta::seconds(14),
        approved,
    }
}

/// The daemon's waiting list, as it sends it.
fn fold(h: &mut Harness<'_>, requests: &[PendingDevice]) {
    h.send(DaemonMessage::PendingDevices {
        requests: requests.to_vec(),
    });
}

/// The Waiting devices rows as drawn: each line and its countdown or state.
fn waiting_rows(h: &mut Harness<'_>) -> Vec<(String, String)> {
    h.root(|root, _| root.pairing_rows())
}

/// The seconds an `m:ss left` countdown reads.
fn countdown_secs(text: &str) -> u64 {
    let clock = text.strip_suffix(" left").expect("an m:ss left countdown");
    let (minutes, seconds) = clock.split_once(':').expect("m:ss");
    assert_eq!(seconds.len(), 2, "{text}");
    minutes.parse::<u64>().expect("minutes") * 60 + seconds.parse::<u64>().expect("seconds")
}

#[gpui::test]
fn waiting_devices_row_lists_the_waiting_request(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_page(&mut h);
    fold(&mut h, &[request("r1", false)]);
    let rows = waiting_rows(&mut h);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "phone · alice@example.com");
    let left = countdown_secs(&rows[0].1);
    assert!(left > 0 && left <= 286, "asked 14 s ago: {}", rows[0].1);
    assert_eq!(
        h.root(|root, _| root.pairing_heading()),
        Some("Waiting devices · 1".to_owned())
    );
    assert!(painted(&mut h, "settings-waiting-devices"));
    assert!(painted(&mut h, "settings-waiting-row-r1"));
    assert!(painted(&mut h, "settings-waiting-approve-r1"));
    assert!(painted(&mut h, "settings-waiting-deny-r1"));
    let row = h.bounds("settings-waiting-row-r1").origin.y;
    let port = h.bounds(PORT).origin.y;
    assert!(row < port, "the section sits above the port row");
}

#[gpui::test]
fn waiting_devices_section_hidden_without_requests(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_page(&mut h);
    assert_eq!(waiting_rows(&mut h), []);
    assert_eq!(h.root(|root, _| root.pairing_heading()), None);
    assert!(!painted(&mut h, "settings-waiting-devices"));
}

#[gpui::test]
fn approved_row_waits_for_the_device_without_approve(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_page(&mut h);
    fold(&mut h, &[request("r1", true)]);
    assert_eq!(
        waiting_rows(&mut h),
        [(
            "phone · alice@example.com".to_owned(),
            "Approved · waiting for the device".to_owned()
        )]
    );
    assert!(painted(&mut h, "settings-waiting-deny-r1"));
    assert!(!painted(&mut h, "settings-waiting-approve-r1"));
}

#[gpui::test]
fn row_approve_opens_the_confirm_dialog_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_page(&mut h);
    fold(&mut h, &[request("r1", false)]);
    h.sent();
    h.click_on("settings-waiting-approve-r1");
    assert!(h.root(|root, _| root.pairing_dialog_open()));
    assert_eq!(
        h.root(|root, _| root.pairing_dialog_texts()).first(),
        Some(&"Approve phone".to_owned())
    );
    assert_eq!(
        h.root(|root, _| root.pairing_dialog_buttons()),
        ["Approve", "Deny", "Close"]
    );
    assert!(h.sent().is_empty(), "opening the dialog approves nothing");
}

#[gpui::test]
fn row_deny_sends_deny_device(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_page(&mut h);
    fold(&mut h, &[request("r1", false)]);
    h.sent();
    h.click_on("settings-waiting-deny-r1");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::DenyDevice { request_id }] if request_id == "r1"),
        "{sent:?}"
    );
    assert!(!h.root(|root, _| root.pairing_dialog_open()));
}

#[gpui::test]
fn no_tick_runs_while_the_remote_access_page_is_hidden(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let armed = |h: &mut Harness<'_>| h.root(|root, _| root.pairing_tick_armed());
    let second = Duration::from_secs(1);
    fold(&mut h, &[request("r1", false)]);
    assert!(!armed(&mut h), "Settings closed");
    h.keys("ctrl-,");
    assert!(!armed(&mut h), "Settings on General");
    h.click_on(TAB);
    assert!(armed(&mut h), "the page counts down");
    h.click_on("settings-tab-general");
    h.advance(second);
    assert!(!armed(&mut h), "another page");
    h.click_on(TAB);
    assert!(armed(&mut h), "back on the page");
    h.keys("escape");
    assert!(!h.root(|root, _| root.settings_open()));
    h.advance(second);
    assert!(!armed(&mut h), "Settings closed again");
    open_page(&mut h);
    assert!(armed(&mut h));
    fold(&mut h, &[request("r1", true)]);
    h.advance(second);
    assert!(!armed(&mut h), "an approved row has no countdown");
}

#[gpui::test]
fn waiting_count_badges_the_remote_access_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    assert_eq!(h.root(|root, _| root.pairing_badge()), None);
    assert!(!painted(&mut h, "settings-tab-remote-access-badge"));
    fold(&mut h, &[request("r1", false), request("r2", true)]);
    assert_eq!(
        h.root(|root, _| root.pairing_badge()),
        Some("2".to_owned()),
        "approved requests count"
    );
    assert!(painted(&mut h, "settings-tab-remote-access-badge"));
    fold(&mut h, &[request("r2", true)]);
    assert_eq!(h.root(|root, _| root.pairing_badge()), Some("1".to_owned()));
    fold(&mut h, &[]);
    assert_eq!(h.root(|root, _| root.pairing_badge()), None);
}

#[gpui::test]
fn waiting_requests_are_forgotten_on_a_remote_target(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_page(&mut h);
    fold(&mut h, &[request("r1", false)]);
    h.click_on("settings-waiting-approve-r1");
    assert!(h.root(|root, _| root.pairing_dialog_open()));
    h.set_connection(remote());
    reload(&mut h);
    assert_eq!(waiting_rows(&mut h), [], "the rows are forgotten");
    assert_eq!(h.root(|root, _| root.pairing_badge()), None);
    assert!(!h.root(|root, _| root.pairing_dialog_open()));
    h.keys("ctrl-,");
    assert!(
        !h.root(|root, _| root.settings_tab_labels())
            .contains(&"Remote access"),
        "the page and its rows are hidden"
    );
}

#[gpui::test]
fn remote_access_tab_follows_worktrees(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.keys("ctrl-,");
    assert_eq!(
        h.root(|root, _| root.settings_tab_rows()),
        [
            "General",
            "Notifications",
            "Spawn defaults",
            "Worktrees",
            "Remote access",
            "Appearance",
            "App title",
            "Agents",
            "Claude"
        ]
    );
    assert_eq!(
        h.root(|root, _| root.settings_tab_labels()),
        [
            "General",
            "Notifications",
            "Spawn defaults",
            "Worktrees",
            "Remote access",
            "Appearance",
            "App title",
            "Claude"
        ]
    );
    let worktrees = h.bounds("settings-tab-worktrees").origin.y;
    let appearance = h.bounds("settings-tab-appearance").origin.y;
    let page = h.bounds(TAB).origin.y;
    assert!(worktrees < page && page < appearance, "drawn between them");
}

#[gpui::test]
fn enable_sends_configure_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(status(false, 8787, RemoteState::Off));
    open_page(&mut h);
    assert_eq!(toggle(&mut h), (false, true));
    h.click_on(TOGGLE);
    assert_eq!(configures(&mut h), [(true, None)]);
    assert_eq!(toggle(&mut h), (false, true), "it follows the status only");

    h.send(status(true, 8787, serving(8787)));
    assert_eq!(toggle(&mut h), (true, true));
    assert_eq!(status_line(&mut h), "Serving on your tailnet.");
    assert_eq!(
        h.root(|root, _| root.remote_address()),
        Some(format!("https://{HOST}:8787"))
    );
    assert!(painted(&mut h, "settings-remote-address"));
}

#[gpui::test]
fn disable_sends_configure_remote_off(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(status(true, 8787, serving(8787)));
    open_page(&mut h);
    h.click_on(TOGGLE);
    assert_eq!(configures(&mut h), [(false, None)]);
}

#[gpui::test]
fn port_apply_sends_the_port_with_the_current_switch(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(status(true, 8787, RemoteState::Starting));
    open_page(&mut h);
    assert_eq!(
        h.root(|root, _| root.remote_port()),
        Some("8787".to_owned())
    );
    assert!(!h.root(|root, _| root.remote_port_apply()), "unchanged");

    h.click_on(PORT);
    h.keys("ctrl-a backspace");
    type_text(&mut h, "9000");
    assert!(h.root(|root, _| root.remote_port_apply()));
    h.click_on(APPLY);
    assert_eq!(configures(&mut h), [(true, Some(9000))]);

    h.send(status(true, 9000, RemoteState::Starting));
    assert!(!h.root(|root, _| root.remote_port_apply()), "applied");
    h.click_on(PORT);
    h.keys("ctrl-a backspace");
    type_text(&mut h, "9100");
    h.keys("enter");
    assert_eq!(configures(&mut h), [(true, Some(9100))], "Enter applies");

    h.keys("ctrl-a backspace");
    type_text(&mut h, "0");
    assert!(!h.root(|root, _| root.remote_port_apply()));
    assert_eq!(
        h.root(|root, _| root.remote_port_error()),
        Some("Enter a port from 1 to 65535.".to_owned())
    );
    h.keys("enter");
    h.click_on(APPLY);
    assert_eq!(configures(&mut h), [], "an invalid port sends nothing");
}

#[gpui::test]
fn status_line_follows_each_remote_state(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_page(&mut h);
    let reason = |text: &str| text.to_owned();
    let states = [
        RemoteState::Off,
        RemoteState::Starting,
        RemoteState::NotInstalled,
        RemoteState::NotRunning,
        RemoteState::OtherWindowsUser,
        RemoteState::NotConnected,
        RemoteState::TailscaleError {
            reason: reason("Tailscale timed out; see the daemon log."),
        },
        RemoteState::HttpsDisabled,
        RemoteState::HostTagged,
        RemoteState::CertFailed {
            reason: reason("The certificate could not be fetched."),
        },
        RemoteState::PortInUse,
        RemoteState::BindFailed {
            reason: reason("Binding the tailnet address failed."),
        },
        serving(9000),
        RemoteState::Unknown,
    ];
    for state in states {
        h.send(status(true, 9000, state.clone()));
        assert_eq!(status_line(&mut h), state_line(&state, 9000), "{state:?}");
    }
}

#[gpui::test]
fn controls_are_inert_before_a_status_arrives(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_page(&mut h);
    assert_eq!(toggle(&mut h), (false, false));
    assert_eq!(status_line(&mut h), NOT_CONNECTED);
    h.click_on(TOGGLE);
    h.click_on(PORT);
    type_text(&mut h, "9000");
    h.keys("enter");
    h.click_on(APPLY);
    assert_eq!(configures(&mut h), [], "nothing is sent before a status");
    assert_eq!(
        h.root(|root, _| root.remote_port()),
        Some(String::new()),
        "the field takes no keys"
    );
}

#[gpui::test]
fn a_new_connection_forgets_the_remote_status(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(status(false, 8787, RemoteState::Off));
    assert_eq!(status_line(&mut h), "Remote access is off.");
    h.send_welcome(support::PROTOCOL);
    assert_eq!(status_line(&mut h), NOT_CONNECTED);
    assert_eq!(toggle(&mut h), (false, false));
}

#[gpui::test]
fn page_is_hidden_while_the_target_is_remote(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.set_connection(remote());
    reload(&mut h);
    h.keys("ctrl-,");
    let has_page = |h: &mut Harness<'_>| {
        let rows = h.root(|root, _| root.settings_tab_rows());
        let labels = h.root(|root, _| root.settings_tab_labels());
        (
            rows.contains(&"Remote access"),
            labels.contains(&"Remote access"),
        )
    };
    assert_eq!(has_page(&mut h), (false, false));
    assert!(!painted(&mut h, TAB), "the row is never drawn");
    h.click_on("settings-tab-worktrees");
    h.keys("down");
    assert_eq!(
        shown_tab(&mut h),
        "Appearance",
        "Down skips the absent page"
    );

    h.set_connection(local());
    reload(&mut h);
    h.keys("ctrl-,");
    assert_eq!(has_page(&mut h), (true, true));
    h.click_on(TAB);
    assert_eq!(shown_tab(&mut h), "Remote access");
    h.set_connection(remote());
    reload(&mut h);
    assert_eq!(shown_tab(&mut h), "General", "the shown page falls back");
    h.keys("ctrl-,");
    assert_eq!(has_page(&mut h), (false, false));
    h.set_connection(local());
    reload(&mut h);
    h.keys("ctrl-,");
    assert_eq!(has_page(&mut h), (true, true), "the row comes back");
}

#[gpui::test]
fn tab_walks_the_remote_access_controls(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    h.send(status(false, 8787, RemoteState::Off));
    open_page(&mut h);
    assert_eq!(settings_focus(&mut h), Some("list"));
    let mut walk = Vec::new();
    for _ in 0..4 {
        h.keys("tab");
        walk.push(settings_focus(&mut h));
    }
    assert_eq!(
        walk,
        [Some(TOGGLE), Some(PORT), Some(AUTOSTART), Some("list")],
        "Apply is out of the ring while the port is unchanged"
    );
    let mut back = Vec::new();
    for _ in 0..4 {
        h.keys("shift-tab");
        back.push(settings_focus(&mut h));
    }
    assert_eq!(
        back,
        [Some(AUTOSTART), Some(PORT), Some(TOGGLE), Some("list")]
    );

    h.keys("tab tab");
    assert_eq!(settings_focus(&mut h), Some(PORT));
    h.keys("end");
    type_text(&mut h, "1");
    h.keys("up down");
    assert_eq!(
        shown_tab(&mut h),
        "Remote access",
        "the field keeps Up and Down"
    );
    assert_eq!(
        h.root(|root, _| root.remote_port()),
        Some("87871".to_owned())
    );
    h.keys("ctrl-a backspace");
    type_text(&mut h, "9000");
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some(APPLY), "a changed port joins");
    h.keys("tab");
    assert_eq!(settings_focus(&mut h), Some(AUTOSTART));
}

#[gpui::test]
fn autostart_toggle_calls_the_seam_and_shows_a_failure(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_page(&mut h);
    assert_eq!(h.root(|root, _| root.remote_autostart()), Some(false));
    h.click_on(AUTOSTART);
    assert_eq!(h.autostart().sets(), [true]);
    assert_eq!(h.root(|root, _| root.remote_autostart()), Some(true));
    assert_eq!(h.root(|root, _| root.remote_autostart_line()), None);

    h.autostart().fail_next_set("unsupported");
    h.click_on(AUTOSTART);
    assert_eq!(h.autostart().sets(), [true, false]);
    assert_eq!(
        h.root(|root, _| root.remote_autostart_line()),
        Some("Could not change start on login: unsupported.".to_owned())
    );
    assert_eq!(
        h.root(|root, _| root.remote_autostart()),
        Some(true),
        "a failed switch leaves the launch as it was"
    );
    assert!(painted(&mut h, "settings-remote-autostart-error"));

    h.set_connection(remote());
    reload(&mut h);
    assert_eq!(shown_tab(&mut h), "General");
    assert!(
        !h.root(|root, _| root.settings_tab_labels())
            .contains(&"Remote access"),
        "the toggle leaves with its page"
    );
    assert_eq!(h.autostart().sets(), [true, false]);
}

const DEVICES: &str = "settings-devices";
const CONFIRM_CANCEL: &str = "device-block-confirm-cancel";
const CONFIRM_BLOCK: &str = "device-block-confirm-block";

/// The window reconnects to this PC's daemon, whose `Welcome` lists
/// `device_block`; returns what the client sent on it.
fn with_device_block(h: &mut Harness<'_>) -> Vec<ClientMessage> {
    let mut conn = Connection::for_target(Target::Local);
    conn.on_connecting();
    conn.on_socket_open(4242);
    conn.on_welcome(support::PROTOCOL, vec![FEATURE_DEVICE_BLOCK.to_owned()]);
    h.set_connection(conn);
    h.send_welcome(support::PROTOCOL);
    reload(h);
    h.sent()
}

fn lists_devices(sent: &[ClientMessage]) -> usize {
    sent.iter()
        .filter(|msg| matches!(msg, ClientMessage::ListDevices))
        .count()
}

/// An approved device `id` named `name` with `connected` live connections,
/// last seen `seen_secs_ago` (never when none).
fn device(id: &str, name: &str, connected: u32, seen_secs_ago: Option<i64>) -> DeviceInfo {
    DeviceInfo {
        device_id: id.to_owned(),
        name: name.to_owned(),
        login: "alice@example.com".to_owned(),
        created_at: Utc::now() - TimeDelta::days(30),
        last_seen: seen_secs_ago.map(|secs| Utc::now() - TimeDelta::seconds(secs)),
        revoked: false,
        connected,
    }
}

fn blocked_device(id: &str, name: &str) -> DeviceInfo {
    DeviceInfo {
        revoked: true,
        ..device(id, name, 0, Some(60))
    }
}

/// The daemon's device list, as it sends it.
fn fold_devices(h: &mut Harness<'_>, devices: Vec<DeviceInfo>) {
    h.send(DaemonMessage::Devices { devices });
}

/// The Devices rows as drawn: name, login, and status or tag.
fn device_rows(h: &mut Harness<'_>) -> Vec<(String, String, String)> {
    h.root(|root, _| root.device_rows())
}

/// The block confirm's title and focused button, while it shows.
fn block_confirm(h: &mut Harness<'_>) -> Option<(String, &'static str)> {
    h.root(|root, _| root.device_block_confirm())
}

/// The Remote access page open on a daemon that blocks devices, listing
/// `devices`, with what was sent drained.
fn devices_page(h: &mut Harness<'_>, devices: Vec<DeviceInfo>) {
    with_device_block(h);
    open_page(h);
    fold_devices(h, devices);
    h.sent();
}

#[gpui::test]
fn devices_section_lists_approved_devices(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    devices_page(
        &mut h,
        vec![
            blocked_device("b1", "laptop"),
            device("s1", "tablet", 0, Some(7_200)),
            device("c1", "phone", 1, Some(5)),
        ],
    );
    let login = "alice@example.com".to_owned();
    assert_eq!(
        device_rows(&mut h),
        [
            ("phone".to_owned(), login.clone(), "Connected".to_owned()),
            (
                "tablet".to_owned(),
                login.clone(),
                "Last seen 2h ago".to_owned()
            ),
            ("laptop".to_owned(), login, "Blocked".to_owned()),
        ]
    );
    assert!(painted(&mut h, DEVICES));
    assert!(painted(&mut h, "settings-device-block-c1"));
    assert!(painted(&mut h, "settings-device-block-s1"));
    assert!(painted(&mut h, "settings-device-blocked-b1"));
    assert!(painted(&mut h, "settings-device-forget-b1"));
    assert!(!painted(&mut h, "settings-device-block-b1"));
    assert!(!painted(&mut h, "settings-device-forget-c1"));
    let y = |h: &mut Harness<'_>, id: &str| h.bounds(&format!("settings-device-{id}")).origin.y;
    let (c1, s1, b1) = (y(&mut h, "c1"), y(&mut h, "s1"), y(&mut h, "b1"));
    assert!(
        c1 < s1 && s1 < b1,
        "connected, then last seen, then blocked"
    );
}

#[gpui::test]
fn opening_the_remote_access_page_asks_for_devices(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    let sent = with_device_block(&mut h);
    assert_eq!(lists_devices(&sent), 1, "asked on Welcome: {sent:?}");
    h.keys("ctrl-,");
    h.sent();
    h.click_on(TAB);
    let sent = h.sent();
    assert_eq!(lists_devices(&sent), 1, "asked on the page: {sent:?}");
    fold_devices(&mut h, Vec::new());
    assert!(painted(&mut h, DEVICES));
    assert_eq!(device_rows(&mut h), []);
    assert!(painted(&mut h, "settings-devices-empty"));
}

#[gpui::test]
fn block_opens_confirm_and_confirming_sends_revoke_device(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    devices_page(&mut h, vec![device("d1", "phone", 1, Some(5))]);
    h.click_on("settings-device-block-d1");
    assert_eq!(
        block_confirm(&mut h),
        Some(("Block phone?".to_owned(), CONFIRM_CANCEL))
    );
    assert!(h.sent().is_empty(), "opening the confirm blocks nothing");
    h.click_on(CONFIRM_BLOCK);
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::RevokeDevice { device_id }] if device_id == "d1"),
        "{sent:?}"
    );
    assert_eq!(block_confirm(&mut h), None);
}

#[gpui::test]
fn cancel_closes_the_confirm_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    devices_page(&mut h, vec![device("d1", "phone", 0, None)]);
    h.click_on("settings-device-block-d1");
    assert!(block_confirm(&mut h).is_some());
    h.click_on(CONFIRM_CANCEL);
    assert_eq!(block_confirm(&mut h), None);
    assert!(h.sent().is_empty(), "{:?}", h.sent());
}

#[gpui::test]
fn escape_cancels_the_block_confirm(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    devices_page(&mut h, vec![device("d1", "phone", 0, None)]);
    h.click_on("settings-device-block-d1");
    h.keys("tab");
    assert_eq!(
        block_confirm(&mut h),
        Some(("Block phone?".to_owned(), CONFIRM_BLOCK))
    );
    h.keys("shift-tab");
    assert_eq!(
        block_confirm(&mut h),
        Some(("Block phone?".to_owned(), CONFIRM_CANCEL))
    );
    h.keys("escape");
    assert_eq!(block_confirm(&mut h), None);
    assert!(h.sent().is_empty());
    assert!(
        h.root(|root, _| root.settings_open()),
        "Esc closed only the confirm"
    );
    assert_eq!(shown_tab(&mut h), "Remote access");

    h.click_on("settings-device-block-d1");
    h.keys("tab enter");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::RevokeDevice { device_id }] if device_id == "d1"),
        "Enter presses the focused Block: {sent:?}"
    );
}

#[gpui::test]
fn forget_sends_forget_device(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    devices_page(&mut h, vec![blocked_device("b1", "phone")]);
    h.click_on("settings-device-forget-b1");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::ForgetDevice { device_id }] if device_id == "b1"),
        "{sent:?}"
    );
    assert_eq!(block_confirm(&mut h), None, "Forget asks nothing");
}

#[gpui::test]
fn blocked_device_row_shows_the_blocked_tag_after_the_reply(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    devices_page(&mut h, vec![device("d1", "phone", 1, Some(5))]);
    assert!(!painted(&mut h, "settings-device-blocked-d1"));
    h.click_on("settings-device-block-d1");
    h.click_on(CONFIRM_BLOCK);
    h.sent();
    fold_devices(&mut h, vec![blocked_device("d1", "phone")]);
    assert_eq!(
        device_rows(&mut h),
        [(
            "phone".to_owned(),
            "alice@example.com".to_owned(),
            "Blocked".to_owned()
        )]
    );
    assert!(painted(&mut h, "settings-device-blocked-d1"));
    assert!(painted(&mut h, "settings-device-forget-d1"));
}

#[gpui::test]
fn block_confirm_keeps_keys_over_an_approval_dialog_opened_beneath_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    devices_page(&mut h, vec![device("d1", "phone", 1, Some(5))]);
    fold(&mut h, &[request("r1", false)]);
    h.click_on("settings-device-block-d1");
    h.activate_pairing_toast(Some("approve:r1"));
    assert!(
        h.root(|root, _| root.pairing_dialog_open()),
        "the toast opens the approval dialog beneath the confirm"
    );
    h.sent();
    h.keys("tab enter");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::RevokeDevice { device_id }] if device_id == "d1"),
        "the confirm on top took the keys, and nothing was approved: {sent:?}"
    );
    assert_eq!(block_confirm(&mut h), None);
    let root = h.root.clone();
    assert!(
        h.cx.update(|window, cx| root.read(cx).pairing_has_focus(window)),
        "the approval dialog, now on top, has the keyboard"
    );
}

#[gpui::test]
fn devices_section_hidden_without_the_device_block_feature(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = opened(cx, &dir);
    open_page(&mut h);
    let sent = h.sent();
    assert_eq!(lists_devices(&sent), 0, "an older daemon: {sent:?}");
    fold_devices(&mut h, vec![device("d1", "phone", 0, None)]);
    assert!(!painted(&mut h, DEVICES));
    assert!(!painted(&mut h, "settings-device-d1"));
}
