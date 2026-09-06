import { useRef } from "react";
import { useAutoFocus, useEscape, useFocusReturn } from "../utils/a11y";

interface Props {
  activeSessionCount: number;
  activeWorktreeSessionCount: number;
  /// Sessions still running on the daemon side but whose PTY handles
  /// were lost across a restart (`is_orphan`). The daemon's
  /// `stop_session` is a no-op for these, so "Stop sessions and quit"
  /// will NOT actually terminate them — they'll keep running as zombie
  /// `claude` processes. Surfaced so the user can act on that knowledge.
  orphanSessionCount: number;
  busy: boolean;
  /// Suppress this dialog's Escape handler while another dialog is stacked
  /// on top of it. `useEscape` binds at the document level, so without this
  /// one Escape press would dismiss both layers at once. Unlike `busy` it
  /// leaves the buttons live and their labels alone.
  escapeDisabled: boolean;
  /// True once the 2 s shutdown grace window has elapsed without the
  /// daemon's WS connection closing. Without this the dialog sits in
  /// "Stopping…" forever — App.tsx used to auto-close the window after
  /// 2 s, which silently abandoned a hung daemon. Now the user gets a
  /// warning + explicit Force-quit affordance instead.
  stuck: boolean;
  onStopAndQuit: () => void;
  onStopAndQuitRemoveWorktrees: () => void;
  /// Quit the daemon but retain session sidecars so the next launch
  /// surfaces them in the Abandoned bucket where the user can Resume.
  /// Pre-Phase-C the children still die (their ConPTY master goes away
  /// with the daemon); abandon-mode preserves the recovery context.
  onAbandonAndQuit: () => void;
  onQuitLeaveRunning: () => void;
  /// Bypass the daemon round-trip and close the OS window immediately.
  /// Only reachable when `stuck` is true.
  onForceQuit: () => void;
  onCancel: () => void;
}

/// Shown when the user tries to close the main window. The daemon outlives
/// the app by design (so claude sessions survive an app restart), and that's
/// the recommended path: keeping sessions running in the background is the
/// primary action.
export default function ExitConfirmDialog({
  activeSessionCount,
  activeWorktreeSessionCount,
  orphanSessionCount,
  busy,
  escapeDisabled,
  stuck,
  onStopAndQuit,
  onStopAndQuitRemoveWorktrees,
  onAbandonAndQuit,
  onQuitLeaveRunning,
  onForceQuit,
  onCancel,
}: Props) {
  const quitLeaveRef = useRef<HTMLButtonElement | null>(null);
  const hasActiveWorktreeSessions = activeWorktreeSessionCount > 0;
  // Escape dismisses while not busy and nothing is stacked above. Autofocus
  // the primary action since it keeps sessions running and does not destroy
  // worktrees.
  useEscape(onCancel, !busy && !escapeDisabled);
  useAutoFocus(quitLeaveRef);
  useFocusReturn();
  return (
    <div
      className="modal-backdrop modal-backdrop-exit"
      data-testid="exit-confirm-dialog"
    >
      <div
        className="modal"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-label="Quit rustling-tulip"
      >
        <header className="modal-header">
          <h2>Quit rustling-tulip?</h2>
        </header>
        <div className="modal-body">
          <p>
            The background daemon owns your claude sessions. By default it keeps
            running after the app closes so sessions survive app restarts.
          </p>
          <p>
            {activeSessionCount > 0 ? (
              <>
                <strong>{activeSessionCount}</strong>{" "}
                {activeSessionCount === 1 ? "session is" : "sessions are"}{" "}
                currently active.
              </>
            ) : (
              <span className="muted">No active sessions.</span>
            )}
            {orphanSessionCount > 0 && (
              <>
                {" "}
                <span className="muted small">
                  ({orphanSessionCount} orphan
                  {orphanSessionCount === 1 ? "" : "s"} — daemon can't stop
                  these; they'll stay running)
                </span>
              </>
            )}
          </p>
          {hasActiveWorktreeSessions && (
            <p className="muted small">
              {activeWorktreeSessionCount} active{" "}
              {activeWorktreeSessionCount === 1 ? "session has" : "sessions have"}{" "}
              per-session worktrees. You can stop the sessions and either keep
              or remove those worktree directories.
            </p>
          )}
          {busy && stuck && (
            <p className="modal-warning" data-testid="exit-stuck-warning">
              <strong>Daemon not responding after 2 seconds.</strong> The
              shutdown request may have hung. You can force-close this window
              now; on next launch the supervisor will try to reattach to any
              leftover <code>claude</code> processes.
            </p>
          )}
        </div>
        <footer className="modal-footer exit-confirm-footer">
          {busy && stuck ? (
            <button
              type="button"
              className="danger"
              onClick={onForceQuit}
              data-testid="exit-force-quit"
              title="Close the window now; the daemon may continue shutting down in the background"
            >
              Force quit
            </button>
          ) : (
            <>
              <button
                type="button"
                onClick={onCancel}
                disabled={busy}
                data-testid="exit-cancel"
              >
                Cancel
              </button>
              <button
                type="button"
                className="danger"
                onClick={onStopAndQuit}
                disabled={busy}
                title="Terminate every session and stop the daemon"
                data-testid="exit-stop-keep-worktrees"
              >
                {busy
                  ? "Stopping..."
                  : hasActiveWorktreeSessions
                  ? "Stop sessions, keep worktrees"
                  : "Stop sessions and quit"}
              </button>
              {hasActiveWorktreeSessions && (
                <button
                  type="button"
                  className="danger"
                  onClick={onStopAndQuitRemoveWorktrees}
                  disabled={busy}
                  data-testid="exit-stop-remove-worktrees"
                  title="Terminate every session, remove per-session worktrees, and stop the daemon"
                >
                  {busy ? "Stopping..." : "Stop sessions, remove worktrees"}
                </button>
              )}
              {activeSessionCount > 0 && !hasActiveWorktreeSessions && (
                <button
                  type="button"
                  onClick={onAbandonAndQuit}
                  disabled={busy}
                  data-testid="exit-abandon-quit"
                  title={
                    "Stop the daemon but keep session metadata. " +
                    "Next launch will show these sessions as Abandoned " +
                    "with a Resume button that replays their config + prompt."
                  }
                >
                  Abandon & quit
                </button>
              )}
              <button
                ref={quitLeaveRef}
                type="button"
                className="primary"
                onClick={onQuitLeaveRunning}
                disabled={busy}
                data-testid="exit-keep-running"
                title="Close the window; daemon and sessions keep running"
              >
                Keep sessions running in background
              </button>
            </>
          )}
        </footer>
      </div>
    </div>
  );
}
