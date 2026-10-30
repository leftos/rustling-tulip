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
use protocol::{DaemonMessage, InboundClientMessage, error_codes};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
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

/// How long one frame's send to a tailnet peer may take, plus [`SEND_BYTES_PER_SEC`]'s share
/// for its size, before the connection is judged dead: a peer that stopped
/// reading, or a link that went away without a reset.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// The slowest transfer rate a send is allowed: each frame gets one more
/// second per this many payload bytes, so a full `Scrollback` reply (about
/// 2.8 MB) gets about 73 s.
const SEND_BYTES_PER_SEC: u64 = 64 * 1024;

/// How often a tailnet connection is pinged. Loopback connections are never
/// pinged.
const PING_INTERVAL: Duration = Duration::from_secs(15);

/// Pings in a row a tailnet peer may leave unanswered; the next ping tick
/// closes the connection.
const MISSED_PONGS: u32 = 3;

/// A connection's socket: the axum `WebSocket`, or a test's in-memory one.
pub(super) trait Socket:
    futures::Stream<Item = Result<Message, axum::Error>> + futures::Sink<Message> + Send + 'static
{
}

impl<T> Socket for T where
    T: futures::Stream<Item = Result<Message, axum::Error>>
        + futures::Sink<Message>
        + Send
        + 'static
{
}

/// The half of a connection's socket the daemon reads.
pub(super) trait Inbound:
    futures::Stream<Item = Result<Message, axum::Error>> + Unpin + Send
{
}

impl<T> Inbound for T where T: futures::Stream<Item = Result<Message, axum::Error>> + Unpin + Send {}

/// The half of a connection's socket the daemon writes.
pub(super) trait Outbound: futures::Sink<Message> + Unpin + Send + 'static {}

impl<T> Outbound for T where T: futures::Sink<Message> + Unpin + Send + 'static {}

/// The signals a connection's send pump and its receive loop share.
#[derive(Clone)]
pub(super) struct ConnLife {
    /// The connection's own life: a child of its listener server's, and never
    /// cancelled on loopback.
    pub(super) life: CancellationToken,
    /// Cancelled by the send pump when it gives up on the peer: a send that
    /// timed out or failed, or [`MISSED_PONGS`] unanswered pings.
    pub(super) dead: CancellationToken,
    /// The `Pong` frames the peer has sent.
    pub(super) pongs: Arc<AtomicU64>,
    /// Set while the receive loop awaits a request's handler, when it reads
    /// no frame and so counts no `Pong`: a ping tick then counts no miss.
    pub(super) dispatching: Arc<AtomicBool>,
    /// The connection's PTY forwarders and the bytes each session has queued,
    /// which the send pump releases as it takes them.
    pub(super) lane: Arc<PtyLane>,
}

impl ConnLife {
    fn new(life: CancellationToken) -> Self {
        Self {
            life,
            dead: CancellationToken::new(),
            pongs: Arc::new(AtomicU64::new(0)),
            dispatching: Arc::new(AtomicBool::new(false)),
            lane: Arc::default(),
        }
    }
}

/// The checks a connection's send pump makes on its peer.
#[derive(Debug, Clone, Copy)]
pub(super) struct PeerChecks {
    /// How often the peer is pinged; `None` sends no pings.
    ping: Option<Duration>,
    /// Whether a send that outlasts [`send_limit`] gives up on the peer.
    timed_sends: bool,
}

impl PeerChecks {
    /// Tailnet connections are pinged every [`PING_INTERVAL`] and their sends
    /// are timed. Loopback connections are neither: the SSH bridge relays
    /// over loopback and writes each frame to a slow ssh link before it reads
    /// the next.
    fn for_auth(auth: &AuthKind) -> Self {
        let tailnet = matches!(auth, AuthKind::Tailnet(_));
        Self {
            ping: tailnet.then_some(PING_INTERVAL),
            timed_sends: tailnet,
        }
    }
}

/// The most base64 PTY output (`data_b64` bytes) one session may have queued
/// on a connection that its send pump has not taken yet. It sits above one
/// full scrollback ring's base64 (the 2 MB ring is about 2.7 MB of it), so a
/// trip, whose resync sends at most one ring, never sends more than the
/// backlog it stops.
pub(super) const PTY_SESSION_BUDGET: usize = 4 * 1024 * 1024;

/// A connection's PTY lane: its forwarders, at most one per session, and the
/// base64 PTY bytes each session has queued on the connection's `out_tx`
/// that the send pump has not taken yet.
#[derive(Default)]
pub(super) struct PtyLane {
    /// Each attached session's forwarder task.
    pub(super) forwarders: AsyncMutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// Queued bytes by session; a session with none has no entry. Keyed by
    /// session, never by forwarder, so a replaced forwarder's bytes stay
    /// counted.
    queued: std::sync::Mutex<HashMap<String, usize>>,
    /// Woken each time the send pump takes PTY output off the queue.
    taken: tokio::sync::Notify,
}

impl PtyLane {
    /// Charge `len` queued bytes to `session_id`, unless they would take it
    /// past [`PTY_SESSION_BUDGET`]: whether they were charged.
    pub(super) fn try_charge(&self, session_id: &str, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        let mut queued = crate::sync::lock(&self.queued);
        // No entry is the correct answer for a session with nothing queued.
        let now = queued.get(session_id).copied().unwrap_or(0);
        if now + len > PTY_SESSION_BUDGET {
            return false;
        }
        queued.insert(session_id.to_owned(), now + len);
        true
    }

    /// Release the bytes of `msg`, when it is PTY output the send pump just
    /// took off the queue, and wake whoever waits for its session to drain.
    fn release_taken(&self, msg: &DaemonMessage) {
        let DaemonMessage::PtyOutput {
            session_id,
            data_b64,
        } = msg
        else {
            return;
        };
        if data_b64.is_empty() {
            return;
        }
        {
            let mut queued = crate::sync::lock(&self.queued);
            let now = queued.get(session_id).copied().unwrap_or(0);
            debug_assert!(
                now >= data_b64.len(),
                "PTY output for {session_id} was released without being charged"
            );
            let left = now.saturating_sub(data_b64.len());
            if left == 0 {
                queued.remove(session_id);
            } else {
                queued.insert(session_id.clone(), left);
            }
        }
        self.taken.notify_waiters();
    }

    /// The bytes `session_id` has queued.
    pub(super) fn queued(&self, session_id: &str) -> usize {
        crate::sync::lock(&self.queued)
            .get(session_id)
            .copied()
            .unwrap_or(0)
    }

    /// Wait until the send pump has taken everything `session_id` queued.
    pub(super) async fn drained(&self, session_id: &str) {
        loop {
            // Enabled before the check, so a release between the check and
            // the wait still wakes it.
            let notified = self.taken.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.queued(session_id) == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// A loopback connection: [`client_session_until`] with no server life, so
/// nothing but the peer or the daemon's shutdown ends it.
pub(super) async fn client_session(hub: Hub, socket: WebSocket, auth: AuthKind) {
    client_session_until(hub, socket, auth, None).await;
}

/// The guards that end `connection`'s Dashboard watch and unload its chats
/// when dropped.
fn connection_watches(
    hub: &Hub,
    connection: u64,
) -> (
    crate::dashboard::WatchGuard,
    crate::chat_mirror::ChatLoadGuard,
) {
    (
        hub.dashboard.watch_guard(connection),
        hub.chat_mirror.guard(connection),
    )
}

/// Serve one connection until the peer closes it, the daemon shuts down,
/// `server_life` (the life of the tailnet listener server that accepted it,
/// `None` on loopback) is cancelled, or the send pump gives up on the peer. A
/// connection whose server ended is sent `Close` without what was still
/// queued for it, and tears down as a peer-closed one does.
pub(super) async fn client_session_until<S: Socket>(
    hub: Hub,
    socket: S,
    auth: AuthKind,
    server_life: Option<CancellationToken>,
) {
    let conn = ConnLife::new(
        server_life.map_or_else(CancellationToken::new, |server| server.child_token()),
    );
    let life = &conn.life;
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

    let checks = PeerChecks::for_auth(&auth);
    let send_task = tokio::spawn(send_pump(sender, out_rx, file_rx, conn.clone(), checks));

    // Mandatory handshake.
    let mut handshake = match handshake(&hub, &auth, life, &mut receiver, &out_tx).await {
        Ok(Some(v)) => v,
        Ok(None) => {
            info!("client_session: the listener server ended during the handshake; closing");
            end_served(send_task, &mut receiver, life).await;
            return;
        }
        Err(err) => {
            warn!(?err, "client handshake failed");
            let _ = out_tx.send(DaemonMessage::AuthFailed {
                reason: err.to_string(),
            });
            drop(client_guard);
            drop(out_tx);
            end_refused_handshake(send_task, life, &mut receiver).await;
            return;
        }
    };
    // An unpaired tailnet device gets the pairing loop and nothing else: no
    // layout, no forwarder, no initial state.
    if handshake.pairing
        && let AuthKind::Tailnet(peer) = &auth
    {
        drop(client_guard);
        pairing_session(&hub, &mut receiver, &out_tx, peer, &conn).await;
        drop(file_tx);
        drop(out_tx);
        // The device may still be sending when its session ends; a proper
        // close keeps the last reply from being lost to a reset. What was
        // already queued for it (a decision, a refusal) goes first, even
        // when its server ended.
        let limit = hub.tailnet_handshake_timeout;
        end_within(send_task, limit, life, async |(sender, queued)| {
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
    //   - Kept in `conn.lane`, which also counts the bytes each session has
    //     queued: a forwarder whose session goes over its budget resyncs the
    //     client with a fresh snapshot in its own task.
    // Active `FetchFile` transfers by client-assigned id; cancelled by
    // `CancelFetch` and on teardown below.
    let fetches: FetchRegistry = Arc::new(AsyncMutex::new(HashMap::new()));

    // Subscribe to global session events. After splitting PtyOutput out, this
    // channel only carries low-volume control events.
    let connection = NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed);
    // End any Dashboard watch and chat load of this connection on every way
    // out, panic included; dropped before `out_tx`, since each holds a clone.
    let watches = connection_watches(&hub, connection);
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
    let preset_event_task = spawn_preset_forwarder(&hub, out_tx.clone());
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
        pty_lane: &conn.lane,
        fetches: &fetches,
        client_id: &client_id,
        client_name: client_name.as_deref(),
        connection,
        auth: &auth,
    };
    recv_loop(&hub, &mut receiver, &ctx, &conn, live.as_ref()).await;

    info!("client_session: recv_loop returned; tearing down");
    stop_transfers(&conn.lane, &fetches).await;
    drop(watches);
    drop(file_tx);
    drop(out_tx);
    event_task.abort();
    tab_event_task.abort();
    preset_event_task.abort();
    state_event_task.abort();
    end_served(send_task, &mut receiver, life).await;
    info!("client_session: returning");
}

/// What the send pump hands back when it ends: the sink and the queue.
type PumpEnd<S> = (S, mpsc::UnboundedReceiver<DaemonMessage>);

/// End a served (or still handshaking) connection: once its server ended,
/// with `Close` within [`SERVER_ENDED_CLOSE_LIMIT`] and whatever is still
/// queued dropped; otherwise once the pump has drained, dropping the socket.
async fn end_served<S: Outbound, R: Inbound>(
    send_task: tokio::task::JoinHandle<PumpEnd<S>>,
    receiver: &mut R,
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
pub(super) async fn stop_transfers(pty_lane: &PtyLane, fetches: &FetchRegistry) {
    for (_, handle) in pty_lane.forwarders.lock().await.drain() {
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

/// End a connection whose handshake was refused: send the refusal still
/// queued for it, then close it with the closing handshake, within
/// [`REFUSED_CLOSE_LIMIT`]. The client's next frame may still be unread here,
/// and a bare drop would reset the socket, which can destroy the refusal
/// before the client reads it.
async fn end_refused_handshake<S: Outbound, R: Inbound>(
    send_task: tokio::task::JoinHandle<PumpEnd<S>>,
    life: &CancellationToken,
    receiver: &mut R,
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

/// `msg` as one text frame, or `None` (logged) when it does not serialize.
fn encode(msg: &DaemonMessage) -> Option<Message> {
    match serde_json::to_string(msg) {
        Ok(json) => Some(Message::Text(json.into())),
        Err(err) => {
            error!(?err, "serializing daemon msg");
            None
        }
    }
}

/// Send `msg` as one text frame; `false` once the socket has failed.
async fn send_message<S: Outbound>(sender: &mut S, msg: &DaemonMessage) -> bool {
    match encode(msg) {
        Some(frame) => sender.send(frame).await.is_ok(),
        None => true,
    }
}

/// Why the send pump gave up on its peer.
#[derive(Debug, Clone, Copy)]
enum GaveUp {
    SendFailed,
    SendTimedOut,
    MissedPongs,
}

/// How long a send of a frame with `len` payload bytes may take.
fn send_limit(len: usize) -> Duration {
    let len = u64::try_from(len).unwrap_or(u64::MAX);
    SEND_TIMEOUT + Duration::from_millis(len.saturating_mul(1000) / SEND_BYTES_PER_SEC)
}

/// Send one frame; when `timed`, within [`send_limit`] of its payload's size.
async fn send_within<S: Outbound>(
    sender: &mut S,
    frame: Message,
    timed: bool,
) -> Result<(), GaveUp> {
    if !timed {
        return sender.send(frame).await.map_err(|_| GaveUp::SendFailed);
    }
    let len = match &frame {
        Message::Text(text) => text.as_str().len(),
        Message::Binary(bytes) | Message::Ping(bytes) | Message::Pong(bytes) => bytes.len(),
        Message::Close(_) => 0,
    };
    match tokio::time::timeout(send_limit(len), sender.send(frame)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(GaveUp::SendFailed),
        Err(_elapsed) => Err(GaveUp::SendTimedOut),
    }
}

/// The missed-pong count of a pinged connection.
#[derive(Default)]
struct PingState {
    /// The pong count when the last ping went out; `None` before the first.
    pongs_at_ping: Option<u64>,
    /// Pings in a row that no pong followed.
    missed: u32,
}

impl PingState {
    /// On a ping tick, with `pongs` received so far: `false` once
    /// [`MISSED_PONGS`] pings in a row went unanswered, otherwise `true`, and
    /// the next ping is to go out. While `dispatching`, the receive loop reads
    /// no pong, so the tick counts no miss.
    fn on_tick(&mut self, pongs: u64, dispatching: bool) -> bool {
        if let Some(at_ping) = self.pongs_at_ping
            && !dispatching
        {
            if pongs == at_ping {
                self.missed += 1;
            } else {
                self.missed = 0;
            }
        }
        if self.missed >= MISSED_PONGS {
            return false;
        }
        self.pongs_at_ping = Some(pongs);
        true
    }
}

/// The ping ticker for a connection pinged every `period`: its first tick
/// is one period from now, and a tick held up by a slow send is delayed, not
/// made up in a burst.
fn ping_ticker(period: Option<Duration>) -> Option<tokio::time::Interval> {
    period.map(|period| {
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker
    })
}

/// The next tick of `ticker`; never, for a connection that is not pinged.
async fn next_tick(ticker: &mut Option<tokio::time::Interval>) {
    match ticker {
        Some(ticker) => {
            ticker.tick().await;
        }
        None => std::future::pending().await,
    }
}

/// Send what is still `queued` for a connection that is ending (a pairing
/// connection, or a failed handshake), then close it with the closing
/// handshake.
async fn send_queued_then_close<S: Outbound, R: Inbound>(
    mut sender: S,
    mut queued: mpsc::UnboundedReceiver<DaemonMessage>,
    receiver: &mut R,
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
                Ok(StateEvent::SummarizerStatus {
                    settings,
                    providers,
                    keys,
                    spend_today_usd,
                    cap_reached,
                }) => {
                    let _ = out_tx.send(DaemonMessage::SummarizerStatus {
                        settings,
                        providers,
                        keys,
                        spend_today_usd,
                        cap_reached,
                    });
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(lagged = n, "client state event stream lagged");
                    // The report is sent once and no later event repeats it,
                    // so a stream that lagged past it asks for it again here.
                    send_resume_report(&hub, &out_tx, &client_id);
                    // A lost summarizer status is not repeated until the
                    // next change, so the current one is sent again too.
                    hub.summarizer.send_status(&out_tx);
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Forward preset events to one connection. When its stream lags, the
/// connection is resent every stored job snapshot, in job start order, before
/// forwarding resumes, so a lost terminal `JobUpdated` still reaches it.
fn spawn_preset_forwarder(
    hub: &Hub,
    out_tx: mpsc::UnboundedSender<DaemonMessage>,
) -> tokio::task::JoinHandle<()> {
    let mut rx = hub.preset_events.subscribe();
    let jobs = Arc::clone(&hub.preset_job_snapshots);
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
                    let snapshots = crate::sync::lock(&jobs).snapshots();
                    warn!(
                        lagged = n,
                        resent = snapshots.len(),
                        "client preset event stream lagged"
                    );
                    for job in snapshots {
                        let _ = out_tx.send(DaemonMessage::PresetLaunchJobUpdated { job });
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

pub(super) type FetchRegistry = Arc<AsyncMutex<HashMap<String, CancellationToken>>>;

/// Per-connection handles that [`dispatch`] needs beyond the [`Hub`].
#[derive(Clone, Copy)]
pub(super) struct ConnCtx<'a> {
    pub(super) out_tx: &'a mpsc::UnboundedSender<DaemonMessage>,
    /// Bounded, lower-priority queue for `FetchFile` transfer messages.
    pub(super) file_tx: &'a mpsc::Sender<DaemonMessage>,
    pub(super) pty_lane: &'a Arc<PtyLane>,
    pub(super) fetches: &'a FetchRegistry,
    pub(super) client_id: &'a str,
    pub(super) client_name: Option<&'a str>,
    /// This connection's id, unique for the daemon's run: a client may hold
    /// several connections under one `client_id`.
    pub(super) connection: u64,
    /// How this connection authenticated, deciding which messages it may use.
    pub(super) auth: &'a AuthKind,
}

/// Drain a connection's outbound queues into its socket, making the peer
/// `checks` (pings, timed sends). Control and PTY traffic on `out_rx` always
/// goes before file-transfer messages on `file_rx`. Ends when `out_rx` closes
/// or `conn.life` is cancelled, and gives up on the peer, cancelling
/// `conn.dead`, when a send fails, a timed send outlasts [`send_limit`], or
/// [`MISSED_PONGS`] pings in a row go unanswered outside a request's
/// handling (`conn.dispatching`); `file_rx` closing only
/// retires its own arm. Hands the sink back, so a caller can still close the
/// socket properly, with `out_rx` and whatever is still queued on it, for the
/// caller to send or drop.
async fn send_pump<S: Outbound>(
    mut sender: S,
    mut out_rx: mpsc::UnboundedReceiver<DaemonMessage>,
    mut file_rx: mpsc::Receiver<DaemonMessage>,
    conn: ConnLife,
    checks: PeerChecks,
) -> PumpEnd<S> {
    let mut ticker = ping_ticker(checks.ping);
    let mut pings = PingState::default();
    let mut file_open = true;
    let gave_up = loop {
        let frame = tokio::select! {
            biased;
            () = conn.life.cancelled() => break None,
            () = next_tick(&mut ticker) => {
                let pongs = conn.pongs.load(Ordering::SeqCst);
                if !pings.on_tick(pongs, conn.dispatching.load(Ordering::SeqCst)) {
                    break Some(GaveUp::MissedPongs);
                }
                Some(Message::Ping(axum::body::Bytes::new()))
            }
            msg = out_rx.recv() => match msg {
                Some(msg) => {
                    conn.lane.release_taken(&msg);
                    encode(&msg)
                }
                None => break None,
            },
            msg = file_rx.recv(), if file_open => {
                let Some(msg) = msg else {
                    file_open = false;
                    continue;
                };
                encode(&msg)
            }
        };
        let Some(frame) = frame else {
            continue;
        };
        if let Err(why) = send_within(&mut sender, frame, checks.timed_sends).await {
            break Some(why);
        }
    };
    if let Some(why) = gave_up {
        warn!(
            ?why,
            "send pump: giving up on the peer; closing the connection"
        );
        conn.dead.cancel();
    }
    (sender, out_rx)
}

/// End a connection with the WebSocket closing handshake once everything
/// queued for it is on the wire: send `Close`, then read and discard what the
/// peer still sends until its `Close` or the end of the stream. The caller
/// bounds it ([`end_within`]). Dropping a socket that still holds unread
/// frames resets it, and the reset can destroy replies the peer has not read
/// yet.
async fn close_gracefully<S: Outbound, R: Inbound>(mut sender: S, receiver: &mut R) {
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
/// shutdown flag flips, the connection's `life` is cancelled, or the send
/// pump gave up on the peer (`dead`). Each `Pong` raises `conn.pongs`.
/// Honoring `shutdown_tx` here is what lets axum's graceful-shutdown future
/// resolve after a [`ClientMessage::Shutdown`] — otherwise every `recv()`
/// blocks forever and the daemon hangs.
async fn recv_loop<R: Inbound>(
    hub: &Hub,
    receiver: &mut R,
    ctx: &ConnCtx<'_>,
    conn: &ConnLife,
    live: Option<&LiveConnection>,
) {
    let life = &conn.life;
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
            () = conn.dead.cancelled() => {
                info!("recv_loop: the send pump gave up on the peer; closing");
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
                    Message::Pong(_) => {
                        conn.pongs.fetch_add(1, Ordering::SeqCst);
                        continue;
                    }
                    Message::Binary(_) | Message::Ping(_) => continue,
                };
                let parsed = match InboundClientMessage::from_json_str(&text) {
                    Ok(p) => p,
                    Err(err) => {
                        let _ = out_tx.send(DaemonMessage::Error {
                            message: format!("malformed message: {err}"),
                            request_id: None,
                            code: Some(error_codes::MALFORMED_MESSAGE.to_owned()),
                        });
                        continue;
                    }
                };
                let parsed = match parsed {
                    InboundClientMessage::Known(msg) => msg,
                    InboundClientMessage::Unknown { ref type_tag, .. } => {
                        warn!(
                            %type_tag,
                            "undecodable client message (forward-compat path): answered only when it carries a request_id"
                        );
                        if let Some(reply) = undecoded_reply(&parsed) {
                            let _ = out_tx.send(reply);
                        }
                        continue;
                    }
                };
                // A handler may run for a minute (a recovery, a worktree
                // add), and no pong is read meanwhile.
                conn.dispatching.store(true, Ordering::SeqCst);
                let handled = dispatch(hub, parsed, ctx).await;
                conn.dispatching.store(false, Ordering::SeqCst);
                if let Err(err) = handled {
                    let _ = out_tx.send(DaemonMessage::Error {
                        message: err.to_string(),
                        request_id: None,
                        code: Some(error_codes::INTERNAL.to_owned()),
                    });
                }
            }
        }
    }
}

/// The `Error` answering a frame that did not decode as a client message,
/// when it carries a top-level string `request_id`: `invalid_request` when
/// its `type` names a known message (a field was refused), else
/// `unsupported_message`. `None` for a frame without one, which gets no reply.
pub(super) fn undecoded_reply(frame: &InboundClientMessage) -> Option<DaemonMessage> {
    let InboundClientMessage::Unknown { type_tag, .. } = frame else {
        return None;
    };
    let request_id = frame.request_id()?.to_owned();
    let (code, message) = if frame.names_known_type() {
        (
            error_codes::INVALID_REQUEST,
            format!("invalid {type_tag} message: a field did not decode"),
        )
    } else {
        (
            error_codes::UNSUPPORTED_MESSAGE,
            format!("unsupported message type: {type_tag}"),
        )
    };
    Some(DaemonMessage::Error {
        message,
        request_id: Some(request_id),
        code: Some(code.to_owned()),
    })
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::server::test_support::{
        MemClient, WsClient, hello, loopback_ws_client, mem_session, next_message,
        tailnet_ws_client, test_hub, test_peer, ws_next, ws_send,
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

    /// Send `text` as one raw text frame.
    async fn send_raw(client: &mut WsClient, text: &str) {
        use futures::SinkExt as _;
        client
            .send(tokio_tungstenite::tungstenite::Message::text(
                text.to_owned(),
            ))
            .await
            .expect("send a frame");
    }

    /// On a served loopback connection, send `frame`, then a `ListEnvNames`
    /// the daemon answers with its id: every `Error` that came before that
    /// answer, which proves the connection still serves requests.
    async fn errors_answering(tag: &str, frame: &str) -> Vec<DaemonMessage> {
        let (hub, _scratch) = test_hub(tag);
        let mut client = loopback_ws_client(&hub).await;
        ws_send(&mut client, &hello(&hub.auth_token, None)).await;
        send_raw(&mut client, frame).await;
        ws_send(
            &mut client,
            &protocol::ClientMessage::ListEnvNames {
                request_id: Some("after".to_owned()),
            },
        )
        .await;
        let mut errors = Vec::new();
        loop {
            let message = ws_next(&mut client)
                .await
                .expect("a message before the connection closes");
            match message {
                DaemonMessage::EnvNames { request_id, .. }
                    if request_id.as_deref() == Some("after") =>
                {
                    return errors;
                }
                error @ DaemonMessage::Error { .. } => errors.push(error),
                _ => {}
            }
        }
    }

    fn assert_coded(errors: &[DaemonMessage], want_code: &str, want_id: &str) {
        assert!(
            matches!(
                errors,
                [DaemonMessage::Error { code: Some(code), request_id: Some(id), .. }]
                    if code == want_code && id == want_id
            ),
            "{errors:?}"
        );
    }

    #[tokio::test]
    async fn an_unknown_message_with_a_request_id_is_answered_unsupported() {
        let errors = errors_answering(
            "unknown-message-reply",
            r#"{"type":"future_message","request_id":"r1"}"#,
        )
        .await;
        assert_coded(&errors, protocol::error_codes::UNSUPPORTED_MESSAGE, "r1");
    }

    #[tokio::test]
    async fn a_known_message_with_a_bad_field_is_answered_invalid() {
        let errors = errors_answering(
            "bad-field-reply",
            r#"{"type":"stop_session","session_id":7,"request_id":"r2"}"#,
        )
        .await;
        assert_coded(&errors, protocol::error_codes::INVALID_REQUEST, "r2");
    }

    #[tokio::test]
    async fn an_unknown_message_without_a_request_id_gets_no_reply() {
        let errors =
            errors_answering("unknown-message-silent", r#"{"type":"future_message"}"#).await;
        assert!(errors.is_empty(), "{errors:?}");
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

    /// A hang guard far past any passing run, in virtual time.
    const GUARD: Duration = Duration::from_secs(600);

    /// A sink that never takes a frame, as one whose peer stopped reading.
    struct NeverReady;

    impl futures::Sink<Message> for NeverReady {
        type Error = axum::Error;

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Pending
        }

        fn start_send(self: std::pin::Pin<&mut Self>, _frame: Message) -> Result<(), Self::Error> {
            Ok(())
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Pending
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Pending
        }
    }

    /// A sink that takes every frame and answers each `Ping` at once by
    /// raising `pongs`, as a live peer does, counting the pings in `pings`.
    struct AnsweringPeer {
        pongs: Arc<AtomicU64>,
        pings: Arc<AtomicU64>,
    }

    impl futures::Sink<Message> for AnsweringPeer {
        type Error = axum::Error;

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn start_send(self: std::pin::Pin<&mut Self>, frame: Message) -> Result<(), Self::Error> {
            if matches!(frame, Message::Ping(_)) {
                self.pings.fetch_add(1, Ordering::SeqCst);
                self.pongs.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    fn pty_output(data: &str) -> DaemonMessage {
        DaemonMessage::PtyOutput {
            session_id: "s1".to_owned(),
            data_b64: data.to_owned(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn send_pump_ends_when_a_send_never_completes() {
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (_file_tx, file_rx) = mpsc::channel(4);
        let conn = ConnLife::new(CancellationToken::new());
        assert!(conn.lane.try_charge("s1", 4));
        out_tx
            .send(pty_output("AAAA"))
            .expect("queue a frame for the pump");
        let checks = PeerChecks {
            ping: None,
            timed_sends: true,
        };
        let pump = tokio::spawn(send_pump(NeverReady, out_rx, file_rx, conn.clone(), checks));
        let ended = tokio::time::timeout(GUARD, pump).await;
        assert!(ended.is_ok(), "the pump still waits on the stalled send");
        assert!(
            conn.dead.is_cancelled(),
            "a pump that gave up on its send marks the connection dead"
        );
        drop(out_tx);
    }

    #[tokio::test(start_paused = true)]
    async fn a_busy_pump_does_not_burst_pings() {
        let conn = ConnLife::new(CancellationToken::new());
        let pings = Arc::new(AtomicU64::new(0));
        let peer = AnsweringPeer {
            pongs: Arc::clone(&conn.pongs),
            pings: Arc::clone(&pings),
        };
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (_file_tx, file_rx) = mpsc::channel(4);
        let pump = tokio::spawn(send_pump(
            peer,
            out_rx,
            file_rx,
            conn.clone(),
            PeerChecks::for_auth(&AuthKind::Tailnet(test_peer())),
        ));
        let flowing = tokio::time::Instant::now();
        while flowing.elapsed() < Duration::from_secs(60) {
            assert!(conn.lane.try_charge("s1", 4), "the pump keeps up");
            out_tx
                .send(pty_output("AAAA"))
                .expect("the pump is running");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(out_tx);
        tokio::time::timeout(GUARD, pump)
            .await
            .expect("the pump ends once its queue closes")
            .expect("the pump did not panic");
        let sent = pings.load(Ordering::SeqCst);
        assert!(
            (1..=5).contains(&sent),
            "one ping per interval over 60 s, not a burst: {sent}"
        );
        assert!(
            !conn.dead.is_cancelled(),
            "a peer that answered every ping stays open"
        );
    }

    /// A sink that takes every frame and never answers a `Ping`, as a peer
    /// whose pongs go unread.
    struct SilentPeer;

    impl futures::Sink<Message> for SilentPeer {
        type Error = axum::Error;

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn start_send(self: std::pin::Pin<&mut Self>, _frame: Message) -> Result<(), Self::Error> {
            Ok(())
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_pinged_connection_counts_no_miss_while_a_request_runs() {
        let conn = ConnLife::new(CancellationToken::new());
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (_file_tx, file_rx) = mpsc::channel(4);
        conn.dispatching.store(true, Ordering::SeqCst);
        let mut pump = tokio::spawn(send_pump(
            SilentPeer,
            out_rx,
            file_rx,
            conn.clone(),
            PeerChecks::for_auth(&AuthKind::Tailnet(test_peer())),
        ));

        tokio::time::sleep(Duration::from_secs(120)).await;
        assert!(
            !pump.is_finished() && !conn.dead.is_cancelled(),
            "the pump gave up on a peer whose pongs go unread while a request runs"
        );

        conn.dispatching.store(false, Ordering::SeqCst);
        let ended = tokio::time::timeout(Duration::from_secs(60), &mut pump).await;
        assert!(
            ended.is_ok(),
            "the pump keeps a peer that answers no ping once the request ended"
        );
        // The sink takes every frame, so only missed pongs can end the pump.
        assert!(
            conn.dead.is_cancelled(),
            "the pump gave up on the peer for its missed pongs"
        );
        drop(out_tx);
    }

    #[tokio::test(start_paused = true)]
    async fn a_loopback_connection_is_never_closed_for_a_slow_send() {
        let (hub, _scratch) = test_hub("loopback-slow-send");
        let (mut client, session) = mem_session(&hub, AuthKind::Local);
        client.send(&hello(&hub.auth_token, None));
        let welcome = tokio::time::timeout(GUARD, client.next(true))
            .await
            .expect("the daemon answers the Hello");
        assert!(
            matches!(welcome, Some(DaemonMessage::Welcome { .. })),
            "the connection is served: {welcome:?}"
        );

        client.stall();
        client.send(&protocol::ClientMessage::ListRepos);
        tokio::time::sleep(Duration::from_secs(600)).await;
        assert!(
            !session.is_finished(),
            "a loopback connection was closed for a slow send"
        );
        assert_eq!(*hub.client_count.borrow(), 1, "the client is still counted");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_send_closes_the_whole_connection() {
        let (hub, _scratch) = test_hub("stalled-send");
        let (mut client, session) = served_tailnet_session(&hub);
        let welcome = tokio::time::timeout(GUARD, client.next(true))
            .await
            .expect("the daemon answers the Hello");
        assert!(
            matches!(welcome, Some(DaemonMessage::Welcome { .. })),
            "the connection is served: {welcome:?}"
        );
        assert_eq!(*hub.client_count.borrow(), 1, "the client is counted");

        client.stall();
        client.send(&protocol::ClientMessage::ListRepos);
        let ended = tokio::time::timeout(GUARD, session).await;
        assert!(
            ended.is_ok(),
            "the connection stayed open after its send stalled"
        );
        assert_eq!(
            *hub.client_count.borrow(),
            0,
            "the client count is released"
        );
    }

    /// A served tailnet connection for an approved device.
    fn served_tailnet_session(hub: &Hub) -> (MemClient, tokio::task::JoinHandle<()>) {
        let (_, token) = crate::sync::lock(&hub.devices)
            .issue(&test_peer(), chrono::Utc::now())
            .expect("issue a device token");
        let (client, session) = mem_session(hub, AuthKind::Tailnet(test_peer()));
        client.send(&hello(&token, None));
        (client, session)
    }

    /// Read `client` for `span` of virtual time, answering pings when
    /// `answer_pings`: whether the daemon kept the connection open throughout.
    async fn stays_open(client: &mut MemClient, span: Duration, answer_pings: bool) -> bool {
        let read = async {
            while let Some(message) = client.next(answer_pings).await {
                let refused = matches!(
                    &message,
                    DaemonMessage::DevicePairFailed { reason } if reason != protocol::DEVICE_NOT_APPROVED
                );
                assert!(!refused, "the pairing connection was refused: {message:?}");
            }
        };
        tokio::time::timeout(span, read).await.is_err()
    }

    #[tokio::test(start_paused = true)]
    async fn a_tailnet_connection_is_closed_after_three_missed_pongs() {
        let (hub, _scratch) = test_hub("tailnet-missed-pongs");
        let (mut client, session) = served_tailnet_session(&hub);
        assert!(
            !stays_open(&mut client, GUARD, false).await,
            "a connection that never answers a ping is closed"
        );
        assert_eq!(
            client.pings,
            usize::try_from(MISSED_PONGS).expect("a small count"),
            "closed on the tick after the third unanswered ping"
        );
        tokio::time::timeout(GUARD, session)
            .await
            .expect("the session ends with its connection")
            .expect("the session did not panic");
        assert_eq!(
            *hub.client_count.borrow(),
            0,
            "the client count is released"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_tailnet_connection_that_answers_pings_stays_open() {
        let (hub, _scratch) = test_hub("tailnet-answered-pings");
        let (mut client, session) = served_tailnet_session(&hub);
        assert!(
            stays_open(&mut client, PING_INTERVAL * 6, true).await,
            "a connection that answers every ping stays open"
        );
        assert!(client.pings >= 4, "it was pinged: {}", client.pings);
        assert!(!session.is_finished());
    }

    #[tokio::test(start_paused = true)]
    async fn a_loopback_connection_is_never_pinged() {
        let (hub, _scratch) = test_hub("loopback-no-ping");
        let (mut client, session) = mem_session(&hub, AuthKind::Local);
        client.send(&hello(&hub.auth_token, None));
        assert!(
            stays_open(&mut client, PING_INTERVAL * 6, false).await,
            "a loopback connection that answers nothing stays open"
        );
        assert_eq!(client.pings, 0, "a loopback connection is never pinged");
        assert!(!session.is_finished());
    }

    #[tokio::test(start_paused = true)]
    async fn a_pairing_connection_that_answers_pings_stays_open() {
        let (hub, _scratch) = test_hub("pairing-answered-pings");
        hub.remote_enabled.send_replace(true);
        let (mut client, session) = mem_session(&hub, AuthKind::Tailnet(test_peer()));
        client.send(&hello("not-a-device-token", None));
        client.send(&protocol::ClientMessage::DevicePairRequest);
        assert!(
            stays_open(&mut client, Duration::from_secs(75), true).await,
            "a pending pairing connection that answers every ping stays open"
        );
        assert!(client.pings >= 4, "it was pinged: {}", client.pings);
        assert!(!session.is_finished());
    }

    #[tokio::test]
    async fn a_lagged_tab_forwarder_resyncs_with_the_tab_list() {
        let (hub, _scratch) = test_hub("lagged-tab-forwarder");
        let client_id = "lagged-client".to_string();
        let tab = hub
            .state
            .mutate_client_layout(&client_id, |tabs| {
                let tab = crate::tabs::make_tab(None, None, None, tabs);
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

    fn job_snapshot(
        job_id: &str,
        status: protocol::PresetLaunchJobStatus,
    ) -> protocol::PresetLaunchJobSnapshot {
        protocol::PresetLaunchJobSnapshot {
            job_id: job_id.to_string(),
            preset_id: "preset-1".to_string(),
            target: protocol::PresetTarget::Repo {
                repo_id: "repo-1".to_string(),
            },
            total: 1,
            launched: 0,
            created_session_ids: Vec::new(),
            created_tab_ids: Vec::new(),
            status,
            error: None,
            current_tab_id: None,
        }
    }

    #[tokio::test]
    async fn a_lagged_preset_forwarder_resends_the_job_snapshots() {
        let (hub, _scratch) = test_hub("lagged-preset-forwarder");
        let (out_tx, mut rx) = mpsc::unbounded_channel();
        let forwarder = spawn_preset_forwarder(&hub, out_tx);

        let running = job_snapshot("job-running", protocol::PresetLaunchJobStatus::Spawning);
        let done = job_snapshot("job-done", protocol::PresetLaunchJobStatus::Completed);
        crate::presets::send_job_update(&hub, running.clone());
        crate::presets::send_job_update(&hub, done.clone());
        // The current-thread runtime first polls the forwarder at the await
        // below, by which time these progress events have pushed both job
        // snapshots out of the test hub's 16-slot channel.
        for _ in 0..=crate::server::PRESET_EVENT_CAPACITY {
            let _ = hub.preset_events.send(PresetEvent::Progress {
                job_id: "job-running".to_string(),
                preset_id: "preset-1".to_string(),
                total: 1,
                launched: 0,
                current_tab_id: None,
                tab_ids: Vec::new(),
            });
        }

        let mut resent = Vec::new();
        for _ in 0..2 {
            let job = match next_message(&mut rx).await {
                DaemonMessage::PresetLaunchJobUpdated { job } => Some(job),
                _ => None,
            };
            resent.push(job.expect("the lagged connection gets a job snapshot"));
        }
        assert_eq!(
            resent,
            vec![running, done],
            "every stored snapshot is resent, in job start order"
        );
        assert!(
            matches!(
                next_message(&mut rx).await,
                DaemonMessage::PresetLaunchProgress { .. }
            ),
            "the forwarder keeps forwarding after the resend"
        );
        forwarder.abort();
    }

    #[tokio::test]
    async fn a_lagged_state_stream_resends_the_summarizer_status() {
        let (hub, _scratch) = test_hub("lagged-summarizer-status");
        let (out_tx, mut rx) = mpsc::unbounded_channel();
        let forwarder = spawn_state_forwarder(
            &hub,
            hub.state_events.subscribe(),
            out_tx,
            "c1".to_owned(),
            true,
        );

        for _ in 0..=crate::server::STATE_EVENT_CAPACITY {
            let _ = hub.state_events.send(StateEvent::Repos(Vec::new()));
        }

        let first = next_message(&mut rx).await;
        forwarder.abort();
        assert!(
            matches!(first, DaemonMessage::SummarizerStatus { .. }),
            "the lagged stream hears the current summarizer status first, got {first:?}"
        );
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
