// Mirrors the Rust protocol crate. Keep in sync with crates/protocol/src/lib.rs.

import protocolVersion from "../../../protocol-version.json";

export const PROTOCOL_VERSION: number = protocolVersion.version;
export const SUPPORTED_PROTOCOL_VERSIONS: readonly number[] = protocolVersion.supported;

export type Agent = "claude" | "codex";

export type CodexSandbox = "read-only" | "workspace-write" | "danger-full-access";

export interface AppearanceOverrides {
  accent_color: string | null;
  terminal_background_color: string | null;
  terminal_frame_color: string | null;
  terminal_font_family: string | null;
  terminal_font_size: number | null;
  terminal_font_bold: boolean | null;
}

export interface RepoEntry {
  id: string;
  name: string;
  path: string;
  default_branch: string | null;
  default_use_worktree: boolean;
  appearance: AppearanceOverrides;
  // Last agent spawned against this repo. Drives the spawn-dialog default.
  // null for repos that have never been launched.
  last_agent: Agent | null;
  // Full spawn config captured from the last successful single-repo spawn.
  // Drives "Launch last again" (double-click + context-menu submenu). null
  // until the user has launched at least once.
  last_spawn_config: SpawnConfig | null;
}

export interface WorkspaceEntry {
  id: string;
  name: string;
  member_repo_ids: string[];
  linked_vscode_workspace: string | null;
  default_use_worktree: boolean;
  appearance: AppearanceOverrides;
  // Full spawn config captured from the last successful workspace spawn.
  // Drives "Launch last again" on the workspace row. null until the user
  // has launched the workspace at least once.
  last_spawn_config: SpawnConfig | null;
}

/// Tagged reference to either a repo or a workspace by id. Mirrors the
/// Rust `protocol::ContainerRef` enum (serde tagged: `kind` + `id`).
/// Used by the sidebar's manual flat-list container order.
export type ContainerRef =
  | { kind: "repo"; id: string }
  | { kind: "workspace"; id: string };

export type SessionKind = "single" | "workspace" | "standalone";
export type SessionStatus =
  | "spawning"
  | "idle"
  | "working"
  | "awaiting_input"
  | "stopped"
  | "error";
export type SessionMode = "interactive" | "headless" | "plain_shell";

export interface SessionMember {
  repo_id: string;
  repo_name: string;
  branch: string;
  worktree_path: string;
}

export interface SessionMetrics {
  input_tokens: number;
  output_tokens: number;
  cost_usd: number;
  last_activity_at: string | null;
}

export interface SessionSnapshot {
  id: string;
  label: string;
  // Explicit label set through the app's session rename command. null/absent
  // means the UI may prefer a meaningful terminal_title for display.
  user_label?: string | null;
  kind: SessionKind;
  members: SessionMember[];
  status: SessionStatus;
  mode: SessionMode;
  started_at: string;
  exit_code: number | null;
  metrics: SessionMetrics;
  recent_actions: string[];
  is_orphan: boolean;
  // True when this session was abandoned mid-run: the previous daemon
  // crashed (or was killed) and the underlying `claude` process is gone.
  // Drives the "Resume" affordance in the sidebar.
  is_abandoned: boolean;
  // User's initial prompt at spawn, surfaced so the abandoned-bucket UI
  // can show "what this session was trying to do". null when no prompt
  // was supplied or for sidecars written before Phase B.1.
  last_prompt: string | null;
  // For workspace-kind sessions: the workspace this session belongs to.
  // null for single-repo sessions.
  workspace_id: string | null;
  // Which CLI is driving this session. Drives the agent badge in the UI.
  agent: Agent;
  // Last OSC 0/1/2 window title emitted by the agent/shell. Distinct from
  // `label` so explicit app renames can still win while dynamic terminal
  // titles are available for display.
  terminal_title: string | null;
  // Short program label driving this session — "claude" / "codex" for
  // agent sessions, "pwsh" / "powershell" / "cmd" / "bash" / "sh" for
  // plain_shell sessions. Used by the UI to render a muted runtime chip
  // next to the session title. null when reattached from a daemon
  // sidecar written before this field existed.
  program_name: string | null;
  // Latest cwd reported by the session. Plain shells initialize this from
  // the spawn folder and update it from OSC 7 prompt metadata.
  current_cwd: string | null;
  // Session-level appearance overrides. null fields inherit from the
  // owning repo/workspace, then the app default.
  appearance: AppearanceOverrides;
  // True when the session was launched with approvals/permission prompts
  // bypassed. Drives visible trusted-launch badges after spawn.
  elevated_authority: boolean;
  // True when the session was spawned with use_worktree=true (per-session
  // worktree under <repo>.wt/<branch>). Drives the close-context-menu's
  // "delete worktree" choice. false for orphans whose stored spawn config
  // pre-dates this field — the worktree may still exist on disk.
  has_per_session_worktree: boolean;
  // True when the session is parked: process stopped, worktree kept on disk,
  // session stays in the sidebar so the user can Resume it later.
  is_inactive: boolean;
  // Absolute on-disk paths of the per-session worktrees; empty when
  // has_per_session_worktree is false. Used by the spawn dialog's "use
  // existing worktree" picker to mark which worktrees are in active use.
  worktree_paths: string[];
}

export type SpawnTarget =
  | {
      kind: "single";
      repo_id: string;
      branch_name: string;
      base_branch: string | null;
      use_worktree: boolean;
    }
  | {
      kind: "workspace";
      workspace_id: string;
      branch_name: string;
      base_branch: string | null;
      use_worktree: boolean;
    }
  | {
      kind: "standalone";
      cwd: string | null;
    };

export type PermissionMode =
  | "default"
  | "accept_edits"
  | "bypass_permissions"
  | "plan";

export interface SpawnRequest {
  label: string | null;
  target: SpawnTarget;
  mode: SessionMode;
  initial_prompt: string | null;
  // For agent === "claude": claude's --dangerously-skip-permissions.
  // For agent === "codex": codex's --yolo (--dangerously-bypass-approvals-and-sandbox).
  dangerously_skip_permissions: boolean;
  // Which CLI to spawn.
  agent: Agent;
  model: string | null;
  // Claude-only. Ignored when agent === "codex".
  permission_mode: PermissionMode | null;
  // Codex-only. Ignored when agent === "claude" or when dangerously_skip_permissions is true.
  codex_sandbox: CodexSandbox | null;
  extra_env: Array<[string, string]>;
  prompt_injector: PromptInjector | null;
}

// Subset of SpawnRequest persisted on each session record + orphan sidecar
// so duplicates can be reconstructed without re-prompting. Excludes label
// (auto-generated for clones), initial_prompt (one-shot kickoff), and
// prompt_injector (also one-shot).
export interface SpawnConfig {
  target: SpawnTarget;
  mode: SessionMode;
  dangerously_skip_permissions: boolean;
  agent: Agent;
  model: string | null;
  permission_mode: PermissionMode | null;
  codex_sandbox: CodexSandbox | null;
  extra_env: Array<[string, string]>;
}

// ------- Prompt injection & presets -------

export type InjectorStep =
  | { kind: "delay"; ms: number }
  | { kind: "write"; data_b64: string }
  | { kind: "text"; content: string; newline: boolean }
  // Forward-compat: a daemon newer than this client may declare step kinds
  // we don't know. Injector runner skips them with a warn.
  | { kind: "unknown" };

export interface PromptInjector {
  steps: InjectorStep[];
  // Optional substring (case-insensitive) the daemon expects to see in the
  // TUI after the pre-input keystrokes. When set and not seen, the daemon
  // re-sends the pre-input portion. Copied from
  // InjectorTemplate.verify_mode_marker by the preset launcher.
  verify_mode_marker?: string | null;
}

export interface InjectorTemplate {
  startup_delay_ms: number;
  pre_input: InjectorStep[];
  post_input: InjectorStep[];
  // Optional substring the daemon expects to see in the rendered TUI after
  // pre_input runs (case-insensitive). When set and the marker doesn't show
  // up, the daemon re-sends pre_input a few times before giving up. Used to
  // self-heal dropped Shift+Tab keystrokes when entering Plan Mode.
  verify_mode_marker?: string | null;
}

export type PresetTarget =
  | { kind: "repo"; repo_id: string }
  | { kind: "workspace"; workspace_id: string };

export type PresetLaunchJobStatus =
  | "resolving"
  | "running_scripts"
  | "spawning"
  | "completed"
  | "cancelled"
  | "failed"
  | "unknown";

export interface PresetLaunchJobSnapshot {
  job_id: string;
  preset_id: string;
  target: PresetTarget;
  total: number;
  launched: number;
  created_session_ids: string[];
  created_tab_ids: string[];
  status: PresetLaunchJobStatus;
  error: string | null;
  current_tab_id: string | null;
}

export type PresetPromptSource =
  | { kind: "file" }
  | { kind: "folder"; relative_path: string }
  | { kind: "inline" };

export type LaunchPresetSource =
  | { kind: "file"; path: string }
  | { kind: "folder"; path: string }
  | { kind: "inline"; prompts: string[] };

export type PresetVariableKind =
  | { kind: "text" }
  | { kind: "file_path"; extensions: string[] }
  | { kind: "folder_path" }
  | { kind: "env_var"; name: string }
  | { kind: "literal_path"; path: string }
  | {
      kind: "script";
      cmd: string;
      args: string[];
      // Regex applied to trimmed stdout; first capture group becomes the
      // value. null → use the trimmed stdout verbatim.
      extract_pattern: string | null;
      timeout_ms: number;
      // When set, the script is skipped (variable resolves to empty) if the
      // earlier-declared variable named here resolved to empty. Lets a
      // preset gate an optional fetch on user input.
      skip_if_empty: string | null;
    }
  // Forward-compat: presets from a newer daemon may declare variable
  // kinds this client can't render. Launch dialog skips them.
  | { kind: "unknown" };

export interface ScriptCommandPreview {
  variable_name: string;
  cmd: string;
  args: string[];
}

export interface PresetVariable {
  name: string;
  label: string;
  kind: PresetVariableKind;
  prompt_at_launch: boolean;
  default: string | null;
  optional: boolean;
}

export interface FooterLine {
  label: string;
  variable: string;
}

export type TabLayout =
  | "tile_horizontal"
  | "tile_vertical"
  | "balanced_horizontal"
  | "balanced_vertical"
  | "auto_grid"
  // Forward-compat fallback. A daemon newer than this client may emit
  // layouts the client doesn't render natively; rather than crash the
  // sidebar, the union carries the unknown string and the renderer
  // picks a sensible fallback (BalancedHorizontal).
  | "unknown";

export type RearrangeLayout =
  | { kind: "horizontal" }
  | { kind: "vertical" }
  | { kind: "balanced" }
  // cols=0 → daemon auto-picks ceil(sqrt(N)).
  | { kind: "grid"; cols: number }
  | { kind: "unknown" };

export type TabGroupingConfig =
  | { kind: "none" }
  | {
      kind: "new_tab";
      layout: TabLayout;
      max_panes_per_tab: number | null;
      tab_name_template: string | null;
    };

export interface PresetEntry {
  id: string;
  name: string;
  description: string | null;
  source_repo_id: string;
  prompt_sources: PresetPromptSource[];
  prompt_template: string;
  context_footer_lines: FooterLine[];
  variables: PresetVariable[];
  branch_template: string;
  session_label_template: string | null;
  default_use_worktree: boolean | null;
  dangerously_skip_permissions: boolean;
  model: string | null;
  permission_mode: PermissionMode | null;
  // Which CLI a preset launches. Defaults to "claude" for preset files that
  // pre-date the codex field.
  agent: Agent;
  // Codex sandbox policy. Ignored when agent !== "codex".
  codex_sandbox: CodexSandbox | null;
  tab_grouping: TabGroupingConfig;
  injector: InjectorTemplate;
  stagger_ms: number;
}

export interface CleanupAction {
  repo_id: string;
  remove_worktree: boolean;
}

export interface WorktreeInfo {
  branch: string;
  path: string;
  // True if a non-stopped, non-parked session is currently using this path.
  is_active: boolean;
}

// Status of one `wt.<branch>/` group reported by InspectWorktreesRoot.
// Mirrors `protocol::RootWorktreeStatus`. New backend variants we don't
// recognize fall into "unknown" — the modal renders them with a dimmed
// badge but does NOT include them in bulk "Delete all stale".
export type RootWorktreeStatus =
  | { kind: "active" }
  | { kind: "detached" }
  | { kind: "stale" }
  | { kind: "unknown" };

export interface RootWorktreeMember {
  worktree_path: string;
  // Null when the gitfile inside the worktree is unreadable, points at a
  // non-existent repo, or has been corrupted. The daemon falls back to
  // plain `fs::remove_dir_all` for these.
  repo_path: string | null;
  // Best-effort display name: leaf component of repo_path when known,
  // else leaf component of worktree_path.
  repo_name_hint: string;
}

export interface RootWorktreeEntry {
  // Absolute path of the `wt.<branch>/` directory. This is the address
  // DeleteWorktreeAt operates on.
  path: string;
  // Display-only parent directory name (the sanitized "anchor").
  anchor: string;
  // Branch slug (the `wt.` prefix already stripped).
  branch_slug: string;
  members: RootWorktreeMember[];
  status: RootWorktreeStatus;
  // Session id referencing this group when status is "active" or
  // "detached". Null for "stale".
  session_id: string | null;
  // Total bytes used by all member directories. Null if the walk failed.
  size_bytes: number | null;
  // Most-recent modified timestamp across the group's contents, unix
  // seconds. Null if no readable metadata.
  last_modified_unix: number | null;
}

// Surfaced inside WorktreeCleanupFailed: one process holding open file
// handles inside a worktree dir the daemon couldn't remove. Populated
// from the daemon's Restart Manager probe on Windows; on other
// platforms the daemon always returns an empty array.
export interface WorktreeLockingProcess {
  pid: number;
  // Friendly process name (typically the .exe leaf — "node.exe",
  // "Code.exe", etc.).
  name: string;
  // Full command line joined by spaces. Null when the process exited
  // between RM's snapshot and the per-pid cmdline enrichment.
  cmdline: string | null;
}

// One member worktree that survived every cleanup attempt during a
// discard. The modal lets the user kill the locking processes and
// retry, open the parent dir in Explorer, or ignore.
export interface WorktreeCleanupFailure {
  member_path: string;
  // Repo path the daemon needs for the retry's `git worktree remove`.
  repo_path: string;
  // Final error chain from the last cleanup attempt.
  reason: string;
  locking_processes: WorktreeLockingProcess[];
}

// One cleanup target inside a RetryWorktreeCleanup request. The daemon
// re-validates these against the configured worktrees root before
// touching anything.
export interface RetryWorktreeTarget {
  member_path: string;
  repo_path: string;
}

export interface MemberSpawnPreview {
  repo_id: string;
  repo_name: string;
  branch_exists: boolean;
  effective_base: string | null;
  worktree_path: string;
}

export interface VscodeWorkspaceFolder {
  path: string;
  name: string | null;
  matched_repo_id: string | null;
}

export interface VscodeWorkspaceSuggestion {
  source_path: string;
  suggested_name: string;
  folders: VscodeWorkspaceFolder[];
}

export interface GitCommit {
  sha: string;
  short_sha: string;
  author_name: string;
  author_email: string;
  authored_at: string;
  subject: string;
}

export interface GitFileChange {
  path: string;
  status: string;
  from_path: string | null;
}

export interface GitCommitDetail {
  commit: GitCommit;
  body: string;
  parent_shas: string[];
  changes: GitFileChange[];
}

export interface GitRemoteUrl {
  repo_id: string;
  raw_url: string;
  web_url: string | null;
  forge: string;
}

export interface GitStash {
  // `stash@{N}` ref. Renumbered after each drop/pop, so the UI must
  // re-request after any stash mutation.
  id: string;
  // `WIP on <branch>: <sha> <message>` subject from `git stash list`.
  subject: string;
  // ISO-8601 timestamp.
  created_at: string;
}

// ------- Tabs and grids -------

export type SplitDirection = "horizontal" | "vertical";
export type SplitPlace = "first" | "second";
export type PaneDropEdge = "left" | "right" | "top" | "bottom" | "replace";
export type MergeLayout = "tile_horizontal" | "tile_vertical";

export type GridNode =
  | { kind: "pane"; pane_id: string; session_id: string | null }
  | {
      kind: "split";
      direction: SplitDirection;
      ratio: number;
      first: GridNode;
      second: GridNode;
    };

export type TabContent =
  | { kind: "grid"; grid: GridNode }
  | {
      kind: "diff";
      repo_id: string;
      path: string;
      against: string | null;
    };

export interface TabEntry {
  id: string;
  name: string;
  content: TabContent;
  created_at: string;
}

/**
 * Borrow the grid of `tab` when the tab is a grid kind. Returns `null` for
 * other kinds — callers that render pane layouts must guard on this, since
 * pane-level operations are undefined for non-grid tabs.
 */
export function tabGrid(tab: TabEntry): GridNode | null {
  return tab.content.kind === "grid" ? tab.content.grid : null;
}

// ------- Wire envelopes -------

export type ClientMessage =
  | {
      type: "hello";
      protocol_version: number;
      protocol_versions: number[];
      auth_token: string;
    }
  | { type: "list_repos" }
  | { type: "add_repo"; path: string; name: string | null }
  | { type: "remove_repo"; repo_id: string }
  | { type: "list_workspaces" }
  | {
      type: "upsert_workspace";
      id: string | null;
      name: string;
      member_repo_ids: string[];
      linked_vscode_workspace: string | null;
    }
  | { type: "remove_workspace"; workspace_id: string }
  | { type: "list_sessions" }
  | ({ type: "spawn_session" } & SpawnRequest)
  | { type: "duplicate_session"; session_id: string }
  | { type: "get_spawn_config"; session_id: string }
  | { type: "rename_session"; session_id: string; label: string | null }
  | {
      type: "set_repo_appearance";
      repo_id: string;
      appearance: AppearanceOverrides;
    }
  | {
      type: "set_workspace_appearance";
      workspace_id: string;
      appearance: AppearanceOverrides;
    }
  | {
      type: "set_session_appearance";
      session_id: string;
      appearance: AppearanceOverrides;
    }
  | { type: "attach"; session_id: string }
  | { type: "detach"; session_id: string }
  | { type: "send_input"; session_id: string; data_b64: string }
  | { type: "resize"; session_id: string; cols: number; rows: number }
  | { type: "stop_session"; session_id: string; cleanup: CleanupAction[] }
  | { type: "resume_abandoned"; session_id: string }
  | { type: "discard_abandoned"; session_id: string }
  | { type: "park_session"; session_id: string }
  | { type: "discard_session"; session_id: string; cleanup: CleanupAction[] }
  | { type: "resume_all_abandoned" }
  | { type: "list_branches"; repo_id: string }
  | { type: "list_worktrees"; repo_id: string }
  | {
      type: "preview_workspace_spawn";
      workspace_id: string;
      branch_name: string;
      base_branch: string | null;
    }
  | {
      type: "accept_vscode_workspace_suggestion";
      suggestion: VscodeWorkspaceSuggestion;
      watch: boolean;
    }
  | {
      type: "list_commits";
      repo_id: string;
      branch: string | null;
      limit: number;
      // Maps to `git log --skip`. Defaults to 0 server-side.
      offset?: number;
    }
  | { type: "get_commit"; repo_id: string; sha: string }
  | {
      type: "get_file_diff";
      repo_id: string;
      path: string;
      against: string | null;
    }
  | { type: "get_remote_url"; repo_id: string }
  | { type: "repo_status"; repo_id: string }
  | { type: "stage_files"; repo_id: string; paths: string[] }
  | { type: "unstage_files"; repo_id: string; paths: string[] }
  | { type: "commit_repo"; repo_id: string; message: string }
  | { type: "discard_changes"; repo_id: string; paths: string[] }
  | { type: "stash_push"; repo_id: string; message: string }
  | { type: "list_stashes"; repo_id: string }
  | { type: "stash_pop"; repo_id: string; stash_id: string }
  | { type: "stash_apply"; repo_id: string; stash_id: string }
  | { type: "stash_drop"; repo_id: string; stash_id: string }
  | {
      type: "open_diff_tab";
      id: string;
      repo_id: string;
      path: string;
      against: string | null;
    }
  | {
      type: "get_file_snapshot";
      id: string;
      repo_id: string;
      path: string;
      against: string | null;
    }
  | { type: "load_scrollback"; session_id: string }
  // `drain` defaults to true server-side. Pass `drain: false` to retain
  // session sidecars so the next daemon start surfaces them as
  // Abandoned (B.3). Pre-Phase-C the children still die — sidecar
  // preservation is the half that exists today.
  | { type: "shutdown"; drain?: boolean }
  | { type: "set_repo_worktree_default"; repo_id: string; value: boolean }
  | {
      type: "set_workspace_worktree_default";
      workspace_id: string;
      value: boolean;
    }
  // path: null clears the user override and reverts to the env/platform
  // default. Daemon validates + creates the directory; broadcasts
  // `worktrees_root_changed` on success.
  | { type: "set_worktrees_root"; path: string | null }
  // Scan the worktrees root and cross-reference against the session
  // registry. Reply: `worktrees_root_snapshot`.
  | { type: "inspect_worktrees_root" }
  // Delete one `wt.<branch>/` group on disk. `path` is taken from a
  // prior `worktrees_root_snapshot` entry. Daemon refuses if the group
  // is in Active status and replies with a fresh snapshot on success.
  | { type: "delete_worktree_at"; path: string }
  // Sent from the WorktreeCleanupFailedDialog. Daemon attempts a polite
  // Restart Manager shutdown of `kill_pids` first, then hard-kills any
  // survivor and re-runs the robust cleanup for each `targets` member.
  // If any member is still wedged the daemon re-emits
  // `worktree_cleanup_failed` so the user can iterate.
  | {
      type: "retry_worktree_cleanup";
      session_id: string;
      kill_pids: number[];
      targets: RetryWorktreeTarget[];
    }
  | { type: "list_tabs" }
  | {
      type: "create_tab";
      name: string | null;
      initial_session_id: string | null;
    }
  | { type: "close_tab"; tab_id: string }
  | { type: "restore_tab"; tab: TabEntry; index: number }
  | { type: "restore_tab_snapshot"; tab: TabEntry }
  | { type: "rename_tab"; tab_id: string; name: string }
  | { type: "reorder_tabs"; ordered_ids: string[] }
  | { type: "reorder_containers"; ordered: ContainerRef[] }
  | { type: "parse_vscode_workspace"; path: string }
  | { type: "scan_vscode_workspaces"; path: string }
  | {
      type: "reorder_sessions";
      container_id: string;
      ordered_ids: string[];
    }
  | {
      type: "split_pane";
      tab_id: string;
      pane_id: string;
      direction: SplitDirection;
      place: SplitPlace;
      new_session_id: string | null;
    }
  | { type: "close_pane"; tab_id: string; pane_id: string }
  | {
      type: "set_pane_ratio";
      tab_id: string;
      split_path: number[];
      ratio: number;
    }
  | {
      type: "move_pane";
      src_tab_id: string;
      src_pane_id: string;
      dst_tab_id: string;
      dst_pane_id: string;
      edge: PaneDropEdge;
    }
  | {
      type: "replace_pane_session";
      tab_id: string;
      pane_id: string;
      session_id: string | null;
    }
  | {
      type: "rearrange_tab";
      tab_id: string;
      layout: RearrangeLayout;
    }
  | {
      type: "merge_tabs";
      tab_ids: string[];
      name: string | null;
      layout: MergeLayout;
    }
  | {
      type: "extract_to_new_tab";
      source_tab_id: string;
      pane_ids: string[];
      name: string | null;
    }
  | { type: "list_presets"; target: PresetTarget }
  | {
      type: "launch_preset";
      job_id: string;
      target: PresetTarget;
      preset_id: string;
      source: LaunchPresetSource;
      variable_values: Array<[string, string]>;
      use_worktree_override: boolean | null;
      max_panes_per_tab_override: number | null;
    }
  | { type: "cancel_preset_launch"; job_id: string }
  | {
      type: "preview_preset";
      id: string;
      target: PresetTarget;
      preset_id: string;
      source: LaunchPresetSource;
      variable_values: Array<[string, string]>;
    }
  | {
      type: "resolve_preset_scripts";
      id: string;
      target: PresetTarget;
      preset_id: string;
      variable_values: Array<[string, string]>;
    };

export type DaemonMessage =
  | {
      type: "welcome";
      protocol_version: number;
      supported_versions: number[];
    }
  | { type: "auth_failed"; reason: string }
  | { type: "shutdown_ack" }
  | { type: "repos"; repos: RepoEntry[] }
  | { type: "workspaces"; workspaces: WorkspaceEntry[] }
  | {
      type: "branches";
      repo_id: string;
      branches: string[];
      current: string | null;
    }
  | {
      type: "worktrees";
      repo_id: string;
      worktrees: WorktreeInfo[];
    }
  | {
      type: "worktrees_root_changed";
      root: string;
      is_override: boolean;
    }
  | {
      type: "worktrees_root_snapshot";
      root: string;
      is_override: boolean;
      entries: RootWorktreeEntry[];
    }
  // Emitted by `discard_session` (and its retry) when one or more
  // member worktrees survived every cleanup attempt (git remove + retry
  // + fs::remove_dir_all all failed). The frontend shows a modal listing
  // the locking processes and lets the user kill them and retry, open
  // the parent dir in Explorer, or ignore.
  | {
      type: "worktree_cleanup_failed";
      session_id: string;
      session_label: string;
      failures: WorktreeCleanupFailure[];
    }
  | { type: "sessions"; sessions: SessionSnapshot[] }
  | { type: "session_updated"; session: SessionSnapshot }
  | { type: "session_removed"; session_id: string }
  | {
      type: "spawn_config_reply";
      session_id: string;
      // null when the session is unknown or its sidecar pre-dates the
      // spawn-config field; the UI should fall back to Settings defaults
      // for the spawn dialog in that case.
      config: SpawnConfig | null;
    }
  | { type: "pty_output"; session_id: string; data_b64: string }
  | { type: "attention"; session_id: string; reason: string }
  | {
      type: "workspace_spawn_preview";
      workspace_id: string;
      branch_name: string;
      per_member: MemberSpawnPreview[];
    }
  | {
      type: "vscode_workspace_suggestion";
      repo_id: string;
      suggestion: VscodeWorkspaceSuggestion;
    }
  | {
      type: "vscode_workspaces_scanned";
      path: string;
      suggestions: VscodeWorkspaceSuggestion[];
    }
  | {
      type: "commits";
      repo_id: string;
      commits: GitCommit[];
      // Echoed from the request: 0 = fresh listing, >0 = append batch.
      offset: number;
    }
  | { type: "commit_detail"; repo_id: string; detail: GitCommitDetail }
  | {
      type: "file_diff";
      repo_id: string;
      path: string;
      against: string | null;
      diff: string;
    }
  | ({ type: "remote_url" } & GitRemoteUrl)
  | {
      type: "repo_status";
      repo_id: string;
      index_changes: GitFileChange[];
      worktree_changes: GitFileChange[];
    }
  | {
      type: "commit_ok";
      repo_id: string;
      sha: string;
      short_sha: string;
    }
  | {
      type: "git_write_error";
      repo_id: string;
      operation: string;
      error: string;
    }
  | { type: "stashes"; repo_id: string; stashes: GitStash[] }
  | { type: "diff_tab_opened"; id: string; tab_id: string }
  | {
      type: "file_snapshot";
      id: string;
      repo_id: string;
      path: string;
      against: string | null;
      old: string;
      new: string;
      language: string;
    }
  | {
      type: "file_snapshot_error";
      id: string;
      repo_id: string;
      path: string;
      against: string | null;
      error: string;
    }
  | {
      type: "scrollback";
      session_id: string;
      data_b64: string;
      truncated: boolean;
    }
  | { type: "tabs"; tabs: TabEntry[] }
  | { type: "tab_updated"; tab: TabEntry }
  | { type: "tab_removed"; tab_id: string }
  | { type: "tabs_reordered"; ordered_ids: string[] }
  | { type: "containers_reordered"; ordered: ContainerRef[] }
  | {
      type: "vscode_workspace_parsed";
      suggestion: VscodeWorkspaceSuggestion;
    }
  | {
      type: "sessions_reordered";
      container_id: string;
      ordered_ids: string[];
    }
  | { type: "presets"; target: PresetTarget; entries: PresetEntry[] }
  | {
      type: "preset_launch_progress";
      job_id: string;
      preset_id: string;
      total: number;
      launched: number;
      current_tab_id: string | null;
      tab_ids: string[];
    }
  | {
      type: "preset_launch_failed";
      job_id: string;
      preset_id: string;
      error: string;
      partial_session_ids: string[];
      partial_tab_ids: string[];
    }
  | { type: "preset_launch_job_updated"; job: PresetLaunchJobSnapshot }
  | { type: "preset_preview"; id: string; prompts: string[] }
  | { type: "preset_preview_error"; id: string; error: string }
  | {
      type: "preset_scripts_resolved";
      id: string;
      values: Array<[string, string]>;
      executed_commands: ScriptCommandPreview[];
    }
  | {
      type: "preset_scripts_error";
      id: string;
      error: string;
      variable_name: string | null;
    }
  | { type: "error"; message: string };
