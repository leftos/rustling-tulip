import { useRef } from "react";
import { useAutoFocus, useEscape, useFocusReturn } from "../utils/a11y";

interface Props {
  activeSessionCount: number;
  /// Sessions still running on the daemon side but whose PTY handles
  /// were lost across a restart (`is_orphan`). The daemon's
  /// `stop_session` is a no-op for these, so "Stop sessions & quit"
  /// will NOT actually terminate them — they'll keep running as zombie
  /// `claude` processes. Surfaced so the user can act on that knowledge.
  orphanSessionCount: number;
  busy: boolean;
  onStopAndQuit: () => void;
  onQuitLeaveRunning: () => void;
  onCancel: () => void;
}

/// Shown when the user tries to close the main window. The daemon outlives
/// the app by design (so claude sessions survive an app restart), and that's
/// the recommended path — `Quit, leave running` is the primary action. The
/// destructive `Stop sessions & quit` is intentionally visually demoted so
/// users don't mis-click it (it was the visual focal point pre-iter-24).
export default function ExitConfirmDialog({
  activeSessionCount,
  orphanSessionCount,
  busy,
  onStopAndQuit,
  onQuitLeaveRunning,
  onCancel,
}: Props) {
  const quitLeaveRef = useRef<HTMLButtonElement | null>(null);
  // Escape dismisses while not busy. Autofocus the primary action ("Quit,
  // leave running") since it's the recommended path and the safest of the
  // three (sessions keep running, nothing is destroyed).
  useEscape(onCancel, !busy);
  useAutoFocus(quitLeaveRef);
  useFocusReturn();
  return (
    <div
      className="modal-backdrop modal-backdrop-exit"
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
        </div>
        <footer className="modal-footer">
          <button type="button" onClick={onCancel} disabled={busy}>
            Cancel
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
          <button
            ref={quitLeaveRef}
            type="button"
            className="primary"
            onClick={onQuitLeaveRunning}
            disabled={busy}
            title="Close the window; daemon and sessions keep running"
          >
            Quit, leave running
          </button>
        </footer>
      </div>
    </div>
  );
}
