//! WebSocket server, message dispatch, and session orchestration glue.

#[cfg(test)]
mod test_support;

mod remote;
pub(crate) use self::remote::RemoteSlot;

mod connection;
mod handshake;
pub(crate) use self::handshake::TAILNET_HANDSHAKE_TIMEOUT;

mod git;
mod tabs;

mod hooks;
mod recovery;
mod settings;
pub(crate) use self::recovery::RecoveryClaims;

use self::connection::{
    AuthKind, ConnCtx, PtyForwarders, cancel_fetch, client_session, start_fetch,
};
use self::git::{
    active_sessions_using_worktree, blocked_discard_message, probe_locking_processes,
    repo_target_or_err, retry_worktree_cleanup, worktree_in_use_failure,
};
use self::hooks::{hook_handler, presented_bearer};
use self::recovery::{reattach_orphans, record_end, start_history_broadcaster};
use self::remote::{
    configure_remote, host_only_refusal, remove_retired_lan_files, start_remote_access,
};
use self::settings::start_keep_awake;
use self::tabs::close_session_panes;
use crate::agents::CommonSpawnFields;
use crate::branch_fate::{FateInput, SessionBase};
use crate::orphan::OrphanMeta;
use crate::paths::Dirs;
use crate::pty::PtySpawnSpec;
use crate::registry::{
    persist_last_agent, persist_repo_last_spawn_config, persist_workspace_last_spawn_config,
    set_repo_appearance, set_workspace_appearance,
};
use crate::session::{
    ScrollbackSnapshot, SessionEvent, SessionRecord, SessionRegistry, UpdateOrigin,
    attach_lifecycle, build_replay_snapshot, new_id, push_recent_action,
};
use crate::state::AppState;
use crate::tailnet_listener::TailnetListener;
use crate::{
    branch_fate, branch_names, env_secrets, file_fetch, headless, history, inject, orphan,
    osc_title, pty_state, spawn_plan, tracer_client, user_env, workspace as ws,
};
use anyhow::{Context as _, anyhow};
use axum::Router;
use axum::extract::State;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use chrono::Utc;
use directories::UserDirs;
use protocol::{
    Agent, AgentOptions, AppearanceOverrides, AttentionReason, BranchCleanup, ClientMessage,
    DaemonHandshake, DaemonMessage, MemberBranchFate, PROTOCOL_VERSION, PresetLaunchJobSnapshot,
    SUPPORTED_PROTOCOL_VERSIONS, SessionEnd, SessionKind, SessionMember, SessionMetrics,
    SessionMode, SessionStatus, SpawnRequest, SpawnTarget, TabEntry,
};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc};
use tracing::{debug, info, warn};

#[derive(Clone)]
pub struct Hub {
    pub state: Arc<AppState>,
    pub sessions: Arc<SessionRegistry>,
    pub auth_token: String,
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
    /// The persisted keep-awake setting, published to the watcher.
    /// [`ClientMessage::SetKeepAwake`] writes it here after saving to
    /// `state.json`; the watcher re-derives the OS hold and broadcasts the
    /// resulting status itself.
    pub keep_awake_enabled: Arc<tokio::sync::watch::Sender<bool>>,
    /// Latest keep-awake status from the watcher, read by the initial-state
    /// push so a freshly-connected client renders the toggle correctly
    /// without waiting for the next broadcast.
    pub keep_awake_status: tokio::sync::watch::Receiver<crate::keep_awake::Status>,
    /// The session ids a recovery or a Resume is spawning for right now, so
    /// the two never start the same session twice.
    pub recovering: RecoveryClaims,
    /// Hook events that arrived for a session id before its spawn inserted
    /// it; applied to the record at insertion (bounded by age and count).
    pub pending_hooks: crate::hook_status::PendingHooks,
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
        enabled: bool,
        active: bool,
    },
    /// Broadcast on every remote-access status change: a
    /// [`ClientMessage::ConfigureRemote`] or the tailnet listener's own state
    /// moving. Mirrors [`protocol::DaemonMessage::RemoteStatus`]; it carries
    /// no token, so every client may see it.
    RemoteStatus {
        enabled: bool,
        port: u16,
        state: protocol::RemoteState,
    },
    /// The whole session history, broadcast after it changes.
    SessionHistory(Vec<protocol::SessionHistoryItem>),
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

pub async fn run(
    state: Arc<AppState>,
    dirs: Dirs,
    orphans: Vec<OrphanMeta>,
    abandoned: Vec<OrphanMeta>,
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
    reattach_orphans(&sessions, &dirs, &attention_tx, orphans, abandoned).await;

    // Forward attention events through the registry's broadcast so all
    // attached clients see them via the same SessionEvent channel they
    // already subscribe to.
    spawn_attention_forwarder(Arc::clone(&sessions), attention_rx);

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

    let hub = Hub {
        state,
        sessions,
        auth_token: auth_token.clone(),
        tailnet_handshake_timeout: TAILNET_HANDSHAKE_TIMEOUT,
        attention_tx,
        dirs: dirs.clone(),
        shutdown_tx,
        tab_events,
        preset_events,
        preset_cancellations: Arc::new(AsyncMutex::new(HashMap::new())),
        state_events,
        client_count,
        remote_enabled: Arc::new(tokio::sync::watch::Sender::new(remote.enabled)),
        tailnet: Arc::new(AsyncMutex::new(None)),
        remote_status: Arc::new(tokio::sync::watch::Sender::new(RemoteSlot::initial(remote))),
        keep_awake_enabled: Arc::new(keep_awake_tx),
        keep_awake_status: keep_awake_status_rx,
        recovering: RecoveryClaims::default(),
        pending_hooks: crate::hook_status::PendingHooks::default(),
    };
    start_hub_tasks(&hub, client_count_rx);

    let app = build_loopback_router(hub.clone());

    let bind_start = std::time::Instant::now();
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("binding loopback listener")?;
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

/// The loopback listener's router: `/ws`, `/health`, `/shutdown` and the hook
/// endpoint the shim posts to on `127.0.0.1`.
fn build_loopback_router(hub: Hub) -> Router {
    Router::new()
        .route("/ws", get(ws_handler))
        .route("/health", get(|| async { "ok" }))
        .route("/shutdown", post(shutdown_handler))
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
    // Carries the auth token — owner-only, and written to the tmp path so the
    // rename lands an already-restricted file.
    crate::secret::write_private(&tmp, &bytes).context("writing handshake tmp")?;
    std::fs::rename(&tmp, &dirs.handshake_file).context("renaming handshake")?;
    Ok(())
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

#[expect(
    clippy::too_many_lines,
    reason = "dispatch is a flat match over the protocol message set; \
              splitting by category would add indirection without clarity"
)]
async fn dispatch(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) -> anyhow::Result<()> {
    let ConnCtx {
        out_tx,
        auth,
        pty_forwarders,
        client_id,
        connection,
        ..
    } = *ctx;
    if let Some(reason) = host_only_refusal(&msg, auth) {
        warn!(%reason, "dispatch: host-only message refused on a tailnet connection");
        let _ = out_tx.send(DaemonMessage::Error {
            message: reason.to_owned(),
            request_id: None,
        });
        return Ok(());
    }
    match msg {
        ClientMessage::Hello { .. } | ClientMessage::Attach { session_id: _ } => {
            // Hello: already consumed by handshake; ignore subsequent ones.
            // Attach: no-op — PTY forwarders are started by LoadScrollback
            // so the live subscription is atomic with the scrollback
            // snapshot read. Kept in the protocol for back-compat with
            // older clients that send it.
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
        | ClientMessage::InspectWorktreesRoot
        | ClientMessage::DeleteWorktreeAt { .. }) => settings::handle(hub, m, ctx).await?,
        ClientMessage::PreviewWorkspaceSpawn {
            workspace_id,
            branch_name,
            base_branch,
            request_id,
        } => {
            // Preview always reflects the worktree path; the in-place
            // affordance is hidden behind the use_worktree spawn flag and
            // doesn't change preview output meaningfully.
            let resolved = ws::resolve_workspace(
                &hub.state,
                &hub.state.worktrees_dir(),
                &workspace_id,
                ws::ResolveRequest {
                    branch_name: &branch_name,
                    explicit_base: base_branch.as_deref(),
                    use_worktree: true,
                    // Preview answers "where would a fresh spawn land"; a pin
                    // has no fork point to preview.
                    pins: &[],
                },
            )
            .await;
            match resolved {
                Ok((_, resolved)) => {
                    let per_member = ws::previews(&resolved, &branch_name).await;
                    let _ = out_tx.send(DaemonMessage::WorkspaceSpawnPreview {
                        workspace_id,
                        branch_name,
                        per_member,
                        request_id,
                    });
                }
                Err(err) => send_preview_failure(out_tx, &err, request_id),
            }
        }
        ClientMessage::PreviewSpawn {
            repo_id,
            branch_name,
            base_branch,
            use_worktree,
            request_id,
        } => {
            let preview = preview_single_spawn(
                hub,
                &repo_id,
                &branch_name,
                base_branch.as_deref(),
                use_worktree,
            )
            .await;
            match preview {
                Ok(preview) => {
                    let _ = out_tx.send(DaemonMessage::SpawnPreview {
                        repo_id,
                        branch_name,
                        preview,
                        request_id,
                    });
                }
                Err(err) => send_preview_failure(out_tx, &err, request_id),
            }
        }
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
        ClientMessage::ListSessions => {
            let _ = out_tx.send(DaemonMessage::Sessions {
                sessions: hub.sessions.snapshots(),
            });
        }
        ClientMessage::SpawnSession(req) => {
            // A request `check_spawn_request` refuses is refused before the
            // user is asked to confirm a checkout it would never make.
            if let Err(failure) = check_spawn_request(&req) {
                send_spawn_failure(out_tx, &failure.into(), req.request_id.as_deref());
            } else if let Some(confirm) = in_place_checkout_confirm(hub, &req).await {
                let _ = out_tx.send(confirm);
            } else {
                // The requester hears of its spawn through the registry's one
                // broadcast, which carries `request_id` on this connection.
                let request_id = req.request_id.clone();
                let origin = request_origin(connection, request_id.clone());
                if let Err(err) = spawn_session(hub, req, origin).await {
                    send_spawn_failure(out_tx, &err, request_id.as_deref());
                }
            }
        }
        ClientMessage::DuplicateSession {
            session_id,
            request_id,
            carry_busy_excluded,
        } => {
            let ask = DuplicateAsk {
                session_id: &session_id,
                request_id,
                carry_busy_excluded,
            };
            duplicate_session(hub, ask, connection, out_tx).await;
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
            // The registry update mirrors the new labels into the sidecar.
            hub.sessions.update(&session_id, |guard| {
                guard.user_label.clone_from(&normalized);
                guard.label = normalized
                    .clone()
                    .unwrap_or_else(|| guard.default_label.clone());
            });
        }
        ClientMessage::SetSessionBusyExcluded {
            session_id,
            excluded,
        } => {
            // The registry update mirrors the flag into the sidecar and
            // broadcasts the session; an unknown session is ignored, as a
            // rename of one is.
            hub.sessions.update(&session_id, |guard| {
                guard.busy_excluded = excluded;
            });
        }
        ClientMessage::SetRepoAppearance {
            repo_id,
            appearance,
        } => {
            let appearance = appearance.normalized()?;
            set_repo_appearance(&hub.state, &repo_id, appearance)?;
            let repos = hub.state.with_persisted(|s| s.repos.clone());
            let _ = hub.state_events.send(StateEvent::Repos(repos));
        }
        ClientMessage::SetWorkspaceAppearance {
            workspace_id,
            appearance,
        } => {
            let appearance = appearance.normalized()?;
            set_workspace_appearance(&hub.state, &workspace_id, appearance)?;
            let workspaces = hub.state.with_persisted(|s| s.workspaces.clone());
            let _ = hub.state_events.send(StateEvent::Workspaces(workspaces));
        }
        ClientMessage::SetSessionAppearance {
            session_id,
            appearance,
            request_id,
        } => {
            let change = AppearanceChangeRequest {
                session_id: &session_id,
                appearance: &appearance,
                request_id,
                connection,
            };
            set_session_appearance(&hub.sessions, change, out_tx);
        }
        ClientMessage::Detach { session_id } => {
            let mut forwarders = pty_forwarders.lock().await;
            if let Some(handle) = forwarders.remove(&session_id) {
                handle.abort();
            }
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
                // The owner's key answers a pending hook prompt: leave
                // `AwaitingInput` at once, with no settle window. The test and
                // the clear share one registry update, so a hook event cannot
                // slip between them; a session without hooks is untouched,
                // and the bytes are forwarded outside the record lock.
                hub.sessions.update_if_changed(&session_id, |rec| {
                    if !rec.hook_driven
                        || (rec.status != SessionStatus::AwaitingInput
                            && rec.pending_input.is_none())
                    {
                        return false;
                    }
                    if rec.status == SessionStatus::AwaitingInput {
                        rec.status = SessionStatus::Idle;
                    }
                    rec.pending_input = None;
                    true
                });
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
        m @ (ClientMessage::OpenDiffTab { .. }
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
        | ClientMessage::RearrangeTab { .. }
        | ClientMessage::MergeTabs { .. }
        | ClientMessage::ExtractToNewTab { .. }) => tabs::handle(hub, m, ctx)?,
        m @ (ClientMessage::ResumeAbandoned { .. }
        | ClientMessage::DiscardAbandoned { .. }
        | ClientMessage::ResumeAllAbandoned
        | ClientMessage::ListEnvNames { .. }
        | ClientMessage::ListSessionHistory { .. }
        | ClientMessage::RecoverSessions { .. }) => recovery::handle(hub, m, ctx).await?,
        ClientMessage::ParkSession { session_id } => {
            park_session(hub, &session_id).await;
        }
        ClientMessage::DiscardSession {
            session_id,
            cleanup,
        } => {
            discard_session(hub, &session_id, &cleanup, out_tx).await;
        }
        ClientMessage::PreviewDiscard { session_id } => {
            preview_discard(hub, &session_id, out_tx).await;
        }
        ClientMessage::RetryWorktreeCleanup {
            session_id,
            kill_pids,
            targets,
        } => {
            retry_worktree_cleanup(hub, &session_id, &kill_pids, &targets, out_tx).await;
        }
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
        ClientMessage::LoadScrollback {
            session_id,
            request_id,
        } => {
            load_scrollback_and_attach_forwarder(
                hub,
                &session_id,
                request_id,
                out_tx,
                pty_forwarders,
            )
            .await;
        }
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
        ClientMessage::ListPresets { target } => {
            let entries = crate::presets::list(hub, &target).await?;
            let _ = out_tx.send(DaemonMessage::Presets { target, entries });
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

/// Stop every active session (kill PTY / headless child, drop orphan sidecar)
/// and clear the registry. Worktrees are intentionally left in place — exit
/// is the wrong moment to ask the user about per-member cleanup.
async fn shutdown_all(hub: &Hub) {
    let ids: Vec<String> = hub.sessions.snapshots().into_iter().map(|s| s.id).collect();
    info!(count = ids.len(), "shutdown_all: starting");
    // Recorded before the stops so the shutdown, not their `StoppedByUser`,
    // is what the history keeps.
    for id in &ids {
        record_end(hub, id, SessionEnd::DaemonShutdown);
    }
    for id in &ids {
        info!(session_id = %id, "shutdown_all: stopping session");
        if let Err(err) = stop_session(hub, id).await {
            warn!(session_id = %id, ?err, "stop_session during shutdown failed");
        }
        info!(session_id = %id, "shutdown_all: session stopped");
    }
    info!("shutdown_all: complete");
}

/// Whether a spawn target runs in a daemon-managed worktree. A standalone
/// shell has no repo and never does.
fn target_uses_worktree(target: &protocol::SpawnTarget) -> bool {
    match target {
        protocol::SpawnTarget::Single { use_worktree, .. }
        | protocol::SpawnTarget::Workspace { use_worktree, .. } => *use_worktree,
        protocol::SpawnTarget::Standalone { .. } => false,
    }
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
        protocol::SpawnTarget::Standalone { .. } => false,
    };
    if use_worktree {
        members.iter().map(|m| m.worktree_path.clone()).collect()
    } else {
        Vec::new()
    }
}

/// Answers a failed spawn preview (`PreviewSpawn`, `PreviewWorkspaceSpawn`)
/// with an `Error` to this requester only, carrying the `request_id` it asked
/// with so the client can clear that pending preview.
fn send_preview_failure(
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    err: &anyhow::Error,
    request_id: Option<String>,
) {
    warn!(?err, "spawn preview failed");
    let _ = out_tx.send(DaemonMessage::Error {
        message: err.to_string(),
        request_id,
    });
}

/// If `req` is an in-place single-repo spawn that would switch a dirty working
/// tree to a *different* branch and no strategy was chosen yet, return the
/// `CheckoutConfirmRequired` message asking the client how to proceed. Returns
/// `None` when the spawn can run normally (worktree mode, same branch, clean
/// tree, non-git folder, or a strategy already chosen).
async fn in_place_checkout_confirm(hub: &Hub, req: &SpawnRequest) -> Option<DaemonMessage> {
    let SpawnTarget::Single {
        repo_id,
        branch_name,
        use_worktree: false,
        checkout_strategy: None,
        ..
    } = &req.target
    else {
        return None;
    };
    let repo_path = hub.state.with_persisted(|s| {
        s.repos
            .iter()
            .find(|r| r.id == *repo_id)
            .map(|r| r.path.clone())
    })?;
    let repo_path = PathBuf::from(repo_path);
    if !crate::git::is_initialized(&repo_path).await {
        return None;
    }
    match crate::git::in_place_checkout_preflight(&repo_path, branch_name).await {
        Ok(crate::git::InPlaceCheckout::Dirty { count }) => Some(checkout_confirm_reply(
            repo_id,
            branch_name,
            req.request_id.as_deref(),
            count,
        )),
        Ok(_) => None,
        Err(err) => {
            warn!(
                ?err,
                repo_id, "in_place_checkout_confirm: preflight failed; spawning anyway"
            );
            None
        }
    }
}

/// The prompt for an in-place spawn of `branch` in `repo_id` whose tree has
/// `dirty_count` uncommitted changes, echoing the spawn's `request_id`: it
/// goes to the requester only.
fn checkout_confirm_reply(
    repo_id: &str,
    branch: &str,
    request_id: Option<&str>,
    dirty_count: usize,
) -> DaemonMessage {
    DaemonMessage::CheckoutConfirmRequired {
        repo_id: repo_id.to_owned(),
        branch: branch.to_owned(),
        dirty_count: u32::try_from(dirty_count).unwrap_or(u32::MAX),
        request_id: request_id.map(str::to_owned),
    }
}

pub(crate) async fn spawn_session(
    hub: &Hub,
    req: SpawnRequest,
    origin: Option<UpdateOrigin>,
) -> anyhow::Result<protocol::SessionSnapshot> {
    spawn_session_with(hub, req, origin, user_env::resolve).await
}

/// [`spawn_session`] with the variable lookup `resolve` in place of
/// [`user_env::resolve`]: it resolves the rows' env references and the
/// provider's key.
#[expect(
    clippy::too_many_lines,
    reason = "Spawn is naturally a flat pipeline (resolve → config → mode switch → \
              timing). Extracting helpers would require threading the timing values \
              and adding a result wrapper for diminishing returns"
)]
async fn spawn_session_with<R>(
    hub: &Hub,
    mut req: SpawnRequest,
    origin: Option<UpdateOrigin>,
    resolve: R,
) -> anyhow::Result<protocol::SessionSnapshot>
where
    R: Fn(&str) -> Option<(user_env::Secret, user_env::Origin)>,
{
    // Phase-level timing for the spawn pipeline so we can tell at a glance
    // whether a slow spawn was the git worktree setup, the PTY/child boot,
    // or something on the registry side. Logged at info so users debugging
    // perceived slowness can read them without flipping log filters.
    let t_total = std::time::Instant::now();
    // A plain shell asking for a provider, a provider this daemon doesn't
    // know, or a row setting one of the provider's locked variables or a
    // variable it removes from the child refuses
    // before the rows are sealed and before any git work, so a refused spawn
    // writes nothing to the credential store and checks nothing out.
    let provider = request_provider(&req);
    crate::agents::providers::check_request(req.mode, provider, &req.extra_env)?;
    // A secret row's literal moves into the credential store and becomes a
    // `${secret:<id>}` reference before anything captures the request, so the
    // session record, the sidecar, `state.json`, the `Repos`/`Workspaces`
    // broadcast, the `GetSpawnConfig` reply and the history entry all hold the
    // reference and never the value.
    env_secrets::seal_rows(&mut req.extra_env, &req.secret_env_keys)?;
    // Capture the persist-able config before destructuring so duplicates
    // can be reconstructed verbatim from the session record / sidecar.
    let stored_config = protocol::SpawnConfig::from_request(&req);
    let SpawnRequest {
        label,
        target,
        mode,
        initial_prompt,
        dangerously_skip_permissions,
        agent_options,
        model,
        extra_env,
        secret_env_keys: _,
        prompt_injector,
        request_id: _,
        resume_conversation,
    } = req;
    let agent = agent_options.agent();
    info!(
        ?mode,
        agent = agent.as_label(),
        target_kind = match &target {
            SpawnTarget::Single { .. } => "single",
            SpawnTarget::Workspace { .. } => "workspace",
            SpawnTarget::Standalone { .. } => "standalone",
        },
        "spawn_session: begin"
    );
    if matches!(&target, SpawnTarget::Standalone { .. }) && !standalone_supports(mode, agent) {
        return Err(anyhow!(
            "standalone targets only support plain_shell and interactive sessions"
        ));
    }
    reject_pin_without_worktree(&target)?;
    let backend = crate::agents::backend_for(agent);
    if mode == SessionMode::Headless && !backend.supports_headless() {
        return Err(anyhow!(
            "headless mode is not yet supported for {}; use interactive mode",
            agent.as_label()
        ));
    }
    // Env references resolve before any git work, so a refusal leaves no
    // worktree behind. `stored_config` keeps the references as sent; only the
    // child's environment gets their values.
    let extra_env = resolve_env_refs(&extra_env, &resolve)?;
    // The provider's routing variables and key join the child's environment
    // here, after `stored_config` was captured, so they are never stored; a
    // missing key refuses before any git work.
    let env = crate::agents::providers::spawn_env(provider, &extra_env, &resolve)?;

    let t_resolve = std::time::Instant::now();
    let mut standalone_add_dirs = Vec::new();
    let (kind, members, primary_cwd, default_label, workspace_id) = match target {
        SpawnTarget::Single {
            repo_id,
            branch_name,
            base_branch,
            use_worktree,
            checkout_strategy,
            worktree_reuse,
            existing_worktree,
        } => {
            spawn_single(
                hub,
                &repo_id,
                SinglePlan {
                    branch_name: &branch_name,
                    base_branch,
                    use_worktree,
                    checkout_strategy,
                    worktree_reuse,
                    existing_worktree,
                },
            )
            .await?
        }
        SpawnTarget::Workspace {
            workspace_id,
            branch_name,
            base_branch,
            use_worktree,
            worktree_reuse,
            existing_worktrees,
        } => {
            spawn_workspace(
                hub,
                &workspace_id,
                ws::ResolveRequest {
                    branch_name: &branch_name,
                    explicit_base: base_branch.as_deref(),
                    use_worktree,
                    pins: &existing_worktrees,
                },
                worktree_reuse,
            )
            .await?
        }
        SpawnTarget::Standalone { cwd, add_dirs } => {
            if mode == SessionMode::Interactive {
                standalone_add_dirs = add_dirs;
                spawn_standalone_agent(cwd.as_deref())?
            } else {
                spawn_standalone_shell(cwd.as_deref())?
            }
        }
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
        agent_options,
        model,
        env,
        prompt_injector,
        origin,
        resume_conversation,
        add_dirs: standalone_add_dirs,
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
        SpawnTarget::Standalone { .. } => {}
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
    if let Err(err) = &result {
        warn!(
            error = %format!("{err:#}"),
            "spawn_session: child boot failed"
        );
    }
    result
}

fn expected_output_subscribers(mode: SessionMode) -> usize {
    match mode {
        SessionMode::Interactive | SessionMode::PlainShell => 3,
        SessionMode::Headless => 0,
    }
}

/// How long the `LoadScrollback` handler waits for `attach_lifecycle`'s task
/// to reply with the atomic snapshot before falling back to a direct
/// scrollback file read. In normal operation the reply arrives within a
/// single task tick; the timeout protects against a hung lifecycle task
/// (which would otherwise block this client forever).
const SCROLLBACK_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);

/// Atomic `LoadScrollback`: pull the scrollback bytes AND the
/// `broadcast::Receiver` positioned at the next un-persisted PTY chunk from
/// `attach_lifecycle`'s task, spawn a per-client forwarder using that receiver
/// (so future PTY bytes for this session reach this WS client), and finally
/// reply with the Scrollback frame. Doing all three in one handler closes the
/// window in which output emitted between `Attach` and the file read would be
/// dropped or duplicated.
///
/// Sessions without an active PTY (orphans, abandoned, headless) have
/// `scrollback_snapshot_req == None`; the helper falls back to a direct file
/// read in that case and does NOT spawn a forwarder.
async fn load_scrollback_and_attach_forwarder(
    hub: &Hub,
    session_id: &str,
    request_id: Option<String>,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    pty_forwarders: &PtyForwarders,
) {
    let snap_tx = hub
        .sessions
        .get(session_id)
        .and_then(|arc| crate::sync::lock(&arc).scrollback_snapshot_req.clone());

    let snapshot = if let Some(snap_tx) = snap_tx {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if snap_tx.send(reply_tx).is_ok() {
            match tokio::time::timeout(SCROLLBACK_SNAPSHOT_TIMEOUT, reply_rx).await {
                Ok(Ok(snap)) => Some(snap),
                Ok(Err(_recv_err)) => {
                    warn!(
                        %session_id,
                        "load_scrollback: lifecycle task dropped snapshot reply"
                    );
                    None
                }
                Err(_elapsed) => {
                    warn!(
                        %session_id,
                        "load_scrollback: timed out waiting for snapshot from lifecycle task"
                    );
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    if let Some(snap) = snapshot {
        reply_then_forward(snap, session_id, request_id, out_tx, pty_forwarders).await;
    } else {
        // No live PTY (orphan/abandoned/headless), or the lifecycle task failed
        // to answer in time. Read the file directly, but still re-assert the
        // child's bracketed-paste mode from the persisted sidecar: on the
        // snapshot-timeout path the session *is* live, and handing it an
        // unprefixed replay re-opens the stale-paste-flag bug.
        let bp_enabled = crate::termstate::load(&hub.dirs, session_id);
        let (data, truncated) = build_replay_snapshot(Some(&hub.dirs), session_id, bp_enabled);
        let _ = out_tx.send(scrollback_reply(
            session_id, &data, truncated, request_id, false,
        ));
    }
}

/// Queues the snapshot's `Scrollback` reply, then starts the per-client
/// forwarder for its live receiver. The receiver was subscribed when the
/// snapshot was taken, so output produced meanwhile waits in it; and since
/// `out_tx` is a FIFO, the reply reaches the client before any `PtyOutput`
/// from the new forwarder.
async fn reply_then_forward(
    snap: ScrollbackSnapshot,
    session_id: &str,
    request_id: Option<String>,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    pty_forwarders: &PtyForwarders,
) {
    // Replace any existing forwarder for this session (e.g. a previous
    // Terminal mount in the same WS session that didn't get a clean
    // Detach). Stop it before the reply, spawn the fresh one after.
    let mut forwarders = pty_forwarders.lock().await;
    if let Some(old) = forwarders.remove(session_id) {
        old.abort();
        // `abort` does not wait, and on the multi-threaded runtime the old
        // forwarder may be mid-send on another worker; awaiting it (it
        // yields `Err(Cancelled)` once stopped) keeps its `PtyOutput` from
        // landing after the reply. It holds only its live receiver and an
        // `out_tx` clone, never this map's lock, so waiting here is safe.
        let _ = old.await;
    }
    let _ = out_tx.send(scrollback_reply(
        session_id,
        &snap.data,
        snap.truncated,
        request_id,
        true,
    ));
    let mut live = snap.live;
    let out = out_tx.clone();
    let sid = session_id.to_string();
    let handle = tokio::spawn(async move {
        loop {
            match live.recv().await {
                Ok(bytes) => {
                    let data_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    if out
                        .send(DaemonMessage::PtyOutput {
                            session_id: sid.clone(),
                            data_b64,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(
                        session_id = %sid,
                        lagged = n,
                        "per-client pty forwarder lagged"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    forwarders.insert(session_id.to_string(), handle);
}

/// One `Scrollback` reply, echoing the request's id.
fn scrollback_reply(
    session_id: &str,
    data: &[u8],
    truncated: bool,
    request_id: Option<String>,
    forwarder_restarted: bool,
) -> DaemonMessage {
    DaemonMessage::Scrollback {
        session_id: session_id.to_string(),
        data_b64: base64::engine::general_purpose::STANDARD.encode(data),
        truncated,
        request_id,
        forwarder_restarted,
    }
}

/// Per-spawn configuration bundled together to keep spawn-fn signatures narrow.
/// The agent kind is carried inside `agent_options` — call
/// `cfg.agent_options.agent()` to dispatch through [`crate::agents::backend_for`].
struct SpawnArgs {
    dangerously_skip_permissions: bool,
    /// Per-agent options. The variant tag doubles as the agent selector.
    agent_options: AgentOptions,
    model: Option<String>,
    /// The child's environment: the keep-list, the resolved rows and the
    /// provider's routing variables. Never stored.
    env: crate::agents::providers::SpawnEnv,
    /// Scripted PTY input driven post-spawn for interactive sessions. When
    /// `Some`, the interactive spawn path omits the prompt CLI flag — the
    /// injector is responsible for delivering the prompt. Ignored by the
    /// headless and plain-shell paths.
    prompt_injector: Option<protocol::PromptInjector>,
    /// The request that asked for this spawn; the session's first broadcast
    /// carries it, so the requester hears of its spawn once, in order.
    origin: Option<UpdateOrigin>,
    /// The Claude conversation an interactive Claude spawn resumes.
    resume_conversation: Option<String>,
    /// A standalone target's extra `--add-dir` directories.
    add_dirs: Vec<String>,
}

impl SpawnArgs {
    /// Which agent this spawn targets. Derived from `agent_options`.
    fn agent(&self) -> Agent {
        self.agent_options.agent()
    }

    /// Build the borrowed [`CommonSpawnFields`] view backends consume.
    fn common(&self) -> CommonSpawnFields<'_> {
        CommonSpawnFields {
            dangerously_skip_permissions: self.dangerously_skip_permissions,
            model: self.model.as_deref(),
            has_prompt_injector: self.prompt_injector.is_some(),
            claude_session_id: None,
            resume_conversation: self.resume_conversation.as_deref(),
            add_dirs: &self.add_dirs,
            // Only `spawn_interactive_session` writes a settings file and
            // overrides this.
            hook_settings: None,
        }
    }
}

/// What a new interactive session's record takes from its spawn.
struct NewInteractive {
    session_id: String,
    label: String,
    kind: SessionKind,
    members: Vec<SessionMember>,
    workspace_id: Option<String>,
    agent: Agent,
    cwd: String,
    stored_config: protocol::SpawnConfig,
    last_prompt: Option<String>,
    claude_session_id: Option<String>,
    started_at: chrono::DateTime<Utc>,
}

/// The record of a new interactive session.
fn interactive_record(
    new: NewInteractive,
    pty: Option<Arc<crate::pty::PtyHandle>>,
) -> SessionRecord {
    let mut record = SessionRecord {
        id: new.session_id,
        label: new.label.clone(),
        default_label: new.label,
        user_label: None,
        kind: new.kind,
        worktree_paths: worktree_paths_for_config(&new.stored_config, &new.members),
        members: new.members,
        mode: SessionMode::Interactive,
        started_at: new.started_at,
        status: SessionStatus::Idle,
        status_since: Some(new.started_at),
        exit_code: None,
        metrics: SessionMetrics::default(),
        recent_actions: Vec::new(),
        pty,
        headless: None,
        workspace_id: new.workspace_id,
        agent: new.agent,
        terminal_title: None,
        program_name: Some(new.agent.as_label().to_string()),
        current_cwd: Some(new.cwd),
        appearance: AppearanceOverrides::default(),
        spawn_config: Some(new.stored_config),
        is_abandoned: false,
        is_inactive: false,
        last_prompt: new.last_prompt,
        input_notifier: None,
        scrollback_snapshot_req: None,
        spawn_origin: None,
        claude_session_id: new.claude_session_id,
        pending_input: None,
        last_hook_time: None,
        hook_driven: false,
        last_assistant_message: None,
        busy_excluded: false,
    };
    push_recent_action(&mut record, "session started".to_string());
    record
}

/// Where a session's tracer runs, as its sidecar records it.
struct TracerSidecar {
    pid: u32,
    pipe: String,
    exe_path: String,
}

/// The spawn-time sidecar of the new interactive session `record`, whose
/// child is `pid`. The direct daemon child is `rt-tracer.exe`, not the agent
/// CLI, so `program_name` names the tracer and `agent` the CLI;
/// `is_session_alive` prefers `tracer_pid` + "rt-tracer" when those are set.
fn interactive_meta(
    record: &SessionRecord,
    pid: u32,
    tracer: TracerSidecar,
) -> anyhow::Result<OrphanMeta> {
    orphan::meta_from_record(
        record.id.clone(),
        pid,
        record.label.clone(),
        record.kind.clone(),
        SessionMode::Interactive,
        record.members.clone(),
        record.started_at,
        record.workspace_id.clone(),
        Some("rt-tracer".to_string()),
        record.agent,
        record.spawn_config.clone(),
        record.last_prompt.clone(),
        record.current_cwd.clone(),
        Some(tracer.pid),
        Some(tracer.pipe),
        Some(tracer.exe_path),
        record.claude_session_id.clone(),
    )
}

/// The injected hook settings one spawn wrote — an interactive Claude session
/// or a plain-shell recovery — and what it made on disk.
struct HookSettings {
    /// The `--settings` file's path.
    path: PathBuf,
    /// The cached `rt-tracer` the file's hook command names. This is the same
    /// path the spawn runs its tracer from, so the file and the supervisor
    /// cannot disagree about which copy runs the shim.
    tracer_exe: PathBuf,
    /// True when writing the file created `sessions/<id>/`; a spawn that then
    /// fails removes only a directory it made.
    created_dir: bool,
}

impl HookSettings {
    /// The file the spawn passes as `--settings`.
    fn path(&self) -> &Path {
        &self.path
    }

    /// Remove what this spawn wrote after the tracer spawn that would have run
    /// it failed: always the file, and the directory only when this spawn
    /// created it, so no sidecar-less `sessions/<id>/` is left behind for the
    /// orphan scan to skip forever.
    fn discard(self, session_id: &str) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => warn!(
                %session_id,
                path = %self.path.display(),
                error = %format!("{err:#}"),
                "removing the hook settings file of a failed spawn failed"
            ),
        }
        if self.created_dir {
            remove_session_dir_if_empty(&self.path, session_id);
        }
    }
}

/// Remove the session directory `path` sits in, best-effort: only a directory
/// this daemon made for the failed spawn is a candidate, and a non-empty one
/// (another writer got there first) is left alone.
fn remove_session_dir_if_empty(path: &Path, session_id: &str) {
    let Some(dir) = path.parent() else {
        return;
    };
    match std::fs::remove_dir(dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => warn!(
            %session_id,
            path = %dir.display(),
            error = %format!("{err:#}"),
            "removing the session dir of a failed spawn failed"
        ),
    }
}

/// Remove a gracefully stopped session's hook settings file, then its session
/// directory when that file was all it still held. A natural exit keeps the
/// file (see [`write_hook_settings`]); a session that never got hooks — one
/// whose settings write failed — has no file to remove, which is not an error. A directory still holding `scrollback.bin` is left in place.
fn remove_hook_settings(hub: &Hub, session_id: &str) {
    let path = hook_settings_path(&hub.dirs, session_id);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => warn!(
            %session_id,
            path = %path.display(),
            error = %format!("{err:#}"),
            "removing a stopped session's hook settings failed"
        ),
    }
    let Some(dir) = path.parent() else {
        return;
    };
    match std::fs::remove_dir(dir) {
        Ok(()) => {}
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) => {}
        Err(err) => warn!(
            %session_id,
            path = %dir.display(),
            error = %format!("{err:#}"),
            "removing a stopped session's empty directory failed"
        ),
    }
}

/// Render a recovered shell's typed command for `settings`: the settings
/// clause names the quoted path when the spawn wrote a file, and is dropped
/// entirely otherwise.
///
/// The path is always double-quoted, since the config dir can hold spaces. A
/// path holding a double quote cannot be quoted for cmd and POSIX shells at
/// once, so it renders the hookless form a missing file does: the shell parses
/// the command either way.
pub(crate) fn render_shell_resume(content: &str, settings: Option<&Path>) -> String {
    match settings {
        Some(path) if !path.to_string_lossy().contains('"') => content.replace(
            history::HOOK_SETTINGS_SLOT,
            &format!("\"{}\"", path.display()),
        ),
        Some(_) | None => {
            content.replace(&format!("--settings {} ", history::HOOK_SETTINGS_SLOT), "")
        }
    }
}

/// The file name a spawn's injected Claude hook settings land under, inside the
/// session's own directory.
const HOOK_SETTINGS_FILE: &str = "hook-settings.json";

/// `session_id`'s injected hook settings file, which a Claude spawn passes as
/// `--settings` and the hook shim reads.
fn hook_settings_path(dirs: &Dirs, session_id: &str) -> PathBuf {
    dirs.sessions_dir.join(session_id).join(HOOK_SETTINGS_FILE)
}

/// Write the new session's injected hook settings file and return what to pass
/// as `--settings`, or `None` when this spawn gets no hooks.
///
/// Only an interactive Claude spawn and a plain-shell recovery (a spawn
/// carrying a [`protocol::InjectorStartup::ShellPrompt`] injector) get hooks.
/// A cached tracer binary that cannot be resolved, or a failed write, turns
/// hooks off for this spawn with a `warn!` and never refuses it: a session
/// without them falls back to the terminal heuristic. The file is written
/// eagerly, before whatever names it (an interactive spawn's argv, a
/// recovery's typed command), into the session's own `sessions/<id>/`,
/// creating that directory before `meta.json`'s writer would. A graceful stop
/// deletes the sidecar and this file, and the directory itself when that
/// leaves it empty; a natural exit keeps the file. A spawn that then fails
/// removes what this call made (see [`HookSettings::discard`]).
fn write_hook_settings(hub: &Hub, session_id: &str, agent: Agent) -> Option<HookSettings> {
    if agent != Agent::Claude {
        return None;
    }
    let tracer_exe = match tracer_client::cached_tracer_exe(&hub.dirs) {
        Ok(path) => path,
        Err(err) => {
            warn!(
                %session_id,
                error = %format!("{err:#}"),
                "resolving the cached tracer for the hook settings failed; spawning without hooks"
            );
            return None;
        }
    };
    let dir = hub.dirs.sessions_dir.join(session_id);
    let path = dir.join(HOOK_SETTINGS_FILE);
    let created_dir = !dir.exists();
    let settings = crate::agents::build_hook_settings(&tracer_exe, session_id, &hub.dirs.config);
    let written = std::fs::create_dir_all(&dir)
        .map_err(anyhow::Error::from)
        .and_then(|()| orphan::write_atomic(&path, settings.as_bytes()));
    if let Err(err) = written {
        warn!(
            %session_id,
            path = %path.display(),
            error = %format!("{err:#}"),
            "writing the hook settings file failed; spawning without hooks"
        );
        // The spawn continues without hooks, so release a directory this call
        // made; a later success on this path can create it again.
        if created_dir {
            remove_session_dir_if_empty(&path, session_id);
        }
        return None;
    }
    Some(HookSettings {
        path,
        tracer_exe,
        created_dir,
    })
}

/// Announce a replayed prompt's attention, once its session is published and
/// only when the record still holds the prompt after the whole replay.
fn announce_replayed_prompt(hub: &Hub, session_id: &str, awaiting: bool) {
    if awaiting {
        pty_state::announce_awaiting_input(session_id, &hub.attention_tx);
    }
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
    let agent = cfg.agent();
    let backend = crate::agents::backend_for(agent);
    let raw_program = backend.resolve_program();
    let (program, prepend_args) = resolve_agent_program(&raw_program);
    // Claude gets a known conversation id so the session can be resumed with
    // `claude --resume <id>` after it ends. A resumed conversation keeps its id.
    let claude_session_id = (agent == Agent::Claude).then(|| {
        cfg.resume_conversation
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
    });
    // The argv names the injected settings file, so it is written before the
    // argv is built. Only Claude takes it; a failure leaves this spawn on the
    // heuristic.
    let hook_settings = write_hook_settings(hub, &session_id, agent);
    let common = CommonSpawnFields {
        claude_session_id: claude_session_id.as_deref(),
        hook_settings: hook_settings.as_ref().map(HookSettings::path),
        ..cfg.common()
    };
    let args = backend.build_interactive_args(
        &cfg.agent_options,
        &common,
        &members,
        initial_prompt.as_deref(),
    );

    let mut final_args = prepend_args;
    final_args.extend(args);
    info!(
        agent = agent.as_label(),
        raw = %raw_program,
        program = %program,
        argc = final_args.len(),
        args = ?final_args,
        "spawn_interactive_session: resolved program"
    );

    let initial_cwd = primary_cwd.to_string_lossy().into_owned();
    let spec = PtySpawnSpec {
        session_id: session_id.clone(),
        program,
        args: final_args,
        cwd: primary_cwd,
        env: cfg.env.set.clone(),
        env_remove: cfg.env.remove.clone(),
        cols: 120,
        rows: 32,
    };
    #[cfg(test)]
    child_env_probe::record(&spec.env, &spec.env_remove);
    let expected_subscribers = expected_output_subscribers(SessionMode::Interactive);
    let spawn_result = match hook_settings
        .as_ref()
        .map(|settings| settings.tracer_exe.as_path())
    {
        // The same cached copy the injected settings file names, so the hook
        // command can only name the tracer this session really runs.
        Some(tracer_exe) => {
            tracer_client::spawn_with_tracer(tracer_exe, &hub.dirs, spec, expected_subscribers)
                .await
        }
        None => tracer_client::spawn(&hub.dirs, spec, expected_subscribers).await,
    };
    let tracer_spawn = match spawn_result {
        Ok(spawned) => spawned,
        Err(err) => {
            // A failed spawn must leave neither a settings file with nothing
            // to clean it up nor a sidecar-less session dir, which the orphan
            // scan skips forever.
            if let Some(settings) = hook_settings {
                settings.discard(&session_id);
            }
            return Err(err.context(format!("spawning {} via tracer", agent.as_label())));
        }
    };
    let pty = tracer_spawn.handle;
    let tracer = TracerSidecar {
        pid: tracer_spawn.tracer_pid,
        pipe: tracer_spawn.pipe_name,
        exe_path: tracer_spawn.tracer_exe_path.to_string_lossy().into_owned(),
    };
    let record = interactive_record(
        NewInteractive {
            session_id: session_id.clone(),
            label,
            kind,
            members,
            workspace_id,
            agent,
            cwd: initial_cwd,
            stored_config,
            last_prompt,
            claude_session_id,
            started_at: Utc::now(),
        },
        Some(Arc::clone(&pty)),
    );
    let meta = pty
        .pid()
        .and_then(|pid| match interactive_meta(&record, pid, tracer) {
            Ok(meta) => Some(meta),
            Err(err) => {
                warn!(?err, %session_id, "no sidecar written for the session");
                None
            }
        });

    // Two-stage insert: put the record in the registry first so attach_lifecycle's
    // exit watcher can find it, then patch the lifecycle channels onto the record
    // before publishing SessionUpdated. Publishing earlier would race with the
    // client's LoadScrollback request — the client could see the session and
    // request scrollback before scrollback_snapshot_req is wired up, fall through
    // to a direct file read (empty before first output), and miss any output that
    // streams in before the per-client forwarder is started. Hook events held
    // for this id while the tracer spawned are applied to the record here, so
    // the first broadcast is still the wired publish below.
    let (pending, awaiting) = hub.pending_hooks.insert_with_pending(hub, record);
    let snap_tx = attach_lifecycle(
        &hub.sessions,
        session_id.clone(),
        &pty,
        Some(hub.dirs.clone()),
    );
    let (in_tx, in_rx) = mpsc::unbounded_channel::<usize>();
    {
        let mut guard = crate::sync::lock(pending.arc());
        guard.scrollback_snapshot_req = Some(snap_tx);
        guard.input_notifier = Some(in_tx);
    }
    pty_state::watch(
        &hub.sessions,
        session_id.clone(),
        pty_state::WatchStreams {
            output: pty.output.subscribe(),
            input_pulses: in_rx,
        },
        hub.attention_tx.clone(),
        SessionStatus::Idle,
    );
    osc_title::watch(
        &hub.sessions,
        session_id.clone(),
        pty.output.subscribe(),
        false,
    );
    let snap = crate::sync::lock(pending.arc()).snapshot();
    pending.publish_from(cfg.origin.clone());
    announce_replayed_prompt(hub, &session_id, awaiting);

    if let Some(mut meta) = meta {
        meta.status = Some(snap.status);
        meta.status_since = snap.status_since;
        write_meta_then_sync(hub, &meta);
    }

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
    if let AgentOptions::Claude {
        permission_mode: Some(_),
        ..
    } = &cfg.agent_options
    {
        return Err(anyhow!(
            "plain shell sessions do not accept a permission_mode"
        ));
    }
    if cfg.dangerously_skip_permissions {
        return Err(anyhow!(
            "plain shell sessions do not accept dangerously_skip_permissions"
        ));
    }

    let shell = resolve_shell_program(&hub.dirs)?;
    // A recovered shell gets the same injected hooks an interactive Claude
    // spawn does, so the CLI's reports drive its status. The file is written
    // after the last fallible setup step and before the spawn, and the
    // command the injector types names it.
    let hook_settings = if cfg
        .prompt_injector
        .as_ref()
        .is_some_and(|injector| injector.startup == protocol::InjectorStartup::ShellPrompt)
    {
        write_hook_settings(hub, &session_id, cfg.agent())
    } else {
        None
    };
    let initial_cwd = primary_cwd.to_string_lossy().into_owned();
    let mut env = cfg.env.set.clone();
    for (k, v) in shell.extra_env {
        if let Some(slot) = env.iter_mut().find(|(existing, _)| existing == &k) {
            slot.1 = v;
        } else {
            env.push((k, v));
        }
    }
    let spec = PtySpawnSpec {
        session_id: session_id.clone(),
        program: shell.program,
        args: shell.args,
        cwd: primary_cwd,
        env,
        env_remove: cfg.env.remove.clone(),
        cols: 120,
        rows: 32,
    };
    #[cfg(test)]
    child_env_probe::record(&spec.env, &spec.env_remove);
    let expected_subscribers = expected_output_subscribers(SessionMode::PlainShell);
    let spawn_result = match &hook_settings {
        // The same cached copy the settings file names, so that file can only
        // name the tracer this session really runs.
        Some(settings) => {
            tracer_client::spawn_with_tracer(
                &settings.tracer_exe,
                &hub.dirs,
                spec,
                expected_subscribers,
            )
            .await
        }
        None => tracer_client::spawn(&hub.dirs, spec, expected_subscribers).await,
    };
    let tracer_spawn = match spawn_result {
        Ok(spawned) => spawned,
        Err(err) => {
            // A failed spawn must leave neither a settings file with nothing
            // to clean it up nor a sidecar-less session dir, which the orphan
            // scan skips forever.
            if let Some(settings) = hook_settings {
                settings.discard(&session_id);
            }
            return Err(err.context("spawning plain shell via tracer"));
        }
    };
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
        status: SessionStatus::Idle,
        status_since: Some(started_at),
        exit_code: None,
        metrics: SessionMetrics::default(),
        recent_actions: Vec::new(),
        pty: Some(Arc::clone(&pty)),
        headless: None,
        workspace_id: workspace_id.clone(),
        // Plain-shell sessions have no agent CLI in the picture; the record
        // still needs an agent, so it carries the default one.
        agent: Agent::Claude,
        terminal_title: None,
        program_name: Some(shell.label.clone()),
        current_cwd: Some(initial_cwd.clone()),
        appearance: AppearanceOverrides::default(),
        spawn_config: Some(stored_config.clone()),
        is_abandoned: false,
        is_inactive: false,
        worktree_paths: worktree_paths_for_config(&stored_config, &members),
        // Plain shells never carry a kickoff prompt.
        last_prompt: None,
        input_notifier: None,
        scrollback_snapshot_req: None,
        spawn_origin: None,
        claude_session_id: None,
        pending_input: None,
        last_hook_time: None,
        hook_driven: false,
        last_assistant_message: None,
        busy_excluded: false,
    };
    push_recent_action(
        &mut record,
        format!("session started: {}", shell.label.as_str()),
    );

    // Two-stage insert — see spawn_interactive_session for the same pattern
    // and rationale, including the held hook events applied here.
    let (pending, awaiting) = hub.pending_hooks.insert_with_pending(hub, record);
    let snap_tx = attach_lifecycle(
        &hub.sessions,
        session_id.clone(),
        &pty,
        Some(hub.dirs.clone()),
    );
    let (in_tx, in_rx) = mpsc::unbounded_channel::<usize>();
    {
        let mut guard = crate::sync::lock(pending.arc());
        guard.scrollback_snapshot_req = Some(snap_tx);
        guard.input_notifier = Some(in_tx);
    }
    pty_state::watch_plain_shell(
        &hub.sessions,
        session_id.clone(),
        pty_state::WatchStreams {
            output: pty.output.subscribe(),
            input_pulses: in_rx,
        },
        SessionStatus::Idle,
    );
    // Keep `osc_title::watch` so window-title escape sequences (which pwsh
    // emits by default) are recorded as `terminal_title` annotations; the
    // canonical session `label` stays whatever the spawn pipeline assigned.
    osc_title::watch(
        &hub.sessions,
        session_id.clone(),
        pty.output.subscribe(),
        true,
    );
    let snap = crate::sync::lock(pending.arc()).snapshot();
    pending.publish_from(cfg.origin.clone());
    announce_replayed_prompt(hub, &session_id, awaiting);

    if let Some(pid) = pid
        && let Ok(mut meta) = orphan::meta_from_record(
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
            Some(initial_cwd),
            Some(tracer_pid),
            Some(tracer_pipe),
            Some(tracer_exe_path),
            None,
        )
    {
        meta.status = Some(snap.status);
        meta.status_since = snap.status_since;
        write_meta_then_sync(hub, &meta);
    }

    // A recovered shell types its resume command through an injector, naming
    // the hook settings file this spawn wrote, or the hookless form when it
    // wrote none.
    if let Some(mut injector) = cfg.prompt_injector.clone() {
        let settings = hook_settings.as_ref().map(HookSettings::path);
        for step in &mut injector.steps {
            if let protocol::InjectorStep::Text { content, .. } = step {
                *content = render_shell_resume(content, settings);
            }
        }
        inject::run(session_id.clone(), Arc::clone(&pty), injector);
    }
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
    // The dispatcher already rejects headless mode for backends that don't
    // support it; if we got here, the backend can build headless args.
    let backend = crate::agents::backend_for(cfg.agent());
    debug_assert!(backend.supports_headless());
    let last_prompt = Some(prompt.clone());
    let args = backend.build_headless_args(&cfg.agent_options, &cfg.common(), &members, &prompt);

    let initial_cwd = primary_cwd.to_string_lossy().into_owned();
    let spec = headless::HeadlessSpec {
        program: backend.resolve_program(),
        args,
        cwd: primary_cwd,
        env: cfg.env.set.clone(),
        agent: cfg.agent(),
    };
    #[cfg(test)]
    child_env_probe::record(&spec.env, &[]);

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
        status_since: Some(started_at),
        exit_code: None,
        metrics: SessionMetrics::default(),
        recent_actions: Vec::new(),
        pty: None,
        headless: None,
        workspace_id: workspace_id.clone(),
        agent: cfg.agent(),
        terminal_title: None,
        program_name: Some(cfg.agent().as_label().to_string()),
        current_cwd: Some(initial_cwd.clone()),
        appearance: AppearanceOverrides::default(),
        spawn_config: Some(stored_config.clone()),
        is_abandoned: false,
        is_inactive: false,
        worktree_paths: worktree_paths_for_config(&stored_config, &members),
        last_prompt: last_prompt.clone(),
        input_notifier: None,
        scrollback_snapshot_req: None,
        spawn_origin: None,
        claude_session_id: None,
        pending_input: None,
        last_hook_time: None,
        hook_driven: false,
        last_assistant_message: None,
        busy_excluded: false,
    };
    push_recent_action(&mut record, "headless session started".to_string());
    // Held unpublished until the child runs, so a failed spawn is never
    // broadcast and the first snapshot clients see carries its handle.
    let pending = hub.sessions.insert_pending(record);
    let handle = match headless::spawn(&spec, &hub.sessions, session_id.clone(), hub.dirs.clone()) {
        Ok(handle) => handle,
        Err(err) => {
            pending.discard();
            return Err(err.context(format!("spawning headless {}", cfg.agent().as_label())));
        }
    };
    let snap = {
        let mut rec = crate::sync::lock(pending.arc());
        rec.headless = Some(Arc::clone(&handle));
        rec.snapshot()
    };
    pending.publish_from(cfg.origin.clone());

    if let Some(pid) = handle.pid()
        && let Ok(mut meta) = orphan::meta_from_record(
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
            cfg.agent(),
            Some(stored_config),
            last_prompt,
            Some(initial_cwd),
            // Headless `claude --print` doesn't use a PTY and therefore doesn't
            // go through rt-tracer; the tracer fields stay None.
            None,
            None,
            None,
            None,
        )
    {
        meta.status = Some(snap.status);
        meta.status_since = snap.status_since;
        write_meta_then_sync(hub, &meta);
    }

    Ok(snap)
}

type SpawnResolution = (
    SessionKind,
    Vec<SessionMember>,
    PathBuf,
    String,
    Option<String>,
);

/// Map a raw `git worktree add` failure to a tailored `SpawnFailure`. Known
/// git refusals (branch already checked out, path already exists) get an
/// actionable message; anything else falls back to git's full stderr chain.
fn classify_worktree_error(err: &anyhow::Error, branch_name: &str) -> spawn_plan::SpawnFailure {
    let chain = format!("{err:#}");
    let lower = chain.to_lowercase();
    if lower.contains("already checked out") || lower.contains("already used by worktree") {
        spawn_plan::SpawnFailure {
            title: "Branch already checked out".to_string(),
            detail: format!(
                "Git won't create a worktree for '{branch_name}' because that branch is already checked out elsewhere — a branch can only be checked out in one worktree at a time."
            ),
            hint: Some(
                "Pick a different branch name (the Random button generates a unique one), or base the new worktree on a fresh branch."
                    .to_string(),
            ),
        }
    } else if lower.contains("already exists") {
        spawn_plan::SpawnFailure {
            title: "Worktree path already exists".to_string(),
            detail: format!("A directory for this worktree already exists on disk.\n\n{chain}"),
            hint: Some(
                "Choose a different branch name, or remove the stale worktree directory and try again."
                    .to_string(),
            ),
        }
    } else {
        spawn_plan::SpawnFailure {
            title: "Couldn't create worktree".to_string(),
            detail: chain,
            hint: None,
        }
    }
}

/// A [`ClientMessage::DuplicateSession`], as the daemon handles it.
struct DuplicateAsk<'a> {
    session_id: &'a str,
    request_id: Option<String>,
    carry_busy_excluded: bool,
}

/// Spawns a copy of the asked session from its stored config, carrying its
/// busy exclusion when asked to. The copy's first broadcast carries
/// `request_id` on connection `connection`; a failure answers the requester
/// directly, carrying it too.
async fn duplicate_session(
    hub: &Hub,
    ask: DuplicateAsk<'_>,
    connection: u64,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) {
    let DuplicateAsk {
        session_id,
        request_id,
        carry_busy_excluded,
    } = ask;
    let busy_excluded = carried_busy_excluded(&hub.sessions, session_id, carry_busy_excluded);
    let req = match duplicate_request(hub, session_id).await {
        Ok(req) => req,
        Err(err) => {
            let _ = out_tx.send(DaemonMessage::Error {
                message: err.to_string(),
                request_id,
            });
            return;
        }
    };
    let origin = request_origin(connection, request_id.clone());
    if let Err(err) = spawn_keeping_busy_excluded(hub, req, origin, busy_excluded).await {
        send_spawn_failure(out_tx, &err, request_id.as_deref());
    }
}

/// Spawn `req`, then, when `busy_excluded` is set, mark the new session as
/// not counted as busy (see [`adopt_busy_excluded`]). Every spawn that keeps
/// a session's exclusion (Restart, Resume, recovery) goes through here. The
/// returned snapshot is the spawn's, taken before the flag was set.
async fn spawn_keeping_busy_excluded(
    hub: &Hub,
    req: SpawnRequest,
    origin: Option<UpdateOrigin>,
    busy_excluded: bool,
) -> anyhow::Result<protocol::SessionSnapshot> {
    let snapshot = spawn_session(hub, req, origin).await?;
    adopt_busy_excluded(&hub.sessions, &snapshot.id, busy_excluded);
    Ok(snapshot)
}

/// The busy exclusion a copy of `source_id` starts with: the source's when
/// `carry` is set (a Restart or a Resume), else none (a Duplicate). A source
/// the registry no longer holds has nothing to carry.
fn carried_busy_excluded(sessions: &SessionRegistry, source_id: &str, carry: bool) -> bool {
    carry
        && sessions
            .get(source_id)
            .is_some_and(|rec| crate::sync::lock(&rec).busy_excluded)
}

/// Mark the freshly spawned `session_id` as not counted as busy when
/// `excluded` is set; the registry update mirrors it into the sidecar and
/// broadcasts the session. A counted session needs no update.
fn adopt_busy_excluded(sessions: &SessionRegistry, session_id: &str, excluded: bool) {
    if excluded {
        sessions.update(session_id, |guard| guard.busy_excluded = true);
    }
}

/// The origin of a registry update made by connection `connection`'s
/// request `request_id`; `None` when the request carried no id, since
/// there is then nothing to echo.
fn request_origin(connection: u64, request_id: Option<String>) -> Option<UpdateOrigin> {
    request_id.map(|request_id| UpdateOrigin {
        connection,
        request_id,
    })
}

/// The request that duplicates `session_id`. A worktree duplicate runs on
/// its own branch: the source still holds its checkout, and a name the
/// daemon picks can't attach a leftover the user was never shown.
async fn duplicate_request(hub: &Hub, session_id: &str) -> anyhow::Result<SpawnRequest> {
    let stored = hub
        .sessions
        .get(session_id)
        .map(|rec| crate::sync::lock(&rec).spawn_config.clone())
        .ok_or_else(|| anyhow!("unknown session: {session_id}"))?;
    let Some(stored) = stored else {
        return Err(anyhow!(
            "session {session_id} has no stored spawn config; cannot duplicate"
        ));
    };
    let fresh = match stored.target.suggest_target() {
        Some(target) if target_uses_worktree(&stored.target) => {
            Some(branch_names::suggest(&hub.state, &hub.state.worktrees_dir(), &target).await?)
        }
        _ => None,
    };
    info!(source = %session_id, ?fresh, "duplicate_session: spawning");
    Ok(stored.to_duplicate_request(fresh))
}

/// What connection `connection` receives for a registry event. A
/// `SessionUpdated` carries the `request_id` of the request that made it
/// only on the connection that sent that request.
fn session_event_message(event: SessionEvent, connection: u64) -> DaemonMessage {
    match event {
        SessionEvent::Updated(snap, origin) => DaemonMessage::SessionUpdated {
            session: *snap,
            request_id: origin
                .filter(|origin| origin.connection == connection)
                .map(|origin| origin.request_id),
        },
        SessionEvent::Removed(session_id) => DaemonMessage::SessionRemoved { session_id },
        SessionEvent::Attention { session_id, reason } => {
            DaemonMessage::Attention { session_id, reason }
        }
    }
}

/// A [`ClientMessage::SetSessionAppearance`], as the daemon handles it.
struct AppearanceChangeRequest<'a> {
    session_id: &'a str,
    appearance: &'a AppearanceOverrides,
    request_id: Option<String>,
    /// The connection that sent the request.
    connection: u64,
}

/// Stores a session's appearance. Every client sees the change through the
/// registry's one `SessionUpdated` broadcast, in order with every other
/// update; on the requester's connection it carries `request_id`. A refused
/// appearance, or a session the registry does not hold, answers the
/// requester with an `Error` carrying it.
fn set_session_appearance(
    sessions: &SessionRegistry,
    change: AppearanceChangeRequest<'_>,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) {
    let AppearanceChangeRequest {
        session_id,
        appearance,
        request_id,
        connection,
    } = change;
    let appearance = match appearance.normalized() {
        Ok(appearance) => appearance,
        Err(err) => {
            let _ = out_tx.send(DaemonMessage::Error {
                message: err.to_string(),
                request_id,
            });
            return;
        }
    };
    let origin = request_origin(connection, request_id.clone());
    // The registry update mirrors the new appearance into the sidecar.
    let found = sessions.update_from(session_id, origin, |guard| {
        guard.appearance.clone_from(&appearance);
    });
    if !found {
        let _ = out_tx.send(DaemonMessage::Error {
            message: format!("unknown session: {session_id}"),
            request_id,
        });
    }
}

/// Forwards the registry's session events to connection `connection`, in
/// the order the registry sent them. A connection that falls behind the
/// broadcast buffer gets the whole `Sessions` list in place of the events it
/// missed, and the events after it.
fn spawn_session_forwarder(
    registry: Arc<SessionRegistry>,
    out_tx: mpsc::UnboundedSender<DaemonMessage>,
    connection: u64,
) -> tokio::task::JoinHandle<()> {
    // Subscribed before the task starts, so no event sent after this call
    // is missed.
    let mut rx = registry.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let _ = out_tx.send(session_event_message(event, connection));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(
                        lagged = n,
                        connection, "client event stream lagged; resending the session list"
                    );
                    // Subscribe before taking the list: the stale buffered
                    // events go with the old receiver, and every event after
                    // the list applies on top of it.
                    rx = registry.subscribe();
                    for msg in resync_messages(&registry, connection) {
                        let _ = out_tx.send(msg);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// What a connection that lagged past the broadcast buffer gets: the whole
/// `Sessions` list, then each listed session's spawn reply once more for the
/// sessions this connection's own requests spawned, since the list carries
/// no `request_id` and the reply may have been among the events it missed.
fn resync_messages(registry: &SessionRegistry, connection: u64) -> Vec<DaemonMessage> {
    let listed = registry.snapshots_with_spawn_origins();
    let replies: Vec<DaemonMessage> = listed
        .iter()
        .filter_map(|(session, origin)| {
            let origin = origin.as_ref().filter(|o| o.connection == connection)?;
            Some(DaemonMessage::SessionUpdated {
                session: session.clone(),
                request_id: Some(origin.request_id.clone()),
            })
        })
        .collect();
    let sessions = listed.into_iter().map(|(session, _)| session).collect();
    std::iter::once(DaemonMessage::Sessions { sessions })
        .chain(replies)
        .collect()
}

/// Send a spawn failure to the requesting client as a blocking `ActionFailed`
/// modal, echoing the request's id. Uses the structured `SpawnFailure` fields
/// when present; otherwise surfaces the full anyhow chain under a generic
/// title.
fn send_spawn_failure(
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    err: &anyhow::Error,
    request_id: Option<&str>,
) {
    let msg = err.downcast_ref::<spawn_plan::SpawnFailure>().map_or_else(
        || DaemonMessage::ActionFailed {
            title: "Couldn't start session".to_string(),
            detail: format!("{err:#}"),
            hint: None,
            request_id: request_id.map(str::to_owned),
        },
        |sf| DaemonMessage::ActionFailed {
            title: sf.title.clone(),
            detail: sf.detail.clone(),
            hint: sf.hint.clone(),
            request_id: request_id.map(str::to_owned),
        },
    );
    let _ = out_tx.send(msg);
}

/// Resolve a single-repo spawn without touching the disk: which ref the
/// branch would fork from, how stale that ref is, and whether a worktree is
/// already sitting at the target path.
///
/// Deliberately mirrors `spawn_single`'s resolution rather than duplicating
/// it — both call `spawn_plan::resolve_base_for_create`, so a preview can't
/// promise a fork point the spawn won't honor.
async fn preview_single_spawn(
    hub: &Hub,
    repo_id: &str,
    branch_name: &str,
    base_branch: Option<&str>,
    use_worktree: bool,
) -> anyhow::Result<protocol::MemberSpawnPreview> {
    let repo = hub
        .state
        .with_persisted(|s| s.repos.iter().find(|r| r.id == repo_id).cloned())
        .ok_or_else(|| anyhow!("unknown repo: {repo_id}"))?;
    let repo_path = PathBuf::from(&repo.path);

    let branch_exists = crate::git::list_branches(&repo_path)
        .await
        .unwrap_or_default()
        .iter()
        .any(|b| b == branch_name);
    // `effective_base` describes branch *creation*, so it stays empty when the
    // branch already exists. The resolved ref is still computed either way:
    // RecreateFromBase forks the worktree from it, and a leftover worktree
    // always has its branch, so gating this on `branch_exists` would blank out
    // the staleness figures in exactly the case they matter most.
    let resolved_base = spawn_plan::resolve_base_for_create(&hub.state, &repo, base_branch).await;
    let effective_base = (!branch_exists).then(|| resolved_base.clone());

    let worktree_path = if use_worktree {
        single_worktree_target(&hub.state.worktrees_dir(), &repo, branch_name).await?
    } else {
        repo_path.clone()
    };
    let existing = (use_worktree && worktree_path.exists()).then_some(worktree_path.as_path());
    // Branch exists but no worktree does: the leftover a discarded session
    // leaves behind. A plain add would attach it at its old tip.
    let leftover_branch =
        (use_worktree && branch_exists && existing.is_none()).then_some(branch_name);

    let fork = spawn_plan::fork_point(
        &repo_path,
        Some(&resolved_base),
        Some(&resolved_base),
        existing,
        leftover_branch,
    )
    .await;
    Ok(protocol::MemberSpawnPreview {
        repo_id: repo.id.clone(),
        repo_name: repo.name.clone(),
        branch_exists,
        effective_base,
        worktree_path: worktree_path.to_string_lossy().into_owned(),
        resolved_base_ref: Some(resolved_base),
        base_remote_ref: fork.base_remote_ref,
        base_behind_remote: fork.base_behind_remote,
        worktree_exists: existing.is_some(),
        existing_worktree_head: fork.existing_worktree_head,
        existing_worktree_dirty: fork.existing_worktree_dirty,
        existing_worktree_behind_base: fork.existing_worktree_behind_base,
        existing_branch_head: fork.existing_branch_head,
        existing_branch_behind_base: fork.existing_branch_behind_base,
    })
}

/// Where a single-repo session of `repo` on `branch_name` puts its worktree:
/// the worktree under `worktrees_root` that already holds the branch, else
/// the repo's group folder `wt.<branch>/<repo-name>`. Errors when that folder
/// exists but belongs to something other than this repo.
async fn single_worktree_target(
    worktrees_root: &Path,
    repo: &protocol::RepoEntry,
    branch_name: &str,
) -> anyhow::Result<PathBuf> {
    let repo_path = Path::new(&repo.path);
    let derived = crate::git::single_worktree_path(worktrees_root, branch_name, &repo.name);
    let target =
        crate::git::existing_or_derived_worktree(repo_path, worktrees_root, branch_name, derived)
            .await;
    crate::git::refuse_foreign_worktree_folder(repo_path, &repo.name, &target).await?;
    Ok(target)
}

/// Measure the leftover sitting where a spawn wanted to go, the same way the
/// spawn dialog's preview measures it, and wrap it in the refusal the client
/// renders as a blocking modal.
///
/// A leftover directory is described through its own HEAD; a branch with no
/// directory through the branch tip. Nothing is mutated — the caller returns
/// this instead of creating anything.
async fn leftover_refusal_for(
    repo_name: &str,
    repo_path: &Path,
    worktree_path: &Path,
    branch_name: &str,
    base: &str,
) -> anyhow::Error {
    let existing = worktree_path.exists().then_some(worktree_path);
    let leftover_branch = existing.is_none().then_some(branch_name);
    let fork =
        spawn_plan::fork_point(repo_path, Some(base), Some(base), existing, leftover_branch).await;
    warn!(
        branch_name,
        base,
        worktree = %worktree_path.display(),
        "refusing to spawn onto a leftover branch or worktree"
    );
    anyhow::Error::new(spawn_plan::leftover_refusal(
        repo_name,
        branch_name,
        base,
        &fork,
        worktree_path,
    ))
}

/// Create or bind the single-repo worktree at `worktree_path`, whatever
/// state the target is in: a directory already present (delegated to
/// [`recreate_or_reuse_worktree`]), a branch-only leftover from a discarded
/// session, or nothing at all (a plain add from `base`).
///
/// The branch-only case is the stale-pin trap: a `-b` add against the
/// leftover branch fails on "already exists", while attaching it silently
/// pins the session to the old fork point. Reuse attaches it at its old tip
/// deliberately (the collision prompt showed the user that tip); recreate
/// deletes the branch and forks fresh from `base`; refuse raises a
/// [`spawn_plan::leftover_refusal`] before anything is created, because the
/// caller picked the name automatically and nobody was asked about it.
async fn materialize_single_worktree(
    repo_name: &str,
    repo_path: &Path,
    worktree_path: &Path,
    branch_name: &str,
    base: &str,
    policy: protocol::WorktreeReusePolicy,
) -> anyhow::Result<()> {
    if worktree_path.exists() {
        return recreate_or_reuse_worktree(
            repo_name,
            repo_path,
            worktree_path,
            branch_name,
            base,
            policy,
        )
        .await;
    }
    let branch_exists = crate::git::list_branches(repo_path)
        .await
        .unwrap_or_default()
        .iter()
        .any(|b| b == branch_name);
    let create_from = if branch_exists {
        match policy {
            protocol::WorktreeReusePolicy::Reuse | protocol::WorktreeReusePolicy::Unknown => {
                info!(
                    branch_name,
                    "attaching pre-existing branch at its current tip; base branch not applied"
                );
                None
            }
            protocol::WorktreeReusePolicy::RecreateFromBase => {
                warn!(
                    branch_name,
                    base, "deleting leftover branch and forking fresh from base"
                );
                if let Err(err) = crate::git::delete_branch(repo_path, branch_name).await {
                    debug!(?err, branch_name, "leftover branch delete failed");
                }
                Some(base)
            }
            protocol::WorktreeReusePolicy::RefuseLeftover => {
                return Err(leftover_refusal_for(
                    repo_name,
                    repo_path,
                    worktree_path,
                    branch_name,
                    base,
                )
                .await);
            }
        }
    } else {
        Some(base)
    };
    crate::git::worktree_add(repo_path, worktree_path, branch_name, create_from)
        .await
        .map_err(|e| anyhow::Error::new(classify_worktree_error(&e, branch_name)))
}

/// Apply `policy` to a worktree directory that already exists at
/// `worktree_path`.
///
/// `Reuse` (and `Unknown`, which decodes from a client newer than this
/// daemon) binds to it untouched — the historical behavior. Recreating drops
/// the worktree and re-adds it from `base`, which discards anything
/// uncommitted; the client only offers that after showing the user the
/// worktree's dirty state. Refusing raises before either happens.
async fn recreate_or_reuse_worktree(
    repo_name: &str,
    repo_path: &Path,
    worktree_path: &Path,
    branch_name: &str,
    base: &str,
    policy: protocol::WorktreeReusePolicy,
) -> anyhow::Result<()> {
    match policy {
        protocol::WorktreeReusePolicy::RefuseLeftover => {
            Err(leftover_refusal_for(repo_name, repo_path, worktree_path, branch_name, base).await)
        }
        protocol::WorktreeReusePolicy::Reuse | protocol::WorktreeReusePolicy::Unknown => {
            info!(
                worktree = %worktree_path.display(),
                branch_name,
                "reusing existing worktree; base branch not applied"
            );
            Ok(())
        }
        protocol::WorktreeReusePolicy::RecreateFromBase => {
            warn!(
                worktree = %worktree_path.display(),
                branch_name,
                base,
                "recreating existing worktree from base; uncommitted work there is discarded"
            );
            crate::git::worktree_remove(repo_path, worktree_path)
                .await
                .with_context(|| {
                    format!(
                        "removing existing worktree at {} before recreating it",
                        worktree_path.display()
                    )
                })?;
            // The branch itself survives `worktree remove`, so re-adding with
            // `-b` would fail on "branch already exists". Delete it too — the
            // user asked for a fresh fork from `base`, not a rebind to the old
            // branch tip.
            if let Err(err) = crate::git::delete_branch(repo_path, branch_name).await {
                debug!(
                    ?err,
                    branch_name, "no local branch to delete before recreate"
                );
            }
            crate::git::worktree_add(repo_path, worktree_path, branch_name, Some(base))
                .await
                .map_err(|e| anyhow::Error::new(classify_worktree_error(&e, branch_name)))
        }
    }
}

/// Record where a worktree actually ended up relative to its base. Cheap
/// local git calls, and the one line in the log that answers "why is this
/// branch missing work that landed weeks ago".
async fn log_fork_point(repo_path: &Path, branch_name: &str, base: &str, worktree_path: &Path) {
    let fork =
        spawn_plan::fork_point(repo_path, Some(base), Some(base), Some(worktree_path), None).await;
    info!(
        branch_name,
        base,
        head = fork.existing_worktree_head.as_deref().unwrap_or("?"),
        behind_base = fork.existing_worktree_behind_base.unwrap_or(0),
        base_behind_remote = fork.base_behind_remote.unwrap_or(0),
        "worktree fork point"
    );
}

/// Everything a single-repo spawn needs beyond the repo id. Grouped into a
/// struct rather than passed positionally — the list crossed the readable
/// arity a while back, and the two worktree-shaped fields
/// (`worktree_reuse`, `existing_worktree`) are easy to transpose at a call
/// site when they're bare positions.
struct SinglePlan<'a> {
    branch_name: &'a str,
    base_branch: Option<String>,
    use_worktree: bool,
    checkout_strategy: Option<protocol::CheckoutStrategy>,
    worktree_reuse: protocol::WorktreeReusePolicy,
    existing_worktree: Option<String>,
}

/// A pin names a directory to run in; in-place mode runs in the repo itself.
/// Asking for both is contradictory, and honoring either one silently would
/// put the session somewhere the user didn't choose.
fn reject_pin_without_worktree(target: &SpawnTarget) -> anyhow::Result<()> {
    let pinned = match target {
        SpawnTarget::Single {
            use_worktree,
            existing_worktree,
            ..
        } => !*use_worktree && existing_worktree.is_some(),
        SpawnTarget::Workspace {
            use_worktree,
            existing_worktrees,
            ..
        } => !*use_worktree && !existing_worktrees.is_empty(),
        SpawnTarget::Standalone { .. } => false,
    };
    if pinned {
        return Err(anyhow!(
            "cannot pin an existing worktree on an in-place spawn: pass use_worktree = true or drop the pin"
        ));
    }
    Ok(())
}

/// Branch actually checked out in `worktree`, falling back to `requested` for
/// a detached HEAD or an unreadable tree. Pinned spawns record this rather
/// than the requested name so the sidebar shows the branch the agent will
/// really be working on.
async fn pinned_branch(worktree: &Path, requested: &str) -> String {
    match crate::git::current_branch(worktree).await {
        Ok(Some(branch)) => branch,
        Ok(None) => {
            info!(
                worktree = %worktree.display(),
                requested, "pinned worktree is on a detached HEAD; labelling with the requested branch"
            );
            requested.to_string()
        }
        Err(err) => {
            warn!(
                ?err,
                worktree = %worktree.display(),
                "reading pinned worktree HEAD failed; labelling with the requested branch"
            );
            requested.to_string()
        }
    }
}

async fn spawn_single(
    hub: &Hub,
    repo_id: &str,
    plan: SinglePlan<'_>,
) -> anyhow::Result<SpawnResolution> {
    let SinglePlan {
        branch_name,
        base_branch,
        use_worktree,
        checkout_strategy,
        worktree_reuse,
        existing_worktree,
    } = plan;
    let repo = hub
        .state
        .with_persisted(|s| s.repos.iter().find(|r| r.id == repo_id).cloned())
        .ok_or_else(|| anyhow!("unknown repo: {repo_id}"))?;
    let repo_path = PathBuf::from(&repo.path);

    // Non-git folder (no `.git`, or `git init` with no commits): skip
    // worktree/branch ops entirely and spawn directly in the folder. The
    // caller-supplied `branch_name` is still recorded on the member so the
    // session label and sidebar grouping stay coherent, but no branch is
    // created or checked out.
    if !crate::git::is_initialized(&repo_path).await {
        if let Some(pinned) = existing_worktree {
            return Err(anyhow!(
                "cannot pin worktree {pinned}: {} is not a git repository",
                repo.name
            ));
        }
        let member = SessionMember {
            repo_id: repo.id.clone(),
            repo_name: repo.name.clone(),
            branch: branch_name.to_string(),
            worktree_path: repo_path.to_string_lossy().into_owned(),
        };
        let label = format!("{}: {branch_name}", repo.name);
        return Ok((SessionKind::Single, vec![member], repo_path, label, None));
    }

    // Base branch used when the target needs to be *created*. See
    // `spawn_plan::resolve_base_for_create` for the priority chain and why an
    // auto-detected default is upgraded to its remote-tracking counterpart.
    let base_for_create =
        spawn_plan::resolve_base_for_create(&hub.state, &repo, base_branch.as_deref()).await;

    let (working_path, member_branch) = if let Some(pinned) = existing_worktree {
        let worktree_path = crate::paths::resolve_existing_dir(&pinned)
            .with_context(|| format!("pinned worktree for {}", repo.name))?;
        // Deliberately no in-use refusal here. The client resolves a pin from
        // a picker that shows which worktrees a live session already holds and
        // confirms before sending one, so the pin *is* the acknowledgement.
        let in_use = active_sessions_using_worktree(hub, &worktree_path.to_string_lossy(), None);
        if !in_use.is_empty() {
            let labels: Vec<&str> = in_use.iter().map(|(_, label)| label.as_str()).collect();
            warn!(
                worktree = %worktree_path.display(),
                sharing_with = ?labels,
                "spawning into a worktree a live session already holds"
            );
        }
        let branch = pinned_branch(&worktree_path, branch_name).await;
        (worktree_path, branch)
    } else if use_worktree {
        let worktree_path =
            single_worktree_target(&hub.state.worktrees_dir(), &repo, branch_name).await?;
        // Refuse to bind a worktree that's already driving a live session.
        // Without this the reuse path below would silently share one worktree
        // across two sessions.
        let in_use = active_sessions_using_worktree(hub, &worktree_path.to_string_lossy(), None);
        if !in_use.is_empty() {
            return Err(worktree_in_use_failure(branch_name, &in_use).into());
        }
        materialize_single_worktree(
            &repo.name,
            &repo_path,
            &worktree_path,
            branch_name,
            &base_for_create,
            worktree_reuse,
        )
        .await?;
        crate::worktrees_admin::mark_group(
            &crate::git::group_dir(&hub.state.worktrees_dir(), branch_name, &repo.name),
            crate::git::GroupKind::Repo,
            &repo.name,
            &[worktree_path.as_path()],
        );
        log_fork_point(&repo_path, branch_name, &base_for_create, &worktree_path).await;
        (worktree_path, branch_name.to_string())
    } else {
        crate::git::checkout_in_place(
            &repo_path,
            branch_name,
            Some(&base_for_create),
            checkout_strategy,
        )
        .await
        .context("checking out branch in-place")?;
        (repo_path.clone(), branch_name.to_string())
    };

    let member = SessionMember {
        repo_id: repo.id.clone(),
        repo_name: repo.name.clone(),
        branch: member_branch.clone(),
        worktree_path: working_path.to_string_lossy().into_owned(),
    };
    let label = format!("{}: {member_branch}", repo.name);
    Ok((SessionKind::Single, vec![member], working_path, label, None))
}

fn spawn_standalone_shell(cwd: Option<&str>) -> anyhow::Result<SpawnResolution> {
    let cwd = resolve_standalone_cwd(cwd)?;
    let label = format!("Shell: {}", path_leaf_label(&cwd));
    Ok((SessionKind::Standalone, Vec::new(), cwd, label, None))
}

/// Whether a standalone (no repo) target can run `mode` for `agent`: a plain
/// shell, or any agent interactively in the folder, with no worktree.
/// Headless sessions still need a repo or workspace.
fn standalone_supports(mode: SessionMode, agent: Agent) -> bool {
    match mode {
        SessionMode::PlainShell => true,
        SessionMode::Interactive => matches!(agent, Agent::Claude),
        SessionMode::Headless => false,
    }
}

/// An agent running in a plain folder, labelled with the folder's name.
fn spawn_standalone_agent(cwd: Option<&str>) -> anyhow::Result<SpawnResolution> {
    let cwd = resolve_standalone_cwd(cwd)?;
    let label = path_leaf_label(&cwd);
    Ok((SessionKind::Standalone, Vec::new(), cwd, label, None))
}

fn resolve_standalone_cwd(cwd: Option<&str>) -> anyhow::Result<PathBuf> {
    let path = match cwd.map(str::trim).filter(|s| !s.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => default_standalone_cwd()?,
    };
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("reading the standalone folder: {}", path.display()))?;
    if !metadata.is_dir() {
        return Err(anyhow!(
            "standalone path is not a folder: {}",
            path.display()
        ));
    }
    let canonical = std::fs::canonicalize(&path).unwrap_or(path);
    Ok(crate::paths::simplify_path(&canonical))
}

fn default_standalone_cwd() -> anyhow::Result<PathBuf> {
    if let Some(user_dirs) = UserDirs::new() {
        return Ok(user_dirs.home_dir().to_path_buf());
    }
    std::env::current_dir().context("resolving fallback standalone shell directory")
}

fn path_leaf_label(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map_or_else(|| path.display().to_string(), str::to_string)
}

async fn spawn_workspace(
    hub: &Hub,
    workspace_id: &str,
    req: ws::ResolveRequest<'_>,
    worktree_reuse: protocol::WorktreeReusePolicy,
) -> anyhow::Result<SpawnResolution> {
    let branch_name = req.branch_name;
    let use_worktree = req.use_worktree;
    info!(
        workspace_id,
        branch_name, use_worktree, "spawn_workspace: begin"
    );
    let (workspace, resolved) =
        ws::resolve_workspace(&hub.state, &hub.state.worktrees_dir(), workspace_id, req).await?;
    // Refuse to bind any member worktree that's already driving a live
    // session — mirrors the single-repo guard so a workspace re-spawn on a
    // branch whose worktrees already exist can't be shared across sessions.
    // Pinned members are exempt: the client resolves a pin from a picker that
    // shows which worktrees are live and confirms before sending one.
    if use_worktree {
        for member in &resolved {
            let in_use =
                active_sessions_using_worktree(hub, &member.working_path.to_string_lossy(), None);
            if in_use.is_empty() {
                continue;
            }
            if !member.pinned {
                return Err(worktree_in_use_failure(branch_name, &in_use).into());
            }
            let labels: Vec<&str> = in_use.iter().map(|(_, label)| label.as_str()).collect();
            warn!(
                worktree = %member.working_path.display(),
                sharing_with = ?labels,
                "spawning into a worktree a live session already holds"
            );
        }
    }
    ws::ensure_branches(&resolved, branch_name, worktree_reuse)
        .await
        .map_err(|e| anyhow::Error::new(classify_worktree_error(&e, branch_name)))?;
    if use_worktree {
        let created: Vec<&Path> = resolved
            .iter()
            .filter(|m| !m.pinned)
            .map(|m| m.working_path.as_path())
            .collect();
        crate::worktrees_admin::mark_group(
            &crate::git::group_dir(&hub.state.worktrees_dir(), branch_name, &workspace.name),
            crate::git::GroupKind::Workspace,
            &workspace.name,
            &created,
        );
        for member in &resolved {
            if member.pinned {
                continue;
            }
            let repo_path = PathBuf::from(&member.repo.path);
            let base = member.effective_base.clone().unwrap_or_default();
            log_fork_point(&repo_path, branch_name, &base, &member.working_path).await;
        }
    }

    let primary = resolved
        .first()
        .ok_or_else(|| anyhow!("workspace has no members"))?
        .working_path
        .clone();
    let mut members: Vec<SessionMember> = Vec::with_capacity(resolved.len());
    for member in &resolved {
        // A pinned member runs on whatever its worktree already has checked
        // out, which need not be the branch the group's directory is named for.
        let branch = if member.pinned {
            pinned_branch(&member.working_path, branch_name).await
        } else {
            branch_name.to_string()
        };
        members.push(SessionMember {
            repo_id: member.repo.id.clone(),
            repo_name: member.repo.name.clone(),
            branch,
            worktree_path: member.working_path.to_string_lossy().into_owned(),
        });
    }
    let label = format!("{}: {branch_name}", workspace.name);
    Ok((
        SessionKind::Workspace,
        members,
        primary,
        label,
        Some(workspace.id.clone()),
    ))
}

/// Start the tasks that watch the hub for the daemon's lifetime.
fn start_hub_tasks(hub: &Hub, client_count_rx: tokio::sync::watch::Receiver<usize>) {
    start_history_broadcaster(hub);
    // Idle self-exit: once the last client disconnects and no session has a
    // live child for a full grace period, flip the shutdown watch so a
    // resident daemon doesn't pin a stale executable across app updates.
    // Mirrors the HTTP /shutdown path (no drain): sidecars stay on disk, and
    // abandoned sessions come back from them on the next daemon start.
    crate::idle_exit::spawn(
        Arc::clone(&hub.sessions),
        crate::idle_exit::Watches::new(client_count_rx, hub.remote_enabled.subscribe()),
        hub.shutdown_tx.clone(),
        Arc::clone(&hub.preset_cancellations),
        crate::idle_exit::GRACE_PERIOD,
    );
}

/// Kill the session's process if still alive, then mark it `is_inactive =
/// true` in the registry so the sidebar shows it as parked. The session
/// record stays in the registry — the user can Resume it later.
async fn park_session(hub: &Hub, session_id: &str) {
    if hub.sessions.get(session_id).is_none() {
        return;
    }
    // Recorded before the kill, so this wins over the exit watcher's later
    // `Exited` for the same session.
    record_end(hub, session_id, SessionEnd::StoppedByUser);
    // The handles are taken in the same update that marks the session
    // parked, before the kill, as `stop_session` does: the exit watcher then
    // either ran first (and took the pty itself) or finds it released, and
    // never reads a tracer lost during the kill as an unexpected loss.
    let mut pty = None;
    let mut headless_handle = None;
    hub.sessions.update(session_id, |r| {
        r.is_inactive = true;
        pty = r.pty.take();
        headless_handle = r.headless.take();
        r.input_notifier = None;
        if !matches!(
            r.status,
            protocol::SessionStatus::Stopped | protocol::SessionStatus::Error
        ) {
            r.status = protocol::SessionStatus::Stopped;
        }
        push_recent_action(r, "parked — worktree retained".to_string());
    });
    if let Some(pty) = pty {
        pty.kill();
    }
    if let Some(h) = headless_handle {
        h.kill().await;
    }
    // Auto-close panes bound to this session in every client's layout; the
    // session record itself lives on in the registry for the Resume button.
    close_session_panes(hub, session_id);
}

/// Remove a stopped or inactive session from the registry. Optionally removes
/// per-member worktrees from disk. Does not kill a running session.
///
/// On cleanup failure (member dir still on disk after git remove + retry +
/// fs fallback), emits [`DaemonMessage::WorktreeCleanupFailed`] back to the
/// requesting client with the list of processes holding the dir open so the
/// user can decide whether to kill them and retry.
/// Snapshot the discard-relevant state of a session record: its members,
/// whether it owns per-session worktrees, and its label for failure
/// messages. Missing sessions fall back to empty/id defaults so a repeated
/// discard stays a no-op instead of an error.
fn discard_snapshot(hub: &Hub, session_id: &str) -> (Vec<SessionMember>, bool, String) {
    hub.sessions.get(session_id).map_or_else(
        || (Vec::new(), false, session_id.to_string()),
        |rec| {
            let guard = crate::sync::lock(&rec);
            let spawned_with_worktree =
                guard
                    .spawn_config
                    .as_ref()
                    .is_some_and(|cfg| match &cfg.target {
                        SpawnTarget::Single { use_worktree, .. }
                        | SpawnTarget::Workspace { use_worktree, .. } => *use_worktree,
                        SpawnTarget::Standalone { .. } => false,
                    });
            (
                guard.members.clone(),
                spawned_with_worktree || !guard.worktree_paths.is_empty(),
                guard.label.clone(),
            )
        },
    )
}

/// Reap the branch a removed session worktree leaves behind, per the
/// caller's [`BranchCleanup`] choice.
///
/// `git worktree remove` never deletes the branch it had checked out, and
/// left alone that branch pins a future spawn of the same name to this
/// session's old fork point. `Keep` leaves it regardless; `Delete` removes it
/// unconditionally; `Auto` (and any mode this build does not know) deletes it
/// only when its commits already exist in one of `targets`, by ancestry or by
/// patch equivalence. Failure is logged, never raised — the worktree the
/// user asked to delete is already gone.
async fn reap_discarded_session_branch(
    repo_path: &str,
    branch: &str,
    mode: BranchCleanup,
    targets: &[String],
) {
    match mode {
        BranchCleanup::Keep => {
            info!(
                branch,
                repo = %repo_path,
                "discard_session: keeping the session branch at the user's request"
            );
            return;
        }
        BranchCleanup::Delete => {
            force_delete_session_branch(repo_path, branch, "at the user's request").await;
            return;
        }
        BranchCleanup::Auto | BranchCleanup::Unknown => {}
    }

    match crate::git::branch_merge_status(Path::new(repo_path), branch, targets).await {
        Ok(crate::git::BranchMergeStatus::Merged { into, via }) => {
            let reason = format!("already in {into} ({via:?})");
            force_delete_session_branch(repo_path, branch, &reason).await;
        }
        Ok(crate::git::BranchMergeStatus::Unmerged { unique_commits }) => info!(
            branch,
            repo = %repo_path,
            unique_commits,
            ?targets,
            "discard_session: kept the session branch — it has unmerged commits"
        ),
        Err(err) => warn!(
            ?err,
            branch,
            repo = %repo_path,
            ?targets,
            "discard_session: merge check failed; branch left in place"
        ),
    }
}

/// `git branch -D` with the outcome logged either way. `why` is folded into
/// the success line so the log says what licensed the delete.
async fn force_delete_session_branch(repo_path: &str, branch: &str, why: &str) {
    match crate::git::delete_branch(Path::new(repo_path), branch).await {
        Ok(()) => info!(
            branch,
            repo = %repo_path,
            why,
            "discard_session: deleted the session branch"
        ),
        Err(err) => warn!(
            ?err,
            branch,
            repo = %repo_path,
            "discard_session: session branch delete failed; branch left in place"
        ),
    }
}

/// Answer [`ClientMessage::PreviewDiscard`]: what would happen to each
/// member's branch if the user confirmed the discard right now. Read-only.
/// An unknown session, or one with no per-session worktrees, yields an empty
/// member list — the same no-op stance a repeat discard takes.
async fn preview_discard(
    hub: &Hub,
    session_id: &str,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) {
    let (members, has_per_session_worktree, _) = discard_snapshot(hub, session_id);
    let SessionBase::Repo(explicit_base) = session_spawn_base(hub, session_id) else {
        info!(
            session_id,
            "preview_discard: standalone session has no branches"
        );
        let _ = out_tx.send(DaemonMessage::DiscardPreview {
            session_id: session_id.to_string(),
            members: Vec::new(),
        });
        return;
    };
    if members.is_empty() {
        info!(session_id, "preview_discard: no members to preview");
    }

    let worktrees_root = hub.state.worktrees_dir();
    let mut fates: Vec<MemberBranchFate> = Vec::new();
    for member in &members {
        let fate = if has_per_session_worktree {
            member_fate(hub, member, explicit_base.as_deref(), &worktrees_root).await
        } else {
            // An in-place session runs in the repo itself; its branch is not
            // the daemon's to reap.
            protocol::BranchFate::Untouched {
                reason: protocol::UntouchedReason::ExternalWorktree,
            }
        };
        fates.push(MemberBranchFate {
            repo_id: member.repo_id.clone(),
            repo_name: member.repo_name.clone(),
            branch: member.branch.clone(),
            fate,
        });
    }

    let _ = out_tx.send(DaemonMessage::DiscardPreview {
        session_id: session_id.to_string(),
        members: fates,
    });
}

/// Resolve one member's targets and run the fate rules against them.
async fn member_fate(
    hub: &Hub,
    member: &SessionMember,
    explicit_base: Option<&str>,
    worktrees_root: &Path,
) -> protocol::BranchFate {
    let Some(repo) = hub
        .state
        .with_persisted(|s| s.repos.iter().find(|r| r.id == member.repo_id).cloned())
    else {
        return protocol::BranchFate::Untouched {
            reason: protocol::UntouchedReason::ExternalWorktree,
        };
    };
    let targets = branch_fate::merge_targets(&hub.state, &repo, explicit_base).await;
    branch_fate::member_branch_fate(FateInput {
        repo_path: Path::new(&repo.path),
        member_worktree: Path::new(&member.worktree_path),
        worktrees_root,
        branch: &member.branch,
        targets: &targets,
    })
    .await
}

/// The base branch a session's members forked from, per its stored spawn
/// config. A session that is already gone reads as repo-backed with no
/// explicit base, which re-derives the repo default rather than guessing.
fn session_spawn_base(hub: &Hub, session_id: &str) -> SessionBase {
    hub.sessions
        .get(session_id)
        .map_or(SessionBase::Repo(None), |rec| {
            branch_fate::session_base(crate::sync::lock(&rec).spawn_config.as_ref())
        })
}

/// Write a session's spawn-time sidecar and mirror the record into it (see
/// [`SessionRegistry::write_sidecar`]).
fn write_meta_then_sync(hub: &Hub, meta: &orphan::OrphanMeta) {
    hub.sessions.write_sidecar(meta);
}

async fn discard_session(
    hub: &Hub,
    session_id: &str,
    cleanup: &[protocol::CleanupAction],
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
) {
    // A repeated discard reaches here after the record is gone — the client's
    // pane close following its own auto-discard for a self-exited session.
    // Every step below is a no-op for an unknown id at best (the history end,
    // the registry removal) and a needless write at worst: the layout sweep
    // rewrites state.json, and the sidecar deletes would take a session dir
    // the registry no longer vouches for with them.
    if hub.sessions.get(session_id).is_none() {
        debug!(%session_id, "discard_session for unknown session; ignoring");
        return;
    }
    // Discard drops the registry record and can delete worktrees from disk —
    // the most destructive thing a client can ask for. It left no trail at
    // all, which made "the session vanished" impossible to attribute from a
    // log alone.
    info!(
        %session_id,
        cleanup_targets = cleanup.len(),
        remove_worktrees = cleanup.iter().filter(|c| c.remove_worktree).count(),
        "discard_session: begin"
    );
    let (members, has_per_session_worktree, session_label) = discard_snapshot(hub, session_id);
    let explicit_base = match session_spawn_base(hub, session_id) {
        SessionBase::Repo(base) => base,
        SessionBase::Standalone => None,
    };

    // Refuse the whole discard if any worktree it would remove is still in
    // use by another live session. The user must close the other session(s)
    // first, or discard while keeping the worktree.
    if has_per_session_worktree
        && let Some(blocked) = blocked_discard_message(hub, session_id, cleanup, &members)
    {
        let _ = out_tx.send(blocked);
        return;
    }

    // Brief grace period before touching the filesystem. The typical
    // call sequence is stop_session → discard_session back-to-back, and
    // stop_session's pty.kill() is fire-and-forget — the actual
    // TerminateProcess hop runs through the tracer pipe asynchronously.
    // Without this pause, `git worktree remove --force` races the still-
    // dying child's file handles and fails on Windows. 250 ms covers
    // the typical kill propagation; the fs fallback in
    // `worktree_cleanup::remove_member` handles the rest.
    let wants_removal = cleanup
        .iter()
        .any(|a| a.remove_worktree && has_per_session_worktree);
    if wants_removal {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    // Track each removed member's immediate parent so we can prune the
    // now-empty anchor skeleton + wt.<branch-slug>/ wrapper once all
    // members of the group have been processed. A `HashSet` would dedupe
    // in O(1), but the typical N is 1–3 members so a Vec scan is cheaper
    // and keeps allocations bounded.
    let worktrees_root = hub.state.worktrees_dir();
    let mut member_parents: Vec<PathBuf> = Vec::new();
    let mut failures: Vec<protocol::WorktreeCleanupFailure> = Vec::new();
    for action in cleanup {
        if !action.remove_worktree || !has_per_session_worktree {
            continue;
        }
        let Some(member) = members.iter().find(|m| m.repo_id == action.repo_id) else {
            continue;
        };
        let repo = hub
            .state
            .with_persisted(|s| s.repos.iter().find(|r| r.id == member.repo_id).cloned());
        let Some(repo) = repo else {
            continue;
        };
        let repo_path = repo.path.clone();
        // Resolved while the session record is still intact, and only for the
        // mode that consults them.
        let targets = if matches!(action.branch, BranchCleanup::Auto | BranchCleanup::Unknown) {
            branch_fate::merge_targets(&hub.state, &repo, explicit_base.as_deref()).await
        } else {
            Vec::new()
        };
        let worktree_path = PathBuf::from(&member.worktree_path);
        match crate::worktree_cleanup::remove_member(Path::new(&repo_path), &worktree_path).await {
            crate::worktree_cleanup::CleanupOutcome::Removed => {
                // Gated to worktrees under the daemon's own root so a pinned
                // external worktree never loses its branch.
                if worktree_path.starts_with(&worktrees_root) {
                    reap_discarded_session_branch(
                        &repo_path,
                        &member.branch,
                        action.branch,
                        &targets,
                    )
                    .await;
                }
            }
            crate::worktree_cleanup::CleanupOutcome::StillOnDisk { reason } => {
                warn!(
                    %reason,
                    member = %worktree_path.display(),
                    "discard_session: worktree cleanup failed; member dir still on disk"
                );
                let locking_processes = probe_locking_processes(&worktree_path);
                failures.push(protocol::WorktreeCleanupFailure {
                    member_path: worktree_path.to_string_lossy().into_owned(),
                    repo_path: repo_path.clone(),
                    reason,
                    locking_processes,
                });
            }
        }
        if let Some(parent) = worktree_path.parent()
            && !member_parents.iter().any(|p| p == parent)
        {
            member_parents.push(parent.to_path_buf());
        }
    }
    for parent in &member_parents {
        crate::worktree_cleanup::prune_empty_ancestors(parent, &worktrees_root);
    }
    record_end(hub, session_id, SessionEnd::StoppedByUser);
    hub.sessions.delete_sidecar(session_id);
    orphan::try_delete_session_dir(&hub.dirs, session_id);
    hub.sessions.remove(session_id);
    close_session_panes(hub, session_id);

    if !failures.is_empty() {
        let _ = out_tx.send(DaemonMessage::WorktreeCleanupFailed {
            session_id: session_id.to_string(),
            session_label,
            failures,
        });
    }
}

async fn stop_session(hub: &Hub, session_id: &str) -> anyhow::Result<()> {
    if hub.sessions.get(session_id).is_none() {
        return Err(anyhow!("unknown session: {session_id}"));
    }
    // Recorded before the kill, so this wins over the exit watcher's later
    // `Exited` for the same session.
    record_end(hub, session_id, SessionEnd::StoppedByUser);
    // Mark the session stopped BEFORE killing the child.
    //
    // The child's own exit watcher (`session::attach_lifecycle`) also marks it
    // stopped, but *with* an exit code — and a client reads a
    // running-to-stopped transition carrying an exit code as a self-exit,
    // which for a worktree-less session means auto-discard. Killing first
    // leaves a window where the exit watcher's snapshot is the first
    // "stopped" the client ever sees, making a deliberate Stop
    // indistinguishable from a crash and silently discarding a session the
    // user only asked to stop. Claiming the transition here closes it: the
    // watcher's later update finds the status already stopped.
    //
    // The handles are taken in this same update, so the exit watcher either
    // ran first (and took the pty itself) or finds it released: a tracer lost
    // during the kill never reads as an unexpected loss.
    let mut pty = None;
    let mut headless_handle = None;
    hub.sessions.update(session_id, |r| {
        r.status = protocol::SessionStatus::Stopped;
        pty = r.pty.take();
        headless_handle = r.headless.take();
        r.input_notifier = None;
        r.is_inactive = false;
        push_recent_action(r, "stopped by user".to_string());
    });
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
    hub.sessions.delete_sidecar(session_id);
    remove_hook_settings(hub, session_id);
    Ok(())
}

/// Resolve an agent-program name into something `CreateProcess` /
/// portable-pty can actually launch. Returns `(program, prepended_args)`.
///
/// On Windows, agent CLIs distributed via npm (e.g. claude from
/// `@anthropic-ai/claude-code`) land on
/// PATH as `.cmd` shims. Two layers of fixup matter here:
///
/// 1. `CreateProcess` does NOT execute `.cmd` or `.bat` directly, so
///    a raw `.cmd` path passed to portable-pty silently fails (the
///    tracer's pipe never opens; daemon times out after 10s).
/// 2. Even wrapping with `cmd.exe /d /c <shim>.cmd` is enough for the
///    process to launch — but inside portable-pty's pseudo-console
///    that chain (cmd.exe → npm shim → node.exe → real binary) ends
///    up swallowing the child's stdout: the agent never renders, and
///    flags like `--dangerously-skip-permissions` look like they're
///    being ignored because the user sees no output at all. The same
///    `cmd.exe /d /c <shim>.cmd …` argv works fine in a real Windows
///    console and from `PowerShell`, so it's specific to the
///    portable-pty + cmd.exe boundary.
///
/// We dodge both by, when the `.cmd` is an npm-style shim, peeking
/// into the sibling `node_modules/.../bin/` directory and spawning
/// either the native `<stem>.exe` (newer claude-code distribution) or
/// `node.exe <stem>.js` (older claude) directly. That removes
/// cmd.exe from the chain entirely.
///
/// Order of preference for a `.cmd` shim:
///   1. native binary → `(<bin>/<stem>.exe, [...])`.
///   2. JS entrypoint → `(node.exe, [<bin>/<stem>.js, ...])`.
///   3. Fallback     → `(cmd.exe, ["/d", "/c", <shim-path>, ...])`.
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
            && let Some((program, prepend)) = npm_shim_to_native_program(&path)
        {
            return (program.to_string_lossy().into_owned(), prepend);
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
/// the underlying program the shim ultimately invokes. Two layouts
/// are supported, in this order of preference:
///
/// - Native binary distribution (`@anthropic-ai/claude-code` since
///   ~2026): `<dir>/node_modules/<pkg>/bin/<stem>.exe`. Returned as
///   `(<exe>, [])` — no node prefix.
/// - JS entrypoint (older claude): `<dir>/node_modules/<pkg>/
///   bin/<stem>.js`. Returned as `(node.exe, [<js>])`. The `node.exe`
///   resolution prefers a sibling next to the shim (nvm-style install
///   layout) over a PATH-resolved one, so node-version managers stay
///   self-consistent.
///
/// `<pkg>` may be scoped as `@org/name`.
///
/// Returns `None` if the shim doesn't match either pattern (custom
/// wrapper `.cmd`, non-npm CLI, etc.) — caller falls back to
/// `cmd.exe /d /c <shim>`.
#[cfg(windows)]
fn npm_shim_to_native_program(shim: &Path) -> Option<(PathBuf, Vec<String>)> {
    let dir = shim.parent()?;
    let stem = shim.file_stem()?.to_str()?;
    let nm = dir.join("node_modules");
    if !nm.is_dir() {
        return None;
    }

    let exe_filename = format!("{stem}.exe");
    let js_filename = format!("{stem}.js");

    let mut exe_candidates: Vec<PathBuf> = Vec::new();
    let mut js_candidates: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&nm).ok()?.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_str()?;
        if name_str.starts_with('@') {
            // Scoped: walk one more level (e.g. node_modules/@anthropic-ai/claude-code/bin/claude.js).
            if let Ok(inner) = std::fs::read_dir(&p) {
                for sub in inner.flatten() {
                    let bin = sub.path().join("bin");
                    exe_candidates.push(bin.join(&exe_filename));
                    js_candidates.push(bin.join(&js_filename));
                }
            }
        } else {
            // Unscoped: node_modules/<pkg>/bin/<stem>.{exe,js}.
            let bin = p.join("bin");
            exe_candidates.push(bin.join(&exe_filename));
            js_candidates.push(bin.join(&js_filename));
        }
    }

    if let Some(exe) = exe_candidates.into_iter().find(|c| c.is_file()) {
        return Some((exe, Vec::new()));
    }

    let js = js_candidates.into_iter().find(|c| c.is_file())?;
    let sibling_node = dir.join("node.exe");
    let node = if sibling_node.is_file() {
        sibling_node
    } else {
        which::which("node").ok()?
    };
    Some((node, vec![js.to_string_lossy().into_owned()]))
}

#[derive(Debug, Clone)]
struct ShellProgram {
    program: String,
    args: Vec<String>,
    label: String,
    extra_env: Vec<(String, String)>,
}

/// Pick a shell executable for [`SessionMode::PlainShell`] spawns. `program`
/// is what we pass to `CommandBuilder` (resolved on PATH if it has no
/// separator); `label` is the short token shown in the UI; `args` and
/// `extra_env` together install the shell-integration init script for shells
/// that support it (PowerShell / bash / zsh) — emits OSC 7 for cwd and
/// OSC 133 + OSC 633 marks the frontend parses for the per-command chip UI.
/// cmd.exe gets a minimal OSC 7 prompt hook only (no command tracking).
///
/// Resolution order:
/// - `RUSTLING_TULIP_SHELL` env override (any platform);
/// - Windows: `pwsh.exe` → `powershell.exe` → `cmd.exe`;
/// - Unix: `$SHELL` → `bash` → `sh`.
fn resolve_shell_program(dirs: &Dirs) -> anyhow::Result<ShellProgram> {
    if let Ok(p) = std::env::var("RUSTLING_TULIP_SHELL") {
        let label = shell_label_from_path(&p);
        let (args, extra_env) = plain_shell_args(dirs, &label)?;
        return Ok(ShellProgram {
            program: p,
            args,
            label,
            extra_env,
        });
    }
    #[cfg(windows)]
    {
        for (candidate, label) in [
            ("pwsh.exe", "pwsh"),
            ("powershell.exe", "powershell"),
            ("cmd.exe", "cmd"),
        ] {
            if which::which(candidate).is_ok() {
                let label = label.to_string();
                let (args, extra_env) = plain_shell_args(dirs, &label)?;
                return Ok(ShellProgram {
                    program: candidate.to_string(),
                    args,
                    label,
                    extra_env,
                });
            }
        }
        anyhow::bail!("no shell on PATH (tried pwsh.exe, powershell.exe, cmd.exe)")
    }
    #[cfg(not(windows))]
    {
        if let Ok(p) = std::env::var("SHELL") {
            let label = shell_label_from_path(&p);
            let (args, extra_env) = plain_shell_args(dirs, &label)?;
            return Ok(ShellProgram {
                program: p,
                args,
                label,
                extra_env,
            });
        }
        for (candidate, label) in [("bash", "bash"), ("zsh", "zsh"), ("sh", "sh")] {
            if which::which(candidate).is_ok() {
                let label = label.to_string();
                let (args, extra_env) = plain_shell_args(dirs, &label)?;
                return Ok(ShellProgram {
                    program: candidate.to_string(),
                    args,
                    label,
                    extra_env,
                });
            }
        }
        anyhow::bail!("no shell on PATH (tried bash, zsh, sh)")
    }
}

fn shell_label_from_path(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .map_or_else(|| "shell".to_string(), str::to_lowercase)
}

/// True iff shell integration (OSC 133 + OSC 633 hooks) should be injected.
/// Defaulted on; set `RUSTLING_TULIP_SHELL_INTEGRATION=0` to disable for
/// debugging or when the user ships their own integration in `.bashrc` etc.
fn shell_integration_enabled() -> bool {
    match std::env::var("RUSTLING_TULIP_SHELL_INTEGRATION") {
        Ok(v) => !matches!(v.trim(), "0" | "false" | "no" | "off"),
        Err(_) => true,
    }
}

type ShellLaunchExtras = (Vec<String>, Vec<(String, String)>);

fn plain_shell_args(dirs: &Dirs, label: &str) -> anyhow::Result<ShellLaunchExtras> {
    let integrate = shell_integration_enabled();
    match label {
        "pwsh" | "powershell" => {
            let script = if integrate {
                powershell_prompt_script_with_integration()
            } else {
                powershell_cwd_prompt_script_minimal()
            };
            Ok((
                vec!["-NoExit".to_string(), "-Command".to_string(), script],
                Vec::new(),
            ))
        }
        "cmd" => Ok((
            vec![
                "/K".to_string(),
                "prompt $E]7;file:///$P$E\\$P$G".to_string(),
            ],
            Vec::new(),
        )),
        "bash" => {
            if !integrate {
                return Ok((Vec::new(), Vec::new()));
            }
            let path = ensure_bash_rcfile(dirs)?;
            Ok((
                vec![
                    "--rcfile".to_string(),
                    path.to_string_lossy().into_owned(),
                    "-i".to_string(),
                ],
                Vec::new(),
            ))
        }
        "zsh" => {
            if !integrate {
                return Ok((Vec::new(), Vec::new()));
            }
            let zdotdir = ensure_zsh_dotdir(dirs)?;
            let mut env = vec![(
                "ZDOTDIR".to_string(),
                zdotdir.to_string_lossy().into_owned(),
            )];
            // Preserve the user's original ZDOTDIR so our .zshrc can source
            // the right rc file. Falls back to $HOME inside the script.
            if let Ok(orig) = std::env::var("ZDOTDIR") {
                env.push(("RUSTLING_TULIP_ZSH_USER_ZDOTDIR".to_string(), orig));
            }
            Ok((Vec::new(), env))
        }
        _ => Ok((Vec::new(), Vec::new())),
    }
}

/// Directory used to stage shell-integration init scripts (bash rcfile,
/// zsh ZDOTDIR). Lives under the config root so it survives reboots; the
/// content is regenerated on demand whenever the daemon writes a script.
fn shell_init_dir(dirs: &Dirs) -> PathBuf {
    dirs.config.join("shell-init")
}

fn ensure_bash_rcfile(dirs: &Dirs) -> anyhow::Result<PathBuf> {
    let dir = shell_init_dir(dirs);
    std::fs::create_dir_all(&dir).context("creating shell-init dir")?;
    let path = dir.join("bash-init.sh");
    std::fs::write(&path, BASH_INIT_SCRIPT).context("writing bash-init.sh")?;
    Ok(path)
}

fn ensure_zsh_dotdir(dirs: &Dirs) -> anyhow::Result<PathBuf> {
    let dir = shell_init_dir(dirs).join("zsh-dotdir");
    std::fs::create_dir_all(&dir).context("creating zsh ZDOTDIR")?;
    let path = dir.join(".zshrc");
    std::fs::write(&path, ZSH_INIT_SCRIPT).context("writing zsh .zshrc")?;
    Ok(dir)
}

/// PowerShell prompt-wrapper script — emits OSC 7 (cwd), plus OSC 133;A/B/D
/// and OSC 633;E for the frontend's per-command chip UI. The previous
/// command's exit code is read from `$LASTEXITCODE` / `$?` inside the
/// prompt function, which PowerShell calls between commands. OSC 133;C and
/// OSC 633;E are emitted from a `PSReadLine` `AddToHistoryHandler` so
/// they land the instant the user presses Enter, before the command starts
/// running. If `PSReadLine` isn't loaded the chip still works (D + A marks
/// only); the frontend falls back to scraping the command text out of the
/// terminal buffer.
fn powershell_prompt_script_with_integration() -> String {
    // Architecture follows VSCode's shellIntegration.ps1 (MIT):
    // - Wrap `PSConsoleHostReadLine` (PSReadLine's host hook) so every
    //   committed line emits OSC 633;E + OSC 133;C and flips a flag.
    //   This fires for ALL commands, including duplicates that
    //   `AddToHistoryHandler` skips when `HistoryNoDuplicates=$true`.
    // - In `prompt`, if the flag was set, emit OSC 133;D with `[int]!$?`
    //   as the exit code. Sidesteps `$LASTEXITCODE` sticky behavior
    //   entirely by using only PowerShell's last-statement success bit.
    // - Re-emit a soft error before calling the original prompt so its
    //   own `$?` check still reflects the user's command outcome.
    [
        // Capture or stub the original prompt function.
        "$global:__rt_original_prompt = if (Test-Path function:\\prompt) {",
        "(Get-Command prompt).ScriptBlock",
        "} else {",
        "{ \"PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) \" }",
        "};",
        "$global:__rt_in_command = $false;",
        "$global:__rt_orig_psreadline = $null;",
        // Escape per VSCode's OSC 633 spec: \\, semicolons, newlines.
        "function global:__rt_esc {",
        "param([string]$s)",
        "if ($null -eq $s) { return '' };",
        "$s = $s -replace '\\\\', '\\\\\\\\';",
        "$s = $s -replace ';', '\\x3b';",
        "$s = $s -replace \"`n\", '\\x0a';",
        "$s",
        "};",
        // OSC 7 cwd emitter — kept for cwd tracking outside shell integration.
        "function global:__rt_emit_cwd {",
        "try {",
        "$path = (Get-Location).ProviderPath;",
        "if ($path) {",
        "$uri = [System.Uri]::new($path).AbsoluteUri;",
        "[Console]::Out.Write([char]27 + ']7;' + $uri + [char]7)",
        "}",
        "} catch { Write-Debug $_ };",
        "};",
        // Prompt function: emit D for the just-finished command (when
        // the PSConsoleHostReadLine wrapper set the in-command flag),
        // then A + the original prompt text + B. Returns one composed
        // string so PowerShell prints the marks in the right visual
        // position relative to the visible prompt.
        "function global:prompt {",
        "$rt_exit = $null;",
        "if ($global:__rt_in_command) {",
        // [int]!$? is 0 for success, 1 for failure. Simple and not
        // poisoned by $LASTEXITCODE staleness; matches VSCode's
        // approach. Real native-process exit codes aren't reported,
        // but a binary success/failure signal is enough for the chip.
        "$rt_exit = [int]!$global:?;",
        "$global:__rt_in_command = $false;",
        // Re-set $? to its pre-prompt value so the user's own prompt
        // function sees the correct success state for the last
        // command. Writing an ignored soft error flips $? to false.
        "if ($rt_exit -ne 0) { Write-Error 'command failed' -ErrorAction Ignore };",
        "};",
        "global:__rt_emit_cwd;",
        "$esc = [char]27; $bel = [char]7;",
        "$marks = '';",
        "if ($null -ne $rt_exit) { $marks += \"$esc]133;D;$rt_exit$bel\" };",
        "$marks += \"$esc]133;A$bel\";",
        "$orig = try { & $global:__rt_original_prompt } catch { 'PS> ' };",
        "\"$marks$orig$esc]133;B$bel\"",
        "};",
        // Wrap PSConsoleHostReadLine — PSReadLine's host entry point
        // for line editing. The wrapper fires for every committed line
        // including duplicates that AddToHistoryHandler skips when
        // HistoryNoDuplicates=$true. Empty/whitespace input is filtered
        // so a bare Enter doesn't produce a phantom chip.
        "try {",
        "if (Get-Module -ListAvailable PSReadLine) {",
        "Import-Module PSReadLine -ErrorAction SilentlyContinue",
        "};",
        "if (Get-Command PSConsoleHostReadLine -ErrorAction SilentlyContinue) {",
        "$global:__rt_orig_psreadline = $function:PSConsoleHostReadLine;",
        "function global:PSConsoleHostReadLine {",
        "$cmd = $global:__rt_orig_psreadline.Invoke();",
        "if (-not [string]::IsNullOrWhiteSpace($cmd)) {",
        "$esc = [char]27; $bel = [char]7;",
        "$escaped = global:__rt_esc $cmd;",
        "[Console]::Out.Write(\"$esc]633;E;$escaped$bel\");",
        "[Console]::Out.Write(\"$esc]133;C$bel\");",
        "$global:__rt_in_command = $true;",
        "};",
        "$cmd",
        "}",
        "}",
        "} catch { Write-Debug $_ };",
        "global:__rt_emit_cwd",
    ]
    .join(" ")
}

/// PowerShell prompt wrapper without OSC 133/633 — used when
/// `RUSTLING_TULIP_SHELL_INTEGRATION=0`. Keeps the OSC 7 cwd emitter so the
/// session sidebar still tracks `cd` changes.
fn powershell_cwd_prompt_script_minimal() -> String {
    [
        "$global:__rt_original_prompt = if (Test-Path function:\\prompt) {",
        "(Get-Command prompt).ScriptBlock",
        "} else {",
        "{ \"PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) \" }",
        "};",
        "function global:__rt_emit_cwd {",
        "try {",
        "$path = (Get-Location).ProviderPath;",
        "if ($path) {",
        "$uri = [System.Uri]::new($path).AbsoluteUri;",
        "[Console]::Out.Write([char]27 + ']7;' + $uri + [char]7)",
        "}",
        "} catch { Write-Debug $_ };",
        "}",
        "function global:prompt {",
        "global:__rt_emit_cwd;",
        "& $global:__rt_original_prompt",
        "};",
        "global:__rt_emit_cwd",
    ]
    .join(" ")
}

const BASH_INIT_SCRIPT: &str = include_str!("../shell-init/bash-init.sh");
const ZSH_INIT_SCRIPT: &str = include_str!("../shell-init/zsh-init.zshrc");

/// The spawn's `extra_env` with each reference (see
/// [`protocol::env_rows::reference`]) replaced by the value it stands for: an
/// env reference by the value `lookup` finds for it, a secret reference by the
/// value saved under its id in Windows Credential Manager. Literals pass
/// through as-is.
///
/// A reference that can't be resolved refuses the spawn, naming the row's key
/// and never the value.
pub(crate) fn resolve_env_refs(
    extra_env: &[(String, String)],
    mut lookup: impl FnMut(&str) -> Option<(user_env::Secret, user_env::Origin)>,
) -> Result<Vec<(String, String)>, spawn_plan::SpawnFailure> {
    extra_env
        .iter()
        .map(|(key, value)| match protocol::env_rows::reference(value) {
            Some(protocol::env_rows::Reference::Env(name)) => {
                let Some((secret, origin)) = lookup(&name) else {
                    return Err(spawn_plan::SpawnFailure {
                        title: "Environment variable not set".to_owned(),
                        detail: format!(
                            "`{name}` isn't set in the daemon's environment, your user environment or the system environment."
                        ),
                        hint: Some(
                            "Set it as a user environment variable, then spawn again.".to_owned(),
                        ),
                    });
                };
                debug!(key = %key, name = %name, ?origin, "spawn env reference resolved");
                Ok((key.clone(), secret.expose().to_owned()))
            }
            Some(protocol::env_rows::Reference::Secret(id)) => {
                let Some(secret) = env_secrets::open(&id) else {
                    return Err(spawn_plan::SpawnFailure {
                        title: "A saved secret is missing".to_owned(),
                        detail: format!(
                            "`{key}`'s saved value is no longer in Windows Credential Manager."
                        ),
                        hint: Some(
                            "Open the spawn dialog, type the value again, and spawn.".to_owned(),
                        ),
                    });
                };
                debug!(key = %key, id = %id, "spawn secret reference resolved");
                Ok((key.clone(), secret.expose().to_owned()))
            }
            None => Ok((key.clone(), value.clone())),
        })
        .collect()
}

/// Everything the daemon can check about a spawn request before it does work
/// it can't undo, on the mode, the provider (`Anthropic` when its options
/// name none) and the env rows the request will spawn with: see
/// [`check_env_rows_with`]. The dispatcher runs it before a checkout
/// confirm, and recovery on the planned request before it registers a repo;
/// `spawn_session` seals and resolves again for the child.
fn check_spawn_request(request: &SpawnRequest) -> Result<(), spawn_plan::SpawnFailure> {
    check_spawn_request_with(request, user_env::resolve)
}

/// [`check_spawn_request`] with the variable lookup `resolve` in place of
/// [`user_env::resolve`].
fn check_spawn_request_with(
    request: &SpawnRequest,
    resolve: impl Fn(&str) -> Option<(user_env::Secret, user_env::Origin)>,
) -> Result<(), spawn_plan::SpawnFailure> {
    check_env_rows_with(
        &request.extra_env,
        &request.secret_env_keys,
        request.mode,
        request_provider(request),
        resolve,
    )
}

/// The Claude provider `request` spawns on: its options', else Anthropic.
fn request_provider(request: &SpawnRequest) -> protocol::ClaudeProvider {
    request.agent_options.claude_provider().unwrap_or_default()
}

/// The checks behind [`check_spawn_request`]: the request must pass
/// [`crate::agents::providers::check_request`], every secret row must be
/// sealable (see [`env_secrets::check_sealable`]), every reference
/// resolvable through `resolve`, and the provider's key set.
fn check_env_rows_with(
    rows: &[(String, String)],
    secret_keys: &[String],
    mode: SessionMode,
    provider: protocol::ClaudeProvider,
    resolve: impl Fn(&str) -> Option<(user_env::Secret, user_env::Origin)>,
) -> Result<(), spawn_plan::SpawnFailure> {
    crate::agents::providers::check_request(mode, provider, rows)?;
    env_secrets::check_sealable(rows, secret_keys)?;
    resolve_env_refs(rows, &resolve)?;
    crate::agents::providers::check_key(provider, &resolve)?;
    Ok(())
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
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "tests assert preconditions with expect/unwrap; failure messages aid debugging"
)]
mod tests {
    use super::connection::FetchRegistry;
    use super::recovery::{discard_abandoned, resume_abandoned};
    use super::test_support::{
        DEEPSEEK_KEY_SENTINEL, MISSING_SECRET_ID, ScratchDir, UNSET_REF, appearance_registry,
        deepseek_key, deepseek_options, deepseek_spawn, dispatch_all, dispatch_one, env,
        env_ref_repo_hub, env_spawn, git_init, git_ok, hook_settings_file, hook_test_hub,
        idle_record, lookup_a, next_message, only_update, repo_entry, secret_ref, snapshot_of,
        spawnless_test_hub, test_hub, wait_until, workspace_entry,
    };
    use super::*;

    /// A hub with one `git init`ed repo `r1` and a workspace `w1` holding it.
    fn hub_with_previewable_repo(tag: &str) -> (Hub, ScratchDir) {
        let (hub, scratch) = test_hub(tag);
        let repo = scratch.path().join("repo");
        std::fs::create_dir_all(&repo).expect("create repo dir");
        git_init(&repo);
        hub.state
            .mutate(|s| {
                s.repos.push(protocol::RepoEntry {
                    id: "r1".to_string(),
                    name: "r1".to_string(),
                    path: repo.to_string_lossy().into_owned(),
                    default_branch: None,
                    default_use_worktree: true,
                    appearance: AppearanceOverrides::default(),
                    last_agent: None,
                    last_spawn_config: None,
                });
                s.workspaces.push(protocol::WorkspaceEntry {
                    id: "w1".to_string(),
                    name: "w1".to_string(),
                    member_repo_ids: vec!["r1".to_string()],
                    linked_vscode_workspace: None,
                    watch_vscode_workspace: false,
                    default_use_worktree: true,
                    appearance: AppearanceOverrides::default(),
                    last_spawn_config: None,
                });
            })
            .expect("register repo and workspace");
        (hub, scratch)
    }

    #[tokio::test]
    async fn a_spawn_preview_echoes_its_request_id() {
        let (hub, _scratch) = hub_with_previewable_repo("preview-rid");
        let msg = dispatch_one(
            &hub,
            ClientMessage::PreviewSpawn {
                repo_id: "r1".to_string(),
                branch_name: "feat".to_string(),
                base_branch: None,
                use_worktree: true,
                request_id: Some("p1".to_string()),
            },
        )
        .await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::SpawnPreview { repo_id, request_id: Some(id), .. }
                    if id == "p1" && repo_id == "r1"
            ),
            "{msg:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_spawn_preview_echoes_its_request_id() {
        let (hub, _scratch) = test_hub("preview-fail-rid");
        let msg = dispatch_one(
            &hub,
            ClientMessage::PreviewSpawn {
                repo_id: "no-such-repo".to_string(),
                branch_name: "feat".to_string(),
                base_branch: None,
                use_worktree: true,
                request_id: Some("p2".to_string()),
            },
        )
        .await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::Error { message, request_id: Some(id) }
                    if id == "p2" && message.contains("no-such-repo")
            ),
            "{msg:?}"
        );
    }

    #[tokio::test]
    async fn a_workspace_spawn_preview_echoes_its_request_id() {
        let (hub, _scratch) = hub_with_previewable_repo("ws-preview-rid");
        let msg = dispatch_one(
            &hub,
            ClientMessage::PreviewWorkspaceSpawn {
                workspace_id: "w1".to_string(),
                branch_name: "feat".to_string(),
                base_branch: None,
                request_id: Some("p3".to_string()),
            },
        )
        .await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::WorkspaceSpawnPreview { workspace_id, request_id: Some(id), .. }
                    if id == "p3" && workspace_id == "w1"
            ),
            "{msg:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_workspace_spawn_preview_echoes_its_request_id() {
        let (hub, _scratch) = test_hub("ws-preview-fail-rid");
        let msg = dispatch_one(
            &hub,
            ClientMessage::PreviewWorkspaceSpawn {
                workspace_id: "no-such-workspace".to_string(),
                branch_name: "feat".to_string(),
                base_branch: None,
                request_id: Some("p4".to_string()),
            },
        )
        .await;
        assert!(
            matches!(
                &msg,
                DaemonMessage::Error { request_id: Some(id), .. } if id == "p4"
            ),
            "{msg:?}"
        );
    }

    /// Output the PTY produced after the snapshot waits in its live receiver;
    /// the reply must still reach the client first. Multi-threaded, so a
    /// forwarder spawned before the reply is queued gets the chance to win.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scrollback_reply_precedes_live_output_and_echoes_the_request_id() {
        let (live_tx, live) = broadcast::channel(8);
        live_tx.send(b"after".to_vec()).unwrap();
        let snap = ScrollbackSnapshot {
            data: b"history".to_vec(),
            truncated: true,
            live,
        };
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let forwarders: PtyForwarders = Arc::new(AsyncMutex::new(HashMap::new()));
        let old = tokio::spawn(std::future::pending::<()>());
        forwarders.lock().await.insert("s1".to_string(), old);

        reply_then_forward(snap, "s1", Some("req-1".to_string()), &out_tx, &forwarders).await;

        let first = out_rx.recv().await.unwrap();
        assert!(
            matches!(
                &first,
                DaemonMessage::Scrollback {
                    session_id,
                    truncated: true,
                    request_id: Some(id),
                    forwarder_restarted: true,
                    ..
                } if session_id == "s1" && id == "req-1"
            ),
            "{first:?}"
        );
        let second = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await
            .expect("the new forwarder sends the queued output")
            .unwrap();
        let expected = base64::engine::general_purpose::STANDARD.encode(b"after");
        assert!(
            matches!(&second, DaemonMessage::PtyOutput { data_b64, .. } if *data_b64 == expected),
            "{second:?}"
        );
        assert!(forwarders.lock().await.contains_key("s1"));
    }

    /// Dispatch `msg` against `hub` without expecting a reply.
    async fn dispatch_silently(hub: &Hub, msg: ClientMessage) {
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        let (file_tx, _file_rx) = mpsc::channel(1);
        let pty_forwarders: PtyForwarders = Arc::new(AsyncMutex::new(HashMap::new()));
        let fetches: FetchRegistry = Arc::new(AsyncMutex::new(HashMap::new()));
        let local = AuthKind::Local;
        let ctx = ConnCtx {
            out_tx: &out_tx,
            file_tx: &file_tx,
            pty_forwarders: &pty_forwarders,
            fetches: &fetches,
            client_id: "c1",
            client_name: None,
            connection: 1,
            auth: &local,
        };
        dispatch(hub, msg, &ctx)
            .await
            .expect("a send-input dispatch succeeds");
    }

    /// The owner's key leaves a hook-reported prompt at once, with no settle
    /// window.
    #[tokio::test]
    async fn owner_input_clears_a_pending_hook_prompt() {
        use crate::history::test_support::{fake_pty, insert_live, record};
        let (hub, _scratch) = test_hub("owner-input-clears");
        let (pty, _exit_tx) = fake_pty();
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        hub.sessions.update("s1", |rec| {
            rec.hook_driven = true;
            rec.status = SessionStatus::AwaitingInput;
            rec.pending_input = Some(protocol::PendingInput::Other {
                message: "approve?".to_owned(),
            });
        });
        let data_b64 = base64::engine::general_purpose::STANDARD.encode(b"y");
        dispatch_silently(
            &hub,
            ClientMessage::SendInput {
                session_id: "s1".to_owned(),
                data_b64,
            },
        )
        .await;
        let snapshot = snapshot_of(&hub, "s1");
        assert_eq!(
            snapshot.status,
            SessionStatus::Idle,
            "the owner's key leaves the prompt at once"
        );
        assert_eq!(
            snapshot.pending_input, None,
            "the pending prompt is cleared"
        );
    }

    /// A session with no hook events keeps exactly today's behaviour: input
    /// never touches its status or pending state.
    #[tokio::test]
    async fn owner_input_leaves_a_heuristic_prompt_alone() {
        use crate::history::test_support::{fake_pty, insert_live, record};
        let (hub, _scratch) = test_hub("owner-input-heuristic");
        let (pty, _exit_tx) = fake_pty();
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        hub.sessions.update("s1", |rec| {
            rec.status = SessionStatus::AwaitingInput;
            rec.pending_input = Some(protocol::PendingInput::Other {
                message: "approve?".to_owned(),
            });
        });
        let data_b64 = base64::engine::general_purpose::STANDARD.encode(b"y");
        dispatch_silently(
            &hub,
            ClientMessage::SendInput {
                session_id: "s1".to_owned(),
                data_b64,
            },
        )
        .await;
        let snapshot = snapshot_of(&hub, "s1");
        assert_eq!(
            snapshot.status,
            SessionStatus::AwaitingInput,
            "a heuristic prompt is the classifier's to clear"
        );
        assert!(
            snapshot.pending_input.is_some(),
            "the heuristic's pending input is untouched"
        );
    }

    #[tokio::test]
    async fn stop_then_exit_keeps_stopped_by_user() {
        use crate::history::test_support::{fake_pty, insert_live, record};
        use crate::pty::PtyExit;
        let (hub, _scratch) = test_hub("stop-then-exit");
        let (pty, exit_tx) = fake_pty();
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        stop_session(&hub, "s1").await.expect("stop");
        // The exit watcher deletes meta.json as its last step, so a fresh one
        // tells the test when the watcher has finished.
        let meta = orphan::meta_from_record(
            "s1".to_string(),
            1,
            "s1".to_string(),
            SessionKind::Standalone,
            SessionMode::Interactive,
            Vec::new(),
            Utc::now(),
            None,
            None,
            Agent::Claude,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("build meta");
        orphan::write_meta(&hub.dirs, &meta).expect("write meta");
        exit_tx.send(PtyExit::Code(0)).expect("report the exit");
        for _ in 0..500 {
            if orphan::load_meta(&hub.dirs, "s1").is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            orphan::load_meta(&hub.dirs, "s1").is_err(),
            "the exit watcher ran"
        );
        let entries = history::read_all(&hub.dirs);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].end, SessionEnd::StoppedByUser);
    }

    #[tokio::test]
    async fn discard_writes_stopped_by_user() {
        use crate::history::test_support::record;
        let (hub, _scratch) = test_hub("discard-history");
        hub.sessions.insert(record("s1", SessionMode::PlainShell));
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        discard_session(&hub, "s1", &[], &out_tx).await;
        assert!(hub.sessions.get("s1").is_none(), "the session is gone");
        let entries = history::read_all(&hub.dirs);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, "s1");
        assert_eq!(entries[0].end, SessionEnd::StoppedByUser);
    }

    /// The spawn-time sidecar is built from a snapshot taken before the
    /// session's first status tick; the sync right after the write must leave
    /// the status that actually reached the record on disk.
    #[tokio::test]
    async fn spawn_sidecar_sync_picks_up_a_status_change() {
        use crate::history::test_support::record;
        let (hub, _scratch) = test_hub("spawn-sidecar-sync");
        hub.sessions.insert(record("s1", SessionMode::Interactive));
        let mut meta = orphan::meta_from_record(
            "s1".to_string(),
            1,
            "s1".to_string(),
            SessionKind::Standalone,
            SessionMode::Interactive,
            Vec::new(),
            Utc::now(),
            None,
            None,
            Agent::Claude,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("build meta");
        meta.status = Some(SessionStatus::Working);
        meta.status_since =
            Some(chrono::DateTime::from_timestamp(1_600_000_000, 0).expect("a fixed stamp"));

        // The first status tick lands after the snapshot was taken, and its own
        // sync found no sidecar to update.
        hub.sessions.update("s1", |rec| {
            rec.status = SessionStatus::AwaitingInput;
        });

        write_meta_then_sync(&hub, &meta);

        let loaded = orphan::load_meta(&hub.dirs, "s1").expect("load meta");
        assert_eq!(loaded.status, Some(SessionStatus::AwaitingInput));
        let live = hub
            .sessions
            .snapshots()
            .into_iter()
            .next()
            .expect("the record");
        assert_eq!(loaded.status_since, live.status_since);
    }

    /// A discard for a session the registry no longer holds — the client's
    /// pane close racing its own auto-discard — must leave the disk alone.
    #[tokio::test]
    async fn discard_of_unknown_session_is_a_no_op() {
        let (hub, _scratch) = test_hub("discard-unknown");
        hub.state.mutate(|_| ()).expect("persist state.json");
        // A sidecar left behind by a session the registry no longer lists.
        let stray = hub.dirs.sessions_dir.join("ghost");
        std::fs::create_dir_all(&stray).expect("create stray session dir");
        std::fs::write(stray.join("meta.json"), b"{}").expect("write stray meta");
        let before = std::fs::read(&hub.dirs.state_file).expect("read state.json");
        let before_mtime = std::fs::metadata(&hub.dirs.state_file)
            .and_then(|m| m.modified())
            .expect("state.json mtime");

        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        discard_session(&hub, "ghost", &[], &out_tx).await;

        assert!(
            out_rx.try_recv().is_err(),
            "an unknown session produces no reply"
        );
        assert!(
            stray.join("meta.json").is_file(),
            "the stray sidecar survives"
        );
        assert_eq!(
            std::fs::read(&hub.dirs.state_file).expect("read state.json"),
            before,
            "state.json keeps its contents"
        );
        assert_eq!(
            std::fs::metadata(&hub.dirs.state_file)
                .and_then(|m| m.modified())
                .expect("state.json mtime"),
            before_mtime,
            "state.json is not rewritten"
        );
        assert!(
            crate::history::read_one(&hub.dirs, "ghost").is_none(),
            "no history entry for a session that never existed"
        );
    }

    #[tokio::test]
    async fn drained_shutdown_writes_daemon_shutdown() {
        use crate::history::test_support::{fake_pty, insert_live, record};
        let (hub, _scratch) = test_hub("shutdown-history");
        let (pty, _exit_tx) = fake_pty();
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        shutdown_all(&hub).await;
        let entries = history::read_all(&hub.dirs);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].end, SessionEnd::DaemonShutdown);
    }

    #[tokio::test]
    async fn park_writes_stopped_by_user() {
        use crate::history::test_support::{fake_pty, insert_live, record};
        let (hub, _scratch) = test_hub("park-history");
        let (pty, _exit_tx) = fake_pty();
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        park_session(&hub, "s1").await;
        let entries = history::read_all(&hub.dirs);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].end, SessionEnd::StoppedByUser);
    }

    #[tokio::test]
    async fn tracer_lost_after_user_stop_does_not_abandon() {
        use crate::history::test_support::{fake_pty, insert_live, record, write_meta_for};
        use crate::pty::PtyExit;
        let (hub, _scratch) = test_hub("stop-then-lost");
        let (pty, exit_tx) = fake_pty();
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        write_meta_for(&hub.dirs, "s1");
        stop_session(&hub, "s1").await.expect("stop");
        exit_tx.send(PtyExit::TracerLost).expect("report the loss");
        // Only the exit watcher's non-abandoned path sets an exit code, so
        // this waits for the watcher to have run.
        let watcher_ran = || {
            hub.sessions
                .get("s1")
                .is_some_and(|rec| crate::sync::lock(&rec).exit_code == Some(-1))
        };
        assert!(
            wait_until(watcher_ran).await,
            "the exit watcher ends a stopped session like any exit"
        );
        let rec = hub.sessions.get("s1").expect("still listed");
        assert!(!crate::sync::lock(&rec).is_abandoned);
        assert!(
            orphan::load_meta(&hub.dirs, "s1").is_err(),
            "a stopped session's sidecar is deleted"
        );
        let entries = history::read_all(&hub.dirs);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].end, SessionEnd::StoppedByUser);
    }

    #[tokio::test]
    async fn dismiss_after_tracer_loss_keeps_tracer_lost_history() {
        use crate::history::test_support::{fake_pty, insert_live, record, write_meta_for};
        use crate::pty::PtyExit;
        let (hub, _scratch) = test_hub("lost-then-dismiss");
        let (pty, exit_tx) = fake_pty();
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        write_meta_for(&hub.dirs, "s1");
        exit_tx.send(PtyExit::TracerLost).expect("report the loss");
        let abandoned = || {
            hub.sessions
                .get("s1")
                .is_some_and(|rec| crate::sync::lock(&rec).is_abandoned)
        };
        assert!(wait_until(abandoned).await, "the session is abandoned");
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        discard_abandoned(&hub, "s1", &out_tx);
        assert!(hub.sessions.get("s1").is_none(), "dismissed");
        let entries = history::read_all(&hub.dirs);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].end, SessionEnd::TracerLost);
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a missing broadcast fails the test loudly")]
    async fn set_session_busy_excluded_updates_the_registry_and_broadcasts() {
        let (hub, _scratch) = test_hub("busy-excluded-set");
        hub.sessions.insert(idle_record("s1"));
        let mut events = hub.sessions.subscribe();
        for excluded in [true, false] {
            let sent = dispatch_all(
                &hub,
                ClientMessage::SetSessionBusyExcluded {
                    session_id: "s1".to_owned(),
                    excluded,
                },
            )
            .await;
            assert!(sent.is_empty(), "the broadcast is the answer: {sent:?}");
            let rec = hub.sessions.get("s1").expect("s1 is registered");
            assert_eq!(crate::sync::lock(&rec).busy_excluded, excluded);
            let Ok(SessionEvent::Updated(snap, None)) = events.try_recv() else {
                panic!("expected a SessionUpdated broadcast for excluded = {excluded}");
            };
            assert_eq!(snap.busy_excluded, excluded);
        }
    }

    #[tokio::test]
    async fn set_session_busy_excluded_for_an_unknown_session_answers_like_rename() {
        let (hub, _scratch) = test_hub("busy-excluded-unknown");
        let renamed = dispatch_all(
            &hub,
            ClientMessage::RenameSession {
                session_id: "ghost".to_owned(),
                label: Some("x".to_owned()),
            },
        )
        .await;
        let excluded = dispatch_all(
            &hub,
            ClientMessage::SetSessionBusyExcluded {
                session_id: "ghost".to_owned(),
                excluded: true,
            },
        )
        .await;
        assert!(
            renamed.is_empty(),
            "a rename of an unknown session: {renamed:?}"
        );
        assert!(
            excluded.is_empty(),
            "answers as a rename does: {excluded:?}"
        );
        assert!(hub.sessions.get("ghost").is_none(), "no session is made");
    }

    #[test]
    fn carried_busy_excluded_is_the_sources_flag_only_when_carried() {
        let (sessions, _dirs) = appearance_registry("busy-carry");
        sessions.update("s1", |rec| rec.busy_excluded = true);
        assert!(
            !carried_busy_excluded(&sessions, "s1", false),
            "a Duplicate starts counted"
        );
        assert!(
            carried_busy_excluded(&sessions, "s1", true),
            "a Restart or Resume keeps the flag"
        );
        assert!(!carried_busy_excluded(&sessions, "ghost", true));
    }

    #[test]
    fn adopt_busy_excluded_marks_and_broadcasts_only_an_excluded_session() {
        let (sessions, _dirs) = appearance_registry("busy-adopt");
        sessions.insert(idle_record("copy"));
        let mut events = sessions.subscribe();
        adopt_busy_excluded(&sessions, "copy", false);
        assert!(events.try_recv().is_err(), "a counted copy needs no update");
        adopt_busy_excluded(&sessions, "copy", true);
        let rec = sessions.get("copy").expect("the copy is registered");
        assert!(crate::sync::lock(&rec).busy_excluded);
        assert!(
            matches!(events.try_recv(), Ok(SessionEvent::Updated(snap, None)) if snap.busy_excluded),
            "the copy's flag is broadcast"
        );
    }

    #[tokio::test]
    async fn duplicate_keeps_a_deepseek_provider() {
        let (hub, scratch) = spawnless_test_hub("duplicate-deepseek");
        let target = SpawnTarget::Standalone {
            cwd: Some(scratch.path().to_string_lossy().into_owned()),
            add_dirs: Vec::new(),
        };
        let mut source = idle_record("source");
        source.spawn_config = Some(protocol::SpawnConfig::from_request(&deepseek_spawn(
            target,
            SessionMode::Interactive,
            Vec::new(),
            None,
        )));
        hub.sessions.insert(source);
        let request = duplicate_request(&hub, "source")
            .await
            .expect("source can be duplicated");
        assert_eq!(request.agent_options, deepseek_options());
    }

    /// Run a real interactive Claude spawn for `session_id` in `folder` on
    /// `hub`, which gets as far as the stub tracer and fails there, and answer
    /// the error's whole chain.
    async fn failing_interactive_claude_spawn(
        hub: &Hub,
        folder: &Path,
        session_id: &str,
    ) -> String {
        let request = env_spawn(
            SpawnTarget::Standalone {
                cwd: Some(folder.to_string_lossy().into_owned()),
                add_dirs: Vec::new(),
            },
            SessionMode::Interactive,
            Vec::new(),
            None,
        );
        let stored_config = protocol::SpawnConfig::from_request(&request);
        let cfg = SpawnArgs {
            dangerously_skip_permissions: false,
            agent_options: AgentOptions::claude(None),
            model: None,
            env: crate::agents::providers::SpawnEnv::default(),
            prompt_injector: None,
            origin: None,
            resume_conversation: None,
            add_dirs: Vec::new(),
        };
        format!(
            "{:#}",
            spawn_interactive_session(
                hub,
                session_id.to_owned(),
                "cleanup".to_owned(),
                SessionKind::Standalone,
                Vec::new(),
                None,
                folder.to_path_buf(),
                None,
                &cfg,
                stored_config,
            )
            .await
            .expect_err("the stub tracer is not a runnable executable")
        )
    }

    /// The command `history::shell_request` puts on a recovery's injector,
    /// before the spawn renders its settings slot.
    fn shell_resume_command(conversation: &str) -> String {
        format!(
            "claude --settings {} --resume {conversation}",
            history::HOOK_SETTINGS_SLOT
        )
    }

    /// Run a real plain-shell recovery spawn for `session_id` in `folder` on
    /// `hub`, which gets as far as the stub tracer and fails there, and answer
    /// the error's whole chain.
    async fn failing_plain_shell_recovery_spawn(
        hub: &Hub,
        folder: &Path,
        session_id: &str,
    ) -> String {
        let request = env_spawn(
            SpawnTarget::Standalone {
                cwd: Some(folder.to_string_lossy().into_owned()),
                add_dirs: Vec::new(),
            },
            SessionMode::PlainShell,
            Vec::new(),
            None,
        );
        let stored_config = protocol::SpawnConfig::from_request(&request);
        let cfg = SpawnArgs {
            dangerously_skip_permissions: false,
            agent_options: AgentOptions::claude(None),
            model: None,
            env: crate::agents::providers::SpawnEnv::default(),
            prompt_injector: Some(protocol::PromptInjector {
                steps: vec![
                    protocol::InjectorStep::Delay { ms: 2000 },
                    protocol::InjectorStep::Text {
                        content: shell_resume_command("abc-123"),
                        newline: true,
                    },
                ],
                verify_mode_marker: None,
                startup: protocol::InjectorStartup::ShellPrompt,
            }),
            origin: None,
            resume_conversation: None,
            add_dirs: Vec::new(),
        };
        format!(
            "{:#}",
            spawn_plain_shell_session(
                hub,
                session_id.to_owned(),
                "cleanup".to_owned(),
                SessionKind::Standalone,
                Vec::new(),
                None,
                folder.to_path_buf(),
                &cfg,
                stored_config,
            )
            .await
            .expect_err("the stub tracer is not a runnable executable")
        )
    }

    #[test]
    fn hook_settings_are_off_without_a_cached_tracer() {
        let (hub, _scratch) = spawnless_test_hub("hook-settings-no-tracer");
        assert!(
            write_hook_settings(&hub, "no-tracer", Agent::Claude).is_none(),
            "hooks are off when the cached tracer cannot be resolved"
        );
        let file = hook_settings_file(&hub, "no-tracer");
        assert_eq!(
            file.file_name().and_then(|name| name.to_str()),
            Some("hook-settings.json")
        );
        assert!(!file.exists(), "no settings file was written");
        assert!(
            !hub.dirs.sessions_dir.join("no-tracer").exists(),
            "no session dir was created either"
        );
    }

    #[test]
    fn hook_settings_name_the_cached_tracer_for_an_interactive_claude() {
        let (hub, _scratch) = hook_test_hub("hook-settings-claude");
        let settings = write_hook_settings(&hub, "claude-session", Agent::Claude)
            .expect("a cached tracer resolves");
        assert!(settings.created_dir, "the session dir did not exist before");
        let file = hook_settings_file(&hub, "claude-session");
        assert_eq!(
            settings.path(),
            file,
            "the spawn passes the file under its literal name"
        );
        let cached = crate::tracer_client::cached_tracer_exe(&hub.dirs).expect("resolves");
        let text = std::fs::read_to_string(&file).expect("the settings file is readable");
        let document: serde_json::Value =
            serde_json::from_str(&text).expect("the settings file is JSON");
        assert_eq!(
            document
                .as_object()
                .expect("a JSON object")
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["hooks"],
            "the file holds only a hooks key"
        );
        let hook = &document["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(
            hook["command"].as_str(),
            cached.to_str(),
            "the hook command names the cached tracer the spawn runs"
        );
        assert_eq!(hook["args"][1], "--session");
        assert_eq!(hook["args"][2], "claude-session");
        assert_eq!(hook["args"][3], "--config-dir");
        assert_eq!(
            hook["args"][4],
            hub.dirs.config.to_string_lossy().as_ref(),
            "the shim reads daemon.json from the daemon's config dir"
        );
    }

    #[test]
    fn discarding_hook_settings_keeps_a_session_dir_it_did_not_create() {
        let (hub, _scratch) = hook_test_hub("hook-settings-keep-dir");
        let dir = hub.dirs.sessions_dir.join("pre-made");
        std::fs::create_dir_all(&dir).expect("pre-create the session dir");
        let settings = write_hook_settings(&hub, "pre-made", Agent::Claude).expect("writes");
        assert!(!settings.created_dir, "the dir was already there");
        assert!(hook_settings_file(&hub, "pre-made").exists());
        settings.discard("pre-made");
        assert!(
            !hook_settings_file(&hub, "pre-made").exists(),
            "the file is removed"
        );
        assert!(
            dir.exists(),
            "a directory this spawn did not create is kept"
        );
    }

    #[tokio::test]
    async fn a_failed_tracer_spawn_removes_its_hook_settings() {
        let (hub, scratch) = hook_test_hub("hook-settings-cleanup");
        let folder = scratch.path().join("folder");
        std::fs::create_dir_all(&folder).expect("create folder");
        let session_id = "cleanup-session";
        let file = hook_settings_file(&hub, session_id);

        // The writer itself works on this hub: it answers `Some` and leaves the
        // file on disk. Discarding the probe first keeps the assertions after
        // the spawn about what the spawn wrote, not about the probe.
        let probe = write_hook_settings(&hub, session_id, Agent::Claude)
            .expect("this hub resolves a cached tracer");
        assert!(file.exists(), "the writer wrote the settings file");
        probe.discard(session_id);
        assert!(!file.exists(), "the probe is removed");

        // A real interactive Claude spawn at that same id goes through the
        // writer and then fails at the stub tracer, which is not runnable.
        let error = failing_interactive_claude_spawn(&hub, &folder, session_id).await;
        assert!(error.contains("spawning claude via tracer"), "{error}");
        assert!(
            !file.exists(),
            "the failed spawn removed the settings file it wrote"
        );
        assert!(
            !hub.dirs.sessions_dir.join(session_id).exists(),
            "and the session dir it created"
        );

        // A file already at the id is gone afterwards only if the spawn itself
        // rewrote and then removed it — a spawn that skipped the writer would
        // leave the marker behind. The dir pre-existed, so this spawn keeps it.
        let marker =
            write_hook_settings(&hub, session_id, Agent::Claude).expect("writes the marker file");
        assert!(file.exists(), "the marker is there before the second spawn");
        let error = failing_interactive_claude_spawn(&hub, &folder, session_id).await;
        assert!(error.contains("spawning claude via tracer"), "{error}");
        drop(marker);
        assert!(
            !file.exists(),
            "the failed spawn rewrote and removed the marked file"
        );
        assert!(
            hub.dirs.sessions_dir.join(session_id).exists(),
            "a directory this spawn did not create is kept"
        );
    }

    #[test]
    fn render_shell_resume_quotes_a_path_with_spaces() {
        let path = Path::new(r"C:\Users\Some Name\config\sessions\s1\hook-settings.json");
        assert_eq!(
            render_shell_resume(&shell_resume_command("abc-123"), Some(path)),
            r#"claude --settings "C:\Users\Some Name\config\sessions\s1\hook-settings.json" --resume abc-123"#
        );
    }

    #[test]
    fn render_shell_resume_drops_the_clause_without_settings() {
        assert_eq!(
            render_shell_resume(&shell_resume_command("abc-123"), None),
            "claude --resume abc-123"
        );
    }

    #[test]
    fn render_shell_resume_drops_the_clause_for_an_unquotable_path() {
        let path = Path::new("C:\\odd\"name\\hook-settings.json");
        assert_eq!(
            render_shell_resume(&shell_resume_command("abc-123"), Some(path)),
            "claude --resume abc-123"
        );
    }

    #[tokio::test]
    async fn render_shell_resume_removes_the_settings_of_a_failed_plain_shell_spawn() {
        let (hub, scratch) = hook_test_hub("shell-hook-settings-cleanup");
        let folder = scratch.path().join("folder");
        std::fs::create_dir_all(&folder).expect("create folder");
        let session_id = "cleanup-shell";
        let file = hook_settings_file(&hub, session_id);

        let error = failing_plain_shell_recovery_spawn(&hub, &folder, session_id).await;
        assert!(error.contains("spawning plain shell via tracer"), "{error}");
        assert!(
            !file.exists(),
            "the failed spawn removed the settings file it wrote"
        );
        assert!(
            !hub.dirs.sessions_dir.join(session_id).exists(),
            "and the session dir it created"
        );

        // A marker at the id is gone afterwards only if the spawn itself
        // rewrote and then removed it — a spawn that skipped the writer would
        // leave it behind. The dir pre-existed, so this spawn keeps it.
        let dir = hub.dirs.sessions_dir.join(session_id);
        std::fs::create_dir_all(&dir).expect("pre-create the session dir");
        std::fs::write(&file, b"stale marker").expect("write the marker");
        let error = failing_plain_shell_recovery_spawn(&hub, &folder, session_id).await;
        assert!(error.contains("spawning plain shell via tracer"), "{error}");
        assert!(
            !file.exists(),
            "the failed spawn rewrote and removed the marked file"
        );
        assert!(
            dir.exists(),
            "a directory this spawn did not create is kept"
        );
    }

    /// A graceful stop removes the hook settings file a spawn wrote, its
    /// sidecar, and the session directory once that file was all it held.
    #[tokio::test]
    async fn a_graceful_stop_removes_the_hook_settings_file() {
        use crate::history::test_support::{fake_pty, insert_live, record, write_meta_for};
        let (hub, _scratch) = test_hub("stop-hook-settings");
        let (pty, _exit_tx) = fake_pty();
        insert_live(
            &hub.sessions,
            &hub.dirs,
            record("s1", SessionMode::Interactive),
            &pty,
        );
        write_meta_for(&hub.dirs, "s1");
        let file = hook_settings_file(&hub, "s1");
        std::fs::write(&file, b"{\"hooks\":{}}").expect("write hook settings");

        stop_session(&hub, "s1").await.expect("stop");

        assert!(
            !file.exists(),
            "a graceful stop removes the hook settings file"
        );
        assert!(
            orphan::load_meta(&hub.dirs, "s1").is_err(),
            "and the session no longer appears as an orphan"
        );
        assert!(
            !hub.dirs.sessions_dir.join("s1").exists(),
            "and its directory, which held nothing else"
        );
    }

    #[test]
    fn a_whole_value_env_reference_resolves() {
        let resolved = resolve_env_refs(&env(&[("K", "${env:A}")]), lookup_a).expect("resolves");
        assert_eq!(resolved, env(&[("K", "va")]));
    }

    #[test]
    fn literals_and_embedded_or_invalid_references_pass_through() {
        let extra = env(&[
            ("LIT", "plain"),
            ("EMBEDDED", "x${env:A}y"),
            ("BAD_NAME", "${env:1BAD}"),
            ("NO_NAME", "${env:}"),
            ("UNCLOSED", "${env:A"),
            ("EXTRA_BRACE", "${env:A}}"),
            ("EMPTY", ""),
        ]);
        let resolved = resolve_env_refs(&extra, |_| None).expect("no references");
        assert_eq!(resolved, extra);
    }

    #[test]
    fn an_unset_env_reference_refuses_naming_the_variable() {
        let failure = resolve_env_refs(&env(&[("K", "${env:MISSING_1}")]), lookup_a)
            .expect_err("unset reference refuses");
        assert_eq!(failure.title, "Environment variable not set");
        assert_eq!(
            failure.detail,
            "`MISSING_1` isn't set in the daemon's environment, your user environment or the system environment."
        );
        assert_eq!(
            failure.hint.as_deref(),
            Some("Set it as a user environment variable, then spawn again.")
        );
    }

    /// The reply must be the unset-reference refusal, echoing `rid`.
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    fn assert_unset_refusal(reply: DaemonMessage, rid: &str) {
        let DaemonMessage::ActionFailed {
            title,
            detail,
            hint,
            request_id,
        } = reply
        else {
            panic!("expected action_failed, got {reply:?}");
        };
        assert_eq!(title, "Environment variable not set");
        assert!(detail.contains("`RT_TEST_UNSET_ENV_REF_4C1D`"), "{detail}");
        assert!(hint.is_some());
        assert_eq!(request_id.as_deref(), Some(rid));
    }

    #[tokio::test]
    async fn a_worktree_spawn_with_an_unset_env_reference_is_refused_before_any_git_work() {
        let (hub, _scratch, repo) = env_ref_repo_hub("env-ref-unset");
        let req = env_spawn(
            single_target(true, None),
            SessionMode::Interactive,
            env(&[("ANTHROPIC_API_KEY", UNSET_REF)]),
            Some("rq-env"),
        );
        // Straight into `spawn_session`, the path every replay (duplicate,
        // resume, recover) takes, past the dispatcher's own early check.
        let err = spawn_session(&hub, req, None)
            .await
            .expect_err("an unset reference refuses");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        send_spawn_failure(&out_tx, &err, Some("rq-env"));
        assert_unset_refusal(out_rx.try_recv().expect("a reply"), "rq-env");
        let worktrees = git_ok(&repo, &["worktree", "list", "--porcelain"]);
        assert_eq!(
            worktrees
                .lines()
                .filter(|l| l.starts_with("worktree "))
                .count(),
            1,
            "only the main worktree: {worktrees}"
        );
        assert_eq!(git_ok(&repo, &["branch", "--list", "wt/x"]).trim(), "");
        let made = std::fs::read_dir(hub.state.worktrees_dir()).map_or(0, Iterator::count);
        assert_eq!(made, 0, "nothing under the worktrees root");
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn an_unset_env_reference_is_refused_before_the_checkout_confirm() {
        let (hub, _scratch, repo) = env_ref_repo_hub("env-ref-confirm");
        std::fs::write(repo.join("dirty.txt"), "x\n").expect("dirty the tree");
        let literal = env_spawn(
            single_target(false, None),
            SessionMode::Interactive,
            env(&[("K", "v")]),
            Some("rq-lit"),
        );
        let reply = dispatch_one(&hub, ClientMessage::SpawnSession(literal)).await;
        let DaemonMessage::CheckoutConfirmRequired { .. } = reply else {
            panic!("the literal spawn needs a checkout confirm, got {reply:?}");
        };
        let unset = env_spawn(
            single_target(false, None),
            SessionMode::Interactive,
            env(&[("K", UNSET_REF)]),
            Some("rq-ref"),
        );
        let reply = dispatch_one(&hub, ClientMessage::SpawnSession(unset)).await;
        assert_unset_refusal(reply, "rq-ref");
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
    }

    #[tokio::test]
    async fn the_child_env_gets_reference_values_while_stored_and_replayed_configs_keep_references()
    {
        let (hub, _scratch, _repo) = env_ref_repo_hub("env-ref-stored");
        let extra = env(&[("RT_PATH_COPY", "${env:PATH}"), ("RT_LIT", "x${env:PATH}")]);
        let mut target = single_target(false, None);
        if let SpawnTarget::Single { branch_name, .. } = &mut target {
            "main".clone_into(branch_name);
        }
        let req = env_spawn(target, SessionMode::PlainShell, extra.clone(), None);
        let _ = child_env_probe::take();
        // The spawnless hub fails at the tracer, after the spec is built.
        spawn_session(&hub, req, None)
            .await
            .expect_err("no tracer can start");
        let child = child_env_probe::take().expect("a spawn spec's env was built");
        let path = std::env::var("PATH").expect("PATH is set");
        let value = |key: &str| child.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        assert_eq!(
            value("RT_PATH_COPY"),
            Some(path),
            "the child gets the value"
        );
        assert_eq!(value("RT_LIT").as_deref(), Some("x${env:PATH}"));

        let stored = hub
            .state
            .with_persisted(|s| s.repos[0].last_spawn_config.clone())
            .expect("last spawn config stored");
        assert_eq!(stored.extra_env, extra, "the reference is stored as sent");
        assert_eq!(
            stored.to_clone_request().extra_env,
            extra,
            "resume replays it"
        );
        assert_eq!(
            stored
                .to_duplicate_request(Some("wt/y".to_owned()))
                .extra_env,
            extra,
            "duplicate replays it"
        );
    }

    /// Repo `r1`'s `main`, in place: no worktree.
    fn in_place_main() -> SpawnTarget {
        let mut target = single_target(false, None);
        if let SpawnTarget::Single { branch_name, .. } = &mut target {
            "main".clone_into(branch_name);
        }
        target
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn a_deepseek_spawn_without_a_key_is_refused_before_git_work() {
        let (hub, _scratch, repo) = env_ref_repo_hub("deepseek-no-key");
        let req = deepseek_spawn(
            single_target(true, None),
            SessionMode::Interactive,
            Vec::new(),
            Some("rq-ds"),
        );
        let err = spawn_session_with(&hub, req, None, |_| None)
            .await
            .expect_err("a missing key refuses");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        send_spawn_failure(&out_tx, &err, Some("rq-ds"));
        let reply = out_rx.try_recv().expect("a reply");
        let DaemonMessage::ActionFailed {
            title,
            detail,
            hint,
            request_id,
        } = reply
        else {
            panic!("expected action_failed, got {reply:?}");
        };
        assert_eq!(title, "Couldn't start the DeepSeek session");
        assert!(detail.contains("`DEEPSEEK_API_KEY`"), "{detail}");
        assert!(
            hint.as_deref()
                .is_some_and(|h| h.contains("setx DEEPSEEK_API_KEY")),
            "{hint:?}"
        );
        assert_eq!(request_id.as_deref(), Some("rq-ds"));
        let worktrees = git_ok(&repo, &["worktree", "list", "--porcelain"]);
        assert_eq!(
            worktrees
                .lines()
                .filter(|l| l.starts_with("worktree "))
                .count(),
            1,
            "only the main worktree: {worktrees}"
        );
        assert_eq!(git_ok(&repo, &["branch", "--list", "wt/x"]).trim(), "");
        let made = std::fs::read_dir(hub.state.worktrees_dir()).map_or(0, Iterator::count);
        assert_eq!(made, 0, "nothing under the worktrees root");
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
    }

    #[tokio::test]
    async fn the_stored_config_of_a_deepseek_spawn_holds_only_the_users_rows() {
        let (hub, _scratch, _repo) = env_ref_repo_hub("deepseek-stored");
        let extra = env(&[("RT_USER_ROW", "x")]);
        let req = deepseek_spawn(
            in_place_main(),
            SessionMode::Interactive,
            extra.clone(),
            None,
        );
        let _ = child_env_probe::take();
        // The spawnless hub fails at the tracer, after the spec is built.
        spawn_session_with(&hub, req, None, deepseek_key)
            .await
            .expect_err("no tracer can start");
        let child = child_env_probe::take().expect("a spawn spec's env was built");
        assert_eq!(
            child_value(&child, "ANTHROPIC_AUTH_TOKEN").as_deref(),
            Some(DEEPSEEK_KEY_SENTINEL),
            "the child gets the key"
        );
        assert_eq!(child_value(&child, "RT_USER_ROW").as_deref(), Some("x"));

        let stored = hub
            .state
            .with_persisted(|s| s.repos[0].last_spawn_config.clone())
            .expect("last spawn config stored");
        assert_eq!(stored.extra_env, extra, "only the user's rows are stored");
        assert_eq!(
            stored.agent_options.claude_provider(),
            Some(protocol::ClaudeProvider::Deepseek)
        );
        let state = std::fs::read_to_string(&hub.dirs.state_file).expect("state.json written");
        assert!(!state.contains(DEEPSEEK_KEY_SENTINEL), "{state}");
        assert!(!state.contains("ANTHROPIC_BASE_URL"), "{state}");
    }

    #[tokio::test]
    async fn a_deepseek_interactive_spec_carries_the_routing_set_and_removals() {
        let (hub, _scratch, _repo) = env_ref_repo_hub("deepseek-spec");
        let req = deepseek_spawn(in_place_main(), SessionMode::Interactive, Vec::new(), None);
        let _ = child_env_probe::take_both();
        // The spawnless hub fails at the tracer, after the spec is built.
        spawn_session_with(&hub, req, None, deepseek_key)
            .await
            .expect_err("no tracer can start");
        let (child, removed) = child_env_probe::take_both().expect("a spawn spec was built");
        assert_eq!(
            child_value(&child, "ANTHROPIC_BASE_URL").as_deref(),
            Some("https://api.deepseek.com/anthropic")
        );
        assert_eq!(
            child_value(&child, "CLAUDE_DEEPSEEK_SESSION").as_deref(),
            Some("1")
        );
        assert_eq!(
            child_value(&child, "ANTHROPIC_AUTH_TOKEN").as_deref(),
            Some(DEEPSEEK_KEY_SENTINEL)
        );
        assert_eq!(child_value(&child, "ANTHROPIC_API_KEY"), None);
        for name in ["ANTHROPIC_API_KEY", "CLAUDE_CODE_USE_FOUNDRY"] {
            assert!(removed.iter().any(|r| r == name), "{name} in {removed:?}");
        }
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn a_plain_shell_deepseek_request_is_refused() {
        let (hub, _scratch, repo) = env_ref_repo_hub("deepseek-shell");
        let req = deepseek_spawn(
            single_target(true, None),
            SessionMode::PlainShell,
            Vec::new(),
            None,
        );
        // No key anywhere: the shell refusal comes first, not the key's.
        let err = spawn_session_with(&hub, req, None, |_| None)
            .await
            .expect_err("a plain shell refuses a provider");
        let text = format!("{err:#}");
        assert!(
            text.contains("Plain shell sessions do not accept a Claude provider"),
            "{text}"
        );
        assert!(!text.contains("DEEPSEEK_API_KEY"), "{text}");
        let worktrees = git_ok(&repo, &["worktree", "list", "--porcelain"]);
        assert_eq!(
            worktrees
                .lines()
                .filter(|l| l.starts_with("worktree "))
                .count(),
            1,
            "only the main worktree: {worktrees}"
        );
        assert_eq!(git_ok(&repo, &["branch", "--list", "wt/x"]).trim(), "");
        assert!(
            hub.state
                .with_persisted(|s| s.repos[0].last_spawn_config.clone())
                .is_none(),
            "no last spawn config was stored"
        );

        let req = deepseek_spawn(
            single_target(true, None),
            SessionMode::PlainShell,
            Vec::new(),
            Some("rq-sh"),
        );
        let reply = dispatch_one(&hub, ClientMessage::SpawnSession(req)).await;
        let DaemonMessage::ActionFailed {
            detail, request_id, ..
        } = reply
        else {
            panic!("the pre-check refuses, got {reply:?}");
        };
        assert!(
            detail.contains("do not accept a Claude provider"),
            "{detail}"
        );
        assert_eq!(request_id.as_deref(), Some("rq-sh"));
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
    }

    #[test]
    fn the_env_row_pre_check_refuses_a_deepseek_spawn_without_its_key() {
        let failure = check_env_rows_with(
            &[],
            &[],
            SessionMode::Interactive,
            protocol::ClaudeProvider::Deepseek,
            |_| None,
        )
        .expect_err("a missing key refuses");
        assert!(
            failure.detail.contains("`DEEPSEEK_API_KEY`"),
            "{}",
            failure.detail
        );
        assert!(
            check_env_rows_with(
                &[],
                &[],
                SessionMode::Interactive,
                protocol::ClaudeProvider::Deepseek,
                deepseek_key,
            )
            .is_ok()
        );
        assert!(
            check_env_rows_with(
                &[],
                &[],
                SessionMode::Interactive,
                protocol::ClaudeProvider::Anthropic,
                |_| None,
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn a_locked_key_row_is_refused_before_it_is_sealed() {
        assert_deepseek_row_refused_before_sealing(
            "deepseek-locked",
            "ANTHROPIC_AUTH_TOKEN",
            "`ANTHROPIC_AUTH_TOKEN` is set by the DeepSeek provider",
        )
        .await;
    }

    #[tokio::test]
    async fn a_removed_key_row_is_refused_before_it_is_sealed() {
        assert_deepseek_row_refused_before_sealing(
            "deepseek-removed",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "`CLAUDE_CODE_OAUTH_TOKEN` is removed from DeepSeek sessions",
        )
        .await;
    }

    /// A `DeepSeek` spawn with a secret-shaped `key` row is refused with a detail
    /// holding `expected`, by `spawn_session` and by the dispatcher's pre-check
    /// before a checkout confirm, and neither seals the row.
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn assert_deepseek_row_refused_before_sealing(tag: &str, key: &str, expected: &str) {
        let (_lock, _secrets_dir) = env_secrets::test_support::scratch_async(tag).await;
        let (hub, _scratch, repo) = env_ref_repo_hub(tag);
        let rows = env(&[(key, SECRET_SENTINEL)]);
        assert!(
            protocol::env_rows::is_secret_key(key),
            "the row is one sealing would take"
        );
        let req = deepseek_spawn(
            in_place_main(),
            SessionMode::Interactive,
            rows.clone(),
            None,
        );
        let err = spawn_session_with(&hub, req, None, deepseek_key)
            .await
            .expect_err("the row refuses");
        let failure = err
            .downcast_ref::<spawn_plan::SpawnFailure>()
            .expect("a spawn failure");
        assert!(failure.detail.contains(expected), "{}", failure.detail);
        assert!(!format!("{err:#}").contains(SECRET_SENTINEL));
        let config = std::env::var_os("RUSTLING_TULIP_CONFIG_DIR").expect("a scratch config dir");
        assert!(
            !Path::new(&config).join("env-secrets.json").exists(),
            "nothing was sealed"
        );

        // A dirty tree makes this target need a checkout confirm, so the
        // refusal below can only come from the dispatcher's pre-check.
        std::fs::write(repo.join("dirty.txt"), "x\n").expect("dirty the tree");
        let control = env_spawn(
            single_target(false, None),
            SessionMode::Interactive,
            env(&[("K", "v")]),
            Some("rq-ctl"),
        );
        let reply = dispatch_one(&hub, ClientMessage::SpawnSession(control)).await;
        let DaemonMessage::CheckoutConfirmRequired { .. } = reply else {
            panic!("the target needs a checkout confirm, got {reply:?}");
        };
        let req = deepseek_spawn(
            single_target(false, None),
            SessionMode::Interactive,
            rows,
            Some("rq-lk"),
        );
        let reply = dispatch_one(&hub, ClientMessage::SpawnSession(req)).await;
        let DaemonMessage::ActionFailed {
            detail, request_id, ..
        } = reply
        else {
            panic!("the pre-check refuses before the confirm, got {reply:?}");
        };
        assert!(detail.contains(expected), "{detail}");
        assert!(!detail.contains(SECRET_SENTINEL), "{detail}");
        assert_eq!(request_id.as_deref(), Some("rq-lk"));
        assert!(
            !Path::new(&config).join("env-secrets.json").exists(),
            "the pre-check sealed nothing"
        );
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
    }

    /// A value no stored copy of a sealed row may ever hold.
    const SECRET_SENTINEL: &str = "rt-secret-sentinel-3d8f1a";

    /// `key`'s value in a child env built by `providers::spawn_env`.
    fn child_value(child: &[(String, String)], key: &str) -> Option<String> {
        child.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    /// `text` holds no copy of the sentinel and does carry a reference.
    fn assert_sealed_only(text: &str, what: &str) {
        assert!(!text.contains(SECRET_SENTINEL), "{what}: {text}");
        assert!(text.contains("${secret:"), "{what}: {text}");
    }

    /// The `Repos` list of the next `StateEvent::Repos` on `rx`.
    #[expect(clippy::panic, reason = "a wrong event fails the test loudly")]
    fn next_repos(rx: &mut broadcast::Receiver<StateEvent>) -> Vec<protocol::RepoEntry> {
        loop {
            match rx.try_recv() {
                Ok(StateEvent::Repos(repos)) => return repos,
                Ok(_) => {}
                Err(err) => panic!("no repos broadcast arrived: {err}"),
            }
        }
    }

    /// An interactive spawn onto `target` whose `ANTHROPIC_API_KEY` row is the
    /// literal sentinel.
    fn secret_literal_spawn(target: SpawnTarget) -> SpawnRequest {
        env_spawn(
            target,
            SessionMode::Interactive,
            env(&[("ANTHROPIC_API_KEY", SECRET_SENTINEL)]),
            None,
        )
    }

    /// Register `id` as an interactive session whose record and sidecar carry
    /// `stored`, exactly as the spawn path's constructors leave them, and write
    /// the history entry its stop would.
    ///
    /// A spawn cannot run to completion under a test hub — it needs a real
    /// tracer — so a test that would otherwise read the record, the sidecar or
    /// the history off a live spawn feeds the config the pipeline captured
    /// (which is what the spawn hands these constructors) to the same
    /// constructors here.
    fn record_stopped_session(hub: &Hub, id: &str, stored: &protocol::SpawnConfig) {
        let record = interactive_record(
            NewInteractive {
                session_id: id.to_owned(),
                label: id.to_owned(),
                kind: SessionKind::Standalone,
                members: Vec::new(),
                workspace_id: None,
                agent: Agent::Claude,
                cwd: hub.dirs.config.to_string_lossy().into_owned(),
                stored_config: stored.clone(),
                last_prompt: None,
                claude_session_id: None,
                started_at: Utc::now(),
            },
            None,
        );
        let meta = interactive_meta(
            &record,
            4242,
            TracerSidecar {
                pid: 4243,
                pipe: r"\\.\pipe\rt-tracer-sidecar".to_owned(),
                exe_path: "rt-tracer.exe".to_owned(),
            },
        )
        .expect("sidecar meta");
        let entry = history::entry_from_record(&record, SessionEnd::StoppedByUser, Utc::now());
        hub.sessions.insert(record);
        hub.sessions.write_sidecar(&meta);
        history::write_if_absent(&hub.dirs, &entry).expect("write the history entry");
    }

    /// A spawn whose dialog row holds a literal secret. The spawnless hub
    /// cannot stage a tracer, so the spawn stops after the config is captured —
    /// which is the part this test is about.
    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn a_literal_secret_row_is_stored_as_a_reference_everywhere() {
        let (_lock, _secrets_dir) =
            env_secrets::test_support::scratch_async("seal-everywhere").await;
        let (hub, _scratch, _repo) = env_ref_repo_hub("seal-everywhere");
        let mut repos_rx = hub.state_events.subscribe();
        let mut target = single_target(false, None);
        if let SpawnTarget::Single { branch_name, .. } = &mut target {
            "main".clone_into(branch_name);
        }
        spawn_session(&hub, secret_literal_spawn(target), None)
            .await
            .expect_err("no tracer can start");

        let repos = serde_json::to_string(&next_repos(&mut repos_rx)).expect("serialize repos");
        assert_sealed_only(&repos, "the Repos broadcast");
        let state = std::fs::read_to_string(&hub.dirs.state_file).expect("state.json written");
        assert_sealed_only(&state, "state.json");

        let stored = hub
            .state
            .with_persisted(|s| s.repos[0].last_spawn_config.clone())
            .expect("last spawn config stored");
        assert_eq!(stored.extra_env[0].0, "ANTHROPIC_API_KEY");
        record_stopped_session(&hub, "sealed", &stored);
        let recorded = hub
            .sessions
            .get("sealed")
            .and_then(|rec| crate::sync::lock(&rec).spawn_config.clone())
            .expect("the record carries the config");
        assert_sealed_only(
            &serde_json::to_string(&recorded).expect("serialize config"),
            "the record's config",
        );

        let meta = std::fs::read_to_string(hub.dirs.sessions_dir.join("sealed").join("meta.json"))
            .expect("meta.json written");
        assert_sealed_only(&meta, "meta.json");
        let entry = std::fs::read_to_string(hub.dirs.history_dir().join("sealed.json"))
            .expect("the history entry written");
        assert_sealed_only(&entry, "the history entry");

        let reply = dispatch_one(
            &hub,
            ClientMessage::GetSpawnConfig {
                session_id: "sealed".to_owned(),
            },
        )
        .await;
        let DaemonMessage::SpawnConfigReply { config, .. } = reply else {
            panic!("expected spawn_config_reply, got {reply:?}");
        };
        assert_sealed_only(
            &serde_json::to_string(&config.expect("the record has a config")).expect("serialize"),
            "the GetSpawnConfig reply",
        );
    }

    #[tokio::test]
    async fn the_child_gets_the_sealed_value() {
        let (_lock, _secrets_dir) = env_secrets::test_support::scratch_async("seal-child").await;
        let (hub, _scratch, _repo) = env_ref_repo_hub("seal-child");
        let id = env_secrets::seal("ANTHROPIC_API_KEY", SECRET_SENTINEL).expect("seal the row");
        let mut target = single_target(false, None);
        if let SpawnTarget::Single { branch_name, .. } = &mut target {
            "main".clone_into(branch_name);
        }
        let req = env_spawn(
            target,
            SessionMode::Interactive,
            env(&[("ANTHROPIC_API_KEY", &secret_ref(&id))]),
            None,
        );
        let _ = child_env_probe::take();
        spawn_session(&hub, req, None)
            .await
            .expect_err("no tracer can start");
        let child = child_env_probe::take().expect("a spawn spec's env was built");
        assert_eq!(
            child_value(&child, "ANTHROPIC_API_KEY").as_deref(),
            Some(SECRET_SENTINEL),
            "a secret reference resolves to the value it stands for"
        );
    }

    /// A literal under a key that names no credential is left exactly as sent,
    /// and reaches the child unchanged. Nothing here seals, so this passes
    /// before ES.3 too: it pins the harm the pattern must not do.
    #[tokio::test]
    async fn a_plain_literal_row_is_stored_verbatim() {
        let (_lock, _secrets_dir) = env_secrets::test_support::scratch_async("seal-plain").await;
        let (hub, _scratch, _repo) = env_ref_repo_hub("seal-plain");
        let rows = env(&[("RUST_LOG", "debug"), ("RT_PLAIN", "not-a-secret")]);
        let mut target = single_target(false, None);
        if let SpawnTarget::Single { branch_name, .. } = &mut target {
            "main".clone_into(branch_name);
        }
        let _ = child_env_probe::take();
        spawn_session(
            &hub,
            env_spawn(target, SessionMode::Interactive, rows.clone(), None),
            None,
        )
        .await
        .expect_err("no tracer can start");

        let stored = hub
            .state
            .with_persisted(|s| s.repos[0].last_spawn_config.clone())
            .expect("last spawn config stored");
        assert_eq!(stored.extra_env, rows, "a plain literal is stored as sent");
        let child = child_env_probe::take().expect("a spawn spec's env was built");
        assert_eq!(
            child_value(&child, "RT_PLAIN").as_deref(),
            Some("not-a-secret")
        );
        assert_eq!(child_value(&child, "RUST_LOG").as_deref(), Some("debug"));
    }

    #[tokio::test]
    async fn a_secret_env_keys_row_is_sealed_though_its_name_is_plain() {
        let (_lock, _secrets_dir) = env_secrets::test_support::scratch_async("seal-toggled").await;
        let (hub, _scratch, _repo) = env_ref_repo_hub("seal-toggled");
        let mut target = single_target(false, None);
        if let SpawnTarget::Single { branch_name, .. } = &mut target {
            "main".clone_into(branch_name);
        }
        let mut req = env_spawn(
            target,
            SessionMode::Interactive,
            env(&[("GH_PAT", SECRET_SENTINEL)]),
            None,
        );
        req.secret_env_keys = vec!["GH_PAT".to_owned()];
        let _ = child_env_probe::take();
        spawn_session(&hub, req, None)
            .await
            .expect_err("no tracer can start");

        let stored = hub
            .state
            .with_persisted(|s| s.repos[0].last_spawn_config.clone())
            .expect("last spawn config stored");
        let (key, value) = &stored.extra_env[0];
        assert_eq!(key, "GH_PAT");
        assert!(value.starts_with("${secret:"), "{value}");
        assert!(!value.contains(SECRET_SENTINEL), "{value}");
        let child = child_env_probe::take().expect("a spawn spec's env was built");
        assert_eq!(
            child_value(&child, "GH_PAT").as_deref(),
            Some(SECRET_SENTINEL),
            "a toggled row is sealed and resolved like a pattern row"
        );
    }

    #[tokio::test]
    async fn duplicate_resume_and_recover_resolve_the_sealed_value() {
        let (_lock, _secrets_dir) = env_secrets::test_support::scratch_async("seal-replay").await;
        let (hub, scratch) = spawnless_test_hub("seal-replay");
        let folder = scratch.path().join("replay");
        std::fs::create_dir_all(&folder).expect("create folder");
        let folder = folder.to_string_lossy().into_owned();
        let id = env_secrets::seal("ANTHROPIC_API_KEY", SECRET_SENTINEL).expect("seal the row");
        let target = SpawnTarget::Standalone {
            cwd: Some(folder.clone()),
            add_dirs: Vec::new(),
        };
        let request = env_spawn(
            target.clone(),
            SessionMode::Interactive,
            env(&[("ANTHROPIC_API_KEY", &secret_ref(&id))]),
            None,
        );
        let stored = protocol::SpawnConfig::from_request(&request);
        let (out_tx, _out_rx) = mpsc::unbounded_channel::<DaemonMessage>();

        let mut source = idle_record("source");
        source.spawn_config = Some(stored.clone());
        hub.sessions.insert(source);
        let _ = child_env_probe::take();
        let ask = DuplicateAsk {
            session_id: "source",
            request_id: None,
            carry_busy_excluded: false,
        };
        duplicate_session(&hub, ask, 1, &out_tx).await;
        assert_eq!(
            child_value(
                &child_env_probe::take().expect("the duplicate built an env"),
                "ANTHROPIC_API_KEY"
            )
            .as_deref(),
            Some(SECRET_SENTINEL),
            "a duplicate resolves the reference the record holds"
        );

        let mut abandoned = idle_record("abandoned");
        abandoned.spawn_config = Some(stored.clone());
        abandoned.is_abandoned = true;
        abandoned.status = protocol::SessionStatus::Stopped;
        hub.sessions.insert(abandoned);
        let _ = child_env_probe::take();
        resume_abandoned(&hub, "abandoned", &out_tx)
            .await
            .expect_err("no tracer can start");
        assert_eq!(
            child_value(
                &child_env_probe::take().expect("the resume built an env"),
                "ANTHROPIC_API_KEY"
            )
            .as_deref(),
            Some(SECRET_SENTINEL),
            "a resume resolves the reference the record holds"
        );
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn a_missing_secret_refuses_before_any_git_work() {
        let (_lock, _secrets_dir) = env_secrets::test_support::scratch_async("seal-missing").await;
        let (hub, _scratch, repo) = env_ref_repo_hub("seal-missing");
        let req = env_spawn(
            single_target(true, None),
            SessionMode::Interactive,
            env(&[("ANTHROPIC_API_KEY", &secret_ref(MISSING_SECRET_ID))]),
            Some("rq-secret"),
        );
        // Straight into `spawn_session`, the path every replay takes, past the
        // dispatcher's own early check.
        let err = spawn_session(&hub, req, None)
            .await
            .expect_err("a missing saved secret refuses");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        send_spawn_failure(&out_tx, &err, Some("rq-secret"));
        let DaemonMessage::ActionFailed {
            title,
            detail,
            hint,
            request_id,
        } = out_rx.try_recv().expect("a reply")
        else {
            panic!("expected action_failed");
        };
        assert_eq!(title, "A saved secret is missing");
        assert!(detail.contains("`ANTHROPIC_API_KEY`"), "{detail}");
        assert!(detail.contains("Windows Credential Manager"), "{detail}");
        assert!(!detail.contains(SECRET_SENTINEL), "{detail}");
        assert!(hint.is_some());
        assert_eq!(request_id.as_deref(), Some("rq-secret"));

        let worktrees = git_ok(&repo, &["worktree", "list", "--porcelain"]);
        assert_eq!(
            worktrees
                .lines()
                .filter(|l| l.starts_with("worktree "))
                .count(),
            1,
            "only the main worktree: {worktrees}"
        );
        assert_eq!(git_ok(&repo, &["branch", "--list", "wt/x"]).trim(), "");
        let made = std::fs::read_dir(hub.state.worktrees_dir()).map_or(0, Iterator::count);
        assert_eq!(made, 0, "nothing under the worktrees root");
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
        assert!(
            hub.state
                .with_persisted(|s| s.repos[0].last_spawn_config.is_none()),
            "no config was captured either"
        );
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn a_missing_secret_is_refused_before_the_checkout_confirm() {
        let (_lock, _secrets_dir) =
            env_secrets::test_support::scratch_async("seal-missing-confirm").await;
        let (hub, _scratch, repo) = env_ref_repo_hub("seal-missing-confirm");
        std::fs::write(repo.join("dirty.txt"), "x\n").expect("dirty the tree");
        let req = env_spawn(
            single_target(false, None),
            SessionMode::Interactive,
            env(&[("ANTHROPIC_API_KEY", &secret_ref(MISSING_SECRET_ID))]),
            Some("rq-secret"),
        );
        let reply = dispatch_one(&hub, ClientMessage::SpawnSession(req)).await;
        let DaemonMessage::ActionFailed { title, detail, .. } = reply else {
            panic!("the missing secret is refused before the confirm, got {reply:?}");
        };
        assert_eq!(title, "A saved secret is missing");
        assert!(detail.contains("`ANTHROPIC_API_KEY`"), "{detail}");
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
    }

    #[tokio::test]
    #[expect(clippy::panic, reason = "a wrong reply fails the test loudly")]
    async fn an_over_long_secret_is_refused_before_the_checkout_confirm() {
        let (_lock, _secrets_dir) =
            env_secrets::test_support::scratch_async("seal-too-long-confirm").await;
        let (hub, _scratch, repo) = env_ref_repo_hub("seal-too-long-confirm");
        std::fs::write(repo.join("dirty.txt"), "x\n").expect("dirty the tree");
        // One UTF-16 code unit over what Windows Credential Manager takes in
        // one credential blob.
        let too_long = "a".repeat(1281);
        let req = env_spawn(
            single_target(false, None),
            SessionMode::Interactive,
            env(&[("ANTHROPIC_API_KEY", &too_long)]),
            Some("rq-long"),
        );
        let reply = dispatch_one(&hub, ClientMessage::SpawnSession(req)).await;
        let DaemonMessage::ActionFailed {
            title,
            detail,
            hint,
            request_id,
        } = reply
        else {
            panic!("the over-long secret is refused before the confirm, got {reply:?}");
        };
        assert_eq!(title, "Could not save a secret");
        assert!(detail.contains("ANTHROPIC_API_KEY"), "{detail}");
        assert!(detail.contains("too long"), "{detail}");
        assert!(
            !detail.contains(&too_long),
            "the refusal never carries the value: {detail}"
        );
        assert!(hint.is_none());
        assert_eq!(request_id.as_deref(), Some("rq-long"));
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
    }

    #[tokio::test]
    async fn standalone_target_refuses_headless() {
        let (hub, _scratch) = spawnless_test_hub("standalone-agents");
        for (mode, agent_options) in [(SessionMode::Headless, AgentOptions::claude(None))] {
            let req = SpawnRequest {
                label: None,
                target: SpawnTarget::Standalone {
                    cwd: None,
                    add_dirs: Vec::new(),
                },
                mode,
                initial_prompt: Some("x".to_owned()),
                dangerously_skip_permissions: false,
                agent_options,
                model: None,
                extra_env: Vec::new(),
                secret_env_keys: Vec::new(),
                prompt_injector: None,
                request_id: None,
                resume_conversation: None,
            };
            let err = spawn_session(&hub, req, None)
                .await
                .expect_err("refused before anything spawns");
            assert!(
                err.to_string().contains("standalone targets only support"),
                "{err:#}"
            );
        }
        for &agent in Agent::all() {
            assert!(standalone_supports(SessionMode::Interactive, agent));
            assert!(standalone_supports(SessionMode::PlainShell, agent));
            assert!(!standalone_supports(SessionMode::Headless, agent));
        }
    }

    /// Where an interactive `agent_options` spawn in `folder` stops on the
    /// spawnless hub, as the error's whole chain.
    async fn standalone_agent_spawn_error(
        hub: &Hub,
        agent_options: AgentOptions,
        folder: &Path,
    ) -> String {
        let mut req = env_spawn(
            SpawnTarget::Standalone {
                cwd: Some(folder.to_string_lossy().into_owned()),
                add_dirs: Vec::new(),
            },
            SessionMode::Interactive,
            Vec::new(),
            None,
        );
        req.agent_options = agent_options;
        let err = spawn_session(hub, req, None)
            .await
            .expect_err("the spawnless hub launches nothing");
        format!("{err:#}")
    }

    /// A standalone agent in `folder` runs there, with no members and so no
    /// worktree.
    fn assert_standalone_agent_runs_in(folder: &Path) {
        let (kind, members, cwd, label, workspace_id) =
            spawn_standalone_agent(Some(&folder.to_string_lossy())).expect("resolve the folder");
        let canonical = std::fs::canonicalize(folder).expect("canonicalize the folder");
        assert_eq!(kind, SessionKind::Standalone);
        assert!(members.is_empty(), "{members:?}");
        assert_eq!(cwd, crate::paths::simplify_path(&canonical));
        assert_eq!(label, path_leaf_label(folder));
        assert_eq!(workspace_id, None);
    }

    #[tokio::test]
    async fn standalone_target_spawns_interactive_claude_in_the_folder() {
        let (hub, scratch) = spawnless_test_hub("standalone-claude");
        let folder = scratch.path().join("plain-claude");
        std::fs::create_dir_all(&folder).expect("create folder");
        let error = standalone_agent_spawn_error(&hub, AgentOptions::claude(None), &folder).await;
        assert!(error.contains("spawning claude via tracer"), "{error}");
        assert_standalone_agent_runs_in(&folder);
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
    }

    #[tokio::test]
    async fn standalone_claude_in_a_missing_folder_fails_with_folder_not_found() {
        let (hub, scratch) = spawnless_test_hub("standalone-claude-missing");
        let folder = scratch.path().join("gone");
        let error = standalone_agent_spawn_error(&hub, AgentOptions::claude(None), &folder).await;
        assert!(error.contains("reading the standalone folder"), "{error}");
        assert!(error.contains(&*folder.to_string_lossy()), "{error}");
    }

    fn accent(color: &str) -> AppearanceOverrides {
        AppearanceOverrides {
            accent_color: Some(color.to_string()),
            ..AppearanceOverrides::default()
        }
    }

    /// The accent and `request_id` of each of the next `count` messages on
    /// `rx`, which must all be `SessionUpdated`s; then checks nothing follows.
    async fn next_updates(
        rx: &mut mpsc::UnboundedReceiver<DaemonMessage>,
        count: usize,
    ) -> Vec<(Option<String>, Option<String>)> {
        let mut updates = Vec::new();
        for _ in 0..count {
            let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("the forwarder delivers the update")
                .expect("the channel is open");
            let update = match msg {
                DaemonMessage::SessionUpdated {
                    session,
                    request_id,
                } => Some((session.appearance.accent_color, request_id)),
                _ => None,
            };
            updates.push(update.expect("only SessionUpdated messages arrive"));
        }
        let extra = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(extra.is_err(), "one message per update: {extra:?}");
        updates
    }

    #[tokio::test]
    async fn session_appearance_echoes_the_request_id_in_order_to_the_requester_only() {
        let (sessions, _dirs) = appearance_registry("echo");
        let (out_tx, mut requester) = mpsc::unbounded_channel();
        let (other_tx, mut other) = mpsc::unbounded_channel();
        let forwarders = [
            spawn_session_forwarder(Arc::clone(&sessions), out_tx.clone(), 1),
            spawn_session_forwarder(Arc::clone(&sessions), other_tx, 2),
        ];

        sessions.update("s1", |guard| {
            guard.terminal_title = Some("busy".to_string());
        });
        for (color, id) in [("#38BDF8", "req-a"), ("#FB7185", "req-b")] {
            let change = AppearanceChangeRequest {
                session_id: "s1",
                appearance: &accent(color),
                request_id: Some(id.to_string()),
                connection: 1,
            };
            set_session_appearance(&sessions, change, &out_tx);
        }

        let sky = Some("#38bdf8".to_string());
        let rose = Some("#fb7185".to_string());
        assert_eq!(
            next_updates(&mut requester, 3).await,
            [
                (None, None),
                (sky.clone(), Some("req-a".to_string())),
                (rose.clone(), Some("req-b".to_string())),
            ],
            "the requester's echoes follow the earlier broadcast, once each"
        );
        assert_eq!(
            next_updates(&mut other, 3).await,
            [(None, None), (sky, None), (rose, None)],
            "another connection sees the same updates without the id"
        );
        for forwarder in forwarders {
            forwarder.abort();
        }
    }

    #[tokio::test]
    async fn a_spawn_reply_reaches_the_requester_once_with_its_request_id() {
        let (sessions, _dirs) = appearance_registry("spawn-reply");
        let (requester_tx, mut requester) = mpsc::unbounded_channel();
        let (other_tx, mut other) = mpsc::unbounded_channel();
        let forwarders = [
            spawn_session_forwarder(Arc::clone(&sessions), requester_tx, 1),
            spawn_session_forwarder(Arc::clone(&sessions), other_tx, 2),
        ];

        // A spawn needs a real child, so this drives the seam the handler
        // uses: the origin it builds, published the way the interactive and
        // plain-shell spawn paths publish their record.
        let pending = sessions.insert_pending(idle_record("s2"));
        pending.publish_from(request_origin(1, Some("r1".to_string())));

        assert_eq!(
            only_update(&mut requester).await,
            ("s2".to_string(), Some("r1".to_string())),
            "the requester hears of its spawn once, with its id"
        );
        assert_eq!(
            only_update(&mut other).await,
            ("s2".to_string(), None),
            "another connection hears of it once, without the id"
        );
        for forwarder in forwarders {
            forwarder.abort();
        }
    }

    #[tokio::test]
    async fn a_duplicate_reply_reaches_the_requester_once_with_its_request_id() {
        let (sessions, _dirs) = appearance_registry("duplicate-reply");
        let (other_tx, mut other) = mpsc::unbounded_channel();
        let (requester_tx, mut requester) = mpsc::unbounded_channel();
        let forwarders = [
            spawn_session_forwarder(Arc::clone(&sessions), other_tx, 1),
            spawn_session_forwarder(Arc::clone(&sessions), requester_tx, 2),
        ];

        // The duplicate's origin, published the way the headless spawn path
        // inserts its record.
        sessions.insert_from(idle_record("s2"), request_origin(2, Some("d1".to_string())));
        assert_eq!(
            only_update(&mut requester).await,
            ("s2".to_string(), Some("d1".to_string())),
            "the requester hears of its duplicate once, with its id"
        );
        assert_eq!(only_update(&mut other).await, ("s2".to_string(), None));

        sessions.insert_from(idle_record("s3"), request_origin(2, None));
        assert_eq!(
            only_update(&mut requester).await,
            ("s3".to_string(), None),
            "a request without an id has nothing to echo"
        );
        assert_eq!(only_update(&mut other).await, ("s3".to_string(), None));
        for forwarder in forwarders {
            forwarder.abort();
        }
    }

    #[tokio::test]
    async fn a_lagged_forwarder_resyncs_with_a_sessions_list() {
        let (sessions, _dirs) = appearance_registry("lagged");
        let (out_tx, mut rx) = mpsc::unbounded_channel();
        let forwarder = spawn_session_forwarder(Arc::clone(&sessions), out_tx, 1);

        // The current-thread runtime first polls the forwarder at the await
        // below, by which time the broadcast buffer has overflowed.
        for step in 0..=crate::session::EVENT_BROADCAST_CAPACITY {
            sessions.update("s1", |guard| {
                guard.terminal_title = Some(format!("step {step}"));
            });
        }
        let listed = match next_message(&mut rx).await {
            DaemonMessage::Sessions { sessions } => Some(sessions),
            _ => None,
        };
        assert_eq!(
            listed.expect("the lagged connection gets the session list"),
            sessions.snapshots(),
            "the list is the registry as it stands"
        );

        sessions.update("s1", |guard| {
            guard.terminal_title = Some("after".to_string());
        });
        let title = match next_message(&mut rx).await {
            DaemonMessage::SessionUpdated { session, .. } => Some(session.terminal_title),
            _ => None,
        };
        assert_eq!(
            title.expect("updates after the list still arrive"),
            Some("after".to_string()),
            "the missed updates are not replayed after the list"
        );
        forwarder.abort();
    }

    #[tokio::test]
    async fn a_lag_resends_the_spawn_reply_to_its_requester() {
        let (sessions, _dirs) = appearance_registry("lag-reply");
        let (out_tx, mut rx) = mpsc::unbounded_channel();
        let forwarder = spawn_session_forwarder(Arc::clone(&sessions), out_tx, 1);

        // Connection 1 spawns s2 and connection 2 spawns s3; the updates
        // after them push both replies out of connection 1's buffer.
        sessions
            .insert_pending(idle_record("s2"))
            .publish_from(request_origin(1, Some("r1".to_string())));
        sessions.insert_from(idle_record("s3"), request_origin(2, Some("r2".to_string())));
        for step in 0..=crate::session::EVENT_BROADCAST_CAPACITY {
            sessions.update("s1", |guard| {
                guard.terminal_title = Some(format!("step {step}"));
            });
        }

        let listed = match next_message(&mut rx).await {
            DaemonMessage::Sessions { sessions } => Some(sessions.len()),
            _ => None,
        };
        assert_eq!(listed, Some(3), "the resync lists every session first");
        assert_eq!(
            only_update(&mut rx).await,
            ("s2".to_string(), Some("r1".to_string())),
            "the lost reply follows the list, to its requester only"
        );
        forwarder.abort();
    }

    #[test]
    fn a_discarded_pending_insert_leaves_no_trace() {
        let (sessions, _dirs) = appearance_registry("discard");
        let mut events = sessions.subscribe();

        sessions.insert_pending(idle_record("s2")).discard();

        assert!(sessions.get("s2").is_none(), "the record is gone");
        assert!(
            matches!(
                events.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "no client ever hears of a record that was never published"
        );
    }

    #[test]
    fn a_session_appearance_for_an_unknown_session_echoes_the_request_id_on_the_error() {
        let (sessions, _dirs) = appearance_registry("unknown");
        let mut other_client = sessions.subscribe();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let change = AppearanceChangeRequest {
            session_id: "gone",
            appearance: &accent("#FB7185"),
            request_id: Some("req-11".to_string()),
            connection: 1,
        };

        set_session_appearance(&sessions, change, &out_tx);

        let reply = out_rx.try_recv().expect("the requester hears of the miss");
        assert!(
            matches!(
                &reply,
                DaemonMessage::Error { message, request_id: Some(id) }
                    if id == "req-11" && message == "unknown session: gone"
            ),
            "{reply:?}"
        );
        assert!(other_client.try_recv().is_err(), "nothing changed");
    }

    #[test]
    fn a_refused_session_appearance_echoes_the_request_id_on_the_error() {
        let (sessions, _dirs) = appearance_registry("refused");
        let mut other_client = sessions.subscribe();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let change = AppearanceChangeRequest {
            session_id: "s1",
            appearance: &accent("blue"),
            request_id: Some("req-10".to_string()),
            connection: 1,
        };

        set_session_appearance(&sessions, change, &out_tx);

        let reply = out_rx.try_recv().expect("the requester hears the refusal");
        assert!(
            matches!(
                &reply,
                DaemonMessage::Error { message, request_id: Some(id) }
                    if id == "req-10" && message == "accent color must use #RRGGBB format"
            ),
            "{reply:?}"
        );
        assert!(other_client.try_recv().is_err(), "nothing changed");
    }

    fn single_target(use_worktree: bool, existing_worktree: Option<&str>) -> SpawnTarget {
        SpawnTarget::Single {
            repo_id: "r1".to_string(),
            branch_name: "wt/x".to_string(),
            base_branch: None,
            use_worktree,
            checkout_strategy: None,
            worktree_reuse: protocol::WorktreeReusePolicy::Reuse,
            existing_worktree: existing_worktree.map(str::to_string),
        }
    }

    #[test]
    fn checkout_confirm_echoes_the_spawn_request_id() {
        let reply = checkout_confirm_reply("r1", "wt/x", Some("req-9"), 4);
        assert!(
            matches!(
                &reply,
                DaemonMessage::CheckoutConfirmRequired {
                    repo_id,
                    branch,
                    dirty_count: 4,
                    request_id: Some(id),
                } if repo_id == "r1" && branch == "wt/x" && id == "req-9"
            ),
            "{reply:?}"
        );
        let reply = checkout_confirm_reply("r1", "wt/x", None, 4);
        assert!(
            matches!(
                &reply,
                DaemonMessage::CheckoutConfirmRequired {
                    request_id: None,
                    ..
                }
            ),
            "a spawn without an id gets a prompt without one: {reply:?}"
        );
    }

    #[test]
    fn pinning_an_in_place_single_spawn_is_refused() {
        // In-place runs in the repo; a pin names a different directory. Doing
        // either silently would put the session somewhere unchosen.
        let err = reject_pin_without_worktree(&single_target(false, Some("X:/wt/repo")))
            .expect_err("pin + in-place must not be accepted");
        assert!(
            format!("{err:#}").contains("cannot pin an existing worktree"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn pinning_a_worktree_spawn_is_allowed() {
        reject_pin_without_worktree(&single_target(true, Some("X:/wt/repo")))
            .expect("a pin with use_worktree is the supported combination");
    }

    #[test]
    fn an_unpinned_in_place_spawn_is_allowed() {
        reject_pin_without_worktree(&single_target(false, None))
            .expect("in-place without a pin is the ordinary case");
    }

    #[test]
    fn pinning_an_in_place_workspace_spawn_is_refused() {
        let target = SpawnTarget::Workspace {
            workspace_id: "ws1".to_string(),
            branch_name: "wt/x".to_string(),
            base_branch: None,
            use_worktree: false,
            worktree_reuse: protocol::WorktreeReusePolicy::Reuse,
            existing_worktrees: vec![protocol::PinnedMemberWorktree {
                repo_id: "r1".to_string(),
                path: "X:/wt/repo".to_string(),
            }],
        };
        reject_pin_without_worktree(&target).expect_err("pin + in-place must not be accepted");
    }

    // Per-agent argv builders + workspace-prelude tests live in the
    // `crate::agents` submodules now.

    mod refuse_leftover {
        use super::super::materialize_single_worktree;
        use crate::spawn_plan::SpawnFailure;
        use std::path::{Path, PathBuf};
        use std::process::Command;

        fn git(cwd: &Path, args: &[&str]) {
            let out = Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .output()
                .expect("spawn git");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        fn commit(repo: &Path, name: &str) {
            std::fs::write(repo.join(name), "x\n").expect("write file");
            git(repo, &["add", "."]);
            git(repo, &["commit", "-m", name]);
        }

        fn rev(repo: &Path, refname: &str) -> String {
            let out = Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["rev-parse", refname])
                .output()
                .expect("spawn git");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        /// A repo where `wt/x` sits at the old `main` tip with no worktree
        /// attached — what "close session and delete worktree" leaves behind
        /// — while `release` stands in for a base that moved on.
        fn seed_branch_only_leftover(tag: &str) -> (PathBuf, PathBuf) {
            let root = std::env::temp_dir().join(format!("rt-refuse-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let repo = root.join("repo");
            std::fs::create_dir_all(&repo).expect("create repo dir");
            git(&repo, &["init", "-b", "main"]);
            git(&repo, &["config", "user.email", "t@example.com"]);
            git(&repo, &["config", "user.name", "Test"]);
            git(&repo, &["config", "commit.gpgsign", "false"]);
            commit(&repo, "seed.txt");
            git(&repo, &["branch", "wt/x"]);
            git(&repo, &["checkout", "-b", "release"]);
            commit(&repo, "landed.txt");
            git(&repo, &["checkout", "main"]);
            (root, repo)
        }

        fn refusal_title(err: &anyhow::Error) -> String {
            err.downcast_ref::<SpawnFailure>().map_or_else(
                || format!("not a SpawnFailure: {err:#}"),
                |failure| failure.title.clone(),
            )
        }

        /// The trap this policy exists for: a branch a discarded session left
        /// behind must stop the spawn, not get attached at its stale tip.
        #[tokio::test]
        async fn a_branch_only_leftover_is_refused_without_creating_anything() {
            let (root, repo) = seed_branch_only_leftover("branch-only");
            let worktree = root.join("wt");

            let err = materialize_single_worktree(
                "fixture",
                &repo,
                &worktree,
                "wt/x",
                "release",
                protocol::WorktreeReusePolicy::RefuseLeftover,
            )
            .await
            .expect_err("a leftover branch must refuse the spawn");

            assert_eq!(refusal_title(&err), "Leftover branch in the way");
            assert!(!worktree.exists(), "no worktree may be created");
            let listed = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["worktree", "list"])
                .output()
                .expect("spawn git");
            let listed = String::from_utf8_lossy(&listed.stdout);
            assert!(
                !listed.contains("wt/x"),
                "the leftover branch must still have no worktree: {listed}"
            );

            let _ = std::fs::remove_dir_all(&root);
        }

        #[tokio::test]
        async fn a_leftover_worktree_directory_is_refused() {
            let (root, repo) = seed_branch_only_leftover("dir");
            let worktree = root.join("wt");
            git(
                &repo,
                &["worktree", "add", &worktree.to_string_lossy(), "wt/x"],
            );
            let before = rev(&worktree, "HEAD");

            let err = materialize_single_worktree(
                "fixture",
                &repo,
                &worktree,
                "wt/x",
                "release",
                protocol::WorktreeReusePolicy::RefuseLeftover,
            )
            .await
            .expect_err("a leftover worktree must refuse the spawn");

            assert_eq!(refusal_title(&err), "Leftover branch in the way");
            assert_eq!(
                rev(&worktree, "HEAD"),
                before,
                "the refusal must not touch the existing worktree"
            );

            let _ = std::fs::remove_dir_all(&root);
        }

        /// Refusing is only about leftovers: a name nothing has claimed still
        /// gets its worktree, forked from the resolved base.
        #[tokio::test]
        async fn a_clean_name_is_created_from_the_base() {
            let (root, repo) = seed_branch_only_leftover("clean");
            let worktree = root.join("wt-fresh");

            materialize_single_worktree(
                "fixture",
                &repo,
                &worktree,
                "wt/fresh",
                "release",
                protocol::WorktreeReusePolicy::RefuseLeftover,
            )
            .await
            .expect("a free name must still be created");

            assert_eq!(
                rev(&worktree, "HEAD"),
                rev(&repo, "release"),
                "a fresh worktree must fork from the resolved base"
            );

            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[cfg(windows)]
    mod npm_shim {
        use super::super::npm_shim_to_native_program;
        use std::fs;
        use std::path::{Path, PathBuf};
        use uuid::Uuid;

        /// RAII scratch dir under the OS temp root (drop removes the tree).
        /// Mirrors the `Scratch` helper in `binary_cache.rs` rather than
        /// pulling in `tempfile` just for tests.
        struct Scratch {
            path: PathBuf,
        }

        impl Scratch {
            fn new(label: &str) -> Self {
                let path = std::env::temp_dir()
                    .join(format!("rt-npm-shim-{label}-{}", Uuid::new_v4().simple()));
                fs::create_dir_all(&path).unwrap();
                Self { path }
            }

            fn path(&self) -> &Path {
                &self.path
            }
        }

        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.path);
            }
        }

        fn touch(path: &Path) {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, b"").unwrap();
        }

        /// Mirrors the `@anthropic-ai/claude-code` native-binary layout
        /// (the package that triggered the original bug — the resolver
        /// fell through to `cmd.exe /d /c claude.cmd` because it only
        /// knew about `bin/<stem>.js`, and the portable-pty + cmd.exe
        /// boundary then swallowed claude's stdout).
        #[test]
        fn scoped_native_exe_is_preferred_over_js() {
            let scratch = Scratch::new("scoped-exe");
            let dir = scratch.path();
            let shim = dir.join("claude.cmd");
            touch(&shim);
            let pkg = dir
                .join("node_modules")
                .join("@anthropic-ai")
                .join("claude-code")
                .join("bin");
            let exe = pkg.join("claude.exe");
            let js = pkg.join("claude.js");
            touch(&exe);
            touch(&js);

            let (program, prepend) =
                npm_shim_to_native_program(&shim).expect("resolver should find the native exe");
            assert_eq!(program, exe);
            assert!(
                prepend.is_empty(),
                "native exe should not need a node prefix, got {prepend:?}"
            );
        }

        /// Older claude installs' layout: no native exe, just the JS
        /// entry. Resolver should fall back to running it under
        /// `node.exe`.
        #[test]
        fn scoped_js_entry_resolved_via_node() {
            let scratch = Scratch::new("scoped-js");
            let dir = scratch.path();
            let shim = dir.join("claude.cmd");
            touch(&shim);
            let js = dir
                .join("node_modules")
                .join("@anthropic-ai")
                .join("claude-code")
                .join("bin")
                .join("claude.js");
            touch(&js);
            // Sibling node.exe so the test doesn't depend on a
            // PATH-resolved `node` being installed on the runner.
            let sibling_node = dir.join("node.exe");
            touch(&sibling_node);

            let (program, prepend) = npm_shim_to_native_program(&shim)
                .expect("resolver should fall back to node + js entry");
            assert_eq!(program, sibling_node);
            assert_eq!(prepend.len(), 1, "expected single js arg, got {prepend:?}");
            assert_eq!(PathBuf::from(&prepend[0]), js);
        }

        #[test]
        fn unscoped_native_exe_is_resolved() {
            let scratch = Scratch::new("unscoped-exe");
            let dir = scratch.path();
            let shim = dir.join("mytool.cmd");
            touch(&shim);
            let exe = dir
                .join("node_modules")
                .join("mytool")
                .join("bin")
                .join("mytool.exe");
            touch(&exe);

            let (program, prepend) = npm_shim_to_native_program(&shim)
                .expect("resolver should find unscoped native exe");
            assert_eq!(program, exe);
            assert_eq!(prepend, [] as [std::string::String; 0]);
        }

        /// A `.cmd` that isn't an npm shim (no sibling `node_modules`)
        /// should return `None` so the caller can fall back to the
        /// `cmd.exe /d /c <shim>` path.
        #[test]
        fn non_npm_shim_returns_none() {
            let scratch = Scratch::new("non-npm");
            let shim = scratch.path().join("randomtool.cmd");
            touch(&shim);
            assert!(npm_shim_to_native_program(&shim).is_none());
        }
    }

    mod worktree_layout {
        use super::*;
        use std::process::Command;

        fn git(cwd: &Path, args: &[&str]) {
            let out = Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .output()
                .expect("spawn git");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        /// A repo at `dir` with one commit on `main`.
        fn seed_repo(dir: &Path) {
            std::fs::create_dir_all(dir).expect("create repo dir");
            git(dir, &["init", "-b", "main"]);
            git(dir, &["config", "user.email", "t@example.com"]);
            git(dir, &["config", "user.name", "Test"]);
            git(dir, &["config", "commit.gpgsign", "false"]);
            std::fs::write(dir.join("seed.txt"), "x\n").expect("write seed file");
            git(dir, &["add", "."]);
            git(dir, &["commit", "-m", "seed"]);
        }

        fn key(path: &str) -> String {
            crate::paths::normalize_path_key(path)
        }

        fn single_preview(msg: DaemonMessage) -> Option<protocol::MemberSpawnPreview> {
            match msg {
                DaemonMessage::SpawnPreview { preview, .. } => Some(preview),
                _ => None,
            }
        }

        fn workspace_previews(msg: DaemonMessage) -> Option<Vec<protocol::MemberSpawnPreview>> {
            match msg {
                DaemonMessage::WorkspaceSpawnPreview { per_member, .. } => Some(per_member),
                _ => None,
            }
        }

        async fn preview_single(hub: &Hub, branch: &str) -> protocol::MemberSpawnPreview {
            let msg = dispatch_one(
                hub,
                ClientMessage::PreviewSpawn {
                    repo_id: "r1".to_string(),
                    branch_name: branch.to_string(),
                    base_branch: None,
                    use_worktree: true,
                    request_id: None,
                },
            )
            .await;
            single_preview(msg).expect("a spawn preview")
        }

        /// A branch already checked out in a worktree under the root (here
        /// at the older anchor layout) is reused where it is; a branch nobody
        /// holds gets the new `wt.<branch>/<repo>` folder.
        #[tokio::test]
        async fn single_spawn_reuses_an_old_layout_worktree_holding_the_branch() {
            let (hub, scratch) = test_hub("reuse-single");
            let repo = scratch.path().join("src").join("app");
            seed_repo(&repo);
            hub.state
                .mutate(|s| s.repos.push(repo_entry("r1", "App", &repo)))
                .expect("register repo");
            let root = hub.state.worktrees_dir();
            let old = root.join("wt.feat").join("X").join("src").join("app");
            git(
                &repo,
                &["worktree", "add", "-b", "feat", &old.to_string_lossy()],
            );

            let held = preview_single(&hub, "feat").await;
            assert_eq!(key(&held.worktree_path), key(&old.to_string_lossy()));
            assert!(
                held.worktree_exists,
                "the held worktree is reported as existing"
            );

            let fresh = preview_single(&hub, "fresh").await;
            assert_eq!(
                key(&fresh.worktree_path),
                key(&root.join("wt.fresh").join("app").to_string_lossy())
            );
            assert!(!fresh.worktree_exists);
        }

        /// A workspace member whose branch sits in an old-layout worktree
        /// keeps it; a member with no worktree for the branch goes to the new
        /// `wt.<branch>/<workspace>/<offset>` path.
        #[tokio::test]
        async fn workspace_reuses_old_layout_member_worktrees() {
            let (hub, scratch) = test_hub("reuse-ws");
            let web = scratch.path().join("src").join("apps").join("web");
            let core = scratch.path().join("src").join("libs").join("core");
            seed_repo(&web);
            seed_repo(&core);
            hub.state
                .mutate(|s| {
                    s.repos.push(repo_entry("web", "web", &web));
                    s.repos.push(repo_entry("core", "core", &core));
                    s.workspaces
                        .push(workspace_entry("w1", "Shop", &["web", "core"]));
                })
                .expect("register workspace");
            let root = hub.state.worktrees_dir();
            let old_web = root
                .join("wt.feat")
                .join("D")
                .join("src")
                .join("apps")
                .join("web");
            git(
                &web,
                &["worktree", "add", "-b", "feat", &old_web.to_string_lossy()],
            );

            let msg = dispatch_one(
                &hub,
                ClientMessage::PreviewWorkspaceSpawn {
                    workspace_id: "w1".to_string(),
                    branch_name: "feat".to_string(),
                    base_branch: None,
                    request_id: None,
                },
            )
            .await;
            let rows = workspace_previews(msg).expect("a workspace spawn preview");
            assert_eq!(rows.len(), 2);
            assert_eq!(key(&rows[0].worktree_path), key(&old_web.to_string_lossy()));
            assert!(rows[0].worktree_exists);
            let new_core = root.join("wt.feat").join("shop").join("libs").join("core");
            assert_eq!(
                key(&rows[1].worktree_path),
                key(&new_core.to_string_lossy())
            );
            assert!(!rows[1].worktree_exists);
        }

        fn foreign_folder_message(path: &Path, repo_name: &str) -> String {
            format!(
                "Can't use the worktree folder {}: it exists but isn't a worktree of {repo_name}. Remove it from Manage worktrees, or rename the repo or workspace.",
                path.display()
            )
        }

        /// A registered repo `App` plus another repo whose worktree already
        /// sits at App's derived `wt.feat/app` folder.
        fn seed_foreign_single(hub: &Hub, scratch: &Path) -> PathBuf {
            let repo = scratch.join("src").join("app");
            let other = scratch.join("elsewhere").join("app");
            seed_repo(&repo);
            seed_repo(&other);
            hub.state
                .mutate(|s| s.repos.push(repo_entry("r1", "App", &repo)))
                .expect("register repo");
            let derived = hub.state.worktrees_dir().join("wt.feat").join("app");
            git(
                &other,
                &["worktree", "add", "-b", "feat", &derived.to_string_lossy()],
            );
            derived
        }

        #[tokio::test]
        async fn spawn_refuses_a_derived_folder_that_is_not_this_repos_worktree() {
            let (hub, scratch) = test_hub("foreign-spawn");
            let derived = seed_foreign_single(&hub, scratch.path());

            let err = spawn_single(
                &hub,
                "r1",
                SinglePlan {
                    branch_name: "feat",
                    base_branch: None,
                    use_worktree: true,
                    checkout_strategy: None,
                    worktree_reuse: protocol::WorktreeReusePolicy::Reuse,
                    existing_worktree: None,
                },
            )
            .await
            .expect_err("the spawn is refused");

            assert_eq!(err.to_string(), foreign_folder_message(&derived, "App"));
            assert!(
                derived.join("seed.txt").is_file(),
                "the other repo's worktree is left alone"
            );
        }

        #[tokio::test]
        async fn preview_reports_a_foreign_folder_as_an_error() {
            let (hub, scratch) = test_hub("foreign-preview");
            let derived = seed_foreign_single(&hub, scratch.path());

            let msg = dispatch_one(
                &hub,
                ClientMessage::PreviewSpawn {
                    repo_id: "r1".to_string(),
                    branch_name: "feat".to_string(),
                    base_branch: None,
                    use_worktree: true,
                    request_id: Some("p1".to_string()),
                },
            )
            .await;

            let expected = foreign_folder_message(&derived, "App");
            assert!(
                matches!(
                    &msg,
                    DaemonMessage::Error { message, request_id: Some(id) }
                        if id == "p1" && *message == expected
                ),
                "{msg:?}"
            );
        }

        #[tokio::test]
        async fn workspace_member_refuses_a_foreign_folder() {
            let (hub, scratch) = test_hub("foreign-ws");
            let web = scratch.path().join("src").join("apps").join("web");
            let core = scratch.path().join("src").join("libs").join("core");
            seed_repo(&web);
            seed_repo(&core);
            hub.state
                .mutate(|s| {
                    s.repos.push(repo_entry("web", "web", &web));
                    s.repos.push(repo_entry("core", "core", &core));
                    s.workspaces
                        .push(workspace_entry("w1", "Shop", &["web", "core"]));
                })
                .expect("register workspace");
            let foreign = hub
                .state
                .worktrees_dir()
                .join("wt.feat")
                .join("shop")
                .join("libs")
                .join("core");
            std::fs::create_dir_all(&foreign).expect("create foreign folder");
            std::fs::write(foreign.join("keep.txt"), "x\n").expect("write foreign file");

            let msg = dispatch_one(
                &hub,
                ClientMessage::PreviewWorkspaceSpawn {
                    workspace_id: "w1".to_string(),
                    branch_name: "feat".to_string(),
                    base_branch: None,
                    request_id: Some("p2".to_string()),
                },
            )
            .await;

            let expected = foreign_folder_message(&foreign, "core");
            assert!(
                matches!(
                    &msg,
                    DaemonMessage::Error { message, request_id: Some(id) }
                        if id == "p2" && *message == expected
                ),
                "{msg:?}"
            );
            assert!(foreign.join("keep.txt").is_file());
        }
    }

    // --- Tailnet listener: auth kind, routers, Origin/Host, handshake ---

    // --- Boot wiring and the remote switch ---

    // --- ConfigureRemote and RemoteStatus ---
}
