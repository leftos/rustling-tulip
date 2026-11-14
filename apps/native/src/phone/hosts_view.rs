//! The phone's hosts screen: the first-run form that adds a tailnet host
//! by name and port, and once hosts are saved, one row per host with its
//! connection state and a Pair button for a host this phone is not paired
//! with. No tailnet scan runs on a phone: hosts are typed.

use std::collections::HashSet;
use std::mem::Discriminant;

use gpui::{
    ClickEvent, Context, Div, Entity, Focusable as _, FontWeight, MouseButton, MouseDownEvent,
    Stateful, Window, div, prelude::*, px,
};

use super::PhoneRoot;
use crate::RootView;
use crate::buttons::{ButtonKind, ButtonSize, Layer, button, button_look};
use crate::connection::{Connection, State};
use crate::connection_picker::{
    LiveState, RowFacts, RowHost, SavedHost, offers_pair, typed_target,
};
use crate::driver_id::DriverId as _;
use crate::sidebar_view::SidebarChrome;
use crate::target::{DEFAULT_TAILNET_PORT, HostKey, TailnetHost, Target};
use crate::text_input::TextInput;
use crate::ui_theme::{UiTheme, theme};

/// The form's title, and the label of the button that shows it again.
const ADD_TITLE: &str = "Add a host";
/// The name field's placeholder.
const NAME_PLACEHOLDER: &str = "host name on your tailnet";
/// The port field's placeholder.
const PORT_PLACEHOLDER: &str = "port";
/// The form's submit button.
const ADD_LABEL: &str = "Add";
/// A row's Pair button.
const PAIR_LABEL: &str = "Pair";

/// Each saved host's name with the kind of state its connection was last
/// seen in (`None` while the window holds none).
type SeenStates = Vec<(String, Option<Discriminant<State>>)>;

/// The add form's state: its two fields, the last refusal and whether it
/// shows beside saved hosts (with none saved it always shows); and the
/// saved tailnet hosts the phone holds no device token for, read from the
/// token store only when a host's connection changes state, never per frame.
pub(crate) struct PhoneHosts {
    name: Entity<TextInput>,
    port: Entity<TextInput>,
    error: Option<String>,
    form_open: bool,
    not_paired: HashSet<String>,
    seen: SeenStates,
}

impl PhoneHosts {
    /// An empty name and the desktop picker's default port, with the
    /// not-paired hosts read from `root`'s token store.
    pub(crate) fn new(root: &Entity<RootView>, cx: &mut Context<PhoneRoot>) -> Self {
        let mut hosts = Self {
            name: cx.new(|cx| TextInput::new(String::new(), NAME_PLACEHOLDER, cx)),
            port: cx
                .new(|cx| TextInput::new(DEFAULT_TAILNET_PORT.to_string(), PORT_PLACEHOLDER, cx)),
            error: None,
            form_open: false,
            not_paired: HashSet::new(),
            seen: Vec::new(),
        };
        hosts.read_tokens(root.read(cx));
        hosts
    }

    /// Reads the not-paired hosts afresh from `root`'s token store.
    fn read_tokens(&mut self, root: &RootView) {
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

    /// Back to an empty name, the default port and no refusal.
    fn reset(&mut self, cx: &mut Context<PhoneRoot>) {
        self.error = None;
        self.name
            .update(cx, |input, cx| input.set_text(String::new(), cx));
        self.port.update(cx, |input, cx| {
            input.set_text(DEFAULT_TAILNET_PORT.to_string(), cx);
        });
    }
}

/// What a saved host's row shows.
struct HostRow {
    name: String,
    key: HostKey,
    state: &'static str,
    pair: bool,
}

/// A connection's state in the words the sidebar's host header uses, with
/// "Online" for an open one, which the header leaves unsaid.
fn state_words(state: Option<&State>) -> &'static str {
    match state {
        Some(State::Open { .. }) => "Online",
        Some(State::Init | State::Connecting) => "Connecting…",
        Some(State::NotApproved { .. }) => "Not paired",
        Some(State::Closed { .. } | State::AuthFailed { .. } | State::Error { .. }) | None => {
            "Offline"
        }
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
            .ok()
            .and_then(|target| root.conn_of(&HostKey::of(&target)))
            .is_some_and(|conn| matches!(conn.state(), State::Open { .. }))
    })
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
    Some(HostRow {
        name: host.name.clone(),
        key,
        state: state_words(state),
        pair: offers_pair(&facts),
    })
}

/// A button on the phone's ground, tagged `selector`.
fn phone_button(selector: &str, kind: ButtonKind, cx: &Context<PhoneRoot>) -> Stateful<Div> {
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
fn field_box(
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
    /// The hosts screen: the saved hosts' rows, then the add form or the
    /// button that shows it; the form alone while no host is saved.
    pub(super) fn hosts_body(&self, cx: &mut Context<Self>) -> Div {
        let root = self.root.read(cx);
        let rows: Vec<HostRow> = root
            .sidebar
            .ui_state()
            .saved_hosts
            .iter()
            .filter_map(|host| host_row(root, host, &self.hosts.not_paired))
            .collect();
        let form_shows = rows.is_empty() || self.hosts.form_open;
        let t = theme(cx);
        let row_elements: Vec<Div> = rows.into_iter().map(|row| Self::row(row, &t, cx)).collect();
        let tail = self.hosts_tail(form_shows, &t, cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(16.0))
            .children(row_elements)
            .child(tail)
    }

    /// The sessions list `width` wide while a saved host is open, filling
    /// the height over the hosts screen's tail.
    pub(super) fn list_body(&self, width: f32, window: &Window, cx: &mut Context<Self>) -> Div {
        let panel = self.root.update(cx, |root, cx| {
            root.sidebar_panel(width, SidebarChrome::Phone, window, cx)
        });
        let t = theme(cx);
        let tail = self.hosts_tail(self.hosts.form_open, &t, cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(panel)
            .child(div().flex_none().p(px(16.0)).child(tail))
    }

    /// Under the hosts: the add form while `form_shows`, else the button
    /// that shows it.
    fn hosts_tail(&self, form_shows: bool, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        if form_shows {
            self.add_form(t, cx)
        } else {
            div().child(
                phone_button("phone-host-add-another", ButtonKind::Outlined, cx)
                    .child(ADD_TITLE)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.show_add_form(window, cx);
                    })),
            )
        }
    }

    /// One saved host: its name, its state in words and, when it offers
    /// one, its Pair button.
    fn row(row: HostRow, t: &UiTheme, cx: &mut Context<Self>) -> Div {
        let HostRow {
            name,
            key,
            state,
            pair,
        } = row;
        let pair_button = pair.then(|| {
            phone_button(&format!("phone-host-pair-{name}"), ButtonKind::Primary, cx)
                .child(PAIR_LABEL)
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.pair(&key, window, cx);
                }))
        });
        let row_id = format!("phone-host-row-{name}");
        div()
            .driver_id(|| row_id)
            .flex()
            .items_center()
            .gap(px(8.0))
            .p(px(12.0))
            .rounded(px(10.0))
            .border_1()
            .border_color(gpui::rgb(t.line))
            .bg(gpui::rgb(t.raised))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().truncate().child(name))
                    .child(div().text_color(gpui::rgb(t.muted)).child(state)),
            )
            .children(pair_button)
    }

    /// The add form: its title, the name and port fields, a refusal under
    /// them and the Add button.
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
            .child(field_box("phone-host-name", &self.hosts.name, t, cx))
            .child(field_box("phone-host-port", &self.hosts.port, t, cx))
            .children(error)
            .child(
                div().flex().justify_end().child(
                    phone_button("phone-host-add", ButtonKind::Primary, cx)
                        .child(ADD_LABEL)
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.add_typed_host(window, cx);
                        })),
                ),
            )
    }

    /// "Add a host" under the rows: the form shows, empty, with the
    /// keyboard in its name field.
    fn show_add_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.hosts.reset(cx);
        self.hosts.form_open = true;
        self.hosts.name.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// Add: the typed host is saved and connected, or the form says why it
    /// was refused.
    fn add_typed_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = typed_target(
            self.hosts.name.read(cx).text(),
            self.hosts.port.read(cx).text(),
        );
        match typed {
            Ok(target) => {
                tracing::info!(?target, "phone: adding a host");
                self.hosts.reset(cx);
                self.hosts.form_open = false;
                self.root
                    .update(cx, |root, cx| root.add_saved_host(target, window, cx));
                self.hosts.read_tokens(self.root.read(cx));
            }
            Err(refusal) => self.hosts.error = Some(refusal.to_string()),
        }
        cx.notify();
    }

    /// Pair on `host`'s row: the dialog opens for it.
    fn pair(&self, host: &HostKey, window: &mut Window, cx: &mut Context<Self>) {
        self.root
            .update(cx, |root, cx| root.pair_with_host(host, window, cx));
    }
}
