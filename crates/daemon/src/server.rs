//! WebSocket server, message dispatch, and session orchestration glue.

use crate::orphan::{self, OrphanMeta};
use crate::paths::Dirs;
use crate::pty::PtySpawnSpec;
use crate::registry::{
    add_repo, persist_last_agent, persist_repo_last_spawn_config,
    persist_workspace_last_spawn_config, remove_repo, remove_workspace, reorder_containers,
    set_repo_worktree_default, set_session_order, set_workspace_worktree_default, upsert_workspace,
};
use crate::scrollback;
use crate::session::{
    SessionEvent, SessionRecord, SessionRegistry, attach_lifecycle, new_id, push_recent_action,
};
use crate::state::AppState;
use crate::tabs;
use crate::tracer_client;
use crate::{
    git, git_inspect, git_write, headless, inject, osc_title, pty_state, vscode, workspace as ws,
};
use anyhow::{Context as _, anyhow};
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use chrono::Utc;
use futures::{SinkExt as _, StreamExt as _};
use protocol::{
    Agent, AttentionReason, ClientMessage, CodexSandbox, DaemonHandshake, DaemonMessage,
    InboundClientMessage, PROTOCOL_VERSION, PaneDropEdge, PermissionMode,
    SUPPORTED_PROTOCOL_VERSIONS, SessionKind, SessionMember, SessionMetrics, SessionMode,
    SessionStatus, SpawnRequest, SpawnTarget, TabContent, TabEntry, VscodeWorkspaceSuggestion,
};
use rand::Rng as _;
use rand::distributions::Alphanumeric;
use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

#[derive(Clone)]
pub struct Hub {
    pub state: Arc<AppState>,
    pub sessions: Arc<SessionRegistry>,
    pub auth_token: String,
    pub attention_tx: mpsc::UnboundedSender<pty_state::AttentionEvent>,
    pub dirs: Dirs,
    /// Set to `true` by the [`ClientMessage::Shutdown`] handler; axum's
    /// graceful-shutdown future awaits this and returns once flipped, after
    /// which the daemon process exits cleanly.
    pub shutdown_tx: tokio::sync::watch::Sender<bool>,
    /// Broadcast for tab-layout changes. Mirrors the [`SessionRegistry`]
    /// event channel: every connected client subscribes and forwards events
    /// to its WebSocket sender. Buffered to keep up with bursty drag-reorder
    /// or split-pane sequences.
    pub tab_events: broadcast::Sender<TabEvent>,
    /// Broadcast for preset-launch progress/failure events. Every connected
    /// client subscribes and forwards. Lets the launching client auto-switch
    /// tabs while the batch runs, and lets other clients see new tabs appear.
    pub preset_events: broadcast::Sender<PresetEvent>,
    /// Broadcast for repo + workspace registry mutations. Every connected
    /// client subscribes and forwards full `Repos` / `Workspaces` snapshots
    /// to its WS sender. Without this, a registry change made on one client
    /// (e.g. `add_repo` from the side-channel or a pop-out window) was never
    /// visible to other clients until they reconnected. See ux-audit:
    /// "Daemon repo/workspace updates are NOT broadcast to all clients."
    pub state_events: broadcast::Sender<StateEvent>,
    /// Number of currently-attached WS clients. Maintained by
    /// [`ClientCountGuard`] in `client_session` (RAII inc/dec) and consumed
    /// by `git_watch` to park its per-repo refreshers while no UI is
    /// listening — the broadcasts would just be dropped on the floor and
    /// every event would cost a couple of `git` subprocesses for nothing.
    pub client_count: Arc<tokio::sync::watch::Sender<usize>>,
}

/// RAII guard around [`Hub::client_count`]: increments on construction,
/// decrements on drop. Drop runs on normal return and panic alike, so the
/// count stays consistent even if a client task unwinds.
struct ClientCountGuard {
    counter: Arc<tokio::sync::watch::Sender<usize>>,
}

impl ClientCountGuard {
    fn new(counter: Arc<tokio::sync::watch::Sender<usize>>) -> Self {
        counter.send_modify(|c| *c += 1);
        Self { counter }
    }
}

impl Drop for ClientCountGuard {
    fn drop(&mut self) {
        self.counter.send_modify(|c| *c = c.saturating_sub(1));
    }
}

#[derive(Debug, Clone)]
pub enum StateEvent {
    Repos(Vec<protocol::RepoEntry>),
    Workspaces(Vec<protocol::WorkspaceEntry>),
    /// Manual sidebar container order. Broadcast on every registry
    /// mutation that might affect the persisted order (add/remove/
    /// reorder) so connected clients stay in sync.
    ContainersReordered(Vec<protocol::ContainerRef>),
    /// Per-container session display order. Broadcast after a successful
    /// `ReorderSessions` so all connected windows stay in sync.
    SessionsReordered {
        container_id: String,
        ordered_ids: Vec<String>,
    },
    /// Broadcast after a successful stage/unstage/commit/discard so every
    /// connected source-control sidebar refreshes without each window having
    /// to re-request explicitly.
    RepoStatus {
        repo_id: String,
        index_changes: Vec<protocol::GitFileChange>,
        worktree_changes: Vec<protocol::GitFileChange>,
    },
    /// Broadcast after a successful stash push/pop/drop so the sidebar's
    /// STASHES section stays in sync across multiple client windows.
    Stashes {
        repo_id: String,
        stashes: Vec<protocol::GitStash>,
    },
}

#[derive(Debug, Clone)]
pub enum TabEvent {
    Updated(TabEntry),
    Removed(String),
    Reordered(Vec<String>),
}

#[derive(Debug, Clone)]
pub enum PresetEvent {
    Progress {
        preset_id: String,
        total: u32,
        launched: u32,
        current_tab_id: Option<String>,
        tab_ids: Vec<String>,
    },
    Failed {
        preset_id: String,
        error: String,
        partial_session_ids: Vec<String>,
        partial_tab_ids: Vec<String>,
    },
}

const TAB_EVENT_CAPACITY: usize = 256;
const PRESET_EVENT_CAPACITY: usize = 64;
const STATE_EVENT_CAPACITY: usize = 64;

/// Reattach orphan sessions (alive but detached) and abandoned sessions
/// (dead, daemon crashed mid-run) before any clients connect so the
/// initial state snapshot already includes them.
///
/// Post-C.3: orphans whose sidecar carries `tracer_pid` + `tracer_pipe` get
/// wired up to the still-running rt-tracer via pipe, restoring full IO.
/// A reattach failure (tracer died between liveness check and pipe open,
/// or another daemon claimed the pipe first) routes the session to the
/// abandoned bucket so the user can Resume from B.2's UI. Legacy sidecars
/// (no tracer fields) fall through to the read-only orphan path — pre-C.3
/// children can't survive the daemon anyway, so this branch is only ever
/// taken by sidecars from a downgrade scenario.
async fn reattach_orphans(
    sessions: &Arc<SessionRegistry>,
    dirs: &Dirs,
    attention_tx: &mpsc::UnboundedSender<pty_state::AttentionEvent>,
    orphans: Vec<OrphanMeta>,
    abandoned: Vec<OrphanMeta>,
) {
    for meta in orphans {
        match (meta.tracer_pipe.as_deref(), meta.tracer_pid) {
            (Some(pipe), Some(tracer_pid)) => {
                match tracer_client::reattach(
                    &meta.session_id,
                    pipe,
                    tracer_pid,
                    expected_output_subscribers(meta.mode),
                )
                .await
                {
                    Ok(pty) => {
                        info!(
                            session_id = %meta.session_id,
                            tracer_pid,
                            "tracer reattach succeeded"
                        );
                        sessions.insert_reattached(&meta, Arc::clone(&pty));
                        attach_lifecycle(
                            sessions,
                            meta.session_id.clone(),
                            &pty,
                            Some(dirs.clone()),
                        );
                        if meta.mode != SessionMode::PlainShell {
                            let (in_tx, in_rx) = mpsc::unbounded_channel::<usize>();
                            if let Some(arc) = sessions.get(&meta.session_id) {
                                crate::sync::lock(&arc).input_notifier = Some(in_tx);
                            }
                            pty_state::watch(
                                sessions,
                                meta.session_id.clone(),
                                pty.output.subscribe(),
                                in_rx,
                                attention_tx.clone(),
                            );
                        }
                        osc_title::watch(
                            sessions,
                            meta.session_id.clone(),
                            pty.output.subscribe(),
                            dirs.clone(),
                        );
                    }
                    Err(err) => {
                        warn!(
                            ?err,
                            session_id = %meta.session_id,
                            tracer_pid,
                            "tracer reattach failed; routing to abandoned"
                        );
                        sessions.insert_abandoned(&meta);
                    }
                }
            }
            _ => {
                sessions.insert_orphan(&meta);
            }
        }
    }
    for meta in abandoned {
        sessions.insert_abandoned(&meta);
    }
}

pub async fn run(
    state: Arc<AppState>,
    dirs: Dirs,
    orphans: Vec<OrphanMeta>,
    abandoned: Vec<OrphanMeta>,
) -> anyhow::Result<()> {
    let auth_token: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(48)
        .map(char::from)
        .collect();

    let (attention_tx, mut attention_rx) = mpsc::unbounded_channel::<pty_state::AttentionEvent>();
    let sessions = SessionRegistry::new(dirs.clone());

    reattach_orphans(&sessions, &dirs, &attention_tx, orphans, abandoned).await;

    // Forward attention events through the registry's broadcast so all
    // attached clients see them via the same SessionEvent channel they
    // already subscribe to.
    let sessions_for_attention = Arc::clone(&sessions);
    tokio::spawn(async move {
        while let Some(evt) = attention_rx.recv().await {
            sessions_for_attention.fan_out_attention(evt.session_id, evt.reason);
        }
    });

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let (tab_events, _) = broadcast::channel(TAB_EVENT_CAPACITY);
    let (preset_events, _) = broadcast::channel(PRESET_EVENT_CAPACITY);
    let (state_events, _) = broadcast::channel(STATE_EVENT_CAPACITY);
    let (client_count_tx, client_count_rx) = tokio::sync::watch::channel(0usize);
    let client_count = Arc::new(client_count_tx);

    // Per-repo filesystem watchers that keep the source-control sidebar
    // live without user-triggered refreshes. The supervisor task owns the
    // per-repo debouncer handles and listens on `state_events` for repo
    // add/remove broadcasts, so adding a repo at runtime spawns a watcher
    // and removing one stops it. Refreshers park while `client_count` is
    // 0 — see `crates/daemon/src/git_watch.rs`.
    crate::git_watch::start(&state, &state_events, client_count_rx);

    let hub = Hub {
        state,
        sessions,
        auth_token: auth_token.clone(),
        attention_tx,
        dirs: dirs.clone(),
        shutdown_tx,
        tab_events,
        preset_events,
        state_events,
        client_count,
    };

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/health", get(|| async { "ok" }))
        .route("/shutdown", post(shutdown_handler))
        .with_state(hub);

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("binding loopback listener")?;
    let bound = listener.local_addr().context("local addr")?;
    info!(port = bound.port(), "rustling-tulipd listening");

    write_handshake(&dirs, bound.port(), &auth_token)?;

    let shutdown_signal = async move {
        // Returns once the Shutdown handler flips the watch to `true`. A
        // `recv_error` means the sender was dropped (programmer error) — fall
        // through and let axum exit naturally rather than panicking.
        while shutdown_rx.changed().await.is_ok() {
            if *shutdown_rx.borrow() {
                info!("shutdown signal: watch flipped true; stopping accept loop");
                break;
            }
        }
        info!("shutdown signal: future complete");
    };
    info!("server::run: entering axum::serve");
    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal)
        .await
        .context("axum serve failed");
    info!(?serve_result, "server::run: axum::serve returned");
    // Best-effort cleanup: remove the discovery file so a subsequent app
    // launch doesn't try to reconnect to a defunct port.
    let _ = std::fs::remove_file(&dirs.handshake_file);
    info!("server::run: discovery file removed; returning");
    serve_result
}

fn write_handshake(dirs: &Dirs, port: u16, auth_token: &str) -> anyhow::Result<()> {
    let pid = std::process::id();
    let payload = DaemonHandshake {
        protocol_version: PROTOCOL_VERSION,
        port,
        auth_token: auth_token.to_string(),
        pid,
    };
    let bytes = serde_json::to_vec_pretty(&payload).context("serializing handshake")?;
    // Tag the tmp file with our pid so concurrent daemons don't stomp each
    // other's mid-write file (which would lose one daemon's atomic rename
    // when the other consumed the shared tmp). Two daemons can briefly
    // co-exist during wdio test sessions; this lets both write+rename
    // independently without colliding.
    let tmp = dirs
        .handshake_file
        .with_extension(format!("json.tmp.{pid}"));
    std::fs::write(&tmp, &bytes).context("writing handshake tmp")?;
    std::fs::rename(&tmp, &dirs.handshake_file).context("renaming handshake")?;
    Ok(())
}

async fn ws_handler(State(hub): State<Hub>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| client_session(hub, socket))
}

/// Out-of-band graceful-shutdown trigger. Mirrors the WS
/// `ClientMessage::Shutdown { drain: false }` path so an installer (or any
/// other non-WS caller) can take the daemon down cleanly without dropping
/// tracers — sidecars + scrollback stay on disk and a freshly-spawned daemon
/// reattaches every still-running supervisor.
///
/// Auth: requires `Authorization: Bearer <auth_token>` matching the daemon's
/// in-memory token (the same one written to `daemon.json`). Without a valid
/// token the endpoint returns 401 — anyone with read access to the config
/// dir can already shut us down via `taskkill`, but we still gate the soft
/// path so a stray local script can't trip it.
async fn shutdown_handler(State(hub): State<Hub>, headers: HeaderMap) -> Response {
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    let Some(token) = presented else {
        return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response();
    };
    if token != hub.auth_token {
        return (StatusCode::UNAUTHORIZED, "bad bearer token").into_response();
    }

    info!("HTTP /shutdown: graceful shutdown requested (no drain)");
    // Mirrors the WS handler: don't drain — leave sidecars + scrollback on
    // disk so the next daemon start can surface these sessions in the
    // Abandoned bucket (or, when tracers are still alive, reattach them).
    let send_result = hub.shutdown_tx.send(true);
    info!(?send_result, "HTTP /shutdown: shutdown_tx.send returned");
    (StatusCode::ACCEPTED, "shutting down").into_response()
}

async fn client_session(hub: Hub, socket: WebSocket) {
    // RAII-tracked so `git_watch` can pause its refreshers when no UI is
    // listening. Dropped on normal return and on panic.
    let _client_guard = ClientCountGuard::new(Arc::clone(&hub.client_count));
    let (mut sender, mut receiver) = socket.split();

    // Each outgoing message goes through this channel so PTY/event tasks can
    // push to the WS without sharing the sink across tasks.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<DaemonMessage>();

    // Sender pump.
    let send_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let json = match serde_json::to_string(&msg) {
                Ok(s) => s,
                Err(err) => {
                    error!(?err, "serializing daemon msg");
                    continue;
                }
            };
            if sender.send(Message::Text(json.into())).await.is_err() {
                break;
            }
        }
    });

    // Mandatory handshake.
    let _negotiated_version = match handshake(&hub, &mut receiver, &out_tx).await {
        Ok(v) => v,
        Err(err) => {
            warn!(?err, "client handshake failed");
            let _ = out_tx.send(DaemonMessage::AuthFailed {
                reason: err.to_string(),
            });
            drop(out_tx);
            let _ = send_task.await;
            return;
        }
    };

    let attached = Arc::new(AsyncMutex::new(HashSet::<String>::new()));

    // Subscribe to global session events; forward only those relevant to this
    // client (registry-wide updates always; PTY only for attached sessions).
    let mut events_rx = hub.sessions.subscribe();
    let attached_for_events = Arc::clone(&attached);
    let out_for_events = out_tx.clone();
    let event_task = tokio::spawn(async move {
        loop {
            match events_rx.recv().await {
                Ok(SessionEvent::Updated(snap)) => {
                    let _ = out_for_events.send(DaemonMessage::SessionUpdated { session: *snap });
                }
                Ok(SessionEvent::Removed(id)) => {
                    let _ = out_for_events.send(DaemonMessage::SessionRemoved { session_id: id });
                }
                Ok(SessionEvent::PtyOutput { session_id, data }) => {
                    if attached_for_events.lock().await.contains(&session_id) {
                        let data_b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                        let _ = out_for_events.send(DaemonMessage::PtyOutput {
                            session_id,
                            data_b64,
                        });
                    }
                }
                Ok(SessionEvent::Attention { session_id, reason }) => {
                    let _ = out_for_events.send(DaemonMessage::Attention { session_id, reason });
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(lagged = n, "client event stream lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Subscribe to tab-layout and preset-launch events; both forwarded
    // unconditionally to every connected client.
    let tab_event_task = spawn_tab_forwarder(hub.tab_events.subscribe(), out_tx.clone());
    let preset_event_task = spawn_preset_forwarder(hub.preset_events.subscribe(), out_tx.clone());
    let state_event_task = spawn_state_forwarder(hub.state_events.subscribe(), out_tx.clone());

    // Send initial state snapshots.
    push_initial_state(&hub, &out_tx);

    recv_loop(&hub, &mut receiver, &out_tx, &attached).await;

    info!("client_session: recv_loop returned; tearing down");
    drop(out_tx);
    event_task.abort();
    tab_event_task.abort();
    preset_event_task.abort();
    state_event_task.abort();
    let _ = send_task.await;
    info!("client_session: returning");
}

fn spawn_tab_forwarder(
    mut rx: broadcast::Receiver<TabEvent>,
    out_tx: mpsc::UnboundedSender<DaemonMessage>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(TabEvent::Updated(tab)) => {
                    let _ = out_tx.send(DaemonMessage::TabUpdated { tab });
                }
                Ok(TabEvent::Removed(tab_id)) => {
                    let _ = out_tx.send(DaemonMessage::TabRemoved { tab_id });
                }
                Ok(TabEvent::Reordered(ids)) => {
                    let _ = out_tx.send(DaemonMessage::TabsReordered { ordered_ids: ids });
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(lagged = n, "client tab event stream lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

fn spawn_state_forwarder(
    mut rx: broadcast::Receiver<StateEvent>,
    out_tx: mpsc::UnboundedSender<DaemonMessage>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(StateEvent::Repos(repos)) => {
                    let _ = out_tx.send(DaemonMessage::Repos { repos });
                }
                Ok(StateEvent::Workspaces(workspaces)) => {
                    let _ = out_tx.send(DaemonMessage::Workspaces { workspaces });
                }
                Ok(StateEvent::ContainersReordered(ordered)) => {
                    let _ = out_tx.send(DaemonMessage::ContainersReordered { ordered });
                }
                Ok(StateEvent::SessionsReordered {
                    container_id,
                    ordered_ids,
                }) => {
                    let _ = out_tx.send(DaemonMessage::SessionsReordered {
                        container_id,
                        ordered_ids,
                    });
                }
                Ok(StateEvent::RepoStatus {
                    repo_id,
                    index_changes,
                    worktree_changes,
                }) => {
                    let _ = out_tx.send(DaemonMessage::RepoStatus {
                        repo_id,
                        index_changes,
                        worktree_changes,
                    });
                }
                Ok(StateEvent::Stashes { repo_id, stashes }) => {
                    let _ = out_tx.send(DaemonMessage::Stashes { repo_id, stashes });
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(lagged = n, "client state event stream lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

fn spawn_preset_forwarder(
    mut rx: broadcast::Receiver<PresetEvent>,
    out_tx: mpsc::UnboundedSender<DaemonMessage>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(PresetEvent::Progress {
                    preset_id,
                    total,
                    launched,
                    current_tab_id,
                    tab_ids,
                }) => {
                    let _ = out_tx.send(DaemonMessage::PresetLaunchProgress {
                        preset_id,
                        total,
                        launched,
                        current_tab_id,
                        tab_ids,
                    });
                }
                Ok(PresetEvent::Failed {
                    preset_id,
                    error,
                    partial_session_ids,
                    partial_tab_ids,
                }) => {
                    let _ = out_tx.send(DaemonMessage::PresetLaunchFailed {
                        preset_id,
                        error,
                        partial_session_ids,
                        partial_tab_ids,
                    });
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(lagged = n, "client preset event stream lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Run the WebSocket receive loop until the peer closes or the daemon-wide
/// shutdown flag flips. Honoring `shutdown_tx` here is what lets axum's
/// graceful-shutdown future resolve after a [`ClientMessage::Shutdown`] —
/// otherwise every `recv()` blocks forever and the daemon hangs.
async fn recv_loop(
    hub: &Hub,
    receiver: &mut futures::stream::SplitStream<WebSocket>,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    attached: &Arc<AsyncMutex<HashSet<String>>>,
) {
    let mut shutdown_rx = hub.shutdown_tx.subscribe();
    loop {
        tokio::select! {
            biased;
            res = shutdown_rx.changed() => {
                if res.is_err() {
                    info!("recv_loop: shutdown_rx closed; exiting");
                    break;
                }
                if *shutdown_rx.borrow_and_update() {
                    info!("recv_loop: observed daemon shutdown signal; exiting");
                    break;
                }
            }
            next = receiver.next() => {
                let Some(Ok(msg)) = next else {
                    info!("recv_loop: peer closed websocket; exiting");
                    break;
                };
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Close(_) => break,
                    Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => continue,
                };
                let parsed = match InboundClientMessage::from_json_str(&text) {
                    Ok(p) => p,
                    Err(err) => {
                        let _ = out_tx.send(DaemonMessage::Error {
                            message: format!("malformed message: {err}"),
                        });
                        continue;
                    }
                };
                let parsed = match parsed {
                    InboundClientMessage::Known(msg) => msg,
                    InboundClientMessage::Unknown { type_tag, .. } => {
                        warn!(
                            %type_tag,
                            "ignoring unknown client message type (forward-compat path)"
                        );
                        continue;
                    }
                };
                if let Err(err) = dispatch(hub, parsed, out_tx, attached).await {
                    let _ = out_tx.send(DaemonMessage::Error {
                        message: err.to_string(),
                    });
                }
            }
        }
    }
}

async fn handshake(
    hub: &Hub,
    receiver: &mut futures::stream::SplitStream<WebSocket>,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) -> anyhow::Result<u32> {
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
    }) = parsed
    else {
        return Err(anyhow!("first message must be Hello"));
    };
    if auth_token != hub.auth_token {
        return Err(anyhow!("invalid auth token"));
    }
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
        "handshake negotiated protocol version"
    );
    let _ = out_tx.send(DaemonMessage::Welcome {
        protocol_version: negotiated,
        supported_versions: SUPPORTED_PROTOCOL_VERSIONS.to_vec(),
    });
    Ok(negotiated)
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

fn push_initial_state(hub: &Hub, out_tx: &mpsc::UnboundedSender<DaemonMessage>) {
    let (repos, workspaces, tabs, container_order, session_order) = hub.state.with_persisted(|s| {
        (
            s.repos.clone(),
            s.workspaces.clone(),
            s.tabs.clone(),
            s.container_order.clone(),
            s.session_order.clone(),
        )
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
    let _ = out_tx.send(DaemonMessage::Sessions {
        sessions: hub.sessions.snapshots(),
    });
    let _ = out_tx.send(DaemonMessage::Tabs { tabs });
}

#[expect(
    clippy::too_many_lines,
    reason = "dispatch is a flat match over the protocol message set; \
              splitting by category would add indirection without clarity"
)]
async fn dispatch(
    hub: &Hub,
    msg: ClientMessage,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    attached: &Arc<AsyncMutex<HashSet<String>>>,
) -> anyhow::Result<()> {
    match msg {
        ClientMessage::Hello { .. } => {
            // Already consumed by handshake; ignore subsequent ones.
        }
        ClientMessage::ListRepos => {
            let repos = hub.state.with_persisted(|s| s.repos.clone());
            let _ = out_tx.send(DaemonMessage::Repos { repos });
        }
        ClientMessage::AddRepo { path, name } => {
            let entry = add_repo(&hub.state, &path, name).await?;
            let (repos, container_order) = hub
                .state
                .with_persisted(|s| (s.repos.clone(), s.container_order.clone()));
            let _ = hub.state_events.send(StateEvent::Repos(repos));
            let _ = hub
                .state_events
                .send(StateEvent::ContainersReordered(container_order));
            for suggestion in scan_vscode_workspaces(hub, &entry.path) {
                // VSCode workspace suggestions are per-client (only the
                // adder gets prompted), so this stays on out_tx.
                let _ = out_tx.send(DaemonMessage::VscodeWorkspaceSuggestion {
                    repo_id: entry.id.clone(),
                    suggestion,
                });
            }
            debug!(repo_id = %entry.id, "repo added");
        }
        ClientMessage::RemoveRepo { repo_id } => {
            remove_repo(&hub.state, &repo_id)?;
            let (repos, workspaces, container_order) = hub.state.with_persisted(|s| {
                (
                    s.repos.clone(),
                    s.workspaces.clone(),
                    s.container_order.clone(),
                )
            });
            let _ = hub.state_events.send(StateEvent::Repos(repos));
            let _ = hub.state_events.send(StateEvent::Workspaces(workspaces));
            let _ = hub
                .state_events
                .send(StateEvent::ContainersReordered(container_order));
        }
        ClientMessage::ListWorkspaces => {
            let workspaces = hub.state.with_persisted(|s| s.workspaces.clone());
            let _ = out_tx.send(DaemonMessage::Workspaces { workspaces });
        }
        ClientMessage::UpsertWorkspace {
            id,
            name,
            member_repo_ids,
            linked_vscode_workspace,
        } => {
            upsert_workspace(
                &hub.state,
                id,
                &name,
                member_repo_ids,
                linked_vscode_workspace,
            )?;
            let (workspaces, container_order) = hub
                .state
                .with_persisted(|s| (s.workspaces.clone(), s.container_order.clone()));
            let _ = hub.state_events.send(StateEvent::Workspaces(workspaces));
            let _ = hub
                .state_events
                .send(StateEvent::ContainersReordered(container_order));
        }
        ClientMessage::RemoveWorkspace { workspace_id } => {
            remove_workspace(&hub.state, &workspace_id)?;
            let (workspaces, container_order) = hub
                .state
                .with_persisted(|s| (s.workspaces.clone(), s.container_order.clone()));
            let _ = hub.state_events.send(StateEvent::Workspaces(workspaces));
            let _ = hub
                .state_events
                .send(StateEvent::ContainersReordered(container_order));
        }
        ClientMessage::PreviewWorkspaceSpawn {
            workspace_id,
            branch_name,
            base_branch,
        } => {
            // Preview always reflects the worktree path; the in-place
            // affordance is hidden behind the use_worktree spawn flag and
            // doesn't change preview output meaningfully.
            let (_, resolved) = ws::resolve_workspace(
                &hub.state,
                &hub.dirs.worktrees_dir,
                &workspace_id,
                &branch_name,
                base_branch.as_deref(),
                true,
            )
            .await?;
            let _ = out_tx.send(DaemonMessage::WorkspaceSpawnPreview {
                workspace_id,
                branch_name,
                per_member: ws::previews(&resolved),
            });
        }
        ClientMessage::AcceptVscodeWorkspaceSuggestion { suggestion, watch } => {
            accept_vscode_suggestion(hub, suggestion, watch, out_tx).await?;
            // Broadcast container order in case new repos were registered.
            let container_order = hub.state.with_persisted(|s| s.container_order.clone());
            let _ = hub
                .state_events
                .send(StateEvent::ContainersReordered(container_order));
        }
        ClientMessage::ParseVscodeWorkspace { path } => {
            let known_repos: Vec<(String, String)> = hub.state.with_persisted(|s| {
                s.repos
                    .iter()
                    .map(|r| (r.id.clone(), r.path.clone()))
                    .collect()
            });
            let suggestion =
                crate::vscode::parse_workspace_file(std::path::Path::new(&path), &known_repos)
                    .map_err(|e| anyhow!("parse_vscode_workspace: {e}"))?;
            let _ = out_tx.send(DaemonMessage::VscodeWorkspaceParsed { suggestion });
        }
        ClientMessage::ListSessions => {
            let _ = out_tx.send(DaemonMessage::Sessions {
                sessions: hub.sessions.snapshots(),
            });
        }
        ClientMessage::SpawnSession(req) => {
            let snap = spawn_session(hub, req).await?;
            let _ = out_tx.send(DaemonMessage::SessionUpdated { session: snap });
        }
        ClientMessage::DuplicateSession { session_id } => {
            let stored = hub
                .sessions
                .get(&session_id)
                .map(|rec| crate::sync::lock(&rec).spawn_config.clone())
                .ok_or_else(|| anyhow!("unknown session: {session_id}"))?;
            let Some(stored) = stored else {
                return Err(anyhow!(
                    "session {session_id} has no stored spawn config; cannot duplicate"
                ));
            };
            let req = stored.to_clone_request();
            let snap = spawn_session(hub, req).await?;
            let _ = out_tx.send(DaemonMessage::SessionUpdated { session: snap });
        }
        ClientMessage::GetSpawnConfig { session_id } => {
            let config = hub
                .sessions
                .get(&session_id)
                .and_then(|rec| crate::sync::lock(&rec).spawn_config.clone());
            let _ = out_tx.send(DaemonMessage::SpawnConfigReply { session_id, config });
        }
        ClientMessage::RenameSession { session_id, label } => {
            // Empty / whitespace-only labels clear the user override and
            // restore the daemon-generated default. The user sees their
            // typed value drop straight to None — no surprise reuse of an
            // accidental space-padded name.
            let normalized = label.and_then(|raw| {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            });
            let mut labels = None;
            hub.sessions.update(&session_id, |guard| {
                guard.user_label.clone_from(&normalized);
                guard.label = normalized
                    .clone()
                    .unwrap_or_else(|| guard.default_label.clone());
                labels = Some((
                    guard.default_label.clone(),
                    guard.user_label.clone(),
                    guard.label.clone(),
                ));
            });
            if let Some((default_label, user_label, effective)) = labels {
                orphan::try_update_labels(
                    &hub.dirs,
                    &session_id,
                    &default_label,
                    user_label.as_deref(),
                    &effective,
                );
            }
        }
        ClientMessage::SetSessionColor { session_id, color } => {
            let color = normalize_session_color(color)?;
            let mut applied = None;
            hub.sessions.update(&session_id, |guard| {
                guard.accent_color.clone_from(&color);
                applied = Some(guard.accent_color.clone());
            });
            if let Some(color) = applied {
                orphan::try_update_accent_color(&hub.dirs, &session_id, color.as_deref());
            }
        }
        ClientMessage::Attach { session_id } => {
            attached.lock().await.insert(session_id);
        }
        ClientMessage::Detach { session_id } => {
            attached.lock().await.remove(&session_id);
        }
        ClientMessage::SendInput {
            session_id,
            data_b64,
        } => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data_b64.as_bytes())
                .context("decoding input b64")?;
            if let Some(rec) = hub.sessions.get(&session_id) {
                let (pty, notifier) = {
                    let guard = crate::sync::lock(&rec);
                    (guard.pty.clone(), guard.input_notifier.clone())
                };
                if let Some(pty) = pty {
                    if let Some(tx) = notifier {
                        let _ = tx.send(bytes.len());
                    }
                    pty.write_input(bytes);
                }
            }
        }
        ClientMessage::Resize {
            session_id,
            cols,
            rows,
        } => {
            if let Some(rec) = hub.sessions.get(&session_id) {
                let pty = crate::sync::lock(&rec).pty.clone();
                if let Some(pty) = pty {
                    pty.resize(cols, rows);
                }
            }
        }
        ClientMessage::StopSession {
            session_id,
            cleanup: _,
        } => {
            stop_session(hub, &session_id).await?;
            // Surface a one-shot Attention so connected clients can prompt.
            let _ = out_tx.send(DaemonMessage::Attention {
                session_id,
                reason: AttentionReason::Stopped,
            });
        }
        ClientMessage::ResumeAbandoned { session_id } => {
            resume_abandoned(hub, &session_id, out_tx).await?;
        }
        ClientMessage::DiscardAbandoned { session_id } => {
            discard_abandoned(hub, &session_id, out_tx);
        }
        ClientMessage::ParkSession { session_id } => {
            park_session(hub, &session_id).await;
        }
        ClientMessage::DiscardSession {
            session_id,
            cleanup,
        } => {
            discard_session(hub, &session_id, &cleanup).await;
        }
        ClientMessage::ResumeAllAbandoned => {
            resume_all_abandoned(hub, out_tx).await;
        }
        ClientMessage::ListBranches { repo_id } => {
            let repo_path = hub
                .state
                .with_persisted(|s| {
                    s.repos
                        .iter()
                        .find(|r| r.id == repo_id)
                        .map(|r| r.path.clone())
                })
                .ok_or_else(|| anyhow!("unknown repo: {repo_id}"))?;
            let path = PathBuf::from(&repo_path);
            let branches = git::list_branches(&path).await.unwrap_or_default();
            let current = git::current_branch(&path).await.unwrap_or(None);
            let _ = out_tx.send(DaemonMessage::Branches {
                repo_id,
                branches,
                current,
            });
        }
        ClientMessage::ListWorktrees { repo_id } => {
            let repo_path = hub
                .state
                .with_persisted(|s| {
                    s.repos
                        .iter()
                        .find(|r| r.id == repo_id)
                        .map(|r| r.path.clone())
                })
                .ok_or_else(|| anyhow!("unknown repo: {repo_id}"))?;
            let path = PathBuf::from(&repo_path);
            let raw = git::list_worktrees(&path).await.unwrap_or_default();
            // Mark worktrees currently in use by active (non-stopped, non-parked) sessions.
            let active_paths: std::collections::HashSet<String> = hub
                .sessions
                .snapshots()
                .into_iter()
                .filter(|s| {
                    !matches!(
                        s.status,
                        protocol::SessionStatus::Stopped | protocol::SessionStatus::Error
                    ) && !s.is_inactive
                })
                .flat_map(|s| s.worktree_paths)
                .collect();
            let worktrees = raw
                .into_iter()
                .map(|(branch, path)| {
                    let path_str = path.to_string_lossy().into_owned();
                    let is_active = active_paths.contains(&path_str);
                    protocol::WorktreeInfo {
                        branch,
                        path: path_str,
                        is_active,
                    }
                })
                .collect();
            let _ = out_tx.send(DaemonMessage::Worktrees { repo_id, worktrees });
        }
        ClientMessage::ListCommits {
            repo_id,
            branch,
            limit,
            offset,
        } => {
            let path = repo_path_or_err(hub, &repo_id)?;
            let commits =
                git_inspect::list_commits(&path, branch.as_deref(), limit, offset).await?;
            let _ = out_tx.send(DaemonMessage::Commits {
                repo_id,
                commits,
                offset,
            });
        }
        ClientMessage::GetCommit { repo_id, sha } => {
            let path = repo_path_or_err(hub, &repo_id)?;
            let detail = git_inspect::get_commit(&path, &sha).await?;
            let _ = out_tx.send(DaemonMessage::CommitDetail { repo_id, detail });
        }
        ClientMessage::GetFileDiff {
            repo_id,
            path,
            against,
        } => {
            let repo = repo_path_or_err(hub, &repo_id)?;
            let diff = git_inspect::file_diff(&repo, &path, against.as_deref()).await?;
            let _ = out_tx.send(DaemonMessage::FileDiff {
                repo_id,
                path,
                against,
                diff,
            });
        }
        ClientMessage::GetRemoteUrl { repo_id } => {
            let repo = repo_path_or_err(hub, &repo_id)?;
            let info = git_inspect::remote_url(&repo_id, &repo).await?;
            let _ = out_tx.send(DaemonMessage::RemoteUrl(info));
        }
        ClientMessage::RepoStatus { repo_id } => {
            let repo = repo_path_or_err(hub, &repo_id)?;
            let (index_changes, worktree_changes) = git_inspect::repo_status(&repo).await?;
            let _ = out_tx.send(DaemonMessage::RepoStatus {
                repo_id,
                index_changes,
                worktree_changes,
            });
        }
        ClientMessage::StageFiles { repo_id, paths } => {
            handle_git_write(hub, out_tx, &repo_id, "stage", async {
                let repo = repo_path_or_err(hub, &repo_id)?;
                git_write::stage(&repo, &paths).await?;
                Ok(None)
            })
            .await;
        }
        ClientMessage::UnstageFiles { repo_id, paths } => {
            handle_git_write(hub, out_tx, &repo_id, "unstage", async {
                let repo = repo_path_or_err(hub, &repo_id)?;
                git_write::unstage(&repo, &paths).await?;
                Ok(None)
            })
            .await;
        }
        ClientMessage::CommitRepo { repo_id, message } => {
            handle_git_write(hub, out_tx, &repo_id, "commit", async {
                let repo = repo_path_or_err(hub, &repo_id)?;
                let (sha, short_sha) = git_write::commit(&repo, &message).await?;
                Ok(Some(DaemonMessage::CommitOk {
                    repo_id: repo_id.clone(),
                    sha,
                    short_sha,
                }))
            })
            .await;
        }
        ClientMessage::DiscardChanges { repo_id, paths } => {
            handle_git_write(hub, out_tx, &repo_id, "discard", async {
                let repo = repo_path_or_err(hub, &repo_id)?;
                git_write::discard(&repo, &paths).await?;
                Ok(None)
            })
            .await;
        }
        ClientMessage::StashPush { repo_id, message } => {
            handle_stash_write(hub, out_tx, &repo_id, "stash_push", async {
                let repo = repo_path_or_err(hub, &repo_id)?;
                git_write::stash_push(&repo, &message).await
            })
            .await;
        }
        ClientMessage::ListStashes { repo_id } => match repo_path_or_err(hub, &repo_id) {
            Ok(repo) => match git_write::stash_list(&repo).await {
                Ok(stashes) => {
                    let _ = out_tx.send(DaemonMessage::Stashes { repo_id, stashes });
                }
                Err(err) => {
                    warn!(?err, repo_id, "stash_list failed");
                    let _ = out_tx.send(DaemonMessage::Error {
                        message: format!("stash list failed: {err:#}"),
                    });
                }
            },
            Err(err) => {
                let _ = out_tx.send(DaemonMessage::Error {
                    message: format!("{err:#}"),
                });
            }
        },
        ClientMessage::StashPop { repo_id, stash_id } => {
            handle_stash_write(hub, out_tx, &repo_id, "stash_pop", async {
                let repo = repo_path_or_err(hub, &repo_id)?;
                git_write::stash_pop(&repo, &stash_id).await
            })
            .await;
        }
        ClientMessage::StashApply { repo_id, stash_id } => {
            handle_stash_write(hub, out_tx, &repo_id, "stash_apply", async {
                let repo = repo_path_or_err(hub, &repo_id)?;
                git_write::stash_apply(&repo, &stash_id).await
            })
            .await;
        }
        ClientMessage::StashDrop { repo_id, stash_id } => {
            handle_stash_write(hub, out_tx, &repo_id, "stash_drop", async {
                let repo = repo_path_or_err(hub, &repo_id)?;
                git_write::stash_drop(&repo, &stash_id).await
            })
            .await;
        }
        ClientMessage::OpenDiffTab {
            id,
            repo_id,
            path,
            against,
        } => {
            let outcome = open_diff_tab(hub, &repo_id, &path, against.as_deref())?;
            if let Some(new_tab) = outcome.created {
                let _ = hub.tab_events.send(TabEvent::Updated(new_tab));
            }
            let _ = out_tx.send(DaemonMessage::DiffTabOpened {
                id,
                tab_id: outcome.tab_id,
            });
        }
        ClientMessage::GetFileSnapshot {
            id,
            repo_id,
            path,
            against,
        } => {
            let repo = repo_path_or_err(hub, &repo_id)?;
            match git_inspect::file_snapshot(&repo, &path, against.as_deref()).await {
                Ok((old, new)) => {
                    let _ = out_tx.send(DaemonMessage::FileSnapshot {
                        id,
                        repo_id,
                        path: path.clone(),
                        against,
                        old,
                        new,
                        language: git_inspect::language_for_path(&path).to_string(),
                    });
                }
                Err(err) => {
                    let _ = out_tx.send(DaemonMessage::FileSnapshotError {
                        id,
                        repo_id,
                        path,
                        against,
                        error: format!("{err:#}"),
                    });
                }
            }
        }
        ClientMessage::LoadScrollback { session_id } => {
            let (data, truncated) = scrollback::load(&hub.dirs, &session_id);
            let data_b64 = base64::engine::general_purpose::STANDARD.encode(&data);
            let _ = out_tx.send(DaemonMessage::Scrollback {
                session_id,
                data_b64,
                truncated,
            });
        }
        ClientMessage::Shutdown { drain } => {
            info!(drain, "dispatch: ClientMessage::Shutdown received");
            if drain {
                shutdown_all(hub).await;
                info!("dispatch: shutdown_all returned; flipping watch");
            } else {
                // Don't drain — leave sidecars on disk so the next daemon
                // start surfaces these sessions in the Abandoned bucket.
                // Pre-Phase-C the children die anyway (their ConPTY master
                // goes away with the daemon); B.3's contribution is the
                // sidecar-preservation half of the round-trip.
                info!("dispatch: shutdown without drain; sidecars retained for resume");
            }
            // Hand the client an explicit ack BEFORE we flip the shutdown
            // signal. The ack rides the existing out_tx → send_task → WS
            // sink path, so it's serialized after every Updated/Removed
            // event we just emitted as part of shutdown_all. With the ack
            // received, the client can close its window without waiting on
            // the WS Close (axum drops the upgraded socket without
            // sending a Close frame, and the OS-level TCP FIN can take
            // several seconds to reach WebView2 on Windows).
            let ack_result = out_tx.send(DaemonMessage::ShutdownAck {});
            info!(
                ack_result_ok = ack_result.is_ok(),
                "dispatch: ShutdownAck queued",
            );
            // Signal the accept loop to exit. The send may fail if a previous
            // Shutdown already flipped it; either way the daemon is on its
            // way out.
            let send_result = hub.shutdown_tx.send(true);
            info!(?send_result, "dispatch: shutdown_tx.send returned");
        }
        ClientMessage::SetRepoWorktreeDefault { repo_id, value } => {
            set_repo_worktree_default(&hub.state, &repo_id, value)?;
            let repos = hub.state.with_persisted(|s| s.repos.clone());
            let _ = hub.state_events.send(StateEvent::Repos(repos));
        }
        ClientMessage::SetWorkspaceWorktreeDefault {
            workspace_id,
            value,
        } => {
            set_workspace_worktree_default(&hub.state, &workspace_id, value)?;
            let workspaces = hub.state.with_persisted(|s| s.workspaces.clone());
            let _ = hub.state_events.send(StateEvent::Workspaces(workspaces));
        }
        ClientMessage::ListTabs => {
            let tabs = hub.state.with_persisted(|s| s.tabs.clone());
            let _ = out_tx.send(DaemonMessage::Tabs { tabs });
        }
        ClientMessage::CreateTab {
            name,
            initial_session_id,
        } => {
            // Resolve the default name against the current tab list so
            // sequential opens produce `Tab`, `Tab 2`, `Tab 3`, ...
            let new_tab = hub.state.mutate(|s| {
                let tab = tabs::make_tab(name, initial_session_id, &s.tabs);
                s.tabs.push(tab.clone());
                tab
            })?;
            let _ = hub.tab_events.send(TabEvent::Updated(new_tab));
        }
        ClientMessage::CloseTab { tab_id } => {
            let removed = hub.state.mutate(|s| {
                let prev = s.tabs.len();
                s.tabs.retain(|t| t.id != tab_id);
                prev != s.tabs.len()
            })?;
            if removed {
                let _ = hub.tab_events.send(TabEvent::Removed(tab_id));
            }
        }
        ClientMessage::RenameTab { tab_id, name } => {
            let updated = hub.state.mutate(|s| {
                tabs::find_tab_mut(&mut s.tabs, &tab_id).map(|t| {
                    t.name = name;
                    t.clone()
                })
            })??;
            let _ = hub.tab_events.send(TabEvent::Updated(updated));
        }
        ClientMessage::ReorderTabs { ordered_ids } => {
            hub.state
                .mutate(|s| tabs::reorder_tabs(&mut s.tabs, &ordered_ids))??;
            let _ = hub.tab_events.send(TabEvent::Reordered(ordered_ids));
        }
        ClientMessage::ReorderContainers { ordered } => {
            let applied = reorder_containers(&hub.state, ordered)?;
            let _ = hub
                .state_events
                .send(StateEvent::ContainersReordered(applied));
        }
        ClientMessage::ReorderSessions {
            container_id,
            ordered_ids,
        } => {
            set_session_order(&hub.state, &container_id, ordered_ids.clone())?;
            let _ = hub.state_events.send(StateEvent::SessionsReordered {
                container_id,
                ordered_ids,
            });
        }
        ClientMessage::SplitPane {
            tab_id,
            pane_id,
            direction,
            place,
            new_session_id,
        } => {
            let updated = hub.state.mutate(|s| {
                let tab = tabs::find_tab_mut(&mut s.tabs, &tab_id)?;
                let grid = tabs::grid_or_err_mut(tab)?;
                tabs::split_pane(grid, &pane_id, direction, place, new_session_id)?;
                Ok::<_, anyhow::Error>(tab.clone())
            })??;
            let _ = hub.tab_events.send(TabEvent::Updated(updated));
        }
        ClientMessage::ClosePane { tab_id, pane_id } => {
            let outcome = hub.state.mutate(|s| {
                let tab = tabs::find_tab_mut(&mut s.tabs, &tab_id)?;
                let grid = tabs::grid_or_err_mut(tab)?;
                let empty = tabs::close_pane(grid, &pane_id)?;
                if empty {
                    s.tabs.retain(|t| t.id != tab_id);
                    Ok::<_, anyhow::Error>(CloseOutcome::TabRemoved(tab_id.clone()))
                } else {
                    Ok(CloseOutcome::TabUpdated(tab.clone()))
                }
            })??;
            match outcome {
                CloseOutcome::TabRemoved(id) => {
                    let _ = hub.tab_events.send(TabEvent::Removed(id));
                }
                CloseOutcome::TabUpdated(tab) => {
                    let _ = hub.tab_events.send(TabEvent::Updated(tab));
                }
            }
        }
        ClientMessage::SetPaneRatio {
            tab_id,
            split_path,
            ratio,
        } => {
            let updated = hub.state.mutate(|s| {
                let tab = tabs::find_tab_mut(&mut s.tabs, &tab_id)?;
                let grid = tabs::grid_or_err_mut(tab)?;
                tabs::set_pane_ratio(grid, &split_path, ratio)?;
                Ok::<_, anyhow::Error>(tab.clone())
            })??;
            let _ = hub.tab_events.send(TabEvent::Updated(updated));
        }
        ClientMessage::ReplacePaneSession {
            tab_id,
            pane_id,
            session_id,
        } => {
            let updated = hub.state.mutate(|s| {
                let tab = tabs::find_tab_mut(&mut s.tabs, &tab_id)?;
                let grid = tabs::grid_or_err_mut(tab)?;
                tabs::replace_pane_session(grid, &pane_id, session_id)?;
                Ok::<_, anyhow::Error>(tab.clone())
            })??;
            let _ = hub.tab_events.send(TabEvent::Updated(updated));
        }
        ClientMessage::MovePane {
            src_tab_id,
            src_pane_id,
            dst_tab_id,
            dst_pane_id,
            edge,
        } => {
            if src_pane_id == dst_pane_id {
                // Self-move is a no-op; don't mutate or broadcast.
            } else {
                let events = move_pane(
                    hub,
                    &src_tab_id,
                    &src_pane_id,
                    &dst_tab_id,
                    &dst_pane_id,
                    edge,
                )?;
                for event in events {
                    let _ = hub.tab_events.send(event);
                }
            }
        }
        ClientMessage::RearrangeTab { tab_id, layout } => {
            let updated = hub.state.mutate(|s| {
                let tab = tabs::find_tab_mut(&mut s.tabs, &tab_id)?;
                let grid = tabs::grid_or_err_mut(tab)?;
                let new_grid = tabs::rearrange_grid(grid, layout)?;
                *grid = new_grid;
                Ok::<_, anyhow::Error>(tab.clone())
            })??;
            let _ = hub.tab_events.send(TabEvent::Updated(updated));
        }
        ClientMessage::MergeTabs {
            tab_ids,
            name,
            layout,
        } => {
            let new_tab = hub
                .state
                .mutate(|s| tabs::merge_tabs(&mut s.tabs, &tab_ids, name, layout))??;
            for id in &tab_ids {
                let _ = hub.tab_events.send(TabEvent::Removed(id.clone()));
            }
            let _ = hub.tab_events.send(TabEvent::Updated(new_tab));
        }
        ClientMessage::ExtractToNewTab {
            source_tab_id,
            pane_ids,
            name,
        } => {
            let (new_tab, source_empty, source_survivor) = hub.state.mutate(|s| {
                let (new_tab, source_empty) =
                    tabs::extract_to_new_tab(&mut s.tabs, &source_tab_id, &pane_ids, name)?;
                let source_survivor = if source_empty {
                    None
                } else {
                    s.tabs.iter().find(|t| t.id == source_tab_id).cloned()
                };
                Ok::<_, anyhow::Error>((new_tab, source_empty, source_survivor))
            })??;
            if source_empty {
                let _ = hub.tab_events.send(TabEvent::Removed(source_tab_id));
            } else if let Some(survivor) = source_survivor {
                let _ = hub.tab_events.send(TabEvent::Updated(survivor));
            }
            let _ = hub.tab_events.send(TabEvent::Updated(new_tab));
        }
        ClientMessage::ListPresets { target } => {
            let entries = crate::presets::list(hub, &target).await?;
            let _ = out_tx.send(DaemonMessage::Presets { target, entries });
        }
        ClientMessage::LaunchPreset {
            target,
            preset_id,
            source,
            variable_values,
            use_worktree_override,
            max_panes_per_tab_override,
        } => {
            // Run the batch in a background task so the dispatcher returns
            // immediately and the connection stays responsive while sessions
            // spawn. Progress/failed messages stream back via the daemon-wide
            // preset_events broadcast that every connected client forwards.
            let hub = hub.clone();
            let args = crate::presets::LaunchArgs {
                target,
                preset_id,
                source,
                variable_values,
                use_worktree_override,
                max_panes_per_tab_override,
            };
            tokio::spawn(async move {
                crate::presets::launch(hub, args).await;
            });
        }
        ClientMessage::PreviewPreset {
            id,
            target,
            preset_id,
            source,
            variable_values: _,
        } => {
            // Variable values aren't applied during preview — we return raw
            // prompt texts (the same shape the inline-source client-side
            // preview produces). Reserved on the wire for forward use.
            match crate::presets::preview_prompts(hub, &target, &preset_id, &source).await {
                Ok(prompts) => {
                    let _ = out_tx.send(DaemonMessage::PresetPreview { id, prompts });
                }
                Err(err) => {
                    let _ = out_tx.send(DaemonMessage::PresetPreviewError {
                        id,
                        error: format!("{err:#}"),
                    });
                }
            }
        }
        ClientMessage::ResolvePresetScripts {
            id,
            target,
            preset_id,
            variable_values,
        } => {
            // Run script-kind variables in a background task so the WS
            // connection stays responsive while a slow fetch script (e.g.
            // pulling docker logs) is in flight. Reply goes back through
            // the same per-connection out_tx the dispatcher uses.
            let hub = hub.clone();
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                match crate::presets::resolve_scripts(&hub, &target, &preset_id, &variable_values)
                    .await
                {
                    Ok((values, executed_commands)) => {
                        let _ = out_tx.send(DaemonMessage::PresetScriptsResolved {
                            id,
                            values,
                            executed_commands,
                        });
                    }
                    Err(err) => {
                        let _ = out_tx.send(DaemonMessage::PresetScriptsError {
                            id,
                            error: format!("{err:#}"),
                            variable_name: None,
                        });
                    }
                }
            });
        }
    }
    Ok(())
}

fn normalize_session_color(color: Option<String>) -> anyhow::Result<Option<String>> {
    let Some(raw) = color else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let Some(hex) = trimmed.strip_prefix('#') else {
        return Err(anyhow!("session color must use #RRGGBB format"));
    };
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(anyhow!("session color must use #RRGGBB format"));
    }
    Ok(Some(format!("#{}", hex.to_ascii_lowercase())))
}

enum CloseOutcome {
    TabRemoved(String),
    TabUpdated(TabEntry),
}

/// Move a pane between (possibly same) tabs and produce the broadcast events
/// to fire afterwards. Performs both extract + insert under a single
/// `state.mutate` so the persisted snapshot is never half-applied.
fn move_pane(
    hub: &Hub,
    src_tab_id: &str,
    src_pane_id: &str,
    dst_tab_id: &str,
    dst_pane_id: &str,
    edge: PaneDropEdge,
) -> anyhow::Result<Vec<TabEvent>> {
    hub.state.mutate(|s| {
        if src_tab_id == dst_tab_id {
            let tab = tabs::find_tab_mut(&mut s.tabs, src_tab_id)?;
            let grid = tabs::grid_or_err_mut(tab)?;
            let (extracted, source_empty) = tabs::extract_pane(grid, src_pane_id)?;
            if source_empty {
                return Err(anyhow!("cannot move the only pane within the same tab"));
            }
            tabs::insert_adjacent(grid, dst_pane_id, edge, extracted)?;
            Ok::<_, anyhow::Error>(vec![TabEvent::Updated(tab.clone())])
        } else {
            let (extracted_node, source_empty) = {
                let src_tab = tabs::find_tab_mut(&mut s.tabs, src_tab_id)?;
                let src_grid = tabs::grid_or_err_mut(src_tab)?;
                tabs::extract_pane(src_grid, src_pane_id)?
            };
            let mut events = Vec::new();
            if source_empty {
                s.tabs.retain(|t| t.id != src_tab_id);
                events.push(TabEvent::Removed(src_tab_id.to_string()));
            } else {
                let src_snapshot = s
                    .tabs
                    .iter()
                    .find(|t| t.id == src_tab_id)
                    .cloned()
                    .ok_or_else(|| anyhow!("source tab vanished mid-mutation: {src_tab_id}"))?;
                events.push(TabEvent::Updated(src_snapshot));
            }
            let dst_tab = tabs::find_tab_mut(&mut s.tabs, dst_tab_id)?;
            let dst_grid = tabs::grid_or_err_mut(dst_tab)?;
            tabs::insert_adjacent(dst_grid, dst_pane_id, edge, extracted_node)?;
            events.push(TabEvent::Updated(dst_tab.clone()));
            Ok(events)
        }
    })?
}

/// Stop every active session (kill PTY / headless child, drop orphan sidecar)
/// and clear the registry. Worktrees are intentionally left in place — exit
/// is the wrong moment to ask the user about per-member cleanup.
async fn shutdown_all(hub: &Hub) {
    let ids: Vec<String> = hub.sessions.snapshots().into_iter().map(|s| s.id).collect();
    info!(count = ids.len(), "shutdown_all: starting");
    for id in &ids {
        info!(session_id = %id, "shutdown_all: stopping session");
        if let Err(err) = stop_session(hub, id).await {
            warn!(session_id = %id, ?err, "stop_session during shutdown failed");
        }
        info!(session_id = %id, "shutdown_all: session stopped");
    }
    info!("shutdown_all: complete");
}

struct OpenDiffOutcome {
    tab_id: String,
    /// `Some` when a new tab was created (caller should broadcast
    /// `TabUpdated`); `None` when an existing diff tab matched and we just
    /// focused it.
    created: Option<TabEntry>,
}

/// Find or create a diff tab keyed on (`repo_id`, `path`, `against`).
/// Returns the tab id either way so the client can activate it.
fn open_diff_tab(
    hub: &Hub,
    repo_id: &str,
    path: &str,
    against: Option<&str>,
) -> anyhow::Result<OpenDiffOutcome> {
    let against_owned = against.map(str::to_string);
    let outcome: anyhow::Result<OpenDiffOutcome> = hub.state.mutate(|s| {
        if let Some(existing) = s.tabs.iter().find(|t| {
            matches!(
                &t.content,
                TabContent::Diff {
                    repo_id: r,
                    path: p,
                    against: a,
                } if r == repo_id && p == path && a.as_deref() == against
            )
        }) {
            return Ok(OpenDiffOutcome {
                tab_id: existing.id.clone(),
                created: None,
            });
        }
        let new_tab = TabEntry {
            id: uuid::Uuid::new_v4().to_string(),
            name: diff_tab_name(path, against_owned.as_deref()),
            content: TabContent::Diff {
                repo_id: repo_id.to_string(),
                path: path.to_string(),
                against: against_owned.clone(),
            },
            created_at: chrono::Utc::now(),
        };
        let id = new_tab.id.clone();
        s.tabs.push(new_tab.clone());
        Ok(OpenDiffOutcome {
            tab_id: id,
            created: Some(new_tab),
        })
    })?;
    outcome
}

fn diff_tab_name(path: &str, against: Option<&str>) -> String {
    let basename = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match against {
        None => format!("{basename} (changes)"),
        Some("HEAD") => format!("{basename} (staged)"),
        Some(rev) => format!("{basename} @ {}", short_rev(rev)),
    }
}

fn short_rev(rev: &str) -> &str {
    if rev.len() > 7 { &rev[..7] } else { rev }
}

/// Derive the on-disk worktree paths from a `SpawnConfig` + the session's
/// `SessionMember` list. Returns the per-member `worktree_path` strings when
/// the config used `use_worktree = true`, or an empty vec for in-place
/// sessions. Used to populate `SessionRecord::worktree_paths` at spawn time.
fn worktree_paths_for_config(
    config: &protocol::SpawnConfig,
    members: &[protocol::SessionMember],
) -> Vec<String> {
    let use_worktree = match &config.target {
        protocol::SpawnTarget::Single { use_worktree, .. }
        | protocol::SpawnTarget::Workspace { use_worktree, .. } => *use_worktree,
    };
    if use_worktree {
        members.iter().map(|m| m.worktree_path.clone()).collect()
    } else {
        Vec::new()
    }
}

fn repo_path_or_err(hub: &Hub, repo_id: &str) -> anyhow::Result<PathBuf> {
    hub.state
        .with_persisted(|s| {
            s.repos
                .iter()
                .find(|r| r.id == repo_id)
                .map(|r| r.path.clone())
        })
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("unknown repo: {repo_id}"))
}

/// Runs a stage/unstage/commit body, then on success broadcasts the fresh
/// `RepoStatus` to all connected clients (including this one) and emits the
/// optional follow-up message (e.g. `CommitOk`). On failure, sends
/// `GitWriteError { operation }` to the caller only — peer clients learn
/// nothing happened because the broadcast is never sent.
async fn handle_git_write<F>(
    hub: &Hub,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    repo_id: &str,
    operation: &str,
    body: F,
) where
    F: std::future::Future<Output = anyhow::Result<Option<DaemonMessage>>>,
{
    let outcome = body.await;
    match outcome {
        Ok(follow_up) => {
            match repo_path_or_err(hub, repo_id) {
                Ok(repo) => match git_inspect::repo_status(&repo).await {
                    Ok((index_changes, worktree_changes)) => {
                        let _ = hub.state_events.send(StateEvent::RepoStatus {
                            repo_id: repo_id.to_string(),
                            index_changes,
                            worktree_changes,
                        });
                    }
                    Err(err) => {
                        warn!(?err, repo_id, "git_write: post-write status refresh failed");
                    }
                },
                Err(err) => {
                    warn!(?err, repo_id, "git_write: repo path lookup failed");
                }
            }
            if let Some(msg) = follow_up {
                let _ = out_tx.send(msg);
            }
        }
        Err(err) => {
            warn!(operation, repo_id, ?err, "git_write: operation failed");
            let _ = out_tx.send(DaemonMessage::GitWriteError {
                repo_id: repo_id.to_string(),
                operation: operation.to_string(),
                error: format!("{err:#}"),
            });
        }
    }
}

/// Like [`handle_git_write`] but for stash mutations: in addition to the
/// post-write `RepoStatus` refresh, broadcasts a fresh `Stashes` snapshot
/// so every connected sidebar sees the new stash list without re-requesting.
async fn handle_stash_write<F>(
    hub: &Hub,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    repo_id: &str,
    operation: &str,
    body: F,
) where
    F: std::future::Future<Output = anyhow::Result<()>>,
{
    let outcome = body.await;
    match outcome {
        Ok(()) => match repo_path_or_err(hub, repo_id) {
            Ok(repo) => {
                match git_inspect::repo_status(&repo).await {
                    Ok((index_changes, worktree_changes)) => {
                        let _ = hub.state_events.send(StateEvent::RepoStatus {
                            repo_id: repo_id.to_string(),
                            index_changes,
                            worktree_changes,
                        });
                    }
                    Err(err) => {
                        warn!(
                            ?err,
                            repo_id, "stash_write: post-write status refresh failed"
                        );
                    }
                }
                match git_write::stash_list(&repo).await {
                    Ok(stashes) => {
                        let _ = hub.state_events.send(StateEvent::Stashes {
                            repo_id: repo_id.to_string(),
                            stashes,
                        });
                    }
                    Err(err) => {
                        warn!(?err, repo_id, "stash_write: stash_list refresh failed");
                    }
                }
            }
            Err(err) => {
                warn!(?err, repo_id, "stash_write: repo path lookup failed");
            }
        },
        Err(err) => {
            warn!(operation, repo_id, ?err, "stash_write: operation failed");
            let _ = out_tx.send(DaemonMessage::GitWriteError {
                repo_id: repo_id.to_string(),
                operation: operation.to_string(),
                error: format!("{err:#}"),
            });
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "Spawn is naturally a flat pipeline (resolve → config → mode switch → \
              timing). Extracting helpers would require threading the timing values \
              and adding a result wrapper for diminishing returns"
)]
pub(crate) async fn spawn_session(
    hub: &Hub,
    req: SpawnRequest,
) -> anyhow::Result<protocol::SessionSnapshot> {
    // Phase-level timing for the spawn pipeline so we can tell at a glance
    // whether a slow spawn was the git worktree setup, the PTY/child boot,
    // or something on the registry side. Logged at info so users debugging
    // perceived slowness can read them without flipping log filters.
    let t_total = std::time::Instant::now();
    // Capture the persist-able config before destructuring so duplicates
    // can be reconstructed verbatim from the session record / sidecar.
    let stored_config = protocol::SpawnConfig::from_request(&req);
    let SpawnRequest {
        label,
        target,
        mode,
        initial_prompt,
        dangerously_skip_permissions,
        agent,
        model,
        permission_mode,
        codex_sandbox,
        extra_env,
        prompt_injector,
    } = req;
    info!(
        ?mode,
        agent = agent.as_label(),
        target_kind = match &target {
            SpawnTarget::Single { .. } => "single",
            SpawnTarget::Workspace { .. } => "workspace",
        },
        "spawn_session: begin"
    );
    if mode == SessionMode::Headless && agent == Agent::Codex {
        return Err(anyhow!(
            "headless mode is not yet supported for codex; use interactive mode"
        ));
    }

    let t_resolve = std::time::Instant::now();
    let (kind, members, primary_cwd, default_label, workspace_id) = match target {
        SpawnTarget::Single {
            repo_id,
            branch_name,
            base_branch,
            use_worktree,
        } => spawn_single(hub, &repo_id, &branch_name, base_branch, use_worktree).await?,
        SpawnTarget::Workspace {
            workspace_id,
            branch_name,
            base_branch,
            use_worktree,
        } => spawn_workspace(hub, &workspace_id, &branch_name, base_branch, use_worktree).await?,
    };
    info!(
        elapsed_ms = u64::try_from(t_resolve.elapsed().as_millis()).unwrap_or(u64::MAX),
        member_count = members.len(),
        cwd = %primary_cwd.display(),
        "spawn_session: git/worktree resolution done"
    );

    let session_id = new_id();
    let label = label.unwrap_or(default_label);
    let cfg = SpawnArgs {
        dangerously_skip_permissions,
        agent,
        model,
        permission_mode,
        codex_sandbox,
        extra_env,
        prompt_injector,
    };

    // Record last-used agent per targeted repo. Best-effort: a state.json
    // write failure here is logged but not fatal to the spawn.
    let mut repos_changed = false;
    let mut workspaces_changed = false;
    for member in &members {
        if let Err(err) = persist_last_agent(&hub.state, &member.repo_id, agent) {
            warn!(?err, repo_id = %member.repo_id, agent = agent.as_label(), "persist_last_agent failed");
        } else {
            repos_changed = true;
        }
    }

    // Record the full spawn config on the targeted container so "Launch last
    // again" (sidebar double-click / context-menu submenu) can replay it.
    // Workspace spawns persist to the workspace only — single-repo "launch
    // last" intentionally stays decoupled from workspace launches.
    match &stored_config.target {
        SpawnTarget::Single { repo_id, .. } => {
            if let Err(err) =
                persist_repo_last_spawn_config(&hub.state, repo_id, stored_config.clone())
            {
                warn!(?err, %repo_id, "persist_repo_last_spawn_config failed");
            } else {
                repos_changed = true;
            }
        }
        SpawnTarget::Workspace { workspace_id, .. } => {
            if let Err(err) =
                persist_workspace_last_spawn_config(&hub.state, workspace_id, stored_config.clone())
            {
                warn!(?err, %workspace_id, "persist_workspace_last_spawn_config failed");
            } else {
                workspaces_changed = true;
            }
        }
    }
    if repos_changed {
        let repos = hub.state.with_persisted(|s| s.repos.clone());
        let _ = hub.state_events.send(StateEvent::Repos(repos));
    }
    if workspaces_changed {
        let workspaces = hub.state.with_persisted(|s| s.workspaces.clone());
        let _ = hub.state_events.send(StateEvent::Workspaces(workspaces));
    }

    let t_child = std::time::Instant::now();
    let result = match mode {
        SessionMode::Headless => {
            let prompt = initial_prompt
                .clone()
                .ok_or_else(|| anyhow!("headless sessions require an initial_prompt"))?;
            spawn_headless_session(
                hub,
                session_id,
                label,
                kind,
                members,
                workspace_id,
                primary_cwd,
                prompt,
                &cfg,
                stored_config,
            )
        }
        SessionMode::PlainShell => {
            if initial_prompt.is_some() {
                return Err(anyhow!(
                    "plain shell sessions do not accept an initial_prompt"
                ));
            }
            spawn_plain_shell_session(
                hub,
                session_id,
                label,
                kind,
                members,
                workspace_id,
                primary_cwd,
                &cfg,
                stored_config,
            )
            .await
        }
        SessionMode::Interactive => {
            spawn_interactive_session(
                hub,
                session_id,
                label,
                kind,
                members,
                workspace_id,
                primary_cwd,
                initial_prompt,
                &cfg,
                stored_config,
            )
            .await
        }
    };
    let outcome = if result.is_ok() { "ok" } else { "error" };
    info!(
        outcome,
        elapsed_child_ms = u64::try_from(t_child.elapsed().as_millis()).unwrap_or(u64::MAX),
        elapsed_total_ms = u64::try_from(t_total.elapsed().as_millis()).unwrap_or(u64::MAX),
        "spawn_session: child boot done"
    );
    result
}

fn expected_output_subscribers(mode: SessionMode) -> usize {
    match mode {
        SessionMode::Interactive => 3,
        SessionMode::PlainShell => 2,
        SessionMode::Headless => 0,
    }
}

/// Per-spawn configuration bundled together to keep spawn-fn signatures narrow.
struct SpawnArgs {
    dangerously_skip_permissions: bool,
    /// Which CLI to spawn. Drives both [`agent_program`] resolution and the
    /// per-agent arg builder branching in [`spawn_interactive_session`].
    agent: Agent,
    model: Option<String>,
    /// Claude-only. Ignored for codex.
    permission_mode: Option<PermissionMode>,
    /// Codex-only. Ignored for claude. Overridden by `dangerously_skip_permissions`
    /// (yolo replaces sandbox).
    codex_sandbox: Option<CodexSandbox>,
    extra_env: Vec<(String, String)>,
    /// Scripted PTY input driven post-spawn for interactive sessions. When
    /// `Some`, the interactive spawn path omits the prompt CLI flag — the
    /// injector is responsible for delivering the prompt. Ignored by the
    /// headless and plain-shell paths.
    prompt_injector: Option<protocol::PromptInjector>,
}

impl SpawnArgs {
    /// Append claude's `--model`, `--permission-mode`, and
    /// `--dangerously-skip-permissions` args. The claude CLI rejects
    /// `--permission-mode` together with `--dangerously-skip-permissions`, so
    /// the latter wins when both are set.
    fn extend_claude_args(&self, args: &mut Vec<String>) {
        if let Some(model) = &self.model {
            args.push("--model".to_string());
            args.push(model.clone());
        }
        if self.dangerously_skip_permissions {
            args.push("--dangerously-skip-permissions".to_string());
        } else if let Some(mode) = self.permission_mode {
            args.push("--permission-mode".to_string());
            args.push(mode.as_cli_arg().to_string());
        }
    }
}

/// Build a workspace-context note for the agent. Returns `Some` only when
/// the session has 2+ members (i.e. it's a workspace spawn). The note maps
/// each member's repo name to the actual worktree path used for THIS
/// session, so the agent doesn't try to navigate to original-repo paths
/// referenced in `CLAUDE.md` / `AGENTS.md` that no longer match where the
/// session is rooted.
///
/// Delivered as `--append-system-prompt` to claude (invisible to the user)
/// and prepended to the positional prompt for codex (codex doesn't have a
/// system-prompt flag, so the trade-off is one extra user turn).
fn build_workspace_prelude(members: &[SessionMember]) -> Option<String> {
    if members.len() < 2 {
        return None;
    }
    let mut out = String::from(
        "Workspace member paths for this session (use these for cross-repo \
         file access — they override any absolute paths referenced in \
         CLAUDE.md / AGENTS.md):\n",
    );
    let name_width = members.iter().map(|m| m.repo_name.len()).max().unwrap_or(0);
    for m in members {
        use std::fmt::Write as _;
        // Width-padded for visual alignment in the agent's view; failure
        // here would mean OOM during string formatting, which we treat as
        // unreachable for a few dozen members at most.
        let _ = writeln!(
            out,
            "  {name:<width$}  ->  {path}",
            name = m.repo_name,
            width = name_width,
            path = m.worktree_path,
        );
    }
    Some(out)
}

/// Build the codex CLI argv for an interactive session. Factored out of
/// [`spawn_interactive_session`] so it can be unit-tested without spawning.
///
/// Layout:
/// - `--add-dir <path>` for every extra member after the primary cwd
/// - `--model <id>` when [`SpawnArgs::model`] is set
/// - permission/sandbox: `--yolo` overrides everything; otherwise
///   `--sandbox <value>` when [`SpawnArgs::codex_sandbox`] is set
/// - trailing positional `<prompt>` when no `prompt_injector` is attached;
///   carries `<workspace_prelude>\n\n<initial_prompt>` for workspace spawns,
///   or just `<initial_prompt>`, or just `<workspace_prelude>`, depending
///   on which are present. Codex doesn't expose a system-prompt flag so
///   the prelude rides along on the user's first message slot.
fn build_codex_args(
    cfg: &SpawnArgs,
    members: &[SessionMember],
    initial_prompt: Option<&str>,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    for extra in members.iter().skip(1) {
        args.push("--add-dir".to_string());
        args.push(extra.worktree_path.clone());
    }
    if let Some(model) = &cfg.model {
        args.push("--model".to_string());
        args.push(model.clone());
    }
    if cfg.dangerously_skip_permissions {
        args.push("--yolo".to_string());
    } else if let Some(sandbox) = cfg.codex_sandbox {
        args.push("--sandbox".to_string());
        args.push(sandbox.as_cli_arg().to_string());
    }
    if cfg.prompt_injector.is_none() {
        let prelude = build_workspace_prelude(members);
        let combined = match (prelude, initial_prompt) {
            (Some(pre), Some(user)) => Some(format!("{pre}\n{user}")),
            (Some(pre), None) => Some(pre),
            (None, Some(user)) => Some(user.to_string()),
            (None, None) => None,
        };
        if let Some(text) = combined {
            args.push(text);
        }
    }
    args
}

#[expect(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "Constructing a session record is naturally wide; bundling into a struct adds noise"
)]
async fn spawn_interactive_session(
    hub: &Hub,
    session_id: String,
    label: String,
    kind: SessionKind,
    members: Vec<SessionMember>,
    workspace_id: Option<String>,
    primary_cwd: PathBuf,
    initial_prompt: Option<String>,
    cfg: &SpawnArgs,
    stored_config: protocol::SpawnConfig,
) -> anyhow::Result<protocol::SessionSnapshot> {
    let last_prompt = initial_prompt.clone();
    let args = match cfg.agent {
        Agent::Codex => build_codex_args(cfg, &members, initial_prompt.as_deref()),
        Agent::Claude => {
            let mut args: Vec<String> = Vec::new();
            for extra in members.iter().skip(1) {
                args.push("--add-dir".to_string());
                args.push(extra.worktree_path.clone());
            }
            // Workspace-context prelude rides on --append-system-prompt
            // so it's invisible to the user but the model sees it. Only
            // emitted for 2+ member sessions (where the worktree-vs-
            // original-path mismatch actually matters).
            if let Some(prelude) = build_workspace_prelude(&members) {
                args.push("--append-system-prompt".to_string());
                args.push(prelude);
            }
            cfg.extend_claude_args(&mut args);
            // When a prompt injector is attached, it carries the prompt as
            // scripted PTY input — passing `-p` would race the injector and
            // disable plan mode. The injector path takes over.
            if cfg.prompt_injector.is_none()
                && let Some(prompt) = initial_prompt
            {
                args.push("-p".to_string());
                args.push(prompt);
            }
            args
        }
    };

    let raw_program = agent_program(cfg.agent);
    let (program, prepend_args) = resolve_agent_program(&raw_program);
    let mut final_args = prepend_args;
    final_args.extend(args);
    info!(
        agent = cfg.agent.as_label(),
        raw = %raw_program,
        program = %program,
        argc = final_args.len(),
        args = ?final_args,
        "spawn_interactive_session: resolved program"
    );

    let spec = PtySpawnSpec {
        session_id: session_id.clone(),
        program,
        args: final_args,
        cwd: primary_cwd,
        env: merged_env(&cfg.extra_env),
        cols: 120,
        rows: 32,
    };
    let tracer_spawn = tracer_client::spawn(
        &hub.dirs,
        spec,
        expected_output_subscribers(SessionMode::Interactive),
    )
    .await
    .with_context(|| format!("spawning {} via tracer", cfg.agent.as_label()))?;
    let pty = tracer_spawn.handle;
    let tracer_pid = tracer_spawn.tracer_pid;
    let tracer_pipe = tracer_spawn.pipe_name;
    let tracer_exe_path = tracer_spawn.tracer_exe_path.to_string_lossy().into_owned();
    let pid = pty.pid();

    let started_at = Utc::now();
    let mut record = SessionRecord {
        id: session_id.clone(),
        label: label.clone(),
        default_label: label.clone(),
        user_label: None,
        kind: kind.clone(),
        members: members.clone(),
        mode: SessionMode::Interactive,
        started_at,
        status: SessionStatus::Idle,
        exit_code: None,
        metrics: SessionMetrics::default(),
        recent_actions: Vec::new(),
        pty: Some(Arc::clone(&pty)),
        headless: None,
        workspace_id: workspace_id.clone(),
        agent: cfg.agent,
        terminal_title: None,
        program_name: Some(cfg.agent.as_label().to_string()),
        accent_color: None,
        spawn_config: Some(stored_config.clone()),
        is_abandoned: false,
        is_inactive: false,
        worktree_paths: worktree_paths_for_config(&stored_config, &members),
        last_prompt: last_prompt.clone(),
        input_notifier: None,
    };
    push_recent_action(&mut record, "session started".to_string());
    hub.sessions.insert(record);
    let snap = hub
        .sessions
        .get(&session_id)
        .map(|rec| crate::sync::lock(&rec).snapshot())
        .ok_or_else(|| anyhow!("session vanished"))?;

    // Post-C.3 the direct daemon child is rt-tracer.exe, not the agent CLI.
    // Sidecar's `pid` + `program_name` therefore describe the tracer; the
    // underlying agent is captured in `agent`. is_session_alive prefers
    // `tracer_pid` + "rt-tracer" when the new fields are present.
    if let Some(pid) = pid
        && let Ok(meta) = orphan::meta_from_record(
            session_id.clone(),
            pid,
            label,
            kind,
            SessionMode::Interactive,
            members,
            started_at,
            workspace_id,
            Some("rt-tracer".to_string()),
            cfg.agent,
            Some(stored_config),
            last_prompt,
            Some(tracer_pid),
            Some(tracer_pipe),
            Some(tracer_exe_path),
        )
    {
        orphan::try_write_meta(&hub.dirs, &meta);
    }

    attach_lifecycle(
        &hub.sessions,
        session_id.clone(),
        &pty,
        Some(hub.dirs.clone()),
    );
    let (in_tx, in_rx) = mpsc::unbounded_channel::<usize>();
    if let Some(arc) = hub.sessions.get(&session_id) {
        crate::sync::lock(&arc).input_notifier = Some(in_tx);
    }
    pty_state::watch(
        &hub.sessions,
        session_id.clone(),
        pty.output.subscribe(),
        in_rx,
        hub.attention_tx.clone(),
    );
    osc_title::watch(
        &hub.sessions,
        session_id.clone(),
        pty.output.subscribe(),
        hub.dirs.clone(),
    );
    if let Some(injector) = cfg.prompt_injector.clone() {
        inject::run(session_id.clone(), Arc::clone(&pty), injector);
    }
    Ok(snap)
}

#[expect(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "Constructing a session record is naturally wide; bundling into a struct adds noise"
)]
async fn spawn_plain_shell_session(
    hub: &Hub,
    session_id: String,
    label: String,
    kind: SessionKind,
    members: Vec<SessionMember>,
    workspace_id: Option<String>,
    primary_cwd: PathBuf,
    cfg: &SpawnArgs,
    stored_config: protocol::SpawnConfig,
) -> anyhow::Result<protocol::SessionSnapshot> {
    // Fail-fast: the dialog should clear these for shell spawns, but a
    // client could send a malformed request. Reject rather than silently
    // ignore so the bug surfaces.
    if cfg.model.is_some() {
        return Err(anyhow!("plain shell sessions do not accept a model"));
    }
    if cfg.permission_mode.is_some() {
        return Err(anyhow!(
            "plain shell sessions do not accept a permission_mode"
        ));
    }
    if cfg.dangerously_skip_permissions {
        return Err(anyhow!(
            "plain shell sessions do not accept dangerously_skip_permissions"
        ));
    }

    let (program, shell_label) = resolve_shell_program()?;
    let spec = PtySpawnSpec {
        session_id: session_id.clone(),
        program,
        args: Vec::new(),
        cwd: primary_cwd,
        env: merged_env(&cfg.extra_env),
        cols: 120,
        rows: 32,
    };
    let tracer_spawn = tracer_client::spawn(
        &hub.dirs,
        spec,
        expected_output_subscribers(SessionMode::PlainShell),
    )
    .await
    .context("spawning plain shell via tracer")?;
    let pty = tracer_spawn.handle;
    let tracer_pid = tracer_spawn.tracer_pid;
    let tracer_pipe = tracer_spawn.pipe_name;
    let tracer_exe_path = tracer_spawn.tracer_exe_path.to_string_lossy().into_owned();
    let pid = pty.pid();

    let started_at = Utc::now();
    let mut record = SessionRecord {
        id: session_id.clone(),
        label: label.clone(),
        default_label: label.clone(),
        user_label: None,
        kind: kind.clone(),
        members: members.clone(),
        mode: SessionMode::PlainShell,
        started_at,
        // Shell sessions never transition status — they sit at Idle for their
        // whole lifetime, then `attach_lifecycle`'s exit watcher flips to
        // Stopped on exit.
        status: SessionStatus::Idle,
        exit_code: None,
        metrics: SessionMetrics::default(),
        recent_actions: Vec::new(),
        pty: Some(Arc::clone(&pty)),
        headless: None,
        workspace_id: workspace_id.clone(),
        // Plain-shell sessions have no agent CLI in the picture; label them
        // as claude so the UI badge doesn't render `· codex` on a pwsh prompt.
        agent: Agent::Claude,
        terminal_title: None,
        program_name: Some(shell_label.clone()),
        accent_color: None,
        spawn_config: Some(stored_config.clone()),
        is_abandoned: false,
        is_inactive: false,
        worktree_paths: worktree_paths_for_config(&stored_config, &members),
        // Plain shells never carry a kickoff prompt.
        last_prompt: None,
        input_notifier: None,
    };
    push_recent_action(&mut record, format!("session started: {shell_label}"));
    hub.sessions.insert(record);
    let snap = hub
        .sessions
        .get(&session_id)
        .map(|rec| crate::sync::lock(&rec).snapshot())
        .ok_or_else(|| anyhow!("session vanished"))?;

    if let Some(pid) = pid
        && let Ok(meta) = orphan::meta_from_record(
            session_id.clone(),
            pid,
            label,
            kind,
            SessionMode::PlainShell,
            members,
            started_at,
            workspace_id,
            Some("rt-tracer".to_string()),
            Agent::Claude,
            Some(stored_config),
            // Plain shells never carry a kickoff prompt.
            None,
            Some(tracer_pid),
            Some(tracer_pipe),
            Some(tracer_exe_path),
        )
    {
        // `shell_label` is now informational only — surfaced to the user via
        // the SessionRecord, not the sidecar's program_name (which describes
        // the OS process, which is rt-tracer).
        let _ = &shell_label;
        orphan::try_write_meta(&hub.dirs, &meta);
    }

    // Deliberately skip `pty_state::watch` — its Claude TUI prompt heuristic
    // would mis-classify a normal shell prompt as `AwaitingInput`. Keep
    // `osc_title::watch` so window-title escape sequences (which pwsh emits
    // by default) are recorded as `terminal_title` annotations; the canonical
    // session `label` stays whatever the spawn pipeline assigned.
    attach_lifecycle(
        &hub.sessions,
        session_id.clone(),
        &pty,
        Some(hub.dirs.clone()),
    );
    osc_title::watch(
        &hub.sessions,
        session_id.clone(),
        pty.output.subscribe(),
        hub.dirs.clone(),
    );
    Ok(snap)
}

#[expect(
    clippy::too_many_arguments,
    clippy::needless_pass_by_value,
    reason = "Constructing a session record is naturally wide; the String args are partly \
              consumed and partly cloned, splitting them by ownership clarifies nothing"
)]
fn spawn_headless_session(
    hub: &Hub,
    session_id: String,
    label: String,
    kind: SessionKind,
    members: Vec<SessionMember>,
    workspace_id: Option<String>,
    primary_cwd: PathBuf,
    prompt: String,
    cfg: &SpawnArgs,
    stored_config: protocol::SpawnConfig,
) -> anyhow::Result<protocol::SessionSnapshot> {
    // The dispatcher already rejects `agent == Codex` for headless mode; if
    // we got here, claude is the only valid agent.
    debug_assert!(matches!(cfg.agent, Agent::Claude));
    let last_prompt = Some(prompt.clone());
    let mut args: Vec<String> = vec![
        "--print".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
    ];
    for extra in members.iter().skip(1) {
        args.push("--add-dir".to_string());
        args.push(extra.worktree_path.clone());
    }
    if let Some(prelude) = build_workspace_prelude(&members) {
        args.push("--append-system-prompt".to_string());
        args.push(prelude);
    }
    cfg.extend_claude_args(&mut args);
    args.push("-p".to_string());
    args.push(prompt);

    let spec = headless::HeadlessSpec {
        program: agent_program(cfg.agent),
        args,
        cwd: primary_cwd,
        env: merged_env(&cfg.extra_env),
    };

    let started_at = Utc::now();
    let mut record = SessionRecord {
        id: session_id.clone(),
        label: label.clone(),
        default_label: label.clone(),
        user_label: None,
        kind: kind.clone(),
        members: members.clone(),
        mode: SessionMode::Headless,
        started_at,
        status: SessionStatus::Spawning,
        exit_code: None,
        metrics: SessionMetrics::default(),
        recent_actions: Vec::new(),
        pty: None,
        headless: None,
        workspace_id: workspace_id.clone(),
        agent: cfg.agent,
        terminal_title: None,
        program_name: Some(cfg.agent.as_label().to_string()),
        accent_color: None,
        spawn_config: Some(stored_config.clone()),
        is_abandoned: false,
        is_inactive: false,
        worktree_paths: worktree_paths_for_config(&stored_config, &members),
        last_prompt: last_prompt.clone(),
        input_notifier: None,
    };
    push_recent_action(&mut record, "headless session started".to_string());
    hub.sessions.insert(record);

    let handle = headless::spawn(&spec, &hub.sessions, session_id.clone(), hub.dirs.clone())
        .context("spawning headless claude")?;

    if let Some(pid) = handle.pid()
        && let Ok(meta) = orphan::meta_from_record(
            session_id.clone(),
            pid,
            label,
            kind,
            SessionMode::Headless,
            members,
            started_at,
            workspace_id,
            // Same as interactive: the child is a node shim on Windows.
            None,
            cfg.agent,
            Some(stored_config),
            last_prompt,
            // Headless `claude --print` doesn't use a PTY and therefore doesn't
            // go through rt-tracer; the tracer fields stay None.
            None,
            None,
            None,
        )
    {
        orphan::try_write_meta(&hub.dirs, &meta);
    }

    hub.sessions.update(&session_id, |rec| {
        rec.headless = Some(Arc::clone(&handle));
    });

    let snap = hub
        .sessions
        .get(&session_id)
        .map(|rec| crate::sync::lock(&rec).snapshot())
        .ok_or_else(|| anyhow!("session vanished"))?;
    Ok(snap)
}

async fn spawn_single(
    hub: &Hub,
    repo_id: &str,
    branch_name: &str,
    base_branch: Option<String>,
    use_worktree: bool,
) -> anyhow::Result<(
    SessionKind,
    Vec<SessionMember>,
    PathBuf,
    String,
    Option<String>,
)> {
    let repo = hub
        .state
        .with_persisted(|s| s.repos.iter().find(|r| r.id == repo_id).cloned())
        .ok_or_else(|| anyhow!("unknown repo: {repo_id}"))?;
    let repo_path = PathBuf::from(&repo.path);

    // The base used when the branch needs to be created. Explicit caller
    // value wins; otherwise we fall back to the repo's recorded default,
    // then to "main" as a last resort.
    let base_for_create = base_branch
        .or_else(|| repo.default_branch.clone())
        .unwrap_or_else(|| "main".to_string());

    let working_path = if use_worktree {
        let mut paths = git::workspace_worktree_paths(
            &hub.dirs.worktrees_dir,
            &[repo_path.as_path()],
            branch_name,
        );
        let worktree_path = paths
            .pop()
            .ok_or_else(|| anyhow!("workspace_worktree_paths returned empty for single repo"))?;
        if !worktree_path.exists() {
            git::worktree_add(
                &repo_path,
                &worktree_path,
                branch_name,
                Some(&base_for_create),
            )
            .await
            .context("creating worktree")?;
        }
        worktree_path
    } else {
        git::checkout_in_place(&repo_path, branch_name, Some(&base_for_create))
            .await
            .context("checking out branch in-place")?;
        repo_path.clone()
    };

    let member = SessionMember {
        repo_id: repo.id.clone(),
        repo_name: repo.name.clone(),
        branch: branch_name.to_string(),
        worktree_path: working_path.to_string_lossy().into_owned(),
    };
    let label = format!("{}: {branch_name}", repo.name);
    Ok((SessionKind::Single, vec![member], working_path, label, None))
}

async fn spawn_workspace(
    hub: &Hub,
    workspace_id: &str,
    branch_name: &str,
    base_branch: Option<String>,
    use_worktree: bool,
) -> anyhow::Result<(
    SessionKind,
    Vec<SessionMember>,
    PathBuf,
    String,
    Option<String>,
)> {
    info!(
        workspace_id,
        branch_name, use_worktree, "spawn_workspace: begin"
    );
    let (workspace, resolved) = ws::resolve_workspace(
        &hub.state,
        &hub.dirs.worktrees_dir,
        workspace_id,
        branch_name,
        base_branch.as_deref(),
        use_worktree,
    )
    .await?;
    ws::ensure_branches(&resolved, branch_name).await?;

    let primary = resolved
        .first()
        .ok_or_else(|| anyhow!("workspace has no members"))?
        .working_path
        .clone();
    let members: Vec<SessionMember> = resolved
        .iter()
        .map(|m| SessionMember {
            repo_id: m.repo.id.clone(),
            repo_name: m.repo.name.clone(),
            branch: branch_name.to_string(),
            worktree_path: m.working_path.to_string_lossy().into_owned(),
        })
        .collect();
    let label = format!("{}: {branch_name}", workspace.name);
    Ok((
        SessionKind::Workspace,
        members,
        primary,
        label,
        Some(workspace.id.clone()),
    ))
}

/// Resume an abandoned session: read its sidecar (spawn config +
/// `last_prompt`), spawn a fresh session with the same config and the
/// captured prompt, then remove the abandoned placeholder from the
/// registry + delete the on-disk sidecar.
///
/// The new session gets a new id (`spawn_session` generates one), so
/// the old abandoned record's UI bindings are NOT automatically
/// transferred. The frontend can listen for the `SessionUpdated`
/// message and decide whether to swap it into the same tab/pane.
async fn resume_abandoned(
    hub: &Hub,
    session_id: &str,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) -> anyhow::Result<()> {
    let rec = hub
        .sessions
        .get(session_id)
        .ok_or_else(|| anyhow!("unknown session: {session_id}"))?;
    let (is_abandoned, spawn_config, last_prompt) = {
        let guard = crate::sync::lock(&rec);
        (
            guard.is_abandoned,
            guard.spawn_config.clone(),
            guard.last_prompt.clone(),
        )
    };
    if !is_abandoned {
        return Err(anyhow!(
            "session {session_id} is not abandoned; refusing to resume"
        ));
    }
    let Some(stored) = spawn_config else {
        return Err(anyhow!(
            "session {session_id} has no stored spawn config; cannot resume"
        ));
    };
    let mut req = stored.to_clone_request();
    req.initial_prompt = last_prompt;
    let snap = spawn_session(hub, req).await?;

    // Remove the abandoned placeholder and delete the sidecar atomically
    // after the spawn succeeds. If spawn failed, the placeholder stays
    // so the user can try again.
    discard_abandoned(hub, session_id, out_tx);

    let _ = out_tx.send(DaemonMessage::SessionUpdated { session: snap });
    Ok(())
}

/// Resume every session currently flagged as abandoned. Iterates the
/// registry snapshot (taken upfront to avoid mutating it while iterating),
/// calls `resume_abandoned` for each, and emits an `Error` message per
/// failure so the user can see which ones didn't make it. Successful
/// resumes are handled by `resume_abandoned` itself (it emits
/// `SessionUpdated` and consumes the sidecar).
async fn resume_all_abandoned(hub: &Hub, out_tx: &mpsc::UnboundedSender<DaemonMessage>) {
    let abandoned_ids: Vec<String> = hub
        .sessions
        .snapshots()
        .into_iter()
        .filter(|s| s.is_abandoned)
        .map(|s| s.id)
        .collect();
    info!(
        count = abandoned_ids.len(),
        "resume_all_abandoned: starting bulk resume"
    );
    for id in abandoned_ids {
        if let Err(err) = resume_abandoned(hub, &id, out_tx).await {
            warn!(session_id = %id, ?err, "bulk resume: per-session failure");
            let _ = out_tx.send(DaemonMessage::Error {
                message: format!("resume {id}: {err}"),
            });
        }
    }
}

/// Remove an abandoned session from the registry + delete its sidecar.
/// Used both as the cleanup step after a successful Resume and as the
/// user-initiated "Dismiss" action for sessions they don't want to
/// recover.
fn discard_abandoned(hub: &Hub, session_id: &str, out_tx: &mpsc::UnboundedSender<DaemonMessage>) {
    orphan::try_delete_meta(&hub.dirs, session_id);
    orphan::try_delete_session_dir(&hub.dirs, session_id);
    hub.sessions.remove(session_id);
    let _ = out_tx.send(DaemonMessage::SessionRemoved {
        session_id: session_id.to_string(),
    });
}

/// Kill the session's process if still alive, then mark it `is_inactive =
/// true` in the registry so the sidebar shows it as parked. The session
/// record stays in the registry — the user can Resume it later.
async fn park_session(hub: &Hub, session_id: &str) {
    let Some(rec) = hub.sessions.get(session_id) else {
        return;
    };
    let (pty, headless_handle) = {
        let guard = crate::sync::lock(&rec);
        (guard.pty.clone(), guard.headless.clone())
    };
    if let Some(pty) = pty {
        pty.kill();
    }
    if let Some(h) = headless_handle {
        h.kill().await;
    }
    hub.sessions.update(session_id, |r| {
        r.is_inactive = true;
        r.pty = None;
        r.headless = None;
        r.input_notifier = None;
        if !matches!(
            r.status,
            protocol::SessionStatus::Stopped | protocol::SessionStatus::Error
        ) {
            r.status = protocol::SessionStatus::Stopped;
        }
        push_recent_action(r, "parked — worktree retained".to_string());
    });
    // Detach from panes so the grid shows an empty slot; the session record
    // itself lives on in the registry for the sidebar's Resume button.
    prune_session_from_tabs(hub, session_id);
}

/// Remove a stopped or inactive session from the registry. Optionally removes
/// per-member worktrees from disk. Does not kill a running session.
async fn discard_session(hub: &Hub, session_id: &str, cleanup: &[protocol::CleanupAction]) {
    let (members, has_per_session_worktree) = hub
        .sessions
        .get(session_id)
        .map(|rec| {
            let guard = crate::sync::lock(&rec);
            let spawned_with_worktree =
                guard
                    .spawn_config
                    .as_ref()
                    .is_some_and(|cfg| match &cfg.target {
                        SpawnTarget::Single { use_worktree, .. }
                        | SpawnTarget::Workspace { use_worktree, .. } => *use_worktree,
                    });
            (
                guard.members.clone(),
                spawned_with_worktree || !guard.worktree_paths.is_empty(),
            )
        })
        .unwrap_or_else(|| (Vec::new(), false));

    for action in cleanup {
        if !action.remove_worktree || !has_per_session_worktree {
            continue;
        }
        let Some(member) = members.iter().find(|m| m.repo_id == action.repo_id) else {
            continue;
        };
        let repo_path = hub.state.with_persisted(|s| {
            s.repos
                .iter()
                .find(|r| r.id == member.repo_id)
                .map(|r| r.path.clone())
        });
        let Some(repo_path) = repo_path else {
            continue;
        };
        let worktree_path = PathBuf::from(&member.worktree_path);
        if let Err(err) = git::worktree_remove(Path::new(&repo_path), &worktree_path).await {
            warn!(?err, "discard_session: worktree remove failed");
        }
    }
    orphan::try_delete_meta(&hub.dirs, session_id);
    orphan::try_delete_session_dir(&hub.dirs, session_id);
    hub.sessions.remove(session_id);
    prune_session_from_tabs(hub, session_id);
}

async fn stop_session(hub: &Hub, session_id: &str) -> anyhow::Result<()> {
    let Some(rec) = hub.sessions.get(session_id) else {
        return Err(anyhow!("unknown session: {session_id}"));
    };
    let (pty, headless_handle) = {
        let guard = crate::sync::lock(&rec);
        (guard.pty.clone(), guard.headless.clone())
    };
    let had_live_handle = pty.is_some() || headless_handle.is_some();
    if let Some(pty) = pty {
        pty.kill();
    }
    if let Some(h) = headless_handle {
        h.kill().await;
    }
    if !had_live_handle {
        // Orphan session — neither PTY nor headless handle survived the
        // daemon restart. Read the sidecar to recover the pid and try a
        // best-effort kill. Without this, `stop_session` on an orphan
        // would mark daemon state as stopped but leave the underlying
        // claude / shell process running forever.
        match orphan::load_meta(&hub.dirs, session_id) {
            Ok(meta) => {
                if orphan::kill_pid(&meta) {
                    tracing::info!(%session_id, pid = meta.pid, "killed orphan process");
                } else {
                    tracing::warn!(
                        %session_id,
                        pid = meta.pid,
                        "orphan kill returned false (pid gone, name mismatch, or signal denied)",
                    );
                }
            }
            Err(err) => {
                tracing::warn!(?err, %session_id, "no orphan sidecar to drive kill");
            }
        }
    }
    hub.sessions.update(session_id, |r| {
        r.status = protocol::SessionStatus::Stopped;
        r.pty = None;
        r.headless = None;
        r.input_notifier = None;
        r.is_inactive = false;
        push_recent_action(r, "stopped by user".to_string());
    });
    orphan::try_delete_meta(&hub.dirs, session_id);
    Ok(())
}

/// Drop dangling pane references to `session_id` from every tab. Each
/// affected tab is broadcast as a fresh `TabUpdated` so all connected clients
/// switch the pane to the empty-placeholder state.
fn prune_session_from_tabs(hub: &Hub, session_id: &str) {
    let modified = match hub.state.mutate(|s| {
        let mut out = Vec::new();
        for tab in &mut s.tabs {
            let Some(grid) = tab.grid_mut() else {
                continue;
            };
            if tabs::prune_session(grid, session_id) {
                out.push(tab.clone());
            }
        }
        out
    }) {
        Ok(list) => list,
        Err(err) => {
            warn!(?err, "prune_session_from_tabs: state.mutate failed");
            return;
        }
    };
    for tab in modified {
        let _ = hub.tab_events.send(TabEvent::Updated(tab));
    }
}

fn agent_program(agent: Agent) -> String {
    match agent {
        Agent::Claude => {
            std::env::var("RUSTLING_TULIP_CLAUDE").unwrap_or_else(|_| "claude".to_string())
        }
        Agent::Codex => {
            std::env::var("RUSTLING_TULIP_CODEX").unwrap_or_else(|_| "codex".to_string())
        }
    }
}

/// Resolve an agent-program name into something `CreateProcess` /
/// portable-pty can actually launch. Returns `(program, prepended_args)`.
///
/// On Windows, agent CLIs distributed via npm (e.g. codex from
/// `@openai/codex`) land on PATH as `.cmd` shims. Two layers of fixup
/// matter here:
///
/// 1. `CreateProcess` does NOT execute `.cmd` or `.bat` directly, so
///    a raw `.cmd` path passed to portable-pty silently fails (the
///    tracer's pipe never opens; daemon times out after 10s).
/// 2. Even wrapping with `cmd.exe /d /c <shim>.cmd` is enough for the
///    process to launch — but inside portable-pty's pseudo-console
///    that chain (cmd.exe → npm shim → node.exe → real binary) ends
///    up swallowing the child's stdout: codex.exe never renders. The
///    same `cmd.exe /d /c codex.cmd …` argv works fine in a real
///    Windows console and from `PowerShell`, so it's specific to the
///    portable-pty + cmd.exe boundary.
///
/// We dodge both by, when the `.cmd` is an npm-style shim, finding
/// the underlying `node_modules/.../bin/<name>.js` entry and spawning
/// `node.exe <entry> <args>` directly. That removes cmd.exe from the
/// chain entirely and the JS launcher still does its
/// platform-detection / PATH-augmentation work as usual.
///
/// Order of preference for a `.cmd` shim:
///   1. npm shim → `(node.exe, [<js-entry>, ...])`.
///   2. Fallback → `(cmd.exe, ["/d", "/c", <shim-path>, ...])`.
///
/// Native `.exe`s and anything else flow through with the resolved
/// path unchanged. When `which` can't find the program at all (custom
/// env-var override that hasn't been deployed yet, etc.) the input
/// string is returned as-is and we let the tracer surface whatever
/// error `CreateProcess` produces — that's more useful diagnostics
/// than guessing.
fn resolve_agent_program(name: &str) -> (String, Vec<String>) {
    let Ok(path) = which::which(name) else {
        return (name.to_string(), Vec::new());
    };
    let path_str = path.to_string_lossy().into_owned();
    #[cfg(windows)]
    {
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase);
        if matches!(ext.as_deref(), Some("cmd" | "bat"))
            && let Some((node, js)) = npm_shim_to_node_entry(&path)
        {
            return (
                node.to_string_lossy().into_owned(),
                vec![js.to_string_lossy().into_owned()],
            );
        }
        if matches!(ext.as_deref(), Some("cmd" | "bat")) {
            return (
                "cmd.exe".to_string(),
                vec!["/d".to_string(), "/c".to_string(), path_str],
            );
        }
    }
    (path_str, Vec::new())
}

/// For a `.cmd` path that looks like an npm-generated shim, locate
/// the JS entrypoint it ultimately invokes (`<dir>/node_modules/<pkg>/
/// bin/<stem>.js`, where `<pkg>` may be scoped as `@org/name`). Also
/// resolves a `node.exe` to run it with: prefers a sibling next to
/// the shim (nvm-style install layout) over a PATH-resolved one, so
/// node-version managers stay self-consistent.
///
/// Returns `None` if the shim doesn't match the npm pattern (custom
/// wrapper `.cmd`, non-Node CLI, etc.) — caller falls back to
/// `cmd.exe /d /c <shim>`.
#[cfg(windows)]
fn npm_shim_to_node_entry(shim: &Path) -> Option<(PathBuf, PathBuf)> {
    let dir = shim.parent()?;
    let stem = shim.file_stem()?.to_str()?;
    let nm = dir.join("node_modules");
    if !nm.is_dir() {
        return None;
    }
    let target_filename = format!("{stem}.js");

    let entries = std::fs::read_dir(&nm).ok()?;
    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_str()?;
        if name_str.starts_with('@') {
            // Scoped: walk one more level (e.g. node_modules/@openai/codex/bin/codex.js).
            if let Ok(inner) = std::fs::read_dir(&p) {
                for sub in inner.flatten() {
                    candidates.push(sub.path().join("bin").join(&target_filename));
                }
            }
        } else {
            // Unscoped: node_modules/<pkg>/bin/<stem>.js.
            candidates.push(p.join("bin").join(&target_filename));
        }
    }

    let js = candidates.into_iter().find(|c| c.is_file())?;

    let sibling_node = dir.join("node.exe");
    let node = if sibling_node.is_file() {
        sibling_node
    } else {
        which::which("node").ok()?
    };

    Some((node, js))
}

/// Pick a shell executable for [`SessionMode::PlainShell`] spawns. Returns
/// `(program, label)` where `program` is what we pass to `CommandBuilder`
/// (resolved on PATH if it has no separator) and `label` is the short token
/// used for `recent_actions` text and for the orphan-recovery name match.
///
/// Resolution order:
/// - `RUSTLING_TULIP_SHELL` env override (any platform);
/// - Windows: `pwsh.exe` → `powershell.exe` → `cmd.exe`;
/// - Unix: `$SHELL` → `bash` → `sh`.
fn resolve_shell_program() -> anyhow::Result<(String, String)> {
    if let Ok(p) = std::env::var("RUSTLING_TULIP_SHELL") {
        let label = shell_label_from_path(&p);
        return Ok((p, label));
    }
    #[cfg(windows)]
    {
        for (candidate, label) in [
            ("pwsh.exe", "pwsh"),
            ("powershell.exe", "powershell"),
            ("cmd.exe", "cmd"),
        ] {
            if which::which(candidate).is_ok() {
                return Ok((candidate.to_string(), label.to_string()));
            }
        }
        anyhow::bail!("no shell on PATH (tried pwsh.exe, powershell.exe, cmd.exe)")
    }
    #[cfg(not(windows))]
    {
        if let Ok(p) = std::env::var("SHELL") {
            let label = shell_label_from_path(&p);
            return Ok((p, label));
        }
        for (candidate, label) in [("bash", "bash"), ("sh", "sh")] {
            if which::which(candidate).is_ok() {
                return Ok((candidate.to_string(), label.to_string()));
            }
        }
        anyhow::bail!("no shell on PATH (tried bash, sh)")
    }
}

fn shell_label_from_path(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .map_or_else(|| "shell".to_string(), str::to_lowercase)
}

fn scan_vscode_workspaces(hub: &Hub, repo_path: &str) -> Vec<VscodeWorkspaceSuggestion> {
    let known: Vec<(String, String)> = hub.state.with_persisted(|s| {
        s.repos
            .iter()
            .map(|r| (r.id.clone(), r.path.clone()))
            .collect()
    });
    let dir = std::path::Path::new(repo_path);
    vscode::find_workspace_files(dir)
        .into_iter()
        .filter_map(|file| match vscode::parse_workspace_file(&file, &known) {
            Ok(s) if s.folders.len() > 1 => Some(s),
            Ok(_) => None,
            Err(err) => {
                warn!(?err, file = %file.display(), "failed to parse code-workspace");
                None
            }
        })
        .collect()
}

async fn accept_vscode_suggestion(
    hub: &Hub,
    suggestion: VscodeWorkspaceSuggestion,
    _watch: bool,
    _out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) -> anyhow::Result<()> {
    let mut member_repo_ids = Vec::with_capacity(suggestion.folders.len());
    for folder in &suggestion.folders {
        let repo_id = if let Some(id) = folder.matched_repo_id.clone() {
            id
        } else {
            let entry = add_repo(&hub.state, &folder.path, folder.name.clone()).await?;
            entry.id
        };
        member_repo_ids.push(repo_id);
    }
    upsert_workspace(
        &hub.state,
        None,
        &suggestion.suggested_name,
        member_repo_ids,
        Some(suggestion.source_path.clone()),
    )?;
    let (repos, workspaces) = hub
        .state
        .with_persisted(|s| (s.repos.clone(), s.workspaces.clone()));
    let _ = hub.state_events.send(StateEvent::Repos(repos));
    let _ = hub.state_events.send(StateEvent::Workspaces(workspaces));
    Ok(())
}

fn passthrough_env() -> Vec<(String, String)> {
    let keep = [
        "PATH",
        "HOME",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "TEMP",
        "TMP",
        "TERM",
        "LANG",
        "LC_ALL",
        "ANTHROPIC_API_KEY",
        "CLAUDE_CONFIG_DIR",
    ];
    let mut out = Vec::new();
    for k in keep {
        if let Ok(v) = std::env::var(k) {
            out.push((k.to_string(), v));
        }
    }
    out.push(("TERM".to_string(), "xterm-256color".to_string()));
    out
}

/// Build the env list for a spawn: the daemon's keep-list plus user-supplied
/// `extra_env`, with `extra_env` overriding the keep-list on key collision so
/// users can override values like `ANTHROPIC_API_KEY`. Deduplicated to avoid
/// passing the same key twice to the child process.
fn merged_env(extra: &[(String, String)]) -> Vec<(String, String)> {
    let mut out = passthrough_env();
    for (k, v) in extra {
        if let Some(slot) = out.iter_mut().find(|(existing, _)| existing == k) {
            slot.1.clone_from(v);
        } else {
            out.push((k.clone(), v.clone()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{InjectorStep, PromptInjector};

    fn member(repo: &str, path: &str) -> SessionMember {
        SessionMember {
            repo_id: repo.to_string(),
            repo_name: repo.to_string(),
            branch: "main".to_string(),
            worktree_path: path.to_string(),
        }
    }

    fn cfg(agent: Agent) -> SpawnArgs {
        SpawnArgs {
            dangerously_skip_permissions: false,
            agent,
            model: None,
            permission_mode: None,
            codex_sandbox: None,
            extra_env: Vec::new(),
            prompt_injector: None,
        }
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
    fn codex_single_repo_sandbox_workspace_write_with_prompt() {
        let members = vec![member("r1", "X:/worktree/r1")];
        let mut c = cfg(Agent::Codex);
        c.codex_sandbox = Some(CodexSandbox::WorkspaceWrite);
        let args = build_codex_args(&c, &members, Some("hello"));
        assert_eq!(args, vec!["--sandbox", "workspace-write", "hello"]);
    }

    #[test]
    fn codex_workspace_emits_add_dir_per_extra_member() {
        let members = vec![
            member("r1", "X:/wt/yaat"),
            member("r2", "X:/wt/yaat-server"),
            member("r3", "X:/wt/yaat-shared"),
        ];
        let c = cfg(Agent::Codex);
        let args = build_codex_args(&c, &members, None);
        // No user prompt + 3 members → positional is the workspace
        // prelude alone.
        assert_eq!(args[0], "--add-dir");
        assert_eq!(args[1], "X:/wt/yaat-server");
        assert_eq!(args[2], "--add-dir");
        assert_eq!(args[3], "X:/wt/yaat-shared");
        assert_eq!(
            args.len(),
            5,
            "expected 4 add-dir args + 1 prelude positional"
        );
        let prelude = &args[4];
        assert!(prelude.contains("Workspace member paths"));
        assert!(prelude.contains("r1"));
        assert!(prelude.contains("X:/wt/yaat-server"));
    }

    #[test]
    fn build_workspace_prelude_skips_single_member() {
        let members = vec![member("r1", "X:/wt/r1")];
        assert!(build_workspace_prelude(&members).is_none());
    }

    #[test]
    fn build_workspace_prelude_lists_each_member() {
        let members = vec![
            member("yaat", "X:/wt/yaat"),
            member("yaat-server", "X:/wt/yaat-server"),
        ];
        let prelude = build_workspace_prelude(&members).unwrap_or_default();
        assert!(
            !prelude.is_empty(),
            "prelude should be present for 2 members"
        );
        // Each member name maps to its worktree path on its own line.
        // Width padding aligns them — assert on the parts, not the exact
        // column count, so this test won't churn if formatting evolves.
        assert!(prelude.contains("yaat"));
        assert!(prelude.contains("X:/wt/yaat\n"));
        assert!(prelude.contains("yaat-server"));
        assert!(prelude.contains("X:/wt/yaat-server\n"));
        assert!(prelude.contains("->"));
    }

    #[test]
    fn codex_workspace_with_user_prompt_concatenates() {
        let members = vec![member("r1", "X:/wt/r1"), member("r2", "X:/wt/r2")];
        let c = cfg(Agent::Codex);
        let args = build_codex_args(&c, &members, Some("do the thing"));
        let positional = args.last().cloned().unwrap_or_default();
        assert!(positional.starts_with("Workspace member paths"));
        assert!(positional.ends_with("do the thing"));
    }

    #[test]
    fn codex_yolo_overrides_sandbox() {
        let members = vec![member("r1", "X:/wt/r1")];
        let mut c = cfg(Agent::Codex);
        c.dangerously_skip_permissions = true;
        c.codex_sandbox = Some(CodexSandbox::ReadOnly);
        let args = build_codex_args(&c, &members, Some("go"));
        assert_eq!(args, vec!["--yolo", "go"]);
        assert!(!args.iter().any(|a| a == "--sandbox"));
    }

    #[test]
    fn codex_model_flag_precedes_sandbox() {
        let members = vec![member("r1", "X:/wt/r1")];
        let mut c = cfg(Agent::Codex);
        c.model = Some("gpt-5.1-codex".to_string());
        c.codex_sandbox = Some(CodexSandbox::DangerFullAccess);
        let args = build_codex_args(&c, &members, None);
        assert_eq!(
            args,
            vec![
                "--model",
                "gpt-5.1-codex",
                "--sandbox",
                "danger-full-access"
            ]
        );
    }

    #[test]
    fn codex_prompt_injector_suppresses_positional_prompt() {
        let members = vec![member("r1", "X:/wt/r1")];
        let mut c = cfg(Agent::Codex);
        c.prompt_injector = Some(PromptInjector {
            steps: vec![InjectorStep::Text {
                content: "injected".to_string(),
                newline: true,
            }],
        });
        let args = build_codex_args(&c, &members, Some("ignored"));
        assert!(!args.iter().any(|a| a == "ignored"));
    }

    #[test]
    fn codex_no_sandbox_no_yolo_omits_permission_flags() {
        let members = vec![member("r1", "X:/wt/r1")];
        let c = cfg(Agent::Codex);
        let args = build_codex_args(&c, &members, None);
        assert!(args.is_empty(), "expected empty args, got {args:?}");
    }
}
