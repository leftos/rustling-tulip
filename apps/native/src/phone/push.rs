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
use futures::future::BoxFuture;
use protocol::{ApnsEnvironment, DaemonMessage, PushRegistration, Secret};
use push_crypto::InstallKeys;

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
}

/// What a [`FakePushPlatform`] answers.
struct FakeScript {
    supported: bool,
    authorization: Authorization,
    request_answer: Authorization,
    token: Result<String, String>,
    calls: Vec<PlatformCall>,
}

/// A scripted platform for specs: supported, permission granted, a fixed
/// token and keys from fixed bytes, each answer settable, and a record of
/// every question asked. A permission request's answer becomes the
/// permission from then on.
pub struct FakePushPlatform {
    script: Mutex<FakeScript>,
}

impl Default for FakePushPlatform {
    fn default() -> Self {
        Self {
            script: Mutex::new(FakeScript {
                supported: true,
                authorization: Authorization::Granted,
                request_answer: Authorization::Granted,
                token: Ok(FakePushPlatform::TOKEN.to_owned()),
                calls: Vec::new(),
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

/// One host's switch: where it stands, the device token the daemon last
/// reported, and the request a reply settles.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushState {
    /// Where the switch stands.
    pub status: Status,
    /// The daemon's device token for this phone, from its last `PushStatus`.
    pub last_token: Option<String>,
    /// The request id the next reply must carry.
    pending: Option<String>,
    /// How many request ids this host's state has made.
    issued: u64,
}

impl PushState {
    /// The request id a reply must echo to settle the work out.
    #[must_use]
    pub fn pending(&self) -> Option<&str> {
        self.pending.as_deref()
    }

    /// Whether the switch shows on: on, or a registration out.
    #[must_use]
    pub fn switched_on(&self) -> bool {
        matches!(self.status, Status::On | Status::Registering)
    }

    /// A fresh request id, remembered as the one a reply must carry.
    fn issue(&mut self) -> String {
        self.issued += 1;
        let request_id = format!("push-{}", self.issued);
        self.pending = Some(request_id.clone());
        request_id
    }

    /// Whether `request_id` is the one out.
    fn owes(&self, request_id: Option<&str>) -> bool {
        request_id.is_some() && self.pending.as_deref() == request_id
    }

    /// The host's connection opened: ask the daemon for this phone's push
    /// state.
    pub fn on_open(&mut self) -> Vec<Effect> {
        vec![Effect::SendGetStatus {
            request_id: self.issue(),
        }]
    }

    /// The daemon's answer for the work out: no token reads off; this
    /// phone's `current` token reads on; another token re-registers when
    /// the current one is known, and reads on until it is.
    pub fn on_status(&mut self, device_token: Option<&str>, current: Option<&str>) -> Vec<Effect> {
        self.pending = None;
        self.last_token = device_token.map(str::to_owned);
        match (device_token, current) {
            (None, _) => {
                self.status = Status::Off;
                Vec::new()
            }
            (Some(token), Some(current)) if token != current => self.register(),
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
    /// switch shows the daemon's text and nothing is out; `false` when it
    /// answers another.
    pub fn on_error(&mut self, request_id: Option<&str>, message: &str) -> bool {
        if !self.owes(request_id) {
            return false;
        }
        self.pending = None;
        self.status = Status::Failed(message.to_owned());
        true
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
    /// asks, granted registers, denied shows blocked and asks nothing. A
    /// registration already out takes no second.
    pub fn switch_on(&mut self, auth: Authorization) -> Vec<Effect> {
        if self.status == Status::Registering {
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
    /// shows blocked, no answer leaves the switch off.
    pub fn authorization_answered(&mut self, auth: Authorization) -> Vec<Effect> {
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

    /// The platform gave no device token or keys for the registration out:
    /// nothing is out, and the switch says it could not turn on.
    pub fn failed(&mut self) {
        self.pending = None;
        self.status = Status::Failed(NOT_REGISTERED.to_owned());
    }

    /// The switch turned off: unregister, the switch off at once. Work
    /// already out takes no unregister.
    pub fn switch_off(&mut self) -> Vec<Effect> {
        if self.pending.is_some() {
            return Vec::new();
        }
        self.status = Status::Off;
        vec![Effect::Unregister {
            request_id: self.issue(),
        }]
    }

    /// A registration out.
    fn register(&mut self) -> Vec<Effect> {
        self.status = Status::Registering;
        vec![Effect::Register {
            request_id: self.issue(),
        }]
    }
}

/// What a host's switch draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchView {
    /// Whether it shows on.
    pub on: bool,
    /// Whether it shows dimmed: a registration out.
    pub busy: bool,
    /// The line under it, if any.
    pub hint: Option<String>,
}

/// What `state`'s switch draws; a host with no state yet shows off.
#[must_use]
pub fn switch_view(state: Option<&PushState>) -> SwitchView {
    let status = state.map(|state| &state.status);
    SwitchView {
        on: state.is_some_and(PushState::switched_on),
        busy: status == Some(&Status::Registering),
        hint: match status {
            Some(Status::Blocked) => Some(BLOCKED.to_owned()),
            Some(Status::Failed(text)) => Some(text.clone()),
            _ => None,
        },
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

/// Every saved host's switch, this phone's device token once known, and the
/// effects the steps made that the phone's root has not run yet.
#[derive(Debug, Default)]
pub struct PushHosts {
    hosts: HashMap<HostKey, PushState>,
    current: Option<String>,
    outbox: Vec<(HostKey, Effect)>,
}

impl PushHosts {
    /// `host`'s switch, once it has one.
    #[must_use]
    pub fn state(&self, host: &HostKey) -> Option<&PushState> {
        self.hosts.get(host)
    }

    /// This phone's device token, once known.
    #[must_use]
    pub fn current_token(&self) -> Option<&str> {
        self.current.as_deref()
    }

    /// Runs `step` on `host`'s switch with this phone's device token,
    /// queueing the effects it makes.
    pub fn step(
        &mut self,
        host: &HostKey,
        step: impl FnOnce(&mut PushState, Option<&str>) -> Vec<Effect>,
    ) {
        let state = self.hosts.entry(host.clone()).or_default();
        let effects = step(state, self.current.as_deref());
        self.outbox
            .extend(effects.into_iter().map(|effect| (host.clone(), effect)));
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

    /// Folds a daemon message from `host`: a `PushStatus` or `Error` that
    /// answers the switch's request out. Whether the switch changed.
    pub fn on_message(&mut self, host: &HostKey, msg: &DaemonMessage) -> bool {
        let Some(state) = self.hosts.get_mut(host) else {
            return false;
        };
        match msg {
            DaemonMessage::PushStatus {
                device_token,
                request_id,
                ..
            } => {
                let token = device_token.as_ref().map(|token| token.0.as_str());
                let current = self.current.as_deref();
                let Some(effects) = state.status_reply(request_id.as_deref(), token, current)
                else {
                    return false;
                };
                self.outbox
                    .extend(effects.into_iter().map(|effect| (host.clone(), effect)));
                true
            }
            DaemonMessage::Error {
                message,
                request_id,
                ..
            } => state.on_error(request_id.as_deref(), message),
            _ => false,
        }
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
        assert!(!state.on_error(Some("other"), "refused"));
        assert_eq!(state.status, Status::Unknown);
        assert_eq!(state.pending(), Some(id.as_str()));
    }

    #[test]
    fn an_error_for_the_request_out_shows_its_text() {
        let (mut state, id) = opened();
        assert!(state.on_error(Some(&id), "this device is not paired"));
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
        register_id(&state.switch_on(Authorization::Granted));
        state.failed();
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
        let registering = PushState {
            status: Status::Registering,
            ..PushState::default()
        };
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
        hosts.step(&host(), |state, _| state.on_open());
        let effects = hosts.take_effects();
        let [(_, Effect::SendGetStatus { request_id })] = effects.as_slice() else {
            panic!("one GetPushStatus: {effects:?}");
        };
        assert!(!hosts.on_message(&host(), &push_status(None, "other")));
        assert!(hosts.on_message(&host(), &push_status(None, request_id)));
        assert_eq!(
            hosts.state(&host()).map(|state| state.status.clone()),
            Some(Status::Off)
        );
    }

    #[test]
    fn learning_this_phones_token_reregisters_a_host_on_for_another() {
        let mut hosts = PushHosts::default();
        hosts.step(&host(), |state, _| state.on_open());
        let effects = hosts.take_effects();
        let [(_, Effect::SendGetStatus { request_id })] = effects.as_slice() else {
            panic!("one GetPushStatus: {effects:?}");
        };
        assert!(hosts.on_message(&host(), &push_status(Some(OTHER), request_id)));
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
