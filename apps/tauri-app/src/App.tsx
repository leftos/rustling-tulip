import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";
import {
  connectDaemon,
  connectRemote,
  decodeConnectionCode,
  deleteRemoteProfile,
  disconnectRemote,
  ensureDaemonStarted,
  getClientIdentity,
  listRemoteProfiles,
  pairWithHost,
  pickDirectory,
  requestSpawnConfig,
  saveRemoteProfile,
  stopDaemon,
  type ClientIdentity,
  type ConnectionState,
  type ConnectionTarget,
  type DaemonClient,
  type DaemonHandshake,
  type DiscoveredHost,
  type RemoteProfile,
} from "./api";
import {
  DEFAULT_CLAUDE_OPTIONS,
  tabGrid,
  type BranchCleanup,
  type CheckoutChoice,
  type ClientMessage,
  type ClonableLayout,
  type ContainerRef,
  type DaemonMessage,
  type GridNode,
  type PresetEntry,
  type PresetLaunchJobSnapshot,
  type PresetTarget,
  type RepoEntry,
  type RootWorktreeEntry,
  type SessionSnapshot,
  type SpawnConfig,
  type SpawnTarget,
  type SplitDirection,
  type SplitPlace,
  type TabEntry,
  type VscodeWorkspaceSuggestion,
  type WorkspaceEntry,
  type WorktreeCleanupFailure,
  type WorktreeLaunchTarget,
} from "./types";
import ActivityBar, {
  type ActivitySection,
  readActivitySection,
  readSidebarCollapsed,
  writeActivitySection,
  writeSidebarCollapsed,
} from "./components/ActivityBar";
import Sidebar, {
  type LaunchLastPlacement,
  type SpawnInitialTarget,
} from "./components/Sidebar";
import type { DuplicateTarget } from "./components/SessionContextMenu";
import SourceControlSidebar from "./components/source-control/SourceControlSidebar";
import PaneWindow from "./components/PaneWindow";
import SessionWindow from "./components/SessionWindow";
import SpawnDialog, { type SpawnPlacement } from "./components/SpawnDialog";
import CheckoutConfirmModal from "./components/CheckoutConfirmModal";
import StandaloneShellDialog from "./components/StandaloneShellDialog";
import PresetLaunchDialog from "./components/PresetLaunchDialog";
import WorkspaceCreator from "./components/WorkspaceCreator";
import VscodeSuggestionToast from "./components/VscodeSuggestionToast";
import WorktreeCleanupFailedDialog from "./components/WorktreeCleanupFailedDialog";
import ActionFailedModal from "./components/ActionFailedModal";
import ErrorToast, { type ToastEntry } from "./components/ErrorToast";
import RepoRemoveDialog from "./components/RepoRemoveDialog";
import DeleteWorktreeDialog from "./components/DeleteWorktreeDialog";
import type { RepoRemoveIntent } from "./components/Sidebar";
import ResizableSplit from "./components/ResizableSplit";
import ExitConfirmDialog from "./components/ExitConfirmDialog";
import SettingsModal from "./components/SettingsModal";
import ConnectionPicker from "./components/ConnectionPicker";
import LayoutChooser, { type ArrangementConfig } from "./components/LayoutChooser";
import ImportArrangementModal from "./components/ImportArrangementModal";
import WorktreesManagerModal from "./components/WorktreesManagerModal";
import TabBar from "./components/TabBar";
import GridRenderer from "./components/GridRenderer";
import DiffPane from "./components/DiffPane";
import TabWindow from "./components/TabWindow";
import UndoShelf, { type UndoShelfEntry } from "./components/UndoShelf";
import { markForAutoFocus } from "./utils/autofocus";
import { logToFile } from "./utils/logger";
import {
  RemoteModeContext,
  REMOTE_UNAVAILABLE_EVENT,
  notifyRemoteUnavailable,
  type RemoteUnavailableDetail,
} from "./utils/remoteMode";
import {
  BRANCH_SUGGESTION_TIMEOUT_MS,
  matchesSuggestion,
  suggestTargetFor,
  suggestTargetKey,
} from "./utils/branchSuggestion";
import {
  balanceSplitDirection,
  collectPanes,
  findPaneBinding,
  findSplitSiblingSession,
  findTabContainingSession,
  paneRect,
  pickBalancedSplitTarget,
  resolveTabFocus,
} from "./utils/grid";
import { useKeyboardShortcuts, type KeyboardShortcut } from "./utils/a11y";
import {
  clearPanePoppedOut,
  prunePoppedOutPanes,
} from "./utils/poppedPanes";
import { loadSettings, saveSettings, useSettings } from "./utils/settings";
import {
  clearTabFontSize,
  getTabFontSize,
  MAX_FONT_SIZE,
  MIN_FONT_SIZE,
  pruneOverrides,
  resolveFontSize,
  setTabFontSize,
} from "./utils/fontSize";
import { resolveAppearance, resolveAppearanceLayers } from "./utils/appearance";
import { computeWindowTitle } from "./utils/windowTitle";
import {
  exitProgress,
  recordExitChoice,
  skipVanished,
  startExitQueue,
  type ExitWorktreeQueue,
} from "./utils/exitWorktreeQueue";

/// Pop-out windows: launched with either `?pane=<id>` (pane-backed pop-out),
/// `?tab=<id>` (per-tab pop-out), or `?session=<id>` (legacy single-session
/// pop-out). The branch in App renders the corresponding standalone window
/// surface without sidebar/modals.
const queryParams = new URLSearchParams(window.location.search);
const popoutPaneId = queryParams.get("pane");
const popoutTabId = queryParams.get("tab");
const popoutSessionId = queryParams.get("session");

const ACTIVE_TAB_KEY = "rt:active-tab:main";
const CONNECTION_TARGET_KEY = "rt:connection-target";
const UNDO_TTL_MS = 8000;
const UNDO_LIMIT = 3;

/// Read the persisted connection target. Defaults to `local` (the bundled
/// per-machine daemon) so a fresh install — and every desktop launch — boots
/// straight into the local daemon with no extra friction.
function loadConnectionTarget(): ConnectionTarget {
  try {
    const raw = localStorage.getItem(CONNECTION_TARGET_KEY);
    if (!raw) return { kind: "local" };
    const parsed = JSON.parse(raw) as unknown;
    if (
      typeof parsed === "object" &&
      parsed !== null &&
      (parsed as { kind?: unknown }).kind === "remote" &&
      typeof (parsed as { profileId?: unknown }).profileId === "string"
    ) {
      return {
        kind: "remote",
        profileId: (parsed as { profileId: string }).profileId,
      };
    }
    return { kind: "local" };
  } catch {
    return { kind: "local" };
  }
}

function saveConnectionTarget(target: ConnectionTarget): void {
  try {
    localStorage.setItem(CONNECTION_TARGET_KEY, JSON.stringify(target));
  } catch {
    /* localStorage unavailable — best-effort, drop silently */
  }
}

type UndoTabSnapshot = {
  tab: TabEntry;
  index: number;
  restoreActive: boolean;
  restoreFocusedPaneId: string | null;
};

type UndoEntry = UndoShelfEntry & {
  kind: "restore_tabs";
  tabs: UndoTabSnapshot[];
};

interface AppState {
  client: DaemonClient | null;
  status: ConnectionState | { kind: "init" } | { kind: "error"; reason: string };
  repos: RepoEntry[];
  workspaces: WorkspaceEntry[];
  sessions: SessionSnapshot[];
  tabs: TabEntry[];
  activeTabId: string | null;
  focusedPaneId: string | null;
  /// Per-tab last-focused pane id. When activating a tab, the resolver
  /// restores the entry here (if the pane still exists in the grid) so
  /// switching tabs preserves focus instead of dropping to "no active
  /// pane". Entries are pruned when their tab is removed or when the
  /// referenced pane is no longer in the tab's grid.
  focusedPaneByTab: Record<string, string>;
  spawnOpen: boolean;
  standaloneShellOpen: boolean;
  standaloneShellInitial: string | null;
  spawnInitial: SpawnInitialTarget | undefined;
  /// When true, `spawnInitial` locks the dialog's target picker (sidebar
  /// context menu, launch-last, duplicate-session). When false, the
  /// target is only pre-selected and the user can still pick a
  /// different repo/workspace (empty-pane "spawn here" inferred from a
  /// split sibling). Irrelevant when `spawnInitial` is undefined.
  spawnInitialLocked: boolean;
  /// Full SpawnConfig to seed the spawn dialog from (used by "Duplicate
  /// session → Shift-click → open dialog pre-filled"). When set, the
  /// dialog hydrates agent, run mode, trusted-launch state, model, permission mode,
  /// codex sandbox, and extra env vars from this config — overriding the
  /// usual Settings defaults. `undefined` for normal spawns.
  spawnPrefill: SpawnConfig | undefined;
  /// Set when the daemon asks to confirm an in-place checkout into a dirty
  /// tree (`checkout_confirm_required`); drives the CheckoutConfirmModal. The
  /// confirmed choice resends the stashed spawn request with a strategy.
  checkoutConfirm: { repo_id: string; branch: string; dirty_count: number } | null;
  /// Counter incremented every time we arm a "next new tab becomes
  /// active" intent (e.g. user spawns a session, merges tabs, extracts a
  /// pane to a new tab). Each subsequent `tab_updated` for an unseen tab
  /// id consumes one (decrements). Was a boolean previously, which lost
  /// arms when two spawns raced — only the first new tab got activated.
  pendingTabActivate: number;
  workspaceCreatorOpen: boolean;
  vscodeQueue: VscodeWorkspaceSuggestion[];
  attentionSessions: Set<string>;
  exitConfirmOpen: boolean;
  exitInFlight: boolean;
  /// Open while "Stop sessions, remove worktrees" walks the worktree
  /// sessions one at a time for a branch fate. Null when no walk is in
  /// progress; the shutdown only goes out once the walk is finished.
  exitWorktreeQueue: ExitWorktreeQueue | null;
  /// True once `onStopAndQuit` has sent `shutdown` to the daemon and 2 s
  /// have elapsed without the WS connection closing. Surfaces a warning
  /// banner + Force-quit button in the exit dialog so the user isn't
  /// trapped staring at "Stopping…" forever when the daemon hangs.
  exitStuck: boolean;
  settingsOpen: boolean;
  presetLaunch: { preset: PresetEntry; target: PresetTarget } | null;
  spotlightSessionIds: Set<string>;
  toasts: ToastEntry[];
  undoEntries: UndoEntry[];
  /// Repo-remove modal state, set when the sidebar's × click hit a repo
  /// with at least one non-stopped session. `null` when no modal is open.
  repoRemove: RepoRemoveIntent | null;
  /// Per-repo count of uncommitted files (staged + worktree, as reported by
  /// the daemon's `repo_status`). Drives the badge on the source-control
  /// activity-bar button. Populated lazily from `repo_status` broadcasts
  /// (filesystem watcher) plus an initial request fired when a repo first
  /// appears in `repos`.
  repoChangeCounts: Record<string, number>;
  /// Latest handshake from `ensureDaemonStarted`. Surfaced into the
  /// DaemonFooter flyout so users can see the daemon's port / pid /
  /// protocol-version without leaving the app. `null` before the first
  /// supervisor call succeeds, and after the user hits "Stop daemon".
  handshake: DaemonHandshake | null;
  /// True iff the user explicitly hit the DaemonFooter's Stop button.
  /// Suppresses the auto-reconnect cycle below so we don't immediately
  /// respawn the daemon they just stopped. Cleared by Restart.
  daemonStopRequested: boolean;
  /// Manual sidebar-container order (workspaces + repos as one flat
  /// list). Empty array means "no manual order; fall back to
  /// alphabetical". Synced from the daemon's `containers_reordered`
  /// broadcasts.
  containerOrder: ContainerRef[];
  /// Per-container session display order. Key is workspace id, repo id,
  /// or tab id. Synced from `sessions_reordered` broadcasts; also
  /// updated optimistically on drag so the drop position sticks.
  sessionOrder: Map<string, string[]>;
  /// Active worktrees root path + whether it came from the user setting
  /// (true) or the env/platform default (false). Populated from the
  /// daemon's `worktrees_root_changed` broadcast — first delivery
  /// arrives inside the initial-state push after Welcome, then on
  /// every `set_worktrees_root` mutation. `null` only until that first
  /// broadcast lands.
  worktreesRoot: { path: string; isOverride: boolean } | null;
  /// Opt-in LAN access status, mirrored from the daemon's `lan_status`
  /// broadcast (first delivery in the initial-state push after Welcome, then
  /// on every `configure_lan`). `null` only until that first broadcast lands.
  /// `fingerprint` is null until LAN has been enabled at least once.
  lanStatus: {
    enabled: boolean;
    port: number;
    fingerprint: string | null;
    addresses: string[];
  } | null;
  /// The local daemon's auth token, captured from the handshake on connect.
  /// Used by the Remote access settings panel to assemble the connection code
  /// the host hands to a remote client. `null` until the first connect.
  daemonToken: string | null;
  /// Which daemon this window currently talks to. `local` is the bundled
  /// per-machine daemon; `remote` reaches a saved host through the pinned-TLS
  /// loopback bridge. Persisted client-side; drives the `connect()` branch and
  /// the remote-mode UI degradation.
  connectionTarget: ConnectionTarget;
  /// Saved remote hosts, loaded from `remote-profiles.json` via the Tauri
  /// side. Powers the connection picker and resolves the active remote target.
  remoteProfiles: RemoteProfile[];
  /// True while the connection picker modal (DaemonFooter → Connections) is
  /// open.
  connectionPickerOpen: boolean;
  /// First-connect layout chooser payload, set on `layout_init_required` (a
  /// new client_id the daemon has never seen). The LayoutChooser modal renders
  /// it; cleared when the daemon answers with `tabs`. `null` otherwise.
  layoutChooser: {
    has_legacy: boolean;
    active_session_count: number;
    clonable: ClonableLayout[];
  } | null;
  /// Set when a remote `sessions` snapshot arrives with a count that differs
  /// from what the client previously knew. The ImportArrangementModal renders
  /// it; cleared on any user choice (keep / import / empty). `null` otherwise.
  importArrangement: {
    incoming_count: number;
    local_count: number;
  } | null;
  /// True while the worktrees-management modal is open. Triggered from
  /// SettingsModal's "Manage worktrees…" button.
  worktreesManagerOpen: boolean;
  /// Worktree group the spawn dialog is pinned to, set by "Launch session
  /// here" in the worktrees manager. Kept separate from `spawnPrefill`:
  /// prefill replays a past config's agent/mode/model, whereas a pin only
  /// fixes *where* the session runs and leaves every other field on its
  /// normal default.
  spawnPin: WorktreeLaunchTarget | undefined;
  /// True once the WS has reached `kind: "open"` at least once this app
  /// lifetime. Drives the "Connecting to daemon…" overlay that blocks
  /// the entire UI on cold boot — once the daemon has answered once,
  /// transient drops use the normal in-app reconnect UX instead. Reset
  /// only when the user explicitly stops the daemon (DaemonFooter Stop).
  hasEverConnected: boolean;
  /// Queue of `WorktreeCleanupFailed` events. The
  /// WorktreeCleanupFailedDialog renders the head entry; user actions
  /// (kill & retry, ignore) dequeue. A second failure for the same
  /// session id replaces the queued entry — that's the retry result.
  worktreeCleanupQueue: WorktreeCleanupQueueEntry[];
  /// Head `ActionFailed` event — a refused/failed action (worktree-in-use
  /// spawn, blocked discard) shown as a blocking modal. `null` when none is
  /// pending; a newer event replaces an unacknowledged one.
  actionFailed: ActionFailedInfo | null;
  /// Pending "delete this session's worktree" confirmation, set by the
  /// `rt:request_delete_worktree` event. The DeleteWorktreeDialog renders it
  /// and App performs the sends on confirm. `null` when none is pending.
  deleteWorktreeRequest: DeleteWorktreeRequest | null;
}

/// Payload of the `rt:request_delete_worktree` window event, which is the
/// only way a worktree gets deleted: every entry point dispatches it instead
/// of sending `discard_session` itself. `closePane` is set when the gesture
/// also removes a pane from a tab (the pane-close dialog); the dispatcher is
/// responsible for any undo snapshot it wants taken first.
interface DeleteWorktreeRequest {
  sessionId: string;
  closePane: { tabId: string; paneId: string } | null;
}

interface WorktreeCleanupQueueEntry {
  session_id: string;
  session_label: string;
  failures: WorktreeCleanupFailure[];
}

interface ActionFailedInfo {
  title: string;
  detail: string;
  hint: string | null;
}

function lastSpawnConfigForTarget(
  state: AppState,
  target: SpawnInitialTarget,
): SpawnConfig | null {
  return target.kind === "repo"
    ? (state.repos.find((r) => r.id === target.repo_id)?.last_spawn_config ??
        null)
    : (state.workspaces.find((w) => w.id === target.workspace_id)
        ?.last_spawn_config ?? null);
}

/// Derive a SpawnInitialTarget from a session snapshot — workspace sessions
/// resolve to their workspace, single-repo sessions to the first member's
/// repo. Returns null for standalone sessions (no repo/workspace anchor).
function spawnTargetFromSession(
  session: SessionSnapshot,
): SpawnInitialTarget | null {
  if (session.workspace_id) {
    return { kind: "workspace", workspace_id: session.workspace_id };
  }
  const member = session.members[0];
  if (member) {
    return { kind: "repo", repo_id: member.repo_id };
  }
  return null;
}

/// Most recently spawned session's repo/workspace, used as the fallback
/// preselect for an empty pane's "spawn here" when no sibling pane carries
/// a session (e.g. the pane became empty because the user stopped its
/// session). Walks `sessions` ordered by `started_at` descending; skips
/// standalone sessions, which have no anchor.
function lastSpawnedTarget(
  sessions: SessionSnapshot[],
): SpawnInitialTarget | null {
  const sorted = [...sessions].sort((a, b) =>
    b.started_at.localeCompare(a.started_at),
  );
  for (const s of sorted) {
    const target = spawnTargetFromSession(s);
    if (target) return target;
  }
  return null;
}

export default function App() {
  const [state, setState] = useState<AppState>({
    client: null,
    status: { kind: "init" },
    repos: [],
    workspaces: [],
    sessions: [],
    tabs: [],
    activeTabId: loadActiveTab(),
    focusedPaneId: null,
    focusedPaneByTab: {},
    spawnOpen: false,
    standaloneShellOpen: false,
    standaloneShellInitial: null,
    spawnInitial: undefined,
    spawnInitialLocked: true,
    spawnPrefill: undefined,
    checkoutConfirm: null,
    pendingTabActivate: 0,
    workspaceCreatorOpen: false,
    vscodeQueue: [],
    attentionSessions: new Set(),
    exitConfirmOpen: false,
    exitInFlight: false,
    exitWorktreeQueue: null,
    exitStuck: false,
    settingsOpen: false,
    presetLaunch: null,
    spotlightSessionIds: new Set<string>(),
    toasts: [],
    undoEntries: [],
    repoRemove: null,
    repoChangeCounts: {},
    handshake: null,
    daemonStopRequested: false,
    containerOrder: [],
    sessionOrder: new Map(),
    worktreesRoot: null,
    lanStatus: null,
    daemonToken: null,
    connectionTarget: loadConnectionTarget(),
    remoteProfiles: [],
    connectionPickerOpen: false,
    layoutChooser: null,
    importArrangement: null,
    worktreesManagerOpen: false,
    spawnPin: undefined,
    hasEverConnected: false,
    worktreeCleanupQueue: [],
    actionFailed: null,
    deleteWorktreeRequest: null,
  });
  // Bumped to force the connection useEffect to re-run (e.g. when the
  // user clicks Restart from the DaemonFooter while the daemon is in
  // the "stopped" state and the auto-reconnect cycle has been suppressed).
  const [connectionVersion, setConnectionVersion] = useState(0);
  // Mirror of `state.daemonStopRequested` for use inside the connection
  // effect's closure (which captures values at mount time).
  const stopRequestedRef = useRef(false);
  stopRequestedRef.current = state.daemonStopRequested;
  // Mirror of `state.connectionTarget` so the connection effect's `connect()`
  // closure always reads the current target (it captures values at mount and
  // re-runs only when `connectionVersion` bumps).
  const connectionTargetRef = useRef<ConnectionTarget>(state.connectionTarget);
  connectionTargetRef.current = state.connectionTarget;
  // Per-install client identity (for per-client tab layouts), fetched once and
  // cached. A window and its pop-outs read the same persisted id, so they
  // share one layout.
  const clientIdentityRef = useRef<ClientIdentity | null>(null);
  // Latch true once this client has seen a non-empty tab / session snapshot.
  // The pop-out close effects use these to tell the initial pre-hydration
  // empty state (don't close) apart from a genuine post-hydration empty state
  // after the last tab/session was removed (do close, instead of orphaning
  // the window on a "Pane not found" placeholder).
  const tabsHydratedRef = useRef(false);
  const sessionsHydratedRef = useRef(false);
  // The last spawn_session request sent, stashed so a `checkout_confirm_required`
  // reply can resend it verbatim with a chosen `checkout_strategy`.
  const lastSpawnReqRef = useRef<Extract<
    ClientMessage,
    { type: "spawn_session" }
  > | null>(null);

  // App-wide user preferences (localStorage-backed). `useSettings` returns
  // a live tuple — any other component that calls `useSettings` will
  // re-render on writes from anywhere (modal, migration), so we don't
  // prop-drill the settings object.
  const [settings] = useSettings();
  const settingsRef = useRef(settings);
  settingsRef.current = settings;

  // PTY output is high-volume — keep it out of React state.
  const ptyListenersRef = useRef(
    new Map<string, Set<(b64: string) => void>>(),
  );

  // Held in a ref so handleMessage's `client.send(...)` can dispatch derived
  // messages (create_tab after a session spawns) without needing to thread
  // state through every closure.
  const clientRef = useRef<DaemonClient | null>(null);

  // Side-effect decisions live in refs, not state. Reasons:
  //   - State updater functions must stay pure (StrictMode double-invokes them
  //     in dev to surface side effects), so we cannot `client.send(...)` from
  //     inside one without double-firing the message.
  //   - The daemon emits each `session_updated` twice for a new session (once
  //     from the registry broadcast, once from the spawn-dispatch direct send),
  //     and click handlers may fire faster than React can commit a render. A
  //     synchronous ref guarantees one-shot semantics across those races.
  const seenSessionIdsRef = useRef(new Set<string>());
  /// Last-processed status per session id, maintained synchronously by the
  /// message handlers. The self-exit detector needs the status of the
  /// PREVIOUS `session_updated` — `latestStateRef` reflects the last
  /// committed render, so when the daemon's two stop snapshots (explicit
  /// stop without exit code, then the exit watcher's with one) arrive
  /// before React re-renders, both would compare against the stale
  /// "working" status and the second would masquerade as a self-exit,
  /// auto-discarding a session the user only asked to stop.
  const sessionStatusRef = useRef(new Map<string, SessionSnapshot["status"]>());
  /// One-shot follow-up for the next freshly-spawned session id.
  ///   - `newTab`     : open a fresh tab containing the session.
  ///   - `replacePane`: drop the session into a specific empty pane (set when
  ///                    the user opened the spawn dialog from that pane's
  ///                    "+ spawn" button).
  ///   - `addToTab`   : add the session to an existing tab.
  /// `discardSessionId` optionally names a stopped predecessor to discard
  /// AFTER the placement sends — ordering the discard behind the bind is what
  /// keeps the daemon's `close_session_panes` from collapsing the very pane
  /// the new session is about to take over (Restart / spawn-into-pane flows).
  /// Consumed by the `session_updated` handler the first time it sees an
  /// unseen session id.
  const pendingSpawnIntentRef = useRef<PendingSpawnIntent>(null);

  /// Launch-last replays waiting on the daemon to name their branch, keyed by
  /// `suggestTargetKey` so a reply routes back to the gesture that asked.
  const pendingLaunchLastRef = useRef<Map<string, PendingLaunchLast>>(
    new Map(),
  );

  /// Set when the user picks an arrangement in LayoutChooser or
  /// ImportArrangementModal before the daemon replies with `tabs`. The `tabs`
  /// handler reads it, clears it, then sends `rearrange_tab` /
  /// `extract_to_new_tab` to apply the chosen shape.
  const pendingArrangementRef = useRef<ArrangementConfig | null>(null);

  /// Stashes the session-count mismatch detected in the `sessions` handler.
  /// Cleared by the `layout_init_required` handler (LayoutChooser takes over)
  /// or by the `tabs` handler (which promotes it to `importArrangement` state).
  const pendingSessionImportRef = useRef<{
    incoming_count: number;
    local_count: number;
  } | null>(null);

  /// Captured when the spawn dialog is opened from an empty pane's "+ spawn"
  /// button or a stopped pane's "New session" button. Read at `onSpawned` to
  /// route current-tab placement into that pane. `discardSessionId` carries
  /// the stopped session occupying the pane, discarded once the new session
  /// takes the slot (see PendingSpawnIntent). Cleared on dialog close so a
  /// manual reopen from the toolbar doesn't inherit a stale target.
  const spawnTargetPaneRef = useRef<{
    tabId: string;
    paneId: string;
    discardSessionId?: string;
  } | null>(null);

  /// Captured when the spawn dialog is opened from a tab container's
  /// right-click "Spawn session here" entry. Read at `onSpawned` to
  /// promote the pending intent to `addToTab`, which appends a new pane
  /// bound to the spawned session to the target tab (or fills an empty
  /// pane if one exists). Cleared on dialog close.
  const spawnTargetTabRef = useRef<string | null>(null);

  /// Captured when a pane sends `split_pane`. Records the tab id + the
  /// pane ids that existed before the split. The next `tab_updated` for
  /// the same tab diffs against this set and focuses whichever pane id
  /// wasn't there before. Cleared after the first matching update or
  /// (defensively) by a later tab/session removal that supersedes the
  /// intent.
  const pendingPaneFocusRef = useRef<
    { tabId: string; knownPaneIds: Set<string> } | null
  >(null);

  // Snapshot of the latest committed state, for click handlers that need to
  // read state and dispatch side effects without putting the send inside a
  // state updater.
  const latestStateRef = useRef<AppState | null>(null);
  latestStateRef.current = state;

  useEffect(() => {
    void (async () => {
      const prefs = loadSettings().notifications;
      if (!prefs.awaiting_input && !prefs.stopped && !prefs.error) {
        return;
      }
      const granted = await isPermissionGranted();
      if (granted) return;
      const requested = await requestPermission();
      // Tauri returns "granted" | "denied" | "default". If the user denied,
      // attention notifications won't fire — log it once at startup so the
      // dev/app.log has a paper trail explaining the silence.
      if (requested !== "granted") {
        logToFile(
          "warn",
          `notification permission state: ${requested} - attention notifications will be silent`,
        );
      }
    })();
  }, []);

  useEffect(() => {
    let cancelled = false;
    let client: DaemonClient | null = null;
    let reconnectTimer: number | null = null;
    let reconnectAttempt = 0;

    /// Resolve a handshake for the current connection target. Local goes
    /// through the daemon supervisor; remote opens the pinned-TLS loopback
    /// bridge to a saved host. A remote target whose profile has vanished
    /// (deleted, or a fresh install with a stale persisted id) self-heals back
    /// to local so the reconnect loop doesn't spin forever on a missing host.
    const resolveHandshake = async (): Promise<DaemonHandshake> => {
      const target = connectionTargetRef.current;
      if (target.kind !== "remote") {
        return await ensureDaemonStarted();
      }
      const profiles = await listRemoteProfiles().catch(
        () => [] as RemoteProfile[],
      );
      const profile = profiles.find((p) => p.id === target.profileId);
      if (!profile) {
        logToFile(
          "warn",
          `remote profile ${target.profileId} not found; falling back to local`,
        );
        saveConnectionTarget({ kind: "local" });
        if (!cancelled) {
          setState((s) => ({
            ...s,
            connectionTarget: { kind: "local" },
            remoteProfiles: profiles,
          }));
        }
        return await ensureDaemonStarted();
      }
      if (!cancelled) {
        setState((s) => ({ ...s, remoteProfiles: profiles }));
      }
      return await connectRemote({
        host: profile.host,
        port: profile.port,
        token: profile.token,
        fingerprint: profile.fingerprint,
      });
    };

    /// Connect (or reconnect) and wire all subscriptions. Called on mount
    /// and from the close handler with exponential backoff.
    const connect = async () => {
      try {
        const handshake = await resolveHandshake();
        if (cancelled) return;
        // Resolve the per-install identity once (cached) before the first
        // connect so the Hello carries client_id for per-client layouts.
        if (clientIdentityRef.current === null) {
          clientIdentityRef.current = await getClientIdentity().catch(
            (err: unknown) => {
              logToFile("warn", `getClientIdentity failed: ${String(err)}`);
              return null;
            },
          );
          if (cancelled) return;
        }
        setState((s) => ({ ...s, daemonToken: handshake.auth_token }));
        client = connectDaemon(handshake, clientIdentityRef.current);
        clientRef.current = client;
        client.onConnectionChange((next) => {
          setState((s) => ({
            ...s,
            status: next,
            hasEverConnected: s.hasEverConnected || next.kind === "open",
          }));
          // Schedule a reconnect when the socket drops unexpectedly.
          // auth_failed is terminal (config issue, retrying won't help).
          // Reset the backoff counter when we successfully reach `open`.
          if (next.kind === "open") {
            reconnectAttempt = 0;
            logToFile("info", "daemon websocket connected");
          } else if (next.kind === "closed" && !cancelled) {
            // Honor the user's explicit Stop request from the DaemonFooter
            // — don't immediately respawn the daemon they just killed.
            // Cleared by clicking Restart, which bumps connectionVersion.
            if (stopRequestedRef.current) {
              logToFile(
                "info",
                "daemon websocket closed but stop was user-requested; not reconnecting",
              );
              return;
            }
            const delay = Math.min(10_000, 500 * 2 ** reconnectAttempt);
            reconnectAttempt += 1;
            logToFile(
              "warn",
              `daemon websocket closed (${next.reason}); reconnecting in ${delay}ms (attempt ${reconnectAttempt})`,
            );
            reconnectTimer = window.setTimeout(() => {
              reconnectTimer = null;
              if (!cancelled) void connect();
            }, delay);
          }
        });
        client.onMessage((msg) =>
          handleMessage(
            msg,
            setState,
            ptyListenersRef.current,
            clientRef,
            seenSessionIdsRef,
            sessionStatusRef,
            pendingSpawnIntentRef,
            pendingArrangementRef,
            pendingSessionImportRef,
            pendingPaneFocusRef,
            latestStateRef,
          ),
        );
        // Dev-only: expose the daemon client on window for e2e specs that
        // need to send messages through the React app's WS (so server
        // replies arrive at this client's onMessage handler). Mirrors the
        // `__rt_terms` / `__rt_console` pattern. Tree-shaken in prod.
        if (import.meta.env.DEV) {
          (globalThis as unknown as { __rt_daemon_client?: DaemonClient }).__rt_daemon_client = client;
        }
        setState((s) => ({ ...s, client, handshake }));
      } catch (err) {
        // Handshake resolution failed: local → supervisor can't spawn the
        // daemon; remote → host unreachable or fingerprint mismatch. Record
        // the error AND schedule a retry (the chosen "stay + retry" behavior
        // for an offline remote host — the user can switch targets from the
        // connection picker). Exception: when the user just hit Stop, don't
        // retry — they asked for the daemon to be down.
        const reason = String(err);
        const targetKind = connectionTargetRef.current.kind;
        setState((s) => ({ ...s, status: { kind: "error", reason } }));
        if (!cancelled && !stopRequestedRef.current) {
          const delay = Math.min(10_000, 500 * 2 ** reconnectAttempt);
          reconnectAttempt += 1;
          logToFile(
            "warn",
            `${targetKind} connect failed (${reason}); retrying in ${delay}ms (attempt ${reconnectAttempt})`,
          );
          reconnectTimer = window.setTimeout(() => {
            reconnectTimer = null;
            if (!cancelled) void connect();
          }, delay);
        }
      }
    };

    void connect();

    return () => {
      cancelled = true;
      if (reconnectTimer !== null) window.clearTimeout(reconnectTimer);
      client?.close();
      clientRef.current = null;
    };
    // `connectionVersion` is bumped by the DaemonFooter's Restart action
    // to force this effect to tear down + reconnect from scratch — the
    // simplest way to recover from a user-Stopped state without
    // refactoring the entire connection lifecycle.
  }, [connectionVersion]);

  // Standby-resume watchdog. A suspended machine freezes JS timers, so a
  // pending short interval firing far later than scheduled is a reliable
  // "the system just resumed" signal. Two jobs:
  //   1. Log the resume (with the connection state at that moment) so
  //      app.log anchors wake incidents without consulting the OS event log
  //      — the 8/13 session-loss investigation had no such anchor.
  //   2. Probe a nominally-open WS. Standby can leave the socket half-open:
  //      TCP is gone but no FIN/RST ever arrives, so `close` never fires,
  //      the reconnect loop never runs, and the UI sits on stale sessions
  //      with dead terminals. Any inbound message within the probe window
  //      proves liveness (the list_repos reply, or any broadcast); silence
  //      means half-open, and an explicit close() hands the socket to the
  //      existing reconnect cycle.
  useEffect(() => {
    const TICK_MS = 30_000;
    const JUMP_THRESHOLD_MS = 90_000;
    const PROBE_TIMEOUT_MS = 3_000;
    let last = Date.now();
    const timer = window.setInterval(() => {
      const now = Date.now();
      const gap = now - last;
      last = now;
      if (gap < JUMP_THRESHOLD_MS) return;
      const client = clientRef.current;
      const status = client?.state().kind ?? "no-client";
      logToFile(
        "info",
        `system resume detected: ${TICK_MS / 1000}s tick fired after ${Math.round(gap / 1000)}s; connection=${status}`,
      );
      if (!client || client.state().kind !== "open") return;
      let alive = false;
      const unsubscribe = client.onMessage(() => {
        alive = true;
      });
      client.send({ type: "list_repos" });
      window.setTimeout(() => {
        unsubscribe();
        if (client !== clientRef.current) return;
        if (alive) {
          logToFile("info", "post-resume liveness probe: daemon replied");
        } else {
          logToFile(
            "warn",
            "post-resume liveness probe: no traffic within " +
              `${PROBE_TIMEOUT_MS}ms on an open socket; forcing reconnect`,
          );
          client.close();
        }
      }, PROBE_TIMEOUT_MS);
    }, TICK_MS);
    return () => window.clearInterval(timer);
  }, []);

  useEffect(() => {
    if (state.activeTabId) {
      localStorage.setItem(ACTIVE_TAB_KEY, state.activeTabId);
    } else {
      localStorage.removeItem(ACTIVE_TAB_KEY);
    }
  }, [state.activeTabId]);

  useEffect(() => {
    if (state.undoEntries.length === 0) {
      return;
    }
    const nextExpiry = Math.min(...state.undoEntries.map((entry) => entry.expiresAt));
    const delayMs = Math.max(0, nextExpiry - Date.now()) + 50;
    const timer = window.setTimeout(() => {
      const now = Date.now();
      setState((s) => ({
        ...s,
        undoEntries: s.undoEntries.filter((entry) => entry.expiresAt > now),
      }));
    }, delayMs);
    return () => window.clearTimeout(timer);
  }, [state.undoEntries]);

  const onAddRepo = useCallback(async () => {
    // Local-FS action: the picker browses *this* machine, but the daemon
    // validates repo paths on its own filesystem. Meaningless over a remote
    // connection — gate it (the UI also disables the affordance).
    if (latestStateRef.current?.connectionTarget.kind === "remote") {
      notifyRemoteUnavailable("Add repository");
      return;
    }
    const path = await pickDirectory(undefined, { lastDirKey: "addRepo" });
    if (!path) return;
    state.client?.send({ type: "add_repo", path, name: null });
  }, [state.client]);

  const onAddRepoPath = useCallback(
    (path: string) => {
      if (latestStateRef.current?.connectionTarget.kind === "remote") {
        notifyRemoteUnavailable("Add repository");
        return;
      }
      state.client?.send({ type: "add_repo", path, name: null });
    },
    [state.client],
  );

  const onActivateTab = useCallback((tabId: string) => {
    setState((s) => {
      const tab = s.tabs.find((t) => t.id === tabId);
      const focusedPaneId = resolveTabFocus(tab, s.focusedPaneByTab[tabId]);
      return { ...s, activeTabId: tabId, focusedPaneId };
    });
  }, []);

  const onFocusPane = useCallback((paneId: string) => {
    setState((s) => {
      if (s.focusedPaneId === paneId) return s;
      const next: AppState = { ...s, focusedPaneId: paneId };
      if (s.activeTabId) {
        next.focusedPaneByTab = {
          ...s.focusedPaneByTab,
          [s.activeTabId]: paneId,
        };
      }
      return next;
    });
  }, []);

  /// Arm the App-level pending-activate-next-new-tab flag. Tab merges and
  /// pane extracts call this right before sending the daemon message so
  /// the subsequent `tab_updated` broadcast switches focus to the new tab.
  const onArmNextNewTab = useCallback(() => {
    setState((s) => ({ ...s, pendingTabActivate: s.pendingTabActivate + 1 }));
  }, []);

  /// Capture the pre-split pane id set so the next tab_updated for the
  /// same tab can diff and focus the fresh pane. See pendingPaneFocusRef
  /// for the consumer.
  const onArmFocusNewPane = useCallback(
    (tabId: string, knownPaneIds: Set<string>) => {
      pendingPaneFocusRef.current = { tabId, knownPaneIds };
    },
    [],
  );

  /// Apply a tab-reorder optimistically. Called by TabBar's drag-drop
  /// handler right before sending `reorder_tabs` so the dropped pill
  /// doesn't snap back to its old position while waiting for the
  /// daemon's broadcast. Filters out unknown ids defensively in case
  /// the order list was built from stale prop state.
  const onLocalReorder = useCallback((orderedIds: string[]) => {
    setState((s) => {
      const byId = new Map(s.tabs.map((t) => [t.id, t] as const));
      const next: TabEntry[] = [];
      const seen = new Set<string>();
      for (const id of orderedIds) {
        const tab = byId.get(id);
        if (tab && !seen.has(id)) {
          next.push(tab);
          seen.add(id);
        }
      }
      // Append any tabs the caller forgot (defensive — keeps state
      // total even if the optimistic order was stale).
      for (const tab of s.tabs) {
        if (!seen.has(tab.id)) next.push(tab);
      }
      return { ...s, tabs: next };
    });
  }, []);

  /// Sibling of `onLocalReorder` for the sidebar's container drag-and-
  /// drop. Applied wholesale to `containerOrder` so the drop position
  /// sticks while the daemon's `containers_reordered` broadcast is in
  /// flight. The daemon-confirmed order replaces this on arrival.
  const onLocalReorderContainers = useCallback(
    (ordered: ContainerRef[]) => {
      setState((s) => ({ ...s, containerOrder: ordered }));
    },
    [],
  );

  /// Optimistic session-list reorder: applied immediately so the drop
  /// position sticks while the daemon's `sessions_reordered` broadcast
  /// is in flight.
  const onLocalReorderSessions = useCallback(
    (containerId: string, orderedIds: string[]) => {
      setState((s) => {
        const next = new Map(s.sessionOrder);
        next.set(containerId, orderedIds);
        return { ...s, sessionOrder: next };
      });
    },
    [],
  );

  const onSelectSession = useCallback((sessionId: string) => {
    // Decide the side-effect from the latest committed state (read via ref so
    // it stays current), THEN dispatch a pure state update. Doing it the other
    // way around would put `client.send(...)` inside a state updater, which
    // StrictMode would double-invoke in dev — hence the two-tabs-per-spawn bug.
    //
    // Resolution: if the session is already shown in some tab → activate
    // that tab (and focus its pane). No new tab, no daemon round-trip.
    // Unbound sessions are left alone here — the explicit recovery paths
    // (double-click → add to active tab; context menu → add / open in new
    // tab) avoid creating tabs by accident when the user just wanted to
    // browse the sidebar.
    const s = latestStateRef.current;
    const client = s?.client;
    if (!client) {
      setState((cur) => {
        if (!cur.attentionSessions.has(sessionId) && cur.spotlightSessionIds.size === 0) {
          return cur;
        }
        const next = new Set(cur.attentionSessions);
        next.delete(sessionId);
        return { ...cur, attentionSessions: next, spotlightSessionIds: new Set<string>() };
      });
      return;
    }
    const existing = findTabContainingSession(s.tabs, sessionId);
    if (existing) {
      setState((cur) => {
        const nextAttention = new Set(cur.attentionSessions);
        nextAttention.delete(sessionId);
        return {
          ...cur,
          attentionSessions: nextAttention,
          activeTabId: existing.tabId,
          focusedPaneId: existing.paneId,
          focusedPaneByTab: {
            ...cur.focusedPaneByTab,
            [existing.tabId]: existing.paneId,
          },
          spotlightSessionIds: new Set<string>(),
        };
      });
      return;
    }
    // Unbound session: left-click is a no-op (matches user preference —
    // recovery happens via double-click → add to current tab, or via the
    // context menu's "Add to current tab" / "Open in new tab" entries).
    // Still clear attention/spotlight so the click visibly acknowledges
    // the session.
    setState((cur) => {
      if (
        !cur.attentionSessions.has(sessionId) &&
        cur.spotlightSessionIds.size === 0
      ) {
        return cur;
      }
      const next = new Set(cur.attentionSessions);
      next.delete(sessionId);
      return {
        ...cur,
        attentionSessions: next,
        spotlightSessionIds: new Set<string>(),
      };
    });
  }, []);

  /// Bring an unbound session into a pane in `tabId` using the smart
  /// placement logic. Used by the sidebar's double-click handler and the
  /// session context menu's "Add to current tab" entry. No-op when the
  /// target tab can't host (diff tab) or the session is already bound to
  /// that tab.
  const onAddSessionToTab = useCallback(
    (sessionId: string, tabId: string) => {
      const s = latestStateRef.current;
      const client = s?.client;
      if (!client) return;
      const tab = s.tabs.find((t) => t.id === tabId);
      const grid = tab ? tabGrid(tab) : null;
      if (!grid) return;
      const alreadyBound = collectPanes(grid).some(
        (p) => p.session_id === sessionId,
      );
      if (alreadyBound) {
        setState((cur) => ({
          ...cur,
          activeTabId: tabId,
          focusedPaneId:
            cur.focusedPaneByTab[tabId] ?? cur.focusedPaneId,
        }));
        return;
      }
      const session = s.sessions.find((candidate) => candidate.id === sessionId);
      if (!session) return;
      sendAddSessionToTab(client, tabId, session, latestStateRef);
      setState((cur) => ({
        ...cur,
        activeTabId: tabId,
      }));
    },
    [],
  );

  const onSpawnInPane = useCallback(
    (paneId: string) => {
      // Record the (tab, pane) the user clicked from so the spawn follow-up
      // can route the new session into that pane via `replace_pane_session`
      // instead of opening a fresh tab.
      const cur = latestStateRef.current;
      if (cur?.activeTabId) {
        spawnTargetPaneRef.current = { tabId: cur.activeTabId, paneId };
      }

      // Preselect repo/ws: prefer the session of the pane this empty pane
      // was split from, otherwise fall back to the most recently spawned
      // session's anchor. Not locked — the user can still pick a
      // different target before submitting.
      let preselect: SpawnInitialTarget | undefined;
      if (cur) {
        const activeTab = cur.tabs.find((t) => t.id === cur.activeTabId);
        const grid = activeTab ? tabGrid(activeTab) : null;
        const siblingSessionId = grid
          ? findSplitSiblingSession(grid, paneId)
          : null;
        if (siblingSessionId) {
          const session = cur.sessions.find((s) => s.id === siblingSessionId);
          if (session) {
            const t = spawnTargetFromSession(session);
            if (t) preselect = t;
          }
        }
        if (!preselect) {
          const fallback = lastSpawnedTarget(cur.sessions);
          if (fallback) preselect = fallback;
        }
      }

      setState((s) => ({
        ...s,
        spawnOpen: true,
        spawnInitial: preselect,
        spawnInitialLocked: false,
        spawnPrefill: undefined,
        focusedPaneId: paneId,
        focusedPaneByTab: s.activeTabId
          ? { ...s.focusedPaneByTab, [s.activeTabId]: paneId }
          : s.focusedPaneByTab,
      }));
    },
    [],
  );

  const onOpenSpawn = useCallback((initial?: SpawnInitialTarget) => {
    // Toolbar/sidebar spawn — explicitly clear any prior pane intent so a
    // leftover from a cancelled "spawn in this pane" doesn't re-target.
    spawnTargetPaneRef.current = null;
    spawnTargetTabRef.current = null;
    setState((s) => ({
      ...s,
      spawnOpen: true,
      spawnInitial: initial,
      spawnInitialLocked: true,
      spawnPrefill: undefined,
    }));
  }, []);

  /// Right-click a tab container in the sidebar's Tabs view → "Spawn
  /// session here". Records the target tab id and opens the spawn
  /// dialog with no initial repo/workspace selection — the user still
  /// picks where the session runs; only the destination tab is pinned.
  const onOpenSpawnIntoTab = useCallback((tabId: string) => {
    spawnTargetPaneRef.current = null;
    spawnTargetTabRef.current = tabId;
    setState((s) => ({
      ...s,
      spawnOpen: true,
      spawnInitial: undefined,
      spawnInitialLocked: true,
      spawnPrefill: undefined,
    }));
  }, []);

  const onCloseSpawn = useCallback(() => {
    spawnTargetPaneRef.current = null;
    spawnTargetTabRef.current = null;
    setState((s) => ({
      ...s,
      spawnOpen: false,
      spawnInitial: undefined,
      spawnInitialLocked: true,
      spawnPrefill: undefined,
      spawnPin: undefined,
    }));
  }, []);

  /// "Launch session here" from the worktrees manager. Opens the spawn dialog
  /// locked to the group's repo or workspace and pinned to its worktrees, so
  /// the agent, run mode, model, and prompt are all still chooseable — the
  /// only thing the gesture decides is *where* the session runs.
  const onLaunchIntoWorktree = useCallback((entry: RootWorktreeEntry) => {
    const launch = entry.launch;
    if (!launch || launch.kind === "unknown") return;
    const initial: SpawnInitialTarget =
      launch.kind === "single"
        ? { kind: "repo", repo_id: launch.repo_id }
        : { kind: "workspace", workspace_id: launch.workspace_id };
    spawnTargetPaneRef.current = null;
    spawnTargetTabRef.current = null;
    setState((s) => ({
      ...s,
      worktreesManagerOpen: false,
      spawnOpen: true,
      spawnInitial: initial,
      spawnInitialLocked: true,
      spawnPrefill: undefined,
      spawnPin: launch,
    }));
  }, []);

  /// Instant duplicate from a session context menu. Arms a pending
  /// spawn intent BEFORE sending DuplicateSession so the daemon-broadcast
  /// `session_updated` for the new clone auto-attaches to the chosen
  /// destination (a fresh tab or an existing one) instead of landing
  /// in the Unbound container.
  const onDuplicateSession = useCallback(
    (sessionId: string, target: DuplicateTarget) => {
      const client = latestStateRef.current?.client;
      if (!client) return;
      pendingSpawnIntentRef.current =
        target === "new_tab"
          ? { kind: "newTab" }
          : { kind: "addToTab", tabId: target.tabId };
      client.send({ type: "duplicate_session", session_id: sessionId });
    },
    [],
  );

  /// Send a launch-last replay. `intent` was resolved when the user clicked,
  /// so a wait on the daemon can't move the session to a tab they have since
  /// switched to. `name` is the branch the daemon picked for a worktree
  /// launch — passing it also flips the spawn to `refuse_leftover`, so a name
  /// that turns out to be taken between suggestion and spawn fails loudly
  /// instead of attaching a stale branch. `null` is the in-place case: the
  /// saved branch is replayed verbatim and the reuse policy is left alone.
  const performLaunchLast = useCallback(
    (
      config: SpawnConfig,
      target: LaunchLastSpawnTarget,
      intent: PendingSpawnIntent,
      name: string | null,
    ) => {
      const client = latestStateRef.current?.client;
      if (!client) return;
      const nextTarget: LaunchLastSpawnTarget =
        name === null
          ? target
          : { ...target, branch_name: name, worktree_reuse: "refuse_leftover" };
      pendingSpawnIntentRef.current = intent;
      client.send({
        type: "spawn_session",
        label: null,
        target: nextTarget,
        mode: config.mode,
        initial_prompt: null,
        dangerously_skip_permissions: config.dangerously_skip_permissions,
        agent_options: config.agent_options,
        model: config.model,
        extra_env: config.extra_env,
        prompt_injector: null,
      });
      pushToast(setState, {
        severity: "info",
        message: "Spawning session…",
        detail: "Worktree creation may take a few seconds.",
      });
    },
    [],
  );

  /// "Launch last again" from the sidebar (double-click on a container or
  /// the matching context-menu submenu). Reads the targeted repo/workspace's
  /// `last_spawn_config` and replays it. When no config is saved yet, falls
  /// back to opening the spawn dialog pre-targeted to the container.
  ///
  /// Branch policy: for `use_worktree === true` the daemon names the branch —
  /// it is the only side that can check every member repo's refs — so the
  /// replay waits for `branch_name_suggestion` and then spawns with
  /// `refuse_leftover`, which fails the spawn rather than attaching a
  /// leftover branch nobody was asked about. `use_worktree === false` spawns
  /// straight away with the saved branch: that mode targets a real branch
  /// (e.g. `main`, `feature/x`) by the user's choice.
  const onLaunchLast = useCallback(
    (target: SpawnInitialTarget, placement: LaunchLastPlacement) => {
      const s = latestStateRef.current;
      const client = s?.client;
      if (!s || !client) return;
      const config = lastSpawnConfigForTarget(s, target);
      if (!config) {
        onOpenSpawn(target);
        return;
      }
      const spawnTarget = config.target;
      if (spawnTarget.kind === "standalone") {
        onOpenSpawn(target);
        return;
      }
      if (config.dangerously_skip_permissions) {
        spawnTargetPaneRef.current = null;
        spawnTargetTabRef.current = null;
        setState((state) => ({
          ...state,
          spawnOpen: true,
          spawnInitial: target,
          spawnInitialLocked: true,
          spawnPrefill: config,
        }));
        pushToast(setState, {
          severity: "warning",
          message: "Review trusted launch",
          detail:
            "Launch last uses elevated authority, so the full spawn dialog opens before replay.",
        });
        return;
      }
      const intent = resolveLaunchIntent(s.tabs, s.activeTabId, placement);
      if (!spawnTarget.use_worktree) {
        performLaunchLast(config, spawnTarget, intent, null);
        return;
      }
      const suggestTarget = suggestTargetFor(
        target.kind === "repo"
          ? { kind: "repo", id: target.repo_id }
          : { kind: "workspace", id: target.workspace_id },
      );
      const key = suggestTargetKey(suggestTarget);
      const inFlight = pendingLaunchLastRef.current.get(key);
      if (inFlight) window.clearTimeout(inFlight.timer);
      const timer = window.setTimeout(() => {
        pendingLaunchLastRef.current.delete(key);
        pushToast(setState, {
          severity: "error",
          message: "Couldn't pick a branch name for launch-last",
          detail:
            "The daemon didn't answer in time. Try again, or use the spawn dialog.",
        });
      }, BRANCH_SUGGESTION_TIMEOUT_MS);
      pendingLaunchLastRef.current.set(key, {
        config,
        target: spawnTarget,
        intent,
        timer,
      });
      client.send({ type: "suggest_branch_name", target: suggestTarget });
    },
    [onOpenSpawn, performLaunchLast],
  );

  /// Answer to a launch-last branch request: spawn the replay that was
  /// waiting on the name. Suggestions for a spawn-dialog form carry a key
  /// nothing here waits on and fall through.
  useEffect(() => {
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      for (const [key, pending] of pendingLaunchLastRef.current) {
        const name = matchesSuggestion(detail, key);
        if (name === null) continue;
        window.clearTimeout(pending.timer);
        pendingLaunchLastRef.current.delete(key);
        performLaunchLast(pending.config, pending.target, pending.intent, name);
      }
    };
    window.addEventListener("rt:branch_name_suggestion", handler);
    return () =>
      window.removeEventListener("rt:branch_name_suggestion", handler);
  }, [performLaunchLast]);

  const onEditLastSpawn = useCallback((target: SpawnInitialTarget) => {
    const s = latestStateRef.current;
    if (!s) return;
    const config = lastSpawnConfigForTarget(s, target);
    spawnTargetPaneRef.current = null;
    spawnTargetTabRef.current = null;
    setState((state) => ({
      ...state,
      spawnOpen: true,
      spawnInitial: target,
      spawnInitialLocked: true,
      spawnPrefill:
        config && config.target.kind !== "standalone" ? config : undefined,
    }));
  }, []);

  /// Shift-click on a session's "Duplicate" context-menu entry. Fetches
  /// the source's persisted SpawnConfig and opens the spawn dialog with
  /// every field pre-filled. If the daemon replies with `null` (the
  /// session is unknown or its sidecar pre-dates spawn-config persistence)
  /// we fall back to opening the dialog with Settings defaults — the user
  /// can still tweak fields manually.
  const onDuplicateSessionWithDialog = useCallback((sessionId: string) => {
    const client = latestStateRef.current?.client;
    if (!client) return;
    void requestSpawnConfig(client, sessionId).then((config) => {
      let initial: SpawnInitialTarget | undefined;
      if (config) {
        if (config.target.kind === "standalone") {
          const cwd = config.target.cwd;
          spawnTargetPaneRef.current = null;
          spawnTargetTabRef.current = null;
          setState((s) => ({
            ...s,
            standaloneShellOpen: true,
            standaloneShellInitial: cwd,
          }));
          return;
        }
        initial =
          config.target.kind === "single"
            ? { kind: "repo", repo_id: config.target.repo_id }
            : { kind: "workspace", workspace_id: config.target.workspace_id };
      }
      spawnTargetPaneRef.current = null;
      spawnTargetTabRef.current = null;
      setState((s) => ({
        ...s,
        spawnOpen: true,
        spawnInitial: initial,
        spawnInitialLocked: true,
        spawnPrefill: config ?? undefined,
      }));
    });
  }, []);

  // Listen for "duplicate with dialog" requests dispatched by surfaces
  // that don't have direct access to App-level callbacks (e.g. the
  // SessionPane header context menu inside the deep GridRenderer tree).
  useEffect(() => {
    const handler = (ev: Event) => {
      const sid = (ev as CustomEvent<string>).detail;
      if (typeof sid === "string" && sid.length > 0) {
        onDuplicateSessionWithDialog(sid);
      }
    };
    window.addEventListener("rt:duplicate_session_with_dialog", handler);
    return () =>
      window.removeEventListener("rt:duplicate_session_with_dialog", handler);
  }, [onDuplicateSessionWithDialog]);

  // Same pattern for the plain-click duplicate path. SessionPane sits
  // deep in the GridRenderer tree and dispatches a window event so we
  // don't have to prop-drill through every intermediate component.
  useEffect(() => {
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<{ sessionId: string; target: DuplicateTarget }>)
        .detail;
      if (detail && typeof detail.sessionId === "string") {
        onDuplicateSession(detail.sessionId, detail.target);
      }
    };
    window.addEventListener("rt:duplicate_session", handler);
    return () => window.removeEventListener("rt:duplicate_session", handler);
  }, [onDuplicateSession]);

  // Add an unbound session to the active tab via smart placement. Fired by
  // the sidebar's session row on double-click and by the session context
  // menu's "Add to current tab" entry. Routed here so neither surface has
  // to know the active tab id — App reads it from its own state.
  useEffect(() => {
    const handler = (ev: Event) => {
      const sid = (ev as CustomEvent<string>).detail;
      if (typeof sid !== "string" || sid.length === 0) return;
      const tabId = latestStateRef.current?.activeTabId;
      if (!tabId) return;
      onAddSessionToTab(sid, tabId);
    };
    window.addEventListener("rt:add_session_to_active_tab", handler);
    return () =>
      window.removeEventListener("rt:add_session_to_active_tab", handler);
  }, [onAddSessionToTab]);

  // Restart-in-place from a stopped session's pane. Sequence matters: the
  // daemon processes WS messages in order, and discarding the stopped session
  // collapses every pane bound to it (`close_session_panes`) — so the discard
  // must NOT be sent here. It rides along as `discardSessionId` on the intent
  // and goes out only after the clone's placement send, at which point the
  // pane references the clone and the discard can't take the slot with it.
  // The pendingSpawnIntentRef routes the new clone:
  //   - replacePane when tabId+paneId are known (grid-rendered pane): the
  //     new clone takes over the dead session's slot via
  //     replace_pane_session, so the restart looks like an in-place swap.
  //   - addToTab when only tabId is known (sidebar / context menu): the
  //     new clone appends to the tab.
  //   - newTab when neither (orphans, single-session pop-out).
  useEffect(() => {
    const handler = (ev: Event) => {
      const detail = (
        ev as CustomEvent<{
          sessionId: string;
          tabId: string | null;
          paneId?: string | null;
        }>
      ).detail;
      if (!detail || typeof detail.sessionId !== "string") return;
      const client = latestStateRef.current?.client;
      if (!client) return;
      if (detail.tabId && detail.paneId) {
        pendingSpawnIntentRef.current = {
          kind: "replacePane",
          tabId: detail.tabId,
          paneId: detail.paneId,
          discardSessionId: detail.sessionId,
        };
      } else if (detail.tabId) {
        pendingSpawnIntentRef.current = {
          kind: "addToTab",
          tabId: detail.tabId,
          discardSessionId: detail.sessionId,
        };
      } else {
        pendingSpawnIntentRef.current = {
          kind: "newTab",
          discardSessionId: detail.sessionId,
        };
      }
      client.send({ type: "duplicate_session", session_id: detail.sessionId });
    };
    window.addEventListener("rt:pane_session_restart", handler);
    return () =>
      window.removeEventListener("rt:pane_session_restart", handler);
  }, []);

  // "New session" from a stopped session's pane: open the spawn dialog with
  // the pane recorded as the placement target so the fresh session takes over
  // the dead session's grid slot. The stopped session rides along as
  // `discardSessionId` and is discarded only after the new session binds into
  // the pane — cancelling the dialog (or overriding placement) leaves it
  // untouched. Preselects the stopped session's repo/workspace, unlocked.
  useEffect(() => {
    const handler = (ev: Event) => {
      const detail = (
        ev as CustomEvent<{
          sessionId: string;
          tabId: string | null;
          paneId?: string | null;
        }>
      ).detail;
      if (!detail || typeof detail.sessionId !== "string") return;
      const cur = latestStateRef.current;
      spawnTargetPaneRef.current =
        detail.tabId && detail.paneId
          ? {
              tabId: detail.tabId,
              paneId: detail.paneId,
              discardSessionId: detail.sessionId,
            }
          : null;
      spawnTargetTabRef.current = null;
      const session = cur?.sessions.find((s) => s.id === detail.sessionId);
      const preselect = session ? spawnTargetFromSession(session) : null;
      setState((s) => ({
        ...s,
        spawnOpen: true,
        spawnInitial: preselect ?? undefined,
        spawnInitialLocked: false,
        spawnPrefill: undefined,
      }));
    };
    window.addEventListener("rt:pane_session_new", handler);
    return () => window.removeEventListener("rt:pane_session_new", handler);
  }, []);

  /// Every "delete the worktree" gesture — the pane-close dialog, the two
  /// context-menu remove entries, "Stop and delete worktree", and the
  /// stopped-pane overlay — dispatches `rt:request_delete_worktree` instead
  /// of sending `discard_session` itself, so the branch-fate confirm is the
  /// single gate in front of a worktree deletion.
  ///
  /// Event detail: `{ sessionId: string; closePane: { tabId, paneId } | null }`.
  /// `closePane` is set only when the gesture also removes the pane from its
  /// tab; the dispatcher takes any undo snapshot it wants before dispatching.
  useEffect(() => {
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DeleteWorktreeRequest>).detail;
      if (!detail || typeof detail.sessionId !== "string") return;
      setState((s) => ({
        ...s,
        deleteWorktreeRequest: {
          sessionId: detail.sessionId,
          closePane: detail.closePane ?? null,
        },
      }));
    };
    window.addEventListener("rt:request_delete_worktree", handler);
    return () =>
      window.removeEventListener("rt:request_delete_worktree", handler);
  }, []);

  /// Answer from the DeleteWorktreeDialog: close the pane if the gesture
  /// included one, stop the session if it's still alive (keeping the worktree
  /// so the discard owns the removal), then discard with the chosen branch
  /// fate.
  const onConfirmDeleteWorktree = useCallback((branch: BranchCleanup) => {
    const cur = latestStateRef.current;
    const request = cur?.deleteWorktreeRequest;
    const client = cur?.client;
    const session = cur?.sessions.find((s) => s.id === request?.sessionId);
    if (client && request && session) {
      if (request.closePane) {
        client.send({
          type: "close_pane",
          tab_id: request.closePane.tabId,
          pane_id: request.closePane.paneId,
        });
      }
      if (session.status !== "stopped" && session.status !== "error") {
        client.send({
          type: "stop_session",
          session_id: session.id,
          cleanup: session.members.map((m) => ({
            repo_id: m.repo_id,
            remove_worktree: false,
            branch: "auto" as const,
          })),
        });
      }
      client.send({
        type: "discard_session",
        session_id: session.id,
        cleanup: session.members.map((m) => ({
          repo_id: m.repo_id,
          remove_worktree: true,
          branch,
        })),
      });
    }
    setState((s) => ({ ...s, deleteWorktreeRequest: null }));
  }, []);

  const onCancelDeleteWorktree = useCallback(() => {
    setState((s) => ({ ...s, deleteWorktreeRequest: null }));
  }, []);

  const deleteWorktreeSession = useMemo<SessionSnapshot | null>(() => {
    const request = state.deleteWorktreeRequest;
    if (!request) return null;
    return state.sessions.find((s) => s.id === request.sessionId) ?? null;
  }, [state.deleteWorktreeRequest, state.sessions]);

  // The session can disappear while the confirm is open — another window
  // discarded it, or the daemon dropped it. Retire the request instead of
  // leaving a dialog pointed at nothing.
  useEffect(() => {
    if (state.deleteWorktreeRequest === null) return;
    if (deleteWorktreeSession !== null) return;
    setState((s) => ({ ...s, deleteWorktreeRequest: null }));
  }, [state.deleteWorktreeRequest, deleteWorktreeSession]);

  const onLaunchPreset = useCallback(
    (preset: PresetEntry, target: PresetTarget) => {
      setState((s) => ({ ...s, presetLaunch: { preset, target } }));
    },
    [],
  );

  const onClosePresetLaunch = useCallback(() => {
    setState((s) => ({ ...s, presetLaunch: null }));
  }, []);

  /// Stash a just-sent spawn request so we can resend it with a checkout
  /// strategy if the daemon asks to confirm a dirty in-place switch.
  const onSpawnRequest = useCallback(
    (req: Extract<ClientMessage, { type: "spawn_session" }>) => {
      lastSpawnReqRef.current = req;
    },
    [],
  );

  /// Resolve the checkout-confirm modal: resend the stashed spawn with the
  /// chosen strategy, then clear the prompt.
  const onCheckoutConfirm = useCallback(
    (strategy: CheckoutChoice) => {
      const base = lastSpawnReqRef.current;
      const client = latestStateRef.current?.client;
      if (base && client && base.target.kind === "single") {
        client.send({
          ...base,
          target: { ...base.target, checkout_strategy: strategy },
        });
      }
      setState((s) => ({ ...s, checkoutConfirm: null }));
    },
    [],
  );

  const onSpawned = useCallback((placement: SpawnPlacement, detail?: string) => {
    // Arm the one-shot follow-up for the next new session. Priority:
    // 1. `new_tab` placement → open a fresh tab.
    // 2. `tab` placement → land in that specific tab (overrides pane/
    //    tab targets — explicit user choice wins).
    // 3. Pane target (user opened dialog from an empty pane's "+ spawn"
    //    button) → drop session into that pane.
    // 4. Tab target (user opened dialog from a tab container's right-
    //    click "Spawn session here") → add a pane bound to the session
    //    in that tab.
    // 5. Default `current_tab` → add a pane to the active tab, with a
    //    new-tab fallback when the current tab cannot host panes.
    const paneTarget = spawnTargetPaneRef.current;
    const tabTarget = spawnTargetTabRef.current;
    const currentState = latestStateRef.current;
    const activeTab = currentState?.tabs.find(
      (t) => t.id === currentState.activeTabId,
    );
    const activeTabCanHost = activeTab ? tabGrid(activeTab) !== null : false;
    if (placement.kind === "new_tab") {
      pendingSpawnIntentRef.current = { kind: "newTab" };
    } else if (placement.kind === "tab") {
      const targetTab = currentState?.tabs.find((t) => t.id === placement.tabId);
      const canHost = targetTab ? tabGrid(targetTab) !== null : false;
      pendingSpawnIntentRef.current = canHost
        ? { kind: "addToTab", tabId: placement.tabId }
        : { kind: "newTab" };
    } else if (paneTarget) {
      pendingSpawnIntentRef.current = {
        kind: "replacePane",
        tabId: paneTarget.tabId,
        paneId: paneTarget.paneId,
        ...(paneTarget.discardSessionId
          ? { discardSessionId: paneTarget.discardSessionId }
          : {}),
      };
    } else if (tabTarget) {
      pendingSpawnIntentRef.current = { kind: "addToTab", tabId: tabTarget };
    } else if (activeTab && activeTabCanHost) {
      pendingSpawnIntentRef.current = {
        kind: "addToTab",
        tabId: activeTab.id,
      };
    } else {
      pendingSpawnIntentRef.current = { kind: "newTab" };
    }
    spawnTargetPaneRef.current = null;
    spawnTargetTabRef.current = null;
    // Optimistic feedback while the daemon resolves the spawn. Worktree
    // creation can take several seconds (Phase 2 workspace flows resolve
    // multiple repos), and previously the dialog vanished without trace
    // until session_updated arrived. The toast auto-dismisses after 8s,
    // by which point the session is normally live in the sidebar.
    pushToast(setState, {
      severity: "info",
      message: "Spawning session…",
      detail: detail ?? "Worktree creation may take a few seconds.",
    });
  }, []);

  const launchStandaloneShell = useCallback(
    (cwd: string | null) => {
      const client = latestStateRef.current?.client;
      if (!client) return;
      client.send({
        type: "spawn_session",
        label: null,
        target: { kind: "standalone", cwd },
        mode: "plain_shell",
        initial_prompt: null,
        dangerously_skip_permissions: false,
        agent_options: DEFAULT_CLAUDE_OPTIONS,
        model: null,
        extra_env: [],
        prompt_injector: null,
      });
      onSpawned(
        { kind: "current_tab" },
        "Shell startup may take a few seconds.",
      );
    },
    [onSpawned],
  );

  const onLaunchStandaloneShellDefault = useCallback(() => {
    launchStandaloneShell(
      settingsRef.current.spawn.standalone_shell_default_dir,
    );
  }, [launchStandaloneShell]);

  const onOpenStandaloneShellDialog = useCallback(() => {
    spawnTargetPaneRef.current = null;
    spawnTargetTabRef.current = null;
    setState((s) => ({
      ...s,
      standaloneShellOpen: true,
      standaloneShellInitial: null,
    }));
  }, []);

  const onCloseStandaloneShellDialog = useCallback(() => {
    setState((s) => ({
      ...s,
      standaloneShellOpen: false,
      standaloneShellInitial: null,
    }));
  }, []);

  const onSubmitStandaloneShell = useCallback(
    (cwd: string, saveAsDefault: boolean) => {
      if (saveAsDefault) {
        const current = settingsRef.current;
        saveSettings({
          ...current,
          spawn: {
            ...current.spawn,
            standalone_shell_default_dir: cwd,
          },
        });
      }
      setState((s) => ({
        ...s,
        standaloneShellOpen: false,
        standaloneShellInitial: null,
      }));
      launchStandaloneShell(cwd);
    },
    [launchStandaloneShell],
  );

  const onOpenWorkspaceCreator = useCallback(() => {
    setState((s) => ({ ...s, workspaceCreatorOpen: true }));
  }, []);

  const onCloseWorkspaceCreator = useCallback(() => {
    setState((s) => ({ ...s, workspaceCreatorOpen: false }));
  }, []);

  const onDismissVscodeSuggestion = useCallback(() => {
    setState((s) => ({ ...s, vscodeQueue: s.vscodeQueue.slice(1) }));
  }, []);

  const onDismissToast = useCallback((id: string) => {
    setState((s) => ({ ...s, toasts: s.toasts.filter((t) => t.id !== id) }));
  }, []);

  const onTabWillClose = useCallback(
    (tab: TabEntry, index: number, restoreActive: boolean) => {
      const entry: UndoEntry = {
        id: newUndoId(),
        kind: "restore_tabs",
        message: `Closed tab "${tab.name}"`,
        actionLabel: "Undo",
        tabs: [
          {
            tab: cloneTab(tab),
            index,
            restoreActive,
            restoreFocusedPaneId: null,
          },
        ],
        expiresAt: Date.now() + UNDO_TTL_MS,
      };
      const affectedTabIds = new Set(entry.tabs.map((snapshot) => snapshot.tab.id));
      setState((s) => ({
        ...s,
        undoEntries: [
          entry,
          ...s.undoEntries.filter((existing) => !undoTouches(existing, affectedTabIds)),
        ].slice(0, UNDO_LIMIT),
      }));
    },
    [],
  );

  const onTabsSnapshotUndo = useCallback(
    (tabs: TabEntry[], message: string, restoreFocusedPaneId: string | null) => {
      const current = latestStateRef.current;
      const snapshots = uniqueTabs(tabs).map((tab) => {
        const index = Math.max(
          0,
          current?.tabs.findIndex((existing) => existing.id === tab.id) ?? 0,
        );
        const restoreActive = current?.activeTabId === tab.id;
        return {
          tab: cloneTab(tab),
          index,
          restoreActive,
          restoreFocusedPaneId: restoreActive ? restoreFocusedPaneId : null,
        };
      });
      if (snapshots.length === 0) {
        return;
      }
      const entry: UndoEntry = {
        id: newUndoId(),
        kind: "restore_tabs",
        message,
        actionLabel: "Undo",
        tabs: snapshots,
        expiresAt: Date.now() + UNDO_TTL_MS,
      };
      const affectedTabIds = new Set(entry.tabs.map((snapshot) => snapshot.tab.id));
      setState((s) => ({
        ...s,
        undoEntries: [
          entry,
          ...s.undoEntries.filter((existing) => !undoTouches(existing, affectedTabIds)),
        ].slice(0, UNDO_LIMIT),
      }));
    },
    [],
  );

  const onDismissUndo = useCallback((id: string) => {
    setState((s) => ({
      ...s,
      undoEntries: s.undoEntries.filter((entry) => entry.id !== id),
    }));
  }, []);

  const onUndo = useCallback((id: string) => {
    const entry = latestStateRef.current?.undoEntries.find((e) => e.id === id);
    if (!entry) {
      return;
    }
    const client = clientRef.current;
    const currentTabs = latestStateRef.current?.tabs ?? [];
    const activeSnapshot =
      entry.tabs.find((snapshot) => snapshot.restoreActive) ?? null;
    setState((s) => {
      const next: AppState = {
        ...s,
        undoEntries: s.undoEntries.filter((e) => e.id !== id),
        activeTabId: activeSnapshot ? activeSnapshot.tab.id : s.activeTabId,
        focusedPaneId: activeSnapshot
          ? activeSnapshot.restoreFocusedPaneId
          : s.focusedPaneId,
      };
      if (
        activeSnapshot &&
        activeSnapshot.restoreFocusedPaneId !== null
      ) {
        next.focusedPaneByTab = {
          ...s.focusedPaneByTab,
          [activeSnapshot.tab.id]: activeSnapshot.restoreFocusedPaneId,
        };
      }
      return next;
    });
    for (const snapshot of [...entry.tabs].sort((a, b) => a.index - b.index)) {
      const tabExists = currentTabs.some((tab) => tab.id === snapshot.tab.id);
      if (tabExists) {
        client?.send({
          type: "restore_tab_snapshot",
          tab: cloneTab(snapshot.tab),
        });
      } else {
        client?.send({
          type: "restore_tab",
          tab: cloneTab(snapshot.tab),
          index: snapshot.index,
        });
      }
    }
  }, []);

  const onRemoveRepoWithLiveSessions = useCallback(
    (intent: RepoRemoveIntent) => {
      setState((s) => ({ ...s, repoRemove: intent }));
    },
    [],
  );

  const onCancelRepoRemove = useCallback(() => {
    setState((s) => ({ ...s, repoRemove: null }));
  }, []);

  const onRepoRemoveAnyway = useCallback(() => {
    setState((s) => {
      if (s.repoRemove !== null) {
        s.client?.send({ type: "remove_repo", repo_id: s.repoRemove.repoId });
      }
      return { ...s, repoRemove: null };
    });
  }, []);

  const onRepoStopAndRemove = useCallback(() => {
    setState((s) => {
      if (s.repoRemove !== null && s.client) {
        // Send stop_session for each live session, then remove the repo.
        // Cleanup uses remove_worktree: false because the user said "stop"
        // not "delete" — preserving the worktree is the safer default.
        for (const session of s.repoRemove.liveSessions) {
          s.client.send({
            type: "stop_session",
            session_id: session.id,
            cleanup: [],
          });
        }
        s.client.send({
          type: "remove_repo",
          repo_id: s.repoRemove.repoId,
        });
      }
      return { ...s, repoRemove: null };
    });
  }, []);

  const onRevealInExplorer = useCallback((path: string) => {
    if (latestStateRef.current?.connectionTarget.kind === "remote") {
      notifyRemoteUnavailable("Reveal in file manager");
      return;
    }
    void invoke("reveal_in_explorer", { path }).catch((err: unknown) => {
      console.error("reveal_in_explorer failed", err);
    });
  }, []);

  // Best-effort cleanup for pane-popout bookkeeping: if a pane id no longer
  // exists in any tab, drop its popped-out marker so stale placeholders do
  // not survive reloads or daemon-side topology changes.
  useEffect(() => {
    if (state.tabs.length === 0) return;
    const keep = new Set<string>();
    for (const tab of state.tabs) {
      const grid = tabGrid(tab);
      if (!grid) continue;
      for (const pane of collectPanes(grid)) {
        keep.add(pane.pane_id);
      }
    }
    prunePoppedOutPanes(keep);
  }, [state.tabs]);

  // Pane pop-out: close itself when the pane disappears or loses its bound
  // session (e.g. tab closed, pane closed, or the session was discarded).
  useEffect(() => {
    if (!popoutPaneId) return;
    if (state.tabs.length > 0) tabsHydratedRef.current = true;
    if (!tabsHydratedRef.current) return; // not yet hydrated
    const binding = findPaneBinding(state.tabs, popoutPaneId);
    if (binding && binding.session_id !== null) return;
    clearPanePoppedOut(popoutPaneId);
    void getCurrentWindow().close();
  }, [state.tabs]);

  // Pop-out window: close itself when the tab it was rendering disappears
  // (e.g. user closed the tab from the main window).
  useEffect(() => {
    if (!popoutTabId) return;
    if (state.tabs.length > 0) tabsHydratedRef.current = true;
    if (!tabsHydratedRef.current) return; // not yet hydrated
    if (state.tabs.some((t) => t.id === popoutTabId)) return;
    void getCurrentWindow().close();
  }, [state.tabs]);

  // Session pop-out: close itself only when the session it was rendering is
  // removed from the registry. A stopped session remains renderable so the
  // pop-out can show the same restart/remove surface as an in-grid pane.
  useEffect(() => {
    if (!popoutSessionId) return;
    if (state.sessions.length > 0) sessionsHydratedRef.current = true;
    if (!sessionsHydratedRef.current) return; // not yet hydrated
    if (state.sessions.some((s) => s.id === popoutSessionId)) return;
    void getCurrentWindow().close();
  }, [state.sessions]);

  // Intercept main-window close so we can ask whether to stop the daemon.
  // Short-circuit when no active sessions exist: nothing to lose, nothing
  // to preserve — just close the window and let the daemon keep running
  // for next launch.
  useEffect(() => {
    if (popoutPaneId || popoutSessionId || popoutTabId) return;
    let unlisten: (() => void) | null = null;
    void (async () => {
      const win = getCurrentWindow();
      unlisten = await win.onCloseRequested((event) => {
        logToFile("info", "main window onCloseRequested fired");
        const sessions = latestStateRef.current?.sessions ?? [];
        const hasActive = sessions.some(
          (s) => s.status !== "stopped" && !s.is_orphan,
        );
        if (!hasActive) {
          logToFile(
            "info",
            "main window close: no active sessions, skipping confirmation",
          );
          // Same path as "keep sessions running" — invoke `quit_app` so the
          // teardown runs from the host side instead of the webview's
          // event loop (see lib.rs::quit_app comment).
          event.preventDefault();
          void closeMainWindow();
          return;
        }
        event.preventDefault();
        setState((s) =>
          s.exitConfirmOpen ? s : { ...s, exitConfirmOpen: true },
        );
      });
    })();
    return () => {
      unlisten?.();
    };
  }, []);

  const closeMainWindow = useCallback(async () => {
    logToFile("info", "closeMainWindow: invoking quit_app");
    try {
      await invoke("quit_app");
      logToFile("info", "closeMainWindow: quit_app returned");
    } catch (err) {
      logToFile("error", `closeMainWindow: quit_app threw: ${String(err)}`);
    }
  }, []);

  const onExitCancel = useCallback(() => {
    logToFile("info", "exit modal: Cancel");
    setState((s) => ({ ...s, exitConfirmOpen: false }));
  }, []);

  useEffect(() => {
    if (import.meta.env["VITE_RT_E2E"] !== "1") {
      return;
    }
    const openExitConfirm = () => {
      setState((s) => ({ ...s, exitConfirmOpen: true }));
    };
    window.addEventListener("rt:e2e_open_exit_confirm", openExitConfirm);
    return () => {
      window.removeEventListener("rt:e2e_open_exit_confirm", openExitConfirm);
    };
  }, []);

  const onOpenSettings = useCallback(() => {
    setState((s) => (s.settingsOpen ? s : { ...s, settingsOpen: true }));
  }, []);

  const onCloseSettings = useCallback(() => {
    setState((s) => ({ ...s, settingsOpen: false }));
  }, []);

  const onQuitLeaveRunning = useCallback(() => {
    logToFile("info", "exit modal: Keep sessions running in background");
    void closeMainWindow();
  }, [closeMainWindow]);

  const onForceQuit = useCallback(() => {
    logToFile("warn", "exit modal: Force quit clicked (daemon stuck)");
    void closeMainWindow();
  }, [closeMainWindow]);

  /// `branchChoices` carries the per-session branch fate collected by the
  /// worktree walk; a session with no entry falls back to "auto". Empty for
  /// the modes that don't remove worktrees.
  const sendShutdown = useCallback(
    (
      mode: "stopKeepWorktrees" | "stopRemoveWorktrees" | "abandon",
      branchChoices: Record<string, BranchCleanup>,
    ) => {
      const drain = mode === "stopKeepWorktrees";
      const tag =
        mode === "stopRemoveWorktrees"
          ? "Stop sessions, remove worktrees"
          : drain
          ? "Stop sessions and quit"
          : "Abandon sessions and quit";
      logToFile("info", `exit modal: ${tag} clicked`);
      setState((s) => ({ ...s, exitInFlight: true, exitStuck: false }));
      const client = clientRef.current;
      if (!client) {
        logToFile("warn", `${tag}: no client; closing window directly`);
        void closeMainWindow();
        return;
      }
      // The daemon answers Shutdown with an explicit `ShutdownAck`
      // message after `shutdown_all` completes (typically <5 ms). We
      // treat that ack as the green light to close the window — we do
      // NOT wait for the WebSocket itself to close, because axum drops
      // the upgraded socket without sending a proper Close frame and
      // the OS-level TCP FIN can take seconds to surface in WebView2.
      // The WS-close fallback below still fires if the daemon dies
      // before acking; the 5 s stuck timer covers the genuinely-wedged
      // case where neither path completes.
      let resolved = false;
      const stuckTimer = window.setTimeout(() => {
        if (resolved) return;
        logToFile(
          "warn",
          `${tag}: 5 s elapsed without ack or WS close; surfacing force-quit option`,
        );
        setState((s) => ({ ...s, exitStuck: true }));
      }, 5000);
      const finish = (reason: string) => {
        if (resolved) return;
        resolved = true;
        window.clearTimeout(stuckTimer);
        unsubAck();
        unsubConn();
        logToFile("info", `${tag}: ${reason}; closing window`);
        void closeMainWindow();
      };
      const unsubAck = client.onMessage((msg) => {
        if (msg.type === "shutdown_ack") finish("daemon acked shutdown");
      });
      const unsubConn = client.onConnectionChange((next) => {
        logToFile("info", `${tag}: connection state -> ${next.kind}`);
        if (next.kind === "open" || next.kind === "connecting") return;
        finish(`WS ${next.kind}`);
      });
      if (mode === "stopRemoveWorktrees") {
        const sessions = latestStateRef.current?.sessions ?? [];
        const activeSessions = sessions.filter(
          (session) => session.status !== "stopped" && !session.is_orphan,
        );
        for (const session of activeSessions) {
          const branch = branchChoices[session.id] ?? "auto";
          const cleanup = session.members.map((member) => ({
            repo_id: member.repo_id,
            remove_worktree: session.has_per_session_worktree,
            branch,
          }));
          client.send({
            type: "stop_session",
            session_id: session.id,
            cleanup: cleanup.map((action) => ({
              ...action,
              remove_worktree: false,
              branch: "auto" as const,
            })),
          });
          client.send({
            type: "discard_session",
            session_id: session.id,
            cleanup,
          });
        }
      }
      logToFile("info", `${tag}: sending shutdown message (drain=${drain})`);
      client.send({ type: "shutdown", drain });
    },
    [closeMainWindow],
  );

  const onStopAndQuit = useCallback(
    () => sendShutdown("stopKeepWorktrees", {}),
    [sendShutdown],
  );
  /// Every worktree the quit is about to delete gets its own branch-fate
  /// confirm first. The shutdown waits for the last answer; with no
  /// worktree session to ask about it goes out immediately.
  const onStopAndQuitRemoveWorktrees = useCallback(() => {
    const queue = startExitQueue(latestStateRef.current?.sessions ?? []);
    if (!queue) {
      sendShutdown("stopRemoveWorktrees", {});
      return;
    }
    logToFile(
      "info",
      `exit modal: Stop sessions, remove worktrees — asking branch fate for ${queue.total} session(s)`,
    );
    setState((s) => ({ ...s, exitWorktreeQueue: queue }));
  }, [sendShutdown]);
  const onAbandonAndQuit = useCallback(
    () => sendShutdown("abandon", {}),
    [sendShutdown],
  );

  const onConfirmExitWorktree = useCallback(
    (branch: BranchCleanup) => {
      const queue = latestStateRef.current?.exitWorktreeQueue;
      const sessionId = queue?.pending[0];
      if (!queue || sessionId === undefined) return;
      logToFile("info", `exit modal: worktree ${sessionId} -> branch ${branch}`);
      const next = recordExitChoice(queue, sessionId, branch);
      const done = next.pending.length === 0;
      setState((s) => ({ ...s, exitWorktreeQueue: done ? null : next }));
      if (done) sendShutdown("stopRemoveWorktrees", next.choices);
    },
    [sendShutdown],
  );

  /// Backing out of a branch-fate prompt abandons the whole walk and sends
  /// no shutdown; the exit dialog is still up so another option is one
  /// click away.
  const onCancelExitWorktree = useCallback(() => {
    logToFile("info", "exit modal: branch fate cancelled; quit not sent");
    setState((s) => ({ ...s, exitWorktreeQueue: null }));
  }, []);

  const exitWorktreeSession = useMemo<SessionSnapshot | null>(() => {
    const sessionId = state.exitWorktreeQueue?.pending[0];
    if (sessionId === undefined) return null;
    return state.sessions.find((s) => s.id === sessionId) ?? null;
  }, [state.exitWorktreeQueue, state.sessions]);

  // Sessions can end or be discarded from another window mid-walk. Drop
  // them from the queue (they shut down under "auto"), and when that
  // empties the queue run the shutdown the walk was collecting answers for.
  useEffect(() => {
    const queue = state.exitWorktreeQueue;
    if (!queue) return;
    const next = skipVanished(queue, new Set(state.sessions.map((s) => s.id)));
    if (next.pending.length === queue.pending.length) return;
    const done = next.pending.length === 0;
    setState((s) => ({ ...s, exitWorktreeQueue: done ? null : next }));
    if (done) sendShutdown("stopRemoveWorktrees", next.choices);
  }, [state.exitWorktreeQueue, state.sessions, sendShutdown]);

  /// DaemonFooter — force-stop the daemon and suppress auto-reconnect.
  /// The Tauri-side `stop_daemon` kills the pid + removes the handshake;
  /// flipping `daemonStopRequested` BEFORE the kill ensures the connection
  /// effect's `closed`-handler skips its retry schedule when the WS drops.
  const onStopDaemon = useCallback(() => {
    setState((s) => ({ ...s, daemonStopRequested: true, handshake: null }));
    logToFile("info", "DaemonFooter: Stop daemon clicked");
    void stopDaemon().catch((err) => {
      logToFile("warn", `stopDaemon invoke failed: ${String(err)}`);
    });
  }, []);

  /// DaemonFooter — bring the daemon back. Paths depend on the
  /// current state:
  ///   - Currently open / connecting / closed (transient): send a
  ///     graceful `shutdown` over the WS so the daemon persists state
  ///     before exiting; the existing auto-reconnect cycle then spawns a
  ///     fresh one.
  ///   - Auth failed: the WS protocol did not negotiate, so force the
  ///     supervisor path to re-run. It can use the daemon's HTTP shutdown
  ///     endpoint before spawning a compatible daemon.
  ///   - Currently stopped (user-Stopped): the auto-reconnect cycle has
  ///     been suppressed and there's no live WS to send to. Clear the
  ///     stop flag and bump `connectionVersion` to force the connection
  ///     useEffect to tear down + re-run `connect()` from scratch.
  const onRestartDaemon = useCallback(() => {
    logToFile("info", "DaemonFooter: Restart daemon clicked");
    const client = clientRef.current;
    const wasStopRequested = stopRequestedRef.current;
    const statusKind = latestStateRef.current?.status.kind;
    // Remote: "Reconnect" must never send a WS `shutdown` (that would stop the
    // remote daemon). Just tear down the WS + bridge and re-run connect().
    if (connectionTargetRef.current.kind === "remote") {
      client?.close();
      clientRef.current = null;
      setState((s) => ({
        ...s,
        client: null,
        daemonStopRequested: false,
        handshake: null,
        status: { kind: "connecting" },
      }));
      setConnectionVersion((v) => v + 1);
      return;
    }
    if (statusKind === "auth_failed") {
      logToFile(
        "info",
        "DaemonFooter: auth failed restart; forcing supervisor reconnect",
      );
      client?.close();
      clientRef.current = null;
      setState((s) => ({
        ...s,
        client: null,
        daemonStopRequested: false,
        handshake: null,
        status: { kind: "connecting" },
      }));
      setConnectionVersion((v) => v + 1);
      return;
    }
    if (wasStopRequested || !client) {
      // No live WS — kick the connection effect.
      setState((s) => ({ ...s, daemonStopRequested: false }));
      setConnectionVersion((v) => v + 1);
      return;
    }
    // WS is up (or transient) — graceful shutdown + let the auto-
    // reconnect respawn the daemon.
    setState((s) => ({ ...s, daemonStopRequested: false }));
    client.send({ type: "shutdown", drain: false });
  }, []);

  // Load saved remote profiles once on mount so the connection picker has
  // them without waiting for the user to open it.
  useEffect(() => {
    void listRemoteProfiles()
      .then((profiles) => setState((s) => ({ ...s, remoteProfiles: profiles })))
      .catch((err: unknown) => {
        logToFile("warn", `listRemoteProfiles failed: ${String(err)}`);
      });
  }, []);

  // Toast for local-FS actions a deeply-nested component couldn't cleanly
  // disable in remote mode (e.g. terminal file links). Dedup-keyed so rapid
  // repeats replace in place rather than stacking.
  useEffect(() => {
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<RemoteUnavailableDetail>).detail;
      pushToast(setState, {
        severity: "info",
        key: "remote-unavailable",
        message: `${detail.action} is unavailable on remote connections`,
        detail: "This action would run on your machine, not the host.",
      });
    };
    window.addEventListener(REMOTE_UNAVAILABLE_EVENT, handler);
    return () => window.removeEventListener(REMOTE_UNAVAILABLE_EVENT, handler);
  }, []);

  /// Switch the active connection target and force a fresh connect. Tears down
  /// the loopback bridge when leaving a remote host for local. Clears any
  /// user-Stopped state so the reconnect actually proceeds.
  const onSwitchConnection = useCallback((target: ConnectionTarget) => {
    const prev = latestStateRef.current?.connectionTarget;
    if (prev?.kind === "remote" && target.kind === "local") {
      void disconnectRemote().catch((err: unknown) => {
        logToFile("warn", `disconnectRemote failed: ${String(err)}`);
      });
    }
    saveConnectionTarget(target);
    clientRef.current?.close();
    clientRef.current = null;
    setState((s) => ({
      ...s,
      connectionTarget: target,
      client: null,
      daemonStopRequested: false,
      handshake: null,
      status: { kind: "connecting" },
      connectionPickerOpen: false,
    }));
    setConnectionVersion((v) => v + 1);
  }, []);

  /// "Add remote (paste code)": decode the code, upsert a profile (re-pairing
  /// the same host updates its token/fingerprint in place), then connect.
  /// Returns a decode error so the picker can show it inline; a successful
  /// decode switches the target even if the host is unreachable (that surfaces
  /// as a retrying error in the footer).
  const onAddRemote = useCallback(
    async (
      code: string,
    ): Promise<{ ok: true } | { ok: false; error: string }> => {
      let params;
      try {
        params = await decodeConnectionCode(code);
      } catch (err) {
        return { ok: false, error: String(err) };
      }
      const existing = (latestStateRef.current?.remoteProfiles ?? []).find(
        (p) => p.host === params.host && p.port === params.port,
      );
      const id =
        existing?.id ??
        `remote-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
      const profile: RemoteProfile = {
        id,
        name: existing?.name ?? params.host,
        host: params.host,
        port: params.port,
        token: params.token,
        fingerprint: params.fingerprint,
      };
      try {
        await saveRemoteProfile(profile);
      } catch (err) {
        return { ok: false, error: `saving profile: ${String(err)}` };
      }
      const profiles = await listRemoteProfiles().catch(
        () => [] as RemoteProfile[],
      );
      setState((s) => ({ ...s, remoteProfiles: profiles }));
      onSwitchConnection({ kind: "remote", profileId: id });
      return { ok: true };
    },
    [onSwitchConnection],
  );

  /// Pair with a host discovered over mDNS: exchange its short code for the
  /// auth token over pinned TLS, save (or update) a profile named after the
  /// host, then connect. Mirrors `onAddRemote` but sourced from discovery +
  /// a code instead of a pasted connection code.
  const onPairHost = useCallback(
    async (
      host: DiscoveredHost,
      code: string,
    ): Promise<{ ok: true } | { ok: false; error: string }> => {
      let params;
      try {
        params = await pairWithHost(host.host, host.port, host.fingerprint, code);
      } catch (err) {
        return { ok: false, error: String(err) };
      }
      const existing = (latestStateRef.current?.remoteProfiles ?? []).find(
        (p) => p.host === params.host && p.port === params.port,
      );
      const id =
        existing?.id ??
        `remote-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
      const profile: RemoteProfile = {
        id,
        name: existing?.name ?? host.name ?? params.host,
        host: params.host,
        port: params.port,
        token: params.token,
        fingerprint: params.fingerprint,
      };
      try {
        await saveRemoteProfile(profile);
      } catch (err) {
        return { ok: false, error: `saving profile: ${String(err)}` };
      }
      const profiles = await listRemoteProfiles().catch(
        () => [] as RemoteProfile[],
      );
      setState((s) => ({ ...s, remoteProfiles: profiles }));
      onSwitchConnection({ kind: "remote", profileId: id });
      return { ok: true };
    },
    [onSwitchConnection],
  );

  /// Forget a saved remote host. If it was the active target, fall back to the
  /// local daemon (which also tears the bridge down + reconnects).
  const onDeleteRemoteProfile = useCallback(
    (id: string) => {
      const wasActive =
        latestStateRef.current?.connectionTarget.kind === "remote" &&
        latestStateRef.current.connectionTarget.profileId === id;
      void deleteRemoteProfile(id)
        .catch((err: unknown) => {
          logToFile("warn", `deleteRemoteProfile failed: ${String(err)}`);
        })
        .finally(() => {
          void listRemoteProfiles()
            .then((profiles) =>
              setState((s) => ({ ...s, remoteProfiles: profiles })),
            )
            .catch(() => {
              /* best-effort refresh */
            });
        });
      if (wasActive) {
        onSwitchConnection({ kind: "local" });
      }
    },
    [onSwitchConnection],
  );

  const activeSessionCount = useMemo(
    () =>
      state.sessions.filter((s) => s.status !== "stopped" && !s.is_orphan)
        .length,
    [state.sessions],
  );
  const activeWorktreeSessionCount = useMemo(
    () =>
      state.sessions.filter(
        (s) =>
          s.status !== "stopped" &&
          !s.is_orphan &&
          s.has_per_session_worktree,
      ).length,
    [state.sessions],
  );
  // Orphans are non-stopped sessions whose PTY handle was lost across a
  // daemon restart. Counted separately so the exit-confirm dialog can
  // explain why "Stop sessions and quit" is a no-op for them (the daemon's
  // an orphan needs a best-effort sidecar kill rather than a live PTY
  // handle. See ux-audit "No active
  // sessions message hides orphan sessions".
  const orphanSessionCount = useMemo(
    () =>
      state.sessions.filter((s) => s.status !== "stopped" && s.is_orphan)
        .length,
    [state.sessions],
  );

  const subscribePty = useCallback(
    (sessionId: string, cb: (b64: string) => void) => {
      const map = ptyListenersRef.current;
      let set = map.get(sessionId);
      if (!set) {
        set = new Set();
        map.set(sessionId, set);
      }
      set.add(cb);
      return () => {
        const s = map.get(sessionId);
        if (!s) return;
        s.delete(cb);
        if (s.size === 0) map.delete(sessionId);
      };
    },
    [],
  );

  const activeTab = useMemo(
    () => state.tabs.find((t) => t.id === state.activeTabId) ?? null,
    [state.tabs, state.activeTabId],
  );

  const isRemote = state.connectionTarget.kind === "remote";
  const activeRemoteProfile = useMemo(() => {
    const target = state.connectionTarget;
    if (target.kind !== "remote") return null;
    return (
      state.remoteProfiles.find((p) => p.id === target.profileId) ?? null
    );
  }, [state.connectionTarget, state.remoteProfiles]);
  // Footer label for the active target: a remote host's name (or a generic
  // "remote" until its profile loads), or null for local (keeps the desktop
  // footer visually unchanged).
  const connectionLabel = isRemote
    ? (activeRemoteProfile?.name ?? "remote")
    : null;

  /// Dynamic OS-window title for the main window. Pop-out windows manage
  /// their own titles via PaneWindow / TabWindow / SessionWindow, so we
  /// only touch this when no popout query param is set. The actual
  /// `setTitle` call is debounced so rapid working ↔ idle oscillation
  /// during a streaming response doesn't flicker the OS taskbar entry.
  const windowTitle = useMemo(
    () => computeWindowTitle(activeTab, state.sessions, settings.title),
    [activeTab, state.sessions, settings.title],
  );
  useEffect(() => {
    if (popoutPaneId || popoutSessionId || popoutTabId) return;
    const handle = window.setTimeout(() => {
      void getCurrentWebviewWindow().setTitle(windowTitle);
    }, 350);
    return () => window.clearTimeout(handle);
  }, [windowTitle]);

  const spawnCurrentTab = useMemo(() => {
    const targetTabId =
      spawnTargetPaneRef.current?.tabId ??
      spawnTargetTabRef.current ??
      activeTab?.id ??
      null;
    return state.tabs.find((t) => t.id === targetTabId) ?? null;
  }, [state.spawnOpen, state.tabs, activeTab]);

  const canUseSpawnCurrentTab =
    spawnCurrentTab !== null && tabGrid(spawnCurrentTab) !== null;

  /// Tabs eligible for the placement picker's per-tab radios. Excludes
  /// non-grid tabs (e.g. diff tabs) since those can't host terminal
  /// panes — picking one would silently fall back to a fresh tab and
  /// confuse the user about why their explicit choice was ignored.
  const spawnPlacementTabs = useMemo(
    () => state.tabs.filter((t) => tabGrid(t) !== null),
    [state.tabs],
  );

  // Set of session ids referenced by the active tab — used by the sidebar for
  // visual selection state.
  const sessionIdsInActiveTab = useMemo(() => {
    if (!activeTab) return new Set<string>();
    const grid = tabGrid(activeTab);
    if (!grid) return new Set<string>();
    return new Set(
      collectPanes(grid)
        .map((p) => p.session_id)
        .filter((id): id is string => id !== null),
    );
  }, [activeTab]);

  const highlightedSessionIds = useMemo(() => {
    const ids = new Set(sessionIdsInActiveTab);
    for (const id of state.spotlightSessionIds) {
      ids.add(id);
    }
    return ids;
  }, [sessionIdsInActiveTab, state.spotlightSessionIds]);

  // Which sidebar panel is showing (Sessions vs Source control). Persisted
  // to localStorage so it survives reloads.
  const [activitySection, setActivitySection] = useState<ActivitySection>(
    () => readActivitySection(),
  );
  // Whether the sidebar pane is collapsed. Clicking the active activity
  // button toggles; clicking a different section while collapsed expands
  // and switches. Ctrl+B also toggles. Persisted to localStorage.
  const [sidebarCollapsed, setSidebarCollapsed] = useState<boolean>(
    () => readSidebarCollapsed(),
  );
  const onSelectActivity = useCallback(
    (next: ActivitySection) => {
      if (next === activitySection) {
        setSidebarCollapsed((c) => {
          const nextCollapsed = !c;
          writeSidebarCollapsed(nextCollapsed);
          return nextCollapsed;
        });
        return;
      }
      setActivitySection(next);
      writeActivitySection(next);
      if (sidebarCollapsed) {
        setSidebarCollapsed(false);
        writeSidebarCollapsed(false);
      }
    },
    [activitySection, sidebarCollapsed],
  );
  const onToggleSidebar = useCallback(() => {
    setSidebarCollapsed((c) => {
      const nextCollapsed = !c;
      writeSidebarCollapsed(nextCollapsed);
      return nextCollapsed;
    });
  }, []);

  const onSelectPresetLaunchSessions = useCallback((sessionIds: string[]) => {
    const wanted = new Set(sessionIds);
    setActivitySection("sessions");
    writeActivitySection("sessions");
    setState((s) => ({
      ...s,
      spotlightSessionIds: new Set(
        s.sessions.filter((session) => wanted.has(session.id)).map((session) => session.id),
      ),
    }));
  }, []);

  const onStopPresetLaunchSessions = useCallback((sessionIds: string[]) => {
    const cur = latestStateRef.current;
    const client = cur?.client;
    if (!client) return;
    const wanted = new Set(sessionIds);
    const launched = cur.sessions.filter(
      (session) => wanted.has(session.id) && session.status !== "stopped",
    );
    for (const session of launched) {
      client.send({
        type: "stop_session",
        session_id: session.id,
        cleanup: session.members.map((member) => ({
          repo_id: member.repo_id,
          remove_worktree: false,
          branch: "auto" as const,
        })),
      });
    }
    pushToast(setState, {
      severity: "info",
      message:
        launched.length === 0
          ? "No running launched sessions"
          : launched.length === 1
          ? "Stopping launched session"
          : `Stopping ${launched.length} launched sessions`,
      detail:
        launched.length === 0
          ? "The launched session records are already stopped."
          : "The session records stay visible after the processes stop.",
    });
  }, []);

  // Resolve the focused pane's session for session-scoped controls. Diff tabs
  // carry their repo id directly and are handled in `focusedRepoId`.
  const focusedSession = useMemo(() => {
    if (!activeTab || !state.focusedPaneId) return null;
    const grid = tabGrid(activeTab);
    if (!grid) return null;
    const pane = collectPanes(grid).find(
      (p) => p.pane_id === state.focusedPaneId,
    );
    if (!pane?.session_id) return null;
    return state.sessions.find((s) => s.id === pane.session_id) ?? null;
  }, [activeTab, state.focusedPaneId, state.sessions]);

  // Source-control activity-bar badge. Scoped to whichever session is
  // currently focused: single-repo sessions sum their one member; workspace
  // sessions union across every member. When nothing is focused, fall back
  // to the total across all registered repos so the badge still surfaces
  // unflagged changes the user might want to address.
  const sourceControlBadgeTotal = useMemo(() => {
    if (focusedSession) {
      let total = 0;
      for (const m of focusedSession.members) {
        total += state.repoChangeCounts[m.repo_id] ?? 0;
      }
      return total;
    }
    let total = 0;
    for (const count of Object.values(state.repoChangeCounts)) {
      total += count;
    }
    return total;
  }, [focusedSession, state.repoChangeCounts]);

  // Derive the focused repo id for the source-control sidebar. Diff tabs keep
  // following the repo they were opened from, so selecting a diff does not make
  // the sidebar fall back to another registered repo.
  const focusedRepoId = useMemo(() => {
    if (activeTab?.content.kind === "diff") {
      return activeTab.content.repo_id;
    }
    return focusedSession?.members[0]?.repo_id ?? null;
  }, [activeTab, focusedSession]);

  // Session id of the focused pane (or null). Reused by keyboard
  // shortcuts that update session-scoped appearance overrides.
  const focusedSessionId = useMemo<string | null>(
    () => focusedSession?.id ?? null,
    [focusedSession],
  );

  // Drop font-size overrides for sessions / tabs that no longer exist.
  // Runs whenever the live id sets shift — typically once per
  // session-close or tab-close. Keeps the localStorage maps from growing
  // forever.
  useEffect(() => {
    pruneOverrides(
      new Set(state.tabs.map((t) => t.id)),
      new Set(state.sessions.map((s) => s.id)),
    );
  }, [state.tabs, state.sessions]);

  // App-level keyboard shortcuts. Skip when a modal is open so the
  // user's typing in a dialog input doesn't fire a tab cycle.
  const anyModalOpen =
    state.spawnOpen ||
    state.presetLaunch !== null ||
    state.workspaceCreatorOpen ||
    state.exitConfirmOpen ||
    state.repoRemove !== null ||
    state.settingsOpen ||
    state.vscodeQueue.length > 0 ||
    state.worktreeCleanupQueue.length > 0 ||
    state.deleteWorktreeRequest !== null ||
    state.exitWorktreeQueue !== null;
  const shortcuts = useMemo<KeyboardShortcut[]>(() => {
    if (
      anyModalOpen ||
      popoutPaneId !== null ||
      popoutTabId !== null ||
      popoutSessionId !== null
    ) {
      return [];
    }
    const list: KeyboardShortcut[] = [];
    // Ctrl+B: toggle the sidebar pane. Matches the VSCode chord.
    // Always available — works even before the daemon is connected.
    list.push({ key: "b", handler: onToggleSidebar });
    if (state.client) {
      list.push({
        key: "t",
        handler: () =>
          state.client?.send({
            type: "create_tab",
            name: null,
            initial_session_id: null,
          }),
      });
    }
    if (state.repos.length > 0) {
      list.push({ key: "n", handler: () => onOpenSpawn() });
    }
    // Ctrl/Cmd+, opens the Settings modal — same chord every macOS
    // app uses, and `,` doesn't collide with anything else we bind.
    list.push({ key: ",", handler: onOpenSettings });
    if (state.tabs.length > 1) {
      const cycle = (dir: 1 | -1) => {
        const ids = state.tabs.map((t) => t.id);
        const cur = state.activeTabId
          ? ids.indexOf(state.activeTabId)
          : -1;
        const next = (cur + dir + ids.length) % ids.length;
        const target = ids[next];
        if (target) onActivateTab(target);
      };
      list.push({ key: "Tab", handler: () => cycle(1) });
      list.push({ key: "Tab", shift: true, handler: () => cycle(-1) });
    }
    // Ctrl+1..9: activate tab at index N-1 (1-indexed for users).
    for (let i = 1; i <= 9; i++) {
      const idx = i - 1;
      const target = state.tabs[idx];
      if (!target) break;
      list.push({
        key: String(i),
        handler: () => onActivateTab(target.id),
      });
    }
    // Ctrl+Shift+G: rearrange the active tab into an auto-grid. Only
    // bound when the active tab has 2+ panes — a single pane has
    // nothing to rearrange.
    const activeForRearrange = state.tabs.find(
      (t) => t.id === state.activeTabId,
    );
    if (activeForRearrange && state.client) {
      const grid = tabGrid(activeForRearrange);
      const paneCount = grid ? collectPanes(grid).length : 0;
      if (paneCount >= 2) {
        list.push({
          key: "g",
          shift: true,
          handler: () =>
            state.client?.send({
              type: "rearrange_tab",
              tab_id: activeForRearrange.id,
              layout: { kind: "grid", cols: 0 },
            }),
        });
      }
    }
    // Ctrl+= / Ctrl+- adjust the focused session's appearance font size by 1.
    // Ctrl+Shift+= / Ctrl+Shift+- adjust the active tab's override
    // (a view-level override applied after appearance inheritance).
    // Ctrl+0 resets the focused session font-size override and the
    // active tab override back to inheritance.
    //
    // We bind both "=" (the unshifted plus key on US layout) and "+"
    // so the shortcut works regardless of whether the user holds
    // shift to type the plus sign.
    const bumpSessionFont = (delta: number) => {
      if (!focusedSession || !state.client) return;
      const cur = resolveAppearance(
        settingsRef.current,
        state.repos,
        state.workspaces,
        focusedSession,
      ).values.terminal_font_size;
      const next = Math.max(MIN_FONT_SIZE, Math.min(MAX_FONT_SIZE, cur + delta));
      state.client.send({
        type: "set_session_appearance",
        session_id: focusedSession.id,
        appearance: {
          ...focusedSession.appearance,
          terminal_font_size: next,
        },
      });
    };
    const bumpTabFont = (delta: number) => {
      if (!state.activeTabId) return;
      const appFontSize = resolveAppearanceLayers(
        null,
        null,
        settings.appearance,
      ).values.terminal_font_size;
      const cur =
        getTabFontSize(state.activeTabId) ??
        resolveFontSize(state.activeTabId, appFontSize);
      const next = Math.max(MIN_FONT_SIZE, Math.min(MAX_FONT_SIZE, cur + delta));
      setTabFontSize(state.activeTabId, next);
    };
    if (focusedSessionId !== null) {
      list.push({ key: "=", handler: () => bumpSessionFont(1) });
      list.push({ key: "+", handler: () => bumpSessionFont(1) });
      list.push({ key: "-", handler: () => bumpSessionFont(-1) });
    }
    if (state.activeTabId) {
      list.push({ key: "=", shift: true, handler: () => bumpTabFont(1) });
      list.push({ key: "+", shift: true, handler: () => bumpTabFont(1) });
      list.push({ key: "-", shift: true, handler: () => bumpTabFont(-1) });
    }
    // Ctrl+0: reset the focused session override AND the active tab
    // override, falling everything back to the app default.
    if (focusedSessionId !== null || state.activeTabId) {
      list.push({
        key: "0",
        handler: () => {
          if (focusedSession && state.client) {
            state.client.send({
              type: "set_session_appearance",
              session_id: focusedSession.id,
              appearance: {
                ...focusedSession.appearance,
                terminal_font_size: null,
              },
            });
          }
          if (state.activeTabId) {
            clearTabFontSize(state.activeTabId);
          }
        },
      });
    }
    return list;
  }, [
    anyModalOpen,
    state.client,
    state.repos,
    state.workspaces,
    state.tabs,
    state.activeTabId,
    focusedSession,
    focusedSessionId,
    settings.appearance,
    onActivateTab,
    onOpenSpawn,
    onOpenSettings,
    onToggleSidebar,
  ]);
  useKeyboardShortcuts(shortcuts);

  if (popoutPaneId) {
    const binding = findPaneBinding(state.tabs, popoutPaneId);
    const popoutSession =
      binding?.session_id === null || binding === null
        ? null
        : (state.sessions.find((s) => s.id === binding.session_id) ?? null);
    return (
      <div className="app-root">
        {state.client && binding && popoutSession ? (
          <PaneWindow
            session={popoutSession}
            tab={binding.tab}
            paneId={popoutPaneId}
            tabs={state.tabs}
            repos={state.repos}
            workspaces={state.workspaces}
            client={state.client}
            subscribePty={subscribePty}
          />
        ) : (
          <div className="empty-state">
            <h1>Pane not found</h1>
            <p className="status-line">
              Daemon: <ConnectionBadge state={state.status} />
            </p>
            <p className="hint">
              Pane <code>{popoutPaneId}</code> is not currently docked in any
              tab with a live session.
            </p>
          </div>
        )}
      </div>
    );
  }

  if (popoutTabId) {
    const popoutTab = state.tabs.find((t) => t.id === popoutTabId);
    return (
      <div className="app-root">
        {state.client && popoutTab ? (
          <TabWindow
            tab={popoutTab}
            client={state.client}
            sessions={state.sessions}
            repos={state.repos}
            workspaces={state.workspaces}
            subscribePty={subscribePty}
            hasRepos={state.repos.length > 0}
          />
        ) : (
          <div className="empty-state">
            <h1>Tab not found</h1>
            <p className="status-line">
              Daemon: <ConnectionBadge state={state.status} />
            </p>
            <p className="hint">
              Tab <code>{popoutTabId}</code> is not currently registered with
              the daemon.
            </p>
          </div>
        )}
      </div>
    );
  }

  if (popoutSessionId) {
    const popoutSession = state.sessions.find((s) => s.id === popoutSessionId);
    return (
      <div className="app-root">
        {state.client && popoutSession ? (
          <SessionWindow
            session={popoutSession}
            repos={state.repos}
            workspaces={state.workspaces}
            client={state.client}
            subscribePty={subscribePty}
          />
        ) : (
          <div className="empty-state">
            <h1>Session not found</h1>
            <p className="status-line">
              Daemon: <ConnectionBadge state={state.status} />
            </p>
            <p className="hint">
              Session id <code>{popoutSessionId}</code> is not currently
              registered with the daemon.
            </p>
          </div>
        )}
      </div>
    );
  }

  // Cold-boot gate: block the whole UI until the WS has reached `open`
  // at least once this app lifetime. Without this, the user can click
  // Spawn (and many other actions) while the daemon is still warming up
  // and the click silently no-ops because the gating dialogs require
  // state.client to be non-null. After first connect, transient drops
  // fall back to the DaemonFooter + in-app reconnect UX.
  if (
    !state.hasEverConnected
    && state.status.kind !== "auth_failed"
    && state.status.kind !== "error"
  ) {
    return (
      <ConnectingOverlay status={state.status} onRestart={onRestartDaemon} />
    );
  }

  const mainPaneContent = (
    <main className="main-pane">
      <TabBar
        tabs={state.tabs}
        activeTabId={state.activeTabId}
        client={state.client!}
        sessions={state.sessions}
        onActivate={onActivateTab}
        onArmNextNewTab={onArmNextNewTab}
        onLocalReorder={onLocalReorder}
        onTabWillClose={onTabWillClose}
        onTabsSnapshotUndo={onTabsSnapshotUndo}
      />
      {activeTab && state.client ? (
        activeTab.content.kind === "diff" ? (
          <DiffPane
            client={state.client}
            repoId={activeTab.content.repo_id}
            path={activeTab.content.path}
            against={activeTab.content.against}
          />
        ) : (
          <GridRenderer
            tab={activeTab}
            tabs={state.tabs}
            client={state.client}
            sessions={state.sessions}
            repos={state.repos}
            workspaces={state.workspaces}
            subscribePty={subscribePty}
            focusedPaneId={state.focusedPaneId}
            onFocusPane={onFocusPane}
            onSpawnInPane={onSpawnInPane}
            hasRepos={state.repos.length > 0}
            onArmNextNewTab={onArmNextNewTab}
            onArmFocusNewPane={onArmFocusNewPane}
            onTabsSnapshotUndo={onTabsSnapshotUndo}
          />
        )
      ) : (
        <EmptyState
          onOpenSpawn={() => onOpenSpawn()}
          onLaunchShell={onLaunchStandaloneShellDefault}
          onAddRepo={onAddRepo}
          hasRepos={state.repos.length > 0}
          hasTabs={state.tabs.length > 0}
        />
      )}
    </main>
  );

  return (
    <RemoteModeContext.Provider value={isRemote}>
    <div className="app-root" data-testid="app-root">
      <ActivityBar
        active={activitySection}
        collapsed={sidebarCollapsed}
        onSelect={onSelectActivity}
        sourceControlBadge={sourceControlBadgeTotal}
      />
      <ResizableSplit
        storageKey="root.sidebar"
        defaultSize={280}
        minSize={200}
        direction="horizontal"
        firstHidden={sidebarCollapsed}
      >
        {activitySection === "sessions" ? (
          <Sidebar
            repos={state.repos}
            workspaces={state.workspaces}
            sessions={state.sessions}
            client={state.client!}
            highlightedSessionIds={highlightedSessionIds}
            attentionSessions={state.attentionSessions}
            connection={state.status}
            handshake={state.handshake}
            daemonStopRequested={state.daemonStopRequested}
            onRestartDaemon={onRestartDaemon}
            onStopDaemon={onStopDaemon}
            isRemote={isRemote}
            connectionLabel={connectionLabel}
            onOpenConnections={() =>
              setState((s) => ({ ...s, connectionPickerOpen: true }))
            }
            onLocalReorderTabs={onLocalReorder}
            containerOrder={state.containerOrder}
            onLocalReorderContainers={onLocalReorderContainers}
            sessionOrder={state.sessionOrder}
            onLocalReorderSessions={onLocalReorderSessions}
            tabs={state.tabs}
            onAddRepo={onAddRepo}
            onAddRepoPath={onAddRepoPath}
            onRemoveRepo={(id) =>
              state.client?.send({ type: "remove_repo", repo_id: id })
            }
            onRemoveRepoWithLiveSessions={onRemoveRepoWithLiveSessions}
            onRemoveWorkspace={(id) =>
              state.client?.send({ type: "remove_workspace", workspace_id: id })
            }
            onSelectSession={onSelectSession}
            onOpenSpawn={onOpenSpawn}
            onOpenSpawnIntoTab={onOpenSpawnIntoTab}
            onLaunchLast={onLaunchLast}
            onEditLastSpawn={onEditLastSpawn}
            activeTabId={state.activeTabId}
            onArmNextNewTab={onArmNextNewTab}
            onDuplicateSession={onDuplicateSession}
            onDuplicateSessionWithDialog={onDuplicateSessionWithDialog}
            onOpenWorkspaceCreator={onOpenWorkspaceCreator}
            standaloneShellDefaultDir={
              settings.spawn.standalone_shell_default_dir
            }
            onLaunchStandaloneShellDefault={onLaunchStandaloneShellDefault}
            onOpenStandaloneShellDialog={onOpenStandaloneShellDialog}
            onRevealInExplorer={onRevealInExplorer}
            onLaunchPreset={onLaunchPreset}
            onOpenSettings={onOpenSettings}
          />
        ) : (
          <SourceControlSidebar
            repos={state.repos}
            focusedRepoId={focusedRepoId}
            focusedSession={focusedSession}
            client={state.client!}
            onActivateTab={onActivateTab}
          />
        )}
        {mainPaneContent}
      </ResizableSplit>
      {state.spawnOpen && state.client && (
        <SpawnDialog
          repos={state.repos}
          workspaces={state.workspaces}
          client={state.client}
          initialTarget={state.spawnInitial}
          lockInitialTarget={state.spawnInitialLocked}
          spawnPrefill={state.spawnPrefill}
          spawnPin={state.spawnPin}
          canUseCurrentTab={canUseSpawnCurrentTab}
          currentTabName={
            canUseSpawnCurrentTab ? (spawnCurrentTab?.name ?? null) : null
          }
          tabs={spawnPlacementTabs}
          activeTabId={spawnCurrentTab?.id ?? null}
          onClose={onCloseSpawn}
          onSpawned={onSpawned}
          onSpawnRequest={onSpawnRequest}
          onAddRepo={onAddRepo}
        />
      )}
      {state.checkoutConfirm && (
        <CheckoutConfirmModal
          branch={state.checkoutConfirm.branch}
          dirtyCount={state.checkoutConfirm.dirty_count}
          onChoose={onCheckoutConfirm}
          onCancel={() => setState((s) => ({ ...s, checkoutConfirm: null }))}
        />
      )}
      {state.standaloneShellOpen && (
        <StandaloneShellDialog
          initialPath={
            state.standaloneShellInitial ??
            settings.spawn.standalone_shell_default_dir
          }
          onClose={onCloseStandaloneShellDialog}
          onSubmit={onSubmitStandaloneShell}
        />
      )}
      {state.presetLaunch && state.client && (
        <PresetLaunchDialog
          preset={state.presetLaunch.preset}
          target={state.presetLaunch.target}
          repos={state.repos}
          client={state.client}
          onClose={onClosePresetLaunch}
          onSelectSessions={onSelectPresetLaunchSessions}
          onStopSessions={onStopPresetLaunchSessions}
        />
      )}
      {state.workspaceCreatorOpen && state.client && (
        <WorkspaceCreator
          repos={state.repos}
          client={state.client}
          onClose={onCloseWorkspaceCreator}
        />
      )}
      {!isRemote && state.vscodeQueue[0] && state.client && (
        <VscodeSuggestionToast
          suggestion={state.vscodeQueue[0]}
          client={state.client}
          onDismiss={onDismissVscodeSuggestion}
        />
      )}
      {state.worktreeCleanupQueue[0] && (
        <WorktreeCleanupFailedDialog
          sessionId={state.worktreeCleanupQueue[0].session_id}
          sessionLabel={state.worktreeCleanupQueue[0].session_label}
          failures={state.worktreeCleanupQueue[0].failures}
          onKillAndRetry={(kill_pids, targets) => {
            const entry = state.worktreeCleanupQueue[0];
            if (!entry) return;
            state.client?.send({
              type: "retry_worktree_cleanup",
              session_id: entry.session_id,
              kill_pids,
              targets,
            });
            // Drop the head entry — a re-emitted failure will push a
            // fresh entry back onto the queue if retry didn't fully
            // succeed.
            setState((s) => ({
              ...s,
              worktreeCleanupQueue: s.worktreeCleanupQueue.slice(1),
            }));
          }}
          onIgnore={() => {
            setState((s) => ({
              ...s,
              worktreeCleanupQueue: s.worktreeCleanupQueue.slice(1),
            }));
          }}
        />
      )}
      {state.actionFailed && (
        <ActionFailedModal
          info={state.actionFailed}
          onDismiss={() => setState((s) => ({ ...s, actionFailed: null }))}
        />
      )}
      {state.settingsOpen && (
        <SettingsModal
          settings={settings}
          onClose={onCloseSettings}
          client={state.client}
          worktreesRoot={state.worktreesRoot}
          lanStatus={state.lanStatus}
          daemonToken={state.daemonToken}
          onOpenWorktreesManager={() =>
            setState((s) => ({ ...s, worktreesManagerOpen: true }))
          }
        />
      )}
      {state.layoutChooser && (
        <LayoutChooser
          hasLegacy={state.layoutChooser.has_legacy}
          activeSessionCount={state.layoutChooser.active_session_count}
          clonable={state.layoutChooser.clonable}
          onChoose={(kind, arrangement) => {
            // Stash any chosen arrangement before sending so the `tabs`
            // handler can apply it once the daemon replies.
            if (arrangement) pendingArrangementRef.current = arrangement;
            clientRef.current?.send({ type: "init_layout", kind });
            // Optimistically close; the daemon replies with `tabs`, which also
            // clears this. Closing now avoids a flash if the reply is slow.
            setState((s) => ({ ...s, layoutChooser: null }));
          }}
        />
      )}
      {state.importArrangement && (
        <ImportArrangementModal
          incomingCount={state.importArrangement.incoming_count}
          localCount={state.importArrangement.local_count}
          onKeep={() =>
            setState((s) => ({ ...s, importArrangement: null }))
          }
          onEmpty={() => {
            clientRef.current?.send({ type: "init_layout", kind: { kind: "empty" } });
            setState((s) => ({ ...s, importArrangement: null }));
          }}
          onImport={(layout, maxPerTab) => {
            pendingArrangementRef.current = { layout, maxPerTab };
            clientRef.current?.send({
              type: "init_layout",
              kind: { kind: "all_sessions" },
            });
            setState((s) => ({ ...s, importArrangement: null }));
          }}
        />
      )}
      {state.connectionPickerOpen && (
        <ConnectionPicker
          target={state.connectionTarget}
          profiles={state.remoteProfiles}
          onSelectLocal={() => onSwitchConnection({ kind: "local" })}
          onSelectProfile={(id) =>
            onSwitchConnection({ kind: "remote", profileId: id })
          }
          onAddRemote={onAddRemote}
          onPairHost={onPairHost}
          onDeleteProfile={onDeleteRemoteProfile}
          onClose={() =>
            setState((s) => ({ ...s, connectionPickerOpen: false }))
          }
        />
      )}
      {state.worktreesManagerOpen && (
        <WorktreesManagerModal
          client={state.client}
          worktreesRoot={state.worktreesRoot}
          onClose={() =>
            setState((s) => ({ ...s, worktreesManagerOpen: false }))
          }
          onLaunch={onLaunchIntoWorktree}
        />
      )}
      {state.exitConfirmOpen && (
        <ExitConfirmDialog
          activeSessionCount={activeSessionCount}
          activeWorktreeSessionCount={activeWorktreeSessionCount}
          orphanSessionCount={orphanSessionCount}
          busy={state.exitInFlight}
          escapeDisabled={state.exitWorktreeQueue !== null}
          stuck={state.exitStuck}
          onStopAndQuit={onStopAndQuit}
          onStopAndQuitRemoveWorktrees={onStopAndQuitRemoveWorktrees}
          onAbandonAndQuit={onAbandonAndQuit}
          onQuitLeaveRunning={onQuitLeaveRunning}
          onForceQuit={onForceQuit}
          onCancel={onExitCancel}
        />
      )}
      {state.exitWorktreeQueue && exitWorktreeSession && state.client && (
        <DeleteWorktreeDialog
          key={exitWorktreeSession.id}
          session={exitWorktreeSession}
          client={state.client}
          progress={exitProgress(state.exitWorktreeQueue)}
          layer="exit-queue"
          onCancel={onCancelExitWorktree}
          onConfirm={onConfirmExitWorktree}
        />
      )}
      <ErrorToast toasts={state.toasts} onDismiss={onDismissToast} />
      <UndoShelf
        entries={state.undoEntries}
        onUndo={onUndo}
        onDismiss={onDismissUndo}
      />
      {deleteWorktreeSession && state.client && (
        <DeleteWorktreeDialog
          session={deleteWorktreeSession}
          client={state.client}
          onCancel={onCancelDeleteWorktree}
          onConfirm={onConfirmDeleteWorktree}
        />
      )}
      {state.repoRemove && (
        <RepoRemoveDialog
          repoName={state.repoRemove.repoName}
          liveSessions={state.repoRemove.liveSessions}
          onCancel={onCancelRepoRemove}
          onRemoveAnyway={onRepoRemoveAnyway}
          onStopAndRemove={onRepoStopAndRemove}
        />
      )}
    </div>
    </RemoteModeContext.Provider>
  );
}

function loadActiveTab(): string | null {
  try {
    return localStorage.getItem(ACTIVE_TAB_KEY);
  } catch {
    return null;
  }
}

function newUndoId(): string {
  return `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
}

function cloneTab(tab: TabEntry): TabEntry {
  return JSON.parse(JSON.stringify(tab)) as TabEntry;
}

function undoTouches(entry: UndoEntry, tabIds: Set<string>): boolean {
  return entry.tabs.some((snapshot) => tabIds.has(snapshot.tab.id));
}

function uniqueTabs(tabs: TabEntry[]): TabEntry[] {
  const seen = new Set<string>();
  const next: TabEntry[] = [];
  for (const tab of tabs) {
    if (seen.has(tab.id)) {
      continue;
    }
    seen.add(tab.id);
    next.push(tab);
  }
  return next;
}

/// The two spawn targets "launch last again" can replay. A standalone shell
/// has no repo to fork from, so it reopens the dialog instead.
type LaunchLastSpawnTarget = Exclude<SpawnTarget, { kind: "standalone" }>;

/// A launch-last replay held while the daemon picks a branch name for its
/// worktree. `intent` is resolved when the user clicks, not when the reply
/// lands, so a "current tab" launch goes to the tab they were looking at.
/// `timer` is the give-up timeout that drops the entry.
interface PendingLaunchLast {
  config: SpawnConfig;
  target: LaunchLastSpawnTarget;
  intent: PendingSpawnIntent;
  timer: number;
}

/// Where a "launch last again" gesture puts the session it is about to ask
/// for. `current_tab` and a named tab both fall back to a fresh tab when the
/// tab in question can't host panes (it holds a diff or a single full-bleed
/// view), which is also what happens when the named tab is already gone.
function resolveLaunchIntent(
  tabs: TabEntry[],
  activeTabId: string | null,
  placement: LaunchLastPlacement,
): PendingSpawnIntent {
  if (placement.kind === "new_tab") return { kind: "newTab" };
  if (placement.kind === "tab") {
    const targetTab = tabs.find((t) => t.id === placement.tabId);
    const canHost = targetTab ? tabGrid(targetTab) !== null : false;
    return canHost
      ? { kind: "addToTab", tabId: placement.tabId }
      : { kind: "newTab" };
  }
  const activeTab = tabs.find((t) => t.id === activeTabId);
  const canHost = activeTab ? tabGrid(activeTab) !== null : false;
  return activeTab && canHost
    ? { kind: "addToTab", tabId: activeTab.id }
    : { kind: "newTab" };
}

type PendingSpawnIntent =
  | { kind: "newTab"; discardSessionId?: string }
  | {
      kind: "replacePane";
      tabId: string;
      paneId: string;
      discardSessionId?: string;
    }
  | { kind: "addToTab"; tabId: string; discardSessionId?: string }
  | null;

/// Add the freshly spawned session to the existing `tabId`. Prefers a pane
/// beside sessions from the same repo/workspace; when a split is needed,
/// targets the largest pane and bisects along its longer axis so the
/// resulting layout stays balanced (a 2+1 column grid becomes a 2x2 grid
/// on the 4th pane instead of slicing the top-left in two).
function sendAddSessionToTab(
  client: DaemonClient | null,
  tabId: string,
  session: SessionSnapshot,
  latestStateRef: React.MutableRefObject<AppState | null>,
) {
  if (!client) return;
  const tabs = latestStateRef.current?.tabs ?? [];
  const tab = tabs.find((t) => t.id === tabId);
  const grid = tab ? tabGrid(tab) : null;
  if (!grid) return;
  const sessions = latestStateRef.current?.sessions ?? [];
  const target = paneTargetForSession(grid, sessions, session);
  if (!target) return;
  if (target.kind === "replace") {
    client.send({
      type: "replace_pane_session",
      tab_id: tabId,
      pane_id: target.paneId,
      session_id: session.id,
    });
    return;
  }
  client.send({
    type: "split_pane",
    tab_id: tabId,
    pane_id: target.paneId,
    direction: target.direction,
    place: target.place,
    new_session_id: session.id,
  });
}

type PanePlacementTarget =
  | { kind: "replace"; paneId: string }
  | {
      kind: "split";
      paneId: string;
      direction: SplitDirection;
      place: SplitPlace;
    };

function paneTargetForSession(
  grid: GridNode,
  sessions: SessionSnapshot[],
  session: SessionSnapshot,
): PanePlacementTarget | null {
  const panes = collectPanes(grid);
  const sessionKey = sessionParentKey(session);
  if (sessionKey) {
    const sessionById = new Map(sessions.map((s) => [s.id, s] as const));
    const matchingIndexes: number[] = [];
    for (let i = 0; i < panes.length; i += 1) {
      const sessionId = panes[i]?.session_id;
      if (!sessionId) continue;
      const existing = sessionById.get(sessionId);
      if (existing && sessionParentKey(existing) === sessionKey) {
        matchingIndexes.push(i);
      }
    }
    for (const index of matchingIndexes) {
      const after = panes[index + 1];
      if (after?.session_id === null) {
        return { kind: "replace", paneId: after.pane_id };
      }
      const before = panes[index - 1];
      if (before?.session_id === null) {
        return { kind: "replace", paneId: before.pane_id };
      }
    }
    const lastMatch = matchingIndexes.at(-1);
    if (lastMatch !== undefined) {
      const pane = panes[lastMatch];
      if (pane) {
        // Split the matching pane along its longer axis so the new sibling
        // doesn't squeeze a tall column into a skinny strip (or a wide row
        // into a flat sliver). Falls back to horizontal when the rect lookup
        // misses, which can only happen if the tree shifted between snapshot
        // and split — the daemon will reject the split anyway in that case.
        const rect = paneRect(grid, pane.pane_id);
        const direction: SplitDirection = rect
          ? balanceSplitDirection(rect)
          : "horizontal";
        return {
          kind: "split",
          paneId: pane.pane_id,
          direction,
          place: "second",
        };
      }
    }
  }

  const emptyPane = panes.find((p) => p.session_id === null);
  if (emptyPane) {
    return { kind: "replace", paneId: emptyPane.pane_id };
  }
  const balanced = pickBalancedSplitTarget(grid);
  if (!balanced) return null;
  return {
    kind: "split",
    paneId: balanced.paneId,
    direction: balanced.direction,
    place: balanced.place,
  };
}

function sessionParentKey(session: SessionSnapshot): string | null {
  if (session.workspace_id) return `workspace:${session.workspace_id}`;
  const primaryRepo = session.members[0]?.repo_id;
  return primaryRepo ? `repo:${primaryRepo}` : null;
}

function handleMessage(
  msg: DaemonMessage,
  setState: React.Dispatch<React.SetStateAction<AppState>>,
  ptyListeners: Map<string, Set<(b64: string) => void>>,
  clientRef: React.MutableRefObject<DaemonClient | null>,
  seenSessionIdsRef: React.MutableRefObject<Set<string>>,
  sessionStatusRef: React.MutableRefObject<Map<string, SessionSnapshot["status"]>>,
  pendingSpawnIntentRef: React.MutableRefObject<PendingSpawnIntent>,
  pendingArrangementRef: React.MutableRefObject<ArrangementConfig | null>,
  pendingSessionImportRef: React.MutableRefObject<{
    incoming_count: number;
    local_count: number;
  } | null>,
  pendingPaneFocusRef: React.MutableRefObject<
    { tabId: string; knownPaneIds: Set<string> } | null
  >,
  latestStateRef: React.MutableRefObject<AppState | null>,
) {
  switch (msg.type) {
    case "welcome":
      return;
    case "auth_failed":
      setState((s) => ({
        ...s,
        status: { kind: "auth_failed", reason: msg.reason },
      }));
      return;
    case "repos": {
      const incoming = msg.repos;
      // Fire-and-forget initial `repo_status` for repos we haven't seen
      // before. The daemon's filesystem watcher only broadcasts on file
      // changes, so without this seed the activity-bar badge would stay
      // empty until the user makes their first change.
      const known = latestStateRef.current?.repoChangeCounts ?? {};
      for (const r of incoming) {
        if (!(r.id in known)) {
          clientRef.current?.send({ type: "repo_status", repo_id: r.id });
        }
      }
      setState((s) => {
        const incomingIds = new Set(incoming.map((r) => r.id));
        const nextCounts: Record<string, number> = {};
        for (const [id, count] of Object.entries(s.repoChangeCounts)) {
          if (incomingIds.has(id)) nextCounts[id] = count;
        }
        return { ...s, repos: incoming, repoChangeCounts: nextCounts };
      });
      return;
    }
    case "workspaces":
      setState((s) => ({ ...s, workspaces: msg.workspaces }));
      return;
    case "sessions": {
      // Initial snapshot. Seed the seen-set so a subsequent session_updated
      // for one of these ids is recognized as not-new, and the status map so
      // the self-exit detector has a baseline.
      const incoming = msg.sessions;
      for (const s of incoming) {
        seenSessionIdsRef.current.add(s.id);
        sessionStatusRef.current.set(s.id, s.status);
      }
      // On remote connections, stash the count mismatch so the `tabs` handler
      // can offer the import-arrangement modal. The `layout_init_required`
      // handler clears this if LayoutChooser will handle layout init instead.
      const localCount = latestStateRef.current?.sessions.length ?? 0;
      const isRemote =
        latestStateRef.current?.connectionTarget.kind === "remote";
      if (isRemote && incoming.length > 0 && incoming.length !== localCount) {
        pendingSessionImportRef.current = {
          incoming_count: incoming.length,
          local_count: localCount,
        };
      }
      setState((s) => ({ ...s, sessions: incoming }));
      return;
    }
    case "session_updated": {
      // Decide one-shot follow-up BEFORE setState so the state updater stays
      // pure (StrictMode double-invokes updaters in dev). The seen-set is the
      // source of truth for "is this a new session id" — relying on state in
      // the reducer would mis-fire if the daemon emits the same id back-to-back
      // (which it currently does after a spawn).
      const session = msg.session;
      const isNew = !seenSessionIdsRef.current.has(session.id);
      if (isNew) seenSessionIdsRef.current.add(session.id);
      const intent = isNew ? pendingSpawnIntentRef.current : null;
      // Detect self-exit: status just transitioned to "stopped" from an
      // active state with a child exit code, AND the session is not already
      // parked (is_inactive covers the park_session feedback loop). Explicit
      // Stop first emits a retained stopped snapshot without an exit code, so
      // it does not take the auto-discard path for no-worktree sessions. The
      // previous status comes from sessionStatusRef, NOT latestStateRef: the
      // ref map is updated synchronously per message, so the exit watcher's
      // follow-up snapshot (with exit code) always compares against the
      // explicit stop's "stopped" even when both arrive in the same render
      // window.
      const prevStatus = sessionStatusRef.current.get(session.id);
      sessionStatusRef.current.set(session.id, session.status);
      const justSelfExited =
        !isNew &&
        session.status === "stopped" &&
        session.exit_code !== null &&
        !session.is_inactive &&
        prevStatus !== undefined &&
        prevStatus !== "stopped" &&
        prevStatus !== "error";
      if (intent) {
        pendingSpawnIntentRef.current = null;
        // The Terminal mount for this session id is several React
        // re-renders away (we still need a `create_tab` / `tab_updated`
        // round-trip for newTab+addToTab, and a `replace_pane_session`
        // round-trip for replacePane). Stash the id in the autofocus
        // queue so when its Terminal finally mounts, the xterm helper
        // textarea grabs OS keyboard focus and the user can type
        // immediately instead of having to click into the pane.
        markForAutoFocus(session.id);
      }
      setState((s) => {
        const idx = s.sessions.findIndex((sn) => sn.id === session.id);
        const next = s.sessions.slice();
        if (idx === -1) next.push(session);
        else next[idx] = session;
        // Auto-clear the attention flag when the session has transitioned
        // back into a calm running state on its own. Stopped/Error still
        // count as attention-worthy (the daemon's Attention event fires
        // for those, and the user should acknowledge them), so we only
        // clear on awaiting_input → working/idle/spawning self-resolves.
        const calmAgain =
          session.status === "working" ||
          session.status === "idle" ||
          session.status === "spawning";
        let attention = s.attentionSessions;
        if (calmAgain && attention.has(session.id)) {
          attention = new Set(attention);
          attention.delete(session.id);
        }
        if (intent?.kind === "newTab") {
          return {
            ...s,
            sessions: next,
            attentionSessions: attention,
            pendingTabActivate: s.pendingTabActivate + 1,
          };
        }
        if (intent?.kind === "replacePane") {
          // Activate the target tab and focus the pane locally; the
          // `replace_pane_session` send below tells the daemon to attach
          // the session, which will round-trip as a `tab_updated`.
          return {
            ...s,
            sessions: next,
            attentionSessions: attention,
            activeTabId: intent.tabId,
            focusedPaneId: intent.paneId,
            focusedPaneByTab: {
              ...s.focusedPaneByTab,
              [intent.tabId]: intent.paneId,
            },
          };
        }
        if (intent?.kind === "addToTab") {
          // Activate the target tab locally; the send below attaches the
          // session to either an empty pane or a fresh split, and the
          // daemon's `tab_updated` reconciles the actual grid topology.
          return {
            ...s,
            sessions: next,
            attentionSessions: attention,
            activeTabId: intent.tabId,
          };
        }
        return { ...s, sessions: next, attentionSessions: attention };
      });
      // Auto-discard sessions that exited without a worktree (nothing to keep).
      if (justSelfExited && !session.has_per_session_worktree) {
        clientRef.current?.send({
          type: "discard_session",
          session_id: session.id,
          cleanup: [],
        });
      }
      if (intent?.kind === "newTab") {
        clientRef.current?.send({
          type: "create_tab",
          name: null,
          initial_session_id: session.id,
        });
      } else if (intent?.kind === "replacePane") {
        clientRef.current?.send({
          type: "replace_pane_session",
          tab_id: intent.tabId,
          pane_id: intent.paneId,
          session_id: session.id,
        });
      } else if (intent?.kind === "addToTab") {
        sendAddSessionToTab(clientRef.current, intent.tabId, session, latestStateRef);
      }
      // Deferred predecessor discard (Restart / spawn-into-pane): sent after
      // the placement messages so the daemon rebinds the pane to the new
      // session before `close_session_panes` runs for the old one. A failed
      // spawn never reaches this point, so the stopped session survives it.
      if (intent?.discardSessionId) {
        clientRef.current?.send({
          type: "discard_session",
          session_id: intent.discardSessionId,
          cleanup: [],
        });
      }
      return;
    }
    case "session_removed":
      // Daemon prunes pane references and broadcasts tab_updated separately;
      // here we update the session list AND any per-session sets that
      // could otherwise leak the dead id (attentionSessions in particular —
      // without this, a stopped-then-removed session keeps its warning
      // glyph on whatever container would otherwise show it).
      seenSessionIdsRef.current.delete(msg.session_id);
      sessionStatusRef.current.delete(msg.session_id);
      setState((s) => {
        let attention = s.attentionSessions;
        if (attention.has(msg.session_id)) {
          attention = new Set(attention);
          attention.delete(msg.session_id);
        }
        return {
          ...s,
          sessions: s.sessions.filter((sn) => sn.id !== msg.session_id),
          attentionSessions: attention,
        };
      });
      return;
    case "checkout_confirm_required":
      setState((s) => ({
        ...s,
        checkoutConfirm: {
          repo_id: msg.repo_id,
          branch: msg.branch,
          dirty_count: msg.dirty_count,
        },
      }));
      return;
    case "layout_init_required":
      // LayoutChooser handles init — the session-count stash is not needed.
      pendingSessionImportRef.current = null;
      setState((s) => ({
        ...s,
        layoutChooser: {
          has_legacy: msg.has_legacy,
          active_session_count: msg.active_session_count,
          clonable: msg.clonable,
        },
      }));
      return;
    case "tabs": {
      // Apply any pending arrangement chosen from LayoutChooser or
      // ImportArrangementModal (set before `init_layout all_sessions` was sent).
      const pendingArr = pendingArrangementRef.current;
      if (pendingArr) {
        pendingArrangementRef.current = null;
        const firstTab = msg.tabs[0];
        if (firstTab?.content.kind === "grid") {
          const allPanes = collectPanes(firstTab.content.grid);
          const maxPerTab = pendingArr.maxPerTab ?? allPanes.length;
          // Extract groups that exceed `maxPerTab` into new tabs.
          for (
            let start = maxPerTab;
            start < allPanes.length;
            start += maxPerTab
          ) {
            const batch = allPanes
              .slice(start, start + maxPerTab)
              .map((p) => p.pane_id);
            clientRef.current?.send({
              type: "extract_to_new_tab",
              source_tab_id: firstTab.id,
              pane_ids: batch,
              name: null,
              layout: pendingArr.layout,
            });
          }
          // Rearrange the first tab (may have fewer panes after extractions).
          clientRef.current?.send({
            type: "rearrange_tab",
            tab_id: firstTab.id,
            layout: pendingArr.layout,
          });
        }
      }
      // If this `tabs` is the initial reconnect push (not a reply to
      // `init_layout`), promote any stashed session-count mismatch into the
      // import-arrangement modal.
      const sessionImport = pendingSessionImportRef.current;
      pendingSessionImportRef.current = null;
      const showImport = sessionImport !== null && pendingArr === null;
      setState((s) => {
        const ids = new Set(msg.tabs.map((t) => t.id));
        const active =
          s.activeTabId && ids.has(s.activeTabId)
            ? s.activeTabId
            : (msg.tabs[0]?.id ?? null);
        const activeTab = active
          ? msg.tabs.find((t) => t.id === active)
          : undefined;
        const focusedPaneId = active
          ? resolveTabFocus(activeTab, s.focusedPaneByTab[active])
          : null;
        // Prune memory for tabs the daemon no longer reports.
        const focusedPaneByTab: Record<string, string> = {};
        for (const [tabId, paneId] of Object.entries(s.focusedPaneByTab)) {
          if (ids.has(tabId)) focusedPaneByTab[tabId] = paneId;
        }
        return {
          ...s,
          tabs: msg.tabs,
          activeTabId: active,
          focusedPaneId,
          focusedPaneByTab,
          // The daemon answered the first-connect chooser (if one was open).
          layoutChooser: null,
          importArrangement: showImport ? sessionImport : s.importArrangement,
        };
      });
      return;
    }
    case "tab_updated":
      setState((s) => {
        const idx = s.tabs.findIndex((t) => t.id === msg.tab.id);
        const isNew = idx === -1;
        const next = s.tabs.slice();
        if (isNew) next.push(msg.tab);
        else next[idx] = msg.tab;
        let active = s.activeTabId;
        let pendingTabActivate = s.pendingTabActivate;
        if (isNew && (pendingTabActivate > 0 || active === null)) {
          active = msg.tab.id;
          // Consume one arm. If the user mashed Spawn twice in a row,
          // each resulting new tab gets the activation it asked for
          // instead of only the first.
          if (pendingTabActivate > 0) pendingTabActivate -= 1;
        }
        let focusedPaneByTab = s.focusedPaneByTab;
        // Post-split focus: if a split armed pendingPaneFocusRef for this
        // tab, diff the new grid against the snapshot and focus the new
        // pane. Disarms after the first matching update so a stale arm
        // (e.g. user cancelled mentally and never split again, then later
        // pane updates fire for unrelated reasons) doesn't yank focus.
        let focusedPaneId = s.focusedPaneId;
        const arm = pendingPaneFocusRef.current;
        if (arm && arm.tabId === msg.tab.id) {
          const grid = tabGrid(msg.tab);
          if (grid) {
            const fresh = collectPanes(grid).find(
              (p) => !arm.knownPaneIds.has(p.pane_id),
            );
            if (fresh) {
              focusedPaneId = fresh.pane_id;
              focusedPaneByTab = {
                ...focusedPaneByTab,
                [msg.tab.id]: fresh.pane_id,
              };
            }
          }
          pendingPaneFocusRef.current = null;
        }
        // If this is the active tab, re-resolve focus against the new
        // grid: pane closes / moves can invalidate the previously-focused
        // pane id, and we want to fall back to the first leaf rather than
        // sit on a stale id.
        if (active === msg.tab.id) {
          const remembered = focusedPaneByTab[msg.tab.id] ?? focusedPaneId ?? undefined;
          const resolved = resolveTabFocus(msg.tab, remembered ?? undefined);
          focusedPaneId = resolved;
          if (resolved) {
            focusedPaneByTab = {
              ...focusedPaneByTab,
              [msg.tab.id]: resolved,
            };
          } else if (msg.tab.id in focusedPaneByTab) {
            const { [msg.tab.id]: _gone, ...rest } = focusedPaneByTab;
            focusedPaneByTab = rest;
          }
        }
        return {
          ...s,
          tabs: next,
          activeTabId: active,
          pendingTabActivate,
          focusedPaneId,
          focusedPaneByTab,
        };
      });
      return;
    case "tab_removed":
      setState((s) => {
        const next = s.tabs.filter((t) => t.id !== msg.tab_id);
        const active =
          s.activeTabId === msg.tab_id
            ? (next[0]?.id ?? null)
            : s.activeTabId;
        const { [msg.tab_id]: _gone, ...focusedPaneByTab } = s.focusedPaneByTab;
        let focusedPaneId = s.focusedPaneId;
        if (active !== s.activeTabId) {
          const activeTab = active
            ? next.find((t) => t.id === active)
            : undefined;
          focusedPaneId = active
            ? resolveTabFocus(activeTab, focusedPaneByTab[active])
            : null;
        }
        return {
          ...s,
          tabs: next,
          activeTabId: active,
          focusedPaneId,
          focusedPaneByTab,
        };
      });
      return;
    case "tabs_reordered":
      setState((s) => {
        const byId = new Map(s.tabs.map((t) => [t.id, t] as const));
        const next = msg.ordered_ids
          .map((id) => byId.get(id))
          .filter((t): t is TabEntry => t !== undefined);
        return { ...s, tabs: next };
      });
      return;
    case "containers_reordered":
      setState((s) => ({ ...s, containerOrder: msg.ordered }));
      return;
    case "sessions_reordered": {
      const { container_id, ordered_ids } = msg;
      setState((s) => {
        const next = new Map(s.sessionOrder);
        next.set(container_id, ordered_ids);
        return { ...s, sessionOrder: next };
      });
      return;
    }
    case "pty_output": {
      const set = ptyListeners.get(msg.session_id);
      if (set) for (const cb of set) cb(msg.data_b64);
      return;
    }
    case "vscode_workspace_suggestion":
      setState((s) => ({
        ...s,
        vscodeQueue: [...s.vscodeQueue, msg.suggestion],
      }));
      return;
    case "attention": {
      setState((s) => {
        const next = new Set(s.attentionSessions);
        next.add(msg.session_id);
        return { ...s, attentionSessions: next };
      });
      const session = findSession(setState, msg.session_id);
      const title =
        msg.reason === "awaiting_input"
          ? "Claude is awaiting input"
          : msg.reason === "error"
            ? "Claude session errored"
            : "Claude session stopped";
      // Honour the per-reason toggles from Settings. Loaded fresh on
      // each attention event so a live toggle takes effect immediately
      // without round-tripping through React state. Cheap — a
      // localStorage.getItem + JSON.parse.
      const prefs = loadSettings().notifications;
      const enabled =
        msg.reason === "awaiting_input"
          ? prefs.awaiting_input
          : msg.reason === "error"
            ? prefs.error
            : prefs.stopped;
      if (enabled) {
        void (async () => {
          const granted = await isPermissionGranted();
          if (!granted) return;
          sendNotification({
            title,
            body: session?.label ?? "rustling-tulip",
          });
        })();
      }
      window.dispatchEvent(new CustomEvent("rt:attention", { detail: msg }));
      return;
    }
    case "worktrees_root_changed":
      setState((s) => ({
        ...s,
        worktreesRoot: { path: msg.root, isOverride: msg.is_override },
      }));
      window.dispatchEvent(
        new CustomEvent(`rt:${msg.type}`, { detail: msg }),
      );
      return;
    case "lan_status":
      setState((s) => ({
        ...s,
        lanStatus: {
          enabled: msg.enabled,
          port: msg.port,
          fingerprint: msg.fingerprint,
          addresses: msg.addresses,
        },
      }));
      return;
    case "pairing_started":
    case "pairing_ended":
      // Host-side pairing lifecycle. Ephemeral — the Remote access settings
      // panel subscribes via the side-channel event to show/clear the code.
      window.dispatchEvent(new CustomEvent(`rt:${msg.type}`, { detail: msg }));
      return;
    case "worktrees_root_snapshot":
      // Ephemeral — no AppState slot. The WorktreesManager modal
      // subscribes via the side-channel event.
      window.dispatchEvent(
        new CustomEvent(`rt:${msg.type}`, { detail: msg }),
      );
      return;
    case "worktree_cleanup_failed":
      // Queue the failure so the modal can show it. Multiple discards
      // in quick succession just stack here; the modal dequeues one at
      // a time. A second failure for the same session id replaces the
      // queued entry (the latest retry result is the relevant one).
      setState((s) => {
        const queue = s.worktreeCleanupQueue.filter(
          (e) => e.session_id !== msg.session_id,
        );
        queue.push({
          session_id: msg.session_id,
          session_label: msg.session_label,
          failures: msg.failures,
        });
        return { ...s, worktreeCleanupQueue: queue };
      });
      return;
    case "action_failed":
      // A refused/failed action (worktree-in-use spawn, blocked discard).
      // Disarm any pending spawn intent — a rejected spawn must not leave
      // stale "open in new tab" routing armed for the next one — then raise
      // the blocking modal.
      pendingSpawnIntentRef.current = null;
      setState((s) => ({
        ...s,
        actionFailed: {
          title: msg.title,
          detail: msg.detail,
          hint: msg.hint,
        },
      }));
      return;
    case "branches":
    case "worktrees":
    case "discard_preview":
    case "branch_name_suggestion":
    case "workspace_spawn_preview":
    case "spawn_preview":
    case "repo_fetched":
    case "commits":
    case "commit_detail":
    case "file_diff":
    case "remote_url":
    case "repo_status":
    case "scrollback":
    case "spawn_config_reply":
    case "presets":
    case "preset_launch_job_updated":
    case "preset_launch_failed":
    case "preset_preview":
    case "preset_preview_error":
    case "preset_scripts_resolved":
    case "preset_scripts_error":
    case "commit_ok":
    case "git_write_error":
    case "diff_tab_opened":
    case "file_snapshot":
    case "file_snapshot_error":
    case "stashes":
      window.dispatchEvent(
        new CustomEvent(`rt:${msg.type}`, { detail: msg }),
      );
      if (msg.type === "git_write_error") {
        pushToast(setState, {
          severity: "error",
          message: `Git ${msg.operation} failed`,
          detail: msg.error,
        });
      }
      if (msg.type === "repo_status") {
        const repoId = msg.repo_id;
        const count = msg.index_changes.length + msg.worktree_changes.length;
        setState((s) =>
          s.repoChangeCounts[repoId] === count
            ? s
            : { ...s, repoChangeCounts: { ...s.repoChangeCounts, [repoId]: count } },
        );
      }
      if (msg.type === "preset_launch_job_updated") {
        pushPresetJobToast(setState, msg.job);
      }
      if (msg.type === "preset_launch_failed") {
        const partials = `${msg.partial_session_ids.length} session(s), ${msg.partial_tab_ids.length} tab(s) partial`;
        pushToast(setState, {
          key: `preset:${msg.job_id}`,
          severity: "warning",
          message: `Preset '${msg.preset_id}' launch failed`,
          detail: `${msg.error} · ${partials}`,
        });
      }
      return;
    case "preset_launch_progress": {
      // Only auto-activate on the FIRST tab created during this launch.
      // Previously every progress tick switched the active tab as new
      // tabs were created — which hijacked the user's focus mid-launch
      // (audit: "Auto-activate-tab during preset launch hijacks user
      // focus"). The first tab gives the user a useful "here's what's
      // happening" view; after that, the launch runs silently with the
      // progress toast keeping them informed.
      if (msg.current_tab_id !== null && msg.tab_ids.length === 1) {
        const targetTabId = msg.current_tab_id;
        setState((s) =>
          s.tabs.some((t) => t.id === targetTabId) && s.activeTabId !== targetTabId
            ? { ...s, activeTabId: targetTabId }
            : s,
        );
      }
      const done = msg.launched >= msg.total;
      pushToast(setState, {
        key: `preset:${msg.job_id}`,
        severity: "info",
        message: done
          ? `Preset '${msg.preset_id}' launched`
          : `Launching preset '${msg.preset_id}'`,
        detail: `${msg.launched} / ${msg.total} session${msg.total === 1 ? "" : "s"}`,
        sticky: !done,
      });
      window.dispatchEvent(
        new CustomEvent("rt:preset_launch_progress", { detail: msg }),
      );
      return;
    }
    case "error":
      // Surface to the user via toast AND disarm any pending spawn intent.
      // Without the second step a daemon-rejected spawn would leave the
      // intent armed, so the NEXT spawn would inherit the stale "open in
      // a new tab" / "replace pane" routing.
      pendingSpawnIntentRef.current = null;
      pushToast(setState, {
        severity: "error",
        message: "Daemon error",
        detail: msg.message,
      });
      return;
    // Routed to component-level `client.onMessage` subscribers rather than
    // handled here. They still have to be named: without a case they fall
    // through to the forward-compat tail below and log as unknown on every
    // occurrence, which reads as a protocol gap during triage when it isn't.
    case "shutdown_ack": // App.tsx's own daemon-shutdown flow
    case "vscode_workspaces_scanned": // Sidebar.tsx
    case "vscode_workspace_parsed": // WorkspaceCreator.tsx
      return;
  }
  // Forward-compat: a v(N+1) daemon may emit message types this v(N) client
  // doesn't know about. The Rust side has InboundClientMessage::Unknown as
  // its parse-boundary equivalent; on this side, exhaustiveness narrows msg
  // to `never` after the switch, but at runtime an unknown discriminator
  // simply falls through. Log it instead of silently dropping.
  const unknown = msg as unknown as { type?: string };
  logToFile(
    "warn",
    `unknown daemon message type: ${unknown.type ?? "<missing>"}`,
  );
}

function pushToast(
  setState: React.Dispatch<React.SetStateAction<AppState>>,
  toast: Omit<ToastEntry, "id">,
) {
  const id = `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  setState((s) => {
    // Honor the optional dedup key: when a toast with the same key already
    // exists, REPLACE it in place (preserving its React id so there's no
    // remount / animation reset) and bump `generation` so the auto-dismiss
    // timer restarts. Otherwise append.
    if (toast.key) {
      const existingIdx = s.toasts.findIndex((t) => t.key === toast.key);
      if (existingIdx >= 0) {
        const existing = s.toasts[existingIdx]!;
        const next = s.toasts.slice();
        next[existingIdx] = {
          ...existing,
          ...toast,
          id: existing.id,
          generation: (existing.generation ?? 0) + 1,
        };
        return { ...s, toasts: next };
      }
    }
    return { ...s, toasts: [...s.toasts, { id, ...toast, generation: 1 }] };
  });
}

function pushPresetJobToast(
  setState: React.Dispatch<React.SetStateAction<AppState>>,
  job: PresetLaunchJobSnapshot,
) {
  const terminal = isTerminalPresetJob(job);
  const severity = job.status === "failed" ? "warning" : "info";
  pushToast(setState, {
    key: `preset:${job.job_id}`,
    severity,
    message: presetJobToastMessage(job),
    detail: presetJobToastDetail(job),
    sticky: !terminal,
  });
}

function isTerminalPresetJob(job: PresetLaunchJobSnapshot): boolean {
  return (
    job.status === "completed" ||
    job.status === "cancelled" ||
    job.status === "failed"
  );
}

function presetJobToastMessage(job: PresetLaunchJobSnapshot): string {
  switch (job.status) {
    case "completed":
      return `Preset '${job.preset_id}' launched`;
    case "cancelled":
      return `Preset '${job.preset_id}' cancelled`;
    case "failed":
      return `Preset '${job.preset_id}' launch failed`;
    case "spawning":
      return `Launching preset '${job.preset_id}'`;
    case "running_scripts":
      return `Running preset '${job.preset_id}' scripts`;
    case "resolving":
      return `Preparing preset '${job.preset_id}'`;
    case "unknown":
      return `Preset '${job.preset_id}' updated`;
  }
}

function presetJobToastDetail(job: PresetLaunchJobSnapshot): string {
  const sessions = `${job.launched} / ${job.total} session${job.total === 1 ? "" : "s"}`;
  if (job.status === "failed" && job.error) {
    return `${job.error} · ${sessions}`;
  }
  return sessions;
}

function findSession(
  setState: React.Dispatch<React.SetStateAction<AppState>>,
  id: string,
): SessionSnapshot | null {
  let found: SessionSnapshot | null = null;
  setState((s) => {
    found = s.sessions.find((sn) => sn.id === id) ?? null;
    return s;
  });
  return found;
}

/// Cold-boot full-screen overlay shown until the daemon WS reaches
/// `open` for the first time this app lifetime. Replaces every other
/// surface so spawn/tab/preset clicks can't silently no-op while the
/// daemon is still warming up. Once the user has been connected once,
/// the overlay never returns — transient drops fall back to the
/// DaemonFooter's in-app status display.
function ConnectingOverlay({
  status,
  onRestart,
}: {
  status: AppState["status"];
  onRestart: () => void;
}) {
  const label = status.kind === "init"
    ? "Starting daemon…"
    : status.kind === "connecting"
      ? "Connecting to daemon…"
      : status.kind === "closed"
        ? "Daemon dropped — reconnecting…"
        : "Waiting for daemon…";
  return (
    <div className="connecting-overlay" data-testid="connecting-overlay">
      <div className="connecting-overlay-card">
        <div className="connecting-overlay-spinner" aria-hidden="true" />
        <div className="connecting-overlay-label">{label}</div>
        <button
          type="button"
          className="link connecting-overlay-restart"
          onClick={onRestart}
          data-testid="connecting-overlay-restart"
        >
          Restart daemon
        </button>
      </div>
    </div>
  );
}

/// Used only by the pop-out tab/session "not found" error states (which
/// render outside of the main sidebar layout, so they have no
/// DaemonFooter to defer to). The main-window EmptyState delegates
/// daemon-status display entirely to the footer.
function ConnectionBadge({ state }: { state: AppState["status"] }) {
  if (state.kind === "init") return <span className="badge badge-warn">starting</span>;
  if (state.kind === "open") return <span className="badge badge-ok">connected</span>;
  if (state.kind === "connecting") return <span className="badge badge-warn">connecting</span>;
  if (state.kind === "auth_failed")
    return <span className="badge badge-err" title={state.reason}>auth failed</span>;
  if (state.kind === "error")
    return <span className="badge badge-err" title={state.reason}>error</span>;
  return <span className="badge badge-warn" title={state.reason}>disconnected</span>;
}

function EmptyState({
  onOpenSpawn,
  onLaunchShell,
  onAddRepo,
  hasRepos,
  hasTabs,
}: {
  onOpenSpawn: () => void;
  onLaunchShell: () => void;
  onAddRepo: () => void;
  hasRepos: boolean;
  hasTabs: boolean;
}) {
  // Daemon connection state used to render here as a `<ConnectionBadge>`,
  // but the DaemonFooter at the bottom of the sidebar now owns that
  // surface (single source of truth). When the daemon is unhealthy the
  // user gets the same dot + reason in the persistent footer, and the
  // empty pane stays focused on "what should I do next?".
  return (
    <div className="empty-state" data-testid="empty-state">
      <h1>rustling-tulip</h1>
      {hasTabs ? (
        <p className="hint">Select a tab above.</p>
      ) : hasRepos ? (
        <button
          type="button"
          onClick={onOpenSpawn}
          className="primary"
          data-testid="empty-state-spawn"
        >
          Spawn a session
        </button>
      ) : (
        <>
          <p className="hint">
            No repos registered yet.
          </p>
          <div className="empty-state-actions">
            <button
              type="button"
              onClick={onLaunchShell}
              className="primary"
              data-testid="empty-state-open-shell"
            >
              Open shell
            </button>
            <button
              type="button"
              onClick={onAddRepo}
              data-testid="empty-state-add-repo"
            >
              + Add repo
            </button>
          </div>
        </>
      )}
    </div>
  );
}
