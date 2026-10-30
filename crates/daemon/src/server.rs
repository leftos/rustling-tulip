//! WebSocket server, message dispatch, and session orchestration glue.

#[cfg(test)]
pub(crate) mod test_support;

mod remote;
pub(crate) use self::remote::RemoteSlot;

mod chat;
mod connection;
mod dashboard;
mod handshake;
pub(crate) use self::handshake::TAILNET_HANDSHAKE_TIMEOUT;

mod git;
mod tabs;

mod hooks;
pub(crate) mod live_devices;
mod pairing;
mod recovery;
mod screen;
mod settings;
mod summarizer;
pub(crate) use self::recovery::RecoveryClaims;

mod sessions;

mod spawn;
pub(crate) use self::spawn::resolve_agent_program;
#[expect(
    unused_imports,
    reason = "doc comments in history.rs name these at their crate::server path"
)]
pub(crate) use self::spawn::resolve_env_refs;
pub(crate) use self::spawn::spawn_session;

use self::connection::{AuthKind, ConnCtx, cancel_fetch, client_session, start_fetch};
use self::git::repo_target_or_err;
use self::hooks::{hook_handler, presented_bearer, proof_handler};
use self::recovery::{reattach_orphans, start_history_broadcaster};
use self::remote::{
    configure_remote, host_only_refusal, remove_retired_lan_files, start_remote_access,
};
use self::sessions::shutdown_all;
use self::settings::start_keep_awake;
use crate::orphan::OrphanMeta;
use crate::paths::Dirs;
use crate::session::SessionRegistry;
use crate::state::AppState;
use crate::tailnet_listener::TailnetListener;
use crate::{file_fetch, pty_state};
use anyhow::Context as _;
use axum::Router;
use axum::extract::State;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use protocol::{
    ClientMessage, DaemonHandshake, DaemonMessage, PROTOCOL_VERSION, PresetLaunchJobSnapshot,
    SUPPORTED_PROTOCOL_VERSIONS, TabEntry,
};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc};
use tracing::{error, info, warn};

#[derive(Clone)]
pub struct Hub {
    pub state: Arc<AppState>,
    pub sessions: Arc<SessionRegistry>,
    pub auth_token: String,
    /// `tracer_protocol::daemon_proof_key(auth_token)`, derived once per run:
    /// the key `/proof` answers the hook shim's challenges with.
    pub proof_key: [u8; 32],
    /// How long a tailnet connection has to send its `Hello` before the
    /// handshake is refused. Production sets [`TAILNET_HANDSHAKE_TIMEOUT`]; a
    /// test moves it out of the way to observe a connection that is still
    /// waiting, or shrinks it to pin the timeout branch.
    pub tailnet_handshake_timeout: Duration,
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
    pub tab_events: broadcast::Sender<ScopedTabEvent>,
    /// Broadcast for preset-launch progress/failure events. Every connected
    /// client subscribes and forwards. Lets the launching client auto-switch
    /// tabs while the batch runs, and lets other clients see new tabs appear.
    pub preset_events: broadcast::Sender<PresetEvent>,
    /// Cancellation handles for in-flight preset launch jobs. The launch task
    /// inserts a sender on start and removes it on completion/failure/cancel.
    pub preset_cancellations: Arc<AsyncMutex<HashMap<String, tokio::sync::watch::Sender<bool>>>>,
    /// The latest snapshot of each preset job (running jobs and the newest
    /// finished ones), written before each `JobUpdated` broadcast and resent
    /// to a connection whose preset-event stream lags.
    pub preset_job_snapshots: Arc<std::sync::Mutex<crate::presets::PresetJobSnapshots>>,
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
    /// The persisted remote-access switch: `true` while `remote.json` says
    /// remote access is on, whatever the listener's own state. Idle exit
    /// reads it so a host with remote access on never exits.
    pub remote_enabled: Arc<tokio::sync::watch::Sender<bool>>,
    /// The running tailnet listener, if remote access is on and it started.
    /// Dropping it stops the listener; the daemon keeps it for its lifetime.
    pub tailnet: Arc<AsyncMutex<Option<TailnetListener>>>,
    /// The current `RemoteStatus` snapshot and the generation of the listener
    /// it follows. Every client gets the snapshot at connect (the
    /// initial-state push reads it) and again on every change. Written at
    /// startup by `start_remote_access` (through `begin_remote_generation`),
    /// by [`ClientMessage::ConfigureRemote`] and by the listener's status
    /// relay.
    pub remote_status: Arc<tokio::sync::watch::Sender<RemoteSlot>>,
    /// The persisted keep-awake mode, published to the watcher.
    /// [`ClientMessage::SetKeepAwakeMode`] writes it here after saving to
    /// `state.json`; the watcher re-derives the OS hold and broadcasts the
    /// resulting status itself.
    pub keep_awake_mode: Arc<tokio::sync::watch::Sender<protocol::KeepAwakeMode>>,
    /// Latest keep-awake status from the watcher, read by the initial-state
    /// push so a freshly-connected client renders the toggle correctly
    /// without waiting for the next broadcast.
    pub keep_awake_status: tokio::sync::watch::Receiver<crate::keep_awake::Status>,
    /// The session ids a recovery or a Resume is spawning for right now, so
    /// the two never start the same session twice.
    pub recovering: RecoveryClaims,
    /// True while the start-up resume of the sessions the last run lost is
    /// spawning them; idle exit holds off meanwhile.
    pub resume_in_flight: Arc<std::sync::atomic::AtomicBool>,
    /// How many sessions the start-up resume brought back and how many it
    /// left for Recover, once it has finished.
    pub start_resume: Arc<std::sync::Mutex<Option<(usize, usize)>>>,
    /// The client ids already told how the start-up resume went, so each is
    /// told once per daemon run whichever path reaches it first.
    pub resume_reported: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Hook events that arrived for a session id before its spawn inserted
    /// it; applied to the record at insertion (bounded by age and count).
    pub pending_hooks: crate::hook_status::PendingHooks,
    /// Each session's live-line channel, fed by the hook route.
    pub live_lines: crate::hook_status::LiveLines,
    /// The chats loaded by connections, served from each session's transcript
    /// and screen.
    pub chat_mirror: crate::chat_mirror::ChatMirror,
    /// The approved tailnet devices, one record each holding only the hex
    /// SHA-256 of its token (`config/devices.json`).
    pub devices: Arc<std::sync::Mutex<crate::devices::DeviceStore>>,
    /// The pending pairing requests and CLI codes, in memory only.
    pub pairing: Arc<std::sync::Mutex<crate::devices::PairingBook>>,
    /// Each waiting pairing request's connection, by request id: how a
    /// decision reaches the device that asked. Locked only after
    /// [`Hub::pairing`] when both are held.
    pub pairing_waiters: pairing::PairingWaiters,
    /// Each approved device's served tailnet connections, so a block or a
    /// forget cuts them at once. Locked before [`Hub::devices`] when both
    /// are held.
    pub live_devices: live_devices::LiveDevices,
    /// The Dashboard's facts service: its watchers, its reader cache and the
    /// facts it last sent each watcher.
    pub dashboard: crate::dashboard::Dashboard,
    /// The Dashboard summarizer: its settings, key states, queue, cache and
    /// the day's spend.
    pub summarizer: Arc<crate::summarizer::service::Summarizer>,
    /// The turn pipeline: each session's unsummarized reports, summarized
    /// through [`Self::summarizer`] into the session's summary ring.
    pub(crate) turns: Arc<crate::summarizer::turns::Turns>,
}

impl Hub {
    /// Broadcast a tab-layout change scoped to one client's layout. Only that
    /// client's connections (window + same-machine pop-outs) forward it.
    pub(crate) fn emit_tab(&self, client_id: &str, event: TabEvent) {
        let _ = self.tab_events.send(ScopedTabEvent {
            client_id: client_id.to_string(),
            event,
        });
    }

    /// Whether `client_id` has not yet been told this run's resume report;
    /// marks it told when it had not. The first path to reach a client — the
    /// initial-state push or the broadcast — is the one that sends.
    pub(crate) fn claim_resume_report(&self, client_id: &str) -> bool {
        crate::sync::lock(&self.resume_reported).insert(client_id.to_owned())
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
    /// to re-request explicitly. `worktree_path = None` means the registered
    /// repo's main tree; `Some(...)` is a per-session worktree.
    RepoStatus {
        repo_id: String,
        worktree_path: Option<String>,
        index_changes: Vec<protocol::GitFileChange>,
        worktree_changes: Vec<protocol::GitFileChange>,
    },
    /// Broadcast after a successful stash push/pop/drop so the sidebar's
    /// STASHES section stays in sync across multiple client windows.
    Stashes {
        repo_id: String,
        worktree_path: Option<String>,
        stashes: Vec<protocol::GitStash>,
    },
    /// Broadcast whenever the user-customized worktrees root changes (or
    /// the user clears the override and reverts to default). Lets every
    /// connected client refresh its Settings UI and any open worktree-
    /// management modal without polling.
    WorktreesRootChanged {
        root: String,
        is_override: bool,
    },
    /// Broadcast by the keep-awake watcher whenever the setting changes or the
    /// hold engages/releases as sessions come and go, so every connected
    /// client's Settings UI reflects it without polling. Mirrors
    /// [`protocol::DaemonMessage::KeepAwakeStatus`].
    KeepAwakeStatus {
        mode: protocol::KeepAwakeMode,
        active: bool,
    },
    /// Broadcast after every [`ClientMessage::SetAgentCommand`] that is
    /// saved. Mirrors [`protocol::DaemonMessage::AgentCommands`].
    AgentCommands(Vec<protocol::AgentCommandEntry>),
    /// Broadcast after every [`ClientMessage::SetAgentSettings`] that is
    /// saved: every agent's stored settings. Mirrors
    /// [`protocol::DaemonMessage::AgentSettings`].
    AgentSettings(std::collections::BTreeMap<protocol::AgentId, protocol::OptionValues>),
    /// Broadcast on every remote-access status change: a
    /// [`ClientMessage::ConfigureRemote`] or the tailnet listener's own state
    /// moving. Mirrors [`protocol::DaemonMessage::RemoteStatus`]; it carries
    /// no token, so every client may see it.
    RemoteStatus {
        enabled: bool,
        port: u16,
        state: protocol::RemoteState,
    },
    /// Broadcast by the start-up resume when it finishes, so a client that
    /// connected while it ran learns how it went. Mirrors
    /// [`protocol::DaemonMessage::ResumeReport`]; the initial-state push
    /// serves a client that connects after it.
    ResumeReport {
        resumed: u32,
        left_in_recover: u32,
    },
    /// The whole session history, broadcast after it changes.
    SessionHistory(Vec<protocol::SessionHistoryItem>),
    /// The waiting pairing requests, broadcast whenever the list changes.
    /// Mirrors [`protocol::DaemonMessage::PendingDevices`]; only loopback
    /// connections forward it.
    PendingDevices(Vec<protocol::PendingDevice>),
    /// Broadcast on every summarizer change: its settings, a key set,
    /// deleted or refused, and each billed call. Mirrors
    /// [`protocol::DaemonMessage::SummarizerStatus`]; it carries key states,
    /// never a key.
    SummarizerStatus {
        settings: protocol::SummarizerSettings,
        providers: Vec<protocol::SummarizerProvider>,
        keys: Vec<protocol::SummarizerKey>,
        spend_today_usd: f64,
        cap_reached: bool,
    },
}

#[derive(Debug, Clone)]
pub enum TabEvent {
    Updated(TabEntry),
    Removed(String),
    Reordered(Vec<String>),
}

/// A [`TabEvent`] tagged with the `client_id` whose layout it belongs to. Tab
/// layouts are per-client, so a connection only forwards events scoped to its
/// own `client_id` (a window + its same-machine pop-outs share one).
#[derive(Debug, Clone)]
pub struct ScopedTabEvent {
    pub client_id: String,
    pub event: TabEvent,
}

#[derive(Debug, Clone)]
pub enum PresetEvent {
    JobUpdated {
        job: PresetLaunchJobSnapshot,
    },
    Progress {
        job_id: String,
        preset_id: String,
        total: u32,
        launched: u32,
        current_tab_id: Option<String>,
        tab_ids: Vec<String>,
    },
    Failed {
        job_id: String,
        preset_id: String,
        error: String,
        partial_session_ids: Vec<String>,
        partial_tab_ids: Vec<String>,
    },
}

const TAB_EVENT_CAPACITY: usize = 256;
const PRESET_EVENT_CAPACITY: usize = 64;
const STATE_EVENT_CAPACITY: usize = 64;

/// Forward every attention event into the registry's session-event broadcast.
fn spawn_attention_forwarder(
    sessions: Arc<SessionRegistry>,
    mut attention_rx: mpsc::UnboundedReceiver<pty_state::AttentionEvent>,
) {
    tokio::spawn(async move {
        while let Some(evt) = attention_rx.recv().await {
            sessions.fan_out_attention(evt.session_id, evt.reason);
        }
    });
}

/// Binds the daemon's loopback listener on an OS-chosen port of 127.0.0.1.
/// The socket never sets `SO_REUSEADDR`: on Windows that keeps any other
/// socket from binding the same address and port, which the client's
/// connect-then-prove relies on.
pub(crate) async fn bind_loopback() -> anyhow::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .context("binding loopback listener")
}

pub async fn run(
    state: Arc<AppState>,
    dirs: Dirs,
    orphans: Vec<OrphanMeta>,
    abandoned: Vec<OrphanMeta>,
    resume_ids: Vec<String>,
) -> anyhow::Result<()> {
    let run_start = std::time::Instant::now();
    // A fresh loopback token every start. Remote access is off unless
    // `remote.json` turns it on; idle exit reads that switch, so it is seeded
    // from the persisted value.
    remove_retired_lan_files(&dirs.config);
    let remote = crate::remote_config::load(&dirs);
    let auth_token = crate::secret::generate_auth_token();

    let (attention_tx, attention_rx) = mpsc::unbounded_channel::<pty_state::AttentionEvent>();
    let sessions = SessionRegistry::new(dirs.clone());

    // Reattach orphans BEFORE binding the WS server. Doing the reattach
    // off-thread caused a regression where bytes from the tracer's ring
    // snapshot flowed through `attach_lifecycle → fan_out_pty` before the
    // client mounted its Terminal and sent `Attach`, so the per-client
    // `attached` filter dropped them and reattached panes appeared frozen.
    // Keeping reattach on the critical path means clients only see
    // sessions once they're fully wired up, at the cost of delaying the
    // handshake by `reattach_orphans`'s elapsed time. The supervisor's
    // SPAWN_WAIT_TIMEOUT is sized to absorb a worst-case PIPE_CONNECT_TIMEOUT.
    let (client_count, client_count_rx, live_lines, outlets) = chat_hub(&sessions, &attention_tx);
    reattach_orphans(&sessions, &dirs, &outlets, orphans, abandoned).await;

    // Forward attention events through the registry's broadcast so all
    // attached clients see them via the same SessionEvent channel they
    // already subscribe to.
    spawn_attention_forwarder(Arc::clone(&sessions), attention_rx);

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let (tab_events, _) = broadcast::channel(TAB_EVENT_CAPACITY);
    let (preset_events, _) = broadcast::channel(PRESET_EVENT_CAPACITY);
    let (state_events, _) = broadcast::channel(STATE_EVENT_CAPACITY);

    // Per-repo filesystem watchers that keep the source-control sidebar
    // live without user-triggered refreshes. The supervisor task owns the
    // per-repo debouncer handles and listens on `state_events` for repo
    // add/remove broadcasts, so adding a repo at runtime spawns a watcher
    // and removing one stops it. Refreshers park while `client_count` is
    // 0 — see `crates/daemon/src/git_watch.rs`.
    crate::git_watch::start(&state, &sessions, &state_events, &client_count_rx);

    // One watcher per linked `.code-workspace` file a workspace asked to
    // watch: a change re-parses the file and re-syncs the workspace's
    // members. The supervisor owns the handles and listens on `state_events`
    // for workspace changes, so a new link is armed and a removed or
    // disabled one stops — see `crates/daemon/src/vscode_watch.rs`.
    crate::vscode_watch::start(&state, &state_events);

    // Keep the host awake for as long as a session has a live child, so an
    // aggressive power plan can't sleep the machine out from under a running
    // task. Same liveness predicate as the idle-exit watcher below.
    let (keep_awake_tx, keep_awake_status_rx) = start_keep_awake(&state, &sessions, &state_events);

    let dashboard = crate::dashboard::Dashboard::new(Arc::clone(&sessions));
    let summarizer = start_summarizer(&state, &dirs, &client_count, &state_events);
    let turns =
        crate::summarizer::turns::Turns::new(Arc::clone(&sessions), Arc::clone(&summarizer));
    let hub = Hub {
        state,
        sessions,
        proof_key: tracer_protocol::daemon_proof_key(&auth_token),
        auth_token: auth_token.clone(),
        tailnet_handshake_timeout: TAILNET_HANDSHAKE_TIMEOUT,
        attention_tx,
        dirs: dirs.clone(),
        shutdown_tx,
        tab_events,
        preset_events,
        preset_cancellations: Arc::new(AsyncMutex::new(HashMap::new())),
        preset_job_snapshots: Arc::default(),
        state_events,
        client_count,
        remote_enabled: Arc::new(tokio::sync::watch::Sender::new(remote.enabled)),
        tailnet: Arc::new(AsyncMutex::new(None)),
        remote_status: Arc::new(tokio::sync::watch::Sender::new(RemoteSlot::initial(remote))),
        keep_awake_mode: Arc::new(keep_awake_tx),
        keep_awake_status: keep_awake_status_rx,
        recovering: RecoveryClaims::default(),
        resume_in_flight: Arc::default(),
        start_resume: Arc::default(),
        resume_reported: Arc::default(),
        pending_hooks: crate::hook_status::PendingHooks::default(),
        live_lines,
        chat_mirror: outlets.chat,
        devices: Arc::new(std::sync::Mutex::new(crate::devices::DeviceStore::load(
            &crate::paths::devices_file(&dirs),
        ))),
        pairing: Arc::new(std::sync::Mutex::new(crate::devices::PairingBook::new())),
        pairing_waiters: pairing::PairingWaiters::default(),
        live_devices: live_devices::LiveDevices::default(),
        dashboard,
        summarizer,
        turns,
    };
    start_hub_tasks(&hub, client_count_rx);

    let app = build_loopback_router(hub.clone());

    let bind_start = std::time::Instant::now();
    let listener = bind_loopback().await?;
    let bound = listener.local_addr().context("local addr")?;
    info!(
        port = bound.port(),
        bind_elapsed_ms = u64::try_from(bind_start.elapsed().as_millis()).unwrap_or(u64::MAX),
        run_elapsed_ms = u64::try_from(run_start.elapsed().as_millis()).unwrap_or(u64::MAX),
        "rustling-tulipd listening"
    );

    write_handshake(&dirs, bound.port(), &auth_token)?;
    info!(
        run_elapsed_ms = u64::try_from(run_start.elapsed().as_millis()).unwrap_or(u64::MAX),
        "handshake file written; daemon ready for clients"
    );

    // Resume the sessions the last run lost, now that clients can connect:
    // spawning them before the bind would hold up the handshake.
    if !resume_ids.is_empty() {
        let in_flight = recovery::ResumeInFlight::start(&hub.resume_in_flight);
        tokio::spawn(recovery::auto_resume(hub.clone(), resume_ids, in_flight));
    }

    // Opt-in tailnet listener: started after the handshake, never awaited.
    start_remote_access(&hub, remote, shutdown_rx.clone()).await;

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

/// The loopback listener's router: `/ws`, `/health`, `/shutdown`, the `/proof`
/// the hook shim checks first and the hook endpoint it posts to on
/// `127.0.0.1`.
fn build_loopback_router(hub: Hub) -> Router {
    Router::new()
        .route("/ws", get(ws_handler))
        .route("/health", get(|| async { "ok" }))
        .route("/shutdown", post(shutdown_handler))
        .route("/proof", get(proof_handler))
        .route("/hook/{session_id}", post(hook_handler))
        .with_state(hub)
}

fn write_handshake(dirs: &Dirs, port: u16, auth_token: &str) -> anyhow::Result<()> {
    let pid = std::process::id();
    let payload = DaemonHandshake {
        protocol_version: PROTOCOL_VERSION,
        port,
        auth_token: auth_token.to_string(),
        pid,
        supported_versions: SUPPORTED_PROTOCOL_VERSIONS.to_vec(),
        build_version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    let bytes = serde_json::to_vec_pretty(&payload).context("serializing handshake")?;
    // Each write gets its own temp name, so two daemons that briefly co-exist
    // (during wdio test sessions) never write into each other's mid-write file
    // and lose a rename. The file carries the auth token, so the helper creates
    // it owner-only and the replace lands an already-restricted file.
    crate::atomic_file::write_atomic(&dirs.handshake_file, &bytes).context("writing handshake")
}

async fn ws_handler(State(hub): State<Hub>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| client_session(hub, socket, AuthKind::Local))
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
    let Some(token) = presented_bearer(&headers) else {
        return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response();
    };
    if !crate::secret::constant_time_eq(token.as_bytes(), hub.auth_token.as_bytes()) {
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

/// The answer to a `ListPresets`: the target's presets, or the `Error` that
/// says why they cannot be read, each carrying the request's id.
async fn list_presets(
    hub: &Hub,
    target: protocol::PresetTarget,
    request_id: Option<String>,
) -> DaemonMessage {
    match crate::presets::list(hub, &target).await {
        Ok(entries) => DaemonMessage::Presets {
            target,
            entries,
            request_id,
        },
        Err(err) => {
            warn!(?err, ?target, "listing presets failed");
            DaemonMessage::Error {
                message: format!("{err:#}"),
                request_id,
                code: Some(protocol::error_codes::PRESETS_FAILED.to_owned()),
            }
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "dispatch is a flat match over the protocol message set; \
              splitting by category would add indirection without clarity"
)]
async fn dispatch(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) -> anyhow::Result<()> {
    let ConnCtx {
        out_tx,
        auth,
        client_id,
        ..
    } = *ctx;
    if let Some(reason) = host_only_refusal(&msg, auth) {
        warn!(%reason, "dispatch: host-only message refused on a tailnet connection");
        let _ = out_tx.send(DaemonMessage::Error {
            message: reason.to_owned(),
            request_id: None,
            code: Some(protocol::error_codes::LOCAL_ONLY.to_owned()),
        });
        return Ok(());
    }
    match msg {
        m @ (ClientMessage::DevicePairRequest
        | ClientMessage::DevicePairCancel
        | ClientMessage::DevicePairRedeem { .. }
        | ClientMessage::ListPendingDevices
        | ClientMessage::ApproveDevice { .. }
        | ClientMessage::DenyDevice { .. }
        | ClientMessage::MintDevicePairCode
        | ClientMessage::ListDevices
        | ClientMessage::RevokeDevice { .. }
        | ClientMessage::ForgetDevice { .. }) => pairing::handle(hub, m, ctx),
        ClientMessage::Hello { .. } | ClientMessage::Attach { session_id: _ } => {
            // Hello: already consumed by handshake; ignore subsequent ones.
            // Attach: no-op — PTY forwarders are started by LoadScrollback
            // so the live subscription is atomic with the scrollback
            // snapshot read. Kept in the protocol for back-compat with
            // older clients that send it.
        }
        m @ (ClientMessage::LoadChat { .. }
        | ClientMessage::UnloadChat { .. }
        | ClientMessage::LoadSummary { .. }
        | ClientMessage::UnloadSummary { .. }) => {
            chat::handle(hub, m, ctx);
        }
        m @ (ClientMessage::ListRepos
        | ClientMessage::AddRepo { .. }
        | ClientMessage::RemoveRepo { .. }
        | ClientMessage::ListWorkspaces
        | ClientMessage::UpsertWorkspace { .. }
        | ClientMessage::RemoveWorkspace { .. }
        | ClientMessage::AcceptVscodeWorkspaceSuggestion { .. }
        | ClientMessage::ParseVscodeWorkspace { .. }
        | ClientMessage::ScanVscodeWorkspaces { .. }
        | ClientMessage::SetRepoWorktreeDefault { .. }
        | ClientMessage::SetWorkspaceWorktreeDefault { .. }
        | ClientMessage::SetWorktreesRoot { .. }
        | ClientMessage::ConfigureLan { .. }
        | ClientMessage::StartPairing
        | ClientMessage::CancelPairing
        | ClientMessage::SetKeepAwake { .. }
        | ClientMessage::SetKeepAwakeMode { .. }
        | ClientMessage::SetAgentCommand { .. }
        | ClientMessage::SetAgentSettings { .. }
        | ClientMessage::InspectWorktreesRoot
        | ClientMessage::DeleteWorktreeAt { .. }) => settings::handle(hub, m, ctx).await?,
        m @ (ClientMessage::PreviewWorkspaceSpawn { .. }
        | ClientMessage::PreviewSpawn { .. }
        | ClientMessage::SpawnSession { .. }) => spawn::handle(hub, m, ctx).await?,
        m @ (ClientMessage::FetchRepo { .. }
        | ClientMessage::ListBranches { .. }
        | ClientMessage::ListWorktrees { .. }
        | ClientMessage::SuggestBranchName { .. }
        | ClientMessage::ListCommits { .. }
        | ClientMessage::GetCommit { .. }
        | ClientMessage::GetFileDiff { .. }
        | ClientMessage::GetRemoteUrl { .. }
        | ClientMessage::RepoStatus { .. }
        | ClientMessage::StageFiles { .. }
        | ClientMessage::UnstageFiles { .. }
        | ClientMessage::CommitRepo { .. }
        | ClientMessage::DiscardChanges { .. }
        | ClientMessage::StashPush { .. }
        | ClientMessage::ListStashes { .. }
        | ClientMessage::StashPop { .. }
        | ClientMessage::StashApply { .. }
        | ClientMessage::StashDrop { .. }
        | ClientMessage::GetFileSnapshot { .. }) => git::handle(hub, m, ctx).await?,
        m @ (ClientMessage::ListSessions
        | ClientMessage::DuplicateSession { .. }
        | ClientMessage::GetSpawnConfig { .. }
        | ClientMessage::RenameSession { .. }
        | ClientMessage::SetSessionBusyExcluded { .. }
        | ClientMessage::SetRepoAppearance { .. }
        | ClientMessage::SetWorkspaceAppearance { .. }
        | ClientMessage::SetSessionAppearance { .. }
        | ClientMessage::Detach { .. }
        | ClientMessage::SendInput { .. }
        | ClientMessage::Resize { .. }
        | ClientMessage::StopSession { .. }
        | ClientMessage::ParkSession { .. }
        | ClientMessage::DiscardSession { .. }
        | ClientMessage::PreviewDiscard { .. }
        | ClientMessage::RetryWorktreeCleanup { .. }
        | ClientMessage::LoadScrollback { .. }) => sessions::handle(hub, m, ctx).await?,
        m @ (ClientMessage::OpenDiffTab { .. }
        | ClientMessage::OpenViewerTab { .. }
        | ClientMessage::OpenDashboardTab { .. }
        | ClientMessage::ListTabs
        | ClientMessage::CreateTab { .. }
        | ClientMessage::CloseTab { .. }
        | ClientMessage::RestoreTab { .. }
        | ClientMessage::RestoreTabSnapshot { .. }
        | ClientMessage::RenameTab { .. }
        | ClientMessage::ReorderTabs { .. }
        | ClientMessage::InitLayout { .. }
        | ClientMessage::ReorderContainers { .. }
        | ClientMessage::ReorderSessions { .. }
        | ClientMessage::SplitPane { .. }
        | ClientMessage::ClosePane { .. }
        | ClientMessage::SetPaneRatio { .. }
        | ClientMessage::ReplacePaneSession { .. }
        | ClientMessage::MovePane { .. }
        | ClientMessage::SwapPanes { .. }
        | ClientMessage::RearrangeTab { .. }
        | ClientMessage::MergeTabs { .. }
        | ClientMessage::ExtractToNewTab { .. }) => tabs::handle(hub, m, ctx)?,
        m @ (ClientMessage::ResumeAbandoned { .. }
        | ClientMessage::DiscardAbandoned { .. }
        | ClientMessage::ResumeAllAbandoned
        | ClientMessage::ListEnvNames { .. }
        | ClientMessage::ListSessionHistory { .. }
        | ClientMessage::RecoverSessions { .. }) => recovery::handle(hub, m, ctx).await?,
        ClientMessage::FetchFile {
            id,
            repo_id,
            worktree_path,
            path,
        } => {
            let target = repo_target_or_err(hub, &repo_id, worktree_path.as_deref())
                .and_then(|root| file_fetch::confine_path(&root, &path));
            start_fetch(ctx, id, target).await;
        }
        ClientMessage::CancelFetch { id } => cancel_fetch(ctx.fetches, &id).await,
        ClientMessage::Shutdown { drain } => {
            info!(drain, "dispatch: ClientMessage::Shutdown received");
            if drain {
                shutdown_all(hub).await;
                info!("dispatch: shutdown_all returned; flipping watch");
            } else {
                // Don't drain: leave the sidecars on disk. Tracer-backed
                // sessions keep running without the daemon and are reattached
                // by the next daemon start; any that can't be reattached are
                // listed as abandoned so the user can resume or dismiss them.
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
        ClientMessage::ConfigureRemote { enabled, port } => {
            configure_remote(hub, enabled, port, out_tx).await?;
        }
        ClientMessage::ListPresets { target, request_id } => {
            let _ = out_tx.send(list_presets(hub, target, request_id).await);
        }
        ClientMessage::LaunchPreset {
            job_id,
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
                job_id,
                target,
                preset_id,
                source,
                variable_values,
                use_worktree_override,
                max_panes_per_tab_override,
                client_id: client_id.to_string(),
            };
            tokio::spawn(async move {
                crate::presets::launch(hub, args).await;
            });
        }
        ClientMessage::CancelPresetLaunch { job_id } => {
            let sender = hub.preset_cancellations.lock().await.get(&job_id).cloned();
            match sender {
                Some(cancel_tx) => {
                    let _ = cancel_tx.send(true);
                }
                None => {
                    let _ = out_tx.send(DaemonMessage::Error {
                        message: format!("unknown preset launch job: {job_id}"),
                        request_id: None,
                        code: Some(protocol::error_codes::NOT_FOUND.to_owned()),
                    });
                }
            }
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
                    Err(failure) => {
                        let _ = out_tx.send(DaemonMessage::PresetScriptsError {
                            id,
                            error: failure.error,
                            variable_name: failure.variable_name,
                        });
                    }
                }
            });
        }
        m @ (ClientMessage::WatchDashboard { .. } | ClientMessage::GetSessionDetail { .. }) => {
            dashboard::handle(hub, m, ctx).await;
        }
        m @ ClientMessage::DumpScreen { .. } => screen::handle(hub, m, ctx),
        m @ (ClientMessage::SetSummarizerSettings { .. }
        | ClientMessage::SetSummarizerKey { .. }
        | ClientMessage::TestSummary { .. }) => summarizer::handle(hub, m, ctx).await?,
        m @ ClientMessage::AnswerPrompt { .. } => refuse_not_supported_yet(m, out_tx),
        ClientMessage::ChatSend { request_id, .. }
        | ClientMessage::ChatAnswer { request_id, .. } => {
            // The chat is served read-only, so a send is answered with the
            // reason rather than silently dropped.
            let _ = out_tx.send(DaemonMessage::Error {
                message: "sending from the chat is not supported yet".to_string(),
                code: Some(protocol::error_codes::UNSUPPORTED_MESSAGE.to_owned()),
                request_id,
            });
        }
    }
    Ok(())
}

/// Refuse a Dashboard request this daemon does not handle yet: log the
/// variant's name (never its fields), and answer a request that carries an
/// id with an `Error` echoing it.
fn refuse_not_supported_yet(msg: ClientMessage, out_tx: &mpsc::UnboundedSender<DaemonMessage>) {
    let ClientMessage::AnswerPrompt { request_id, .. } = msg else {
        warn!("refuse_not_supported_yet: a supported message was routed here");
        return;
    };
    warn!(
        variant = "AnswerPrompt",
        "dispatch: message not supported yet"
    );
    if let Some(request_id) = request_id {
        let _ = out_tx.send(DaemonMessage::Error {
            message: "not supported yet".to_owned(),
            request_id: Some(request_id),
            code: Some(protocol::error_codes::UNSUPPORTED_MESSAGE.to_owned()),
        });
    }
}

/// The summarizer over the real providers. A failed start never stops the
/// daemon: it is logged, and the summarizer runs on templates alone.
fn start_summarizer(
    state: &Arc<AppState>,
    dirs: &Dirs,
    client_count: &Arc<tokio::sync::watch::Sender<usize>>,
    state_events: &broadcast::Sender<StateEvent>,
) -> Arc<crate::summarizer::service::Summarizer> {
    use crate::summarizer::service::{Caller, LiveCaller, Summarizer, SummarizerParts};
    let rejections = Arc::new(crate::summarizer::keys::Rejections::default());
    let caller: Option<Arc<dyn Caller>> = match LiveCaller::start(Arc::clone(&rejections)) {
        Ok(caller) => Some(Arc::new(caller)),
        Err(err) => {
            error!(
                error = %format!("{err:#}"),
                "summarizer: could not start; every summary is a template until the daemon restarts"
            );
            None
        }
    };
    Arc::new(Summarizer::new(SummarizerParts {
        state: Arc::clone(state),
        spend_file: crate::paths::summarizer_spend_file(dirs),
        caller,
        rejections,
        client_count: client_count.subscribe(),
        events: state_events.clone(),
    }))
}

/// The client count, the live-line channels and the chat mirror over them,
/// made before the reattach so each reattached watcher gets the mirror's tap.
fn chat_hub(
    sessions: &Arc<SessionRegistry>,
    attention_tx: &mpsc::UnboundedSender<pty_state::AttentionEvent>,
) -> (
    Arc<tokio::sync::watch::Sender<usize>>,
    tokio::sync::watch::Receiver<usize>,
    crate::hook_status::LiveLines,
    recovery::Outlets,
) {
    let (client_count_tx, client_count_rx) = tokio::sync::watch::channel(0usize);
    let client_count = Arc::new(client_count_tx);
    let live_lines = crate::hook_status::LiveLines::default();
    let chat = crate::chat_mirror::ChatMirror::new(
        Arc::clone(sessions),
        live_lines.clone(),
        Arc::clone(&client_count),
    );
    let outlets = recovery::Outlets {
        attention_tx: attention_tx.clone(),
        chat,
    };
    (client_count, client_count_rx, live_lines, outlets)
}

/// Start the tasks that watch the hub for the daemon's lifetime.
fn start_hub_tasks(hub: &Hub, client_count_rx: tokio::sync::watch::Receiver<usize>) {
    start_history_broadcaster(hub);
    // The key store blocks, so the summarizer's key states are first read
    // here rather than at construction.
    let summarizer = Arc::clone(&hub.summarizer);
    tokio::spawn(async move { summarizer.startup_key_check().await });
    // The Dashboard's 2 s facts tick; it parks while no connection watches.
    hub.dashboard.spawn_ticker();
    // The 3-minute working refresh; it parks while no connection watches.
    crate::summarizer::refresh::spawn(
        Arc::clone(&hub.sessions),
        hub.dashboard.clone(),
        Arc::clone(&hub.summarizer),
        Arc::clone(&hub.turns),
    );
    // Idle self-exit: once the last client disconnects and no session has a
    // live child for a full grace period, flip the shutdown watch so a
    // resident daemon doesn't pin a stale executable across app updates.
    // Mirrors the HTTP /shutdown path (no drain): sidecars stay on disk, and
    // abandoned sessions come back from them on the next daemon start.
    crate::idle_exit::spawn(
        Arc::clone(&hub.sessions),
        crate::idle_exit::Watches::new(client_count_rx, hub.remote_enabled.subscribe()),
        hub.shutdown_tx.clone(),
        crate::idle_exit::HoldOffs {
            preset_jobs: Arc::clone(&hub.preset_cancellations),
            resume_in_flight: Arc::clone(&hub.resume_in_flight),
        },
        crate::idle_exit::GRACE_PERIOD,
    );
}

/// Test-only record of the last spawn spec's environment built on this
/// thread: the `env` and `env_remove` every `PtySpawnSpec` / `HeadlessSpec`
/// is handed, so a test can see what a child would get without starting one.
#[cfg(test)]
mod child_env_probe {
    use std::cell::RefCell;

    type Recorded = (Vec<(String, String)>, Vec<String>);

    thread_local! {
        static LAST: RefCell<Option<Recorded>> = const { RefCell::new(None) };
    }

    pub fn record(env: &[(String, String)], remove: &[String]) {
        LAST.with(|last| *last.borrow_mut() = Some((env.to_vec(), remove.to_vec())));
    }

    /// The last spec's `env`, clearing the record.
    pub fn take() -> Option<Vec<(String, String)>> {
        take_both().map(|(env, _)| env)
    }

    /// The last spec's `env` and `env_remove`, clearing the record.
    pub fn take_both() -> Option<Recorded> {
        LAST.with(|last| last.borrow_mut().take())
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};

    use protocol::{ClientMessage, DaemonMessage, PresetTarget, TabContent, TabEntry};

    use super::bind_loopback;
    use super::test_support::{
        dispatch_one, repo_entry, seed_dashboard_tab, seed_two_pane_layout, test_hub,
    };

    /// A socket that sets `SO_REUSEADDR` still cannot take the daemon's
    /// loopback port: on Windows the daemon's socket keeps the address
    /// exclusive, which the client's connect-then-prove relies on.
    #[tokio::test]
    async fn loopback_listener_refuses_a_second_reuseaddr_bind_on_its_port() {
        let listener = bind_loopback()
            .await
            .expect("bind the daemon's loopback listener");
        let port = listener.local_addr().expect("the bound address").port();
        let socket = tokio::net::TcpSocket::new_v4().expect("create a socket");
        socket.set_reuseaddr(true).expect("set SO_REUSEADDR");
        let err = socket
            .bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
            .expect_err("a second bind with SO_REUSEADDR must be refused");
        #[cfg(windows)]
        assert_eq!(
            err.raw_os_error(),
            Some(10013),
            "the refusal must be WSAEACCES: {err}"
        );
        #[cfg(not(windows))]
        let _ = err.raw_os_error();
    }

    /// A Dashboard request whose handler has not landed is refused on the
    /// wire with its id, so the requester does not wait on a reply.
    #[tokio::test]
    async fn a_dashboard_request_is_refused_as_not_supported_yet() {
        let (hub, _scratch) = test_hub("answer-prompt-stub");
        let reply = dispatch_one(
            &hub,
            ClientMessage::AnswerPrompt {
                session_id: "s1".to_string(),
                since: chrono::Utc::now(),
                choice: protocol::PromptChoice::Allow,
                request_id: Some("d1".to_string()),
            },
        )
        .await;
        assert!(
            matches!(
                &reply,
                DaemonMessage::Error { message, request_id: Some(id), .. }
                    if message == "not supported yet" && id == "d1"
            ),
            "{reply:?}"
        );
    }

    /// A preset list echoes its request's id, so the requester can tell its
    /// own list from another request's.
    #[tokio::test]
    async fn a_preset_list_echoes_its_request_id() {
        let (hub, scratch) = test_hub("presets-echo");
        hub.state
            .mutate(|s| s.repos.push(repo_entry("r1", "r1", scratch.path())))
            .expect("register the repo");
        let target = PresetTarget::Repo {
            repo_id: "r1".to_owned(),
        };
        let reply = dispatch_one(
            &hub,
            ClientMessage::ListPresets {
                target: target.clone(),
                request_id: Some("p1".to_owned()),
            },
        )
        .await;
        assert!(
            matches!(
                &reply,
                DaemonMessage::Presets { target: listed, entries, request_id: Some(id) }
                    if *listed == target && entries.is_empty() && id == "p1"
            ),
            "{reply:?}"
        );
    }

    /// A preset list that cannot be read is answered with an `Error` that
    /// carries the request's id and the presets code, so the requester's
    /// menu shows the failure and no other request takes it.
    #[tokio::test]
    async fn a_failed_preset_list_is_refused_with_its_request_id() {
        let (hub, scratch) = test_hub("presets-failed");
        let folder = scratch.path().join(".rustling-tulip");
        std::fs::create_dir_all(&folder).expect("create the presets folder");
        std::fs::write(folder.join("presets.json"), "not json").expect("write presets.json");
        hub.state
            .mutate(|s| s.repos.push(repo_entry("r1", "r1", scratch.path())))
            .expect("register the repo");
        let reply = dispatch_one(
            &hub,
            ClientMessage::ListPresets {
                target: PresetTarget::Repo {
                    repo_id: "r1".to_owned(),
                },
                request_id: Some("p1".to_owned()),
            },
        )
        .await;
        assert!(
            matches!(
                &reply,
                DaemonMessage::Error { message, request_id: Some(id), code: Some(code) }
                    if id == "p1"
                        && code == protocol::error_codes::PRESETS_FAILED
                        && message.contains("presets.json")
            ),
            "{reply:?}"
        );
    }

    /// A client's Dashboard tab is listed back to it, and a cloned layout
    /// keeps it under a fresh id.
    #[tokio::test]
    async fn a_dashboard_tab_is_listed_and_cloned() {
        let (hub, _scratch) = test_hub("dashboard-tab");
        seed_dashboard_tab(&hub, "c1");
        let reply = dispatch_one(&hub, ClientMessage::ListTabs).await;
        assert!(
            matches!(
                &reply,
                DaemonMessage::Tabs { tabs }
                    if tabs.len() == 1
                        && tabs.first().map(|tab| &tab.content) == Some(&TabContent::Dashboard)
            ),
            "{reply:?}"
        );
        let tabs = hub.state.client_layout("c1");
        let cloned = crate::tabs::clone_tabs_fresh_ids(&tabs);
        assert_eq!(cloned.len(), 1);
        assert!(
            cloned
                .iter()
                .zip(&tabs)
                .all(|(clone, tab)| clone.content == TabContent::Dashboard && clone.id != tab.id),
            "{cloned:?}"
        );
    }

    /// A tab this build cannot read is never restored over a real one: undo
    /// would otherwise store `{"kind":"unknown"}` for every client.
    #[tokio::test]
    async fn restoring_an_unknown_tab_is_refused() {
        let (hub, _scratch) = test_hub("restore-unknown");
        seed_two_pane_layout(&hub, "c1");
        let before = hub.state.client_layout("c1");
        let unknown = TabEntry {
            content: TabContent::Unknown,
            ..before.first().expect("the seeded tab").clone()
        };
        for msg in [
            ClientMessage::RestoreTab {
                tab: unknown.clone(),
                index: 0,
            },
            ClientMessage::RestoreTabSnapshot { tab: unknown },
        ] {
            let reply = dispatch_one(&hub, msg).await;
            assert!(
                matches!(
                    &reply,
                    DaemonMessage::Error { message, request_id: None, .. }
                        if message == "can't restore a tab this app does not understand"
                ),
                "{reply:?}"
            );
            let listed = dispatch_one(&hub, ClientMessage::ListTabs).await;
            assert!(
                matches!(&listed, DaemonMessage::Tabs { tabs } if *tabs == before),
                "{listed:?}"
            );
        }
    }
}
