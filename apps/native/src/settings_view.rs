//! The Settings modal: a tab list on the left and the chosen tab's rows
//! beside it. General holds keep-awake and copy on select, Notifications
//! the Needs you toggle and sound and Windows' toast state, Appearance
//! the appearance editor at the app level, App title the window title's
//! settings; every change except Appearance is pending until OK or Apply.
//! The main window's title, which the App title tab controls, is kept here
//! too.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, Div, Entity, EntityId, FocusHandle,
    Focusable as _, FontWeight, Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels,
    ScrollHandle, SharedString, Stateful, StyledText, Subscription, Window, canvas, div, point,
    prelude::*, px, svg,
};
use protocol::{
    AgentCommandEntry, AgentDescriptor, AgentId, ClientMessage, DaemonMessage,
    FEATURE_AGENT_COMMANDS, FEATURE_AGENT_USER_HOOKS, FEATURE_DEFAULT_SHELL,
    FEATURE_DETAILED_LOGGING, FieldKind, KeepAwakeMode, KeyState, OptionValue, RemoteState, Secret,
    SummarizerKeySource, SummarizerSettings, SummarySource, UserHooksWire, error_codes,
};

use crate::accounts_settings::{AccountsControl, AliasFields, RemoveConfirm};
use crate::agent_options::{AgentOptions, FieldEditor, FieldView, descriptor_for, trusted_moves};
use crate::agent_options_view::UPDATE_CLIENT;
use crate::appearance_stops::{AppearanceStop, after_lost};
use crate::appearance_view::Level;
use crate::assets::SIDEBAR_CHEVRON_DOWN_ICON;
use crate::buttons::{Layer, field_frame, focus_ring};
use crate::chat::ChatMode;
use crate::combobox::list_placement;
use crate::driver_id::DriverId as _;
use crate::mouse::CopyOnSelect;
use crate::notify::{NotifyState, WINDOWS_NOTIFICATION_SETTINGS};
use crate::open::OpenJob;
use crate::open_view::Then;
use crate::remote_access::{
    PortError, RemoteStatusView, address, apply_port_message, autostart_failure_line,
    autostart_read_failure_line, cert_line, controls_enabled, parse_port, state_line,
};
use crate::scrollbar::{Scrollbar, Thumb};
use crate::section::section;
use crate::session_menu::{backdrop, dialog_button, dialog_look};
use crate::settings_search::{self, DrawnTab, TabHits, Visibility};
use crate::shell_choice::{self, HostShell, RowState, ShellEntry, ShellSetting};
use crate::sidebar::{
    CountMode, FINISHED_GRACE_CHOICES, LeafDensity, NotifySkip, RepoSort, SessionsUnderRepos,
    SidebarView, ToastSound, ToastStyle,
};
use crate::sound::{self, BundledSound, NotifyKind, SOUND_EXTENSIONS};
use crate::spawn_view::{
    DISABLED_OPACITY, Look, SEGMENT_RADIUS, checkbox_row, choice_button, close_button, dialog_card,
    dialog_footer, dialog_title_bar, field, labelled_text, muted, popup_panel, segmented,
};
use crate::summarizer_settings::{SummarizerStatus, TestOutcome, parse_cap};
use crate::target::HostKey;
use crate::text_input::{TextChanged, TextInput, TextInputEvent};
use crate::ui_theme::{UiTheme, theme};
use crate::window_title::compute_title;
use crate::{FileFilter, Pick, RootView, tooltip};

/// The tab list's width, its padding and edge included; fixed so the tabs' column lines up.
const TAB_LIST_WIDTH: f32 = 200.0;
/// A tab row's height and corner radius, as on the Settings board; fixed so the tabs line up.
const TAB_ROW_HEIGHT: f32 = 34.0;
/// A section label's text size, above the rows of its group.
const SECTION_LABEL_SIZE: f32 = 11.0;
/// How opaque a host row whose connection is not open draws: the same dim
/// the sidebar gives a remote host's rows while it is not open
/// (`sidebar_view::DIMMED_ROW_OPACITY`).
const OFFLINE_HOST_OPACITY: f32 = 0.55;
const TAB_ROW_RADIUS: f32 = 7.0;
/// As wide as the Appearance tab's two columns, so the modal keeps its size
/// from tab to tab.
const CONTENT_WIDTH: f32 = 656.0;
/// The padding round the shown tab's rows.
const CONTENT_PAD_X: f32 = 18.0;
const CONTENT_PAD_Y: f32 = 16.0;
/// The room kept above and below the panel on a window too short for it.
const PANEL_MARGIN_Y: f32 = 24.0;
/// The room kept left and right of the panel on a window too narrow for it.
const PANEL_MARGIN_X: f32 = 24.0;
/// The modal's width: the tab list, the content and its padding, and the
/// card's 1 px edges.
const PANEL_WIDTH: f32 = TAB_LIST_WIDTH + CONTENT_WIDTH + 2.0 * CONTENT_PAD_X + 2.0;
/// The panel's height whatever the category, so switching categories never
/// resizes it; a taller category scrolls.
const PANEL_HEIGHT: f32 = 640.0;
/// The size of a hint.
const HINT_SIZE: f32 = 12.5;
/// The width of a sound field, and of its list: an input's width.
const SOUND_FIELD_WIDTH: f32 = 280.0;
/// The most height a sound list takes before it scrolls.
const SOUND_LIST_MAX_HEIGHT: f32 = 240.0;
/// The corner radius of a text link, and of the title preview's chip.
const LINK_RADIUS: f32 = 4.0;
const CHIP_RADIUS: f32 = 5.0;
/// How long the window title waits for its inputs to settle, so a session
/// flickering between working and idle does not flicker the taskbar.
pub(crate) const TITLE_DEBOUNCE: Duration = Duration::from_millis(350);
const NOT_CONNECTED: &str = "(daemon not connected)";
const WORKTREES_HINT: &str = "Per-session git worktrees land under this directory. Changes take effect for new sessions only — sessions already spawned keep their existing worktree paths.";
const REMOTE_INTRO: &str = "Lets your other devices on your tailnet connect to this PC's sessions over HTTPS. Only devices signed in to your own Tailscale account can connect.";
const REMOTE_PORT_HINT: &str = "Changing the port or this PC's Tailscale name changes the address the iPhone app is installed at.";
const REMOTE_RUN_HINT: &str =
    "While remote access is on, the daemon keeps running when no window is open.";
const AUTOSTART_HINT: &str = "So remote access works before you open this app.";
/// The port field's placeholder: the daemon's default port.
const DEFAULT_PORT_TEXT: &str = "8787";
/// The port field's width: an input's width.
const PORT_FIELD_WIDTH: f32 = 96.0;
/// The Maximum chat width field's hint, placeholder and validation error.
const CHAT_WIDTH_HINT: &str = "0 for no limit.";
/// The Detailed logging switch's hint.
const DETAILED_LOGGING_HINT: &str =
    "Records why each session changes status. Fills the 5 MiB log files faster.";
const CHAT_WIDTH_PLACEHOLDER: &str = "960";
const CHAT_WIDTH_ERROR: &str = "Enter a whole number of pixels, 0 or more.";
/// Why an agent page's launch-command rows are disabled on a daemon that
/// does not store a launch command.
const LAUNCH_COMMAND_UNSUPPORTED_TIP: &str =
    "Restart the daemon to set the launch command: .\\rt.ps1 restart";
/// What the disabled Default shell dropdown shows on a daemon that stores
/// no default shell: what such a daemon runs.
const UNSUPPORTED_SHELL_TEXT: &str = "Automatic";
/// The tab list's group row over the agents' pages.
const AGENTS_GROUP: &str = "Agents";
const LAUNCH_ARGS_PLACEHOLDER: &str = "Extra arguments";
/// The Hooks section's note while some of the daemon's entries are missing
/// from the agent's user settings or run another hook shim.
const USER_HOOKS_REPAIR_HINT: &str =
    "Some entries are missing or out of date. Turn it off and on to repair.";
/// The dimmed Hooks switch's tooltip, on a daemon without the feature.
const USER_HOOKS_UNSUPPORTED_TIP: &str =
    "This host's daemon is too old for this setting. Update rustling-tulip there to use it.";
const LAUNCH_USE_HINT: &str = "Used for new sessions, Restart and Resume; not for headless runs.";
const LAUNCH_QUOTING_HINT: &str = "Extra arguments follow POSIX shell rules: quote a Windows path or double its backslashes. In cmd, a doskey macro is not expanded; set Program to a .cmd file instead.";

/// The search box's selector, which [`RootView::settings_focus`] also
/// names it by, its placeholder, and the line the list shows when nothing
/// matches.
const SEARCH_SELECTOR: &str = "settings-search";
const SEARCH_PLACEHOLDER: &str = "Search settings";
const NO_MATCH: &str = "No settings match";
/// How far the search highlight's outline sits outside its row, and the
/// room kept above a row the search scrolls to.
const HIT_OUTSET: f32 = 3.0;
const HIT_MARGIN: f32 = 8.0;
/// The Dashboard's Provider row, which no control of its own names: each
/// provider's control extends it with the provider's id.
const DASHBOARD_PROVIDER_ROW: &str = "settings-dashboard-provider";

/// The Settings search box: built each time Settings opens, dropped when it
/// closes, so its text is never kept.
pub(crate) struct SettingsSearch {
    input: Entity<TextInput>,
    focus: FocusHandle,
    /// The highlighted row scrolls into view the next time it is drawn.
    reveal: bool,
    _subscriptions: Vec<Subscription>,
}

/// The selector of the setting the search highlights, with the `RootView`
/// whose Settings holds it. Each root sets it at the top of its render,
/// before any layer draws, and the row helpers, which cannot read the root
/// they draw for, read it there; a row drawn for any other root, as in
/// another window, finds no hit.
#[derive(Debug, Default)]
struct SearchHit(Option<(EntityId, &'static str)>);

impl gpui::Global for SearchHit {}

/// The hit in `slot` that the rows of root `root` draw: none unless `root`
/// owns it.
fn owned_hit(slot: Option<(EntityId, &'static str)>, root: EntityId) -> Option<&'static str> {
    slot.filter(|(owner, _)| *owner == root).map(|(_, hit)| hit)
}

/// The slot after root `root` drew with `hit`: its own hit, else the slot
/// cleared of any hit it owned; another root's hit stays for that root.
fn next_hit_slot(
    slot: Option<(EntityId, &'static str)>,
    root: EntityId,
    hit: Option<&'static str>,
) -> Option<(EntityId, &'static str)> {
    match hit {
        Some(hit) => Some((root, hit)),
        None => slot.filter(|(owner, _)| *owner != root),
    }
}

/// The search highlight's outline for the row of `selector`, whose corners
/// have `radius`, while that row is the hit of the root it is drawn for: it
/// also measures the row, which scrolls into view when the search asks.
pub(crate) fn search_hit_marker(
    selector: &str,
    radius: f32,
    cx: &mut Context<RootView>,
) -> Option<Div> {
    let slot = cx.try_global::<SearchHit>()?.0;
    let hit = owned_hit(slot, cx.entity_id())?;
    if hit != selector {
        return None;
    }
    let root = cx.weak_entity();
    let accent = theme(cx).accent;
    Some(
        div()
            .absolute()
            .inset(px(-HIT_OUTSET))
            .border_1()
            .border_color(gpui::rgb(accent))
            .rounded(px(radius + HIT_OUTSET))
            .driver_id(|| "settings-search-hit".to_owned())
            .child(
                canvas(
                    move |bounds, window, cx| {
                        // Fails only when the view is gone, and Settings
                        // with it.
                        root.update(cx, |this, cx| this.reveal_search_hit(bounds, window, cx))
                            .ok();
                    },
                    |_, (), _, _| {},
                )
                .size_full(),
            ),
    )
}

/// The search highlight's outline for a row of `controls`, named by its
/// first, while that row is the hit.
fn row_hit_marker(controls: &[SettingsControl], cx: &mut Context<RootView>) -> Option<Div> {
    let first = controls.first()?;
    search_hit_marker(first.selector(), 0.0, cx)
}

/// The content's new scroll offset (negative once scrolled down) that shows
/// `row` from its top, `HIT_MARGIN` below the viewport's, unless it already
/// shows whole; both ranges lie where they are at `offset`, and the offset
/// stays within `0..=-max`.
fn hit_offset(
    viewport: std::ops::Range<Pixels>,
    row: std::ops::Range<Pixels>,
    offset: Pixels,
    max: Pixels,
) -> Pixels {
    if row.start >= viewport.start && row.end <= viewport.end {
        return offset;
    }
    (offset + viewport.start + px(HIT_MARGIN) - row.start)
        .min(px(0.0))
        .max(-max)
}

/// A tab of the Settings modal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum SettingsTab {
    #[default]
    General,
    Notifications,
    Dashboard,
    Worktrees,
    RemoteAccess,
    /// This PC's saved agent accounts and the auto-switch rules.
    Accounts,
    Appearance,
    AppTitle,
    /// The page of the agent with this id (interned), one per agent the
    /// daemon lists.
    Agent(&'static str),
}

impl SettingsTab {
    /// The tabs shown before the agents' pages, in the list's order.
    pub(crate) const FIXED: [Self; 8] = [
        Self::General,
        Self::Notifications,
        Self::Dashboard,
        Self::Worktrees,
        Self::RemoteAccess,
        Self::Accounts,
        Self::Appearance,
        Self::AppTitle,
    ];

    /// The tab's label: an agent page's is its descriptor's label in
    /// `agents`, else the agent's id.
    pub(crate) fn label(self, agents: &[AgentDescriptor]) -> &'static str {
        match self {
            Self::General => "General",
            Self::Notifications => "Notifications",
            Self::Dashboard => "Dashboard",
            Self::Worktrees => "Worktrees",
            Self::RemoteAccess => "Remote access",
            Self::Accounts => "Accounts",
            Self::Appearance => "Appearance",
            Self::AppTitle => "App title",
            Self::Agent(id) => agent_descriptor(agents, id)
                .map_or(id, |descriptor| intern(descriptor.label.clone())),
        }
    }

    /// The tab row's selector: an agent page's names its descriptor's tag.
    fn selector(self, agents: &[AgentDescriptor]) -> &'static str {
        match self {
            Self::General => "settings-tab-general",
            Self::Notifications => "settings-tab-notifications",
            Self::Dashboard => "settings-tab-dashboard",
            Self::Worktrees => "settings-tab-worktrees",
            Self::RemoteAccess => "settings-tab-remote-access",
            Self::Accounts => "settings-tab-accounts",
            Self::Appearance => "settings-tab-appearance",
            Self::AppTitle => "settings-tab-app-title",
            Self::Agent(id) => {
                let tag = agent_descriptor(agents, id).map_or(id, |descriptor| &descriptor.tag);
                intern(format!("settings-tab-{tag}"))
            }
        }
    }

    /// The agent whose page this tab is.
    fn agent(self) -> Option<AgentId> {
        match self {
            Self::Agent(id) => Some(AgentId(id.to_owned())),
            _ => None,
        }
    }

    /// The tab above (`down` false) or below this one among `tabs`, the
    /// list as it is drawn, stopping at the ends.
    fn step(self, tabs: &[Self], down: bool) -> Self {
        let at = tabs.iter().position(|tab| *tab == self).unwrap_or(0);
        let next = if down {
            (at + 1).min(tabs.len().saturating_sub(1))
        } else {
            at.saturating_sub(1)
        };
        tabs.get(next).copied().unwrap_or_default()
    }
}

/// A row of the Settings tab list: a real tab, or a group label with no
/// page of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsRow {
    Group(&'static str),
    Tab(SettingsTab),
}

impl SettingsRow {
    fn label(self, agents: &[AgentDescriptor]) -> &'static str {
        match self {
            Self::Group(label) => label,
            Self::Tab(tab) => tab.label(agents),
        }
    }
}

/// The descriptor `agents` lists under id `id`.
fn agent_descriptor<'a>(agents: &'a [AgentDescriptor], id: &str) -> Option<&'a AgentDescriptor> {
    agents.iter().find(|descriptor| descriptor.id.0 == id)
}

/// The tab list's rows in order: the fixed tabs (Remote access only when
/// `remote`, Accounts only when `accounts`), then, while `agents` lists
/// any, the Agents group row and one page per agent in the daemon's order.
/// A group row is drawn between the tabs and never reached by the keyboard.
fn tab_rows(agents: &[AgentDescriptor], remote: bool, accounts: bool) -> Vec<SettingsRow> {
    let fixed = SettingsTab::FIXED
        .into_iter()
        .filter(|tab| remote || *tab != SettingsTab::RemoteAccess)
        .filter(|tab| accounts || *tab != SettingsTab::Accounts)
        .map(SettingsRow::Tab);
    let group = (!agents.is_empty()).then_some(SettingsRow::Group(AGENTS_GROUP));
    let pages = agents
        .iter()
        .map(|descriptor| SettingsRow::Tab(SettingsTab::Agent(intern(descriptor.id.0.clone()))));
    fixed.chain(group).chain(pages).collect()
}

/// The tabs among `rows`, in order.
fn row_tabs(rows: &[SettingsRow]) -> Vec<SettingsTab> {
    rows.iter()
        .filter_map(|row| match row {
            SettingsRow::Tab(tab) => Some(*tab),
            SettingsRow::Group(_) => None,
        })
        .collect()
}

/// The tab to show once the list is `rows`: `shown` while the list still
/// offers it, else General.
fn tab_kept(shown: SettingsTab, rows: &[SettingsRow]) -> SettingsTab {
    if rows.contains(&SettingsRow::Tab(shown)) {
        shown
    } else {
        SettingsTab::General
    }
}

/// The General tab's keep-awake choices, in the order they show.
const KEEP_AWAKE_CHOICES: [KeepAwakeMode; 3] = [
    KeepAwakeMode::Off,
    KeepAwakeMode::WhenBusy,
    KeepAwakeMode::Always,
];

/// The General tab's Default view choices, in the order they show.
const SIDEBAR_VIEW_CHOICES: [SidebarView; 2] = [SidebarView::Repos, SidebarView::Tabs];

/// The General tab's Leaf density choices, in the order they show.
const LEAF_DENSITY_CHOICES: [LeafDensity; 2] = [LeafDensity::Comfortable, LeafDensity::Compact];

/// The General tab's Sessions under repos choices, in the order they show.
const SESSIONS_UNDER_REPOS_CHOICES: [SessionsUnderRepos; 3] = [
    SessionsUnderRepos::Every,
    SessionsUnderRepos::Busy,
    SessionsUnderRepos::None,
];

/// The General tab's Sort repos choices, in the order they show.
const REPO_SORT_CHOICES: [RepoSort; 3] = [RepoSort::Manual, RepoSort::Active, RepoSort::NeedsYou];

/// The General tab's Chat view choices, in the order they show.
const CHAT_MODE_CHOICES: [ChatMode; 2] = [ChatMode::Compact, ChatMode::Comfortable];

/// The App title tab's count choices, in the order they show.
const COUNT_MODE_CHOICES: [CountMode; 3] = [CountMode::NeedsYou, CountMode::Busy, CountMode::Idle];

/// The Notifications tab's skip choices, in the order they show.
const NOTIFY_SKIP_CHOICES: [NotifySkip; 3] =
    [NotifySkip::OnScreen, NotifySkip::Focused, NotifySkip::Never];

/// The Notifications tab's toast style choices, in the order they show.
#[cfg(windows)]
const TOAST_STYLE_CHOICES: [ToastStyle; 2] = [ToastStyle::Os, ToastStyle::App];

/// An entry of a sound dropdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SoundEntry {
    Off,
    Sound(BundledSound),
    /// Asks for the file.
    Custom,
}

impl SoundEntry {
    /// Every entry, in the order the list shows them.
    const ALL: [Self; BundledSound::ALL.len() + 2] = {
        let mut entries = [Self::Custom; BundledSound::ALL.len() + 2];
        entries[0] = Self::Off;
        let mut at = 0;
        while at < BundledSound::ALL.len() {
            entries[at + 1] = Self::Sound(BundledSound::ALL[at]);
            at += 1;
        }
        entries
    };

    /// The entry `sound` shows as on `kind`'s row.
    const fn of(sound: &ToastSound, kind: NotifyKind) -> Self {
        match sound {
            ToastSound::Off => Self::Off,
            ToastSound::Default => Self::Sound(kind.default_sound()),
            ToastSound::Bundled(bundled) => Self::Sound(*bundled),
            ToastSound::Custom(_) => Self::Custom,
        }
    }

    /// The entry's words on `kind`'s row.
    fn text(self, kind: NotifyKind) -> String {
        match self {
            Self::Off => "Off".to_owned(),
            Self::Sound(sound) if sound == kind.default_sound() => {
                format!("{} (default)", sound.label())
            }
            Self::Sound(sound) => sound.label().to_owned(),
            Self::Custom => "Custom file…".to_owned(),
        }
    }

    /// The entry's row in the open list.
    fn selector(self) -> String {
        let name = match self {
            Self::Off => "off",
            Self::Sound(sound) => sound.id(),
            Self::Custom => "custom",
        };
        format!("settings-sound-option-{name}")
    }
}

/// An open sound dropdown's list.
pub(crate) struct SoundList {
    /// The row it belongs to.
    kind: NotifyKind,
    /// The place in [`SoundEntry::ALL`] Up, Down and the pointer have reached.
    highlight: usize,
    scroll: ScrollHandle,
    /// Where the field and the window were last laid out, which places the
    /// list; `None` until the first frame.
    measured: Option<SoundListRoom>,
}

/// What a sound list is placed from.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SoundListRoom {
    field: Bounds<Pixels>,
    window_height: Pixels,
}

/// How a row of a sound list draws.
#[derive(Debug, Clone, Copy)]
struct SoundRowLook {
    /// Carries a check.
    chosen: bool,
    highlighted: bool,
}

/// The Default shell row's open list.
pub(crate) struct ShellList {
    /// The place among the host's choices Up, Down and the pointer have
    /// reached.
    highlight: usize,
    scroll: ScrollHandle,
    /// Where the field and the window were last laid out, which places the
    /// list; `None` until the first frame.
    measured: Option<SoundListRoom>,
}

impl std::fmt::Debug for ShellList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellList")
            .field("highlight", &self.highlight)
            .finish_non_exhaustive()
    }
}

/// The Default shell row's Custom command field, for the host it was built
/// for, and its draft.
pub(crate) struct ShellCommandField {
    host: HostKey,
    input: Entity<TextInput>,
    focus: FocusHandle,
    /// The field's text, kept as it changes.
    draft: String,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for ShellCommandField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellCommandField")
            .field("host", &self.host)
            .field("draft", &self.draft)
            .finish_non_exhaustive()
    }
}

/// A control of a Settings tab that Tab can reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsControl {
    KeepAwake(KeepAwakeMode),
    SidebarView(SidebarView),
    LeafDensity(LeafDensity),
    SessionsUnderRepos(SessionsUnderRepos),
    RepoSort(RepoSort),
    CopyOnSelect,
    /// The Default shell row's dropdown, on the shown host.
    DefaultShell,
    /// The Default shell row's Custom command field, while Custom is chosen.
    DefaultShellCommand,
    ChatMode(ChatMode),
    ChatWidth,
    CheckUpdates,
    CheckNow,
    /// The Diagnostics section's Detailed logging switch; This PC only.
    DetailedLogging,
    /// The Diagnostics section's Open logs folder button; This PC only, and
    /// an immediate action, never pending.
    OpenLogsFolder,
    CountMode(CountMode),
    TitleCount,
    TitleSuffix,
    NotifySettingsLink,
    NotifyNeedsYou,
    NeedsYouSoundField,
    NeedsYouSoundPlay,
    FinishedSoundField,
    FinishedSoundPlay,
    NotifyFinished,
    NotifyAccountSwitches,
    NotifySkip(NotifySkip),
    FinishedGrace(u32),
    #[cfg_attr(not(windows), expect(dead_code, reason = "Windows-only toast style"))]
    ToastStyle(ToastStyle),
    /// Choice `choice` of field `field` of the shown agent page's
    /// [`AgentOptions::settings_fields`]; `selector` is
    /// `settings-{tag}-{field id}-{choice}`.
    AgentChoice {
        field: usize,
        choice: usize,
        selector: &'static str,
    },
    /// The toggle of field `field` of the shown agent page; `selector` is
    /// `settings-{tag}-{field id}`.
    AgentToggle {
        field: usize,
        selector: &'static str,
    },
    /// The input of the text or model field with id `field` (interned) of
    /// the shown agent page; `selector` is `settings-{tag}-{field id}`.
    AgentText {
        field: &'static str,
        selector: &'static str,
    },
    WorktreesPath,
    WorktreesBrowse,
    WorktreesReset,
    WorktreesManage,
    RemoteToggle,
    RemotePort,
    RemoteAutostart,
    AgentProgram,
    AgentArgs,
    /// The shown agent page's Hooks switch, held until OK or Apply.
    AgentUserHooks,
    Appearance(AppearanceStop),
    DashboardEnabled,
    /// Provider `index` of the shown host's summarizer status; `selector`
    /// is `settings-dashboard-provider-{id}`.
    DashboardProvider {
        index: usize,
        selector: &'static str,
    },
    DashboardKeySource(KeySourceChoice),
    DashboardKey,
    DashboardKeyClear,
    DashboardEnvVariable,
    DashboardCap,
    DashboardTest,
    /// A control of the Accounts page; `selector` is its
    /// [`AccountsControl::selector_text`], interned.
    Accounts {
        control: AccountsControl,
        selector: &'static str,
    },
    /// The footer's OK: Apply, then close.
    FooterOk,
    /// The footer's Apply: sends every host's pending edits.
    FooterApply,
    /// The footer's Cancel: discards every pending edit and closes.
    FooterCancel,
}

/// The footer's buttons, in the order they show and Tab reaches them.
const FOOTER_CONTROLS: [SettingsControl; 3] = [
    SettingsControl::FooterOk,
    SettingsControl::FooterApply,
    SettingsControl::FooterCancel,
];

/// Where the Dashboard tab reads the summarizer's key from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeySourceChoice {
    Stored,
    Env,
}

/// The Dashboard tab's key source choices, in the order they show.
const KEY_SOURCE_CHOICES: [KeySourceChoice; 2] = [KeySourceChoice::Stored, KeySourceChoice::Env];

impl SettingsControl {
    pub(crate) const fn selector(self) -> &'static str {
        match self {
            Self::KeepAwake(mode) => match mode {
                KeepAwakeMode::Off => "settings-keep-awake-off",
                KeepAwakeMode::WhenBusy => "settings-keep-awake-when-busy",
                KeepAwakeMode::Always => "settings-keep-awake-always",
                KeepAwakeMode::Unknown => "settings-keep-awake-unknown",
            },
            Self::SidebarView(SidebarView::Repos) => "settings-sidebar-view-repos",
            Self::SidebarView(SidebarView::Tabs) => "settings-sidebar-view-tabs",
            Self::LeafDensity(LeafDensity::Comfortable) => "settings-leaf-density-comfortable",
            Self::LeafDensity(LeafDensity::Compact) => "settings-leaf-density-compact",
            Self::SessionsUnderRepos(SessionsUnderRepos::Every) => {
                "settings-sessions-under-repos-every"
            }
            Self::SessionsUnderRepos(SessionsUnderRepos::Busy) => {
                "settings-sessions-under-repos-busy"
            }
            Self::SessionsUnderRepos(SessionsUnderRepos::None) => {
                "settings-sessions-under-repos-none"
            }
            Self::RepoSort(RepoSort::Manual) => "settings-repo-sort-manual",
            Self::RepoSort(RepoSort::Active) => "settings-repo-sort-active",
            Self::RepoSort(RepoSort::NeedsYou) => "settings-repo-sort-needs-you",
            Self::CopyOnSelect => "settings-terminal-copy-on-selection",
            Self::DefaultShell => "settings-default-shell-field",
            Self::DefaultShellCommand => "settings-default-shell-command",
            Self::ChatMode(ChatMode::Compact) => "settings-chat-mode-compact",
            Self::ChatMode(ChatMode::Comfortable) => "settings-chat-mode-comfortable",
            Self::ChatWidth => "settings-chat-width-input",
            Self::CheckUpdates => "settings-general-check-updates",
            Self::CheckNow => "settings-general-check-now",
            Self::DetailedLogging => "settings-detailed-logging",
            Self::OpenLogsFolder => "settings-open-logs-folder",
            Self::CountMode(CountMode::NeedsYou) => "settings-count-needs-you",
            Self::CountMode(CountMode::Busy) => "settings-count-busy",
            Self::CountMode(CountMode::Idle) => "settings-count-idle",
            Self::TitleCount => "settings-title-count",
            Self::TitleSuffix => "settings-title-product-suffix",
            Self::NotifySettingsLink => "settings-open-windows-notifications",
            Self::NotifyNeedsYou => "settings-notify-needs-you",
            Self::NeedsYouSoundField => "settings-needs-you-sound-field",
            Self::NeedsYouSoundPlay => "settings-needs-you-sound-play",
            Self::FinishedSoundField => "settings-finished-sound-field",
            Self::FinishedSoundPlay => "settings-finished-sound-play",
            Self::NotifyFinished => "settings-notify-finished",
            Self::NotifyAccountSwitches => "settings-notify-account-switches",
            Self::NotifySkip(NotifySkip::OnScreen) => "settings-notify-skip-on-screen",
            Self::NotifySkip(NotifySkip::Focused) => "settings-notify-skip-focused",
            Self::NotifySkip(NotifySkip::Never) => "settings-notify-skip-never",
            Self::FinishedGrace(0) => "settings-finished-grace-0",
            Self::FinishedGrace(2) => "settings-finished-grace-2",
            Self::FinishedGrace(5) => "settings-finished-grace-5",
            Self::FinishedGrace(10) => "settings-finished-grace-10",
            // Only the offered holds are built, so no other value reaches here.
            Self::FinishedGrace(_) => "settings-finished-grace-20",
            Self::ToastStyle(ToastStyle::Os) => "settings-toast-style-os",
            Self::ToastStyle(ToastStyle::App) => "settings-toast-style-app",
            Self::AgentChoice { selector, .. }
            | Self::AgentToggle { selector, .. }
            | Self::AgentText { selector, .. }
            | Self::DashboardProvider { selector, .. }
            | Self::Accounts { selector, .. } => selector,
            Self::WorktreesPath => "settings-worktrees-root-input",
            Self::WorktreesBrowse => "settings-worktrees-root-browse",
            Self::WorktreesReset => "settings-worktrees-root-reset",
            Self::WorktreesManage => "settings-worktrees-open-manager",
            Self::RemoteToggle => "settings-remote-toggle",
            Self::RemotePort => "settings-remote-port-input",
            Self::RemoteAutostart => "settings-remote-autostart-toggle",
            Self::AgentProgram => "settings-agent-program",
            Self::AgentArgs => "settings-agent-args",
            Self::AgentUserHooks => "settings-agent-user-hooks",
            Self::Appearance(stop) => stop.selector(),
            Self::DashboardEnabled => "settings-dashboard-enabled",
            Self::DashboardKeySource(KeySourceChoice::Stored) => {
                "settings-dashboard-key-source-stored"
            }
            Self::DashboardKeySource(KeySourceChoice::Env) => "settings-dashboard-key-source-env",
            Self::DashboardKey => "settings-dashboard-key-field",
            Self::DashboardKeyClear => "settings-dashboard-key-clear",
            Self::DashboardEnvVariable => "settings-dashboard-env-variable",
            Self::DashboardCap => "settings-dashboard-cap",
            Self::DashboardTest => "settings-dashboard-test",
            Self::FooterOk => "settings-ok",
            Self::FooterApply => "settings-apply",
            Self::FooterCancel => "settings-cancel",
        }
    }

    /// The Appearance stop this control is, if it is one.
    pub(crate) const fn appearance_stop(self) -> Option<AppearanceStop> {
        match self {
            Self::Appearance(stop) => Some(stop),
            _ => None,
        }
    }
}

/// The Worktrees tab's path field, which holds the user's override only,
/// and the save it waits to hear back.
pub(crate) struct WorktreesTab {
    input: Entity<TextInput>,
    focus: FocusHandle,
    /// The field's text, kept as it changes.
    draft: String,
    /// How many sends of the path still owe the daemon's echo.
    awaiting: u32,
    /// The trimmed path the last send carried, empty for no override.
    sent: Option<String>,
    _subscriptions: Vec<Subscription>,
}

/// The Remote access page's port field and this PC's login launch, as the
/// page last read it.
pub(crate) struct RemoteAccessTab {
    input: Entity<TextInput>,
    focus: FocusHandle,
    /// The field's text, kept as it changes.
    draft: String,
    /// Whether the daemon starts on login, or why that could not be read.
    autostart: Result<bool, String>,
    /// Why the last switch of the login launch failed.
    autostart_error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

/// A text field of the Dashboard tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DashboardField {
    Key,
    Variable,
    Cap,
}

/// One host's Dashboard tab key, variable and cap fields.
pub(crate) struct DashboardSettingsTab {
    key: Entity<TextInput>,
    key_focus: FocusHandle,
    variable: Entity<TextInput>,
    variable_focus: FocusHandle,
    cap: Entity<TextInput>,
    cap_focus: FocusHandle,
    /// The fields' texts, kept as they change.
    key_draft: String,
    variable_draft: String,
    cap_draft: String,
    _subscriptions: Vec<Subscription>,
}

impl DashboardSettingsTab {
    fn draft_mut(&mut self, field: DashboardField) -> &mut String {
        match field {
            DashboardField::Key => &mut self.key_draft,
            DashboardField::Variable => &mut self.variable_draft,
            DashboardField::Cap => &mut self.cap_draft,
        }
    }

    /// The daemon's settings went from `old` to `new`: the cap and variable
    /// fields take the new values while they still read as the old ones,
    /// and once the daemon reads a variable the shown key source is its.
    fn follow(
        &mut self,
        old: Option<&SummarizerSettings>,
        new: Option<&SummarizerSettings>,
        cx: &mut Context<RootView>,
    ) {
        let cap_untouched = self.cap_draft.trim().is_empty()
            || old.is_some_and(|old| parse_cap(&self.cap_draft) == Some(old.daily_cap_usd));
        if cap_untouched {
            self.cap_draft = new.map_or_else(String::new, |settings| usd(settings.daily_cap_usd));
            let text = self.cap_draft.clone();
            self.cap.update(cx, |input, cx| input.set_text(text, cx));
        }
        let (old_variable, new_variable) = (env_variable(old), env_variable(new));
        let untouched = self.variable_draft.trim().is_empty()
            || old_variable.as_deref() == Some(self.variable_draft.as_str());
        if untouched && let Some(variable) = new_variable {
            self.variable_draft.clone_from(&variable);
            self.variable
                .update(cx, |input, cx| input.set_text(variable, cx));
        }
    }
}

/// The variable `settings` read the key from, when they read one.
fn env_variable(settings: Option<&SummarizerSettings>) -> Option<String> {
    match &settings?.key_source {
        SummarizerKeySource::Env { variable } => Some(variable.clone()),
        SummarizerKeySource::Stored | SummarizerKeySource::Unknown => None,
    }
}

/// `amount` in US dollars to the cent, without the sign.
fn usd(amount: f64) -> String {
    format!("{amount:.2}")
}

const CAP_PROBLEM: &str = "Enter a number of dollars, 0 or more.";

/// The words the Dashboard tab shows for a key's state.
const fn key_state_words(state: KeyState) -> &'static str {
    match state {
        KeyState::Missing => "No key",
        KeyState::Stored => "Key stored",
        KeyState::EnvFound => "Variable found",
        KeyState::EnvMissing => "Variable not set",
        KeyState::Rejected => "Key rejected",
        KeyState::Unreachable => "Credential Manager unavailable on this host",
        KeyState::Unknown => "Key state unknown",
    }
}

/// The line under the cap: today's spend against the cap, and whether the
/// cap stopped summaries.
fn spend_line(status: &SummarizerStatus) -> String {
    let line = format!(
        "Spent today: ${} of ${}",
        usd(status.spend_today_usd),
        usd(status.settings.daily_cap_usd)
    );
    if status.cap_reached {
        format!("{line} — cap reached, summaries paused until tomorrow")
    } else {
        line
    }
}

/// The Test result slot's text.
fn test_text(outcome: &TestOutcome) -> String {
    match outcome {
        TestOutcome::Waiting => "Testing…".to_owned(),
        TestOutcome::Failed(message) => message.clone(),
        TestOutcome::Reply {
            result,
            latency_ms,
            cost_usd,
        } => {
            let source = match result.source {
                SummarySource::Model => "Model",
                SummarySource::Template => "Template",
                SummarySource::Unknown => "Unknown source",
            };
            format!(
                "{} — {}\n{source} · {latency_ms} ms · ${cost_usd:.4}",
                result.headline, result.notification
            )
        }
    }
}

/// A row of an agent page's Launch command section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchCommandRow {
    Program,
    Args,
}

/// The row a daemon refusal of a launch command belongs under, by its error
/// `code`, or `None` when it is no launch-command refusal.
fn launch_refusal_row(code: Option<&str>) -> Option<LaunchCommandRow> {
    match code? {
        error_codes::LAUNCH_PROGRAM_REFUSED => Some(LaunchCommandRow::Program),
        error_codes::LAUNCH_ARGS_REFUSED => Some(LaunchCommandRow::Args),
        _ => None,
    }
}

/// One agent page's launch-command fields and text fields, and the apply
/// they wait to hear back.
pub(crate) struct AgentTab {
    program: Entity<TextInput>,
    program_focus: FocusHandle,
    args: Entity<TextInput>,
    args_focus: FocusHandle,
    /// The fields' texts, kept as they change.
    program_draft: String,
    args_draft: String,
    /// How many applies still owe the daemon's echo or refusal.
    awaiting: u32,
    /// The `(program, args)` the last apply sent.
    sent: Option<(String, String)>,
    /// The daemon echoed the last apply.
    saved: bool,
    /// The daemon's refusal of the last apply, under the row it names.
    refusal: Option<(LaunchCommandRow, String)>,
    /// The page's text and model fields, one per such field its descriptor
    /// marks for Settings.
    texts: Vec<SettingText>,
    _subscriptions: Vec<Subscription>,
}

/// A text or model field's input on an agent page.
struct SettingText {
    field: String,
    input: Entity<TextInput>,
    focus: FocusHandle,
    /// The input's text, kept as it changes.
    draft: String,
    /// The user edited the text since it was last applied or filled.
    edited: bool,
    /// The placeholder the input shows, as the page's values set it.
    placeholder: String,
}

/// What a [`SettingText`] of agent `agent`'s page shows anew: its value
/// when `text` is set, its placeholder when `placeholder` is.
struct TextRefresh {
    agent: AgentId,
    field: String,
    text: Option<String>,
    placeholder: Option<String>,
}

/// What a new [`SettingText`] starts with.
struct TextSeed {
    field: String,
    draft: String,
    placeholder: String,
}

/// How many sent Settings values [`SettingApply`] keeps waiting for a
/// refusal; an accepted apply is never answered by id, so older ones go.
const WAITING_SETTING_APPLIES: usize = 32;

/// Settings values an agent page sent, kept until the refusal carrying `id`
/// answers them, a disconnect, or [`WAITING_SETTING_APPLIES`] newer ones.
pub(crate) struct SettingApply {
    id: String,
    agent: AgentId,
    /// The option the apply changed, and the value it set it to.
    key: String,
    value: OptionValue,
    /// The apply came from text field `key`.
    from_text: bool,
}

/// The agent pages' state while Settings is open: each page's fields, kept
/// while another tab shows, the agent page last selected, and the request
/// ids and agents of the launch-command applies that still owe the
/// daemon's answer, oldest first.
#[derive(Default)]
pub(crate) struct HostAgentTabs {
    tabs: BTreeMap<AgentId, AgentTab>,
    current: Option<AgentId>,
    applies: VecDeque<(String, AgentId)>,
}

impl HostAgentTabs {
    /// Drops the pages of the agents `agents` no longer lists, and their
    /// applies from the queue.
    fn keep_listed(&mut self, agents: &[AgentDescriptor]) {
        self.tabs
            .retain(|agent, _| descriptor_for(agents, agent).is_some());
        self.applies
            .retain(|(_, agent)| descriptor_for(agents, agent).is_some());
        if self
            .current
            .as_ref()
            .is_some_and(|agent| descriptor_for(agents, agent).is_none())
        {
            self.current = None;
        }
    }

    /// A lost connection: no answer to an apply in flight can arrive, so
    /// the queue empties and every page's apply returns to unsent; the
    /// pages and their drafts stay.
    pub(crate) fn forget_applies(&mut self) {
        self.applies.clear();
        for tab in self.tabs.values_mut() {
            tab.awaiting = 0;
            tab.sent = None;
            tab.saved = false;
        }
    }
}

/// The agent pages' state of each host whose pages Settings has shown, so
/// moving the host selector keeps every host's drafts and applies apart.
#[derive(Default)]
pub(crate) struct AgentTabs {
    hosts: HashMap<HostKey, HostAgentTabs>,
}

impl AgentTabs {
    /// `host`'s agent pages, once Settings has shown one of them.
    fn of(&self, host: &HostKey) -> Option<&HostAgentTabs> {
        self.hosts.get(host)
    }

    /// `host`'s agent pages, to update, once Settings has shown one.
    fn of_mut(&mut self, host: &HostKey) -> Option<&mut HostAgentTabs> {
        self.hosts.get_mut(host)
    }

    /// `host`'s agent pages, to update, made empty when it has none yet.
    fn entry(&mut self, host: &HostKey) -> &mut HostAgentTabs {
        self.hosts.entry(host.clone()).or_default()
    }

    /// Every host's agent pages.
    fn all(&self) -> impl Iterator<Item = (&HostKey, &HostAgentTabs)> {
        self.hosts.iter()
    }

    /// Drops `host`'s agent pages, their drafts and their applies; every
    /// other host's stay.
    pub(crate) fn drop_host(&mut self, host: &HostKey) {
        self.hosts.remove(host);
    }

    /// `host`'s connection was lost: its applies in flight are forgotten as
    /// [`HostAgentTabs::forget_applies`] says; every other host's stay.
    pub(crate) fn forget_applies(&mut self, host: &HostKey) {
        if let Some(tabs) = self.of_mut(host) {
            tabs.forget_applies();
        }
    }
}

/// The host Settings shows once `closed`'s connection closed: This PC when
/// `closed` was the one shown, else the one shown.
pub(crate) fn settings_host_after_close(shown: &HostKey, closed: &HostKey) -> HostKey {
    if shown == closed {
        HostKey::Local
    } else {
        shown.clone()
    }
}

/// The launch commands the daemon broadcast, as one page reads them.
struct CommandEcho {
    /// The page's command before and after the broadcast.
    old: Option<AgentCommandEntry>,
    stored: Option<AgentCommandEntry>,
    read_only: bool,
    /// The broadcast answers this page's oldest apply.
    answers: bool,
}

impl AgentTab {
    /// Puts `text` in `row`'s field and draft.
    fn fill(&mut self, row: LaunchCommandRow, text: String, cx: &mut Context<RootView>) {
        let (draft, input) = match row {
            LaunchCommandRow::Program => (&mut self.program_draft, &self.program),
            LaunchCommandRow::Args => (&mut self.args_draft, &self.args),
        };
        draft.clone_from(&text);
        input.update(cx, |input, cx| input.set_text(text, cx));
    }

    /// The daemon's launch commands: an echo answering the page's last
    /// apply puts the stored command in each field that still reads as the
    /// apply sent it, and shows Saved when both do. An echo answering none
    /// is another client's change: it fills a field only while that field
    /// still reads as the daemon left it.
    fn on_echo(&mut self, echo: CommandEcho, cx: &mut Context<RootView>) {
        let read_only = echo.read_only;
        self.program
            .update(cx, |input, cx| input.set_read_only(read_only, cx));
        let mine = echo.answers && self.awaiting > 0;
        if mine {
            self.awaiting -= 1;
        }
        let settles = mine && self.awaiting == 0;
        let (old_program, old_args) = echo
            .old
            .map_or_else(Default::default, |entry| (entry.program, entry.args));
        let (program, args) = echo
            .stored
            .map_or_else(Default::default, |entry| (entry.program, entry.args));
        let (sent_program, sent_args) = if settles {
            self.sent.take().unwrap_or_default()
        } else {
            Default::default()
        };
        let program_as_sent = settles && self.program_draft.trim() == sent_program;
        let args_as_sent = settles && self.args_draft.trim() == sent_args;
        self.saved = program_as_sent && args_as_sent;
        if program_as_sent || (!mine && self.program_draft == old_program) {
            self.fill(LaunchCommandRow::Program, program, cx);
        }
        if args_as_sent || (!mine && self.args_draft == old_args) {
            self.fill(LaunchCommandRow::Args, args, cx);
        }
    }
}

/// The daemon's keep-awake mode and whether it holds the machine awake now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeepAwake {
    pub(crate) mode: KeepAwakeMode,
    pub(crate) active: bool,
}

/// The line under the keep-awake choices, and their locked note while the
/// daemon is not connected.
const fn keep_awake_status(state: Option<KeepAwake>) -> &'static str {
    match state {
        None => "(daemon not connected)",
        Some(KeepAwake {
            mode: KeepAwakeMode::Off,
            ..
        }) => "The machine may sleep while sessions run.",
        Some(KeepAwake {
            mode: KeepAwakeMode::WhenBusy,
            active: true,
        }) => "Holding the machine awake — a session is busy.",
        Some(KeepAwake {
            mode: KeepAwakeMode::WhenBusy,
            active: false,
        }) => "Sleep is allowed while no session is busy.",
        Some(KeepAwake { active: true, .. }) => "Holding the machine awake — a session is live.",
        Some(KeepAwake { active: false, .. }) => "Sleep is allowed until a session starts.",
    }
}

/// A keep-awake choice's text.
const fn keep_awake_choice_text(mode: KeepAwakeMode) -> &'static str {
    match mode {
        KeepAwakeMode::Off => "Off",
        KeepAwakeMode::WhenBusy => "When busy",
        KeepAwakeMode::Always => "Always",
        KeepAwakeMode::Unknown => "Unknown",
    }
}

impl RootView {
    /// Whether the Settings modal is open.
    #[must_use]
    pub fn settings_open(&self) -> bool {
        self.appearance_editor
            .as_ref()
            .is_some_and(|editor| editor.level == Level::App)
    }

    /// The label of the choice the Settings page of the agent tagged `tag`
    /// shows selected in its field `field`; `None` when the daemon lists no
    /// such agent, its page has no such field, or the field is no choice.
    #[must_use]
    pub fn settings_field_choice(&self, tag: &str, field: &str) -> Option<String> {
        let view = self.settings_page_field(tag, field)?;
        let FieldEditor::Choice { choices, .. } = &view.editor else {
            return None;
        };
        choices
            .iter()
            .find(|choice| choice.value == view.value)
            .map(|choice| choice.label.clone())
    }

    /// The note the Settings page of the agent tagged `tag` shows for its
    /// field `field`: the update note for a kind this client does not know,
    /// else `None`.
    #[must_use]
    pub fn settings_field_note(&self, tag: &str, field: &str) -> Option<&'static str> {
        let view = self.settings_page_field(tag, field)?;
        (view.editor == FieldEditor::Unknown).then_some(UPDATE_CLIENT)
    }

    /// The ids of the fields the Settings page of the agent tagged `tag`
    /// draws, in order; empty while the daemon lists no such agent.
    #[must_use]
    pub fn settings_page_fields(&self, tag: &str) -> Vec<String> {
        self.agents()
            .iter()
            .find(|agent| agent.tag == tag)
            .and_then(|descriptor| self.agent_page(&self.settings_host, &descriptor.id))
            .map(|page| page.fields.into_iter().map(|view| view.id).collect())
            .unwrap_or_default()
    }

    /// The placeholder the input of text or model field `field` shows on
    /// the Settings page of the agent tagged `tag`, once that page was
    /// shown.
    #[must_use]
    pub fn settings_text_placeholder(&self, tag: &str, field: &str) -> Option<String> {
        let descriptor = self.agents().iter().find(|agent| agent.tag == tag)?;
        self.shown_agent_tabs()?
            .tabs
            .get(&descriptor.id)?
            .texts
            .iter()
            .find(|setting| setting.field == field)
            .map(|setting| setting.placeholder.clone())
    }

    fn settings_page_field(&self, tag: &str, field: &str) -> Option<FieldView> {
        let descriptor = self.agents().iter().find(|agent| agent.tag == tag)?;
        self.agent_page(&self.settings_host, &descriptor.id)?
            .fields
            .into_iter()
            .find(|view| view.id == field)
    }

    /// The Settings page of `host`'s agent `id` as its daemon last
    /// described it and broadcast its values; `None` while that daemon
    /// lists no such agent.
    fn agent_page(&self, host: &HostKey, id: &AgentId) -> Option<AgentPage> {
        let mut page = self.saved_agent_page(host, id)?;
        for view in &mut page.fields {
            let pending = self
                .settings_pending
                .host(host)
                .iter()
                .find_map(|value| match value {
                    HostPending::AgentField {
                        agent,
                        field,
                        value,
                    } if agent == id && *field == view.id => Some(value.clone()),
                    _ => None,
                });
            if let Some(value) = pending {
                view.value = value;
            }
        }
        Some(page)
    }

    /// The page as [`Self::agent_page`] draws it, without the toggles and
    /// choices pending on it.
    fn saved_agent_page(&self, host: &HostKey, id: &AgentId) -> Option<AgentPage> {
        let descriptor = descriptor_for(self.sidebar.agents(host), id)?;
        let values = self
            .mirrors_of(host)
            .and_then(|mirrors| mirrors.agent_settings.get(id));
        let options = AgentOptions::new(descriptor.clone(), values);
        Some(AgentPage {
            id: descriptor.id.clone(),
            tag: descriptor.tag.clone(),
            label: descriptor.label.clone(),
            fields: options.settings_fields(),
        })
    }

    /// The page of the agent whose tab is shown.
    fn shown_page(&self) -> Option<AgentPage> {
        self.agent_page(&self.settings_host, &self.settings_tab.agent()?)
    }

    /// The agent pages of the host the selector shows, once Settings has
    /// shown one of them.
    fn shown_agent_tabs(&self) -> Option<&HostAgentTabs> {
        self.agent_tabs.of(&self.settings_host)
    }

    /// The host whose agent pages Settings shows.
    #[must_use]
    pub fn settings_host(&self) -> &HostKey {
        &self.settings_host
    }

    /// The host selector's rows: each held host and whether it can be
    /// chosen (a remote host only while its connection is open); empty
    /// while the window holds one host, when no selector is drawn.
    #[must_use]
    pub fn settings_host_choices(&self) -> Vec<(HostKey, bool)> {
        let held = self.held_hosts();
        if held.len() < 2 {
            return Vec::new();
        }
        held.into_iter()
            .map(|host| {
                let open = host == HostKey::Local
                    || self.conn_of(&host).is_some_and(crate::Connection::is_open);
                (host, open)
            })
            .collect()
    }

    /// The version `host`'s daemon reported in its `Welcome`, and whether it
    /// is older than this client: `None` for a host that reported none, or
    /// one that does not parse.
    fn settings_host_version(&self, host: &HostKey) -> Option<(String, bool)> {
        let version = self.conn_of(host)?.build_version()?;
        crate::connection_picker::version_against(Some(version), protocol::BUILD_VERSION)
    }

    /// A click on the selector's row for `host`: the agent pages show
    /// `host`'s, the shown tab kept while `host` offers it. A host not
    /// connected is not chosen.
    fn choose_settings_host(&mut self, host: HostKey, window: &mut Window, cx: &mut Context<Self>) {
        let open = self
            .settings_host_choices()
            .into_iter()
            .any(|(choice, open)| choice == host && open);
        if !open || host == self.settings_host {
            return;
        }
        tracing::info!(?host, "settings: showing the host's agent pages");
        self.show_settings_host(host, window, cx);
        // The new host draws other rows: the search follows them, so the
        // shown tab is a listed one and the highlight moves with it.
        self.settle_search(window, cx);
    }

    /// The selector moves to `host`: the shown tab stays while `host`
    /// offers it, else General; a shown agent, Worktrees or Dashboard page
    /// holds `host`'s page state, built as selecting the tab would when
    /// `host` has none. Every host's drafts stay.
    fn show_settings_host(&mut self, host: HostKey, window: &mut Window, cx: &mut Context<Self>) {
        if host != self.settings_host {
            self.drop_account_alias_fields();
        }
        self.settings_host = host;
        self.keep_offered_tab();
        self.settings_control = None;
        self.notifications.sound_list = None;
        self.settings_pending.shell_list = None;
        if self.settings_open() {
            if self.settings_tab == SettingsTab::General {
                self.show_shell_row(window, cx);
            }
            if let Some(agent) = self.settings_tab.agent() {
                self.hold_agent_tab(agent, window, cx);
            }
            if self.settings_tab == SettingsTab::Worktrees {
                self.hold_worktrees_tab(window, cx);
            }
            if self.settings_tab == SettingsTab::Dashboard {
                self.hold_dashboard_tab(window, cx);
            }
            if self.settings_tab == SettingsTab::Accounts {
                self.on_accounts_tab_selected(window, cx);
            }
        }
        cx.notify();
    }

    /// `host`'s connection closed: a remote host's agent, Worktrees and
    /// Dashboard pages and their drafts go, and the selector returns to
    /// This PC when it showed `host`.
    pub(crate) fn settings_host_closed(
        &mut self,
        host: &HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if *host != HostKey::Local {
            self.agent_tabs.drop_host(host);
            self.worktrees_tabs.remove(host);
            self.dashboard_settings_tabs.remove(host);
            self.settings_pending.take_host(host);
        }
        // OK no longer hears the closed host's echoes: Settings stays open,
        // its drafts kept.
        if self
            .settings_pending
            .ok_wait
            .as_ref()
            .is_some_and(|wait| wait.iter().any(|owed| owed.host == *host))
        {
            tracing::info!(?host, "settings: a host OK waited on closed; OK stays open");
            self.settings_pending.ok_wait = None;
        }
        let shown = settings_host_after_close(&self.settings_host, host);
        if shown != self.settings_host {
            tracing::info!(
                ?host,
                "settings: the shown host's connection closed; showing This PC"
            );
            self.show_settings_host(shown, window, cx);
            // The keyboard may have been in one of the closed host's inputs,
            // which went with its pages.
            if self.settings_open() {
                self.settings_tabs_focus.focus(window);
            }
        }
    }

    /// The Settings tabs' labels, in the list's order.
    #[must_use]
    pub fn settings_tab_labels(&self) -> Vec<&'static str> {
        let agents = self.agents();
        self.settings_tabs()
            .into_iter()
            .map(|tab| tab.label(agents))
            .collect()
    }

    /// Every label the Settings tab list draws, group rows included.
    #[must_use]
    pub fn settings_tab_rows(&self) -> Vec<&'static str> {
        let agents = self.agents();
        self.settings_rows()
            .into_iter()
            .map(|row| row.label(agents))
            .collect()
    }

    /// The Settings search box's text; empty while Settings is closed.
    #[must_use]
    pub fn settings_search_text(&self, cx: &App) -> String {
        self.settings_search
            .as_ref()
            .map(|search| search.input.read(cx).text().to_owned())
            .unwrap_or_default()
    }

    /// Every row the Settings tab list draws: while the search box holds a
    /// word, each listed tab's label followed by two spaces and its count,
    /// or the one line saying nothing matches.
    #[must_use]
    pub fn settings_search_rows(&self, cx: &App) -> Vec<String> {
        let agents = self.agents();
        let Some(hits) = self.search_hits(cx) else {
            return self
                .settings_tab_rows()
                .into_iter()
                .map(str::to_owned)
                .collect();
        };
        if hits.is_empty() {
            return vec![NO_MATCH.to_owned()];
        }
        self.search_rows(&hits)
            .into_iter()
            .map(|(row, count)| match count {
                Some(count) => format!("{}  {count}", row.label(agents)),
                None => row.label(agents).to_owned(),
            })
            .collect()
    }

    /// The selector of the setting the search highlights: the shown tab's
    /// first match, while the query holds a word and Settings is open.
    #[must_use]
    pub fn settings_search_hit(&self, cx: &App) -> Option<&'static str> {
        self.search_hit(cx)
    }

    /// Whether the search catalogue names `selector`, a control of the
    /// shown host's Settings, or a reviewed exemption covers it.
    #[must_use]
    pub fn settings_search_names(&self, selector: &str) -> bool {
        settings_search::names(selector, self.agents())
    }

    /// The query, while the search box holds a word.
    fn search_query(&self, cx: &App) -> Option<String> {
        Some(self.settings_search_text(cx)).filter(|query| !query.trim().is_empty())
    }

    /// The rows the search covers now, and the tabs that hold a match for
    /// the query, while it holds a word.
    fn search_found(&self, cx: &App) -> Option<(Vec<settings_search::Entry>, Vec<TabHits>)> {
        let query = self.search_query(cx)?;
        let entries = settings_search::drawn(
            settings_search::entries(self.agents()),
            &self.drawn_tabs(cx),
        );
        let visible = Visibility {
            remote_tab: self.settings_tabs().contains(&SettingsTab::RemoteAccess),
            this_pc: self.settings_host == HostKey::Local,
        };
        let hits = settings_search::results(&query, &entries, visible);
        Some((entries, hits))
    }

    /// The tabs that hold a match, while the query holds a word.
    fn search_hits(&self, cx: &App) -> Option<Vec<TabHits>> {
        self.search_found(cx).map(|(_, hits)| hits)
    }

    /// The tabs the list shows: the listed ones while searching, else all.
    fn listed_tabs(&self, cx: &App) -> Vec<SettingsTab> {
        match self.search_hits(cx) {
            Some(hits) => self
                .settings_tabs()
                .into_iter()
                .filter(|tab| hits.iter().any(|hit| hit.tab == *tab))
                .collect(),
            None => self.settings_tabs(),
        }
    }

    /// The tab list's rows for `hits`, in the list's order, each listed tab
    /// with its count; the Agents group row stays while one of its pages
    /// is listed.
    fn search_rows(&self, hits: &[TabHits]) -> Vec<(SettingsRow, Option<usize>)> {
        let agent_listed = hits
            .iter()
            .any(|hit| matches!(hit.tab, SettingsTab::Agent(_)));
        self.settings_rows()
            .into_iter()
            .filter_map(|row| match row {
                SettingsRow::Group(_) => agent_listed.then_some((row, None)),
                SettingsRow::Tab(tab) => hits
                    .iter()
                    .find(|hit| hit.tab == tab)
                    .map(|hit| (row, Some(hit.count))),
            })
            .collect()
    }

    /// The selector of the shown tab's first match, while searching; a
    /// Provider hit names the first provider control drawn.
    fn search_hit(&self, cx: &App) -> Option<&'static str> {
        if !self.settings_open() {
            return None;
        }
        let (entries, hits) = self.search_found(cx)?;
        let hit = hits.iter().find(|hit| hit.tab == self.settings_tab)?;
        let selector = entries.get(hit.first)?.selector.as_ref();
        if selector == DASHBOARD_PROVIDER_ROW {
            let provider = format!("{DASHBOARD_PROVIDER_ROW}-");
            return self
                .drawn_stops_of(SettingsTab::Dashboard, cx)
                .into_iter()
                .map(SettingsControl::selector)
                .find(|stop| stop.starts_with(&provider));
        }
        Some(intern(selector.to_owned()))
    }

    /// What each listed tab draws now: its stop order, and the rows no
    /// control of their own names.
    fn drawn_tabs(&self, cx: &App) -> Vec<DrawnTab> {
        self.settings_tabs()
            .into_iter()
            .map(|tab| {
                let stops: Vec<&'static str> = self
                    .drawn_stops_of(tab, cx)
                    .into_iter()
                    .map(SettingsControl::selector)
                    .collect();
                let rows = self.control_free_rows(tab, &stops);
                DrawnTab { tab, stops, rows }
            })
            .collect()
    }

    /// `tab`'s stops its page draws in the current state, whether the ring
    /// holds them now or not, under the same conditions as the page's draw
    /// code: the Dashboard's key rows by the provider and the key source,
    /// the Worktrees buttons by the host, and an agent page's fields by
    /// their `when` rules over the saved values.
    fn drawn_stops_of(&self, tab: SettingsTab, cx: &App) -> Vec<SettingsControl> {
        match tab {
            SettingsTab::Dashboard => self.dashboard_drawn(),
            SettingsTab::Worktrees => {
                let mut controls = vec![SettingsControl::WorktreesPath];
                if self.worktrees_browses() {
                    controls.push(SettingsControl::WorktreesBrowse);
                }
                // Drawn always, dimmed while there is no override to reset.
                controls.push(SettingsControl::WorktreesReset);
                if self.worktrees_manages() {
                    controls.push(SettingsControl::WorktreesManage);
                }
                controls
            }
            SettingsTab::Agent(_) => {
                let Some(agent) = tab.agent() else {
                    return Vec::new();
                };
                let Some(descriptor) = descriptor_for(self.agents(), &agent) else {
                    return Vec::new();
                };
                let options =
                    AgentOptions::new(descriptor.clone(), self.agent_settings().get(&agent));
                // Only the selectors are compared, so the indices among the
                // drawn fields serve.
                options
                    .settings_fields()
                    .iter()
                    .enumerate()
                    .flat_map(|(index, view)| field_controls(&descriptor.tag, index, view))
                    .chain([SettingsControl::AgentProgram, SettingsControl::AgentArgs])
                    .collect()
            }
            _ => self.tab_stop_order_of(tab, cx),
        }
    }

    /// The ids of `tab`'s drawn rows that no control of their own names:
    /// the Dashboard's Provider row while a provider is drawn (`stops`),
    /// and an agent page's fields of a kind this client draws no control
    /// for.
    fn control_free_rows(&self, tab: SettingsTab, stops: &[&str]) -> Vec<String> {
        match tab {
            SettingsTab::Dashboard => {
                let provider = format!("{DASHBOARD_PROVIDER_ROW}-");
                stops
                    .iter()
                    .any(|stop| stop.starts_with(&provider))
                    .then(|| DASHBOARD_PROVIDER_ROW.to_owned())
                    .into_iter()
                    .collect()
            }
            SettingsTab::Agent(_) => {
                let Some(agent) = tab.agent() else {
                    return Vec::new();
                };
                let Some(descriptor) = descriptor_for(self.agents(), &agent) else {
                    return Vec::new();
                };
                let options =
                    AgentOptions::new(descriptor.clone(), self.agent_settings().get(&agent));
                options
                    .settings_fields()
                    .iter()
                    .enumerate()
                    .filter(|(index, view)| {
                        field_controls(&descriptor.tag, *index, view).is_empty()
                    })
                    .map(|(_, view)| {
                        format!("settings-{}-{}", descriptor.tag, field_part(&view.id))
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// Whether the search box has the keyboard.
    fn settings_search_focused(&self, window: &Window) -> bool {
        self.settings_open()
            && self
                .settings_search
                .as_ref()
                .is_some_and(|search| search.focus.is_focused(window))
    }

    /// Ctrl+F, Tab or Shift+Tab: the keyboard goes to the search box,
    /// its text selected with `select`.
    fn focus_settings_search(&mut self, select: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(search) = &self.settings_search else {
            return;
        };
        self.settings_control = None;
        search.focus.focus(window);
        if select {
            window.dispatch_action(Box::new(crate::text_input::SelectAll), cx);
        }
        cx.notify();
    }

    /// The search box, built afresh: empty, its keys and edits subscribed.
    fn new_settings_search(window: &mut Window, cx: &mut Context<Self>) -> SettingsSearch {
        let input = cx.new(|cx| TextInput::new(String::new(), SEARCH_PLACEHOLDER.to_owned(), cx));
        let focus = input.read(cx).focus_handle(cx);
        let keys = cx.subscribe_in(
            &input,
            window,
            |this, input, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => this.focus_search_hit(window, cx),
                // Esc clears a query first; from an empty box it closes
                // Settings, as from any other field.
                TextInputEvent::Cancel => {
                    if input.read(cx).text().is_empty() {
                        this.close_appearance_editor(window, cx);
                    } else {
                        input.update(cx, |input, cx| input.set_text("", cx));
                        this.follow_search(window, cx);
                    }
                }
            },
        );
        let edits = cx.subscribe_in(&input, window, |this, _, _: &TextChanged, window, cx| {
            this.follow_search(window, cx);
        });
        SettingsSearch {
            input,
            focus,
            reveal: false,
            _subscriptions: vec![keys, edits],
        }
    }

    /// After the query changed: the shown tab stays while it is listed,
    /// else the first listed tab shows, and either way its first match
    /// scrolls into view. The keyboard stays in the box.
    fn follow_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settle_search(window, cx) {
            self.focus_settings_search(false, window, cx);
        }
    }

    /// The shown tab stays while the search lists it, else the first
    /// listed tab shows, and either way its first match scrolls into view;
    /// returns whether another tab was selected.
    fn settle_search(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some(search) = &mut self.settings_search {
            search.reveal = true;
        }
        let first = self.search_hits(cx).and_then(|hits| {
            let shown = hits.iter().any(|hit| hit.tab == self.settings_tab);
            hits.first().filter(|_| !shown).map(|hit| hit.tab)
        });
        if let Some(tab) = first {
            self.select_settings_tab(tab, window, cx);
        }
        cx.notify();
        first.is_some()
    }

    /// Enter in the search box: the keyboard goes to the highlighted
    /// control, as Tab would land on it, without pressing it.
    fn focus_search_hit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hit) = self.search_hit(cx) else {
            return;
        };
        let control = self
            .settings_controls(cx)
            .into_iter()
            .find(|control| control.selector() == hit);
        if let Some(control) = control {
            self.move_settings_focus(control, window, cx);
            cx.notify();
        }
    }

    /// The search hit's outline was laid out at `row`: when the search
    /// asked, the content scrolls to show it.
    fn reveal_search_hit(
        &mut self,
        row: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(search) = &mut self.settings_search else {
            return;
        };
        if !std::mem::take(&mut search.reveal) {
            return;
        }
        let handle = self.settings_scroll.handle();
        let viewport = handle.bounds();
        let offset = handle.offset();
        let y = hit_offset(
            viewport.top()..viewport.bottom(),
            row.top()..row.bottom(),
            offset.y,
            handle.max_offset().height,
        );
        if y != offset.y {
            handle.set_offset(point(offset.x, y));
            cx.defer_in(window, |_, window, _| window.refresh());
        }
    }

    /// The search box over the tab list, as wide as the column.
    fn settings_search_field(&self, window: &Window, cx: &mut Context<Self>) -> Option<Div> {
        let search = self.settings_search.as_ref()?;
        let focused = search.focus.is_focused(window);
        // The tab list under it is already the modal inset, so the search box keeps the ground fill to show.
        let frame = field_frame(focused, Layer::Base, &theme(cx))
            .driver_id(|| SEARCH_SELECTOR.to_owned())
            .w_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.focus_settings_search(false, window, cx);
                }),
            )
            .child(search.input.clone());
        Some(div().flex().flex_none().pb(px(8.0)).child(frame))
    }

    /// The tabs the list shows on this target, in order.
    fn settings_tabs(&self) -> Vec<SettingsTab> {
        row_tabs(&self.settings_rows())
    }

    /// Shows the offered tab labelled `label`, as its row does; a label no
    /// tab has changes nothing.
    pub fn show_settings_tab(&mut self, label: &str, window: &mut Window, cx: &mut Context<Self>) {
        let agents = self.agents();
        let tab = self
            .settings_tabs()
            .into_iter()
            .find(|tab| tab.label(agents) == label);
        if let Some(tab) = tab {
            self.select_settings_tab(tab, window, cx);
        }
    }

    /// The rows the tab list draws for the host Settings shows, in order:
    /// Remote access configures this PC's daemon, so it is offered only
    /// where the client manages it, Accounts there too and only while its
    /// daemon lists the accounts feature, and an agent's page only while
    /// the daemon lists it.
    fn settings_rows(&self) -> Vec<SettingsRow> {
        tab_rows(
            self.agents(),
            self.capabilities_of(&self.settings_host).manages_daemon(),
            self.accounts_tab_offered(),
        )
    }

    /// After the connection target or its state changed: a shown tab the
    /// list no longer offers gives way to General. An agent page stays
    /// through a lost connection, whose agent list is empty; only a
    /// connected daemon's list drops it ([`Self::follow_agent_list`]).
    pub(crate) fn follow_settings_target(&mut self) {
        if !matches!(self.settings_tab, SettingsTab::Agent(_)) {
            self.keep_offered_tab();
        }
    }

    /// After `host`'s connected daemon listed its agents: the page of an
    /// agent it no longer lists gives way to General while `host` is shown,
    /// and drops `host`'s drafts and applies for it.
    fn follow_agent_list(&mut self, host: &HostKey) {
        if *host == self.settings_host {
            self.keep_offered_tab();
        }
        let agents = self.sidebar.agents(host).to_vec();
        if let Some(tabs) = self.agent_tabs.of_mut(host) {
            tabs.keep_listed(&agents);
        }
    }

    /// The shown tab, or General when the list no longer offers it.
    fn keep_offered_tab(&mut self) {
        let kept = tab_kept(self.settings_tab, &self.settings_rows());
        if kept != self.settings_tab {
            self.settings_tab = kept;
            self.settings_control = None;
            self.notifications.sound_list = None;
            self.settings_pending.shell_list = None;
        }
    }

    /// Folds the daemon's agents or their Settings values, as
    /// [`RootView::on_agents`] does, and the agent pages follow them; an
    /// answer about an agent's user hooks goes to
    /// [`RootView::on_agent_user_hooks`]; any other message is dropped.
    pub(crate) fn fold_agents(
        &mut self,
        host: &crate::target::HostKey,
        msg: DaemonMessage,
        cx: &mut Context<Self>,
    ) {
        if matches!(msg, DaemonMessage::AgentUserHooks { .. }) {
            self.on_agent_user_hooks(host, msg, cx);
            return;
        }
        let listed = matches!(msg, DaemonMessage::Agents { .. });
        let pages_follow = listed || matches!(msg, DaemonMessage::AgentSettings { .. });
        self.on_agents(host, msg);
        if listed {
            self.follow_agent_list(host);
            self.ask_user_hooks(host);
        }
        if pages_follow {
            self.move_trusted_default(host);
            self.refresh_setting_texts(host, cx);
        }
    }

    /// Moves the Trusted launch default older releases saved into `host`'s
    /// agent Settings as each agent's bypass choice, once per host: only
    /// while the default is owed, `host` is not recorded yet, its agents'
    /// Settings values were received and its agents name a bypass choice.
    /// A host whose agents name none (an older daemon) is not recorded, so
    /// its next connect tries again.
    fn move_trusted_default(&mut self, host: &HostKey) {
        let spawn = &self.sidebar.ui_state().spawn;
        if !spawn.bypass_owed()
            || spawn
                .trusted_moved
                .contains(&crate::sidebar::trusted_move_key(host))
            || !self
                .mirrors_of(host)
                .is_some_and(|mirrors| mirrors.agent_settings_received)
        {
            return;
        }
        let moves = trusted_moves(self.sidebar.agents(host));
        if moves.is_empty() {
            return;
        }
        tracing::info!(
            ?host,
            "settings: moving the saved trusted-launch default into the agents' bypass choice"
        );
        for (agent, field, value) in moves {
            self.set_agent_setting(host, &agent, &field, value, false);
        }
        if self.sidebar.mark_trusted_moved(host) {
            self.save_ui();
        }
    }

    /// After `host`'s daemon listed its agents, on a connection that lists
    /// the user-hooks feature: asks it about each agent whose descriptor has
    /// `user_hooks`.
    fn ask_user_hooks(&self, host: &HostKey) {
        if !self
            .conn_of(host)
            .is_some_and(|conn| conn.supports(FEATURE_AGENT_USER_HOOKS))
        {
            return;
        }
        for descriptor in self.sidebar.agents(host) {
            if !descriptor.caps.user_hooks {
                continue;
            }
            let request_id = crate::new_request_id();
            tracing::info!(agent = %descriptor.id.0, ?host, %request_id, "settings: asking for an agent's user hooks");
            self.send_to(
                host,
                ClientMessage::GetAgentUserHooks {
                    agent: descriptor.id.clone(),
                    request_id: Some(request_id),
                },
            );
        }
    }

    /// The Hooks switch's press on the shown agent page: holds the opposite
    /// of the switch as shown until OK or Apply, or nothing when that is the
    /// value the switch shows without a held one and the row asks for no
    /// repair.
    fn toggle_user_hooks(&mut self) {
        let host = self.settings_host.clone();
        let Some(agent) = self.settings_tab.agent() else {
            return;
        };
        let Some(row) = self
            .user_hooks_row_of(&host, &agent)
            .filter(|row| row.enabled)
        else {
            return;
        };
        let Some(saved) = self.user_hooks_row_held(&host, &agent, None) else {
            return;
        };
        // An install the row asks to repair has no saved value to fall back
        // to: off and on again holds on, which Apply sends to rewrite it.
        let saved = (!saved.repair).then(|| HostPending::UserHooks {
            agent: agent.clone(),
            enabled: saved.checked,
        });
        self.settings_pending.set_host(
            &host,
            HostPending::UserHooks {
                agent,
                enabled: !row.checked,
            },
            saved.as_ref(),
        );
    }

    /// Apply reaching a held Hooks switch: `host` is asked to install
    /// (`enabled`) or remove agent `agent`'s user hooks, which the switch
    /// shows until the answer, and OK waits for that answer.
    fn apply_user_hooks(&mut self, host: &HostKey, agent: AgentId, enabled: bool) {
        let request_id = crate::new_request_id();
        let Some(mirrors) = self.mirrors_of_mut(host) else {
            return;
        };
        mirrors.agent_user_hooks_pending.insert(
            agent.clone(),
            UserHooksPending {
                request_id: request_id.clone(),
                enabled,
            },
        );
        tracing::info!(agent = %agent.0, ?host, enabled, %request_id, "settings: setting an agent's user hooks");
        self.send_setting(
            host,
            ClientMessage::SetAgentUserHooks {
                agent,
                enabled,
                request_id: Some(request_id),
            },
        );
    }

    /// `host`'s daemon's answer about an agent's user hooks.
    pub(crate) fn on_agent_user_hooks(
        &mut self,
        host: &HostKey,
        msg: DaemonMessage,
        cx: &mut Context<Self>,
    ) {
        let DaemonMessage::AgentUserHooks {
            agent,
            state,
            path,
            error,
            request_id,
        } = msg
        else {
            return;
        };
        let Some(mirrors) = self.mirrors_of_mut(host) else {
            return;
        };
        if let Some(error) = &error {
            tracing::warn!(agent = %agent.0, ?host, %error, "settings: the daemon could not edit an agent's user hooks");
        }
        let crate::hosts::HostMirrors {
            agent_user_hooks,
            agent_user_hooks_pending,
            ..
        } = mirrors;
        fold_user_hooks_reply(
            agent_user_hooks,
            agent_user_hooks_pending,
            agent,
            UserHooksEntry { state, path, error },
            request_id.as_deref(),
        );
        cx.notify();
    }

    /// After `host`'s agents or their Settings values arrived: each of its
    /// pages' text fields shows the placeholder its page's values now set,
    /// and one the user has not edited shows the value its page now holds.
    fn refresh_setting_texts(&mut self, host: &HostKey, cx: &mut Context<Self>) {
        for refresh in self.text_refreshes(host) {
            let Some(setting) = self.setting_text_mut(host, &refresh.agent, &refresh.field) else {
                continue;
            };
            if let Some(text) = refresh.text {
                setting.draft.clone_from(&text);
                setting
                    .input
                    .update(cx, |input, cx| input.set_text(text, cx));
            }
            if let Some(placeholder) = refresh.placeholder {
                setting.placeholder.clone_from(&placeholder);
                setting
                    .input
                    .update(cx, |input, cx| input.set_placeholder(placeholder, cx));
            }
        }
        cx.notify();
    }

    /// The text fields whose value or placeholder no longer reads as their
    /// page's values say, and what each should show.
    fn text_refreshes(&self, host: &HostKey) -> Vec<TextRefresh> {
        let mut refreshes = Vec::new();
        let Some(tabs) = self.agent_tabs.of(host) else {
            return refreshes;
        };
        for (agent, tab) in &tabs.tabs {
            let Some(page) = self.agent_page(host, agent) else {
                continue;
            };
            for setting in &tab.texts {
                let Some(view) = page.fields.iter().find(|view| view.id == setting.field) else {
                    continue;
                };
                let text = Some(setting_text(&view.value))
                    .filter(|text| !setting.edited && *text != setting.draft);
                let placeholder = Some(text_placeholder(view).unwrap_or_default())
                    .filter(|placeholder| *placeholder != setting.placeholder);
                if text.is_some() || placeholder.is_some() {
                    refreshes.push(TextRefresh {
                        agent: agent.clone(),
                        field: setting.field.clone(),
                        text,
                        placeholder,
                    });
                }
            }
        }
        refreshes
    }

    /// The label of the Settings tab shown.
    #[must_use]
    pub fn settings_tab(&self) -> &'static str {
        self.settings_tab.label(self.agents())
    }

    /// The Settings content's scrollbar thumb as the last frame laid it
    /// out, or `None` while the shown page fits.
    #[must_use]
    pub fn settings_scroll_thumb(&self) -> Option<Thumb> {
        self.settings_scroll.thumb()
    }

    /// The keep-awake choice selected, or `None` while the daemon has not
    /// reported one and the choices are locked.
    #[must_use]
    pub fn keep_awake_mode(&self) -> Option<KeepAwakeMode> {
        let pending = self
            .settings_pending
            .host(&self.settings_host)
            .iter()
            .find_map(|value| match value {
                HostPending::KeepAwake(mode) => Some(*mode),
                _ => None,
            });
        self.settings_mirrors()
            .keep_awake
            .map(|state| pending.unwrap_or(state.mode))
    }

    /// The keep-awake status line of the host Settings shows.
    #[must_use]
    pub fn keep_awake_status(&self) -> &'static str {
        keep_awake_status(self.settings_mirrors().keep_awake)
    }

    /// The Power section's keep-awake row label, naming the host Settings
    /// shows: "Keep this PC awake", else "Keep {host} awake".
    #[must_use]
    pub fn keep_awake_label(&self) -> String {
        match self.settings_host() {
            HostKey::Local => "Keep this PC awake".to_owned(),
            host @ HostKey::Remote(_) => format!("Keep {} awake", crate::sidebar::host_label(host)),
        }
    }

    /// The App title tab's preview: the title for a tab named "Tab name".
    #[must_use]
    pub fn title_preview(&self) -> String {
        let preview = compute_title(Some("Tab name"), &self.shown_local().title);
        if preview.is_empty() {
            "(empty)".to_owned()
        } else {
            preview
        }
    }

    /// What in the Settings frame, or in a session's, repo's or
    /// workspace's appearance editor, has the keyboard: `"list"` for the
    /// Settings tab list, else the focused control's or stop's selector.
    #[must_use]
    pub fn settings_focus(&self, window: &Window, cx: &App) -> Option<&'static str> {
        if self.settings_search_focused(window) {
            return Some(SEARCH_SELECTOR);
        }
        if self.settings_tabs_focus.is_focused(window) {
            return Some("list");
        }
        if self.settings_open() {
            return self
                .focused_settings_control(window, cx)
                .map(SettingsControl::selector);
        }
        self.focused_appearance_stop(window, cx)
            .map(AppearanceStop::selector)
    }

    /// The shown tab's controls Tab reaches, in order, then the footer's
    /// buttons; a disabled one is left out.
    fn settings_controls(&self, cx: &App) -> Vec<SettingsControl> {
        let applies = self.settings_apply_enabled();
        let ok = applies || self.account_alias_edited(cx);
        let mut controls = self.tab_controls(cx);
        controls.extend(FOOTER_CONTROLS.into_iter().filter(|control| match control {
            SettingsControl::FooterOk => ok,
            SettingsControl::FooterApply => applies,
            _ => true,
        }));
        controls
    }

    /// The shown tab's own controls Tab reaches, in order; a disabled one
    /// is left out.
    fn tab_controls(&self, cx: &App) -> Vec<SettingsControl> {
        self.tab_controls_of(self.settings_tab, cx)
    }

    /// `tab`'s own controls Tab reaches, in order; a disabled one is left
    /// out. An agent page's are the shown page's, so only the shown tab
    /// asks for one.
    fn tab_controls_of(&self, tab: SettingsTab, cx: &App) -> Vec<SettingsControl> {
        match tab {
            SettingsTab::General => {
                let mut controls = general_controls(
                    self.settings_mirrors().keep_awake.is_some(),
                    self.settings_host == HostKey::Local,
                );
                let live = self.shell_row_state() == RowState::Live;
                let command = self.shell_command_drawn();
                controls.retain(|control| match control {
                    SettingsControl::DefaultShell => live,
                    SettingsControl::DefaultShellCommand => command,
                    _ => true,
                });
                controls
            }
            SettingsTab::AppTitle => COUNT_MODE_CHOICES
                .map(SettingsControl::CountMode)
                .into_iter()
                .chain([SettingsControl::TitleCount, SettingsControl::TitleSuffix])
                .collect(),
            SettingsTab::Notifications => self.notifications_controls(),
            SettingsTab::Dashboard => self.dashboard_controls(false),
            SettingsTab::Agent(_) => self.agent_controls(),
            SettingsTab::Worktrees => {
                let mut controls = vec![SettingsControl::WorktreesPath];
                if self.worktrees_browses() {
                    controls.push(SettingsControl::WorktreesBrowse);
                }
                if self.worktrees_override().is_some() {
                    controls.push(SettingsControl::WorktreesReset);
                }
                if self.worktrees_manages() {
                    controls.push(SettingsControl::WorktreesManage);
                }
                controls
            }
            SettingsTab::RemoteAccess => self.remote_access_controls(),
            SettingsTab::Accounts => self.accounts_controls(true),
            SettingsTab::Appearance => self
                .appearance_stops(false, cx)
                .into_iter()
                .map(SettingsControl::Appearance)
                .collect(),
        }
    }

    /// The shown tab's stops in the order Tab walks them, then the
    /// footer's, each one there whether the ring holds it now or not.
    fn settings_stop_order(&self, cx: &App) -> Vec<SettingsControl> {
        let mut order = self.tab_stop_order(cx);
        order.extend(FOOTER_CONTROLS);
        order
    }

    /// The shown tab's own stops in the order Tab walks them.
    fn tab_stop_order(&self, cx: &App) -> Vec<SettingsControl> {
        self.tab_stop_order_of(self.settings_tab, cx)
    }

    /// `tab`'s own stops in the order Tab walks them, each one there whether
    /// the ring holds it now or not: what the page draws, which the search
    /// covers.
    fn tab_stop_order_of(&self, tab: SettingsTab, cx: &App) -> Vec<SettingsControl> {
        match tab {
            SettingsTab::General => {
                let mut controls = general_controls(true, true);
                if !self.shell_command_drawn() {
                    controls.retain(|control| *control != SettingsControl::DefaultShellCommand);
                }
                controls
            }
            SettingsTab::Dashboard => self.dashboard_controls(true),
            SettingsTab::AppTitle | SettingsTab::Notifications => self.tab_controls_of(tab, cx),
            SettingsTab::Agent(_) => {
                let Some(agent) = tab.agent() else {
                    return Vec::new();
                };
                let Some(descriptor) = descriptor_for(self.agents(), &agent) else {
                    return Vec::new();
                };
                let options =
                    AgentOptions::new(descriptor.clone(), self.agent_settings().get(&agent));
                // The indices are places among every field, not among those
                // shown: only the selectors are compared with the ring.
                options
                    .every_settings_field()
                    .iter()
                    .enumerate()
                    .flat_map(|(index, view)| field_controls(&descriptor.tag, index, view))
                    .chain([SettingsControl::AgentProgram, SettingsControl::AgentArgs])
                    .collect()
            }
            SettingsTab::Worktrees => vec![
                SettingsControl::WorktreesPath,
                SettingsControl::WorktreesBrowse,
                SettingsControl::WorktreesReset,
                SettingsControl::WorktreesManage,
            ],
            SettingsTab::RemoteAccess => vec![
                SettingsControl::RemoteToggle,
                SettingsControl::RemotePort,
                SettingsControl::RemoteAutostart,
            ],
            SettingsTab::Accounts => self.accounts_controls(false),
            SettingsTab::Appearance => self
                .appearance_stop_order(false)
                .into_iter()
                .map(SettingsControl::Appearance)
                .collect(),
        }
    }

    /// The shown agent page's controls in drawing order: each field's, then
    /// the launch rows. A field of a kind this client does not know is out,
    /// every field is out while [`Self::agent_rows_live`] says no, and the launch rows
    /// leave until the daemon reported the agent's command. A Program row
    /// an environment variable sets is out of the ring.
    fn agent_controls(&self) -> Vec<SettingsControl> {
        let Some(page) = self.shown_page() else {
            return Vec::new();
        };
        let host = &self.settings_host;
        let mut controls = Vec::new();
        if self.agent_rows_live(host, &page.id) {
            for (index, view) in page.fields.iter().enumerate() {
                if !view.disabled {
                    controls.extend(field_controls(&page.tag, index, view));
                }
            }
        }
        if self.launch_rows_live(host, &page.id) {
            if !self.program_read_only(host, &page.id) {
                controls.push(SettingsControl::AgentProgram);
            }
            controls.push(SettingsControl::AgentArgs);
        }
        if self
            .user_hooks_row_of(host, &page.id)
            .is_some_and(|row| row.enabled)
        {
            controls.push(SettingsControl::AgentUserHooks);
        }
        controls
    }

    /// Whether `host`'s connected daemon stores a launch command.
    fn launch_command_supported(&self, host: &HostKey) -> bool {
        self.conn_of(host)
            .is_some_and(|conn| conn.supports(FEATURE_AGENT_COMMANDS))
    }

    /// `host`'s agent `agent`'s launch command as its daemon last reported
    /// it.
    fn agent_command(&self, host: &HostKey, agent: &AgentId) -> Option<&AgentCommandEntry> {
        self.mirrors_of(host)?.agent_commands.as_ref()?.get(agent)
    }

    /// The agent page last selected and its fields, while Settings has
    /// shown one of the shown host's.
    fn current_agent_tab(&self) -> Option<(&AgentId, &AgentTab)> {
        let tabs = self.shown_agent_tabs()?;
        let agent = tabs.current.as_ref()?;
        Some((agent, tabs.tabs.get(agent)?))
    }

    /// The shown agent page's fields.
    fn shown_agent_tab(&self) -> Option<&AgentTab> {
        self.shown_agent_tabs()?
            .tabs
            .get(&self.settings_tab.agent()?)
    }

    fn setting_text_mut(
        &mut self,
        host: &HostKey,
        agent: &AgentId,
        field: &str,
    ) -> Option<&mut SettingText> {
        self.agent_tabs
            .of_mut(host)?
            .tabs
            .get_mut(agent)?
            .texts
            .iter_mut()
            .find(|setting| setting.field == field)
    }

    /// Whether the shown host's connected daemon's Welcome leaves the
    /// launch command out: an older daemon. A connection that is not open
    /// says nothing either way.
    fn launch_command_unsupported(&self) -> bool {
        let host = &self.settings_host;
        self.conn_of(host).is_some_and(crate::Connection::is_open)
            && !self.launch_command_supported(host)
    }

    /// Whether `host`'s agent `agent`'s launch-command rows take input: its
    /// daemon stores a launch command and has reported the agent's.
    fn launch_rows_live(&self, host: &HostKey, agent: &AgentId) -> bool {
        self.launch_command_supported(host) && self.agent_command(host, agent).is_some()
    }

    /// The Program field's text on the agent page last selected.
    #[must_use]
    pub fn launch_program(&self) -> Option<String> {
        self.current_agent_tab()
            .map(|(_, tab)| tab.program_draft.clone())
    }

    /// The Extra arguments field's text on the agent page last selected.
    #[must_use]
    pub fn launch_args(&self) -> Option<String> {
        self.current_agent_tab()
            .map(|(_, tab)| tab.args_draft.clone())
    }

    /// The daemon's refusal of the last apply and the row it shows under,
    /// on the agent page last selected.
    #[must_use]
    pub fn launch_command_refusal(&self) -> Option<(LaunchCommandRow, String)> {
        self.current_agent_tab()?.1.refusal.clone()
    }

    /// Whether the launch command's Saved note shows on the agent page
    /// last selected: the daemon echoed the last apply, and both fields
    /// still read as it stored them.
    #[must_use]
    pub fn launch_saved(&self) -> bool {
        self.current_agent_tab()
            .is_some_and(|(agent, tab)| self.shows_saved(agent, tab))
    }

    fn shows_saved(&self, agent: &AgentId, tab: &AgentTab) -> bool {
        let Some(stored) = self.agent_command(&self.settings_host, agent) else {
            return false;
        };
        tab.saved
            && tab.awaiting == 0
            && tab.program_draft.trim() == stored.program
            && tab.args_draft.trim() == stored.args
    }

    /// Whether the Program row of the agent page last selected is
    /// read-only: an environment variable on the host sets the program.
    #[must_use]
    pub fn launch_program_read_only(&self) -> bool {
        self.shown_agent_tabs()
            .and_then(|tabs| tabs.current.as_ref())
            .is_some_and(|agent| self.program_read_only(&self.settings_host, agent))
    }

    fn program_read_only(&self, host: &HostKey, agent: &AgentId) -> bool {
        self.agent_command(host, agent)
            .is_some_and(|entry| entry.program_env.is_some())
    }

    /// The note under the read-only Program row of the agent page last
    /// selected, naming the variable.
    #[must_use]
    pub fn launch_program_env_note(&self) -> Option<String> {
        self.program_env_note(self.shown_agent_tabs()?.current.as_ref()?)
    }

    fn program_env_note(&self, agent: &AgentId) -> Option<String> {
        let env = self
            .agent_command(&self.settings_host, agent)?
            .program_env
            .as_ref()?;
        Some(format!("Set by {env}"))
    }

    /// The shown agent page's Hooks section, while Settings shows an agent
    /// page that draws one.
    #[must_use]
    pub fn settings_user_hooks_row(&self) -> Option<UserHooksRow> {
        if !self.settings_open() {
            return None;
        }
        self.user_hooks_row_of(&self.settings_host, &self.settings_tab.agent()?)
    }

    /// The dimmed Hooks switch's tooltip, while Settings shows an agent page
    /// whose switch is dimmed.
    #[must_use]
    pub fn settings_user_hooks_tip(&self) -> Option<&'static str> {
        self.settings_user_hooks_row()
            .filter(|row| !row.enabled)
            .map(|_| USER_HOOKS_UNSUPPORTED_TIP)
    }

    /// `host`'s agent `agent`'s Hooks section, its held switch included;
    /// none while the connection is not open.
    fn user_hooks_row_of(&self, host: &HostKey, agent: &AgentId) -> Option<UserHooksRow> {
        let held = self
            .settings_pending
            .host(host)
            .iter()
            .find_map(|value| match value {
                HostPending::UserHooks {
                    agent: held_agent,
                    enabled,
                } if held_agent == agent => Some(*enabled),
                _ => None,
            });
        self.user_hooks_row_held(host, agent, held)
    }

    /// `host`'s agent `agent`'s Hooks section with `held` the switch's
    /// value waiting for Apply; none while the connection is not open.
    fn user_hooks_row_held(
        &self,
        host: &HostKey,
        agent: &AgentId,
        held: Option<bool>,
    ) -> Option<UserHooksRow> {
        let conn = self.conn_of(host).filter(|conn| conn.is_open())?;
        let flag = self
            .sidebar
            .agents(host)
            .iter()
            .any(|descriptor| descriptor.id == *agent && descriptor.caps.user_hooks);
        let mirrors = self.mirrors_of(host)?;
        user_hooks_row(
            flag,
            conn.supports(FEATURE_AGENT_USER_HOOKS),
            mirrors.agent_user_hooks.get(agent),
            held,
            mirrors.agent_user_hooks_pending.get(agent),
        )
    }

    /// Why the launch-command rows are disabled, while Settings is open
    /// and they are.
    #[must_use]
    pub fn settings_launch_command_tip(&self) -> Option<&'static str> {
        (self.settings_open() && self.launch_command_unsupported())
            .then_some(LAUNCH_COMMAND_UNSUPPORTED_TIP)
    }

    /// `host`'s agent `agent`'s page afresh: its launch-command fields
    /// holding the daemon's command, Program's placeholder the agent's tag,
    /// and an input per text or model field.
    fn new_agent_tab(
        &self,
        host: &HostKey,
        agent: &AgentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AgentTab {
        let (program_draft, args_draft) = self
            .agent_command(host, agent)
            .map_or_else(Default::default, |entry| {
                (entry.program.clone(), entry.args.clone())
            });
        let read_only = self.program_read_only(host, agent);
        let placeholder = descriptor_for(self.sidebar.agents(host), agent)
            .map_or_else(|| agent.0.clone(), |descriptor| descriptor.tag.clone());
        let program = cx.new(|cx| {
            let mut input = TextInput::new(program_draft.clone(), placeholder, cx);
            input.set_read_only(read_only, cx);
            input
        });
        let args = cx.new(|cx| TextInput::new(args_draft.clone(), LAUNCH_ARGS_PLACEHOLDER, cx));
        let program_focus = program.read(cx).focus_handle(cx);
        let args_focus = args.read(cx).focus_handle(cx);
        let mut subscriptions = Vec::new();
        for (input, row) in [
            (&program, LaunchCommandRow::Program),
            (&args, LaunchCommandRow::Args),
        ] {
            // Enter applies nothing: the rows wait for Apply or OK. Esc is
            // Cancel.
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                |this, _, event: &TextInputEvent, window, cx| {
                    if matches!(event, TextInputEvent::Cancel) {
                        this.close_appearance_editor(window, cx);
                    }
                },
            ));
            let editor = (host.clone(), agent.clone());
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                move |this, input, _: &TextChanged, _, cx| {
                    let text = input.read(cx).text().to_owned();
                    let tab = this
                        .agent_tabs
                        .of_mut(&editor.0)
                        .and_then(|tabs| tabs.tabs.get_mut(&editor.1));
                    if let Some(tab) = tab {
                        match row {
                            LaunchCommandRow::Program => tab.program_draft = text,
                            LaunchCommandRow::Args => tab.args_draft = text,
                        }
                        tab.refusal = None;
                    }
                    cx.notify();
                },
            ));
        }
        let mut texts = Vec::new();
        for seed in self.text_seeds(host, agent) {
            let (text, text_subscriptions) = new_setting_text(host, agent, seed, window, cx);
            texts.push(text);
            subscriptions.extend(text_subscriptions);
        }
        AgentTab {
            program,
            program_focus,
            args,
            args_focus,
            program_draft,
            args_draft,
            awaiting: 0,
            sent: None,
            saved: false,
            refusal: None,
            texts,
            _subscriptions: subscriptions,
        }
    }

    /// What each text or model field `host`'s agent `agent`'s descriptor
    /// marks for Settings starts with: the value its page shows, and its
    /// placeholder.
    fn text_seeds(&self, host: &HostKey, agent: &AgentId) -> Vec<TextSeed> {
        let Some(descriptor) = descriptor_for(self.sidebar.agents(host), agent) else {
            return Vec::new();
        };
        let shown = self.agent_page(host, agent).map(|page| page.fields);
        descriptor
            .fields
            .iter()
            .filter(|field| {
                field.settings
                    && matches!(field.kind, FieldKind::Text { .. } | FieldKind::Model { .. })
            })
            .map(|field| {
                let view = shown.iter().flatten().find(|view| view.id == field.id);
                TextSeed {
                    field: field.id.clone(),
                    draft: view
                        .map(|view| setting_text(&view.value))
                        .unwrap_or_default(),
                    placeholder: view.and_then(text_placeholder).unwrap_or_default(),
                }
            })
            .collect()
    }

    /// Apply reaching `host`'s agent `agent`'s pending text field `field`:
    /// an edited value is sent to `host` as the field's Settings value, an
    /// empty one as no value. Nothing is sent for a field that is locked
    /// or hidden, or while the agent's rows are disabled.
    fn apply_setting_text(&mut self, host: &HostKey, agent: &AgentId, field: &str) {
        let Some(setting) = self
            .agent_tabs
            .of(host)
            .and_then(|tabs| tabs.tabs.get(agent))
            .and_then(|tab| tab.texts.iter().find(|setting| setting.field == field))
        else {
            return;
        };
        if !setting.edited {
            return;
        }
        let value = typed_value(&setting.draft);
        let open = self
            .agent_page(host, agent)
            .and_then(|page| page.fields.into_iter().find(|view| view.id == field))
            .is_some_and(|view| !view.disabled);
        if !open || !self.agent_rows_live(host, agent) {
            tracing::info!(
                agent = %agent.0,
                key = field,
                "settings: the text field takes no value now; nothing sent"
            );
            return;
        }
        if let Some(setting) = self.setting_text_mut(host, agent, field) {
            setting.edited = false;
        }
        self.set_agent_setting(host, agent, field, value, true);
    }

    /// `host`'s pending edits on its agent pages, in the order Apply sends
    /// them: each page's text fields, then its launch command, page by
    /// page. A text field is pending while it takes a value now and its
    /// edited draft differs from the value its page shows; the launch
    /// command while its rows take input and a draft differs from the
    /// daemon's command, unless that is the very command an apply still
    /// waiting sent.
    fn host_pending_edits(&self, host: &HostKey) -> Vec<PendingEdit> {
        let mut pending = Vec::new();
        let Some(tabs) = self.agent_tabs.of(host) else {
            return pending;
        };
        for (agent, tab) in &tabs.tabs {
            pending.extend(self.text_pending_rows(host, agent, tab));
            if self.launch_pending(host, agent, tab) {
                pending.push(PendingEdit {
                    row: PendingRow::Launch {
                        agent: agent.clone(),
                    },
                    valid: true,
                });
            }
        }
        pending
    }

    /// `host`'s agent `agent`'s pending text fields, in the page's order:
    /// none while the agent's rows are disabled.
    fn text_pending_rows(
        &self,
        host: &HostKey,
        agent: &AgentId,
        tab: &AgentTab,
    ) -> Vec<PendingEdit> {
        if !self.agent_rows_live(host, agent) {
            return Vec::new();
        }
        let Some(page) = self.agent_page(host, agent) else {
            return Vec::new();
        };
        tab.texts
            .iter()
            .filter(|setting| {
                page.fields
                    .iter()
                    .find(|view| view.id == setting.field)
                    .is_some_and(|view| {
                        !view.disabled
                            && text_pending(
                                setting.edited,
                                &setting.draft,
                                &setting_text(&view.value),
                            )
                    })
            })
            .map(|setting| PendingEdit {
                row: PendingRow::Text {
                    agent: agent.clone(),
                    field: setting.field.clone(),
                },
                valid: true,
            })
            .collect()
    }

    /// Whether `host`'s agent `agent`'s launch rows hold an edit Apply
    /// would send.
    fn launch_pending(&self, host: &HostKey, agent: &AgentId, tab: &AgentTab) -> bool {
        if !self.launch_rows_live(host, agent) {
            return false;
        }
        let Some(stored) = self.agent_command(host, agent) else {
            return false;
        };
        let (program, args) = (tab.program_draft.trim(), tab.args_draft.trim());
        let in_flight = tab.awaiting > 0
            && tab.sent.as_ref().is_some_and(|(sent_program, sent_args)| {
                sent_program == program && sent_args == args
            });
        (program != stored.program || args != stored.args) && !in_flight
    }

    /// `host`'s every pending edit: its agent pages' rows, its toggles and
    /// choices, its Worktrees path, This PC's remote access and settings,
    /// and its Dashboard rows.
    fn all_host_pending_edits(&self, host: &HostKey) -> Vec<PendingEdit> {
        let mut pending = self.host_pending_edits(host);
        let toggle = PendingEdit {
            row: PendingRow::Toggle,
            valid: true,
        };
        let daemon_toggles = self
            .settings_pending
            .host(host)
            .iter()
            .filter(|value| {
                matches!(
                    value,
                    HostPending::KeepAwake(_)
                        | HostPending::AgentField { .. }
                        | HostPending::RemoteEnabled(_)
                        | HostPending::DefaultShell(_)
                        | HostPending::UserHooks { .. }
                )
            })
            .count();
        pending.extend(std::iter::repeat_n(toggle.clone(), daemon_toggles));
        if self.worktrees_pending(host) {
            pending.push(PendingEdit {
                row: PendingRow::WorktreesRoot,
                valid: true,
            });
        }
        if *host == HostKey::Local {
            if let Some(valid) = self.remote_port_pending() {
                pending.push(PendingEdit {
                    row: PendingRow::RemotePort,
                    valid,
                });
            }
            if let Some(valid) = self.chat_width_pending() {
                pending.push(PendingEdit {
                    row: PendingRow::ChatWidth,
                    valid,
                });
            }
            let local = self.settings_pending.local.len();
            pending.extend(std::iter::repeat_n(toggle, local));
        }
        pending.extend(self.dashboard_pending_rows(host));
        pending
    }

    /// Every host's pending edits, hosts without any left out.
    fn pending_edits(&self) -> HashMap<HostKey, Vec<PendingEdit>> {
        let mut hosts: Vec<HostKey> = vec![HostKey::Local];
        hosts.extend(self.agent_tabs.all().map(|(host, _)| host.clone()));
        hosts.extend(self.worktrees_tabs.keys().cloned());
        hosts.extend(self.settings_pending.hosts.keys().cloned());
        hosts.extend(self.dashboard_settings_tabs.keys().cloned());
        hosts
            .into_iter()
            .map(|host| {
                let edits = self.all_host_pending_edits(&host);
                (host, edits)
            })
            .filter(|(_, edits)| !edits.is_empty())
            .collect()
    }

    /// Whether any host holds an edit Apply would send.
    #[must_use]
    pub fn settings_have_pending_edits(&self) -> bool {
        !self.pending_edits().is_empty()
    }

    /// Whether Settings' OK and Apply can be pressed, as [`apply_enabled`]
    /// says over every host's pending edits.
    #[must_use]
    pub fn settings_apply_enabled(&self) -> bool {
        self.settings_open() && apply_enabled(&self.pending_edits())
    }

    /// Apply: every host's pending edits are sent to that host, one message
    /// per pending row (a host's remote access, and its Dashboard rows, as
    /// one message each), and This PC's pending settings are saved, while
    /// [`apply_enabled`] says so; nothing otherwise. Returns the echoes the
    /// sends owe and whether a This PC setting could not be applied, `None`
    /// when nothing was applied.
    fn apply_pending_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Applied> {
        let pending = self.pending_edits();
        if !apply_enabled(&pending) {
            return None;
        }
        self.settings_pending.log = Some(Vec::new());
        for (host, edits) in pending {
            self.apply_host_edits(&host, &edits, cx);
        }
        let mut failed = false;
        for value in std::mem::take(&mut self.settings_pending.local) {
            failed |= self.commit_local(value, window, cx);
        }
        self.commit_chat_width();
        cx.notify();
        Some(Applied {
            owed: self.settings_pending.log.take().unwrap_or_default(),
            failed,
        })
    }

    /// Apply reaching `host`, whose pending rows are `edits`: its agent
    /// pages' rows, its held toggles and choices, its Worktrees path, This
    /// PC's remote access, and its Dashboard rows, each sent.
    fn apply_host_edits(&mut self, host: &HostKey, edits: &[PendingEdit], cx: &mut Context<Self>) {
        for edit in edits {
            match &edit.row {
                PendingRow::Text { agent, field } => self.apply_setting_text(host, agent, field),
                PendingRow::Launch { agent } => self.apply_launch_command(host, agent),
                _ => {}
            }
        }
        let (remote, dashboard) = self.apply_held_settings(host);
        self.save_worktrees_root(host);
        if *host == HostKey::Local {
            self.apply_remote_access(remote);
        }
        self.apply_dashboard(host, dashboard, edits, cx);
    }

    /// `host`'s held keep-awake mode and agent toggles, sent; returns its
    /// held remote access switch and Dashboard values, which go out in one
    /// message each.
    fn apply_held_settings(&mut self, host: &HostKey) -> (Option<bool>, Vec<HostPending>) {
        let mut remote = None;
        let mut dashboard = Vec::new();
        for value in self.settings_pending.take_host(host) {
            match value {
                HostPending::KeepAwake(mode) => {
                    tracing::info!(?host, ?mode, "settings: applying the keep-awake mode");
                    self.send_setting(host, ClientMessage::SetKeepAwakeMode { mode });
                }
                HostPending::AgentField {
                    agent,
                    field,
                    value,
                } => self.set_agent_setting(host, &agent, &field, value, false),
                HostPending::RemoteEnabled(on) => remote = Some(on),
                HostPending::DefaultShell(setting) => self.apply_default_shell(host, setting),
                HostPending::UserHooks { agent, enabled } => {
                    self.apply_user_hooks(host, agent, enabled);
                }
                other => dashboard.push(other),
            }
        }
        (remote, dashboard)
    }

    /// `msg` to `host` as a send of Apply, whose echo OK waits for.
    fn send_setting(&mut self, host: &HostKey, msg: ClientMessage) {
        if let (Some(log), Some(owed)) =
            (self.settings_pending.log.as_mut(), OwedEcho::of(host, &msg))
        {
            log.push(owed);
        }
        self.send_to(host, msg);
    }

    /// OK: Apply, then close at once when it sent nothing to a daemon, or
    /// once every daemon echoed what it sent; a This PC setting Apply could
    /// not write keeps Settings open.
    fn press_settings_ok(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // An alias typed on the Accounts page is sent as Enter would; it
        // owes no echo, so it never holds Settings open.
        let alias_sent = self.commit_alias_on_close(cx);
        let Some(applied) = self.apply_pending_settings(window, cx) else {
            if alias_sent {
                self.close_appearance_editor(window, cx);
            }
            return;
        };
        if applied.failed {
            tracing::info!("settings: a This PC setting could not be applied; OK stays open");
        }
        if applied.owed.is_empty() {
            if !applied.failed {
                self.close_appearance_editor(window, cx);
            }
        } else {
            tracing::info!(
                sends = applied.owed.len(),
                "settings: OK waits for the daemons' echoes"
            );
            self.settings_pending.ok_wait = Some(applied.owed);
        }
    }

    /// Whether OK waits for a daemon's echo before Settings closes.
    #[must_use]
    pub fn settings_ok_waiting(&self) -> bool {
        self.settings_pending.ok_wait.is_some()
    }

    /// `msg` from `host` while OK waits: an echo it owes in a final state is
    /// crossed off, and the wait ends once the last is, after `msg` has
    /// been folded; a refusal of one of its sends ends the wait at once,
    /// leaving Settings open with the refusal shown.
    pub(crate) fn settings_ok_hears(
        &mut self,
        host: &HostKey,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(wait) = self.settings_pending.ok_wait.as_mut() else {
            return;
        };
        if hear_owed_echo(wait, host, msg) {
            tracing::info!(?host, "settings: a daemon refused an Apply; OK stays open");
            self.settings_pending.ok_wait = None;
            cx.notify();
        } else if wait.is_empty() {
            // The echo is folded after this: what is still pending is read
            // once it is.
            cx.defer_in(window, Self::end_ok_wait);
        }
    }

    /// Every echo OK waited for came: Settings closes, unless edits made
    /// while it waited are pending, which keep it open.
    fn end_ok_wait(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self
            .settings_pending
            .ok_wait
            .as_ref()
            .is_some_and(Vec::is_empty)
        {
            return;
        }
        self.settings_pending.ok_wait = None;
        if self.settings_have_pending_edits() {
            tracing::info!("settings: edits made while OK waited are pending; OK stays open");
            cx.notify();
        } else {
            self.close_appearance_editor(window, cx);
        }
    }

    /// Apply reaching `host`'s agent `agent`'s pending launch command: both
    /// fields, trimmed, as the agent's command on `host`; two empty fields
    /// clear it. Nothing is sent before the daemon reported the agent's
    /// command.
    fn apply_launch_command(&mut self, host: &HostKey, agent: &AgentId) {
        if !self.launch_rows_live(host, agent) {
            return;
        }
        let Some(tabs) = self.agent_tabs.of_mut(host) else {
            return;
        };
        let Some(tab) = tabs.tabs.get_mut(agent) else {
            return;
        };
        let program = tab.program_draft.trim().to_owned();
        let args = tab.args_draft.trim().to_owned();
        tab.awaiting += 1;
        tab.sent = Some((program.clone(), args.clone()));
        tab.saved = false;
        tab.refusal = None;
        let request_id = crate::new_request_id();
        tabs.applies.push_back((request_id.clone(), agent.clone()));
        tracing::info!(agent = %agent.0, ?host, %program, %args, %request_id, "settings: applying an agent's launch command");
        self.send_setting(
            host,
            ClientMessage::SetAgentCommand {
                agent: agent.clone(),
                program,
                args,
                request_id: Some(request_id),
            },
        );
    }

    /// `host`'s daemon's launch commands. Each echo answers `host`'s oldest
    /// apply still waiting, and every one of `host`'s agent pages takes the
    /// broadcast as [`AgentTab::on_echo`] says.
    pub(crate) fn on_agent_commands(
        &mut self,
        host: &HostKey,
        msg: DaemonMessage,
        cx: &mut Context<Self>,
    ) {
        let DaemonMessage::AgentCommands { commands } = msg else {
            return;
        };
        let Some(mirrors) = self.mirrors_of_mut(host) else {
            return;
        };
        let previous = mirrors.agent_commands.replace(
            commands
                .into_iter()
                .map(|entry| (entry.agent.clone(), entry))
                .collect(),
        );
        let Some(tabs) = self.agent_tabs.of_mut(host) else {
            cx.notify();
            return;
        };
        let answered = tabs.applies.pop_front().map(|(_, agent)| agent);
        let agents: Vec<AgentId> = tabs.tabs.keys().cloned().collect();
        for agent in agents {
            let echo = CommandEcho {
                old: previous
                    .as_ref()
                    .and_then(|commands| commands.get(&agent))
                    .cloned(),
                stored: self.agent_command(host, &agent).cloned(),
                read_only: self.program_read_only(host, &agent),
                answers: answered.as_ref() == Some(&agent),
            };
            let tab = self
                .agent_tabs
                .of_mut(host)
                .and_then(|tabs| tabs.tabs.get_mut(&agent));
            if let Some(tab) = tab {
                tab.on_echo(echo, cx);
            }
        }
        cx.notify();
    }

    /// A daemon refusal carrying `request_id`: when it names a
    /// launch-command apply still waiting, it ends that apply, and a refusal
    /// whose `code` names a launch-command row shows under that row on that
    /// apply's page. Returns whether it took `message`; any other refusal
    /// is left to toast.
    pub(crate) fn on_agent_command_error(
        &mut self,
        host: &HostKey,
        request_id: &str,
        code: Option<&str>,
        message: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(tabs) = self.agent_tabs.of_mut(host) else {
            return false;
        };
        let Some(at) = tabs.applies.iter().position(|(id, _)| id == request_id) else {
            return false;
        };
        let Some((_, agent)) = tabs.applies.remove(at) else {
            return false;
        };
        let Some(tab) = tabs.tabs.get_mut(&agent).filter(|tab| tab.awaiting > 0) else {
            return false;
        };
        tab.awaiting -= 1;
        tab.saved = false;
        cx.notify();
        let Some(row) = launch_refusal_row(code) else {
            return false;
        };
        tab.refusal = Some((row, message.to_owned()));
        true
    }

    /// A daemon refusal carrying `request_id`: when it names Settings
    /// values still waiting, that apply ends. The option it changed goes
    /// back to the daemon's value in the agent's pending values, unless a
    /// later apply changed it again, so the next press neither re-sends the
    /// refused value nor drops another apply's; the text field whose apply
    /// it was holds the refused text again, as edited, unless the user
    /// typed on since. The refusal itself still toasts.
    pub(crate) fn on_agent_setting_refused(
        &mut self,
        host: &HostKey,
        request_id: &str,
        message: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(mirrors) = self.mirrors_of_mut(host) else {
            return;
        };
        let Some(at) = mirrors
            .agent_setting_applies
            .iter()
            .position(|apply| apply.id == request_id)
        else {
            return;
        };
        let Some(apply) = mirrors.agent_setting_applies.remove(at) else {
            return;
        };
        tracing::warn!(agent = %apply.agent.0, ?host, request_id, message, "settings: the daemon refused an agent's values");
        let stored = mirrors
            .agent_settings
            .get(&apply.agent)
            .and_then(|values| values.get(&apply.key))
            .cloned();
        if let Some(pending) = mirrors.agent_settings_pending.get_mut(&apply.agent)
            && pending.get(&apply.key) == Some(&apply.value)
        {
            match stored {
                Some(stored) => pending.insert(apply.key.clone(), stored),
                None => pending.remove(&apply.key),
            };
        }
        if apply.from_text
            && let Some(setting) = self.setting_text_mut(host, &apply.agent, &apply.key)
            && !setting.edited
        {
            let text = setting_text(&apply.value);
            setting.draft.clone_from(&text);
            setting.edited = true;
            setting
                .input
                .update(cx, |input, cx| input.set_text(text, cx));
        }
        cx.notify();
    }

    /// A daemon refusal of the worktrees root (by its `code`) while the
    /// tab's field waits: the save the daemon could not make ends here, so a
    /// later broadcast from another client is not taken for its echo. The
    /// refusal is not claimed: the tab has no inline slot for it, and the
    /// generic toast still shows the message.
    pub(crate) fn on_worktrees_root_error(
        &mut self,
        host: &HostKey,
        code: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        if code != Some(error_codes::WORKTREES_ROOT_REFUSED) {
            return;
        }
        let Some(tab) = self
            .worktrees_tabs
            .get_mut(host)
            .filter(|tab| tab.awaiting > 0)
        else {
            return;
        };
        tab.awaiting = tab.awaiting.saturating_sub(1);
        cx.notify();
    }

    /// The control holding the keyboard, if one does; none while Settings
    /// is closed.
    pub(crate) fn focused_settings_control(
        &self,
        window: &Window,
        cx: &App,
    ) -> Option<SettingsControl> {
        if !self.settings_open() {
            return None;
        }
        self.focused_control_in(&self.settings_controls(cx), window, cx)
    }

    /// The control holding the keyboard among `controls`, the shown tab's
    /// ring, if one does.
    fn focused_control_in(
        &self,
        controls: &[SettingsControl],
        window: &Window,
        cx: &App,
    ) -> Option<SettingsControl> {
        if !self.settings_open() {
            return None;
        }
        self.settings_control.filter(|control| {
            let has_keyboard = match control {
                SettingsControl::WorktreesPath => self.worktrees_path_focused(window),
                SettingsControl::RemotePort => self.remote_port_focused(window),
                SettingsControl::ChatWidth => self.chat_width_focused(window),
                SettingsControl::DefaultShellCommand => self.shell_command_focused(window),
                SettingsControl::AgentProgram => self.launch_program_focused(window),
                SettingsControl::AgentArgs => self.launch_args_focused(window),
                SettingsControl::AgentText { field, .. } => {
                    self.agent_page_shown()
                        && self
                            .agent_text_focus(field)
                            .is_some_and(|focus| focus.is_focused(window))
                }
                SettingsControl::Appearance(stop) if stop.is_field() => {
                    self.appearance_field_has_keyboard(*stop, window, cx)
                }
                SettingsControl::DashboardKey
                | SettingsControl::DashboardEnvVariable
                | SettingsControl::DashboardCap => {
                    self.settings_tab == SettingsTab::Dashboard
                        && self
                            .settings_field_focus(*control)
                            .is_some_and(|focus| focus.is_focused(window))
                }
                SettingsControl::Accounts {
                    control: AccountsControl::Alias(_),
                    ..
                } => {
                    self.settings_tab == SettingsTab::Accounts
                        && self
                            .settings_field_focus(*control)
                            .is_some_and(|focus| focus.is_focused(window))
                }
                _ => self.appearance_focus.is_focused(window),
            };
            has_keyboard && controls.contains(control)
        })
    }

    /// Gives the keyboard to `control` as Tab or a press does: a field
    /// takes it itself. Arriving at an Appearance stop puts its cursor or
    /// highlight back on the chosen value.
    pub(crate) fn move_settings_focus(
        &mut self,
        control: SettingsControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let arriving = self.settings_control != Some(control);
        match control {
            SettingsControl::Appearance(stop) => {
                if arriving {
                    self.arrive_at_appearance_stop(stop, cx);
                }
                self.settings_control = Some(control);
                self.focus_appearance_stop(stop, window, cx);
            }
            _ => self.focus_settings_control(control, window),
        }
    }

    /// Gives the keyboard to `control`: a text field takes it itself, every
    /// other control through the modal's focus.
    pub(crate) fn focus_settings_control(&mut self, control: SettingsControl, window: &mut Window) {
        self.settings_control = Some(control);
        match self.settings_field_focus(control) {
            Some(focus) => focus.focus(window),
            None => self.appearance_focus.focus(window),
        }
    }

    /// The focus of `control` when it is a text field; `None` for a control
    /// that has the keyboard through the modal's focus.
    fn settings_field_focus(&self, control: SettingsControl) -> Option<FocusHandle> {
        match control {
            SettingsControl::WorktreesPath => {
                self.shown_worktrees_tab().map(|tab| tab.focus.clone())
            }
            SettingsControl::RemotePort => {
                self.remote_access_tab.as_ref().map(|tab| tab.focus.clone())
            }
            SettingsControl::ChatWidth => self
                .settings_pending
                .chat_width
                .as_ref()
                .map(|field| field.focus.clone()),
            SettingsControl::DefaultShellCommand => self
                .settings_pending
                .shell_command
                .as_ref()
                .filter(|field| field.host == self.settings_host)
                .map(|field| field.focus.clone()),
            SettingsControl::AgentProgram => {
                self.shown_agent_tab().map(|tab| tab.program_focus.clone())
            }
            SettingsControl::AgentArgs => self.shown_agent_tab().map(|tab| tab.args_focus.clone()),
            SettingsControl::AgentText { field, .. } => self.agent_text_focus(field),
            SettingsControl::DashboardKey => {
                self.shown_dashboard_tab().map(|tab| tab.key_focus.clone())
            }
            SettingsControl::DashboardEnvVariable => self
                .shown_dashboard_tab()
                .map(|tab| tab.variable_focus.clone()),
            SettingsControl::DashboardCap => {
                self.shown_dashboard_tab().map(|tab| tab.cap_focus.clone())
            }
            SettingsControl::Accounts {
                control: AccountsControl::Alias(number),
                ..
            } => self.alias_focus(number),
            _ => None,
        }
    }

    /// Run at each render: remembers the control the keyboard ring has
    /// focused, and when an echo or a re-read took it out of the ring while
    /// the keyboard still sits on it (or on the modal's own focus), moves
    /// the keyboard, after this render, to the stop after its place in the
    /// shown tab's full order that the ring holds. With the keyboard
    /// anywhere else, the lost control is forgotten and nothing moves.
    pub(crate) fn keep_settings_keyboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings_open() {
            self.settings_ring_control = None;
            return;
        }
        // The Accounts page's alias fields follow the account list, which
        // changes between renders with no window to build a field with.
        self.hold_account_alias_fields(window, cx);
        let controls = self.settings_controls(cx);
        if let Some(control) = self.focused_control_in(&controls, window, cx) {
            self.settings_ring_control = Some(control);
            return;
        }
        let Some(lost) = self.settings_ring_control.take() else {
            return;
        };
        let still_on_it = self.appearance_focus.is_focused(window)
            || self
                .settings_field_focus(lost)
                .is_some_and(|focus| focus.is_focused(window));
        if self.settings_control != Some(lost) || !still_on_it {
            return;
        }
        // An agent page's control carries its field's place among the
        // fields shown, which a `when` rule hiding an earlier field shifts:
        // stops are matched by their selector, which names the field (and
        // the choice) and survives that.
        let key = lost.selector();
        if let Some(moved) = controls.iter().find(|c| c.selector() == key).copied() {
            self.settings_control = Some(moved);
            self.settings_ring_control = Some(moved);
            return;
        }
        let order: Vec<&str> = self
            .settings_stop_order(cx)
            .into_iter()
            .map(SettingsControl::selector)
            .collect();
        let ring: Vec<&str> = controls.iter().map(|c| c.selector()).collect();
        let Some(next) = after_lost(&order, &ring, key)
            .and_then(|next| controls.iter().find(|c| c.selector() == next).copied())
        else {
            return;
        };
        cx.defer_in(window, move |this, window, cx| {
            if this.settings_open() {
                this.move_settings_focus(next, window, cx);
                cx.notify();
            }
        });
    }

    /// Whether the Remote access page's port field has the keyboard.
    pub(crate) fn remote_port_focused(&self, window: &Window) -> bool {
        self.settings_open()
            && self.settings_tab == SettingsTab::RemoteAccess
            && self
                .remote_access_tab
                .as_ref()
                .is_some_and(|tab| tab.focus.is_focused(window))
    }

    /// Whether the General tab's Maximum chat width field has the keyboard.
    fn chat_width_focused(&self, window: &Window) -> bool {
        self.settings_open()
            && self.settings_tab == SettingsTab::General
            && self
                .settings_pending
                .chat_width
                .as_ref()
                .is_some_and(|field| field.focus.is_focused(window))
    }

    /// Whether the shown agent page's Program field has the keyboard.
    fn launch_program_focused(&self, window: &Window) -> bool {
        self.agent_page_shown()
            && self
                .shown_agent_tab()
                .is_some_and(|tab| tab.program_focus.is_focused(window))
    }

    /// Whether the shown agent page's Extra arguments field has the
    /// keyboard.
    fn launch_args_focused(&self, window: &Window) -> bool {
        self.agent_page_shown()
            && self
                .shown_agent_tab()
                .is_some_and(|tab| tab.args_focus.is_focused(window))
    }

    /// Whether one of the shown agent page's text fields has the keyboard.
    fn agent_texts_focused(&self, window: &Window) -> bool {
        self.agent_page_shown()
            && self.shown_agent_tab().is_some_and(|tab| {
                tab.texts
                    .iter()
                    .any(|setting| setting.focus.is_focused(window))
            })
    }

    /// The focus of the input of the field with id `field` on the shown
    /// agent page.
    fn agent_text_focus(&self, field: &str) -> Option<FocusHandle> {
        self.shown_agent_tabs()?
            .tabs
            .get(&self.settings_tab.agent()?)?
            .texts
            .iter()
            .find(|setting| setting.field == field)
            .map(|setting| setting.focus.clone())
    }

    fn agent_page_shown(&self) -> bool {
        self.settings_open() && matches!(self.settings_tab, SettingsTab::Agent(_))
    }

    /// Whether the Worktrees tab's path field has the keyboard.
    pub(crate) fn worktrees_path_focused(&self, window: &Window) -> bool {
        self.settings_open()
            && self.settings_tab == SettingsTab::Worktrees
            && self
                .shown_worktrees_tab()
                .is_some_and(|tab| tab.focus.is_focused(window))
    }

    /// Moves the keyboard one step along the ring the search box, the tab
    /// list and the shown tab's controls make, backwards with `back`. From
    /// anywhere off the ring it goes to the tab list.
    fn step_settings_focus(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        // Position 0 is the search box, 1 the tab list; the controls follow.
        const SEARCH: usize = 0;
        const LIST: usize = 1;
        let controls = self.settings_controls(cx);
        let at = if self.settings_search_focused(window) {
            Some(SEARCH)
        } else if self.settings_tabs_focus.is_focused(window) {
            Some(LIST)
        } else {
            self.focused_control_in(&controls, window, cx)
                .and_then(|control| controls.iter().position(|c| *c == control))
                .map(|index| index + 2)
        };
        let len = controls.len() + 2;
        let next = match at {
            None => LIST,
            Some(at) if back => (at + len - 1) % len,
            Some(at) => (at + 1) % len,
        };
        match next {
            SEARCH => self.focus_settings_search(false, window, cx),
            LIST => {
                self.settings_control = None;
                self.settings_tabs_focus.focus(window);
            }
            _ => self.move_settings_focus(controls[next - 2], window, cx),
        }
        cx.notify();
    }

    /// A click on `control`, or Space or Enter while it has the keyboard.
    pub(crate) fn press_settings_control(
        &mut self,
        control: SettingsControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_settings_focus(control, window, cx);
        if let Some(value) = self.local_pending_of(control) {
            self.pend_local(value);
            cx.notify();
            return;
        }
        match control {
            SettingsControl::KeepAwake(mode) => self.choose_keep_awake_mode(mode),
            SettingsControl::CheckNow => self.check_for_updates_now(cx),
            SettingsControl::OpenLogsFolder => self.open_logs_folder(window, cx),
            SettingsControl::NotifySettingsLink => self.open_windows_notification_settings(cx),
            SettingsControl::NeedsYouSoundField => {
                self.toggle_sound_list(NotifyKind::NeedsYou, cx);
            }
            SettingsControl::NeedsYouSoundPlay => {
                self.preview_sound(
                    self.sound_of(NotifyKind::NeedsYou),
                    NotifyKind::NeedsYou,
                    cx,
                );
            }
            SettingsControl::FinishedSoundField => {
                self.toggle_sound_list(NotifyKind::Finished, cx);
            }
            SettingsControl::FinishedSoundPlay => {
                self.preview_sound(
                    self.sound_of(NotifyKind::Finished),
                    NotifyKind::Finished,
                    cx,
                );
            }
            SettingsControl::DefaultShell => self.toggle_shell_list(cx),
            SettingsControl::AgentChoice {
                field,
                choice,
                selector,
            } => self.press_agent_field(field, Some(choice), selector),
            SettingsControl::AgentToggle { field, selector } => {
                self.press_agent_field(field, None, selector);
            }
            // A text field takes keys, not presses; a This PC toggle or
            // choice was held above.
            SettingsControl::WorktreesPath
            | SettingsControl::RemotePort
            | SettingsControl::ChatWidth
            | SettingsControl::DefaultShellCommand
            | SettingsControl::AgentProgram
            | SettingsControl::AgentArgs
            | SettingsControl::AgentText { .. }
            | SettingsControl::DashboardKey
            | SettingsControl::DashboardEnvVariable
            | SettingsControl::DashboardCap
            | SettingsControl::SidebarView(_)
            | SettingsControl::LeafDensity(_)
            | SettingsControl::SessionsUnderRepos(_)
            | SettingsControl::RepoSort(_)
            | SettingsControl::CopyOnSelect
            | SettingsControl::ChatMode(_)
            | SettingsControl::CheckUpdates
            | SettingsControl::DetailedLogging
            | SettingsControl::CountMode(_)
            | SettingsControl::TitleCount
            | SettingsControl::TitleSuffix
            | SettingsControl::NotifyNeedsYou
            | SettingsControl::NotifyFinished
            | SettingsControl::NotifyAccountSwitches
            | SettingsControl::NotifySkip(_)
            | SettingsControl::FinishedGrace(_)
            | SettingsControl::ToastStyle(_) => {}
            SettingsControl::DashboardEnabled => self.toggle_summaries(),
            SettingsControl::DashboardProvider { index, .. } => {
                self.choose_summarizer_provider(index);
            }
            SettingsControl::DashboardKeySource(choice) => self.choose_key_source(choice),
            SettingsControl::DashboardKeyClear => self.clear_summarizer_key(cx),
            SettingsControl::DashboardTest => self.run_summary_test(),
            SettingsControl::WorktreesBrowse => self.browse_worktrees_root(cx),
            SettingsControl::WorktreesReset => self.reset_worktrees_root(cx),
            SettingsControl::WorktreesManage => self.open_worktrees_manager(window, cx),
            SettingsControl::RemoteToggle => self.toggle_remote_access(),
            SettingsControl::RemoteAutostart => self.toggle_autostart(),
            SettingsControl::AgentUserHooks => self.toggle_user_hooks(),
            SettingsControl::Accounts { control, .. } => self.press_accounts_control(control, cx),
            SettingsControl::Appearance(stop) => {
                self.press_appearance_stop(stop, window, cx);
                self.keep_keyboard_after_press(stop, window, cx);
            }
            SettingsControl::FooterOk => self.press_settings_ok(window, cx),
            SettingsControl::FooterApply => {
                self.apply_pending_settings(window, cx);
            }
            SettingsControl::FooterCancel => self.close_appearance_editor(window, cx),
        }
        cx.notify();
    }

    /// Opens the client's logs folder in the OS file manager; nothing when
    /// the config folder could not be resolved.
    fn open_logs_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.paths {
            Ok(paths) => {
                let job = OpenJob::Folder(paths.logs_dir.clone());
                self.dispatch_open(&HostKey::Local, job, Then::Nothing, window, cx);
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "settings: the logs folder is unknown; opening nothing"
                );
            }
        }
    }

    /// Whether the Detailed logging switch draws ticked: the saved value with
    /// any pending press over it.
    #[must_use]
    pub fn detailed_logging_shown(&self) -> bool {
        self.shown_local().general.detailed_logging
    }

    /// The hint under the Detailed logging switch.
    #[must_use]
    pub fn detailed_logging_hint(&self) -> &'static str {
        DETAILED_LOGGING_HINT
    }

    /// The This PC setting a press on `control` holds for Apply: a choice's
    /// value, or a toggle's opposite as shown; `None` for any other control.
    fn local_pending_of(&self, control: SettingsControl) -> Option<LocalPending> {
        let shown = self.shown_local();
        let (general, title, notes) = (&shown.general, &shown.title, &shown.notifications);
        Some(match control {
            SettingsControl::SidebarView(view) => LocalPending::SidebarView(view),
            SettingsControl::LeafDensity(density) => LocalPending::LeafDensity(density),
            SettingsControl::SessionsUnderRepos(mode) => LocalPending::SessionsUnderRepos(mode),
            SettingsControl::RepoSort(sort) => LocalPending::RepoSort(sort),
            SettingsControl::CopyOnSelect => LocalPending::CopyOnSelect(!general.copy_on_select),
            SettingsControl::ChatMode(mode) => LocalPending::ChatMode(mode),
            SettingsControl::CheckUpdates => LocalPending::CheckUpdates(!general.check_updates),
            SettingsControl::DetailedLogging => {
                LocalPending::DetailedLogging(!general.detailed_logging)
            }
            SettingsControl::CountMode(mode) => LocalPending::CountMode(mode),
            SettingsControl::TitleCount => LocalPending::TitleCount(!title.show_count),
            SettingsControl::TitleSuffix => LocalPending::TitleSuffix(!title.suffix),
            SettingsControl::NotifyNeedsYou => LocalPending::NotifyNeedsYou(!notes.awaiting_input),
            SettingsControl::NotifyFinished => LocalPending::NotifyFinished(!notes.finished),
            SettingsControl::NotifyAccountSwitches => {
                LocalPending::NotifyAccountSwitches(!notes.account_switches)
            }
            SettingsControl::NotifySkip(skip) => LocalPending::NotifySkip(skip),
            SettingsControl::FinishedGrace(secs) => LocalPending::FinishedGrace(secs),
            SettingsControl::ToastStyle(style) => LocalPending::ToastStyle(style),
            _ => return None,
        })
    }

    /// After a press of Appearance stop `stop`: when the press disabled it,
    /// the keyboard moves to the first stop after its place that the ring
    /// now holds, so the next Tab walks on from there.
    fn keep_keyboard_after_press(
        &mut self,
        stop: AppearanceStop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ring: Vec<AppearanceStop> = self
            .settings_controls(cx)
            .into_iter()
            .filter_map(SettingsControl::appearance_stop)
            .collect();
        if ring.contains(&stop) {
            return;
        }
        let order = self.appearance_stop_order(false);
        if let Some(next) = after_lost(&order, &ring, stop) {
            self.move_settings_focus(SettingsControl::Appearance(next), window, cx);
        }
    }

    /// The worktrees root the user set on the host Settings shows, if they
    /// set one.
    fn worktrees_override(&self) -> Option<&str> {
        match &self.settings_mirrors().worktrees_root {
            Some((root, true)) => Some(root),
            _ => None,
        }
    }

    /// The Worktrees tab of the host Settings shows, once it was shown.
    fn shown_worktrees_tab(&self) -> Option<&WorktreesTab> {
        self.worktrees_tabs.get(&self.settings_host)
    }

    /// Whether `host`'s path field is pending: it differs from the override
    /// (empty when none), and no save of it still waits for its echo.
    fn worktrees_pending(&self, host: &HostKey) -> bool {
        let saved = match self
            .mirrors_of(host)
            .and_then(|mirrors| mirrors.worktrees_root.as_ref())
        {
            Some((root, true)) => root.as_str(),
            _ => "",
        };
        self.worktrees_tabs.get(host).is_some_and(|tab| {
            let draft = tab.draft.trim();
            let in_flight = tab.awaiting > 0 && tab.sent.as_deref() == Some(draft);
            draft != saved && !in_flight
        })
    }

    /// The Worktrees tab's line on the root in use.
    #[must_use]
    pub fn worktrees_active_line(&self) -> String {
        match &self.settings_mirrors().worktrees_root {
            Some((root, true)) => format!("Active: {root} — user override"),
            Some((root, false)) => format!("Active: {root} — default (env or platform fallback)"),
            None => format!("Active: {NOT_CONNECTED} — default (env or platform fallback)"),
        }
    }

    /// Whether Reset to default can be pressed.
    #[must_use]
    pub fn worktrees_reset_enabled(&self) -> bool {
        self.worktrees_override().is_some()
    }

    /// The Worktrees tab's note under the path on a remote host: Save
    /// creates a missing folder there, so a typo is not refused.
    #[must_use]
    pub fn worktrees_host_note(&self) -> Option<String> {
        self.conn_of(&self.settings_host)
            .and_then(|conn| conn.target().short_name())
            .map(|host| format!("Created on {host} if it does not exist."))
    }

    /// Whether the Worktrees tab draws Browse: only on a host whose folders
    /// this PC's picker reaches.
    fn worktrees_browses(&self) -> bool {
        self.capabilities_of(&self.settings_host)
            .opens_local_files()
    }

    /// Whether the Worktrees tab draws Manage worktrees: the manager lists,
    /// prunes and deletes This PC's worktrees only.
    fn worktrees_manages(&self) -> bool {
        self.capabilities_of(&self.settings_host)
            .opens_local_files()
    }

    /// The Worktrees tab's path field, while the tab has been shown.
    #[must_use]
    pub fn worktrees_path(&self) -> Option<String> {
        self.shown_worktrees_tab().map(|tab| tab.draft.clone())
    }

    /// The shown host's Worktrees tab, built afresh unless held: held, it
    /// keeps its draft until Settings closes.
    fn hold_worktrees_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let host = self.settings_host.clone();
        if self.worktrees_tabs.contains_key(&host) {
            return;
        }
        let tab = self.new_worktrees_tab(host.clone(), window, cx);
        self.worktrees_tabs.insert(host, tab);
    }

    /// `host`'s path field afresh, holding its override.
    fn new_worktrees_tab(
        &self,
        host: HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> WorktreesTab {
        let draft = self.worktrees_override().unwrap_or("").to_owned();
        let placeholder = self
            .settings_mirrors()
            .worktrees_root
            .as_ref()
            .map_or(NOT_CONNECTED, |(root, _)| root.as_str())
            .to_owned();
        let input = cx.new(|cx| TextInput::new(draft.clone(), placeholder, cx));
        let focus = input.read(cx).focus_handle(cx);
        let keys = cx.subscribe_in(
            &input,
            window,
            // Enter applies nothing: the path waits for Apply or OK. Esc is
            // Cancel.
            |this, _, event: &TextInputEvent, window, cx| {
                if matches!(event, TextInputEvent::Cancel) {
                    this.close_appearance_editor(window, cx);
                }
            },
        );
        let edits = cx.subscribe_in(
            &input,
            window,
            move |this, input, _: &TextChanged, _, cx| {
                let text = input.read(cx).text().to_owned();
                if let Some(tab) = this.worktrees_tabs.get_mut(&host) {
                    tab.draft = text;
                }
                cx.notify();
            },
        );
        WorktreesTab {
            input,
            focus,
            draft,
            awaiting: 0,
            sent: None,
            _subscriptions: vec![keys, edits],
        }
    }

    /// Apply reaching `host`'s pending path field: the field as the
    /// override, or none when it is empty.
    fn save_worktrees_root(&mut self, host: &HostKey) {
        if !self.worktrees_pending(host) {
            return;
        }
        let Some(tab) = self.worktrees_tabs.get_mut(host) else {
            return;
        };
        let trimmed = tab.draft.trim();
        let path = (!trimmed.is_empty()).then(|| trimmed.to_owned());
        tab.sent = Some(trimmed.to_owned());
        tab.awaiting += 1;
        tracing::info!(?path, ?host, "settings: saving the worktrees root");
        self.send_setting(host, ClientMessage::SetWorktreesRoot { path });
    }

    /// Reset to default empties the path field: a pending edit, which
    /// Apply or OK sends as no override.
    fn reset_worktrees_root(&mut self, cx: &mut Context<Self>) {
        if self.worktrees_override().is_none() {
            return;
        }
        let host = self.settings_host.clone();
        if let Some(tab) = self.worktrees_tabs.get_mut(&host) {
            tab.draft.clear();
            tab.input.update(cx, |input, cx| input.set_text("", cx));
        }
    }

    /// Browse: the folder picked fills the path field, if the field is
    /// still the one that asked.
    fn browse_worktrees_root(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.shown_worktrees_tab() else {
            return;
        };
        let asked = tab.input.entity_id();
        // The picker opens at the path in the field, else at the root the
        // daemon uses.
        let current = tab.input.read(cx).text().trim().to_owned();
        let start = if current.is_empty() {
            self.settings_mirrors()
                .worktrees_root
                .as_ref()
                .map(|(root, _)| PathBuf::from(root))
        } else {
            Some(PathBuf::from(current))
        };
        self.pick_local(Pick::Folder, start, cx, move |this, picked, cx| {
            if let Some(path) = picked {
                this.finish_worktrees_browse(asked, path.to_string_lossy().into_owned(), cx);
            }
        });
    }

    fn finish_worktrees_browse(&mut self, asked: EntityId, path: String, cx: &mut Context<Self>) {
        let Some(tab) = self
            .worktrees_tabs
            .values_mut()
            .find(|tab| tab.input.entity_id() == asked)
        else {
            return;
        };
        tab.draft.clone_from(&path);
        tab.input.update(cx, |input, cx| input.set_text(path, cx));
        cx.notify();
    }

    /// `host`'s daemon's worktrees root. Each echo answers one of that
    /// host's tab's sends, and the one answering the last shows `Saved` and
    /// puts the override the daemon settled on in the field. An echo
    /// answering none is another client's change: it fills the field only
    /// while the field still reads as the daemon left it.
    pub(crate) fn on_worktrees_root_changed(
        &mut self,
        host: &HostKey,
        root: &str,
        is_override: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(mirrors) = self.mirrors_of_mut(host) else {
            return;
        };
        let previous = mirrors
            .worktrees_root
            .as_ref()
            .filter(|(_, was_overridden)| *was_overridden)
            .map_or_else(String::new, |(root, _)| root.clone());
        mirrors.worktrees_root = Some((root.to_owned(), is_override));
        if let Some(tab) = self.worktrees_tabs.get_mut(host) {
            let placeholder = root.to_owned();
            tab.input
                .update(cx, |input, cx| input.set_placeholder(placeholder, cx));
            let mine = tab.awaiting > 0;
            tab.awaiting = tab.awaiting.saturating_sub(1);
            let settles = mine && tab.awaiting == 0;
            let untouched = !mine && tab.draft == previous;
            if settles || untouched {
                tab.draft = if is_override {
                    root.to_owned()
                } else {
                    String::new()
                };
                let text = tab.draft.clone();
                tab.input.update(cx, |input, cx| input.set_text(text, cx));
            }
        }
        cx.notify();
    }

    /// Forgets This PC's root and its tab's field, as a new connection does.
    pub(crate) fn reset_worktrees_tab(&mut self) {
        self.mirrors_mut().worktrees_root = None;
        self.worktrees_tabs.remove(&HostKey::Local);
    }

    /// `host`'s daemon's last `RemoteStatus`, or `None` when its connection
    /// has not reported one. On This PC's, a port field that is empty or
    /// still shows the old port takes the new one.
    pub(crate) fn set_remote_status(
        &mut self,
        host: &HostKey,
        status: Option<RemoteStatusView>,
        cx: &mut Context<Self>,
    ) {
        let Some(mirrors) = self.mirrors_of_mut(host) else {
            return;
        };
        let old = mirrors.remote_status.as_ref().map(|status| status.port);
        let new = status.as_ref().map(|status| status.port);
        mirrors.remote_status = status;
        if *host != HostKey::Local {
            return;
        }
        if let Some(tab) = &mut self.remote_access_tab {
            let untouched = tab.draft.trim().is_empty() || parse_port(&tab.draft).ok() == old;
            if untouched && old != new {
                tab.draft = new.map_or_else(String::new, |port| port.to_string());
                let text = tab.draft.clone();
                tab.input.update(cx, |input, cx| input.set_text(text, cx));
            }
        }
        cx.notify();
    }

    /// `host`'s daemon's `RemoteStatus`; any other message is left alone.
    pub(crate) fn on_remote_status(
        &mut self,
        host: &HostKey,
        msg: DaemonMessage,
        cx: &mut Context<Self>,
    ) {
        if let DaemonMessage::RemoteStatus {
            enabled,
            port,
            state,
        } = msg
        {
            let status = RemoteStatusView {
                enabled,
                port,
                state,
            };
            self.set_remote_status(host, Some(status), cx);
        }
    }

    /// The Remote access page's line on what remote access is doing.
    #[must_use]
    pub fn remote_status_line(&self) -> String {
        self.mirrors().remote_status.as_ref().map_or_else(
            || NOT_CONNECTED.to_owned(),
            |status| state_line(&status.state, status.port),
        )
    }

    /// The remote access switch: whether it is ticked, and whether it can
    /// be pressed.
    #[must_use]
    pub fn remote_toggle(&self) -> (bool, bool) {
        let status = self.mirrors().remote_status.as_ref();
        let pending = self
            .settings_pending
            .host(&HostKey::Local)
            .iter()
            .find_map(|value| match value {
                HostPending::RemoteEnabled(on) => Some(*on),
                _ => None,
            });
        (
            status.is_some_and(|status| pending.unwrap_or(status.enabled)),
            controls_enabled(status),
        )
    }

    /// The address other devices reach this PC at, while it serves.
    #[must_use]
    pub fn remote_address(&self) -> Option<String> {
        self.mirrors()
            .remote_status
            .as_ref()
            .and_then(|status| address(&status.state))
    }

    /// How long the serving certificate has left, while its expiry is known.
    fn remote_cert_line(&self) -> Option<String> {
        let Some(RemoteStatusView {
            state: RemoteState::Serving {
                cert_expires_at, ..
            },
            ..
        }) = &self.mirrors().remote_status
        else {
            return None;
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .inspect_err(|err| tracing::warn!("settings: the clock reads before 1970: {err}"))
            .ok()?;
        cert_line(*cert_expires_at, now.as_secs())
    }

    /// The Remote access page's port field, while the page has been shown.
    #[must_use]
    pub fn remote_port(&self) -> Option<String> {
        self.remote_access_tab.as_ref().map(|tab| tab.draft.clone())
    }

    /// The port field's port as a `ConfigureRemote`, once the daemon has
    /// reported and only for a valid port other than its own; Apply takes
    /// the port from it.
    fn remote_apply_message(&self) -> Option<ClientMessage> {
        let status = self.mirrors().remote_status.as_ref()?;
        let tab = self.remote_access_tab.as_ref()?;
        apply_port_message(status, &tab.draft)
    }

    /// Why the port field's text is no port, while it holds some.
    #[must_use]
    pub fn remote_port_error(&self) -> Option<String> {
        let draft = &self.remote_access_tab.as_ref()?.draft;
        if draft.trim().is_empty() {
            return None;
        }
        parse_port(draft)
            .err()
            .map(|err: PortError| err.to_string())
    }

    /// Whether the daemon starts on login, as the page last read it; `None`
    /// before the page was shown or when the read failed.
    #[must_use]
    pub fn remote_autostart(&self) -> Option<bool> {
        self.remote_access_tab
            .as_ref()
            .and_then(|tab| tab.autostart.as_ref().ok().copied())
    }

    /// Whether the daemon starts on login as the page draws it: the launch
    /// the page read, with a pending switch over it; `None` before the page
    /// was shown or when the read failed.
    #[must_use]
    pub fn remote_autostart_shown(&self) -> Option<bool> {
        self.shown_local().autostart
    }

    /// The line under the login-launch toggle: why its last switch, or its
    /// read, failed.
    #[must_use]
    pub fn remote_autostart_line(&self) -> Option<String> {
        let tab = self.remote_access_tab.as_ref()?;
        match (&tab.autostart_error, &tab.autostart) {
            (Some(error), _) => Some(error.clone()),
            (None, Err(error)) => Some(autostart_read_failure_line(error)),
            (None, Ok(_)) => None,
        }
    }

    /// The Remote access page's controls in drawing order: the switch and
    /// the port field once the daemon has reported, and the login launch
    /// once it was read. The footer's OK and Apply send a changed port.
    fn remote_access_controls(&self) -> Vec<SettingsControl> {
        let mut controls = Vec::new();
        if controls_enabled(self.mirrors().remote_status.as_ref()) {
            controls.extend([SettingsControl::RemoteToggle, SettingsControl::RemotePort]);
        }
        if self.remote_autostart().is_some() {
            controls.push(SettingsControl::RemoteAutostart);
        }
        controls
    }

    /// The Remote access page, built afresh unless held: held, it keeps its
    /// port draft until Settings closes, and the login launch is read
    /// again.
    fn hold_remote_access_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.remote_access_tab.is_none() {
            self.remote_access_tab = Some(self.new_remote_access_tab(window, cx));
            return;
        }
        let autostart = self.read_autostart();
        if let Some(tab) = &mut self.remote_access_tab {
            tab.autostart = autostart;
            tab.autostart_error = None;
        }
    }

    /// The page afresh: the port field holding the daemon's port, and the
    /// login launch read now.
    fn new_remote_access_tab(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RemoteAccessTab {
        let draft = self
            .mirrors()
            .remote_status
            .as_ref()
            .map_or_else(String::new, |status| status.port.to_string());
        let input = cx.new(|cx| TextInput::new(draft.clone(), DEFAULT_PORT_TEXT.to_owned(), cx));
        let focus = input.read(cx).focus_handle(cx);
        let keys = cx.subscribe_in(
            &input,
            window,
            // Enter applies nothing: the port waits for Apply or OK. Esc is
            // Cancel.
            |this, _, event: &TextInputEvent, window, cx| {
                if matches!(event, TextInputEvent::Cancel) {
                    this.close_appearance_editor(window, cx);
                }
            },
        );
        let edits = cx.subscribe_in(&input, window, |this, input, _: &TextChanged, _, cx| {
            let text = input.read(cx).text().to_owned();
            if let Some(tab) = &mut this.remote_access_tab {
                tab.draft = text;
            }
            cx.notify();
        });
        RemoteAccessTab {
            input,
            focus,
            draft,
            autostart: self.read_autostart(),
            autostart_error: None,
            _subscriptions: vec![keys, edits],
        }
    }

    /// Whether the daemon starts on login; a failed read is logged.
    fn read_autostart(&self) -> Result<bool, String> {
        self.autostart
            .get()
            .inspect_err(|err| tracing::warn!("settings: reading the login launch failed: {err}"))
    }

    /// The switch's press: the opposite of the switch as shown, pending
    /// until Apply.
    fn toggle_remote_access(&mut self) {
        if let Some(status) = &self.mirrors().remote_status {
            let saved = HostPending::RemoteEnabled(status.enabled);
            let shown = self.remote_toggle().0;
            self.settings_pending.set_host(
                &HostKey::Local,
                HostPending::RemoteEnabled(!shown),
                Some(&saved),
            );
        }
    }

    /// Whether This PC's port field is pending, and whether it holds a
    /// port: a draft other than the daemon's port, an empty one not.
    fn remote_port_pending(&self) -> Option<bool> {
        let status = self.mirrors().remote_status.as_ref()?;
        let draft = &self.remote_access_tab.as_ref()?.draft;
        if draft.trim().is_empty() {
            return None;
        }
        match parse_port(draft) {
            Ok(port) => (port != status.port).then_some(true),
            Err(_) => Some(false),
        }
    }

    /// Apply reaching This PC's remote access: one message with the switch
    /// as shown and the port field's port when it is pending.
    fn apply_remote_access(&mut self, enabled: Option<bool>) {
        let Some(status) = self.mirrors().remote_status.clone() else {
            return;
        };
        let port = self
            .remote_apply_message()
            .and_then(|message| match message {
                ClientMessage::ConfigureRemote { port, .. } => port,
                _ => None,
            });
        if enabled.is_none() && port.is_none() {
            return;
        }
        let enabled = enabled.unwrap_or(status.enabled);
        tracing::info!(enabled, ?port, "settings: applying remote access");
        self.send_setting(
            &HostKey::Local,
            ClientMessage::ConfigureRemote { enabled, port },
        );
    }

    /// The switch's press: the opposite of the login launch as shown,
    /// pending until Apply; nothing while the launch could not be read.
    fn toggle_autostart(&mut self) {
        let Some(on) = self.remote_autostart_shown() else {
            return;
        };
        self.pend_local(LocalPending::Autostart(!on));
    }

    /// The title last set on the window; empty until the first is.
    #[must_use]
    pub fn window_title(&self) -> &str {
        &self.window_title
    }

    /// Ctrl+, or the gear: the Settings modal on its General tab, with the
    /// keyboard on the tab list.
    pub(crate) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_appearance_editor(Level::App, window, cx);
        if self.settings_open() {
            self.settings_host = HostKey::Local;
            self.settings_tab = SettingsTab::General;
            self.settings_scroll = Scrollbar::default();
            self.settings_control = None;
            self.notifications.sound_list = None;
            self.settings_tabs_focus.focus(window);
            self.arm_pairing_tick(window, cx);
            self.hold_chat_width_field(window, cx);
            self.show_shell_row(window, cx);
            self.settings_search = Some(Self::new_settings_search(window, cx));
        }
    }

    fn select_settings_tab(
        &mut self,
        tab: SettingsTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings_tab = tab;
        self.settings_control = None;
        self.notifications.sound_list = None;
        // While searching, the tab's first match scrolls into view.
        if let Some(search) = &mut self.settings_search {
            search.reveal = true;
        }
        self.arm_pairing_tick(window, cx);
        if tab == SettingsTab::Notifications {
            self.read_notify_state(window, cx);
            self.check_needs_you_sound(cx);
            self.check_finished_sound(cx);
        }
        // A page holding drafts is built once per opening, and per host
        // where it is per host: selecting it again keeps what was typed.
        self.settings_pending.shell_list = None;
        if tab == SettingsTab::General {
            self.hold_chat_width_field(window, cx);
            self.show_shell_row(window, cx);
        }
        if tab == SettingsTab::Worktrees {
            self.hold_worktrees_tab(window, cx);
        }
        if tab == SettingsTab::RemoteAccess {
            self.hold_remote_access_tab(window, cx);
            self.on_devices_page_selected();
        }
        if tab == SettingsTab::Dashboard {
            self.hold_dashboard_tab(window, cx);
        }
        if tab == SettingsTab::Accounts {
            self.on_accounts_tab_selected(window, cx);
        }
        if let Some(agent) = tab.agent() {
            self.hold_agent_tab(agent, window, cx);
        }
        self.settings_tabs_focus.focus(window);
        cx.notify();
    }

    /// The shown host's page of `agent` becomes the one last selected,
    /// built afresh unless held. An agent page keeps its fields and the
    /// apply they wait on until Settings closes, so selecting it again
    /// loses nothing.
    fn hold_agent_tab(&mut self, agent: AgentId, window: &mut Window, cx: &mut Context<Self>) {
        let host = self.settings_host.clone();
        let held = self
            .agent_tabs
            .of(&host)
            .is_some_and(|tabs| tabs.tabs.contains_key(&agent));
        let fresh = (!held).then(|| self.new_agent_tab(&host, &agent, window, cx));
        let tabs = self.agent_tabs.entry(&host);
        if let Some(fresh) = fresh {
            tabs.tabs.insert(agent.clone(), fresh);
        }
        tabs.current = Some(agent);
    }

    /// Reads Windows' toast setting on a background thread; the tab shows
    /// `checking…` until it arrives.
    fn read_notify_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.notifications.state = None;
        let notifier = Arc::clone(&self.notifications.notifier);
        let read = cx
            .background_executor()
            .spawn(async move { notifier.state() });
        cx.spawn_in(window, async move |this, cx| {
            let state = read.await;
            // Fails only when the view is gone, and its window with it.
            this.update(cx, |this, cx| {
                this.notifications.state = Some(state);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The Notifications tab's line on Windows' toast setting.
    #[must_use]
    pub fn notifications_state_line(&self) -> String {
        format!(
            "Windows notifications: {}",
            NotifyState::line_label(self.notifications.state)
        )
    }

    /// What the Notifications tab says is wrong with the Needs you sound's
    /// custom file, `None` when nothing is.
    #[must_use]
    pub fn needs_you_sound_problem(&self) -> Option<String> {
        self.notifications.sound_problem.clone()
    }

    /// What the Notifications tab says is wrong with the Finished sound's
    /// custom file, `None` when nothing is.
    #[must_use]
    pub fn finished_sound_problem(&self) -> Option<String> {
        self.notifications.finished_sound_problem.clone()
    }

    /// The Notifications tab's stops: the link, Needs you with its sound
    /// field and that field's Play (not while the sound is off), Finished
    /// with its own sound field and Play, the Finished grace choices while
    /// Finished is on, Account switches, then the skip choices and, on
    /// Windows, the toast style choices.
    fn notifications_controls(&self) -> Vec<SettingsControl> {
        let shown = self.shown_local();
        let settings = &shown.notifications;
        let mut controls = vec![
            SettingsControl::NotifySettingsLink,
            SettingsControl::NotifyNeedsYou,
            SettingsControl::NeedsYouSoundField,
        ];
        if settings.needs_you_sound != ToastSound::Off {
            controls.push(SettingsControl::NeedsYouSoundPlay);
        }
        controls.push(SettingsControl::NotifyFinished);
        controls.push(SettingsControl::FinishedSoundField);
        if settings.finished_sound != ToastSound::Off {
            controls.push(SettingsControl::FinishedSoundPlay);
        }
        if settings.finished {
            controls.extend(FINISHED_GRACE_CHOICES.map(SettingsControl::FinishedGrace));
        }
        controls.push(SettingsControl::NotifyAccountSwitches);
        controls.extend(NOTIFY_SKIP_CHOICES.map(SettingsControl::NotifySkip));
        #[cfg(windows)]
        controls.extend(TOAST_STYLE_CHOICES.map(SettingsControl::ToastStyle));
        controls
    }

    /// Which row's sound list is open, if one is.
    #[must_use]
    pub fn open_sound_list(&self) -> Option<NotifyKind> {
        self.notifications.sound_list.as_ref().map(|list| list.kind)
    }

    /// The sound chosen on `kind`'s row.
    fn sound_of(&self, kind: NotifyKind) -> ToastSound {
        let settings = self.shown_local().notifications;
        match kind {
            NotifyKind::NeedsYou => settings.needs_you_sound,
            NotifyKind::Finished => settings.finished_sound,
        }
    }

    /// Opens `kind`'s sound list on its chosen entry, in place of any other,
    /// or closes it when it is the one open.
    fn toggle_sound_list(&mut self, kind: NotifyKind, cx: &mut Context<Self>) {
        if self.open_sound_list() == Some(kind) {
            self.notifications.sound_list = None;
        } else {
            let chosen = SoundEntry::of(&self.sound_of(kind), kind);
            let highlight = SoundEntry::ALL
                .iter()
                .position(|entry| *entry == chosen)
                .unwrap_or(0);
            let scroll = ScrollHandle::new();
            scroll.scroll_to_item(highlight);
            self.notifications.sound_list = Some(SoundList {
                kind,
                highlight,
                scroll,
                measured: None,
            });
        }
        cx.notify();
    }

    /// Closes the open sound list, if there is one.
    fn close_sound_list(&mut self, cx: &mut Context<Self>) {
        if self.notifications.sound_list.take().is_some() {
            cx.notify();
        }
    }

    /// Keeps where the open list's field and the window were laid out; a
    /// change draws again so the list is placed from it, its highlight in
    /// view.
    fn measure_sound_list(
        &mut self,
        room: SoundListRoom,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(list) = &mut self.notifications.sound_list else {
            return;
        };
        if list.measured != Some(room) {
            list.measured = Some(room);
            list.scroll.scroll_to_item(list.highlight);
            cx.defer_in(window, |_, window, _| window.refresh());
        }
    }

    /// Whether a held Enter, Space or Esc does nothing: on a sound field or
    /// in an open sound list, where its repeats would strobe the list or
    /// close Settings behind it.
    fn sound_ignores_held_key(&self, key: &str, window: &Window, cx: &App) -> bool {
        matches!(key, "enter" | "space" | "escape")
            && (self.notifications.sound_list.is_some()
                || matches!(
                    self.focused_settings_control(window, cx),
                    Some(SettingsControl::NeedsYouSoundField | SettingsControl::FinishedSoundField)
                ))
    }

    fn hover_sound_entry(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(list) = &mut self.notifications.sound_list
            && list.highlight != index
        {
            list.highlight = index;
            cx.notify();
        }
    }

    /// A key while a sound list is open; returns whether it was the list's:
    /// Up and Down move the highlight, stopping at either end, Enter or Space
    /// chooses it, Esc closes the list, and Tab closes it and walks on.
    fn on_sound_list_key(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.settings_tab != SettingsTab::Notifications {
            return false;
        }
        let Some(list) = &mut self.notifications.sound_list else {
            return false;
        };
        let entries = SoundEntry::ALL;
        match key {
            "up" | "down" => {
                list.highlight = if key == "down" {
                    (list.highlight + 1).min(entries.len() - 1)
                } else {
                    list.highlight.saturating_sub(1)
                };
                list.scroll.scroll_to_item(list.highlight);
                cx.notify();
                true
            }
            "enter" | "space" => {
                let kind = list.kind;
                if let Some(entry) = entries.get(list.highlight) {
                    self.choose_sound_entry(kind, *entry, window, cx);
                }
                true
            }
            "escape" => {
                self.close_sound_list(cx);
                true
            }
            "tab" => {
                self.close_sound_list(cx);
                false
            }
            _ => false,
        }
    }

    /// A choice in `kind`'s sound list: Off and the sounds apply at once, the
    /// row's default entry as the Default choice; Custom asks this PC's file
    /// picker for a `.wav` or `.ogg`, and a cancel keeps the choice as it was.
    /// The keyboard stays on the field.
    fn choose_sound_entry(
        &mut self,
        kind: NotifyKind,
        entry: SoundEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_sound_list(cx);
        self.move_settings_focus(sound_field(kind), window, cx);
        let sound = match entry {
            SoundEntry::Off => ToastSound::Off,
            SoundEntry::Sound(sound) if sound == kind.default_sound() => ToastSound::Default,
            SoundEntry::Sound(sound) => ToastSound::Bundled(sound),
            SoundEntry::Custom => {
                match kind {
                    NotifyKind::NeedsYou => self.pick_needs_you_sound(cx),
                    NotifyKind::Finished => self.pick_finished_sound(cx),
                }
                return;
            }
        };
        match kind {
            NotifyKind::NeedsYou => self.set_needs_you_sound(sound, cx),
            NotifyKind::Finished => self.set_finished_sound(sound, cx),
        }
    }

    /// Asks for the custom sound file, opening at the current one's folder.
    /// The sound plays on this PC, so the picker is always this PC's, even
    /// when the window is on another host.
    fn pick_needs_you_sound(&mut self, cx: &mut Context<Self>) {
        let start = match &self.shown_local().notifications.needs_you_sound {
            ToastSound::Custom(path) => path.parent().map(std::path::Path::to_path_buf),
            ToastSound::Off | ToastSound::Default | ToastSound::Bundled(_) => None,
        };
        let filter = FileFilter::new("Sound", &SOUND_EXTENSIONS);
        let picked = (self.pick_file)(cx, start, filter);
        cx.spawn(async move |this, cx| {
            let Some(path) = picked.await else {
                return;
            };
            // Fails only when the view is gone, and Settings with it.
            this.update(cx, |this, cx| {
                this.set_needs_you_sound(ToastSound::Custom(path), cx);
            })
            .ok();
        })
        .detach();
    }

    /// Chooses `sound`, pending until Apply, and checks its file.
    fn set_needs_you_sound(&mut self, sound: ToastSound, cx: &mut Context<Self>) {
        self.pend_local(LocalPending::NeedsYouSound(sound));
        self.check_needs_you_sound(cx);
        cx.notify();
    }

    /// Apply reaching a pending Needs you sound: records `sound`, saving it
    /// when it changed; Off drops the clip waiting to play, and the one
    /// playing ends on its own.
    fn commit_needs_you_sound(&mut self, sound: ToastSound) {
        if sound == ToastSound::Off {
            self.notifications
                .sounder
                .drop_waiting(crate::sound::NotifyKind::NeedsYou);
        }
        if self.sidebar.set_needs_you_sound(sound) {
            self.save_ui();
        }
    }

    /// Checks the chosen custom sound file on a background thread; the tab
    /// shows what is wrong with it once the check is back, unless the
    /// choice changed meanwhile.
    fn check_needs_you_sound(&mut self, cx: &mut Context<Self>) {
        let choice = self.shown_local().notifications.needs_you_sound;
        self.notifications.sound_problem = None;
        let checked = choice.clone();
        let check = cx
            .background_executor()
            .spawn(async move { sound::problem_of(&checked) });
        cx.spawn(async move |this, cx| {
            let problem = check.await;
            // Fails only when the view is gone, and Settings with it.
            this.update(cx, |this, cx| {
                if this.shown_local().notifications.needs_you_sound == choice {
                    this.notifications.sound_problem = problem;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Asks for the custom sound file, opening at the current one's folder.
    /// The sound plays on this PC, so the picker is always this PC's, even
    /// when the window is on another host.
    fn pick_finished_sound(&mut self, cx: &mut Context<Self>) {
        let start = match &self.shown_local().notifications.finished_sound {
            ToastSound::Custom(path) => path.parent().map(std::path::Path::to_path_buf),
            ToastSound::Off | ToastSound::Default | ToastSound::Bundled(_) => None,
        };
        let filter = FileFilter::new("Sound", &SOUND_EXTENSIONS);
        let picked = (self.pick_file)(cx, start, filter);
        cx.spawn(async move |this, cx| {
            let Some(path) = picked.await else {
                return;
            };
            // Fails only when the view is gone, and Settings with it.
            this.update(cx, |this, cx| {
                this.set_finished_sound(ToastSound::Custom(path), cx);
            })
            .ok();
        })
        .detach();
    }

    /// Chooses `sound`, pending until Apply, and checks its file.
    fn set_finished_sound(&mut self, sound: ToastSound, cx: &mut Context<Self>) {
        self.pend_local(LocalPending::FinishedSound(sound));
        self.check_finished_sound(cx);
        cx.notify();
    }

    /// Apply reaching a pending Finished sound: records `sound`, saving it
    /// when it changed; Off drops the clip waiting to play, and the one
    /// playing ends on its own.
    fn commit_finished_sound(&mut self, sound: ToastSound) {
        if sound == ToastSound::Off {
            self.notifications
                .sounder
                .drop_waiting(crate::sound::NotifyKind::Finished);
        }
        if self.sidebar.set_finished_sound(sound) {
            self.save_ui();
        }
    }

    /// Checks the chosen custom sound file on a background thread; the tab
    /// shows what is wrong with it once the check is back, unless the
    /// choice changed meanwhile.
    fn check_finished_sound(&mut self, cx: &mut Context<Self>) {
        let choice = self.shown_local().notifications.finished_sound;
        self.notifications.finished_sound_problem = None;
        let checked = choice.clone();
        let check = cx
            .background_executor()
            .spawn(async move { sound::problem_of(&checked) });
        cx.spawn(async move |this, cx| {
            let problem = check.await;
            // Fails only when the view is gone, and Settings with it.
            this.update(cx, |this, cx| {
                if this.shown_local().notifications.finished_sound == choice {
                    this.notifications.finished_sound_problem = problem;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// A press on field `field` of the shown agent page, made on the
    /// control `selector` names: its choice `choice`, or its toggle when
    /// `None`. Asks the daemon to store the value; the row shows it when
    /// the daemon's broadcast arrives. A locked field sends nothing, and so
    /// does a press whose control is not the indexed field's, as when a
    /// `when` rule changed the page under it.
    fn press_agent_field(&mut self, field: usize, choice: Option<usize>, selector: &str) {
        let Some(page) = self.shown_page() else {
            tracing::info!("settings: the daemon lists no agent for this page; nothing sent");
            return;
        };
        let Some(view) = page.fields.get(field) else {
            tracing::warn!(
                field,
                "settings: the pressed field left the page; nothing sent"
            );
            return;
        };
        if !control_matches(&page.tag, field, view, selector) {
            tracing::debug!("settings: pressed control does not match its field; ignored");
            return;
        }
        if view.disabled {
            tracing::info!(key = %view.id, "settings: the field is locked; nothing sent");
            return;
        }
        let Some(value) = pressed_value(view, choice) else {
            tracing::warn!(
                key = %view.id,
                ?choice,
                "settings: the press matches none of the field's choices; nothing sent"
            );
            return;
        };
        let host = self.settings_host.clone();
        let pending = |value| HostPending::AgentField {
            agent: page.id.clone(),
            field: view.id.clone(),
            value,
        };
        let saved = self
            .saved_agent_page(&host, &page.id)
            .and_then(|saved| saved.fields.into_iter().find(|field| field.id == view.id))
            .map(|field| pending(field.value));
        self.settings_pending
            .set_host(&host, pending(value), saved.as_ref());
    }

    /// Sends `host`'s agent `agent`'s Settings values with `key` set to
    /// `value`, on top of any change still waiting for that daemon's
    /// broadcast, under a fresh request id; `from_text` marks the apply of
    /// text field `key`. Nothing is sent while the agent's rows are
    /// disabled. A send records `host` in `spawn.trusted_moved`
    /// ([`crate::sidebar::SidebarModel::mark_trusted_moved`]).
    fn set_agent_setting(
        &mut self,
        host: &HostKey,
        agent: &AgentId,
        key: &str,
        value: OptionValue,
        from_text: bool,
    ) {
        if !self.agent_rows_live(host, agent) {
            tracing::info!(agent = %agent.0, key, "settings: the agent's rows are disabled; nothing sent");
            return;
        }
        let Some(options) = self.merge_agent_setting(host, agent, key, value.clone()) else {
            return;
        };
        let Some(mirrors) = self.mirrors_of_mut(host) else {
            return;
        };
        let request_id = crate::new_request_id();
        tracing::info!(agent = %agent.0, ?host, key, %request_id, "settings: setting an agent option");
        mirrors.agent_setting_applies.push_back(SettingApply {
            id: request_id.clone(),
            agent: agent.clone(),
            key: key.to_owned(),
            value,
            from_text,
        });
        while mirrors.agent_setting_applies.len() > WAITING_SETTING_APPLIES {
            mirrors.agent_setting_applies.pop_front();
        }
        self.send_setting(
            host,
            ClientMessage::SetAgentSettings {
                agent: agent.clone(),
                options,
                request_id: Some(request_id),
            },
        );
        // A host whose agent Settings this client changed never takes the
        // Trusted launch default older releases saved on top of them.
        if self.sidebar.mark_trusted_moved(host) {
            self.save_ui();
        }
    }

    /// Whether `host`'s agent `agent`'s Settings rows take input: `host` is
    /// connected, its connection has heard the agents' Settings values, and
    /// its daemon lists the agent.
    fn agent_rows_live(&self, host: &HostKey, agent: &AgentId) -> bool {
        self.conn_of(host).is_some_and(crate::Connection::is_open)
            && self
                .mirrors_of(host)
                .is_some_and(|mirrors| mirrors.agent_settings_received)
            && descriptor_for(self.sidebar.agents(host), agent).is_some()
    }

    /// Opens Windows' notification settings on a background thread; a
    /// failure is only logged.
    fn open_windows_notification_settings(&self, cx: &mut Context<Self>) {
        let opener = Arc::clone(&self.opener);
        cx.background_executor()
            .spawn(async move {
                if let Err(err) = opener.url(WINDOWS_NOTIFICATION_SETTINGS) {
                    tracing::warn!("could not open Windows' notification settings: {err}");
                }
            })
            .detach();
    }

    /// A key while Settings is open; returns whether it was the frame's.
    /// An open sound list has the first say. Tab and Shift+Tab walk the ring
    /// of the tab list and the shown tab's controls, Up and Down on the tab
    /// list change the tab, Space or Enter presses the focused control, and
    /// the arrows move along an Appearance swatch line or family list. While
    /// a text field has the keyboard, only Tab is the frame's.
    pub(crate) fn on_settings_key(
        &mut self,
        keystroke: &Keystroke,
        held: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // The Accounts page's Remove confirm takes every key while it is
        // open, before the frame's own keys and the page beneath it.
        if self.on_account_remove_key(keystroke, window, cx) {
            return true;
        }
        let mods = &keystroke.modifiers;
        let ctrl_only =
            mods.control && !mods.alt && !mods.shift && !mods.platform && !mods.function;
        if self.settings_open() && ctrl_only && keystroke.key == "f" {
            // An open sound or shell list closes first, as Tab closes it.
            self.notifications.sound_list = None;
            self.settings_pending.shell_list = None;
            self.focus_settings_search(true, window, cx);
            return true;
        }
        if !self.settings_open() || mods.control || mods.alt || mods.platform || mods.function {
            return false;
        }
        if held && self.sound_ignores_held_key(&keystroke.key, window, cx) {
            return true;
        }
        if self.on_sound_list_key(&keystroke.key, window, cx)
            || self.on_shell_list_key(&keystroke.key, window, cx)
        {
            return true;
        }
        let on_list = self.settings_tabs_focus.is_focused(window);
        let typing = self.settings_typing(window, cx);
        match (keystroke.key.as_str(), mods.shift) {
            ("tab", back) => {
                self.step_settings_focus(back, window, cx);
                true
            }
            _ if typing => false,
            ("up" | "down", false) if on_list => {
                // While searching, Up and Down walk the listed tabs only.
                let tabs = self.listed_tabs(cx);
                if !tabs.is_empty() {
                    let tab = self.settings_tab.step(&tabs, keystroke.key == "down");
                    self.select_settings_tab(tab, window, cx);
                }
                true
            }
            ("space" | "enter", false) => match self.focused_settings_control(window, cx) {
                Some(control) => {
                    self.press_settings_control(control, window, cx);
                    true
                }
                None => false,
            },
            (key, false) => match self.focused_settings_control(window, cx) {
                Some(SettingsControl::Appearance(stop)) => {
                    self.on_appearance_stop_key(stop, key, cx)
                }
                _ => false,
            },
            _ => false,
        }
    }

    /// Whether a text field in the Settings modal or the appearance editor
    /// has the keyboard, so keys other than Tab are its.
    pub(crate) fn settings_typing(&self, window: &Window, cx: &App) -> bool {
        self.settings_search_focused(window)
            || self.worktrees_path_focused(window)
            || self.remote_port_focused(window)
            || self.chat_width_focused(window)
            || self.shell_command_focused(window)
            || self.launch_program_focused(window)
            || self.launch_args_focused(window)
            || self.agent_texts_focused(window)
            || self.dashboard_fields_focused(window)
            || self.account_alias_focused(window)
            || self.appearance_field_focused(window, cx)
    }

    /// Whether the Default shell row's Custom command field has the
    /// keyboard.
    fn shell_command_focused(&self, window: &Window) -> bool {
        self.settings_open()
            && self.settings_tab == SettingsTab::General
            && self
                .shell_command_field()
                .is_some_and(|field| field.focus.is_focused(window))
    }

    /// Whether one of the shown Dashboard tab's text fields has the
    /// keyboard.
    fn dashboard_fields_focused(&self, window: &Window) -> bool {
        self.settings_open()
            && self.settings_tab == SettingsTab::Dashboard
            && self.shown_dashboard_tab().is_some_and(|tab| {
                [&tab.key_focus, &tab.variable_focus, &tab.cap_focus]
                    .into_iter()
                    .any(|focus| focus.is_focused(window))
            })
    }

    /// Takes `host`'s daemon's [`DaemonMessage::KeepAwakeStatus`] into that
    /// host's settings.
    pub(crate) fn on_keep_awake_status(&mut self, host: &HostKey, msg: &DaemonMessage) {
        if let DaemonMessage::KeepAwakeStatus { mode, active, .. } = *msg
            && let Some(mirrors) = self.mirrors_of_mut(host)
        {
            mirrors.keep_awake = Some(KeepAwake { mode, active });
        }
    }

    /// Chooses `mode` on the shown host, pending until Apply. Nothing is
    /// chosen while the daemon has not reported its mode.
    fn choose_keep_awake_mode(&mut self, mode: KeepAwakeMode) {
        if let Some(state) = self.settings_mirrors().keep_awake {
            let host = self.settings_host.clone();
            self.settings_pending.set_host(
                &host,
                HostPending::KeepAwake(mode),
                Some(&HostPending::KeepAwake(state.mode)),
            );
        }
    }

    /// Apply reaching a pending This PC setting: it is recorded and saved,
    /// and what it changes follows now. Returns whether it could not be
    /// applied, in which case it stays pending.
    fn commit_local(
        &mut self,
        value: LocalPending,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (saves, applied) = match value {
            LocalPending::Autostart(on) => {
                let applied = self.commit_autostart(on);
                if !applied {
                    let saved = self
                        .local_with(&[])
                        .value_like(&LocalPending::Autostart(on));
                    self.settings_pending
                        .set_local(LocalPending::Autostart(on), &saved);
                }
                (false, applied)
            }
            LocalPending::SidebarView(view) => {
                self.set_sidebar_view(view, cx);
                (false, true)
            }
            LocalPending::CountMode(_)
            | LocalPending::TitleCount(_)
            | LocalPending::TitleSuffix(_) => {
                self.commit_title(&value, window, cx);
                (false, true)
            }
            LocalPending::NotifyNeedsYou(_)
            | LocalPending::NeedsYouSound(_)
            | LocalPending::NotifyFinished(_)
            | LocalPending::NotifyAccountSwitches(_)
            | LocalPending::FinishedSound(_)
            | LocalPending::NotifySkip(_)
            | LocalPending::FinishedGrace(_)
            | LocalPending::ToastStyle(_) => (self.commit_notification(value), true),
            other => (self.commit_general(&other, cx), true),
        };
        if saves {
            self.save_ui();
        }
        cx.notify();
        !applied
    }

    /// Apply reaching the login-launch switch: the launch is written and
    /// read back; a failed write leaves it as it was and says why under the
    /// toggle. Returns whether it was written.
    fn commit_autostart(&mut self, on: bool) -> bool {
        match self.autostart.set(on) {
            Ok(()) => {
                let read = self.read_autostart();
                if let Some(tab) = &mut self.remote_access_tab {
                    tab.autostart_error = None;
                    tab.autostart = read;
                }
                true
            }
            Err(error) => {
                tracing::warn!("settings: switching the login launch failed: {error}");
                if let Some(tab) = &mut self.remote_access_tab {
                    tab.autostart_error = Some(autostart_failure_line(&error));
                }
                false
            }
        }
    }

    /// Apply reaching a pending App title setting: recorded, saved, and the
    /// window's title and badge follow.
    fn commit_title(&mut self, value: &LocalPending, window: &mut Window, cx: &mut Context<Self>) {
        match *value {
            LocalPending::CountMode(mode) => {
                if self.sidebar.set_count_mode(mode) {
                    self.after_title_setting(window, cx);
                }
            }
            LocalPending::TitleCount(on) => {
                self.sidebar.set_title_show_count(on);
                self.after_title_setting(window, cx);
            }
            LocalPending::TitleSuffix(on) => {
                self.sidebar.set_title_suffix(on);
                self.after_title_setting(window, cx);
            }
            _ => {}
        }
    }

    /// Apply reaching a pending Notifications setting: recorded; returns
    /// whether it is left to the caller to save.
    fn commit_notification(&mut self, value: LocalPending) -> bool {
        match value {
            LocalPending::NotifyNeedsYou(on) => {
                self.sidebar.set_notification(on);
                true
            }
            LocalPending::NeedsYouSound(sound) => {
                self.commit_needs_you_sound(sound);
                false
            }
            LocalPending::NotifyFinished(on) => {
                self.sidebar.set_finished_notification(on);
                true
            }
            LocalPending::NotifyAccountSwitches(on) => {
                self.sidebar.set_account_switches_notification(on);
                true
            }
            LocalPending::FinishedSound(sound) => {
                self.commit_finished_sound(sound);
                false
            }
            LocalPending::NotifySkip(skip) => self.sidebar.set_notify_skip(skip),
            LocalPending::FinishedGrace(secs) => self.sidebar.set_finished_grace(secs),
            LocalPending::ToastStyle(style) => self.sidebar.set_toast_style(style),
            _ => false,
        }
    }

    /// Apply reaching a pending General setting:
    /// recorded; returns whether it is left to the caller to save.
    fn commit_general(&mut self, value: &LocalPending, cx: &mut Context<Self>) -> bool {
        match *value {
            LocalPending::LeafDensity(density) => self.sidebar.set_leaf_density(density),
            LocalPending::SessionsUnderRepos(mode) => self.sidebar.set_sessions_under_repos(mode),
            LocalPending::RepoSort(sort) => self.sidebar.set_repo_sort(sort),
            LocalPending::CopyOnSelect(on) => {
                self.sidebar.set_copy_on_select(on);
                cx.set_global(CopyOnSelect(on));
                true
            }
            LocalPending::CheckUpdates(on) => {
                self.sidebar.set_check_updates(on);
                true
            }
            LocalPending::DetailedLogging(on) => {
                self.sidebar.set_detailed_logging(on);
                let outcome = crate::logging::set_detailed(on);
                crate::logging::log_outcome(outcome, on);
                if self.conn().supports(FEATURE_DETAILED_LOGGING) {
                    self.send_setting(
                        &HostKey::Local,
                        ClientMessage::SetDetailedLogging { enabled: on },
                    );
                }
                true
            }
            LocalPending::ChatMode(mode) => self.sidebar.set_chat_mode(mode),
            _ => false,
        }
    }

    fn after_title_setting(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_ui();
        self.refresh_window_title(window, cx);
        self.schedule_taskbar_badge(window, cx);
        cx.notify();
    }

    /// The title the window should show now: the active tab's name as the
    /// App title settings shape it.
    fn wanted_window_title(&self) -> String {
        let name = self
            .tabs
            .active_tab()
            .map(|tab| self.sidebar.tab_display_name(tab));
        compute_title(name.as_deref(), &self.sidebar.ui_state().title)
    }

    /// Sets the window title [`TITLE_DEBOUNCE`] after it last changed; a
    /// change inside the wait restarts it.
    pub(crate) fn refresh_window_title(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wanted = self.wanted_window_title();
        let waiting = self.title_pending.as_ref().unwrap_or(&self.window_title);
        if *waiting == wanted {
            return;
        }
        if wanted == self.window_title {
            self.title_pending = None;
            self.title_timer = None;
            return;
        }
        self.title_pending = Some(wanted);
        self.title_timer = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(TITLE_DEBOUNCE).await;
            // Fails only when the view is gone, and its window with it.
            this.update_in(cx, |this, window, _| this.apply_window_title(window))
                .ok();
        }));
    }

    fn apply_window_title(&mut self, window: &mut Window) {
        self.title_timer = None;
        if let Some(title) = self.title_pending.take() {
            window.set_window_title(&title);
            self.window_title = title;
        }
    }

    /// At the top of the render, before any layer draws: this root's
    /// search hit goes where its rows read it, or, with none (a blank query
    /// or Settings closed), any hit it left there is cleared, so no
    /// appearance editor outlines a row.
    pub(crate) fn publish_search_hit(&self, cx: &mut Context<Self>) {
        let slot = cx.try_global::<SearchHit>().and_then(|shown| shown.0);
        let next = next_hit_slot(slot, cx.entity_id(), self.search_hit(cx));
        if cx.try_global::<SearchHit>().is_none() || next != slot {
            cx.set_global(SearchHit(next));
        }
    }

    /// The Settings modal over a backdrop that takes every click beneath
    /// it; a click on the backdrop itself does nothing.
    pub(crate) fn settings_layer(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.settings_open() {
            return None;
        }
        let close = close_button("settings-close", false, &theme(cx)).on_click(cx.listener(
            |this, _: &ClickEvent, window, cx| {
                this.commit_alias_on_close(cx);
                this.close_appearance_editor(window, cx);
            },
        ));
        // The body takes what the window leaves between the title bar and
        // the footer, and the content scrolls within it.
        let body = div()
            .flex()
            .flex_1()
            .min_h(px(0.0))
            .child(self.settings_tab_list(window, cx))
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(
                        div()
                            .id("settings-content")
                            .debug_selector(|| "settings-content".to_owned())
                            .overflow_y_scroll()
                            .track_scroll(self.settings_scroll.handle())
                            .flex()
                            .flex_col()
                            .w_full()
                            .max_w(px(CONTENT_WIDTH + 2.0 * CONTENT_PAD_X))
                            .min_w(px(0.0))
                            .flex_shrink()
                            .px(px(CONTENT_PAD_X))
                            .py(px(CONTENT_PAD_Y))
                            .children(self.settings_content(window, cx)),
                    )
                    .child(self.settings_scroll.overlay()),
            );
        let cap = window.viewport_size().height / px(1.0) - 2.0 * PANEL_MARGIN_Y;
        let width =
            (window.viewport_size().width / px(1.0) - 2.0 * PANEL_MARGIN_X).min(PANEL_WIDTH);
        let panel = dialog_card(
            crate::fonts::ui_text_size(cx),
            "settings-panel",
            width,
            &theme(cx),
        )
        .h(px(PANEL_HEIGHT.min(cap)))
        .track_focus(&self.appearance_focus)
        .child(dialog_title_bar(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            "Settings",
            close,
            &theme(cx),
        ))
        .child(body)
        .child(self.settings_footer(window, cx));
        Some(backdrop("settings", panel, &theme(cx)))
    }

    /// The footer: OK, Apply and Cancel, right-aligned. OK and Apply are
    /// disabled while [`Self::settings_apply_enabled`] says no; OK also
    /// takes a press while an Accounts alias field holds an edit, which it
    /// sends.
    fn settings_footer(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let focused = self.focused_settings_control(window, cx);
        let applies = self.settings_apply_enabled();
        let ok = applies || self.account_alias_edited(cx);
        dialog_footer(&theme(cx))
            .child(settings_button(
                SettingsControl::FooterOk,
                "OK",
                ok,
                focused,
                cx,
            ))
            .child(settings_button(
                SettingsControl::FooterApply,
                "Apply",
                applies,
                focused,
                cx,
            ))
            .child(settings_button(
                SettingsControl::FooterCancel,
                "Cancel",
                true,
                focused,
                cx,
            ))
    }

    /// The tab list on the raised ground; the shown tab is filled and bold,
    /// and ringed while the list has the keyboard. A group row is a label
    /// only: no id, no click, no hover and no ring.
    fn settings_tab_list(&self, window: &Window, cx: &mut Context<Self>) -> Stateful<Div> {
        let t = theme(cx);
        let list_focused = self.settings_tabs_focus.is_focused(window);
        // While the query holds a word, only the tabs holding a match show,
        // each with its count, or one line saying none does.
        let rows: Vec<(SettingsRow, Option<usize>)> = match self.search_hits(cx) {
            Some(hits) => self.search_rows(&hits),
            None => self
                .settings_rows()
                .into_iter()
                .map(|row| (row, None))
                .collect(),
        };
        let no_match = (rows.is_empty() && self.search_query(cx).is_some()).then(|| {
            div()
                .driver_id(|| "settings-search-no-match".to_owned())
                .px(px(12.0))
                .py(px(8.0))
                .text_color(gpui::rgb(t.muted))
                .child(NO_MATCH)
        });
        let tabs: Vec<AnyElement> = rows
            .into_iter()
            .map(|(row, count)| match row {
                SettingsRow::Group(label) => group_row(
                    crate::fonts::ui_text_size(cx),
                    crate::fonts::ui_weight_shift(cx),
                    label,
                    &t,
                )
                .into_any_element(),
                SettingsRow::Tab(tab) => self
                    .settings_tab_row(tab, list_focused, count, cx)
                    .into_any_element(),
            })
            .collect();
        div()
            .id("settings-tabs")
            .track_focus(&self.settings_tabs_focus)
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(2.0))
            .w(px(TAB_LIST_WIDTH))
            .px(px(10.0))
            .py(px(12.0))
            .bg(gpui::rgb(t.modal_inset))
            .border_r_1()
            .border_color(gpui::rgb(t.card_edge))
            .children(self.settings_search_field(window, cx))
            .children(self.settings_host_row(cx))
            .children(tabs)
            .children(no_match)
    }

    /// The host selector over the tab list, while the window holds two or
    /// more hosts: a HOST section label, then one row per held host, the
    /// shown one filled. A host not connected is dimmed and takes no click.
    /// It is not a keyboard stop.
    fn settings_host_row(&self, cx: &mut Context<Self>) -> Option<Div> {
        let choices = self.settings_host_choices();
        if choices.is_empty() {
            return None;
        }
        let t = theme(cx);
        let size = crate::fonts::ui_text_size(cx);
        let shown_weight = crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD);
        let rows: Vec<AnyElement> = choices
            .into_iter()
            .map(|(host, open)| self.settings_host_choice(host, open, shown_weight, cx))
            .collect();
        Some(
            div()
                .driver_id(|| "settings-host-row".to_owned())
                .flex()
                .flex_col()
                .gap(px(2.0))
                .pb(px(8.0))
                .child(section_label(
                    size,
                    crate::fonts::ui_weight_shift(cx),
                    "Host",
                    &t,
                ))
                .children(rows),
        )
    }

    /// One row of the host selector: the host's name, and below it, when the
    /// host reports a version, that version and an older host's Behind tag.
    fn settings_host_choice(
        &self,
        host: HostKey,
        open: bool,
        shown_weight: FontWeight,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = theme(cx);
        let shown = host == self.settings_host;
        let dim = !shown && !open;
        let key = host.wire().unwrap_or("local").to_owned();
        let selector = format!("settings-host-{key}");
        let label = crate::sidebar::host_label(&host);
        let version = self.settings_host_version(&host);
        let label_key = key.clone();
        let name_key = key.clone();
        let version_key = key.clone();
        let behind_key = key;
        let text = match (shown, open) {
            (true, _) => t.text,
            (false, true) => t.text_2,
            (false, false) => t.muted,
        };
        div()
            .id(gpui::SharedString::from(selector.clone()))
            .driver_id(move || selector.clone())
            .flex()
            .flex_col()
            .flex_none()
            .justify_center()
            .min_h(crate::fonts::ui_px(cx, TAB_ROW_HEIGHT))
            .py(px(6.0))
            .px(px(12.0))
            .rounded(px(TAB_ROW_RADIUS))
            .text_color(gpui::rgb(text))
            .when(dim, |row| row.opacity(OFFLINE_HOST_OPACITY))
            .when(shown, |row| {
                row.bg(gpui::rgb(t.selection)).font_weight(shown_weight)
            })
            .when(!shown && open, |row| {
                row.cursor_pointer()
                    .hover(|style| style.bg(gpui::rgb(t.popover_hover)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.choose_settings_host(host.clone(), window, cx);
                    }))
            })
            .child(
                crate::ellipsis::truncating(
                    &format!("settings-host-label-{label_key}"),
                    StyledText::new(label),
                )
                .driver_id(move || format!("settings-host-name-{name_key}")),
            )
            .children(version.as_ref().map(|(version, behind)| {
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x(px(6.0))
                    .gap_y(px(2.0))
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .child(
                        div()
                            .driver_id(move || format!("settings-host-version-{version_key}"))
                            .flex_none()
                            .font_family(crate::fonts::DEFAULT_FAMILY)
                            .font_weight(crate::fonts::mono_weight())
                            .text_size(crate::fonts::ui_px_at(crate::fonts::ui_text_size(cx), 10.5))
                            .text_color(gpui::rgb(t.muted))
                            .child(format!("v{version}")),
                    )
                    .children(behind.then(|| {
                        crate::connection_picker_view::tag(
                            crate::fonts::ui_text_size(cx),
                            crate::fonts::ui_weight_shift(cx),
                            format!("settings-host-behind-{behind_key}"),
                            "Behind".to_owned(),
                            &t,
                        )
                    }))
            }))
            .into_any_element()
    }

    /// One clickable tab row; the shown one is filled, bold, and ringed
    /// while the list has the keyboard. While searching, `count` follows
    /// the label, muted: how many of the tab's rows match.
    fn settings_tab_row(
        &self,
        tab: SettingsTab,
        list_focused: bool,
        count: Option<usize>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let t = theme(cx);
        let shown = tab == self.settings_tab;
        let selector = tab.selector(self.agents());
        let label = tab.label(self.agents());
        let (text, weight) = if shown {
            (t.text, FontWeight::SEMIBOLD)
        } else {
            (t.text_2, FontWeight::MEDIUM)
        };
        div()
            .id(selector)
            .driver_id(|| selector.to_owned())
            .flex()
            .flex_none()
            .items_center()
            .h(crate::fonts::ui_px(cx, TAB_ROW_HEIGHT))
            .px(px(12.0))
            .rounded(px(TAB_ROW_RADIUS))
            .border_1()
            .border_color(gpui::rgba(t.transparent))
            .font_weight(crate::fonts::ui_weight(cx, weight))
            .text_color(gpui::rgb(text))
            .cursor_pointer()
            .when(shown, |row| row.bg(gpui::rgb(t.selection)))
            .when(!shown, |row| {
                row.hover(|style| {
                    style
                        .bg(gpui::rgb(t.popover_hover))
                        .text_color(gpui::rgb(t.text))
                })
            })
            .when(shown && list_focused, |row| {
                row.child(focus_ring(TAB_ROW_RADIUS, &theme(cx)))
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.select_settings_tab(tab, window, cx);
            }))
            .child(label)
            .children(count.map(|count| {
                div()
                    .driver_id(move || format!("{selector}-count"))
                    .text_color(gpui::rgb(t.muted))
                    .whitespace_nowrap()
                    .child(format!("  {count}"))
            }))
            .children(if tab == SettingsTab::RemoteAccess {
                self.remote_access_tab_badge(
                    crate::fonts::ui_text_size(cx),
                    crate::fonts::ui_weight_shift(cx),
                    &t,
                )
            } else {
                None
            })
    }

    fn settings_content(&self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let focused = self.focused_settings_control(window, cx);
        match self.settings_tab {
            SettingsTab::General => self.general_tab(focused, cx),
            SettingsTab::Appearance => self
                .appearance_body(focused.and_then(SettingsControl::appearance_stop), cx)
                .map(IntoElement::into_any_element)
                .into_iter()
                .collect(),
            SettingsTab::AppTitle => self.app_title_tab(focused, cx),
            SettingsTab::Notifications => self.notifications_tab(focused, cx),
            SettingsTab::Dashboard => self.dashboard_tab_content(focused, cx),
            SettingsTab::Agent(_) => self.agent_page_content(focused, cx),
            SettingsTab::Worktrees => self.worktrees_tab_content(focused, cx),
            SettingsTab::RemoteAccess => self.remote_access_tab_content(focused, cx),
            SettingsTab::Accounts => self.accounts_tab_content(focused, cx),
        }
    }

    /// The Worktrees tab: the path field and Browse, the root in use, and
    /// Save, Reset to default and Manage worktrees….
    fn worktrees_tab_content(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let t = theme(cx);
        let Some(tab) = self.shown_worktrees_tab() else {
            return Vec::new();
        };
        let path = SettingsControl::WorktreesPath;
        let field = field_frame(focused == Some(path), Layer::Floating, &theme(cx))
            .driver_id(|| path.selector().to_owned())
            .children(search_hit_marker(path.selector(), SEGMENT_RADIUS, cx))
            .flex_1()
            .min_w(px(0.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.press_settings_control(path, window, cx);
                }),
            )
            .child(tab.input.clone());
        let browse = self.worktrees_browses().then(|| {
            settings_button(
                SettingsControl::WorktreesBrowse,
                "Browse…",
                true,
                focused,
                cx,
            )
        });
        let path_row = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child("Path")
            .child(field)
            .children(browse);
        let active = div()
            .driver_id(|| "settings-worktrees-active".to_owned())
            .text_color(gpui::rgb(t.text_2))
            .child(self.worktrees_active_line());
        let buttons = div()
            .flex()
            .gap(px(8.0))
            .child(settings_button(
                SettingsControl::WorktreesReset,
                "Reset to default",
                self.worktrees_reset_enabled(),
                focused,
                cx,
            ))
            .children(self.worktrees_manages().then(|| {
                settings_button(
                    SettingsControl::WorktreesManage,
                    "Manage worktrees…",
                    true,
                    focused,
                    cx,
                )
            }));
        let host_hint = self.worktrees_host_note().map(|note| {
            crate::tagged_text("settings-worktrees-host-hint", note)
                .text_size(crate::fonts::ui_px(cx, HINT_SIZE))
                .text_color(gpui::rgb(t.subtle))
        });
        let section = section(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            "Worktrees root",
        )
        .child(hint(crate::fonts::ui_text_size(cx), WORKTREES_HINT, &t))
        .child(path_row)
        .children(host_hint)
        .child(active)
        .child(buttons);
        vec![section.into_any_element()]
    }

    /// The Remote access page: the switch and what remote access is doing,
    /// the address while serving, the port, and this PC's login launch.
    fn remote_access_tab_content(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let t = theme(cx);
        let Some(tab) = &self.remote_access_tab else {
            return Vec::new();
        };
        let (checked, enabled) = self.remote_toggle();
        let switch = toggle(
            SettingsControl::RemoteToggle,
            ToggleState { checked, enabled },
            "Allow remote access over Tailscale",
            focused,
            cx,
        );
        let status = div()
            .driver_id(|| "settings-remote-status".to_owned())
            .text_color(gpui::rgb(t.text_2))
            .child(self.remote_status_line());
        let address = self.remote_address().map(|url| {
            div()
                .driver_id(|| "settings-remote-address".to_owned())
                .child(format!("Address: {url}"))
        });
        let cert = self.remote_cert_line().map(|line| {
            hint(crate::fonts::ui_text_size(cx), line, &t)
                .driver_id(|| "settings-remote-cert".to_owned())
        });
        let port_error = self.remote_port_error().map(|error| {
            hint(crate::fonts::ui_text_size(cx), error, &t)
                .driver_id(|| "settings-remote-port-error".to_owned())
        });
        let section = section(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            "Remote access (Tailscale)",
        )
        .child(hint(crate::fonts::ui_text_size(cx), REMOTE_INTRO, &t))
        .child(switch)
        .child(status)
        .children(address)
        .children(cert)
        .children(self.waiting_devices_section(cx))
        .children(self.devices_section(cx))
        .child(Self::remote_port_row(tab, enabled, focused, cx))
        .children(port_error)
        .child(hint(crate::fonts::ui_text_size(cx), REMOTE_PORT_HINT, &t))
        .child(hint(crate::fonts::ui_text_size(cx), REMOTE_RUN_HINT, &t))
        .children(self.autostart_rows(tab, focused, cx));
        vec![section.into_any_element()]
    }

    /// The port field; before the daemon reports, it is a dimmed
    /// placeholder that takes no keys.
    fn remote_port_row(
        tab: &RemoteAccessTab,
        enabled: bool,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Div {
        let t = theme(cx);
        let port = SettingsControl::RemotePort;
        let frame = field_frame(focused == Some(port), Layer::Floating, &theme(cx))
            .driver_id(|| port.selector().to_owned())
            .children(search_hit_marker(port.selector(), SEGMENT_RADIUS, cx))
            .w(px(PORT_FIELD_WIDTH));
        let field = if enabled {
            frame
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        this.press_settings_control(port, window, cx);
                    }),
                )
                .child(tab.input.clone())
        } else {
            frame
                .opacity(DISABLED_OPACITY)
                .text_color(gpui::rgb(t.subtle))
                .child(DEFAULT_PORT_TEXT)
        };
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child("Port")
            .child(field)
    }

    /// The login-launch toggle as the page draws it (the launch it read,
    /// with a pending switch over it), its hint, and why its last apply or
    /// read failed; the toggle is inert while the launch could not be read.
    fn autostart_rows(
        &self,
        tab: &RemoteAccessTab,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<Div> {
        let t = theme(cx);
        let state = ToggleState {
            checked: self.remote_autostart_shown() == Some(true),
            enabled: tab.autostart.is_ok(),
        };
        let launch = toggle(
            SettingsControl::RemoteAutostart,
            state,
            "Start the daemon when I log in",
            focused,
            cx,
        );
        let error = self.remote_autostart_line().map(|line| {
            hint(crate::fonts::ui_text_size(cx), line, &t)
                .driver_id(|| "settings-remote-autostart-error".to_owned())
        });
        [
            launch,
            hint(crate::fonts::ui_text_size(cx), AUTOSTART_HINT, &t),
        ]
        .into_iter()
        .chain(error)
        .collect()
    }

    /// The shown agent's page: a section titled with the agent's label
    /// holding its Settings fields in the descriptor's order, when it has
    /// any, then the launch command.
    fn agent_page_content(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(page) = self.shown_page() else {
            return Vec::new();
        };
        let rows_of = RowPage {
            tag: &page.tag,
            live: self.agent_rows_live(&self.settings_host, &page.id),
            tab: self
                .shown_agent_tabs()
                .and_then(|tabs| tabs.tabs.get(&page.id)),
            focused,
        };
        let rows: Vec<AnyElement> = page
            .fields
            .iter()
            .enumerate()
            .filter_map(|(index, view)| agent_field_row(&rows_of, index, view, cx))
            .collect();
        let mut sections = Vec::new();
        if !rows.is_empty() {
            sections.push(
                section(
                    crate::fonts::ui_text_size(cx),
                    crate::fonts::ui_weight_shift(cx),
                    page.label.clone(),
                )
                .children(rows)
                .into_any_element(),
            );
        }
        sections.extend(self.launch_command_section(&page, focused, cx));
        sections.extend(self.user_hooks_section(&page, focused, cx));
        sections
    }

    /// Agent `page`'s Hooks section: the switch that installs the daemon's
    /// hooks in the agent's user settings, its note, the file it edits and
    /// the last answer's error. On a daemon without the feature the switch
    /// is dimmed and inert; for an agent without user hooks, or one its
    /// daemon says it cannot edit, there is no section.
    fn user_hooks_section(
        &self,
        page: &AgentPage,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let row = self.user_hooks_row_of(&self.settings_host, &page.id)?;
        let t = theme(cx);
        let size = crate::fonts::ui_text_size(cx);
        let name = &page.label;
        let switch = toggle(
            SettingsControl::AgentUserHooks,
            ToggleState {
                checked: row.checked,
                enabled: row.enabled,
            },
            format!("Install hooks in {name}'s user settings"),
            focused,
            cx,
        );
        let switch = if row.enabled {
            switch.into_any_element()
        } else {
            div()
                .id("settings-agent-user-hooks-unsupported")
                .tooltip(tooltip(USER_HOOKS_UNSUPPORTED_TIP))
                .child(switch)
                .into_any_element()
        };
        let repair = row.repair.then(|| {
            hint(size, USER_HOOKS_REPAIR_HINT, &t)
                .driver_id(|| "settings-agent-user-hooks-repair".to_owned())
        });
        let path = row.path.as_ref().map(|path| {
            hint(size, format!("Edits {path}"), &t)
                .driver_id(|| "settings-agent-user-hooks-path".to_owned())
        });
        let error = row.error.clone().map(|error| {
            hint(size, error, &t)
                .text_color(gpui::rgb(t.danger_text))
                .driver_id(|| "settings-agent-user-hooks-error".to_owned())
        });
        let section = section(size, crate::fonts::ui_weight_shift(cx), "Hooks")
            .child(switch)
            .child(hint(
                size,
                format!(
                    "Lets {name} runs you start by hand in a shell report their conversation and status. Applies to shells opened afterwards."
                ),
                &t,
            ))
            .children(repair)
            .children(path)
            .children(error);
        Some(section.into_any_element())
    }

    /// Agent `page`'s Launch command section: the Program and Extra
    /// arguments rows and their notes. On a daemon that stores no launch
    /// command the rows are dimmed and inert; while not connected, and
    /// before a capable daemon reported the agent's command, there is no
    /// section.
    fn launch_command_section(
        &self,
        page: &AgentPage,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let t = theme(cx);
        let host = &self.settings_host;
        let live = self.launch_rows_live(host, &page.id);
        if !live && !self.launch_command_unsupported() {
            return None;
        }
        let section = section(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            "Launch command",
        );
        let section = if live {
            let tab = self.shown_agent_tabs()?.tabs.get(&page.id)?;
            let refusal = |row| {
                tab.refusal
                    .as_ref()
                    .filter(|(at, _)| *at == row)
                    .map(|(_, text)| text.clone())
            };
            let program_error = refusal(LaunchCommandRow::Program).map(|text| {
                hint(crate::fonts::ui_text_size(cx), text, &t)
                    .driver_id(|| "settings-agent-program-error".to_owned())
            });
            let args_error = refusal(LaunchCommandRow::Args).map(|text| {
                hint(crate::fonts::ui_text_size(cx), text, &t)
                    .driver_id(|| "settings-agent-args-error".to_owned())
            });
            let env_note = self.program_env_note(&page.id).map(|note| {
                hint(crate::fonts::ui_text_size(cx), note, &t)
                    .driver_id(|| "settings-agent-program-env".to_owned())
            });
            let program = input_frame(
                SettingsControl::AgentProgram,
                &tab.program,
                !self.program_read_only(host, &page.id),
                focused,
                cx,
            );
            let args = input_frame(SettingsControl::AgentArgs, &tab.args, true, focused, cx);
            let saved = self.shows_saved(&page.id, tab).then(|| {
                hint(crate::fonts::ui_text_size(cx), "Saved", &t)
                    .driver_id(|| "settings-agent-saved".to_owned())
            });
            section
                .child(
                    field(
                        crate::fonts::ui_text_size(cx),
                        crate::fonts::ui_weight_shift(cx),
                        "Program",
                        &theme(cx),
                    )
                    .child(program)
                    .children(env_note)
                    .children(program_error),
                )
                .child(
                    field(
                        crate::fonts::ui_text_size(cx),
                        crate::fonts::ui_weight_shift(cx),
                        "Extra arguments",
                        &theme(cx),
                    )
                    .child(args)
                    .children(args_error),
                )
                .children(saved)
        } else {
            section
                .child(disabled_launch_row(
                    crate::fonts::ui_text_size(cx),
                    crate::fonts::ui_weight_shift(cx),
                    "Program",
                    SettingsControl::AgentProgram,
                    page.tag.clone(),
                    &theme(cx),
                ))
                .child(disabled_launch_row(
                    crate::fonts::ui_text_size(cx),
                    crate::fonts::ui_weight_shift(cx),
                    "Extra arguments",
                    SettingsControl::AgentArgs,
                    LAUNCH_ARGS_PLACEHOLDER.to_owned(),
                    &theme(cx),
                ))
        };
        let section = section
            .child(hint(crate::fonts::ui_text_size(cx), LAUNCH_USE_HINT, &t))
            .child(hint(
                crate::fonts::ui_text_size(cx),
                LAUNCH_QUOTING_HINT,
                &t,
            ));
        Some(section.into_any_element())
    }

    fn notifications_tab(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let t = theme(cx);
        let link = SettingsControl::NotifySettingsLink;
        let selector = link.selector();
        let open = div()
            .id(selector)
            .driver_id(|| selector.to_owned())
            .px(px(4.0))
            .rounded(px(LINK_RADIUS))
            .border_1()
            .border_color(gpui::rgba(t.transparent))
            .text_color(gpui::rgb(t.accent))
            .cursor_pointer()
            .hover(|style| {
                style
                    .bg(gpui::rgb(t.popover_hover))
                    .text_color(gpui::rgb(t.text))
            })
            .on_click(press(link, cx))
            .children(search_hit_marker(selector, LINK_RADIUS, cx))
            .when(focused == Some(link), |open| {
                open.child(focus_ring(LINK_RADIUS, &theme(cx)))
            })
            .child("Open Windows notification settings");
        let state = div()
            .driver_id(|| "settings-notify-state".to_owned())
            .child(self.notifications_state_line());
        let windows = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(state)
            .child(open);
        let shown = self.shown_local();
        let settings = &shown.notifications;
        let kinds = self.notify_kind_rows(settings, focused, cx);
        let skip_when = settings.skip_when;
        let skips = NOTIFY_SKIP_CHOICES.map(|choice| {
            let text = match choice {
                NotifySkip::OnScreen => "Session on screen",
                NotifySkip::Focused => "Window focused",
                NotifySkip::Never => "Never",
            };
            (
                SettingsControl::NotifySkip(choice),
                text,
                choice == skip_when,
            )
        });
        let skip_row = choice_row(
            "Skip notifications when",
            &skips,
            RowLock::Live,
            focused,
            cx,
        );
        let grace_row = settings
            .finished
            .then(|| Self::finished_grace_row(settings.finished_grace_secs, focused, cx));
        let section = section(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            "Notifications",
        )
        .child(windows)
        .child(hint(
            crate::fonts::ui_text_size(cx),
            "Fire an OS notification when a session needs you (a question, \
                 a permission prompt or a plan approval) or finishes its turn \
                 with no background work left, and when auto-switch changes or \
                 runs out of accounts.",
            &t,
        ))
        .children(kinds)
        .children(grace_row)
        .child(Self::account_switches_row(settings, focused, cx))
        .child(skip_row);
        #[cfg(windows)]
        let section = section.child(Self::toast_style_row(settings.toast_style, focused, cx));
        vec![section.into_any_element()]
    }

    /// The Account switches switch, as `settings` holds it: the last of the
    /// toggles, after the Finished grace row.
    fn account_switches_row(
        settings: &crate::sidebar::NotificationSettings,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        toggle(
            SettingsControl::NotifyAccountSwitches,
            ToggleState {
                checked: settings.account_switches,
                enabled: true,
            },
            "Account switches",
            focused,
            cx,
        )
        .into_any_element()
    }

    /// The Needs you switch and its sound rows, then the Finished switch
    /// and its sound rows, as `settings` holds them.
    fn notify_kind_rows(
        &self,
        settings: &crate::sidebar::NotificationSettings,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> [AnyElement; 4] {
        let needs_you = toggle(
            SettingsControl::NotifyNeedsYou,
            ToggleState {
                checked: settings.awaiting_input,
                enabled: true,
            },
            "Needs you",
            focused,
            cx,
        );
        let sound = self.needs_you_sound_rows(focused, cx);
        let finished_sound = self.finished_sound_rows(focused, cx);
        let finished = toggle(
            SettingsControl::NotifyFinished,
            ToggleState {
                checked: settings.finished,
                enabled: true,
            },
            "Finished",
            focused,
            cx,
        );
        [
            needs_you.into_any_element(),
            sound.into_any_element(),
            finished.into_any_element(),
            finished_sound.into_any_element(),
        ]
    }

    /// The row choosing how long a Finished toast is held, `chosen` marked.
    fn finished_grace_row(
        chosen: u32,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let choices = FINISHED_GRACE_CHOICES.map(|secs| {
            let text = match secs {
                0 => "None",
                2 => "2 s",
                5 => "5 s",
                10 => "10 s",
                _ => "20 s",
            };
            (SettingsControl::FinishedGrace(secs), text, secs == chosen)
        });
        choice_row(
            "Hold a Finished toast for",
            &choices,
            RowLock::Live,
            focused,
            cx,
        )
        .into_any_element()
    }

    /// The row choosing whose toasts the client raises, `chosen` marked.
    #[cfg(windows)]
    fn toast_style_row(
        chosen: ToastStyle,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let styles = TOAST_STYLE_CHOICES.map(|choice| {
            let text = match choice {
                ToastStyle::Os => "Windows notifications",
                ToastStyle::App => "The app's own",
            };
            (SettingsControl::ToastStyle(choice), text, choice == chosen)
        });
        choice_row("Show toasts as", &styles, RowLock::Live, focused, cx).into_any_element()
    }

    /// The Needs you sound's dropdown with Play beside it (not while the
    /// sound is off), then the custom file's path and what is wrong with it.
    fn needs_you_sound_rows(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Div {
        let t = theme(cx);
        let size = crate::fonts::ui_text_size(cx);
        let shown = self.shown_local();
        let sound = &shown.notifications.needs_you_sound;
        let dropdown = self.sound_dropdown(NotifyKind::NeedsYou, "Needs you sound", focused, cx);
        let play = (*sound != ToastSound::Off).then(|| {
            settings_button(
                SettingsControl::NeedsYouSoundPlay,
                "Play",
                true,
                focused,
                cx,
            )
        });
        let path = match sound {
            ToastSound::Custom(path) => Some(
                hint(size, path.display().to_string(), &t)
                    .driver_id(|| "settings-needs-you-sound-path".to_owned()),
            ),
            ToastSound::Off | ToastSound::Default | ToastSound::Bundled(_) => None,
        };
        let problem = self.notifications.sound_problem.clone().map(|problem| {
            hint(size, problem, &t)
                .text_color(gpui::rgb(t.danger_text))
                .driver_id(|| "settings-needs-you-sound-problem".to_owned())
        });
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(px(10.0))
                    .child(dropdown)
                    .children(play),
            )
            .children(path)
            .children(problem)
    }

    /// The Finished sound's dropdown with Play beside it (not while the
    /// sound is off), then the custom file's path and what is wrong with it.
    fn finished_sound_rows(&self, focused: Option<SettingsControl>, cx: &mut Context<Self>) -> Div {
        let t = theme(cx);
        let size = crate::fonts::ui_text_size(cx);
        let shown = self.shown_local();
        let sound = &shown.notifications.finished_sound;
        let dropdown = self.sound_dropdown(NotifyKind::Finished, "Finished sound", focused, cx);
        let play = (*sound != ToastSound::Off).then(|| {
            settings_button(
                SettingsControl::FinishedSoundPlay,
                "Play",
                true,
                focused,
                cx,
            )
        });
        let path = match sound {
            ToastSound::Custom(path) => Some(
                hint(size, path.display().to_string(), &t)
                    .driver_id(|| "settings-finished-sound-path".to_owned()),
            ),
            ToastSound::Off | ToastSound::Default | ToastSound::Bundled(_) => None,
        };
        let problem = self
            .notifications
            .finished_sound_problem
            .clone()
            .map(|problem| {
                hint(size, problem, &t)
                    .text_color(gpui::rgb(t.danger_text))
                    .driver_id(|| "settings-finished-sound-problem".to_owned())
            });
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(px(10.0))
                    .child(dropdown)
                    .children(play),
            )
            .children(path)
            .children(problem)
    }

    /// `kind`'s closed sound field under `label`, with its list while open,
    /// over what is below it. A click on the field opens or closes the list,
    /// and a press outside both closes it.
    fn sound_dropdown(
        &self,
        kind: NotifyKind,
        label: &'static str,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Div {
        let t = theme(cx);
        let control = sound_field(kind);
        let selector = control.selector();
        let shown = SoundEntry::of(&self.sound_of(kind), kind);
        let chosen = labelled_text(format!("{selector}-text"), shown.text(kind))
            .flex_1()
            .min_w(px(0.0))
            .overflow_hidden();
        let chevron = svg()
            .path(SIDEBAR_CHEVRON_DOWN_ICON)
            .flex_none()
            .size(px(12.0))
            .text_color(gpui::rgb(t.text_2));
        let trigger = field_frame(focused == Some(control), Layer::Floating, &t)
            .id(selector)
            .driver_id(|| selector.to_owned())
            .w_full()
            .justify_between()
            .gap(px(8.0))
            .cursor_pointer()
            .children(search_hit_marker(selector, SEGMENT_RADIUS, cx))
            .child(chosen)
            .child(chevron)
            .on_click(press(control, cx));
        let list = self
            .notifications
            .sound_list
            .as_ref()
            .filter(|list| list.kind == kind);
        let dropdown = div()
            .relative()
            .w(px(SOUND_FIELD_WIDTH))
            .child(trigger)
            .when_some(list, |dropdown, list| {
                dropdown
                    .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.close_sound_list(cx);
                    }))
                    .child(sound_list_measurer(cx))
                    .child(sound_list_rows_panel(list, shown, cx))
            });
        field(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            label,
            &t,
        )
        .child(dropdown)
    }

    /// The shown host's Default shell state, while the window holds its
    /// connection.
    fn shown_shell(&self) -> Option<&HostShell> {
        self.mirrors_of(&self.settings_host)
            .map(|mirrors| &mirrors.shell)
    }

    /// `host`'s Default shell pick waiting for Apply, if one is held.
    fn pending_shell(&self, host: &HostKey) -> Option<&ShellSetting> {
        self.settings_pending
            .host(host)
            .iter()
            .find_map(|value| match value {
                HostPending::DefaultShell(setting) => Some(setting),
                _ => None,
            })
    }

    /// The default shell the shown host's row shows: its pending pick,
    /// else Custom picked with a blank command, else the one being
    /// applied, else the saved one.
    fn shown_shell_setting(&self) -> Option<ShellSetting> {
        let host = &self.settings_host;
        let pending = self.pending_shell(host);
        if pending.is_none() && self.settings_pending.shell_custom_picked.contains(host) {
            return Some(ShellSetting::new(shell_choice::CUSTOM, None));
        }
        self.shown_shell()?.shown(pending).cloned()
    }

    /// Whether `host`'s connection is open on a daemon that stores a
    /// default shell.
    fn shell_supported(&self, host: &HostKey) -> bool {
        self.conn_of(host)
            .is_some_and(|conn| conn.is_open() && conn.supports(FEATURE_DEFAULT_SHELL))
    }

    /// Whether the shown host's row takes input, and why not.
    fn shell_row_state(&self) -> RowState {
        let conn = self.conn_of(&self.settings_host);
        shell_choice::row_state(
            conn.is_some_and(crate::Connection::is_open),
            conn.is_some_and(|conn| conn.supports(FEATURE_DEFAULT_SHELL)),
            self.shown_shell()
                .is_some_and(|shell| shell.choices.is_some()),
        )
    }

    /// The shown host's row's chosen id, Automatic when nothing is saved.
    fn shown_shell_id(&self) -> Option<String> {
        self.shown_shell_setting().map(|setting| setting.id)
    }

    /// Whether the shown host's row shows Custom.
    fn shell_custom_shown(&self) -> bool {
        self.shown_shell_setting()
            .is_some_and(|setting| setting.id == shell_choice::CUSTOM)
    }

    /// The Custom command field, when it was built for the shown host.
    fn shell_command_field(&self) -> Option<&ShellCommandField> {
        self.settings_pending
            .shell_command
            .as_ref()
            .filter(|field| field.host == self.settings_host)
    }

    /// Whether the Custom command field is drawn: the row takes input and
    /// shows Custom.
    fn shell_command_drawn(&self) -> bool {
        self.shell_row_state() == RowState::Live
            && self.shell_custom_shown()
            && self.shell_command_field().is_some()
    }

    /// The closed dropdown's text on the shown host.
    fn shell_row_text(&self) -> String {
        match self.shell_row_state() {
            RowState::Live => shell_choice::chosen_label(
                self.shown_shell()
                    .and_then(|shell| shell.choices.as_deref()),
                self.shown_shell_id().as_deref(),
            ),
            RowState::Loading => shell_choice::LOADING.to_owned(),
            RowState::Unsupported => UNSUPPORTED_SHELL_TEXT.to_owned(),
        }
    }

    /// The open list's entries on the shown host.
    fn shell_entries(&self) -> Vec<ShellEntry> {
        let Some(choices) = self
            .shown_shell()
            .and_then(|shell| shell.choices.as_deref())
        else {
            return Vec::new();
        };
        let host = crate::sidebar::host_label(&self.settings_host);
        shell_choice::entries(choices, self.shown_shell_id().as_deref(), &host)
    }

    /// The note under the shown host's row.
    fn shell_note(&self) -> Option<String> {
        shell_choice::agents_note(
            self.shown_shell()
                .and_then(|shell| shell.choices.as_deref()),
            self.shown_shell_id().as_deref(),
        )
    }

    /// The Default shell dropdown's text while Settings is open.
    #[must_use]
    pub fn default_shell_text(&self) -> Option<String> {
        self.settings_open().then(|| self.shell_row_text())
    }

    /// Whether the Default shell dropdown opens.
    #[must_use]
    pub fn default_shell_live(&self) -> bool {
        self.settings_open() && self.shell_row_state() == RowState::Live
    }

    /// The Default shell list's entries on the shown host, in the daemon's
    /// order; empty before its choices arrive.
    #[must_use]
    pub fn default_shell_entries(&self) -> Vec<ShellEntry> {
        self.shell_entries()
    }

    /// The note under the Default shell row, while shown.
    #[must_use]
    pub fn default_shell_note(&self) -> Option<String> {
        self.settings_open().then(|| self.shell_note()).flatten()
    }

    /// Why the Default shell row is disabled, while Settings is open on an
    /// older daemon.
    #[must_use]
    pub fn default_shell_tip(&self) -> Option<&'static str> {
        (self.settings_open() && self.shell_row_state() == RowState::Unsupported)
            .then_some(shell_choice::UNSUPPORTED_TIP)
    }

    /// The Custom command field's text, while it is drawn.
    #[must_use]
    pub fn default_shell_command(&self) -> Option<String> {
        if !self.shell_command_drawn() {
            return None;
        }
        self.shell_command_field().map(|field| field.draft.clone())
    }

    /// The daemon's refusal of the shown host's last Default shell apply.
    #[must_use]
    pub fn default_shell_refusal(&self) -> Option<String> {
        self.settings_pending
            .shell_refusals
            .get(&self.settings_host)
            .cloned()
    }

    /// Whether the Default shell list is open.
    #[must_use]
    pub fn shell_list_open(&self) -> bool {
        self.settings_pending.shell_list.is_some()
    }

    /// The General tab shows the shown host's row: its choices are asked
    /// for, and Custom's command field is built when Custom shows.
    fn show_shell_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let host = self.settings_host.clone();
        self.ask_shell_choices(&host);
        if self.shell_custom_shown() {
            self.hold_shell_command_field(window, cx);
        }
    }

    /// Asks `host` for its shell choices while Settings shows its General
    /// tab and its daemon stores a default shell; nothing otherwise.
    fn ask_shell_choices(&self, host: &HostKey) {
        if self.shell_supported(host)
            && self.settings_open()
            && self.settings_tab == SettingsTab::General
            && *host == self.settings_host
        {
            tracing::info!(?host, "settings: asking for the host's shell choices");
            self.send_to(host, ClientMessage::ListShellChoices);
        }
    }

    /// The Custom command field's text for `host`: its field's draft, else
    /// the Custom command shown, else nothing.
    fn shell_command_draft(&self, host: &HostKey) -> String {
        if let Some(field) = self
            .settings_pending
            .shell_command
            .as_ref()
            .filter(|field| field.host == *host)
        {
            return field.draft.clone();
        }
        self.mirrors_of(host)
            .and_then(|mirrors| mirrors.shell.shown(self.pending_shell(host)))
            .filter(|setting| setting.id == shell_choice::CUSTOM)
            .and_then(|setting| setting.command.clone())
            .unwrap_or_default()
    }

    /// Builds the Custom command field for the shown host, unless it is
    /// built; Enter applies nothing and Esc is Cancel.
    fn hold_shell_command_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.shell_command_field().is_some() {
            return;
        }
        let host = self.settings_host.clone();
        let draft = self.shell_command_draft(&host);
        let input =
            cx.new(|cx| TextInput::new(draft.clone(), shell_choice::CUSTOM_PLACEHOLDER, cx));
        let focus = input.read(cx).focus_handle(cx);
        let keys = cx.subscribe_in(
            &input,
            window,
            |this, _, event: &TextInputEvent, window, cx| {
                if matches!(event, TextInputEvent::Cancel) {
                    this.close_appearance_editor(window, cx);
                }
            },
        );
        let edits = cx.subscribe_in(&input, window, |this, input, _: &TextChanged, _, cx| {
            let text = input.read(cx).text().to_owned();
            this.edit_shell_command(text, cx);
        });
        self.settings_pending.shell_command = Some(ShellCommandField {
            host,
            input,
            focus,
            draft,
            _subscriptions: vec![keys, edits],
        });
    }

    /// The Custom command field's text changed: Custom with it is the
    /// host's pending pick.
    fn edit_shell_command(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(field) = self.settings_pending.shell_command.as_mut() else {
            return;
        };
        // The field set to the saved command echoes nothing of the user's.
        if field.draft == text {
            return;
        }
        let pick = shell_choice::pick(shell_choice::CUSTOM, &text);
        field.draft = text;
        let host = field.host.clone();
        self.pend_default_shell(&host, pick);
        cx.notify();
    }

    /// Holds `pick` as `host`'s pending default shell as
    /// [`shell_choice::pending_value`] says, against what the row shows
    /// without it; Custom with a blank command holds nothing but stays
    /// shown. A refusal shown under the row goes.
    fn pend_default_shell(&mut self, host: &HostKey, pick: ShellSetting) {
        let held = self
            .mirrors_of(host)
            .and_then(|mirrors| shell_choice::pending_value(&mirrors.shell, &pick));
        let blank_custom = pick.normalized() == ShellSetting::new(shell_choice::CUSTOM, None);
        if let Some(value) = held {
            self.settings_pending
                .set_host(host, HostPending::DefaultShell(value), None);
        } else {
            let value = HostPending::DefaultShell(pick);
            self.settings_pending
                .set_host(host, value.clone(), Some(&value));
        }
        if blank_custom {
            self.settings_pending
                .shell_custom_picked
                .insert(host.clone());
        } else {
            self.settings_pending.shell_custom_picked.remove(host);
        }
        self.settings_pending.shell_refusals.remove(host);
    }

    /// `host`'s row follows its daemon after an echo or a refusal: with no
    /// pending edit there, Custom picked blank is forgotten and the Custom
    /// command field holds the saved command, empty when none.
    fn sync_shell_field(&mut self, host: &HostKey, cx: &mut Context<Self>) {
        if self.pending_shell(host).is_some() {
            return;
        }
        self.settings_pending.shell_custom_picked.remove(host);
        let text = self
            .mirrors_of(host)
            .map(|mirrors| mirrors.shell.saved_command())
            .unwrap_or_default();
        let Some(field) = self
            .settings_pending
            .shell_command
            .as_mut()
            .filter(|field| field.host == *host)
        else {
            return;
        };
        if field.draft == text {
            return;
        }
        field.draft.clone_from(&text);
        field.input.update(cx, |input, cx| input.set_text(text, cx));
    }

    /// Opens the Default shell list on its chosen entry, or closes it when
    /// open; it opens only once the choices arrived.
    fn toggle_shell_list(&mut self, cx: &mut Context<Self>) {
        if self.settings_pending.shell_list.take().is_none()
            && self.shell_row_state() == RowState::Live
        {
            let highlight = self
                .shell_entries()
                .iter()
                .position(|entry| entry.chosen)
                .unwrap_or(0);
            let scroll = ScrollHandle::new();
            scroll.scroll_to_item(highlight);
            self.settings_pending.shell_list = Some(ShellList {
                highlight,
                scroll,
                measured: None,
            });
        }
        cx.notify();
    }

    /// Closes the Default shell list, if it is open.
    fn close_shell_list(&mut self, cx: &mut Context<Self>) {
        if self.settings_pending.shell_list.take().is_some() {
            cx.notify();
        }
    }

    /// Keeps where the open shell list's field and the window were laid
    /// out, as [`Self::measure_sound_list`] does.
    fn measure_shell_list(
        &mut self,
        room: SoundListRoom,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(list) = &mut self.settings_pending.shell_list else {
            return;
        };
        if list.measured != Some(room) {
            list.measured = Some(room);
            list.scroll.scroll_to_item(list.highlight);
            cx.defer_in(window, |_, window, _| window.refresh());
        }
    }

    fn hover_shell_entry(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(list) = &mut self.settings_pending.shell_list
            && list.highlight != index
        {
            list.highlight = index;
            cx.notify();
        }
    }

    /// A key while the shell list is open; returns whether it was the
    /// list's: Up and Down move the highlight, Enter or Space picks it when
    /// it can be picked, Esc closes the list, and Tab closes it and walks
    /// on.
    fn on_shell_list_key(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.settings_tab != SettingsTab::General || self.settings_pending.shell_list.is_none() {
            return false;
        }
        let entries = self.shell_entries();
        let Some(list) = &mut self.settings_pending.shell_list else {
            return false;
        };
        match key {
            "up" | "down" => {
                list.highlight = if key == "down" {
                    (list.highlight + 1).min(entries.len().saturating_sub(1))
                } else {
                    list.highlight.saturating_sub(1)
                };
                list.scroll.scroll_to_item(list.highlight);
                cx.notify();
                true
            }
            "enter" | "space" => {
                if let Some(entry) = entries.get(list.highlight).filter(|entry| entry.pickable) {
                    self.choose_shell_entry(&entry.id, window, cx);
                }
                true
            }
            "escape" => {
                self.close_shell_list(cx);
                true
            }
            "tab" => {
                self.close_shell_list(cx);
                false
            }
            _ => false,
        }
    }

    /// A pick in the shell list: a choice the host has becomes the shown
    /// host's pending default shell, Custom with its command field's text.
    /// The keyboard stays on the dropdown.
    fn choose_shell_entry(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.close_shell_list(cx);
        self.move_settings_focus(SettingsControl::DefaultShell, window, cx);
        let host = self.settings_host.clone();
        let choices = self
            .mirrors_of(&host)
            .and_then(|mirrors| mirrors.shell.choices.as_deref());
        if !shell_choice::can_pick(choices, id) {
            return;
        }
        let custom = id == shell_choice::CUSTOM;
        let command = if custom {
            self.shell_command_draft(&host)
        } else {
            String::new()
        };
        tracing::info!(?host, id, "settings: picked a default shell");
        self.pend_default_shell(&host, shell_choice::pick(id, &command));
        if custom {
            self.hold_shell_command_field(window, cx);
        }
        cx.notify();
    }

    /// Apply reaching `host`'s pending default shell: sent, the row showing
    /// it until the daemon's echo or refusal. A host whose connection is
    /// closed, or whose daemon stores no default shell, is sent nothing and
    /// owes no echo: the pick is dropped.
    fn apply_default_shell(&mut self, host: &HostKey, setting: ShellSetting) {
        if !self.shell_supported(host) {
            tracing::info!(?host, id = %setting.id, "settings: dropped a default shell the host cannot take");
            return;
        }
        let request_id = crate::new_request_id();
        tracing::info!(?host, id = %setting.id, %request_id, "settings: applying the default shell");
        if let Some(mirrors) = self.mirrors_of_mut(host) {
            mirrors.shell.on_apply(request_id.clone(), setting.clone());
        }
        self.send_setting(
            host,
            ClientMessage::SetDefaultShell {
                id: setting.id,
                command: setting.command,
                request_id: Some(request_id),
            },
        );
    }

    /// Folds `host`'s daemon's `DefaultShell` or `ShellChoices`; any other
    /// message is left alone.
    pub(crate) fn on_shell_message(
        &mut self,
        host: &HostKey,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match msg {
            DaemonMessage::DefaultShell { id, command } => {
                let saved = ShellSetting::new(id, command.as_deref());
                self.on_default_shell(host, saved, window, cx);
            }
            DaemonMessage::ShellChoices { choices } => {
                self.on_shell_choices(host, choices.clone(), cx);
            }
            _ => {}
        }
    }

    /// `host`'s daemon's default shell, at connect or after any client's
    /// set: saved, and, while Settings shows `host`'s General tab, its
    /// choices are asked for again, since Custom's `hosts_agents` follows
    /// the saved command.
    pub(crate) fn on_default_shell(
        &mut self,
        host: &HostKey,
        saved: ShellSetting,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mirrors) = self.mirrors_of_mut(host) else {
            return;
        };
        tracing::info!(?host, id = %saved.id, "settings: the host's default shell");
        let refetch = mirrors.shell.on_saved(saved);
        self.settings_pending.shell_refusals.remove(host);
        self.sync_shell_field(host, cx);
        if refetch
            && self.settings_open()
            && self.settings_tab == SettingsTab::General
            && *host == self.settings_host
        {
            self.show_shell_row(window, cx);
        }
        cx.notify();
    }

    /// `host`'s daemon's shell choices, in its order.
    pub(crate) fn on_shell_choices(
        &mut self,
        host: &HostKey,
        choices: Vec<protocol::ShellChoice>,
        cx: &mut Context<Self>,
    ) {
        let Some(mirrors) = self.mirrors_of_mut(host) else {
            return;
        };
        tracing::info!(
            ?host,
            count = choices.len(),
            "settings: the host's shell choices"
        );
        mirrors.shell.on_choices(choices);
        cx.notify();
    }

    /// A refusal carrying `request_id`: when it answers `host`'s Default
    /// shell apply, it shows under the row. Returns whether it did.
    pub(crate) fn on_default_shell_refused(
        &mut self,
        host: &HostKey,
        request_id: &str,
        message: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(mirrors) = self.mirrors_of_mut(host) else {
            return false;
        };
        if !mirrors.shell.on_refused(request_id) {
            return false;
        }
        self.settings_pending
            .shell_refusals
            .insert(host.clone(), message.to_owned());
        self.sync_shell_field(host, cx);
        tracing::warn!(
            ?host,
            request_id,
            message,
            "settings: the daemon refused the default shell"
        );
        cx.notify();
        true
    }

    /// The Default shell row on the shown host: its dropdown (live, loading
    /// or unsupported), the Custom command field while Custom shows, the
    /// note when the chosen shell does not host agents, and a refusal.
    fn default_shell_row(&self, focused: Option<SettingsControl>, cx: &mut Context<Self>) -> Div {
        let t = theme(cx);
        let control = SettingsControl::DefaultShell;
        let selector = control.selector();
        let state = self.shell_row_state();
        let text = labelled_text(format!("{selector}-text"), self.shell_row_text())
            .flex_1()
            .min_w(px(0.0))
            .overflow_hidden();
        let chevron = svg()
            .path(SIDEBAR_CHEVRON_DOWN_ICON)
            .flex_none()
            .size(px(12.0))
            .text_color(gpui::rgb(t.text_2));
        let trigger = field_frame(focused == Some(control), Layer::Floating, &t)
            .id(selector)
            .driver_id(|| selector.to_owned())
            .w_full()
            .justify_between()
            .gap(px(8.0))
            .children(search_hit_marker(selector, SEGMENT_RADIUS, cx))
            .child(text)
            .child(chevron);
        let trigger = match state {
            RowState::Live => trigger.cursor_pointer().on_click(press(control, cx)),
            RowState::Loading => trigger
                .opacity(DISABLED_OPACITY)
                .text_color(gpui::rgb(t.subtle)),
            RowState::Unsupported => trigger
                .opacity(DISABLED_OPACITY)
                .text_color(gpui::rgb(t.subtle))
                .tooltip(tooltip(shell_choice::UNSUPPORTED_TIP)),
        };
        let entries = self.shell_entries();
        let list = self
            .settings_pending
            .shell_list
            .as_ref()
            .filter(|_| state == RowState::Live);
        let dropdown = div()
            .relative()
            .w(px(SOUND_FIELD_WIDTH))
            .child(trigger)
            .when_some(list, |dropdown, list| {
                dropdown
                    .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.close_shell_list(cx);
                    }))
                    .child(shell_list_measurer(cx))
                    .child(shell_list_rows_panel(list, &entries, cx))
            });
        let command = self
            .shell_command_field()
            .filter(|_| self.shell_command_drawn())
            .map(|field| {
                input_frame(
                    SettingsControl::DefaultShellCommand,
                    &field.input,
                    true,
                    focused,
                    cx,
                )
                .w(px(SOUND_FIELD_WIDTH))
            });
        let note = self.shell_note().map(|note| {
            hint(crate::fonts::ui_text_size(cx), note, &t)
                .driver_id(|| "settings-default-shell-note".to_owned())
        });
        let refusal = self.default_shell_refusal().map(|message| {
            hint(crate::fonts::ui_text_size(cx), message, &t)
                .text_color(gpui::rgb(t.danger_text))
                .driver_id(|| "settings-default-shell-refusal".to_owned())
        });
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                field(
                    crate::fonts::ui_text_size(cx),
                    crate::fonts::ui_weight_shift(cx),
                    "Default shell",
                    &t,
                )
                .child(dropdown),
            )
            .children(command)
            .children(note)
            .children(refusal)
    }

    fn general_tab(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let t = theme(cx);
        let shown = self.shown_local();
        let selected = self.keep_awake_mode();
        let modes = KEEP_AWAKE_CHOICES.map(|choice| {
            (
                SettingsControl::KeepAwake(choice),
                keep_awake_choice_text(choice),
                Some(choice) == selected,
            )
        });
        let status_selector = "settings-keep-awake-status";
        let status_text = self.keep_awake_status();
        let locked = if selected.is_none() {
            RowLock::Locked {
                note: status_text,
                selector: status_selector,
            }
        } else {
            RowLock::Live
        };
        let status = selected.map(|_| {
            hint(crate::fonts::ui_text_size(cx), status_text, &t)
                .driver_id(move || status_selector.to_owned())
        });
        let power = section(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            "Power",
        )
        .child(choice_row(
            self.keep_awake_label(),
            &modes,
            locked,
            focused,
            cx,
        ))
        .children(status);
        let sidebar = self.sidebar_section(focused, cx);
        let chat = self.chat_section(focused, cx);
        let copy = toggle(
            SettingsControl::CopyOnSelect,
            ToggleState {
                checked: shown.general.copy_on_select,
                enabled: true,
            },
            "Copy selection to clipboard automatically",
            focused,
            cx,
        );
        let terminal = section(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            "Terminal behavior",
        )
        .child(copy)
        .child(hint(
            crate::fonts::ui_text_size(cx),
            "Off: select to highlight, copy explicitly with Ctrl+C \
             (when there's a selection) or Ctrl+Shift+C.",
            &t,
        ))
        .child(self.default_shell_row(focused, cx));
        let mut rows = vec![
            power.into_any_element(),
            sidebar,
            terminal.into_any_element(),
            chat,
            self.updates_section(focused, cx).into_any_element(),
        ];
        if self.settings_host == HostKey::Local {
            rows.push(Self::diagnostics_section(&shown, focused, cx).into_any_element());
        }
        rows
    }

    /// The Diagnostics section: the Detailed logging switch, why it fills the
    /// logs faster, and the button that opens the logs folder. This PC only.
    fn diagnostics_section(
        shown: &ShownLocal,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Div {
        let t = theme(cx);
        let detailed = toggle(
            SettingsControl::DetailedLogging,
            ToggleState {
                checked: shown.general.detailed_logging,
                enabled: true,
            },
            "Detailed logging",
            focused,
            cx,
        );
        let open = settings_button(
            SettingsControl::OpenLogsFolder,
            "Open logs folder",
            true,
            focused,
            cx,
        );
        section(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            "Diagnostics",
        )
        .child(detailed)
        .child(
            hint(crate::fonts::ui_text_size(cx), DETAILED_LOGGING_HINT, &t)
                .driver_id(|| "settings-detailed-logging-hint".to_owned()),
        )
        .child(div().flex().gap(px(8.0)).child(open))
    }

    /// The Chat section: how chat panes draw tool calls, the widest their
    /// column draws, and why the width field's text is no width.
    fn chat_section(&self, focused: Option<SettingsControl>, cx: &mut Context<Self>) -> AnyElement {
        let t = theme(cx);
        let mode = self.shown_local().chat_mode;
        let modes = CHAT_MODE_CHOICES.map(|choice| {
            let text = match choice {
                ChatMode::Compact => "Compact",
                ChatMode::Comfortable => "Comfortable",
            };
            (SettingsControl::ChatMode(choice), text, choice == mode)
        });
        // The error line, while the text is no width, sits in the hint's
        // place.
        let note = match self.chat_width_error() {
            Some(error) => hint(crate::fonts::ui_text_size(cx), error, &t)
                .driver_id(|| "settings-chat-width-error".to_owned()),
            None => hint(crate::fonts::ui_text_size(cx), CHAT_WIDTH_HINT, &t),
        };
        section(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            "Chat",
        )
        .child(choice_row("Chat view", &modes, RowLock::Live, focused, cx))
        .children(self.chat_width_row(focused, cx))
        .child(note)
        .into_any_element()
    }

    /// The Maximum chat width field, once Settings built it.
    fn chat_width_row(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let chat_width = self.settings_pending.chat_width.as_ref()?;
        let control = SettingsControl::ChatWidth;
        let frame = field_frame(focused == Some(control), Layer::Floating, &theme(cx))
            .driver_id(|| control.selector().to_owned())
            .children(search_hit_marker(control.selector(), SEGMENT_RADIUS, cx))
            .w(px(PORT_FIELD_WIDTH))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.press_settings_control(control, window, cx);
                }),
            )
            .child(chat_width.input.clone());
        Some(
            field(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                "Maximum chat width",
                &theme(cx),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(frame)
                    .child("px"),
            ),
        )
    }

    /// The General tab's width field, built once per opening of Settings
    /// with the saved width: held, it keeps its draft until Settings
    /// closes.
    fn hold_chat_width_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_pending.chat_width.is_some() {
            return;
        }
        let draft = self.sidebar.chat_max_width().to_string();
        let input =
            cx.new(|cx| TextInput::new(draft.clone(), CHAT_WIDTH_PLACEHOLDER.to_owned(), cx));
        let focus = input.read(cx).focus_handle(cx);
        let keys = cx.subscribe_in(
            &input,
            window,
            // Enter applies nothing: the width waits for Apply or OK. Esc
            // is Cancel.
            |this, _, event: &TextInputEvent, window, cx| {
                if matches!(event, TextInputEvent::Cancel) {
                    this.close_appearance_editor(window, cx);
                }
            },
        );
        let edits = cx.subscribe_in(&input, window, |this, input, _: &TextChanged, _, cx| {
            let text = input.read(cx).text().to_owned();
            if let Some(field) = &mut this.settings_pending.chat_width {
                field.draft = text;
            }
            cx.notify();
        });
        self.settings_pending.chat_width = Some(ChatWidthField {
            input,
            focus,
            draft,
            _subscriptions: vec![keys, edits],
        });
    }

    /// Whether the width field is pending, and whether it holds a width: a
    /// draft other than the saved width; text that is no width is pending
    /// and fails.
    fn chat_width_pending(&self) -> Option<bool> {
        let draft = &self.settings_pending.chat_width.as_ref()?.draft;
        match parse_chat_width(draft) {
            Some(width) => (width != self.sidebar.chat_max_width()).then_some(true),
            None => Some(false),
        }
    }

    /// Apply reaching the width field: its width recorded and saved when it
    /// changed it.
    fn commit_chat_width(&mut self) {
        let Some(width) = self
            .settings_pending
            .chat_width
            .as_ref()
            .and_then(|field| parse_chat_width(&field.draft))
        else {
            return;
        };
        if self.sidebar.set_chat_max_width(width) {
            tracing::info!(width, "settings: applying the maximum chat width");
            self.save_ui();
        }
    }

    /// The Maximum chat width field's text, once Settings built it.
    #[must_use]
    pub fn chat_width_draft(&self) -> Option<String> {
        self.settings_pending
            .chat_width
            .as_ref()
            .map(|field| field.draft.clone())
    }

    /// Why the Maximum chat width field's text is no width.
    #[must_use]
    pub fn chat_width_error(&self) -> Option<String> {
        let draft = &self.settings_pending.chat_width.as_ref()?.draft;
        parse_chat_width(draft)
            .is_none()
            .then(|| CHAT_WIDTH_ERROR.to_owned())
    }

    /// The Updates section: the automatic-check toggle, the client's version
    /// and the last check's result, Check now, and Restart to update once an
    /// update is ready.
    fn updates_section(&self, focused: Option<SettingsControl>, cx: &mut Context<Self>) -> Div {
        let t = theme(cx);
        let check = toggle(
            SettingsControl::CheckUpdates,
            ToggleState {
                checked: self.shown_local().general.check_updates,
                enabled: true,
            },
            "Check for updates",
            focused,
            cx,
        );
        let [version, last] = self.update_settings_lines();
        let check_now = settings_button(
            SettingsControl::CheckNow,
            "Check now",
            !self.update_check_running(),
            focused,
            cx,
        );
        section(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            "Updates",
        )
        .child(check)
        .child(
            hint(crate::fonts::ui_text_size(cx), version, &t)
                .driver_id(|| "settings-update-version".to_owned()),
        )
        .child(
            hint(crate::fonts::ui_text_size(cx), last, &t)
                .driver_id(|| "settings-update-last-check".to_owned()),
        )
        .child(
            div()
                .flex()
                .gap(px(8.0))
                .child(check_now)
                .children(self.settings_restart_button(cx))
                .children(self.settings_whats_new_button(cx)),
        )
    }

    /// The Sidebar section: the default view, how much room a leaf takes, how
    /// many of a repo's or workspace's sessions show under its row and how
    /// the repo and workspace rows are ordered.
    fn sidebar_section(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view = self.shown_local().sidebar_view;
        let choices = SIDEBAR_VIEW_CHOICES.map(|choice| {
            let text = match choice {
                SidebarView::Repos => "Repos",
                SidebarView::Tabs => "Tabs",
            };
            (SettingsControl::SidebarView(choice), text, choice == view)
        });
        let density = self.shown_local().general.leaf_density;
        let densities = LEAF_DENSITY_CHOICES.map(|choice| {
            let text = match choice {
                LeafDensity::Comfortable => "Comfortable",
                LeafDensity::Compact => "Compact",
            };
            (
                SettingsControl::LeafDensity(choice),
                text,
                choice == density,
            )
        });
        let general = self.shown_local().general;
        let under = sessions_under_repos_choices(general.sessions_under_repos);
        let sorts = repo_sort_choices(general.repo_sort);
        section(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            "Sidebar",
        )
        .child(choice_row(
            "Default view",
            &choices,
            RowLock::Live,
            focused,
            cx,
        ))
        .child(choice_row(
            "Leaf density",
            &densities,
            RowLock::Live,
            focused,
            cx,
        ))
        .child(choice_row(
            "Sessions under repos",
            &under,
            RowLock::Live,
            focused,
            cx,
        ))
        .child(choice_row("Sort repos", &sorts, RowLock::Live, focused, cx))
        .into_any_element()
    }

    fn app_title_tab(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let t = theme(cx);
        let shown = self.shown_local();
        let title = &shown.title;
        let modes = count_mode_choices(title.count);
        let count = toggle(
            SettingsControl::TitleCount,
            ToggleState {
                checked: title.show_count,
                enabled: true,
            },
            "Show the count on the taskbar button",
            focused,
            cx,
        );
        let suffix = toggle(
            SettingsControl::TitleSuffix,
            ToggleState {
                checked: title.suffix,
                enabled: true,
            },
            "Append “ — rustling-tulip” suffix",
            focused,
            cx,
        );
        let preview = div().flex().gap(px(10.0)).child("Preview").child(
            div()
                .driver_id(|| "settings-title-preview".to_owned())
                .px(px(8.0))
                .py(px(2.0))
                .rounded(px(CHIP_RADIUS))
                .bg(gpui::rgb(t.chip))
                .child(self.title_preview()),
        );
        let section = section(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            "App title",
        )
        .child(hint(
            crate::fonts::ui_text_size(cx),
            "Controls what the main window's title bar and taskbar button show.",
            &t,
        ))
        .child(choice_row(
            "Tab and badge count",
            &modes,
            RowLock::Live,
            focused,
            cx,
        ))
        .child(hint(
            crate::fonts::ui_text_size(cx),
            "What the tab pills and the taskbar badge count. Needs you counts the \
                 terminals waiting on you and shows nothing at 0; Busy / total and Idle / \
                 total count the busy or idle terminals out of all of them.",
            &t,
        ))
        .child(count)
        .child(hint(
            crate::fonts::ui_text_size(cx),
            "Draws the count over every tab as a badge on the main window's taskbar \
                 button, 9+ above 9 (Windows only). Hidden when there is nothing to count.",
            &t,
        ))
        .child(suffix)
        .child(hint(
            crate::fonts::ui_text_size(cx),
            "Keeps OS taskbars grouping rustling-tulip windows together. Turn off \
                 for a tighter title.",
            &t,
        ))
        .child(preview);
        vec![section.into_any_element()]
    }
}

const SUMMARIES_INTRO: &str = "Short summaries of what each session is doing, for the Dashboard and its notifications. \
     Off until you turn them on: each summary uses the chosen provider's account.";
const VARIABLE_HINT: &str = "The daemon reads this variable from its own environment.";
const CAP_HINT: &str = "Summaries stop for the day once today's spend reaches the cap.";
const TEST_HINT: &str = "Summarizes a fixed sample with the settings above.";
/// The Test button's tooltip while the Dashboard page holds a change.
const TEST_PENDING_TIP: &str = "Apply your changes first to test them.";
const NEWER_DAEMON: &str =
    "This host's daemon is newer than this app; update the app to change these settings.";
/// The widths of the Dashboard tab's key and variable fields: input widths.
const KEY_FIELD_WIDTH: f32 = 280.0;
const VARIABLE_FIELD_WIDTH: f32 = 220.0;

/// The Dashboard tab: the summarizer's settings on the host Settings shows.
impl RootView {
    /// Folds what `msg` from `host` says about its summarizer; the shown
    /// Dashboard tab's cap and variable fields follow a new status while
    /// they still read as the old one.
    pub(crate) fn on_summarizer_message(
        &mut self,
        host: &HostKey,
        msg: &DaemonMessage,
        cx: &mut Context<Self>,
    ) {
        let old = self
            .summarizer_settings
            .status(host)
            .map(|s| s.settings.clone());
        if !self.summarizer_settings.fold(host, msg) {
            return;
        }
        let new = self
            .summarizer_settings
            .status(host)
            .map(|s| s.settings.clone());
        if let Some(tab) = self.dashboard_settings_tabs.get_mut(host) {
            tab.follow(old.as_ref(), new.as_ref(), cx);
        }
        cx.notify();
    }

    /// A daemon refusal from `host` carrying `request_id`: when it answers
    /// the Dashboard tab's Test or cap change, the tab shows it. Returns
    /// whether it did.
    pub(crate) fn on_summarizer_refused(
        &mut self,
        host: &HostKey,
        request_id: &str,
        message: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let answered = self.summarizer_settings.refused(host, request_id, message);
        if answered {
            tracing::info!(
                ?host,
                "settings: the summarizer refused a Dashboard request"
            );
            cx.notify();
        }
        answered
    }

    fn shown_summarizer(&self) -> Option<&SummarizerStatus> {
        self.summarizer_settings.status(&self.settings_host)
    }

    /// Whether the shown host's daemon reads its key from a source this
    /// app cannot name: its settings cannot be sent back whole, so the tab
    /// changes none of them.
    fn summarizer_locked(&self) -> bool {
        self.shown_summarizer()
            .is_some_and(|status| status.settings.key_source == SummarizerKeySource::Unknown)
    }

    fn shown_dashboard_tab(&self) -> Option<&DashboardSettingsTab> {
        self.dashboard_settings_tabs.get(&self.settings_host)
    }

    /// The key source the tab shows: the one pending, else the daemon's.
    fn shown_key_source(&self) -> KeySourceChoice {
        self.pending_key_source(&self.settings_host)
    }

    /// Whether the provider shown as chosen takes a key.
    fn summarizer_needs_key(&self) -> bool {
        self.shown_summarizer().is_some_and(|status| {
            let id = self.shown_provider_id(status);
            status
                .providers
                .iter()
                .any(|provider| provider.id == id && provider.needs_key)
        })
    }

    /// Clear deletes a stored key: one the daemon holds, rejected or not.
    fn key_clear_enabled(&self) -> bool {
        !self.summarizer_locked()
            && matches!(
                self.shown_summarizer()
                    .and_then(SummarizerStatus::key_state),
                Some(KeyState::Stored | KeyState::Rejected)
            )
    }

    /// Whether the Key row's button takes a press: Clear while the daemon
    /// holds a key to delete, Undo while a deletion is held.
    fn key_delete_enabled(&self) -> bool {
        self.held_key_delete().is_some() || self.key_clear_enabled()
    }

    /// The words the Key row's button shows.
    #[must_use]
    pub fn dashboard_key_action_label(&self) -> &'static str {
        if self.held_key_delete().is_some() {
            "Undo delete"
        } else {
            "Delete stored key"
        }
    }

    /// The shown key's state: no key while its deletion is held, else the
    /// daemon's.
    fn shown_key_state(&self) -> Option<KeyState> {
        if self.held_key_delete().is_some() {
            return Some(KeyState::Missing);
        }
        self.shown_summarizer()?.key_state()
    }

    /// Whether the shown host's Dashboard page holds a change Apply would
    /// send, so Test waits for Apply.
    fn dashboard_test_locked(&self) -> bool {
        !self.dashboard_pending_rows(&self.settings_host).is_empty()
    }

    /// The Test button's tooltip: why it waits while the Dashboard page
    /// holds a change.
    #[must_use]
    pub fn dashboard_test_tip(&self) -> Option<&'static str> {
        self.dashboard_test_locked().then_some(TEST_PENDING_TIP)
    }

    fn provider_controls(&self) -> Vec<SettingsControl> {
        let Some(status) = self.shown_summarizer() else {
            return Vec::new();
        };
        status
            .providers
            .iter()
            .enumerate()
            .map(|(index, provider)| SettingsControl::DashboardProvider {
                index,
                selector: intern(format!("settings-dashboard-provider-{}", provider.id)),
            })
            .collect()
    }

    /// The Dashboard tab's stops in drawing order, once the daemon reported
    /// its summarizer; with `every`, each stop whether drawn now or not.
    fn dashboard_controls(&self, every: bool) -> Vec<SettingsControl> {
        if self.shown_summarizer().is_none() {
            return Vec::new();
        }
        if self.summarizer_locked() && !every {
            return vec![SettingsControl::DashboardTest];
        }
        let mut controls = vec![SettingsControl::DashboardEnabled];
        controls.extend(self.provider_controls());
        if every || self.summarizer_needs_key() {
            controls.extend(KEY_SOURCE_CHOICES.map(SettingsControl::DashboardKeySource));
            controls.extend(self.key_controls(every));
        }
        controls.extend([
            SettingsControl::DashboardCap,
            SettingsControl::DashboardTest,
        ]);
        controls
    }

    /// The Dashboard tab's stops [`Self::dashboard_tab_content`] draws now,
    /// a locked or disabled one included: none before the daemon reports
    /// its summarizer, the key rows only for a provider that takes a key,
    /// and Key and its delete button or Variable by the key source.
    fn dashboard_drawn(&self) -> Vec<SettingsControl> {
        if self.shown_summarizer().is_none() {
            return Vec::new();
        }
        let mut controls = vec![SettingsControl::DashboardEnabled];
        controls.extend(self.provider_controls());
        if self.summarizer_needs_key() {
            controls.extend(KEY_SOURCE_CHOICES.map(SettingsControl::DashboardKeySource));
            match self.shown_key_source() {
                KeySourceChoice::Stored => controls.extend([
                    SettingsControl::DashboardKey,
                    SettingsControl::DashboardKeyClear,
                ]),
                KeySourceChoice::Env => controls.push(SettingsControl::DashboardEnvVariable),
            }
        }
        controls.extend([
            SettingsControl::DashboardCap,
            SettingsControl::DashboardTest,
        ]);
        controls
    }

    fn key_controls(&self, every: bool) -> Vec<SettingsControl> {
        if every {
            return vec![
                SettingsControl::DashboardKey,
                SettingsControl::DashboardKeyClear,
                SettingsControl::DashboardEnvVariable,
            ];
        }
        if self.shown_key_source() == KeySourceChoice::Env {
            return vec![SettingsControl::DashboardEnvVariable];
        }
        let mut controls = vec![SettingsControl::DashboardKey];
        if self.key_delete_enabled() {
            controls.push(SettingsControl::DashboardKeyClear);
        }
        controls
    }

    /// The shown host's Dashboard tab, built afresh unless held: held, it
    /// keeps its drafts until Settings closes.
    fn hold_dashboard_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let host = self.settings_host.clone();
        if !self.dashboard_settings_tabs.contains_key(&host) {
            let tab = self.new_dashboard_tab(&host, window, cx);
            self.dashboard_settings_tabs.insert(host, tab);
        }
    }

    /// `host`'s tab afresh: an empty masked key field, and the variable and
    /// cap fields holding the daemon's values.
    fn new_dashboard_tab(
        &self,
        host: &HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> DashboardSettingsTab {
        let settings = self
            .summarizer_settings
            .status(host)
            .map(|status| status.settings.clone());
        let cap_draft = settings
            .as_ref()
            .map_or_else(String::new, |settings| usd(settings.daily_cap_usd));
        let variable_draft = env_variable(settings.as_ref()).unwrap_or_default();
        let input = |field, text, window: &mut Window, cx: &mut Context<Self>| {
            Self::dashboard_input(host.clone(), field, text, window, cx)
        };
        let (key, key_subs) = input(DashboardField::Key, String::new(), window, cx);
        key.update(cx, |input, cx| input.set_masked(true, cx));
        let (variable, variable_subs) =
            input(DashboardField::Variable, variable_draft.clone(), window, cx);
        let (cap, cap_subs) = input(DashboardField::Cap, cap_draft.clone(), window, cx);
        DashboardSettingsTab {
            key_focus: key.read(cx).focus_handle(cx),
            variable_focus: variable.read(cx).focus_handle(cx),
            cap_focus: cap.read(cx).focus_handle(cx),
            key,
            variable,
            cap,
            key_draft: String::new(),
            variable_draft,
            cap_draft,
            _subscriptions: [key_subs, variable_subs, cap_subs]
                .into_iter()
                .flatten()
                .collect(),
        }
    }

    /// A field of `host`'s Dashboard tab, its input holding `text`: an edit
    /// keeps its draft, which waits for Apply or OK; Escape is Cancel.
    fn dashboard_input(
        host: HostKey,
        field: DashboardField,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<TextInput>, [Subscription; 2]) {
        let placeholder = match field {
            DashboardField::Key => "Paste the key",
            DashboardField::Variable => "Variable name",
            DashboardField::Cap => "1.00",
        };
        let input = cx.new(|cx| TextInput::new(text, placeholder.to_owned(), cx));
        let keys = cx.subscribe_in(
            &input,
            window,
            // Enter applies nothing: the field waits for Apply or OK. Esc
            // is Cancel.
            move |this, _, event: &TextInputEvent, window, cx| {
                if matches!(event, TextInputEvent::Cancel) {
                    this.close_appearance_editor(window, cx);
                }
            },
        );
        let edits = cx.subscribe_in(
            &input,
            window,
            move |this, input, _: &TextChanged, _, cx| {
                let text = input.read(cx).text().to_owned();
                if let Some(tab) = this.dashboard_settings_tabs.get_mut(&host) {
                    tab.draft_mut(field).clone_from(&text);
                }
                // A typed key replaces a held deletion of the key it would
                // overwrite; a deletion held for another provider stays.
                if field == DashboardField::Key
                    && !text.trim().is_empty()
                    && let Some(provider) = this.shown_key_provider()
                {
                    this.settings_pending
                        .drop_key_delete(&this.settings_host, &provider);
                }
                cx.notify();
            },
        );
        (input, [keys, edits])
    }

    /// A Dashboard toggle or choice on the shown host: `value`, pending
    /// until Apply unless it is `saved`. Nothing is chosen while the tab is
    /// locked.
    fn pend_dashboard(&mut self, value: HostPending, saved: &HostPending) {
        if self.summarizer_locked() || self.shown_summarizer().is_none() {
            return;
        }
        let host = self.settings_host.clone();
        self.settings_pending.set_host(&host, value, Some(saved));
    }

    /// The shown host's summarizer pending `pick` reads, if any.
    fn dashboard_pending<R>(&self, pick: impl Fn(&HostPending) -> Option<R>) -> Option<R> {
        self.settings_pending
            .host(&self.settings_host)
            .iter()
            .find_map(pick)
    }

    /// The provider whose stored key a held deletion deletes on the shown
    /// host, if one is held.
    fn held_key_delete(&self) -> Option<String> {
        self.dashboard_pending(|value| match value {
            HostPending::SummaryKeyDelete { provider } => Some(provider.clone()),
            _ => None,
        })
    }

    /// Whether `host` holds a deletion of its stored summarizer key.
    fn holds_key_delete(&self, host: &HostKey) -> bool {
        self.settings_pending
            .host(host)
            .iter()
            .any(|value| matches!(value, HostPending::SummaryKeyDelete { .. }))
    }

    /// Whether summaries show as on: pending, else the daemon's.
    fn shown_summaries_on(&self, status: &SummarizerStatus) -> bool {
        self.dashboard_pending(|value| match value {
            HostPending::Summaries(on) => Some(*on),
            _ => None,
        })
        .unwrap_or(status.settings.enabled)
    }

    /// The provider shown as chosen: pending, else the daemon's.
    fn shown_provider_id(&self, status: &SummarizerStatus) -> String {
        self.dashboard_pending(|value| match value {
            HostPending::SummaryProvider(id) => Some(id.clone()),
            _ => None,
        })
        .unwrap_or_else(|| status.settings.provider.clone())
    }

    /// The provider the shown host's typed key would be saved under.
    fn shown_key_provider(&self) -> Option<String> {
        let status = self.shown_summarizer()?;
        Some(self.shown_provider_id(status))
    }

    fn toggle_summaries(&mut self) {
        let Some(status) = self.shown_summarizer() else {
            return;
        };
        let (on, saved) = (self.shown_summaries_on(status), status.settings.enabled);
        self.pend_dashboard(HostPending::Summaries(!on), &HostPending::Summaries(saved));
    }

    fn choose_summarizer_provider(&mut self, index: usize) {
        let Some(status) = self.shown_summarizer() else {
            return;
        };
        let Some(provider) = status.providers.get(index).map(|p| p.id.clone()) else {
            return;
        };
        let saved = HostPending::SummaryProvider(status.settings.provider.clone());
        self.pend_dashboard(HostPending::SummaryProvider(provider), &saved);
    }

    /// The key source, pending until Apply; Environment variable sends
    /// once a variable is named, and until then shows the variable field.
    fn choose_key_source(&mut self, choice: KeySourceChoice) {
        let saved = self.saved_key_source(&self.settings_host);
        self.pend_dashboard(
            HostPending::SummaryKeySource(choice),
            &HostPending::SummaryKeySource(saved),
        );
    }

    /// The key source `host`'s daemon reads.
    fn saved_key_source(&self, host: &HostKey) -> KeySourceChoice {
        let env = self
            .summarizer_settings
            .status(host)
            .is_some_and(|s| matches!(s.settings.key_source, SummarizerKeySource::Env { .. }));
        if env {
            KeySourceChoice::Env
        } else {
            KeySourceChoice::Stored
        }
    }

    /// `host`'s pending Dashboard rows: its toggles and choices, and, while
    /// the Dashboard tab is `host`'s, a named variable, a cap and a key
    /// that differ from the daemon's. A cap that is no number of dollars,
    /// and Environment variable with no variable named, fail validation.
    fn dashboard_pending_rows(&self, host: &HostKey) -> Vec<PendingEdit> {
        let Some(status) = self.summarizer_settings.status(host) else {
            return Vec::new();
        };
        if status.settings.key_source == SummarizerKeySource::Unknown {
            return Vec::new();
        }
        let tab = self.dashboard_settings_tabs.get(host);
        let variable = tab.map_or("", |tab| tab.variable_draft.trim());
        let mut rows = self.dashboard_toggle_rows(host, variable);
        if self.holds_key_delete(host) {
            rows.push(PendingEdit {
                row: PendingRow::Toggle,
                valid: true,
            });
        }
        if let Some(tab) = tab {
            let source = self.pending_key_source(host);
            rows.extend(dashboard_draft_rows(&status.settings, source, tab));
        }
        rows
    }

    /// `host`'s pending Dashboard toggles and choices, as rows: Environment
    /// variable fails validation while `variable` names none.
    fn dashboard_toggle_rows(&self, host: &HostKey, variable: &str) -> Vec<PendingEdit> {
        self.settings_pending
            .host(host)
            .iter()
            .filter_map(|value| {
                let valid = match value {
                    HostPending::SummaryKeySource(KeySourceChoice::Env) => !variable.is_empty(),
                    HostPending::Summaries(_)
                    | HostPending::SummaryProvider(_)
                    | HostPending::SummaryKeySource(KeySourceChoice::Stored) => true,
                    _ => return None,
                };
                Some(PendingEdit {
                    row: PendingRow::Toggle,
                    valid,
                })
            })
            .collect()
    }

    /// The key source `host`'s Dashboard shows: the one pending, else the
    /// daemon's.
    fn pending_key_source(&self, host: &HostKey) -> KeySourceChoice {
        self.settings_pending
            .host(host)
            .iter()
            .find_map(|value| match value {
                HostPending::SummaryKeySource(choice) => Some(*choice),
                _ => None,
            })
            .unwrap_or_else(|| self.saved_key_source(host))
    }

    /// Apply reaching `host`'s Dashboard: its pending toggles, choices,
    /// variable and cap as one settings message, then a typed key, sent as
    /// the key Save sent before and the field emptied.
    fn apply_dashboard(
        &mut self,
        host: &HostKey,
        held: Vec<HostPending>,
        rows: &[PendingEdit],
        cx: &mut Context<Self>,
    ) {
        let Some(mut settings) = self.summarizer_settings.next_settings(host).cloned() else {
            return;
        };
        let has = |row: PendingRow| rows.iter().any(|edit| edit.row == row);
        let mut changed = false;
        let mut delete = None;
        for value in held {
            match value {
                HostPending::SummaryKeyDelete { provider } => delete = Some(provider),
                other => {
                    take_dashboard_value(&mut settings, other);
                    changed = true;
                }
            }
        }
        let tab = self.dashboard_settings_tabs.get(host);
        let variable = tab
            .map(|tab| tab.variable_draft.trim().to_owned())
            .filter(|_| has(PendingRow::DashboardVariable));
        let cap = tab
            .and_then(|tab| parse_cap(&tab.cap_draft))
            .filter(|_| has(PendingRow::DashboardCap));
        if let Some(variable) = variable {
            settings.key_source = SummarizerKeySource::Env { variable };
            changed = true;
        }
        if let Some(cap) = cap {
            settings.daily_cap_usd = cap;
            changed = true;
        }
        let provider = settings.provider.clone();
        if changed {
            self.send_summarizer_settings(host, settings, cap.is_some());
        }
        if let Some(deleted) = delete {
            self.send_summarizer_delete(host, deleted);
        }
        if has(PendingRow::DashboardKey) {
            self.send_summarizer_key(host, provider, cx);
        }
    }

    /// `settings` to `host` as Apply's summarizer send, kept for the next
    /// change to build on; `cap` when it carries a changed cap, whose
    /// refusal the cap field shows.
    fn send_summarizer_settings(
        &mut self,
        host: &HostKey,
        settings: SummarizerSettings,
        cap: bool,
    ) {
        let request_id = crate::new_request_id();
        tracing::info!(?host, "settings: applying the summarizer's settings");
        self.send_setting(
            host,
            ClientMessage::SetSummarizerSettings {
                settings: settings.clone(),
                request_id: Some(request_id.clone()),
            },
        );
        self.summarizer_settings.sent_settings(host, settings);
        if cap {
            self.summarizer_settings.sent_cap(host, request_id);
        }
    }

    /// `host`'s typed key to `host` for `provider`, the key field emptied.
    fn send_summarizer_key(&mut self, host: &HostKey, provider: String, cx: &mut Context<Self>) {
        let Some(tab) = self.dashboard_settings_tabs.get_mut(host) else {
            return;
        };
        let key = tab.key_draft.trim().to_owned();
        tab.key_draft.clear();
        tab.key.update(cx, |input, cx| input.set_text("", cx));
        tracing::info!(%provider, "settings: saving the summarizer key");
        self.send_summarizer_key_value(host, provider, Some(Secret(key)));
    }

    /// `provider`'s stored key deleted on `host`.
    fn send_summarizer_delete(&mut self, host: &HostKey, provider: String) {
        tracing::info!(%provider, "settings: deleting the summarizer key");
        self.send_summarizer_key_value(host, provider, None);
    }

    /// `key` to `host` for `provider` as Apply's key send, whose echo OK
    /// waits for; `None` deletes the stored one.
    fn send_summarizer_key_value(&mut self, host: &HostKey, provider: String, key: Option<Secret>) {
        self.send_setting(
            host,
            ClientMessage::SetSummarizerKey {
                provider,
                key,
                request_id: Some(crate::new_request_id()),
            },
        );
    }

    /// The Key row's button on the shown host: holds a deletion of the
    /// stored key until Apply, or drops the held one.
    fn clear_summarizer_key(&mut self, cx: &mut Context<Self>) {
        if let Some(provider) = self.held_key_delete() {
            self.settings_pending
                .drop_key_delete(&self.settings_host, &provider);
            return;
        }
        let Some(provider) = self
            .shown_summarizer()
            .map(|status| status.settings.provider.clone())
        else {
            return;
        };
        if self.summarizer_locked() || !self.key_clear_enabled() {
            return;
        }
        self.clear_key_draft(cx);
        tracing::info!(%provider, "settings: holding the summarizer key's deletion");
        let host = self.settings_host.clone();
        self.settings_pending
            .set_host(&host, HostPending::SummaryKeyDelete { provider }, None);
    }

    /// The Dashboard tab's key field emptied: a held deletion deletes the
    /// stored key, not a typed one.
    fn clear_key_draft(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.dashboard_settings_tabs.get_mut(&self.settings_host) else {
            return;
        };
        tab.key_draft.clear();
        tab.key.update(cx, |input, cx| input.set_text("", cx));
    }

    fn run_summary_test(&mut self) {
        if self.shown_summarizer().is_none() || self.dashboard_test_locked() {
            return;
        }
        let request_id = crate::new_request_id();
        tracing::info!("settings: testing the summarizer");
        self.send_to(
            &self.settings_host,
            ClientMessage::TestSummary {
                request_id: Some(request_id.clone()),
            },
        );
        self.summarizer_settings
            .start_test(&self.settings_host, request_id);
    }

    /// The chosen provider's privacy line on the shown host.
    #[must_use]
    pub fn dashboard_privacy(&self) -> Option<String> {
        self.shown_summarizer()?
            .provider()
            .map(|provider| provider.privacy.clone())
    }

    /// The words the Dashboard tab shows for the chosen provider's key.
    #[must_use]
    pub fn dashboard_key_state(&self) -> Option<&'static str> {
        self.shown_key_state().map(key_state_words)
    }

    /// The key field's text, once the tab was shown.
    #[must_use]
    pub fn dashboard_key_draft(&self, cx: &App) -> Option<String> {
        self.shown_dashboard_tab()
            .map(|tab| tab.key.read(cx).text().to_owned())
    }

    /// The key source choice the Dashboard tab shows selected.
    #[must_use]
    pub fn dashboard_key_source(&self) -> &'static str {
        match self.shown_key_source() {
            KeySourceChoice::Stored => "Stored key",
            KeySourceChoice::Env => "Environment variable",
        }
    }

    /// The cap field's text, once the tab was shown.
    #[must_use]
    pub fn dashboard_cap_draft(&self, cx: &App) -> Option<String> {
        self.shown_dashboard_tab()
            .map(|tab| tab.cap.read(cx).text().to_owned())
    }

    /// Whether the key field draws dots for its text.
    #[must_use]
    pub fn dashboard_key_masked(&self, cx: &App) -> Option<bool> {
        self.shown_dashboard_tab()
            .map(|tab| tab.key.read(cx).is_masked())
    }

    /// The line under the cap: today's spend on the shown host.
    #[must_use]
    pub fn dashboard_spend_line(&self) -> Option<String> {
        self.shown_summarizer().map(spend_line)
    }

    /// The Test result slot's text on the shown host.
    #[must_use]
    pub fn dashboard_test_result(&self) -> Option<String> {
        self.summarizer_settings
            .test(&self.settings_host)
            .map(test_text)
    }

    /// Why the cap was not applied: the field's text is no cap, or the
    /// daemon refused it.
    #[must_use]
    pub fn dashboard_cap_error(&self) -> Option<String> {
        self.shown_dashboard_tab()
            .filter(|tab| !tab.cap_draft.trim().is_empty() && parse_cap(&tab.cap_draft).is_none())
            .map(|_| CAP_PROBLEM.to_owned())
            .or_else(|| {
                self.summarizer_settings
                    .cap_error(&self.settings_host)
                    .map(str::to_owned)
            })
    }

    /// The Dashboard tab: summaries on or off, the provider and its privacy
    /// line, its key, the daily cap with today's spend, and Test.
    fn dashboard_tab_content(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let t = theme(cx);
        let size = crate::fonts::ui_text_size(cx);
        let shift = crate::fonts::ui_weight_shift(cx);
        let intro =
            section(size, shift, "Dashboard summaries").child(hint(size, SUMMARIES_INTRO, &t));
        let (Some(status), Some(tab)) = (self.shown_summarizer(), self.shown_dashboard_tab())
        else {
            let line = div().text_color(gpui::rgb(t.text_2)).child(NOT_CONNECTED);
            return vec![intro.child(line).into_any_element()];
        };
        let locked = self.summarizer_locked();
        let newer_daemon = locked.then(|| {
            hint(size, NEWER_DAEMON, &t).driver_id(|| "settings-dashboard-newer-daemon".to_owned())
        });
        let enabled = toggle(
            SettingsControl::DashboardEnabled,
            ToggleState {
                checked: self.shown_summaries_on(status),
                enabled: !locked,
            },
            "Summarize sessions",
            focused,
            cx,
        );
        let privacy = self
            .dashboard_privacy()
            .filter(|line| !line.is_empty())
            .map(|line| hint(size, line, &t).driver_id(|| "settings-dashboard-privacy".to_owned()));
        let summaries = intro
            .children(newer_daemon)
            .child(enabled)
            .child(self.provider_row(status, focused, cx))
            .children(privacy)
            .children(self.key_rows(tab, focused, cx));
        let cap_error = self.dashboard_cap_error().map(|error| {
            hint(size, error, &t).driver_id(|| "settings-dashboard-cap-error".to_owned())
        });
        let spend = div()
            .driver_id(|| "settings-dashboard-spend".to_owned())
            .text_color(gpui::rgb(t.text_2))
            .child(spend_line(status));
        let cap_field = self.dashboard_field(
            SettingsControl::DashboardCap,
            &tab.cap,
            PORT_FIELD_WIDTH,
            focused,
            cx,
        );
        let cap = section(size, shift, "Daily cap")
            .child(labelled_row("Cap (USD)", cap_field))
            .children(cap_error)
            .child(spend)
            .child(hint(size, CAP_HINT, &t));
        let result = self.dashboard_test_result().map(|text| {
            div()
                .driver_id(|| "settings-dashboard-test-result".to_owned())
                .text_color(gpui::rgb(t.text_2))
                .children(text.lines().map(|line| div().child(line.to_owned())))
        });
        let test_tip = self.dashboard_test_tip();
        let test_button = settings_button(
            SettingsControl::DashboardTest,
            "Test",
            test_tip.is_none(),
            focused,
            cx,
        );
        let test_button = match test_tip {
            Some(tip) => test_button.tooltip(tooltip(tip)),
            None => test_button,
        };
        let test = section(size, shift, "Test")
            .child(hint(size, TEST_HINT, &t))
            .child(div().flex().child(test_button))
            .children(result);
        vec![
            summaries.into_any_element(),
            cap.into_any_element(),
            test.into_any_element(),
        ]
    }

    /// The provider choices the shown host's daemon offers, its choice
    /// selected.
    fn provider_row(
        &self,
        status: &SummarizerStatus,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chosen = self.shown_provider_id(status);
        let choices: Vec<(SettingsControl, &'static str, bool)> = self
            .provider_controls()
            .into_iter()
            .zip(&status.providers)
            .map(|(control, provider)| {
                (
                    control,
                    intern(provider.name.clone()),
                    provider.id == chosen,
                )
            })
            .collect();
        choice_row("Provider", &choices, self.dashboard_row_lock(), focused, cx)
    }

    /// Live, or dimmed while the tab is locked; the tab's own line says why.
    fn dashboard_row_lock(&self) -> RowLock {
        if self.summarizer_locked() {
            RowLock::Dimmed
        } else {
            RowLock::Live
        }
    }

    /// For a provider that takes a key: the key source, the masked key
    /// field with its delete button or the variable field, and the key
    /// state.
    fn key_rows(
        &self,
        tab: &DashboardSettingsTab,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        if !self.summarizer_needs_key() {
            return Vec::new();
        }
        let t = theme(cx);
        let source = self.shown_key_source();
        let choices = KEY_SOURCE_CHOICES.map(|choice| {
            let text = match choice {
                KeySourceChoice::Stored => "Stored key",
                KeySourceChoice::Env => "Environment variable",
            };
            (
                SettingsControl::DashboardKeySource(choice),
                text,
                choice == source,
            )
        });
        let mut rows = vec![choice_row(
            "Key source",
            &choices,
            self.dashboard_row_lock(),
            focused,
            cx,
        )];
        match source {
            KeySourceChoice::Stored => {
                let field = self.dashboard_field(
                    SettingsControl::DashboardKey,
                    &tab.key,
                    KEY_FIELD_WIDTH,
                    focused,
                    cx,
                );
                let clear = settings_button(
                    SettingsControl::DashboardKeyClear,
                    self.dashboard_key_action_label(),
                    self.key_delete_enabled(),
                    focused,
                    cx,
                );
                rows.push(labelled_row("Key", field).child(clear).into_any_element());
            }
            KeySourceChoice::Env => {
                let field = self.dashboard_field(
                    SettingsControl::DashboardEnvVariable,
                    &tab.variable,
                    VARIABLE_FIELD_WIDTH,
                    focused,
                    cx,
                );
                rows.push(labelled_row("Variable", field).into_any_element());
                rows.push(
                    hint(crate::fonts::ui_text_size(cx), VARIABLE_HINT, &t).into_any_element(),
                );
            }
        }
        rows.extend(self.shown_key_state().map(|state| {
            div()
                .driver_id(|| "settings-dashboard-key-state".to_owned())
                .text_color(gpui::rgb(t.text_2))
                .child(key_state_words(state))
                .into_any_element()
        }));
        rows
    }

    /// A Dashboard text field `width` px wide, ringed while `control` has
    /// the keyboard; a press gives it the keyboard. While the tab is
    /// locked it is dimmed and takes no press.
    fn dashboard_field(
        &self,
        control: SettingsControl,
        input: &Entity<TextInput>,
        width: f32,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Div {
        let frame = field_frame(focused == Some(control), Layer::Floating, &theme(cx))
            .driver_id(move || control.selector().to_owned())
            .children(search_hit_marker(control.selector(), SEGMENT_RADIUS, cx))
            .w(px(width))
            .child(input.clone());
        if self.summarizer_locked() {
            return frame.opacity(DISABLED_OPACITY);
        }
        frame.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                this.press_settings_control(control, window, cx);
            }),
        )
    }
}

/// `label` with `field` beside it.
fn labelled_row(label: &'static str, field: Div) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(label)
        .child(field)
}

/// A section label over a group of the tab list's rows: uppercased, bold
/// and small in the subtle text colour, with the rows' horizontal padding
/// and no row height of its own, so it reads as a heading rather than a
/// row. Painted, never a tab stop; the selector's is tagged for the specs.
fn section_label(offset: i8, shift: i8, label: &str, t: &UiTheme) -> Div {
    crate::tagged_text("settings-host-label", label.to_uppercase())
        .flex_none()
        .px(px(12.0))
        .pt(px(2.0))
        .pb(px(4.0))
        .text_size(crate::fonts::ui_px_at(offset, SECTION_LABEL_SIZE))
        .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::BOLD))
        .text_color(gpui::rgb(t.subtle))
}

/// A group label in the tab list: painted, never a tab stop.
fn group_row(offset: i8, shift: i8, label: &'static str, t: &UiTheme) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .h(crate::fonts::ui_px_at(offset, TAB_ROW_HEIGHT))
        .px(px(12.0))
        .font_weight(crate::fonts::ui_weight_at(shift, FontWeight::SEMIBOLD))
        .text_color(gpui::rgb(t.subtle))
        .child(label)
}

/// A quiet line under or beside a control.
pub(crate) fn hint(offset: i8, text: impl Into<SharedString>, t: &UiTheme) -> Div {
    div()
        .text_size(crate::fonts::ui_px_at(offset, HINT_SIZE))
        .text_color(gpui::rgb(t.subtle))
        .child(text.into())
}

/// A click handler that presses `control`.
fn press(
    control: SettingsControl,
    cx: &mut Context<RootView>,
) -> impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static {
    cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.press_settings_control(control, window, cx);
    })
}

/// `control`'s button, outlined when it has the keyboard; a disabled one is
/// dimmed and inert.
pub(crate) fn settings_button(
    control: SettingsControl,
    label: &'static str,
    enabled: bool,
    focused: Option<SettingsControl>,
    cx: &mut Context<RootView>,
) -> Stateful<Div> {
    let button = dialog_button(
        control.selector(),
        label.to_owned(),
        dialog_look(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            false,
            enabled,
            &theme(cx),
        ),
        focused == Some(control),
    )
    .children(search_hit_marker(control.selector(), SEGMENT_RADIUS, cx));
    if enabled {
        button.on_click(press(control, cx))
    } else {
        button
    }
}

/// How a Settings toggle is drawn: ticked when `checked`, and dimmed and
/// inert when not `enabled`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ToggleState {
    pub(crate) checked: bool,
    pub(crate) enabled: bool,
}

/// `control`'s checkbox row, ticked when its state says so and ringed when
/// it has the keyboard.
pub(crate) fn toggle(
    control: SettingsControl,
    state: ToggleState,
    label: impl Into<SharedString>,
    focused: Option<SettingsControl>,
    cx: &mut Context<RootView>,
) -> Div {
    let hit = search_hit_marker(control.selector(), 0.0, cx);
    let row = checkbox_row(
        crate::fonts::ui_text_size(cx),
        crate::fonts::ui_weight_shift(cx),
        control.selector(),
        state.checked,
        focused == Some(control),
        label.into(),
        &theme(cx),
    );
    let wrap = div().relative().flex().children(hit);
    if state.enabled {
        wrap.child(row.on_click(press(control, cx)))
    } else {
        wrap.opacity(DISABLED_OPACITY).child(row.cursor_default())
    }
}

/// One agent's Settings page: the agent and the fields the page draws.
struct AgentPage {
    id: AgentId,
    tag: String,
    label: String,
    /// [`AgentOptions::settings_fields`], which a page control's indices
    /// point into.
    fields: Vec<FieldView>,
}

/// `selector` as a `&'static str`, leaked once per distinct text. An agent
/// page's selectors come from the daemon's descriptors, a few per field, and
/// a [`SettingsControl`] stays `Copy` with its selector in it.
pub(crate) fn intern(selector: String) -> &'static str {
    static INTERNED: LazyLock<Mutex<HashSet<&'static str>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));
    // The set only ever grows by whole entries, so a poisoned lock still
    // holds a usable set.
    let mut interned = INTERNED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(found) = interned.get(selector.as_str()) {
        return found;
    }
    let leaked: &'static str = Box::leak(selector.into_boxed_str());
    interned.insert(leaked);
    leaked
}

/// The selector part of field `id`: the id with `_` as `-`.
fn field_part(id: &str) -> String {
    id.replace('_', "-")
}

/// The General tab's controls in drawing order; the keep-awake choices
/// only with `keep_awake`, once the daemon reported its state, and the
/// Diagnostics rows only with `logging`, while This PC is chosen. The
/// Default shell row's dropdown and Custom command field are both listed;
/// the ring leaves them out while they take no input or are not drawn.
pub(crate) fn general_controls(keep_awake: bool, logging: bool) -> Vec<SettingsControl> {
    let keep_awake = keep_awake.then_some(KEEP_AWAKE_CHOICES.map(SettingsControl::KeepAwake));
    keep_awake
        .into_iter()
        .flatten()
        .chain(SIDEBAR_VIEW_CHOICES.map(SettingsControl::SidebarView))
        .chain(LEAF_DENSITY_CHOICES.map(SettingsControl::LeafDensity))
        .chain(SESSIONS_UNDER_REPOS_CHOICES.map(SettingsControl::SessionsUnderRepos))
        .chain(REPO_SORT_CHOICES.map(SettingsControl::RepoSort))
        .chain([
            SettingsControl::CopyOnSelect,
            SettingsControl::DefaultShell,
            SettingsControl::DefaultShellCommand,
        ])
        .chain(CHAT_MODE_CHOICES.map(SettingsControl::ChatMode))
        .chain([
            SettingsControl::ChatWidth,
            SettingsControl::CheckUpdates,
            SettingsControl::CheckNow,
        ])
        .chain(
            logging
                .then_some([
                    SettingsControl::DetailedLogging,
                    SettingsControl::OpenLogsFolder,
                ])
                .into_iter()
                .flatten(),
        )
        .collect()
}

/// The width `text` sets as the Maximum chat width: any whole number of px
/// that `native-ui.json` holds, 0 or more; one wider than any pane draws
/// as no cap.
fn parse_chat_width(text: &str) -> Option<u32> {
    text.trim().parse::<u32>().ok()
}

/// The controls of `view`, field `index` of the page of the agent tagged
/// `tag`, in drawing order: one per choice of a choice field, one for a
/// toggle, none for a kind the page does not draw.
pub(crate) fn field_controls(tag: &str, index: usize, view: &FieldView) -> Vec<SettingsControl> {
    let field = field_part(&view.id);
    match &view.editor {
        FieldEditor::Choice { choices, .. } => choices
            .iter()
            .enumerate()
            .map(|(choice, option)| SettingsControl::AgentChoice {
                field: index,
                choice,
                selector: intern(format!("settings-{tag}-{field}-{}", option.selector)),
            })
            .collect(),
        FieldEditor::Toggle => vec![SettingsControl::AgentToggle {
            field: index,
            selector: intern(format!("settings-{tag}-{field}")),
        }],
        FieldEditor::Text { .. } | FieldEditor::Model { .. } => vec![SettingsControl::AgentText {
            field: intern(view.id.clone()),
            selector: intern(format!("settings-{tag}-{field}")),
        }],
        FieldEditor::Unknown => Vec::new(),
    }
}

/// Whether `selector` names one of the controls of `view`, field `index`
/// of the page of the agent tagged `tag`; a press made before the page
/// changed under it names another field's control.
fn control_matches(tag: &str, index: usize, view: &FieldView, selector: &str) -> bool {
    field_controls(tag, index, view)
        .iter()
        .any(|control| control.selector() == selector)
}

/// The text a text or model field's input shows for `value`.
fn setting_text(value: &OptionValue) -> String {
    match value {
        OptionValue::Text(text) => text.clone(),
        OptionValue::Bool(_) | OptionValue::Null => String::new(),
    }
}

/// A row whose draft Apply would send.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingRow {
    /// Text or model field `field` of agent `agent`'s page.
    Text {
        agent: AgentId,
        field: String,
    },
    /// Agent `agent`'s launch command, both rows at once.
    Launch {
        agent: AgentId,
    },
    /// A toggle or choice held in [`PendingSettings`].
    Toggle,
    /// The Worktrees tab's path.
    WorktreesRoot,
    /// This PC's remote access port.
    RemotePort,
    /// The General tab's Maximum chat width.
    ChatWidth,
    /// The Dashboard tab's variable, cap and key.
    DashboardVariable,
    DashboardCap,
    DashboardKey,
}

/// A This PC setting a Settings control changed, waiting for Apply; held
/// once, not per host, as `native-ui.json` holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalPending {
    SidebarView(SidebarView),
    LeafDensity(LeafDensity),
    SessionsUnderRepos(SessionsUnderRepos),
    RepoSort(RepoSort),
    CopyOnSelect(bool),
    CheckUpdates(bool),
    DetailedLogging(bool),
    ChatMode(ChatMode),
    CountMode(CountMode),
    TitleCount(bool),
    TitleSuffix(bool),
    NotifyNeedsYou(bool),
    NeedsYouSound(ToastSound),
    NotifyFinished(bool),
    NotifyAccountSwitches(bool),
    FinishedSound(ToastSound),
    NotifySkip(NotifySkip),
    FinishedGrace(u32),
    ToastStyle(ToastStyle),
    Autostart(bool),
}

/// What an Apply did: the echoes its sends owe, and whether a This PC
/// setting it could not write stayed pending, which keeps OK open.
struct Applied {
    owed: Vec<OwedEcho>,
    failed: bool,
}

/// A host daemon's setting a Settings control changed, waiting for Apply.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HostPending {
    KeepAwake(KeepAwakeMode),
    /// Toggle or choice field `field` of agent `agent`'s page.
    AgentField {
        agent: AgentId,
        field: String,
        value: OptionValue,
    },
    /// The remote access switch; This PC only.
    RemoteEnabled(bool),
    /// The Default shell row's pick, as Apply sends it.
    DefaultShell(ShellSetting),
    /// Agent `agent`'s Hooks switch: install (`enabled`) or remove its
    /// hooks in the agent's user settings.
    UserHooks {
        agent: AgentId,
        enabled: bool,
    },
    Summaries(bool),
    SummaryProvider(String),
    SummaryKeySource(KeySourceChoice),
    /// The stored key of `provider` deleted on Apply.
    SummaryKeyDelete {
        provider: String,
    },
}

impl HostPending {
    /// Whether `other` sets the same setting, whatever its value.
    fn same_slot(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::AgentField { agent, field, .. },
                Self::AgentField {
                    agent: other_agent,
                    field: other_field,
                    ..
                },
            ) => agent == other_agent && field == other_field,
            (
                Self::UserHooks { agent, .. },
                Self::UserHooks {
                    agent: other_agent, ..
                },
            ) => agent == other_agent,
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }
}

/// An agent's user-settings hooks as its daemon last answered
/// [`ClientMessage::GetAgentUserHooks`] or [`ClientMessage::SetAgentUserHooks`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserHooksEntry {
    pub(crate) state: UserHooksWire,
    /// The settings file the daemon read the state from.
    pub(crate) path: Option<String>,
    /// Why the answered request failed; shown until the next answer.
    pub(crate) error: Option<String>,
}

/// A [`ClientMessage::SetAgentUserHooks`] sent and still waiting on its
/// answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserHooksPending {
    pub(crate) request_id: String,
    /// The value asked for, which the switch shows until the answer.
    pub(crate) enabled: bool,
}

/// What an agent page's Hooks section draws.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent part of the row the view reads by name"
)]
pub struct UserHooksRow {
    /// Whether the switch takes clicks: false on a daemon without the
    /// feature, where the row is dimmed.
    pub enabled: bool,
    /// Whether the switch is on.
    pub checked: bool,
    /// Whether the note asking to turn it off and on shows: some entries
    /// are missing or out of date.
    pub repair: bool,
    /// The settings file the daemon edits, once it has said.
    pub path: Option<String>,
    /// The last answer's failure.
    pub error: Option<String>,
    /// Whether a Set is still waiting on its answer.
    pub pending: bool,
}

/// The Hooks section of an agent whose descriptor has `flag` set, on a
/// connection that does (`feature`) or does not list the feature, given the
/// daemon's last answer `entry`, the switch's value `held` for Apply and
/// the Set still waiting `pending`. The switch shows the held value first,
/// then the waiting Set's, then the answer's. `None` draws no section.
pub(crate) fn user_hooks_row(
    flag: bool,
    feature: bool,
    entry: Option<&UserHooksEntry>,
    held: Option<bool>,
    pending: Option<&UserHooksPending>,
) -> Option<UserHooksRow> {
    if !flag {
        return None;
    }
    if !feature {
        return Some(UserHooksRow {
            enabled: false,
            checked: false,
            repair: false,
            path: None,
            error: None,
            pending: false,
        });
    }
    let state = entry.map(|entry| entry.state);
    if matches!(
        state,
        Some(UserHooksWire::Unsupported | UserHooksWire::Unknown)
    ) {
        return None;
    }
    let repair = matches!(state, Some(UserHooksWire::Partial | UserHooksWire::Stale));
    let installed = repair || state == Some(UserHooksWire::On);
    Some(UserHooksRow {
        enabled: true,
        checked: held.unwrap_or_else(|| pending.map_or(installed, |pending| pending.enabled)),
        repair: repair && held.is_none() && pending.is_none(),
        path: entry.and_then(|entry| entry.path.clone()),
        error: entry.and_then(|entry| entry.error.clone()),
        pending: pending.is_some(),
    })
}

/// Folds `agent`'s answer `entry`, carrying `request_id`, into one host's
/// answers and waiting Sets: the answer replaces the last, its error
/// included, and ends the Set it answers.
pub(crate) fn fold_user_hooks_reply(
    entries: &mut HashMap<AgentId, UserHooksEntry>,
    pending: &mut HashMap<AgentId, UserHooksPending>,
    agent: AgentId,
    entry: UserHooksEntry,
    request_id: Option<&str>,
) {
    let answers = pending
        .get(&agent)
        .is_some_and(|sent| request_id == Some(sent.request_id.as_str()));
    if answers {
        pending.remove(&agent);
    }
    entries.insert(agent, entry);
}

/// The daemon message that echoes a send of Apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EchoKind {
    AgentSettings,
    AgentCommands,
    WorktreesRoot,
    KeepAwake,
    Remote,
    Summarizer,
    DetailedLogging,
    DefaultShell,
    UserHooks,
}

/// A send of Apply to `host`, owing its echo; a refusal of it carries
/// `request_id` when the send had one, else no id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwedEcho {
    host: HostKey,
    kind: EchoKind,
    request_id: Option<String>,
}

impl OwedEcho {
    /// What `msg` to `host` owes, when it is a settings send.
    fn of(host: &HostKey, msg: &ClientMessage) -> Option<Self> {
        let (kind, request_id) = match msg {
            ClientMessage::SetAgentSettings { request_id, .. } => {
                (EchoKind::AgentSettings, request_id.clone())
            }
            ClientMessage::SetAgentCommand { request_id, .. } => {
                (EchoKind::AgentCommands, request_id.clone())
            }
            ClientMessage::SetWorktreesRoot { .. } => (EchoKind::WorktreesRoot, None),
            ClientMessage::SetKeepAwakeMode { .. } => (EchoKind::KeepAwake, None),
            ClientMessage::SetDetailedLogging { .. } => (EchoKind::DetailedLogging, None),
            ClientMessage::SetDefaultShell { request_id, .. } => {
                (EchoKind::DefaultShell, request_id.clone())
            }
            ClientMessage::ConfigureRemote { .. } => (EchoKind::Remote, None),
            ClientMessage::SetAgentUserHooks { request_id, .. } => {
                (EchoKind::UserHooks, request_id.clone())
            }
            ClientMessage::SetSummarizerSettings { request_id, .. }
            | ClientMessage::SetSummarizerKey { request_id, .. } => {
                (EchoKind::Summarizer, request_id.clone())
            }
            _ => return None,
        };
        Some(Self {
            host: host.clone(),
            kind,
            request_id,
        })
    }
}

/// How far an echo says its send went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EchoState {
    /// The send took effect.
    Done,
    /// Still under way: a later message says how it ended.
    InProgress,
    /// It ended without taking effect.
    Failed,
}

/// The send of Apply `msg` echoes, if it echoes one, and how far it went.
const fn echo_kind(msg: &DaemonMessage) -> Option<(EchoKind, EchoState)> {
    let kind = match msg {
        DaemonMessage::AgentSettings { .. } => EchoKind::AgentSettings,
        DaemonMessage::AgentCommands { .. } => EchoKind::AgentCommands,
        DaemonMessage::WorktreesRootChanged { .. } => EchoKind::WorktreesRoot,
        DaemonMessage::KeepAwakeStatus { .. } => EchoKind::KeepAwake,
        DaemonMessage::DetailedLogging { .. } => EchoKind::DetailedLogging,
        DaemonMessage::DefaultShell { .. } => EchoKind::DefaultShell,
        DaemonMessage::RemoteStatus { state, .. } => {
            return Some((EchoKind::Remote, remote_echo(state)));
        }
        DaemonMessage::SummarizerStatus { .. } => EchoKind::Summarizer,
        // A failed write comes back in the answer itself, not as an error.
        DaemonMessage::AgentUserHooks { error, .. } => {
            let state = if error.is_some() {
                EchoState::Failed
            } else {
                EchoState::Done
            };
            return Some((EchoKind::UserHooks, state));
        }
        _ => return None,
    };
    Some((kind, EchoState::Done))
}

/// Whether the echo `msg`, of kind `kind`, answers `owed`: an answer
/// carrying its request's id answers that send alone, since the same
/// message also answers requests Apply did not send; any other echo
/// answers any send of its kind.
fn echo_answers(owed: &OwedEcho, kind: EchoKind, msg: &DaemonMessage) -> bool {
    owed.kind == kind
        && match msg {
            DaemonMessage::AgentUserHooks { request_id, .. } => {
                owed.request_id.is_some() && owed.request_id == *request_id
            }
            _ => true,
        }
}

/// How far a `ConfigureRemote` went, as the status `state` its echo
/// carries says: off or serving took effect, starting is still under way,
/// and every other state (one this app cannot read included) ended with
/// remote access not serving.
const fn remote_echo(state: &RemoteState) -> EchoState {
    match state {
        RemoteState::Off | RemoteState::Serving { .. } => EchoState::Done,
        RemoteState::Starting => EchoState::InProgress,
        RemoteState::NotInstalled
        | RemoteState::NotRunning
        | RemoteState::OtherWindowsUser
        | RemoteState::NotConnected
        | RemoteState::TailscaleError { .. }
        | RemoteState::HttpsDisabled
        | RemoteState::HostTagged
        | RemoteState::CertFailed { .. }
        | RemoteState::PortInUse
        | RemoteState::BindFailed { .. }
        | RemoteState::Unknown => EchoState::Failed,
    }
}

/// `msg` from `host` against the echoes OK still waits for: an echo in a
/// final state crosses off the first owed send of its kind to `host`.
/// Returns whether `msg` refuses one of `host`'s sends: an error carrying
/// one's request id, any error with no id (echoes carry none to match),
/// or an echo whose send failed.
fn hear_owed_echo(wait: &mut Vec<OwedEcho>, host: &HostKey, msg: &DaemonMessage) -> bool {
    match msg {
        DaemonMessage::Error {
            request_id: Some(id),
            ..
        } => wait
            .iter()
            .any(|owed| owed.host == *host && owed.request_id.as_deref() == Some(id.as_str())),
        DaemonMessage::Error {
            request_id: None, ..
        } => wait.iter().any(|owed| owed.host == *host),
        other => {
            let Some((kind, state)) = echo_kind(other) else {
                return false;
            };
            let Some(at) = wait
                .iter()
                .position(|owed| owed.host == *host && echo_answers(owed, kind, other))
            else {
                return false;
            };
            match state {
                EchoState::Done => {
                    wait.remove(at);
                    false
                }
                EchoState::InProgress => false,
                EchoState::Failed => true,
            }
        }
    }
}

/// Settings' toggles and choices waiting for Apply, beside the text rows'
/// drafts: This PC's settings once, each host's daemon settings per host.
/// A value pressed back to the saved one is not held. Also what OK waits
/// for. Runtime only; Settings closing drops it.
#[derive(Debug, Default)]
pub(crate) struct PendingSettings {
    local: Vec<LocalPending>,
    hosts: HashMap<HostKey, Vec<HostPending>>,
    /// The sends of the Apply now running.
    log: Option<Vec<OwedEcho>>,
    /// The echoes OK waits for before Settings closes.
    ok_wait: Option<Vec<OwedEcho>>,
    /// The General tab's Maximum chat width field, built when the tab is
    /// first shown.
    chat_width: Option<ChatWidthField>,
    /// The Default shell row's list, while open.
    shell_list: Option<ShellList>,
    /// The Default shell row's Custom command field, built for the shown
    /// host while Custom is chosen there.
    shell_command: Option<ShellCommandField>,
    /// The hosts whose Default shell row shows Custom with a blank command:
    /// picked, but no pending edit.
    shell_custom_picked: HashSet<HostKey>,
    /// Each host's daemon's refusal of its last Default shell apply, shown
    /// under the row until Settings closes.
    shell_refusals: HashMap<HostKey, String>,
    /// The Accounts page's alias fields, one per account it lists, built
    /// while the page shows.
    pub(crate) account_aliases: AliasFields,
    /// The Accounts page's open Remove confirm, for the account it names.
    pub(crate) remove_confirm: Option<RemoveConfirm>,
}

/// The General tab's Maximum chat width field and its draft.
pub(crate) struct ChatWidthField {
    input: Entity<TextInput>,
    focus: FocusHandle,
    /// The field's text, kept as it changes.
    draft: String,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for ChatWidthField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatWidthField")
            .field("draft", &self.draft)
            .finish_non_exhaustive()
    }
}

impl PendingSettings {
    /// Holds `value` in place of any earlier value of its setting, or
    /// nothing for it when it is `saved`.
    fn set_local(&mut self, value: LocalPending, saved: &LocalPending) {
        let slot = std::mem::discriminant(&value);
        self.local
            .retain(|held| std::mem::discriminant(held) != slot);
        if value != *saved {
            self.local.push(value);
        }
    }

    /// Holds `value` for `host` in place of any earlier value of its
    /// setting, or nothing for it when it is `saved`.
    fn set_host(&mut self, host: &HostKey, value: HostPending, saved: Option<&HostPending>) {
        let held = self.hosts.entry(host.clone()).or_default();
        held.retain(|old| !old.same_slot(&value));
        if saved != Some(&value) {
            held.push(value);
        }
        if held.is_empty() {
            self.hosts.remove(host);
        }
    }

    /// `host`'s held values.
    fn host(&self, host: &HostKey) -> &[HostPending] {
        self.hosts.get(host).map_or(&[], Vec::as_slice)
    }

    /// `host`'s held values, no longer held.
    fn take_host(&mut self, host: &HostKey) -> Vec<HostPending> {
        self.hosts.remove(host).unwrap_or_default()
    }

    /// Drops `host`'s held deletion of `provider`'s key, if any: Undo, or a
    /// typed key that wins over it.
    fn drop_key_delete(&mut self, host: &HostKey, provider: &str) {
        let Some(held) = self.hosts.get_mut(host) else {
            return;
        };
        held.retain(|value| match value {
            HostPending::SummaryKeyDelete {
                provider: held_provider,
            } => held_provider != provider,
            _ => true,
        });
        if held.is_empty() {
            self.hosts.remove(host);
        }
    }
}

/// This PC's settings as Settings draws them: saved, with the pending
/// values over them.
pub(crate) struct ShownLocal {
    pub(crate) general: crate::sidebar::GeneralSettings,
    pub(crate) title: crate::sidebar::TitleSettings,
    pub(crate) notifications: crate::sidebar::NotificationSettings,
    pub(crate) sidebar_view: SidebarView,
    pub(crate) chat_mode: ChatMode,
    /// Whether this PC starts the daemon on login, as the Remote access
    /// page read it; `None` when it could not be read.
    pub(crate) autostart: Option<bool>,
}

impl ShownLocal {
    /// `value` drawn over these settings.
    fn take(&mut self, value: &LocalPending) {
        match value.clone() {
            LocalPending::SidebarView(view) => self.sidebar_view = view,
            LocalPending::LeafDensity(density) => self.general.leaf_density = density,
            LocalPending::SessionsUnderRepos(mode) => self.general.sessions_under_repos = mode,
            LocalPending::RepoSort(sort) => self.general.repo_sort = sort,
            LocalPending::CopyOnSelect(on) => self.general.copy_on_select = on,
            LocalPending::CheckUpdates(on) => self.general.check_updates = on,
            LocalPending::DetailedLogging(on) => self.general.detailed_logging = on,
            LocalPending::ChatMode(mode) => self.chat_mode = mode,
            LocalPending::CountMode(mode) => self.title.count = mode,
            LocalPending::TitleCount(on) => self.title.show_count = on,
            LocalPending::TitleSuffix(on) => self.title.suffix = on,
            LocalPending::NotifyNeedsYou(on) => self.notifications.awaiting_input = on,
            LocalPending::NeedsYouSound(sound) => self.notifications.needs_you_sound = sound,
            LocalPending::NotifyFinished(on) => self.notifications.finished = on,
            LocalPending::NotifyAccountSwitches(on) => self.notifications.account_switches = on,
            LocalPending::FinishedSound(sound) => self.notifications.finished_sound = sound,
            LocalPending::NotifySkip(skip) => self.notifications.skip_when = skip,
            LocalPending::FinishedGrace(secs) => self.notifications.finished_grace_secs = secs,
            LocalPending::ToastStyle(style) => self.notifications.toast_style = style,
            LocalPending::Autostart(on) => self.autostart = Some(on),
        }
    }

    /// The value these settings hold for `like`'s setting.
    fn value_like(&self, like: &LocalPending) -> LocalPending {
        let (general, title, notes) = (&self.general, &self.title, &self.notifications);
        match like {
            LocalPending::SidebarView(_) => LocalPending::SidebarView(self.sidebar_view),
            LocalPending::LeafDensity(_) => LocalPending::LeafDensity(general.leaf_density),
            LocalPending::SessionsUnderRepos(_) => {
                LocalPending::SessionsUnderRepos(general.sessions_under_repos)
            }
            LocalPending::RepoSort(_) => LocalPending::RepoSort(general.repo_sort),
            LocalPending::CopyOnSelect(_) => LocalPending::CopyOnSelect(general.copy_on_select),
            LocalPending::CheckUpdates(_) => LocalPending::CheckUpdates(general.check_updates),
            LocalPending::DetailedLogging(_) => {
                LocalPending::DetailedLogging(general.detailed_logging)
            }
            LocalPending::ChatMode(_) => LocalPending::ChatMode(self.chat_mode),
            LocalPending::CountMode(_) => LocalPending::CountMode(title.count),
            LocalPending::TitleCount(_) => LocalPending::TitleCount(title.show_count),
            LocalPending::TitleSuffix(_) => LocalPending::TitleSuffix(title.suffix),
            LocalPending::NotifyNeedsYou(_) => LocalPending::NotifyNeedsYou(notes.awaiting_input),
            LocalPending::NeedsYouSound(_) => {
                LocalPending::NeedsYouSound(notes.needs_you_sound.clone())
            }
            LocalPending::NotifyFinished(_) => LocalPending::NotifyFinished(notes.finished),
            LocalPending::NotifyAccountSwitches(_) => {
                LocalPending::NotifyAccountSwitches(notes.account_switches)
            }
            LocalPending::FinishedSound(_) => {
                LocalPending::FinishedSound(notes.finished_sound.clone())
            }
            LocalPending::NotifySkip(_) => LocalPending::NotifySkip(notes.skip_when),
            LocalPending::FinishedGrace(_) => {
                LocalPending::FinishedGrace(notes.finished_grace_secs)
            }
            LocalPending::ToastStyle(_) => LocalPending::ToastStyle(notes.toast_style),
            // Only a launch that could be read can be drafted, so the
            // unknown saved value is never compared against.
            LocalPending::Autostart(_) => LocalPending::Autostart(self.autostart == Some(true)),
        }
    }
}

impl RootView {
    /// This PC's settings saved, with `pending` over them.
    fn local_with(&self, pending: &[LocalPending]) -> ShownLocal {
        let ui = self.sidebar.ui_state();
        let mut shown = ShownLocal {
            general: ui.general.clone(),
            title: ui.title.clone(),
            notifications: ui.notifications.clone(),
            sidebar_view: ui.sidebar_view,
            chat_mode: ui.chat_mode,
            autostart: self.remote_autostart(),
        };
        for value in pending {
            shown.take(value);
        }
        shown
    }

    /// This PC's settings as Settings draws them; the app itself keeps
    /// acting on the saved ones until Apply.
    pub(crate) fn shown_local(&self) -> ShownLocal {
        self.local_with(&self.settings_pending.local)
    }

    /// A This PC toggle or choice pressed: `value` pending until Apply, or
    /// not pending once it is the saved value again.
    fn pend_local(&mut self, value: LocalPending) {
        let saved = self.local_with(&[]).value_like(&value);
        self.settings_pending.set_local(value, &saved);
    }
}

/// A pending row, and whether its draft passes the row's local
/// validation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingEdit {
    row: PendingRow,
    valid: bool,
}

/// Whether OK and Apply can be pressed over every host's `pending` edits:
/// some host holds one, and none fails its local validation.
fn apply_enabled(pending: &HashMap<HostKey, Vec<PendingEdit>>) -> bool {
    let mut edits = pending.values().flatten().peekable();
    edits.peek().is_some() && edits.all(|edit| edit.valid)
}

/// Whether a text field's `draft` is pending: the user `edited` it since it
/// was last applied or filled, and it reads otherwise than the `saved`
/// value its page shows.
fn text_pending(edited: bool, draft: &str, saved: &str) -> bool {
    edited && draft.trim() != saved.trim()
}

/// A Dashboard tab's pending drafts over the daemon's `settings`, with
/// `source` the key source shown: a named variable other than the one read,
/// a cap other than the daemon's (failing validation when it is no number
/// of dollars), and a typed key for the stored key.
fn dashboard_draft_rows(
    settings: &SummarizerSettings,
    source: KeySourceChoice,
    tab: &DashboardSettingsTab,
) -> Vec<PendingEdit> {
    let mut rows = Vec::new();
    let variable = tab.variable_draft.trim();
    if source == KeySourceChoice::Env
        && !variable.is_empty()
        && env_variable(Some(settings)).as_deref() != Some(variable)
    {
        rows.push(PendingEdit {
            row: PendingRow::DashboardVariable,
            valid: true,
        });
    }
    let cap = tab.cap_draft.trim();
    if !cap.is_empty() && parse_cap(cap) != Some(settings.daily_cap_usd) {
        rows.push(PendingEdit {
            row: PendingRow::DashboardCap,
            valid: parse_cap(cap).is_some(),
        });
    }
    if source == KeySourceChoice::Stored && !tab.key_draft.trim().is_empty() {
        rows.push(PendingEdit {
            row: PendingRow::DashboardKey,
            valid: true,
        });
    }
    rows
}

/// A pending Dashboard toggle or choice drawn over `settings`; Environment
/// variable is drawn by its named variable instead.
fn take_dashboard_value(settings: &mut SummarizerSettings, value: HostPending) {
    match value {
        HostPending::Summaries(on) => settings.enabled = on,
        HostPending::SummaryProvider(id) => settings.provider = id,
        HostPending::SummaryKeySource(KeySourceChoice::Stored) => {
            settings.key_source = SummarizerKeySource::Stored;
        }
        _ => {}
    }
}

/// The value a text field's typed `text` stores: no value when blank.
fn typed_value(text: &str) -> OptionValue {
    let text = text.trim();
    if text.is_empty() {
        OptionValue::Null
    } else {
        OptionValue::Text(text.to_owned())
    }
}

/// A text or model field's placeholder.
fn text_placeholder(view: &FieldView) -> Option<String> {
    match &view.editor {
        FieldEditor::Text { placeholder } | FieldEditor::Model { placeholder, .. } => {
            placeholder.clone()
        }
        FieldEditor::Toggle | FieldEditor::Choice { .. } | FieldEditor::Unknown => None,
    }
}

/// The input of `host`'s agent `agent`'s text field `seed` names, and its
/// subscriptions: typing keeps the draft, which waits for Apply or OK;
/// Enter applies nothing, and Esc is Cancel.
fn new_setting_text(
    host: &HostKey,
    agent: &AgentId,
    seed: TextSeed,
    window: &mut Window,
    cx: &mut Context<RootView>,
) -> (SettingText, Vec<Subscription>) {
    let input = cx.new(|cx| TextInput::new(seed.draft.clone(), seed.placeholder.clone(), cx));
    let focus = input.read(cx).focus_handle(cx);
    let keys = cx.subscribe_in(
        &input,
        window,
        |this, _, event: &TextInputEvent, window, cx| {
            if matches!(event, TextInputEvent::Cancel) {
                this.close_appearance_editor(window, cx);
            }
        },
    );
    let (edit_host, editor, edited) = (host.clone(), agent.clone(), seed.field.clone());
    let edits = cx.subscribe_in(
        &input,
        window,
        move |this, input, _: &TextChanged, _, cx| {
            let text = input.read(cx).text().to_owned();
            if let Some(setting) = this.setting_text_mut(&edit_host, &editor, &edited) {
                setting.draft = text;
                setting.edited = true;
            }
            cx.notify();
        },
    );
    let text = SettingText {
        field: seed.field,
        input,
        focus,
        draft: seed.draft,
        edited: false,
        placeholder: seed.placeholder,
    };
    (text, vec![keys, edits])
}

/// What an agent page's rows draw from: the agent's tag, whether its rows
/// take input, its fields' inputs, and the control with the keyboard.
struct RowPage<'a> {
    tag: &'a str,
    live: bool,
    tab: Option<&'a AgentTab>,
    focused: Option<SettingsControl>,
}

/// The value a press on `view` stores: choice `choice`'s value, or for a
/// toggle (`choice` is `None`) the opposite of the value shown.
fn pressed_value(view: &FieldView, choice: Option<usize>) -> Option<OptionValue> {
    match (&view.editor, choice) {
        (FieldEditor::Choice { choices, .. }, Some(choice)) => {
            choices.get(choice).map(|option| option.value.clone())
        }
        (FieldEditor::Toggle, None) => {
            Some(OptionValue::Bool(view.value != OptionValue::Bool(true)))
        }
        _ => None,
    }
}

/// The row of `view`, field `index` of agent `page`: a choice field as a
/// segmented row of its choices, a toggle as a checkbox row, a text or
/// model field as an input, and a kind this client does not know as a note
/// saying so. The row takes input only while the page is live and the
/// field is not disabled.
fn agent_field_row(
    page: &RowPage<'_>,
    index: usize,
    view: &FieldView,
    cx: &mut Context<RootView>,
) -> Option<AnyElement> {
    let t = theme(cx);
    let tag = page.tag;
    let focused = page.focused;
    let controls = field_controls(tag, index, view);
    let enabled = page.live && !view.disabled;
    let row = match &view.editor {
        FieldEditor::Choice { choices, .. } => {
            let hit = row_hit_marker(&controls, cx);
            let buttons = controls
                .iter()
                .zip(choices)
                .map(|(control, choice)| {
                    let look = Look {
                        selected: choice.value == view.value,
                        focused: focused == Some(*control),
                        enabled,
                    };
                    choice_button(
                        crate::fonts::ui_text_size(cx),
                        crate::fonts::ui_weight_shift(cx),
                        control.selector().to_owned(),
                        SharedString::from(choice.label.clone()),
                        look,
                        press(*control, cx),
                        &theme(cx),
                    )
                    .into_any_element()
                })
                .collect();
            field(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                view.label.clone(),
                &theme(cx),
            )
            .relative()
            .children(hit)
            .child(div().flex().child(segmented(buttons, &theme(cx))))
        }
        FieldEditor::Toggle => {
            let control = *controls.first()?;
            let state = ToggleState {
                checked: view.value == OptionValue::Bool(true),
                enabled,
            };
            div().flex().flex_col().gap(px(7.0)).child(toggle(
                control,
                state,
                view.label.clone(),
                focused,
                cx,
            ))
        }
        FieldEditor::Text { .. } | FieldEditor::Model { .. } => {
            let control = *controls.first()?;
            let setting = page.tab?.texts.iter().find(|text| text.field == view.id)?;
            let frame = input_frame(control, &setting.input, enabled, focused, cx);
            field(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                view.label.clone(),
                &theme(cx),
            )
            .child(if enabled {
                frame
            } else {
                frame.opacity(DISABLED_OPACITY)
            })
        }
        FieldEditor::Unknown => unknown_field_row(tag, view, cx),
    };
    Some(
        row.children(
            view.help
                .clone()
                .map(|help| hint(crate::fonts::ui_text_size(cx), help, &t)),
        )
        .into_any_element(),
    )
}

/// The row of agent page `tag`'s field `view`, of a kind this client draws
/// no control for: the note saying so. The search lists the row, so it
/// draws the outline as every other row does, outside the dimmed note.
fn unknown_field_row(tag: &str, view: &FieldView, cx: &mut Context<RootView>) -> Div {
    let selector = format!("settings-{tag}-{}", field_part(&view.id));
    let hit = search_hit_marker(&selector, SEGMENT_RADIUS, cx);
    field(
        crate::fonts::ui_text_size(cx),
        crate::fonts::ui_weight_shift(cx),
        view.label.clone(),
        &theme(cx),
    )
    .child(
        div()
            .relative()
            .driver_id(move || selector)
            .children(hit)
            .child(
                div()
                    .opacity(DISABLED_OPACITY)
                    .child(muted(UPDATE_CLIENT, &theme(cx))),
            ),
    )
}

/// A text field's frame round `input`; a click gives it the keyboard
/// unless the field takes no input.
pub(crate) fn input_frame(
    control: SettingsControl,
    input: &Entity<TextInput>,
    editable: bool,
    focused: Option<SettingsControl>,
    cx: &mut Context<RootView>,
) -> Div {
    let hit = search_hit_marker(control.selector(), SEGMENT_RADIUS, cx);
    let frame = field_frame(focused == Some(control), Layer::Floating, &theme(cx))
        .driver_id(|| control.selector().to_owned())
        .children(hit)
        .child(input.clone());
    if editable {
        frame.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                this.press_settings_control(control, window, cx);
            }),
        )
    } else {
        frame
    }
}

/// A launch-command row on a daemon that stores no launch command: a dimmed
/// field showing `placeholder` that takes no keys, and a tooltip saying how
/// to get it.
fn disabled_launch_row(
    offset: i8,
    shift: i8,
    label: &'static str,
    control: SettingsControl,
    placeholder: String,
    t: &UiTheme,
) -> Div {
    let selector = control.selector();
    let frame = field_frame(false, Layer::Floating, t)
        .id(selector)
        .driver_id(|| selector.to_owned())
        .opacity(DISABLED_OPACITY)
        .text_color(gpui::rgb(t.subtle))
        .tooltip(tooltip(LAUNCH_COMMAND_UNSUPPORTED_TIP))
        .child(placeholder);
    field(offset, shift, label, t).child(frame)
}

/// Whether a choice row takes input, and the note that says why not.
#[derive(Clone, Copy)]
pub(crate) enum RowLock {
    /// The choices take clicks and keys.
    Live,
    /// The choices are dimmed and inert; the note and its selector say why.
    Locked {
        note: &'static str,
        selector: &'static str,
    },
    /// The choices are dimmed and inert; a line elsewhere on the tab says
    /// why.
    Dimmed,
}

/// The General tab's Sessions under repos choices, `current` selected.
fn sessions_under_repos_choices(
    current: SessionsUnderRepos,
) -> [(SettingsControl, &'static str, bool); 3] {
    SESSIONS_UNDER_REPOS_CHOICES.map(|choice| {
        let text = match choice {
            SessionsUnderRepos::Every => "Every session",
            SessionsUnderRepos::Busy => "Busy only",
            SessionsUnderRepos::None => "None",
        };
        (
            SettingsControl::SessionsUnderRepos(choice),
            text,
            choice == current,
        )
    })
}

/// The General tab's Sort repos choices, `current` selected.
fn repo_sort_choices(current: RepoSort) -> [(SettingsControl, &'static str, bool); 3] {
    REPO_SORT_CHOICES.map(|choice| {
        let text = match choice {
            RepoSort::Manual => "Manual",
            RepoSort::Active => "Active first",
            RepoSort::NeedsYou => "Needs you on top",
        };
        (SettingsControl::RepoSort(choice), text, choice == current)
    })
}

/// The App title tab's count choices, `current` selected.
fn count_mode_choices(current: CountMode) -> [(SettingsControl, &'static str, bool); 3] {
    COUNT_MODE_CHOICES.map(|choice| {
        let text = match choice {
            CountMode::NeedsYou => "Needs you",
            CountMode::Busy => "Busy / total",
            CountMode::Idle => "Idle / total",
        };
        (SettingsControl::CountMode(choice), text, choice == current)
    })
}

/// The dropdown field of `kind`'s sound.
const fn sound_field(kind: NotifyKind) -> SettingsControl {
    match kind {
        NotifyKind::NeedsYou => SettingsControl::NeedsYouSoundField,
        NotifyKind::Finished => SettingsControl::FinishedSoundField,
    }
}

/// Measures the open sound list's field and the window, filling the field's
/// dropdown.
fn sound_list_measurer(cx: &mut Context<RootView>) -> impl IntoElement {
    let root = cx.weak_entity();
    canvas(
        move |field, window, cx| {
            let room = SoundListRoom {
                field,
                window_height: window.viewport_size().height,
            };
            // Fails only when the view is gone, and Settings with it.
            root.update(cx, |this, cx| this.measure_sound_list(room, window, cx))
                .ok();
        },
        |_, (), _, _| {},
    )
    .absolute()
    .inset(px(0.0))
}

/// An open sound list, with `chosen` carrying a check: under its field, over
/// it when there is more room there, inside the window and scrolling past
/// the height that leaves.
fn sound_list_rows_panel(
    list: &SoundList,
    chosen: SoundEntry,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let wanted = px(SOUND_LIST_MAX_HEIGHT);
    let placement = list.measured.map_or((false, wanted), |room| {
        list_placement(
            px(0.0)..room.window_height,
            room.field.top()..room.field.bottom(),
            wanted,
        )
    });
    let rows = SoundEntry::ALL
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            let look = SoundRowLook {
                chosen: entry == chosen,
                highlighted: index == list.highlight,
            };
            sound_list_row(entry, list.kind, index, look, cx)
        })
        .collect();
    popup_panel(
        "settings-sound-list".to_owned(),
        placement,
        Some(&list.scroll),
        rows,
        &theme(cx),
    )
}

/// One row of `kind`'s open sound list at place `index`, following the
/// pointer and chosen by a press.
fn sound_list_row(
    entry: SoundEntry,
    kind: NotifyKind,
    index: usize,
    look: SoundRowLook,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = theme(cx);
    let name = entry.selector();
    let check = format!("{name}-chosen");
    let text = labelled_text(format!("{name}-text"), entry.text(kind));
    div()
        .id(SharedString::from(name.clone()))
        .driver_id(|| name)
        .flex()
        .flex_none()
        .justify_between()
        .gap(px(6.0))
        .px(px(8.0))
        .py(px(4.0))
        .rounded(px(SEGMENT_RADIUS))
        .cursor_pointer()
        .when(look.highlighted, move |row| {
            row.bg(gpui::rgb(t.popover_hover))
        })
        .child(text)
        .when(look.chosen, |row| {
            row.child(div().driver_id(move || check).child("✓"))
        })
        .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, _, cx| {
            this.hover_sound_entry(index, cx);
        }))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
                this.choose_sound_entry(kind, entry, window, cx);
            }),
        )
        .into_any_element()
}

/// Measures the open shell list's field and the window, filling the field's
/// dropdown.
fn shell_list_measurer(cx: &mut Context<RootView>) -> impl IntoElement {
    let root = cx.weak_entity();
    canvas(
        move |field, window, cx| {
            let room = SoundListRoom {
                field,
                window_height: window.viewport_size().height,
            };
            // Fails only when the view is gone, and Settings with it.
            root.update(cx, |this, cx| this.measure_shell_list(room, window, cx))
                .ok();
        },
        |_, (), _, _| {},
    )
    .absolute()
    .inset(px(0.0))
}

/// The open Default shell list over `entries`, placed as a sound list is.
fn shell_list_rows_panel(
    list: &ShellList,
    entries: &[ShellEntry],
    cx: &mut Context<RootView>,
) -> AnyElement {
    let wanted = px(SOUND_LIST_MAX_HEIGHT);
    let placement = list.measured.map_or((false, wanted), |room| {
        list_placement(
            px(0.0)..room.window_height,
            room.field.top()..room.field.bottom(),
            wanted,
        )
    });
    let rows = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| shell_list_row(entry, index, index == list.highlight, cx))
        .collect();
    popup_panel(
        "settings-shell-list".to_owned(),
        placement,
        Some(&list.scroll),
        rows,
        &theme(cx),
    )
}

/// One row of the open Default shell list at place `index`: a choice the
/// host has follows the pointer and is picked by a press; one it lacks is
/// dimmed, takes no pick, and says why in its tooltip.
fn shell_list_row(
    entry: &ShellEntry,
    index: usize,
    highlighted: bool,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = theme(cx);
    let (hover, subtle) = (t.popover_hover, t.subtle);
    let name = format!("settings-shell-option-{}", entry.id);
    let check = format!("{name}-chosen");
    let text = labelled_text(format!("{name}-text"), entry.label.clone());
    let row = div()
        .id(SharedString::from(name.clone()))
        .driver_id(|| name)
        .flex()
        .flex_none()
        .justify_between()
        .gap(px(6.0))
        .px(px(8.0))
        .py(px(4.0))
        .rounded(px(SEGMENT_RADIUS))
        .when(highlighted, move |row| row.bg(gpui::rgb(hover)))
        .child(text)
        .when(entry.chosen, |row| {
            row.child(div().driver_id(move || check).child("✓"))
        })
        .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, _, cx| {
            this.hover_shell_entry(index, cx);
        }));
    if entry.pickable {
        let id = entry.id.clone();
        row.cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                    this.choose_shell_entry(&id, window, cx);
                }),
            )
            .into_any_element()
    } else {
        row.opacity(DISABLED_OPACITY)
            .text_color(gpui::rgb(subtle))
            .tooltip(tooltip(entry.tip.clone().unwrap_or_default()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _: &MouseDownEvent, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                }),
            )
            .into_any_element()
    }
}

/// A labelled row of segmented choices, laid out as the spawn dialog's;
/// unless `lock` is [`RowLock::Live`] the choices are dimmed, inert and out
/// of the ring.
pub(crate) fn choice_row(
    label: impl Into<SharedString>,
    choices: &[(SettingsControl, &'static str, bool)],
    lock: RowLock,
    focused: Option<SettingsControl>,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = theme(cx);
    let enabled = matches!(lock, RowLock::Live);
    let hit = choices
        .iter()
        .find_map(|(control, _, _)| search_hit_marker(control.selector(), 0.0, cx));
    let buttons = choices
        .iter()
        .map(|(control, text, selected)| {
            let look = Look {
                selected: *selected,
                focused: focused == Some(*control),
                enabled,
            };
            choice_button(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                control.selector().to_owned(),
                *text,
                look,
                press(*control, cx),
                &theme(cx),
            )
            .into_any_element()
        })
        .collect();
    let note = match lock {
        RowLock::Locked { note, selector } => Some(
            hint(crate::fonts::ui_text_size(cx), note, &t).driver_id(move || selector.to_owned()),
        ),
        RowLock::Live | RowLock::Dimmed => None,
    };
    field(
        crate::fonts::ui_text_size(cx),
        crate::fonts::ui_weight_shift(cx),
        label,
        &theme(cx),
    )
    .relative()
    .children(hit)
    .child(div().flex().child(segmented(buttons, &theme(cx))))
    .children(note)
    .into_any_element()
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use std::collections::{HashMap, VecDeque};

    use gpui::{AppContext as _, TestAppContext};
    use protocol::{AgentDescriptor, AgentId, KeepAwakeMode, OptionValue, SpawnPlacement};

    use super::{
        AgentTab, AgentTabs, HostAgentTabs, HostPending, LocalPending, PendingEdit, PendingRow,
        PendingSettings, SettingsTab, apply_enabled, control_matches, intern, next_hit_slot,
        owned_hit, row_tabs, settings_host_after_close, tab_kept, tab_rows, text_pending,
    };
    use crate::agent_options::{FieldEditor, FieldView};
    use crate::target::HostKey;
    use crate::text_input::TextInput;

    // The UI harness opens one window per spec and cannot open a second, so
    // the hit's scope to the root that owns it is held here.
    #[test]
    fn a_search_hit_is_drawn_only_for_the_root_that_owns_it() {
        let mine = gpui::EntityId::from(1_u64);
        let other = gpui::EntityId::from(2_u64);
        assert_eq!(owned_hit(Some((mine, "row")), mine), Some("row"));
        assert_eq!(
            owned_hit(Some((other, "row")), mine),
            None,
            "another window's root, and its pane appearance editor, draw no outline"
        );
        assert_eq!(owned_hit(None, mine), None);
    }

    fn hooks_entry(state: protocol::UserHooksWire) -> super::UserHooksEntry {
        super::UserHooksEntry {
            state,
            path: Some("C:\\home\\settings.json".to_owned()),
            error: None,
        }
    }

    fn hooks_pending(request_id: &str, enabled: bool) -> super::UserHooksPending {
        super::UserHooksPending {
            request_id: request_id.to_owned(),
            enabled,
        }
    }

    /// The live row, its switch at `checked`, for the entry above.
    fn live_row(checked: bool, repair: bool, pending: bool) -> super::UserHooksRow {
        super::UserHooksRow {
            enabled: true,
            checked,
            repair,
            path: Some("C:\\home\\settings.json".to_owned()),
            error: None,
            pending,
        }
    }

    #[test]
    fn user_hooks_row_hidden_without_the_descriptor_flag() {
        use protocol::UserHooksWire;
        let on = hooks_entry(UserHooksWire::On);
        assert_eq!(
            super::user_hooks_row(false, true, Some(&on), None, None),
            None
        );
        assert_eq!(super::user_hooks_row(false, true, None, None, None), None);
        assert_eq!(
            super::user_hooks_row(false, false, None, None, None),
            None,
            "an agent without the flag draws nothing, even on an older daemon"
        );
    }

    #[test]
    fn user_hooks_row_disabled_without_the_feature() {
        assert_eq!(
            super::user_hooks_row(true, false, None, None, None),
            Some(super::UserHooksRow {
                enabled: false,
                checked: false,
                repair: false,
                path: None,
                error: None,
                pending: false,
            })
        );
    }

    #[test]
    fn user_hooks_row_shows_on_off_and_repair() {
        use protocol::UserHooksWire;
        let row = |state| super::user_hooks_row(true, true, Some(&hooks_entry(state)), None, None);
        assert_eq!(row(UserHooksWire::On), Some(live_row(true, false, false)));
        assert_eq!(row(UserHooksWire::Off), Some(live_row(false, false, false)));
        assert_eq!(
            row(UserHooksWire::Partial),
            Some(live_row(true, true, false))
        );
        assert_eq!(row(UserHooksWire::Stale), Some(live_row(true, true, false)));
        let mut failed = hooks_entry(UserHooksWire::Off);
        failed.error = Some("denied".to_owned());
        assert_eq!(
            super::user_hooks_row(true, true, Some(&failed), None, None).and_then(|row| row.error),
            Some("denied".to_owned()),
            "the answer's error shows under the row"
        );
        assert_eq!(
            super::user_hooks_row(true, true, None, None, None),
            Some(super::UserHooksRow {
                path: None,
                ..live_row(false, false, false)
            }),
            "before the first answer the switch is off and takes clicks"
        );
    }

    #[test]
    fn user_hooks_row_hidden_when_unsupported_or_unknown() {
        use protocol::UserHooksWire;
        for state in [UserHooksWire::Unsupported, UserHooksWire::Unknown] {
            let entry = hooks_entry(state);
            assert_eq!(
                super::user_hooks_row(true, true, Some(&entry), None, None),
                None
            );
            assert_eq!(
                super::user_hooks_row(
                    true,
                    true,
                    Some(&entry),
                    None,
                    Some(&hooks_pending("r1", true))
                ),
                None,
                "{state:?} hides the section with a Set waiting too"
            );
        }
    }

    #[test]
    fn user_hooks_row_shows_the_pending_value_then_the_reply() {
        use protocol::UserHooksWire;
        let claude = AgentId("claude".to_owned());
        let mut entries = HashMap::new();
        let mut pending = HashMap::new();
        super::fold_user_hooks_reply(
            &mut entries,
            &mut pending,
            claude.clone(),
            hooks_entry(UserHooksWire::Off),
            Some("get"),
        );
        let row = |entries: &HashMap<AgentId, super::UserHooksEntry>,
                   pending: &HashMap<AgentId, super::UserHooksPending>| {
            super::user_hooks_row(true, true, entries.get(&claude), None, pending.get(&claude))
        };
        assert_eq!(row(&entries, &pending), Some(live_row(false, false, false)));

        pending.insert(claude.clone(), hooks_pending("set-1", true));
        assert_eq!(
            row(&entries, &pending),
            Some(live_row(true, false, true)),
            "the asked-for value shows while the Set waits"
        );
        super::fold_user_hooks_reply(
            &mut entries,
            &mut pending,
            claude.clone(),
            hooks_entry(UserHooksWire::Off),
            Some("another"),
        );
        assert_eq!(
            row(&entries, &pending),
            Some(live_row(true, false, true)),
            "an answer to another request leaves the Set waiting"
        );
        super::fold_user_hooks_reply(
            &mut entries,
            &mut pending,
            claude.clone(),
            hooks_entry(UserHooksWire::Partial),
            Some("set-1"),
        );
        assert_eq!(
            row(&entries, &pending),
            Some(live_row(true, true, false)),
            "the answer's state shows once it arrives"
        );

        pending.insert(claude.clone(), hooks_pending("set-2", false));
        assert_eq!(row(&entries, &pending), Some(live_row(false, false, true)));
        let mut failed = hooks_entry(UserHooksWire::On);
        failed.error = Some("denied".to_owned());
        super::fold_user_hooks_reply(
            &mut entries,
            &mut pending,
            claude.clone(),
            failed,
            Some("set-2"),
        );
        assert_eq!(
            row(&entries, &pending),
            Some(super::UserHooksRow {
                error: Some("denied".to_owned()),
                ..live_row(true, false, false)
            }),
            "a failed Set shows the file's state and the error"
        );
        super::fold_user_hooks_reply(
            &mut entries,
            &mut pending,
            claude.clone(),
            hooks_entry(UserHooksWire::On),
            Some("get-2"),
        );
        assert_eq!(
            row(&entries, &pending),
            Some(live_row(true, false, false)),
            "the next answer clears the error"
        );
    }

    #[test]
    fn a_root_with_no_hit_clears_only_its_own() {
        let mine = gpui::EntityId::from(1_u64);
        let other = gpui::EntityId::from(2_u64);
        assert_eq!(
            next_hit_slot(Some((mine, "row")), mine, None),
            None,
            "a closed Settings or a blank query clears this root's hit"
        );
        assert_eq!(
            next_hit_slot(Some((other, "row")), mine, None),
            Some((other, "row")),
            "a root without a hit leaves another root's for that root"
        );
        assert_eq!(
            next_hit_slot(Some((other, "row")), mine, Some("mine")),
            Some((mine, "mine"))
        );
    }

    const FIXED: [&str; 7] = [
        "General",
        "Notifications",
        "Dashboard",
        "Worktrees",
        "Remote access",
        "Appearance",
        "App title",
    ];

    fn agent(id: &str, label: &str, tag: &str) -> AgentDescriptor {
        let mut descriptor = agent_api::test_agent::DESCRIPTOR.clone();
        descriptor.id = AgentId(id.to_owned());
        label.clone_into(&mut descriptor.label);
        tag.clone_into(&mut descriptor.tag);
        descriptor
    }

    fn claude() -> AgentDescriptor {
        agent("claude", "Claude", "claude")
    }

    fn test_agent() -> AgentDescriptor {
        agent("test-agent", "Test agent", "test")
    }

    fn labels(agents: &[AgentDescriptor], remote: bool) -> Vec<&'static str> {
        tab_rows(agents, remote, false)
            .into_iter()
            .map(|row| row.label(agents))
            .collect()
    }

    fn agent_selectors(agents: &[AgentDescriptor]) -> Vec<&'static str> {
        row_tabs(&tab_rows(agents, true, false))
            .into_iter()
            .filter(|tab| matches!(tab, SettingsTab::Agent(_)))
            .map(|tab| tab.selector(agents))
            .collect()
    }

    #[test]
    fn no_listed_agent_draws_no_agents_group() {
        assert_eq!(labels(&[], true), FIXED);
        assert_eq!(agent_selectors(&[]), [] as [&str; 0]);
    }

    #[test]
    fn one_agent_gets_the_group_row_and_its_page() {
        let agents = [claude()];
        let mut want = FIXED.to_vec();
        want.extend(["Agents", "Claude"]);
        assert_eq!(labels(&agents, true), want);
        assert_eq!(agent_selectors(&agents), ["settings-tab-claude"]);
    }

    #[test]
    fn two_agents_get_a_page_each_in_the_daemons_order() {
        let agents = [test_agent(), claude()];
        let mut want = FIXED.to_vec();
        want.extend(["Agents", "Test agent", "Claude"]);
        assert_eq!(labels(&agents, true), want);
        assert_eq!(
            agent_selectors(&agents),
            ["settings-tab-test", "settings-tab-claude"],
            "a page's selector names its agent's tag"
        );
        assert!(
            !labels(&agents, false).contains(&"Remote access"),
            "Remote access only where the client manages the daemon"
        );
        assert!(
            !row_tabs(&tab_rows(&agents, true, false))
                .iter()
                .any(|tab| tab.label(&agents) == "Agents"),
            "the group row is no tab"
        );
    }

    fn box_host() -> HostKey {
        HostKey::Remote("box.tail1234.ts.net".to_owned())
    }

    /// Host agent pages where This PC and `box` each wait on one apply.
    fn two_hosts_waiting() -> AgentTabs {
        let mut hosts = AgentTabs::default();
        for (host, id) in [(HostKey::Local, "local-apply"), (box_host(), "box-apply")] {
            hosts
                .entry(&host)
                .applies
                .push_back((id.to_owned(), AgentId("claude".to_owned())));
        }
        hosts
    }

    fn waiting(hosts: &AgentTabs, host: &HostKey) -> Vec<String> {
        hosts
            .of(host)
            .map(|tabs| tabs.applies.iter().map(|(id, _)| id.clone()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn each_hosts_agent_pages_are_kept_apart() {
        let hosts = two_hosts_waiting();
        assert_eq!(waiting(&hosts, &HostKey::Local), ["local-apply"]);
        assert_eq!(waiting(&hosts, &box_host()), ["box-apply"]);
        assert!(
            hosts.of(&HostKey::Remote("lab".to_owned())).is_none(),
            "a host whose pages were never shown has none"
        );
    }

    #[test]
    fn dropping_a_hosts_pages_leaves_every_other_hosts() {
        let mut hosts = two_hosts_waiting();
        hosts.drop_host(&box_host());
        assert!(hosts.of(&box_host()).is_none(), "box's pages go");
        assert_eq!(waiting(&hosts, &HostKey::Local), ["local-apply"]);

        let mut hosts = two_hosts_waiting();
        hosts.drop_host(&HostKey::Local);
        assert!(hosts.of(&HostKey::Local).is_none(), "This PC's pages go");
        assert_eq!(waiting(&hosts, &box_host()), ["box-apply"]);
    }

    #[test]
    fn a_lost_connection_forgets_only_its_hosts_applies() {
        let mut hosts = two_hosts_waiting();
        hosts.forget_applies(&HostKey::Local);
        assert_eq!(waiting(&hosts, &HostKey::Local), Vec::<String>::new());
        assert_eq!(waiting(&hosts, &box_host()), ["box-apply"]);
        hosts.forget_applies(&HostKey::Remote("lab".to_owned()));
        assert_eq!(
            waiting(&hosts, &box_host()),
            ["box-apply"],
            "a host with no pages changes nothing"
        );
    }

    #[test]
    fn the_selector_falls_back_to_this_pc_only_when_the_shown_host_closes() {
        assert_eq!(
            settings_host_after_close(&box_host(), &box_host()),
            HostKey::Local
        );
        assert_eq!(
            settings_host_after_close(&box_host(), &HostKey::Remote("lab".to_owned())),
            box_host(),
            "another host's close leaves the shown one"
        );
        assert_eq!(
            settings_host_after_close(&HostKey::Local, &HostKey::Local),
            HostKey::Local
        );
        assert_eq!(
            settings_host_after_close(&box_host(), &HostKey::Local),
            box_host(),
            "This PC's close leaves a remote host shown"
        );
    }

    #[test]
    fn a_shown_agent_page_falls_back_to_general_when_its_agent_leaves() {
        let shown = SettingsTab::Agent(intern("test-agent".to_owned()));
        assert_eq!(
            tab_kept(shown, &tab_rows(&[claude(), test_agent()], true, false)),
            shown
        );
        assert_eq!(
            tab_kept(shown, &tab_rows(&[claude()], true, false)),
            SettingsTab::General,
            "the daemon no longer lists the agent"
        );
        assert_eq!(
            tab_kept(shown, &tab_rows(&[], true, false)),
            SettingsTab::General,
            "a disconnected client lists no agent"
        );
        assert_eq!(
            tab_kept(
                SettingsTab::RemoteAccess,
                &tab_rows(&[claude()], false, false)
            ),
            SettingsTab::General
        );
        assert_eq!(
            tab_kept(SettingsTab::Accounts, &tab_rows(&[claude()], true, false)),
            SettingsTab::General,
            "the daemon no longer lists the accounts feature"
        );
    }

    #[test]
    fn accounts_follows_remote_access_only_where_offered() {
        let mut want = FIXED.to_vec();
        want.insert(5, "Accounts");
        let shown: Vec<&str> = tab_rows(&[], true, true)
            .into_iter()
            .map(|row| row.label(&[]))
            .collect();
        assert_eq!(shown, want, "right after Remote access");
        assert_eq!(labels(&[], true), FIXED, "not offered: absent");
        assert_eq!(
            tab_kept(SettingsTab::Accounts, &tab_rows(&[], true, true)),
            SettingsTab::Accounts
        );
    }

    /// An agent page waiting on one apply of `(program, args)`, which its
    /// fields still read as.
    fn waiting_tab(cx: &mut TestAppContext, program: &str, args: &str) -> AgentTab {
        let program_input = cx.new(|cx| TextInput::new(program.to_owned(), "", cx));
        let args_input = cx.new(|cx| TextInput::new(args.to_owned(), "", cx));
        let (program_focus, args_focus) = cx.update(|cx| (cx.focus_handle(), cx.focus_handle()));
        AgentTab {
            program: program_input,
            program_focus,
            args: args_input,
            args_focus,
            program_draft: program.to_owned(),
            args_draft: args.to_owned(),
            awaiting: 1,
            sent: Some((program.to_owned(), args.to_owned())),
            saved: true,
            refusal: None,
            texts: Vec::new(),
            _subscriptions: Vec::new(),
        }
    }

    fn id(agent: &str) -> AgentId {
        AgentId(agent.to_owned())
    }

    #[gpui::test]
    fn a_dropped_page_leaves_the_apply_queue(cx: &mut TestAppContext) {
        let mut tabs = HostAgentTabs::default();
        tabs.tabs
            .insert(id("claude"), waiting_tab(cx, "wrapper", ""));
        tabs.tabs
            .insert(id("test-agent"), waiting_tab(cx, "", "verbose"));
        tabs.applies = VecDeque::from([
            ("r1".to_owned(), id("test-agent")),
            ("r2".to_owned(), id("claude")),
            ("r3".to_owned(), id("test-agent")),
        ]);
        tabs.keep_listed(&[claude()]);
        assert_eq!(
            tabs.applies,
            [("r2".to_owned(), id("claude"))],
            "the dropped agent's applies leave the queue, so the next echo answers Claude's"
        );
        assert!(tabs.tabs.contains_key(&id("claude")));
        assert!(!tabs.tabs.contains_key(&id("test-agent")));
    }

    #[gpui::test]
    fn a_disconnect_clears_the_apply_queue_and_keeps_the_drafts(cx: &mut TestAppContext) {
        let mut tabs = HostAgentTabs::default();
        tabs.tabs
            .insert(id("claude"), waiting_tab(cx, "wrapper", "-v"));
        tabs.applies = VecDeque::from([("r1".to_owned(), id("claude"))]);
        tabs.forget_applies();
        assert!(tabs.applies.is_empty(), "no echo for these can arrive");
        let tab = tabs.tabs.get(&id("claude")).expect("the page is kept");
        assert_eq!(tab.awaiting, 0, "nothing in flight");
        assert_eq!(tab.sent, None, "the apply returns to unsent");
        assert!(!tab.saved);
        assert_eq!(
            (tab.program_draft.as_str(), tab.args_draft.as_str()),
            ("wrapper", "-v"),
            "the drafts stay"
        );
    }

    /// A pending text row of Claude's page, passing its local validation
    /// when `valid`.
    fn pending_text(valid: bool) -> PendingEdit {
        PendingEdit {
            row: PendingRow::Text {
                agent: id("claude"),
                field: "profile".to_owned(),
            },
            valid,
        }
    }

    /// The hosts' pending edits as the gate reads them.
    fn pending(hosts: Vec<(HostKey, Vec<PendingEdit>)>) -> HashMap<HostKey, Vec<PendingEdit>> {
        hosts.into_iter().collect()
    }

    #[test]
    fn the_gate_is_shut_with_nothing_pending() {
        assert!(!apply_enabled(&pending(Vec::new())));
        assert!(
            !apply_enabled(&pending(vec![(HostKey::Local, Vec::new())])),
            "a host listed with no edit holds nothing pending"
        );
    }

    #[test]
    fn the_gate_opens_on_one_valid_pending_row_on_the_chosen_host() {
        assert!(apply_enabled(&pending(vec![(
            HostKey::Local,
            vec![pending_text(true)]
        )])));
    }

    #[test]
    fn a_row_failing_local_validation_shuts_the_gate_on_any_host() {
        assert!(!apply_enabled(&pending(vec![(
            HostKey::Local,
            vec![pending_text(true), pending_text(false)]
        )])));
        assert!(
            !apply_enabled(&pending(vec![
                (HostKey::Local, vec![pending_text(true)]),
                (box_host(), vec![pending_text(false)]),
            ])),
            "an invalid row on another host shuts it too"
        );
    }

    #[test]
    fn the_gate_opens_on_pending_only_on_another_host() {
        assert!(
            apply_enabled(&pending(vec![(box_host(), vec![pending_text(true)])])),
            "box's edit is pending while This PC is chosen"
        );
    }

    #[test]
    fn an_edit_typed_back_to_the_saved_value_is_not_pending() {
        assert!(text_pending(true, "work", ""));
        assert!(
            !text_pending(true, "work", "work"),
            "typed back to the saved value"
        );
        assert!(
            !text_pending(true, " work ", "work"),
            "Apply sends the text trimmed, so spaces around it change nothing"
        );
        assert!(
            !text_pending(false, "work", ""),
            "a draft Apply already sent, or a fill, is not pending"
        );
        let rows: Vec<PendingEdit> = text_pending(true, "work", "work")
            .then(|| pending_text(true))
            .into_iter()
            .collect();
        assert!(
            !apply_enabled(&pending(vec![(HostKey::Local, rows)])),
            "so the gate stays shut"
        );
    }

    #[test]
    fn a_local_toggle_is_held_until_pressed_back_to_the_saved_value() {
        let mut store = PendingSettings::default();
        let saved = LocalPending::CopyOnSelect(false);
        store.set_local(LocalPending::CopyOnSelect(true), &saved);
        assert_eq!(store.local, [LocalPending::CopyOnSelect(true)]);
        store.set_local(
            LocalPending::CheckUpdates(false),
            &LocalPending::CheckUpdates(true),
        );
        store.set_local(LocalPending::CopyOnSelect(false), &saved);
        assert_eq!(
            store.local,
            [LocalPending::CheckUpdates(false)],
            "pressed back, copy on selection is not held; the other toggle is"
        );
    }

    #[test]
    fn a_later_choice_replaces_the_held_one() {
        let mut store = PendingSettings::default();
        let saved = Some(HostPending::KeepAwake(KeepAwakeMode::Off));
        store.set_host(
            &HostKey::Local,
            HostPending::KeepAwake(KeepAwakeMode::Always),
            saved.as_ref(),
        );
        store.set_host(
            &HostKey::Local,
            HostPending::KeepAwake(KeepAwakeMode::WhenBusy),
            saved.as_ref(),
        );
        assert_eq!(
            store.host(&HostKey::Local),
            [HostPending::KeepAwake(KeepAwakeMode::WhenBusy)]
        );
        store.set_host(
            &HostKey::Local,
            HostPending::KeepAwake(KeepAwakeMode::Off),
            saved.as_ref(),
        );
        assert_eq!(store.host(&HostKey::Local), []);
        assert!(store.hosts.is_empty(), "a host holding nothing goes");
    }

    #[test]
    fn host_values_are_held_per_host_and_agent_fields_per_field() {
        let mut store = PendingSettings::default();
        let field = |name: &str, on: bool| HostPending::AgentField {
            agent: id("claude"),
            field: name.to_owned(),
            value: OptionValue::Bool(on),
        };
        store.set_host(&HostKey::Local, field("a", true), Some(&field("a", false)));
        store.set_host(&HostKey::Local, field("b", true), Some(&field("b", false)));
        store.set_host(&box_host(), field("a", true), None);
        assert_eq!(
            store.host(&HostKey::Local).len(),
            2,
            "two fields, two values"
        );
        assert_eq!(store.host(&box_host()), [field("a", true)]);
        assert_eq!(store.take_host(&box_host()), [field("a", true)]);
        assert!(store.host(&box_host()).is_empty(), "taken values go");
        assert_eq!(store.host(&HostKey::Local).len(), 2, "This PC's stay");
    }

    #[test]
    fn a_press_whose_selector_is_not_its_fields_control_is_refused() {
        let toggle = FieldView {
            id: "sandbox".to_owned(),
            label: "sandbox".to_owned(),
            help: None,
            placement: SpawnPlacement::Main,
            editor: FieldEditor::Toggle,
            value: OptionValue::Bool(false),
            disabled: false,
        };
        assert!(control_matches("test", 1, &toggle, "settings-test-sandbox"));
        assert!(
            !control_matches("test", 1, &toggle, "settings-test-profile"),
            "a press on another field's control writes nothing"
        );
    }
}
