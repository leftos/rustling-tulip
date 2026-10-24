//! The approved devices on screen: the Remote access page's Devices
//! section with each row's Block or Forget, and the block confirm with its
//! keys. The model is [`crate::devices`].

use chrono::Utc;
use gpui::{
    AnyElement, ClickEvent, Context, Div, FocusHandle, FontWeight, Keystroke, Window, div,
    prelude::*, px,
};
use protocol::{ClientMessage, DaemonMessage, FEATURE_DEVICE_BLOCK};

use crate::buttons::{ButtonKind, ButtonSize, button};
use crate::devices::{
    BLOCKED_TAG, BlockConfirm, CONFIRM_BODY, ConfirmButton, DeviceRow, DevicesState, NO_DEVICES,
    RowAction, forget_message,
};
use crate::notice_view::modal_panel;
use crate::session_menu::{backdrop, dialog_button};
use crate::settings_view::SettingsTab;
use crate::{DANGER, MUTED, RootView};

/// The devices model with the block confirm's keyboard focus.
pub(crate) struct DevicesView {
    model: DevicesState,
    focus: FocusHandle,
}

impl DevicesView {
    pub(crate) fn new(focus: FocusHandle) -> Self {
        Self {
            model: DevicesState::default(),
            focus,
        }
    }
}

impl RootView {
    /// Whether this client may list, block and forget devices: only on this
    /// PC's daemon, and only one that ends a blocked device's connections.
    fn devices_offered(&self) -> bool {
        self.conn.target().is_local() && self.conn.supports(FEATURE_DEVICE_BLOCK)
    }

    /// Asks this PC's daemon for its approved devices, when it blocks them.
    pub(crate) fn ask_devices(&self) {
        if self.devices_offered() {
            self.send(ClientMessage::ListDevices);
        }
    }

    /// The Remote access page was selected: a confirm left from an earlier
    /// visit goes, and the list is asked for afresh.
    pub(crate) fn on_devices_page_selected(&mut self) {
        self.devices.model.close_confirm();
        self.ask_devices();
    }

    /// Folds in the daemon's device list; any other message is not the
    /// devices model's.
    pub(crate) fn on_devices_message(
        &mut self,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let DaemonMessage::Devices { devices } = msg else {
            return;
        };
        tracing::info!(devices = devices.len(), "devices: the approved list");
        if self.devices.model.on_devices(devices.clone()) {
            self.after_block_confirm_closed(window, cx);
        }
        cx.notify();
    }

    /// The Devices rows as drawn: each device's name, login, and status or
    /// Blocked tag.
    #[must_use]
    pub fn device_rows(&self) -> Vec<(String, String, String)> {
        self.devices
            .model
            .rows(Utc::now())
            .into_iter()
            .map(|row| (row.name, row.login, row.status))
            .collect()
    }

    /// The block confirm's title and the selector of its focused button,
    /// while it shows.
    #[must_use]
    pub fn device_block_confirm(&self) -> Option<(String, &'static str)> {
        self.block_confirm()
            .map(|confirm| (confirm.title(), confirm.focused().selector()))
    }

    /// The block confirm, while Settings shows the Remote access page.
    fn block_confirm(&self) -> Option<&BlockConfirm> {
        let on_page = self.settings_open() && self.settings_tab == SettingsTab::RemoteAccess;
        self.devices.model.confirm().filter(|_| on_page)
    }

    /// Another daemon or a lost one: the list and the confirm go.
    pub(crate) fn reset_devices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let had_confirm = self.devices.model.confirm().is_some();
        self.devices.model.reset();
        if had_confirm {
            self.after_block_confirm_closed(window, cx);
        }
        cx.notify();
    }

    /// Block on a row: its confirm opens with Cancel focused; sends nothing.
    fn open_device_block_confirm(
        &mut self,
        device_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.devices.model.open_confirm(device_id) {
            self.devices.focus.focus(window);
        }
        cx.notify();
    }

    /// Forget on a blocked row: sent at once, since a blocked device has no
    /// live connection to lose.
    fn forget_device(&self, device_id: &str) {
        tracing::info!(device = device_id, "devices: forgetting a device");
        self.send(forget_message(device_id));
    }

    /// Closes the confirm; Block first sends the block.
    fn press_device_block_button(
        &mut self,
        pressed: ConfirmButton,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(msg) = self.devices.model.press(pressed) {
            tracing::info!("devices: blocking a device");
            self.send(msg);
        }
        self.after_block_confirm_closed(window, cx);
    }

    /// A key while the approval dialog or the block confirm shows; returns
    /// whether one did.
    pub(crate) fn on_device_dialogs_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // The confirm is drawn above the approval dialog, so it takes the
        // keys first.
        self.on_device_block_key(keystroke, window, cx)
            || self.on_pairing_key(keystroke, window, cx)
    }

    /// Whether the block confirm shows, above any approval dialog.
    pub(crate) fn device_block_confirm_open(&self) -> bool {
        self.block_confirm().is_some()
    }

    /// Hands the keyboard to the block confirm, if it shows; returns
    /// whether it does.
    pub(crate) fn focus_device_block_confirm(&self, window: &mut Window) -> bool {
        let open = self.device_block_confirm_open();
        if open {
            self.devices.focus.focus(window);
        }
        open
    }

    /// The confirm closed: the keyboard goes to the approval dialog if one
    /// is open beneath it, else wherever a closed modal hands it.
    fn after_block_confirm_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_pairing_dialog(window) {
            self.after_notice_closed(window, cx);
        }
        cx.notify();
    }

    /// A key while the block confirm shows; returns whether it did. Esc
    /// cancels, Tab and Shift+Tab move between the buttons, and Enter
    /// presses the focused one.
    fn on_device_block_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.block_confirm().is_none() {
            return false;
        }
        match self.devices.model.confirm_key(keystroke.key.as_str()) {
            Some(pressed) => self.press_device_block_button(pressed, window, cx),
            None => cx.notify(),
        }
        true
    }

    /// The Remote access page's Devices section, while this client may
    /// block devices.
    pub(crate) fn devices_section(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.devices_offered() {
            return None;
        }
        let rows = self.devices.model.rows(Utc::now());
        let empty = rows.is_empty().then(|| {
            div()
                .debug_selector(|| "settings-devices-empty".to_owned())
                .text_color(gpui::rgb(MUTED))
                .child(NO_DEVICES)
        });
        let section = div()
            .debug_selector(|| "settings-devices".to_owned())
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Devices"))
            .children(empty)
            .children(rows.iter().map(|row| device_row(row, cx)));
        Some(section.into_any_element())
    }

    /// The block confirm over a backdrop that takes every click beneath it.
    pub(crate) fn device_block_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let confirm = self.block_confirm()?;
        let buttons = ConfirmButton::ALL.into_iter().map(|pressed| {
            dialog_button(
                pressed.selector(),
                pressed.label().to_owned(),
                pressed == ConfirmButton::Block,
                confirm.focused() == pressed,
                true,
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.press_device_block_button(pressed, window, cx);
            }))
        });
        let panel = modal_panel("device-block-panel")
            .track_focus(&self.devices.focus)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(confirm.title()),
            )
            .child(div().text_color(gpui::rgb(MUTED)).child(CONFIRM_BODY))
            .child(div().flex().justify_end().gap(px(8.0)).children(buttons));
        Some(backdrop("device-block-confirm", panel))
    }
}

/// One approved device: its name and login, its status or the Blocked tag,
/// then Block, or Forget once blocked.
fn device_row(row: &DeviceRow, cx: &mut Context<RootView>) -> Div {
    let id = row.device_id.clone();
    let action = row.action();
    let pressed_id = id.clone();
    let press = button(
        &action.selector(&id),
        match action {
            RowAction::Block => ButtonKind::Danger,
            RowAction::Forget => ButtonKind::Outlined,
        },
        ButtonSize::Compact,
        true,
    )
    .child(action.label())
    .on_click(
        cx.listener(move |this, _: &ClickEvent, window, cx| match action {
            RowAction::Block => this.open_device_block_confirm(&pressed_id, window, cx),
            RowAction::Forget => this.forget_device(&pressed_id),
        }),
    );
    let state = if row.blocked {
        let selector = format!("settings-device-blocked-{id}");
        div()
            .debug_selector(|| selector)
            .px(px(6.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(gpui::rgb(DANGER))
            .text_color(gpui::rgb(DANGER))
            .child(BLOCKED_TAG)
    } else {
        div().text_color(gpui::rgb(MUTED)).child(row.status.clone())
    };
    let selector = format!("settings-device-{id}");
    div()
        .debug_selector(|| selector)
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .gap(px(8.0))
                .child(row.name.clone())
                .child(div().text_color(gpui::rgb(MUTED)).child(row.login.clone())),
        )
        .child(state)
        .child(press)
}
