import { useCallback, useEffect, useMemo, useRef, useState } from "react";
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
import Sidebar from "./components/Sidebar";
import SessionPane from "./components/SessionPane";
import SpawnDialog from "./components/SpawnDialog";
import WorkspaceCreator from "./components/WorkspaceCreator";
import VscodeSuggestionToast from "./components/VscodeSuggestionToast";

interface AppState {
  client: DaemonClient | null;
  status: ConnectionState | { kind: "init" } | { kind: "error"; reason: string };
  repos: RepoEntry[];
  workspaces: WorkspaceEntry[];
  sessions: SessionSnapshot[];
  selectedSessionId: string | null;
  spawnOpen: boolean;
  workspaceCreatorOpen: boolean;
  vscodeQueue: VscodeWorkspaceSuggestion[];
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
    workspaceCreatorOpen: false,
    vscodeQueue: [],
  });

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
    setState((s) => ({ ...s, selectedSessionId: id }));
  }, []);

  const onOpenSpawn = useCallback(() => {
    setState((s) => ({ ...s, spawnOpen: true }));
  }, []);

  const onCloseSpawn = useCallback(() => {
    setState((s) => ({ ...s, spawnOpen: false }));
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

  return (
    <div className="app-root">
      <Sidebar
        repos={state.repos}
        workspaces={state.workspaces}
        sessions={state.sessions}
        selectedSessionId={state.selectedSessionId}
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
            onOpenSpawn={onOpenSpawn}
            hasRepos={state.repos.length > 0}
          />
        )}
      </main>
      {state.spawnOpen && state.client && (
        <SpawnDialog
          repos={state.repos}
          workspaces={state.workspaces}
          client={state.client}
          onClose={onCloseSpawn}
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
        if (idx === -1) next.push(msg.session);
        else next[idx] = msg.session;
        return { ...s, sessions: next };
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
    case "branches":
    case "session_diff":
    case "workspace_spawn_preview":
    case "attention":
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
