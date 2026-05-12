import type { DaemonClient } from "../api";
import type { VscodeWorkspaceSuggestion } from "../types";
import { useEscape, useFocusReturn } from "../utils/a11y";

interface Props {
  suggestion: VscodeWorkspaceSuggestion;
  client: DaemonClient;
  onDismiss: () => void;
}

export default function VscodeSuggestionToast({
  suggestion,
  client,
  onDismiss,
}: Props) {
  useEscape(onDismiss);
  useFocusReturn();
  const accept = (watch: boolean) => {
    client.send({
      type: "accept_vscode_workspace_suggestion",
      suggestion,
      watch,
    });
    onDismiss();
  };

  return (
    <div
      className="modal-backdrop modal-backdrop-vscode"
      data-testid="vscode-suggestion-toast"
    >
      <div
        className="modal"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-label="VS Code workspace detected"
      >
        <header className="modal-header">
          <h2>VS Code workspace detected</h2>
          <button
            type="button"
            className="link"
            onClick={onDismiss}
            aria-label="Dismiss suggestion"
          >
            ✕
          </button>
        </header>
        <div className="modal-body">
          <p>
            <code>{suggestion.source_path}</code> references{" "}
            {suggestion.folders.length} folders. Create a rustling-tulip
            workspace named <strong>{suggestion.suggested_name}</strong> with
            these members?
          </p>
          <ul className="list">
            {suggestion.folders.map((f) => (
              <li key={f.path} className="list-item static">
                <span className="list-item-label" title={f.path}>
                  {f.name ?? folderLeaf(f.path)}
                </span>
                <span className="list-item-meta">
                  {f.matched_repo_id ? "registered" : "will register"}
                </span>
              </li>
            ))}
          </ul>
        </div>
        <footer className="modal-footer">
          <button type="button" onClick={onDismiss}>
            Not now
          </button>
          <button type="button" onClick={() => accept(false)}>
            Create
          </button>
          <button
            type="button"
            className="primary"
            onClick={() => accept(true)}
            title="Watch the .code-workspace file and re-sync members on changes"
          >
            Create &amp; watch
          </button>
        </footer>
      </div>
    </div>
  );
}

function folderLeaf(path: string): string {
  const norm = path.replace(/\\/g, "/").replace(/\/$/, "");
  const idx = norm.lastIndexOf("/");
  return idx >= 0 ? norm.slice(idx + 1) : norm;
}
