import { invoke } from "@tauri-apps/api/core";
import {
  PROTOCOL_VERSION,
  SUPPORTED_PROTOCOL_VERSIONS,
  type ClientMessage,
  type DaemonMessage,
  type GitRemoteUrl,
  type GitStash,
  type LaunchPresetSource,
  type PresetEntry,
  type PresetTarget,
  type ScriptCommandPreview,
  type SpawnConfig,
} from "./types";

export interface DaemonHandshake {
  protocol_version: number;
  port: number;
  auth_token: string;
  pid: number;
}

export interface DaemonClient {
  send(msg: ClientMessage): void;
  onMessage(cb: (msg: DaemonMessage) => void): () => void;
  onConnectionChange(cb: (state: ConnectionState) => void): () => void;
  state(): ConnectionState;
  close(): void;
}

export type ConnectionState =
  | { kind: "connecting" }
  // `negotiatedVersion` is undefined until the daemon's Welcome arrives;
  // feature-gated UI should treat undefined as "assume oldest supported".
  | { kind: "open"; negotiatedVersion?: number }
  | { kind: "closed"; reason: string }
  | { kind: "auth_failed"; reason: string };

export async function ensureDaemonStarted(): Promise<DaemonHandshake> {
  return await invoke<DaemonHandshake>("ensure_daemon_started");
}

/// Paths the daemon-footer troubleshooting flyout exposes for "open log",
/// "reveal config dir", and "copy handshake path". Mirrors the Rust
/// `DaemonPaths` struct in `apps/tauri-app/src-tauri/src/lib.rs`.
export interface DaemonPaths {
  config_dir: string;
  daemon_log: string;
  app_log: string;
  handshake_file: string;
}

export async function fetchDaemonPaths(): Promise<DaemonPaths> {
  return await invoke<DaemonPaths>("daemon_paths");
}

/// Force-stop the running daemon (pid-kill + remove handshake). Bypasses
/// the WS-level `shutdown` message because the WS may not be in a state to
/// accept one (e.g. mid-reconnect). Caller should set the stop-requested
/// flag BEFORE calling so the auto-reconnect cycle doesn't immediately
/// respawn it.
export async function stopDaemon(): Promise<void> {
  await invoke<void>("stop_daemon");
}

// Picker memory: store the parent directory of the last successful pick
// keyed by a caller-supplied purpose tag, so the next time the same picker
// opens (across reloads / app restarts) it lands the user where they were.
//
// Scoping is by tag, not by absolute global state — so the "Add repo"
// picker doesn't fight the "pick a bug bundle" picker over which dir to
// open at. Tags are arbitrary strings; pick what makes sense per call site
// (e.g. "addRepo", "presetSourceFile", `presetVar:${name}`).
const PICKER_LAST_DIR_PREFIX = "rt.picker.lastDir:";

function loadLastDir(key: string): string | undefined {
  try {
    return localStorage.getItem(PICKER_LAST_DIR_PREFIX + key) ?? undefined;
  } catch {
    return undefined;
  }
}

function saveLastDir(key: string, dir: string): void {
  try {
    localStorage.setItem(PICKER_LAST_DIR_PREFIX + key, dir);
  } catch {
    /* localStorage unavailable — best-effort, drop silently */
  }
}

function parentDir(p: string): string {
  // Cross-platform parent extraction: handle both `\` and `/` separators
  // because file pickers on Windows return native back-slash paths and
  // hand-typed defaults could use either.
  const trimmed = p.replace(/[\\/]+$/, "");
  const lastSep = Math.max(trimmed.lastIndexOf("\\"), trimmed.lastIndexOf("/"));
  return lastSep > 0 ? trimmed.slice(0, lastSep) : trimmed;
}

export interface PickDirectoryOpts {
  /// If set and `defaultPath` isn't supplied, opens the picker at the
  /// directory the user last picked from with this key. On a successful
  /// pick, the parent of the chosen folder is stored back under the key.
  lastDirKey?: string;
}

export async function pickDirectory(
  defaultPath?: string,
  opts: PickDirectoryOpts = {},
): Promise<string | null> {
  const effective =
    defaultPath ?? (opts.lastDirKey ? loadLastDir(opts.lastDirKey) : undefined);
  const result = await invoke<string | null>("pick_directory", {
    defaultPath: effective ?? null,
  });
  if (result !== null && opts.lastDirKey) {
    saveLastDir(opts.lastDirKey, parentDir(result));
  }
  return result;
}

export interface PickFileOpts {
  defaultPath?: string;
  extensions?: string[];
  filterName?: string;
  /// See [`PickDirectoryOpts.lastDirKey`]. On a successful file pick the
  /// parent directory of the chosen file is stored back under the key, so
  /// the next prompt opens in the same folder.
  lastDirKey?: string;
}

export async function pickFile(opts: PickFileOpts = {}): Promise<string | null> {
  const effective =
    opts.defaultPath ?? (opts.lastDirKey ? loadLastDir(opts.lastDirKey) : undefined);
  const result = await invoke<string | null>("pick_file", {
    defaultPath: effective ?? null,
    extensions: opts.extensions ?? null,
    filterName: opts.filterName ?? null,
  });
  if (result !== null && opts.lastDirKey) {
    saveLastDir(opts.lastDirKey, parentDir(result));
  }
  return result;
}

export function connectDaemon(handshake: DaemonHandshake): DaemonClient {
  const url = `ws://127.0.0.1:${handshake.port}/ws`;
  const ws = new WebSocket(url);
  const messageCbs = new Set<(msg: DaemonMessage) => void>();
  const stateCbs = new Set<(s: ConnectionState) => void>();
  let state: ConnectionState = { kind: "connecting" };

  const setState = (next: ConnectionState) => {
    state = next;
    for (const cb of stateCbs) cb(next);
  };

  ws.addEventListener("open", () => {
    ws.send(
      JSON.stringify({
        type: "hello",
        protocol_version: PROTOCOL_VERSION,
        protocol_versions: [...SUPPORTED_PROTOCOL_VERSIONS],
        auth_token: handshake.auth_token,
      } satisfies ClientMessage),
    );
    setState({ kind: "open" });
  });

  ws.addEventListener("message", (ev) => {
    let parsed: DaemonMessage;
    try {
      parsed = JSON.parse(ev.data) as DaemonMessage;
    } catch (err) {
      console.error("malformed daemon message", err);
      return;
    }
    if (parsed.type === "welcome") {
      setState({ kind: "open", negotiatedVersion: parsed.protocol_version });
    }
    if (parsed.type === "auth_failed") {
      setState({ kind: "auth_failed", reason: parsed.reason });
    }
    for (const cb of messageCbs) cb(parsed);
  });

  ws.addEventListener("close", (ev) => {
    if (state.kind !== "auth_failed") {
      setState({
        kind: "closed",
        reason: ev.reason || `code ${ev.code}`,
      });
    }
  });

  ws.addEventListener("error", () => {
    if (ws.readyState === WebSocket.CLOSED && state.kind !== "auth_failed") {
      setState({ kind: "closed", reason: "socket error" });
    }
  });

  return {
    send(msg) {
      if (ws.readyState !== WebSocket.OPEN) {
        console.warn("send while not open", msg.type);
        return;
      }
      ws.send(JSON.stringify(msg));
    },
    onMessage(cb) {
      messageCbs.add(cb);
      return () => messageCbs.delete(cb);
    },
    onConnectionChange(cb) {
      stateCbs.add(cb);
      cb(state);
      return () => stateCbs.delete(cb);
    },
    state() {
      return state;
    },
    close() {
      ws.close();
    },
  };
}

/**
 * Request the persisted scrollback for a session and resolve with the
 * decoded payload. Times out after 2 s if the daemon never answers (e.g.
 * because the session was already removed) — the terminal still attaches
 * after the promise resolves.
 *
 * Listens on the `rt:scrollback` window event (dispatched by App.tsx's
 * message router) so this works from any component without piercing the
 * client's onMessage subscription.
 */
export function loadScrollback(
  client: DaemonClient,
  sessionId: string,
): Promise<{ data_b64: string; truncated: boolean } | null> {
  return new Promise((resolve) => {
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "scrollback" || detail.session_id !== sessionId) {
        return;
      }
      cleanup();
      resolve({ data_b64: detail.data_b64, truncated: detail.truncated });
    };
    const timer = window.setTimeout(() => {
      cleanup();
      resolve(null);
    }, 2000);
    const cleanup = () => {
      window.removeEventListener("rt:scrollback", handler);
      window.clearTimeout(timer);
    };
    window.addEventListener("rt:scrollback", handler);
    client.send({ type: "load_scrollback", session_id: sessionId });
  });
}

/**
 * Fetch the persisted SpawnConfig for a session so the spawn dialog can
 * pre-fill all fields with the source's values. Resolves with `null` when
 * the session is unknown, its sidecar pre-dates spawn-config persistence,
 * or the daemon doesn't reply within 2s.
 *
 * Listens on the `rt:spawn_config_reply` window event (dispatched by
 * App.tsx's message router) so this works from any component.
 */
export function requestSpawnConfig(
  client: DaemonClient,
  sessionId: string,
): Promise<SpawnConfig | null> {
  return new Promise((resolve) => {
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (
        detail.type !== "spawn_config_reply" ||
        detail.session_id !== sessionId
      ) {
        return;
      }
      cleanup();
      resolve(detail.config);
    };
    const timer = window.setTimeout(() => {
      cleanup();
      resolve(null);
    }, 2000);
    const cleanup = () => {
      window.removeEventListener("rt:spawn_config_reply", handler);
      window.clearTimeout(timer);
    };
    window.addEventListener("rt:spawn_config_reply", handler);
    client.send({ type: "get_spawn_config", session_id: sessionId });
  });
}

export type ListPresetsResult =
  | { ok: true; entries: PresetEntry[] }
  | { ok: false; reason: string };

/**
 * Request the preset list for a repo or workspace and resolve with the
 * response. Times out after 2 s. Mirrors loadScrollback's pattern: relies
 * on App.tsx re-dispatching `rt:presets` so any caller can subscribe.
 *
 * Returns a discriminated `{ ok: true } | { ok: false }` so the caller
 * can distinguish "fetched, none defined" (`ok: true, entries: []`) from
 * "request failed / timed out" (`ok: false`). Previously both fell
 * through to `entries: []`, which left the context menu stuck on
 * "(loading)" when the daemon was disconnected mid-request.
 */
export function listPresets(
  client: DaemonClient,
  target: PresetTarget,
): Promise<ListPresetsResult> {
  return new Promise((resolve) => {
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "presets") return;
      if (!targetsMatch(detail.target, target)) return;
      cleanup();
      resolve({ ok: true, entries: detail.entries });
    };
    const timer = window.setTimeout(() => {
      cleanup();
      resolve({ ok: false, reason: "timed out (daemon not responding)" });
    }, 2000);
    const cleanup = () => {
      window.removeEventListener("rt:presets", handler);
      window.clearTimeout(timer);
    };
    window.addEventListener("rt:presets", handler);
    client.send({ type: "list_presets", target });
  });
}

function targetsMatch(a: PresetTarget, b: PresetTarget): boolean {
  if (a.kind === "repo" && b.kind === "repo") return a.repo_id === b.repo_id;
  if (a.kind === "workspace" && b.kind === "workspace") {
    return a.workspace_id === b.workspace_id;
  }
  return false;
}

export interface LaunchPresetOpts {
  job_id?: string;
  target: PresetTarget;
  preset_id: string;
  source: LaunchPresetSource;
  variable_values: Array<[string, string]>;
  use_worktree_override: boolean | null;
  max_panes_per_tab_override: number | null;
}

/**
 * Fire-and-forget preset launch. Progress and completion stream back via
 * `rt:preset_launch_progress` / `rt:preset_launch_failed` window events
 * dispatched by App.tsx.
 */
export function launchPreset(client: DaemonClient, opts: LaunchPresetOpts) {
  const job_id = opts.job_id ?? newPresetLaunchJobId();
  client.send({ type: "launch_preset", ...opts, job_id });
  return job_id;
}

function newPresetLaunchJobId(): string {
  return `preset-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
}

export interface PreviewPresetOpts {
  target: PresetTarget;
  preset_id: string;
  source: LaunchPresetSource;
  variable_values: Array<[string, string]>;
}

export type PreviewPresetResult =
  | { ok: true; prompts: string[] }
  | { ok: false; error: string };

/**
 * Request the daemon to resolve a preset's prompt source into raw prompt
 * texts without spawning anything. Used by the launch dialog so file/folder
 * sources can show the prompt count before the user commits.
 *
 * Correlation: the caller passes a fresh request id; the daemon echoes it on
 * the response so the dialog can drop stale replies from rapid Back/Next.
 * Times out after 5 s with `{ ok: false, error: "timed out" }`.
 */
export function previewPreset(
  client: DaemonClient,
  opts: PreviewPresetOpts,
): Promise<PreviewPresetResult> {
  const id = `preview-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
  return new Promise((resolve) => {
    const onPreview = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "preset_preview" || detail.id !== id) return;
      cleanup();
      resolve({ ok: true, prompts: detail.prompts });
    };
    const onError = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "preset_preview_error" || detail.id !== id) return;
      cleanup();
      resolve({ ok: false, error: detail.error });
    };
    const timer = window.setTimeout(() => {
      cleanup();
      resolve({ ok: false, error: "preview timed out" });
    }, 5000);
    const cleanup = () => {
      window.removeEventListener("rt:preset_preview", onPreview);
      window.removeEventListener("rt:preset_preview_error", onError);
      window.clearTimeout(timer);
    };
    window.addEventListener("rt:preset_preview", onPreview);
    window.addEventListener("rt:preset_preview_error", onError);
    client.send({ type: "preview_preset", id, ...opts });
  });
}

/**
 * Request the remote URL for a repo via the daemon. Resolves on the first
 * `rt:remote_url` event whose `repo_id` matches, times out after 2 s if the
 * daemon never replies. Fixes the previous "remove listener on first event"
 * bug where a concurrent request for another repo would consume our handler.
 */
export function getRemoteUrl(
  client: DaemonClient,
  repoId: string,
): Promise<GitRemoteUrl | null> {
  return new Promise((resolve) => {
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "remote_url" || detail.repo_id !== repoId) return;
      cleanup();
      const { repo_id, raw_url, web_url, forge } = detail;
      resolve({ repo_id, raw_url, web_url, forge });
    };
    const timer = window.setTimeout(() => {
      cleanup();
      resolve(null);
    }, 2000);
    const cleanup = () => {
      window.removeEventListener("rt:remote_url", handler);
      window.clearTimeout(timer);
    };
    window.addEventListener("rt:remote_url", handler);
    client.send({ type: "get_remote_url", repo_id: repoId });
  });
}

/**
 * Open (or focus) a diff tab for a given (repo, path, against) triple.
 * Resolves with the tab id once the daemon replies, so callers can
 * activate the freshly opened tab. Times out after 4 s.
 */
export function openDiffTab(
  client: DaemonClient,
  args: { repoId: string; path: string; against: string | null },
): Promise<string | null> {
  return new Promise((resolve) => {
    const reqId = `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "diff_tab_opened" || detail.id !== reqId) return;
      cleanup();
      resolve(detail.tab_id);
    };
    const timer = window.setTimeout(() => {
      cleanup();
      resolve(null);
    }, 4000);
    const cleanup = () => {
      window.removeEventListener("rt:diff_tab_opened", handler);
      window.clearTimeout(timer);
    };
    window.addEventListener("rt:diff_tab_opened", handler);
    client.send({
      type: "open_diff_tab",
      id: reqId,
      repo_id: args.repoId,
      path: args.path,
      against: args.against,
    });
  });
}

export interface ResolvePresetScriptsOpts {
  target: PresetTarget;
  preset_id: string;
  variable_values: Array<[string, string]>;
}

export type ResolvePresetScriptsResult =
  | {
      ok: true;
      values: Array<[string, string]>;
      executed_commands: ScriptCommandPreview[];
    }
  | { ok: false; error: string; variable_name: string | null };

/**
 * Resolve every Script-kind variable in a preset by running its command on
 * the daemon. Returns the full variable map (user inputs + script outputs)
 * which the caller passes verbatim to `launchPreset`. The daemon never
 * auto-runs scripts as a side effect — this round-trip is the security gate.
 *
 * Times out after 90 s (longer than the daemon's per-script default of 60 s
 * so the daemon's own timeout wins on slow fetches).
 */
export function resolvePresetScripts(
  client: DaemonClient,
  opts: ResolvePresetScriptsOpts,
): Promise<ResolvePresetScriptsResult> {
  const id = `scripts-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
  return new Promise((resolve) => {
    const onResolved = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "preset_scripts_resolved" || detail.id !== id) return;
      cleanup();
      resolve({
        ok: true,
        values: detail.values,
        executed_commands: detail.executed_commands,
      });
    };
    const onError = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "preset_scripts_error" || detail.id !== id) return;
      cleanup();
      resolve({
        ok: false,
        error: detail.error,
        variable_name: detail.variable_name,
      });
    };
    const timer = window.setTimeout(() => {
      cleanup();
      resolve({
        ok: false,
        error: "script resolution timed out",
        variable_name: null,
      });
    }, 90_000);
    const cleanup = () => {
      window.removeEventListener("rt:preset_scripts_resolved", onResolved);
      window.removeEventListener("rt:preset_scripts_error", onError);
      window.clearTimeout(timer);
    };
    window.addEventListener("rt:preset_scripts_resolved", onResolved);
    window.addEventListener("rt:preset_scripts_error", onError);
    client.send({ type: "resolve_preset_scripts", id, ...opts });
  });
}

/**
 * Build a forge browse URL for `<base>` (the repo home, e.g.
 * `https://github.com/owner/repo`) at the given branch. Returns `null` when
 * forge is unrecognized so callers can fall back to the repo home.
 */
export function branchUrl(
  webUrl: string,
  forge: string,
  branch: string,
): string | null {
  const trimmed = webUrl.replace(/\/+$/, "");
  const encoded = encodeURIComponent(branch).replace(/%2F/g, "/");
  switch (forge) {
    case "github":
      return `${trimmed}/tree/${encoded}`;
    case "gitlab":
      return `${trimmed}/-/tree/${encoded}`;
    case "bitbucket":
      return `${trimmed}/src/${encoded}`;
    default:
      return null;
  }
}

/**
 * Request the current stash list for a repo and resolve with it. Times out
 * after 2 s. Subsequent `rt:stashes` broadcast events keep the sidebar in
 * sync without a fresh listStashes round-trip.
 */
export function listStashes(
  client: DaemonClient,
  repoId: string,
): Promise<GitStash[]> {
  return new Promise((resolve) => {
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "stashes" || detail.repo_id !== repoId) return;
      cleanup();
      resolve(detail.stashes);
    };
    const timer = window.setTimeout(() => {
      cleanup();
      resolve([]);
    }, 2000);
    const cleanup = () => {
      window.removeEventListener("rt:stashes", handler);
      window.clearTimeout(timer);
    };
    window.addEventListener("rt:stashes", handler);
    client.send({ type: "list_stashes", repo_id: repoId });
  });
}

export function bytesToBase64(bytes: Uint8Array): string {
  let bin = "";
  for (let i = 0; i < bytes.byteLength; i++) {
    bin += String.fromCharCode(bytes[i] ?? 0);
  }
  return btoa(bin);
}

export function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}
