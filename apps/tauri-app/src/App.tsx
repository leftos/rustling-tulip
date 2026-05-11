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
  type ConnectionState,
  type DaemonClient,
} from "./api";
import type {
  DaemonMessage,
  RepoEntry,
  SessionSnapshot,
  TabEntry,
  VscodeWorkspaceSuggestion,
  WorkspaceEntry,
} from "./types";
import Sidebar, { type SpawnInitialTarget } from "./components/Sidebar";
import SessionWindow from "./components/SessionWindow";
import SpawnDialog from "./components/SpawnDialog";
import WorkspaceCreator from "./components/WorkspaceCreator";
import VscodeSuggestionToast from "./components/VscodeSuggestionToast";
import ResizableSplit from "./components/ResizableSplit";
import ExitConfirmDialog from "./components/ExitConfirmDialog";
import TabBar from "./components/TabBar";
import GridRenderer from "./components/GridRenderer";
import TabWindow from "./components/TabWindow";
import { logToFile } from "./utils/logger";
import { collectPanes } from "./utils/grid";

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
  /// Pipeline: when SpawnDialog fires `onSpawned`, this flag is set. The next
  /// new session triggers an automatic `create_tab` (see `pendingTabActivate`).
  pendingSessionToTab: boolean;
  /// Set after we send `create_tab` for a freshly spawned session; the next
  /// `tab_updated` for an unseen tab id becomes the active tab.
  pendingTabActivate: boolean;
  workspaceCreatorOpen: boolean;
  vscodeQueue: VscodeWorkspaceSuggestion[];
  attentionSessions: Set<string>;
  exitConfirmOpen: boolean;
  exitInFlight: boolean;
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
    pendingSessionToTab: false,
    pendingTabActivate: false,
    workspaceCreatorOpen: false,
    vscodeQueue: [],
    attentionSessions: new Set(),
    exitConfirmOpen: false,
    exitInFlight: false,
  });

  // PTY output is high-volume — keep it out of React state.
  const ptyListenersRef = useRef(
    new Map<string, Set<(b64: string) => void>>(),
  );

  // Held in a ref so handleMessage's `client.send(...)` can dispatch derived
  // messages (create_tab after a session spawns) without needing to thread
  // state through every closure.
  const clientRef = useRef<DaemonClient | null>(null);

  useEffect(() => {
    void (async () => {
      const granted = await isPermissionGranted();
      if (!granted) await requestPermission();
    })();
  }, []);

  useEffect(() => {
    let cancelled = false;
    let client: DaemonClient | null = null;

    (async () => {
      try {
        const handshake = await ensureDaemonStarted();
        if (cancelled) return;
        client = connectDaemon(handshake);
        clientRef.current = client;
        client.onConnectionChange((next) => {
          setState((s) => ({ ...s, status: next }));
        });
        client.onMessage((msg) =>
          handleMessage(msg, setState, ptyListenersRef.current, clientRef),
        );
        setState((s) => ({ ...s, client }));
      } catch (err) {
        setState((s) => ({
          ...s,
          status: { kind: "error", reason: String(err) },
        }));
      }
    })();

    return () => {
      cancelled = true;
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

  const onSelectSession = useCallback((sessionId: string) => {
    setState((s) => {
      const next = new Set(s.attentionSessions);
      next.delete(sessionId);
      if (!s.client) return { ...s, attentionSessions: next };
      const activeTab = s.tabs.find((t) => t.id === s.activeTabId);
      if (activeTab) {
        const panes = collectPanes(activeTab.grid);
        const focusedPane = panes.find((p) => p.pane_id === s.focusedPaneId);
        if (focusedPane && focusedPane.session_id === null) {
          s.client.send({
            type: "replace_pane_session",
            tab_id: activeTab.id,
            pane_id: focusedPane.pane_id,
            session_id: sessionId,
          });
          return { ...s, attentionSessions: next };
        }
      }
      // No focused empty pane → open the session in a fresh tab.
      s.client.send({
        type: "create_tab",
        name: null,
        initial_session_id: sessionId,
      });
      return {
        ...s,
        attentionSessions: next,
        pendingTabActivate: true,
      };
    });
  }, []);

  const onSpawnInPane = useCallback(
    (paneId: string) => {
      setState((s) => ({
        ...s,
        spawnOpen: true,
        spawnInitial: undefined,
        focusedPaneId: paneId,
      }));
    },
    [],
  );

  const onOpenSpawn = useCallback((initial?: SpawnInitialTarget) => {
    setState((s) => ({ ...s, spawnOpen: true, spawnInitial: initial }));
  }, []);

  const onCloseSpawn = useCallback(() => {
    setState((s) => ({ ...s, spawnOpen: false, spawnInitial: undefined }));
  }, []);

  const onSpawned = useCallback(() => {
    setState((s) => ({ ...s, pendingSessionToTab: true }));
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

  const onQuitLeaveRunning = useCallback(() => {
    logToFile("info", "exit modal: Quit, leave running");
    void closeMainWindow();
  }, [closeMainWindow]);

  const onStopAndQuit = useCallback(() => {
    logToFile("info", "exit modal: Stop sessions & quit clicked");
    setState((s) => ({ ...s, exitInFlight: true }));
    const client = state.client;
    if (!client) {
      logToFile("warn", "onStopAndQuit: no client; closing window directly");
      void closeMainWindow();
      return;
    }
    const closed = new Promise<void>((resolve) => {
      let done = false;
      const finish = (reason: string) => {
        if (done) return;
        done = true;
        logToFile("info", `onStopAndQuit: closed promise resolving (${reason})`);
        window.clearTimeout(timer);
        unsubscribe();
        resolve();
      };
      const unsubscribe = client.onConnectionChange((next) => {
        logToFile("info", `onStopAndQuit: connection state -> ${next.kind}`);
        if (next.kind !== "open" && next.kind !== "connecting") {
          finish(`connection ${next.kind}`);
        }
      });
      const timer = window.setTimeout(() => finish("timeout 2s"), 2000);
    });
    logToFile("info", "onStopAndQuit: sending shutdown message");
    client.send({ type: "shutdown" });
    void closed.then(closeMainWindow);
  }, [closeMainWindow, state.client]);

  const activeSessionCount = useMemo(
    () =>
      state.sessions.filter((s) => s.status !== "stopped" && !s.is_orphan)
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
    return new Set(
      collectPanes(activeTab.grid)
        .map((p) => p.session_id)
        .filter((id): id is string => id !== null),
    );
  }, [activeTab]);

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
    <div className="app-root">
      <ResizableSplit
        storageKey="root.sidebar"
        defaultSize={280}
        minSize={200}
        direction="horizontal"
      >
        <Sidebar
          repos={state.repos}
          workspaces={state.workspaces}
          sessions={state.sessions}
          highlightedSessionIds={sessionIdsInActiveTab}
          attentionSessions={state.attentionSessions}
          connection={state.status}
          onAddRepo={onAddRepo}
          onRemoveRepo={(id) =>
            state.client?.send({ type: "remove_repo", repo_id: id })
          }
          onRemoveWorkspace={(id) =>
            state.client?.send({ type: "remove_workspace", workspace_id: id })
          }
          onSelectSession={onSelectSession}
          onOpenSpawn={onOpenSpawn}
          onOpenWorkspaceCreator={onOpenWorkspaceCreator}
          onRevealInExplorer={onRevealInExplorer}
        />
        <main className="main-pane">
          <TabBar
            tabs={state.tabs}
            activeTabId={state.activeTabId}
            client={state.client!}
            onActivate={onActivateTab}
          />
          {activeTab && state.client ? (
            <GridRenderer
              tab={activeTab}
              client={state.client}
              sessions={state.sessions}
              subscribePty={subscribePty}
              focusedPaneId={state.focusedPaneId}
              onFocusPane={onFocusPane}
              onSpawnInPane={onSpawnInPane}
              hasRepos={state.repos.length > 0}
            />
          ) : (
            <EmptyState
              connection={state.status}
              onOpenSpawn={() => onOpenSpawn()}
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
          onClose={onCloseSpawn}
          onSpawned={onSpawned}
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
      {state.exitConfirmOpen && (
        <ExitConfirmDialog
          activeSessionCount={activeSessionCount}
          busy={state.exitInFlight}
          onStopAndQuit={onStopAndQuit}
          onQuitLeaveRunning={onQuitLeaveRunning}
          onCancel={onExitCancel}
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

function handleMessage(
  msg: DaemonMessage,
  setState: React.Dispatch<React.SetStateAction<AppState>>,
  ptyListeners: Map<string, Set<(b64: string) => void>>,
  clientRef: React.MutableRefObject<DaemonClient | null>,
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
    case "sessions":
      setState((s) => ({ ...s, sessions: msg.sessions }));
      return;
    case "session_updated":
      setState((s) => {
        const idx = s.sessions.findIndex((sn) => sn.id === msg.session.id);
        const next = s.sessions.slice();
        const isNew = idx === -1;
        if (isNew) next.push(msg.session);
        else next[idx] = msg.session;
        if (isNew && s.pendingSessionToTab) {
          clientRef.current?.send({
            type: "create_tab",
            name: null,
            initial_session_id: msg.session.id,
          });
          return {
            ...s,
            sessions: next,
            pendingSessionToTab: false,
            pendingTabActivate: true,
          };
        }
        return { ...s, sessions: next };
      });
      return;
    case "session_removed":
      // Daemon prunes pane references and broadcasts tab_updated separately;
      // here we only update the session list.
      setState((s) => ({
        ...s,
        sessions: s.sessions.filter((sn) => sn.id !== msg.session_id),
      }));
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
        if (isNew && (pendingTabActivate || active === null)) {
          active = msg.tab.id;
          pendingTabActivate = false;
        }
        return {
          ...s,
          tabs: next,
          activeTabId: active,
          pendingTabActivate,
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
      void (async () => {
        const granted = await isPermissionGranted();
        if (!granted) return;
        sendNotification({
          title,
          body: session?.label ?? "rustling-tulip",
        });
      })();
      window.dispatchEvent(new CustomEvent("rt:attention", { detail: msg }));
      return;
    }
    case "branches":
    case "session_diff":
    case "workspace_spawn_preview":
    case "commits":
    case "commit_detail":
    case "file_diff":
    case "remote_url":
    case "repo_status":
    case "scrollback":
    case "presets":
    case "preset_launch_failed":
      window.dispatchEvent(
        new CustomEvent(`rt:${msg.type}`, { detail: msg }),
      );
      if (msg.type === "preset_launch_failed") {
        console.error(
          `preset launch failed (${msg.preset_id}): ${msg.error}`,
          `${msg.partial_session_ids.length} sessions and ` +
            `${msg.partial_tab_ids.length} tabs were created before failure`,
        );
      }
      return;
    case "preset_launch_progress":
      // Auto-activate the tab being filled so the user sees panes appear.
      if (msg.current_tab_id !== null) {
        const targetTabId = msg.current_tab_id;
        setState((s) =>
          s.tabs.some((t) => t.id === targetTabId) && s.activeTabId !== targetTabId
            ? { ...s, activeTabId: targetTabId }
            : s,
        );
      }
      window.dispatchEvent(
        new CustomEvent("rt:preset_launch_progress", { detail: msg }),
      );
      return;
    case "error":
      console.error("daemon error:", msg.message);
      return;
  }
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
  hasRepos,
  hasTabs,
}: {
  connection: AppState["status"];
  onOpenSpawn: () => void;
  hasRepos: boolean;
  hasTabs: boolean;
}) {
  return (
    <div className="empty-state">
      <h1>rustling-tulip</h1>
      <p className="status-line">
        Daemon: <ConnectionBadge state={connection} />
      </p>
      {hasTabs ? (
        <p className="hint">Select a tab above.</p>
      ) : hasRepos ? (
        <button type="button" onClick={onOpenSpawn} className="primary">
          Spawn a session
        </button>
      ) : (
        <p className="hint">Add a repo from the sidebar to get started.</p>
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
