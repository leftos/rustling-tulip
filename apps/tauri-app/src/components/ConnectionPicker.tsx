import { useCallback, useEffect, useRef, useState } from "react";
import type { ConnectionTarget, RemoteProfile } from "../api";
import Icon from "./Icon";

interface Props {
  target: ConnectionTarget;
  profiles: RemoteProfile[];
  /// Switch to the bundled per-machine daemon.
  onSelectLocal: () => void;
  /// Switch to a saved remote profile by id.
  onSelectProfile: (id: string) => void;
  /// Decode + auto-save a pasted code, then connect. Resolves with an error
  /// string when the code is malformed (the modal stays open to show it); a
  /// successful decode closes the modal even if the host turns out to be
  /// unreachable — that surfaces in the daemon footer as a retrying error.
  onAddRemote: (
    code: string,
  ) => Promise<{ ok: true } | { ok: false; error: string }>;
  onDeleteProfile: (id: string) => void;
  onClose: () => void;
}

/// Client-side connection picker: choose the local daemon, a saved remote
/// host, or pair a new one by pasting its connection code. Opened from the
/// DaemonFooter. The actual connect happens in App (this only reports intent).
export default function ConnectionPicker({
  target,
  profiles,
  onSelectLocal,
  onSelectProfile,
  onAddRemote,
  onDeleteProfile,
  onClose,
}: Props) {
  const [addOpen, setAddOpen] = useState(profiles.length === 0);
  const [code, setCode] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  useEffect(() => {
    if (addOpen) textareaRef.current?.focus();
  }, [addOpen]);

  const activeProfileId =
    target.kind === "remote" ? target.profileId : null;

  const onSubmitCode = useCallback(async () => {
    const trimmed = code.trim();
    if (!trimmed || busy) return;
    setBusy(true);
    setError(null);
    const result = await onAddRemote(trimmed);
    if (result.ok) {
      // App closes the modal via state; clear local fields defensively.
      setCode("");
    } else {
      setError(result.error);
      setBusy(false);
    }
  }, [code, busy, onAddRemote]);

  return (
    <div className="modal-backdrop" data-testid="connection-picker">
      <div
        className="modal"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-label="Connections"
      >
        <header className="modal-header">
          <h2>Connections</h2>
          <button
            type="button"
            className="modal-close"
            onClick={onClose}
            aria-label="Close dialog"
            title="Close"
          >
            <Icon name="close" />
          </button>
        </header>
        <div className="modal-body">
          <p className="settings-section-hint">
            Choose which daemon this window controls. The local daemon runs on
            this machine; a remote host runs on another machine over an
            encrypted (TLS) connection.
          </p>

          <ul className="connection-list" data-testid="connection-list">
            <li>
              <button
                type="button"
                className={
                  target.kind === "local"
                    ? "connection-option is-active"
                    : "connection-option"
                }
                onClick={onSelectLocal}
                data-testid="connection-option-local"
                aria-pressed={target.kind === "local"}
              >
                <span className="connection-option-name">
                  This machine (Local)
                </span>
                <span className="connection-option-detail">
                  bundled daemon
                </span>
              </button>
            </li>
            {profiles.map((p) => (
              <li key={p.id} className="connection-row">
                <button
                  type="button"
                  className={
                    activeProfileId === p.id
                      ? "connection-option is-active"
                      : "connection-option"
                  }
                  onClick={() => onSelectProfile(p.id)}
                  data-testid={`connection-option-${p.id}`}
                  aria-pressed={activeProfileId === p.id}
                >
                  <span className="connection-option-name">{p.name}</span>
                  <span className="connection-option-detail">
                    {p.host}:{p.port}
                  </span>
                </button>
                <button
                  type="button"
                  className="connection-delete"
                  onClick={() => onDeleteProfile(p.id)}
                  aria-label={`Forget ${p.name}`}
                  title="Forget this host"
                >
                  <Icon name="close" />
                </button>
              </li>
            ))}
          </ul>

          <div className="connection-add">
            {addOpen ? (
              <>
                <label htmlFor="connection-code-input">
                  Paste a connection code
                </label>
                <textarea
                  id="connection-code-input"
                  ref={textareaRef}
                  className="connection-code-input"
                  value={code}
                  onChange={(e) => {
                    setCode(e.target.value);
                    setError(null);
                  }}
                  rows={3}
                  spellCheck={false}
                  autoComplete="off"
                  placeholder="Paste the code shown in the host's Remote access settings…"
                  data-testid="connection-code-input"
                />
                {error && (
                  <p
                    className="connection-error"
                    data-testid="connection-code-error"
                  >
                    {error}
                  </p>
                )}
                <div className="connection-add-actions">
                  <button
                    type="button"
                    onClick={() => void onSubmitCode()}
                    disabled={!code.trim() || busy}
                    data-testid="connection-code-submit"
                  >
                    {busy ? "Connecting…" : "Connect"}
                  </button>
                  {profiles.length > 0 && (
                    <button
                      type="button"
                      className="link"
                      onClick={() => {
                        setAddOpen(false);
                        setError(null);
                      }}
                    >
                      Cancel
                    </button>
                  )}
                </div>
              </>
            ) : (
              <button
                type="button"
                className="connection-add-toggle"
                onClick={() => setAddOpen(true)}
                data-testid="connection-add-remote"
              >
                + Add remote (paste code)
              </button>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
