import { browser } from "@wdio/globals";
import { setTimeout as delay } from "node:timers/promises";

import type { DaemonWsClient } from "./ws-client.js";
import type {
  ClientMessage,
  DaemonMessage,
  SessionMode,
  SessionSnapshot,
} from "./types.js";

/**
 * Per-agent spawn options, mirroring `protocol::AgentOptions`
 * (`#[serde(tag = "kind")]`). The variant tag doubles as the agent selector.
 * Plain-shell sessions ignore these fields but must still carry a valid
 * `agent_options` so `SpawnRequest` deserializes — default to Claude.
 */
export type AgentOptions =
  | { kind: "claude"; permission_mode: string | null }
  | { kind: "codex"; sandbox: string | null }
  | { kind: "cursor"; plan_mode: boolean; sandbox: string | null };

export const DEFAULT_AGENT_OPTIONS: AgentOptions = {
  kind: "claude",
  permission_mode: null,
};

export interface SpawnOptions {
  label: string;
  repoId: string;
  branchName?: string;
  baseBranch?: string | null;
  useWorktree?: boolean;
  mode?: SessionMode;
  agentOptions?: AgentOptions;
  initialPrompt?: string | null;
  dangerouslySkipPermissions?: boolean;
  model?: string | null;
  extraEnv?: [string, string][];
  promptInjector?: unknown | null;
  /** How long to wait for the matching `session_updated`. */
  timeoutMs?: number;
}

/**
 * Build the current `spawn_session` wire payload. Single source of truth for
 * the shape so a protocol change to `SpawnRequest` is a one-file edit here
 * rather than a hunt through every spec. Mirrors `protocol::SpawnRequest`:
 * per-agent options live under `agent_options` (tagged by `kind`), not the
 * old flat `agent` / `permission_mode` / `codex_sandbox` fields.
 */
export function buildSpawnMessage(options: SpawnOptions): ClientMessage {
  return {
    type: "spawn_session",
    label: options.label,
    target: {
      kind: "single",
      repo_id: options.repoId,
      branch_name: options.branchName ?? "main",
      base_branch: options.baseBranch ?? null,
      use_worktree: options.useWorktree ?? false,
    },
    mode: options.mode ?? "interactive",
    initial_prompt: options.initialPrompt ?? null,
    dangerously_skip_permissions: options.dangerouslySkipPermissions ?? false,
    agent_options: options.agentOptions ?? DEFAULT_AGENT_OPTIONS,
    model: options.model ?? null,
    extra_env: options.extraEnv ?? [],
    prompt_injector: options.promptInjector ?? null,
  };
}

export type SessionUpdatedMessage = DaemonMessage & {
  type: "session_updated";
  session: SessionSnapshot;
};

export function isSessionUpdated(
  msg: DaemonMessage,
): msg is SessionUpdatedMessage {
  if (msg.type !== "session_updated") return false;
  const session = (msg as { session?: unknown }).session;
  return typeof session === "object" && session !== null;
}

/**
 * Send a `spawn_session` and resolve with the spawned session's snapshot. Waits
 * for the first `session_updated` whose label (and mode) match what we asked
 * for, so concurrent sessions in the same spec don't cross the streams.
 */
export async function spawnSession(
  ws: DaemonWsClient,
  options: SpawnOptions,
): Promise<SessionSnapshot> {
  const mode = options.mode ?? "interactive";
  const spawned = ws.waitFor(
    (msg): msg is SessionUpdatedMessage =>
      isSessionUpdated(msg) &&
      msg.session.label === options.label &&
      msg.session.mode === mode,
    { timeoutMs: options.timeoutMs ?? 20_000 },
  );
  ws.send(buildSpawnMessage(options));
  return (await spawned).session;
}

/**
 * Dismiss the first-connect LayoutChooser modal. A fresh e2e client (new config
 * dir → unseen client_id) always receives `layout_init_required`, so the daemon
 * renders the mandatory chooser; its `.modal-backdrop` intercepts every
 * session-pane click until resolved. "Start empty" leaves the shared sessions
 * in the sidebar to open normally.
 *
 * @param waitMs When > 0, wait up to this long for the modal to appear (use in
 *   a spec `before` where it's guaranteed). When 0 (default), only dismiss it
 *   if it's already present — a cheap guard for later interactions.
 */
export async function dismissLayoutChooser(waitMs = 0): Promise<void> {
  const modal = await browser.$("[data-testid=layout-chooser]");
  const present =
    waitMs > 0
      ? await modal.waitForExist({ timeout: waitMs }).then(
          () => true,
          () => false,
        )
      : await modal.isExisting();
  if (!present) return;
  const startEmpty = await browser.$("[data-testid=layout-choose-empty]");
  await startEmpty.click();
  await modal.waitForExist({ timeout: 5_000, reverse: true });
  // Let the daemon's `tabs` reply settle before the caller touches panes.
  await delay(100);
}

/**
 * Open a session's pane in the UI and wait for it to mount. A freshly spawned
 * session is unbound (in no tab): a plain sidebar click is a no-op, and
 * double-click needs a pre-existing active tab (which "Start empty" leaves
 * none of). The session context menu's "Open in new tab" creates a tab + pane
 * unconditionally — the real recovery action — so we drive that.
 */
export async function openSessionPane(sessionId: string): Promise<void> {
  await dismissLayoutChooser();
  const paneSelector = `[data-testid=session-pane][data-session-id="${sessionId}"]`;
  if (await (await browser.$(paneSelector)).isExisting()) return;

  const row = await browser.$(
    `[data-testid=sidebar-session][data-session-id="${sessionId}"]`,
  );
  await row.waitForExist({ timeout: 10_000 });
  await row.click({ button: "right" });

  const openInNewTab = await browser.$(
    "[data-testid=session-context-open-in-new-tab]",
  );
  await openInNewTab.waitForExist({ timeout: 5_000 });
  await openInNewTab.click();

  await (await browser.$(paneSelector)).waitForExist({ timeout: 10_000 });
}
