//! The host rebind dialog: its modal, its keys, the layouts it asks the host
//! for, and the unbind and bind it sends This PC on a confirm; and the unbind
//! its entry points send at once. The model is [`crate::host_rebind`].

use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, Keystroke, Stateful, Window, div, prelude::*,
    px,
};
use protocol::{ClientMessage, DaemonMessage, FEATURE_HOST_LAYOUTS, HostLayout};

use crate::RootView;
use crate::buttons::{ButtonKind, ButtonSize, Layer, button, button_look, focus_ring};
use crate::driver_id::DriverId as _;
use crate::host_rebind::{HostRebind, RebindChoice};
use crate::layout_chooser_view::{
    PickerButtons, RowMarks, option_row_element, picker_body, picker_button_look,
};
use crate::layout_picker::Mode;
use crate::notice_view::modal_panel;
use crate::session_menu::backdrop;
use crate::tabs::{BindTarget, SessionRef};
use crate::target::HostKey;
use crate::ui_theme::theme;

/// The choice row that lays the sessions out with the picker.
const AUTO_LABEL: &str = "Auto layout";
const CONFIRM_LABEL: &str = "Rebind";
const CANCEL_LABEL: &str = "Cancel";

/// A control of the dialog that takes a press and the keyboard's focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RebindControl {
    /// The Auto layout choice.
    Auto,
    Mode(Mode),
    /// A grid shape by its column count; 0 is Auto.
    Shape(usize),
    MaxDown,
    MaxUp,
    /// Follows the layout of the client with this id.
    Follow(String),
    Confirm,
    Cancel,
}

impl RebindControl {
    /// The debug selector the control is tagged with.
    fn selector(&self) -> String {
        match self {
            Self::Auto => "host-rebind-auto".to_owned(),
            Self::Mode(Mode::Grid) => "host-rebind-layout-grid".to_owned(),
            Self::Mode(Mode::SideBySide) => "host-rebind-layout-horizontal".to_owned(),
            Self::Mode(Mode::Stacked) => "host-rebind-layout-vertical".to_owned(),
            Self::Shape(cols) => format!("host-rebind-grid-shape-{cols}"),
            Self::MaxDown => "host-rebind-max-down".to_owned(),
            Self::MaxUp => "host-rebind-max-up".to_owned(),
            Self::Follow(client_id) => format!("host-rebind-follow-{client_id}"),
            Self::Confirm => "host-rebind-confirm".to_owned(),
            Self::Cancel => "host-rebind-cancel".to_owned(),
        }
    }
}

/// The open dialog: its model, the layouts request still awaiting the
/// host's answer, and the control the keyboard is on.
#[derive(Debug, Clone)]
pub(crate) struct HostRebindDialog {
    model: HostRebind,
    layouts_request: Option<String>,
    focus: RebindControl,
}

impl HostRebindDialog {
    /// Every control shown, in the order Tab walks them: Auto, the picker's
    /// while Auto is the choice, each layout to follow, then the buttons.
    fn controls(&self) -> Vec<RebindControl> {
        let mut out = vec![RebindControl::Auto];
        if *self.model.choice() == RebindChoice::Auto {
            let picker = self.model.picker();
            out.push(RebindControl::Mode(Mode::Grid));
            if picker.mode() == Mode::Grid {
                out.extend(picker.shape_cols().map(RebindControl::Shape));
            }
            out.extend([
                RebindControl::Mode(Mode::SideBySide),
                RebindControl::Mode(Mode::Stacked),
                RebindControl::MaxDown,
                RebindControl::MaxUp,
            ]);
        }
        out.extend(
            self.model
                .follow_options()
                .into_iter()
                .map(|(client_id, _)| RebindControl::Follow(client_id)),
        );
        out.extend([RebindControl::Confirm, RebindControl::Cancel]);
        out
    }

    /// The control the keyboard is on: the one last focused while it is
    /// still shown, else Auto.
    fn focused(&self) -> RebindControl {
        if self.controls().contains(&self.focus) {
            self.focus.clone()
        } else {
            RebindControl::Auto
        }
    }

    /// Moves the focus to the next control shown (the previous one when
    /// not `forward`), wrapping around.
    fn move_focus(&mut self, forward: bool) {
        let controls = self.controls();
        let count = controls.len();
        if count == 0 {
            return;
        }
        let focused = self.focused();
        let at = controls
            .iter()
            .position(|control| *control == focused)
            .unwrap_or(0);
        let next = if forward {
            (at + 1) % count
        } else {
            (at + count - 1) % count
        };
        if let Some(next) = controls.get(next) {
            self.focus = next.clone();
        }
    }

    /// Presses `control`, which takes the focus; a control not shown does
    /// nothing. Confirm and Cancel are the view's to act on.
    fn press(&mut self, control: &RebindControl) {
        if !self.controls().contains(control) {
            return;
        }
        self.focus = control.clone();
        match control {
            RebindControl::Auto => self.model.choose_auto(),
            RebindControl::Mode(mode) => self.model.picker_mut().set_mode(*mode),
            RebindControl::Shape(cols) => self.model.picker_mut().set_shape(*cols),
            RebindControl::MaxDown => self.model.picker_mut().max_down(),
            RebindControl::MaxUp => self.model.picker_mut().max_up(),
            RebindControl::Follow(client_id) => self.model.choose_follow(client_id.clone()),
            RebindControl::Confirm | RebindControl::Cancel => {}
        }
    }

    /// Whether `control` shows as chosen: the choice, the layout mode and
    /// the grid shape picked.
    fn is_selected(&self, control: &RebindControl) -> bool {
        let picker = self.model.picker();
        match control {
            RebindControl::Auto => *self.model.choice() == RebindChoice::Auto,
            RebindControl::Follow(client_id) => {
                matches!(self.model.choice(), RebindChoice::Follow(chosen) if chosen == client_id)
            }
            RebindControl::Mode(mode) => picker.mode() == *mode,
            RebindControl::Shape(cols) => picker.cols() == *cols,
            _ => false,
        }
    }

    /// What `control` reads.
    fn label(&self, control: &RebindControl) -> String {
        match control {
            RebindControl::Auto => AUTO_LABEL.to_owned(),
            RebindControl::Mode(Mode::Grid) => "Grid".to_owned(),
            RebindControl::Mode(Mode::SideBySide) => "Side by side".to_owned(),
            RebindControl::Mode(Mode::Stacked) => "Stacked".to_owned(),
            RebindControl::Shape(cols) => self.model.picker().shape_label(*cols),
            RebindControl::MaxDown => "−".to_owned(),
            RebindControl::MaxUp => "+".to_owned(),
            RebindControl::Follow(client_id) => self
                .model
                .follow_options()
                .into_iter()
                .find(|(listed, _)| listed == client_id)
                .map_or_else(String::new, |(_, label)| format!("Follow {label}'s layout")),
            RebindControl::Confirm => CONFIRM_LABEL.to_owned(),
            RebindControl::Cancel => CANCEL_LABEL.to_owned(),
        }
    }

    fn title(&self) -> String {
        format!(
            "Rebind {}'s sessions",
            self.model.host_name().unwrap_or_default()
        )
    }

    /// The line under the title: how many active sessions are rebound.
    fn count_line(&self) -> String {
        match self.model.session_ids().len() {
            0 => "No active sessions".to_owned(),
            1 => "1 active session".to_owned(),
            count => format!("{count} active sessions"),
        }
    }
}

/// `host`'s slice of `by_host`; none when the host lists no sessions.
fn sessions_of<'a>(
    by_host: &[(HostKey, &'a [protocol::SessionSnapshot])],
    host: &HostKey,
) -> &'a [protocol::SessionSnapshot] {
    by_host
        .iter()
        .find(|(key, _)| key == host)
        .map_or(&[][..], |(_, sessions)| sessions)
}

impl RootView {
    /// Whether the host rebind dialog is open.
    #[must_use]
    pub fn host_rebind_open(&self) -> bool {
        self.host_rebind.is_some()
    }

    /// The open dialog's controls in the keyboard's order: selector and
    /// label.
    #[must_use]
    pub fn host_rebind_controls(&self) -> Vec<(String, String)> {
        let Some(dialog) = &self.host_rebind else {
            return Vec::new();
        };
        dialog
            .controls()
            .iter()
            .map(|control| (control.selector(), dialog.label(control)))
            .collect()
    }

    /// The open dialog's title and the active-session count under it.
    #[must_use]
    pub fn host_rebind_heading(&self) -> Option<(String, String)> {
        self.host_rebind
            .as_ref()
            .map(|dialog| (dialog.title(), dialog.count_line()))
    }

    /// Whether `host`'s sessions can be unbound and rebound: This PC
    /// always, a remote host while its connection is open.
    pub(crate) fn rebind_host_online(&self, host: &HostKey) -> bool {
        *host == HostKey::Local
            || self
                .conn_of(host)
                .is_some_and(crate::connection::Connection::is_open)
    }

    /// Unbinds every pane of `host`'s sessions at once; the sessions keep
    /// running. An offline host's is refused.
    pub(crate) fn unbind_host(&mut self, host: &HostKey) {
        if !self.rebind_host_online(host) {
            tracing::info!(?host, "unbind: the host is offline");
            return;
        }
        tracing::info!(?host, "unbinding the host's sessions");
        let msg = HostRebind::new(host.clone(), None, &[]).unbind_message(crate::new_request_id());
        self.send_to(&HostKey::Local, msg);
    }

    /// Opens the rebind dialog over `host`'s active sessions on the Auto
    /// layout, asking a host that can list its layouts for them. Any open
    /// menu gives way; an offline host's is refused.
    pub(crate) fn open_host_rebind(
        &mut self,
        host: &HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.rebind_host_online(host) {
            tracing::info!(?host, "rebind: the host is offline");
            return;
        }
        self.close_host_menu(window, cx);
        self.close_more_menu(window, cx);
        self.close_session_menu(window, cx);
        self.close_container_menu(window, cx);
        let model = {
            let by_host = self.sidebar.sessions_by_host();
            let name = self.host_name(host);
            HostRebind::new(host.clone(), Some(name), sessions_of(&by_host, host))
        };
        let layouts_request = self
            .conn_of(host)
            .is_some_and(|conn| conn.supports(FEATURE_HOST_LAYOUTS))
            .then(crate::new_request_id);
        if let Some(id) = &layouts_request {
            self.send_to(
                host,
                ClientMessage::ListHostLayouts {
                    request_id: Some(id.clone()),
                },
            );
        }
        tracing::info!(
            ?host,
            sessions = model.session_ids().len(),
            asks_layouts = layouts_request.is_some(),
            "host rebind: open"
        );
        self.host_rebind = Some(HostRebindDialog {
            model,
            layouts_request,
            focus: RebindControl::Auto,
        });
        if self.exit.is_none() {
            self.host_rebind_focus.focus(window, cx);
        }
        cx.notify();
    }

    /// Closes the dialog, when it is open, and hands the keyboard back; the
    /// layouts request still awaited goes with it.
    pub(crate) fn close_host_rebind(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.host_rebind.take().is_none() {
            return;
        }
        if self.exit.is_none() {
            self.after_notice_closed(window, cx);
        }
        cx.notify();
    }

    /// `host`'s connection closed: its dialog closes.
    pub(crate) fn close_host_rebind_of(
        &mut self,
        host: &HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .host_rebind
            .as_ref()
            .is_some_and(|dialog| dialog.model.host() == host)
        {
            self.close_host_rebind(window, cx);
        }
    }

    /// Folds `host`'s message into the open dialog: its layouts, or the
    /// refusal of the request for them.
    pub(crate) fn on_host_rebind_message(
        &mut self,
        host: &HostKey,
        msg: &DaemonMessage,
        cx: &mut Context<Self>,
    ) {
        match msg {
            DaemonMessage::HostLayouts {
                host_name,
                layouts,
                request_id,
            } => self.on_host_layouts(
                host,
                host_name.clone(),
                layouts.clone(),
                request_id.as_deref(),
                cx,
            ),
            DaemonMessage::Error {
                request_id: Some(request_id),
                ..
            } => self.host_rebind_refused(request_id, cx),
            _ => {}
        }
    }

    /// The first-connect chooser's layer, then the rebind dialog's.
    pub(crate) fn layout_dialog_layers(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut layers: Vec<AnyElement> = self.layout_chooser_layer(cx).into_iter().collect();
        layers.extend(self.host_rebind_layer(cx));
        layers
    }

    /// `host`'s `HostLayouts`: the open dialog that asked for them offers
    /// them to follow; any other is ignored.
    pub(crate) fn on_host_layouts(
        &mut self,
        host: &HostKey,
        host_name: Option<String>,
        layouts: Vec<HostLayout>,
        request_id: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = self.host_rebind.as_mut().filter(|dialog| {
            dialog.model.host() == host
                && dialog.layouts_request.is_some()
                && dialog.layouts_request.as_deref() == request_id
        }) else {
            tracing::debug!(
                ?host,
                ?request_id,
                "host layouts: no open rebind dialog asked for them"
            );
            return;
        };
        tracing::info!(
            ?host,
            layouts = layouts.len(),
            "host rebind: the host's layouts"
        );
        dialog.model.set_layouts(host_name, layouts);
        dialog.layouts_request = None;
        cx.notify();
    }

    /// An `Error` answering `request_id`: when it refused the dialog's
    /// layouts request, the dialog stays on Auto with nothing to follow.
    pub(crate) fn host_rebind_refused(&mut self, request_id: &str, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.host_rebind
            && dialog.layouts_request.as_deref() == Some(request_id)
        {
            tracing::info!(host = ?dialog.model.host(), "host rebind: the host refused its layouts");
            dialog.layouts_request = None;
            cx.notify();
        }
    }

    /// `host`'s session list changed: its open dialog rebuilds its list.
    pub(crate) fn refresh_host_rebind(&mut self, host: &HostKey, cx: &mut Context<Self>) {
        let Some(dialog) = self
            .host_rebind
            .as_mut()
            .filter(|dialog| dialog.model.host() == host)
        else {
            return;
        };
        let by_host = self.sidebar.sessions_by_host();
        dialog.model.refresh(sessions_of(&by_host, host));
        cx.notify();
    }

    /// A key while the dialog is open; returns whether it was. Esc closes
    /// it, Tab and Shift+Tab move the focus, Enter presses the focused
    /// control, and every other key does nothing.
    pub(crate) fn on_host_rebind_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(dialog) = &mut self.host_rebind else {
            return false;
        };
        match keystroke.key.as_str() {
            "escape" => self.close_host_rebind(window, cx),
            "tab" => {
                dialog.move_focus(!keystroke.modifiers.shift);
                cx.notify();
            }
            "enter" => {
                let control = dialog.focused();
                self.press_host_rebind_control(&control, window, cx);
            }
            _ => {}
        }
        true
    }

    /// Presses `control`: Rebind sends, Cancel closes, any other changes
    /// the choice.
    fn press_host_rebind_control(
        &mut self,
        control: &RebindControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match control {
            RebindControl::Confirm => self.confirm_host_rebind(window, cx),
            RebindControl::Cancel => self.close_host_rebind(window, cx),
            _ => {
                if let Some(dialog) = &mut self.host_rebind {
                    dialog.press(control);
                    cx.notify();
                }
            }
        }
    }

    /// Rebind: This PC unbinds the host's sessions, then binds its active
    /// ones again as chosen, each claiming its size in its new pane. The
    /// new tabs take no focus.
    fn confirm_host_rebind(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let aspect = self.pane_area_aspect();
        let Some(dialog) = self.host_rebind.take() else {
            return;
        };
        if !self.rebind_host_online(dialog.model.host()) {
            tracing::info!(host = ?dialog.model.host(), "host rebind: the host went offline; nothing sent");
            if self.exit.is_none() {
                self.after_notice_closed(window, cx);
            }
            cx.notify();
            return;
        }
        let ids = (crate::new_request_id(), crate::new_request_id());
        tracing::info!(
            host = ?dialog.model.host(),
            sessions = dialog.model.session_ids().len(),
            "host rebind: unbinding and binding again"
        );
        // The bind the user made claims each rebound session's size, as any
        // bind this client makes does; the unbind's id carries none.
        let bind_id = ids.1.clone();
        for id in dialog.model.session_ids() {
            self.tabs.arm_bind(
                &SessionRef::new(dialog.model.host().clone(), id.clone()),
                BindTarget::Request(bind_id.clone()),
            );
        }
        for msg in dialog.model.rebind_messages(aspect, ids) {
            self.send_to(&HostKey::Local, msg);
        }
        if self.exit.is_none() {
            self.after_notice_closed(window, cx);
        }
        cx.notify();
    }

    /// The dialog over a backdrop that takes every click beneath it.
    pub(crate) fn host_rebind_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.host_rebind.as_ref()?;
        let t = theme(cx);
        let focused = dialog.focused();
        let mut options = vec![choice_row(dialog, &RebindControl::Auto, &focused, cx)];
        if *dialog.model.choice() == RebindChoice::Auto {
            options.push(rebind_picker(dialog, &focused, cx).into_any_element());
        }
        for control in dialog
            .controls()
            .into_iter()
            .filter(|control| matches!(control, RebindControl::Follow(_)))
        {
            options.push(choice_row(dialog, &control, &focused, cx));
        }
        let buttons = div()
            .flex()
            .justify_end()
            .gap(px(8.0))
            .child(action_button(dialog, &RebindControl::Cancel, &focused, cx))
            .child(action_button(dialog, &RebindControl::Confirm, &focused, cx));
        let panel = modal_panel(crate::fonts::ui_text_size(cx), "host-rebind-panel", &t)
            .driver_id(|| "host-rebind-dialog".to_owned())
            .track_focus(&self.host_rebind_focus)
            .child(
                div()
                    .driver_id(|| "host-rebind-title".to_owned())
                    .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                    .child(dialog.title()),
            )
            .child(
                div()
                    .driver_id(|| "host-rebind-count".to_owned())
                    .text_color(gpui::rgb(t.muted))
                    .child(dialog.count_line()),
            )
            .child(div().flex().flex_col().gap(px(6.0)).children(options))
            .child(buttons);
        Some(backdrop("host-rebind", panel, &t))
    }
}

/// A choice row of the dialog: Auto, or a layout to follow; filled while
/// chosen and outlined while focused.
fn choice_row(
    dialog: &HostRebindDialog,
    control: &RebindControl,
    focused: &RebindControl,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let pressed = control.clone();
    option_row_element(
        crate::fonts::ui_weight_shift(cx),
        control.selector(),
        dialog.label(control),
        None,
        RowMarks {
            focused: control == focused,
            selected: dialog.is_selected(control),
        },
        &theme(cx),
    )
    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.press_host_rebind_control(&pressed, window, cx);
    }))
    .into_any_element()
}

/// A small button of the picker: filled when chosen, ringed when focused.
fn picker_button(
    dialog: &HostRebindDialog,
    control: &RebindControl,
    focused: &RebindControl,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let pressed = control.clone();
    picker_button_look(
        &control.selector(),
        dialog.label(control),
        control == focused,
        dialog.is_selected(control),
        cx,
    )
    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.press_host_rebind_control(&pressed, window, cx);
    }))
}

/// The Auto layout's picker: the layout modes with the grid shapes under
/// Grid, and the max-per-tab stepper.
fn rebind_picker(
    dialog: &HostRebindDialog,
    focused: &RebindControl,
    cx: &mut Context<RootView>,
) -> Div {
    let t = theme(cx);
    let mut button = |control: &RebindControl| picker_button(dialog, control, focused, cx);
    let grid = button(&RebindControl::Mode(Mode::Grid));
    let shapes: Vec<Stateful<Div>> = dialog
        .controls()
        .iter()
        .filter(|control| matches!(control, RebindControl::Shape(_)))
        .map(&mut button)
        .collect();
    let buttons = PickerButtons {
        grid,
        shapes,
        side: button(&RebindControl::Mode(Mode::SideBySide)),
        stacked: button(&RebindControl::Mode(Mode::Stacked)),
        down: button(&RebindControl::MaxDown),
        up: button(&RebindControl::MaxUp),
    };
    picker_body(
        buttons,
        "host-rebind-max-value",
        dialog.model.picker().max_label(),
        &t,
    )
}

/// Rebind (primary) or Cancel, ringed while focused.
fn action_button(
    dialog: &HostRebindDialog,
    control: &RebindControl,
    focused: &RebindControl,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let t = theme(cx);
    let kind = if *control == RebindControl::Confirm {
        ButtonKind::Primary
    } else {
        ButtonKind::Outlined
    };
    let size = ButtonSize::Regular;
    let pressed = control.clone();
    button(
        &control.selector(),
        button_look(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            kind,
            size,
            true,
            Layer::Floating,
            &t,
        ),
    )
    .relative()
    .when(control == focused, |button| {
        button.child(focus_ring(size.radius(), &t))
    })
    .child(dialog.label(control))
    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.press_host_rebind_control(&pressed, window, cx);
    }))
}
