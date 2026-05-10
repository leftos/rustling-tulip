// Mirrors the Rust protocol crate. Keep in sync with crates/protocol/src/lib.rs.

export const PROTOCOL_VERSION = 1;

export interface RepoEntry {
  id: string;
  name: string;
  path: string;
  default_branch: string | null;
}

export interface WorkspaceEntry {
  id: string;
  name: string;
  member_repo_ids: string[];
  linked_vscode_workspace: string | null;
}

export type SessionKind = "single" | "workspace";
export type SessionStatus =
  | "spawning"
  | "idle"
  | "working"
  | "awaiting_input"
  | "stopped"
  | "error";
export type SessionMode = "interactive" | "headless";

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
}

export type BranchTarget =
  | { kind: "existing"; name: string }
  | { kind: "new_from_base"; name: string; base: string };

export type SpawnTarget =
  | { kind: "single"; repo_id: string; branch: BranchTarget }
  | {
      kind: "workspace";
      workspace_id: string;
      branch_name: string;
      base_branch: string | null;
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
  dangerously_skip_permissions: boolean;
  model: string | null;
  permission_mode: PermissionMode | null;
  extra_env: Array<[string, string]>;
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
  | { type: "repo_status"; repo_id: string };

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
  | { type: "error"; message: string };
