//! The phone's Notifications switch, one per saved tailnet host: whether this
//! phone is registered for that host's pushes, read from the daemon's
//! `PushStatus` and never saved. Plain Rust over the replies, so every rule
//! reads and tests without a window: each step returns the effects the
//! phone's root runs (a message to send, a question for the platform).
//!
//! The platform's side, asking for permission, the device token and the
//! install's keys, sits behind [`PushPlatform`].

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use futures::FutureExt as _;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::channel::oneshot;
use futures::future::BoxFuture;
use protocol::{ApnsEnvironment, DaemonMessage, PushRegistration, Secret};
use push_crypto::InstallKeys;

use crate::sidebar::DEFAULT_FINISHED_GRACE_SECS;
use crate::target::HostKey;

/// The switch's label.
pub const NOTIFICATIONS_LABEL: &str = "Notifications";

/// The line under the switch when the phone's settings block notifications,
/// the phone web app's wording.
pub const BLOCKED: &str =
    "Notifications are blocked for this app. Turn them on in Settings, then come back.";

/// The line under the switch when the platform could not give a device token
/// or the install's keys; the cause goes to the log only.
pub const NOT_REGISTERED: &str = "Notifications could not be turned on.";

/// What the phone's settings say about notifications for this app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authorization {
    /// The app has not asked yet.
    NotDetermined,
    /// The owner turned them off; only the phone's Settings can undo that.
    Denied,
    /// The app may show notifications.
    Granted,
}

/// One notification the phone's notification drawer still shows: its
/// identifier and the `host`, `session_id` and `kind` the notification
/// extension put in its user info, when it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivered {
    /// The notification request's identifier.
    pub id: String,
    /// The daemon's host name the push came from, when the user info has it.
    pub host: Option<String>,
    /// The session the push was about, when the user info has it.
    pub session_id: Option<String>,
    /// The push notice's wire kind (`needs_you`, `finished`, …), when the
    /// user info has it; an older extension build writes none.
    pub kind: Option<String>,
    /// When the wait the push was about began, RFC 3339, when the user info
    /// has it; a background clear keeps a notification of a newer wait.
    pub waited_since: Option<String>,
}

/// The wire kind of a Needs you push notice, the only kind a sweep removes.
pub const NEEDS_YOU_KIND: &str = "needs_you";

/// A tap on a notification the drawer showed: the `host` and `session_id`
/// the notification extension put in its user info, as text the app reads
/// but never connects to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tap {
    /// The daemon's host name the notification came from.
    pub host: String,
    /// The session the notification was about.
    pub session_id: String,
}

/// What the phone's platform offers for push: whether it has push at all,
/// the APNs environment its tokens come from, its notification permission,
/// its device token and the install's push keys.
pub trait PushPlatform: Send + Sync {
    /// Whether this platform receives pushes at all; the switch draws only
    /// when it does.
    fn supported(&self) -> bool;
    /// The APNs environment this build's device tokens come from.
    fn environment(&self) -> ApnsEnvironment;
    /// The current notification permission, asking nothing.
    fn authorization(&self) -> BoxFuture<'static, Authorization>;
    /// Asks the owner for the notification permission and gives the answer.
    fn request_authorization(&self) -> BoxFuture<'static, Authorization>;
    /// The device token, hex; the error text goes to the log only.
    fn device_token(&self) -> BoxFuture<'static, Result<String, String>>;
    /// The install's push keys, made on the first call and loaded after.
    ///
    /// # Errors
    ///
    /// The keys could not be made, stored or read.
    fn install_keys(&self) -> Result<InstallKeys, String>;
    /// The notifications the drawer shows now; empty when it cannot tell.
    fn delivered(&self) -> BoxFuture<'static, Vec<Delivered>>;
    /// Removes the delivered notifications with identifiers `ids`.
    fn remove_delivered(&self, ids: Vec<String>);
    /// Sets the app's badge to `count`.
    fn set_badge(&self, count: u32);
    /// The taps on this phone's notifications, oldest first; a tap that
    /// arrived before the phone's root existed waits in it.
    fn taps(&self) -> UnboundedReceiver<Tap>;
}

/// A tap channel nothing sends on, so its drain ends at once.
pub(super) fn no_taps() -> UnboundedReceiver<Tap> {
    let (_sender, receiver) = futures::channel::mpsc::unbounded();
    receiver
}

/// The APNs environment a build reports: `sandbox` for debug and simulator
/// builds, `production` for release builds.
#[must_use]
pub fn build_environment() -> ApnsEnvironment {
    if cfg!(debug_assertions) {
        ApnsEnvironment::Sandbox
    } else {
        ApnsEnvironment::Production
    }
}

/// Why [`UnsupportedPlatform`] gives nothing.
const NO_PUSH: &str = "this platform has no push notifications";

/// A platform with no push: the switch never draws, and every question
/// fails.
pub struct UnsupportedPlatform;

impl PushPlatform for UnsupportedPlatform {
    fn supported(&self) -> bool {
        false
    }

    fn environment(&self) -> ApnsEnvironment {
        build_environment()
    }

    fn authorization(&self) -> BoxFuture<'static, Authorization> {
        futures::future::ready(Authorization::Denied).boxed()
    }

    fn request_authorization(&self) -> BoxFuture<'static, Authorization> {
        futures::future::ready(Authorization::Denied).boxed()
    }

    fn device_token(&self) -> BoxFuture<'static, Result<String, String>> {
        futures::future::ready(Err(NO_PUSH.to_owned())).boxed()
    }

    fn install_keys(&self) -> Result<InstallKeys, String> {
        Err(NO_PUSH.to_owned())
    }

    fn delivered(&self) -> BoxFuture<'static, Vec<Delivered>> {
        futures::future::ready(Vec::new()).boxed()
    }

    fn remove_delivered(&self, _ids: Vec<String>) {}

    fn set_badge(&self, _count: u32) {}

    fn taps(&self) -> UnboundedReceiver<Tap> {
        no_taps()
    }
}

/// One question a [`FakePushPlatform`] was asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformCall {
    /// [`PushPlatform::authorization`].
    Authorization,
    /// [`PushPlatform::request_authorization`].
    RequestAuthorization,
    /// [`PushPlatform::device_token`].
    DeviceToken,
    /// [`PushPlatform::install_keys`].
    InstallKeys,
    /// [`PushPlatform::delivered`].
    Delivered,
}

/// What a [`FakePushPlatform`] answers.
struct FakeScript {
    supported: bool,
    authorization: Authorization,
    request_answer: Authorization,
    token: Result<String, String>,
    calls: Vec<PlatformCall>,
    delivered: Vec<Delivered>,
    removed: Vec<Vec<String>>,
    badges: Vec<u32>,
    /// Whether a delivered-notifications question waits for
    /// [`FakePushPlatform::release_delivered`].
    hold_delivered: bool,
    held: Vec<oneshot::Sender<Vec<Delivered>>>,
    /// The phone's end of the taps, until it takes it.
    taps: Option<UnboundedReceiver<Tap>>,
    /// The fake's end of the taps, which [`FakePushPlatform::tap`] sends on.
    tapper: UnboundedSender<Tap>,
}

/// A scripted platform for specs: supported, permission granted, a fixed
/// token and keys from fixed bytes, each answer settable, and a record of
/// every question asked. A permission request's answer becomes the
/// permission from then on. Its drawer holds the notifications a spec
/// scripts, less those removed since; each removal and badge is recorded.
pub struct FakePushPlatform {
    script: Mutex<FakeScript>,
}

impl Default for FakePushPlatform {
    fn default() -> Self {
        let (tapper, taps) = futures::channel::mpsc::unbounded();
        Self {
            script: Mutex::new(FakeScript {
                supported: true,
                authorization: Authorization::Granted,
                request_answer: Authorization::Granted,
                token: Ok(FakePushPlatform::TOKEN.to_owned()),
                calls: Vec::new(),
                delivered: Vec::new(),
                removed: Vec::new(),
                badges: Vec::new(),
                hold_delivered: false,
                held: Vec::new(),
                taps: Some(taps),
                tapper,
            }),
        }
    }
}

impl FakePushPlatform {
    /// The device token the fake gives unless a spec sets another.
    pub const TOKEN: &'static str = "a1b2c3d4";

    fn script(&self) -> std::sync::MutexGuard<'_, FakeScript> {
        self.script.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn record(&self, call: PlatformCall) {
        self.script().calls.push(call);
    }

    /// Sets whether the platform has push.
    pub fn set_supported(&self, supported: bool) {
        self.script().supported = supported;
    }

    /// Sets the current permission.
    pub fn set_authorization(&self, authorization: Authorization) {
        self.script().authorization = authorization;
    }

    /// Sets what a permission request answers.
    pub fn set_request_answer(&self, answer: Authorization) {
        self.script().request_answer = answer;
    }

    /// Sets the device token, or the error the platform gives for it.
    pub fn set_token(&self, token: Result<String, String>) {
        self.script().token = token;
    }

    /// Every question asked so far, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<PlatformCall> {
        self.script().calls.clone()
    }

    /// Sets the notifications the drawer shows.
    pub fn set_delivered(&self, delivered: Vec<Delivered>) {
        self.script().delivered = delivered;
    }

    /// Every [`PushPlatform::remove_delivered`] so far, its identifiers, in
    /// order.
    #[must_use]
    pub fn removed(&self) -> Vec<Vec<String>> {
        self.script().removed.clone()
    }

    /// Every badge set so far, in order.
    #[must_use]
    pub fn badges(&self) -> Vec<u32> {
        self.script().badges.clone()
    }

    /// Sets whether each delivered-notifications question waits for
    /// [`Self::release_delivered`] before it answers.
    pub fn set_hold_delivered(&self, hold: bool) {
        self.script().hold_delivered = hold;
    }

    /// Answers every held delivered-notifications question, oldest first,
    /// with the drawer as it is now.
    pub fn release_delivered(&self) {
        let (held, delivered) = {
            let mut script = self.script();
            (std::mem::take(&mut script.held), script.delivered.clone())
        };
        for sender in held {
            // A receiver gone means its sweep was dropped.
            let _unheard = sender.send(delivered.clone());
        }
    }

    /// Hands the phone a tap on a notification for `session_id` on `host`,
    /// as a tap on the drawer delivers one.
    pub fn tap(&self, host: &str, session_id: &str) {
        let tap = Tap {
            host: host.to_owned(),
            session_id: session_id.to_owned(),
        };
        // A receiver gone means the phone's root was dropped.
        let _unheard = self.script().tapper.unbounded_send(tap);
    }

    /// The keys [`PushPlatform::install_keys`] gives: a fixed scalar and
    /// secret.
    #[must_use]
    pub fn keys() -> InstallKeys {
        let mut private_key = [0u8; 32];
        for (octet, value) in private_key.iter_mut().zip(1u8..) {
            *octet = value;
        }
        InstallKeys {
            private_key,
            auth: [0x5a; 16],
        }
    }
}

impl PushPlatform for FakePushPlatform {
    fn supported(&self) -> bool {
        self.script().supported
    }

    fn environment(&self) -> ApnsEnvironment {
        ApnsEnvironment::Sandbox
    }

    fn authorization(&self) -> BoxFuture<'static, Authorization> {
        self.record(PlatformCall::Authorization);
        futures::future::ready(self.script().authorization).boxed()
    }

    fn request_authorization(&self) -> BoxFuture<'static, Authorization> {
        self.record(PlatformCall::RequestAuthorization);
        let mut script = self.script();
        script.authorization = script.request_answer;
        futures::future::ready(script.request_answer).boxed()
    }

    fn device_token(&self) -> BoxFuture<'static, Result<String, String>> {
        self.record(PlatformCall::DeviceToken);
        futures::future::ready(self.script().token.clone()).boxed()
    }

    fn install_keys(&self) -> Result<InstallKeys, String> {
        self.record(PlatformCall::InstallKeys);
        Ok(Self::keys())
    }

    fn delivered(&self) -> BoxFuture<'static, Vec<Delivered>> {
        self.record(PlatformCall::Delivered);
        let mut script = self.script();
        if !script.hold_delivered {
            return futures::future::ready(script.delivered.clone()).boxed();
        }
        let (tx, rx) = oneshot::channel();
        script.held.push(tx);
        rx.map(Result::unwrap_or_default).boxed()
    }

    fn remove_delivered(&self, ids: Vec<String>) {
        let mut script = self.script();
        script.delivered.retain(|shown| !ids.contains(&shown.id));
        script.removed.push(ids);
    }

    fn set_badge(&self, count: u32) {
        self.script().badges.push(count);
    }

    fn taps(&self) -> UnboundedReceiver<Tap> {
        self.script().taps.take().unwrap_or_else(no_taps)
    }
}

/// Where a host's switch stands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Status {
    /// No `PushStatus` read yet.
    #[default]
    Unknown,
    /// The daemon holds no device token for this phone.
    Off,
    /// The daemon holds this phone's current device token.
    On,
    /// A registration is out.
    Registering,
    /// The phone's settings block notifications.
    Blocked,
    /// The daemon refused, or the platform failed; the text shows under the
    /// switch.
    Failed(String),
}

/// What the phone's root runs for a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Send `GetPushStatus` with `request_id`.
    SendGetStatus { request_id: String },
    /// Ask the owner for the notification permission and fold the answer
    /// back with [`PushState::authorization_answered`].
    RequestAuthorization,
    /// Read the device token and keys, then send `RegisterPush` with all
    /// three switches on and `request_id`.
    Register { request_id: String },
    /// Send `UnregisterPush` with `request_id`.
    Unregister { request_id: String },
}

/// What the request out asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request {
    /// `GetPushStatus` on open.
    Status,
    /// `GetPushStatus` after the daemon refused a register or unregister.
    Recheck,
    /// The device token and keys, then `RegisterPush`.
    Register,
    /// `UnregisterPush`.
    Unregister,
}

/// One host's switch: where it stands, the device token the daemon last
/// reported, and the request a reply settles.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushState {
    /// Where the switch stands.
    pub status: Status,
    /// The daemon's device token for this phone, from its last `PushStatus`.
    pub last_token: Option<String>,
    /// The request id the next reply or completion must carry, and what it
    /// asks.
    pending: Option<(String, Request)>,
    /// The daemon's text for the last refused register or unregister,
    /// shown until the switch asks anything new.
    refusal: Option<String>,
    /// How many request ids this host's state has made.
    issued: u64,
}

impl PushState {
    /// A switch whose request ids follow the `issued` ones an earlier
    /// switch for the same host made, so none is reused.
    fn after(issued: u64) -> Self {
        Self {
            issued,
            ..Self::default()
        }
    }

    /// The request id a reply must echo to settle the work out.
    #[must_use]
    pub fn pending(&self) -> Option<&str> {
        self.pending.as_ref().map(|(id, _)| id.as_str())
    }

    /// Whether a request is out: the switch shows dimmed and a tap does
    /// nothing.
    #[must_use]
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }

    /// Whether the switch shows on: on, or a registration out.
    #[must_use]
    pub fn switched_on(&self) -> bool {
        matches!(self.status, Status::On | Status::Registering)
    }

    /// The line under the switch, if any: the blocked text, a failure's
    /// text, or the refusal the switch still holds.
    #[must_use]
    pub fn hint(&self) -> Option<String> {
        match &self.status {
            Status::Blocked => Some(BLOCKED.to_owned()),
            Status::Failed(text) => Some(text.clone()),
            _ => self.refusal.clone(),
        }
    }

    /// A fresh request id for `kind`, remembered as the one a reply must
    /// carry; anything but a re-check clears the last refusal.
    fn issue(&mut self, kind: Request) -> String {
        self.issued += 1;
        let request_id = format!("push-{}", self.issued);
        self.pending = Some((request_id.clone(), kind));
        if kind != Request::Recheck {
            self.refusal = None;
        }
        request_id
    }

    /// Whether `request_id` is the one out.
    fn owes(&self, request_id: Option<&str>) -> bool {
        request_id.is_some() && self.pending() == request_id
    }

    /// Whether `request_id` is the registration out, so the device token
    /// and keys read for it may be sent.
    #[must_use]
    pub fn token_ready(&self, request_id: &str) -> bool {
        matches!(&self.pending, Some((id, Request::Register)) if id == request_id)
    }

    /// The host's connection opened: ask the daemon for this phone's push
    /// state. Any request out is superseded; a registration out no longer
    /// shows.
    pub fn on_open(&mut self) -> Vec<Effect> {
        if self.status == Status::Registering {
            self.status = Status::Unknown;
        }
        vec![Effect::SendGetStatus {
            request_id: self.issue(Request::Status),
        }]
    }

    /// The daemon's answer for the work out: no token reads off; this
    /// phone's `current` token reads on; another token re-registers when
    /// the current one is known, and reads on until it is. The answer to a
    /// re-check after a refusal reads on for another token too, so a
    /// refused registration is never retried on its own.
    pub fn on_status(&mut self, device_token: Option<&str>, current: Option<&str>) -> Vec<Effect> {
        let recheck = matches!(self.pending.take(), Some((_, Request::Recheck)));
        self.last_token = device_token.map(str::to_owned);
        match (device_token, current) {
            (None, _) => {
                self.status = Status::Off;
                Vec::new()
            }
            (Some(token), Some(current)) if token != current && !recheck => self.register(),
            (Some(_), _) => {
                self.status = Status::On;
                Vec::new()
            }
        }
    }

    /// A `PushStatus` from the daemon: folded with [`Self::on_status`] when
    /// it answers the request out; `None` when it answers another.
    pub fn status_reply(
        &mut self,
        request_id: Option<&str>,
        device_token: Option<&str>,
        current: Option<&str>,
    ) -> Option<Vec<Effect>> {
        self.owes(request_id)
            .then(|| self.on_status(device_token, current))
    }

    /// An `Error` from the daemon: when it answers the request out, the
    /// switch shows the daemon's text; a refused register or unregister
    /// also asks for the status again, so the switch returns to the
    /// daemon's truth with the text still under it. `None` when it answers
    /// another request.
    pub fn on_error(&mut self, request_id: Option<&str>, message: &str) -> Option<Vec<Effect>> {
        if !self.owes(request_id) {
            return None;
        }
        let kind = self.pending.take().map(|(_, kind)| kind);
        self.status = Status::Failed(message.to_owned());
        if !matches!(kind, Some(Request::Register | Request::Unregister)) {
            return Some(Vec::new());
        }
        self.refusal = Some(message.to_owned());
        Some(vec![Effect::SendGetStatus {
            request_id: self.issue(Request::Recheck),
        }])
    }

    /// This phone's device token became known: a switch on for another
    /// token re-registers.
    pub fn token_known(&mut self, current: &str) -> Vec<Effect> {
        let other = self
            .last_token
            .as_deref()
            .is_some_and(|token| token != current);
        if self.status == Status::On && self.pending.is_none() && other {
            return self.register();
        }
        Vec::new()
    }

    /// The switch turned on with the permission as it stands: not asked yet
    /// asks, granted registers, denied shows blocked and asks nothing. While
    /// any request is out it does nothing.
    pub fn switch_on(&mut self, auth: Authorization) -> Vec<Effect> {
        if self.busy() {
            return Vec::new();
        }
        match auth {
            Authorization::NotDetermined => vec![Effect::RequestAuthorization],
            Authorization::Granted => self.register(),
            Authorization::Denied => {
                self.status = Status::Blocked;
                Vec::new()
            }
        }
    }

    /// The owner answered the permission request: granted registers, denied
    /// shows blocked, no answer leaves the switch off. While any request is
    /// out, a registration included, it does nothing.
    pub fn authorization_answered(&mut self, auth: Authorization) -> Vec<Effect> {
        if self.busy() {
            return Vec::new();
        }
        match auth {
            Authorization::Granted => self.register(),
            Authorization::Denied => {
                self.status = Status::Blocked;
                Vec::new()
            }
            Authorization::NotDetermined => {
                self.status = Status::Off;
                Vec::new()
            }
        }
    }

    /// The platform gave no device token or keys for registration
    /// `request_id`: when it is the one out, nothing is out and the switch
    /// says it could not turn on. Whether it was.
    pub fn failed(&mut self, request_id: &str) -> bool {
        if !self.token_ready(request_id) {
            return false;
        }
        self.pending = None;
        self.status = Status::Failed(NOT_REGISTERED.to_owned());
        true
    }

    /// The switch turned off: unregister, the switch off at once. Work
    /// already out takes no unregister.
    pub fn switch_off(&mut self) -> Vec<Effect> {
        if self.busy() {
            return Vec::new();
        }
        self.status = Status::Off;
        vec![Effect::Unregister {
            request_id: self.issue(Request::Unregister),
        }]
    }

    /// A setting the registration carries changed: a switch that is on with
    /// nothing out takes a registration out again, its fresh request id
    /// given; any other switch takes none.
    fn reregister(&mut self) -> Option<String> {
        if self.status != Status::On || self.busy() {
            return None;
        }
        self.status = Status::Registering;
        Some(self.issue(Request::Register))
    }

    /// A registration out.
    fn register(&mut self) -> Vec<Effect> {
        self.status = Status::Registering;
        vec![Effect::Register {
            request_id: self.issue(Request::Register),
        }]
    }
}

/// What a host's switch draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchView {
    /// Whether it shows on.
    pub on: bool,
    /// Whether it shows dimmed: a request out.
    pub busy: bool,
    /// The line under it, if any.
    pub hint: Option<String>,
}

/// What `state`'s switch draws; a host with no state yet shows off.
#[must_use]
pub fn switch_view(state: Option<&PushState>) -> SwitchView {
    SwitchView {
        on: state.is_some_and(PushState::switched_on),
        busy: state.is_some_and(PushState::busy),
        hint: state.and_then(PushState::hint),
    }
}

/// The `RegisterPush` registration for `token` and the install's keys from
/// `platform`.
///
/// # Errors
///
/// The platform gave no keys, or its keys have no public key; the text goes
/// to the log only and names no key octet.
pub fn apns_registration(
    platform: &dyn PushPlatform,
    token: String,
) -> Result<PushRegistration, String> {
    let keys = platform.install_keys()?;
    let p256dh = keys.p256dh_b64url().map_err(|err| format!("{err:#}"))?;
    Ok(PushRegistration::Apns {
        token: Secret(token),
        environment: platform.environment(),
        p256dh,
        auth: Secret(keys.auth_b64url()),
    })
}

/// Every open host's switch, the connection generation each was opened on,
/// this phone's device token once known, and the effects the steps made
/// that the phone's root has not run yet.
#[derive(Debug, Default)]
pub struct PushHosts {
    hosts: HashMap<HostKey, PushState>,
    /// The connection generation each host in `hosts` was last opened on.
    open: HashMap<HostKey, u64>,
    /// How many request ids a forgotten host's switch had made, so its next
    /// switch reuses none.
    spent: HashMap<HostKey, u64>,
    current: Option<String>,
    /// This phone's Finished grace in seconds, once set; until then the
    /// default.
    grace: Option<u32>,
    outbox: Vec<(HostKey, Effect)>,
}

impl PushHosts {
    /// The hosts open and taking relay pushes now, each with its
    /// connection generation: a host whose (key, generation) was not open
    /// before asks for its push state, superseding any request out; a host
    /// no longer listed is forgotten, so a late reply or completion for it
    /// finds no switch.
    pub fn hosts_open(&mut self, open: &[(HostKey, u64)]) -> Vec<(HostKey, Effect)> {
        let listed = |key: &HostKey| open.iter().any(|(listed, _)| listed == key);
        let gone: Vec<HostKey> = self
            .hosts
            .keys()
            .filter(|key| !listed(key))
            .cloned()
            .collect();
        for key in gone {
            if let Some(state) = self.hosts.remove(&key) {
                self.spent.insert(key, state.issued);
            }
        }
        let mut effects = Vec::new();
        for (key, generation) in open {
            if self.open.get(key) == Some(generation) && self.hosts.contains_key(key) {
                continue;
            }
            let spent = self.spent.remove(key).unwrap_or(0);
            let state = self
                .hosts
                .entry(key.clone())
                .or_insert_with(|| PushState::after(spent));
            effects.extend(
                state
                    .on_open()
                    .into_iter()
                    .map(|effect| (key.clone(), effect)),
            );
        }
        self.open = open.iter().cloned().collect();
        effects
    }

    /// `host`'s switch, once it has one.
    #[must_use]
    pub fn state(&self, host: &HostKey) -> Option<&PushState> {
        self.hosts.get(host)
    }

    /// Whether any open host's switch carries a hint.
    #[must_use]
    pub fn any_hint(&self) -> bool {
        self.hosts.values().any(|state| state.hint().is_some())
    }

    /// This phone's device token, once known.
    #[must_use]
    pub fn current_token(&self) -> Option<&str> {
        self.current.as_deref()
    }

    /// Runs `step` on `host`'s switch with this phone's device token,
    /// queueing the effects it makes; a host with no switch (not open, or
    /// forgotten since) takes no step.
    pub fn step(
        &mut self,
        host: &HostKey,
        step: impl FnOnce(&mut PushState, Option<&str>) -> Vec<Effect>,
    ) {
        let Some(state) = self.hosts.get_mut(host) else {
            tracing::debug!(?host, "phone: no push switch for a step");
            return;
        };
        let effects = step(state, self.current.as_deref());
        self.outbox
            .extend(effects.into_iter().map(|effect| (host.clone(), effect)));
    }

    /// Whether `request_id` is `host`'s registration out, so the device
    /// token and keys read for it may be sent.
    #[must_use]
    pub fn token_ready(&self, host: &HostKey, request_id: &str) -> bool {
        self.hosts
            .get(host)
            .is_some_and(|state| state.token_ready(request_id))
    }

    /// The platform gave no token or keys for `host`'s registration
    /// `request_id`: [`PushState::failed`]. Whether the switch changed.
    pub fn failed(&mut self, host: &HostKey, request_id: &str) -> bool {
        self.hosts
            .get_mut(host)
            .is_some_and(|state| state.failed(request_id))
    }

    /// A daemon refusal from `host` carrying `request_id`: when it answers
    /// the switch's request out, the switch shows `message` and the effects
    /// it makes are queued. Whether it answered.
    pub fn on_refused(&mut self, host: &HostKey, request_id: &str, message: &str) -> bool {
        let Some(state) = self.hosts.get_mut(host) else {
            return false;
        };
        let Some(effects) = state.on_error(Some(request_id), message) else {
            return false;
        };
        self.outbox
            .extend(effects.into_iter().map(|effect| (host.clone(), effect)));
        true
    }

    /// This phone's device token is `token`: each switch on for another
    /// token re-registers.
    pub fn set_current(&mut self, token: &str) {
        if self.current.as_deref() == Some(token) {
            return;
        }
        self.current = Some(token.to_owned());
        for (host, state) in &mut self.hosts {
            let effects = state.token_known(token);
            self.outbox
                .extend(effects.into_iter().map(|effect| (host.clone(), effect)));
        }
    }

    /// Folds a daemon message from `host`: a `PushStatus` that answers the
    /// switch's request out (a refusal comes through [`Self::on_refused`]).
    /// Whether the switch changed.
    ///
    /// The answer to an open's status request that holds this phone's token
    /// with another Finished grace (none reads as the default) registers
    /// again, so the daemon holds the phone's grace; a register or re-check
    /// answer never does, so a daemon that keeps no grace cannot loop.
    pub fn on_message(&mut self, host: &HostKey, msg: &DaemonMessage) -> bool {
        let DaemonMessage::PushStatus {
            device_token,
            finished_grace_secs,
            request_id,
            ..
        } = msg
        else {
            return false;
        };
        let grace = self.grace();
        let Some(state) = self.hosts.get_mut(host) else {
            return false;
        };
        let asked_status = matches!(state.pending, Some((_, Request::Status)));
        let token = device_token.as_ref().map(|token| token.0.as_str());
        let current = self.current.as_deref();
        let Some(mut effects) = state.status_reply(request_id.as_deref(), token, current) else {
            return false;
        };
        let held = finished_grace_secs.unwrap_or(DEFAULT_FINISHED_GRACE_SECS);
        if asked_status && token.is_some() && token == current && held != grace {
            effects.extend(state.register());
        }
        self.outbox
            .extend(effects.into_iter().map(|effect| (host.clone(), effect)));
        true
    }

    /// A setting the registration carries changed: each host whose switch is
    /// on with nothing out takes a registration out again, and each gives its
    /// fresh request id. None does while this phone's device token is not
    /// known, since the registration must carry it.
    pub fn reregister_on(&mut self) -> Vec<(HostKey, String)> {
        if self.current.is_none() {
            return Vec::new();
        }
        self.hosts
            .iter_mut()
            .filter_map(|(host, state)| state.reregister().map(|id| (host.clone(), id)))
            .collect()
    }

    /// This phone's Finished grace in seconds, the default until set.
    #[must_use]
    pub fn grace(&self) -> u32 {
        self.grace.unwrap_or(DEFAULT_FINISHED_GRACE_SECS)
    }

    /// This phone's Finished grace is `secs`, which the next open's status
    /// answer compares against.
    pub fn set_grace(&mut self, secs: u32) {
        self.grace = Some(secs);
    }

    /// The effects queued since the last call, in order.
    pub fn take_effects(&mut self) -> Vec<(HostKey, Effect)> {
        std::mem::take(&mut self.outbox)
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::{
        Authorization, BLOCKED, Effect, FakePushPlatform, NOT_REGISTERED, PushHosts, PushState,
        Status, apns_registration, switch_view,
    };
    use crate::target::{HostKey, Target};
    use protocol::{ApnsEnvironment, DaemonMessage, PushRegistration, Secret};

    /// This phone's device token.
    const MINE: &str = "a1a1";
    /// Another device token the daemon holds.
    const OTHER: &str = "b2b2";

    /// A switch whose `GetPushStatus` is out, and that request's id.
    fn opened() -> (PushState, String) {
        let mut state = PushState::default();
        let effects = state.on_open();
        let [Effect::SendGetStatus { request_id }] = effects.as_slice() else {
            panic!("an open sends one GetPushStatus: {effects:?}");
        };
        let request_id = request_id.clone();
        (state, request_id)
    }

    /// The request id of the one register in `effects`.
    fn register_id(effects: &[Effect]) -> String {
        let [Effect::Register { request_id }] = effects else {
            panic!("one register: {effects:?}");
        };
        request_id.clone()
    }

    /// The request id of the one `GetPushStatus` in `effects`.
    fn status_id(effects: &[Effect]) -> String {
        let [Effect::SendGetStatus { request_id }] = effects else {
            panic!("one GetPushStatus: {effects:?}");
        };
        request_id.clone()
    }

    /// The request id of the one `GetPushStatus` the hosts' `effects` hold
    /// for [`host`].
    fn host_status_id(effects: &[(HostKey, Effect)]) -> String {
        let [(key, Effect::SendGetStatus { request_id })] = effects else {
            panic!("one GetPushStatus: {effects:?}");
        };
        assert_eq!(*key, host());
        request_id.clone()
    }

    /// A switch on for this phone's token.
    fn on() -> PushState {
        let (mut state, id) = opened();
        let _ = state.status_reply(Some(&id), Some(MINE), Some(MINE));
        assert_eq!(state.status, Status::On);
        state
    }

    fn host() -> HostKey {
        HostKey::of(&Target::tailnet("box.tail1234.ts.net", Some(8787)))
    }

    fn push_status(token: Option<&str>, request_id: &str) -> DaemonMessage {
        DaemonMessage::PushStatus {
            vapid_public_key: "vapid".to_owned(),
            endpoint: None,
            device_token: token.map(|token| Secret(token.to_owned())),
            needs_you: true,
            finished: true,
            hold_at_desk: true,
            finished_grace_secs: None,
            request_id: Some(request_id.to_owned()),
        }
    }

    #[test]
    fn opening_asks_for_the_status() {
        let (state, id) = opened();
        assert_eq!(state.pending(), Some(id.as_str()));
        assert_eq!(state.status, Status::Unknown);
    }

    #[test]
    fn status_without_a_token_reads_off() {
        let (mut state, id) = opened();
        let effects = state.status_reply(Some(&id), None, Some(MINE));
        assert_eq!(effects, Some(Vec::new()));
        assert_eq!(state.status, Status::Off);
        assert_eq!(state.pending(), None);
    }

    #[test]
    fn status_with_this_phones_token_reads_on() {
        let (mut state, id) = opened();
        let effects = state.status_reply(Some(&id), Some(MINE), Some(MINE));
        assert_eq!(effects, Some(Vec::new()));
        assert_eq!(state.status, Status::On);
        assert_eq!(state.last_token.as_deref(), Some(MINE));
    }

    #[test]
    fn a_different_token_reregisters() {
        let (mut state, id) = opened();
        let effects = state
            .status_reply(Some(&id), Some(OTHER), Some(MINE))
            .expect("the reply answers the open");
        let request_id = register_id(&effects);
        assert_eq!(state.status, Status::Registering);
        assert_eq!(state.pending(), Some(request_id.as_str()));
    }

    #[test]
    fn a_token_while_this_phones_is_unknown_reads_on_until_it_is_known() {
        let (mut state, id) = opened();
        let effects = state.status_reply(Some(&id), Some(OTHER), None);
        assert_eq!(effects, Some(Vec::new()));
        assert_eq!(state.status, Status::On);
        let effects = state.token_known(MINE);
        register_id(&effects);
        assert_eq!(state.status, Status::Registering);
    }

    #[test]
    fn learning_the_same_token_changes_nothing() {
        let mut state = on();
        assert_eq!(state.token_known(MINE), Vec::new());
        assert_eq!(state.status, Status::On);
    }

    #[test]
    fn a_reply_for_another_request_is_ignored() {
        let (mut state, id) = opened();
        assert_eq!(state.status_reply(Some("other"), None, Some(MINE)), None);
        assert_eq!(state.status_reply(None, None, Some(MINE)), None);
        assert_eq!(state.on_error(Some("other"), "refused"), None);
        assert_eq!(state.status, Status::Unknown);
        assert_eq!(state.pending(), Some(id.as_str()));
    }

    #[test]
    fn an_error_for_the_request_out_shows_its_text() {
        let (mut state, id) = opened();
        assert_eq!(
            state.on_error(Some(&id), "this device is not paired"),
            Some(Vec::new()),
            "a refused status request asks nothing more"
        );
        assert_eq!(
            state.status,
            Status::Failed("this device is not paired".to_owned())
        );
        assert_eq!(state.pending(), None);
    }

    #[test]
    fn switch_on_when_not_asked_asks_first() {
        let mut state = PushState::default();
        assert_eq!(
            state.switch_on(Authorization::NotDetermined),
            vec![Effect::RequestAuthorization]
        );
        assert_eq!(state.pending(), None);
    }

    #[test]
    fn switch_on_when_granted_registers() {
        let mut state = PushState::default();
        let effects = state.switch_on(Authorization::Granted);
        register_id(&effects);
        assert_eq!(state.status, Status::Registering);
        assert!(state.switched_on());
    }

    #[test]
    fn switch_on_when_denied_blocks_without_asking() {
        let mut state = PushState::default();
        assert_eq!(state.switch_on(Authorization::Denied), Vec::new());
        assert_eq!(state.status, Status::Blocked);
    }

    #[test]
    fn switch_on_while_registering_registers_once() {
        let mut state = PushState::default();
        register_id(&state.switch_on(Authorization::Granted));
        assert_eq!(state.switch_on(Authorization::Granted), Vec::new());
    }

    #[test]
    fn a_granted_answer_registers() {
        let mut state = PushState::default();
        register_id(&state.authorization_answered(Authorization::Granted));
    }

    #[test]
    fn a_denied_answer_blocks() {
        let mut state = PushState::default();
        assert_eq!(
            state.authorization_answered(Authorization::Denied),
            Vec::new()
        );
        assert_eq!(state.status, Status::Blocked);
    }

    #[test]
    fn no_answer_leaves_it_off() {
        let mut state = PushState::default();
        assert_eq!(
            state.authorization_answered(Authorization::NotDetermined),
            Vec::new()
        );
        assert_eq!(state.status, Status::Off);
    }

    #[test]
    fn the_registers_reply_reads_on() {
        let mut state = PushState::default();
        let id = register_id(&state.switch_on(Authorization::Granted));
        let effects = state.status_reply(Some(&id), Some(MINE), Some(MINE));
        assert_eq!(effects, Some(Vec::new()));
        assert_eq!(state.status, Status::On);
    }

    #[test]
    fn a_platform_failure_says_it_could_not_turn_on() {
        let mut state = PushState::default();
        let id = register_id(&state.switch_on(Authorization::Granted));
        assert!(state.failed(&id));
        assert_eq!(state.status, Status::Failed(NOT_REGISTERED.to_owned()));
        assert_eq!(state.pending(), None);
    }

    #[test]
    fn switch_off_unregisters() {
        let mut state = on();
        let effects = state.switch_off();
        let [Effect::Unregister { request_id }] = effects.as_slice() else {
            panic!("one unregister: {effects:?}");
        };
        assert_eq!(state.pending(), Some(request_id.as_str()));
        assert_eq!(state.status, Status::Off);
        assert!(!state.switched_on());
    }

    #[test]
    fn switch_off_with_work_out_sends_nothing() {
        let (mut state, _) = opened();
        assert_eq!(state.switch_off(), Vec::new());
    }

    #[test]
    fn request_ids_differ() {
        let mut state = PushState::default();
        let first = state.on_open();
        let second = state.on_open();
        assert_ne!(first, second);
    }

    #[test]
    fn the_view_shows_each_status() {
        assert!(!switch_view(None).on);
        let mut registering = PushState::default();
        register_id(&registering.switch_on(Authorization::Granted));
        let view = switch_view(Some(&registering));
        assert!(view.on && view.busy);
        let blocked = PushState {
            status: Status::Blocked,
            ..PushState::default()
        };
        let view = switch_view(Some(&blocked));
        assert!(!view.on && !view.busy);
        assert_eq!(view.hint.as_deref(), Some(BLOCKED));
        assert_eq!(switch_view(Some(&on())).hint, None);
    }

    #[test]
    fn the_hosts_fold_only_a_reply_the_switch_owes() {
        let mut hosts = PushHosts::default();
        assert!(!hosts.on_message(&host(), &push_status(None, "push-1")));
        let request_id = host_status_id(&hosts.hosts_open(&[(host(), 1)]));
        assert!(!hosts.on_message(&host(), &push_status(None, "other")));
        assert!(hosts.on_message(&host(), &push_status(None, &request_id)));
        assert_eq!(
            hosts.state(&host()).map(|state| state.status.clone()),
            Some(Status::Off)
        );
    }

    #[test]
    fn learning_this_phones_token_reregisters_a_host_on_for_another() {
        let mut hosts = PushHosts::default();
        let request_id = host_status_id(&hosts.hosts_open(&[(host(), 1)]));
        assert!(hosts.on_message(&host(), &push_status(Some(OTHER), &request_id)));
        assert_eq!(hosts.take_effects(), Vec::new());
        hosts.set_current(MINE);
        let effects = hosts.take_effects();
        assert!(
            matches!(effects.as_slice(), [(key, Effect::Register { .. })] if *key == host()),
            "{effects:?}"
        );
        assert_eq!(hosts.current_token(), Some(MINE));
    }

    #[test]
    fn a_completion_for_a_superseded_request_is_dropped() {
        let mut state = PushState::default();
        let first = register_id(&state.switch_on(Authorization::Granted));
        assert!(
            state.token_ready(&first),
            "the registration out takes its token"
        );
        let reopened = status_id(&state.on_open());
        assert!(
            !state.token_ready(&first),
            "a reopen supersedes the registration"
        );
        assert!(
            !state.token_ready(&reopened),
            "a status request takes no token"
        );
        assert_eq!(state.pending(), Some(reopened.as_str()));
    }

    #[test]
    fn a_late_failure_does_not_clear_a_newer_request() {
        let mut state = PushState::default();
        let first = register_id(&state.switch_on(Authorization::Granted));
        let reopened = status_id(&state.on_open());
        assert!(
            !state.failed(&first),
            "the failure answers a superseded request"
        );
        assert_eq!(state.pending(), Some(reopened.as_str()));
        assert_eq!(state.status, Status::Unknown);
        assert_eq!(switch_view(Some(&state)).hint, None);
    }

    #[test]
    fn the_switch_is_busy_while_any_request_is_out() {
        let (mut state, id) = opened();
        assert!(
            switch_view(Some(&state)).busy,
            "a status request out dims it"
        );
        assert_eq!(state.switch_off(), Vec::new());
        assert_eq!(state.switch_on(Authorization::Granted), Vec::new());
        assert_eq!(state.switch_on(Authorization::NotDetermined), Vec::new());
        assert_eq!(state.pending(), Some(id.as_str()));
        assert_eq!(state.status, Status::Unknown);
        let _ = state.status_reply(Some(&id), None, Some(MINE));
        assert!(!switch_view(Some(&state)).busy, "the reply undims it");
    }

    #[test]
    fn a_second_permission_answer_while_registering_adds_nothing() {
        let mut state = PushState::default();
        let id = register_id(&state.authorization_answered(Authorization::Granted));
        assert_eq!(
            state.authorization_answered(Authorization::Granted),
            Vec::new()
        );
        assert_eq!(
            state.authorization_answered(Authorization::Denied),
            Vec::new()
        );
        assert_eq!(state.status, Status::Registering);
        assert_eq!(state.pending(), Some(id.as_str()));
    }

    #[test]
    fn a_refused_register_asks_for_the_status_again() {
        let mut state = PushState::default();
        let id = register_id(&state.switch_on(Authorization::Granted));
        let effects = state
            .on_error(Some(&id), "refused")
            .expect("the error answers the registration");
        let again = status_id(&effects);
        assert_ne!(again, id);
        let view = switch_view(Some(&state));
        assert!(view.busy && !view.on, "{view:?}");
        assert_eq!(view.hint.as_deref(), Some("refused"));

        let effects = state.status_reply(Some(&again), Some(OTHER), Some(MINE));
        assert_eq!(
            effects,
            Some(Vec::new()),
            "a refused registration is not retried on its own"
        );
        let view = switch_view(Some(&state));
        assert!(view.on && !view.busy, "{view:?}");
        assert_eq!(view.hint.as_deref(), Some("refused"), "the refusal shows");
    }

    #[test]
    fn a_refused_unregister_asks_for_the_status_again() {
        let mut state = on();
        let effects = state.switch_off();
        let [Effect::Unregister { request_id }] = effects.as_slice() else {
            panic!("one unregister: {effects:?}");
        };
        let effects = state
            .on_error(Some(request_id), "refused")
            .expect("the error answers the unregister");
        let again = status_id(&effects);
        assert_eq!(
            state.status_reply(Some(&again), Some(MINE), Some(MINE)),
            Some(Vec::new())
        );
        let view = switch_view(Some(&state));
        assert!(view.on, "the daemon still holds this phone's token");
        assert_eq!(view.hint.as_deref(), Some("refused"));
    }

    #[test]
    fn a_reopen_with_a_new_generation_asks_again() {
        let mut hosts = PushHosts::default();
        let first = host_status_id(&hosts.hosts_open(&[(host(), 1)]));
        assert_eq!(
            hosts.hosts_open(&[(host(), 1)]),
            Vec::new(),
            "the same connection asks once"
        );
        let second = host_status_id(&hosts.hosts_open(&[(host(), 2)]));
        assert_ne!(first, second);
        assert!(
            !hosts.on_message(&host(), &push_status(None, &first)),
            "the first connection's request is superseded"
        );
        assert!(hosts.on_message(&host(), &push_status(None, &second)));
    }

    #[test]
    fn a_host_no_longer_open_is_forgotten() {
        let mut hosts = PushHosts::default();
        let asked = host_status_id(&hosts.hosts_open(&[(host(), 1)]));
        assert!(hosts.on_message(&host(), &push_status(None, &asked)));
        hosts.step(&host(), |state, _| state.switch_on(Authorization::Granted));
        let effects = hosts.take_effects();
        let [(_, Effect::Register { request_id })] = effects.as_slice() else {
            panic!("one register: {effects:?}");
        };
        assert!(hosts.token_ready(&host(), request_id));

        assert_eq!(hosts.hosts_open(&[]), Vec::new());
        assert_eq!(hosts.state(&host()), None, "its state is dropped");
        assert!(!hosts.token_ready(&host(), request_id));
        assert!(!hosts.failed(&host(), request_id));
        assert!(!hosts.on_refused(&host(), request_id, "refused"));
        hosts.step(&host(), |state, _| state.switch_on(Authorization::Granted));
        assert_eq!(hosts.take_effects(), Vec::new(), "a step finds no switch");
        assert_eq!(hosts.state(&host()), None);

        let reopened = host_status_id(&hosts.hosts_open(&[(host(), 1)]));
        assert_ne!(&reopened, request_id, "a new switch reuses no request id");
        assert_ne!(reopened, asked);
    }

    #[test]
    fn no_host_has_a_hint_when_every_switch_is_clear() {
        let mut hosts = PushHosts::default();
        let asked = host_status_id(&hosts.hosts_open(&[(host(), 1)]));
        assert!(!hosts.any_hint(), "a status request out is no hint");
        assert!(hosts.on_message(&host(), &push_status(None, &asked)));
        assert!(!hosts.any_hint(), "a switch off carries no hint");

        let mut hosts = PushHosts::default();
        hosts.set_current(MINE);
        let asked = host_status_id(&hosts.hosts_open(&[(host(), 1)]));
        assert!(hosts.on_message(&host(), &push_status(Some(MINE), &asked)));
        assert!(!hosts.any_hint(), "a switch on carries no hint");
    }

    #[test]
    fn a_failed_register_gives_a_hint() {
        let mut hosts = PushHosts::default();
        let asked = host_status_id(&hosts.hosts_open(&[(host(), 1)]));
        assert!(hosts.on_message(&host(), &push_status(None, &asked)));
        hosts.step(&host(), |state, _| state.switch_on(Authorization::Granted));
        let effects = hosts.take_effects();
        let [(_, Effect::Register { request_id })] = effects.as_slice() else {
            panic!("one register: {effects:?}");
        };
        assert!(hosts.failed(&host(), request_id));

        assert!(hosts.any_hint(), "the failed register marks");
        let state = hosts.state(&host()).expect("the host has a switch");
        assert_eq!(state.hint(), Some(NOT_REGISTERED.to_owned()));
        assert_eq!(switch_view(Some(state)).hint, state.hint());
    }

    #[test]
    fn a_forgotten_host_takes_its_hint_with_it() {
        let mut hosts = PushHosts::default();
        let asked = host_status_id(&hosts.hosts_open(&[(host(), 1)]));
        assert!(hosts.on_message(&host(), &push_status(None, &asked)));
        hosts.step(&host(), |state, _| state.switch_on(Authorization::Granted));
        let effects = hosts.take_effects();
        let [(_, Effect::Register { request_id })] = effects.as_slice() else {
            panic!("one register: {effects:?}");
        };
        assert!(hosts.failed(&host(), request_id));
        assert!(hosts.any_hint());

        assert_eq!(hosts.hosts_open(&[]), Vec::new());
        assert!(!hosts.any_hint(), "the forgotten host's hint is gone");
    }

    #[test]
    fn the_registration_carries_the_token_environment_and_keys() {
        let platform = FakePushPlatform::default();
        let registration =
            apns_registration(&platform, MINE.to_owned()).expect("the fake gives keys");
        let keys = FakePushPlatform::keys();
        assert_eq!(
            registration,
            PushRegistration::Apns {
                token: Secret(MINE.to_owned()),
                environment: ApnsEnvironment::Sandbox,
                p256dh: keys.p256dh_b64url().expect("the fixed key is valid"),
                auth: Secret(keys.auth_b64url()),
            }
        );
    }
}
