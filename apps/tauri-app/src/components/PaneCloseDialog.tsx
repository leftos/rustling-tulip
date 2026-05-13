import { useRef } from "react";
import type { SessionSnapshot } from "../types";
import { sessionDisplayLabel } from "../utils/sessionLabel";
import { useAutoFocus, useEscape, useFocusReturn } from "../utils/a11y";

interface Props {
  session: SessionSnapshot;
  onCancel: () => void;
  /// close_pane only — session keeps running in the sidebar / other tabs.
  onClosePaneKeepSession: () => void;
  /// stop_session + close_pane. The worktree directory survives on disk so
  /// the user can keep poking at files.
  onCloseAndStopKeepWorktree: () => void;
  /// stop_session + discard_session with remove_worktree=true. Daemon runs
  /// `git worktree remove --force` for each member after the explicit
  /// discard. Only offered when the session was spawned with a worktree.
  onCloseAndStopRemoveWorktree: () => void;
}

/// Confirmation modal for the pane-corner × button when the pane has a
/// session bound to it. Three destructive choices plus Cancel — the
/// safest option (close pane only, session keeps running) is autofocused
/// so a stray Enter never destroys state.
///
/// Skipped entirely for empty panes (the caller does the close
/// inline) — no session to protect.
///
/// "Close pane only" is shown for every session including stopped ones
/// because removing the layout reference is still useful. The two
/// "stop session" buttons disappear once the session is already stopped
/// — there's nothing left to kill, only the worktree to clean up via
/// "delete worktree".
export default function PaneCloseDialog({
  session,
  onCancel,
  onClosePaneKeepSession,
  onCloseAndStopKeepWorktree,
  onCloseAndStopRemoveWorktree,
}: Props) {
  const keepRef = useRef<HTMLButtonElement | null>(null);
  useEscape(onCancel);
  useAutoFocus(keepRef);
  useFocusReturn();

  const isStopped = session.status === "stopped";
  const hasWorktree = session.has_per_session_worktree;
  const label = sessionDisplayLabel(session);
  const stopLabel = hasWorktree ? "Stop session, keep worktree" : "Stop session";

  return (
    <div
      className="modal-backdrop modal-backdrop-destructive"
      data-testid="pane-close-dialog"
    >
      <div
        className="modal"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-label={`Close pane for session ${label}`}
      >
        <header className="modal-header">
          <h2>Close pane?</h2>
          <button
            type="button"
            className="link"
            onClick={onCancel}
            aria-label="Cancel"
            data-testid="pane-close-dialog-close"
          >
            ✕
          </button>
        </header>
        <div className="modal-body">
          <p>
            This pane is showing{" "}
            <strong>{label}</strong>
            {isStopped ? " (already stopped)" : ""}. What would you like to do?
          </p>
          <p className="muted small">
            Closing the pane only removes it from this tab — the session keeps
            running in the sidebar and you can re-bind it to another pane
            later. Stopping the session sends the underlying agent the kill
            signal.
          </p>
        </div>
        <footer className="modal-footer pane-close-dialog-footer">
          <button
            type="button"
            onClick={onCancel}
            data-testid="pane-close-dialog-cancel"
          >
            Cancel
          </button>
          <button
            ref={keepRef}
            type="button"
            onClick={onClosePaneKeepSession}
            data-testid="pane-close-dialog-pane-only"
          >
            Close pane, keep session
          </button>
          {!isStopped && (
            <button
              type="button"
              className="danger"
              onClick={onCloseAndStopKeepWorktree}
              data-testid="pane-close-dialog-stop-keep"
            >
              {stopLabel}
            </button>
          )}
          {hasWorktree && (
            <button
              type="button"
              className="danger"
              onClick={onCloseAndStopRemoveWorktree}
              data-testid="pane-close-dialog-stop-remove"
            >
              {isStopped ? "Delete worktree" : "Stop session, delete worktree"}
            </button>
          )}
        </footer>
      </div>
    </div>
  );
}
