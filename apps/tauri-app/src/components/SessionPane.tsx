import { useCallback, useState } from "react";
import type { DaemonClient } from "../api";
import type { SessionSnapshot } from "../types";
import Terminal from "./Terminal";

interface Props {
  session: SessionSnapshot;
  client: DaemonClient | null;
  subscribePty: (sessionId: string, cb: (b64: string) => void) => () => void;
}

export default function SessionPane({ session, client, subscribePty }: Props) {
  const [confirming, setConfirming] = useState(false);

  const onStop = useCallback(() => {
    if (!client) return;
    client.send({
      type: "stop_session",
      session_id: session.id,
      cleanup: session.members.map((m) => ({
        repo_id: m.repo_id,
        remove_worktree: false, // Phase 2 will offer per-member toggles
      })),
    });
    setConfirming(false);
  }, [client, session]);

  return (
    <div className="session-pane">
      <header className="session-header">
        <div className="session-title">
          <span className={`status-dot status-${session.status}`} />
          <h2>{session.label}</h2>
          <span className="session-meta">
            {session.kind === "workspace"
              ? `${session.members.length} repos`
              : session.members[0]?.repo_name ?? ""}
          </span>
        </div>
        <div className="session-actions">
          {session.status !== "stopped" ? (
            confirming ? (
              <>
                <button type="button" onClick={onStop} className="danger">
                  Confirm stop
                </button>
                <button type="button" onClick={() => setConfirming(false)}>
                  Cancel
                </button>
              </>
            ) : (
              <button type="button" onClick={() => setConfirming(true)}>
                Stop
              </button>
            )
          ) : (
            <span className="muted">
              exit code {session.exit_code ?? "?"}
            </span>
          )}
        </div>
      </header>
      <div className="session-members">
        {session.members.map((m) => (
          <span key={m.repo_id} className="chip" title={m.worktree_path}>
            {m.repo_name}: {m.branch}
          </span>
        ))}
      </div>
      <div className="terminal-host">
        {client && session.status !== "stopped" ? (
          <Terminal
            sessionId={session.id}
            client={client}
            subscribePty={subscribePty}
          />
        ) : (
          <div className="terminal-placeholder">
            {session.status === "stopped"
              ? "Session has exited."
              : "Waiting for daemon connection..."}
          </div>
        )}
      </div>
    </div>
  );
}
