import { useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { DaemonClient } from "../api";
import { tabGrid, type SessionSnapshot, type TabEntry } from "../types";
import { clampMenuCoord } from "../utils/a11y";
import { collectPanes, sessionTabBindings } from "../utils/grid";
import {
  clearPanePoppedOut,
  markPanePoppedOut,
} from "../utils/poppedPanes";
import {
  clearSessionFontSize,
  getSessionFontSize,
  MAX_FONT_SIZE,
  MIN_FONT_SIZE,
  resolveFontSize,
  setSessionFontSize,
} from "../utils/fontSize";
import {
  DEFAULT_SESSION_COLOR,
  normalizeSessionColor,
  SESSION_COLOR_PRESETS,
  sessionAccentStyle,
} from "../utils/sessionColor";

export interface SessionContextMenuState {
  x: number;
  y: number;
  session: SessionSnapshot;
}

/// Destination tab for a duplicate action.
/// - `new_tab`  — fresh tab containing the duplicate (the default).
/// - `{ tabId }` — drop the duplicate into an existing tab (replacing an
///   empty pane if one exists, else splitting on the right).
export type DuplicateTarget = "new_tab" | { tabId: string };

interface Props {
  state: SessionContextMenuState;
  tabs: TabEntry[];
  client: DaemonClient;
  preferredPaneId?: string;
  onClose: () => void;
  /** `withDialog` = true when the click was modified with Shift. When
   *  `withDialog` is false, `target` specifies where the duplicate
   *  lands; defaults to a fresh new tab. */
  onDuplicate: (withDialog: boolean, target: DuplicateTarget) => void;
  onTabsSnapshotUndo?: (
    tabs: TabEntry[],
    message: string,
    restoreFocusedPaneId: string | null,
  ) => void;
}

type CloseMode = "idle" | "confirming";

/// Inline rename mode for the session context menu. `null` while the
/// menu shows its usual items; a string captures the in-progress edit
/// value when the user clicks "Rename". Empty + Enter clears the user
/// override and restores the daemon-generated default label.
type RenameMode = null | { value: string };

export default function SessionContextMenu({
  state,
  tabs,
  client,
  preferredPaneId,
  onClose,
  onDuplicate,
  onTabsSnapshotUndo,
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

  /// Action-bucket detection. Mutually exclusive:
  /// - `inactive`: parked by the user; no pane, sits in the sidebar
  ///   waiting to be resumed.
  /// - `stopped`: process exited self-or-by-user, still bound to a
  ///   pane (waiting for the user to restart / discard / park).
  /// - `running`: anything else (spawning / idle / working / awaiting_input).
  /// Orphans + abandoned have their own inline affordances in the sidebar
  /// and fall under `running` here for the close-confirm path.
  const isInactive = s.is_inactive;
  const isStopped =
    !isInactive && (s.status === "stopped" || s.status === "error");
  const [closeMode, setCloseMode] = useState<CloseMode>("idle");
  const [renameMode, setRenameMode] = useState<RenameMode>(null);
  const currentColor = normalizeSessionColor(s.accent_color);

  const sendRename = (raw: string) => {
    const trimmed = raw.trim();
    client.send({
      type: "rename_session",
      session_id: s.id,
      label: trimmed.length > 0 ? trimmed : null,
    });
    setRenameMode(null);
    onClose();
  };
  /// Re-read on every menu render — the source-of-truth lives in
  /// localStorage and the menu is short-lived. Memoizing would just
  /// require an extra subscription for no benefit.
  const sessionTabId = sourceBinding?.tab_id ?? null;
  const sessionFontSize =
    getSessionFontSize(s.id) ?? resolveFontSize(s.id, sessionTabId);
  const hasSessionFontOverride = getSessionFontSize(s.id) !== null;
  const bumpFont = (delta: number) => {
    const next = Math.max(
      MIN_FONT_SIZE,
      Math.min(MAX_FONT_SIZE, sessionFontSize + delta),
    );
    setSessionFontSize(s.id, next);
  };
  const sendColor = (color: string | null) => {
    client.send({
      type: "set_session_color",
      session_id: s.id,
      color,
    });
    onClose();
  };

  const sendStop = (removeWorktree: boolean) => {
    const cleanup = s.members.map((m) => ({
      repo_id: m.repo_id,
      remove_worktree: removeWorktree,
    }));
    client.send({
      type: "stop_session",
      session_id: s.id,
      cleanup: cleanup.map((action) => ({
        ...action,
        remove_worktree: false,
      })),
    });
    if (removeWorktree) {
      client.send({
        type: "discard_session",
        session_id: s.id,
        cleanup,
      });
    }
    onClose();
  };

  const sendDiscard = (removeWorktree: boolean) => {
    client.send({
      type: "discard_session",
      session_id: s.id,
      cleanup: s.members.map((m) => ({
        repo_id: m.repo_id,
        remove_worktree: removeWorktree,
      })),
    });
    onClose();
  };

  const sendPark = () => {
    client.send({ type: "park_session", session_id: s.id });
    onClose();
  };

  /// Restart routes through App.tsx so the new clone lands in the same
  /// tab the stopped session is pinned to. Inactive sessions have no
  /// pane to anchor against — they get a fresh tab.
  const sendRestart = () => {
    window.dispatchEvent(
      new CustomEvent("rt:pane_session_restart", {
        detail: { sessionId: s.id, tabId: sessionTabId },
      }),
    );
    onClose();
  };

  const onMoveTo = (dstTab: TabEntry) => {
    if (!sourceBinding) return;
    const dstGrid = tabGrid(dstTab);
    const dstPanes = dstGrid ? collectPanes(dstGrid) : [];
    // Prefer absorbing an empty placeholder pane over splitting next to
    // it — otherwise "Move to Tab 2" on a fresh Tab 2 leaves the empty
    // pane visible alongside the moved session, which feels broken.
    const emptyPane = dstPanes.find((p) => p.session_id === null);
    const dstPane = emptyPane ?? dstPanes[0];
    if (!dstPane) return;
    const sourceTab = tabs.find((t) => t.id === sourceBinding.tab_id) ?? null;
    if (onTabsSnapshotUndo && sourceTab) {
      onTabsSnapshotUndo([sourceTab, dstTab], "Moved pane", sourceBinding.pane_id);
    }
    client.send({
      type: "move_pane",
      src_tab_id: sourceBinding.tab_id,
      src_pane_id: sourceBinding.pane_id,
      dst_tab_id: dstTab.id,
      dst_pane_id: dstPane.pane_id,
      edge: emptyPane ? "replace" : "right",
    });
    onClose();
  };

  const onPopOut = () => {
    const paneId = preferredPaneId ?? sourceBinding?.pane_id ?? null;
    if (!paneId) {
      void invoke("open_session_window", { sessionId: s.id });
      onClose();
      return;
    }
    markPanePoppedOut(paneId);
    void invoke("open_pane_window", { paneId })
      .catch(() => {
        clearPanePoppedOut(paneId);
      })
      .finally(onClose);
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
        ) : renameMode !== null ? (
          <RenameForm
            value={renameMode.value}
            onChange={(next) => setRenameMode({ value: next })}
            onSubmit={() => sendRename(renameMode.value)}
            onCancel={() => setRenameMode(null)}
          />
        ) : (
          <>
            <li>
              <button
                type="button"
                onClick={() => setRenameMode({ value: s.label })}
                title="Custom name for this session. Submit blank to restore the auto-generated default."
                data-testid="session-context-rename"
              >
                Rename…
              </button>
            </li>
            <MenuSubmenu
              label="Duplicate"
              disabled={!canDuplicate}
              title={
                canDuplicate
                  ? "Clone this session into a new or existing tab."
                  : "Headless sessions can't be duplicated; they're one-shot kickoffs."
              }
              dataTestId="session-context-duplicate-menu"
            >
              <li>
                <button
                  type="button"
                  onClick={(e) => onDuplicate(e.shiftKey, "new_tab")}
                  title="Clone into a fresh tab. Shift-click to open the spawn dialog pre-filled."
                  data-testid="session-context-duplicate"
                >
                  New tab
                  <span className="context-menu-hint">Shift to edit</span>
                </button>
              </li>
              {tabs.length > 0 && (
                <li className="context-menu-separator" aria-hidden="true" />
              )}
              {tabs.map((t) => (
                <li key={`dup:${t.id}`}>
                  <button
                    type="button"
                    onClick={() => onDuplicate(false, { tabId: t.id })}
                    title={`Spawn a clone of this session into "${t.name}"`}
                    data-testid="session-context-duplicate-to-tab"
                    data-tab-id={t.id}
                  >
                    {t.name}
                  </button>
                </li>
              ))}
            </MenuSubmenu>

            {moveTargets.length > 0 && (
              <MenuSubmenu label="Move to" dataTestId="session-context-move-menu">
                {moveTargets.map((t) => (
                  <li key={`mv:${t.id}`}>
                    <button
                      type="button"
                      onClick={() => onMoveTo(t)}
                      title={`Move this session into "${t.name}"`}
                      data-testid="session-context-move-to-tab"
                      data-tab-id={t.id}
                    >
                      {t.name}
                    </button>
                  </li>
                ))}
              </MenuSubmenu>
            )}

            <li className="context-menu-separator" aria-hidden="true" />
            <li>
              <button type="button" onClick={onPopOut}>
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
            <li className="context-menu-label">
              Font size{" "}
              <span
                className={
                  hasSessionFontOverride
                    ? "context-menu-chip context-menu-chip-emph"
                    : "context-menu-chip"
                }
                title={
                  hasSessionFontOverride
                    ? "Session override active"
                    : "Inheriting tab/app default"
                }
              >
                {sessionFontSize}px
              </span>
            </li>
            <li>
              <button
                type="button"
                onClick={() => bumpFont(1)}
                disabled={sessionFontSize >= MAX_FONT_SIZE}
                data-testid="session-context-font-bump"
              >
                &nbsp;&nbsp;Increase
              </button>
            </li>
            <li>
              <button
                type="button"
                onClick={() => bumpFont(-1)}
                disabled={sessionFontSize <= MIN_FONT_SIZE}
                data-testid="session-context-font-shrink"
              >
                &nbsp;&nbsp;Decrease
              </button>
            </li>
            {hasSessionFontOverride && (
              <li>
                <button
                  type="button"
                  onClick={() => {
                    clearSessionFontSize(s.id);
                    onClose();
                  }}
                  data-testid="session-context-font-reset"
                >
                  &nbsp;&nbsp;Reset to tab/app default
                </button>
              </li>
            )}

            <li className="context-menu-separator" aria-hidden="true" />
            <MenuSubmenu
              label="Color"
              chip={currentColor ?? "default"}
              chipEmphasis={currentColor !== null}
              dataTestId="session-context-color-menu"
            >
              <li>
                <div
                  className="session-color-list"
                  role="group"
                  aria-label="Preset colors"
                >
                  {SESSION_COLOR_PRESETS.map((preset, index) => (
                    <button
                      key={preset.color}
                      type="button"
                      className="session-color-preset"
                      style={sessionAccentStyle(preset.color)}
                      title={`Use ${preset.name} (${preset.color})`}
                      aria-label={`Use ${preset.name} session color`}
                      data-testid={`session-context-color-${index}`}
                      data-session-color={preset.color}
                      onClick={() => sendColor(preset.color)}
                    >
                      <span
                        className="session-color-preview-dot"
                        aria-hidden="true"
                      />
                      <span className="session-color-preview-label">
                        {preset.name}
                      </span>
                      <span className="context-menu-hint">{preset.color}</span>
                    </button>
                  ))}
                </div>
              </li>
              <li>
                <label className="session-color-custom">
                  <span>Custom</span>
                  <input
                    type="color"
                    value={currentColor ?? DEFAULT_SESSION_COLOR}
                    aria-label="Custom session color"
                    data-testid="session-context-color-custom"
                    onChange={(e) => sendColor(e.currentTarget.value)}
                  />
                </label>
              </li>
              {currentColor && (
                <li>
                  <button
                    type="button"
                    onClick={() => sendColor(null)}
                    data-testid="session-context-color-reset"
                  >
                    Reset color
                  </button>
                </li>
              )}
            </MenuSubmenu>

            <li className="context-menu-separator" aria-hidden="true" />
            {isInactive ? (
              <>
                <li>
                  <button
                    type="button"
                    onClick={sendRestart}
                    data-testid="session-context-resume"
                    title="Spawn a fresh session that reuses this worktree"
                  >
                    Resume
                  </button>
                </li>
                <li>
                  <button
                    type="button"
                    onClick={() => sendDiscard(false)}
                    data-testid="session-context-remove-keep-worktree"
                    title="Remove this entry from the sidebar; keep the worktree on disk"
                  >
                    Remove from sidebar
                  </button>
                </li>
                {s.has_per_session_worktree && (
                  <li>
                    <button
                      type="button"
                      className="danger"
                      onClick={() => sendDiscard(true)}
                      data-testid="session-context-remove-with-worktree"
                      title="Remove this entry and delete the worktree from disk"
                    >
                      Remove from sidebar and delete worktree
                    </button>
                  </li>
                )}
              </>
            ) : isStopped ? (
              <>
                <li>
                  <button
                    type="button"
                    onClick={sendRestart}
                    data-testid="session-context-restart"
                    title="Spawn a fresh session with the same configuration"
                  >
                    Restart
                  </button>
                </li>
                {s.has_per_session_worktree && (
                  <li>
                    <button
                      type="button"
                      onClick={sendPark}
                      data-testid="session-context-park"
                      title="Move to inactive in the sidebar; keep the worktree on disk"
                    >
                      Remove pane, keep worktree
                    </button>
                  </li>
                )}
                <li>
                  <button
                    type="button"
                    className="danger"
                    onClick={() => sendDiscard(s.has_per_session_worktree)}
                    data-testid="session-context-remove-pane"
                    title={
                      s.has_per_session_worktree
                        ? "Remove the pane and delete the worktree from disk"
                        : "Remove the pane"
                    }
                  >
                    {s.has_per_session_worktree
                      ? "Remove pane and delete worktree"
                      : "Remove pane"}
                  </button>
                </li>
              </>
            ) : (
              <li>
                <button
                  type="button"
                  className="danger"
                  onClick={() => setCloseMode("confirming")}
                  title="Stop this session (asks before deleting worktrees)"
                  data-testid="session-context-close"
                >
                  Stop session…
                </button>
              </li>
            )}
          </>
        )}
      </ul>
    </div>
  );
}

interface MenuSubmenuProps {
  label: string;
  children: ReactNode;
  disabled?: boolean;
  title?: string;
  dataTestId?: string;
  chip?: string;
  chipEmphasis?: boolean;
}

function MenuSubmenu({
  label,
  children,
  disabled = false,
  title,
  dataTestId,
  chip,
  chipEmphasis = false,
}: MenuSubmenuProps) {
  return (
    <li className="context-menu-submenu">
      <button
        type="button"
        className="context-menu-submenu-trigger"
        disabled={disabled}
        title={title}
        aria-haspopup="menu"
        data-testid={dataTestId}
        onClick={(e) => {
          e.preventDefault();
          e.currentTarget.focus();
        }}
      >
        <span className="context-menu-submenu-label">{label}</span>
        {chip && (
          <span
            className={
              chipEmphasis
                ? "context-menu-chip context-menu-chip-emph"
                : "context-menu-chip"
            }
          >
            {chip}
          </span>
        )}
        <span className="context-menu-submenu-arrow" aria-hidden="true">
          &gt;
        </span>
      </button>
      {!disabled && (
        <ul className="context-menu-submenu-panel" role="menu">
          {children}
        </ul>
      )}
    </li>
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
            Stop and delete worktree
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
          {hasWorktree ? "Stop session, keep worktree" : "Stop session"}
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

interface RenameFormProps {
  value: string;
  onChange: (next: string) => void;
  onSubmit: () => void;
  onCancel: () => void;
}

function RenameForm({ value, onChange, onSubmit, onCancel }: RenameFormProps) {
  return (
    <>
      <li className="context-menu-label">
        Rename session{" "}
        <span className="muted small inline-note">(blank = default)</span>
      </li>
      <li>
        {/* Inputs inside a <li> stay clickable; the parent <ul>'s
            click-stops-propagation guard keeps the backdrop from
            closing the menu when the user clicks the field. */}
        <input
          type="text"
          autoFocus
          value={value}
          placeholder="Use default"
          onChange={(e) => onChange(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              onSubmit();
            } else if (e.key === "Escape") {
              e.preventDefault();
              onCancel();
            }
          }}
          data-testid="session-context-rename-input"
        />
      </li>
      <li>
        <button
          type="button"
          className="primary"
          onClick={onSubmit}
          data-testid="session-context-rename-submit"
        >
          Save
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
