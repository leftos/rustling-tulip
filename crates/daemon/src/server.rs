//! WebSocket server, message dispatch, and session orchestration glue.

use crate::orphan::{self, OrphanMeta};
use crate::paths::Dirs;
use crate::pty::{self, PtySpawnSpec};
use crate::registry::{
    add_repo, remove_repo, remove_workspace, set_repo_worktree_default,
    set_workspace_worktree_default, upsert_workspace,
};
use crate::scrollback;
use crate::session::{
    SessionEvent, SessionRecord, SessionRegistry, attach_lifecycle, new_id, push_recent_action,
};
use crate::state::AppState;
use crate::tabs;
use crate::{git, git_inspect, headless, osc_title, pty_state, vscode, workspace as ws};
use anyhow::{Context as _, anyhow};
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use base64::Engine as _;
use chrono::Utc;
use futures::{SinkExt as _, StreamExt as _};
use protocol::{
    AttentionReason, ClientMessage, DaemonHandshake, DaemonMessage, MemberDiff, PROTOCOL_VERSION,
    PaneDropEdge, PermissionMode, SessionKind, SessionMember, SessionMetrics, SessionMode,
    SessionStatus, SpawnRequest, SpawnTarget, TabEntry, VscodeWorkspaceSuggestion,
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
}

#[derive(Debug, Clone)]
pub enum TabEvent {
    Updated(TabEntry),
    Removed(String),
    Reordered(Vec<String>),
}

const TAB_EVENT_CAPACITY: usize = 256;

pub async fn run(state: Arc<AppState>, dirs: Dirs, orphans: Vec<OrphanMeta>) -> anyhow::Result<()> {
    let auth_token: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(48)
        .map(char::from)
        .collect();

    let (attention_tx, mut attention_rx) = mpsc::unbounded_channel::<pty_state::AttentionEvent>();
    let sessions = SessionRegistry::new();

    // Reattach orphan sessions before any clients connect so the initial
    // state snapshot already includes them.
    for meta in orphans {
        sessions.insert_orphan(&meta);
    }

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

    let hub = Hub {
        state,
        sessions,
        auth_token: auth_token.clone(),
        attention_tx,
        dirs: dirs.clone(),
        shutdown_tx,
        tab_events,
    };

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/health", get(|| async { "ok" }))
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
    let payload = DaemonHandshake {
        protocol_version: PROTOCOL_VERSION,
        port,
        auth_token: auth_token.to_string(),
        pid: std::process::id(),
    };
    let bytes = serde_json::to_vec_pretty(&payload).context("serializing handshake")?;
    let tmp = dirs.handshake_file.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes).context("writing handshake tmp")?;
    std::fs::rename(&tmp, &dirs.handshake_file).context("renaming handshake")?;
    Ok(())
}

async fn ws_handler(State(hub): State<Hub>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| client_session(hub, socket))
}

async fn client_session(hub: Hub, socket: WebSocket) {
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
    if let Err(err) = handshake(&hub, &mut receiver, &out_tx).await {
        warn!(?err, "client handshake failed");
        let _ = out_tx.send(DaemonMessage::AuthFailed {
            reason: err.to_string(),
        });
        drop(out_tx);
        let _ = send_task.await;
        return;
    }

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
                    let _ = out_for_events.send(DaemonMessage::SessionUpdated { session: snap });
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

    // Subscribe to tab-layout events; forwarded unconditionally (all clients
    // see structural changes).
    let mut tab_events_rx = hub.tab_events.subscribe();
    let out_for_tabs = out_tx.clone();
    let tab_event_task = tokio::spawn(async move {
        loop {
            match tab_events_rx.recv().await {
                Ok(TabEvent::Updated(tab)) => {
                    let _ = out_for_tabs.send(DaemonMessage::TabUpdated { tab });
                }
                Ok(TabEvent::Removed(tab_id)) => {
                    let _ = out_for_tabs.send(DaemonMessage::TabRemoved { tab_id });
                }
                Ok(TabEvent::Reordered(ids)) => {
                    let _ = out_for_tabs.send(DaemonMessage::TabsReordered { ordered_ids: ids });
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(lagged = n, "client tab event stream lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Send initial state snapshots.
    push_initial_state(&hub, &out_tx);

    recv_loop(&hub, &mut receiver, &out_tx, &attached).await;

    info!("client_session: recv_loop returned; tearing down");
    drop(out_tx);
    event_task.abort();
    tab_event_task.abort();
    let _ = send_task.await;
    info!("client_session: returning");
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
                let parsed: ClientMessage = match serde_json::from_str(&text) {
                    Ok(p) => p,
                    Err(err) => {
                        let _ = out_tx.send(DaemonMessage::Error {
                            message: format!("malformed message: {err}"),
                        });
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
) -> anyhow::Result<()> {
    let Some(Ok(msg)) = receiver.next().await else {
        return Err(anyhow!("client closed before handshake"));
    };
    let text = match msg {
        Message::Text(t) => t.to_string(),
        _ => return Err(anyhow!("first frame must be text Hello")),
    };
    let parsed: ClientMessage = serde_json::from_str(&text).context("parsing handshake message")?;
    let ClientMessage::Hello {
        protocol_version,
        auth_token,
    } = parsed
    else {
        return Err(anyhow!("first message must be Hello"));
    };
    if protocol_version != PROTOCOL_VERSION {
        return Err(anyhow!(
            "protocol version mismatch: client {protocol_version}, daemon {PROTOCOL_VERSION}"
        ));
    }
    if auth_token != hub.auth_token {
        return Err(anyhow!("invalid auth token"));
    }
    let _ = out_tx.send(DaemonMessage::Welcome {
        protocol_version: PROTOCOL_VERSION,
    });
    Ok(())
}

fn push_initial_state(hub: &Hub, out_tx: &mpsc::UnboundedSender<DaemonMessage>) {
    let (repos, workspaces, tabs) = hub
        .state
        .with_persisted(|s| (s.repos.clone(), s.workspaces.clone(), s.tabs.clone()));
    let _ = out_tx.send(DaemonMessage::Repos { repos });
    let _ = out_tx.send(DaemonMessage::Workspaces { workspaces });
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
            let repos = hub.state.with_persisted(|s| s.repos.clone());
            let _ = out_tx.send(DaemonMessage::Repos { repos });
            for suggestion in scan_vscode_workspaces(hub, &entry.path) {
                let _ = out_tx.send(DaemonMessage::VscodeWorkspaceSuggestion {
                    repo_id: entry.id.clone(),
                    suggestion,
                });
            }
            debug!(repo_id = %entry.id, "repo added");
        }
        ClientMessage::RemoveRepo { repo_id } => {
            remove_repo(&hub.state, &repo_id)?;
            let repos = hub.state.with_persisted(|s| s.repos.clone());
            let _ = out_tx.send(DaemonMessage::Repos { repos });
            let workspaces = hub.state.with_persisted(|s| s.workspaces.clone());
            let _ = out_tx.send(DaemonMessage::Workspaces { workspaces });
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
                name,
                member_repo_ids,
                linked_vscode_workspace,
            )?;
            let workspaces = hub.state.with_persisted(|s| s.workspaces.clone());
            let _ = out_tx.send(DaemonMessage::Workspaces { workspaces });
        }
        ClientMessage::RemoveWorkspace { workspace_id } => {
            remove_workspace(&hub.state, &workspace_id)?;
            let workspaces = hub.state.with_persisted(|s| s.workspaces.clone());
            let _ = out_tx.send(DaemonMessage::Workspaces { workspaces });
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
                let pty = crate::sync::lock(&rec).pty.clone();
                if let Some(pty) = pty {
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
            cleanup,
        } => {
            stop_session(hub, &session_id, &cleanup).await?;
            // Surface a one-shot Attention so connected clients can prompt.
            let _ = out_tx.send(DaemonMessage::Attention {
                session_id,
                reason: AttentionReason::Stopped,
            });
        }
        ClientMessage::SessionDiff { session_id } => {
            let diff = compute_session_diff(hub, &session_id).await?;
            let _ = out_tx.send(DaemonMessage::SessionDiff {
                session_id,
                per_member: diff,
            });
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
        ClientMessage::ListCommits {
            repo_id,
            branch,
            limit,
        } => {
            let path = repo_path_or_err(hub, &repo_id)?;
            let commits = git_inspect::list_commits(&path, branch.as_deref(), limit).await?;
            let _ = out_tx.send(DaemonMessage::Commits { repo_id, commits });
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
            let changes = git_inspect::repo_status(&repo).await?;
            let _ = out_tx.send(DaemonMessage::RepoStatus { repo_id, changes });
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
        ClientMessage::Shutdown => {
            info!("dispatch: ClientMessage::Shutdown received");
            shutdown_all(hub).await;
            info!("dispatch: shutdown_all returned; flipping watch");
            // Signal the accept loop to exit. The send may fail if a previous
            // Shutdown already flipped it; either way the daemon is on its
            // way out.
            let send_result = hub.shutdown_tx.send(true);
            info!(?send_result, "dispatch: shutdown_tx.send returned");
        }
        ClientMessage::SetRepoWorktreeDefault { repo_id, value } => {
            set_repo_worktree_default(&hub.state, &repo_id, value)?;
            let repos = hub.state.with_persisted(|s| s.repos.clone());
            let _ = out_tx.send(DaemonMessage::Repos { repos });
        }
        ClientMessage::SetWorkspaceWorktreeDefault {
            workspace_id,
            value,
        } => {
            set_workspace_worktree_default(&hub.state, &workspace_id, value)?;
            let workspaces = hub.state.with_persisted(|s| s.workspaces.clone());
            let _ = out_tx.send(DaemonMessage::Workspaces { workspaces });
        }
        ClientMessage::ListTabs => {
            let tabs = hub.state.with_persisted(|s| s.tabs.clone());
            let _ = out_tx.send(DaemonMessage::Tabs { tabs });
        }
        ClientMessage::CreateTab {
            name,
            initial_session_id,
        } => {
            let new_tab = tabs::make_tab(name, initial_session_id);
            hub.state.mutate(|s| s.tabs.push(new_tab.clone()))?;
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
        ClientMessage::SplitPane {
            tab_id,
            pane_id,
            direction,
            place,
            new_session_id,
        } => {
            let updated = hub.state.mutate(|s| {
                let tab = tabs::find_tab_mut(&mut s.tabs, &tab_id)?;
                tabs::split_pane(&mut tab.grid, &pane_id, direction, place, new_session_id)?;
                Ok::<_, anyhow::Error>(tab.clone())
            })??;
            let _ = hub.tab_events.send(TabEvent::Updated(updated));
        }
        ClientMessage::ClosePane { tab_id, pane_id } => {
            let outcome = hub.state.mutate(|s| {
                let tab = tabs::find_tab_mut(&mut s.tabs, &tab_id)?;
                let empty = tabs::close_pane(&mut tab.grid, &pane_id)?;
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
                tabs::set_pane_ratio(&mut tab.grid, &split_path, ratio)?;
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
                tabs::replace_pane_session(&mut tab.grid, &pane_id, session_id)?;
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
    }
    Ok(())
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
            let (extracted, source_empty) = tabs::extract_pane(&mut tab.grid, src_pane_id)?;
            if source_empty {
                return Err(anyhow!("cannot move the only pane within the same tab"));
            }
            tabs::insert_adjacent(&mut tab.grid, dst_pane_id, edge, extracted)?;
            Ok::<_, anyhow::Error>(vec![TabEvent::Updated(tab.clone())])
        } else {
            let (extracted_node, source_empty) = {
                let src_tab = tabs::find_tab_mut(&mut s.tabs, src_tab_id)?;
                tabs::extract_pane(&mut src_tab.grid, src_pane_id)?
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
            tabs::insert_adjacent(&mut dst_tab.grid, dst_pane_id, edge, extracted_node)?;
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
        if let Err(err) = stop_session(hub, id, &[]).await {
            warn!(session_id = %id, ?err, "stop_session during shutdown failed");
        }
        info!(session_id = %id, "shutdown_all: session stopped");
    }
    info!("shutdown_all: complete");
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

async fn spawn_session(hub: &Hub, req: SpawnRequest) -> anyhow::Result<protocol::SessionSnapshot> {
    let SpawnRequest {
        label,
        target,
        mode,
        initial_prompt,
        dangerously_skip_permissions,
        model,
        permission_mode,
        extra_env,
    } = req;

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

    let session_id = new_id();
    let label = label.unwrap_or(default_label);
    let cfg = SpawnConfig {
        dangerously_skip_permissions,
        model,
        permission_mode,
        extra_env,
    };

    match mode {
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
            )
        }
        SessionMode::Interactive => spawn_interactive_session(
            hub,
            session_id,
            label,
            kind,
            members,
            workspace_id,
            primary_cwd,
            initial_prompt,
            &cfg,
        ),
    }
}

/// Per-spawn configuration bundled together to keep spawn-fn signatures narrow.
struct SpawnConfig {
    dangerously_skip_permissions: bool,
    model: Option<String>,
    permission_mode: Option<PermissionMode>,
    extra_env: Vec<(String, String)>,
}

impl SpawnConfig {
    /// Append `--model` and `--permission-mode` (when applicable) plus
    /// `--dangerously-skip-permissions` to the argv being built. The CLI
    /// rejects `--permission-mode` together with `--dangerously-skip-permissions`,
    /// so the latter wins when both are set.
    fn extend_args(&self, args: &mut Vec<String>) {
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

#[expect(
    clippy::too_many_arguments,
    reason = "Constructing a session record is naturally wide; bundling into a struct adds noise"
)]
fn spawn_interactive_session(
    hub: &Hub,
    session_id: String,
    label: String,
    kind: SessionKind,
    members: Vec<SessionMember>,
    workspace_id: Option<String>,
    primary_cwd: PathBuf,
    initial_prompt: Option<String>,
    cfg: &SpawnConfig,
) -> anyhow::Result<protocol::SessionSnapshot> {
    let mut args: Vec<String> = Vec::new();
    for extra in members.iter().skip(1) {
        args.push("--add-dir".to_string());
        args.push(extra.worktree_path.clone());
    }
    cfg.extend_args(&mut args);
    if let Some(prompt) = initial_prompt {
        args.push("-p".to_string());
        args.push(prompt);
    }

    let spec = PtySpawnSpec {
        program: claude_program(),
        args,
        cwd: primary_cwd,
        env: merged_env(&cfg.extra_env),
        cols: 120,
        rows: 32,
    };
    let pty = pty::spawn(spec).context("spawning claude pty")?;
    let pid = pty.pid();

    let started_at = Utc::now();
    let mut record = SessionRecord {
        id: session_id.clone(),
        label: label.clone(),
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
    };
    push_recent_action(&mut record, "session started".to_string());
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
            SessionMode::Interactive,
            members,
            started_at,
            workspace_id,
            // Claude's PTY child is a node shim on Windows; let the legacy
            // claude|node fallback in `is_session_alive` handle it.
            None,
        )
    {
        orphan::try_write_meta(&hub.dirs, &meta);
    }

    pty_state::watch(
        &hub.sessions,
        session_id.clone(),
        pty.output.subscribe(),
        hub.attention_tx.clone(),
    );
    osc_title::watch(
        &hub.sessions,
        session_id.clone(),
        pty.output.subscribe(),
        hub.dirs.clone(),
    );
    attach_lifecycle(&hub.sessions, session_id, &pty, Some(hub.dirs.clone()));
    Ok(snap)
}

#[expect(
    clippy::too_many_arguments,
    reason = "Constructing a session record is naturally wide; bundling into a struct adds noise"
)]
fn spawn_plain_shell_session(
    hub: &Hub,
    session_id: String,
    label: String,
    kind: SessionKind,
    members: Vec<SessionMember>,
    workspace_id: Option<String>,
    primary_cwd: PathBuf,
    cfg: &SpawnConfig,
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
        program,
        args: Vec::new(),
        cwd: primary_cwd,
        env: merged_env(&cfg.extra_env),
        cols: 120,
        rows: 32,
    };
    let pty = pty::spawn(spec).context("spawning plain shell pty")?;
    let pid = pty.pid();

    let started_at = Utc::now();
    let mut record = SessionRecord {
        id: session_id.clone(),
        label: label.clone(),
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
            Some(shell_label),
        )
    {
        orphan::try_write_meta(&hub.dirs, &meta);
    }

    // Deliberately skip `pty_state::watch` — its Claude TUI prompt heuristic
    // would mis-classify a normal shell prompt as `AwaitingInput`. Keep
    // `osc_title::watch` so window-title escape sequences (which pwsh emits
    // by default) still update the session label.
    osc_title::watch(
        &hub.sessions,
        session_id.clone(),
        pty.output.subscribe(),
        hub.dirs.clone(),
    );
    attach_lifecycle(&hub.sessions, session_id, &pty, Some(hub.dirs.clone()));
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
    cfg: &SpawnConfig,
) -> anyhow::Result<protocol::SessionSnapshot> {
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
    cfg.extend_args(&mut args);
    args.push("-p".to_string());
    args.push(prompt);

    let spec = headless::HeadlessSpec {
        program: claude_program(),
        args,
        cwd: primary_cwd,
        env: merged_env(&cfg.extra_env),
    };

    let started_at = Utc::now();
    let mut record = SessionRecord {
        id: session_id.clone(),
        label: label.clone(),
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
        let worktree_path = git::default_worktree_path(&repo_path, branch_name);
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
    let (workspace, resolved) = ws::resolve_workspace(
        &hub.state,
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

async fn stop_session(
    hub: &Hub,
    session_id: &str,
    cleanup: &[protocol::CleanupAction],
) -> anyhow::Result<()> {
    let Some(rec) = hub.sessions.get(session_id) else {
        return Err(anyhow!("unknown session: {session_id}"));
    };
    let (members, pty, headless_handle) = {
        let guard = crate::sync::lock(&rec);
        (
            guard.members.clone(),
            guard.pty.clone(),
            guard.headless.clone(),
        )
    };
    if let Some(pty) = pty {
        pty.kill();
    }
    if let Some(h) = headless_handle {
        h.kill().await;
    }
    for action in cleanup {
        if !action.remove_worktree {
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
            warn!(?err, "worktree remove failed");
        }
    }
    hub.sessions.remove(session_id);
    orphan::try_delete_session_dir(&hub.dirs, session_id);
    prune_session_from_tabs(hub, session_id);
    Ok(())
}

/// Drop dangling pane references to `session_id` from every tab. Each
/// affected tab is broadcast as a fresh `TabUpdated` so all connected clients
/// switch the pane to the empty-placeholder state.
fn prune_session_from_tabs(hub: &Hub, session_id: &str) {
    let modified = match hub.state.mutate(|s| {
        let mut out = Vec::new();
        for tab in &mut s.tabs {
            if tabs::prune_session(&mut tab.grid, session_id) {
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

async fn compute_session_diff(hub: &Hub, session_id: &str) -> anyhow::Result<Vec<MemberDiff>> {
    let members = hub
        .sessions
        .get(session_id)
        .map(|rec| crate::sync::lock(&rec).members.clone())
        .ok_or_else(|| anyhow!("unknown session: {session_id}"))?;
    let mut out = Vec::new();
    for m in members {
        let path = PathBuf::from(&m.worktree_path);
        let changed = git::changed_files(&path).await.unwrap_or_default();
        let clean = changed.is_empty();
        out.push(MemberDiff {
            repo_id: m.repo_id,
            repo_name: m.repo_name,
            changed_files: changed,
            clean,
        });
    }
    Ok(out)
}

fn claude_program() -> String {
    std::env::var("RUSTLING_TULIP_CLAUDE").unwrap_or_else(|_| "claude".to_string())
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
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
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
        suggestion.suggested_name.clone(),
        member_repo_ids,
        Some(suggestion.source_path.clone()),
    )?;
    let repos = hub.state.with_persisted(|s| s.repos.clone());
    let workspaces = hub.state.with_persisted(|s| s.workspaces.clone());
    let _ = out_tx.send(DaemonMessage::Repos { repos });
    let _ = out_tx.send(DaemonMessage::Workspaces { workspaces });
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
