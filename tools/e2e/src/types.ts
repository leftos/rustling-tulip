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
}

export type SessionStatus =
  | "spawning"
  | "idle"
  | "working"
  | "awaiting_input"
  | "stopped"
  | "error";

export type SessionMode = "interactive" | "headless" | "plain_shell";
export type SessionKind = "single" | "workspace";

export interface SessionMember {
  repo_id: string;
  repo_name: string;
  branch: string;
  worktree_path: string;
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
}

// --- Client → Daemon ---------------------------------------------------------

export type PresetTarget =
  | { kind: "repo"; repo_id: string }
  | { kind: "workspace"; workspace_id: string };

export type LaunchPresetSource =
  | { kind: "file"; path: string }
  | { kind: "folder"; path: string }
  | { kind: "inline"; prompts: string[] };

export type ClientMessage =
  | { type: "hello"; protocol_version: number; auth_token: string }
  | { type: "list_repos" }
  | { type: "add_repo"; path: string; name: string | null }
  | { type: "remove_repo"; repo_id: string }
  | { type: "list_sessions" }
  | { type: "spawn_session"; [k: string]: unknown }
  | { type: "attach"; session_id: string }
  | { type: "detach"; session_id: string }
  | { type: "load_scrollback"; session_id: string }
  | { type: "stop_session"; session_id: string; cleanup: Array<{ repo_id: string; remove_worktree: boolean }> }
  | { type: "list_tabs" }
  | {
      type: "create_tab";
      name: string | null;
      initial_session_id: string | null;
    }
  | { type: "close_tab"; tab_id: string }
  | { type: "repo_status"; repo_id: string }
  | { type: "stage_files"; repo_id: string; paths: string[] }
  | { type: "unstage_files"; repo_id: string; paths: string[] }
  | { type: "commit_repo"; repo_id: string; message: string }
  | { type: "list_presets"; target: PresetTarget }
  | {
      type: "preview_preset";
      id: string;
      target: PresetTarget;
      preset_id: string;
      source: LaunchPresetSource;
      variable_values: Array<[string, string]>;
    }
  | { type: "shutdown" };

// --- Daemon → Client ---------------------------------------------------------

export type DaemonMessage =
  | { type: "welcome"; protocol_version: number }
  | { type: "auth_failed"; reason: string }
  | { type: "repos"; repos: RepoEntry[] }
  | { type: "sessions"; sessions: SessionSnapshot[] }
  | { type: "session_updated"; session: SessionSnapshot }
  | { type: "session_removed"; session_id: string }
  | { type: "pty_output"; session_id: string; data_b64: string }
  | { type: "preset_preview"; id: string; prompts: string[] }
  | { type: "preset_preview_error"; id: string; error: string }
  | { type: "error"; message: string }
  // Catch-all for messages we don't model — keeps the union exhaustive
  // without forcing us to track every variant.
  | { type: string; [k: string]: unknown };
