import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { DaemonClient } from "../api";
import { tabGrid, type SessionSnapshot, type TabEntry } from "../types";
import { clampMenuCoord } from "../utils/a11y";
import { collectPanes, sessionTabBindings } from "../utils/grid";

export interface SessionContextMenuState {
  x: number;
  y: number;
  session: SessionSnapshot;
}

interface Props {
  state: SessionContextMenuState;
  tabs: TabEntry[];
  client: DaemonClient;
  onClose: () => void;
  /** `withDialog` = true when the click was modified with Shift. */
  onDuplicate: (withDialog: boolean) => void;
}

type CloseMode = "idle" | "confirming";

export default function SessionContextMenu({
  state,
  tabs,
  client,
  onClose,
  onDuplicate,
}: Props) {
  const s = state.session;

  // Headless sessions are one-shot — their identity IS the kickoff prompt,
  // which we deliberately drop on duplicate. Cloning one without the prompt
  // would just re-run the same `claude --print` with no input and exit.
  const canDuplicate = s.mode !== "headless";

  // Source bindings drive the "Move to" submenu. With zero bindings the
  // session has no live pane to move — fall back to the unbound-pill flow.
  const bindings = sessionTabBindings(s.id, tabs);
  const sourceBinding = bindings[0] ?? null;
  const bindingTabIds = new Set(bindings.map((b) => b.tab_id));
  const moveTargets = sourceBinding
    ? tabs.filter((t) => !bindingTabIds.has(t.id))
    : [];

  // Reveal worktree picks the first member's worktree path. For workspace
  // sessions the user can still drill into individual members via the
  // sidebar; the menu offers the primary one as a fast-path.
  const worktreePath = s.members[0]?.worktree_path ?? null;

  const isStopped = s.status === "stopped" || s.status === "error";
  const [closeMode, setCloseMode] = useState<CloseMode>("idle");

  const sendStop = (removeWorktree: boolean) => {
    client.send({
      type: "stop_session",
      session_id: s.id,
      cleanup: s.members.map((m) => ({
        repo_id: m.repo_id,
        remove_worktree: removeWorktree,
      })),
    });
    onClose();
  };

  const onMoveTo = (dstTab: TabEntry) => {
    if (!sourceBinding) return;
    const dstGrid = tabGrid(dstTab);
    const dstPane = dstGrid ? collectPanes(dstGrid)[0] : null;
    if (!dstPane) return;
    client.send({
      type: "move_pane",
      src_tab_id: sourceBinding.tab_id,
      src_pane_id: sourceBinding.pane_id,
      dst_tab_id: dstTab.id,
      dst_pane_id: dstPane.pane_id,
      edge: "right",
    });
    onClose();
  };

  return (
    <div
      className="context-menu-backdrop"
      onClick={onClose}
      onContextMenu={(e) => {
        e.preventDefault();
        onClose();
      }}
    >
      <ul
        className="context-menu"
        style={{
          left: clampMenuCoord(state.x, 260),
          top: clampMenuCoord(state.y, 320, "height"),
        }}
        onClick={(e) => e.stopPropagation()}
      >
        {closeMode === "confirming" ? (
          <CloseConfirm
            hasWorktree={s.has_per_session_worktree}
            onCancel={() => setCloseMode("idle")}
            onCloseKeep={() => sendStop(false)}
            onCloseRemove={() => sendStop(true)}
          />
        ) : (
          <>
            <li>
              <button
                type="button"
                disabled={!canDuplicate}
                onClick={(e) => {
                  if (!canDuplicate) return;
                  onDuplicate(e.shiftKey);
                }}
                title={
                  canDuplicate
                    ? "Click to spawn a clone immediately. Shift-click to open the spawn dialog pre-filled."
                    : "Headless sessions can't be duplicated — they're one-shot kickoffs."
                }
                data-testid="session-context-duplicate"
              >
                Duplicate
                {canDuplicate && (
                  <span className="context-menu-hint">⇧ to edit</span>
                )}
              </button>
            </li>

            {moveTargets.length > 0 && (
              <>
                <li className="context-menu-separator" aria-hidden="true" />
                <li className="context-menu-label">Move to</li>
                {moveTargets.map((t) => (
                  <li key={t.id}>
                    <button
                      type="button"
                      onClick={() => onMoveTo(t)}
                      title={`Move this session into "${t.name}"`}
                    >
                      {t.name}
                    </button>
                  </li>
                ))}
              </>
            )}

            <li className="context-menu-separator" aria-hidden="true" />
            <li>
              <button
                type="button"
                onClick={() => {
                  void invoke("open_session_window", { sessionId: s.id });
                  onClose();
                }}
              >
                Pop out window
              </button>
            </li>
            {worktreePath && (
              <li>
                <button
                  type="button"
                  onClick={() => {
                    void invoke("reveal_in_explorer", { path: worktreePath });
                    onClose();
                  }}
                  title={worktreePath}
                >
                  Reveal worktree in Explorer
                </button>
              </li>
            )}

            <li className="context-menu-separator" aria-hidden="true" />
            <li>
              <button
                type="button"
                className="danger"
                disabled={isStopped}
                onClick={() => setCloseMode("confirming")}
                title={
                  isStopped
                    ? "Session already stopped"
                    : "Stop this session (asks before removing worktrees)"
                }
                data-testid="session-context-close"
              >
                Close…
              </button>
            </li>
          </>
        )}
      </ul>
    </div>
  );
}

interface CloseConfirmProps {
  hasWorktree: boolean;
  onCancel: () => void;
  onCloseKeep: () => void;
  onCloseRemove: () => void;
}

function CloseConfirm({
  hasWorktree,
  onCancel,
  onCloseKeep,
  onCloseRemove,
}: CloseConfirmProps) {
  return (
    <>
      <li className="context-menu-label">Stop session?</li>
      {hasWorktree && (
        <li>
          <button
            type="button"
            className="danger"
            onClick={onCloseRemove}
            data-testid="session-context-close-remove-worktree"
          >
            Close and remove worktree
          </button>
        </li>
      )}
      <li>
        <button
          type="button"
          className="danger"
          onClick={onCloseKeep}
          data-testid={
            hasWorktree
              ? "session-context-close-keep-worktree"
              : "session-context-close-confirm"
          }
        >
          {hasWorktree ? "Close and keep worktree" : "Confirm close"}
        </button>
      </li>
      <li>
        <button type="button" onClick={onCancel}>
          Cancel
        </button>
      </li>
    </>
  );
}
