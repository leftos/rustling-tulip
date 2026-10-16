//! OS notifications for the daemon's attention events: a session awaiting
//! input raises Needs you, titled so with the session's label as its body;
//! the toast sender, and Windows' own setting for this app's toasts. An
//! exited or errored session raises none.

use std::sync::Arc;

use gpui::Context;
use protocol::{AttentionReason, DaemonMessage, SessionSnapshot};

use crate::RootView;
use crate::sidebar::{NotifySkip, display_label};
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
pub(crate) fn body(session: Option<&SessionSnapshot>) -> String {
    session
        .map(|s| (display_label(s), s.id.as_str()))
        .filter(|(label, id)| label != id)
        .map_or_else(|| PRODUCT_NAME.to_owned(), |(label, _)| label)
}

impl RootView {
    /// Folds `msg` into the notifications: a session awaiting input raises
    /// Needs you while its toggle is on, unless the skip setting says the
    /// owner is looking at it already. `window_focused` is whether this
    /// client window has the keyboard.
    pub(crate) fn notify_on(
        &self,
        msg: &DaemonMessage,
        window_focused: bool,
        cx: &mut Context<Self>,
    ) {
        let DaemonMessage::Attention {
            session_id,
            reason: AttentionReason::AwaitingInput,
        } = msg
        else {
            return;
        };
        let settings = &self.sidebar.ui_state().notifications;
        if !settings.awaiting_input {
            return;
        }
        if skips(settings.skip_when, window_focused, || {
            self.session_on_screen(session_id)
        }) {
            return;
        }
        let session = self.sidebar.sessions().iter().find(|s| &s.id == session_id);
        let body = body(session);
        let notifier = Arc::clone(&self.notifications.notifier);
        cx.background_executor()
            .spawn(async move { notifier.notify(NEEDS_YOU_TITLE, &body) })
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
