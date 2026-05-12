import { useEffect, useRef, useState } from "react";
import { open as openInShell } from "@tauri-apps/plugin-shell";
import {
  fetchDaemonPaths,
  type ConnectionState,
  type DaemonHandshake,
  type DaemonPaths,
} from "../api";
import { copyToClipboard } from "../utils/clipboard";

/// Union the footer accepts. Mirrors the shape `Sidebar.tsx::renderConnectionBadge`
/// took before this component subsumed it — App's `status` adds `init` and
/// `error` variants on top of the WS-client's `ConnectionState`.
type FooterConnection =
  | ConnectionState
  | { kind: "init" }
  | { kind: "error"; reason: string };

interface Props {
  connection: FooterConnection;
  /// Handshake from `ensureDaemonStarted`. Held in AppState so the footer
  /// can render port/pid/protocol-version chips inside the flyout. `null`
  /// before the first successful supervisor call (e.g. boot, after Stop).
  handshake: DaemonHandshake | null;
  sessionCount: number;
  /// Set when the user explicitly hit "Stop daemon" from this footer; the
  /// auto-reconnect loop in App.tsx is suppressed while this is true. The
  /// footer text changes to "stopped" to make the intent visible (vs the
  /// transient "disconnected" that just means the WS dropped).
  stopRequested: boolean;
  onRestart: () => void;
  onStop: () => void;
}

export default function DaemonFooter({
  connection,
  handshake,
  sessionCount,
  stopRequested,
  onRestart,
  onStop,
}: Props) {
  const [open, setOpen] = useState(false);
  const [paths, setPaths] = useState<DaemonPaths | null>(null);
  const [confirmingStop, setConfirmingStop] = useState(false);
  const [copied, setCopied] = useState<keyof DaemonPaths | null>(null);
  const buttonRef = useRef<HTMLButtonElement | null>(null);

  // Lazy-load paths on first flyout open. The values rarely change inside
  // a session (only when RUSTLING_TULIP_CONFIG_DIR is overridden mid-run,
  // which is an e2e-only path) so one fetch per app boot is enough.
  useEffect(() => {
    if (!open || paths !== null) return;
    void fetchDaemonPaths()
      .then(setPaths)
      .catch(() => {
        /* best-effort — flyout still shows the action buttons */
      });
  }, [open, paths]);

  // Close on Esc + outside-click. Both are scoped to `open` so the listener
  // pair is only live while the flyout is mounted.
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        setOpen(false);
        setConfirmingStop(false);
        buttonRef.current?.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open]);

  const stateKind = connection.kind;
  const isStopped = stopRequested && stateKind !== "open";
  const dotClass = isStopped
    ? "daemon-footer-dot daemon-footer-dot-stopped"
    : `daemon-footer-dot daemon-footer-dot-${stateKind}`;

  // Footer text: name + status detail. Healthy case shows session count
  // and port; everything else shows the state name. The dot color does
  // most of the work — text is for the user who wants the exact reason.
  let stateLabel: string;
  if (isStopped) {
    stateLabel = "stopped";
  } else if (stateKind === "open") {
    stateLabel = `${sessionCount} ${sessionCount === 1 ? "session" : "sessions"}`;
  } else if (stateKind === "connecting" || stateKind === "init") {
    stateLabel = stateKind === "init" ? "starting…" : "connecting…";
  } else if (stateKind === "closed") {
    stateLabel = "disconnected";
  } else if (stateKind === "auth_failed") {
    stateLabel = "auth failed";
  } else {
    stateLabel = "error";
  }
  const portChip =
    stateKind === "open" && handshake ? `:${handshake.port}` : null;

  const reason =
    "reason" in connection && typeof connection.reason === "string"
      ? connection.reason
      : null;

  const openPath = async (path: string | undefined) => {
    if (!path) return;
    try {
      await openInShell(path);
    } catch (err) {
      // Failure-to-open is non-fatal — surface in app.log so a user
      // chasing a hung flyout can see what blew up.
      console.warn("daemon-footer: open path failed", path, err);
    }
  };

  const copyPath = async (key: keyof DaemonPaths) => {
    const value = paths?.[key];
    if (!value) return;
    try {
      await copyToClipboard(value, "path");
      setCopied(key);
      window.setTimeout(() => {
        setCopied((cur) => (cur === key ? null : cur));
      }, 1200);
    } catch {
      /* best-effort */
    }
  };

  return (
    <div className="daemon-footer-host" data-testid="daemon-footer-host">
      <button
        ref={buttonRef}
        type="button"
        className={open ? "daemon-footer is-open" : "daemon-footer"}
        onClick={() => setOpen((prev) => !prev)}
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-label={`Daemon status: ${stateLabel}. Click to open troubleshooting menu.`}
        data-testid="daemon-footer"
        data-state={isStopped ? "stopped" : stateKind}
        title={reason ? `${stateLabel} — ${reason}` : stateLabel}
      >
        <span className={dotClass} aria-hidden="true" />
        <span className="daemon-footer-name">daemon</span>
        <span className="daemon-footer-sep" aria-hidden="true">·</span>
        <span className="daemon-footer-state">{stateLabel}</span>
        {portChip && (
          <>
            <span className="daemon-footer-sep" aria-hidden="true">·</span>
            <span className="daemon-footer-port">{portChip}</span>
          </>
        )}
      </button>

      {open && (
        <>
          <div
            className="daemon-footer-backdrop"
            onClick={() => {
              setOpen(false);
              setConfirmingStop(false);
            }}
            data-testid="daemon-footer-backdrop"
          />
          <div
            className="daemon-footer-flyout"
            role="dialog"
            aria-label="Daemon troubleshooting"
            data-testid="daemon-footer-flyout"
            onClick={(e) => e.stopPropagation()}
          >
            <header className="daemon-footer-flyout-header">
              <h3>Daemon</h3>
              <button
                type="button"
                className="link"
                onClick={() => {
                  setOpen(false);
                  setConfirmingStop(false);
                }}
                aria-label="Close"
              >
                ✕
              </button>
            </header>

            <ul className="daemon-footer-details">
              <li>
                <span className="daemon-footer-detail-label">state</span>
                <span className="daemon-footer-detail-value">
                  {isStopped ? "stopped" : stateKind}
                </span>
              </li>
              {handshake && (
                <>
                  <li>
                    <span className="daemon-footer-detail-label">port</span>
                    <span className="daemon-footer-detail-value">
                      :{handshake.port}
                    </span>
                  </li>
                  <li>
                    <span className="daemon-footer-detail-label">pid</span>
                    <span className="daemon-footer-detail-value">
                      {handshake.pid}
                    </span>
                  </li>
                  <li>
                    <span className="daemon-footer-detail-label">proto</span>
                    <span className="daemon-footer-detail-value">
                      v{handshake.protocol_version}
                    </span>
                  </li>
                </>
              )}
              {stateKind === "open" && (
                <li>
                  <span className="daemon-footer-detail-label">sessions</span>
                  <span className="daemon-footer-detail-value">
                    {sessionCount}
                  </span>
                </li>
              )}
              {reason && (
                <li>
                  <span className="daemon-footer-detail-label">reason</span>
                  <span className="daemon-footer-detail-value daemon-footer-detail-reason">
                    {reason}
                  </span>
                </li>
              )}
              {paths && (
                <>
                  <li
                    className="daemon-footer-path-row"
                    title={paths.handshake_file}
                  >
                    <span className="daemon-footer-detail-label">handshake</span>
                    <button
                      type="button"
                      className="daemon-footer-path-copy"
                      onClick={() => copyPath("handshake_file")}
                      title="Copy path to clipboard"
                      aria-label="Copy handshake path"
                    >
                      {copied === "handshake_file" ? "copied" : "copy path"}
                    </button>
                  </li>
                </>
              )}
            </ul>

            <div className="daemon-footer-section">
              <div className="daemon-footer-section-label">Logs &amp; files</div>
              <button
                type="button"
                className="daemon-footer-action"
                onClick={() => openPath(paths?.daemon_log)}
                disabled={!paths}
                data-testid="daemon-footer-open-daemon-log"
              >
                <span>Open daemon.log</span>
                <span className="daemon-footer-action-hint">
                  {paths ? "open in default editor" : "loading…"}
                </span>
              </button>
              <button
                type="button"
                className="daemon-footer-action"
                onClick={() => openPath(paths?.app_log)}
                disabled={!paths}
                data-testid="daemon-footer-open-app-log"
              >
                <span>Open app.log</span>
                <span className="daemon-footer-action-hint">
                  {paths ? "open in default editor" : "loading…"}
                </span>
              </button>
              <button
                type="button"
                className="daemon-footer-action"
                onClick={() => openPath(paths?.config_dir)}
                disabled={!paths}
                data-testid="daemon-footer-reveal-config"
              >
                <span>Reveal config dir</span>
                <span className="daemon-footer-action-hint">
                  state.json · sessions · logs
                </span>
              </button>
            </div>

            <div className="daemon-footer-section">
              <div className="daemon-footer-section-label">Control</div>
              <button
                type="button"
                className="daemon-footer-action"
                onClick={() => {
                  onRestart();
                  setOpen(false);
                  setConfirmingStop(false);
                }}
                data-testid="daemon-footer-restart"
              >
                <span>Restart daemon</span>
                <span className="daemon-footer-action-hint">
                  graceful · tracer-backed sessions reattach
                </span>
              </button>
              <button
                type="button"
                className={
                  confirmingStop
                    ? "daemon-footer-action danger is-confirming"
                    : "daemon-footer-action danger"
                }
                onClick={() => {
                  if (!confirmingStop) {
                    setConfirmingStop(true);
                    return;
                  }
                  onStop();
                  setOpen(false);
                  setConfirmingStop(false);
                }}
                data-testid="daemon-footer-stop"
                data-confirming={confirmingStop ? "true" : "false"}
              >
                <span>
                  {confirmingStop ? "Confirm: stop daemon" : "Stop daemon"}
                </span>
                <span className="daemon-footer-action-hint">
                  no auto-respawn · Restart to bring back
                </span>
              </button>
            </div>
          </div>
        </>
      )}
    </div>
  );
}
