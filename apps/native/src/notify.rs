//! OS notifications for the daemon's attention events: a session awaiting
//! input raises Needs you and one whose turn ended raises Finished, each
//! with the session's label as its body; the toast sender, and Windows' own
//! setting for this app's toasts. An exited or errored session raises none.
//! How a press reaches the client is [`crate::toast_activation`].

use std::sync::Arc;

use gpui::Context;
use protocol::{
    AgentDescriptor, AttentionReason, DaemonMessage, SessionMode, SessionSnapshot, SessionStatus,
};

use crate::RootView;
use crate::pairing::{PairingAction, PairingToast};
use crate::sidebar::{NotifySkip, display_label};
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
}

impl Notifications {
    pub(crate) fn new(notifier: Arc<dyn Notifier>) -> Self {
        Self {
            notifier,
            state: None,
        }
    }
}

/// The title of the notification a session awaiting input raises.
pub(crate) const NEEDS_YOU_TITLE: &str = "Needs you";

/// The title of the notification a session raises when its turn ends.
pub(crate) const FINISHED_TITLE: &str = "Finished";

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
    /// Folds `msg` into the notifications: a session awaiting input raises
    /// Needs you while its toggle is on, and one `finished` marks as having
    /// ended its turn raises Finished while that toggle is on, unless the
    /// skip setting says the owner is looking at it already. `window_focused`
    /// is whether this client window has the keyboard.
    pub(crate) fn notify_on(
        &self,
        msg: &DaemonMessage,
        finished: bool,
        window_focused: bool,
        cx: &mut Context<Self>,
    ) {
        let (session_id, session, title, wanted) = match msg {
            DaemonMessage::SessionUpdated { session, .. } if finished => {
                let wanted = self.sidebar.ui_state().notifications.finished;
                (session.id.as_str(), Some(session), FINISHED_TITLE, wanted)
            }
            DaemonMessage::Attention {
                session_id,
                reason: AttentionReason::AwaitingInput,
            } => {
                let wanted = self.sidebar.ui_state().notifications.awaiting_input;
                (
                    session_id.as_str(),
                    self.sidebar.session(session_id),
                    NEEDS_YOU_TITLE,
                    wanted,
                )
            }
            _ => return,
        };
        if !wanted {
            return;
        }
        let skip_when = self.sidebar.ui_state().notifications.skip_when;
        // A session a pop-out shows with its active window's keyboard is on
        // screen in a focused window of this client.
        let in_popout = self.popout_focused_session().as_deref() == Some(session_id);
        if skips(skip_when, window_focused || in_popout, || {
            in_popout || self.session_on_screen(session_id)
        }) {
            return;
        }
        let body = body(session, &self.agents);
        let notifier = Arc::clone(&self.notifications.notifier);
        cx.background_executor()
            .spawn(async move { notifier.notify(title, &body) })
            .detach();
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
