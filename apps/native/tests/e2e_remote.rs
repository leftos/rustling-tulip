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
use std::time::Duration;

use daemon_client::device_tokens::MemoryTokens;
use gpui::TestAppContext;
use protocol::{ClientMessage, DaemonMessage, RemoteState};
use rustling_tulip_native::target::Target;
use support::Harness;
use support::live::{LiveDaemon, REMOTE_HOST, free_port};

const CONNECT: Duration = Duration::from_secs(20);
/// Hang guard for the listener's first `whoami` and `cert`, each a run of
/// the node shim.
const SERVING: Duration = Duration::from_secs(60);
/// Hang guard for the tailnet dial: TLS, the shim's `whois`, then `Hello`.
const PAIRING: Duration = Duration::from_secs(60);

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
    host.wait_until("the loopback client to connect", CONNECT, |_| {
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
    host.wait_until("remote access to report Serving", SERVING, |_| {
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
    device.wait_until("the host to refuse the unpaired device", PAIRING, |_| {
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
