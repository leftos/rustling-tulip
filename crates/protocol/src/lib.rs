//! Wire protocol between the rustling-tulip daemon and its clients.
//!
//! All messages are JSON-encoded. Both directions are tagged enums (`type` field)
//! so that adding a new variant is backward-compatible: older peers either match a
//! known variant or surface an `Error` for the unknown one.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 3;

fn default_true() -> bool {
    true
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
    /// For `kind == Workspace`, the id of the workspace this session belongs
    /// to. `None` for single-repo sessions. Clients use this to group sessions
    /// under their workspace node in the sidebar tree.
    #[serde(default)]
    pub workspace_id: Option<String>,
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
    pub dangerously_skip_permissions: bool,
    /// Optional model override. When `None`, the CLI's default applies.
    /// Sent as `--model <id>` to the `claude` CLI.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional permission mode. Ignored (and `--permission-mode` omitted)
    /// when `dangerously_skip_permissions` is true.
    #[serde(default)]
    pub permission_mode: Option<PermissionMode>,
    /// Extra environment variables merged on top of the daemon's keep-list.
    /// Later entries override the keep-list on key collision so users can
    /// override values like `ANTHROPIC_API_KEY`.
    #[serde(default)]
    pub extra_env: Vec<(String, String)>,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TabEntry {
    pub id: String,
    pub name: String,
    pub grid: GridNode,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Client -> Daemon
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Always the first message after WS upgrade. Fails the connection if the
    /// token does not match.
    Hello {
        protocol_version: u32,
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
    /// Request the diff status (changed file list) for a session's worktrees.
    SessionDiff {
        session_id: String,
    },
    /// List branches in a registered repo.
    ListBranches {
        repo_id: String,
    },
    /// Recent commits for a repo. `branch` defaults to current.
    ListCommits {
        repo_id: String,
        branch: Option<String>,
        limit: u32,
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
    /// non-session-scoped repo view.
    RepoStatus {
        repo_id: String,
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
}

// ---------------------------------------------------------------------------
// Daemon -> Client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonMessage {
    Welcome {
        protocol_version: u32,
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
    PtyOutput {
        session_id: String,
        data_b64: String,
    },
    Attention {
        session_id: String,
        reason: AttentionReason,
    },
    SessionDiff {
        session_id: String,
        per_member: Vec<MemberDiff>,
    },
    WorkspaceSpawnPreview {
        workspace_id: String,
        branch_name: String,
        per_member: Vec<MemberSpawnPreview>,
    },
    Commits {
        repo_id: String,
        commits: Vec<GitCommit>,
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
        changes: Vec<GitFileChange>,
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
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemberDiff {
    pub repo_id: String,
    pub repo_name: String,
    pub changed_files: Vec<String>,
    pub clean: bool,
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
}
