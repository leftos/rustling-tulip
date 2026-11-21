//! The daemon's Linear wiring: the one Linear client every connection and the
//! Dashboard share, the title scan that fetches the issues session titles and
//! labels name, the 5-minute refresh of the issues live sessions name, the
//! answer to [`ClientMessage::GetLinearIssues`], the Linear settings messages
//! (the key, the switch and the Test) and the `LinearContext`,
//! `LinearIssues` and `LinearStatus` broadcasts.
//!
//! The client, its key, the switch, its cache and the identifiers Linear did
//! not know are read and written only under [`LinearHub`]'s one async lock,
//! so while the switch is off no request starts, and the key in use never
//! changes during a fetch. What the clients were last sent (the context, the
//! status and every issue) is mirrored under a plain lock, written while the
//! async lock is held and broadcast under the mirror's lock, so a
//! connection's initial state, read under that lock, never lands after a
//! newer broadcast. The Test's request runs with neither lock held.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use chrono::{DateTime, TimeDelta, Utc};
use futures::future::BoxFuture;
use protocol::{
    ClientMessage, DaemonMessage, LinearIssue, LinearKeyState, SessionSnapshot, SessionStatus,
};
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc, watch};
use tracing::{debug, error, info, warn};

use super::connection::ConnCtx;
use super::{Hub, StateEvent};
use crate::linear::{
    Failure, LinearCache, LinearClient, LinearContextData, LinearError, LinearTransport,
    ReqwestTransport, identifiers_in,
};
use crate::linear_key::REJECTION_ID;
use crate::session::{SessionEvent, SessionRegistry};
use crate::state::AppState;
use crate::summarizer::keys::Rejections;
use crate::user_env::Secret;

/// The Test's answer when the daemon has no key to test.
const NO_KEY_ERROR: &str = "No Linear API key is set.";
/// The Test's answer on a daemon taken off Linear.
const OFFLINE_ERROR: &str =
    "Linear is turned off for this daemon (RUSTLING_TULIP_LINEAR_OFFLINE=1).";

/// Where the hub chooses its key: the key and its state. It may block on the
/// secret store, so an async caller runs it on a blocking thread.
type KeySource = fn() -> (Option<Secret>, LinearKeyState);

/// How often the issues live sessions name are fetched again.
pub(crate) const REFRESH_TICK: Duration = Duration::from_secs(300);
/// How long an identifier Linear answered without is not asked for again.
const UNKNOWN_TTL: TimeDelta = TimeDelta::minutes(10);
/// How long the title scan leaves an identifier it asked for, whatever came
/// of it: a title that changes many times a second (an agent's spinner)
/// never asks again after a failed batch. The refresh and a hover still do.
const SCAN_RETRY: TimeDelta = TimeDelta::minutes(5);

/// Where the wiring reads the time from: the wall clock, or in tests tokio's
/// clock, so a paused test moves it with `tokio::time::advance`.
#[derive(Debug, Clone, Copy)]
enum Clock {
    System,
    #[cfg(test)]
    Tokio {
        base: DateTime<Utc>,
        start: tokio::time::Instant,
    },
}

impl Clock {
    #[cfg(test)]
    fn tokio() -> Self {
        Self::Tokio {
            base: Utc::now(),
            start: tokio::time::Instant::now(),
        }
    }

    fn now(self) -> DateTime<Utc> {
        match self {
            Self::System => Utc::now(),
            #[cfg(test)]
            Self::Tokio { base, start } => {
                base + TimeDelta::from_std(start.elapsed()).unwrap_or_default()
            }
        }
    }
}

/// The transport of a daemon whose HTTP client could not start: every
/// request fails. It is never called, since such a daemon has no key.
struct NoTransport;

impl LinearTransport for NoTransport {
    fn post<'a>(
        &'a self,
        _key: &'a Secret,
        _body: Value,
    ) -> BoxFuture<'a, Result<Value, LinearError>> {
        Box::pin(async { Err(LinearError::Shape("no HTTP client".to_owned())) })
    }
}

/// The key source of a daemon that never reads one.
fn no_key() -> (Option<Secret>, LinearKeyState) {
    (None, LinearKeyState::Missing)
}

/// A copy of `key`.
fn copy(key: &Secret) -> Secret {
    Secret::new(key.expose().to_owned())
}

/// Run `source` on a blocking thread; `None` when that thread failed.
async fn choose_key(source: KeySource) -> Option<(Option<Secret>, LinearKeyState)> {
    match tokio::task::spawn_blocking(source).await {
        Ok(chosen) => Some(chosen),
        Err(err) => {
            warn!(error = %err, "linear: choosing the key failed; the key in use stays");
            None
        }
    }
}

/// `LinearContext` as the clients were sent it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct WireContext {
    team_keys: Vec<String>,
    workspace: Option<String>,
    key_configured: bool,
}

impl WireContext {
    /// The context to publish: empty, with no key configured, while the
    /// switch is off, so an older client draws nothing either.
    fn of(service: &Service) -> Self {
        if !service.enabled {
            return Self::default();
        }
        let client = &service.client;
        let data = client.context().cloned().unwrap_or_default();
        Self {
            team_keys: data.team_keys,
            workspace: data.workspace,
            key_configured: client.key_configured(),
        }
    }

    fn message(&self) -> DaemonMessage {
        DaemonMessage::LinearContext {
            team_keys: self.team_keys.clone(),
            workspace: self.workspace.clone(),
            key_configured: self.key_configured,
        }
    }

    fn event(&self) -> StateEvent {
        StateEvent::LinearContext {
            team_keys: self.team_keys.clone(),
            workspace: self.workspace.clone(),
            key_configured: self.key_configured,
        }
    }
}

/// `LinearStatus` as the clients were sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WireStatus {
    enabled: bool,
    key: LinearKeyState,
    workspace_name: Option<String>,
    team_keys: Vec<String>,
    last_error: Option<String>,
    /// Whether the key in use is the saved one.
    key_saved: bool,
}

impl WireStatus {
    fn of(service: &Service) -> Self {
        let data = service.client.context().cloned().unwrap_or_default();
        Self {
            enabled: service.enabled,
            key: service.key_state(),
            workspace_name: data.workspace_name,
            team_keys: data.team_keys,
            last_error: service.last_error.clone(),
            key_saved: service.chosen == LinearKeyState::Stored,
        }
    }

    fn message(&self) -> DaemonMessage {
        DaemonMessage::LinearStatus {
            enabled: self.enabled,
            key: self.key,
            workspace_name: self.workspace_name.clone(),
            team_keys: self.team_keys.clone(),
            last_error: self.last_error.clone(),
            key_saved: self.key_saved,
        }
    }
}

/// What the clients were last sent.
struct Published {
    context: WireContext,
    status: WireStatus,
    issues: HashMap<String, LinearIssue>,
}

/// The client, the switch, the key's state and the identifiers Linear did
/// not know, behind the one lock.
struct Service {
    client: LinearClient,
    /// Each identifier a batch Linear answered came back without, with when.
    unknown: HashMap<String, DateTime<Utc>>,
    /// Each identifier the title scan asked for in the last 5 minutes, with
    /// when; only the scan reads it.
    scan_tried: HashMap<String, DateTime<Utc>>,
    /// The host's Linear switch; off, no request is made.
    enabled: bool,
    /// The state the key in use was chosen with.
    chosen: LinearKeyState,
    /// The key Linear refused, if it did, until the key changes or a Test
    /// with it succeeds.
    rejections: Rejections,
    /// The display text of the last failed request, until the next success.
    last_error: Option<String>,
}

impl Service {
    fn new(client: LinearClient, chosen: LinearKeyState, enabled: bool) -> Self {
        Self {
            client,
            unknown: HashMap::new(),
            scan_tried: HashMap::new(),
            enabled,
            chosen,
            rejections: Rejections::default(),
            last_error: None,
        }
    }

    /// Whether Linear refused the key in use.
    fn key_rejected(&self) -> bool {
        self.client
            .key()
            .is_some_and(|key| self.rejections.is_rejected(REJECTION_ID, key))
    }

    /// Whether a request may be made: the switch is on and a key Linear has
    /// not refused is in use.
    fn usable(&self) -> bool {
        self.enabled && self.client.key_configured() && !self.key_rejected()
    }

    /// The key's state as clients are told it.
    fn key_state(&self) -> LinearKeyState {
        if self.key_rejected() {
            LinearKeyState::Rejected
        } else {
            self.chosen
        }
    }

    /// Use `key`, chosen with `state`. A different key forgets what the old
    /// one was refused or answered without, so it is asked again.
    fn use_key(&mut self, (key, state): (Option<Secret>, LinearKeyState)) {
        if self.client.key().map(Secret::expose) != key.as_ref().map(Secret::expose) {
            self.forget_tries();
        }
        self.client.set_key(key);
        self.chosen = state;
    }

    /// Forget the identifiers the scan asked for and the ones Linear
    /// answered without, so the next scan and request ask for them again.
    fn forget_tries(&mut self) {
        self.scan_tried.clear();
        self.unknown.clear();
    }

    /// Record how a request ended: a success clears the last error; a
    /// failure becomes it, and a refused key is remembered.
    fn record(&mut self, failure: Option<&Failure>) {
        let Some(failure) = failure else {
            self.last_error = None;
            return;
        };
        self.last_error = Some(failure.text.clone());
        if failure.rejected
            && let Some(key) = self.client.key()
        {
            self.rejections.reject(REJECTION_ID, key);
        }
    }

    /// Of the scan's `wanted`, the ones it has not asked for in the last 5
    /// minutes, each recorded as asked at `now`.
    fn scan_due(&mut self, wanted: Vec<String>, now: DateTime<Utc>) -> Vec<String> {
        self.scan_tried.retain(|_, since| now - *since < SCAN_RETRY);
        let due: Vec<String> = wanted
            .into_iter()
            .filter(|identifier| !self.scan_tried.contains_key(identifier))
            .collect();
        for identifier in &due {
            self.scan_tried.insert(identifier.clone(), now);
        }
        due
    }

    /// Of `identifiers`, deduplicated, the ones to ask Linear for at `now`:
    /// not answered without in the last 10 minutes, and `due` by the cache.
    fn wanted(
        &mut self,
        identifiers: &[String],
        now: DateTime<Utc>,
        due: impl Fn(&LinearCache, &str) -> bool,
    ) -> Vec<String> {
        self.unknown.retain(|_, since| now - *since < UNKNOWN_TTL);
        let mut wanted: Vec<String> = Vec::new();
        for identifier in identifiers {
            if !wanted.contains(identifier)
                && !self.unknown.contains_key(identifier)
                && due(self.client.cache(), identifier)
            {
                wanted.push(identifier.clone());
            }
        }
        wanted
    }

    /// Ask Linear for `identifiers` at `now`, recording the ones a batch
    /// answered without and how the request ended; nothing is asked while
    /// the switch is off, with no usable key or with no identifier.
    async fn fetch(&mut self, identifiers: &[String], now: DateTime<Utc>) -> Vec<LinearIssue> {
        if identifiers.is_empty() || !self.usable() {
            return Vec::new();
        }
        let outcome = self.client.fetch_issues(identifiers, now).await;
        self.record(outcome.failure.as_ref());
        for identifier in outcome.unknown {
            self.unknown.insert(identifier, now);
        }
        outcome.issues
    }
}

/// The daemon's one Linear client, its cache and what the clients were last
/// sent. The Dashboard reuses it.
pub(crate) struct LinearHub {
    service: AsyncMutex<Service>,
    published: Mutex<Published>,
    clock: Clock,
    /// Where the hub chooses its key after a key change, and where the
    /// 5-minute tick looks for one while none is in use.
    key_source: KeySource,
    /// Whether `RUSTLING_TULIP_LINEAR_OFFLINE` took the daemon off Linear.
    offline: bool,
}

impl LinearHub {
    fn new(service: Service, clock: Clock, key_source: KeySource, offline: bool) -> Arc<Self> {
        let published = Published {
            context: WireContext::of(&service),
            status: WireStatus::of(&service),
            issues: HashMap::new(),
        };
        Arc::new(Self {
            service: AsyncMutex::new(service),
            published: Mutex::new(published),
            clock,
            key_source,
            offline,
        })
    }

    /// The daemon's hub: the key saved in the secret store, else the one
    /// `LINEAR_API_KEY` holds, posting over HTTPS, with the switch at
    /// `enabled`; with `RUSTLING_TULIP_LINEAR_OFFLINE` exactly `1`, no key is
    /// ever read. Reads the store at once.
    pub(crate) fn live(enabled: bool) -> Arc<Self> {
        let offline = std::env::var(crate::linear::OFFLINE_VARIABLE).ok();
        let offline = crate::linear::is_offline(offline.as_deref());
        if offline {
            info!(
                "linear offline: RUSTLING_TULIP_LINEAR_OFFLINE is 1, so this daemon reads no \
                 Linear key and never calls Linear"
            );
        }
        match ReqwestTransport::new() {
            Ok(transport) => {
                let (key, state) = crate::linear_key::current();
                let client = LinearClient::new(key, Box::new(transport));
                let service = Service::new(client, state, enabled);
                Self::new(service, Clock::System, crate::linear_key::current, offline)
            }
            Err(err) => {
                error!(
                    error = %format!("{err:#}"),
                    "linear: the HTTP client could not start; Linear stays off until the daemon restarts"
                );
                let client = LinearClient::new(None, Box::new(NoTransport));
                let service = Service::new(client, LinearKeyState::Missing, enabled);
                Self::new(service, Clock::System, no_key, offline)
            }
        }
    }

    /// A hub over `client`, switched on, that never looks for another key,
    /// on tokio's clock; a key it holds reads as saved.
    #[cfg(test)]
    pub(crate) fn with_client(client: LinearClient) -> Arc<Self> {
        let state = if client.key_configured() {
            LinearKeyState::Stored
        } else {
            LinearKeyState::Missing
        };
        Self::new(
            Service::new(client, state, true),
            Clock::tokio(),
            no_key,
            false,
        )
    }

    /// A test hub's: no key, so nothing is ever posted.
    #[cfg(test)]
    pub(crate) fn without_key() -> Arc<Self> {
        Self::with_client(LinearClient::new(None, Box::new(NoTransport)))
    }

    /// Send `out_tx` the context, the status and every issue the clients
    /// were last sent (none when there is none), under the lock every
    /// broadcast is sent under.
    pub(crate) fn send_current(&self, out_tx: &mpsc::UnboundedSender<DaemonMessage>) {
        let published = crate::sync::lock(&self.published);
        let _ = out_tx.send(published.context.message());
        let _ = out_tx.send(published.status.message());
        if !published.issues.is_empty() {
            let _ = out_tx.send(DaemonMessage::LinearIssues {
                issues: published.issues.values().cloned().collect(),
            });
        }
    }

    /// The team keys the clients were last sent.
    fn team_keys(&self) -> Vec<String> {
        crate::sync::lock(&self.published).context.team_keys.clone()
    }

    /// Record `issues` as sent and broadcast them; nothing when there are
    /// none. Called with the service lock held.
    fn publish_issues(&self, issues: Vec<LinearIssue>, events: &broadcast::Sender<StateEvent>) {
        if issues.is_empty() {
            return;
        }
        let mut published = crate::sync::lock(&self.published);
        for issue in &issues {
            published
                .issues
                .insert(issue.identifier.clone(), issue.clone());
        }
        let _ = events.send(StateEvent::LinearIssues(issues));
    }

    /// Broadcast the context and the status, each when it differs from the
    /// one last sent; whether the context did. Called with the service lock
    /// held.
    fn publish(&self, service: &Service, events: &broadcast::Sender<StateEvent>) -> bool {
        let context = WireContext::of(service);
        let status = WireStatus::of(service);
        let mut published = crate::sync::lock(&self.published);
        let context_changed = published.context != context;
        if context_changed {
            let _ = events.send(context.event());
            published.context = context;
        }
        if published.status != status {
            let _ = events.send(StateEvent::LinearStatus(Box::new(status.message())));
            published.status = status;
        }
        context_changed
    }

    /// Fetch the team keys when they are due, or at once when `force`,
    /// keeping the last ones when the fetch fails, and broadcast the context
    /// and status on change; whether the context changed. Nothing is asked
    /// while the switch is off or with no usable key.
    async fn refresh_context(
        &self,
        service: &mut Service,
        events: &broadcast::Sender<StateEvent>,
        force: bool,
    ) -> bool {
        let now = self.clock.now();
        if service.usable() && (force || service.client.context_due(now)) {
            match service.client.refresh_context(now).await {
                Ok(()) => service.record(None),
                Err(err) => {
                    warn!(error = %err, "linear: the team keys could not be fetched; the last ones stay");
                    service.record(Some(&Failure::of(&err)));
                }
            }
        }
        self.publish(service, events)
    }

    /// Save the switch at `enabled` in `state.json` and apply it, in one
    /// critical section under the service lock, then broadcast what changed.
    /// The caller refreshes the context and rescans when it turned on.
    ///
    /// # Errors
    ///
    /// `state.json` could not be written; the switch is left as it was, on
    /// disk and in memory.
    pub(crate) async fn set_enabled(
        &self,
        state: &AppState,
        enabled: bool,
        events: &broadcast::Sender<StateEvent>,
    ) -> anyhow::Result<()> {
        let mut service = self.service.lock().await;
        state.set_linear_enabled(enabled)?;
        service.enabled = enabled;
        self.publish(&service, events);
        Ok(())
    }

    /// Save `key`, or delete the saved key when `None`, on a blocking
    /// thread, choose the key again and use it, all in one critical section
    /// under the service lock, then broadcast what changed.
    ///
    /// # Errors
    ///
    /// The key is blank or too long, or the store refused the change or is
    /// out of reach; the key in use is left as it was. No message carries
    /// the key.
    async fn save_key(
        &self,
        key: Option<Secret>,
        events: &broadcast::Sender<StateEvent>,
    ) -> anyhow::Result<()> {
        let mut service = self.service.lock().await;
        let source = self.key_source;
        let chosen = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            match &key {
                Some(key) => crate::linear_key::set(key)?,
                None => crate::linear_key::delete()?,
            }
            Ok(source())
        })
        .await
        .context("the secret store task failed")??;
        service.use_key(chosen);
        self.publish(&service, events);
        Ok(())
    }

    /// Send `out_tx` the status the clients were last sent, under the lock
    /// every broadcast is sent under: the echo of a successful Set.
    fn send_status(&self, out_tx: &mpsc::UnboundedSender<DaemonMessage>) {
        let published = crate::sync::lock(&self.published);
        let _ = out_tx.send(published.status.message());
    }

    /// Check the key in use against Linear now, whatever the switch and the
    /// context cache: the workspace it reaches, or the error to show. The
    /// request runs with no lock held; its outcome is recorded only while
    /// the same key is still in use, and broadcast on change.
    pub(crate) async fn test(
        &self,
        events: &broadcast::Sender<StateEvent>,
    ) -> Result<LinearContextData, String> {
        if self.offline {
            return Err(OFFLINE_ERROR.to_owned());
        }
        let (key, transport) = {
            let service = self.service.lock().await;
            (service.client.key().map(copy), service.client.transport())
        };
        let Some(key) = key else {
            return Err(NO_KEY_ERROR.to_owned());
        };
        let result = crate::linear::test_key(transport.as_ref(), &key).await;
        let mut service = self.service.lock().await;
        if service.client.key().map(Secret::expose) == Some(key.expose()) {
            match &result {
                Ok(_) => {
                    if service.key_rejected() {
                        service.forget_tries();
                    }
                    service.rejections = Rejections::default();
                    service.record(None);
                }
                Err(err) => service.record(Some(&Failure::of(err))),
            }
            self.publish(&service, events);
        }
        result.map_err(|err| err.to_string())
    }

    /// Answer a request for `identifiers`: the copies missing or older than a
    /// minute are fetched. `None` when the fetch broadcast something, which
    /// answers the requester too, and while the switch is off, when nothing
    /// is answered; else the cached copies it named, possibly none, for the
    /// requester alone.
    pub(crate) async fn request(
        &self,
        identifiers: &[String],
        events: &broadcast::Sender<StateEvent>,
    ) -> Option<Vec<LinearIssue>> {
        let mut service = self.service.lock().await;
        if !service.enabled {
            return None;
        }
        let now = self.clock.now();
        let wanted = service.wanted(identifiers, now, |cache, id| cache.needs_fetch(id, now));
        let fetched = service.fetch(&wanted, now).await;
        self.publish(&service, events);
        if !fetched.is_empty() {
            self.publish_issues(fetched, events);
            return None;
        }
        let mut cached: Vec<LinearIssue> = Vec::new();
        for identifier in identifiers {
            if let Some(issue) = service.client.cache().get(identifier)
                && !cached.iter().any(|seen| &seen.identifier == identifier)
            {
                cached.push(issue.clone());
            }
        }
        Some(cached)
    }
}

/// Handle the Linear messages `dispatch` routes here, each on a task of its
/// own, so the connection never waits on Linear or on the hub's lock. No log
/// line or reply carries a key.
pub(super) fn handle(hub: &Hub, m: ClientMessage, ctx: &ConnCtx<'_>) {
    match m {
        ClientMessage::GetLinearIssues { identifiers, .. } => {
            spawn_request(hub, ctx.out_tx.clone(), identifiers);
        }
        ClientMessage::SetLinearKey { key, request_id } => {
            let key = key.map(|key| Secret::new(key.0));
            spawn_set(hub, ctx.out_tx.clone(), Change::Key(key), request_id);
        }
        ClientMessage::SetLinearEnabled {
            enabled,
            request_id,
        } => {
            spawn_set(
                hub,
                ctx.out_tx.clone(),
                Change::Enabled(enabled),
                request_id,
            );
        }
        ClientMessage::TestLinear { request_id } => {
            spawn_test(hub, ctx.out_tx.clone(), request_id);
        }
        _ => {
            warn!("linear::handle: a message that is not Linear's was routed here");
        }
    }
}

/// A Linear settings change a client asked for.
enum Change {
    /// Save the key, or delete the saved key when `None`.
    Key(Option<Secret>),
    /// Turn the switch.
    Enabled(bool),
}

/// Make `change` on a task of its own. A success is echoed to the requester
/// with the current status (whatever the broadcast sent everyone), then the
/// context is refreshed and every session rescanned when Linear may now be
/// asked; a refusal is answered to the requester alone.
fn spawn_set(
    hub: &Hub,
    out_tx: mpsc::UnboundedSender<DaemonMessage>,
    change: Change,
    request_id: Option<String>,
) {
    let state = Arc::clone(&hub.state);
    let wiring = Wiring::of(hub);
    tokio::spawn(async move {
        let linear = &wiring.linear;
        let outcome = match change {
            Change::Key(key) => linear.save_key(key, &wiring.events).await.map(|()| true),
            Change::Enabled(enabled) => linear
                .set_enabled(&state, enabled, &wiring.events)
                .await
                .map(|()| enabled),
        };
        match outcome {
            Ok(refresh) => {
                linear.send_status(&out_tx);
                if refresh {
                    wiring.refresh_all().await;
                }
            }
            Err(err) => answer_refusal(&out_tx, &err, request_id),
        }
    });
}

/// Answer [`ClientMessage::GetLinearIssues`] on a task of its own, so a slow
/// Linear never holds up the connection: a fetch is answered by the
/// `LinearIssues` broadcast, a request while the switch is off by nothing,
/// anything else by a `LinearIssues` to the requester alone.
fn spawn_request(
    hub: &Hub,
    out_tx: mpsc::UnboundedSender<DaemonMessage>,
    identifiers: Vec<String>,
) {
    let linear = Arc::clone(&hub.linear);
    let events = hub.state_events.clone();
    tokio::spawn(async move {
        if let Some(issues) = linear.request(&identifiers, &events).await {
            let _ = out_tx.send(DaemonMessage::LinearIssues { issues });
        }
    });
}

/// Answer a refused Set to its requester alone with an `Error` echoing its
/// `request_id` (`None` when it carried none) and code `linear_refused`.
fn answer_refusal(
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    err: &anyhow::Error,
    request_id: Option<String>,
) {
    warn!(error = %format!("{err:#}"), request_id = request_id.as_deref().unwrap_or(""), "linear request refused");
    let _ = out_tx.send(DaemonMessage::Error {
        message: err.to_string(),
        request_id,
        code: Some(protocol::error_codes::LINEAR_REFUSED.to_owned()),
    });
}

/// Run the Test on its own task, so a slow Linear does not hold up the
/// connection, and answer the requester alone with its `LinearTest`. A
/// successful Test then fetches the team keys at once and scans every
/// session again, so a team added since the last fetch is picked up; a
/// failed one changes nothing more.
fn spawn_test(hub: &Hub, out_tx: mpsc::UnboundedSender<DaemonMessage>, request_id: Option<String>) {
    let wiring = Wiring::of(hub);
    tokio::spawn(async move {
        let result = wiring.linear.test(&wiring.events).await;
        let passed = result.is_ok();
        let reply = match result {
            Ok(data) => DaemonMessage::LinearTest {
                workspace_name: Some(data.workspace_name.or(data.workspace).unwrap_or_default()),
                team_keys: data.team_keys,
                error: None,
                request_id,
            },
            Err(error) => DaemonMessage::LinearTest {
                workspace_name: None,
                team_keys: Vec::new(),
                error: Some(error),
                request_id,
            },
        };
        let _ = out_tx.send(reply);
        if passed {
            // `test` has released the service lock; `refresh_all` takes it
            // afresh, never nested.
            wiring.refresh_all().await;
        }
    });
}

/// The identifiers in `snapshot`'s label, user label and terminal title.
fn session_identifiers(snapshot: &SessionSnapshot, team_keys: &[String]) -> Vec<String> {
    [
        Some(snapshot.label.as_str()),
        snapshot.user_label.as_deref(),
        snapshot.terminal_title.as_deref(),
    ]
    .into_iter()
    .flatten()
    .flat_map(|text| identifiers_in(text, team_keys))
    .collect()
}

/// Whether `snapshot`'s session has a live child.
fn is_live(snapshot: &SessionSnapshot) -> bool {
    !snapshot.is_abandoned
        && !matches!(
            snapshot.status,
            SessionStatus::Stopped | SessionStatus::Error
        )
}

/// What the title scan and the refresh work over.
#[derive(Clone)]
struct Wiring {
    linear: Arc<LinearHub>,
    sessions: Arc<SessionRegistry>,
    events: broadcast::Sender<StateEvent>,
}

impl Wiring {
    fn of(hub: &Hub) -> Self {
        Self {
            linear: Arc::clone(&hub.linear),
            sessions: Arc::clone(&hub.sessions),
            events: hub.state_events.clone(),
        }
    }

    /// Fetch the identifiers in `snapshots`' titles and labels that have no
    /// cached copy, that Linear did not lately answer without, and that the
    /// scan did not ask for in the last 5 minutes.
    async fn scan(&self, snapshots: &[SessionSnapshot]) {
        let team_keys = self.linear.team_keys();
        let identifiers: Vec<String> = snapshots
            .iter()
            .flat_map(|snapshot| session_identifiers(snapshot, &team_keys))
            .collect();
        if identifiers.is_empty() {
            return;
        }
        let mut service = self.linear.service.lock().await;
        if !service.usable() {
            // Nothing is asked, so nothing is recorded as tried.
            return;
        }
        let now = self.linear.clock.now();
        let wanted = service.wanted(&identifiers, now, |cache, id| cache.get(id).is_none());
        let wanted = service.scan_due(wanted, now);
        let fetched = service.fetch(&wanted, now).await;
        self.linear.publish(&service, &self.events);
        self.linear.publish_issues(fetched, &self.events);
    }

    /// After the key or the switch changed, or a Test passed: fetch the context at once and
    /// scan every session again; nothing while off or with no usable key.
    async fn refresh_all(&self) {
        {
            let mut service = self.linear.service.lock().await;
            self.linear
                .refresh_context(&mut service, &self.events, true)
                .await;
            if !service.usable() {
                return;
            }
        }
        self.scan(&self.sessions.snapshots()).await;
    }

    /// One 5-minute pass, nothing while the switch is off: choose the key
    /// again (the store, then the variable) while none is in use, refresh
    /// the team keys when due (scanning every session again when they
    /// changed), then fetch, in one call, every identifier a live session
    /// names whose copy is missing or older than a minute.
    async fn tick(&self) {
        let changed = {
            let mut service = self.linear.service.lock().await;
            if !service.enabled {
                return;
            }
            if !service.client.key_configured()
                && let Some(chosen) = choose_key(self.linear.key_source).await
            {
                service.use_key(chosen);
            }
            // Forced, so every tick fetches the team keys even when a Test
            // or a key change fetched them less than a tick ago.
            self.linear
                .refresh_context(&mut service, &self.events, true)
                .await
        };
        if changed {
            self.scan(&self.sessions.snapshots()).await;
        }
        let team_keys = self.linear.team_keys();
        let identifiers: Vec<String> = self
            .sessions
            .snapshots()
            .iter()
            .filter(|snapshot| is_live(snapshot))
            .flat_map(|snapshot| session_identifiers(snapshot, &team_keys))
            .collect();
        if identifiers.is_empty() {
            return;
        }
        let mut service = self.linear.service.lock().await;
        let now = self.linear.clock.now();
        let wanted = service.wanted(&identifiers, now, |cache, id| cache.needs_fetch(id, now));
        let fetched = service.fetch(&wanted, now).await;
        self.linear.publish(&service, &self.events);
        self.linear.publish_issues(fetched, &self.events);
    }
}

/// Start the title scan for the daemon's life: it fetches the team keys when
/// due, scans every session, then scans each session update; a stream that
/// lagged scans every session again. It subscribes before it returns, so no
/// update after this call is missed.
pub(crate) fn spawn_title_scan(hub: &Hub) -> tokio::task::JoinHandle<()> {
    let wiring = Wiring::of(hub);
    let mut updates = hub.sessions.subscribe();
    tokio::spawn(async move {
        let changed = {
            let mut service = wiring.linear.service.lock().await;
            wiring
                .linear
                .refresh_context(&mut service, &wiring.events, false)
                .await
        };
        debug!(changed, "linear: title scan started");
        wiring.scan(&wiring.sessions.snapshots()).await;
        loop {
            match updates.recv().await {
                Ok(SessionEvent::Updated(snapshot, _)) => {
                    wiring.scan(std::slice::from_ref(&*snapshot)).await;
                }
                Ok(_) => {}
                Err(RecvError::Lagged(skipped)) => {
                    debug!(
                        skipped,
                        "linear: the title scan lagged; scanning every session"
                    );
                    wiring.scan(&wiring.sessions.snapshots()).await;
                }
                Err(RecvError::Closed) => return,
            }
        }
    })
}

/// Start the 5-minute refresh for the daemon's life: it parks while no
/// client is connected, and passes every [`REFRESH_TICK`] while one is, the
/// first time one tick after the first client came.
pub(crate) fn spawn_refresh(
    hub: &Hub,
    mut clients: watch::Receiver<usize>,
) -> tokio::task::JoinHandle<()> {
    let wiring = Wiring::of(hub);
    tokio::spawn(async move {
        loop {
            if clients.wait_for(|count| *count > 0).await.is_err() {
                return;
            }
            let start = tokio::time::Instant::now() + REFRESH_TICK;
            let mut interval = tokio::time::interval_at(start, REFRESH_TICK);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = interval.tick() => wiring.tick().await,
                    changed = clients.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        if *clients.borrow_and_update() == 0 {
                            break;
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use std::cell::Cell;

    use serde_json::json;

    use super::*;
    use crate::linear::test_support::{Fake, batch_reply, context_reply, keyed};
    use crate::server::connection::AuthKind;
    use crate::server::test_support::{
        HANG_GUARD, ScratchDir, dispatch_on, hello, idle_record, mem_session, next_message,
        test_hub,
    };

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// A test hub whose Linear client posts to a fake with a key, the team
    /// keys (`RT`, at `acme`) already fetched; the fake, for more replies.
    async fn keyed_hub(tag: &str) -> (Hub, ScratchDir, Fake) {
        let (mut hub, scratch) = test_hub(tag);
        let fake = Fake::with(vec![Ok(context_reply(&["RT"], "acme"))]);
        let mut client = keyed(&fake);
        client
            .refresh_context(Utc::now())
            .await
            .expect("the context parses");
        hub.linear = LinearHub::with_client(client);
        (hub, scratch, fake)
    }

    /// The identifiers each issue request asked for, sorted, in request
    /// order; the context requests are left out.
    fn asked(fake: &Fake) -> Vec<Vec<String>> {
        fake.requests()
            .iter()
            .filter(|request| {
                !request["query"]
                    .as_str()
                    .is_some_and(|query| query.starts_with("query Context"))
            })
            .map(|request| {
                let mut asked: Vec<String> = request["variables"]
                    .as_object()
                    .expect("the request carries variables")
                    .values()
                    .map(|value| value.as_str().expect("a string").to_owned())
                    .collect();
                asked.sort();
                asked
            })
            .collect()
    }

    fn identifiers_of(issues: &[LinearIssue]) -> Vec<String> {
        let mut identifiers: Vec<String> = issues
            .iter()
            .map(|issue| issue.identifier.clone())
            .collect();
        identifiers.sort();
        identifiers
    }

    /// The next `LinearIssues` broadcast, skipping every other event.
    async fn next_broadcast(events: &mut broadcast::Receiver<StateEvent>) -> Vec<LinearIssue> {
        tokio::time::timeout(HANG_GUARD, async {
            loop {
                if let StateEvent::LinearIssues(issues) =
                    events.recv().await.expect("the state stream is open")
                {
                    return issues;
                }
            }
        })
        .await
        .expect("hung: no LinearIssues broadcast")
    }

    /// The issues of a `LinearIssues` reply, failing on anything else.
    fn issues_of(reply: DaemonMessage) -> Vec<LinearIssue> {
        assert!(
            matches!(reply, DaemonMessage::LinearIssues { .. }),
            "expected LinearIssues, got {reply:?}"
        );
        match reply {
            DaemonMessage::LinearIssues { issues } => issues,
            _ => Vec::new(),
        }
    }

    fn get(names: &[&str]) -> ClientMessage {
        ClientMessage::GetLinearIssues {
            identifiers: ids(names),
            request_id: Some("l1".to_owned()),
        }
    }

    /// The messages a connection is sent after its `Hello`, up to and
    /// including the first one `stop` picks.
    async fn after_hello(hub: &Hub, stop: fn(&DaemonMessage) -> bool) -> Vec<DaemonMessage> {
        let (mut client, _session) = mem_session(hub, AuthKind::Local);
        client.send(&hello(&hub.auth_token, None));
        tokio::time::timeout(HANG_GUARD, async {
            let mut seen = Vec::new();
            loop {
                let message = client.next(true).await.expect("the connection is open");
                let last = stop(&message);
                seen.push(message);
                if last {
                    return seen;
                }
            }
        })
        .await
        .expect("hung: the message never came")
    }

    #[tokio::test]
    async fn welcome_lists_linear_links() {
        let (hub, _scratch) = test_hub("linear-welcome");
        let seen = after_hello(&hub, |message| {
            matches!(message, DaemonMessage::Welcome { .. })
        })
        .await;
        assert!(
            matches!(
                &seen[0],
                DaemonMessage::Welcome { features, .. }
                    if features.iter().any(|f| f == protocol::FEATURE_LINEAR_LINKS)
            ),
            "{seen:?}"
        );
    }

    #[tokio::test]
    async fn linear_context_follows_welcome_with_no_key_configured() {
        let (hub, _scratch) = test_hub("linear-context-no-key");
        let seen = after_hello(&hub, |message| {
            matches!(message, DaemonMessage::LinearContext { .. })
        })
        .await;
        assert!(
            matches!(&seen[0], DaemonMessage::Welcome { .. }),
            "{seen:?}"
        );
        assert!(
            matches!(
                seen.last(),
                Some(DaemonMessage::LinearContext { team_keys, workspace: None, key_configured: false })
                    if team_keys.is_empty()
            ),
            "{seen:?}"
        );
    }

    #[tokio::test]
    async fn a_session_title_naming_an_issue_fetches_it_once_and_broadcasts_it() {
        let (hub, _scratch, fake) = keyed_hub("linear-title-scan").await;
        fake.push(Ok(batch_reply(&ids(&["RT-1"]))));
        let mut events = hub.state_events.subscribe();
        let scan = spawn_title_scan(&hub);

        let mut record = idle_record("s1");
        record.terminal_title = Some("RT-1 fix the thing".to_owned());
        hub.sessions.insert(record);
        let issues = next_broadcast(&mut events).await;
        assert_eq!(identifiers_of(&issues), ids(&["RT-1"]));

        // The same issue named again asks nothing; a new one is fetched.
        hub.sessions.update("s1", |record| {
            record.terminal_title = Some("RT-1 still".to_owned());
        });
        fake.push(Ok(batch_reply(&ids(&["RT-2"]))));
        let mut other = idle_record("s2");
        other.user_label = Some("RT-2".to_owned());
        hub.sessions.insert(other);
        let issues = next_broadcast(&mut events).await;
        assert_eq!(identifiers_of(&issues), ids(&["RT-2"]));
        assert_eq!(asked(&fake), [ids(&["RT-1"]), ids(&["RT-2"])]);
        scan.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_batch_is_not_retried_by_the_title_scan() {
        let (hub, _scratch, fake) = keyed_hub("linear-scan-failed").await;
        fake.push(Err(LinearError::Status(500)));
        let mut events = hub.state_events.subscribe();
        let scan = spawn_title_scan(&hub);
        // Let the scan make its start-up pass over the empty registry, so
        // every session below reaches it as an update, in order.
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }

        let mut record = idle_record("s1");
        record.terminal_title = Some("RT-200 working".to_owned());
        hub.sessions.insert(record);
        for frame in ["⠋", "⠙", "⠹", "⠸", "⠼"].iter().cycle().take(20) {
            hub.sessions.update("s1", |record| {
                record.terminal_title = Some(format!("{frame} RT-200 working"));
            });
        }
        // A sentinel in an exited session: once it is broadcast, the scan has
        // seen every update before it, and the refresh leaves it out.
        fake.push(Ok(batch_reply(&ids(&["RT-201"]))));
        let mut sentinel = idle_record("s2");
        sentinel.terminal_title = Some("RT-201".to_owned());
        sentinel.status = SessionStatus::Stopped;
        hub.sessions.insert(sentinel);
        let issues = next_broadcast(&mut events).await;
        assert_eq!(identifiers_of(&issues), ids(&["RT-201"]));
        assert_eq!(
            asked(&fake),
            [ids(&["RT-200"]), ids(&["RT-201"])],
            "the failed identifier is asked once, however often the title changes"
        );

        // The tick fetches the team keys first, then the issues.
        fake.push(Ok(context_reply(&["RT"], "acme")));
        fake.push(Ok(batch_reply(&ids(&["RT-200"]))));
        let (_clients_tx, clients_rx) = watch::channel(1usize);
        let refresh = spawn_refresh(&hub, clients_rx);
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(REFRESH_TICK).await;
        let issues = next_broadcast(&mut events).await;
        assert_eq!(identifiers_of(&issues), ids(&["RT-200"]));
        assert_eq!(
            asked(&fake),
            [ids(&["RT-200"]), ids(&["RT-201"]), ids(&["RT-200"])],
            "the refresh asks the failed identifier again, once"
        );
        scan.abort();
        refresh.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_hover_on_a_copy_younger_than_a_minute_fetches_nothing_and_answers_the_requester() {
        let (hub, _scratch, fake) = keyed_hub("linear-hover-young").await;
        fake.push(Ok(batch_reply(&ids(&["RT-1"]))));
        let mut events = hub.state_events.subscribe();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();

        dispatch_on(&hub, get(&["RT-1"]), 1, &out_tx).await;
        let fetched = next_broadcast(&mut events).await;
        assert_eq!(identifiers_of(&fetched), ids(&["RT-1"]));
        assert!(
            out_rx.try_recv().is_err(),
            "a fetch is answered by the broadcast alone"
        );

        tokio::time::advance(Duration::from_secs(59)).await;
        dispatch_on(&hub, get(&["RT-1"]), 1, &out_tx).await;
        let cached = issues_of(next_message(&mut out_rx).await);
        assert_eq!(cached, fetched, "the requester gets the cached copy");
        assert_eq!(asked(&fake), [ids(&["RT-1"])], "nothing is fetched again");
        assert!(events.try_recv().is_err(), "nothing is broadcast");
    }

    #[tokio::test(start_paused = true)]
    async fn a_hover_on_a_copy_older_than_a_minute_fetches_it_again() {
        let (hub, _scratch, fake) = keyed_hub("linear-hover-old").await;
        fake.push(Ok(batch_reply(&ids(&["RT-1"]))));
        fake.push(Ok(batch_reply(&ids(&["RT-1"]))));
        let mut events = hub.state_events.subscribe();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();

        dispatch_on(&hub, get(&["RT-1"]), 1, &out_tx).await;
        let first = next_broadcast(&mut events).await;
        tokio::time::advance(Duration::from_secs(61)).await;
        dispatch_on(&hub, get(&["RT-1"]), 1, &out_tx).await;
        let second = next_broadcast(&mut events).await;

        assert_eq!(asked(&fake), [ids(&["RT-1"]), ids(&["RT-1"])]);
        assert!(
            second[0].fetched_at > first[0].fetched_at,
            "{first:?} {second:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_identifier_linear_answered_without_is_not_asked_again_for_ten_minutes() {
        let (hub, _scratch, fake) = keyed_hub("linear-unknown").await;
        fake.push(Ok(json!({ "data": { "i0": null } })));
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();

        dispatch_on(&hub, get(&["RT-404"]), 1, &out_tx).await;
        assert_eq!(issues_of(next_message(&mut out_rx).await), []);
        tokio::time::advance(Duration::from_mins(9)).await;
        dispatch_on(&hub, get(&["RT-404"]), 1, &out_tx).await;
        assert_eq!(issues_of(next_message(&mut out_rx).await), []);
        assert_eq!(
            asked(&fake),
            [ids(&["RT-404"])],
            "not asked within 10 minutes"
        );

        tokio::time::advance(Duration::from_mins(2)).await;
        fake.push(Ok(json!({ "data": { "i0": null } })));
        dispatch_on(&hub, get(&["RT-404"]), 1, &out_tx).await;
        assert_eq!(issues_of(next_message(&mut out_rx).await), []);
        assert_eq!(asked(&fake).len(), 2, "asked again after 10 minutes");
    }

    #[tokio::test(start_paused = true)]
    async fn an_identifier_in_a_failed_batch_is_asked_again_on_the_next_request() {
        let (hub, _scratch, fake) = keyed_hub("linear-failed").await;
        fake.push(Err(LinearError::Status(500)));
        let mut events = hub.state_events.subscribe();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();

        dispatch_on(&hub, get(&["RT-1"]), 1, &out_tx).await;
        assert_eq!(
            issues_of(next_message(&mut out_rx).await),
            [],
            "a failed fetch with no copy answers the requester with none"
        );

        fake.push(Ok(batch_reply(&ids(&["RT-1"]))));
        dispatch_on(&hub, get(&["RT-1"]), 1, &out_tx).await;
        let issues = next_broadcast(&mut events).await;
        assert_eq!(identifiers_of(&issues), ids(&["RT-1"]));
        assert_eq!(asked(&fake), [ids(&["RT-1"]), ids(&["RT-1"])]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_refresh_asks_every_live_sessions_identifiers_in_one_batch() {
        let (hub, _scratch, fake) = keyed_hub("linear-refresh").await;
        let mut first = idle_record("s1");
        first.terminal_title = Some("RT-1 fix".to_owned());
        hub.sessions.insert(first);
        let mut second = idle_record("s2");
        second.user_label = Some("RT-2 and https://linear.app/acme/issue/RT-1".to_owned());
        hub.sessions.insert(second);
        let mut exited = idle_record("s3");
        exited.terminal_title = Some("RT-3".to_owned());
        exited.status = SessionStatus::Stopped;
        hub.sessions.insert(exited);
        // The tick fetches the team keys first, then the issues.
        fake.push(Ok(context_reply(&["RT"], "acme")));
        fake.push(Ok(batch_reply(&ids(&["RT-1", "RT-2"]))));
        let mut events = hub.state_events.subscribe();
        let (_clients_tx, clients_rx) = watch::channel(1usize);

        let refresh = spawn_refresh(&hub, clients_rx);
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(299)).await;
        assert_eq!(
            asked(&fake),
            Vec::<Vec<String>>::new(),
            "nothing before the tick"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        let issues = next_broadcast(&mut events).await;

        assert_eq!(identifiers_of(&issues), ids(&["RT-1", "RT-2"]));
        assert_eq!(
            asked(&fake),
            [ids(&["RT-1", "RT-2"])],
            "one batch, the exited session left out"
        );
        refresh.abort();
    }

    /// Save and apply the switch at `enabled`.
    async fn switch(hub: &Hub, enabled: bool) {
        hub.linear
            .set_enabled(&hub.state, enabled, &hub.state_events)
            .await
            .expect("the switch saves");
    }

    /// The status the clients were last sent.
    fn published_status(hub: &Hub) -> WireStatus {
        crate::sync::lock(&hub.linear.published).status.clone()
    }

    /// The context the clients were last sent.
    fn published_context(hub: &Hub) -> WireContext {
        crate::sync::lock(&hub.linear.published).context.clone()
    }

    /// Answer `message` from a local connection and wait for the reply.
    async fn reply_to(hub: &Hub, message: ClientMessage) -> DaemonMessage {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        dispatch_on(hub, message, 1, &out_tx).await;
        next_message(&mut out_rx).await
    }

    fn test_linear() -> ClientMessage {
        ClientMessage::TestLinear {
            request_id: Some("t1".to_owned()),
        }
    }

    #[tokio::test]
    async fn welcome_lists_linear_settings_and_the_initial_state_carries_the_status() {
        let (hub, _scratch) = test_hub("linear-settings-welcome");
        let seen = after_hello(&hub, |message| {
            matches!(message, DaemonMessage::LinearStatus { .. })
        })
        .await;
        assert!(
            matches!(
                &seen[0],
                DaemonMessage::Welcome { features, .. }
                    if features.iter().any(|f| f == protocol::FEATURE_LINEAR_SETTINGS)
            ),
            "{seen:?}"
        );
        assert!(
            matches!(
                seen.last(),
                Some(DaemonMessage::LinearStatus {
                    enabled: true,
                    key: LinearKeyState::Missing,
                    workspace_name: None,
                    last_error: None,
                    ..
                })
            ),
            "{seen:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn while_off_no_path_makes_a_request_and_the_context_is_empty() {
        let (hub, _scratch, fake) = keyed_hub("linear-off").await;
        let mut record = idle_record("s1");
        record.terminal_title = Some("RT-1 fix".to_owned());
        hub.sessions.insert(record);
        switch(&hub, false).await;

        assert_eq!(
            hub.linear.request(&ids(&["RT-1"]), &hub.state_events).await,
            None,
            "a request while off is not answered"
        );
        let wiring = Wiring::of(&hub);
        wiring.scan(&hub.sessions.snapshots()).await;
        wiring.tick().await;
        {
            let mut service = hub.linear.service.lock().await;
            hub.linear
                .refresh_context(&mut service, &hub.state_events, true)
                .await;
        }
        wiring.refresh_all().await;

        assert_eq!(fake.requests().len(), 1, "only the setup's context fetch");
        assert_eq!(published_context(&hub), WireContext::default());
        assert!(!published_status(&hub).enabled);
    }

    #[tokio::test(start_paused = true)]
    async fn turning_linear_on_refreshes_the_context() {
        let (hub, _scratch, fake) = keyed_hub("linear-on-again").await;
        switch(&hub, false).await;
        fake.push(Ok(context_reply(&["RT", "OPS"], "acme")));

        switch(&hub, true).await;
        Wiring::of(&hub).refresh_all().await;

        assert_eq!(fake.requests().len(), 2, "the context is fetched again");
        let context = published_context(&hub);
        assert_eq!(context.team_keys, ids(&["RT", "OPS"]));
        assert!(context.key_configured);
        let status = published_status(&hub);
        assert!(status.enabled);
        assert_eq!(status.team_keys, ids(&["RT", "OPS"]));
        assert_eq!(
            status.workspace_name.as_deref(),
            Some(crate::linear::test_support::workspace_name("acme").as_str())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_rejected_key_makes_no_further_request_until_the_key_changes() {
        let (hub, _scratch, fake) = keyed_hub("linear-rejected").await;
        fake.push(Err(LinearError::Rejected));

        hub.linear.request(&ids(&["RT-1"]), &hub.state_events).await;
        let status = published_status(&hub);
        assert_eq!(status.key, LinearKeyState::Rejected);
        assert_eq!(
            status.last_error.as_deref(),
            Some("Linear refused the API key")
        );

        tokio::time::advance(Duration::from_secs(120)).await;
        hub.linear.request(&ids(&["RT-2"]), &hub.state_events).await;
        Wiring::of(&hub).tick().await;
        assert_eq!(asked(&fake), [ids(&["RT-1"])], "nothing more is asked");

        let changed = (
            Some(Secret::new("lin_api_other".to_owned())),
            LinearKeyState::Stored,
        );
        let mut service = hub.linear.service.lock().await;
        service.use_key(changed);
        assert!(service.usable(), "a different key may be used again");
        assert_eq!(service.key_state(), LinearKeyState::Stored);
    }

    #[tokio::test(start_paused = true)]
    async fn a_rejected_key_says_whether_it_is_saved() {
        for (chosen, saved) in [
            (LinearKeyState::Environment, false),
            (LinearKeyState::Stored, true),
        ] {
            let (hub, _scratch, fake) = keyed_hub("linear-rejected-source").await;
            hub.linear.service.lock().await.chosen = chosen;
            fake.push(Err(LinearError::Rejected));

            hub.linear.request(&ids(&["RT-1"]), &hub.state_events).await;
            let status = published_status(&hub);
            assert_eq!(status.key, LinearKeyState::Rejected, "{chosen:?}");
            assert_eq!(status.key_saved, saved, "{chosen:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_fetch_sets_the_last_error_and_the_next_success_clears_it() {
        let (hub, _scratch, fake) = keyed_hub("linear-last-error").await;
        fake.push(Err(LinearError::Status(500)));
        fake.push(Ok(batch_reply(&ids(&["RT-1"]))));

        hub.linear.request(&ids(&["RT-1"]), &hub.state_events).await;
        let status = published_status(&hub);
        assert_eq!(
            status.last_error.as_deref(),
            Some("Linear answered HTTP 500")
        );
        assert_eq!(status.key, LinearKeyState::Stored, "a 500 is not a refusal");

        hub.linear.request(&ids(&["RT-1"]), &hub.state_events).await;
        assert_eq!(published_status(&hub).last_error, None);
    }

    #[tokio::test(start_paused = true)]
    async fn get_linear_issues_while_off_gets_no_reply() {
        let (hub, _scratch, fake) = keyed_hub("linear-off-get").await;
        switch(&hub, false).await;
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();

        dispatch_on(&hub, get(&["RT-1"]), 1, &out_tx).await;
        tokio::time::sleep(Duration::from_secs(5)).await;

        assert!(out_rx.try_recv().is_err(), "nothing is answered while off");
        assert_eq!(fake.requests().len(), 1, "only the setup's context fetch");
    }

    #[tokio::test]
    async fn the_test_with_no_key_says_so() {
        let (hub, _scratch) = test_hub("linear-test-no-key");
        let reply = reply_to(&hub, test_linear()).await;
        assert!(
            matches!(
                &reply,
                DaemonMessage::LinearTest { workspace_name: None, error: Some(error), request_id: Some(id), .. }
                    if error == "No Linear API key is set." && id == "t1"
            ),
            "{reply:?}"
        );
    }

    #[tokio::test]
    async fn the_test_reports_the_workspace_and_team_keys() {
        let (hub, _scratch, fake) = keyed_hub("linear-test-ok").await;
        switch(&hub, false).await;
        fake.push(Ok(context_reply(&["RT", "OPS"], "acme")));

        let reply = reply_to(&hub, test_linear()).await;
        let expected = crate::linear::test_support::workspace_name("acme");
        assert!(
            matches!(
                &reply,
                DaemonMessage::LinearTest { workspace_name: Some(name), team_keys, error: None, request_id: Some(id) }
                    if *name == expected && *team_keys == ids(&["RT", "OPS"]) && id == "t1"
            ),
            "{reply:?}"
        );
        let requests = fake.requests();
        let query = requests[1]["query"].as_str().expect("a query string");
        assert_eq!(
            query, "query Test { teams { nodes { key } } organization { name urlKey } }",
            "the Test runs whatever the switch, bypassing the cached context"
        );
    }

    /// A session whose terminal title names `CITY-1`, a team the setup's
    /// context does not know.
    fn naming_a_new_team(hub: &Hub) {
        let mut record = idle_record("s1");
        record.terminal_title = Some("CITY-1 plan the route".to_owned());
        hub.sessions.insert(record);
    }

    #[tokio::test(start_paused = true)]
    async fn a_tick_picks_up_a_team_added_since_the_last_fetch() {
        let (hub, _scratch, fake) = keyed_hub("linear-tick-new-team").await;
        naming_a_new_team(&hub);
        fake.push(Ok(context_reply(&["RT", "CITY"], "acme")));
        fake.push(Ok(batch_reply(&ids(&["CITY-1"]))));
        let mut events = hub.state_events.subscribe();

        tokio::time::advance(REFRESH_TICK).await;
        Wiring::of(&hub).tick().await;

        let issues = next_broadcast(&mut events).await;
        assert_eq!(identifiers_of(&issues), ids(&["CITY-1"]));
        assert_eq!(hub.linear.team_keys(), ids(&["RT", "CITY"]));
        assert_eq!(published_status(&hub).team_keys, ids(&["RT", "CITY"]));
        assert_eq!(asked(&fake), [ids(&["CITY-1"])], "fetched once");
    }

    #[tokio::test(start_paused = true)]
    async fn a_successful_test_picks_up_a_team_added_since_the_last_fetch() {
        let (hub, _scratch, fake) = keyed_hub("linear-test-new-team").await;
        naming_a_new_team(&hub);
        tokio::time::advance(Duration::from_mins(1)).await;
        fake.push(Ok(context_reply(&["RT", "CITY"], "acme")));
        fake.push(Ok(context_reply(&["RT", "CITY"], "acme")));
        fake.push(Ok(batch_reply(&ids(&["CITY-1"]))));
        let mut events = hub.state_events.subscribe();

        let reply = reply_to(&hub, test_linear()).await;
        assert!(
            matches!(
                &reply,
                DaemonMessage::LinearTest { team_keys, error: None, request_id: Some(id), .. }
                    if *team_keys == ids(&["RT", "CITY"]) && id == "t1"
            ),
            "{reply:?}"
        );
        let issues = next_broadcast(&mut events).await;

        assert_eq!(identifiers_of(&issues), ids(&["CITY-1"]));
        assert_eq!(hub.linear.team_keys(), ids(&["RT", "CITY"]));
        let queries: Vec<String> = fake
            .requests()
            .iter()
            .map(|request| request["query"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert!(queries[1].starts_with("query Test"), "{queries:?}");
        assert!(queries[2].starts_with("query Context"), "{queries:?}");
        assert_eq!(
            queries.len(),
            4,
            "setup, Test, refresh, CITY-1: {queries:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_test_refreshes_nothing() {
        let (hub, _scratch, fake) = keyed_hub("linear-test-failed-no-refresh").await;
        naming_a_new_team(&hub);
        fake.push(Err(LinearError::Status(500)));

        let reply = reply_to(&hub, test_linear()).await;
        assert!(
            matches!(&reply, DaemonMessage::LinearTest { error: Some(_), .. }),
            "{reply:?}"
        );
        tokio::time::sleep(Duration::from_secs(5)).await;

        assert_eq!(fake.requests().len(), 2, "only the setup and the Test");
        assert_eq!(hub.linear.team_keys(), ids(&["RT"]));
    }

    #[tokio::test]
    async fn a_refused_test_reports_the_error_and_the_key_reads_rejected() {
        let (hub, _scratch, fake) = keyed_hub("linear-test-rejected").await;
        fake.push(Err(LinearError::Rejected));
        let mut events = hub.state_events.subscribe();

        let reply = reply_to(&hub, test_linear()).await;
        assert!(
            matches!(
                &reply,
                DaemonMessage::LinearTest { workspace_name: None, error: Some(error), .. }
                    if error == "Linear refused the API key"
            ),
            "{reply:?}"
        );
        assert_eq!(published_status(&hub).key, LinearKeyState::Rejected);
        let broadcast = tokio::time::timeout(HANG_GUARD, async {
            loop {
                if let StateEvent::LinearStatus(message) =
                    events.recv().await.expect("the state stream is open")
                {
                    return message;
                }
            }
        })
        .await
        .expect("hung: no LinearStatus broadcast");
        assert!(
            matches!(
                *broadcast,
                DaemonMessage::LinearStatus {
                    key: LinearKeyState::Rejected,
                    ..
                }
            ),
            "{broadcast:?}"
        );
    }

    #[tokio::test]
    async fn a_blank_key_is_refused_with_the_request_id_and_linear_refused() {
        let (hub, _scratch) = test_hub("linear-blank-key");
        let reply = reply_to(
            &hub,
            ClientMessage::SetLinearKey {
                key: Some(protocol::Secret("   ".to_owned())),
                request_id: Some("k1".to_owned()),
            },
        )
        .await;
        assert!(
            matches!(
                &reply,
                DaemonMessage::Error { request_id: Some(id), code: Some(code), .. }
                    if id == "k1" && code == protocol::error_codes::LINEAR_REFUSED
            ),
            "{reply:?}"
        );
    }

    /// A key the fakes below accept besides [`crate::linear::test_support::KEY`].
    const NEW_KEY: &str = "lin_api_new";

    /// The key source of a hub that reads the (mock) secret store alone.
    fn stored_key() -> (Option<Secret>, LinearKeyState) {
        crate::linear_key::choose(false, crate::linear_key::read_stored(), None)
    }

    /// [`keyed_hub`], but choosing its key from the secret store after a
    /// key change, and accepting [`NEW_KEY`].
    async fn stored_hub(tag: &str) -> (Hub, ScratchDir, Fake) {
        let (mut hub, scratch) = test_hub(tag);
        let fake = Fake::with(vec![Ok(context_reply(&["RT"], "acme"))]);
        fake.accept(NEW_KEY);
        let mut client = keyed(&fake);
        client
            .refresh_context(Utc::now())
            .await
            .expect("the context parses");
        let service = Service::new(client, LinearKeyState::Stored, true);
        hub.linear = LinearHub::new(service, Clock::tokio(), stored_key, false);
        (hub, scratch, fake)
    }

    fn set_key(key: &str, request_id: &str) -> ClientMessage {
        ClientMessage::SetLinearKey {
            key: Some(protocol::Secret(key.to_owned())),
            request_id: Some(request_id.to_owned()),
        }
    }

    /// The next message on `out_rx` that is a `LinearStatus`.
    async fn next_status(out_rx: &mut mpsc::UnboundedReceiver<DaemonMessage>) -> DaemonMessage {
        loop {
            let message = next_message(out_rx).await;
            if matches!(message, DaemonMessage::LinearStatus { .. }) {
                return message;
            }
        }
    }

    #[tokio::test]
    async fn turning_linear_on_while_on_still_echoes_the_status_to_the_requester() {
        let (hub, _scratch) = test_hub("linear-echo-enabled");
        let reply = reply_to(
            &hub,
            ClientMessage::SetLinearEnabled {
                enabled: true,
                request_id: Some("e1".to_owned()),
            },
        )
        .await;
        assert!(
            matches!(reply, DaemonMessage::LinearStatus { enabled: true, .. }),
            "{reply:?}"
        );
    }

    #[tokio::test]
    async fn saving_a_key_while_off_echoes_the_status_to_the_requester() {
        let (_env, _store) =
            crate::env_secrets::test_support::scratch_async("linear-echo-key").await;
        let (hub, _scratch, fake) = stored_hub("linear-echo-key").await;
        crate::linear_key::set(&Secret::new(crate::linear::test_support::KEY.to_owned()))
            .expect("the first key saves");
        let off = reply_to(
            &hub,
            ClientMessage::SetLinearEnabled {
                enabled: false,
                request_id: Some("e1".to_owned()),
            },
        )
        .await;
        assert!(
            matches!(off, DaemonMessage::LinearStatus { enabled: false, .. }),
            "{off:?}"
        );

        let reply = reply_to(&hub, set_key(NEW_KEY, "k1")).await;
        assert!(
            matches!(
                reply,
                DaemonMessage::LinearStatus {
                    enabled: false,
                    key: LinearKeyState::Stored,
                    ..
                }
            ),
            "{reply:?}"
        );
        assert_eq!(fake.requests().len(), 1, "nothing is asked while off");
    }

    #[tokio::test]
    async fn a_new_key_rescans_what_the_refused_key_failed_on() {
        let (_env, _store) = crate::env_secrets::test_support::scratch_async("linear-rescan").await;
        let (hub, _scratch, fake) = stored_hub("linear-rescan").await;
        let mut record = idle_record("s1");
        record.terminal_title = Some("RT-5 fix".to_owned());
        hub.sessions.insert(record);
        fake.push(Err(LinearError::Rejected));
        Wiring::of(&hub).scan(&hub.sessions.snapshots()).await;
        assert_eq!(asked(&fake), [ids(&["RT-5"])]);
        assert_eq!(published_status(&hub).key, LinearKeyState::Rejected);

        fake.push(Ok(context_reply(&["RT"], "acme")));
        fake.push(Ok(batch_reply(&ids(&["RT-5"]))));
        let mut events = hub.state_events.subscribe();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        dispatch_on(&hub, set_key(NEW_KEY, "k1"), 1, &out_tx).await;
        let issues = next_broadcast(&mut events).await;

        assert_eq!(identifiers_of(&issues), ids(&["RT-5"]));
        assert_eq!(asked(&fake), [ids(&["RT-5"]), ids(&["RT-5"])]);
        assert_eq!(fake.keys().last().map(String::as_str), Some(NEW_KEY));
    }

    #[tokio::test(start_paused = true)]
    async fn a_key_change_asks_again_for_an_identifier_linear_answered_without() {
        let (hub, _scratch, fake) = keyed_hub("linear-unknown-key-change").await;
        fake.accept(NEW_KEY);
        fake.push(Ok(json!({ "data": { "i0": null } })));
        hub.linear
            .request(&ids(&["RT-404"]), &hub.state_events)
            .await;
        assert_eq!(asked(&fake), [ids(&["RT-404"])]);

        hub.linear.service.lock().await.use_key((
            Some(Secret::new(NEW_KEY.to_owned())),
            LinearKeyState::Stored,
        ));
        fake.push(Ok(json!({ "data": { "i0": null } })));
        hub.linear
            .request(&ids(&["RT-404"]), &hub.state_events)
            .await;
        assert_eq!(
            asked(&fake),
            [ids(&["RT-404"]), ids(&["RT-404"])],
            "a new key asks again"
        );
    }

    #[tokio::test]
    async fn two_saved_keys_leave_the_store_and_the_key_in_use_agreeing() {
        let (_env, _store) =
            crate::env_secrets::test_support::scratch_async("linear-two-keys").await;
        let (hub, _scratch, fake) = stored_hub("linear-two-keys").await;
        fake.push(Ok(context_reply(&["RT"], "acme")));
        fake.push(Ok(context_reply(&["RT"], "acme")));
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        dispatch_on(
            &hub,
            set_key(crate::linear::test_support::KEY, "k1"),
            1,
            &out_tx,
        )
        .await;
        dispatch_on(&hub, set_key(NEW_KEY, "k2"), 1, &out_tx).await;
        let _ = next_status(&mut out_rx).await;
        let _ = next_status(&mut out_rx).await;

        let stored = crate::linear_key::read_stored()
            .expect("the store is within reach")
            .map(|key| key.expose().to_owned());
        let service = hub.linear.service.lock().await;
        let in_use = service.client.key().map(|key| key.expose().to_owned());
        assert!(stored.is_some(), "a key is saved");
        assert_eq!(stored, in_use, "the saved key is the key in use");
    }

    #[test]
    fn the_offline_switch_resolves_no_key() {
        let read = Cell::new(false);
        let key = crate::linear::resolve_key(Some("1"), || {
            read.set(true);
            Some(Secret::new("lin_api_owner".to_owned()))
        });
        assert!(key.is_none(), "offline resolves no key");
        assert!(!read.get(), "offline reads no environment at all");

        for value in [None, Some(""), Some("0"), Some("true"), Some(" 1")] {
            let key =
                crate::linear::resolve_key(value, || Some(Secret::new("lin_api_owner".to_owned())));
            assert!(key.is_some(), "{value:?} leaves the key as found");
        }
    }
}
