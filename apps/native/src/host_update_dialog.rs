//! Update host: the confirm dialog the Connections row's Update host opens,
//! which lists the host's sessions, and the toasts that follow the update
//! it asks for. What a pending update is and which toast each step shows is
//! [`crate::remote_update`]'s; this draws the dialog, sends the request and
//! feeds the host's answers and the clock to it.

use std::time::Duration;

use gpui::{
    AnyElement, ClickEvent, Context, FocusHandle, FontWeight, Keystroke, Task, Window, div,
    prelude::*, px,
};
use protocol::{ClientMessage, DaemonMessage, SessionSnapshot, SessionStatus};

use crate::RootView;
use crate::buttons::{ButtonKind, ButtonSize, Layer, button, button_look};
use crate::driver_id::DriverId as _;
use crate::notice_view::modal_panel;
use crate::session_menu::{backdrop, dialog_button, dialog_look};
use crate::target::HostKey;
use crate::ui_theme::theme;
use crate::update::BreaksOffer;

/// How often the pending updates' bounds are checked.
const REMOTE_UPDATE_TICK: Duration = Duration::from_secs(1);
/// The dialog's Update button.
pub const CONFIRM_SELECTOR: &str = "host-update-confirm";
/// The dialog's Cancel button.
pub const CANCEL_SELECTOR: &str = "host-update-cancel";

/// One of the host's sessions, as the dialog lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostUpdateSession {
    pub title: String,
    /// The session's status in words.
    pub status: &'static str,
    /// The session is working, which the line marks.
    pub working: bool,
}

/// The open dialog, as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostUpdateDialogView {
    pub title: String,
    pub body: String,
    /// The host's sessions, in the sidebar's order.
    pub sessions: Vec<HostUpdateSession>,
    /// What shows in place of the list when the host runs no session.
    pub no_sessions: Option<String>,
    /// Lines that warn what the update ends, above the buttons.
    pub warnings: Vec<String>,
    /// The Update button's label.
    pub confirm: String,
    /// Whether the Update button takes a click: not while the release check
    /// runs.
    pub confirm_enabled: bool,
}

/// The warning while the release check runs.
const CHECKING_LINE: &str = "Checking what this update changes…";
/// The warning when the release check failed.
const CHECK_FAILED_LINE: &str = "Couldn't check what this update changes.";

/// What the dialog knows of what the update breaks.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BreaksCheck {
    /// The release check runs.
    Checking,
    /// The release check answered: the newest release after the host's
    /// version and what it breaks, or `None` when nothing is newer.
    Known(Option<BreaksOffer>),
    /// The check failed, or could not run; `shown` says whether a line says
    /// so (not in a build that does not check for updates).
    Failed { shown: bool },
}

/// The confirm dialog's state.
pub(crate) struct HostUpdateDialog {
    host: HostKey,
    /// The host's name as its Connections row shows it.
    name: String,
    sessions: Vec<HostUpdateSession>,
    check: BreaksCheck,
    /// The release check under way; dropped with the dialog.
    _check_task: Option<Task<()>>,
    focus: FocusHandle,
}

impl HostUpdateDialog {
    fn view(&self) -> HostUpdateDialogView {
        let name = &self.name;
        let ends = matches!(&self.check, BreaksCheck::Known(Some(offer)) if offer.breaks.sessions);
        let sessions_line = if ends {
            "Its running sessions end."
        } else {
            "Its sessions keep running."
        };
        HostUpdateDialogView {
            title: format!("Update {name} to the newest release?"),
            body: format!(
                "{name} will download and check the newest official release itself, then restart \
                 its daemon. {sessions_line}"
            ),
            sessions: self.sessions.clone(),
            no_sessions: self
                .sessions
                .is_empty()
                .then(|| format!("No sessions are running on {name}.")),
            warnings: self.warnings(),
            confirm: format!("Update {name}"),
            confirm_enabled: self.check != BreaksCheck::Checking,
        }
    }

    /// The lines above the buttons: the check running or failing, or what
    /// the newest release breaks.
    fn warnings(&self) -> Vec<String> {
        match &self.check {
            BreaksCheck::Checking => vec![CHECKING_LINE.to_owned()],
            BreaksCheck::Failed { shown: true } => vec![CHECK_FAILED_LINE.to_owned()],
            BreaksCheck::Failed { shown: false } | BreaksCheck::Known(None) => Vec::new(),
            BreaksCheck::Known(Some(offer)) => breaks_lines(&self.name, offer),
        }
    }

    /// Whether this app updates itself once the host runs the new release:
    /// the release breaks something and is newer than this app.
    fn self_update_after(&self) -> bool {
        let BreaksCheck::Known(Some(offer)) = &self.check else {
            return false;
        };
        offer.breaks.any()
            && RootView::client_version().is_some_and(|version| version < offer.newest)
    }
}

/// The warnings for `offer` on the host named `name`, sessions first.
fn breaks_lines(name: &str, offer: &BreaksOffer) -> Vec<String> {
    let new = &offer.newest;
    let mut lines = Vec::new();
    if offer.breaks.sessions {
        lines.push(format!("v{new} ends every session on {name}."));
    }
    if offer.breaks.protocol {
        lines.push(format!(
            "v{new} can't talk to this app's version, so this app updates itself right after \
             {name}."
        ));
    }
    lines
}

/// A session's status in words.
fn status_words(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Spawning => "starting",
        SessionStatus::Idle => "idle",
        SessionStatus::Working => "working",
        SessionStatus::AwaitingInput => "awaiting input",
        SessionStatus::Stopped => "stopped",
        SessionStatus::Error => "error",
    }
}

/// `session` as the dialog lists it.
fn session_line(session: &SessionSnapshot) -> HostUpdateSession {
    HostUpdateSession {
        title: session
            .user_label
            .clone()
            .unwrap_or_else(|| session.label.clone()),
        status: status_words(session.status),
        working: session.status == SessionStatus::Working,
    }
}

impl RootView {
    /// Opens the confirm dialog for `host`, named `name`, listing its
    /// sessions; the Connections row's Update host opens it.
    pub fn open_host_update_dialog(
        &mut self,
        host: HostKey,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sessions = self
            .sidebar
            .sessions_of(&host)
            .iter()
            .map(session_line)
            .collect();
        let (check, check_task) = self.start_breaks_check(&host, cx);
        let focus = cx.focus_handle();
        focus.focus(window);
        self.host_update_dialog = Some(HostUpdateDialog {
            host,
            name,
            sessions,
            check,
            _check_task: check_task,
            focus,
        });
        cx.notify();
    }

    /// Starts reading from the feed what the releases after `host`'s
    /// version break. A build that does not check for updates, or a host
    /// whose version is unknown, cannot run it.
    fn start_breaks_check(
        &self,
        host: &HostKey,
        cx: &mut Context<Self>,
    ) -> (BreaksCheck, Option<Task<()>>) {
        let updater = self.updater();
        if !updater.is_active() {
            return (BreaksCheck::Failed { shown: false }, None);
        }
        let from = self
            .conn_of(host)
            .and_then(crate::Connection::build_version)
            .and_then(|version| semver::Version::parse(version).ok());
        let Some(from) = from else {
            tracing::warn!(
                ?host,
                "update: the host's version is unknown; its release check fails"
            );
            return (BreaksCheck::Failed { shown: true }, None);
        };
        let fetch = updater.breaks_since(from);
        let task = cx.spawn(async move |this, cx| {
            let found = fetch.await;
            // Fails only when the view is gone, and the dialog with it.
            this.update(cx, |root, cx| root.host_update_checked(found, cx))
                .ok();
        });
        (BreaksCheck::Checking, Some(task))
    }

    /// The open dialog's release check answered.
    fn host_update_checked(
        &mut self,
        found: anyhow::Result<Option<BreaksOffer>>,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = &mut self.host_update_dialog else {
            return;
        };
        dialog.check = match found {
            Ok(offer) => BreaksCheck::Known(offer),
            Err(err) => {
                tracing::warn!("update: the release check failed: {err:#}");
                BreaksCheck::Failed { shown: true }
            }
        };
        cx.notify();
    }

    /// The open confirm dialog, as text.
    #[must_use]
    pub fn host_update_dialog(&self) -> Option<HostUpdateDialogView> {
        self.host_update_dialog.as_ref().map(HostUpdateDialog::view)
    }

    /// Update: asks the dialog's host to update, shows the Asking toast and
    /// starts the clock on its answer; then the dialog closes.
    fn confirm_host_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .host_update_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.check == BreaksCheck::Checking)
        {
            return;
        }
        let Some(dialog) = self.host_update_dialog.take() else {
            return;
        };
        let request_id = crate::new_request_id();
        let self_update_after = dialog.self_update_after();
        tracing::info!(
            host = ?dialog.host,
            %request_id,
            self_update_after,
            "update: asking a host to update"
        );
        self.send_to(
            &dialog.host,
            ClientMessage::RemoteUpdate {
                request_id: request_id.clone(),
            },
        );
        let spec = self.remote_updates.request(
            &dialog.host,
            &dialog.name,
            request_id,
            self_update_after,
            (self.now)(),
        );
        self.push_toast_spec(spec, cx);
        // A new loop for each request; dropping the old task stops it.
        self.remote_update_timer = Some(Self::tick_remote_updates(cx));
        self.after_notice_closed(window, cx);
    }

    /// Cancel, Esc or the host's connection closing: the dialog closes and
    /// nothing is sent.
    fn close_host_update_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.host_update_dialog.take().is_some() {
            self.after_notice_closed(window, cx);
            cx.notify();
        }
    }

    /// A key while the dialog is open; returns whether it was. Esc cancels;
    /// every other key, Enter included, does nothing.
    pub(crate) fn on_host_update_dialog_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.host_update_dialog.is_none() {
            return false;
        }
        if keystroke.key == "escape" {
            self.close_host_update_dialog(window, cx);
        }
        true
    }

    /// `host`'s connection is not open: a started update waits for it to
    /// come back, and a dialog over it closes.
    pub(crate) fn remote_update_dropped(
        &mut self,
        host: &HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remote_updates.on_drop(host, (self.now)());
        if self
            .host_update_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.host == *host)
        {
            self.close_host_update_dialog(window, cx);
        }
    }

    /// The answers to an update this client asked of `host`: the update
    /// starting, the host's `Welcome` once it is back, or its refusal of a
    /// protocol this app no longer shares with it, and how the update ended.
    /// After a flagged update whose host runs the new release, this app's
    /// own update counts down.
    pub(crate) fn on_remote_update_message(
        &mut self,
        host: &HostKey,
        msg: &DaemonMessage,
        cx: &mut Context<Self>,
    ) {
        let spec = match msg {
            DaemonMessage::RemoteUpdateStarted {
                request_id,
                version,
            } => self
                .remote_updates
                .on_started(host, request_id, version, (self.now)()),
            DaemonMessage::Welcome { build_version, .. } => self
                .remote_updates
                .on_welcome(host, build_version.as_deref()),
            DaemonMessage::HostUpdateResult { ok, message, .. } => {
                self.remote_updates.on_result(host, *ok, message)
            }
            DaemonMessage::AuthFailed { reason } => {
                self.remote_updates.on_auth_failed(host, reason)
            }
            _ => None,
        };
        if let Some(spec) = spec {
            self.push_toast_spec(spec, cx);
        }
        if let Some((host, id)) = self.remote_updates.take_self_update() {
            tracing::info!(?host, request_id = %id, "update: the host runs the new release; this app updates next");
            self.client_update_scheduled(&host, &id, cx);
        }
    }

    /// A refusal or cancel of `request_id` from `host`: when it answers this
    /// client's pending update, its toast shows the host's `message`, and it
    /// returns `true`, since nothing else is owed.
    pub(crate) fn on_remote_update_refused(
        &mut self,
        host: &HostKey,
        request_id: &str,
        message: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(spec) = self.remote_updates.on_refused(host, request_id, message) else {
            return false;
        };
        tracing::info!(?host, request_id, "update: the host did not update");
        self.push_toast_spec(spec, cx);
        true
    }

    /// Checks the pending updates' bounds every second while any is
    /// pending; dropping the task stops it.
    fn tick_remote_updates(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REMOTE_UPDATE_TICK).await;
                // Fails only when the view is gone, and the updates with it.
                let pending = this
                    .update(cx, Self::remote_updates_ticked)
                    .unwrap_or(false);
                if !pending {
                    break;
                }
            }
        })
    }

    /// A second went by: each update past its bound ends with its toast.
    /// Returns whether any update is still pending.
    fn remote_updates_ticked(&mut self, cx: &mut Context<Self>) -> bool {
        for spec in self.remote_updates.expire((self.now)()) {
            tracing::info!(title = %spec.title, "update: a host did not come back in time");
            self.push_toast_spec(spec, cx);
        }
        self.remote_updates.any_pending()
    }

    /// The dialog over a backdrop that takes every click beneath it.
    pub(crate) fn host_update_dialog_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.host_update_dialog.as_ref()?;
        let view = dialog.view();
        let t = theme(cx);
        let offset = crate::fonts::ui_text_size(cx);
        let shift = crate::fonts::ui_weight_shift(cx);
        let list = session_list(&view, cx);
        let warnings = view
            .warnings
            .iter()
            .map(|line| div().text_color(gpui::rgb(t.danger)).child(line.clone()));
        let confirm = button(
            CONFIRM_SELECTOR,
            button_look(
                offset,
                shift,
                ButtonKind::Primary,
                ButtonSize::Regular,
                view.confirm_enabled,
                Layer::Floating,
                &t,
            ),
        )
        .child(view.confirm.clone())
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.confirm_host_update(window, cx);
        }));
        let cancel = dialog_button(
            CANCEL_SELECTOR,
            "Cancel".to_owned(),
            dialog_look(offset, shift, false, true, &t),
            false,
        )
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.close_host_update_dialog(window, cx);
        }));
        let panel = modal_panel(offset, "host-update-dialog-panel", &t)
            .track_focus(&dialog.focus)
            .child(
                div()
                    .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                    .child(view.title.clone()),
            )
            .child(
                div()
                    .text_color(gpui::rgb(t.muted))
                    .child(view.body.clone()),
            )
            .child(list)
            .children(warnings)
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(6.0))
                    .child(cancel)
                    .child(confirm),
            );
        Some(backdrop("host-update-dialog", panel, &t))
    }
}

/// The host's sessions, one line each with its status, a working one
/// marked; or the line that says it runs none.
fn session_list(view: &HostUpdateDialogView, cx: &mut Context<RootView>) -> gpui::Div {
    let t = theme(cx);
    let list = div().flex().flex_col().gap(px(2.0));
    if let Some(none) = &view.no_sessions {
        return list.text_color(gpui::rgb(t.muted)).child(none.clone());
    }
    list.children(view.sessions.iter().enumerate().map(|(index, session)| {
        let selector = format!("host-update-session-{index}");
        div()
            .driver_id(move || selector)
            .flex()
            .gap(px(6.0))
            .child(
                div()
                    .w(px(10.0))
                    .text_color(gpui::rgb(t.accent))
                    .child(if session.working { "●" } else { "" }),
            )
            .child(div().flex_1().min_w_0().child(session.title.clone()))
            .child(
                div()
                    .text_color(gpui::rgb(if session.working { t.accent } else { t.muted }))
                    .child(session.status),
            )
    }))
}
