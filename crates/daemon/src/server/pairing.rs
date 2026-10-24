//! Device pairing: the session an unpaired tailnet device gets, and the
//! owner's loopback-only handlers that approve, deny and list devices.
//!
//! A tailnet `Hello` without a valid device token lands in
//! [`pairing_session`], which accepts only the device's own request, cancel
//! and redeem, and sends no state. The owner's local client answers through
//! [`handle`]; a decision reaches the waiting device through its entry in
//! [`Hub::pairing_waiters`]. When both locks are held, [`Hub::pairing`] is
//! taken first.
use super::connection::{AuthKind, ConnCtx};
use super::{Hub, StateEvent};
use crate::devices::{
    CODE_LIFETIME, PairError, PairingBook, PendingView, REQUEST_LIFETIME, RequestId,
};
use axum::extract::ws::{Message, WebSocket};
use chrono::Utc;
use futures::StreamExt as _;
use protocol::{
    ClientMessage, DEVICE_NOT_APPROVED, DaemonMessage, DevicePairOutcome, InboundClientMessage,
    Secret,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Each waiting request's connection, by request id.
pub type PairingWaiters =
    Arc<std::sync::Mutex<HashMap<RequestId, mpsc::UnboundedSender<DaemonMessage>>>>;

/// The answer to a pairing step on a served tailnet connection.
pub(super) const ALREADY_PAIRED: &str = "this device is already paired";

/// The answer to a pairing step on a loopback connection.
pub(super) const TAILNET_ONLY: &str = "device pairing is for tailnet devices";

/// The answer to an approval message on any connection but loopback.
pub(super) const LOCAL_ONLY: &str = "device approval is local-only";

/// What the device hears when its token could not be stored.
const SAVE_FAILED: &str = "could not save the device; ask again";

/// What a pairing connection hears, before it closes, while remote access is
/// off.
const REMOTE_OFF: &str = "remote access is off";

/// What the request after [`MAX_REQUESTS`] on one connection hears.
const TOO_MANY_REQUESTS: &str = "too many requests from this device; reconnect to ask again";

/// What a redeem hears once the code the owner approved has lapsed; the
/// request keeps waiting for a fresh approval.
const CODE_LAPSED: &str = "the code has expired; ask the owner to approve again";

/// Frames other than the three pairing steps a connection may send before
/// it is closed.
const MAX_STRAY_FRAMES: u32 = 16;

/// Requests one connection may make; the next one closes it.
const MAX_REQUESTS: u32 = 3;

/// How often a pairing connection checks for lapsed requests and codes.
const TICK: Duration = Duration::from_secs(1);

/// Why a redeem of the connection's own request failed.
enum Refusal {
    Pair(PairError),
    /// The approved code lapsed; the request still waits.
    CodeLapsed,
}

/// Whether the pairing loop goes on after an event.
enum Flow {
    Stay,
    Close,
}

/// One thing the pairing loop woke for.
enum Event {
    /// No request came before the deadline.
    Idle,
    Frame(Option<Result<Message, axum::Error>>),
    /// A decision about this connection's request.
    Waiter(Box<DaemonMessage>),
    Tick,
    /// The shutdown watch moved; `true` when its sender is gone.
    Shutdown(bool),
}

/// The state of one pairing connection.
struct PairingConn<'a> {
    hub: &'a Hub,
    peer: &'a tailnet::PeerIdentity,
    out_tx: &'a mpsc::UnboundedSender<DaemonMessage>,
    /// Registered in [`Hub::pairing_waiters`] under this connection's request.
    waiter_tx: mpsc::UnboundedSender<DaemonMessage>,
    /// This connection's request, once it asked.
    own: Option<RequestId>,
    /// Whether this connection forwarded `Approved` for its current request
    /// and has not since learned that code lapsed.
    saw_approved: bool,
    /// Frames that were not one of the three pairing steps.
    stray_frames: u32,
    /// `DevicePairRequest`s this connection sent.
    requests: u32,
}

/// Serve an unpaired tailnet device: tell it it must pair, then accept only
/// [`ClientMessage::DevicePairRequest`], [`ClientMessage::DevicePairCancel`]
/// and [`ClientMessage::DevicePairRedeem`] until the request ends.
///
/// Without a request the connection closes [`Hub::tailnet_handshake_timeout`]
/// after its `Welcome`; with one it lives while the request waits. Whatever
/// ends it, its own request goes from the book.
pub(super) async fn pairing_session(
    hub: &Hub,
    receiver: &mut futures::stream::SplitStream<WebSocket>,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    peer: &tailnet::PeerIdentity,
) {
    info!(device = %peer.name, login = %peer.login, "tailnet device without a valid token; pairing only");
    let _ = out_tx.send(DaemonMessage::DevicePairFailed {
        reason: DEVICE_NOT_APPROVED.to_owned(),
    });
    let (waiter_tx, mut waiter_rx) = mpsc::unbounded_channel();
    let mut conn = PairingConn {
        hub,
        peer,
        out_tx,
        waiter_tx,
        own: None,
        saw_approved: false,
        stray_frames: 0,
        requests: 0,
    };
    let idle = tokio::time::sleep(hub.tailnet_handshake_timeout);
    tokio::pin!(idle);
    let mut tick = tokio::time::interval(TICK);
    let mut shutdown_rx = hub.shutdown_tx.subscribe();
    loop {
        let event = tokio::select! {
            () = &mut idle, if conn.own.is_none() => Event::Idle,
            frame = receiver.next() => Event::Frame(frame),
            Some(msg) = waiter_rx.recv() => Event::Waiter(Box::new(msg)),
            _ = tick.tick() => Event::Tick,
            res = shutdown_rx.changed() => Event::Shutdown(res.is_err()),
        };
        let flow = match event {
            Event::Idle => {
                info!(device = %peer.name, "pairing connection closed: no request before the deadline");
                Flow::Close
            }
            Event::Frame(frame) => conn.on_frame(frame),
            Event::Waiter(msg) => conn.on_waiter(*msg),
            Event::Tick => conn.on_tick(&mut waiter_rx),
            Event::Shutdown(gone) => {
                if gone || *shutdown_rx.borrow_and_update() {
                    Flow::Close
                } else {
                    Flow::Stay
                }
            }
        };
        if matches!(flow, Flow::Close) {
            break;
        }
    }
    conn.teardown();
}

impl PairingConn<'_> {
    fn fail(&self, reason: impl Into<String>) {
        let _ = self.out_tx.send(DaemonMessage::DevicePairFailed {
            reason: reason.into(),
        });
    }

    fn on_frame(&mut self, frame: Option<Result<Message, axum::Error>>) -> Flow {
        let Some(Ok(message)) = frame else {
            return Flow::Close;
        };
        let text = match message {
            Message::Text(text) => text,
            Message::Close(_) => return Flow::Close,
            Message::Ping(_) | Message::Pong(_) => return Flow::Stay,
            Message::Binary(_) => return self.stray(),
        };
        match InboundClientMessage::from_json_str(text.as_str()) {
            Ok(InboundClientMessage::Known(msg)) => self.on_message(msg),
            Ok(InboundClientMessage::Unknown { type_tag, .. }) => {
                debug!(%type_tag, "pairing connection: ignoring an unknown client message type");
                self.stray()
            }
            Err(err) => {
                if matches!(self.stray(), Flow::Close) {
                    return Flow::Close;
                }
                let _ = self.out_tx.send(DaemonMessage::Error {
                    message: format!("malformed message: {err}"),
                    request_id: None,
                });
                Flow::Stay
            }
        }
    }

    /// Count a frame that is not a pairing step; the [`MAX_STRAY_FRAMES`]th
    /// closes the connection.
    fn stray(&mut self) -> Flow {
        self.stray_frames += 1;
        if self.stray_frames >= MAX_STRAY_FRAMES {
            warn!(device = %self.peer.name, "pairing connection closed: too many frames that are not pairing steps");
            return Flow::Close;
        }
        Flow::Stay
    }

    fn on_message(&mut self, msg: ClientMessage) -> Flow {
        match msg {
            ClientMessage::DevicePairRequest => self.on_request(),
            ClientMessage::DevicePairCancel => {
                info!(device = %self.peer.name, "pairing request cancelled by the device");
                Flow::Close
            }
            ClientMessage::DevicePairRedeem { request_id, code } => {
                self.on_redeem(&request_id, &code)
            }
            _ => {
                if matches!(self.stray(), Flow::Close) {
                    return Flow::Close;
                }
                self.fail(DEVICE_NOT_APPROVED);
                Flow::Stay
            }
        }
    }

    fn remote_off(&self) -> bool {
        !*self.hub.remote_enabled.borrow()
    }

    fn on_request(&mut self) -> Flow {
        if self.remote_off() {
            self.fail(REMOTE_OFF);
            return Flow::Close;
        }
        self.requests += 1;
        if self.requests > MAX_REQUESTS {
            info!(device = %self.peer.name, "pairing connection closed: too many requests");
            self.fail(TOO_MANY_REQUESTS);
            return Flow::Close;
        }
        let mut book = crate::sync::lock(&self.hub.pairing);
        let requested = match book.request(self.peer, Instant::now(), Utc::now()) {
            Ok(requested) => requested,
            Err(err) => {
                drop(book);
                self.fail(err.to_string());
                return Flow::Stay;
            }
        };
        let previous = self.own.take();
        let mut waiters = crate::sync::lock(&self.hub.pairing_waiters);
        // The previous request may have left the book without its waiter
        // entry going with it.
        if let Some(previous) = &previous {
            waiters.remove(previous);
        }
        if let Some(old) = requested.superseded {
            let tx = waiters.remove(&old);
            // This connection's own earlier request is simply replaced.
            if previous.as_deref() != Some(old.as_str())
                && let Some(tx) = tx
            {
                let _ = tx.send(decided(&old, DevicePairOutcome::Superseded));
            }
        }
        waiters.insert(requested.id.clone(), self.waiter_tx.clone());
        drop(waiters);
        info!(device = %self.peer.name, request_id = %requested.id, "device asked to pair");
        let _ = self.out_tx.send(DaemonMessage::DevicePairPending {
            request_id: requested.id.clone(),
            expires_in_secs: REQUEST_LIFETIME.as_secs(),
        });
        self.own = Some(requested.id);
        self.saw_approved = false;
        broadcast_pending(self.hub, &book);
        Flow::Stay
    }

    fn on_waiter(&mut self, msg: DaemonMessage) -> Flow {
        let (ends, approved) = match &msg {
            DaemonMessage::DevicePairDecided {
                request_id,
                outcome,
            } => (
                *outcome != DevicePairOutcome::Approved,
                *outcome == DevicePairOutcome::Approved
                    && self.own.as_deref() == Some(request_id.as_str()),
            ),
            _ => (false, false),
        };
        if approved {
            self.saw_approved = true;
        }
        let _ = self.out_tx.send(msg);
        if ends { Flow::Close } else { Flow::Stay }
    }

    /// Close the connection while remote access is off; otherwise drop
    /// lapsed requests and codes, tell their devices, and close this
    /// connection when its own request is no longer in the book.
    fn on_tick(&mut self, waiter_rx: &mut mpsc::UnboundedReceiver<DaemonMessage>) -> Flow {
        if self.remote_off() {
            // Turning remote access off tells each waiting device first.
            if let Ok(msg) = waiter_rx.try_recv() {
                return self.on_waiter(msg);
            }
            self.fail(REMOTE_OFF);
            return Flow::Close;
        }
        let gone = {
            let mut book = crate::sync::lock(&self.hub.pairing);
            let before = book.pending();
            let lapsed = book.expire(Instant::now());
            if !lapsed.is_empty() {
                let mut waiters = crate::sync::lock(&self.hub.pairing_waiters);
                for id in &lapsed {
                    if let Some(tx) = waiters.remove(id) {
                        let _ = tx.send(decided(id, DevicePairOutcome::Expired));
                    }
                }
            }
            let after = book.pending();
            let gone = self
                .own
                .as_ref()
                .is_some_and(|own| !after.iter().any(|view| &view.id == own));
            if !lapsed.is_empty() || after != before {
                broadcast_views(self.hub, after);
            }
            gone
        };
        let Some(own) = self.own.as_deref().filter(|_| gone) else {
            return Flow::Stay;
        };
        // A decision sent before the request left the book is already
        // queued; it says why better than a guess.
        if let Ok(msg) = waiter_rx.try_recv() {
            return self.on_waiter(msg);
        }
        let _ = self.out_tx.send(decided(own, DevicePairOutcome::Expired));
        Flow::Close
    }

    fn on_redeem(&mut self, request_id: &str, code: &Secret) -> Flow {
        if self.own.as_deref() != Some(request_id) {
            // Another connection's request is never redeemed or charged an
            // attempt from here, even from the same device.
            let held = crate::sync::lock(&self.hub.pairing)
                .pending()
                .iter()
                .any(|view| view.id == request_id);
            let err = if held {
                PairError::NotYours
            } else {
                PairError::Unknown
            };
            self.fail(err.to_string());
            return Flow::Stay;
        }
        match self.redeem_own(request_id, code) {
            Ok(()) => self.issue_token(),
            Err(Refusal::CodeLapsed) => {
                self.fail(CODE_LAPSED);
                Flow::Stay
            }
            Err(Refusal::Pair(PairError::Expired)) => {
                let _ = self
                    .out_tx
                    .send(decided(request_id, DevicePairOutcome::Expired));
                Flow::Close
            }
            Err(Refusal::Pair(err @ PairError::TooManyAttempts)) => {
                self.fail(err.to_string());
                Flow::Close
            }
            Err(Refusal::Pair(err)) => {
                self.fail(err.to_string());
                Flow::Stay
            }
        }
    }

    /// Redeem `code` for this connection's own request. A request no longer
    /// in the book has expired, whatever code comes with it. An approved one
    /// takes its own code; one whose approved code lapsed says so; one this
    /// connection never saw approved takes a CLI code, which then consumes
    /// it. Broadcasts the list when the book changed.
    fn redeem_own(&mut self, request_id: &str, code: &Secret) -> Result<(), Refusal> {
        let now = Instant::now();
        let mut book = crate::sync::lock(&self.hub.pairing);
        let before = book.pending();
        let result = match before.iter().find(|view| view.id == request_id) {
            None => Err(Refusal::Pair(PairError::Expired)),
            Some(view) if view.approved => {
                match book.redeem(request_id, &self.peer.stable_id, &code.0, now) {
                    // The request is still there, so only its code lapsed.
                    Err(PairError::Expired)
                        if book.pending().iter().any(|view| view.id == request_id) =>
                    {
                        Err(Refusal::CodeLapsed)
                    }
                    other => other.map_err(Refusal::Pair),
                }
            }
            Some(_) if self.saw_approved => Err(Refusal::CodeLapsed),
            Some(_) => redeem_cli(&mut book, request_id, code, now).map_err(Refusal::Pair),
        };
        if matches!(result, Err(Refusal::CodeLapsed)) {
            self.saw_approved = false;
        }
        let after = book.pending();
        if after != before {
            broadcast_views(self.hub, after);
        }
        result
    }

    fn issue_token(&self) -> Flow {
        let issued = crate::sync::lock(&self.hub.devices).issue(self.peer, Utc::now());
        match issued {
            Ok((device_id, token)) => {
                info!(device = %self.peer.name, %device_id, "device paired; token issued");
                let _ = self.out_tx.send(DaemonMessage::DeviceToken {
                    device_id,
                    token: Secret(token),
                });
            }
            Err(err) => {
                warn!(?err, device = %self.peer.name, "could not save the paired device");
                self.fail(SAVE_FAILED);
            }
        }
        Flow::Close
    }

    /// Remove this connection's own request and waiter, by id only: a newer
    /// connection from the same device may own the device's current request.
    fn teardown(&self) {
        let Some(own) = &self.own else {
            return;
        };
        let mut book = crate::sync::lock(&self.hub.pairing);
        let removed = book.deny(own);
        crate::sync::lock(&self.hub.pairing_waiters).remove(own);
        if removed {
            broadcast_pending(self.hub, &book);
        }
    }
}

/// Redeem a CLI code for a request the owner has not approved; success
/// consumes the request. With no CLI code live the code is simply wrong, so
/// a device that types too early keeps its request.
fn redeem_cli(
    book: &mut PairingBook,
    request_id: &str,
    code: &Secret,
    now: Instant,
) -> Result<(), PairError> {
    match book.redeem_cli_code(&code.0, now) {
        Ok(()) => {
            book.deny(request_id);
            Ok(())
        }
        Err(PairError::Expired) => Err(PairError::WrongCode),
        Err(err) => Err(err),
    }
}

/// The owner's device messages, and the pairing steps that reach a served
/// connection. Every approval message returns early unless `ctx` is a
/// loopback connection, beside [`super::remote::host_only_refusal`].
pub(super) fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) {
    let out_tx = ctx.out_tx;
    let local = matches!(ctx.auth, AuthKind::Local);
    match msg {
        ClientMessage::DevicePairRequest
        | ClientMessage::DevicePairCancel
        | ClientMessage::DevicePairRedeem { .. } => {
            refuse(out_tx, if local { TAILNET_ONLY } else { ALREADY_PAIRED });
        }
        _ if !local => {
            warn!("pairing: an approval message reached a tailnet connection; refused");
            refuse(out_tx, LOCAL_ONLY);
        }
        ClientMessage::ListPendingDevices => {
            let requests = pending_list(&crate::sync::lock(&hub.pairing));
            let _ = out_tx.send(DaemonMessage::PendingDevices { requests });
        }
        ClientMessage::ApproveDevice { request_id } => approve(hub, out_tx, &request_id),
        ClientMessage::DenyDevice { request_id } => deny(hub, &request_id),
        ClientMessage::MintDevicePairCode => {
            let code = crate::sync::lock(&hub.pairing).mint_cli_code(Instant::now());
            info!("pairing: a CLI code was minted");
            let _ = out_tx.send(DaemonMessage::DevicePairCode {
                request_id: None,
                code: Secret(code),
                expires_in_secs: CODE_LIFETIME.as_secs(),
            });
        }
        ClientMessage::ListDevices => send_devices(hub, out_tx),
        ClientMessage::RevokeDevice { device_id } => {
            let revoked = crate::sync::lock(&hub.devices).revoke(&device_id);
            match revoked {
                Ok(found) => {
                    info!(%device_id, found, "pairing: device revoked");
                    send_devices(hub, out_tx);
                }
                Err(err) => refuse(out_tx, err.to_string()),
            }
        }
        _ => debug!("pairing: dispatch routed a message this module does not handle"),
    }
}

/// Approve `request_id`: the code goes to the approving connection only, and
/// the waiting device hears it was approved.
fn approve(hub: &Hub, out_tx: &mpsc::UnboundedSender<DaemonMessage>, request_id: &str) {
    let mut book = crate::sync::lock(&hub.pairing);
    match book.approve(request_id, Instant::now()) {
        Ok(code) => {
            info!(%request_id, "pairing request approved");
            let _ = out_tx.send(DaemonMessage::DevicePairCode {
                request_id: Some(request_id.to_owned()),
                code: Secret(code),
                expires_in_secs: CODE_LIFETIME.as_secs(),
            });
            if let Some(waiter) = crate::sync::lock(&hub.pairing_waiters).get(request_id) {
                let _ = waiter.send(decided(request_id, DevicePairOutcome::Approved));
            }
            broadcast_pending(hub, &book);
        }
        Err(err) => {
            // A lapsed request was just dropped from the book.
            if err == PairError::Expired {
                broadcast_pending(hub, &book);
            }
            drop(book);
            refuse(out_tx, err.to_string());
        }
    }
}

/// Deny `request_id`; its device hears it and its connection closes.
fn deny(hub: &Hub, request_id: &str) {
    let mut book = crate::sync::lock(&hub.pairing);
    if book.deny(request_id) {
        info!(%request_id, "pairing request denied");
        if let Some(waiter) = crate::sync::lock(&hub.pairing_waiters).remove(request_id) {
            let _ = waiter.send(decided(request_id, DevicePairOutcome::Denied));
        }
    }
    broadcast_pending(hub, &book);
}

/// Empty the book and tell every waiting device its request expired, for
/// remote access turning off.
pub(super) fn drop_all(hub: &Hub) {
    let mut book = crate::sync::lock(&hub.pairing);
    *book = PairingBook::new();
    let waiters: Vec<_> = crate::sync::lock(&hub.pairing_waiters).drain().collect();
    for (id, waiter) in waiters {
        let _ = waiter.send(decided(&id, DevicePairOutcome::Expired));
    }
    broadcast_pending(hub, &book);
}

fn decided(request_id: &str, outcome: DevicePairOutcome) -> DaemonMessage {
    DaemonMessage::DevicePairDecided {
        request_id: request_id.to_owned(),
        outcome,
    }
}

fn refuse(out_tx: &mpsc::UnboundedSender<DaemonMessage>, message: impl Into<String>) {
    let _ = out_tx.send(DaemonMessage::Error {
        message: message.into(),
        request_id: None,
    });
}

fn to_wire(view: PendingView) -> protocol::PendingDevice {
    protocol::PendingDevice {
        request_id: view.id,
        name: view.name,
        login: view.login,
        asked_at: view.asked_at,
        approved: view.approved,
    }
}

fn pending_list(book: &PairingBook) -> Vec<protocol::PendingDevice> {
    book.pending().into_iter().map(to_wire).collect()
}

fn broadcast_views(hub: &Hub, views: Vec<PendingView>) {
    let requests = views.into_iter().map(to_wire).collect();
    let _ = hub.state_events.send(StateEvent::PendingDevices(requests));
}

/// Broadcast the waiting requests while the caller still holds the book, so
/// broadcasts go out in the order the book changed.
fn broadcast_pending(hub: &Hub, book: &PairingBook) {
    broadcast_views(hub, book.pending());
}

/// Reply with every approved device; no token or hash ever leaves the store.
fn send_devices(hub: &Hub, out_tx: &mpsc::UnboundedSender<DaemonMessage>) {
    let devices = crate::sync::lock(&hub.devices)
        .list()
        .iter()
        .map(|record| protocol::DeviceInfo {
            device_id: record.id.clone(),
            name: record.name.clone(),
            login: record.login.clone(),
            created_at: record.created_at,
            last_seen: record.last_seen,
            revoked: record.revoked,
        })
        .collect();
    let _ = out_tx.send(DaemonMessage::Devices { devices });
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert preconditions with expect and fail loudly on a wrong reply"
)]
mod tests {
    use super::*;
    use crate::server::test_support::{
        WsClient, dispatch_all, dispatch_one, env_spawn, hello, loopback_ws_client,
        spawnless_test_hub, tailnet_ws_client, test_hub, test_peer, ws_frame, ws_next, ws_send,
    };

    fn other_peer() -> tailnet::PeerIdentity {
        tailnet::PeerIdentity {
            stable_id: "nOtherNode".to_owned(),
            name: "tablet.tail1234.ts.net".to_owned(),
            user_id: 2,
            login: "owner@example.com".to_owned(),
        }
    }

    async fn next(client: &mut WsClient) -> DaemonMessage {
        ws_next(client)
            .await
            .expect("a message before the connection closes")
    }

    async fn assert_closed(client: &mut WsClient) {
        let rest = ws_next(client).await;
        assert!(
            rest.is_none(),
            "the daemon closed the connection, got {rest:?}"
        );
    }

    fn assert_failed(message: &DaemonMessage, want: &str) {
        assert!(
            matches!(message, DaemonMessage::DevicePairFailed { reason } if reason == want),
            "want DevicePairFailed {want:?}, got {message:?}"
        );
    }

    fn assert_decided(message: &DaemonMessage, id: &str, want: DevicePairOutcome) {
        assert!(
            matches!(
                message,
                DaemonMessage::DevicePairDecided { request_id, outcome }
                    if request_id == id && *outcome == want
            ),
            "want {want:?} for {id}, got {message:?}"
        );
    }

    /// A tailnet connection as `peer` whose `Hello` (no token) put it in
    /// pairing mode, past its `Welcome` and the "not approved" signal.
    async fn pairing_client(hub: &Hub, peer: tailnet::PeerIdentity) -> WsClient {
        pairing_client_named(hub, peer, None).await
    }

    async fn pairing_client_named(
        hub: &Hub,
        peer: tailnet::PeerIdentity,
        client_name: Option<&str>,
    ) -> WsClient {
        remote_on(hub);
        let mut client = tailnet_ws_client(hub, peer).await;
        ws_send(&mut client, &hello("", client_name)).await;
        let welcome = next(&mut client).await;
        assert!(
            matches!(&welcome, DaemonMessage::Welcome { features, .. }
                if features.iter().any(|f| f == protocol::FEATURE_DEVICE_PAIRING)),
            "{welcome:?}"
        );
        assert_failed(&next(&mut client).await, DEVICE_NOT_APPROVED);
        client
    }

    /// Remote access on, as it is whenever a tailnet connection exists: the
    /// switch and the status a `ConfigureRemote` compares against.
    fn remote_on(hub: &Hub) {
        hub.remote_enabled.send_replace(true);
        hub.remote_status
            .send_modify(|slot| slot.snapshot.enabled = true);
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

    /// Ask to pair on `client`; returns the request id.
    async fn ask(client: &mut WsClient) -> String {
        ws_send(client, &ClientMessage::DevicePairRequest).await;
        match next(client).await {
            DaemonMessage::DevicePairPending {
                request_id,
                expires_in_secs,
            } => {
                assert_eq!(expires_in_secs, REQUEST_LIFETIME.as_secs());
                request_id
            }
            other => panic!("want DevicePairPending, got {other:?}"),
        }
    }

    /// Approve `request_id` from loopback; returns the code it minted.
    async fn approve_from_loopback(hub: &Hub, request_id: &str) -> String {
        let sent = dispatch_all(
            hub,
            ClientMessage::ApproveDevice {
                request_id: request_id.to_owned(),
            },
        )
        .await;
        match sent.as_slice() {
            [
                DaemonMessage::DevicePairCode {
                    request_id: Some(id),
                    code,
                    expires_in_secs,
                },
            ] if id == request_id => {
                assert_eq!(*expires_in_secs, CODE_LIFETIME.as_secs());
                code.0.clone()
            }
            other => panic!("want one DevicePairCode, got {other:?}"),
        }
    }

    async fn redeem(client: &mut WsClient, request_id: &str, code: &str) {
        ws_send(
            client,
            &ClientMessage::DevicePairRedeem {
                request_id: request_id.to_owned(),
                code: Secret(code.to_owned()),
            },
        )
        .await;
    }

    fn wrong(code: &str) -> String {
        if code == "000000" { "000001" } else { "000000" }.to_owned()
    }

    fn pending_ids(hub: &Hub) -> Vec<String> {
        crate::sync::lock(&hub.pairing)
            .pending()
            .into_iter()
            .map(|view| view.id)
            .collect()
    }

    /// Pair `peer` end to end; returns its device id and token.
    async fn pair(hub: &Hub, peer: tailnet::PeerIdentity) -> (String, String) {
        let mut client = pairing_client(hub, peer).await;
        let id = ask(&mut client).await;
        let code = approve_from_loopback(hub, &id).await;
        assert_decided(&next(&mut client).await, &id, DevicePairOutcome::Approved);
        redeem(&mut client, &id, &code).await;
        let issued = match next(&mut client).await {
            DaemonMessage::DeviceToken { device_id, token } => (device_id, token.0),
            other => panic!("want DeviceToken, got {other:?}"),
        };
        assert_closed(&mut client).await;
        issued
    }

    /// Read `client` until a message `stop` matches; returns everything read
    /// before it.
    async fn read_until(
        client: &mut WsClient,
        stop: impl Fn(&DaemonMessage) -> bool,
    ) -> Vec<DaemonMessage> {
        let mut seen = Vec::new();
        loop {
            let message = next(client).await;
            if stop(&message) {
                return seen;
            }
            seen.push(message);
        }
    }

    fn is_state(message: &DaemonMessage) -> bool {
        matches!(
            message,
            DaemonMessage::Repos { .. }
                | DaemonMessage::Workspaces { .. }
                | DaemonMessage::Sessions { .. }
                | DaemonMessage::Tabs { .. }
                | DaemonMessage::LayoutInitRequired { .. }
                | DaemonMessage::RemoteStatus { .. }
        )
    }

    #[tokio::test]
    async fn tokenless_tailnet_hello_enters_pairing_mode_and_gets_no_state() {
        let (hub, _scratch) = test_hub("pairing-no-state");
        let mut client = pairing_client(&hub, test_peer()).await;
        ws_send(&mut client, &ClientMessage::DevicePairCancel).await;
        let mut rest = Vec::new();
        while let Some(message) = ws_next(&mut client).await {
            rest.push(message);
        }
        assert!(
            !rest.iter().any(is_state),
            "an unpaired device never gets state: {rest:?}"
        );
        assert_eq!(
            *hub.client_count.borrow(),
            0,
            "a pairing connection is not counted"
        );
    }

    #[tokio::test]
    async fn unpaired_connection_cannot_dispatch_other_messages() {
        let (hub, _scratch) = spawnless_test_hub("pairing-no-dispatch");
        let mut client = pairing_client(&hub, test_peer()).await;
        let spawn = env_spawn(
            protocol::SpawnTarget::Standalone {
                cwd: None,
                add_dirs: Vec::new(),
            },
            protocol::SessionMode::Interactive,
            Vec::new(),
            None,
        );
        for msg in [
            ClientMessage::ListSessions,
            ClientMessage::SpawnSession(spawn),
            ClientMessage::ListRepos,
            ClientMessage::MintDevicePairCode,
            hello("test-token", None),
        ] {
            ws_send(&mut client, &msg).await;
            assert_failed(&next(&mut client).await, DEVICE_NOT_APPROVED);
        }
        assert!(hub.sessions.snapshots().is_empty(), "nothing was spawned");
        assert!(
            crate::sync::lock(&hub.pairing).pending().is_empty(),
            "nothing was asked"
        );
    }

    /// The deadline is the hub's 10 s; the clock is paused once the device
    /// is in pairing mode and read in one-second virtual steps, so the
    /// close is observed without a real wait and without auto-advance
    /// outrunning the socket.
    #[tokio::test]
    async fn pairing_connection_without_a_request_closes_after_the_timeout() {
        let (hub, _scratch) = test_hub("pairing-idle-timeout");
        let mut client = pairing_client(&hub, test_peer()).await;
        tokio::time::pause();
        let mut closed = false;
        for _ in 0..1_000 {
            match tokio::time::timeout(Duration::from_secs(1), ws_frame(&mut client)).await {
                Ok(None) => {
                    closed = true;
                    break;
                }
                Ok(Some(message)) => panic!("an idle pairing connection is sent {message:?}"),
                Err(_) => {}
            }
        }
        assert!(closed, "the idle pairing connection closes at its deadline");
    }

    #[tokio::test]
    async fn approve_from_loopback_returns_the_code_and_tells_the_waiting_device() {
        let (hub, _scratch) = test_hub("pairing-approve");
        let mut client = pairing_client(&hub, test_peer()).await;
        let id = ask(&mut client).await;
        let code = approve_from_loopback(&hub, &id).await;
        assert_eq!(code.len(), 6, "a six-digit code");
        assert!(code.chars().all(|c| c.is_ascii_digit()));
        let told = next(&mut client).await;
        assert_decided(&told, &id, DevicePairOutcome::Approved);
        let wire = serde_json::to_string(&told).expect("serialize");
        assert!(
            !wire.contains(&code),
            "the device is never sent the code: {wire}"
        );
        let pending = crate::sync::lock(&hub.pairing).pending();
        assert!(pending[0].approved, "the request shows approved");
    }

    #[tokio::test]
    async fn redeem_with_the_right_code_issues_a_token_once_and_closes() {
        let (hub, _scratch) = test_hub("pairing-redeem");
        let mut client = pairing_client(&hub, test_peer()).await;
        let id = ask(&mut client).await;
        let code = approve_from_loopback(&hub, &id).await;
        assert_decided(&next(&mut client).await, &id, DevicePairOutcome::Approved);
        redeem(&mut client, &id, &code).await;
        let (device_id, token) = match next(&mut client).await {
            DaemonMessage::DeviceToken { device_id, token } => (device_id, token.0),
            other => panic!("want DeviceToken, got {other:?}"),
        };
        assert_closed(&mut client).await;
        {
            let devices = crate::sync::lock(&hub.devices);
            let record = devices
                .verify(&test_peer().stable_id, &token)
                .expect("the issued token verifies for this device");
            assert_eq!(record.id, device_id);
            assert_eq!(record.name, test_peer().name);
            assert_eq!(record.login, test_peer().login);
        }
        assert!(pending_ids(&hub).is_empty(), "the request is consumed");

        let mut again = pairing_client(&hub, test_peer()).await;
        redeem(&mut again, &id, &code).await;
        assert_failed(&next(&mut again).await, &PairError::Unknown.to_string());
        assert_eq!(
            crate::sync::lock(&hub.devices).list().len(),
            1,
            "the code issued one token"
        );
    }

    #[tokio::test]
    async fn token_issued_over_pairing_then_hello_is_served() {
        let (hub, _scratch) = test_hub("pairing-then-served");
        let (_, token) = pair(&hub, test_peer()).await;
        let mut client = tailnet_ws_client(&hub, test_peer()).await;
        ws_send(&mut client, &hello(&token, None)).await;
        assert!(matches!(
            next(&mut client).await,
            DaemonMessage::Welcome { .. }
        ));
        let first = next(&mut client).await;
        assert!(
            matches!(first, DaemonMessage::Repos { .. }),
            "a paired device gets its state, got {first:?}"
        );
    }

    #[tokio::test]
    async fn redeem_after_deny_gets_nothing() {
        let (hub, _scratch) = test_hub("pairing-deny");
        let mut client = pairing_client(&hub, test_peer()).await;
        let id = ask(&mut client).await;
        dispatch_all(
            &hub,
            ClientMessage::DenyDevice {
                request_id: id.clone(),
            },
        )
        .await;
        assert_decided(&next(&mut client).await, &id, DevicePairOutcome::Denied);
        assert_closed(&mut client).await;

        let mut again = pairing_client(&hub, test_peer()).await;
        redeem(&mut again, &id, "123456").await;
        assert_failed(&next(&mut again).await, &PairError::Unknown.to_string());
        assert!(
            crate::sync::lock(&hub.devices).list().is_empty(),
            "no token"
        );
    }

    #[tokio::test]
    async fn redeem_after_expiry_gets_nothing() {
        let (hub, _scratch) = test_hub("pairing-expiry");
        let mut client = pairing_client(&hub, test_peer()).await;
        let id = ask(&mut client).await;
        let code = approve_from_loopback(&hub, &id).await;
        assert_decided(&next(&mut client).await, &id, DevicePairOutcome::Approved);
        let lapsed = crate::sync::lock(&hub.pairing)
            .expire(Instant::now() + REQUEST_LIFETIME + CODE_LIFETIME);
        assert_eq!(lapsed, std::slice::from_ref(&id), "the request lapses");
        assert_decided(&next(&mut client).await, &id, DevicePairOutcome::Expired);
        assert_closed(&mut client).await;

        let mut again = pairing_client(&hub, test_peer()).await;
        redeem(&mut again, &id, &code).await;
        assert_failed(&next(&mut again).await, &PairError::Unknown.to_string());
        assert!(
            crate::sync::lock(&hub.devices).list().is_empty(),
            "no token"
        );
    }

    #[tokio::test]
    async fn wrong_code_five_times_ends_the_request() {
        let (hub, _scratch) = test_hub("pairing-wrong-code");
        let mut client = pairing_client(&hub, test_peer()).await;
        let id = ask(&mut client).await;
        let code = approve_from_loopback(&hub, &id).await;
        assert_decided(&next(&mut client).await, &id, DevicePairOutcome::Approved);
        for _ in 1..crate::devices::MAX_WRONG_ATTEMPTS {
            redeem(&mut client, &id, &wrong(&code)).await;
            assert_failed(&next(&mut client).await, &PairError::WrongCode.to_string());
        }
        redeem(&mut client, &id, &wrong(&code)).await;
        assert_failed(
            &next(&mut client).await,
            &PairError::TooManyAttempts.to_string(),
        );
        assert_closed(&mut client).await;
        assert!(pending_ids(&hub).is_empty(), "the request is gone");
        assert!(
            crate::sync::lock(&hub.devices).list().is_empty(),
            "no token"
        );
    }

    #[tokio::test]
    async fn a_new_request_from_the_same_device_supersedes_the_old_connection() {
        let (hub, _scratch) = test_hub("pairing-supersede");
        let mut old = pairing_client(&hub, test_peer()).await;
        let old_id = ask(&mut old).await;
        let mut new = pairing_client(&hub, test_peer()).await;
        let new_id = ask(&mut new).await;
        assert_decided(
            &next(&mut old).await,
            &old_id,
            DevicePairOutcome::Superseded,
        );
        assert_closed(&mut old).await;
        let code = approve_from_loopback(&hub, &new_id).await;
        assert_decided(&next(&mut new).await, &new_id, DevicePairOutcome::Approved);
        assert_eq!(
            approve_from_loopback_err(&hub, &old_id).await,
            PairError::Unknown.to_string(),
            "the superseded request is gone"
        );
        assert_eq!(code.len(), 6);
    }

    async fn approve_from_loopback_err(hub: &Hub, request_id: &str) -> String {
        match dispatch_one(
            hub,
            ClientMessage::ApproveDevice {
                request_id: request_id.to_owned(),
            },
        )
        .await
        {
            DaemonMessage::Error { message, .. } => message,
            other => panic!("want Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn old_connection_teardown_keeps_the_newer_request() {
        let (hub, _scratch) = test_hub("pairing-teardown-newer");
        let mut old = pairing_client(&hub, test_peer()).await;
        let _old_id = ask(&mut old).await;
        let mut new = pairing_client(&hub, test_peer()).await;
        let new_id = ask(&mut new).await;
        // The old connection's teardown has run once its socket closes.
        while ws_next(&mut old).await.is_some() {}
        assert_eq!(
            pending_ids(&hub),
            std::slice::from_ref(&new_id),
            "the newer request stays"
        );
        assert!(
            crate::sync::lock(&hub.pairing_waiters).contains_key(&new_id),
            "and its waiter"
        );
        approve_from_loopback(&hub, &new_id).await;
        assert_decided(&next(&mut new).await, &new_id, DevicePairOutcome::Approved);
    }

    #[tokio::test]
    async fn pending_connection_drop_removes_its_request() {
        let (hub, _scratch) = test_hub("pairing-drop");
        let mut events = hub.state_events.subscribe();
        let mut client = pairing_client(&hub, test_peer()).await;
        let id = ask(&mut client).await;
        drop(client);
        let emptied = tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                if let Ok(StateEvent::PendingDevices(requests)) = events.recv().await
                    && requests.is_empty()
                {
                    break;
                }
            }
        })
        .await;
        assert!(emptied.is_ok(), "the list empties when the device goes");
        assert!(pending_ids(&hub).is_empty(), "the request {id} is gone");
        assert!(
            crate::sync::lock(&hub.pairing_waiters).is_empty(),
            "and its waiter"
        );
    }

    #[tokio::test]
    async fn identity_comes_from_whois_not_hello() {
        let (hub, _scratch) = test_hub("pairing-identity");
        let hostile = "<b>evil</b> admin@attacker.example";
        let mut client = pairing_client_named(&hub, test_peer(), Some(hostile)).await;
        ask(&mut client).await;
        let DaemonMessage::PendingDevices { requests } =
            dispatch_one(&hub, ClientMessage::ListPendingDevices).await
        else {
            panic!("want PendingDevices");
        };
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].name, test_peer().name);
        assert_eq!(requests[0].login, test_peer().login);
        let printed = format!("{requests:?}");
        assert!(
            !printed.contains("evil"),
            "Hello's name is never shown: {printed}"
        );
    }

    #[tokio::test]
    async fn cli_code_redeems_for_a_pending_device_once() {
        let (hub, _scratch) = test_hub("pairing-cli-code");
        let DaemonMessage::DevicePairCode {
            request_id: None,
            code,
            expires_in_secs,
        } = dispatch_one(&hub, ClientMessage::MintDevicePairCode).await
        else {
            panic!("want a CLI DevicePairCode");
        };
        assert_eq!(expires_in_secs, CODE_LIFETIME.as_secs());

        let mut first = pairing_client(&hub, test_peer()).await;
        let first_id = ask(&mut first).await;
        redeem(&mut first, &first_id, &code.0).await;
        let token = match next(&mut first).await {
            DaemonMessage::DeviceToken { token, .. } => token.0,
            other => panic!("want DeviceToken, got {other:?}"),
        };
        assert_closed(&mut first).await;
        assert!(
            crate::sync::lock(&hub.devices)
                .verify(&test_peer().stable_id, &token)
                .is_some()
        );

        let mut second = pairing_client(&hub, other_peer()).await;
        let second_id = ask(&mut second).await;
        redeem(&mut second, &second_id, &code.0).await;
        assert_failed(&next(&mut second).await, &PairError::WrongCode.to_string());
        assert_eq!(
            pending_ids(&hub),
            [second_id],
            "the second request keeps waiting"
        );
        assert_eq!(crate::sync::lock(&hub.devices).list().len(), 1, "one token");
    }

    #[tokio::test]
    async fn typing_a_code_before_approval_keeps_the_request() {
        let (hub, _scratch) = test_hub("pairing-early-code");
        let mut client = pairing_client(&hub, test_peer()).await;
        let id = ask(&mut client).await;
        redeem(&mut client, &id, "123456").await;
        assert_failed(&next(&mut client).await, &PairError::WrongCode.to_string());
        assert_eq!(
            pending_ids(&hub),
            std::slice::from_ref(&id),
            "the request keeps waiting"
        );
        let code = approve_from_loopback(&hub, &id).await;
        assert_decided(&next(&mut client).await, &id, DevicePairOutcome::Approved);
        redeem(&mut client, &id, &code).await;
        assert!(matches!(
            next(&mut client).await,
            DaemonMessage::DeviceToken { .. }
        ));
    }

    #[tokio::test]
    async fn redeem_of_another_connections_request_neither_consumes_nor_costs_it() {
        let (hub, _scratch) = test_hub("pairing-foreign-redeem");
        let mut owner = pairing_client(&hub, test_peer()).await;
        let id = ask(&mut owner).await;
        let code = approve_from_loopback(&hub, &id).await;
        assert_decided(&next(&mut owner).await, &id, DevicePairOutcome::Approved);

        // A second connection from the same device, with no request.
        let mut other = pairing_client(&hub, test_peer()).await;
        redeem(&mut other, &id, &code).await;
        assert_failed(&next(&mut other).await, &PairError::NotYours.to_string());
        for _ in 0..crate::devices::MAX_WRONG_ATTEMPTS {
            redeem(&mut other, &id, &wrong(&code)).await;
            assert_failed(&next(&mut other).await, &PairError::NotYours.to_string());
        }
        redeem(&mut other, "no-such-request", &code).await;
        assert_failed(&next(&mut other).await, &PairError::Unknown.to_string());
        assert!(
            crate::sync::lock(&hub.devices).list().is_empty(),
            "no token yet"
        );
        assert_eq!(
            pending_ids(&hub),
            std::slice::from_ref(&id),
            "the request is intact"
        );

        redeem(&mut owner, &id, &code).await;
        assert!(
            matches!(next(&mut owner).await, DaemonMessage::DeviceToken { .. }),
            "no attempt was spent, so the owner's right code still works"
        );
    }

    #[tokio::test]
    async fn pairing_after_remote_off_is_refused_and_closes() {
        let (hub, _scratch) = test_hub("pairing-after-remote-off");
        let mut idle = pairing_client(&hub, test_peer()).await;
        dispatch_all(
            &hub,
            ClientMessage::ConfigureRemote {
                enabled: false,
                port: None,
            },
        )
        .await;
        assert!(!*hub.remote_enabled.borrow(), "the switch is off");
        // The request reaches the session before or after its tick; either
        // way it is refused and the connection closes.
        ws_send(&mut idle, &ClientMessage::DevicePairRequest).await;
        assert_failed(&next(&mut idle).await, REMOTE_OFF);
        assert_closed(&mut idle).await;
        assert!(pending_ids(&hub).is_empty(), "no request was added");
        assert!(crate::sync::lock(&hub.pairing_waiters).is_empty());
    }

    #[tokio::test]
    async fn cli_code_after_deny_issues_nothing() {
        let (hub, _scratch) = test_hub("pairing-cli-after-deny");
        let DaemonMessage::DevicePairCode { code, .. } =
            dispatch_one(&hub, ClientMessage::MintDevicePairCode).await
        else {
            panic!("want a CLI DevicePairCode");
        };
        let mut client = pairing_client(&hub, test_peer()).await;
        let id = ask(&mut client).await;
        // Denied in the book with no waiter message, as when the session has
        // not read its waiter yet: the redeem frame comes first.
        assert!(crate::sync::lock(&hub.pairing).deny(&id));
        redeem(&mut client, &id, &code.0).await;
        let mut rest = Vec::new();
        while let Some(message) = ws_next(&mut client).await {
            rest.push(message);
        }
        assert!(
            !rest
                .iter()
                .any(|m| matches!(m, DaemonMessage::DeviceToken { .. })),
            "a denied request redeems nothing: {rest:?}"
        );
        assert!(
            crate::sync::lock(&hub.devices).list().is_empty(),
            "no token"
        );
    }

    #[tokio::test]
    async fn pairing_connection_closes_after_sixteen_stray_frames() {
        let (hub, _scratch) = test_hub("pairing-stray-frames");
        let mut client = pairing_client(&hub, test_peer()).await;
        for _ in 0..MAX_STRAY_FRAMES - 2 {
            ws_send(&mut client, &ClientMessage::ListSessions).await;
            assert_failed(&next(&mut client).await, DEVICE_NOT_APPROVED);
        }
        send_raw(&mut client, r#"{"type":"no_such_message"}"#).await;
        send_raw(&mut client, "not json").await;
        assert_closed(&mut client).await;
    }

    #[tokio::test]
    async fn fourth_request_on_one_connection_closes_it() {
        let (hub, _scratch) = test_hub("pairing-fourth-request");
        let mut client = pairing_client(&hub, test_peer()).await;
        let mut last = String::new();
        for _ in 0..MAX_REQUESTS {
            last = ask(&mut client).await;
        }
        assert_eq!(pending_ids(&hub), std::slice::from_ref(&last));
        ws_send(&mut client, &ClientMessage::DevicePairRequest).await;
        assert_failed(&next(&mut client).await, TOO_MANY_REQUESTS);
        assert_closed(&mut client).await;
        assert!(pending_ids(&hub).is_empty(), "its request goes with it");
    }

    #[tokio::test]
    async fn redeem_after_the_code_lapsed_says_it_expired() {
        let (hub, _scratch) = test_hub("pairing-code-lapsed");
        let mut client = pairing_client(&hub, test_peer()).await;
        let id = ask(&mut client).await;
        let code = approve_from_loopback(&hub, &id).await;
        assert_decided(&next(&mut client).await, &id, DevicePairOutcome::Approved);
        let lapsed = crate::sync::lock(&hub.pairing).expire(Instant::now() + CODE_LIFETIME);
        assert!(lapsed.is_empty(), "only the code lapsed");
        redeem(&mut client, &id, &code).await;
        assert_failed(&next(&mut client).await, CODE_LAPSED);
        assert_eq!(
            pending_ids(&hub),
            std::slice::from_ref(&id),
            "the request waits"
        );
        let fresh = approve_from_loopback(&hub, &id).await;
        assert_decided(&next(&mut client).await, &id, DevicePairOutcome::Approved);
        redeem(&mut client, &id, &fresh).await;
        assert!(matches!(
            next(&mut client).await,
            DaemonMessage::DeviceToken { .. }
        ));
    }

    /// The previous request leaves the book with no waiter message; the
    /// session's tick may notice first and close, which is the benign path,
    /// so the scenario is tried again.
    #[tokio::test]
    async fn re_request_leaves_one_waiter_entry() {
        let (hub, _scratch) = test_hub("pairing-re-request");
        for _ in 0..20 {
            let mut client = pairing_client(&hub, test_peer()).await;
            let first = ask(&mut client).await;
            assert!(crate::sync::lock(&hub.pairing).deny(&first));
            ws_send(&mut client, &ClientMessage::DevicePairRequest).await;
            match next(&mut client).await {
                DaemonMessage::DevicePairPending { request_id, .. } => {
                    let waiters = crate::sync::lock(&hub.pairing_waiters);
                    assert_eq!(
                        waiters.keys().collect::<Vec<_>>(),
                        [&request_id],
                        "only the current request has a waiter"
                    );
                    return;
                }
                DaemonMessage::DevicePairDecided { .. } => {
                    while ws_next(&mut client).await.is_some() {}
                }
                other => panic!("want DevicePairPending, got {other:?}"),
            }
        }
        panic!("the tick closed every attempt first");
    }

    #[tokio::test]
    async fn remote_off_drops_pending_requests() {
        let (hub, _scratch) = test_hub("pairing-remote-off");
        let mut client = pairing_client(&hub, test_peer()).await;
        let id = ask(&mut client).await;
        dispatch_all(
            &hub,
            ClientMessage::ConfigureRemote {
                enabled: false,
                port: None,
            },
        )
        .await;
        assert_decided(&next(&mut client).await, &id, DevicePairOutcome::Expired);
        assert_closed(&mut client).await;
        assert!(pending_ids(&hub).is_empty(), "the book is empty");
        assert!(crate::sync::lock(&hub.pairing_waiters).is_empty());
    }

    #[tokio::test]
    async fn pending_devices_reach_loopback_clients_only() {
        let (hub, _scratch) = test_hub("pairing-broadcast");
        let mut local = loopback_ws_client(&hub).await;
        ws_send(&mut local, &hello("test-token", None)).await;
        read_until(&mut local, |m| {
            matches!(m, DaemonMessage::RemoteStatus { .. })
        })
        .await;

        let (_, token) = pair(&hub, other_peer()).await;
        let mut served = tailnet_ws_client(&hub, other_peer()).await;
        ws_send(&mut served, &hello(&token, None)).await;
        read_until(&mut served, |m| {
            matches!(m, DaemonMessage::RemoteStatus { .. })
        })
        .await;

        let mut device = pairing_client(&hub, test_peer()).await;
        let id = ask(&mut device).await;
        // A marker every forwarder passes on after anything sent before it.
        let _ = hub
            .state_events
            .send(StateEvent::SessionHistory(Vec::new()));

        let marker = |m: &DaemonMessage| matches!(m, DaemonMessage::SessionHistory { .. });
        let to_local = read_until(&mut local, marker).await;
        assert!(
            to_local
                .iter()
                .any(|m| matches!(m, DaemonMessage::PendingDevices { requests }
                if requests.iter().any(|r| r.request_id == id))),
            "the loopback client hears of the request: {to_local:?}"
        );
        let to_served = read_until(&mut served, marker).await;
        assert!(
            !to_served
                .iter()
                .any(|m| matches!(m, DaemonMessage::PendingDevices { .. })),
            "a served tailnet device never does: {to_served:?}"
        );
    }
}
