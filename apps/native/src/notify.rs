//! OS notifications for the daemon's attention events: a session awaiting
//! input raises Needs you, whose body names the session and the prompt it
//! waits on, and one whose turn ended raises Finished, held for the session's
//! `WaitingAlert` while the summarizer can still write one and worded from
//! it, else titled plainly with the session's label as its body; the toast
//! sender, and Windows' own setting for this app's toasts. An exited or
//! errored session raises none. How a press reaches the client is
//! [`crate::toast_activation`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui::Context;
use protocol::{
    AgentDescriptor, AlertKind, AttentionReason, DaemonMessage, KeyState, SessionMode,
    SessionSnapshot, SessionStatus, SummarizerKey, SummarizerSettings,
};

use crate::RootView;
use crate::needs_you::{self, Reason};
use crate::pairing::{PairingAction, PairingToast};
use crate::sidebar::{NotifySkip, display_label};
use crate::target::HostKey;
#[cfg(windows)]
use crate::toast_activation;
use crate::window_title::PRODUCT_NAME;

/// The URI of Windows' notification settings page.
pub(crate) const WINDOWS_NOTIFICATION_SETTINGS: &str = "ms-settings:notifications";

/// Whether Windows lets this app's toasts show.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NotifyState {
    On,
    /// Turned off for this app or for the user, or by policy.
    Blocked,
    /// The setting could not be read.
    #[default]
    Unknown,
}

impl NotifyState {
    /// How the Notifications tab words the state.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Blocked => "blocked by Windows",
            Self::Unknown => "unknown",
        }
    }

    /// How the Notifications tab words `state`, `None` while it is read.
    #[must_use]
    pub const fn line_label(state: Option<Self>) -> &'static str {
        match state {
            Some(state) => state.label(),
            None => "checking…",
        }
    }
}

/// How a press on a pairing toast reaches the app: the action is handed to
/// this sink on whichever thread the OS raises it from.
pub type PairingActionSink = Arc<dyn Fn(PairingAction) + Send + Sync>;

/// Shows OS notifications.
pub trait Notifier: Send + Sync {
    /// Shows a toast titled `title` with `body`. Called off the UI thread;
    /// a failure is logged, never returned.
    fn notify(&self, title: &str, body: &str);

    /// Raises the pairing toast for `toast`, its buttons handing what was
    /// pressed to `on_action`. Called off the UI thread; a failure is logged,
    /// never returned. A notifier that raises no pairing toast leaves this
    /// as it is.
    fn notify_pairing(&self, _toast: &PairingToast, _on_action: PairingActionSink) {}

    /// Whether Windows lets this app's toasts show.
    fn state(&self) -> NotifyState;
}

/// The notifier the client runs with: Windows toasts under the client's own
/// app id once its toast activator is registered, whose presses reach the
/// client through the activator; otherwise under the PowerShell app id,
/// where only a pairing toast's press from its popup reaches the client.
pub struct SystemNotifier;

#[cfg(windows)]
impl Notifier for SystemNotifier {
    fn notify(&self, title: &str, body: &str) {
        let xml = toast_activation::toast_xml(title, body, &[], toast_activation::registered());
        if let Err(err) = show_toast(&xml, None) {
            tracing::warn!("notification {title:?} was not shown: {err}");
        }
    }

    fn notify_pairing(&self, toast: &PairingToast, on_action: PairingActionSink) {
        let registered = toast_activation::registered();
        let xml = toast_activation::pairing_toast_xml(toast, registered);
        // Once registered, every press reaches the client through the
        // activator; an in-process handler too would apply it twice.
        let in_process = (!registered).then_some(on_action);
        if let Err(err) = show_toast(&xml, in_process) {
            tracing::warn!(
                "pairing notification {} was not shown: {err}",
                toast.request_id
            );
        }
    }

    fn state(&self) -> NotifyState {
        match toasts_enabled() {
            Ok(true) => NotifyState::On,
            Ok(false) => NotifyState::Blocked,
            Err(err) => {
                tracing::warn!("could not read Windows' notification setting: {err}");
                NotifyState::Unknown
            }
        }
    }
}

/// Shows the toast `xml` under the app id toasts are raised under, handing
/// a press on it to `on_action` in-process when one is given.
#[cfg(windows)]
fn show_toast(xml: &str, on_action: Option<PairingActionSink>) -> windows::core::Result<()> {
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::Notifications::{
        ToastActivatedEventArgs, ToastNotification, ToastNotificationManager,
    };
    use windows::core::{HSTRING, IInspectable, Interface as _, Ref};

    let document = XmlDocument::new()?;
    document.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&document)?;
    if let Some(on_action) = on_action {
        toast.Activated(&TypedEventHandler::new(
            move |_: Ref<'_, ToastNotification>, args: Ref<'_, IInspectable>| {
                let arg = match args.as_ref() {
                    Some(args) => Some(
                        args.cast::<ToastActivatedEventArgs>()?
                            .Arguments()?
                            .to_string(),
                    ),
                    None => None,
                };
                if let Some(action) = PairingAction::from_activation(arg.as_deref()) {
                    on_action(action);
                }
                Ok(())
            },
        ))?;
    }
    let app_id = HSTRING::from(toast_activation::app_id());
    ToastNotificationManager::CreateToastNotifierWithId(&app_id)?.Show(&toast)
}

/// Whether Windows lets toasts under the app id toasts are raised under
/// show.
#[cfg(windows)]
fn toasts_enabled() -> windows::core::Result<bool> {
    use windows::UI::Notifications::{NotificationSetting, ToastNotificationManager};
    use windows::core::HSTRING;
    let app_id = HSTRING::from(toast_activation::app_id());
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&app_id)?;
    Ok(notifier.Setting()? == NotificationSetting::Enabled)
}

#[cfg(not(windows))]
impl Notifier for SystemNotifier {
    fn notify(&self, title: &str, _: &str) {
        tracing::debug!("no OS notifications on this platform: {title:?}");
    }

    fn notify_pairing(&self, toast: &PairingToast, _on_action: PairingActionSink) {
        tracing::debug!(
            "no OS notifications on this platform: pairing {}",
            toast.request_id
        );
    }

    fn state(&self) -> NotifyState {
        NotifyState::Unknown
    }
}

/// A notifier that shows nothing, for the cloaked smoke-tier window, which
/// must never raise a real toast.
pub struct SilentNotifier;

impl Notifier for SilentNotifier {
    fn notify(&self, title: &str, _: &str) {
        tracing::debug!("offscreen window: notification {title:?} not shown");
    }

    fn notify_pairing(&self, toast: &PairingToast, _on_action: PairingActionSink) {
        tracing::debug!(
            "offscreen window: pairing notification {} not shown",
            toast.request_id
        );
    }

    fn state(&self) -> NotifyState {
        NotifyState::Unknown
    }
}

/// The root view's notification state.
pub(crate) struct Notifications {
    /// Shows the notifications, on background threads.
    pub(crate) notifier: Arc<dyn Notifier>,
    /// Windows' toast setting as last read when the Notifications tab
    /// opened; `None` while a read is out.
    pub(crate) state: Option<NotifyState>,
    /// Each host's summarizer status as last reported; a host has none
    /// before one arrives and after every reconnect, so a newly chosen host
    /// never inherits "on".
    summaries: HashMap<HostKey, Summaries>,
    /// Finished toasts waiting for their session's alert, by host and
    /// session id.
    held: HashMap<SessionKey, Held>,
    /// Each session's latest alert, kept for a Finished toast the alert came
    /// too early to word, by host and session id.
    alerts: HashMap<SessionKey, Alert>,
    /// The generation the next hold is given.
    next_hold: u64,
}

/// A session as the notifications know it: its host and its id.
type SessionKey = (HostKey, String);

/// The key of session `id` on `host`.
fn key_of(host: &HostKey, id: &str) -> SessionKey {
    (host.clone(), id.to_owned())
}

impl Notifications {
    pub(crate) fn new(notifier: Arc<dyn Notifier>) -> Self {
        Self {
            notifier,
            state: None,
            summaries: HashMap::new(),
            held: HashMap::new(),
            alerts: HashMap::new(),
            next_hold: 0,
        }
    }

    /// Whether a Finished toast from now about a session on `host` waits
    /// for its alert: only while that host's summarizer reports it can
    /// still write one.
    fn holds(&self, host: &HostKey) -> bool {
        self.summaries.get(host).is_some_and(|s| s.active())
    }

    /// Forgets what `host`'s old connection left: its summarizer's status
    /// and its sessions' held toasts and alerts.
    fn forget_host(&mut self, host: &HostKey) {
        self.summaries.remove(host);
        self.held.retain(|(owner, _), _| owner != host);
        self.alerts.retain(|(owner, _), _| owner != host);
    }

    /// Folds what `msg` from `host` says about the summarizer, held toasts
    /// and alerts; true when `msg` raises nothing further.
    fn fold_state(&mut self, host: &HostKey, msg: &DaemonMessage) -> bool {
        match msg {
            DaemonMessage::SummarizerStatus {
                settings,
                keys,
                cap_reached,
                ..
            } => {
                self.summaries
                    .insert(host.clone(), Summaries::of(settings, keys, *cap_reached));
                true
            }
            // A toast held for the old connection has no alert coming, and
            // another daemon's summarizer says nothing about this one's.
            DaemonMessage::Welcome { .. } => {
                self.forget_host(host);
                true
            }
            DaemonMessage::SessionRemoved { session_id } => {
                let key = key_of(host, session_id);
                self.held.remove(&key);
                self.alerts.remove(&key);
                true
            }
            // A relist that has a session working, waiting, exited or gone
            // leaves its held toast with no alert to word it, and a session
            // at work has an alert of its own coming.
            DaemonMessage::Sessions { sessions } => {
                let keep = |(owner, id): &SessionKey| {
                    owner != host
                        || sessions.iter().any(|session| {
                            session.id == *id && session.status == SessionStatus::Idle
                        })
                };
                self.held.retain(|key, _| keep(key));
                self.alerts.retain(|key, _| keep(key));
                false
            }
            DaemonMessage::SessionUpdated { session, .. } => {
                let key = key_of(host, &session.id);
                if session.status != SessionStatus::Idle {
                    self.held.remove(&key);
                }
                if session.status == SessionStatus::Working {
                    self.alerts.remove(&key);
                }
                false
            }
            _ => false,
        }
    }
}

/// The title of the notification a session awaiting input raises.
pub(crate) const NEEDS_YOU_TITLE: &str = "Needs you";

/// The title of the notification a session raises when its turn ends.
pub(crate) const FINISHED_TITLE: &str = "Finished";

/// How long a Finished toast waits for its session's `WaitingAlert` before
/// it falls back to the plain wording.
pub(crate) const FINISHED_HOLD: Duration = Duration::from_secs(12);

/// The title an alert's `kind` words for a toast about `label`, or `None`
/// when the kind words none: a template alert falls back to
/// [`FINISHED_TITLE`] and the label alone.
fn alert_title(kind: Option<AlertKind>, label: &str) -> Option<String> {
    match kind {
        Some(AlertKind::NeedsAnswer) => Some(format!("{label} needs your answer")),
        Some(AlertKind::DoneWaiting) => Some(format!("{label} finished")),
        Some(AlertKind::WorkingUpdate) => Some(format!("{label} posted an update")),
        Some(AlertKind::Unknown) | None => None,
    }
}

/// A Finished toast waiting for its session's alert.
struct Held {
    /// The session's label: the fallback toast's body, and the name a worded
    /// title starts with.
    label: String,
    /// Which hold this is, so the timer of one that was dropped raises
    /// nothing once its session is held again.
    generation: u64,
}

/// The last alert a session raised, as a Finished toast words itself from it.
struct Alert {
    /// What the summary says the session needs; `None` when a template wrote
    /// it and could not tell.
    kind: Option<AlertKind>,
    /// At most 100 characters: the worded toast's body.
    notification: String,
}

/// The summarizer's last reported state, as far as a held Finished toast
/// cares: only a summary that can still come is worth waiting for.
#[derive(Debug, Clone, Copy)]
struct Summaries {
    /// Whether the owner has summaries on.
    enabled: bool,
    /// Whether the day's spending cap has been reached.
    cap_reached: bool,
    /// Whether the selected provider's key is one a call can use.
    key_usable: bool,
}

impl Summaries {
    /// The state `settings`, `keys` and `cap_reached` report.
    fn of(settings: &SummarizerSettings, keys: &[SummarizerKey], cap_reached: bool) -> Self {
        let key_usable = keys
            .iter()
            .find(|key| key.provider == settings.provider)
            .is_none_or(|key| {
                !matches!(
                    key.state,
                    KeyState::Missing
                        | KeyState::EnvMissing
                        | KeyState::Rejected
                        | KeyState::Unreachable
                )
            });
        Self {
            enabled: settings.enabled,
            cap_reached,
            key_usable,
        }
    }

    /// Whether a summary can still come: summaries on, under the cap, with a
    /// key the provider accepts.
    fn active(self) -> bool {
        self.enabled && !self.cap_reached && self.key_usable
    }
}

/// One toast ready to raise, unless its toggle is off or the skip setting
/// says the owner is looking at the session already.
struct Toast<'a> {
    /// The session it reports.
    session_id: &'a str,
    title: &'a str,
    body: &'a str,
    /// Whether the setting's toggle for this kind of toast is on.
    wanted: bool,
}

/// Whether a notification for a session is skipped under `skip_when`:
/// `focused` is whether the client window has the keyboard, `on_screen`
/// answers whether a pane of its active tab shows the session, and is
/// asked only for `OnScreen` with the window focused.
#[must_use]
pub(crate) fn skips(
    skip_when: NotifySkip,
    focused: bool,
    on_screen: impl FnOnce() -> bool,
) -> bool {
    match skip_when {
        NotifySkip::OnScreen => focused && on_screen(),
        NotifySkip::Focused => focused,
        NotifySkip::Never => false,
    }
}

/// The notification's body: the session's label, or the product name for
/// a session the client does not know or whose only name is its id.
#[must_use]
pub(crate) fn body(session: Option<&SessionSnapshot>, agents: &[AgentDescriptor]) -> String {
    session
        .map(|s| (display_label(s, agents), s.id.as_str()))
        .filter(|(label, id)| label != id)
        .map_or_else(|| PRODUCT_NAME.to_owned(), |(label, _)| label)
}

/// The body a session awaiting input raises: its label, then what it waits
/// for as the Needs You list words it, so the toast never loses which
/// session it is about.
fn needs_you_body(session: Option<&SessionSnapshot>, agents: &[AgentDescriptor]) -> String {
    let line = session.map_or_else(
        || needs_you::WAITING_FOR_INPUT.to_owned(),
        |session| needs_you::detail_of(Reason::Asking, session, agents),
    );
    format!("{}: {line}", body(session, agents))
}

/// Whether `after` ends a turn the session was running: it is idle with no
/// background work left, an agent rather than a plain shell, and the copy the
/// client still held was working, or idle waiting on background work the
/// agent left running. A session seen for the first time raises nothing.
#[must_use]
pub(crate) fn finished(before: Option<&SessionSnapshot>, after: &SessionSnapshot) -> bool {
    if after.status != SessionStatus::Idle
        || after.background
        || after.mode == SessionMode::PlainShell
    {
        return false;
    }
    before.is_some_and(|before| {
        before.status == SessionStatus::Working
            || (before.status == SessionStatus::Idle && before.background)
    })
}

impl RootView {
    /// Folds `msg` into the notifications: the summarizer's status decides
    /// whether a Finished toast waits; a session awaiting input raises Needs
    /// you, and one `finished` marks as having ended its turn raises
    /// Finished, held for the session's alert while summaries can still come;
    /// an alert words the toast held for its session, or is kept for a toast
    /// still to come. `window_focused` is whether this client window has the
    /// keyboard.
    pub(crate) fn notify_on(
        &mut self,
        host: &HostKey,
        msg: &DaemonMessage,
        finished: bool,
        window_focused: bool,
        cx: &mut Context<Self>,
    ) {
        if self.notifications.fold_state(host, msg) {
            return;
        }
        match msg {
            DaemonMessage::SessionUpdated { session, .. } if finished => {
                let label = body(Some(session), self.sidebar.agents(host));
                if !self.waits_for_alert(host, session) {
                    let wanted = self.sidebar.ui_state().notifications.finished;
                    self.raise_toast(
                        &Toast {
                            session_id: &session.id,
                            title: FINISHED_TITLE,
                            body: &label,
                            wanted,
                        },
                        window_focused,
                        cx,
                    );
                } else if let Some(alert) =
                    self.notifications.alerts.remove(&key_of(host, &session.id))
                {
                    self.raise_alert(&session.id, &label, &alert, window_focused, cx);
                } else {
                    self.hold_finished(key_of(host, &session.id), label, cx);
                }
            }
            DaemonMessage::Attention {
                session_id,
                reason: AttentionReason::AwaitingInput,
                ..
            } => {
                let wanted = self.sidebar.ui_state().notifications.awaiting_input;
                let body =
                    needs_you_body(self.sidebar.session(session_id), self.sidebar.agents(host));
                self.raise_toast(
                    &Toast {
                        session_id,
                        title: NEEDS_YOU_TITLE,
                        body: &body,
                        wanted,
                    },
                    window_focused,
                    cx,
                );
            }
            DaemonMessage::WaitingAlert {
                session_id,
                kind,
                notification,
                ..
            } => {
                let alert = Alert {
                    kind: *kind,
                    notification: notification.clone(),
                };
                let key = key_of(host, session_id);
                self.release_hold(&key, &alert, window_focused, cx);
                // A Stop leaves its session Idle with background work still
                // running, so the alert for that turn can beat its end.
                self.notifications.alerts.insert(key, alert);
            }
            _ => {}
        }
    }

    /// Whether a Finished toast about `session` on `host` waits for its
    /// alert: that host's summarizer can still write one, and writes about
    /// this session at all — an interactive run of a registered agent.
    fn waits_for_alert(&self, host: &HostKey, session: &SessionSnapshot) -> bool {
        self.notifications.holds(host)
            && session.mode != SessionMode::Headless
            && session.agent.is_some()
    }

    /// Raises `toast` unless its toggle is off or the skip setting says the
    /// owner is looking at the session already.
    fn raise_toast(&self, toast: &Toast<'_>, window_focused: bool, cx: &mut Context<Self>) {
        if !toast.wanted {
            return;
        }
        let skip_when = self.sidebar.ui_state().notifications.skip_when;
        // A session a pop-out shows with its active window's keyboard is on
        // screen in a focused window of this client.
        let in_popout = self
            .popout_focused_session()
            .is_some_and(|shown| shown.id == toast.session_id);
        if skips(skip_when, window_focused || in_popout, || {
            in_popout || self.session_on_screen(toast.session_id)
        }) {
            return;
        }
        let title = toast.title.to_owned();
        let body = toast.body.to_owned();
        let notifier = Arc::clone(&self.notifications.notifier);
        cx.background_executor()
            .spawn(async move { notifier.notify(&title, &body) })
            .detach();
    }

    /// Holds a Finished toast about `label` for session `key`'s alert for
    /// [`FINISHED_HOLD`], raising the plain one if none comes.
    fn hold_finished(&mut self, key: SessionKey, label: String, cx: &mut Context<Self>) {
        self.notifications.next_hold += 1;
        let generation = self.notifications.next_hold;
        self.notifications
            .held
            .insert(key.clone(), Held { label, generation });
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FINISHED_HOLD).await;
            // Fails only when the view is gone, and the hold with it.
            this.update(cx, |this, cx| {
                this.hold_expired(&key, generation, cx);
            })
            .ok();
        })
        .detach();
    }

    /// The hold's time is up with no alert having worded it: the plain toast
    /// is raised, under the toggle and skip rule as they stand now.
    fn hold_expired(&mut self, key: &SessionKey, generation: u64, cx: &mut Context<Self>) {
        if self
            .notifications
            .held
            .get(key)
            .is_none_or(|held| held.generation != generation)
        {
            return;
        }
        let Some(held) = self.notifications.held.remove(key) else {
            return;
        };
        let session_id = key.1.as_str();
        let focused = cx
            .active_window()
            .is_some_and(|window| window.window_id() == self.layouts.main());
        let wanted = self.sidebar.ui_state().notifications.finished;
        self.raise_toast(
            &Toast {
                session_id,
                title: FINISHED_TITLE,
                body: &held.label,
                wanted,
            },
            focused,
            cx,
        );
    }

    /// Ends the toast held for session `key` with its alert's wording. An
    /// alert with no held toast behind it raises nothing.
    fn release_hold(
        &mut self,
        key: &SessionKey,
        alert: &Alert,
        window_focused: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(held) = self.notifications.held.remove(key) else {
            return;
        };
        self.raise_alert(&key.1, &held.label, alert, window_focused, cx);
    }

    /// Raises the Finished toast about `session_id` that `alert` words, or
    /// the plain one when the alert's kind words none (a template), under the
    /// toggle and skip rule as they stand now.
    fn raise_alert(
        &self,
        session_id: &str,
        label: &str,
        alert: &Alert,
        window_focused: bool,
        cx: &mut Context<Self>,
    ) {
        let (title, body) = alert_title(alert.kind, label).map_or_else(
            || (FINISHED_TITLE.to_owned(), label.to_owned()),
            |title| (title, alert.notification.clone()),
        );
        let wanted = self.sidebar.ui_state().notifications.finished;
        self.raise_toast(
            &Toast {
                session_id,
                title: &title,
                body: &body,
                wanted,
            },
            window_focused,
            cx,
        );
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;

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

    /// `skip_when`'s verdict for (focused, on screen): both, focused only,
    /// on screen only, neither.
    fn verdicts(skip_when: NotifySkip) -> [bool; 4] {
        [(true, true), (true, false), (false, true), (false, false)]
            .map(|(focused, on_screen)| skips(skip_when, focused, || on_screen))
    }

    #[test]
    fn on_screen_skips_only_when_focused_and_visible() {
        assert_eq!(verdicts(NotifySkip::OnScreen), [true, false, false, false]);
    }

    #[test]
    fn focused_skips_whenever_the_window_is_focused() {
        assert_eq!(verdicts(NotifySkip::Focused), [true, true, false, false]);
    }

    #[test]
    fn never_skips_nothing() {
        assert_eq!(verdicts(NotifySkip::Never), [false; 4]);
    }

    #[test]
    fn body_is_label_or_product_name() {
        let labelled = snapshot(&serde_json::json!({ "user_label": "fix login" }));
        assert_eq!(body(Some(&labelled), &[]), "fix login");
        assert_eq!(body(None, &[]), "rustling-tulip", "an unknown session");
        let bare = snapshot(&serde_json::json!({ "label": "", "mode": "plain_shell" }));
        assert_eq!(
            body(Some(&bare), &[]),
            "rustling-tulip",
            "named only by its id"
        );
    }

    #[test]
    fn alert_title_per_kind() {
        assert_eq!(
            alert_title(Some(AlertKind::NeedsAnswer), "review").as_deref(),
            Some("review needs your answer")
        );
        assert_eq!(
            alert_title(Some(AlertKind::DoneWaiting), "review").as_deref(),
            Some("review finished")
        );
        assert_eq!(
            alert_title(Some(AlertKind::WorkingUpdate), "review").as_deref(),
            Some("review posted an update")
        );
    }

    #[test]
    fn alert_title_for_none_and_unknown_falls_back() {
        assert_eq!(alert_title(None, "review"), None, "a template alert");
        assert_eq!(
            alert_title(Some(AlertKind::Unknown), "review"),
            None,
            "a kind this build does not know"
        );
    }

    #[test]
    fn finished_fires_when_working_goes_idle() {
        let working = snapshot(&serde_json::json!({ "status": "working" }));
        let idle = snapshot(&serde_json::json!({ "status": "idle" }));
        assert!(finished(Some(&working), &idle));
    }

    #[test]
    fn finished_waits_while_background_work_runs() {
        let working = snapshot(&serde_json::json!({ "status": "working" }));
        let holding = snapshot(&serde_json::json!({ "status": "idle", "background": true }));
        assert!(!finished(Some(&working), &holding));
    }

    #[test]
    fn finished_fires_when_background_work_ends() {
        let holding = snapshot(&serde_json::json!({ "status": "idle", "background": true }));
        let idle = snapshot(&serde_json::json!({ "status": "idle" }));
        assert!(finished(Some(&holding), &idle));
    }

    #[test]
    fn finished_ignores_a_plain_shell() {
        let working = snapshot(&serde_json::json!({ "status": "working", "mode": "plain_shell" }));
        let idle = snapshot(&serde_json::json!({ "status": "idle", "mode": "plain_shell" }));
        assert!(!finished(Some(&working), &idle), "a shell has no turn");
    }

    #[test]
    fn finished_ignores_a_first_sighting() {
        let idle = snapshot(&serde_json::json!({ "status": "idle" }));
        assert!(!finished(None, &idle), "nothing says a turn ended");
    }

    #[test]
    fn finished_ignores_a_prompt_answered_to_idle() {
        let asking = snapshot(&serde_json::json!({ "status": "awaiting_input" }));
        let idle = snapshot(&serde_json::json!({ "status": "idle" }));
        assert!(
            !finished(Some(&asking), &idle),
            "an answer is no turn's end"
        );
    }

    #[test]
    fn silent_notifier_raises_no_pairing_toast() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let pressed = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&pressed);
        let sink: PairingActionSink = Arc::new(move |_action| {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let toast = PairingToast {
            request_id: "r1".to_owned(),
            title: "A device asks for remote access".to_owned(),
            body: "phone · signed in to Tailscale as alice@example.com".to_owned(),
        };
        SilentNotifier.notify_pairing(&toast, sink);
        assert_eq!(
            pressed.load(Ordering::SeqCst),
            0,
            "nothing is raised, so nothing is pressed"
        );
        assert_eq!(SilentNotifier.state(), NotifyState::Unknown);
    }

    #[test]
    fn state_labels() {
        assert_eq!(NotifyState::On.label(), "on");
        assert_eq!(NotifyState::Blocked.label(), "blocked by Windows");
        assert_eq!(NotifyState::Unknown.label(), "unknown");
        assert_eq!(NotifyState::line_label(None), "checking…");
        assert_eq!(NotifyState::line_label(Some(NotifyState::On)), "on");
    }
}
