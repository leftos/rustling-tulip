import { useRef } from "react";
import { useAutoFocus, useEscape, useFocusReturn } from "../../utils/a11y";

interface Props {
  paths: string[];
  onCancel: () => void;
  onConfirm: () => void;
}

/**
 * Two-step confirmation modal for discarding worktree changes. Lists the
 * affected files so the user can sanity-check before committing to a
 * destructive action. Cancel is autofocused so a stray Enter never triggers
 * the discard.
 */
export default function DiscardConfirmDialog({
  paths,
  onCancel,
  onConfirm,
}: Props) {
  const count = paths.length;
  const cancelRef = useRef<HTMLButtonElement | null>(null);
  useEscape(onCancel);
  useAutoFocus(cancelRef);
  useFocusReturn();

  return (
    <div
      className="modal-backdrop modal-backdrop-destructive"
      onClick={onCancel}
      data-testid="discard-confirm-dialog"
    >
      <div
        className="modal"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-label={`Discard ${count} ${count === 1 ? "change" : "changes"}`}
      >
        <header className="modal-header">
          <h2>
            Discard {count} {count === 1 ? "change" : "changes"}
          </h2>
          <button
            type="button"
            className="link"
            onClick={onCancel}
            aria-label="Cancel"
            data-testid="discard-confirm-close"
          >
            ✕
          </button>
        </header>
        <div className="modal-body">
          <p className="muted small">
            Discarded edits cannot be recovered. Untracked files are removed;
            modified and deleted files are restored from the index.
          </p>
          <ul
            className="discard-confirm-list"
            data-testid="discard-confirm-list"
          >
            {paths.map((p) => (
              <li key={p}>{p}</li>
            ))}
          </ul>
        </div>
        <footer className="modal-footer">
          <button
            ref={cancelRef}
            type="button"
            onClick={onCancel}
            data-testid="discard-confirm-cancel"
          >
            Cancel
          </button>
          <button
            type="button"
            className="danger"
            onClick={onConfirm}
            data-testid="discard-confirm-submit"
          >
            Discard {count} {count === 1 ? "change" : "changes"}
          </button>
        </footer>
      </div>
    </div>
  );
}
