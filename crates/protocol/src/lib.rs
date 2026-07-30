//! Wire protocol between the rustling-tulip daemon and its clients.
//!
//! All messages are JSON-encoded. Both directions are tagged enums (`type` field)
//! so that adding a new variant is backward-compatible: older peers either match a
//! known variant or surface an `Error` for the unknown one.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

include!(concat!(env!("OUT_DIR"), "/protocol_version.rs"));

/// mDNS service type the daemon advertises and clients browse for LAN
/// discovery. Single source of truth so the advertiser and the browser can't
/// drift (a mismatch would silently break discovery).
pub const MDNS_SERVICE_TYPE: &str = "_rustling-tulip._tcp.local.";

/// mDNS TXT key carrying the LAN cert's SHA-256 fingerprint (lowercase hex), so
/// a discovering client can pin it before pairing.
pub const MDNS_TXT_FINGERPRINT: &str = "fp";

/// mDNS TXT key carrying the host's human-readable name (e.g. its hostname).
pub const MDNS_TXT_NAME: &str = "name";

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
    Cursor,
}

impl Agent {
    /// Lowercase tag for UI badges and log messages.
    #[must_use]
    pub fn as_label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
        }
    }

    /// All agents the daemon knows how to spawn. Used by callers that need to
    /// iterate (e.g. orphan liveness checks that match the executable name
    /// against every known agent's program name).
    #[must_use]
    pub fn all() -> &'static [Self] {
        &[Self::Claude, Self::Codex, Self::Cursor]
    }
}

/// Per-agent configuration carried on [`SpawnRequest`], [`SpawnConfig`], and
/// [`PresetEntry`]. The discriminant doubles as the agent selector — its
/// `agent()` accessor returns the matching [`Agent`] variant — so callers do
/// not need to thread `Agent` separately. Adding a new agent means adding a
/// variant here and an [`Agent`] variant; the wire shape stays the same.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentOptions {
    Claude {
        /// Claude's `--permission-mode <value>` flag. Ignored (and the flag
        /// omitted) when [`SpawnRequest::dangerously_skip_permissions`] is true.
        #[serde(default)]
        permission_mode: Option<PermissionMode>,
    },
    Codex {
        /// Codex's `--sandbox <value>` flag. Ignored when
        /// [`SpawnRequest::dangerously_skip_permissions`] is true (yolo
        /// overrides sandbox).
        #[serde(default)]
        sandbox: Option<CodexSandbox>,
    },
    Cursor {
        /// When `true`, cursor spawns with `--plan` (read-only / planning
        /// mode). Ignored when [`SpawnRequest::dangerously_skip_permissions`]
        /// is true (yolo bypasses sandboxing entirely).
        #[serde(default)]
        plan_mode: bool,
        /// Cursor's `--sandbox enabled|disabled` flag. Ignored when
        /// `dangerously_skip_permissions` is true.
        #[serde(default)]
        sandbox: Option<CursorSandbox>,
    },
}

impl AgentOptions {
    /// Which [`Agent`] this variant selects. Derived from the variant tag.
    #[must_use]
    pub fn agent(&self) -> Agent {
        match self {
            Self::Claude { .. } => Agent::Claude,
            Self::Codex { .. } => Agent::Codex,
            Self::Cursor { .. } => Agent::Cursor,
        }
    }
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self::Claude {
            permission_mode: None,
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

/// Cursor sandbox policy. Maps to `cursor-agent --sandbox <value>`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CursorSandbox {
    Enabled,
    Disabled,
}

impl CursorSandbox {
    /// CLI value as expected by `cursor-agent --sandbox <X>`.
    #[must_use]
    pub fn as_cli_arg(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AppearanceOverrides {
    /// Sidebar row + pane accent color. `None` inherits from the next
    /// appearance level.
    #[serde(default)]
    pub accent_color: Option<String>,
    /// Background color for xterm's canvas.
    #[serde(default)]
    pub terminal_background_color: Option<String>,
    /// Background color for the shell frame/chrome around the terminal.
    #[serde(default)]
    pub terminal_frame_color: Option<String>,
    /// Terminal font family. `None` inherits the next level's family.
    #[serde(default)]
    pub terminal_font_family: Option<String>,
    /// Terminal font size in px.
    #[serde(default)]
    pub terminal_font_size: Option<u16>,
    /// Terminal normal text weight override.
    #[serde(default)]
    pub terminal_font_bold: Option<bool>,
}

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
    /// Appearance defaults inherited by sessions spawned from this repo.
    #[serde(default)]
    pub appearance: AppearanceOverrides,
    /// Last agent spawned against this repo. Drives the spawn-dialog default
    /// so repeated launches don't force the user to re-pick. `None` for repos
    /// that have never been launched (or were last touched before this field
    /// existed, hence the serde default).
    #[serde(default)]
    pub last_agent: Option<Agent>,
    /// Full spawn config captured from the last successful single-repo spawn
    /// against this repo. Drives "Launch last again" (double-click on the
    /// sidebar row, or the matching context-menu submenu). `None` for repos
    /// that have never been launched.
    #[serde(default)]
    pub last_spawn_config: Option<SpawnConfig>,
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
    /// Appearance defaults inherited by sessions spawned from this workspace.
    #[serde(default)]
    pub appearance: AppearanceOverrides,
    /// Full spawn config captured from the last successful workspace spawn.
    /// Drives "Launch last again" for the workspace row. `None` for
    /// workspaces that have never been launched.
    #[serde(default)]
    pub last_spawn_config: Option<SpawnConfig>,
}

/// Tagged reference to either a repo or a workspace by id. Used by the
/// sidebar's manual container order (a single flat list mixing both
/// kinds — see [`ClientMessage::ReorderContainers`]). The `kind` tag is
/// `snake_case` to match the rest of the wire vocabulary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum ContainerRef {
    Repo(String),
    Workspace(String),
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
    /// Plain shell launched outside any registered repo or workspace.
    Standalone,
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
#[expect(
    clippy::struct_excessive_bools,
    reason = "SessionSnapshot is the canonical session shape on the wire — \
              each bool maps to an orthogonal lifecycle state (orphan, abandoned, \
              parked, has-worktree) the UI surfaces independently."
)]
pub struct SessionSnapshot {
    pub id: String,
    pub label: String,
    /// Explicit label set through the app's session rename command. When
    /// `None`, clients may choose a dynamic display label, such as a terminal
    /// title emitted by the running program, while keeping `label` as the
    /// canonical repo/workspace fallback.
    #[serde(default)]
    pub user_label: Option<String>,
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
    /// Window title last emitted by the agent via OSC 0/1/2 escape sequences.
    /// Distinct from `label` so clients can preserve explicit user labels and
    /// still expose dynamic terminal titles when no user override is set.
    /// `None` until the agent emits its first title.
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
    /// Current working directory last reported by the session. Plain shells
    /// start with their spawn directory and update when the shell emits OSC 7
    /// after prompt redraws. `None` for older sidecars or non-PTY paths that
    /// have not reported a cwd.
    #[serde(default)]
    pub current_cwd: Option<String>,
    /// Session-level appearance overrides. `None` fields inherit from the
    /// owning repo/workspace, then the app default.
    #[serde(default)]
    pub appearance: AppearanceOverrides,
    /// True when the session was launched with approvals/permission prompts
    /// bypassed (`--dangerously-skip-permissions` for Claude or `--yolo` for
    /// Codex). Clients use this to keep elevated authority visible after the
    /// spawn dialog closes.
    #[serde(default)]
    pub elevated_authority: bool,
    /// True when the session was spawned with `use_worktree = true`, i.e.
    /// the daemon created (or reused) a per-session git worktree under
    /// `<repo>.wt/<branch>`. Drives the close-context-menu's "delete
    /// worktree" choice: only when this is true does the menu offer
    /// per-session worktree deletion. For orphans with no stored spawn config we conservatively default to
    /// `false` (the cleanup choice is hidden, but the worktree is still
    /// on disk for the user to clean up manually).
    #[serde(default)]
    pub has_per_session_worktree: bool,
    /// True when the session is parked: its process has stopped but the
    /// worktree is retained and the session remains in the sidebar so the
    /// user can resume it later. Set via `ParkSession`; cleared implicitly
    /// when the session is discarded or a new session is spawned from it.
    #[serde(default)]
    pub is_inactive: bool,
    /// Absolute on-disk paths of the per-session worktrees used by this
    /// session. Empty when `has_per_session_worktree` is false. Populated at
    /// spawn time; carried on the snapshot so `ListWorktrees` can mark which
    /// worktrees are currently in active use.
    #[serde(default)]
    pub worktree_paths: Vec<String>,
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
        /// runs there directly.
        use_worktree: bool,
        /// How to resolve a dirty working tree on an in-place
        /// (`use_worktree = false`) switch to a *different* branch. `None`
        /// (default) makes the daemon decline a dirty switch and emit
        /// [`DaemonMessage::CheckoutConfirmRequired`] so the client can confirm;
        /// the confirmed retry resends with `Carry` or `Stash`.
        #[serde(default)]
        checkout_strategy: Option<CheckoutStrategy>,
        /// What to do when a worktree already exists at the target path.
        /// Defaults to [`WorktreeReusePolicy::Reuse`] — the historical
        /// behavior, and the right answer for non-interactive callers
        /// (presets, duplicate, resume) that never saw a collision prompt.
        #[serde(default)]
        worktree_reuse: WorktreeReusePolicy,
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
        /// See [`SpawnTarget::Single::worktree_reuse`]; applied to every member.
        #[serde(default)]
        worktree_reuse: WorktreeReusePolicy,
    },
    Standalone {
        /// Absolute directory for a non-repo shell. `None` lets the daemon
        /// choose its platform default, usually the user's home directory.
        #[serde(default)]
        cwd: Option<String>,
    },
}

/// How to resolve a dirty working tree when checking out a branch in place
/// (`SpawnTarget::Single { use_worktree: false }`). Sent on the confirmed retry
/// after the daemon asked via [`DaemonMessage::CheckoutConfirmRequired`]. Grows
/// a catch-all so an older daemon decoding a newer choice falls back to the
/// safe (decline) path.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutStrategy {
    /// Switch with `git checkout`, carrying uncommitted changes across (git
    /// refuses if they would conflict with the target branch).
    Carry,
    /// Stash uncommitted changes (including untracked), switch, and leave the
    /// stash for the user to pop.
    Stash,
    #[serde(other)]
    Unknown,
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
    /// Per-agent options. The variant tag doubles as the agent selector —
    /// `agent_options.agent()` yields the [`Agent`] kind that drives executable
    /// resolution. Each variant carries only the fields that apply to that
    /// agent's CLI.
    pub agent_options: AgentOptions,
    /// Optional model override. When `None`, the CLI's default applies.
    /// Sent as `--model <id>` to both claude and codex.
    pub model: Option<String>,
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

impl SpawnRequest {
    /// Convenience accessor — the agent kind selected by `agent_options`.
    #[must_use]
    pub fn agent(&self) -> Agent {
        self.agent_options.agent()
    }
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
///
/// Decoding accepts both the current shape (with `agent_options`) and the
/// legacy v18 shape (with flat `agent` / `permission_mode` / `codex_sandbox`).
/// Serialization always emits the current shape, so `state.json` entries
/// rewritten by the daemon are upgraded transparently.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SpawnConfig {
    pub target: SpawnTarget,
    pub mode: SessionMode,
    pub dangerously_skip_permissions: bool,
    pub agent_options: AgentOptions,
    pub model: Option<String>,
    #[serde(default)]
    pub extra_env: Vec<(String, String)>,
}

impl<'de> Deserialize<'de> for SpawnConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Helper {
            target: SpawnTarget,
            mode: SessionMode,
            dangerously_skip_permissions: bool,
            model: Option<String>,
            #[serde(default)]
            extra_env: Vec<(String, String)>,
            #[serde(default)]
            agent_options: Option<AgentOptions>,
            #[serde(default)]
            agent: Option<Agent>,
            #[serde(default)]
            permission_mode: Option<PermissionMode>,
            #[serde(default)]
            codex_sandbox: Option<CodexSandbox>,
        }
        let helper = Helper::deserialize(deserializer)?;
        let agent_options =
            helper
                .agent_options
                .unwrap_or_else(|| match helper.agent.unwrap_or_default() {
                    Agent::Claude => AgentOptions::Claude {
                        permission_mode: helper.permission_mode,
                    },
                    Agent::Codex => AgentOptions::Codex {
                        sandbox: helper.codex_sandbox,
                    },
                    // Cursor was introduced in v20 alongside `agent_options`,
                    // so legacy (v18) JSON should never carry `agent: "cursor"`.
                    // If it somehow does (manually-edited state file), fall
                    // back to the default Cursor variant.
                    Agent::Cursor => AgentOptions::Cursor {
                        plan_mode: false,
                        sandbox: None,
                    },
                });
        Ok(Self {
            target: helper.target,
            mode: helper.mode,
            dangerously_skip_permissions: helper.dangerously_skip_permissions,
            agent_options,
            model: helper.model,
            extra_env: helper.extra_env,
        })
    }
}

impl SpawnTarget {
    /// Reset any worktree-collision decision back to
    /// [`WorktreeReusePolicy::Reuse`].
    ///
    /// `RecreateFromBase` is a one-shot answer the user gave about one
    /// specific collision they were shown. It must not survive into a
    /// persisted [`SpawnConfig`], where "launch last again" or a duplicate
    /// would replay it and delete a worktree nobody was asked about.
    #[must_use]
    pub fn with_reuse_policy_reset(mut self) -> Self {
        match &mut self {
            Self::Single { worktree_reuse, .. } | Self::Workspace { worktree_reuse, .. } => {
                *worktree_reuse = WorktreeReusePolicy::Reuse;
            }
            Self::Standalone { .. } => {}
        }
        self
    }
}

impl SpawnConfig {
    /// Capture the spawn-time config from a [`SpawnRequest`], dropping the
    /// fields a duplicate doesn't want to inherit.
    #[must_use]
    pub fn from_request(req: &SpawnRequest) -> Self {
        Self {
            target: req.target.clone().with_reuse_policy_reset(),
            mode: req.mode,
            dangerously_skip_permissions: req.dangerously_skip_permissions,
            agent_options: req.agent_options.clone(),
            model: req.model.clone(),
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
            agent_options: self.agent_options.clone(),
            model: self.model.clone(),
            extra_env: self.extra_env.clone(),
            prompt_injector: None,
        }
    }

    /// Convenience accessor — the agent kind selected by `agent_options`.
    #[must_use]
    pub fn agent(&self) -> Agent {
        self.agent_options.agent()
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
    /// Promote source to a full-edge sibling of the *entire* destination
    /// tab grid, on the named outer edge. Emitted when the cursor drops in
    /// the narrow outermost band of a pane that touches that outer edge of
    /// the grid — bypasses per-pane nesting so the source becomes a
    /// full-height (Left/Right) or full-width (Top/Bottom) column/row at
    /// the topmost split level. `dst_pane_id` is ignored for these
    /// variants; the daemon wraps the destination tab's root.
    OuterLeft,
    OuterRight,
    OuterTop,
    OuterBottom,
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
    Grid { cols: u32 },
    /// Forward-compat fallback: a layout variant the current build doesn't
    /// know. Daemon dispatch falls back to `Balanced`. Serializes as
    /// `{"kind":"unknown"}` if ever round-tripped — daemon never
    /// constructs this directly, so the wire shouldn't see it.
    #[serde(other)]
    Unknown,
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
    /// A Monaco-backed diff view. Identified by the
    /// (`repo`, `path`, `against`, `worktree_path`) tuple so clicking the
    /// same file in the source-control sidebar twice just focuses the
    /// existing tab instead of opening a duplicate, while two different
    /// worktrees of the same repo get their own tabs.
    Diff {
        repo_id: String,
        path: String,
        /// Selects the "old" side of the diff. Mirrors
        /// [`ClientMessage::GetFileDiff::against`]: `None` = worktree vs
        /// index (the CHANGES bucket), `Some("HEAD")` = index vs HEAD (the
        /// STAGED bucket), `Some(sha)` = that commit's content as the old
        /// side.
        against: Option<String>,
        /// `None` = the registered repo's main tree; `Some(path)` = a
        /// session worktree. Two diff tabs for the same (`repo_id`, `path`,
        /// `against`) but different `worktree_path` values are separate tabs.
        #[serde(default)]
        worktree_path: Option<String>,
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
    /// Forward-compat fallback: a step kind the current build doesn't know.
    /// The injector runtime skips Unknown steps with a warn log. Daemon
    /// never constructs this directly.
    #[serde(other)]
    Unknown,
}

/// Scripted PTY input fed to a freshly spawned interactive session after the
/// PTY comes up. Used by the preset launcher to enter Claude's plan mode and
/// submit a prompt without relying on the `-p` CLI arg (which submits
/// immediately and disables plan mode). Standalone outside presets: any
/// `SpawnRequest` can carry one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptInjector {
    pub steps: Vec<InjectorStep>,
    /// Optional substring (case-insensitive) the daemon expects to see in
    /// the rendered TUI after the pre-input portion of `steps` runs. When
    /// set, the daemon re-sends the pre-input keystrokes if the marker
    /// doesn't appear in a short window. Copied from
    /// [`InjectorTemplate::verify_mode_marker`] by `build_injector`. The
    /// pre-input portion is identified at runtime as the contiguous run
    /// of steps after the leading startup `Delay` (if any) and before the
    /// prompt-text `Text` step.
    #[serde(default)]
    pub verify_mode_marker: Option<String>,
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
    /// Optional substring (case-insensitive) the daemon expects to see in
    /// the rendered TUI after `pre_input` runs. When set, the injector
    /// scans PTY output for the marker; if absent within a short window,
    /// it re-sends `pre_input` and checks again (up to a few attempts)
    /// before falling through. Lets a Plan-Mode preset self-heal when
    /// the Shift+Tab keystrokes are dropped because Claude wasn't yet
    /// listening. `None` (or absent in JSON) disables verification.
    #[serde(default)]
    pub verify_mode_marker: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PresetTarget {
    Repo { repo_id: String },
    Workspace { workspace_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PresetLaunchJobStatus {
    Resolving,
    RunningScripts,
    Spawning,
    Completed,
    Cancelled,
    Failed,
    #[serde(other)]
    Unknown,
}

fn default_preset_launch_job_status() -> PresetLaunchJobStatus {
    PresetLaunchJobStatus::Resolving
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PresetLaunchJobSnapshot {
    pub job_id: String,
    pub preset_id: String,
    pub target: PresetTarget,
    #[serde(default)]
    pub total: u32,
    #[serde(default)]
    pub launched: u32,
    #[serde(default)]
    pub created_session_ids: Vec<String>,
    #[serde(default)]
    pub created_tab_ids: Vec<String>,
    #[serde(default = "default_preset_launch_job_status")]
    pub status: PresetLaunchJobStatus,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub current_tab_id: Option<String>,
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
    /// User types a comma-separated list of GitHub issue numbers, with
    /// contiguous inclusive ranges allowed (e.g. `123-127, 131, 134-136`).
    /// The daemon expands the spec into one prompt per issue; each prompt is
    /// a `GitHub issue #<n>` reference (the spawned session fetches the issue
    /// body itself via `gh` / the GitHub MCP).
    GithubIssues,
}

/// Concrete prompt-source picked at launch time. Mirrors
/// [`PresetPromptSource`] but with the chosen paths/text.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LaunchPresetSource {
    File {
        path: String,
    },
    Folder {
        path: String,
    },
    Inline {
        prompts: Vec<String>,
    },
    /// Raw issue spec as typed by the user (e.g. `123-127, 131, 134-136`).
    /// The daemon parses + expands it at resolve time.
    GithubIssues {
        spec: String,
    },
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
    /// A checkbox in the launch wizard. When checked, resolves to `value`
    /// (with the same `${ENV}` / `%ENV%` / `{repo_root}` expansion as
    /// `LiteralPath`); when unchecked, resolves to an empty string so any
    /// footer line referencing it is omitted. The variable's `default`
    /// (`"true"` / `"false"`) sets the initial checkbox state. Lets a preset
    /// offer an opt-in attachment (e.g. a log file) instead of always
    /// appending it.
    Toggle { value: String },
    /// Forward-compat fallback: a variable kind the current build doesn't
    /// know. Preset resolution treats this as a hard error since we can't
    /// compute the value. Daemon never constructs this directly.
    #[serde(other)]
    Unknown,
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
    /// Forward-compat fallback: a layout the current build doesn't know.
    /// Preset rendering falls back to `BalancedHorizontal` (the safest
    /// known shape — fans the panes out without making strong claims
    /// about orientation). Daemon never constructs this directly.
    #[serde(other)]
    Unknown,
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
///
/// Decoding accepts both the current shape (with `agent_options`) and the
/// legacy v18 shape (with flat `agent` / `permission_mode` / `codex_sandbox`).
/// Serialization always emits the current shape, so files re-saved by the
/// daemon are upgraded transparently. See the manual `Deserialize` impl below.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
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
    /// Per-agent options. See [`AgentOptions`].
    pub agent_options: AgentOptions,
    pub tab_grouping: TabGroupingConfig,
    pub injector: InjectorTemplate,
    /// Pause between successive session spawns. Default 3000ms matches
    /// yaat's pacing; the value avoids overlapping git worktree adds.
    #[serde(default = "default_stagger_ms")]
    pub stagger_ms: u32,
}

impl<'de> Deserialize<'de> for PresetEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Helper holds every field accepted on the wire, including the legacy
        // flat fields (`agent` / `permission_mode` / `codex_sandbox`) that
        // pre-v19 preset files may carry. After decode we normalize into the
        // canonical `agent_options` shape.
        #[derive(Deserialize)]
        struct Helper {
            id: String,
            name: String,
            #[serde(default)]
            description: Option<String>,
            #[serde(default)]
            source_repo_id: String,
            prompt_sources: Vec<PresetPromptSource>,
            prompt_template: String,
            #[serde(default)]
            context_footer_lines: Vec<FooterLine>,
            #[serde(default)]
            variables: Vec<PresetVariable>,
            branch_template: String,
            #[serde(default)]
            session_label_template: Option<String>,
            #[serde(default)]
            default_use_worktree: Option<bool>,
            #[serde(default)]
            dangerously_skip_permissions: bool,
            #[serde(default)]
            model: Option<String>,
            // Canonical (v19+) shape.
            #[serde(default)]
            agent_options: Option<AgentOptions>,
            // Legacy (≤v18) flat fields. Read-only; ignored when
            // `agent_options` is present.
            #[serde(default)]
            agent: Option<Agent>,
            #[serde(default)]
            permission_mode: Option<PermissionMode>,
            #[serde(default)]
            codex_sandbox: Option<CodexSandbox>,
            tab_grouping: TabGroupingConfig,
            injector: InjectorTemplate,
            #[serde(default = "default_stagger_ms")]
            stagger_ms: u32,
        }

        let helper = Helper::deserialize(deserializer)?;
        let agent_options =
            helper
                .agent_options
                .unwrap_or_else(|| match helper.agent.unwrap_or_default() {
                    Agent::Claude => AgentOptions::Claude {
                        permission_mode: helper.permission_mode,
                    },
                    Agent::Codex => AgentOptions::Codex {
                        sandbox: helper.codex_sandbox,
                    },
                    // Cursor was introduced in v20 alongside `agent_options`,
                    // so legacy (v18) JSON should never carry `agent: "cursor"`.
                    // If it somehow does (manually-edited state file), fall
                    // back to the default Cursor variant.
                    Agent::Cursor => AgentOptions::Cursor {
                        plan_mode: false,
                        sandbox: None,
                    },
                });
        Ok(Self {
            id: helper.id,
            name: helper.name,
            description: helper.description,
            source_repo_id: helper.source_repo_id,
            prompt_sources: helper.prompt_sources,
            prompt_template: helper.prompt_template,
            context_footer_lines: helper.context_footer_lines,
            variables: helper.variables,
            branch_template: helper.branch_template,
            session_label_template: helper.session_label_template,
            default_use_worktree: helper.default_use_worktree,
            dangerously_skip_permissions: helper.dangerously_skip_permissions,
            model: helper.model,
            agent_options,
            tab_grouping: helper.tab_grouping,
            injector: helper.injector,
            stagger_ms: helper.stagger_ms,
        })
    }
}

fn default_stagger_ms() -> u32 {
    3000
}

/// One cloneable layout offered in the first-connect chooser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClonableLayout {
    pub client_id: String,
    /// Display label (e.g. the other machine's hostname), if it reported one.
    pub name: Option<String>,
}

/// How a client wants to seed its brand-new per-client layout, chosen at first
/// connect. Grows over time, so it carries a catch-all `Unknown` so an older
/// daemon keeps decoding a newer client's choice (falls back to empty).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InitLayoutKind {
    /// Start with no tabs; curate from scratch.
    Empty,
    /// Adopt the pre-per-client global layout (offered once after upgrade).
    CloneLegacy,
    /// Deep-copy another client's current layout (fresh pane ids, same
    /// session ids).
    CloneClient { client_id: String },
    /// One tab holding a pane per currently-running session.
    AllSessions,
    #[serde(other)]
    Unknown,
}

/// Why a pairing window ended, carried by [`DaemonMessage::PairingEnded`] so the
/// host UI can clear (or annotate) the displayed code. Serialized as a
/// `snake_case` string with a catch-all so an older client keeps decoding a
/// newer reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingEndReason {
    /// A device completed pairing with the code.
    Paired,
    /// The code's time-to-live elapsed before anyone paired.
    Expired,
    /// The host cancelled pairing.
    Cancelled,
    /// Too many wrong codes were submitted; the window was invalidated.
    TooManyAttempts,
    #[serde(other)]
    Unknown,
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
        /// Stable per-install client identity. The daemon keys each client's
        /// tab/pane layout on this. `None` (older clients) falls back to a
        /// shared legacy layout so they keep working. `#[serde(default)]` keeps
        /// this additive — no protocol bump.
        #[serde(default)]
        client_id: Option<String>,
        /// Human-readable label for this client (e.g. the machine hostname),
        /// shown in another client's "clone layout" chooser. Optional.
        #[serde(default)]
        client_name: Option<String>,
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
    /// Parse a `.code-workspace` file at `path`, resolve folder paths, and
    /// match against registered repos. The daemon replies with
    /// [`DaemonMessage::VscodeWorkspaceParsed`] on success or
    /// [`DaemonMessage::Error`] on I/O or parse failure.
    ParseVscodeWorkspace {
        path: String,
    },
    /// Scan a directory for `.code-workspace` files. The daemon replies with
    /// [`DaemonMessage::VscodeWorkspacesScanned`]. Used by shell cwd actions
    /// so the UI can offer "Add workspace" only when the cwd actually has a
    /// VS Code workspace file.
    ScanVscodeWorkspaces {
        path: String,
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
    /// Single-repo counterpart of [`ClientMessage::PreviewWorkspaceSpawn`]:
    /// resolve where a spawn would fork from, and whether it would land on an
    /// existing worktree, without creating anything. Replied with
    /// [`DaemonMessage::SpawnPreview`].
    PreviewSpawn {
        repo_id: String,
        branch_name: String,
        base_branch: Option<String>,
        use_worktree: bool,
    },
    /// Run `git fetch --prune` for a repo so subsequent previews compare
    /// against current remote refs. Advisory and non-blocking: the daemon
    /// answers [`DaemonMessage::RepoFetched`] when it settles, and a failure
    /// never blocks spawning.
    FetchRepo {
        repo_id: String,
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
    /// Set or clear the user-provided label override for a session.
    /// `label = Some(text)` pins a custom name; `label = None` (or an
    /// empty string after trim) restores the daemon-generated default
    /// (`<repo>:<branch>` for single, `<workspace>:<branch>` for
    /// workspace). Persisted on the orphan sidecar so the override
    /// survives reattach.
    RenameSession {
        session_id: String,
        label: Option<String>,
    },
    /// Set appearance defaults inherited by sessions launched from this repo.
    SetRepoAppearance {
        repo_id: String,
        appearance: AppearanceOverrides,
    },
    /// Set appearance defaults inherited by sessions launched from this
    /// workspace.
    SetWorkspaceAppearance {
        workspace_id: String,
        appearance: AppearanceOverrides,
    },
    /// Set session-level appearance overrides. `None` fields inherit from the
    /// owning repo/workspace, then the app default.
    SetSessionAppearance {
        session_id: String,
        appearance: AppearanceOverrides,
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
        /// Legacy field kept for wire compatibility. Stop only terminates the
        /// process and retains the stopped session record; use
        /// [`ClientMessage::DiscardSession`] to remove the session record or
        /// delete worktrees.
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
    /// Park a stopped session in the sidebar: keep the session record in the
    /// registry (marked `is_inactive = true`) and retain its worktrees on
    /// disk so the user can resume work later. If the session is still
    /// running, kills it first. No-op if the session does not exist.
    ParkSession {
        session_id: String,
    },
    /// Remove a stopped or inactive session from the registry. Optionally
    /// removes its per-member worktrees from disk. Does not kill a running
    /// session — use `StopSession` for that. Covers the same surface as
    /// `DiscardAbandoned` but applies to any terminal-state session.
    DiscardSession {
        session_id: String,
        cleanup: Vec<CleanupAction>,
    },
    /// List all non-main git worktrees for a registered repo. Returns a
    /// `Worktrees` reply keyed on `repo_id`. Each entry is marked with
    /// whether an active (non-stopped) session is currently using it.
    ListWorktrees {
        repo_id: String,
    },
    /// Resume every session currently in the abandoned bucket. The daemon
    /// iterates and re-spawns each one; per-session failures (missing
    /// spawn config, spawn error) are surfaced as `Error` messages but
    /// don't abort the bulk operation.
    ResumeAllAbandoned,
    /// List branches in a registered repo.
    ListBranches {
        repo_id: String,
    },
    /// Recent commits for a repo. `branch` defaults to current. `offset`
    /// (default 0) maps to `git log --skip`, so the source-control sidebar
    /// can request successive batches for "load more" pagination. When
    /// `worktree_path` is `Some`, `git log` runs at that worktree (so its
    /// HEAD position is honored) instead of the registered repo's main tree.
    ListCommits {
        repo_id: String,
        branch: Option<String>,
        limit: u32,
        #[serde(default)]
        offset: u32,
        /// Optional session worktree path. When omitted the daemon uses the
        /// registered repo path.
        #[serde(default)]
        worktree_path: Option<String>,
    },
    /// Full detail (subject, body, parents, file list) for one commit.
    GetCommit {
        repo_id: String,
        sha: String,
    },
    /// Unified diff for a single file. `against` is `None` for working-tree
    /// vs index, `Some("HEAD")` for index vs HEAD, or a sha for that commit.
    /// When `worktree_path` is `Some`, the diff is computed in that worktree
    /// so its on-disk file content (and HEAD) are honored.
    GetFileDiff {
        repo_id: String,
        path: String,
        against: Option<String>,
        #[serde(default)]
        worktree_path: Option<String>,
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
    /// can render VSCode-style STAGED + CHANGES sections. When
    /// `worktree_path` is `Some`, status is computed against that worktree
    /// instead of the registered repo's main tree.
    RepoStatus {
        repo_id: String,
        #[serde(default)]
        worktree_path: Option<String>,
    },
    /// Stage one or more paths in `repo_id` (`git add -- <path>`...). On
    /// success the daemon broadcasts a fresh [`DaemonMessage::RepoStatus`]
    /// to every connected client. Failure surfaces as
    /// [`DaemonMessage::GitWriteError`]. `worktree_path = Some(...)` targets
    /// a session worktree instead of the registered repo's main tree.
    StageFiles {
        repo_id: String,
        paths: Vec<String>,
        #[serde(default)]
        worktree_path: Option<String>,
    },
    /// Unstage one or more paths (`git restore --staged -- <path>`...).
    /// Same response shape as [`StageFiles`].
    UnstageFiles {
        repo_id: String,
        paths: Vec<String>,
        #[serde(default)]
        worktree_path: Option<String>,
    },
    /// Commit the current index. The daemon shells out to
    /// `git commit -m <message>` with no `--amend`, no `--no-verify`. On
    /// success it broadcasts the fresh status and replies with
    /// [`DaemonMessage::CommitOk`]; on failure it emits
    /// [`DaemonMessage::GitWriteError`] with the operation tag `"commit"`.
    CommitRepo {
        repo_id: String,
        message: String,
        #[serde(default)]
        worktree_path: Option<String>,
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
        #[serde(default)]
        worktree_path: Option<String>,
    },
    /// Push a new stash. Empty `message` is allowed — git falls back to its
    /// default "WIP on <branch>" subject. Always run with `-u` so untracked
    /// files come along. Daemon broadcasts a refreshed `RepoStatus` and a
    /// fresh [`DaemonMessage::Stashes`] snapshot for `repo_id` on success.
    StashPush {
        repo_id: String,
        message: String,
        #[serde(default)]
        worktree_path: Option<String>,
    },
    /// Request the current stash list. Daemon replies with
    /// [`DaemonMessage::Stashes`].
    ListStashes {
        repo_id: String,
        #[serde(default)]
        worktree_path: Option<String>,
    },
    /// `git stash pop <stash_id>` — apply and drop. Broadcasts fresh
    /// `RepoStatus` + `Stashes` on success.
    StashPop {
        repo_id: String,
        stash_id: String,
        #[serde(default)]
        worktree_path: Option<String>,
    },
    /// `git stash apply <stash_id>` — apply without dropping.
    StashApply {
        repo_id: String,
        stash_id: String,
        #[serde(default)]
        worktree_path: Option<String>,
    },
    /// `git stash drop <stash_id>` — drop without applying.
    StashDrop {
        repo_id: String,
        stash_id: String,
        #[serde(default)]
        worktree_path: Option<String>,
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
        #[serde(default)]
        worktree_path: Option<String>,
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
        #[serde(default)]
        worktree_path: Option<String>,
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
    /// Graceful daemon shutdown.
    ///
    /// `drain = true` (default, current behaviour): every live session is
    /// stopped cleanly via `stop_session` and its sidecar deleted. Used
    /// for "quit and stop everything".
    ///
    /// `drain = false`: sessions are NOT stopped; their sidecars are
    /// flipped to abandoned-on-next-startup state and the daemon exits
    /// without firing the worktree-remove cleanup. Today (pre-Phase-C)
    /// the children still die when the daemon's `ConPTY` master handle
    /// closes — but the next daemon start will surface them in the
    /// "Abandoned" bucket where the user can Resume. Once Phase C lands
    /// (per-session tracer) `drain = false` will actually preserve the
    /// live processes.
    Shutdown {
        #[serde(default = "default_true")]
        drain: bool,
    },
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
    /// Override the worktrees root path. `path = None` clears the user
    /// override and reverts to the env-or-default location (see the
    /// daemon's `resolve_worktrees_dir`). Takes effect immediately for
    /// *new* sessions; already-spawned sessions keep their existing
    /// worktree paths. The daemon validates the path is creatable +
    /// writable, persists it on `PersistedState`, then broadcasts
    /// [`DaemonMessage::WorktreesRootChanged`] to every connected client.
    SetWorktreesRoot {
        path: Option<String>,
    },
    /// Enable or disable opt-in LAN access (the `0.0.0.0` TLS listener) and
    /// set its port. The daemon persists this to `lan.json` (along with the
    /// stable auth token), binds or tears down the TLS listener immediately
    /// (no restart needed), then broadcasts [`DaemonMessage::LanStatus`] to
    /// every connected client. Loopback access is unaffected.
    ConfigureLan {
        enabled: bool,
        port: u16,
    },
    /// Begin a pairing window. The daemon generates a short numeric code and
    /// replies — to this connection only, so the code never reaches other
    /// clients — with [`DaemonMessage::PairingStarted`]. A discovering device
    /// submits the code to the daemon's `/pair` endpoint over the pinned-TLS
    /// LAN channel to obtain the auth token. Only meaningful on the host while
    /// LAN access is enabled.
    StartPairing,
    /// Cancel the active pairing window early. The daemon clears it and
    /// broadcasts [`DaemonMessage::PairingEnded`].
    CancelPairing,
    /// Scan the current worktrees root and report every `wt.<branch>/`
    /// group found under it, cross-referenced against the live and
    /// abandoned session registries. Daemon replies with
    /// [`DaemonMessage::WorktreesRootSnapshot`].
    InspectWorktreesRoot,
    /// Delete a worktree group on disk. `path` is the absolute path of
    /// the `wt.<branch>/` directory as reported by
    /// [`DaemonMessage::WorktreesRootSnapshot`]. The daemon refuses to
    /// delete a group whose status is `Active` (a session is using it
    /// right now); the client must stop the session first. For each
    /// member directory the daemon tries `git -C <repo> worktree remove
    /// --force <member>` first; if the originating repo is gone or the
    /// command fails, it falls back to `fs::remove_dir_all` and (where
    /// possible) prunes the originating repo's stale worktree refs.
    /// Daemon emits a fresh `WorktreesRootSnapshot` on success.
    DeleteWorktreeAt {
        path: String,
    },
    /// User chose "Kill processes & retry" from the
    /// [`DaemonMessage::WorktreeCleanupFailed`] modal. Daemon attempts a
    /// Restart Manager graceful shutdown of the listed pids first (so an
    /// editor with unsaved buffers can prompt to save), then hard-kills
    /// any survivors and re-runs the robust worktree cleanup against
    /// each [`RetryWorktreeTarget`]. On still-stuck members the daemon
    /// emits another `WorktreeCleanupFailed` so the user can iterate.
    RetryWorktreeCleanup {
        /// Echoes the session id from the original failure for UI
        /// correlation. The session itself is already gone from the
        /// registry by this point — the id is purely a key.
        session_id: String,
        kill_pids: Vec<u32>,
        targets: Vec<RetryWorktreeTarget>,
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
    /// Restore a previously closed tab snapshot at its prior index. Used by
    /// the desktop undo shelf; preserves pane ids, tab id, tab name, and tab
    /// content exactly as they existed before close. Fails if the tab id is
    /// already live.
    RestoreTab {
        tab: TabEntry,
        /// Desired insertion index. Values past the end append.
        index: usize,
    },
    /// Replace an existing live tab with a previously captured snapshot.
    /// Used by pane/layout undo when the tab itself remains open.
    RestoreTabSnapshot {
        tab: TabEntry,
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
    /// Reply to [`DaemonMessage::LayoutInitRequired`]: the client picked how to
    /// seed its brand-new per-client layout. The daemon creates the layout for
    /// the connection's `client_id` and then sends [`DaemonMessage::Tabs`].
    InitLayout {
        kind: InitLayoutKind,
    },
    /// Manual ordering of the sidebar's top-level containers as a single
    /// flat list mixing workspaces and repos. Anything not present in the
    /// list falls back to alphabetical at the end (defensive — handles new
    /// items registered after the order was last persisted).
    ReorderContainers {
        ordered: Vec<ContainerRef>,
    },
    /// Set the display order of sessions within a single sidebar container.
    /// `container_id` is the workspace id, repo id, or tab id whose session
    /// list the user reordered. Stale ids in `ordered_ids` (e.g. from a
    /// session that was stopped since the drag) are silently ignored on
    /// merge. Falls back to incoming order for any session not in the list.
    ReorderSessions {
        container_id: String,
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
    /// otherwise cross-tab). For non-replace edges, the source pane is removed
    /// from its current position (with the parent split collapsing if needed)
    /// and inserted at `edge` of the destination pane. For same-tab
    /// `replace`, the two pane session bindings are swapped so both grid
    /// positions remain intact.
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
    /// collected in the order `pane_ids` is given. `layout` controls how the
    /// new tab arranges them; `None` (older clients) keeps the historical
    /// horizontal tiling. `Some(_)` builds the grid via the same path as
    /// [`ClientMessage::RearrangeTab`], so the new tab can be a true grid.
    ExtractToNewTab {
        source_tab_id: String,
        pane_ids: Vec<String>,
        name: Option<String>,
        #[serde(default)]
        layout: Option<RearrangeLayout>,
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
        /// Client-assigned id for this preset-launch job. Older clients may
        /// omit it; the daemon normalizes an empty id to a generated one.
        #[serde(default)]
        job_id: String,
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
    /// Request cancellation for an in-flight preset launch. The daemon stops
    /// before the next session spawn; already-created sessions and tabs stay.
    CancelPresetLaunch {
        job_id: String,
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

/// One entry in the `Worktrees` reply: a non-main git worktree for a repo.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeInfo {
    /// Short branch name (e.g. `wt/brave-fox`, `feature/foo`).
    pub branch: String,
    /// Absolute path to the worktree directory on disk.
    pub path: String,
    /// True if a non-stopped, non-inactive session is currently using this
    /// worktree path. Such worktrees should be shown as unavailable in the
    /// spawn dialog's "use existing" picker.
    pub is_active: bool,
}

/// Status of one `wt.<branch>/` group as discovered by
/// [`ClientMessage::InspectWorktreesRoot`].
///
/// New variants must default-deserialize through `#[serde(other)] Unknown`
/// so a client speaking an older protocol still decodes the snapshot
/// (containing message keeps decoding, single entry's status renders as
/// "Unknown" in the UI).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RootWorktreeStatus {
    /// At least one live (non-stopped, non-abandoned) session is using a
    /// member of this group. Deletion is refused.
    Active,
    /// A session record references this group but is currently stopped
    /// or abandoned. Safe to delete after explicit user confirm — the
    /// session can be Resumed before deletion if the user changes their
    /// mind.
    Detached,
    /// No session in the registry references this group. Safe to delete
    /// without further consequences. This is the bucket that the bulk
    /// "Delete all stale" action targets.
    Stale,
    /// Forward-compat absorber for clients that decode a newer status
    /// kind we don't recognize yet. Treat as "Detached" for the purpose
    /// of bulk-delete safety.
    #[serde(other)]
    Unknown,
}

/// One member within a `wt.<branch>/` group: the on-disk worktree path
/// plus the originating repo's working-tree path (resolved from the
/// member's `.git` gitfile), used by the daemon to invoke
/// `git -C <repo> worktree remove`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RootWorktreeMember {
    pub worktree_path: String,
    /// Absolute path of the originating repo's working tree. `None` when
    /// the gitfile inside the worktree is unreadable, points at a
    /// non-existent repo, or has been corrupted. Deletion falls back to
    /// plain `fs::remove_dir_all` in that case.
    pub repo_path: Option<String>,
    /// Best-effort display name (leaf component of `repo_path` when
    /// known; otherwise leaf component of `worktree_path`).
    pub repo_name_hint: String,
}

/// One `wt.<branch>/` group under the worktrees root.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RootWorktreeEntry {
    /// Absolute path of the `wt.<branch>/` directory itself. This is
    /// the address `DeleteWorktreeAt` operates on.
    pub path: String,
    /// Parent directory name (the "anchor" — common-ancestor prefix of
    /// member repo parents, sanitized). Pure display field.
    pub anchor: String,
    /// Branch slug (`<wt.>` prefix already stripped).
    pub branch_slug: String,
    /// One entry per child member directory under `wt.<branch>/`.
    /// A single-repo session has one member; a workspace session has
    /// one per workspace member.
    pub members: Vec<RootWorktreeMember>,
    pub status: RootWorktreeStatus,
    /// Session id referencing this group, if any. For Active and
    /// Detached statuses this points at the owning session. None for
    /// Stale.
    pub session_id: Option<String>,
    /// Total bytes used by all member directories under this group.
    /// Best-effort: `None` if the walk fails (permission errors, etc.).
    pub size_bytes: Option<u64>,
    /// Most recent modified timestamp (unix seconds) across the group's
    /// member directories. `None` if no readable metadata.
    pub last_modified_unix: Option<i64>,
}

/// One process holding open handles inside a worktree directory that
/// `discard_session` failed to remove. Populated by the daemon's
/// Restart Manager probe on Windows; empty list on other platforms.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeLockingProcess {
    pub pid: u32,
    /// Friendly process name (typically the exe leaf, e.g. `node.exe`).
    pub name: String,
    /// Full command line joined with spaces. `None` when the process
    /// exited between the lock probe and the cmdline enrichment.
    #[serde(default)]
    pub cmdline: Option<String>,
}

/// One member worktree that survived every cleanup attempt during
/// `discard_session` (or its retry). Surfaced inside
/// [`DaemonMessage::WorktreeCleanupFailed`] so the frontend modal can
/// show the path, why it failed, and which processes are blocking it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeCleanupFailure {
    /// Absolute path of the member worktree dir that's still on disk.
    pub member_path: String,
    /// Absolute path of the originating repo. Sent so the retry handler
    /// can re-run `git worktree remove` without the frontend needing to
    /// look up which repo owns the worktree.
    pub repo_path: String,
    /// Concatenated error chain from the final cleanup attempt
    /// (e.g. `git worktree remove (retry): ...; fs::remove_dir_all: ...`).
    pub reason: String,
    /// Processes holding open handles inside `member_path`. Empty if
    /// the Restart Manager probe didn't return anything or the daemon
    /// is running on a non-Windows host.
    #[serde(default)]
    pub locking_processes: Vec<WorktreeLockingProcess>,
}

/// One cleanup target inside a [`ClientMessage::RetryWorktreeCleanup`]
/// request. The daemon validates `member_path` is under the configured
/// worktrees root before touching anything (defence against a
/// confused-deputy attack via a crafted message).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetryWorktreeTarget {
    pub member_path: String,
    pub repo_path: String,
}

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
    /// Acknowledgement that the daemon has finished its
    /// [`ClientMessage::Shutdown`] teardown (`shutdown_all` complete; the
    /// daemon will exit imminently). Clients listening for this can close
    /// their window without waiting for the WebSocket itself to close —
    /// the daemon-side WS Close is wedged by the time the handler returns
    /// because of how axum drops the upgraded socket, and on Windows the
    /// TCP FIN can take several seconds to surface in `WebView2`.
    /// Additive on top of protocol v15 (no version bump): older clients
    /// route this through `InboundDaemonMessage::Unknown` and ignore it.
    ShutdownAck {},
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
    VscodeWorkspacesScanned {
        path: String,
        suggestions: Vec<VscodeWorkspaceSuggestion>,
    },
    Branches {
        repo_id: String,
        branches: Vec<String>,
        current: Option<String>,
        /// Remote-tracking branches as short names (`origin/main`), so the
        /// base-branch picker can offer a ref that is actually current
        /// rather than only long-stale local ones.
        #[serde(default)]
        remote_branches: Vec<String>,
    },
    /// Reply to [`ClientMessage::ListWorktrees`]. Lists all non-main
    /// worktrees for the given repo, each annotated with whether an active
    /// session is currently using it.
    Worktrees {
        repo_id: String,
        worktrees: Vec<WorktreeInfo>,
    },
    /// Broadcast whenever the worktrees root changes — fired both in
    /// response to [`ClientMessage::SetWorktreesRoot`] and at first
    /// `Welcome` so a freshly-connected client knows the current value
    /// without polling. `is_override` is true iff the active path came
    /// from the user setting (vs the env/default fallback).
    WorktreesRootChanged {
        root: String,
        is_override: bool,
    },
    /// Current opt-in LAN access status. Broadcast in response to
    /// [`ClientMessage::ConfigureLan`] and sent once at initial state so a
    /// freshly-connected client can render the Settings UI without polling.
    /// `fingerprint` is the SHA-256 (lowercase hex) of the self-signed TLS
    /// leaf cert that remote clients pin; `None` until LAN has been enabled
    /// at least once (no cert yet). `addresses` is the daemon host's detected
    /// non-loopback IPv4 addresses, for assembling the remote connection code.
    LanStatus {
        enabled: bool,
        port: u16,
        fingerprint: Option<String>,
        addresses: Vec<String>,
    },
    /// Reply to [`ClientMessage::StartPairing`], sent only to the requesting
    /// connection (the code must never reach other clients). `code` is the
    /// short numeric pairing code for the user to read out; `ttl_secs` is how
    /// long it stays valid, so the UI can show a countdown.
    PairingStarted {
        code: String,
        ttl_secs: u32,
    },
    /// Broadcast when the active pairing window ends — a device paired, the
    /// code expired or was cancelled, or too many wrong codes were tried — so
    /// the host UI can clear the displayed code.
    PairingEnded {
        reason: PairingEndReason,
    },
    /// Reply to [`ClientMessage::InspectWorktreesRoot`]. Carries every
    /// `wt.<branch>/` group discovered under the current root, plus a
    /// snapshot of the root path itself so the client can reconcile
    /// against any concurrent root change.
    WorktreesRootSnapshot {
        root: String,
        is_override: bool,
        entries: Vec<RootWorktreeEntry>,
    },
    /// Emitted by `discard_session` (and by the retry handler) when one
    /// or more member worktrees survived every cleanup attempt — git
    /// remove failed, the retry failed, and `fs::remove_dir_all` also
    /// failed (the directory is wedged by open handles, almost always
    /// from grandchild processes the PTY kill didn't reach: dev servers,
    /// build subprocesses, the user's editor, etc.). The frontend shows
    /// the list of locking processes (Restart Manager API on Windows;
    /// empty list elsewhere) and offers Kill-and-retry / Open-in-Explorer
    /// / Ignore. See [`ClientMessage::RetryWorktreeCleanup`].
    WorktreeCleanupFailed {
        session_id: String,
        /// Human-readable label for the discarded session (e.g. the
        /// branch name) so the modal can identify which session this is
        /// about without the frontend having to look it up.
        session_label: String,
        failures: Vec<WorktreeCleanupFailure>,
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
    /// An in-place spawn (`SpawnTarget::Single { use_worktree: false }`) would
    /// switch a dirty working tree to a different branch. The daemon declines
    /// rather than touch uncommitted work, and asks the client how to proceed;
    /// the client confirms by resending the same spawn with a `checkout_strategy`
    /// of `Carry` or `Stash`.
    CheckoutConfirmRequired {
        repo_id: String,
        branch: String,
        /// Count of uncommitted (index + worktree) changes that would be
        /// affected, for the confirmation prompt.
        dirty_count: u32,
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
    /// Reply to [`ClientMessage::PreviewSpawn`].
    SpawnPreview {
        repo_id: String,
        branch_name: String,
        preview: MemberSpawnPreview,
    },
    /// Reply to [`ClientMessage::FetchRepo`]. `error` carries git's message
    /// when the fetch failed; the client reports it as a freshness caveat
    /// rather than an error, since a stale comparison is still usable.
    RepoFetched {
        repo_id: String,
        error: Option<String>,
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
        /// Echoed from the request so the sidebar can route the response
        /// to the correct workspace section.
        #[serde(default)]
        worktree_path: Option<String>,
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
        #[serde(default)]
        worktree_path: Option<String>,
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
        /// `None` = registered repo's main tree; `Some(path)` = the session
        /// worktree the status was computed against. The sidebar uses this
        /// to route events to the correct workspace section.
        #[serde(default)]
        worktree_path: Option<String>,
    },
    /// Successful response to [`ClientMessage::CommitRepo`].
    CommitOk {
        repo_id: String,
        sha: String,
        short_sha: String,
        #[serde(default)]
        worktree_path: Option<String>,
    },
    /// Failure response to a stage/unstage/commit/discard/stash request.
    /// `operation` is one of `"stage"`, `"unstage"`, `"commit"`,
    /// `"discard"`, `"stash_push"`, `"stash_pop"`, `"stash_apply"`,
    /// `"stash_drop"` so the UI can attribute the error.
    GitWriteError {
        repo_id: String,
        operation: String,
        error: String,
        #[serde(default)]
        worktree_path: Option<String>,
    },
    /// Response to [`ClientMessage::ListStashes`] and a broadcast after any
    /// successful stash push/pop/drop. Carries the full current stash list
    /// in `stash@{0}` → `stash@{N}` order (newest first).
    Stashes {
        repo_id: String,
        stashes: Vec<GitStash>,
        #[serde(default)]
        worktree_path: Option<String>,
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
        #[serde(default)]
        worktree_path: Option<String>,
    },
    /// Failure response to [`ClientMessage::GetFileSnapshot`].
    FileSnapshotError {
        id: String,
        repo_id: String,
        path: String,
        against: Option<String>,
        error: String,
        #[serde(default)]
        worktree_path: Option<String>,
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
    /// Sent instead of [`DaemonMessage::Tabs`] when a client connects with a
    /// `client_id` the daemon has never seen. The client shows a first-connect
    /// chooser and replies with [`ClientMessage::InitLayout`]; the daemon then
    /// creates the layout and sends `Tabs`. Clients that don't recognize this
    /// message (older builds) just won't render tabs until they send any tab
    /// mutation, which lazily creates an empty layout.
    LayoutInitRequired {
        /// Whether a pre-per-client global layout is available to adopt.
        has_legacy: bool,
        /// Count of currently-running sessions (for the "open all" option).
        active_session_count: u32,
        /// Other clients' non-empty layouts that can be cloned.
        clonable: Vec<ClonableLayout>,
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
    /// Manual sidebar-container order. Broadcast on initial connect (so
    /// new clients render in the persisted order) and after every
    /// successful `ReorderContainers`. An empty vec means "no manual
    /// order is set; client should fall back to alphabetical".
    ContainersReordered {
        ordered: Vec<ContainerRef>,
    },
    /// Broadcast after a successful `ReorderSessions` and replayed on
    /// initial connect for every container that has a stored order.
    SessionsReordered {
        container_id: String,
        ordered_ids: Vec<String>,
    },
    /// Response to [`ClientMessage::ParseVscodeWorkspace`].
    VscodeWorkspaceParsed {
        suggestion: VscodeWorkspaceSuggestion,
    },
    /// Response to [`ClientMessage::ListPresets`].
    Presets {
        target: PresetTarget,
        entries: Vec<PresetEntry>,
    },
    /// Streamed updates while a `LaunchPreset` batch runs. UI uses
    /// `current_tab_id` to auto-switch into the freshly populated tab.
    PresetLaunchProgress {
        #[serde(default)]
        job_id: String,
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
        #[serde(default)]
        job_id: String,
        preset_id: String,
        error: String,
        #[serde(default)]
        partial_session_ids: Vec<String>,
        #[serde(default)]
        partial_tab_ids: Vec<String>,
    },
    /// Streamed snapshot of the daemon-visible preset launch job. Future UI
    /// progress and cancellation surfaces key off this job id.
    PresetLaunchJobUpdated {
        job: PresetLaunchJobSnapshot,
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
    /// A user-requested action was refused or failed in a way that warrants a
    /// blocking modal rather than a transient toast — e.g. a worktree spawn
    /// that collided with an in-use worktree, or a "remove worktree" discard
    /// blocked because another live session still uses it. `title` is the
    /// modal heading, `detail` the explanation, and `hint` an optional
    /// actionable next step.
    ActionFailed {
        title: String,
        detail: String,
        #[serde(default)]
        hint: Option<String>,
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
    /// Base branch *name* the daemon settled on — the caller's explicit
    /// value, else the repo's default. `None` when the target branch
    /// already exists and nothing will be created.
    pub effective_base: Option<String>,
    pub worktree_path: String,
    /// The ref actually handed to `git worktree add`. Differs from
    /// `effective_base` when the daemon upgraded a local branch name to its
    /// remote-tracking counterpart (`main` → `origin/main`).
    #[serde(default)]
    pub resolved_base_ref: Option<String>,
    /// Remote-tracking ref `effective_base` was compared against, when one
    /// exists.
    #[serde(default)]
    pub base_remote_ref: Option<String>,
    /// How many commits the *local* `effective_base` trails
    /// `base_remote_ref`. Non-zero means the local branch is stale; the UI
    /// surfaces it so forking from a long-untouched local ref stops being
    /// a silent surprise.
    #[serde(default)]
    pub base_behind_remote: Option<u32>,
    /// Whether a worktree directory already sits at `worktree_path`. When
    /// true the daemon binds to it as-is unless the client asks for
    /// [`WorktreeReusePolicy::RecreateFromBase`], and `effective_base` is
    /// therefore ignored.
    #[serde(default)]
    pub worktree_exists: bool,
    /// Abbreviated HEAD of the existing worktree, when there is one.
    #[serde(default)]
    pub existing_worktree_head: Option<String>,
    /// Whether the existing worktree has uncommitted changes. Recreating it
    /// would discard them, so the client must warn before offering that.
    #[serde(default)]
    pub existing_worktree_dirty: bool,
    /// How many commits the existing worktree's HEAD trails the resolved
    /// base. This is the number that catches "this branch forked from a
    /// main that has since moved N commits".
    #[serde(default)]
    pub existing_worktree_behind_base: Option<u32>,
}

/// What to do when a worktree directory already exists at the path a spawn
/// wants to use. Grows a catch-all so an older daemon decoding a newer
/// choice falls back to the non-destructive path.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeReusePolicy {
    /// Bind to the existing worktree untouched. The base branch is ignored —
    /// the worktree keeps whatever fork point it was created with.
    #[default]
    Reuse,
    /// Remove the existing worktree and re-add it from the resolved base.
    /// Destructive: uncommitted work in that worktree is lost.
    RecreateFromBase,
    #[serde(other)]
    Unknown,
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
            (PaneDropEdge::OuterLeft, "outer_left"),
            (PaneDropEdge::OuterRight, "outer_right"),
            (PaneDropEdge::OuterTop, "outer_top"),
            (PaneDropEdge::OuterBottom, "outer_bottom"),
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
                        extract_pattern: Some(r"Saved \d+ lines to (.+?)\s*$".to_string()),
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
            agent_options: AgentOptions::Claude {
                permission_mode: None,
            },
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
                verify_mode_marker: None,
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
    fn toggle_variable_kind_tagged() {
        let kind = PresetVariableKind::Toggle {
            value: "${LOCALAPPDATA}/yaat/yaat-client.log".to_string(),
        };
        let json = serde_json::to_string(&kind).expect("serialize");
        assert!(json.contains(r#""kind":"toggle""#));
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
        let ClientMessage::ResolvePresetScripts { id, preset_id, .. } = decoded else {
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
    fn launch_preset_message_carries_job_id() {
        let msg = ClientMessage::LaunchPreset {
            job_id: "preset-job-1".to_string(),
            target: PresetTarget::Repo {
                repo_id: "repo-1".to_string(),
            },
            preset_id: "bug-triage".to_string(),
            source: LaunchPresetSource::Inline {
                prompts: vec!["one".to_string()],
            },
            variable_values: Vec::new(),
            use_worktree_override: None,
            max_panes_per_tab_override: None,
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(json.contains(r#""job_id":"preset-job-1""#));
        let decoded: ClientMessage = serde_json::from_str(&json).expect("deserialize");
        let ClientMessage::LaunchPreset { job_id, .. } = decoded else {
            panic!("wrong variant");
        };
        assert_eq!(job_id, "preset-job-1");
    }

    #[test]
    fn github_issues_prompt_source_roundtrips() {
        let src = PresetPromptSource::GithubIssues;
        let json = serde_json::to_string(&src).expect("serialize");
        assert_eq!(json, r#"{"kind":"github_issues"}"#);
        let decoded: PresetPromptSource = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, src);
    }

    #[test]
    fn github_issues_launch_source_roundtrips() {
        let src = LaunchPresetSource::GithubIssues {
            spec: "123-127, 131".to_string(),
        };
        let json = serde_json::to_string(&src).expect("serialize");
        let decoded: LaunchPresetSource = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, src);
    }

    #[test]
    fn launch_preset_message_defaults_legacy_job_id() {
        let json = r#"{
            "type":"launch_preset",
            "target":{"kind":"repo","repo_id":"repo-1"},
            "preset_id":"bug-triage",
            "source":{"kind":"inline","prompts":["one"]},
            "variable_values":[],
            "use_worktree_override":null,
            "max_panes_per_tab_override":null
        }"#;
        let decoded: ClientMessage = serde_json::from_str(json).expect("deserialize");
        let ClientMessage::LaunchPreset { job_id, .. } = decoded else {
            panic!("wrong variant");
        };
        assert_eq!(job_id, "");
    }

    #[test]
    fn cancel_preset_launch_message_tagged() {
        let msg = ClientMessage::CancelPresetLaunch {
            job_id: "preset-job-1".to_string(),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(json.contains(r#""type":"cancel_preset_launch""#));
        let decoded: ClientMessage = serde_json::from_str(&json).expect("deserialize");
        let ClientMessage::CancelPresetLaunch { job_id } = decoded else {
            panic!("wrong variant");
        };
        assert_eq!(job_id, "preset-job-1");
    }

    #[test]
    fn preset_launch_job_updated_message_tagged() {
        let msg = DaemonMessage::PresetLaunchJobUpdated {
            job: PresetLaunchJobSnapshot {
                job_id: "preset-job-1".to_string(),
                preset_id: "bug-triage".to_string(),
                target: PresetTarget::Repo {
                    repo_id: "repo-1".to_string(),
                },
                total: 2,
                launched: 1,
                created_session_ids: vec!["session-1".to_string()],
                created_tab_ids: vec!["tab-1".to_string()],
                status: PresetLaunchJobStatus::Spawning,
                error: None,
                current_tab_id: Some("tab-1".to_string()),
            },
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(json.contains(r#""type":"preset_launch_job_updated""#));
        assert!(json.contains(r#""status":"spawning""#));
        let decoded: DaemonMessage = serde_json::from_str(&json).expect("deserialize");
        let DaemonMessage::PresetLaunchJobUpdated { job } = decoded else {
            panic!("wrong variant");
        };
        assert_eq!(job.job_id, "preset-job-1");
        assert_eq!(job.status, PresetLaunchJobStatus::Spawning);
        assert_eq!(job.created_session_ids, vec!["session-1".to_string()]);
    }

    #[test]
    fn preset_launch_running_scripts_status_tagged() {
        let status = PresetLaunchJobStatus::RunningScripts;
        let json = serde_json::to_string(&status).expect("serialize");
        assert_eq!(json, r#""running_scripts""#);
        let decoded: PresetLaunchJobStatus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, PresetLaunchJobStatus::RunningScripts);
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
        assert_eq!(
            preset.agent_options,
            AgentOptions::Claude {
                permission_mode: None,
            }
        );
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
                checkout_strategy: None,
                worktree_reuse: WorktreeReusePolicy::Reuse,
            },
            mode: SessionMode::Interactive,
            initial_prompt: None,
            dangerously_skip_permissions: true,
            agent_options: AgentOptions::Claude {
                permission_mode: None,
            },
            model: None,
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
                verify_mode_marker: None,
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
                worktree_reuse: WorktreeReusePolicy::Reuse,
            },
            mode: SessionMode::Interactive,
            initial_prompt: Some("refactor authentication".to_string()),
            dangerously_skip_permissions: false,
            agent_options: AgentOptions::Codex {
                sandbox: Some(CodexSandbox::WorkspaceWrite),
            },
            model: Some("gpt-5.1-codex".to_string()),
            extra_env: vec![],
            prompt_injector: None,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let decoded: SpawnRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, decoded);
        assert_eq!(req.agent(), Agent::Codex);
        assert!(json.contains(r#""kind":"codex""#));
        assert!(json.contains(r#""sandbox":"workspace-write""#));
    }

    #[test]
    fn spawn_request_round_trip_cursor() {
        let req = SpawnRequest {
            label: Some("cursor-test".to_string()),
            target: SpawnTarget::Single {
                repo_id: "r1".to_string(),
                branch_name: "feat/y".to_string(),
                base_branch: None,
                use_worktree: true,
                checkout_strategy: None,
                worktree_reuse: WorktreeReusePolicy::Reuse,
            },
            mode: SessionMode::Interactive,
            initial_prompt: Some("investigate".to_string()),
            dangerously_skip_permissions: false,
            agent_options: AgentOptions::Cursor {
                plan_mode: true,
                sandbox: Some(CursorSandbox::Enabled),
            },
            model: Some("sonnet-4".to_string()),
            extra_env: vec![],
            prompt_injector: None,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let decoded: SpawnRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, decoded);
        assert_eq!(req.agent(), Agent::Cursor);
        assert!(json.contains(r#""kind":"cursor""#));
        assert!(json.contains(r#""plan_mode":true"#));
        assert!(json.contains(r#""sandbox":"enabled""#));
    }

    #[test]
    fn spawn_request_round_trip_standalone_shell() {
        let req = SpawnRequest {
            label: None,
            target: SpawnTarget::Standalone {
                cwd: Some("X:/scratch".to_string()),
            },
            mode: SessionMode::PlainShell,
            initial_prompt: None,
            dangerously_skip_permissions: false,
            agent_options: AgentOptions::Claude {
                permission_mode: None,
            },
            model: None,
            extra_env: vec![],
            prompt_injector: None,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let decoded: SpawnRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, decoded);
        assert!(json.contains(r#""kind":"standalone""#));
        assert!(json.contains(r#""mode":"plain_shell""#));
    }

    #[test]
    fn spawn_config_decodes_legacy_v18_shape() {
        // Pre-v19 `state.json` carried flat `agent` + `permission_mode` +
        // `codex_sandbox` instead of `agent_options`. The custom Deserialize
        // must convert old entries transparently so existing state files
        // load on the first run of a v19 daemon.
        let legacy = r#"{
            "target": {
                "kind": "single",
                "repo_id": "r1",
                "branch_name": "bug/1",
                "base_branch": null,
                "use_worktree": true
            },
            "mode": "interactive",
            "dangerously_skip_permissions": false,
            "agent": "codex",
            "model": "gpt-5",
            "permission_mode": null,
            "codex_sandbox": "workspace-write",
            "extra_env": []
        }"#;
        let cfg: SpawnConfig = serde_json::from_str(legacy).expect("decode legacy");
        assert_eq!(cfg.agent_options.agent(), Agent::Codex);
        assert_eq!(
            cfg.agent_options,
            AgentOptions::Codex {
                sandbox: Some(CodexSandbox::WorkspaceWrite),
            }
        );

        // Re-encoding emits the new shape; decoding round-trips.
        let json = serde_json::to_string(&cfg).expect("serialize new shape");
        assert!(json.contains(r#""agent_options""#));
        let decoded: SpawnConfig = serde_json::from_str(&json).expect("decode new shape");
        assert_eq!(cfg, decoded);
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
        assert!(repo.last_spawn_config.is_none());
    }

    #[test]
    fn workspace_entry_decodes_without_last_spawn_config() {
        // Existing state.json workspace entries lack `last_spawn_config`;
        // serde default must produce `None` so legacy state files still load.
        let legacy = r#"{
            "id": "ws1",
            "name": "fullstack",
            "member_repo_ids": ["r1","r2"]
        }"#;
        let ws: WorkspaceEntry = serde_json::from_str(legacy).expect("parse legacy workspace");
        assert!(ws.last_spawn_config.is_none());
        assert!(ws.linked_vscode_workspace.is_none());
        assert!(ws.default_use_worktree);
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
                ..
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
        assert_eq!(
            raw.get("new_field").and_then(serde_json::Value::as_i64),
            Some(42)
        );
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
    fn rearrange_layout_unknown_variant_decodes() {
        // A v(N+1) daemon sends a kind this client doesn't know. Older
        // clients route it to Unknown instead of failing the whole
        // containing message.
        let future = r#"{"kind":"future_only_layout"}"#;
        let parsed: RearrangeLayout = serde_json::from_str(future).expect("decode");
        assert!(matches!(parsed, RearrangeLayout::Unknown));
    }

    #[test]
    fn tab_layout_unknown_variant_decodes() {
        let future = r#""future_only_layout""#;
        let parsed: TabLayout = serde_json::from_str(future).expect("decode");
        assert!(matches!(parsed, TabLayout::Unknown));
    }

    #[test]
    fn injector_step_unknown_variant_decodes() {
        let future = r#"{"kind":"future_step"}"#;
        let parsed: InjectorStep = serde_json::from_str(future).expect("decode");
        assert!(matches!(parsed, InjectorStep::Unknown));
    }

    #[test]
    fn preset_variable_kind_unknown_variant_decodes() {
        let future = r#"{"kind":"future_var"}"#;
        let parsed: PresetVariableKind = serde_json::from_str(future).expect("decode");
        assert!(matches!(parsed, PresetVariableKind::Unknown));
    }

    #[test]
    fn known_rearrange_variant_still_decodes() {
        // Regression: make sure adding Unknown didn't break the existing
        // happy path.
        let json = r#"{"kind":"grid","cols":3}"#;
        let parsed: RearrangeLayout = serde_json::from_str(json).expect("decode");
        assert!(matches!(parsed, RearrangeLayout::Grid { cols: 3 }));
    }

    #[test]
    fn preset_entry_decodes_without_agent() {
        // User-edited `.rustling-tulip/presets.json` files may lack any agent
        // information at all (pre-v17). With no `agent_options`, no flat
        // `agent`, and no `permission_mode`, the legacy fallback chooses the
        // default Claude variant with no permission mode.
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
        assert_eq!(
            preset.agent_options,
            AgentOptions::Claude {
                permission_mode: None,
            }
        );
    }

    #[test]
    fn preset_entry_decodes_legacy_v18_codex_shape() {
        // v17–v18 preset files carried flat `agent: "codex"` + `codex_sandbox`.
        // The custom Deserialize must map them onto `agent_options` so old
        // presets keep working on a v19 daemon.
        let legacy = r#"{
            "id": "yolo",
            "name": "Yolo",
            "prompt_sources": [{"kind":"inline"}],
            "prompt_template": "{prompt}",
            "branch_template": "tmp/{index}",
            "tab_grouping": {"kind":"none"},
            "injector": {"startup_delay_ms": 0, "pre_input": [], "post_input": []},
            "agent": "codex",
            "codex_sandbox": "danger-full-access"
        }"#;
        let preset: PresetEntry = serde_json::from_str(legacy).expect("parse legacy preset");
        assert_eq!(
            preset.agent_options,
            AgentOptions::Codex {
                sandbox: Some(CodexSandbox::DangerFullAccess),
            }
        );

        // Re-serialization always emits the new shape; the legacy fields
        // never appear.
        let json = serde_json::to_string(&preset).expect("serialize");
        assert!(json.contains(r#""agent_options""#));
        assert!(!json.contains(r#""codex_sandbox""#));
    }
}
