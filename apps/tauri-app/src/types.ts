// Mirrors the Rust protocol crate. Keep in sync with crates/protocol/src/lib.rs.

export const PROTOCOL_VERSION = 6;

export type Agent = "claude" | "codex";

export type CodexSandbox = "read-only" | "workspace-write" | "danger-full-access";

export interface RepoEntry {
  id: string;
  name: string;
  path: string;
  default_branch: string | null;
  default_use_worktree: boolean;
  // Last agent spawned against this repo. Drives the spawn-dialog default.
  // null for repos that have never been launched.
  last_agent: Agent | null;
}

export interface WorkspaceEntry {
  id: string;
  name: string;
  member_repo_ids: string[];
  linked_vscode_workspace: string | null;
  default_use_worktree: boolean;
}

export type SessionKind = "single" | "workspace";
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
  kind: SessionKind;
  members: SessionMember[];
  status: SessionStatus;
  mode: SessionMode;
  started_at: string;
  exit_code: number | null;
  metrics: SessionMetrics;
  recent_actions: string[];
  is_orphan: boolean;
  // For workspace-kind sessions: the workspace this session belongs to.
  // null for single-repo sessions.
  workspace_id: string | null;
  // Which CLI is driving this session. Drives the agent badge in the UI.
  agent: Agent;
  // Last OSC 0/2 window title emitted by the agent/shell. Distinct from
  // `label` so the canonical sidebar/header name stays the daemon-curated
  // `<repo>:<branch>` (or user override) rather than being clobbered by
  // shell-emitted titles like `C:\WINDOWS\system32\cmd.exe`. Surfaced as a
  // tooltip; never as the primary label. null until the agent emits its
  // first title.
  terminal_title: string | null;
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

// ------- Prompt injection & presets -------

export type InjectorStep =
  | { kind: "delay"; ms: number }
  | { kind: "write"; data_b64: string }
  | { kind: "text"; content: string; newline: boolean };

export interface PromptInjector {
  steps: InjectorStep[];
}

export interface InjectorTemplate {
  startup_delay_ms: number;
  pre_input: InjectorStep[];
  post_input: InjectorStep[];
}

export type PresetTarget =
  | { kind: "repo"; repo_id: string }
  | { kind: "workspace"; workspace_id: string };

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
  | { kind: "literal_path"; path: string };

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
  | "balanced_vertical";

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

export interface MemberDiff {
  repo_id: string;
  repo_name: string;
  changed_files: string[];
  clean: boolean;
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

export interface TabEntry {
  id: string;
  name: string;
  grid: GridNode;
  created_at: string;
}

// ------- Wire envelopes -------

export type ClientMessage =
  | { type: "hello"; protocol_version: number; auth_token: string }
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
  | { type: "attach"; session_id: string }
  | { type: "detach"; session_id: string }
  | { type: "send_input"; session_id: string; data_b64: string }
  | { type: "resize"; session_id: string; cols: number; rows: number }
  | { type: "stop_session"; session_id: string; cleanup: CleanupAction[] }
  | { type: "session_diff"; session_id: string }
  | { type: "list_branches"; repo_id: string }
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
  | { type: "load_scrollback"; session_id: string }
  | { type: "shutdown" }
  | { type: "set_repo_worktree_default"; repo_id: string; value: boolean }
  | {
      type: "set_workspace_worktree_default";
      workspace_id: string;
      value: boolean;
    }
  | { type: "list_tabs" }
  | {
      type: "create_tab";
      name: string | null;
      initial_session_id: string | null;
    }
  | { type: "close_tab"; tab_id: string }
  | { type: "rename_tab"; tab_id: string; name: string }
  | { type: "reorder_tabs"; ordered_ids: string[] }
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
      target: PresetTarget;
      preset_id: string;
      source: LaunchPresetSource;
      variable_values: Array<[string, string]>;
      use_worktree_override: boolean | null;
      max_panes_per_tab_override: number | null;
    }
  | {
      type: "preview_preset";
      id: string;
      target: PresetTarget;
      preset_id: string;
      source: LaunchPresetSource;
      variable_values: Array<[string, string]>;
    };

export type DaemonMessage =
  | { type: "welcome"; protocol_version: number }
  | { type: "auth_failed"; reason: string }
  | { type: "repos"; repos: RepoEntry[] }
  | { type: "workspaces"; workspaces: WorkspaceEntry[] }
  | {
      type: "branches";
      repo_id: string;
      branches: string[];
      current: string | null;
    }
  | { type: "sessions"; sessions: SessionSnapshot[] }
  | { type: "session_updated"; session: SessionSnapshot }
  | { type: "session_removed"; session_id: string }
  | { type: "pty_output"; session_id: string; data_b64: string }
  | { type: "attention"; session_id: string; reason: string }
  | { type: "session_diff"; session_id: string; per_member: MemberDiff[] }
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
  | { type: "commits"; repo_id: string; commits: GitCommit[] }
  | { type: "commit_detail"; repo_id: string; detail: GitCommitDetail }
  | {
      type: "file_diff";
      repo_id: string;
      path: string;
      against: string | null;
      diff: string;
    }
  | ({ type: "remote_url" } & GitRemoteUrl)
  | { type: "repo_status"; repo_id: string; changes: GitFileChange[] }
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
  | { type: "presets"; target: PresetTarget; entries: PresetEntry[] }
  | {
      type: "preset_launch_progress";
      preset_id: string;
      total: number;
      launched: number;
      current_tab_id: string | null;
      tab_ids: string[];
    }
  | {
      type: "preset_launch_failed";
      preset_id: string;
      error: string;
      partial_session_ids: string[];
      partial_tab_ids: string[];
    }
  | { type: "preset_preview"; id: string; prompts: string[] }
  | { type: "preset_preview_error"; id: string; error: string }
  | { type: "error"; message: string };
