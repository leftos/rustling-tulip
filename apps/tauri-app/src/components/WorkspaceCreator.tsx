import { useState } from "react";
import type { DaemonClient } from "../api";
import type { RepoEntry } from "../types";
import { useEscape, useFocusReturn } from "../utils/a11y";

interface Props {
  repos: RepoEntry[];
  client: DaemonClient;
  onClose: () => void;
}

export default function WorkspaceCreator({ repos, client, onClose }: Props) {
  const [name, setName] = useState("");
  const [selected, setSelected] = useState<Set<string>>(new Set());
  useEscape(onClose);
  useFocusReturn();

  const toggle = (id: string) => {
    setSelected((s) => {
      const next = new Set(s);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const canSubmit = name.trim().length > 0 && selected.size >= 2;

  const submit = () => {
    if (!canSubmit) return;
    client.send({
      type: "upsert_workspace",
      id: null,
      name: name.trim(),
      member_repo_ids: Array.from(selected),
      linked_vscode_workspace: null,
    });
    onClose();
  };

  return (
    <div
      className="modal-backdrop"
      onClick={onClose}
      data-testid="workspace-creator"
    >
      <div
        className="modal"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-label="Create workspace"
      >
        <header className="modal-header">
          <h2>New workspace</h2>
          <button
            type="button"
            className="link"
            onClick={onClose}
            aria-label="Close dialog"
          >
            ✕
          </button>
        </header>
        <div className="modal-body">
          <label className="field">
            <span>Name</span>
            <input
              type="text"
              value={name}
              autoFocus
              onChange={(e) => setName(e.target.value)}
              placeholder="e.g. yaat-stack"
            />
          </label>
          <div className="field">
            <span>Members (pick at least 2)</span>
            <ul className="list">
              {repos.length === 0 ? (
                <p className="empty">No repos registered yet.</p>
              ) : (
                repos.map((r) => (
                  <li key={r.id} className="list-item static">
                    <label className="checkbox" style={{ flex: 1 }}>
                      <input
                        type="checkbox"
                        checked={selected.has(r.id)}
                        onChange={() => toggle(r.id)}
                      />
                      <span>{r.name}</span>
                    </label>
                  </li>
                ))
              )}
            </ul>
          </div>
        </div>
        <footer className="modal-footer">
          <button type="button" onClick={onClose}>
            Cancel
          </button>
          <button
            type="button"
            className="primary"
            disabled={!canSubmit}
            onClick={submit}
          >
            Create
          </button>
        </footer>
      </div>
    </div>
  );
}
