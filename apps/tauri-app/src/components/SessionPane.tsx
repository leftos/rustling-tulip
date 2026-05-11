import { useCallback, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { DaemonClient } from "../api";
import type { SessionSnapshot } from "../types";
import Terminal from "./Terminal";
import GitPanel from "./GitPanel";

/// True when running inside the pop-out session window. Pop-out windows
/// shouldn't show "Pop out" themselves — the toolbar already lives in the
/// SessionWindow chrome.
const isPopoutWindow =
  new URLSearchParams(window.location.search).get("session") !== null;

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

  const onPopOut = useCallback(() => {
    void invoke("open_session_window", { sessionId: session.id });
  }, [session.id]);

  const isHeadless = session.mode === "headless";
  const isPlainShell = session.mode === "plain_shell";
  const modeSuffix = isHeadless
    ? " · headless"
    : isPlainShell
      ? " · shell"
      : "";

  return (
    <div
      className="session-pane"
      data-testid="session-pane"
      data-session-id={session.id}
      data-session-status={session.status}
      data-session-mode={session.mode}
    >
      <header className="session-header">
        <div className="session-title">
          {/* Shell sessions sit at Idle forever — a green dot would be
              misleading. Show a terminal glyph in its place instead. */}
          {isPlainShell ? (
            <span className="status-glyph" aria-hidden="true">
              {">_"}
            </span>
          ) : (
            <span className={`status-dot status-${session.status}`} />
          )}
          <h2>{session.label}</h2>
          <span className="session-meta">
            {session.kind === "workspace"
              ? `${session.members.length} repos`
              : session.members[0]?.repo_name ?? ""}
            {modeSuffix}
            {!isPlainShell && ` · ${session.agent}`}
          </span>
        </div>
        <div className="session-actions">
          {!isPopoutWindow && (
            <button
              type="button"
              onClick={onPopOut}
              title="Open this session in its own window"
            >
              Pop out
            </button>
          )}
          {session.status !== "stopped" ? (
            confirming ? (
              <>
                <button
                  type="button"
                  onClick={onStop}
                  className="danger"
                  data-testid="session-stop-confirm"
                >
                  Confirm stop
                </button>
                <button type="button" onClick={() => setConfirming(false)}>
                  Cancel
                </button>
              </>
            ) : (
              <button
                type="button"
                onClick={() => setConfirming(true)}
                data-testid="session-stop"
              >
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
            data-testid="session-view-terminal"
          >
            {isHeadless ? "Events" : "Terminal"}
          </button>
          <button
            type="button"
            className={view === "git" ? "tab active" : "tab"}
            onClick={() => setView("git")}
            data-testid="session-view-git"
          >
            Git
          </button>
        </div>
      </div>
      {session.is_orphan && (
        <div className="orphan-banner">
          PTY stream lost across daemon restart. The underlying{" "}
          {isPlainShell ? "shell" : "claude"} process is still running, but
          live input/output is not available. Stop the session and spawn a
          new one to resume.
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
