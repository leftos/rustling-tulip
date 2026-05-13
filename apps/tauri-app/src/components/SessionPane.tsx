import { useCallback, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { DaemonClient } from "../api";
import type { SessionSnapshot, TabEntry } from "../types";
import {
  clearPanePoppedOut,
  markPanePoppedOut,
} from "../utils/poppedPanes";
import {
  sessionDisplayLabel,
  sessionLabelTooltip,
  sessionRuntimeLabel,
} from "../utils/sessionLabel";
import {
  DEFAULT_SESSION_COLOR,
  normalizeSessionColor,
  sessionAccentStyle,
} from "../utils/sessionColor";
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
  return (
    params.get("pane") !== null ||
    params.get("session") !== null ||
    params.get("tab") !== null
  );
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
  /// Grid pane id this session is bound to. Used by Restart so the new
  /// clone takes the SAME pane the stopped session was in, instead of
  /// appearing as a new pane appended to the tab. `undefined` outside
  /// grid contexts (e.g. pop-out windows) — Restart falls back to the
  /// addToTab / newTab behavior in that case.
  paneId?: string;
  /// Pane-chrome callbacks. When provided, the pane-level controls
  /// (split right, split down, extract to new tab, close pane) render
  /// inline in the session header alongside the session-specific
  /// actions. Omitted for the pop-out SessionWindow, which has no
  /// enclosing grid.
  ///
  /// `Shift+click` flips the split direction (left instead of right,
  /// up instead of down) — wiring the raw event lets each click target
  /// read `shiftKey` rather than threading a second boolean.
  onSplitRight?: (e: React.MouseEvent) => void;
  onSplitDown?: (e: React.MouseEvent) => void;
  onExtractToTab?: () => void;
  onClosePane?: () => void;
  /// Wrapper pop-outs already render session chrome in their window toolbar.
  /// Hide the in-pane header there so title/status/actions are not duplicated.
  hideHeader?: boolean;
}

export default function SessionPane({
  session,
  client,
  tabs,
  subscribePty,
  onHeaderDragStart,
  tabId,
  paneId,
  onSplitRight,
  onSplitDown,
  onExtractToTab,
  onClosePane,
  hideHeader = false,
}: Props) {
  const showPaneControls =
    onSplitRight !== undefined ||
    onSplitDown !== undefined ||
    onExtractToTab !== undefined ||
    onClosePane !== undefined;
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
    if (!paneId) {
      void invoke("open_session_window", { sessionId: session.id });
      return;
    }
    markPanePoppedOut(paneId);
    void invoke("open_pane_window", { paneId }).catch(() => {
      clearPanePoppedOut(paneId);
    });
  }, [paneId, session.id]);

  const onRestart = useCallback(() => {
    // App.tsx owns pane-slot routing via pendingSpawnIntentRef + sends
    // duplicate_session / discard_session in the right order. Dispatch a
    // window event so we don't have to prop-drill those handlers through
    // the GridRenderer tree. When `paneId` is known (grid-rendered pane,
    // i.e. NOT a pop-out), include it so App.tsx can use the replacePane
    // intent and the new clone takes over the dead session's slot rather
    // than appending a fresh pane to the tab.
    window.dispatchEvent(
      new CustomEvent("rt:pane_session_restart", {
        detail: {
          sessionId: session.id,
          tabId: tabId ?? null,
          paneId: paneId ?? null,
        },
      }),
    );
  }, [session.id, tabId, paneId]);

  const onDiscardWithWorktree = useCallback(() => {
    if (!client) return;
    client.send({
      type: "discard_session",
      session_id: session.id,
      cleanup: session.members.map((m) => ({
        repo_id: m.repo_id,
        remove_worktree: session.has_per_session_worktree,
      })),
    });
  }, [client, session]);

  const onPark = useCallback(() => {
    if (!client) return;
    client.send({ type: "park_session", session_id: session.id });
  }, [client, session.id]);

  const isHeadless = session.mode === "headless";
  const isPlainShell = session.mode === "plain_shell";
  const runtimeLabel = sessionRuntimeLabel(session);
  const modeSuffix = isHeadless ? " · headless" : "";
  const accentColor = normalizeSessionColor(session.accent_color);
  const sessionStyle = sessionAccentStyle(accentColor);
  const headerClasses = [
    "session-header",
    onHeaderDragStart ? "session-header-draggable" : "",
    accentColor ? "has-session-accent" : "",
  ]
    .filter(Boolean)
    .join(" ");
  const setColor = useCallback(
    (color: string | null) => {
      if (!client) return;
      client.send({
        type: "set_session_color",
        session_id: session.id,
        color,
      });
    },
    [client, session.id],
  );

  return (
    <div
      className={accentColor ? "session-pane has-session-accent" : "session-pane"}
      style={sessionStyle}
      data-testid="session-pane"
      data-session-id={session.id}
      data-session-status={session.status}
      data-session-mode={session.mode}
      data-session-color={accentColor ?? ""}
    >
      {!hideHeader && (
        <header
          className={headerClasses}
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
              so a workspace session doesn't waste an
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
            {client && (
              <input
                type="color"
                className="session-color-picker"
                value={accentColor ?? DEFAULT_SESSION_COLOR}
                title="Set session color"
                aria-label="Set session color"
                data-testid="session-color-picker"
                onChange={(e) => setColor(e.currentTarget.value)}
              />
            )}
          </div>
          <div className="session-actions">
            {!isPopoutWindow && (
              <button
                type="button"
                onClick={onPopOut}
                title="Open this session in its own window"
                data-testid="session-pop-out"
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
            {showPaneControls && (
              <>
                <span
                  className="session-actions-divider"
                  aria-hidden="true"
                />
                {onSplitRight && (
                  <button
                    type="button"
                    className="session-action-icon"
                    onClick={onSplitRight}
                    title="Split right (Shift+click: split left)"
                    aria-label="Split pane horizontally; hold Shift to place the new pane on the left"
                    data-testid="pane-split-right"
                  >
                    {"▶|"}
                  </button>
                )}
                {onSplitDown && (
                  <button
                    type="button"
                    className="session-action-icon"
                    onClick={onSplitDown}
                    title="Split down (Shift+click: split up)"
                    aria-label="Split pane vertically; hold Shift to place the new pane on top"
                    data-testid="pane-split-down"
                  >
                    {"▼="}
                  </button>
                )}
                {onExtractToTab && (
                  <button
                    type="button"
                    className="session-action-icon"
                    onClick={onExtractToTab}
                    title="Move this pane to a new tab"
                    aria-label="Move pane to a new tab"
                    data-testid="pane-extract"
                  >
                    ↗
                  </button>
                )}
                {onClosePane && (
                  <button
                    type="button"
                    className="session-action-icon session-action-close"
                    onClick={onClosePane}
                    title="Close pane"
                    aria-label="Close pane"
                    data-testid="pane-close"
                  >
                    ×
                  </button>
                )}
              </>
            )}
          </div>
        </header>
      )}
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
          {...(paneId ? { preferredPaneId: paneId } : {})}
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
              agent={session.agent}
              mode={session.mode}
            />
          ) : session.status === "stopped" ? (
            <div
              className="terminal-placeholder session-exited-placeholder"
              data-testid="session-exited"
            >
              <div className="session-exited-message">
                Session has exited (code {session.exit_code ?? "?"}).
              </div>
              {client && (
                <div className="session-exited-actions">
                  <button
                    type="button"
                    onClick={onRestart}
                    data-testid="session-restart"
                  >
                    Restart
                  </button>
                  {session.has_per_session_worktree && (
                    <button
                      type="button"
                      onClick={onPark}
                      data-testid="session-park"
                      title="Keep the worktree on disk; session moves to inactive in the sidebar"
                    >
                      Remove pane, keep worktree
                    </button>
                  )}
                  <button
                    type="button"
                    onClick={onDiscardWithWorktree}
                    className="danger"
                    data-testid="session-discard-worktree"
                    title={
                      session.has_per_session_worktree
                        ? "Remove this pane and delete the worktree from disk"
                        : "Remove this pane"
                    }
                  >
                    {session.has_per_session_worktree
                      ? "Remove pane and delete worktree"
                      : "Remove pane"}
                  </button>
                </div>
              )}
            </div>
          ) : (
            <div className="terminal-placeholder">
              Waiting for daemon connection...
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
