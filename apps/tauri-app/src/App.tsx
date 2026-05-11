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
  VscodeWorkspaceSuggestion,
  WorkspaceEntry,
} from "./types";
import Sidebar, { type SpawnInitialTarget } from "./components/Sidebar";
import SessionPane from "./components/SessionPane";
import SessionWindow from "./components/SessionWindow";
import SpawnDialog from "./components/SpawnDialog";
import WorkspaceCreator from "./components/WorkspaceCreator";
import VscodeSuggestionToast from "./components/VscodeSuggestionToast";
import ResizableSplit from "./components/ResizableSplit";
import ExitConfirmDialog from "./components/ExitConfirmDialog";
import { logToFile } from "./utils/logger";

/// Pop-out session window: when launched with `?session=<id>` we render
/// only the SessionWindow component, no sidebar or modals. The daemon
/// already accepts multiple WS clients, so this window opens its own
/// connection independently of the main window.
const popoutSessionId = new URLSearchParams(window.location.search).get(
  "session",
);

interface AppState {
  client: DaemonClient | null;
  status: ConnectionState | { kind: "init" } | { kind: "error"; reason: string };
  repos: RepoEntry[];
  workspaces: WorkspaceEntry[];
  sessions: SessionSnapshot[];
  selectedSessionId: string | null;
  spawnOpen: boolean;
  spawnInitial: SpawnInitialTarget | undefined;
  /// True after the user submitted SpawnDialog and we're waiting for the
  /// daemon's session_updated to know the new id. The first arriving session
  /// that we haven't seen before gets auto-selected. Cleared once consumed
  /// or after a short timeout in case spawn fails server-side.
  pendingSpawnSelect: boolean;
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
    selectedSessionId: null,
    spawnOpen: false,
    spawnInitial: undefined,
    pendingSpawnSelect: false,
    workspaceCreatorOpen: false,
    vscodeQueue: [],
    attentionSessions: new Set(),
    exitConfirmOpen: false,
    exitInFlight: false,
  });

  useEffect(() => {
    void (async () => {
      const granted = await isPermissionGranted();
      if (!granted) await requestPermission();
    })();
  }, []);

  // PTY output is high-volume — keep it out of React state.
  const ptyListenersRef = useRef(
    new Map<string, Set<(b64: string) => void>>(),
  );

  useEffect(() => {
    let cancelled = false;
    let client: DaemonClient | null = null;

    (async () => {
      try {
        const handshake = await ensureDaemonStarted();
        if (cancelled) return;
        client = connectDaemon(handshake);
        client.onConnectionChange((next) => {
          setState((s) => ({ ...s, status: next }));
        });
        client.onMessage((msg) =>
          handleMessage(msg, setState, ptyListenersRef.current),
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
    };
  }, []);

  const onAddRepo = useCallback(async () => {
    const path = await pickDirectory();
    if (!path) return;
    state.client?.send({ type: "add_repo", path, name: null });
  }, [state.client]);

  const onSelectSession = useCallback((id: string) => {
    setState((s) => {
      const next = new Set(s.attentionSessions);
      next.delete(id);
      return { ...s, selectedSessionId: id, attentionSessions: next };
    });
  }, []);

  const onOpenSpawn = useCallback((initial?: SpawnInitialTarget) => {
    setState((s) => ({ ...s, spawnOpen: true, spawnInitial: initial }));
  }, []);

  const onCloseSpawn = useCallback(() => {
    setState((s) => ({ ...s, spawnOpen: false, spawnInitial: undefined }));
  }, []);

  const onSpawned = useCallback(() => {
    setState((s) => ({ ...s, pendingSpawnSelect: true }));
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

  // Intercept main-window close so we can ask whether to stop the daemon.
  // Pop-out windows skip this entirely (popoutSessionId is non-null there).
  useEffect(() => {
    if (popoutSessionId) return;
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
    // Tauri v2's WebviewWindow.destroy() can hang when called from inside
    // the webview's own event loop (the round-trip IPC waits on a loop
    // that's awaiting the IPC). AppHandle::exit on the host side avoids
    // that deadlock — it tears down every window and returns control to
    // the OS, bypassing our onCloseRequested handler.
    try {
      await invoke("quit_app");
      logToFile("info", "closeMainWindow: quit_app returned (process should exit shortly)");
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
    // Subscribe before sending so the daemon can race-close before we get
    // here. Resolves on the first non-open connection state, or after a
    // safety timeout in case the daemon never closes the socket cleanly.
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

  const selectedSession = useMemo(
    () => state.sessions.find((s) => s.id === state.selectedSessionId) ?? null,
    [state.sessions, state.selectedSessionId],
  );

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
          selectedSessionId={state.selectedSessionId}
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
          {selectedSession ? (
            <SessionPane
              session={selectedSession}
              client={state.client}
              subscribePty={subscribePty}
            />
          ) : (
            <EmptyState
              connection={state.status}
              onOpenSpawn={() => onOpenSpawn()}
              hasRepos={state.repos.length > 0}
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

function handleMessage(
  msg: DaemonMessage,
  setState: React.Dispatch<React.SetStateAction<AppState>>,
  ptyListeners: Map<string, Set<(b64: string) => void>>,
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
        // Auto-select the first new session that arrives after the user
        // submitted SpawnDialog. The pending flag is consumed so subsequent
        // session_updated messages don't yank the selection around.
        const shouldSelect = isNew && s.pendingSpawnSelect;
        return {
          ...s,
          sessions: next,
          selectedSessionId: shouldSelect ? msg.session.id : s.selectedSessionId,
          pendingSpawnSelect: shouldSelect ? false : s.pendingSpawnSelect,
        };
      });
      return;
    case "session_removed":
      setState((s) => ({
        ...s,
        sessions: s.sessions.filter((sn) => sn.id !== msg.session_id),
        selectedSessionId:
          s.selectedSessionId === msg.session_id ? null : s.selectedSessionId,
      }));
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
      // Update the attention state for badge rendering.
      setState((s) => {
        if (s.selectedSessionId === msg.session_id) return s;
        const next = new Set(s.attentionSessions);
        next.add(msg.session_id);
        return { ...s, attentionSessions: next };
      });
      // Fire OS notification for non-stopped reasons (stopped is loud already).
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
      // Routed by components that asked for it via custom events.
      window.dispatchEvent(
        new CustomEvent(`rt:${msg.type}`, { detail: msg }),
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
}: {
  connection: AppState["status"];
  onOpenSpawn: () => void;
  hasRepos: boolean;
}) {
  return (
    <div className="empty-state">
      <h1>rustling-tulip</h1>
      <p className="status-line">
        Daemon: <ConnectionBadge state={connection} />
      </p>
      {hasRepos ? (
        <button type="button" onClick={onOpenSpawn} className="primary">
          Spawn a session
        </button>
      ) : (
        <p className="hint">
          Add a repo from the sidebar to get started.
        </p>
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
