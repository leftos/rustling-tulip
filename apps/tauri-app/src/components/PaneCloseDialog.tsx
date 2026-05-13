import { useRef } from "react";
import type { SessionSnapshot } from "../types";
import { sessionDisplayLabel } from "../utils/sessionLabel";
import { useAutoFocus, useEscape, useFocusReturn } from "../utils/a11y";

interface Props {
  session: SessionSnapshot;
  onCancel: () => void;
  /// close_pane only — session keeps running in the sidebar / other tabs.
  onClosePaneKeepSession: () => void;
  /// stop_session when needed, then discard_session with remove_worktree=false.
  onClosePaneDiscardSessionKeepWorktree: () => void;
  /// stop_session when needed, then discard_session with remove_worktree=true.
  /// Daemon runs `git worktree remove --force` for each member after discard.
  /// Only offered when the session was spawned with a worktree.
  onClosePaneDiscardSessionRemoveWorktree: () => void;
}

/// Confirmation modal for the pane-corner × button when the pane has a
/// session bound to it. The safest option (close pane only, session keeps
/// running) is autofocused so a stray Enter never destroys state.
///
/// Skipped entirely for empty panes (the caller does the close
/// inline) — no session to protect.
///
/// "Close pane only" is shown for every session including stopped ones
/// because removing the layout reference is still useful. The discard
/// actions are also shown for stopped sessions; in that case they remove
/// the sidebar record without sending another stop request.
export default function PaneCloseDialog({
  session,
  onCancel,
  onClosePaneKeepSession,
  onClosePaneDiscardSessionKeepWorktree,
  onClosePaneDiscardSessionRemoveWorktree,
}: Props) {
  const keepRef = useRef<HTMLButtonElement | null>(null);
  useEscape(onCancel);
  useAutoFocus(keepRef);
  useFocusReturn();

  const isStopped = session.status === "stopped";
  const hasWorktree = session.has_per_session_worktree;
  const label = sessionDisplayLabel(session);
  const keepSessionLabel = hasWorktree
    ? "Close pane, keep session, keep worktree"
    : "Close pane but keep session in sidebar";
  const discardSessionLabel = hasWorktree
    ? "Close pane, don't keep session, keep worktree"
    : "Close pane and close session";
  const stoppedDiscardSessionLabel = hasWorktree
    ? discardSessionLabel
    : "Close pane and remove session";

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
            Closing the pane only removes it from this tab. Keeping the session
            leaves it running in the sidebar so you can re-bind it later.
            Closing the session stops the underlying process and removes it
            from the sidebar.
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
            {keepSessionLabel}
          </button>
          <button
            type="button"
            className="danger"
            onClick={onClosePaneDiscardSessionKeepWorktree}
            data-testid="pane-close-dialog-close-session-keep-worktree"
          >
            {isStopped ? stoppedDiscardSessionLabel : discardSessionLabel}
          </button>
          {hasWorktree && (
            <button
              type="button"
              className="danger"
              onClick={onClosePaneDiscardSessionRemoveWorktree}
              data-testid="pane-close-dialog-close-session-delete-worktree"
            >
              Close pane, don't keep session, delete worktree
            </button>
          )}
        </footer>
      </div>
    </div>
  );
}
