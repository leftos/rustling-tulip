//! This PC's pairing with a tailnet host on screen: the dialog over a
//! backdrop, its code field, its keys, the one-second tick that keeps its
//! countdown moving, and when it opens and closes as the connection moves.
//! The model is [`crate::device_pairing`]; the network thread stores the
//! token the host issues.

use std::collections::HashSet;
use std::time::Duration;

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, FocusHandle, Focusable as _, FontWeight,
    Keystroke, Stateful, Subscription, Task, Window, div, prelude::*, px,
};
use protocol::DaemonMessage;

use crate::RootView;
use crate::buttons::{ButtonKind, ButtonSize, Layer, button, button_look, focus_ring};
use crate::connection::State;
use crate::device_pairing::{DeviceButton, DevicePairing};
use crate::net::{NetCommand, NetEvent};
use crate::notice_view::modal_panel;
use crate::session_menu::backdrop;
use crate::target::HostKey;
use crate::text_input::{TextChanged, TextInput, TextInputEvent};
use crate::ui_theme::{UiTheme, theme};

/// How often the countdown redraws.
const TICK: Duration = Duration::from_secs(1);
/// The code field's placeholder.
const CODE_PLACEHOLDER: &str = "6 digits";
/// The code field's width.
const CODE_WIDTH: f32 = 120.0;
/// The link that reveals the code field while the request waits.
const CODE_LINK_LABEL: &str = "Have a code?";
const CODE_LINK_SELECTOR: &str = "device-pairing-have-code";
/// The code link's corner radius.
const LINK_RADIUS: f32 = 4.0;

/// What of a network event the pairing dialog follows.
pub(crate) enum PairingInput {
    /// The connection changed.
    State,
    /// A pairing message from the host.
    Message(Box<DaemonMessage>),
    Paired,
    Lost,
    TokenSaveFailed,
    /// The request is on the host's socket.
    RequestSent,
}

impl PairingInput {
    /// The part of `event` the dialog follows, if any.
    pub(crate) fn of(event: &NetEvent) -> Option<Self> {
        match event {
            NetEvent::State(_) => Some(Self::State),
            NetEvent::Message(msg) => matches!(
                **msg,
                DaemonMessage::DevicePairPending { .. }
                    | DaemonMessage::DevicePairDecided { .. }
                    | DaemonMessage::DevicePairFailed { .. }
            )
            .then(|| Self::Message(msg.clone())),
            NetEvent::Paired => Some(Self::Paired),
            NetEvent::PairingLost => Some(Self::Lost),
            NetEvent::TokenSaveFailed => Some(Self::TokenSaveFailed),
            NetEvent::PairRequestSent => Some(Self::RequestSent),
            NetEvent::Handshake(_)
            | NetEvent::DaemonId(_)
            | NetEvent::ShutdownSent
            | NetEvent::ShutdownFailed => None,
        }
    }
}

/// A control of the dialog, in the order Tab walks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceControl {
    /// The "Have a code?" link.
    CodeLink,
    /// The code field.
    Field,
    Button(DeviceButton),
}

/// The open dialog: the host it pairs with, its model, its code field and
/// the control Tab moved to.
struct DevicePairingDialog {
    host: HostKey,
    model: DevicePairing,
    code_input: Entity<TextInput>,
    control: Option<DeviceControl>,
    _subscriptions: Vec<Subscription>,
}

/// The pairing dialog with what only the screen needs: its keyboard focus,
/// the hosts whose answer the owner's Pair or Reconnect waits for, and the
/// countdown's redraw tick.
pub(crate) struct DevicePairingView {
    dialog: Option<DevicePairingDialog>,
    focus: FocusHandle,
    /// The hosts the owner asked to pair with or reconnected: each one's
    /// next "not approved" opens the dialog for it.
    pair_on_not_approved: HashSet<HostKey>,
    /// The countdown as last drawn, so a tick redraws only on a change.
    drawn: Option<String>,
    tick: Option<Task<()>>,
}

impl DevicePairingView {
    pub(crate) fn new(focus: FocusHandle) -> Self {
        Self {
            dialog: None,
            focus,
            pair_on_not_approved: HashSet::new(),
            drawn: None,
            tick: None,
        }
    }
}

/// The controls Tab walks: the code link or the code field while it shows,
/// then the buttons that take a press.
fn control_ring(model: &DevicePairing) -> Vec<DeviceControl> {
    let mut ring = Vec::new();
    if model.offers_code_link() {
        ring.push(DeviceControl::CodeLink);
    }
    if model.code().is_some() {
        ring.push(DeviceControl::Field);
    }
    ring.extend(
        model
            .buttons()
            .iter()
            .filter(|pressed| button_enabled(model, **pressed))
            .map(|pressed| DeviceControl::Button(*pressed)),
    );
    ring
}

/// Whether `pressed` takes a press: Pair only with six digits typed.
fn button_enabled(model: &DevicePairing, pressed: DeviceButton) -> bool {
    pressed != DeviceButton::Pair || model.can_pair()
}

/// Where the keyboard goes as a phase starts: the code field once it shows.
fn phase_control(model: &DevicePairing) -> Option<DeviceControl> {
    model.code().is_some().then_some(DeviceControl::Field)
}

impl RootView {
    /// Whether the pairing dialog is open.
    #[must_use]
    pub fn device_pairing_open(&self) -> bool {
        self.device_pairing.dialog.is_some()
    }

    /// The dialog's title, while it is open.
    #[must_use]
    pub fn device_pairing_title(&self) -> Option<String> {
        self.device_pairing_model().map(DevicePairing::title)
    }

    /// The dialog's sentence, while it is open.
    #[must_use]
    pub fn device_pairing_line(&self) -> Option<String> {
        self.device_pairing_model().map(DevicePairing::line)
    }

    /// The dialog's countdown as it reads now, while one shows.
    #[must_use]
    pub fn device_pairing_countdown(&self) -> Option<String> {
        self.device_pairing_model()?.countdown((self.now)())
    }

    /// The host's refusal of the last code, while it shows.
    #[must_use]
    pub fn device_pairing_refusal(&self) -> Option<String> {
        self.device_pairing_model()?.refusal().map(str::to_owned)
    }

    /// The dialog's button labels, in order.
    #[must_use]
    pub fn device_pairing_buttons(&self) -> Vec<&'static str> {
        self.device_pairing_model()
            .map(|model| model.buttons().iter().map(|b| b.label()).collect())
            .unwrap_or_default()
    }

    /// The code field's text, while it shows.
    #[must_use]
    pub fn device_pairing_code(&self, cx: &App) -> Option<String> {
        let dialog = self.device_pairing.dialog.as_ref()?;
        dialog.model.code()?;
        Some(dialog.code_input.read(cx).text().to_owned())
    }

    fn device_pairing_model(&self) -> Option<&DevicePairing> {
        self.device_pairing
            .dialog
            .as_ref()
            .map(|dialog| &dialog.model)
    }

    /// The owner asked to pair with `host` or reconnected it: if it answers
    /// "not approved", the dialog opens for it by itself.
    pub(crate) fn pair_on_not_approved(&mut self, host: HostKey) {
        self.device_pairing.pair_on_not_approved.insert(host);
    }

    /// The window let `host`'s connection go: a Pair armed for it is
    /// forgotten, and its open dialog closes; sends nothing.
    pub(crate) fn forget_device_pairing(
        &mut self,
        host: &HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.device_pairing.pair_on_not_approved.remove(host);
        if self.device_pairing_is_for(host) {
            tracing::info!(
                ?host,
                "device pairing: the host's connection is gone; the dialog closes"
            );
            self.close_device_pairing(window, cx);
        }
    }

    /// Whether the open dialog pairs with `host`.
    fn device_pairing_is_for(&self, host: &HostKey) -> bool {
        self.device_pairing
            .dialog
            .as_ref()
            .is_some_and(|dialog| dialog.host == *host)
    }

    /// Pair on `host`'s Connections row: the dialog opens for it now while
    /// the host says "not approved", else on its next "not approved".
    pub(crate) fn pair_with_host(
        &mut self,
        host: &HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let not_approved = self
            .conn_of(host)
            .is_some_and(|conn| matches!(conn.state(), State::NotApproved { .. }));
        if not_approved {
            self.open_device_pairing(host, window, cx);
        } else {
            self.pair_on_not_approved(host.clone());
        }
    }

    /// Opens the dialog for `host` in its Intro phase while its connection
    /// is a pairing one, replacing one already open; sends nothing.
    pub(crate) fn open_device_pairing(
        &mut self,
        host: &HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(conn) = self.conn_of(host) else {
            tracing::warn!(
                ?host,
                "device pairing: no connection to the host; no dialog"
            );
            return;
        };
        let State::NotApproved { had_token, .. } = conn.state() else {
            return;
        };
        let had_token = *had_token;
        let Some(name) = conn.target().tailnet_host().map(|h| h.name().to_owned()) else {
            tracing::warn!("a pairing connection that is not to a tailnet host; no dialog");
            return;
        };
        tracing::info!(
            host = name.as_str(),
            had_token,
            "device pairing: the dialog opens"
        );
        self.close_flyout();
        let code_input = cx.new(|cx| TextInput::new(String::new(), CODE_PLACEHOLDER, cx));
        let subscriptions = Self::watch_device_code_field(&code_input, window, cx);
        self.device_pairing.dialog = Some(DevicePairingDialog {
            host: host.clone(),
            model: DevicePairing::new(&name, had_token),
            code_input,
            control: None,
            _subscriptions: subscriptions,
        });
        self.apply_device_pairing_focus(window, cx);
        self.sync_device_pairing(window, cx);
    }

    /// Closes the dialog and hands the keyboard back to the active pane;
    /// sends nothing.
    pub(crate) fn close_device_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.device_pairing.dialog.take().is_some() {
            self.device_pairing.tick = None;
            self.device_pairing.drawn = None;
            self.focus_active_pane(window, cx);
            cx.notify();
        }
    }

    /// Cancel, Close or Esc: a request still out is withdrawn, then the
    /// dialog closes and the connection stays not paired.
    fn cancel_device_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.device_pairing.dialog else {
            return;
        };
        if dialog.model.cancels_request() {
            tracing::info!("device pairing: withdrawing the request");
            self.command_to(&dialog.host, NetCommand::PairCancel);
        }
        self.close_device_pairing(window, cx);
    }

    /// Ask to pair, or Ask again.
    fn ask_device_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.device_pairing.dialog else {
            return;
        };
        if dialog.model.ask() {
            dialog.control = None;
            clear_code_field(dialog, cx);
            tracing::info!("device pairing: asking the host");
            let host = dialog.host.clone();
            self.command_to(&host, NetCommand::PairRequest);
            self.apply_device_pairing_focus(window, cx);
        }
        self.sync_device_pairing(window, cx);
    }

    /// Pair: the six typed digits go to the host, the field clears and
    /// takes the keyboard for the next code.
    fn submit_device_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.device_pairing.dialog else {
            return;
        };
        let Some(code) = dialog.model.take_code() else {
            return;
        };
        clear_code_field(dialog, cx);
        dialog.control = Some(DeviceControl::Field);
        tracing::info!("device pairing: sending the code");
        let host = dialog.host.clone();
        self.command_to(&host, NetCommand::PairRedeem { code });
        self.apply_device_pairing_focus(window, cx);
    }

    /// "Have a code?": the code field shows and takes the keyboard while the
    /// request keeps waiting for the host's approval.
    fn reveal_device_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.device_pairing.dialog else {
            return;
        };
        if !dialog.model.offers_code_link() {
            return;
        }
        dialog.model.reveal_code();
        dialog.control = Some(DeviceControl::Field);
        tracing::info!("device pairing: the code field shows while waiting");
        self.apply_device_pairing_focus(window, cx);
    }

    fn press_device_button(
        &mut self,
        pressed: DeviceButton,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match pressed {
            DeviceButton::Ask | DeviceButton::AskAgain => self.ask_device_pairing(window, cx),
            DeviceButton::Pair => self.submit_device_code(window, cx),
            DeviceButton::Cancel | DeviceButton::Close => self.cancel_device_pairing(window, cx),
        }
    }

    /// Enter or Space on a control that is not the code field.
    fn press_device_control(
        &mut self,
        control: DeviceControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match control {
            DeviceControl::CodeLink => self.reveal_device_code(window, cx),
            DeviceControl::Button(pressed) => self.press_device_button(pressed, window, cx),
            DeviceControl::Field => {}
        }
    }

    /// Follows a network event: the connection opens or closes the dialog,
    /// the host's messages move its phase, and the network thread's pairing
    /// outcomes end it.
    /// An event from a host other than the dialog's leaves the dialog alone.
    pub(crate) fn on_device_pairing_input(
        &mut self,
        host: &HostKey,
        input: PairingInput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let PairingInput::State = input {
            self.follow_connection_for_pairing(host, window, cx);
        } else if self.device_pairing_is_for(host) {
            self.on_dialog_host_input(input, window, cx);
        } else if let PairingInput::TokenSaveFailed = input {
            tracing::warn!(
                ?host,
                "device pairing: the token was lost with no dialog open for the host"
            );
        }
    }

    /// A pairing event from the dialog's own host.
    fn on_dialog_host_input(
        &mut self,
        input: PairingInput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let now = (self.now)();
        let before = self.device_pairing_model().map(DevicePairing::kind);
        match input {
            PairingInput::State => {}
            PairingInput::Paired => {
                tracing::info!("device pairing: paired; the dialog closes");
                self.close_device_pairing(window, cx);
            }
            PairingInput::Message(msg) => {
                if let Some(dialog) = &mut self.device_pairing.dialog {
                    dialog.model.on_message(&msg, now);
                }
            }
            PairingInput::Lost => {
                if let Some(dialog) = &mut self.device_pairing.dialog {
                    dialog.model.on_lost();
                }
            }
            PairingInput::RequestSent => {
                if let Some(dialog) = &mut self.device_pairing.dialog {
                    dialog.model.on_request_sent();
                }
            }
            PairingInput::TokenSaveFailed => {
                if let Some(dialog) = &mut self.device_pairing.dialog {
                    dialog.model.on_token_save_failed();
                }
            }
        }
        let Some(dialog) = &mut self.device_pairing.dialog else {
            return;
        };
        let after = dialog.model.kind();
        if before.is_some_and(|kind| kind != after) {
            if dialog.model.code().is_none_or(str::is_empty) {
                clear_code_field(dialog, cx);
            }
            dialog.control = phase_control(&dialog.model);
            self.apply_device_pairing_focus(window, cx);
        }
        self.sync_device_pairing(window, cx);
    }

    /// `host`'s connection moved: a "not approved" the owner's Pair or
    /// Reconnect waited for opens the dialog for it; a connection that
    /// opened or was refused otherwise closes its dialog, and one that
    /// failed withdraws a request still out, so no later redial asks with
    /// no dialog to finish it. While the host is still being dialled its
    /// dialog stays as it is. Another host's dialog is left alone.
    fn follow_connection_for_pairing(
        &mut self,
        host: &HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.conn_of(host).map(|conn| conn.state().clone()) else {
            return;
        };
        let ours = self.device_pairing_is_for(host);
        match state {
            State::NotApproved { .. } => {
                if self.device_pairing.pair_on_not_approved.remove(host) {
                    self.open_device_pairing(host, window, cx);
                }
            }
            State::Init | State::Connecting => {}
            State::Open { .. } | State::AuthFailed { .. } => {
                self.device_pairing.pair_on_not_approved.remove(host);
                if ours {
                    self.close_device_pairing(window, cx);
                }
            }
            State::Closed { .. } | State::Error { .. } => {
                if ours {
                    self.cancel_device_pairing(window, cx);
                }
            }
        }
    }

    /// Enter pairs (through the field's own event), Esc cancels, an edit
    /// keeps only the code's digits, and a click in the field moves the
    /// focus there.
    fn watch_device_code_field(
        input: &Entity<TextInput>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        let keys = cx.subscribe_in(
            input,
            window,
            |this, _, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => this.submit_device_code(window, cx),
                TextInputEvent::Cancel => this.cancel_device_pairing(window, cx),
            },
        );
        let edits = cx.subscribe_in(input, window, |this, _, _: &TextChanged, _, cx| {
            this.on_device_code_edited(cx);
        });
        let handle = input.read(cx).focus_handle(cx);
        let focus = cx.on_focus(&handle, window, |this, _, cx| {
            if let Some(dialog) = &mut this.device_pairing.dialog {
                dialog.control = Some(DeviceControl::Field);
            }
            cx.notify();
        });
        vec![keys, edits, focus]
    }

    /// The field keeps only digits, at most six.
    fn on_device_code_edited(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.device_pairing.dialog else {
            return;
        };
        let text = dialog.code_input.read(cx).text().to_owned();
        let kept = dialog.model.type_code(&text);
        if kept != text {
            dialog
                .code_input
                .update(cx, |input, cx| input.set_text(kept, cx));
        }
        cx.notify();
    }

    /// A key while the dialog is open: whether it was the dialog's (every
    /// key but typing in the code field is), and `None` while it is closed.
    /// Tab and Shift+Tab move the focus, Esc cancels, Enter presses the
    /// focused button or link or else the phase's main button, and Space
    /// the focused button or link.
    pub(crate) fn on_device_pairing_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<bool> {
        let dialog = self.device_pairing.dialog.as_ref()?;
        let focused = dialog
            .control
            .filter(|control| *control != DeviceControl::Field);
        let primary = dialog.model.primary().map(DeviceControl::Button);
        Some(match keystroke.key.as_str() {
            "tab" => {
                self.move_device_pairing_focus(!keystroke.modifiers.shift);
                self.apply_device_pairing_focus(window, cx);
                true
            }
            "escape" => {
                self.cancel_device_pairing(window, cx);
                true
            }
            _ if self.device_code_field_focused(window, cx) => false,
            "enter" | "space" => {
                let pressed = focused.or(primary.filter(|_| keystroke.key == "enter"));
                if let Some(pressed) = pressed {
                    self.press_device_control(pressed, window, cx);
                }
                true
            }
            _ => true,
        })
    }

    /// Steps the focus to the next control, or the previous one, wrapping.
    fn move_device_pairing_focus(&mut self, forward: bool) {
        let Some(dialog) = &mut self.device_pairing.dialog else {
            return;
        };
        let ring = control_ring(&dialog.model);
        if ring.is_empty() {
            return;
        }
        let at = dialog
            .control
            .and_then(|control| ring.iter().position(|c| *c == control));
        let next = match (at, forward) {
            (None, true) => 0,
            (None, false) => ring.len() - 1,
            (Some(i), true) => (i + 1) % ring.len(),
            (Some(i), false) => (i + ring.len() - 1) % ring.len(),
        };
        dialog.control = ring.get(next).copied();
    }

    /// The keyboard goes where the dialog's focus is: the code field, else
    /// the dialog itself.
    fn apply_device_pairing_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.device_pairing.dialog else {
            return;
        };
        match dialog.control {
            Some(DeviceControl::Field) => dialog.code_input.read(cx).focus_handle(cx).focus(window),
            Some(DeviceControl::CodeLink | DeviceControl::Button(_)) | None => {
                self.device_pairing.focus.focus(window);
            }
        }
        cx.notify();
    }

    /// Whether the code field holds the keyboard.
    fn device_code_field_focused(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.device_pairing.dialog.as_ref().is_some_and(|dialog| {
            dialog
                .code_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        })
    }

    /// Keeps the tick going while a countdown shows, and redraws.
    fn sync_device_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.device_pairing.drawn = self.device_pairing_countdown();
        if self.device_pairing.tick.is_none() && self.device_pairing.drawn.is_some() {
            self.device_pairing.tick = Some(cx.spawn_in(window, async move |this, cx| {
                loop {
                    cx.background_executor().timer(TICK).await;
                    // Fails only when the view is gone, and the countdown with it.
                    let going = this
                        .update_in(cx, |view, _, cx| view.on_device_pairing_tick(cx))
                        .unwrap_or(false);
                    if !going {
                        break;
                    }
                }
            }));
        }
        cx.notify();
    }

    /// A second passed: the view redraws only when the countdown reads
    /// differently. Returns whether the tick goes on.
    fn on_device_pairing_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let countdown = self.device_pairing_countdown();
        let going = countdown.is_some();
        if countdown != self.device_pairing.drawn {
            self.device_pairing.drawn = countdown;
            cx.notify();
        }
        if !going {
            self.device_pairing.tick = None;
        }
        going
    }

    /// The dialog over a backdrop that takes every click beneath it, drawn
    /// above the first-connect overlay.
    pub(crate) fn device_pairing_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = theme(cx);
        let dialog = self.device_pairing.dialog.as_ref()?;
        let model = &dialog.model;
        let body = div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .debug_selector(|| "device-pairing-line".to_owned())
                    .child(model.line()),
            )
            .children(model.offers_code_link().then(|| {
                let focused = dialog.control == Some(DeviceControl::CodeLink);
                div().flex().child(code_link(focused, cx))
            }))
            .children(model.code().map(|_| code_box(dialog, &t)))
            .children(model.countdown((self.now)()).map(|countdown| {
                div()
                    .debug_selector(|| "device-pairing-countdown".to_owned())
                    .text_color(gpui::rgb(t.muted))
                    .child(countdown)
            }))
            .children(model.refusal().map(|refusal| {
                div()
                    .debug_selector(|| "device-pairing-refusal".to_owned())
                    .px(px(6.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .bg(gpui::rgb(t.danger_bg))
                    .text_color(gpui::rgb(t.danger))
                    .child(refusal.to_owned())
            }));
        let footer = div()
            .flex()
            .justify_end()
            .gap(px(8.0))
            .children(model.buttons().iter().map(|pressed| {
                let focused = dialog.control == Some(DeviceControl::Button(*pressed));
                let enabled = button_enabled(model, *pressed);
                device_button(*pressed, focused, enabled, cx)
            }));
        let panel = modal_panel(
            crate::fonts::ui_text_size(cx),
            "device-pairing-panel",
            &theme(cx),
        )
        .track_focus(&self.device_pairing.focus)
        .child(div().font_weight(FontWeight::SEMIBOLD).child(model.title()))
        .child(body)
        .child(footer);
        Some(backdrop("device-pairing", panel, &theme(cx)))
    }
}

/// Empties the code field, as the owner did not type it: the model's code
/// is already empty or about to be.
fn clear_code_field(dialog: &DevicePairingDialog, cx: &mut Context<RootView>) {
    dialog
        .code_input
        .update(cx, |input, cx| input.set_text(String::new(), cx));
}

/// "Have a code?": plain text that lights up under the pointer, ringed
/// while it has the keyboard.
fn code_link(focused: bool, cx: &mut Context<RootView>) -> Stateful<Div> {
    let t = theme(cx);
    div()
        .id(CODE_LINK_SELECTOR)
        .debug_selector(|| CODE_LINK_SELECTOR.to_owned())
        .relative()
        .px(px(4.0))
        .py(px(2.0))
        .rounded(px(LINK_RADIUS))
        .text_color(gpui::rgb(if focused { t.text } else { t.muted }))
        .cursor_pointer()
        .hover(|style| {
            style
                .bg(gpui::rgb(t.popover_hover))
                .text_color(gpui::rgb(t.text))
        })
        .when(focused, |link| {
            link.child(focus_ring(LINK_RADIUS, &theme(cx)))
        })
        .child(CODE_LINK_LABEL)
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.reveal_device_code(window, cx);
        }))
}

/// The code field in its box, outlined brighter while it has the focus.
fn code_box(dialog: &DevicePairingDialog, t: &UiTheme) -> Div {
    let focused = dialog.control == Some(DeviceControl::Field);
    div()
        .debug_selector(|| "device-pairing-code".to_owned())
        .flex()
        .w(px(CODE_WIDTH))
        .px(px(6.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(if focused { t.text } else { t.border }))
        .child(dialog.code_input.clone())
}

/// A dialog button, ringed while it has the keyboard.
fn device_button(
    pressed: DeviceButton,
    focused: bool,
    enabled: bool,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let size = ButtonSize::Regular;
    let kind = if pressed.is_primary() {
        ButtonKind::Primary
    } else {
        ButtonKind::Outlined
    };
    let drawn = button(
        pressed.selector(),
        button_look(
            crate::fonts::ui_text_size(cx),
            kind,
            size,
            enabled,
            Layer::Floating,
            &theme(cx),
        ),
    )
    .when(focused, |button| {
        button.child(focus_ring(size.radius(), &theme(cx)))
    })
    .child(pressed.label());
    if !enabled {
        return drawn;
    }
    drawn.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.press_device_button(pressed, window, cx);
    }))
}
