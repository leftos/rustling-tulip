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
}

/// The confirm dialog's state.
pub(crate) struct HostUpdateDialog {
    host: HostKey,
    /// The host's name as its Connections row shows it.
    name: String,
    sessions: Vec<HostUpdateSession>,
    /// Lines that warn what the update ends; none yet.
    warnings: Vec<String>,
    focus: FocusHandle,
}

impl HostUpdateDialog {
    fn view(&self) -> HostUpdateDialogView {
        let name = &self.name;
        HostUpdateDialogView {
            title: format!("Update {name} to the newest release?"),
            body: format!(
                "{name} will download and check the newest official release itself, then restart \
                 its daemon. Its sessions keep running."
            ),
            sessions: self.sessions.clone(),
            no_sessions: self
                .sessions
                .is_empty()
                .then(|| format!("No sessions are running on {name}.")),
            warnings: self.warnings.clone(),
            confirm: format!("Update {name}"),
        }
    }
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
        let focus = cx.focus_handle();
        focus.focus(window);
        self.host_update_dialog = Some(HostUpdateDialog {
            host,
            name,
            sessions,
            warnings: Vec::new(),
            focus,
        });
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
        let Some(dialog) = self.host_update_dialog.take() else {
            return;
        };
        let request_id = crate::new_request_id();
        tracing::info!(host = ?dialog.host, %request_id, "update: asking a host to update");
        self.send_to(
            &dialog.host,
            ClientMessage::RemoteUpdate {
                request_id: request_id.clone(),
            },
        );
        let spec =
            self.remote_updates
                .request(&dialog.host, &dialog.name, request_id, (self.now)());
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
    /// starting, the host's `Welcome` once it is back, and how the update
    /// ended.
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
            _ => None,
        };
        if let Some(spec) = spec {
            self.push_toast_spec(spec, cx);
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
                true,
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
