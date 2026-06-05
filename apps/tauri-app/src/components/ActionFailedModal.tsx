import { useRef } from "react";
import { useAutoFocus, useEscape, useFocusReturn } from "../utils/a11y";

interface ActionFailedInfo {
  title: string;
  detail: string;
  hint: string | null;
}

interface Props {
  info: ActionFailedInfo;
  onDismiss: () => void;
}

/// Blocking modal for a refused or failed action that needs the user's
/// attention more than a transient toast can give — e.g. a worktree spawn
/// that collided with an in-use worktree, or a "remove worktree" discard
/// blocked because another live session still uses it. `detail` may contain
/// newlines (a list of blocking sessions / the git reason), so it's rendered
/// pre-wrapped.
export default function ActionFailedModal({ info, onDismiss }: Props) {
  const dismissRef = useRef<HTMLButtonElement | null>(null);
  useEscape(onDismiss);
  useAutoFocus(dismissRef);
  useFocusReturn();
  return (
    <div className="modal-backdrop" data-testid="action-failed-modal">
      <div
        className="modal"
        onClick={(e) => e.stopPropagation()}
        role="alertdialog"
        aria-modal="true"
        aria-label={info.title}
      >
        <header className="modal-header">
          <h2>{info.title}</h2>
        </header>
        <div className="modal-body">
          <p className="action-failed-detail">{info.detail}</p>
          {info.hint && <p className="muted small">{info.hint}</p>}
        </div>
        <footer className="modal-footer">
          <button
            ref={dismissRef}
            type="button"
            className="primary"
            onClick={onDismiss}
            data-testid="action-failed-dismiss"
          >
            OK
          </button>
        </footer>
      </div>
    </div>
  );
}
