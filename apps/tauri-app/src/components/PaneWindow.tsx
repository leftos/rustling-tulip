import { useCallback, useEffect, useRef } from "react";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import type { DaemonClient } from "../api";
import type { SessionSnapshot, TabEntry } from "../types";
import { markForAutoFocus } from "../utils/autofocus";
import { clearPanePoppedOut, markPanePoppedOut } from "../utils/poppedPanes";
import {
  sessionDisplayLabel,
  sessionLabelTooltip,
  sessionRuntimeLabel,
} from "../utils/sessionLabel";
import SessionPane from "./SessionPane";

interface Props {
  session: SessionSnapshot;
  tab: TabEntry;
  paneId: string;
  tabs: TabEntry[];
  client: DaemonClient;
  subscribePty: (sessionId: string, cb: (b64: string) => void) => () => void;
}

/// Standalone window chrome for a single grid-backed pane pop-out. Unlike
/// the legacy session pop-out, this window is anchored to a concrete pane id,
/// and the source tab keeps that pane slot reserved so the user can dock the
/// pane back into the grid later.
export default function PaneWindow({
  session,
  tab,
  paneId,
  tabs,
  client,
  subscribePty,
}: Props) {
  const markedRef = useRef(false);
  if (!markedRef.current) {
    markedRef.current = true;
    markForAutoFocus(session.id);
  }

  useEffect(() => {
    markPanePoppedOut(paneId);
  }, [paneId]);

  const onStop = useCallback(() => {
    client.send({
      type: "stop_session",
      session_id: session.id,
      cleanup: session.members.map((m) => ({
        repo_id: m.repo_id,
        remove_worktree: false,
      })),
    });
  }, [client, session]);

  const onDockBack = useCallback(async () => {
    clearPanePoppedOut(paneId);
    await getCurrentWebviewWindow().close();
  }, [paneId]);

  useEffect(() => {
    let unlisten: (() => void) | null = null;
    void (async () => {
      const win = getCurrentWebviewWindow();
      unlisten = await win.onCloseRequested(() => {
        clearPanePoppedOut(paneId);
      });
    })();
    return () => {
      unlisten?.();
    };
  }, [paneId]);

  const displayLabel = sessionDisplayLabel(session);
  useEffect(() => {
    void getCurrentWebviewWindow().setTitle(displayLabel);
  }, [displayLabel]);

  return (
    <div
      className="session-window-root pane-window-root"
      data-testid="pane-window-root"
      data-pane-id={paneId}
    >
      <header className="session-window-toolbar">
        <span className={`status-dot status-${session.status}`} />
        <h1 title={sessionLabelTooltip(session)}>{displayLabel}</h1>
        <span className="chip session-member-chip" title={`Docked in "${tab.name}"`}>
          {tab.name}
        </span>
        {session.members.map((m) => (
          <span
            key={m.repo_id}
            className="chip session-member-chip"
            title={m.worktree_path}
          >
            {m.repo_name}: {m.branch}
          </span>
        ))}
        {(() => {
          const runtime = sessionRuntimeLabel(session);
          return runtime ? (
            <span
              className="chip session-runtime-chip"
              title={`Running ${runtime}`}
            >
              {runtime}
            </span>
          ) : null;
        })()}
        {session.elevated_authority && (
          <span
            className="chip session-authority-chip"
            title="Trusted launch: permission prompts were bypassed"
            data-testid="session-authority-badge"
          >
            trusted
          </span>
        )}
        {session.mode === "headless" && (
          <span className="session-window-meta">· headless</span>
        )}
        <span className="spacer" />
        {session.status !== "stopped" && (
          <button type="button" onClick={onStop} className="danger">
            Stop session
          </button>
        )}
        <button
          type="button"
          onClick={() => {
            void onDockBack();
          }}
          data-testid="pane-dock-back"
        >
          Dock back
        </button>
      </header>
      <div className="session-window-body">
        <SessionPane
          session={session}
          client={client}
          tabs={tabs}
          subscribePty={subscribePty}
          tabId={tab.id}
          paneId={paneId}
          hideHeader
        />
      </div>
    </div>
  );
}
