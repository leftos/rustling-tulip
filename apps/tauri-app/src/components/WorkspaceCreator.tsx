import { useEffect, useRef, useState } from "react";
import { pickFile } from "../api";
import type { DaemonClient } from "../api";
import type { RepoEntry, VscodeWorkspaceSuggestion } from "../types";
import { useEscape, useFocusReturn } from "../utils/a11y";

interface Props {
  repos: RepoEntry[];
  client: DaemonClient;
  onClose: () => void;
}

type Mode = "repos" | "vscode";

export default function WorkspaceCreator({ repos, client, onClose }: Props) {
  const [mode, setMode] = useState<Mode>("repos");
  useEscape(onClose);
  useFocusReturn();

  return (
    <div className="modal-backdrop" data-testid="workspace-creator">
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

        <div className="modal-tabs" role="tablist" aria-label="Creation mode">
          <button
            type="button"
            role="tab"
            aria-selected={mode === "repos"}
            className={mode === "repos" ? "active" : ""}
            onClick={() => setMode("repos")}
          >
            From repos
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={mode === "vscode"}
            className={mode === "vscode" ? "active" : ""}
            onClick={() => setMode("vscode")}
          >
            From VS Code workspace
          </button>
        </div>

        {mode === "repos" ? (
          <FromReposPanel repos={repos} client={client} onClose={onClose} />
        ) : (
          <FromVscodePanel client={client} onClose={onClose} />
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// "From repos" panel — existing behaviour
// ---------------------------------------------------------------------------

interface FromReposPanelProps {
  repos: RepoEntry[];
  client: DaemonClient;
  onClose: () => void;
}

function FromReposPanel({ repos, client, onClose }: FromReposPanelProps) {
  const [name, setName] = useState("");
  const [selected, setSelected] = useState<Set<string>>(new Set());

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
    <>
      <div className="modal-body">
        <label className="field">
          <span>Name</span>
          <input
            type="text"
            value={name}
            autoFocus
            onChange={(e) => setName(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") submit();
            }}
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
    </>
  );
}

// ---------------------------------------------------------------------------
// "From VS Code workspace" panel
// ---------------------------------------------------------------------------

interface FromVscodePanelProps {
  client: DaemonClient;
  onClose: () => void;
}

function FromVscodePanel({ client, onClose }: FromVscodePanelProps) {
  const [filePath, setFilePath] = useState<string | null>(null);
  const [parsing, setParsing] = useState(false);
  const [suggestion, setSuggestion] = useState<VscodeWorkspaceSuggestion | null>(null);
  const [name, setName] = useState("");
  const [parseError, setParseError] = useState<string | null>(null);

  // One-shot listener: resolve the pending parse request when the daemon replies.
  const pendingResolve = useRef<
    | ((s: VscodeWorkspaceSuggestion) => void)
    | null
  >(null);
  const pendingReject = useRef<((e: string) => void) | null>(null);

  useEffect(() => {
    const unsub = client.onMessage((msg) => {
      if (msg.type === "vscode_workspace_parsed" && pendingResolve.current) {
        const resolve = pendingResolve.current;
        pendingResolve.current = null;
        pendingReject.current = null;
        resolve(msg.suggestion);
      } else if (msg.type === "error" && pendingReject.current) {
        const reject = pendingReject.current;
        pendingResolve.current = null;
        pendingReject.current = null;
        reject(msg.message);
      }
    });
    return unsub;
  }, [client]);

  const browse = async () => {
    const path = await pickFile({
      extensions: ["code-workspace"],
      filterName: "VS Code Workspace",
      lastDirKey: "vscodeWorkspace",
    });
    if (!path) return;
    setFilePath(path);
    setSuggestion(null);
    setParseError(null);
    setParsing(true);
    try {
      const result = await new Promise<VscodeWorkspaceSuggestion>((resolve, reject) => {
        pendingResolve.current = resolve;
        pendingReject.current = reject;
        client.send({ type: "parse_vscode_workspace", path });
      });
      setSuggestion(result);
      setName(result.suggested_name);
    } catch (err) {
      setParseError(typeof err === "string" ? err : "Failed to parse workspace file.");
    } finally {
      setParsing(false);
    }
  };

  const canSubmit = suggestion !== null && name.trim().length > 0;

  const submit = () => {
    if (!suggestion || !canSubmit) return;
    client.send({
      type: "accept_vscode_workspace_suggestion",
      suggestion: { ...suggestion, suggested_name: name.trim() },
      watch: false,
    });
    onClose();
  };

  return (
    <>
      <div className="modal-body">
        <div className="field">
          <span>VS Code workspace file</span>
          <div className="file-picker-row">
            <span className="file-picker-path" title={filePath ?? undefined}>
              {filePath
                ? filePath.replace(/.*[/\\]/, "")
                : "No file selected"}
            </span>
            <button
              type="button"
              className="small"
              onClick={() => void browse()}
            >
              Browse…
            </button>
          </div>
        </div>

        {parsing && (
          <p className="status-message">Parsing workspace file…</p>
        )}

        {parseError && (
          <p className="status-message error" role="alert">
            {parseError}
          </p>
        )}

        {suggestion && (
          <>
            <label className="field">
              <span>Workspace name</span>
              <input
                type="text"
                value={name}
                autoFocus
                onChange={(e) => setName(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") submit();
                }}
              />
            </label>

            <div className="field">
              <span>Folders ({suggestion.folders.length})</span>
              <ul className="list">
                {suggestion.folders.map((f) => {
                  const displayName =
                    f.name ?? f.path.replace(/.*[/\\]/, "");
                  const isRegistered = f.matched_repo_id !== null;
                  return (
                    <li
                      key={f.path}
                      className="list-item static"
                      title={f.path}
                    >
                      <span className="list-item-name">{displayName}</span>
                      <span
                        className={
                          isRegistered
                            ? "list-item-badge registered"
                            : "list-item-badge new"
                        }
                      >
                        {isRegistered ? "registered" : "will register"}
                      </span>
                    </li>
                  );
                })}
              </ul>
            </div>
          </>
        )}
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
    </>
  );
}
