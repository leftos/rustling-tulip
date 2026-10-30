//! Hub, scratch-directory, dispatch and git fixtures shared by the server's test modules.
#![expect(
    clippy::expect_used,
    reason = "fixtures assert preconditions with expect; failure messages aid debugging"
)]
use super::connection::{
    AuthKind, ConnCtx, FetchRegistry, NEXT_CONNECTION, PtyLane, client_session_until,
};
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
    AgentSelection, AppearanceOverrides, ClientMessage, DaemonMessage, OptionValue, OptionValues,
    SessionKind, SessionMetrics, SessionMode, SessionStatus, SpawnRequest, SpawnTarget, TabContent,
    TabEntry,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// A scratch directory for a hub's config, state and worktrees, removed
/// when the test ends.
pub(crate) struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    pub(crate) fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "rt-server-{tag}-{}-{}",
            std::process::id(),
            NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Self { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A hub over a scratch config dir with no repos registered.
pub(crate) fn test_hub(tag: &str) -> (Hub, ScratchDir) {
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
    let (keep_awake_mode, _) = tokio::sync::watch::channel(protocol::KeepAwakeMode::default());
    let (remote_enabled, _) = tokio::sync::watch::channel(false);
    let (_, keep_awake_status) = tokio::sync::watch::channel(crate::keep_awake::Status {
        mode: protocol::KeepAwakeMode::default(),
        active: false,
    });
    let devices = crate::devices::DeviceStore::load(&crate::paths::devices_file(&dirs));
    let dashboard = crate::dashboard::Dashboard::new(Arc::clone(&sessions))
        .with_pr_program(crate::dashboard::pr::NO_PROGRAM);
    let summarizer = test_summarizer(
        &state,
        &dirs,
        &client_count,
        &state_events,
        Arc::new(crate::summarizer::service::test_support::FakeCaller::default()),
    );
    let turns =
        crate::summarizer::turns::Turns::new(Arc::clone(&sessions), Arc::clone(&summarizer));
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
        preset_job_snapshots: Arc::default(),
        state_events,
        client_count: Arc::new(client_count),
        remote_enabled: Arc::new(remote_enabled),
        tailnet: Arc::new(AsyncMutex::new(None)),
        remote_status: Arc::new(tokio::sync::watch::Sender::new(RemoteSlot::initial(
            crate::remote_config::RemoteConfig::default(),
        ))),
        keep_awake_mode: Arc::new(keep_awake_mode),
        keep_awake_status,
        recovering: RecoveryClaims::default(),
        resume_in_flight: Arc::default(),
        start_resume: Arc::default(),
        resume_reported: Arc::default(),
        pending_hooks: crate::hook_status::PendingHooks::default(),
        live_lines: crate::hook_status::LiveLines::default(),
        devices: Arc::new(std::sync::Mutex::new(devices)),
        pairing: Arc::new(std::sync::Mutex::new(crate::devices::PairingBook::new())),
        pairing_waiters: super::pairing::PairingWaiters::default(),
        live_devices: super::live_devices::LiveDevices::default(),
        dashboard,
        summarizer,
        turns,
    };
    (hub, scratch, attention_rx)
}

/// A hub whose provider is a fake billing `cost_usd` per call, with one
/// client connected.
pub(crate) fn billing_hub(
    tag: &str,
    cost_usd: f64,
) -> (
    Hub,
    ScratchDir,
    Arc<crate::summarizer::service::test_support::FakeCaller>,
) {
    let (mut hub, scratch) = test_hub(tag);
    let fake = Arc::new(crate::summarizer::service::test_support::FakeCaller::billing(cost_usd));
    let (client_count, _) = tokio::sync::watch::channel(1usize);
    hub.client_count = Arc::new(client_count);
    hub.summarizer = test_summarizer(
        &hub.state,
        &hub.dirs,
        &hub.client_count,
        &hub.state_events,
        fake.clone(),
    );
    hub.turns = crate::summarizer::turns::Turns::new(
        Arc::clone(&hub.sessions),
        Arc::clone(&hub.summarizer),
    );
    (hub, scratch, fake)
}

/// A summarizer over the hub's state and config dir whose provider calls
/// `caller` answers.
pub(crate) fn test_summarizer(
    state: &Arc<AppState>,
    dirs: &Dirs,
    client_count: &tokio::sync::watch::Sender<usize>,
    state_events: &broadcast::Sender<super::StateEvent>,
    caller: Arc<dyn crate::summarizer::service::Caller>,
) -> Arc<crate::summarizer::service::Summarizer> {
    Arc::new(crate::summarizer::service::Summarizer::new(
        crate::summarizer::service::SummarizerParts {
            state: Arc::clone(state),
            spend_file: crate::paths::summarizer_spend_file(dirs),
            caller: Some(caller),
            rejections: Arc::default(),
            client_count: client_count.subscribe(),
            events: state_events.clone(),
        },
    ))
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
    let pty_lane: Arc<PtyLane> = Arc::default();
    let fetches: FetchRegistry = Arc::new(AsyncMutex::new(HashMap::new()));
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
        status_seq: 0,
        exit_code: None,
        metrics: SessionMetrics::default(),
        recent_actions: Vec::new(),
        pty: None,
        headless: None,
        workspace_id: None,
        agent: agent_registry::default_agent(),
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
        conversation_id: None,
        pending_input: None,
        last_hook_time: None,
        hook_driven: false,
        background: false,
        last_assistant_message: None,
        summary: None,
        busy_excluded: false,
        agent_in_shell: false,
        shell_label: None,
        agent_exit_announced: false,
        applied_size: None,
        hook_agent: None,
        hook_options: protocol::OptionValues::new(),
        hook_trusted: false,
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

/// The bound on every wait in these fixtures: a hang guard a passing run
/// never comes near, failing as "hung".
pub(super) const HANG_GUARD: Duration = Duration::from_secs(120);

/// Poll until `cond` holds, failing as hung, naming `what`, when it still
/// does not after [`HANG_GUARD`].
pub(super) async fn wait_until(what: &str, cond: impl Fn() -> bool) {
    let held = tokio::time::timeout(HANG_GUARD, async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(held.is_ok(), "hung waiting for {what}");
}

/// Every message dispatching `msg` puts on the wire, for a message that
/// may answer with none.
pub(super) async fn dispatch_all(hub: &Hub, msg: ClientMessage) -> Vec<DaemonMessage> {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    let (file_tx, _file_rx) = mpsc::channel(1);
    let pty_lane: Arc<PtyLane> = Arc::default();
    let fetches: FetchRegistry = Arc::new(AsyncMutex::new(HashMap::new()));
    let local = AuthKind::Local;
    let ctx = ConnCtx {
        out_tx: &out_tx,
        file_tx: &file_tx,
        pty_lane: &pty_lane,
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

/// The next message on `rx`, failing as hung when none arrives within
/// [`HANG_GUARD`].
pub(super) async fn next_message(rx: &mut mpsc::UnboundedReceiver<DaemonMessage>) -> DaemonMessage {
    tokio::time::timeout(HANG_GUARD, rx.recv())
        .await
        .expect("hung: no message arrived")
        .expect("the channel is open")
}

/// The terminal title [`publish_sentinel`] gives `s1`.
const SENTINEL_TITLE: &str = "rt-test-sentinel";

/// Publish an update to [`appearance_registry`]'s `s1` that marks the end of
/// what a test published. A forwarder delivers its registry's events in the
/// order they were sent, so once a connection reads the sentinel it has read
/// everything published before it.
pub(super) fn publish_sentinel(sessions: &SessionRegistry) {
    sessions.update("s1", |record| {
        record.terminal_title = Some(SENTINEL_TITLE.to_string());
    });
}

/// Every message on `rx` before the next [`publish_sentinel`] update.
pub(super) async fn messages_before_sentinel(
    rx: &mut mpsc::UnboundedReceiver<DaemonMessage>,
) -> Vec<DaemonMessage> {
    let mut before = Vec::new();
    loop {
        let message = next_message(rx).await;
        if let DaemonMessage::SessionUpdated { session, .. } = &message
            && session.terminal_title.as_deref() == Some(SENTINEL_TITLE)
        {
            return before;
        }
        before.push(message);
    }
}

/// The session id and `request_id` of the one message on `rx` before the
/// sentinel, which must be a `SessionUpdated`.
pub(super) async fn only_update(
    rx: &mut mpsc::UnboundedReceiver<DaemonMessage>,
) -> (String, Option<String>) {
    let mut before = messages_before_sentinel(rx).await;
    assert_eq!(before.len(), 1, "one message per update: {before:?}");
    match before.remove(0) {
        DaemonMessage::SessionUpdated {
            session,
            request_id,
        } => (session.id, request_id),
        other => unreachable!("only a SessionUpdated arrives, got {other:?}"),
    }
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

/// Seed `client_id` with one Dashboard tab.
pub(super) fn seed_dashboard_tab(hub: &Hub, client_id: &str) {
    hub.state
        .mutate_client_layout(client_id, |tabs| {
            tabs.push(TabEntry {
                id: "d1".to_string(),
                name: "Dashboard".to_string(),
                content: TabContent::Dashboard,
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

/// A WebSocket client connection to a scratch router.
pub(super) type WsClient = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

/// Serve `app` on a scratch loopback listener and return the bound port. The
/// serving task lives until the test's runtime ends.
async fn scratch_listener(app: axum::Router) -> u16 {
    let listener = super::bind_loopback()
        .await
        .expect("bind a scratch listener");
    let addr = listener.local_addr().expect("the bound address");
    tokio::spawn(async move { axum::serve(listener, app).await });
    addr.port()
}

/// Serve `hub`'s loopback router on a scratch listener and return its port.
pub(crate) async fn serve_loopback(hub: &Hub) -> u16 {
    scratch_listener(super::build_loopback_router(hub.clone())).await
}

/// Serves one connection to [`unproven_listener`]: a `GET /proof` gets a
/// 404 and `None`; any other connection returns every byte it sent, once
/// it closes.
fn serve_unproven(mut stream: std::net::TcpStream) -> Option<Vec<u8>> {
    use std::io::{Read as _, Write as _};
    let mut received = Vec::new();
    let mut byte = [0u8; 1];
    while !received.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => received.push(byte[0]),
            _ => break,
        }
    }
    if received.starts_with(b"GET /proof ") {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return None;
    }
    let _ = stream.read_to_end(&mut received);
    Some(received)
}

/// A loopback listener that proves no token: see [`serve_unproven`]. The
/// receiver gets the bytes of each connection that was not a proof
/// request, once the client closes it.
pub(crate) fn unproven_listener() -> (u16, std::sync::mpsc::Receiver<Vec<u8>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let port = listener.local_addr().expect("listener address").port();
    let (received_tx, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let received_tx = received_tx.clone();
            std::thread::spawn(move || {
                if let Some(bytes) = serve_unproven(stream) {
                    let _ = received_tx.send(bytes);
                }
            });
        }
    });
    (port, received)
}

/// Serve `app` on a scratch loopback listener and open a WebSocket client
/// to `url`'s path on it; `url`'s authority is what the upgrade's `Host`
/// carries. The serving task lives until the test's runtime ends.
async fn scratch_ws_client(app: axum::Router, url: impl FnOnce(u16) -> String) -> WsClient {
    let port = scratch_listener(app).await;
    let tcp = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .expect("connect to the scratch listener");
    let (client, _) = tokio_tungstenite::client_async(url(port), tcp)
        .await
        .expect("the WebSocket upgrade succeeds");
    client
}

/// A tailnet `/ws` connection the listener's acceptor admitted as `peer`,
/// served over plain ws on a scratch listener, and the token of the listener
/// server it was accepted under: cancelling it stands for that server ending.
pub(super) async fn tailnet_ws_client(
    hub: &Hub,
    peer: tailnet::PeerIdentity,
) -> (WsClient, tokio_util::sync::CancellationToken) {
    let server_life = tokio_util::sync::CancellationToken::new();
    let client = tailnet_ws_client_under(hub, peer, server_life.clone()).await;
    (client, server_life)
}

/// [`tailnet_ws_client`] accepted under `server_life`, such as one from
/// the hub's own listener.
pub(super) async fn tailnet_ws_client_under(
    hub: &Hub,
    peer: tailnet::PeerIdentity,
    server_life: tokio_util::sync::CancellationToken,
) -> WsClient {
    let names = super::remote::name_slot();
    super::remote::set_serving_name(&names, Some("host.tail1234.ts.net".to_owned()));
    let app = super::remote::build_tailnet_router(hub.clone(), names, 8787).layer(axum::Extension(
        crate::tailnet_listener::AuthorizedPeer::for_tests(peer, server_life),
    ));
    scratch_ws_client(app, |_| "ws://host.tail1234.ts.net:8787/ws".to_owned()).await
}

/// A loopback `/ws` connection, served on a scratch listener.
pub(super) async fn loopback_ws_client(hub: &Hub) -> WsClient {
    let app = super::build_loopback_router(hub.clone());
    scratch_ws_client(app, |port| format!("ws://127.0.0.1:{port}/ws")).await
}

/// Send `msg` as one text frame.
pub(super) async fn ws_send(client: &mut WsClient, msg: &ClientMessage) {
    use futures::SinkExt as _;
    let json = serde_json::to_string(msg).expect("serialize a client message");
    client
        .send(tokio_tungstenite::tungstenite::Message::text(json))
        .await
        .expect("send a frame");
}

/// The next daemon message on `client`, or `None` once the daemon closed
/// the connection. No hang guard: the caller sets one.
pub(super) async fn ws_frame(client: &mut WsClient) -> Option<DaemonMessage> {
    use futures::StreamExt as _;
    use tokio_tungstenite::tungstenite::Message;
    loop {
        match client.next().await {
            None | Some(Err(_) | Ok(Message::Close(_))) => return None,
            Some(Ok(Message::Text(text))) => {
                return Some(serde_json::from_str(text.as_str()).expect("a daemon message"));
            }
            Some(Ok(_)) => {}
        }
    }
}

/// [`ws_frame`] under a hang guard far past any passing run.
pub(super) async fn ws_next(client: &mut WsClient) -> Option<DaemonMessage> {
    tokio::time::timeout(Duration::from_secs(120), ws_frame(client))
        .await
        .expect("the daemon answers or closes the connection")
}

/// The daemon's end of an in-memory connection ([`mem_session`]): the frames
/// the test client sent, and a sink into the client that takes no frame while
/// its stall switch is on, as a socket whose peer stopped reading, until the
/// client resumes reading.
pub(super) struct MemSocket {
    from_client: mpsc::UnboundedReceiver<WsFrame>,
    to_client: Option<mpsc::UnboundedSender<WsFrame>>,
    stalled: Arc<AtomicBool>,
    resumed: Arc<futures::task::AtomicWaker>,
}

/// A frame as the daemon's side of a socket sees it.
type WsFrame = axum::extract::ws::Message;

impl futures::Stream for MemSocket {
    type Item = Result<WsFrame, axum::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.from_client.poll_recv(cx).map(|frame| frame.map(Ok))
    }
}

impl futures::Sink<WsFrame> for MemSocket {
    type Error = axum::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.resumed.register(cx.waker());
        if self.stalled.load(Ordering::SeqCst) {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn start_send(self: Pin<&mut Self>, frame: WsFrame) -> Result<(), Self::Error> {
        let sent = self
            .to_client
            .as_ref()
            .is_some_and(|tx| tx.send(frame).is_ok());
        if sent {
            Ok(())
        } else {
            Err(axum::Error::new("the test client is gone"))
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.to_client = None;
        Poll::Ready(Ok(()))
    }
}

/// The test's end of an in-memory connection ([`mem_session`]).
pub(super) struct MemClient {
    to_daemon: mpsc::UnboundedSender<WsFrame>,
    from_daemon: mpsc::UnboundedReceiver<WsFrame>,
    stalled: Arc<AtomicBool>,
    resumed: Arc<futures::task::AtomicWaker>,
    /// The `Ping` frames [`MemClient::next`] has read.
    pub(super) pings: usize,
}

impl MemClient {
    /// Stop taking the daemon's frames, as a peer that stopped reading.
    pub(super) fn stall(&self) {
        self.stalled.store(true, Ordering::SeqCst);
    }

    /// Take the daemon's frames again after [`MemClient::stall`].
    pub(super) fn resume(&self) {
        self.stalled.store(false, Ordering::SeqCst);
        self.resumed.wake();
    }

    /// Send `msg` as one text frame.
    pub(super) fn send(&self, msg: &ClientMessage) {
        let json = serde_json::to_string(msg).expect("serialize a client message");
        self.to_daemon
            .send(WsFrame::Text(json.into()))
            .expect("the daemon reads the connection");
    }

    /// The next daemon message, answering each `Ping` before it with a `Pong`
    /// when `answer_pings`; `None` once the daemon closed the connection. No
    /// hang guard: the caller sets one.
    pub(super) async fn next(&mut self, answer_pings: bool) -> Option<DaemonMessage> {
        loop {
            match self.from_daemon.recv().await? {
                WsFrame::Text(text) => {
                    return Some(serde_json::from_str(text.as_str()).expect("a daemon message"));
                }
                WsFrame::Ping(_) => {
                    self.pings += 1;
                    if answer_pings {
                        let _ = self.to_daemon.send(WsFrame::Pong(axum::body::Bytes::new()));
                    }
                }
                WsFrame::Close(_) => return None,
                WsFrame::Binary(_) | WsFrame::Pong(_) => {}
            }
        }
    }
}

/// An in-memory connection served by `client_session_until` as `auth` (a
/// tailnet one under a server life nothing cancels), and the task serving it,
/// which ends when the connection does. No socket and no I/O, so a test on a
/// paused clock sees time move only when every task waits on a timer.
pub(super) fn mem_session(hub: &Hub, auth: AuthKind) -> (MemClient, tokio::task::JoinHandle<()>) {
    let (to_daemon, from_client) = mpsc::unbounded_channel();
    let (to_client, from_daemon) = mpsc::unbounded_channel();
    let stalled = Arc::new(AtomicBool::new(false));
    let resumed = Arc::new(futures::task::AtomicWaker::new());
    let socket = MemSocket {
        resumed: Arc::clone(&resumed),
        from_client,
        to_client: Some(to_client),
        stalled: Arc::clone(&stalled),
    };
    let server_life = matches!(auth, AuthKind::Tailnet(_)).then(CancellationToken::new);
    let session = tokio::spawn(client_session_until(hub.clone(), socket, auth, server_life));
    let client = MemClient {
        to_daemon,
        from_daemon,
        stalled,
        resumed,
        pings: 0,
    };
    (client, session)
}

/// A current-version `Hello` carrying `auth_token` and `client_name`.
pub(super) fn hello(auth_token: &str, client_name: Option<&str>) -> ClientMessage {
    ClientMessage::Hello {
        protocol_version: protocol::PROTOCOL_VERSION,
        protocol_versions: vec![protocol::PROTOCOL_VERSION],
        auth_token: auth_token.to_owned(),
        client_id: Some("test-client".to_owned()),
        client_name: client_name.map(str::to_owned),
    }
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
    let pty_lane: Arc<PtyLane> = Arc::default();
    let fetches: FetchRegistry = Arc::new(AsyncMutex::new(HashMap::new()));
    let local = AuthKind::Local;
    let ctx = ConnCtx {
        out_tx: &out_tx,
        file_tx: &file_tx,
        pty_lane: &pty_lane,
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
        agent: (mode != SessionMode::PlainShell).then(|| AgentSelection {
            agent: agent_registry::default_agent(),
            options: validated_claude(&OptionValues::new()),
        }),
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

/// Claude's options as a spawn stores them: `options` with every other
/// field's default, as its schema validates them.
pub(crate) fn validated_claude(options: &OptionValues) -> OptionValues {
    let claude =
        agent_registry::plugin(&agent_registry::default_agent()).expect("Claude is registered");
    agent_api::validate(claude.descriptor(), options).expect("valid Claude options")
}

/// Claude options on the `DeepSeek` provider.
pub(super) fn deepseek_options() -> OptionValues {
    validated_claude(&OptionValues::from([(
        "provider".to_owned(),
        OptionValue::Text("deepseek".to_owned()),
    )]))
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
        agent: Some(AgentSelection {
            agent: agent_registry::default_agent(),
            options: deepseek_options(),
        }),
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
