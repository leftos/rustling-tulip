//! Wire protocol between the rustling-tulip daemon and its clients.
//!
//! All messages are JSON-encoded. Both directions are tagged enums (`type` field)
//! so that adding a new variant is backward-compatible: older peers either match a
//! known variant or surface an `Error` for the unknown one.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

include!(concat!(env!("OUT_DIR"), "/protocol_version.rs"));

fn default_true() -> bool {
    true
}

/// Which CLI the daemon spawns for a given session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Agent {
    #[default]
    Claude,
    Codex,
}

impl Agent {
    /// Lowercase tag for UI badges and log messages.
    #[must_use]
    pub fn as_label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// Codex sandbox policy. Maps to `codex --sandbox <value>`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum CodexSandbox {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl CodexSandbox {
    /// CLI value as expected by `codex --sandbox <X>`.
    #[must_use]
    pub fn as_cli_arg(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

/// The `daemon.json` discovery file written by the daemon on startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonHandshake {
    pub protocol_version: u32,
    pub port: u16,
    pub auth_token: String,
    pub pid: u32,
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoEntry {
    pub id: String,
    pub name: String,
    pub path: String,
    pub default_branch: Option<String>,
    /// Persistent UI preference: when a session is spawned against this repo,
    /// should the spawn dialog default to creating a worktree (`true`) or to
    /// running claude in the repo's main directory (`false`)? Defaults to
    /// `true` for backwards compatibility with state.json files that don't
    /// carry this field yet.
    #[serde(default = "default_true")]
    pub default_use_worktree: bool,
    /// Last agent spawned against this repo. Drives the spawn-dialog default
    /// so repeated launches don't force the user to re-pick. `None` for repos
    /// that have never been launched (or were last touched before this field
    /// existed, hence the serde default).
    #[serde(default)]
    pub last_agent: Option<Agent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceEntry {
    pub id: String,
    pub name: String,
    pub member_repo_ids: Vec<String>,
    /// If this workspace mirrors a VS Code `.code-workspace` file, the absolute
    /// path is recorded here so the daemon can re-sync members when the file
    /// changes. `None` for workspaces created manually.
    #[serde(default)]
    pub linked_vscode_workspace: Option<String>,
    /// Persistent UI preference; see [`RepoEntry::default_use_worktree`].
    #[serde(default = "default_true")]
    pub default_use_worktree: bool,
}

/// Detected VS Code workspace alongside a registered repo that we may want to
/// mirror as a rustling-tulip [`WorkspaceEntry`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VscodeWorkspaceSuggestion {
    /// Absolute path to the `.code-workspace` file.
    pub source_path: String,
    /// Suggested workspace name (filename stem).
    pub suggested_name: String,
    /// Each folder entry resolved against the source file's directory.
    pub folders: Vec<VscodeWorkspaceFolder>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VscodeWorkspaceFolder {
    /// Resolved absolute path.
    pub path: String,
    /// Optional `name` field from the workspace file.
    pub name: Option<String>,
    /// Repo ID if a registered repo already matches this path; otherwise `None`
    /// (the client may register it on accept).
    pub matched_repo_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    Single,
    Workspace,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Spawning,
    Idle,
    Working,
    AwaitingInput,
    Stopped,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    Interactive,
    Headless,
    /// Non-agentic shell (pwsh/powershell/cmd on Windows, $SHELL/bash/sh on
    /// Unix) running in the session's primary cwd. Bypasses the `claude` CLI
    /// entirely; no model/permission/prompt fields apply.
    PlainShell,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMember {
    pub repo_id: String,
    pub repo_name: String,
    pub branch: String,
    pub worktree_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct SessionMetrics {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub last_activity_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionSnapshot {
    pub id: String,
    pub label: String,
    pub kind: SessionKind,
    pub members: Vec<SessionMember>,
    pub status: SessionStatus,
    pub mode: SessionMode,
    pub started_at: DateTime<Utc>,
    pub exit_code: Option<i32>,
    pub metrics: SessionMetrics,
    pub recent_actions: Vec<String>,
    /// True when the session was reattached on daemon startup and we no
    /// longer have a live PTY/headless handle. The underlying `claude`
    /// process is still running but its stdio is detached from us, so the
    /// UI shows the session in read-only mode (scrollback only).
    #[serde(default)]
    pub is_orphan: bool,
    /// True when this session was abandoned mid-run: the previous daemon
    /// crashed (or was killed) and the `claude` process is gone. The UI
    /// shows a "Resume" affordance that replays the captured spawn config
    /// + `last_prompt` against a fresh session. Different from `is_orphan`
    ///   (which is "still running, just detached") and from a normal
    ///   `Stopped` session (which exited gracefully).
    #[serde(default)]
    pub is_abandoned: bool,
    /// The user's initial prompt at spawn time, surfaced on the snapshot
    /// so the abandoned-bucket UI can display "what this session was
    /// trying to do". `None` for sessions spawned without a prompt and
    /// for sidecars written before Phase B.1.
    #[serde(default)]
    pub last_prompt: Option<String>,
    /// For `kind == Workspace`, the id of the workspace this session belongs
    /// to. `None` for single-repo sessions. Clients use this to group sessions
    /// under their workspace node in the sidebar tree.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Which CLI is driving this session. Daemon always populates this so the
    /// UI can label the session.
    pub agent: Agent,
    /// Window title last emitted by the agent via OSC 0/2 escape sequences.
    /// Distinct from `label` so the canonical sidebar/header name stays
    /// `<repo>:<branch>` (or the user override) instead of being clobbered by
    /// whatever the underlying shell/agent broadcasts. UIs may surface it as a
    /// tooltip or subtitle. `None` until the agent emits its first title.
    #[serde(default)]
    pub terminal_title: Option<String>,
    /// Short label for whatever program is driving this session — `"claude"`,
    /// `"codex"`, `"pwsh"`, `"powershell"`, `"cmd"`, `"bash"`, `"sh"`, etc.
    /// Surfaced in the UI next to the session title so users can tell
    /// what's running at a glance regardless of mode. `None` for sessions
    /// reattached from sidecars written by daemons that pre-date this
    /// field — the UI falls back to `agent` (which only distinguishes
    /// claude / codex) in that case.
    #[serde(default)]
    pub program_name: Option<String>,
    /// True when the session was spawned with `use_worktree = true`, i.e.
    /// the daemon created (or reused) a per-session git worktree under
    /// `<repo>.wt/<branch>`. Drives the close-context-menu's "remove
    /// worktree" choice: only when this is true does the menu offer
    /// "Close and remove worktree" vs "Close and keep worktree". For
    /// orphans with no stored spawn config we conservatively default to
    /// `false` (the cleanup choice is hidden, but the worktree is still
    /// on disk for the user to clean up manually).
    #[serde(default)]
    pub has_per_session_worktree: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpawnTarget {
    Single {
        repo_id: String,
        /// Branch the session will run on. If it doesn't exist yet, the
        /// daemon creates it from `base_branch` (or the repo's default
        /// branch, falling back to `main`).
        branch_name: String,
        /// Base for branch creation. `None` means "use the repo's default".
        base_branch: Option<String>,
        /// When `true`, the daemon adds a worktree under
        /// `<repo>.wt/<branch>` and runs claude there. When `false`, the
        /// branch is checked out in the repo's primary directory and claude
        /// runs there directly (errors out on a dirty working tree).
        use_worktree: bool,
    },
    Workspace {
        workspace_id: String,
        /// Same branch name used across all member repos.
        branch_name: String,
        /// Base branch when a member repo doesn't already have `branch_name`.
        base_branch: Option<String>,
        /// See [`SpawnTarget::Single::use_worktree`]; applied independently to
        /// each member.
        use_worktree: bool,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    BypassPermissions,
    Plan,
}

impl PermissionMode {
    /// CLI flag value as expected by `claude --permission-mode <X>`.
    #[must_use]
    pub fn as_cli_arg(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::BypassPermissions => "bypassPermissions",
            Self::Plan => "plan",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpawnRequest {
    pub label: Option<String>,
    pub target: SpawnTarget,
    pub mode: SessionMode,
    pub initial_prompt: Option<String>,
    /// For `agent == Claude`: corresponds to claude's
    /// `--dangerously-skip-permissions`. For `agent == Codex`: corresponds to
    /// codex's `--yolo` (`--dangerously-bypass-approvals-and-sandbox`).
    pub dangerously_skip_permissions: bool,
    /// Which CLI to spawn. Drives both the executable resolution
    /// (`RUSTLING_TULIP_CLAUDE` vs `RUSTLING_TULIP_CODEX`) and the per-agent
    /// arg builder.
    pub agent: Agent,
    /// Optional model override. When `None`, the CLI's default applies.
    /// Sent as `--model <id>` to both claude and codex.
    pub model: Option<String>,
    /// Claude-only permission mode. Ignored (and `--permission-mode` omitted)
    /// when `dangerously_skip_permissions` is true. Ignored entirely when
    /// `agent == Codex`.
    pub permission_mode: Option<PermissionMode>,
    /// Codex-only sandbox policy. Maps to `codex --sandbox <value>`. Ignored
    /// when `dangerously_skip_permissions` is true (yolo overrides sandbox)
    /// and ignored entirely when `agent == Claude`.
    pub codex_sandbox: Option<CodexSandbox>,
    /// Extra environment variables merged on top of the daemon's keep-list.
    /// Later entries override the keep-list on key collision so users can
    /// override values like `ANTHROPIC_API_KEY`.
    #[serde(default)]
    pub extra_env: Vec<(String, String)>,
    /// Optional scripted PTY input fed to the child after the PTY comes up.
    /// When set on `SessionMode::Interactive`, the daemon omits the positional
    /// prompt arg (the injector is expected to deliver the prompt instead).
    /// Ignored entirely for headless and plain-shell sessions.
    #[serde(default)]
    pub prompt_injector: Option<PromptInjector>,
}

/// Subset of [`SpawnRequest`] that fully describes "what kind of session
/// this was launched as", minus identifiers and one-shot kickoff fields.
/// Daemon persists this on each session record + orphan sidecar so a
/// session can be duplicated without re-prompting the user for every option.
///
/// Excluded fields versus `SpawnRequest`:
/// - `label`: a duplicate auto-generates a fresh `<repo>:<branch>` label.
/// - `initial_prompt`: kickoff messages are one-shot, not part of identity.
/// - `prompt_injector`: same — injectors deliver kickoff content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpawnConfig {
    pub target: SpawnTarget,
    pub mode: SessionMode,
    pub dangerously_skip_permissions: bool,
    pub agent: Agent,
    pub model: Option<String>,
    pub permission_mode: Option<PermissionMode>,
    pub codex_sandbox: Option<CodexSandbox>,
    #[serde(default)]
    pub extra_env: Vec<(String, String)>,
}

impl SpawnConfig {
    /// Capture the spawn-time config from a [`SpawnRequest`], dropping the
    /// fields a duplicate doesn't want to inherit.
    #[must_use]
    pub fn from_request(req: &SpawnRequest) -> Self {
        Self {
            target: req.target.clone(),
            mode: req.mode,
            dangerously_skip_permissions: req.dangerously_skip_permissions,
            agent: req.agent,
            model: req.model.clone(),
            permission_mode: req.permission_mode,
            codex_sandbox: req.codex_sandbox,
            extra_env: req.extra_env.clone(),
        }
    }

    /// Build a fresh [`SpawnRequest`] for cloning this session. The new
    /// request gets no label override (daemon picks `<repo>:<branch>`),
    /// no kickoff prompt, and no injector — every other spawn-time field
    /// is preserved verbatim.
    #[must_use]
    pub fn to_clone_request(&self) -> SpawnRequest {
        SpawnRequest {
            label: None,
            target: self.target.clone(),
            mode: self.mode,
            initial_prompt: None,
            dangerously_skip_permissions: self.dangerously_skip_permissions,
            agent: self.agent,
            model: self.model.clone(),
            permission_mode: self.permission_mode,
            codex_sandbox: self.codex_sandbox,
            extra_env: self.extra_env.clone(),
            prompt_injector: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CleanupAction {
    pub repo_id: String,
    pub remove_worktree: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttentionReason {
    AwaitingInput,
    Stopped,
    Error,
}

// ---------------------------------------------------------------------------
// Tabs and grids
// ---------------------------------------------------------------------------

/// Orientation of a split's two children.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    /// Children are laid out left/right; the divider is a vertical line.
    Horizontal,
    /// Children are laid out top/bottom; the divider is a horizontal line.
    Vertical,
}

/// Recursive tree describing a tab's layout: leaf panes (each referencing at
/// most one session) joined by binary splits with adjustable ratios.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GridNode {
    Pane {
        /// Daemon-allocated UUID; stable across sibling-close tree rotations.
        pane_id: String,
        /// `None` when the pane has no session attached (empty placeholder).
        /// Becomes `None` when the referenced session is removed.
        #[serde(default)]
        session_id: Option<String>,
    },
    Split {
        direction: SplitDirection,
        /// Size of `first` as a fraction of the split (clamped 0.05..=0.95).
        ratio: f32,
        first: Box<GridNode>,
        second: Box<GridNode>,
    },
}

/// Which side of a [`SplitPane`](ClientMessage::SplitPane) the newly created
/// pane occupies.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SplitPlace {
    /// New pane is the "first" child (left or top).
    First,
    /// New pane is the "second" child (right or bottom).
    Second,
}

/// Edge of a destination pane onto which a source pane is dropped.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PaneDropEdge {
    Left,
    Right,
    Top,
    Bottom,
    /// Replace the destination pane's `session_id` with the source pane's
    /// `session_id`; topology is unchanged and the source pane is removed.
    Replace,
}

/// Layout used when merging multiple tabs into a single new tab.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergeLayout {
    /// Tile panes left-to-right (each pair joined by a horizontal split).
    TileHorizontal,
    /// Tile panes top-to-bottom (each pair joined by a vertical split).
    TileVertical,
}

/// Target layout for [`ClientMessage::RearrangeTab`]. Pane identities are
/// preserved (so sessions stay bound to the same pane id); only the
/// surrounding split tree is rebuilt.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RearrangeLayout {
    /// One row of N columns.
    Horizontal,
    /// One column of N rows.
    Vertical,
    /// Balanced binary split tree (rough square for N >= 3, but every
    /// split shares the same direction so the layout reads as a single
    /// strip with sub-strips).
    Balanced,
    /// True 2D grid. `cols` is the number of columns per row; the daemon
    /// computes rows from `ceil(N/cols)`. When `cols == 0`, the daemon
    /// auto-picks `ceil(sqrt(N))` so callers that just want "make this
    /// look like a grid" don't have to do the math.
    Grid {
        cols: u32,
    },
}

/// What a tab is rendering. Most tabs hold a grid of panes/splits
/// (`Grid`); future variants (e.g. `Diff` for Monaco-backed file diffs)
/// hang off this enum without reshaping the tab list or invalidating
/// pane-level operations, which only apply to `Grid` tabs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TabContent {
    Grid {
        grid: GridNode,
    },
    /// A Monaco-backed diff view. Identified by the (repo, path, against)
    /// tuple so clicking the same file in the source-control sidebar twice
    /// just focuses the existing tab instead of opening a duplicate.
    Diff {
        repo_id: String,
        path: String,
        /// Selects the "old" side of the diff. Mirrors
        /// [`ClientMessage::GetFileDiff::against`]: `None` = worktree vs
        /// index (the CHANGES bucket), `Some("HEAD")` = index vs HEAD (the
        /// STAGED bucket), `Some(sha)` = that commit's content as the old
        /// side.
        against: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TabEntry {
    pub id: String,
    pub name: String,
    pub content: TabContent,
    pub created_at: DateTime<Utc>,
}

impl TabEntry {
    /// Borrow the underlying grid when this tab is a [`TabContent::Grid`].
    /// Returns `None` for non-grid tab kinds; callers that mutate panes
    /// must error with a clear message when this returns `None`.
    #[must_use]
    pub fn grid(&self) -> Option<&GridNode> {
        match &self.content {
            TabContent::Grid { grid } => Some(grid),
            TabContent::Diff { .. } => None,
        }
    }

    /// Mutable counterpart to [`Self::grid`].
    pub fn grid_mut(&mut self) -> Option<&mut GridNode> {
        match &mut self.content {
            TabContent::Grid { grid } => Some(grid),
            TabContent::Diff { .. } => None,
        }
    }
}

// Custom Deserialize so legacy `state.json` files written before the
// TabContent split (top-level `grid` field, no `content`) still load.
// Newer files have `content` and we use it directly.
impl<'de> Deserialize<'de> for TabEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            id: String,
            name: String,
            #[serde(default)]
            content: Option<TabContent>,
            #[serde(default)]
            grid: Option<GridNode>,
            created_at: DateTime<Utc>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let content = match (raw.content, raw.grid) {
            (Some(c), _) => c,
            (None, Some(grid)) => TabContent::Grid { grid },
            (None, None) => {
                return Err(serde::de::Error::custom(
                    "tab entry missing both `content` and legacy `grid`",
                ));
            }
        };
        Ok(TabEntry {
            id: raw.id,
            name: raw.name,
            content,
            created_at: raw.created_at,
        })
    }
}

// ---------------------------------------------------------------------------
// Presets and prompt injection
// ---------------------------------------------------------------------------

/// One step in a [`PromptInjector`] script. Steps run in declaration order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InjectorStep {
    /// Wait `ms` milliseconds before the next step.
    Delay { ms: u32 },
    /// Write raw bytes to the PTY master. Base64 to keep the JSON wire safe
    /// for control sequences (e.g. Shift+Tab is `\x1b[Z` → `"G1ta"`).
    Write { data_b64: String },
    /// Write UTF-8 text to the PTY master. When `newline` is true, a single
    /// `\r` (carriage return) is appended — that's what TUIs expect for the
    /// Enter key.
    Text { content: String, newline: bool },
}

/// Scripted PTY input fed to a freshly spawned interactive session after the
/// PTY comes up. Used by the preset launcher to enter Claude's plan mode and
/// submit a prompt without relying on the `-p` CLI arg (which submits
/// immediately and disables plan mode). Standalone outside presets: any
/// `SpawnRequest` can carry one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptInjector {
    pub steps: Vec<InjectorStep>,
}

/// Template form of [`PromptInjector`] used inside a [`PresetEntry`]. The
/// daemon composes a per-session `PromptInjector` by prefixing a
/// `Delay { ms: startup_delay_ms }`, then `pre_input`, then a `Text` step
/// holding the rendered prompt, then `post_input`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InjectorTemplate {
    /// Time to wait after the PTY spawns before sending any keystrokes.
    /// Lets the `claude` TUI finish painting its first frame.
    pub startup_delay_ms: u32,
    /// Steps sent before the prompt text. Typical use: keystrokes to enter
    /// plan mode (Shift+Tab × 4 for Claude).
    pub pre_input: Vec<InjectorStep>,
    /// Steps sent after the prompt text. Typical use: a small delay then
    /// `{ kind: "text", content: "", newline: true }` to submit.
    pub post_input: Vec<InjectorStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PresetTarget {
    Repo { repo_id: String },
    Workspace { workspace_id: String },
}

/// Where the launcher draws its list of prompts from for a single run.
/// Declared on the preset to constrain what the launch dialog offers, and
/// echoed back as [`LaunchPresetSource`] with concrete values.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PresetPromptSource {
    /// User picks a `.txt`/`.md` file at launch.
    File,
    /// Scan a repo-relative directory; one prompt per `.md` file found.
    /// Prompts are file references (e.g. `@docs/plans/open-issues/foo.md`)
    /// so the templated message can ask Claude to read them.
    Folder { relative_path: String },
    /// User types prompts directly into a textarea. Same bullet/paragraph
    /// parsing as `File`.
    Inline,
}

/// Concrete prompt-source picked at launch time. Mirrors
/// [`PresetPromptSource`] but with the chosen paths/text.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LaunchPresetSource {
    File { path: String },
    Folder { path: String },
    Inline { prompts: Vec<String> },
}

/// Where the variable's value comes from. Pure data shape — the daemon
/// resolves these at launch time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PresetVariableKind {
    /// Free-form text. Source: launch dialog (when `prompt_at_launch` is
    /// true) or `default` (otherwise).
    Text,
    /// File path picker in the launch dialog; the value is the chosen path.
    FilePath {
        #[serde(default)]
        extensions: Vec<String>,
    },
    /// Folder path picker in the launch dialog.
    FolderPath,
    /// Read the named environment variable from the daemon's process env.
    /// Empty if the env var is missing.
    EnvVar { name: String },
    /// A fixed path string with `${ENV_VAR}` / `%ENV_VAR%` env expansion and
    /// `{repo_root}` substitution applied. Useful for "the log file always
    /// lives at this OS-conventional spot" cases.
    LiteralPath { path: String },
    /// Run a child process (cwd = repo root) and capture its output as the
    /// variable's value. `args` support `{var_name}` substitution from
    /// earlier-resolved variables, plus the same `${ENV}` / `%ENV%` /
    /// `{repo_root}` expansions as `LiteralPath`. When `extract_pattern` is
    /// set, the trimmed stdout is matched against it and the first capture
    /// group is used; otherwise the value is the trimmed stdout.
    ///
    /// Always runs via the daemon's [`ClientMessage::ResolvePresetScripts`]
    /// round-trip, *after* the user reviews the rendered command + args on
    /// the launch dialog's preview stage. The daemon never auto-runs a
    /// script as a side effect of plain variable resolution — that's the
    /// security gate.
    Script {
        cmd: String,
        #[serde(default)]
        args: Vec<String>,
        /// Regex applied to trimmed stdout; first capture group becomes the
        /// value. On compile error or no match → resolution fails. `None`
        /// means "use trimmed stdout verbatim".
        #[serde(default)]
        extract_pattern: Option<String>,
        /// Process timeout. Default `60_000` ms.
        #[serde(default = "default_script_timeout_ms")]
        timeout_ms: u32,
        /// When `Some(name)`, the script is silently skipped (variable
        /// resolves to empty) if the earlier-declared variable `name`
        /// resolved to an empty string. Lets a preset gate an optional
        /// fetch on a user-typed input (e.g. "only fetch prod logs when
        /// the user fills in the minutes window").
        #[serde(default)]
        skip_if_empty: Option<String>,
    },
}

fn default_script_timeout_ms() -> u32 {
    60_000
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PresetVariable {
    /// Referenced in templates as `{name}`.
    pub name: String,
    /// Human label used by the launch dialog when prompting.
    pub label: String,
    pub kind: PresetVariableKind,
    /// When true, the launch dialog asks the user for this value. When
    /// false, the daemon resolves it silently (typical for `EnvVar` and
    /// `LiteralPath`).
    #[serde(default)]
    pub prompt_at_launch: bool,
    /// Optional pre-fill / fallback. For `prompt_at_launch = false`
    /// variables, used as the value when the kind doesn't otherwise resolve.
    #[serde(default)]
    pub default: Option<String>,
    /// When true, an empty/missing value is acceptable and the variable
    /// renders as empty (and is skipped in `context_footer_lines`).
    #[serde(default)]
    pub optional: bool,
}

/// One line of the optional context footer appended to each rendered prompt.
/// Emitted as `"<label>: <value>"`; omitted entirely if `variable` resolves
/// to an empty string.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FooterLine {
    pub label: String,
    pub variable: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TabLayout {
    /// One row of N panes, joined by horizontal splits at every level.
    TileHorizontal,
    /// One column of N panes.
    TileVertical,
    /// Balanced binary tree with a horizontal primary direction. Every
    /// split shares the same direction so the result is a horizontal
    /// strip with uneven column widths — not a true 2D grid.
    BalancedHorizontal,
    /// Balanced binary tree, vertical primary direction. Same caveat
    /// as `BalancedHorizontal`.
    BalancedVertical,
    /// True 2D grid. Daemon auto-picks `ceil(sqrt(N))` columns and lays
    /// panes out row-major; the result is a rows-of-columns split tree
    /// that actually looks like a grid in the UI. Recommended for
    /// preset launches with `N >= 3`.
    AutoGrid,
}

/// How a preset launch arranges its N spawned sessions in the tab bar.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TabGroupingConfig {
    /// No grouping. Sessions land in the sidebar only.
    None,
    /// Pack sessions into one or more new tabs. When the per-tab cap is hit,
    /// a fresh tab is allocated with `" 2"`, `" 3"`, ... suffixed to the
    /// base name.
    NewTab {
        layout: TabLayout,
        /// Optional cap. `None` = unlimited (one tab for the whole batch).
        #[serde(default)]
        max_panes_per_tab: Option<u32>,
        /// Optional override for the tab's base name. When `None`, daemon
        /// uses the preset's `name`. Same template placeholders as
        /// `session_label_template` (no `{prompt}`).
        #[serde(default)]
        tab_name_template: Option<String>,
    },
}

/// Preset definition. Lives on disk in a repo's `.rustling-tulip/presets.json`
/// (as a JSON array of these); the daemon parses + ships to clients via the
/// [`DaemonMessage::Presets`] response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PresetEntry {
    /// Stable id within a repo (author-chosen). Used by `LaunchPreset` to
    /// reference this preset.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Repo this preset was loaded from. Set by the daemon at load time;
    /// users do not write this in the preset file. For workspace listings,
    /// this is the member repo that contributed the preset.
    #[serde(default)]
    pub source_repo_id: String,
    pub prompt_sources: Vec<PresetPromptSource>,
    /// Template applied to each raw prompt. `{prompt}` is substituted with
    /// the raw prompt; other placeholders are also supported (see
    /// daemon-side renderer).
    pub prompt_template: String,
    #[serde(default)]
    pub context_footer_lines: Vec<FooterLine>,
    #[serde(default)]
    pub variables: Vec<PresetVariable>,
    /// Template for branch names. Placeholders: `{date}`, `{datetime}`,
    /// `{index}` (1-based), `{slug}`, `{stem}`, `{repo}`, plus custom vars.
    pub branch_template: String,
    /// Template for session labels in the sidebar. Defaults to a generic
    /// `"<preset name> {index}"` when omitted.
    #[serde(default)]
    pub session_label_template: Option<String>,
    /// When `None`, the daemon falls back to the source repo's
    /// [`RepoEntry::default_use_worktree`] at launch time.
    #[serde(default)]
    pub default_use_worktree: Option<bool>,
    #[serde(default)]
    pub dangerously_skip_permissions: bool,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<PermissionMode>,
    /// Which CLI to spawn for sessions created from this preset. Defaults to
    /// `Agent::Claude` for preset files that pre-date the codex field.
    #[serde(default)]
    pub agent: Agent,
    /// Codex sandbox policy. Ignored when `agent != Codex`.
    #[serde(default)]
    pub codex_sandbox: Option<CodexSandbox>,
    pub tab_grouping: TabGroupingConfig,
    pub injector: InjectorTemplate,
    /// Pause between successive session spawns. Default 3000ms matches
    /// yaat's pacing; the value avoids overlapping git worktree adds.
    #[serde(default = "default_stagger_ms")]
    pub stagger_ms: u32,
}

fn default_stagger_ms() -> u32 {
    3000
}

// ---------------------------------------------------------------------------
// Client -> Daemon
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Always the first message after WS upgrade. Fails the connection if the
    /// token does not match. The daemon negotiates a wire version from the
    /// union of `protocol_version` (scalar back-compat) and `protocol_versions`
    /// (new clients advertise every version they can speak). Sending both is
    /// the supported pattern; older peers that only know about the scalar
    /// still decode via `#[serde(default)]` on the vec.
    Hello {
        protocol_version: u32,
        #[serde(default)]
        protocol_versions: Vec<u32>,
        auth_token: String,
    },
    ListRepos,
    AddRepo {
        path: String,
        name: Option<String>,
    },
    RemoveRepo {
        repo_id: String,
    },
    ListWorkspaces,
    UpsertWorkspace {
        /// `None` to create, `Some(id)` to update.
        id: Option<String>,
        name: String,
        member_repo_ids: Vec<String>,
        #[serde(default)]
        linked_vscode_workspace: Option<String>,
    },
    /// Materialize a workspace from a previously emitted
    /// [`DaemonMessage::VscodeWorkspaceSuggestion`]. Auto-registers any folders
    /// whose `matched_repo_id` is `None` using the leaf directory as the name.
    AcceptVscodeWorkspaceSuggestion {
        suggestion: VscodeWorkspaceSuggestion,
        watch: bool,
    },
    RemoveWorkspace {
        workspace_id: String,
    },
    /// Resolve which branches exist in each member repo of a workspace
    /// without spawning anything. Used to render the spawn preview UI.
    PreviewWorkspaceSpawn {
        workspace_id: String,
        branch_name: String,
        base_branch: Option<String>,
    },
    ListSessions,
    SpawnSession(SpawnRequest),
    /// Clone an existing session: daemon reads its stored [`SpawnConfig`]
    /// and spawns a new session with the same agent, mode, target, model,
    /// permission mode, sandbox, skip-perms flag, and extra env vars. The
    /// new session gets a freshly generated label and no initial prompt —
    /// it's a fresh process with the same config, not a continuation.
    DuplicateSession {
        session_id: String,
    },
    /// Fetch the persisted [`SpawnConfig`] for a session so the spawn
    /// dialog can pre-fill all fields with the source's values. Replied
    /// with [`DaemonMessage::SpawnConfigReply`].
    GetSpawnConfig {
        session_id: String,
    },
    Attach {
        session_id: String,
    },
    Detach {
        session_id: String,
    },
    SendInput {
        session_id: String,
        data_b64: String,
    },
    Resize {
        session_id: String,
        cols: u16,
        rows: u16,
    },
    StopSession {
        session_id: String,
        cleanup: Vec<CleanupAction>,
    },
    /// Replay an abandoned session: read its stored [`SpawnConfig`] and
    /// `last_prompt` from the sidecar, spawn a fresh session, and delete
    /// the abandoned sidecar atomically. Surfaces an error if the
    /// sidecar is missing, has no spawn config (pre-B.1 sidecars), or
    /// the spawn itself fails.
    ResumeAbandoned {
        session_id: String,
    },
    /// User dismisses an abandoned session without resuming. The sidecar
    /// and session record are removed.
    DiscardAbandoned {
        session_id: String,
    },
    /// List branches in a registered repo.
    ListBranches {
        repo_id: String,
    },
    /// Recent commits for a repo. `branch` defaults to current. `offset`
    /// (default 0) maps to `git log --skip`, so the source-control sidebar
    /// can request successive batches for "load more" pagination.
    ListCommits {
        repo_id: String,
        branch: Option<String>,
        limit: u32,
        #[serde(default)]
        offset: u32,
    },
    /// Full detail (subject, body, parents, file list) for one commit.
    GetCommit {
        repo_id: String,
        sha: String,
    },
    /// Unified diff for a single file. `against` is `None` for working-tree
    /// vs index, `Some("HEAD")` for index vs HEAD, or a sha for that commit.
    GetFileDiff {
        repo_id: String,
        path: String,
        against: Option<String>,
    },
    /// Resolve the repo's `origin` remote URL and parse it into a forge link
    /// for "Open in GitHub" actions.
    GetRemoteUrl {
        repo_id: String,
    },
    /// Working-tree status (changed files) for a single repo. Useful for the
    /// non-session-scoped repo view. Daemon replies with
    /// [`DaemonMessage::RepoStatus`], which splits results into staged
    /// (index vs HEAD) and unstaged (worktree vs index) buckets so the UI
    /// can render VSCode-style STAGED + CHANGES sections.
    RepoStatus {
        repo_id: String,
    },
    /// Stage one or more paths in `repo_id` (`git add -- <path>`...). On
    /// success the daemon broadcasts a fresh [`DaemonMessage::RepoStatus`]
    /// to every connected client. Failure surfaces as
    /// [`DaemonMessage::GitWriteError`].
    StageFiles {
        repo_id: String,
        paths: Vec<String>,
    },
    /// Unstage one or more paths (`git restore --staged -- <path>`...).
    /// Same response shape as [`StageFiles`].
    UnstageFiles {
        repo_id: String,
        paths: Vec<String>,
    },
    /// Commit the current index. The daemon shells out to
    /// `git commit -m <message>` with no `--amend`, no `--no-verify`. On
    /// success it broadcasts the fresh status and replies with
    /// [`DaemonMessage::CommitOk`]; on failure it emits
    /// [`DaemonMessage::GitWriteError`] with the operation tag `"commit"`.
    CommitRepo {
        repo_id: String,
        message: String,
    },
    /// Discard unstaged worktree changes for the named paths
    /// (`git restore -- <path>`...). Index entries are untouched; callers
    /// should unstage first if they want a fully clean reset for a file.
    /// On success the daemon broadcasts a fresh [`DaemonMessage::RepoStatus`];
    /// failures surface as [`DaemonMessage::GitWriteError`] with
    /// `operation = "discard"`.
    DiscardChanges {
        repo_id: String,
        paths: Vec<String>,
    },
    /// Push a new stash. Empty `message` is allowed — git falls back to its
    /// default "WIP on <branch>" subject. Always run with `-u` so untracked
    /// files come along. Daemon broadcasts a refreshed `RepoStatus` and a
    /// fresh [`DaemonMessage::Stashes`] snapshot for `repo_id` on success.
    StashPush {
        repo_id: String,
        message: String,
    },
    /// Request the current stash list. Daemon replies with
    /// [`DaemonMessage::Stashes`].
    ListStashes {
        repo_id: String,
    },
    /// `git stash pop <stash_id>` — apply and drop. Broadcasts fresh
    /// `RepoStatus` + `Stashes` on success.
    StashPop {
        repo_id: String,
        stash_id: String,
    },
    /// `git stash apply <stash_id>` — apply without dropping.
    StashApply {
        repo_id: String,
        stash_id: String,
    },
    /// `git stash drop <stash_id>` — drop without applying.
    StashDrop {
        repo_id: String,
        stash_id: String,
    },
    /// Open (or focus, if one already exists) a diff tab for the given
    /// (repo, path, against) triple. The daemon broadcasts a `TabUpdated`
    /// when a new tab is created, then replies with
    /// [`DaemonMessage::DiffTabOpened`] so the requesting client knows
    /// which tab id to activate.
    OpenDiffTab {
        /// Client-assigned request id, echoed back on
        /// [`DaemonMessage::DiffTabOpened`].
        id: String,
        repo_id: String,
        path: String,
        against: Option<String>,
    },
    /// Fetch the OLD and NEW file contents that back a Monaco diff view.
    /// Single round-trip (atomic snapshot) — see
    /// [`DaemonMessage::FileSnapshot`].
    GetFileSnapshot {
        /// Client-assigned request id, echoed back on the response so the
        /// caller can drop stale replies.
        id: String,
        repo_id: String,
        path: String,
        against: Option<String>,
    },
    /// Request the persisted scrollback for a session, replayed on attach.
    /// The daemon answers with [`DaemonMessage::Scrollback`].
    LoadScrollback {
        session_id: String,
    },
    /// Stop every active session and exit the daemon process. The client is
    /// expected to wait for the WebSocket to close as the shutdown signal —
    /// no explicit response is sent. Issued by the desktop app's exit
    /// confirmation when the user opts to terminate everything.
    Shutdown,
    /// Update [`RepoEntry::default_use_worktree`]. Daemon replies with the
    /// fresh `Repos` snapshot so every connected client refreshes its UI.
    SetRepoWorktreeDefault {
        repo_id: String,
        value: bool,
    },
    /// Update [`WorkspaceEntry::default_use_worktree`]. Daemon replies with
    /// the fresh `Workspaces` snapshot.
    SetWorkspaceWorktreeDefault {
        workspace_id: String,
        value: bool,
    },
    /// Request the full tab list. Also delivered automatically after
    /// [`DaemonMessage::Welcome`].
    ListTabs,
    /// Create a new tab containing a single pane, optionally seeded with a
    /// session. Daemon replies with [`DaemonMessage::TabUpdated`].
    CreateTab {
        name: Option<String>,
        initial_session_id: Option<String>,
    },
    /// Close a tab. The underlying sessions are NOT stopped — only the tab's
    /// layout entry is removed.
    CloseTab {
        tab_id: String,
    },
    RenameTab {
        tab_id: String,
        name: String,
    },
    /// Full reorder of the tab list. The set of ids must match the current
    /// set; mismatches are rejected.
    ReorderTabs {
        ordered_ids: Vec<String>,
    },
    /// Split an existing pane in two along `direction`. The newly allocated
    /// pane occupies `place`; the existing pane occupies the other side.
    SplitPane {
        tab_id: String,
        pane_id: String,
        direction: SplitDirection,
        place: SplitPlace,
        new_session_id: Option<String>,
    },
    /// Close a pane. If it was the only pane in the tab, the tab is removed.
    /// Otherwise the parent split collapses to the remaining sibling.
    ClosePane {
        tab_id: String,
        pane_id: String,
    },
    /// Update a single split's ratio. `split_path` is a sequence of 0/1
    /// indices (0 = first child, 1 = second child) descending from the tab
    /// root. The value is clamped to 0.05..=0.95 by the daemon.
    SetPaneRatio {
        tab_id: String,
        split_path: Vec<u8>,
        ratio: f32,
    },
    /// Move a pane to a new location (same tab if `src_tab_id == dst_tab_id`,
    /// otherwise cross-tab). The source pane is removed from its current
    /// position (with the parent split collapsing if needed) and inserted at
    /// `edge` of the destination pane.
    MovePane {
        src_tab_id: String,
        src_pane_id: String,
        dst_tab_id: String,
        dst_pane_id: String,
        edge: PaneDropEdge,
    },
    /// Set the session a pane references without changing topology. `None`
    /// turns the pane into an empty placeholder.
    ReplacePaneSession {
        tab_id: String,
        pane_id: String,
        session_id: Option<String>,
    },
    /// Rebuild a tab's grid using `layout`. Pane identities (and their
    /// `session_id` bindings) are preserved — only the surrounding split
    /// tree changes. Used by the "Rearrange panes" UI to flip a tab's
    /// layout between horizontal / vertical / balanced / grid without
    /// re-spawning anything.
    RearrangeTab {
        tab_id: String,
        layout: RearrangeLayout,
    },
    /// Combine multiple existing tabs into a single new tab, removing the
    /// originals. Panes are collected in the order `tab_ids` is given.
    MergeTabs {
        tab_ids: Vec<String>,
        name: Option<String>,
        layout: MergeLayout,
    },
    /// Move a set of panes out of `source_tab_id` into a new tab. Panes are
    /// appended to the new tab using horizontal tiling.
    ExtractToNewTab {
        source_tab_id: String,
        pane_ids: Vec<String>,
        name: Option<String>,
    },
    /// Request the list of presets available for a repo or workspace. The
    /// daemon reads `.rustling-tulip/presets.json` from the target repo (or
    /// from each member repo of a workspace) and replies with
    /// [`DaemonMessage::Presets`].
    ListPresets {
        target: PresetTarget,
    },
    /// Batch-spawn N sessions from a preset. The daemon expands the prompt
    /// source into raw prompts, renders templates with `variable_values`,
    /// and spawns sessions sequentially with `preset.stagger_ms` between
    /// them. Progress streams back as [`DaemonMessage::PresetLaunchProgress`].
    LaunchPreset {
        target: PresetTarget,
        preset_id: String,
        source: LaunchPresetSource,
        /// Name → value pairs for every `prompt_at_launch` variable the
        /// preset declares (plus any user-supplied overrides for the
        /// silently-resolved ones).
        #[serde(default)]
        variable_values: Vec<(String, String)>,
        /// Override `preset.default_use_worktree` (and the repo fallback)
        /// for this launch.
        #[serde(default)]
        use_worktree_override: Option<bool>,
        /// Override `preset.tab_grouping.max_panes_per_tab` for this launch
        /// only. `None` = keep the preset's value.
        #[serde(default)]
        max_panes_per_tab_override: Option<u32>,
    },
    /// Resolve a preset's prompt source into the list of prompts that would
    /// be launched, without spawning anything. Used by the launch dialog to
    /// preview file/folder sources before the user commits. Daemon replies
    /// with [`DaemonMessage::PresetPreview`] on success or
    /// [`DaemonMessage::PresetPreviewError`] on failure (e.g. folder does
    /// not exist).
    PreviewPreset {
        /// Client-assigned request id, echoed back on the response so the
        /// caller can drop stale replies from rapid stage navigation.
        id: String,
        target: PresetTarget,
        preset_id: String,
        source: LaunchPresetSource,
        #[serde(default)]
        variable_values: Vec<(String, String)>,
    },
    /// Resolve any `Script`-kind variables in a preset by running them and
    /// capturing their output. The dialog calls this after the user reviews
    /// the rendered commands and clicks Launch; the resolved values are
    /// then forwarded to [`ClientMessage::LaunchPreset`] as plain
    /// `variable_values` entries so the daemon doesn't re-spawn the
    /// scripts. Daemon replies with [`DaemonMessage::PresetScriptsResolved`]
    /// or [`DaemonMessage::PresetScriptsError`].
    ResolvePresetScripts {
        /// Client-assigned request id, echoed back on the response.
        id: String,
        target: PresetTarget,
        preset_id: String,
        /// User-supplied values for non-script variables. These feed into
        /// `{var_name}` substitution in the scripts' args.
        #[serde(default)]
        variable_values: Vec<(String, String)>,
    },
}

// ---------------------------------------------------------------------------
// Daemon -> Client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonMessage {
    /// `protocol_version` is the negotiated wire version the daemon picked
    /// from the intersection of its `SUPPORTED_PROTOCOL_VERSIONS` and the
    /// client's advertised set. `supported_versions` echoes the daemon's full
    /// support list so the client can dim feature-gated UI when running
    /// against an older daemon.
    Welcome {
        protocol_version: u32,
        #[serde(default)]
        supported_versions: Vec<u32>,
    },
    AuthFailed {
        reason: String,
    },
    Repos {
        repos: Vec<RepoEntry>,
    },
    Workspaces {
        workspaces: Vec<WorkspaceEntry>,
    },
    VscodeWorkspaceSuggestion {
        repo_id: String,
        suggestion: VscodeWorkspaceSuggestion,
    },
    Branches {
        repo_id: String,
        branches: Vec<String>,
        current: Option<String>,
    },
    Sessions {
        sessions: Vec<SessionSnapshot>,
    },
    SessionUpdated {
        session: SessionSnapshot,
    },
    SessionRemoved {
        session_id: String,
    },
    /// Reply to [`ClientMessage::GetSpawnConfig`]. `config` is `None` when
    /// the session no longer exists or pre-dates spawn-config persistence
    /// (orphan sidecars written before v13). UIs that prefilled a dialog
    /// optimistically should fall back to Settings defaults in that case.
    SpawnConfigReply {
        session_id: String,
        config: Option<SpawnConfig>,
    },
    PtyOutput {
        session_id: String,
        data_b64: String,
    },
    Attention {
        session_id: String,
        reason: AttentionReason,
    },
    WorkspaceSpawnPreview {
        workspace_id: String,
        branch_name: String,
        per_member: Vec<MemberSpawnPreview>,
    },
    Commits {
        repo_id: String,
        commits: Vec<GitCommit>,
        /// Echoed from the request so the client can tell whether this is
        /// a fresh listing (`offset == 0`, replace local state) or an
        /// append for "load more" (`offset > 0`, push onto the existing
        /// list).
        #[serde(default)]
        offset: u32,
    },
    CommitDetail {
        repo_id: String,
        detail: GitCommitDetail,
    },
    FileDiff {
        repo_id: String,
        path: String,
        against: Option<String>,
        diff: String,
    },
    RemoteUrl(GitRemoteUrl),
    RepoStatus {
        repo_id: String,
        /// Files with index-vs-HEAD differences (the "STAGED" bucket).
        /// `status` is the porcelain X column (`A`, `M`, `D`, `R`).
        index_changes: Vec<GitFileChange>,
        /// Files with worktree-vs-index differences (the "CHANGES" bucket).
        /// `status` is the porcelain Y column. Untracked files appear here
        /// with `status = "?"`.
        worktree_changes: Vec<GitFileChange>,
    },
    /// Successful response to [`ClientMessage::CommitRepo`].
    CommitOk {
        repo_id: String,
        sha: String,
        short_sha: String,
    },
    /// Failure response to a stage/unstage/commit/discard/stash request.
    /// `operation` is one of `"stage"`, `"unstage"`, `"commit"`,
    /// `"discard"`, `"stash_push"`, `"stash_pop"`, `"stash_apply"`,
    /// `"stash_drop"` so the UI can attribute the error.
    GitWriteError {
        repo_id: String,
        operation: String,
        error: String,
    },
    /// Response to [`ClientMessage::ListStashes`] and a broadcast after any
    /// successful stash push/pop/drop. Carries the full current stash list
    /// in `stash@{0}` → `stash@{N}` order (newest first).
    Stashes {
        repo_id: String,
        stashes: Vec<GitStash>,
    },
    /// Response to [`ClientMessage::OpenDiffTab`]. Carries the tab id (new
    /// or pre-existing) so the requesting client can activate it.
    DiffTabOpened {
        id: String,
        tab_id: String,
    },
    /// Response to [`ClientMessage::GetFileSnapshot`]. `old` + `new` are
    /// the file contents Monaco's `DiffEditor` needs to render. `language`
    /// is an extension-derived hint (e.g. `"typescript"`, `"rust"`); the
    /// client uses it to set Monaco's model language for syntax
    /// highlighting.
    FileSnapshot {
        id: String,
        repo_id: String,
        path: String,
        against: Option<String>,
        old: String,
        new: String,
        language: String,
    },
    /// Failure response to [`ClientMessage::GetFileSnapshot`].
    FileSnapshotError {
        id: String,
        repo_id: String,
        path: String,
        against: Option<String>,
        error: String,
    },
    /// Persisted scrollback bytes (raw PTY output for interactive sessions,
    /// raw stream-json lines for headless), base64-encoded. `truncated` is
    /// true when the on-disk ring buffer overflowed at some point and the
    /// caller should surface "earlier output discarded" to the user.
    Scrollback {
        session_id: String,
        data_b64: String,
        truncated: bool,
    },
    /// Initial tab snapshot sent on connect.
    Tabs {
        tabs: Vec<TabEntry>,
    },
    /// Broadcast on any structural change to a single tab (create, rename,
    /// split, close pane, ratio adjustment, move, session-prune).
    TabUpdated {
        tab: TabEntry,
    },
    /// Broadcast when a tab is closed (explicit close or last pane removed).
    TabRemoved {
        tab_id: String,
    },
    /// Cheap broadcast for drag-reorder; avoids resending full `TabEntry`
    /// payloads.
    TabsReordered {
        ordered_ids: Vec<String>,
    },
    /// Response to [`ClientMessage::ListPresets`].
    Presets {
        target: PresetTarget,
        entries: Vec<PresetEntry>,
    },
    /// Streamed updates while a `LaunchPreset` batch runs. UI uses
    /// `current_tab_id` to auto-switch into the freshly populated tab.
    PresetLaunchProgress {
        preset_id: String,
        total: u32,
        launched: u32,
        /// Most-recent tab the launcher is filling. `None` when the preset's
        /// `tab_grouping` is `None` (sessions go to sidebar only).
        #[serde(default)]
        current_tab_id: Option<String>,
        /// All tabs created during this launch so far, in creation order.
        #[serde(default)]
        tab_ids: Vec<String>,
    },
    /// Emitted when a batch fails partway. Sessions and tabs already
    /// created are kept; the user can decide what to do with them.
    PresetLaunchFailed {
        preset_id: String,
        error: String,
        #[serde(default)]
        partial_session_ids: Vec<String>,
        #[serde(default)]
        partial_tab_ids: Vec<String>,
    },
    /// Response to [`ClientMessage::PreviewPreset`]. Echoes the request `id`
    /// so the caller can correlate; carries the resolved prompts list.
    PresetPreview {
        id: String,
        prompts: Vec<String>,
    },
    /// Failure response to [`ClientMessage::PreviewPreset`]. The dialog
    /// surfaces `error` to the user; `id` correlates against the in-flight
    /// request so stale responses can be dropped.
    PresetPreviewError {
        id: String,
        error: String,
    },
    /// Successful response to [`ClientMessage::ResolvePresetScripts`].
    /// `values` carries the *full* resolved variable map (user inputs +
    /// script outputs), ready to be passed to `LaunchPreset` as-is.
    /// `executed_commands` lets the dialog confirm to the user which
    /// commands actually ran, even if their `args` contained env or var
    /// substitution that wasn't obvious in the unresolved form.
    PresetScriptsResolved {
        id: String,
        values: Vec<(String, String)>,
        executed_commands: Vec<ScriptCommandPreview>,
    },
    /// Failure response to [`ClientMessage::ResolvePresetScripts`].
    /// `variable_name` (if known) identifies the script variable that
    /// failed so the dialog can highlight it inline.
    PresetScriptsError {
        id: String,
        error: String,
        #[serde(default)]
        variable_name: Option<String>,
    },
    Error {
        message: String,
    },
}

/// Parse-time wrapper around [`ClientMessage`] that captures unknown message
/// types as `Unknown { type_tag, raw }` instead of failing. The daemon reads
/// from the wire via [`Self::from_json_str`] and matches on the result:
/// `Known(msg)` to dispatch normally, `Unknown { .. }` to log + drop.
///
/// This is the wire-level implementation of forward-compatible deserialization:
/// a v(N+1) client speaking to a v(N) daemon may send message types the
/// daemon doesn't know yet. Today those would crash decoding; with the
/// wrapper, the daemon logs the unknown type and continues servicing the
/// connection.
#[derive(Debug, Clone)]
pub enum InboundClientMessage {
    Known(ClientMessage),
    Unknown {
        type_tag: String,
        raw: serde_json::Value,
    },
}

impl InboundClientMessage {
    /// Parse a JSON frame received from a client. Returns `Err` only for
    /// frames that aren't valid JSON at all; unknown message types resolve
    /// to `Ok(Unknown { .. })`.
    pub fn from_json_str(s: &str) -> Result<Self, serde_json::Error> {
        let value: serde_json::Value = serde_json::from_str(s)?;
        if let Ok(known) = serde_json::from_value::<ClientMessage>(value.clone()) {
            return Ok(Self::Known(known));
        }
        let type_tag = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| "<missing-type>".to_string(), String::from);
        Ok(Self::Unknown {
            type_tag,
            raw: value,
        })
    }
}

/// Parse-time wrapper around [`DaemonMessage`]; symmetric to
/// [`InboundClientMessage`]. Used by Rust clients (the Tauri side, tests)
/// that consume daemon output. The TS client mirrors the same pattern in
/// its message dispatch.
#[derive(Debug, Clone)]
pub enum InboundDaemonMessage {
    /// Boxed because `DaemonMessage` is a wide tagged enum (~264 bytes,
    /// dominated by `SessionUpdated`); keeping it in-line would force
    /// the wrapper to be that size for every Unknown frame too.
    Known(Box<DaemonMessage>),
    Unknown {
        type_tag: String,
        raw: serde_json::Value,
    },
}

impl InboundDaemonMessage {
    pub fn from_json_str(s: &str) -> Result<Self, serde_json::Error> {
        let value: serde_json::Value = serde_json::from_str(s)?;
        if let Ok(known) = serde_json::from_value::<DaemonMessage>(value.clone()) {
            return Ok(Self::Known(Box::new(known)));
        }
        let type_tag = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| "<missing-type>".to_string(), String::from);
        Ok(Self::Unknown {
            type_tag,
            raw: value,
        })
    }
}

/// A single resolved script invocation. The dialog renders these in the
/// "Scripts to run" preview list and as a confirmation strip in the
/// post-resolution toast.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScriptCommandPreview {
    pub variable_name: String,
    pub cmd: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemberSpawnPreview {
    pub repo_id: String,
    pub repo_name: String,
    pub branch_exists: bool,
    pub effective_base: Option<String>,
    pub worktree_path: String,
}

// ---------------------------------------------------------------------------
// Git inspection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitCommit {
    pub sha: String,
    pub short_sha: String,
    pub author_name: String,
    pub author_email: String,
    pub authored_at: String,
    pub subject: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitFileChange {
    pub path: String,
    /// Single-character status from `git status --porcelain` or `git
    /// diff-tree`: `M`, `A`, `D`, `R`, `?`, etc.
    pub status: String,
    /// Source path for renames (`R` status); `None` otherwise.
    pub from_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitCommitDetail {
    pub commit: GitCommit,
    pub body: String,
    pub parent_shas: Vec<String>,
    pub changes: Vec<GitFileChange>,
}

/// One stash entry from `git stash list`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitStash {
    /// Stash ref, e.g. `stash@{0}`. Stable for the duration of the listing
    /// only — drop/pop renumbers subsequent stashes, so the UI must
    /// re-request after every write.
    pub id: String,
    /// `%gs` from `git stash list` — the subject line including the
    /// "WIP on <branch>: <sha> <message>" prefix git generates.
    pub subject: String,
    /// ISO-8601 timestamp from `%aI`.
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitRemoteUrl {
    pub repo_id: String,
    /// Raw URL from `git remote get-url origin` (or other remote).
    pub raw_url: String,
    /// Canonicalized https form for browser links, when recognizable
    /// (github / gitlab / bitbucket). `None` for unknown forge formats.
    pub web_url: Option<String>,
    /// Forge identifier: `github`, `gitlab`, `bitbucket`, or `unknown`.
    pub forge: String,
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert preconditions with expect/panic; failure messages aid debugging"
)]
mod tests {
    use super::*;

    fn sample_tab() -> TabEntry {
        TabEntry {
            id: "tab-1".to_string(),
            name: "Main".to_string(),
            content: TabContent::Grid {
                grid: GridNode::Split {
                    direction: SplitDirection::Horizontal,
                    ratio: 0.6,
                    first: Box::new(GridNode::Pane {
                        pane_id: "p1".to_string(),
                        session_id: Some("s1".to_string()),
                    }),
                    second: Box::new(GridNode::Split {
                        direction: SplitDirection::Vertical,
                        ratio: 0.5,
                        first: Box::new(GridNode::Pane {
                            pane_id: "p2".to_string(),
                            session_id: None,
                        }),
                        second: Box::new(GridNode::Pane {
                            pane_id: "p3".to_string(),
                            session_id: Some("s3".to_string()),
                        }),
                    }),
                },
            },
            created_at: DateTime::from_timestamp(1_700_000_000, 0).expect("valid timestamp"),
        }
    }

    #[test]
    fn tab_entry_round_trip() {
        let original = sample_tab();
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: TabEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    #[test]
    fn grid_node_pane_omits_session_id_default() {
        let json = r#"{"kind":"pane","pane_id":"p1"}"#;
        let node: GridNode = serde_json::from_str(json).expect("parse");
        assert_eq!(
            node,
            GridNode::Pane {
                pane_id: "p1".to_string(),
                session_id: None,
            }
        );
    }

    #[test]
    fn client_message_tab_variants_tagged() {
        let msg = ClientMessage::SplitPane {
            tab_id: "t1".to_string(),
            pane_id: "p1".to_string(),
            direction: SplitDirection::Horizontal,
            place: SplitPlace::Second,
            new_session_id: None,
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(json.contains(r#""type":"split_pane""#));
        assert!(json.contains(r#""direction":"horizontal""#));
        assert!(json.contains(r#""place":"second""#));
    }

    #[test]
    fn daemon_message_tab_variants_tagged() {
        let msg = DaemonMessage::TabUpdated { tab: sample_tab() };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(json.contains(r#""type":"tab_updated""#));
        let decoded: DaemonMessage = serde_json::from_str(&json).expect("deserialize");
        let DaemonMessage::TabUpdated { tab } = decoded else {
            panic!("wrong variant");
        };
        assert_eq!(tab, sample_tab());
    }

    #[test]
    fn pane_drop_edge_serialization() {
        let cases = [
            (PaneDropEdge::Left, "left"),
            (PaneDropEdge::Right, "right"),
            (PaneDropEdge::Top, "top"),
            (PaneDropEdge::Bottom, "bottom"),
            (PaneDropEdge::Replace, "replace"),
        ];
        for (variant, expected) in cases {
            let json = serde_json::to_string(&variant).expect("serialize");
            assert_eq!(json, format!("\"{expected}\""));
        }
    }

    fn sample_preset() -> PresetEntry {
        PresetEntry {
            id: "bug-triage".to_string(),
            name: "Bug triage".to_string(),
            description: Some("Free-form bug prompts".to_string()),
            source_repo_id: "repo-1".to_string(),
            prompt_sources: vec![
                PresetPromptSource::File,
                PresetPromptSource::Folder {
                    relative_path: "docs/plans/open-issues".to_string(),
                },
                PresetPromptSource::Inline,
            ],
            prompt_template: "{prompt}\n\nInvestigate.".to_string(),
            context_footer_lines: vec![FooterLine {
                label: "Client log".to_string(),
                variable: "client_log".to_string(),
            }],
            variables: vec![
                PresetVariable {
                    name: "client_log".to_string(),
                    label: "Client log".to_string(),
                    kind: PresetVariableKind::LiteralPath {
                        path: "${LOCALAPPDATA}/yaat/yaat-client.log".to_string(),
                    },
                    prompt_at_launch: false,
                    default: None,
                    optional: true,
                },
                PresetVariable {
                    name: "minutes".to_string(),
                    label: "Minutes".to_string(),
                    kind: PresetVariableKind::Text,
                    prompt_at_launch: true,
                    default: Some("60".to_string()),
                    optional: true,
                },
                PresetVariable {
                    name: "prod_log".to_string(),
                    label: "Production server log".to_string(),
                    kind: PresetVariableKind::Script {
                        cmd: "pwsh".to_string(),
                        args: vec![
                            "-NoProfile".to_string(),
                            "-File".to_string(),
                            "{repo_root}/tools/fetch-server-logs.ps1".to_string(),
                            "-Minutes".to_string(),
                            "{minutes}".to_string(),
                        ],
                        extract_pattern: Some(
                            r"Saved \d+ lines to (.+?)\s*$".to_string(),
                        ),
                        timeout_ms: 60_000,
                        skip_if_empty: Some("minutes".to_string()),
                    },
                    prompt_at_launch: false,
                    default: None,
                    optional: true,
                },
            ],
            branch_template: "bug/{date}/{index}".to_string(),
            session_label_template: Some("bug-{index}: {slug}".to_string()),
            default_use_worktree: Some(true),
            dangerously_skip_permissions: true,
            model: None,
            permission_mode: None,
            agent: Agent::Claude,
            codex_sandbox: None,
            tab_grouping: TabGroupingConfig::NewTab {
                layout: TabLayout::BalancedHorizontal,
                max_panes_per_tab: Some(6),
                tab_name_template: Some("bugs {date}".to_string()),
            },
            injector: InjectorTemplate {
                startup_delay_ms: 6000,
                pre_input: vec![
                    InjectorStep::Write {
                        data_b64: "G1ta".to_string(),
                    },
                    InjectorStep::Delay { ms: 200 },
                ],
                post_input: vec![InjectorStep::Text {
                    content: String::new(),
                    newline: true,
                }],
            },
            stagger_ms: 3000,
        }
    }

    #[test]
    fn preset_entry_round_trip() {
        let original = sample_preset();
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: PresetEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, decoded);
    }

    #[test]
    fn script_variable_kind_tagged() {
        let kind = PresetVariableKind::Script {
            cmd: "pwsh".to_string(),
            args: vec!["-File".to_string(), "x.ps1".to_string()],
            extract_pattern: Some("(\\S+)".to_string()),
            timeout_ms: 30_000,
            skip_if_empty: Some("minutes".to_string()),
        };
        let json = serde_json::to_string(&kind).expect("serialize");
        assert!(json.contains(r#""kind":"script""#));
        let decoded: PresetVariableKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, kind);
    }

    #[test]
    fn script_variable_kind_defaults() {
        let json = r#"{"kind":"script","cmd":"echo","args":["hi"]}"#;
        let decoded: PresetVariableKind = serde_json::from_str(json).expect("parse");
        match decoded {
            PresetVariableKind::Script {
                cmd,
                args,
                extract_pattern,
                timeout_ms,
                skip_if_empty,
            } => {
                assert_eq!(cmd, "echo");
                assert_eq!(args, vec!["hi"]);
                assert_eq!(extract_pattern, None);
                assert_eq!(timeout_ms, 60_000);
                assert_eq!(skip_if_empty, None);
            }
            other => panic!("expected Script kind, got {other:?}"),
        }
    }

    #[test]
    fn resolve_preset_scripts_message_tagged() {
        let msg = ClientMessage::ResolvePresetScripts {
            id: "r1".to_string(),
            target: PresetTarget::Repo {
                repo_id: "repo-1".to_string(),
            },
            preset_id: "bug-triage".to_string(),
            variable_values: vec![("minutes".to_string(), "30".to_string())],
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(json.contains(r#""type":"resolve_preset_scripts""#));
        let decoded: ClientMessage = serde_json::from_str(&json).expect("deserialize");
        let ClientMessage::ResolvePresetScripts {
            id, preset_id, ..
        } = decoded
        else {
            panic!("wrong variant");
        };
        assert_eq!(id, "r1");
        assert_eq!(preset_id, "bug-triage");
    }

    #[test]
    fn preset_scripts_resolved_message_tagged() {
        let msg = DaemonMessage::PresetScriptsResolved {
            id: "r1".to_string(),
            values: vec![("prod_log".to_string(), "C:/logs/p.log".to_string())],
            executed_commands: vec![ScriptCommandPreview {
                variable_name: "prod_log".to_string(),
                cmd: "pwsh".to_string(),
                args: vec!["-Minutes".to_string(), "30".to_string()],
            }],
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(json.contains(r#""type":"preset_scripts_resolved""#));
        let decoded: DaemonMessage = serde_json::from_str(&json).expect("deserialize");
        let DaemonMessage::PresetScriptsResolved {
            values,
            executed_commands,
            ..
        } = decoded
        else {
            panic!("wrong variant");
        };
        assert_eq!(values.len(), 1);
        assert_eq!(executed_commands.len(), 1);
        assert_eq!(executed_commands[0].variable_name, "prod_log");
    }

    #[test]
    fn injector_step_variants_tagged() {
        let steps = vec![
            InjectorStep::Delay { ms: 200 },
            InjectorStep::Write {
                data_b64: "G1ta".to_string(),
            },
            InjectorStep::Text {
                content: "hello".to_string(),
                newline: true,
            },
        ];
        let json = serde_json::to_string(&steps).expect("serialize");
        assert!(json.contains(r#""kind":"delay""#));
        assert!(json.contains(r#""kind":"write""#));
        assert!(json.contains(r#""kind":"text""#));
        let decoded: Vec<InjectorStep> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(steps, decoded);
    }

    #[test]
    fn preset_target_tagged() {
        let repo = PresetTarget::Repo {
            repo_id: "r1".to_string(),
        };
        let json = serde_json::to_string(&repo).expect("serialize");
        assert!(json.contains(r#""kind":"repo""#));
        assert!(json.contains(r#""repo_id":"r1""#));
    }

    #[test]
    fn preset_entry_omits_optional_fields_when_default() {
        let minimal = r#"{
            "id": "smoke",
            "name": "Smoke",
            "prompt_sources": [{"kind":"inline"}],
            "prompt_template": "{prompt}",
            "branch_template": "tmp/{index}",
            "tab_grouping": {"kind":"none"},
            "injector": {"startup_delay_ms": 0, "pre_input": [], "post_input": []}
        }"#;
        let preset: PresetEntry = serde_json::from_str(minimal).expect("parse minimal preset");
        assert_eq!(preset.id, "smoke");
        assert_eq!(preset.stagger_ms, 3000);
        assert!(!preset.dangerously_skip_permissions);
        assert!(preset.variables.is_empty());
        assert!(preset.context_footer_lines.is_empty());
        assert_eq!(preset.default_use_worktree, None);
        assert_eq!(preset.agent, Agent::Claude);
        assert_eq!(preset.codex_sandbox, None);
    }

    #[test]
    fn spawn_request_round_trip_with_injector() {
        let req = SpawnRequest {
            label: Some("test".to_string()),
            target: SpawnTarget::Single {
                repo_id: "r1".to_string(),
                branch_name: "bug/1".to_string(),
                base_branch: None,
                use_worktree: true,
            },
            mode: SessionMode::Interactive,
            initial_prompt: None,
            dangerously_skip_permissions: true,
            agent: Agent::Claude,
            model: None,
            permission_mode: None,
            codex_sandbox: None,
            extra_env: vec![],
            prompt_injector: Some(PromptInjector {
                steps: vec![
                    InjectorStep::Delay { ms: 6000 },
                    InjectorStep::Write {
                        data_b64: "G1ta".to_string(),
                    },
                    InjectorStep::Text {
                        content: "investigate".to_string(),
                        newline: false,
                    },
                ],
            }),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let decoded: SpawnRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, decoded);
    }

    #[test]
    fn spawn_request_round_trip_codex() {
        let req = SpawnRequest {
            label: Some("codex-test".to_string()),
            target: SpawnTarget::Workspace {
                workspace_id: "ws1".to_string(),
                branch_name: "feat/x".to_string(),
                base_branch: None,
                use_worktree: true,
            },
            mode: SessionMode::Interactive,
            initial_prompt: Some("refactor authentication".to_string()),
            dangerously_skip_permissions: false,
            agent: Agent::Codex,
            model: Some("gpt-5.1-codex".to_string()),
            permission_mode: None,
            codex_sandbox: Some(CodexSandbox::WorkspaceWrite),
            extra_env: vec![],
            prompt_injector: None,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let decoded: SpawnRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, decoded);
        assert!(json.contains(r#""agent":"codex""#));
        assert!(json.contains(r#""codex_sandbox":"workspace-write""#));
    }

    #[test]
    fn repo_entry_decodes_without_last_agent() {
        // Existing state.json files lack `last_agent`; serde default must
        // produce `None` so the daemon can load them after the version bump.
        let legacy = r#"{
            "id": "r1",
            "name": "yaat",
            "path": "X:/dev/yaat",
            "default_branch": "main"
        }"#;
        let repo: RepoEntry = serde_json::from_str(legacy).expect("parse legacy repo");
        assert_eq!(repo.last_agent, None);
        assert!(repo.default_use_worktree, "default_use_worktree default");
    }

    #[test]
    fn tab_entry_legacy_grid_field_migrates_to_content_grid() {
        // Old state.json files (pre-PROTOCOL_VERSION 8) wrote `grid` at the
        // top level. The custom Deserialize impl must keep loading those.
        let legacy = r#"{
            "id": "tab-1",
            "name": "Main",
            "grid": {"kind":"pane","pane_id":"p1","session_id":"s1"},
            "created_at": "2024-01-01T00:00:00Z"
        }"#;
        let tab: TabEntry = serde_json::from_str(legacy).expect("parse legacy");
        let TabContent::Grid { grid } = &tab.content else {
            panic!("expected grid variant");
        };
        match grid {
            GridNode::Pane {
                pane_id,
                session_id,
            } => {
                assert_eq!(pane_id, "p1");
                assert_eq!(session_id.as_deref(), Some("s1"));
            }
            GridNode::Split { .. } => panic!("expected pane"),
        }
    }

    #[test]
    fn tab_entry_modern_content_field_loads_directly() {
        let modern = r#"{
            "id": "tab-2",
            "name": "Modern",
            "content": {"kind":"grid","grid":{"kind":"pane","pane_id":"px","session_id":null}},
            "created_at": "2024-01-01T00:00:00Z"
        }"#;
        let tab: TabEntry = serde_json::from_str(modern).expect("parse modern");
        let TabContent::Grid { grid } = &tab.content else {
            panic!("expected grid variant");
        };
        assert!(matches!(grid, GridNode::Pane { pane_id, .. } if pane_id == "px"));
    }

    #[test]
    fn tab_entry_missing_both_fields_errors() {
        let bad = r#"{"id":"x","name":"x","created_at":"2024-01-01T00:00:00Z"}"#;
        let res: Result<TabEntry, _> = serde_json::from_str(bad);
        assert!(res.is_err());
    }

    #[test]
    fn hello_decodes_with_scalar_only() {
        // A v15-era client predates `protocol_versions`. Its Hello carries
        // only the scalar; `#[serde(default)]` on the new vec keeps the
        // message decodable by post-Phase-A daemons.
        let legacy = r#"{
            "type": "hello",
            "protocol_version": 15,
            "auth_token": "abc"
        }"#;
        let parsed: ClientMessage = serde_json::from_str(legacy).expect("parse legacy hello");
        match parsed {
            ClientMessage::Hello {
                protocol_version,
                protocol_versions,
                auth_token,
            } => {
                assert_eq!(protocol_version, 15);
                assert!(protocol_versions.is_empty());
                assert_eq!(auth_token, "abc");
            }
            _ => panic!("expected Hello"),
        }
    }

    #[test]
    fn hello_decodes_with_range_field() {
        let new_shape = r#"{
            "type": "hello",
            "protocol_version": 16,
            "protocol_versions": [16, 15],
            "auth_token": "abc"
        }"#;
        let parsed: ClientMessage = serde_json::from_str(new_shape).expect("parse new hello");
        match parsed {
            ClientMessage::Hello {
                protocol_version,
                protocol_versions,
                ..
            } => {
                assert_eq!(protocol_version, 16);
                assert_eq!(protocol_versions, vec![16, 15]);
            }
            _ => panic!("expected Hello"),
        }
    }

    #[test]
    fn welcome_decodes_with_scalar_only() {
        // A v15-era daemon sends Welcome without `supported_versions`. The
        // new client still decodes it via `#[serde(default)]`.
        let legacy = r#"{"type":"welcome","protocol_version":15}"#;
        let parsed: DaemonMessage = serde_json::from_str(legacy).expect("parse legacy welcome");
        match parsed {
            DaemonMessage::Welcome {
                protocol_version,
                supported_versions,
            } => {
                assert_eq!(protocol_version, 15);
                assert!(supported_versions.is_empty());
            }
            _ => panic!("expected Welcome"),
        }
    }

    #[test]
    fn inbound_client_message_known_passes_through() {
        let json = r#"{"type":"list_repos"}"#;
        let parsed = InboundClientMessage::from_json_str(json).expect("parse");
        let InboundClientMessage::Known(ClientMessage::ListRepos) = parsed else {
            panic!("expected Known(ListRepos), got {parsed:?}");
        };
    }

    #[test]
    fn inbound_client_message_unknown_tag_captures_payload() {
        // A future v(N+1) message type that the current daemon has never
        // seen. The wrapper preserves the type_tag and the raw JSON so
        // diagnostics can log what got ignored.
        let json = r#"{"type":"future_only_message","new_field":42}"#;
        let parsed = InboundClientMessage::from_json_str(json).expect("parse");
        let InboundClientMessage::Unknown { type_tag, raw } = parsed else {
            panic!("expected Unknown, got {parsed:?}");
        };
        assert_eq!(type_tag, "future_only_message");
        assert_eq!(raw.get("new_field").and_then(serde_json::Value::as_i64), Some(42));
    }

    #[test]
    fn inbound_client_message_missing_type_field_captures_marker() {
        let json = r#"{"some_field":"value"}"#;
        let parsed = InboundClientMessage::from_json_str(json).expect("parse");
        let InboundClientMessage::Unknown { type_tag, .. } = parsed else {
            panic!("expected Unknown");
        };
        assert_eq!(type_tag, "<missing-type>");
    }

    #[test]
    fn inbound_client_message_invalid_json_errors() {
        let res = InboundClientMessage::from_json_str("not json");
        assert!(res.is_err());
    }

    #[test]
    fn inbound_daemon_message_unknown_tag_captures_payload() {
        let json = r#"{"type":"future_only_broadcast","detail":"hi"}"#;
        let parsed = InboundDaemonMessage::from_json_str(json).expect("parse");
        let InboundDaemonMessage::Unknown { type_tag, .. } = parsed else {
            panic!("expected Unknown");
        };
        assert_eq!(type_tag, "future_only_broadcast");
    }

    #[test]
    fn welcome_decodes_with_range_field() {
        let new_shape = r#"{"type":"welcome","protocol_version":15,"supported_versions":[16,15]}"#;
        let parsed: DaemonMessage = serde_json::from_str(new_shape).expect("parse new welcome");
        match parsed {
            DaemonMessage::Welcome {
                protocol_version,
                supported_versions,
            } => {
                assert_eq!(protocol_version, 15);
                assert_eq!(supported_versions, vec![16, 15]);
            }
            _ => panic!("expected Welcome"),
        }
    }

    #[test]
    fn preset_entry_decodes_without_agent() {
        // User-edited `.rustling-tulip/presets.json` files lack `agent`; the
        // serde default keeps them loadable as claude presets.
        let legacy = r#"{
            "id": "smoke",
            "name": "Smoke",
            "prompt_sources": [{"kind":"inline"}],
            "prompt_template": "{prompt}",
            "branch_template": "tmp/{index}",
            "tab_grouping": {"kind":"none"},
            "injector": {"startup_delay_ms": 0, "pre_input": [], "post_input": []}
        }"#;
        let preset: PresetEntry = serde_json::from_str(legacy).expect("parse legacy preset");
        assert_eq!(preset.agent, Agent::Claude);
        assert_eq!(preset.codex_sandbox, None);
    }
}
