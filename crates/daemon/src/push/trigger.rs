//! The push trigger: which session changes raise a push (Needs you,
//! Finished), the declarative payload each one carries, and the delivery
//! round that encrypts and posts it to every subscribed device and acts on
//! the push services' answers (`docs/plans/mobile-app.md`, "Push").
//!
//! A round takes `Hub::devices` then `Hub::push` to collect its targets and
//! releases `devices` as soon as it holds `push`, so the push state's file
//! I/O runs under `push` alone; it releases `push` before any network call,
//! and takes both again the same way afterwards to delete a subscription its
//! push service dropped.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::StreamExt as _;
use protocol::{
    PUSH_NOTICE_VERSION, PushNotice, PushNoticeKind, RemoteState, SessionMode, SessionSnapshot,
    SessionStatus,
};
use push_crypto::Subscriber;
use reqwest::Url;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Instant, sleep_until};

use super::relay;
use super::send::{self, Outcome, PushPolicy};
use super::state::PushState;
use super::store::{Channel, Subscription};
use super::vapid::{JwtCache, VapidKey};
use crate::devices::DeviceStore;
use crate::presence::Presence;
use crate::session::{SessionEvent, SessionRegistry};
use crate::sync::lock;

/// The body of a push whose session has no name but its id.
const PRODUCT_NAME: &str = "Rustling Tulip";

/// The most characters of a session's name a push carries.
const LABEL_MOST: usize = 200;

/// The declarative Web Push magic number (`"web_push": 8030`).
const DECLARATIVE_WEB_PUSH: u16 = 8030;

/// How long a Finished push waits for its session to stay finished: the
/// default of the desktop's Finished grace (`apps/native/src/notify.rs`
/// `hold_finished`).
const FINISHED_GRACE: Duration = Duration::from_secs(5);

/// The first back-off after a push service throttled, failed or could not be
/// reached; each further one doubles up to [`BACKOFF_MOST`].
const BACKOFF_FIRST: Duration = Duration::from_secs(30);

/// The longest back-off.
const BACKOFF_MOST: Duration = Duration::from_secs(600);

/// Failures in a row after which a subscription is deleted, counted only
/// while its push service accepts another subscription in the same round.
const FAILURES_TO_DELETE: u32 = 5;

/// How many of a round's pushes are in flight at once.
const SENDS_AT_ONCE: usize = 8;

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

/// When each session's Finished push ends its grace (the phone's counterpart
/// of the desktop's `fold_state` over its held toasts). A hold is only a
/// deadline: what goes out is read from the registry when it is due.
#[derive(Debug, Default)]
struct FinishedHolds {
    until: HashMap<String, Instant>,
}

impl FinishedHolds {
    /// Hold a session's Finished push until `until`.
    fn hold(&mut self, session_id: &str, until: Instant) {
        self.until.insert(session_id.to_owned(), until);
    }

    /// Take in a session's newer copy: one that left `Idle` or shows
    /// background work drops its hold.
    fn fold(&mut self, snapshot: &SessionSnapshot) {
        if snapshot.status != SessionStatus::Idle || snapshot.background {
            self.until.remove(&snapshot.id);
        }
    }

    /// Forget a removed session's hold.
    fn forget(&mut self, session_id: &str) {
        self.until.remove(session_id);
    }

    /// When the next hold ends.
    fn next_due(&self) -> Option<Instant> {
        self.until.values().min().copied()
    }

    /// Take the sessions whose holds have ended by `now`, each once.
    fn due(&mut self, now: Instant) -> Vec<String> {
        self.until
            .extract_if(|_, until| *until <= now)
            .map(|(session_id, _)| session_id)
            .collect()
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

/// `snapshots` with `snapshot` in place of its session's entry; a session
/// the registry no longer holds is not added back.
fn with_snapshot(
    mut snapshots: Vec<SessionSnapshot>,
    snapshot: &SessionSnapshot,
) -> Vec<SessionSnapshot> {
    if let Some(seen) = snapshots.iter_mut().find(|seen| seen.id == snapshot.id) {
        *seen = snapshot.clone();
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

/// The tailnet name the listener serves under, without the port: the
/// `host` of a relay push. `None` while the listener is not serving.
fn host_name(state: &RemoteState) -> Option<&str> {
    match state {
        RemoteState::Serving { name, .. } => Some(name),
        _ => None,
    }
}

/// The relay push's plaintext for the same alert [`payload`] carries: the
/// title, the session's label ([`label`]), its id, the tailnet `host` and
/// the Needs you count as the badge.
pub(crate) fn notice(
    kind: PushKind,
    session: &SessionSnapshot,
    host: Option<&str>,
    count: u32,
) -> PushNotice {
    PushNotice {
        v: PUSH_NOTICE_VERSION,
        kind: match kind {
            PushKind::NeedsYou => PushNoticeKind::NeedsYou,
            PushKind::Finished => PushNoticeKind::Finished,
        },
        title: kind.title().to_owned(),
        body: label(session),
        session_id: session.id.clone(),
        host: host.map(str::to_owned),
        badge: count,
    }
}

/// Which plaintext a target is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plaintext {
    /// The declarative Web Push JSON ([`payload`]).
    WebPush,
    /// The relay's [`PushNotice`] JSON ([`notice`]).
    Notice,
}

/// A round's plaintexts, built once: the declarative JSON only while the
/// listener serves the phone app's origin, the notice unless it failed to
/// serialize.
struct Plaintexts {
    web_push: Option<Vec<u8>>,
    notice: Option<Vec<u8>>,
}

impl Plaintexts {
    /// Both plaintexts for `alert`, the declarative one under `origin`.
    fn of(alert: Alert<'_>, origin: Option<&str>, remote: &RemoteState) -> Self {
        let session = alert.session;
        let web_push = origin.map(|origin| {
            payload(
                alert.kind,
                &label(session),
                origin,
                &session.id,
                alert.count,
            )
            .to_string()
            .into_bytes()
        });
        let notice = notice(alert.kind, session, host_name(remote), alert.count);
        let notice = match serde_json::to_vec(&notice) {
            Ok(bytes) => Some(bytes),
            Err(err) => {
                tracing::warn!(
                    err = format!("{err:#}"),
                    "push: the relay notice did not serialize; no relay push is sent this round"
                );
                None
            }
        };
        Self { web_push, notice }
    }

    /// The plaintext a target of `kind` is sent, if the round has it.
    fn get(&self, kind: Plaintext) -> Option<&[u8]> {
        match kind {
            Plaintext::WebPush => self.web_push.as_deref(),
            Plaintext::Notice => self.notice.as_deref(),
        }
    }
}

/// The hub's stores a round reads and writes, locked in this order.
#[derive(Clone, Copy)]
pub(crate) struct Stores<'a> {
    /// `Hub::devices`, taken first and released once `push` is held.
    pub devices: &'a Mutex<DeviceStore>,
    /// `Hub::push`, a leaf taken after `devices`; the push state's file
    /// I/O runs under it alone.
    pub push: &'a Mutex<PushState>,
}

/// Lock `Hub::devices`, read its unrevoked ids, lock `Hub::push`, then
/// release `devices`, returning the ids and the `push` guard; the caller
/// loads the state with those ids and does its file I/O under `push` alone.
///
/// `devices` is released only once `push` is held: a revoke whose
/// `remove_device` found the state unloaded changed `devices` before this
/// read, so the first-load sweep drops its subscription, and a revoke after
/// this read waits on `push` and then deletes the subscription from the
/// loaded state.
fn lock_push(stores: Stores<'_>) -> (Option<HashSet<String>>, MutexGuard<'_, PushState>) {
    let devices = lock(stores.devices);
    let paired = super::state::unrevoked_ids(&devices);
    let push = lock(stores.push);
    drop(devices);
    (paired, push)
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
    /// Whether the owner was at the desk when the alert was raised, read once
    /// per alert before any `.await`.
    pub at_desk: bool,
    /// Whether only the targets whose `hold_at_desk` is on are pushed to: a
    /// held Needs you push delivered once the owner left the desk, after the
    /// other targets got it the first time.
    pub only_held: bool,
}

/// What one session event raises.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Step {
    /// A Needs you push to deliver.
    Alert {
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
    /// The endpoint or token the subscription is reached at: the key its
    /// failures and a late "gone" are recorded under.
    address: String,
    url: Url,
    /// The push service's or the relay's origin: the JWT's audience, and
    /// what a round's accepted pushes are counted by for the failure rule.
    audience: String,
    /// The key the back-off is kept under: the audience for Web Push, so a
    /// push service is backed off as one; the audience and the token's
    /// [`relay::fingerprint`] for a relay target, so one phone's throttling
    /// pauses no other phone. Never logged.
    backoff_key: String,
    /// Which of the round's plaintexts it is sent.
    plaintext: Plaintext,
    subscriber: Subscriber,
    jwt: String,
}

/// Where a target's push goes and how it is signed, before its keys are
/// read: one arm of [`Trigger::target`].
struct Route<'a> {
    url: Url,
    audience: String,
    backoff_key: String,
    plaintext: Plaintext,
    /// The JWT's `sub`.
    subject: &'a str,
}

/// A round's targets, the daemon's VAPID public key, how many targets the
/// desk held back, and what the store holds.
struct Round {
    public_key: String,
    targets: Vec<Target>,
    held: u32,
    live: Live,
}

/// What a round signs its JWTs with: the daemon's key, its public point
/// (base64url, computed once a round), the phone app's origin as a Web Push
/// subject (`None` while the listener is not serving, when no Web Push
/// target is built), the relay's origin (`None` when [`send::check_relay`]
/// refused it, when no relay target is built), and the time.
struct Signer<'a> {
    key: &'a VapidKey,
    public_key: &'a str,
    origin: Option<&'a str>,
    relay: Option<&'a Url>,
    now: DateTime<Utc>,
}

/// The relay origin a round posts under: `origin` when [`send::check_relay`]
/// admits it, else `None`, warned once for the round.
fn checked_relay(origin: &Url, policy: PushPolicy) -> Option<Url> {
    match send::check_relay(origin, policy) {
        Ok(()) => Some(origin.clone()),
        Err(err) => {
            tracing::warn!(
                err = format!("{err:#}"),
                "push: the relay origin is refused; this round sends no relay push"
            );
            None
        }
    }
}

/// The p256dh and auth keys of any channel.
fn channel_keys(channel: &Channel) -> (&str, &str) {
    match channel {
        Channel::WebPush { p256dh, auth, .. }
        | Channel::Apns { p256dh, auth, .. }
        | Channel::Fcm { p256dh, auth, .. } => (p256dh, auth),
    }
}

/// A Web Push subscription's route: its endpoint checked, its push service's
/// origin as audience and back-off key, the phone app's origin as subject.
/// `None`, with a debug line, while the listener is not serving.
fn web_push_route<'a>(
    signer: &Signer<'a>,
    device_id: &str,
    endpoint: &str,
    policy: PushPolicy,
) -> anyhow::Result<Option<Route<'a>>> {
    let Some(origin) = signer.origin else {
        tracing::debug!(
            %device_id,
            "push: the tailnet listener is not serving; the Web Push subscription is skipped"
        );
        return Ok(None);
    };
    let url = send::check_endpoint(endpoint, policy)?;
    let audience = url.origin().ascii_serialization();
    Ok(Some(Route {
        url,
        backoff_key: audience.clone(),
        audience,
        plaintext: Plaintext::WebPush,
        subject: origin,
    }))
}

/// A relay subscription's route: the relay URL for its token, the relay's
/// origin as audience, a per-token back-off key and the fixed product
/// subject. `None` when the round's relay origin was refused, and for an
/// `fcm` subscription until the relay sends FCM (it answers 404 there, which
/// would delete the subscription).
fn relay_route(signer: &Signer<'_>, device_id: &str, channel: &Channel) -> Option<Route<'static>> {
    if matches!(channel, Channel::Fcm { .. }) {
        tracing::debug!(
            %device_id,
            "push: the relay has no FCM sender yet; the fcm subscription is skipped"
        );
        return None;
    }
    let origin = signer.relay?;
    let url = relay::relay_url(origin, channel)?;
    let audience = origin.origin().ascii_serialization();
    let backoff_key = relay_backoff_key(&audience, channel);
    Some(Route {
        url,
        audience,
        backoff_key,
        plaintext: Plaintext::Notice,
        subject: relay::SUBJECT,
    })
}

/// A relay subscription's back-off key under the relay `audience`: the
/// audience and its token's [`relay::fingerprint`], never the token.
fn relay_backoff_key(audience: &str, channel: &Channel) -> String {
    format!("{audience}#{}", relay::fingerprint(channel.address()))
}

/// The back-off key a stored subscription's targets have: its endpoint's
/// origin for Web Push, [`relay_backoff_key`] under `relay_origin` for a
/// relay channel. `None` for an endpoint that is not a URL.
fn backoff_key_of(channel: &Channel, relay_origin: &Url) -> Option<String> {
    match channel {
        Channel::WebPush { endpoint, .. } => Url::parse(endpoint)
            .ok()
            .map(|url| url.origin().ascii_serialization()),
        Channel::Apns { .. } | Channel::Fcm { .. } => Some(relay_backoff_key(
            &relay_origin.origin().ascii_serialization(),
            channel,
        )),
    }
}

/// What the store holds, whichever subscriptions a round pushes to: the
/// back-off keys and the (device id, address) pairs of every stored
/// subscription of a paired device. A back-off or a failure count goes only
/// once its subscription leaves the store.
#[derive(Default)]
struct Live {
    keys: HashSet<String>,
    addresses: HashSet<(String, String)>,
}

impl Live {
    /// The live sets of `subs`, relay keys under `relay_origin`.
    fn of<'s>(subs: impl Iterator<Item = (&'s str, &'s Subscription)>, relay_origin: &Url) -> Self {
        let mut live = Self::default();
        for (device_id, subscription) in subs {
            let channel = &subscription.channel;
            live.keys.extend(backoff_key_of(channel, relay_origin));
            live.addresses
                .insert((device_id.to_owned(), channel.address().to_owned()));
        }
        live
    }
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
    /// Back-offs by [`Target::backoff_key`]: a push service's origin, or a
    /// relay origin and a token's fingerprint.
    backoff: HashMap<String, Backoff>,
    /// Failures in a row by (device id, address), so a new address starts at
    /// zero.
    failures: HashMap<(String, String), u32>,
    /// The sessions whose Needs you push was held at the desk; it is
    /// delivered to the held subscriptions when the owner leaves, if the
    /// session still needs the user then.
    held: HashSet<String>,
    /// The Finished pushes waiting out their grace.
    finishing: FinishedHolds,
    /// How long a Finished push waits: [`FINISHED_GRACE`], but for a test's.
    finished_grace: Duration,
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
            held: HashSet::new(),
            finishing: FinishedHolds::default(),
            finished_grace: FINISHED_GRACE,
        }
    }

    /// Push `alert` to every subscribed device whose switch for its kind is
    /// on, signing at `now`, and act on the answers: a dropped subscription
    /// is deleted, a throttled or failing push service or relay token is
    /// backed off. While `remote` is not serving, only the relay targets are
    /// pushed. Returns each answer.
    pub(crate) async fn deliver(
        &mut self,
        stores: Stores<'_>,
        remote: &RemoteState,
        alert: Alert<'_>,
        now: DateTime<Utc>,
    ) -> Vec<Delivery> {
        let origin = origin(remote);
        if origin.is_none() {
            tracing::debug!(
                kind = ?alert.kind,
                "push: the tailnet listener is not serving; only relay pushes are sent"
            );
        }
        let Some(mut round) = self.collect(
            stores,
            alert.kind,
            origin.as_deref(),
            now,
            alert.at_desk,
            alert.only_held,
        ) else {
            return Vec::new();
        };
        if alert.kind == PushKind::NeedsYou && round.held > 0 {
            self.held.insert(alert.session.id.clone());
        }
        let live = std::mem::take(&mut round.live);
        self.failures.retain(|key, _| live.addresses.contains(key));
        let plaintexts = Plaintexts::of(alert, origin.as_deref(), remote);
        let answers = self
            .send_all(round, &plaintexts, &send::topic(&alert.session.id))
            .await;
        self.prune_backoff(&live.keys);
        self.apply(stores, answers)
    }

    /// Drop every back-off that has run out and whose key no stored
    /// subscription has, so a token deleted or replaced while backed off
    /// leaves no entry behind.
    fn prune_backoff(&mut self, keys: &HashSet<String>) {
        let now = Instant::now();
        self.backoff
            .retain(|key, backoff| keys.contains(key) || now < backoff.until);
    }

    /// The round's targets: [`lock_push`] reads `devices` and releases it
    /// once `push` is held, and the state is loaded and read under `push`
    /// alone, released when it returns. A target whose subscription holds at the
    /// desk is left out while `at_desk`, and counted in [`Round::held`], so
    /// its session can be delivered when the owner leaves; `only_held` is
    /// that later round, keeping only those targets. `None` when the push
    /// state does not load.
    fn collect(
        &mut self,
        stores: Stores<'_>,
        kind: PushKind,
        origin: Option<&str>,
        now: DateTime<Utc>,
        at_desk: bool,
        only_held: bool,
    ) -> Option<Round> {
        // A push request stores a subscription under `push` alone and
        // undoes it if the device was revoked meanwhile; until it does, the
        // subscription of a device that is not an unrevoked record is skipped.
        let (paired, mut push) = lock_push(stores);
        let policy = push.policy();
        let relay_origin = push.relay_origin().clone();
        let relay = checked_relay(&relay_origin, policy);
        let loaded = match push.load(paired.as_ref()) {
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
            relay: relay.as_ref(),
            now,
        };
        let mut targets = Vec::new();
        let mut held = 0_u32;
        let stored = || {
            loaded
                .subs
                .iter()
                .filter(|(id, _)| paired.as_ref().is_none_or(|ids| ids.contains(*id)))
        };
        let live = Live::of(stored(), &relay_origin);
        let wanted = stored().filter(|(_, s)| kind.wanted_by(s));
        for (device_id, subscription) in wanted {
            if only_held && !subscription.hold_at_desk {
                continue;
            }
            if at_desk && subscription.hold_at_desk {
                held += 1;
                continue;
            }
            match self.target(&signer, device_id, subscription, policy) {
                Ok(Some(target)) => targets.push(target),
                Ok(None) => {}
                Err(err) => tracing::warn!(
                    %device_id,
                    kind = subscription.channel.kind(),
                    err = format!("{err:#}"),
                    "push: the device's subscription cannot be pushed to"
                ),
            }
        }
        if held > 0 {
            tracing::debug!(
                held,
                kind = ?kind,
                "push: the owner is at the desk; the round's held pushes wait for the owner to leave"
            );
        }
        Some(Round {
            public_key,
            targets,
            held,
            live,
        })
    }

    /// The held sessions still present and still needing the user, as their
    /// current snapshots.
    fn held_now(&self, sessions: &SessionRegistry) -> Vec<SessionSnapshot> {
        let snapshots = sessions.snapshots();
        self.held
            .iter()
            .filter_map(|id| snapshots.iter().find(|seen| &seen.id == id).cloned())
            .filter(needs_you)
            .collect()
    }

    /// One subscription as a target: a Web Push endpoint checked or a relay
    /// URL built for its token, its keys read and the JWT for the audience.
    /// A Web Push subscription while the listener is not serving, and a relay
    /// one while the relay origin is refused, are skipped (`Ok(None)`).
    fn target(
        &mut self,
        signer: &Signer<'_>,
        device_id: &str,
        subscription: &Subscription,
        policy: PushPolicy,
    ) -> anyhow::Result<Option<Target>> {
        let channel = &subscription.channel;
        let route = match channel {
            Channel::WebPush { endpoint, .. } => {
                web_push_route(signer, device_id, endpoint, policy)?
            }
            Channel::Apns { .. } | Channel::Fcm { .. } => relay_route(signer, device_id, channel),
        };
        let Some(route) = route else {
            return Ok(None);
        };
        let (p256dh, auth) = channel_keys(channel);
        let subscriber = Subscriber::from_b64url(p256dh, auth)?;
        let jwt = self.jwt.token(
            signer.key,
            signer.public_key,
            &route.audience,
            route.subject,
            signer.now,
        )?;
        Ok(Some(Target {
            device_id: device_id.to_owned(),
            address: channel.address().to_owned(),
            url: route.url,
            audience: route.audience,
            backoff_key: route.backoff_key,
            plaintext: route.plaintext,
            subscriber,
            jwt,
        }))
    }

    /// Encrypt every target that is not backed off with the plaintext it
    /// takes, then post them: at most [`SENDS_AT_ONCE`] pushes are in flight
    /// at once, and each back-off key is noted once, after every answer is
    /// in. Holds no lock; returns the answers in target order.
    async fn send_all(
        &mut self,
        round: Round,
        plaintexts: &Plaintexts,
        topic: &str,
    ) -> Vec<(Target, Outcome)> {
        let prepared = self.prepare(round.targets, plaintexts);
        let client = &self.client;
        let public_key = &round.public_key;
        let answers: Vec<(Target, anyhow::Result<Outcome>)> = futures::stream::iter(prepared)
            .map(|(target, body)| async move {
                let answer =
                    send::send(client, &target.url, body, topic, &target.jwt, public_key).await;
                (target, answer)
            })
            .buffered(SENDS_AT_ONCE)
            .collect()
            .await;
        self.note_round(&answers);
        let mut delivered = Vec::new();
        for (target, answer) in answers {
            match answer {
                Ok(outcome) => delivered.push((target, outcome)),
                Err(err) => {
                    tracing::warn!(device_id = %target.device_id, err = format!("{err:#}"), "push: no answer from the push service");
                }
            }
        }
        delivered
    }

    /// The targets to post, each with its body: a target whose back-off key
    /// is backed off is skipped, and so is one whose plaintext the round has
    /// not got.
    fn prepare(&self, targets: Vec<Target>, plaintexts: &Plaintexts) -> Vec<(Target, Vec<u8>)> {
        let mut prepared = Vec::new();
        for target in targets {
            if self.backed_off(&target.backoff_key) {
                tracing::debug!(device_id = %target.device_id, push_service = %target.audience, "push: the push service is backed off; skipped");
                continue;
            }
            let Some(plaintext) = plaintexts.get(target.plaintext) else {
                tracing::debug!(device_id = %target.device_id, plaintext = ?target.plaintext, "push: no plaintext for the target; skipped");
                continue;
            };
            match push_crypto::encrypt(&target.subscriber, plaintext) {
                Ok(body) => prepared.push((target, body)),
                Err(err) => {
                    tracing::warn!(device_id = %target.device_id, err = format!("{err:#}"), "push: the payload did not encrypt");
                }
            }
        }
        prepared
    }

    /// Note each back-off key's answers once for the round: a key with a
    /// throttled, failed or unreachable answer is backed off once, at the
    /// longest `Retry-After` it asked for; a key whose answers all arrived
    /// and none asked to retry has its back-off cleared.
    fn note_round(&mut self, answers: &[(Target, anyhow::Result<Outcome>)]) {
        let mut keys: Vec<(&str, &str)> = Vec::new();
        for (target, _) in answers {
            if !keys.iter().any(|(key, _)| *key == target.backoff_key) {
                keys.push((target.backoff_key.as_str(), target.audience.as_str()));
            }
        }
        for (key, service) in keys {
            self.note_key(key, service, answers);
        }
    }

    /// One back-off key's answers, noted once for the round; `service` is
    /// its audience, the origin the log names.
    fn note_key(
        &mut self,
        key: &str,
        service: &str,
        answers: &[(Target, anyhow::Result<Outcome>)],
    ) {
        let mut back_off = false;
        let mut longest: Option<(Option<Duration>, &Outcome)> = None;
        let mut first_ok: Option<&Outcome> = None;
        for (_, answer) in answers
            .iter()
            .filter(|(target, _)| target.backoff_key == key)
        {
            match answer {
                Err(_) => back_off = true,
                Ok(outcome @ Outcome::Retry { retry_after, .. }) => {
                    back_off = true;
                    let asked = *retry_after;
                    if longest.is_none_or(|(seen, _)| asked > seen) {
                        longest = Some((asked, outcome));
                    }
                }
                Ok(outcome) => {
                    first_ok.get_or_insert(outcome);
                }
            }
        }
        if back_off {
            self.note_push_service(key, service, longest.map(|(_, outcome)| outcome));
        } else if let Some(outcome) = first_ok {
            self.note_push_service(key, service, Some(outcome));
        }
    }

    /// Whether `key`'s back-off is still running.
    fn backed_off(&self, key: &str) -> bool {
        self.backoff
            .get(key)
            .is_some_and(|backoff| Instant::now() < backoff.until)
    }

    /// Back `key` off when it throttled, failed (`Retry`) or could not be
    /// reached (`None`), for at most [`BACKOFF_MOST`]; any other answer ends
    /// its back-off. The log names `service`, the key's origin, never the key.
    fn note_push_service(&mut self, key: &str, service: &str, outcome: Option<&Outcome>) {
        let retry_after = match outcome {
            Some(Outcome::Retry { retry_after, .. }) => *retry_after,
            None => None,
            Some(_) => {
                self.backoff.remove(key);
                return;
            }
        };
        let delay = self.backoff.get(key).map_or(BACKOFF_FIRST, |backoff| {
            (backoff.delay * 2).min(BACKOFF_MOST)
        });
        let wait = retry_after
            .map_or(delay, |asked| asked.max(delay))
            .min(BACKOFF_MOST);
        tracing::info!(push_service = %service, wait_secs = wait.as_secs(), "push: backing the push service off");
        let now = Instant::now();
        let until = now
            .checked_add(wait)
            .or_else(|| now.checked_add(BACKOFF_MOST))
            .unwrap_or(now);
        self.backoff
            .insert(key.to_owned(), Backoff { until, delay });
    }

    /// Act on the answers: count failures and delete each subscription that
    /// is gone or failed too often, under `push` alone once [`lock_push`]
    /// has read `devices`, loading the state again if it was unloaded since
    /// the round collected.
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
            let (paired, mut push) = lock_push(stores);
            match push.load(paired.as_ref()).map(|_| ()) {
                Ok(()) => {
                    for target in doomed {
                        push.remove_if_address(&target.device_id, &target.address);
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
        let key = (target.device_id.clone(), target.address.clone());
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
    /// raises nothing for the missed window, keeping the Finished holds, which
    /// read the registry when they end; a removal is forgotten, and an
    /// update is judged against what was seen before it; the count takes the
    /// event's own snapshot as its session's current state, and an update for
    /// a session the registry no longer holds raises nothing. A Finished raises
    /// no push here: it is held through its grace, and any update first
    /// drops a hold its session no longer earns. No I/O.
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
                if !needs_you(&snapshot) {
                    self.held.remove(&snapshot.id);
                }
                self.finishing.fold(&snapshot);
                match self.tracker.observe(&snapshot) {
                    None => Step::Nothing,
                    Some(PushKind::Finished) => {
                        let until = Instant::now() + self.finished_grace;
                        self.finishing.hold(&snapshot.id, until);
                        Step::Nothing
                    }
                    Some(PushKind::NeedsYou) => {
                        let snapshots = sessions.snapshots();
                        if !snapshots.iter().any(|seen| seen.id == snapshot.id) {
                            return Step::Nothing;
                        }
                        let count = needs_you_count(with_snapshot(snapshots, &snapshot).iter());
                        Step::Alert {
                            session: snapshot,
                            count,
                        }
                    }
                }
            }
            Ok(SessionEvent::Removed(id)) => {
                self.held.remove(&id);
                self.finishing.forget(&id);
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
    /// The live connections reporting the owner at the desk, a leaf lock read
    /// once per alert.
    pub presence: Arc<Presence>,
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
    Some(run(Trigger::new(client), sources))
}

/// Start the push task with `trigger`.
fn run(mut trigger: Trigger, sources: Sources) -> tokio::task::JoinHandle<()> {
    let Sources {
        sessions,
        devices,
        push,
        presence,
        remote,
        mut shutdown,
    } = sources;
    tokio::spawn(async move {
        let stores = Stores {
            devices: &devices,
            push: &push,
        };
        // Subscribe before seeding, so no change after the seed is lost.
        let mut desk = presence.subscribe();
        let mut events = sessions.subscribe();
        trigger.tracker.seed(&sessions.snapshots());
        if *shutdown.borrow() {
            return;
        }
        loop {
            let next_finished = trigger.finishing.next_due();
            // In arm order: an event waiting is folded before a hold it may
            // drop can end.
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
                event = events.recv() => match trigger.step(event, &sessions) {
                    Step::Alert { session, count } => {
                        let alert = Alert {
                            kind: PushKind::NeedsYou,
                            session: &session,
                            count,
                            at_desk: presence.at_desk(),
                            only_held: false,
                        };
                        deliver_alert(&mut trigger, stores, &remote, alert).await;
                    }
                    Step::Nothing => {}
                    Step::End => return,
                },
                changed = desk.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    // The owner left the desk: what was held now arrives,
                    // to the subscriptions that held it only.
                    if !*desk.borrow_and_update() {
                        deliver_held(&mut trigger, &sessions, stores, &remote).await;
                    }
                }
                () = sleep_until(next_finished.unwrap_or_else(Instant::now)), if next_finished.is_some() => {
                    deliver_finished(&mut trigger, &sessions, stores, &presence, &remote).await;
                }
            }
        }
    })
}

/// Deliver `alert` as one round, with the listener's state read just before
/// it, so no watch guard or lock is held across an `.await`.
async fn deliver_alert(
    trigger: &mut Trigger,
    stores: Stores<'_>,
    remote: &Arc<dyn Fn() -> RemoteState + Send + Sync>,
    alert: Alert<'_>,
) {
    let remote_state = remote();
    let answered = trigger
        .deliver(stores, &remote_state, alert, Utc::now())
        .await;
    tracing::debug!(
        session = %alert.session.id,
        kind = ?alert.kind,
        only_held = alert.only_held,
        answered = answered.len(),
        "push: a round is done"
    );
}

/// Deliver the Finished pushes whose grace has ended, each read from the
/// registry just before its round: only a session that still exists, reads
/// `Idle` and shows no background work is sent, with the Needs you count as
/// it stands then.
async fn deliver_finished(
    trigger: &mut Trigger,
    sessions: &SessionRegistry,
    stores: Stores<'_>,
    presence: &Presence,
    remote: &Arc<dyn Fn() -> RemoteState + Send + Sync>,
) {
    for session_id in trigger.finishing.due(Instant::now()) {
        let snapshots = sessions.snapshots();
        let Some(session) = snapshots.iter().find(|seen| {
            seen.id == session_id && seen.status == SessionStatus::Idle && !seen.background
        }) else {
            continue;
        };
        let alert = Alert {
            kind: PushKind::Finished,
            session,
            count: needs_you_count(&snapshots),
            at_desk: presence.at_desk(),
            only_held: false,
        };
        deliver_alert(trigger, stores, remote, alert).await;
    }
}

/// Deliver the held Needs you pushes once the owner left the desk, to the
/// subscriptions whose `hold_at_desk` is on only: the others got the push
/// when it was raised. A session that no longer needs the user or is gone
/// sends nothing. Nothing is queued once the held list is cleared.
async fn deliver_held(
    trigger: &mut Trigger,
    sessions: &SessionRegistry,
    stores: Stores<'_>,
    remote: &Arc<dyn Fn() -> RemoteState + Send + Sync>,
) {
    let sessions_due = trigger.held_now(sessions);
    trigger.held.clear();
    for session in &sessions_due {
        let snapshots = sessions.snapshots();
        if !snapshots.iter().any(|seen| seen.id == session.id) {
            continue;
        }
        let alert = Alert {
            kind: PushKind::NeedsYou,
            session,
            count: needs_you_count(with_snapshot(snapshots, session).iter()),
            at_desk: false,
            only_held: true,
        };
        deliver_alert(trigger, stores, remote, alert).await;
    }
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
    use push_crypto::Subscriber;
    use reqwest::Url;
    use tokio::sync::broadcast::error::RecvError;
    use tokio::time::Instant;

    use protocol::{PUSH_NOTICE_VERSION, PushNotice, PushNoticeKind};

    use super::{
        Alert, BACKOFF_FIRST, BACKOFF_MOST, Backoff, Delivery, FINISHED_GRACE, FinishedHolds,
        PushKind, Sources, Step, Stores, Tracker, Trigger, deliver_finished, deliver_held, label,
        lock_push, needs_you_count, notice, origin, payload, run, spawn,
    };
    use crate::devices::DeviceStore;
    use crate::paths::Dirs;
    use crate::presence::Presence;
    use crate::push::relay;
    use crate::push::send::{Outcome, client, topic};
    use crate::push::state::{PushState, unrevoked_ids};
    use crate::push::store::{
        ApnsEnvironment, Channel, SUBSCRIPTIONS_FILE, Subscription, SubscriptionStore,
    };
    use crate::push::test_fake::{
        Fake, Server, TESTS, fake_push_service, fake_push_service_by_path, fake_push_service_gated,
        fake_push_service_hooked, fake_push_service_with, origin_of,
    };
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

    /// [`relay_setup`] with a relay origin nothing listens on: a test that
    /// never stores a relay subscription posts nothing to it.
    fn setup(tag: &str) -> Setup {
        relay_setup(tag, &Url::parse("http://127.0.0.1:9").expect("a URL"))
    }

    /// A [`Setup`] whose push state posts relay pushes under `relay`, a fake
    /// relay's loopback origin.
    fn relay_setup(tag: &str, relay: &Url) -> Setup {
        let dir =
            std::env::temp_dir().join(format!("rt-push-trigger-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        let devices = DeviceStore::load(&dir.join("devices.json"));
        let push = PushState::new(Some(dir.join("push")), TESTS, relay.clone());
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
            issue(&self.devices, n)
        }

        /// Whether `subscriptions.json` holds a subscription for
        /// `device_id`, read from the file without loading the state.
        fn filed(&self, device_id: &str) -> bool {
            SubscriptionStore::load(&self.dir.join("push").join(SUBSCRIPTIONS_FILE))
                .get(device_id)
                .is_some()
        }

        fn subscribe(&self, device_id: &str, endpoint: &str, needs_you: bool, finished: bool) {
            self.subscribe_holding(device_id, endpoint, needs_you, finished, true);
        }

        /// [`Setup::subscribe`] with the at-desk hold switch set.
        fn subscribe_holding(
            &self,
            device_id: &str,
            endpoint: &str,
            needs_you: bool,
            finished: bool,
            hold_at_desk: bool,
        ) {
            put(
                &self.devices,
                &self.push,
                device_id,
                endpoint,
                needs_you,
                finished,
                hold_at_desk,
            );
        }

        /// Store an `apns` subscription for `device_id` (both switches on,
        /// held at the desk); returns the phone's keys.
        fn put_apns(&self, device_id: &str, token: &str) -> PhoneKeys {
            let keys = phone_keys();
            self.store(
                device_id,
                Channel::Apns {
                    token: token.to_owned(),
                    environment: ApnsEnvironment::Production,
                    p256dh: keys.p256dh.clone(),
                    auth: keys.auth_b64.clone(),
                },
            );
            keys
        }

        /// Store an `fcm` subscription for `device_id`; returns the phone's
        /// keys.
        fn put_fcm(&self, device_id: &str, token: &str) -> PhoneKeys {
            let keys = phone_keys();
            self.store(
                device_id,
                Channel::Fcm {
                    token: token.to_owned(),
                    p256dh: keys.p256dh.clone(),
                    auth: keys.auth_b64.clone(),
                },
            );
            keys
        }

        fn store(&self, device_id: &str, channel: Channel) {
            let devices = lock(&self.devices);
            let mut push = lock(&self.push);
            push.load(unrevoked_ids(&devices).as_ref())
                .expect("load the push state")
                .subs
                .put(
                    device_id,
                    Subscription {
                        channel,
                        needs_you: true,
                        finished: true,
                        hold_at_desk: true,
                        created_at: "2026-11-07T00:00:00Z".to_owned(),
                    },
                )
                .expect("store the subscription");
        }

        /// The daemon's VAPID public key, base64url.
        fn public_key(&self) -> String {
            let devices = lock(&self.devices);
            let mut push = lock(&self.push);
            push.load(unrevoked_ids(&devices).as_ref())
                .expect("load the push state")
                .key
                .public_b64url()
        }

        fn endpoint_of(&self, device_id: &str) -> Option<String> {
            let devices = lock(&self.devices);
            let mut push = lock(&self.push);
            let loaded = push
                .load(unrevoked_ids(&devices).as_ref())
                .expect("load the push state");
            loaded
                .subs
                .get(device_id)
                .map(|sub| sub.channel.address().to_owned())
        }
    }

    /// Pair (or pair again) the device of peer `n`; returns its id.
    fn issue(devices: &Mutex<DeviceStore>, n: u32) -> String {
        let peer = tailnet::PeerIdentity {
            stable_id: format!("nPeer{n}"),
            name: format!("phone{n}.tail1234.ts.net"),
            user_id: 2,
            login: "owner@example.com".to_owned(),
        };
        lock(devices)
            .issue(&peer, Utc::now())
            .expect("issue a device token")
            .0
    }

    /// Block `device_id` as the pairing path does: revoke its record, then
    /// delete its subscription from the push state.
    fn block(devices: &Mutex<DeviceStore>, push: &Mutex<PushState>, device_id: &str) {
        lock(devices).revoke(device_id).expect("revoke");
        lock(push).remove_device(device_id);
    }

    fn put(
        devices: &Mutex<DeviceStore>,
        push: &Mutex<PushState>,
        device_id: &str,
        endpoint: &str,
        needs_you: bool,
        finished: bool,
        hold_at_desk: bool,
    ) {
        let secret = p256::SecretKey::try_generate().expect("a P-256 key");
        let p256dh = URL_SAFE_NO_PAD.encode(secret.public_key().to_sec1_point(false).as_bytes());
        let auth = URL_SAFE_NO_PAD.encode(<[u8; 16]>::try_generate().expect("16 random bytes"));
        let devices = lock(devices);
        let mut push = lock(push);
        push.load(unrevoked_ids(&devices).as_ref())
            .expect("load the push state")
            .subs
            .put(
                device_id,
                Subscription {
                    channel: Channel::WebPush {
                        endpoint: endpoint.to_owned(),
                        p256dh,
                        auth,
                    },
                    needs_you,
                    finished,
                    hold_at_desk,
                    created_at: "2026-11-07T00:00:00Z".to_owned(),
                },
            )
            .expect("store the subscription");
    }

    /// A phone's key pair and auth secret, the private halves kept so a
    /// test can decrypt what it was sent.
    struct PhoneKeys {
        secret: [u8; 32],
        auth: [u8; 16],
        p256dh: String,
        auth_b64: String,
    }

    fn phone_keys() -> PhoneKeys {
        let secret = p256::SecretKey::try_generate().expect("a P-256 key");
        let auth = <[u8; 16]>::try_generate().expect("16 random bytes");
        PhoneKeys {
            p256dh: URL_SAFE_NO_PAD.encode(secret.public_key().to_sec1_point(false).as_bytes()),
            auth_b64: URL_SAFE_NO_PAD.encode(auth),
            secret: secret.to_bytes().into(),
            auth,
        }
    }

    /// The notice a relay POST's `body` decrypts to with `keys`.
    fn opened(body: &[u8], keys: &PhoneKeys) -> PushNotice {
        let plaintext =
            push_crypto::decrypt(body, &keys.secret, &keys.auth).expect("the body decrypts");
        serde_json::from_slice(&plaintext).expect("a push notice")
    }

    /// The `t=` JWT's claims and the `k=` key of a POST's `Authorization`.
    fn vapid(headers: &axum::http::HeaderMap) -> (serde_json::Value, String) {
        let authorization = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .expect("an Authorization header");
        let rest = authorization.strip_prefix("vapid t=").expect("vapid t=");
        let (jwt, key) = rest.split_once(", k=").expect(", k=");
        let claims = jwt.split('.').nth(1).expect("a claims part");
        let claims = URL_SAFE_NO_PAD.decode(claims).expect("base64url claims");
        (
            serde_json::from_slice(&claims).expect("JSON claims"),
            key.to_owned(),
        )
    }

    /// A paired device with an `apns` subscription for `token` on a fake
    /// relay answering `code`.
    async fn one_phone(tag: &str, token: &str, code: u16) -> (Setup, String, Arc<Fake>, Server) {
        let (url, fake, server) = fake_push_service(code, "").await;
        let setup = relay_setup(tag, &origin_of(&url));
        let device = setup.pair(1);
        setup.put_apns(&device, token);
        (setup, device, fake, server)
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
                    at_desk: false,
                    only_held: false,
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

    /// Two paired devices, each on its own fake push service: the first held
    /// at the desk, the second not. Drives one round with `at_desk` and
    /// returns how many POSTs each fake saw.
    async fn posts_at_the_desk(tag: &str, at_desk: bool) -> (usize, usize) {
        let setup = setup(tag);
        let held = setup.pair(1);
        let sent = setup.pair(2);
        let (held_url, held_fake, _held_server) = fake_push_service(201, "").await;
        let (sent_url, sent_fake, _sent_server) = fake_push_service(201, "").await;
        setup.subscribe_holding(&held, held_url.as_str(), true, true, true);
        setup.subscribe_holding(&sent, sent_url.as_str(), true, true, false);

        let session = session();
        trigger()
            .deliver(
                setup.stores(),
                &serving(),
                Alert {
                    kind: PushKind::NeedsYou,
                    session: &session,
                    count: 1,
                    at_desk,
                    only_held: false,
                },
                Utc::now(),
            )
            .await;

        (
            held_fake.seen.lock().expect("unpoisoned").len(),
            sent_fake.seen.lock().expect("unpoisoned").len(),
        )
    }

    #[tokio::test]
    async fn a_held_subscription_gets_nothing_at_the_desk() {
        let (held_posts, sent_posts) = posts_at_the_desk("hold-at-desk", true).await;

        assert_eq!(held_posts, 0, "the held device gets no POST at the desk");
        assert_eq!(sent_posts, 1, "the device that is not held gets its POST");
    }

    #[tokio::test]
    async fn nothing_is_held_away_from_the_desk() {
        let (held_posts, sent_posts) = posts_at_the_desk("hold-away", false).await;

        assert_eq!(held_posts, 1, "away from the desk the held device gets one");
        assert_eq!(sent_posts, 1, "and so does the other device");
    }

    #[tokio::test]
    async fn a_held_session_removed_during_an_earlier_held_push_is_skipped() {
        let setup = setup("held-removed");
        let device = setup.pair(1);
        let sessions = registry(&setup);
        for id in ["s1", "s2"] {
            sessions.insert(idle_record(id));
            sessions.update(id, |record| record.status = SessionStatus::AwaitingInput);
        }
        let mut trigger = trigger();
        trigger.held.extend(["s1".to_owned(), "s2".to_owned()]);
        // The held list's order decides which session is delivered first;
        // the fake removes the other one while that round is in flight.
        let due = trigger.held_now(&sessions);
        assert_eq!(due.len(), 2, "both held sessions are due");
        let second = due[1].id.clone();
        let hook = {
            let sessions = Arc::clone(&sessions);
            Box::new(move || sessions.remove(&second))
        };
        let (url, fake, _server) = fake_push_service_hooked(201, hook).await;
        setup.subscribe(&device, url.as_str(), true, true);
        let remote: Arc<dyn Fn() -> RemoteState + Send + Sync> = Arc::new(serving);

        deliver_held(&mut trigger, &sessions, setup.stores(), &remote).await;

        assert_eq!(
            fake.posts(),
            1,
            "the session removed during the first push is not pushed to"
        );
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
    async fn collect_skips_a_revoked_device() {
        let (setup, device, _url, fake, _server) = one_device("revoked", 201).await;
        // Revoked, with its subscription still stored: a push request's
        // write that a block raced, before the request's re-check undoes it.
        lock(&setup.devices).revoke(&device).expect("revoke");

        let delivered = round(&mut trigger(), &setup, &serving(), PushKind::Finished).await;

        assert!(delivered.is_empty(), "{delivered:?}");
        assert_eq!(fake.posts(), 0, "no POST to a revoked device");
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
            Box::new(move || put(&devices, &push, &device, fresh, true, true, true))
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

    #[test]
    fn the_round_holds_no_device_lock_during_its_push_work() {
        let setup = setup("lock-push");

        let (_paired, push) = lock_push(setup.stores());

        assert!(
            setup.devices.try_lock().is_ok(),
            "devices is free while the round holds push"
        );
        drop(push);
    }

    #[tokio::test]
    async fn a_round_after_a_block_sends_the_device_nothing() {
        let (setup, device, _url, fake, _server) = one_device("after-block", 201).await;
        block(&setup.devices, &setup.push, &device);

        let delivered = round(&mut trigger(), &setup, &serving(), PushKind::NeedsYou).await;

        assert!(delivered.is_empty(), "{delivered:?}");
        assert_eq!(fake.posts(), 0, "no POST to a blocked device");
        assert!(!setup.filed(&device), "the file no longer holds the device");
        assert_eq!(setup.endpoint_of(&device), None, "nor does memory");
    }

    #[tokio::test]
    async fn a_round_that_loads_the_state_sweeps_a_device_blocked_while_unloaded() {
        let (setup, device, _url, fake, _server) = one_device("blocked-unloaded", 201).await;
        lock(&setup.push).unload();
        block(&setup.devices, &setup.push, &device);
        assert!(
            setup.filed(&device),
            "the block found the state unloaded, so the file still holds the device"
        );

        let delivered = round(&mut trigger(), &setup, &serving(), PushKind::NeedsYou).await;

        assert!(delivered.is_empty(), "{delivered:?}");
        assert_eq!(fake.posts(), 0, "no POST to a blocked device");
        assert!(!setup.filed(&device), "the round's load swept the device");
    }

    /// A device blocked by the fake's hook while its POST is in flight, the
    /// fake answering `code`; checks that only that POST went out, that the
    /// device's subscription is gone and that a second round sends nothing.
    /// Returns the first round's outcome.
    async fn block_during_the_post(tag: &str, code: u16) -> Outcome {
        let setup = setup(tag);
        let device = setup.pair(1);
        let hook = {
            let devices = Arc::clone(&setup.devices);
            let push = Arc::clone(&setup.push);
            let device = device.clone();
            Box::new(move || block(&devices, &push, &device))
        };
        let (url, fake, _server) = fake_push_service_hooked(code, hook).await;
        setup.subscribe(&device, url.as_str(), true, true);
        let mut trigger = trigger();

        let delivered = round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;

        assert_eq!(delivered.len(), 1, "{delivered:?}");
        assert_eq!(fake.posts(), 1, "only the POST in flight at the block");
        assert!(lock(&setup.push).is_loaded(), "the state stays loaded");
        assert!(!setup.filed(&device), "the file no longer holds the device");
        assert_eq!(setup.endpoint_of(&device), None, "nor does memory");
        let again = round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;
        assert!(again.is_empty(), "{again:?}");
        assert_eq!(fake.posts(), 1, "no later push to the blocked device");
        delivered[0].outcome.clone()
    }

    #[tokio::test]
    async fn a_block_during_the_post_leaves_no_subscription_and_no_later_push() {
        let outcome = block_during_the_post("block-during-post", 201).await;

        assert_eq!(outcome, Outcome::Sent);
    }

    #[tokio::test]
    async fn a_block_during_the_post_leaves_no_subscription_and_no_later_push_with_a_gone_answer() {
        // `remove_if_address` finds nothing to delete and reports no error.
        let outcome = block_during_the_post("block-during-post-gone", 410).await;

        assert_eq!(outcome, Outcome::Gone);
    }

    #[tokio::test]
    async fn a_gone_answer_that_reloads_the_state_sweeps_a_device_blocked_meanwhile() {
        let setup = setup("gone-reload-sweep");
        let resting = setup.pair(1);
        let target = setup.pair(2);
        let hook = {
            let devices = Arc::clone(&setup.devices);
            let push = Arc::clone(&setup.push);
            let resting = resting.clone();
            Box::new(move || {
                lock(&push).unload();
                block(&devices, &push, &resting);
            })
        };
        let (url, fake, _server) = fake_push_service_hooked(410, hook).await;
        setup.subscribe(&resting, url.as_str(), false, true);
        setup.subscribe(&target, url.as_str(), true, true);

        let delivered = round(&mut trigger(), &setup, &serving(), PushKind::NeedsYou).await;

        assert_eq!(delivered.len(), 1, "{delivered:?}");
        assert_eq!(delivered[0].outcome, Outcome::Gone);
        assert_eq!(
            fake.posts(),
            1,
            "only the device wanting Needs you is pushed"
        );
        assert!(
            !setup.filed(&resting),
            "the reload's sweep dropped the blocked device"
        );
        assert!(!setup.filed(&target), "the gone answer dropped the target");
        assert_eq!(setup.endpoint_of(&resting), None);
        assert_eq!(setup.endpoint_of(&target), None);
    }

    #[tokio::test]
    async fn a_re_pair_during_the_post_leaves_no_subscription_for_the_old_record() {
        let setup = setup("re-pair-during-post");
        let old = setup.pair(1);
        let hook = {
            let devices = Arc::clone(&setup.devices);
            let push = Arc::clone(&setup.push);
            let old = old.clone();
            Box::new(move || {
                let new = issue(&devices, 1);
                assert_ne!(new, old, "the re-pair replaced the record");
                lock(&push).remove_device(&old);
            })
        };
        let (url, fake, _server) = fake_push_service_hooked(201, hook).await;
        setup.subscribe(&old, url.as_str(), true, true);
        let mut trigger = trigger();

        let delivered = round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;

        assert_eq!(delivered.len(), 1, "{delivered:?}");
        assert_eq!(fake.posts(), 1);
        assert!(
            !setup.filed(&old),
            "the file no longer holds the old record"
        );
        assert_eq!(setup.endpoint_of(&old), None, "nor does memory");
        let again = round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;
        assert!(again.is_empty(), "{again:?}");
        assert_eq!(fake.posts(), 1, "nothing more is sent to the old record");
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
    async fn a_slow_push_service_holds_no_other_device() {
        let setup = setup("slow-service");
        let first = setup.pair(1);
        let second = setup.pair(2);
        // A round posts in device-id order and the ids are random, so the
        // gated service goes on whichever device sorts first.
        let (gated_device, other_device) = if first < second {
            (first, second)
        } else {
            (second, first)
        };
        let gate = Arc::new(tokio::sync::Notify::new());
        let (gated_url, gated_fake, _gated_server) =
            fake_push_service_gated(201, Arc::clone(&gate)).await;
        let released = Arc::clone(&gate);
        let (url, fake, _server) =
            fake_push_service_hooked(201, Box::new(move || released.notify_one())).await;
        setup.subscribe(&gated_device, gated_url.as_str(), true, true);
        setup.subscribe(&other_device, url.as_str(), true, true);

        let delivered = round(&mut trigger(), &setup, &serving(), PushKind::NeedsYou).await;

        assert_eq!(delivered.len(), 2, "{delivered:?}");
        assert!(
            delivered
                .iter()
                .all(|delivery| delivery.outcome == Outcome::Sent),
            "{delivered:?}"
        );
        assert_eq!(
            delivered
                .iter()
                .map(|delivery| delivery.device_id.as_str())
                .collect::<Vec<_>>(),
            [gated_device.as_str(), other_device.as_str()],
            "the answers come back in target order, though the first one answered last"
        );
        assert_eq!(gated_fake.posts(), 1, "the gated service was posted to");
        assert_eq!(fake.posts(), 1, "the other device got its POST too");
    }

    #[tokio::test]
    async fn a_sent_and_a_throttled_answer_back_the_service_off() {
        let setup = setup("sent-and-throttled");
        let first = setup.pair(1);
        let second = setup.pair(2);
        let (url, fake, _server) =
            fake_push_service_by_path(&[("one", 201, &[]), ("two", 429, &[])]).await;
        let one = url.join("one").expect("a URL");
        let two = url.join("two").expect("a URL");
        setup.subscribe(&first, one.as_str(), true, true);
        setup.subscribe(&second, two.as_str(), true, true);
        let mut trigger = trigger();

        let delivered = round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;

        assert_eq!(delivered.len(), 2, "both targets were in flight");
        assert_eq!(fake.posts(), 2);
        let backoff = trigger
            .backoff
            .get(&audience(&url))
            .expect("the push service is backed off");
        assert_eq!(
            backoff.delay, BACKOFF_FIRST,
            "a throttled answer wins over a sent one"
        );
    }

    #[tokio::test]
    async fn the_longest_retry_after_wins() {
        let setup = setup("longest-retry-after");
        let first = setup.pair(1);
        let second = setup.pair(2);
        let (url, fake, _server) = fake_push_service_by_path(&[
            ("one", 429, &[("retry-after", "60")]),
            ("two", 429, &[("retry-after", "120")]),
        ])
        .await;
        let one = url.join("one").expect("a URL");
        let two = url.join("two").expect("a URL");
        setup.subscribe(&first, one.as_str(), true, true);
        setup.subscribe(&second, two.as_str(), true, true);
        let mut trigger = trigger();

        let before = Instant::now();
        let delivered = round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;

        assert_eq!(delivered.len(), 2);
        assert_eq!(fake.posts(), 2);
        let backoff = trigger
            .backoff
            .get(&audience(&url))
            .expect("the push service is backed off");
        assert!(
            backoff.until >= before + Duration::from_secs(120),
            "the longest retry-after decides the wait"
        );
    }

    #[tokio::test]
    async fn two_throttled_answers_on_one_push_service_back_it_off_once() {
        let setup = setup("throttled-twice");
        let first = setup.pair(1);
        let second = setup.pair(2);
        let (url, fake, _server) = fake_push_service(429, "").await;
        let one = url.join("one").expect("a URL");
        let two = url.join("two").expect("a URL");
        setup.subscribe(&first, one.as_str(), true, true);
        setup.subscribe(&second, two.as_str(), true, true);
        let mut trigger = trigger();

        let delivered = round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;

        assert_eq!(delivered.len(), 2, "both targets were in flight");
        assert_eq!(fake.posts(), 2);
        let backoff = trigger
            .backoff
            .get(&audience(&url))
            .expect("the push service is backed off");
        assert_eq!(
            backoff.delay, BACKOFF_FIRST,
            "one back-off for the round, not one per answer"
        );
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

    // ---- relay pushes against a fake relay ----

    const TOKEN_A: &str = "0a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9";
    const TOKEN_B: &str = "f9e8d7c6b5a4938271605f4e3d2c1b0af9e8d7c6b5a4938271605f4e3d2c1b0a";

    #[tokio::test]
    async fn an_apns_subscription_posts_the_notice_to_the_relay() {
        let (url, fake, _server) = fake_push_service(201, "").await;
        let relay_origin = origin_of(&url);
        let setup = relay_setup("relay-apns", &relay_origin);
        let device = setup.pair(1);
        let keys = setup.put_apns(&device, TOKEN_A);

        let delivered = round(&mut trigger(), &setup, &serving(), PushKind::NeedsYou).await;

        assert_eq!(
            delivered,
            vec![Delivery {
                device_id: device,
                outcome: Outcome::Sent
            }]
        );
        assert_eq!(fake.paths(), [format!("/v1/apns/production/{TOKEN_A}")]);
        let seen = fake.seen.lock().expect("unpoisoned");
        let (headers, body) = &seen[0];
        let (claims, key) = vapid(headers);
        assert_eq!(key, setup.public_key());
        assert_eq!(claims["aud"], relay_origin.origin().ascii_serialization());
        assert_eq!(claims["sub"], relay::SUBJECT);
        assert_eq!(
            opened(body, &keys),
            PushNotice {
                v: PUSH_NOTICE_VERSION,
                kind: PushNoticeKind::NeedsYou,
                title: "Needs you".to_owned(),
                body: "parser".to_owned(),
                session_id: "s1".to_owned(),
                host: Some("host.tail1234.ts.net".to_owned()),
                badge: 1,
            }
        );
    }

    #[tokio::test]
    async fn an_fcm_subscription_is_not_pushed_until_the_relay_sends_fcm() {
        let (url, fake, _server) = fake_push_service(404, "").await;
        let setup = relay_setup("relay-fcm", &origin_of(&url));
        let device = setup.pair(1);
        setup.put_fcm(&device, "cZ1:APA91bHPRgkF");

        let delivered = round(&mut trigger(), &setup, &serving(), PushKind::Finished).await;

        assert_eq!(delivered, Vec::new());
        assert_eq!(
            fake.posts(),
            0,
            "nothing is posted to the relay's fcm route"
        );
        assert_eq!(
            setup.endpoint_of(&device).as_deref(),
            Some("cZ1:APA91bHPRgkF"),
            "the subscription is kept"
        );
    }

    #[tokio::test]
    async fn a_back_off_survives_a_round_that_skips_its_subscription() {
        let setup = setup("backoff-skipped");
        let device = setup.pair(1);
        let (url, fake, _server) = fake_push_service(429, "").await;
        setup.subscribe(&device, url.as_str(), true, false);
        let mut trigger = trigger();
        let service = audience(&url);
        round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;
        trigger.backoff.get_mut(&service).expect("backed off").until = Instant::now();

        let finished = round(&mut trigger, &setup, &serving(), PushKind::Finished).await;

        assert_eq!(finished, Vec::new(), "the finished switch is off");
        assert_eq!(
            trigger.backoff.get(&service).map(|backoff| backoff.delay),
            Some(BACKOFF_FIRST),
            "the run-out back-off of a stored subscription is kept"
        );
        round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;
        assert_eq!(fake.posts(), 2);
        assert_eq!(
            trigger.backoff.get(&service).map(|backoff| backoff.delay),
            Some(BACKOFF_FIRST * 2),
            "the next throttle doubles the kept delay"
        );
    }

    #[tokio::test]
    async fn a_failure_count_survives_a_round_without_a_serving_listener() {
        let setup = setup("failures-not-serving");
        let first = setup.pair(1);
        let second = setup.pair(2);
        let (url, fake, _server) =
            fake_push_service_by_path(&[("one", 201, &[]), ("two", 400, &[])]).await;
        let two = url.join("two").expect("a URL");
        setup.subscribe(&first, url.join("one").expect("a URL").as_str(), true, true);
        setup.subscribe(&second, two.as_str(), true, true);
        let mut trigger = trigger();
        round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;
        let key = (second.clone(), two.as_str().to_owned());
        assert_eq!(trigger.failures.get(&key), Some(&1), "a counted failure");

        let delivered = round(
            &mut trigger,
            &setup,
            &RemoteState::Unknown,
            PushKind::NeedsYou,
        )
        .await;

        assert_eq!(delivered, Vec::new());
        assert_eq!(fake.posts(), 2);
        assert_eq!(
            trigger.failures.get(&key),
            Some(&1),
            "the count is unchanged"
        );
    }

    #[tokio::test]
    async fn a_relay_gone_answer_deletes_the_subscription() {
        let (setup, device, fake, _server) = one_phone("relay-gone", TOKEN_A, 410).await;

        let delivered = round(&mut trigger(), &setup, &serving(), PushKind::NeedsYou).await;

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
    async fn a_throttled_relay_token_backs_off_only_that_phone() {
        let (url, fake, _server) =
            fake_push_service_by_path(&[(TOKEN_A, 429, &[]), (TOKEN_B, 201, &[])]).await;
        let setup = relay_setup("relay-throttled", &origin_of(&url));
        let throttled = setup.pair(1);
        let other = setup.pair(2);
        setup.put_apns(&throttled, TOKEN_A);
        setup.put_apns(&other, TOKEN_B);
        let mut trigger = trigger();

        let first = round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;
        assert_eq!(first.len(), 2, "{first:?}");
        assert_eq!(fake.posts(), 2);

        let second = round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;

        assert_eq!(
            second,
            vec![Delivery {
                device_id: other,
                outcome: Outcome::Sent
            }],
            "the throttled phone is backed off, the other is not"
        );
        assert_eq!(fake.posts(), 3);
        assert_eq!(
            fake.paths().last().map(String::as_str),
            Some(format!("/v1/apns/production/{TOKEN_B}").as_str())
        );
    }

    #[tokio::test]
    async fn a_relay_503_leaves_web_push_targets_alone() {
        let (relay_url, relay_fake, _relay_server) = fake_push_service(503, "").await;
        let (url, fake, _server) = fake_push_service(201, "").await;
        let setup = relay_setup("relay-503", &origin_of(&relay_url));
        let phone = setup.pair(1);
        let browser = setup.pair(2);
        setup.put_apns(&phone, TOKEN_A);
        setup.subscribe(&browser, url.as_str(), true, true);
        let mut trigger = trigger();

        let first = round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;
        assert_eq!(first.len(), 2, "{first:?}");

        let second = round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;

        assert_eq!(
            second,
            vec![Delivery {
                device_id: browser,
                outcome: Outcome::Sent
            }]
        );
        assert_eq!(relay_fake.posts(), 1, "the relay token is backed off");
        assert_eq!(fake.posts(), 2, "the Web Push target is still posted");
    }

    #[tokio::test]
    async fn with_the_listener_not_serving_only_relay_targets_are_pushed() {
        let (relay_url, relay_fake, _relay_server) = fake_push_service(201, "").await;
        let (url, fake, _server) = fake_push_service(201, "").await;
        let setup = relay_setup("relay-not-serving", &origin_of(&relay_url));
        let phone = setup.pair(1);
        let browser = setup.pair(2);
        let keys = setup.put_apns(&phone, TOKEN_A);
        setup.subscribe(&browser, url.as_str(), true, true);

        let delivered = round(
            &mut trigger(),
            &setup,
            &RemoteState::Unknown,
            PushKind::NeedsYou,
        )
        .await;

        assert_eq!(
            delivered,
            vec![Delivery {
                device_id: phone,
                outcome: Outcome::Sent
            }]
        );
        assert_eq!(
            fake.posts(),
            0,
            "no Web Push without the phone app's origin"
        );
        assert_eq!(relay_fake.posts(), 1);
        let seen = relay_fake.seen.lock().expect("unpoisoned");
        assert_eq!(opened(&seen[0].1, &keys).host, None);
    }

    #[tokio::test]
    async fn an_expired_back_off_for_a_gone_token_is_pruned() {
        let (url, _fake, _server) =
            fake_push_service_by_path(&[(TOKEN_A, 503, &[]), (TOKEN_B, 201, &[])]).await;
        let setup = relay_setup("relay-pruned", &origin_of(&url));
        let device = setup.pair(1);
        setup.put_apns(&device, TOKEN_A);
        let mut trigger = trigger();
        round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;
        let keys: Vec<String> = trigger.backoff.keys().cloned().collect();
        assert_eq!(keys.len(), 1, "{keys:?}");
        let gone = keys[0].clone();
        assert!(!gone.contains(TOKEN_A), "the key never carries the token");
        trigger.backoff.get_mut(&gone).expect("backed off").until = Instant::now();
        let running = "https://gone.example#000000000000".to_owned();
        trigger.backoff.insert(
            running.clone(),
            Backoff {
                until: Instant::now() + Duration::from_secs(60),
                delay: BACKOFF_FIRST,
            },
        );

        setup.put_apns(&device, TOKEN_B);
        round(&mut trigger, &setup, &serving(), PushKind::NeedsYou).await;

        assert!(
            !trigger.backoff.contains_key(&gone),
            "the run-out back-off of the replaced token is pruned"
        );
        assert!(
            trigger.backoff.contains_key(&running),
            "a back-off still running is kept"
        );
    }

    #[test]
    fn the_notice_carries_the_alert() {
        let session = session();

        let built = notice(PushKind::Finished, &session, None, 3);

        assert_eq!(
            built,
            PushNotice {
                v: PUSH_NOTICE_VERSION,
                kind: PushNoticeKind::Finished,
                title: "Finished".to_owned(),
                body: "parser".to_owned(),
                session_id: "s1".to_owned(),
                host: None,
                badge: 3,
            }
        );
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
        push_crypto::encrypt(&subscriber, body.as_bytes()).expect("the payload fits one record");
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
    fn an_idle_session_with_a_linked_run_is_not_finished() {
        let setup = setup("linked-run");
        let sessions = registry(&setup);
        let mut linked = idle_record("s1");
        linked.conversation_id = Some("c1".to_owned());
        sessions.insert(linked);
        let read = || {
            let record = sessions.get("s1").expect("s1 is registered");
            lock(&record).snapshot()
        };

        sessions.dispatch_links().link("c1");
        let holding = read();
        assert!(holding.background, "the link reads as background work");
        assert_eq!(raised(&status("working"), &holding), None);

        sessions.dispatch_links().unlink("c1");
        assert_eq!(raised(&holding, &read()), Some(PushKind::Finished));
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
                session: Box::new(asking),
                count: 1,
            }
        );
    }

    #[test]
    fn event_step_holds_a_finished_turn_through_its_grace() {
        let setup = setup("step-finished-grace");
        let sessions = registry(&setup);
        let mut trigger = trigger();
        trigger.tracker.seed([&status("working")]);
        let (_, idle) = update("idle");
        assert_eq!(
            trigger.step(idle, &sessions),
            Step::Nothing,
            "Finished raised the moment the session read idle"
        );
    }

    /// A trigger holding `s1`'s Finished, raised by a step over `sessions`.
    fn holding_finished(sessions: &SessionRegistry) -> Trigger {
        let mut trigger = trigger();
        trigger.tracker.seed([&status("working")]);
        let (_, idle) = update("idle");
        assert_eq!(trigger.step(idle, sessions), Step::Nothing);
        trigger
    }

    /// An instant past the grace of any hold taken before this call.
    fn past_the_grace() -> Instant {
        Instant::now() + FINISHED_GRACE
    }

    #[test]
    fn a_held_finished_is_due_after_its_grace() {
        let start = Instant::now();
        let until = start + FINISHED_GRACE;
        let mut holds = FinishedHolds::default();
        holds.hold("s1", until);

        assert_eq!(holds.next_due(), Some(until));
        assert!(
            holds.due(until - Duration::from_millis(1)).is_empty(),
            "due inside its grace"
        );
        assert_eq!(holds.due(until), vec!["s1".to_owned()]);
        assert!(holds.due(until).is_empty(), "sent once");
        assert_eq!(holds.next_due(), None);

        let setup = setup("due-after-grace");
        let sessions = registry(&setup);
        let mut trigger = holding_finished(&sessions);
        assert_eq!(
            trigger.finishing.due(past_the_grace()),
            vec!["s1".to_owned()],
            "the step holds what it did not raise"
        );
    }

    #[test]
    fn a_held_finished_is_dropped_when_the_session_works_again() {
        let setup = setup("due-works-again");
        let sessions = registry(&setup);
        let mut trigger = holding_finished(&sessions);

        let (_, working) = update("working");
        assert_eq!(trigger.step(working, &sessions), Step::Nothing);

        assert_eq!(
            trigger.finishing.due(past_the_grace()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_held_finished_is_dropped_when_background_work_starts() {
        let setup = setup("due-background");
        let sessions = registry(&setup);
        let mut trigger = holding_finished(&sessions);

        let holding = snapshot(&serde_json::json!({ "status": "idle", "background": true }));
        let event = Ok(SessionEvent::Updated(Box::new(holding), None));
        assert_eq!(trigger.step(event, &sessions), Step::Nothing);

        assert_eq!(
            trigger.finishing.due(past_the_grace()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_lag_keeps_a_finished_that_is_still_idle() {
        let setup = setup("due-lag");
        let sessions = registry(&setup);
        let mut trigger = holding_finished(&sessions);

        assert_eq!(
            trigger.step(Err(RecvError::Lagged(1)), &sessions),
            Step::Nothing
        );

        assert_eq!(trigger.finishing.due(past_the_grace()).len(), 1);
    }

    #[test]
    fn a_removed_sessions_held_finished_is_forgotten() {
        let setup = setup("due-removed");
        let sessions = registry(&setup);
        let mut trigger = holding_finished(&sessions);

        let removed = Ok(SessionEvent::Removed("s1".to_owned()));
        assert_eq!(trigger.step(removed, &sessions), Step::Nothing);
        assert_eq!(
            trigger.finishing.due(past_the_grace()),
            Vec::<String>::new()
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
                session: Box::new(asking),
                count: 1,
            }
        );
    }

    #[test]
    fn event_step_skips_a_session_the_registry_no_longer_holds() {
        let setup = setup("step-gone");
        let sessions = registry(&setup);
        let mut trigger = trigger();
        trigger.tracker.seed([&status("working")]);

        // The session was removed from the registry while its `Removed`
        // event still sits behind this update, so the registry no longer
        // holds it: no alert is raised for a session that is gone.
        let (_, event) = update("awaiting_input");

        assert!(matches!(trigger.step(event, &sessions), Step::Nothing));
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
        presence: &Arc<Presence>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Sources {
        Sources {
            sessions: Arc::clone(sessions),
            devices: Arc::clone(&setup.devices),
            push: Arc::clone(&setup.push),
            presence: Arc::clone(presence),
            remote: Arc::new(serving),
            shutdown,
        }
    }

    #[tokio::test]
    async fn a_due_finished_is_not_sent_when_the_session_works_again_by_delivery() {
        let (setup, _device, _url, fake, _server) = one_device("due-at-delivery", 201).await;
        let sessions = registry(&setup);
        sessions.insert(idle_record("s1"));
        sessions.update("s1", |record| record.status = SessionStatus::Working);
        let presence = Presence::default();
        let remote: Arc<dyn Fn() -> RemoteState + Send + Sync> = Arc::new(serving);
        let mut trigger = trigger();

        trigger.finishing.hold("s1", Instant::now());
        deliver_finished(&mut trigger, &sessions, setup.stores(), &presence, &remote).await;
        assert_eq!(fake.posts(), 0, "the registry says the session works");

        sessions.update("s1", |record| record.status = SessionStatus::Idle);
        trigger.finishing.hold("s1", Instant::now());
        deliver_finished(&mut trigger, &sessions, setup.stores(), &presence, &remote).await;
        assert_eq!(fake.posts(), 1, "the registry says the session is idle");
    }

    /// The push task with no Finished grace: its pushes go on a real socket,
    /// which a paused clock would not wait for.
    fn run_with_no_grace(sources: Sources) -> tokio::task::JoinHandle<()> {
        let mut trigger = trigger();
        trigger.finished_grace = Duration::ZERO;
        run(trigger, sources)
    }

    #[tokio::test]
    async fn the_task_pushes_needs_you_and_finished() {
        let setup = setup("task-pushes");
        let device = setup.pair(1);
        let (url, fake, _server) = fake_push_service(201, "").await;
        setup.subscribe(&device, url.as_str(), true, true);
        let sessions = registry(&setup);
        let presence = Arc::new(Presence::default());
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = run_with_no_grace(sources(&setup, &sessions, &presence, shutdown_rx));
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
        let presence = Arc::new(Presence::default());
        sessions.insert(idle_record("s1"));
        sessions.update("s1", |record| record.status = SessionStatus::AwaitingInput);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = run_with_no_grace(sources(&setup, &sessions, &presence, shutdown_rx));
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
    async fn a_needs_you_held_at_the_desk_arrives_when_the_owner_leaves() {
        let setup = setup("task-held-left");
        let holding = setup.pair(1);
        let other = setup.pair(2);
        let (held_url, held_fake, _held_server) = fake_push_service(201, "").await;
        let (other_url, other_fake, _other_server) = fake_push_service(201, "").await;
        setup.subscribe_holding(&holding, held_url.as_str(), true, true, true);
        setup.subscribe_holding(&other, other_url.as_str(), true, true, false);
        let sessions = registry(&setup);
        let presence = Arc::new(Presence::default());
        presence.report(1, true);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = spawn(sources(&setup, &sessions, &presence, shutdown_rx))
            .expect("the push task starts");
        tokio::task::yield_now().await;

        sessions.insert(idle_record("s1"));
        sessions.update("s1", |record| record.status = SessionStatus::Working);
        sessions.update("s1", |record| record.status = SessionStatus::AwaitingInput);
        // The other device's push proves the round ran while the owner was at
        // the desk, where the held device got nothing.
        wait_for_posts(&other_fake, 1).await;
        assert_eq!(
            held_fake.posts(),
            0,
            "the push is held while the owner is at the desk"
        );

        presence.report(1, false);

        wait_for_posts(&held_fake, 1).await;
        assert_eq!(held_fake.posts(), 1, "the held push arrives once he leaves");
        assert_eq!(
            other_fake.posts(),
            1,
            "the other device is not pushed twice"
        );
        let header = {
            let seen = held_fake.seen.lock().expect("unpoisoned");
            seen[0]
                .0
                .get("topic")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        assert_eq!(
            header.as_deref(),
            Some(topic("s1").as_str()),
            "the late push is about the held session"
        );

        shutdown_tx.send_replace(true);
        let joined = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("the task ends");
        joined.expect("the task ends cleanly");
    }

    #[tokio::test]
    async fn a_held_session_answered_before_the_owner_leaves_sends_nothing() {
        let setup = setup("task-held-answered");
        let holding = setup.pair(1);
        let other = setup.pair(2);
        let (held_url, held_fake, _held_server) = fake_push_service(201, "").await;
        let (other_url, other_fake, _other_server) = fake_push_service(201, "").await;
        setup.subscribe_holding(&holding, held_url.as_str(), true, true, true);
        setup.subscribe_holding(&other, other_url.as_str(), true, true, false);
        let sessions = registry(&setup);
        let presence = Arc::new(Presence::default());
        presence.report(1, true);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = spawn(sources(&setup, &sessions, &presence, shutdown_rx))
            .expect("the push task starts");
        tokio::task::yield_now().await;

        sessions.insert(idle_record("s1"));
        sessions.update("s1", |record| record.status = SessionStatus::Working);
        sessions.update("s1", |record| record.status = SessionStatus::AwaitingInput);
        // The other device's push proves the round ran while the owner was at
        // the desk; then the session is answered, all before he leaves.
        wait_for_posts(&other_fake, 1).await;
        sessions.update("s1", |record| record.status = SessionStatus::Working);
        presence.report(1, false);

        // The later session's push, raised away from the desk, is the only
        // one the held device gets: the answered session's never arrives.
        sessions.insert(idle_record("s2"));
        sessions.update("s2", |record| record.status = SessionStatus::Working);
        sessions.update("s2", |record| record.status = SessionStatus::AwaitingInput);
        wait_for_posts(&held_fake, 1).await;

        assert_eq!(
            held_fake.posts(),
            1,
            "only the later session's push arrives; the answered one never did"
        );

        shutdown_tx.send_replace(true);
        let joined = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("the task ends");
        joined.expect("the task ends cleanly");
    }

    #[tokio::test]
    async fn the_late_push_skips_devices_that_got_it_the_first_time() {
        let setup = setup("task-late-held-only");
        let holding = setup.pair(1);
        let other = setup.pair(2);
        let (held_url, held_fake, _held_server) = fake_push_service(201, "").await;
        let (other_url, other_fake, _other_server) = fake_push_service(201, "").await;
        setup.subscribe_holding(&holding, held_url.as_str(), true, true, true);
        setup.subscribe_holding(&other, other_url.as_str(), true, true, false);
        let sessions = registry(&setup);
        let presence = Arc::new(Presence::default());
        presence.report(1, true);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = spawn(sources(&setup, &sessions, &presence, shutdown_rx))
            .expect("the push task starts");
        tokio::task::yield_now().await;

        sessions.insert(idle_record("s1"));
        sessions.update("s1", |record| record.status = SessionStatus::Working);
        sessions.update("s1", |record| record.status = SessionStatus::AwaitingInput);
        wait_for_posts(&other_fake, 1).await;
        assert_eq!(
            held_fake.posts(),
            0,
            "the first push reaches only the device that is not held"
        );

        presence.report(1, false);

        wait_for_posts(&held_fake, 1).await;
        assert_eq!(
            (held_fake.posts(), other_fake.posts()),
            (1, 1),
            "the leaving sends one more, to the held device only"
        );

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
        let presence = Arc::new(Presence::default());
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = spawn(sources(&setup, &sessions, &presence, shutdown_rx))
            .expect("the push task starts");

        shutdown_tx.send_replace(true);

        let joined = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("the task ends");
        joined.expect("the task ends cleanly");
    }
}
