import { useState } from "react";
import { pickDirectory } from "../api";
import { useEscape, useFocusReturn } from "../utils/a11y";

interface Props {
  initialPath: string | null;
  onClose: () => void;
  onSubmit: (cwd: string, saveAsDefault: boolean) => void;
}

export default function StandaloneShellDialog({
  initialPath,
  onClose,
  onSubmit,
}: Props) {
  const [cwd, setCwd] = useState(initialPath ?? "");
  const [saveAsDefault, setSaveAsDefault] = useState(true);
  const trimmed = cwd.trim();
  const canSubmit = trimmed.length > 0;

  useEscape(onClose);
  useFocusReturn();

  const browse = async () => {
    const path = await pickDirectory(trimmed || undefined, {
      lastDirKey: "standaloneShell",
    });
    if (path) setCwd(path);
  };

  return (
    <div className="modal-backdrop" data-testid="standalone-shell-dialog">
      <div
        className="modal modal-narrow"
        role="dialog"
        aria-modal="true"
        aria-label="Standalone shell"
      >
        <header className="modal-header">
          <h2>Standalone shell</h2>
          <button
            type="button"
            className="link"
            onClick={onClose}
            aria-label="Close dialog"
            data-testid="standalone-shell-close"
          >
            x
          </button>
        </header>
        <div className="modal-body">
          <label className="field">
            <span>Folder</span>
            <div className="path-input-row">
              <input
                type="text"
                value={cwd}
                onChange={(e) => setCwd(e.target.value)}
                placeholder="C:\Users\you"
                data-testid="standalone-shell-path"
              />
              <button
                type="button"
                onClick={browse}
                data-testid="standalone-shell-browse"
              >
                Browse
              </button>
            </div>
          </label>
          <label className="checkbox">
            <input
              type="checkbox"
              checked={saveAsDefault}
              onChange={(e) => setSaveAsDefault(e.target.checked)}
              data-testid="standalone-shell-save-default"
            />
            <span>Use as quick shell default</span>
          </label>
          <div className="modal-footer-inline">
            <button
              type="button"
              onClick={onClose}
              data-testid="standalone-shell-cancel"
            >
              Cancel
            </button>
            <button
              type="button"
              className="primary"
              disabled={!canSubmit}
              onClick={() => onSubmit(trimmed, saveAsDefault)}
              data-testid="standalone-shell-submit"
            >
              Open shell
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
