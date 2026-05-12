import { useCallback, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { DaemonClient } from "../api";
import type { SessionSnapshot, TabEntry } from "../types";
import {
  sessionDisplayLabel,
  sessionLabelTooltip,
  sessionRuntimeLabel,
} from "../utils/sessionLabel";
import SessionContextMenu, {
  type SessionContextMenuState,
} from "./SessionContextMenu";
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
  /// Full tab list — used by the session context menu to render the
  /// "Move to → <tab>" submenu. Threaded through GridRenderer from
  /// App.tsx. Empty arrays are fine; the menu just omits the entry.
  tabs: TabEntry[];
  subscribePty: (sessionId: string, cb: (b64: string) => void) => () => void;
  /// Wired from GridRenderer so the entire session-header strip acts as
  /// the pane drag handle, not just the small ⠿ icon in the corner.
  /// Omitted when SessionPane is rendered outside a grid (e.g. inside
  /// the single-session pop-out window, where there is nowhere to drop).
  onHeaderDragStart?: (e: React.DragEvent) => void;
  /// Tab id this pane is rendered inside. Threaded down to Terminal so
  /// per-tab font-size overrides resolve correctly. `null` for the
  /// single-session pop-out window (no enclosing tab).
  tabId?: string | null;
}

export default function SessionPane({
  session,
  client,
  tabs,
  subscribePty,
  onHeaderDragStart,
  tabId,
}: Props) {
  const [confirming, setConfirming] = useState(false);
  const [sessionMenu, setSessionMenu] =
    useState<SessionContextMenuState | null>(null);
  const onHeaderContextMenu = useCallback(
    (e: React.MouseEvent) => {
      e.preventDefault();
      setSessionMenu({ x: e.clientX, y: e.clientY, session });
    },
    [session],
  );

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
  const runtimeLabel = sessionRuntimeLabel(session);
  const modeSuffix = isHeadless ? " · headless" : "";

  return (
    <div
      className="session-pane"
      data-testid="session-pane"
      data-session-id={session.id}
      data-session-status={session.status}
      data-session-mode={session.mode}
    >
      <header
        className={
          onHeaderDragStart
            ? "session-header session-header-draggable"
            : "session-header"
        }
        draggable={onHeaderDragStart !== undefined}
        onDragStart={onHeaderDragStart}
        onContextMenu={onHeaderContextMenu}
      >
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
          <h2 title={sessionLabelTooltip(session)}>
            {sessionDisplayLabel(session)}
          </h2>
          {/* Per-member <repo>:<branch> chips sit inline with the title
              now (iter 51) so a workspace session doesn't waste an
              entire row. Single-repo sessions render one chip; workspace
              sessions render N. Hover surfaces the worktree path. */}
          {session.members.map((m) => (
            <span
              key={m.repo_id}
              className="chip session-member-chip"
              title={m.worktree_path}
            >
              {m.repo_name}: {m.branch}
            </span>
          ))}
          {runtimeLabel && (
            <span
              className="chip session-runtime-chip"
              title={`Running ${runtimeLabel}`}
              data-testid="session-runtime-chip"
            >
              {runtimeLabel}
            </span>
          )}
          {modeSuffix && (
            <span className="session-meta">{modeSuffix}</span>
          )}
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
      {session.is_orphan && (
        <div className="orphan-banner">
          PTY stream lost across daemon restart. The underlying{" "}
          {isPlainShell ? "shell" : "claude"} process is still running, but
          live input/output is not available. Use Stop to kill the recorded
          PID and clean up, then spawn a new session.
        </div>
      )}

      {sessionMenu && client && (
        <SessionContextMenu
          state={sessionMenu}
          tabs={tabs}
          client={client}
          onClose={() => setSessionMenu(null)}
          onDuplicate={(withDialog, target) => {
            const sid = sessionMenu.session.id;
            if (withDialog) {
              // App.tsx listens for this event; it fetches the source's
              // SpawnConfig and opens the spawn dialog pre-filled.
              window.dispatchEvent(
                new CustomEvent("rt:duplicate_session_with_dialog", {
                  detail: sid,
                }),
              );
            } else {
              // App.tsx listens for this event; arms a pending spawn
              // intent so the clone auto-focuses into the chosen tab
              // instead of landing unbound.
              window.dispatchEvent(
                new CustomEvent("rt:duplicate_session", {
                  detail: { sessionId: sid, target },
                }),
              );
            }
            setSessionMenu(null);
          }}
        />
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
              status={session.status}
              tabId={tabId ?? null}
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

/// Cap on the number of recent_actions rows rendered at once. Beyond
/// this, the user can click "Show all N entries" to opt into the full
/// list. Audit finding: a long-running headless session can accumulate
/// thousands of entries, and rendering them all as `<li>` elements at
/// once freezes the React reconciler.
const HEADLESS_RECENT_ACTIONS_TAIL = 200;

function HeadlessView({ session }: { session: SessionSnapshot }) {
  const m = session.metrics;
  const [showAll, setShowAll] = useState(false);
  const total = session.recent_actions.length;
  const overflowed = total > HEADLESS_RECENT_ACTIONS_TAIL && !showAll;
  // Slice from the end so the most recent N stay visible; the user can
  // expand to see earlier entries.
  const sliceStart = overflowed ? total - HEADLESS_RECENT_ACTIONS_TAIL : 0;
  const visible = session.recent_actions.slice(sliceStart);
  return (
    <div className="headless-view">
      <div className="headless-stats">
        <Stat label="status" value={session.status} />
        <Stat label="in tokens" value={m.input_tokens.toLocaleString()} />
        <Stat label="out tokens" value={m.output_tokens.toLocaleString()} />
        <Stat label="cost" value={`$${m.cost_usd.toFixed(4)}`} />
      </div>
      <div className="headless-log">
        {total === 0 ? (
          <p className="empty">No events yet…</p>
        ) : (
          <>
            {overflowed && (
              <button
                type="button"
                className="link headless-show-all"
                onClick={() => setShowAll(true)}
                data-testid="headless-show-all"
              >
                Show all {total} entries (earlier {sliceStart} hidden)
              </button>
            )}
            <ol>
              {visible.map((line, idx) => (
                // Stable order, append-only — composed index is fine.
                // eslint-disable-next-line react/no-array-index-key
                <li key={sliceStart + idx}>{line}</li>
              ))}
            </ol>
          </>
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
