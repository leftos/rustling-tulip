//! The Connections picker on screen: the rows under their This PC, Saved
//! hosts and On your tailnet headers, each row a two-line choice with its
//! state chip and, on a saved host, a Forget button shown on hover or focus;
//! below them `+ Add host`, which opens the inline add form with its Tailnet
//! | SSH switch (Name and Port; or Host, Remote command and Test
//! connection). Its rows and rules live in [`crate::connection_picker`]; a
//! pick goes to the network thread as [`NetCommand::SwitchTarget`], and the
//! reset for the new target runs when its state comes back.

use std::collections::HashSet;
use std::sync::Arc;

use futures::channel::oneshot;
use gpui::{
    AnyElement, App, ClickEvent, Context, Div, ElementId, Entity, Focusable as _, FontWeight,
    Keystroke, SharedString, Stateful, Subscription, Task, Window, div, prelude::*, px, relative,
    svg,
};

use crate::RootView;
use crate::assets::{CHECK_ICON, HOST_PC_ICON, HOST_SSH_ICON, HOST_TAILNET_ICON, TAB_CLOSE_ICON};
use crate::buttons::{ButtonKind, ButtonSize, button, field_frame, focus_ring};
use crate::connection_picker::{
    AddButton, AddField, AddMode, ConnectionPicker, HostKind, LiveState, PeerSource, PickerSection,
    Row, RowDetail, RowFacts, RowHost, RowKind, RowState, SCAN_PENDING, SavedHost, Section,
    SshTest, SshTester, TypedHostError, pairs_on_switch, row_detail, typed_ssh_target,
    typed_target,
};
use crate::fonts::DEFAULT_FAMILY;
use crate::net::NetCommand;
use crate::notice_view::modal_panel;
use crate::notices::ToastKind;
use crate::palette::{
    ASK_BORDER, ASK_WASH, ASKING, CARD_EDGE, DANGER, DANGER_BG, FAINT, HOVER, LINE, MODAL_INSET,
    OK_WASH, RAISED, SELECTION, STATUS_IDLE, STATUS_OK, SUBTLE, TEXT, TEXT_2,
};
use crate::session_menu::backdrop;
use crate::ssh::DEFAULT_REMOTE_COMMAND;
use crate::target::{DEFAULT_TAILNET_PORT, SshHost, TailnetHost, Target};
use crate::text_input::{TextChanged, TextInput, TextInputEvent};

const TITLE: &str = "Connections";
const NAME_PLACEHOLDER: &str = "box.tail1234.ts.net";
const SSH_HOST_PLACEHOLDER: &str = "devbox or user@host";
/// The room the panel leaves above and below it, so it never reaches the
/// window's edge.
const PANEL_MARGIN: f32 = 24.0;
/// The width of the add form's field labels.
const FIELD_LABEL_WIDTH: f32 = 104.0;
/// A row's corner radius.
const ROW_RADIUS: f32 = 8.0;

/// A control of the picker, in the order Tab walks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerControl {
    /// The row at this index of [`ConnectionPicker::rows`].
    Row(usize),
    /// The Forget button of the saved host at this row index.
    Forget(usize),
    /// `+ Add host`: opens the add form.
    AddHost,
    ModeTailnet,
    ModeSsh,
    Name,
    Port,
    SshHost,
    SshCommand,
    /// The add form's Cancel: closes the form.
    AddCancel,
    /// Test connection, "Cancel" while a test runs.
    SshTest,
    /// Add and connect, in either mode.
    AddSubmit,
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
            Self::AddHost => "connections-add-host".to_owned(),
            Self::ModeTailnet => "connections-add-mode-tailnet".to_owned(),
            Self::ModeSsh => "connections-add-mode-ssh".to_owned(),
            Self::Name => "connections-name".to_owned(),
            Self::Port => "connections-port".to_owned(),
            Self::SshHost => "connections-ssh-form-host".to_owned(),
            Self::SshCommand => "connections-ssh-form-command".to_owned(),
            Self::AddCancel => "connections-add-cancel".to_owned(),
            Self::SshTest => "connections-ssh-form-test".to_owned(),
            Self::AddSubmit => "connections-add-submit".to_owned(),
            Self::Close => "connections-close".to_owned(),
        }
    }

    /// The control an add form button is.
    const fn of_button(button: AddButton) -> Self {
        match button {
            AddButton::Cancel => Self::AddCancel,
            AddButton::Test => Self::SshTest,
            AddButton::Submit => Self::AddSubmit,
        }
    }

    /// The segment that picks `mode`.
    const fn of_mode(mode: AddMode) -> Self {
        match mode {
            AddMode::Tailnet => Self::ModeTailnet,
            AddMode::Ssh => Self::ModeSsh,
        }
    }

    /// The two fields `mode` shows, top to bottom.
    const fn fields(mode: AddMode) -> [Self; 2] {
        match mode {
            AddMode::Tailnet => [Self::Name, Self::Port],
            AddMode::Ssh => [Self::SshHost, Self::SshCommand],
        }
    }
}

/// Every control of a picker showing `rows`, in Tab order: each row then
/// its Forget button; then `+ Add host`, or, while the add form shows in
/// `add`'s mode, its two segments, that mode's fields and its buttons;
/// Close last.
fn control_ring(rows: &[Row], add: Option<AddMode>) -> Vec<PickerControl> {
    let mut ring = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        ring.push(PickerControl::Row(index));
        if row.kind == RowKind::Saved {
            ring.push(PickerControl::Forget(index));
        }
    }
    match add {
        None => ring.push(PickerControl::AddHost),
        Some(mode) => {
            ring.extend([PickerControl::ModeTailnet, PickerControl::ModeSsh]);
            ring.extend(PickerControl::fields(mode));
            ring.extend(mode.buttons().into_iter().map(PickerControl::of_button));
        }
    }
    ring.push(PickerControl::Close);
    ring
}

/// The open picker: its model, the add form while it shows, the focus, the
/// typed host's refusal, the saved tailnet hosts this PC holds no device
/// token for and the Tailscale scan in flight.
pub(crate) struct ConnectionPickerDialog {
    picker: ConnectionPicker,
    /// The saved tailnet hosts with no device token, read once as the
    /// picker opened.
    not_paired: HashSet<String>,
    /// The add form, under the list while it shows.
    add: Option<AddForm>,
    focus: PickerControl,
    error: Option<String>,
    _scan: Task<()>,
}

/// The add form: its mode, both modes' fields (each keeps its text across a
/// switch) and what Test connection last did.
struct AddForm {
    mode: AddMode,
    name_input: Entity<TextInput>,
    port_input: Entity<TextInput>,
    host_input: Entity<TextInput>,
    command_input: Entity<TextInput>,
    test: SshTest,
    /// The test in flight; dropping it tells the test to kill its `ssh`.
    running: Option<RunningTest>,
    _subscriptions: Vec<Subscription>,
}

impl AddForm {
    /// The field `control` names, if it is one.
    fn input(&self, control: PickerControl) -> Option<&Entity<TextInput>> {
        match control {
            PickerControl::Name => Some(&self.name_input),
            PickerControl::Port => Some(&self.port_input),
            PickerControl::SshHost => Some(&self.host_input),
            PickerControl::SshCommand => Some(&self.command_input),
            _ => None,
        }
    }

    /// Every field of both modes.
    fn inputs(&self) -> [&Entity<TextInput>; 4] {
        [
            &self.name_input,
            &self.port_input,
            &self.host_input,
            &self.command_input,
        ]
    }

    /// Cancels a test in flight and clears the last result.
    fn reset_test(&mut self) {
        if let Some(run) = self.running.take() {
            run.cancel();
        }
        self.test = SshTest::Idle;
    }
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

    /// The rows whose chip reads "Not paired": saved tailnet hosts this PC
    /// holds no device token for, in row order.
    #[must_use]
    pub fn connection_picker_not_paired(&self) -> Vec<String> {
        let Some(dialog) = &self.connection_picker else {
            return Vec::new();
        };
        dialog
            .picker
            .rows()
            .into_iter()
            .filter(|row| {
                row_detail(&self.row_facts(dialog, row)).state == Some(RowState::NotPaired)
            })
            .map(|row| row.label)
            .collect()
    }

    /// The second line and state chip of the row whose key ([`Row::key`])
    /// is `key`.
    #[must_use]
    pub fn connection_picker_row_detail(&self, key: &str) -> Option<RowDetail> {
        let dialog = self.connection_picker.as_ref()?;
        let row = dialog
            .picker
            .rows()
            .into_iter()
            .find(|row| row.key() == key)?;
        Some(row_detail(&self.row_facts(dialog, &row)))
    }

    /// Whether the keyboard focus shows the Forget button of the saved row
    /// keyed `key` in place of its state chip (hovering the row shows it
    /// as well, which this does not report).
    #[must_use]
    pub fn connection_picker_forget_shown(&self, key: &str) -> bool {
        self.connection_picker.as_ref().is_some_and(|dialog| {
            dialog
                .picker
                .rows()
                .iter()
                .position(|row| row.key() == key && row.kind == RowKind::Saved)
                .is_some_and(|index| forget_focused(dialog.focus, index))
        })
    }

    /// What the picker knows about `row` for its second line and chip.
    fn row_facts(&self, dialog: &ConnectionPickerDialog, row: &Row) -> RowFacts {
        let current = row.target == *self.conn.target();
        let host = match (row.kind, &row.target) {
            (RowKind::Local, _) => RowHost::ThisPc,
            (RowKind::Peer, _) => RowHost::Peer,
            (RowKind::Saved, Target::Ssh(_)) => RowHost::Ssh,
            (RowKind::Saved, _) => RowHost::Tailnet,
        };
        let sessions_last_time = row
            .target
            .session_count_key()
            .and_then(|key| self.sidebar.host_session_count(&key))
            .and_then(|count| usize::try_from(count).ok());
        RowFacts {
            host,
            port: row.target.tailnet_host().map(TailnetHost::port),
            remote_command: row
                .target
                .ssh_host()
                .and_then(SshHost::remote_command)
                .map(str::to_owned),
            current,
            live: if current {
                LiveState::from_state(self.conn.state())
            } else {
                None
            },
            has_token: !dialog.not_paired.contains(&row.label),
            sessions_last_time,
            live_sessions: current.then(|| self.sidebar.sessions().len()),
        }
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

    /// The typed host's refusal in the add form.
    #[must_use]
    pub fn connection_picker_error(&self) -> Option<String> {
        self.connection_picker.as_ref()?.error.clone()
    }

    /// The "On your tailnet" header's wait or error, while the scan has not
    /// answered or has failed.
    #[must_use]
    pub fn connection_picker_scan_text(&self) -> Option<String> {
        let picker = &self.connection_picker.as_ref()?.picker;
        if picker.scan_pending() {
            return Some(SCAN_PENDING.to_owned());
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

    /// The add form's mode while it shows.
    #[must_use]
    pub fn connection_picker_add_mode(&self) -> Option<AddMode> {
        Some(self.connection_picker.as_ref()?.add.as_ref()?.mode)
    }

    /// Whether the add form shows its SSH mode.
    #[must_use]
    pub fn connection_picker_adding_ssh(&self) -> bool {
        self.connection_picker_add_mode() == Some(AddMode::Ssh)
    }

    /// The add form's Test connection line.
    #[must_use]
    pub fn connection_picker_ssh_result(&self) -> Option<String> {
        self.connection_picker.as_ref()?.add.as_ref()?.test.text()
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
        let scan = self.start_connection_scan(window, cx);
        self.close_flyout();
        self.connection_picker = Some(ConnectionPickerDialog {
            picker,
            not_paired,
            add: None,
            focus: PickerControl::Row(current),
            error: None,
            _scan: scan,
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

    /// Enter adds and connects the typed tailnet host (in an SSH field, runs
    /// Test connection), Esc closes the form, an edit clears the refusal
    /// (and an SSH field's test result, cancelling a test in flight), and a
    /// click in the field moves the focus there.
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
                TextInputEvent::Submit => this.add_submit(window, cx),
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
        let mode = dialog.add.as_ref().map(|form| form.mode);
        let ring = control_ring(&dialog.picker.rows(), mode);
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
            PickerControl::AddHost => self.open_add_form(window, cx),
            PickerControl::ModeTailnet => self.set_add_mode(AddMode::Tailnet, window, cx),
            PickerControl::ModeSsh => self.set_add_mode(AddMode::Ssh, window, cx),
            PickerControl::AddCancel => self.close_add_form(window, cx),
            PickerControl::SshTest => self.press_ssh_test(window, cx),
            PickerControl::AddSubmit => self.add_submit(window, cx),
            PickerControl::Close => self.close_connection_picker(window, cx),
            PickerControl::Name
            | PickerControl::Port
            | PickerControl::SshHost
            | PickerControl::SshCommand => {
                self.apply_connection_picker_focus(window, cx);
            }
        }
    }

    /// Esc: the add form closes; else the picker closes.
    fn connection_picker_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form_open = self
            .connection_picker
            .as_ref()
            .is_some_and(|dialog| dialog.add.is_some());
        if form_open {
            self.close_add_form(window, cx);
        } else {
            self.close_connection_picker(window, cx);
        }
    }

    /// `+ Add host`: the add form under the list, on Tailnet, its Name field
    /// focused.
    fn open_add_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.connection_picker.is_none() {
            return;
        }
        let name_input = cx.new(|cx| TextInput::new(String::new(), NAME_PLACEHOLDER, cx));
        let port_input = cx.new(|cx| TextInput::new(DEFAULT_TAILNET_PORT.to_string(), "", cx));
        let host_input = cx.new(|cx| TextInput::new(String::new(), SSH_HOST_PLACEHOLDER, cx));
        let command_input = cx.new(|cx| TextInput::new(String::new(), DEFAULT_REMOTE_COMMAND, cx));
        let mut subscriptions = Vec::new();
        for (input, control) in [
            (&name_input, PickerControl::Name),
            (&port_input, PickerControl::Port),
            (&host_input, PickerControl::SshHost),
            (&command_input, PickerControl::SshCommand),
        ] {
            subscriptions.extend(Self::watch_picker_field(input, control, window, cx));
        }
        if let Some(dialog) = &mut self.connection_picker {
            dialog.add = Some(AddForm {
                mode: AddMode::Tailnet,
                name_input,
                port_input,
                host_input,
                command_input,
                test: SshTest::Idle,
                running: None,
                _subscriptions: subscriptions,
            });
            dialog.error = None;
            dialog.focus = PickerControl::Name;
        }
        self.apply_connection_picker_focus(window, cx);
    }

    /// Cancel: the list alone again, a test in flight cancelled and the
    /// typed hosts dropped; the focus goes back to `+ Add host`.
    fn close_add_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.connection_picker else {
            return;
        };
        if let Some(mut form) = dialog.add.take() {
            form.reset_test();
        }
        dialog.error = None;
        dialog.focus = PickerControl::AddHost;
        self.apply_connection_picker_focus(window, cx);
    }

    /// A segment: the form shows `mode`'s fields as they were left, a test
    /// running in the SSH mode is cancelled when it leaves, and the focus
    /// goes to the mode's first field.
    fn set_add_mode(&mut self, mode: AddMode, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.connection_picker else {
            return;
        };
        let Some(form) = &mut dialog.add else {
            return;
        };
        if form.mode.switch_cancels_test(mode, &form.test) {
            form.reset_test();
        }
        form.mode = mode;
        dialog.error = None;
        dialog.focus = PickerControl::fields(mode)[0];
        self.apply_connection_picker_focus(window, cx);
    }

    fn add_form_mut(&mut self) -> Option<&mut AddForm> {
        self.connection_picker.as_mut()?.add.as_mut()
    }

    /// The SSH fields' host and remote command as a target, or why the
    /// host is refused; `None` while the form is closed.
    fn typed_ssh(&self, cx: &Context<Self>) -> Option<Result<Target, TypedHostError>> {
        let form = self.connection_picker.as_ref()?.add.as_ref()?;
        Some(typed_ssh_target(
            form.host_input.read(cx).text(),
            form.command_input.read(cx).text(),
        ))
    }

    /// Shows `refusal` in the add form.
    fn refuse_typed_host(&mut self, refusal: TypedHostError, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.connection_picker {
            dialog.error = Some(refusal.to_string());
        }
        cx.notify();
    }

    /// Enter in an SSH field: Test connection, unless one is running.
    fn submit_ssh_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let testing = self
            .add_form_mut()
            .is_some_and(|form| form.test == SshTest::Testing);
        if !testing {
            self.start_ssh_test(window, cx);
        }
    }

    /// Test connection, or Cancel while a test runs.
    fn press_ssh_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let testing = self
            .add_form_mut()
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
        if let Some(form) = self.add_form_mut() {
            form.reset_test();
        }
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
        if let Some(form) = self.add_form_mut() {
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
        let Some(form) = self.add_form_mut() else {
            return;
        };
        let Some(run) = form.running.take() else {
            return;
        };
        run.task.detach();
        form.test = SshTest::finished(host, outcome);
        cx.notify();
    }

    /// Add and connect: saves the host the form's mode holds and switches
    /// to it (an SSH host tested or not), or shows why it is refused.
    fn add_submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.connection_picker.as_ref().and_then(|d| d.add.as_ref()) else {
            return;
        };
        let typed = match form.mode {
            AddMode::Tailnet => typed_target(
                form.name_input.read(cx).text(),
                form.port_input.read(cx).text(),
            ),
            AddMode::Ssh => typed_ssh_target(
                form.host_input.read(cx).text(),
                form.command_input.read(cx).text(),
            ),
        };
        match typed {
            Ok(target) => self.switch_to_target(target, window, cx),
            Err(refusal) => self.refuse_typed_host(refusal, cx),
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
        let input = dialog
            .add
            .as_ref()
            .and_then(|form| form.input(dialog.focus));
        match input {
            Some(input) => input.read(cx).focus_handle(cx).focus(window),
            None => self.connection_picker_focus.focus(window),
        }
        cx.notify();
    }

    /// Whether one of the picker's text fields holds the keyboard.
    fn connection_field_focused(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.connection_picker
            .as_ref()
            .and_then(|dialog| dialog.add.as_ref())
            .is_some_and(|form| {
                form.inputs()
                    .into_iter()
                    .any(|input| input.read(cx).focus_handle(cx).is_focused(window))
            })
    }

    /// The picker over a backdrop that takes every click beneath it, drawn
    /// above the first-connect overlay: the header, the sections' rows in a
    /// list that scrolls once the panel reaches the window's height less
    /// its margins, the add form while it shows, and the footer.
    pub(crate) fn connection_picker_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.connection_picker.as_ref()?;
        let rows = dialog.picker.rows();
        let draw = Draw {
            rows: &rows,
            focus: dialog.focus,
        };
        let scan_failed = dialog.picker.scan_error().is_some();
        let mut list = div()
            .id("connections-list")
            .debug_selector(|| "connections-list".to_owned())
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(8.0))
            .flex()
            .flex_col()
            .gap(px(2.0));
        for (n, section) in dialog.picker.sections().iter().enumerate() {
            list = list.child(section_header(section, n == 0, scan_failed));
            for &index in &section.rows {
                if let Some(row) = rows.get(index) {
                    let facts = self.row_facts(dialog, row);
                    list = list.child(host_row(index, row, &facts, &draw, cx));
                }
            }
        }
        let form = dialog
            .add
            .as_ref()
            .map(|form| add_form(form, dialog.error.as_deref(), &draw, cx));
        let footer = footer(dialog.add.is_some(), &draw, cx);
        let panel = modal_panel("connections-panel")
            .p_0()
            .gap_0()
            .max_h(relative(1.0))
            .debug_selector(|| "connections-panel".to_owned())
            .track_focus(&self.connection_picker_focus)
            .child(header(cx))
            .child(list)
            .children(form)
            .child(footer);
        let frame = div()
            .id("connections-frame")
            .size_full()
            .py(px(PANEL_MARGIN))
            .flex()
            .items_center()
            .justify_center()
            .child(panel);
        Some(backdrop("connections-picker", frame))
    }
}

/// What every part of the picker draws against: its rows and its focus.
struct Draw<'a> {
    rows: &'a [Row],
    focus: PickerControl,
}

/// A `kind` button of `size` for `control`, ringed while it has the focus,
/// that presses it on a click (and keeps the click from the row under it).
fn control_button(
    control: PickerControl,
    kind: ButtonKind,
    size: ButtonSize,
    draw: &Draw<'_>,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    button(&control.selector(draw.rows), kind, size, true)
        .relative()
        .when(draw.focus == control, |button| {
            button.child(focus_ring(size.radius()))
        })
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            cx.stop_propagation();
            this.press_connection_control(control, window, cx);
        }))
}

/// The title and the mouse-only close X.
fn header(cx: &mut Context<RootView>) -> Div {
    let close = div()
        .id("connections-close-x")
        .debug_selector(|| "connections-close-x".to_owned())
        .flex()
        .items_center()
        .justify_center()
        .size(px(26.0))
        .rounded(px(6.0))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER)))
        .child(
            svg()
                .path(TAB_CLOSE_ICON)
                .size(px(14.0))
                .text_color(gpui::rgb(TEXT_2)),
        )
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.close_connection_picker(window, cx);
        }));
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_between()
        .pt(px(12.0))
        .pr(px(12.0))
        .pb(px(8.0))
        .pl(px(16.0))
        .child(
            div()
                .text_size(px(14.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(TITLE),
        )
        .child(close)
}

/// The slug of `section` in its header's selector.
const fn section_slug(section: Section) -> &'static str {
    match section {
        Section::ThisPc => "this-pc",
        Section::Saved => "saved",
        Section::Tailnet => "tailnet",
    }
}

/// A section's header: its title and, at the right, its note; the tailnet
/// note is the scan's wait, its count, or its error in the danger colour.
fn section_header(section: &PickerSection, first: bool, scan_failed: bool) -> Div {
    let selector = format!("connections-section-{}", section_slug(section.section));
    let tailnet = section.section == Section::Tailnet;
    let note = section.note.clone().map(|note| {
        let color = if tailnet && scan_failed {
            DANGER
        } else {
            SUBTLE
        };
        div()
            .when(tailnet, |note| {
                note.debug_selector(|| "connections-scan".to_owned())
            })
            .font_weight(FontWeight::NORMAL)
            .text_size(px(11.0))
            .text_color(gpui::rgb(color))
            .child(note)
    });
    div()
        .debug_selector(move || selector)
        .flex()
        .items_baseline()
        .justify_between()
        .pt(px(if first { 2.0 } else { 10.0 }))
        .px(px(10.0))
        .pb(px(4.0))
        .text_size(px(11.5))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(gpui::rgb(TEXT_2))
        .child(section.section.title())
        .children(note)
}

/// One host row, clickable as a whole: its kind tile, its name over its
/// second line, its state chip, and a saved host's Forget, which shows on
/// hover or while the row or the button has the focus and takes a click
/// even while it does not show.
fn host_row(
    index: usize,
    row: &Row,
    facts: &RowFacts,
    draw: &Draw<'_>,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let control = PickerControl::Row(index);
    let selector = control.selector(draw.rows);
    let group = SharedString::from(format!("connections-row-group-{index}"));
    let detail = row_detail(facts);
    let saved = row.kind == RowKind::Saved;
    let forget_shown = forget_focused(draw.focus, index);
    let trail = div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .text_size(px(11.5))
        .text_color(gpui::rgb(TEXT_2))
        .children(detail.state.as_ref().map(|state| state_chip(row, state)))
        .when(saved && forget_shown, |trail| trail.opacity(0.0))
        .when(saved && !forget_shown, |trail| {
            trail.group_hover(group.clone(), |style| style.opacity(0.0))
        });
    let text = div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .child(name_line(row))
        .child(detail_line(
            &row.key(),
            facts.host,
            &detail.line,
            group.clone(),
        ));
    div()
        .id(ElementId::Name(SharedString::from(selector.clone())))
        .debug_selector(move || selector)
        .group(group.clone())
        .relative()
        .flex()
        .items_center()
        .gap(px(10.0))
        .min_h(px(46.0))
        .px(px(10.0))
        .py(px(7.0))
        .rounded(px(ROW_RADIUS))
        .cursor_pointer()
        .when(facts.current, |row| row.bg(gpui::rgb(SELECTION)))
        .hover(|style| style.bg(gpui::rgb(HOVER)))
        .child(kind_tile(facts.host))
        .child(text)
        .child(trail)
        .when(saved, |line| {
            line.child(forget_button(index, forget_shown, group, draw, cx))
        })
        .when(draw.focus == control, |row| {
            row.child(focus_ring(ROW_RADIUS))
        })
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.press_connection_control(control, window, cx);
        }))
}

/// Whether the keyboard focus shows the Forget button of the row at
/// `index` in place of its state chip: only while Forget itself has the
/// focus, so a focused row keeps its chip. The pointer over the row shows
/// it too.
const fn forget_focused(focus: PickerControl, index: usize) -> bool {
    matches!(focus, PickerControl::Forget(i) if i == index)
}

/// The Forget button of the saved host at row `index`, over the row's right
/// end: at rest it draws nothing yet keeps its place, so a click there still
/// forgets.
fn forget_button(
    index: usize,
    shown: bool,
    group: SharedString,
    draw: &Draw<'_>,
    cx: &mut Context<RootView>,
) -> Div {
    let button = control_button(
        PickerControl::Forget(index),
        ButtonKind::Danger,
        ButtonSize::Compact,
        draw,
        cx,
    )
    .child("Forget")
    .when(!shown, |button| {
        button
            .opacity(0.0)
            .group_hover(group, |style| style.opacity(1.0))
    });
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .right(px(10.0))
        .flex()
        .items_center()
        .child(button)
}

/// The 28 px tile with the row's kind of host.
fn kind_tile(host: RowHost) -> Div {
    let icon = match host {
        RowHost::ThisPc => HOST_PC_ICON,
        RowHost::Tailnet | RowHost::Peer => HOST_TAILNET_ICON,
        RowHost::Ssh => HOST_SSH_ICON,
    };
    div()
        .flex_none()
        .size(px(28.0))
        .rounded(px(7.0))
        .bg(gpui::rgb(RAISED))
        .border_1()
        .border_color(gpui::rgb(CARD_EDGE))
        .flex()
        .items_center()
        .justify_center()
        .child(
            svg()
                .path(icon)
                .size(px(16.0))
                .text_color(gpui::rgb(TEXT_2)),
        )
}

/// The row's name, a tailnet host's suffix greyed.
fn name_line(row: &Row) -> Div {
    let (name, suffix) = row.name_parts();
    div()
        .flex()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_size(px(12.5))
        .font_weight(FontWeight::MEDIUM)
        .line_height(relative(1.25))
        .child(name)
        .children(suffix.map(|suffix| {
            div()
                .font_weight(FontWeight::NORMAL)
                .text_color(gpui::rgb(FAINT))
                .child(suffix)
        }))
}

/// A piece of a row's second line as it is drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DetailPiece {
    Text(String),
    /// A peer's "Online", after a green dot.
    Online(String),
    /// A value in the mono face (a port, a remote command), after its lead.
    Mono {
        lead: Option<&'static str>,
        value: String,
    },
}

/// The pieces of `line` ([`RowDetail::line`]) for a row of `host`: a
/// tailnet port and an SSH remote command in the mono face, a peer's
/// "Online" after its dot.
fn detail_pieces(host: RowHost, line: &str) -> Vec<DetailPiece> {
    line.split(" · ")
        .enumerate()
        .map(|(index, part)| {
            let port = matches!(host, RowHost::Tailnet | RowHost::Peer)
                .then(|| part.strip_prefix("port "))
                .flatten();
            if let Some(port) = port {
                DetailPiece::Mono {
                    lead: Some("port"),
                    value: port.to_owned(),
                }
            } else if host == RowHost::Ssh && index == 1 {
                DetailPiece::Mono {
                    lead: None,
                    value: part.to_owned(),
                }
            } else if host == RowHost::Peer && index == 0 {
                DetailPiece::Online(part.to_owned())
            } else {
                DetailPiece::Text(part.to_owned())
            }
        })
        .collect()
}

/// A row's second line, its pieces split by faint dots, lifted from quiet to
/// secondary text while its row, the hover `group`, is hovered.
fn detail_line(key: &str, host: RowHost, line: &str, group: SharedString) -> Div {
    let selector = format!("connections-detail-{key}");
    let mut drawn = div()
        .debug_selector(move || selector)
        .flex()
        .items_center()
        .gap(px(6.0))
        .mt(px(2.0))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_size(px(11.0))
        .text_color(gpui::rgb(SUBTLE))
        .group_hover(group, |style| style.text_color(gpui::rgb(TEXT_2)));
    for (index, piece) in detail_pieces(host, line).into_iter().enumerate() {
        if index > 0 {
            drawn = drawn.child(div().text_color(gpui::rgb(FAINT)).child("·"));
        }
        drawn = drawn.child(detail_piece(piece));
    }
    drawn
}

fn detail_piece(piece: DetailPiece) -> Div {
    match piece {
        DetailPiece::Text(text) => div().child(text),
        DetailPiece::Online(text) => div()
            .flex()
            .items_center()
            .gap(px(5.0))
            .child(dot(6.0, STATUS_OK))
            .child(text),
        DetailPiece::Mono { lead, value } => div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .children(lead)
            .child(
                div()
                    .font_family(DEFAULT_FAMILY)
                    .text_size(px(10.5))
                    .child(value),
            ),
    }
}

/// A round dot `size` px across.
fn dot(size: f32, color: u32) -> Div {
    div()
        .flex_none()
        .size(px(size))
        .rounded_full()
        .bg(gpui::rgb(color))
}

/// The chip at a row's trail: a dot and the state for the current row, an
/// amber tag for one that needs pairing.
fn state_chip(row: &Row, state: &RowState) -> Div {
    let label = state.label();
    let selector = format!("connections-state-{}", row.key());
    match state {
        RowState::NotPaired => tag(format!("connections-not-paired-{}", row.label), label),
        RowState::NeedsPairing => tag(selector, label),
        RowState::Connected => dotted(selector, STATUS_OK, label),
        RowState::Disconnected { .. } => dotted(selector, STATUS_IDLE, label),
        RowState::Connecting => div().debug_selector(move || selector).child(label),
    }
}

/// `label` after a 7 px dot of `color`.
fn dotted(selector: String, color: u32, label: &'static str) -> Div {
    div()
        .debug_selector(move || selector)
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(dot(7.0, color))
        .child(label)
}

/// An amber tag reading `label`.
fn tag(selector: String, label: &'static str) -> Div {
    div()
        .debug_selector(move || selector)
        .flex()
        .items_center()
        .h(px(18.0))
        .px(px(6.0))
        .rounded(px(5.0))
        .text_size(px(10.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(gpui::rgb(ASKING))
        .bg(gpui::rgba(ASK_WASH))
        .border_1()
        .border_color(gpui::rgba(ASK_BORDER))
        .child(label)
}

/// The footer: `+ Add host` and Close, ruled off from the list; Close alone
/// while the add form shows.
fn footer(form_open: bool, draw: &Draw<'_>, cx: &mut Context<RootView>) -> Div {
    let close = control_button(
        PickerControl::Close,
        ButtonKind::Outlined,
        ButtonSize::Regular,
        draw,
        cx,
    )
    .child("Close");
    if form_open {
        return div()
            .flex_none()
            .flex()
            .justify_end()
            .pt(px(10.0))
            .px(px(14.0))
            .pb(px(14.0))
            .child(close);
    }
    let add = control_button(
        PickerControl::AddHost,
        ButtonKind::Outlined,
        ButtonSize::Regular,
        draw,
        cx,
    )
    .gap(px(6.0))
    .child(
        div()
            .text_size(px(15.0))
            .text_color(gpui::rgb(TEXT_2))
            .child("+"),
    )
    .child("Add host");
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_between()
        .mt(px(10.0))
        .pt(px(12.0))
        .px(px(14.0))
        .pb(px(14.0))
        .border_t_1()
        .border_color(gpui::rgb(LINE))
        .child(add)
        .child(close)
}

/// The add form: its title and Tailnet | SSH switch, the mode's two
/// fields, a host's refusal, the SSH mode's Test connection line, and the
/// mode's buttons.
fn add_form(
    form: &AddForm,
    error: Option<&str>,
    draw: &Draw<'_>,
    cx: &mut Context<RootView>,
) -> Div {
    let [first, second] = form.mode.fields();
    let [first_control, second_control] = PickerControl::fields(form.mode);
    let field = |field: AddField, control: PickerControl| {
        form.input(control)
            .map(|input| field_row(field, input, control, draw.focus))
    };
    let result = (form.mode == AddMode::Ssh)
        .then(|| form.test.text())
        .flatten()
        .map(|text| aligned(test_result(&form.test, text)));
    let error = error.map(|error| {
        aligned(error_box(
            div().debug_selector(|| "connections-error".to_owned()),
            error.to_owned(),
        ))
    });
    let fields: Vec<Div> = [field(first, first_control), field(second, second_control)]
        .into_iter()
        .flatten()
        .collect();
    let buttons = div().flex().justify_end().gap(px(6.0)).children(
        form.mode
            .buttons()
            .into_iter()
            .map(|button| add_button(button, &form.test, draw, cx))
            .collect::<Vec<_>>(),
    );
    div()
        .flex_none()
        .mt(px(6.0))
        .mx(px(14.0))
        .p(px(12.0))
        .border_1()
        .border_color(gpui::rgb(LINE))
        .rounded(px(10.0))
        .bg(gpui::rgb(RAISED))
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(form_top(form.mode, draw, cx))
        .children(fields)
        .children(error)
        .children(result)
        .child(buttons)
}

/// The form's title and its Tailnet | SSH switch.
fn form_top(mode: AddMode, draw: &Draw<'_>, cx: &mut Context<RootView>) -> Div {
    let switch = div()
        .flex()
        .p(px(3.0))
        .gap(px(2.0))
        .bg(gpui::rgb(MODAL_INSET))
        .border_1()
        .border_color(gpui::rgb(LINE))
        .rounded(px(8.0))
        .child(segment(AddMode::Tailnet, mode, draw, cx))
        .child(segment(AddMode::Ssh, mode, draw, cx));
    div()
        .flex()
        .items_center()
        .justify_between()
        .child(
            div()
                .text_size(px(12.5))
                .font_weight(FontWeight::SEMIBOLD)
                .child("Add host"),
        )
        .child(switch)
}

/// The segment that switches the form to `this`, lit while `mode` is it.
fn segment(
    this: AddMode,
    mode: AddMode,
    draw: &Draw<'_>,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let control = PickerControl::of_mode(this);
    let selector = control.selector(draw.rows);
    let on = this == mode;
    div()
        .id(ElementId::Name(SharedString::from(selector.clone())))
        .debug_selector(move || selector)
        .relative()
        .px(px(12.0))
        .py(px(4.0))
        .rounded(px(6.0))
        .text_size(px(12.0))
        .cursor_pointer()
        .when(on, |segment| {
            segment
                .bg(gpui::rgb(HOVER))
                .text_color(gpui::rgb(TEXT))
                .font_weight(FontWeight::SEMIBOLD)
        })
        .when(!on, |segment| {
            segment
                .text_color(gpui::rgb(TEXT_2))
                .hover(|style| style.text_color(gpui::rgb(TEXT)))
        })
        .when(draw.focus == control, |segment| {
            segment.child(focus_ring(6.0))
        })
        .child(this.label())
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.press_connection_control(control, window, cx);
        }))
}

/// A button of the add form; Add and connect is the primary one.
fn add_button(
    button: AddButton,
    test: &SshTest,
    draw: &Draw<'_>,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let kind = if button == AddButton::Submit {
        ButtonKind::Primary
    } else {
        ButtonKind::Outlined
    };
    control_button(
        PickerControl::of_button(button),
        kind,
        ButtonSize::Regular,
        draw,
        cx,
    )
    .child(button.label(test))
}

/// A field of the form with its label before it; a mono field draws its
/// text in the mono face.
fn field_row(
    field: AddField,
    input: &Entity<TextInput>,
    control: PickerControl,
    focus: PickerControl,
) -> Div {
    let selector = control.selector(&[]);
    let frame = field_frame(focus == control)
        .debug_selector(move || selector)
        .flex_1()
        .min_w_0()
        .text_size(px(12.0))
        .when(field.mono, |frame| {
            frame.font_family(DEFAULT_FAMILY).text_size(px(11.5))
        })
        .child(input.clone());
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(
            div()
                .flex_none()
                .w(px(FIELD_LABEL_WIDTH))
                .text_color(gpui::rgb(TEXT_2))
                .child(field.label),
        )
        .child(frame)
}

/// `content` under the fields, past their labels' column.
fn aligned(content: Div) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(div().flex_none().w(px(FIELD_LABEL_WIDTH)))
        .child(content.flex_1())
}

/// Test connection's line: a green check and the host's name once it
/// passed, the failure in the error box, the wait muted.
fn test_result(test: &SshTest, text: String) -> Div {
    let line = div().debug_selector(|| "connections-ssh-form-result".to_owned());
    match test {
        SshTest::Failed(_) => error_box(line, text),
        SshTest::Passed(_) => line
            .flex()
            .items_center()
            .gap(px(7.0))
            .text_color(gpui::rgb(TEXT))
            .child(check_disc())
            .child(text),
        SshTest::Idle | SshTest::Testing => line.text_color(gpui::rgb(TEXT_2)).child(text),
    }
}

/// A green check on its 16 px disc.
fn check_disc() -> Div {
    div()
        .flex_none()
        .size(px(16.0))
        .rounded_full()
        .bg(gpui::rgba(OK_WASH))
        .flex()
        .items_center()
        .justify_center()
        .child(
            svg()
                .path(CHECK_ICON)
                .size(px(11.0))
                .text_color(gpui::rgb(STATUS_OK)),
        )
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
