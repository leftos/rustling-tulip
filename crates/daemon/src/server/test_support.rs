//! Hub, scratch-directory, dispatch and git fixtures shared by the server's test modules.
#![expect(
    clippy::expect_used,
    reason = "fixtures assert preconditions with expect; failure messages aid debugging"
)]
use super::connection::{AuthKind, ConnCtx, FetchRegistry, NEXT_CONNECTION, PtyForwarders};
use super::handshake::TAILNET_HANDSHAKE_TIMEOUT;
use super::recovery::RecoveryClaims;
use super::remote::RemoteSlot;
use super::spawn::hook_settings_path;
use super::{Hub, dispatch};
use crate::paths::Dirs;
use crate::session::{SessionRecord, SessionRegistry};
use crate::state::AppState;
use crate::{pty_state, user_env};
use axum::body::Body;
use axum::http::Request;
use chrono::Utc;
use protocol::{
    Agent, AgentOptions, AppearanceOverrides, ClientMessage, DaemonMessage, SessionKind,
    SessionMetrics, SessionMode, SessionStatus, SpawnRequest, SpawnTarget, TabContent, TabEntry,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc};

/// A scratch directory for a hub's config, state and worktrees, removed
/// when the test ends.
pub(super) struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    pub(super) fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "rt-server-{tag}-{}-{}",
            std::process::id(),
            NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Self { path }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A hub over a scratch config dir with no repos registered.
pub(super) fn test_hub(tag: &str) -> (Hub, ScratchDir) {
    test_hub_in(ScratchDir::new(tag), None)
}

/// A hub over an already-made scratch dir, its `dirs.bin_templates` set to
/// `bin_templates`, so a spawn resolves the tracer template from that
/// directory first (see [`spawnless_test_hub`]).
pub(super) fn test_hub_in(
    scratch: ScratchDir,
    bin_templates: Option<PathBuf>,
) -> (Hub, ScratchDir) {
    let (hub, scratch, _attention) = build_test_hub(scratch, bin_templates);
    (hub, scratch)
}

/// A hub over a fresh scratch dir whose attention events the test reads.
pub(super) fn test_hub_with_attention(
    tag: &str,
) -> (
    Hub,
    ScratchDir,
    mpsc::UnboundedReceiver<pty_state::AttentionEvent>,
) {
    build_test_hub(ScratchDir::new(tag), None)
}

pub(super) fn build_test_hub(
    scratch: ScratchDir,
    bin_templates: Option<PathBuf>,
) -> (
    Hub,
    ScratchDir,
    mpsc::UnboundedReceiver<pty_state::AttentionEvent>,
) {
    let config = scratch.path().join("config");
    let dirs = Dirs {
        config: config.clone(),
        state_file: config.join("state.json"),
        handshake_file: config.join("daemon.json"),
        remote_config_file: config.join("remote.json"),
        sessions_dir: config.join("sessions"),
        worktrees_dir: scratch.path().join("worktrees"),
        binaries_dir: scratch.path().join("binaries"),
        bin_templates,
    };
    std::fs::create_dir_all(&dirs.sessions_dir).expect("create sessions dir");
    std::fs::create_dir_all(&dirs.worktrees_dir).expect("create worktrees dir");
    std::fs::create_dir_all(&dirs.binaries_dir).expect("create binaries dir");
    let state = Arc::new(AppState::load_or_default(&dirs).expect("load test state"));
    let sessions = SessionRegistry::new(dirs.clone());
    let (attention_tx, attention_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, _) = tokio::sync::watch::channel(false);
    let (tab_events, _) = broadcast::channel(16);
    let (preset_events, _) = broadcast::channel(16);
    let (state_events, _) = broadcast::channel(16);
    let (client_count, _) = tokio::sync::watch::channel(0usize);
    let (keep_awake_enabled, _) = tokio::sync::watch::channel(true);
    let (remote_enabled, _) = tokio::sync::watch::channel(false);
    let (_, keep_awake_status) = tokio::sync::watch::channel(crate::keep_awake::Status {
        enabled: true,
        active: false,
    });
    let hub = Hub {
        state,
        sessions,
        auth_token: "test-token".to_string(),
        proof_key: tracer_protocol::daemon_proof_key("test-token"),
        tailnet_handshake_timeout: TAILNET_HANDSHAKE_TIMEOUT,
        attention_tx,
        dirs,
        shutdown_tx,
        tab_events,
        preset_events,
        preset_cancellations: Arc::new(AsyncMutex::new(HashMap::new())),
        state_events,
        client_count: Arc::new(client_count),
        remote_enabled: Arc::new(remote_enabled),
        tailnet: Arc::new(AsyncMutex::new(None)),
        remote_status: Arc::new(tokio::sync::watch::Sender::new(RemoteSlot::initial(
            crate::remote_config::RemoteConfig::default(),
        ))),
        keep_awake_enabled: Arc::new(keep_awake_enabled),
        keep_awake_status,
        recovering: RecoveryClaims::default(),
        resume_in_flight: Arc::default(),
        start_resume: Arc::default(),
        resume_reported: Arc::default(),
        pending_hooks: crate::hook_status::PendingHooks::default(),
    };
    (hub, scratch, attention_rx)
}

/// Dispatch `msg` against `hub` and return the single message it put on
/// the wire, failing when it dispatched none or more than one.
pub(super) async fn dispatch_one(hub: &Hub, msg: ClientMessage) -> DaemonMessage {
    let local = AuthKind::Local;
    dispatch_one_as(hub, msg, &local).await
}

/// Dispatch `msg` over a connection of `auth` and return its single
/// reply.
pub(super) async fn dispatch_one_as(
    hub: &Hub,
    msg: ClientMessage,
    auth: &AuthKind,
) -> DaemonMessage {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    let (file_tx, _file_rx) = mpsc::channel(1);
    let pty_forwarders: PtyForwarders = Arc::new(AsyncMutex::new(HashMap::new()));
    let fetches: FetchRegistry = Arc::new(AsyncMutex::new(HashMap::new()));
    let ctx = ConnCtx {
        out_tx: &out_tx,
        file_tx: &file_tx,
        pty_forwarders: &pty_forwarders,
        fetches: &fetches,
        client_id: "c1",
        client_name: None,
        connection: 1,
        auth,
    };
    dispatch(hub, msg, &ctx)
        .await
        .expect("a failed read is reported on the wire, not returned from dispatch");
    let first = out_rx.try_recv().expect("one reply is sent");
    assert!(out_rx.try_recv().is_err(), "exactly one reply is sent");
    first
}

/// `git init` a fresh repo at `repo` (no remotes).
pub(super) fn git_init(repo: &Path) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["init", "-b", "main"])
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Every message waiting on `out_rx`.
pub(super) fn drain(out_rx: &mut mpsc::UnboundedReceiver<DaemonMessage>) -> Vec<DaemonMessage> {
    let mut messages = Vec::new();
    while let Ok(msg) = out_rx.try_recv() {
        messages.push(msg);
    }
    messages
}

/// A registry holding one session `s1`, under a scratch config dir.
pub(super) fn appearance_registry(tag: &str) -> (Arc<SessionRegistry>, Dirs) {
    let root = std::env::temp_dir().join(format!("rt-appearance-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create scratch dir");
    let dirs = Dirs {
        config: root.clone(),
        state_file: root.join("state.json"),
        handshake_file: root.join("daemon.json"),
        remote_config_file: root.join("remote.json"),
        sessions_dir: root.join("sessions"),
        worktrees_dir: root.join("worktrees"),
        binaries_dir: root.join("binaries"),
        bin_templates: None,
    };
    let sessions = SessionRegistry::new(dirs.clone());
    sessions.insert(idle_record("s1"));
    (sessions, dirs)
}

/// An idle standalone session `id` with no process behind it.
pub(super) fn idle_record(id: &str) -> SessionRecord {
    SessionRecord {
        id: id.to_string(),
        label: id.to_string(),
        default_label: id.to_string(),
        user_label: None,
        kind: SessionKind::Standalone,
        members: Vec::new(),
        mode: SessionMode::Interactive,
        started_at: Utc::now(),
        status: SessionStatus::Idle,
        status_since: Some(Utc::now()),
        exit_code: None,
        metrics: SessionMetrics::default(),
        recent_actions: Vec::new(),
        pty: None,
        headless: None,
        workspace_id: None,
        agent: Agent::default(),
        terminal_title: None,
        program_name: None,
        current_cwd: None,
        appearance: AppearanceOverrides::default(),
        spawn_config: None,
        is_abandoned: false,
        is_inactive: false,
        worktree_paths: Vec::new(),
        last_prompt: None,
        input_notifier: None,
        screen: None,
        scrollback_snapshot_req: None,
        spawn_origin: None,
        claude_session_id: None,
        pending_input: None,
        last_hook_time: None,
        hook_driven: false,
        background: false,
        last_assistant_message: None,
        busy_excluded: false,
        agent_in_shell: false,
        shell_label: None,
        agent_exit_announced: false,
    }
}

/// `id`'s snapshot, failing when the registry does not hold it.
pub(super) fn snapshot_of(hub: &Hub, id: &str) -> protocol::SessionSnapshot {
    hub.sessions
        .snapshots()
        .into_iter()
        .find(|snapshot| snapshot.id == id)
        .expect("session is in the registry")
}

/// Wait up to five seconds for `cond` to hold; return whether it did.
pub(super) async fn wait_until(cond: impl Fn() -> bool) -> bool {
    for _ in 0..500 {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cond()
}

/// Every message dispatching `msg` puts on the wire, for a message that
/// may answer with none.
pub(super) async fn dispatch_all(hub: &Hub, msg: ClientMessage) -> Vec<DaemonMessage> {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
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
    dispatch(hub, msg, &ctx).await.expect("dispatch succeeds");
    let mut sent = Vec::new();
    while let Ok(message) = out_rx.try_recv() {
        sent.push(message);
    }
    sent
}

/// A test hub whose spawns fail before any process starts. Its scratch dir
/// holds a dummy `rt-tracer` template, which the spawn does find; its
/// binaries dir is a plain file instead, so staging that template into the
/// binary cache fails and no tracer (and so no child) is ever launched.
pub(super) fn spawnless_test_hub(tag: &str) -> (Hub, ScratchDir) {
    let scratch = ScratchDir::new(tag);
    let templates = scratch.path().join("templates");
    std::fs::create_dir_all(&templates).expect("create templates dir");
    std::fs::write(
        templates.join(crate::tracer_client::tracer_exe_name()),
        b"stub rt-tracer template",
    )
    .expect("write stub tracer template");
    let (hub, scratch) = test_hub_in(scratch, Some(templates));
    std::fs::remove_dir_all(&hub.dirs.binaries_dir).expect("remove binaries dir");
    std::fs::write(&hub.dirs.binaries_dir, b"not a directory").expect("block binaries dir");
    (hub, scratch)
}

/// A test hub whose cached tracer *does* resolve: its scratch dir holds a
/// dummy `rt-tracer` template and its binaries dir is a real directory, so
/// staging the template into the binary cache works. The stub is not a
/// runnable executable, so a spawn that gets as far as starting the tracer
/// still fails there — after the hook settings file was written.
pub(super) fn hook_test_hub(tag: &str) -> (Hub, ScratchDir) {
    let scratch = ScratchDir::new(tag);
    let templates = scratch.path().join("templates");
    std::fs::create_dir_all(&templates).expect("create templates dir");
    std::fs::write(
        templates.join(crate::tracer_client::tracer_exe_name()),
        b"stub rt-tracer template",
    )
    .expect("write stub tracer template");
    test_hub_in(scratch, Some(templates))
}

/// A session `id`'s hook settings file, under the fixed name its spawn
/// passes as `--settings` and the hook command reads.
pub(super) fn hook_settings_file(hub: &Hub, id: &str) -> PathBuf {
    hook_settings_path(&hub.dirs, id)
}

/// Run git in `repo`, failing the test when it fails; returns stdout.
pub(super) fn git_ok(repo: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.email=t@example.com", "-c", "user.name=Test"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .expect("spawn git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A spawnless hub with repo `r1`: a git repo on `main` with one commit.
pub(super) fn env_ref_repo_hub(tag: &str) -> (Hub, ScratchDir, PathBuf) {
    let (hub, scratch) = spawnless_test_hub(tag);
    let repo = scratch.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    git_init(&repo);
    git_ok(&repo, &["commit", "--allow-empty", "-m", "seed"]);
    hub.state
        .mutate(|s| {
            s.repos.push(protocol::RepoEntry {
                id: "r1".to_string(),
                name: "r1".to_string(),
                path: repo.to_string_lossy().into_owned(),
                default_branch: None,
                default_use_worktree: false,
                appearance: AppearanceOverrides::default(),
                last_agent: None,
                last_spawn_config: None,
            });
        })
        .expect("register repo");
    (hub, scratch, repo)
}

/// The next message on `rx`.
pub(super) async fn next_message(rx: &mut mpsc::UnboundedReceiver<DaemonMessage>) -> DaemonMessage {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("the forwarder delivers a message")
        .expect("the channel is open")
}

/// The session id and `request_id` of the one message on `rx`, which
/// must be a `SessionUpdated`; then checks nothing follows.
pub(super) async fn only_update(
    rx: &mut mpsc::UnboundedReceiver<DaemonMessage>,
) -> (String, Option<String>) {
    let update = match next_message(rx).await {
        DaemonMessage::SessionUpdated {
            session,
            request_id,
        } => Some((session.id, request_id)),
        _ => None,
    };
    let extra = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
    assert!(extra.is_err(), "one message per update: {extra:?}");
    update.expect("only a SessionUpdated arrives")
}

/// Seed `client_id` with one tab whose grid is two empty panes side by
/// side, so a move has a source and a destination pane to aim at.
pub(super) fn seed_two_pane_layout(hub: &Hub, client_id: &str) {
    let grid = crate::tabs::build_balanced(
        &[("p1".to_string(), None), ("p2".to_string(), None)],
        protocol::SplitDirection::Horizontal,
    )
    .expect("two panes build a grid");
    hub.state
        .mutate_client_layout(client_id, |tabs| {
            tabs.push(TabEntry {
                id: "t1".to_string(),
                name: "Tab".to_string(),
                content: TabContent::Grid { grid },
                created_at: Utc::now(),
                picked_layout: None,
            });
        })
        .expect("seed the client layout");
}

/// A `POST /hook/{session}` request, carrying the token and hook time
/// headers the shim sends when each is given.
pub(super) fn hook_request(
    session: &str,
    token: Option<&str>,
    hook_time: Option<&str>,
    body: &str,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("/hook/{session}"));
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(hook_time) = hook_time {
        builder = builder.header("x-rt-hook-time", hook_time);
    }
    builder
        .body(Body::from(body.to_owned()))
        .expect("build hook request")
}

/// The owner's laptop as `tailscale whois` describes it.
pub(super) fn test_peer() -> tailnet::PeerIdentity {
    tailnet::PeerIdentity {
        stable_id: "nPeerCNTRL".to_owned(),
        name: "laptop.tail1234.ts.net".to_owned(),
        user_id: 2,
        login: "owner@example.com".to_owned(),
    }
}

pub(super) fn repo_entry(id: &str, name: &str, path: &Path) -> protocol::RepoEntry {
    protocol::RepoEntry {
        id: id.to_string(),
        name: name.to_string(),
        path: path.to_string_lossy().into_owned(),
        default_branch: None,
        default_use_worktree: true,
        appearance: AppearanceOverrides::default(),
        last_agent: None,
        last_spawn_config: None,
    }
}

pub(super) fn workspace_entry(id: &str, name: &str, members: &[&str]) -> protocol::WorkspaceEntry {
    protocol::WorkspaceEntry {
        id: id.to_string(),
        name: name.to_string(),
        member_repo_ids: members.iter().map(|m| (*m).to_string()).collect(),
        linked_vscode_workspace: None,
        watch_vscode_workspace: false,
        default_use_worktree: true,
        appearance: AppearanceOverrides::default(),
        last_spawn_config: None,
    }
}

/// Dispatch `msg` and return the error the client would be sent.
pub(super) async fn dispatch_err(hub: &Hub, msg: ClientMessage) -> Option<String> {
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
    dispatch(hub, msg, &ctx).await.err().map(|e| e.to_string())
}

/// A lookup that knows only `A` (as `va`) in the process scope.
pub(super) fn lookup_a(name: &str) -> Option<(user_env::Secret, user_env::Origin)> {
    user_env::resolve_with(
        name,
        |n| (n == "A").then(|| "va".to_owned()),
        |_| None,
        |_| None,
    )
}

pub(super) fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

pub(super) fn env_spawn(
    target: SpawnTarget,
    mode: SessionMode,
    extra_env: Vec<(String, String)>,
    request_id: Option<&str>,
) -> SpawnRequest {
    SpawnRequest {
        label: None,
        target,
        mode,
        initial_prompt: None,
        dangerously_skip_permissions: false,
        agent_options: AgentOptions::claude(None),
        model: None,
        extra_env,
        secret_env_keys: Vec::new(),
        prompt_injector: None,
        request_id: request_id.map(str::to_owned),
        resume_conversation: None,
    }
}

pub(super) const UNSET_REF: &str = "${env:RT_TEST_UNSET_ENV_REF_4C1D}";

/// A `DEEPSEEK_API_KEY` value no stored file or reply may hold.
pub(super) const DEEPSEEK_KEY_SENTINEL: &str = "sk-deepseek-sentinel-a41c9e";

/// Claude options on the `DeepSeek` provider.
pub(super) fn deepseek_options() -> AgentOptions {
    AgentOptions::Claude {
        permission_mode: None,
        provider: protocol::ClaudeProvider::Deepseek,
    }
}

/// A lookup that knows no variable.
pub(super) fn no_key(_name: &str) -> Option<(user_env::Secret, user_env::Origin)> {
    None
}

/// A lookup that knows `DEEPSEEK_API_KEY` only.
pub(super) fn deepseek_key(name: &str) -> Option<(user_env::Secret, user_env::Origin)> {
    (name == "DEEPSEEK_API_KEY").then(|| {
        (
            user_env::Secret::new(DEEPSEEK_KEY_SENTINEL.to_owned()),
            user_env::Origin::Process,
        )
    })
}

pub(super) fn deepseek_spawn(
    target: SpawnTarget,
    mode: SessionMode,
    extra_env: Vec<(String, String)>,
    request_id: Option<&str>,
) -> SpawnRequest {
    SpawnRequest {
        agent_options: deepseek_options(),
        ..env_spawn(target, mode, extra_env, request_id)
    }
}

/// A syntactically valid secret id (32 lower-case hex) the store has never
/// seen, so a row written `${secret:…}` with it resolves nowhere.
pub(super) const MISSING_SECRET_ID: &str = "0123456789abcdef0123456789abcdef";

/// The `${secret:<id>}` reference standing for `id`.
pub(super) fn secret_ref(id: &str) -> String {
    format!("${{secret:{id}}}")
}
