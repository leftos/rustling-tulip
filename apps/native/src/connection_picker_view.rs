//! The Connections picker on screen: This PC, the saved hosts and the
//! owner's online tailnet peers, a Forget button per saved host, and the
//! Add a host fields. Its rows and rules live in [`crate::connection_picker`];
//! a pick goes to the network thread as [`NetCommand::SwitchTarget`], and
//! the reset for the new target runs when its state comes back.

use std::collections::HashSet;
use std::sync::Arc;

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, Focusable as _, FontWeight, Keystroke,
    Subscription, Task, Window, div, prelude::*, px,
};

use crate::RootView;
use crate::connection_picker::{
    ConnectionPicker, PeerSource, Row, RowKind, SavedHost, typed_target,
};
use crate::net::NetCommand;
use crate::notice_view::modal_panel;
use crate::notices::ToastKind;
use crate::palette::{BORDER, DANGER, DANGER_BG, MUTED, TEXT};
use crate::session_menu::{backdrop, dialog_button};
use crate::target::{DEFAULT_TAILNET_PORT, Target};
use crate::text_input::{TextChanged, TextInput, TextInputEvent};

const TITLE: &str = "Connections";
const NAME_PLACEHOLDER: &str = "box.tail1234.ts.net";
const SCANNING: &str = "Looking for your devices…";

/// A control of the picker, in the order Tab walks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerControl {
    /// The row at this index of [`ConnectionPicker::rows`].
    Row(usize),
    /// The Forget button of the saved host at this row index.
    Forget(usize),
    Name,
    Port,
    Connect,
    Close,
}

impl PickerControl {
    /// The control's debug selector; a row's carries its label.
    fn selector(self, rows: &[Row]) -> String {
        let label = |index: usize| rows.get(index).map_or("", |row| row.label.as_str());
        match self {
            Self::Row(index) => format!("connections-row-{}", label(index)),
            Self::Forget(index) => format!("connections-forget-{}", label(index)),
            Self::Name => "connections-name".to_owned(),
            Self::Port => "connections-port".to_owned(),
            Self::Connect => "connections-connect".to_owned(),
            Self::Close => "connections-close".to_owned(),
        }
    }
}

/// Every control of a picker showing `rows`, in Tab order: each row then
/// its Forget button, the two fields, Connect and Close.
fn control_ring(rows: &[Row]) -> Vec<PickerControl> {
    let mut ring = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        ring.push(PickerControl::Row(index));
        if row.kind == RowKind::Saved {
            ring.push(PickerControl::Forget(index));
        }
    }
    ring.extend([
        PickerControl::Name,
        PickerControl::Port,
        PickerControl::Connect,
        PickerControl::Close,
    ]);
    ring
}

/// The open picker: its model, the Add a host fields, the focus, the typed
/// host's refusal, the saved hosts this PC holds no device token for and the
/// Tailscale scan in flight.
pub(crate) struct ConnectionPickerDialog {
    picker: ConnectionPicker,
    /// The saved hosts with no device token, read once as the picker opened.
    not_paired: HashSet<String>,
    name_input: Entity<TextInput>,
    port_input: Entity<TextInput>,
    focus: PickerControl,
    error: Option<String>,
    _scan: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl RootView {
    /// Whether the Connections picker is open.
    #[must_use]
    pub fn connection_picker_open(&self) -> bool {
        self.connection_picker.is_some()
    }

    /// Each row's label and whether it is the current target.
    #[must_use]
    pub fn connection_picker_rows(&self) -> Vec<(String, bool)> {
        let Some(dialog) = &self.connection_picker else {
            return Vec::new();
        };
        dialog
            .picker
            .rows()
            .into_iter()
            .map(|row| {
                let current = row.target == *self.conn.target();
                (row.label, current)
            })
            .collect()
    }

    /// The rows tagged "Not paired": saved hosts this PC holds no device
    /// token for, in row order.
    #[must_use]
    pub fn connection_picker_not_paired(&self) -> Vec<String> {
        let Some(dialog) = &self.connection_picker else {
            return Vec::new();
        };
        dialog
            .picker
            .rows()
            .into_iter()
            .filter(|row| row.kind == RowKind::Saved && dialog.not_paired.contains(&row.label))
            .map(|row| row.label)
            .collect()
    }

    /// The saved hosts this PC holds no device token for. A host whose
    /// token cannot be read is left untagged.
    fn hosts_not_paired(&self, saved: &[SavedHost]) -> HashSet<String> {
        saved
            .iter()
            .filter(|host| match self.tokens.get(&host.name) {
                Ok(token) => token.is_none(),
                Err(err) => {
                    tracing::warn!(
                        host = host.name.as_str(),
                        "reading the host's device token for the picker: {err:#}"
                    );
                    false
                }
            })
            .map(|host| host.name.clone())
            .collect()
    }

    /// The typed host's refusal under the fields.
    #[must_use]
    pub fn connection_picker_error(&self) -> Option<String> {
        self.connection_picker.as_ref()?.error.clone()
    }

    /// The line under the rows: the scan's wait or its error.
    #[must_use]
    pub fn connection_picker_scan_text(&self) -> Option<String> {
        let picker = &self.connection_picker.as_ref()?.picker;
        if picker.scan_pending() {
            return Some(SCANNING.to_owned());
        }
        picker.scan_error().map(str::to_owned)
    }

    /// The selector of the picker's focused control.
    #[must_use]
    pub fn connection_picker_focus(&self, _cx: &App) -> Option<String> {
        let dialog = self.connection_picker.as_ref()?;
        Some(dialog.focus.selector(&dialog.picker.rows()))
    }

    /// Replaces the Tailscale the picker scans.
    pub fn set_peer_source(&mut self, source: Arc<dyn PeerSource + Send + Sync>) {
        self.peer_source = source;
    }

    /// Connections: opens the picker over the saved hosts, focused on the
    /// current target's row, and starts one Tailscale scan.
    pub fn open_connection_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.connection_picker.is_some() {
            return;
        }
        let saved = self.sidebar.ui_state().saved_hosts.clone();
        let not_paired = self.hosts_not_paired(&saved);
        let picker = ConnectionPicker::new(saved);
        let current = picker
            .rows()
            .iter()
            .position(|row| row.target == *self.conn.target())
            .unwrap_or(0);
        let name_input = cx.new(|cx| TextInput::new(String::new(), NAME_PLACEHOLDER, cx));
        let port_input = cx.new(|cx| TextInput::new(DEFAULT_TAILNET_PORT.to_string(), "", cx));
        let mut subscriptions =
            Self::watch_picker_field(&name_input, PickerControl::Name, window, cx);
        subscriptions.extend(Self::watch_picker_field(
            &port_input,
            PickerControl::Port,
            window,
            cx,
        ));
        let scan = self.start_connection_scan(window, cx);
        self.close_flyout();
        self.connection_picker = Some(ConnectionPickerDialog {
            picker,
            not_paired,
            name_input,
            port_input,
            focus: PickerControl::Row(current),
            error: None,
            _scan: scan,
            _subscriptions: subscriptions,
        });
        self.apply_connection_picker_focus(window, cx);
    }

    /// Closes the picker, dropping a scan still in flight, and hands the
    /// keyboard back to the active pane.
    fn close_connection_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.connection_picker.take().is_none() {
            return;
        }
        self.focus_active_pane(window, cx);
        cx.notify();
    }

    /// Runs the blocking scan off the UI thread; its answer reaches the
    /// picker that asked, and is dropped with the task when it closes.
    fn start_connection_scan(&self, window: &mut Window, cx: &mut Context<Self>) -> Task<()> {
        let source = Arc::clone(&self.peer_source);
        let scan = cx
            .background_executor()
            .spawn(async move { source.status() });
        cx.spawn_in(window, async move |this, cx| {
            let outcome = scan.await;
            // Fails only when the view is gone, leaving nowhere to show it.
            this.update_in(cx, |root, _, cx| {
                if let Some(dialog) = &mut root.connection_picker {
                    dialog.picker.apply_scan(outcome);
                    cx.notify();
                }
            })
            .ok();
        })
    }

    /// Enter connects the typed host, Esc closes, an edit clears the
    /// refusal, and a click in the field moves the focus there.
    fn watch_picker_field(
        input: &Entity<TextInput>,
        control: PickerControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        let keys = cx.subscribe_in(
            input,
            window,
            |this, _, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => this.connect_typed_host(window, cx),
                TextInputEvent::Cancel => this.close_connection_picker(window, cx),
            },
        );
        let edits = cx.subscribe_in(input, window, |this, _, _: &TextChanged, _, cx| {
            if let Some(dialog) = &mut this.connection_picker {
                dialog.error = None;
            }
            cx.notify();
        });
        let handle = input.read(cx).focus_handle(cx);
        let focus = cx.on_focus(&handle, window, move |this, _, cx| {
            if let Some(dialog) = &mut this.connection_picker {
                dialog.focus = control;
            }
            cx.notify();
        });
        vec![keys, edits, focus]
    }

    /// A key while the picker is open: whether it was the picker's (every
    /// key but typing in a field is), and `None` while it is closed. While
    /// it is open no key reaches a layer under it. Esc closes, Tab and
    /// Shift+Tab move the focus, Enter or Space press the focused control.
    pub(crate) fn on_connection_picker_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<bool> {
        let focused = self.connection_picker.as_ref()?.focus;
        Some(match keystroke.key.as_str() {
            "escape" => {
                self.close_connection_picker(window, cx);
                true
            }
            "tab" => {
                self.move_connection_picker_focus(!keystroke.modifiers.shift);
                self.apply_connection_picker_focus(window, cx);
                true
            }
            _ if self.connection_field_focused(window, cx) => false,
            "enter" | "space" => {
                self.press_connection_control(focused, window, cx);
                true
            }
            _ => true,
        })
    }

    /// Steps the focus to the next control, or the previous one, wrapping.
    fn move_connection_picker_focus(&mut self, forward: bool) {
        let Some(dialog) = &mut self.connection_picker else {
            return;
        };
        let ring = control_ring(&dialog.picker.rows());
        let at = ring
            .iter()
            .position(|control| *control == dialog.focus)
            .unwrap_or(0);
        let next = if forward {
            (at + 1) % ring.len()
        } else {
            (at + ring.len() - 1) % ring.len()
        };
        if let Some(control) = ring.get(next) {
            dialog.focus = *control;
        }
    }

    /// A click on `control`, or Enter or Space while it has the focus.
    fn press_connection_control(
        &mut self,
        control: PickerControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(dialog) = &mut self.connection_picker {
            dialog.focus = control;
        }
        match control {
            PickerControl::Row(index) => self.pick_connection_row(index, window, cx),
            PickerControl::Forget(index) => self.forget_connection_row(index, window, cx),
            PickerControl::Connect => self.connect_typed_host(window, cx),
            PickerControl::Close => self.close_connection_picker(window, cx),
            PickerControl::Name | PickerControl::Port => {
                self.apply_connection_picker_focus(window, cx);
            }
        }
    }

    /// Connects to the row at `index`.
    fn pick_connection_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let target = self
            .connection_picker
            .as_ref()
            .and_then(|dialog| dialog.picker.rows().into_iter().nth(index))
            .map(|row| row.target);
        if let Some(target) = target {
            self.switch_to_target(target, window, cx);
        }
    }

    /// Connect: the typed name and port, or why they are refused.
    fn connect_typed_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.connection_picker else {
            return;
        };
        let name = dialog.name_input.read(cx).text().to_owned();
        let port = dialog.port_input.read(cx).text().to_owned();
        match typed_target(&name, &port) {
            Ok(target) => self.switch_to_target(target, window, cx),
            Err(refusal) => {
                dialog.error = Some(refusal.to_string());
                cx.notify();
            }
        }
    }

    /// Closes the picker and, unless `target` is the current one, sends the
    /// switch and saves it as the boot target, a tailnet host also among the
    /// saved hosts. The current host, while it does not know this PC, opens
    /// the pairing dialog instead.
    fn switch_to_target(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        let current = target == *self.conn.target();
        let pair_current = current && self.device_pairing_offered();
        if !current {
            let host = SavedHost::from_target(&target);
            if let (Some(host), Some(dialog)) = (&host, &mut self.connection_picker) {
                dialog.picker.remember(host.clone());
                self.sidebar.set_saved_hosts(dialog.picker.saved().to_vec());
            }
            if host.is_some() {
                self.pair_on_not_approved();
            }
            self.sidebar.set_last_target(host);
            self.save_ui();
            self.command(NetCommand::SwitchTarget(target));
        }
        self.close_connection_picker(window, cx);
        if pair_current {
            self.open_device_pairing(window, cx);
        }
    }

    /// Forget on the saved host at row `index`, its device token deleted
    /// with it; forgetting the current host switches to this PC. The picker
    /// stays open.
    fn forget_connection_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.conn.target().clone();
        let Some(dialog) = &mut self.connection_picker else {
            return;
        };
        let Some(row) = dialog.picker.rows().into_iter().nth(index) else {
            return;
        };
        let was_current = dialog.picker.forget(&row.label, &current);
        self.sidebar.forget_host_session_count(&row.label);
        let rows = dialog.picker.rows().len();
        dialog.focus = PickerControl::Row(index.min(rows.saturating_sub(1)));
        self.sidebar.set_saved_hosts(dialog.picker.saved().to_vec());
        if was_current {
            self.sidebar.set_last_target(None);
            self.command(NetCommand::SwitchTarget(Target::Local));
        }
        self.save_ui();
        self.forget_device_token(&row.label, cx);
        self.apply_connection_picker_focus(window, cx);
    }

    /// Deletes `host`'s device token; a failed delete says so in a toast.
    fn forget_device_token(&mut self, host: &str, cx: &mut Context<Self>) {
        if let Err(err) = self.tokens.delete(host) {
            tracing::warn!(host, "deleting the forgotten host's device token: {err:#}");
            self.push_toast(
                ToastKind::Error,
                "Couldn't forget this host's device token",
                Some(err.to_string()),
                cx,
            );
        }
    }

    /// The keyboard goes where the picker's focus is: a field, else the
    /// picker itself.
    fn apply_connection_picker_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.connection_picker else {
            return;
        };
        match dialog.focus {
            PickerControl::Name => dialog.name_input.read(cx).focus_handle(cx).focus(window),
            PickerControl::Port => dialog.port_input.read(cx).focus_handle(cx).focus(window),
            _ => self.connection_picker_focus.focus(window),
        }
        cx.notify();
    }

    /// Whether the Name or Port field holds the keyboard.
    fn connection_field_focused(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.connection_picker.as_ref().is_some_and(|dialog| {
            [&dialog.name_input, &dialog.port_input]
                .iter()
                .any(|input| input.read(cx).focus_handle(cx).is_focused(window))
        })
    }

    /// The picker over a backdrop that takes every click beneath it, drawn
    /// above the first-connect overlay.
    pub(crate) fn connection_picker_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.connection_picker.as_ref()?;
        let rows = dialog.picker.rows();
        let focus = dialog.focus;
        let current = self.conn.target();
        let list = div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .children(rows.iter().enumerate().map(|(index, row)| {
                let tags = RowTags {
                    current: row.target == *current,
                    not_paired: row.kind == RowKind::Saved
                        && dialog.not_paired.contains(&row.label),
                };
                row_line(index, row, tags, focus, &rows, cx)
            }));
        let scan = self.connection_picker_scan_text().map(|text| {
            let failed = !dialog.picker.scan_pending();
            div()
                .debug_selector(|| "connections-scan".to_owned())
                .text_color(gpui::rgb(if failed { DANGER } else { MUTED }))
                .child(text)
        });
        let close = picker_button(PickerControl::Close, "Close", focus, &rows, cx);
        let panel = modal_panel("connections-panel")
            .debug_selector(|| "connections-panel".to_owned())
            .track_focus(&self.connection_picker_focus)
            .child(div().font_weight(FontWeight::SEMIBOLD).child(TITLE))
            .child(list)
            .children(scan)
            .child(add_host(dialog, focus, &rows, cx))
            .child(div().flex().justify_end().child(close));
        Some(backdrop("connections-picker", panel))
    }
}

/// A dialog button for `control` that presses it on a click.
fn picker_button(
    control: PickerControl,
    label: &str,
    focus: PickerControl,
    rows: &[Row],
    cx: &mut Context<RootView>,
) -> gpui::Stateful<Div> {
    dialog_button(
        &control.selector(rows),
        label.to_owned(),
        false,
        focus == control,
        true,
    )
    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.press_connection_control(control, window, cx);
    }))
}

/// The muted tags a row carries.
#[derive(Debug, Clone, Copy)]
struct RowTags {
    /// The row is the current target.
    current: bool,
    /// A saved host this PC holds no device token for.
    not_paired: bool,
}

/// One row: its button, a muted "Not paired" tag on a saved host this PC
/// holds no device token for, a muted "Current" tag on the current target,
/// and a saved host's Forget button.
fn row_line(
    index: usize,
    row: &Row,
    tags: RowTags,
    focus: PickerControl,
    rows: &[Row],
    cx: &mut Context<RootView>,
) -> Div {
    let pick = picker_button(PickerControl::Row(index), &row.label, focus, rows, cx).flex_1();
    let not_paired_selector = format!("connections-not-paired-{}", row.label);
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(pick)
        .when(tags.not_paired, |line| {
            line.child(
                div()
                    .debug_selector(|| not_paired_selector)
                    .text_color(gpui::rgb(MUTED))
                    .child("Not paired"),
            )
        })
        .when(tags.current, |line| {
            line.child(div().text_color(gpui::rgb(MUTED)).child("Current"))
        })
        .when(row.kind == RowKind::Saved, |line| {
            line.child(picker_button(
                PickerControl::Forget(index),
                "Forget",
                focus,
                rows,
                cx,
            ))
        })
}

/// Add a host: the Name and Port fields, Connect, and the refusal under
/// them.
fn add_host(
    dialog: &ConnectionPickerDialog,
    focus: PickerControl,
    rows: &[Row],
    cx: &mut Context<RootView>,
) -> Div {
    let fields = div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(field_box(&dialog.name_input, PickerControl::Name, focus).flex_1())
        .child(field_box(&dialog.port_input, PickerControl::Port, focus).w(px(80.0)))
        .child(picker_button(
            PickerControl::Connect,
            "Connect",
            focus,
            rows,
            cx,
        ));
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(div().text_color(gpui::rgb(MUTED)).child("Add a host"))
        .child(fields)
        .children(dialog.error.clone().map(|error| {
            div()
                .debug_selector(|| "connections-error".to_owned())
                .px(px(6.0))
                .py(px(3.0))
                .rounded(px(4.0))
                .bg(gpui::rgb(DANGER_BG))
                .text_color(gpui::rgb(DANGER))
                .child(error)
        }))
}

/// A text field in its box, outlined brighter while it has the focus.
fn field_box(input: &Entity<TextInput>, control: PickerControl, focus: PickerControl) -> Div {
    div()
        .debug_selector(move || control.selector(&[]))
        .flex()
        .px(px(6.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(if focus == control { TEXT } else { BORDER }))
        .child(input.clone())
}
