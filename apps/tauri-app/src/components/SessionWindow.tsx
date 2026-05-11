import { useCallback } from "react";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import type { DaemonClient } from "../api";
import type { SessionSnapshot } from "../types";
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

  return (
    <div className="session-window-root">
      <header className="session-window-toolbar">
        <span className={`status-dot status-${session.status}`} />
        <h1
          title={
            session.terminal_title && session.terminal_title !== session.label
              ? `${session.label}\nTerminal: ${session.terminal_title}`
              : session.label
          }
        >
          {session.label}
        </h1>
        <span className="session-window-meta">
          {session.kind === "workspace"
            ? `${session.members.length} repos`
            : (session.members[0]?.repo_name ?? "")}
          {session.mode === "headless" ? " · headless" : ""}
          {session.mode !== "plain_shell" && ` · ${session.agent}`}
        </span>
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
          subscribePty={subscribePty}
        />
      </div>
    </div>
  );
}
