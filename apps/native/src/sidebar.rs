//! The sidebar's model: the repos view's containers and session leaves built
//! from the daemon's registry, the attention set, and the persisted sidebar
//! layout. Plain Rust, so every rule is unit-tested; `sidebar_view` renders it.

use protocol::{
    AppearanceOverrides, ContainerRef, DaemonMessage, RepoEntry, SessionKind, SessionMode,
    SessionSnapshot, SessionStatus, WorkspaceEntry,
};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use crate::appearance::{self, AppColors, AppLevel, Resolved};
use crate::fonts::{self, FontSettings};
use crate::source_control::{Part, ScKey, ScUiState};

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
        }
    }
}

/// One session row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leaf {
    pub id: String,
    pub status: SessionStatus,
    pub label: String,
    pub runtime: Option<String>,
    pub attention: bool,
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
    #[default]
    Sessions,
    SourceControl,
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
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            sidebar_width: DEFAULT_WIDTH,
            sidebar_collapsed: false,
            activity: Activity::Sessions,
            collapsed_containers: BTreeSet::new(),
            active_tab_id: None,
            quick_shell_dir: None,
            terminal_font: FontSettings::default(),
            tab_font_sizes: BTreeMap::new(),
            source_control: ScUiState::default(),
            app_appearance: AppColors::default(),
            recent_colors: Vec::new(),
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
            DaemonMessage::Sessions { sessions } => sessions.clone_into(&mut self.sessions),
            DaemonMessage::SessionUpdated { session, .. } => self.update_session(session),
            DaemonMessage::SessionRemoved { session_id } => {
                self.sessions.retain(|s| &s.id != session_id);
                self.attention.remove(session_id);
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
            collapsed: &self.ui.collapsed_containers,
        })
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
        .map(|s| Leaf {
            id: s.id.clone(),
            status: s.status,
            label: display_label(s),
            runtime: runtime_label(s),
            attention: inputs.attention.contains(&s.id),
        })
        .collect();
    let key = match kind {
        ContainerKind::Workspace => format!("ws:{id}"),
        ContainerKind::Repo => format!("repo:{id}"),
        ContainerKind::Shell => format!("standalone:{id}"),
        ContainerKind::Dir => format!("cwd:{id}"),
        ContainerKind::Detached => "detached".to_owned(),
    };
    Container {
        collapsed: inputs.collapsed.contains(&key),
        attention: leaves.iter().any(|l| l.attention),
        key,
        id: id.to_owned(),
        kind,
        name,
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
    use protocol::{AttentionReason, SessionMember};
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
        };
        save_ui_state(&dir.0, &state).expect("first save");
        save_ui_state(&dir.0, &state).expect("save over the existing file");
        assert_eq!(load_ui_state(&dir.0), state);
        assert!(!dir.0.join(format!("{UI_FILE}.tmp")).exists());
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
}
