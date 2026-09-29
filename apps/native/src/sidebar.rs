//! The sidebar's model: the repos view's containers and session leaves built
//! from the daemon's registry, the attention set, and the persisted sidebar
//! layout. Plain Rust, so every rule is unit-tested; `sidebar_view` renders it.

use protocol::{
    AppearanceOverrides, AttentionReason, CodexSandbox, ContainerRef, DaemonMessage,
    PermissionMode, RepoEntry, SessionKind, SessionMode, SessionSnapshot, SessionStatus, TabEntry,
    WorkspaceEntry,
};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use crate::appearance::{self, AppColors, AppLevel, Resolved};
use crate::fonts::{self, FontSettings};
use crate::source_control::{Part, ScKey, ScUiState};
use crate::tabs::collect_panes;
use crate::window_state::WindowState;

/// The sidebar layout file, in the client's config dir.
pub const UI_FILE: &str = "native-ui.json";
pub const MIN_WIDTH: f32 = 200.0;
pub const DEFAULT_WIDTH: f32 = 280.0;

/// What a container row groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerKind {
    Workspace,
    Repo,
    /// One standalone shell outside every registered repo.
    Shell,
    /// Plain shells sharing an unregistered working directory.
    Dir,
    /// Sessions whose repo or workspace is no longer registered.
    Detached,
    /// The sessions a daemon tab's panes show, in the tabs view.
    Tab,
    /// Live sessions no tab's pane shows, in the tabs view.
    Unbound,
}

impl ContainerKind {
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Workspace => "WS",
            Self::Repo => "REPO",
            Self::Shell => "SH",
            Self::Dir => "DIR",
            Self::Detached => "Detached",
            Self::Tab => "TAB",
            Self::Unbound => "UNB",
        }
    }
}

/// The Unbound container's hover text.
pub const UNBOUND_HOVER: &str = "Sessions alive but not referenced by any tab. \
     Click the unbound pill on a session to open it in a new tab.";

/// How the sessions panel groups its sessions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SidebarView {
    /// By daemon tab, then the sessions no tab shows.
    Tabs,
    /// By workspace, repo, shell and folder; also what a value this build
    /// does not know loads as (`serde(other)` must be the last variant).
    #[default]
    #[serde(other)]
    Repos,
}

/// One session row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leaf {
    pub id: String,
    pub status: SessionStatus,
    pub mode: SessionMode,
    pub label: String,
    pub runtime: Option<String>,
    pub attention: bool,
    /// The agent's last turn ended while this client looked elsewhere.
    pub unseen: bool,
    pub state: LeafState,
    /// Launched with approval prompts bypassed.
    pub trusted: bool,
    /// The label's hover text.
    pub tooltip: String,
}

/// What a leaf tags its session as besides its runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeafState {
    Live,
    /// Reattached after a daemon restart with its PTY lost.
    Orphan,
    /// Left behind by a daemon crash, with the prompt it was running.
    Abandoned {
        last_prompt: Option<String>,
    },
    /// Parked, with the worktrees it kept.
    Inactive {
        worktree_paths: Vec<String>,
    },
}

impl LeafState {
    /// A parked session is parked whatever else it is; an abandoned one
    /// is not also an orphan.
    fn of(s: &SessionSnapshot) -> Self {
        if s.is_inactive {
            Self::Inactive {
                worktree_paths: s.worktree_paths.clone(),
            }
        } else if s.is_abandoned {
            Self::Abandoned {
                last_prompt: non_empty(s.last_prompt.as_deref()).map(str::to_owned),
            }
        } else if s.is_orphan {
            Self::Orphan
        } else {
            Self::Live
        }
    }

    /// The state's tag and its hover text; `None` for a live session.
    fn tag(&self) -> Option<(String, String)> {
        let (tag, tip) = match self {
            Self::Live => return None,
            Self::Orphan => (
                "orphan",
                "Reattached after daemon restart; PTY detached".to_owned(),
            ),
            Self::Abandoned { last_prompt } => (
                "abandoned",
                last_prompt.as_ref().map_or_else(
                    || "Daemon crashed mid-run".to_owned(),
                    |prompt| format!("Daemon crashed mid-run. Last prompt:\n{prompt}"),
                ),
            ),
            Self::Inactive { worktree_paths } if worktree_paths.is_empty() => {
                ("inactive", "Parked".to_owned())
            }
            Self::Inactive { worktree_paths } => (
                "inactive",
                format!(
                    "Parked. Worktree kept on disk:\n{}",
                    worktree_paths.join("\n")
                ),
            ),
        };
        Some((tag.to_owned(), tip))
    }
}

impl Leaf {
    /// The row's tags as text and hover text: the runtime, then the
    /// orphan, abandoned or parked state.
    #[must_use]
    pub fn tags(&self) -> Vec<(String, String)> {
        let mut tags = Vec::new();
        if let Some(runtime) = &self.runtime {
            let tip = if self.trusted {
                format!("Running {runtime}; approval prompts were bypassed")
            } else {
                format!("Running {runtime}")
            };
            tags.push((runtime.clone(), tip));
        }
        tags.extend(self.state.tag());
        tags
    }
}

/// One container row and its sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    /// Stable key, also the persisted collapsed-container entry.
    pub key: String,
    /// The id a manual session order is stored under.
    pub id: String,
    pub kind: ContainerKind,
    pub name: String,
    /// The row's hover text, when it has one.
    pub hover: Option<String>,
    pub leaves: Vec<Leaf>,
    /// Any leaf has attention.
    pub attention: bool,
    pub collapsed: bool,
}

/// Everything the container tree is built from.
pub struct TreeInputs<'a> {
    pub repos: &'a [RepoEntry],
    pub workspaces: &'a [WorkspaceEntry],
    pub sessions: &'a [SessionSnapshot],
    pub container_order: &'a [ContainerRef],
    /// Manual session order per container id.
    pub session_order: &'a HashMap<String, Vec<String>>,
    pub attention: &'a HashSet<String>,
    /// Agent sessions whose finished turn this client has not seen.
    pub unseen: &'a HashSet<String>,
    pub collapsed: &'a BTreeSet<String>,
}

/// Where a working directory belongs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CwdHome {
    Workspace(String),
    Repo(String),
    /// No registered repo contains it; keyed by the normalised path.
    Dir(String),
}

/// Which panel the activity rail shows beside it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    SourceControl,
    /// The sessions waiting on the user.
    NeedsYou,
    /// The sessions panel; also what a value this build does not know loads
    /// as, so a newer build's panel keeps the rest of the file (`serde(other)`
    /// must be the last variant).
    #[default]
    #[serde(other)]
    Sessions,
}

/// The persisted sidebar layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiState {
    pub sidebar_width: f32,
    pub sidebar_collapsed: bool,
    /// The panel the rail last showed.
    #[serde(default)]
    pub activity: Activity,
    /// How the sessions panel groups its sessions.
    #[serde(default)]
    pub sidebar_view: SidebarView,
    pub collapsed_containers: BTreeSet<String>,
    /// The tab shown when the client last ran, restored when the daemon
    /// sends the tab list.
    pub active_tab_id: Option<String>,
    /// The folder "+ Shell" opens in, when the user picked one.
    #[serde(default)]
    pub quick_shell_dir: Option<String>,
    /// The font every new terminal pane starts from.
    #[serde(default)]
    pub terminal_font: FontSettings,
    /// Terminal font sizes overridden per tab, by tab id; a tab's size wins
    /// over its session's, its container's and the app's.
    #[serde(default)]
    pub tab_font_sizes: BTreeMap<String, f32>,
    /// The source-control panel's pinned repo and collapsed sections.
    #[serde(default)]
    pub source_control: ScUiState,
    /// The app level's colours, below every repo's, workspace's and
    /// session's.
    #[serde(default)]
    pub app_appearance: AppColors,
    /// The custom colours chosen lately, newest first, `#rrggbb`.
    #[serde(default)]
    pub recent_colors: Vec<String>,
    /// Whether the diff tabs show whitespace-only changes.
    #[serde(default = "include_whitespace_by_default")]
    pub diff_include_whitespace: bool,
    /// Whether the diff tabs colour code by the file's language.
    #[serde(default = "highlight_by_default")]
    pub diff_highlight: bool,
    /// The main window's place when it last moved; `None` until it has.
    #[serde(default)]
    pub window: Option<WindowState>,
    /// The Settings modal's General tab.
    #[serde(default)]
    pub general: GeneralSettings,
    /// The Settings modal's App title tab.
    #[serde(default)]
    pub title: TitleSettings,
    /// Which attention reasons fire an OS notification.
    #[serde(default)]
    pub notifications: NotificationSettings,
    /// The Settings modal's Spawn defaults tab.
    #[serde(default)]
    pub spawn: SpawnDefaults,
}

/// What the spawn dialog pre-fills from the Spawn defaults tab; every
/// field off is the `claude` / `codex` CLI's own default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpawnDefaults {
    /// New Claude, Codex and Cursor sessions bypass approval prompts; Codex
    /// and Cursor also bypass sandboxing.
    pub trusted: bool,
    /// Claude's `--permission-mode` pre-fill.
    pub permission_mode: Option<PermissionMode>,
    /// Codex's `--sandbox` pre-fill.
    pub codex_sandbox: Option<CodexSandbox>,
}

/// Which attention reasons fire an OS notification; each is on unless
/// turned off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationSettings {
    pub awaiting_input: bool,
    pub stopped: bool,
    pub error: bool,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            awaiting_input: true,
            stopped: true,
            error: true,
        }
    }
}

impl NotificationSettings {
    /// Whether `reason` fires a notification.
    #[must_use]
    pub const fn fires(&self, reason: AttentionReason) -> bool {
        match reason {
            AttentionReason::AwaitingInput => self.awaiting_input,
            AttentionReason::Stopped => self.stopped,
            AttentionReason::Error => self.error,
        }
    }

    fn set(&mut self, reason: AttentionReason, on: bool) {
        match reason {
            AttentionReason::AwaitingInput => self.awaiting_input = on,
            AttentionReason::Stopped => self.stopped = on,
            AttentionReason::Error => self.error = on,
        }
    }
}

/// The General settings saved on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralSettings {
    /// Whether a mouse selection in a terminal is copied on release.
    pub copy_on_select: bool,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            copy_on_select: true,
        }
    }
}

/// What the main window's title shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TitleSettings {
    /// The active tab's busy/total terminal count before its name.
    pub show_count: bool,
    /// ` — rustling-tulip` after it.
    pub suffix: bool,
}

impl Default for TitleSettings {
    fn default() -> Self {
        Self {
            show_count: true,
            suffix: true,
        }
    }
}

/// A layout saved before the diff tabs' whitespace toggle existed shows
/// whitespace changes.
fn include_whitespace_by_default() -> bool {
    true
}

/// A layout saved before the diff tabs' highlight toggle existed colours
/// the diffs.
fn highlight_by_default() -> bool {
    true
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            sidebar_width: DEFAULT_WIDTH,
            sidebar_collapsed: false,
            activity: Activity::Sessions,
            sidebar_view: SidebarView::Repos,
            collapsed_containers: BTreeSet::new(),
            active_tab_id: None,
            quick_shell_dir: None,
            terminal_font: FontSettings::default(),
            tab_font_sizes: BTreeMap::new(),
            source_control: ScUiState::default(),
            app_appearance: AppColors::default(),
            recent_colors: Vec::new(),
            diff_include_whitespace: true,
            diff_highlight: true,
            window: None,
            general: GeneralSettings::default(),
            title: TitleSettings::default(),
            notifications: NotificationSettings::default(),
            spawn: SpawnDefaults::default(),
        }
    }
}

/// The daemon's registry as the sidebar sees it, plus the attention set and
/// the layout.
#[derive(Debug, Default)]
pub struct SidebarModel {
    repos: Vec<RepoEntry>,
    repos_loaded: bool,
    workspaces: Vec<WorkspaceEntry>,
    sessions: Vec<SessionSnapshot>,
    container_order: Vec<ContainerRef>,
    session_order: HashMap<String, Vec<String>>,
    attention: HashSet<String>,
    /// Agent sessions whose turn ended while this client showed another
    /// session focused, until the user looks at them.
    unseen: HashSet<String>,
    /// The session this client shows focused, as the root last reported it
    /// before folding a message in.
    focused: Option<String>,
    ui: UiState,
}

impl SidebarModel {
    pub fn new(ui: UiState) -> Self {
        Self {
            ui,
            ..Self::default()
        }
    }

    /// Fold a daemon message into the model; messages it does not track are
    /// ignored.
    pub fn apply(&mut self, msg: &DaemonMessage) {
        match msg {
            DaemonMessage::Repos { repos } => {
                repos.clone_into(&mut self.repos);
                self.repos_loaded = true;
            }
            DaemonMessage::Workspaces { workspaces } => workspaces.clone_into(&mut self.workspaces),
            DaemonMessage::ContainersReordered { ordered } => {
                ordered.clone_into(&mut self.container_order);
            }
            DaemonMessage::SessionsReordered {
                container_id,
                ordered_ids,
            } => {
                self.session_order
                    .insert(container_id.clone(), ordered_ids.clone());
            }
            _ => self.apply_session_message(msg),
        }
    }

    fn apply_session_message(&mut self, msg: &DaemonMessage) {
        match msg {
            DaemonMessage::Sessions { sessions } => {
                for session in sessions {
                    let before = self.session(&session.id).map(|s| s.status);
                    self.note_turn(before, session);
                }
                self.unseen
                    .retain(|id| sessions.iter().any(|s| &s.id == id));
                sessions.clone_into(&mut self.sessions);
            }
            DaemonMessage::SessionUpdated { session, .. } => self.update_session(session),
            DaemonMessage::SessionRemoved { session_id } => {
                self.sessions.retain(|s| &s.id != session_id);
                self.attention.remove(session_id);
                self.unseen.remove(session_id);
            }
            DaemonMessage::Attention { session_id, .. } => {
                self.attention.insert(session_id.clone());
            }
            _ => {}
        }
    }

    /// Replace or append the session; a session that settled back into
    /// working, idle or spawning on its own no longer needs attention.
    fn update_session(&mut self, session: &SessionSnapshot) {
        let before = self.session(&session.id).map(|s| s.status);
        self.note_turn(before, session);
        match self.sessions.iter_mut().find(|s| s.id == session.id) {
            Some(existing) => existing.clone_from(session),
            None => self.sessions.push(session.clone()),
        }
        if matches!(
            session.status,
            SessionStatus::Working | SessionStatus::Idle | SessionStatus::Spawning
        ) {
            self.attention.remove(&session.id);
        }
    }

    /// An agent session going from working to idle while another session
    /// is shown focused joins the unseen set; any status but idle leaves
    /// it, so the set holds only idle sessions.
    fn note_turn(&mut self, before: Option<SessionStatus>, session: &SessionSnapshot) {
        match session.status {
            SessionStatus::Idle
                if before == Some(SessionStatus::Working)
                    && session.mode != SessionMode::PlainShell
                    && self.focused.as_deref() != Some(session.id.as_str()) =>
            {
                self.unseen.insert(session.id.clone());
            }
            SessionStatus::Idle => {}
            _ => {
                self.unseen.remove(&session.id);
            }
        }
    }

    /// Records the session this client shows focused, so a turn that ends
    /// there is not marked unseen.
    pub fn set_focused_session(&mut self, id: Option<&str>) {
        self.focused = id.map(str::to_owned);
    }

    /// The user looked at the session: its finished turn is seen.
    pub fn mark_seen(&mut self, id: &str) {
        self.unseen.remove(id);
    }

    /// Whether the session's last turn ended unseen.
    pub fn is_unseen(&self, id: &str) -> bool {
        self.unseen.contains(id)
    }

    pub fn sessions(&self) -> &[SessionSnapshot] {
        &self.sessions
    }

    /// The registered repos, in the daemon's order.
    pub fn repos(&self) -> &[RepoEntry] {
        &self.repos
    }

    /// Whether the daemon's repo list has arrived.
    pub fn repos_loaded(&self) -> bool {
        self.repos_loaded
    }

    /// The registered workspaces, in the daemon's order.
    pub fn workspaces(&self) -> &[WorkspaceEntry] {
        &self.workspaces
    }

    pub fn session(&self, id: &str) -> Option<&SessionSnapshot> {
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn containers(&self) -> Vec<Container> {
        build_containers(&TreeInputs {
            repos: &self.repos,
            workspaces: &self.workspaces,
            sessions: &self.sessions,
            container_order: &self.container_order,
            session_order: &self.session_order,
            attention: &self.attention,
            unseen: &self.unseen,
            collapsed: &self.ui.collapsed_containers,
        })
    }

    /// The tabs view's containers for `tabs`, in the daemon's tab order.
    pub fn tab_containers(&self, tabs: &[TabEntry]) -> Vec<Container> {
        build_tab_containers(
            tabs,
            &self.sessions,
            &self.attention,
            &self.unseen,
            &self.ui.collapsed_containers,
        )
    }

    /// How the sessions panel groups its sessions.
    pub fn sidebar_view(&self) -> SidebarView {
        self.ui.sidebar_view
    }

    /// Records how the sessions panel groups its sessions; returns whether
    /// it changed.
    pub fn set_sidebar_view(&mut self, view: SidebarView) -> bool {
        if self.ui.sidebar_view == view {
            return false;
        }
        self.ui.sidebar_view = view;
        true
    }

    /// Drops the folds of the tabs that are not in `live`; returns whether
    /// any went.
    pub fn prune_tab_folds(&mut self, live: &HashSet<&str>) -> bool {
        let collapsed = &mut self.ui.collapsed_containers;
        let before = collapsed.len();
        collapsed.retain(|key| key.strip_prefix("tab:").is_none_or(|id| live.contains(id)));
        collapsed.len() != before
    }

    pub fn clear_attention(&mut self, id: &str) {
        self.attention.remove(id);
    }

    pub fn toggle_container(&mut self, key: &str) {
        let collapsed = &mut self.ui.collapsed_containers;
        if !collapsed.remove(key) {
            collapsed.insert(key.to_owned());
        }
    }

    pub fn toggle_sidebar(&mut self) {
        self.ui.sidebar_collapsed = !self.ui.sidebar_collapsed;
    }

    pub fn is_collapsed(&self) -> bool {
        self.ui.sidebar_collapsed
    }

    /// The panel the rail shows.
    pub fn activity(&self) -> Activity {
        self.ui.activity
    }

    /// A rail click on `item`: the active item folds or unfolds the panel;
    /// the other one takes its place, unfolding a folded panel.
    pub fn click_activity(&mut self, item: Activity) {
        if self.ui.activity == item {
            self.ui.sidebar_collapsed = !self.ui.sidebar_collapsed;
        } else {
            self.ui.activity = item;
            self.ui.sidebar_collapsed = false;
        }
    }

    /// The source-control panel's persisted state.
    pub fn source_control(&self) -> &ScUiState {
        &self.ui.source_control
    }

    /// Pins the source-control panel to `repo_id`, or back to following the
    /// active pane; returns whether the pin moved.
    pub fn set_pinned_repo(&mut self, repo_id: Option<String>) -> bool {
        let pinned = &mut self.ui.source_control.pinned_repo;
        if *pinned == repo_id {
            return false;
        }
        *pinned = repo_id;
        true
    }

    /// Records whether a part of a source-control section is collapsed;
    /// returns whether the stored value changed.
    pub fn set_sc_collapsed(&mut self, key: &ScKey, part: Part, collapsed: bool) -> bool {
        self.ui.source_control.set_collapsed(key, part, collapsed)
    }

    /// Drops the source-control pin and collapse entries of repos not in
    /// `repos`; returns whether any went.
    pub fn prune_source_control(&mut self, repos: &[RepoEntry]) -> bool {
        self.ui.source_control.prune(repos)
    }

    /// Sets the source-control changes area's height, already clamped.
    pub fn set_sc_changes_height(&mut self, height: f32) {
        self.ui.source_control.changes_height = Some(height);
    }

    /// Sets a section's commit-list height, already clamped, by section key
    /// id.
    pub fn set_sc_history_list_height(&mut self, key_id: &str, height: f32) {
        self.ui
            .source_control
            .history_list_height
            .insert(key_id.to_owned(), height);
    }

    /// The sidebar width to lay out in a window this wide.
    pub fn width(&self, window_width: f32) -> f32 {
        clamp_width(self.ui.sidebar_width, window_width)
    }

    pub fn set_width(&mut self, width: f32, window_width: f32) {
        self.ui.sidebar_width = clamp_width(width, window_width);
    }

    pub fn ui_state(&self) -> &UiState {
        &self.ui
    }

    pub fn set_terminal_font(&mut self, font: FontSettings) {
        self.ui.terminal_font = font;
    }

    /// Whether the diff tabs show whitespace-only changes.
    pub fn set_diff_include_whitespace(&mut self, include: bool) {
        self.ui.diff_include_whitespace = include;
    }

    /// Whether the diff tabs colour code by the file's language.
    pub fn set_diff_highlight(&mut self, highlight: bool) {
        self.ui.diff_highlight = highlight;
    }

    /// Whether a terminal selection is copied on release.
    pub fn set_copy_on_select(&mut self, on: bool) {
        self.ui.general.copy_on_select = on;
    }

    /// Whether the window title shows the active tab's busy/total count.
    pub fn set_title_show_count(&mut self, on: bool) {
        self.ui.title.show_count = on;
    }

    /// Whether the window title ends with the product name.
    pub fn set_title_suffix(&mut self, on: bool) {
        self.ui.title.suffix = on;
    }

    /// Whether `reason` fires an OS notification.
    pub fn set_notification(&mut self, reason: AttentionReason, on: bool) {
        self.ui.notifications.set(reason, on);
    }

    /// Whether a spawn dialog opens with trusted launch on.
    pub fn set_spawn_trusted(&mut self, on: bool) {
        self.ui.spawn.trusted = on;
    }

    /// The approval mode a spawn dialog opens with for Claude.
    pub fn set_spawn_permission_mode(&mut self, mode: Option<PermissionMode>) {
        self.ui.spawn.permission_mode = mode;
    }

    /// The sandbox mode a spawn dialog opens with for Codex.
    pub fn set_spawn_codex_sandbox(&mut self, sandbox: Option<CodexSandbox>) {
        self.ui.spawn.codex_sandbox = sandbox;
    }

    /// Records the main window's place; returns whether it changed.
    pub fn set_window_state(&mut self, state: WindowState) -> bool {
        if self.ui.window.as_ref() == Some(&state) {
            return false;
        }
        self.ui.window = Some(state);
        true
    }

    /// The size stored for `tab_id`, if the tab has an override.
    pub fn tab_font_size(&self, tab_id: &str) -> Option<f32> {
        self.ui.tab_font_sizes.get(tab_id).copied()
    }

    /// Records `size` as `tab_id`'s override.
    pub fn set_tab_font_size(&mut self, tab_id: &str, size: f32) {
        self.ui
            .tab_font_sizes
            .insert(tab_id.to_owned(), fonts::clamp_size(size));
    }

    /// Drops `tab_id`'s override; returns whether there was one.
    pub fn clear_tab_font_size(&mut self, tab_id: &str) -> bool {
        self.ui.tab_font_sizes.remove(tab_id).is_some()
    }

    /// Drops the overrides of the tabs that are not in `live`; returns
    /// whether any went.
    pub fn prune_tab_font_sizes(&mut self, live: &HashSet<&str>) -> bool {
        let before = self.ui.tab_font_sizes.len();
        self.ui
            .tab_font_sizes
            .retain(|tab_id, _| live.contains(tab_id.as_str()));
        self.ui.tab_font_sizes.len() != before
    }

    /// The size a pane of `session_id` in a tab overridden to `tab_size`
    /// draws at: the tab's override, else the session's size, else its
    /// container's, else the app's.
    pub fn resolved_font_size(&self, tab_size: Option<f32>, session_id: Option<&str>) -> f32 {
        self.appearance(session_id)
            .with_tab_size(tab_size)
            .font_size
            .value
    }

    /// Every appearance field of `session_id` (or of a pane showing none)
    /// as it resolves through the session, its container and the app.
    pub fn appearance(&self, session_id: Option<&str>) -> Resolved {
        self.resolve(session_id.and_then(|id| self.session(id)))
    }

    /// Every appearance field of `session_id` as it resolves when its own
    /// overrides are `own` rather than those it holds; `None` for a
    /// session the list does not hold.
    pub fn appearance_with(&self, session_id: &str, own: &AppearanceOverrides) -> Option<Resolved> {
        let mut session = self.session(session_id)?.clone();
        session.appearance.clone_from(own);
        Some(self.resolve(Some(&session)))
    }

    /// Each session's resolved accent, `0xRRGGBB`, by session id.
    pub fn session_accents(&self) -> HashMap<&str, u32> {
        self.sessions
            .iter()
            .map(|session| {
                (
                    session.id.as_str(),
                    self.resolve(Some(session)).accent.value,
                )
            })
            .collect()
    }

    fn resolve(&self, session: Option<&SessionSnapshot>) -> Resolved {
        appearance::resolve(
            session,
            &self.repos,
            &self.workspaces,
            AppLevel {
                colors: &self.ui.app_appearance,
                font: &self.ui.terminal_font,
            },
        )
    }

    pub fn set_app_colors(&mut self, colors: AppColors) {
        self.ui.app_appearance = colors;
    }

    /// The recent custom colours to offer, `0xRRGGBB`, newest first.
    pub fn recent_colors(&self) -> Vec<u32> {
        appearance::recent_swatches(&self.ui.recent_colors)
    }

    /// Puts `color` first among the recent custom colours; returns whether
    /// it went in.
    pub fn push_recent_color(&mut self, color: &str) -> bool {
        appearance::push_recent(&mut self.ui.recent_colors, color)
    }

    /// Records the active tab; returns whether it changed.
    pub fn set_active_tab(&mut self, tab_id: Option<&str>) -> bool {
        if self.ui.active_tab_id.as_deref() == tab_id {
            return false;
        }
        self.ui.active_tab_id = tab_id.map(str::to_owned);
        true
    }

    /// The folder a quick shell opens in, when the user picked one.
    pub fn quick_shell_dir(&self) -> Option<&str> {
        self.ui.quick_shell_dir.as_deref()
    }

    /// Records the quick shell's folder; returns whether it changed, so the
    /// caller knows whether there is anything to save.
    pub fn set_quick_shell_dir(&mut self, dir: Option<&str>) -> bool {
        if self.ui.quick_shell_dir.as_deref() == dir {
            return false;
        }
        self.ui.quick_shell_dir = dir.map(str::to_owned);
        true
    }
}

/// The sidebar width kept between the minimum and the window width less the
/// terminal's minimum; a non-finite width falls back to the default.
pub fn clamp_width(width: f32, window_width: f32) -> f32 {
    let width = if width.is_finite() {
        width
    } else {
        DEFAULT_WIDTH
    };
    let max = (window_width - MIN_WIDTH).max(MIN_WIDTH);
    width.clamp(MIN_WIDTH, max)
}

/// Sessions grouped by the container that will hold them.
#[derive(Default)]
struct Groups<'a> {
    workspaces: HashMap<&'a str, Vec<&'a SessionSnapshot>>,
    repos: HashMap<&'a str, Vec<&'a SessionSnapshot>>,
    dirs: BTreeMap<String, Vec<&'a SessionSnapshot>>,
    shells: Vec<&'a SessionSnapshot>,
    detached: Vec<&'a SessionSnapshot>,
}

impl<'a> Groups<'a> {
    fn place(&mut self, s: &'a SessionSnapshot, inputs: &TreeInputs<'a>, members: &HashSet<&str>) {
        if s.mode == SessionMode::PlainShell
            && let Some(cwd) = s.current_cwd.as_deref().filter(|cwd| !cwd.is_empty())
        {
            self.place_by_cwd(s, cwd, inputs, members);
        } else if s.kind == SessionKind::Standalone {
            self.shells.push(s);
        } else {
            self.place_by_owner(s, inputs, members);
        }
    }

    fn place_by_cwd(
        &mut self,
        s: &'a SessionSnapshot,
        cwd: &str,
        inputs: &TreeInputs<'a>,
        members: &HashSet<&str>,
    ) {
        match find_container_for_cwd(cwd, inputs.repos, inputs.workspaces, members) {
            CwdHome::Workspace(id) => self.push_workspace(inputs, &id, s),
            CwdHome::Repo(id) => self.push_repo(inputs, &id, s),
            CwdHome::Dir(_) if s.kind == SessionKind::Standalone => self.shells.push(s),
            CwdHome::Dir(path) => self.dirs.entry(path).or_default().push(s),
        }
    }

    fn place_by_owner(
        &mut self,
        s: &'a SessionSnapshot,
        inputs: &TreeInputs<'a>,
        members: &HashSet<&str>,
    ) {
        if let Some(workspace_id) = s.workspace_id.as_deref() {
            self.push_workspace(inputs, workspace_id, s);
            return;
        }
        let owner = s
            .members
            .first()
            .map(|m| m.repo_id.as_str())
            .filter(|id| !members.contains(id));
        match owner {
            Some(repo_id) => self.push_repo(inputs, repo_id, s),
            None => self.detached.push(s),
        }
    }

    /// File under the registered workspace, or Detached when it is gone.
    fn push_workspace(&mut self, inputs: &TreeInputs<'a>, id: &str, s: &'a SessionSnapshot) {
        match inputs.workspaces.iter().find(|w| w.id == id) {
            Some(w) => self.workspaces.entry(w.id.as_str()).or_default().push(s),
            None => self.detached.push(s),
        }
    }

    /// File under the registered repo, or Detached when it is gone.
    fn push_repo(&mut self, inputs: &TreeInputs<'a>, id: &str, s: &'a SessionSnapshot) {
        match inputs.repos.iter().find(|r| r.id == id) {
            Some(r) => self.repos.entry(r.id.as_str()).or_default().push(s),
            None => self.detached.push(s),
        }
    }
}

/// The repos view: workspaces and repos (manual order first, the rest
/// alphabetically), then standalone shells, then unregistered directories,
/// then Detached when it holds anything.
pub fn build_containers(inputs: &TreeInputs<'_>) -> Vec<Container> {
    let members: HashSet<&str> = inputs
        .workspaces
        .iter()
        .flat_map(|w| w.member_repo_ids.iter().map(String::as_str))
        .collect();
    let mut groups = Groups::default();
    for s in inputs.sessions {
        groups.place(s, inputs, &members);
    }
    let mut out = apply_container_order(
        workspace_containers(inputs, &mut groups),
        repo_containers(inputs, &mut groups, &members),
        inputs.container_order,
    );
    let mut shells: Vec<Container> = groups
        .shells
        .iter()
        .map(|s| {
            container(
                inputs,
                ContainerKind::Shell,
                &s.id,
                display_label(s),
                vec![s],
            )
        })
        .collect();
    shells.sort_by(|a, b| cmp_ci(&a.name, &b.name));
    out.append(&mut shells);
    let mut dirs: Vec<Container> = std::mem::take(&mut groups.dirs)
        .into_iter()
        .map(|(path, sessions)| {
            container(
                inputs,
                ContainerKind::Dir,
                &path,
                path_leaf_name(&path),
                sessions,
            )
        })
        .collect();
    dirs.sort_by(|a, b| cmp_ci(&a.name, &b.name));
    out.append(&mut dirs);
    if !groups.detached.is_empty() {
        let detached = std::mem::take(&mut groups.detached);
        out.push(container(
            inputs,
            ContainerKind::Detached,
            "",
            "Detached".to_owned(),
            detached,
        ));
    }
    out
}

fn workspace_containers(inputs: &TreeInputs<'_>, groups: &mut Groups<'_>) -> Vec<Container> {
    let mut workspaces: Vec<&WorkspaceEntry> = inputs.workspaces.iter().collect();
    workspaces.sort_by(|a, b| cmp_ci(&a.name, &b.name));
    workspaces
        .into_iter()
        .map(|w| {
            let sessions = groups.workspaces.remove(w.id.as_str()).unwrap_or_default();
            container(
                inputs,
                ContainerKind::Workspace,
                &w.id,
                w.name.clone(),
                sessions,
            )
        })
        .collect()
}

fn repo_containers(
    inputs: &TreeInputs<'_>,
    groups: &mut Groups<'_>,
    members: &HashSet<&str>,
) -> Vec<Container> {
    let mut repos: Vec<&RepoEntry> = inputs
        .repos
        .iter()
        .filter(|r| !members.contains(r.id.as_str()))
        .collect();
    repos.sort_by(|a, b| cmp_ci(&a.name, &b.name));
    repos
        .into_iter()
        .map(|r| {
            let sessions = groups.repos.remove(r.id.as_str()).unwrap_or_default();
            container(inputs, ContainerKind::Repo, &r.id, r.name.clone(), sessions)
        })
        .collect()
}

/// A container whose sessions are sorted by label, then by the container's
/// manual order.
fn container(
    inputs: &TreeInputs<'_>,
    kind: ContainerKind,
    id: &str,
    name: String,
    mut sessions: Vec<&SessionSnapshot>,
) -> Container {
    sessions.sort_by(|a, b| cmp_ci(&a.label, &b.label));
    let leaves: Vec<Leaf> = apply_session_order(sessions, inputs.session_order.get(id))
        .into_iter()
        .map(|s| leaf(s, inputs.attention, inputs.unseen))
        .collect();
    let key = container_key(kind, id);
    Container {
        collapsed: inputs.collapsed.contains(&key),
        attention: leaves.iter().any(|l| l.attention),
        key,
        id: id.to_owned(),
        kind,
        name,
        hover: None,
        leaves,
    }
}

fn leaf(s: &SessionSnapshot, attention: &HashSet<String>, unseen: &HashSet<String>) -> Leaf {
    Leaf {
        id: s.id.clone(),
        status: s.status,
        mode: s.mode,
        label: display_label(s),
        runtime: runtime_label(s),
        attention: attention.contains(&s.id),
        unseen: unseen.contains(&s.id),
        state: LeafState::of(s),
        trusted: s.elevated_authority,
        tooltip: label_tooltip(s),
    }
}

/// The key a container folds by.
fn container_key(kind: ContainerKind, id: &str) -> String {
    match kind {
        ContainerKind::Workspace => format!("ws:{id}"),
        ContainerKind::Repo => format!("repo:{id}"),
        ContainerKind::Shell => format!("standalone:{id}"),
        ContainerKind::Dir => format!("cwd:{id}"),
        ContainerKind::Detached => "detached".to_owned(),
        ContainerKind::Tab => format!("tab:{id}"),
        ContainerKind::Unbound => "unbound".to_owned(),
    }
}

/// The tabs view: one container per tab in the daemon's order, holding the
/// listed sessions its panes show, each once, in pane order (a diff tab's is
/// empty); then Unbound, the sessions no pane shows sorted by label, when
/// there are any.
pub fn build_tab_containers(
    tabs: &[TabEntry],
    sessions: &[SessionSnapshot],
    attention: &HashSet<String>,
    unseen: &HashSet<String>,
    collapsed: &BTreeSet<String>,
) -> Vec<Container> {
    let by_id: HashMap<&str, &SessionSnapshot> =
        sessions.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut shown: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();
    for tab in tabs {
        let mut seen: HashSet<&str> = HashSet::new();
        let leaves: Vec<Leaf> = tab
            .grid()
            .map(collect_panes)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|pane| pane.session)
            .filter(|id| seen.insert(id))
            .filter_map(|id| by_id.get(id).copied())
            .map(|s| {
                shown.insert(s.id.as_str());
                leaf(s, attention, unseen)
            })
            .collect();
        out.push(tab_view_container(
            ContainerKind::Tab,
            &tab.id,
            tab.name.clone(),
            leaves,
            collapsed,
        ));
    }
    let mut unbound: Vec<&SessionSnapshot> = sessions
        .iter()
        .filter(|s| !shown.contains(s.id.as_str()))
        .collect();
    if !unbound.is_empty() {
        unbound.sort_by(|a, b| cmp_ci(&a.label, &b.label));
        let leaves = unbound
            .into_iter()
            .map(|s| leaf(s, attention, unseen))
            .collect();
        out.push(tab_view_container(
            ContainerKind::Unbound,
            "",
            "Unbound".to_owned(),
            leaves,
            collapsed,
        ));
    }
    out
}

fn tab_view_container(
    kind: ContainerKind,
    id: &str,
    name: String,
    leaves: Vec<Leaf>,
    collapsed: &BTreeSet<String>,
) -> Container {
    let key = container_key(kind, id);
    let hover = match kind {
        ContainerKind::Unbound => UNBOUND_HOVER.to_owned(),
        _ => format!("Tab \"{name}\""),
    };
    Container {
        collapsed: collapsed.contains(&key),
        attention: leaves.iter().any(|l| l.attention),
        key,
        id: id.to_owned(),
        kind,
        name,
        hover: Some(hover),
        leaves,
    }
}

/// Sessions named in `order` first, in that order; the rest keep their
/// place after them. Ids of sessions that are gone are skipped.
fn apply_session_order<'a>(
    sessions: Vec<&'a SessionSnapshot>,
    order: Option<&Vec<String>>,
) -> Vec<&'a SessionSnapshot> {
    let Some(order) = order.filter(|o| !o.is_empty()) else {
        return sessions;
    };
    let mut out: Vec<&SessionSnapshot> = order
        .iter()
        .filter_map(|id| sessions.iter().copied().find(|s| &s.id == id))
        .collect();
    out.extend(sessions.iter().copied().filter(|s| !order.contains(&s.id)));
    out
}

/// Containers named in `order` first, in that order; unlisted workspaces
/// and then unlisted repos follow in their incoming order.
fn apply_container_order(
    workspaces: Vec<Container>,
    repos: Vec<Container>,
    order: &[ContainerRef],
) -> Vec<Container> {
    let mut workspaces: Vec<Option<Container>> = workspaces.into_iter().map(Some).collect();
    let mut repos: Vec<Option<Container>> = repos.into_iter().map(Some).collect();
    let mut out = Vec::new();
    for entry in order {
        let (pool, id) = match entry {
            ContainerRef::Workspace(id) => (&mut workspaces, id),
            ContainerRef::Repo(id) => (&mut repos, id),
        };
        if let Some(slot) = pool
            .iter_mut()
            .find(|c| c.as_ref().is_some_and(|c| &c.id == id))
        {
            out.extend(slot.take());
        }
    }
    out.extend(workspaces.into_iter().flatten());
    out.extend(repos.into_iter().flatten());
    out
}

fn cmp_ci(a: &str, b: &str) -> Ordering {
    a.to_lowercase().cmp(&b.to_lowercase())
}

/// The container a plain shell's working directory belongs to: the longest
/// registered repo path containing it, or the workspace that repo is a member
/// of, or else its own directory.
pub fn find_container_for_cwd(
    cwd: &str,
    repos: &[RepoEntry],
    workspaces: &[WorkspaceEntry],
    members: &HashSet<&str>,
) -> CwdHome {
    let best =
        repos
            .iter()
            .filter(|r| path_contains(&r.path, cwd))
            .fold(None::<&RepoEntry>, |best, r| match best {
                Some(b) if normalize_fs_path(&r.path).len() <= normalize_fs_path(&b.path).len() => {
                    Some(b)
                }
                _ => Some(r),
            });
    if let Some(repo) = best {
        if let Some(w) = workspaces
            .iter()
            .find(|w| w.member_repo_ids.contains(&repo.id))
        {
            return CwdHome::Workspace(w.id.clone());
        }
        if !members.contains(repo.id.as_str()) {
            return CwdHome::Repo(repo.id.clone());
        }
    }
    CwdHome::Dir(normalize_fs_path(cwd))
}

fn path_contains(parent: &str, child: &str) -> bool {
    let parent = normalize_fs_path(parent).to_lowercase();
    let child = normalize_fs_path(child).to_lowercase();
    if parent == child {
        return true;
    }
    let sep = if parent.contains('\\') { '\\' } else { '/' };
    child
        .strip_prefix(&parent)
        .is_some_and(|rest| rest.starts_with(sep))
}

/// Windows-looking paths (a drive prefix or any backslash) get backslashes;
/// trailing separators go, except on a root of three characters or fewer.
fn normalize_fs_path(path: &str) -> String {
    let drive = matches!(
        path.as_bytes(),
        [letter, b':', b'\\' | b'/', ..] if letter.is_ascii_alphabetic()
    );
    let windows_like = drive || path.contains('\\');
    let normalized = if windows_like {
        path.replace('/', "\\")
    } else {
        path.to_owned()
    };
    if normalized.chars().count() <= 3 {
        return normalized;
    }
    let sep = if windows_like { '\\' } else { '/' };
    normalized.trim_end_matches(sep).to_owned()
}

fn path_leaf_name(path: &str) -> String {
    let trimmed = path.trim_end_matches(['\\', '/']);
    match trimmed.rsplit(['\\', '/']).next() {
        Some(leaf) if !leaf.is_empty() => leaf.to_owned(),
        _ => path.to_owned(),
    }
}

/// The name a session shows: its user label, a plain shell's directory, a
/// meaningful terminal title, its label, its runtime, then its id.
pub fn display_label(s: &SessionSnapshot) -> String {
    let cwd_leaf = || {
        (s.mode == SessionMode::PlainShell)
            .then(|| non_empty(s.current_cwd.as_deref()))
            .flatten()
            .map(path_leaf_name)
    };
    let title = || non_empty(s.terminal_title.as_deref()).filter(|t| !is_noisy_shell_title(t));
    non_empty(s.user_label.as_deref())
        .map(str::to_owned)
        .or_else(cwd_leaf)
        .or_else(|| title().map(str::to_owned))
        .or_else(|| non_empty(Some(&s.label)).map(str::to_owned))
        .or_else(|| runtime_label(s))
        .unwrap_or_else(|| s.id.clone())
}

/// A session name's hover text: the name, then the daemon's label and the
/// terminal title where they say something else, then the working directory.
pub fn label_tooltip(s: &SessionSnapshot) -> String {
    let display = display_label(s);
    let mut lines = vec![display.clone()];
    if let Some(label) = non_empty(Some(&s.label)).filter(|l| *l != display) {
        lines.push(format!("Session: {label}"));
    }
    if let Some(title) = non_empty(s.terminal_title.as_deref()).filter(|t| *t != display) {
        lines.push(format!("Terminal title: {title}"));
    }
    if let Some(cwd) = non_empty(s.current_cwd.as_deref()) {
        lines.push(format!("Cwd: {cwd}"));
    }
    lines.join("\n")
}

/// The runtime tag: the agent for agent sessions, the program for plain
/// shells.
pub fn runtime_label(s: &SessionSnapshot) -> Option<String> {
    if s.mode != SessionMode::PlainShell {
        return Some(s.agent.as_label().to_owned());
    }
    s.program_name.clone().filter(|p| !p.is_empty())
}

/// Whether a leaf click may attach the terminal to the session: only
/// sessions with a PTY, stopped or errored ones included so their
/// scrollback can be read.
pub fn can_attach(s: &SessionSnapshot) -> bool {
    matches!(s.mode, SessionMode::Interactive | SessionMode::PlainShell)
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// A title that only names the shell executable says nothing about the
/// session.
fn is_noisy_shell_title(title: &str) -> bool {
    let normalized = title.trim().to_lowercase().replace('\\', "/");
    let basename = normalized.rsplit('/').next().unwrap_or(&normalized);
    matches!(
        basename,
        "cmd.exe"
            | "powershell.exe"
            | "pwsh.exe"
            | "bash.exe"
            | "sh.exe"
            | "bash"
            | "zsh"
            | "sh"
            | "pwsh"
    ) || matches!(
        normalized.as_str(),
        "windows powershell" | "administrator: windows powershell"
    )
}

/// The persisted layout in `dir`; the defaults when the file is missing or
/// unreadable.
pub fn load_ui_state(dir: &Path) -> UiState {
    let path = dir.join(UI_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return UiState::default(),
        Err(err) => {
            tracing::warn!(
                "reading {}: {err}; using the default sidebar layout",
                path.display()
            );
            return UiState::default();
        }
    };
    serde_json::from_str(&text).unwrap_or_else(|err| {
        tracing::warn!(
            "{} is not valid: {err}; using the default sidebar layout",
            path.display()
        );
        UiState::default()
    })
}

/// The key in the layout file listing the network hosts a terminal link may
/// open a `\\host\…` path on besides those behind a mapped drive. The user
/// edits it by hand; the client reads it afresh on each use and never writes
/// its own copy.
const UNC_HOSTS_KEY: &str = "unc_hosts";

/// The hosts listed under `unc_hosts` in `dir`'s layout file right now;
/// none when the file or the key is missing or unreadable.
pub fn load_unc_hosts(dir: &Path) -> Vec<String> {
    #[derive(Deserialize)]
    struct Listed {
        #[serde(default)]
        unc_hosts: Vec<String>,
    }
    let path = dir.join(UI_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            tracing::warn!(
                "reading {}: {err}; no network host is listed",
                path.display()
            );
            return Vec::new();
        }
    };
    serde_json::from_str::<Listed>(&text).map_or_else(
        |err| {
            tracing::warn!(
                "{} has no valid {UNC_HOSTS_KEY}: {err}; no network host is listed",
                path.display()
            );
            Vec::new()
        },
        |listed| listed.unc_hosts,
    )
}

/// Whatever `unc_hosts` the layout file at `path` holds right now, as it is.
fn file_unc_hosts(path: &Path) -> Option<serde_json::Value> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::warn!(
                "reading {}: {err}; its {UNC_HOSTS_KEY} is not kept",
                path.display()
            );
            return None;
        }
    };
    let mut file: serde_json::Value = serde_json::from_str(&text)
        .inspect_err(|err| {
            tracing::warn!(
                "{} is not valid: {err}; its {UNC_HOSTS_KEY} is not kept",
                path.display()
            );
        })
        .ok()?;
    file.as_object_mut()?.remove(UNC_HOSTS_KEY)
}

/// Write the layout to `dir` through a temporary file, so a crash mid-write
/// never leaves a torn file. The `unc_hosts` the file holds is written back
/// as it is; a file without one gets none.
pub fn save_ui_state(dir: &Path, state: &UiState) -> anyhow::Result<()> {
    use anyhow::Context as _;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(UI_FILE);
    let tmp = dir.join(format!("{UI_FILE}.tmp"));
    let mut layout = serde_json::to_value(state).context("serializing the sidebar layout")?;
    if let Some(hosts) = file_unc_hosts(&path)
        && let Some(object) = layout.as_object_mut()
    {
        object.insert(UNC_HOSTS_KEY.to_owned(), hosts);
    }
    let json = serde_json::to_vec_pretty(&layout).context("serializing the sidebar layout")?;
    std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::tabs::tests::{pane, tab};
    use protocol::{CodexSandbox, GridNode, PermissionMode, SessionMember, SplitDirection};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    fn repo(id: &str, path: &str) -> RepoEntry {
        serde_json::from_value(json!({ "id": id, "name": id, "path": path })).expect("repo fixture")
    }

    #[test]
    fn sidebar_keeps_repos_and_workspaces_in_daemon_order() {
        let mut model = SidebarModel::default();
        assert!(model.repos().is_empty() && model.workspaces().is_empty());
        model.apply(&DaemonMessage::Repos {
            repos: vec![repo("zeta", "C:/z"), repo("alpha", "C:/a")],
        });
        model.apply(&DaemonMessage::Workspaces {
            workspaces: vec![workspace("w2", &["zeta"]), workspace("w1", &["alpha"])],
        });
        let repos: Vec<&str> = model.repos().iter().map(|r| r.id.as_str()).collect();
        let workspaces: Vec<&str> = model.workspaces().iter().map(|w| w.id.as_str()).collect();
        assert_eq!(repos, ["zeta", "alpha"], "not sorted by name");
        assert_eq!(workspaces, ["w2", "w1"]);
        model.apply(&DaemonMessage::Repos { repos: Vec::new() });
        assert!(model.repos().is_empty(), "a new snapshot replaces the old");
    }

    fn workspace(id: &str, members: &[&str]) -> WorkspaceEntry {
        serde_json::from_value(json!({ "id": id, "name": id, "member_repo_ids": members }))
            .expect("workspace fixture")
    }

    fn session(id: &str) -> SessionSnapshot {
        serde_json::from_value(json!({
            "id": id,
            "label": id,
            "kind": "single",
            "members": [],
            "status": "idle",
            "mode": "interactive",
            "started_at": "2026-01-01T00:00:00Z",
            "exit_code": null,
            "metrics": { "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0, "last_activity_at": null },
            "recent_actions": [],
            "agent": "claude",
        }))
        .expect("session fixture")
    }

    fn in_repo(id: &str, repo_id: &str) -> SessionSnapshot {
        let mut s = session(id);
        s.members = vec![SessionMember {
            repo_id: repo_id.to_owned(),
            repo_name: repo_id.to_owned(),
            branch: "main".to_owned(),
            worktree_path: String::new(),
        }];
        s
    }

    fn shell(id: &str, cwd: Option<&str>, kind: SessionKind) -> SessionSnapshot {
        let mut s = session(id);
        s.mode = SessionMode::PlainShell;
        s.kind = kind;
        s.current_cwd = cwd.map(str::to_owned);
        s
    }

    struct Fixture {
        repos: Vec<RepoEntry>,
        workspaces: Vec<WorkspaceEntry>,
        sessions: Vec<SessionSnapshot>,
        container_order: Vec<ContainerRef>,
        session_order: HashMap<String, Vec<String>>,
    }

    impl Fixture {
        fn new(
            repos: Vec<RepoEntry>,
            workspaces: Vec<WorkspaceEntry>,
            sessions: Vec<SessionSnapshot>,
        ) -> Self {
            Self {
                repos,
                workspaces,
                sessions,
                container_order: Vec::new(),
                session_order: HashMap::new(),
            }
        }

        fn build(&self) -> Vec<Container> {
            build_containers(&TreeInputs {
                repos: &self.repos,
                workspaces: &self.workspaces,
                sessions: &self.sessions,
                container_order: &self.container_order,
                session_order: &self.session_order,
                attention: &HashSet::new(),
                unseen: &HashSet::new(),
                collapsed: &BTreeSet::new(),
            })
        }
    }

    fn keys(containers: &[Container]) -> Vec<&str> {
        containers.iter().map(|c| c.key.as_str()).collect()
    }

    fn leaf_ids<'a>(containers: &'a [Container], key: &str) -> Vec<&'a str> {
        containers
            .iter()
            .find(|c| c.key == key)
            .map(|c| c.leaves.iter().map(|l| l.id.as_str()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn workspace_member_repo_gets_no_container() {
        let tree = Fixture::new(
            vec![repo("r1", "D:\\src\\r1"), repo("r2", "D:\\src\\r2")],
            vec![workspace("w1", &["r1"])],
            vec![],
        )
        .build();
        assert_eq!(keys(&tree), ["ws:w1", "repo:r2"]);
    }

    #[test]
    fn plain_shell_in_repo_cwd_lands_in_repo() {
        let tree = Fixture::new(
            vec![repo("r1", "D:\\src\\r1")],
            vec![],
            vec![shell(
                "s",
                Some("D:\\src\\r1\\sub"),
                SessionKind::Standalone,
            )],
        )
        .build();
        assert_eq!(keys(&tree), ["repo:r1"]);
        assert_eq!(leaf_ids(&tree, "repo:r1"), ["s"]);
    }

    #[test]
    fn plain_shell_in_member_repo_cwd_lands_in_workspace() {
        let tree = Fixture::new(
            vec![repo("r1", "D:\\src\\r1")],
            vec![workspace("w1", &["r1"])],
            vec![shell("s", Some("D:\\src\\r1"), SessionKind::Single)],
        )
        .build();
        assert_eq!(leaf_ids(&tree, "ws:w1"), ["s"]);
    }

    #[test]
    fn longest_repo_prefix_wins() {
        let repos = [repo("outer", "D:\\src"), repo("inner", "D:\\src\\inner")];
        let home = find_container_for_cwd("D:\\src\\inner\\x", &repos, &[], &HashSet::new());
        assert_eq!(home, CwdHome::Repo("inner".to_owned()));
        let sibling = find_container_for_cwd("D:\\src\\innerx", &repos, &[], &HashSet::new());
        assert_eq!(sibling, CwdHome::Repo("outer".to_owned()));
    }

    #[test]
    fn cwd_match_ignores_case_and_separator_style() {
        let repos = [repo("r1", "D:\\Src\\R1")];
        let home = find_container_for_cwd("d:/src/r1/deep", &repos, &[], &HashSet::new());
        assert_eq!(home, CwdHome::Repo("r1".to_owned()));
    }

    #[test]
    fn unmatched_standalone_shell_gets_sh() {
        let tree = Fixture::new(
            vec![repo("r1", "D:\\src\\r1")],
            vec![],
            vec![shell("s", Some("C:\\elsewhere"), SessionKind::Standalone)],
        )
        .build();
        let sh = tree.iter().find(|c| c.key == "standalone:s").expect("SH");
        assert_eq!(sh.kind, ContainerKind::Shell);
        assert_eq!(sh.name, "elsewhere");
    }

    #[test]
    fn standalone_session_without_cwd_gets_sh() {
        let tree = Fixture::new(
            vec![],
            vec![],
            vec![shell("s", None, SessionKind::Standalone)],
        )
        .build();
        assert_eq!(keys(&tree), ["standalone:s"]);
    }

    #[test]
    fn unmatched_non_standalone_shell_gets_dir() {
        let tree = Fixture::new(
            vec![],
            vec![],
            vec![shell("s", Some("C:/elsewhere/proj/"), SessionKind::Single)],
        )
        .build();
        let dir = tree.first().expect("DIR");
        assert_eq!(dir.key, "cwd:C:\\elsewhere\\proj");
        assert_eq!(dir.kind, ContainerKind::Dir);
        assert_eq!(dir.name, "proj");
        assert_eq!(dir.id, "C:\\elsewhere\\proj");
    }

    #[test]
    fn unregistered_workspace_goes_to_detached() {
        let mut s = session("s");
        s.kind = SessionKind::Workspace;
        s.workspace_id = Some("gone".to_owned());
        let tree = Fixture::new(vec![], vec![workspace("w1", &[])], vec![s]).build();
        assert_eq!(keys(&tree), ["ws:w1", "detached"]);
        assert_eq!(leaf_ids(&tree, "detached"), ["s"]);
    }

    #[test]
    fn registered_workspace_session_lands_in_workspace() {
        let mut s = session("s");
        s.workspace_id = Some("w1".to_owned());
        let tree = Fixture::new(vec![], vec![workspace("w1", &[])], vec![s]).build();
        assert_eq!(leaf_ids(&tree, "ws:w1"), ["s"]);
    }

    #[test]
    fn unregistered_repo_goes_to_detached() {
        let tree = Fixture::new(
            vec![repo("r1", "D:\\r1")],
            vec![],
            vec![in_repo("a", "r1"), in_repo("b", "gone")],
        )
        .build();
        assert_eq!(leaf_ids(&tree, "repo:r1"), ["a"]);
        assert_eq!(leaf_ids(&tree, "detached"), ["b"]);
    }

    #[test]
    fn session_of_workspace_member_repo_goes_to_detached() {
        let tree = Fixture::new(
            vec![repo("r1", "D:\\r1")],
            vec![workspace("w1", &["r1"])],
            vec![in_repo("a", "r1")],
        )
        .build();
        assert_eq!(leaf_ids(&tree, "detached"), ["a"]);
    }

    #[test]
    fn sessions_sort_by_label_then_manual_order() {
        let mut beta = in_repo("b", "r1");
        beta.label = "beta".to_owned();
        let mut alpha = in_repo("a", "r1");
        alpha.label = "Alpha".to_owned();
        let mut gamma = in_repo("g", "r1");
        gamma.label = "gamma".to_owned();
        let mut fixture =
            Fixture::new(vec![repo("r1", "D:\\r1")], vec![], vec![beta, alpha, gamma]);
        assert_eq!(leaf_ids(&fixture.build(), "repo:r1"), ["a", "b", "g"]);
        fixture
            .session_order
            .insert("r1".to_owned(), vec!["g".to_owned(), "stale".to_owned()]);
        assert_eq!(leaf_ids(&fixture.build(), "repo:r1"), ["g", "a", "b"]);
    }

    #[test]
    fn default_container_order_is_workspaces_then_repos_alphabetically() {
        let tree = Fixture::new(
            vec![repo("rb", "D:\\rb"), repo("Ra", "D:\\ra")],
            vec![workspace("wz", &[]), workspace("wa", &[])],
            vec![],
        )
        .build();
        assert_eq!(keys(&tree), ["ws:wa", "ws:wz", "repo:Ra", "repo:rb"]);
    }

    #[test]
    fn manual_container_order_then_unlisted_appended() {
        let mut fixture = Fixture::new(
            vec![repo("ra", "D:\\ra"), repo("rb", "D:\\rb")],
            vec![workspace("wz", &[])],
            vec![],
        );
        fixture.container_order = vec![
            ContainerRef::Repo("rb".to_owned()),
            ContainerRef::Workspace("wz".to_owned()),
            ContainerRef::Repo("rb".to_owned()),
            ContainerRef::Repo("gone".to_owned()),
        ];
        assert_eq!(keys(&fixture.build()), ["repo:rb", "ws:wz", "repo:ra"]);
    }

    #[test]
    fn sh_then_dir_then_detached_come_last() {
        let tree = Fixture::new(
            vec![repo("r1", "D:\\r1")],
            vec![],
            vec![
                in_repo("gone", "missing"),
                shell("d1", Some("C:\\z\\bbb"), SessionKind::Single),
                shell("d2", Some("C:\\a\\ccc"), SessionKind::Single),
                shell("d3", Some("C:\\m\\aaa"), SessionKind::Single),
                shell("sz", Some("C:\\x\\zed"), SessionKind::Standalone),
                shell("sa", Some("C:\\x\\abc"), SessionKind::Standalone),
                in_repo("a", "r1"),
            ],
        )
        .build();
        assert_eq!(
            keys(&tree),
            [
                "repo:r1",
                "standalone:sa",
                "standalone:sz",
                "cwd:C:\\m\\aaa",
                "cwd:C:\\z\\bbb",
                "cwd:C:\\a\\ccc",
                "detached",
            ]
        );
    }

    #[test]
    fn detached_is_omitted_when_empty() {
        let tree =
            Fixture::new(vec![repo("r1", "D:\\r1")], vec![], vec![in_repo("a", "r1")]).build();
        assert!(tree.iter().all(|c| c.kind != ContainerKind::Detached));
    }

    #[test]
    fn label_prefers_user_label() {
        let mut s = shell("id", Some("C:\\work\\proj"), SessionKind::Single);
        s.user_label = Some("  mine  ".to_owned());
        assert_eq!(display_label(&s), "mine");
    }

    #[test]
    fn label_uses_cwd_leaf_for_plain_shell_only() {
        let mut s = shell("id", Some("C:\\work\\proj\\"), SessionKind::Single);
        s.terminal_title = Some("vim".to_owned());
        assert_eq!(display_label(&s), "proj");
        let mut agent = session("id");
        agent.current_cwd = Some("C:\\work\\proj".to_owned());
        assert_eq!(display_label(&agent), "id");
    }

    #[test]
    fn label_uses_terminal_title_when_not_noisy() {
        let mut s = session("id");
        s.terminal_title = Some("Fixing the bug".to_owned());
        assert_eq!(display_label(&s), "Fixing the bug");
    }

    #[test]
    fn label_skips_noisy_shell_titles() {
        for title in [
            "C:\\WINDOWS\\system32\\cmd.exe",
            "/usr/bin/bash",
            "pwsh",
            "Administrator: Windows PowerShell",
            "windows powershell",
            "zsh",
        ] {
            let mut s = session("id");
            s.label = "fallback".to_owned();
            s.terminal_title = Some(title.to_owned());
            assert_eq!(display_label(&s), "fallback", "title {title}");
        }
    }

    #[test]
    fn label_falls_back_to_session_label() {
        let mut s = session("id");
        s.label = "repo · main".to_owned();
        assert_eq!(display_label(&s), "repo · main");
    }

    #[test]
    fn label_falls_back_to_runtime_then_id() {
        let mut s = shell("id", None, SessionKind::Standalone);
        s.label = "   ".to_owned();
        s.program_name = Some("pwsh".to_owned());
        assert_eq!(display_label(&s), "pwsh");
        s.program_name = None;
        assert_eq!(display_label(&s), "id");
    }

    #[test]
    fn tooltip_is_the_label_alone_when_nothing_differs() {
        assert_eq!(label_tooltip(&session("id")), "id");
    }

    #[test]
    fn tooltip_names_the_session_label_when_it_differs() {
        let mut s = session("id");
        s.user_label = Some("mine".to_owned());
        s.label = "repo · main".to_owned();
        assert_eq!(label_tooltip(&s), "mine\nSession: repo · main");
    }

    #[test]
    fn tooltip_names_the_terminal_title_when_it_differs() {
        let mut s = session("id");
        s.label = "repo · main".to_owned();
        s.terminal_title = Some("pwsh".to_owned());
        assert_eq!(label_tooltip(&s), "repo · main\nTerminal title: pwsh");
        s.terminal_title = Some("Building".to_owned());
        assert_eq!(
            label_tooltip(&s),
            "Building\nSession: repo · main",
            "a title that is the label is not repeated"
        );
    }

    #[test]
    fn tooltip_ends_with_the_cwd() {
        let mut s = shell("id", Some("D:/src/app"), SessionKind::Standalone);
        s.terminal_title = Some("vim".to_owned());
        assert_eq!(
            label_tooltip(&s),
            "app\nSession: id\nTerminal title: vim\nCwd: D:/src/app"
        );
    }

    #[test]
    fn only_pty_sessions_can_attach() {
        assert!(can_attach(&session("interactive")));
        assert!(can_attach(&shell("shell", None, SessionKind::Standalone)));
        let mut headless = session("headless");
        headless.mode = SessionMode::Headless;
        assert!(!can_attach(&headless));
        let mut stopped = session("stopped");
        stopped.status = SessionStatus::Stopped;
        assert!(can_attach(&stopped));
    }

    #[test]
    fn runtime_tag_is_agent_or_program() {
        let mut agent = session("a");
        agent.agent = protocol::Agent::Codex;
        agent.program_name = Some("node".to_owned());
        assert_eq!(runtime_label(&agent).as_deref(), Some("codex"));
        let mut sh = shell("s", None, SessionKind::Standalone);
        assert_eq!(runtime_label(&sh), None);
        sh.program_name = Some("pwsh".to_owned());
        assert_eq!(runtime_label(&sh).as_deref(), Some("pwsh"));
    }

    fn model_with(sessions: Vec<SessionSnapshot>) -> SidebarModel {
        let mut model = SidebarModel::new(UiState::default());
        model.apply(&DaemonMessage::Repos {
            repos: vec![repo("r1", "D:\\r1")],
        });
        model.apply(&DaemonMessage::Sessions { sessions });
        model
    }

    fn attention_of(model: &SidebarModel, id: &str) -> bool {
        model
            .containers()
            .iter()
            .flat_map(|c| c.leaves.iter())
            .find(|l| l.id == id)
            .is_some_and(|l| l.attention)
    }

    fn flag(model: &mut SidebarModel, id: &str) {
        model.apply(&DaemonMessage::Attention {
            session_id: id.to_owned(),
            reason: AttentionReason::AwaitingInput,
        });
    }

    #[test]
    fn attention_message_flags_the_session() {
        let mut model = model_with(vec![in_repo("a", "r1")]);
        assert!(!attention_of(&model, "a"));
        flag(&mut model, "a");
        assert!(attention_of(&model, "a"));
    }

    #[test]
    fn calm_status_update_clears_attention() {
        for (status, cleared) in [
            (SessionStatus::Working, true),
            (SessionStatus::Idle, true),
            (SessionStatus::Spawning, true),
            (SessionStatus::AwaitingInput, false),
            (SessionStatus::Stopped, false),
            (SessionStatus::Error, false),
        ] {
            let mut model = model_with(vec![in_repo("a", "r1")]);
            flag(&mut model, "a");
            let mut updated = in_repo("a", "r1");
            updated.status = status;
            model.apply(&DaemonMessage::SessionUpdated {
                session: updated,
                request_id: None,
            });
            assert_eq!(attention_of(&model, "a"), !cleared, "status {status:?}");
        }
    }

    #[test]
    fn leaf_click_clears_attention() {
        let mut model = model_with(vec![in_repo("a", "r1")]);
        flag(&mut model, "a");
        model.clear_attention("a");
        assert!(!attention_of(&model, "a"));
    }

    fn unseen_of(model: &SidebarModel, id: &str) -> bool {
        model
            .containers()
            .iter()
            .flat_map(|c| c.leaves.iter())
            .find(|l| l.id == id)
            .is_some_and(|l| l.unseen)
    }

    fn set_status(model: &mut SidebarModel, session: &SessionSnapshot, status: SessionStatus) {
        let mut updated = session.clone();
        updated.status = status;
        model.apply(&DaemonMessage::SessionUpdated {
            session: updated,
            request_id: None,
        });
    }

    /// `session` works, then its turn ends.
    fn finish_turn(model: &mut SidebarModel, session: &SessionSnapshot) {
        set_status(model, session, SessionStatus::Working);
        set_status(model, session, SessionStatus::Idle);
    }

    #[test]
    fn background_agent_turn_ending_is_unseen() {
        for mode in [SessionMode::Interactive, SessionMode::Headless] {
            let mut a = in_repo("a", "r1");
            a.mode = mode;
            let mut model = model_with(vec![a.clone(), in_repo("b", "r1")]);
            model.set_focused_session(Some("b"));
            finish_turn(&mut model, &a);
            assert!(unseen_of(&model, "a"), "{mode:?}");
            assert!(!unseen_of(&model, "b"), "{mode:?}: b never worked");
        }
    }

    #[test]
    fn turn_ending_across_a_relist_is_unseen() {
        let a = in_repo("a", "r1");
        let mut model = model_with(vec![a.clone()]);
        set_status(&mut model, &a, SessionStatus::Working);
        model.apply(&DaemonMessage::Sessions {
            sessions: vec![a.clone()],
        });
        assert!(unseen_of(&model, "a"));
    }

    #[test]
    fn shell_turn_is_never_unseen() {
        let sh = shell("s", Some("D:\\r1"), SessionKind::Standalone);
        let mut model = model_with(vec![sh.clone()]);
        finish_turn(&mut model, &sh);
        assert!(!model.is_unseen("s"));
    }

    #[test]
    fn focused_session_turn_is_seen() {
        let a = in_repo("a", "r1");
        let mut model = model_with(vec![a.clone()]);
        model.set_focused_session(Some("a"));
        finish_turn(&mut model, &a);
        assert!(!model.is_unseen("a"));
    }

    #[test]
    fn unseen_clears_on_any_other_status_seen_or_removal() {
        for clear in [
            "working", "stopped", "asking", "seen", "removed", "relisted",
        ] {
            let a = in_repo("a", "r1");
            let mut model = model_with(vec![a.clone()]);
            finish_turn(&mut model, &a);
            assert!(model.is_unseen("a"), "{clear}: joined first");
            match clear {
                "working" => set_status(&mut model, &a, SessionStatus::Working),
                "stopped" => set_status(&mut model, &a, SessionStatus::Stopped),
                "asking" => set_status(&mut model, &a, SessionStatus::AwaitingInput),
                "seen" => model.mark_seen("a"),
                "removed" => model.apply(&DaemonMessage::SessionRemoved {
                    session_id: "a".to_owned(),
                }),
                _ => model.apply(&DaemonMessage::Sessions {
                    sessions: Vec::new(),
                }),
            }
            assert!(!model.is_unseen("a"), "{clear}");
        }
    }

    #[test]
    fn attention_rolls_up_to_container() {
        let mut model = model_with(vec![in_repo("a", "r1"), in_repo("b", "r1")]);
        let rolled = |m: &SidebarModel| m.containers().first().is_some_and(|c| c.attention);
        assert!(!rolled(&model));
        flag(&mut model, "b");
        assert!(rolled(&model));
    }

    #[test]
    fn session_messages_update_the_session_list() {
        let mut model = model_with(vec![in_repo("a", "r1")]);
        model.apply(&DaemonMessage::SessionUpdated {
            session: in_repo("b", "r1"),
            request_id: None,
        });
        assert_eq!(model.sessions().len(), 2);
        model.apply(&DaemonMessage::SessionRemoved {
            session_id: "a".to_owned(),
        });
        assert_eq!(leaf_ids(&model.containers(), "repo:r1"), ["b"]);
    }

    #[test]
    fn toggling_a_container_collapses_it() {
        let mut model = model_with(vec![in_repo("a", "r1")]);
        model.toggle_container("repo:r1");
        assert!(model.containers().first().is_some_and(|c| c.collapsed));
        assert!(model.ui_state().collapsed_containers.contains("repo:r1"));
        model.toggle_container("repo:r1");
        assert!(model.containers().first().is_some_and(|c| !c.collapsed));
    }

    #[test]
    fn toggling_the_sidebar_flips_collapsed() {
        let mut model = SidebarModel::new(UiState::default());
        model.toggle_sidebar();
        assert!(model.is_collapsed());
        model.toggle_sidebar();
        assert!(!model.is_collapsed());
    }

    #[test]
    fn set_sc_collapsed_returns_whether_the_value_changed() {
        use crate::source_control::{Part, ScKey};
        let mut model = SidebarModel::new(UiState::default());
        let key = ScKey {
            repo_id: "r1".to_owned(),
            worktree: None,
        };
        assert!(model.set_sc_collapsed(&key, Part::Changes, true));
        assert!(
            !model.set_sc_collapsed(&key, Part::Changes, true),
            "the same value again changes nothing"
        );
        assert!(model.set_sc_collapsed(&key, Part::Changes, false));
        assert!(
            !model
                .source_control()
                .is_collapsed(&key, Part::Changes, Some(0)),
            "the stored value beats the collapsed-when-empty default"
        );
    }

    #[test]
    fn width_is_clamped() {
        assert!((clamp_width(100.0, 1000.0) - MIN_WIDTH).abs() < f32::EPSILON);
        assert!((clamp_width(950.0, 1000.0) - 800.0).abs() < f32::EPSILON);
        assert!((clamp_width(300.0, 1000.0) - 300.0).abs() < f32::EPSILON);
        assert!((clamp_width(300.0, 300.0) - MIN_WIDTH).abs() < f32::EPSILON);
        assert!((clamp_width(f32::NAN, 1000.0) - DEFAULT_WIDTH).abs() < f32::EPSILON);
        let mut model = SidebarModel::new(UiState::default());
        model.set_width(5000.0, 1000.0);
        assert!((model.width(1000.0) - 800.0).abs() < f32::EPSILON);
        assert!((model.width(600.0) - 400.0).abs() < f32::EPSILON);
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, AtomicOrdering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("rt-native-ui-{}-{tag}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create test dir");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn missing_ui_file_gives_defaults() {
        let dir = TestDir::new("missing");
        assert_eq!(load_ui_state(&dir.0), UiState::default());
    }

    #[test]
    fn corrupt_ui_file_gives_defaults() {
        let dir = TestDir::new("corrupt");
        let state = UiState {
            sidebar_width: 432.0,
            ..UiState::default()
        };
        save_ui_state(&dir.0, &state).expect("save");
        std::fs::write(dir.0.join(UI_FILE), "{ not json").expect("write corrupt file");
        assert_eq!(load_ui_state(&dir.0), UiState::default());
    }

    #[test]
    fn partial_ui_file_fills_defaults() {
        let dir = TestDir::new("partial");
        std::fs::write(dir.0.join(UI_FILE), r#"{ "sidebar_collapsed": true }"#).expect("write");
        let loaded = load_ui_state(&dir.0);
        assert!(loaded.sidebar_collapsed);
        assert!((loaded.sidebar_width - DEFAULT_WIDTH).abs() < f32::EPSILON);
        assert!(
            loaded.quick_shell_dir.is_none(),
            "an older file has no folder"
        );
    }

    #[test]
    fn ui_state_round_trips() {
        let dir = TestDir::new("round-trip");
        let state = UiState {
            sidebar_width: 333.0,
            sidebar_collapsed: true,
            activity: Activity::SourceControl,
            sidebar_view: SidebarView::Tabs,
            collapsed_containers: ["repo:r1".to_owned(), "detached".to_owned()].into(),
            active_tab_id: Some("t1".to_owned()),
            quick_shell_dir: Some("C:\\work".to_owned()),
            terminal_font: FontSettings {
                family: Some("JetBrains Mono".to_owned()),
                size: 15.0,
                bold: true,
            },
            tab_font_sizes: [("t1".to_owned(), 20.0)].into(),
            source_control: ScUiState {
                pinned_repo: Some("r1".to_owned()),
                ..ScUiState::default()
            },
            app_appearance: AppColors {
                accent_color: Some("#38bdf8".to_owned()),
                terminal_background_color: Some("#f6f4ef".to_owned()),
                terminal_frame_color: None,
            },
            recent_colors: vec!["#abcdef".to_owned()],
            diff_include_whitespace: false,
            diff_highlight: false,
            window: None,
            general: GeneralSettings {
                copy_on_select: false,
            },
            title: TitleSettings {
                show_count: false,
                suffix: false,
            },
            notifications: NotificationSettings {
                awaiting_input: true,
                stopped: false,
                error: false,
            },
            spawn: SpawnDefaults {
                trusted: true,
                permission_mode: Some(PermissionMode::Plan),
                codex_sandbox: Some(CodexSandbox::DangerFullAccess),
            },
        };
        save_ui_state(&dir.0, &state).expect("first save");
        save_ui_state(&dir.0, &state).expect("save over the existing file");
        assert_eq!(load_ui_state(&dir.0), state);
        assert!(!dir.0.join(format!("{UI_FILE}.tmp")).exists());
    }

    #[test]
    fn spawn_defaults_round_trip_and_default_off() {
        let dir = TestDir::new("spawn-defaults");
        let defaults = SpawnDefaults {
            trusted: true,
            permission_mode: Some(PermissionMode::AcceptEdits),
            codex_sandbox: Some(CodexSandbox::WorkspaceWrite),
        };
        let state = UiState {
            spawn: defaults,
            ..UiState::default()
        };
        save_ui_state(&dir.0, &state).expect("save");
        assert_eq!(load_ui_state(&dir.0).spawn, defaults, "round trips");

        let off = SpawnDefaults::default();
        assert!(!off.trusted);
        assert_eq!(off.permission_mode, None);
        assert_eq!(off.codex_sandbox, None);

        std::fs::write(dir.0.join(UI_FILE), r#"{ "sidebar_collapsed": true }"#).expect("write");
        assert_eq!(
            load_ui_state(&dir.0).spawn,
            off,
            "a file saved before the tab existed loads the defaults"
        );
    }

    #[test]
    fn spawn_defaults_setters_write_each_field() {
        let mut model = SidebarModel::new(UiState::default());
        model.set_spawn_trusted(true);
        model.set_spawn_permission_mode(Some(PermissionMode::Plan));
        model.set_spawn_codex_sandbox(Some(CodexSandbox::ReadOnly));
        assert_eq!(
            model.ui_state().spawn,
            SpawnDefaults {
                trusted: true,
                permission_mode: Some(PermissionMode::Plan),
                codex_sandbox: Some(CodexSandbox::ReadOnly),
            }
        );
    }

    #[test]
    fn settings_groups_missing_from_an_older_file_default_on() {
        let dir = TestDir::new("settings-groups");
        std::fs::write(dir.0.join(UI_FILE), r#"{ "sidebar_collapsed": true }"#).expect("write");
        let loaded = load_ui_state(&dir.0);
        assert!(loaded.general.copy_on_select);
        assert!(loaded.title.show_count);
        assert!(loaded.title.suffix);

        std::fs::write(dir.0.join(UI_FILE), r#"{ "title": { "suffix": false } }"#).expect("write");
        let loaded = load_ui_state(&dir.0);
        assert!(loaded.title.show_count, "a group missing a field fills it");
        assert!(!loaded.title.suffix);
    }

    #[test]
    fn notification_toggles_default_on_when_missing_and_round_trip() {
        let dir = TestDir::new("notifications");
        std::fs::write(dir.0.join(UI_FILE), r#"{ "sidebar_collapsed": true }"#).expect("write");
        let loaded = load_ui_state(&dir.0);
        assert_eq!(loaded.notifications, NotificationSettings::default());
        for reason in [
            AttentionReason::AwaitingInput,
            AttentionReason::Stopped,
            AttentionReason::Error,
        ] {
            assert!(loaded.notifications.fires(reason), "{reason:?} on");
        }

        std::fs::write(
            dir.0.join(UI_FILE),
            r#"{ "notifications": { "stopped": false } }"#,
        )
        .expect("write");
        let loaded = load_ui_state(&dir.0);
        assert!(loaded.notifications.awaiting_input, "a missing field fills");
        assert!(!loaded.notifications.fires(AttentionReason::Stopped));

        let mut model = SidebarModel::new(loaded);
        model.set_notification(AttentionReason::Error, false);
        save_ui_state(&dir.0, model.ui_state()).expect("save");
        assert_eq!(&load_ui_state(&dir.0), model.ui_state());
        assert!(!load_ui_state(&dir.0).notifications.error);
    }

    #[test]
    fn quick_shell_dir_is_set_and_round_trips() {
        let mut model = SidebarModel::default();
        assert_eq!(model.quick_shell_dir(), None);
        assert!(model.set_quick_shell_dir(Some("C:\\work\\deep")));
        assert!(
            !model.set_quick_shell_dir(Some("C:\\work\\deep")),
            "unchanged"
        );
        assert_eq!(model.quick_shell_dir(), Some("C:\\work\\deep"));

        let dir = TestDir::new("quick-shell");
        save_ui_state(&dir.0, model.ui_state()).expect("save");
        assert_eq!(&load_ui_state(&dir.0), model.ui_state());

        assert!(model.set_quick_shell_dir(None));
        assert_eq!(model.quick_shell_dir(), None);
    }

    #[test]
    fn tab_font_sizes_clamp_clear_and_prune() {
        let mut model = SidebarModel::default();
        assert_eq!(model.tab_font_size("t1"), None);
        model.set_tab_font_size("t1", 20.4);
        model.set_tab_font_size("t2", 100.0);
        assert_eq!(model.tab_font_size("t1"), Some(20.0), "stored rounded");
        assert_eq!(model.tab_font_size("t2"), Some(32.0), "stored clamped");

        let live: std::collections::HashSet<&str> = ["t2"].into_iter().collect();
        assert!(model.prune_tab_font_sizes(&live), "t1's override went");
        assert_eq!(model.tab_font_size("t1"), None);
        assert!(!model.prune_tab_font_sizes(&live), "nothing left to prune");

        assert!(model.clear_tab_font_size("t2"));
        assert_eq!(model.tab_font_size("t2"), None);
        assert!(!model.clear_tab_font_size("t2"), "already clear");
    }

    fn split2(first: GridNode, second: GridNode) -> GridNode {
        GridNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    fn named_tab(id: &str, name: &str, grid: &GridNode) -> TabEntry {
        let mut entry = tab(id, grid);
        name.clone_into(&mut entry.name);
        entry
    }

    fn diff_tab(id: &str) -> TabEntry {
        serde_json::from_value(json!({
            "id": id,
            "name": id,
            "content": { "kind": "diff", "repo_id": "r", "path": "a.rs", "against": null },
            "created_at": "2026-01-01T00:00:00Z",
        }))
        .expect("diff tab fixture")
    }

    fn tab_view(tabs: &[TabEntry], sessions: &[SessionSnapshot]) -> Vec<Container> {
        build_tab_containers(
            tabs,
            sessions,
            &HashSet::new(),
            &HashSet::new(),
            &BTreeSet::new(),
        )
    }

    #[test]
    fn tab_view_lists_one_container_per_tab_in_daemon_order() {
        let tabs = [
            named_tab("t2", "zeta", &pane("p1", Some("s1"))),
            named_tab("t1", "alpha", &pane("p2", Some("s2"))),
        ];
        let tree = tab_view(&tabs, &[session("s1"), session("s2")]);
        assert_eq!(keys(&tree), ["tab:t2", "tab:t1"], "not sorted by name");
        let first = &tree[0];
        assert_eq!(
            (first.kind, first.id.as_str(), first.name.as_str()),
            (ContainerKind::Tab, "t2", "zeta")
        );
        assert_eq!(first.kind.tag(), "TAB");
        assert_eq!(first.hover.as_deref(), Some("Tab \"zeta\""));
    }

    #[test]
    fn tab_container_lists_its_sessions_once_in_pane_order() {
        let grid = split2(
            pane("p1", Some("s2")),
            split2(pane("p2", Some("s1")), pane("p3", Some("s2"))),
        );
        let tree = tab_view(&[tab("t1", &grid)], &[session("s1"), session("s2")]);
        assert_eq!(leaf_ids(&tree, "tab:t1"), ["s2", "s1"]);
    }

    #[test]
    fn tab_view_skips_pane_sessions_the_daemon_does_not_list() {
        let grid = split2(
            pane("p1", Some("gone")),
            split2(pane("p2", None), pane("p3", Some("s1"))),
        );
        let tree = tab_view(&[tab("t1", &grid)], &[session("s1")]);
        assert_eq!(leaf_ids(&tree, "tab:t1"), ["s1"]);
        assert_eq!(keys(&tree), ["tab:t1"], "s1 is bound, so no Unbound");
    }

    #[test]
    fn diff_tab_gets_an_empty_container() {
        let tabs = [diff_tab("d1"), tab("t1", &pane("p1", Some("s1")))];
        let tree = tab_view(&tabs, &[session("s1")]);
        assert_eq!(keys(&tree), ["tab:d1", "tab:t1"]);
        assert!(tree[0].leaves.is_empty());
        assert_eq!(tree[0].kind, ContainerKind::Tab);
    }

    #[test]
    fn unbound_bucket_holds_unreferenced_sessions_sorted_by_label() {
        let mut beta = session("s-b");
        "Beta".clone_into(&mut beta.label);
        let mut alpha = session("s-a");
        "alpha".clone_into(&mut alpha.label);
        let tabs = [tab("t1", &pane("p1", Some("s1")))];
        let tree = tab_view(&tabs, &[beta, session("s1"), alpha]);
        assert_eq!(keys(&tree), ["tab:t1", "unbound"]);
        let unbound = &tree[1];
        assert_eq!(
            (unbound.kind, unbound.kind.tag(), unbound.name.as_str()),
            (ContainerKind::Unbound, "UNB", "Unbound")
        );
        assert_eq!(unbound.hover.as_deref(), Some(UNBOUND_HOVER));
        assert_eq!(
            leaf_ids(&tree, "unbound"),
            ["s-a", "s-b"],
            "by label, ignoring case"
        );
    }

    #[test]
    fn unbound_bucket_is_omitted_when_empty() {
        let tree = tab_view(&[tab("t1", &pane("p1", Some("s1")))], &[session("s1")]);
        assert_eq!(keys(&tree), ["tab:t1"]);
        assert!(tab_view(&[], &[]).is_empty());
    }

    #[test]
    fn tab_and_unbound_containers_fold_by_their_own_keys() {
        let mut model = SidebarModel::default();
        model.apply(&DaemonMessage::Sessions {
            sessions: vec![session("s1"), session("s2")],
        });
        let tabs = [tab("t1", &pane("p1", Some("s1")))];
        let folds = |model: &SidebarModel| -> Vec<(String, bool)> {
            model
                .tab_containers(&tabs)
                .into_iter()
                .map(|c| (c.key, c.collapsed))
                .collect()
        };
        model.toggle_container("tab:t1");
        assert_eq!(
            folds(&model),
            [("tab:t1".to_owned(), true), ("unbound".to_owned(), false)]
        );
        model.toggle_container("tab:t1");
        model.toggle_container("unbound");
        assert_eq!(
            folds(&model),
            [("tab:t1".to_owned(), false), ("unbound".to_owned(), true)]
        );
    }

    #[test]
    fn tab_container_rolls_up_attention() {
        let tabs = [
            tab("t1", &pane("p1", Some("s1"))),
            tab("t2", &pane("p2", Some("s2"))),
        ];
        let attention: HashSet<String> = ["s1".to_owned()].into();
        let tree = build_tab_containers(
            &tabs,
            &[session("s1"), session("s2")],
            &attention,
            &HashSet::new(),
            &BTreeSet::new(),
        );
        let marks: Vec<(bool, bool)> = tree
            .iter()
            .map(|c| (c.attention, c.leaves.iter().any(|l| l.attention)))
            .collect();
        assert_eq!(marks, [(true, true), (false, false)]);
    }

    #[test]
    fn sidebar_view_defaults_to_repos_and_round_trips() {
        assert_eq!(UiState::default().sidebar_view, SidebarView::Repos);
        let dir = TestDir::new("sidebar-view");
        std::fs::write(dir.0.join(UI_FILE), r#"{ "sidebar_collapsed": true }"#).expect("write");
        assert_eq!(
            load_ui_state(&dir.0).sidebar_view,
            SidebarView::Repos,
            "an older file shows repos"
        );

        let mut model = SidebarModel::default();
        assert!(model.set_sidebar_view(SidebarView::Tabs));
        assert!(!model.set_sidebar_view(SidebarView::Tabs), "unchanged");
        assert_eq!(model.sidebar_view(), SidebarView::Tabs);
        save_ui_state(&dir.0, model.ui_state()).expect("save");
        let text = std::fs::read_to_string(dir.0.join(UI_FILE)).expect("read");
        let saved: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(saved["sidebar_view"], "tabs");
        assert_eq!(load_ui_state(&dir.0).sidebar_view, SidebarView::Tabs);
    }

    #[test]
    fn closed_tab_fold_key_is_pruned() {
        let mut model = SidebarModel::default();
        for key in ["tab:t1", "tab:t2", "repo:r1", "unbound"] {
            model.toggle_container(key);
        }
        let live: HashSet<&str> = ["t2"].into_iter().collect();
        assert!(model.prune_tab_folds(&live), "t1's fold went");
        let kept: Vec<&str> = model
            .ui_state()
            .collapsed_containers
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(kept, ["repo:r1", "tab:t2", "unbound"]);
        assert!(!model.prune_tab_folds(&live), "nothing left to prune");
    }

    #[test]
    fn unknown_sidebar_view_loads_as_repos() {
        let dir = TestDir::new("unknown-view");
        std::fs::write(
            dir.0.join(UI_FILE),
            r#"{ "sidebar_view": "timeline", "sidebar_collapsed": true }"#,
        )
        .expect("write");
        let loaded = load_ui_state(&dir.0);
        assert_eq!(loaded.sidebar_view, SidebarView::Repos);
        assert!(loaded.sidebar_collapsed, "the rest of the file still loads");

        std::fs::write(dir.0.join(UI_FILE), r#"{ "sidebar_view": "repos" }"#).expect("write");
        assert_eq!(load_ui_state(&dir.0).sidebar_view, SidebarView::Repos);
        std::fs::write(dir.0.join(UI_FILE), r#"{ "sidebar_view": "tabs" }"#).expect("write");
        assert_eq!(load_ui_state(&dir.0).sidebar_view, SidebarView::Tabs);
    }

    #[test]
    fn activity_round_trips_needs_you() {
        let text = serde_json::to_string(&Activity::NeedsYou).expect("serialise");
        assert_eq!(text, r#""needs_you""#);
        let back: Activity = serde_json::from_str(&text).expect("deserialise");
        assert_eq!(back, Activity::NeedsYou);
        let source_control: Activity =
            serde_json::from_str(r#""source_control""#).expect("deserialise");
        assert_eq!(source_control, Activity::SourceControl);
    }

    #[test]
    fn unknown_activity_loads_as_sessions() {
        let dir = TestDir::new("unknown-activity");
        std::fs::write(
            dir.0.join(UI_FILE),
            r#"{ "activity": "timeline", "sidebar_width": 333.0 }"#,
        )
        .expect("write");
        let loaded = load_ui_state(&dir.0);
        assert_eq!(loaded.activity, Activity::Sessions);
        assert!(
            (loaded.sidebar_width - 333.0).abs() < f32::EPSILON,
            "the rest of the file still loads"
        );
    }
}
