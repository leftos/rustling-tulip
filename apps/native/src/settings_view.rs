//! The Settings modal: a tab list on the left and the chosen tab's rows
//! beside it. General holds keep-awake and copy on select, Notifications
//! the Needs you toggle and sound and Windows' toast state, Appearance
//! the appearance editor at the app level, App title the window title's
//! settings; every change applies at once. The main window's title, which
//! the App title tab controls, is kept here too.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, EntityId, FocusHandle, Focusable as _,
    FontWeight, Keystroke, MouseButton, MouseDownEvent, SharedString, Stateful, Subscription,
    Window, div, prelude::*, px,
};
use protocol::{
    AgentCommandEntry, AgentDescriptor, AgentId, ClientMessage, DaemonMessage,
    FEATURE_AGENT_COMMANDS, FieldKind, KeepAwakeMode, OptionValue, RemoteState, error_codes,
};

use crate::agent_options::{AgentOptions, FieldEditor, FieldView, descriptor_for};
use crate::agent_options_view::{LOCKED, UPDATE_CLIENT};
use crate::appearance_stops::{AppearanceStop, after_lost};
use crate::appearance_view::{Level, close_footer};
use crate::buttons::{field_frame, focus_ring};
use crate::driver_id::DriverId as _;
use crate::mouse::CopyOnSelect;
use crate::notify::{NotifyState, WINDOWS_NOTIFICATION_SETTINGS};
use crate::remote_access::{
    PortError, RemoteStatusView, address, apply_port_message, autostart_failure_line,
    autostart_read_failure_line, cert_line, controls_enabled, parse_port, state_line,
    toggle_message,
};
use crate::scrollbar::{Scrollbar, Thumb};
use crate::section::section;
use crate::session_menu::{backdrop, dialog_button, dialog_look};
use crate::sidebar::{
    CountMode, LeafDensity, NeedsYouSound, NotifySkip, RepoSort, SessionsUnderRepos, SidebarView,
};
use crate::sound::{self, SOUND_EXTENSIONS};
use crate::spawn_view::{
    DISABLED_OPACITY, Look, checkbox_row, choice_button, close_button, dialog_card,
    dialog_title_bar, field, muted, segmented,
};
use crate::target::HostKey;
use crate::text_input::{TextChanged, TextInput, TextInputEvent};
use crate::ui_theme::{UiTheme, theme};
use crate::window_title::compute_title;
use crate::{FileFilter, Pick, RootView, tooltip};

/// The tab list's width, its padding and edge included.
const TAB_LIST_WIDTH: f32 = 150.0;
/// A tab row's height and corner radius, as on the Settings board.
const TAB_ROW_HEIGHT: f32 = 34.0;
const TAB_ROW_RADIUS: f32 = 7.0;
/// As wide as the Appearance tab's two columns, so the modal keeps its size
/// from tab to tab.
const CONTENT_WIDTH: f32 = 656.0;
/// The padding round the shown tab's rows.
const CONTENT_PAD_X: f32 = 18.0;
const CONTENT_PAD_Y: f32 = 16.0;
/// The room kept above and below the panel on a window too short for it.
const PANEL_MARGIN_Y: f32 = 24.0;
/// The modal's width: the tab list, the content and its padding, and the
/// card's 1 px edges.
const PANEL_WIDTH: f32 = TAB_LIST_WIDTH + CONTENT_WIDTH + 2.0 * CONTENT_PAD_X + 2.0;
/// The panel's height whatever the category, so switching categories never
/// resizes it; a taller category scrolls.
const PANEL_HEIGHT: f32 = 640.0;
/// The size of a hint.
const HINT_SIZE: f32 = 12.5;
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
/// The port field's width.
const PORT_FIELD_WIDTH: f32 = 96.0;
/// Why an agent page's launch-command rows are disabled on a daemon that
/// does not store a launch command.
const LAUNCH_COMMAND_UNSUPPORTED_TIP: &str =
    "Restart the daemon to set the launch command: .\\rt.ps1 restart";
/// The tab list's group row over the agents' pages.
const AGENTS_GROUP: &str = "Agents";
const LAUNCH_ARGS_PLACEHOLDER: &str = "Extra arguments";
const LAUNCH_USE_HINT: &str = "Used for new sessions, Restart and Resume; not for headless runs.";
const LAUNCH_QUOTING_HINT: &str = "Extra arguments follow POSIX shell rules: quote a Windows path or double its backslashes. In cmd, a doskey macro is not expanded; set Program to a .cmd file instead.";

/// A tab of the Settings modal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum SettingsTab {
    #[default]
    General,
    Notifications,
    SpawnDefaults,
    Worktrees,
    RemoteAccess,
    Appearance,
    AppTitle,
    /// The page of the agent with this id (interned), one per agent the
    /// daemon lists.
    Agent(&'static str),
}

impl SettingsTab {
    /// The tabs shown before the agents' pages, in the list's order.
    const FIXED: [Self; 7] = [
        Self::General,
        Self::Notifications,
        Self::SpawnDefaults,
        Self::Worktrees,
        Self::RemoteAccess,
        Self::Appearance,
        Self::AppTitle,
    ];

    /// The tab's label: an agent page's is its descriptor's label in
    /// `agents`, else the agent's id.
    fn label(self, agents: &[AgentDescriptor]) -> &'static str {
        match self {
            Self::General => "General",
            Self::Notifications => "Notifications",
            Self::SpawnDefaults => "Spawn defaults",
            Self::Worktrees => "Worktrees",
            Self::RemoteAccess => "Remote access",
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
            Self::SpawnDefaults => "settings-tab-spawn-defaults",
            Self::Worktrees => "settings-tab-worktrees",
            Self::RemoteAccess => "settings-tab-remote-access",
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
/// `remote`), then, while `agents` lists any, the Agents group row and one
/// page per agent in the daemon's order. A group row is drawn between the
/// tabs and never reached by the keyboard.
fn tab_rows(agents: &[AgentDescriptor], remote: bool) -> Vec<SettingsRow> {
    let fixed = SettingsTab::FIXED
        .into_iter()
        .filter(|tab| remote || *tab != SettingsTab::RemoteAccess)
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

/// The App title tab's count choices, in the order they show.
const COUNT_MODE_CHOICES: [CountMode; 3] = [CountMode::NeedsYou, CountMode::Busy, CountMode::Idle];

/// The Notifications tab's skip choices, in the order they show.
const NOTIFY_SKIP_CHOICES: [NotifySkip; 3] =
    [NotifySkip::OnScreen, NotifySkip::Focused, NotifySkip::Never];

/// The Notifications tab's Needs you sound choices; Custom asks for the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SoundKind {
    Off,
    Default,
    Custom,
}

impl SoundKind {
    /// The choice `sound` shows as.
    const fn of(sound: &NeedsYouSound) -> Self {
        match sound {
            NeedsYouSound::Off => Self::Off,
            NeedsYouSound::Default => Self::Default,
            NeedsYouSound::Custom(_) => Self::Custom,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Default => "Default",
            Self::Custom => "Custom…",
        }
    }
}

/// The Notifications tab's Needs you sound choices, in the order they show.
const SOUND_CHOICES: [SoundKind; 3] = [SoundKind::Off, SoundKind::Default, SoundKind::Custom];

/// A control of a Settings tab that Tab can reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsControl {
    KeepAwake(KeepAwakeMode),
    SidebarView(SidebarView),
    LeafDensity(LeafDensity),
    SessionsUnderRepos(SessionsUnderRepos),
    RepoSort(RepoSort),
    CopyOnSelect,
    CheckUpdates,
    CheckNow,
    CountMode(CountMode),
    TitleCount,
    TitleSuffix,
    NotifySettingsLink,
    NotifyNeedsYou,
    NeedsYouSound(SoundKind),
    NeedsYouSoundPlay,
    NotifyFinished,
    NotifySkip(NotifySkip),
    SpawnTrusted,
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
    WorktreesSave,
    WorktreesReset,
    WorktreesManage,
    RemoteToggle,
    RemotePort,
    RemotePortApply,
    RemoteAutostart,
    AgentProgram,
    AgentArgs,
    Appearance(AppearanceStop),
}

impl SettingsControl {
    const fn selector(self) -> &'static str {
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
            Self::CheckUpdates => "settings-general-check-updates",
            Self::CheckNow => "settings-general-check-now",
            Self::CountMode(CountMode::NeedsYou) => "settings-count-needs-you",
            Self::CountMode(CountMode::Busy) => "settings-count-busy",
            Self::CountMode(CountMode::Idle) => "settings-count-idle",
            Self::TitleCount => "settings-title-count",
            Self::TitleSuffix => "settings-title-product-suffix",
            Self::NotifySettingsLink => "settings-open-windows-notifications",
            Self::NotifyNeedsYou => "settings-notify-needs-you",
            Self::NeedsYouSound(SoundKind::Off) => "settings-needs-you-sound-off",
            Self::NeedsYouSound(SoundKind::Default) => "settings-needs-you-sound-default",
            Self::NeedsYouSound(SoundKind::Custom) => "settings-needs-you-sound-custom",
            Self::NeedsYouSoundPlay => "settings-needs-you-sound-play",
            Self::NotifyFinished => "settings-notify-finished",
            Self::NotifySkip(NotifySkip::OnScreen) => "settings-notify-skip-on-screen",
            Self::NotifySkip(NotifySkip::Focused) => "settings-notify-skip-focused",
            Self::NotifySkip(NotifySkip::Never) => "settings-notify-skip-never",
            Self::SpawnTrusted => "settings-spawn-trusted",
            Self::AgentChoice { selector, .. }
            | Self::AgentToggle { selector, .. }
            | Self::AgentText { selector, .. } => selector,
            Self::WorktreesPath => "settings-worktrees-root-input",
            Self::WorktreesBrowse => "settings-worktrees-root-browse",
            Self::WorktreesSave => "settings-worktrees-root-save",
            Self::WorktreesReset => "settings-worktrees-root-reset",
            Self::WorktreesManage => "settings-worktrees-open-manager",
            Self::RemoteToggle => "settings-remote-toggle",
            Self::RemotePort => "settings-remote-port-input",
            Self::RemotePortApply => "settings-remote-port-apply",
            Self::RemoteAutostart => "settings-remote-autostart-toggle",
            Self::AgentProgram => "settings-agent-program",
            Self::AgentArgs => "settings-agent-args",
            Self::Appearance(stop) => stop.selector(),
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
    /// How many Save or Reset sends still owe the daemon's echo.
    awaiting: u32,
    /// The daemon echoed the last save.
    saved: bool,
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

    /// Whether trusted launch locks field `field` on the Settings page of
    /// the agent tagged `tag`.
    #[must_use]
    pub fn settings_field_locked(&self, tag: &str, field: &str) -> bool {
        self.settings_page_field(tag, field)
            .is_some_and(|view| view.disabled && view.editor != FieldEditor::Unknown)
    }

    /// The note the Settings page of the agent tagged `tag` shows for its
    /// field `field`: the update note for a kind this client does not know,
    /// the locked note for a field trusted launch locks, else `None`.
    #[must_use]
    pub fn settings_field_note(&self, tag: &str, field: &str) -> Option<&'static str> {
        let view = self.settings_page_field(tag, field)?;
        if view.editor == FieldEditor::Unknown {
            Some(UPDATE_CLIENT)
        } else if view.disabled {
            Some(LOCKED)
        } else {
            None
        }
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
        let descriptor = descriptor_for(self.sidebar.agents(host), id)?;
        let values = self
            .mirrors_of(host)
            .and_then(|mirrors| mirrors.agent_settings.get(id));
        let options = AgentOptions::new(descriptor.clone(), values);
        Some(AgentPage {
            id: descriptor.id.clone(),
            tag: descriptor.tag.clone(),
            label: descriptor.label.clone(),
            fields: options.settings_fields(self.sidebar.ui_state().spawn.trusted),
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
    }

    /// The selector moves to `host`: the shown tab stays while `host`
    /// offers it, else General; a shown agent or Worktrees page holds
    /// `host`'s page state, built as selecting the tab would when `host`
    /// has none. Every host's drafts stay.
    fn show_settings_host(&mut self, host: HostKey, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_host = host;
        self.keep_offered_tab();
        self.settings_control = None;
        if self.settings_open() {
            if let Some(agent) = self.settings_tab.agent() {
                self.hold_agent_tab(agent, window, cx);
            }
            if self.settings_tab == SettingsTab::Worktrees {
                self.hold_worktrees_tab(true, window, cx);
            }
        }
        cx.notify();
    }

    /// `host`'s connection closed: a remote host's agent and Worktrees
    /// pages and their drafts go, and the selector returns to This PC when
    /// it showed `host`.
    pub(crate) fn settings_host_closed(
        &mut self,
        host: &HostKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if *host != HostKey::Local {
            self.agent_tabs.drop_host(host);
            self.worktrees_tabs.remove(host);
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

    /// The tabs the list shows on this target, in order.
    fn settings_tabs(&self) -> Vec<SettingsTab> {
        row_tabs(&self.settings_rows())
    }

    /// The rows the tab list draws for the host Settings shows, in order:
    /// Remote access configures this PC's daemon, so it is offered only
    /// where the client manages it, and an agent's page only while the
    /// daemon lists it.
    fn settings_rows(&self) -> Vec<SettingsRow> {
        tab_rows(
            self.agents(),
            self.capabilities_of(&self.settings_host).manages_daemon(),
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
        }
    }

    /// Folds the daemon's agents or their Settings values, as
    /// [`RootView::on_agents`] does, and the agent pages follow them; any
    /// other message is dropped.
    pub(crate) fn fold_agents(
        &mut self,
        host: &crate::target::HostKey,
        msg: DaemonMessage,
        cx: &mut Context<Self>,
    ) {
        let listed = matches!(msg, DaemonMessage::Agents { .. });
        let pages_follow = listed || matches!(msg, DaemonMessage::AgentSettings { .. });
        self.on_agents(host, msg);
        if listed {
            self.follow_agent_list(host);
        }
        if pages_follow {
            self.refresh_setting_texts(host, cx);
        }
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
        self.settings_mirrors().keep_awake.map(|state| state.mode)
    }

    /// The keep-awake status line of the host Settings shows.
    #[must_use]
    pub fn keep_awake_status(&self) -> &'static str {
        keep_awake_status(self.settings_mirrors().keep_awake)
    }

    /// The App title tab's preview: the title for a tab named "Tab name".
    #[must_use]
    pub fn title_preview(&self) -> String {
        let preview = compute_title(Some("Tab name"), &self.sidebar.ui_state().title);
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

    /// The shown tab's controls Tab reaches, in order; a disabled one is
    /// left out.
    fn settings_controls(&self, cx: &App) -> Vec<SettingsControl> {
        match self.settings_tab {
            SettingsTab::General => general_controls(self.settings_mirrors().keep_awake.is_some()),
            SettingsTab::AppTitle => COUNT_MODE_CHOICES
                .map(SettingsControl::CountMode)
                .into_iter()
                .chain([SettingsControl::TitleCount, SettingsControl::TitleSuffix])
                .collect(),
            SettingsTab::Notifications => self.notifications_controls(),
            SettingsTab::SpawnDefaults => vec![SettingsControl::SpawnTrusted],
            SettingsTab::Agent(_) => self.agent_controls(),
            SettingsTab::Worktrees => {
                let mut controls = vec![SettingsControl::WorktreesPath];
                if self.worktrees_browses() {
                    controls.push(SettingsControl::WorktreesBrowse);
                }
                if self.worktrees_save_enabled() {
                    controls.push(SettingsControl::WorktreesSave);
                }
                if self.worktrees_override().is_some() {
                    controls.push(SettingsControl::WorktreesReset);
                }
                controls.push(SettingsControl::WorktreesManage);
                controls
            }
            SettingsTab::RemoteAccess => self.remote_access_controls(),
            SettingsTab::Appearance => self
                .appearance_stops(false, cx)
                .into_iter()
                .map(SettingsControl::Appearance)
                .collect(),
        }
    }

    /// The shown tab's stops in the order Tab walks them, each one there
    /// whether the ring holds it now or not.
    fn settings_stop_order(&self, cx: &App) -> Vec<SettingsControl> {
        match self.settings_tab {
            SettingsTab::General => general_controls(true),
            SettingsTab::AppTitle | SettingsTab::Notifications | SettingsTab::SpawnDefaults => {
                self.settings_controls(cx)
            }
            SettingsTab::Agent(_) => {
                let Some(agent) = self.settings_tab.agent() else {
                    return Vec::new();
                };
                let Some(descriptor) = descriptor_for(self.agents(), &agent) else {
                    return Vec::new();
                };
                let options =
                    AgentOptions::new(descriptor.clone(), self.agent_settings().get(&agent));
                let trusted = self.sidebar.ui_state().spawn.trusted;
                // The indices are places among every field, not among those
                // shown: only the selectors are compared with the ring.
                options
                    .every_settings_field(trusted)
                    .iter()
                    .enumerate()
                    .flat_map(|(index, view)| field_controls(&descriptor.tag, index, view))
                    .chain([SettingsControl::AgentProgram, SettingsControl::AgentArgs])
                    .collect()
            }
            SettingsTab::Worktrees => vec![
                SettingsControl::WorktreesPath,
                SettingsControl::WorktreesBrowse,
                SettingsControl::WorktreesSave,
                SettingsControl::WorktreesReset,
                SettingsControl::WorktreesManage,
            ],
            SettingsTab::RemoteAccess => vec![
                SettingsControl::RemoteToggle,
                SettingsControl::RemotePort,
                SettingsControl::RemotePortApply,
                SettingsControl::RemoteAutostart,
            ],
            SettingsTab::Appearance => self
                .appearance_stop_order(false)
                .into_iter()
                .map(SettingsControl::Appearance)
                .collect(),
        }
    }

    /// The shown agent page's controls in drawing order: each field's, then
    /// the launch rows. A field trusted launch locks is out, every field is
    /// out while [`Self::agent_rows_live`] says no, and the launch rows
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
            let submitter = (host.clone(), agent.clone());
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                move |this, _, event: &TextInputEvent, window, cx| match event {
                    TextInputEvent::Submit => {
                        this.apply_launch_command(&submitter.0, &submitter.1);
                        cx.notify();
                    }
                    TextInputEvent::Cancel => this.close_appearance_editor(window, cx),
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

    /// Enter in, or leaving, the input of `host`'s agent `agent`'s text
    /// field `field`: an edited value is sent to `host` as the field's
    /// Settings value, an empty one as no value. Nothing is sent for a
    /// field that is locked or hidden, or while the agent's rows are
    /// disabled.
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

    /// Tab leaving the field with id `field` on the shown agent page
    /// applies it, as any other way of leaving the input does.
    fn leave_agent_text(&mut self, field: &str) {
        if let Some(agent) = self.settings_tab.agent() {
            let host = self.settings_host.clone();
            self.apply_setting_text(&host, &agent, field);
        }
    }

    /// Settings closing: every text field the user edited, on any host's
    /// pages, is applied to its host, as Enter in it would apply it.
    pub(crate) fn apply_edited_texts(&mut self) {
        let edited: Vec<(HostKey, AgentId, String)> = self
            .agent_tabs
            .all()
            .flat_map(|(host, tabs)| {
                tabs.tabs.iter().flat_map(move |(agent, tab)| {
                    tab.texts
                        .iter()
                        .filter(|setting| setting.edited)
                        .map(move |setting| (host.clone(), agent.clone(), setting.field.clone()))
                })
            })
            .collect();
        for (host, agent, field) in edited {
            self.apply_setting_text(&host, &agent, &field);
        }
    }

    /// Enter in either launch-command row of `host`'s agent `agent`'s page:
    /// both fields, trimmed, as the agent's command on `host`; two empty
    /// fields clear it. Nothing is sent before the daemon reported the
    /// agent's command.
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
        self.send_to(
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
        tab.saved = false;
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
            SettingsControl::AgentProgram => {
                self.shown_agent_tab().map(|tab| tab.program_focus.clone())
            }
            SettingsControl::AgentArgs => self.shown_agent_tab().map(|tab| tab.args_focus.clone()),
            SettingsControl::AgentText { field, .. } => self.agent_text_focus(field),
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

    /// Moves the keyboard one step along the ring the tab list and the
    /// shown tab's controls make, backwards with `back`. From anywhere off
    /// the ring it goes to the tab list.
    fn step_settings_focus(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(SettingsControl::AgentText { field, .. }) =
            self.focused_settings_control(window, cx)
        {
            self.leave_agent_text(field);
        }
        let controls = self.settings_controls(cx);
        let at = if self.settings_tabs_focus.is_focused(window) {
            Some(0)
        } else {
            self.focused_control_in(&controls, window, cx)
                .and_then(|control| controls.iter().position(|c| *c == control))
                .map(|index| index + 1)
        };
        let len = controls.len() + 1;
        let next = match at {
            None => 0,
            Some(at) if back => (at + len - 1) % len,
            Some(at) => (at + 1) % len,
        };
        if next == 0 {
            self.settings_control = None;
            self.settings_tabs_focus.focus(window);
        } else {
            self.move_settings_focus(controls[next - 1], window, cx);
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
        match control {
            SettingsControl::KeepAwake(mode) => self.choose_keep_awake_mode(mode),
            SettingsControl::SidebarView(view) => self.set_sidebar_view(view, cx),
            SettingsControl::LeafDensity(density) => self.set_leaf_density(density, cx),
            SettingsControl::SessionsUnderRepos(mode) => self.set_sessions_under_repos(mode, cx),
            SettingsControl::RepoSort(sort) => self.set_repo_sort(sort, cx),
            SettingsControl::CopyOnSelect => self.toggle_copy_on_select(cx),
            SettingsControl::CheckUpdates => self.toggle_check_updates(cx),
            SettingsControl::CheckNow => self.check_for_updates_now(cx),
            SettingsControl::CountMode(mode) => self.set_count_mode(mode, window, cx),
            SettingsControl::TitleCount => self.toggle_title_count(window, cx),
            SettingsControl::TitleSuffix => self.toggle_title_suffix(window, cx),
            SettingsControl::NotifySettingsLink => self.open_windows_notification_settings(cx),
            SettingsControl::NotifyNeedsYou => self.toggle_notification(cx),
            SettingsControl::NeedsYouSound(kind) => self.choose_needs_you_sound(kind, cx),
            SettingsControl::NeedsYouSoundPlay => self.play_needs_you_sound(cx),
            SettingsControl::NotifyFinished => self.toggle_finished_notification(cx),
            SettingsControl::NotifySkip(skip) => self.set_notify_skip(skip, cx),
            SettingsControl::SpawnTrusted => self.toggle_spawn_trusted(cx),
            SettingsControl::AgentChoice {
                field,
                choice,
                selector,
            } => self.press_agent_field(field, Some(choice), selector),
            SettingsControl::AgentToggle { field, selector } => {
                self.press_agent_field(field, None, selector);
            }
            SettingsControl::WorktreesPath
            | SettingsControl::RemotePort
            | SettingsControl::AgentProgram
            | SettingsControl::AgentArgs
            | SettingsControl::AgentText { .. } => {}
            SettingsControl::WorktreesBrowse => self.browse_worktrees_root(cx),
            SettingsControl::WorktreesSave => self.save_worktrees_root(),
            SettingsControl::WorktreesReset => self.reset_worktrees_root(cx),
            SettingsControl::WorktreesManage => self.open_worktrees_manager(window, cx),
            SettingsControl::RemoteToggle => self.toggle_remote_access(),
            SettingsControl::RemotePortApply => self.apply_remote_port(),
            SettingsControl::RemoteAutostart => self.toggle_autostart(),
            SettingsControl::Appearance(stop) => {
                self.press_appearance_stop(stop, window, cx);
                self.keep_keyboard_after_press(stop, window, cx);
            }
        }
        cx.notify();
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

    /// Whether the path field differs from the override (empty when none).
    fn worktrees_save_enabled(&self) -> bool {
        self.shown_worktrees_tab()
            .is_some_and(|tab| tab.draft.trim() != self.worktrees_override().unwrap_or(""))
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

    /// The Save button's label and whether it can be pressed.
    #[must_use]
    pub fn worktrees_save_button(&self) -> (&'static str, bool) {
        let enabled = self.worktrees_save_enabled();
        let saved = self.shown_worktrees_tab().is_some_and(|tab| tab.saved);
        (if saved && !enabled { "Saved" } else { "Save" }, enabled)
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

    /// The Worktrees tab's path field, while the tab has been shown.
    #[must_use]
    pub fn worktrees_path(&self) -> Option<String> {
        self.shown_worktrees_tab().map(|tab| tab.draft.clone())
    }

    /// The shown host's Worktrees tab, built afresh unless `keep` and it is
    /// held.
    fn hold_worktrees_tab(&mut self, keep: bool, window: &mut Window, cx: &mut Context<Self>) {
        let host = self.settings_host.clone();
        if keep && self.worktrees_tabs.contains_key(&host) {
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
            |this, _, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => {
                    this.press_settings_control(SettingsControl::WorktreesSave, window, cx);
                }
                TextInputEvent::Cancel => this.close_appearance_editor(window, cx),
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
            saved: false,
            _subscriptions: vec![keys, edits],
        }
    }

    /// Save: the field as the override, or none when it is empty.
    fn save_worktrees_root(&mut self) {
        if !self.worktrees_save_enabled() {
            return;
        }
        let host = self.settings_host.clone();
        let Some(tab) = self.worktrees_tabs.get_mut(&host) else {
            return;
        };
        let trimmed = tab.draft.trim();
        let path = (!trimmed.is_empty()).then(|| trimmed.to_owned());
        tab.awaiting += 1;
        tab.saved = false;
        tracing::info!(?path, ?host, "settings: saving the worktrees root");
        self.send_to(&host, ClientMessage::SetWorktreesRoot { path });
    }

    fn reset_worktrees_root(&mut self, cx: &mut Context<Self>) {
        if self.worktrees_override().is_none() {
            return;
        }
        let host = self.settings_host.clone();
        if let Some(tab) = self.worktrees_tabs.get_mut(&host) {
            tab.awaiting += 1;
            tab.saved = false;
            tab.draft.clear();
            tab.input.update(cx, |input, cx| input.set_text("", cx));
        }
        tracing::info!(?host, "settings: resetting the worktrees root");
        self.send_to(&host, ClientMessage::SetWorktreesRoot { path: None });
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
                if settles {
                    tab.saved = true;
                }
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
        (
            status.is_some_and(|status| status.enabled),
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

    /// What Apply sends: the field's port with the switch as it is, once
    /// the daemon has reported and only for a valid port other than its own.
    fn remote_apply_message(&self) -> Option<ClientMessage> {
        let status = self.mirrors().remote_status.as_ref()?;
        let tab = self.remote_access_tab.as_ref()?;
        apply_port_message(status, &tab.draft)
    }

    /// Whether the port's Apply can be pressed.
    #[must_use]
    pub fn remote_port_apply(&self) -> bool {
        self.remote_apply_message().is_some()
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
    /// the port once the daemon has reported, Apply for a changed valid
    /// port, and the login launch once it was read.
    fn remote_access_controls(&self) -> Vec<SettingsControl> {
        let mut controls = Vec::new();
        if controls_enabled(self.mirrors().remote_status.as_ref()) {
            controls.extend([SettingsControl::RemoteToggle, SettingsControl::RemotePort]);
        }
        if self.remote_port_apply() {
            controls.push(SettingsControl::RemotePortApply);
        }
        if self.remote_autostart().is_some() {
            controls.push(SettingsControl::RemoteAutostart);
        }
        controls
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
            |this, _, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => {
                    this.apply_remote_port();
                    cx.notify();
                }
                TextInputEvent::Cancel => this.close_appearance_editor(window, cx),
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

    fn toggle_remote_access(&self) {
        if let Some(status) = &self.mirrors().remote_status {
            tracing::info!(
                enabled = !status.enabled,
                "settings: switching remote access"
            );
            self.send_to(&HostKey::Local, toggle_message(status));
        }
    }

    fn apply_remote_port(&self) {
        if let Some(message) = self.remote_apply_message() {
            tracing::info!("settings: applying the remote access port");
            self.send_to(&HostKey::Local, message);
        }
    }

    /// Switches the login launch and reads it back; a failed switch leaves
    /// it as it was and says why under the toggle.
    fn toggle_autostart(&mut self) {
        let Some(Ok(on)) = self
            .remote_access_tab
            .as_ref()
            .map(|tab| tab.autostart.clone())
        else {
            return;
        };
        let switched = self.autostart.set(!on);
        let read = switched.is_ok().then(|| self.read_autostart());
        let Some(tab) = &mut self.remote_access_tab else {
            return;
        };
        match switched {
            Ok(()) => {
                tab.autostart_error = None;
                if let Some(read) = read {
                    tab.autostart = read;
                }
            }
            Err(error) => {
                tracing::warn!("settings: switching the login launch failed: {error}");
                tab.autostart_error = Some(autostart_failure_line(&error));
            }
        }
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
            self.settings_tabs_focus.focus(window);
            self.arm_pairing_tick(window, cx);
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
        self.arm_pairing_tick(window, cx);
        if tab == SettingsTab::Notifications {
            self.read_notify_state(window, cx);
            self.check_needs_you_sound(cx);
        }
        if tab == SettingsTab::Worktrees {
            self.hold_worktrees_tab(false, window, cx);
        }
        if tab == SettingsTab::RemoteAccess {
            self.remote_access_tab = Some(self.new_remote_access_tab(window, cx));
            self.on_devices_page_selected();
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

    /// The Notifications tab's stops: the link, Needs you with its sound
    /// choices and their Play (not while the sound is off), Finished, then
    /// the skip choices.
    fn notifications_controls(&self) -> Vec<SettingsControl> {
        let sound = SoundKind::of(&self.sidebar.ui_state().notifications.needs_you_sound);
        let mut controls = vec![
            SettingsControl::NotifySettingsLink,
            SettingsControl::NotifyNeedsYou,
        ];
        controls.extend(SOUND_CHOICES.map(SettingsControl::NeedsYouSound));
        if sound != SoundKind::Off {
            controls.push(SettingsControl::NeedsYouSoundPlay);
        }
        controls.push(SettingsControl::NotifyFinished);
        controls.extend(NOTIFY_SKIP_CHOICES.map(SettingsControl::NotifySkip));
        controls
    }

    /// Off and Default apply at once; Custom asks this PC's file picker for
    /// a `.wav` or `.ogg`, and a cancel keeps the choice as it was.
    fn choose_needs_you_sound(&mut self, kind: SoundKind, cx: &mut Context<Self>) {
        match kind {
            SoundKind::Off => self.set_needs_you_sound(NeedsYouSound::Off, cx),
            SoundKind::Default => self.set_needs_you_sound(NeedsYouSound::Default, cx),
            SoundKind::Custom => self.pick_needs_you_sound(cx),
        }
    }

    /// Asks for the custom sound file, opening at the current one's folder.
    /// The sound plays on this PC, so the picker is always this PC's, even
    /// when the window is on another host.
    fn pick_needs_you_sound(&mut self, cx: &mut Context<Self>) {
        let start = match &self.sidebar.ui_state().notifications.needs_you_sound {
            NeedsYouSound::Custom(path) => path.parent().map(std::path::Path::to_path_buf),
            NeedsYouSound::Off | NeedsYouSound::Default => None,
        };
        let filter = FileFilter::new("Sound", &SOUND_EXTENSIONS);
        let picked = (self.pick_file)(cx, start, filter);
        cx.spawn(async move |this, cx| {
            let Some(path) = picked.await else {
                return;
            };
            // Fails only when the view is gone, and Settings with it.
            this.update(cx, |this, cx| {
                this.set_needs_you_sound(NeedsYouSound::Custom(path), cx);
            })
            .ok();
        })
        .detach();
    }

    /// Records `sound`, saving it when it changed, and checks its file; Off
    /// drops the clip waiting to play, and the one playing ends on its own.
    fn set_needs_you_sound(&mut self, sound: NeedsYouSound, cx: &mut Context<Self>) {
        if sound == NeedsYouSound::Off {
            self.notifications.sounder.drop_waiting();
        }
        if self.sidebar.set_needs_you_sound(sound) {
            self.save_ui();
        }
        self.check_needs_you_sound(cx);
        cx.notify();
    }

    /// Checks the chosen custom sound file on a background thread; the tab
    /// shows what is wrong with it once the check is back, unless the
    /// choice changed meanwhile.
    fn check_needs_you_sound(&mut self, cx: &mut Context<Self>) {
        let choice = self
            .sidebar
            .ui_state()
            .notifications
            .needs_you_sound
            .clone();
        self.notifications.sound_problem = None;
        let checked = choice.clone();
        let check = cx
            .background_executor()
            .spawn(async move { sound::problem_of(&checked) });
        cx.spawn(async move |this, cx| {
            let problem = check.await;
            // Fails only when the view is gone, and Settings with it.
            this.update(cx, |this, cx| {
                if this.sidebar.ui_state().notifications.needs_you_sound == choice {
                    this.notifications.sound_problem = problem;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn toggle_notification(&mut self, cx: &mut Context<Self>) {
        let on = !self.sidebar.ui_state().notifications.awaiting_input;
        self.sidebar.set_notification(on);
        self.save_ui();
        cx.notify();
    }

    fn toggle_finished_notification(&mut self, cx: &mut Context<Self>) {
        let on = !self.sidebar.ui_state().notifications.finished;
        self.sidebar.set_finished_notification(on);
        self.save_ui();
        cx.notify();
    }

    /// Sets when a notification for a session on screen is skipped, saving
    /// it when it changed.
    fn set_notify_skip(&mut self, skip: NotifySkip, cx: &mut Context<Self>) {
        if self.sidebar.set_notify_skip(skip) {
            self.save_ui();
        }
        cx.notify();
    }

    fn toggle_spawn_trusted(&mut self, cx: &mut Context<Self>) {
        let on = !self.sidebar.ui_state().spawn.trusted;
        self.sidebar.set_spawn_trusted(on);
        self.save_ui();
        cx.notify();
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
        self.set_agent_setting(&host, &page.id, &view.id, value, false);
    }

    /// Sends `host`'s agent `agent`'s Settings values with `key` set to
    /// `value`, on top of any change still waiting for that daemon's
    /// broadcast, under a fresh request id; `from_text` marks the apply of
    /// text field `key`. Nothing is sent while the agent's rows are
    /// disabled.
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
        self.send_to(
            host,
            ClientMessage::SetAgentSettings {
                agent: agent.clone(),
                options,
                request_id: Some(request_id),
            },
        );
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
    /// Tab and Shift+Tab walk the ring of the tab list and the shown tab's
    /// controls, Up and Down on the tab list change the tab, Space or
    /// Enter presses the focused control, and the arrows move along an
    /// Appearance swatch line or family list. While a text field has the
    /// keyboard, only Tab is the frame's.
    pub(crate) fn on_settings_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mods = &keystroke.modifiers;
        if !self.settings_open() || mods.control || mods.alt || mods.platform || mods.function {
            return false;
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
                let tab = self
                    .settings_tab
                    .step(&self.settings_tabs(), keystroke.key == "down");
                self.select_settings_tab(tab, window, cx);
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
        self.worktrees_path_focused(window)
            || self.remote_port_focused(window)
            || self.launch_program_focused(window)
            || self.launch_args_focused(window)
            || self.agent_texts_focused(window)
            || self.appearance_field_focused(window, cx)
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

    /// Asks the daemon for `mode`; the choice follows its echo. Nothing is
    /// sent while the daemon is not connected.
    fn choose_keep_awake_mode(&mut self, mode: KeepAwakeMode) {
        if self.settings_mirrors().keep_awake.is_some() {
            self.send_to(
                &self.settings_host,
                ClientMessage::SetKeepAwakeMode { mode },
            );
        }
    }

    /// Sets how much room each session leaf takes, saving it when it
    /// changed.
    fn set_leaf_density(&mut self, density: LeafDensity, cx: &mut Context<Self>) {
        if self.sidebar.set_leaf_density(density) {
            self.save_ui();
        }
        cx.notify();
    }

    /// Sets how many of a repo or workspace's sessions show under its row,
    /// saving it when it changed.
    fn set_sessions_under_repos(&mut self, mode: SessionsUnderRepos, cx: &mut Context<Self>) {
        if self.sidebar.set_sessions_under_repos(mode) {
            self.save_ui();
        }
        cx.notify();
    }

    /// Sets how the repos view orders its repo and workspace rows, saving it
    /// when it changed.
    fn set_repo_sort(&mut self, sort: RepoSort, cx: &mut Context<Self>) {
        if self.sidebar.set_repo_sort(sort) {
            self.save_ui();
        }
        cx.notify();
    }

    fn toggle_copy_on_select(&mut self, cx: &mut Context<Self>) {
        let on = !self.sidebar.ui_state().general.copy_on_select;
        self.sidebar.set_copy_on_select(on);
        cx.set_global(CopyOnSelect(on));
        self.save_ui();
        cx.notify();
    }

    fn toggle_check_updates(&mut self, cx: &mut Context<Self>) {
        let on = !self.sidebar.ui_state().general.check_updates;
        self.sidebar.set_check_updates(on);
        self.save_ui();
        cx.notify();
    }

    fn set_count_mode(&mut self, mode: CountMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar.set_count_mode(mode) {
            self.after_title_setting(window, cx);
        }
    }

    fn toggle_title_count(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let on = !self.sidebar.ui_state().title.show_count;
        self.sidebar.set_title_show_count(on);
        self.after_title_setting(window, cx);
    }

    fn toggle_title_suffix(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let on = !self.sidebar.ui_state().title.suffix;
        self.sidebar.set_title_suffix(on);
        self.after_title_setting(window, cx);
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
        compute_title(
            self.tabs.active_tab().map(|tab| tab.name.as_str()),
            &self.sidebar.ui_state().title,
        )
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
                    .flex_none()
                    .child(
                        div()
                            .id("settings-content")
                            .debug_selector(|| "settings-content".to_owned())
                            .overflow_y_scroll()
                            .track_scroll(self.settings_scroll.handle())
                            .flex()
                            .flex_col()
                            .flex_none()
                            .w(px(CONTENT_WIDTH + 2.0 * CONTENT_PAD_X))
                            .px(px(CONTENT_PAD_X))
                            .py(px(CONTENT_PAD_Y))
                            .children(self.settings_content(window, cx)),
                    )
                    .child(self.settings_scroll.overlay()),
            );
        let cap = window.viewport_size().height / px(1.0) - 2.0 * PANEL_MARGIN_Y;
        let panel = dialog_card(
            crate::fonts::ui_text_size(cx),
            "settings-panel",
            PANEL_WIDTH,
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
        .child(close_footer("settings-footer-close", false, cx));
        Some(backdrop("settings", panel, &theme(cx)))
    }

    /// The tab list on the raised ground; the shown tab is filled and bold,
    /// and ringed while the list has the keyboard. A group row is a label
    /// only: no id, no click, no hover and no ring.
    fn settings_tab_list(&self, window: &Window, cx: &mut Context<Self>) -> Stateful<Div> {
        let t = theme(cx);
        let list_focused = self.settings_tabs_focus.is_focused(window);
        let tabs: Vec<AnyElement> = self
            .settings_rows()
            .into_iter()
            .map(|row| match row {
                SettingsRow::Group(label) => group_row(
                    crate::fonts::ui_text_size(cx),
                    crate::fonts::ui_weight_shift(cx),
                    label,
                    &t,
                )
                .into_any_element(),
                SettingsRow::Tab(tab) => self
                    .settings_tab_row(tab, list_focused, cx)
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
            .children(self.settings_host_row(cx))
            .children(tabs)
    }

    /// The host selector over the tab list, while the window holds two or
    /// more hosts: a Host label, then one row per held host, the shown one
    /// filled. A host not connected is dimmed and takes no click. It is not
    /// a keyboard stop.
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
            .map(|(host, open)| {
                let shown = host == self.settings_host;
                let selector = format!("settings-host-{}", host.wire().unwrap_or("local"));
                let label = crate::sidebar::host_label(&host);
                let text = match (shown, open) {
                    (true, _) => t.text,
                    (false, true) => t.text_2,
                    (false, false) => t.muted,
                };
                div()
                    .id(gpui::SharedString::from(selector.clone()))
                    .driver_id(move || selector.clone())
                    .flex()
                    .flex_none()
                    .items_center()
                    .h(crate::fonts::ui_px(cx, TAB_ROW_HEIGHT))
                    .px(px(12.0))
                    .rounded(px(TAB_ROW_RADIUS))
                    .text_color(gpui::rgb(text))
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
                    .child(label)
                    .into_any_element()
            })
            .collect();
        Some(
            div()
                .driver_id(|| "settings-host-row".to_owned())
                .flex()
                .flex_col()
                .gap(px(2.0))
                .pb(px(8.0))
                .child(group_row(
                    size,
                    crate::fonts::ui_weight_shift(cx),
                    "Host",
                    &t,
                ))
                .children(rows),
        )
    }

    /// One clickable tab row; the shown one is filled, bold, and ringed
    /// while the list has the keyboard.
    fn settings_tab_row(
        &self,
        tab: SettingsTab,
        list_focused: bool,
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
            SettingsTab::SpawnDefaults => self.spawn_defaults_tab(focused, cx),
            SettingsTab::Agent(_) => self.agent_page_content(focused, cx),
            SettingsTab::Worktrees => self.worktrees_tab_content(focused, cx),
            SettingsTab::RemoteAccess => self.remote_access_tab_content(focused, cx),
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
        let field = field_frame(focused == Some(path), &theme(cx))
            .driver_id(|| path.selector().to_owned())
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
        let (save_label, save_enabled) = self.worktrees_save_button();
        let buttons = div()
            .flex()
            .gap(px(8.0))
            .child(settings_button(
                SettingsControl::WorktreesSave,
                save_label,
                save_enabled,
                focused,
                cx,
            ))
            .child(settings_button(
                SettingsControl::WorktreesReset,
                "Reset to default",
                self.worktrees_reset_enabled(),
                focused,
                cx,
            ))
            .child(settings_button(
                SettingsControl::WorktreesManage,
                "Manage worktrees…",
                true,
                focused,
                cx,
            ));
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
        .child(self.remote_port_row(tab, enabled, focused, cx))
        .children(port_error)
        .child(hint(crate::fonts::ui_text_size(cx), REMOTE_PORT_HINT, &t))
        .child(hint(crate::fonts::ui_text_size(cx), REMOTE_RUN_HINT, &t))
        .children(self.autostart_rows(tab, focused, cx));
        vec![section.into_any_element()]
    }

    /// The port field and its Apply; before the daemon reports, the field
    /// is a dimmed placeholder that takes no keys.
    fn remote_port_row(
        &self,
        tab: &RemoteAccessTab,
        enabled: bool,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Div {
        let t = theme(cx);
        let port = SettingsControl::RemotePort;
        let frame = field_frame(focused == Some(port), &theme(cx))
            .driver_id(|| port.selector().to_owned())
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
        let apply = settings_button(
            SettingsControl::RemotePortApply,
            "Apply",
            self.remote_port_apply(),
            focused,
            cx,
        );
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child("Port")
            .child(field)
            .child(apply)
    }

    /// The login-launch toggle, its hint, and why its last switch or read
    /// failed; the toggle is inert while the launch could not be read.
    fn autostart_rows(
        &self,
        tab: &RemoteAccessTab,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<Div> {
        let t = theme(cx);
        let state = ToggleState {
            checked: tab.autostart.as_ref().is_ok_and(|on| *on),
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

    /// The Spawn defaults tab: trusted launch, which locks the agent fields
    /// marked for it on their pages while it is on.
    fn spawn_defaults_tab(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let t = theme(cx);
        let trusted = toggle(
            SettingsControl::SpawnTrusted,
            ToggleState {
                checked: self.sidebar.ui_state().spawn.trusted,
                enabled: true,
            },
            "Trusted launch by default",
            focused,
            cx,
        );
        let section = section(
            crate::fonts::ui_text_size(cx),
            crate::fonts::ui_weight_shift(cx),
            "Spawn defaults",
        )
        .child(hint(
            crate::fonts::ui_text_size(cx),
            "Pre-fill these in the spawn dialog. You can override on every spawn.",
            &t,
        ))
        .child(trusted)
        .child(hint(
            crate::fonts::ui_text_size(cx),
            "When enabled, new agent sessions skip their approval prompts.",
            &t,
        ));
        vec![section.into_any_element()]
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
        sections
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
        let settings = &self.sidebar.ui_state().notifications;
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
                 with no background work left.",
            &t,
        ))
        .child(needs_you)
        .child(sound)
        .child(finished)
        .child(skip_row);
        vec![section.into_any_element()]
    }

    /// The Needs you sound's choices with Play beside them (not while the
    /// sound is off), then the custom file's path and what is wrong with it.
    fn needs_you_sound_rows(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Div {
        let t = theme(cx);
        let size = crate::fonts::ui_text_size(cx);
        let sound = &self.sidebar.ui_state().notifications.needs_you_sound;
        let current = SoundKind::of(sound);
        let choices = SOUND_CHOICES.map(|choice| {
            (
                SettingsControl::NeedsYouSound(choice),
                choice.label(),
                choice == current,
            )
        });
        let choices = choice_row("Needs you sound", &choices, RowLock::Live, focused, cx);
        let play = (current != SoundKind::Off).then(|| {
            settings_button(
                SettingsControl::NeedsYouSoundPlay,
                "Play",
                true,
                focused,
                cx,
            )
        });
        let path = match sound {
            NeedsYouSound::Custom(path) => Some(
                hint(size, path.display().to_string(), &t)
                    .driver_id(|| "settings-needs-you-sound-path".to_owned()),
            ),
            NeedsYouSound::Off | NeedsYouSound::Default => None,
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
                    .child(choices)
                    .children(play),
            )
            .children(path)
            .children(problem)
    }

    fn general_tab(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let t = theme(cx);
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
            "Keep this PC awake",
            &modes,
            locked,
            focused,
            cx,
        ))
        .children(status);
        let sidebar = self.sidebar_section(focused, cx);
        let copy = toggle(
            SettingsControl::CopyOnSelect,
            ToggleState {
                checked: self.sidebar.ui_state().general.copy_on_select,
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
        ));
        vec![
            power.into_any_element(),
            sidebar,
            terminal.into_any_element(),
            self.updates_section(focused, cx).into_any_element(),
        ]
    }

    /// The Updates section: the automatic-check toggle, the client's version
    /// and the last check's result, Check now, and Restart to update once an
    /// update is ready.
    fn updates_section(&self, focused: Option<SettingsControl>, cx: &mut Context<Self>) -> Div {
        let t = theme(cx);
        let check = toggle(
            SettingsControl::CheckUpdates,
            ToggleState {
                checked: self.sidebar.ui_state().general.check_updates,
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
                .children(self.settings_restart_button(cx)),
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
        let view = self.sidebar.sidebar_view();
        let choices = SIDEBAR_VIEW_CHOICES.map(|choice| {
            let text = match choice {
                SidebarView::Repos => "Repos",
                SidebarView::Tabs => "Tabs",
            };
            (SettingsControl::SidebarView(choice), text, choice == view)
        });
        let density = self.sidebar.leaf_density();
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
        let under = sessions_under_repos_choices(self.sidebar.sessions_under_repos());
        let sorts = repo_sort_choices(self.sidebar.repo_sort());
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
        let title = &self.sidebar.ui_state().title;
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
fn hint(offset: i8, text: impl Into<SharedString>, t: &UiTheme) -> Div {
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
fn settings_button(
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
    );
    if enabled {
        button.on_click(press(control, cx))
    } else {
        button
    }
}

/// How a Settings toggle is drawn: ticked when `checked`, and dimmed and
/// inert when not `enabled`.
#[derive(Debug, Clone, Copy)]
struct ToggleState {
    checked: bool,
    enabled: bool,
}

/// `control`'s checkbox row, ticked when its state says so and ringed when
/// it has the keyboard.
fn toggle(
    control: SettingsControl,
    state: ToggleState,
    label: impl Into<SharedString>,
    focused: Option<SettingsControl>,
    cx: &mut Context<RootView>,
) -> Div {
    let row = checkbox_row(
        crate::fonts::ui_text_size(cx),
        crate::fonts::ui_weight_shift(cx),
        control.selector(),
        state.checked,
        focused == Some(control),
        label.into(),
        &theme(cx),
    );
    if state.enabled {
        div().flex().child(row.on_click(press(control, cx)))
    } else {
        div()
            .flex()
            .opacity(DISABLED_OPACITY)
            .child(row.cursor_default())
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
fn intern(selector: String) -> &'static str {
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
/// only with `keep_awake`, once the daemon reported its state.
fn general_controls(keep_awake: bool) -> Vec<SettingsControl> {
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
            SettingsControl::CheckUpdates,
            SettingsControl::CheckNow,
        ])
        .collect()
}

/// The controls of `view`, field `index` of the page of the agent tagged
/// `tag`, in drawing order: one per choice of a choice field, one for a
/// toggle, none for a kind the page does not draw.
fn field_controls(tag: &str, index: usize, view: &FieldView) -> Vec<SettingsControl> {
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
/// subscriptions: typing keeps the draft, and Enter or leaving the input
/// applies it to `host`.
fn new_setting_text(
    host: &HostKey,
    agent: &AgentId,
    seed: TextSeed,
    window: &mut Window,
    cx: &mut Context<RootView>,
) -> (SettingText, Vec<Subscription>) {
    let input = cx.new(|cx| TextInput::new(seed.draft.clone(), seed.placeholder.clone(), cx));
    let focus = input.read(cx).focus_handle(cx);
    let (submit_host, submitter, submitted) = (host.clone(), agent.clone(), seed.field.clone());
    let keys = cx.subscribe_in(
        &input,
        window,
        move |this, _, event: &TextInputEvent, window, cx| match event {
            TextInputEvent::Submit => {
                this.apply_setting_text(&submit_host, &submitter, &submitted);
                cx.notify();
            }
            TextInputEvent::Cancel => this.close_appearance_editor(window, cx),
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
    let (leave_host, leaver, left) = (host.clone(), agent.clone(), seed.field.clone());
    let blur = cx.on_blur(&focus, window, move |this, _, cx| {
        this.apply_setting_text(&leave_host, &leaver, &left);
        cx.notify();
    });
    let text = SettingText {
        field: seed.field,
        input,
        focus,
        draft: seed.draft,
        edited: false,
        placeholder: seed.placeholder,
    };
    (text, vec![keys, edits, blur])
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
/// field is not disabled; a field trusted launch locks carries the locked
/// note.
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
        FieldEditor::Unknown => {
            let selector = format!("settings-{tag}-{}", field_part(&view.id));
            field(
                crate::fonts::ui_text_size(cx),
                crate::fonts::ui_weight_shift(cx),
                view.label.clone(),
                &theme(cx),
            )
            .child(
                div()
                    .driver_id(move || selector)
                    .opacity(DISABLED_OPACITY)
                    .child(muted(UPDATE_CLIENT, &theme(cx))),
            )
        }
    };
    let locked = view.disabled && view.editor != FieldEditor::Unknown;
    let note = locked.then(|| {
        let selector = format!("settings-{tag}-{}-locked", field_part(&view.id));
        hint(crate::fonts::ui_text_size(cx), LOCKED, &t).driver_id(move || selector)
    });
    Some(
        row.children(
            view.help
                .clone()
                .map(|help| hint(crate::fonts::ui_text_size(cx), help, &t)),
        )
        .children(note)
        .into_any_element(),
    )
}

/// A text field's frame round `input`; a click gives it the keyboard
/// unless the field takes no input.
fn input_frame(
    control: SettingsControl,
    input: &Entity<TextInput>,
    editable: bool,
    focused: Option<SettingsControl>,
    cx: &mut Context<RootView>,
) -> Div {
    let frame = field_frame(focused == Some(control), &theme(cx))
        .driver_id(|| control.selector().to_owned())
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
    let frame = field_frame(false, t)
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
enum RowLock {
    /// The choices take clicks and keys.
    Live,
    /// The choices are dimmed and inert; the note and its selector say why.
    Locked {
        note: &'static str,
        selector: &'static str,
    },
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

/// A labelled row of segmented choices, laid out as the spawn dialog's;
/// unless `lock` is [`RowLock::Live`] the choices are dimmed, inert and out
/// of the ring.
fn choice_row(
    label: &'static str,
    choices: &[(SettingsControl, &'static str, bool)],
    lock: RowLock,
    focused: Option<SettingsControl>,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = theme(cx);
    let enabled = matches!(lock, RowLock::Live);
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
        RowLock::Live => None,
    };
    field(
        crate::fonts::ui_text_size(cx),
        crate::fonts::ui_weight_shift(cx),
        label,
        &theme(cx),
    )
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
    use std::collections::VecDeque;

    use gpui::{AppContext as _, TestAppContext};
    use protocol::{AgentDescriptor, AgentId, OptionValue, SpawnPlacement};

    use super::{
        AgentTab, AgentTabs, HostAgentTabs, SettingsTab, control_matches, intern, row_tabs,
        settings_host_after_close, tab_kept, tab_rows,
    };
    use crate::agent_options::{FieldEditor, FieldView};
    use crate::target::HostKey;
    use crate::text_input::TextInput;

    const FIXED: [&str; 7] = [
        "General",
        "Notifications",
        "Spawn defaults",
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
        tab_rows(agents, remote)
            .into_iter()
            .map(|row| row.label(agents))
            .collect()
    }

    fn agent_selectors(agents: &[AgentDescriptor]) -> Vec<&'static str> {
        row_tabs(&tab_rows(agents, true))
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
            !row_tabs(&tab_rows(&agents, true))
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
            tab_kept(shown, &tab_rows(&[claude(), test_agent()], true)),
            shown
        );
        assert_eq!(
            tab_kept(shown, &tab_rows(&[claude()], true)),
            SettingsTab::General,
            "the daemon no longer lists the agent"
        );
        assert_eq!(
            tab_kept(shown, &tab_rows(&[], true)),
            SettingsTab::General,
            "a disconnected client lists no agent"
        );
        assert_eq!(
            tab_kept(SettingsTab::RemoteAccess, &tab_rows(&[claude()], false)),
            SettingsTab::General
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
