//! Native (GPUI) rustling-tulip client: the daemon's tabs and split panes for
//! this client, each pane a session rendered with `alacritty_terminal`,
//! beside a sidebar of every session.
//!
//! The binary opens [`open_main_window`]; the UI specs build a [`RootView`]
//! over their own transport with [`RootView::with_transport`].

mod activity_bar;
mod agent_options;
mod agent_options_view;
pub mod appearance;
mod appearance_stops;
mod appearance_view;
mod assets;
mod branch_fate;
mod buttons;
mod changes_view;
mod cleanup_failed;
mod cleanup_failed_view;
mod combobox;
mod connection;
pub mod connection_picker;
mod connection_picker_view;
mod copied;
mod copy_text;
mod device_pairing;
mod device_pairing_view;
mod devices;
mod devices_view;
pub mod diff_model;
mod diff_tab;
mod diff_tab_view;
pub mod diff_view;
mod discard_confirm;
mod drag;
pub mod ellipsis;
mod fetch_dialog;
mod fetch_dialog_view;
mod fetch_link;
pub mod file_fetch;
mod file_fetch_view;
mod folder_picker;
pub mod fonts;
mod footer;
mod grid_view;
mod history;
mod history_view;
mod host_path_prompt;
mod host_path_prompt_view;
mod html_media;
mod keys;
pub mod launch_last;
mod layout_chooser;
mod layout_chooser_view;
mod links;
mod md_blocks;
mod mouse;
mod move_panes;
mod move_panes_view;
mod needs_you;
mod needs_you_view;
mod net;
mod notice_view;
mod notices;
mod notify;
pub mod offscreen;
mod open;
mod open_view;
mod pairing;
mod pairing_view;
pub mod palette;
mod pane_close;
mod pane_close_view;
mod pane_menu;
mod popout_view;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "pruning pop-outs whose content went is not wired to the session and tab folds yet"
    )
)]
mod popouts;
mod preset_launch;
mod preset_prompts;
pub(crate) mod preset_wizard;
mod preset_wizard_view;
mod quit;
mod quit_view;
mod recover;
mod recover_view;
pub mod remote_access;
pub(crate) mod remote_mode;
mod repo_add;
mod repo_remove;
mod repo_remove_view;
mod run_confirm;
mod sc_writes;
mod scrollback_load;
mod section;
mod session_actions;
mod session_menu;
mod settings_view;
mod shell_dialog;
mod shell_marks;
mod shell_view;
mod sidebar;
mod sidebar_view;
mod source_control;
mod source_control_view;
mod spawn_form;
mod spawn_preview;
mod spawn_view;
mod spawns;
mod stash_view;
mod stashes;
mod status_glyph;
pub mod syntax;
mod tab_bar;
mod tab_menu;
mod tabs;
pub mod target;
mod term;
mod term_input;
mod term_view;
mod text_input;
mod theme;
pub mod toast_activation;
mod undo;
mod undo_view;
mod viewer_find;
mod viewer_md_view;
mod viewer_tab;
mod viewer_tab_view;
mod viewer_text;
mod vscode_suggestion;
mod vscode_suggestion_view;
mod window_state;
mod window_title;
mod workspace_creator;
mod workspace_creator_view;
mod worktrees_manager;
mod worktrees_manager_view;

use alacritty_terminal::vte::ansi::CursorShape;
use daemon_client::device_tokens::DeviceTokenStore;
use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::future::LocalBoxFuture;
use gpui::{
    Animation, AnimationExt as _, AnyElement, AnyView, AnyWindowHandle, App, Bounds, ClickEvent,
    Context, CursorStyle, DisplayId, Div, ElementId, ElementInputHandler, FocusHandle, FontWeight,
    InputHandler, KeyDownEvent, Keystroke, ModifiersChangedEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, SharedString, Stateful, StyledText, Task, Window,
    WindowBounds, WindowHandle, WindowId, WindowOptions, div, point, prelude::*, pulsating_between,
    px, size,
};
use protocol::{
    AppearanceOverrides, ClientMessage, DaemonMessage, GridNode, PaneDropEdge, RepoEntry,
    SessionMode, SessionSnapshot, SuggestTarget, TabEntry, VscodeWorkspaceSuggestion,
    WorkspaceEntry,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::appearance::AppearanceChange;
use crate::appearance_view::AppearanceEditor;
use crate::changes_view::ChangesUi;
use crate::connection::{DotKind, Footer};
use crate::connection_picker::{PeerSource, SystemPeers};
use crate::connection_picker_view::ConnectionPickerDialog;
use crate::copied::Copied;
use crate::drag::DragPayload;
use crate::fetch_dialog_view::FetchDialog;
use crate::footer::{CountSpan, StopConfirm, flyout_rows, log_paths, status_counts};
use crate::grid_view::{PaneSlot, RetryGate, divider_ratio};
use crate::host_path_prompt_view::HostPathDialog;
use crate::needs_you_view::moves_needs_you;
use crate::notice_view::MODAL_RADIUS;
use crate::notices::Notices;
use crate::notify::{SilentNotifier, SystemNotifier};
use crate::open::SystemOpener;
use crate::popout_view::PopoutView;
use crate::popouts::{POPOUT_CASCADE, POPOUT_MIN_SIZE, POPOUT_SIZE, Popouts};
use crate::quit_view::{ExitView, Quitter};
use crate::remote_mode::Capabilities;
use crate::repo_remove::RepoRemove;
use crate::run_confirm::RunConfirm;
use crate::section::flyout_section;
use crate::session_actions::{
    Duplicates, HEADLESS_UNSUPPORTED_TIP, HeaderStopConfirm, Prefills, self_exited,
};
use crate::session_menu::popover_frame;
use crate::session_menu::{ContainerMenu, DeleteDialog, SessionMenu, ShellMenu};
use crate::shell_dialog::PendingQuickShell;
use crate::shell_view::ShellDialog;
use crate::sidebar::{
    SidebarModel, UiState, can_attach, load_ui_state, normalize_fs_path, save_ui_state,
};
use crate::source_control::ScModel;
use crate::spawn_form::{BranchCache, Lock, Prefill, Target};
use crate::spawn_view::{SpawnEntry, SpawnModal};
use crate::spawns::PendingSpawns;
use crate::stash_view::StashUi;
use crate::status_glyph::{GlyphSize, glyph_view};
use crate::tab_bar::{Rename, TabMenu};
use crate::tabs::{
    PaneTarget, Placement, TabsModel, auto_grid_resends, find_tab_containing_session, is_auto_grid,
    tab_pills,
};
use crate::term::ShellCommand;
use crate::term_view::ScrollbackReply;
use crate::undo::UndoShelf;
use crate::vscode_suggestion::VscodeSuggestions;
use crate::window_state::{RestoreRect, WindowState};
use crate::workspace_creator_view::WorkspaceCreatorDialog;

pub use crate::assets::Assets;
pub use crate::changes_view::{
    DiscardConfirmView, ScAction, ScBucketRow, ScButton, ScFileRow, ScFolderRow, ScTreeRow,
};
pub use crate::connection::Connection;
pub use crate::diff_tab::{
    BINARY_TEXT, DiffTabBody, DiffTabHeader, EMPTY_TEXT, FINAL_NEWLINE_NOTE, LINE_ENDINGS_NOTE,
    LOADING_TEXT as DIFF_LOADING_TEXT, OPEN_FAILED_TITLE as DIFF_OPEN_FAILED_TITLE, WHITESPACE_TIP,
};
pub use crate::drag::{Side, SidebarRow};
pub use crate::footer::LogPaths;
pub use crate::grid_view::{HeaderChip, HeaderChipKind, POPPED_OUT_NOTE, PaneHeaderParts};
pub use crate::history::{
    CommitDetailView, CommitRow, DetailFile, DetailPane, ForgeButton, HistoryBlock, HistoryBody,
    MoreRow,
};
pub use crate::needs_you_view::NeedsYouEntry;
pub use crate::net::{
    EnsureFuture, HandshakeInfo, NATIVE_PROTOCOL_VERSIONS, NetCommand, NetDeps, NetEvent,
    OpenLocalFuture, StopFuture, spawn_with as spawn_net,
};
pub use crate::notices::{
    ActionFailedNotice, CheckoutChoice, CheckoutPrompt, TOAST_LIFETIME, Toast, ToastAction,
    ToastKind, ToastSpec, toast_action_selector,
};
pub use crate::notify::{Notifier, NotifyState, PairingActionSink};
pub use crate::open::{OpenFailure, Opener};
pub use crate::pairing::{
    DialogPhase, DialogView, PairingAction, PairingState, PairingToast, QrMatrix, WaitingRow,
    approve_message, deny_message,
};
pub use crate::popouts::{OpenOutcome, PopoutKey};
pub use crate::preset_wizard::{LaunchButton, NextButton};
pub use crate::quit_view::QuitFn;
pub use crate::recover_view::RecoverRow;
pub use crate::settings_view::LaunchCommandRow;
pub use crate::shell_marks::{ShellDot, ShellStatus};
pub use crate::sidebar::{
    Activity, Container, ContainerKind, DEFAULT_WIDTH as SIDEBAR_DEFAULT_WIDTH, DETACHED_BANNER,
    Leaf, LeafState, RepoFilter, SidebarView, resume_all_label,
};
pub use crate::source_control::{Bucket, ScKey};
pub use crate::source_control_view::{ScContext, ScPanel, ScPickerRow, ScSectionRow};
pub use crate::spawns::{OpenIn, PaneAim};
pub use crate::stash_view::{
    ScStashPush, ScStashRow, ScStashes, StashAction, StashButton, StashDropView,
};
pub use crate::tabs::TabPill;
pub use crate::text_input::bind_keys;
pub use crate::undo::{TabSnapshot, UndoEntry};
pub use crate::viewer_tab::{
    MarkdownMode, OPEN_FAILED_TITLE as VIEWER_OPEN_FAILED_TITLE, ViewerLoad, ViewerOrigin,
    ViewerTab,
};
pub use crate::worktrees_manager_view::{WorktreesButton, WorktreesRow};

const PADDING: f32 = 6.0;
/// Thickness of the drag handles between the sidebar and the tabs, and
/// between the source-control panel's sections.
pub const DIVIDER_WIDTH: f32 = 4.0;
/// The gap between two pane cards, which is their divider's drag handle,
/// and the padding round a tab's grid of cards.
pub const PANE_GUTTER: f32 = 6.0;
/// How long tab font steps and window moves wait before the layout is
/// written out.
const UI_SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
/// How long a window resize waits before an automatic grid re-picks its
/// columns: long enough that a drag is one re-pick, short enough that letting
/// go of the window edge settles within a quarter second.
const AUTO_GRID_RESIZE_DEBOUNCE: Duration = Duration::from_millis(250);
/// The toast a session appearance change the daemon refused raises.
const APPEARANCE_FAILED_TITLE: &str = "Couldn't change the appearance";
/// The toast a launch-last replay of a trusted config raises before the
/// spawn dialog opens.
const TRUSTED_LAUNCH_TITLE: &str = "Review trusted launch";
const TRUSTED_LAUNCH_DETAIL: &str =
    "Launch last uses elevated authority, so the full spawn dialog opens before replay.";
/// The toast a launch-last branch wait raises when the daemon stays silent.
const LAUNCH_LAST_TIMEOUT_TITLE: &str = "Couldn't pick a branch name for launch-last";
const LAUNCH_LAST_TIMEOUT_DETAIL: &str =
    "The daemon didn't answer in time. Try again, or use the spawn dialog.";
/// This client's log file, under `<config dir>/logs/`.
pub const LOG_FILE: &str = "native.log";

/// Where the terminals read the time, for their scrollback timeouts.
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Asks the user for one folder, opening at the given folder when there is
/// one; resolves to `None` when they cancel or the picker fails.
pub type FolderPicker =
    Rc<dyn Fn(&mut App, Option<PathBuf>) -> LocalBoxFuture<'static, Option<PathBuf>>>;

/// The file types a file picker offers: the filter's name and the
/// extensions it lets through, without dots; no extensions offers every
/// file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFilter {
    pub name: String,
    pub extensions: Vec<String>,
}

impl FileFilter {
    #[must_use]
    pub fn new(name: &str, extensions: &[&str]) -> Self {
        Self {
            name: name.to_owned(),
            extensions: extensions.iter().map(|ext| (*ext).to_owned()).collect(),
        }
    }
}

/// Asks the user for one file of the filter's types, opening at the given
/// folder when there is one; resolves to `None` when they cancel or the
/// picker fails. The workspace creator's Browse and the preset wizard's file
/// pickers call it.
pub type FilePicker =
    Rc<dyn Fn(&mut App, Option<PathBuf>, FileFilter) -> LocalBoxFuture<'static, Option<PathBuf>>>;

/// Which of this PC's pickers [`RootView::pick_local`] asks, and for a file
/// the types it offers.
#[derive(Debug, Clone)]
pub(crate) enum Pick {
    Folder,
    File(FileFilter),
}

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
    /// Shows the OS notifications attention events fire, on background
    /// threads.
    pub notify: Arc<dyn Notifier>,
    /// Asks for a folder: the Shell… dialog's and the Worktrees tab's
    /// Browse, and Add repo.
    pub pick_folder: FolderPicker,
    /// Asks for a file: the workspace creator's `.code-workspace` Browse and
    /// the preset wizard's file pickers.
    pub pick_file: FilePicker,
    /// Reads and switches whether this PC starts the daemon at login: the
    /// Remote access page's toggle.
    pub autostart: Arc<dyn remote_access::Autostart>,
    /// The device tokens this PC holds per tailnet host: the same store the
    /// network thread dials with. The host picker reads it for its "Not
    /// paired" tags and Forget deletes from it.
    pub tokens: Arc<dyn DeviceTokenStore>,
}

/// Opens the client's window on a live daemon connection, focusing
/// `wanted_session` once it arrives.
pub fn open_main_window(wanted_session: Option<String>, cx: &mut App) {
    bind_keys(cx);
    fonts::register_bundled(cx);
    let offscreen = offscreen::requested();
    // The cloaked window opens as it always has, whatever was saved.
    let saved = if offscreen {
        None
    } else {
        saved_window_state()
    };
    let (display_id, window_bounds) = window_placement(saved.as_ref(), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(window_bounds),
            display_id,
            // gpui activates any window it shows, so a cloaked window is
            // opened hidden and shown by `show_cloaked`.
            focus: !offscreen,
            show: !offscreen,
            ..Default::default()
        },
        move |window, cx| {
            if offscreen {
                offscreen::show_cloaked(window, WINDOW_SIZE);
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

/// The main window's place saved in `native-ui.json`, if the config dir
/// resolves and the file holds one.
fn saved_window_state() -> Option<WindowState> {
    match daemon_client::config_dir() {
        Ok(dir) => load_ui_state(&dir).window,
        Err(err) => {
            tracing::warn!("the window opens at its default place: {err:#}");
            None
        }
    }
}

/// The monitor and bounds the main window opens with, from `saved`.
fn window_placement(saved: Option<&WindowState>, cx: &App) -> (Option<DisplayId>, WindowBounds) {
    let displays: Vec<(DisplayId, String)> = cx
        .displays()
        .iter()
        .filter_map(|display| Some((display.id(), display.uuid().ok()?.to_string())))
        .collect();
    let (width, height) = WINDOW_SIZE;
    let default_size = size(px(width.into()), px(height.into()));
    let restore = window_state::restore_options(saved, &displays, default_size);
    let bounds = match restore.rect {
        RestoreRect::Centered(size) => Bounds::centered(None, size, cx),
        RestoreRect::At(bounds) => bounds,
    };
    let bounds = if restore.maximized {
        WindowBounds::Maximized(bounds)
    } else {
        WindowBounds::Windowed(bounds)
    };
    (restore.display, bounds)
}

/// The main window's size in logical pixels.
pub(crate) const WINDOW_SIZE: (u16, u16) = (1000, 640);

/// Where a pop-out opens: centred on `display_id`, moved right and down by
/// [`POPOUT_CASCADE`] for each pop-out opened before it.
fn popout_bounds(display_id: Option<DisplayId>, cascade: usize, cx: &App) -> Bounds<Pixels> {
    let (width, height) = POPOUT_SIZE;
    let mut bounds = Bounds::centered(display_id, size(px(width.into()), px(height.into())), cx);
    let steps = u16::try_from(cascade).unwrap_or(u16::MAX);
    let offset = px(f32::from(POPOUT_CASCADE) * f32::from(steps));
    bounds.origin += point(offset, offset);
    bounds
}

/// What a pop-out window opens with, gathered from the root view.
struct PopoutOpening {
    key: PopoutKey,
    display_id: Option<DisplayId>,
    bounds: Bounds<Pixels>,
    title: String,
    /// The popped pane's terminal, which takes the keyboard in the window.
    terminal: Option<gpui::Entity<term_view::TerminalPane>>,
}

/// An open pop-out window and the title it shows.
struct PopoutWindow {
    handle: WindowHandle<PopoutView>,
    title: String,
}

/// Opens the window `opening` describes and records it on `root`; a window
/// that fails to open closes its pop-out again. A pop-out docked back (or
/// closed with the main window) before this ran opens nothing.
fn open_popout_now(root: &gpui::WeakEntity<RootView>, opening: PopoutOpening, cx: &mut App) {
    let still_open = root
        .read_with(cx, |root, _| root.popouts.is_open(&opening.key))
        .unwrap_or(false);
    if !still_open {
        tracing::debug!(key = ?opening.key, "pop-out closed before its window opened");
        return;
    }
    let PopoutOpening {
        key,
        display_id,
        bounds,
        title,
        terminal,
    } = opening;
    let offscreen = offscreen::requested();
    let (min_width, min_height) = POPOUT_MIN_SIZE;
    let view_root = root.clone();
    let view_key = key.clone();
    let shown_title = title.clone();
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(min_width.into()), px(min_height.into()))),
            display_id,
            // As the main window: a cloaked window opens hidden and is
            // shown by `show_cloaked`, never activated.
            focus: !offscreen,
            show: !offscreen,
            ..Default::default()
        },
        move |window, cx| {
            if offscreen {
                offscreen::show_cloaked(window, POPOUT_SIZE);
            }
            window.set_window_title(&title);
            if let Some(terminal) = &terminal {
                terminal.read(cx).focus(window);
            }
            cx.new(|cx| PopoutView::new(view_root, view_key, window, cx))
        },
    );
    let recorded = root.update(cx, |root, cx| match opened {
        Ok(handle) => {
            let popout = PopoutWindow {
                handle,
                title: shown_title,
            };
            root.popout_windows.insert(key, popout);
        }
        Err(err) => {
            tracing::error!(?key, "opening a pop-out window: {err:#}");
            root.forget_popout(&key, cx);
        }
    });
    if let Err(err) = recorded {
        tracing::debug!("a pop-out opened after the root view went: {err:#}");
    }
}

/// Closes `key`'s pop-out window; one already gone is logged.
fn close_popout_window(key: &PopoutKey, handle: WindowHandle<PopoutView>, cx: &mut App) {
    if let Err(err) = handle.update(cx, |_, window, _| window.remove_window()) {
        tracing::debug!(?key, "the pop-out window was already closed: {err:#}");
    }
}

/// Text size of the footer, flyout and overlay.
const UI_TEXT_SIZE: f32 = 12.0;
/// How far a divider press must travel before it resizes the split; anything
/// shorter is a click on the handle and sends nothing.
const DIVIDER_DRAG_THRESHOLD: f32 = 3.0;
/// The footer's height in logical pixels.
pub const FOOTER_HEIGHT: f32 = 28.0;
/// Text size of the footer's counts and status text.
const FOOTER_TEXT_SIZE: f32 = 11.5;
/// The footer's daemon pill: its height and text size.
const FOOTER_PILL_HEIGHT: f32 = 20.0;
const FOOTER_PILL_TEXT_SIZE: f32 = 11.0;
/// Space between the footer's pill and its counts, and between two counts.
const FOOTER_GAP: f32 = 14.0;
pub use activity_bar::{ITEM_HEIGHT as RAIL_ITEM_HEIGHT, RAIL_WIDTH};
pub use palette::{BAR_BG, PANEL_BG};
pub(crate) use palette::{BORDER, DANGER, DANGER_BG, HOVER_BG, MUTED, TEXT, WARNING};
use palette::{
    CHIP, LINE_STRONG, SCRIM, STATUS_ERR, STATUS_IDLE, STATUS_OK, SUNKEN, SURFACE, TEXT_2,
};

/// What a press on a drag handle is resizing.
enum Drag {
    Sidebar,
    /// A split of `tab_id`; `ratio` is where the drag has taken it so far.
    Divider {
        tab_id: String,
        split_path: Vec<u8>,
        /// Where the press landed; a press that stays within
        /// [`DIVIDER_DRAG_THRESHOLD`] of it is a click, not a resize.
        origin: (f32, f32),
        ratio: Option<f32>,
    },
    /// The source-control split between the changes and the History.
    ScSplit,
    /// The split between a section's commit list and its detail pane, by
    /// section key id.
    HistoryList(String),
}

/// The window's content: sidebar, tabs and panes, footer and overlays.
#[expect(
    clippy::struct_excessive_bools,
    reason = "the flags belong to unrelated features (the sessions list loaded, the flyout, \
              the source-control picker, a drag-ending Esc), not to one state an enum could hold"
)]
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
    /// The pane of the shown tab a drag-and-drop gesture is over, and the
    /// edge of it a drop there takes, while the drop would be accepted.
    pane_drop: Option<(String, PaneDropEdge)>,
    /// The pane a header drag picked up, set when gpui starts the drag. It
    /// tells a pane drag from any other gpui drag (files dragged in from
    /// the OS), and counts only while gpui still has a drag under way.
    pane_drag: Option<DragPayload>,
    /// The session the dragged pane showed when the drag started, `None` for
    /// an empty pane: a pane that turns into another session, or loses this
    /// one, ends the drag.
    pane_drag_session: Option<String>,
    /// The pill a pill drag is over and the side of it the dragged pill
    /// lands on, while a drop there would change the strip's order.
    tab_drop: Option<(String, Side)>,
    /// The tab whose pill a drag picked up, set when gpui starts the drag;
    /// it counts only while gpui still has a drag under way.
    tab_drag: Option<String>,
    /// The sidebar container or leaf a row drag picked up, set when gpui
    /// starts the drag; it counts only while gpui still has a drag under
    /// way. A tab container's drag is a pill drag and sets `tab_drag`.
    sidebar_drag: Option<DragPayload>,
    /// The sidebar row a container, tab or leaf drag is over and the side
    /// of it the dragged row lands on, while a drop there would change the
    /// order; apart from `tab_drop`, which only pills draw.
    sidebar_drop: Option<(SidebarRow, Side)>,
    /// Whether the key press under way is an Esc that ended a drag; the
    /// root's key capture then drops it so nothing below sees it.
    escape_ended_drag: bool,
    /// The window's keystroke interceptor, which lets Esc end a drag before
    /// gpui dispatches the key to any keymap action.
    _drag_escape: gpui::Subscription,
    /// The open pop-out windows' contents, in the order they opened.
    popouts: Popouts,
    /// Each open pop-out's window.
    popout_windows: HashMap<PopoutKey, PopoutWindow>,
    /// The popped pane whose terminal has its pop-out window's keyboard.
    popout_focused: Option<String>,
    /// The main window's own focus, on its outermost element: a pane popping
    /// out hands the keyboard here when its terminal had it, so the app's
    /// shortcuts keep working with no terminal focused.
    root_focus: FocusHandle,
    /// Focus for the sidebar, which takes it on a click so Ctrl+B there
    /// toggles the sidebar instead of reaching the terminal.
    sidebar_focus: FocusHandle,
    /// The container row the sidebar's keyboard acts on, when any.
    row_focus: Option<String>,
    /// The container row the pointer is over, while it is over one.
    hovered_container: Option<String>,
    /// The rail button whose tooltip a press put away.
    rail_tip: activity_bar::RailTipSuppression,
    /// Whether the pointer is in the window, in place of the platform's
    /// answer; `None` asks the platform.
    window_hover_override: Option<bool>,
    /// The launch-last replay waiting for the daemon's fresh branch name,
    /// if any.
    launch_last: launch_last::State,
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
    /// The repo or workspace context menu, while open.
    container_menu: Option<ContainerMenu>,
    /// The sessions toolbar's ⋯ menu.
    more_menu: sidebar_view::MoreMenu,
    /// The appearance editor, or the Settings modal holding it at the app
    /// level, while open.
    appearance_editor: Option<AppearanceEditor>,
    /// The editor's keyboard focus when none of its fields holds it.
    appearance_focus: FocusHandle,
    /// The Settings modal's shown tab.
    settings_tab: settings_view::SettingsTab,
    /// The Settings tab list's keyboard focus, where Up and Down move.
    settings_tabs_focus: FocusHandle,
    /// The Settings control Tab or a click last moved to; it has the
    /// keyboard while the modal's own focus does.
    settings_control: Option<settings_view::SettingsControl>,
    /// The Settings control the keyboard ring last had focused, as the
    /// last render saw it; when it leaves the ring the keyboard moves on
    /// from its place.
    settings_ring_control: Option<settings_view::SettingsControl>,
    /// The daemon's keep-awake state as this connection last heard it.
    keep_awake: Option<settings_view::KeepAwake>,
    /// The daemon's remote access status as this connection last heard it.
    remote_status: Option<remote_access::RemoteStatusView>,
    /// The Remote access page's port field and login launch, once shown.
    remote_access_tab: Option<settings_view::RemoteAccessTab>,
    /// Each agent's launch command as this connection last heard it.
    agent_commands: Option<HashMap<protocol::AgentId, protocol::AgentCommandEntry>>,
    /// The daemon's agents, in its order, as this connection last heard them.
    agents: Vec<protocol::AgentDescriptor>,
    /// Each agent's Settings page values as this connection last heard them.
    agent_settings: std::collections::BTreeMap<protocol::AgentId, protocol::OptionValues>,
    /// Whether this connection has heard the agents' Settings values.
    agent_settings_received: bool,
    /// Each agent's Settings values as its page last sent them, until a
    /// daemon broadcast naming the agent answers.
    agent_settings_pending: std::collections::BTreeMap<protocol::AgentId, protocol::OptionValues>,
    /// The Settings values sent and not yet answered, oldest first, so a
    /// refusal finds the one it answers by its request id.
    agent_setting_applies: std::collections::VecDeque<settings_view::SettingApply>,
    /// The agent pages' launch-command and text fields, each kept per
    /// agent once its page was shown, while Settings is open.
    agent_tabs: settings_view::AgentTabs,
    /// Reads and switches whether this PC starts the daemon at login.
    autostart: Arc<dyn remote_access::Autostart>,
    /// The title last set on the window.
    window_title: String,
    /// The title waiting out its debounce, if it differs from the one set.
    title_pending: Option<String>,
    /// Sets the pending title once its debounce is up.
    title_timer: Option<Task<()>>,
    /// Each repo's or workspace's last appearance sent, until the daemon's
    /// list echoes it: the daemon answers with the whole list and no
    /// request id, so the next change builds on this, not on what is
    /// stored.
    container_sends: HashMap<appearance_view::Level, AppearanceOverrides>,
    /// The tab context menu, while open.
    tab_menu: Option<TabMenu>,
    /// The open tab menu's keyboard focus, so Esc reaches it.
    tab_menu_focus: FocusHandle,
    /// The pane-close and Move panes dialogs and the empty pane's menu.
    pane_ui: pane_menu::PaneUi,
    /// Each session's appearance changes the client sent and the daemon has
    /// not answered, keyed by request. Every send starts from the stored
    /// appearance with these over it, so a held key steps from where its
    /// last press left off and one change does not undo another still on
    /// its way.
    pending_appearance: appearance::InFlightAppearance,
    /// The sessions this connection has had a snapshot of. A status the
    /// sidebar kept from before a reconnect says nothing about an exit.
    seen_sessions: HashSet<String>,
    /// When the pending layout save is due, if one is.
    ui_save_deadline: Option<Instant>,
    /// Wakes the view when the layout save is due.
    ui_save_timer: Option<Task<()>>,
    /// Wakes the view when a resize's automatic grid re-pick is due.
    auto_grid_timer: Option<Task<()>>,
    /// A header Stop waiting for its second click.
    confirm: HeaderStopConfirm,
    /// The Detached banner's Stop all waiting for its second click.
    detached_stop_all: sidebar_view::DetachedStopAll,
    /// Restarts and resumes waiting for their duplicate.
    duplicates: Duplicates,
    /// A Shift-duplicate waiting for its source's spawn config.
    prefills: Prefills,
    /// The delete-worktree confirm, while open.
    delete_dialog: Option<DeleteDialog>,
    /// The confirm's keyboard focus, so Esc, Enter and Tab reach it.
    dialog_focus: FocusHandle,
    /// The repo/workspace Remove flow: the armed container-menu row and the
    /// dialog that lists its live sessions.
    repo_remove: RepoRemove,
    /// The Remove dialog's keyboard focus, so Esc, Enter and Tab reach it.
    repo_remove_focus: FocusHandle,
    /// The workspace creator, while open: its mode, name, ticks, parse and
    /// wait for the daemon, beside the name field's input entity.
    workspace_creator: Option<WorkspaceCreatorDialog>,
    /// The creator's keyboard focus when its name field has it not.
    workspace_creator_focus: FocusHandle,
    /// The VS Code workspace suggestions the daemon pushed, shown one at a
    /// time.
    vscode_suggestions: VscodeSuggestions,
    /// The prompt's keyboard focus, so Esc, Enter and Tab reach it.
    vscode_prompt_focus: FocusHandle,
    /// The `.code-workspace` scan answers for the DIR and SH container
    /// folders, keyed by normalized path; `None` until the answer arrives.
    cwd_workspace_scans: HashMap<String, Option<VscodeWorkspaceSuggestion>>,
    /// The toasts, the action-failed notice and the checkout prompt.
    notices: Notices,
    /// Wakes the view when the next toast's time is up.
    toast_timer: Option<Task<()>>,
    /// The modal notices' keyboard focus.
    notice_focus: FocusHandle,
    /// What a close or a move can be taken back with, newest first.
    undo: UndoShelf,
    /// Wakes the view when the next undo entry's time is up.
    undo_timer: Option<Task<()>>,
    /// Spawns waiting for the daemon's reply.
    spawns: PendingSpawns,
    /// The spawn dialog, while open: its form, or its no-repos state.
    spawn_dialog: Option<SpawnModal>,
    /// The spawn dialog's keyboard focus when no text field of it holds it.
    spawn_focus: FocusHandle,
    /// The Shell… dialog, while open.
    shell_dialog: Option<ShellDialog>,
    /// The Shell… dialog's keyboard focus when its folder field has it not.
    shell_focus: FocusHandle,
    /// Numbers the Shell… dialogs, so a folder picker's answer reaches only
    /// the dialog that asked for it.
    shell_generation: u64,
    /// The Fetch file… dialog, while open.
    fetch_dialog: Option<FetchDialog>,
    /// The Fetch file… dialog's keyboard focus when its path field has it
    /// not.
    fetch_focus: FocusHandle,
    /// The Connections picker, while open.
    connection_picker: Option<ConnectionPickerDialog>,
    /// The Connections picker's keyboard focus when its fields have it not.
    connection_picker_focus: FocusHandle,
    /// The Tailscale the Connections picker scans for peers.
    peer_source: Arc<dyn PeerSource + Send + Sync>,
    /// The Add repo prompt on a remote host, while open.
    host_path_prompt: Option<HostPathDialog>,
    /// The Add repo prompt's keyboard focus when its path field has it not.
    host_path_focus: FocusHandle,
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
    /// The first-connect layout chooser, while open.
    layout_chooser: Option<layout_chooser::LayoutChooser>,
    /// The layout chooser's keyboard focus.
    chooser_focus: FocusHandle,
    /// The arrangement the chooser picked, applied to the daemon's next tab
    /// list.
    pending_arrangement: Option<layout_chooser::Arrangement>,
    /// The import modal, while open.
    import_modal: Option<layout_chooser::ImportChooser>,
    /// The import a remote host's first session list offered, shown on the
    /// next tab list.
    pending_import: Option<layout_chooser::ImportOffer>,
    /// True from `Welcome` until the connection's first `Sessions`: only that
    /// list can offer an import; a later one is a resync.
    awaiting_initial_sessions: bool,
    /// Worktree cleanups that failed, one per session; the head is shown.
    cleanup_failed: cleanup_failed::CleanupQueue,
    /// The cleanup-failed dialog's keyboard focus.
    cleanup_focus: FocusHandle,
    /// Opens the terminals' links, on background threads.
    opener: Arc<dyn Opener>,
    /// The OS notifications attention events fire.
    notifications: notify::Notifications,
    /// The confirm open before a link runs a file that runs code.
    run_confirm: Option<RunConfirm>,
    /// The run confirm's keyboard focus.
    run_focus: FocusHandle,
    /// The source-control statuses and the requests out for them.
    sc: ScModel,
    /// Whether the source-control repo picker's menu is open.
    sc_picker_open: bool,
    /// The source-control History: commits, details and remotes.
    history: history::HistoryModel,
    /// Where the source-control body and History blocks were last laid
    /// out; the split drags map the pointer through it.
    sc_layout: history::ScLayout,
    /// The source-control writes out, the banners failed ones left, the
    /// file menu and the discard confirm.
    changes: ChangesUi,
    /// The stash lists, the stash writes out, the push inputs and the drop
    /// confirm.
    stash: StashUi,
    /// A view for every diff tab, by tab id.
    diff_tabs: HashMap<String, diff_tab_view::DiffSlot>,
    /// The diff requests' ids, and the tab an open waits to activate.
    diff_opens: diff_tab::DiffOpens,
    /// The state of every viewer tab, by tab id.
    viewer_tabs: HashMap<String, viewer_tab::ViewerTab>,
    /// The viewer opens out, and the tab one waits to activate.
    viewer_opens: viewer_tab::ViewerOpens,
    /// The keyboard of the viewer tab shown.
    viewer_focus: FocusHandle,
    /// The tab active after the last tab change, so a diff or viewer tab
    /// takes the keyboard when it becomes active.
    last_active_tab: Option<String>,
    /// The session a terminal tab focused last, so the source-control
    /// panel keeps its sections while a diff tab is active.
    last_focused_session: Option<String>,
    /// Asks for a folder.
    pick_folder: FolderPicker,
    /// Asks for a file: a `.code-workspace` for the workspace creator, a
    /// prompts or variable file for the preset wizard.
    pick_file: FilePicker,
    /// The preset wizard, while open.
    preset_wizard: Option<preset_wizard_view::PresetWizardDialog>,
    /// The preset wizard's keyboard focus.
    preset_wizard_focus: FocusHandle,
    /// The preset launch jobs this window started, which outlive the wizard.
    preset_launches: preset_launch::PresetLaunches,
    /// The sessions a launch toast's Select marked in the sidebar, until a
    /// session click or a new connection; in memory only.
    launched_highlight: preset_launch::LaunchHighlight,
    /// How many times the launch toasts' buttons were refitted; specs read
    /// it to see a terminal frame skip that upkeep.
    preset_launch_syncs: u64,
    /// The worktrees root and whether the user set it, as this connection
    /// last heard.
    worktrees_root: Option<(String, bool)>,
    /// The Settings Worktrees tab's path field and its save in flight.
    worktrees_tab: Option<settings_view::WorktreesTab>,
    /// The Manage worktrees modal, while open.
    worktrees_manager: Option<worktrees_manager::WorktreesManager>,
    /// The Manage worktrees modal's keyboard focus.
    worktrees_focus: FocusHandle,
    /// The ended sessions the daemon keeps, which the Recover dialog lists.
    session_history: recover::SessionHistory,
    /// The Recover dialog, while open.
    recover: Option<recover::RecoverDialog>,
    /// The Recover dialog's keyboard focus.
    recover_focus: FocusHandle,
    /// The devices waiting for remote access and the approval dialog.
    pairing: pairing_view::PairingView,
    /// The approved devices and the block confirm.
    devices: devices_view::DevicesView,
    /// This PC's own pairing with a tailnet host that does not know it.
    device_pairing: device_pairing_view::DevicePairingView,
    /// The device tokens this PC holds per tailnet host.
    tokens: Arc<dyn DeviceTokenStore>,
    /// Ends the wait for a recovery that goes unanswered.
    recover_timer: Option<Task<()>>,
    /// The recoveries this connection sent and has had no answer to, by
    /// request id, each with the requested sessions that were abandoned
    /// (the daemon hands those their old panes itself).
    recover_requests: HashMap<String, HashSet<String>>,
    /// Recovered sessions to place, one batch per answer, waiting for every
    /// snapshot of their batch.
    recover_batches: Vec<Vec<String>>,
    /// Repaints the Needs You panel while it shows, so its waits count up.
    needs_you_timer: Option<Task<()>>,
    /// How many times that timer has repainted the panel.
    needs_you_repaints: usize,
    /// The files being fetched from a remote host.
    fetches: file_fetch::FetchRegistry,
    /// Where the open the run confirm asks about came from.
    run_confirm_origin: open_view::Origin,
    /// The folder fetched files are saved under, or why it could not be
    /// resolved.
    remote_files_root: Result<PathBuf, String>,
}

impl RootView {
    /// A view on the live daemon: the network thread, the config dir's
    /// layout and files, and the system clock.
    pub fn new(
        wanted_session: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let config_dir = daemon_client::config_dir();
        let ui_dir = match &config_dir {
            Ok(dir) => Some(dir.clone()),
            Err(err) => {
                tracing::warn!("sidebar layout will not be saved: {err:#}");
                None
            }
        };
        let ui = ui_dir
            .as_deref()
            .map_or_else(UiState::default, load_ui_state);
        let (out_tx, out_rx) = unbounded();
        let (in_tx, in_rx) = unbounded();
        let tokens = net::device_token_store();
        net::spawn(
            connection_picker::resolve_boot_target(&ui),
            Arc::clone(&tokens),
            out_rx,
            in_tx,
        );
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
            notify: if offscreen::requested() {
                Arc::new(SilentNotifier)
            } else {
                Arc::new(SystemNotifier)
            },
            pick_folder: folder_picker::system_folder_picker(window),
            pick_file: folder_picker::system_file_picker(window),
            autostart: Arc::new(remote_access::SystemAutostart),
            tokens,
        };
        let root = Self::with_transport(deps, window, cx);
        // Presses the toast activator receives, those that arrived before
        // this window among them, go to this window's pairing actions.
        toast_activation::attach(&root.pairing_sink());
        // The cloaked window neither restores nor saves its place.
        if !offscreen::requested() {
            cx.observe_window_bounds(window, |root, window, cx| {
                root.remember_window_place(window, cx);
            })
            .detach();
        }
        root
    }

    /// The daemon the connection is on, or is trying to reach.
    #[must_use]
    pub fn connection_target(&self) -> &target::Target {
        self.conn.target()
    }

    /// Follows the window's activation and bounds. Ctrl stays down in the
    /// window's record while another window has the keyboard, so leaving
    /// the window ends link mode; a resize moves every dot, so an open
    /// gutter menu closes with it rather than point at a row that is no
    /// longer there, and it also re-fits an automatic grid's columns.
    fn watch_window(window: &mut Window, cx: &mut Context<Self>) {
        cx.observe_window_activation(window, |root, window, cx| {
            if !window.is_window_active() {
                root.set_link_mode(false, cx);
            }
        })
        .detach();
        cx.observe_window_bounds(window, |root, window, cx| {
            root.mark_active_panes_resized(cx);
            root.close_shell_menu(window, cx);
            root.schedule_auto_grid_resend(window, cx);
        })
        .detach();
    }

    /// Records where the window now is and schedules a save, unless it is
    /// minimized, has no area, or has not moved.
    fn remember_window_place(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.ui_dir.is_none() || window_state::is_minimized(window) {
            return;
        }
        let display_uuid = window
            .display(cx)
            .and_then(|display| display.uuid().ok())
            .map(|uuid| uuid.to_string());
        let Some(state) = window_state::to_window_state(window.window_bounds(), display_uuid)
        else {
            return;
        };
        if self.sidebar.set_window_state(state) {
            self.schedule_ui_save(cx);
        }
    }

    /// A view over `deps`: it sends through `deps.tx` and handles every
    /// event `deps.events` delivers. A request to close the window goes
    /// through the quit flow.
    #[expect(
        clippy::too_many_lines,
        reason = "One line per RootView field, newest last: the list is the struct, \
                  and a split constructor would hide a field's default among helpers."
    )]
    pub fn with_transport(deps: RootDeps, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let RootDeps {
            tx,
            events,
            ui_dir,
            paths,
            wanted,
            now,
            quit,
            open,
            notify,
            pick_folder,
            pick_file,
            autostart,
            tokens,
        } = deps;
        Self::watch_window(window, cx);
        Self::watch_close_request(window, cx);
        Self::forward_net_events(events, window, cx);
        let drag_escape = Self::intercept_drag_escape(window, cx);
        let ui = ui_dir
            .as_deref()
            .map_or_else(UiState::default, load_ui_state);
        cx.set_global(mouse::CopyOnSelect(ui.general.copy_on_select));
        fonts::set_ui_family(ui.ui_font_family.as_deref(), cx);
        let mut view = Self {
            tx,
            now,
            conn: Connection::for_target(connection_picker::resolve_boot_target(&ui)),
            handshake: None,
            tabs: TabsModel::new(ui.active_tab_id.clone()),
            sidebar: SidebarModel::new(ui),
            panes: HashMap::new(),
            retries: RetryGate::default(),
            grid_bounds: None,
            renaming: None,
            ui_dir,
            drag: None,
            pane_drop: None,
            pane_drag: None,
            pane_drag_session: None,
            tab_drop: None,
            tab_drag: None,
            sidebar_drag: None,
            sidebar_drop: None,
            escape_ended_drag: false,
            _drag_escape: drag_escape,
            popouts: Popouts::default(),
            popout_windows: HashMap::new(),
            popout_focused: None,
            root_focus: cx.focus_handle(),
            sidebar_focus: cx.focus_handle(),
            row_focus: None,
            hovered_container: None,
            rail_tip: activity_bar::RailTipSuppression::default(),
            window_hover_override: None,
            launch_last: launch_last::State::default(),
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
            container_menu: None,
            more_menu: sidebar_view::MoreMenu::Closed,
            appearance_editor: None,
            appearance_focus: cx.focus_handle(),
            settings_tab: settings_view::SettingsTab::default(),
            settings_tabs_focus: cx.focus_handle(),
            settings_control: None,
            settings_ring_control: None,
            keep_awake: None,
            remote_status: None,
            remote_access_tab: None,
            agent_commands: None,
            agents: Vec::new(),
            agent_settings: std::collections::BTreeMap::new(),
            agent_settings_received: false,
            agent_settings_pending: std::collections::BTreeMap::new(),
            agent_setting_applies: std::collections::VecDeque::new(),
            agent_tabs: settings_view::AgentTabs::default(),
            autostart,
            window_title: String::new(),
            title_pending: None,
            title_timer: None,
            container_sends: HashMap::new(),
            tab_menu: None,
            tab_menu_focus: cx.focus_handle(),
            pane_ui: pane_menu::PaneUi::new(cx.focus_handle(), cx.focus_handle()),
            pending_appearance: appearance::InFlightAppearance::default(),
            seen_sessions: HashSet::new(),
            ui_save_deadline: None,
            ui_save_timer: None,
            auto_grid_timer: None,
            confirm: HeaderStopConfirm::default(),
            detached_stop_all: sidebar_view::DetachedStopAll::default(),
            duplicates: Duplicates::default(),
            prefills: Prefills::default(),
            delete_dialog: None,
            dialog_focus: cx.focus_handle(),
            repo_remove: RepoRemove::default(),
            repo_remove_focus: cx.focus_handle(),
            notices: Notices::default(),
            toast_timer: None,
            notice_focus: cx.focus_handle(),
            undo: UndoShelf::default(),
            undo_timer: None,
            spawns: PendingSpawns::default(),
            spawn_dialog: None,
            spawn_focus: cx.focus_handle(),
            shell_dialog: None,
            shell_focus: cx.focus_handle(),
            fetch_dialog: None,
            fetch_focus: cx.focus_handle(),
            connection_picker: None,
            connection_picker_focus: cx.focus_handle(),
            peer_source: Arc::new(SystemPeers),
            host_path_prompt: None,
            host_path_focus: cx.focus_handle(),
            shell_generation: 0,
            quick_shell_saves: PendingQuickShell::default(),
            branch_cache: BranchCache::default(),
            quitter: Quitter::new(quit),
            exit: None,
            quit_focus: cx.focus_handle(),
            layout_chooser: None,
            chooser_focus: cx.focus_handle(),
            pending_arrangement: None,
            import_modal: None,
            pending_import: None,
            awaiting_initial_sessions: false,
            cleanup_failed: cleanup_failed::CleanupQueue::default(),
            cleanup_focus: cx.focus_handle(),
            opener: open,
            notifications: notify::Notifications::new(notify),
            run_confirm: None,
            run_focus: cx.focus_handle(),
            sc: ScModel::default(),
            sc_picker_open: false,
            history: history::HistoryModel::default(),
            sc_layout: history::ScLayout::default(),
            changes: ChangesUi::new(cx.focus_handle()),
            stash: StashUi::new(cx.focus_handle()),
            diff_tabs: HashMap::new(),
            diff_opens: diff_tab::DiffOpens::default(),
            viewer_tabs: HashMap::new(),
            viewer_opens: viewer_tab::ViewerOpens::default(),
            viewer_focus: cx.focus_handle(),
            last_active_tab: None,
            last_focused_session: None,
            pick_folder,
            pick_file,
            preset_wizard: None,
            preset_wizard_focus: cx.focus_handle(),
            preset_launches: preset_launch::PresetLaunches::default(),
            launched_highlight: preset_launch::LaunchHighlight::default(),
            preset_launch_syncs: 0,
            workspace_creator: None,
            workspace_creator_focus: cx.focus_handle(),
            vscode_suggestions: VscodeSuggestions::default(),
            vscode_prompt_focus: cx.focus_handle(),
            cwd_workspace_scans: HashMap::new(),
            worktrees_root: None,
            worktrees_tab: None,
            worktrees_manager: None,
            worktrees_focus: cx.focus_handle(),
            session_history: recover::SessionHistory::default(),
            recover: None,
            recover_focus: cx.focus_handle(),
            pairing: pairing_view::PairingView::new(
                cx.focus_handle(),
                Self::forward_pairing_actions(window, cx),
            ),
            devices: devices_view::DevicesView::new(cx.focus_handle()),
            device_pairing: device_pairing_view::DevicePairingView::new(cx.focus_handle()),
            tokens,
            recover_timer: None,
            recover_requests: HashMap::new(),
            recover_batches: Vec::new(),
            needs_you_timer: None,
            needs_you_repaints: 0,
            fetches: file_fetch::FetchRegistry::new(&format!(
                "{:x}",
                uuid::Uuid::new_v4().as_u64_pair().0
            )),
            run_confirm_origin: open_view::Origin::Link,
            remote_files_root: daemon_client::remote_files_dir()
                .map(file_fetch_view::absolute_root)
                .map_err(|err| format!("{err:#}")),
        };
        view.arm_needs_you_repaint(cx);
        view
    }

    /// Sends a request to close the window through the quit flow; a close
    /// that goes ahead closes every pop-out window first.
    fn watch_close_request(window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            // A view that is gone has nothing to ask; let the window close.
            view.update(cx, |root, cx| {
                let close = root.on_close_requested(window, cx);
                if close {
                    root.close_popouts(cx);
                }
                close
            })
            .unwrap_or(true)
        });
    }

    /// Handles every event `events` delivers, until the view is gone.
    fn forward_net_events(
        mut events: UnboundedReceiver<NetEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
    }

    /// Opens `key` in a window of its own, or activates the open pop-out
    /// that already shows it. Only a pane pop-out opens; the other kinds
    /// open nothing and report themselves as already open.
    pub fn open_popout(
        &mut self,
        key: PopoutKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> OpenOutcome {
        if !matches!(key, PopoutKey::Pane { .. }) {
            tracing::debug!(?key, "pop-out kind not supported yet");
            return OpenOutcome::Existing(key);
        }
        let outcome = self.popouts.open(key.clone(), self.tabs.tabs());
        match &outcome {
            OpenOutcome::Opened => {
                self.unfocus_popped_pane(&key, window, cx);
                self.update_pane_roles(cx);
                self.mark_popout_resized(&key, cx);
                self.open_popout_window(key, window, cx);
            }
            OpenOutcome::Existing(open) | OpenOutcome::Overlaps(open) => {
                self.activate_popout(open, cx);
            }
        }
        cx.notify();
        outcome
    }

    /// The main window lets go of `key`'s pane as it pops out: a terminal
    /// that had the keyboard there hands it to the main window's own focus,
    /// so the app's shortcuts work there again.
    fn unfocus_popped_pane(&self, key: &PopoutKey, window: &mut Window, cx: &Context<Self>) {
        let PopoutKey::Pane { pane_id, .. } = key else {
            return;
        };
        let focused = self
            .panes
            .get(pane_id)
            .is_some_and(|slot| slot.view().read(cx).is_focused(window));
        if focused {
            self.root_focus.focus(window);
        }
    }

    /// Opens the window for `key`, already in the registry, at its cascade
    /// place on the main window's display, and gives the keyboard to its
    /// terminal.
    fn open_popout_window(&mut self, key: PopoutKey, window: &Window, cx: &mut Context<Self>) {
        let cascade = self.popouts.cascade_index(&key).unwrap_or(0);
        let display_id = window.display(cx).map(|display| display.id());
        let bounds = popout_bounds(display_id, cascade, cx);
        let title = popouts::title(
            &key,
            self.tabs.tabs(),
            self.sidebar.sessions(),
            &self.agents,
        );
        let terminal = self.popout_terminal(&key);
        let opening = PopoutOpening {
            key,
            display_id,
            bounds,
            title,
            terminal,
        };
        let root = cx.weak_entity();
        // The new window draws at once and reads this view, so it opens once
        // this update is over.
        cx.defer(move |cx| open_popout_now(&root, opening, cx));
    }

    /// The terminal `key`'s pop-out draws: a pane pop-out's pane while some
    /// tab holds it. The pane is judged by its id, not by the tab the key
    /// names, so the fold in which a moved pane is in its old tab and its
    /// new one still finds it.
    pub(crate) fn popout_terminal(
        &self,
        key: &PopoutKey,
    ) -> Option<gpui::Entity<term_view::TerminalPane>> {
        let PopoutKey::Pane { pane_id, .. } = key else {
            return None;
        };
        popouts::pane_in_tabs(pane_id, self.tabs.tabs())?;
        self.panes.get(pane_id).map(|slot| slot.view().clone())
    }

    /// Pane `pane_id`'s pop-out window became active with its terminal
    /// focused (`shown`), or stopped being either: a session shown that way
    /// counts as shown, so its finished turn is seen.
    pub(crate) fn popout_terminal_focus(
        &mut self,
        pane_id: &str,
        shown: bool,
        cx: &mut Context<Self>,
    ) {
        if shown {
            self.popout_focused = Some(pane_id.to_owned());
            if let Some(session) = self.popout_focused_session() {
                self.sidebar.mark_seen(&session);
            }
        } else if self.popout_focused.as_deref() == Some(pane_id) {
            self.popout_focused = None;
        }
        cx.notify();
    }

    /// The session a pane pop-out's terminal shows with its window's
    /// keyboard, while that pane is still popped out.
    fn popout_focused_session(&self) -> Option<String> {
        let pane_id = self.popout_focused.as_deref()?;
        self.popouts.popped_key(pane_id)?;
        let session = popouts::pane_in_tabs(pane_id, self.tabs.tabs())?.session?;
        Some(session.to_owned())
    }

    /// Closes the pop-outs whose content has gone: a pane pop-out whose pane
    /// is in no tab or whose session the daemon removed. Their panes take
    /// their roles in the main window back.
    fn prune_popouts(&mut self, cx: &mut Context<Self>) {
        let gone = self.popouts.prune(&self.tabs);
        if gone.is_empty() {
            return;
        }
        self.update_pane_roles(cx);
        for key in &gone {
            tracing::debug!(?key, "closing a pop-out whose content has gone");
            self.mark_popout_resized(key, cx);
            if let Some(popout) = self.popout_windows.remove(key) {
                close_popout_window(key, popout.handle, cx);
            }
        }
        cx.notify();
    }

    /// Brings `key`'s pop-out window to the front and gives the keyboard to
    /// its pane's terminal; a cloaked client never activates a window.
    fn activate_popout(&self, key: &PopoutKey, cx: &mut Context<Self>) {
        let Some(PopoutWindow { handle, .. }) = self.popout_windows.get(key) else {
            return;
        };
        let terminal = self.popout_terminal(key);
        let activate = !offscreen::requested();
        let raised = handle.update(cx, |_, window, cx| {
            if activate {
                window.activate_window();
            }
            if let Some(terminal) = &terminal {
                terminal.read(cx).focus(window);
            }
        });
        if let Err(err) = raised {
            tracing::warn!(?key, "activating a pop-out window: {err:#}");
        }
    }

    /// Docks `key` back: its content returns to the main window, and its
    /// window closes if it is still open. Nothing goes to the daemon.
    pub fn dock_back(&mut self, key: &PopoutKey, cx: &mut Context<Self>) {
        if let Some(handle) = self.forget_popout(key, cx) {
            close_popout_window(key, handle, cx);
        }
    }

    /// Drops `key` from the open pop-outs without touching its window, as
    /// its own close request does; the window it had, if any.
    pub(crate) fn forget_popout(
        &mut self,
        key: &PopoutKey,
        cx: &mut Context<Self>,
    ) -> Option<WindowHandle<PopoutView>> {
        if self.popouts.close(key) {
            self.update_pane_roles(cx);
            self.mark_popout_resized(key, cx);
        }
        let handle = self.popout_windows.remove(key).map(|popout| popout.handle);
        cx.notify();
        handle
    }

    /// Drops the pop-out whose window is `window_id`, as that window's own
    /// close request does; the window's key may have followed its pane to
    /// another tab since it opened.
    pub(crate) fn forget_popout_window(&mut self, window_id: WindowId, cx: &mut Context<Self>) {
        let key = self
            .popout_windows
            .iter()
            .find(|(_, popout)| AnyWindowHandle::from(popout.handle).window_id() == window_id)
            .map(|(key, _)| key.clone());
        if let Some(key) = key {
            self.forget_popout(&key, cx);
        } else {
            tracing::debug!(?window_id, "a closed window is no open pop-out");
        }
    }

    /// A tab change moved popped panes to other tabs: their pop-outs follow
    /// them, windows and all.
    fn follow_popout_moves(&mut self, cx: &mut Context<Self>) {
        for (old, new) in self.popouts.follow_moves(&self.tabs) {
            let Some(popout) = self.popout_windows.remove(&old) else {
                continue;
            };
            let key = new.clone();
            let followed = popout.handle.update(cx, |view, _, cx| {
                view.follow(key);
                cx.notify();
            });
            if let Err(err) = followed {
                tracing::debug!(?new, "the moved pane's pop-out window has gone: {err:#}");
            }
            self.popout_windows.insert(new, popout);
        }
    }

    /// Sets each open pop-out window's title to what its content now reads,
    /// after a rename or a re-bind; a title that did not change is left.
    fn refresh_popout_titles(&mut self, cx: &mut Context<Self>) {
        let (tabs, sessions, agents) = (self.tabs.tabs(), self.sidebar.sessions(), &self.agents);
        for (key, popout) in &mut self.popout_windows {
            let title = popouts::title(key, tabs, sessions, agents);
            if title == popout.title {
                continue;
            }
            let set = popout
                .handle
                .update(cx, |_, window, _| window.set_window_title(&title));
            match set {
                Ok(()) => popout.title = title,
                Err(err) => tracing::debug!(?key, "retitling a pop-out window: {err:#}"),
            }
        }
    }

    /// `key`'s pane moves between windows: its next size claims its session.
    fn mark_popout_resized(&self, key: &PopoutKey, cx: &mut Context<Self>) {
        if let PopoutKey::Pane { pane_id, .. } = key {
            self.mark_pane_resized(pane_id, cx);
        }
    }

    /// Closes every pop-out window, as the main window closes.
    fn close_popouts(&mut self, cx: &mut Context<Self>) {
        for (key, popout) in std::mem::take(&mut self.popout_windows) {
            self.popouts.close(&key);
            close_popout_window(&key, popout.handle, cx);
        }
    }

    /// Whether pane `pane_id` of tab `tab_id` draws in a pop-out window.
    #[must_use]
    pub fn is_popped_out(&self, tab_id: &str, pane_id: &str) -> bool {
        self.popouts.is_open(&PopoutKey::Pane {
            tab_id: tab_id.to_owned(),
            pane_id: pane_id.to_owned(),
        })
    }

    /// Whether pane `pane_id` draws in a pop-out window, whichever tab the
    /// pop-out's key names.
    #[must_use]
    pub fn is_pane_popped(&self, pane_id: &str) -> bool {
        self.popouts.popped_key(pane_id).is_some()
    }

    /// How many pop-out windows are open.
    #[must_use]
    pub fn popout_count(&self) -> usize {
        self.popouts.keys().len()
    }

    /// The sidebar's containers for the view it shows, in the order it
    /// shows them.
    #[must_use]
    pub fn sidebar_containers(&self) -> Vec<Container> {
        match self.sidebar.sidebar_view() {
            SidebarView::Repos => self.sidebar.display_containers(),
            SidebarView::Tabs if self.tabs.is_loaded() => {
                self.sidebar.tab_containers(self.tabs.tabs())
            }
            SidebarView::Tabs => Vec::new(),
        }
    }

    /// The `.code-workspace` file a DIR or SH container's folder holds, once
    /// its scan answered; `None` while the answer is pending or empty.
    #[must_use]
    pub fn cwd_workspace_suggestion(&self, path: &str) -> Option<&VscodeWorkspaceSuggestion> {
        self.cwd_workspace_scans
            .get(&scan_key(path))
            .and_then(Option::as_ref)
    }

    /// Asks the daemon to scan each DIR or SH container's folder for a
    /// `.code-workspace` file, once per distinct folder on a connection.
    fn scan_unseen_cwd_paths(&mut self) {
        if !self.conn.is_open() {
            return;
        }
        for container in self.sidebar.containers() {
            if !matches!(container.kind, ContainerKind::Shell | ContainerKind::Dir) {
                continue;
            }
            let Some(path) = container.path else {
                continue;
            };
            let key = scan_key(&path);
            if self.cwd_workspace_scans.contains_key(&key) {
                continue;
            }
            self.cwd_workspace_scans.insert(key, None);
            self.send(ClientMessage::ScanVscodeWorkspaces { path });
        }
    }

    /// How the sessions panel groups its sessions.
    #[must_use]
    pub fn sidebar_view(&self) -> SidebarView {
        self.sidebar.sidebar_view()
    }

    /// Which repo and workspace rows the repos view shows.
    #[must_use]
    pub fn repo_filter(&self) -> RepoFilter {
        self.sidebar.repo_filter()
    }

    /// The container row the sidebar's keys act on, if any.
    #[must_use]
    pub fn sidebar_row_focus(&self) -> Option<&str> {
        self.row_focus.as_deref()
    }

    /// How many sessions are abandoned, the set the ⋯ menu's bulk resume
    /// acts on.
    #[must_use]
    pub fn sidebar_abandoned_count(&self) -> usize {
        self.sidebar.abandoned_count()
    }

    /// Which tabs show session `id`, as its leaf's pill tells it; `None`
    /// until the daemon's tab list is in, when leaves show no pill.
    #[must_use]
    pub fn leaf_tab_pill(&self, id: &str) -> Option<TabPill> {
        if !self.tabs.is_loaded() {
            return None;
        }
        let pill = tab_pills(self.tabs.tabs()).remove(id);
        Some(pill.unwrap_or(TabPill::Unbound))
    }

    /// Groups the sessions panel by `view`, saving the choice when it
    /// changed.
    pub(crate) fn set_sidebar_view(&mut self, view: SidebarView, cx: &mut Context<Self>) {
        if self.sidebar.set_sidebar_view(view) {
            self.row_focus = None;
            self.save_ui();
        }
        cx.notify();
    }

    /// Shows every repo and workspace row, or only the active ones, saving
    /// the choice when it changed.
    pub(crate) fn set_repo_filter(&mut self, filter: RepoFilter, cx: &mut Context<Self>) {
        if self.sidebar.set_repo_filter(filter) {
            self.row_focus = None;
            self.save_ui();
        }
        cx.notify();
    }

    /// The unbound pill: opens session `id` in a new tab, when it can be
    /// shown in a pane.
    pub(crate) fn open_in_new_tab(&mut self, id: &str) {
        if !self.sidebar.session(id).is_some_and(can_attach) {
            return;
        }
        let request_id = new_request_id();
        self.tabs.arm_create(&request_id);
        self.send(Placement::NewTab.message(id, Some(request_id)));
    }

    /// What the client may do on the daemon it connects to now.
    fn capabilities(&self) -> Capabilities {
        Capabilities::for_target(self.conn.target())
    }

    /// Asks this PC's `pick` picker, opening at `start`, and hands its
    /// answer to `then`. Every picker goes through here: none opens on a
    /// remote host (returns `false`), and an answer that lands after the
    /// target turned remote reaches `then` as a cancel.
    fn pick_local(
        &mut self,
        pick: Pick,
        start: Option<PathBuf>,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, Option<PathBuf>, &mut Context<Self>) + 'static,
    ) -> bool {
        if !self.capabilities().opens_local_files() {
            tracing::info!(?pick, "no picker opens on a remote host");
            return false;
        }
        let picked = match &pick {
            Pick::Folder => (self.pick_folder)(cx, start),
            Pick::File(filter) => (self.pick_file)(cx, start, filter.clone()),
        };
        cx.spawn(async move |this, cx| {
            let picked = picked.await;
            // Fails only when the view is gone, and its window with it.
            this.update(cx, |this, cx| {
                let picked = if this.capabilities().opens_local_files() {
                    picked
                } else {
                    if picked.is_some() {
                        tracing::info!(
                            ?pick,
                            "dropping a pick that landed after the switch to a remote host"
                        );
                    }
                    None
                };
                then(this, picked, cx);
            })
            .ok();
        })
        .detach();
        true
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

    /// Whether a dialog, modal or menu is drawn over the panes and holds the
    /// keyboard: the Settings modal, the spawn, Shell…, Fetch file…,
    /// delete-worktree, repo/workspace Remove, discard, stash drop, run,
    /// recover, Manage worktrees, layout chooser, cleanup-failed, exit and
    /// pane dialogs and menus, or a modal notice.
    fn overlay_open(&self) -> bool {
        self.spawn_dialog.is_some()
            || self.shell_dialog.is_some()
            || self.fetch_dialog.is_some()
            || self.host_path_prompt.is_some()
            || self.appearance_editor.is_some()
            || self.worktrees_manager.is_some()
            || self.recover.is_some()
            || self.pairing_dialog_open()
            || self.device_pairing_open()
            || self.delete_dialog.is_some()
            || self.repo_remove.dialog().is_some()
            || self.workspace_creator.is_some()
            || self.preset_wizard.is_some()
            || self.vscode_prompt_open()
            || self.changes.discard.is_some()
            || self.changes.file_menu.is_some()
            || self.stash.drop.is_some()
            || self.run_confirm.is_some()
            || self.notices.has_modal()
            || self.layout_chooser.is_some()
            || self.import_modal.is_some()
            || !self.cleanup_failed.is_empty()
            || self.exit.is_some()
            || self.pane_ui.close.is_some()
            || self.pane_ui.move_panes.is_some()
            || self.pane_ui.menu.is_some()
            || self.connection_picker.is_some()
    }

    /// Whether a pane of the tab on screen shows `session_id` with nothing
    /// drawn over the panes; a diff tab shows none.
    pub(crate) fn session_on_screen(&self, session_id: &str) -> bool {
        !self.overlay_open()
            && self
                .tabs
                .active_tab()
                .and_then(TabEntry::grid)
                .is_some_and(|grid| {
                    crate::tabs::collect_panes(grid)
                        .iter()
                        .any(|pane| pane.session == Some(session_id))
                })
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

    /// Whether the sidebar has the main window's keyboard.
    #[must_use]
    pub fn sidebar_focused(&self, window: &Window) -> bool {
        self.sidebar_focus.is_focused(window)
    }

    /// The id of pane `pane_id`'s terminal, which stays the same for as long
    /// as the pane keeps its terminal.
    #[must_use]
    pub fn pane_terminal_id(&self, pane_id: &str) -> Option<gpui::EntityId> {
        Some(self.panes.get(pane_id)?.view().entity_id())
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

    /// Makes `family` (`None`: the default) the family the interface draws
    /// in: saved on this machine and applied to every window at once.
    pub fn set_ui_font(&mut self, family: Option<String>, cx: &mut Context<Self>) {
        self.sidebar.set_ui_font_family(family);
        let stored = self.sidebar.ui_state().ui_font_family.as_deref();
        // The open editor has the installed families listed already.
        match self.appearance_editor.as_ref() {
            Some(editor) => fonts::set_ui_family_among(stored, editor.system_families(), cx),
            None => fonts::set_ui_family(stored, cx),
        }
        self.save_ui();
        cx.refresh_windows();
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
        self.apply_pane_fonts(true, cx);
        cx.notify();
    }

    /// Drops the font overrides and the sidebar folds of tabs the daemon no
    /// longer lists, once it has listed any: before the first full list
    /// nothing is known to be gone, so an update for one tab prunes nothing.
    /// A remote host's tab list prunes nothing: the saved entries are the
    /// local daemon's.
    fn prune_tab_font_sizes(&mut self) {
        if !self.tabs.is_loaded() || !self.conn.target().is_local() {
            return;
        }
        let live: HashSet<&str> = self.tabs.tabs().iter().map(|tab| tab.id.as_str()).collect();
        let fonts = self.sidebar.prune_tab_font_sizes(&live);
        let folds = self.sidebar.prune_tab_folds(&live);
        if fonts || folds {
            self.save_ui();
        }
    }

    /// Drops the source-control pin and collapse state of repos a registry
    /// snapshot no longer lists, saving when any went. A remote host's list
    /// prunes nothing: the saved entries are the local daemon's.
    fn prune_source_control(&mut self, repos: &[RepoEntry]) {
        if self.conn.target().is_local() && self.sidebar.prune_source_control(repos) {
            self.save_ui();
        }
    }

    /// A repo or workspace snapshot: a container's appearance is a level
    /// above the session's, so the panes' fonts follow it.
    fn on_containers_changed(&mut self, cx: &mut Context<Self>) {
        let own_change = self.settle_container_sends();
        self.apply_pane_fonts(own_change, cx);
        // A registry change regroups shells: one can gain a new SH or DIR
        // container whose folder has not been scanned.
        self.scan_unseen_cwd_paths();
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

    /// Writes the layout at most once per [`UI_SAVE_DEBOUNCE`] while tab
    /// font steps or window moves keep coming, so a held key or a drag does
    /// not rewrite the file for every step; the save that fires writes the
    /// layout as it then stands.
    fn schedule_ui_save(&mut self, cx: &mut Context<Self>) {
        if self.ui_save_deadline.is_some() {
            return;
        }
        let deadline = (self.now)() + UI_SAVE_DEBOUNCE;
        self.ui_save_deadline = Some(deadline);
        self.arm_ui_save(deadline, cx);
    }

    /// Arms the timer for the rest of the wait for the pending save.
    fn arm_ui_save(&mut self, deadline: Instant, cx: &mut Context<Self>) {
        let delay = deadline.saturating_duration_since((self.now)());
        self.ui_save_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            // Fails only when the view is gone, and the save with it.
            this.update(cx, Self::tick_ui_save).ok();
        }));
    }

    /// The save timer fired: write once the debounce has passed by the
    /// clock, which may lag the timer, else wait out the rest.
    fn tick_ui_save(&mut self, cx: &mut Context<Self>) {
        let Some(deadline) = self.ui_save_deadline else {
            return;
        };
        if deadline > (self.now)() {
            self.arm_ui_save(deadline, cx);
        } else {
            self.flush_ui_save();
        }
    }

    /// Writes a pending save now, as the quit path must before the app goes.
    fn flush_ui_save(&mut self) {
        if self.ui_save_deadline.take().is_some() {
            self.ui_save_timer = None;
            self.save_ui();
        }
    }

    /// Schedules the automatic grids' re-pick after
    /// [`AUTO_GRID_RESIZE_DEBOUNCE`], when a tab is automatic at all;
    /// `watch_window`'s `observe_window_bounds` calls this. Every resize during
    /// the wait replaces the timer, so a drag settles as one re-pick; the timer
    /// reads the aspect then, since the pane area recorded by the frame before
    /// this callback is the size the window just left.
    pub fn schedule_auto_grid_resend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tabs.tabs().iter().any(is_auto_grid) {
            return;
        }
        self.auto_grid_timer = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(AUTO_GRID_RESIZE_DEBOUNCE)
                .await;
            // Fails only when the view is gone, and the re-pick with it.
            this.update_in(cx, |this, _window, _cx| {
                let aspect = this.pane_area_aspect();
                this.auto_grid_resends_at(aspect);
            })
            .ok();
        }));
    }

    /// The debounced automatic-grid re-send, run now for a pane area `aspect`
    /// wide per unit of height: one `RearrangeTab` per automatic tab whose
    /// stored aspect is not that one, returning how many were sent. The timer
    /// above runs it with the measured aspect; a spec calls it directly.
    pub fn auto_grid_resends_at(&mut self, aspect: f32) -> usize {
        let resends = auto_grid_resends(self.tabs.tabs(), aspect);
        for (tab_id, layout) in &resends {
            self.send(ClientMessage::RearrangeTab {
                tab_id: tab_id.clone(),
                layout: *layout,
            });
        }
        resends.len()
    }

    /// Hide or show the sidebar; hiding it hands the keyboard back to the
    /// active tab's focused pane, since the sidebar may have held it.
    fn toggle_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_shell_menu(window, cx);
        self.close_more_menu(window, cx);
        self.close_sc_picker(window, cx);
        self.close_sc_file_menu(window, cx);
        self.sidebar.toggle_sidebar();
        // The fold always gives the panes a new width.
        self.mark_active_panes_resized(cx);
        // The Stop all chip and the container rows live only in the sessions
        // panel, so a fold takes any arm and the row focus and hover with
        // it.
        if self.sidebar.is_collapsed() {
            self.detached_stop_all.disarm();
            self.row_focus = None;
            self.hovered_container = None;
        }
        self.drag = None;
        self.save_ui();
        if self.sidebar.is_collapsed() {
            self.focus_active_pane(window, cx);
        } else {
            self.seed_history();
        }
        self.arm_needs_you_repaint(cx);
        cx.notify();
    }

    fn toggle_container(&mut self, key: &str) {
        self.sidebar.toggle_container(key);
        self.save_ui();
    }

    /// Focuses a container row, so the sidebar's keys fold it and the ring
    /// follows it.
    pub fn focus_container_row(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.row_focus = Some(key.to_owned());
        self.sidebar_focus.focus(window);
        cx.notify();
    }

    /// Sets whether the pointer counts as in the window, in place of the
    /// platform's answer; `None` asks the platform again.
    pub fn set_window_hover_override(&mut self, hovered: Option<bool>, cx: &mut Context<Self>) {
        self.window_hover_override = hovered;
        cx.notify();
    }

    /// Records the container row under the pointer, redrawing only when the
    /// row changes.
    pub(crate) fn set_hovered_container(&mut self, key: &str, over: bool, cx: &mut Context<Self>) {
        let next = if over {
            Some(key.to_owned())
        } else if self.hovered_container.as_deref() == Some(key) {
            None
        } else {
            self.hovered_container.clone()
        };
        if next != self.hovered_container {
            self.hovered_container = next;
            cx.notify();
        }
    }

    /// The ▶ or a double-click on a repo or workspace row: replay its last
    /// launch, or open the spawn dialog when there is nothing to replay. A
    /// second gesture for the same target inside the double-click interval
    /// is the same gesture, so it launches nothing; a spec also calls this
    /// where a modal covers the row.
    pub fn launch_last_row(
        &mut self,
        container: launch_last::Container<'_>,
        placement: launch_last::Placement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let gesture_target = match container {
            launch_last::Container::Repo(id) => SuggestTarget::Repo {
                repo_id: id.to_owned(),
            },
            launch_last::Container::Workspace(id) => SuggestTarget::Workspace {
                workspace_id: id.to_owned(),
            },
        };
        if !self
            .launch_last
            .note_activation(&gesture_target, (self.now)())
        {
            return;
        }
        let Some(config) = self.sidebar.last_spawn_config(container) else {
            let target = match container {
                launch_last::Container::Repo(id) => Target::Repo(id.to_owned()),
                launch_last::Container::Workspace(id) => Target::Workspace(id.to_owned()),
            };
            let lock = Lock::for_target(target);
            if lock
                .unregistered(self.sidebar.repos(), self.sidebar.workspaces())
                .is_none()
            {
                self.open_spawn_dialog(
                    SpawnEntry::Locked {
                        lock,
                        preselect: None,
                    },
                    window,
                    cx,
                );
            }
            return;
        };
        if config.mode == SessionMode::Headless {
            self.push_toast(ToastKind::Warning, HEADLESS_UNSUPPORTED_TIP, None, cx);
            return;
        }
        let Some(target) = Target::from_spawn(&config.target) else {
            return;
        };
        let placement = self.launch_last_placement(placement);
        if config.dangerously_skip_permissions {
            let prefill = Prefill {
                config: Some(config),
                open_in: Some(placement.open_choice()),
            };
            if self.open_spawn_dialog(
                SpawnEntry::Duplicate {
                    lock: Some(Lock::for_target(target)),
                    prefill: Box::new(prefill),
                },
                window,
                cx,
            ) {
                self.push_toast(
                    ToastKind::Warning,
                    TRUSTED_LAUNCH_TITLE,
                    Some(TRUSTED_LAUNCH_DETAIL.to_owned()),
                    cx,
                );
            }
            return;
        }
        if let Some(target) = launch_last::suggestion_target(&config) {
            let (generation, request_id) =
                self.launch_last.begin(config, placement, target.clone());
            self.send(ClientMessage::SuggestBranchName {
                target,
                request_id: Some(request_id),
            });
            Self::arm_launch_last_timeout(generation, cx);
            return;
        }
        let request = launch_last::replay_request(&config, None);
        let open_in = placement.open_in();
        self.spawn(request, open_in, cx);
    }

    /// The placement a click resolved, with the tab on screen read now.
    fn launch_last_placement(&self, placement: launch_last::Placement) -> launch_last::Placement {
        match placement {
            launch_last::Placement::CurrentTab => {
                let tabs =
                    spawn_form::TabChoices::from_tabs(self.tabs.tabs(), self.tabs.active_id());
                launch_last::Placement::from_open_in(spawn_form::current_tab_open_in(&tabs))
            }
            placement => placement,
        }
    }

    /// Arms the branch wait's deadline: past it, nothing is spawned and a
    /// toast says why. A later click replaces the wait, and its timer with
    /// it.
    fn arm_launch_last_timeout(generation: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(spawn_form::SUGGESTION_TIMEOUT)
                .await;
            // Fails only when the view is gone, and the wait with it.
            this.update(cx, |this, cx| this.launch_last_expired(generation, cx))
                .ok();
        })
        .detach();
    }

    /// The branch wait expired without a reply: the launch is dropped and a
    /// toast names the failure.
    fn launch_last_expired(&mut self, generation: u64, cx: &mut Context<Self>) {
        if self.launch_last.expire(generation).is_none() {
            return;
        }
        self.push_toast(
            ToastKind::Error,
            LAUNCH_LAST_TIMEOUT_TITLE,
            Some(LAUNCH_LAST_TIMEOUT_DETAIL.to_owned()),
            cx,
        );
    }

    /// A branch-name reply for the launch-last wait: the spawn goes out with
    /// the fresh name. Only the reply echoing the wait's own `request_id`
    /// finishes it, so the spawn dialog's suggestion does not.
    fn on_launch_last_message(&mut self, msg: &DaemonMessage, cx: &mut Context<Self>) {
        let DaemonMessage::BranchNameSuggestion {
            target,
            name,
            request_id,
        } = msg
        else {
            return;
        };
        let Some(pending) = self.launch_last.take_match(target, request_id.as_deref()) else {
            return;
        };
        // The name just went to this launch, so a spawn dialog opened later
        // must not prefill it as free.
        if let Some(target) = Target::from_suggest(target) {
            self.branch_cache.remove(&target);
        }
        let request = launch_last::replay_request(&pending.config, Some(name.clone()));
        let open_in = pending.placement.open_in();
        self.spawn(request, open_in, cx);
    }

    /// Drops the row focus when its container is no longer listed.
    fn prune_container_row_focus(&mut self) {
        let Some(key) = self.row_focus.as_deref() else {
            return;
        };
        if !self.sidebar_containers().iter().any(|c| c.key == key) {
            self.row_focus = None;
        }
    }

    /// A session click (a sidebar leaf, a Needs You row): clears the Select
    /// launched marks and shows the session; see [`Self::show_session`].
    fn select_session(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.launched_highlight.clear();
        self.show_session(id, window, cx);
    }

    /// Shows the pane holding the session, or places the session when no
    /// pane shows it. A session without a terminal only has its attention
    /// and its unseen turn cleared.
    fn show_session(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar.clear_attention(id);
        self.sidebar.mark_seen(id);
        if let Some(key) = self.popouts.showing_session(id, self.tabs.tabs()).cloned() {
            // Its pane draws in a pop-out: that window comes to the front.
            self.activate_popout(&key, cx);
        } else if let Some((tab_id, pane_id)) = find_tab_containing_session(self.tabs.tabs(), id) {
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
    pub(crate) fn place_session(
        &mut self,
        session: &SessionSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let placement = self.tabs.place(session, self.sidebar.sessions());
        let request_id = match &placement {
            Placement::NewTab => {
                let request_id = new_request_id();
                self.tabs.arm_create(&request_id);
                Some(request_id)
            }
            Placement::Pane {
                tab_id,
                target: PaneTarget::Replace { pane_id },
            } => {
                self.tabs.focus_pane(tab_id, pane_id);
                self.after_tabs_change(window, cx);
                None
            }
            Placement::Pane { .. } => None,
        };
        let msg = placement.message(&session.id, request_id);
        self.tabs.note_layout_request(&msg);
        self.send(msg);
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
        self.show_session(&id, window, cx);
    }

    /// After any change to the tab model: terminals follow the layout, the
    /// requested pane takes the keyboard (unless the spawn dialog, the
    /// delete-worktree confirm, the discard or stash drop confirm, the run
    /// confirm, the source-control file menu or a modal notice holds it;
    /// closing it focuses the tab's pane), the spawn dialog's Open in
    /// follows the tabs and the active tab is saved. The session the last
    /// terminal tab focused is remembered before a diff tab takes over. A
    /// diff or viewer tab an open waited for is activated, and a diff or
    /// viewer tab that becomes active takes the keyboard.
    fn after_tabs_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.remember_focused_session();
        self.activate_pending_diff_tab();
        self.reconcile_viewer_tabs(cx);
        self.activate_pending_viewer_tab();
        self.reconcile_panes(window, cx);
        self.reconcile_diff_tabs(window, cx);
        self.prune_tab_font_sizes();
        self.apply_pane_fonts(false, cx);
        self.refresh_spawn_tabs();
        let keyboard_free = !self.overlay_open();
        if let Some(pane_id) = self.tabs.take_focus_request()
            && keyboard_free
        {
            self.focus_pane_view(&pane_id, window, cx);
        }
        let active = self.tabs.active_id().map(str::to_owned);
        if active != self.last_active_tab {
            self.last_active_tab = active;
            if keyboard_free && !self.focus_active_diff_tab(window, cx) {
                self.focus_active_viewer_tab(window);
            }
            self.reload_active_viewer(cx);
        }
        // The saved active tab is the local daemon's; a remote host's tab
        // ids are neither remembered nor saved.
        if self.conn.target().is_local() && self.sidebar.set_active_tab(self.tabs.active_id()) {
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
        let dropped_pane_ui = self.drop_stale_pane_ui();
        let dropped_picker = self.drop_stale_sc_picker();
        self.try_wanted_session(window, cx);
        let dropped_changes = self.seed_source_control(window, cx);
        if dropped_pane_ui || dropped_picker || dropped_changes {
            self.after_notice_closed(window, cx);
        }
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
                self.mark_active_panes_resized(cx);
            }
            Some(Drag::Divider {
                tab_id,
                split_path,
                origin,
                ratio,
            }) => {
                let travelled = (at.0 - origin.0).hypot(at.1 - origin.1);
                if (ratio.is_some() || travelled >= DIVIDER_DRAG_THRESHOLD)
                    && let Some(next) =
                        divider_ratio(&self.tabs, self.grid_bounds, tab_id, split_path, at)
                {
                    self.tabs.set_ratio(tab_id, split_path, next);
                    *ratio = Some(next);
                    let tab_id = tab_id.clone();
                    self.mark_tab_panes_resized(&tab_id, cx);
                }
            }
            Some(Drag::ScSplit) => self.drag_sc_split(at.1),
            Some(Drag::HistoryList(key_id)) => {
                let key_id = key_id.clone();
                self.drag_history_list(&key_id, at.1);
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
        let pane_drop = self.pane_drop.take().is_some();
        let tab_drop = self.tab_drop.take().is_some();
        let sidebar_drop = self.sidebar_drop.take().is_some();
        if pane_drop || tab_drop || sidebar_drop {
            cx.notify();
        }
    }

    /// Lets Esc end a pane or pill drag before gpui dispatches the key to
    /// any keymap action, so a focused text input that binds Esc never sees
    /// it. gpui runs keystroke interceptors before keymap actions and skips
    /// the actions once one stops propagation; it still hands that key to
    /// the first capture listener on the focus path, the root's
    /// [`Self::on_key_capture`], which drops it.
    fn intercept_drag_escape(window: &mut Window, cx: &mut Context<Self>) -> gpui::Subscription {
        let root = cx.weak_entity();
        let handle = window.window_handle();
        cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle() != handle {
                return;
            }
            match root.update(cx, |this, cx| {
                this.on_drag_escape(&event.keystroke, window, cx)
            }) {
                Ok(true) => cx.stop_propagation(),
                Ok(false) => {}
                Err(err) => tracing::warn!(%err, "drag Esc: the window's root view is gone"),
            }
        })
    }

    /// Esc while a pane or pill drag is under way ends it, whatever has the
    /// keyboard; remembers whether it did, for the root's key capture, so
    /// every key press sets it afresh. Returns whether it did.
    fn on_drag_escape(
        &mut self,
        ks: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.escape_ended_drag = ks.key == "escape" && self.end_drag(window, cx);
        self.escape_ended_drag
    }

    /// Whether the key press under way is an Esc that ended a drag; it is
    /// answered once.
    fn take_drag_escape(&mut self) -> bool {
        std::mem::take(&mut self.escape_ended_drag)
    }

    /// The pane being dragged, while gpui's drag under way is a pane drag.
    pub(crate) fn active_pane_drag(&self, cx: &App) -> Option<&DragPayload> {
        self.pane_drag.as_ref().filter(|_| cx.has_active_drag())
    }

    /// The sidebar container or leaf being dragged, while gpui's drag under
    /// way is a sidebar row drag.
    pub(crate) fn active_sidebar_drag(&self, cx: &App) -> Option<&DragPayload> {
        self.sidebar_drag.as_ref().filter(|_| cx.has_active_drag())
    }

    /// Ends a pane, pill or sidebar row drag without a drop: Esc, or for a
    /// pane drag the loss of its pane or of that pane's session. Any other
    /// gpui drag is left alone. Returns whether such a drag was under way.
    fn end_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.active_pane_drag(cx).is_none()
            && self.active_tab_drag(cx).is_none()
            && self.active_sidebar_drag(cx).is_none()
        {
            return false;
        }
        cx.stop_active_drag(window);
        self.clear_drag_state();
        cx.notify();
        true
    }

    /// Forgets every pane, pill and sidebar row drag and the marks they
    /// draw.
    fn clear_drag_state(&mut self) {
        self.pane_drag = None;
        self.pane_drag_session = None;
        self.pane_drop = None;
        self.tab_drag = None;
        self.tab_drop = None;
        self.sidebar_drag = None;
        self.sidebar_drop = None;
    }

    /// Ends a pane drag whose dragged pane left its tab, took another
    /// session, or whose session left the client: the drop targets and the
    /// overlay worked out from them are stale from then on.
    fn end_dead_pane_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let alive = match self.active_pane_drag(cx) {
            Some(DragPayload::Pane { tab_id, pane_id }) => drag::pane_drag_alive(
                self.tabs.tabs(),
                tab_id,
                pane_id,
                self.pane_drag_session.as_deref(),
                |id| self.sidebar.session(id).is_some(),
            ),
            _ => true,
        };
        if !alive {
            self.end_drag(window, cx);
        }
    }

    /// Ends a drag: the sidebar width is saved; a moved divider's ratio goes
    /// to the daemon.
    fn finish_drag(&mut self) {
        match self.drag.take() {
            Some(Drag::Sidebar | Drag::ScSplit | Drag::HistoryList(_)) => self.save_ui(),
            Some(Drag::Divider {
                tab_id,
                split_path,
                ratio: Some(ratio),
                ..
            }) => self.send(ClientMessage::SetPaneRatio {
                tab_id,
                split_path,
                ratio,
            }),
            Some(Drag::Divider { ratio: None, .. }) | None => {}
        }
    }

    /// Folds a tab message in, ending a divider drag whose tree it rebuilt.
    /// Returns whether the message was a tab message.
    fn fold_tabs(
        &mut self,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let drag_tree = self.divider_tree_before(msg);
        if !self.tabs.apply(msg) {
            return false;
        }
        self.drop_stale_divider_drag(drag_tree);
        self.confirm.disarm();
        self.follow_popout_moves(cx);
        self.after_tabs_change(window, cx);
        self.refresh_popout_titles(cx);
        self.prune_popouts(cx);
        true
    }

    /// Folds a tab-list message, then drops a container menu whose tab,
    /// repo or workspace it removed and clears the drop targets it may have
    /// rebuilt; every message also ends a pane drag whose dragged pane or
    /// session is gone. Answers whether the message folded a tab list.
    fn fold_tabs_and_drop_stale_menus(
        &mut self,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let folded = self.fold_tabs(msg, window, cx);
        self.drop_stale_container_menu(window, cx);
        if folded {
            self.pane_drop = None;
            self.drop_stale_pill_drag(window, cx);
        }
        self.end_dead_pane_drag(window, cx);
        folded
    }

    /// The tree an in-flight divider drag points into, kept when `msg` can
    /// rebuild it and compared after the fold.
    fn divider_tree_before(&self, msg: &DaemonMessage) -> Option<(String, GridNode)> {
        if matches!(
            msg,
            DaemonMessage::Tabs { .. }
                | DaemonMessage::TabUpdated { .. }
                | DaemonMessage::TabRemoved { .. }
        ) {
            self.divider_drag_tree()
        } else {
            None
        }
    }

    /// The tree an in-flight divider drag's `split_path` points into, if any.
    fn divider_drag_tree(&self) -> Option<(String, GridNode)> {
        let Some(Drag::Divider { tab_id, .. }) = &self.drag else {
            return None;
        };
        let grid = self.tabs.tab(tab_id)?.grid()?.clone();
        Some((tab_id.clone(), grid))
    }

    /// A tab update that rebuilt the dragged tab's tree renumbers its splits,
    /// so the drag's held path would land the ratio on a split the press did
    /// not grab. The drag ends instead, and its release sends nothing.
    fn drop_stale_divider_drag(&mut self, before: Option<(String, GridNode)>) {
        let Some((tab_id, before)) = before else {
            return;
        };
        let still_dragging = matches!(
            &self.drag,
            Some(Drag::Divider { tab_id: dragged, .. }) if *dragged == tab_id
        );
        if !still_dragging {
            return;
        }
        let after = self.tabs.tab(&tab_id).and_then(|tab| tab.grid().cloned());
        if after.as_ref() != Some(&before) {
            self.drag = None;
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
        self.pair_on_not_approved();
        self.command(NetCommand::Restart);
        self.handshake = None;
        self.close_flyout();
    }

    fn click_stop(&mut self) {
        if !self.capabilities().manages_daemon() {
            tracing::warn!("ignoring Stop: the connected daemon is a remote host's");
            return;
        }
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
            NetEvent::Handshake(_)
            | NetEvent::ShutdownSent
            | NetEvent::ShutdownFailed
            | NetEvent::Paired
            | NetEvent::PairingLost
            | NetEvent::TokenSaveFailed
            | NetEvent::PairRequestSent => false,
        };
        let pairing_input = device_pairing_view::PairingInput::of(&event);
        match event {
            NetEvent::State(conn) => {
                // Everything the old target's daemon sent goes before the
                // new target's state does, so the notices the rest of this
                // arm raises survive it.
                if conn.target() != self.conn.target() {
                    self.reset_for_target(conn.target(), window, cx);
                }
                if !conn.is_open() {
                    self.set_keep_awake(None);
                    self.forget_agent_commands();
                    self.forget_agents();
                    self.agent_tabs.forget_applies();
                    self.launch_last.clear();
                }
                // The handshake is the old target's daemon's.
                if conn.target() != self.conn.target() {
                    self.handshake = None;
                }
                // A fetch streams over the connection it was asked on, and
                // the remote access status is that connection's daemon's.
                if !conn.is_open() || conn.target() != self.conn.target() {
                    self.end_fetches_on_disconnect(window, cx);
                    self.set_remote_status(None, cx);
                }
                self.conn = conn;
                self.follow_settings_target();
            }
            NetEvent::Handshake(info) => self.handshake = Some(info),
            NetEvent::Message(msg) => {
                let reseed = moves_source_control_inputs(&msg);
                self.on_message(*msg, window, cx);
                let dropped_picker = self.drop_stale_sc_picker();
                let dropped_changes = reseed && self.seed_source_control(window, cx);
                if dropped_picker || dropped_changes {
                    self.after_notice_closed(window, cx);
                }
            }
            NetEvent::ShutdownSent => self.on_shutdown_sent(),
            NetEvent::ShutdownFailed => self.on_shutdown_failed(window, cx),
            NetEvent::Paired
            | NetEvent::PairingLost
            | NetEvent::TokenSaveFailed
            | NetEvent::PairRequestSent => {}
        }
        // The overlay covers the footer; a flyout left open under it would
        // reappear (possibly armed) when the overlay goes.
        if self.conn.overlay().is_some() {
            self.close_flyout();
            self.close_spawn_dialog(window, cx);
            self.close_shell_dialog(window, cx);
            self.close_fetch_dialog(window, cx);
            self.close_workspace_creator(window, cx);
            self.close_preset_wizard(window, cx);
            self.vscode_suggestions.clear();
            self.cwd_workspace_scans.clear();
            self.close_appearance_editor(window, cx);
            self.close_discard_confirm(window, cx);
            self.close_stash_drop_confirm(window, cx);
            self.reset_session_ui(window, cx);
            self.reset_notices(window, cx);
            self.reset_undo(cx);
            self.close_recover_on_overlay(window, cx);
            self.reset_pairing(window, cx);
            self.reset_devices(window, cx);
        }
        if let Some(input) = pairing_input {
            self.on_device_pairing_input(input, window, cx);
        }
        if exit_relevant {
            self.after_exit_event(window, cx);
        }
        cx.notify();
    }

    /// Folds a message in to the file fetches, the diff and viewer tabs and
    /// the source-control panel's parts; returns whether nothing else is
    /// owed it.
    fn on_panel_message(
        &mut self,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.on_fetch_message(msg, window, cx)
            || self.on_diff_message(msg, window, cx)
            || self.on_viewer_message(msg, window, cx)
            || self.on_sc_message(msg, window, cx)
            || self.on_stash_message(msg, window, cx)
            || self.apply_history(msg, cx)
    }

    /// Folds the message into the sidebar, and forgets the remembered
    /// session when the message removes it. A message that changed the Needs
    /// You list re-arms the panel's repaint, so its cycle follows the
    /// youngest wait rather than the list as it stood when the timer was
    /// armed.
    fn fold_sidebar(&mut self, msg: &DaemonMessage, cx: &mut Context<Self>) {
        let before = moves_needs_you(msg).then(|| self.needs_you_list());
        let focused = self.shown_session();
        self.sidebar.set_focused_session(focused.as_deref());
        self.sidebar.apply(msg);
        // A session focused in a pop-out is shown too: a turn ending there
        // is seen.
        if let Some(session) = self.popout_focused_session() {
            self.sidebar.mark_seen(&session);
        }
        self.prune_container_row_focus();
        self.forget_remembered_session(msg);
        if before.is_some_and(|before| self.needs_you_list() != before) {
            self.arm_needs_you_repaint(cx);
        }
        if matches!(
            msg,
            DaemonMessage::Sessions { .. }
                | DaemonMessage::SessionUpdated { .. }
                | DaemonMessage::SessionRemoved { .. }
        ) {
            self.refresh_popout_titles(cx);
        }
    }

    /// Remembers the session the active terminal tab focuses, so the
    /// source-control panel keeps reading it while a diff tab is active.
    fn remember_focused_session(&mut self) {
        if let Some(id) = self.focused_session() {
            self.last_focused_session = Some(id);
        }
    }

    /// Drops the remembered session once the daemon removes it, or its
    /// whole list no longer names it.
    fn forget_remembered_session(&mut self, msg: &DaemonMessage) {
        let Some(remembered) = self.last_focused_session.as_deref() else {
            return;
        };
        let gone = match msg {
            DaemonMessage::SessionRemoved { session_id } => session_id.as_str() == remembered,
            DaemonMessage::Sessions { sessions } => !sessions
                .iter()
                .any(|session| session.id.as_str() == remembered),
            _ => false,
        };
        if gone {
            self.last_focused_session = None;
        }
    }

    fn on_message(&mut self, msg: DaemonMessage, window: &mut Window, cx: &mut Context<Self>) {
        // The sidebar takes the message before the panes do, so what a
        // snapshot changed is read from the old one.
        let (moved, exit_discard, finished) = self.before_fold(&msg);
        self.fold_sidebar(&msg, cx);
        self.on_import_message(&msg, window, cx);
        if let Some(discard) = exit_discard {
            self.send(discard);
        }
        // Windows deactivates a window it minimizes, so a minimized window
        // counts as not focused.
        self.notify_on(&msg, finished, window.is_window_active(), cx);
        if self.on_panel_message(&msg, window, cx) {
            return;
        }
        self.drop_stale_session_ui(window, cx);
        self.on_spawn_dialog_message(&msg, window, cx);
        self.on_launch_last_message(&msg, cx);
        self.on_dialog_message(&msg, window, cx);
        if self.on_vscode_message(&msg, window, cx) {
            return;
        }
        if let DaemonMessage::Tabs { tabs } = &msg {
            self.on_tab_list(tabs, window, cx);
        }
        if self.fold_tabs_and_drop_stale_menus(&msg, window, cx) {
            return;
        }
        if self.on_resume_report(&msg, cx) || self.on_agent_exited(&msg, cx) {
            return;
        }
        match msg {
            DaemonMessage::Welcome { .. } => self.on_welcome(window, cx),
            DaemonMessage::Error {
                message,
                request_id,
            } => self.on_daemon_failure(message, request_id.as_deref(), window, cx),
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
            } => self.ask_checkout(repo_id, branch, dirty_count, request_id, window, cx),
            DaemonMessage::LayoutInitRequired {
                has_legacy,
                active_session_count,
                clonable,
            } => self.open_layout_chooser(has_legacy, active_session_count, clonable, window, cx),
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
            DaemonMessage::Repos { repos } => self.on_repos(&repos, cx),
            DaemonMessage::Workspaces { workspaces } => self.on_workspaces(&workspaces, window, cx),
            msg @ DaemonMessage::KeepAwakeStatus { .. } => self.on_keep_awake_status(&msg),
            msg @ DaemonMessage::RemoteStatus { .. } => self.on_remote_status(msg, cx),
            msg @ DaemonMessage::AgentCommands { .. } => self.on_agent_commands(msg, cx),
            other => self.fold_agents(other, cx),
        }
    }

    /// A fresh repo list from the daemon: the source-control panel drops the
    /// repos that are gone, and the sidebar's containers re-render.
    fn on_repos(&mut self, repos: &[RepoEntry], cx: &mut Context<Self>) {
        self.prune_source_control(repos);
        self.on_containers_changed(cx);
    }

    /// Shows the daemon's start-up resume report as one toast; a later report
    /// replaces it in place. Returns whether `msg` was that report.
    fn on_resume_report(&mut self, msg: &DaemonMessage, cx: &mut Context<Self>) -> bool {
        let DaemonMessage::ResumeReport {
            resumed,
            left_in_recover,
        } = msg
        else {
            return false;
        };
        self.push_toast_spec(notices::resume_report_toast(*resumed, *left_in_recover), cx);
        true
    }

    /// Shows a session's agent exit as one toast, keyed by the session so a
    /// second exit updates it in place; an exit for a session this client
    /// does not list raises nothing. Returns whether `msg` was that notice.
    fn on_agent_exited(&mut self, msg: &DaemonMessage, cx: &mut Context<Self>) -> bool {
        let DaemonMessage::AgentExited { session_id } = msg else {
            return false;
        };
        let Some(session) = self.sidebar.session(session_id) else {
            return true;
        };
        let label = session
            .agent
            .as_ref()
            .and_then(|id| crate::agent_options::descriptor_for(&self.agents, id))
            .map(|descriptor| descriptor.label.clone());
        self.push_toast_spec(
            notices::agent_exited_toast(session_id, label.as_deref()),
            cx,
        );
        true
    }

    /// The Close session button of an agent-exit toast: stops and discards
    /// the session, leaving its worktrees on disk; a session this client no
    /// longer lists sends nothing.
    fn close_session_after_agent_exit(&mut self, session_id: &str) {
        let messages = self
            .sidebar
            .session(session_id)
            .map(branch_fate::close_after_agent_exit)
            .unwrap_or_default();
        for msg in messages {
            self.send(msg);
        }
    }

    /// A daemon `Error`: the launch-last wait it answers ends here, so the
    /// daemon's report is the only one and its timeout toast never follows.
    fn on_daemon_failure(
        &mut self,
        message: String,
        request_id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = request_id {
            let _ = self.launch_last.take_request(id);
        }
        self.on_refusal(message, request_id, window, cx);
    }

    /// A daemon refusal: an unrouted one (`None`) while the workspace creator
    /// waits fails a pending create or parse inline there, raising no toast,
    /// and a refused worktrees root ends that tab's save, whose message
    /// still toasts. A routed refusal (`Some`) answers its own request: one
    /// of the launch-command refusals of an apply shows under its row with
    /// no toast; refused Settings values end their apply and toast; any
    /// other takes the generic path.
    fn on_refusal(
        &mut self,
        message: String,
        request_id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if request_id.is_none() {
            self.on_worktrees_root_error(&message, cx);
        }
        let inline = match request_id {
            None => self.on_creator_error(&message, cx) || self.on_preset_list_error(&message, cx),
            Some(id) => {
                self.on_agent_setting_refused(id, &message, cx);
                self.on_agent_command_error(id, &message, cx)
            }
        };
        self.repo_remove.disarm();
        if !inline {
            self.on_daemon_error(message, request_id, window, cx);
        }
    }

    /// The registry's workspaces after a broadcast: the panels follow, and
    /// the workspace creator closes on the name it was waiting for.
    fn on_workspaces(
        &mut self,
        workspaces: &[WorkspaceEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.on_containers_changed(cx);
        self.settle_workspace_creator(workspaces, window, cx);
    }

    /// The daemon asks whether to discard uncommitted changes before a
    /// checkout; the confirm prompt takes it from here.
    fn ask_checkout(
        &mut self,
        repo_id: String,
        branch: String,
        dirty_count: u32,
        request_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ask = notices::CheckoutAsk::new(repo_id, branch, dirty_count, request_id);
        self.on_checkout_confirm(ask, window, cx);
    }

    /// Hands the message to the cleanup-failed dialog, the worktrees
    /// manager, the Recover dialog, the preset wizard and its launch jobs,
    /// and a spawn config to the Shift-duplicate waiting for it.
    fn on_dialog_message(
        &mut self,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.on_cleanup_failed_message(msg, window, cx);
        self.on_worktrees_message(msg, cx);
        self.on_recover_message(msg, window, cx);
        self.on_pairing_message(msg, window, cx);
        self.on_devices_message(msg, window, cx);
        self.on_preset_message(msg, cx);
        self.on_preset_launch_message(msg, window, cx);
        if let DaemonMessage::SpawnConfigReply { session_id, config } = msg {
            self.on_spawn_config(session_id, config.clone(), window, cx);
        }
    }

    /// A new connection: fresh panes, nothing in flight, every dialog, menu
    /// and notice of the old one closed, and the session history asked for
    /// afresh.
    fn on_welcome(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reset_for_new_daemon(window, cx);
        self.reset_recover(window, cx);
        self.reset_devices(window, cx);
        self.ask_pending_devices();
        self.ask_devices();
        self.awaiting_initial_sessions = true;
    }

    /// The connection moved to another daemon (this PC's or a tailnet
    /// host's): nothing the old daemon sent stays on screen or takes a
    /// click, no drag goes on, and the exit dialog opened for it closes.
    /// Sends nothing to either daemon and saves nothing; the new daemon's
    /// lists fill the view again, and back on this PC its saved tab shows.
    fn reset_for_target(
        &mut self,
        target: &target::Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::info!("connection target changed: clearing the old daemon's state");
        self.pane_ui.close = None;
        self.pane_ui.move_panes = None;
        self.pane_ui.menu = None;
        self.drag = None;
        self.reset_for_new_daemon(window, cx);
        self.sidebar.clear_daemon_state();
        let restored_active = if target.is_local() {
            self.sidebar.ui_state().active_tab_id.clone()
        } else {
            None
        };
        self.tabs.clear_for_switch(restored_active);
        self.panes.clear();
        self.sessions_loaded = false;
        self.seen_sessions.clear();
        self.last_focused_session = None;
        self.last_active_tab = None;
        self.retries = RetryGate::default();
        self.renaming = None;
        self.wanted_session = None;
        self.sc = ScModel::default();
        self.sc_picker_open = false;
        self.changes.writes = sc_writes::ScWrites::default();
        self.changes.file_menu = None;
        self.stash.model = stashes::StashModel::default();
        self.history = history::HistoryModel::default();
        self.diff_tabs.clear();
        self.diff_opens = diff_tab::DiffOpens::default();
        self.viewer_tabs.clear();
        self.viewer_opens = viewer_tab::ViewerOpens::default();
        self.session_history = recover::SessionHistory::default();
        self.recover_requests.clear();
        self.recover_batches.clear();
        self.close_recover_on_overlay(window, cx);
        self.reset_pairing(window, cx);
        self.reset_devices(window, cx);
        self.close_device_pairing(window, cx);
        self.branch_cache = BranchCache::default();
        self.end_drag(window, cx);
        self.clear_drag_state();
        if self.exit.is_some() {
            self.close_exit_dialog(window, cx);
        }
        cx.notify();
    }

    /// The part of a new connection's reset that sends nothing: fresh
    /// panes, nothing in flight, and every dialog, menu and notice of the
    /// old connection closed.
    fn reset_for_new_daemon(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_keep_awake(None);
        self.forget_agent_commands();
        self.forget_agents();
        self.agent_tabs = settings_view::AgentTabs::default();
        self.set_remote_status(None, cx);
        self.reset_panes(cx);
        self.pending_appearance.clear();
        self.container_sends.clear();
        self.status.clear();
        self.duplicates.clear();
        self.prefills.clear();
        self.launch_last.clear();
        self.close_spawn_dialog(window, cx);
        self.close_shell_dialog(window, cx);
        self.close_fetch_dialog(window, cx);
        self.close_host_path_prompt(window, cx);
        self.close_workspace_creator(window, cx);
        self.close_preset_wizard(window, cx);
        self.reset_preset_launches(cx);
        self.launched_highlight.clear();
        self.vscode_suggestions.clear();
        self.cwd_workspace_scans.clear();
        self.close_appearance_editor(window, cx);
        self.close_discard_confirm(window, cx);
        self.close_stash_drop_confirm(window, cx);
        self.reset_session_ui(window, cx);
        self.reset_notices(window, cx);
        self.reset_undo(cx);
        self.tabs.clear_pending_restore();
        self.tabs.clear_armed_creates();
        self.tabs.clear_layout_requests();
        self.pending_arrangement = None;
        self.close_layout_chooser(window, cx);
        self.reset_import(window, cx);
        self.reset_cleanup_failed(window, cx);
        self.reset_worktrees(window, cx);
    }

    /// Whether `session`'s snapshot moves its appearance, read against the
    /// copy the sidebar still holds.
    fn appearance_moved(&self, session: &SessionSnapshot) -> bool {
        self.sidebar
            .session(&session.id)
            .map(|before| before.appearance.clone())
            != Some(session.appearance.clone())
    }

    /// What a message changes against the copy the sidebar still holds:
    /// whether a session snapshot moved its appearance, the discard its exit
    /// calls for, and whether it ended the session's turn. A relist or a
    /// reconnect never ends one.
    fn before_fold(&mut self, msg: &DaemonMessage) -> (Option<bool>, Option<ClientMessage>, bool) {
        match msg {
            DaemonMessage::Welcome { .. } => self.seen_sessions.clear(),
            DaemonMessage::SessionRemoved { session_id } => {
                self.seen_sessions.remove(session_id);
            }
            DaemonMessage::Sessions { sessions } => {
                self.seen_sessions = sessions.iter().map(|s| s.id.clone()).collect();
            }
            DaemonMessage::SessionUpdated { session, .. } => {
                let discard = self.exit_discard(session);
                let finished = crate::notify::finished(self.sidebar.session(&session.id), session);
                self.seen_sessions.insert(session.id.clone());
                return (Some(self.appearance_moved(session)), discard, finished);
            }
            _ => {}
        }
        (None, None, false)
    }

    /// The discard of a session with no worktree of its own that `session`
    /// shows just exited by itself, read against the status the sidebar
    /// holds from this connection: there is nothing left to keep.
    fn exit_discard(&self, session: &SessionSnapshot) -> Option<ClientMessage> {
        let prev = self
            .sidebar
            .session(&session.id)
            .filter(|_| self.seen_sessions.contains(&session.id))
            .map(|before| before.status);
        (self_exited(prev, session) && !session.has_per_session_worktree).then(|| {
            ClientMessage::DiscardSession {
                session_id: session.id.clone(),
                cleanup: Vec::new(),
            }
        })
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
            view.update(cx, |pane, _| pane.update_session(session, &self.agents));
        }
        let own_change = request_id
            .is_some_and(|request_id| self.pending_appearance.answered(&session.id, request_id));
        self.attach_waiting_panes(cx);
        if moved {
            self.apply_session_pane_fonts(&session.id, own_change, cx);
        }
        self.place_duplicate(request_id, session, window, cx);
        self.place_spawn(request_id, session, window, cx);
        // A shell spawned after the session list, or moved to a new folder by
        // its own cwd report, reveals a folder only its snapshot names.
        self.scan_unseen_cwd_paths();
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
            view.update(cx, |pane, _| pane.refresh_sessions(sessions, &self.agents));
        }
        self.attach_waiting_panes(cx);
        self.apply_pane_fonts(false, cx);
        self.try_wanted_session(window, cx);
        self.scan_unseen_cwd_paths();
    }

    /// A key for what is drawn on top: Esc ending a drag, then the
    /// Connections picker (above every other layer, the first-connect
    /// overlay included), then the cleanup-failed dialog. Returns whether
    /// the key goes no further down the chain; it stops propagating only
    /// when one of them took it, so typing in the picker's fields still
    /// reaches them.
    fn on_top_layer_key(
        &mut self,
        ks: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let taken = if self.take_drag_escape() {
            Some(true)
        } else if let Some(taken) = self.on_connection_picker_key(ks, window, cx) {
            Some(taken)
        } else if let Some(taken) = self.on_device_pairing_key(ks, window, cx) {
            Some(taken)
        } else {
            self.on_cleanup_failed_key(ks, window, cx).then_some(true)
        };
        if taken == Some(true) {
            cx.stop_propagation();
        }
        taken.is_some()
    }

    /// Keys the root takes before the panes see them. An Esc that ended a
    /// pane or pill drag ([`Self::intercept_drag_escape`]) goes no further.
    /// The worktree
    /// cleanup-failed dialog, else the exit dialog, else
    /// the first-connect layout chooser, else the run confirm, else a modal
    /// notice, else the delete-worktree confirm, else the spawn
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
        if self.on_top_layer_key(ks, window, cx) {
            return;
        }
        if self.on_exit_key(ks, window, cx) {
            cx.stop_propagation();
            return;
        }
        if self.on_layout_chooser_key(ks, window, cx) || self.on_import_key(ks, window, cx) {
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
        if self.on_discard_confirm_key(ks, window, cx) {
            cx.stop_propagation();
            return;
        }
        if self.on_stash_drop_key(ks, window, cx) {
            cx.stop_propagation();
            return;
        }
        if self.on_pane_ui_key(ks, window, cx) {
            cx.stop_propagation();
            return;
        }
        if self.pane_ui.move_panes.is_some() {
            if self.on_move_panes_key(ks, window, cx) {
                cx.stop_propagation();
            }
            return;
        }
        if self.creator_dialog_open() {
            if self.on_creator_dialogs_key(ks, window, cx) {
                cx.stop_propagation();
            }
            return;
        }
        if self.repo_remove.dialog().is_some() {
            self.on_repo_remove_key(ks, window, cx);
            cx.stop_propagation();
            return;
        }
        if self.delete_dialog.is_some() {
            self.on_delete_dialog_key(ks, window, cx);
            cx.stop_propagation();
            return;
        }
        if let Some(taken) = self.on_add_repo_dialogs_key(ks, window, cx) {
            if taken {
                cx.stop_propagation();
            }
            return;
        }
        if let Some(taken) = self.on_path_dialog_key(ks, window, cx) {
            if taken {
                cx.stop_propagation();
            }
            return;
        }
        if self.on_device_dialogs_key(ks, window, cx) || self.on_recover_key(ks, window, cx) {
            cx.stop_propagation();
            return;
        }
        if self.worktrees_manager.is_some() {
            self.on_worktrees_manager_key(ks, window, cx);
            cx.stop_propagation();
            return;
        }
        if self.appearance_editor.is_some() {
            let typing = self.settings_typing(window, cx);
            if self.on_settings_key(ks, window, cx)
                || self.on_editor_key(ks, window, cx)
                || (!typing && self.on_appearance_key(ks, window, cx))
            {
                cx.stop_propagation();
            }
            return;
        }
        if self.on_container_row_key(ks, window, cx) {
            cx.stop_propagation();
            return;
        }
        if ks.key == "escape" {
            let armed = self.disarm_escape_arms();
            if armed {
                cx.notify();
            }
        }
        if self.on_shortcut(ks, window, cx) {
            cx.stop_propagation();
            cx.notify();
        }
    }

    /// Esc's one-click arms: a header Stop, a tab close, a container
    /// removal's armed row and the Detached banner's Stop all. Returns
    /// whether one was dropped.
    fn disarm_escape_arms(&mut self) -> bool {
        let tab_close = self.tabs.close_confirm.disarm();
        let remove_armed = self.repo_remove.disarm();
        let detached_stop_all = self.detached_stop_all.disarm();
        self.confirm.disarm() || tab_close || remove_armed || detached_stop_all
    }

    /// The keys a focused container row takes: Up and Down move the row
    /// focus, Enter and Space fold the row, and Escape hands the keyboard
    /// back to the terminal. `false` when the sidebar does not hold the
    /// keyboard or no row is focused, so the keys reach the terminal.
    fn on_container_row_key(
        &mut self,
        ks: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.sidebar_focus.is_focused(window) || self.row_focus.is_none() {
            return false;
        }
        self.prune_container_row_focus();
        let Some(key) = self.row_focus.clone() else {
            return false;
        };
        match ks.key.as_str() {
            "up" | "down" => {
                let keys: Vec<String> = self
                    .sidebar_containers()
                    .into_iter()
                    .map(|container| container.key)
                    .collect();
                let delta = if ks.key == "down" { 1 } else { -1 };
                if let Some(next) = crate::sidebar::move_row_focus(&keys, Some(&key), delta) {
                    self.row_focus = Some(next);
                    cx.notify();
                }
                true
            }
            "enter" | "space" => {
                self.toggle_container(&key);
                cx.notify();
                true
            }
            "escape" => {
                self.row_focus = None;
                self.focus_active_pane(window, cx);
                true
            }
            _ => false,
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
        } else if self.container_menu.is_some() && ks.key == "escape" {
            self.close_container_menu(window, cx);
        } else if self.sidebar_more_open() && ks.key == "escape" {
            self.close_more_menu(window, cx);
        } else if self.shell_menu.is_some() && ks.key == "escape" {
            self.close_shell_menu(window, cx);
        } else if self.sc_picker_open && ks.key == "escape" {
            self.close_sc_picker(window, cx);
        } else if self.changes.file_menu.is_some() && ks.key == "escape" {
            self.close_sc_file_menu(window, cx);
        } else if self.flyout_open && ks.key == "escape" {
            self.close_flyout();
        } else if let Some(key) = font_key(ks) {
            self.close_shell_menu(window, cx);
            self.on_font_key(key, cx);
        } else if ctrl_only && ks.key == "b" && self.outside_terminal(window, cx) {
            self.toggle_sidebar(window, cx);
        } else if ctrl_only && ks.key == "," {
            self.open_settings(window, cx);
        } else if self.is_spawn_shortcut(ks, window, cx) {
            self.open_spawn_dialog(crate::spawn_view::SpawnEntry::Toolbar, window, cx);
        } else {
            return self.on_tab_shortcut(ks, window, cx);
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
            self.apply_pane_fonts(true, cx);
            self.schedule_ui_save(cx);
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

    /// A press anywhere that did not stop at a tab's close button, a session
    /// action's confirm or a container menu's Remove row drops what they
    /// armed.
    fn on_any_mouse_down(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let tab_close = self.tabs.close_confirm.disarm();
        let remove_armed = self.repo_remove.disarm();
        if self.confirm.disarm() || tab_close || remove_armed {
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
            |session| crate::sidebar::display_label(session, &self.agents),
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

/// Whether `msg` can change what the source-control panel or the rail's badge
/// reads: the connection, the repo registry or a session. Tab changes seed on
/// their own path.
fn moves_source_control_inputs(msg: &DaemonMessage) -> bool {
    matches!(
        msg,
        DaemonMessage::Welcome { .. }
            | DaemonMessage::Repos { .. }
            | DaemonMessage::Sessions { .. }
            | DaemonMessage::SessionUpdated { .. }
            | DaemonMessage::SessionRemoved { .. }
    )
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
        // Tab switches and renames made here change the title without a
        // daemon message.
        self.refresh_window_title(window, cx);
        // A daemon echo or a re-read can take the focused Settings control
        // out of the ring; the keyboard moves on from its place.
        self.keep_settings_keyboard(window, cx);
        // A pane, pill or sidebar row drag gpui ended on a release no
        // listener took (a modal over the window swallows it) leaves nothing
        // behind to mistake a later drag for it: no payload, no overlay, no
        // mark.
        if !cx.has_active_drag() {
            self.clear_drag_state();
        }
        let footer = self.conn.footer(self.sidebar.sessions().len());
        let flyout = self
            .flyout_open
            .then(|| [flyout_backdrop(cx).into_any_element(), self.flyout(cx)]);
        let restart_label = self.conn.restart_label();
        let overlay = self
            .conn
            .overlay_text()
            .map(|text| connecting_overlay(text, restart_label, footer.dot, cx));
        // The quit's walk draws its confirm above the exit dialog.
        let delete_dialog = self.delete_dialog_layer(cx);
        let (delete_under, delete_over) = if self.exit_walk_confirm_open() {
            (None, delete_dialog)
        } else {
            (delete_dialog, None)
        };
        let repo_remove_layer = self.repo_remove_layer(cx);
        let workspace_creator_layer = self.workspace_creator_layer(cx);
        let vscode_prompt_layer = self.vscode_prompt_layer(cx);

        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .font_family(fonts::ui_family(cx))
            .track_focus(&self.root_focus)
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
            .children(self.empty_pane_menu_layer(cx).into_iter().flatten())
            .children(self.container_menu_layer(cx).into_iter().flatten())
            .children(self.more_menu_layer())
            .children(self.sc_picker_layer())
            .children(self.sc_file_menu_layer(cx).into_iter().flatten())
            .children(flyout.into_iter().flatten())
            // Above the flyout's backdrop, so the chip's tooltip is reachable
            // while the flyout is open: its own copy button has no other
            // confirmation to show.
            .children(self.chip_layer())
            .children(self.spawn_dialog_layers(cx))
            .children(self.shell_dialog_layer(cx))
            .children(self.fetch_dialog_layer(cx))
            .children(self.host_path_prompt_layer(cx))
            .children(self.appearance_editor_layer(window, cx))
            .children(self.settings_layer(window, cx))
            .children(self.worktrees_manager_layer(window, cx))
            .children(self.recover_layer(window, cx))
            .children(self.pairing_layer(cx))
            .children(self.device_block_layer(cx))
            .children(self.pane_close_layer(cx))
            .children(self.move_panes_layer(cx))
            .children(delete_under)
            .children(repo_remove_layer)
            .children(workspace_creator_layer)
            .children(self.preset_wizard_layer(cx))
            .children(vscode_prompt_layer)
            .children(self.discard_confirm_layer(cx))
            .children(self.stash_drop_layer(cx))
            .children(self.notice_layers(cx))
            .children(self.toast_layer(cx))
            .children(self.undo_layer(window, cx))
            .children(self.run_confirm_layer(cx))
            .children(self.layout_chooser_layer(cx))
            .children(self.import_layer(cx))
            .children(self.exit_layer(cx))
            .children(delete_over)
            .children(self.cleanup_failed_layer(window, cx))
            .children(overlay)
            .children(self.device_pairing_layer(cx))
            .children(self.connection_picker_layer(cx))
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
    /// The bottom bar: the Connections button, the daemon pill, the status
    /// counts, then the attached session's status, right-aligned and the
    /// first to truncate.
    fn footer_bar(&self, footer: &Footer, cx: &mut Context<Self>) -> Div {
        let connections = div()
            .id("footer-connections")
            .debug_selector(|| "footer-connections".to_owned())
            .flex()
            .flex_none()
            .items_center()
            .h(px(FOOTER_PILL_HEIGHT))
            .px(px(9.0))
            .rounded(px(FOOTER_PILL_HEIGHT / 2.0))
            .border_1()
            .border_color(gpui::rgb(LINE_STRONG))
            .text_size(px(FOOTER_PILL_TEXT_SIZE))
            .cursor_pointer()
            .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
            .child("Connections")
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.open_connection_picker(window, cx);
            }));
        let text = footer.pill();
        let counts = status_counts(self.sidebar.sessions(), |id| self.sidebar.is_unseen(id));
        let pill = div()
            .id("daemon-pill")
            .debug_selector(|| "daemon-pill".to_owned())
            .flex()
            .flex_none()
            .items_center()
            .gap(px(7.0))
            .h(px(FOOTER_PILL_HEIGHT))
            .px(px(9.0))
            .rounded(px(FOOTER_PILL_HEIGHT / 2.0))
            .border_1()
            .border_color(gpui::rgb(LINE_STRONG))
            .text_size(px(FOOTER_PILL_TEXT_SIZE))
            .cursor_pointer()
            .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
            .when(self.flyout_open, |pill| pill.bg(gpui::rgb(HOVER_BG)))
            .child(status_dot(footer.dot, "footer-dot"))
            .child(tagged_text("daemon-pill", text))
            .tooltip(tooltip(footer.tooltip.clone()))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                if this.device_pairing_offered() {
                    this.open_device_pairing(window, cx);
                } else {
                    this.toggle_flyout();
                }
                cx.notify();
            }));
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(FOOTER_GAP))
            .px(px(12.0))
            .h(px(FOOTER_HEIGHT))
            .bg(gpui::rgb(SUNKEN))
            .border_t_1()
            .border_color(gpui::rgb(BORDER))
            .text_size(px(FOOTER_TEXT_SIZE))
            .text_color(gpui::rgb(TEXT_2))
            .child(connections)
            .child(pill)
            .children(counts.spans().into_iter().map(footer_count))
            .child(
                ellipsis::truncating("footer-status", StyledText::new(self.footer_status()))
                    .debug_selector(|| "footer-status".to_owned())
                    .flex_1()
                    .text_right(),
            )
    }

    /// The troubleshooting flyout above the pill: details, files, control.
    fn flyout(&self, cx: &mut Context<Self>) -> AnyElement {
        let manages_daemon = self.capabilities().manages_daemon();
        let rows = flyout_rows(
            &self.conn,
            self.handshake.as_ref(),
            self.sidebar.sessions().len(),
        );
        let labels: Vec<&'static str> = rows.iter().map(|(label, _)| *label).collect();
        let details = div()
            .debug_selector(move || format!("flyout-details={}", labels.join("|")))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(
                rows.into_iter()
                    .map(|(label, value)| detail_row(label, value)),
            )
            .when(manages_daemon, |details| {
                details.child(self.handshake_row(cx))
            });
        popover_frame("daemon-flyout")
            .absolute()
            .left(px(PADDING))
            .bottom(px(FOOTER_HEIGHT + 4.0))
            .w(px(320.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(10.0))
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Daemon"))
            .child(details)
            .child(self.files_section(manages_daemon))
            .child(self.control_section(manages_daemon, cx))
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
    /// the error when the config dir could not be resolved. `daemon.log`
    /// shows only with `daemon_log`, as a remote host's log is not here.
    fn files_section(&self, daemon_log: bool) -> Div {
        let section = flyout_section("Logs & files");
        match &self.paths {
            Ok(paths) => section
                .when(daemon_log, |section| {
                    section.child(open_button(
                        "open-daemon-log",
                        "Open daemon.log",
                        &paths.daemon_log,
                    ))
                })
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
                .when(daemon_log, |section| {
                    section.child(action_button(
                        "open-daemon-log",
                        "Open daemon.log",
                        TEXT,
                        false,
                    ))
                })
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

    /// "Control": restart (Reconnect now on a remote host), and the
    /// two-click stop only with `stop`.
    fn control_section(&self, stop: bool, cx: &mut Context<Self>) -> Div {
        let armed = self.stop.armed;
        flyout_section("Control")
            .child(
                action_button("restart-daemon", self.conn.restart_label(), TEXT, true).on_click(
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.restart();
                        cx.notify();
                    }),
                ),
            )
            .when(stop, |section| {
                section.child(
                    action_button("stop-daemon", self.stop.label(), DANGER, true)
                        .when(armed, |button| button.bg(gpui::rgb(DANGER_BG)))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.click_stop();
                            cx.notify();
                        })),
                )
            })
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

/// The full-window card shown until the first connect, with a restart link
/// and a Connections link, since the card covers the footer's button.
fn connecting_overlay(
    text: String,
    restart_label: &'static str,
    dot: DotKind,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let restart = div()
        .id("overlay-restart")
        .text_size(px(UI_TEXT_SIZE))
        .text_color(gpui::rgb(MUTED))
        .cursor_pointer()
        .hover(|style| style.text_color(gpui::rgb(TEXT)))
        .child(tagged_text("overlay-restart", restart_label))
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            this.restart();
            cx.notify();
        }));
    let connections = div()
        .id("overlay-connections")
        .debug_selector(|| "overlay-connections".to_owned())
        .text_size(px(UI_TEXT_SIZE))
        .text_color(gpui::rgb(MUTED))
        .cursor_pointer()
        .hover(|style| style.text_color(gpui::rgb(TEXT)))
        .child("Connections")
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.open_connection_picker(window, cx);
        }));
    let links = div()
        .flex()
        .items_center()
        .gap(px(16.0))
        .child(restart)
        .child(connections);
    let card = div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(16.0))
        .px(px(40.0))
        .py(px(32.0))
        .min_w(px(280.0))
        .bg(gpui::rgb(SURFACE))
        .border_1()
        .border_color(gpui::rgb(LINE_STRONG))
        .rounded(px(MODAL_RADIUS))
        .child(status_dot(dot, "overlay-dot"))
        .child(tagged_text("connecting-overlay", text).text_color(gpui::rgb(TEXT)))
        .child(links);
    div()
        .id("connecting-overlay")
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::rgba(SCRIM))
        .occlude()
        .child(card)
}

/// One of the footer's status counts: its glyph, then its text, tagged
/// `footer-count-<kind>`.
fn footer_count(span: CountSpan) -> Div {
    let name = span.kind.name();
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.0))
        .debug_selector(move || format!("footer-count-{name}"))
        .child(glyph_view(
            span.kind.glyph(),
            GlyphSize::Footer,
            &format!("footer-glyph-{name}"),
        ))
        .child(span.text)
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
        DotKind::Ok => STATUS_OK,
        DotKind::Pending => WARNING,
        DotKind::Idle | DotKind::Stopped => STATUS_IDLE,
        DotKind::Err => STATUS_ERR,
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

/// A clickable text button; a disabled one is dimmed and takes no clicks.
fn action_button(
    id: &'static str,
    label: &'static str,
    color: u32,
    enabled: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
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
        .child(tagged_text(id, label))
}

/// `text` in a box a spec finds by `<selector>=<text>`, so it reads what the
/// element shows rather than what the model would say.
fn tagged_text(selector: &'static str, text: impl Into<SharedString>) -> Div {
    let text = text.into();
    let tag = text.clone();
    div()
        .debug_selector(move || format!("{selector}={tag}"))
        .child(text)
}

/// A button that opens `path` with the system's default application.
fn open_button(id: &'static str, label: &'static str, path: &Path) -> Stateful<Div> {
    let path: PathBuf = path.to_path_buf();
    action_button(id, label, TEXT, true).on_click(move |_, _, cx| cx.open_with_system(&path))
}

/// A plain text tooltip.
struct Tip(SharedString);

impl Render for Tip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(6.0))
            .py(px(2.0))
            .font_family(fonts::ui_family(cx))
            .bg(gpui::rgb(CHIP))
            .border_1()
            .border_color(gpui::rgb(LINE_STRONG))
            .rounded(px(10.0))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(TEXT))
            .debug_selector(|| format!("tooltip={}", self.0))
            .child(self.0.clone())
    }
}

/// The key a DIR or SH folder's scan answer is stored under: its normalized
/// path, case-folded where the file system folds case.
fn scan_key(path: &str) -> String {
    let key = normalize_fs_path(path);
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

fn tooltip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = text.into();
    move |_, cx| cx.new(|_| Tip(text.clone())).into()
}
