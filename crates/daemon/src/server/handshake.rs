//! The WebSocket handshake: reading and authorising `Hello`, negotiating the protocol version, `Welcome`, and the initial state push.
use super::Hub;
use super::connection::{AuthKind, Inbound, origin_of};
use super::live_devices::LiveConnection;
use crate::devices::DeviceId;
use anyhow::{Context as _, anyhow};
use axum::extract::ws::Message;
use futures::StreamExt as _;
use protocol::{
    ClientKind, ClientMessage, ClonableLayout, ConnectionOrigin, DaemonMessage,
    InboundClientMessage, NO_COMMON_PROTOCOL_REASON, SUPPORTED_PROTOCOL_VERSIONS,
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
    /// The client's build version, absent when it has none.
    pub(super) client_version: Option<String>,
    /// What kind of program it is; `None` for a command-line tool and for an
    /// older client.
    pub(super) client_kind: Option<ClientKind>,
    /// The `CLIENT_FEATURE_*` strings it implements.
    pub(super) client_features: Vec<String>,
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

/// How long a loopback peer the door refused has to send its `Hello`, take
/// the refusal and finish the closing handshake before its socket is dropped.
const REFUSED_PEER_LIMIT: Duration = Duration::from_secs(5);

/// Answer a loopback peer an elevated daemon refused
/// ([`super::loopback_door`]): read its `Hello`, send `AuthFailed` with
/// `reason`, and close. The `Hello` is read first so the close leaves no
/// unread data behind, which would turn it into a reset that loses the
/// refusal. Nothing the peer sends is acted on.
pub(super) async fn refuse_local_peer(mut socket: axum::extract::ws::WebSocket, reason: String) {
    let refuse = async {
        let _hello = socket.recv().await;
        let text = match serde_json::to_string(&DaemonMessage::AuthFailed { reason }) {
            Ok(text) => text,
            Err(err) => {
                warn!(?err, "could not serialize a refusal");
                return;
            }
        };
        if socket.send(Message::Text(text.into())).await.is_err() {
            return;
        }
        let _ = socket.send(Message::Close(None)).await;
        while let Some(Ok(_)) = socket.recv().await {}
    };
    if tokio::time::timeout(REFUSED_PEER_LIMIT, refuse)
        .await
        .is_err()
    {
        debug!("a refused loopback peer did not finish closing; dropping its socket");
    }
}

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
        client_version,
        client_kind,
        client_features,
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
                "{NO_COMMON_PROTOCOL_REASON}: client advertises scalar={protocol_version} \
                 + range={protocol_versions:?}, daemon supports {SUPPORTED_PROTOCOL_VERSIONS:?}"
            )
        })?;
    info!(
        negotiated,
        scalar = protocol_version,
        ?protocol_versions,
        client_id = client_id.as_deref().unwrap_or("(legacy)"),
        client_version = client_version.as_deref().unwrap_or("(unknown)"),
        ?client_kind,
        ?client_features,
        "handshake negotiated protocol version"
    );
    let _ = out_tx.send(welcome_message(
        negotiated,
        &hub.daemon_id,
        origin_of(auth),
        HeldServices {
            accounts: hub.accounts.is_some(),
            gate_state: hub.gate_state.is_some(),
            dispatch_runs: hub.dispatch_runs.is_some(),
            perf_monitor: hub.perf.has_source(),
        },
    ));
    Ok(Handshake {
        negotiated_version: negotiated,
        client_id,
        client_name,
        client_version,
        client_kind,
        client_features,
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
            Some(live) => {
                // The connection is registered and its locks are released by
                // now, so a client's list can follow the count it added.
                super::pairing::broadcast_devices(
                    &hub.live_devices,
                    &hub.devices,
                    &hub.state_events,
                );
                HelloAuth::Device(live)
            }
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

/// The `screen` subcommand's dump: a daemon listing it answers `DumpScreen`
/// with a session's grid and the trace of its screen rules over it.
const FEATURE_SCREEN_DUMP: &str = "screen_dump";

/// The capabilities this daemon lists in `Welcome.features` (`protocol::FEATURE_*`).
/// A feature joins only once the daemon fully honours it, since a client that
/// sees it listed offers the capability to the user.
///
/// These are the features every daemon lists, whatever its state. The account,
/// gate state, dispatch-run and performance monitor features are not here:
/// they join in [`welcome_message`] only when the hub holds an accounts
/// service, a gate state, a live-run folder's runs or a sample service with a
/// source, since a daemon without one cannot honour them.
const DAEMON_FEATURES: &[&str] = &[
    protocol::FEATURE_AGENT_COMMANDS,
    protocol::FEATURE_DEVICE_PAIRING,
    protocol::FEATURE_DEVICE_BLOCK,
    protocol::FEATURE_DAEMON_ID,
    FEATURE_SCREEN_DUMP,
    protocol::FEATURE_FILE_TAIL,
    protocol::FEATURE_FOLDER_FETCH,
    protocol::FEATURE_SESSION_FOLDERS,
    protocol::FEATURE_DETAILED_LOGGING,
    protocol::FEATURE_DEFAULT_SHELL,
    protocol::FEATURE_SUBAGENT_LISTING,
    protocol::FEATURE_AGENT_USER_HOOKS,
    protocol::FEATURE_WEB_PUSH,
    protocol::FEATURE_RELAY_PUSH,
    protocol::FEATURE_UPLOAD_FILE,
    protocol::FEATURE_CLIENT_PRESENCE,
    protocol::FEATURE_TYPED_PROMPT_ANSWERS,
    protocol::FEATURE_RESUME_SESSION,
    protocol::FEATURE_LAUNCH_LAST,
    protocol::FEATURE_PRESET_JOB_REPEAT,
    protocol::FEATURE_NEW_SESSION,
    protocol::FEATURE_SPAWN_EXTRA_ARGS,
    protocol::FEATURE_REMOTE_UPDATE,
    protocol::FEATURE_HOST_LAYOUTS,
    protocol::FEATURE_CLIENT_UPDATE,
];

/// The features a daemon lists only when it holds the service behind them.
#[derive(Debug, Clone, Copy, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each bool is one independent service the hub holds or not"
)]
struct HeldServices {
    accounts: bool,
    gate_state: bool,
    dispatch_runs: bool,
    perf_monitor: bool,
}

/// The `Welcome` answering an accepted `Hello`: the negotiated version, every
/// version this daemon speaks, [`DAEMON_FEATURES`], this daemon's stable id
/// and the connection's `origin`, which only the accepting listener decides.
/// `accounts` appends `protocol::FEATURE_ACCOUNTS` and
/// `protocol::FEATURE_ACCOUNT_EVENTS`, in that order, after those entries:
/// the hub holds an accounts service only where the agent's account store
/// works, and a daemon without one lists neither. `gate_state` then appends
/// `protocol::FEATURE_GATE_STATE`: the hub holds a gate state only where the
/// gate's live inputs are read (64-bit Windows). `dispatch_runs` then appends
/// `protocol::FEATURE_DISPATCH_RUNS`: the hub holds dispatch runs only where
/// the daemon watches a live-run folder. `perf_monitor` then appends
/// `protocol::FEATURE_PERF_MONITOR`: the hub's sample service has a source
/// only on Windows and macOS.
fn welcome_message(
    negotiated: u32,
    daemon_id: &str,
    origin: ConnectionOrigin,
    held: HeldServices,
) -> DaemonMessage {
    let mut features: Vec<String> = DAEMON_FEATURES
        .iter()
        .map(|feature| (*feature).to_owned())
        .collect();
    if held.accounts {
        features.push(protocol::FEATURE_ACCOUNTS.to_owned());
        features.push(protocol::FEATURE_ACCOUNT_EVENTS.to_owned());
    }
    if held.gate_state {
        features.push(protocol::FEATURE_GATE_STATE.to_owned());
    }
    if held.dispatch_runs {
        features.push(protocol::FEATURE_DISPATCH_RUNS.to_owned());
    }
    if held.perf_monitor {
        features.push(protocol::FEATURE_PERF_MONITOR.to_owned());
    }
    DaemonMessage::Welcome {
        protocol_version: negotiated,
        supported_versions: SUPPORTED_PROTOCOL_VERSIONS.to_vec(),
        features,
        daemon_id: Some(daemon_id.to_owned()),
        origin: Some(origin),
        build_version: Some(protocol::BUILD_VERSION.to_owned()),
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

/// Send the full dispatch-run list, under the runs lock, when the daemon
/// follows a live-run folder; nothing otherwise.
fn send_dispatch_runs(hub: &Hub, out_tx: &mpsc::UnboundedSender<DaemonMessage>) {
    if let Some(dispatch) = &hub.dispatch_runs {
        dispatch.runs.send_list(out_tx);
    }
}

pub(super) fn push_initial_state(
    hub: &Hub,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    client_id: &str,
    needs_layout_chooser: bool,
    local: bool,
) {
    // A folder may have gained or lost `.git` since the list was last read;
    // the other clients hear of a change through the broadcast.
    crate::git_watch::announce_git_changes(&hub.state, &hub.state_events);
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
    send_dispatch_runs(hub, out_tx);
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
    let _ = out_tx.send(DaemonMessage::DetailedLogging {
        enabled: hub.state.detailed_logging(),
    });
    let _ = out_tx.send(DaemonMessage::AgentCommands {
        commands: super::settings::agent_command_entries(&hub.state, &|name| {
            std::env::var(name).ok()
        }),
    });
    // Sent to every client: Hello carries no client features, and an older
    // client's Unknown wrapper absorbs it.
    let _ = out_tx.send(super::settings::default_shell_message(&hub.state));
    let _ = out_tx.send(super::settings::subagent_listing_message(&hub.state));
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
    // Held across the send for the same reason: every gate state broadcast
    // is published under the watch's write lock. A daemon that reads no gate
    // state, or has found none yet, sends nothing.
    if let Some(gate) = &hub.gate_state {
        let current = gate.current();
        if let Some(state) = &*current {
            let _ = out_tx.send(DaemonMessage::GateState {
                state: Some(Box::new(state.clone())),
            });
        }
        drop(current);
    }
    // A local connection only: the host update's current phase, read and
    // sent under the lock every host update notice is published under.
    if local {
        hub.host_update.send_phase(out_tx);
    }
    send_resume_report(hub, out_tx, client_id);
    // Last: how the host update this client asked for ended, sent once and
    // only to the client whose `client_id` asked, local, SSH or tailnet.
    if let Some(result) = hub.host_update.host_result_for(client_id) {
        let _ = out_tx.send(result);
    }
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
    use crate::server::test_support::{HANG_GUARD, hello, mem_session, test_hub, test_peer};
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

    /// Every `HostUpdateScheduled` (its id and seconds) an initial-state push
    /// put on the wire, draining `out_rx`.
    fn scheduled_notices(
        out_rx: &mut mpsc::UnboundedReceiver<DaemonMessage>,
    ) -> Vec<(String, u32)> {
        let mut notices = Vec::new();
        while let Ok(message) = out_rx.try_recv() {
            if let DaemonMessage::HostUpdateScheduled {
                update_id, seconds, ..
            } = message
            {
                notices.push((update_id, seconds));
            }
        }
        notices
    }

    #[tokio::test(start_paused = true)]
    async fn a_local_connection_that_connects_mid_countdown_gets_the_seconds_left() {
        let (hub, _scratch) = test_hub("host-update-mid-countdown");
        let update_id = hub.host_update.begin_test_countdown(&hub.state_events);
        tokio::time::advance(std::time::Duration::from_secs(3)).await;

        let (local_tx, mut local_rx) = mpsc::unbounded_channel();
        push_initial_state(&hub, &local_tx, "c1", false, true);
        let (tailnet_tx, mut tailnet_rx) = mpsc::unbounded_channel();
        push_initial_state(&hub, &tailnet_tx, "c2", false, false);

        assert_eq!(scheduled_notices(&mut local_rx), [(update_id, 7)]);
        assert_eq!(
            scheduled_notices(&mut tailnet_rx),
            [],
            "a tailnet connection is not told"
        );
    }

    #[test]
    fn initial_repos_report_a_folder_without_git_as_plain() {
        let (hub, scratch) = test_hub("initial-repos-plain");
        let folder = scratch.path().join("notes");
        std::fs::create_dir_all(&folder).expect("create the plain folder");
        hub.state
            .mutate(|s| {
                s.repos.push(crate::server::test_support::repo_entry(
                    "r-notes", "notes", &folder,
                ));
            })
            .expect("register the folder as git");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();

        push_initial_state(&hub, &out_tx, "c1", false, true);

        let mut repos = None;
        while let Ok(message) = out_rx.try_recv() {
            if let DaemonMessage::Repos { repos: sent } = message {
                repos = Some(sent);
            }
        }
        let repos = repos.expect("the initial state carries the repos");
        assert_eq!(repos.len(), 1);
        assert!(!repos[0].is_git, "the refresh ran first: {repos:?}");
        assert!(
            !hub.state.with_persisted(|s| s.repos[0].is_git),
            "the corrected flag is saved"
        );
    }

    #[test]
    fn a_client_connecting_after_the_resume_gets_the_report_once() {
        let (hub, _scratch) = test_hub("resume-report-connect");
        *crate::sync::lock(&hub.start_resume) = Some((2, 1));
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();

        push_initial_state(&hub, &out_tx, "c1", false, true);
        assert_eq!(
            resume_reports(&mut out_rx),
            [(2, 1)],
            "a client connecting after the resume hears how it went"
        );

        push_initial_state(&hub, &out_tx, "c1", false, true);
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

        push_initial_state(&hub, &out_tx, "c1", false, true);

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

        push_initial_state(&hub, &out_tx, "c1", false, true);

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
    fn welcome_lists_detailed_logging_and_the_initial_state_carries_the_switch() {
        let (hub, _scratch) = test_hub("detailed-logging-initial");
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices {
                accounts: true,
                ..HeldServices::default()
            },
        );
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { features, .. }
                    if features.iter().any(|f| f == protocol::FEATURE_DETAILED_LOGGING)
            ),
            "{welcome:?}"
        );

        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        push_initial_state(&hub, &out_tx, "c1", false, true);
        let mut seen = None;
        while let Ok(message) = out_rx.try_recv() {
            if let DaemonMessage::DetailedLogging { enabled } = message {
                seen = Some(enabled);
            }
        }
        assert_eq!(seen, Some(false), "a fresh daemon sends the switch as off");
    }

    #[test]
    fn welcome_lists_subagent_listing_and_the_initial_state_carries_it() {
        let (hub, _scratch) = test_hub("subagent-listing-initial");
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices {
                accounts: true,
                ..HeldServices::default()
            },
        );
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { features, .. }
                    if features.iter().any(|f| f == protocol::FEATURE_SUBAGENT_LISTING)
            ),
            "{welcome:?}"
        );

        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        push_initial_state(&hub, &out_tx, "c1", false, true);
        let mut seen = None;
        while let Ok(message) = out_rx.try_recv() {
            if let DaemonMessage::SubagentListing { listing } = message {
                seen = Some(listing);
            }
        }
        assert_eq!(
            seen,
            Some(protocol::SubagentListing::default()),
            "a fresh daemon sends five and no window"
        );
    }

    #[test]
    fn a_clean_start_sends_no_report() {
        let (hub, _scratch) = test_hub("resume-report-clean");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();

        push_initial_state(&hub, &out_tx, "c1", false, true);

        assert_eq!(
            resume_reports(&mut out_rx),
            [],
            "nothing was resumed, so there is nothing to report"
        );
    }
    #[test]
    fn welcome_carries_the_daemon_features_and_the_negotiated_version() {
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices {
                accounts: true,
                ..HeldServices::default()
            },
        );
        let mut expected: Vec<String> = DAEMON_FEATURES
            .iter()
            .map(|feature| (*feature).to_owned())
            .collect();
        expected.push(protocol::FEATURE_ACCOUNTS.to_owned());
        expected.push(protocol::FEATURE_ACCOUNT_EVENTS.to_owned());
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { protocol_version: 22, supported_versions, features, .. }
                    if supported_versions == SUPPORTED_PROTOCOL_VERSIONS
                        && features == &expected
            ),
            "{welcome:?}"
        );
        assert!(
            !DAEMON_FEATURES.contains(&protocol::FEATURE_ACCOUNT_EVENTS),
            "the account events feature joins only with the accounts feature"
        );
        assert!(
            DAEMON_FEATURES.contains(&protocol::FEATURE_REMOTE_UPDATE),
            "every daemon answers a remote update, if only with why it cannot"
        );
    }

    #[test]
    fn welcome_lists_client_update() {
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices::default(),
        );
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { features, .. }
                    if features.iter().any(|f| f == protocol::FEATURE_CLIENT_UPDATE)
            ),
            "{welcome:?}"
        );
    }

    #[test]
    fn welcome_carries_the_daemon_build_version() {
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices::default(),
        );
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { build_version: Some(version), .. }
                    if version == env!("CARGO_PKG_VERSION")
            ),
            "{welcome:?}"
        );
    }

    #[test]
    fn welcome_lists_web_push() {
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices::default(),
        );
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { features, .. }
                    if features.iter().any(|f| f == protocol::FEATURE_WEB_PUSH)
            ),
            "{welcome:?}"
        );
    }

    #[test]
    fn welcome_lists_relay_push() {
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices::default(),
        );
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { features, .. }
                    if features.iter().any(|f| f == protocol::FEATURE_RELAY_PUSH)
            ),
            "{welcome:?}"
        );
    }

    #[test]
    fn welcome_lists_resume_session() {
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices::default(),
        );
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { features, .. }
                    if features.iter().any(|f| f == protocol::FEATURE_RESUME_SESSION)
            ),
            "{welcome:?}"
        );
    }

    #[test]
    fn welcome_lists_launch_last() {
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices::default(),
        );
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { features, .. }
                    if features.iter().any(|f| f == protocol::FEATURE_LAUNCH_LAST)
            ),
            "{welcome:?}"
        );
    }

    #[test]
    fn welcome_lists_new_session() {
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices::default(),
        );
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { features, .. }
                    if features.iter().any(|f| f == protocol::FEATURE_NEW_SESSION)
            ),
            "{welcome:?}"
        );
    }

    #[test]
    fn welcome_lists_spawn_extra_args() {
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices::default(),
        );
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { features, .. }
                    if features.iter().any(|f| f == protocol::FEATURE_SPAWN_EXTRA_ARGS)
            ),
            "{welcome:?}"
        );
    }

    #[test]
    fn welcome_lists_preset_job_repeat() {
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices::default(),
        );
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { features, .. }
                    if features.iter().any(|f| f == protocol::FEATURE_PRESET_JOB_REPEAT)
            ),
            "{welcome:?}"
        );
    }

    #[test]
    fn the_welcome_lists_client_presence() {
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices::default(),
        );
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { features, .. }
                    if features.iter().any(|f| f == protocol::FEATURE_CLIENT_PRESENCE)
            ),
            "{welcome:?}"
        );
    }

    #[test]
    fn the_welcome_lists_upload_file() {
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices::default(),
        );
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { features, .. }
                    if features.iter().any(|f| f == protocol::FEATURE_UPLOAD_FILE)
            ),
            "{welcome:?}"
        );
    }

    /// Every `DefaultShell` an initial-state push put on the wire.
    fn default_shells(
        out_rx: &mut mpsc::UnboundedReceiver<DaemonMessage>,
    ) -> Vec<(String, Option<String>)> {
        let mut sent = Vec::new();
        while let Ok(message) = out_rx.try_recv() {
            if let DaemonMessage::DefaultShell { id, command } = message {
                sent.push((id, command));
            }
        }
        sent
    }

    /// Every message the initial state sends to a connection.
    fn initial_state(hub: &Hub, local: bool) -> Vec<DaemonMessage> {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        push_initial_state(hub, &out_tx, "c1", false, local);
        let mut sent = Vec::new();
        while let Ok(message) = out_rx.try_recv() {
            sent.push(message);
        }
        sent
    }

    /// Every message the initial state sends to a connection of `client_id`.
    fn initial_state_of(hub: &Hub, client_id: &str, local: bool) -> Vec<DaemonMessage> {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        push_initial_state(hub, &out_tx, client_id, false, local);
        let mut sent = Vec::new();
        while let Ok(message) = out_rx.try_recv() {
            sent.push(message);
        }
        sent
    }

    #[test]
    fn initial_state_ends_with_the_host_result_for_its_requester_once() {
        let (mut hub, scratch) = test_hub("host-update-result-initial");
        let updates = scratch.path().join("updates");
        std::fs::create_dir_all(&updates).expect("updates folder");
        let running = protocol::BUILD_VERSION;
        std::fs::write(
            updates.join("host-update-request.json"),
            serde_json::json!({ "client_id": "c1", "from": running, "to": "99.0.0" }).to_string(),
        )
        .expect("request record");
        std::fs::write(
            updates.join("host-update-result.json"),
            serde_json::json!({
                "from": running,
                "to": "99.0.0",
                "ok": false,
                "message": "The update failed.",
            })
            .to_string(),
        )
        .expect("host result");
        hub.host_update =
            std::sync::Arc::new(crate::server::HostUpdate::nothing_installed_in(updates));
        let is_result =
            |message: &DaemonMessage| matches!(message, DaemonMessage::HostUpdateResult { .. });

        let other = initial_state_of(&hub, "c2", false);
        let requester = initial_state_of(&hub, "c1", false);
        let again = initial_state_of(&hub, "c1", true);

        assert!(!other.iter().any(is_result), "another client: {other:?}");
        assert!(
            matches!(
                requester.last(),
                Some(DaemonMessage::HostUpdateResult { from: Some(from), to, ok: false, message })
                    if from == running && to == "99.0.0" && message == "The update failed."
            ),
            "the requester: {requester:?}"
        );
        assert!(!again.iter().any(is_result), "sent once: {again:?}");
    }

    #[test]
    fn initial_state_sends_no_update_result_without_one() {
        let (mut hub, scratch) = test_hub("no-update-result-initial");
        let updates = scratch.path().join("updates");
        hub.host_update =
            std::sync::Arc::new(crate::server::HostUpdate::nothing_installed_in(updates));

        let sent = initial_state(&hub, true);

        assert!(
            !sent
                .iter()
                .any(|message| matches!(message, DaemonMessage::HostUpdateResult { .. })),
            "{sent:?}"
        );
    }

    #[test]
    fn initial_state_sends_the_default_shell() {
        let (hub, _scratch) = test_hub("default-shell-initial");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        push_initial_state(&hub, &out_tx, "c1", false, true);
        assert_eq!(
            default_shells(&mut out_rx),
            [("auto".to_owned(), None)],
            "a fresh daemon sends Automatic"
        );

        hub.state
            .set_default_shell(Some(protocol::DefaultShell {
                id: "custom".to_owned(),
                command: Some("bash -l".to_owned()),
            }))
            .expect("store a shell");
        push_initial_state(&hub, &out_tx, "c1", false, true);
        assert_eq!(
            default_shells(&mut out_rx),
            [("custom".to_owned(), Some("bash -l".to_owned()))]
        );

        hub.state
            .set_default_shell(Some(protocol::DefaultShell {
                id: "tcsh".to_owned(),
                command: None,
            }))
            .expect("store an unknown shell");
        push_initial_state(&hub, &out_tx, "c1", false, true);
        assert_eq!(
            default_shells(&mut out_rx),
            [("auto".to_owned(), None)],
            "an id this build does not know reads as Automatic"
        );
    }

    #[test]
    fn welcome_lists_default_shell() {
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices {
                accounts: true,
                ..HeldServices::default()
            },
        );
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { features, .. }
                    if features.iter().any(|f| f == "default_shell")
            ),
            "{welcome:?}"
        );
    }

    #[test]
    fn welcome_lists_agent_commands() {
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices {
                accounts: true,
                ..HeldServices::default()
            },
        );
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
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices {
                accounts: true,
                ..HeldServices::default()
            },
        );
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
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices {
                accounts: true,
                ..HeldServices::default()
            },
        );
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
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices {
                accounts: true,
                ..HeldServices::default()
            },
        );
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
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices {
                accounts: true,
                ..HeldServices::default()
            },
        );
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { features, .. }
                    if features.iter().any(|f| f == protocol::FEATURE_DAEMON_ID)
            ),
            "{welcome:?}"
        );
    }

    #[test]
    fn welcome_lists_accounts_feature() {
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices {
                accounts: true,
                ..HeldServices::default()
            },
        );
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { features, .. }
                    if features.iter().any(|f| f == protocol::FEATURE_ACCOUNTS)
                        && features.iter().any(|f| f == protocol::FEATURE_ACCOUNT_EVENTS)
            ),
            "{welcome:?}"
        );
    }

    #[test]
    fn welcome_without_an_accounts_service_lists_no_account_features() {
        let welcome = welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices::default(),
        );
        assert!(
            matches!(
                &welcome,
                DaemonMessage::Welcome { features, .. }
                    if !features.iter().any(|f| f == protocol::FEATURE_ACCOUNTS)
                        && !features.iter().any(|f| f == protocol::FEATURE_ACCOUNT_EVENTS)
                        && DAEMON_FEATURES.iter().all(|f| features.iter().any(|seen| seen == f))
            ),
            "{welcome:?}"
        );
    }

    /// The features a `Welcome` lists.
    fn features_of(welcome: &DaemonMessage) -> Vec<String> {
        match welcome {
            DaemonMessage::Welcome { features, .. } => features.clone(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn gate_welcome_lists_the_feature_only_when_enabled() {
        let enabled = features_of(&welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices {
                accounts: true,
                gate_state: true,
                ..HeldServices::default()
            },
        ));
        assert_eq!(
            enabled.last().map(String::as_str),
            Some(protocol::FEATURE_GATE_STATE),
            "the gate feature follows the account features: {enabled:?}"
        );

        let disabled = features_of(&welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices {
                accounts: true,
                ..HeldServices::default()
            },
        ));
        assert!(
            !disabled.iter().any(|f| f == protocol::FEATURE_GATE_STATE),
            "{disabled:?}"
        );
        assert!(
            !DAEMON_FEATURES.contains(&protocol::FEATURE_GATE_STATE),
            "the gate feature joins only where the gate state is read"
        );
    }

    #[test]
    fn welcome_lists_dispatch_runs_only_with_a_folder() {
        let with = features_of(&welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices {
                accounts: true,
                gate_state: true,
                dispatch_runs: true,
                ..HeldServices::default()
            },
        ));
        assert_eq!(
            with.last().map(String::as_str),
            Some(protocol::FEATURE_DISPATCH_RUNS),
            "the dispatch-run feature follows the gate feature: {with:?}"
        );
        let gate_at = with.iter().position(|f| f == protocol::FEATURE_GATE_STATE);
        assert_eq!(gate_at, Some(with.len() - 2), "{with:?}");

        let without = features_of(&welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices {
                accounts: true,
                gate_state: true,
                ..HeldServices::default()
            },
        ));
        assert!(
            !without.iter().any(|f| f == protocol::FEATURE_DISPATCH_RUNS),
            "{without:?}"
        );
        assert!(
            !DAEMON_FEATURES.contains(&protocol::FEATURE_DISPATCH_RUNS),
            "the dispatch-run feature joins only where a live-run folder is watched"
        );
    }

    #[test]
    fn welcome_lists_perf_monitor_only_with_a_source() {
        let with = features_of(&welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices {
                perf_monitor: true,
                ..HeldServices::default()
            },
        ));
        let expected: Vec<&str> = DAEMON_FEATURES
            .iter()
            .copied()
            .chain([protocol::FEATURE_PERF_MONITOR])
            .collect();
        assert_eq!(with, expected, "the perf feature follows DAEMON_FEATURES");

        let without = features_of(&welcome_message(
            22,
            TEST_DAEMON_ID,
            ConnectionOrigin::Local,
            HeldServices::default(),
        ));
        assert!(
            !without.iter().any(|f| f == protocol::FEATURE_PERF_MONITOR),
            "{without:?}"
        );
        assert!(
            !DAEMON_FEATURES.contains(&protocol::FEATURE_PERF_MONITOR),
            "the perf feature joins only where the sample service has a source"
        );
    }

    #[test]
    fn dispatch_runs_follow_sessions_in_the_initial_state() {
        let (mut hub, scratch) = test_hub("dispatch-runs-initial-state");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        push_initial_state(&hub, &out_tx, "c1", false, true);
        let mut none = Vec::new();
        while let Ok(message) = out_rx.try_recv() {
            none.push(message);
        }
        assert!(
            !none
                .iter()
                .any(|m| matches!(m, DaemonMessage::DispatchRuns { .. })),
            "a daemon that watches no live-run folder sends no list"
        );

        let runs = crate::server::test_support::enable_dispatch_runs(
            &mut hub,
            scratch.path().join("live"),
        );
        runs.list_for_test("run-i", 4242, Some(7), chrono::Utc::now());
        push_initial_state(&hub, &out_tx, "c1", false, true);
        let mut sent = Vec::new();
        while let Ok(message) = out_rx.try_recv() {
            sent.push(message);
        }
        let sessions_at = sent
            .iter()
            .position(|m| matches!(m, DaemonMessage::Sessions { .. }))
            .expect("the sessions list is sent");
        let listed = match sent.get(sessions_at + 1) {
            Some(DaemonMessage::DispatchRuns { runs }) => Some(
                runs.iter()
                    .map(|run| run.run_id.clone())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        };
        assert_eq!(
            listed,
            Some(vec!["run-i".to_owned()]),
            "the run list follows the sessions list: {sent:?}"
        );
    }

    /// Every `GateState` an initial-state push put on the wire, draining `out_rx`.
    fn gate_states(
        out_rx: &mut mpsc::UnboundedReceiver<DaemonMessage>,
    ) -> Vec<Option<protocol::GateState>> {
        let mut sent = Vec::new();
        while let Ok(message) = out_rx.try_recv() {
            if let DaemonMessage::GateState { state } = message {
                sent.push(state.map(|state| *state));
            }
        }
        sent
    }

    #[test]
    fn gate_initial_state_carries_the_current_value() {
        let (mut hub, _scratch) = test_hub("gate-initial-state");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        push_initial_state(&hub, &out_tx, "c1", false, true);
        assert_eq!(
            gate_states(&mut out_rx),
            [],
            "a daemon that reads no gate state sends none"
        );

        crate::server::test_support::enable_gate_state(&mut hub, None, None);
        push_initial_state(&hub, &out_tx, "c1", false, true);
        assert_eq!(
            gate_states(&mut out_rx),
            [],
            "nothing is sent before a read finds a gate"
        );

        let state = protocol::GateState {
            now: None,
            machine: "TESTBOX".to_owned(),
            processors: 16,
            critical: None,
            queue: Vec::new(),
            pools: Vec::new(),
            remote: None,
            builders: Vec::new(),
            covered: Vec::new(),
        };
        crate::server::test_support::enable_gate_state(&mut hub, Some(state.clone()), None);
        push_initial_state(&hub, &out_tx, "c1", false, true);
        assert_eq!(gate_states(&mut out_rx), [Some(state)]);
    }

    /// The first message the daemon sends a connection of `auth` that
    /// presented `token` in its `Hello`.
    async fn first_reply(hub: &Hub, auth: AuthKind, token: &str) -> Option<DaemonMessage> {
        let (mut client, _session) = mem_session(hub, auth);
        client.send(&hello(token, None));
        tokio::time::timeout(HANG_GUARD, client.next(true))
            .await
            .expect("the daemon answers the Hello")
    }

    #[tokio::test]
    async fn welcome_reports_local_origin_on_loopback() {
        let (hub, _scratch) = test_hub("welcome-origin-local");
        let token = hub.auth_token.clone();
        let welcome = first_reply(&hub, AuthKind::Local, &token).await;
        assert!(
            matches!(
                welcome,
                Some(DaemonMessage::Welcome {
                    origin: Some(ConnectionOrigin::Local),
                    ..
                })
            ),
            "{welcome:?}"
        );
    }

    #[tokio::test]
    async fn a_hello_with_no_common_version_is_refused_with_the_stable_reason() {
        let (hub, _scratch) = test_hub("no-common-version");
        let token = hub.auth_token.clone();
        let (mut client, _session) = mem_session(&hub, AuthKind::Local);
        let mut refused = hello(&token, None);
        if let protocol::ClientMessage::Hello {
            protocol_version,
            protocol_versions,
            ..
        } = &mut refused
        {
            // u32::MAX is guaranteed not to be in the daemon's supported list.
            *protocol_version = u32::MAX;
            *protocol_versions = vec![u32::MAX];
        }
        client.send(&refused);
        let reply = tokio::time::timeout(HANG_GUARD, client.next(true))
            .await
            .expect("the daemon answers the Hello");
        assert!(
            matches!(
                &reply,
                Some(DaemonMessage::AuthFailed { reason })
                    if reason.starts_with(protocol::NO_COMMON_PROTOCOL_REASON)
            ),
            "{reply:?}"
        );
    }

    #[tokio::test]
    async fn welcome_reports_remote_origin_on_tailnet() {
        let (hub, _scratch) = test_hub("welcome-origin-tailnet");
        // Even the daemon's own token earns a tailnet connection no local
        // origin: the listener decides it, not what the client presents.
        let token = hub.auth_token.clone();
        let welcome = first_reply(&hub, AuthKind::Tailnet(test_peer()), &token).await;
        assert!(
            matches!(
                welcome,
                Some(DaemonMessage::Welcome {
                    origin: Some(ConnectionOrigin::Remote),
                    ..
                })
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
