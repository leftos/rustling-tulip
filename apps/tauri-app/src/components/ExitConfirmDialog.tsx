import { useRef } from "react";
import { useAutoFocus, useEscape } from "../utils/a11y";

interface Props {
  activeSessionCount: number;
  busy: boolean;
  onStopAndQuit: () => void;
  onQuitLeaveRunning: () => void;
  onCancel: () => void;
}

/// Shown when the user tries to close the main window. The daemon outlives
/// the app by design (so claude sessions survive an app restart), but the
/// user can opt to terminate everything from here. `busy` is true while a
/// shutdown request is in flight — buttons are disabled to avoid double-send.
export default function ExitConfirmDialog({
  activeSessionCount,
  busy,
  onStopAndQuit,
  onQuitLeaveRunning,
  onCancel,
}: Props) {
  const cancelRef = useRef<HTMLButtonElement | null>(null);
  // Escape dismisses while not busy. Land focus on Cancel so a stray Enter
  // doesn't pick the destructive option.
  useEscape(onCancel, !busy);
  useAutoFocus(cancelRef);
  return (
    <div
      className="modal-backdrop"
      onClick={busy ? undefined : onCancel}
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
          {activeSessionCount > 0 ? (
            <p>
              <strong>{activeSessionCount}</strong>{" "}
              {activeSessionCount === 1 ? "session is" : "sessions are"}{" "}
              currently active.
            </p>
          ) : (
            <p className="muted">No active sessions.</p>
          )}
        </div>
        <footer className="modal-footer">
          <button
            ref={cancelRef}
            type="button"
            onClick={onCancel}
            disabled={busy}
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={onQuitLeaveRunning}
            disabled={busy}
            title="Close the window; daemon and sessions keep running"
          >
            Quit, leave running
          </button>
          <button
            type="button"
            className="danger"
            onClick={onStopAndQuit}
            disabled={busy}
            title="Terminate every session and stop the daemon"
          >
            {busy ? "Stopping…" : "Stop sessions & quit"}
          </button>
        </footer>
      </div>
    </div>
  );
}
