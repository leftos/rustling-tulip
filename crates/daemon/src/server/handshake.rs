//! The WebSocket handshake: reading and authorising `Hello`, negotiating the protocol version, `Welcome`, and the initial state push.
use super::Hub;
use super::connection::{AuthKind, Inbound};
use super::live_devices::LiveConnection;
use crate::devices::DeviceId;
use anyhow::{Context as _, anyhow};
use axum::extract::ws::Message;
use futures::StreamExt as _;
use protocol::{
    ClientMessage, ClonableLayout, DaemonMessage, InboundClientMessage, SUPPORTED_PROTOCOL_VERSIONS,
};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Outcome of a successful handshake: the negotiated protocol version plus the
/// client's self-reported identity (used to key its per-client tab layout).
pub(super) struct Handshake {
    #[expect(dead_code, reason = "kept for symmetry/logging; not yet read")]
    negotiated_version: u32,
    /// `None` when the client didn't send a `client_id` (older builds); the
    /// connection then falls back to the shared legacy layout.
    pub(super) client_id: Option<String>,
    pub(super) client_name: Option<String>,
    /// The tailnet device presented no valid token: the connection may only
    /// pair, and gets no state.
    pub(super) pairing: bool,
    /// The approved device a served tailnet connection is; `None` on
    /// loopback and for a pairing connection.
    pub(super) device: Option<DeviceId>,
    /// That device's registration in [`Hub::live_devices`], made under the
    /// same lock as its token's verify; the connection holds it until it
    /// ends.
    pub(super) live: Option<LiveConnection>,
}

/// What a `Hello`'s token earns on its listener.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum HelloAuth {
    /// The daemon's own token on loopback: the connection is served.
    Local,
    /// An approved device's token on tailnet: the connection is served as
    /// that device, registered live from the verify on.
    Device(LiveConnection),
    /// A tailnet device without a valid token: the connection may only pair.
    Pairing,
    /// Refused with this reason, which never echoes the presented value.
    Refused(&'static str),
}

pub const TAILNET_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Read and check the mandatory `Hello`. A tailnet connection gets
/// [`Hub::tailnet_handshake_timeout`] to send it; the loopback listener keeps
/// an un-timed handshake. `Ok(None)` when the connection's `life` is
/// cancelled first: the listener server that accepted it ended.
pub(super) async fn handshake<R: Inbound>(
    hub: &Hub,
    auth: &AuthKind,
    life: &CancellationToken,
    receiver: &mut R,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) -> anyhow::Result<Option<Handshake>> {
    let hello = async {
        match auth {
            AuthKind::Local => read_hello(hub, auth, life, receiver, out_tx).await,
            AuthKind::Tailnet(_) => tokio::time::timeout(
                hub.tailnet_handshake_timeout,
                read_hello(hub, auth, life, receiver, out_tx),
            )
            .await
            .map_err(|_| anyhow!("tailnet handshake timed out before Hello"))?,
        }
    };
    tokio::select! {
        biased;
        () = life.cancelled() => Ok(None),
        hello = hello => hello.map(Some),
    }
}

async fn read_hello<R: Inbound>(
    hub: &Hub,
    auth: &AuthKind,
    life: &CancellationToken,
    receiver: &mut R,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) -> anyhow::Result<Handshake> {
    let Some(Ok(msg)) = receiver.next().await else {
        return Err(anyhow!("client closed before handshake"));
    };
    let text = match msg {
        Message::Text(t) => t.to_string(),
        _ => return Err(anyhow!("first frame must be text Hello")),
    };
    let parsed = InboundClientMessage::from_json_str(&text).context("parsing handshake message")?;
    let InboundClientMessage::Known(ClientMessage::Hello {
        protocol_version,
        protocol_versions,
        auth_token,
        client_id,
        client_name,
    }) = parsed
    else {
        return Err(anyhow!("first message must be Hello"));
    };
    let (pairing, live) = match check_hello_auth(hub, auth, &auth_token, life) {
        HelloAuth::Local => (false, None),
        HelloAuth::Device(live) => (false, Some(live)),
        HelloAuth::Pairing => (true, None),
        HelloAuth::Refused(reason) => return Err(anyhow!(reason)),
    };
    let negotiated =
        negotiate_protocol_version(protocol_version, &protocol_versions).ok_or_else(|| {
            anyhow!(
                "no compatible protocol version: client advertises scalar={protocol_version} \
                 + range={protocol_versions:?}, daemon supports {SUPPORTED_PROTOCOL_VERSIONS:?}"
            )
        })?;
    info!(
        negotiated,
        scalar = protocol_version,
        ?protocol_versions,
        client_id = client_id.as_deref().unwrap_or("(legacy)"),
        "handshake negotiated protocol version"
    );
    let _ = out_tx.send(welcome_message(negotiated, &hub.daemon_id));
    Ok(Handshake {
        negotiated_version: negotiated,
        client_id,
        client_name,
        pairing,
        device: live.as_ref().map(|live| live.device_id().to_owned()),
        live,
    })
}

/// Decide what the value in `Hello.auth_token` earns on this listener. This
/// is the single allow/deny point for a tailnet device token.
///
/// Loopback takes only the daemon's own token. A tailnet connection never
/// compares against that token: an approved, unrevoked device's token bound to
/// this node is served, and anything else (no token, an unknown one, another
/// node's, a revoked one, the daemon's, or a store that could not be read)
/// may only pair. A served device's connection is registered under `life`.
fn check_hello_auth(
    hub: &Hub,
    auth: &AuthKind,
    presented: &str,
    life: &CancellationToken,
) -> HelloAuth {
    match auth {
        AuthKind::Local => {
            if crate::secret::constant_time_eq(presented.as_bytes(), hub.auth_token.as_bytes()) {
                HelloAuth::Local
            } else {
                HelloAuth::Refused("invalid auth token")
            }
        }
        AuthKind::Tailnet(peer) => match authorize_device(hub, peer, presented, life) {
            Some(live) => HelloAuth::Device(live),
            None => HelloAuth::Pairing,
        },
    }
}

/// Verify `peer`'s device token and, when it holds, stamp the device as seen
/// and register the connection `life` ends under the verified device id.
///
/// The verify and the registration happen under one hold of
/// [`Hub::live_devices`], the lock a block holds while it sets the flag and
/// cuts the device's connections, so no connection verifies before a block
/// and registers after its cut. The store compares digests in constant time;
/// the presented value is never logged, stored or echoed. A failed stamp is
/// logged and still serves the device.
fn authorize_device(
    hub: &Hub,
    peer: &tailnet::PeerIdentity,
    presented_token: &str,
    life: &CancellationToken,
) -> Option<LiveConnection> {
    debug!(device = %peer.name, "tailnet device token presented");
    let mut live = hub.live_devices.lock();
    let mut devices = crate::sync::lock(&hub.devices);
    let device_id = devices
        .verify(&peer.stable_id, presented_token)
        .map(|record| record.id.clone())?;
    if let Err(err) = devices.touch(&device_id, chrono::Utc::now()) {
        warn!(%device_id, ?err, "could not stamp the device as last seen");
    }
    drop(devices);
    Some(live.register(&device_id, life.clone()))
}

/// The capabilities this daemon lists in `Welcome.features` (`protocol::FEATURE_*`).
/// A feature joins only once the daemon fully honours it, since a client that
/// sees it listed offers the capability to the user.
/// The `screen` subcommand's dump: a daemon listing it answers `DumpScreen`
/// with a session's grid and the trace of its screen rules over it.
const FEATURE_SCREEN_DUMP: &str = "screen_dump";

const DAEMON_FEATURES: &[&str] = &[
    protocol::FEATURE_AGENT_COMMANDS,
    protocol::FEATURE_DEVICE_PAIRING,
    protocol::FEATURE_DEVICE_BLOCK,
    protocol::FEATURE_DAEMON_ID,
    FEATURE_SCREEN_DUMP,
    protocol::FEATURE_FILE_TAIL,
];

/// The `Welcome` answering an accepted `Hello`: the negotiated version, every
/// version this daemon speaks, [`DAEMON_FEATURES`] and this daemon's stable id.
fn welcome_message(negotiated: u32, daemon_id: &str) -> DaemonMessage {
    DaemonMessage::Welcome {
        protocol_version: negotiated,
        supported_versions: SUPPORTED_PROTOCOL_VERSIONS.to_vec(),
        features: DAEMON_FEATURES
            .iter()
            .map(|feature| (*feature).to_owned())
            .collect(),
        daemon_id: Some(daemon_id.to_owned()),
    }
}

/// Pick the highest protocol version both the client and daemon understand.
/// The client's advertised set is the union of its scalar `protocol_version`
/// and its `protocol_versions` vec; the daemon's set is the build-time
/// `SUPPORTED_PROTOCOL_VERSIONS` constant.
fn negotiate_protocol_version(scalar: u32, range: &[u32]) -> Option<u32> {
    let mut best: Option<u32> = None;
    for &v in std::iter::once(&scalar).chain(range.iter()) {
        if SUPPORTED_PROTOCOL_VERSIONS.contains(&v) {
            best = Some(best.map_or(v, |cur| cur.max(v)));
        }
    }
    best
}

pub(super) fn push_initial_state(
    hub: &Hub,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    client_id: &str,
    needs_layout_chooser: bool,
) {
    let (repos, workspaces, container_order, session_order) = hub.state.with_persisted(|s| {
        (
            s.repos.clone(),
            s.workspaces.clone(),
            s.container_order.clone(),
            s.session_order.clone(),
        )
    });
    // The agents and their settings come first: the repos' last spawn
    // configs and the sessions name agents.
    let _ = out_tx.send(DaemonMessage::Agents {
        agents: agent_registry::descriptors(),
    });
    let _ = out_tx.send(DaemonMessage::AgentSettings {
        settings: hub.state.agent_settings(),
    });
    let _ = out_tx.send(DaemonMessage::Repos { repos });
    let _ = out_tx.send(DaemonMessage::Workspaces { workspaces });
    let _ = out_tx.send(DaemonMessage::ContainersReordered {
        ordered: container_order,
    });
    for (container_id, ordered_ids) in session_order {
        if !ordered_ids.is_empty() {
            let _ = out_tx.send(DaemonMessage::SessionsReordered {
                container_id,
                ordered_ids,
            });
        }
    }
    let sessions = hub.sessions.snapshots();
    let session_count = u32::try_from(sessions.len()).unwrap_or(u32::MAX);
    let _ = out_tx.send(DaemonMessage::Sessions { sessions });
    if needs_layout_chooser {
        let clonable = hub
            .state
            .clonable_layouts(client_id)
            .into_iter()
            .map(|(client_id, name)| ClonableLayout { client_id, name })
            .collect();
        let _ = out_tx.send(DaemonMessage::LayoutInitRequired {
            has_legacy: !hub.state.legacy_tabs().is_empty(),
            active_session_count: session_count,
            clonable,
        });
    } else {
        let _ = out_tx.send(DaemonMessage::Tabs {
            tabs: hub.state.client_layout(client_id),
        });
    }
    let _ = out_tx.send(DaemonMessage::WorktreesRootChanged {
        root: hub.state.worktrees_dir().to_string_lossy().into_owned(),
        is_override: hub.state.worktrees_root_is_override(),
    });
    let keep_awake = *hub.keep_awake_status.borrow();
    let _ = out_tx.send(DaemonMessage::KeepAwakeStatus {
        enabled: keep_awake.mode != protocol::KeepAwakeMode::Off,
        mode: keep_awake.mode,
        active: keep_awake.active,
    });
    let _ = out_tx.send(DaemonMessage::AgentCommands {
        commands: super::settings::agent_command_entries(&hub.state, &|name| {
            std::env::var(name).ok()
        }),
    });
    hub.summarizer.send_status(out_tx);
    // Held across the send: every RemoteStatus broadcast is published under
    // the watch's write lock, so none can reach the state forwarder between
    // this read and the send, and the client never ends on an older status.
    let remote = hub.remote_status.borrow();
    let _ = out_tx.send(DaemonMessage::RemoteStatus {
        enabled: remote.snapshot.enabled,
        port: remote.snapshot.port,
        state: remote.snapshot.state.clone(),
    });
    drop(remote);
    send_resume_report(hub, out_tx, client_id);
}

/// Tells `client_id` how the start-up resume went, when there is something to
/// report and this client id has not heard it yet. The initial-state push and
/// a state stream that lagged past the broadcast both come through here, so
/// the one-shot report survives either way.
pub(super) fn send_resume_report(
    hub: &Hub,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    client_id: &str,
) {
    if let Some((resumed, left_in_recover)) = *crate::sync::lock(&hub.start_resume)
        && (resumed > 0 || left_in_recover > 0)
        && hub.claim_resume_report(client_id)
    {
        let _ = out_tx.send(DaemonMessage::ResumeReport {
            resumed: u32::try_from(resumed).unwrap_or(u32::MAX),
            left_in_recover: u32::try_from(left_in_recover).unwrap_or(u32::MAX),
        });
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::server::remote::{build_tailnet_router, name_slot, set_serving_name};
    use crate::server::test_support::{test_hub, test_peer};
    use crate::tailnet_listener::AuthorizedPeer;
    use protocol::PROTOCOL_VERSION;
    use std::net::Ipv4Addr;

    /// Every `ResumeReport` an initial-state push put on the wire, draining
    /// `out_rx`.
    fn resume_reports(out_rx: &mut mpsc::UnboundedReceiver<DaemonMessage>) -> Vec<(u32, u32)> {
        let mut reports = Vec::new();
        while let Ok(message) = out_rx.try_recv() {
            if let DaemonMessage::ResumeReport {
                resumed,
                left_in_recover,
            } = message
            {
                reports.push((resumed, left_in_recover));
            }
        }
        reports
    }

    #[test]
    fn a_client_connecting_after_the_resume_gets_the_report_once() {
        let (hub, _scratch) = test_hub("resume-report-connect");
        *crate::sync::lock(&hub.start_resume) = Some((2, 1));
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();

        push_initial_state(&hub, &out_tx, "c1", false);
        assert_eq!(
            resume_reports(&mut out_rx),
            [(2, 1)],
            "a client connecting after the resume hears how it went"
        );

        push_initial_state(&hub, &out_tx, "c1", false);
        assert_eq!(
            resume_reports(&mut out_rx),
            [],
            "the same client id is told once per daemon run"
        );
    }

    #[test]
    fn initial_state_sends_the_agent_commands() {
        let (hub, _scratch) = test_hub("agent-commands-initial");
        hub.state
            .set_agent_command(
                &agent_registry::default_agent(),
                crate::state::AgentCommand {
                    program: "cl".to_owned(),
                    args: "--debug".to_owned(),
                },
            )
            .expect("store a command");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();

        push_initial_state(&hub, &out_tx, "c1", false);

        let mut sent = Vec::new();
        while let Ok(message) = out_rx.try_recv() {
            if let DaemonMessage::AgentCommands { commands } = message {
                sent.push(commands);
            }
        }
        assert_eq!(sent.len(), 1, "one AgentCommands at initial state");
        assert!(
            sent[0]
                .iter()
                .any(|entry| entry.agent == agent_registry::default_agent()
                    && entry.program == "cl"
                    && entry.args == "--debug"),
            "{sent:?}"
        );
    }

    #[test]
    fn initial_state_sends_agents_and_settings_before_repos_and_sessions() {
        let (hub, _scratch) = test_hub("agents-initial");
        let claude = agent_registry::default_agent();
        let options = protocol::OptionValues::from([(
            "provider".to_owned(),
            protocol::OptionValue::Text("deepseek".to_owned()),
        )]);
        hub.state
            .set_agent_settings(&claude, options.clone())
            .expect("store settings");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();

        push_initial_state(&hub, &out_tx, "c1", false);

        let mut kinds = Vec::new();
        while let Ok(message) = out_rx.try_recv() {
            match message {
                DaemonMessage::Agents { agents } => {
                    assert_eq!(agents, agent_registry::descriptors());
                    kinds.push("agents");
                }
                DaemonMessage::AgentSettings { settings } => {
                    assert_eq!(
                        settings,
                        std::collections::BTreeMap::from([(claude.clone(), options.clone())])
                    );
                    kinds.push("agent_settings");
                }
                DaemonMessage::Repos { .. } => kinds.push("repos"),
                DaemonMessage::Sessions { .. } => kinds.push("sessions"),
                _ => {}
            }
        }
        assert_eq!(kinds, ["agents", "agent_settings", "repos", "sessions"]);
    }

    #[test]
    fn a_clean_start_sends_no_report() {
        let (hub, _scratch) = test_hub("resume-report-clean");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();

        push_initial_state(&hub, &out_tx, "c1", false);

        assert_eq!(
            resume_reports(&mut out_rx),
            [],
            "nothing was resumed, so there is nothing to report"
        );
    }
    #[test]
    fn welcome_carries_the_daemon_features_and_the_negotiated_version() {
        let welcome = welcome_message(22, TEST_DAEMON_ID);
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { protocol_version: 22, supported_versions, features, .. }
                    if supported_versions == SUPPORTED_PROTOCOL_VERSIONS
                        && features == DAEMON_FEATURES
            ),
            "{welcome:?}"
        );
    }

    #[test]
    fn welcome_lists_agent_commands() {
        let welcome = welcome_message(22, TEST_DAEMON_ID);
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { features, .. }
                    if features.iter().any(|f| f == "agent_commands")
            ),
            "{welcome:?}"
        );
    }

    #[test]
    fn negotiate_picks_highest_match_from_range() {
        // SUPPORTED_PROTOCOL_VERSIONS is whatever this build advertises.
        // Use the daemon's own current version as the guaranteed-supported
        // anchor and probe the negotiator around it.
        let v = PROTOCOL_VERSION;
        let result = negotiate_protocol_version(v, &[v]);
        assert_eq!(result, Some(v));
    }

    #[test]
    fn negotiate_returns_none_when_no_overlap() {
        // u32::MAX is guaranteed not to be in the daemon's supported list.
        let result = negotiate_protocol_version(u32::MAX, &[u32::MAX - 1, u32::MAX - 2]);
        assert!(result.is_none());
    }

    #[test]
    fn negotiate_accepts_scalar_only() {
        // Old-shape Hello: empty range vec, scalar carries the version.
        let v = PROTOCOL_VERSION;
        assert_eq!(negotiate_protocol_version(v, &[]), Some(v));
    }

    #[test]
    fn negotiate_prefers_highest_when_both_supported() {
        // If the client lists multiple versions and several are supported,
        // pick the highest. Use a synthetic case by constructing a list
        // where two entries are real: a v15 anchored to PROTOCOL_VERSION
        // and a higher unsupported one (u32::MAX) — the supported one wins.
        let v = PROTOCOL_VERSION;
        let result = negotiate_protocol_version(v, &[u32::MAX, v]);
        assert_eq!(result, Some(v));
    }

    #[test]
    fn tailnet_hello_never_accepts_the_daemon_token() {
        let (hub, _scratch) = test_hub("tailnet-hello");
        let auth = AuthKind::Tailnet(test_peer());
        let verdict = hello_verdict(&hub, &auth, "test-token");
        assert_eq!(
            verdict,
            HelloAuth::Pairing,
            "the daemon token is not a device token"
        );
        let printed = format!("{verdict:?}");
        assert!(
            !printed.contains("test-token"),
            "the verdict never echoes the presented value: {printed}"
        );
        assert!(
            !printed.contains("invalid auth token"),
            "a tailnet verdict is never the loopback reason: {printed}"
        );
        assert_eq!(
            hello_verdict(&hub, &auth, "any-other-value"),
            HelloAuth::Pairing,
            "an unknown token may only pair"
        );
        assert_eq!(
            hello_verdict(&hub, &auth, ""),
            HelloAuth::Pairing,
            "no token may only pair"
        );
    }

    #[test]
    fn local_hello_still_accepts_the_daemon_token() {
        let (hub, _scratch) = test_hub("local-hello");
        assert_eq!(
            hello_verdict(&hub, &AuthKind::Local, "test-token"),
            HelloAuth::Local
        );
        assert_eq!(
            hello_verdict(&hub, &AuthKind::Local, "wrong"),
            HelloAuth::Refused("invalid auth token"),
            "a wrong loopback token is refused"
        );
    }

    /// Issue a device token for `peer` straight into the hub's store.
    fn issue_token(hub: &Hub, peer: &tailnet::PeerIdentity) -> (String, String) {
        crate::sync::lock(&hub.devices)
            .issue(peer, chrono::Utc::now())
            .expect("issue a device token")
    }

    fn other_peer() -> tailnet::PeerIdentity {
        tailnet::PeerIdentity {
            stable_id: "nOtherNode".to_owned(),
            name: "tablet.tail1234.ts.net".to_owned(),
            user_id: 2,
            login: "owner@example.com".to_owned(),
        }
    }

    /// What `presented` earns on `auth`, for a connection nothing ends.
    fn hello_verdict(hub: &Hub, auth: &AuthKind, presented: &str) -> HelloAuth {
        check_hello_auth(hub, auth, presented, &CancellationToken::new())
    }

    /// The owner blocks the device while its handshake has taken the live
    /// registry and waits on the store: the block waits for the handshake's
    /// verify and registration, then cuts that connection, and a handshake
    /// after the block lands in pairing.
    #[test]
    fn a_revoked_device_cannot_register_a_connection_that_races_the_revoke() {
        let (hub, _scratch) = test_hub("race-revoke");
        let (device_id, token) = issue_token(&hub, &test_peer());
        let life = CancellationToken::new();

        let store = crate::sync::lock(&hub.devices);
        let connecting = {
            let (hub, token, life) = (hub.clone(), token.clone(), life.clone());
            std::thread::spawn(move || {
                check_hello_auth(&hub, &AuthKind::Tailnet(test_peer()), &token, &life)
            })
        };
        // The handshake takes the live registry before the store; one that
        // never does leaves this loop to nextest's hang guard.
        while !hub.live_devices.is_locked() {
            std::thread::yield_now();
        }
        let blocking = {
            let (hub, device_id) = (hub.clone(), device_id.clone());
            std::thread::spawn(move || crate::server::pairing::block_device(&hub, &device_id))
        };
        drop(store);

        let verdict = connecting.join().expect("the handshake thread");
        let blocked = blocking.join().expect("the block thread");
        assert!(blocked.expect("the flag is written"), "the record exists");
        assert!(
            matches!(&verdict, HelloAuth::Device(_)),
            "the token verified before the flag: {verdict:?}"
        );
        assert!(
            life.is_cancelled(),
            "the block cut the connection that verified before it"
        );
        assert_eq!(hub.live_devices.lock().connected(&device_id), 0);
        drop(verdict);
        assert_eq!(
            hello_verdict(&hub, &AuthKind::Tailnet(test_peer()), &token),
            HelloAuth::Pairing,
            "a handshake after the block only pairs"
        );
    }

    #[test]
    fn tailnet_hello_with_an_issued_token_is_served() {
        let (hub, _scratch) = test_hub("tailnet-hello-issued");
        let (device_id, token) = issue_token(&hub, &test_peer());
        let auth = AuthKind::Tailnet(test_peer());
        let verdict = hello_verdict(&hub, &auth, &token);
        assert!(
            matches!(&verdict, HelloAuth::Device(live) if live.device_id() == device_id),
            "{verdict:?}"
        );
        assert_eq!(
            hub.live_devices.lock().connected(&device_id),
            1,
            "a served Hello registers the connection under its device"
        );
        drop(verdict);
        assert_eq!(hub.live_devices.lock().connected(&device_id), 0);
        let devices = crate::sync::lock(&hub.devices);
        let record = devices
            .list()
            .iter()
            .find(|record| record.id == device_id)
            .expect("the device's record");
        assert!(
            record.last_seen.is_some(),
            "a served Hello stamps last_seen"
        );
    }

    #[test]
    fn tailnet_hello_with_another_devices_token_enters_pairing() {
        let (hub, _scratch) = test_hub("tailnet-hello-other");
        let (_, token) = issue_token(&hub, &other_peer());
        let auth = AuthKind::Tailnet(test_peer());
        assert_eq!(
            hello_verdict(&hub, &auth, &token),
            HelloAuth::Pairing,
            "a token copied to another node only pairs"
        );
    }

    #[test]
    fn tailnet_hello_with_a_revoked_token_enters_pairing() {
        let (hub, _scratch) = test_hub("tailnet-hello-revoked");
        let (device_id, token) = issue_token(&hub, &test_peer());
        assert!(
            crate::sync::lock(&hub.devices)
                .revoke(&device_id)
                .expect("revoke the device"),
            "the record exists"
        );
        let auth = AuthKind::Tailnet(test_peer());
        assert_eq!(hello_verdict(&hub, &auth, &token), HelloAuth::Pairing);
    }

    #[test]
    fn local_hello_refuses_a_device_token() {
        let (hub, _scratch) = test_hub("local-hello-device-token");
        let (_, token) = issue_token(&hub, &test_peer());
        let verdict = hello_verdict(&hub, &AuthKind::Local, &token);
        assert_eq!(verdict, HelloAuth::Refused("invalid auth token"));
        assert!(
            !format!("{verdict:?}").contains(&token),
            "the refusal never echoes the token"
        );
    }

    #[test]
    fn welcome_advertises_device_pairing() {
        let welcome = welcome_message(22, TEST_DAEMON_ID);
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { features, .. }
                    if features.iter().any(|f| f == protocol::FEATURE_DEVICE_PAIRING)
            ),
            "{welcome:?}"
        );
    }

    #[test]
    fn welcome_lists_device_block() {
        let welcome = welcome_message(22, TEST_DAEMON_ID);
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { features, .. }
                    if features.iter().any(|f| f == protocol::FEATURE_DEVICE_BLOCK)
            ),
            "{welcome:?}"
        );
    }

    const TEST_DAEMON_ID: &str = "0b9f4a52-6c7d-4e1f-8a3b-5c2d1e0f9a87";

    #[test]
    fn welcome_carries_the_daemon_id() {
        let welcome = welcome_message(22, TEST_DAEMON_ID);
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { daemon_id: Some(id), .. } if id == TEST_DAEMON_ID
            ),
            "{welcome:?}"
        );
    }

    #[test]
    fn welcome_lists_daemon_id_feature() {
        let welcome = welcome_message(22, TEST_DAEMON_ID);
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { features, .. }
                    if features.iter().any(|f| f == protocol::FEATURE_DAEMON_ID)
            ),
            "{welcome:?}"
        );
    }

    /// Serve the tailnet router on a scratch listener and open one tailnet
    /// WebSocket connection that never sends its `Hello`. Returns the
    /// connected client stream and the serving task.
    async fn silent_tailnet_connection(
        hub: &Hub,
    ) -> (
        tokio::net::TcpStream,
        tokio::task::JoinHandle<std::io::Result<()>>,
    ) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let names = name_slot();
        set_serving_name(&names, Some("host.tail1234.ts.net".to_owned()));
        let app = build_tailnet_router(hub.clone(), names, 8787).layer(axum::Extension(
            AuthorizedPeer::for_tests(test_peer(), tokio_util::sync::CancellationToken::new()),
        ));
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind a scratch listener");
        let addr = listener.local_addr().expect("the bound address");
        let server = tokio::spawn(async move { axum::serve(listener, app).await });

        let mut client = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect to the scratch listener");
        let request = "GET /ws HTTP/1.1\r\n\
                       Host: host.tail1234.ts.net:8787\r\n\
                       Connection: Upgrade\r\n\
                       Upgrade: websocket\r\n\
                       Sec-WebSocket-Version: 13\r\n\
                       Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n";
        client
            .write_all(request.as_bytes())
            .await
            .expect("send the upgrade request");
        let mut response = Vec::new();
        let mut buf = [0u8; 1024];
        while !response.windows(4).any(|w| w == b"\r\n\r\n") {
            let read = client
                .read(&mut buf)
                .await
                .expect("read the upgrade response");
            assert!(read > 0, "the server closed before upgrading");
            response.extend_from_slice(&buf[..read]);
        }
        assert!(
            String::from_utf8_lossy(&response).starts_with("HTTP/1.1 101"),
            "{}",
            String::from_utf8_lossy(&response)
        );
        (client, server)
    }

    /// Wait until the client count reaches `want`, failing at the hang guard
    /// rather than at a timing bound.
    async fn wait_for_client_count(count: &mut tokio::sync::watch::Receiver<usize>, want: usize) {
        let reached = tokio::time::timeout(Duration::from_secs(120), async {
            while *count.borrow_and_update() != want {
                count.changed().await.expect("the count channel stays open");
            }
        })
        .await;
        assert!(reached.is_ok(), "the client count never reached {want}");
    }

    /// A tailnet connection that has not sent `Hello` yet holds a client slot
    /// for as long as the handshake is open, and closing it releases the slot.
    /// The handshake deadline is set far past any passing run, so no deadline
    /// can race the count.
    #[tokio::test]
    async fn tailnet_connection_counts_while_it_waits_for_hello() {
        let (mut hub, _scratch) = test_hub("tailnet-count-while-waiting");
        hub.tailnet_handshake_timeout = Duration::from_secs(600);
        let mut count = hub.client_count.subscribe();

        let (client, server) = silent_tailnet_connection(&hub).await;
        wait_for_client_count(&mut count, 1).await;

        drop(client);
        wait_for_client_count(&mut count, 0).await;

        server.abort();
    }

    /// A tailnet connection that never sends `Hello` is answered and closed at
    /// the handshake deadline, releasing its client slot. The 1 ms limit pins
    /// the timeout branch; the test asserts what the branch did and never how
    /// long it took.
    #[tokio::test]
    async fn tailnet_handshake_without_hello_times_out_and_releases_the_client_count() {
        use tokio::io::AsyncReadExt as _;

        let (mut hub, _scratch) = test_hub("tailnet-handshake-timeout");
        hub.tailnet_handshake_timeout = Duration::from_millis(1);
        let mut count = hub.client_count.subscribe();
        // Mark the starting 0 as already seen, so a receiver that never
        // observes the count move waits out the guard instead of passing on
        // its first value.
        count.borrow_and_update();

        let (mut client, server) = silent_tailnet_connection(&hub).await;

        // The daemon closed the connection: after whatever it sends on a
        // refused handshake, the client's stream ends at EOF or a connection
        // error. Anything else means it is still open.
        let mut buf = [0u8; 256];
        tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                match client.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(err) => {
                        assert!(
                            matches!(
                                err.kind(),
                                std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::ConnectionAborted
                                    | std::io::ErrorKind::BrokenPipe
                            ),
                            "unexpected read error: {err}"
                        );
                        break;
                    }
                }
            }
        })
        .await
        .expect("the daemon closes a connection that missed its handshake");

        // The client can see EOF before the session's guard is dropped, so the
        // count is checked after the close. One observed change proves the
        // connection took a slot, and a value back at 0 proves the timed-out
        // handshake gave it back; a leaked guard never sends again and waits
        // out the guard.
        let released = tokio::time::timeout(Duration::from_secs(120), async {
            count.changed().await.expect("the count channel stays open");
            while *count.borrow_and_update() != 0 {
                count.changed().await.expect("the count channel stays open");
            }
        })
        .await;
        assert!(
            released.is_ok(),
            "a timed-out handshake releases its client count"
        );

        server.abort();
    }
}
