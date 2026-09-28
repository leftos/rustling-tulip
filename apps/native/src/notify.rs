//! OS notifications for the daemon's attention events: which reasons fire,
//! their title and body, the toast sender, and Windows' own setting for
//! this app's toasts. A Stop this client sent does not notify it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use gpui::Context;
use protocol::{AttentionReason, ClientMessage, DaemonMessage, SessionSnapshot, SessionStatus};

use crate::RootView;
use crate::sidebar::display_label;
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

/// Shows OS notifications.
pub trait Notifier: Send + Sync {
    /// Shows a toast titled `title` with `body`. Called off the UI thread;
    /// a failure is logged, never returned.
    fn notify(&self, title: &str, body: &str);

    /// Whether Windows lets this app's toasts show.
    fn state(&self) -> NotifyState;
}

/// The notifier the client runs with: Windows toasts under the PowerShell
/// app id, as the client has no registered app id of its own.
pub struct SystemNotifier;

#[cfg(windows)]
impl Notifier for SystemNotifier {
    fn notify(&self, title: &str, body: &str) {
        use tauri_winrt_notification::Toast;
        let toast = Toast::new(Toast::POWERSHELL_APP_ID)
            .title(title)
            .text1(body);
        if let Err(err) = toast.show() {
            tracing::warn!("notification {title:?} was not shown: {err}");
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

/// Whether Windows lets toasts under the PowerShell app id show.
#[cfg(windows)]
fn toasts_enabled() -> windows::core::Result<bool> {
    use windows::UI::Notifications::{NotificationSetting, ToastNotificationManager};
    use windows::core::HSTRING;
    let app_id = HSTRING::from(tauri_winrt_notification::Toast::POWERSHELL_APP_ID);
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&app_id)?;
    Ok(notifier.Setting()? == NotificationSetting::Enabled)
}

#[cfg(not(windows))]
impl Notifier for SystemNotifier {
    fn notify(&self, title: &str, _: &str) {
        tracing::debug!("no OS notifications on this platform: {title:?}");
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

    fn state(&self) -> NotifyState {
        NotifyState::Unknown
    }
}

/// The root view's notification state.
pub(crate) struct Notifications {
    /// Shows the notifications, on background threads.
    pub(crate) notifier: Arc<dyn Notifier>,
    /// The sessions this client sent a Stop for, each with whether the
    /// daemon has reported it stopped since. Their Stopped attentions stay
    /// quiet until the session is removed, runs again after it was seen
    /// stopped, or the connection is new. Filled from the root's `send`,
    /// which takes `&self`.
    pub(crate) own_stops: RefCell<HashMap<String, bool>>,
    /// Windows' toast setting as last read when the Notifications tab
    /// opened; `None` while a read is out.
    pub(crate) state: Option<NotifyState>,
}

impl Notifications {
    pub(crate) fn new(notifier: Arc<dyn Notifier>) -> Self {
        Self {
            notifier,
            own_stops: RefCell::default(),
            state: None,
        }
    }

    /// Records the sessions `messages` stop.
    pub(crate) fn note_stops<'a>(&self, messages: impl IntoIterator<Item = &'a ClientMessage>) {
        let mut own = self.own_stops.borrow_mut();
        for msg in messages {
            if let ClientMessage::StopSession { session_id, .. } = msg {
                own.insert(session_id.clone(), false);
            }
        }
    }

    /// Folds a session snapshot in: a stopped one is marked seen, and one
    /// seen stopped that runs again is forgotten.
    fn on_session(&self, session: &SessionSnapshot) {
        let mut own = self.own_stops.borrow_mut();
        let Some(seen_stopped) = own.get_mut(&session.id) else {
            return;
        };
        if session.status == SessionStatus::Stopped {
            *seen_stopped = true;
        } else if *seen_stopped {
            own.remove(&session.id);
        }
    }

    /// Whether a Stopped attention for `session_id` is this client's own
    /// stop.
    fn is_own_stop(&self, session_id: &str) -> bool {
        self.own_stops.borrow().contains_key(session_id)
    }
}

/// The notification's title for `reason`.
#[must_use]
pub(crate) const fn title(reason: AttentionReason) -> &'static str {
    match reason {
        AttentionReason::AwaitingInput => "Claude is awaiting input",
        AttentionReason::Stopped => "Claude session stopped",
        AttentionReason::Error => "Claude session errored",
    }
}

/// The notification's body: the session's label, or the product name for
/// a session the client does not know or whose only name is its id.
#[must_use]
pub(crate) fn body(session: Option<&SessionSnapshot>) -> String {
    session
        .map(|s| (display_label(s), s.id.as_str()))
        .filter(|(label, id)| label != id)
        .map_or_else(|| PRODUCT_NAME.to_owned(), |(label, _)| label)
}

impl RootView {
    /// Folds `msg` into the notifications: an attention may fire one, and
    /// a removal or a new connection forgets this client's stops.
    pub(crate) fn notify_on(&self, msg: &DaemonMessage, cx: &mut Context<Self>) {
        match msg {
            DaemonMessage::Attention { session_id, reason } => {
                self.on_attention(session_id, *reason, cx);
            }
            DaemonMessage::SessionUpdated { session, .. } => {
                self.notifications.on_session(session);
            }
            DaemonMessage::Sessions { sessions } => {
                for session in sessions {
                    self.notifications.on_session(session);
                }
            }
            DaemonMessage::SessionRemoved { session_id } => {
                self.notifications.own_stops.borrow_mut().remove(session_id);
            }
            DaemonMessage::Welcome { .. } => self.notifications.own_stops.borrow_mut().clear(),
            _ => {}
        }
    }

    fn on_attention(&self, session_id: &str, reason: AttentionReason, cx: &mut Context<Self>) {
        if reason == AttentionReason::Stopped && self.notifications.is_own_stop(session_id) {
            return;
        }
        if !self.sidebar.ui_state().notifications.fires(reason) {
            return;
        }
        let session = self.sidebar.sessions().iter().find(|s| s.id == session_id);
        let body = body(session);
        let notifier = Arc::clone(&self.notifications.notifier);
        cx.background_executor()
            .spawn(async move { notifier.notify(title(reason), &body) })
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

    #[test]
    fn titles_are_tauri_strings() {
        assert_eq!(
            title(AttentionReason::AwaitingInput),
            "Claude is awaiting input"
        );
        assert_eq!(title(AttentionReason::Stopped), "Claude session stopped");
        assert_eq!(title(AttentionReason::Error), "Claude session errored");
    }

    #[test]
    fn body_is_label_or_product_name() {
        let labelled = snapshot(&serde_json::json!({ "user_label": "fix login" }));
        assert_eq!(body(Some(&labelled)), "fix login");
        assert_eq!(body(None), "rustling-tulip", "an unknown session");
        let bare = snapshot(&serde_json::json!({ "label": "", "mode": "plain_shell" }));
        assert_eq!(body(Some(&bare)), "rustling-tulip", "named only by its id");
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
