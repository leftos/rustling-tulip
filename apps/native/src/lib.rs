//! Native (GPUI) rustling-tulip client: the daemon's tabs and split panes for
//! this client, each pane a session rendered with `alacritty_terminal`,
//! beside a sidebar of every session.
//!
//! The binary opens [`open_main_window`]; the UI specs build a [`RootView`]
//! over their own transport with [`RootView::with_transport`].

mod connection;
mod footer;
mod grid_view;
mod keys;
mod mouse;
mod net;
mod scrollback_load;
mod sidebar;
mod sidebar_view;
mod tab_bar;
mod tabs;
mod term;
mod term_input;
mod term_view;
mod text_input;

use alacritty_terminal::vte::ansi::CursorShape;
use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{
    Animation, AnimationExt as _, AnyElement, AnyView, App, Bounds, ClickEvent, ClipboardItem,
    Context, CursorStyle, Div, ElementId, FocusHandle, FontWeight, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, SharedString, Stateful, Window,
    WindowBounds, WindowOptions, div, prelude::*, pulsating_between, px, size,
};
use protocol::{ClientMessage, DaemonMessage, InitLayoutKind, SessionSnapshot, TabEntry};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::connection::{DotKind, Footer};
use crate::footer::{StopConfirm, flyout_rows, log_paths};
use crate::grid_view::{PaneSlot, RetryGate, divider_ratio};
use crate::sidebar::{SidebarModel, UiState, can_attach, load_ui_state, save_ui_state};
use crate::tab_bar::Rename;
use crate::tabs::{PaneTarget, Placement, TabsModel, find_tab_containing_session};

pub use crate::connection::Connection;
pub use crate::footer::LogPaths;
pub use crate::net::{HandshakeInfo, NetCommand, NetEvent};
pub use crate::sidebar::{Container, ContainerKind, Leaf};
pub use crate::text_input::bind_keys;

const PADDING: f32 = 6.0;
/// Thickness of the drag handles between the sidebar and the tabs, and
/// between split panes.
const DIVIDER_WIDTH: f32 = 4.0;
/// This client's log file, under `<config dir>/logs/`.
pub const LOG_FILE: &str = "native.log";

/// Where the terminals read the time, for their scrollback timeouts.
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// What the root view talks to and where it keeps its files.
pub struct RootDeps {
    /// Commands for the network thread (or a spec's fake daemon).
    pub tx: UnboundedSender<NetCommand>,
    /// What the network thread (or a spec) reports.
    pub events: UnboundedReceiver<NetEvent>,
    /// Where `native-ui.json` is loaded from and saved to; `None` keeps the
    /// layout in memory only.
    pub ui_dir: Option<PathBuf>,
    /// The flyout's files, or why the config dir could not be resolved.
    pub paths: Result<LogPaths, String>,
    /// The session to focus once the layout and the sessions arrive.
    pub wanted: Option<String>,
    pub now: Clock,
}

/// Opens the client's window on a live daemon connection, focusing
/// `wanted_session` once it arrives.
pub fn open_main_window(wanted_session: Option<String>, cx: &mut App) {
    bind_keys(cx);
    let bounds = Bounds::centered(None, size(px(1000.0), px(640.0)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        move |window, cx| cx.new(|cx| RootView::new(wanted_session, window, cx)),
    );
    if let Err(err) = opened {
        tracing::error!("opening window: {err:#}");
        cx.quit();
        return;
    }
    cx.activate(true);
}

/// Text size of the footer, flyout and overlay.
const UI_TEXT_SIZE: f32 = 12.0;
const FOOTER_HEIGHT: f32 = 22.0;
const BAR_BG: u32 = 0x0025_2526;
const PANEL_BG: u32 = 0x000f_1014;
const OVERLAY_BG: u32 = 0x001e_1e1e;
const HOVER_BG: u32 = 0x002d_2f36;
const BORDER: u32 = 0x0020_222a;
const TEXT: u32 = 0x00cc_cccc;
const MUTED: u32 = 0x009a_9a9a;
const DANGER: u32 = 0x00ef_5c5c;
const DANGER_BG: u32 = 0x003a_1c1f;

/// What a press on a drag handle is resizing.
enum Drag {
    Sidebar,
    /// A split of `tab_id`; `ratio` is where the drag has taken it so far.
    Divider {
        tab_id: String,
        split_path: Vec<u8>,
        ratio: Option<f32>,
    },
}

/// The window's content: sidebar, tabs and panes, footer and overlays.
pub struct RootView {
    tx: UnboundedSender<NetCommand>,
    /// The terminals' clock.
    now: Clock,
    /// The connection state machine as the network thread last reported it.
    conn: Connection,
    handshake: Option<HandshakeInfo>,
    /// Repos, workspaces and every session the daemon knows, plus the
    /// sidebar layout.
    sidebar: SidebarModel,
    /// The tab list, the active tab and each tab's focused pane.
    tabs: TabsModel,
    /// A terminal for every pane of every tab, by pane id.
    panes: HashMap<String, PaneSlot>,
    /// Scrollback retries already sent, so each goes out once per session.
    retries: RetryGate,
    /// Where the active tab's split tree was last laid out; divider drags
    /// map the pointer through it.
    grid_bounds: Option<Bounds<Pixels>>,
    /// The tab whose name is being edited.
    renaming: Option<Rename>,
    /// Where the sidebar layout is saved; `None` when the config dir could
    /// not be resolved.
    ui_dir: Option<PathBuf>,
    drag: Option<Drag>,
    /// Focus for the sidebar, which takes it on a click so Ctrl+B there
    /// toggles the sidebar instead of reaching the terminal.
    sidebar_focus: FocusHandle,
    /// The session named on the command line, focused once the tabs and
    /// the sessions have arrived.
    wanted_session: Option<String>,
    /// The daemon has sent its session list.
    sessions_loaded: bool,
    /// Why a request could not be carried out.
    status: String,
    /// Whether the daemon troubleshooting flyout is open.
    flyout_open: bool,
    /// The flyout's two-click stop.
    stop: StopConfirm,
    /// The handshake path was copied since the flyout opened.
    copied: bool,
    /// The flyout's files, or why the config dir could not be resolved.
    paths: Result<LogPaths, String>,
}

impl RootView {
    /// A view on the live daemon: the network thread, the config dir's
    /// layout and files, and the system clock.
    pub fn new(
        wanted_session: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (out_tx, out_rx) = unbounded();
        let (in_tx, in_rx) = unbounded();
        net::spawn(out_rx, in_tx);
        let config_dir = daemon_client::config_dir();
        let ui_dir = match &config_dir {
            Ok(dir) => Some(dir.clone()),
            Err(err) => {
                tracing::warn!("sidebar layout will not be saved: {err:#}");
                None
            }
        };
        let deps = RootDeps {
            tx: out_tx,
            events: in_rx,
            ui_dir,
            paths: config_dir
                .map(|dir| log_paths(&dir))
                .map_err(|err| format!("config folder unavailable: {err:#}")),
            wanted: wanted_session,
            now: Arc::new(Instant::now),
        };
        Self::with_transport(deps, window, cx)
    }

    /// A view over `deps`: it sends through `deps.tx` and handles every
    /// event `deps.events` delivers.
    pub fn with_transport(deps: RootDeps, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let RootDeps {
            tx,
            mut events,
            ui_dir,
            paths,
            wanted,
            now,
        } = deps;
        cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = events.next().await {
                if this
                    .update_in(cx, |view, window, cx| view.on_net(event, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let ui = ui_dir
            .as_deref()
            .map_or_else(UiState::default, load_ui_state);
        Self {
            tx,
            now,
            conn: Connection::new(),
            handshake: None,
            tabs: TabsModel::new(ui.active_tab_id.clone()),
            sidebar: SidebarModel::new(ui),
            panes: HashMap::new(),
            retries: RetryGate::default(),
            grid_bounds: None,
            renaming: None,
            ui_dir,
            drag: None,
            sidebar_focus: cx.focus_handle(),
            wanted_session: wanted,
            sessions_loaded: false,
            status: String::new(),
            flyout_open: false,
            stop: StopConfirm::default(),
            copied: false,
            paths,
        }
    }

    /// The sidebar's containers, in the order it shows them.
    #[must_use]
    pub fn sidebar_containers(&self) -> Vec<Container> {
        self.sidebar.containers()
    }

    /// Whether the sidebar is hidden.
    #[must_use]
    pub fn sidebar_collapsed(&self) -> bool {
        self.sidebar.is_collapsed()
    }

    /// The tab on screen.
    #[must_use]
    pub fn active_tab_id(&self) -> Option<&str> {
        self.tabs.active_id()
    }

    /// Every tab's id, in strip order.
    #[must_use]
    pub fn tab_ids(&self) -> Vec<String> {
        self.tabs.tabs().iter().map(|tab| tab.id.clone()).collect()
    }

    /// The panes of the tab on screen.
    #[must_use]
    pub fn active_pane_ids(&self) -> Vec<String> {
        self.tabs
            .active_tab()
            .and_then(TabEntry::grid)
            .map(|grid| {
                crate::tabs::collect_panes(grid)
                    .into_iter()
                    .map(|pane| pane.id.to_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The tab whose name is being edited.
    #[must_use]
    pub fn renaming_tab(&self) -> Option<&str> {
        self.renaming.as_ref().map(Rename::tab_id)
    }

    /// Pane `pane_id`'s visible screen, one string per row with trailing
    /// blanks trimmed.
    #[must_use]
    pub fn pane_grid_text(&self, pane_id: &str, cx: &App) -> Option<Vec<String>> {
        let slot = self.panes.get(pane_id)?;
        Some(slot.view().read(cx).grid_text())
    }

    /// The cursor shape pane `pane_id` renders.
    #[must_use]
    pub fn pane_cursor_shape(&self, pane_id: &str, cx: &App) -> Option<CursorShape> {
        let slot = self.panes.get(pane_id)?;
        Some(slot.view().read(cx).cursor_shape())
    }

    /// The window position of the centre of cell (`col`, `row`) in pane
    /// `pane_id`, as last laid out.
    #[must_use]
    pub fn pane_cell_center(
        &self,
        pane_id: &str,
        col: usize,
        row: usize,
        cx: &App,
    ) -> Option<Point<Pixels>> {
        self.panes
            .get(pane_id)?
            .view()
            .read(cx)
            .cell_center(col, row)
    }

    fn save_ui(&self) {
        let Some(dir) = &self.ui_dir else {
            return;
        };
        if let Err(err) = save_ui_state(dir, self.sidebar.ui_state()) {
            tracing::warn!("saving the sidebar layout: {err:#}");
        }
    }

    /// Hide or show the sidebar; hiding it hands the keyboard back to the
    /// active tab's focused pane, since the sidebar may have held it.
    fn toggle_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar.toggle_sidebar();
        self.drag = None;
        self.save_ui();
        if self.sidebar.is_collapsed() {
            self.focus_active_pane(window, cx);
        }
        cx.notify();
    }

    fn toggle_container(&mut self, key: &str) {
        self.sidebar.toggle_container(key);
        self.save_ui();
    }

    /// A leaf click: show the pane holding the session, or place the
    /// session when no pane shows it. A session without a terminal only has
    /// its attention cleared.
    fn select_session(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar.clear_attention(id);
        if let Some((tab_id, pane_id)) = find_tab_containing_session(self.tabs.tabs(), id) {
            self.tabs.focus_pane(&tab_id, &pane_id);
            self.after_tabs_change(window, cx);
        } else if let Some(session) = self.sidebar.session(id).filter(|s| can_attach(s)).cloned() {
            self.place_session(&session, window, cx);
        }
        cx.notify();
    }

    /// Sends the session to where smart placement puts it. A filled empty
    /// pane is focused now; a new pane or tab takes focus when the daemon's
    /// update arrives.
    fn place_session(
        &mut self,
        session: &SessionSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let placement = self.tabs.place(session, self.sidebar.sessions());
        match &placement {
            Placement::NewTab => self.tabs.arm_create(),
            Placement::Pane {
                tab_id,
                target: PaneTarget::Replace { pane_id },
            } => {
                self.tabs.focus_pane(tab_id, pane_id);
                self.after_tabs_change(window, cx);
            }
            Placement::Pane { .. } => {}
        }
        self.send(placement.message(&session.id));
    }

    /// Focuses the command line's session once, when the layout and the
    /// session list are both in.
    fn try_wanted_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !(self.tabs.is_loaded() && self.sessions_loaded) {
            return;
        }
        let Some(id) = self.wanted_session.take() else {
            return;
        };
        if self.sidebar.session(&id).is_none() {
            self.status = format!("session {id} not found");
            return;
        }
        self.select_session(&id, window, cx);
    }

    /// After any change to the tab model: terminals follow the layout, the
    /// requested pane takes the keyboard and the active tab is saved.
    fn after_tabs_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reconcile_panes(window, cx);
        if let Some(pane_id) = self.tabs.take_focus_request() {
            self.focus_pane_view(&pane_id, window, cx);
        }
        if self.sidebar.set_active_tab(self.tabs.active_id()) {
            self.save_ui();
        }
        if self
            .renaming
            .as_ref()
            .is_some_and(|rename| self.tabs.tab(rename.tab_id()).is_none())
        {
            self.renaming = None;
        }
        self.try_wanted_session(window, cx);
        cx.notify();
    }

    fn start_drag(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.drag = Some(Drag::Sidebar);
        cx.stop_propagation();
    }

    /// Follows a drag: the sidebar width, or a split's ratio (kept locally
    /// until the release sends it).
    fn on_drag_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.drag.is_none() {
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            self.finish_drag();
            cx.notify();
            return;
        }
        let at = (event.position.x / px(1.0), event.position.y / px(1.0));
        match &mut self.drag {
            Some(Drag::Sidebar) => {
                let window_width = window.viewport_size().width / px(1.0);
                self.sidebar.set_width(at.0, window_width);
            }
            Some(Drag::Divider {
                tab_id,
                split_path,
                ratio,
            }) => {
                if let Some(next) =
                    divider_ratio(&self.tabs, self.grid_bounds, tab_id, split_path, at)
                {
                    self.tabs.set_ratio(tab_id, split_path, next);
                    *ratio = Some(next);
                }
            }
            None => {}
        }
        cx.notify();
    }

    fn on_drag_end(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.drag.is_some() {
            self.finish_drag();
            cx.notify();
        }
    }

    /// Ends a drag: the sidebar width is saved; a moved divider's ratio goes
    /// to the daemon.
    fn finish_drag(&mut self) {
        match self.drag.take() {
            Some(Drag::Sidebar) => self.save_ui(),
            Some(Drag::Divider {
                tab_id,
                split_path,
                ratio: Some(ratio),
            }) => self.send(ClientMessage::SetPaneRatio {
                tab_id,
                split_path,
                ratio,
            }),
            Some(Drag::Divider { ratio: None, .. }) | None => {}
        }
    }

    fn command(&self, command: NetCommand) {
        // Fails only once the network thread has exited, which it does only
        // after this view drops its sender.
        let _ = self.tx.unbounded_send(command);
    }

    fn send(&self, msg: ClientMessage) {
        self.command(NetCommand::Send(Box::new(msg)));
    }

    fn toggle_flyout(&mut self) {
        if self.flyout_open {
            self.close_flyout();
        } else {
            self.flyout_open = true;
        }
    }

    fn close_flyout(&mut self) {
        self.flyout_open = false;
        self.stop.reset();
        self.copied = false;
    }

    /// Restart the daemon. The old daemon's handshake is dropped so the
    /// flyout never shows its port, pid or protocol; the next ensure brings
    /// the new one.
    fn restart(&mut self) {
        self.command(NetCommand::Restart);
        self.handshake = None;
        self.close_flyout();
    }

    fn click_stop(&mut self) {
        if self.stop.click() {
            self.command(NetCommand::Stop);
            self.handshake = None;
            self.close_flyout();
        }
    }

    fn on_net(&mut self, event: NetEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            NetEvent::State(conn) => self.conn = conn,
            NetEvent::Handshake(info) => self.handshake = Some(info),
            NetEvent::Message(msg) => self.on_message(*msg, window, cx),
        }
        // The overlay covers the footer; a flyout left open under it would
        // reappear (possibly armed) when the overlay goes.
        if self.conn.overlay().is_some() {
            self.close_flyout();
        }
        cx.notify();
    }

    fn on_message(&mut self, msg: DaemonMessage, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar.apply(&msg);
        if self.tabs.apply(&msg) {
            self.after_tabs_change(window, cx);
            return;
        }
        match msg {
            DaemonMessage::Welcome { .. } => {
                self.reset_panes(cx);
                self.status.clear();
            }
            DaemonMessage::LayoutInitRequired {
                active_session_count,
                ..
            } => {
                tracing::info!(
                    active_session_count,
                    "first connect for this client: seeding the layout with every running session"
                );
                self.send(ClientMessage::InitLayout {
                    kind: InitLayoutKind::AllSessions,
                });
            }
            DaemonMessage::Sessions { sessions } => {
                self.sessions_loaded = true;
                for view in self.pane_views(None) {
                    view.update(cx, |pane, _| pane.refresh_sessions(&sessions));
                }
                self.attach_waiting_panes(cx);
                self.try_wanted_session(window, cx);
            }
            DaemonMessage::Scrollback {
                session_id,
                data_b64,
                truncated,
            } => self.feed_panes(&session_id, cx, |pane| {
                pane.on_scrollback(&data_b64, truncated)
            }),
            DaemonMessage::PtyOutput {
                session_id,
                data_b64,
            } => self.feed_panes(&session_id, cx, |pane| pane.on_pty_output(&data_b64)),
            DaemonMessage::SessionUpdated { session } => {
                for view in self.pane_views(Some(&session.id)) {
                    view.update(cx, |pane, _| pane.update_session(&session));
                }
                self.attach_waiting_panes(cx);
            }
            _ => {}
        }
    }

    /// Keys the root takes before the panes see them. Esc drops an armed
    /// tab close (and still reaches the pane). Ctrl+B toggles the sidebar
    /// only when no terminal and no tab rename has the keyboard; in a
    /// terminal it is the PTY's 0x02.
    fn on_key_capture(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ks = &event.keystroke;
        if ks.key == "escape" && self.tabs.close_confirm.disarm() {
            cx.notify();
        }
        let ctrl_only = ks.modifiers.control && !ks.modifiers.shift && !ks.modifiers.alt;
        if self.flyout_open && ks.key == "escape" {
            self.close_flyout();
        } else if ctrl_only
            && ks.key == "b"
            && self.renaming.is_none()
            && !self.terminal_focused(window, cx)
        {
            self.toggle_sidebar(window, cx);
        } else {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// A press anywhere that did not stop at a tab's close button drops its
    /// armed close.
    fn on_any_mouse_down(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.close_confirm.disarm() {
            cx.notify();
        }
    }

    /// The footer's text: the last problem, else the focused session.
    fn footer_status(&self) -> String {
        if !self.status.is_empty() {
            return self.status.clone();
        }
        self.focused_session()
            .map(|id| format!("{} · {id}", self.session_label(&id)))
            .unwrap_or_default()
    }

    /// The name the sidebar shows for a session, or its id when unknown.
    fn session_label(&self, id: &str) -> String {
        self.sidebar.session(id).map_or_else(
            || id.to_owned(),
            |s| s.user_label.clone().unwrap_or_else(|| s.label.clone()),
        )
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let footer = self.conn.footer(self.sidebar.sessions().len());
        let flyout = self
            .flyout_open
            .then(|| [flyout_backdrop(cx).into_any_element(), self.flyout(cx)]);
        let overlay = self
            .conn
            .overlay()
            .map(|text| connecting_overlay(text, footer.dot, cx));

        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .capture_key_down(cx.listener(Self::on_key_capture))
            .on_any_mouse_down(cx.listener(Self::on_any_mouse_down))
            .on_mouse_move(cx.listener(Self::on_drag_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_drag_end))
            .child(self.main_row(window, cx))
            .child(self.footer_bar(&footer, cx))
            .children(flyout.into_iter().flatten())
            .children(overlay)
    }
}

/// A handle a press starts dragging: a vertical line between side-by-side
/// parts, or a horizontal one between stacked parts.
fn drag_handle(id: impl Into<ElementId>, vertical_line: bool, active: bool) -> Stateful<Div> {
    let handle = div()
        .id(id)
        .flex_none()
        .bg(gpui::rgb(BORDER))
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .when(active, |handle| handle.bg(gpui::rgb(HOVER_BG)));
    if vertical_line {
        handle
            .w(px(DIVIDER_WIDTH))
            .h_full()
            .cursor(CursorStyle::ResizeLeftRight)
    } else {
        handle
            .h(px(DIVIDER_WIDTH))
            .w_full()
            .cursor(CursorStyle::ResizeUpDown)
    }
}

impl RootView {
    /// The bottom bar: the daemon pill, then the attached session's status.
    fn footer_bar(&self, footer: &Footer, cx: &mut Context<Self>) -> Div {
        let text = match footer.port {
            Some(port) => format!("daemon · {} · :{port}", footer.label),
            None => format!("daemon · {}", footer.label),
        };
        let pill = div()
            .id("daemon-pill")
            .flex()
            .items_center()
            .gap(px(6.0))
            .h_full()
            .px(px(8.0))
            .cursor_pointer()
            .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
            .when(self.flyout_open, |pill| pill.bg(gpui::rgb(HOVER_BG)))
            .child(status_dot(footer.dot, "footer-dot"))
            .child(text)
            .tooltip(tooltip(footer.tooltip.clone()))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.toggle_flyout();
                cx.notify();
            }));
        div()
            .flex()
            .flex_none()
            .items_center()
            .h(px(FOOTER_HEIGHT))
            .bg(gpui::rgb(BAR_BG))
            .border_t_1()
            .border_color(gpui::rgb(BORDER))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(MUTED))
            .child(pill)
            .child(div().px(px(PADDING)).child(self.footer_status()))
    }

    /// The troubleshooting flyout above the pill: details, files, control.
    fn flyout(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows = flyout_rows(
            &self.conn,
            self.handshake.as_ref(),
            self.sidebar.sessions().len(),
        );
        let details = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(
                rows.into_iter()
                    .map(|(label, value)| detail_row(label, value)),
            )
            .child(self.handshake_row(cx));
        div()
            .id("daemon-flyout")
            .absolute()
            .left(px(PADDING))
            .bottom(px(FOOTER_HEIGHT + 4.0))
            .w(px(320.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(10.0))
            .bg(gpui::rgb(PANEL_BG))
            .border_1()
            .border_color(gpui::rgb(BORDER))
            .rounded(px(6.0))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(TEXT))
            .occlude()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Daemon"))
            .child(details)
            .child(self.files_section())
            .child(self.control_section(cx))
            .into_any_element()
    }

    /// The "Handshake file" row with its copy button.
    fn handshake_row(&self, cx: &mut Context<Self>) -> Div {
        let label = if self.copied { "copied" } else { "copy" };
        let button = action_button("copy-handshake", label, TEXT, self.paths.is_ok());
        let button = match &self.paths {
            Ok(paths) => {
                let path = paths.handshake.display().to_string();
                button.tooltip(tooltip(path.clone())).on_click(cx.listener(
                    move |this, _: &ClickEvent, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                        this.copied = true;
                        cx.notify();
                    },
                ))
            }
            Err(_) => button,
        };
        detail_row("Handshake file", button)
    }

    /// "Logs & files": open the logs, reveal the config folder; disabled with
    /// the error when the config dir could not be resolved.
    fn files_section(&self) -> Div {
        let section = section("Logs & files");
        match &self.paths {
            Ok(paths) => section
                .child(open_button(
                    "open-daemon-log",
                    "Open daemon.log",
                    &paths.daemon_log,
                ))
                .child(open_button(
                    "open-native-log",
                    "Open native.log",
                    &paths.native_log,
                ))
                .child({
                    let dir = paths.config_dir.clone();
                    action_button("reveal-config", "Reveal config folder", TEXT, true)
                        .on_click(move |_, _, cx| cx.reveal_path(&dir))
                }),
            Err(err) => section
                .child(action_button(
                    "open-daemon-log",
                    "Open daemon.log",
                    TEXT,
                    false,
                ))
                .child(action_button(
                    "open-native-log",
                    "Open native.log",
                    TEXT,
                    false,
                ))
                .child(action_button(
                    "reveal-config",
                    "Reveal config folder",
                    TEXT,
                    false,
                ))
                .child(div().text_color(gpui::rgb(DANGER)).child(err.clone())),
        }
    }

    /// "Control": restart, and the two-click stop.
    fn control_section(&self, cx: &mut Context<Self>) -> Div {
        let armed = self.stop.armed;
        section("Control")
            .child(
                action_button("restart-daemon", "Restart daemon", TEXT, true).on_click(
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.restart();
                        cx.notify();
                    }),
                ),
            )
            .child(
                action_button("stop-daemon", self.stop.label(), DANGER, true)
                    .when(armed, |button| button.bg(gpui::rgb(DANGER_BG)))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.click_stop();
                        cx.notify();
                    })),
            )
    }
}

/// A transparent full-window layer under the flyout; a click on it (the pill
/// included) closes the flyout.
fn flyout_backdrop(cx: &mut Context<RootView>) -> Stateful<Div> {
    div()
        .id("flyout-backdrop")
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .occlude()
        .on_any_mouse_down(cx.listener(|this, _: &MouseDownEvent, _, cx| {
            this.close_flyout();
            cx.stop_propagation();
            cx.notify();
        }))
}

/// The full-window card shown until the first connect, with a restart link.
fn connecting_overlay(
    text: &'static str,
    dot: DotKind,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let restart = div()
        .id("overlay-restart")
        .text_size(px(UI_TEXT_SIZE))
        .text_color(gpui::rgb(MUTED))
        .cursor_pointer()
        .hover(|style| style.text_color(gpui::rgb(TEXT)))
        .child("Restart daemon")
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            this.restart();
            cx.notify();
        }));
    let card = div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(16.0))
        .px(px(40.0))
        .py(px(32.0))
        .min_w(px(280.0))
        .bg(gpui::rgb(PANEL_BG))
        .border_1()
        .border_color(gpui::rgb(BORDER))
        .rounded(px(6.0))
        .child(status_dot(dot, "overlay-dot"))
        .child(div().text_color(gpui::rgb(TEXT)).child(text))
        .child(restart);
    div()
        .id("connecting-overlay")
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::rgb(OVERLAY_BG))
        .occlude()
        .child(card)
}

/// The status dot: pulsing while pending, dimmed when stopped.
fn status_dot(dot: DotKind, id: impl Into<ElementId>) -> AnyElement {
    let base = div()
        .flex_none()
        .size(px(8.0))
        .rounded_full()
        .bg(gpui::rgb(dot_color(dot)));
    match dot {
        DotKind::Pending => base
            .with_animation(
                id,
                Animation::new(Duration::from_millis(1400))
                    .repeat()
                    .with_easing(pulsating_between(0.35, 1.0)),
                Styled::opacity,
            )
            .into_any_element(),
        DotKind::Stopped => base.opacity(0.55).into_any_element(),
        DotKind::Ok | DotKind::Idle | DotKind::Err => base.into_any_element(),
    }
}

/// The footer dot's colour, from the Tauri app's status tokens.
fn dot_color(dot: DotKind) -> u32 {
    match dot {
        DotKind::Ok => 0x003f_b96a,
        DotKind::Pending => 0x00e8_a531,
        DotKind::Idle | DotKind::Stopped => 0x0083_8a96,
        DotKind::Err => 0x00ef_5c5c,
    }
}

/// A flyout detail row: a muted label, the value on the right.
fn detail_row(label: &'static str, value: impl IntoElement) -> Div {
    div()
        .flex()
        .justify_between()
        .items_center()
        .gap(px(12.0))
        .child(div().text_color(gpui::rgb(MUTED)).child(label))
        .child(value)
}

/// A flyout section: a top border and a muted heading.
fn section(label: &'static str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .pt(px(6.0))
        .border_t_1()
        .border_color(gpui::rgb(BORDER))
        .child(div().text_color(gpui::rgb(MUTED)).child(label))
}

/// A clickable text button; a disabled one is dimmed and takes no clicks.
fn action_button(
    id: &'static str,
    label: &'static str,
    color: u32,
    enabled: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .px(px(6.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .text_color(gpui::rgb(color))
        .when(enabled, |button| {
            button
                .cursor_pointer()
                .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        })
        .when(!enabled, |button| button.opacity(0.5))
        .child(label)
}

/// A button that opens `path` with the system's default application.
fn open_button(id: &'static str, label: &'static str, path: &Path) -> Stateful<Div> {
    let path: PathBuf = path.to_path_buf();
    action_button(id, label, TEXT, true).on_click(move |_, _, cx| cx.open_with_system(&path))
}

/// A plain text tooltip.
struct Tip(SharedString);

impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(6.0))
            .py(px(2.0))
            .bg(gpui::rgb(PANEL_BG))
            .border_1()
            .border_color(gpui::rgb(BORDER))
            .rounded(px(4.0))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(TEXT))
            .child(self.0.clone())
    }
}

fn tooltip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = text.into();
    move |_, cx| cx.new(|_| Tip(text.clone())).into()
}
