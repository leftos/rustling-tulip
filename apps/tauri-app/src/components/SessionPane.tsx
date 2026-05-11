import { useCallback, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { DaemonClient } from "../api";
import type { SessionSnapshot } from "../types";
import Terminal from "./Terminal";

/// True when running inside any pop-out window (single-session
/// `?session=<id>` OR full-tab `?tab=<id>`). Pop-out windows shouldn't
/// show "Pop out" themselves — opening yet another window from inside a
/// pop-out gives the user nothing useful, and the SessionWindow chrome
/// already exposes the right toolbar for the single-session form.
const isPopoutWindow = (() => {
  const params = new URLSearchParams(window.location.search);
  return params.get("session") !== null || params.get("tab") !== null;
})();

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
            <span
              className={`status-dot status-${session.status}`}
              title={`status: ${session.status}`}
              aria-label={`status ${session.status}`}
              role="img"
            />
          )}
          <h2
            title={
              session.terminal_title && session.terminal_title !== session.label
                ? `${session.label}\nTerminal: ${session.terminal_title}`
                : session.label
            }
          >
            {session.label}
          </h2>
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
          {/* Hide both the Stop button and the exit-code label inside a
              pop-out — SessionWindow's chrome toolbar already exposes
              the right controls. Previously the two were behaving
              differently (chrome single-click vs inner two-step) which
              confused which Stop the user thought they were clicking. */}
          {!isPopoutWindow &&
            (session.status !== "stopped" ? (
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
            ))}
        </div>
      </header>
      <div className="session-members">
        {session.members.map((m) => (
          <span key={m.repo_id} className="chip" title={m.worktree_path}>
            {m.repo_name}: {m.branch}
          </span>
        ))}
      </div>
      {session.is_orphan && (
        <div className="orphan-banner">
          PTY stream lost across daemon restart. The underlying{" "}
          {isPlainShell ? "shell" : "claude"} process is still running, but
          live input/output is not available. Use Stop to kill the recorded
          PID and clean up, then spawn a new session.
        </div>
      )}

      {isHeadless ? (
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
