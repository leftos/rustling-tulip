//! End-to-end specs for a remote update of the host, run through the
//! daemon's host-update sandbox: a daemon built with `e2e-loopback-peers` and
//! started with `RUSTLING_TULIP_E2E_HOST_UPDATE=1` from a scratch install of
//! its own records the installer it would start in
//! `<config>/e2e-host-update-launch.json` and starts nothing. The release
//! comes from the fake feed [`UpdateFeed`]. They need that daemon build beside
//! the test binary, so they are ignored by default and run by
//! `.\rt.ps1 native-e2e`.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use std::net::TcpStream;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use daemon_client::device_tokens::{DeviceTokenStore as _, MemoryTokens};
use gpui::TestAppContext;
use protocol::error_codes::{BUSY, UPDATE_CANCELED, UPDATE_REFUSED};
use protocol::{ClientKind, ClientMessage, DaemonMessage, RemoteState};
use rustling_tulip_native::target::{HostKey, Target};
use rustling_tulip_native::{
    Connection, NATIVE_PROTOCOL_VERSIONS, NetCommand, Toast, toast_action_selector,
};
use serde_json::{Value, json};
use support::Harness;
use support::live::{
    HANG_GUARD, LiveClient, LiveDaemon, LiveHostSpec, LiveHosts, REMOTE_HOST, free_port,
};
use support::update_feed::UpdateFeed;
use tokio_tungstenite::tungstenite::stream::MaybeTlsStream;
use tokio_tungstenite::tungstenite::{self, Message, WebSocket};

/// The release the fake feed offers: newer than the build's `0.11.0-alpha`.
const FEED_VERSION: &str = "9.9.9-alpha";
/// The installer's bytes; the daemon checks only their `.sha256`.
const INSTALLER: &[u8] = b"fake installer";
/// The installer's file name in the feed.
const FEED_INSTALLER: &str = "rustling-tulip-9.9.9-alpha-setup.exe";
/// The path the daemon reads the release list from.
const FEED_LIST: &str = "/repos/leftos/rustling-tulip/releases?per_page=100";
/// The record the sandbox writes in the config dir in place of a launch.
const LAUNCH_RECORD: &str = "e2e-host-update-launch.json";
/// The installer's arguments for an update with the app not running
/// (`INSTALLER_ARGS` in `crates/daemon/src/server/update.rs`).
const INSTALLER_ARGS: [&str; 3] = ["/S", "/UPDATE", "/RESTARTDAEMON"];
/// Who asked, for a request from this PC (`THIS_PC` in the same file).
const THIS_PC: &str = "a connection on this PC";
/// The tailnet device's name: the fake tailscale's `whois` node name,
/// without its trailing dot.
const DEVICE_NAME: &str = "e2e-peer";
/// What a download whose hash does not match refuses with
/// (`DOWNLOAD_FAILED` in the same file).
const DOWNLOAD_FAILED: &str = "This host couldn't download the update; try again later.";
/// The line the daemon logs when no local connection holds the update back.
const LAUNCH_AT_ONCE: &str = "remote update: no local connection; launching at once";
/// The login the fake tailscale gives every node's user.
const FAKE_LOGIN: &str = "owner@example.com";

/// The client id of the test's raw socket that connects as a phone.
const PHONE_ID: &str = "rt-e2e-phone";
/// The client id of the test's raw socket that connects as a native client
/// without the `update_yourself` capability, as an older build does.
const OLD_NATIVE_ID: &str = "rt-e2e-old-native";
/// The self-update countdown toast's title, before its seconds
/// (`update_yourself_counts_down_naming_the_host` in `tests/ui/ui_update.rs`).
const SELF_UPDATE_TITLE: &str = "Updating this app in ";
/// The self-update countdown toast's Cancel button.
const CLIENT_UPDATE_CANCEL: &str = "client-update-cancel";

/// A request's answer: the version a `RemoteUpdateStarted` names, or an
/// `Error`'s code and message.
type Answer = Result<String, (Option<String>, String)>;

/// A raw socket of the test's own on a daemon's local port.
type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

/// With no local connection on the host, a paired tailnet device's update
/// launches at once: the sandbox records the installer under the updates
/// dir, after the feed served the release list, the `.sha256` and the
/// installer. Once the host restarts as the installer would leave it, the
/// reconnected device is told the update's result after its `Welcome`.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn a_sandboxed_update_from_the_tailnet_launches_at_once_and_reports_after_restart(
    host_cx: &mut TestAppContext,
    device_cx: &mut TestAppContext,
) {
    // The home daemon starts first: the update host has no client until the
    // loopback one connects, and an idle exit before then would be replaced
    // by a daemon without the sandbox.
    let home = LiveDaemon::start("update-launch-home");
    let feed = UpdateFeed::start(FEED_VERSION, INSTALLER);
    let mut daemon = LiveDaemon::start_update_host("update-launch", &feed, None, true);
    let (mut host, host_client) = Harness::open_live(host_cx, &daemon);
    let port = serve_remote(&mut host, &host_client, &daemon);
    assert_same_daemon(&daemon);
    let tokens = Arc::new(MemoryTokens::default());
    let (mut device, device_client) = open_device(device_cx, &home, &daemon, port, &tokens);
    wait_not_approved(&mut device, &device_client, 0);
    pair(&mut host, &host_client, &mut device, &device_client);

    // The loopback client quits as the app does, never to reconnect, so the
    // host runs again with the tailnet device as its only connection.
    host_client.command(NetCommand::Shutdown {
        before: Vec::new(),
        drain: false,
    });
    host.wait_until("the host's daemon to stop", |_| !daemon.answers_health());
    let from = device_client.message_count();
    daemon.restart_after_reboot();
    wait_welcome(&mut device, &device_client, from, "the device to reconnect");
    assert_same_daemon(&daemon);

    let from = device_client.message_count();
    device_client.send(ClientMessage::RemoteUpdate {
        request_id: "ru-1".to_owned(),
    });
    let answer = wait_answer(&mut device, &device_client, from, "ru-1");
    assert_eq!(answer, Ok(FEED_VERSION.to_owned()), "the update started");
    assert!(
        daemon.daemon_log().contains(LAUNCH_AT_ONCE),
        "no local connection counted the update down"
    );
    let record = launch_record(&daemon).expect("the sandbox recorded the launch");
    assert_eq!(record["args"], json!(INSTALLER_ARGS), "{record}");
    let program = record["program"]
        .as_str()
        .expect("the record names a program");
    assert!(
        Path::new(program).starts_with(daemon.updates_dir()),
        "the installer is the download under the updates dir: {program}"
    );
    let requests = feed.requests();
    for path in [
        FEED_LIST.to_owned(),
        format!("/dl/{FEED_INSTALLER}.sha256"),
        format!("/dl/{FEED_INSTALLER}"),
    ] {
        assert!(
            requests.contains(&path),
            "the feed served {path}: {requests:?}"
        );
    }

    let result = json!({
        "from": "0.11.0-alpha",
        "to": FEED_VERSION,
        "ok": true,
        "message": "Updated to 9.9.9-alpha.",
    });
    let from = device_client.message_count();
    daemon.restart_after_update(&result);
    assert_eq!(
        update_core::read_last_result(&daemon.updates_dir().join("last-result.json")),
        Some(update_core::LastResult {
            from: Some("0.11.0-alpha".to_owned()),
            to: FEED_VERSION.to_owned(),
            ok: true,
            message: "Updated to 9.9.9-alpha.".to_owned(),
        }),
        "the fixture is what the updater writes"
    );
    let mut reported = None;
    device.wait_until("the restarted host to report the result", |_| {
        reported = device_client.find_message_from(from, host_update_result);
        reported.is_some()
    });
    assert_eq!(
        reported,
        Some((
            Some("0.11.0-alpha".to_owned()),
            FEED_VERSION.to_owned(),
            true,
            "Updated to 9.9.9-alpha.".to_owned()
        ))
    );
    let welcome = last_index(&device_client, from, |msg| {
        matches!(msg, DaemonMessage::Welcome { .. })
    });
    let result_at = last_index(&device_client, from, |msg| {
        host_update_result(msg).is_some()
    });
    assert!(
        welcome.is_some() && welcome < result_at,
        "the result came after the reconnect's Welcome: {welcome:?} < {result_at:?}"
    );
}

/// A host update the tailnet device asks for counts down on the host's local
/// client; that client's cancel ends it: the device is told
/// `update_canceled`, the local client `HostUpdateCanceled`, and no installer
/// is recorded.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn a_local_cancel_ends_the_host_update_and_tells_the_requester(
    host_cx: &mut TestAppContext,
    device_cx: &mut TestAppContext,
) {
    let mut rig = Countdown::start(host_cx, device_cx, "update-cancel");
    assert_eq!(rig.requested_by, DEVICE_NAME, "the notice names the device");

    rig.host_client.send(ClientMessage::CancelHostUpdate {
        update_id: rig.update_id.clone(),
    });
    let answer = wait_answer(&mut rig.device, &rig.device_client, rig.device_from, "ru-2");
    let (code, message) = answer.expect_err("the cancel refuses the update");
    assert_eq!(code.as_deref(), Some(UPDATE_CANCELED), "{message}");
    let update_id = rig.update_id.clone();
    let host_client = rig.host_client.clone();
    rig.host
        .wait_until("the local client to hear of the cancel", |_| {
            host_client
                .find_message_from(rig.host_from, |msg| match msg {
                    DaemonMessage::HostUpdateCanceled { update_id: id } if *id == update_id => {
                        Some(())
                    }
                    _ => None,
                })
                .is_some()
        });
    assert_eq!(
        launch_record(&rig.daemon),
        None,
        "no installer was recorded"
    );
}

/// A second update request while one counts down is refused as busy.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn a_second_request_during_a_countdown_is_busy(
    host_cx: &mut TestAppContext,
    device_cx: &mut TestAppContext,
) {
    let mut rig = Countdown::start(host_cx, device_cx, "update-busy");

    let from = rig.device_client.message_count();
    rig.device_client.send(ClientMessage::RemoteUpdate {
        request_id: "ru-3".to_owned(),
    });
    let answer = wait_answer(&mut rig.device, &rig.device_client, from, "ru-3");
    let (code, message) = answer.expect_err("the second request is refused");
    assert_eq!(code.as_deref(), Some(BUSY), "{message}");
    assert_eq!(
        launch_record(&rig.daemon),
        None,
        "no installer was recorded"
    );
}

/// An SSH requester reaches the host over loopback, so it counts as local:
/// its own connection is told the countdown, and the update starts.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn a_sandboxed_update_over_ssh_counts_down_for_the_ssh_requester(ssh_cx: &mut TestAppContext) {
    // The home daemon starts first: the update host has no client until the
    // bridge connects, and an idle exit before then would have the bridge
    // start a daemon without the sandbox.
    let home = LiveDaemon::start("update-ssh-home");
    let feed = UpdateFeed::start(FEED_VERSION, INSTALLER);
    let daemon = LiveDaemon::start_update_host("update-ssh", &feed, Some(1), false);
    let fake = daemon.fake_ssh(None);
    fake.use_for_this_process();
    let (mut h, hosts) = open_ssh(ssh_cx, &home, &daemon);
    let client = hosts.client(&ssh_key());
    h.wait_until("the client to connect over ssh", |_| {
        client.states().last().is_some_and(Connection::is_open)
    });
    assert_same_daemon(&daemon);

    let from = client.message_count();
    client.send(ClientMessage::RemoteUpdate {
        request_id: "ru-ssh".to_owned(),
    });
    let answer = wait_answer(&mut h, &client, from, "ru-ssh");
    assert_eq!(answer, Ok(FEED_VERSION.to_owned()), "the update started");
    // Only the presence of the notice is checked, not its order against the
    // answer: the notice reaches the connection through the broadcast and its
    // forwarder task, the answer straight through its outbox. Notices reach
    // only local connections, and a requester that is not local launches
    // with no notice at all, so the notice alone shows the SSH requester
    // counted as local.
    let notice =
        client.find_message_from(from, |msg| scheduled(msg).filter(|(_, by)| by == THIS_PC));
    assert!(
        notice.is_some(),
        "the SSH requester's own connection was told the countdown"
    );
    assert!(
        launch_record(&daemon).is_some(),
        "the sandbox recorded the launch"
    );
}

/// A release whose `.sha256` does not match its installer is refused, and
/// no installer is recorded.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn a_hash_mismatch_refuses_the_update(cx: &mut TestAppContext) {
    let feed = UpdateFeed::start_with_wrong_hash(FEED_VERSION, INSTALLER);
    let daemon = LiveDaemon::start_update_host("update-hash", &feed, None, false);
    let (mut h, client) = Harness::open_live(cx, &daemon);
    h.wait_until("the loopback client to connect", |_| {
        client.states().last().is_some_and(Connection::is_open)
    });
    assert_same_daemon(&daemon);

    let from = client.message_count();
    client.send(ClientMessage::RemoteUpdate {
        request_id: "ru-hash".to_owned(),
    });
    let answer = wait_answer(&mut h, &client, from, "ru-hash");
    assert_eq!(
        answer,
        Err((Some(UPDATE_REFUSED.to_owned()), DOWNLOAD_FAILED.to_owned())),
        "the mismatch refuses the update"
    );
    // The installer is fetched only once the `.sha256` parsed, so both
    // requests pin the refusal to the hash check.
    let requests = feed.requests();
    for path in [
        format!("/dl/{FEED_INSTALLER}.sha256"),
        format!("/dl/{FEED_INSTALLER}"),
    ] {
        assert!(
            requests.contains(&path),
            "the daemon fetched {path}: {requests:?}"
        );
    }
    assert_eq!(launch_record(&daemon), None, "no installer was recorded");
}

/// A host's `UpdateConnectedClients` reaches each connected client by its
/// kind: the paired tailnet device, a native client that updates itself, is
/// told `UpdateYourself` and counts down; a phone is told `ReloadApp`; a
/// native client without the capability is counted as too old and told
/// nothing; and the requester hears the three counts. The device's Cancel
/// ends its countdown, so nothing installs, and the host stays up.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn a_host_tells_its_connected_clients_to_update_each_by_its_kind(
    host_cx: &mut TestAppContext,
    device_cx: &mut TestAppContext,
) {
    // The home daemon starts first: the update host has no client until the
    // loopback one connects, and an idle exit before then would be replaced
    // by a daemon without the sandbox.
    let home = LiveDaemon::start("update-clients-home");
    let feed = UpdateFeed::start(FEED_VERSION, INSTALLER);
    let daemon = LiveDaemon::start_update_host("update-clients", &feed, None, true);
    let (mut host, host_client) = Harness::open_live(host_cx, &daemon);
    let port = serve_remote(&mut host, &host_client, &daemon);
    assert_same_daemon(&daemon);
    let tokens = Arc::new(MemoryTokens::default());
    let (mut device, device_client) = open_device(device_cx, &home, &daemon, port, &tokens);
    wait_not_approved(&mut device, &device_client, 0);
    pair(&mut host, &host_client, &mut device, &device_client);
    let mut phone = connect_as(&daemon, PHONE_ID, ClientKind::Phone);
    let mut old_native = connect_as(&daemon, OLD_NATIVE_ID, ClientKind::Native);

    let from = host_client.message_count();
    host_client.send(ClientMessage::UpdateConnectedClients {
        request_id: Some("ucc-1".to_owned()),
    });
    let mut counts = None;
    host.wait_until("the host to answer ucc-1", |_| {
        counts = host_client.find_message_from(from, |msg| notified(msg, "ucc-1"));
        counts.is_some()
    });
    let (native, phones, too_old) = counts.expect("the wait just found it");
    assert_eq!(native, 1, "the paired device updates itself");
    assert_eq!(phones, 1, "the phone socket");
    assert_eq!(too_old, 1, "the old native socket");

    let frames = read_until(&mut phone, "\"type\":\"reload_app\"");
    let last: Value = serde_json::from_str(frames.last().expect("read_until returns the match"))
        .expect("a JSON daemon message");
    assert_eq!(last["type"], "reload_app", "the phone is told to reload");

    // The daemon queues each connection's UpdateYourself before it answers
    // the requester, and a socket's messages arrive in order, so the answer
    // to a request sent now comes after any UpdateYourself.
    send(&mut old_native, &ClientMessage::ListSessions);
    let frames = read_until(&mut old_native, "\"type\":\"sessions\"");
    assert!(
        !frames
            .iter()
            .any(|text| text.contains("\"type\":\"update_yourself\"")),
        "the old native client is not told to update itself: {frames:?}"
    );

    cancel_self_update(&mut device);
    assert!(daemon.answers_health(), "the host is still up");
    let _ = phone.close(None);
    let _ = old_native.close(None);
}

/// A `ConnectedClientsNotified` answering `request_id`: its native, phone and
/// too-old counts.
fn notified(msg: &DaemonMessage, request_id: &str) -> Option<(u32, u32, u32)> {
    match msg {
        DaemonMessage::ConnectedClientsNotified {
            request_id: Some(id),
            native,
            phones,
            too_old,
        } if id == request_id => Some((*native, *phones, *too_old)),
        _ => None,
    }
}

/// Waits for `device`'s self-update countdown toast, clicks its Cancel and
/// waits for the toast to go.
fn cancel_self_update(device: &mut Harness<'_>) {
    let mut id = None;
    device.wait_until("the device to count its own update down", |h| {
        id = self_update_toasts(h)
            .iter()
            .find(|toast| toast.title.starts_with(SELF_UPDATE_TITLE))
            .map(|toast| toast.id);
        id.is_some()
    });
    let id = id.expect("the wait just found it");
    device.click_on(&toast_action_selector(id, CLIENT_UPDATE_CANCEL));
    device.wait_until("the self-update countdown to go", |h| {
        self_update_toasts(h).is_empty()
    });
}

/// The self-update countdown toasts that show.
fn self_update_toasts(h: &mut Harness<'_>) -> Vec<Toast> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .filter(|toast| {
                toast
                    .key
                    .as_deref()
                    .is_some_and(|key| key.starts_with("client-update-"))
            })
            .cloned()
            .collect()
    })
}

/// A socket of the test's own on `daemon`'s local port, saying it is a
/// `kind` client with no features, read past its handshake and the initial
/// session list: the daemon queues that list after it registers the
/// connection, so a request sent once it is read counts this socket, and a
/// later `sessions` frame is the answer to the test's own `ListSessions`.
/// Its read timeout is a hang guard only.
fn connect_as(daemon: &LiveDaemon, client_id: &str, kind: ClientKind) -> Socket {
    let handshake = daemon.handshake();
    let url = format!("ws://127.0.0.1:{}/ws", handshake.port);
    let (mut ws, _) = tungstenite::connect(url).expect("open a socket to the daemon");
    if let MaybeTlsStream::Plain(stream) = ws.get_ref() {
        TcpStream::set_read_timeout(stream, Some(HANG_GUARD)).expect("set the socket's timeout");
    }
    send(
        &mut ws,
        &ClientMessage::Hello {
            protocol_version: NATIVE_PROTOCOL_VERSIONS[0],
            protocol_versions: NATIVE_PROTOCOL_VERSIONS.to_vec(),
            auth_token: handshake.auth_token.clone(),
            client_id: Some(client_id.to_owned()),
            client_name: None,
            client_version: None,
            client_kind: Some(kind),
            client_features: Vec::new(),
        },
    );
    read_until(&mut ws, "\"type\":\"sessions\"");
    ws
}

/// Sends `msg` on the test's socket.
fn send(ws: &mut Socket, msg: &ClientMessage) {
    let text = serde_json::to_string(msg).expect("encode a client message");
    ws.send(Message::Text(text.into()))
        .expect("write to the test's socket");
}

/// Reads text frames until one contains `needle`; returns every text frame
/// read, the match last. The daemon's broadcasts keep the socket busy, so
/// the wait has its own hang guard besides the socket's read timeout.
fn read_until(ws: &mut Socket, needle: &str) -> Vec<String> {
    let deadline = Instant::now() + HANG_GUARD;
    let mut frames = Vec::new();
    loop {
        assert!(
            Instant::now() < deadline,
            "hung: waited {HANG_GUARD:?} for a daemon message containing {needle:?}"
        );
        let frame = ws
            .read()
            .map_err(|err| format!("hung or closed waiting for {needle:?}: {err}"))
            .expect("the daemon answers the test's socket");
        if let Message::Text(text) = frame {
            let text = text.as_str().to_owned();
            let found = text.contains(needle);
            frames.push(text);
            if found {
                return frames;
            }
        }
    }
}

/// A host with a 600 s countdown, its loopback client attached, and a paired
/// tailnet device whose `RemoteUpdate` `ru-2` the local client was told of.
/// The harnesses are declared first, so they drop before the daemons.
struct Countdown<'a> {
    device: Harness<'a>,
    device_client: LiveClient,
    host: Harness<'a>,
    host_client: LiveClient,
    _home: LiveDaemon,
    daemon: LiveDaemon,
    _feed: UpdateFeed,
    /// The countdown's id and who it names as asking.
    update_id: String,
    requested_by: String,
    /// The message counts as `ru-2` went out.
    device_from: usize,
    host_from: usize,
}

impl<'a> Countdown<'a> {
    fn start(
        host_cx: &'a mut TestAppContext,
        device_cx: &'a mut TestAppContext,
        test: &str,
    ) -> Self {
        // The home daemon starts first, so the update host is not left
        // without a client long enough to exit idle.
        let home = LiveDaemon::start(&format!("{test}-home"));
        let feed = UpdateFeed::start(FEED_VERSION, INSTALLER);
        let daemon = LiveDaemon::start_update_host(test, &feed, Some(600), true);
        let (mut host, host_client) = Harness::open_live(host_cx, &daemon);
        let port = serve_remote(&mut host, &host_client, &daemon);
        assert_same_daemon(&daemon);
        let tokens = Arc::new(MemoryTokens::default());
        let (mut device, device_client) = open_device(device_cx, &home, &daemon, port, &tokens);
        wait_not_approved(&mut device, &device_client, 0);
        pair(&mut host, &host_client, &mut device, &device_client);

        let host_from = host_client.message_count();
        let device_from = device_client.message_count();
        device_client.send(ClientMessage::RemoteUpdate {
            request_id: "ru-2".to_owned(),
        });
        let mut notice = None;
        host.wait_until("the local client to be told the countdown", |_| {
            notice = host_client.find_message_from(host_from, scheduled);
            notice.is_some()
        });
        let (update_id, requested_by) = notice.expect("the wait just found it");
        Self {
            device,
            device_client,
            host,
            host_client,
            _home: home,
            daemon,
            _feed: feed,
            update_id,
            requested_by,
            device_from,
            host_from,
        }
    }
}

/// Asserts `daemon.json` still names the daemon `daemon` started, so a
/// replacement started without the sandbox fails loudly.
fn assert_same_daemon(daemon: &LiveDaemon) {
    let on_disk = daemon_client::read_handshake_in(&daemon.config_dir())
        .expect("the daemon's daemon.json reads");
    assert_eq!(
        on_disk.pid,
        daemon.handshake().pid,
        "daemon.json names the sandboxed daemon, not a replacement"
    );
}

/// The launch the sandbox recorded in `daemon`'s config dir, once written.
fn launch_record(daemon: &LiveDaemon) -> Option<Value> {
    let text = std::fs::read_to_string(daemon.config_dir().join(LAUNCH_RECORD)).ok()?;
    Some(serde_json::from_str(&text).expect("the launch record is JSON"))
}

/// `msg` as the answer to `request_id`, if it is one.
fn answer_to(msg: &DaemonMessage, request_id: &str) -> Option<Answer> {
    match msg {
        DaemonMessage::RemoteUpdateStarted {
            request_id: id,
            version,
        } if id == request_id => Some(Ok(version.clone())),
        DaemonMessage::Error {
            request_id: Some(id),
            message,
            code,
        } if id == request_id => Some(Err((code.clone(), message.clone()))),
        _ => None,
    }
}

/// Waits for the answer to `request_id` among `client`'s messages from
/// index `from` on.
fn wait_answer(h: &mut Harness<'_>, client: &LiveClient, from: usize, request_id: &str) -> Answer {
    let mut answer = None;
    h.wait_until(&format!("the host to answer {request_id}"), |_| {
        answer = client.find_message_from(from, |msg| answer_to(msg, request_id));
        answer.is_some()
    });
    answer.expect("the wait just found it")
}

/// A `HostUpdateScheduled`'s update id and who it names as asking.
fn scheduled(msg: &DaemonMessage) -> Option<(String, String)> {
    match msg {
        DaemonMessage::HostUpdateScheduled {
            update_id,
            requested_by,
            ..
        } => Some((update_id.clone(), requested_by.clone())),
        _ => None,
    }
}

/// A `HostUpdateResult`'s fields.
fn host_update_result(msg: &DaemonMessage) -> Option<(Option<String>, String, bool, String)> {
    match msg {
        DaemonMessage::HostUpdateResult {
            from,
            to,
            ok,
            message,
        } => Some((from.clone(), to.clone(), *ok, message.clone())),
        _ => None,
    }
}

/// The index of the newest of `client`'s messages from `from` on that
/// `matches`.
fn last_index(
    client: &LiveClient,
    from: usize,
    matches: impl Fn(&DaemonMessage) -> bool,
) -> Option<usize> {
    (from..client.message_count()).rev().find(|index| {
        client
            .find_message_from(*index, |msg| matches(msg).then_some(()))
            .is_some()
    })
}

/// Waits for a `Welcome` among `client`'s messages from `from` on, with the
/// view's latest state open.
fn wait_welcome(h: &mut Harness<'_>, client: &LiveClient, from: usize, what: &str) {
    h.wait_until(what, |_| {
        client
            .find_message_from(from, |msg| {
                matches!(msg, DaemonMessage::Welcome { .. }).then_some(())
            })
            .is_some()
            && client.states().last().is_some_and(Connection::is_open)
    });
}

/// Connects `host` over loopback, turns remote access on at a free port and
/// waits for it to serve; returns the port.
fn serve_remote(host: &mut Harness<'_>, host_client: &LiveClient, daemon: &LiveDaemon) -> u16 {
    let daemon_port = daemon.handshake().port;
    host.wait_until("the loopback client to connect", |_| {
        host_client
            .latest()
            .is_some_and(|(port, overlay)| port == Some(daemon_port) && overlay.is_none())
    });
    let port = free_port();
    host_client.send(ClientMessage::ConfigureRemote {
        enabled: true,
        port: Some(port),
    });
    host.wait_until("remote access to report Serving", |_| {
        matches!(
            remote_state(host_client),
            Some(RemoteState::Serving { port: serving, .. }) if serving == port
        )
    });
    port
}

/// The newest remote access state the daemon sent `client`.
fn remote_state(client: &LiveClient) -> Option<RemoteState> {
    client.find_message(|msg| match msg {
        DaemonMessage::RemoteStatus { state, .. } => Some(state.clone()),
        _ => None,
    })
}

/// The host key of the tailnet host [`REMOTE_HOST`], whatever its port.
fn remote_key() -> HostKey {
    HostKey::of(&Target::tailnet(REMOTE_HOST, None))
}

/// A device window whose This PC is `home` and whose one saved host is
/// `daemon`'s remote access on `port`, trusting its test CA and holding
/// device tokens in `tokens`; returns the client of that tailnet host.
fn open_device<'a>(
    cx: &'a mut TestAppContext,
    home: &LiveDaemon,
    daemon: &LiveDaemon,
    port: u16,
    tokens: &Arc<MemoryTokens>,
) -> (Harness<'a>, LiveClient) {
    let paired = tokens
        .get(REMOTE_HOST)
        .expect("the token store answers")
        .is_some();
    let remote = LiveHostSpec {
        target: Target::tailnet(REMOTE_HOST, Some(port)),
        config_dir: daemon.config_dir(),
        tls: Some(daemon.tailnet_tls()),
        connects: paired,
    };
    let (h, hosts) = Harness::open_hosts(cx, home, vec![remote], Arc::clone(tokens));
    (h, hosts.client(&remote_key()))
}

/// Waits for a "device not approved" among the messages from index `from` on,
/// with the view showing the device as not paired.
fn wait_not_approved(h: &mut Harness<'_>, client: &LiveClient, from: usize) {
    h.wait_until("the host to land the device in pairing", |_| {
        client
            .find_message_from(from, |msg| {
                matches!(
                    msg,
                    DaemonMessage::DevicePairFailed { reason } if reason == protocol::DEVICE_NOT_APPROVED
                )
                .then_some(())
            })
            .is_some()
            && client
                .footer_labels()
                .last()
                .is_some_and(|label| label == "not paired")
    });
}

/// Pairs `device` through the owner's loopback `host`: the device asks, the
/// host approves its waiting request, and the device redeems the code and
/// reconnects with its new token. Returns once the device is connected.
fn pair(
    host: &mut Harness<'_>,
    host_client: &LiveClient,
    device: &mut Harness<'_>,
    device_client: &LiveClient,
) {
    let from = device_client.message_count();
    device_client.command(NetCommand::PairRequest);
    let mut request_id = None;
    device.wait_until("the host to hold the pairing request", |_| {
        request_id = device_client.find_message_from(from, |msg| match msg {
            DaemonMessage::DevicePairPending { request_id, .. } => Some(request_id.clone()),
            _ => None,
        });
        request_id.is_some()
    });
    let request_id = request_id.expect("the wait just found it");

    host_client.send(ClientMessage::ListPendingDevices);
    let mut pending = None;
    host.wait_until("the host to list the waiting device", |_| {
        pending = host_client.find_message(|msg| match msg {
            DaemonMessage::PendingDevices { requests } => requests
                .iter()
                .find(|request| request.request_id == request_id)
                .cloned(),
            _ => None,
        });
        pending.is_some()
    });
    let pending = pending.expect("the wait just found it");
    assert_eq!(
        pending.login, FAKE_LOGIN,
        "the request names the peer's login"
    );

    host_client.send(ClientMessage::ApproveDevice {
        request_id: request_id.clone(),
    });
    let mut code = None;
    host.wait_until("the code for the approved request", |_| {
        code = host_client.find_message(|msg| match msg {
            DaemonMessage::DevicePairCode {
                request_id: Some(id),
                code,
                ..
            } if *id == request_id => Some(code.0.clone()),
            _ => None,
        });
        code.is_some()
    });
    device_client.command(NetCommand::PairRedeem {
        code: code.expect("the wait just found it"),
    });
    device.wait_until("the paired device to connect with its token", |_| {
        device_client
            .states()
            .last()
            .is_some_and(Connection::is_open)
    });
}

/// The saved SSH host's string; the fake ssh ignores it.
const SSH_HOST: &str = "devbox";

/// The host key of the SSH host [`SSH_HOST`].
fn ssh_key() -> HostKey {
    HostKey::of(&Target::parse_ssh(SSH_HOST, None).expect("devbox is a valid ssh host"))
}

/// A window whose This PC is `home` and whose one saved host is
/// [`SSH_HOST`], reaching `daemon` through the fake ssh.
fn open_ssh<'a>(
    cx: &'a mut TestAppContext,
    home: &LiveDaemon,
    daemon: &LiveDaemon,
) -> (Harness<'a>, LiveHosts) {
    let remote = LiveHostSpec {
        target: Target::parse_ssh(SSH_HOST, None).expect("devbox is a valid ssh host"),
        config_dir: daemon.config_dir(),
        tls: None,
        connects: true,
    };
    Harness::open_hosts(cx, home, vec![remote], Arc::new(MemoryTokens::default()))
}
