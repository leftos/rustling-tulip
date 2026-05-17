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
  ensureDaemonStarted,
  pickDirectory,
  requestSpawnConfig,
  stopDaemon,
  type ConnectionState,
  type DaemonClient,
  type DaemonHandshake,
} from "./api";
import {
  tabGrid,
  type ContainerRef,
  type DaemonMessage,
  type PresetEntry,
  type PresetLaunchJobSnapshot,
  type PresetTarget,
  type RepoEntry,
  type SessionSnapshot,
  type SpawnConfig,
  type TabEntry,
  type VscodeWorkspaceSuggestion,
  type WorkspaceEntry,
} from "./types";
import ActivityBar, {
  type ActivitySection,
  readActivitySection,
  writeActivitySection,
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
import StandaloneShellDialog from "./components/StandaloneShellDialog";
import PresetLaunchDialog from "./components/PresetLaunchDialog";
import WorkspaceCreator from "./components/WorkspaceCreator";
import VscodeSuggestionToast from "./components/VscodeSuggestionToast";
import ErrorToast, { type ToastEntry } from "./components/ErrorToast";
import RepoRemoveDialog from "./components/RepoRemoveDialog";
import type { RepoRemoveIntent } from "./components/Sidebar";
import ResizableSplit from "./components/ResizableSplit";
import ExitConfirmDialog from "./components/ExitConfirmDialog";
import SettingsModal from "./components/SettingsModal";
import TabBar from "./components/TabBar";
import GridRenderer from "./components/GridRenderer";
import DiffPane from "./components/DiffPane";
import TabWindow from "./components/TabWindow";
import UndoShelf, { type UndoShelfEntry } from "./components/UndoShelf";
import { markForAutoFocus } from "./utils/autofocus";
import { logToFile } from "./utils/logger";
import { randomWorktreeBranchName } from "./utils/randomName";
import {
  collectPanes,
  findPaneBinding,
  findTabContainingSession,
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
import { sessionDisplayLabel } from "./utils/sessionLabel";
import { computeWindowTitle } from "./utils/windowTitle";

/// Pop-out windows: launched with either `?pane=<id>` (pane-backed pop-out),
/// `?tab=<id>` (per-tab pop-out), or `?session=<id>` (legacy single-session
/// pop-out). The branch in App renders the corresponding standalone window
/// surface without sidebar/modals.
const queryParams = new URLSearchParams(window.location.search);
const popoutPaneId = queryParams.get("pane");
const popoutTabId = queryParams.get("tab");
const popoutSessionId = queryParams.get("session");

const ACTIVE_TAB_KEY = "rt:active-tab:main";
const UNDO_TTL_MS = 8000;
const UNDO_LIMIT = 3;

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
  /// Full SpawnConfig to seed the spawn dialog from (used by "Duplicate
  /// session → Shift-click → open dialog pre-filled"). When set, the
  /// dialog hydrates agent, run mode, trusted-launch state, model, permission mode,
  /// codex sandbox, and extra env vars from this config — overriding the
  /// usual Settings defaults. `undefined` for normal spawns.
  spawnPrefill: SpawnConfig | undefined;
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
    spawnPrefill: undefined,
    pendingTabActivate: 0,
    workspaceCreatorOpen: false,
    vscodeQueue: [],
    attentionSessions: new Set(),
    exitConfirmOpen: false,
    exitInFlight: false,
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
  });
  // Bumped to force the connection useEffect to re-run (e.g. when the
  // user clicks Restart from the DaemonFooter while the daemon is in
  // the "stopped" state and the auto-reconnect cycle has been suppressed).
  const [connectionVersion, setConnectionVersion] = useState(0);
  // Mirror of `state.daemonStopRequested` for use inside the connection
  // effect's closure (which captures values at mount time).
  const stopRequestedRef = useRef(false);
  stopRequestedRef.current = state.daemonStopRequested;

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
  /// One-shot follow-up for the next freshly-spawned session id.
  ///   - `newTab`     : open a fresh tab containing the session.
  ///   - `replacePane`: drop the session into a specific empty pane (set when
  ///                    the user opened the spawn dialog from that pane's
  ///                    "+ spawn" button).
  ///   - `addToTab`   : add the session to an existing tab.
  /// Consumed by the `session_updated` handler the first time it sees an
  /// unseen session id.
  const pendingSpawnIntentRef = useRef<
    | { kind: "newTab" }
    | { kind: "replacePane"; tabId: string; paneId: string }
    | { kind: "addToTab"; tabId: string }
    | null
  >(null);

  /// Captured when the spawn dialog is opened from an empty pane's "+ spawn"
  /// button. Read at `onSpawned` to route current-tab placement into that pane.
  /// Cleared on dialog close so a manual reopen from the toolbar doesn't
  /// inherit a stale target.
  const spawnTargetPaneRef = useRef<{ tabId: string; paneId: string } | null>(
    null,
  );

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

    /// Connect (or reconnect) and wire all subscriptions. Called on mount
    /// and from the close handler with exponential backoff.
    const connect = async () => {
      try {
        const handshake = await ensureDaemonStarted();
        if (cancelled) return;
        client = connectDaemon(handshake);
        clientRef.current = client;
        client.onConnectionChange((next) => {
          setState((s) => ({ ...s, status: next }));
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
            pendingSpawnIntentRef,
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
        // ensureDaemonStarted failure (e.g. supervisor can't spawn the
        // daemon) — record the error AND schedule a retry. The supervisor
        // may need a moment to clean up a stale daemon.json. Exception:
        // when the user has just hit Stop, don't retry — they asked for
        // the daemon to be down.
        const reason = String(err);
        setState((s) => ({ ...s, status: { kind: "error", reason } }));
        if (!cancelled && !stopRequestedRef.current) {
          const delay = Math.min(10_000, 500 * 2 ** reconnectAttempt);
          reconnectAttempt += 1;
          logToFile(
            "warn",
            `daemon supervisor failed (${reason}); retrying in ${delay}ms (attempt ${reconnectAttempt})`,
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
    const path = await pickDirectory(undefined, { lastDirKey: "addRepo" });
    if (!path) return;
    state.client?.send({ type: "add_repo", path, name: null });
  }, [state.client]);

  const onAddRepoPath = useCallback(
    (path: string) => {
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
    // Resolution order:
    //   1. If the session is already shown in some tab → activate that tab
    //      (and focus its pane). No new tab, no daemon round-trip.
    //   2. Else if the active tab has a focused empty pane → fill that pane.
    //   3. Else → open the session in a fresh tab.
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
    let openInNewTab = true;
    let bindUndoEntry: UndoEntry | null = null;
    const activeTab = s.tabs.find((t) => t.id === s.activeTabId);
    const activeGrid = activeTab ? tabGrid(activeTab) : null;
    if (activeTab && activeGrid) {
      const panes = collectPanes(activeGrid);
      const focusedPane = panes.find((p) => p.pane_id === s.focusedPaneId);
      if (focusedPane && focusedPane.session_id === null) {
        const session = s.sessions.find((candidate) => candidate.id === sessionId);
        bindUndoEntry = {
          id: newUndoId(),
          kind: "restore_tabs",
          message: session
            ? `Bound session "${sessionDisplayLabel(session)}"`
            : "Bound session",
          actionLabel: "Undo",
          tabs: [
            {
              tab: cloneTab(activeTab),
              index: Math.max(
                0,
                s.tabs.findIndex((existingTab) => existingTab.id === activeTab.id),
              ),
              restoreActive: true,
              restoreFocusedPaneId: focusedPane.pane_id,
            },
          ],
          expiresAt: Date.now() + UNDO_TTL_MS,
        };
        client.send({
          type: "replace_pane_session",
          tab_id: activeTab.id,
          pane_id: focusedPane.pane_id,
          session_id: sessionId,
        });
        openInNewTab = false;
      }
    }
    if (openInNewTab) {
      client.send({
        type: "create_tab",
        name: null,
        initial_session_id: sessionId,
      });
    }
    setState((cur) => {
      const next = new Set(cur.attentionSessions);
      next.delete(sessionId);
      const nextState = openInNewTab
        ? {
            ...cur,
            attentionSessions: next,
            pendingTabActivate: cur.pendingTabActivate + 1,
            spotlightSessionIds: new Set<string>(),
          }
        : { ...cur, attentionSessions: next, spotlightSessionIds: new Set<string>() };
      if (!bindUndoEntry) {
        return nextState;
      }
      const affectedTabIds = new Set(
        bindUndoEntry.tabs.map((snapshot) => snapshot.tab.id),
      );
      return {
        ...nextState,
        undoEntries: [
          bindUndoEntry,
          ...cur.undoEntries.filter(
            (existingUndo) => !undoTouches(existingUndo, affectedTabIds),
          ),
        ].slice(0, UNDO_LIMIT),
      };
    });
  }, []);

  const onSpawnInPane = useCallback(
    (paneId: string) => {
      // Record the (tab, pane) the user clicked from so the spawn follow-up
      // can route the new session into that pane via `replace_pane_session`
      // instead of opening a fresh tab.
      const cur = latestStateRef.current;
      if (cur?.activeTabId) {
        spawnTargetPaneRef.current = { tabId: cur.activeTabId, paneId };
      }
      setState((s) => ({
        ...s,
        spawnOpen: true,
        spawnInitial: undefined,
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
      spawnPrefill: undefined,
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

  /// "Launch last again" from the sidebar (double-click on a container or
  /// the matching context-menu submenu). Reads the targeted repo/workspace's
  /// `last_spawn_config` and replays it. When no config is saved yet, falls
  /// back to opening the spawn dialog pre-targeted to the container.
  ///
  /// Branch policy: for `use_worktree === true` we regenerate `branch_name`
  /// with a fresh `wt/<adjective>-<noun>` so each relaunch lands on its own
  /// branch instead of colliding with the still-running prior session.
  /// `use_worktree === false` reuses the saved branch verbatim — that mode
  /// targets a real branch (e.g. `main`, `feature/x`) by the user's choice.
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
      if (config.target.kind === "standalone") {
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
      const nextTarget = {
        ...config.target,
        branch_name: config.target.use_worktree
          ? randomWorktreeBranchName()
          : config.target.branch_name,
      };
      const activeTab = s.tabs.find((t) => t.id === s.activeTabId);
      const activeTabCanHost = activeTab ? tabGrid(activeTab) !== null : false;
      let intent: PendingSpawnIntent;
      if (placement.kind === "new_tab") {
        intent = { kind: "newTab" };
      } else if (placement.kind === "tab") {
        const targetTab = s.tabs.find((t) => t.id === placement.tabId);
        const canHost = targetTab ? tabGrid(targetTab) !== null : false;
        intent = canHost
          ? { kind: "addToTab", tabId: placement.tabId }
          : { kind: "newTab" };
      } else if (activeTab && activeTabCanHost) {
        intent = { kind: "addToTab", tabId: activeTab.id };
      } else {
        intent = { kind: "newTab" };
      }
      pendingSpawnIntentRef.current = intent;
      client.send({
        type: "spawn_session",
        label: null,
        target: nextTarget,
        mode: config.mode,
        initial_prompt: null,
        dangerously_skip_permissions: config.dangerously_skip_permissions,
        agent: config.agent,
        model: config.model,
        permission_mode: config.permission_mode,
        codex_sandbox: config.codex_sandbox,
        extra_env: config.extra_env,
        prompt_injector: null,
      });
      pushToast(setState, {
        severity: "info",
        message: "Spawning session…",
        detail: "Worktree creation may take a few seconds.",
      });
    },
    [onOpenSpawn],
  );

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

  // Restart-in-place from a stopped session's pane. Sequence matters: the
  // daemon processes WS messages in order, so duplicate_session reads the
  // stopped session's spawn_config BEFORE discard_session removes it. The
  // pendingSpawnIntentRef routes the new clone:
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
        };
      } else if (detail.tabId) {
        pendingSpawnIntentRef.current = {
          kind: "addToTab",
          tabId: detail.tabId,
        };
      } else {
        pendingSpawnIntentRef.current = { kind: "newTab" };
      }
      client.send({ type: "duplicate_session", session_id: detail.sessionId });
      client.send({
        type: "discard_session",
        session_id: detail.sessionId,
        cleanup: [],
      });
    };
    window.addEventListener("rt:pane_session_restart", handler);
    return () =>
      window.removeEventListener("rt:pane_session_restart", handler);
  }, []);

  const onLaunchPreset = useCallback(
    (preset: PresetEntry, target: PresetTarget) => {
      setState((s) => ({ ...s, presetLaunch: { preset, target } }));
    },
    [],
  );

  const onClosePresetLaunch = useCallback(() => {
    setState((s) => ({ ...s, presetLaunch: null }));
  }, []);

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
        agent: "claude",
        model: null,
        permission_mode: null,
        codex_sandbox: null,
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
    if (state.tabs.length === 0) return; // not yet hydrated
    const binding = findPaneBinding(state.tabs, popoutPaneId);
    if (binding && binding.session_id !== null) return;
    clearPanePoppedOut(popoutPaneId);
    void getCurrentWindow().close();
  }, [state.tabs]);

  // Pop-out window: close itself when the tab it was rendering disappears
  // (e.g. user closed the tab from the main window).
  useEffect(() => {
    if (!popoutTabId) return;
    if (state.tabs.length === 0) return; // not yet hydrated
    if (state.tabs.some((t) => t.id === popoutTabId)) return;
    void getCurrentWindow().close();
  }, [state.tabs]);

  // Session pop-out: close itself only when the session it was rendering is
  // removed from the registry. A stopped session remains renderable so the
  // pop-out can show the same restart/remove surface as an in-grid pane.
  useEffect(() => {
    if (!popoutSessionId) return;
    if (state.sessions.length === 0) return; // not yet hydrated
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

  const sendShutdown = useCallback(
    (mode: "stopKeepWorktrees" | "stopRemoveWorktrees" | "abandon") => {
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
          const cleanup = session.members.map((member) => ({
            repo_id: member.repo_id,
            remove_worktree: session.has_per_session_worktree,
          }));
          client.send({
            type: "stop_session",
            session_id: session.id,
            cleanup: cleanup.map((action) => ({
              ...action,
              remove_worktree: false,
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
    () => sendShutdown("stopKeepWorktrees"),
    [sendShutdown],
  );
  const onStopAndQuitRemoveWorktrees = useCallback(
    () => sendShutdown("stopRemoveWorktrees"),
    [sendShutdown],
  );
  const onAbandonAndQuit = useCallback(
    () => sendShutdown("abandon"),
    [sendShutdown],
  );

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
  const onSelectActivity = useCallback((next: ActivitySection) => {
    setActivitySection(next);
    writeActivitySection(next);
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
    state.vscodeQueue.length > 0;
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

  return (
    <div className="app-root" data-testid="app-root">
      <ActivityBar
        active={activitySection}
        onSelect={onSelectActivity}
        sourceControlBadge={sourceControlBadgeTotal}
      />
      <ResizableSplit
        storageKey="root.sidebar"
        defaultSize={280}
        minSize={200}
        direction="horizontal"
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
            client={state.client!}
            onActivateTab={onActivateTab}
          />
        )}
        <main className="main-pane">
          <TabBar
            tabs={state.tabs}
            activeTabId={state.activeTabId}
            client={state.client!}
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
      </ResizableSplit>
      {state.spawnOpen && state.client && (
        <SpawnDialog
          repos={state.repos}
          workspaces={state.workspaces}
          client={state.client}
          initialTarget={state.spawnInitial}
          spawnPrefill={state.spawnPrefill}
          canUseCurrentTab={canUseSpawnCurrentTab}
          currentTabName={
            canUseSpawnCurrentTab ? (spawnCurrentTab?.name ?? null) : null
          }
          tabs={spawnPlacementTabs}
          activeTabId={spawnCurrentTab?.id ?? null}
          onClose={onCloseSpawn}
          onSpawned={onSpawned}
          onAddRepo={onAddRepo}
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
      {state.vscodeQueue[0] && state.client && (
        <VscodeSuggestionToast
          suggestion={state.vscodeQueue[0]}
          client={state.client}
          onDismiss={onDismissVscodeSuggestion}
        />
      )}
      {state.settingsOpen && (
        <SettingsModal settings={settings} onClose={onCloseSettings} />
      )}
      {state.exitConfirmOpen && (
        <ExitConfirmDialog
          activeSessionCount={activeSessionCount}
          activeWorktreeSessionCount={activeWorktreeSessionCount}
          orphanSessionCount={orphanSessionCount}
          busy={state.exitInFlight}
          stuck={state.exitStuck}
          onStopAndQuit={onStopAndQuit}
          onStopAndQuitRemoveWorktrees={onStopAndQuitRemoveWorktrees}
          onAbandonAndQuit={onAbandonAndQuit}
          onQuitLeaveRunning={onQuitLeaveRunning}
          onForceQuit={onForceQuit}
          onCancel={onExitCancel}
        />
      )}
      <ErrorToast toasts={state.toasts} onDismiss={onDismissToast} />
      <UndoShelf
        entries={state.undoEntries}
        onUndo={onUndo}
        onDismiss={onDismissUndo}
      />
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

type PendingSpawnIntent =
  | { kind: "newTab" }
  | { kind: "replacePane"; tabId: string; paneId: string }
  | { kind: "addToTab"; tabId: string }
  | null;

/// Add the freshly spawned session to the existing `tabId`. Prefers a pane
/// beside sessions from the same repo/workspace before falling back to the
/// first empty pane or first pane split.
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
  const panes = collectPanes(grid);
  const sessions = latestStateRef.current?.sessions ?? [];
  const target = paneTargetForSession(panes, sessions, session);
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
    direction: "horizontal",
    place: "second",
    new_session_id: session.id,
  });
}

type PanePlacementTarget =
  | { kind: "replace"; paneId: string }
  | { kind: "split"; paneId: string };

function paneTargetForSession(
  panes: Array<{ pane_id: string; session_id: string | null }>,
  sessions: SessionSnapshot[],
  session: SessionSnapshot,
): PanePlacementTarget | null {
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
      if (pane) return { kind: "split", paneId: pane.pane_id };
    }
  }

  const emptyPane = panes.find((p) => p.session_id === null);
  if (emptyPane) {
    return { kind: "replace", paneId: emptyPane.pane_id };
  }
  const anchor = panes[0];
  return anchor ? { kind: "split", paneId: anchor.pane_id } : null;
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
  pendingSpawnIntentRef: React.MutableRefObject<PendingSpawnIntent>,
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
      // for one of these ids is recognized as not-new.
      for (const s of msg.sessions) seenSessionIdsRef.current.add(s.id);
      setState((s) => ({ ...s, sessions: msg.sessions }));
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
      // it does not take the auto-discard path for no-worktree sessions.
      const prevSession = latestStateRef.current?.sessions.find(
        (sn) => sn.id === session.id,
      );
      const justSelfExited =
        !isNew &&
        session.status === "stopped" &&
        session.exit_code !== null &&
        !session.is_inactive &&
        prevSession !== undefined &&
        prevSession.status !== "stopped" &&
        prevSession.status !== "error";
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
      return;
    }
    case "session_removed":
      // Daemon prunes pane references and broadcasts tab_updated separately;
      // here we update the session list AND any per-session sets that
      // could otherwise leak the dead id (attentionSessions in particular —
      // without this, a stopped-then-removed session keeps its warning
      // glyph on whatever container would otherwise show it).
      seenSessionIdsRef.current.delete(msg.session_id);
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
    case "tabs":
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
        };
      });
      return;
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
    case "branches":
    case "worktrees":
    case "workspace_spawn_preview":
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
