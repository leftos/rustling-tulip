//! End-to-end specs for remote access: the real daemon's tailnet listener,
//! built with the `e2e-loopback-peers` feature and run against
//! `tools/e2e/fake-tailscale`, with the native client dialling it over TLS
//! from this machine. They need that daemon build beside the test binary, so
//! they are ignored by default and run by `.\rt.ps1 native-e2e`.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use std::sync::Arc;

use daemon_client::device_tokens::{DeviceTokenStore as _, MemoryTokens};
use gpui::TestAppContext;
use protocol::{ClientMessage, DaemonMessage, InitLayoutKind, RemoteState};
use rustling_tulip_native::file_fetch::FetchRequest;
use rustling_tulip_native::target::{HostKey, Target};
use rustling_tulip_native::{Connection, NetCommand};
use support::live::{LiveClient, LiveDaemon, REMOTE_HOST, commit_all, free_port, spawn_shell};
use support::{Harness, Opened};

/// The line the listener logs when the owner rule refuses a peer
/// (`crates/daemon/src/tailnet_listener.rs`).
const OWNER_RULE_REFUSAL: &str = "tailnet listener: peer refused by the owner rule";
/// The line the daemon logs when a tailnet `Hello` without a valid token
/// puts the connection in pairing (`crates/daemon/src/server/pairing.rs`).
const PAIRING_ONLY: &str = "tailnet device without a valid token; pairing only";
/// The line the daemon logs when a blocked device asks to pair
/// (`crates/daemon/src/server/pairing.rs`).
const BLOCKED_PAIR_REFUSAL: &str = "a blocked device asked to pair; refused";
/// The line a served connection logs when the listener server that accepted
/// it ended, as it sends that connection its Close frame
/// (`crates/daemon/src/server/connection.rs`).
const SERVER_ENDED_CLOSE: &str =
    "recv_loop: the tailnet listener server that accepted this connection ended; closing";
/// The login the fake tailscale gives every node's user.
const FAKE_LOGIN: &str = "owner@example.com";

/// Remote access comes up for the fake tailnet, and an unpaired device that
/// dials it over TLS from loopback passes the owner rule, gets `Welcome` and
/// lands in pairing.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn remote_listener_comes_up_for_the_fake_tailnet(
    host_cx: &mut TestAppContext,
    device_cx: &mut TestAppContext,
) {
    let daemon = LiveDaemon::start_remote("remote-up");
    let (mut host, host_client) = Harness::open_live(host_cx, &daemon);
    let daemon_port = daemon.handshake().port;
    host.wait_until("the loopback client to connect", |_| {
        host_client
            .latest()
            .is_some_and(|(port, overlay)| port == Some(daemon_port) && overlay.is_none())
    });

    let remote_port = free_port();
    host_client.send(ClientMessage::ConfigureRemote {
        enabled: true,
        port: Some(remote_port),
    });
    let mut serving = None;
    host.wait_until("remote access to report Serving", |_| {
        serving = host_client.find_message(|msg| match msg {
            DaemonMessage::RemoteStatus {
                state: state @ RemoteState::Serving { .. },
                ..
            } => Some(state.clone()),
            _ => None,
        });
        serving.is_some()
    });
    let serving = serving.expect("the wait just found it");
    assert!(
        matches!(
            &serving,
            RemoteState::Serving { name, port, .. } if name == REMOTE_HOST && *port == remote_port
        ),
        "serving as the fake tailnet's host on the configured port: {serving:?}"
    );
    for file in ["localhost.crt", "localhost.key"] {
        let path = daemon.tailnet_dir().join(file);
        assert!(
            path.is_file(),
            "the certificate was written under the isolated tailnet dir: {}",
            path.display()
        );
    }

    let (mut device, device_client) = Harness::open_live_on(
        device_cx,
        &daemon,
        Target::tailnet(REMOTE_HOST, Some(remote_port)),
        Some(daemon.tailnet_tls()),
        Arc::new(MemoryTokens::default()),
    );
    device.wait_until("the host to refuse the unpaired device", |_| {
        device_client
            .find_message(|msg| {
                matches!(
                    msg,
                    DaemonMessage::DevicePairFailed { reason }
                        if reason == protocol::DEVICE_NOT_APPROVED
                )
                .then_some(())
            })
            .is_some()
    });
    // The network thread reports "not paired" only for a refusal that
    // followed the host's `Welcome`.
    let labels = device_client.footer_labels();
    assert_eq!(
        labels.last().map(String::as_str),
        Some("not paired"),
        "footer labels: {labels:?}"
    );
}

/// An unpaired device lands in pairing, asks, and the owner approves it from
/// the host's loopback client: the device redeems the code, stores its token
/// for the host and connects.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn unpaired_device_lands_in_pairing_and_is_approved_on_the_host(
    host_cx: &mut TestAppContext,
    device_cx: &mut TestAppContext,
) {
    let daemon = LiveDaemon::start_remote("remote-pair");
    let (mut host, host_client) = Harness::open_live(host_cx, &daemon);
    let port = serve_remote(&mut host, &host_client, &daemon);
    let tokens = Arc::new(MemoryTokens::default());
    let (mut device, device_client) = open_device(device_cx, &daemon, port, &tokens);

    wait_not_approved(&mut device, &device_client, 0);
    assert!(
        !device_client.states().iter().any(Connection::is_open),
        "the pairing connection never shows as open: {:?}",
        device_client.footer_labels()
    );
    assert_eq!(
        tokens.get(REMOTE_HOST).expect("the token store answers"),
        None,
        "no token before the owner approves"
    );

    pair(&mut host, &host_client, &mut device, &device_client);

    let token = tokens.get(REMOTE_HOST).expect("the token store answers");
    assert!(
        token.is_some_and(|token| !token.is_empty()),
        "the device token is stored for {REMOTE_HOST}: {tokens:?}"
    );
    assert!(
        device_client
            .find_message(|msg| matches!(msg, DaemonMessage::Welcome { .. }).then_some(()))
            .is_some(),
        "the connected device was welcomed"
    );
}

/// A paired device's token takes a new client straight to Connected, and the
/// tailnet connection drives a shell: spawn, type, see the echo.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn paired_device_reconnects_with_its_token_and_drives_a_shell(
    host_cx: &mut TestAppContext,
    device_cx: &mut TestAppContext,
    paired_cx: &mut TestAppContext,
) {
    let daemon = LiveDaemon::start_remote("remote-shell");
    let (mut host, host_client) = Harness::open_live(host_cx, &daemon);
    let port = serve_remote(&mut host, &host_client, &daemon);
    let tokens = Arc::new(MemoryTokens::default());
    let (mut device, device_client) = open_device(device_cx, &daemon, port, &tokens);
    wait_not_approved(&mut device, &device_client, 0);
    pair(&mut host, &host_client, &mut device, &device_client);
    let token = tokens
        .get(REMOTE_HOST)
        .expect("the token store answers")
        .expect("pairing stored a token");

    let paired_tokens = Arc::new(MemoryTokens::with(REMOTE_HOST, &token));
    let (mut h, client) = open_device(paired_cx, &daemon, port, &paired_tokens);
    wait_open(
        &mut h,
        &client,
        "the paired device to connect with its token",
    );
    assert!(
        client.find_message(not_approved).is_none(),
        "a paired device never lands in pairing"
    );
    assert!(
        !client
            .footer_labels()
            .iter()
            .any(|label| label == "not paired"),
        "footer labels: {:?}",
        client.footer_labels()
    );

    client.send(spawn_shell(daemon.dir()));
    let pane = place_first_session(&mut h, &client);
    h.wait_for_row(&pane, "the shell's prompt", |row| {
        row.trim_end().ends_with('>')
    });
    let marker = format!("rt-remote-marker-{}", std::process::id());
    h.type_into(&pane, &format!("echo {marker}\n"));
    h.wait_for_row(&pane, "the echoed marker on a line of its own", |row| {
        row.trim() == marker
    });
}

/// A peer the owner rule refuses is dropped before the TLS handshake: no
/// `Welcome`, no pairing, never Connected, and the listener logs why. The
/// owner-rule refusal is logged only by the listener's gate, which runs in
/// the TCP acceptor that `RustlsAcceptor` wraps, so before any TLS handshake
/// (`crates/daemon/src/tailnet_listener.rs`, `TailnetAcceptor::accept`).
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn non_owner_peer_is_dropped_before_the_handshake(
    host_cx: &mut TestAppContext,
    device_cx: &mut TestAppContext,
) {
    let daemon = LiveDaemon::start_remote("remote-non-owner");
    let (mut host, host_client) = Harness::open_live(host_cx, &daemon);
    let port = serve_remote(&mut host, &host_client, &daemon);
    daemon.make_peers_non_owners();
    let tokens = Arc::new(MemoryTokens::default());
    let (mut device, device_client) = open_device(device_cx, &daemon, port, &tokens);

    let mut refusal = None;
    device.wait_until(
        "the listener to refuse the peer and the dial to end",
        |_| {
            refusal = owner_rule_refusal(&daemon);
            refusal.is_some() && ended_attempts(&device_client.states()) >= 1
        },
    );
    let refusal = refusal.expect("the wait just found it");
    assert!(
        refusal.contains("WARN") && refusal.contains("peer=127.0.0.1:"),
        "the refusal names the loopback peer: {refusal}"
    );
    assert!(
        device_client
            .find_message(|msg| matches!(msg, DaemonMessage::Welcome { .. }).then_some(()))
            .is_none(),
        "a refused peer gets no Welcome"
    );
    assert!(
        device_client.find_message(not_approved).is_none(),
        "a refused peer never reaches pairing"
    );
    assert!(
        !daemon.daemon_log().contains(PAIRING_ONLY),
        "the daemon never served the refused peer a connection"
    );
    assert!(
        !device_client.states().iter().any(Connection::is_open),
        "a refused peer is never Connected: {:?}",
        device_client.footer_labels()
    );
}

/// A paired device cannot stop the host's daemon or turn its remote access
/// off: both are refused with the daemon's own text, and the daemon stays up
/// and Serving with the device still connected.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn shutdown_and_remote_off_are_refused_from_the_tailnet_client(
    host_cx: &mut TestAppContext,
    device_cx: &mut TestAppContext,
) {
    let daemon = LiveDaemon::start_remote("remote-host-only");
    let (mut host, host_client) = Harness::open_live(host_cx, &daemon);
    let port = serve_remote(&mut host, &host_client, &daemon);
    let tokens = Arc::new(MemoryTokens::default());
    let (mut device, device_client) = open_device(device_cx, &daemon, port, &tokens);
    wait_not_approved(&mut device, &device_client, 0);
    pair(&mut host, &host_client, &mut device, &device_client);

    let from = device_client.message_count();
    let host_from = host_client.message_count();
    device_client.send(ClientMessage::Shutdown { drain: false });
    device_client.send(ClientMessage::ConfigureRemote {
        enabled: false,
        port: Some(port),
    });
    device_client.send(ClientMessage::ListRepos);
    device.wait_until("the daemon to answer all three requests", |_| {
        device_client
            .find_message_from(from, |msg| {
                matches!(msg, DaemonMessage::Repos { .. }).then_some(())
            })
            .is_some()
    });
    for refusal in ["shutdown is local-only", "remote access is local-only"] {
        assert!(
            device_client
                .find_message_from(from, |msg| {
                    matches!(msg, DaemonMessage::Error { message, .. } if message == refusal)
                        .then_some(())
                })
                .is_some(),
            "the daemon refused with {refusal:?}"
        );
    }

    // A loopback round trip after the refusals, so a side effect of either
    // one has reached the host before the checks.
    let round_trip = host_client.message_count();
    host_client.send(ClientMessage::ListRepos);
    host.wait_until("the host's round trip after the refusals", |_| {
        host_client
            .find_message_from(round_trip, |msg| {
                matches!(msg, DaemonMessage::Repos { .. }).then_some(())
            })
            .is_some()
    });

    assert!(daemon.answers_health(), "the daemon is still up");
    assert!(
        device_client
            .states()
            .last()
            .is_some_and(Connection::is_open),
        "the device is still connected: {:?}",
        device_client.footer_labels()
    );
    let state = remote_state(&host_client);
    assert!(
        matches!(&state, Some(RemoteState::Serving { port: serving, .. }) if *serving == port),
        "remote access still serves: {state:?}"
    );
    let turned_off = host_client.find_message_from(host_from, |msg| {
        matches!(
            msg,
            DaemonMessage::RemoteStatus { enabled: false, .. }
                | DaemonMessage::RemoteStatus {
                    state: RemoteState::Off,
                    ..
                }
        )
        .then_some(())
    });
    assert_eq!(
        turned_off, None,
        "the host never heard remote access go off after the refusals"
    );
}

/// Blocking a connected device closes its connection; its redial with the
/// blocked token lands in pairing, and a new pairing request is refused.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn revoke_while_connected_closes_the_connection_and_the_next_hello_lands_in_pairing(
    host_cx: &mut TestAppContext,
    device_cx: &mut TestAppContext,
) {
    let daemon = LiveDaemon::start_remote("remote-revoke");
    let (mut host, host_client) = Harness::open_live(host_cx, &daemon);
    let port = serve_remote(&mut host, &host_client, &daemon);
    let tokens = Arc::new(MemoryTokens::default());
    let (mut device, device_client) = open_device(device_cx, &daemon, port, &tokens);
    wait_not_approved(&mut device, &device_client, 0);
    pair(&mut host, &host_client, &mut device, &device_client);

    host_client.send(ClientMessage::ListDevices);
    let mut device_id = None;
    host.wait_until("the paired device listed as connected", |_| {
        device_id = host_client.find_message(|msg| match msg {
            DaemonMessage::Devices { devices } => devices
                .iter()
                .find(|info| info.connected >= 1 && !info.revoked)
                .map(|info| info.device_id.clone()),
            _ => None,
        });
        device_id.is_some()
    });
    let device_id = device_id.expect("the wait just found it");

    let token = tokens.get(REMOTE_HOST).expect("the token store answers");
    assert!(
        token.is_some(),
        "the device holds its token before the block"
    );
    let states_from = device_client.states().len();
    let from = device_client.message_count();
    let host_from = host_client.message_count();
    host_client.send(ClientMessage::RevokeDevice {
        device_id: device_id.clone(),
    });
    device.wait_until("the revoked device's redial to land in pairing", |_| {
        device_client
            .find_message_from(from, not_approved)
            .is_some()
    });
    let labels = device_client.footer_labels()[states_from..].to_vec();
    assert_eq!(
        labels.first().map(String::as_str),
        Some("disconnected"),
        "the block closed the connection first: {labels:?}"
    );
    assert_eq!(
        labels.last().map(String::as_str),
        Some("not paired"),
        "the redial landed in pairing: {labels:?}"
    );
    assert!(
        !device_client.states()[states_from..]
            .iter()
            .any(Connection::is_open),
        "the blocked device never reconnected: {labels:?}"
    );
    // The network thread dials with the stored token and never clears it,
    // so the redial that landed in pairing presented the blocked token.
    assert_eq!(
        tokens.get(REMOTE_HOST).expect("the token store answers"),
        token,
        "the redial carried the blocked token"
    );
    let mut blocked = None;
    host.wait_until("the host to list the device as blocked", |_| {
        blocked = host_client.find_message_from(host_from, |msg| match msg {
            DaemonMessage::Devices { devices } => devices
                .iter()
                .find(|info| info.device_id == device_id)
                .map(|info| info.revoked),
            _ => None,
        });
        blocked.is_some()
    });
    assert_eq!(blocked, Some(true), "the host lists the device as blocked");

    let from = device_client.message_count();
    device_client.command(NetCommand::PairRequest);
    device.wait_until("the host to refuse the blocked device's request", |_| {
        device_client
            .find_message_from(from, not_approved)
            .is_some()
            && daemon.daemon_log().contains(BLOCKED_PAIR_REFUSAL)
    });
    assert!(
        device_client
            .find_message_from(from, |msg| {
                matches!(msg, DaemonMessage::DevicePairPending { .. }).then_some(())
            })
            .is_none(),
        "a blocked device's request is never held for approval"
    );
}

/// A page fetched over the tailnet brings the media it names beside it: both
/// land under the host's and repo's download folder, and the page opens.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn remote_html_fetch_brings_its_relative_media(
    host_cx: &mut TestAppContext,
    device_cx: &mut TestAppContext,
) {
    const PAGE: &[u8] = b"<!doctype html><html><body><img src=\"img/dot.png\"></body></html>\n";
    const DOT: &[u8] = b"\x89PNG\r\n\x1a\nrt-e2e dot";
    let daemon = LiveDaemon::start_remote("remote-media");
    let repo = daemon.git_fixture();
    std::fs::create_dir_all(repo.join("img")).expect("create the fixture's img dir");
    std::fs::write(repo.join("page.html"), PAGE).expect("write the page");
    std::fs::write(repo.join("img").join("dot.png"), DOT).expect("write the image");
    commit_all(&repo, "a page with an image");
    // Text files open externally, so the page's open is the recorded one.
    std::fs::write(
        daemon.dir().join("ui").join("native-ui.json"),
        serde_json::json!({ "general": { "open_text_externally": true } }).to_string(),
    )
    .expect("seed native-ui.json");

    let (mut host, host_client) = Harness::open_live(host_cx, &daemon);
    let port = serve_remote(&mut host, &host_client, &daemon);
    host_client.send(ClientMessage::AddRepo {
        path: repo.to_string_lossy().into_owned(),
        name: None,
    });
    let mut repo_id = None;
    host.wait_until("the fixture repo in state.json", |_| {
        repo_id = daemon.repo_id(&repo);
        repo_id.is_some()
    });
    let repo_name = repo
        .file_name()
        .expect("the fixture dir has a name")
        .to_string_lossy()
        .into_owned();

    let tokens = Arc::new(MemoryTokens::default());
    let (mut device, device_client) = open_device(device_cx, &daemon, port, &tokens);
    wait_not_approved(&mut device, &device_client, 0);
    pair(&mut host, &host_client, &mut device, &device_client);
    let root = daemon.dir().join("remote-files");
    let set = root.clone();
    device.root_update(move |view, _, _| view.set_remote_files_root(set));

    let request = FetchRequest {
        host: HostKey::of(&Target::tailnet(REMOTE_HOST, None)),
        repo_id: repo_id.expect("the wait just found it"),
        repo_name: repo_name.clone(),
        worktree_path: None,
        rel_path: "page.html".to_owned(),
        line: None,
        column: None,
        open_when_done: true,
    };
    device
        .root_update(move |view, _, cx| view.fetch_remote_file(request, cx))
        .expect("a fetch on the tailnet host starts");
    device.wait_until("the page to open once its media landed", |h| {
        !h.opened().is_empty()
    });

    let landed = root.join(REMOTE_HOST).join(&repo_name);
    let page = landed.join("page.html");
    assert_eq!(device.opened(), [Opened::DefaultApp(page.clone())]);
    assert_eq!(
        std::fs::read(&page).expect("the page landed"),
        PAGE,
        "{}",
        page.display()
    );
    let dot = landed.join("img").join("dot.png");
    assert_eq!(
        std::fs::read(&dot).expect("the page's image landed beside it"),
        DOT,
        "{}",
        dot.display()
    );
}

/// Turning remote access off closes the served connection, and the device's
/// redials fail while it stays off. The Close frame is proven by the daemon's
/// close-path log line ([`SERVER_ENDED_CLOSE`]), written as it sends it: the
/// client cannot tell that close from a dropped socket.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn turning_remote_access_off_closes_served_connections(
    host_cx: &mut TestAppContext,
    device_cx: &mut TestAppContext,
) {
    let daemon = LiveDaemon::start_remote("remote-off");
    let (mut host, host_client) = Harness::open_live(host_cx, &daemon);
    let port = serve_remote(&mut host, &host_client, &daemon);
    let tokens = Arc::new(MemoryTokens::default());
    let (mut device, device_client) = open_device(device_cx, &daemon, port, &tokens);
    wait_not_approved(&mut device, &device_client, 0);
    pair(&mut host, &host_client, &mut device, &device_client);

    let states_from = device_client.states().len();
    host_client.send(ClientMessage::ConfigureRemote {
        enabled: false,
        port: Some(port),
    });
    host.wait_until("remote access to report Off", |_| {
        matches!(remote_state(&host_client), Some(RemoteState::Off))
    });
    // The close, then at least one redial that ended without opening, and
    // the daemon's record of closing the served connection.
    device.wait_until("the device to fail a redial", |_| {
        ended_attempts(&device_client.states()[states_from..]) >= 2
            && daemon.daemon_log().contains(SERVER_ENDED_CLOSE)
    });

    let after = device_client.states()[states_from..].to_vec();
    let labels = device_client.footer_labels()[states_from..].to_vec();
    // The daemon drops TLS without `close_notify` after the closing
    // handshake, so the network thread reports the Close frame's end as a
    // read error; only the daemon's log tells it from a dropped socket.
    assert_eq!(
        labels.first().map(String::as_str),
        Some("disconnected"),
        "the served connection was closed first: {labels:?}"
    );
    assert!(
        !after.iter().any(Connection::is_open),
        "the device never reconnects while remote access is off: {labels:?}"
    );
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

/// A tailnet client of `daemon`'s remote access on `port`, trusting its test
/// CA and holding device tokens in `tokens`.
fn open_device<'a>(
    cx: &'a mut TestAppContext,
    daemon: &LiveDaemon,
    port: u16,
    tokens: &Arc<MemoryTokens>,
) -> (Harness<'a>, LiveClient) {
    Harness::open_live_on(
        cx,
        daemon,
        Target::tailnet(REMOTE_HOST, Some(port)),
        Some(daemon.tailnet_tls()),
        Arc::clone(tokens),
    )
}

/// The newest remote access state the daemon sent `client`.
fn remote_state(client: &LiveClient) -> Option<RemoteState> {
    client.find_message(|msg| match msg {
        DaemonMessage::RemoteStatus { state, .. } => Some(state.clone()),
        _ => None,
    })
}

/// Answers a `DevicePairFailed` "device not approved".
fn not_approved(msg: &DaemonMessage) -> Option<()> {
    matches!(
        msg,
        DaemonMessage::DevicePairFailed { reason } if reason == protocol::DEVICE_NOT_APPROVED
    )
    .then_some(())
}

/// Waits for a "device not approved" among the messages from index `from` on,
/// with the view showing the device as not paired.
fn wait_not_approved(h: &mut Harness<'_>, client: &LiveClient, from: usize) {
    h.wait_until("the host to land the device in pairing", |_| {
        client.find_message_from(from, not_approved).is_some()
            && client
                .footer_labels()
                .last()
                .is_some_and(|label| label == "not paired")
    });
}

/// Waits until the view's latest state is open.
fn wait_open(h: &mut Harness<'_>, client: &LiveClient, what: &str) {
    h.wait_until(what, |_| {
        client.states().last().is_some_and(Connection::is_open)
    });
}

/// How many of `states` end a dial without it opening.
fn ended_attempts(states: &[Connection]) -> usize {
    states
        .iter()
        .filter(|conn| matches!(conn.footer(0).label.as_str(), "disconnected" | "error"))
        .count()
}

/// The listener's owner-rule refusal line in `daemon`'s log, once written.
fn owner_rule_refusal(daemon: &LiveDaemon) -> Option<String> {
    daemon
        .daemon_log()
        .lines()
        .find(|line| line.contains(OWNER_RULE_REFUSAL))
        .map(str::to_owned)
}

/// Pairs `device`, on its pairing connection, through the owner's loopback
/// `host`: the device asks, the host approves its waiting request, and the
/// device redeems the code and reconnects with its new token. Returns once
/// the device is connected.
fn pair(
    host: &mut Harness<'_>,
    host_client: &LiveClient,
    device: &mut Harness<'_>,
    device_client: &LiveClient,
) {
    let states_from = device_client.states().len();
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
    let labels = device_client.footer_labels()[states_from..].to_vec();
    assert!(
        labels.is_empty(),
        "the request went out on the pairing connection, with no redial: {labels:?}"
    );

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
    wait_open(
        device,
        device_client,
        "the paired device to connect with its token",
    );
}

/// Waits for a session in the sidebar, has the daemon lay out every session
/// for this client, and returns the pane it lands in.
fn place_first_session(h: &mut Harness<'_>, client: &LiveClient) -> String {
    h.wait_until("a session in the sidebar", |h| {
        h.root(|root, _| {
            root.sidebar_containers()
                .iter()
                .any(|c| !c.leaves.is_empty())
        })
    });
    client.send(ClientMessage::InitLayout {
        kind: InitLayoutKind::AllSessions,
    });
    h.wait_until("a pane for the session", |h| h.first_pane().is_some());
    h.first_pane().expect("a pane was just laid out")
}
