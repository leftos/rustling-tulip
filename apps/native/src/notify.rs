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

use chrono::Utc;
use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, DisplayId, Task, WeakEntity,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions, point, px,
    size,
};
use protocol::{
    AccountEventKind, AgentDescriptor, AlertKind, AttentionReason, DaemonMessage,
    FEATURE_TYPED_PROMPT_ANSWERS, KeyState, PendingInput, SessionMode, SessionSnapshot,
    SessionStatus, SummarizerKey, SummarizerSettings,
};

use crate::RootView;
use crate::connection::Connection;
use crate::dashboard_rows::{AskBlock, RowInput, ask_block, row_of};
use crate::needs_you::{self, Reason};
use crate::offscreen;
use crate::pairing::{PairingAction, PairingToast};
use crate::sidebar::{NotifySkip, ToastSound, ToastStyle, display_label};
use crate::sound::{self, NotifyKind, Sounder};
use crate::target::HostKey;
#[cfg(windows)]
use crate::toast_activation;
use crate::toast_stack::{
    ACCOUNTS_TOAST_KEY, AppToast, AppToastKind, MAX_TOAST_CHOICES, SessionKey, ToastStack, WIDTH,
    WorkArea, key_of, place,
};
use crate::toast_view::{CardText, ToastView, card_height};
use crate::toast_window::{PhysicalRect, ToastPlacer, to_physical};
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
    /// Shows a toast titled `title` with `body`, without Windows' own
    /// notification sound when `silent`, carrying `launch` as the argument a
    /// press on the toast hands back to the activator when there is one.
    /// Called off the UI thread; a failure is logged, never returned.
    fn notify(&self, title: &str, body: &str, silent: bool, launch: Option<&str>);

    /// Raises the pairing toast for `toast`, its buttons handing what was
    /// pressed to `on_action`. Called off the UI thread; a failure is logged,
    /// never returned. A notifier that raises no pairing toast leaves this
    /// as it is.
    fn notify_pairing(&self, _toast: &PairingToast, _on_action: PairingActionSink) {}

    /// Whether Windows lets this app's toasts show.
    fn state(&self) -> NotifyState;

    /// Whether Windows would hold back a toast raised now: this app's
    /// toasts are off, or the user is not taking notifications (Do Not
    /// Disturb, a presentation, a full-screen app). Called off the UI
    /// thread.
    fn suppressed(&self) -> bool;

    /// Whether the user is in Do Not Disturb, a presentation or a
    /// full-screen app, whatever Windows' own toast setting for this app
    /// says. Called off the UI thread.
    fn user_busy(&self) -> bool;

    /// Whether this platform shows OS toasts at all; where it does not, every
    /// toast is the app's own.
    fn has_os_toasts(&self) -> bool {
        true
    }
}

/// The notifier the client runs with: Windows toasts under the client's own
/// app id once its toast activator is registered, whose presses reach the
/// client through the activator; otherwise under the PowerShell app id,
/// where only a pairing toast's press from its popup reaches the client.
pub struct SystemNotifier;

#[cfg(windows)]
impl Notifier for SystemNotifier {
    fn notify(&self, title: &str, body: &str, silent: bool, launch: Option<&str>) {
        let audio = if silent {
            toast_activation::ToastAudio::Silent
        } else {
            toast_activation::ToastAudio::Windows
        };
        let registered = toast_activation::registered();
        let xml = toast_activation::toast_xml(title, body, launch, &[], registered, audio);
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

    fn suppressed(&self) -> bool {
        let os_off = match toasts_enabled() {
            Ok(enabled) => !enabled,
            Err(err) => {
                tracing::debug!("could not read Windows' notification setting: {err}");
                false
            }
        };
        os_off || self.user_busy()
    }

    fn user_busy(&self) -> bool {
        let busy = match user_takes_notifications() {
            Ok(takes) => !takes,
            Err(err) => {
                tracing::debug!("could not read whether the user takes notifications: {err}");
                false
            }
        };
        busy || do_not_disturb_on()
    }
}

/// Whether Windows lets notifications through now: not a presentation, a
/// full-screen app or a game, and not the first hour after signing in or
/// upgrading. Do Not Disturb is not part of this state; [`do_not_disturb_on`]
/// reads it from the quiet-hours state.
#[cfg(windows)]
fn user_takes_notifications() -> windows::core::Result<bool> {
    use windows::Win32::UI::Shell::SHQueryUserNotificationState;
    // SAFETY: a plain call that writes only its own out parameter.
    let state = unsafe { SHQueryUserNotificationState() }?;
    Ok(takes_notifications(state))
}

/// Whether `state` has Windows letting notifications through: only
/// `QUNS_ACCEPTS_NOTIFICATIONS` does, while a presentation, a full-screen app,
/// a game, a locked screen, the first hour after signing in or upgrading and
/// the rest hold them back. `QUNS_QUIET_TIME` is that first hour, not Do Not
/// Disturb, which this state does not report.
#[cfg(windows)]
pub(crate) fn takes_notifications(
    state: windows::Win32::UI::Shell::QUERY_USER_NOTIFICATION_STATE,
) -> bool {
    state == windows::Win32::UI::Shell::QUNS_ACCEPTS_NOTIFICATIONS
}

/// The shell's quiet-hours state, which Windows 11's Do Not Disturb sets: its
/// value is the active profile's level, 0 when Do Not Disturb is off.
#[cfg(windows)]
const WNF_SHEL_QUIETHOURS_ACTIVE_PROFILE_CHANGED: u64 = 0x0D83_063E_A3BF_1C75;

// The one read of that state: the `windows` crate has no binding for it.
#[cfg(windows)]
#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    /// Reads the data a WNF state holds. The state name is passed as a `u64`,
    /// which has the layout of `WNF_STATE_NAME { Data: [u32; 2] }` on
    /// little-endian x64.
    fn NtQueryWnfStateData(
        state_name: *const u64,
        type_id: *const core::ffi::c_void,
        explicit_scope: *const core::ffi::c_void,
        change_stamp: *mut u32,
        buffer: *mut core::ffi::c_void,
        buffer_size: *mut u32,
    ) -> windows::Win32::Foundation::NTSTATUS;
}

/// The level of the quiet-hours profile that is active now: 0 when Do Not
/// Disturb is off.
#[cfg(windows)]
fn quiet_hours_level() -> windows::core::Result<u32> {
    let mut level = 0u32;
    let mut size = 4u32;
    let mut change_stamp = 0u32;
    // SAFETY: the call reads only the state name, which outlives it, and
    // writes the two out parameters through pointers to live locals of the
    // size it is told the buffer has.
    let status = unsafe {
        NtQueryWnfStateData(
            &WNF_SHEL_QUIETHOURS_ACTIVE_PROFILE_CHANGED,
            core::ptr::null(),
            core::ptr::null(),
            &raw mut change_stamp,
            (&raw mut level).cast(),
            &raw mut size,
        )
    };
    windows::core::HRESULT::from_nt(status.0).ok()?;
    if size != 4 {
        return Err(windows::core::Error::new(
            windows::Win32::Foundation::E_UNEXPECTED,
            format!("quiet hours state holds {size} bytes, not 4"),
        ));
    }
    Ok(level)
}

/// Whether the quiet-hours `level` holds this app's own toasts back: 0 is the
/// Unrestricted profile, and any other level, one not seen yet included, is a
/// restricting profile.
#[cfg(windows)]
pub(crate) fn dnd_blocks(level: u32) -> bool {
    level != 0
}

/// Whether Windows 11's Do Not Disturb is on. A read that fails counts as off,
/// so the toasts show.
#[cfg(windows)]
fn do_not_disturb_on() -> bool {
    match quiet_hours_level() {
        Ok(level) => dnd_blocks(level),
        Err(err) => {
            static DND_READ_FAILED: std::sync::Once = std::sync::Once::new();
            DND_READ_FAILED.call_once(|| {
                tracing::warn!(
                    "could not read whether Do Not Disturb is on; toasts show as if it were off: \
                     {err}"
                );
            });
            false
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
    fn notify(&self, title: &str, _: &str, _: bool, _: Option<&str>) {
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

    fn suppressed(&self) -> bool {
        false
    }

    fn user_busy(&self) -> bool {
        false
    }

    #[cfg(target_os = "macos")]
    fn has_os_toasts(&self) -> bool {
        false
    }
}

/// A notifier that shows nothing, for the cloaked smoke-tier window, which
/// must never raise a real toast.
pub struct SilentNotifier;

impl Notifier for SilentNotifier {
    fn notify(&self, title: &str, _: &str, _: bool, _: Option<&str>) {
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

    fn suppressed(&self) -> bool {
        false
    }

    fn user_busy(&self) -> bool {
        false
    }
}

/// The root view's notification state.
pub(crate) struct Notifications {
    /// Shows the notifications, on background threads.
    pub(crate) notifier: Arc<dyn Notifier>,
    /// Plays the toast sounds, on background threads.
    pub(crate) sounder: Arc<dyn Sounder>,
    /// What is wrong with the Needs you sound's custom file, as last checked
    /// when the Notifications tab opened or the choice changed.
    pub(crate) sound_problem: Option<String>,
    /// What is wrong with the Finished sound's custom file, as last checked
    /// when the Notifications tab opened or the choice changed.
    pub(crate) finished_sound_problem: Option<String>,
    /// The Notifications tab's open sound dropdown list, if one is.
    pub(crate) sound_list: Option<crate::settings_view::SoundList>,
    /// Windows' toast setting as last read when the Notifications tab
    /// opened; `None` while a read is out.
    pub(crate) state: Option<NotifyState>,
    /// Each host's summarizer status as last reported; a host has none
    /// before one arrives and after every reconnect, so a newly chosen host
    /// never inherits "on".
    summaries: HashMap<HostKey, Summaries>,
    /// Finished toasts waiting out their grace or their session's alert, by
    /// host and session id.
    held: HashMap<SessionKey, Held>,
    /// Each session's latest alert, kept for a Finished toast the alert came
    /// too early to word, by host and session id.
    alerts: HashMap<SessionKey, Alert>,
    /// The generation the next hold is given.
    next_hold: u64,
    /// The app's own toasts and their windows.
    toasts: ToastHost,
}

/// The app's own toasts: touched only on the UI thread.
struct ToastHost {
    /// Puts the windows on the screen.
    placer: Arc<dyn ToastPlacer>,
    /// The toasts that are up.
    stack: ToastStack,
    /// Each toast's open window.
    windows: HashMap<SessionKey, ToastWindow>,
    /// Sleeps until the stack's next deadline; replaced after every change.
    timer: Option<Task<()>>,
}

/// One toast's open window.
struct ToastWindow {
    handle: WindowHandle<ToastView>,
    /// The window's handle, for the placer.
    address: Option<usize>,
    /// Whether an answer was out for the session when the window was last
    /// placed: its card is shorter then, so a change places it again.
    sent: bool,
}

/// Removes the toast window `handle`; one already closed is logged at debug.
fn remove_toast_window(handle: WindowHandle<ToastView>, cx: &mut App) {
    if let Err(err) = handle.update(cx, |_, window, _| window.remove_window()) {
        tracing::debug!("the toast window was already closed: {err:#}");
    }
}

/// Where the toasts go right now.
struct ToastScreen {
    area: WorkArea,
    dpi: u32,
    /// The display holding the main window.
    display: Option<DisplayId>,
}

impl Notifications {
    pub(crate) fn new(
        notifier: Arc<dyn Notifier>,
        placer: Arc<dyn ToastPlacer>,
        sounder: Arc<dyn Sounder>,
    ) -> Self {
        Self {
            notifier,
            toasts: ToastHost {
                placer,
                stack: ToastStack::default(),
                windows: HashMap::new(),
                timer: None,
            },
            sounder,
            sound_problem: None,
            finished_sound_problem: None,
            sound_list: None,
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
            // at work has an alert of its own coming. Background work holds a
            // toast back but keeps its alert for when the work ends.
            DaemonMessage::Sessions { sessions } => {
                let idle = |(owner, id): &SessionKey, finished: bool| {
                    owner != host
                        || sessions.iter().any(|session| {
                            session.id == *id
                                && session.status == SessionStatus::Idle
                                && !(finished && session.background)
                        })
                };
                self.held.retain(|key, _| idle(key, true));
                self.alerts.retain(|key, _| idle(key, false));
                false
            }
            DaemonMessage::SessionUpdated { session, .. } => {
                let key = key_of(host, &session.id);
                if session.status != SessionStatus::Idle || session.background {
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

/// A session toast's title line: `title`, then the session's repo or
/// workspace (`group`) and its `host`, each when it is named, `" · "` between.
/// An empty name counts as none.
fn toast_title(title: &str, group: Option<&str>, host: Option<&str>) -> String {
    let mut line = title.to_owned();
    for part in [group, host].into_iter().flatten() {
        if !part.is_empty() {
            line.push_str(" · ");
            line.push_str(part);
        }
    }
    line
}

/// The title of the notification a session awaiting input raises.
pub(crate) const NEEDS_YOU_TITLE: &str = "Needs you";

/// The title of the notification a session raises when its turn ends.
pub(crate) const FINISHED_TITLE: &str = "Finished";

/// The title of every account toast.
pub(crate) const ACCOUNTS_TITLE: &str = "Accounts";

/// How long after its turn ended a Finished toast waits for its session's
/// `WaitingAlert` before it falls back to the plain wording.
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

/// A Finished toast waiting out its grace and then, when a summary can word
/// it, its session's alert.
struct Held {
    /// The session's label: the fallback toast's body, and the name a worded
    /// title starts with.
    label: String,
    /// Which hold this is, so the timer of one that was dropped raises
    /// nothing once its session is held again.
    generation: u64,
    /// How long the hold lasts before the toast is raised.
    grace: Duration,
    /// Whether a summary can still word the toast once the grace ends.
    awaits_alert: bool,
    /// Whether the grace has ended, so an alert raises the toast at once.
    graced: bool,
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
    /// The host of the session it reports.
    host: &'a HostKey,
    /// The session it reports.
    session_id: &'a str,
    title: &'a str,
    body: &'a str,
    /// Whether the setting's toggle for this kind of toast is on.
    wanted: bool,
    /// Which sound it plays as it is raised, in place of Windows' own.
    kind: NotifyKind,
    /// Whether an `Attention` raised it, so it stands only while the wait
    /// that Attention announced does.
    attention: bool,
}

/// Plays `choice`'s clip with a `kind` toast, unless Windows holds that
/// toast back.
fn sound_with_toast(
    notifier: &dyn Notifier,
    sounder: &dyn Sounder,
    choice: &ToastSound,
    kind: NotifyKind,
) {
    if notifier.suppressed() {
        tracing::debug!("Windows holds notifications back: the toast sound does not play");
        return;
    }
    if let Some(clip) = sound::clip_for(choice, kind) {
        sounder.play(clip);
    }
}

/// Whether this driven run shows the app's own toast windows though its main
/// window is cloaked, read from the environment at each toast
/// ([`crate::driver_gate::driver_toasts_wanted`]).
fn driven_toasts() -> bool {
    crate::driver_gate::driver_toasts_wanted(
        |name| std::env::var(name).ok(),
        cfg!(all(feature = "driver", windows)),
    )
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

/// How the `status_trace` line words why `skip_when` skipped a toast.
const fn skip_reason(skip_when: NotifySkip) -> &'static str {
    match skip_when {
        NotifySkip::OnScreen => "the session is on screen in the focused window",
        NotifySkip::Focused => "the window is focused",
        NotifySkip::Never => "the skip setting is never",
    }
}

/// Whether the toast about `session_id` goes out: `wanted` is its toggle in
/// Settings, and `skipped` what the skip setting made of the window and the
/// screen. Logs the decision under `status_trace`.
fn toast_decided(
    session_id: &str,
    kind: NotifyKind,
    wanted: bool,
    skip_when: NotifySkip,
    skipped: bool,
) -> bool {
    if !wanted {
        tracing::debug!(
            target: "status_trace",
            session = %session_id,
            ?kind,
            reason = "its toggle is off",
            "notification skipped"
        );
        return false;
    }
    if skipped {
        tracing::debug!(
            target: "status_trace",
            session = %session_id,
            ?kind,
            reason = skip_reason(skip_when),
            "notification skipped"
        );
        return false;
    }
    tracing::debug!(
        target: "status_trace",
        session = %session_id,
        ?kind,
        "notification raised"
    );
    true
}

/// Whether `session`, as it now reads, has ended the wait `toast` stands
/// for: it left `AwaitingInput`, or waits again from another status stamp.
fn wait_is_over(toast: &AppToast, session: &SessionSnapshot) -> bool {
    toast.waiting
        && (session.status != SessionStatus::AwaitingInput
            || toast
                .ask
                .as_ref()
                .is_some_and(|ask| ask.since != session.status_since))
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
    /// Finished once its grace ends, held on for the session's alert while
    /// summaries can still come; an alert words the toast held for its
    /// session, or is kept for a toast still to come. `window_focused` is
    /// whether this client window has the keyboard.
    pub(crate) fn notify_on(
        &mut self,
        host: &HostKey,
        msg: &DaemonMessage,
        finished: bool,
        window_focused: bool,
        cx: &mut Context<Self>,
    ) {
        self.drop_stale_toasts(host, msg, cx);
        self.rebuild_toast_asks(host, msg, cx);
        if self.notifications.fold_state(host, msg) {
            return;
        }
        match msg {
            DaemonMessage::SessionUpdated { session, .. } if finished => {
                let label = body(Some(session), self.sidebar.label_agents(host));
                let awaits_alert = self.waits_for_alert(host, session);
                self.hold_finished(key_of(host, &session.id), label, awaits_alert, cx);
            }
            DaemonMessage::Attention {
                session_id,
                reason: AttentionReason::AwaitingInput,
                ..
            } => {
                let wanted = self.sidebar.ui_state().notifications.awaiting_input;
                let body = needs_you_body(
                    self.sidebar.session(session_id),
                    self.sidebar.label_agents(host),
                );
                self.raise_toast(
                    &Toast {
                        host,
                        session_id,
                        title: NEEDS_YOU_TITLE,
                        body: &body,
                        wanted,
                        kind: NotifyKind::NeedsYou,
                        attention: true,
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
            DaemonMessage::AccountEvent { event } => self.raise_account_toast(host, event, cx),
            _ => {}
        }
    }

    /// Raises the account toast `event` from `host` calls for, unless the
    /// Account switches toggle is off. No skip rule, as it names no session
    /// on screen, and no sound.
    fn raise_account_toast(
        &mut self,
        host: &HostKey,
        event: &AccountEventKind,
        cx: &mut Context<Self>,
    ) {
        let notifications = &self.sidebar.ui_state().notifications;
        let (wanted, style) = (notifications.account_switches, notifications.toast_style);
        let body =
            self.accounts
                .account_event_toast(host, event, wanted, Utc::now(), &chrono::Local);
        let Some(body) = body else {
            return;
        };
        let title = self.host_title(ACCOUNTS_TITLE, host);
        if style == ToastStyle::App {
            self.raise_app_account_toast(host, title, body, cx);
            return;
        }
        let notifier = Arc::clone(&self.notifications.notifier);
        cx.background_executor()
            .spawn(async move {
                notifier.notify(&title, &body, true, None);
            })
            .detach();
    }

    /// Raises the account toast `title` and `body` about `host` as the app's
    /// own, under the same guards as [`Self::raise_app_toast`]: it replaces
    /// the host's account toast already up.
    fn raise_app_account_toast(
        &self,
        host: &HostKey,
        title: String,
        body: String,
        cx: &mut Context<Self>,
    ) {
        if offscreen::requested() && !driven_toasts() {
            tracing::debug!("offscreen window: toast {title:?} not shown");
            return;
        }
        let key = (host.clone(), ACCOUNTS_TOAST_KEY.to_owned());
        let notifier = Arc::clone(&self.notifications.notifier);
        cx.spawn(async move |this, cx| {
            let busy = cx
                .background_executor()
                .spawn(async move { notifier.user_busy() })
                .await;
            if busy {
                tracing::debug!("the user is busy: toast {title:?} not shown");
                return;
            }
            // Fails only when the view is gone, and its toasts with it.
            this.update(cx, |this, cx| {
                let now = cx.background_executor().now();
                let toast = AppToast::new(key, AppToastKind::Account, title, body, now);
                this.show_app_toast(toast, None, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Whether a Finished toast about `session` on `host` waits for its
    /// alert: that host's summarizer can still write one, and writes about
    /// this session at all — an interactive run of a registered agent.
    fn waits_for_alert(&self, host: &HostKey, session: &SessionSnapshot) -> bool {
        self.notifications.holds(host)
            && session.mode != SessionMode::Headless
            && session.agent.is_some()
    }

    /// Whether the skip setting says the owner is looking at `session_id`
    /// already; `window_focused` as for [`Self::notify_on`].
    fn skips_session(&self, session_id: &str, window_focused: bool) -> bool {
        let skip_when = self.sidebar.ui_state().notifications.skip_when;
        // A session a pop-out shows with its active window's keyboard is on
        // screen in a focused window of this client.
        let in_popout = self
            .popout_focused_session()
            .is_some_and(|shown| shown.id == session_id);
        skips(skip_when, window_focused || in_popout, || {
            in_popout || self.session_on_screen(session_id)
        })
    }

    /// Raises `toast` unless its toggle is off or the skip setting says the
    /// owner is looking at the session already.
    fn raise_toast(&self, toast: &Toast<'_>, window_focused: bool, cx: &mut Context<Self>) {
        let skip_when = self.sidebar.ui_state().notifications.skip_when;
        let skipped = toast.wanted && self.skips_session(toast.session_id, window_focused);
        if !toast_decided(
            toast.session_id,
            toast.kind,
            toast.wanted,
            skip_when,
            skipped,
        ) {
            return;
        }
        if self.sidebar.ui_state().notifications.toast_style == ToastStyle::App
            || !self.notifications.notifier.has_os_toasts()
        {
            self.raise_app_toast(toast, cx);
            return;
        }
        let title = self.app_toast_title(toast);
        let body = toast.body.to_owned();
        // A press on it shows its session's pane, as its own card's title
        // and `Go to pane` do.
        let launch = PairingAction::go_arg(toast.host, toast.session_id);
        // The toast's sound is the client's own (none when it is off), never
        // Windows' beside it, so every app toast is raised silent.
        let kind = toast.kind;
        let choice = self.sound_choice(kind);
        let chime = (choice != ToastSound::Off).then_some(choice);
        let notifier = Arc::clone(&self.notifications.notifier);
        let sounder = Arc::clone(&self.notifications.sounder);
        cx.background_executor()
            .spawn(async move {
                if let Some(choice) = chime {
                    sound_with_toast(notifier.as_ref(), sounder.as_ref(), &choice, kind);
                }
                notifier.notify(&title, &body, true, Some(&launch));
            })
            .detach();
    }

    /// The sound `kind`'s toast plays as chosen in Settings.
    fn sound_choice(&self, kind: NotifyKind) -> ToastSound {
        let notifications = &self.sidebar.ui_state().notifications;
        match kind {
            NotifyKind::NeedsYou => notifications.needs_you_sound.clone(),
            NotifyKind::Finished => notifications.finished_sound.clone(),
        }
    }

    /// Plays the Needs you sound as chosen in Settings, unless it is off, as
    /// Settings' Play previews it; the custom file is read and checked on a
    /// background thread.
    pub(crate) fn play_needs_you_sound(&self, cx: &gpui::App) {
        self.play_sound(NotifyKind::NeedsYou, cx);
    }

    /// Plays the Finished sound as chosen in Settings, unless it is off, as
    /// Settings' Play previews it; the custom file is read and checked on a
    /// background thread.
    pub(crate) fn play_finished_sound(&self, cx: &gpui::App) {
        self.play_sound(NotifyKind::Finished, cx);
    }

    /// Plays `kind`'s chosen sound once, unless it is off.
    fn play_sound(&self, kind: NotifyKind, cx: &gpui::App) {
        let choice = self.sound_choice(kind);
        if choice == ToastSound::Off {
            return;
        }
        let sounder = Arc::clone(&self.notifications.sounder);
        cx.background_executor()
            .spawn(async move {
                if let Some(clip) = sound::clip_for(&choice, kind) {
                    sounder.play(clip);
                }
            })
            .detach();
    }

    /// Holds a Finished toast about `label` for session `key` through the
    /// grace the Notifications tab sets, and on until [`FINISHED_HOLD`] after
    /// the turn ended when `awaits_alert` says a summary can still word it. A
    /// session that leaves Idle or shows background work meanwhile drops the
    /// hold (`fold_state`). The setting is read when the hold starts, so a
    /// change affects the next hold, not one already running.
    fn hold_finished(
        &mut self,
        key: SessionKey,
        label: String,
        awaits_alert: bool,
        cx: &mut Context<Self>,
    ) {
        let grace = Duration::from_secs(u64::from(
            self.sidebar.ui_state().notifications.finished_grace_secs,
        ));
        self.notifications.next_hold += 1;
        let generation = self.notifications.next_hold;
        tracing::debug!(
            target: "status_trace",
            session = %key.1,
            awaits_alert,
            grace_ms = grace.as_millis(),
            "notification held through its grace"
        );
        self.notifications.held.insert(
            key.clone(),
            Held {
                label,
                generation,
                grace,
                awaits_alert,
                graced: false,
            },
        );
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(grace).await;
            // Fails only when the view is gone, and the hold with it.
            this.update(cx, |this, cx| {
                this.grace_ended(key, generation, cx);
            })
            .ok();
        })
        .detach();
    }

    /// The turn stayed ended through its grace: the toast is raised, worded
    /// by an alert that came meanwhile, or held on for one that can still
    /// come.
    fn grace_ended(&mut self, key: SessionKey, generation: u64, cx: &mut Context<Self>) {
        let Some(held) = self.notifications.held.get_mut(&key) else {
            return;
        };
        if held.generation != generation {
            return;
        }
        let grace = held.grace;
        if !held.awaits_alert {
            self.hold_expired(&key, generation, cx);
            return;
        }
        held.graced = true;
        if let Some(alert) = self.notifications.alerts.remove(&key) {
            let window_focused = self.main_window_focused(cx);
            self.release_hold(&key, &alert, window_focused, cx);
            return;
        }
        cx.spawn(async move |this, cx| {
            let rest = FINISHED_HOLD.saturating_sub(grace);
            cx.background_executor().timer(rest).await;
            // Fails only when the view is gone, and the hold with it.
            this.update(cx, |this, cx| {
                this.hold_expired(&key, generation, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Whether this client's main window has the keyboard.
    fn main_window_focused(&self, cx: &Context<Self>) -> bool {
        cx.active_window()
            .is_some_and(|window| window.window_id() == self.layouts.main())
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
        let (host, session_id) = (&key.0, key.1.as_str());
        let focused = self.main_window_focused(cx);
        let wanted = self.sidebar.ui_state().notifications.finished;
        self.raise_toast(
            &Toast {
                host,
                session_id,
                title: FINISHED_TITLE,
                body: &held.label,
                wanted,
                kind: NotifyKind::Finished,
                attention: false,
            },
            focused,
            cx,
        );
    }

    /// Ends the toast held for session `key` with its alert's wording, once
    /// its grace has ended; the alert is kept for that until then. An alert
    /// with no held toast behind it raises nothing.
    fn release_hold(
        &mut self,
        key: &SessionKey,
        alert: &Alert,
        window_focused: bool,
        cx: &mut Context<Self>,
    ) {
        let graced = self.notifications.held.get(key).is_some_and(|h| h.graced);
        if !graced {
            tracing::debug!(
                target: "status_trace",
                session = %key.1,
                kind = ?alert.kind,
                reason = "its grace has not ended",
                "notification skipped"
            );
            return;
        }
        let Some(held) = self.notifications.held.remove(key) else {
            return;
        };
        self.raise_alert(key, &held.label, alert, window_focused, cx);
    }

    /// Raises the Finished toast about session `key` that `alert` words, or
    /// the plain one when the alert's kind words none (a template), under the
    /// toggle and skip rule as they stand now.
    fn raise_alert(
        &self,
        key: &SessionKey,
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
        // A NeedsAnswer alert asks the owner for an answer, so it sounds with
        // Needs you's own sound; every other kind sounds as Finished.
        let kind = match alert.kind {
            Some(AlertKind::NeedsAnswer) => NotifyKind::NeedsYou,
            _ => NotifyKind::Finished,
        };
        self.raise_toast(
            &Toast {
                host: &key.0,
                session_id: &key.1,
                title: &title,
                body: &body,
                wanted,
                kind,
                attention: false,
            },
            window_focused,
            cx,
        );
    }

    /// Raises `toast` as the app's own: not while the user is busy, and
    /// never in a cloaked run unless it is a driven run that asks for its
    /// toasts. The busy check runs off the UI thread.
    fn raise_app_toast(&self, toast: &Toast<'_>, cx: &mut Context<Self>) {
        if offscreen::requested() && !driven_toasts() {
            tracing::debug!("offscreen window: toast {:?} not shown", toast.title);
            return;
        }
        let key = key_of(toast.host, toast.session_id);
        let title = self.app_toast_title(toast);
        let body = toast.body.to_owned();
        let sound = toast.kind;
        let kind = match sound {
            NotifyKind::NeedsYou => AppToastKind::NeedsYou { has_answers: false },
            NotifyKind::Finished => AppToastKind::Finished,
        };
        let notifier = Arc::clone(&self.notifications.notifier);
        let attention = toast.attention;
        let waited = attention && self.session_waits(toast.session_id);
        cx.spawn(async move |this, cx| {
            let busy = cx
                .background_executor()
                .spawn(async move { notifier.user_busy() })
                .await;
            if busy {
                tracing::debug!("the user is busy: toast {title:?} not shown");
                return;
            }
            // Fails only when the view is gone, and its toasts with it.
            this.update(cx, |this, cx| {
                if attention && this.wait_ended(&key, waited) {
                    tracing::debug!("the wait ended first: toast {title:?} not shown");
                    return;
                }
                let now = cx.background_executor().now();
                let waits = matches!(sound, NotifyKind::NeedsYou) && this.session_waits(&key.1);
                let wait = waits.then(|| (this.toast_ask(&key), this.toast_pending(&key.1)));
                let mut toast = AppToast::new(key, kind, title, body, now);
                if let Some((ask, pending)) = wait {
                    toast = toast.waiting_on(ask, pending);
                }
                this.show_app_toast(toast, Some(sound), cx);
            })
            .ok();
        })
        .detach();
    }

    /// Whether session `id` is awaiting input.
    fn session_waits(&self, id: &str) -> bool {
        self.sidebar
            .session(id)
            .is_some_and(|session| session.status == SessionStatus::AwaitingInput)
    }

    /// Whether the session `key` names ended the wait its `Attention`
    /// announced, while the toast waited on the busy check: its host's
    /// connection is not open, it is gone, or it `waited` and waits no more.
    fn wait_ended(&self, key: &SessionKey, waited: bool) -> bool {
        let open = self.conn_of(&key.0).is_some_and(Connection::is_open);
        !open
            || self
                .sidebar
                .session(&key.1)
                .is_none_or(|session| waited && session.status != SessionStatus::AwaitingInput)
    }

    /// The prompt session `id` waits on, as its snapshot reads now.
    fn toast_pending(&self, id: &str) -> Option<PendingInput> {
        self.sidebar
            .session(id)
            .and_then(|session| session.pending_input.clone())
    }

    /// The ask block of the session `key` names, read as the Dashboard reads
    /// its row. A toast offers at most [`MAX_TOAST_CHOICES`]; a prompt with
    /// more is answered from its pane. Its text box shows only when the
    /// session's host lists [`FEATURE_TYPED_PROMPT_ANSWERS`], since an older
    /// daemon refuses a typed answer.
    fn toast_ask(&self, key: &SessionKey) -> Option<AskBlock> {
        let (host, id) = (&key.0, key.1.as_str());
        let session = self.sidebar.session(id)?;
        let row = row_of(&self.sidebar, session);
        let mut ask = ask_block(&RowInput {
            row: &row,
            session,
            facts: self.dashboard_facts.facts_for(id),
            agents: self.sidebar.session_label_agents(id),
            unseen: self.sidebar.is_unseen(id),
            prompt: self.chat.held_prompt(id),
        })?;
        if ask.choices.len() > MAX_TOAST_CHOICES {
            ask.choices.clear();
        }
        let typed = self
            .conn_of(host)
            .is_some_and(|conn| conn.supports(FEATURE_TYPED_PROMPT_ANSWERS));
        if !typed {
            ask.typed = None;
        }
        Some(ask)
    }

    /// `toast`'s title line, naming the session's repo or workspace when it
    /// has one, and its host unless it is This PC.
    fn app_toast_title(&self, toast: &Toast<'_>) -> String {
        let group = self.toast_group(toast.host, toast.session_id);
        let host = (*toast.host != HostKey::Local).then(|| self.host_name(toast.host));
        toast_title(toast.title, group.as_deref(), host.as_deref())
    }

    /// The repo or workspace session `session_id` on `host` works in: its
    /// workspace's name for a workspace session, else its first member's
    /// repo name; `None` for a session in neither, such as a shell outside
    /// any repo.
    fn toast_group(&self, host: &HostKey, session_id: &str) -> Option<String> {
        let session = self.sidebar.session_on(host, session_id)?;
        // The session's own host's workspace: another host may hold one of
        // the same id.
        let workspace = session.workspace_id.as_deref().and_then(|id| {
            self.sidebar
                .workspaces_of(host)
                .iter()
                .find(|workspace| workspace.id == id)
        });
        workspace
            .map(|workspace| workspace.name.clone())
            .filter(|name| !name.is_empty())
            .or_else(|| {
                session
                    .members
                    .first()
                    .map(|member| member.repo_name.clone())
            })
            .filter(|name| !name.is_empty())
    }

    /// `title`, naming `host` unless it is This PC.
    fn host_title(&self, title: &str, host: &HostKey) -> String {
        let name = (*host != HostKey::Local).then(|| self.host_name(host));
        toast_title(title, None, name.as_deref())
    }

    /// Puts `toast` on the stack, in a new window or in place of the
    /// session's toast, and plays its `sound`, when it has one, if it is
    /// still up once placed; with no work area to place it on, there is no
    /// toast.
    fn show_app_toast(
        &mut self,
        toast: AppToast,
        sound: Option<NotifyKind>,
        cx: &mut Context<Self>,
    ) {
        let Some(screen) = self.toast_screen(cx) else {
            return;
        };
        let now = cx.background_executor().now();
        let key = toast.key.clone();
        let toasts = &mut self.notifications.toasts;
        let dropped = toasts.stack.push_or_update(toast, now);
        for key in dropped {
            self.close_toast_window(&key, cx);
        }
        self.place_app_toasts(&screen, cx);
        self.refresh_app_toasts(cx);
        let stack = &self.notifications.toasts.stack;
        if let Some(sound) = sound
            && stack.items().iter().any(|item| item.key == key)
        {
            self.play_sound(sound, cx);
        }
    }

    /// The main window, while it is open.
    fn main_window(&self, cx: &App) -> Option<AnyWindowHandle> {
        let main = self.layouts.main();
        cx.windows()
            .into_iter()
            .find(|window| window.window_id() == main)
    }

    /// Where the toasts go now: the work area of the monitor holding the
    /// main window. `None`, logged at warn, when it cannot be read.
    fn toast_screen(&self, cx: &mut Context<Self>) -> Option<ToastScreen> {
        let placer = Arc::clone(&self.notifications.toasts.placer);
        let main = self.main_window(cx).and_then(|main| {
            main.update(cx, |_, window, cx| {
                let display = window.display(cx).map(|display| display.id());
                (placer.address_of(window), display)
            })
            .inspect_err(|err| tracing::debug!("reading the main window for a toast: {err:#}"))
            .ok()
        });
        let (near, display) = main.unwrap_or_default();
        let Some((area, dpi)) = placer.work_area(near) else {
            tracing::warn!("no work area to place a toast on");
            return None;
        };
        Some(ToastScreen { area, dpi, display })
    }

    /// Places every toast on `screen`, opening the windows of new ones and
    /// moving the rest; a toast that does not fit is dismissed. Rearms the
    /// timer, as this ends every change to the stack.
    fn place_app_toasts(&mut self, screen: &ToastScreen, cx: &mut Context<Self>) {
        let items = self.notifications.toasts.stack.items().to_vec();
        // A toast taller than the work area's room cannot be shown: its card
        // is cut there.
        let room = crate::toast_stack::room(screen.area);
        let sent: Vec<bool> = items
            .iter()
            .map(|toast| self.toast_answer_out(&toast.key))
            .collect();
        let measure = CardText::new(cx);
        let heights: Vec<(AppToastKind, f32)> = items
            .iter()
            .zip(&sent)
            .map(|(toast, sent)| (toast.kind, card_height(toast, *sent, &measure, room)))
            .collect();
        let placed = place(screen.area, &heights);
        for (index, toast) in items.iter().enumerate() {
            if !placed.iter().any(|(kept, _)| *kept == index) {
                let now = cx.background_executor().now();
                self.notifications.toasts.stack.dismiss(&toast.key, now);
                self.close_toast_window(&toast.key, cx);
            }
        }
        for (index, (left, top)) in placed {
            let toast = &items[index];
            let height = heights[index].1;
            let rect = to_physical(left, top, WIDTH, height, screen.dpi);
            let toasts = &mut self.notifications.toasts;
            if let Some(open) = toasts.windows.get_mut(&toast.key) {
                open.sent = sent[index];
                toasts.placer.restack(open.address, rect);
            } else {
                let bounds = Bounds::new(point(px(left), px(top)), size(px(WIDTH), px(height)));
                self.open_toast_window(toast, sent[index], bounds, rect, screen.display, cx);
            }
        }
        self.arm_toast_timer(cx);
    }

    /// Opens `toast`'s window at `bounds` on `display`, its height counted
    /// with an answer out (`sent`) or not, and hands it to the
    /// placer to show on `rect`: the window opens unshown, and the placer
    /// orders it in once it has made the panel stay up (macOS) or shown it
    /// without activation (Windows). A window that fails to open, or that the
    /// placer cannot show, dismisses its toast rather than holding a slot
    /// invisibly.
    fn open_toast_window(
        &mut self,
        toast: &AppToast,
        sent: bool,
        bounds: Bounds<gpui::Pixels>,
        rect: PhysicalRect,
        display: Option<DisplayId>,
        cx: &mut Context<Self>,
    ) {
        let placer = Arc::clone(&self.notifications.toasts.placer);
        let root = cx.weak_entity();
        let shown = toast.clone();
        let mut address = None;
        let mut placed = false;
        let opened = cx.open_window(toast_window_options(bounds, display), |window, cx| {
            address = placer.address_of(window);
            placed = placer.show(address, rect);
            cx.new(|_| ToastView::new(root, shown))
        });
        match opened {
            Ok(handle) => {
                let open = ToastWindow {
                    handle,
                    address,
                    sent,
                };
                self.notifications
                    .toasts
                    .windows
                    .insert(toast.key.clone(), open);
                if !placed {
                    tracing::warn!(
                        "the placer could not show a toast window; dismissing its toast"
                    );
                    let now = cx.background_executor().now();
                    self.notifications.toasts.stack.dismiss(&toast.key, now);
                    self.close_toast_window(&toast.key, cx);
                }
            }
            Err(err) => {
                tracing::warn!("opening a toast window: {err:#}");
                let now = cx.background_executor().now();
                self.notifications.toasts.stack.dismiss(&toast.key, now);
            }
        }
    }

    /// The Win32 handle of `key`'s toast window as the placer gave it;
    /// `None` without a window or a handle (a recording placer gives none),
    /// so a toast's text box takes the foreground only on a real window.
    #[cfg(windows)]
    pub(crate) fn toast_window_address(&self, key: &SessionKey) -> Option<usize> {
        self.notifications
            .toasts
            .windows
            .get(key)
            .and_then(|open| open.address)
    }

    /// Closes `key`'s window, if it has one. The close waits for the effects
    /// to flush, as a press on a toast runs inside its window's update. A
    /// toast that holds the keys gives them back first, while it is still
    /// up, on a later turn of the loop, and closes after.
    fn close_toast_window(&mut self, key: &SessionKey, cx: &mut Context<Self>) {
        let Some(open) = self.notifications.toasts.windows.remove(key) else {
            return;
        };
        cx.defer(move |cx| {
            let keys = open
                .handle
                .update(cx, |view, _, _| view.release_keys())
                .ok()
                .flatten();
            let Some((address, before)) = keys else {
                remove_toast_window(open.handle, cx);
                return;
            };
            cx.spawn(async move |cx| {
                #[cfg(windows)]
                crate::toast_window::give_back_foreground(address, before, false);
                #[cfg(not(windows))]
                let _ = (address, before);
                if let Err(err) = cx.update(|cx| remove_toast_window(open.handle, cx)) {
                    tracing::debug!("the app quit before the toast window closed: {err:#}");
                }
            })
            .detach();
        });
    }

    /// Sleeps until the stack's next deadline, in place of the last sleep;
    /// a held or empty stack has none.
    fn arm_toast_timer(&mut self, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        let deadline = self.notifications.toasts.stack.next_deadline(now);
        self.notifications.toasts.timer = deadline.map(|deadline| {
            let wait = deadline.saturating_duration_since(now);
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(wait).await;
                // Fails only when the view is gone, and its toasts with it.
                this.update(cx, Self::expire_app_toasts).ok();
            })
        });
    }

    /// Closes the toasts whose time is up.
    fn expire_app_toasts(&mut self, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        let gone = self.notifications.toasts.stack.expire(now);
        for key in gone {
            self.close_toast_window(&key, cx);
        }
        Self::after_toast_change(cx);
    }

    /// Moves the windows to fit the stack; with no work area they stay
    /// where they are. Waits for the effects to flush, as a change made
    /// while the main window updates cannot read which monitor holds it.
    fn after_toast_change(cx: &mut Context<Self>) {
        let root = cx.weak_entity();
        cx.defer(move |cx| {
            // Fails only when the view is gone, and its toasts with it.
            root.update(cx, |this, cx| match this.toast_screen(cx) {
                Some(screen) => this.place_app_toasts(&screen, cx),
                None => this.arm_toast_timer(cx),
            })
            .ok();
        });
    }

    /// Takes `key`'s toast down.
    pub(crate) fn dismiss_app_toast(&mut self, key: &SessionKey, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        self.notifications.toasts.stack.dismiss(key, now);
        self.close_toast_window(key, cx);
        Self::after_toast_change(cx);
    }

    /// Takes down every toast of `host`, whose connection ended or began
    /// anew.
    pub(crate) fn drop_host_toasts(&mut self, host: &HostKey, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        let gone = self.notifications.toasts.stack.drop_host(host, now);
        if gone.is_empty() {
            return;
        }
        for key in gone {
            self.close_toast_window(&key, cx);
        }
        Self::after_toast_change(cx);
    }

    /// Takes down the toasts `msg` from `host` leaves with nothing to say.
    fn drop_stale_toasts(&mut self, host: &HostKey, msg: &DaemonMessage, cx: &mut Context<Self>) {
        if matches!(msg, DaemonMessage::Welcome { .. }) {
            self.drop_host_toasts(host, cx);
            return;
        }
        let of_host = || {
            let items = self.notifications.toasts.stack.items().iter();
            items.filter(|toast| toast.key.0 == *host)
        };
        // An account toast's key names no session, so no session message
        // takes it down.
        let of_session = |toast: &&AppToast| toast.kind != AppToastKind::Account;
        let stale: Vec<SessionKey> = match msg {
            DaemonMessage::SessionRemoved { session_id } => of_host()
                .filter(of_session)
                .filter(|toast| toast.key.1 == *session_id)
                .map(|toast| toast.key.clone())
                .collect(),
            DaemonMessage::SessionUpdated { session, .. } => of_host()
                .filter(of_session)
                .filter(|toast| toast.key.1 == session.id && wait_is_over(toast, session))
                .map(|toast| toast.key.clone())
                .collect(),
            DaemonMessage::Sessions { sessions } => of_host()
                .filter(of_session)
                .filter(|toast| {
                    let listed = sessions.iter().find(|session| session.id == toast.key.1);
                    listed.is_none_or(|session| wait_is_over(toast, session))
                })
                .map(|toast| toast.key.clone())
                .collect(),
            _ => Vec::new(),
        };
        for key in stale {
            self.dismiss_app_toast(&key, cx);
        }
    }

    /// Reads again the ask of each of `host`'s toasts whose session now
    /// waits on another prompt under the same wait, as `msg` left it: an
    /// answer goes to the prompt that is pending, so the toast must show that
    /// one. No sound, and its time runs on.
    fn rebuild_toast_asks(&mut self, host: &HostKey, msg: &DaemonMessage, cx: &mut Context<Self>) {
        if !matches!(
            msg,
            DaemonMessage::SessionUpdated { .. } | DaemonMessage::Sessions { .. }
        ) {
            return;
        }
        let moved: Vec<SessionKey> = self
            .notifications
            .toasts
            .stack
            .items()
            .iter()
            .filter(|toast| toast.waiting && toast.key.0 == *host)
            .filter(|toast| {
                let session = self.sidebar.session(&toast.key.1);
                session.is_some_and(|session| session.pending_input != toast.pending)
            })
            .map(|toast| toast.key.clone())
            .collect();
        if moved.is_empty() {
            return;
        }
        let now = cx.background_executor().now();
        for key in moved {
            let (ask, pending) = (self.toast_ask(&key), self.toast_pending(&key.1));
            let stack = &mut self.notifications.toasts.stack;
            stack.update_ask(&key, ask, pending, now);
        }
        Self::after_toast_change(cx);
        self.refresh_app_toasts(cx);
    }

    /// Takes down each Needs you toast the skip rule now skips.
    /// `window_focused` as for [`Self::notify_on`].
    pub(crate) fn dismiss_skipped_toasts(&mut self, window_focused: bool, cx: &mut Context<Self>) {
        let skipped: Vec<SessionKey> = self
            .notifications
            .toasts
            .stack
            .items()
            .iter()
            .filter(|toast| matches!(toast.kind, AppToastKind::NeedsYou { .. }))
            .filter(|toast| self.skips_session(&toast.key.1, window_focused))
            .map(|toast| toast.key.clone())
            .collect();
        for key in skipped {
            self.dismiss_app_toast(&key, cx);
        }
    }

    /// Whether an answer is out for `key`'s session.
    fn toast_answer_out(&self, key: &SessionKey) -> bool {
        self.sidebar
            .session(&key.1)
            .is_some_and(|session| self.dashboard_answers.is_out(session))
    }

    /// Tells each open toast's view its toast as the stack holds it and
    /// whether an answer is out for its session, placing the toasts again
    /// when that changed for one, as its card's height follows it. Waits for
    /// the effects to flush, as a press on a toast runs inside its window's
    /// update.
    pub(crate) fn refresh_app_toasts(&self, cx: &mut Context<Self>) {
        let toasts = &self.notifications.toasts;
        let mut flipped = false;
        let open: Vec<(WindowHandle<ToastView>, AppToast, bool)> = toasts
            .stack
            .items()
            .iter()
            .filter_map(|toast| {
                let window = toasts.windows.get(&toast.key)?;
                let sent = self.toast_answer_out(&toast.key);
                flipped |= sent != window.sent;
                Some((window.handle, toast.clone(), sent))
            })
            .collect();
        if flipped {
            Self::after_toast_change(cx);
        }
        cx.defer(move |cx| {
            for (handle, toast, sent) in open {
                let told = handle.update(cx, |view, _, cx| view.show(toast, sent, cx));
                if let Err(err) = told {
                    tracing::debug!("the toast's window had closed: {err:#}");
                }
            }
        });
    }

    /// The pointer came to rest on `key`'s toast or left it.
    pub(crate) fn hover_toast(&mut self, key: &SessionKey, hovered: bool, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        let stack = &mut self.notifications.toasts.stack;
        if hovered {
            stack.hover(key, now);
        } else {
            stack.unhover(key, now);
        }
        self.arm_toast_timer(cx);
    }

    /// Go to pane: the main window comes to the front on `key`'s session,
    /// and the toast goes. Runs outside the root view's update, as the main
    /// window's update has to enter it.
    pub(crate) fn toast_go_to_pane(root: &WeakEntity<Self>, key: &SessionKey, cx: &mut App) {
        let main = root.read_with(cx, Self::main_window).ok().flatten();
        let Some(main) = main else {
            tracing::warn!("Go to pane: the main window has gone");
            return;
        };
        let (host, id) = key.clone();
        let target = root.clone();
        let shown = main.update(cx, move |_, window, cx| {
            window.activate_window();
            target.update(cx, |root, cx| {
                root.select_session_on(&host, &id, window, cx);
            })
        });
        if !matches!(shown, Ok(Ok(()))) {
            tracing::warn!("Go to pane: the session was not shown");
        }
        let dismissed = root.update(cx, |root, cx| root.dismiss_app_toast(key, cx));
        if let Err(err) = dismissed {
            tracing::debug!("toast dismissed after the root view went: {err:#}");
        }
    }

    /// Takes every toast down, as the main window closes.
    pub(crate) fn close_toasts(&mut self, cx: &mut Context<Self>) {
        let keys: Vec<SessionKey> = self
            .notifications
            .toasts
            .stack
            .items()
            .iter()
            .map(|toast| toast.key.clone())
            .collect();
        let now = cx.background_executor().now();
        for key in keys {
            self.notifications.toasts.stack.dismiss(&key, now);
            self.close_toast_window(&key, cx);
        }
        self.notifications.toasts.timer = None;
    }

    /// The toasts that are up, newest last, as (session id, title).
    #[must_use]
    pub fn shown_toasts(&self) -> Vec<(String, String)> {
        self.notifications
            .toasts
            .stack
            .items()
            .iter()
            .map(|toast| (toast.key.1.clone(), toast.title.clone()))
            .collect()
    }
}

/// The options a toast's window opens with at `bounds` on `display`: a
/// borderless panel that never takes the keyboard and opens unshown, for the
/// placer to order in once the panel stays up (`keep_panel_up` has the why).
fn toast_window_options(bounds: Bounds<gpui::Pixels>, display: Option<DisplayId>) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        kind: WindowKind::PopUp,
        focus: false,
        show: false,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        display_id: display,
        window_background: WindowBackgroundAppearance::Transparent,
        ..Default::default()
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;

    #[test]
    fn toast_title_names_the_group_and_the_host_that_are_known() {
        let cases = [
            (None, None, "Needs you", "neither"),
            (Some("web"), None, "Needs you · web", "the repo alone"),
            (None, Some("box"), "Needs you · box", "the host alone"),
            (Some("web"), Some("box"), "Needs you · web · box", "both"),
            (
                Some(""),
                Some("box"),
                "Needs you · box",
                "an empty group is none",
            ),
        ];
        for (group, host, title, why) in cases {
            assert_eq!(toast_title("Needs you", group, host), title, "{why}");
        }
    }

    #[test]
    fn a_toast_window_opens_unshown_so_the_placer_orders_it_in() {
        let bounds = Bounds::new(point(px(1544.0), px(795.0)), size(px(360.0), px(182.0)));
        let options = toast_window_options(bounds, None);
        assert!(matches!(options.kind, WindowKind::PopUp));
        assert!(!options.focus, "a toast never takes the keyboard");
        assert!(
            !options.show,
            "the placer orders the toast in once the panel stays up"
        );
    }

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

    /// A `MakeWriter` that keeps what a subscriber formats, for the
    /// `status_trace` tests.
    #[derive(Clone, Default)]
    struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("the log buffer is not poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl tracing_subscriber::fmt::MakeWriter<'_> for LogCapture {
        type Writer = Self;

        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    impl LogCapture {
        fn text(&self) -> String {
            let bytes = self.0.lock().expect("the log buffer is not poisoned");
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }

    /// What `toast_decided` logs for its arguments, through the subscriber the
    /// Detailed logging switch raises.
    fn decision_log(wanted: bool, skip_when: NotifySkip, skipped: bool) -> (bool, String) {
        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("status_trace=debug"))
            .with_ansi(false)
            .with_writer(capture.clone())
            .finish();
        let raised = tracing::subscriber::with_default(subscriber, || {
            toast_decided("s1", NotifyKind::Finished, wanted, skip_when, skipped)
        });
        (raised, capture.text())
    }

    #[test]
    fn a_skipped_notification_logs_its_reason() {
        let (raised, toggle) = decision_log(false, NotifySkip::Focused, false);
        assert!(!raised, "a toast with its toggle off is not raised");
        assert!(toggle.contains("notification skipped"), "{toggle}");
        assert!(toggle.contains("its toggle is off"), "{toggle}");
        assert!(toggle.contains("session=s1"), "{toggle}");

        let (raised, looking) = decision_log(true, NotifySkip::OnScreen, true);
        assert!(!raised, "a toast the skip rule covers is not raised");
        assert!(looking.contains("notification skipped"), "{looking}");
        assert!(
            looking.contains("the session is on screen in the focused window"),
            "{looking}"
        );
    }

    #[test]
    fn a_raised_notification_logs_it() {
        let (raised, text) = decision_log(true, NotifySkip::OnScreen, false);
        assert!(raised, "a wanted, uncovered toast is raised");
        assert!(text.contains("notification raised"), "{text}");
        assert!(text.contains("session=s1"), "{text}");
        assert!(!text.contains("notification skipped"), "{text}");
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

    #[cfg(windows)]
    #[test]
    fn only_accepts_notifications_takes_them() {
        use windows::Win32::UI::Shell::{
            QUNS_ACCEPTS_NOTIFICATIONS, QUNS_APP, QUNS_BUSY, QUNS_NOT_PRESENT,
            QUNS_PRESENTATION_MODE, QUNS_QUIET_TIME, QUNS_RUNNING_D3D_FULL_SCREEN,
        };
        for state in [
            QUNS_NOT_PRESENT,
            QUNS_BUSY,
            QUNS_RUNNING_D3D_FULL_SCREEN,
            QUNS_PRESENTATION_MODE,
            QUNS_QUIET_TIME,
            QUNS_APP,
        ] {
            assert!(
                !takes_notifications(state),
                "{state:?} holds notifications back"
            );
        }
        assert!(takes_notifications(QUNS_ACCEPTS_NOTIFICATIONS));
    }

    #[cfg(windows)]
    #[test]
    fn do_not_disturb_off_lets_toasts_through() {
        assert!(!dnd_blocks(0), "level 0 is the Unrestricted profile");
    }

    #[cfg(windows)]
    #[test]
    fn do_not_disturb_on_holds_toasts_back() {
        assert!(
            dnd_blocks(1),
            "level 1 is Priority only, Windows 11's Do Not Disturb"
        );
        assert!(dnd_blocks(2), "level 2 is Alarms only");
    }

    #[cfg(windows)]
    #[test]
    fn an_unknown_quiet_hours_level_holds_toasts_back() {
        assert!(dnd_blocks(3));
        assert!(dnd_blocks(u32::MAX));
    }
}
