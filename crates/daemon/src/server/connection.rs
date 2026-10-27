//! One client WebSocket connection: its id and auth kind, the client count, the event forwarders, the send pump and the receive loop.
use super::handshake::{handshake, push_initial_state, send_resume_report};
use super::live_devices::LiveConnection;
use super::pairing::pairing_session;
use super::sessions::spawn_session_forwarder;
use super::tabs::ensure_client_layout;
use super::{Hub, PresetEvent, ScopedTabEvent, StateEvent, TabEvent, dispatch};
use crate::file_fetch;
use crate::state::AppState;
use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt as _, StreamExt as _};
use protocol::{DaemonMessage, InboundClientMessage};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// The id the next client connection takes.
pub(super) static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);

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

/// How a connection authenticates. The loopback listener compares the
/// `daemon.json` token; the tailnet listener never looks at it and instead
/// reads the presented value as the device's token.
#[derive(Clone, Debug)]
pub(super) enum AuthKind {
    Local,
    Tailnet(tailnet::PeerIdentity),
}

/// How long a connection whose listener server ended may take to finish the
/// closing handshake before its socket is dropped.
const SERVER_ENDED_CLOSE_LIMIT: std::time::Duration = std::time::Duration::from_secs(2);

/// How long a refused handshake's connection has to finish sending its
/// refusal and the closing handshake before its socket is dropped.
const REFUSED_CLOSE_LIMIT: std::time::Duration = std::time::Duration::from_secs(2);

/// A loopback connection: [`client_session_until`] with no server life, so
/// nothing but the peer or the daemon's shutdown ends it.
pub(super) async fn client_session(hub: Hub, socket: WebSocket, auth: AuthKind) {
    client_session_until(hub, socket, auth, None).await;
}

/// Serve one connection until the peer closes it, the daemon shuts down, or
/// `server_life` (the life of the tailnet listener server that accepted it,
/// `None` on loopback) is cancelled. A connection whose server ended is sent
/// `Close` without what was still queued for it, and tears down as a
/// peer-closed one does.
pub(super) async fn client_session_until(
    hub: Hub,
    socket: WebSocket,
    auth: AuthKind,
    server_life: Option<CancellationToken>,
) {
    // This connection's own life: a child of its server's, and never
    // cancelled on loopback.
    let life = server_life.map_or_else(CancellationToken::new, |server| server.child_token());
    // RAII-tracked so `git_watch` can pause its refreshers when no UI is
    // listening. Dropped on normal return and on panic, and as soon as the
    // handshake puts the connection in pairing mode: a device that may only
    // pair is no UI.
    let client_guard = ClientCountGuard::new(Arc::clone(&hub.client_count));
    let (sender, mut receiver) = socket.split();

    // Each outgoing message goes through this channel so PTY/event tasks can
    // push to the WS without sharing the sink across tasks.
    let (out_tx, out_rx) = mpsc::unbounded_channel::<DaemonMessage>();
    // File transfers get their own small bounded queue: a slow socket stalls
    // the file read instead of buffering the whole file in memory.
    let (file_tx, file_rx) = mpsc::channel::<DaemonMessage>(4);

    let send_task = tokio::spawn(send_pump(sender, out_rx, file_rx, life.clone()));

    // Mandatory handshake.
    let mut handshake = match handshake(&hub, &auth, &life, &mut receiver, &out_tx).await {
        Ok(Some(v)) => v,
        Ok(None) => {
            info!("client_session: the listener server ended during the handshake; closing");
            end_served(send_task, &mut receiver, &life).await;
            return;
        }
        Err(err) => {
            warn!(?err, "client handshake failed");
            let _ = out_tx.send(DaemonMessage::AuthFailed {
                reason: err.to_string(),
            });
            drop(client_guard);
            drop(out_tx);
            end_refused_handshake(send_task, &life, &mut receiver).await;
            return;
        }
    };
    // An unpaired tailnet device gets the pairing loop and nothing else: no
    // layout, no forwarder, no initial state.
    if handshake.pairing
        && let AuthKind::Tailnet(peer) = &auth
    {
        drop(client_guard);
        pairing_session(&hub, &mut receiver, &out_tx, peer, &life).await;
        drop(file_tx);
        drop(out_tx);
        // The device may still be sending when its session ends; a proper
        // close keeps the last reply from being lost to a reset. What was
        // already queued for it (a decision, a refusal) goes first, even
        // when its server ended.
        let limit = hub.tailnet_handshake_timeout;
        end_within(send_task, limit, &life, async |(sender, queued)| {
            send_queued_then_close(sender, queued, &mut receiver).await;
        })
        .await;
        return;
    }
    // An approved device's connection was registered in the handshake, before
    // anything but `Welcome` went out; it stays registered, so a block can cut
    // it, until this function returns on any path.
    let live = handshake.live.take();
    if let Some(device) = &handshake.device {
        info!(%device, "client_session: serving an approved tailnet device");
    }
    // Effective per-client layout key: the client's id, or the shared legacy
    // key for connections that don't send one. Same value drives the tab-event
    // filter, push_initial_state, and every tab handler in `dispatch`.
    let client_id = handshake
        .client_id
        .clone()
        .unwrap_or_else(|| crate::state::LEGACY_CLIENT_ID.to_string());
    let client_name = handshake.client_name.clone();
    // A real client_id the daemon has never seen gets the first-connect chooser
    // (LayoutInitRequired instead of Tabs). Legacy/no-id clients can't handle
    // the chooser, so they auto-init the shared legacy layout.
    let needs_layout_chooser =
        client_id != crate::state::LEGACY_CLIENT_ID && !hub.state.has_client_layout(&client_id);
    if !needs_layout_chooser {
        ensure_client_layout(&hub, &client_id, client_name.as_deref());
    }

    // Per-session PTY forwarder tasks. Each task subscribes directly to a
    // session's `pty.output` broadcast and pumps bytes into this client's
    // `out_tx`. Bypasses the registry's global SessionEvent channel for the
    // PTY stream — that single 256-msg channel could saturate under heavy
    // TUI redraw bursts, dropping output for every client (see daemon log
    // `client event stream lagged`). One forwarder per (client, session)
    // pair gives each session its own backpressure boundary.
    //
    // Lifetimes:
    //   - Spawned by `LoadScrollback` handler (atomically with the scrollback
    //     snapshot read so the live receiver starts at exactly the byte after
    //     the snapshot).
    //   - Aborted by `Detach` handler when the client unmounts a session's
    //     Terminal, and on client_session teardown below.
    let pty_forwarders = Arc::new(AsyncMutex::new(HashMap::<
        String,
        tokio::task::JoinHandle<()>,
    >::new()));
    // Active `FetchFile` transfers by client-assigned id; cancelled by
    // `CancelFetch` and on teardown below.
    let fetches: FetchRegistry = Arc::new(AsyncMutex::new(HashMap::new()));

    // Subscribe to global session events. After splitting PtyOutput out, this
    // channel only carries low-volume control events.
    let connection = NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed);
    let event_task = spawn_session_forwarder(Arc::clone(&hub.sessions), out_tx.clone(), connection);

    // Subscribe to tab-layout and preset-launch events. Tab events are
    // filtered to this connection's `client_id` (per-client layouts); preset
    // and registry events are forwarded to every client.
    let tab_event_task = spawn_tab_forwarder(
        hub.tab_events.clone(),
        Arc::clone(&hub.state),
        out_tx.clone(),
        client_id.clone(),
    );
    let preset_event_task = spawn_preset_forwarder(hub.preset_events.subscribe(), out_tx.clone());
    let state_event_task = spawn_state_forwarder(
        &hub,
        hub.state_events.subscribe(),
        out_tx.clone(),
        client_id.clone(),
        matches!(auth, AuthKind::Local),
    );

    // Send initial state snapshots: this client's layout, or the first-connect
    // chooser when it has none yet.
    push_initial_state(&hub, &out_tx, &client_id, needs_layout_chooser);

    let ctx = ConnCtx {
        out_tx: &out_tx,
        file_tx: &file_tx,
        pty_forwarders: &pty_forwarders,
        fetches: &fetches,
        client_id: &client_id,
        client_name: client_name.as_deref(),
        connection,
        auth: &auth,
    };
    recv_loop(&hub, &mut receiver, &ctx, &life, live.as_ref()).await;

    info!("client_session: recv_loop returned; tearing down");
    stop_transfers(&pty_forwarders, &fetches).await;
    drop(file_tx);
    drop(out_tx);
    event_task.abort();
    tab_event_task.abort();
    preset_event_task.abort();
    state_event_task.abort();
    end_served(send_task, &mut receiver, &life).await;
    info!("client_session: returning");
}

/// What the send pump hands back when it ends: the sink and the queue.
type PumpEnd = (Sink, mpsc::UnboundedReceiver<DaemonMessage>);

/// End a served (or still handshaking) connection: once its server ended,
/// with `Close` within [`SERVER_ENDED_CLOSE_LIMIT`] and whatever is still
/// queued dropped; otherwise once the pump has drained, dropping the socket.
async fn end_served(
    send_task: tokio::task::JoinHandle<PumpEnd>,
    receiver: &mut futures::stream::SplitStream<WebSocket>,
    life: &CancellationToken,
) {
    if !life.is_cancelled() {
        let _ = send_task.await;
        return;
    }
    end_within(
        send_task,
        SERVER_ENDED_CLOSE_LIMIT,
        life,
        async |(sender, _queued)| {
            close_gracefully(sender, receiver).await;
        },
    )
    .await;
}

/// Abort every PTY forwarder and cancel every file fetch of a connection
/// that is going.
async fn stop_transfers(pty_forwarders: &PtyForwarders, fetches: &FetchRegistry) {
    for (_, handle) in pty_forwarders.lock().await.drain() {
        handle.abort();
    }
    for (_, token) in fetches.lock().await.drain() {
        token.cancel();
    }
}

/// Wait for the send pump to hand back what it holds, then run `close` on
/// it, all within `limit`, cut short to [`SERVER_ENDED_CLOSE_LIMIT`] once
/// `life` is cancelled. Past the deadline the pump is aborted and whatever
/// it or `close` held (the socket) is dropped.
async fn end_within<S>(
    send_task: tokio::task::JoinHandle<S>,
    limit: std::time::Duration,
    life: &CancellationToken,
    close: impl AsyncFnOnce(S),
) {
    let abort = send_task.abort_handle();
    let finish = async move {
        if let Ok(held) = send_task.await {
            close(held).await;
        }
    };
    let server_ended = async {
        life.cancelled().await;
        tokio::time::sleep(SERVER_ENDED_CLOSE_LIMIT).await;
    };
    let finished = tokio::select! {
        biased;
        () = finish => true,
        () = tokio::time::sleep(limit) => false,
        () = server_ended => false,
    };
    if !finished {
        abort.abort();
        debug!("the connection did not finish closing in time; dropping the socket");
    }
}

/// The sink of a connection's WebSocket.
type Sink = futures::stream::SplitSink<WebSocket, Message>;

/// End a connection whose handshake was refused: send the refusal still
/// queued for it, then close it with the closing handshake, within
/// [`REFUSED_CLOSE_LIMIT`]. The client's next frame may still be unread here,
/// and a bare drop would reset the socket, which can destroy the refusal
/// before the client reads it.
async fn end_refused_handshake(
    send_task: tokio::task::JoinHandle<PumpEnd>,
    life: &CancellationToken,
    receiver: &mut futures::stream::SplitStream<WebSocket>,
) {
    end_within(
        send_task,
        REFUSED_CLOSE_LIMIT,
        life,
        async |(sender, queued)| {
            send_queued_then_close(sender, queued, receiver).await;
        },
    )
    .await;
}

/// Send `msg` as one text frame; `false` once the socket has failed.
async fn send_message(sender: &mut Sink, msg: &DaemonMessage) -> bool {
    match serde_json::to_string(msg) {
        Ok(json) => sender.send(Message::Text(json.into())).await.is_ok(),
        Err(err) => {
            error!(?err, "serializing daemon msg");
            true
        }
    }
}

/// Send what is still `queued` for a connection that is ending (a pairing
/// connection, or a failed handshake), then close it with the closing
/// handshake.
async fn send_queued_then_close(
    mut sender: Sink,
    mut queued: mpsc::UnboundedReceiver<DaemonMessage>,
    receiver: &mut futures::stream::SplitStream<WebSocket>,
) {
    while let Ok(msg) = queued.try_recv() {
        if !send_message(&mut sender, &msg).await {
            return;
        }
    }
    close_gracefully(sender, receiver).await;
}

/// Forwards tab-layout events for `client_id` to the connection's `out_tx`. A
/// connection that falls behind the broadcast buffer gets its whole tab list
/// in place of the events it missed, and the events after it.
fn spawn_tab_forwarder(
    sender: broadcast::Sender<ScopedTabEvent>,
    state: Arc<AppState>,
    out_tx: mpsc::UnboundedSender<DaemonMessage>,
    client_id: String,
) -> tokio::task::JoinHandle<()> {
    // Subscribed before the task starts, so no event sent after this call
    // is missed.
    let mut rx = sender.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(scoped) => {
                    if scoped.client_id != client_id {
                        continue;
                    }
                    match scoped.event {
                        TabEvent::Updated(tab) => {
                            let _ = out_tx.send(DaemonMessage::TabUpdated { tab });
                        }
                        TabEvent::Removed(tab_id) => {
                            let _ = out_tx.send(DaemonMessage::TabRemoved { tab_id });
                        }
                        TabEvent::Reordered(ids) => {
                            let _ = out_tx.send(DaemonMessage::TabsReordered { ordered_ids: ids });
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(
                        lagged = n,
                        %client_id,
                        "client tab event stream lagged; resending the tab list"
                    );
                    // Subscribe before taking the layout: the stale buffered
                    // events go with the old receiver, and every event after
                    // the list applies on top of it.
                    rx = sender.subscribe();
                    let tabs = state.client_layout(&client_id);
                    let _ = out_tx.send(DaemonMessage::Tabs { tabs });
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Forwards this connection's share of the registry's state events. The
/// start-up resume's report reaches a client at most once per daemon run: a
/// connection whose client id the initial-state push already told drops it.
/// The waiting pairing requests reach only a loopback connection (`local`).
#[expect(
    clippy::too_many_lines,
    reason = "one arm per state event; each arm only forwards its message"
)]
fn spawn_state_forwarder(
    hub: &Hub,
    mut rx: broadcast::Receiver<StateEvent>,
    out_tx: mpsc::UnboundedSender<DaemonMessage>,
    client_id: String,
    local: bool,
) -> tokio::task::JoinHandle<()> {
    let hub = hub.clone();
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
                    worktree_path,
                    index_changes,
                    worktree_changes,
                }) => {
                    let _ = out_tx.send(DaemonMessage::RepoStatus {
                        repo_id,
                        worktree_path,
                        index_changes,
                        worktree_changes,
                    });
                }
                Ok(StateEvent::Stashes {
                    repo_id,
                    worktree_path,
                    stashes,
                }) => {
                    let _ = out_tx.send(DaemonMessage::Stashes {
                        repo_id,
                        worktree_path,
                        stashes,
                    });
                }
                Ok(StateEvent::WorktreesRootChanged { root, is_override }) => {
                    let _ = out_tx.send(DaemonMessage::WorktreesRootChanged { root, is_override });
                }
                Ok(StateEvent::KeepAwakeStatus { mode, active }) => {
                    let _ = out_tx.send(DaemonMessage::KeepAwakeStatus {
                        enabled: mode != protocol::KeepAwakeMode::Off,
                        mode,
                        active,
                    });
                }
                Ok(StateEvent::AgentCommands(commands)) => {
                    let _ = out_tx.send(DaemonMessage::AgentCommands { commands });
                }
                Ok(StateEvent::AgentSettings(settings)) => {
                    let _ = out_tx.send(DaemonMessage::AgentSettings { settings });
                }
                Ok(StateEvent::RemoteStatus {
                    enabled,
                    port,
                    state,
                }) => {
                    let _ = out_tx.send(DaemonMessage::RemoteStatus {
                        enabled,
                        port,
                        state,
                    });
                }
                Ok(StateEvent::ResumeReport {
                    resumed,
                    left_in_recover,
                }) => {
                    if hub.claim_resume_report(&client_id) {
                        let _ = out_tx.send(DaemonMessage::ResumeReport {
                            resumed,
                            left_in_recover,
                        });
                    }
                }
                Ok(StateEvent::SessionHistory(items)) => {
                    let _ = out_tx.send(DaemonMessage::SessionHistory {
                        request_id: None,
                        items,
                    });
                }
                Ok(StateEvent::PendingDevices(requests)) => {
                    if local {
                        let _ = out_tx.send(DaemonMessage::PendingDevices { requests });
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(lagged = n, "client state event stream lagged");
                    // The report is sent once and no later event repeats it,
                    // so a stream that lagged past it asks for it again here.
                    send_resume_report(&hub, &out_tx, &client_id);
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
                Ok(PresetEvent::JobUpdated { job }) => {
                    let _ = out_tx.send(DaemonMessage::PresetLaunchJobUpdated { job });
                }
                Ok(PresetEvent::Progress {
                    job_id,
                    preset_id,
                    total,
                    launched,
                    current_tab_id,
                    tab_ids,
                }) => {
                    let _ = out_tx.send(DaemonMessage::PresetLaunchProgress {
                        job_id,
                        preset_id,
                        total,
                        launched,
                        current_tab_id,
                        tab_ids,
                    });
                }
                Ok(PresetEvent::Failed {
                    job_id,
                    preset_id,
                    error,
                    partial_session_ids,
                    partial_tab_ids,
                }) => {
                    let _ = out_tx.send(DaemonMessage::PresetLaunchFailed {
                        job_id,
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

pub(super) type PtyForwarders = Arc<AsyncMutex<HashMap<String, tokio::task::JoinHandle<()>>>>;

pub(super) type FetchRegistry = Arc<AsyncMutex<HashMap<String, CancellationToken>>>;

/// Per-connection handles that [`dispatch`] needs beyond the [`Hub`].
#[derive(Clone, Copy)]
pub(super) struct ConnCtx<'a> {
    pub(super) out_tx: &'a mpsc::UnboundedSender<DaemonMessage>,
    /// Bounded, lower-priority queue for `FetchFile` transfer messages.
    pub(super) file_tx: &'a mpsc::Sender<DaemonMessage>,
    pub(super) pty_forwarders: &'a PtyForwarders,
    pub(super) fetches: &'a FetchRegistry,
    pub(super) client_id: &'a str,
    pub(super) client_name: Option<&'a str>,
    /// This connection's id, unique for the daemon's run: a client may hold
    /// several connections under one `client_id`.
    pub(super) connection: u64,
    /// How this connection authenticated, deciding which messages it may use.
    pub(super) auth: &'a AuthKind,
}

/// Drain a connection's outbound queues into its WebSocket. Control and PTY
/// traffic on `out_rx` always goes before file-transfer messages on
/// `file_rx`. Ends when `out_rx` closes, the socket fails, or `life` is
/// cancelled; `file_rx` closing only retires its own arm. Hands the sink
/// back, so a caller can still close the socket properly, with `out_rx` and
/// whatever is still queued on it, for the caller to send or drop.
async fn send_pump(
    mut sender: Sink,
    mut out_rx: mpsc::UnboundedReceiver<DaemonMessage>,
    mut file_rx: mpsc::Receiver<DaemonMessage>,
    life: CancellationToken,
) -> (Sink, mpsc::UnboundedReceiver<DaemonMessage>) {
    let mut file_open = true;
    loop {
        let msg = tokio::select! {
            biased;
            () = life.cancelled() => break,
            msg = out_rx.recv() => match msg {
                Some(msg) => msg,
                None => break,
            },
            msg = file_rx.recv(), if file_open => {
                let Some(msg) = msg else {
                    file_open = false;
                    continue;
                };
                msg
            }
        };
        if !send_message(&mut sender, &msg).await {
            break;
        }
    }
    (sender, out_rx)
}

/// End a connection with the WebSocket closing handshake once everything
/// queued for it is on the wire: send `Close`, then read and discard what the
/// peer still sends until its `Close` or the end of the stream. The caller
/// bounds it ([`end_within`]). Dropping a socket that still holds unread
/// frames resets it, and the reset can destroy replies the peer has not read
/// yet.
async fn close_gracefully(
    mut sender: Sink,
    receiver: &mut futures::stream::SplitStream<WebSocket>,
) {
    if sender.send(Message::Close(None)).await.is_err() {
        return;
    }
    while let Some(Ok(message)) = receiver.next().await {
        if matches!(message, Message::Close(_)) {
            break;
        }
    }
}

/// Start streaming a confined file for `FetchFile`, or answer with
/// `FileFetchError` when the path was refused or `id` is already active.
pub(super) async fn start_fetch(ctx: &ConnCtx<'_>, id: String, target: anyhow::Result<PathBuf>) {
    let path = match target {
        Ok(path) => path,
        Err(err) => {
            let error = format!("{err:#}");
            let _ = ctx.out_tx.send(DaemonMessage::FileFetchError { id, error });
            return;
        }
    };
    let token = CancellationToken::new();
    let duplicate = match ctx.fetches.lock().await.entry(id.clone()) {
        Entry::Occupied(_) => true,
        Entry::Vacant(slot) => {
            slot.insert(token.clone());
            false
        }
    };
    if duplicate {
        let _ = ctx.out_tx.send(DaemonMessage::FileFetchError {
            id,
            error: "duplicate fetch id".to_string(),
        });
        return;
    }
    let fetches = Arc::clone(ctx.fetches);
    let file_tx = ctx.file_tx.clone();
    tokio::spawn(async move {
        file_fetch::stream_file(id.clone(), path, file_tx, token.clone()).await;
        let mut fetches = fetches.lock().await;
        // A cancelled entry was already removed by whoever cancelled it, and
        // the id may since belong to a newer fetch.
        if !token.is_cancelled() {
            fetches.remove(&id);
        }
    });
}

/// Cancel and forget the `FetchFile` transfer `id`, if it is still active.
pub(super) async fn cancel_fetch(fetches: &FetchRegistry, id: &str) {
    let mut fetches = fetches.lock().await;
    if let Some(token) = fetches.remove(id) {
        token.cancel();
    } else {
        debug!(%id, "CancelFetch for an unknown or finished fetch id");
    }
}

/// Run the WebSocket receive loop until the peer closes, the daemon-wide
/// shutdown flag flips, or the connection's `life` is cancelled. Honoring
/// `shutdown_tx` here is what lets axum's graceful-shutdown future resolve
/// after a [`ClientMessage::Shutdown`] — otherwise every `recv()` blocks
/// forever and the daemon hangs.
async fn recv_loop(
    hub: &Hub,
    receiver: &mut futures::stream::SplitStream<WebSocket>,
    ctx: &ConnCtx<'_>,
    life: &CancellationToken,
    live: Option<&LiveConnection>,
) {
    let out_tx = ctx.out_tx;
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
            () = life.cancelled() => {
                if live.is_some_and(LiveConnection::is_cut) {
                    info!("recv_loop: the device was blocked, forgotten or paired again; closing");
                } else {
                    info!("recv_loop: the tailnet listener server that accepted this connection ended; closing");
                }
                break;
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
                            request_id: None,
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
                if let Err(err) = dispatch(hub, parsed, ctx).await {
                    let _ = out_tx.send(DaemonMessage::Error {
                        message: err.to_string(),
                        request_id: None,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::server::test_support::{
        WsClient, hello, loopback_ws_client, next_message, tailnet_ws_client, test_hub, test_peer,
        ws_next, ws_send,
    };
    use std::time::Duration;

    /// Read `client` until the daemon ends the connection: whether a `Close`
    /// frame came before the end. Fails at a guard far past any passing run.
    async fn closed_with_a_close_frame(client: &mut WsClient) -> bool {
        use tokio_tungstenite::tungstenite::Message as WsMessage;
        let ended = tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                match client.next().await {
                    Some(Ok(WsMessage::Close(_))) => return true,
                    None | Some(Err(_)) => return false,
                    Some(Ok(_)) => {}
                }
            }
        })
        .await;
        ended.expect("the daemon ends the connection")
    }

    /// Read `client` until a message `stop` accepts, failing if the
    /// connection ends first or a pairing refusal comes.
    async fn read_until(client: &mut WsClient, stop: impl Fn(&DaemonMessage) -> bool) {
        loop {
            let message = ws_next(client)
                .await
                .expect("a message before the connection closes");
            assert!(
                !matches!(message, DaemonMessage::DevicePairFailed { .. }),
                "the connection was not served: {message:?}"
            );
            if stop(&message) {
                return;
            }
        }
    }

    fn is_remote_status(message: &DaemonMessage) -> bool {
        matches!(message, DaemonMessage::RemoteStatus { .. })
    }

    #[tokio::test]
    async fn served_tailnet_session_ends_when_its_server_token_is_cancelled() {
        let (hub, _scratch) = test_hub("served-server-ended");
        let (_, token) = crate::sync::lock(&hub.devices)
            .issue(&test_peer(), chrono::Utc::now())
            .expect("issue a device token");
        let (mut client, server_life) = tailnet_ws_client(&hub, test_peer()).await;
        ws_send(&mut client, &hello(&token, None)).await;
        read_until(&mut client, is_remote_status).await;

        server_life.cancel();
        assert!(
            closed_with_a_close_frame(&mut client).await,
            "the daemon sends Close before it ends the connection"
        );
    }

    #[tokio::test]
    async fn revoking_a_device_closes_its_served_tailnet_connection() {
        let (hub, _scratch) = test_hub("served-device-revoked");
        let (device_id, token) = crate::sync::lock(&hub.devices)
            .issue(&test_peer(), chrono::Utc::now())
            .expect("issue a device token");
        let (mut client, _server_life) = tailnet_ws_client(&hub, test_peer()).await;
        ws_send(&mut client, &hello(&token, None)).await;
        read_until(&mut client, is_remote_status).await;

        let reply = crate::server::test_support::dispatch_one(
            &hub,
            protocol::ClientMessage::RevokeDevice {
                device_id: device_id.clone(),
            },
        )
        .await;
        assert!(matches!(reply, DaemonMessage::Devices { .. }), "{reply:?}");
        assert!(
            closed_with_a_close_frame(&mut client).await,
            "the daemon sends Close before it ends the blocked device's connection"
        );
        assert_eq!(hub.live_devices.lock().connected(&device_id), 0);
    }

    /// The handshake deadline is far past the guard, so only the server's end
    /// can close the connection.
    #[tokio::test]
    async fn a_connection_still_in_its_handshake_ends_with_its_server() {
        let (mut hub, _scratch) = test_hub("handshake-server-ended");
        hub.tailnet_handshake_timeout = Duration::from_secs(600);
        let (mut client, server_life) = tailnet_ws_client(&hub, test_peer()).await;

        server_life.cancel();
        assert!(
            closed_with_a_close_frame(&mut client).await,
            "the daemon sends Close before it ends the connection"
        );
    }

    /// A refused handshake still delivers `AuthFailed`: the client's next
    /// frame is unread when the daemon refuses it, and a bare drop would
    /// reset the socket, which can destroy the refusal before the client
    /// reads it. The daemon ends the connection with the closing handshake
    /// instead.
    #[tokio::test]
    async fn failed_handshake_delivers_auth_failed_before_the_close() {
        use futures::SinkExt as _;
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        let (hub, _scratch) = test_hub("failed-handshake-close");
        let mut client = loopback_ws_client(&hub).await;
        // Both frames leave together, so the second is still unread at the
        // daemon when it refuses the handshake.
        for frame in [
            hello("not-the-daemons-token", None),
            protocol::ClientMessage::ListRepos,
        ] {
            let json = serde_json::to_string(&frame).expect("serialize a client message");
            client
                .feed(WsMessage::text(json))
                .await
                .expect("queue a frame");
        }
        client.flush().await.expect("send the frames");

        let first = ws_next(&mut client).await;
        assert!(
            matches!(first, Some(DaemonMessage::AuthFailed { .. })),
            "the first message is the refusal: {first:?}"
        );
        assert!(
            closed_with_a_close_frame(&mut client).await,
            "the daemon closes a failed handshake, so the refusal is not lost to a reset"
        );
    }

    /// No in-test socket can be made to block the pump's `send` reliably (the
    /// loopback buffers absorb what a test can queue), so this proves the
    /// bound on the connection's end with a send pump that never hands its
    /// sink back, as one stuck on a peer that stopped reading would.
    #[tokio::test(start_paused = true)]
    async fn a_peer_that_stops_reading_is_dropped_within_the_close_limit() {
        let life = CancellationToken::new();
        life.cancel();
        let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
        let stuck = tokio::spawn(async move {
            let _held = held_tx;
            std::future::pending::<()>().await;
        });
        let began = tokio::time::Instant::now();
        let ended = tokio::time::timeout(
            Duration::from_secs(120),
            end_within(stuck, Duration::from_secs(600), &life, async |()| {}),
        )
        .await;
        assert!(ended.is_ok(), "the end waited on the stuck pump");
        assert!(
            began.elapsed() <= SERVER_ENDED_CLOSE_LIMIT,
            "ended within the close limit: {:?}",
            began.elapsed()
        );
        let aborted = tokio::time::timeout(Duration::from_secs(120), held_rx).await;
        assert!(
            matches!(aborted, Ok(Err(_))),
            "the stuck pump was aborted, dropping what it held"
        );
    }

    #[tokio::test]
    async fn a_lagged_tab_forwarder_resyncs_with_the_tab_list() {
        let (hub, _scratch) = test_hub("lagged-tab-forwarder");
        let client_id = "lagged-client".to_string();
        let tab = hub
            .state
            .mutate_client_layout(&client_id, |tabs| {
                let tab = crate::tabs::make_tab(None, None, tabs);
                tabs.push(tab.clone());
                tab
            })
            .expect("seed the client's layout with one tab");

        let (out_tx, mut rx) = mpsc::unbounded_channel();
        let forwarder = spawn_tab_forwarder(
            hub.tab_events.clone(),
            Arc::clone(&hub.state),
            out_tx,
            client_id.clone(),
        );

        // The current-thread runtime first polls the forwarder at the await
        // below, by which time the broadcast buffer has overflowed. One more
        // event than production's capacity outruns the smaller channel the
        // test hub builds (test_support gives it room for 16).
        for _ in 0..=crate::server::TAB_EVENT_CAPACITY {
            hub.emit_tab(&client_id, TabEvent::Updated(tab.clone()));
        }
        let listed = match next_message(&mut rx).await {
            DaemonMessage::Tabs { tabs } => Some(tabs),
            _ => None,
        };
        assert_eq!(
            listed.expect("the lagged connection gets the tab list"),
            hub.state.client_layout(&client_id),
            "the list is this client's layout as it stands"
        );

        // The missed events are not replayed after the list; the events after
        // it arrive on the fresh subscription.
        hub.emit_tab(&client_id, TabEvent::Removed("after".to_string()));
        let removed = match next_message(&mut rx).await {
            DaemonMessage::TabRemoved { tab_id } => Some(tab_id),
            _ => None,
        };
        assert_eq!(
            removed.expect("events after the list still arrive"),
            "after".to_string(),
            "the forwarder keeps draining its new subscription"
        );
        forwarder.abort();
    }

    #[tokio::test]
    async fn a_lagged_state_stream_still_delivers_the_report() {
        let (hub, _scratch) = test_hub("lagged-state-forwarder");
        *crate::sync::lock(&hub.start_resume) = Some((2, 1));
        let (out_tx, mut rx) = mpsc::unbounded_channel();
        let forwarder = spawn_state_forwarder(
            &hub,
            hub.state_events.subscribe(),
            out_tx,
            "c1".to_owned(),
            true,
        );

        // The report is the oldest event, so a stream that overflows the
        // broadcast buffer skips exactly it. One more event than production's
        // capacity outruns the smaller channel the test hub builds
        // (test_support gives it room for 16).
        let _ = hub.state_events.send(StateEvent::ResumeReport {
            resumed: 2,
            left_in_recover: 1,
        });
        for _ in 0..=crate::server::STATE_EVENT_CAPACITY {
            let _ = hub.state_events.send(StateEvent::Repos(Vec::new()));
        }

        let first = next_message(&mut rx).await;
        assert!(
            matches!(
                first,
                DaemonMessage::ResumeReport {
                    resumed: 2,
                    left_in_recover: 1
                }
            ),
            "the lagged stream still hears the report, got {first:?}"
        );
        forwarder.abort();
        let mut reports = 1;
        while let Ok(message) = rx.try_recv() {
            if matches!(message, DaemonMessage::ResumeReport { .. }) {
                reports += 1;
            }
        }
        assert_eq!(reports, 1, "and hears it once");
    }

    #[tokio::test]
    async fn the_forwarder_tells_each_client_once() {
        let (hub, _scratch) = test_hub("resume-report-once");
        // c1 heard it at its initial state.
        assert!(hub.claim_resume_report("c1"));
        let (out_tx, mut rx) = mpsc::unbounded_channel();
        let told = spawn_state_forwarder(
            &hub,
            hub.state_events.subscribe(),
            out_tx,
            "c1".to_owned(),
            true,
        );

        let _ = hub.state_events.send(StateEvent::ResumeReport {
            resumed: 2,
            left_in_recover: 1,
        });
        let _ = hub.state_events.send(StateEvent::Repos(Vec::new()));
        let first = next_message(&mut rx).await;
        assert!(
            matches!(first, DaemonMessage::Repos { .. }),
            "a client the initial push told gets no second report, got {first:?}"
        );
        told.abort();

        let (out_tx, mut rx) = mpsc::unbounded_channel();
        let told = spawn_state_forwarder(
            &hub,
            hub.state_events.subscribe(),
            out_tx,
            "c2".to_owned(),
            true,
        );

        let _ = hub.state_events.send(StateEvent::ResumeReport {
            resumed: 2,
            left_in_recover: 1,
        });
        let _ = hub.state_events.send(StateEvent::Repos(Vec::new()));
        let first = next_message(&mut rx).await;
        assert!(
            matches!(
                first,
                DaemonMessage::ResumeReport {
                    resumed: 2,
                    left_in_recover: 1
                }
            ),
            "an untold client hears it, got {first:?}"
        );
        let second = next_message(&mut rx).await;
        assert!(matches!(second, DaemonMessage::Repos { .. }));
        told.abort();
    }
}
