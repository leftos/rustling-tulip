//! The Settings modal: a tab list on the left and the chosen tab's rows
//! beside it. General holds keep-awake and copy on select, Notifications
//! the Needs you toggle and Windows' toast state, Appearance
//! the appearance editor at the app level, App title the window title's
//! settings; every change applies at once. The main window's title, which
//! the App title tab controls, is kept here too.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, EntityId, FocusHandle, Focusable as _,
    FontWeight, Keystroke, MouseButton, MouseDownEvent, SharedString, Stateful, Subscription,
    Window, div, prelude::*, px,
};
use protocol::{
    ClaudeProvider, ClientMessage, DaemonMessage, FEATURE_CLAUDE_PROVIDER, PermissionMode,
    RemoteState,
};

use crate::appearance;
use crate::appearance_stops::{AppearanceStop, after_lost};
use crate::appearance_view::{Level, close_footer};
use crate::buttons::{field_frame, focus_ring};
use crate::mouse::CopyOnSelect;
use crate::notify::{NotifyState, WINDOWS_NOTIFICATION_SETTINGS};
use crate::palette::{CHIP, HOVER, LINE, RAISED, SUBTLE, TEXT_2, TRANSPARENT};
use crate::remote_access::{
    PortError, RemoteStatusView, address, apply_port_message, autostart_failure_line,
    autostart_read_failure_line, cert_line, controls_enabled, parse_port, state_line,
    toggle_message,
};
use crate::section::section;
use crate::session_menu::{backdrop, dialog_button};
use crate::sidebar::{LeafDensity, NotifySkip, SidebarView};
use crate::spawn_form::{
    APPROVAL_CHOICES, PROVIDER_CHOICES, PROVIDER_UNSUPPORTED_TIP, approval_label, provider_label,
};
use crate::spawn_view::{
    CLAUDE_LOCKED, DISABLED_OPACITY, Look, checkbox_row, choice_button, close_button, dialog_card,
    dialog_title_bar, field, segmented,
};
use crate::tabs::tab_session_counts;
use crate::text_input::{TextChanged, TextInput, TextInputEvent};
use crate::window_title::compute_title;
use crate::{Pick, RootView, TEXT, tooltip};

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
/// The size of a hint.
const HINT_SIZE: f32 = 12.5;
/// The corner radius of a text link, and of the title preview's chip.
const LINK_RADIUS: f32 = 4.0;
const CHIP_RADIUS: f32 = 5.0;
/// How long the window title waits for its inputs to settle, so a session
/// flickering between working and idle does not flicker the taskbar.
const TITLE_DEBOUNCE: Duration = Duration::from_millis(350);
const NOT_CONNECTED: &str = "(daemon not connected)";
/// The Claude page's locked provider note: unlike the approval mode, the
/// provider is sent under trusted launch, so the choice stays in effect.
const PROVIDER_LOCKED: &str =
    "Trusted launch keeps the provider choice in effect; this lock only stops editing it here.";
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

/// A tab of the Settings modal, in the order the list shows them.
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
    Claude,
}

impl SettingsTab {
    const ALL: [Self; 8] = [
        Self::General,
        Self::Notifications,
        Self::SpawnDefaults,
        Self::Worktrees,
        Self::RemoteAccess,
        Self::Appearance,
        Self::AppTitle,
        Self::Claude,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Notifications => "Notifications",
            Self::SpawnDefaults => "Spawn defaults",
            Self::Worktrees => "Worktrees",
            Self::RemoteAccess => "Remote access",
            Self::Appearance => "Appearance",
            Self::AppTitle => "App title",
            Self::Claude => "Claude",
        }
    }

    const fn selector(self) -> &'static str {
        match self {
            Self::General => "settings-tab-general",
            Self::Notifications => "settings-tab-notifications",
            Self::SpawnDefaults => "settings-tab-spawn-defaults",
            Self::Worktrees => "settings-tab-worktrees",
            Self::RemoteAccess => "settings-tab-remote-access",
            Self::Appearance => "settings-tab-appearance",
            Self::AppTitle => "settings-tab-app-title",
            Self::Claude => "settings-tab-claude",
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
    const fn label(self) -> &'static str {
        match self {
            Self::Group(label) => label,
            Self::Tab(tab) => tab.label(),
        }
    }
}

/// The tab list's rows, in order. A group row is drawn between the tabs
/// and never reached by the keyboard.
const ROWS: [SettingsRow; 9] = [
    SettingsRow::Tab(SettingsTab::General),
    SettingsRow::Tab(SettingsTab::Notifications),
    SettingsRow::Tab(SettingsTab::SpawnDefaults),
    SettingsRow::Tab(SettingsTab::Worktrees),
    SettingsRow::Tab(SettingsTab::RemoteAccess),
    SettingsRow::Tab(SettingsTab::Appearance),
    SettingsRow::Tab(SettingsTab::AppTitle),
    SettingsRow::Group("Agents"),
    SettingsRow::Tab(SettingsTab::Claude),
];

/// The General tab's Default view choices, in the order they show.
const SIDEBAR_VIEW_CHOICES: [SidebarView; 2] = [SidebarView::Repos, SidebarView::Tabs];

/// The General tab's Leaf density choices, in the order they show.
const LEAF_DENSITY_CHOICES: [LeafDensity; 2] = [LeafDensity::Comfortable, LeafDensity::Compact];

/// The Notifications tab's skip choices, in the order they show.
const NOTIFY_SKIP_CHOICES: [NotifySkip; 3] =
    [NotifySkip::OnScreen, NotifySkip::Focused, NotifySkip::Never];

/// A control of a Settings tab that Tab can reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsControl {
    KeepAwake,
    SidebarView(SidebarView),
    LeafDensity(LeafDensity),
    OpenTextExternally,
    CopyOnSelect,
    TitleCount,
    TitleSuffix,
    NotifySettingsLink,
    NotifyNeedsYou,
    NotifyFinished,
    NotifySkip(NotifySkip),
    SpawnTrusted,
    SpawnApproval(Option<PermissionMode>),
    SpawnProvider(ClaudeProvider),
    WorktreesPath,
    WorktreesBrowse,
    WorktreesSave,
    WorktreesReset,
    WorktreesManage,
    RemoteToggle,
    RemotePort,
    RemotePortApply,
    RemoteAutostart,
    Appearance(AppearanceStop),
}

impl SettingsControl {
    const fn selector(self) -> &'static str {
        match self {
            Self::KeepAwake => "settings-keep-awake-toggle",
            Self::SidebarView(SidebarView::Repos) => "settings-sidebar-view-repos",
            Self::SidebarView(SidebarView::Tabs) => "settings-sidebar-view-tabs",
            Self::LeafDensity(LeafDensity::Comfortable) => "settings-leaf-density-comfortable",
            Self::LeafDensity(LeafDensity::Compact) => "settings-leaf-density-compact",
            Self::OpenTextExternally => "settings-general-open-text-externally",
            Self::CopyOnSelect => "settings-terminal-copy-on-selection",
            Self::TitleCount => "settings-title-busy-count",
            Self::TitleSuffix => "settings-title-product-suffix",
            Self::NotifySettingsLink => "settings-open-windows-notifications",
            Self::NotifyNeedsYou => "settings-notify-needs-you",
            Self::NotifyFinished => "settings-notify-finished",
            Self::NotifySkip(NotifySkip::OnScreen) => "settings-notify-skip-on-screen",
            Self::NotifySkip(NotifySkip::Focused) => "settings-notify-skip-focused",
            Self::NotifySkip(NotifySkip::Never) => "settings-notify-skip-never",
            Self::SpawnTrusted => "settings-spawn-trusted",
            Self::SpawnApproval(mode) => match mode {
                None => "settings-spawn-approval-cli-default",
                Some(PermissionMode::Default) => "settings-spawn-approval-default",
                Some(PermissionMode::AcceptEdits) => "settings-spawn-approval-accept-edits",
                Some(PermissionMode::BypassPermissions) => {
                    "settings-spawn-approval-bypass-permissions"
                }
                Some(PermissionMode::Plan) => "settings-spawn-approval-plan",
            },
            Self::SpawnProvider(provider) => match provider {
                ClaudeProvider::Anthropic => "settings-spawn-provider-anthropic",
                ClaudeProvider::Deepseek => "settings-spawn-provider-deepseek",
                ClaudeProvider::Unknown => "settings-spawn-provider-unknown",
            },
            Self::WorktreesPath => "settings-worktrees-root-input",
            Self::WorktreesBrowse => "settings-worktrees-root-browse",
            Self::WorktreesSave => "settings-worktrees-root-save",
            Self::WorktreesReset => "settings-worktrees-root-reset",
            Self::WorktreesManage => "settings-worktrees-open-manager",
            Self::RemoteToggle => "settings-remote-toggle",
            Self::RemotePort => "settings-remote-port-input",
            Self::RemotePortApply => "settings-remote-port-apply",
            Self::RemoteAutostart => "settings-remote-autostart-toggle",
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

/// The daemon's keep-awake setting and whether it holds the machine awake
/// now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeepAwake {
    pub(crate) enabled: bool,
    pub(crate) active: bool,
}

/// What the keep-awake button offers.
const fn keep_awake_label(state: Option<KeepAwake>) -> &'static str {
    match state {
        Some(KeepAwake { enabled: true, .. }) => "Allow sleep while sessions run",
        _ => "Keep this machine awake while sessions run",
    }
}

/// The line beside the keep-awake button.
const fn keep_awake_status(state: Option<KeepAwake>) -> &'static str {
    match state {
        None => "(daemon not connected)",
        Some(KeepAwake {
            enabled: true,
            active: true,
        }) => "Holding the machine awake — a session is live.",
        Some(KeepAwake {
            enabled: true,
            active: false,
        }) => "Sleep is allowed until a session starts.",
        Some(KeepAwake { enabled: false, .. }) => "The machine may sleep while sessions run.",
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

    /// Why the Claude page's "Claude provider" row is disabled, while
    /// Settings is open and it is.
    #[must_use]
    pub fn settings_provider_tip(&self) -> Option<&'static str> {
        (self.settings_open() && !self.claude_provider_supported())
            .then_some(PROVIDER_UNSUPPORTED_TIP)
    }

    /// The label of the "Claude provider" choice shown selected.
    #[must_use]
    pub fn settings_provider_choice(&self) -> &'static str {
        provider_label(self.shown_default_provider())
    }

    /// The note under the Claude page's locked provider row, as it draws it.
    #[must_use]
    pub fn settings_provider_locked_note(&self) -> &'static str {
        PROVIDER_LOCKED
    }

    /// The "Claude provider" choice shown selected: the saved default, or
    /// Anthropic while the daemon cannot route elsewhere, as the spawn
    /// dialog shows. The saved default is kept for a capable daemon.
    fn shown_default_provider(&self) -> ClaudeProvider {
        if self.claude_provider_supported() {
            self.sidebar.ui_state().spawn.claude_provider
        } else {
            ClaudeProvider::Anthropic
        }
    }

    /// The Settings tabs' labels, in the list's order.
    #[must_use]
    pub fn settings_tab_labels(&self) -> Vec<&'static str> {
        self.settings_tabs()
            .into_iter()
            .map(SettingsTab::label)
            .collect()
    }

    /// Every label the Settings tab list draws, group rows included.
    #[must_use]
    pub fn settings_tab_rows(&self) -> Vec<&'static str> {
        self.settings_rows()
            .into_iter()
            .map(SettingsRow::label)
            .collect()
    }

    /// Whether the connection target offers `tab`: Remote access configures
    /// this PC's daemon, so it is offered only where the client manages it.
    fn offers_settings_tab(&self, tab: SettingsTab) -> bool {
        tab != SettingsTab::RemoteAccess || self.capabilities().manages_daemon()
    }

    /// The tabs the list shows on this target, in order.
    fn settings_tabs(&self) -> Vec<SettingsTab> {
        SettingsTab::ALL
            .into_iter()
            .filter(|tab| self.offers_settings_tab(*tab))
            .collect()
    }

    /// The rows the tab list draws on this target, in order.
    fn settings_rows(&self) -> Vec<SettingsRow> {
        ROWS.into_iter()
            .filter(|row| match row {
                SettingsRow::Group(_) => true,
                SettingsRow::Tab(tab) => self.offers_settings_tab(*tab),
            })
            .collect()
    }

    /// After the connection target changed: a shown tab the new target
    /// does not offer gives way to General.
    pub(crate) fn follow_settings_target(&mut self) {
        if !self.offers_settings_tab(self.settings_tab) {
            self.settings_tab = SettingsTab::General;
            self.settings_control = None;
        }
    }

    /// The label of the Settings tab shown.
    #[must_use]
    pub fn settings_tab(&self) -> &'static str {
        self.settings_tab.label()
    }

    /// The keep-awake button's label and whether it can be pressed.
    #[must_use]
    pub fn keep_awake_button(&self) -> (&'static str, bool) {
        (keep_awake_label(self.keep_awake), self.keep_awake.is_some())
    }

    /// The keep-awake status line.
    #[must_use]
    pub fn keep_awake_status(&self) -> &'static str {
        keep_awake_status(self.keep_awake)
    }

    /// The App title tab's preview: the title for a tab named "Tab name"
    /// with one of three terminals busy.
    #[must_use]
    pub fn title_preview(&self) -> String {
        let title = &self.sidebar.ui_state().title;
        let preview = compute_title(
            Some((1, 3)),
            Some("Tab name"),
            title.show_count,
            title.suffix,
        );
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
            SettingsTab::General => {
                let keep_awake = self.keep_awake.map(|_| SettingsControl::KeepAwake);
                keep_awake
                    .into_iter()
                    .chain(SIDEBAR_VIEW_CHOICES.map(SettingsControl::SidebarView))
                    .chain(LEAF_DENSITY_CHOICES.map(SettingsControl::LeafDensity))
                    .chain([
                        SettingsControl::OpenTextExternally,
                        SettingsControl::CopyOnSelect,
                    ])
                    .collect()
            }
            SettingsTab::AppTitle => {
                vec![SettingsControl::TitleCount, SettingsControl::TitleSuffix]
            }
            SettingsTab::Notifications => [
                SettingsControl::NotifySettingsLink,
                SettingsControl::NotifyNeedsYou,
                SettingsControl::NotifyFinished,
            ]
            .into_iter()
            .chain(NOTIFY_SKIP_CHOICES.map(SettingsControl::NotifySkip))
            .collect(),
            SettingsTab::SpawnDefaults => vec![SettingsControl::SpawnTrusted],
            SettingsTab::Claude => self.claude_controls(),
            SettingsTab::Worktrees => {
                let mut controls = vec![SettingsControl::WorktreesPath];
                if self.capabilities().opens_local_files() {
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

    /// The Claude page's controls in drawing order, while trusted launch
    /// has not locked them; the provider choices leave while the daemon
    /// cannot route to another provider.
    fn claude_controls(&self) -> Vec<SettingsControl> {
        if self.sidebar.ui_state().spawn.trusted {
            return Vec::new();
        }
        let mut controls = APPROVAL_CHOICES
            .map(SettingsControl::SpawnApproval)
            .to_vec();
        if self.claude_provider_supported() {
            controls.extend(PROVIDER_CHOICES.map(SettingsControl::SpawnProvider));
        }
        controls
    }

    /// Whether the connected daemon routes Claude to a chosen provider.
    fn claude_provider_supported(&self) -> bool {
        self.conn.supports(FEATURE_CLAUDE_PROVIDER)
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
        let field = match control {
            SettingsControl::WorktreesPath => self.worktrees_tab.as_ref().map(|tab| &tab.focus),
            SettingsControl::RemotePort => self.remote_access_tab.as_ref().map(|tab| &tab.focus),
            _ => None,
        };
        match field {
            Some(focus) => focus.focus(window),
            None => self.appearance_focus.focus(window),
        }
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

    /// Whether the Worktrees tab's path field has the keyboard.
    pub(crate) fn worktrees_path_focused(&self, window: &Window) -> bool {
        self.settings_open()
            && self.settings_tab == SettingsTab::Worktrees
            && self
                .worktrees_tab
                .as_ref()
                .is_some_and(|tab| tab.focus.is_focused(window))
    }

    /// Moves the keyboard one step along the ring the tab list and the
    /// shown tab's controls make, backwards with `back`. From anywhere off
    /// the ring it goes to the tab list.
    fn step_settings_focus(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
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
            SettingsControl::KeepAwake => self.toggle_keep_awake(),
            SettingsControl::SidebarView(view) => self.set_sidebar_view(view, cx),
            SettingsControl::LeafDensity(density) => self.set_leaf_density(density, cx),
            SettingsControl::OpenTextExternally => self.toggle_open_text_externally(cx),
            SettingsControl::CopyOnSelect => self.toggle_copy_on_select(cx),
            SettingsControl::TitleCount => self.toggle_title_count(window, cx),
            SettingsControl::TitleSuffix => self.toggle_title_suffix(window, cx),
            SettingsControl::NotifySettingsLink => self.open_windows_notification_settings(cx),
            SettingsControl::NotifyNeedsYou => self.toggle_notification(cx),
            SettingsControl::NotifyFinished => self.toggle_finished_notification(cx),
            SettingsControl::NotifySkip(skip) => self.set_notify_skip(skip, cx),
            SettingsControl::SpawnTrusted => self.toggle_spawn_trusted(cx),
            SettingsControl::SpawnApproval(mode) => self.set_spawn_permission_mode(mode, cx),
            SettingsControl::SpawnProvider(provider) => {
                self.set_spawn_claude_provider(provider, cx);
            }
            SettingsControl::WorktreesPath | SettingsControl::RemotePort => {}
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

    /// The worktrees root the user set, if they set one.
    fn worktrees_override(&self) -> Option<&str> {
        match &self.worktrees_root {
            Some((root, true)) => Some(root),
            _ => None,
        }
    }

    /// Whether the path field differs from the override (empty when none).
    fn worktrees_save_enabled(&self) -> bool {
        self.worktrees_tab
            .as_ref()
            .is_some_and(|tab| tab.draft.trim() != self.worktrees_override().unwrap_or(""))
    }

    /// The Worktrees tab's line on the root in use.
    #[must_use]
    pub fn worktrees_active_line(&self) -> String {
        match &self.worktrees_root {
            Some((root, true)) => format!("Active: {root} — user override"),
            Some((root, false)) => format!("Active: {root} — default (env or platform fallback)"),
            None => format!("Active: {NOT_CONNECTED} — default (env or platform fallback)"),
        }
    }

    /// The Save button's label and whether it can be pressed.
    #[must_use]
    pub fn worktrees_save_button(&self) -> (&'static str, bool) {
        let enabled = self.worktrees_save_enabled();
        let saved = self.worktrees_tab.as_ref().is_some_and(|tab| tab.saved);
        (if saved && !enabled { "Saved" } else { "Save" }, enabled)
    }

    /// Whether Reset to default can be pressed.
    #[must_use]
    pub fn worktrees_reset_enabled(&self) -> bool {
        self.worktrees_override().is_some()
    }

    /// The Worktrees tab's path field, while the tab has been shown.
    #[must_use]
    pub fn worktrees_path(&self) -> Option<String> {
        self.worktrees_tab.as_ref().map(|tab| tab.draft.clone())
    }

    /// The path field afresh, holding the override.
    fn new_worktrees_tab(&self, window: &mut Window, cx: &mut Context<Self>) -> WorktreesTab {
        let draft = self.worktrees_override().unwrap_or("").to_owned();
        let placeholder = self
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
        let edits = cx.subscribe_in(&input, window, |this, input, _: &TextChanged, _, cx| {
            let text = input.read(cx).text().to_owned();
            if let Some(tab) = &mut this.worktrees_tab {
                tab.draft = text;
            }
            cx.notify();
        });
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
        let Some(tab) = &mut self.worktrees_tab else {
            return;
        };
        let trimmed = tab.draft.trim();
        let path = (!trimmed.is_empty()).then(|| trimmed.to_owned());
        tab.awaiting += 1;
        tab.saved = false;
        tracing::info!(?path, "settings: saving the worktrees root");
        self.send(ClientMessage::SetWorktreesRoot { path });
    }

    fn reset_worktrees_root(&mut self, cx: &mut Context<Self>) {
        if self.worktrees_override().is_none() {
            return;
        }
        if let Some(tab) = &mut self.worktrees_tab {
            tab.awaiting += 1;
            tab.saved = false;
            tab.draft.clear();
            tab.input.update(cx, |input, cx| input.set_text("", cx));
        }
        tracing::info!("settings: resetting the worktrees root");
        self.send(ClientMessage::SetWorktreesRoot { path: None });
    }

    /// Browse: the folder picked fills the path field, if the field is
    /// still the one that asked.
    fn browse_worktrees_root(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = &self.worktrees_tab else {
            return;
        };
        let asked = tab.input.entity_id();
        // The picker opens at the path in the field, else at the root the
        // daemon uses.
        let current = tab.input.read(cx).text().trim().to_owned();
        let start = if current.is_empty() {
            self.worktrees_root
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
        let Some(tab) = &mut self.worktrees_tab else {
            return;
        };
        if tab.input.entity_id() != asked {
            return;
        }
        tab.draft.clone_from(&path);
        tab.input.update(cx, |input, cx| input.set_text(path, cx));
        cx.notify();
    }

    /// The daemon's worktrees root. Each echo answers one of this tab's
    /// sends, and the one answering the last shows `Saved` and puts the
    /// override the daemon settled on in the field. An echo answering none
    /// is another client's change: it fills the field only while the field
    /// still reads as the daemon left it.
    pub(crate) fn on_worktrees_root_changed(
        &mut self,
        root: &str,
        is_override: bool,
        cx: &mut Context<Self>,
    ) {
        let previous = self
            .worktrees_root
            .as_ref()
            .filter(|(_, was_overridden)| *was_overridden)
            .map_or_else(String::new, |(root, _)| root.clone());
        self.worktrees_root = Some((root.to_owned(), is_override));
        if let Some(tab) = &mut self.worktrees_tab {
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

    /// Forgets the root and the tab's field, as a new connection does.
    pub(crate) fn reset_worktrees_tab(&mut self) {
        self.worktrees_root = None;
        self.worktrees_tab = None;
    }

    /// The daemon's last `RemoteStatus`, or `None` when this connection has
    /// not reported one. A port field that is empty or still shows the old
    /// port takes the new one.
    pub(crate) fn set_remote_status(
        &mut self,
        status: Option<RemoteStatusView>,
        cx: &mut Context<Self>,
    ) {
        let old = self.remote_status.as_ref().map(|status| status.port);
        let new = status.as_ref().map(|status| status.port);
        self.remote_status = status;
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

    /// The daemon's `RemoteStatus`; any other message is left alone.
    pub(crate) fn on_remote_status(&mut self, msg: DaemonMessage, cx: &mut Context<Self>) {
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
            self.set_remote_status(Some(status), cx);
        }
    }

    /// The Remote access page's line on what remote access is doing.
    #[must_use]
    pub fn remote_status_line(&self) -> String {
        self.remote_status.as_ref().map_or_else(
            || NOT_CONNECTED.to_owned(),
            |status| state_line(&status.state, status.port),
        )
    }

    /// The remote access switch: whether it is ticked, and whether it can
    /// be pressed.
    #[must_use]
    pub fn remote_toggle(&self) -> (bool, bool) {
        let status = self.remote_status.as_ref();
        (
            status.is_some_and(|status| status.enabled),
            controls_enabled(status),
        )
    }

    /// The address other devices reach this PC at, while it serves.
    #[must_use]
    pub fn remote_address(&self) -> Option<String> {
        self.remote_status
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
        }) = &self.remote_status
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
        let status = self.remote_status.as_ref()?;
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
        if controls_enabled(self.remote_status.as_ref()) {
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
        if let Some(status) = &self.remote_status {
            tracing::info!(
                enabled = !status.enabled,
                "settings: switching remote access"
            );
            self.send(toggle_message(status));
        }
    }

    fn apply_remote_port(&self) {
        if let Some(message) = self.remote_apply_message() {
            tracing::info!("settings: applying the remote access port");
            self.send(message);
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
            self.settings_tab = SettingsTab::General;
            self.settings_control = None;
            self.settings_tabs_focus.focus(window);
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
        if tab == SettingsTab::Notifications {
            self.read_notify_state(window, cx);
        }
        if tab == SettingsTab::Worktrees {
            self.worktrees_tab = Some(self.new_worktrees_tab(window, cx));
        }
        if tab == SettingsTab::RemoteAccess {
            self.remote_access_tab = Some(self.new_remote_access_tab(window, cx));
        }
        self.settings_tabs_focus.focus(window);
        cx.notify();
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

    fn set_spawn_permission_mode(&mut self, mode: Option<PermissionMode>, cx: &mut Context<Self>) {
        self.sidebar.set_spawn_permission_mode(mode);
        self.save_ui();
        cx.notify();
    }

    /// Saves the provider a Claude spawn pre-selects; nothing while the
    /// daemon cannot route to another provider.
    fn set_spawn_claude_provider(&mut self, provider: ClaudeProvider, cx: &mut Context<Self>) {
        if !self.claude_provider_supported() {
            return;
        }
        self.sidebar.set_spawn_claude_provider(provider);
        self.save_ui();
        cx.notify();
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
            || self.appearance_field_focused(window, cx)
    }

    /// The daemon's keep-awake state, or `None` when this connection has
    /// not reported it.
    pub(crate) fn set_keep_awake(&mut self, state: Option<KeepAwake>) {
        self.keep_awake = state;
    }

    fn toggle_keep_awake(&mut self) {
        if let Some(state) = self.keep_awake {
            self.send(ClientMessage::SetKeepAwake {
                enabled: !state.enabled,
            });
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

    fn toggle_copy_on_select(&mut self, cx: &mut Context<Self>) {
        let on = !self.sidebar.ui_state().general.copy_on_select;
        self.sidebar.set_copy_on_select(on);
        cx.set_global(CopyOnSelect(on));
        self.save_ui();
        cx.notify();
    }

    fn toggle_open_text_externally(&mut self, cx: &mut Context<Self>) {
        let on = !self.sidebar.ui_state().general.open_text_externally;
        self.sidebar.set_open_text_externally(on);
        self.save_ui();
        cx.notify();
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
        cx.notify();
    }

    /// The title the window should show now: the active tab's name and
    /// counts as the App title settings shape them.
    fn wanted_window_title(&self) -> String {
        let title = &self.sidebar.ui_state().title;
        let tab = self.tabs.active_tab();
        let counts = tab.and_then(|tab| tab_session_counts(tab, self.sidebar.sessions()));
        compute_title(
            counts,
            tab.map(|tab| tab.name.as_str()),
            title.show_count,
            title.suffix,
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
        let close = close_button("settings-close", false).on_click(cx.listener(
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
                    .id("settings-content")
                    .debug_selector(|| "settings-content".to_owned())
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .w(px(CONTENT_WIDTH + 2.0 * CONTENT_PAD_X))
                    .px(px(CONTENT_PAD_X))
                    .py(px(CONTENT_PAD_Y))
                    .children(self.settings_content(window, cx)),
            );
        let panel = dialog_card("settings-panel", PANEL_WIDTH)
            .max_h(window.viewport_size().height - px(2.0 * PANEL_MARGIN_Y))
            .track_focus(&self.appearance_focus)
            .child(dialog_title_bar("Settings", close))
            .child(body)
            .child(close_footer("settings-footer-close", false, cx));
        Some(backdrop("settings", panel))
    }

    /// The tab list on the raised ground; the shown tab is filled and bold,
    /// and ringed while the list has the keyboard. A group row is a label
    /// only: no id, no click, no hover and no ring.
    fn settings_tab_list(&self, window: &Window, cx: &mut Context<Self>) -> Stateful<Div> {
        let list_focused = self.settings_tabs_focus.is_focused(window);
        let tabs: Vec<AnyElement> = self
            .settings_rows()
            .into_iter()
            .map(|row| match row {
                SettingsRow::Group(label) => group_row(label).into_any_element(),
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
            .bg(gpui::rgb(RAISED))
            .border_r_1()
            .border_color(gpui::rgb(LINE))
            .children(tabs)
    }

    /// One clickable tab row; the shown one is filled, bold, and ringed
    /// while the list has the keyboard.
    fn settings_tab_row(
        &self,
        tab: SettingsTab,
        list_focused: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let shown = tab == self.settings_tab;
        let selector = tab.selector();
        let (text, weight) = if shown {
            (TEXT, FontWeight::SEMIBOLD)
        } else {
            (TEXT_2, FontWeight::MEDIUM)
        };
        div()
            .id(selector)
            .debug_selector(|| selector.to_owned())
            .flex()
            .flex_none()
            .items_center()
            .h(px(TAB_ROW_HEIGHT))
            .px(px(12.0))
            .rounded(px(TAB_ROW_RADIUS))
            .border_1()
            .border_color(gpui::rgba(TRANSPARENT))
            .font_weight(weight)
            .text_color(gpui::rgb(text))
            .cursor_pointer()
            .when(shown, |row| row.bg(gpui::rgb(CHIP)))
            .when(!shown, |row| {
                row.hover(|style| style.bg(gpui::rgb(HOVER)).text_color(gpui::rgb(TEXT)))
            })
            .when(shown && list_focused, |row| {
                row.child(focus_ring(TAB_ROW_RADIUS))
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.select_settings_tab(tab, window, cx);
            }))
            .child(tab.label())
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
            SettingsTab::Claude => self.claude_tab(focused, cx),
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
        let Some(tab) = &self.worktrees_tab else {
            return Vec::new();
        };
        let path = SettingsControl::WorktreesPath;
        let field = field_frame(focused == Some(path))
            .debug_selector(|| path.selector().to_owned())
            .flex_1()
            .min_w(px(0.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.press_settings_control(path, window, cx);
                }),
            )
            .child(tab.input.clone());
        let refusal = self.capabilities().local_files_refusal();
        let browse = settings_button(
            SettingsControl::WorktreesBrowse,
            "Browse…",
            refusal.is_none(),
            focused,
            cx,
        )
        .when_some(refusal, |button, tip| button.tooltip(tooltip(tip)));
        let path_row = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child("Path")
            .child(field)
            .child(browse);
        let active = div()
            .debug_selector(|| "settings-worktrees-active".to_owned())
            .text_color(gpui::rgb(TEXT_2))
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
        let section = section("Worktrees root")
            .child(hint(WORKTREES_HINT))
            .child(path_row)
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
            .debug_selector(|| "settings-remote-status".to_owned())
            .text_color(gpui::rgb(TEXT_2))
            .child(self.remote_status_line());
        let address = self.remote_address().map(|url| {
            div()
                .debug_selector(|| "settings-remote-address".to_owned())
                .child(format!("Address: {url}"))
        });
        let cert = self
            .remote_cert_line()
            .map(|line| hint(line).debug_selector(|| "settings-remote-cert".to_owned()));
        let port_error = self
            .remote_port_error()
            .map(|error| hint(error).debug_selector(|| "settings-remote-port-error".to_owned()));
        let section = section("Remote access (Tailscale)")
            .child(hint(REMOTE_INTRO))
            .child(switch)
            .child(status)
            .children(address)
            .children(cert)
            .child(self.remote_port_row(tab, enabled, focused, cx))
            .children(port_error)
            .child(hint(REMOTE_PORT_HINT))
            .child(hint(REMOTE_RUN_HINT))
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
        let port = SettingsControl::RemotePort;
        let frame = field_frame(focused == Some(port))
            .debug_selector(|| port.selector().to_owned())
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
                .text_color(gpui::rgb(SUBTLE))
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
        let error = self
            .remote_autostart_line()
            .map(|line| hint(line).debug_selector(|| "settings-remote-autostart-error".to_owned()));
        [launch, hint(AUTOSTART_HINT)]
            .into_iter()
            .chain(error)
            .collect()
    }

    /// The Spawn defaults tab: trusted launch, which locks every agent row
    /// on its own page while it is on.
    fn spawn_defaults_tab(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
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
        let section = section("Spawn defaults")
            .child(hint(
                "Pre-fill these in the spawn dialog. You can override on every spawn.",
            ))
            .child(trusted)
            .child(hint(
                "When enabled, new Claude sessions bypass approval prompts.",
            ));
        vec![section.into_any_element()]
    }

    /// The Claude page: approval mode and provider.
    fn claude_tab(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let spawn = self.sidebar.ui_state().spawn;
        let locked = spawn.trusted;
        let approval = choice_row(
            "Claude approval mode",
            &APPROVAL_CHOICES.map(|mode| {
                (
                    SettingsControl::SpawnApproval(mode),
                    approval_label(mode),
                    mode == spawn.permission_mode,
                )
            }),
            locked.then_some((CLAUDE_LOCKED, "settings-spawn-approval-locked")),
            focused,
            cx,
        );
        let provider = provider_row(
            self.shown_default_provider(),
            self.claude_provider_supported(),
            locked,
            focused,
            cx,
        );
        let section = section("Claude").child(approval).child(provider);
        vec![section.into_any_element()]
    }

    fn notifications_tab(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let link = SettingsControl::NotifySettingsLink;
        let selector = link.selector();
        let open = div()
            .id(selector)
            .debug_selector(|| selector.to_owned())
            .px(px(4.0))
            .rounded(px(LINK_RADIUS))
            .border_1()
            .border_color(gpui::rgba(TRANSPARENT))
            .text_color(gpui::rgb(appearance::BUILTIN_ACCENT))
            .cursor_pointer()
            .hover(|style| style.bg(gpui::rgb(HOVER)))
            .on_click(press(link, cx))
            .when(focused == Some(link), |open| {
                open.child(focus_ring(LINK_RADIUS))
            })
            .child("Open Windows notification settings");
        let state = div()
            .debug_selector(|| "settings-notify-state".to_owned())
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
        let skip_row = choice_row("Skip notifications when", &skips, None, focused, cx);
        let section = section("Notifications")
            .child(windows)
            .child(hint(
                "Fire an OS notification when a session needs you (a question, \
                 a permission prompt or a plan approval) or finishes its turn \
                 with no background work left.",
            ))
            .child(needs_you)
            .child(finished)
            .child(skip_row);
        vec![section.into_any_element()]
    }

    fn general_tab(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let (label, enabled) = self.keep_awake_button();
        let keep_awake = SettingsControl::KeepAwake;
        let button = dialog_button(
            keep_awake.selector(),
            label.to_owned(),
            false,
            focused == Some(keep_awake),
            enabled,
        );
        let button = if enabled {
            button.on_click(press(keep_awake, cx))
        } else {
            button
        };
        let status = hint(self.keep_awake_status())
            .debug_selector(|| "settings-keep-awake-status".to_owned());
        let power = section("Power").child(
            div()
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(button)
                .child(status),
        );
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
        let sidebar = section("Sidebar")
            .child(choice_row("Default view", &choices, None, focused, cx))
            .child(choice_row("Leaf density", &densities, None, focused, cx));
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
        let terminal = section("Terminal behavior").child(copy).child(hint(
            "Off: select to highlight, copy explicitly with Ctrl+C \
             (when there's a selection) or Ctrl+Shift+C.",
        ));
        let open_text = toggle(
            SettingsControl::OpenTextExternally,
            ToggleState {
                checked: self.sidebar.ui_state().general.open_text_externally,
                enabled: true,
            },
            "Open text files in the external app",
            focused,
            cx,
        );
        let files = section("Files").child(open_text);
        vec![
            power.into_any_element(),
            sidebar.into_any_element(),
            files.into_any_element(),
            terminal.into_any_element(),
        ]
    }

    fn app_title_tab(
        &self,
        focused: Option<SettingsControl>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let title = &self.sidebar.ui_state().title;
        let count = toggle(
            SettingsControl::TitleCount,
            ToggleState {
                checked: title.show_count,
                enabled: true,
            },
            "Show busy/total terminal count",
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
                .debug_selector(|| "settings-title-preview".to_owned())
                .px(px(8.0))
                .py(px(2.0))
                .rounded(px(CHIP_RADIUS))
                .bg(gpui::rgb(CHIP))
                .child(self.title_preview()),
        );
        let section = section("App title")
            .child(hint(
                "Controls what the OS window title bar shows for the main window.",
            ))
            .child(count)
            .child(hint(
                "Renders (M/N) before the tab name where M is the number of non-idle \
                 terminals in the active tab and N is the total terminal count. Hidden \
                 when the tab has no terminals.",
            ))
            .child(suffix)
            .child(hint(
                "Keeps OS taskbars grouping rustling-tulip windows together. Turn off \
                 for a tighter title.",
            ))
            .child(preview);
        vec![section.into_any_element()]
    }
}

/// A group label in the tab list: painted, never a tab stop.
fn group_row(label: &'static str) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .h(px(TAB_ROW_HEIGHT))
        .px(px(12.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(gpui::rgb(SUBTLE))
        .child(label)
}

/// A quiet line under or beside a control.
fn hint(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(HINT_SIZE))
        .text_color(gpui::rgb(SUBTLE))
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
        false,
        focused == Some(control),
        enabled,
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
    label: &'static str,
    focused: Option<SettingsControl>,
    cx: &mut Context<RootView>,
) -> Div {
    let row = checkbox_row(
        control.selector(),
        state.checked,
        focused == Some(control),
        label,
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

/// The "Claude provider" row; without daemon support its choices are dimmed,
/// inert and out of the ring, and a tooltip says how to get it.
fn provider_row(
    chosen: ClaudeProvider,
    supported: bool,
    locked: bool,
    focused: Option<SettingsControl>,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let enabled = supported && !locked;
    let buttons = PROVIDER_CHOICES
        .into_iter()
        .map(|provider| {
            let control = SettingsControl::SpawnProvider(provider);
            let look = Look {
                selected: provider == chosen,
                focused: focused == Some(control),
                enabled,
            };
            choice_button(
                control.selector().to_owned(),
                provider_label(provider),
                look,
                press(control, cx),
            )
            .into_any_element()
        })
        .collect();
    let row = div()
        .id("settings-spawn-provider")
        .flex()
        .child(segmented(buttons));
    let row = if supported {
        row
    } else {
        row.tooltip(tooltip(PROVIDER_UNSUPPORTED_TIP))
    };
    let row = field("Claude provider").child(row);
    let row = if locked {
        row.child(
            hint(PROVIDER_LOCKED).debug_selector(|| "settings-spawn-provider-locked".to_owned()),
        )
    } else {
        row
    };
    row.into_any_element()
}

/// A labelled row of segmented choices, laid out as the spawn dialog's;
/// with a locked note the choices are dimmed, inert and out of the ring,
/// and the note and its selector say why.
fn choice_row(
    label: &'static str,
    choices: &[(SettingsControl, &'static str, bool)],
    locked_note: Option<(&'static str, &'static str)>,
    focused: Option<SettingsControl>,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let buttons = choices
        .iter()
        .map(|(control, text, selected)| {
            let look = Look {
                selected: *selected,
                focused: focused == Some(*control),
                enabled: locked_note.is_none(),
            };
            choice_button(
                control.selector().to_owned(),
                *text,
                look,
                press(*control, cx),
            )
            .into_any_element()
        })
        .collect();
    let note =
        locked_note.map(|(text, selector)| hint(text).debug_selector(move || selector.to_owned()));
    field(label)
        .child(div().flex().child(segmented(buttons)))
        .children(note)
        .into_any_element()
}
