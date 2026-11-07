//! The push trigger: which session changes raise a push (Needs you,
//! Finished), the declarative payload each one carries, and the delivery
//! round that encrypts and posts it to every subscribed device and acts on
//! the push services' answers (`docs/plans/mobile-app.md`, "Push").
//!
//! A round takes `Hub::devices` then `Hub::push` to collect its targets,
//! releases both before any network call, and takes `Hub::push` alone
//! afterwards to delete a subscription its push service dropped.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use protocol::{RemoteState, SessionMode, SessionSnapshot, SessionStatus};
use reqwest::Url;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;

use super::send::{self, Outcome, PushPolicy};
use super::state::PushState;
use super::store::Subscription;
use super::vapid::{JwtCache, VapidKey};
use super::webpush::{self, Subscriber};
use crate::devices::DeviceStore;
use crate::session::{SessionEvent, SessionRegistry};
use crate::sync::lock;

/// The body of a push whose session has no name but its id.
const PRODUCT_NAME: &str = "Rustling Tulip";

/// The most characters of a session's name a push carries.
const LABEL_MOST: usize = 200;

/// The declarative Web Push magic number (`"web_push": 8030`).
const DECLARATIVE_WEB_PUSH: u16 = 8030;

/// The first back-off after a push service throttled, failed or could not be
/// reached; each further one doubles up to [`BACKOFF_MOST`].
const BACKOFF_FIRST: Duration = Duration::from_secs(30);

/// The longest back-off.
const BACKOFF_MOST: Duration = Duration::from_secs(600);

/// Failures in a row after which a subscription is deleted, counted only
/// while its push service accepts another subscription in the same round.
const FAILURES_TO_DELETE: u32 = 5;

/// What a session change raises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PushKind {
    /// The session started waiting for an answer.
    NeedsYou,
    /// The session's turn ended.
    Finished,
}

impl PushKind {
    /// The notification's title.
    fn title(self) -> &'static str {
        match self {
            Self::NeedsYou => "Needs you",
            Self::Finished => "Finished",
        }
    }

    /// Whether the device's switch for this kind is on.
    fn wanted_by(self, subscription: &Subscription) -> bool {
        match self {
            Self::NeedsYou => subscription.needs_you,
            Self::Finished => subscription.finished,
        }
    }
}

/// What the trigger last saw of a session.
#[derive(Debug, Clone, Copy)]
struct Prior {
    status: SessionStatus,
    background: bool,
}

impl Prior {
    fn of(snapshot: &SessionSnapshot) -> Self {
        Self {
            status: snapshot.status,
            background: snapshot.background,
        }
    }
}

/// Each session's last seen status, so a snapshot can be judged against the
/// one before it.
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    prior: HashMap<String, Prior>,
}

impl Tracker {
    /// Forget everything and remember `snapshots` as seen: nothing they
    /// show raises a push.
    pub(crate) fn seed<'a>(&mut self, snapshots: impl IntoIterator<Item = &'a SessionSnapshot>) {
        self.prior.clear();
        for snapshot in snapshots {
            self.prior.insert(snapshot.id.clone(), Prior::of(snapshot));
        }
    }

    /// Forget a removed session.
    pub(crate) fn forget(&mut self, session_id: &str) {
        self.prior.remove(session_id);
    }

    /// Remember `after` and say what it raises against the copy seen
    /// before it. A session seen for the first time raises nothing.
    pub(crate) fn observe(&mut self, after: &SessionSnapshot) -> Option<PushKind> {
        let before = self.prior.insert(after.id.clone(), Prior::of(after))?;
        if finished(before, after) {
            Some(PushKind::Finished)
        } else if needs_you(after) && before.status != SessionStatus::AwaitingInput {
            Some(PushKind::NeedsYou)
        } else {
            None
        }
    }
}

/// The desktop's Finished rule (`apps/native/src/notify.rs` `finished`):
/// `after` is idle with no background work left and is an agent rather than
/// a plain shell, and `before` was working, or idle waiting on background
/// work.
fn finished(before: Prior, after: &SessionSnapshot) -> bool {
    if after.status != SessionStatus::Idle
        || after.background
        || after.mode == SessionMode::PlainShell
    {
        return false;
    }
    before.status == SessionStatus::Working
        || (before.status == SessionStatus::Idle && before.background)
}

/// Whether the session needs the user: awaiting input, neither inactive nor
/// abandoned (the phone's Needs you list).
fn needs_you(snapshot: &SessionSnapshot) -> bool {
    snapshot.status == SessionStatus::AwaitingInput
        && !snapshot.is_inactive
        && !snapshot.is_abandoned
}

/// How many sessions need the user (the phone's Needs you count).
pub(crate) fn needs_you_count<'a>(snapshots: impl IntoIterator<Item = &'a SessionSnapshot>) -> u32 {
    let count = snapshots.into_iter().filter(|s| needs_you(s)).count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// `snapshots` with `snapshot` standing for its session, added when the
/// registry no longer holds it: the event can be newer than the registry read.
fn with_snapshot(
    mut snapshots: Vec<SessionSnapshot>,
    snapshot: &SessionSnapshot,
) -> Vec<SessionSnapshot> {
    match snapshots.iter_mut().find(|seen| seen.id == snapshot.id) {
        Some(seen) => *seen = snapshot.clone(),
        None => snapshots.push(snapshot.clone()),
    }
    snapshots
}

/// The session's name in a push: its user label, else its label; the
/// product name when the only name it has is its id. A name over
/// [`LABEL_MOST`] characters is cut to that many, the last one `…`, so the
/// payload stays inside one push record.
pub(crate) fn label(snapshot: &SessionSnapshot) -> String {
    let named = |text: &str| !text.trim().is_empty() && text != snapshot.id;
    let name = snapshot
        .user_label
        .as_deref()
        .filter(|text| named(text))
        .or_else(|| Some(snapshot.label.as_str()).filter(|text| named(text)))
        .unwrap_or(PRODUCT_NAME);
    if name.chars().count() <= LABEL_MOST {
        return name.to_owned();
    }
    let mut cut: String = name.chars().take(LABEL_MOST - 1).collect();
    cut.push('…');
    cut
}

/// The phone app's origin, `https://{name}:{port}` (no port at 443), while
/// the tailnet listener is serving; `None` otherwise, when the phone could
/// not open a push's link.
pub(crate) fn origin(state: &RemoteState) -> Option<String> {
    match state {
        RemoteState::Serving {
            name, port: 443, ..
        } => Some(format!("https://{name}")),
        RemoteState::Serving { name, port, .. } => Some(format!("https://{name}:{port}")),
        _ => None,
    }
}

/// `text` as one URL path segment: every byte but the unreserved ones
/// percent-encoded.
fn path_segment(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// The declarative Web Push message: the title, the session's label, the
/// session's page on the phone app and the Needs you count as a string.
pub(crate) fn payload(
    kind: PushKind,
    label: &str,
    origin: &str,
    session_id: &str,
    count: u32,
) -> serde_json::Value {
    serde_json::json!({
        "web_push": DECLARATIVE_WEB_PUSH,
        "notification": {
            "title": kind.title(),
            "body": label,
            "navigate": format!("{origin}/s/{}", path_segment(session_id)),
            "app_badge": count.to_string(),
        },
    })
}

/// The hub's stores a round reads and writes, locked in this order.
#[derive(Clone, Copy)]
pub(crate) struct Stores<'a> {
    /// `Hub::devices`, taken first.
    pub devices: &'a Mutex<DeviceStore>,
    /// `Hub::push`, a leaf taken after `devices`.
    pub push: &'a Mutex<PushState>,
}

/// One push to deliver.
#[derive(Clone, Copy)]
pub(crate) struct Alert<'a> {
    /// What the session change raised.
    pub kind: PushKind,
    /// The session it is about.
    pub session: &'a SessionSnapshot,
    /// The Needs you count the badge shows.
    pub count: u32,
}

/// What one session event raises.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Step {
    /// A push to deliver.
    Alert {
        /// The change it reports.
        kind: PushKind,
        /// The session it is about.
        session: Box<SessionSnapshot>,
        /// The Needs you count the badge shows.
        count: u32,
    },
    /// Nothing to send.
    Nothing,
    /// The event stream closed; the task ends.
    End,
}

/// The answer one device's push service gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Delivery {
    pub device_id: String,
    pub outcome: Outcome,
}

/// One subscription a round posts to, with everything it needs once the
/// locks are released.
struct Target {
    device_id: String,
    endpoint: String,
    url: Url,
    /// The push service's origin: the JWT's audience and the back-off key.
    audience: String,
    subscriber: Subscriber,
    jwt: String,
}

/// A round's targets and the daemon's VAPID public key.
struct Round {
    public_key: String,
    targets: Vec<Target>,
}

/// What a round signs its JWTs with: the daemon's key, its public point
/// (base64url, computed once a round), the phone app's origin as the
/// subject, and the time.
struct Signer<'a> {
    key: &'a VapidKey,
    public_key: &'a str,
    origin: &'a str,
    now: DateTime<Utc>,
}

/// A push service skipped until `until`.
struct Backoff {
    until: Instant,
    delay: Duration,
}

/// The trigger's own state, owned by its task: the sessions it has seen,
/// the signed tokens, the push services backed off and each subscription's
/// failures in a row. None of it is kept on disk.
pub(crate) struct Trigger {
    /// Each session's last seen status.
    pub tracker: Tracker,
    client: reqwest::Client,
    jwt: JwtCache,
    backoff: HashMap<String, Backoff>,
    /// Failures in a row by (device id, endpoint), so a new endpoint starts
    /// at zero.
    failures: HashMap<(String, String), u32>,
}

impl Trigger {
    /// A trigger posting on `client` ([`send::client`]).
    pub(crate) fn new(client: reqwest::Client) -> Self {
        Self {
            tracker: Tracker::default(),
            client,
            jwt: JwtCache::default(),
            backoff: HashMap::new(),
            failures: HashMap::new(),
        }
    }

    /// Push `alert` to every subscribed device whose switch for its kind is
    /// on, signing at `now`, and act on the answers: a dropped subscription
    /// is deleted, a throttled or failing push service is backed off. Sends
    /// nothing while `remote` is not serving. Returns each answer.
    pub(crate) async fn deliver(
        &mut self,
        stores: Stores<'_>,
        remote: &RemoteState,
        alert: Alert<'_>,
        now: DateTime<Utc>,
    ) -> Vec<Delivery> {
        let Some(origin) = origin(remote) else {
            tracing::debug!(
                kind = ?alert.kind,
                "push: the tailnet listener is not serving; no push is sent"
            );
            return Vec::new();
        };
        let Some(round) = self.collect(stores, alert.kind, &origin, now) else {
            return Vec::new();
        };
        let live: HashSet<(&str, &str)> = round
            .targets
            .iter()
            .map(|target| (target.device_id.as_str(), target.endpoint.as_str()))
            .collect();
        self.failures.retain(|(device_id, endpoint), _| {
            live.contains(&(device_id.as_str(), endpoint.as_str()))
        });
        let session = alert.session;
        let body = payload(
            alert.kind,
            &label(session),
            &origin,
            &session.id,
            alert.count,
        );
        let plaintext = body.to_string().into_bytes();
        let answers = self
            .send_all(round, &plaintext, &send::topic(&session.id))
            .await;
        self.apply(stores, answers)
    }

    /// The round's targets, read under `devices` then `push`; both are
    /// released when it returns. `None` when the push state does not load.
    fn collect(
        &mut self,
        stores: Stores<'_>,
        kind: PushKind,
        origin: &str,
        now: DateTime<Utc>,
    ) -> Option<Round> {
        let devices = lock(stores.devices);
        let mut push = lock(stores.push);
        let policy = push.policy();
        let loaded = match push.load(&devices) {
            Ok(loaded) => loaded,
            Err(err) => {
                tracing::warn!(
                    err = format!("{err:#}"),
                    "push: the push state did not load; no push is sent"
                );
                return None;
            }
        };
        let public_key = loaded.key.public_b64url();
        let signer = Signer {
            key: &loaded.key,
            public_key: &public_key,
            origin,
            now,
        };
        let mut targets = Vec::new();
        for (device_id, subscription) in loaded.subs.iter().filter(|(_, s)| kind.wanted_by(s)) {
            match self.target(&signer, device_id, subscription, policy) {
                Ok(target) => targets.push(target),
                Err(err) => tracing::warn!(
                    %device_id,
                    err = format!("{err:#}"),
                    "push: the device's subscription cannot be pushed to"
                ),
            }
        }
        Some(Round {
            public_key,
            targets,
        })
    }

    /// One subscription as a target: its endpoint checked, its keys read
    /// and the JWT for its push service.
    fn target(
        &mut self,
        signer: &Signer<'_>,
        device_id: &str,
        subscription: &Subscription,
        policy: PushPolicy,
    ) -> anyhow::Result<Target> {
        let url = send::check_endpoint(&subscription.endpoint, policy)?;
        let audience = url.origin().ascii_serialization();
        let subscriber = Subscriber::from_b64url(&subscription.p256dh, &subscription.auth)?;
        let jwt = self.jwt.token(
            signer.key,
            signer.public_key,
            &audience,
            signer.origin,
            signer.now,
        )?;
        Ok(Target {
            device_id: device_id.to_owned(),
            endpoint: subscription.endpoint.clone(),
            url,
            audience,
            subscriber,
            jwt,
        })
    }

    /// Encrypt and post `plaintext` to each target whose push service is
    /// not backed off, holding no lock; returns the answers.
    async fn send_all(
        &mut self,
        round: Round,
        plaintext: &[u8],
        topic: &str,
    ) -> Vec<(Target, Outcome)> {
        let mut answers = Vec::new();
        for target in round.targets {
            if self.backed_off(&target.audience) {
                tracing::debug!(device_id = %target.device_id, push_service = %target.audience, "push: the push service is backed off; skipped");
                continue;
            }
            let body = match webpush::encrypt(&target.subscriber, plaintext) {
                Ok(body) => body,
                Err(err) => {
                    tracing::warn!(device_id = %target.device_id, err = format!("{err:#}"), "push: the payload did not encrypt");
                    continue;
                }
            };
            let answer = send::send(
                &self.client,
                &target.url,
                body,
                topic,
                &target.jwt,
                &round.public_key,
            )
            .await;
            self.note_push_service(&target.audience, answer.as_ref().ok());
            match answer {
                Ok(outcome) => answers.push((target, outcome)),
                Err(err) => {
                    tracing::warn!(device_id = %target.device_id, err = format!("{err:#}"), "push: no answer from the push service");
                }
            }
        }
        answers
    }

    /// Whether `audience`'s back-off is still running.
    fn backed_off(&self, audience: &str) -> bool {
        self.backoff
            .get(audience)
            .is_some_and(|backoff| Instant::now() < backoff.until)
    }

    /// Back `audience` off when it throttled, failed (`Retry`) or could not
    /// be reached (`None`), for at most [`BACKOFF_MOST`]; any other answer
    /// ends its back-off.
    fn note_push_service(&mut self, audience: &str, outcome: Option<&Outcome>) {
        let retry_after = match outcome {
            Some(Outcome::Retry { retry_after, .. }) => *retry_after,
            None => None,
            Some(_) => {
                self.backoff.remove(audience);
                return;
            }
        };
        let delay = self.backoff.get(audience).map_or(BACKOFF_FIRST, |backoff| {
            (backoff.delay * 2).min(BACKOFF_MOST)
        });
        let wait = retry_after
            .map_or(delay, |asked| asked.max(delay))
            .min(BACKOFF_MOST);
        tracing::info!(push_service = %audience, wait_secs = wait.as_secs(), "push: backing the push service off");
        let now = Instant::now();
        let until = now
            .checked_add(wait)
            .or_else(|| now.checked_add(BACKOFF_MOST))
            .unwrap_or(now);
        self.backoff
            .insert(audience.to_owned(), Backoff { until, delay });
    }

    /// Act on the answers: count failures and delete each subscription that
    /// is gone or failed too often, under `devices` then `push`, loading the
    /// state again if it was unloaded since the round collected.
    fn apply(&mut self, stores: Stores<'_>, answers: Vec<(Target, Outcome)>) -> Vec<Delivery> {
        let accepted: HashSet<String> = answers
            .iter()
            .filter(|(_, outcome)| *outcome == Outcome::Sent)
            .map(|(target, _)| target.audience.clone())
            .collect();
        let doomed: Vec<&Target> = answers
            .iter()
            .filter(|(target, outcome)| self.judge(target, outcome, &accepted))
            .map(|(target, _)| target)
            .collect();
        if !doomed.is_empty() {
            let devices = lock(stores.devices);
            let mut push = lock(stores.push);
            match push.load(&devices).map(|_| ()) {
                Ok(()) => {
                    for target in doomed {
                        push.remove_if_endpoint(&target.device_id, &target.endpoint);
                    }
                }
                Err(err) => tracing::warn!(
                    err = format!("{err:#}"),
                    "push: the push state did not load; a dropped subscription is kept until the next round"
                ),
            }
        }
        answers
            .into_iter()
            .map(|(target, outcome)| Delivery {
                device_id: target.device_id,
                outcome,
            })
            .collect()
    }

    /// Whether `target`'s subscription is to be deleted after `outcome`;
    /// `accepted` holds the push services that took a push this round.
    fn judge(&mut self, target: &Target, outcome: &Outcome, accepted: &HashSet<String>) -> bool {
        let key = (target.device_id.clone(), target.endpoint.clone());
        match outcome {
            Outcome::Sent => {
                self.failures.remove(&key);
                false
            }
            Outcome::Gone => {
                self.failures.remove(&key);
                true
            }
            Outcome::Rejected { status: 403, .. } => {
                tracing::info!(device_id = %target.device_id, "push: the push service refused the daemon's token; the subscription is kept");
                false
            }
            Outcome::Rejected { status, .. } => {
                self.count_failure(&key, *status, accepted.contains(&target.audience))
            }
            Outcome::PayloadTooLarge { .. } | Outcome::Retry { .. } => false,
        }
    }

    /// Count a failure against `key` when its push service accepted another
    /// subscription this round; true at the fifth in a row.
    fn count_failure(&mut self, key: &(String, String), status: u16, counted: bool) -> bool {
        if !counted {
            tracing::info!(device_id = %key.0, status, "push: a failure is not counted: no other subscription on its push service was accepted");
            return false;
        }
        let failures = self.failures.entry(key.clone()).or_insert(0);
        *failures += 1;
        if *failures < FAILURES_TO_DELETE {
            return false;
        }
        tracing::warn!(device_id = %key.0, status, "push: the subscription failed five times in a row; it is deleted");
        self.failures.remove(key);
        true
    }

    /// What one broadcast event raises. A lag re-seeds from the registry and
    /// raises nothing for the missed window, a removal is forgotten, and an
    /// update is judged against what was seen before it; the count takes the
    /// event's own snapshot as its session's current state. No I/O.
    pub(crate) fn step(
        &mut self,
        event: Result<SessionEvent, RecvError>,
        sessions: &SessionRegistry,
    ) -> Step {
        match event {
            Err(RecvError::Lagged(missed)) => {
                tracing::warn!(missed, "push: the session event stream lagged; re-seeded");
                self.tracker.seed(&sessions.snapshots());
                Step::Nothing
            }
            Err(RecvError::Closed) => Step::End,
            Ok(SessionEvent::Updated(snapshot, _origin)) => {
                let Some(kind) = self.tracker.observe(&snapshot) else {
                    return Step::Nothing;
                };
                let count = needs_you_count(with_snapshot(sessions.snapshots(), &snapshot).iter());
                Step::Alert {
                    kind,
                    session: snapshot,
                    count,
                }
            }
            Ok(SessionEvent::Removed(id)) => {
                self.tracker.forget(&id);
                Step::Nothing
            }
            Ok(_) => Step::Nothing,
        }
    }
}

/// What the push task reads and watches.
pub(crate) struct Sources {
    /// Every session's registry: the event stream and the Needs you count.
    pub sessions: Arc<SessionRegistry>,
    /// `Hub::devices`, taken first.
    pub devices: Arc<Mutex<DeviceStore>>,
    /// `Hub::push`, a leaf taken after `devices`.
    pub push: Arc<Mutex<PushState>>,
    /// The tailnet listener's current state, read once per alert.
    pub remote: Arc<dyn Fn() -> RemoteState + Send + Sync>,
    /// Flips to `true` when the daemon is stopping.
    pub shutdown: tokio::sync::watch::Receiver<bool>,
}

/// Run the trigger for the daemon's lifetime, clients or none: every session
/// change it raises a push for is delivered. The HTTP client is built here;
/// a failed build logs and starts nothing.
pub(crate) fn spawn(sources: Sources) -> Option<tokio::task::JoinHandle<()>> {
    let client = match send::client() {
        Ok(client) => client,
        Err(err) => {
            tracing::error!(
                err = format!("{err:#}"),
                "push: the HTTP client did not build; no push is sent until the daemon restarts"
            );
            return None;
        }
    };
    let Sources {
        sessions,
        devices,
        push,
        remote,
        mut shutdown,
    } = sources;
    let mut trigger = Trigger::new(client);
    Some(tokio::spawn(async move {
        // Subscribe before seeding, so no change after the seed is lost.
        let mut events = sessions.subscribe();
        trigger.tracker.seed(&sessions.snapshots());
        if *shutdown.borrow() {
            return;
        }
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
                event = events.recv() => match trigger.step(event, &sessions) {
                    Step::Alert { kind, session, count } => {
                        // Read the listener's state once, before any `.await`,
                        // so no watch guard is held across one.
                        let remote_state = remote();
                        let stores = Stores {
                            devices: &devices,
                            push: &push,
                        };
                        let answered = trigger
                            .deliver(stores, &remote_state, Alert { kind, session: &session, count }, Utc::now())
                            .await;
                        tracing::debug!(answered = answered.len(), "push: a round is done");
                    }
                    Step::Nothing => {}
                    Step::End => return,
                },
            }
        }
    }))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use chrono::Utc;
    use p256::elliptic_curve::Generate;
    use p256::elliptic_curve::sec1::ToSec1Point;
    use protocol::{RemoteState, SessionSnapshot, SessionStatus};
    use reqwest::Url;
    use tokio::sync::broadcast::error::RecvError;
    use tokio::time::Instant;

    use super::{
        Alert, BACKOFF_FIRST, BACKOFF_MOST, Delivery, PushKind, Sources, Step, Stores, Tracker,
        Trigger, label, needs_you_count, origin, payload, spawn,
    };
    use crate::devices::DeviceStore;
    use crate::paths::Dirs;
    use crate::push::send::{Outcome, client, topic};
    use crate::push::state::PushState;
    use crate::push::store::Subscription;
    use crate::push::test_fake::{
        Fake, Server, TESTS, fake_push_service, fake_push_service_hooked, fake_push_service_with,
    };
    use crate::push::webpush::{self, Subscriber};
    use crate::server::test_support::idle_record;
    use crate::session::{SessionEvent, SessionRegistry};
    use crate::sync::lock;

    fn snapshot(fields: &serde_json::Value) -> SessionSnapshot {
        let mut session = serde_json::json!({
            "id": "s1",
            "label": "s1",
            "kind": "single",
            "members": [],
            "status": "idle",
            "mode": "interactive",
            "started_at": "2026-01-01T00:00:00Z",
            "exit_code": null,
            "metrics": { "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0, "last_activity_at": null },
            "recent_actions": [],
            "agent": "claude",
        });
        if let (Some(into), Some(from)) = (session.as_object_mut(), fields.as_object()) {
            into.extend(from.clone());
        }
        serde_json::from_value(session).expect("session fixture")
    }

    fn status(status: &str) -> SessionSnapshot {
        snapshot(&serde_json::json!({ "status": status }))
    }

    /// A tracker that has already seen `before`, then what `after` raises.
    fn raised(before: &SessionSnapshot, after: &SessionSnapshot) -> Option<PushKind> {
        let mut tracker = Tracker::default();
        tracker.seed([before]);
        tracker.observe(after)
    }

    #[test]
    fn finished_fires_when_working_goes_idle() {
        assert_eq!(
            raised(&status("working"), &status("idle")),
            Some(PushKind::Finished)
        );
    }

    #[test]
    fn finished_waits_while_background_work_runs() {
        let holding = snapshot(&serde_json::json!({ "status": "idle", "background": true }));
        assert_eq!(raised(&status("working"), &holding), None);
    }

    #[test]
    fn finished_fires_when_background_work_ends() {
        let holding = snapshot(&serde_json::json!({ "status": "idle", "background": true }));
        assert_eq!(raised(&holding, &status("idle")), Some(PushKind::Finished));
    }

    #[test]
    fn finished_ignores_a_plain_shell() {
        let working = snapshot(&serde_json::json!({ "status": "working", "mode": "plain_shell" }));
        let idle = snapshot(&serde_json::json!({ "status": "idle", "mode": "plain_shell" }));
        assert_eq!(raised(&working, &idle), None, "a shell has no turn");
    }

    #[test]
    fn finished_ignores_a_first_sighting() {
        let mut tracker = Tracker::default();
        assert_eq!(
            tracker.observe(&status("idle")),
            None,
            "nothing says a turn ended"
        );
    }

    #[test]
    fn finished_ignores_a_prompt_answered_to_idle() {
        assert_eq!(
            raised(&status("awaiting_input"), &status("idle")),
            None,
            "an answer is no turn's end"
        );
    }

    #[test]
    fn observe_remembers_the_status_it_saw() {
        let mut tracker = Tracker::default();
        assert_eq!(tracker.observe(&status("working")), None, "first seen");
        assert_eq!(tracker.observe(&status("idle")), Some(PushKind::Finished));
        assert_eq!(tracker.observe(&status("idle")), None, "already idle");
    }

    #[test]
    fn needs_you_fires_on_entering_awaiting_input() {
        assert_eq!(
            raised(&status("working"), &status("awaiting_input")),
            Some(PushKind::NeedsYou)
        );
        assert_eq!(
            raised(&status("idle"), &status("awaiting_input")),
            Some(PushKind::NeedsYou)
        );
    }

    #[test]
    fn needs_you_does_not_fire_while_staying_in_it() {
        assert_eq!(
            raised(&status("awaiting_input"), &status("awaiting_input")),
            None
        );
    }

    #[test]
    fn needs_you_ignores_a_first_sighting() {
        let mut trigger = trigger();
        assert_eq!(trigger.tracker.observe(&status("awaiting_input")), None);
    }

    #[test]
    fn a_forgotten_session_is_seen_for_the_first_time_again() {
        let mut tracker = Tracker::default();
        tracker.seed([&status("working")]);
        tracker.forget("s1");
        assert_eq!(tracker.observe(&status("idle")), None);
    }

    #[test]
    fn needs_you_count_skips_inactive_and_abandoned_sessions() {
        let sessions = [
            snapshot(&serde_json::json!({ "id": "a", "status": "awaiting_input" })),
            snapshot(&serde_json::json!({ "id": "b", "status": "awaiting_input" })),
            snapshot(
                &serde_json::json!({ "id": "c", "status": "awaiting_input", "is_inactive": true }),
            ),
            snapshot(
                &serde_json::json!({ "id": "d", "status": "awaiting_input", "is_abandoned": true }),
            ),
            snapshot(&serde_json::json!({ "id": "e", "status": "working" })),
        ];
        assert_eq!(needs_you_count(&sessions), 2);
    }

    #[test]
    fn label_prefers_the_user_label_then_the_label_then_the_product_name() {
        let named = snapshot(
            &serde_json::json!({ "id": "s1", "label": "parser", "user_label": "fix the parser" }),
        );
        assert_eq!(label(&named), "fix the parser");
        let labelled = snapshot(&serde_json::json!({ "id": "s1", "label": "parser" }));
        assert_eq!(label(&labelled), "parser");
        let blank_user =
            snapshot(&serde_json::json!({ "id": "s1", "label": "parser", "user_label": "" }));
        assert_eq!(label(&blank_user), "parser");
        let only_id = snapshot(&serde_json::json!({ "id": "s1", "label": "s1" }));
        assert_eq!(label(&only_id), "Rustling Tulip");
    }

    #[test]
    fn origin_comes_only_from_a_serving_listener() {
        let serving = |port| RemoteState::Serving {
            name: "host.tail1234.ts.net".to_owned(),
            port,
            cert_expires_at: None,
        };
        assert_eq!(
            origin(&serving(8787)).as_deref(),
            Some("https://host.tail1234.ts.net:8787")
        );
        assert_eq!(
            origin(&serving(443)).as_deref(),
            Some("https://host.tail1234.ts.net")
        );
        assert_eq!(origin(&RemoteState::Unknown), None);
    }

    #[test]
    fn payload_is_the_declarative_push() {
        let built = payload(
            PushKind::NeedsYou,
            "fix the parser",
            "https://host.tail1234.ts.net:8787",
            "a b/c",
            3,
        );
        assert_eq!(
            built,
            serde_json::json!({
                "web_push": 8030,
                "notification": {
                    "title": "Needs you",
                    "body": "fix the parser",
                    "navigate": "https://host.tail1234.ts.net:8787/s/a%20b%2Fc",
                    "app_badge": "3",
                },
            })
        );
        let finished = payload(PushKind::Finished, "parser", "https://h", "s1", 0);
        assert_eq!(finished["notification"]["title"], "Finished");
        assert_eq!(finished["notification"]["navigate"], "https://h/s/s1");
        assert_eq!(finished["notification"]["app_badge"], "0");
    }

    // ---- delivery against the fake push service ----

    /// A devices store and a push state over a scratch folder, removed on drop.
    struct Setup {
        dir: std::path::PathBuf,
        devices: Arc<Mutex<DeviceStore>>,
        push: Arc<Mutex<PushState>>,
    }

    impl Drop for Setup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn setup(tag: &str) -> Setup {
        let dir =
            std::env::temp_dir().join(format!("rt-push-trigger-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        let devices = DeviceStore::load(&dir.join("devices.json"));
        let push = PushState::new(Some(dir.join("push")), TESTS);
        Setup {
            dir,
            devices: Arc::new(Mutex::new(devices)),
            push: Arc::new(Mutex::new(push)),
        }
    }

    impl Setup {
        fn stores(&self) -> Stores<'_> {
            Stores {
                devices: &self.devices,
                push: &self.push,
            }
        }

        /// Pair a device; returns its id.
        fn pair(&self, n: u32) -> String {
            let peer = tailnet::PeerIdentity {
                stable_id: format!("nPeer{n}"),
                name: format!("phone{n}.tail1234.ts.net"),
                user_id: 2,
                login: "owner@example.com".to_owned(),
            };
            lock(&self.devices)
                .issue(&peer, Utc::now())
                .expect("issue a device token")
                .0
        }

        fn subscribe(&self, device_id: &str, endpoint: &str, needs_you: bool, finished: bool) {
            put(
                &self.devices,
                &self.push,
                device_id,
                endpoint,
                needs_you,
                finished,
            );
        }

        fn endpoint_of(&self, device_id: &str) -> Option<String> {
            let devices = lock(&self.devices);
            let mut push = lock(&self.push);
            let loaded = push.load(&devices).expect("load the push state");
            loaded.subs.get(device_id).map(|sub| sub.endpoint.clone())
        }
    }

    fn put(
        devices: &Mutex<DeviceStore>,
        push: &Mutex<PushState>,
        device_id: &str,
        endpoint: &str,
        needs_you: bool,
        finished: bool,
    ) {
        let secret = p256::SecretKey::try_generate().expect("a P-256 key");
        let p256dh = URL_SAFE_NO_PAD.encode(secret.public_key().to_sec1_point(false).as_bytes());
        let auth = URL_SAFE_NO_PAD.encode(<[u8; 16]>::try_generate().expect("16 random bytes"));
        let devices = lock(devices);
        let mut push = lock(push);
        push.load(&devices)
            .expect("load the push state")
            .subs
            .put(
                device_id,
                Subscription {
                    kind: "web_push".to_owned(),
                    endpoint: endpoint.to_owned(),
                    p256dh,
                    auth,
                    needs_you,
                    finished,
                    created_at: "2026-11-07T00:00:00Z".to_owned(),
                },
            )
            .expect("store the subscription");
    }

    fn serving() -> RemoteState {
        RemoteState::Serving {
            name: "host.tail1234.ts.net".to_owned(),
            port: 8787,
            cert_expires_at: None,
        }
    }

    fn session() -> SessionSnapshot {
        snapshot(&serde_json::json!({ "id": "s1", "label": "parser", "status": "awaiting_input" }))
    }

    async fn round(
        trigger: &mut Trigger,
        setup: &Setup,
        remote: &RemoteState,
        kind: PushKind,
    ) -> Vec<Delivery> {
        let session = session();
        trigger
            .deliver(
                setup.stores(),
                remote,
                Alert {
                    kind,
                    session: &session,
                    count: 1,
                },
                Utc::now(),
            )
            .await
    }

    fn trigger() -> Trigger {
        Trigger::new(client().expect("a client"))
    }

    /// A paired device subscribed with both switches on to a fake answering
    /// `code`.
    async fn one_device(tag: &str, code: u16) -> (Setup, String, Url, Arc<Fake>, Server) {
        let setup = setup(tag);
        let device = setup.pair(1);
        let (url, fake, server) = fake_push_service(code, "").await;
        setup.subscribe(&device, url.as_str(), true, true);
        (setup, device, url, fake, server)
    }

    #[tokio::test]
    async fn a_created_push_carries_the_web_push_headers() {
        let (setup, device, _url, fake, _server) = one_device("created", 201).await;

        let delivered = round(&mut trigger(), &setup, &serving(), PushKind::NeedsYou).await;

        assert_eq!(
            delivered,
            vec![Delivery {
                device_id: device,
                outcome: Outcome::Sent
            }]
        );
        let seen = fake.seen.lock().expect("unpoisoned");
        assert_eq!(seen.len(), 1, "one POST");
        let (headers, body) = &seen[0];
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        assert_eq!(header("ttl").as_deref(), Some("21600"));
        assert_eq!(header("urgency").as_deref(), Some("high"));
        assert_eq!(header("topic"), Some(topic("s1")));
        assert_eq!(header("content-encoding").as_deref(), Some("aes128gcm"));
        let authorization = header("authorization").expect("an Authorization header");
        assert!(authorization.starts_with("vapid t="), "{authorization}");
        assert!(authorization.contains(", k="), "{authorization}");
        assert!(body.len() > 86, "an encrypted record");
    }

    #[tokio::test]
    async fn a_device_whose_switch_is_off_gets_nothing() {
        let setup = setup("switch-off");
        let quiet = setup.pair(1);
        let loud = setup.pair(2);
        let (url, fake, _server) = fake_push_service(201, "").await;
        setup.subscribe(
            &quiet,
            url.join("quiet").expect("a URL").as_str(),
            false,
            true,
        );
        setup.subscribe(
            &loud,
            url.join("loud").expect("a URL").as_str(),
            true,
            false,
        );

        let needs_you = round(&mut trigger(), &setup, &serving(), PushKind::NeedsYou).await;
        assert_eq!(
            needs_you,
            vec![Delivery {
                device_id: loud.clone(),
                outcome: Outcome::Sent
            }]
        );
        let finished = round(&mut trigger(), &setup, &serving(), PushKind::Finished).await;
        assert_eq!(
            finished,
            vec![Delivery {
                device_id: quiet,
                outcome: Outcome::Sent
            }]
        );
        assert_eq!(fake.posts(), 2, "one POST per round");
    }

    #[tokio::test]
    async fn a_gone_subscription_is_removed() {
        let (setup, device, _url, fake, _server) = one_device("gone", 410).await;

        let delivered = round(&mut trigger(), &setup, &serving(), PushKind::Finished).await;

        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].outcome, Outcome::Gone);
        assert_eq!(fake.posts(), 1);
        assert_eq!(
            setup.endpoint_of(&device),
            None,
            "the subscription is deleted"
        );
    }

    #[tokio::test]
    async fn a_gone_answer_keeps_an_endpoint_registered_during_the_post() {
        let setup = setup("gone-race");
        let device = setup.pair(1);
        let fresh = "http://127.0.0.1:9/push/fresh";
        let hook = {
            let devices = Arc::clone(&setup.devices);
            let push = Arc::clone(&setup.push);
            let device = device.clone();
            Box::new(move || put(&devices, &push, &device, fresh, true, true))
        };
        let (url, fake, _server) = fake_push_service_hooked(410, hook).await;
        setup.subscribe(&device, url.as_str(), true, true);

        let delivered = round(&mut trigger(), &setup, &serving(), PushKind::NeedsYou).await;

        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].outcome, Outcome::Gone);
        assert_eq!(fake.posts(), 1);
        assert_eq!(
            setup.endpoint_of(&device).as_deref(),
            Some(fresh),
            "the re-registered endpoint stays"
        );
    }

    #[tokio::test]
    async fn a_forbidden_push_keeps_the_subscription() {
        let (setup, device, url, fake, _server) = one_device("forbidden", 403).await;

        let delivered = round(&mut trigger(), &setup, &serving(), PushKind::NeedsYou).await;

        assert!(
            matches!(delivered[0].outcome, Outcome::Rejected { status: 403, .. }),
            "{delivered:?}"
        );
        assert_eq!(fake.posts(), 1);
        assert_eq!(
            setup.endpoint_of(&device).as_deref(),
            Some(url.as_str()),
            "a 403 is the sender's fault"
        );
    }

    #[tokio::test]
    async fn too_many_requests_backs_the_host_off() {
        let (setup, device, url, fake, _server) = one_device("throttled", 429).await;
        let mut trigger = trigger();

        let first = round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;
        assert_eq!(
            first,
            vec![Delivery {
                device_id: device.clone(),
                outcome: Outcome::Retry {
                    status: 429,
                    retry_after: None
                }
            }]
        );
        let second = round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;

        assert_eq!(second, Vec::new(), "the host is backed off");
        assert_eq!(fake.posts(), 1, "no second POST inside the back-off");
        assert_eq!(setup.endpoint_of(&device).as_deref(), Some(url.as_str()));
    }

    #[tokio::test]
    async fn no_origin_sends_nothing() {
        let (setup, _device, _url, fake, _server) = one_device("no-origin", 201).await;

        let delivered = round(
            &mut trigger(),
            &setup,
            &RemoteState::Unknown,
            PushKind::NeedsYou,
        )
        .await;

        assert_eq!(delivered, Vec::new());
        assert_eq!(fake.posts(), 0);
    }

    fn audience(url: &Url) -> String {
        url.origin().ascii_serialization()
    }

    #[tokio::test]
    async fn a_huge_retry_after_backs_off_at_most_ten_minutes() {
        let setup = setup("huge-retry-after");
        let device = setup.pair(1);
        let (url, fake, _server) =
            fake_push_service_with(429, "", &[("retry-after", "18446744073709551615")]).await;
        setup.subscribe(&device, url.as_str(), true, true);
        let mut trigger = trigger();

        let delivered = round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;

        assert!(
            matches!(delivered[0].outcome, Outcome::Retry { status: 429, .. }),
            "{delivered:?}"
        );
        assert_eq!(fake.posts(), 1);
        let backoff = trigger
            .backoff
            .get(&audience(&url))
            .expect("the push service is backed off");
        assert!(
            backoff.until <= Instant::now() + BACKOFF_MOST,
            "backed off past ten minutes"
        );
    }

    #[test]
    fn a_long_label_is_cut_so_the_payload_encrypts() {
        let long = snapshot(&serde_json::json!({ "user_label": "x".repeat(4000) }));

        let cut = label(&long);

        assert_eq!(cut.chars().count(), 200, "{cut}");
        assert!(cut.ends_with('…'), "{cut}");
        let body = payload(
            PushKind::NeedsYou,
            &cut,
            "https://host.tail1234.ts.net:8787",
            "0b6f3c1e-58a3-4c86-9d7e-2f0f7b3a9c41",
            999,
        )
        .to_string();
        let secret = p256::SecretKey::try_generate().expect("a P-256 key");
        let subscriber = Subscriber {
            p256dh: secret
                .public_key()
                .to_sec1_point(false)
                .as_bytes()
                .try_into()
                .expect("65 octets"),
            auth: [7; 16],
        };
        webpush::encrypt(&subscriber, body.as_bytes()).expect("the payload fits one record");
    }

    #[tokio::test]
    async fn a_gone_answer_after_the_state_unloaded_still_removes_the_subscription() {
        let setup = setup("gone-unloaded");
        let device = setup.pair(1);
        let hook = {
            let push = Arc::clone(&setup.push);
            Box::new(move || lock(&push).unload())
        };
        let (url, fake, _server) = fake_push_service_hooked(410, hook).await;
        setup.subscribe(&device, url.as_str(), true, true);

        let delivered = round(&mut trigger(), &setup, &serving(), PushKind::NeedsYou).await;

        assert_eq!(delivered[0].outcome, Outcome::Gone);
        assert_eq!(fake.posts(), 1);
        assert_eq!(
            setup.endpoint_of(&device),
            None,
            "the subscription is deleted"
        );
    }

    #[test]
    fn needs_you_ignores_an_abandoned_or_inactive_session() {
        for flag in ["is_abandoned", "is_inactive"] {
            let working = snapshot(&serde_json::json!({ "status": "working", flag: true }));
            let asking = snapshot(&serde_json::json!({ "status": "awaiting_input", flag: true }));
            assert_eq!(raised(&working, &asking), None, "{flag}");
        }
    }

    #[tokio::test]
    async fn any_answer_clears_the_back_off() {
        let (setup, device, url, fake, _server) = one_device("answer-clears", 503).await;
        let mut trigger = trigger();
        let service = audience(&url);

        round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;
        let backoff = trigger.backoff.get_mut(&service).expect("backed off");
        assert_eq!(backoff.delay, BACKOFF_FIRST);
        backoff.until = Instant::now();

        fake.set_status(410);
        let gone = round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;
        assert_eq!(gone[0].outcome, Outcome::Gone);

        setup.subscribe(&device, url.as_str(), true, true);
        fake.set_status(503);
        round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;

        assert_eq!(fake.posts(), 3);
        assert_eq!(
            trigger
                .backoff
                .get(&service)
                .expect("backed off again")
                .delay,
            BACKOFF_FIRST,
            "the 410 ended the back-off, so it starts over"
        );
    }

    #[tokio::test]
    async fn the_failure_counter_forgets_a_replaced_endpoint() {
        let (setup, device, url, _fake, _server) = one_device("failures-pruned", 201).await;
        let mut trigger = trigger();
        let replaced = (device.clone(), "http://127.0.0.1:9/push/old".to_owned());
        trigger.failures.insert(replaced.clone(), 1);

        round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;

        assert!(
            !trigger.failures.contains_key(&replaced),
            "the old endpoint's count is dropped"
        );
        assert!(!trigger.failures.contains_key(&(device, url.to_string())));
    }

    // ---- the event step ----

    /// A registry over the setup's scratch dir, its sessions folder made.
    fn registry(setup: &Setup) -> Arc<SessionRegistry> {
        let dirs = Dirs {
            config: setup.dir.clone(),
            state_file: setup.dir.join("state.json"),
            handshake_file: setup.dir.join("daemon.json"),
            remote_config_file: setup.dir.join("remote.json"),
            sessions_dir: setup.dir.join("sessions"),
            worktrees_dir: setup.dir.join("worktrees"),
            binaries_dir: setup.dir.join("binaries"),
            bin_templates: None,
        };
        std::fs::create_dir_all(&dirs.sessions_dir).expect("create sessions dir");
        SessionRegistry::new(dirs)
    }

    /// The snapshot an update of `s1` to `status` carries, and its event.
    fn update(status: &str) -> (SessionSnapshot, Result<SessionEvent, RecvError>) {
        let snapshot = snapshot(&serde_json::json!({ "status": status }));
        let event = Ok(SessionEvent::Updated(Box::new(snapshot.clone()), None));
        (snapshot, event)
    }

    #[test]
    fn event_step_raises_needs_you_on_entering_awaiting_input() {
        let setup = setup("step-needs-you");
        let sessions = registry(&setup);
        sessions.insert(idle_record("s1"));
        let mut trigger = trigger();
        trigger.tracker.seed(&sessions.snapshots());
        let (_, working) = update("working");
        assert_eq!(trigger.step(working, &sessions), Step::Nothing);

        let (asking, event) = update("awaiting_input");

        assert_eq!(
            trigger.step(event, &sessions),
            Step::Alert {
                kind: PushKind::NeedsYou,
                session: Box::new(asking),
                count: 1,
            }
        );
    }

    #[test]
    fn event_step_counts_the_triggering_session_by_its_event() {
        let setup = setup("step-count");
        let sessions = registry(&setup);
        sessions.insert(idle_record("s1"));
        sessions.update("s1", |record| record.status = SessionStatus::Working);
        let mut trigger = trigger();
        trigger.tracker.seed(&sessions.snapshots());

        // The registry still shows the session working; the event shows it
        // asking, and the count must follow the event.
        let (asking, event) = update("awaiting_input");

        assert_eq!(
            trigger.step(event, &sessions),
            Step::Alert {
                kind: PushKind::NeedsYou,
                session: Box::new(asking),
                count: 1,
            }
        );
    }

    #[test]
    fn event_step_reseeds_on_lag_and_raises_nothing() {
        let setup = setup("step-lag");
        let sessions = registry(&setup);
        sessions.insert(idle_record("s1"));
        sessions.update("s1", |record| record.status = SessionStatus::AwaitingInput);
        let mut trigger = trigger();
        trigger.tracker.seed([&status("working")]);

        assert_eq!(
            trigger.step(Err(RecvError::Lagged(5)), &sessions),
            Step::Nothing
        );

        let (_, event) = update("awaiting_input");
        assert_eq!(
            trigger.step(event, &sessions),
            Step::Nothing,
            "the re-seed already saw this state"
        );
    }

    #[test]
    fn event_step_forgets_a_removed_session() {
        let setup = setup("step-removed");
        let sessions = registry(&setup);
        sessions.insert(idle_record("s1"));
        let mut trigger = trigger();
        trigger.tracker.seed([&status("working")]);

        assert_eq!(
            trigger.step(Ok(SessionEvent::Removed("s1".to_owned())), &sessions),
            Step::Nothing
        );
        let (_, event) = update("idle");
        assert_eq!(
            trigger.step(event, &sessions),
            Step::Nothing,
            "seen for the first time again"
        );
    }

    #[test]
    fn event_step_ends_when_the_stream_closes() {
        let setup = setup("step-closed");
        let sessions = registry(&setup);
        let mut trigger = trigger();

        assert_eq!(trigger.step(Err(RecvError::Closed), &sessions), Step::End);
    }

    // ---- the task ----

    /// Wait until the fake has seen `want` POSTs; the hang guard fails the
    /// test as hung.
    async fn wait_for_posts(fake: &Fake, want: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while fake.posts() < want {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the push arrives");
    }

    /// The task's sources over the setup's stores, the listener serving.
    fn sources(
        setup: &Setup,
        sessions: &Arc<SessionRegistry>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Sources {
        Sources {
            sessions: Arc::clone(sessions),
            devices: Arc::clone(&setup.devices),
            push: Arc::clone(&setup.push),
            remote: Arc::new(serving),
            shutdown,
        }
    }

    #[tokio::test]
    async fn the_task_pushes_needs_you_and_finished() {
        let setup = setup("task-pushes");
        let device = setup.pair(1);
        let (url, fake, _server) = fake_push_service(201, "").await;
        setup.subscribe(&device, url.as_str(), true, true);
        let sessions = registry(&setup);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = spawn(sources(&setup, &sessions, shutdown_rx)).expect("the push task starts");
        // Let the task subscribe and seed before the changes it must see.
        tokio::task::yield_now().await;

        sessions.insert(idle_record("s1"));
        sessions.update("s1", |record| record.status = SessionStatus::Working);
        sessions.update("s1", |record| record.status = SessionStatus::AwaitingInput);
        wait_for_posts(&fake, 1).await;
        sessions.update("s1", |record| record.status = SessionStatus::Working);
        sessions.update("s1", |record| record.status = SessionStatus::Idle);
        wait_for_posts(&fake, 2).await;

        let header = {
            let seen = fake.seen.lock().expect("unpoisoned");
            let second = &seen[1];
            second
                .0
                .get("topic")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        assert_eq!(
            header.as_deref(),
            Some(topic("s1").as_str()),
            "the Finished push's topic"
        );

        shutdown_tx.send_replace(true);
        let joined = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("the task ends");
        joined.expect("the task ends cleanly");
    }

    #[tokio::test]
    async fn a_session_present_at_start_raises_nothing_until_it_changes() {
        let setup = setup("task-first-seen");
        let device = setup.pair(1);
        let (url, fake, _server) = fake_push_service(201, "").await;
        setup.subscribe(&device, url.as_str(), true, true);
        let sessions = registry(&setup);
        sessions.insert(idle_record("s1"));
        sessions.update("s1", |record| record.status = SessionStatus::AwaitingInput);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = spawn(sources(&setup, &sessions, shutdown_rx)).expect("the push task starts");
        // The task seeds with the session already asking; that raises nothing.
        tokio::task::yield_now().await;

        sessions.update("s1", |record| record.status = SessionStatus::Working);
        sessions.update("s1", |record| record.status = SessionStatus::Idle);
        wait_for_posts(&fake, 1).await;

        assert_eq!(fake.posts(), 1, "the start state raised nothing");

        shutdown_tx.send_replace(true);
        let joined = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("the task ends");
        joined.expect("the task ends cleanly");
    }

    #[tokio::test]
    async fn shutdown_ends_the_task() {
        let setup = setup("task-shutdown");
        let sessions = registry(&setup);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = spawn(sources(&setup, &sessions, shutdown_rx)).expect("the push task starts");

        shutdown_tx.send_replace(true);

        let joined = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("the task ends");
        joined.expect("the task ends cleanly");
    }
}
