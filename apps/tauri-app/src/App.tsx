import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
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
  type ConnectionState,
  type DaemonClient,
} from "./api";
import {
  tabGrid,
  type DaemonMessage,
  type PresetEntry,
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
import Sidebar, { type SpawnInitialTarget } from "./components/Sidebar";
import SourceControlSidebar from "./components/source-control/SourceControlSidebar";
import SessionWindow from "./components/SessionWindow";
import SpawnDialog from "./components/SpawnDialog";
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
import { logToFile } from "./utils/logger";
import { collectPanes, findTabContainingSession } from "./utils/grid";
import { useKeyboardShortcuts, type KeyboardShortcut } from "./utils/a11y";
import { loadSettings, useSettings } from "./utils/settings";

/// Pop-out windows: launched with either `?tab=<id>` (per-tab pop-out) or
/// `?session=<id>` (legacy single-session pop-out). The branch in App
/// renders either TabWindow or SessionWindow without sidebar/modals.
const queryParams = new URLSearchParams(window.location.search);
const popoutTabId = queryParams.get("tab");
const popoutSessionId = queryParams.get("session");

const ACTIVE_TAB_KEY = "rt:active-tab:main";

interface AppState {
  client: DaemonClient | null;
  status: ConnectionState | { kind: "init" } | { kind: "error"; reason: string };
  repos: RepoEntry[];
  workspaces: WorkspaceEntry[];
  sessions: SessionSnapshot[];
  tabs: TabEntry[];
  activeTabId: string | null;
  focusedPaneId: string | null;
  spawnOpen: boolean;
  spawnInitial: SpawnInitialTarget | undefined;
  /// Full SpawnConfig to seed the spawn dialog from (used by "Duplicate
  /// session → Shift-click → open dialog pre-filled"). When set, the
  /// dialog hydrates agent, run mode, skip-perms, model, permission mode,
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
  toasts: ToastEntry[];
  /// Repo-remove modal state, set when the sidebar's × click hit a repo
  /// with at least one non-stopped session. `null` when no modal is open.
  repoRemove: RepoRemoveIntent | null;
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
    spawnOpen: false,
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
    toasts: [],
    repoRemove: null,
  });

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
  /// Consumed by the `session_updated` handler the first time it sees an
  /// unseen session id.
  const pendingSpawnIntentRef = useRef<
    | { kind: "newTab" }
    | { kind: "replacePane"; tabId: string; paneId: string }
    | null
  >(null);

  /// Captured when the spawn dialog is opened from an empty pane's "+ spawn"
  /// button. Read at `onSpawned` to upgrade the pending intent from `newTab`
  /// (the default) to `replacePane`. Cleared on dialog close so a manual reopen
  /// from the toolbar doesn't inherit a stale target.
  const spawnTargetPaneRef = useRef<{ tabId: string; paneId: string } | null>(
    null,
  );

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
          ),
        );
        // Dev-only: expose the daemon client on window for e2e specs that
        // need to send messages through the React app's WS (so server
        // replies arrive at this client's onMessage handler). Mirrors the
        // `__rt_terms` / `__rt_console` pattern. Tree-shaken in prod.
        if (import.meta.env.DEV) {
          (globalThis as unknown as { __rt_daemon_client?: DaemonClient }).__rt_daemon_client = client;
        }
        setState((s) => ({ ...s, client }));
      } catch (err) {
        // ensureDaemonStarted failure (e.g. supervisor can't spawn the
        // daemon) — record the error AND schedule a retry. The supervisor
        // may need a moment to clean up a stale daemon.json.
        const reason = String(err);
        setState((s) => ({ ...s, status: { kind: "error", reason } }));
        if (!cancelled) {
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
  }, []);

  useEffect(() => {
    if (state.activeTabId) {
      localStorage.setItem(ACTIVE_TAB_KEY, state.activeTabId);
    } else {
      localStorage.removeItem(ACTIVE_TAB_KEY);
    }
  }, [state.activeTabId]);

  const onAddRepo = useCallback(async () => {
    const path = await pickDirectory();
    if (!path) return;
    state.client?.send({ type: "add_repo", path, name: null });
  }, [state.client]);

  const onActivateTab = useCallback((tabId: string) => {
    setState((s) => ({ ...s, activeTabId: tabId, focusedPaneId: null }));
  }, []);

  const onFocusPane = useCallback((paneId: string) => {
    setState((s) => (s.focusedPaneId === paneId ? s : { ...s, focusedPaneId: paneId }));
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
        if (!cur.attentionSessions.has(sessionId)) return cur;
        const next = new Set(cur.attentionSessions);
        next.delete(sessionId);
        return { ...cur, attentionSessions: next };
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
        };
      });
      return;
    }
    let openInNewTab = true;
    const activeTab = s.tabs.find((t) => t.id === s.activeTabId);
    const activeGrid = activeTab ? tabGrid(activeTab) : null;
    if (activeTab && activeGrid) {
      const panes = collectPanes(activeGrid);
      const focusedPane = panes.find((p) => p.pane_id === s.focusedPaneId);
      if (focusedPane && focusedPane.session_id === null) {
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
      return openInNewTab
        ? {
            ...cur,
            attentionSessions: next,
            pendingTabActivate: cur.pendingTabActivate + 1,
          }
        : { ...cur, attentionSessions: next };
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
      }));
    },
    [],
  );

  const onOpenSpawn = useCallback((initial?: SpawnInitialTarget) => {
    // Toolbar/sidebar spawn — explicitly clear any prior pane intent so a
    // leftover from a cancelled "spawn in this pane" doesn't re-target.
    spawnTargetPaneRef.current = null;
    setState((s) => ({
      ...s,
      spawnOpen: true,
      spawnInitial: initial,
      spawnPrefill: undefined,
    }));
  }, []);

  const onCloseSpawn = useCallback(() => {
    spawnTargetPaneRef.current = null;
    setState((s) => ({
      ...s,
      spawnOpen: false,
      spawnInitial: undefined,
      spawnPrefill: undefined,
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
        initial =
          config.target.kind === "single"
            ? { kind: "repo", repo_id: config.target.repo_id }
            : { kind: "workspace", workspace_id: config.target.workspace_id };
      }
      spawnTargetPaneRef.current = null;
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

  const onLaunchPreset = useCallback(
    (preset: PresetEntry, target: PresetTarget) => {
      setState((s) => ({ ...s, presetLaunch: { preset, target } }));
    },
    [],
  );

  const onClosePresetLaunch = useCallback(() => {
    setState((s) => ({ ...s, presetLaunch: null }));
  }, []);

  const onSpawned = useCallback(() => {
    // Arm the one-shot follow-up for the next new session. If the user opened
    // the dialog from an empty pane's "+ spawn" button, route the result into
    // that pane (replacePane). Otherwise open a fresh tab (newTab).
    const target = spawnTargetPaneRef.current;
    pendingSpawnIntentRef.current = target
      ? { kind: "replacePane", tabId: target.tabId, paneId: target.paneId }
      : { kind: "newTab" };
    spawnTargetPaneRef.current = null;
    // Optimistic feedback while the daemon resolves the spawn. Worktree
    // creation can take several seconds (Phase 2 workspace flows resolve
    // multiple repos), and previously the dialog vanished without trace
    // until session_updated arrived. The toast auto-dismisses after 8s,
    // by which point the session is normally live in the sidebar.
    pushToast(setState, {
      severity: "info",
      message: "Spawning session…",
      detail: "Worktree creation may take a few seconds.",
    });
  }, []);

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

  // Pop-out window: close itself when the tab it was rendering disappears
  // (e.g. user closed the tab from the main window).
  useEffect(() => {
    if (!popoutTabId) return;
    if (state.tabs.length === 0) return; // not yet hydrated
    if (state.tabs.some((t) => t.id === popoutTabId)) return;
    void getCurrentWindow().close();
  }, [state.tabs]);

  // Session pop-out: close itself when the session it was rendering is
  // removed from the registry (Stop session in the main window, or any
  // daemon-side cleanup). Previously the pop-out kept rendering an
  // exit-code state forever — see ux-audit "Pop-out session window never
  // auto-closes when its session is stopped or removed".
  useEffect(() => {
    if (!popoutSessionId) return;
    if (state.sessions.length === 0) return; // not yet hydrated
    if (state.sessions.some((s) => s.id === popoutSessionId)) return;
    void getCurrentWindow().close();
  }, [state.sessions]);

  // Intercept main-window close so we can ask whether to stop the daemon.
  useEffect(() => {
    if (popoutSessionId || popoutTabId) return;
    let unlisten: (() => void) | null = null;
    void (async () => {
      const win = getCurrentWindow();
      unlisten = await win.onCloseRequested((event) => {
        logToFile("info", "main window onCloseRequested fired");
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

  const onOpenSettings = useCallback(() => {
    setState((s) => (s.settingsOpen ? s : { ...s, settingsOpen: true }));
  }, []);

  const onCloseSettings = useCallback(() => {
    setState((s) => ({ ...s, settingsOpen: false }));
  }, []);

  const onQuitLeaveRunning = useCallback(() => {
    logToFile("info", "exit modal: Quit, leave running");
    void closeMainWindow();
  }, [closeMainWindow]);

  const onForceQuit = useCallback(() => {
    logToFile("warn", "exit modal: Force quit clicked (daemon stuck)");
    void closeMainWindow();
  }, [closeMainWindow]);

  const onStopAndQuit = useCallback(() => {
    logToFile("info", "exit modal: Stop sessions & quit clicked");
    setState((s) => ({ ...s, exitInFlight: true, exitStuck: false }));
    const client = state.client;
    if (!client) {
      logToFile("warn", "onStopAndQuit: no client; closing window directly");
      void closeMainWindow();
      return;
    }
    // Two timers gate the shutdown UX:
    //   - At 2 s with the WS still open, flip `exitStuck` so the dialog
    //     swaps to a warning + Force-quit affordance instead of pretending
    //     to still be making progress.
    //   - When the WS actually closes (graceful daemon shutdown), close
    //     the OS window normally.
    // Previously the 2 s timer itself closed the window, which silently
    // dropped the user out of the app while the daemon was still up —
    // they'd see "daemon supervisor failed" errors on next launch.
    let resolved = false;
    const stuckTimer = window.setTimeout(() => {
      if (resolved) return;
      logToFile(
        "warn",
        "onStopAndQuit: 2 s elapsed without WS close; surfacing force-quit option",
      );
      setState((s) => ({ ...s, exitStuck: true }));
    }, 2000);
    const unsubscribe = client.onConnectionChange((next) => {
      logToFile("info", `onStopAndQuit: connection state -> ${next.kind}`);
      if (next.kind === "open" || next.kind === "connecting") return;
      if (resolved) return;
      resolved = true;
      window.clearTimeout(stuckTimer);
      unsubscribe();
      logToFile(
        "info",
        `onStopAndQuit: WS closed (${next.kind}); closing window`,
      );
      void closeMainWindow();
    });
    logToFile("info", "onStopAndQuit: sending shutdown message");
    client.send({ type: "shutdown" });
  }, [closeMainWindow, state.client]);

  const activeSessionCount = useMemo(
    () =>
      state.sessions.filter((s) => s.status !== "stopped" && !s.is_orphan)
        .length,
    [state.sessions],
  );
  // Orphans are non-stopped sessions whose PTY handle was lost across a
  // daemon restart. Counted separately so the exit-confirm dialog can
  // explain why "Stop sessions & quit" is a no-op for them (the daemon's
  // stop_session early-returns when the pty handle is None — there's
  // nothing to send the kill signal to). See ux-audit "No active
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

  // Which sidebar panel is showing (Sessions vs Source control). Persisted
  // to localStorage so it survives reloads.
  const [activitySection, setActivitySection] = useState<ActivitySection>(
    () => readActivitySection(),
  );
  const onSelectActivity = useCallback((next: ActivitySection) => {
    setActivitySection(next);
    writeActivitySection(next);
  }, []);

  // Derive the focused-repo id from the focused pane's session. Walks
  // active tab → focused pane → session → first member's repo_id.
  // `null` when no session is focused; the source-control sidebar falls
  // back to a manual override or the first registered repo.
  const focusedRepoId = useMemo(() => {
    if (!activeTab || !state.focusedPaneId) return null;
    const grid = tabGrid(activeTab);
    if (!grid) return null;
    const pane = collectPanes(grid).find(
      (p) => p.pane_id === state.focusedPaneId,
    );
    if (!pane?.session_id) return null;
    const session = state.sessions.find((s) => s.id === pane.session_id);
    return session?.members[0]?.repo_id ?? null;
  }, [activeTab, state.focusedPaneId, state.sessions]);

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
    if (anyModalOpen || popoutTabId !== null || popoutSessionId !== null) {
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
    return list;
  }, [
    anyModalOpen,
    state.client,
    state.repos.length,
    state.tabs,
    state.activeTabId,
    onActivateTab,
    onOpenSpawn,
    onOpenSettings,
  ]);
  useKeyboardShortcuts(shortcuts);

  if (popoutTabId) {
    const popoutTab = state.tabs.find((t) => t.id === popoutTabId);
    return (
      <div className="app-root">
        {state.client && popoutTab ? (
          <TabWindow
            tab={popoutTab}
            client={state.client}
            sessions={state.sessions}
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
      <ActivityBar active={activitySection} onSelect={onSelectActivity} />
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
            highlightedSessionIds={sessionIdsInActiveTab}
            attentionSessions={state.attentionSessions}
            connection={state.status}
            tabs={state.tabs}
            onAddRepo={onAddRepo}
            onRemoveRepo={(id) =>
              state.client?.send({ type: "remove_repo", repo_id: id })
            }
            onRemoveRepoWithLiveSessions={onRemoveRepoWithLiveSessions}
            onRemoveWorkspace={(id) =>
              state.client?.send({ type: "remove_workspace", workspace_id: id })
            }
            onSelectSession={onSelectSession}
            onOpenSpawn={onOpenSpawn}
            onDuplicateSessionWithDialog={onDuplicateSessionWithDialog}
            onOpenWorkspaceCreator={onOpenWorkspaceCreator}
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
                subscribePty={subscribePty}
                focusedPaneId={state.focusedPaneId}
                onFocusPane={onFocusPane}
                onSpawnInPane={onSpawnInPane}
                hasRepos={state.repos.length > 0}
                onArmNextNewTab={onArmNextNewTab}
                onArmFocusNewPane={onArmFocusNewPane}
              />
            )
          ) : (
            <EmptyState
              connection={state.status}
              onOpenSpawn={() => onOpenSpawn()}
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
          onClose={onCloseSpawn}
          onSpawned={onSpawned}
          onAddRepo={onAddRepo}
        />
      )}
      {state.presetLaunch && state.client && (
        <PresetLaunchDialog
          preset={state.presetLaunch.preset}
          target={state.presetLaunch.target}
          repos={state.repos}
          client={state.client}
          onClose={onClosePresetLaunch}
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
          orphanSessionCount={orphanSessionCount}
          busy={state.exitInFlight}
          stuck={state.exitStuck}
          onStopAndQuit={onStopAndQuit}
          onQuitLeaveRunning={onQuitLeaveRunning}
          onForceQuit={onForceQuit}
          onCancel={onExitCancel}
        />
      )}
      <ErrorToast toasts={state.toasts} onDismiss={onDismissToast} />
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

type PendingSpawnIntent =
  | { kind: "newTab" }
  | { kind: "replacePane"; tabId: string; paneId: string }
  | null;

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
    case "repos":
      setState((s) => ({ ...s, repos: msg.repos }));
      return;
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
      if (intent) pendingSpawnIntentRef.current = null;
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
          };
        }
        return { ...s, sessions: next, attentionSessions: attention };
      });
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
        return {
          ...s,
          tabs: msg.tabs,
          activeTabId: active,
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
            if (fresh) focusedPaneId = fresh.pane_id;
          }
          pendingPaneFocusRef.current = null;
        }
        return {
          ...s,
          tabs: next,
          activeTabId: active,
          pendingTabActivate,
          focusedPaneId,
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
        return { ...s, tabs: next, activeTabId: active };
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
    case "workspace_spawn_preview":
    case "commits":
    case "commit_detail":
    case "file_diff":
    case "remote_url":
    case "repo_status":
    case "scrollback":
    case "spawn_config_reply":
    case "presets":
    case "preset_launch_failed":
    case "preset_preview":
    case "preset_preview_error":
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
      if (msg.type === "preset_launch_failed") {
        const partials = `${msg.partial_session_ids.length} session(s), ${msg.partial_tab_ids.length} tab(s) partial`;
        pushToast(setState, {
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
        key: `preset:${msg.preset_id}`,
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

function EmptyState({
  connection,
  onOpenSpawn,
  onAddRepo,
  hasRepos,
  hasTabs,
}: {
  connection: AppState["status"];
  onOpenSpawn: () => void;
  onAddRepo: () => void;
  hasRepos: boolean;
  hasTabs: boolean;
}) {
  return (
    <div className="empty-state" data-testid="empty-state">
      <h1>rustling-tulip</h1>
      <p className="status-line">
        Daemon: <ConnectionBadge state={connection} />
      </p>
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
            No repos registered yet. Add one to spawn sessions and create
            workspaces.
          </p>
          <button
            type="button"
            onClick={onAddRepo}
            className="primary"
            data-testid="empty-state-add-repo"
          >
            + Add repo
          </button>
        </>
      )}
    </div>
  );
}

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
