//! The Connections picker on screen: This PC, the saved tailnet and SSH
//! hosts (an SSH one tagged "SSH") and the owner's online tailnet peers, a
//! Forget button per saved host, and the Add a host fields, which Add SSH
//! host… swaps for the SSH form (Host, Remote command, Test connection, Add,
//! Back). Its rows and rules live in [`crate::connection_picker`]; a pick
//! goes to the network thread as [`NetCommand::SwitchTarget`], and the reset
//! for the new target runs when its state comes back.

use std::collections::HashSet;
use std::sync::Arc;

use futures::channel::oneshot;
use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, Focusable as _, FontWeight, Keystroke,
    Subscription, Task, Window, div, prelude::*, px,
};

use crate::RootView;
use crate::connection_picker::{
    ConnectionPicker, HostKind, PeerSource, Row, RowKind, SavedHost, SshTest, SshTester,
    TypedHostError, pairs_on_switch, typed_ssh_target, typed_target,
};
use crate::net::NetCommand;
use crate::notice_view::modal_panel;
use crate::notices::ToastKind;
use crate::palette::{BORDER, DANGER, DANGER_BG, MUTED, TEXT};
use crate::session_menu::{backdrop, dialog_button};
use crate::ssh::DEFAULT_REMOTE_COMMAND;
use crate::target::{DEFAULT_TAILNET_PORT, SshHost, Target};
use crate::text_input::{TextChanged, TextInput, TextInputEvent};

const TITLE: &str = "Connections";
const NAME_PLACEHOLDER: &str = "box.tail1234.ts.net";
const SSH_HOST_PLACEHOLDER: &str = "devbox or user@host";
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
    /// Opens the Add SSH host form.
    AddSsh,
    SshHost,
    SshCommand,
    /// Test connection, "Cancel" while a test runs.
    SshTest,
    SshAdd,
    SshBack,
    Close,
}

impl PickerControl {
    /// The control's debug selector; a row's carries its key
    /// ([`Row::key`]), so an SSH and a tailnet host of one name differ.
    fn selector(self, rows: &[Row]) -> String {
        let key = |index: usize| rows.get(index).map_or_else(String::new, Row::key);
        match self {
            Self::Row(index) => format!("connections-row-{}", key(index)),
            Self::Forget(index) => format!("connections-forget-{}", key(index)),
            Self::Name => "connections-name".to_owned(),
            Self::Port => "connections-port".to_owned(),
            Self::Connect => "connections-connect".to_owned(),
            Self::AddSsh => "connections-add-ssh".to_owned(),
            Self::SshHost => "connections-ssh-form-host".to_owned(),
            Self::SshCommand => "connections-ssh-form-command".to_owned(),
            Self::SshTest => "connections-ssh-form-test".to_owned(),
            Self::SshAdd => "connections-ssh-form-add".to_owned(),
            Self::SshBack => "connections-ssh-form-back".to_owned(),
            Self::Close => "connections-close".to_owned(),
        }
    }
}

/// Every control of a picker showing `rows`, in Tab order: each row then
/// its Forget button, then the Add a host fields, Connect and Add SSH
/// host…, or, while the SSH form shows, its two fields, Test connection,
/// Add and Back; Close last.
fn control_ring(rows: &[Row], adding_ssh: bool) -> Vec<PickerControl> {
    let mut ring = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        ring.push(PickerControl::Row(index));
        if row.kind == RowKind::Saved {
            ring.push(PickerControl::Forget(index));
        }
    }
    if adding_ssh {
        ring.extend([
            PickerControl::SshHost,
            PickerControl::SshCommand,
            PickerControl::SshTest,
            PickerControl::SshAdd,
            PickerControl::SshBack,
        ]);
    } else {
        ring.extend([
            PickerControl::Name,
            PickerControl::Port,
            PickerControl::Connect,
            PickerControl::AddSsh,
        ]);
    }
    ring.push(PickerControl::Close);
    ring
}

/// The open picker: its model, the Add a host fields or the Add SSH host
/// form, the focus, the typed host's refusal, the saved tailnet hosts this
/// PC holds no device token for and the Tailscale scan in flight.
pub(crate) struct ConnectionPickerDialog {
    picker: ConnectionPicker,
    /// The saved tailnet hosts with no device token, read once as the
    /// picker opened.
    not_paired: HashSet<String>,
    name_input: Entity<TextInput>,
    port_input: Entity<TextInput>,
    /// The Add SSH host form, in place of the Add a host fields.
    ssh: Option<SshForm>,
    focus: PickerControl,
    error: Option<String>,
    _scan: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl ConnectionPickerDialog {
    /// Whether `row` is tagged "Not paired": a saved tailnet host with no
    /// device token.
    fn not_paired(&self, row: &Row) -> bool {
        row.kind == RowKind::Saved
            && matches!(row.target, Target::Tailnet(_))
            && self.not_paired.contains(&row.label)
    }
}

/// The Add SSH host form: its fields and what Test connection last did.
struct SshForm {
    host_input: Entity<TextInput>,
    command_input: Entity<TextInput>,
    test: SshTest,
    /// The test in flight; dropping it tells the test to kill its `ssh`.
    running: Option<RunningTest>,
    _subscriptions: Vec<Subscription>,
}

/// A Test connection in flight: the signal that cancels it (dropping the
/// sender cancels it too) and the task waiting for its answer.
struct RunningTest {
    cancel: oneshot::Sender<()>,
    task: Task<()>,
}

impl RunningTest {
    /// Tells the test to stop, killing its `ssh`, and drops its answer.
    fn cancel(self) {
        if self.cancel.send(()).is_err() {
            tracing::debug!("the SSH test had already ended when it was cancelled");
        }
        drop(self.task);
    }
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
            .filter(|row| dialog.not_paired(row))
            .map(|row| row.label)
            .collect()
    }

    /// The saved tailnet hosts this PC holds no device token for. A host
    /// whose token cannot be read is left untagged; an SSH host has none.
    fn hosts_not_paired(&self, saved: &[SavedHost]) -> HashSet<String> {
        saved
            .iter()
            .filter(|host| host.kind == HostKind::Tailnet)
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

    /// Replaces the `ssh` Test connection runs.
    pub fn set_ssh_tester(&mut self, tester: Arc<dyn SshTester + Send + Sync>) {
        self.ssh_tester = tester;
    }

    /// Whether the picker shows the Add SSH host form.
    #[must_use]
    pub fn connection_picker_adding_ssh(&self) -> bool {
        self.connection_picker
            .as_ref()
            .is_some_and(|dialog| dialog.ssh.is_some())
    }

    /// The Add SSH host form's Test connection line.
    #[must_use]
    pub fn connection_picker_ssh_result(&self) -> Option<String> {
        self.connection_picker.as_ref()?.ssh.as_ref()?.test.text()
    }

    /// The labels of the rows that are SSH hosts, in row order.
    #[must_use]
    pub fn connection_picker_ssh_rows(&self) -> Vec<String> {
        let Some(dialog) = &self.connection_picker else {
            return Vec::new();
        };
        dialog
            .picker
            .rows()
            .into_iter()
            .filter(Row::is_ssh)
            .map(|row| row.label)
            .collect()
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
            ssh: None,
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

    /// Enter connects the typed host (in the SSH form, runs Test
    /// connection), Esc steps back, an edit clears the refusal (and an SSH
    /// field's test result, cancelling a test in flight), and a click in
    /// the field moves the focus there.
    fn watch_picker_field(
        input: &Entity<TextInput>,
        control: PickerControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        let ssh_field = matches!(control, PickerControl::SshHost | PickerControl::SshCommand);
        let keys = cx.subscribe_in(
            input,
            window,
            move |this, _, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit if ssh_field => this.submit_ssh_field(window, cx),
                TextInputEvent::Submit => this.connect_typed_host(window, cx),
                TextInputEvent::Cancel => this.connection_picker_escape(window, cx),
            },
        );
        let edits = cx.subscribe_in(input, window, move |this, _, _: &TextChanged, _, cx| {
            if let Some(dialog) = &mut this.connection_picker {
                dialog.error = None;
            }
            if ssh_field {
                this.reset_ssh_test();
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
                self.connection_picker_escape(window, cx);
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
        let ring = control_ring(&dialog.picker.rows(), dialog.ssh.is_some());
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
            PickerControl::AddSsh => self.open_ssh_form(window, cx),
            PickerControl::SshTest => self.press_ssh_test(window, cx),
            PickerControl::SshAdd => self.add_ssh_host(window, cx),
            PickerControl::SshBack => self.leave_ssh_form(window, cx),
            PickerControl::Close => self.close_connection_picker(window, cx),
            PickerControl::Name
            | PickerControl::Port
            | PickerControl::SshHost
            | PickerControl::SshCommand => {
                self.apply_connection_picker_focus(window, cx);
            }
        }
    }

    /// Esc: the SSH form steps back to the list; else the picker closes.
    fn connection_picker_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.connection_picker_adding_ssh() {
            self.leave_ssh_form(window, cx);
        } else {
            self.close_connection_picker(window, cx);
        }
    }

    /// Add SSH host…: the SSH form in place of the Add a host fields, its
    /// Host field focused.
    fn open_ssh_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.connection_picker.is_none() {
            return;
        }
        let host_input = cx.new(|cx| TextInput::new(String::new(), SSH_HOST_PLACEHOLDER, cx));
        let command_input = cx.new(|cx| TextInput::new(String::new(), DEFAULT_REMOTE_COMMAND, cx));
        let mut subscriptions =
            Self::watch_picker_field(&host_input, PickerControl::SshHost, window, cx);
        subscriptions.extend(Self::watch_picker_field(
            &command_input,
            PickerControl::SshCommand,
            window,
            cx,
        ));
        if let Some(dialog) = &mut self.connection_picker {
            dialog.ssh = Some(SshForm {
                host_input,
                command_input,
                test: SshTest::Idle,
                running: None,
                _subscriptions: subscriptions,
            });
            dialog.error = None;
            dialog.focus = PickerControl::SshHost;
        }
        self.apply_connection_picker_focus(window, cx);
    }

    /// Back: the list again, a test in flight cancelled and the typed host
    /// dropped.
    fn leave_ssh_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.connection_picker else {
            return;
        };
        if let Some(run) = dialog.ssh.take().and_then(|form| form.running) {
            run.cancel();
        }
        dialog.error = None;
        dialog.focus = PickerControl::AddSsh;
        self.apply_connection_picker_focus(window, cx);
    }

    fn ssh_form_mut(&mut self) -> Option<&mut SshForm> {
        self.connection_picker.as_mut()?.ssh.as_mut()
    }

    /// The SSH form's host and remote command as a target, or why the
    /// host is refused; `None` while the form is closed.
    fn typed_ssh(&self, cx: &Context<Self>) -> Option<Result<Target, TypedHostError>> {
        let form = self.connection_picker.as_ref()?.ssh.as_ref()?;
        Some(typed_ssh_target(
            form.host_input.read(cx).text(),
            form.command_input.read(cx).text(),
        ))
    }

    /// Shows `refusal` under the fields.
    fn refuse_typed_host(&mut self, refusal: TypedHostError, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.connection_picker {
            dialog.error = Some(refusal.to_string());
        }
        cx.notify();
    }

    /// Enter in an SSH field: Test connection, unless one is running.
    fn submit_ssh_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let testing = self
            .ssh_form_mut()
            .is_some_and(|form| form.test == SshTest::Testing);
        if !testing {
            self.start_ssh_test(window, cx);
        }
    }

    /// Test connection, or Cancel while a test runs.
    fn press_ssh_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let testing = self
            .ssh_form_mut()
            .is_some_and(|form| form.test == SshTest::Testing);
        if testing {
            self.reset_ssh_test();
            cx.notify();
        } else {
            self.start_ssh_test(window, cx);
        }
    }

    /// Cancels a test in flight and clears the last result.
    fn reset_ssh_test(&mut self) {
        let Some(form) = self.ssh_form_mut() else {
            return;
        };
        if let Some(run) = form.running.take() {
            run.cancel();
        }
        form.test = SshTest::Idle;
    }

    /// Runs Test connection on the typed host through the picker's
    /// [`SshTester`]; a refused host shows why and runs nothing.
    fn start_ssh_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = match self.typed_ssh(cx) {
            None => return,
            Some(Err(refusal)) => return self.refuse_typed_host(refusal, cx),
            Some(Ok(target)) => target,
        };
        let Some(host) = target.ssh_host().cloned() else {
            return;
        };
        let (cancel, cancelled) = oneshot::channel();
        let answer = self.ssh_tester.test(host.clone(), cancelled);
        let task = cx.spawn_in(window, async move |this, cx| {
            let outcome = answer.await;
            // Fails only when the view is gone, leaving nowhere to show it.
            this.update_in(cx, |root, _, cx| root.finish_ssh_test(&host, outcome, cx))
                .ok();
        });
        if let Some(form) = self.ssh_form_mut() {
            if let Some(run) = form.running.replace(RunningTest { cancel, task }) {
                run.cancel();
            }
            form.test = SshTest::Testing;
        }
        cx.notify();
    }

    /// A test's answer reaches the form that is still waiting for it.
    fn finish_ssh_test(
        &mut self,
        host: &SshHost,
        outcome: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.ssh_form_mut() else {
            return;
        };
        let Some(run) = form.running.take() else {
            return;
        };
        run.task.detach();
        form.test = SshTest::finished(host, outcome);
        cx.notify();
    }

    /// Add: saves the typed SSH host and switches to it, tested or not.
    fn add_ssh_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.typed_ssh(cx) {
            None => {}
            Some(Err(refusal)) => self.refuse_typed_host(refusal, cx),
            Some(Ok(target)) => self.switch_to_target(target, window, cx),
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
    /// switch and saves it as the boot target, a remote host also among the
    /// saved hosts; only a tailnet host waits for a "not approved" to open
    /// the pairing dialog. The current host, while it does not know this
    /// PC, opens the pairing dialog instead.
    fn switch_to_target(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        let current = target == *self.conn.target();
        let pair_current = current && self.device_pairing_offered();
        if !current {
            let host = SavedHost::from_target(&target);
            if let (Some(host), Some(dialog)) = (&host, &mut self.connection_picker) {
                dialog.picker.remember(host.clone());
                self.sidebar.set_saved_hosts(dialog.picker.saved().to_vec());
            }
            if pairs_on_switch(&target) {
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

    /// Forget on the saved host at row `index`, its session count and a
    /// tailnet host's device token deleted with it; forgetting the current
    /// host switches to this PC. The picker stays open.
    fn forget_connection_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.conn.target().clone();
        let Some(dialog) = &mut self.connection_picker else {
            return;
        };
        let Some(row) = dialog.picker.rows().into_iter().nth(index) else {
            return;
        };
        let was_current = dialog.picker.forget(&row.target, &current);
        if let Some(key) = row.target.session_count_key() {
            self.sidebar.forget_host_session_count(&key);
        }
        let rows = dialog.picker.rows().len();
        dialog.focus = PickerControl::Row(index.min(rows.saturating_sub(1)));
        self.sidebar.set_saved_hosts(dialog.picker.saved().to_vec());
        if was_current {
            self.sidebar.set_last_target(None);
            self.command(NetCommand::SwitchTarget(Target::Local));
        }
        self.save_ui();
        if let Target::Tailnet(host) = &row.target {
            self.forget_device_token(host.name(), cx);
        }
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
        let ssh = dialog.ssh.as_ref();
        let input = match dialog.focus {
            PickerControl::Name => Some(&dialog.name_input),
            PickerControl::Port => Some(&dialog.port_input),
            PickerControl::SshHost => ssh.map(|form| &form.host_input),
            PickerControl::SshCommand => ssh.map(|form| &form.command_input),
            _ => None,
        };
        match input {
            Some(input) => input.read(cx).focus_handle(cx).focus(window),
            None => self.connection_picker_focus.focus(window),
        }
        cx.notify();
    }

    /// Whether one of the picker's text fields holds the keyboard.
    fn connection_field_focused(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.connection_picker.as_ref().is_some_and(|dialog| {
            let ssh = dialog
                .ssh
                .iter()
                .flat_map(|form| [&form.host_input, &form.command_input]);
            [&dialog.name_input, &dialog.port_input]
                .into_iter()
                .chain(ssh)
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
                    not_paired: dialog.not_paired(row),
                    ssh: row.is_ssh(),
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
        let lower = match &dialog.ssh {
            Some(form) => ssh_form(dialog, form, focus, &rows, cx),
            None => add_host(dialog, focus, &rows, cx),
        };
        let panel = modal_panel("connections-panel")
            .debug_selector(|| "connections-panel".to_owned())
            .track_focus(&self.connection_picker_focus)
            .child(div().font_weight(FontWeight::SEMIBOLD).child(TITLE))
            .child(list)
            .children(scan)
            .child(lower)
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
    /// A saved tailnet host this PC holds no device token for.
    not_paired: bool,
    /// An SSH host.
    ssh: bool,
}

/// One row: its button, a muted "SSH" tag on an SSH host, a muted "Not
/// paired" tag on a saved tailnet host this PC holds no device token for, a
/// muted "Current" tag on the current target, and a saved host's Forget
/// button.
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
    let ssh_selector = format!("connections-ssh-tag-{}", row.label);
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(pick)
        .when(tags.ssh, |line| {
            line.child(
                div()
                    .debug_selector(|| ssh_selector)
                    .text_color(gpui::rgb(MUTED))
                    .child("SSH"),
            )
        })
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

/// The Add SSH host form: the Host and Remote command fields, a host's
/// refusal, Test connection's line, and Test connection (Cancel while it
/// runs), Add and Back.
fn ssh_form(
    dialog: &ConnectionPickerDialog,
    form: &SshForm,
    focus: PickerControl,
    rows: &[Row],
    cx: &mut Context<RootView>,
) -> Div {
    let result = form.test.text().map(|text| {
        let line = div().debug_selector(|| "connections-ssh-form-result".to_owned());
        match form.test {
            SshTest::Failed(_) => error_box(line, text),
            SshTest::Passed(_) => line.text_color(gpui::rgb(TEXT)).child(text),
            SshTest::Idle | SshTest::Testing => line.text_color(gpui::rgb(MUTED)).child(text),
        }
    });
    let buttons = div()
        .flex()
        .justify_end()
        .gap(px(6.0))
        .child(picker_button(
            PickerControl::SshTest,
            form.test.button_label(),
            focus,
            rows,
            cx,
        ))
        .child(picker_button(PickerControl::SshAdd, "Add", focus, rows, cx))
        .child(picker_button(
            PickerControl::SshBack,
            "Back",
            focus,
            rows,
            cx,
        ));
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(div().text_color(gpui::rgb(MUTED)).child("Add an SSH host"))
        .child(labelled_field(
            "Host",
            &form.host_input,
            PickerControl::SshHost,
            focus,
        ))
        .child(labelled_field(
            "Remote command",
            &form.command_input,
            PickerControl::SshCommand,
            focus,
        ))
        .children(dialog.error.clone().map(|error| {
            error_box(
                div().debug_selector(|| "connections-error".to_owned()),
                error,
            )
        }))
        .children(result)
        .child(buttons)
}

/// A field of the SSH form with its muted label before it.
fn labelled_field(
    label: &'static str,
    input: &Entity<TextInput>,
    control: PickerControl,
    focus: PickerControl,
) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(div().w(px(120.0)).text_color(gpui::rgb(MUTED)).child(label))
        .child(field_box(input, control, focus).flex_1())
}

/// `line` styled as the picker's error box, showing `text`.
fn error_box(line: Div, text: String) -> Div {
    line.px(px(6.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .bg(gpui::rgb(DANGER_BG))
        .text_color(gpui::rgb(DANGER))
        .child(text)
}

/// Add a host: the Name and Port fields, Connect, and the refusal under
/// them, with Add SSH host… beside the title.
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
    let title = div()
        .flex()
        .items_center()
        .justify_between()
        .child(div().text_color(gpui::rgb(MUTED)).child("Add a host"))
        .child(picker_button(
            PickerControl::AddSsh,
            "Add SSH host…",
            focus,
            rows,
            cx,
        ));
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(title)
        .child(fields)
        .children(dialog.error.clone().map(|error| {
            error_box(
                div().debug_selector(|| "connections-error".to_owned()),
                error,
            )
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
