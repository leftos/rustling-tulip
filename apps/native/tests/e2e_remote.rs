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

use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::process::Command;
use std::sync::Arc;

use daemon_client::device_tokens::{DeviceTokenStore as _, MemoryTokens};
use gpui::TestAppContext;
use protocol::{ClientMessage, DaemonMessage, RemoteState};
use rustling_tulip_native::LinkAction;
use rustling_tulip_native::file_fetch::FetchRequest;
use rustling_tulip_native::target::{HostKey, Target};
use rustling_tulip_native::{Connection, NetCommand};
use sha2::{Digest as _, Sha256};
use support::live::{
    LiveClient, LiveDaemon, LiveHostSpec, REMOTE_HOST, commit_all, free_port, spawn_shell,
};
use support::update_feed::UpdateFeed;
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
/// The daemon's refusals of a remote update on a host that has no installed
/// copy, or whose daemon does not run from it (`NO_INSTALL` and
/// `NOT_OWN_INSTALL` in `crates/daemon/src/server/update.rs`). A live daemon
/// runs from `target/test-release`, so it answers one of them; which one
/// depends on whether the machine running the spec has the app installed.
const NO_INSTALL: &str = "No installed copy of rustling-tulip was found on this host.";
const NOT_OWN_INSTALL: &str =
    "This daemon doesn't run from the installed copy, so it can't update it.";
/// The release the fake update feed offers, and its installer's name.
const FEED_VERSION: &str = "9.9.9-alpha";
const FEED_INSTALLER: &str = "rustling-tulip-9.9.9-alpha-setup.exe";

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

    let home = LiveDaemon::start("remote-up-home");
    let tokens = Arc::new(MemoryTokens::default());
    let (mut device, device_client) = open_device(device_cx, &home, &daemon, remote_port, &tokens);
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
    let home = LiveDaemon::start("remote-pair-home");
    let (mut host, host_client) = Harness::open_live(host_cx, &daemon);
    let port = serve_remote(&mut host, &host_client, &daemon);
    let tokens = Arc::new(MemoryTokens::default());
    let (mut device, device_client) = open_device(device_cx, &home, &daemon, port, &tokens);

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
    let home = LiveDaemon::start("remote-shell-home");
    let paired_home = LiveDaemon::start("remote-shell-paired-home");
    let (mut host, host_client) = Harness::open_live(host_cx, &daemon);
    let port = serve_remote(&mut host, &host_client, &daemon);
    let tokens = Arc::new(MemoryTokens::default());
    let (mut device, device_client) = open_device(device_cx, &home, &daemon, port, &tokens);
    wait_not_approved(&mut device, &device_client, 0);
    pair(&mut host, &host_client, &mut device, &device_client);
    let token = tokens
        .get(REMOTE_HOST)
        .expect("the token store answers")
        .expect("pairing stored a token");

    let paired_tokens = Arc::new(MemoryTokens::with(REMOTE_HOST, &token));
    let (mut h, client) = open_device(paired_cx, &paired_home, &daemon, port, &paired_tokens);
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
    let pane = place_remote_session(&mut h);
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
    let home = LiveDaemon::start("remote-non-owner-home");
    let (mut host, host_client) = Harness::open_live(host_cx, &daemon);
    let port = serve_remote(&mut host, &host_client, &daemon);
    daemon.make_peers_non_owners();
    let tokens = Arc::new(MemoryTokens::default());
    let (mut device, device_client) = open_device(device_cx, &home, &daemon, port, &tokens);

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
    let home = LiveDaemon::start("remote-host-only-home");
    let (mut host, host_client) = Harness::open_live(host_cx, &daemon);
    let port = serve_remote(&mut host, &host_client, &daemon);
    let tokens = Arc::new(MemoryTokens::default());
    let (mut device, device_client) = open_device(device_cx, &home, &daemon, port, &tokens);
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

/// The fake update feed serves its release list, the installer's `.sha256`
/// and the installer, records each path in order, and answers anything else
/// with a 404.
#[test]
fn the_fake_update_feed_serves_its_release_and_installer() {
    let feed = UpdateFeed::start(FEED_VERSION, b"fake installer");
    let base = feed.base_url();
    let list = "/repos/leftos/rustling-tulip/releases?per_page=100";
    let sha = format!("/dl/{FEED_INSTALLER}.sha256");
    let installer = format!("/dl/{FEED_INSTALLER}");

    let (status, body) = http_get(&base, list);
    assert!(status.contains(" 200 "), "the release list: {status}");
    let releases: serde_json::Value =
        serde_json::from_slice(&body).expect("the release list is JSON");
    assert_eq!(releases[0]["tag_name"], "v9.9.9-alpha", "{releases}");
    assert_eq!(
        releases[0]["assets"].as_array().map(Vec::len),
        Some(2),
        "the release offers the installer and its .sha256: {releases}"
    );

    let hex = Sha256::digest(b"fake installer")
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    let (status, body) = http_get(&base, &sha);
    assert!(status.contains(" 200 "), "the .sha256: {status}");
    assert_eq!(
        String::from_utf8_lossy(&body),
        format!("{hex}  {FEED_INSTALLER}\n"),
        "the .sha256 names the installer's hash and file"
    );

    let (status, body) = http_get(&base, &installer);
    assert!(status.contains(" 200 "), "the installer: {status}");
    assert_eq!(body, b"fake installer", "the installer's bytes");
    assert_eq!(
        feed.requests(),
        [list.to_owned(), sha, installer],
        "the feed recorded each path in order"
    );

    let (status, _) = http_get(&base, "/nope");
    assert!(status.contains(" 404 "), "an unknown path: {status}");
}

/// A paired tailnet device's remote update is refused by a live daemon,
/// which runs from no install of its own, before it downloads anything;
/// its request to update the connected clients is refused as local-only, and
/// the daemon stays up with the device connected.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn a_remote_update_from_the_tailnet_is_refused_by_a_daemon_with_no_install_of_its_own(
    host_cx: &mut TestAppContext,
    device_cx: &mut TestAppContext,
) {
    let feed = UpdateFeed::start(FEED_VERSION, b"fake installer");
    let daemon = LiveDaemon::start_remote_with_env(
        "remote-update-refused",
        &[("RUSTLING_TULIP_UPDATE_FEED", feed.base_url().into())],
    );
    let home = LiveDaemon::start("remote-update-refused-home");
    let (mut host, host_client) = Harness::open_live(host_cx, &daemon);
    let port = serve_remote(&mut host, &host_client, &daemon);
    let tokens = Arc::new(MemoryTokens::default());
    let (mut device, device_client) = open_device(device_cx, &home, &daemon, port, &tokens);
    wait_not_approved(&mut device, &device_client, 0);
    pair(&mut host, &host_client, &mut device, &device_client);

    let from = device_client.message_count();
    device_client.send(ClientMessage::RemoteUpdate {
        request_id: "ru-1".to_owned(),
    });
    device_client.send(ClientMessage::UpdateConnectedClients {
        request_id: Some("ucc-1".to_owned()),
    });
    let local_only = |msg: &DaemonMessage| {
        matches!(
            msg,
            DaemonMessage::Error { message, .. } if message == "updating connected clients is local-only"
        )
        .then_some(())
    };
    let mut refusal = None;
    device.wait_until("the host to answer both requests", |_| {
        refusal = device_client.find_message_from(from, |msg| update_refusal(msg, "ru-1"));
        refusal.is_some() && device_client.find_message_from(from, local_only).is_some()
    });
    let (code, message) = refusal.expect("the wait just found it");
    assert_no_install_refusal(code.as_deref(), &message);
    assert_eq!(
        feed.requests(),
        Vec::<String>::new(),
        "the refusal came before any download"
    );

    assert!(daemon.answers_health(), "the daemon is still up");
    assert!(
        device_client
            .states()
            .last()
            .is_some_and(Connection::is_open),
        "the device is still connected: {:?}",
        device_client.footer_labels()
    );
}

/// The code and message of the `Error` answering the request `request_id`.
fn update_refusal(msg: &DaemonMessage, request_id: &str) -> Option<(Option<String>, String)> {
    match msg {
        DaemonMessage::Error {
            request_id: Some(id),
            message,
            code,
        } if id == request_id => Some((code.clone(), message.clone())),
        _ => None,
    }
}

/// Asserts a remote update was refused as needing an install of the daemon's
/// own: code `update_refused` and one of the two texts for it.
fn assert_no_install_refusal(code: Option<&str>, message: &str) {
    assert_eq!(
        code,
        Some(protocol::error_codes::UPDATE_REFUSED),
        "the refusal's code ({message:?})"
    );
    assert!(
        [NO_INSTALL, NOT_OWN_INSTALL].contains(&message),
        "the refusal is for a daemon with no install of its own: {message:?}"
    );
}

/// The status line and body of `GET path` from the server at `base`
/// (`http://host:port`), over one `Connection: close` request.
fn http_get(base: &str, path: &str) -> (String, Vec<u8>) {
    let authority = base.trim_start_matches("http://");
    let mut stream = TcpStream::connect(authority).expect("connect to the feed");
    let request = format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("send the request");
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).expect("read the answer");
    let end = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("the answer has a head");
    let head = String::from_utf8_lossy(&bytes[..end]);
    let status = head.lines().next().unwrap_or_default().to_owned();
    (status, bytes[end + 4..].to_vec())
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
    let home = LiveDaemon::start("remote-revoke-home");
    let (mut host, host_client) = Harness::open_live(host_cx, &daemon);
    let port = serve_remote(&mut host, &host_client, &daemon);
    let tokens = Arc::new(MemoryTokens::default());
    let (mut device, device_client) = open_device(device_cx, &home, &daemon, port, &tokens);
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
    let home = LiveDaemon::start("remote-media-home");
    let repo = daemon.git_fixture();
    std::fs::create_dir_all(repo.join("img")).expect("create the fixture's img dir");
    std::fs::write(repo.join("page.html"), PAGE).expect("write the page");
    std::fs::write(repo.join("img").join("dot.png"), DOT).expect("write the image");
    commit_all(&repo, "a page with an image");

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
    let (mut device, device_client) = open_device(device_cx, &home, &daemon, port, &tokens);
    wait_not_approved(&mut device, &device_client, 0);
    pair(&mut host, &host_client, &mut device, &device_client);
    let root = home.dir().join("remote-files");
    let set = root.clone();
    device.root_update(move |view, _, _| view.set_remote_files_root(set));

    let request = FetchRequest {
        host: remote_key(),
        repo_id: repo_id.expect("the wait just found it"),
        repo_name: repo_name.clone(),
        worktree_path: None,
        rel_path: "page.html".to_owned(),
        line: None,
        column: None,
        open_when_done: true,
        action: LinkAction::DefaultApp,
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

/// `len` bytes no compressor shrinks, the same on every run.
fn incompressible(len: usize) -> Vec<u8> {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect()
}

/// The files and folders directly in `dir`, by name, sorted.
fn names_in(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read the folder")
        .map(|entry| {
            entry
                .expect("read a folder entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// A folder fetched over the tailnet lands byte-exact under the host's and
/// repo's download folder, its chunks spanning a large file, without the
/// folders the host skips and without its stage.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn remote_folder_fetch_lands_the_tree_byte_exact(
    host_cx: &mut TestAppContext,
    device_cx: &mut TestAppContext,
) {
    const A: &[u8] = b"rt-e2e folder file a\n";
    let big = incompressible(3 * 256 * 1024 + 12_345);
    let daemon = LiveDaemon::start_remote("remote-folder");
    let home = LiveDaemon::start("remote-folder-home");
    let repo = daemon.git_fixture();
    let docs = repo.join("docs");
    std::fs::create_dir_all(docs.join("sub")).expect("create the fixture's docs/sub dir");
    std::fs::create_dir_all(docs.join("node_modules")).expect("create docs/node_modules");
    std::fs::write(docs.join("a.txt"), A).expect("write a.txt");
    std::fs::write(docs.join("sub").join("b.bin"), &big).expect("write b.bin");
    std::fs::write(docs.join("node_modules").join("x.js"), b"skipped\n").expect("write x.js");
    commit_all(&repo, "a folder to fetch");

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
    let (mut device, device_client) = open_device(device_cx, &home, &daemon, port, &tokens);
    wait_not_approved(&mut device, &device_client, 0);
    pair(&mut host, &host_client, &mut device, &device_client);
    let root = home.dir().join("remote-files");
    let set = root.clone();
    device.root_update(move |view, _, _| view.set_remote_files_root(set));

    let request = FetchRequest {
        host: remote_key(),
        repo_id: repo_id.expect("the wait just found it"),
        repo_name: repo_name.clone(),
        worktree_path: None,
        rel_path: "docs".to_owned(),
        line: None,
        column: None,
        open_when_done: false,
        action: LinkAction::DefaultApp,
    };
    device
        .root_update(move |view, _, cx| view.fetch_remote_folder(request, cx))
        .expect("a folder fetch on the tailnet host starts");
    device.wait_until("the folder's done toast", |h| {
        h.root(|root, _| {
            root.toasts()
                .iter()
                .any(|toast| toast.title == "Fetched folder docs")
        })
    });

    let landed = root.join(REMOTE_HOST).join(&repo_name);
    assert_eq!(names_in(&landed), ["docs"], "no stage is left beside it");
    let folder = landed.join("docs");
    assert_eq!(
        names_in(&folder),
        ["a.txt", "sub"],
        "node_modules is skipped"
    );
    assert_eq!(
        std::fs::read(folder.join("a.txt")).expect("a.txt landed"),
        A
    );
    assert!(
        std::fs::read(folder.join("sub").join("b.bin")).expect("b.bin landed") == big,
        "b.bin landed byte-exact"
    );
    assert!(device.opened().is_empty(), "a fetched folder does not open");
}

/// A Ctrl+clicked path in a worktree `git worktree add` cut beside the
/// session's repo is fetched from the host's own list of the session's
/// folders, and lands byte-exact under the host's and repo's download
/// folder.
#[gpui::test]
#[ignore = "e2e: run via .\\rt.ps1 native-e2e"]
fn remote_fetch_from_a_sibling_worktree_lands_the_file(
    host_cx: &mut TestAppContext,
    device_cx: &mut TestAppContext,
) {
    const FILE: &[u8] = b"rt-e2e sibling worktree note\n";
    let daemon = LiveDaemon::start_remote("remote-sibling");
    let home = LiveDaemon::start("remote-sibling-home");
    let repo = daemon.git_fixture();
    let name = repo
        .file_name()
        .expect("the fixture dir has a name")
        .to_string_lossy()
        .into_owned();
    let sibling = repo
        .parent()
        .expect("the fixture has a parent")
        .join(format!("{name}.wt"))
        .join("s")
        .join(&name);
    std::fs::create_dir_all(&sibling).expect("create the sibling worktree folder");
    let added = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "add", "-b", "sibling"])
        .arg(&sibling)
        .status()
        .expect("run git (is it on PATH?)");
    assert!(added.success(), "git worktree add failed: {added}");
    let note = sibling.join("note.txt");
    std::fs::write(&note, FILE).expect("write the worktree's note");

    let (mut host, host_client) = Harness::open_live(host_cx, &daemon);
    let port = serve_remote(&mut host, &host_client, &daemon);
    host_client.send(ClientMessage::AddRepo {
        path: repo.to_string_lossy().into_owned(),
        name: None,
    });
    host.wait_until("the fixture repo in state.json", |_| {
        daemon.repo_id(&repo).is_some()
    });

    let tokens = Arc::new(MemoryTokens::default());
    let (mut device, device_client) = open_device(device_cx, &home, &daemon, port, &tokens);
    wait_not_approved(&mut device, &device_client, 0);
    pair(&mut host, &host_client, &mut device, &device_client);
    let root = home.dir().join("remote-files");
    let set = root.clone();
    device.root_update(move |view, _, _| view.set_remote_files_root(set));

    device_client.send(spawn_shell(&repo));
    let pane = place_remote_session(&mut device);
    device.wait_for_row(&pane, "the shell's prompt", |row| {
        row.trim_end().ends_with('>')
    });

    let shown = note.to_string_lossy().replace('\\', "/");
    device.type_into(&pane, &format!("echo {shown}\n"));
    let (col, row) = text_row(&mut device, &pane, &shown);
    let at = device.cell_center(&pane, col + 2, row);
    device.pick_link(at);

    let landed = root.join(REMOTE_HOST).join(&name).join("note.txt");
    device.wait_until("the sibling worktree's note to land", |_| landed.exists());
    assert_eq!(
        std::fs::read(&landed).expect("the note landed"),
        FILE,
        "{}",
        landed.display()
    );
}

/// Waits for the first row of `pane` holding `text` and returns the column
/// its first character sits in, and the row's index.
fn text_row(h: &mut Harness<'_>, pane: &str, text: &str) -> (usize, usize) {
    let mut found = None;
    h.wait_until("the text on a row of the pane", |h| {
        found = h.root(|root, app| {
            root.pane_grid_text(pane, app).and_then(|rows| {
                rows.iter().enumerate().find_map(|(row, line)| {
                    line.find(text).map(|at| (line[..at].chars().count(), row))
                })
            })
        });
        found.is_some()
    });
    found.expect("the wait just found it")
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
    let home = LiveDaemon::start("remote-off-home");
    let (mut host, host_client) = Harness::open_live(host_cx, &daemon);
    let port = serve_remote(&mut host, &host_client, &daemon);
    let tokens = Arc::new(MemoryTokens::default());
    let (mut device, device_client) = open_device(device_cx, &home, &daemon, port, &tokens);
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

/// Waits for a session of the tailnet host in the sidebar, clicks its leaf
/// and returns the pane it lands in. The window dials [`REMOTE_HOST`]
/// without saving it, so the host has no header and no `+ Shell` button.
fn place_remote_session(h: &mut Harness<'_>) -> String {
    let mut session = None;
    h.wait_until("the tailnet host's session in the sidebar", |h| {
        session = h.root(|root, _| {
            root.sidebar_containers()
                .iter()
                .filter(|container| container.host == remote_key())
                .find_map(|container| container.leaves.first().map(|leaf| leaf.id.clone()))
        });
        session.is_some()
    });
    let session = session.expect("the wait just found it");
    let host = remote_key();
    let wire = host.wire().expect("the tailnet host is remote");
    let leaf = format!("leaf-{wire}::{session}");
    h.wait_until(&format!("the {leaf} row"), |h| {
        h.bounds(&leaf).size.height > gpui::px(0.0)
    });
    h.click_on(&leaf);
    let mut placed = None;
    h.wait_until("a pane holding the clicked session", |h| {
        placed = h.first_pane().and_then(|pane| {
            let held = h.root(|root, _| root.pane_session(&pane));
            (held.as_deref() == Some(session.as_str())).then_some(pane)
        });
        placed.is_some()
    });
    placed.expect("the wait just found it")
}

/// The host key of the tailnet host [`REMOTE_HOST`], whatever its port.
fn remote_key() -> HostKey {
    HostKey::of(&Target::tailnet(REMOTE_HOST, None))
}

/// A device window whose This PC is `home` and whose one saved host is
/// `daemon`'s remote access on `port`, trusting its test CA and holding
/// device tokens in `tokens`; returns the client of that tailnet host. A
/// device with a token for [`REMOTE_HOST`] is expected to connect, so the
/// window is returned once that host has its layout too.
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
