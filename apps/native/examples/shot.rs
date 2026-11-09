//! Writes a PNG of the native client's real window showing one view, filled
//! with fixed fake content by a scripted fake daemon. No daemon starts, and
//! the user's config dir and live sessions are never read or written. The
//! window opens cloaked and is never activated, so it never shows on screen
//! or takes focus; its pixels are read with `PrintWindow`.
//!
//! `cargo run -p rustling-tulip-native --example shot -- [<view>] [<out.png>]`
//!
//! Views: `main` (the default: the sidebar and a split tab), `main-compact`
//! (the same window with the sidebar's leaves one line each), `main-narrow`
//! (the tulip tab split three ways, its narrow headers' buttons in their ⋯
//! menus), `source-control`, `diff`, `settings`, `spawn` (the spawn dialog with its
//! Target dropdown open), `viewer` (a viewer tab
//! over a small Rust file, scrolled to a line) and `viewer-md` (a viewer tab
//! over a Markdown file, rendered), `dashboard`, `connections` (the host
//! picker with a saved tailnet host and a saved SSH host),
//! `connections-ssh` (its add form switched to SSH, filled in, after a
//! passing Test connection) and `connections-scan-error` (the host picker
//! over a tailnet scan that failed with a long error), `multi-host` (the
//! main window with the tailnet host "box" online, one of its sessions in a
//! pane of the first tab, and the tailnet host "lab" offline), `spawn-hosts`
//! (the spawn
//! dialog over those hosts, its Target dropdown closed), `settings-hosts`
//! (Settings on the Claude page over those hosts, the host selector showing
//! This PC), `settings-dashboard` (Settings on the Dashboard tab with a keyed
//! provider chosen, its key stored, some spend and a Test result), `menu`
//! (the sessions panel's ⋯
//! menu open over the repo rows, with a toast over the panes), `chat` (the
//! Petal pane switched to its chat, a row of each kind, an answer with one
//! stretch open and two closed, and a live answer) and `summary` (the Petal
//! pane switched to its summary: three lines, the second one open, under a
//! pinned Permission card), `themes-edit` (the theme editor on a duplicate of
//! Nord with its Ground edited) and `themes` (the Themes modal over Settings,
//! over a user theme, a broken file and a file named after a built-in).
//! `settings-sounds` shows Settings on the Notifications tab with the Needs
//! you sound dropdown open, and `toast` a Needs you card on a permission,
//! with Allow and Deny, and a Finished card, as
//! the app's own toasts draw them. `open-shell` shows the Open a shell
//! dialog with a folder path longer than its field, the caret at its end.
//! `safe-start` shows the notice a start after two crashed ones raises.
//! `panes-accent` shows a 2x2 grid of four panes with the Theme pane
//! focused, and `panes-accent-chosen` the same grid with the Periwinkle
//! pane focused.
//! `update` shows Settings scrolled to its
//! update rows with a ready update in the footer, and `update-notes` the
//! What's new window the footer's button opens over that ready update.
//! The PNG goes to
//! `.tmp/shots/<view>.png` under the repo root unless `<out.png>` is given;
//! the window's layout file is kept beside it in `.ui/` and cleared on every
//! run.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Utc};
use daemon_client::device_tokens::MemoryTokens;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::{FutureExt as _, StreamExt as _};
use gpui::{
    App, AppContext as _, Application, AsyncApp, Bounds, Context, Entity, IntoElement, Keystroke,
    Render, Window, WindowBounds, WindowHandle, WindowOptions, div, prelude::*, px, size,
};
use protocol::{
    AgentCommandEntry, ChatEntry, ChatOption, ChatPrompt, ChatPromptKind, ChatQuestionTab,
    ChatScreen, ClientMessage, ComposerState, DaemonMessage, GridNode, SessionSnapshot,
    SplitDirection, StretchLine, SummarySource, ToolKind, ToolOutcome, WorkShell, WorkSubagent,
};
use rustling_tulip_native::builtin_themes::DRACULA;
use rustling_tulip_native::chat::{ChatMode, PaneView, RowShape};
use rustling_tulip_native::connection_picker::{PeerSource, SshTestCancel, SshTester};
use rustling_tulip_native::fonts;
use rustling_tulip_native::offscreen::show_cloaked;
use rustling_tulip_native::target::{HostKey, SshHost, Target};
use rustling_tulip_native::theme_store::{THEMES_DIR, ThemeStore};
use rustling_tulip_native::ui_theme::theme;
use rustling_tulip_native::update::{LastResult, Ready, Updater};
use rustling_tulip_native::{
    Activity, AppToast, AppToastKind, AskBlock, AskBox, AskChoice, Assets, CardText, Clock,
    Connection, FileFilter, FilePicker, FolderPicker, HandshakeInfo, NATIVE_PROTOCOL_VERSIONS,
    NetCommand, NetEvent, Notifier, NotifyKind, NotifyState, OpenFailure, Opener, RAIL_ITEM_HEIGHT,
    RAIL_WIDTH, RootDeps, RootView, SavePicker, SystemToastPlacer, TOAST_CARD_WIDTH, TextInput,
    ToastKind, ToastPresses, ToastSpec, Transport, TypedRow, ViewerLoad, bind_keys, card_height,
    key_of, remote_access::Autostart, toast_card,
};
use serde_json::{Value, json};
use tailnet::{PeerStatus, Status, TailnetError};

/// The window's size in logical pixels, the client's own.
const WINDOW_SIZE: (u16, u16) = (1000, 640);
/// The `dashboard` shot's window: wide enough for every column of the By
/// status view.
const DASHBOARD_WINDOW_SIZE: (u16, u16) = (2000, 900);

/// The window `toast` draws its three cards in.
const TOAST_WINDOW_SIZE: (u16, u16) = (420, 760);
/// The `dashboard-compact` shot's window: the 1080p the view must fit.
const DASHBOARD_COMPACT_WINDOW_SIZE: (u16, u16) = (1920, 1080);
/// The `summary` shot's window: tall enough for the split's top pane to
/// hold the pinned card, all three lines with one open, the strip and the
/// composer.
const SUMMARY_WINDOW_SIZE: (u16, u16) = (1000, 1000);
/// The `chat` shot's window: tall enough for the split's top pane to hold
/// the finished answer with its stretches above the newest rows.
const CHAT_WINDOW_SIZE: (u16, u16) = (1000, 1100);
/// The `spawn-hosts` shot's window: tall enough for the dialog to show its
/// grouped picker above the branch field the keyboard opens on.
const SPAWN_HOSTS_WINDOW_SIZE: (u16, u16) = (1000, 1300);
/// How long the window gets to draw the fixture before the view is driven.
const LOAD_WAIT: Duration = Duration::from_millis(1500);
/// How long the view gets to answer the input that reaches it.
const DRIVE_WAIT: Duration = Duration::from_millis(1000);
/// How many Downs on the Settings tab list reach the Claude tab, the page of
/// the registry's first agent: every tab this PC offers sits above it.
const CLAUDE_TAB_DOWNS: usize = 8;
/// How many Downs on the Settings tab list reach the Dashboard tab.
const DASHBOARD_TAB_DOWNS: usize = 2;
/// The port and pid the fake handshake reports.
const FAKE_PORT: u16 = 4242;
const FAKE_PID: u32 = 1;

/// The Source control item's place on the rail, after Sessions and Needs You.
/// The rail's buttons stack from its top with no gap, each
/// [`RAIL_ITEM_HEIGHT`] tall.
const SOURCE_CONTROL_ITEM: f32 = 2.0;
/// The Dashboard button's place on the rail, after the three panel items.
const DASHBOARD_ITEM: f32 = 3.0;
/// The Dashboard tab the `dashboard` shot's open makes.
const DASHBOARD_TAB: &str = "t-dashboard";

const TULIP: &str = "rustling-tulip";
const YAAT: &str = "yaat";
const WORKSPACE: &str = "flight-deck";
const TULIP_PATH: &str = r"D:\src\rustling-tulip";
const YAAT_PATH: &str = r"D:\src\yaat";
const DECK_SERVER: &str = "deck-server";
const DECK_CLIENT: &str = "deck-client";
const DECK_SERVER_PATH: &str = r"D:\src\flight-deck\deck-server";
const DECK_CLIENT_PATH: &str = r"D:\src\flight-deck\deck-client";
const PETAL_TREE: &str = r"D:\worktrees\wt.feat-petal-footer\rustling-tulip";
const RECOVER_TREE: &str = r"D:\worktrees\wt.feat-recover-keys\rustling-tulip";
const PETAL_BRANCH: &str = "feat/petal-footer";
const DIFF_TAB: &str = "t-diff";
const DIFF_PATH: &str = "apps/native/src/footer.rs";
const VIEWER_TAB: &str = "t-viewer";
/// The file the `viewer` shot views, and the line its open asks for.
const VIEWER_FILE: &str = "footer.rs";
const VIEWER_LINE: u32 = 30;
/// The file the `viewer-md` shot views.
const VIEWER_MD_FILE: &str = "README.md";
/// The `summary` shot's line drawn open: the second of its three.
const SUMMARY_OPEN_LINE: &str = "t1";
/// The stretch the `chat` shot's finished answer draws open, with a closed
/// stretch nested in it and another closed beside it.
const CHAT_OPEN_STRETCH: &str = "pills";
/// The `chat` shot's running Bash call, drawn open one statement per line.
const FOOTER_SPECS: &str = "cargo build -p rustling-tulip-native 2>&1 | tail -n 3; \
     cargo nextest run -p rustling-tulip-native -E 'test(/footer/)' > .tmp/footer.log 2>&1 && \
     echo specs passed || tail -n 20 .tmp/footer.log; git status --short apps/native/src/footer.rs";
/// The log that call writes, which the `chat` shot tails under its card.
const FOOTER_LOG: &str = "C:/dev/petal/.tmp/footer.log";
/// What the fake daemon's follow of [`FOOTER_LOG`] sends: nextest's
/// coloured lines, the colours stripped in the box.
const FOOTER_LOG_TEXT: &str = "\x1b[32;1m    Starting\x1b[0m 4 tests across 1 binary\n\
     \x1b[32;1m        PASS\x1b[0m [   0.412s] ui ui_footer::the_pills_keep_their_gap\n\
     \x1b[32;1m        PASS\x1b[0m [   0.388s] ui ui_footer::the_status_chip_reads_calm\n\
     \x1b[32;1m        PASS\x1b[0m [   0.405s] ui ui_footer::the_branch_chip_truncates\r\n";

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::WARN)
        .init();
    let shot = Shot::from_args(std::env::args().skip(1))?;
    shot.prepare()?;
    let outcome: Rc<RefCell<Option<Result<()>>>> = Rc::new(RefCell::new(None));
    let slot = Rc::clone(&outcome);
    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            if let Err(err) = start(shot, Rc::clone(&slot), cx) {
                *slot.borrow_mut() = Some(Err(err));
                cx.quit();
            }
        });
    outcome
        .take()
        .unwrap_or_else(|| Err(anyhow!("the app quit before the shot was taken")))
}

/// One of the windows the shot can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Main,
    MainCompact,
    MainNarrow,
    SourceControl,
    Diff,
    Settings,
    SettingsClaude,
    Spawn,
    Jump,
    Viewer,
    ViewerMd,
    Dashboard,
    DashboardCompact,
    Connections,
    ConnectionsSsh,
    ConnectionsScanError,
    Menu,
    Update,
    UpdateNotes,
    Chat,
    ChatCompact,
    Summary,
    Themes,
    ThemesEdit,
    MultiHost,
    SpawnHosts,
    SettingsHosts,
    SettingsDashboard,
    SettingsAccounts,
    Toast,
    SettingsSounds,
    Accounts,
    Gates,
    OpenShell,
    SafeStart,
    PanesAccent,
    PanesAccentChosen,
}

impl View {
    const ALL: [Self; 37] = [
        Self::Main,
        Self::MainCompact,
        Self::MainNarrow,
        Self::SourceControl,
        Self::Diff,
        Self::Settings,
        Self::SettingsClaude,
        Self::Spawn,
        Self::Jump,
        Self::Viewer,
        Self::ViewerMd,
        Self::Dashboard,
        Self::DashboardCompact,
        Self::Connections,
        Self::ConnectionsSsh,
        Self::ConnectionsScanError,
        Self::Menu,
        Self::Update,
        Self::UpdateNotes,
        Self::Chat,
        Self::ChatCompact,
        Self::Summary,
        Self::Themes,
        Self::ThemesEdit,
        Self::MultiHost,
        Self::SpawnHosts,
        Self::SettingsHosts,
        Self::SettingsDashboard,
        Self::SettingsAccounts,
        Self::SettingsSounds,
        Self::Accounts,
        Self::Gates,
        Self::Toast,
        Self::OpenShell,
        Self::SafeStart,
        Self::PanesAccent,
        Self::PanesAccentChosen,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::MainCompact => "main-compact",
            Self::MainNarrow => "main-narrow",
            Self::SourceControl => "source-control",
            Self::Diff => "diff",
            Self::Settings => "settings",
            Self::SettingsClaude => "settings-claude",
            Self::Spawn => "spawn",
            Self::Jump => "jump",
            Self::Viewer => "viewer",
            Self::ViewerMd => "viewer-md",
            Self::Dashboard => "dashboard",
            Self::DashboardCompact => "dashboard-compact",
            Self::Connections => "connections",
            Self::ConnectionsSsh => "connections-ssh",
            Self::ConnectionsScanError => "connections-scan-error",
            Self::Menu => "menu",
            Self::Update => "update",
            Self::UpdateNotes => "update-notes",
            Self::Chat => "chat",
            Self::ChatCompact => "chat-compact",
            Self::Summary => "summary",
            Self::Themes => "themes",
            Self::ThemesEdit => "themes-edit",
            Self::MultiHost => "multi-host",
            Self::SpawnHosts => "spawn-hosts",
            Self::SettingsHosts => "settings-hosts",
            Self::SettingsDashboard => "settings-dashboard",
            Self::SettingsAccounts => "settings-accounts",
            Self::SettingsSounds => "settings-sounds",
            Self::Accounts => "accounts",
            Self::Gates => "gates",
            Self::OpenShell => "open-shell",
            Self::SafeStart => "safe-start",
            Self::PanesAccent => "panes-accent",
            Self::PanesAccentChosen => "panes-accent-chosen",
            Self::Toast => "toast",
        }
    }

    /// How many rounds of input reach the view, each given [`DRIVE_WAIT`]
    /// to draw before the next: the add form opens from the picker, its SSH
    /// segment is pressed once drawn, and its Host field takes typing only
    /// once drawn; the update shot checks and opens Settings, then moves the
    /// keyboard to Check now, which scrolls the update rows into view; the
    /// update-notes shot checks, then opens the What's new window over the
    /// update it found; a
    /// summary line opens only once the lines have arrived; the Themes
    /// modal opens over Settings once drawn, and takes its Down once drawn;
    /// the editor opens on the modal once drawn, its input takes the keyboard
    /// once drawn and takes its typing once focused; the chat opens, then its
    /// stretch, Bash card and Tail box, then scrolls up to the box once drawn;
    /// Claude's page opens, then the wheel takes it down to its Hooks
    /// section; the accounts page opens at its top, then wheels down to the
    /// auto-switch block that ends it.
    fn steps(self) -> usize {
        match self {
            Self::ConnectionsSsh => 4,
            Self::Themes | Self::Chat | Self::SettingsDashboard => 3,
            Self::ThemesEdit => 5,
            // The jump palette opens, then takes its query and Enter.
            Self::PanesAccentChosen
            | Self::Update
            | Self::UpdateNotes
            | Self::Summary
            | Self::SettingsClaude
            | Self::SettingsAccounts => 2,
            _ => 1,
        }
    }

    /// The window's size for this view.
    fn window_size(self) -> (u16, u16) {
        match self {
            Self::Dashboard => DASHBOARD_WINDOW_SIZE,
            Self::Toast => TOAST_WINDOW_SIZE,
            Self::DashboardCompact => DASHBOARD_COMPACT_WINDOW_SIZE,
            Self::Summary => SUMMARY_WINDOW_SIZE,
            Self::Chat | Self::ChatCompact => CHAT_WINDOW_SIZE,
            Self::SpawnHosts => SPAWN_HOSTS_WINDOW_SIZE,
            _ => WINDOW_SIZE,
        }
    }

    fn parse(name: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|view| view.name() == name)
            .ok_or_else(|| {
                let names: Vec<&str> = Self::ALL.iter().map(|view| view.name()).collect();
                anyhow!("unknown view {name:?}; the views are {}", names.join(", "))
            })
    }

    /// Whether the view opens over the `multi-host` shot's hosts: "box"
    /// online, "lab" offline.
    fn on_hosts(self) -> bool {
        matches!(
            self,
            Self::MultiHost | Self::SpawnHosts | Self::SettingsHosts
        )
    }

    /// Whether `root` shows this view.
    #[expect(
        clippy::too_many_lines,
        reason = "a flat exhaustive match over the views"
    )]
    fn reached(self, root: &RootView) -> bool {
        match self {
            // Drawn by `start_toast_shot`, in a window with no root view.
            Self::Toast => true,
            // The popover open over the fake daemon's accounts.
            Self::Accounts => root.accounts_open() && root.accounts_chip_text().is_some(),
            // The Gates panel over the fake daemon's gate state.
            Self::Gates => root.shown_activity() == Activity::Gates,
            Self::OpenShell => root.shell_dialog_open(),
            Self::SafeStart => root.start_notice_open(),
            Self::Main | Self::MainCompact | Self::MainNarrow => {
                !root.sidebar_containers().is_empty() && !root.tab_ids().is_empty()
            }
            // The four-pane grid with its Theme pane focused.
            Self::PanesAccent => root.focused_pane().as_deref() == Some("p1"),
            // The same grid with the Periwinkle pane focused from the jump
            // palette.
            Self::PanesAccentChosen => root.focused_pane().as_deref() == Some("p3"),
            Self::SourceControl => root.activity() == Activity::SourceControl,
            Self::Diff => root.active_tab_id() == Some(DIFF_TAB),
            Self::Settings => root.settings_open(),
            Self::SettingsClaude => root.settings_open() && root.settings_tab() == "Claude",
            Self::SettingsAccounts => root.settings_open() && root.settings_tab() == "Accounts",
            Self::SettingsSounds => {
                root.settings_open() && root.open_sound_list() == Some(NotifyKind::NeedsYou)
            }
            Self::Spawn => root.spawn_dialog_open(),
            Self::Jump => root.quick_jump_open(),
            Self::Viewer | Self::ViewerMd => {
                root.active_tab_id() == Some(VIEWER_TAB)
                    && root
                        .viewer_tab(VIEWER_TAB)
                        .is_some_and(|tab| tab.load() == ViewerLoad::Loaded)
            }
            Self::Dashboard | Self::DashboardCompact => {
                root.active_tab_id() == Some(DASHBOARD_TAB)
                    && root
                        .dashboard_view_rows()
                        .iter()
                        .flat_map(|(_, rows)| rows)
                        .any(|row| row.context != "—")
            }
            Self::Chat => {
                root.pane_shows_chat("p1")
                    && root
                        .chat_answer_stretchtext("s-petal", "a1")
                        .iter()
                        .any(|(_, key)| key == CHAT_OPEN_STRETCH)
                    && root
                        .chat_answer_text("s-petal", "a1")
                        .is_some_and(|text| text.contains("bottom edge"))
                    && root.chat_tool_lines("s-petal", "t3").len() > 1
                    && root
                        .chat_tail_lines("s-petal", FOOTER_LOG)
                        .is_some_and(|lines| lines.len() > 1)
            }
            // The run of read, search and run calls near the chat's end,
            // drawn as one Compact line at its bottom.
            Self::ChatCompact => {
                root.pane_shows_chat("p1")
                    && root.chat_mode() == ChatMode::Compact
                    && root.chat_display_rows("s-petal").iter().any(
                        |row| matches!(row, RowShape::Run { ids, .. } if ids.first().is_some_and(|id| id == "t4")),
                    )
            }
            Self::Summary => {
                root.pane_view("p1") == PaneView::Summary
                    && root.summary_line_open("s-petal", SUMMARY_OPEN_LINE)
                    && !root.chat_rows("s-petal").is_empty()
            }
            Self::Connections => {
                root.connection_picker_open() && root.connection_picker_scan_text().is_none()
            }
            Self::ConnectionsSsh => {
                root.connection_picker_ssh_result().as_deref()
                    == Some(&*format!("Connected: {SHOT_SSH_SHORT}"))
            }
            Self::ConnectionsScanError => {
                root.connection_picker_scan_text() == Some(shot_scan_error_text())
            }
            Self::Menu => root.sidebar_more_open() && !root.toasts().is_empty(),
            Self::Update => root.settings_open() && root.update_footer_text().is_some(),
            Self::UpdateNotes => {
                root.whats_new_open() && root.whats_new_text().as_deref() == Some(SHOT_NOTES)
            }
            Self::SettingsDashboard => {
                root.settings_open()
                    && root.settings_tab() == "Dashboard"
                    && root
                        .dashboard_test_result()
                        .is_some_and(|result| result.contains(" ms "))
            }
            Self::Themes => root.themes_modal_open(),
            Self::ThemesEdit => root
                .themes_editor()
                .is_some_and(rustling_tulip_native::themes::ThemeEditor::is_dirty),
            Self::SpawnHosts => root.spawn_dialog_open() && Self::MultiHost.reached(root),
            // This PC and box choosable, lab listed offline.
            Self::SettingsHosts => {
                let choices = root.settings_host_choices();
                Self::SettingsClaude.reached(root)
                    && choices.len() == 3
                    && choices.iter().filter(|(_, open)| *open).count() == 2
            }
            Self::MultiHost => {
                !root.tab_ids().is_empty()
                    && root
                        .sidebar_containers()
                        .iter()
                        .flat_map(|container| &container.leaves)
                        .any(|leaf| leaf.id == BOX_SESSION)
            }
        }
    }
}

/// The folder the `open-shell` shot puts in the Open a shell dialog: longer
/// than its field.
const SHOT_SHELL_FOLDER: &str = r"C:\Users\someone\source\repos\a-very-long-folder-name\with\several\levels\of\nesting\to-overflow-the-field";

/// The Ground the `themes-edit` shot types.
const SHOT_EDITED_GROUND: &str = "#5a6478";

/// What to show and where to write it.
struct Shot {
    view: View,
    out: PathBuf,
    ui_dir: PathBuf,
}

impl Shot {
    fn from_args(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let view = View::parse(args.next().as_deref().unwrap_or("main"))?;
        let out = match args.next() {
            Some(out) => PathBuf::from(out),
            None => default_dir()?.join(format!("{}.png", view.name())),
        };
        if let Some(extra) = args.next() {
            bail!("unexpected argument {extra:?}; usage: shot [<view>] [<out.png>]");
        }
        let out = std::path::absolute(&out)
            .with_context(|| format!("resolving the output path {}", out.display()))?;
        let dir = out
            .parent()
            .with_context(|| format!("the output path {} has no folder", out.display()))?;
        let ui_dir = dir.join(".ui");
        Ok(Self { view, out, ui_dir })
    }

    /// Creates the output folders and clears the layout an earlier run saved,
    /// so every run starts from the same state; a `main-compact` shot then
    /// seeds the sidebar's compact leaf density, which no keystroke in a
    /// cloaked window can reach.
    fn prepare(&self) -> Result<()> {
        std::fs::create_dir_all(&self.ui_dir)
            .with_context(|| format!("creating {}", self.ui_dir.display()))?;
        let saved = self.ui_dir.join("native-ui.json");
        if let Err(err) = std::fs::remove_file(&saved)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            return Err(err).with_context(|| format!("clearing {}", saved.display()));
        }
        let sample_text = match self.view {
            View::Viewer => Some(FOOTER_OLD),
            View::ViewerMd => Some(README_MD),
            View::Main
            | View::MainCompact
            | View::MainNarrow
            | View::SourceControl
            | View::Diff
            | View::Settings
            | View::SettingsClaude
            | View::Spawn
            | View::Jump
            | View::Dashboard
            | View::DashboardCompact
            | View::Connections
            | View::ConnectionsSsh
            | View::ConnectionsScanError
            | View::Menu
            | View::Update
            | View::UpdateNotes
            | View::Chat
            | View::ChatCompact
            | View::Summary
            | View::Themes
            | View::ThemesEdit
            | View::MultiHost
            | View::SpawnHosts
            | View::SettingsHosts
            | View::SettingsDashboard
            | View::SettingsAccounts
            | View::SettingsSounds
            | View::Accounts
            | View::Gates
            | View::OpenShell
            | View::SafeStart
            | View::PanesAccent
            | View::PanesAccentChosen
            | View::Toast => None,
        };
        if let Some(text) = sample_text {
            let sample = self.viewer_sample();
            return std::fs::write(&sample, text)
                .with_context(|| format!("writing {}", sample.display()));
        }
        if matches!(self.view, View::Update | View::UpdateNotes) {
            let installer = self.ui_dir.join(SHOT_INSTALLER);
            return std::fs::write(&installer, b"stand-in installer")
                .with_context(|| format!("writing {}", installer.display()));
        }
        if self.view == View::Themes {
            return self.seed_themes();
        }
        let state = match self.view {
            View::MainCompact => compact_ui_state(),
            View::Dashboard => json!({ "dashboard": { "view": "by_status" } }),
            View::Connections | View::ConnectionsSsh | View::ConnectionsScanError => {
                connections_ui_state()
            }
            View::MultiHost | View::SpawnHosts | View::SettingsHosts => multi_host_ui_state(),
            _ => return Ok(()),
        };
        let layout = serde_json::to_vec_pretty(&state)
            .with_context(|| format!("serializing the layout for {}", saved.display()))?;
        std::fs::write(&saved, layout).with_context(|| format!("writing {}", saved.display()))
    }

    /// Writes the `themes` shot's user themes: one good, one that does not
    /// parse, and one named after a built-in. A run starts from none.
    fn seed_themes(&self) -> Result<()> {
        let dir = self.ui_dir.join(THEMES_DIR);
        if let Err(err) = std::fs::remove_dir_all(&dir)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            return Err(err).with_context(|| format!("clearing {}", dir.display()));
        }
        let id = ThemeStore::new(dir.clone())
            .save("Harbour", &DRACULA, Some("dracula"), Some("dracula"))
            .map_err(|err| anyhow!("saving the shot's theme: {err}"))?;
        std::fs::copy(dir.join(format!("{id}.json")), dir.join("nord.json"))
            .with_context(|| format!("writing the shadow file in {}", dir.display()))?;
        std::fs::write(dir.join("broken.json"), "{")
            .with_context(|| format!("writing the broken file in {}", dir.display()))
    }

    /// The file the `viewer` or `viewer-md` shot's tab views, beside the
    /// layout file.
    fn viewer_sample(&self) -> PathBuf {
        let file = if self.view == View::ViewerMd {
            VIEWER_MD_FILE
        } else {
            VIEWER_FILE
        };
        self.ui_dir.join(file)
    }
}

/// The layout a `main-compact` shot starts from: the sidebar's leaves one
/// line each. A file holding the general section alone loads as the layout
/// defaults everywhere else, which is what a missing file loads as.
fn compact_ui_state() -> Value {
    json!({ "general": { "leaf_density": "compact" } })
}

/// The tailnet host the `connections` shots have saved, not yet paired.
const SHOT_TAILNET_HOST: &str = "box.tail1234.ts.net";
/// The SSH host the `connections` shots have saved.
const SHOT_SSH_SAVED: &str = "me@buildbox";
/// The SSH host the `connections-ssh` shot types, and its short name.
const SHOT_SSH_TYPED: &str = "me@devbox";
const SHOT_SSH_SHORT: &str = "devbox";

/// The layout the `connections` shots start from: a saved tailnet host,
/// which had 4 sessions last time, and a saved SSH host switched off.
fn connections_ui_state() -> Value {
    json!({
        "saved_hosts": [
            { "name": SHOT_TAILNET_HOST, "port": 8787 },
            { "kind": "ssh", "name": SHOT_SSH_SAVED, "enabled": false },
        ],
        "host_session_counts": { SHOT_TAILNET_HOST: 4 },
    })
}

/// The tailnet host the `multi-host` shot has saved and cannot reach.
const SHOT_LAB_HOST: &str = "lab.tail1234.ts.net";
/// The repo the `multi-host` shot's "box" lists, and its one session.
const BOX_REPO: &str = "relay";
const BOX_REPO_PATH: &str = r"D:\src\relay";
const BOX_SESSION: &str = "b-relay";

/// The layout the `multi-host` shot starts from: "box" and "lab" saved and
/// switched on, so launch connects both.
fn multi_host_ui_state() -> Value {
    json!({
        "saved_hosts": [
            { "name": SHOT_TAILNET_HOST, "port": 8787 },
            { "name": SHOT_LAB_HOST, "port": 8787 },
        ],
    })
}

/// The owner's tailnet as the `connections` shots see it: this PC and one
/// online laptop.
struct ShotPeers;

impl PeerSource for ShotPeers {
    fn status(&self) -> Result<Status, TailnetError> {
        let node = |name: &str| PeerStatus {
            dns_name: format!("{name}."),
            user_id: 1,
            online: true,
            ..PeerStatus::default()
        };
        Ok(Status {
            self_node: Some(node("this-pc.tail1234.ts.net")),
            peer: [("nodekey:0".to_owned(), node("laptop.tail1234.ts.net"))]
                .into_iter()
                .collect(),
        })
    }
}

/// The error the `connections-scan-error` shot's Tailscale fails with.
const SHOT_SCAN_ERROR: &str = "Tailscale could not report this PC's tailnet: the local \
                               tailscaled service did not answer. Start Tailscale and sign \
                               in, then open Connections again.";

/// The text the picker shows for [`SHOT_SCAN_ERROR`]: the error's `Display`.
fn shot_scan_error_text() -> String {
    TailnetError::Failed(SHOT_SCAN_ERROR.to_owned()).to_string()
}

/// A Tailscale whose every scan fails with [`SHOT_SCAN_ERROR`].
struct ShotPeersFailed;

impl PeerSource for ShotPeersFailed {
    fn status(&self) -> Result<Status, TailnetError> {
        Err(TailnetError::Failed(SHOT_SCAN_ERROR.to_owned()))
    }
}

/// A Test connection that every host passes, so the shot runs no `ssh`.
struct ShotSsh;

impl SshTester for ShotSsh {
    fn test(
        &self,
        _host: SshHost,
        _cancel: SshTestCancel,
    ) -> BoxFuture<'static, Result<(), String>> {
        futures::future::ready(Ok(())).boxed()
    }
}

/// Step `step` of the `themes` shot: Settings, then the modal over it, then
/// Down, so the preview shows the theme after the one in use.
fn drive_themes(step: usize, window: &mut Window, cx: &mut App) -> Result<()> {
    match step {
        0 => press("ctrl-,", window, cx),
        1 => {
            let root = window
                .root::<RootView>()
                .flatten()
                .context("the window has no root view")?;
            root.update(cx, |root, cx| root.open_themes_modal(window, cx));
            Ok(())
        }
        _ => press("down", window, cx),
    }
}

/// Step `step` of the `themes-edit` shot: the modal as in the `themes` shot,
/// then Duplicate and edit on the theme in use, the hex input, and a new Ground
/// that takes some pairs below their floor.
fn drive_themes_edit(step: usize, window: &mut Window, cx: &mut App) -> Result<()> {
    match step {
        0 | 1 => drive_themes(step, window, cx),
        2 => {
            let root = window
                .root::<RootView>()
                .flatten()
                .context("the window has no root view")?;
            root.update(cx, |root, cx| root.duplicate_theme(window, cx));
            Ok(())
        }
        3 => press("enter", window, cx),
        _ => {
            for key in SHOT_EDITED_GROUND.chars() {
                press(&key.to_string(), window, cx)?;
            }
            press("enter", window, cx)
        }
    }
}

/// Opens the Connections picker over the shot's Tailscale and `ssh`.
fn open_picker(window: &mut Window, cx: &mut App) -> Result<()> {
    open_picker_over(Arc::new(ShotPeers), window, cx)
}

/// Opens the Connections picker over `peers` and the shot's `ssh`.
fn open_picker_over(
    peers: Arc<dyn PeerSource + Send + Sync>,
    window: &mut Window,
    cx: &mut App,
) -> Result<()> {
    let root = window
        .root::<RootView>()
        .flatten()
        .context("the window has no root view")?;
    root.update(cx, |root, cx| {
        root.set_peer_source(peers);
        root.set_ssh_tester(Arc::new(ShotSsh));
        root.open_connection_picker(window, cx);
    });
    Ok(())
}

/// Step `step` of the `connections-ssh` shot: open the picker; Shift+Tab
/// twice from This PC's row (past Close) to `+ Add host` and press Enter,
/// which opens the form on its Name field; Shift+Tab once to the SSH
/// segment and press Enter, which focuses the Host field; type the host and
/// press Enter, which runs Test connection.
fn drive_connections_ssh(step: usize, window: &mut Window, cx: &mut App) -> Result<()> {
    match step {
        0 => open_picker(window, cx),
        1 => ["shift-tab", "shift-tab", "enter"]
            .into_iter()
            .try_for_each(|key| press(key, window, cx)),
        2 => ["shift-tab", "enter"]
            .into_iter()
            .try_for_each(|key| press(key, window, cx)),
        _ => {
            for c in SHOT_SSH_TYPED.chars() {
                press(&c.to_string(), window, cx)?;
            }
            press("enter", window, cx)
        }
    }
}

/// `.tmp/shots` under the repo root.
fn default_dir() -> Result<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .context("the native client's folder has no repo root two levels up")?;
    Ok(root.join(".tmp").join("shots"))
}

/// Opens the window on the fake daemon, then drives it to the view and
/// captures it; `slot` receives the outcome and the app quits.
fn start(shot: Shot, slot: Rc<RefCell<Option<Result<()>>>>, cx: &mut App) -> Result<()> {
    bind_keys(cx);
    fonts::register_bundled(cx);
    if shot.view == View::Toast {
        return start_toast_shot(shot, slot, cx);
    }
    let (cmd_tx, cmd_rx) = unbounded();
    let (event_tx, event_rx) = unbounded();
    let now = Utc::now();
    let fixture = Fixture::build(shot.view, now)?;
    let failure = Arc::new(Mutex::new(None));
    let remote = if shot.view.on_hosts() {
        Some(start_box_daemon(now, &failure)?)
    } else {
        None
    };
    let handle = open_window(
        deps(shot.view, (cmd_tx, event_rx), remote, &shot.ui_dir),
        shot.view.window_size(),
        cx,
    )?;
    let address = handle.update(cx, |_, window, _| win32::window_address(window))??;
    let daemon = FakeDaemon {
        events: event_tx,
        view: shot.view,
        now,
        failure: Arc::clone(&failure),
    };
    std::thread::Builder::new()
        .name("fake-daemon".to_owned())
        .spawn(move || daemon.run(&fixture, cmd_rx))
        .context("starting the fake daemon's thread")?;
    cx.spawn(async move |cx| {
        let result = drive_and_capture(handle, &shot, address, cx, &failure).await;
        *slot.borrow_mut() = Some(result);
        if let Err(err) = cx.update(|cx| cx.quit()) {
            tracing::warn!("quitting after the shot: {err:#}");
        }
    })
    .detach();
    Ok(())
}

/// One connection's channels: the commands the window sends and the
/// events it reads.
type HostPair = (UnboundedSender<NetCommand>, UnboundedReceiver<NetEvent>);

/// Starts the fake daemon the `multi-host` shot's "box" answers with, on a
/// thread of its own, and returns the channels the window's connection to
/// "box" uses.
fn start_box_daemon(
    now: DateTime<Utc>,
    failure: &Arc<Mutex<Option<anyhow::Error>>>,
) -> Result<HostPair> {
    let fixture = RemoteFixture::build(now)?;
    let (cmd_tx, cmd_rx) = unbounded();
    let (event_tx, event_rx) = unbounded();
    let daemon = FakeDaemon {
        events: event_tx,
        view: View::MultiHost,
        now,
        failure: Arc::clone(failure),
    };
    let target = Target::tailnet(SHOT_TAILNET_HOST, Some(8787));
    std::thread::Builder::new()
        .name("fake-box-daemon".to_owned())
        .spawn(move || daemon.run_remote(&fixture, &target, cmd_rx))
        .context("starting the box daemon's thread")?;
    Ok((cmd_tx, event_rx))
}

/// The state a later connection's pair reports: the `multi-host` shot's
/// other hosts were reached and dropped, and the `connections` shots'
/// saved tailnet host does not know this PC.
fn later_host_state(view: View, target: &Target) -> Option<Connection> {
    let mut conn = Connection::for_target(target.clone());
    conn.on_connecting();
    conn.on_socket_open(8787);
    if view.on_hosts() {
        let version = NATIVE_PROTOCOL_VERSIONS.first().copied().unwrap_or(1);
        conn.on_welcome(version, Vec::new(), None);
        let _ = conn.on_closed("socket closed".to_owned());
        return Some(conn);
    }
    target.tailnet_host()?;
    conn.on_not_approved("device not approved".to_owned(), false);
    Some(conn)
}

/// The root view's dependencies: the fake daemon's channels (and, for the
/// `multi-host` shot, the "box" daemon's), a frozen clock, and an opener,
/// notifier and folder picker that do nothing.
fn deps(view: View, local: HostPair, remote: Option<HostPair>, ui_dir: &Path) -> RootDeps {
    let started = Instant::now();
    let now: Clock = Arc::new(move || started);
    let pick_folder: FolderPicker =
        Rc::new(|_: &mut App, _: Option<PathBuf>| futures::future::ready(None).boxed_local());
    let pick_file: FilePicker = Rc::new(|_: &mut App, _: Option<PathBuf>, _: FileFilter| {
        futures::future::ready(None).boxed_local()
    });
    let pick_save: SavePicker = Rc::new(
        |_: &mut App, _: Option<PathBuf>, _: String, _: FileFilter| {
            futures::future::ready(Ok(None)).boxed_local()
        },
    );
    // The fake daemon's channels serve the first connection, and the "box"
    // daemon's the `multi-host` shot's "box"; any other gets a pair nothing
    // answers but its state.
    let first = RefCell::new(Some(local));
    let remote = RefCell::new(remote);
    let open_host: Transport = Rc::new(move |target: &Target| {
        if let Some(pair) = first.borrow_mut().take() {
            return pair;
        }
        if target
            .tailnet_host()
            .is_some_and(|host| host.name() == SHOT_TAILNET_HOST)
            && let Some(pair) = remote.borrow_mut().take()
        {
            return pair;
        }
        let (tx, _) = unbounded();
        let (states, events) = unbounded();
        if let Some(conn) = later_host_state(view, target)
            && states.unbounded_send(NetEvent::State(conn)).is_err()
        {
            tracing::warn!("the shot's host state found no listener");
        }
        (tx, events)
    });
    RootDeps {
        start_mode: if view == View::SafeStart {
            rustling_tulip_native::start_guard::StartMode::CloseTabs
        } else {
            rustling_tulip_native::start_guard::StartMode::Normal
        },
        open_host,
        ui_dir: Some(ui_dir.to_path_buf()),
        paths: Err("the shot has no config dir".to_owned()),
        wanted: None,
        now,
        quit: Box::new(|cx: &mut App| cx.quit()),
        open: Arc::new(NoOpener),
        notify: Arc::new(NoNotifier),
        toast_placer: Arc::new(SystemToastPlacer),
        sound: Arc::new(rustling_tulip_native::SilentSounder),
        badge: std::rc::Rc::new(rustling_tulip_native::NoTaskbarBadge),
        pick_folder,
        pick_file,
        pick_save,
        autostart: Arc::new(NoAutostart),
        // No device token, so the `connections` shots' saved tailnet host
        // reads as not paired.
        tokens: Arc::new(MemoryTokens::default()),
        updater: Arc::new(ShotUpdater {
            installer: ui_dir.join(SHOT_INSTALLER),
            offers: matches!(view, View::Update | View::UpdateNotes),
        }),
    }
}

/// The ask block of a permission, with Allow and Deny.
fn permission_ask() -> AskBlock {
    let choice = |label: &str, choice| AskChoice {
        label: label.to_owned(),
        hint: None,
        choice,
        description: None,
    };
    AskBlock {
        session_id: "s1".to_owned(),
        label: "Permission ·".to_owned(),
        text: "Bash".to_owned(),
        command: Some("gh pr merge 118 --squash".to_owned()),
        since: Some(Utc::now()),
        choices: vec![
            choice("Allow", protocol::PromptChoice::Allow),
            choice("Deny", protocol::PromptChoice::Deny),
        ],
        typed: None,
    }
}

/// The ask block of a single-choice question whose three answers are long
/// sentences, each with a description.
fn question_ask() -> AskBlock {
    let choice = |index: u16, label: &str, description: &str| AskChoice {
        label: label.to_owned(),
        hint: Some((index + 1).to_string()),
        choice: protocol::PromptChoice::Option { index },
        description: Some(description.to_owned()),
    };
    AskBlock {
        session_id: "s3".to_owned(),
        label: "Question ·".to_owned(),
        text: "Which way should the release go out?".to_owned(),
        command: None,
        since: Some(Utc::now()),
        choices: vec![
            choice(
                0,
                "Tag it now and publish the installer once the signing step has passed",
                "The release goes out today, signed, with the notes as they stand.",
            ),
            choice(
                1,
                "Hold it until the phone app's terminal fixes land, then tag both together",
                "One release carries both changes; the notes cover the phone app too.",
            ),
            choice(
                2,
                "Publish a preview build only, for the people who asked to try it first",
                "Nothing is tagged; the preview channel gets the build.",
            ),
        ],
        typed: Some(AskBox::type_something()),
    }
}

/// A card's controls, doing nothing: nothing presses them in a shot. With
/// `typed`, the card draws its text box with that input.
fn idle_presses(typed: Option<Entity<TextInput>>) -> ToastPresses {
    ToastPresses {
        answer: Rc::new(|_, _, _| {}),
        go: Rc::new(|_, _, _| {}),
        dismiss: Rc::new(|_, _, _| {}),
        typed: typed.map(|input| TypedRow {
            input,
            send: Rc::new(|_, _, _| {}),
            take_keys: Rc::new(|_, _, _| {}),
        }),
    }
}

/// A Needs you card, a Finished card and a Needs you card asking a question
/// with long answers and its text box, as the app's own toasts draw them.
struct ToastShot {
    needs_you: AppToast,
    finished: AppToast,
    question: AppToast,
    /// The question card's text box input, empty, showing its placeholder.
    typed: Entity<TextInput>,
}

impl ToastShot {
    fn new(cx: &mut Context<Self>) -> Self {
        let typed = cx.new(|cx| TextInput::new("", AskBox::type_something().placeholder, cx));
        let toast = |id: &str, kind, title: &str, body: &str| {
            AppToast::new(
                key_of(&HostKey::Local, id),
                kind,
                title.to_owned(),
                body.to_owned(),
                Instant::now(),
            )
        };
        Self {
            needs_you: toast(
                "s1",
                AppToastKind::NeedsYou { has_answers: false },
                "Needs you · box",
                "Petal: Waiting for your permission to merge the pull request that \
                 moves the sessions table",
            )
            .waiting_on(Some(permission_ask()), None),
            finished: toast("s2", AppToastKind::Finished, "Finished", "Bramble"),
            question: toast(
                "s3",
                AppToastKind::NeedsYou { has_answers: false },
                "Needs you · web",
                "Thistle: Which way should the release go out?",
            )
            .waiting_on(Some(question_ask()), None),
            typed,
        }
    }
}

impl Render for ToastShot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let card = |toast: &AppToast, typed: Option<Entity<TextInput>>, cx: &App| {
            div()
                .w(px(TOAST_CARD_WIDTH))
                .h(px(card_height(toast, false, &CardText::new(cx), f32::MAX)))
                .child(toast_card(toast, false, None, idle_presses(typed), cx))
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(16.0))
            .bg(gpui::rgb(theme(cx).surface))
            .child(card(&self.needs_you, None, cx))
            .child(card(&self.finished, None, cx))
            .child(card(&self.question, Some(self.typed.clone()), cx))
    }
}

/// Opens [`ToastShot`] in a cloaked window, captures it, and quits; `slot`
/// receives the outcome. No daemon is involved.
fn start_toast_shot(shot: Shot, slot: Rc<RefCell<Option<Result<()>>>>, cx: &mut App) -> Result<()> {
    let window_size = shot.view.window_size();
    let (width, height) = window_size;
    let bounds = Bounds::centered(None, size(px(width.into()), px(height.into())), cx);
    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                focus: false,
                show: false,
                ..Default::default()
            },
            move |window, cx| {
                show_cloaked(window, window_size);
                cx.new(ToastShot::new)
            },
        )
        .context("opening the window")?;
    let address = handle.update(cx, |_, window, _| win32::window_address(window))??;
    cx.spawn(async move |cx| {
        cx.background_executor().timer(LOAD_WAIT).await;
        let result = capture(shot.out, address, &Mutex::new(None)).await;
        *slot.borrow_mut() = Some(result);
        if let Err(err) = cx.update(|cx| cx.quit()) {
            tracing::warn!("quitting after the shot: {err:#}");
        }
    })
    .detach();
    Ok(())
}

/// Opens the root view in a cloaked window that is never activated.
fn open_window(
    deps: RootDeps,
    window_size: (u16, u16),
    cx: &mut App,
) -> Result<WindowHandle<RootView>> {
    let (width, height) = window_size;
    let bounds = Bounds::centered(None, size(px(width.into()), px(height.into())), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // gpui activates any window it shows, so it opens hidden and
            // `show_cloaked` shows it.
            focus: false,
            show: false,
            ..Default::default()
        },
        move |window, cx| {
            show_cloaked(window, window_size);
            cx.new(|cx| RootView::with_transport(deps, window, cx))
        },
    )
    .context("opening the window")
}

/// Waits for the fixture to draw, drives the window to the view, checks it
/// got there, then captures it on a thread of its own.
async fn drive_and_capture(
    handle: WindowHandle<RootView>,
    shot: &Shot,
    address: usize,
    cx: &mut AsyncApp,
    failure: &Mutex<Option<anyhow::Error>>,
) -> Result<()> {
    let view = shot.view;
    let sample = shot.viewer_sample();
    cx.background_executor().timer(LOAD_WAIT).await;
    for step in 0..view.steps() {
        cx.update_window(handle.into(), |_, window, cx| {
            drive(view, step, address, &sample, window, cx)
        })??;
        cx.background_executor().timer(DRIVE_WAIT).await;
    }
    let reached = handle.update(cx, |root, _, _| view.reached(root))?;
    if !reached {
        bail!("the input for the {} view did not open it", view.name());
    }
    check_daemon(failure)?;
    capture(shot.out.clone(), address, failure).await
}

/// Writes the window at `address` to `out`, on a thread of its own.
async fn capture(
    out: PathBuf,
    address: usize,
    failure: &Mutex<Option<anyhow::Error>>,
) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    std::thread::Builder::new()
        .name("capture".to_owned())
        .spawn(move || {
            if tx.send(win32::write_settled(address, &out)).is_err() {
                tracing::warn!("the capture finished after the app stopped waiting");
            }
        })
        .context("starting the capture thread")?;
    let settled = rx
        .await
        .context("the capture thread ended without an answer")??;
    check_daemon(failure)?;
    if !settled {
        tracing::warn!("the window kept changing; wrote its last capture");
    }
    Ok(())
}

/// Switches pane `p1` to its chat, which opens in Compact and follows its
/// bottom.
fn open_chat(window: &mut Window, cx: &mut App) -> Result<()> {
    let root = window
        .root::<RootView>()
        .flatten()
        .context("the window has no root view")?;
    root.update(cx, |root, cx| root.toggle_pane_view("p1", window, cx));
    Ok(())
}

/// Sends the input that opens `view`: a keystroke dispatched in the window,
/// or, where no key opens it, a click posted to the window at `address`;
/// the `viewer` view opens a viewer tab of `sample` at a line, as a link
/// does. `step` counts the rounds of [`View::steps`].
#[expect(
    clippy::too_many_lines,
    reason = "a flat exhaustive match over the views"
)]
fn drive(
    view: View,
    step: usize,
    address: usize,
    sample: &Path,
    window: &mut Window,
    cx: &mut App,
) -> Result<()> {
    match view {
        View::Main
        | View::MainCompact
        | View::MainNarrow
        | View::MultiHost
        | View::SafeStart
        | View::PanesAccent
        | View::Toast => Ok(()),
        View::PanesAccentChosen => drive_panes_accent_chosen(step, window, cx),
        View::Accounts => open_accounts(window, cx),
        View::Gates => show_gates(window, cx),
        View::OpenShell => open_shell(window, cx),
        View::SourceControl => {
            let y = (SOURCE_CONTROL_ITEM + 0.5) * RAIL_ITEM_HEIGHT;
            #[expect(
                clippy::cast_possible_truncation,
                reason = "the rail's middle lies within a few hundred logical pixels"
            )]
            let at = ((RAIL_WIDTH / 2.0).round() as i32, y.round() as i32);
            win32::click(address, at)
        }
        View::Diff => press("ctrl-3", window, cx),
        View::Settings => press("ctrl-,", window, cx),
        View::SettingsClaude | View::SettingsHosts => drive_claude_page(step, address, window, cx),
        View::SettingsSounds => drive_settings_sounds(window, cx),
        View::SettingsAccounts => drive_settings_accounts(step, address, window, cx),
        View::Spawn => {
            press("ctrl-shift-n", window, cx)?;
            let root = window
                .root::<RootView>()
                .flatten()
                .context("the window has no root view")?;
            root.update(cx, |root, cx| root.open_spawn_target_list(window, cx));
            Ok(())
        }
        View::SpawnHosts => press("ctrl-shift-n", window, cx),
        View::Jump => press("ctrl-shift-p", window, cx),
        View::Dashboard | View::DashboardCompact => {
            let y = (DASHBOARD_ITEM + 0.5) * RAIL_ITEM_HEIGHT;
            #[expect(
                clippy::cast_possible_truncation,
                reason = "the rail's middle lies within a few hundred logical pixels"
            )]
            let at = ((RAIL_WIDTH / 2.0).round() as i32, y.round() as i32);
            win32::click(address, at)
        }
        View::Viewer => {
            let root = window
                .root::<RootView>()
                .flatten()
                .context("the window has no root view")?;
            let path = sample.to_string_lossy().into_owned();
            root.update(cx, |root, _| root.open_viewer(&path, Some(VIEWER_LINE)));
            Ok(())
        }
        View::ViewerMd => {
            let root = window
                .root::<RootView>()
                .flatten()
                .context("the window has no root view")?;
            let path = sample.to_string_lossy().into_owned();
            root.update(cx, |root, _| root.open_viewer(&path, None));
            Ok(())
        }
        View::Connections => open_picker(window, cx),
        View::ConnectionsScanError => open_picker_over(Arc::new(ShotPeersFailed), window, cx),
        View::Themes => drive_themes(step, window, cx),
        View::ThemesEdit => drive_themes_edit(step, window, cx),
        View::ConnectionsSsh => drive_connections_ssh(step, window, cx),
        View::Menu => open_menu(window, cx),
        View::Update => drive_update(step, address, window, cx),
        View::UpdateNotes => drive_update_notes(step, window, cx),
        View::SettingsDashboard => drive_settings_dashboard(step, address, window, cx),
        View::Chat if step == 2 => win32::wheel_down(address, CHAT_TAIL_POINT, CHAT_TAIL_NOTCHES),
        View::ChatCompact => open_chat(window, cx),
        View::Chat => {
            let root = window
                .root::<RootView>()
                .flatten()
                .context("the window has no root view")?;
            root.update(cx, |root, cx| {
                if step == 0 {
                    root.toggle_pane_view("p1", window, cx);
                } else {
                    root.toggle_stretchtext("s-petal", "a1", CHAT_OPEN_STRETCH, cx);
                    root.toggle_chat_card("s-petal", "t3", cx);
                    root.toggle_chat_tail("s-petal", "t3", 0, FOOTER_LOG, cx);
                }
            });
            Ok(())
        }
        View::Summary => {
            let root = window
                .root::<RootView>()
                .flatten()
                .context("the window has no root view")?;
            root.update(cx, |root, cx| {
                if step == 0 {
                    root.set_pane_view("p1", PaneView::Summary, window, cx);
                } else {
                    root.toggle_summary_line("s-petal", SUMMARY_OPEN_LINE, cx);
                }
            });
            Ok(())
        }
    }
}

/// Step `step` of the `panes-accent-chosen` shot: opens the jump palette,
/// then types the Periwinkle pane's label and presses Enter, which focuses
/// it through the tab's own focus. A click cannot do it: the cloaked window
/// never becomes active, so a press on a pane raises no focus event for the
/// tab to record.
fn drive_panes_accent_chosen(step: usize, window: &mut Window, cx: &mut App) -> Result<()> {
    if step == 0 {
        return press("ctrl-shift-p", window, cx);
    }
    for c in "periwinkle".chars() {
        press(&c.to_string(), window, cx)?;
    }
    press("enter", window, cx)
}

/// The stand-in installer the `update` shot's ready update holds open,
/// beside the layout file.
const SHOT_INSTALLER: &str = "rustling-tulip-0.99.0-setup.exe";

/// Where the `update` shot's wheel turns: inside Settings' General page.
const SETTINGS_PAGE_POINT: (i32, i32) = (560, 300);
/// A point on the `chat` shot's rows, under the Tail box, and how far the
/// wheel turns there to bring the box into view.
const CHAT_TAIL_POINT: (i32, i32) = (660, 155);
const CHAT_TAIL_NOTCHES: i16 = -3;
/// Enough wheel notches to reach a Settings page's end.
const SETTINGS_PAGE_NOTCHES: i16 = 40;

/// Step `step` of the `settings-claude` and `settings-hosts` shots: Settings
/// opens with the keyboard on its tab list, where Down walks the tabs to the
/// last, Claude; then (`settings-claude` only) the wheel takes the page to
/// its end, where the Hooks section is drawn whole.
fn drive_claude_page(step: usize, address: usize, window: &mut Window, cx: &mut App) -> Result<()> {
    if step == 1 {
        return win32::wheel_down(address, SETTINGS_PAGE_POINT, SETTINGS_PAGE_NOTCHES);
    }
    press("ctrl-,", window, cx)?;
    for _ in 0..CLAUDE_TAB_DOWNS {
        press("down", window, cx)?;
    }
    Ok(())
}

/// Step `step` of the `update` shot: Check now, which the shot's updater
/// answers with a ready 0.99.0, and Settings; then the wheel over the
/// General page down to its end, where the update rows are.
fn drive_update(step: usize, address: usize, window: &mut Window, cx: &mut App) -> Result<()> {
    if step == 0 {
        let root = window
            .root::<RootView>()
            .flatten()
            .context("the window has no root view")?;
        root.update(cx, RootView::check_for_updates_now);
        return press("ctrl-,", window, cx);
    }
    win32::wheel_down(address, SETTINGS_PAGE_POINT, SETTINGS_PAGE_NOTCHES)
}

/// Step `step` of the `update-notes` shot: Check now, which the shot's
/// updater answers with a ready 0.99.0, then the What's new window the
/// footer's button opens over it.
fn drive_update_notes(step: usize, window: &mut Window, cx: &mut App) -> Result<()> {
    let root = window
        .root::<RootView>()
        .flatten()
        .context("the window has no root view")?;
    if step == 0 {
        root.update(cx, RootView::check_for_updates_now);
        return Ok(());
    }
    root.update(cx, |root, cx| root.whats_new_ready(window, cx));
    Ok(())
}

/// Opens Settings and walks the tab list down to the Dashboard tab; once it
/// is drawn, Shift+Tab from the list wraps to its last stop, Test, and
/// Enter presses it; then the page scrolls down to the Test result.
fn drive_settings_dashboard(
    step: usize,
    address: usize,
    window: &mut Window,
    cx: &mut App,
) -> Result<()> {
    match step {
        0 => {
            press("ctrl-,", window, cx)?;
            for _ in 0..DASHBOARD_TAB_DOWNS {
                press("down", window, cx)?;
            }
            Ok(())
        }
        1 => {
            press("shift-tab", window, cx)?;
            press("enter", window, cx)
        }
        _ => win32::wheel_down(address, SETTINGS_PAGE_POINT, SETTINGS_PAGE_NOTCHES),
    }
}

/// The `settings-dashboard` shot's summarizer: a keyed provider chosen with
/// its key stored, 37 cents of a dollar spent today.
fn shot_summarizer_status() -> DaemonMessage {
    let provider =
        |id: &str, name: &str, needs_key: bool, privacy: &str| protocol::SummarizerProvider {
            id: id.to_owned(),
            name: name.to_owned(),
            needs_key,
            privacy: privacy.to_owned(),
        };
    DaemonMessage::SummarizerStatus {
        settings: protocol::SummarizerSettings {
            enabled: true,
            provider: "deepseek".to_owned(),
            key_source: protocol::SummarizerKeySource::Stored,
            daily_cap_usd: 1.0,
        },
        providers: vec![
            provider(
                "claude-haiku",
                "Claude Haiku (CLI)",
                false,
                "Runs through the claude CLI on your own account.",
            ),
            provider(
                "deepseek",
                "DeepSeek",
                true,
                "Sends session text to DeepSeek's API.",
            ),
        ],
        keys: vec![protocol::SummarizerKey {
            provider: "deepseek".to_owned(),
            state: protocol::KeyState::Stored,
        }],
        spend_today_usd: 0.37,
        cap_reached: false,
    }
}

/// The `settings-dashboard` shot's Test reply, answering `request_id`.
fn shot_summary_test(request_id: Option<String>) -> DaemonMessage {
    DaemonMessage::SummaryTest {
        result: protocol::SummaryTestResult {
            kind: None,
            headline: "Tests pass after the parser fix".to_owned(),
            notification: "The parser fix is in and all 214 tests pass.".to_owned(),
            source: protocol::SummarySource::Model,
        },
        latency_ms: 840,
        cost_usd: 0.0004,
        request_id,
    }
}

/// The notes every update shot's offer and changelog answer carry.
const SHOT_NOTES: &str = "## v0.99.0\n\n### Added\n\n- A What's new button on the update notices shows what changed since the version you had.\n\n### Fixed\n\n- Remote hosts keep their own cleanup and launch notices across a reconnect.";

/// An updater whose every check finds 0.99.0 ready at `installer` in the
/// `update` shot and nothing in any other, which starts nothing and has no
/// last result.
struct ShotUpdater {
    installer: PathBuf,
    /// Whether a check finds the update: only the `update` shot's does.
    offers: bool,
}

impl Updater for ShotUpdater {
    /// The shot's window never records a version or announces an install.
    fn is_active(&self) -> bool {
        false
    }

    fn check(&self, _current: &semver::Version) -> BoxFuture<'static, Result<Option<Ready>>> {
        if !self.offers {
            return futures::future::ready(Ok(None)).boxed();
        }
        let path = self.installer.clone();
        let ready = std::fs::File::open(&path)
            .with_context(|| format!("opening {}", path.display()))
            .map(|file| {
                Some(Ready {
                    version: semver::Version::new(0, 99, 0),
                    path,
                    file,
                    notes: SHOT_NOTES.to_owned(),
                })
            });
        futures::future::ready(ready).boxed()
    }

    fn start_installer(&self, _ready: &Ready) -> Result<()> {
        bail!("the shot starts no installer")
    }

    fn changelog(
        &self,
        _after: Option<semver::Version>,
        _through: semver::Version,
    ) -> BoxFuture<'static, Result<String>> {
        use futures::FutureExt as _;
        futures::future::ready(Ok(SHOT_NOTES.to_owned())).boxed()
    }

    fn read_last_result(&self) -> Option<LastResult> {
        None
    }
}

/// Opens the sessions panel's ⋯ menu, which lies over the repo rows, and
/// raises a sticky toast over the panes, so both floating surfaces show.
fn open_menu(window: &mut Window, cx: &mut App) -> Result<()> {
    let root = window
        .root::<RootView>()
        .flatten()
        .context("the window has no root view")?;
    root.update(cx, |root, cx| {
        root.push_toast_spec(
            ToastSpec {
                kind: ToastKind::Info,
                title: "Resumed 2 sessions after restart".to_owned(),
                detail: Some("1 stays in Recover".to_owned()),
                key: None,
                sticky: true,
                actions: Vec::new(),
            },
            cx,
        );
        root.open_more_menu(window, cx);
    });
    Ok(())
}

/// Step `step` of the `settings-accounts` shot: Settings on its Accounts
/// tab, then the wheel down the page to the auto-switch block. The tab list's
/// position depends on the host's feature list and the agents it offers, so
/// the tab is chosen by name rather than walked to with Down.
fn drive_settings_accounts(
    step: usize,
    address: usize,
    window: &mut Window,
    cx: &mut App,
) -> Result<()> {
    if step > 0 {
        return win32::wheel_down(address, SETTINGS_PAGE_POINT, SETTINGS_PAGE_NOTCHES);
    }
    press("ctrl-,", window, cx)?;
    let root = window
        .root::<RootView>()
        .flatten()
        .context("the window has no root view")?;
    root.update(cx, |root, cx| {
        root.show_settings_tab("Accounts", window, cx);
    });
    Ok(())
}

fn drive_settings_sounds(window: &mut Window, cx: &mut App) -> Result<()> {
    for key in ["ctrl-,", "down", "tab", "tab", "tab", "enter"] {
        press(key, window, cx)?;
    }
    Ok(())
}

fn press(key: &str, window: &mut Window, cx: &mut App) -> Result<()> {
    let keystroke =
        Keystroke::parse(key).map_err(|err| anyhow!("parsing the keystroke {key}: {err:?}"))?;
    window.dispatch_keystroke(keystroke, cx);
    Ok(())
}

/// An opener that opens nothing: the shot never leaves its window.
struct NoOpener;

impl Opener for NoOpener {
    fn url(&self, _url: &str) -> Result<(), String> {
        Err("the shot opens nothing".to_owned())
    }

    fn vscode(&self, _path: &Path, _line: u32, _column: u32) -> Result<(), OpenFailure> {
        Err(OpenFailure::Failed("the shot opens nothing".to_owned()))
    }

    fn vscode_folder(&self, _paths: &[PathBuf]) -> Result<(), OpenFailure> {
        Err(OpenFailure::Failed("the shot opens nothing".to_owned()))
    }

    fn vscode_workspace(&self, _path: &Path) -> Result<(), OpenFailure> {
        Err(OpenFailure::Failed("the shot opens nothing".to_owned()))
    }

    fn default_app(&self, _path: &Path) -> Result<(), String> {
        Err("the shot opens nothing".to_owned())
    }

    fn reveal(&self, _path: &Path) -> Result<(), String> {
        Err("the shot opens nothing".to_owned())
    }

    fn mapped_unc_hosts(&self) -> Vec<String> {
        Vec::new()
    }

    fn is_dangerous_type(&self, _extension: &str) -> bool {
        true
    }

    fn existing(&self, _reading: &Path) -> Option<Result<PathBuf, String>> {
        None
    }

    /// The shot's fake paths count as present, so its menus draw their open
    /// rows enabled.
    fn path_exists(&self, _path: &Path) -> bool {
        true
    }
}

/// A notifier that shows nothing and reports notifications on.
struct NoNotifier;

impl Notifier for NoNotifier {
    fn notify(&self, _title: &str, _body: &str, _silent: bool, _launch: Option<&str>) {}

    fn state(&self) -> NotifyState {
        NotifyState::On
    }

    fn suppressed(&self) -> bool {
        false
    }

    fn user_busy(&self) -> bool {
        false
    }
}

/// A login launch that is off and never touches the registry.
struct NoAutostart;

impl Autostart for NoAutostart {
    fn get(&self) -> Result<bool, String> {
        Ok(false)
    }

    fn set(&self, _enabled: bool) -> Result<(), String> {
        Ok(())
    }
}

/// What the fake daemon holds at connect.
struct Fixture {
    repos: DaemonMessage,
    workspaces: DaemonMessage,
    sessions: Vec<SessionSnapshot>,
    tabs: DaemonMessage,
    keep_awake: DaemonMessage,
    attention: DaemonMessage,
}

impl Fixture {
    /// Two repos, a workspace of two more (a workspace member's own
    /// sessions would file under Detached), seven sessions, and two tabs
    /// (the first a split); the diff view adds a diff tab third. Every
    /// fixed message is parsed here, so a change to the wire shape fails
    /// before the window opens rather than mid-shot.
    fn build(view: View, now: DateTime<Utc>) -> Result<Self> {
        let repos = parse_message(json!({
            "type": "repos",
            "repos": [
                { "id": TULIP, "name": TULIP, "path": TULIP_PATH, "default_branch": "main" },
                { "id": YAAT, "name": YAAT, "path": YAAT_PATH, "default_branch": "main" },
                { "id": DECK_SERVER, "name": DECK_SERVER, "path": DECK_SERVER_PATH, "default_branch": "main" },
                { "id": DECK_CLIENT, "name": DECK_CLIENT, "path": DECK_CLIENT_PATH, "default_branch": "main" },
            ],
        }))
        .context("the fixture's repos")?;
        let workspaces = parse_message(json!({
            "type": "workspaces",
            "workspaces": [
                { "id": WORKSPACE, "name": WORKSPACE, "member_repo_ids": [DECK_SERVER, DECK_CLIENT] },
            ],
        }))
        .context("the fixture's workspaces")?;
        let mut rows = session_rows();
        if matches!(view, View::PanesAccent | View::PanesAccentChosen) {
            rows.extend(accent_rows());
        }
        if matches!(view, View::Dashboard | View::DashboardCompact) {
            dashboard_rows(&mut rows);
        }
        if view == View::DashboardCompact {
            compact_rows(&mut rows);
        }
        let sessions = rows
            .into_iter()
            .map(|row| row.snapshot(now))
            .collect::<Result<Vec<_>>>()?;
        let mut tabs = vec![grid_tab("t-tulip", "tulip", &tulip_grid(view))];
        // The accent shots' four panes take p1 to p4, so their one tab is
        // the only one, and a second tab's panes cannot clash with them.
        if !matches!(view, View::PanesAccent | View::PanesAccentChosen) {
            tabs.push(grid_tab("t-yaat", "yaat", &yaat_grid()));
        }
        if view == View::Diff {
            tabs.push(json!({
                "id": DIFF_TAB,
                "name": DIFF_PATH,
                "content": {
                    "kind": "diff", "repo_id": TULIP, "path": DIFF_PATH,
                    "against": null, "worktree_path": PETAL_TREE,
                },
                "created_at": "2026-01-01T00:00:00Z",
            }));
        }
        check_reply_shapes(now)?;
        if matches!(view, View::Dashboard | View::DashboardCompact) {
            dashboard_facts(now)?;
            dashboard_tracks(now)?;
        }
        Ok(Self {
            repos,
            workspaces,
            sessions,
            tabs: parse_message(json!({ "type": "tabs", "tabs": tabs }))
                .context("the fixture's tabs")?,
            keep_awake: parse_message(json!({
                "type": "keep_awake_status", "enabled": true, "mode": "when_busy", "active": true,
            }))
            .context("the fixture's keep-awake status")?,
            attention: parse_message(json!({
                "type": "attention", "session_id": "s-recover", "reason": "awaiting_input",
            }))
            .context("the fixture's attention notice")?,
        })
    }
}

/// Parses `value` into the daemon message it names.
fn parse_message(value: Value) -> Result<DaemonMessage> {
    serde_json::from_value(value).context("a fake daemon message")
}

/// Builds and parses one sample of every reply the daemon builds per
/// request, so a shape change fails when the fixture is built rather than
/// mid-shot.
fn check_reply_shapes(now: DateTime<Utc>) -> Result<()> {
    for sample in [
        repo_status(TULIP, None),
        stashes(now, TULIP, None),
        commits(now, TULIP, 0, None),
        branches(TULIP),
    ] {
        parse_message(sample)?;
    }
    Ok(())
}

fn pane(id: &str, session: &str) -> GridNode {
    GridNode::Pane {
        pane_id: id.to_owned(),
        session_id: Some(session.to_owned()),
        host: None,
    }
}

/// The Petal session above its shell; the `multi-host` shot shows "box"'s
/// session in the shell's place, and the `main-narrow` shot sets the
/// Petal session, its shell and an empty pane side by side, each too
/// narrow for its header's buttons.
fn tulip_grid(view: View) -> GridNode {
    if matches!(view, View::PanesAccent | View::PanesAccentChosen) {
        return GridNode::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(GridNode::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(pane("p1", "s-shell")),
                second: Box::new(pane("p2", "s-rose")),
            }),
            second: Box::new(GridNode::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(pane("p3", "s-periwinkle")),
                second: Box::new(pane("p4", "s-lilac")),
            }),
        };
    }
    if view == View::MainNarrow {
        return GridNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.34,
            first: Box::new(pane("p1", "s-petal")),
            second: Box::new(GridNode::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(pane("p2", "s-shell")),
                second: Box::new(GridNode::Pane {
                    pane_id: "p5".to_owned(),
                    session_id: None,
                    host: None,
                }),
            }),
        };
    }
    let second = if view.on_hosts() {
        GridNode::Pane {
            pane_id: "p2".to_owned(),
            session_id: Some(BOX_SESSION.to_owned()),
            host: Some(SHOT_TAILNET_HOST.to_owned()),
        }
    } else {
        pane("p2", "s-shell")
    };
    GridNode::Split {
        direction: SplitDirection::Vertical,
        ratio: 0.58,
        first: Box::new(pane("p1", "s-petal")),
        second: Box::new(second),
    }
}

/// What the `multi-host` shot's "box" lists: one repo and its session.
struct RemoteFixture {
    repos: DaemonMessage,
    sessions: Vec<SessionSnapshot>,
}

impl RemoteFixture {
    fn build(now: DateTime<Utc>) -> Result<Self> {
        let repos = parse_message(json!({
            "type": "repos",
            "repos": [
                { "id": BOX_REPO, "name": BOX_REPO, "path": BOX_REPO_PATH, "default_branch": "main" },
            ],
        }))
        .context("the box fixture's repos")?;
        let session = Row {
            id: BOX_SESSION,
            label: "Relay soak test",
            status: "working",
            members: json!([member(BOX_REPO, "main", BOX_REPO_PATH)]),
            minutes: 3,
            extra: json!({}),
        }
        .snapshot(now)?;
        Ok(Self {
            repos,
            sessions: vec![session],
        })
    }
}

/// The METAR session beside the asking flight plan session.
fn yaat_grid() -> GridNode {
    GridNode::Split {
        direction: SplitDirection::Horizontal,
        ratio: 0.5,
        first: Box::new(pane("p3", "s-metar")),
        second: Box::new(pane("p4", "s-ask")),
    }
}

fn grid_tab(id: &str, name: &str, grid: &GridNode) -> Value {
    json!({
        "id": id,
        "name": name,
        "content": { "kind": "grid", "grid": grid },
        "created_at": "2026-01-01T00:00:00Z",
    })
}

fn member(repo: &str, branch: &str, tree: &str) -> Value {
    json!({ "repo_id": repo, "repo_name": repo, "branch": branch, "worktree_path": tree })
}

/// One session of the fixture: its status changed `minutes` ago, and
/// `extra` holds the snapshot fields past the common ones.
struct Row {
    id: &'static str,
    label: &'static str,
    status: &'static str,
    members: Value,
    minutes: i64,
    extra: Value,
}

impl Row {
    fn snapshot(self, now: DateTime<Utc>) -> Result<SessionSnapshot> {
        let since = now - chrono::Duration::minutes(self.minutes);
        let mut value = json!({
            "id": self.id,
            "label": self.label,
            "kind": "single",
            "members": self.members,
            "status": self.status,
            "status_since": since,
            "mode": "interactive",
            "started_at": since - chrono::Duration::minutes(40),
            "exit_code": null,
            "metrics": { "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0, "last_activity_at": since },
            "recent_actions": [],
            "agent": "claude",
        });
        if let (Some(fields), Value::Object(extra)) = (value.as_object_mut(), self.extra) {
            fields.extend(extra);
        }
        serde_json::from_value(value).with_context(|| format!("the session fixture {}", self.id))
    }
}

fn session_rows() -> Vec<Row> {
    vec![
        Row {
            id: "s-petal",
            label: "Petal footer polish",
            status: "working",
            members: json!([member(TULIP, PETAL_BRANCH, PETAL_TREE)]),
            minutes: 2,
            extra: json!({
                "has_per_session_worktree": true,
                "worktree_paths": [PETAL_TREE],
                "terminal_title": "Petal footer polish",
                "appearance": { "accent_color": "#a3be8c", "terminal_frame_color": "#a3be8c" },
            }),
        },
        Row {
            id: "s-shell",
            label: "pwsh",
            status: "idle",
            members: json!([member(TULIP, "main", TULIP_PATH)]),
            minutes: 4,
            extra: json!({ "mode": "plain_shell", "current_cwd": TULIP_PATH, "program_name": "pwsh" }),
        },
        Row {
            id: "s-recover",
            label: "Recover dialog keys",
            status: "awaiting_input",
            members: json!([member(TULIP, "feat/recover-keys", RECOVER_TREE)]),
            minutes: 6,
            extra: json!({ "has_per_session_worktree": true, "worktree_paths": [RECOVER_TREE] }),
        },
        Row {
            id: "s-review",
            label: "Review diff model",
            status: "idle",
            members: json!([member(TULIP, "main", TULIP_PATH)]),
            minutes: 1,
            extra: json!({}),
        },
        Row {
            id: "s-strip",
            label: "Flight strip layout",
            status: "spawning",
            members: json!([member(YAAT, "feat/strip-layout", YAAT_PATH)]),
            minutes: 0,
            extra: json!({}),
        },
        Row {
            id: "s-metar",
            label: "METAR parser fix",
            status: "idle",
            members: json!([member(YAAT, "main", YAAT_PATH)]),
            minutes: 25,
            extra: json!({ "background": true }),
        },
        Row {
            id: "s-ask",
            label: "Flight plan parser",
            status: "awaiting_input",
            members: json!([member(YAAT, "main", YAAT_PATH)]),
            minutes: 3,
            extra: json!({}),
        },
        Row {
            id: "s-proto",
            label: "Protocol v24 bump",
            status: "idle",
            members: json!([
                member(DECK_SERVER, "feat/proto-v24", DECK_SERVER_PATH),
                member(DECK_CLIENT, "feat/proto-v24", DECK_CLIENT_PATH),
            ]),
            minutes: 48,
            extra: json!({ "kind": "workspace", "workspace_id": WORKSPACE }),
        },
    ]
}

/// The three chosen accents the `panes-accent` and `panes-accent-chosen`
/// shots draw beside the Theme shell, each with the frame band the accent
/// picker stores with it.
fn accent_rows() -> Vec<Row> {
    let shell = |id: &'static str, label: &'static str, accent: &str| Row {
        id,
        label,
        status: "idle",
        members: json!([member(TULIP, "main", TULIP_PATH)]),
        minutes: 4,
        extra: json!({
            "mode": "plain_shell",
            "current_cwd": TULIP_PATH,
            "program_name": "pwsh",
            "user_label": label,
            "appearance": { "accent_color": accent, "terminal_frame_color": accent },
        }),
    };
    vec![
        shell("s-rose", "Rose", "#e7849b"),
        shell("s-periwinkle", "Periwinkle", "#8fb3f0"),
        shell("s-lilac", "Lilac", "#c9b8ff"),
    ]
}

/// Records `err` as the failure that stopped the fake daemon.
fn record_failure(failure: &Mutex<Option<anyhow::Error>>, err: anyhow::Error) {
    *failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(err);
}

/// Fails with the fake daemon's failure, if it stopped.
fn check_daemon(failure: &Mutex<Option<anyhow::Error>>) -> Result<()> {
    match failure
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
    {
        Some(err) => Err(err).context("the fake daemon stopped"),
        None => Ok(()),
    }
}

/// The daemon side of the shot: greets the view with the fixture, then
/// answers the requests the views make.
struct FakeDaemon {
    events: UnboundedSender<NetEvent>,
    /// The view the shot shows, which picks the chat's screen.
    view: View,
    now: DateTime<Utc>,
    failure: Arc<Mutex<Option<anyhow::Error>>>,
}

impl FakeDaemon {
    fn run(&self, fixture: &Fixture, commands: UnboundedReceiver<NetCommand>) {
        if let Err(err) = self.greet(fixture) {
            self.fail(err);
            return;
        }
        self.serve(commands);
    }

    /// As [`Self::run`], for a remote host on `target`: no handshake file,
    /// and only its repos and sessions.
    fn run_remote(
        &self,
        fixture: &RemoteFixture,
        target: &Target,
        commands: UnboundedReceiver<NetCommand>,
    ) {
        if let Err(err) = self.greet_remote(fixture, target) {
            self.fail(err);
            return;
        }
        self.serve(commands);
    }

    /// Answers the window's requests until it stops sending.
    fn serve(&self, mut commands: UnboundedReceiver<NetCommand>) {
        while let Some(command) = futures::executor::block_on(commands.next()) {
            let NetCommand::Send(msg) = command else {
                continue;
            };
            let sent = self
                .answer(&msg)
                .and_then(|replies| replies.into_iter().try_for_each(|reply| self.send(reply)));
            if let Err(err) = sent {
                self.fail(err);
                return;
            }
        }
    }

    /// Records the error that stopped the daemon, so the shot fails with it
    /// instead of writing a PNG of a half-filled client.
    fn fail(&self, err: anyhow::Error) {
        tracing::error!("the fake daemon stopped: {err:#}");
        record_failure(&self.failure, err);
    }

    fn event(&self, event: NetEvent) -> Result<()> {
        self.events
            .unbounded_send(event)
            .map_err(|_| anyhow!("the window stopped listening"))
    }

    fn send(&self, msg: DaemonMessage) -> Result<()> {
        self.event(NetEvent::Message(Box::new(msg)))
    }

    /// The connection, the handshake and the lists a daemon sends first,
    /// then a finished review turn (unseen) and a session asking for input.
    fn greet(&self, fixture: &Fixture) -> Result<()> {
        let version = NATIVE_PROTOCOL_VERSIONS.first().copied().unwrap_or(1);
        let mut conn = Connection::new();
        conn.on_connecting();
        conn.on_socket_open(FAKE_PORT);
        let mut features = vec![
            protocol::FEATURE_AGENT_COMMANDS.to_owned(),
            protocol::FEATURE_FILE_TAIL.to_owned(),
            protocol::FEATURE_DEFAULT_SHELL.to_owned(),
            protocol::FEATURE_AGENT_USER_HOOKS.to_owned(),
            protocol::FEATURE_ACCOUNTS.to_owned(),
        ];
        // Only the gates shot lists the gate feature, so the other views keep
        // no Gates item or footer pill.
        if self.view == View::Gates {
            features.push(protocol::FEATURE_GATE_STATE.to_owned());
        }
        conn.on_welcome(version, features.clone(), None);
        self.event(NetEvent::State(conn))?;
        self.event(NetEvent::Handshake(HandshakeInfo {
            port: FAKE_PORT,
            pid: FAKE_PID,
            protocol_version: version,
            build_version: env!("CARGO_PKG_VERSION").to_owned(),
        }))?;
        self.send(DaemonMessage::Welcome {
            protocol_version: version,
            supported_versions: vec![version],
            features,
            daemon_id: None,
            origin: None,
        })?;
        self.send(DaemonMessage::Agents {
            agents: agent_registry::descriptors(),
        })?;
        self.send(DaemonMessage::AgentSettings {
            settings: std::collections::BTreeMap::new(),
        })?;
        self.send(fixture.repos.clone())?;
        self.send(fixture.workspaces.clone())?;
        self.send(DaemonMessage::Sessions {
            sessions: fixture.sessions.clone(),
        })?;
        self.send(fixture.tabs.clone())?;
        self.send(fixture.keep_awake.clone())?;
        self.send(DaemonMessage::DefaultShell {
            id: "pwsh".to_owned(),
            command: None,
        })?;
        if self.view == View::SettingsDashboard {
            self.send(shot_summarizer_status())?;
        }
        if self.view == View::Gates {
            self.send(DaemonMessage::GateState {
                state: Some(Box::new(shot_gate_state(self.now))),
            })?;
        }
        self.send(DaemonMessage::AgentCommands {
            commands: vec![AgentCommandEntry {
                agent: protocol::AgentId("claude".to_owned()),
                program: "claude".to_owned(),
                args: "--verbose".to_owned(),
                program_env: None,
            }],
        })?;
        self.finish_review_turn(fixture)?;
        self.send(fixture.attention.clone())
    }

    /// What a remote host on `target` sends first: its open connection,
    /// `Welcome`, its agents, then its repos and sessions. A tailnet host
    /// reports no handshake file.
    fn greet_remote(&self, fixture: &RemoteFixture, target: &Target) -> Result<()> {
        let version = NATIVE_PROTOCOL_VERSIONS.first().copied().unwrap_or(1);
        let mut conn = Connection::for_target(target.clone());
        conn.on_connecting();
        conn.on_socket_open(8787);
        conn.on_welcome(version, Vec::new(), None);
        self.event(NetEvent::State(conn))?;
        self.send(DaemonMessage::Welcome {
            protocol_version: version,
            supported_versions: vec![version],
            features: Vec::new(),
            daemon_id: None,
            origin: None,
        })?;
        self.send(DaemonMessage::Agents {
            agents: agent_registry::descriptors(),
        })?;
        self.send(DaemonMessage::AgentSettings {
            settings: std::collections::BTreeMap::new(),
        })?;
        self.send(fixture.repos.clone())?;
        self.send(DaemonMessage::Sessions {
            sessions: fixture.sessions.clone(),
        })
    }

    /// The review session works, then goes idle while nobody looks at it.
    fn finish_review_turn(&self, fixture: &Fixture) -> Result<()> {
        let review = fixture
            .sessions
            .iter()
            .find(|session| session.id == "s-review")
            .context("the fixture has no review session")?;
        for status in [
            protocol::SessionStatus::Working,
            protocol::SessionStatus::Idle,
        ] {
            let mut session = review.clone();
            session.status = status;
            session.status_since = Some(self.now);
            self.send(DaemonMessage::SessionUpdated {
                session,
                request_id: None,
            })?;
        }
        Ok(())
    }

    /// The replies to `msg`; requests the views do not need go unanswered.
    fn answer(&self, msg: &ClientMessage) -> Result<Vec<DaemonMessage>> {
        let reply = match msg {
            ClientMessage::LoadScrollback {
                session_id,
                request_id,
            } => DaemonMessage::Scrollback {
                session_id: session_id.clone(),
                data_b64: B64.encode(screen_for(session_id)),
                truncated: false,
                request_id: request_id.clone(),
                forwarder_restarted: true,
            },
            ClientMessage::RepoStatus {
                repo_id,
                worktree_path,
                ..
            } => parse_message(repo_status(repo_id, worktree_path.as_deref()))?,
            ClientMessage::ListStashes {
                repo_id,
                worktree_path,
                ..
            } => parse_message(stashes(self.now, repo_id, worktree_path.as_deref()))?,
            ClientMessage::ListCommits {
                repo_id,
                offset,
                worktree_path,
                ..
            } => parse_message(commits(
                self.now,
                repo_id,
                *offset,
                worktree_path.as_deref(),
            ))?,
            ClientMessage::LoadChat {
                session_id,
                request_id,
            } => {
                let screen = match self.view {
                    View::Summary => permission_screen(),
                    View::Chat => question_screen(),
                    _ => ChatScreen {
                        composer: ComposerState::Working,
                        prompt: None,
                    },
                };
                return Ok(vec![
                    chat_reply(session_id, request_id.clone(), screen),
                    chat_work(self.now, session_id),
                ]);
            }
            ClientMessage::LoadSummary {
                session_id,
                request_id,
            } => summary_reply(session_id, request_id.clone()),
            ClientMessage::TailFile {
                session_id, path, ..
            } => DaemonMessage::FileTail {
                session_id: session_id.clone(),
                path: path.clone(),
                data_b64: B64.encode(FOOTER_LOG_TEXT),
                offset: 0,
                reset: true,
            },
            ClientMessage::GetFileSnapshot { .. } => file_snapshot(msg)?,
            ClientMessage::ListBranches { repo_id } => parse_message(branches(repo_id))?,
            ClientMessage::OpenViewerTab { id, path } => return viewer_opened(id, path),
            ClientMessage::OpenDashboardTab { id } => return dashboard_opened(id),
            ClientMessage::WatchDashboard { on: true } => {
                return Ok(vec![
                    dashboard_facts(self.now)?,
                    dashboard_tracks(self.now)?,
                ]);
            }
            ClientMessage::TestSummary { request_id } => shot_summary_test(request_id.clone()),
            ClientMessage::ListShellChoices => shot_shell_choices(),
            ClientMessage::GetAgentUserHooks { agent, request_id } => {
                DaemonMessage::AgentUserHooks {
                    agent: agent.clone(),
                    state: protocol::UserHooksWire::On,
                    path: Some("C:\\Users\\you\\.claude\\settings.json".to_owned()),
                    error: None,
                    request_id: request_id.clone(),
                }
            }
            ClientMessage::ListAccounts { request_id } => {
                shot_accounts(self.now, request_id.clone())
            }
            ClientMessage::GateHistory { request_id } => {
                shot_gate_history(self.now, request_id.clone())
            }
            ClientMessage::SuggestBranchName { target, request_id } => {
                DaemonMessage::BranchNameSuggestion {
                    target: target.clone(),
                    name: "feat/sidebar-density".to_owned(),
                    request_id: request_id.clone(),
                }
            }
            _ => return Ok(Vec::new()),
        };
        Ok(vec![reply])
    }
}

/// The fake daemon's accounts at `now`, the mock's five: main in use, ops
/// and lab ready (lab's five-hour window near its limit), spare out of
/// rotation and fetched long ago, client signed out.
fn shot_accounts(now: DateTime<Utc>, request_id: Option<String>) -> DaemonMessage {
    let window = |pct: f64, resets_in_minutes: Option<i64>| protocol::AccountUsageWindow {
        pct,
        resets_at: resets_in_minutes.map(|minutes| now + chrono::Duration::minutes(minutes)),
    };
    let account = |number: u32, alias: &str, five: f64, seven: f64| protocol::AccountSnapshot {
        number,
        email: format!("{alias}@example.com"),
        alias: Some(alias.to_owned()),
        organization_name: None,
        kind: protocol::AccountKind::Oauth,
        active: false,
        disabled: false,
        status: protocol::AccountUsageStatus::Ok,
        five_hour: Some(window(five, Some(140))),
        seven_day: Some(window(seven, Some(60 * 45))),
        model_limits: Vec::new(),
        extra_usage: None,
        fetched_at: Some(now - chrono::Duration::seconds(12)),
        error: None,
    };
    let main = protocol::AccountSnapshot {
        active: true,
        model_limits: vec![protocol::AccountModelLimit {
            model: "Fable".to_owned(),
            pct: 37.0,
            resets_at: Some(now + chrono::Duration::minutes(60 * 45)),
        }],
        ..account(1, "main", 34.0, 41.0)
    };
    let lab = protocol::AccountSnapshot {
        five_hour: Some(window(86.0, Some(55))),
        ..account(3, "lab", 86.0, 63.0)
    };
    let spare = protocol::AccountSnapshot {
        disabled: true,
        five_hour: Some(window(3.0, None)),
        fetched_at: Some(now - chrono::Duration::minutes(42)),
        ..account(4, "spare", 3.0, 9.0)
    };
    let client = protocol::AccountSnapshot {
        status: protocol::AccountUsageStatus::ReloginRequired,
        ..account(5, "client", 0.0, 0.0)
    };
    DaemonMessage::Accounts {
        accounts: vec![main, account(2, "ops", 12.0, 28.0), lab, spare, client],
        auto: Some(shot_auto(now)),
        request_id,
    }
}

/// The fake daemon's auto-switch engine at `now`, running on the stock
/// rules with a fleet verdict that runs dry in two days.
fn shot_auto(now: DateTime<Utc>) -> protocol::AutoSwitchState {
    protocol::AutoSwitchState {
        enabled: true,
        threshold_pct: 85.0,
        hysteresis_pct: 5.0,
        cooldown_secs: 300,
        strategy: protocol::AutoSwitchStrategy::Best,
        rate_window_minutes: Some(60),
        paused_reason: None,
        warm_up_done: true,
        last_decision: None,
        fleet_verdict: Some(protocol::FleetVerdict {
            window: protocol::FleetWindow::SevenDay,
            members: 5,
            accounts: 5,
            remaining: 1.8,
            rate: 0.4,
            rate_window: protocol::FleetWindow::FiveHour,
            runs_dry: true,
            blocked_at: Some(now + chrono::Duration::days(2)),
            resumes_at: Some(now + chrono::Duration::days(3)),
            first_reset: Some(now + chrono::Duration::days(1)),
        }),
    }
}

/// The fake daemon's `GateHistory` reply to `request_id` at `now`: four
/// finished runs (an ok `cargo nextest run --workspace` in rustling-tulip,
/// a failed `dotnet test` in yaat, a timed-out run in claude-config and an
/// ok run that fell back from the remote pool), one running run, and five
/// seven-day summary rows.
#[expect(
    clippy::too_many_lines,
    reason = "one literal fixture, read top to bottom like the mock's state"
)]
#[expect(
    clippy::cast_possible_truncation,
    reason = "a fixture's held seconds are far inside i64"
)]
fn shot_gate_history(now: DateTime<Utc>, request_id: Option<String>) -> DaemonMessage {
    use protocol::{GateCommandSummary, GateHistoryRun, GateRunState};
    let run = |pid: u32,
               command: &str,
               repo: &str,
               slot: Option<u32>,
               state: GateRunState,
               status: Option<i64>,
               ended: i64,
               held: f64| {
        let started = now
            - chrono::Duration::minutes(ended)
            - chrono::Duration::milliseconds((held * 1000.0) as i64);
        GateHistoryRun {
            command: command.to_owned(),
            cwd: Some(format!(r"X:\dev\{repo}")),
            repo: Some(repo.to_owned()),
            kind: "heavy".to_owned(),
            slot,
            remote: false,
            host: None,
            fallback: None,
            reason: None,
            pid,
            process_start: Some(started - chrono::Duration::seconds(3)),
            parent_pid: None,
            state,
            running: false,
            queued: Some(started - chrono::Duration::seconds(3)),
            started: Some(started),
            waited_seconds: Some(3.0),
            held_seconds: Some(held),
            status,
            verdict: None,
            wall_seconds: None,
            adjusted_seconds: None,
            abandoned: Vec::new(),
            remote_take: None,
            cold: None,
            critical_pid: None,
        }
    };
    let mut running = run(
        4100,
        r"C:\Users\me\.cargo\bin\cargo.exe nextest run -p rustling-tulip-native",
        "rustling-tulip",
        Some(0),
        GateRunState::Running,
        None,
        0,
        360.0,
    );
    running.running = true;
    running.held_seconds = None;
    let mut fell_back = run(
        3980,
        "cargo build --release",
        "rustling-tulip",
        Some(2),
        GateRunState::Ok,
        Some(0),
        41,
        203.0,
    );
    fell_back.fallback = Some("full".to_owned());
    let runs = vec![
        run(
            3990,
            "cargo nextest run --workspace",
            "rustling-tulip",
            Some(1),
            GateRunState::Ok,
            Some(0),
            4,
            108.0,
        ),
        run(
            3985,
            "dotnet test",
            "yaat",
            Some(0),
            GateRunState::Failed,
            Some(1),
            3,
            42.0,
        ),
        run(
            3982,
            "pwsh -NoProfile -File tools/check.ps1",
            "claude-config",
            None,
            GateRunState::Failed,
            Some(124),
            26,
            600.0,
        ),
        fell_back,
    ];
    let summary = |command: &str, repo: &str, count: u32, p90: f64| GateCommandSummary {
        command: command.to_owned(),
        repo: Some(repo.to_owned()),
        remote: false,
        count,
        p50_seconds: p90 * 0.7,
        p90_seconds: p90,
        max_seconds: p90 * 1.4,
    };
    DaemonMessage::GateHistory {
        request_id,
        runs,
        open: vec![running],
        summary: vec![
            summary("cargo nextest run --workspace", "rustling-tulip", 42, 118.0),
            summary("cargo clippy --workspace", "rustling-tulip", 31, 64.0),
            summary("dotnet test", "yaat", 18, 252.0),
            summary("cargo build --release", "rustling-tulip", 9, 3780.0),
            summary("pwsh tools/check.ps1", "claude-config", 6, 37.0),
            // The live gates' rows, past the four the week block shows.
            summary(
                r"C:\Users\me\.cargo\bin\cargo.exe nextest run -p rustling-tulip-native",
                "rustling-tulip",
                5,
                343.0,
            ),
            summary("cargo clippy --workspace -- -D warnings", "gate", 5, 257.0),
            summary("cargo fmt --all --check", "rustling-tulip", 5, 129.0),
            summary("cargo nextest run --workspace", "ops-tower", 5, 343.0),
            GateCommandSummary {
                remote: true,
                ..summary("dotnet test yaat.slnx", "yaat", 5, 429.0)
            },
        ],
    }
}

/// The fake daemon's gate state at `now`, after lane A's busy state: heavy
/// 4 with two busy slots, a held one and a free one with a stale claim, and
/// a gate waiting for it; light 2 with one busy; one queue ticket; a remote
/// pool of 2 with one busy.
#[expect(
    clippy::too_many_lines,
    reason = "one literal fixture, read top to bottom like the mock's state"
)]
fn shot_gate_state(now: DateTime<Utc>) -> protocol::GateState {
    use protocol::{
        GateClaim, GatePool, GateQueueTicket, GateRemotePool, GateSlot, GateSlotKind,
        GateSlotState, GateWaiting,
    };
    let slot = |index: u32, state: GateSlotState| GateSlot {
        index,
        name: format!("gate-slot-{index}"),
        state,
        claim: None,
        stale: None,
        processes: 0,
        last_line: None,
        critical: false,
        extra: Some(false),
    };
    let claim = |pid: u32, minutes: i64, cwd: &str, command: &str| GateClaim {
        pid: Some(pid),
        started: Some(now - chrono::Duration::minutes(minutes)),
        cwd: Some(cwd.to_owned()),
        command: Some(command.to_owned()),
        log: Some(format!(r"C:\Users\me\AppData\Local\gate\logs\{pid}.log")),
        timeout_seconds: Some(1800.0),
        command_key: None,
        repo: None,
    };
    // A busy slot's claim carries the key and repo of its summary row in
    // `shot_gate_history`, so its row shows the usual time.
    let busy = |index: u32,
                pid: u32,
                minutes: i64,
                cwd: &str,
                command: &str,
                line: &str,
                (repo, key): (&str, &str)| GateSlot {
        claim: Some(GateClaim {
            command_key: Some(key.to_owned()),
            repo: Some(repo.to_owned()),
            ..claim(pid, minutes, cwd, command)
        }),
        processes: 4,
        last_line: Some(line.to_owned()),
        ..slot(index, GateSlotState::Busy)
    };
    let waiting_since = now - chrono::Duration::minutes(1);
    let heavy = GatePool {
        kind: GateSlotKind::Heavy,
        count: 4,
        default: 4,
        set: Some(4),
        problem: None,
        slots: vec![
            busy(
                0,
                4100,
                6,
                r"X:\dev\rustling-tulip.wt\rt-1001",
                r"C:\Users\me\.cargo\bin\cargo.exe nextest run -p rustling-tulip-native",
                "PASS [ 0.071s] ui_gates::the_badge_counts_busy_and_held_slots",
                (
                    "rustling-tulip",
                    r"C:\Users\me\.cargo\bin\cargo.exe nextest run -p rustling-tulip-native",
                ),
            ),
            busy(
                1,
                4102,
                2,
                r"X:\dev\gate.wt\g-41",
                "cargo clippy --workspace -- -D warnings",
                "Checking gate-state v0.4.0",
                ("gate", "cargo clippy --workspace -- -D warnings"),
            ),
            slot(2, GateSlotState::Held),
            GateSlot {
                stale: Some(claim(3920, 40, r"X:\dev\repo", "cargo build")),
                ..slot(3, GateSlotState::Free)
            },
        ],
        waiting: vec![GateWaiting {
            pid: 4210,
            since: Some(waiting_since),
            cwd: Some(r"X:\dev\ops-tower".to_owned()),
            command_line: "cargo nextest run --workspace".to_owned(),
            command_key: Some("cargo nextest run --workspace".to_owned()),
            repo: Some("ops-tower".to_owned()),
        }],
    };
    let light = GatePool {
        kind: GateSlotKind::Light,
        count: 2,
        default: 2,
        set: None,
        problem: None,
        slots: vec![
            busy(
                0,
                4120,
                1,
                r"X:\dev\rustling-tulip",
                "cargo fmt --all --check",
                "Diff in apps/native/src/gates_view.rs",
                ("rustling-tulip", "cargo fmt --all --check"),
            ),
            slot(1, GateSlotState::Free),
        ],
        waiting: Vec::new(),
    };
    protocol::GateState {
        now: Some(now),
        machine: "LEFTOS-DESK".to_owned(),
        processors: 16,
        critical: None,
        queue: vec![GateQueueTicket {
            kind: "heavy".to_owned(),
            holds: false,
            remote: false,
            pool: None,
            pinned: false,
            pid: 4210,
            arrived: Some(waiting_since),
            age_seconds: 60.0,
            cwd: Some(r"X:\dev\ops-tower".to_owned()),
            command_line: Some("cargo nextest run --workspace".to_owned()),
            reason: None,
            expected: None,
            command_key: Some("cargo nextest run --workspace".to_owned()),
            repo: Some("ops-tower".to_owned()),
        }],
        pools: vec![heavy, light],
        remote: Some(GateRemotePool {
            count: 2,
            problem: None,
            slots: vec![
                busy(
                    0,
                    5100,
                    3,
                    r"X:\dev\yaat",
                    "dotnet test yaat.slnx",
                    "Passed!  - Failed: 0, Passed: 412",
                    ("yaat", "dotnet test yaat.slnx"),
                ),
                slot(1, GateSlotState::Free),
            ],
        }),
        builders: Vec::new(),
        covered: Vec::new(),
    }
}

/// Shows the Gates panel.
fn show_gates(window: &mut Window, cx: &mut App) -> Result<()> {
    let root = window
        .root::<RootView>()
        .flatten()
        .context("the window has no root view")?;
    root.update(cx, |root, cx| root.show_gates(window, cx));
    Ok(())
}

/// Opens the Open a shell dialog on This PC with [`SHOT_SHELL_FOLDER`] in
/// its folder field, the caret at its end.
fn open_shell(window: &mut Window, cx: &mut App) -> Result<()> {
    let root = window
        .root::<RootView>()
        .flatten()
        .context("the window has no root view")?;
    root.update(cx, |root, cx| {
        root.open_shell_dialog_with_folder(SHOT_SHELL_FOLDER, window, cx);
    });
    Ok(())
}

/// Opens the accounts popover from the footer's chip.
fn open_accounts(window: &mut Window, cx: &mut App) -> Result<()> {
    let root = window
        .root::<RootView>()
        .flatten()
        .context("the window has no root view")?;
    root.update(cx, |root, cx| {
        root.toggle_accounts();
        cx.notify();
    });
    Ok(())
}

/// The fake daemon's default-shell choices, as a Windows host with
/// PowerShell 7 and Git Bash but no WSL lists them.
fn shot_shell_choices() -> DaemonMessage {
    let choice = |id: &str, label: &str, available: bool| protocol::ShellChoice {
        id: id.to_owned(),
        label: label.to_owned(),
        available,
        path: None,
        hosts_agents: true,
    };
    DaemonMessage::ShellChoices {
        choices: vec![
            choice("auto", "Automatic", true),
            choice("pwsh", "PowerShell 7", true),
            choice("powershell", "Windows PowerShell", true),
            choice("cmd", "Command Prompt", true),
            choice("git-bash", "Git Bash", true),
            choice("wsl", "WSL", false),
            choice("custom", "Custom", true),
        ],
    }
}

/// One finished or running tool call of the `chat` shot.
fn chat_tool(
    id: &str,
    name: &str,
    kind: ToolKind,
    summary: &str,
    result: Option<&str>,
) -> ChatEntry {
    ChatEntry::Tool {
        id: id.to_owned(),
        name: name.to_owned(),
        summary: summary.to_owned(),
        input: format!("{{\"file_path\": \"{DIFF_PATH}\"}}"),
        result: result.map(|text| ToolOutcome {
            text: text.to_owned(),
            is_error: false,
        }),
        parent_id: None,
        tail_paths: Vec::new(),
        kind,
    }
}

/// The `chat` shot's run of read, search and run calls, a thinking row
/// between two of them, which Compact draws as one line.
fn footer_run() -> Vec<ChatEntry> {
    vec![
        chat_tool(
            "t4",
            "Read",
            ToolKind::Read,
            "apps/native/tests/ui/ui_footer.rs",
            Some("214 lines"),
        ),
        ChatEntry::Thinking {
            id: "k2".to_owned(),
            secs: Some(4),
        },
        chat_tool(
            "t5",
            "Grep",
            ToolKind::Search,
            "fn pill_gap",
            Some("3 matches"),
        ),
        chat_tool(
            "t6",
            "Glob",
            ToolKind::Search,
            "**/footer*.rs",
            Some("2 files"),
        ),
        chat_tool(
            "t7",
            "Bash",
            ToolKind::Run,
            "git status --short",
            Some("M footer.rs"),
        ),
    ]
}

/// The `chat` shot's conversation: a row of each kind, a tool card done and
/// one still running, a run of read, search and run calls with a thinking
/// row between two (one Compact line), a message not yet in the
/// transcript, and a live answer, under `screen`.
fn chat_reply(session_id: &str, request_id: Option<String>, screen: ChatScreen) -> DaemonMessage {
    let tool = chat_tool;
    let answer = "The footer now keeps **8 px** between its \
                  {{pills|pills, the {{status chips|status, branch and model chips}} along \
                  its bottom edge}}, so it {{reads calmer|reads calmer at a glance}}:\n\n\
                  - `pill()` takes the gap from its caller\n\
                  - the colours come from `palette.rs`\n\n\
                  ```rust\nlet gap = px(8.0);\n```";
    let mut entries = vec![
        ChatEntry::System {
            id: "cleared:c1".to_owned(),
            text: "Conversation cleared".to_owned(),
        },
        ChatEntry::User {
            id: "u1".to_owned(),
            text: "Tighten the footer spacing and check the pill colours.".to_owned(),
            pending: false,
        },
        ChatEntry::Thinking {
            id: "k1".to_owned(),
            secs: Some(14),
        },
        tool("t1", "Read", ToolKind::Read, DIFF_PATH, Some("412 lines")),
        ChatEntry::Receipt {
            id: "r1".to_owned(),
            question: "Allow Edit on footer.rs?".to_owned(),
            answer: "Yes".to_owned(),
        },
        tool(
            "t2",
            "Edit",
            ToolKind::Other,
            "footer.rs: the pill gap 6 px to 8 px",
            Some("ok"),
        ),
        ChatEntry::Answer {
            id: "a1".to_owned(),
            text: answer.to_owned(),
            live: false,
        },
        ChatEntry::TurnEnd {
            id: "e1".to_owned(),
            duration_ms: Some(72_000),
        },
        ChatEntry::User {
            id: "u2".to_owned(),
            text: "Now run the footer specs.".to_owned(),
            pending: true,
        },
        ChatEntry::Tool {
            id: "t3".to_owned(),
            name: "Bash".to_owned(),
            summary: FOOTER_SPECS.to_owned(),
            input: format!("{:#}", serde_json::json!({ "command": FOOTER_SPECS })),
            result: None,
            parent_id: None,
            tail_paths: vec![FOOTER_LOG.to_owned()],
            kind: ToolKind::Run,
        },
    ];
    entries.extend(footer_run());
    entries.push(ChatEntry::Answer {
        id: "live:m2".to_owned(),
        text: "Running the footer specs now.\nThe first two pass".to_owned(),
        live: true,
    });
    DaemonMessage::Chat {
        session_id: session_id.to_owned(),
        entries,
        hidden: 0,
        screen,
        request_id,
    }
}

/// The `summary` shot's screen: a Permission dialog for the footer specs'
/// command, pinned over the lines.
fn permission_screen() -> ChatScreen {
    let options = [(1, "Yes"), (2, "Yes, and don't ask again"), (3, "No")]
        .into_iter()
        .map(|(number, label)| ChatOption {
            number,
            label: label.to_owned(),
            description: None,
            selected: false,
        })
        .collect();
    ChatScreen {
        composer: ComposerState::Dialog,
        prompt: Some(ChatPrompt {
            id: "perm-1".to_owned(),
            kind: ChatPromptKind::Permission,
            subject: "cargo nextest run footer".to_owned(),
            body: "Do you want to run this command?".to_owned(),
            options,
            questions: Vec::new(),
            can_amend: true,
            always_allow_tip: None,
            multi: false,
            cursor: Some(1),
        }),
    }
}

/// The `chat` shot's screen: the second of two questions, its first
/// answered, its question wrapped, two of its options described, and its
/// "Type something" option with the card's empty box beside it.
fn question_screen() -> ChatScreen {
    let options = [
        (
            1,
            "Integer helpers",
            Some("add, sub and div: the bodies that changed most since the last release"),
        ),
        (
            2,
            "Float helpers",
            Some("Moved files only; their bodies are as they were"),
        ),
        (3, "Both", None),
        (4, "Type something", None),
    ]
    .into_iter()
    .map(|(number, label, description)| ChatOption {
        number,
        label: label.to_owned(),
        description: description.map(str::to_owned),
        selected: false,
    })
    .collect();
    let questions = [("Scope", true, false), ("Tests", false, true)]
        .into_iter()
        .map(|(label, answered, current)| ChatQuestionTab {
            label: label.to_owned(),
            answered,
            current,
        })
        .collect();
    ChatScreen {
        composer: ComposerState::Dialog,
        prompt: Some(ChatPrompt {
            id: "ask-1".to_owned(),
            kind: ChatPromptKind::Question,
            subject: "Which helpers should the new footer specs cover first, given that the release goes out \
                to the beta channel tomorrow morning?"
                .to_owned(),
            body: String::new(),
            options,
            questions,
            can_amend: false,
            always_allow_tip: None,
            multi: false,
            cursor: Some(1),
        }),
    }
}

/// The `summary` shot's lines over [`chat_reply`]'s entries: the ask, the
/// edit (drawn open) and the answer with the specs it started.
fn summary_reply(session_id: &str, request_id: Option<String>) -> DaemonMessage {
    let line = |id: &str, from: &str, to: &str, text: &str| StretchLine {
        id: id.to_owned(),
        from_entry: from.to_owned(),
        to_entry: to.to_owned(),
        started_at: None,
        ended_at: None,
        text: text.to_owned(),
        source: SummarySource::Model,
    };
    DaemonMessage::Summary {
        session_id: session_id.to_owned(),
        lines: vec![
            line(
                "u1",
                "u1",
                "k1",
                "Asked to tighten the footer spacing and check the pill colours",
            ),
            line(
                SUMMARY_OPEN_LINE,
                "t1",
                "t2",
                "Read footer.rs and, once allowed, widened the pill gap from 6 px to 8 px",
            ),
            line(
                "a1",
                "a1",
                "live:m2",
                "Explained the new gap and started the footer specs",
            ),
        ],
        request_id,
    }
}

/// The `chat` shot's running work as of `now`: two subagents, the footer
/// specs' shell and a wakeup 4 minutes off.
fn chat_work(now: DateTime<Utc>, session_id: &str) -> DaemonMessage {
    let sleeper = |id: &str, description: &str| WorkSubagent {
        id: id.to_owned(),
        description: description.to_owned(),
        agent_type: Some("general-purpose".to_owned()),
        since: Some(now - chrono::Duration::minutes(2)),
        from_screen: false,
    };
    DaemonMessage::ChatWork {
        session_id: session_id.to_owned(),
        subagents: vec![sleeper("a1", "Sleeper A"), sleeper("a2", "Sleeper B")],
        shells: vec![WorkShell {
            id: "b1".to_owned(),
            description: "Run the footer specs".to_owned(),
            command: Some("cargo nextest run footer".to_owned()),
            since: Some(now - chrono::Duration::minutes(3)),
            from_screen: false,
        }],
        wakeup_at: Some(now + chrono::Duration::minutes(4)),
    }
}

/// The stashes a repo reports, as of `now`.
fn stashes(now: DateTime<Utc>, repo_id: &str, worktree_path: Option<&str>) -> Value {
    let at = |hours: i64| (now - chrono::Duration::hours(hours)).to_rfc3339();
    json!({
        "type": "stashes",
        "repo_id": repo_id,
        "worktree_path": worktree_path,
        "stashes": [
            { "id": "stash@{0}", "subject": "On feat/petal-footer: try a darker pill border", "created_at": at(3) },
            { "id": "stash@{1}", "subject": "WIP on main: 73ee3ee feat: recover dialog keeps the chosen conversation", "created_at": at(50) },
        ],
    })
}

/// The commits a repo reports, as of `now`: one page, then nothing.
fn commits(now: DateTime<Utc>, repo_id: &str, offset: u32, worktree_path: Option<&str>) -> Value {
    let subjects = [
        (
            "3de017a",
            "docs: plan agent CLI self-update and client screenshots for agents",
        ),
        (
            "87a6270",
            "docs: record Petal build-time rulings for PT.5-PT.9",
        ),
        (
            "3ee329a",
            "docs: plan built-in claude-swap accounts and CLI self-update",
        ),
        (
            "e35c200",
            "docs: land marked features through a feature branch PR",
        ),
        (
            "73ee3ee",
            "feat: recover dialog keeps the chosen conversation",
        ),
    ];
    let commits: Vec<Value> = if offset == 0 {
        (1_i64..)
            .zip(subjects)
            .map(|(hours, (short, subject))| {
                json!({
                    "sha": format!("{short}{}", "0".repeat(33)),
                    "short_sha": short,
                    "author_name": "Leftos Aslanoglou",
                    "author_email": "dev@example.com",
                    "authored_at": (now - chrono::Duration::hours(hours * 5)).to_rfc3339(),
                    "subject": subject,
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    json!({
        "type": "commits",
        "repo_id": repo_id,
        "commits": commits,
        "offset": offset,
        "worktree_path": worktree_path,
    })
}

fn change(path: &str, status: &str) -> Value {
    json!({ "path": path, "status": status, "from_path": null })
}

/// Two staged and three unstaged files, for any repo and tree.
fn repo_status(repo_id: &str, worktree_path: Option<&str>) -> Value {
    json!({
        "type": "repo_status",
        "repo_id": repo_id,
        "worktree_path": worktree_path,
        "index_changes": [
            change("apps/native/src/footer.rs", "M"),
            change("apps/native/assets/icons/petal.svg", "A"),
        ],
        "worktree_changes": [
            change("apps/native/src/palette.rs", "M"),
            change("apps/native/src/sidebar_view.rs", "M"),
            change("docs/native-client.md", "M"),
        ],
    })
}

fn branches(repo_id: &str) -> Value {
    json!({
        "type": "branches",
        "repo_id": repo_id,
        "branches": ["main", PETAL_BRANCH, "feat/recover-keys", "feat/proto-v24"],
        "current": "main",
        "remote_branches": ["origin/main", "origin/feat/petal-footer"],
    })
}

/// The snapshot a `GetFileSnapshot` asks for: the footer before and after
/// the Petal session's edits, whatever path it names.
fn file_snapshot(msg: &ClientMessage) -> Result<DaemonMessage> {
    let ClientMessage::GetFileSnapshot {
        id,
        repo_id,
        path,
        against,
        worktree_path,
    } = msg
    else {
        bail!("not a file snapshot request");
    };
    Ok(DaemonMessage::FileSnapshot {
        id: id.clone(),
        repo_id: repo_id.clone(),
        path: path.clone(),
        against: against.clone(),
        old: FOOTER_OLD.to_owned(),
        new: footer_new()?,
        language: "rust".to_owned(),
        unavailable: None,
        worktree_path: worktree_path.clone(),
    })
}

/// The daemon's answer to a viewer shot's `OpenViewerTab` `id` of `path`:
/// the new tab, then the open's reply naming it.
fn viewer_opened(id: &str, path: &str) -> Result<Vec<DaemonMessage>> {
    let name = Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .with_context(|| format!("the viewer path {path} has no file name"))?;
    let tab = serde_json::from_value(json!({
        "id": VIEWER_TAB,
        "name": name,
        "content": { "kind": "viewer", "path": path },
        "created_at": "2026-01-01T00:00:00Z",
    }))
    .context("the viewer tab fixture")?;
    Ok(vec![
        DaemonMessage::TabUpdated { tab },
        DaemonMessage::ViewerTabOpened {
            id: id.to_owned(),
            tab_id: VIEWER_TAB.to_owned(),
        },
    ])
}

/// The daemon's answer to the `dashboard` shot's `OpenDashboardTab` `id`:
/// the new tab, then the open's reply naming it.
fn dashboard_opened(id: &str) -> Result<Vec<DaemonMessage>> {
    let tab = serde_json::from_value(json!({
        "id": DASHBOARD_TAB,
        "name": "Dashboard",
        "content": { "kind": "dashboard" },
        "created_at": "2026-01-01T00:00:00Z",
    }))
    .context("the Dashboard tab fixture")?;
    Ok(vec![
        DaemonMessage::TabUpdated { tab },
        DaemonMessage::DashboardTabOpened {
            id: id.to_owned(),
            tab_id: DASHBOARD_TAB.to_owned(),
        },
    ])
}

/// The `dashboard` shot's sessions: the asking one holds a question, and
/// an exited run joins the unseen review in Done.
fn dashboard_rows(rows: &mut Vec<Row>) {
    for row in rows.iter_mut().filter(|row| row.id == "s-recover") {
        row.extra["pending_input"] = json!({
            "kind": "question",
            "questions": [{
                "question": "Bind Ctrl+R to Recover, or keep it free for the terminal?",
                "header": "Keys",
                "options": ["Bind Ctrl+R to Recover", "Keep it free"],
                "multi_select": false,
            }],
        });
    }
    for row in rows.iter_mut().filter(|row| row.id == "s-ask") {
        row.extra["pending_input"] = json!({
            "kind": "permission",
            "tool_name": "Bash",
            "summary": "gh pr merge 118 --squash --delete-branch",
        });
    }
    rows.push(Row {
        id: "s-costs",
        label: "Costs per repository",
        status: "stopped",
        members: json!([member(YAAT, "feat/costs-per-repo", YAAT_PATH)]),
        minutes: 25,
        extra: json!({ "exit_code": 0 }),
    });
}

/// The `dashboard-compact` shot's other agents, about twenty in all across
/// the repos and the workspace; `dashboard_facts` gives three of them chips.
fn compact_rows(rows: &mut Vec<Row>) {
    let more = [
        (
            "s-c1",
            "Fold the Done group",
            "working",
            DECK_SERVER,
            DECK_SERVER_PATH,
        ),
        (
            "s-c2",
            "Measure the height probe",
            "idle",
            DECK_SERVER,
            DECK_SERVER_PATH,
        ),
        ("s-c3", "Port the replay parser", "working", YAAT, YAAT_PATH),
        (
            "s-c4",
            "Trim the arrival loader",
            "working",
            YAAT,
            YAAT_PATH,
        ),
        (
            "s-c5",
            "Rewrite the scenario docs",
            "working",
            DECK_CLIENT,
            DECK_CLIENT_PATH,
        ),
        (
            "s-c6",
            "Card text pass",
            "idle",
            DECK_CLIENT,
            DECK_CLIENT_PATH,
        ),
        ("s-c7", "Spike the hold pattern UI", "idle", YAAT, YAAT_PATH),
        ("s-c8", "Sort the strip bay", "idle", YAAT, YAAT_PATH),
        (
            "s-c9",
            "Add runway crossing checks",
            "idle",
            YAAT,
            YAAT_PATH,
        ),
        ("s-c10", "Daemon spawn verbs", "working", TULIP, TULIP_PATH),
    ];
    let mut minutes = 3;
    for (id, label, status, repo, path) in more {
        rows.push(Row {
            id,
            label,
            status,
            members: json!([member(repo, "feat/compact-fleet", path)]),
            minutes,
            extra: json!({}),
        });
        minutes += 4;
    }
}

/// The tracks the `dashboard` shot's watch is sent, as of `now`: a working
/// session in a gate after a restart gap, one that asks, and one idle.
fn dashboard_tracks(now: DateTime<Utc>) -> Result<DaemonMessage> {
    let ago = |minutes: i64| now - chrono::Duration::minutes(minutes);
    let stretch = |session: &str, activity: &str, from: i64, to: Option<i64>| {
        json!({
            "repo": "tulip", "key": format!("session-{session}"), "kind": "session",
            "label": session, "activity": activity, "from": ago(from), "to": to.map(ago),
        })
    };
    let mark = |session: &str, kind: &str, from: i64, to: Option<i64>| {
        json!({
            "repo": "tulip", "key": format!("session-{session}"), "kind": kind,
            "label": session, "from": ago(from), "to": to.map(ago),
        })
    };
    parse_message(json!({
        "type": "activity_tracks",
        "now": now,
        "hours": 2,
        "segments": [
            mark("s-petal", "gap", 120, Some(85)),
            stretch("s-petal", "idle", 85, Some(70)),
            stretch("s-petal", "working", 70, Some(40)),
            stretch("s-petal", "asking", 40, Some(30)),
            stretch("s-petal", "working", 30, None),
            mark("s-petal", "hold", 52, Some(47)),
            mark("s-petal", "hold", 4, None),
            stretch("s-ask", "working", 100, Some(12)),
            stretch("s-ask", "asking", 12, None),
            stretch("s-recover", "idle", 120, None),
            stretch("s-metar", "idle", 120, Some(60)),
            stretch("s-metar", "working", 60, Some(15)),
            stretch("s-metar", "idle", 15, None),
        ],
    }))
}

/// The facts the `dashboard` shot's watch is sent, as of `now`: the
/// working session runs a subagent with a finished child, a shell and a
/// dispatch run, and its context reads hot.
fn dashboard_facts(now: DateTime<Utc>) -> Result<DaemonMessage> {
    let ago = |minutes: i64| now - chrono::Duration::minutes(minutes);
    let message = json!({
        "type": "dashboard_facts",
        "facts": [
            {
                "session_id": "s-petal",
                "now": { "kind": "running_tool", "text": "Running Bash · 1m: cargo nextest run -p native" },
                "agents": [
                    {
                        "key": "subagent:a1", "kind": "subagent", "name": "Check the footer pill spacing",
                        "task": "Running Grep · footer.rs", "state": "running", "started_at": ago(4),
                        "model": "Opus 5.5", "effort": "high",
                        "children": [{
                            "key": "subagent:a2", "kind": "subagent", "name": "Read the palette",
                            "task": "finished · 3 tokens to reuse", "state": "finished", "started_at": ago(3),
                        }],
                    },
                    {
                        "key": "bg:b1", "kind": "shell", "name": "bg shell b1",
                        "task": "Running cargo watch -x check", "state": "running", "started_at": ago(12),
                    },
                    {
                        "key": "dispatch:d1", "kind": "dispatch", "name": "dispatch · RT-455 part B",
                        "task": "Writing the UI specs", "state": "running", "started_at": ago(9),
                        "stuck": { "kind": "tool_failures", "count": 3 },
                    },
                ],
                "context_percent": 86,
                "tokens_in": 3_100_000,
                "cost_usd": 2.46,
                "activity": [2, 4, 6, 3, 8, 12, 9, 5, 7, 10],
                "stuck": [{ "kind": "context_high" }],
                "last_activity_at": ago(0),
                "issue": "RT-455",
            },
            {
                "session_id": "s-recover",
                "now": { "kind": "unknown", "text": "" },
                "context_percent": 41,
                "tokens_in": 842_000,
                "cost_usd": 0.46,
                "activity": [3, 2, 1, 0, 0, 0, 0, 0, 0, 0],
                "last_activity_at": ago(6),
                "issue": "RT-444",
            },
            {
                "session_id": "s-ask",
                "now": { "kind": "unknown", "text": "" },
                "context_percent": 32,
                "tokens_in": 410_000,
                "cost_usd": 0.21,
                "activity": [2, 3, 1, 0, 0, 0, 0, 0, 0, 0],
                "last_activity_at": ago(3),
            },
            {
                "session_id": "s-metar",
                "now": { "kind": "waiting_on_background", "text": "Waiting on a background task" },
                "agents": [{
                    "key": "bg:m1", "kind": "monitor", "name": "bg monitor m1",
                    "task": "Watching the METAR feed", "state": "running", "started_at": ago(25),
                }],
                "context_percent": 23,
                "tokens_in": 210_000,
                "cost_usd": 0.12,
                "activity": [0, 0, 1, 0, 0, 0, 0, 1, 0, 0],
                "last_activity_at": ago(25),
            },
            {
                "session_id": "s-review",
                "now": { "kind": "unknown", "text": "" },
                "context_percent": 64,
                "tokens_in": 1_400_000,
                "cost_usd": 1.08,
                "activity": [5, 6, 2, 0, 0, 0, 0, 0, 0, 0],
                "last_activity_at": ago(1),
            },
            {
                "session_id": "s-proto",
                "now": { "kind": "unknown", "text": "" },
                "context_percent": 18,
                "tokens_in": 96_000,
                "cost_usd": 0.004,
                "activity": [0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                "last_activity_at": ago(48),
            },
            {
                "session_id": "s-costs",
                "now": { "kind": "unknown", "text": "" },
                "context_percent": 51,
                "tokens_in": 960_000,
                "cost_usd": 4.1,
                "activity": [0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                "last_activity_at": ago(25),
                "issue": "OPS-340",
            },
        ],
        "totals": {},
        "linear_synced_at": null,
    });
    parse_message(with_compact_facts(message, now)).context("the Dashboard facts")
}

/// `message` with `compact_facts` added to its facts.
fn with_compact_facts(mut message: Value, now: DateTime<Utc>) -> Value {
    if let Some(facts) = message["facts"].as_array_mut() {
        facts.extend(compact_facts(now));
    }
    message
}

/// The facts of the `dashboard-compact` shot's own agents: one runs five
/// nodes (a finished and a failed one among them), one three, and an idle
/// one keeps a dev server running.
fn compact_facts(now: DateTime<Utc>) -> [Value; 3] {
    let ago = |minutes: i64| now - chrono::Duration::minutes(minutes);
    [
        json!({
            "session_id": "s-c1",
            "now": { "kind": "running_tool", "text": "Running Bash · ssh mac-1" },
            "agents": [
                { "key": "subagent:c1", "kind": "subagent", "name": "Explore", "task": "reading the routes", "state": "running", "started_at": ago(6) },
                { "key": "subagent:c2", "kind": "subagent", "name": "code-review", "task": "3 findings", "state": "finished", "started_at": ago(5) },
                { "key": "bg:c3", "kind": "shell", "name": "gate: clippy", "task": "2 warnings", "state": "failed", "started_at": ago(4) },
                { "key": "gate:c4", "kind": "gate", "name": "gate: probe", "task": "40s", "state": "running", "started_at": ago(2) },
                { "key": "dispatch:c5", "kind": "dispatch", "name": "deepseek: gate-88", "task": "fix round 2", "state": "running", "started_at": ago(1) },
            ],
            "issue": "DLV-52",
        }),
        json!({
            "session_id": "s-c3",
            "now": { "kind": "running_tool", "text": "Running Bash · dotnet build" },
            "agents": [
                { "key": "subagent:c6", "kind": "subagent", "name": "implementer", "task": "porting the frame reader", "state": "running", "started_at": ago(8) },
                { "key": "subagent:c7", "kind": "subagent", "name": "Explore", "task": "read the old parser", "state": "finished", "started_at": ago(7) },
                { "key": "bg:c8", "kind": "shell", "name": "gate: dotnet build", "task": "1m", "state": "running", "started_at": ago(1) },
            ],
            "issue": "YAAT-590",
        }),
        json!({
            "session_id": "s-c7",
            "now": { "kind": "unknown", "text": "" },
            "agents": [
                { "key": "bg:c9", "kind": "shell", "name": "npm dev", "task": ":5174", "state": "running", "started_at": ago(39) },
            ],
        }),
    ]
}

/// The screen a session's scrollback replays.
fn screen_for(session_id: &str) -> String {
    let lines = match session_id {
        "s-shell" => SHELL_SCREEN,
        "s-review" => CLAUDE_REVIEW_SCREEN,
        "s-petal" => CLAUDE_WORKING_SCREEN,
        _ => CLAUDE_IDLE_SCREEN,
    };
    lines.join("\r\n")
}

const CLAUDE_WORKING_SCREEN: &[&str] = &[
    "\x1b[90m>\x1b[0m Tighten the footer pill spacing and use the petal palette",
    "",
    "\x1b[32m●\x1b[0m \x1b[1mRead\x1b[0m(apps/native/src/footer.rs)",
    "  \x1b[90m⎿  Read 212 lines\x1b[0m",
    "",
    "\x1b[32m●\x1b[0m \x1b[1mUpdate\x1b[0m(apps/native/src/footer.rs)",
    "  \x1b[90m⎿  Updated footer.rs with 6 additions and 3 removals\x1b[0m",
    "",
    "\x1b[37m●\x1b[0m The pill now sits PILL_GAP (6 px) from the status text",
    "  and takes the petal accent. Running the sidebar specs next.",
    "",
    "\x1b[32m●\x1b[0m \x1b[1mBash\x1b[0m(cargo test -p rustling-tulip-native --test ui_sidebar)",
    "  \x1b[90m⎿  Running…\x1b[0m",
    "",
    "\x1b[38;5;209m✻ Brewing…\x1b[0m \x1b[90m(38s · ↑ 2.1k tokens · esc to interrupt)\x1b[0m",
    "\x1b[90m──────────────────────────────────────────────────────────────────\x1b[0m",
    "> ",
    "\x1b[90m──────────────────────────────────────────────────────────────────\x1b[0m",
    "  \x1b[38;5;177m⏵⏵ accept edits on\x1b[0m \x1b[90m(shift+tab to cycle)\x1b[0m\x1b[?25l",
];

const CLAUDE_IDLE_SCREEN: &[&str] = &[
    "\x1b[90m>\x1b[0m Summarize what changed since the last release",
    "",
    "\x1b[37m●\x1b[0m Three changes landed since v0.9.2: the recover dialog",
    "  keeps the chosen conversation, the METAR parser accepts RMK groups,",
    "  and the strip layout keeps its column widths across restarts.",
    "",
    "\x1b[90m──────────────────────────────────────────────────────────────────\x1b[0m",
    "> ",
    "\x1b[90m──────────────────────────────────────────────────────────────────\x1b[0m",
    "  \x1b[90m? for shortcuts\x1b[0m\x1b[?25l",
];

const CLAUDE_REVIEW_SCREEN: &[&str] = &[
    "\x1b[90m>\x1b[0m Review diff_model.rs for the hunk-merge edge cases",
    "",
    "\x1b[37m●\x1b[0m Two hunks one context line apart merge into one; the tests",
    "  cover a gap of three but not of one. Added that case.",
    "",
    "\x1b[90m──────────────────────────────────────────────────────────────────\x1b[0m",
    "> ",
    "\x1b[90m──────────────────────────────────────────────────────────────────\x1b[0m",
    "  \x1b[90m? for shortcuts\x1b[0m\x1b[?25l",
];

const SHELL_SCREEN: &[&str] = &[
    "\x1b[32mPS\x1b[0m D:\\src\\rustling-tulip> git log --oneline -3",
    "\x1b[33m3de017a\x1b[0m (\x1b[36mHEAD -> main\x1b[0m, \x1b[31morigin/main\x1b[0m) docs: plan agent CLI self-update",
    "\x1b[33m87a6270\x1b[0m docs: record Petal build-time rulings for PT.5-PT.9",
    "\x1b[33m3ee329a\x1b[0m docs: plan built-in claude-swap accounts and CLI self-update",
    "\x1b[32mPS\x1b[0m D:\\src\\rustling-tulip> cargo test -p protocol hello_decodes",
    "\x1b[1;32m    Finished\x1b[0m `test` profile [unoptimized + debuginfo] target(s) in 0.41s",
    "running 2 tests",
    "test tests::hello_decodes_with_scalar_only ... \x1b[32mok\x1b[0m",
    "test tests::hello_decodes_with_range_field ... \x1b[32mok\x1b[0m",
    "",
    "test result: \x1b[32mok\x1b[0m. 2 passed; 0 failed; 0 ignored; 0 measured; 119 filtered out",
    "",
    "\x1b[32mPS\x1b[0m D:\\src\\rustling-tulip> ",
];

/// The footer before the Petal session's edits.
/// The Markdown file the `viewer-md` shot renders: one block of each kind.
const README_MD: &str = "# Rustling Tulip

A native client for many parallel **Claude** sessions, with *worktrees*, \
`git` status and ~~no~~ a [Markdown viewer](https://example.com).

## Getting started

1. Install the prerequisites with `.\\rt.ps1 setup`.
2. Build and run the client:
   - `.\\rt.ps1 build`
   - `.\\rt.ps1 native`
3. Open a `.md` file from a terminal link.

- [x] Headings, lists and quotes
- [ ] Link clicks

> The daemon owns every PTY; the client is just a client.

```rust
fn main() -> anyhow::Result<()> {
    let count = 3; // sessions
    println!(\"{count} sessions\");
    Ok(())
}
```

| Tier | Runs | Needs |
|:-----|:----:|------:|
| UI specs | in process | nothing |
| Live e2e | real daemon | node |

---

![the main window](docs/main.png)
";

const FOOTER_OLD: &str = r"//! The footer: the connection pill, the status text and the log flyout.

use gpui::{Div, SharedString, div, prelude::*, px};

use crate::palette::{BAR_BG, BORDER, MUTED, TEXT};

/// The pill's height in logical pixels.
const PILL_HEIGHT: f32 = 16.0;
/// Space between the pill and the status text.
const PILL_GAP: f32 = 4.0;

/// What the footer shows.
pub struct Footer {
    pub label: SharedString,
    pub status: SharedString,
    pub connected: bool,
}

impl Footer {
    /// The pill: the connection label in a rounded border.
    fn pill(&self) -> Div {
        let color = if self.connected { TEXT } else { MUTED };
        div()
            .flex()
            .items_center()
            .h(px(PILL_HEIGHT))
            .px(px(6.0))
            .rounded_full()
            .border_1()
            .border_color(gpui::rgb(BORDER))
            .text_color(gpui::rgb(color))
            .child(self.label.clone())
    }

    /// The whole footer row.
    pub fn render(&self) -> Div {
        div()
            .flex()
            .items_center()
            .gap(px(PILL_GAP))
            .px(px(8.0))
            .bg(gpui::rgb(BAR_BG))
            .child(self.pill())
            .child(div().text_color(gpui::rgb(MUTED)).child(self.status.clone()))
    }
}
";

/// The Petal session's edits to [`FOOTER_OLD`], each `(from, to)`.
const FOOTER_EDITS: &[(&str, &str)] = &[
    ("BORDER, MUTED, TEXT};", "BORDER, MUTED, PETAL, TEXT};"),
    (
        "const PILL_GAP: f32 = 4.0;\n",
        "const PILL_GAP: f32 = 6.0;\n/// The status dot left of the pill's label.\nconst DOT_SIZE: f32 = 6.0;\n",
    ),
    (
        "    /// The pill: the connection label in a rounded border.\n",
        "    /// The pill: a status dot and the connection label in a rounded border.\n",
    ),
    ("{ TEXT } else { MUTED };", "{ PETAL } else { MUTED };"),
    (
        "            .text_color(gpui::rgb(color))\n            .child(self.label.clone())",
        "            .gap(px(4.0))\n            .text_color(gpui::rgb(TEXT))\n            .child(div().size(px(DOT_SIZE)).rounded_full().bg(gpui::rgb(color)))\n            .child(self.label.clone())",
    ),
    (
        "            .px(px(8.0))\n            .bg(gpui::rgb(BAR_BG))",
        "            .px(px(10.0))\n            .border_t_1()\n            .border_color(gpui::rgb(BORDER))\n            .bg(gpui::rgb(BAR_BG))",
    ),
];

/// The footer after the Petal session's edits.
fn footer_new() -> Result<String> {
    FOOTER_EDITS
        .iter()
        .try_fold(FOOTER_OLD.to_owned(), |text, (from, to)| {
            if text.contains(from) {
                Ok(text.replacen(from, to, 1))
            } else {
                Err(anyhow!("the footer fixture has no {from:?} to edit"))
            }
        })
}

/// Clicking the cloaked window, reading its pixels and writing them as a PNG.
#[cfg(windows)]
mod win32 {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use anyhow::{Context as _, Result, anyhow, bail};
    use gpui::Window;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, ClientToScreen, CreateCompatibleBitmap,
        CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC,
        SelectObject,
    };
    use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PW_CLIENTONLY, PrintWindow};
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClientRect, GetForegroundWindow, PW_RENDERFULLCONTENT, PostMessageW, WM_LBUTTONDOWN,
        WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    };

    /// How long the window gets to stop changing between captures.
    const SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
    /// The gap between two captures that must match.
    const CAPTURE_GAP: Duration = Duration::from_millis(250);
    /// `wParam` of a mouse message while the left button is down.
    const MK_LBUTTON: usize = 0x0001;
    /// Where the pointer is parked after a click: off the client area, so no
    /// hover or tooltip shows in the capture.
    const PARKED: (i32, i32) = (-10, -10);

    fn hwnd(address: usize) -> HWND {
        HWND(std::ptr::with_exposed_provenance_mut(address))
    }

    /// Clicks client point `at`, in logical pixels, of the window at
    /// `address` with posted mouse messages (a move, a press, a release), as
    /// the OS delivers a click, then parks the pointer off the client area.
    /// Posted messages reach only this window: the user's pointer and focus
    /// are untouched.
    pub fn click(address: usize, (x, y): (i32, i32)) -> Result<()> {
        let hwnd = hwnd(address);
        // SAFETY: a plain query on this live window; 0 means it has no DPI.
        let dpi = match unsafe { GetDpiForWindow(hwnd) } {
            0 => 96,
            dpi => i32::try_from(dpi).unwrap_or(96),
        };
        let at = coordinates(x * dpi / 96, y * dpi / 96);
        post(hwnd, WM_MOUSEMOVE, 0, at)?;
        post(hwnd, WM_LBUTTONDOWN, MK_LBUTTON, at)?;
        post(hwnd, WM_LBUTTONUP, 0, at)?;
        post(hwnd, WM_MOUSEMOVE, 0, coordinates(PARKED.0, PARKED.1))
    }

    /// Turns the wheel `notches` notches toward the user over client point
    /// `at`, in logical pixels, of the window at `address`, with one posted
    /// wheel message, whose position Windows gives in screen pixels.
    pub fn wheel_down(address: usize, (x, y): (i32, i32), notches: i16) -> Result<()> {
        /// One notch of the wheel, as `WM_MOUSEWHEEL` counts it.
        const WHEEL_DELTA: i16 = 120;
        let hwnd = hwnd(address);
        // SAFETY: a plain query on this live window; 0 means it has no DPI.
        let dpi = match unsafe { GetDpiForWindow(hwnd) } {
            0 => 96,
            dpi => i32::try_from(dpi).unwrap_or(96),
        };
        let mut at = POINT {
            x: x * dpi / 96,
            y: y * dpi / 96,
        };
        // SAFETY: `at` is a live POINT and `hwnd` a live window.
        if !unsafe { ClientToScreen(hwnd, &raw mut at) }.as_bool() {
            bail!("mapping the wheel's point to the screen failed");
        }
        let delta = notches.saturating_mul(-WHEEL_DELTA);
        let wparam = usize::from(delta.cast_unsigned()) << 16;
        post(hwnd, WM_MOUSEWHEEL, wparam, coordinates(at.x, at.y))
    }

    /// Client pixel (`x`, `y`) packed as a mouse message's `lParam`: two
    /// signed 16-bit words, `y` high.
    fn coordinates(x: i32, y: i32) -> isize {
        let packed = ((y & 0xffff) << 16) | (x & 0xffff);
        isize::try_from(packed).unwrap_or_default()
    }

    fn post(hwnd: HWND, message: u32, wparam: usize, lparam: isize) -> Result<()> {
        // SAFETY: posts a plain input message to a live window.
        unsafe { PostMessageW(Some(hwnd), message, WPARAM(wparam), LPARAM(lparam)) }
            .with_context(|| format!("posting mouse message {message:#x} to the window"))
    }

    /// The address of `window`'s Win32 handle, which crosses threads where
    /// an `HWND` cannot.
    pub fn window_address(window: &Window) -> Result<usize> {
        // `Window::window_handle` is gpui's own handle; the raw one is the trait's.
        match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
            Ok(RawWindowHandle::Win32(handle)) => Ok(handle.hwnd.get().cast_unsigned()),
            other => Err(anyhow!("the window has no Win32 handle ({other:?})")),
        }
    }

    /// A capture of the client area: top-down rows of BGRA pixels.
    #[derive(PartialEq, Eq)]
    struct Frame {
        width: u32,
        height: u32,
        bgra: Vec<u8>,
    }

    /// Captures the window at `address` until two captures in a row match or
    /// [`SETTLE_TIMEOUT`] passes, and writes the last one to `out`. Returns
    /// whether the window settled.
    pub fn write_settled(address: usize, out: &Path) -> Result<bool> {
        let hwnd = hwnd(address);
        let deadline = Instant::now() + SETTLE_TIMEOUT;
        let mut last = capture(hwnd)?;
        let settled = loop {
            std::thread::sleep(CAPTURE_GAP);
            let next = capture(hwnd)?;
            let same = next == last;
            last = next;
            if same {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
        };
        let blank = last
            .bgra
            .as_chunks::<4>()
            .0
            .iter()
            .all(|&[blue, green, red, _]| [blue, green, red] == [0, 0, 0]);
        if blank {
            bail!("the window captured black: it drew nothing");
        }
        write_png(&last, out)?;
        Ok(settled)
    }

    /// The client area through `PrintWindow` with `PW_RENDERFULLCONTENT`,
    /// which reads the compositor's copy of a cloaked window without
    /// showing, moving or activating it.
    fn capture(hwnd: HWND) -> Result<Frame> {
        let mut rect = RECT::default();
        // SAFETY: an out-pointer to a local, on a live window handle.
        unsafe { GetClientRect(hwnd, &raw mut rect) }.context("reading the client rect")?;
        let (width, height) = (rect.right, rect.bottom);
        if width <= 0 || height <= 0 {
            bail!("the window has no client area ({width}x{height})");
        }
        let pixels = usize::try_from(width)? * usize::try_from(height)?;
        let mut bgra = vec![0u8; pixels * 4];
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: u32::try_from(size_of::<BITMAPINFOHEADER>())?,
                biWidth: width,
                // Negative: top-down rows.
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let flags = PRINT_WINDOW_FLAGS(PW_CLIENTONLY.0 | PW_RENDERFULLCONTENT);
        let rows = u32::try_from(height)?;
        // SAFETY: the DCs and bitmap are created, used and released here, on
        // a live window; `bgra` holds the bitmap's full size in the format
        // `info` describes.
        let (printed, lines) = unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            let bitmap = CreateCompatibleBitmap(screen, width, height);
            let previous = SelectObject(dc, bitmap.into());
            let printed = PrintWindow(hwnd, dc, flags);
            SelectObject(dc, previous);
            let bits = Some(bgra.as_mut_ptr().cast());
            let lines = GetDIBits(dc, bitmap, 0, rows, bits, &raw mut info, DIB_RGB_COLORS);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
            (printed.as_bool(), lines)
        };
        if !printed {
            bail!("PrintWindow failed on the shot window");
        }
        if lines != height {
            bail!("GetDIBits copied {lines} of {height} rows");
        }
        check_out_of_the_way(hwnd)?;
        Ok(Frame {
            width: u32::try_from(width)?,
            height: rows,
            bgra,
        })
    }

    /// Fails unless the window is still cloaked and not the foreground
    /// window: the shot must never show on screen or take the user's focus.
    fn check_out_of_the_way(hwnd: HWND) -> Result<()> {
        let mut cloaked: u32 = 0;
        let size = u32::try_from(size_of::<u32>())?;
        // SAFETY: an out-pointer to a local of the size passed, on a live
        // window handle.
        unsafe { DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&raw mut cloaked).cast(), size) }
            .context("reading the window's cloaked state")?;
        if cloaked == 0 {
            bail!("the shot window is not cloaked, so it may show on screen");
        }
        // SAFETY: a plain query.
        if unsafe { GetForegroundWindow() } == hwnd {
            bail!("the shot window became the foreground window");
        }
        Ok(())
    }

    /// Writes `frame` to `out` as an RGBA PNG; the capture's alpha is
    /// undefined, so every pixel is written opaque.
    fn write_png(frame: &Frame, out: &Path) -> Result<()> {
        let mut rgba = Vec::with_capacity(frame.bgra.len());
        for &[blue, green, red, _] in frame.bgra.as_chunks::<4>().0 {
            rgba.extend_from_slice(&[red, green, blue, u8::MAX]);
        }
        let file =
            std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
        let mut encoder =
            png::Encoder::new(std::io::BufWriter::new(file), frame.width, frame.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().context("writing the PNG header")?;
        writer
            .write_image_data(&rgba)
            .context("writing the PNG pixels")?;
        writer.finish().context("finishing the PNG")
    }
}

/// The shot drives and reads a Win32 window.
#[cfg(not(windows))]
mod win32 {
    use std::path::Path;

    use anyhow::{Result, bail};
    use gpui::Window;

    pub fn window_address(_window: &Window) -> Result<usize> {
        bail!("the shot reads a Win32 window's pixels, so it runs on Windows only")
    }

    pub fn click(_address: usize, _at: (i32, i32)) -> Result<()> {
        bail!("the shot clicks a Win32 window, so it runs on Windows only")
    }

    pub fn wheel_down(_address: usize, _at: (i32, i32), _notches: i16) -> Result<()> {
        bail!("the shot scrolls a Win32 window, so it runs on Windows only")
    }

    pub fn write_settled(_address: usize, _out: &Path) -> Result<bool> {
        bail!("the shot reads a Win32 window's pixels, so it runs on Windows only")
    }
}

/// The fixture is parsed before the window opens, so a change to the wire
/// shape fails here rather than in a PNG nobody looks at.
#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;

    #[test]
    fn every_view_has_a_fixture() {
        for view in View::ALL {
            let built = Fixture::build(view, Utc::now());
            assert!(
                built.is_ok(),
                "the {} fixture: {:?}",
                view.name(),
                built.err()
            );
        }
    }

    #[test]
    fn a_message_that_is_malformed_does_not_parse() {
        let malformed = parse_message(json!({ "type": "repos" }));
        assert!(
            malformed.is_err(),
            "a repos message without its repos field parsed"
        );
    }

    #[test]
    fn main_compact_seeds_compact_density() {
        let dir = scratch_dir("main-compact");
        shot(View::MainCompact, dir.clone())
            .prepare()
            .expect("preparing the main-compact shot");
        let file = dir.join("native-ui.json");
        let text = std::fs::read_to_string(&file).expect("the layout the shot saved");
        let layout: Value = serde_json::from_str(&text).expect("the saved layout parses");
        assert_eq!(
            layout
                .pointer("/general/leaf_density")
                .and_then(Value::as_str),
            Some("compact"),
            "the shot saved {text}"
        );
    }

    #[test]
    fn main_clears_saved_ui_state() {
        let dir = scratch_dir("main-clears");
        shot(View::MainCompact, dir.clone())
            .prepare()
            .expect("preparing the main-compact shot");
        let file = dir.join("native-ui.json");
        assert!(file.exists(), "the main-compact shot saved no layout");
        shot(View::Main, dir.clone())
            .prepare()
            .expect("preparing the main shot");
        assert!(!file.exists(), "the main shot left a saved layout behind");
    }

    /// A shot whose layout file lands in a `.ui` folder of its own under the
    /// repo's `.tmp/`, named after the test that asked for it.
    fn shot(view: View, ui_dir: PathBuf) -> Shot {
        Shot {
            view,
            out: ui_dir.join("shot.png"),
            ui_dir,
        }
    }

    /// `.tmp/shot-tests/<name>/.ui` under the repo root.
    fn scratch_dir(name: &str) -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("the native client's folder has no repo root two levels up");
        root.join(".tmp").join("shot-tests").join(name).join(".ui")
    }
}
