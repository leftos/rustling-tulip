//! The daemon's Linear wiring: the one Linear client every connection and the
//! Dashboard share, the title scan that fetches the issues session titles and
//! labels name, the 5-minute refresh of the issues live sessions name, the
//! answer to [`ClientMessage::GetLinearIssues`] and the `LinearContext` and
//! `LinearIssues` broadcasts.
//!
//! The client, its cache and the identifiers Linear did not know are read and
//! written only under [`LinearHub`]'s one async lock. What the clients were
//! last sent (the context and every issue) is mirrored under a plain lock,
//! written while the async lock is held and broadcast under the mirror's lock,
//! so a connection's initial state, read under that lock, never lands after a
//! newer broadcast.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use futures::future::BoxFuture;
use protocol::{ClientMessage, DaemonMessage, LinearIssue, SessionSnapshot, SessionStatus};
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc, watch};
use tracing::{debug, error, info, warn};

use super::connection::ConnCtx;
use super::{Hub, StateEvent};
use crate::linear::{LinearCache, LinearClient, LinearError, LinearTransport, identifiers_in};
use crate::session::{SessionEvent, SessionRegistry};
use crate::user_env::Secret;

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
fn no_key() -> Option<Secret> {
    None
}

/// `LinearContext` as the clients were sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WireContext {
    team_keys: Vec<String>,
    workspace: Option<String>,
    key_configured: bool,
}

impl WireContext {
    fn of(client: &LinearClient) -> Self {
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

/// What the clients were last sent.
struct Published {
    context: WireContext,
    issues: HashMap<String, LinearIssue>,
}

/// The client and the identifiers Linear did not know, behind the one lock.
struct Service {
    client: LinearClient,
    /// Each identifier a batch Linear answered came back without, with when.
    unknown: HashMap<String, DateTime<Utc>>,
    /// Each identifier the title scan asked for in the last 5 minutes, with
    /// when; only the scan reads it.
    scan_tried: HashMap<String, DateTime<Utc>>,
}

impl Service {
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
    /// answered without; nothing is asked with no key or no identifier.
    async fn fetch(&mut self, identifiers: &[String], now: DateTime<Utc>) -> Vec<LinearIssue> {
        if identifiers.is_empty() || !self.client.key_configured() {
            return Vec::new();
        }
        let outcome = self.client.fetch_issues(identifiers, now).await;
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
    /// Where the 5-minute tick looks for a key while none is configured.
    key_source: fn() -> Option<Secret>,
}

impl LinearHub {
    fn new(client: LinearClient, clock: Clock, key_source: fn() -> Option<Secret>) -> Arc<Self> {
        let context = WireContext::of(&client);
        Arc::new(Self {
            service: AsyncMutex::new(Service {
                client,
                unknown: HashMap::new(),
                scan_tried: HashMap::new(),
            }),
            published: Mutex::new(Published {
                context,
                issues: HashMap::new(),
            }),
            clock,
            key_source,
        })
    }

    /// The daemon's hub: the key `LINEAR_API_KEY` holds, posting over HTTPS;
    /// with `RUSTLING_TULIP_LINEAR_OFFLINE` exactly `1`, no key is ever read.
    pub(crate) fn live() -> Arc<Self> {
        let offline = std::env::var(crate::linear::OFFLINE_VARIABLE).ok();
        if crate::linear::is_offline(offline.as_deref()) {
            info!(
                "linear offline: RUSTLING_TULIP_LINEAR_OFFLINE is 1, so this daemon reads no \
                 Linear key and never calls Linear"
            );
        }
        match LinearClient::from_env() {
            Ok(client) => Self::new(client, Clock::System, crate::linear::configured_key),
            Err(err) => {
                error!(
                    error = %format!("{err:#}"),
                    "linear: the HTTP client could not start; Linear stays off until the daemon restarts"
                );
                Self::new(
                    LinearClient::new(None, Box::new(NoTransport)),
                    Clock::System,
                    no_key,
                )
            }
        }
    }

    /// A hub over `client` that never looks for another key, on tokio's
    /// clock.
    #[cfg(test)]
    pub(crate) fn with_client(client: LinearClient) -> Arc<Self> {
        Self::new(client, Clock::tokio(), no_key)
    }

    /// A test hub's: no key, so nothing is ever posted.
    #[cfg(test)]
    pub(crate) fn without_key() -> Arc<Self> {
        Self::with_client(LinearClient::new(None, Box::new(NoTransport)))
    }

    /// Send `out_tx` the context and every issue the clients were last sent
    /// (none when there is none), under the lock every broadcast is sent
    /// under.
    pub(crate) fn send_current(&self, out_tx: &mpsc::UnboundedSender<DaemonMessage>) {
        let published = crate::sync::lock(&self.published);
        let _ = out_tx.send(published.context.message());
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

    /// Broadcast the client's context when it differs from the one last
    /// sent; whether it did. Called with the service lock held.
    fn publish_context(&self, service: &Service, events: &broadcast::Sender<StateEvent>) -> bool {
        let current = WireContext::of(&service.client);
        let mut published = crate::sync::lock(&self.published);
        if published.context == current {
            return false;
        }
        let _ = events.send(current.event());
        published.context = current;
        true
    }

    /// Fetch the team keys when they are due, keeping the last ones when the
    /// fetch fails, and broadcast the context on change; whether it changed.
    async fn refresh_context(
        &self,
        service: &mut Service,
        events: &broadcast::Sender<StateEvent>,
    ) -> bool {
        let now = self.clock.now();
        if service.client.key_configured()
            && service.client.context_due(now)
            && let Err(err) = service.client.refresh_context(now).await
        {
            warn!(error = %err, "linear: the team keys could not be fetched; the last ones stay");
        }
        self.publish_context(service, events)
    }

    /// Answer a request for `identifiers`: the copies missing or older than a
    /// minute are fetched. `None` when the fetch broadcast something, which
    /// answers the requester too; else the cached copies it named, possibly
    /// none, for the requester alone.
    pub(crate) async fn request(
        &self,
        identifiers: &[String],
        events: &broadcast::Sender<StateEvent>,
    ) -> Option<Vec<LinearIssue>> {
        let mut service = self.service.lock().await;
        let now = self.clock.now();
        let wanted = service.wanted(identifiers, now, |cache, id| cache.needs_fetch(id, now));
        let fetched = service.fetch(&wanted, now).await;
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

/// Answer [`ClientMessage::GetLinearIssues`] on a task of its own, so a slow
/// Linear never holds up the connection: a fetch is answered by the
/// `LinearIssues` broadcast, anything else by a `LinearIssues` to the
/// requester alone.
pub(super) fn handle(hub: &Hub, m: ClientMessage, ctx: &ConnCtx<'_>) {
    let ClientMessage::GetLinearIssues { identifiers, .. } = m else {
        return;
    };
    let linear = Arc::clone(&hub.linear);
    let events = hub.state_events.clone();
    let out_tx = ctx.out_tx.clone();
    tokio::spawn(async move {
        if let Some(issues) = linear.request(&identifiers, &events).await {
            let _ = out_tx.send(DaemonMessage::LinearIssues { issues });
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
        if !service.client.key_configured() {
            // Nothing is asked, so nothing is recorded as tried.
            return;
        }
        let now = self.linear.clock.now();
        let wanted = service.wanted(&identifiers, now, |cache, id| cache.get(id).is_none());
        let wanted = service.scan_due(wanted, now);
        let fetched = service.fetch(&wanted, now).await;
        self.linear.publish_issues(fetched, &self.events);
    }

    /// One 5-minute pass: look for a key while none is configured, refresh
    /// the team keys when due (scanning every session again when they
    /// changed), then fetch, in one call, every identifier a live session
    /// names whose copy is missing or older than a minute.
    async fn tick(&self) {
        let changed = {
            let mut service = self.linear.service.lock().await;
            if !service.client.key_configured() {
                service.client.set_key((self.linear.key_source)());
            }
            self.linear
                .refresh_context(&mut service, &self.events)
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
                .refresh_context(&mut service, &wiring.events)
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
