//! Native (GPUI) rustling-tulip client: the daemon's tabs and split panes for
//! this client, each pane a session rendered with `alacritty_terminal`,
//! beside a sidebar of every session.
//!
//! The binary opens [`open_main_window`]; the UI specs build a [`RootView`]
//! over their own transport with [`RootView::with_transport`].

pub mod appearance;
mod branch_fate;
mod connection;
mod copied;
pub mod fonts;
mod footer;
mod grid_view;
mod keys;
mod links;
mod mouse;
mod net;
mod notice_view;
mod notices;
mod open;
mod open_view;
mod quit;
mod quit_view;
mod run_confirm;
mod scrollback_load;
mod session_actions;
mod session_menu;
mod shell_dialog;
mod shell_marks;
mod shell_view;
mod sidebar;
mod sidebar_view;
mod source_control;
mod spawn_form;
mod spawn_view;
mod spawns;
mod tab_bar;
mod tabs;
mod term;
mod term_input;
mod term_view;
mod text_input;
mod theme;

use alacritty_terminal::vte::ansi::CursorShape;
use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::{
    Animation, AnimationExt as _, AnyElement, AnyView, App, Bounds, ClickEvent, Context,
    CursorStyle, Div, ElementId, ElementInputHandler, FocusHandle, FontWeight, InputHandler,
    KeyDownEvent, Keystroke, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, SharedString, Stateful, Task, Window, WindowBounds, WindowOptions,
    div, prelude::*, pulsating_between, px, size,
};
use protocol::{
    AppearanceOverrides, ClientMessage, DaemonMessage, InitLayoutKind, SessionSnapshot, TabEntry,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::appearance::AppearanceChange;
use crate::connection::{DotKind, Footer};
use crate::copied::Copied;
use crate::footer::{StopConfirm, flyout_rows, log_paths};
use crate::grid_view::{PaneSlot, RetryGate, divider_ratio};
use crate::notices::Notices;
use crate::open::SystemOpener;
use crate::quit_view::{ExitView, Quitter};
use crate::run_confirm::RunConfirm;
use crate::session_actions::{Duplicates, HeaderStopConfirm};
use crate::session_menu::{DeleteDialog, SessionMenu, ShellMenu};
use crate::shell_dialog::PendingQuickShell;
use crate::shell_view::ShellDialog;
use crate::sidebar::{SidebarModel, UiState, can_attach, load_ui_state, save_ui_state};
use crate::spawn_form::BranchCache;
use crate::spawn_view::SpawnDialog;
use crate::spawns::PendingSpawns;
use crate::tab_bar::{Rename, TabMenu};
use crate::tabs::{PaneTarget, Placement, TabsModel, find_tab_containing_session};
use crate::term::ShellCommand;
use crate::term_view::ScrollbackReply;

pub use crate::connection::Connection;
pub use crate::footer::LogPaths;
pub use crate::net::{
    EnsureFuture, HandshakeInfo, NATIVE_PROTOCOL_VERSIONS, NetCommand, NetDeps, NetEvent,
    StopFuture, spawn_with as spawn_net,
};
pub use crate::notices::{
    ActionFailedNotice, CheckoutChoice, CheckoutPrompt, TOAST_LIFETIME, Toast, ToastKind,
};
pub use crate::open::{OpenFailure, Opener};
pub use crate::quit_view::QuitFn;
pub use crate::shell_marks::{ShellDot, ShellStatus};
pub use crate::sidebar::{Container, ContainerKind, DEFAULT_WIDTH as SIDEBAR_DEFAULT_WIDTH, Leaf};
pub use crate::spawns::{OpenIn, PaneAim};
pub use crate::text_input::bind_keys;

const PADDING: f32 = 6.0;
/// Thickness of the drag handles between the sidebar and the tabs, and
/// between split panes.
const DIVIDER_WIDTH: f32 = 4.0;
/// How long tab font steps wait before the layout is written out.
const FONT_SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
/// The toast a session appearance change the daemon refused raises.
const APPEARANCE_FAILED_TITLE: &str = "Couldn't change the appearance";
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
    /// Quits the app once the quit flow is done.
    pub quit: QuitFn,
    /// Opens the links Ctrl+click picks in the terminals, on background
    /// threads.
    pub open: Arc<dyn Opener>,
}

/// Opens the client's window on a live daemon connection, focusing
/// `wanted_session` once it arrives.
pub fn open_main_window(wanted_session: Option<String>, cx: &mut App) {
    bind_keys(cx);
    fonts::register_bundled(cx);
    let offscreen = std::env::var_os(OFFSCREEN_ENV).is_some_and(|value| !value.is_empty());
    let (width, height) = WINDOW_SIZE;
    let bounds = Bounds::centered(None, size(px(width.into()), px(height.into())), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // gpui activates any window it shows, so a cloaked window is
            // opened hidden and shown by `show_cloaked`.
            focus: !offscreen,
            show: !offscreen,
            ..Default::default()
        },
        move |window, cx| {
            if offscreen {
                show_cloaked(window);
            }
            cx.new(|cx| RootView::new(wanted_session, window, cx))
        },
    );
    if let Err(err) = opened {
        tracing::error!("opening window: {err:#}");
        cx.quit();
        return;
    }
    if !offscreen {
        cx.activate(true);
    }
}

/// The main window's size in logical pixels.
const WINDOW_SIZE: (u16, u16) = (1000, 640);

/// When set to a non-empty value, the window opens cloaked and is never
/// activated, so the smoke specs never cover or take focus from the user's
/// work. This mode must never call `activate`: gpui's activate path sends a
/// synthetic Alt through `SendInput` and calls `SetForegroundWindow`, which
/// would reach the user's keyboard state and take their focus.
const OFFSCREEN_ENV: &str = "RUSTLING_TULIP_OFFSCREEN_WINDOW";

/// Places `window` at the primary display's work-area origin at its full
/// size (gpui applies neither to a window it opens hidden), cloaks it and
/// shows it without activating it. The compositor still renders a cloaked
/// window, so its pixels can be read, but the user never sees it. A window
/// that cannot be cloaked stays hidden.
#[cfg(windows)]
fn show_cloaked(window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // `Window::window_handle` is gpui's own handle; the raw one is the trait's.
    let raw = HasWindowHandle::window_handle(window).map(|handle| handle.as_raw());
    let address = match raw {
        Ok(RawWindowHandle::Win32(handle)) => handle.hwnd.get().cast_unsigned(),
        other => {
            tracing::error!("{OFFSCREEN_ENV}: the window has no Win32 handle ({other:?})");
            return;
        }
    };
    // Moving and showing the window sends it messages that gpui handles at
    // once. Sent from here, inside gpui's own callback, they find its state
    // borrowed and are lost, so the window never learns its size. Sent from
    // another thread, they wait for gpui's message loop.
    let spawned = std::thread::Builder::new()
        .name("cloak-window".to_owned())
        .spawn(move || cloak_and_show(address));
    if let Err(err) = spawned {
        tracing::error!("{OFFSCREEN_ENV}: starting the thread that shows the window: {err}");
    }
}

/// The work of [`show_cloaked`] on the window whose handle is `address`.
#[cfg(windows)]
fn cloak_and_show(address: usize) {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::{
        SPI_GETWORKAREA, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOZORDER,
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetWindowPos, ShowWindow, SystemParametersInfoW,
    };
    use windows::core::BOOL;
    let hwnd = HWND(std::ptr::with_exposed_provenance_mut(address));
    let mut work_area = RECT::default();
    let cloak = BOOL::from(true);
    let flags = SWP_NOZORDER | SWP_NOACTIVATE;
    // SAFETY: a plain query on this live window; 0 means it has no DPI.
    let dpi = match unsafe { GetDpiForWindow(hwnd) } {
        0 => 96,
        dpi => i32::try_from(dpi).unwrap_or(96),
    };
    let scaled = |logical: u16| i32::from(logical) * dpi / 96;
    let (width, height) = (scaled(WINDOW_SIZE.0), scaled(WINDOW_SIZE.1));
    // SAFETY: `hwnd` is this live window's handle and the pointers are to
    // locals that outlive the calls; none of the calls activates the window.
    unsafe {
        let area = (&raw mut work_area).cast();
        let none = SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0);
        if let Err(err) = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(area), none) {
            tracing::warn!("{OFFSCREEN_ENV}: reading the primary work area: {err}");
        }
        let (left, top) = (work_area.left, work_area.top);
        if let Err(err) = SetWindowPos(hwnd, None, left, top, width, height, flags) {
            tracing::error!("{OFFSCREEN_ENV}: moving the window onto the work area: {err}");
        }
        let cloak_ptr = (&raw const cloak).cast();
        let cloak_size = u32::try_from(size_of::<BOOL>()).unwrap_or(4);
        if let Err(err) = DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, cloak_ptr, cloak_size) {
            tracing::error!("{OFFSCREEN_ENV}: cloaking the window, so it stays hidden: {err}");
            return;
        }
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    tracing::info!("{OFFSCREEN_ENV}: the window is cloaked, never activated");
}

#[cfg(not(windows))]
fn show_cloaked(_window: &Window) {
    tracing::warn!("{OFFSCREEN_ENV} is only honoured on Windows; the window stays hidden");
}

/// Text size of the footer, flyout and overlay.
const UI_TEXT_SIZE: f32 = 12.0;
/// The footer's height in logical pixels.
pub const FOOTER_HEIGHT: f32 = 22.0;
/// Background of the footer and the tab bar, as `0xRRGGBB`.
pub const BAR_BG: u32 = 0x0025_2526;
/// Background of the sidebar and panels, as `0xRRGGBB`.
pub const PANEL_BG: u32 = 0x000f_1014;
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
    /// The last copy, while its "copied" chip is up.
    copy_chip: Copied,
    /// Wakes the view when the chip's time is up.
    chip_timer: Option<Task<()>>,
    /// The flyout's files, or why the config dir could not be resolved.
    paths: Result<LogPaths, String>,
    /// The session context menu, while open.
    menu: Option<SessionMenu>,
    /// The gutter-dot command menu, while open.
    shell_menu: Option<ShellMenu>,
    /// The open menu's keyboard focus, so Esc reaches it.
    menu_focus: FocusHandle,
    /// The tab context menu, while open.
    tab_menu: Option<TabMenu>,
    /// The open tab menu's keyboard focus, so Esc reaches it.
    tab_menu_focus: FocusHandle,
    /// Each session's appearance changes the client sent and the daemon has
    /// not answered, keyed by request. Every send starts from the stored
    /// appearance with these over it, so a held key steps from where its
    /// last press left off and one change does not undo another still on
    /// its way.
    pending_appearance: appearance::InFlightAppearance,
    /// When the pending tab-font save is due, if one is.
    font_save_deadline: Option<Instant>,
    /// Wakes the view when the tab-font save is due.
    font_save_timer: Option<Task<()>>,
    /// A header Stop waiting for its second click.
    confirm: HeaderStopConfirm,
    /// Restarts and resumes waiting for their duplicate.
    duplicates: Duplicates,
    /// The delete-worktree confirm, while open.
    delete_dialog: Option<DeleteDialog>,
    /// The confirm's keyboard focus, so Esc, Enter and Tab reach it.
    dialog_focus: FocusHandle,
    /// The toasts, the action-failed notice and the checkout prompt.
    notices: Notices,
    /// Wakes the view when the next toast's time is up.
    toast_timer: Option<Task<()>>,
    /// The modal notices' keyboard focus.
    notice_focus: FocusHandle,
    /// Spawns waiting for the daemon's reply.
    spawns: PendingSpawns,
    /// The spawn dialog, while open.
    spawn_dialog: Option<SpawnDialog>,
    /// The spawn dialog's keyboard focus when no text field of it holds it.
    spawn_focus: FocusHandle,
    /// The Shell… dialog, while open.
    shell_dialog: Option<ShellDialog>,
    /// The Shell… dialog's keyboard focus when its folder field has it not.
    shell_focus: FocusHandle,
    /// Numbers the Shell… dialogs, so a folder picker's answer reaches only
    /// the dialog that asked for it.
    shell_generation: u64,
    /// Quick-shell folders waiting for the spawn they came with to land.
    quick_shell_saves: PendingQuickShell,
    /// Branch names the daemon suggested, per repo or workspace.
    branch_cache: BranchCache,
    /// Quits the app, once.
    quitter: Quitter,
    /// The exit dialog, while open.
    exit: Option<ExitView>,
    /// The exit dialog's keyboard focus.
    quit_focus: FocusHandle,
    /// Opens the terminals' links, on background threads.
    opener: Arc<dyn Opener>,
    /// The confirm open before a link runs a file that runs code.
    run_confirm: Option<RunConfirm>,
    /// The run confirm's keyboard focus.
    run_focus: FocusHandle,
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
            quit: Box::new(|cx: &mut App| cx.quit()),
            open: Arc::new(SystemOpener),
        };
        Self::with_transport(deps, window, cx)
    }

    /// A view over `deps`: it sends through `deps.tx` and handles every
    /// event `deps.events` delivers. A request to close the window goes
    /// through the quit flow.
    pub fn with_transport(deps: RootDeps, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let RootDeps {
            tx,
            mut events,
            ui_dir,
            paths,
            wanted,
            now,
            quit,
            open,
        } = deps;
        // Ctrl stays down in the window's record while another window has
        // the keyboard, so leaving the window ends link mode.
        cx.observe_window_activation(window, |root, window, cx| {
            if !window.is_window_active() {
                root.set_link_mode(false, cx);
            }
        })
        .detach();
        // A resize moves every dot, so an open gutter menu closes with it
        // rather than point at a row that is no longer there.
        cx.observe_window_bounds(window, |root, window, cx| {
            root.close_shell_menu(window, cx);
        })
        .detach();
        let view = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            // A view that is gone has nothing to ask; let the window close.
            view.update(cx, |root, cx| root.on_close_requested(window, cx))
                .unwrap_or(true)
        });
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
            copy_chip: Copied::default(),
            chip_timer: None,
            paths,
            menu: None,
            shell_menu: None,
            menu_focus: cx.focus_handle(),
            tab_menu: None,
            tab_menu_focus: cx.focus_handle(),
            pending_appearance: appearance::InFlightAppearance::default(),
            font_save_deadline: None,
            font_save_timer: None,
            confirm: HeaderStopConfirm::default(),
            duplicates: Duplicates::default(),
            delete_dialog: None,
            dialog_focus: cx.focus_handle(),
            notices: Notices::default(),
            toast_timer: None,
            notice_focus: cx.focus_handle(),
            spawns: PendingSpawns::default(),
            spawn_dialog: None,
            spawn_focus: cx.focus_handle(),
            shell_dialog: None,
            shell_focus: cx.focus_handle(),
            shell_generation: 0,
            quick_shell_saves: PendingQuickShell::default(),
            branch_cache: BranchCache::default(),
            quitter: Quitter::new(quit),
            exit: None,
            quit_focus: cx.focus_handle(),
            opener: open,
            run_confirm: None,
            run_focus: cx.focus_handle(),
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

    /// Pane `pane_id`'s text input, which the platform drives with typed
    /// characters and the IME's composition.
    #[must_use]
    pub fn pane_input_handler(&self, pane_id: &str) -> Option<impl InputHandler + use<>> {
        let view = self.panes.get(pane_id)?.view().clone();
        Some(ElementInputHandler::new(Bounds::default(), view))
    }

    /// The marked text pane `pane_id` draws at its cursor: an IME
    /// composition or a pending dead key.
    #[must_use]
    pub fn pane_preedit(&self, pane_id: &str, cx: &App) -> Option<String> {
        let slot = self.panes.get(pane_id)?;
        slot.view().read(cx).preedit().map(str::to_owned)
    }

    /// The font settings pane `pane_id` renders with.
    #[must_use]
    pub fn pane_font(&self, pane_id: &str, cx: &App) -> Option<fonts::FontSettings> {
        let slot = self.panes.get(pane_id)?;
        Some(slot.view().read(cx).font().clone())
    }

    /// Makes `settings` the app's terminal font: saved as the default every
    /// new pane starts from, and the size every pane falls back to. Each
    /// open pane re-applies the size it resolves to, with this family and
    /// weight.
    pub fn set_app_font(&mut self, settings: fonts::FontSettings, cx: &mut Context<Self>) {
        self.sidebar.set_terminal_font(settings.normalized());
        self.after_font_change(cx);
    }

    /// Sets the app level's colours, below every repo's, workspace's and
    /// session's; every pane and sidebar row re-resolves its own.
    pub fn set_app_colors(&mut self, colors: appearance::AppColors, cx: &mut Context<Self>) {
        self.sidebar.set_app_colors(colors);
        self.after_font_change(cx);
    }

    /// The accent session `session_id` resolves to, `0xRRGGBB`.
    #[must_use]
    pub fn session_accent(&self, session_id: &str) -> Option<u32> {
        self.sidebar.session_accents().get(session_id).copied()
    }

    /// The session the pane `pane_id` shows.
    #[must_use]
    pub fn pane_session(&self, pane_id: &str) -> Option<String> {
        Some(self.panes.get(pane_id)?.session()?.to_owned())
    }

    /// Saves the layout, re-applies every pane's resolved font and colours
    /// and redraws.
    fn after_font_change(&mut self, cx: &mut Context<Self>) {
        self.save_ui();
        self.apply_pane_fonts(cx);
        cx.notify();
    }

    /// Drops the font overrides of tabs the daemon no longer lists, once it
    /// has listed any: before the first full list nothing is known to be
    /// gone, so an update for one tab prunes nothing.
    fn prune_tab_font_sizes(&mut self) {
        if !self.tabs.is_loaded() {
            return;
        }
        let live: HashSet<&str> = self.tabs.tabs().iter().map(|tab| tab.id.as_str()).collect();
        if self.sidebar.prune_tab_font_sizes(&live) {
            self.save_ui();
        }
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

    /// The command dots pane `pane_id` draws in its gutter: the finished
    /// commands whose prompt row is on screen, top first.
    #[must_use]
    pub fn pane_shell_records(&self, pane_id: &str, cx: &App) -> Vec<ShellDot> {
        self.panes
            .get(pane_id)
            .map(|slot| slot.view().read(cx).shell_dots())
            .unwrap_or_default()
    }

    /// The finished command the gutter dot at `index` of pane `pane_id`
    /// stands for, as its menu shows it.
    #[must_use]
    pub fn pane_shell_command(
        &self,
        pane_id: &str,
        index: usize,
        cx: &App,
    ) -> Option<ShellCommand> {
        self.panes
            .get(pane_id)?
            .view()
            .read(cx)
            .shell_command(index)
    }

    /// The text of the link pane `pane_id` underlines: the one under the
    /// mouse while Ctrl is held.
    #[must_use]
    pub fn pane_hovered_link(&self, pane_id: &str, cx: &App) -> Option<String> {
        let slot = self.panes.get(pane_id)?;
        slot.view().read(cx).hovered_link().map(|link| link.text)
    }

    /// Ctrl went down or up. Modifier changes reach only the focused
    /// element's ancestors, so the root tells every pane, focused or not.
    fn on_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_link_mode(event.modifiers.secondary(), cx);
    }

    fn set_link_mode(&mut self, on: bool, cx: &mut Context<Self>) {
        for slot in self.panes.values() {
            slot.view()
                .update(cx, |pane, cx| pane.set_link_mode(on, cx));
        }
    }

    fn save_ui(&self) {
        let Some(dir) = &self.ui_dir else {
            return;
        };
        if let Err(err) = save_ui_state(dir, self.sidebar.ui_state()) {
            tracing::warn!("saving the sidebar layout: {err:#}");
        }
    }

    /// Writes the layout at most once per [`FONT_SAVE_DEBOUNCE`] while tab
    /// font steps keep coming, so a held key does not rewrite the file for
    /// every step; the save that fires writes the sizes as they then stand.
    fn schedule_font_save(&mut self, cx: &mut Context<Self>) {
        if self.font_save_deadline.is_some() {
            return;
        }
        let deadline = (self.now)() + FONT_SAVE_DEBOUNCE;
        self.font_save_deadline = Some(deadline);
        self.arm_font_save(deadline, cx);
    }

    /// Arms the timer for the rest of the wait for the pending save.
    fn arm_font_save(&mut self, deadline: Instant, cx: &mut Context<Self>) {
        let delay = deadline.saturating_duration_since((self.now)());
        self.font_save_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            // Fails only when the view is gone, and the save with it.
            this.update(cx, Self::tick_font_save).ok();
        }));
    }

    /// The save timer fired: write once the debounce has passed by the
    /// clock, which may lag the timer, else wait out the rest.
    fn tick_font_save(&mut self, cx: &mut Context<Self>) {
        let Some(deadline) = self.font_save_deadline else {
            return;
        };
        if deadline > (self.now)() {
            self.arm_font_save(deadline, cx);
        } else {
            self.flush_font_save();
        }
    }

    /// Writes a pending save now, as the quit path must before the app goes.
    fn flush_font_save(&mut self) {
        if self.font_save_deadline.take().is_some() {
            self.font_save_timer = None;
            self.save_ui();
        }
    }

    /// Hide or show the sidebar; hiding it hands the keyboard back to the
    /// active tab's focused pane, since the sidebar may have held it.
    fn toggle_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_shell_menu(window, cx);
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
    /// requested pane takes the keyboard (unless the spawn dialog, the
    /// delete-worktree confirm or a modal notice holds it; closing it
    /// focuses the tab's pane), the spawn dialog's Open in follows the tabs
    /// and the active tab is saved.
    fn after_tabs_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reconcile_panes(window, cx);
        self.prune_tab_font_sizes();
        self.apply_pane_fonts(cx);
        self.refresh_spawn_tabs();
        if let Some(pane_id) = self.tabs.take_focus_request()
            && self.spawn_dialog.is_none()
            && self.shell_dialog.is_none()
            && self.delete_dialog.is_none()
            && !self.notices.has_modal()
            && self.exit.is_none()
        {
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
        self.drop_stale_tab_menu();
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
        // Only the connection and the session list move the exit dialog's
        // wait, walk and focus.
        let exit_relevant = match &event {
            NetEvent::State(_) => true,
            NetEvent::Message(msg) => matches!(
                **msg,
                DaemonMessage::Sessions { .. }
                    | DaemonMessage::SessionUpdated { .. }
                    | DaemonMessage::SessionRemoved { .. }
            ),
            NetEvent::Handshake(_) | NetEvent::ShutdownSent | NetEvent::ShutdownFailed => false,
        };
        match event {
            NetEvent::State(conn) => self.conn = conn,
            NetEvent::Handshake(info) => self.handshake = Some(info),
            NetEvent::Message(msg) => self.on_message(*msg, window, cx),
            NetEvent::ShutdownSent => self.on_shutdown_sent(),
            NetEvent::ShutdownFailed => self.on_shutdown_failed(window, cx),
        }
        // The overlay covers the footer; a flyout left open under it would
        // reappear (possibly armed) when the overlay goes.
        if self.conn.overlay().is_some() {
            self.close_flyout();
            self.close_spawn_dialog(window, cx);
            self.close_shell_dialog(window, cx);
            self.reset_session_ui(window, cx);
            self.reset_notices(window, cx);
        }
        if exit_relevant {
            self.after_exit_event(window, cx);
        }
        cx.notify();
    }

    fn on_message(&mut self, msg: DaemonMessage, window: &mut Window, cx: &mut Context<Self>) {
        // The sidebar takes the message before the panes do, so whether a
        // snapshot moved a session's appearance is read from the old one.
        let moved = match &msg {
            DaemonMessage::SessionUpdated { session, .. } => Some(self.appearance_moved(session)),
            _ => None,
        };
        self.sidebar.apply(&msg);
        self.drop_stale_session_ui(window, cx);
        self.on_spawn_dialog_message(&msg, window, cx);
        if self.tabs.apply(&msg) {
            self.confirm.disarm();
            self.after_tabs_change(window, cx);
            return;
        }
        match msg {
            DaemonMessage::Welcome { .. } => {
                self.reset_panes(cx);
                self.pending_appearance.clear();
                self.status.clear();
                self.duplicates.clear();
                self.close_spawn_dialog(window, cx);
                self.close_shell_dialog(window, cx);
                self.reset_session_ui(window, cx);
                self.reset_notices(window, cx);
            }
            DaemonMessage::Error {
                message,
                request_id,
            } => self.on_daemon_error(message, request_id.as_deref(), window, cx),
            DaemonMessage::ActionFailed {
                title,
                detail,
                hint,
                request_id,
            } => {
                let notice = ActionFailedNotice {
                    title,
                    detail,
                    hint,
                };
                self.on_action_failed(notice, request_id.as_deref(), window, cx);
            }
            DaemonMessage::CheckoutConfirmRequired {
                repo_id,
                branch,
                dirty_count,
                request_id,
            } => {
                let ask = notices::CheckoutAsk::new(repo_id, branch, dirty_count, request_id);
                self.on_checkout_confirm(ask, window, cx);
            }
            DaemonMessage::LayoutInitRequired {
                active_session_count,
                ..
            } => {
                tracing::info!(
                    active_session_count,
                    "first connect for this client: seeding the layout with every running session"
                );
                self.request_layout_seed();
            }
            DaemonMessage::Sessions { sessions } => self.on_sessions(&sessions, window, cx),
            DaemonMessage::Scrollback {
                session_id,
                data_b64,
                truncated,
                request_id,
                forwarder_restarted,
            } => self.feed_scrollback(
                &ScrollbackReply {
                    session_id,
                    data_b64,
                    truncated,
                    request_id,
                    forwarder_restarted,
                },
                cx,
            ),
            DaemonMessage::PtyOutput {
                session_id,
                data_b64,
            } => self.feed_output(&session_id, &data_b64, cx),
            DaemonMessage::SessionUpdated {
                session,
                request_id,
            } => self.on_session_updated(
                &session,
                moved.unwrap_or(false),
                request_id.as_deref(),
                window,
                cx,
            ),
            DaemonMessage::DiscardPreview {
                session_id,
                members,
            } => self.on_discard_preview(&session_id, &members, cx),
            DaemonMessage::ShutdownAck {} => self.on_shutdown_ack(cx),
            // A repo's or workspace's appearance is a level above the
            // session's.
            DaemonMessage::Repos { .. } | DaemonMessage::Workspaces { .. } => {
                self.apply_pane_fonts(cx);
            }
            _ => {}
        }
    }

    /// Whether `session`'s snapshot moves its appearance, read against the
    /// copy the sidebar still holds.
    fn appearance_moved(&self, session: &SessionSnapshot) -> bool {
        self.sidebar
            .session(&session.id)
            .map(|before| before.appearance.clone())
            != Some(session.appearance.clone())
    }

    /// A session's snapshot: every pane showing it refreshes, the size it
    /// resolves to moves with its appearance, and a spawn or a duplicate
    /// answers with it.
    fn on_session_updated(
        &mut self,
        session: &SessionSnapshot,
        moved: bool,
        request_id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for view in self.pane_views(Some(&session.id)) {
            view.update(cx, |pane, _| pane.update_session(session));
        }
        if let Some(request_id) = request_id {
            self.pending_appearance.answered(&session.id, request_id);
        }
        self.attach_waiting_panes(cx);
        if moved {
            self.apply_session_pane_fonts(&session.id, cx);
        }
        self.place_duplicate(request_id, &session.id, window, cx);
        self.place_spawn(request_id, session, window, cx);
    }

    /// The daemon's whole session list: every pane refreshes from it, the
    /// panes whose session it names attach, each pane re-applies the size it
    /// resolves to, and the session named on the command line is focused.
    fn on_sessions(
        &mut self,
        sessions: &[SessionSnapshot],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sessions_loaded = true;
        for view in self.pane_views(None) {
            view.update(cx, |pane, _| pane.refresh_sessions(sessions));
        }
        self.attach_waiting_panes(cx);
        self.apply_pane_fonts(cx);
        self.try_wanted_session(window, cx);
    }

    /// Ask the daemon to seed this client's layout with every running session.
    fn request_layout_seed(&self) {
        self.send(ClientMessage::InitLayout {
            kind: InitLayoutKind::AllSessions,
        });
    }

    /// Keys the root takes before the panes see them. The exit dialog, else
    /// a modal notice, else the delete-worktree confirm, else the spawn
    /// dialog, else the Shell… dialog, while open, holds the focus and takes
    /// every key that reaches this listener (the spawn and Shell… dialogs
    /// let typing through to their text fields); gpui runs keymap
    /// actions before capture listeners, so that holds only while no
    /// key-bound context (a text input) has the focus. Esc drops an armed
    /// tab close (and still reaches the pane).
    fn on_key_capture(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ks = &event.keystroke;
        if self.on_exit_key(ks, window, cx) {
            cx.stop_propagation();
            return;
        }
        if self.on_run_confirm_key(ks, window, cx) {
            cx.stop_propagation();
            return;
        }
        if self.on_notice_key(ks, window, cx) {
            cx.stop_propagation();
            return;
        }
        if self.delete_dialog.is_some() {
            self.on_delete_dialog_key(ks, window, cx);
            cx.stop_propagation();
            return;
        }
        if self.spawn_dialog.is_some() {
            if self.on_spawn_dialog_key(ks, window, cx) {
                cx.stop_propagation();
            }
            return;
        }
        if self.shell_dialog.is_some() {
            if self.on_shell_dialog_key(ks, window, cx) {
                cx.stop_propagation();
            }
            return;
        }
        if ks.key == "escape" {
            let tab_close = self.tabs.close_confirm.disarm();
            if self.confirm.disarm() || tab_close {
                cx.notify();
            }
        }
        if self.on_shortcut(ks, window, cx) {
            cx.stop_propagation();
            cx.notify();
        }
    }

    /// The root's own keys; returns whether `ks` was one. Esc closes a
    /// context menu or the flyout. Ctrl+B toggles the sidebar and Ctrl+N
    /// opens the spawn dialog only when no terminal and no tab rename has
    /// the keyboard; in a terminal they are the PTY's 0x02 and 0x0e.
    /// Ctrl+Shift+N opens the spawn dialog from anywhere.
    fn on_shortcut(&mut self, ks: &Keystroke, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let ctrl_only = ks.modifiers.control && !ks.modifiers.shift && !ks.modifiers.alt;
        if self.tab_menu.is_some() && ks.key == "escape" {
            self.close_tab_menu(window, cx);
        } else if self.menu.is_some() && ks.key == "escape" {
            self.close_session_menu(window, cx);
        } else if self.shell_menu.is_some() && ks.key == "escape" {
            self.close_shell_menu(window, cx);
        } else if self.flyout_open && ks.key == "escape" {
            self.close_flyout();
        } else if let Some(key) = font_key(ks) {
            self.close_shell_menu(window, cx);
            self.on_font_key(key, cx);
        } else if ctrl_only && ks.key == "b" && self.outside_terminal(window, cx) {
            self.toggle_sidebar(window, cx);
        } else if self.is_spawn_shortcut(ks, window, cx) {
            self.open_spawn_dialog(crate::spawn_view::SpawnEntry::Toolbar, window, cx);
        } else {
            return false;
        }
        true
    }

    /// A font-size shortcut: Ctrl+`=`/`+`/`-` steps the focused session's
    /// size, with Shift the active tab's override, and Ctrl+0 clears both.
    /// The pane follows the session's change when the daemon echoes it.
    fn on_font_key(&mut self, key: FontKey, cx: &mut Context<Self>) {
        match key {
            FontKey::Session(step) => self.step_session_font(step, cx),
            FontKey::Tab(step) => {
                if let Some(tab_id) = self.tabs.active_id().map(str::to_owned) {
                    self.bump_tab_font(&tab_id, step, cx);
                }
            }
            FontKey::Clear => self.clear_font_overrides(cx),
        }
    }

    /// Sends the focused pane's session the next size, keeping its other
    /// appearance fields. The step starts from the size the session
    /// resolves to without its tab's override — a tab's size is the tab
    /// level's to change — and from the size a press already sent while the
    /// daemon has not echoed it, so a held key does not lose steps. At a
    /// clamp limit nothing goes out.
    fn step_session_font(&mut self, step: f32, cx: &mut Context<Self>) {
        let Some(pane_id) = self.focused_pane() else {
            return;
        };
        let Some(session_id) = self.pane_session(&pane_id) else {
            return;
        };
        let Some(own) = self.effective_appearance(&session_id) else {
            return;
        };
        let Some(resolved) = self.sidebar.appearance_with(&session_id, &own) else {
            return;
        };
        let Some(next) = fonts::stepped(resolved.font_size.value, step) else {
            return;
        };
        let change = AppearanceChange::font_size(Some(font_size_to_u16(next)));
        self.send_session_appearance(&session_id, &own, &change);
        cx.notify();
    }

    /// `session_id`'s stored appearance with each change sent and not yet
    /// answered over it, in order; `None` for a session the list does not
    /// hold.
    pub(crate) fn effective_appearance(&self, session_id: &str) -> Option<AppearanceOverrides> {
        let stored = &self.sidebar.session(session_id)?.appearance;
        Some(self.pending_appearance.overlay(session_id, stored))
    }

    /// Sends `session_id` `own` (its effective appearance) with `change`
    /// over it, and holds `change` in flight until the daemon answers its
    /// request.
    pub(crate) fn send_session_appearance(
        &mut self,
        session_id: &str,
        own: &AppearanceOverrides,
        change: &AppearanceChange,
    ) {
        let appearance = change.apply_to(own);
        let request_id = new_request_id();
        self.pending_appearance
            .push(session_id, request_id.clone(), change.clone());
        self.send(ClientMessage::SetSessionAppearance {
            session_id: session_id.to_owned(),
            appearance,
            request_id: Some(request_id),
        });
    }

    /// Whether `request_id` answers an appearance send, which the daemon
    /// refused: the send stops overlaying later ones, and a toast says so.
    fn refuse_appearance(
        &mut self,
        request_id: Option<&str>,
        detail: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if !request_id.is_some_and(|id| self.pending_appearance.refused(id)) {
            return false;
        }
        tracing::warn!("the daemon refused an appearance change: {detail}");
        self.push_toast(
            ToastKind::Error,
            APPEARANCE_FAILED_TITLE,
            Some(detail.to_owned()),
            cx,
        );
        true
    }

    /// Clears the focused session's size and the active tab's override, so
    /// both fall back to the container's and the app's. A level that is
    /// already clear sends and saves nothing.
    fn clear_font_overrides(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        let target = self
            .focused_pane()
            .and_then(|pane_id| self.pane_session(&pane_id));
        if let Some(session_id) = target
            && let Some(own) = self.effective_appearance(&session_id)
            && own.terminal_font_size.is_some()
        {
            self.send_session_appearance(&session_id, &own, &AppearanceChange::font_size(None));
            changed = true;
        }
        let tab_id = self.tabs.active_id().map(str::to_owned);
        if let Some(tab_id) = tab_id
            && self.sidebar.clear_tab_font_size(&tab_id)
        {
            changed = true;
        }
        if changed {
            self.apply_pane_fonts(cx);
            self.schedule_font_save(cx);
            cx.notify();
        }
    }

    /// Neither a terminal nor a tab rename has the keyboard.
    fn outside_terminal(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.renaming.is_none() && !self.terminal_focused(window, cx)
    }

    /// Ctrl+Shift+N anywhere, or Ctrl+N outside the terminals.
    fn is_spawn_shortcut(&self, ks: &Keystroke, window: &Window, cx: &Context<Self>) -> bool {
        let mods = &ks.modifiers;
        if !mods.control || mods.alt || mods.platform || !ks.key.eq_ignore_ascii_case("n") {
            return false;
        }
        mods.shift || self.outside_terminal(window, cx)
    }

    /// A press anywhere that did not stop at a tab's close button or a
    /// session action's confirm drops what they armed.
    fn on_any_mouse_down(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let tab_close = self.tabs.close_confirm.disarm();
        if self.confirm.disarm() || tab_close {
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

/// What a font-size keystroke asks for.
#[derive(Debug, Clone, Copy)]
enum FontKey {
    /// Step the focused session's size by this much.
    Session(f32),
    /// Step the active tab's override by this much.
    Tab(f32),
    /// Clear the focused session's size and the active tab's override.
    Clear,
}

/// The font-size shortcut `ks` is, if any: Ctrl with `=` (or the `+` Shift
/// types on a US layout) and `-` (or the `_` it shifts to), and Ctrl+0.
/// Shift makes a step act on the tab's override; Ctrl+Shift+0 is nothing.
fn font_key(ks: &Keystroke) -> Option<FontKey> {
    let m = ks.modifiers;
    if !m.control || m.alt || m.platform {
        return None;
    }
    let step = match ks.key.as_str() {
        "=" | "+" => 1.0,
        "-" | "_" => -1.0,
        "0" if !m.shift => return Some(FontKey::Clear),
        _ => return None,
    };
    Some(if m.shift {
        FontKey::Tab(step)
    } else {
        FontKey::Session(step)
    })
}

/// `size`, already clamped to the range, as the wire's whole pixel count.
fn font_size_to_u16(size: f32) -> u16 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the resolver clamps the size to 8..=32"
    )]
    let size = size as u16;
    size
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
        // The quit's walk draws its confirm above the exit dialog.
        let delete_dialog = self.delete_dialog_layer(cx);
        let (delete_under, delete_over) = if self.exit_walk_confirm_open() {
            (None, delete_dialog)
        } else {
            (delete_dialog, None)
        };

        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .capture_key_down(cx.listener(Self::on_key_capture))
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            .on_any_mouse_down(cx.listener(Self::on_any_mouse_down))
            .on_mouse_move(cx.listener(Self::on_drag_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_drag_end))
            .child(self.main_row(window, cx))
            .child(self.footer_bar(&footer, cx))
            .children(self.session_menu_layer(cx).into_iter().flatten())
            .children(self.shell_menu_layer(cx).into_iter().flatten())
            .children(self.tab_menu_layer(cx).into_iter().flatten())
            .children(flyout.into_iter().flatten())
            // Above the flyout's backdrop, so the chip's tooltip is reachable
            // while the flyout is open: its own copy button has no other
            // confirmation to show.
            .children(self.chip_layer())
            .children(self.spawn_dialog_layers(cx))
            .children(self.shell_dialog_layer(cx))
            .children(delete_under)
            .children(self.notice_layers(cx))
            .children(self.toast_layer(cx))
            .children(self.run_confirm_layer(cx))
            .children(self.exit_layer(cx))
            .children(delete_over)
            .children(overlay)
    }
}

/// A fresh id for a request whose reply the client matches to it.
pub(crate) fn new_request_id() -> String {
    uuid::Uuid::new_v4().to_string()
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

    /// The "Handshake file" row with its copy button. The button keeps its
    /// label and the copy answers with the chip, which draws over the flyout.
    fn handshake_row(&self, cx: &mut Context<Self>) -> Div {
        let button = action_button("copy-handshake", "copy", TEXT, self.paths.is_ok());
        let button = match &self.paths {
            Ok(paths) => {
                let path = paths.handshake.display().to_string();
                button.tooltip(tooltip(path.clone())).on_click(cx.listener(
                    move |this, _: &ClickEvent, _, cx| {
                        this.copy_to_clipboard(&path, cx);
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
