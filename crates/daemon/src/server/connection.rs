//! One client WebSocket connection: its id and auth kind, the client count, the event forwarders, the send pump and the receive loop.
use super::handshake::{handshake, push_initial_state, send_resume_report};
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

pub(super) async fn client_session(hub: Hub, socket: WebSocket, auth: AuthKind) {
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

    let send_task = tokio::spawn(send_pump(sender, out_rx, file_rx));

    // Mandatory handshake.
    let handshake = match handshake(&hub, &auth, &mut receiver, &out_tx).await {
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
    // An unpaired tailnet device gets the pairing loop and nothing else: no
    // layout, no forwarder, no initial state.
    if handshake.pairing
        && let AuthKind::Tailnet(peer) = &auth
    {
        drop(client_guard);
        pairing_session(&hub, &mut receiver, &out_tx, peer).await;
        drop(file_tx);
        drop(out_tx);
        // The device may still be sending when its session ends; a proper
        // close keeps the last reply from being lost to a reset.
        if let Ok(sender) = send_task.await {
            close_gracefully(sender, &mut receiver, hub.tailnet_handshake_timeout).await;
        }
        return;
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
    recv_loop(&hub, &mut receiver, &ctx).await;

    info!("client_session: recv_loop returned; tearing down");
    {
        let mut forwarders = pty_forwarders.lock().await;
        for (_, handle) in forwarders.drain() {
            handle.abort();
        }
    }
    {
        let mut fetches = fetches.lock().await;
        for (_, token) in fetches.drain() {
            token.cancel();
        }
    }
    drop(file_tx);
    drop(out_tx);
    event_task.abort();
    tab_event_task.abort();
    preset_event_task.abort();
    state_event_task.abort();
    let _ = send_task.await;
    info!("client_session: returning");
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
/// `file_rx`. Ends when `out_rx` closes or the socket fails; `file_rx`
/// closing only retires its own arm. Hands the sink back, so a caller can
/// still close the socket properly.
async fn send_pump(
    mut sender: futures::stream::SplitSink<WebSocket, Message>,
    mut out_rx: mpsc::UnboundedReceiver<DaemonMessage>,
    mut file_rx: mpsc::Receiver<DaemonMessage>,
) -> futures::stream::SplitSink<WebSocket, Message> {
    let mut file_open = true;
    loop {
        let msg = tokio::select! {
            biased;
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
    sender
}

/// End a connection with the WebSocket closing handshake once everything
/// queued for it is on the wire: send `Close`, then read and discard what the
/// peer still sends until its `Close` or the end of the stream, for at most
/// `limit`. Dropping a socket that still holds unread frames resets it, and
/// the reset can destroy replies the peer has not read yet.
async fn close_gracefully(
    mut sender: futures::stream::SplitSink<WebSocket, Message>,
    receiver: &mut futures::stream::SplitStream<WebSocket>,
    limit: std::time::Duration,
) {
    if sender.send(Message::Close(None)).await.is_err() {
        return;
    }
    let drained = tokio::time::timeout(limit, async {
        while let Some(Ok(message)) = receiver.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    })
    .await;
    if drained.is_err() {
        debug!("the peer did not finish the closing handshake in time; dropping the socket");
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

/// Run the WebSocket receive loop until the peer closes or the daemon-wide
/// shutdown flag flips. Honoring `shutdown_tx` here is what lets axum's
/// graceful-shutdown future resolve after a [`ClientMessage::Shutdown`] —
/// otherwise every `recv()` blocks forever and the daemon hangs.
async fn recv_loop(
    hub: &Hub,
    receiver: &mut futures::stream::SplitStream<WebSocket>,
    ctx: &ConnCtx<'_>,
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
    use crate::server::test_support::{next_message, test_hub};

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
