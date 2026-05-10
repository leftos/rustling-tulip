//! WebSocket server, message dispatch, and session orchestration glue.

use crate::paths::Dirs;
use crate::pty::{self, PtySpawnSpec};
use crate::registry::{add_repo, remove_repo, remove_workspace, upsert_workspace};
use crate::session::{
    SessionEvent, SessionRecord, SessionRegistry, attach_lifecycle, new_id, push_recent_action,
};
use crate::state::AppState;
use crate::{git, git_inspect, headless, pty_state, vscode, workspace as ws};
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
    AttentionReason, BranchTarget, ClientMessage, DaemonHandshake, DaemonMessage, MemberDiff,
    PROTOCOL_VERSION, SessionKind, SessionMember, SessionMetrics, SessionMode, SessionStatus,
    SpawnRequest, SpawnTarget, VscodeWorkspaceSuggestion,
};
use rand::Rng as _;
use rand::distributions::Alphanumeric;
use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

#[derive(Clone)]
pub struct Hub {
    pub state: Arc<AppState>,
    pub sessions: Arc<SessionRegistry>,
    pub auth_token: String,
    pub attention_tx: mpsc::UnboundedSender<pty_state::AttentionEvent>,
}

pub async fn run(state: Arc<AppState>, dirs: Dirs) -> anyhow::Result<()> {
    let auth_token: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(48)
        .map(char::from)
        .collect();

    let (attention_tx, mut attention_rx) = mpsc::unbounded_channel::<pty_state::AttentionEvent>();
    let sessions = SessionRegistry::new();

    // Forward attention events through the registry's broadcast so all
    // attached clients see them via the same SessionEvent channel they
    // already subscribe to.
    let sessions_for_attention = Arc::clone(&sessions);
    tokio::spawn(async move {
        while let Some(evt) = attention_rx.recv().await {
            sessions_for_attention.fan_out_attention(evt.session_id, evt.reason);
        }
    });

    let hub = Hub {
        state,
        sessions,
        auth_token: auth_token.clone(),
        attention_tx,
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

    axum::serve(listener, app)
        .await
        .context("axum serve failed")?;
    Ok(())
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
                        let data_b64 =
                            base64::engine::general_purpose::STANDARD.encode(&data);
                        let _ = out_for_events
                            .send(DaemonMessage::PtyOutput { session_id, data_b64 });
                    }
                }
                Ok(SessionEvent::Attention { session_id, reason }) => {
                    let _ = out_for_events
                        .send(DaemonMessage::Attention { session_id, reason });
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(lagged = n, "client event stream lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Send initial state snapshots.
    push_initial_state(&hub, &out_tx);

    // Main receive loop.
    while let Some(Ok(msg)) = receiver.next().await {
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
        if let Err(err) = dispatch(&hub, parsed, &out_tx, &attached).await {
            let _ = out_tx.send(DaemonMessage::Error {
                message: err.to_string(),
            });
        }
    }

    debug!("client disconnected");
    drop(out_tx);
    event_task.abort();
    let _ = send_task.await;
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
    let parsed: ClientMessage =
        serde_json::from_str(&text).context("parsing handshake message")?;
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
    let (repos, workspaces) = hub
        .state
        .with_persisted(|s| (s.repos.clone(), s.workspaces.clone()));
    let _ = out_tx.send(DaemonMessage::Repos { repos });
    let _ = out_tx.send(DaemonMessage::Workspaces { workspaces });
    let _ = out_tx.send(DaemonMessage::Sessions {
        sessions: hub.sessions.snapshots(),
    });
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
            let (_, resolved) = ws::resolve_workspace(
                &hub.state,
                &workspace_id,
                &branch_name,
                base_branch.as_deref(),
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
                .with_persisted(|s| s.repos.iter().find(|r| r.id == repo_id).map(|r| r.path.clone()))
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
    }
    Ok(())
}

fn repo_path_or_err(hub: &Hub, repo_id: &str) -> anyhow::Result<PathBuf> {
    hub.state
        .with_persisted(|s| s.repos.iter().find(|r| r.id == repo_id).map(|r| r.path.clone()))
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
    } = req;

    let (kind, members, primary_cwd, default_label) = match target {
        SpawnTarget::Single { repo_id, branch } => spawn_single(hub, &repo_id, branch).await?,
        SpawnTarget::Workspace {
            workspace_id,
            branch_name,
            base_branch,
        } => spawn_workspace(hub, &workspace_id, &branch_name, base_branch).await?,
    };

    let session_id = new_id();
    let label = label.unwrap_or(default_label);

    if mode == SessionMode::Headless {
        let prompt = initial_prompt
            .clone()
            .ok_or_else(|| anyhow!("headless sessions require an initial_prompt"))?;
        return spawn_headless_session(
            hub,
            session_id,
            label,
            kind,
            members,
            primary_cwd,
            prompt,
            dangerously_skip_permissions,
        );
    }

    spawn_interactive_session(
        hub,
        session_id,
        label,
        kind,
        members,
        primary_cwd,
        initial_prompt,
        dangerously_skip_permissions,
    )
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
    primary_cwd: PathBuf,
    initial_prompt: Option<String>,
    dangerously_skip_permissions: bool,
) -> anyhow::Result<protocol::SessionSnapshot> {
    let mut args: Vec<String> = Vec::new();
    for extra in members.iter().skip(1) {
        args.push("--add-dir".to_string());
        args.push(extra.worktree_path.clone());
    }
    if dangerously_skip_permissions {
        args.push("--dangerously-skip-permissions".to_string());
    }
    if let Some(prompt) = initial_prompt {
        args.push("-p".to_string());
        args.push(prompt);
    }

    let spec = PtySpawnSpec {
        program: claude_program(),
        args,
        cwd: primary_cwd,
        env: passthrough_env(),
        cols: 120,
        rows: 32,
    };
    let pty = pty::spawn(spec).context("spawning claude pty")?;

    let mut record = SessionRecord {
        id: session_id.clone(),
        label,
        kind,
        members,
        mode: SessionMode::Interactive,
        started_at: Utc::now(),
        status: SessionStatus::Idle,
        exit_code: None,
        metrics: SessionMetrics::default(),
        recent_actions: Vec::new(),
        pty: Some(Arc::clone(&pty)),
        headless: None,
    };
    push_recent_action(&mut record, "session started".to_string());
    hub.sessions.insert(record);
    let snap = hub
        .sessions
        .get(&session_id)
        .map(|rec| crate::sync::lock(&rec).snapshot())
        .ok_or_else(|| anyhow!("session vanished"))?;

    pty_state::watch(
        &hub.sessions,
        session_id.clone(),
        pty.output.subscribe(),
        hub.attention_tx.clone(),
    );
    attach_lifecycle(&hub.sessions, session_id, &pty);
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
    primary_cwd: PathBuf,
    prompt: String,
    dangerously_skip_permissions: bool,
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
    if dangerously_skip_permissions {
        args.push("--dangerously-skip-permissions".to_string());
    }
    args.push("-p".to_string());
    args.push(prompt);

    let spec = headless::HeadlessSpec {
        program: claude_program(),
        args,
        cwd: primary_cwd,
        env: passthrough_env(),
    };

    let mut record = SessionRecord {
        id: session_id.clone(),
        label,
        kind,
        members,
        mode: SessionMode::Headless,
        started_at: Utc::now(),
        status: SessionStatus::Spawning,
        exit_code: None,
        metrics: SessionMetrics::default(),
        recent_actions: Vec::new(),
        pty: None,
        headless: None,
    };
    push_recent_action(&mut record, "headless session started".to_string());
    hub.sessions.insert(record);

    let handle = headless::spawn(&spec, &hub.sessions, session_id.clone())
        .context("spawning headless claude")?;

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
    branch: BranchTarget,
) -> anyhow::Result<(SessionKind, Vec<SessionMember>, PathBuf, String)> {
    let repo = hub
        .state
        .with_persisted(|s| s.repos.iter().find(|r| r.id == repo_id).cloned())
        .ok_or_else(|| anyhow!("unknown repo: {repo_id}"))?;
    let repo_path = PathBuf::from(&repo.path);

    let (branch_name, base_for_create) = match branch {
        BranchTarget::Existing { name } => (name, None),
        BranchTarget::NewFromBase { name, base } => (name, Some(base)),
    };

    let worktree_path = git::default_worktree_path(&repo_path, &branch_name);
    if !worktree_path.exists() {
        git::worktree_add(
            &repo_path,
            &worktree_path,
            &branch_name,
            base_for_create.as_deref(),
        )
        .await
        .context("creating worktree")?;
    }

    let member = SessionMember {
        repo_id: repo.id.clone(),
        repo_name: repo.name.clone(),
        branch: branch_name.clone(),
        worktree_path: worktree_path.to_string_lossy().into_owned(),
    };
    let label = format!("{}: {branch_name}", repo.name);
    Ok((SessionKind::Single, vec![member], worktree_path, label))
}

async fn spawn_workspace(
    hub: &Hub,
    workspace_id: &str,
    branch_name: &str,
    base_branch: Option<String>,
) -> anyhow::Result<(SessionKind, Vec<SessionMember>, PathBuf, String)> {
    let (workspace, resolved) = ws::resolve_workspace(
        &hub.state,
        workspace_id,
        branch_name,
        base_branch.as_deref(),
    )
    .await?;
    ws::ensure_worktrees(&resolved, branch_name).await?;

    let primary = resolved
        .first()
        .ok_or_else(|| anyhow!("workspace has no members"))?
        .worktree_path
        .clone();
    let members: Vec<SessionMember> = resolved
        .iter()
        .map(|m| SessionMember {
            repo_id: m.repo.id.clone(),
            repo_name: m.repo.name.clone(),
            branch: branch_name.to_string(),
            worktree_path: m.worktree_path.to_string_lossy().into_owned(),
        })
        .collect();
    let label = format!("{}: {branch_name}", workspace.name);
    Ok((SessionKind::Workspace, members, primary, label))
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
        let repo_path = hub
            .state
            .with_persisted(|s| s.repos.iter().find(|r| r.id == member.repo_id).map(|r| r.path.clone()));
        let Some(repo_path) = repo_path else {
            continue;
        };
        let worktree_path = PathBuf::from(&member.worktree_path);
        if let Err(err) = git::worktree_remove(Path::new(&repo_path), &worktree_path).await {
            warn!(?err, "worktree remove failed");
        }
    }
    hub.sessions.remove(session_id);
    Ok(())
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

fn scan_vscode_workspaces(hub: &Hub, repo_path: &str) -> Vec<VscodeWorkspaceSuggestion> {
    let known: Vec<(String, String)> = hub
        .state
        .with_persisted(|s| s.repos.iter().map(|r| (r.id.clone(), r.path.clone())).collect());
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
