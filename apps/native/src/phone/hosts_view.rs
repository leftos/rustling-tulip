//! The phone's hosts screen: the first-run form that adds a tailnet host
//! by name and port, and once hosts are saved, one row per host with its
//! connection state and a Pair button for a host this phone is not paired
//! with. No tailnet scan runs on a phone: hosts are typed.

use std::collections::HashSet;
use std::mem::Discriminant;

use gpui::{
    App, ClickEvent, Context, Div, Entity, Focusable as _, FontWeight, MouseButton, MouseDownEvent,
    Stateful, Window, div, prelude::*, px,
};

use super::PhoneRoot;
use super::home_view::TOUCH_TARGET;
use super::push::{NOTIFICATIONS_LABEL, SwitchView, switch_view};
use super::settings::Page;
use crate::buttons::{ButtonKind, ButtonSize, Layer, button, button_look};
use crate::connection::{Connection, State};
use crate::connection_picker::{
    LiveState, RowFacts, RowHost, SavedHost, offers_pair, typed_ssh_target, typed_target,
};
use crate::driver_id::DriverId as _;
use crate::target::{DEFAULT_TAILNET_PORT, HostKey, TailnetHost, Target};
use crate::text_input::TextInput;
use crate::ui_theme::{UiTheme, theme};
use crate::{NetCommand, RootView};

/// Home's line while a tap waits for its host's session list.
const PENDING_OPEN_ID: &str = "phone-open-pending";
/// Home's line when a tap's session could not be opened.
const OPEN_NOTE_ID: &str = "phone-open-note";
/// The form's title, and the label of the button that shows it again.
const ADD_TITLE: &str = "Add a host";
/// The name field's placeholder.
const NAME_PLACEHOLDER: &str = "host name on your tailnet";
/// The port field's placeholder.
const PORT_PLACEHOLDER: &str = "port";
/// The form's submit button.
const ADD_LABEL: &str = "Add";
/// The form's mode switch: the tailnet name and port, or an SSH host.
const MODE_TAILNET: &str = "Tailnet";
const MODE_SSH: &str = "SSH";
/// The SSH host field's placeholder.
const SSH_PLACEHOLDER: &str = "user@host or user@host:port";
/// The remote command field's placeholder.
const COMMAND_PLACEHOLDER: &str = "remote command (optional)";
/// The phone's key block: its label, the hint under the fingerprint and the
/// button that copies the key.
const KEY_LABEL: &str = "This phone's key";
const KEY_HINT: &str = "Check that it matches the PC's key before you trust it.";
const COPY_LABEL: &str = "Copy";
/// The unknown host key prompt's title, and the buttons that answer it.
const UNKNOWN_KEY_LABEL: &str = "Unknown host key";
const TRUST_LABEL: &str = "Trust";
const CANCEL_LABEL: &str = "Cancel";
/// An offline or stopped host's Retry button.
const RETRY_LABEL: &str = "Retry";
/// A row's Pair button.
const PAIR_LABEL: &str = "Pair";
/// Why the phone's add form refuses an SSH host with no `user@`.
const SSH_NEEDS_USER: &str =
    "Type it as user@host or user@host:port: the phone has no SSH config to look a name up in.";

/// Each saved host's name with the kind of state its connection was last
/// seen in (`None` while the window holds none).
type SeenStates = Vec<(String, Option<Discriminant<State>>)>;

/// Which kind of host the add form takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostMode {
    /// A `MagicDNS` name and port on the tailnet.
    Tailnet,
    /// A `user@host[:port]` string and an optional remote command.
    Ssh,
}

/// The add form's state: the mode it takes and each mode's fields, the last
/// refusal, the phone's key once SSH mode has read it, and whether the form
/// shows beside saved hosts (with none saved it always shows); and the
/// saved tailnet hosts the phone holds no device token for, read from the
/// token store only when a host's connection changes state, never per frame.
pub(crate) struct PhoneHosts {
    mode: HostMode,
    name: Entity<TextInput>,
    port: Entity<TextInput>,
    ssh: Entity<TextInput>,
    command: Entity<TextInput>,
    /// The phone's public key line, or why it cannot be read: read once,
    /// when the form first opens on SSH, since reading it reaches the
    /// Keychain.
    key: Option<Result<String, String>>,
    error: Option<String>,
    form_open: bool,
    /// Whether any saved host's connection was open at the last root
    /// change, so a host that just connected can be told from one that was
    /// open already.
    was_open: bool,
    not_paired: HashSet<String>,
    seen: SeenStates,
}

impl PhoneHosts {
    /// An empty form on the tailnet mode and the desktop picker's default
    /// port, with the not-paired hosts read from `root`'s token store.
    pub(crate) fn new(root: &Entity<RootView>, cx: &mut Context<PhoneRoot>) -> Self {
        let mut hosts = Self {
            mode: HostMode::Tailnet,
            name: cx.new(|cx| TextInput::new(String::new(), NAME_PLACEHOLDER, cx)),
            port: cx
                .new(|cx| TextInput::new(DEFAULT_TAILNET_PORT.to_string(), PORT_PLACEHOLDER, cx)),
            ssh: cx.new(|cx| TextInput::new(String::new(), SSH_PLACEHOLDER, cx)),
            command: cx.new(|cx| TextInput::new(String::new(), COMMAND_PLACEHOLDER, cx)),
            key: None,
            error: None,
            form_open: false,
            was_open: false,
            not_paired: HashSet::new(),
            seen: Vec::new(),
        };
        hosts.read_tokens(root.read(cx));
        hosts
    }

    /// The saved tailnet hosts this phone holds no device token for, as the
    /// token store gave them when a host's connection last changed state.
    pub(super) fn not_paired(&self) -> &HashSet<String> {
        &self.not_paired
    }

    /// Whether a host just connected while the add form is open: the hosts
    /// screen that shows the form is about to give way to Home.
    pub(super) fn form_left_by_a_connection(&mut self, open: bool) -> bool {
        let connected = open && !self.was_open;
        self.was_open = open;
        connected && self.form_open
    }

    /// The mode the form takes.
    fn mode(&self) -> HostMode {
        self.mode
    }

    /// The phone's public key, once SSH mode has read it.
    fn key(&self) -> Option<&Result<String, String>> {
        self.key.as_ref()
    }

    /// Keeps the key SSH mode read.
    fn set_key(&mut self, key: Result<String, String>) {
        self.key = Some(key);
    }

    /// The form switches to `mode`, keeping each mode's typed text; the
    /// refusal goes with the mode it was typed in.
    fn set_mode(&mut self, mode: HostMode) {
        self.mode = mode;
        self.error = None;
    }

    /// The target the form's fields name, or why the mode refuses them.
    fn typed(&self, cx: &App) -> Result<Target, String> {
        match self.mode {
            HostMode::Tailnet => typed_target(self.name.read(cx).text(), self.port.read(cx).text())
                .map_err(|refusal| refusal.to_string()),
            HostMode::Ssh => {
                typed_phone_ssh_target(self.ssh.read(cx).text(), self.command.read(cx).text())
            }
        }
    }

    /// Reads the not-paired hosts afresh from `root`'s token store.
    pub(super) fn read_tokens(&mut self, root: &RootView) {
        self.seen = seen_states(root);
        self.not_paired = root.hosts_not_paired(&root.sidebar.ui_state().saved_hosts);
    }

    /// The root view changed: the token store is read again only when a
    /// saved host's connection is in another state than last seen, or the
    /// saved hosts changed.
    pub(crate) fn root_changed(&mut self, root: &RootView) {
        if seen_states(root) != self.seen {
            self.read_tokens(root);
        }
    }

    /// Why the last Add was refused, while the form says so.
    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Back to an empty form on the tailnet mode and no refusal; a key
    /// already read is kept.
    fn reset(&mut self, cx: &mut Context<PhoneRoot>) {
        self.mode = HostMode::Tailnet;
        self.error = None;
        self.name
            .update(cx, |input, cx| input.set_text(String::new(), cx));
        self.port.update(cx, |input, cx| {
            input.set_text(DEFAULT_TAILNET_PORT.to_string(), cx);
        });
        self.ssh
            .update(cx, |input, cx| input.set_text(String::new(), cx));
        self.command
            .update(cx, |input, cx| input.set_text(String::new(), cx));
    }

    /// Collapses the form, emptying it: the Hosts page shows its rows, and a
    /// later Add starts from an empty form.
    fn close_form(&mut self, cx: &mut Context<PhoneRoot>) {
        self.reset(cx);
        self.form_open = false;
    }
}

/// What a saved host's row shows.
struct HostRow {
    name: String,
    key: HostKey,
    parts: RowParts,
    pair: bool,
}

/// What a host shows beside and under its head: the state word always, and
/// the parts only the states that call for them. A host's row on the hosts screen and
/// its header on Home both draw these, so the two never disagree.
pub(super) struct RowParts {
    pub(super) state_word: &'static str,
    /// Why the connection stopped, or failed.
    pub(super) reason: Option<String>,
    /// The host key the user has not trusted yet, as its fingerprint, while
    /// one is pending.
    pub(super) prompt: Option<String>,
    /// Whether the row offers a Retry line under its reason, which a
    /// stopped host does.
    pub(super) retry: bool,
    /// Whether the row offers Retry for an offline host: in the head of its
    /// Home header, so the header does not grow, and as a line on its hosts
    /// screen row.
    pub(super) redial: bool,
}

impl RowParts {
    /// The parts of a row for `state`, with `pending` the fingerprint the
    /// user has not answered for. A pending host key takes the place of the
    /// reason line and Retry: the prompt is the row's one answer.
    fn of(state: Option<&State>, pending: Option<&str>) -> Self {
        let state_word = state_words(state);
        let parts = match state {
            Some(State::AuthFailed { reason }) => Some((Some(reason.clone()), true, false)),
            Some(State::Error { reason }) => Some((Some(reason.clone()), false, true)),
            Some(State::Closed { .. }) => Some((None, false, true)),
            _ => None,
        };
        let Some((reason, retry, redial)) = parts else {
            return Self {
                state_word,
                reason: None,
                prompt: None,
                retry: false,
                redial: false,
            };
        };
        match pending {
            Some(fingerprint) => Self {
                state_word,
                reason: None,
                prompt: Some(fingerprint.to_owned()),
                retry: false,
                redial: false,
            },
            None => Self {
                state_word,
                reason,
                prompt: None,
                retry,
                redial,
            },
        }
    }
}

/// What a host's row on the hosts screen, or its header on Home, shows
/// besides its name: the parts its connection's state and any host key
/// still waiting on the user call for.
pub(super) fn row_parts(root: &RootView, key: &HostKey) -> RowParts {
    RowParts::of(
        root.conn_of(key).map(Connection::state),
        root.unverified_fingerprint(key),
    )
}

/// The name a host's saved record has: its key without the `ssh:` marker an
/// SSH host carries, so both screens tag its parts alike.
pub(super) fn saved_name(host: &HostKey) -> &str {
    match host {
        HostKey::Local => "This PC",
        HostKey::Remote(key) => key.strip_prefix("ssh:").unwrap_or(key),
    }
}

/// The SSH target the add form's fields name, or why it is refused. The
/// phone has no `~/.ssh/config` to look a bare name up in, so the string
/// must carry its user.
fn typed_phone_ssh_target(host: &str, remote_command: &str) -> Result<Target, String> {
    if !host.contains('@') {
        return Err(SSH_NEEDS_USER.to_owned());
    }
    typed_ssh_target(host, remote_command).map_err(|refusal| refusal.to_string())
}

/// A connection's state in the words the sidebar's host header uses, with
/// "Online" for an open one, which the header leaves unsaid.
fn state_words(state: Option<&State>) -> &'static str {
    match state {
        Some(State::Open { .. }) => "Online",
        Some(State::Init | State::Connecting) => "Connecting…",
        Some(State::NotApproved { .. }) => "Not paired",
        Some(State::AuthFailed { .. }) => "Stopped",
        Some(State::Closed { .. } | State::Error { .. }) | None => "Offline",
    }
}

/// Each saved host's name and its connection's kind of state now.
fn seen_states(root: &RootView) -> SeenStates {
    root.sidebar
        .ui_state()
        .saved_hosts
        .iter()
        .map(|host| {
            let state = host
                .target()
                .ok()
                .and_then(|target| root.conn_of(&HostKey::of(&target)))
                .map(|conn| std::mem::discriminant(conn.state()));
            (host.name.clone(), state)
        })
        .collect()
}

/// Whether any saved host's connection is open, when the phone lists the
/// sessions in place of the hosts' rows.
pub(super) fn any_host_open(root: &RootView) -> bool {
    root.sidebar.ui_state().saved_hosts.iter().any(|host| {
        host.target()
            .is_ok_and(|target| host_open(root, &HostKey::of(&target)))
    })
}

/// Whether the window's connection to `host` is open.
pub(super) fn host_open(root: &RootView, host: &HostKey) -> bool {
    root.conn_of(host)
        .is_some_and(|conn| matches!(conn.state(), State::Open { .. }))
}

/// The key of `host` when it is a tailnet host whose connection is open and
/// whose daemon lists relay push: the hosts a Notifications switch is for.
fn relay_push_key(root: &RootView, host: &SavedHost) -> Option<HostKey> {
    let target = host.target().ok()?;
    target.tailnet_host()?;
    let key = HostKey::of(&target);
    root.conn_of(&key)?
        .supports(protocol::FEATURE_RELAY_PUSH)
        .then_some(key)
}

/// Every saved tailnet host open and taking relay pushes, in saved order,
/// with the generation of the connection the window holds to it.
pub(super) fn relay_push_hosts(root: &RootView) -> Vec<(HostKey, u64)> {
    root.sidebar
        .ui_state()
        .saved_hosts
        .iter()
        .filter_map(|host| {
            let key = relay_push_key(root, host)?;
            let generation = root.connection_generation(&key)?;
            Some((key, generation))
        })
        .collect()
}

/// What `host`'s Notifications switch draws, when it draws: only on a
/// platform with push (`supported`), for a host [`relay_push_key`] takes.
fn push_switch_view(root: &RootView, host: &SavedHost, supported: bool) -> Option<SwitchView> {
    if !supported {
        return None;
    }
    let key = relay_push_key(root, host)?;
    Some(switch_view(root.push_hosts().state(&key)))
}

/// `host`'s row: Pair shows exactly when the desktop picker's row offers
/// it, for a held tailnet host that refused this device or that it holds
/// no token for (`not_paired`).
fn host_row(root: &RootView, host: &SavedHost, not_paired: &HashSet<String>) -> Option<HostRow> {
    let target: Target = host.target().ok()?;
    let key = HostKey::of(&target);
    let conn = root.conn_of(&key);
    let state = conn.map(Connection::state);
    let facts = RowFacts {
        host: if target.tailnet_host().is_some() {
            RowHost::Tailnet
        } else {
            RowHost::Ssh
        },
        port: target.tailnet_host().map(TailnetHost::port),
        remote_command: None,
        enabled: host.enabled,
        held: conn.is_some(),
        live: state.and_then(LiveState::from_state),
        duplicate_of: None,
        has_token: !not_paired.contains(&host.name),
        sessions_last_time: None,
        live_sessions: None,
        build_version: None,
        remote_update: false,
        client_update: false,
    };
    let parts = row_parts(root, &key);
    Some(HostRow {
        name: host.name.clone(),
        key,
        parts,
        pair: offers_pair(&facts),
    })
}

/// A button on the phone's ground, tagged `selector`.
pub(super) fn phone_button(
    selector: &str,
    kind: ButtonKind,
    cx: &Context<PhoneRoot>,
) -> Stateful<Div> {
    button(
        selector,
        button_look(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            kind,
            ButtonSize::Large,
            true,
            Layer::Base,
            &theme(cx),
        ),
    )
}

/// A field in its outlined box, tagged `selector`; a press anywhere on the
/// box gives the field the keyboard.
pub(super) fn field_box(
    selector: &'static str,
    input: &Entity<TextInput>,
    t: &UiTheme,
    cx: &Context<PhoneRoot>,
) -> Div {
    let focus = input.read(cx).focus_handle(cx);
    div()
        .driver_id(|| selector.to_owned())
        .flex()
        .w_full()
        .px(px(8.0))
        .py(px(8.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(gpui::rgb(t.border))
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            focus.focus(window, cx);
        })
        .child(input.clone())
}

impl PhoneRoot {
    /// The hosts screen: under the header with its `Settings` button, the
    /// hosts' content, which Settings' Hosts page draws too, scrolling so
    /// Add stays above the keyboard the SSH form raises.
    pub(super) fn hosts_body(&self, cx: &mut Context<Self>) -> Div {
        let t = theme(cx);
        let marked = self.root.read(cx).push_hosts().any_hint();
        let header = Self::home_header_row(false, marked, &t, cx);
        div().size_full().flex().flex_col().child(header).child(
            div()
                .id("phone-hosts-body")
                .driver_id(|| "phone-hosts-body".to_owned())
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(self.hosts_content(&t, cx)),
        )
    }

    /// The saved hosts' rows and the tail under them: the add form or the
    /// button that shows it, the form alone while no host is saved.
    pub(super) fn hosts_content(&self, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let root = self.root.read(cx);
        let rows: Vec<HostRow> = root
            .sidebar
            .ui_state()
            .saved_hosts
            .iter()
            .filter_map(|host| host_row(root, host, &self.hosts.not_paired))
            .collect();
        let form_shows = rows.is_empty() || self.hosts.form_open;
        let row_elements: Vec<Div> = rows.iter().map(|row| Self::row(row, t, cx)).collect();
        let tail = self.hosts_tail(form_shows, t, cx);
        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(16.0))
            .children(self.tap_lines(t))
            .children(row_elements)
            .child(tail)
    }

    /// Leaving Settings' Hosts page collapses its add form, so the page
    /// shows its rows when it comes back: with no host saved the form is the
    /// page itself, and it stays.
    pub(super) fn leave_hosts_page(&mut self, cx: &mut Context<Self>) {
        let on_hosts = self
            .settings
            .as_ref()
            .is_some_and(|settings| settings.page == Some(Page::Hosts));
        if on_hosts && self.a_host_is_saved(cx) {
            self.hosts.close_form(cx);
        }
    }

    /// Whether the Hosts page draws any saved host's row: the same rows
    /// [`Self::hosts_content`] builds.
    fn a_host_is_saved(&self, cx: &App) -> bool {
        let root = self.root.read(cx);
        let not_paired = self.hosts.not_paired();
        root.sidebar
            .ui_state()
            .saved_hosts
            .iter()
            .any(|host| host_row(root, host, not_paired).is_some())
    }

    /// Home while a saved host is open: under the header with its sort and
    /// `Settings` buttons, the session list filling the height.
    pub(super) fn list_body(&self, cx: &mut Context<Self>) -> Div {
        let (panel, pinned) = self.home_list(cx);
        let t = theme(cx);
        let marked = self.root.read(cx).push_hosts().any_hint();
        let header = Self::home_header_row(true, marked, &t, cx);
        let tap_lines = self.tap_lines(&t);
        let over = (!tap_lines.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .px(px(16.0))
                .pt(px(12.0))
                .children(tap_lines)
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .children(over)
            .child(
                // No padding, so the pinned header sits on the list's top.
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(panel)
                    .children(pinned),
            )
    }

    /// The tap's line: what the phone is opening, or why it could not open
    /// the session.
    fn tap_lines(&self, t: &UiTheme) -> Vec<Div> {
        let pending = self.pending_open.as_ref().map(|pending| {
            div()
                .driver_id(|| PENDING_OPEN_ID.to_owned())
                .text_color(gpui::rgb(t.muted))
                .child(format!("Opening a session on {}…", pending.host_name()))
        });
        let note = self.open_note.as_ref().map(|note| {
            div()
                .driver_id(|| OPEN_NOTE_ID.to_owned())
                .text_color(gpui::rgb(t.muted))
                .child(note.clone())
        });
        pending.into_iter().chain(note).collect()
    }

    /// One line per saved host with a Notifications switch: its name and
    /// the switch, for Settings' Notifications page.
    pub(super) fn push_lines(&self, t: &UiTheme, cx: &mut Context<Self>) -> Vec<Div> {
        let supported = self.platform.supported();
        let root = self.root.read(cx);
        let switches: Vec<(String, HostKey, SwitchView)> = root
            .sidebar
            .ui_state()
            .saved_hosts
            .iter()
            .filter_map(|host| {
                let view = push_switch_view(root, host, supported)?;
                let key = relay_push_key(root, host)?;
                Some((host.name.clone(), key, view))
            })
            .collect();
        switches
            .into_iter()
            .map(|(name, key, view)| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .truncate()
                            .text_color(gpui::rgb(t.muted))
                            .child(name.clone()),
                    )
                    .child(Self::push_switch(&name, key, view, t, cx))
            })
            .collect()
    }

    /// `name`'s Notifications switch: on or off as `view` says, dimmed while
    /// a request is out, and its hint under it; at least 44 pt tall.
    fn push_switch(
        name: &str,
        key: HostKey,
        view: SwitchView,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Div {
        let toggle = crate::spawn_view::checkbox_row(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            &format!("phone-host-push-{name}"),
            view.on,
            false,
            NOTIFICATIONS_LABEL,
            t,
        )
        .min_h(px(44.0))
        .items_center()
        .when(view.busy, |toggle| toggle.opacity(0.45))
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.toggle_push(&key, cx);
        }));
        let hint_id = format!("phone-host-push-hint-{name}");
        let hint = view.hint.map(|hint| {
            div()
                .driver_id(|| hint_id)
                .text_color(gpui::rgb(t.muted))
                .child(hint)
        });
        div().flex().flex_col().child(toggle).children(hint)
    }

    /// Under the hosts: the add form while `form_shows`, else the button
    /// that shows it.
    fn hosts_tail(&self, form_shows: bool, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        if form_shows {
            self.add_form(t, cx)
        } else {
            div().child(
                phone_button("phone-host-add-another", ButtonKind::Outlined, cx)
                    .min_h(px(TOUCH_TARGET))
                    .child(ADD_TITLE)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.show_add_form(window, cx);
                    })),
            )
        }
    }

    /// One saved host: its head, the unknown host key prompt, its reason
    /// line and its Retry button, as its state calls for them.
    fn row(row: &HostRow, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let row_id = format!("phone-host-row-{}", row.name);
        let mut card = div()
            .driver_id(|| row_id)
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(12.0))
            .rounded(px(10.0))
            .border_1()
            .border_color(gpui::rgb(t.line))
            .bg(gpui::rgb(t.raised))
            .child(Self::row_head(row, t, cx));
        if let Some(fingerprint) = row.parts.prompt.as_deref() {
            card = card.child(Self::host_key_prompt(
                &row.name,
                &row.key,
                fingerprint,
                t,
                cx,
            ));
        }
        if let Some(reason) = row.parts.reason.as_deref() {
            card = card.child(Self::reason_line(&row.name, reason, t));
        }
        if row.parts.retry || row.parts.redial {
            card = card.child(Self::retry_line(&row.name, &row.key, cx));
        }
        card
    }

    /// A row's head: its name and state word, and its Pair button when it
    /// offers one.
    fn row_head(row: &HostRow, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let pair_button = row.pair.then(|| {
            let key = row.key.clone();
            phone_button(
                &format!("phone-host-pair-{}", row.name),
                ButtonKind::Primary,
                cx,
            )
            .min_h(px(TOUCH_TARGET))
            .child(PAIR_LABEL)
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.pair(&key, window, cx);
            }))
        });
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().truncate().child(row.name.clone()))
                    .child(
                        div()
                            .text_color(gpui::rgb(t.muted))
                            .child(row.parts.state_word),
                    ),
            )
            .children(pair_button)
    }

    /// The prompt on a host whose connection offered a key this phone has
    /// not trusted: the fingerprint, and Trust and Cancel.
    pub(super) fn host_key_prompt(
        name: &str,
        key: &HostKey,
        fingerprint: &str,
        t: &UiTheme,
        cx: &mut Context<Self>,
    ) -> Div {
        let block_id = format!("phone-host-hostkey-{name}");
        let trust_id = format!("phone-host-trust-{name}");
        let cancel_id = format!("phone-host-cancel-{name}");
        let trusted = key.clone();
        let cancelled = key.clone();
        let trust = phone_button(&trust_id, ButtonKind::Primary, cx)
            .min_h(px(TOUCH_TARGET))
            .child(TRUST_LABEL)
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.root
                    .update(cx, |root, cx| root.trust_host_key(&trusted, window, cx));
            }));
        let cancel = phone_button(&cancel_id, ButtonKind::Outlined, cx)
            .min_h(px(TOUCH_TARGET))
            .child(CANCEL_LABEL)
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.root
                    .update(cx, |root, cx| root.cancel_host_key(&cancelled, cx));
            }));
        div()
            .driver_id(|| block_id)
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                    .child(UNKNOWN_KEY_LABEL),
            )
            .child(
                div()
                    .min_w_0()
                    .font_family(crate::fonts::DEFAULT_FAMILY)
                    .child(fingerprint.to_owned()),
            )
            .child(div().text_color(gpui::rgb(t.muted)).child(KEY_HINT))
            .child(div().flex().gap(px(8.0)).child(trust).child(cancel))
    }

    /// A host's reason line: why its connection stopped or failed, wrapped
    /// so the whole of it shows.
    pub(super) fn reason_line(name: &str, reason: &str, t: &UiTheme) -> Div {
        div()
            .driver_id(|| format!("phone-host-reason-{name}"))
            .min_w_0()
            .text_color(gpui::rgb(t.muted))
            .child(reason.to_owned())
    }

    /// A host's Retry button: dials it again.
    pub(super) fn retry_button(name: &str, key: &HostKey, cx: &mut Context<Self>) -> Stateful<Div> {
        let key = key.clone();
        phone_button(
            &format!("phone-host-retry-{name}"),
            ButtonKind::Outlined,
            cx,
        )
        .min_h(px(TOUCH_TARGET))
        .child(RETRY_LABEL)
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.retry(&key, cx);
        }))
    }

    /// A host's Retry on a line of its own: a stopped host's under its
    /// reason, and an offline host's on its hosts screen row.
    pub(super) fn retry_line(name: &str, key: &HostKey, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .justify_end()
            .child(Self::retry_button(name, key, cx))
    }

    /// The add form: its title, the mode switch, the fields that mode
    /// takes, the phone's key in SSH mode, a refusal under them and the Add
    /// button.
    fn add_form(&self, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let error = self.hosts.error.clone().map(|error| {
            div()
                .driver_id(|| "phone-host-error".to_owned())
                .px(px(6.0))
                .py(px(3.0))
                .rounded(px(4.0))
                .bg(gpui::rgb(t.danger_bg))
                .text_color(gpui::rgb(t.danger))
                .child(error)
        });
        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                    .child(ADD_TITLE),
            )
            .child(self.mode_switch(cx))
            .children(self.mode_fields(t, cx))
            .children(self.key_block(t, cx))
            .children(error)
            .child(Self::add_row(cx))
    }

    /// The form's mode switch: Tailnet and SSH side by side, the current
    /// one Primary.
    fn mode_switch(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .gap(px(8.0))
            .child(self.mode_button(
                HostMode::Tailnet,
                MODE_TAILNET,
                "phone-host-mode-tailnet",
                cx,
            ))
            .child(self.mode_button(HostMode::Ssh, MODE_SSH, "phone-host-mode-ssh", cx))
    }

    /// One mode button: `label` in `selector`, Primary while it is the
    /// form's mode, and tall enough to tap.
    fn mode_button(
        &self,
        mode: HostMode,
        label: &'static str,
        selector: &'static str,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let kind = if self.hosts.mode() == mode {
            ButtonKind::Primary
        } else {
            ButtonKind::Outlined
        };
        phone_button(selector, kind, cx)
            .flex_1()
            .justify_center()
            .min_h(px(44.0))
            .child(label)
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.switch_host_mode(mode, cx);
            }))
    }

    /// The fields the form's mode takes.
    fn mode_fields(&self, t: &UiTheme, cx: &mut Context<Self>) -> Vec<Div> {
        match self.hosts.mode() {
            HostMode::Tailnet => vec![
                field_box("phone-host-name", &self.hosts.name, t, cx),
                field_box("phone-host-port", &self.hosts.port, t, cx),
            ],
            HostMode::Ssh => vec![
                field_box("phone-host-ssh", &self.hosts.ssh, t, cx),
                field_box("phone-host-command", &self.hosts.command, t, cx),
            ],
        }
    }

    /// The phone's key in SSH mode, with Copy; nothing in Tailnet mode and
    /// nothing before the key has been read.
    fn key_block(&self, t: &UiTheme, cx: &mut Context<Self>) -> Option<Div> {
        if self.hosts.mode() != HostMode::Ssh {
            return None;
        }
        let key = self.hosts.key()?;
        let block = div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(div().text_color(gpui::rgb(t.muted)).child(KEY_LABEL));
        let block = match key {
            Ok(line) => {
                let copy = phone_button("phone-host-copy-key", ButtonKind::Outlined, cx)
                    .min_h(px(TOUCH_TARGET))
                    .child(COPY_LABEL)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.copy_phone_key(cx)));
                block
                    .child(
                        div()
                            .driver_id(|| "phone-host-key".to_owned())
                            .min_w_0()
                            .font_family(crate::fonts::DEFAULT_FAMILY)
                            .child(line.clone()),
                    )
                    .child(div().flex().justify_end().child(copy))
            }
            Err(err) => block.child(
                div()
                    .driver_id(|| "phone-host-key-error".to_owned())
                    .min_w_0()
                    .text_color(gpui::rgb(t.danger))
                    .child(format!("The phone's key could not be read: {err}")),
            ),
        };
        Some(block)
    }

    /// The form's Add button.
    fn add_row(cx: &mut Context<Self>) -> Div {
        div().flex().justify_end().child(
            phone_button("phone-host-add", ButtonKind::Primary, cx)
                .min_h(px(TOUCH_TARGET))
                .child(ADD_LABEL)
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.add_typed_host(window, cx);
                })),
        )
    }

    /// The add form switches to `mode`: its refusal goes, and the phone's
    /// key is read the first time SSH opens.
    fn switch_host_mode(&mut self, mode: HostMode, cx: &mut Context<Self>) {
        if self.hosts.mode() == mode {
            return;
        }
        self.hosts.set_mode(mode);
        if mode == HostMode::Ssh && self.hosts.key().is_none() {
            let key = self.root.read(cx).phone_public_key();
            self.hosts.set_key(key);
        }
        cx.notify();
    }

    /// Copy on the key block: the key line goes to the clipboard.
    fn copy_phone_key(&mut self, cx: &mut Context<Self>) {
        let Some(Ok(line)) = self.hosts.key() else {
            return;
        };
        let line = line.clone();
        self.root
            .update(cx, |root, cx| root.copy_to_clipboard(&line, cx));
    }

    /// Retry on `host`'s row: its network thread dials it again.
    fn retry(&mut self, host: &HostKey, cx: &mut Context<Self>) {
        self.root.update(cx, |root, cx| {
            root.command_to(host, NetCommand::Restart);
            cx.notify();
        });
    }

    /// "Add a host" under the rows: the form shows, empty, with the
    /// keyboard in its name field.
    fn show_add_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.forget_pending_open();
        self.hosts.reset(cx);
        self.hosts.form_open = true;
        self.hosts.name.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// Add: the typed host is saved and connected, or the form says why it
    /// was refused.
    fn add_typed_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.hosts.typed(cx) {
            Ok(target) => {
                tracing::info!(?target, "phone: adding a host");
                self.hosts.reset(cx);
                self.hosts.form_open = false;
                self.root
                    .update(cx, |root, cx| root.add_saved_host(target, window, cx));
                self.hosts.read_tokens(self.root.read(cx));
            }
            Err(refusal) => self.hosts.error = Some(refusal),
        }
        cx.notify();
    }

    /// Pair on `host`'s row: the dialog opens for it.
    pub(super) fn pair(&self, host: &HostKey, window: &mut Window, cx: &mut Context<Self>) {
        self.root
            .update(cx, |root, cx| root.pair_with_host(host, window, cx));
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "a test states the rule it exercises")]
mod tests {
    use super::*;
    use crate::target::SshHost;

    /// The refusal an SSH host with no `user@` gets.
    const NEEDS_USER: &str =
        "Type it as user@host or user@host:port: the phone has no SSH config to look a name up in.";

    #[test]
    fn an_ssh_host_without_user_at_is_refused() {
        let refusal = typed_phone_ssh_target("box", "").expect_err("a bare host name is refused");
        assert_eq!(refusal, NEEDS_USER);
    }

    #[test]
    fn a_typed_ssh_host_keeps_its_command() {
        let target = typed_phone_ssh_target("me@box", "hostname").expect("a typed SSH host");
        assert_eq!(
            target.ssh_host().and_then(SshHost::remote_command),
            Some("hostname"),
            "the remote command reaches the target"
        );
        assert_eq!(target.ssh_host().map(SshHost::name), Some("me@box"));
    }

    #[test]
    fn an_auth_failed_row_says_stopped_with_its_reason_and_retry() {
        let state = State::AuthFailed {
            reason: "box refused this phone's key".to_owned(),
        };
        let parts = RowParts::of(Some(&state), None);
        assert_eq!(parts.state_word, "Stopped");
        assert_eq!(
            parts.reason.as_deref(),
            Some("box refused this phone's key")
        );
        assert!(parts.prompt.is_none(), "nothing is pending to trust");
        assert!(parts.retry, "a stopped host offers Retry");
        assert!(!parts.redial, "its Retry is a line under its reason");
    }

    #[test]
    fn an_error_row_says_offline_with_its_reason_and_redial() {
        let state = State::Error {
            reason: "no handshake".to_owned(),
        };
        let parts = RowParts::of(Some(&state), None);
        assert_eq!(parts.state_word, "Offline");
        assert_eq!(parts.reason.as_deref(), Some("no handshake"));
        assert!(parts.prompt.is_none());
        assert!(
            !parts.retry,
            "an offline host draws no Retry under its reason"
        );
        assert!(parts.redial, "an offline host offers Retry in its head");
    }

    #[test]
    fn a_closed_row_says_offline_and_offers_redial() {
        let state = State::Closed {
            reason: "gone".to_owned(),
        };
        let parts = RowParts::of(Some(&state), None);
        assert_eq!(parts.state_word, "Offline");
        assert!(parts.reason.is_none(), "a closed host shows no reason");
        assert!(parts.prompt.is_none());
        assert!(
            !parts.retry,
            "an offline host draws no Retry under its reason"
        );
        assert!(parts.redial, "an offline host offers Retry in its head");
    }

    #[test]
    fn a_connecting_row_offers_no_retry() {
        for state in [State::Init, State::Connecting] {
            let parts = RowParts::of(Some(&state), None);
            assert!(
                !parts.retry && !parts.redial,
                "{state:?} offers no Retry: it is dialling already"
            );
        }
    }

    #[test]
    fn a_pending_host_key_shows_the_prompt_instead_of_the_reason() {
        let state = State::AuthFailed {
            reason: "box offered a host key this phone has not seen: SHA256:abc".to_owned(),
        };
        let parts = RowParts::of(Some(&state), Some("SHA256:abc"));
        assert_eq!(parts.state_word, "Stopped");
        assert_eq!(parts.prompt.as_deref(), Some("SHA256:abc"));
        assert!(
            parts.reason.is_none(),
            "the prompt replaces the reason line"
        );
        assert!(!parts.retry, "the prompt replaces Retry");
        assert!(!parts.redial, "and the Retry in the head");
    }
}
