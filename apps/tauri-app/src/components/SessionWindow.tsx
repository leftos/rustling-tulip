import { useCallback, useEffect } from "react";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import type { DaemonClient } from "../api";
import type { SessionSnapshot } from "../types";
import {
  sessionDisplayLabel,
  sessionLabelTooltip,
  sessionRuntimeLabel,
} from "../utils/sessionLabel";
import SessionPane from "./SessionPane";

interface Props {
  session: SessionSnapshot;
  client: DaemonClient;
  subscribePty: (sessionId: string, cb: (b64: string) => void) => () => void;
}

/// Standalone window chrome for a single session pop-out. Renders a
/// minimal toolbar (label, status badge, stop, close-window) plus the
/// reused [`SessionPane`] component below. Closing this window does not
/// affect the daemon-side session — the main window still shows it.
export default function SessionWindow({
  session,
  client,
  subscribePty,
}: Props) {
  const onStop = useCallback(() => {
    client.send({
      type: "stop_session",
      session_id: session.id,
      cleanup: session.members.map((m) => ({
        repo_id: m.repo_id,
        remove_worktree: false,
      })),
    });
  }, [client, session]);

  const onCloseWindow = useCallback(() => {
    void getCurrentWebviewWindow().close();
  }, []);

  // Replace the OS window title (originally set to a raw UUID by the
  // Tauri builder in `open_session_window`) with the user-visible
  // label — preferring the agent's OSC title when set, falling back to
  // the canonical `<repo>:<branch>` otherwise. Renames + agent-side
  // title changes both flow through here.
  const displayLabel = sessionDisplayLabel(session);
  useEffect(() => {
    void getCurrentWebviewWindow().setTitle(displayLabel);
  }, [displayLabel]);

  return (
    <div className="session-window-root">
      <header className="session-window-toolbar">
        <span className={`status-dot status-${session.status}`} />
        <h1 title={sessionLabelTooltip(session)}>{displayLabel}</h1>
        {session.members.map((m) => (
          <span
            key={m.repo_id}
            className="chip session-member-chip"
            title={m.worktree_path}
          >
            {m.repo_name}: {m.branch}
          </span>
        ))}
        {(() => {
          const runtime = sessionRuntimeLabel(session);
          return runtime ? (
            <span
              className="chip session-runtime-chip"
              title={`Running ${runtime}`}
            >
              {runtime}
            </span>
          ) : null;
        })()}
        {session.mode === "headless" && (
          <span className="session-window-meta">· headless</span>
        )}
        <span className="spacer" />
        {session.status !== "stopped" && (
          <button type="button" onClick={onStop} className="danger">
            Stop session
          </button>
        )}
        <button type="button" onClick={onCloseWindow}>
          Close window
        </button>
      </header>
      <div className="session-window-body">
        <SessionPane
          session={session}
          client={client}
          tabs={[]}
          subscribePty={subscribePty}
        />
      </div>
    </div>
  );
}
