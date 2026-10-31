//! The device approval on screen: the Remote access page's Waiting devices
//! rows, the Remote access tab's badge, and the approval dialog with its QR
//! code, its keys and the one-second tick that keeps its countdowns moving.
//! The model is [`crate::pairing`].

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{
    AnyElement, Bounds, ClickEvent, Context, Div, FocusHandle, FontWeight, Keystroke, Pixels,
    SharedString, Stateful, Task, Window, canvas, div, point, prelude::*, px,
};
use protocol::{ClientMessage, DaemonMessage, FEATURE_DEVICE_PAIRING, PendingDevice, Secret};

use crate::RootView;
use crate::activity_bar::badge_text;
use crate::buttons::{ButtonKind, ButtonSize, Layer, button, button_look, focus_ring};
use crate::fonts::DEFAULT_FAMILY;
use crate::notice_view::modal_panel;
use crate::notify::PairingActionSink;
use crate::pairing::{
    Arrival, DialogPhase, DialogView, PairingAction, PairingState, PairingToast, QrMatrix,
    ToastActionQueue, WaitingRow, approve_message, deny_message,
};
use crate::session_menu::backdrop;
use crate::settings_view::SettingsTab;
use crate::ui_theme::{UiTheme, theme};

/// What the Confirm phase says approving does.
const CONFIRM_SENTENCE: &str = "Approving shows a one-time code for this device.";
/// The line between the QR code and the code it carries.
const OR_TYPE: &str = "or type on the phone";
/// An approved request's state on its row.
const APPROVED_STATUS: &str = "Approved · waiting for the device";
/// One QR module's side, in whole pixels so its edges stay sharp.
const QR_CELL: f32 = 6.0;
/// The light margin a scanner needs around a QR code, in modules.
const QR_QUIET_ZONE: usize = 4;
/// A QR code's light modules: always white, whatever the theme.
const QR_LIGHT: u32 = 0x00ff_ffff;
/// A QR code's dark modules: always black, whatever the theme.
const QR_DARK: u32 = 0x0000_0000;
/// The code's text size.
const CODE_SIZE: f32 = 24.0;
/// The Remote access tab's badge: its height and least width.
const BADGE_SIZE: f32 = 16.0;
/// The badge's text size.
const BADGE_TEXT_SIZE: f32 = 10.0;
/// How often the countdowns redraw.
const TICK: Duration = Duration::from_secs(1);

/// The pairing model with what only the screen needs: the dialog's keyboard
/// focus, its focused button and the redraw tick.
pub(crate) struct PairingView {
    model: PairingState,
    focus: FocusHandle,
    /// The dialog button Tab moved to, by label; none until Tab is pressed.
    button: Option<&'static str>,
    /// Whether the dialog showed at the last look, so its end is noticed.
    shown: bool,
    /// Whether Approve was pressed and the dialog waits for its code.
    approving: bool,
    /// The countdowns as last drawn, so a tick redraws only on a change.
    drawn: Vec<String>,
    tick: Option<Task<()>>,
    /// Where a pressed pairing toast hands its action, from any thread.
    actions: UnboundedSender<PairingAction>,
    /// Toast presses waiting for the daemon's first waiting list.
    toast_actions: ToastActionQueue,
}

impl PairingView {
    pub(crate) fn new(focus: FocusHandle, actions: UnboundedSender<PairingAction>) -> Self {
        Self {
            model: PairingState::new(),
            focus,
            button: None,
            shown: false,
            approving: false,
            drawn: Vec::new(),
            tick: None,
            actions,
            toast_actions: ToastActionQueue::default(),
        }
    }
}

/// A button of the approval dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairingButton {
    Approve,
    Deny,
    Close,
}

impl PairingButton {
    /// The button a label of [`DialogView::buttons`] names.
    fn of(label: &str) -> Option<Self> {
        match label {
            "Approve" => Some(Self::Approve),
            "Deny" => Some(Self::Deny),
            "Close" => Some(Self::Close),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Approve => "Approve",
            Self::Deny => "Deny",
            Self::Close => "Close",
        }
    }

    fn selector(self) -> &'static str {
        match self {
            Self::Approve => "pairing-approve",
            Self::Deny => "pairing-deny",
            Self::Close => "pairing-close",
        }
    }

    fn kind(self) -> ButtonKind {
        match self {
            Self::Approve => ButtonKind::Primary,
            Self::Deny => ButtonKind::Danger,
            Self::Close => ButtonKind::Outlined,
        }
    }
}

impl RootView {
    /// The channel a pressed pairing toast's action travels on, and the
    /// task that hands each action to [`Self::on_pairing_action`] until the
    /// view is gone.
    pub(crate) fn forward_pairing_actions(
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> UnboundedSender<PairingAction> {
        let (actions, mut pressed) = unbounded();
        cx.spawn_in(window, async move |this, cx| {
            while let Some(action) = pressed.next().await {
                if this
                    .update_in(cx, |view, window, cx| {
                        view.on_pairing_action(action, window, cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        actions
    }

    /// A pairing toast was pressed: applied now, or once the daemon's first
    /// waiting list arrives when it has not yet.
    fn on_pairing_action(
        &mut self,
        action: PairingAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::info!(?action, "pairing: a toast was pressed");
        match self.pairing.toast_actions.arrive(action) {
            Arrival::Apply(action) => self.apply_pairing_action(action, window, cx),
            Arrival::Held => tracing::info!("pairing: the toast press waits for the waiting list"),
            Arrival::Dropped(action) => log_dropped_presses(&[action]),
        }
    }

    /// Applies a toast press. Approve… brings the window forward and,
    /// unless a dialog is already open, opens the request's dialog as the
    /// row's Approve… does; Deny declines the request; a press on the body
    /// only brings the window forward. A press for a request the client no
    /// longer lists is dropped.
    fn apply_pairing_action(
        &mut self,
        action: PairingAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.pairing.model.applies(&action, Utc::now()) {
            tracing::info!(
                ?action,
                "pairing: a toast press for a request no longer listed was dropped"
            );
            return;
        }
        match action {
            PairingAction::Approve { request_id } => {
                window.activate_window();
                // An open dialog (a code on screen, or an approval on its
                // way) is never replaced from a toast.
                let waiting = self
                    .pairing_row(&request_id)
                    .is_some_and(|row| !row.approved);
                if waiting && self.pairing_dialog().is_none() {
                    self.open_pairing_dialog(&request_id, window, cx);
                }
            }
            PairingAction::Deny { request_id } => {
                if self.pairing_row(&request_id).is_some() {
                    self.deny_pairing(&request_id, window, cx);
                }
            }
            PairingAction::Open => window.activate_window(),
        }
    }

    /// The waiting row of `request_id`, while the client lists it.
    fn pairing_row(&self, request_id: &str) -> Option<WaitingRow> {
        self.pairing
            .model
            .rows(Utc::now())
            .into_iter()
            .find(|row| row.request_id == request_id)
    }

    /// Asks this PC's daemon for its waiting list, when it pairs devices;
    /// a tailnet host's list is not this client's to approve.
    /// A toast press held for a list that cannot come is dropped.
    pub(crate) fn ask_pending_devices(&mut self) {
        let local = self.conn.target().is_local();
        let can_pair = self.conn.supports(FEATURE_DEVICE_PAIRING);
        log_dropped_presses(&self.pairing.toast_actions.connected(local, can_pair));
        if local && can_pair {
            self.send(ClientMessage::ListPendingDevices);
        }
    }

    /// Folds in the daemon's waiting list or a code it minted; any other
    /// message is not the pairing model's. A waiting list is folded only
    /// from this PC's daemon: one an SSH host's daemon pushes is that host's
    /// to approve.
    pub(crate) fn on_pairing_message(
        &mut self,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match msg {
            DaemonMessage::PendingDevices { requests } if !self.conn.target().is_local() => {
                tracing::debug!(
                    requests = requests.len(),
                    "pairing: a remote host's waiting list ignored"
                );
            }
            DaemonMessage::PendingDevices { requests } => {
                let fresh = self.fold_pending_devices(requests, window, cx);
                self.raise_pairing_toasts(requests, &fresh, cx);
                for action in self.pairing.toast_actions.list_arrived() {
                    self.apply_pairing_action(action, window, cx);
                }
            }
            DaemonMessage::DevicePairCode {
                request_id,
                code,
                expires_in_secs,
            } => {
                self.fold_device_code(request_id.as_deref(), code, *expires_in_secs, window, cx);
            }
            _ => {}
        }
    }

    /// Raises the toast of each request in `fresh`, off the UI thread, while
    /// the connection is to this PC's daemon.
    fn raise_pairing_toasts(
        &self,
        requests: &[PendingDevice],
        fresh: &[String],
        cx: &mut Context<Self>,
    ) {
        if !self.conn.target().is_local() {
            return;
        }
        for request in requests.iter().filter(|r| fresh.contains(&r.request_id)) {
            let toast = PairingToast::for_request(request);
            let notifier = self.notifications.notifier.clone();
            let sink = self.pairing_sink();
            cx.background_executor()
                .spawn(async move { notifier.notify_pairing(&toast, sink) })
                .detach();
        }
    }

    /// Hands a pressed toast's action to the window, from whichever thread
    /// the OS raises it on.
    pub(crate) fn pairing_sink(&self) -> PairingActionSink {
        let actions = self.pairing.actions.clone();
        Arc::new(move |action: PairingAction| {
            if let Err(err) = actions.unbounded_send(action) {
                tracing::warn!("a pairing toast was pressed after its window closed: {err}");
            }
        })
    }

    /// Folds in the daemon's waiting list: rows come and go, and a dialog
    /// whose request left the list closes. Returns the requests that raise
    /// a toast, each once.
    pub(crate) fn fold_pending_devices(
        &mut self,
        requests: &[PendingDevice],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<String> {
        let fresh = self.pairing.model.on_pending(requests, Utc::now());
        tracing::info!(
            requests = requests.len(),
            fresh = fresh.len(),
            "pairing: waiting devices"
        );
        self.sync_pairing(window, cx);
        fresh
    }

    /// Folds in a code the daemon minted, `expires_in_secs` from now: the
    /// dialog of `request_id` opens on it. A CLI-minted code names no
    /// request and shows nothing.
    pub(crate) fn fold_device_code(
        &mut self,
        request_id: Option<&str>,
        code: &Secret,
        expires_in_secs: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shown = self
            .pairing
            .model
            .on_code(request_id, code, expires_in_secs, (self.now)());
        if shown {
            self.pairing.button = None;
            self.pairing.approving = false;
            if self.pairing_dialog().is_some() && !self.device_block_confirm_open() {
                self.pairing.focus.focus(window);
            }
        }
        self.sync_pairing(window, cx);
    }

    /// Hands the keyboard to the approval dialog, if it shows; returns
    /// whether it does.
    pub(crate) fn focus_pairing_dialog(&self, window: &mut Window) -> bool {
        let open = self.pairing_dialog().is_some();
        if open {
            self.pairing.focus.focus(window);
        }
        open
    }

    /// The Waiting devices rows as drawn: each line and its countdown, or
    /// an approved request's state.
    #[must_use]
    pub fn pairing_rows(&self) -> Vec<(String, String)> {
        self.pairing
            .model
            .rows(Utc::now())
            .iter()
            .map(|row| (row_line(row), row_status(row)))
            .collect()
    }

    /// The Waiting devices heading, while a request waits.
    #[must_use]
    pub fn pairing_heading(&self) -> Option<String> {
        let count = self.pairing.model.count(Utc::now());
        (count > 0).then(|| waiting_heading(count))
    }

    /// The Remote access tab's badge as drawn, when it shows.
    #[must_use]
    pub fn pairing_badge(&self) -> Option<String> {
        badge_text(self.pairing.model.count(Utc::now()))
    }

    /// Whether the approval dialog shows.
    #[must_use]
    pub fn pairing_dialog_open(&self) -> bool {
        self.pairing_dialog().is_some()
    }

    /// The approval dialog's lines, top to bottom; none while it is closed.
    #[must_use]
    pub fn pairing_dialog_texts(&self) -> Vec<String> {
        self.pairing_dialog()
            .map(|dialog| dialog_texts(&dialog, (self.now)()))
            .unwrap_or_default()
    }

    /// The approval dialog's button labels, in order.
    #[must_use]
    pub fn pairing_dialog_buttons(&self) -> Vec<&'static str> {
        self.pairing_dialog()
            .map(|dialog| dialog.buttons().to_vec())
            .unwrap_or_default()
    }

    /// The modules a side of the dialog's QR code has, while it shows one.
    #[must_use]
    pub fn pairing_qr_width(&self) -> Option<usize> {
        self.pairing_dialog()
            .and_then(|dialog| dialog.qr().map(|qr| qr.width))
    }

    /// The label of the dialog button that has the keyboard, if one has.
    #[must_use]
    pub fn pairing_focused_button(&self) -> Option<&'static str> {
        let dialog = self.pairing_dialog()?;
        self.pairing
            .button
            .filter(|label| dialog.buttons().contains(label))
    }

    /// Whether the countdowns' redraw tick is armed.
    #[must_use]
    pub fn pairing_tick_armed(&self) -> bool {
        self.pairing.tick.is_some()
    }

    /// Whether the view still holds a dialog it last drew; it lets go once
    /// it notices the dialog closed, its request left or its code lapsed.
    #[must_use]
    pub fn pairing_dialog_held(&self) -> bool {
        self.pairing.shown
    }

    /// Whether the approval dialog has the keyboard.
    #[must_use]
    pub fn pairing_has_focus(&self, window: &Window) -> bool {
        self.pairing.focus.is_focused(window)
    }

    fn pairing_dialog(&self) -> Option<DialogView> {
        self.pairing.model.dialog((self.now)())
    }

    /// Opens the approval dialog of `request_id` in its Confirm phase;
    /// sends nothing.
    pub(crate) fn open_pairing_dialog(
        &mut self,
        request_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pairing.model.open_dialog(request_id);
        self.pairing.button = None;
        self.pairing.approving = false;
        // A block confirm open above keeps the keyboard until it closes.
        if !self.device_block_confirm_open() {
            self.pairing.focus.focus(window);
        }
        self.sync_pairing(window, cx);
    }

    /// Approves `request_id` once; the dialog waits for its code with
    /// Approve disabled.
    pub(crate) fn approve_pairing(&mut self, request_id: &str, cx: &mut Context<Self>) {
        if self.pairing.approving {
            return;
        }
        self.pairing.approving = true;
        tracing::info!(request = request_id, "pairing: approving a device");
        self.send(approve_message(request_id));
        cx.notify();
    }

    /// Declines `request_id`, closing its dialog if it shows.
    pub(crate) fn deny_pairing(
        &mut self,
        request_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::info!(request = request_id, "pairing: denying a device");
        self.send(deny_message(request_id));
        if self
            .pairing_dialog()
            .is_some_and(|dialog| dialog.request_id == request_id)
        {
            self.pairing.model.close_dialog();
        }
        self.sync_pairing(window, cx);
    }

    /// Closes the approval dialog and drops its code; sends nothing.
    pub(crate) fn close_pairing_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pairing.model.close_dialog();
        self.sync_pairing(window, cx);
    }

    /// Another daemon or a lost one: every request and the dialog go.
    pub(crate) fn reset_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pairing.model.reset();
        log_dropped_presses(&self.pairing.toast_actions.reset());
        self.pairing.tick = None;
        self.sync_pairing(window, cx);
    }

    /// Notices a dialog that stopped showing (closed, its request gone or
    /// its code lapsed): the model drops it and the keyboard goes back to
    /// what it covered. Then keeps the tick going while anything counts
    /// down, and redraws.
    fn sync_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open = self.pairing_dialog().is_some();
        if !open {
            self.pairing.model.close_dialog();
            self.pairing.button = None;
        }
        let closed = self.pairing.shown && !open;
        self.pairing.shown = open;
        if !open {
            self.pairing.approving = false;
        }
        if closed && !self.focus_device_block_confirm(window) {
            self.after_notice_closed(window, cx);
        }
        self.arm_pairing_tick(window, cx);
        self.pairing.drawn = self.pairing_countdowns();
        cx.notify();
    }

    /// Starts the redraw tick when a countdown is on screen and none runs.
    pub(crate) fn arm_pairing_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pairing.tick.is_some() || !self.pairing_counting_down() {
            return;
        }
        self.pairing.tick = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                // Fails only when the view is gone, and the countdowns with it.
                let going = this
                    .update_in(cx, RootView::on_pairing_tick)
                    .unwrap_or(false);
                if !going {
                    break;
                }
            }
        }));
    }

    /// A second passed: a lapsed dialog closes, and the view redraws only
    /// when a countdown it shows reads differently. Returns whether the
    /// tick goes on.
    fn on_pairing_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.pairing.shown && self.pairing_dialog().is_none() {
            self.sync_pairing(window, cx);
        } else {
            let countdowns = self.pairing_countdowns();
            if countdowns != self.pairing.drawn {
                self.pairing.drawn = countdowns;
                cx.notify();
            }
        }
        let going = self.pairing_counting_down();
        if !going {
            self.pairing.tick = None;
        }
        going
    }

    /// Whether a countdown is on screen: the dialog, or Settings showing
    /// the Remote access page with a request not yet approved.
    fn pairing_counting_down(&self) -> bool {
        if self.pairing_dialog().is_some() {
            return true;
        }
        self.settings_open()
            && self.settings_tab == SettingsTab::RemoteAccess
            && self
                .pairing
                .model
                .rows(Utc::now())
                .iter()
                .any(|row| !row.approved)
    }

    /// What the countdowns read now: the rows' and the dialog's lines.
    fn pairing_countdowns(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .pairing_rows()
            .into_iter()
            .map(|(_, status)| status)
            .collect();
        lines.extend(self.pairing_dialog_texts());
        lines
    }

    /// A key while the dialog shows; returns whether it did. Esc closes,
    /// Tab and Shift+Tab move across the buttons, and Enter or Space press
    /// the focused one; with none focused they do nothing.
    pub(crate) fn on_pairing_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(dialog) = self.pairing_dialog() else {
            return false;
        };
        match keystroke.key.as_str() {
            "escape" => self.close_pairing_dialog(window, cx),
            "tab" => {
                let forward = !keystroke.modifiers.shift;
                self.pairing.button =
                    cycle(dialog.buttons(), self.pairing_focused_button(), forward);
                cx.notify();
            }
            "enter" | "space" => {
                if let Some(pressed) = self.pairing_focused_button().and_then(PairingButton::of) {
                    self.press_pairing_button(pressed, &dialog.request_id, window, cx);
                }
            }
            _ => {}
        }
        true
    }

    fn press_pairing_button(
        &mut self,
        pressed: PairingButton,
        request_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match pressed {
            PairingButton::Approve => self.approve_pairing(request_id, cx),
            PairingButton::Deny => self.deny_pairing(request_id, window, cx),
            PairingButton::Close => self.close_pairing_dialog(window, cx),
        }
    }

    /// The Remote access page's Waiting devices section, while a request
    /// waits.
    pub(crate) fn waiting_devices_section(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let rows = self.pairing.model.rows(Utc::now());
        if rows.is_empty() {
            return None;
        }
        let section = div()
            .debug_selector(|| "settings-waiting-devices".to_owned())
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(waiting_heading(rows.len())),
            )
            .children(rows.iter().map(|row| waiting_row(row, cx)));
        Some(section.into_any_element())
    }

    /// The Remote access tab's count of waiting requests, when there is one.
    pub(crate) fn remote_access_tab_badge(&self, t: &UiTheme) -> Option<AnyElement> {
        let text = self.pairing_badge()?;
        let badge = div()
            .debug_selector(|| "settings-tab-remote-access-badge".to_owned())
            .ml_auto()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .min_w(px(BADGE_SIZE))
            .h(px(BADGE_SIZE))
            .px(px(4.0))
            .rounded_full()
            .bg(gpui::rgb(t.asking))
            .text_color(gpui::rgb(t.on_asking))
            .text_size(px(BADGE_TEXT_SIZE))
            .font_weight(FontWeight::BOLD)
            .child(text);
        Some(badge.into_any_element())
    }

    /// The approval dialog over a backdrop that takes every click beneath
    /// it.
    pub(crate) fn pairing_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = theme(cx);
        let dialog = self.pairing_dialog()?;
        let focused = self.pairing_focused_button();
        let body = match dialog.phase {
            DialogPhase::Confirm => confirm_body(&dialog, &t),
            DialogPhase::Code { .. } => code_body(&dialog, (self.now)(), &t),
        };
        let footer = div().flex().justify_end().gap(px(8.0)).children(
            dialog
                .buttons()
                .iter()
                .filter_map(|label| PairingButton::of(label))
                .map(|pressed| {
                    let enabled = !(pressed == PairingButton::Approve && self.pairing.approving);
                    dialog_button(pressed, &dialog.request_id, focused, enabled, cx)
                }),
        );
        let panel = modal_panel("pairing-panel", &theme(cx))
            .track_focus(&self.pairing.focus)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(dialog.title()),
            )
            .child(body)
            .child(footer);
        Some(backdrop("pairing", panel, &theme(cx)))
    }
}

/// Logs each toast press dropped because this PC's daemon, whose requests
/// the pairing toasts name, is not the one connected.
fn log_dropped_presses(dropped: &[PairingAction]) {
    for action in dropped {
        tracing::info!(
            ?action,
            "pairing: a toast press was dropped: this PC's daemon is not connected"
        );
    }
}

/// The section's heading, counting the waiting requests.
fn waiting_heading(count: usize) -> String {
    format!("Waiting devices · {count}")
}

/// Who asks: the device and its Tailscale login.
fn row_line(row: &WaitingRow) -> String {
    format!("{} · {}", row.name, row.login)
}

/// How long the request has left, or that it waits for the device once
/// approved.
fn row_status(row: &WaitingRow) -> String {
    if row.approved {
        return APPROVED_STATUS.to_owned();
    }
    let secs = row.remaining_secs;
    format!("{}:{:02} left", secs / 60, secs % 60)
}

/// The dialog's lines, top to bottom, at `now`.
fn dialog_texts(dialog: &DialogView, now: std::time::Instant) -> Vec<String> {
    let mut texts = vec![dialog.title()];
    match dialog.phase {
        DialogPhase::Confirm => {
            texts.push(dialog.device_line());
            texts.push(CONFIRM_SENTENCE.to_owned());
        }
        DialogPhase::Code { .. } => {
            if dialog.qr().is_some() {
                texts.push(OR_TYPE.to_owned());
            }
            texts.extend(dialog.code_groups());
            texts.extend(dialog.timer_text(now));
        }
    }
    texts
}

/// The button after (or before) `current` among `buttons`, wrapping; the
/// first (or last) when none has the focus.
fn cycle(
    buttons: &[&'static str],
    current: Option<&'static str>,
    forward: bool,
) -> Option<&'static str> {
    let count = buttons.len();
    if count == 0 {
        return None;
    }
    let at = current.and_then(|label| buttons.iter().position(|b| *b == label));
    let next = match (at, forward) {
        (None, true) => 0,
        (None, false) => count - 1,
        (Some(i), true) => (i + 1) % count,
        (Some(i), false) => (i + count - 1) % count,
    };
    buttons.get(next).copied()
}

/// The dark modules of `matrix`, as (column, row).
fn qr_cells(matrix: &QrMatrix) -> Vec<(usize, usize)> {
    (0..matrix.width)
        .flat_map(|y| (0..matrix.width).map(move |x| (x, y)))
        .filter(|&(x, y)| matrix.dark(x, y))
        .collect()
}

/// A drawn QR code's side in pixels: `width` modules and the quiet zone.
#[expect(
    clippy::cast_precision_loss,
    reason = "a QR code is at most 177 modules a side"
)]
fn qr_side(width: usize) -> f32 {
    (width + 2 * QR_QUIET_ZONE) as f32 * QR_CELL
}

/// The QR code of `matrix`, black on white inside its quiet zone.
fn qr_element(matrix: &QrMatrix) -> AnyElement {
    let cells = qr_cells(matrix);
    div()
        .debug_selector(|| "pairing-qr".to_owned())
        .flex_none()
        .size(px(qr_side(matrix.width)))
        .child(
            canvas(
                |_, _, _| {},
                move |bounds, (), window, _| paint_qr(bounds, &cells, window),
            )
            .size_full(),
        )
        .into_any_element()
}

#[expect(
    clippy::cast_precision_loss,
    reason = "a QR code is at most 177 modules a side"
)]
fn paint_qr(bounds: Bounds<Pixels>, cells: &[(usize, usize)], window: &mut Window) {
    window.paint_quad(gpui::fill(bounds, gpui::rgb(QR_LIGHT)));
    let cell = gpui::size(px(QR_CELL), px(QR_CELL));
    for &(x, y) in cells {
        let at = point(
            px((QR_QUIET_ZONE + x) as f32 * QR_CELL),
            px((QR_QUIET_ZONE + y) as f32 * QR_CELL),
        );
        let module = Bounds::new(bounds.origin + at, cell);
        window.paint_quad(gpui::fill(module, gpui::rgb(QR_DARK)));
    }
}

/// The Confirm phase: who asks, and what approving does.
fn confirm_body(dialog: &DialogView, t: &UiTheme) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(muted(dialog.device_line(), t))
        .child(CONFIRM_SENTENCE)
        .into_any_element()
}

/// The code phase: the QR code beside the code and its countdown.
fn code_body(dialog: &DialogView, now: std::time::Instant, t: &UiTheme) -> AnyElement {
    let code = dialog.code_groups().map(|code| {
        div()
            .debug_selector(|| "pairing-code".to_owned())
            .font_family(DEFAULT_FAMILY)
            .text_size(px(CODE_SIZE))
            .font_weight(FontWeight::BOLD)
            .child(code)
    });
    let readout = div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .children(dialog.qr().map(|_| muted(OR_TYPE, t)))
        .children(code)
        .children(
            dialog
                .timer_text(now)
                .map(|timer| muted(timer, t).debug_selector(|| "pairing-timer".to_owned())),
        );
    div()
        .flex()
        .items_center()
        .gap(px(14.0))
        .children(dialog.qr().map(qr_element))
        .child(readout)
        .into_any_element()
}

/// One waiting request: who asks and its countdown or state, then
/// Approve… (which only opens the dialog) unless approved, and Deny.
fn waiting_row(row: &WaitingRow, cx: &mut Context<RootView>) -> Div {
    let t = theme(cx);
    let id = row.request_id.clone();
    let deny_id = id.clone();
    let deny = button(
        &format!("settings-waiting-deny-{id}"),
        button_look(
            ButtonKind::Danger,
            ButtonSize::Compact,
            true,
            Layer::Floating,
            &theme(cx),
        ),
    )
    .child("Deny")
    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.deny_pairing(&deny_id, window, cx);
    }));
    let approve = (!row.approved).then(|| {
        let approve_id = id.clone();
        button(
            &format!("settings-waiting-approve-{id}"),
            button_look(
                ButtonKind::Primary,
                ButtonSize::Compact,
                true,
                Layer::Floating,
                &theme(cx),
            ),
        )
        .child("Approve…")
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.open_pairing_dialog(&approve_id, window, cx);
        }))
    });
    let selector = format!("settings-waiting-row-{id}");
    div()
        .debug_selector(|| selector)
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(div().flex_1().min_w_0().child(muted(row_line(row), &t)))
        .child(muted(row_status(row), &t))
        .children(approve)
        .child(deny)
}

/// A dialog button, ringed while it has the keyboard.
fn dialog_button(
    pressed: PairingButton,
    request_id: &str,
    focused: Option<&'static str>,
    enabled: bool,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let size = ButtonSize::Regular;
    let request_id = request_id.to_owned();
    let drawn = button(
        pressed.selector(),
        button_look(pressed.kind(), size, enabled, Layer::Floating, &theme(cx)),
    )
    .when(focused == Some(pressed.label()), |button| {
        button.child(focus_ring(size.radius(), &theme(cx)))
    })
    .child(pressed.label());
    if !enabled {
        return drawn;
    }
    drawn.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.press_pairing_button(pressed, &request_id, window, cx);
    }))
}

fn muted(text: impl Into<SharedString>, t: &UiTheme) -> Div {
    div().text_color(gpui::rgb(t.muted)).child(text.into())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use crate::pairing::{WaitingRow, qr_matrix};

    use super::{cycle, qr_cells, qr_side, row_line, row_status, waiting_heading};

    fn row(remaining_secs: u64, approved: bool) -> WaitingRow {
        WaitingRow {
            request_id: "r1".to_owned(),
            name: "phone".to_owned(),
            login: "alice@example.com".to_owned(),
            approved,
            remaining_secs,
        }
    }

    #[test]
    fn waiting_row_line_names_device_login_and_countdown() {
        assert_eq!(row_line(&row(286, false)), "phone · alice@example.com");
        assert_eq!(row_status(&row(286, false)), "4:46 left");
        assert_eq!(row_status(&row(300, false)), "5:00 left");
        assert_eq!(row_status(&row(5, false)), "0:05 left");
    }

    #[test]
    fn approved_row_waits_for_the_device() {
        assert_eq!(row_line(&row(0, true)), "phone · alice@example.com");
        assert_eq!(
            row_status(&row(0, true)),
            "Approved · waiting for the device"
        );
    }

    #[test]
    fn waiting_heading_counts_requests() {
        assert_eq!(waiting_heading(1), "Waiting devices · 1");
        assert_eq!(waiting_heading(2), "Waiting devices · 2");
    }

    #[test]
    fn qr_cells_match_the_matrix() {
        let matrix = qr_matrix("735160").expect("six digits fit a QR code");
        let cells = qr_cells(&matrix);
        for y in 0..matrix.width {
            for x in 0..matrix.width {
                assert_eq!(cells.contains(&(x, y)), matrix.dark(x, y), "({x}, {y})");
            }
        }
        assert!((qr_side(matrix.width) - 174.0).abs() < f32::EPSILON);
    }

    #[test]
    fn tab_cycles_the_buttons_and_wraps() {
        let buttons = ["Approve", "Deny", "Close"];
        assert_eq!(cycle(&buttons, None, true), Some("Approve"));
        assert_eq!(cycle(&buttons, None, false), Some("Close"));
        assert_eq!(cycle(&buttons, Some("Close"), true), Some("Approve"));
        assert_eq!(cycle(&buttons, Some("Approve"), false), Some("Close"));
        assert_eq!(cycle(&[], None, true), None);
    }
}
