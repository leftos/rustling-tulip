/**
 * Hand-mirrored subset of `crates/protocol/src/lib.rs` — only the messages
 * the e2e harness actually sends or receives. Bump when the Rust enum changes.
 *
 * The serialization contract is `#[serde(tag = "type", rename_all = "snake_case")]`
 * for both directions, matching `apps/tauri-app/src/types.ts`.
 */

export interface RepoEntry {
  id: string;
  name: string;
  path: string;
  default_branch: string | null;
  default_use_worktree: boolean;
  appearance: AppearanceOverrides;
}

export interface AppearanceOverrides {
  accent_color: string | null;
  terminal_background_color: string | null;
  terminal_frame_color: string | null;
  terminal_font_family: string | null;
  terminal_font_size: number | null;
  terminal_font_bold: boolean | null;
}

export type SessionStatus =
  | "spawning"
  | "idle"
  | "working"
  | "awaiting_input"
  | "stopped"
  | "error";

export type SessionMode = "interactive" | "headless" | "plain_shell";
export type SessionKind = "single" | "workspace" | "standalone";

export interface SessionMember {
  repo_id: string;
  repo_name: string;
  branch: string;
  worktree_path: string;
}

export interface SessionSnapshot {
  id: string;
  label: string;
  user_label?: string | null;
  kind: SessionKind;
  members: SessionMember[];
  status: SessionStatus;
  mode: SessionMode;
  started_at: string;
  exit_code: number | null;
  metrics: {
    input_tokens: number;
    output_tokens: number;
    cost_usd: number;
    last_activity_at: string | null;
  };
  recent_actions: string[];
  is_orphan: boolean;
  workspace_id: string | null;
  terminal_title?: string | null;
  current_cwd?: string | null;
  appearance?: AppearanceOverrides;
  elevated_authority?: boolean;
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

export type SplitDirection = "horizontal" | "vertical";
export type SplitPlace = "first" | "second";

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

// --- Client → Daemon ---------------------------------------------------------

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

export type LaunchPresetSource =
  | { kind: "file"; path: string }
  | { kind: "folder"; path: string }
  | { kind: "inline"; prompts: string[] };

export type ClientMessage =
  | {
      type: "hello";
      protocol_version: number;
      auth_token: string;
      client_id?: string;
    }
  | { type: "list_repos" }
  | { type: "add_repo"; path: string; name: string | null }
  | { type: "scan_vscode_workspaces"; path: string }
  | { type: "remove_repo"; repo_id: string }
  | { type: "list_sessions" }
  | { type: "spawn_session"; [k: string]: unknown }
  | { type: "attach"; session_id: string }
  | { type: "detach"; session_id: string }
  | { type: "send_input"; session_id: string; data_b64: string }
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
  | { type: "load_scrollback"; session_id: string }
  | { type: "stop_session"; session_id: string; cleanup: Array<{ repo_id: string; remove_worktree: boolean }> }
  | { type: "discard_session"; session_id: string; cleanup: Array<{ repo_id: string; remove_worktree: boolean }> }
  | { type: "list_tabs" }
  | {
      type: "create_tab";
      name: string | null;
      initial_session_id: string | null;
    }
  | {
      type: "split_pane";
      tab_id: string;
      pane_id: string;
      direction: SplitDirection;
      place: SplitPlace;
      new_session_id: string | null;
    }
  | { type: "close_tab"; tab_id: string }
  | { type: "restore_tab"; tab: TabEntry; index: number }
  | { type: "restore_tab_snapshot"; tab: TabEntry }
  | { type: "repo_status"; repo_id: string }
  | { type: "stage_files"; repo_id: string; paths: string[] }
  | { type: "unstage_files"; repo_id: string; paths: string[] }
  | { type: "commit_repo"; repo_id: string; message: string }
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
  | { type: "inspect_worktrees_root" }
  | {
      type: "preview_preset";
      id: string;
      target: PresetTarget;
      preset_id: string;
      source: LaunchPresetSource;
      variable_values: Array<[string, string]>;
    }
  | { type: "shutdown" };

// --- Worktrees root inspection -----------------------------------------------

export type RootWorktreeStatus =
  | { kind: "active" }
  | { kind: "detached" }
  | { kind: "stale" }
  | { kind: "unknown" };

export interface PinnedMemberWorktree {
  repo_id: string;
  path: string;
}

export type WorktreeLaunchTarget =
  | {
      kind: "single";
      repo_id: string;
      branch: string | null;
      worktree_path: string;
    }
  | {
      kind: "workspace";
      workspace_id: string;
      branch: string | null;
      members: PinnedMemberWorktree[];
    }
  | { kind: "unknown" };

export interface RootWorktreeEntry {
  path: string;
  anchor: string;
  branch_slug: string;
  members: Array<{
    worktree_path: string;
    repo_path: string | null;
    repo_name_hint: string;
  }>;
  status: RootWorktreeStatus;
  session_id: string | null;
  size_bytes: number | null;
  last_modified_unix: number | null;
  launch: WorktreeLaunchTarget | null;
  launch_blocked_reason: string | null;
}

// --- Daemon → Client ---------------------------------------------------------

export type DaemonMessage =
  | { type: "welcome"; protocol_version: number }
  | { type: "auth_failed"; reason: string }
  | { type: "repos"; repos: RepoEntry[] }
  | { type: "sessions"; sessions: SessionSnapshot[] }
  | { type: "session_updated"; session: SessionSnapshot }
  | {
      type: "vscode_workspaces_scanned";
      path: string;
      suggestions: VscodeWorkspaceSuggestion[];
    }
  | { type: "session_removed"; session_id: string }
  | { type: "tab_updated"; tab: TabEntry }
  | { type: "pty_output"; session_id: string; data_b64: string }
  | {
      type: "preset_launch_job_updated";
      job: PresetLaunchJobSnapshot;
    }
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
  | { type: "preset_preview"; id: string; prompts: string[] }
  | { type: "preset_preview_error"; id: string; error: string }
  | {
      type: "worktrees_root_snapshot";
      root: string;
      is_override: boolean;
      entries: RootWorktreeEntry[];
    }
  | { type: "error"; message: string }
  // Catch-all for messages we don't model — keeps the union exhaustive
  // without forcing us to track every variant.
  | { type: string; [k: string]: unknown };
