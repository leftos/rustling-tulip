//! Remote access over the tailnet: its switch, listener, router and serving name, remote status, and LAN refusals.
use super::connection::{AuthKind, client_session_until};
use super::{Hub, StateEvent};
use crate::paths::Dirs;
use crate::tailnet_listener::{AuthorizedPeer, ListenerStatus, TailnetListener};
use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use protocol::{ClientMessage, DaemonMessage};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};

/// The files the retired LAN listener kept in the config dir: its config,
/// which holds a bearer token, its certificate and its private key. Nothing
/// reads them.
const RETIRED_LAN_FILES: [&str; 3] = ["lan.json", "lan-cert.pem", "lan-key.pem"];

/// Delete [`RETIRED_LAN_FILES`] from `config`, best-effort, at startup. A
/// missing file is the usual case; a failed delete is logged and never stops
/// the daemon.
pub(super) fn remove_retired_lan_files(config: &Path) {
    for name in RETIRED_LAN_FILES {
        match std::fs::remove_file(config.join(name)) {
            Ok(()) => info!(file = name, "removed a retired LAN access file"),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => error!(
                ?err,
                file = name,
                "could not remove a retired LAN access file"
            ),
        }
    }
}

/// The `MagicDNS` name the tailnet router accepts: the name of the
/// certificate in service, shared between the listener's status watch and the
/// `/ws` handler. Cleared whenever no certificate is serving, so a rename or a
/// failed certificate fails closed.
type NameSlot = Arc<std::sync::Mutex<Option<String>>>;

pub(super) fn name_slot() -> NameSlot {
    Arc::new(std::sync::Mutex::new(None))
}

pub(super) fn set_serving_name(slot: &NameSlot, name: Option<String>) {
    *crate::sync::lock(slot) = name;
}

/// The name a tailnet upgrade may carry: only a certificate in service names
/// one; every other status clears it, so the accepted origin follows the
/// certificate, never the last `whoami` answer.
fn serving_name(status: &ListenerStatus) -> Option<String> {
    match status {
        ListenerStatus::Serving { name, .. } => Some(name.clone()),
        _ => None,
    }
}

/// The remote-access status clients are shown: the persisted switch and
/// port, and what the listener is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSnapshot {
    pub enabled: bool,
    pub port: u16,
    pub state: protocol::RemoteState,
}

/// The current remote status and the generation of the listener slot it
/// belongs to. Every start or stop of the listener begins a new generation,
/// and a relay publishes only into its own, so a dropped listener's late
/// status never lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSlot {
    pub generation: u64,
    pub snapshot: RemoteSnapshot,
}

impl RemoteSlot {
    /// Generation zero: `remote`'s switch and port, no listener yet.
    pub(crate) fn initial(remote: crate::remote_config::RemoteConfig) -> Self {
        Self {
            generation: 0,
            snapshot: RemoteSnapshot {
                enabled: remote.enabled,
                port: remote.port,
                state: protocol::RemoteState::Off,
            },
        }
    }
}

fn broadcast_remote(state_events: &broadcast::Sender<StateEvent>, snapshot: &RemoteSnapshot) {
    let _ = state_events.send(StateEvent::RemoteStatus {
        enabled: snapshot.enabled,
        port: snapshot.port,
        state: snapshot.state.clone(),
    });
}

/// Begin a new generation showing `remote`'s switch and port with `state`,
/// broadcast it, and return it. Called with [`Hub::tailnet`] held, whenever
/// the listener slot changes; the broadcast goes out under the watch's lock,
/// so it is ordered with every relay's.
fn begin_remote_generation(
    hub: &Hub,
    remote: crate::remote_config::RemoteConfig,
    state: protocol::RemoteState,
) -> u64 {
    let mut generation = 0;
    hub.remote_status.send_modify(|slot| {
        slot.generation += 1;
        generation = slot.generation;
        slot.snapshot = RemoteSnapshot {
            enabled: remote.enabled,
            port: remote.port,
            state,
        };
        broadcast_remote(&hub.state_events, &slot.snapshot);
    });
    generation
}

/// Broadcast the current remote status unchanged, so a
/// [`ClientMessage::ConfigureRemote`] that changes nothing is still answered.
fn rebroadcast_remote(hub: &Hub) {
    hub.remote_status.send_if_modified(|slot| {
        broadcast_remote(&hub.state_events, &slot.snapshot);
        false
    });
}

/// Follows one listener's status watch for the generation it was started
/// in: the serving name into the router's [`NameSlot`], the mapped state
/// into [`Hub::remote_status`] and to every client.
struct RemoteRelay {
    remote_status: Arc<tokio::sync::watch::Sender<RemoteSlot>>,
    state_events: broadcast::Sender<StateEvent>,
    generation: u64,
    port: u16,
}

impl RemoteRelay {
    /// Publish `state` unless a newer generation began or nothing changed.
    fn publish(&self, state: protocol::RemoteState) {
        self.remote_status.send_if_modified(|slot| {
            if slot.generation != self.generation || slot.snapshot.state == state {
                return false;
            }
            slot.snapshot.state = state;
            broadcast_remote(&self.state_events, &slot.snapshot);
            true
        });
    }

    /// Follow `status` until the listener ends, then clear `names`.
    fn spawn(
        self,
        mut status: tokio::sync::watch::Receiver<ListenerStatus>,
        names: NameSlot,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let current = status.borrow_and_update().clone();
                set_serving_name(&names, serving_name(&current));
                self.publish(crate::tailnet_listener::remote_state(&current, self.port));
                if status.changed().await.is_err() {
                    break;
                }
            }
            set_serving_name(&names, None);
        })
    }
}

/// Split a `Host` header into its name and port, trimming a trailing dot from
/// the name. A suffix that is not a port number stays part of the name.
fn split_host(host: &str) -> (&str, Option<u16>) {
    match host.rsplit_once(':') {
        Some((name, port)) => match port.parse::<u16>() {
            Ok(port) => (name.trim_end_matches('.'), Some(port)),
            Err(_) => (host.trim_end_matches('.'), None),
        },
        None => (host.trim_end_matches('.'), None),
    }
}

/// Whether a tailnet `/ws` upgrade carries the headers the PWA or the native
/// client would send. Fail closed: an unknown name, a missing or duplicated
/// `Host`, a foreign or duplicated `Origin` are all refused. The accepted
/// `Origin` is exactly `https://{name}:{port}`, and the bare name is a `Host`
/// only on the default HTTPS port.
fn tailnet_upgrade_allowed(headers: &HeaderMap, name: Option<&str>, port: u16) -> bool {
    let Some(name) = name else {
        return false;
    };
    let mut hosts = headers.get_all(axum::http::header::HOST).iter();
    let (Some(host), None) = (hosts.next(), hosts.next()) else {
        return false;
    };
    let Ok(host) = host.to_str() else {
        return false;
    };
    let (host_name, host_port) = split_host(host);
    if !host_name.eq_ignore_ascii_case(name) {
        return false;
    }
    let host_ok = match host_port {
        Some(host_port) => host_port == port,
        None => port == 443,
    };
    if !host_ok {
        return false;
    }

    let mut origins = headers.get_all(axum::http::header::ORIGIN).iter();
    match (origins.next(), origins.next()) {
        (None, None) => true,
        (Some(origin), None) => origin.to_str().is_ok_and(|origin| {
            // A browser serializes the default HTTPS port out of `Origin`, so
            // at 443 the bare origin is the one the PWA sends.
            origin == format!("https://{name}:{port}")
                || (port == 443 && origin == format!("https://{name}"))
        }),
        _ => false,
    }
}

#[derive(Clone)]
struct TailnetState {
    hub: Hub,
    names: NameSlot,
    port: u16,
}

/// The router the tailnet listener serves: `/ws` and `/health` only. It never
/// carries `/shutdown` or a CORS layer, so a tailnet client cannot
/// manage the host's process and a browser cannot act for another origin.
pub(super) fn build_tailnet_router(hub: Hub, names: NameSlot, port: u16) -> Router {
    Router::new()
        .route("/ws", get(tailnet_ws_handler))
        .route("/health", get(|| async { "ok" }))
        .with_state(TailnetState { hub, names, port })
}

/// The peer the tailnet listener's acceptor admitted. Extracting it refuses
/// the request with a bodiless 403 when it is absent, so a request that never
/// passed the acceptor can never become a connection.
struct TailnetPeer(AuthorizedPeer);

impl<S: Send + Sync> FromRequestParts<S> for TailnetPeer {
    type Rejection = Response;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        std::future::ready(
            parts
                .extensions
                .get::<AuthorizedPeer>()
                .cloned()
                .map(TailnetPeer)
                .ok_or_else(|| {
                    warn!("tailnet /ws without an AuthorizedPeer extension; refused");
                    StatusCode::FORBIDDEN.into_response()
                }),
        )
    }
}

/// The tailnet `/ws` upgrade: refused with a bodiless 403 unless the request
/// came through the listener's acceptor and its `Host`/`Origin` name this
/// host. Only an [`AuthorizedPeer`] becomes a connection.
async fn tailnet_ws_handler(
    State(state): State<TailnetState>,
    TailnetPeer(peer): TailnetPeer,
    uri: Uri,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if uri.scheme().is_some() || uri.authority().is_some() {
        warn!(%uri, "tailnet /ws with an absolute-form request URI; refused");
        return StatusCode::FORBIDDEN.into_response();
    }
    let name = crate::sync::lock(&state.names).clone();
    if !tailnet_upgrade_allowed(&headers, name.as_deref(), state.port) {
        warn!("tailnet /ws upgrade refused: Host/Origin does not name this host");
        return StatusCode::FORBIDDEN.into_response();
    }
    let identity = peer.identity().clone();
    let server_life = peer.server_life().clone();
    ws.on_upgrade(move |socket| {
        client_session_until(
            state.hub,
            socket,
            AuthKind::Tailnet(identity),
            Some(server_life),
        )
    })
}

/// Whether the tailnet listener should start: remote access on and a usable
/// port.
fn remote_listener_allowed(remote: crate::remote_config::RemoteConfig) -> bool {
    remote.enabled && remote.port != 0
}

/// Start the tailnet listener when [`remote_listener_allowed`] says so, or
/// return the state that says why it did not: [`protocol::RemoteState::Off`]
/// while remote access is off, or the certificate dir that could not be
/// created. Every refusal is logged; none propagates out of [`run`].
fn start_tailnet_listener(
    hub: &Hub,
    remote: crate::remote_config::RemoteConfig,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<TailnetListener, protocol::RemoteState> {
    if !remote_listener_allowed(remote) {
        return Err(protocol::RemoteState::Off);
    }
    let tailnet_dir = match Dirs::ensure_tailnet_dir() {
        Ok(dir) => dir,
        Err(err) => {
            error!(
                ?err,
                "remote access enabled but the tailnet certificate dir could not be created"
            );
            return Err(protocol::RemoteState::CertFailed {
                // Fixed text: the error names a host path, which stays in
                // the log line above and never goes on the wire.
                reason: "the tailnet certificate dir could not be created; see daemon.log"
                    .to_owned(),
            });
        }
    };
    let names = name_slot();
    let router = build_tailnet_router(hub.clone(), Arc::clone(&names), remote.port);
    let listener = TailnetListener::start(router, tailnet_dir, remote.port, shutdown);
    let relay = RemoteRelay {
        remote_status: Arc::clone(&hub.remote_status),
        state_events: hub.state_events.clone(),
        generation: begin_remote_generation(hub, remote, protocol::RemoteState::Starting),
        port: remote.port,
    };
    relay.spawn(listener.status(), names);
    Ok(listener)
}

/// Bring the listener in `slot` in line with `remote`: drop the running one,
/// then start a new one when remote access is on, beginning a new status
/// generation either way. The caller holds [`Hub::tailnet`].
fn apply_remote_config(
    hub: &Hub,
    slot: &mut Option<TailnetListener>,
    remote: crate::remote_config::RemoteConfig,
    shutdown: tokio::sync::watch::Receiver<bool>,
) {
    if slot.take().is_some() {
        info!("remote access: tailnet listener stopped");
    }
    match start_tailnet_listener(hub, remote, shutdown) {
        Ok(listener) => {
            info!(
                port = remote.port,
                "remote access enabled; tailnet listener started"
            );
            *slot = Some(listener);
        }
        Err(state) => {
            begin_remote_generation(hub, remote, state);
        }
    }
}

/// Start the tailnet listener into the hub when `remote.json` turns remote
/// access on, or leave it down with the reason logged and published.
pub(super) async fn start_remote_access(
    hub: &Hub,
    remote: crate::remote_config::RemoteConfig,
    shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut slot = hub.tailnet.lock().await;
    apply_remote_config(hub, &mut slot, remote, shutdown);
}

/// [`ClientMessage::ConfigureRemote`]: persist the switch, flip the watch
/// idle exit reads, start, stop or restart the listener, and broadcast the
/// status. [`Hub::tailnet`] stays locked throughout, so concurrent requests
/// serialise and `remote.json`, [`Hub::remote_enabled`] and the listener slot
/// agree whenever it is released. A request that changes nothing writes
/// nothing, keeps the running listener and re-broadcasts the status. When
/// the previous start was refused (no listener in the slot), an identical
/// `enabled: true` is not "nothing": it re-saves and retries the start.
/// `port: Some(0)` is refused with an `Error` and changes nothing. A tailnet
/// (or any non-local) connection is refused before this runs, with "remote
/// access is local-only".
pub(super) async fn configure_remote(
    hub: &Hub,
    enabled: bool,
    port: Option<u16>,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) -> anyhow::Result<()> {
    if port == Some(0) {
        let _ = out_tx.send(DaemonMessage::Error {
            message: "remote access needs a port from 1 to 65535".to_owned(),
            request_id: None,
            code: Some(protocol::error_codes::INVALID_REQUEST.to_owned()),
        });
        return Ok(());
    }
    let mut slot = hub.tailnet.lock().await;
    let shown = hub.remote_status.borrow().snapshot.clone();
    let remote = crate::remote_config::RemoteConfig {
        enabled,
        port: port.unwrap_or(shown.port),
    };
    let unchanged =
        shown.enabled == enabled && shown.port == remote.port && slot.is_some() == enabled;
    // With remote access off no device can be waiting: every request and
    // code goes, and every waiting device is told and closed. Told before
    // the listener is dropped, so the decision is queued ahead of the close.
    if unchanged {
        if !enabled {
            super::pairing::drop_all(hub);
        }
        rebroadcast_remote(hub);
    } else {
        crate::remote_config::save(&hub.dirs, remote)?;
        hub.remote_enabled.send_replace(enabled);
        if !enabled {
            super::pairing::drop_all(hub);
        }
        apply_remote_config(hub, &mut slot, remote, hub.shutdown_tx.subscribe());
        info!(enabled, port = remote.port, "remote access configured");
    }
    Ok(())
}

/// The `Error` every connection gets for `ConfigureLan`, `StartPairing` and
/// `CancelPairing`: the messages stay decodable, and nothing else happens.
pub(super) const LAN_RETIRED: &str = "LAN access and pairing codes are retired; remote access runs over \
                           Tailscale (turn it on with ConfigureRemote).";

/// The reason `msg` is host-only and refused on a tailnet connection, if it
/// is. Checked before the match so no side effect can run first; a message
/// that manages the host joins this set here.
pub(super) fn host_only_refusal(msg: &ClientMessage, auth: &AuthKind) -> Option<&'static str> {
    if matches!(auth, AuthKind::Local) {
        return None;
    }
    match msg {
        ClientMessage::Shutdown { .. } => Some("shutdown is local-only"),
        ClientMessage::ConfigureRemote { .. } => Some("remote access is local-only"),
        ClientMessage::DumpScreen { .. } => Some("screen dump is local-only"),
        ClientMessage::ListPendingDevices
        | ClientMessage::ApproveDevice { .. }
        | ClientMessage::DenyDevice { .. }
        | ClientMessage::MintDevicePairCode
        | ClientMessage::ListDevices
        | ClientMessage::RevokeDevice { .. }
        | ClientMessage::ForgetDevice { .. } => Some("device approval is local-only"),
        _ => None,
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::server::build_loopback_router;
    use crate::server::handshake::push_initial_state;
    use crate::server::test_support::{
        ScratchDir, WsClient, dispatch_all, dispatch_one_as, hello, hook_request, idle_record,
        loopback_ws_client, tailnet_ws_client_under, test_hub, test_hub_in, test_peer, ws_next,
        ws_send,
    };
    use crate::tailnet_listener::CertFailedCause;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use std::net::Ipv4Addr;
    use std::time::Duration;
    use tower::ServiceExt as _;
    fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                axum::http::HeaderName::try_from(*name).expect("a header name"),
                axum::http::HeaderValue::try_from(*value).expect("a header value"),
            );
        }
        headers
    }

    /// A WebSocket upgrade request, with any extra headers a case needs.
    fn ws_upgrade_request(uri: &str, extra: &[(&str, &str)]) -> Request<Body> {
        let mut builder = Request::builder()
            .uri(uri)
            .header("connection", "Upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==");
        for (name, value) in extra {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).expect("build the request")
    }

    async fn body_text(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("read the response body");
        String::from_utf8(bytes.to_vec()).expect("utf-8 body")
    }

    #[tokio::test]
    async fn tailnet_router_has_no_shutdown_or_pair() {
        let (hub, _scratch) = test_hub("tailnet-router-routes");
        let router = build_tailnet_router(hub, name_slot(), 8787);
        for uri in ["/shutdown", "/pair"] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(uri)
                        .body(Body::empty())
                        .expect("build the request"),
                )
                .await
                .expect("the router answers");
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{uri} must not exist on the tailnet router"
            );
        }
        for uri in ["/health", "/ws"] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .body(Body::empty())
                        .expect("build the request"),
                )
                .await
                .expect("the router answers");
            assert_ne!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{uri} must exist on the tailnet router"
            );
        }
    }

    #[tokio::test]
    async fn tailnet_router_has_no_hook_route() {
        let (hub, _scratch) = test_hub("hook-tailnet");
        hub.sessions.insert(idle_record("s1"));
        let response = build_tailnet_router(hub, name_slot(), 8787)
            .oneshot(hook_request(
                "s1",
                Some("test-token"),
                Some("100"),
                r#"{"hook_event_name":"UserPromptSubmit"}"#,
            ))
            .await
            .expect("the router answers");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn tailnet_router_serves_health_constantly() {
        let (hub, _scratch) = test_hub("tailnet-router-health");
        let router = build_tailnet_router(hub, name_slot(), 8787);
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .expect("build the request"),
            )
            .await
            .expect("the router answers");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_text(response).await, "ok");
    }

    #[tokio::test]
    async fn loopback_router_keeps_its_routes() {
        let (hub, _scratch) = test_hub("loopback-router-routes");
        let router = build_loopback_router(hub);
        let health = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .expect("build the request"),
            )
            .await
            .expect("the router answers");
        assert_eq!(health.status(), StatusCode::OK);

        let shutdown = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/shutdown")
                    .body(Body::empty())
                    .expect("build the request"),
            )
            .await
            .expect("the router answers");
        assert_eq!(shutdown.status(), StatusCode::UNAUTHORIZED);

        // The hook endpoint is loopback-only and needs the bearer token.
        let hook = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/hook/some-session")
                    .body(Body::empty())
                    .expect("build the request"),
            )
            .await
            .expect("the router answers");
        assert_eq!(hook.status(), StatusCode::UNAUTHORIZED);

        let ws = router
            .oneshot(ws_upgrade_request("/ws", &[]))
            .await
            .expect("the router answers");
        assert_ne!(ws.status(), StatusCode::NOT_FOUND, "/ws must exist");
    }

    #[tokio::test]
    async fn tailnet_ws_without_authorized_peer_does_not_upgrade() {
        let (hub, _scratch) = test_hub("tailnet-no-peer");
        let names = name_slot();
        set_serving_name(&names, Some("host.tail1234.ts.net".to_owned()));
        let router = build_tailnet_router(hub, names, 8787);
        let response = router
            .oneshot(ws_upgrade_request(
                "/ws",
                &[("host", "host.tail1234.ts.net:8787")],
            ))
            .await
            .expect("the router answers");
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "a request without an AuthorizedPeer never becomes a connection"
        );
    }

    #[test]
    fn tailnet_upgrade_allowed() {
        let name = Some("host.tail1234.ts.net");
        let host = "host.tail1234.ts.net:8787";
        let origin = "https://host.tail1234.ts.net:8787";

        // A native client sends Host and no Origin.
        assert!(super::tailnet_upgrade_allowed(
            &header_map(&[("host", host)]),
            name,
            8787
        ));
        // The PWA's own origin.
        assert!(super::tailnet_upgrade_allowed(
            &header_map(&[("host", host), ("origin", origin)]),
            name,
            8787
        ));
        // Mixed case and a trailing dot name the same host.
        assert!(super::tailnet_upgrade_allowed(
            &header_map(&[("host", "HOST.Tail1234.TS.NET.:8787")]),
            name,
            8787
        ));
        // `Origin: null` is an opaque origin, never the PWA.
        assert!(!super::tailnet_upgrade_allowed(
            &header_map(&[("host", host), ("origin", "null")]),
            name,
            8787
        ));
        // A foreign origin.
        assert!(!super::tailnet_upgrade_allowed(
            &header_map(&[("host", host), ("origin", "https://evil.example")]),
            name,
            8787
        ));
        // A look-alike name.
        assert!(!super::tailnet_upgrade_allowed(
            &header_map(&[
                ("host", host),
                ("origin", "https://host.tail1234.ts.net.evil.ts.net:8787"),
            ]),
            name,
            8787
        ));
        // Duplicate Origin headers are ambiguous.
        assert!(!super::tailnet_upgrade_allowed(
            &header_map(&[("host", host), ("origin", origin), ("origin", origin)]),
            name,
            8787
        ));
        // Host is required, and exactly once.
        assert!(!super::tailnet_upgrade_allowed(
            &header_map(&[]),
            name,
            8787
        ));
        assert!(!super::tailnet_upgrade_allowed(
            &header_map(&[("host", "other.ts.net:8787")]),
            name,
            8787
        ));
        assert!(!super::tailnet_upgrade_allowed(
            &header_map(&[("host", host), ("host", "other.ts.net:8787")]),
            name,
            8787
        ));
        // The bare name is the default port only.
        assert!(!super::tailnet_upgrade_allowed(
            &header_map(&[("host", "host.tail1234.ts.net")]),
            name,
            8787
        ));
        assert!(super::tailnet_upgrade_allowed(
            &header_map(&[("host", "host.tail1234.ts.net")]),
            name,
            443
        ));
        assert!(super::tailnet_upgrade_allowed(
            &header_map(&[
                ("host", "host.tail1234.ts.net:443"),
                ("origin", "https://host.tail1234.ts.net:443"),
            ]),
            name,
            443
        ));
        // A browser drops the default HTTPS port from Origin.
        assert!(super::tailnet_upgrade_allowed(
            &header_map(&[
                ("host", "host.tail1234.ts.net"),
                ("origin", "https://host.tail1234.ts.net"),
            ]),
            name,
            443
        ));
        // The bare origin is the default HTTPS port only.
        assert!(!super::tailnet_upgrade_allowed(
            &header_map(&[("host", host), ("origin", "https://host.tail1234.ts.net"),]),
            name,
            8787
        ));
        // Without a certificate in service there is no name to match.
        assert!(!super::tailnet_upgrade_allowed(
            &header_map(&[("host", host)]),
            None,
            8787
        ));
        // A rename only matters once the slot follows the certificate.
        assert!(!super::tailnet_upgrade_allowed(
            &header_map(&[("host", host)]),
            Some("renamed.tail1234.ts.net"),
            8787
        ));
    }

    #[test]
    fn serving_name_follows_only_the_serving_status() {
        let serving = ListenerStatus::Serving {
            name: "host.tail1234.ts.net".to_owned(),
            addrs: Vec::new(),
            cert_expires_at: None,
        };
        assert_eq!(
            serving_name(&serving),
            Some("host.tail1234.ts.net".to_owned())
        );
        for status in [
            ListenerStatus::Starting,
            ListenerStatus::HttpsDisabled,
            ListenerStatus::CertFailed(CertFailedCause::Command),
            ListenerStatus::PortInUse { port: 8787 },
            ListenerStatus::BindFailed("in use".to_owned()),
            ListenerStatus::TailscaleDown(tailnet::TailnetError::NotConnected),
        ] {
            assert_eq!(
                serving_name(&status),
                None,
                "{status:?} must clear the name"
            );
        }
    }

    #[tokio::test]
    async fn shutdown_from_tailnet_is_refused_and_local_still_shuts_down() {
        let (hub, _scratch) = test_hub("shutdown-tailnet");
        let mut local = hub.shutdown_tx.subscribe();
        let tailnet = AuthKind::Tailnet(test_peer());
        let reply = dispatch_one_as(&hub, ClientMessage::Shutdown { drain: false }, &tailnet).await;
        assert!(
            matches!(&reply, DaemonMessage::Error { message, .. } if message == "shutdown is local-only"),
            "{reply:?}"
        );
        assert!(
            !*hub.shutdown_tx.borrow(),
            "a tailnet Shutdown never flips the watch"
        );

        let local_auth = AuthKind::Local;
        let _ = dispatch_one_as(&hub, ClientMessage::Shutdown { drain: false }, &local_auth).await;
        assert!(
            *local.borrow_and_update(),
            "a local Shutdown still flips the watch"
        );
    }

    #[tokio::test]
    async fn retired_lan_messages_get_the_tailscale_error() {
        let (hub, _scratch) = test_hub("retired-lan");
        let lan_file = hub.dirs.config.join("lan.json");
        let tailnet = AuthKind::Tailnet(test_peer());
        for auth in [&AuthKind::Local, &tailnet] {
            for msg in [
                ClientMessage::ConfigureLan {
                    enabled: false,
                    port: 9000,
                },
                ClientMessage::StartPairing,
                ClientMessage::CancelPairing,
            ] {
                let reply = dispatch_one_as(&hub, msg.clone(), auth).await;
                assert!(
                    matches!(
                        &reply,
                        DaemonMessage::Error { message, request_id: None, code: Some(code) }
                            if message == LAN_RETIRED && code == protocol::error_codes::GONE
                    ),
                    "{auth:?} {msg:?}: {reply:?}"
                );
            }
        }
        assert!(LAN_RETIRED.contains("Tailscale"), "{LAN_RETIRED}");
        assert!(!lan_file.exists(), "a retired message writes no lan.json");
    }

    /// The seven host-only device messages, aimed at `request_id` and
    /// `device_id`.
    fn approval_messages(request_id: &str, device_id: &str) -> [ClientMessage; 7] {
        [
            ClientMessage::ListPendingDevices,
            ClientMessage::ApproveDevice {
                request_id: request_id.to_owned(),
            },
            ClientMessage::DenyDevice {
                request_id: request_id.to_owned(),
            },
            ClientMessage::MintDevicePairCode,
            ClientMessage::ListDevices,
            ClientMessage::RevokeDevice {
                device_id: device_id.to_owned(),
            },
            ClientMessage::ForgetDevice {
                device_id: device_id.to_owned(),
            },
        ]
    }

    /// The three messages only an unpaired device's pairing loop honours.
    fn pair_messages() -> [ClientMessage; 3] {
        [
            ClientMessage::DevicePairRequest,
            ClientMessage::DevicePairCancel,
            ClientMessage::DevicePairRedeem {
                request_id: "r1".to_owned(),
                code: protocol::Secret("123456".to_owned()),
            },
        ]
    }

    /// Every message the pairing module's own handler sends for `msg` on a
    /// connection of `auth`, past `dispatch` and its host-only check.
    fn pairing_handler_replies(
        hub: &Hub,
        msg: ClientMessage,
        auth: &AuthKind,
    ) -> Vec<DaemonMessage> {
        use crate::server::connection::{ConnCtx, FetchRegistry, PtyLane};
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let (file_tx, _file_rx) = mpsc::channel(1);
        let pty_lane: Arc<PtyLane> = Arc::default();
        let fetches: FetchRegistry =
            Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
        let ctx = ConnCtx {
            out_tx: &out_tx,
            file_tx: &file_tx,
            pty_lane: &pty_lane,
            fetches: &fetches,
            client_id: "c1",
            client_name: None,
            connection: 1,
            auth,
        };
        crate::server::pairing::handle(hub, msg, &ctx);
        let mut sent = Vec::new();
        while let Ok(message) = out_rx.try_recv() {
            sent.push(message);
        }
        sent
    }

    #[tokio::test]
    async fn device_approval_messages_are_refused_on_a_tailnet_connection() {
        let (hub, _scratch) = test_hub("device-approval-tailnet");
        // A real request and a real device, so a refusal that leaked through
        // would show in the book or the store.
        let request_id = crate::sync::lock(&hub.pairing)
            .request(&test_peer(), std::time::Instant::now(), chrono::Utc::now())
            .expect("request a pairing")
            .id;
        let (device_id, _) = crate::sync::lock(&hub.devices)
            .issue(&test_peer(), chrono::Utc::now())
            .expect("issue a device token");
        let tailnet = AuthKind::Tailnet(test_peer());
        for msg in approval_messages(&request_id, &device_id) {
            let reply = dispatch_one_as(&hub, msg.clone(), &tailnet).await;
            assert!(
                matches!(
                    &reply,
                    DaemonMessage::Error { message, .. } if message == "device approval is local-only"
                ),
                "through dispatch, {msg:?}: {reply:?}"
            );
            let replies = pairing_handler_replies(&hub, msg.clone(), &tailnet);
            assert!(
                matches!(
                    replies.as_slice(),
                    [DaemonMessage::Error { message, .. }]
                        if message == crate::server::pairing::LOCAL_ONLY
                ),
                "through the handler's own check, {msg:?}: {replies:?}"
            );
        }
        let pending = crate::sync::lock(&hub.pairing).pending();
        assert_eq!(
            pending.len(),
            1,
            "the request is neither denied nor dropped"
        );
        assert!(!pending[0].approved, "and not approved");
        let devices = crate::sync::lock(&hub.devices);
        assert!(!devices.list()[0].revoked, "the device is not revoked");
    }

    #[tokio::test]
    async fn pair_messages_on_a_served_connection_say_already_paired() {
        let (hub, _scratch) = test_hub("device-pair-served");
        let tailnet = AuthKind::Tailnet(test_peer());
        for msg in pair_messages() {
            let reply = dispatch_one_as(&hub, msg.clone(), &tailnet).await;
            assert!(
                matches!(
                    &reply,
                    DaemonMessage::Error { message, request_id: None, .. }
                        if message == crate::server::pairing::ALREADY_PAIRED
                ),
                "{msg:?}: {reply:?}"
            );
        }
        assert!(
            crate::sync::lock(&hub.pairing).pending().is_empty(),
            "a served connection never opens a request"
        );
    }

    #[tokio::test]
    async fn pair_messages_on_a_loopback_connection_say_tailnet_only() {
        let (hub, _scratch) = test_hub("device-pair-loopback");
        let local = AuthKind::Local;
        for msg in pair_messages() {
            let reply = dispatch_one_as(&hub, msg.clone(), &local).await;
            assert!(
                matches!(
                    &reply,
                    DaemonMessage::Error { message, request_id: None, .. }
                        if message == crate::server::pairing::TAILNET_ONLY
                ),
                "{msg:?}: {reply:?}"
            );
        }
        assert!(
            crate::sync::lock(&hub.pairing).pending().is_empty(),
            "a loopback connection never opens a request"
        );
    }

    #[tokio::test]
    async fn hub_builds_with_a_device_store() {
        let (hub, _scratch) = test_hub("device-store");
        let devices = hub.devices.lock().expect("lock the device store");
        assert!(!devices.is_corrupt(), "a fresh hub has a usable store");
        assert!(devices.list().is_empty(), "no device is approved yet");
        drop(devices);
        let book = hub.pairing.lock().expect("lock the pairing book");
        assert!(book.pending().is_empty(), "nothing waits for approval");
    }

    #[tokio::test]
    async fn inert_configure_lan_binds_nothing() {
        let (hub, _scratch) = test_hub("inert-configure-lan");
        let mut events = hub.state_events.subscribe();
        // The test holds the port itself, on loopback only, for the whole test.
        let own = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .expect("bind the test's own loopback listener");
        let port = own.local_addr().expect("the bound address").port();
        let reply = dispatch_one_as(
            &hub,
            ClientMessage::ConfigureLan {
                enabled: true,
                port,
            },
            &AuthKind::Local,
        )
        .await;
        assert!(
            matches!(&reply, DaemonMessage::Error { message, .. } if message == LAN_RETIRED),
            "{reply:?}"
        );
        assert!(hub.tailnet.lock().await.is_none(), "no listener starts");
        assert!(
            matches!(
                events.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "nothing is broadcast"
        );
        let client = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .expect("connect to the test's port");
        let (_accepted, peer) = own
            .accept()
            .expect("the test's own listener still owns the port");
        assert_eq!(
            peer,
            client.local_addr().expect("the client's address"),
            "the connection reached the test's listener"
        );
    }

    #[tokio::test]
    async fn pair_route_is_gone() {
        let (hub, _scratch) = test_hub("pair-route-gone");
        let routers = [
            ("loopback", build_loopback_router(hub.clone())),
            ("tailnet", build_tailnet_router(hub, name_slot(), 8787)),
        ];
        for (name, router) in routers {
            let response = router
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/pair")
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"code":"123456"}"#))
                        .expect("build the request"),
                )
                .await
                .expect("the router answers");
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "/pair must not exist on the {name} router"
            );
        }
    }

    #[tokio::test]
    async fn stale_lan_json_never_blocks_remote_start() {
        let scratch = ScratchDir::new("stale-lan-json");
        // Declared before the hub, so the env vars are restored only
        // after the hub and its listener are gone.
        let _env = FakeTailscaleEnv::new(scratch.path()).await;
        let (hub, _scratch) = test_hub_in(scratch, None);
        std::fs::write(
            hub.dirs.config.join("lan.json"),
            br#"{"enabled":true,"port":8787,"auth_token":"x"}"#,
        )
        .expect("write a leftover lan.json");
        let mut events = hub.state_events.subscribe();
        let sent = dispatch_all(
            &hub,
            ClientMessage::ConfigureRemote {
                enabled: true,
                port: Some(8787),
            },
        )
        .await;
        assert!(sent.is_empty(), "the answer is a broadcast: {sent:?}");
        let (_, port, state) = next_remote_status(&mut events, |_, _, _| true).await;
        assert_eq!(port, 8787);
        assert!(
            !matches!(
                state,
                protocol::RemoteState::PortInUse | protocol::RemoteState::BindFailed { .. }
            ),
            "a leftover lan.json claims no port: {state:?}"
        );
        assert!(hub.tailnet.lock().await.is_some(), "the listener runs");
    }

    #[tokio::test]
    async fn remote_disabled_starts_no_tailnet_listener() {
        let (hub, _scratch) = test_hub("remote-off");
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let started = start_tailnet_listener(
            &hub,
            crate::remote_config::RemoteConfig::default(),
            shutdown_rx,
        );
        assert!(
            matches!(started, Err(protocol::RemoteState::Off)),
            "remote access off starts no tailnet listener"
        );
        assert!(hub.tailnet.lock().await.is_none());
    }

    #[test]
    fn leftover_lan_files_are_deleted_at_startup() {
        let scratch = ScratchDir::new("leftover-lan-files");
        let config = scratch.path();
        std::fs::write(config.join("lan.json"), b"{}").expect("write lan.json");
        std::fs::write(config.join("lan-cert.pem"), b"cert").expect("write lan-cert.pem");
        std::fs::write(config.join("remote.json"), b"{}").expect("write remote.json");
        remove_retired_lan_files(config);
        for name in RETIRED_LAN_FILES {
            assert!(!config.join(name).exists(), "{name} is removed");
        }
        assert!(
            config.join("remote.json").exists(),
            "only the retired files go"
        );
        // Every file already gone: still no panic and nothing to do.
        remove_retired_lan_files(config);
    }

    #[tokio::test]
    async fn remote_start_reports_cert_failed_when_the_tailnet_dir_cannot_be_made() {
        let scratch = ScratchDir::new("remote-start-no-dir");
        let blocker = scratch.path().join("a-file");
        std::fs::write(&blocker, b"not a dir").expect("write the blocking file");
        // Declared before the hub, so the env vars are restored only
        // after the hub and its listener are gone.
        let _env =
            FakeTailscaleEnv::with_tailnet_dir(scratch.path(), &blocker.join("tailnet")).await;
        let (hub, scratch) = test_hub_in(scratch, None);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let on = crate::remote_config::RemoteConfig {
            enabled: true,
            port: 8787,
        };
        let started = start_tailnet_listener(&hub, on, shutdown_rx);
        let state = started.err();
        assert!(
            matches!(&state, Some(protocol::RemoteState::CertFailed { reason }) if reason.contains("tailnet")),
            "the certificate dir sits under a file: {state:?}"
        );
        if let Some(protocol::RemoteState::CertFailed { reason }) = &state {
            assert!(
                !reason.contains('\\') && !reason.contains('/'),
                "the wire reason carries no path: {reason}"
            );
            let scratch_name = scratch
                .path()
                .file_name()
                .expect("the scratch dir has a name")
                .to_string_lossy();
            assert!(
                !reason.contains(scratch_name.as_ref()),
                "the wire reason carries no part of the host's paths: {reason}"
            );
        }
    }

    #[tokio::test]
    async fn remote_start_with_a_usable_setup_starts_the_listener() {
        let scratch = ScratchDir::new("remote-start-ok");
        // Declared before the hub, so the env vars are restored only
        // after the hub and its listener are gone.
        let _env = FakeTailscaleEnv::new(scratch.path()).await;
        let (hub, _scratch) = test_hub_in(scratch, None);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let on = crate::remote_config::RemoteConfig {
            enabled: true,
            port: 8787,
        };
        let started = start_tailnet_listener(&hub, on, shutdown_rx);
        assert!(started.is_ok(), "remote access on starts the listener");
    }

    #[test]
    fn remote_listener_needs_the_switch_and_a_port() {
        assert!(remote_listener_allowed(
            crate::remote_config::RemoteConfig {
                enabled: true,
                port: 8787,
            }
        ));
        assert!(!remote_listener_allowed(
            crate::remote_config::RemoteConfig::default()
        ));
        assert!(!remote_listener_allowed(
            crate::remote_config::RemoteConfig {
                enabled: true,
                port: 0
            }
        ));
    }

    /// Points the tailnet listener at a `tailscale` that does not exist (so
    /// `whoami` fails with `NotInstalled`) and at a scratch certificate dir
    /// while it lives, holding the crate's env lock; dropping it restores
    /// both variables before the lock is released (fields drop in order).
    struct FakeTailscaleEnv {
        _vars: [crate::env_secrets::test_support::EnvVarGuard; 2],
        _lock: tokio::sync::MutexGuard<'static, ()>,
    }

    impl FakeTailscaleEnv {
        async fn new(scratch: &Path) -> Self {
            Self::with_tailnet_dir(scratch, &scratch.join("tailnet")).await
        }

        /// The same, with the certificate dir at `tailnet_dir`.
        async fn with_tailnet_dir(scratch: &Path, tailnet_dir: &Path) -> Self {
            use crate::env_secrets::test_support::EnvVarGuard;
            let lock = crate::env_secrets::test_support::ENV_LOCK.lock().await;
            let vars = [
                EnvVarGuard::set(
                    "RUSTLING_TULIP_TAILSCALE",
                    scratch.join("no-such-tailscale.exe"),
                ),
                EnvVarGuard::set("RUSTLING_TULIP_TAILNET_DIR", tailnet_dir),
            ];
            Self {
                _vars: vars,
                _lock: lock,
            }
        }
    }

    /// The next `RemoteStatus` broadcast on `events` that `pred` accepts,
    /// skipping every other event.
    async fn next_remote_status(
        events: &mut broadcast::Receiver<StateEvent>,
        mut pred: impl FnMut(bool, u16, &protocol::RemoteState) -> bool,
    ) -> (bool, u16, protocol::RemoteState) {
        // A hang guard on the real clock: a missing broadcast fails here as
        // hung; every awaited one is a local event.
        tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                if let StateEvent::RemoteStatus {
                    enabled,
                    port,
                    state,
                } = events
                    .recv()
                    .await
                    .expect("the state event channel stays open")
                    && pred(enabled, port, &state)
                {
                    return (enabled, port, state);
                }
            }
        })
        .await
        .expect("hung: the awaited RemoteStatus was never broadcast")
    }

    #[tokio::test]
    async fn tailnet_configure_remote_is_refused_and_writes_nothing() {
        let scratch = ScratchDir::new("configure-remote-tailnet");
        // A regression of the refusal must never reach a real Tailscale.
        // Declared before the hub, so the env vars are restored only
        // after the hub and its listener are gone.
        let _env = FakeTailscaleEnv::new(scratch.path()).await;
        let (hub, _scratch) = test_hub_in(scratch, None);
        let remote_file = hub.dirs.remote_config_file.clone();
        assert!(!remote_file.exists());
        let mut events = hub.state_events.subscribe();
        let tailnet = AuthKind::Tailnet(test_peer());
        let reply = dispatch_one_as(
            &hub,
            ClientMessage::ConfigureRemote {
                enabled: true,
                port: Some(9000),
            },
            &tailnet,
        )
        .await;
        assert!(
            matches!(&reply, DaemonMessage::Error { message, .. } if message == "remote access is local-only"),
            "{reply:?}"
        );
        assert!(
            !remote_file.exists(),
            "a refused ConfigureRemote writes no remote.json"
        );
        assert!(!*hub.remote_enabled.borrow(), "the switch stays off");
        assert!(hub.tailnet.lock().await.is_none(), "no listener starts");
        assert!(
            matches!(
                events.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "nothing is broadcast"
        );
    }

    #[tokio::test]
    async fn local_configure_remote_on_persists_starts_and_broadcasts() {
        let scratch = ScratchDir::new("configure-remote-on");
        // Declared before the hub, so the env vars are restored only
        // after the hub and its listener are gone.
        let _env = FakeTailscaleEnv::new(scratch.path()).await;
        let (hub, _scratch) = test_hub_in(scratch, None);
        let mut events = hub.state_events.subscribe();
        let sent = dispatch_all(
            &hub,
            ClientMessage::ConfigureRemote {
                enabled: true,
                port: None,
            },
        )
        .await;
        assert!(sent.is_empty(), "the answer is a broadcast: {sent:?}");

        let saved: crate::remote_config::RemoteConfig = serde_json::from_slice(
            &std::fs::read(&hub.dirs.remote_config_file).expect("remote.json is written"),
        )
        .expect("remote.json parses");
        assert!(saved.enabled);
        assert_eq!(saved.port, crate::remote_config::DEFAULT_REMOTE_PORT);
        assert!(*hub.remote_enabled.borrow(), "idle exit sees the switch on");
        assert!(hub.tailnet.lock().await.is_some(), "the listener runs");

        let (enabled, port, _) = next_remote_status(&mut events, |_, _, _| true).await;
        assert!(enabled);
        assert_eq!(port, crate::remote_config::DEFAULT_REMOTE_PORT);
        let (_, _, state) = next_remote_status(&mut events, |_, _, state| {
            *state != protocol::RemoteState::Starting
        })
        .await;
        assert_eq!(
            state,
            protocol::RemoteState::NotInstalled,
            "the listener ran the fake (missing) tailscale, never a real one"
        );
    }

    #[tokio::test]
    async fn local_configure_remote_off_persists_stops_and_broadcasts_off() {
        let scratch = ScratchDir::new("configure-remote-off");
        // Declared before the hub, so the env vars are restored only
        // after the hub and its listener are gone.
        let _env = FakeTailscaleEnv::new(scratch.path()).await;
        let (hub, _scratch) = test_hub_in(scratch, None);
        let on = ClientMessage::ConfigureRemote {
            enabled: true,
            port: None,
        };
        dispatch_all(&hub, on).await;
        assert!(hub.tailnet.lock().await.is_some());
        let mut events = hub.state_events.subscribe();

        let off = ClientMessage::ConfigureRemote {
            enabled: false,
            port: None,
        };
        dispatch_all(&hub, off).await;
        let saved = crate::remote_config::load(&hub.dirs);
        assert!(hub.dirs.remote_config_file.exists());
        assert!(!saved.enabled, "remote.json says off");
        assert!(
            !*hub.remote_enabled.borrow(),
            "idle exit sees the switch off"
        );
        assert!(
            hub.tailnet.lock().await.is_none(),
            "the listener is dropped"
        );
        let (_, _, state) = next_remote_status(&mut events, |enabled, _, _| !enabled).await;
        assert_eq!(state, protocol::RemoteState::Off);
        assert_eq!(
            hub.remote_status.borrow().snapshot.state,
            protocol::RemoteState::Off
        );
    }

    /// A life under the hub's own listener, as a connection it accepted
    /// carries: it ends when that listener is dropped.
    async fn listener_life(hub: &Hub) -> tokio_util::sync::CancellationToken {
        hub.tailnet
            .lock()
            .await
            .as_ref()
            .expect("remote access on starts a listener")
            .server_life_for_tests()
    }

    /// Read `client` until a message `stop` accepts.
    async fn read_until(client: &mut WsClient, stop: impl Fn(&DaemonMessage) -> bool) {
        loop {
            let message = ws_next(client)
                .await
                .expect("a message before the connection closes");
            if stop(&message) {
                return;
            }
        }
    }

    fn configure(enabled: bool) -> ClientMessage {
        ClientMessage::ConfigureRemote {
            enabled,
            port: None,
        }
    }

    #[tokio::test]
    async fn waiting_pairing_device_is_told_expired_when_remote_turns_off() {
        let scratch = ScratchDir::new("remote-off-pairing-expired");
        let _env = FakeTailscaleEnv::new(scratch.path()).await;
        let (hub, _scratch) = test_hub_in(scratch, None);
        dispatch_all(&hub, configure(true)).await;
        let life = listener_life(&hub).await;
        let mut device = tailnet_ws_client_under(&hub, test_peer(), life).await;
        ws_send(&mut device, &hello("", None)).await;
        let welcome = ws_next(&mut device).await;
        assert!(
            matches!(welcome, Some(DaemonMessage::Welcome { .. })),
            "{welcome:?}"
        );
        let refused = ws_next(&mut device).await;
        assert!(
            matches!(refused, Some(DaemonMessage::DevicePairFailed { .. })),
            "{refused:?}"
        );
        ws_send(&mut device, &ClientMessage::DevicePairRequest).await;
        let request_id = match ws_next(&mut device).await {
            Some(DaemonMessage::DevicePairPending { request_id, .. }) => Some(request_id),
            _ => None,
        }
        .expect("the request is pending");

        dispatch_all(&hub, configure(false)).await;
        let told = ws_next(&mut device).await;
        assert!(
            matches!(&told, Some(DaemonMessage::DevicePairDecided { request_id: id, outcome })
                if *id == request_id && *outcome == protocol::DevicePairOutcome::Expired),
            "the device is told its request expired: {told:?}"
        );
        assert!(ws_next(&mut device).await.is_none(), "then it is closed");
    }

    #[tokio::test]
    async fn loopback_session_survives_remote_access_turning_off() {
        let scratch = ScratchDir::new("loopback-survives-remote-off");
        let _env = FakeTailscaleEnv::new(scratch.path()).await;
        let (hub, _scratch) = test_hub_in(scratch, None);
        dispatch_all(&hub, configure(true)).await;
        let life = listener_life(&hub).await;
        let mut local = loopback_ws_client(&hub).await;
        ws_send(&mut local, &hello("test-token", None)).await;
        read_until(&mut local, |m| {
            matches!(m, DaemonMessage::RemoteStatus { .. })
        })
        .await;

        dispatch_all(&hub, configure(false)).await;
        assert!(
            life.is_cancelled(),
            "turning remote off dropped the listener"
        );

        let request = ClientMessage::ListEnvNames {
            request_id: Some("after-off".to_owned()),
        };
        ws_send(&mut local, &request).await;
        read_until(&mut local, |m| {
            matches!(m, DaemonMessage::EnvNames { request_id: Some(id), .. } if id == "after-off")
        })
        .await;
    }

    #[tokio::test]
    async fn a_repeated_configure_remote_rewrites_nothing_and_rebroadcasts() {
        let scratch = ScratchDir::new("configure-remote-repeat");
        // Declared before the hub, so the env vars are restored only
        // after the hub and its listener are gone.
        let _env = FakeTailscaleEnv::new(scratch.path()).await;
        let (hub, _scratch) = test_hub_in(scratch, None);
        let on = || ClientMessage::ConfigureRemote {
            enabled: true,
            port: None,
        };
        dispatch_all(&hub, on()).await;
        // Same switch and port as saved; the marker shows whether a second
        // request rewrites the file.
        let marked = br#"{"enabled":true,"port":8787,"marker":1}"#;
        std::fs::write(&hub.dirs.remote_config_file, marked).expect("mark remote.json");
        let generation = hub.remote_status.borrow().generation;
        let mut events = hub.state_events.subscribe();

        dispatch_all(&hub, on()).await;
        assert_eq!(
            std::fs::read(&hub.dirs.remote_config_file).expect("remote.json is still there"),
            marked,
            "an identical request does not rewrite remote.json"
        );
        assert_eq!(
            hub.remote_status.borrow().generation,
            generation,
            "the running listener is kept"
        );
        assert!(hub.tailnet.lock().await.is_some());
        assert!(*hub.remote_enabled.borrow());
        let (enabled, port, _) = next_remote_status(&mut events, |_, _, _| true).await;
        assert!(enabled, "the current status is re-broadcast");
        assert_eq!(port, crate::remote_config::DEFAULT_REMOTE_PORT);
    }

    #[tokio::test]
    async fn a_stale_remote_relay_never_overwrites_off() {
        let (hub, _scratch) = test_hub("remote-stale-relay");
        let on = crate::remote_config::RemoteConfig {
            enabled: true,
            port: 8787,
        };
        let generation = begin_remote_generation(&hub, on, protocol::RemoteState::Starting);
        let (old_status, status_rx) = tokio::sync::watch::channel(ListenerStatus::Starting);
        let relay = RemoteRelay {
            remote_status: Arc::clone(&hub.remote_status),
            state_events: hub.state_events.clone(),
            generation,
            port: 8787,
        }
        .spawn(status_rx, name_slot());

        dispatch_all(
            &hub,
            ClientMessage::ConfigureRemote {
                enabled: false,
                port: None,
            },
        )
        .await;
        let mut events = hub.state_events.subscribe();
        // The old listener's watch fires after the switch went off, then ends.
        old_status.send_replace(ListenerStatus::TailscaleDown(
            tailnet::TailnetError::NotRunning,
        ));
        drop(old_status);
        relay.await.expect("the relay task ends with its watch");

        assert_eq!(
            hub.remote_status.borrow().snapshot.state,
            protocol::RemoteState::Off,
            "the last status stays Off"
        );
        assert!(
            matches!(
                events.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "the stale status is never broadcast"
        );
    }

    #[test]
    fn a_new_client_gets_remote_status_at_connect() {
        let (hub, _scratch) = test_hub("remote-status-connect");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        push_initial_state(&hub, &out_tx, "c1", false);
        let mut remote = Vec::new();
        while let Ok(message) = out_rx.try_recv() {
            if let DaemonMessage::RemoteStatus {
                enabled,
                port,
                state,
            } = message
            {
                remote.push((enabled, port, state));
            }
        }
        assert_eq!(
            remote,
            [(
                false,
                crate::remote_config::DEFAULT_REMOTE_PORT,
                protocol::RemoteState::Off
            )]
        );
    }
}
