import type { SessionSnapshot } from "../types";

interface Props {
  repoName: string;
  liveSessions: SessionSnapshot[];
  onCancel: () => void;
  /// Remove the repo, leave sessions running. They migrate to the
  /// "Detached" container in the sidebar.
  onRemoveAnyway: () => void;
  /// Stop every listed session, then remove the repo.
  onStopAndRemove: () => void;
}

/**
 * Confirmation modal for repo removal when at least one non-stopped session
 * is running against that repo. Three-way choice — see audit
 * `docs/ux-audit.md` "Removing a repo silently abandons its live sessions".
 */
export default function RepoRemoveDialog({
  repoName,
  liveSessions,
  onCancel,
  onRemoveAnyway,
  onStopAndRemove,
}: Props) {
  const count = liveSessions.length;
  return (
    <div
      className="modal-backdrop"
      onClick={onCancel}
      data-testid="repo-remove-dialog"
    >
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <header className="modal-header">
          <h2>Remove repo &middot; {repoName}</h2>
          <button
            type="button"
            className="link"
            onClick={onCancel}
            aria-label="Cancel"
            data-testid="repo-remove-close"
          >
            ✕
          </button>
        </header>
        <div className="modal-body">
          <p>
            <strong>{count}</strong>{" "}
            {count === 1 ? "session is" : "sessions are"} running against this
            repo. Choose what happens to them:
          </p>
          <ul className="repo-remove-list" data-testid="repo-remove-session-list">
            {liveSessions.map((s) => (
              <li key={s.id} data-session-id={s.id}>
                <span className={`status-dot status-${s.status}`} />
                <span className="repo-remove-session-label">{s.label}</span>
                <span className="muted small">{s.status}</span>
              </li>
            ))}
          </ul>
          <p className="muted small">
            <strong>Remove anyway</strong> keeps the {count === 1 ? "session"
              : "sessions"} alive (they move to the &ldquo;Detached&rdquo;
            group in the sidebar). <strong>Stop and remove</strong> ends them
            cleanly before unregistering the repo.
          </p>
        </div>
        <footer className="modal-footer">
          <button
            type="button"
            onClick={onCancel}
            data-testid="repo-remove-cancel"
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={onRemoveAnyway}
            data-testid="repo-remove-anyway"
          >
            Remove anyway
          </button>
          <button
            type="button"
            className="danger"
            onClick={onStopAndRemove}
            data-testid="repo-remove-stop-and-remove"
          >
            Stop {count === 1 ? "session" : "sessions"} and remove
          </button>
        </footer>
      </div>
    </div>
  );
}
