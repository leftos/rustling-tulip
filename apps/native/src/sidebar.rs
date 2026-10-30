//! The sidebar's model: the repos view's containers and session leaves built
//! from the daemon's registry, the attention set, and the persisted sidebar
//! layout. Plain Rust, so every rule is unit-tested; `sidebar_view` renders it.

use protocol::{
    AccentToken, AgentDescriptor, AgentId, AppearanceOverrides, CharRange, ContainerRef,
    DaemonMessage, RepoEntry, SessionChip, SessionKind, SessionMode, SessionSnapshot,
    SessionStatus, SpawnConfig, SpawnTarget, TabEntry, WorkspaceEntry,
};
use serde::{Deserialize, Deserializer, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use crate::agent_options::{AgentOptions, descriptor_for};
use crate::appearance::{self, AppColors, AppLevel, Resolved, ThemeBase};
use crate::chat::PaneView;
use crate::connection_picker::SavedHost;
use crate::fonts::{self, FontSettings};
use crate::launch_last;
use crate::popouts::{PopoutKey, SavedPopout};
use crate::source_control::{Part, ScKey, ScUiState};
use crate::tabs::collect_panes;
use crate::target::HostKey;
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
            Self::Detached => "DET",
            Self::Tab => "TAB",
            Self::Unbound => "UNB",
        }
    }
}

/// The Unbound container's hover text.
pub const UNBOUND_HOVER: &str = "Sessions alive but not referenced by any tab. \
     Click the unbound pill on a session to open it in a new tab.";

/// What an expanded Detached container says above its sessions.
pub const DETACHED_BANNER: &str = "These sessions are still running, but their repo or workspace \
     is no longer registered. Resume one from its row, or stop it from its header.";

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

/// Which repo and workspace rows the repos view shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoFilter {
    /// Only the ones a live session works in.
    Active,
    /// Every one of them; also what a value this build does not know loads
    /// as (`serde(other)` must be the last variant).
    #[default]
    #[serde(other)]
    All,
}

/// How much room a session leaf takes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LeafDensity {
    /// One line, the subline moved into the hover text.
    Compact,
    /// The label line over the subline, with a chip row when the chips do
    /// not fit beside the subline; also what a value this build does not
    /// know loads as (`serde(other)` must be the last variant).
    #[default]
    #[serde(other)]
    Comfortable,
}

/// How many of a repo or workspace's sessions show under its row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionsUnderRepos {
    /// Only the sessions still at work: a starting, working, waiting or
    /// failed one, and one carrying background work, an unseen finish or a
    /// request for the owner.
    Busy,
    /// None of them; the row keeps its session count.
    None,
    /// Every session of the repo or workspace; also what a value this build
    /// does not know loads as (`serde(other)` must be the last variant).
    #[default]
    #[serde(other)]
    Every,
}

impl SessionsUnderRepos {
    /// Whether `leaf` shows under its repo or workspace row in this mode.
    #[must_use]
    pub fn shows(self, leaf: &Leaf) -> bool {
        match self {
            Self::Every => true,
            Self::None => false,
            Self::Busy => {
                matches!(
                    leaf.status,
                    SessionStatus::Spawning
                        | SessionStatus::Working
                        | SessionStatus::AwaitingInput
                        | SessionStatus::Error
                ) || leaf.background
                    || leaf.unseen
                    || leaf.attention
            }
        }
    }
}

/// How the repos view orders its repo and workspace rows; shell, folder and
/// Detached rows stay after them whatever the choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoSort {
    /// The manual order a drag sets.
    Manual,
    /// The ones a live session counts for first, then the rest.
    Active,
    /// The ones with a session that needs the owner first, then the active
    /// ones, then the rest; also what a value this build does not know loads
    /// as (`serde(other)` must be the last variant).
    #[default]
    #[serde(other)]
    NeedsYou,
}

/// One session row.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent per-session marks the row draws, not a state machine"
)]
pub struct Leaf {
    pub id: String,
    pub status: SessionStatus,
    pub mode: SessionMode,
    /// The agent driving the session; `None` for a plain shell.
    pub agent: Option<AgentId>,
    pub label: String,
    /// The line under the label: the branch, the members' repos and
    /// branches, or a shell's folder.
    pub subline: Option<String>,
    pub runtime: Option<String>,
    /// The runtime tag wears its agent's accent colour: the agent's
    /// descriptor asks for one and the tag names the agent itself.
    pub accent: bool,
    /// The chips the daemon evaluated from the session's agent options;
    /// empty when there is nothing to say.
    pub chips: Vec<SessionChip>,
    /// The session is in the attention set; Needs You reads this raw value,
    /// the `!` badge reads [`Leaf::shows_attention`].
    pub attention: bool,
    /// The session is marked "Don't count as busy".
    pub busy_excluded: bool,
    /// The agent's last turn ended while this client looked elsewhere.
    pub unseen: bool,
    /// The session is idle with its turn's background work still running.
    pub background: bool,
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
    #[must_use]
    pub fn tag(&self) -> Option<(String, String)> {
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
    /// Whether the leaf wears the `!` badge and rolls it up to its
    /// container: it needs attention and counts as busy.
    #[must_use]
    pub fn shows_attention(&self) -> bool {
        self.attention && !self.busy_excluded
    }

    /// The row's tags as text and hover text: the runtime, then the
    /// orphan, abandoned or parked state.
    #[must_use]
    pub fn tags(&self) -> Vec<(String, String)> {
        let mut tags: Vec<(String, String)> = self.runtime_tag().into_iter().collect();
        tags.extend(self.state.tag());
        tags
    }

    /// The runtime tag and its hover text; `None` for a shell with no
    /// known program.
    #[must_use]
    pub fn runtime_tag(&self) -> Option<(String, String)> {
        let runtime = self.runtime.as_ref()?;
        let tip = if self.trusted {
            format!("Running {runtime}; approval prompts were bypassed")
        } else {
            format!("Running {runtime}")
        };
        Some((runtime.clone(), tip))
    }

    /// The hover text at `density`: Compact adds the subline it does not
    /// draw, except on a shell, whose hover already ends with its folder.
    #[must_use]
    pub fn hover(&self, density: LeafDensity) -> String {
        match (&self.subline, density) {
            (Some(subline), LeafDensity::Compact) if self.mode != SessionMode::PlainShell => {
                format!("{}\n{subline}", self.tooltip)
            }
            _ => self.tooltip.clone(),
        }
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
    /// The last launch's one-line summary; repos and workspaces that have
    /// launched before have one.
    pub summary: Option<String>,
    /// The row's hover text, when it has one.
    pub hover: Option<String>,
    /// The folder the row's actions act on: a DIR container's path, or a
    /// standalone shell's current folder; `None` for every other kind and
    /// for a shell with no folder.
    pub path: Option<String>,
    pub leaves: Vec<Leaf>,
    /// Any leaf shows attention ([`Leaf::shows_attention`]).
    pub attention: bool,
    pub collapsed: bool,
}

impl Container {
    /// The row's hover text: its own, else the last launch's summary.
    #[must_use]
    pub fn tooltip(&self) -> Option<String> {
        self.hover.clone().or_else(|| {
            self.summary
                .as_ref()
                .map(|summary| format!("Last launch: {summary}"))
        })
    }
}

/// Everything the container tree is built from.
pub struct TreeInputs<'a> {
    pub repos: &'a [RepoEntry],
    pub workspaces: &'a [WorkspaceEntry],
    pub sessions: &'a [SessionSnapshot],
    /// The daemon's agents, in its order; empty while disconnected.
    pub agents: &'a [AgentDescriptor],
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
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is a saved toggle of its own, set and read separately"
)]
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
    /// Which repo and workspace rows the repos view shows.
    #[serde(default)]
    pub repo_filter: RepoFilter,
    pub collapsed_containers: BTreeSet<String>,
    /// The tab shown when the client last ran, restored when the daemon
    /// sends the tab list.
    pub active_tab_id: Option<String>,
    /// The folder "+ Shell" opens in, when the user picked one.
    #[serde(default)]
    pub quick_shell_dir: Option<String>,
    /// The folder Add repo's picker opens at: the parent of the folder it
    /// last added.
    #[serde(default)]
    pub last_repo_dir: Option<String>,
    /// The folder the workspace creator's file picker opens at: the parent of
    /// the `.code-workspace` file it last picked.
    #[serde(default)]
    pub last_vscode_dir: Option<String>,
    /// The font every new terminal pane starts from.
    #[serde(default)]
    pub terminal_font: FontSettings,
    /// The family the interface draws in; `None` is
    /// [`crate::fonts::DEFAULT_UI_FAMILY`].
    #[serde(default)]
    pub ui_font_family: Option<String>,
    /// The whole pixels added to every UI text size, within
    /// [`crate::fonts::UI_TEXT_SIZE_OFFSETS`]; a stored value outside it
    /// clamps on load.
    #[serde(default, deserialize_with = "clamped_ui_text_size")]
    pub ui_text_size: i8,
    /// The interface's body weight, one of [`crate::fonts::UI_WEIGHTS`]; any
    /// other stored value loads as [`crate::fonts::DEFAULT_UI_WEIGHT`].
    #[serde(deserialize_with = "offered_ui_font_weight")]
    pub ui_font_weight: u16,
    /// The theme the interface draws in: a built-in's id or a user theme's
    /// file slug; `None` is the built-in theme.
    #[serde(default)]
    pub theme: Option<String>,
    /// Terminal font sizes overridden per tab, by tab id; a tab's size wins
    /// over its session's, its container's and the app's.
    #[serde(default)]
    pub tab_font_sizes: BTreeMap<String, f32>,
    /// The source-control panel's pinned repo and collapsed sections.
    #[serde(default)]
    pub source_control: ScUiState,
    /// The app-level colours releases up to v0.5.2-alpha saved, read so the
    /// boot can move them into a theme
    /// ([`crate::theme_store::migrate_app_colors`]); written back only while
    /// that move has not happened.
    #[serde(default, skip_serializing_if = "AppColors::is_empty")]
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
    /// Whether a Markdown file opens in the viewer rendered, not as source.
    #[serde(default = "rendered_by_default")]
    pub viewer_markdown_rendered: bool,
    /// The main window's place when it last moved; `None` until it has.
    #[serde(default)]
    pub window: Option<WindowState>,
    /// The open pop-outs, in the order they opened, reopened at the next
    /// launch. An entry that does not decode is dropped on load.
    #[serde(default, deserialize_with = "decodable_popouts")]
    pub popouts: Vec<SavedPopout>,
    /// The Settings modal's General tab.
    #[serde(default)]
    pub general: GeneralSettings,
    /// The Settings modal's App title tab.
    #[serde(default)]
    pub title: TitleSettings,
    /// Which OS notifications fire.
    #[serde(default)]
    pub notifications: NotificationSettings,
    /// The Settings modal's Spawn defaults tab.
    #[serde(default)]
    pub spawn: SpawnDefaults,
    /// The tailnet or SSH host the client last connected to; `None` is this
    /// PC.
    #[serde(default, deserialize_with = "last_target_or_local")]
    pub last_target: Option<SavedHost>,
    /// The tailnet and SSH hosts the user added, in the order they were
    /// added. An entry that is not a valid host, or of an unknown kind, is
    /// dropped on load.
    #[serde(default, deserialize_with = "valid_saved_hosts")]
    pub saved_hosts: Vec<SavedHost>,
    /// The session count each remote host last had, by
    /// [`crate::target::Target::host_key`]: a tailnet host's
    /// lower-cased `MagicDNS` name, or `ssh:` and an SSH host's string. A
    /// connect whose first list differs offers the import.
    #[serde(default)]
    pub host_session_counts: BTreeMap<String, u32>,
    /// The view each session was last switched to, by host and session id;
    /// a new pane showing the session opens in it.
    #[serde(default)]
    pub session_views: SessionViews,
}

/// What a session's entry under `session_views` holds when its last view
/// was its chat, or its summary.
const CHAT_VIEW: &str = "chat";
const SUMMARY_VIEW: &str = "summary";

/// The name `session_views` writes for `view`; a terminal is not written.
fn view_name(view: PaneView) -> Option<&'static str> {
    match view {
        PaneView::Terminal => None,
        PaneView::Chat => Some(CHAT_VIEW),
        PaneView::Summary => Some(SUMMARY_VIEW),
    }
}

/// The view a saved `session_views` name stands for, when it names one.
fn view_of_name(name: &str) -> Option<PaneView> {
    match name {
        CHAT_VIEW => Some(PaneView::Chat),
        SUMMARY_VIEW => Some(PaneView::Summary),
        _ => None,
    }
}

/// The host key `session_views` files this PC's daemon under; a remote host
/// uses [`crate::target::Target::session_count_key`].
pub const LOCAL_HOST_KEY: &str = "local";

/// The view each session was last switched to, saved under `session_views`
/// as `{"<host key>": {"<session id>": "chat" | "summary"}}`, so each host's
/// sessions are kept apart. A terminal is not written: a session not named
/// shows its terminal, and a host with none is not written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionViews {
    hosts: BTreeMap<String, BTreeMap<String, PaneView>>,
}

impl SessionViews {
    /// The view session `session_id` of host `host` was last switched to.
    #[must_use]
    pub fn get(&self, host: &str, session_id: &str) -> PaneView {
        self.hosts
            .get(host)
            .and_then(|views| views.get(session_id))
            .copied()
            .unwrap_or_default()
    }

    /// Remembers `view` as session `session_id`'s of host `host`; returns
    /// whether that changed what is remembered.
    pub fn remember(&mut self, host: &str, session_id: &str, view: PaneView) -> bool {
        if view != PaneView::Terminal {
            let previous = self
                .hosts
                .entry(host.to_owned())
                .or_default()
                .insert(session_id.to_owned(), view);
            return previous != Some(view);
        }
        let Some(views) = self.hosts.get_mut(host) else {
            return false;
        };
        let removed = views.remove(session_id).is_some();
        if views.is_empty() {
            self.hosts.remove(host);
        }
        removed
    }

    /// Forgets every session of host `host` that `keep` refuses, leaving
    /// the other hosts alone; returns whether one went.
    pub fn retain(&mut self, host: &str, mut keep: impl FnMut(&str) -> bool) -> bool {
        let Some(views) = self.hosts.get_mut(host) else {
            return false;
        };
        let before = views.len();
        views.retain(|session_id, _| keep(session_id));
        let changed = views.len() != before;
        if views.is_empty() {
            self.hosts.remove(host);
        }
        changed
    }
}

/// One host's remembered views as written: `{"<session id>": "chat"}`.
struct HostViews<'a>(&'a BTreeMap<String, PaneView>);

impl Serialize for HostViews<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(
            self.0
                .iter()
                .filter_map(|(session_id, view)| Some((session_id, view_name(*view)?))),
        )
    }
}

impl Serialize for SessionViews {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(
            self.hosts
                .iter()
                .map(|(host, views)| (host, HostViews(views))),
        )
    }
}

/// Reads `session_views`, never failing the file: a value that is not a map
/// remembers nothing, a host whose value is not a map is dropped, and an
/// entry whose view is not `"chat"` or `"summary"` is dropped, each with a
/// warning.
impl<'de> Deserialize<'de> for SessionViews {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let entries = match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::Null => return Ok(Self::default()),
            serde_json::Value::Object(entries) => entries,
            other => {
                tracing::warn!(
                    "saved session views are {}, not a map; every session opens on its terminal",
                    json_type(&other)
                );
                return Ok(Self::default());
            }
        };
        let hosts = entries
            .into_iter()
            .filter_map(|(host, views)| {
                let views = decode_host_views(&host, views)?;
                (!views.is_empty()).then_some((host, views))
            })
            .collect();
        Ok(Self { hosts })
    }
}

/// The views saved for host `host`'s sessions; `None`, with a warning, when
/// its value is not a map.
fn decode_host_views(host: &str, views: serde_json::Value) -> Option<BTreeMap<String, PaneView>> {
    let serde_json::Value::Object(entries) = views else {
        tracing::warn!(
            "saved session views of host {host} are {}, not a map; dropped",
            json_type(&views)
        );
        return None;
    };
    Some(
        entries
            .into_iter()
            .filter_map(|(session_id, view)| {
                if let Some(known) = view.as_str().and_then(view_of_name) {
                    return Some((session_id, known));
                }
                tracing::warn!(
                    "saved view {view} of session {session_id} on host {host} is not a chat or a summary; dropped"
                );
                None
            })
            .collect(),
    )
}

/// Reads `ui_text_size`, never failing the file: a whole number clamps to
/// [`crate::fonts::UI_TEXT_SIZE_OFFSETS`], and anything else loads as no
/// offset with a warning naming it.
fn clamped_ui_text_size<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i8, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    let whole = value.as_i64().or_else(|| value.as_u64().map(|_| i64::MAX));
    Ok(whole.map_or_else(
        || {
            tracing::warn!("ui_text_size {value} is not a whole number; drawing at no offset");
            0
        },
        crate::fonts::clamped_ui_text_size,
    ))
}

/// Reads `ui_font_weight`, never failing the file: anything but a weight the
/// setting offers loads as the default with a warning naming it.
fn offered_ui_font_weight<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u16, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    let offered = value
        .as_u64()
        .and_then(|weight| u16::try_from(weight).ok())
        .filter(|weight| crate::fonts::UI_WEIGHTS.contains(weight));
    Ok(offered.unwrap_or_else(|| {
        tracing::warn!(
            "ui_font_weight {value} is not 400, 500 or 600; drawing at {}",
            crate::fonts::DEFAULT_UI_WEIGHT
        );
        crate::fonts::DEFAULT_UI_WEIGHT
    }))
}

/// Reads `last_target` so that a value that is not a host record loads as
/// this PC and never fails the rest of the file.
fn last_target_or_local<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<SavedHost>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    let last: Option<SavedHost> = serde_json::from_value(value.clone()).unwrap_or_else(|err| {
        tracing::warn!("last_target {value} is not a saved host ({err}); starting on this PC");
        None
    });
    Ok(last.and_then(|host| {
        host.normalised()
            .inspect_err(|err| {
                tracing::warn!(
                    "last_target {:?} {}:{} is not valid ({err}); starting on this PC",
                    host.kind,
                    host.name,
                    host.port
                );
            })
            .ok()
    }))
}

/// Reads `saved_hosts`, keeping each tailnet entry whose name passes the
/// DNS-name check and whose port is 1 to 65535, and each SSH entry whose
/// host string passes its check, normalised; every other entry, an unknown
/// kind included, is dropped with a warning naming it.
fn valid_saved_hosts<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<SavedHost>, D::Error> {
    let entries = Option::<Vec<serde_json::Value>>::deserialize(deserializer)?;
    Ok(entries
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            let host: SavedHost = serde_json::from_value(entry.clone())
                .inspect_err(|err| {
                    tracing::warn!("saved host {entry} is not a host record ({err}); dropped");
                })
                .ok()?;
            match host.normalised() {
                Ok(normal) => Some(normal),
                Err(err) => {
                    tracing::warn!(
                        "saved host {:?} {}:{} is not valid ({err}); dropped",
                        host.kind,
                        host.name,
                        host.port
                    );
                    None
                }
            }
        })
        .collect())
}

/// Reads `popouts`, never failing the file: a value that is not a list
/// loads as no pop-outs, and each entry is read by [`decode_saved_popout`],
/// each with a warning naming what was wrong.
fn decodable_popouts<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<SavedPopout>, D::Error> {
    let entries = match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Null => return Ok(Vec::new()),
        serde_json::Value::Array(entries) => entries,
        other => {
            tracing::warn!(
                "saved pop-outs are {}, not a list; no pop-out reopens",
                json_type(&other)
            );
            return Ok(Vec::new());
        }
    };
    Ok(entries.iter().filter_map(decode_saved_popout).collect())
}

/// One saved pop-out: an entry whose window place does not decode keeps
/// its key with no place, and one whose key does not decode is dropped.
fn decode_saved_popout(entry: &serde_json::Value) -> Option<SavedPopout> {
    if let Ok(saved) = SavedPopout::deserialize(entry) {
        return Some(saved);
    }
    match PopoutKey::deserialize(entry.get("key").unwrap_or(&serde_json::Value::Null)) {
        Ok(key) => {
            tracing::warn!(
                "saved pop-out {entry} has a window place that does not decode; \
                 it reopens at its first-open place"
            );
            Some(SavedPopout { key, window: None })
        }
        Err(err) => {
            tracing::warn!("saved pop-out {entry} is not a pop-out record ({err}); dropped");
            None
        }
    }
}

/// What kind of JSON value `value` is, as a warning names it.
fn json_type(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "a list",
        serde_json::Value::Object(_) => "an object",
    }
}

/// What the spawn dialog pre-fills from the client's Spawn defaults: only
/// whether new agent sessions launch trusted. The agents' own option
/// defaults are the daemon's agent settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpawnDefaults {
    /// New agent sessions bypass approval prompts.
    pub trusted: bool,
}

/// Which OS notifications fire. Keys this build does not know, such as
/// the old `stopped` and `error` toggles, are dropped on load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationSettings {
    /// Whether a session awaiting input raises Needs you; on unless turned
    /// off.
    pub awaiting_input: bool,
    /// Whether a session whose turn ended with no background work left
    /// raises Finished; on unless turned off.
    pub finished: bool,
    /// When a notification for a session the owner is looking at is skipped.
    #[serde(deserialize_with = "skip_or_default")]
    pub skip_when: NotifySkip,
}

/// Reads `skip_when` so that any value that is not a known choice (null, a
/// number, an object, an unknown word) loads as the default and never fails
/// the rest of the file.
fn skip_or_default<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<NotifySkip, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value.clone()).unwrap_or_else(|err| {
        tracing::warn!(
            "notifications.skip_when {value} is not a known choice ({err}); using on_screen"
        );
        NotifySkip::default()
    }))
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            awaiting_input: true,
            finished: true,
            skip_when: NotifySkip::default(),
        }
    }
}

/// When the desktop client skips a notification for a session the owner is
/// already looking at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifySkip {
    /// When the client window is focused and a pane of its active tab shows
    /// the session, with nothing drawn over the panes.
    #[default]
    OnScreen,
    /// Whenever the client window is focused.
    Focused,
    /// Never: every notification fires.
    Never,
}

/// The General settings saved on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralSettings {
    /// Whether a mouse selection in a terminal is copied on release.
    pub copy_on_select: bool,
    /// How much room each session leaf takes.
    pub leaf_density: LeafDensity,
    /// How many of a repo or workspace's sessions show under its row.
    pub sessions_under_repos: SessionsUnderRepos,
    /// How the repos view orders its repo and workspace rows.
    pub repo_sort: RepoSort,
    /// Whether text files a link or a fetch opens go to the external app
    /// instead of the built-in viewer.
    pub open_text_externally: bool,
    /// Whether the client checks for updates 30 s after launch and every
    /// 24 h; Settings' Check now checks either way.
    pub check_updates: bool,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            copy_on_select: true,
            leaf_density: LeafDensity::Comfortable,
            sessions_under_repos: SessionsUnderRepos::Every,
            repo_sort: RepoSort::NeedsYou,
            open_text_externally: false,
            check_updates: true,
        }
    }
}

/// What the tab pills' badges and the window title's count count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CountMode {
    /// Busy terminals over all live ones.
    Busy,
    /// Idle terminals over all live ones.
    Idle,
    /// The terminals waiting on the owner, nothing at 0; also what a value
    /// this build does not know loads as (`serde(other)` must be the last
    /// variant).
    #[default]
    #[serde(other)]
    NeedsYou,
}

/// What the main window's title shows, and what the tab pills count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TitleSettings {
    /// The active tab's count before its name.
    pub show_count: bool,
    /// ` — rustling-tulip` after it.
    pub suffix: bool,
    /// What the window title's count and the tab pills' badges count.
    pub count: CountMode,
}

impl Default for TitleSettings {
    fn default() -> Self {
        Self {
            show_count: true,
            suffix: true,
            count: CountMode::NeedsYou,
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

/// A layout saved before the viewer's Markdown toggle existed renders
/// Markdown.
fn rendered_by_default() -> bool {
    true
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            sidebar_width: DEFAULT_WIDTH,
            sidebar_collapsed: false,
            activity: Activity::Sessions,
            sidebar_view: SidebarView::Repos,
            repo_filter: RepoFilter::All,
            collapsed_containers: BTreeSet::new(),
            active_tab_id: None,
            quick_shell_dir: None,
            last_repo_dir: None,
            last_vscode_dir: None,
            terminal_font: FontSettings::default(),
            ui_font_family: None,
            ui_text_size: 0,
            ui_font_weight: crate::fonts::DEFAULT_UI_WEIGHT,
            theme: None,
            tab_font_sizes: BTreeMap::new(),
            source_control: ScUiState::default(),
            app_appearance: AppColors::default(),
            recent_colors: Vec::new(),
            diff_include_whitespace: true,
            diff_highlight: true,
            viewer_markdown_rendered: true,
            window: None,
            popouts: Vec::new(),
            general: GeneralSettings::default(),
            title: TitleSettings::default(),
            notifications: NotificationSettings::default(),
            spawn: SpawnDefaults::default(),
            last_target: None,
            saved_hosts: Vec::new(),
            host_session_counts: BTreeMap::new(),
            session_views: SessionViews::default(),
        }
    }
}

/// The key [`SidebarModel::slices`] names This PC's slice by.
static LOCAL_HOST: HostKey = HostKey::Local;

/// One host's part of the sidebar: the registry, sessions and agents its
/// daemon sent, and the orders and sets kept from them.
#[derive(Debug, Default)]
struct HostSlice {
    repos: Vec<RepoEntry>,
    repos_loaded: bool,
    workspaces: Vec<WorkspaceEntry>,
    sessions: Vec<SessionSnapshot>,
    /// The daemon's agents, in its order, as the connection last sent them;
    /// empty while disconnected.
    agents: Vec<AgentDescriptor>,
    container_order: Vec<ContainerRef>,
    session_order: HashMap<String, Vec<String>>,
    attention: HashSet<String>,
    /// Agent sessions whose turn ended while this client showed another
    /// session focused, until the user looks at them.
    unseen: HashSet<String>,
}

impl HostSlice {
    /// Folds a whole-list or order message into the slice, replacing what
    /// it held of that list; messages it does not track are ignored.
    fn apply(&mut self, msg: &DaemonMessage, focused: Option<&str>) {
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
            _ => self.apply_session_message(msg, focused),
        }
    }

    fn apply_session_message(&mut self, msg: &DaemonMessage, focused: Option<&str>) {
        match msg {
            DaemonMessage::Sessions { sessions } => {
                for session in sessions {
                    let before = self.turn_state(&session.id);
                    self.note_turn(before, session, focused);
                }
                self.unseen
                    .retain(|id| sessions.iter().any(|s| &s.id == id));
                sessions.clone_into(&mut self.sessions);
            }
            DaemonMessage::SessionUpdated { session, .. } => self.update_session(session, focused),
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
    fn update_session(&mut self, session: &SessionSnapshot, focused: Option<&str>) {
        let before = self.turn_state(&session.id);
        self.note_turn(before, session, focused);
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

    fn session(&self, id: &str) -> Option<&SessionSnapshot> {
        self.sessions.iter().find(|s| s.id == id)
    }

    /// The listed session's status and background flag, before an update
    /// replaces them.
    fn turn_state(&self, id: &str) -> Option<(SessionStatus, bool)> {
        self.session(id).map(|s| (s.status, s.background))
    }

    /// An agent session whose turn ends while another session is shown
    /// focused joins the unseen set: it goes idle with no background work
    /// left, from working or from idle with background work. Any status but
    /// idle leaves the set, so it holds only idle sessions.
    fn note_turn(
        &mut self,
        before: Option<(SessionStatus, bool)>,
        session: &SessionSnapshot,
        focused: Option<&str>,
    ) {
        let turn_running = matches!(
            before,
            Some((SessionStatus::Working, _) | (SessionStatus::Idle, true))
        );
        match session.status {
            SessionStatus::Idle
                if turn_running
                    && !session.background
                    && session.mode != SessionMode::PlainShell
                    && focused != Some(session.id.as_str()) =>
            {
                self.unseen.insert(session.id.clone());
            }
            SessionStatus::Idle => {}
            _ => {
                self.unseen.remove(&session.id);
            }
        }
    }

    /// The container tree's inputs from this slice, folded by `collapsed`.
    fn tree_inputs<'a>(&'a self, collapsed: &'a BTreeSet<String>) -> TreeInputs<'a> {
        TreeInputs {
            repos: &self.repos,
            workspaces: &self.workspaces,
            sessions: &self.sessions,
            agents: &self.agents,
            container_order: &self.container_order,
            session_order: &self.session_order,
            attention: &self.attention,
            unseen: &self.unseen,
            collapsed,
        }
    }

    /// Whether the registry is in and holds a repo or a workspace.
    fn has_registry(&self) -> bool {
        self.repos_loaded && !(self.repos.is_empty() && self.workspaces.is_empty())
    }
}

/// The key a container of `host` folds by: This PC's keeps the plain `key`,
/// a remote host's carries its host key in front, `<host key>::<key>`, so
/// two hosts' `detached` rows or equal folders never share a fold.
#[must_use]
pub fn host_container_key(host: &HostKey, key: &str) -> String {
    match host.wire() {
        None => key.to_owned(),
        Some(name) => format!("{name}::{key}"),
    }
}

/// Every daemon's registry as the sidebar sees it, one slice per host, plus
/// the window's own layout.
#[derive(Debug, Default)]
pub struct SidebarModel {
    /// This PC's slice, the layout home's.
    local: HostSlice,
    /// The remote hosts' slices, in the order they first sent one.
    remote: Vec<(HostKey, HostSlice)>,
    /// The session this client shows focused, as the root last reported it
    /// before folding a message in.
    focused: Option<String>,
    ui: UiState,
    /// The saved pop-outs whose windows the user closed since the layout was
    /// last written, each with its place in the saved list when it left,
    /// oldest first: a quit that starts before the next write saves them
    /// again.
    closed_popouts: Vec<(usize, SavedPopout)>,
    /// The active theme's accent and terminal background, the bottom of
    /// every session's appearance cascade.
    theme_base: ThemeBase,
}

impl SidebarModel {
    pub fn new(ui: UiState) -> Self {
        Self {
            ui,
            ..Self::default()
        }
    }

    /// Makes `base` the bottom of every appearance cascade, as a theme
    /// switch does.
    pub fn set_theme_base(&mut self, base: ThemeBase) {
        self.theme_base = base;
    }

    /// The active theme's accent and terminal background.
    #[must_use]
    pub fn theme_base(&self) -> ThemeBase {
        self.theme_base
    }

    /// Every host's slice, This PC's first, then the remote hosts in order.
    fn slices(&self) -> impl Iterator<Item = (&HostKey, &HostSlice)> {
        std::iter::once((&LOCAL_HOST, &self.local))
            .chain(self.remote.iter().map(|(host, slice)| (host, slice)))
    }

    /// `host`'s slice; `None` for a remote host that never sent one.
    fn slice(&self, host: &HostKey) -> Option<&HostSlice> {
        match host {
            HostKey::Local => Some(&self.local),
            HostKey::Remote(_) => self
                .remote
                .iter()
                .find(|(key, _)| key == host)
                .map(|(_, slice)| slice),
        }
    }

    /// `host`'s slice, to update; a remote host's first one joins the end.
    fn slice_mut(&mut self, host: &HostKey) -> &mut HostSlice {
        if *host == HostKey::Local {
            return &mut self.local;
        }
        let index = self
            .remote
            .iter()
            .position(|(key, _)| key == host)
            .unwrap_or_else(|| {
                self.remote.push((host.clone(), HostSlice::default()));
                self.remote.len() - 1
            });
        &mut self.remote[index].1
    }

    /// The slice that lists session `id`, else This PC's.
    fn slice_of_session(&self, id: &str) -> &HostSlice {
        self.slices()
            .map(|(_, slice)| slice)
            .find(|slice| slice.session(id).is_some())
            .unwrap_or(&self.local)
    }

    /// Forgets everything `host`'s daemon sent, as a switch of its
    /// connection to another daemon must; other hosts' slices and the
    /// layout in `ui`, which is this PC's, stay. A remote host keeps its
    /// place in the order; This PC's also forgets the focused session, a
    /// session of the layout home.
    pub fn clear_host(&mut self, host: &HostKey) {
        if *host == HostKey::Local {
            self.focused = None;
        }
        if let Some(slice) = match host {
            HostKey::Local => Some(&mut self.local),
            HostKey::Remote(_) => self
                .remote
                .iter_mut()
                .find(|(key, _)| key == host)
                .map(|(_, slice)| slice),
        } {
            *slice = HostSlice::default();
        }
    }

    /// Keeps `host`'s agents, in its daemon's order, as its connection
    /// sends them.
    pub fn set_agents(&mut self, host: &HostKey, agents: Vec<AgentDescriptor>) {
        self.slice_mut(host).agents = agents;
    }

    /// `host`'s agents, in its daemon's order, as its connection last sent
    /// them; empty for a host that sent none.
    #[must_use]
    pub fn agents(&self, host: &HostKey) -> &[AgentDescriptor] {
        self.slice(host).map_or(&[], |slice| &slice.agents)
    }

    /// The agents of the host that lists session `id`, This PC's for a
    /// session no host lists: a session's label and chips read its own
    /// host's descriptors.
    #[must_use]
    pub fn session_agents(&self, id: &str) -> &[AgentDescriptor] {
        &self.slice_of_session(id).agents
    }

    /// Fold a message from `host`'s daemon into that host's slice; a whole
    /// list replaces only that host's list, and messages the model does not
    /// track are ignored.
    pub fn apply(&mut self, host: &HostKey, msg: &DaemonMessage) {
        let focused = self.focused.clone();
        self.slice_mut(host).apply(msg, focused.as_deref());
    }

    /// Records the session this client shows focused, so a turn that ends
    /// there is not marked unseen.
    pub fn set_focused_session(&mut self, id: Option<&str>) {
        self.focused = id.map(str::to_owned);
    }

    /// The user looked at the session: its finished turn is seen.
    pub fn mark_seen(&mut self, id: &str) {
        self.local.unseen.remove(id);
        for (_, slice) in &mut self.remote {
            slice.unseen.remove(id);
        }
    }

    /// Whether the session's last turn ended unseen.
    pub fn is_unseen(&self, id: &str) -> bool {
        self.slices().any(|(_, slice)| slice.unseen.contains(id))
    }

    /// This PC's sessions, the layout home's.
    pub fn local_sessions(&self) -> &[SessionSnapshot] {
        &self.local.sessions
    }

    /// Every host's sessions, This PC's first, then the remote hosts' in
    /// order.
    pub fn all_sessions(&self) -> impl Iterator<Item = &SessionSnapshot> {
        self.slices().flat_map(|(_, slice)| slice.sessions.iter())
    }

    /// How many of This PC's sessions are abandoned, the set
    /// `ResumeAllAbandoned` acts on.
    #[must_use]
    pub fn abandoned_count(&self) -> usize {
        self.local
            .sessions
            .iter()
            .filter(|s| s.is_abandoned)
            .count()
    }

    /// This PC's registered repos, in its daemon's order.
    pub fn repos(&self) -> &[RepoEntry] {
        &self.local.repos
    }

    /// Whether This PC's repo list has arrived.
    pub fn repos_loaded(&self) -> bool {
        self.local.repos_loaded
    }

    /// This PC's registered workspaces, in its daemon's order.
    pub fn workspaces(&self) -> &[WorkspaceEntry] {
        &self.local.workspaces
    }

    /// The spawn config `container` last launched with, when it has one.
    #[must_use]
    pub fn last_spawn_config(&self, container: launch_last::Container<'_>) -> Option<SpawnConfig> {
        self.slices().find_map(|(_, slice)| {
            launch_last::stored_config(container, &slice.repos, &slice.workspaces)
        })
    }

    /// Session `id`, on whichever host lists it.
    pub fn session(&self, id: &str) -> Option<&SessionSnapshot> {
        self.find_session(id).map(|(_, session)| session)
    }

    /// Session `id` and the host that lists it.
    #[must_use]
    pub fn find_session(&self, id: &str) -> Option<(HostKey, &SessionSnapshot)> {
        self.slices()
            .find_map(|(host, slice)| slice.session(id).map(|session| (host.clone(), session)))
    }

    /// Repo `id` and the host that registered it.
    #[must_use]
    pub fn find_repo(&self, id: &str) -> Option<(HostKey, &RepoEntry)> {
        self.slices().find_map(|(host, slice)| {
            slice
                .repos
                .iter()
                .find(|repo| repo.id == id)
                .map(|repo| (host.clone(), repo))
        })
    }

    /// Workspace `id` and the host that registered it.
    #[must_use]
    pub fn find_workspace(&self, id: &str) -> Option<(HostKey, &WorkspaceEntry)> {
        self.slices().find_map(|(host, slice)| {
            slice
                .workspaces
                .iter()
                .find(|workspace| workspace.id == id)
                .map(|workspace| (host.clone(), workspace))
        })
    }

    /// Every host's containers, This PC's first, then each remote host's in
    /// order, a remote host's keys carrying its host key
    /// ([`host_container_key`]).
    pub fn containers(&self) -> Vec<Container> {
        let collapsed = &self.ui.collapsed_containers;
        self.slices()
            .flat_map(|(host, slice)| {
                let built = build_containers(&slice.tree_inputs(collapsed));
                built.into_iter().map(move |mut container| {
                    if *host != HostKey::Local {
                        container.key = host_container_key(host, &container.key);
                        container.collapsed = collapsed.contains(&container.key);
                    }
                    container
                })
            })
            .collect()
    }

    /// The keys of every host's repo and workspace containers a live
    /// session counts for ([`active_container_keys`]).
    fn active_keys(&self) -> HashSet<String> {
        self.slices()
            .flat_map(|(host, slice)| {
                active_container_keys(&slice.repos, &slice.workspaces, &slice.sessions)
                    .into_iter()
                    .map(move |key| host_container_key(host, &key))
            })
            .collect()
    }

    /// The repos view's containers in the order its rows show:
    /// [`Self::containers`], which keeps the manual order, sorted by the
    /// chosen [`RepoSort`] and, under [`RepoFilter::Active`], cut down to the
    /// repo and workspace rows a live session works in or the Needs You list
    /// shows.
    pub fn display_containers(&self) -> Vec<Container> {
        let active = self.active_keys();
        let sorted = sort_containers(self.containers(), self.repo_sort(), &active);
        if self.repo_filter() == RepoFilter::All {
            return sorted;
        }
        sorted
            .into_iter()
            .filter(|c| {
                !matches!(c.kind, ContainerKind::Repo | ContainerKind::Workspace)
                    || active.contains(&c.key)
                    || needs_you(c)
            })
            .collect()
    }

    /// Whether the repos view's Active filter has left the body without a
    /// repo or workspace row: the view is Repos, a daemon's registry is in
    /// and holds one, and none of `shown` is a repo or workspace.
    #[must_use]
    pub fn active_filter_empty(&self, shown: &[Container]) -> bool {
        self.sidebar_view() == SidebarView::Repos
            && self.repo_filter() == RepoFilter::Active
            && self.slices().any(|(_, slice)| slice.has_registry())
            && !shown
                .iter()
                .any(|c| matches!(c.kind, ContainerKind::Repo | ContainerKind::Workspace))
    }

    /// Whether the repos view's rows keyed `a` and `b` sort into the same
    /// group under the chosen [`RepoSort`] ([`sort_group`]); false when
    /// either row is not there.
    pub fn same_sort_group(&self, a: &str, b: &str) -> bool {
        let active = self.active_keys();
        let sort = self.repo_sort();
        let containers = self.containers();
        let group = |key: &str| {
            containers
                .iter()
                .find(|c| c.key == key)
                .map(|c| sort_group(c, sort, &active))
        };
        let a = group(a);
        a.is_some() && a == group(b)
    }

    /// The tabs view's containers for `tabs`, This PC's layout, in the
    /// daemon's tab order.
    pub fn tab_containers(&self, tabs: &[TabEntry]) -> Vec<Container> {
        let local = &self.local;
        build_tab_containers(
            tabs,
            &local.sessions,
            &local.agents,
            &TabRollups {
                session_order: &local.session_order,
                attention: &local.attention,
                unseen: &local.unseen,
            },
            &self.ui.collapsed_containers,
        )
    }

    /// Takes `ordered` as `host`'s manual container order at once, before
    /// its daemon's broadcast of it arrives; that broadcast writes the same
    /// order, and one that disagrees replaces it.
    pub fn reorder_containers_local(&mut self, host: &HostKey, ordered: &[ContainerRef]) {
        ordered.clone_into(&mut self.slice_mut(host).container_order);
    }

    /// Takes `ordered_ids` as the manual session order of `host`'s
    /// workspace, repo or tab `container_id` at once, before its daemon's
    /// broadcast of it arrives; that broadcast writes the same order, and
    /// one that disagrees replaces it.
    pub fn reorder_sessions_local(
        &mut self,
        host: &HostKey,
        container_id: &str,
        ordered_ids: &[String],
    ) {
        self.slice_mut(host)
            .session_order
            .insert(container_id.to_owned(), ordered_ids.to_vec());
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

    /// Which repo and workspace rows the repos view shows.
    pub fn repo_filter(&self) -> RepoFilter {
        self.ui.repo_filter
    }

    /// Records which repo and workspace rows the repos view shows; returns
    /// whether it changed.
    pub fn set_repo_filter(&mut self, filter: RepoFilter) -> bool {
        if self.ui.repo_filter == filter {
            return false;
        }
        self.ui.repo_filter = filter;
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
        self.local.attention.remove(id);
        for (_, slice) in &mut self.remote {
            slice.attention.remove(id);
        }
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

    /// Shows `item`'s panel, unfolding a folded one; never folds it.
    pub fn show_activity(&mut self, item: Activity) {
        self.ui.activity = item;
        self.ui.sidebar_collapsed = false;
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

    /// Sets the family the interface draws in; `None` is the default.
    pub fn set_ui_font_family(&mut self, family: Option<String>) {
        self.ui.ui_font_family = family;
    }

    /// Sets the whole pixels added to every UI text size, clamped to
    /// [`crate::fonts::UI_TEXT_SIZE_OFFSETS`].
    pub fn set_ui_text_size(&mut self, offset: i8) {
        self.ui.ui_text_size = crate::fonts::clamped_ui_text_size(i64::from(offset));
    }

    /// Makes `weight` the interface's body weight; one the setting does not
    /// offer is [`crate::fonts::DEFAULT_UI_WEIGHT`].
    pub fn set_ui_font_weight(&mut self, weight: u16) {
        self.ui.ui_font_weight = if crate::fonts::UI_WEIGHTS.contains(&weight) {
            weight
        } else {
            crate::fonts::DEFAULT_UI_WEIGHT
        };
    }

    /// Sets the theme the interface draws in; `None` is the built-in theme.
    pub fn set_theme_id(&mut self, id: Option<String>) {
        self.ui.theme = id;
    }

    /// Whether the diff tabs show whitespace-only changes.
    pub fn set_diff_include_whitespace(&mut self, include: bool) {
        self.ui.diff_include_whitespace = include;
    }

    /// Whether the diff tabs colour code by the file's language.
    pub fn set_diff_highlight(&mut self, highlight: bool) {
        self.ui.diff_highlight = highlight;
    }

    /// Whether a Markdown file opens in the viewer rendered, not as source.
    pub fn set_viewer_markdown_rendered(&mut self, rendered: bool) {
        self.ui.viewer_markdown_rendered = rendered;
    }

    /// Whether a terminal selection is copied on release.
    pub fn set_copy_on_select(&mut self, on: bool) {
        self.ui.general.copy_on_select = on;
    }

    /// Whether text files go to the external app instead of the viewer.
    pub fn set_open_text_externally(&mut self, on: bool) {
        self.ui.general.open_text_externally = on;
    }

    /// Whether the client checks for updates on its own.
    pub fn set_check_updates(&mut self, on: bool) {
        self.ui.general.check_updates = on;
    }

    /// How much room each session leaf takes.
    pub fn leaf_density(&self) -> LeafDensity {
        self.ui.general.leaf_density
    }

    /// Records how much room each session leaf takes; returns whether it
    /// changed.
    pub fn set_leaf_density(&mut self, density: LeafDensity) -> bool {
        if self.ui.general.leaf_density == density {
            return false;
        }
        self.ui.general.leaf_density = density;
        true
    }

    /// How many of a repo or workspace's sessions show under its row.
    pub fn sessions_under_repos(&self) -> SessionsUnderRepos {
        self.ui.general.sessions_under_repos
    }

    /// Records how many of a repo or workspace's sessions show under its row;
    /// returns whether it changed.
    pub fn set_sessions_under_repos(&mut self, mode: SessionsUnderRepos) -> bool {
        if self.ui.general.sessions_under_repos == mode {
            return false;
        }
        self.ui.general.sessions_under_repos = mode;
        true
    }

    /// How the repos view orders its repo and workspace rows.
    pub fn repo_sort(&self) -> RepoSort {
        self.ui.general.repo_sort
    }

    /// Records how the repos view orders its repo and workspace rows;
    /// returns whether it changed.
    pub fn set_repo_sort(&mut self, sort: RepoSort) -> bool {
        if self.ui.general.repo_sort == sort {
            return false;
        }
        self.ui.general.repo_sort = sort;
        true
    }

    /// Whether the window title shows the active tab's count.
    pub fn set_title_show_count(&mut self, on: bool) {
        self.ui.title.show_count = on;
    }

    /// Records what the tab pills and the window title count; returns
    /// whether it changed.
    pub fn set_count_mode(&mut self, mode: CountMode) -> bool {
        if self.ui.title.count == mode {
            return false;
        }
        self.ui.title.count = mode;
        true
    }

    /// Whether the window title ends with the product name.
    pub fn set_title_suffix(&mut self, on: bool) {
        self.ui.title.suffix = on;
    }

    /// Whether a session awaiting input raises Needs you.
    pub fn set_notification(&mut self, on: bool) {
        self.ui.notifications.awaiting_input = on;
    }

    /// Whether a session whose turn ended raises Finished.
    pub fn set_finished_notification(&mut self, on: bool) {
        self.ui.notifications.finished = on;
    }

    /// Records when a notification for a session on screen is skipped;
    /// returns whether it changed.
    pub fn set_notify_skip(&mut self, skip: NotifySkip) -> bool {
        if self.ui.notifications.skip_when == skip {
            return false;
        }
        self.ui.notifications.skip_when = skip;
        true
    }

    /// Whether a spawn dialog opens with trusted launch on.
    pub fn set_spawn_trusted(&mut self, on: bool) {
        self.ui.spawn.trusted = on;
    }

    /// Records the main window's place; returns whether it changed.
    pub fn set_window_state(&mut self, state: WindowState) -> bool {
        if self.ui.window.as_ref() == Some(&state) {
            return false;
        }
        self.ui.window = Some(state);
        true
    }

    /// The view session `session_id` of host `host` was last switched to.
    #[must_use]
    pub fn session_view(&self, host: &str, session_id: &str) -> PaneView {
        self.ui.session_views.get(host, session_id)
    }

    /// Remembers `view` as session `session_id`'s last on host `host`;
    /// returns whether that changed what is saved.
    pub fn remember_session_view(&mut self, host: &str, session_id: &str, view: PaneView) -> bool {
        self.ui.session_views.remember(host, session_id, view)
    }

    /// Forgets the view of every session of host `host`, the one whose
    /// list this is, that the list no longer holds; another host's views
    /// stay. Returns whether one went.
    pub fn drop_ended_session_views(&mut self, host: &str) -> bool {
        let live: HashSet<String> = self.sessions.iter().map(|s| s.id.clone()).collect();
        self.ui
            .session_views
            .retain(host, |session_id| live.contains(session_id))
    }

    /// Hands over the saved pop-outs, leaving none saved.
    pub fn take_saved_popouts(&mut self) -> Vec<SavedPopout> {
        std::mem::take(&mut self.ui.popouts)
    }

    /// Replaces the saved pop-outs with `popouts`.
    pub fn set_saved_popouts(&mut self, popouts: Vec<SavedPopout>) {
        self.ui.popouts = popouts;
    }

    /// Records an opened pop-out, last, with the place its window opens at;
    /// returns whether it was not already saved.
    pub fn save_popout(&mut self, key: PopoutKey, window: Option<WindowState>) -> bool {
        if self.ui.popouts.iter().any(|saved| saved.key == key) {
            return false;
        }
        self.ui.popouts.push(SavedPopout { key, window });
        true
    }

    /// Records where `key`'s pop-out window now is; returns whether the
    /// saved entry changed.
    pub fn set_popout_window(&mut self, key: &PopoutKey, state: WindowState) -> bool {
        let Some(saved) = self.ui.popouts.iter_mut().find(|saved| saved.key == *key) else {
            return false;
        };
        if saved.window.as_ref() == Some(&state) {
            return false;
        }
        saved.window = Some(state);
        true
    }

    /// Gives the saved pop-out `old` the key `new`; returns whether it was
    /// saved.
    pub fn rekey_saved_popout(&mut self, old: &PopoutKey, new: PopoutKey) -> bool {
        let Some(saved) = self.ui.popouts.iter_mut().find(|saved| saved.key == *old) else {
            return false;
        };
        saved.key = new;
        true
    }

    /// Forgets the saved pop-out `key`; returns whether it was saved.
    pub fn forget_saved_popout(&mut self, key: &PopoutKey) -> bool {
        let before = self.ui.popouts.len();
        self.ui.popouts.retain(|saved| saved.key != *key);
        self.ui.popouts.len() != before
    }

    /// Forgets the saved pop-out `key` as its window closes, keeping it to
    /// save again should a quit start before the layout is next written;
    /// returns whether it was saved.
    pub fn close_saved_popout(&mut self, key: &PopoutKey) -> bool {
        let Some(index) = self.ui.popouts.iter().position(|saved| saved.key == *key) else {
            return false;
        };
        let entry = self.ui.popouts.remove(index);
        self.closed_popouts.push((index, entry));
        true
    }

    /// Saves again, each in its old place, the pop-outs whose windows closed
    /// since the layout was last written, as a quit closing every window
    /// must; one saved again since is left as it is. Returns whether any
    /// came back.
    pub fn keep_closed_popouts(&mut self) -> bool {
        let mut kept = false;
        while let Some((index, entry)) = self.closed_popouts.pop() {
            if self.ui.popouts.iter().any(|saved| saved.key == entry.key) {
                continue;
            }
            let index = index.min(self.ui.popouts.len());
            self.ui.popouts.insert(index, entry);
            kept = true;
        }
        kept
    }

    /// The layout was written: the pop-outs closed before it stay forgotten.
    pub fn popouts_written(&mut self) {
        self.closed_popouts.clear();
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
        self.all_sessions()
            .map(|session| {
                (
                    session.id.as_str(),
                    self.resolve(Some(session)).accent.value,
                )
            })
            .collect()
    }

    /// `session`'s appearance through the containers of the host that lists
    /// it (This PC's for none).
    fn resolve(&self, session: Option<&SessionSnapshot>) -> Resolved {
        let slice = session.map_or(&self.local, |s| self.slice_of_session(&s.id));
        appearance::resolve(
            session,
            &slice.repos,
            &slice.workspaces,
            AppLevel {
                font: &self.ui.terminal_font,
                theme: self.theme_base,
            },
        )
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

    /// The folder Add repo's picker opens at, once a repo was added.
    pub fn last_repo_dir(&self) -> Option<&str> {
        self.ui.last_repo_dir.as_deref()
    }

    /// Records the folder Add repo's picker opens at next; returns whether
    /// it changed, so the caller knows whether there is anything to save.
    pub fn set_last_repo_dir(&mut self, dir: &str) -> bool {
        if self.ui.last_repo_dir.as_deref() == Some(dir) {
            return false;
        }
        self.ui.last_repo_dir = Some(dir.to_owned());
        true
    }

    /// The folder the workspace creator's file picker opens at, once a
    /// `.code-workspace` file was picked.
    pub fn last_vscode_dir(&self) -> Option<&str> {
        self.ui.last_vscode_dir.as_deref()
    }

    /// Records the folder the workspace creator's file picker opens at next;
    /// returns whether it changed, so the caller knows whether to save.
    pub fn set_last_vscode_dir(&mut self, dir: &str) -> bool {
        if self.ui.last_vscode_dir.as_deref() == Some(dir) {
            return false;
        }
        self.ui.last_vscode_dir = Some(dir.to_owned());
        true
    }

    /// Records the tailnet host the client boots onto next; `None` is this
    /// PC.
    pub fn set_last_target(&mut self, host: Option<SavedHost>) {
        self.ui.last_target = host;
    }

    /// Replaces the tailnet hosts the host picker offers.
    pub fn set_saved_hosts(&mut self, hosts: Vec<SavedHost>) {
        self.ui.saved_hosts = hosts;
    }

    /// The session count last recorded for the tailnet host `host`.
    pub fn host_session_count(&self, host: &str) -> Option<u32> {
        self.ui.host_session_counts.get(host).copied()
    }

    /// Records `host`'s session count; returns whether it changed, so the
    /// caller knows whether to save.
    pub fn set_host_session_count(&mut self, host: &str, count: u32) -> bool {
        self.ui.host_session_counts.insert(host.to_owned(), count) != Some(count)
    }

    /// Drops `host`'s recorded session count; returns whether it had one.
    pub fn forget_host_session_count(&mut self, host: &str) -> bool {
        self.ui.host_session_counts.remove(host).is_some()
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
                display_label(s, inputs.agents),
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
    let path = match kind {
        ContainerKind::Dir => Some(id.to_owned()),
        ContainerKind::Shell => sessions
            .first()
            .and_then(|s| s.current_cwd.as_deref())
            .filter(|cwd| !cwd.is_empty())
            .map(normalize_fs_path),
        ContainerKind::Workspace
        | ContainerKind::Repo
        | ContainerKind::Detached
        | ContainerKind::Tab
        | ContainerKind::Unbound => None,
    };
    sessions.sort_by(|a, b| cmp_ci(&a.label, &b.label));
    let leaves: Vec<Leaf> = apply_session_order(sessions, inputs.session_order.get(id))
        .into_iter()
        .map(|s| leaf(s, inputs.attention, inputs.unseen, false, inputs.agents))
        .collect();
    let key = container_key(kind, id);
    let summary = match kind {
        ContainerKind::Repo => inputs
            .repos
            .iter()
            .find(|r| r.id == id)
            .and_then(|r| launch_summary(r.last_spawn_config.as_ref(), inputs.agents)),
        ContainerKind::Workspace => inputs
            .workspaces
            .iter()
            .find(|w| w.id == id)
            .and_then(|w| launch_summary(w.last_spawn_config.as_ref(), inputs.agents)),
        ContainerKind::Shell
        | ContainerKind::Dir
        | ContainerKind::Detached
        | ContainerKind::Tab
        | ContainerKind::Unbound => None,
    };
    Container {
        collapsed: inputs.collapsed.contains(&key),
        attention: leaves.iter().any(Leaf::shows_attention),
        key,
        id: id.to_owned(),
        kind,
        name,
        summary,
        hover: None,
        path,
        leaves,
    }
}

/// The user's home folder, which a shell's subline shows as `~`.
static HOME: LazyLock<Option<String>> =
    LazyLock::new(|| std::env::home_dir().map(|home| home.to_string_lossy().into_owned()));

/// Session `s`'s leaf; `tabs_view` names the repo beside a single repo's
/// branch, since no repo container above it does.
fn leaf(
    s: &SessionSnapshot,
    attention: &HashSet<String>,
    unseen: &HashSet<String>,
    tabs_view: bool,
    agents: &[AgentDescriptor],
) -> Leaf {
    let runtime = runtime_label(s, agents);
    Leaf {
        id: s.id.clone(),
        status: s.status,
        mode: s.mode,
        agent: s.agent.clone(),
        label: display_label(s, agents),
        subline: subline(s, HOME.as_deref(), tabs_view),
        accent: runtime_accent(s, runtime.as_deref(), agents),
        runtime,
        chips: s.agent_chips.clone(),
        attention: attention.contains(&s.id),
        busy_excluded: s.busy_excluded,
        unseen: unseen.contains(&s.id),
        background: s.background,
        state: LeafState::of(s),
        trusted: s.elevated_authority,
        tooltip: leaf_tooltip(s, agents),
    }
}

/// One line naming a container's last launch: the agent and run mode, the
/// target, each field the launch set away from its default in the agent's
/// order (a choice's label, a typed value, a toggle's label), `trusted`, and
/// the env rows it carried. Parts that are absent are left out; no config,
/// no line.
#[must_use]
pub fn launch_summary(config: Option<&SpawnConfig>, agents: &[AgentDescriptor]) -> Option<String> {
    let config = config?;
    let mut parts: Vec<String> = agent_summary_label(config, agents).into_iter().collect();
    match config.mode {
        SessionMode::Interactive => parts.push("Interactive".to_owned()),
        SessionMode::Headless => parts.push("Headless".to_owned()),
        SessionMode::PlainShell => {}
    }
    parts.push(match &config.target {
        SpawnTarget::Single {
            branch_name,
            base_branch,
            use_worktree,
            ..
        }
        | SpawnTarget::Workspace {
            branch_name,
            base_branch,
            use_worktree,
            ..
        } => {
            if *use_worktree {
                format!(
                    "new worktree from {}",
                    base_branch.as_deref().unwrap_or(branch_name)
                )
            } else {
                branch_name.clone()
            }
        }
        SpawnTarget::Standalone { .. } => "standalone".to_owned(),
    });
    if config.mode != SessionMode::PlainShell
        && let Some(selection) = &config.agent
        && let Some(descriptor) = descriptor_for(agents, &selection.agent)
    {
        let options = AgentOptions::new(descriptor.clone(), Some(&selection.options));
        parts.extend(options.summary_fields(config.dangerously_skip_permissions));
    }
    if config.dangerously_skip_permissions {
        parts.push("trusted".to_owned());
    }
    if !config.extra_env.is_empty() {
        parts.push(format!("{} env", config.extra_env.len()));
    }
    Some(parts.join(" · "))
}

/// The agent's name in a launch summary: a plain-shell config names the
/// shell, an agent session its descriptor's label, and a launch that names
/// no agent the default descriptor's. `None` only when nothing names it.
fn agent_summary_label(config: &SpawnConfig, agents: &[AgentDescriptor]) -> Option<String> {
    if config.mode == SessionMode::PlainShell {
        return Some("Shell".to_owned());
    }
    match &config.agent {
        Some(selection) => Some(descriptor_for(agents, &selection.agent).map_or_else(
            || selection.agent.0.clone(),
            |descriptor| descriptor.label.clone(),
        )),
        None => agents.first().map(|descriptor| descriptor.label.clone()),
    }
}

/// The ⋯ menu's bulk-resume row while two or more sessions are abandoned.
#[must_use]
pub fn resume_all_label(count: usize) -> Option<String> {
    (count >= 2).then(|| format!("Resume all abandoned ({count})"))
}

/// The container row drawn as hovered: the one under the pointer, while the
/// pointer is still in the window.
#[must_use]
pub fn shown_hovered_row(hovered: Option<&str>, window_hovered: bool) -> Option<&str> {
    hovered.filter(|_| window_hovered)
}

/// The key focused row navigation moves to: `delta` rows from `current`
/// among `keys`, clamped to the ends; with no current row, the first for a
/// forward move and the last for a backward one.
#[must_use]
pub fn move_row_focus(keys: &[String], current: Option<&str>, delta: isize) -> Option<String> {
    let last = keys.len().checked_sub(1)?.cast_signed();
    let index = match current.and_then(|key| keys.iter().position(|k| k == key)) {
        Some(index) => (index.cast_signed() + delta).clamp(0, last),
        None if delta < 0 => last,
        None => 0,
    };
    keys.get(index.cast_unsigned()).cloned()
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

/// The per-session state the tabs view orders and marks its leaves by: each
/// tab's manual session order, and the sessions that need attention or have
/// output not yet seen.
#[derive(Clone, Copy)]
pub struct TabRollups<'a> {
    pub session_order: &'a HashMap<String, Vec<String>>,
    pub attention: &'a HashSet<String>,
    pub unseen: &'a HashSet<String>,
}

/// The tabs view: one container per tab in the daemon's order, holding the
/// listed sessions its panes show, each once, in the tab's manual order from
/// `session_order` and then pane order (a diff tab's is empty); then
/// Unbound, the sessions no pane shows sorted by label, when there are any.
pub fn build_tab_containers(
    tabs: &[TabEntry],
    sessions: &[SessionSnapshot],
    agents: &[AgentDescriptor],
    rollups: &TabRollups<'_>,
    collapsed: &BTreeSet<String>,
) -> Vec<Container> {
    let TabRollups {
        session_order,
        attention,
        unseen,
    } = *rollups;
    let by_id: HashMap<&str, &SessionSnapshot> =
        sessions.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut shown: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();
    for tab in tabs {
        let mut seen: HashSet<&str> = HashSet::new();
        let in_pane_order: Vec<&SessionSnapshot> = tab
            .grid()
            .map(collect_panes)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|pane| pane.session)
            .filter(|id| seen.insert(id))
            .filter_map(|id| by_id.get(id).copied())
            .collect();
        let leaves: Vec<Leaf> = apply_session_order(in_pane_order, session_order.get(&tab.id))
            .into_iter()
            .map(|s| {
                shown.insert(s.id.as_str());
                leaf(s, attention, unseen, true, agents)
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
            .map(|s| leaf(s, attention, unseen, true, agents))
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
        attention: leaves.iter().any(Leaf::shows_attention),
        key,
        id: id.to_owned(),
        kind,
        name,
        summary: None,
        hover: Some(hover),
        path: None,
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

/// The keys of the repo and workspace containers a live session counts for
/// ([`crate::repo_remove::is_live`]: orphans, headless runs and plain shells
/// count). A repo is active when a session's member names it, or a member's
/// start folder, one of the session's worktrees or its current folder is the
/// repo's folder or under it; a workspace when a session was spawned for it
/// or any of its member repos is active.
#[must_use]
pub(crate) fn active_container_keys(
    repos: &[RepoEntry],
    workspaces: &[WorkspaceEntry],
    sessions: &[SessionSnapshot],
) -> HashSet<String> {
    let live: Vec<&SessionSnapshot> = sessions
        .iter()
        .filter(|s| crate::repo_remove::is_live(s))
        .collect();
    let active_repos: HashSet<&str> = repos
        .iter()
        .filter(|repo| live.iter().any(|s| counts_for_repo(s, repo)))
        .map(|repo| repo.id.as_str())
        .collect();
    let mut keys: HashSet<String> = active_repos
        .iter()
        .map(|id| container_key(ContainerKind::Repo, id))
        .collect();
    for w in workspaces {
        let spawned_for = live
            .iter()
            .any(|s| s.workspace_id.as_deref() == Some(w.id.as_str()));
        let member_active = w
            .member_repo_ids
            .iter()
            .any(|id| active_repos.contains(id.as_str()));
        if spawned_for || member_active {
            keys.insert(container_key(ContainerKind::Workspace, &w.id));
        }
    }
    keys
}

/// Whether `s` counts for `repo`: a member names it, or a member's start
/// folder, one of its worktrees or its current folder is the repo's folder or
/// under it.
fn counts_for_repo(s: &SessionSnapshot, repo: &RepoEntry) -> bool {
    let under = |path: &str| !path.is_empty() && path_contains(&repo.path, path);
    s.members
        .iter()
        .any(|m| m.repo_id == repo.id || under(&m.worktree_path))
        || s.worktree_paths.iter().any(|path| under(path))
        || s.current_cwd.as_deref().is_some_and(under)
}

/// Whether a session of `container` needs the owner: the Needs You list
/// shows it.
fn needs_you(container: &Container) -> bool {
    container.leaves.iter().any(crate::needs_you::is_listed)
}

/// The group `container` sorts into under `sort`, lower first: under Needs
/// you on top 0 for needing the owner, 1 for active, 2 for the rest; under
/// Active first 0 for active, 1 for the rest; under Manual 0. A shell, folder
/// or Detached row is always 3, after every repo and workspace.
#[must_use]
pub fn sort_group(container: &Container, sort: RepoSort, active: &HashSet<String>) -> u8 {
    if !matches!(
        container.kind,
        ContainerKind::Repo | ContainerKind::Workspace
    ) {
        return 3;
    }
    let inactive = u8::from(!active.contains(&container.key));
    match sort {
        RepoSort::Manual => 0,
        RepoSort::Active => inactive,
        RepoSort::NeedsYou if needs_you(container) => 0,
        RepoSort::NeedsYou => 1 + inactive,
    }
}

/// `containers` in `sort`'s order: the repo and workspace rows sorted into
/// its groups ([`sort_group`]), each group keeping their incoming manual
/// order, and every other row after them in its own order.
#[must_use]
pub fn sort_containers(
    mut containers: Vec<Container>,
    sort: RepoSort,
    active: &HashSet<String>,
) -> Vec<Container> {
    containers.sort_by_key(|c| sort_group(c, sort, active));
    containers
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
pub(crate) fn normalize_fs_path(path: &str) -> String {
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
/// meaningful terminal title, its label, its runtime (an agent's tag from
/// its descriptor in `agents`), then its id.
pub fn display_label(s: &SessionSnapshot, agents: &[AgentDescriptor]) -> String {
    let cwd_leaf = || {
        (s.mode == SessionMode::PlainShell)
            .then(|| non_empty(s.current_cwd.as_deref()))
            .flatten()
            .map(path_leaf_name)
    };
    let title = || session_title(s, agents).filter(|t| !is_noisy_shell_title(t));
    non_empty(s.user_label.as_deref())
        .map(str::to_owned)
        .or_else(cwd_leaf)
        .or_else(|| title().map(str::to_owned))
        .or_else(|| non_empty(Some(&s.label)).map(str::to_owned))
        .or_else(|| runtime_label(s, agents))
        .unwrap_or_else(|| s.id.clone())
}

/// A session name's hover text: the name, then the daemon's label and the
/// terminal title where they say something else, then the working directory.
/// It is the pane header's title tip as well as part of a leaf's hover.
/// `agents` names an agent session's runtime, as [`display_label`] says.
pub fn label_tooltip(s: &SessionSnapshot, agents: &[AgentDescriptor]) -> String {
    let display = display_label(s, agents);
    let mut lines = vec![display.clone()];
    if let Some(label) = non_empty(Some(&s.label)).filter(|l| *l != display) {
        lines.push(format!("Session: {label}"));
    }
    if let Some(title) = session_title(s, agents).filter(|t| *t != display) {
        lines.push(format!("Terminal title: {title}"));
    }
    if let Some(cwd) = non_empty(s.current_cwd.as_deref()) {
        lines.push(format!("Cwd: {cwd}"));
    }
    lines.join("\n")
}

/// The session's terminal title as its agent means it, trimmed: without a
/// leading title glyph from the agent's descriptor in `agents` and the
/// whitespace after it, and `None` when what is left is blank or only the
/// agent's placeholder. A session with no agent, or whose descriptor is not
/// at hand, keeps its title as it stands.
pub(crate) fn session_title<'a>(
    s: &'a SessionSnapshot,
    agents: &[AgentDescriptor],
) -> Option<&'a str> {
    let title = non_empty(s.terminal_title.as_deref())?;
    let Some(rules) = s
        .agent
        .as_ref()
        .and_then(|agent| descriptor_for(agents, agent))
        .map(|descriptor| &descriptor.title)
    else {
        return Some(title);
    };
    let name = strip_title_glyph(title, &rules.glyphs);
    (!name.is_empty() && rules.placeholder.as_deref() != Some(name)).then_some(name)
}

/// `title` without a leading glyph from `glyphs` and the whitespace after
/// it; a glyph with no whitespace after it, and not the whole title, stays.
fn strip_title_glyph<'a>(title: &'a str, glyphs: &[CharRange]) -> &'a str {
    let mut chars = title.chars();
    let Some(first) = chars.next() else {
        return title;
    };
    let rest = chars.as_str();
    let leads = glyphs.iter().any(|range| range.contains(first));
    if leads && (rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        rest.trim_start()
    } else {
        title
    }
}

/// A leaf's hover text: [`label_tooltip`], then a line per agent chip, its
/// tip (else its text). Only the leaf carries them, since its pane header
/// already shows the chips saying the same.
pub fn leaf_tooltip(s: &SessionSnapshot, agents: &[AgentDescriptor]) -> String {
    let mut text = label_tooltip(s, agents);
    for chip in &s.agent_chips {
        text.push('\n');
        text.push_str(chip_tip(chip));
    }
    text
}

/// What an agent chip says on hover: its tip, else its text.
pub fn chip_tip(chip: &SessionChip) -> &str {
    chip.tip.as_deref().unwrap_or(&chip.text)
}

/// The line under a leaf's label: a plain shell's folder with `home` shown
/// as `~`; a single repo's branch, as `repo:branch` when `with_repo`; every
/// member as `repo:branch`, joined by ` · `, for a workspace. A blank repo
/// name or branch is left out, and a member with both blank is skipped.
/// `None` when there is nothing to show.
pub fn subline(s: &SessionSnapshot, home: Option<&str>, with_repo: bool) -> Option<String> {
    if s.mode == SessionMode::PlainShell {
        return non_empty(s.current_cwd.as_deref()).map(|cwd| home_as_tilde(cwd, home));
    }
    let parts: Vec<String> = match s.members.as_slice() {
        [only] if !with_repo && s.workspace_id.is_none() => non_empty(Some(&only.branch))
            .map(str::to_owned)
            .into_iter()
            .collect(),
        members => members
            .iter()
            .filter_map(|m| repo_and_branch(&m.repo_name, &m.branch))
            .collect(),
    };
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// `repo:branch`, or whichever of the two is not blank; `None` when both are.
fn repo_and_branch(repo: &str, branch: &str) -> Option<String> {
    match (non_empty(Some(repo)), non_empty(Some(branch))) {
        (Some(repo), Some(branch)) => Some(format!("{repo}:{branch}")),
        (Some(part), None) | (None, Some(part)) => Some(part.to_owned()),
        (None, None) => None,
    }
}

/// `path` normalised, with a leading `home` folder written as `~`.
fn home_as_tilde(path: &str, home: Option<&str>) -> String {
    let path = normalize_fs_path(path);
    let Some(home) = home.map(normalize_fs_path).filter(|home| !home.is_empty()) else {
        return path;
    };
    let under_home = path.get(..home.len()).is_some_and(|head| {
        let rest = &path[home.len()..];
        head.eq_ignore_ascii_case(&home) && (rest.is_empty() || rest.starts_with(['\\', '/']))
    });
    if under_home {
        format!("~{}", &path[home.len()..])
    } else {
        path
    }
}

/// The runtime tag: the program the pane runs — the session's
/// `program_name` when the daemon knows one, else the agent's tag for an
/// agent session (its id when no descriptor is at hand) — and nothing for a
/// plain shell with no known program.
pub fn runtime_label(s: &SessionSnapshot, agents: &[AgentDescriptor]) -> Option<String> {
    s.program_name
        .clone()
        .filter(|p| !p.is_empty())
        .or_else(|| {
            s.agent
                .as_ref()
                .filter(|_| s.mode != SessionMode::PlainShell)
                .map(|agent| {
                    descriptor_for(agents, agent)
                        .map_or_else(|| agent.0.clone(), |descriptor| descriptor.tag.clone())
                })
        })
}

/// The agent driving `s`, when it is the program up in the pane: the session
/// names one, its descriptor is at hand, and the runtime tag showing
/// (`runtime`) is the descriptor's own tag — not the shell's label that sits
/// on screen before a hosted agent starts and after it ends.
#[must_use]
pub(crate) fn agent_up<'a>(
    s: &SessionSnapshot,
    runtime: Option<&str>,
    agents: &'a [AgentDescriptor],
) -> Option<&'a AgentDescriptor> {
    let descriptor = descriptor_for(agents, s.agent.as_ref()?)?;
    (runtime == Some(descriptor.tag.as_str())).then_some(descriptor)
}

/// Whether a session's runtime tag wears its agent's accent: the agent is
/// up and its descriptor asks for one.
#[must_use]
pub(crate) fn runtime_accent(
    s: &SessionSnapshot,
    runtime: Option<&str>,
    agents: &[AgentDescriptor],
) -> bool {
    agent_up(s, runtime, agents).is_some_and(|descriptor| descriptor.accent == AccentToken::Claude)
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
pub(crate) mod tests {
    use super::*;
    use crate::agent_options::test_selection as agent_selection;
    use crate::tabs::tests::{pane, tab};
    use protocol::{
        AgentSelection, AttentionReason, GridNode, OptionValue, SessionMember, SplitDirection,
    };
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    #[test]
    fn no_hovered_row_once_the_pointer_has_left_the_window() {
        assert_eq!(shown_hovered_row(Some("repo:r1"), false), None);
        assert_eq!(shown_hovered_row(None, false), None);
    }

    #[test]
    fn the_hovered_row_shows_while_the_window_is_hovered() {
        assert_eq!(shown_hovered_row(Some("repo:r1"), true), Some("repo:r1"));
        assert_eq!(shown_hovered_row(None, true), None);
    }

    pub(crate) fn repo(id: &str, path: &str) -> RepoEntry {
        serde_json::from_value(json!({ "id": id, "name": id, "path": path })).expect("repo fixture")
    }

    #[test]
    fn sidebar_keeps_repos_and_workspaces_in_daemon_order() {
        let mut model = SidebarModel::default();
        assert!(model.repos().is_empty() && model.workspaces().is_empty());
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Repos {
                repos: vec![repo("zeta", "C:/z"), repo("alpha", "C:/a")],
            },
        );
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Workspaces {
                workspaces: vec![workspace("w2", &["zeta"]), workspace("w1", &["alpha"])],
            },
        );
        let repos: Vec<&str> = model.repos().iter().map(|r| r.id.as_str()).collect();
        let workspaces: Vec<&str> = model.workspaces().iter().map(|w| w.id.as_str()).collect();
        assert_eq!(repos, ["zeta", "alpha"], "not sorted by name");
        assert_eq!(workspaces, ["w2", "w1"]);
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Repos { repos: Vec::new() },
        );
        assert!(model.repos().is_empty(), "a new snapshot replaces the old");
    }

    #[test]
    fn clear_daemon_state_keeps_the_ui_state() {
        let mut model = SidebarModel::default();
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Repos {
                repos: vec![repo("r1", "C:/r1")],
            },
        );
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Workspaces {
                workspaces: vec![workspace("w1", &["r1"])],
            },
        );
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Sessions {
                sessions: vec![in_repo("s1", "r1")],
            },
        );
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::SessionsReordered {
                container_id: "r1".to_owned(),
                ordered_ids: vec!["s1".to_owned()],
            },
        );
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Attention {
                session_id: "s1".to_owned(),
                reason: AttentionReason::AwaitingInput,
            },
        );
        model.set_focused_session(Some("s1"));
        assert!(model.set_active_tab(Some("t1")), "the UI value is set");

        model.clear_host(&crate::target::HostKey::Local);

        assert!(model.repos().is_empty(), "the old daemon's repos are gone");
        assert!(!model.repos_loaded(), "the next host's list is awaited");
        assert!(model.workspaces().is_empty(), "and its workspaces");
        assert!(model.local_sessions().is_empty(), "and its sessions");
        assert!(
            model.local.session_order.is_empty(),
            "and its session order"
        );
        assert!(model.local.attention.is_empty(), "and its attention marks");
        assert!(model.focused.is_none());
        assert_eq!(
            model.ui_state().active_tab_id.as_deref(),
            Some("t1"),
            "the layout is this PC's and stays"
        );
    }

    fn remote_host(name: &str) -> crate::target::HostKey {
        crate::target::HostKey::Remote(name.to_owned())
    }

    fn sessions_msg(ids: &[&str]) -> DaemonMessage {
        DaemonMessage::Sessions {
            sessions: ids.iter().map(|id| session(id)).collect(),
        }
    }

    fn ids<'a>(sessions: impl Iterator<Item = &'a SessionSnapshot>) -> Vec<&'a str> {
        sessions.map(|s| s.id.as_str()).collect()
    }

    #[test]
    fn a_hosts_session_list_replaces_only_its_own_slice() {
        let local = crate::target::HostKey::Local;
        let remote = remote_host("box");
        let mut model = SidebarModel::default();
        model.apply(&local, &sessions_msg(&["a1", "a2"]));
        model.apply(&remote, &sessions_msg(&["b1"]));
        model.apply(&remote, &sessions_msg(&["b2"]));
        model.apply(
            &remote,
            &DaemonMessage::SessionRemoved {
                session_id: "a1".to_owned(),
            },
        );

        assert_eq!(ids(model.local_sessions().iter()), ["a1", "a2"]);
        assert_eq!(ids(model.all_sessions()), ["a1", "a2", "b2"]);
    }

    #[test]
    fn a_hosts_repo_list_leaves_the_other_hosts_repos() {
        let remote = remote_host("box");
        let mut model = SidebarModel::default();
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Repos {
                repos: vec![repo("r1", "C:/r1")],
            },
        );
        model.apply(
            &remote,
            &DaemonMessage::Repos {
                repos: vec![repo("r2", "C:/r2")],
            },
        );

        assert_eq!(model.repos().len(), 1, "This PC's list stays");
        assert_eq!(
            model.find_repo("r1").map(|(host, _)| host),
            Some(crate::target::HostKey::Local)
        );
        assert_eq!(model.find_repo("r2").map(|(host, _)| host), Some(remote));
        assert!(model.find_repo("r3").is_none());
    }

    #[test]
    fn find_names_the_host_that_lists_the_item() {
        let remote = remote_host("box");
        let mut model = SidebarModel::default();
        model.apply(&crate::target::HostKey::Local, &sessions_msg(&["a1"]));
        model.apply(&remote, &sessions_msg(&["b1"]));
        model.apply(
            &remote,
            &DaemonMessage::Workspaces {
                workspaces: vec![workspace("w2", &["r2"])],
            },
        );

        assert_eq!(
            model
                .find_session("a1")
                .map(|(host, s)| (host, s.id.as_str())),
            Some((crate::target::HostKey::Local, "a1"))
        );
        assert_eq!(
            model.find_session("b1").map(|(host, _)| host),
            Some(remote.clone())
        );
        assert_eq!(model.session("b1").map(|s| s.id.as_str()), Some("b1"));
        assert_eq!(
            model.find_workspace("w2").map(|(host, _)| host),
            Some(remote)
        );
        assert!(model.find_session("zz").is_none());
    }

    #[test]
    fn all_sessions_go_local_first_then_remotes_in_the_order_they_came() {
        let mut model = SidebarModel::default();
        model.apply(&remote_host("second"), &sessions_msg(&["c1"]));
        model.apply(&remote_host("third"), &sessions_msg(&["d1"]));
        model.apply(&crate::target::HostKey::Local, &sessions_msg(&["a1"]));
        model.apply(&remote_host("second"), &sessions_msg(&["c2"]));

        assert_eq!(ids(model.all_sessions()), ["a1", "c2", "d1"]);
    }

    #[test]
    fn clear_host_forgets_only_that_hosts_slice() {
        let remote = remote_host("box");
        let mut model = SidebarModel::default();
        model.apply(&crate::target::HostKey::Local, &sessions_msg(&["a1"]));
        model.apply(&remote, &sessions_msg(&["b1"]));
        model.apply(&remote_host("other"), &sessions_msg(&["c1"]));
        model.set_agents(&remote, agent_registry::descriptors());

        model.clear_host(&remote);

        assert_eq!(ids(model.all_sessions()), ["a1", "c1"]);
        assert_eq!(model.agents(&remote), [], "its agents went too");
        model.apply(&remote, &sessions_msg(&["b2"]));
        assert_eq!(
            ids(model.all_sessions()),
            ["a1", "b2", "c1"],
            "it keeps its place"
        );
    }

    #[test]
    fn each_host_keeps_its_own_agents() {
        let mut model = SidebarModel::default();
        model.set_agents(
            &crate::target::HostKey::Local,
            agent_registry::descriptors(),
        );

        assert_ne!(model.agents(&crate::target::HostKey::Local), []);
        assert_eq!(model.agents(&remote_host("box")), []);
    }

    #[test]
    fn a_remote_hosts_container_keys_carry_its_host_key() {
        let remote = remote_host("box");
        let mut model = SidebarModel::default();
        let detached = DaemonMessage::Sessions {
            sessions: vec![in_repo("a1", "gone")],
        };
        model.apply(&crate::target::HostKey::Local, &detached);
        model.apply(
            &remote,
            &DaemonMessage::Sessions {
                sessions: vec![in_repo("b1", "gone")],
            },
        );
        model.toggle_container("box::detached");

        let containers = model.containers();
        assert_eq!(keys(&containers), ["detached", "box::detached"]);
        let folded: Vec<bool> = containers.iter().map(|c| c.collapsed).collect();
        assert_eq!(
            folded,
            [false, true],
            "each host's row folds by its own key"
        );
        assert_eq!(
            host_container_key(&crate::target::HostKey::Local, "cwd:C:/x"),
            "cwd:C:/x"
        );
        assert_eq!(host_container_key(&remote, "cwd:C:/x"), "box::cwd:C:/x");
    }

    pub(crate) fn workspace(id: &str, members: &[&str]) -> WorkspaceEntry {
        serde_json::from_value(json!({ "id": id, "name": id, "member_repo_ids": members }))
            .expect("workspace fixture")
    }

    pub(crate) fn session(id: &str) -> SessionSnapshot {
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

    pub(crate) fn in_repo(id: &str, repo_id: &str) -> SessionSnapshot {
        let mut s = session(id);
        s.members = vec![SessionMember {
            repo_id: repo_id.to_owned(),
            repo_name: repo_id.to_owned(),
            branch: "main".to_owned(),
            worktree_path: String::new(),
        }];
        s
    }

    pub(crate) fn shell(id: &str, cwd: Option<&str>, kind: SessionKind) -> SessionSnapshot {
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
                agents: &[],
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
    fn dir_container_banner_path_is_its_normalized_id() {
        let tree = Fixture::new(
            vec![],
            vec![],
            vec![shell("s", Some("C:/elsewhere/proj/"), SessionKind::Single)],
        )
        .build();
        let dir = tree.first().expect("DIR");
        assert_eq!(dir.kind, ContainerKind::Dir);
        assert_eq!(dir.path.as_deref(), Some("C:\\elsewhere\\proj"));
    }

    #[test]
    fn shell_container_banner_path_is_its_session_cwd() {
        let tree = Fixture::new(
            vec![],
            vec![],
            vec![shell("s", Some("C:\\elsewhere"), SessionKind::Standalone)],
        )
        .build();
        let sh = tree.first().expect("SH");
        assert_eq!(sh.kind, ContainerKind::Shell);
        assert_eq!(sh.path.as_deref(), Some("C:\\elsewhere"));

        let forward = Fixture::new(
            vec![],
            vec![],
            vec![shell(
                "s",
                Some("C:/elsewhere/proj"),
                SessionKind::Standalone,
            )],
        )
        .build();
        assert_eq!(
            forward.first().expect("SH").path.as_deref(),
            Some("C:\\elsewhere\\proj"),
            "a forward-slash cwd is normalized like a DIR's id"
        );
    }

    #[test]
    fn shell_container_without_a_cwd_has_no_banner_path() {
        let tree = Fixture::new(
            vec![],
            vec![],
            vec![shell("s", None, SessionKind::Standalone)],
        )
        .build();
        assert_eq!(tree.first().expect("SH").path, None);
    }

    #[test]
    fn repo_workspace_and_tab_containers_have_no_banner_path() {
        let tree = Fixture::new(
            vec![repo("r1", "D:\\src\\r1")],
            vec![workspace("w1", &[])],
            vec![],
        )
        .build();
        assert_eq!(keys(&tree), ["ws:w1", "repo:r1"]);
        assert!(tree.iter().all(|c| c.path.is_none()));

        let tabs = [tab("t1", &pane("p1", Some("s1")))];
        let tab_tree = build_tab_containers(
            &tabs,
            &[session("s1")],
            &[],
            &TabRollups {
                session_order: &HashMap::new(),
                attention: &HashSet::new(),
                unseen: &HashSet::new(),
            },
            &BTreeSet::new(),
        );
        let tab_container = tab_tree.first().expect("TAB");
        assert_eq!(tab_container.kind, ContainerKind::Tab);
        assert_eq!(tab_container.path, None);
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
        assert_eq!(display_label(&s, &[]), "mine");
    }

    #[test]
    fn label_uses_cwd_leaf_for_plain_shell_only() {
        let mut s = shell("id", Some("C:\\work\\proj\\"), SessionKind::Single);
        s.terminal_title = Some("vim".to_owned());
        assert_eq!(display_label(&s, &[]), "proj");
        let mut agent = session("id");
        agent.current_cwd = Some("C:\\work\\proj".to_owned());
        assert_eq!(display_label(&agent, &[]), "id");
    }

    #[test]
    fn label_uses_terminal_title_when_not_noisy() {
        let mut s = session("id");
        s.terminal_title = Some("Fixing the bug".to_owned());
        assert_eq!(display_label(&s, &[]), "Fixing the bug");
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
            assert_eq!(display_label(&s, &[]), "fallback", "title {title}");
        }
    }

    #[test]
    fn label_falls_back_to_session_label() {
        let mut s = session("id");
        s.label = "repo · main".to_owned();
        assert_eq!(display_label(&s, &[]), "repo · main");
    }

    #[test]
    fn label_falls_back_to_runtime_then_id() {
        let mut s = shell("id", None, SessionKind::Standalone);
        s.label = "   ".to_owned();
        s.program_name = Some("pwsh".to_owned());
        assert_eq!(display_label(&s, &[]), "pwsh");
        s.program_name = None;
        assert_eq!(display_label(&s, &[]), "id");
    }

    #[test]
    fn display_label_falls_back_to_the_descriptor_tag() {
        let mut s = session("id");
        s.agent = Some(AgentId("test-agent".to_owned()));
        s.label = String::new();
        s.user_label = None;
        s.terminal_title = None;
        s.program_name = None;
        assert_eq!(
            display_label(&s, &[test_descriptor()]),
            "test",
            "the descriptor's tag, not the agent's id"
        );
        assert_eq!(label_tooltip(&s, &[test_descriptor()]), "test");
        assert_eq!(
            display_label(&s, &[]),
            "test-agent",
            "no descriptor at hand: the agent id"
        );
    }

    #[test]
    fn tooltip_is_the_label_alone_when_nothing_differs() {
        assert_eq!(label_tooltip(&session("id"), &[]), "id");
    }

    #[test]
    fn tooltip_names_the_session_label_when_it_differs() {
        let mut s = session("id");
        s.user_label = Some("mine".to_owned());
        s.label = "repo · main".to_owned();
        assert_eq!(label_tooltip(&s, &[]), "mine\nSession: repo · main");
    }

    #[test]
    fn tooltip_names_the_terminal_title_when_it_differs() {
        let mut s = session("id");
        s.label = "repo · main".to_owned();
        s.terminal_title = Some("pwsh".to_owned());
        assert_eq!(label_tooltip(&s, &[]), "repo · main\nTerminal title: pwsh");
        s.terminal_title = Some("Building".to_owned());
        assert_eq!(
            label_tooltip(&s, &[]),
            "Building\nSession: repo · main",
            "a title that is the label is not repeated"
        );
    }

    #[test]
    fn tooltip_ends_with_the_cwd() {
        let mut s = shell("id", Some("D:/src/app"), SessionKind::Standalone);
        s.terminal_title = Some("vim".to_owned());
        assert_eq!(
            label_tooltip(&s, &[]),
            "app\nSession: id\nTerminal title: vim\nCwd: D:/src/app"
        );
    }

    /// A Claude session labelled `repo · main` whose terminal title is `title`.
    fn titled(title: &str) -> SessionSnapshot {
        let mut s = session("id");
        s.label = "repo · main".to_owned();
        s.terminal_title = Some(title.to_owned());
        s
    }

    #[test]
    fn label_strips_the_agents_title_glyph() {
        let agents = [claude_descriptor()];
        for (title, name) in [
            ("◑ main.rs greet function", "main.rs greet function"),
            ("⠋ Fixing", "Fixing"),
            ("✳  Two spaces\tand a tab", "Two spaces\tand a tab"),
            ("  ◐\tLeading blanks  ", "Leading blanks"),
        ] {
            assert_eq!(
                display_label(&titled(title), &agents),
                name,
                "title {title:?}"
            );
        }
    }

    #[test]
    fn label_strips_every_glyph_in_the_descriptor_ranges() {
        let descriptor = claude_descriptor();
        let ends = descriptor
            .title
            .glyphs
            .iter()
            .flat_map(|range| [range.first, range.last]);
        let agents = [descriptor.clone()];
        for glyph in ends.chain(['◐', '◒', '⡀']) {
            let title = format!("{glyph} Fixing");
            assert_eq!(
                display_label(&titled(&title), &agents),
                "Fixing",
                "title {title:?}"
            );
            assert_eq!(
                display_label(&titled(&glyph.to_string()), &agents),
                "repo · main",
                "a glyph alone names nothing: {glyph:?}"
            );
        }
    }

    #[test]
    fn label_falls_through_when_the_title_is_only_the_placeholder() {
        let agents = [claude_descriptor()];
        for title in [
            "✳ Claude Code",
            "◐ Claude Code",
            "Claude Code",
            "  ✳   Claude Code  ",
        ] {
            assert_eq!(
                display_label(&titled(title), &agents),
                "repo · main",
                "title {title:?}"
            );
        }
        let mut unlabelled = titled("✳ Claude Code");
        unlabelled.label = String::new();
        assert_eq!(
            display_label(&unlabelled, &agents),
            "claude",
            "then the runtime tag"
        );
        for (title, name) in [
            ("✳ Claude Code rocks", "Claude Code rocks"),
            ("✳ claude code", "claude code"),
        ] {
            assert_eq!(
                display_label(&titled(title), &agents),
                name,
                "title {title:?}"
            );
        }
    }

    #[test]
    fn label_keeps_a_glyph_lead_title_for_an_agent_without_title_rules() {
        let mut s = titled("✳ Claude Code");
        s.agent = Some(AgentId("test-agent".to_owned()));
        assert_eq!(display_label(&s, &[test_descriptor()]), "✳ Claude Code");
        let unknown = titled("◑ Fixing");
        assert_eq!(
            display_label(&unknown, &[]),
            "◑ Fixing",
            "no descriptor at hand"
        );
        let mut no_agent = titled("◑ Fixing");
        no_agent.agent = None;
        assert_eq!(display_label(&no_agent, &[claude_descriptor()]), "◑ Fixing");
    }

    #[test]
    fn label_does_not_strip_a_glyph_without_the_following_space() {
        let agents = [claude_descriptor()];
        for title in ["✳x", "◑main.rs", "⠋-Fixing"] {
            assert_eq!(
                display_label(&titled(title), &agents),
                title,
                "title {title:?}"
            );
        }
    }

    #[test]
    fn tooltip_leaves_out_a_placeholder_title() {
        let agents = [claude_descriptor()];
        assert_eq!(
            label_tooltip(&titled("✳ Claude Code"), &agents),
            "repo · main"
        );
        assert_eq!(
            label_tooltip(&titled("◑ Fixing"), &agents),
            "Fixing\nSession: repo · main",
            "the stripped title is the name, so it is not repeated"
        );
        let mut named = titled("◑ Fixing");
        named.user_label = Some("mine".to_owned());
        assert_eq!(
            label_tooltip(&named, &agents),
            "mine\nSession: repo · main\nTerminal title: Fixing"
        );
    }

    /// The chip the daemon evaluates for a `DeepSeek` Claude session.
    fn deepseek_chip() -> SessionChip {
        SessionChip {
            text: "DeepSeek".to_owned(),
            tip: Some("Runs on DeepSeek".to_owned()),
        }
    }

    #[test]
    fn a_leaf_shows_the_snapshots_agent_chips() {
        let mut s = session("id");
        s.agent_chips = vec![deepseek_chip()];
        let listed = leaf(&s, &HashSet::new(), &HashSet::new(), false, &[]);
        assert_eq!(listed.chips, [deepseek_chip()]);
        assert_eq!(listed.tooltip, "id\nRuns on DeepSeek");
        assert_eq!(
            label_tooltip(&s, &[]),
            "id",
            "the pane header says it in its chip, not in the title tip"
        );

        let sh = shell("sh", Some("D:/src/app"), SessionKind::Standalone);
        let listed = leaf(&sh, &HashSet::new(), &HashSet::new(), false, &[]);
        assert_eq!(listed.chips, [], "a shell shows none");
    }

    #[test]
    fn an_anthropic_claude_leaf_has_no_provider() {
        let s = session("id");
        assert_eq!(s.agent_chips, []);
        let listed = leaf(&s, &HashSet::new(), &HashSet::new(), false, &[]);
        assert_eq!(listed.chips, []);
        assert_eq!(listed.tooltip, "id", "no line is added");
        assert_eq!(leaf_tooltip(&s, &[]), label_tooltip(&s, &[]));
    }

    #[test]
    fn a_chip_without_a_tip_hovers_as_its_text() {
        let mut s = session("id");
        s.agent_chips = vec![SessionChip {
            text: "mistral".to_owned(),
            tip: None,
        }];
        let listed = leaf(&s, &HashSet::new(), &HashSet::new(), false, &[]);
        assert_eq!(listed.tooltip, "id\nmistral");
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
        agent.agent = Some(protocol::AgentId("claude".to_owned()));
        agent.program_name = Some("node".to_owned());
        assert_eq!(runtime_label(&agent, &[]).as_deref(), Some("node"));
        let mut sh = shell("s", None, SessionKind::Standalone);
        assert_eq!(runtime_label(&sh, &[]), None);
        sh.program_name = Some("pwsh".to_owned());
        assert_eq!(runtime_label(&sh, &[]).as_deref(), Some("pwsh"));
    }

    #[test]
    fn runtime_label_follows_program_name_for_an_agent_session() {
        let mut agent = session("a");
        agent.agent = Some(protocol::AgentId("claude".to_owned()));
        agent.program_name = Some("pwsh".to_owned());
        assert_eq!(
            runtime_label(&agent, &[claude_descriptor()]).as_deref(),
            Some("pwsh"),
            "the shell Claude left behind"
        );
        agent.program_name = None;
        assert_eq!(
            runtime_label(&agent, &[claude_descriptor()]).as_deref(),
            Some("claude"),
            "no known program: the agent"
        );
        let mut sh = shell("s", None, SessionKind::Standalone);
        sh.program_name = Some("pwsh".to_owned());
        assert_eq!(runtime_label(&sh, &[]).as_deref(), Some("pwsh"));
        let programless = shell("s2", None, SessionKind::Standalone);
        assert_eq!(
            runtime_label(&programless, &[]),
            None,
            "a shell with no program"
        );
    }

    #[test]
    fn runtime_label_falls_back_to_the_descriptor_tag() {
        let mut agent = session("a");
        agent.agent = Some(AgentId("test-agent".to_owned()));
        assert_eq!(
            runtime_label(&agent, &[test_descriptor()]).as_deref(),
            Some("test"),
            "the descriptor's tag names the runtime"
        );
        assert_eq!(
            runtime_label(&agent, &[]).as_deref(),
            Some("test-agent"),
            "no descriptor at hand: the agent id"
        );
    }

    #[test]
    fn accent_follows_the_descriptor_not_the_text() {
        let mut agent = session("a");
        agent.agent = Some(AgentId("test-agent".to_owned()));
        let mut accented = test_descriptor();
        accented.accent = AccentToken::Claude;
        let listed = leaf(&agent, &HashSet::new(), &HashSet::new(), false, &[accented]);
        assert!(listed.accent, "a descriptor asking for the accent wears it");
        assert_eq!(listed.runtime.as_deref(), Some("test"), "the tag names it");
        let listed = leaf(
            &agent,
            &HashSet::new(),
            &HashSet::new(),
            false,
            &[test_descriptor()],
        );
        assert!(!listed.accent, "a default accent stays plain");
        let mut left = session("a");
        left.agent = Some(protocol::AgentId("claude".to_owned()));
        left.program_name = Some("pwsh".to_owned());
        let listed = leaf(
            &left,
            &HashSet::new(),
            &HashSet::new(),
            false,
            &[claude_descriptor()],
        );
        assert!(
            !listed.accent,
            "the shell Claude left behind keeps the plain tag"
        );
    }

    fn model_with(sessions: Vec<SessionSnapshot>) -> SidebarModel {
        let mut model = SidebarModel::new(UiState::default());
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Repos {
                repos: vec![repo("r1", "D:\\r1")],
            },
        );
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Sessions { sessions },
        );
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
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Attention {
                session_id: id.to_owned(),
                reason: AttentionReason::AwaitingInput,
            },
        );
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
            model.apply(
                &crate::target::HostKey::Local,
                &DaemonMessage::SessionUpdated {
                    session: updated,
                    request_id: None,
                },
            );
            assert_eq!(attention_of(&model, "a"), !cleared, "status {status:?}");
        }
    }

    fn excluded_in_repo(id: &str) -> SessionSnapshot {
        let mut s = in_repo(id, "r1");
        s.busy_excluded = true;
        s
    }

    #[test]
    fn an_excluded_session_leaf_has_no_attention_highlight() {
        let mut model = model_with(vec![excluded_in_repo("a")]);
        flag(&mut model, "a");
        let leaf = model
            .containers()
            .into_iter()
            .flat_map(|c| c.leaves)
            .find(|l| l.id == "a")
            .expect("a is listed");
        assert!(leaf.busy_excluded, "the leaf carries the flag");
        assert!(leaf.attention, "the raw attention stays for Needs You");
        assert!(!leaf.shows_attention(), "no `!` on an excluded leaf");
    }

    #[test]
    fn an_excluded_leaf_does_not_roll_attention_up_to_its_container() {
        let mut model = model_with(vec![excluded_in_repo("a"), in_repo("b", "r1")]);
        flag(&mut model, "a");
        let rolled = |m: &SidebarModel| m.containers().first().is_some_and(|c| c.attention);
        assert!(!rolled(&model), "the excluded leaf's attention stays put");
        flag(&mut model, "b");
        assert!(rolled(&model), "a counted leaf still rolls up");

        let mut excluded = session("s1");
        excluded.busy_excluded = true;
        let tabs = [tab("t1", &pane("p1", Some("s1")))];
        let attention: HashSet<String> = ["s1".to_owned()].into();
        let tree = build_tab_containers(
            &tabs,
            &[excluded],
            &[],
            &TabRollups {
                session_order: &HashMap::new(),
                attention: &attention,
                unseen: &HashSet::new(),
            },
            &BTreeSet::new(),
        );
        assert!(
            tree.iter().all(|c| !c.attention),
            "the tab container skips it too"
        );
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
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::SessionUpdated {
                session: updated,
                request_id: None,
            },
        );
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

    /// `session` goes idle, with `background` work still running or not.
    fn set_idle(model: &mut SidebarModel, session: &SessionSnapshot, background: bool) {
        let mut updated = session.clone();
        updated.status = SessionStatus::Idle;
        updated.background = background;
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::SessionUpdated {
                session: updated,
                request_id: None,
            },
        );
    }

    fn leaf_background(model: &SidebarModel, id: &str) -> Option<bool> {
        model
            .containers()
            .iter()
            .flat_map(|c| c.leaves.iter())
            .find(|l| l.id == id)
            .map(|l| l.background)
    }

    #[test]
    fn a_turn_ending_with_background_is_not_unseen() {
        let a = in_repo("a", "r1");
        let mut model = model_with(vec![a.clone(), in_repo("b", "r1")]);
        model.set_focused_session(Some("b"));
        set_status(&mut model, &a, SessionStatus::Working);
        set_idle(&mut model, &a, true);
        assert!(!model.is_unseen("a"), "the turn's work is still running");
        assert_eq!(leaf_background(&model, "a"), Some(true));
    }

    #[test]
    fn background_ending_marks_the_session_unseen() {
        let a = in_repo("a", "r1");
        let mut model = model_with(vec![a.clone(), in_repo("b", "r1")]);
        model.set_focused_session(Some("b"));
        set_status(&mut model, &a, SessionStatus::Working);
        set_idle(&mut model, &a, true);
        set_idle(&mut model, &a, false);
        assert!(model.is_unseen("a"), "the background work ended the turn");
        assert_eq!(leaf_background(&model, "a"), Some(false));

        let mut model = model_with(vec![a.clone(), in_repo("b", "r1")]);
        model.set_focused_session(Some("a"));
        set_status(&mut model, &a, SessionStatus::Working);
        set_idle(&mut model, &a, true);
        set_idle(&mut model, &a, false);
        assert!(!model.is_unseen("a"), "it ended in the focused pane");
    }

    #[test]
    fn turn_ending_across_a_relist_is_unseen() {
        let a = in_repo("a", "r1");
        let mut model = model_with(vec![a.clone()]);
        set_status(&mut model, &a, SessionStatus::Working);
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Sessions {
                sessions: vec![a.clone()],
            },
        );
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
                "removed" => model.apply(
                    &crate::target::HostKey::Local,
                    &DaemonMessage::SessionRemoved {
                        session_id: "a".to_owned(),
                    },
                ),
                _ => model.apply(
                    &crate::target::HostKey::Local,
                    &DaemonMessage::Sessions {
                        sessions: Vec::new(),
                    },
                ),
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
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::SessionUpdated {
                session: in_repo("b", "r1"),
                request_id: None,
            },
        );
        assert_eq!(model.local_sessions().len(), 2);
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::SessionRemoved {
                session_id: "a".to_owned(),
            },
        );
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
    fn ui_font_family_round_trips_and_defaults_to_none() {
        assert_eq!(UiState::default().ui_font_family, None);
        let dir = TestDir::new("ui-font");
        let state = UiState {
            ui_font_family: Some("Inter".to_owned()),
            ..UiState::default()
        };
        save_ui_state(&dir.0, &state).expect("save");
        assert_eq!(
            load_ui_state(&dir.0).ui_font_family.as_deref(),
            Some("Inter")
        );

        let older: UiState = serde_json::from_value(json!({ "sidebar_width": 300.0 }))
            .expect("a file from before the UI font was saved");
        assert_eq!(older.ui_font_family, None, "absent: the default family");
    }

    #[test]
    fn ui_state_without_ui_text_size_loads_0() {
        assert_eq!(UiState::default().ui_text_size, 0);
        let older: UiState = serde_json::from_value(json!({ "sidebar_width": 300.0 }))
            .expect("a file from before the UI text size was saved");
        assert_eq!(older.ui_text_size, 0, "absent: no offset");
    }

    #[test]
    fn ui_state_round_trips_ui_text_size() {
        let dir = TestDir::new("ui-text-size");
        let state = UiState {
            ui_text_size: -2,
            ..UiState::default()
        };
        save_ui_state(&dir.0, &state).expect("save");
        assert_eq!(load_ui_state(&dir.0).ui_text_size, -2);
        let text = std::fs::read_to_string(dir.0.join(UI_FILE)).expect("the saved layout");
        let saved: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(saved["ui_text_size"], -2, "saved as an integer");
    }

    #[test]
    fn an_out_of_range_ui_text_size_clamps_on_load() {
        let load = |value: serde_json::Value| -> UiState {
            serde_json::from_value(json!({ "sidebar_width": 300.0, "ui_text_size": value }))
                .expect("a UI text size never fails the file")
        };
        assert_eq!(load(json!(9)).ui_text_size, 4, "above: the largest");
        assert_eq!(load(json!(-50)).ui_text_size, -2, "below: the smallest");
        assert_eq!(load(json!(100_000)).ui_text_size, 4, "past an i8");
        assert_eq!(load(json!(u64::MAX)).ui_text_size, 4, "past an i64");
        assert_eq!(load(json!(3)).ui_text_size, 3, "in range: kept");
        let bad = load(json!("big"));
        assert_eq!(bad.ui_text_size, 0, "not a whole number: no offset");
        assert!(
            (bad.sidebar_width - 300.0).abs() < f32::EPSILON,
            "the rest of the file loads"
        );
    }

    #[test]
    fn ui_state_without_ui_font_weight_loads_400() {
        assert_eq!(UiState::default().ui_font_weight, 400);
        let older: UiState = serde_json::from_value(json!({ "sidebar_width": 300.0 }))
            .expect("a file from before the UI weight was saved");
        assert_eq!(older.ui_font_weight, 400, "absent: Regular");
    }

    #[test]
    fn ui_state_round_trips_ui_font_weight() {
        let dir = TestDir::new("ui-font-weight");
        let state = UiState {
            ui_font_weight: 600,
            ..UiState::default()
        };
        save_ui_state(&dir.0, &state).expect("save");
        assert_eq!(load_ui_state(&dir.0).ui_font_weight, 600);
        let text = std::fs::read_to_string(dir.0.join(UI_FILE)).expect("the saved layout");
        let saved: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(saved["ui_font_weight"], 600, "saved as a number");
    }

    #[test]
    fn an_unknown_ui_font_weight_loads_400() {
        let load = |value: serde_json::Value| -> UiState {
            serde_json::from_value(json!({ "sidebar_width": 300.0, "ui_font_weight": value }))
                .expect("a UI weight never fails the file")
        };
        assert_eq!(load(json!(500)).ui_font_weight, 500, "offered: kept");
        for bad in [
            json!(450),
            json!(700),
            json!(-400),
            json!(100_000),
            json!(500.5),
            json!("bold"),
            serde_json::Value::Null,
        ] {
            assert_eq!(load(bad.clone()).ui_font_weight, 400, "{bad}");
        }
        assert!(
            (load(json!("bold")).sidebar_width - 300.0).abs() < f32::EPSILON,
            "the rest of the file loads"
        );
    }

    #[test]
    fn ui_state_without_a_theme_key_loads_none() {
        assert_eq!(UiState::default().theme, None);
        let older: UiState = serde_json::from_value(json!({ "sidebar_width": 300.0 }))
            .expect("a file without a theme key");
        assert_eq!(older.theme, None, "absent: the built-in theme");
    }

    #[test]
    fn ui_state_round_trips_its_theme_key() {
        let dir = TestDir::new("theme-key");
        let state = UiState {
            theme: Some("late-night".to_owned()),
            ..UiState::default()
        };
        save_ui_state(&dir.0, &state).expect("save");
        assert_eq!(load_ui_state(&dir.0).theme.as_deref(), Some("late-night"));
        let text = std::fs::read_to_string(dir.0.join(UI_FILE)).expect("the saved layout");
        let saved: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(saved["theme"], "late-night");
    }

    #[test]
    fn ui_state_round_trips() {
        let dir = TestDir::new("round-trip");
        let state = UiState {
            sidebar_width: 333.0,
            sidebar_collapsed: true,
            activity: Activity::SourceControl,
            sidebar_view: SidebarView::Tabs,
            repo_filter: RepoFilter::Active,
            collapsed_containers: ["repo:r1".to_owned(), "detached".to_owned()].into(),
            active_tab_id: Some("t1".to_owned()),
            quick_shell_dir: Some("C:\\work".to_owned()),
            last_repo_dir: Some("D:\\src".to_owned()),
            last_vscode_dir: Some("E:\\workspaces".to_owned()),
            terminal_font: FontSettings {
                family: Some("JetBrains Mono".to_owned()),
                size: 15.0,
                weight: 700,
            },
            ui_font_family: Some("Inter".to_owned()),
            ui_text_size: 3,
            ui_font_weight: 500,
            theme: Some("my-theme".to_owned()),
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
            viewer_markdown_rendered: false,
            window: None,
            popouts: Vec::new(),
            general: GeneralSettings {
                copy_on_select: false,
                leaf_density: LeafDensity::Compact,
                sessions_under_repos: SessionsUnderRepos::Busy,
                repo_sort: RepoSort::Active,
                open_text_externally: true,
                check_updates: false,
            },
            title: TitleSettings {
                show_count: false,
                suffix: false,
                count: CountMode::Idle,
            },
            notifications: NotificationSettings {
                awaiting_input: false,
                finished: false,
                skip_when: NotifySkip::Never,
            },
            spawn: SpawnDefaults { trusted: true },
            last_target: None,
            saved_hosts: Vec::new(),
            host_session_counts: BTreeMap::new(),
            session_views: SessionViews {
                hosts: BTreeMap::from([(
                    "local".to_owned(),
                    BTreeMap::from([("s1".to_owned(), PaneView::Summary)]),
                )]),
            },
        };
        save_ui_state(&dir.0, &state).expect("first save");
        save_ui_state(&dir.0, &state).expect("save over the existing file");
        assert_eq!(load_ui_state(&dir.0), state);
        assert!(!dir.0.join(format!("{UI_FILE}.tmp")).exists());
    }

    /// `native-ui.json` as v0.5.1-alpha wrote it for the state its
    /// `ui_state_round_trips` built: every key that release saved, each
    /// away from its default, with a custom app accent and background.
    fn v0_5_1_alpha_ui_file() -> serde_json::Value {
        serde_json::json!({
            "sidebar_width": 333.0,
            "sidebar_collapsed": true,
            "activity": "source_control",
            "sidebar_view": "tabs",
            "repo_filter": "active",
            "collapsed_containers": ["detached", "repo:r1"],
            "active_tab_id": "t1",
            "quick_shell_dir": "C:\\work",
            "last_repo_dir": "D:\\src",
            "last_vscode_dir": "E:\\workspaces",
            "terminal_font": { "family": "JetBrains Mono", "size": 15.0, "bold": true },
            "ui_font_family": "Inter",
            "tab_font_sizes": { "t1": 20.0 },
            "source_control": {
                "pinned_repo": "r1",
                "collapsed": {},
                "changes_height": null,
                "history_list_height": {}
            },
            "app_appearance": {
                "accent_color": "#38bdf8",
                "terminal_background_color": "#f6f4ef",
                "terminal_frame_color": null
            },
            "recent_colors": ["#abcdef"],
            "diff_include_whitespace": false,
            "diff_highlight": false,
            "viewer_markdown_rendered": false,
            "window": null,
            "popouts": [],
            "general": {
                "copy_on_select": false,
                "leaf_density": "compact",
                "sessions_under_repos": "busy",
                "repo_sort": "active",
                "open_text_externally": true
            },
            "title": { "show_count": false, "suffix": false },
            "notifications": { "awaiting_input": false, "finished": false, "skip_when": "never" },
            "spawn": { "trusted": true },
            "last_target": null,
            "saved_hosts": [],
            "host_session_counts": {}
        })
    }

    /// The state `file` loads and migrates to, with its themes in `dir`.
    fn load_released(dir: &TestDir, file: &serde_json::Value) -> UiState {
        std::fs::create_dir_all(&dir.0).expect("create the config dir");
        std::fs::write(dir.0.join(UI_FILE), file.to_string()).expect("write");
        let mut ui = load_ui_state(&dir.0);
        let store = crate::theme_store::ThemeStore::new(dir.0.join(crate::theme_store::THEMES_DIR));
        assert!(
            crate::theme_store::migrate_app_colors(&mut ui, &store),
            "the colours move"
        );
        ui
    }

    /// Every value v0.5.1-alpha's file sets, as main holds it once the app
    /// colours moved into the theme `theme`.
    fn released_values(theme: Option<String>, general: GeneralSettings) -> UiState {
        UiState {
            sidebar_width: 333.0,
            sidebar_collapsed: true,
            activity: Activity::SourceControl,
            sidebar_view: SidebarView::Tabs,
            repo_filter: RepoFilter::Active,
            collapsed_containers: ["repo:r1".to_owned(), "detached".to_owned()].into(),
            active_tab_id: Some("t1".to_owned()),
            quick_shell_dir: Some("C:\\work".to_owned()),
            last_repo_dir: Some("D:\\src".to_owned()),
            last_vscode_dir: Some("E:\\workspaces".to_owned()),
            terminal_font: FontSettings {
                family: Some("JetBrains Mono".to_owned()),
                size: 15.0,
                weight: 700,
            },
            ui_font_family: Some("Inter".to_owned()),
            theme,
            tab_font_sizes: [("t1".to_owned(), 20.0)].into(),
            source_control: ScUiState {
                pinned_repo: Some("r1".to_owned()),
                ..ScUiState::default()
            },
            recent_colors: vec!["#abcdef".to_owned()],
            diff_include_whitespace: false,
            diff_highlight: false,
            viewer_markdown_rendered: false,
            general,
            title: TitleSettings {
                show_count: false,
                suffix: false,
                ..TitleSettings::default()
            },
            notifications: NotificationSettings {
                awaiting_input: false,
                finished: false,
                skip_when: NotifySkip::Never,
            },
            spawn: SpawnDefaults { trusted: true },
            ..UiState::default()
        }
    }

    fn released_general() -> GeneralSettings {
        GeneralSettings {
            copy_on_select: false,
            leaf_density: LeafDensity::Compact,
            sessions_under_repos: SessionsUnderRepos::Busy,
            repo_sort: RepoSort::Active,
            open_text_externally: true,
            ..GeneralSettings::default()
        }
    }

    #[test]
    fn ui_file_written_by_v0_5_1_alpha_loads_with_every_set_value_kept() {
        let dir = TestDir::new("v0-5-1-alpha");
        let ui = load_released(&dir, &v0_5_1_alpha_ui_file());
        let id = ui.theme.clone().expect("the migrated theme is selected");
        assert_eq!(ui, released_values(Some(id.clone()), released_general()));
        assert_eq!(ui.terminal_font.weight, 700, "bold: true reads as Bold");

        let store = crate::theme_store::ThemeStore::new(dir.0.join(crate::theme_store::THEMES_DIR));
        assert_eq!(store.name_of(&id).as_deref(), Some("My colours"));
        let theme = store.load(&id).expect("the migrated theme loads");
        assert_eq!(theme.accent, 0x38_bdf8);
        assert_eq!(appearance::packed(theme.terminal.bg), 0xf6_f4ef);

        save_ui_state(&dir.0, &ui).expect("save");
        let text = std::fs::read_to_string(dir.0.join(UI_FILE)).expect("the saved layout");
        let saved: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert!(saved.get("app_appearance").is_none(), "{saved}");
        assert_eq!(saved["theme"], id.as_str());
    }

    #[test]
    fn ui_file_written_by_v0_5_2_alpha_keeps_its_update_check_choice() {
        let dir = TestDir::new("v0-5-2-alpha");
        let mut file = v0_5_1_alpha_ui_file();
        file["general"]["check_updates"] = serde_json::Value::Bool(false);
        let ui = load_released(&dir, &file);
        let general = GeneralSettings {
            check_updates: false,
            ..released_general()
        };
        assert_eq!(ui, released_values(ui.theme.clone(), general));
        assert!(ui.theme.is_some());
    }

    fn pane_popout(tab_id: &str, pane_id: &str) -> SavedPopout {
        SavedPopout {
            key: PopoutKey::Pane {
                tab_id: tab_id.to_owned(),
                pane_id: pane_id.to_owned(),
            },
            window: None,
        }
    }

    #[test]
    fn ui_state_round_trips_its_popouts() {
        let dir = TestDir::new("popouts");
        let state = UiState {
            popouts: vec![
                SavedPopout {
                    key: PopoutKey::Session {
                        session_id: "s1".to_owned(),
                    },
                    window: Some(WindowState {
                        x: 40.0,
                        y: 50.0,
                        width: 900.0,
                        height: 600.0,
                        maximized: false,
                        display_uuid: Some("secondary".to_owned()),
                    }),
                },
                pane_popout("t1", "p1"),
            ],
            ..UiState::default()
        };
        save_ui_state(&dir.0, &state).expect("save");
        assert_eq!(load_ui_state(&dir.0), state);
        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.0.join(UI_FILE)).expect("read"))
                .expect("JSON");
        assert_eq!(
            file["popouts"][1],
            json!({ "key": { "kind": "pane", "tab_id": "t1", "pane_id": "p1" }, "window": null })
        );
    }

    #[test]
    fn session_views_without_the_key_or_with_a_malformed_one_load_empty() {
        let older: UiState = serde_json::from_value(json!({ "sidebar_collapsed": true }))
            .expect("a file from before views were remembered");
        assert_eq!(older.session_views, SessionViews::default());
        let dir = TestDir::new("bad-session-views");
        for views in [json!(null), json!(5), json!("s1"), json!(["s1"])] {
            let file = json!({ "sidebar_collapsed": true, "session_views": views });
            std::fs::write(dir.0.join(UI_FILE), file.to_string()).expect("write");
            let loaded = load_ui_state(&dir.0);
            assert!(
                loaded.sidebar_collapsed,
                "{views}: the rest of the file loads"
            );
            assert_eq!(
                loaded.session_views,
                SessionViews::default(),
                "{views}: no view is remembered"
            );
        }
    }

    #[test]
    fn session_views_keep_only_chat_and_summary_entries_of_well_formed_hosts() {
        let loaded: UiState = serde_json::from_value(json!({
            "session_views": {
                "local": { "s1": "chat", "s2": "terminal", "s3": "summary", "s4": 7, "s6": "Summary" },
                "box.tail.ts.net": { "s9": "chat" },
                "ssh:dev": "chat",
                "ssh:old": ["s1"],
            },
        }))
        .expect("a layout");
        let views = &loaded.session_views;
        assert_eq!(views.get("local", "s1"), PaneView::Chat);
        assert_eq!(views.get("local", "s3"), PaneView::Summary);
        for other in ["s2", "s4", "s5", "s6", "s9"] {
            assert_eq!(views.get("local", other), PaneView::Terminal, "{other}");
        }
        assert_eq!(views.get("box.tail.ts.net", "s9"), PaneView::Chat);
        assert_eq!(views.get("box.tail.ts.net", "s1"), PaneView::Terminal);
        assert_eq!(
            serde_json::to_value(views).expect("serialize"),
            json!({ "box.tail.ts.net": { "s9": "chat" }, "local": { "s1": "chat", "s3": "summary" } })
        );
    }

    #[test]
    fn a_remembered_summary_replaces_the_chat_and_a_terminal_forgets_it() {
        let mut views = SessionViews::default();
        assert!(views.remember("local", "s1", PaneView::Chat));
        assert!(views.remember("local", "s1", PaneView::Summary));
        assert!(
            !views.remember("local", "s1", PaneView::Summary),
            "no change"
        );
        assert_eq!(views.get("local", "s1"), PaneView::Summary);
        assert_eq!(
            serde_json::to_value(&views).expect("serialize"),
            json!({ "local": { "s1": "summary" } })
        );
        assert!(views.remember("local", "s1", PaneView::Terminal));
        assert_eq!(
            views,
            SessionViews::default(),
            "a host with none is left out"
        );
    }

    #[test]
    fn a_remembered_view_round_trips_through_the_file() {
        let dir = TestDir::new("session-views");
        let mut model = model_with(vec![session("s1"), session("s2")]);
        assert!(model.remember_session_view("local", "s1", PaneView::Chat));
        assert!(
            !model.remember_session_view("local", "s1", PaneView::Chat),
            "no change"
        );
        assert!(model.remember_session_view("local", "s2", PaneView::Chat));
        assert!(model.remember_session_view("local", "s2", PaneView::Terminal));
        assert!(model.remember_session_view("local", "s3", PaneView::Summary));
        save_ui_state(&dir.0, model.ui_state()).expect("save");
        let loaded = load_ui_state(&dir.0);
        assert_eq!(loaded.session_views.get("local", "s1"), PaneView::Chat);
        assert_eq!(loaded.session_views.get("local", "s2"), PaneView::Terminal);
        assert_eq!(loaded.session_views.get("local", "s3"), PaneView::Summary);
    }

    #[test]
    fn dropping_ended_sessions_forgets_only_the_current_hosts() {
        let mut model = model_with(vec![session("s1")]);
        model.remember_session_view("local", "s1", PaneView::Chat);
        model.remember_session_view("local", "gone", PaneView::Chat);
        model.remember_session_view("ssh:dev", "gone", PaneView::Chat);
        assert!(model.drop_ended_session_views("local"));
        assert!(
            !model.drop_ended_session_views("local"),
            "nothing more to drop"
        );
        assert_eq!(model.session_view("local", "s1"), PaneView::Chat);
        assert_eq!(model.session_view("local", "gone"), PaneView::Terminal);
        assert_eq!(
            model.session_view("ssh:dev", "gone"),
            PaneView::Chat,
            "another host's sessions are not judged by this host's list"
        );
        assert_eq!(
            serde_json::to_value(&model.ui_state().session_views).expect("serialize"),
            json!({ "local": { "s1": "chat" }, "ssh:dev": { "gone": "chat" } })
        );
    }

    #[test]
    fn a_hosts_views_survive_a_switch_to_another_host_and_a_save_there() {
        let dir = TestDir::new("session-views-hosts");
        let mut model = model_with(vec![session("a1")]);
        model.remember_session_view("local", "a1", PaneView::Chat);
        model.apply(&DaemonMessage::Sessions {
            sessions: vec![session("b1")],
        });
        model.remember_session_view("ssh:dev", "b1", PaneView::Chat);
        model.drop_ended_session_views("ssh:dev");
        save_ui_state(&dir.0, model.ui_state()).expect("save on host B");
        let mut back = SidebarModel::new(load_ui_state(&dir.0));
        back.apply(&DaemonMessage::Sessions {
            sessions: vec![session("a1")],
        });
        back.drop_ended_session_views("local");
        assert_eq!(back.session_view("local", "a1"), PaneView::Chat);
        assert_eq!(back.session_view("ssh:dev", "b1"), PaneView::Chat);
    }

    #[test]
    fn ui_state_without_popouts_loads_empty() {
        let older: UiState = serde_json::from_value(json!({ "sidebar_collapsed": true }))
            .expect("a file from before pop-outs were saved");
        assert_eq!(older.popouts, Vec::<SavedPopout>::new());
        let null: UiState =
            serde_json::from_value(json!({ "popouts": null })).expect("a null list");
        assert_eq!(null.popouts, Vec::<SavedPopout>::new());
    }

    #[test]
    fn ui_state_skips_an_undecodable_popout() {
        let dir = TestDir::new("bad-popouts");
        let file = json!({
            "sidebar_collapsed": true,
            "popouts": [
                { "key": { "kind": "pane", "tab_id": "t1", "pane_id": "p1" }, "window": null },
                { "key": { "kind": "window", "id": "w1" } },
                { "key": { "kind": "tab" } },
                "not a pop-out",
                { "key": { "kind": "pane", "tab_id": "t2", "pane_id": "p2" } },
            ],
        });
        std::fs::write(dir.0.join(UI_FILE), file.to_string()).expect("write");
        let loaded = load_ui_state(&dir.0);
        assert!(loaded.sidebar_collapsed, "the rest of the file still loads");
        assert_eq!(
            loaded.popouts,
            vec![pane_popout("t1", "p1"), pane_popout("t2", "p2")],
            "only the entries that decode stay, in their order"
        );
    }

    #[test]
    fn ui_state_with_a_non_list_popouts_keeps_every_other_setting() {
        let dir = TestDir::new("non-list-popouts");
        let not_lists = [
            json!(5),
            json!("t1"),
            json!(true),
            json!({ "key": { "kind": "tab", "tab_id": "t1" } }),
        ];
        for popouts in not_lists {
            let file = json!({ "sidebar_collapsed": true, "popouts": popouts });
            std::fs::write(dir.0.join(UI_FILE), file.to_string()).expect("write");
            let loaded = load_ui_state(&dir.0);
            assert!(
                loaded.sidebar_collapsed,
                "{popouts}: the rest of the file loads"
            );
            assert_eq!(
                loaded.popouts,
                Vec::<SavedPopout>::new(),
                "{popouts}: no pop-out reopens"
            );
        }
    }

    #[test]
    fn a_saved_popout_with_a_bad_place_keeps_its_key() {
        let dir = TestDir::new("bad-popout-place");
        let file = json!({
            "popouts": [
                { "key": { "kind": "pane", "tab_id": "t1", "pane_id": "p1" }, "window": "nowhere" },
                { "key": { "kind": "pane", "tab_id": "t2", "pane_id": "p2" }, "window": { "x": "left" } },
                { "key": { "kind": "nowhere" }, "window": null },
            ],
        });
        std::fs::write(dir.0.join(UI_FILE), file.to_string()).expect("write");
        assert_eq!(
            load_ui_state(&dir.0).popouts,
            vec![pane_popout("t1", "p1"), pane_popout("t2", "p2")],
            "a bad place loads as none; only the entry whose key does not decode is dropped"
        );
    }

    fn tailnet_host(name: &str, port: u16) -> SavedHost {
        SavedHost {
            kind: crate::connection_picker::HostKind::Tailnet,
            name: name.to_owned(),
            port,
            remote_command: None,
        }
    }

    fn ssh_host(name: &str, remote_command: Option<&str>) -> SavedHost {
        SavedHost {
            kind: crate::connection_picker::HostKind::Ssh,
            name: name.to_owned(),
            port: 0,
            remote_command: remote_command.map(str::to_owned),
        }
    }

    #[test]
    fn ssh_saved_host_round_trips_with_kind_and_remote_command() {
        let dir = TestDir::new("ssh-saved-hosts");
        let devbox = ssh_host("Me@DevBox:2222", Some("C:\\rt\\rustling-tulipd.exe bridge"));
        let state = UiState {
            last_target: Some(devbox.clone()),
            saved_hosts: vec![devbox, ssh_host("devbox", None)],
            ..UiState::default()
        };
        save_ui_state(&dir.0, &state).expect("save");
        assert_eq!(load_ui_state(&dir.0), state, "kept exactly as typed");
        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.0.join(UI_FILE)).expect("read"))
                .expect("JSON");
        assert_eq!(
            file["last_target"],
            json!({
                "kind": "ssh",
                "name": "Me@DevBox:2222",
                "remote_command": "C:\\rt\\rustling-tulipd.exe bridge",
            })
        );
        assert_eq!(
            file["saved_hosts"][1],
            json!({ "kind": "ssh", "name": "devbox" }),
            "no port and no remote command"
        );
    }

    #[test]
    fn an_unknown_kind_is_dropped_on_load() {
        let dir = TestDir::new("unknown-host-kind");
        let file = json!({
            "saved_hosts": [
                { "kind": "mosh", "name": "devbox" },
                { "kind": "ssh", "name": "devbox" },
                { "kind": "ssh", "name": "-oProxyCommand=calc" },
                { "kind": "ssh", "name": "devbox", "port": 22, "remote_command": " " },
            ],
            "last_target": { "kind": "mosh", "name": "devbox" },
        });
        std::fs::write(dir.0.join(UI_FILE), file.to_string()).expect("write");
        let loaded = load_ui_state(&dir.0);
        assert_eq!(
            loaded.saved_hosts,
            [ssh_host("devbox", None), ssh_host("devbox", None)],
            "an unknown kind and a refused host string are dropped; a stray port and a blank command are not kept"
        );
        assert_eq!(loaded.last_target, None, "an unknown kind is this PC");
    }

    #[test]
    fn a_record_without_kind_is_tailnet() {
        let dir = TestDir::new("host-kind-default");
        let host = json!({ "name": "box.tail1234.ts.net", "port": 9443 });
        let file = json!({ "saved_hosts": [host.clone()], "last_target": host });
        std::fs::write(dir.0.join(UI_FILE), file.to_string()).expect("write");
        let loaded = load_ui_state(&dir.0);
        let tailnet = tailnet_host("box.tail1234.ts.net", 9443);
        assert_eq!(loaded.saved_hosts, std::slice::from_ref(&tailnet));
        assert_eq!(loaded.last_target, Some(tailnet));
        save_ui_state(&dir.0, &loaded).expect("save");
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.0.join(UI_FILE)).expect("read"))
                .expect("JSON");
        assert_eq!(
            saved["saved_hosts"][0], host,
            "a tailnet record is written without a kind"
        );
    }

    #[test]
    fn last_target_and_saved_hosts_round_trip() {
        let dir = TestDir::new("saved-hosts");
        let box_host = tailnet_host("box.tail1234.ts.net", 9443);
        let state = UiState {
            last_target: Some(box_host.clone()),
            saved_hosts: vec![
                box_host.clone(),
                tailnet_host("laptop.tail1234.ts.net", 8787),
            ],
            ..UiState::default()
        };
        save_ui_state(&dir.0, &state).expect("save");
        assert_eq!(load_ui_state(&dir.0), state);
        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.0.join(UI_FILE)).expect("read"))
                .expect("JSON");
        assert_eq!(
            file["last_target"],
            json!({ "name": "box.tail1234.ts.net", "port": 9443 })
        );
        assert_eq!(
            file["saved_hosts"][1],
            json!({ "name": "laptop.tail1234.ts.net", "port": 8787 })
        );

        std::fs::write(dir.0.join(UI_FILE), r#"{ "last_target": null }"#).expect("write");
        let local = load_ui_state(&dir.0);
        assert_eq!(local.last_target, None, "null is this PC");
        assert_eq!(local.saved_hosts, [] as [SavedHost; 0]);
        std::fs::write(dir.0.join(UI_FILE), r#"{ "sidebar_collapsed": true }"#).expect("write");
        assert_eq!(load_ui_state(&dir.0).last_target, None, "absent is this PC");
    }

    #[test]
    fn host_session_counts_round_trip() {
        let dir = TestDir::new("host-session-counts");
        let state = UiState {
            host_session_counts: BTreeMap::from([
                ("box.tail1234.ts.net".to_owned(), 3),
                ("laptop.tail1234.ts.net".to_owned(), 0),
            ]),
            ..UiState::default()
        };
        save_ui_state(&dir.0, &state).expect("save");
        assert_eq!(load_ui_state(&dir.0), state);
        let file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.0.join(UI_FILE)).expect("read"))
                .expect("JSON");
        assert_eq!(
            file["host_session_counts"],
            json!({ "box.tail1234.ts.net": 3, "laptop.tail1234.ts.net": 0 })
        );
        std::fs::write(dir.0.join(UI_FILE), r#"{ "sidebar_collapsed": true }"#).expect("write");
        assert!(
            load_ui_state(&dir.0).host_session_counts.is_empty(),
            "absent is no host recorded"
        );
    }

    #[test]
    fn set_host_session_count_reports_a_change_only() {
        let mut model = SidebarModel::default();
        let host = "box.tail1234.ts.net";
        assert_eq!(model.host_session_count(host), None);
        assert!(model.set_host_session_count(host, 2), "first record");
        assert!(!model.set_host_session_count(host, 2), "same value");
        assert!(model.set_host_session_count(host, 0), "changed");
        assert_eq!(model.host_session_count(host), Some(0));
        assert_eq!(model.host_session_count("other.tail1234.ts.net"), None);
        assert!(model.forget_host_session_count(host), "dropped");
        assert!(!model.forget_host_session_count(host), "nothing left");
        assert_eq!(model.host_session_count(host), None);
    }

    #[test]
    fn boot_target_matches_a_saved_host_whatever_the_case_or_trailing_dot() {
        let dir = TestDir::new("boot-target-case");
        let host = json!({ "name": "Box.tail1234.ts.net.", "port": 9443 });
        let file = json!({ "saved_hosts": [host.clone()], "last_target": host });
        std::fs::write(dir.0.join(UI_FILE), file.to_string()).expect("write");
        let loaded = load_ui_state(&dir.0);
        assert_eq!(
            loaded.last_target,
            Some(tailnet_host("box.tail1234.ts.net", 9443)),
            "the last target's name loads normalised, as a saved host's does"
        );
        assert_eq!(
            crate::connection_picker::resolve_boot_target(&loaded),
            crate::target::Target::tailnet("box.tail1234.ts.net", Some(9443))
        );

        let bad = json!({ "last_target": { "name": "box/ws.tail1234.ts.net", "port": 9443 } });
        std::fs::write(dir.0.join(UI_FILE), bad.to_string()).expect("write");
        assert_eq!(
            load_ui_state(&dir.0).last_target,
            None,
            "a last target whose name fails the check is this PC"
        );
    }

    #[test]
    fn a_hand_edited_bad_saved_host_is_dropped_on_load() {
        let dir = TestDir::new("bad-saved-hosts");
        let file = json!({
            "sidebar_collapsed": true,
            "saved_hosts": [
                { "name": "box.tail1234.ts.net", "port": 9443 },
                { "name": "evil.example/ws", "port": 8787 },
                { "name": "user@box.tail1234.ts.net", "port": 8787 },
                { "name": "box", "port": 8787 },
                { "name": "zero.tail1234.ts.net", "port": 0 },
                { "name": "big.tail1234.ts.net", "port": 70000 },
                { "name": "Up.Tail1234.ts.net.", "port": 8787 },
                "not a host",
            ],
        });
        std::fs::write(dir.0.join(UI_FILE), file.to_string()).expect("write");
        let loaded = load_ui_state(&dir.0);
        assert!(loaded.sidebar_collapsed, "the rest of the file still loads");
        assert_eq!(
            loaded.saved_hosts,
            [
                tailnet_host("box.tail1234.ts.net", 9443),
                tailnet_host("up.tail1234.ts.net", 8787),
            ],
            "only the hosts whose name passes the DNS check and whose port is 1 to 65535 stay"
        );
    }

    #[test]
    fn spawn_defaults_round_trip_and_default_off() {
        let dir = TestDir::new("spawn-defaults");
        let defaults = SpawnDefaults { trusted: true };
        let state = UiState {
            spawn: defaults,
            ..UiState::default()
        };
        save_ui_state(&dir.0, &state).expect("save");
        assert_eq!(load_ui_state(&dir.0).spawn, defaults, "round trips");

        std::fs::write(
            dir.0.join(UI_FILE),
            r#"{ "spawn": { "trusted": true, "permission_mode": "plan", "claude_provider": "deepseek" } }"#,
        )
        .expect("write");
        assert_eq!(
            load_ui_state(&dir.0).spawn,
            defaults,
            "the retired Claude keys load ignored"
        );

        let off = SpawnDefaults::default();
        assert!(!off.trusted);

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
        assert_eq!(model.ui_state().spawn, SpawnDefaults { trusted: true });
    }

    #[test]
    fn settings_groups_missing_from_an_older_file_default_on() {
        let dir = TestDir::new("settings-groups");
        std::fs::write(dir.0.join(UI_FILE), r#"{ "sidebar_collapsed": true }"#).expect("write");
        let loaded = load_ui_state(&dir.0);
        assert!(loaded.general.copy_on_select);
        assert!(loaded.title.show_count);
        assert!(loaded.title.suffix);
        assert_eq!(loaded.title.count, CountMode::NeedsYou);

        std::fs::write(dir.0.join(UI_FILE), r#"{ "title": { "suffix": false } }"#).expect("write");
        let loaded = load_ui_state(&dir.0);
        assert!(loaded.title.show_count, "a group missing a field fills it");
        assert!(!loaded.title.suffix);
    }

    #[test]
    fn count_mode_loads_needs_you_when_missing_or_unknown() {
        let dir = TestDir::new("count-mode");
        let mut model = SidebarModel::new(UiState::default());
        assert_eq!(
            model.ui_state().title.count,
            CountMode::NeedsYou,
            "the default"
        );
        for (mode, saved) in [
            (CountMode::Busy, "busy"),
            (CountMode::Idle, "idle"),
            (CountMode::NeedsYou, "needs_you"),
        ] {
            assert!(model.set_count_mode(mode), "{saved}");
            assert!(!model.set_count_mode(mode), "unchanged");
            save_ui_state(&dir.0, model.ui_state()).expect("save");
            let text = std::fs::read_to_string(dir.0.join(UI_FILE)).expect("read");
            let written: serde_json::Value = serde_json::from_str(&text).expect("JSON");
            assert_eq!(written["title"]["count"], saved);
            assert_eq!(load_ui_state(&dir.0).title.count, mode);
        }

        for older in [
            r#"{ "sidebar_collapsed": true }"#,
            r#"{ "title": { "suffix": false } }"#,
        ] {
            std::fs::write(dir.0.join(UI_FILE), older).expect("write");
            assert_eq!(
                load_ui_state(&dir.0).title.count,
                CountMode::NeedsYou,
                "{older}"
            );
        }

        let unknown = r#"{ "title": { "suffix": false, "count": "newest" } }"#;
        std::fs::write(dir.0.join(UI_FILE), unknown).expect("write");
        let loaded = load_ui_state(&dir.0);
        assert_eq!(loaded.title.count, CountMode::NeedsYou);
        assert!(!loaded.title.suffix, "the rest of the group loads");
    }

    #[test]
    fn viewer_markdown_rendered_defaults_on() {
        assert!(UiState::default().viewer_markdown_rendered);
    }

    #[test]
    fn a_file_without_the_key_loads_rendered() {
        let dir = TestDir::new("viewer-markdown-rendered");
        std::fs::write(dir.0.join(UI_FILE), r#"{ "diff_highlight": false }"#).expect("write");
        let loaded = load_ui_state(&dir.0);
        assert!(
            !loaded.diff_highlight,
            "the file was read: its highlight toggle is not the default"
        );
        assert!(loaded.viewer_markdown_rendered);
    }

    #[test]
    fn open_text_externally_defaults_off() {
        assert!(!GeneralSettings::default().open_text_externally);
        assert!(!UiState::default().general.open_text_externally);
    }

    #[test]
    fn a_file_without_the_key_loads_off() {
        let dir = TestDir::new("open-text-externally");
        std::fs::write(
            dir.0.join(UI_FILE),
            r#"{ "general": { "copy_on_select": false } }"#,
        )
        .expect("write");
        let loaded = load_ui_state(&dir.0);
        assert!(
            !loaded.general.copy_on_select,
            "the file was read: its copy-on-select is not the default"
        );
        assert!(!loaded.general.open_text_externally);
    }

    #[test]
    fn leaf_density_round_trips_and_loads_comfortable_when_missing_or_unknown() {
        let dir = TestDir::new("leaf-density");
        let mut model = SidebarModel::new(UiState::default());
        assert_eq!(
            model.leaf_density(),
            LeafDensity::Comfortable,
            "the default"
        );
        assert!(model.set_leaf_density(LeafDensity::Compact));
        assert!(!model.set_leaf_density(LeafDensity::Compact), "unchanged");
        save_ui_state(&dir.0, model.ui_state()).expect("save");
        let text = std::fs::read_to_string(dir.0.join(UI_FILE)).expect("read");
        let saved: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(saved["general"]["leaf_density"], "compact");
        assert_eq!(
            load_ui_state(&dir.0).general.leaf_density,
            LeafDensity::Compact
        );

        for older in [
            r#"{ "sidebar_collapsed": true }"#,
            r#"{ "general": { "copy_on_select": false } }"#,
        ] {
            std::fs::write(dir.0.join(UI_FILE), older).expect("write");
            let loaded = load_ui_state(&dir.0);
            assert_eq!(
                loaded.general.leaf_density,
                LeafDensity::Comfortable,
                "{older}"
            );
        }

        let unknown = r#"{ "general": { "copy_on_select": false, "leaf_density": "dense" } }"#;
        std::fs::write(dir.0.join(UI_FILE), unknown).expect("write");
        let loaded = load_ui_state(&dir.0);
        assert_eq!(loaded.general.leaf_density, LeafDensity::Comfortable);
        assert!(
            !loaded.general.copy_on_select,
            "the rest of the group loads"
        );
    }

    #[test]
    fn sessions_under_repos_round_trips_and_loads_every_when_missing_or_unknown() {
        let dir = TestDir::new("sessions-under-repos");
        let mut model = SidebarModel::new(UiState::default());
        assert_eq!(
            model.sessions_under_repos(),
            SessionsUnderRepos::Every,
            "the default"
        );
        for (mode, saved) in [
            (SessionsUnderRepos::Busy, "busy"),
            (SessionsUnderRepos::None, "none"),
            (SessionsUnderRepos::Every, "every"),
        ] {
            assert!(model.set_sessions_under_repos(mode), "{saved}");
            assert!(!model.set_sessions_under_repos(mode), "unchanged");
            save_ui_state(&dir.0, model.ui_state()).expect("save");
            let text = std::fs::read_to_string(dir.0.join(UI_FILE)).expect("read");
            let written: serde_json::Value = serde_json::from_str(&text).expect("JSON");
            assert_eq!(written["general"]["sessions_under_repos"], saved);
            assert_eq!(load_ui_state(&dir.0).general.sessions_under_repos, mode);
        }

        for older in [
            r#"{ "sidebar_collapsed": true }"#,
            r#"{ "general": { "copy_on_select": false } }"#,
        ] {
            std::fs::write(dir.0.join(UI_FILE), older).expect("write");
            assert_eq!(
                load_ui_state(&dir.0).general.sessions_under_repos,
                SessionsUnderRepos::Every,
                "{older}"
            );
        }

        let unknown =
            r#"{ "general": { "copy_on_select": false, "sessions_under_repos": "all" } }"#;
        std::fs::write(dir.0.join(UI_FILE), unknown).expect("write");
        let loaded = load_ui_state(&dir.0);
        assert_eq!(
            loaded.general.sessions_under_repos,
            SessionsUnderRepos::Every
        );
        assert!(
            !loaded.general.copy_on_select,
            "the rest of the group loads"
        );
    }

    /// The leaf of a session with `status`, `background`, `unseen` and
    /// `attention` set.
    fn busy_leaf(status: SessionStatus, background: bool, unseen: bool, attention: bool) -> Leaf {
        let mut s = session("s1");
        s.status = status;
        s.background = background;
        let id = s.id.clone();
        let marked = |on: bool| -> HashSet<String> { on.then(|| id.clone()).into_iter().collect() };
        leaf(&s, &marked(attention), &marked(unseen), false, &[])
    }

    #[test]
    fn sessions_under_repos_every_shows_every_leaf() {
        for status in [
            SessionStatus::Working,
            SessionStatus::AwaitingInput,
            SessionStatus::Idle,
            SessionStatus::Stopped,
        ] {
            assert!(SessionsUnderRepos::Every.shows(&busy_leaf(status, false, false, false)));
        }
    }

    #[test]
    fn sessions_under_repos_busy_shows_only_the_sessions_still_at_work() {
        let busy = SessionsUnderRepos::Busy;
        assert!(busy.shows(&busy_leaf(SessionStatus::Working, false, false, false)));
        assert!(busy.shows(&busy_leaf(
            SessionStatus::AwaitingInput,
            false,
            false,
            false
        )));
        assert!(busy.shows(&busy_leaf(SessionStatus::Spawning, false, false, false)));
        assert!(busy.shows(&busy_leaf(SessionStatus::Error, false, false, false)));
        assert!(
            busy.shows(&busy_leaf(SessionStatus::Idle, true, false, false)),
            "background work still runs"
        );
        assert!(
            busy.shows(&busy_leaf(SessionStatus::Idle, false, true, false)),
            "its finish is unseen"
        );
        assert!(
            busy.shows(&busy_leaf(SessionStatus::Idle, false, false, true)),
            "it needs the owner"
        );
        assert!(
            !busy.shows(&busy_leaf(SessionStatus::Idle, false, false, false)),
            "an idle session"
        );
        assert!(
            !busy.shows(&busy_leaf(SessionStatus::Stopped, false, false, false)),
            "an ended one"
        );
        assert!(
            busy.shows(&busy_leaf(SessionStatus::Stopped, false, false, true)),
            "an ended one still needing the owner"
        );
    }

    #[test]
    fn sessions_under_repos_none_hides_every_leaf() {
        for status in [
            SessionStatus::Working,
            SessionStatus::AwaitingInput,
            SessionStatus::Idle,
            SessionStatus::Stopped,
            SessionStatus::Error,
        ] {
            assert!(
                !SessionsUnderRepos::None.shows(&busy_leaf(status, true, true, true)),
                "no leaf shows"
            );
        }
    }

    #[test]
    fn repo_sort_round_trips_and_loads_needs_you_when_missing_or_unknown() {
        let dir = TestDir::new("repo-sort");
        let mut model = SidebarModel::new(UiState::default());
        assert_eq!(model.repo_sort(), RepoSort::NeedsYou, "the default");
        for (sort, saved) in [
            (RepoSort::Manual, "manual"),
            (RepoSort::Active, "active"),
            (RepoSort::NeedsYou, "needs_you"),
        ] {
            assert!(model.set_repo_sort(sort), "{saved}");
            assert!(!model.set_repo_sort(sort), "unchanged");
            save_ui_state(&dir.0, model.ui_state()).expect("save");
            let text = std::fs::read_to_string(dir.0.join(UI_FILE)).expect("read");
            let written: serde_json::Value = serde_json::from_str(&text).expect("JSON");
            assert_eq!(written["general"]["repo_sort"], saved);
            assert_eq!(load_ui_state(&dir.0).general.repo_sort, sort);
        }

        for older in [
            r#"{ "sidebar_collapsed": true }"#,
            r#"{ "general": { "copy_on_select": false } }"#,
        ] {
            std::fs::write(dir.0.join(UI_FILE), older).expect("write");
            assert_eq!(
                load_ui_state(&dir.0).general.repo_sort,
                RepoSort::NeedsYou,
                "{older}"
            );
        }

        let unknown = r#"{ "general": { "copy_on_select": false, "repo_sort": "newest" } }"#;
        std::fs::write(dir.0.join(UI_FILE), unknown).expect("write");
        let loaded = load_ui_state(&dir.0);
        assert_eq!(loaded.general.repo_sort, RepoSort::NeedsYou);
        assert!(
            !loaded.general.copy_on_select,
            "the rest of the group loads"
        );
    }

    #[test]
    fn repo_filter_round_trips_and_loads_all_when_missing_or_unknown() {
        let dir = TestDir::new("repo-filter");
        let mut model = SidebarModel::new(UiState::default());
        assert_eq!(model.repo_filter(), RepoFilter::All, "the default");
        assert!(model.set_repo_filter(RepoFilter::Active));
        assert!(!model.set_repo_filter(RepoFilter::Active), "unchanged");
        save_ui_state(&dir.0, model.ui_state()).expect("save");
        let text = std::fs::read_to_string(dir.0.join(UI_FILE)).expect("read");
        let written: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(written["repo_filter"], "active");
        assert_eq!(load_ui_state(&dir.0).repo_filter, RepoFilter::Active);

        assert!(model.set_repo_filter(RepoFilter::All), "back to every row");
        save_ui_state(&dir.0, model.ui_state()).expect("save");
        let text = std::fs::read_to_string(dir.0.join(UI_FILE)).expect("read");
        let written: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(written["repo_filter"], "all");

        for older in [
            r#"{ "sidebar_collapsed": true }"#,
            r#"{ "repo_filter": "pinned", "sidebar_collapsed": true }"#,
        ] {
            std::fs::write(dir.0.join(UI_FILE), older).expect("write");
            let loaded = load_ui_state(&dir.0);
            assert_eq!(loaded.repo_filter, RepoFilter::All, "{older}");
            assert!(
                loaded.sidebar_collapsed,
                "{older} keeps the rest of the file"
            );
        }
    }

    #[test]
    fn active_filter_leaves_out_the_repos_and_workspaces_with_no_live_session() {
        let mut model = SidebarModel::new(UiState::default());
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Repos {
                repos: vec![repo("r1", "D:/src/r1"), repo("r2", "D:/src/r2")],
            },
        );
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Workspaces {
                workspaces: vec![workspace("w1", &["r3"]), workspace("w2", &[])],
            },
        );
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Sessions {
                sessions: vec![
                    in_repo("busy", "r1"),
                    shell("sh", None, SessionKind::Standalone),
                ],
            },
        );

        let shown = model.display_containers();
        let mut all = keys(&shown);
        all.sort_unstable();
        assert_eq!(
            all,
            ["repo:r1", "repo:r2", "standalone:sh", "ws:w1", "ws:w2"],
            "All leaves every row in"
        );

        assert!(model.set_repo_filter(RepoFilter::Active));
        assert_eq!(
            keys(&model.display_containers()),
            ["repo:r1", "standalone:sh"],
            "the repo a live session works in stays, and so does the shell"
        );
    }

    #[test]
    fn active_filter_note_waits_for_the_registry_and_never_shows_in_the_tabs_view() {
        let mut model = SidebarModel::new(UiState::default());
        assert!(model.set_repo_filter(RepoFilter::Active));
        assert!(
            !model.active_filter_empty(&model.display_containers()),
            "before the daemon's registry arrives the view shows what All shows"
        );

        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Repos {
                repos: vec![repo("r1", "D:/src/r1")],
            },
        );
        let shown = model.display_containers();
        assert!(
            model.active_filter_empty(&shown),
            "no live session leaves the note"
        );

        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Sessions {
                sessions: vec![in_repo("busy", "r1")],
            },
        );
        let shown = model.display_containers();
        assert!(!model.active_filter_empty(&shown), "the repo shows");

        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Sessions {
                sessions: Vec::new(),
            },
        );
        let shown = model.display_containers();
        assert!(model.active_filter_empty(&shown));
        assert!(model.set_sidebar_view(SidebarView::Tabs));
        assert!(
            !model.active_filter_empty(&shown),
            "the note is the repos view's"
        );
    }

    #[test]
    fn active_filter_keeps_a_row_the_needs_you_list_shows() {
        let mut model = SidebarModel::new(UiState::default());
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Repos {
                repos: vec![repo("r1", "D:/src/r1"), repo("r2", "D:/src/r2")],
            },
        );
        let mut failed = in_repo("failed", "r2");
        failed.status = SessionStatus::Error;
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Sessions {
                sessions: vec![failed],
            },
        );
        flag(&mut model, "failed");
        assert!(model.set_repo_filter(RepoFilter::Active));

        assert_eq!(
            keys(&model.display_containers()),
            ["repo:r2"],
            "the ended session needing the owner keeps its row"
        );
    }

    /// The container keys in `active`, sorted.
    fn sorted_keys(active: &HashSet<String>) -> Vec<&str> {
        let mut keys: Vec<&str> = active.iter().map(String::as_str).collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn a_repo_is_active_when_a_live_session_names_it_or_works_under_it() {
        let repos = vec![
            repo("r1", "D:/src/r1"),
            repo("r2", "D:/src/r2"),
            repo("r3", "D:/src/r3"),
            repo("r4", "D:/src/r4"),
            repo("r5", "D:/src/r5"),
        ];
        let by_member = in_repo("a", "r1");
        let mut by_worktree = session("b");
        by_worktree.worktree_paths = vec!["D:\\src\\r2\\.wt\\feat".to_owned()];
        let by_cwd = shell("c", Some("d:/SRC/r3/sub"), SessionKind::Standalone);
        let mut by_start_folder = in_repo("d", "elsewhere");
        by_start_folder.members[0].worktree_path = "D:/src/r4/nested".to_owned();
        let mut beside = session("e");
        beside.current_cwd = Some("D:/src/r55".to_owned());

        let active = active_container_keys(
            &repos,
            &[],
            &[by_member, by_worktree, by_cwd, by_start_folder, beside],
        );

        assert_eq!(
            sorted_keys(&active),
            ["repo:r1", "repo:r2", "repo:r3", "repo:r4"],
            "a folder beside r5 that only shares its name's start does not count"
        );
    }

    #[test]
    fn a_repo_is_not_active_when_its_only_session_has_ended_is_abandoned_or_parked() {
        let repos = vec![repo("r1", "D:/src/r1")];
        let mut stopped = in_repo("s", "r1");
        stopped.status = SessionStatus::Stopped;
        let mut errored = in_repo("s", "r1");
        errored.status = SessionStatus::Error;
        let mut abandoned = in_repo("s", "r1");
        abandoned.is_abandoned = true;
        let mut parked = in_repo("s", "r1");
        parked.is_inactive = true;
        for (why, s) in [
            ("stopped", stopped),
            ("errored", errored),
            ("abandoned", abandoned),
            ("parked", parked),
        ] {
            assert!(active_container_keys(&repos, &[], &[s]).is_empty(), "{why}");
        }

        let mut orphan = in_repo("s", "r1");
        orphan.is_orphan = true;
        assert_eq!(
            sorted_keys(&active_container_keys(&repos, &[], &[orphan])),
            ["repo:r1"],
            "an orphan still runs"
        );
    }

    #[test]
    fn a_workspace_is_active_when_a_session_is_spawned_for_it_or_a_member_is_active() {
        let repos = vec![repo("r1", "D:/src/r1"), repo("r2", "D:/src/r2")];
        let workspaces = vec![
            workspace("w1", &["r1"]),
            workspace("w2", &["r2"]),
            workspace("w3", &[]),
        ];
        let mut spawned_for = session("s1");
        spawned_for.workspace_id = Some("w3".to_owned());
        let member = in_repo("s2", "r1");

        let active = active_container_keys(&repos, &workspaces, &[spawned_for, member]);

        assert_eq!(sorted_keys(&active), ["repo:r1", "ws:w1", "ws:w3"]);
    }

    /// Repos `ra` (a working session), `rb` (a stopped one), `rc` (one
    /// awaiting input), `rd` (none) and workspace `wz` (an idle one), in the
    /// manual order rd, ra, wz, rc, rb; then a shell, a folder and Detached.
    fn mixed_fixture() -> Fixture {
        let mut working = in_repo("working", "ra");
        working.status = SessionStatus::Working;
        let mut stopped = in_repo("stopped", "rb");
        stopped.status = SessionStatus::Stopped;
        let mut waiting = in_repo("waiting", "rc");
        waiting.status = SessionStatus::AwaitingInput;
        let mut idle = session("idle");
        idle.workspace_id = Some("wz".to_owned());
        let mut fixture = Fixture::new(
            vec![
                repo("ra", "D:/src/ra"),
                repo("rb", "D:/src/rb"),
                repo("rc", "D:/src/rc"),
                repo("rd", "D:/src/rd"),
            ],
            vec![workspace("wz", &[])],
            vec![
                working,
                stopped,
                waiting,
                idle,
                shell("sh", None, SessionKind::Standalone),
                shell("dir", Some("E:/elsewhere"), SessionKind::Single),
                in_repo("gone", "unregistered"),
            ],
        );
        fixture.container_order = refs(&[
            ("rd", true),
            ("ra", true),
            ("wz", false),
            ("rc", true),
            ("rb", true),
        ]);
        fixture
    }

    #[test]
    fn each_repo_sort_groups_repos_and_workspaces_and_keeps_other_rows_last() {
        let fixture = mixed_fixture();
        let manual = fixture.build();
        assert_eq!(
            keys(&manual)[..5],
            ["repo:rd", "repo:ra", "ws:wz", "repo:rc", "repo:rb"]
        );
        let rest = keys(&manual)[5..].to_vec();
        assert_eq!(rest.len(), 3, "a shell, a folder and Detached");
        let active = active_container_keys(&fixture.repos, &fixture.workspaces, &fixture.sessions);

        for (sort, expected) in [
            (
                RepoSort::Manual,
                ["repo:rd", "repo:ra", "ws:wz", "repo:rc", "repo:rb"],
            ),
            (
                RepoSort::Active,
                ["repo:ra", "ws:wz", "repo:rc", "repo:rd", "repo:rb"],
            ),
            (
                RepoSort::NeedsYou,
                ["repo:rc", "repo:ra", "ws:wz", "repo:rd", "repo:rb"],
            ),
        ] {
            let sorted = sort_containers(fixture.build(), sort, &active);
            assert_eq!(keys(&sorted)[..5], expected, "{sort:?}");
            assert_eq!(
                keys(&sorted)[5..],
                rest,
                "{sort:?} keeps the other rows last"
            );
        }
    }

    #[test]
    fn needs_you_on_top_floats_a_row_whose_ended_session_the_needs_you_list_shows() {
        for excluded in [false, true] {
            let mut fixture = mixed_fixture();
            let mut asked = in_repo("asked", "rd");
            asked.status = SessionStatus::Stopped;
            asked.busy_excluded = excluded;
            fixture.sessions.push(asked);
            let attention: HashSet<String> = ["asked".to_owned()].into();
            let containers = build_containers(&TreeInputs {
                repos: &fixture.repos,
                workspaces: &fixture.workspaces,
                sessions: &fixture.sessions,
                agents: &[],
                container_order: &fixture.container_order,
                session_order: &fixture.session_order,
                attention: &attention,
                unseen: &HashSet::new(),
                collapsed: &BTreeSet::new(),
            });
            let active =
                active_container_keys(&fixture.repos, &fixture.workspaces, &fixture.sessions);

            let sorted = sort_containers(containers, RepoSort::NeedsYou, &active);

            assert_eq!(
                keys(&sorted)[..5],
                ["repo:rd", "repo:rc", "repo:ra", "ws:wz", "repo:rb"],
                "the Needs You list shows rd's ended session (busy excluded: {excluded}), \
                 so rd joins rc on top in manual order"
            );
        }
    }

    #[test]
    fn needs_you_on_top_ignores_an_abandoned_session_awaiting_input() {
        let mut fixture = mixed_fixture();
        let mut left = in_repo("left", "rd");
        left.status = SessionStatus::AwaitingInput;
        left.is_abandoned = true;
        fixture.sessions.push(left);
        let active = active_container_keys(&fixture.repos, &fixture.workspaces, &fixture.sessions);

        let sorted = sort_containers(fixture.build(), RepoSort::NeedsYou, &active);

        assert_eq!(
            keys(&sorted)[..5],
            ["repo:rc", "repo:ra", "ws:wz", "repo:rd", "repo:rb"],
            "the Needs You list never shows an abandoned session, so rd stays with the rest"
        );
    }

    fn member(repo: &str, branch: &str) -> SessionMember {
        SessionMember {
            repo_id: format!("id-{repo}"),
            repo_name: repo.to_owned(),
            branch: branch.to_owned(),
            worktree_path: String::new(),
        }
    }

    #[test]
    fn subline_of_a_single_repo_is_its_branch_prefixed_by_the_repo_in_the_tabs_view() {
        let mut s = session("s1");
        s.members = vec![member("tulip", "feat/petal")];
        assert_eq!(subline(&s, None, false).as_deref(), Some("feat/petal"));
        assert_eq!(subline(&s, None, true).as_deref(), Some("tulip:feat/petal"));
        s.members = vec![member("tulip", " ")];
        assert_eq!(subline(&s, None, false), None, "no branch to show");
        assert_eq!(
            subline(&s, None, true).as_deref(),
            Some("tulip"),
            "the repo alone when the branch is blank"
        );
        s.members = vec![member("", "main")];
        assert_eq!(
            subline(&s, None, true).as_deref(),
            Some("main"),
            "the branch alone when the repo name is blank"
        );
        s.members = vec![member(" ", " ")];
        assert_eq!(subline(&s, None, true), None, "nothing left to show");
    }

    #[test]
    fn subline_of_a_workspace_drops_blank_parts_and_blank_members() {
        let mut s = session("s1");
        s.kind = SessionKind::Workspace;
        s.workspace_id = Some("ws1".to_owned());
        s.members = vec![
            member("repo-a", "main"),
            member("repo-b", ""),
            member(" ", "feat/x"),
            member("", " "),
        ];
        assert_eq!(
            subline(&s, None, false).as_deref(),
            Some("repo-a:main · repo-b · feat/x")
        );
        s.members = vec![member("", ""), member(" ", " ")];
        assert_eq!(subline(&s, None, false), None, "every member blank");
    }

    #[test]
    fn subline_of_a_workspace_lists_every_member_as_repo_and_branch() {
        let mut s = session("s1");
        s.kind = SessionKind::Workspace;
        s.workspace_id = Some("ws1".to_owned());
        s.members = vec![
            member("deck-server", "proto-v24"),
            member("deck-client", "main"),
        ];
        let expected = Some("deck-server:proto-v24 · deck-client:main");
        assert_eq!(subline(&s, None, false).as_deref(), expected);
        assert_eq!(subline(&s, None, true).as_deref(), expected, "tabs view");

        s.members.truncate(1);
        assert_eq!(
            subline(&s, None, false).as_deref(),
            Some("deck-server:proto-v24"),
            "a one-member workspace still names its repo"
        );
    }

    #[test]
    fn subline_of_an_agent_with_no_members_is_none() {
        assert_eq!(subline(&session("s1"), Some("C:\\Users\\me"), true), None);
    }

    #[test]
    fn subline_of_a_shell_is_its_folder_with_home_as_tilde() {
        let home = Some("C:\\Users\\me");
        let sh = |cwd: Option<&str>| shell("sh", cwd, SessionKind::Standalone);
        assert_eq!(subline(&sh(None), home, false), None, "no folder");
        assert_eq!(
            subline(&sh(Some("  ")), home, false),
            None,
            "a blank folder"
        );
        assert_eq!(
            subline(&sh(Some("D:/scratch/dir")), home, false).as_deref(),
            Some("D:\\scratch\\dir")
        );
        assert_eq!(
            subline(&sh(Some("c:/users/me/src/tulip/")), home, true).as_deref(),
            Some("~\\src\\tulip"),
            "home matched whatever the case and separators"
        );
        assert_eq!(
            subline(&sh(Some("C:\\Users\\me")), home, false).as_deref(),
            Some("~")
        );
        assert_eq!(
            subline(&sh(Some("C:\\Users\\meg\\src")), home, false).as_deref(),
            Some("C:\\Users\\meg\\src"),
            "a sibling that shares the home's prefix"
        );
        assert_eq!(
            subline(&sh(Some("/home/me/src")), Some("/home/me"), false).as_deref(),
            Some("~/src")
        );
    }

    #[test]
    fn compact_hover_adds_the_subline_except_on_a_shell() {
        let mut s = in_repo("s1", "r1");
        s.members[0].branch = "feat/x".to_owned();
        let agent = leaf(&s, &HashSet::new(), &HashSet::new(), false, &[]);
        assert_eq!(agent.hover(LeafDensity::Comfortable), agent.tooltip);
        assert_eq!(
            agent.hover(LeafDensity::Compact),
            format!("{}\nfeat/x", agent.tooltip)
        );
        let sh = shell("sh", Some("D:/work"), SessionKind::Standalone);
        let sh = leaf(&sh, &HashSet::new(), &HashSet::new(), false, &[]);
        assert_eq!(sh.hover(LeafDensity::Compact), sh.tooltip, "Cwd: says it");
    }

    #[test]
    fn needs_you_toggle_defaults_on_when_missing() {
        let dir = TestDir::new("notifications");
        std::fs::write(dir.0.join(UI_FILE), r#"{ "sidebar_collapsed": true }"#).expect("write");
        let loaded = load_ui_state(&dir.0);
        assert_eq!(loaded.notifications, NotificationSettings::default());
        assert!(loaded.notifications.awaiting_input, "Needs you is on");
    }

    #[test]
    fn finished_notification_defaults_on() {
        let dir = TestDir::new("notifications-finished-default");
        std::fs::write(
            dir.0.join(UI_FILE),
            r#"{ "notifications": { "awaiting_input": false } }"#,
        )
        .expect("write");
        let loaded = load_ui_state(&dir.0);
        assert!(
            !loaded.notifications.awaiting_input,
            "the saved choice survives"
        );
        assert!(loaded.notifications.finished, "Finished is on");
    }

    #[test]
    fn finished_notification_round_trips() {
        let dir = TestDir::new("notifications-finished-round-trip");
        let mut model = SidebarModel::default();
        assert!(model.ui_state().notifications.finished, "on by default");
        model.set_finished_notification(false);
        save_ui_state(&dir.0, model.ui_state()).expect("save");
        let text = std::fs::read_to_string(dir.0.join(UI_FILE)).expect("read");
        let saved: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(saved["notifications"]["finished"], false);
        assert!(!load_ui_state(&dir.0).notifications.finished);
    }

    #[test]
    fn old_stopped_and_error_keys_are_dropped_on_load() {
        let dir = TestDir::new("notifications-old-keys");
        std::fs::write(
            dir.0.join(UI_FILE),
            r#"{"notifications":{"awaiting_input":false,"stopped":false,"error":true}}"#,
        )
        .expect("write");
        let loaded = load_ui_state(&dir.0);
        assert!(
            !loaded.notifications.awaiting_input,
            "the saved choice survives"
        );

        save_ui_state(&dir.0, &loaded).expect("save");
        let text = std::fs::read_to_string(dir.0.join(UI_FILE)).expect("read");
        let saved: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        let notifications = &saved["notifications"];
        assert_eq!(notifications["awaiting_input"], false);
        assert!(notifications.get("stopped").is_none(), "no stopped key");
        assert!(notifications.get("error").is_none(), "no error key");
    }

    #[test]
    fn skip_when_defaults_to_on_screen_when_absent() {
        let dir = TestDir::new("notify-skip-absent");
        std::fs::write(
            dir.0.join(UI_FILE),
            r#"{"notifications":{"awaiting_input":false}}"#,
        )
        .expect("write");
        let loaded = load_ui_state(&dir.0);
        assert_eq!(loaded.notifications.skip_when, NotifySkip::OnScreen);
        assert!(!loaded.notifications.awaiting_input);
    }

    #[test]
    fn unknown_skip_when_loads_as_on_screen_and_keeps_the_rest() {
        let dir = TestDir::new("notify-skip-unknown");
        for skip_when in [json!("sometimes"), json!(null), json!(3), json!({})] {
            let ui = json!({
                "sidebar_width": 333.0,
                "sidebar_collapsed": true,
                "notifications": { "awaiting_input": false, "skip_when": skip_when },
            });
            std::fs::write(dir.0.join(UI_FILE), ui.to_string()).expect("write");
            let loaded = load_ui_state(&dir.0);
            assert_eq!(
                loaded.notifications.skip_when,
                NotifySkip::OnScreen,
                "{skip_when} loads as on_screen"
            );
            assert!(
                !loaded.notifications.awaiting_input,
                "{skip_when}: the toggle still loads"
            );
            assert!(
                loaded.sidebar_collapsed,
                "{skip_when}: the rest of the file still loads"
            );
            assert!(
                (loaded.sidebar_width - 333.0).abs() < f32::EPSILON,
                "{skip_when}: the sidebar width still loads"
            );
        }
    }

    #[test]
    fn each_skip_choice_round_trips() {
        let dir = TestDir::new("notify-skip-round-trip");
        let mut model = SidebarModel::default();
        for (choice, word) in [
            (NotifySkip::Focused, "focused"),
            (NotifySkip::Never, "never"),
            (NotifySkip::OnScreen, "on_screen"),
        ] {
            assert!(model.set_notify_skip(choice), "{word} is a change");
            assert!(!model.set_notify_skip(choice), "{word} again is not");
            save_ui_state(&dir.0, model.ui_state()).expect("save");
            let text = std::fs::read_to_string(dir.0.join(UI_FILE)).expect("read");
            let saved: serde_json::Value = serde_json::from_str(&text).expect("JSON");
            assert_eq!(saved["notifications"]["skip_when"], word);
            assert_eq!(load_ui_state(&dir.0).notifications.skip_when, choice);
        }
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
    fn last_repo_dir_round_trips_through_native_ui_json() {
        let mut model = SidebarModel::default();
        assert_eq!(model.last_repo_dir(), None);
        assert!(model.set_last_repo_dir("D:\\src"));
        assert!(!model.set_last_repo_dir("D:\\src"), "unchanged");
        assert_eq!(model.last_vscode_dir(), None);
        assert!(model.set_last_vscode_dir("E:\\workspaces"));
        assert!(!model.set_last_vscode_dir("E:\\workspaces"), "unchanged");

        let dir = TestDir::new("last-repo-dir");
        save_ui_state(&dir.0, model.ui_state()).expect("save");
        let loaded = load_ui_state(&dir.0);
        assert_eq!(loaded.last_repo_dir.as_deref(), Some("D:\\src"));
        assert_eq!(
            loaded.last_vscode_dir.as_deref(),
            Some("E:\\workspaces"),
            "the file picker's own folder round-trips too"
        );
        assert_eq!(&loaded, model.ui_state());
    }

    #[test]
    fn an_old_ui_file_without_last_repo_dir_loads() {
        let dir = TestDir::new("no-last-repo-dir");
        std::fs::write(
            dir.0.join(UI_FILE),
            r#"{ "sidebar_collapsed": true, "quick_shell_dir": "C:\\work" }"#,
        )
        .expect("write");
        let loaded = load_ui_state(&dir.0);
        assert!(loaded.sidebar_collapsed, "the rest of the file still loads");
        assert_eq!(loaded.quick_shell_dir.as_deref(), Some("C:\\work"));
        assert_eq!(
            loaded.last_repo_dir, None,
            "an older file has no repo folder"
        );
        assert_eq!(
            loaded.last_vscode_dir, None,
            "an older file has no VS Code folder either"
        );
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
            &[],
            &TabRollups {
                session_order: &HashMap::new(),
                attention: &HashSet::new(),
                unseen: &HashSet::new(),
            },
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
        assert_eq!(tree[0].leaves, [] as [crate::sidebar::Leaf; 0]);
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
        assert_eq!(tab_view(&[], &[]), [] as [crate::sidebar::Container; 0]);
    }

    #[test]
    fn tab_and_unbound_containers_fold_by_their_own_keys() {
        let mut model = SidebarModel::default();
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Sessions {
                sessions: vec![session("s1"), session("s2")],
            },
        );
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
            &[],
            &TabRollups {
                session_order: &HashMap::new(),
                attention: &attention,
                unseen: &HashSet::new(),
            },
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
    fn showing_an_activity_unfolds_the_panel_and_never_folds_it() {
        let mut model = model_with(Vec::new());
        model.click_activity(Activity::Sessions);
        assert!(model.is_collapsed(), "a click on the active item folds");
        model.show_activity(Activity::Sessions);
        assert!(!model.is_collapsed());
        assert_eq!(model.activity(), Activity::Sessions);
        model.show_activity(Activity::Sessions);
        assert!(!model.is_collapsed(), "showing it again keeps it open");
        model.show_activity(Activity::NeedsYou);
        assert_eq!(model.activity(), Activity::NeedsYou);
        assert!(!model.is_collapsed());
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

    fn spawn_config(target: SpawnTarget, mode: SessionMode) -> SpawnConfig {
        SpawnConfig {
            target,
            mode,
            dangerously_skip_permissions: false,
            agent: (mode != SessionMode::PlainShell).then(|| agent_selection("claude", &[])),
            extra_env: Vec::new(),
        }
    }

    /// The registry's Claude descriptor, as the daemon sends it.
    fn claude_descriptor() -> AgentDescriptor {
        agent_registry::descriptors()
            .into_iter()
            .find(|descriptor| descriptor.id.0 == "claude")
            .expect("the registry has Claude")
    }

    /// The test agent's descriptor: label `Test agent`, tag `test`, no accent.
    fn test_descriptor() -> AgentDescriptor {
        agent_api::test_agent::DESCRIPTOR.clone()
    }

    /// A test-agent selection holding `options`.
    fn test_selection(options: &[(&str, OptionValue)]) -> AgentSelection {
        AgentSelection {
            agent: AgentId("test-agent".to_owned()),
            options: options
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect(),
        }
    }

    fn single_target(branch: &str, base: Option<&str>, use_worktree: bool) -> SpawnTarget {
        SpawnTarget::Single {
            repo_id: "r1".to_owned(),
            branch_name: branch.to_owned(),
            base_branch: base.map(str::to_owned),
            use_worktree,
            checkout_strategy: None,
            worktree_reuse: protocol::WorktreeReusePolicy::default(),
            existing_worktree: None,
        }
    }

    #[test]
    fn launch_summary_names_a_worktree_configs_parts() {
        let mut config = spawn_config(
            single_target("feat/x", Some("main"), true),
            SessionMode::Interactive,
        );
        config.agent = Some(agent_selection(
            "claude",
            &[
                (
                    "permission_mode",
                    OptionValue::Text("accept_edits".to_owned()),
                ),
                ("model", OptionValue::Text("opus".to_owned())),
            ],
        ));
        config.extra_env = vec![
            ("A".to_owned(), "1".to_owned()),
            ("B".to_owned(), "2".to_owned()),
        ];
        assert_eq!(
            launch_summary(Some(&config), &[claude_descriptor()]).as_deref(),
            Some("Claude · Interactive · new worktree from main · Accept edits · opus · 2 env")
        );
    }

    #[test]
    fn launch_summary_names_an_in_place_branch() {
        let config = spawn_config(
            single_target("feat/x", Some("main"), false),
            SessionMode::Interactive,
        );
        assert_eq!(
            launch_summary(Some(&config), &[claude_descriptor()]).as_deref(),
            Some("Claude · Interactive · feat/x"),
            "no base, model, authority or env is invented"
        );
    }

    #[test]
    fn launch_summary_names_a_standalone_shell() {
        let config = spawn_config(
            SpawnTarget::Standalone {
                cwd: None,
                add_dirs: Vec::new(),
            },
            SessionMode::PlainShell,
        );
        assert_eq!(
            launch_summary(Some(&config), &[]).as_deref(),
            Some("Shell · standalone")
        );
    }

    #[test]
    fn launch_summary_is_none_without_a_config() {
        assert_eq!(launch_summary(None, &[]), None);
    }

    #[test]
    fn last_spawn_config_reads_the_repo_and_workspace_records() {
        let mut model = model_with(Vec::new());
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Workspaces {
                workspaces: vec![workspace("w1", &["r1"])],
            },
        );
        let repo_config = spawn_config(
            single_target("feat/x", None, false),
            SessionMode::Interactive,
        );
        let mut launched = repo("r1", "D:\\r1");
        launched.last_spawn_config = Some(repo_config.clone());
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Repos {
                repos: vec![launched],
            },
        );

        assert_eq!(
            model.last_spawn_config(launch_last::Container::Repo("r1")),
            Some(repo_config),
            "the repo's stored config"
        );
        assert_eq!(
            model.last_spawn_config(launch_last::Container::Repo("r9")),
            None,
            "an unknown repo has none"
        );

        let mut workspace_config = spawn_config(
            SpawnTarget::Workspace {
                workspace_id: "w1".to_owned(),
                branch_name: "feat/x".to_owned(),
                base_branch: None,
                use_worktree: false,
                worktree_reuse: protocol::WorktreeReusePolicy::default(),
                existing_worktrees: Vec::new(),
            },
            SessionMode::Interactive,
        );
        workspace_config.agent = Some(agent_selection(
            "claude",
            &[("model", OptionValue::Text("sonnet".to_owned()))],
        ));
        let mut launched = workspace("w1", &["r1"]);
        launched.last_spawn_config = Some(workspace_config.clone());
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Workspaces {
                workspaces: vec![launched],
            },
        );

        assert_eq!(
            model.last_spawn_config(launch_last::Container::Workspace("w1")),
            Some(workspace_config),
            "the workspace's stored config"
        );
        assert_eq!(
            model.last_spawn_config(launch_last::Container::Workspace("w9")),
            None,
            "an unknown workspace has none"
        );
    }

    #[test]
    fn abandoned_count_counts_the_abandoned_sessions() {
        let mut model = model_with(vec![in_repo("a", "r1"), in_repo("b", "r1")]);
        assert_eq!(model.abandoned_count(), 0);
        let mut abandoned = in_repo("b", "r1");
        abandoned.is_abandoned = true;
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::SessionUpdated {
                session: abandoned,
                request_id: None,
            },
        );
        assert_eq!(model.abandoned_count(), 1);
    }

    #[test]
    fn row_focus_moves_and_clamps_at_the_ends() {
        let keys: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|key| (*key).to_owned())
            .collect();
        assert_eq!(move_row_focus(&keys, Some("b"), -1).as_deref(), Some("a"));
        assert_eq!(move_row_focus(&keys, Some("b"), 1).as_deref(), Some("c"));
        assert_eq!(
            move_row_focus(&keys, Some("a"), -1).as_deref(),
            Some("a"),
            "the top holds"
        );
        assert_eq!(
            move_row_focus(&keys, Some("c"), 1).as_deref(),
            Some("c"),
            "the bottom holds"
        );
        assert_eq!(
            move_row_focus(&keys, None, 1).as_deref(),
            Some("a"),
            "no current row starts at the first"
        );
        assert_eq!(
            move_row_focus(&keys, None, -1).as_deref(),
            Some("c"),
            "a backward move with no current row starts at the last"
        );
        assert_eq!(move_row_focus(&[], None, 1), None, "no rows, no focus");
    }

    #[test]
    fn launch_summary_says_trusted_instead_of_the_authority() {
        let mut config = spawn_config(
            single_target("feat/x", Some("main"), false),
            SessionMode::Interactive,
        );
        config.agent = Some(agent_selection(
            "claude",
            &[(
                "permission_mode",
                OptionValue::Text("accept_edits".to_owned()),
            )],
        ));
        config.dangerously_skip_permissions = true;
        assert_eq!(
            launch_summary(Some(&config), &[claude_descriptor()]).as_deref(),
            Some("Claude · Interactive · feat/x · trusted")
        );
    }

    #[test]
    fn launch_summary_names_the_approval_mode() {
        let mut config = spawn_config(
            single_target("feat/x", Some("main"), false),
            SessionMode::Interactive,
        );
        config.agent = Some(agent_selection(
            "claude",
            &[(
                "permission_mode",
                OptionValue::Text("accept_edits".to_owned()),
            )],
        ));
        assert_eq!(
            launch_summary(Some(&config), &[claude_descriptor()]).as_deref(),
            Some("Claude · Interactive · feat/x · Accept edits")
        );
    }

    #[test]
    fn launch_summary_names_the_descriptor_label() {
        let mut config = spawn_config(
            single_target("feat/x", None, false),
            SessionMode::Interactive,
        );
        config.agent = Some(test_selection(&[]));
        assert_eq!(
            launch_summary(Some(&config), &[test_descriptor()]).as_deref(),
            Some("Test agent · Interactive · feat/x"),
            "the descriptor's label, not the agent id"
        );
    }

    #[test]
    fn launch_summary_falls_back_to_the_agent_id() {
        let mut config = spawn_config(
            single_target("feat/x", None, false),
            SessionMode::Interactive,
        );
        config.agent = Some(test_selection(&[]));
        assert_eq!(
            launch_summary(Some(&config), &[]).as_deref(),
            Some("test-agent · Interactive · feat/x"),
            "no descriptor at hand: the id"
        );
    }

    #[test]
    fn launch_summary_shows_the_provider_choice_label() {
        let mut config = spawn_config(
            single_target("feat/x", None, false),
            SessionMode::Interactive,
        );
        config.agent = Some(agent_selection(
            "claude",
            &[("provider", OptionValue::Text("deepseek".to_owned()))],
        ));
        assert_eq!(
            launch_summary(Some(&config), &[claude_descriptor()]).as_deref(),
            Some("Claude · Interactive · feat/x · DeepSeek"),
            "the descriptor's choice label, not the stored value"
        );
    }

    #[test]
    fn launch_summary_lists_non_default_fields_in_descriptor_order() {
        let mut config = spawn_config(
            single_target("feat/x", None, false),
            SessionMode::Interactive,
        );
        config.agent = Some(test_selection(&[
            ("profile", OptionValue::Text("work".to_owned())),
            ("sandbox", OptionValue::Bool(true)),
            ("mode", OptionValue::Text("slow".to_owned())),
        ]));
        assert_eq!(
            launch_summary(Some(&config), &[test_descriptor()]).as_deref(),
            Some("Test agent · Interactive · feat/x · slow · sandbox · work"),
            "the descriptor's order: the choice, then the toggle, then the text"
        );
    }

    /// Repos `r1` and `r2` holding sessions `a`, `b` (in `r1`) and the empty
    /// workspace `w1`.
    fn ordered_model() -> SidebarModel {
        let mut model = SidebarModel::new(UiState::default());
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Repos {
                repos: vec![repo("r1", "D:\\r1"), repo("r2", "D:\\r2")],
            },
        );
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Workspaces {
                workspaces: vec![workspace("w1", &[])],
            },
        );
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Sessions {
                sessions: vec![in_repo("a", "r1"), in_repo("b", "r1")],
            },
        );
        model
    }

    fn refs(ids: &[(&str, bool)]) -> Vec<ContainerRef> {
        ids.iter()
            .map(|(id, is_repo)| {
                if *is_repo {
                    ContainerRef::Repo((*id).to_owned())
                } else {
                    ContainerRef::Workspace((*id).to_owned())
                }
            })
            .collect()
    }

    #[test]
    fn local_order_of_containers_shows_before_any_echo() {
        let mut model = ordered_model();
        assert_eq!(keys(&model.containers()), ["ws:w1", "repo:r1", "repo:r2"]);

        model.reorder_containers_local(
            &crate::target::HostKey::Local,
            &refs(&[("r2", true), ("w1", false), ("r1", true)]),
        );
        assert_eq!(
            keys(&model.containers()),
            ["repo:r2", "ws:w1", "repo:r1"],
            "the new order shows with no broadcast"
        );

        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Sessions {
                sessions: vec![in_repo("a", "r1"), in_repo("b", "r1")],
            },
        );
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::Repos {
                repos: vec![repo("r1", "D:\\r1"), repo("r2", "D:\\r2")],
            },
        );
        assert_eq!(
            keys(&model.containers()),
            ["repo:r2", "ws:w1", "repo:r1"],
            "unrelated messages leave the local order"
        );
    }

    #[test]
    fn local_order_of_sessions_shows_before_any_echo() {
        let mut model = ordered_model();
        assert_eq!(leaf_ids(&model.containers(), "repo:r1"), ["a", "b"]);

        model.reorder_sessions_local(
            &crate::target::HostKey::Local,
            "r1",
            &["b".to_owned(), "a".to_owned()],
        );
        assert_eq!(
            leaf_ids(&model.containers(), "repo:r1"),
            ["b", "a"],
            "the new order shows with no broadcast"
        );

        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::SessionUpdated {
                session: in_repo("a", "r1"),
                request_id: None,
            },
        );
        assert_eq!(
            leaf_ids(&model.containers(), "repo:r1"),
            ["b", "a"],
            "a session update leaves the local order"
        );
    }

    #[test]
    fn local_order_echo_with_the_same_order_changes_nothing() {
        let mut model = ordered_model();
        let ordered = refs(&[("r2", true), ("r1", true), ("w1", false)]);
        model.reorder_containers_local(&crate::target::HostKey::Local, &ordered);
        model.reorder_sessions_local(
            &crate::target::HostKey::Local,
            "r1",
            &["b".to_owned(), "a".to_owned()],
        );
        let before = model.containers();

        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::ContainersReordered { ordered },
        );
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::SessionsReordered {
                container_id: "r1".to_owned(),
                ordered_ids: vec!["b".to_owned(), "a".to_owned()],
            },
        );
        assert_eq!(model.containers(), before);
    }

    #[test]
    fn local_order_gives_way_to_an_echo_that_disagrees() {
        let mut model = ordered_model();
        model.reorder_containers_local(
            &crate::target::HostKey::Local,
            &refs(&[("r2", true), ("r1", true)]),
        );
        model.reorder_sessions_local(
            &crate::target::HostKey::Local,
            "r1",
            &["b".to_owned(), "a".to_owned()],
        );

        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::ContainersReordered {
                ordered: refs(&[("r1", true), ("w1", false)]),
            },
        );
        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::SessionsReordered {
                container_id: "r1".to_owned(),
                ordered_ids: vec!["a".to_owned(), "b".to_owned()],
            },
        );
        let containers = model.containers();
        assert_eq!(keys(&containers), ["repo:r1", "ws:w1", "repo:r2"]);
        assert_eq!(leaf_ids(&containers, "repo:r1"), ["a", "b"]);
    }

    #[test]
    fn local_order_tab_view_honours_the_session_order() {
        let mut model = ordered_model();
        let tabs = [tab(
            "t1",
            &crate::tabs::tests::split(
                SplitDirection::Horizontal,
                0.5,
                pane("p1", Some("a")),
                pane("p2", Some("b")),
            ),
        )];
        assert_eq!(
            leaf_ids(&model.tab_containers(&tabs), "tab:t1"),
            ["a", "b"],
            "pane order without a manual order"
        );

        model.reorder_sessions_local(
            &crate::target::HostKey::Local,
            "t1",
            &["b".to_owned(), "a".to_owned()],
        );
        assert_eq!(leaf_ids(&model.tab_containers(&tabs), "tab:t1"), ["b", "a"]);

        model.apply(
            &crate::target::HostKey::Local,
            &DaemonMessage::SessionsReordered {
                container_id: "t1".to_owned(),
                ordered_ids: vec!["a".to_owned(), "b".to_owned()],
            },
        );
        assert_eq!(
            leaf_ids(&model.tab_containers(&tabs), "tab:t1"),
            ["a", "b"],
            "the daemon's order applies by tab id"
        );
    }
}
