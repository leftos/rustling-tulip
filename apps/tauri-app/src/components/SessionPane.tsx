import { useCallback, useState } from "react";
import type { DaemonClient } from "../api";
import type { SessionSnapshot } from "../types";
import Terminal from "./Terminal";
import GitPanel from "./GitPanel";

interface Props {
  session: SessionSnapshot;
  client: DaemonClient | null;
  subscribePty: (sessionId: string, cb: (b64: string) => void) => () => void;
}

type View = "terminal" | "git";

export default function SessionPane({ session, client, subscribePty }: Props) {
  const [confirming, setConfirming] = useState(false);
  const [view, setView] = useState<View>("terminal");

  const onStop = useCallback(() => {
    if (!client) return;
    client.send({
      type: "stop_session",
      session_id: session.id,
      cleanup: session.members.map((m) => ({
        repo_id: m.repo_id,
        remove_worktree: false,
      })),
    });
    setConfirming(false);
  }, [client, session]);

  const isHeadless = session.mode === "headless";

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
            {isHeadless ? " · headless" : ""}
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
        <div className="view-toggle">
          <button
            type="button"
            className={view === "terminal" ? "tab active" : "tab"}
            onClick={() => setView("terminal")}
          >
            {isHeadless ? "Events" : "Terminal"}
          </button>
          <button
            type="button"
            className={view === "git" ? "tab active" : "tab"}
            onClick={() => setView("git")}
          >
            Git
          </button>
        </div>
      </div>
      {session.is_orphan && (
        <div className="orphan-banner">
          PTY stream lost across daemon restart. The underlying claude process
          is still running, but live input/output is not available. Stop the
          session and spawn a new one to resume.
        </div>
      )}

      {view === "git" && client ? (
        <GitPanel members={session.members} client={client} />
      ) : isHeadless ? (
        <HeadlessView session={session} />
      ) : (
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
      )}
    </div>
  );
}

function HeadlessView({ session }: { session: SessionSnapshot }) {
  const m = session.metrics;
  return (
    <div className="headless-view">
      <div className="headless-stats">
        <Stat label="status" value={session.status} />
        <Stat label="in tokens" value={m.input_tokens.toLocaleString()} />
        <Stat label="out tokens" value={m.output_tokens.toLocaleString()} />
        <Stat label="cost" value={`$${m.cost_usd.toFixed(4)}`} />
      </div>
      <div className="headless-log">
        {session.recent_actions.length === 0 ? (
          <p className="empty">No events yet…</p>
        ) : (
          <ol>
            {session.recent_actions.map((line, idx) => (
              // Stable order, append-only — index is fine here.
              // eslint-disable-next-line react/no-array-index-key
              <li key={idx}>{line}</li>
            ))}
          </ol>
        )}
      </div>
    </div>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div className="stat">
      <span className="stat-label">{label}</span>
      <span className="stat-value">{value}</span>
    </div>
  );
}
