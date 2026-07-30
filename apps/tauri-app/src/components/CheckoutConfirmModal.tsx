import type { CheckoutChoice } from "../types";
import { useEscape } from "../utils/a11y";
import Icon from "./Icon";

interface Props {
  branch: string;
  dirtyCount: number;
  /// User picked how to resolve the dirty tree.
  onChoose: (strategy: CheckoutChoice) => void;
  onCancel: () => void;
}

/// Shown when an in-place spawn would switch a dirty working tree to a
/// different branch (the daemon declines and asks via `checkout_confirm_required`).
/// Offers carrying the changes across or stashing them first.
export default function CheckoutConfirmModal({
  branch,
  dirtyCount,
  onChoose,
  onCancel,
}: Props) {
  useEscape(onCancel);
  const plural = dirtyCount === 1 ? "" : "s";

  return (
    <div className="modal-backdrop" data-testid="checkout-confirm">
      <div
        className="modal"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-label="Switch branch in place"
      >
        <header className="modal-header">
          <h2>Switch branch in place?</h2>
          <button
            type="button"
            className="modal-close"
            onClick={onCancel}
            aria-label="Close dialog"
            title="Close"
          >
            <Icon name="close" />
          </button>
        </header>
        <div className="modal-body">
          <p>
            This repo has {dirtyCount} uncommitted change{plural} and isn't on{" "}
            <code>{branch}</code>. Switching in place changes what's checked out
            in your working directory.
          </p>
          <ul className="settings-section-hint">
            <li>
              <strong>Carry changes</strong> — keep your edits and switch
              (git refuses if they'd conflict with {branch}).
            </li>
            <li>
              <strong>Stash &amp; switch</strong> — stash your edits first, then
              switch; pop the stash yourself afterward.
            </li>
          </ul>
        </div>
        <div className="modal-footer-inline">
          <button type="button" onClick={onCancel} data-testid="checkout-cancel">
            Cancel
          </button>
          <button
            type="button"
            onClick={() => onChoose("stash")}
            data-testid="checkout-stash"
          >
            Stash &amp; switch
          </button>
          <button
            type="button"
            className="primary"
            onClick={() => onChoose("carry")}
            data-testid="checkout-carry"
          >
            Carry changes
          </button>
        </div>
      </div>
    </div>
  );
}
