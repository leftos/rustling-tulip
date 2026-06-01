import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { DaemonClient } from "../api";
import type { RearrangeLayout, SessionSnapshot, TabEntry } from "../types";
import { useClampedMenuPosition, useEscape } from "../utils/a11y";
import {
  bestFitGridCols,
  gridArrangements,
  paneAreaAspect,
  pickBalancedDropTarget,
  tabBoundPaneCount,
  tabHasBoundSessions,
  tabPaneCount,
} from "../utils/grid";
import { tabGrid } from "../types";
import { MenuSubmenu } from "./MenuSubmenu";
import MovePanesDialog from "./MovePanesDialog";
import {
  clearTabFontSize,
  getTabFontSize,
  MAX_FONT_SIZE,
  MIN_FONT_SIZE,
  resolveFontSize,
  setTabFontSize,
} from "../utils/fontSize";
import { resolveAppearanceLayers } from "../utils/appearance";
import { useSettings } from "../utils/settings";
import Icon from "./Icon";

interface Props {
  tabs: TabEntry[];
  activeTabId: string | null;
  client: DaemonClient;
  /// All known sessions, used to label panes by their session title in the
  /// "move panes to a new tab" dialog.
  sessions: SessionSnapshot[];
  onActivate: (tabId: string) => void;
  /// Arm the next-new-tab activation flag at the App level. Called right
  /// before a merge/extract send so the freshly broadcast `tab_updated`
  /// auto-switches the user into the new tab.
  onArmNextNewTab: () => void;
  /// Apply a tab-reorder optimistically at the App level — no waiting for
  /// the daemon's `tabs_reordered` broadcast. Without this, dropping a
  /// pill briefly snaps back to its old position on slow links until the
  /// round-trip completes. The daemon broadcast still reconciles on
  /// arrival; for a same-order broadcast the reducer is a no-op.
  onLocalReorder: (orderedIds: string[]) => void;
  onTabWillClose: (tab: TabEntry, index: number, restoreActive: boolean) => void;
  onTabsSnapshotUndo: (
    tabs: TabEntry[],
    message: string,
    restoreFocusedPaneId: string | null,
  ) => void;
}

interface ContextMenuState {
  x: number;
  y: number;
  tabId: string;
}

interface DragState {
  draggingId: string;
  overId: string | null;
  side: "before" | "after";
}

export default function TabBar({
  tabs,
  activeTabId,
  client,
  sessions,
  onActivate,
  onArmNextNewTab,
  onLocalReorder,
  onTabWillClose,
  onTabsSnapshotUndo,
}: Props) {
  const [selectedTabIds, setSelectedTabIds] = useState<Set<string>>(new Set());
  const [movePanesTabId, setMovePanesTabId] = useState<string | null>(null);
  const [settings] = useSettings();
  const appFontSize = resolveAppearanceLayers(
    null,
    null,
    settings.appearance,
  ).values.terminal_font_size;
  const [renamingTabId, setRenamingTabId] = useState<string | null>(null);
  const [contextMenu, setContextMenu] = useState<ContextMenuState | null>(null);
  const [dragState, setDragState] = useState<DragState | null>(null);
  /// Tab id currently in two-state "confirm close" mode. Only one tab can
  /// be armed at a time; clicking another tab's close button resets the
  /// previous arm.
  const [confirmingCloseId, setConfirmingCloseId] = useState<string | null>(
    null,
  );
  /// True when "Close other tabs" is armed for the menu's current target.
  const [confirmingCloseOthers, setConfirmingCloseOthers] = useState(false);
  const lastClickedRef = useRef<string | null>(null);

  // While a tab's close button is armed for confirm, listen for any
  // pointer-down or Escape outside the armed button and reset the arm.
  // Without this, the checkmark stays stuck after the user moves on,
  // forcing them to either click it (closing the tab) or close another
  // tab to clear the state.
  useEffect(() => {
    if (confirmingCloseId === null) return;
    const onPointer = (e: PointerEvent) => {
      const target = e.target as Element | null;
      // If the pointer-down is on the armed button (or any descendant),
      // let the button's own click handler perform the confirm — the
      // armed-state reset happens there.
      if (
        target?.closest?.('[data-testid="tab-pill-close"][data-confirming="true"]')
      ) {
        return;
      }
      setConfirmingCloseId(null);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setConfirmingCloseId(null);
    };
    document.addEventListener("pointerdown", onPointer);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onPointer);
      document.removeEventListener("keydown", onKey);
    };
  }, [confirmingCloseId]);

  // Drop selection entries when those tabs go away (e.g. close after merge).
  useEffect(() => {
    setSelectedTabIds((prev) => {
      const live = new Set(tabs.map((t) => t.id));
      let changed = false;
      const next = new Set<string>();
      for (const id of prev) {
        if (live.has(id)) next.add(id);
        else changed = true;
      }
      return changed ? next : prev;
    });
  }, [tabs]);

  const onNewTab = useCallback(() => {
    client.send({ type: "create_tab", name: null, initial_session_id: null });
  }, [client]);

  const onCloseTab = useCallback(
    (tabId: string, e?: React.MouseEvent) => {
      e?.stopPropagation();
      const tab = tabs.find((t) => t.id === tabId);
      // Confirm when closing would destroy something non-trivial:
      //   - bound sessions (user might lose the visible session pane and
      //     not realise the session itself is still alive in Detached),
      //   - or 2+ panes of grid layout (the layout is persisted per-tab
      //     and re-creating an identical split tree by hand is tedious).
      const requiresConfirm =
        tab !== undefined &&
        (tabHasBoundSessions(tab) || tabPaneCount(tab) >= 2);
      if (requiresConfirm && confirmingCloseId !== tabId) {
        // First click on a non-trivial tab — arm the confirm. Resets any
        // other tab's arm to enforce one-at-a-time.
        setConfirmingCloseId(tabId);
        return;
      }
      setConfirmingCloseId(null);
      if (tab) {
        const index = Math.max(0, tabs.findIndex((t) => t.id === tabId));
        onTabWillClose(tab, index, activeTabId === tabId);
      }
      client.send({ type: "close_tab", tab_id: tabId });
    },
    [client, tabs, confirmingCloseId, activeTabId, onTabWillClose],
  );

  const onPillClick = useCallback(
    (tabId: string, e: React.MouseEvent) => {
      if (e.shiftKey && lastClickedRef.current) {
        // Range select between anchor and clicked.
        const anchorIdx = tabs.findIndex((t) => t.id === lastClickedRef.current);
        const targetIdx = tabs.findIndex((t) => t.id === tabId);
        if (anchorIdx !== -1 && targetIdx !== -1) {
          const [lo, hi] =
            anchorIdx < targetIdx ? [anchorIdx, targetIdx] : [targetIdx, anchorIdx];
          const range = tabs.slice(lo, hi + 1).map((t) => t.id);
          setSelectedTabIds(new Set(range));
        }
        return;
      }
      if (e.ctrlKey || e.metaKey) {
        setSelectedTabIds((prev) => {
          const next = new Set(prev);
          if (next.has(tabId)) next.delete(tabId);
          else next.add(tabId);
          return next;
        });
        lastClickedRef.current = tabId;
        return;
      }
      setSelectedTabIds(new Set());
      lastClickedRef.current = tabId;
      onActivate(tabId);
    },
    [tabs, onActivate],
  );

  const onContextMenu = useCallback((tabId: string, e: React.MouseEvent) => {
    e.preventDefault();
    setContextMenu({ x: e.clientX, y: e.clientY, tabId });
  }, []);

  const closeMenu = useCallback(() => setContextMenu(null), []);

  const onMergeSelected = useCallback(
    (layout: "tile_horizontal" | "tile_vertical") => {
      if (selectedTabIds.size < 2) return;
      const ordered = tabs
        .filter((t) => selectedTabIds.has(t.id))
        .map((t) => t.id);
      onArmNextNewTab();
      client.send({
        type: "merge_tabs",
        tab_ids: ordered,
        name: null,
        layout,
      });
      setSelectedTabIds(new Set());
    },
    [tabs, selectedTabIds, client, onArmNextNewTab],
  );

  const onCloseOthers = useCallback(
    (keepId: string) => {
      for (const t of tabs) {
        if (t.id !== keepId) {
          client.send({ type: "close_tab", tab_id: t.id });
        }
      }
      setSelectedTabIds(new Set());
    },
    [tabs, client],
  );

  const onRearrangeTab = useCallback(
    (tabId: string, layout: RearrangeLayout) => {
      client.send({ type: "rearrange_tab", tab_id: tabId, layout });
    },
    [client],
  );

  const onCommitRename = useCallback(
    (tabId: string, name: string) => {
      const trimmed = name.trim();
      if (trimmed.length > 0) {
        client.send({ type: "rename_tab", tab_id: tabId, name: trimmed });
      }
      setRenamingTabId(null);
    },
    [client],
  );

  const onDragStart = useCallback(
    (tabId: string, e: React.DragEvent) => {
      e.dataTransfer.effectAllowed = "move";
      e.dataTransfer.setData("text/x-rt-tab", tabId);
      setDragState({ draggingId: tabId, overId: null, side: "before" });
    },
    [],
  );

  const onDragOver = useCallback(
    (tabId: string, e: React.DragEvent) => {
      // Tab-reorder drag: track which side of which pill we're over.
      if (dragState && dragState.draggingId !== tabId) {
        e.preventDefault();
        e.dataTransfer.dropEffect = "move";
        const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
        const side: "before" | "after" =
          e.clientX < rect.left + rect.width / 2 ? "before" : "after";
        setDragState((prev) =>
          prev && (prev.overId !== tabId || prev.side !== side)
            ? { ...prev, overId: tabId, side }
            : prev,
        );
        return;
      }
      // Pane-cross-tab drag: accept the dragover so the browser doesn't
      // cancel the operation when the cursor briefly enters a pill on the
      // way to a pane in another tab.
      if (e.dataTransfer.types.includes("text/x-rt-pane")) {
        e.preventDefault();
        e.dataTransfer.dropEffect = "move";
      }
    },
    [dragState],
  );

  // Pane drag entering a tab pill activates that tab so the user can drop
  // onto a pane in it.
  const onPaneDragEnter = useCallback(
    (tabId: string, e: React.DragEvent) => {
      if (!e.dataTransfer.types.includes("text/x-rt-pane")) return;
      if (tabId === activeTabId) return;
      onActivate(tabId);
    },
    [activeTabId, onActivate],
  );

  const onDragEnd = useCallback(() => {
    setDragState(null);
  }, []);

  const onDrop = useCallback(
    (tabId: string, e: React.DragEvent) => {
      e.preventDefault();
      // Pane drop on a tab pill: route through `move_pane` with a
      // smart-balanced destination so the source lands next to the
      // largest pane along its longer axis — matching the auto-add
      // behavior for new sessions. An empty placeholder pane absorbs
      // the drop in place; otherwise the layout grows toward a grid
      // instead of always slicing the leftmost pane.
      const panePayload = e.dataTransfer.getData("text/x-rt-pane");
      if (panePayload) {
        setDragState(null);
        const sep = panePayload.indexOf(":");
        if (sep === -1) return;
        const srcTab = panePayload.slice(0, sep);
        const srcPane = panePayload.slice(sep + 1);
        const targetTab = tabs.find((t) => t.id === tabId);
        if (!targetTab) return;
        const targetGrid = tabGrid(targetTab);
        // Diff tabs (and any future non-grid kinds) have no leaf panes.
        if (!targetGrid) return;
        const dst = pickBalancedDropTarget(targetGrid);
        if (!dst) return;
        // Pane-into-self-target no-op (the same pane is already the
        // destination — moving it to its own edge would just collapse
        // and re-create the parent split).
        if (srcTab === tabId && srcPane === dst.paneId) return;
        const sourceTab = tabs.find((t) => t.id === srcTab) ?? null;
        if (sourceTab) {
          onTabsSnapshotUndo([sourceTab, targetTab], "Moved pane", srcPane);
        }
        client.send({
          type: "move_pane",
          src_tab_id: srcTab,
          src_pane_id: srcPane,
          dst_tab_id: tabId,
          dst_pane_id: dst.paneId,
          edge: dst.edge,
        });
        return;
      }
      const src = e.dataTransfer.getData("text/x-rt-tab");
      const local = dragState;
      setDragState(null);
      if (!src || src === tabId) return;
      const side = local?.overId === tabId ? local.side : "after";
      const order = tabs.map((t) => t.id);
      const fromIdx = order.indexOf(src);
      if (fromIdx === -1) return;
      order.splice(fromIdx, 1);
      const toIdxBase = order.indexOf(tabId);
      if (toIdxBase === -1) return;
      const toIdx = side === "before" ? toIdxBase : toIdxBase + 1;
      order.splice(toIdx, 0, src);
      // Apply optimistically; the daemon broadcast will reconcile on
      // arrival. For a same-order broadcast the reducer is a no-op.
      onLocalReorder(order);
      client.send({ type: "reorder_tabs", ordered_ids: order });
    },
    [tabs, dragState, client, onLocalReorder, onTabsSnapshotUndo],
  );

  if (tabs.length === 0) {
    return (
      <div className="tab-bar tab-bar-empty" data-testid="tab-bar">
        <button
          type="button"
          className="tab-bar-new"
          onClick={onNewTab}
          data-testid="tab-bar-new"
        >
          + New tab
        </button>
      </div>
    );
  }

  return (
    <div className="tab-bar" data-testid="tab-bar">
      <div className="tab-bar-list" role="tablist">
        {tabs.map((t) => {
          const isActive = t.id === activeTabId;
          const isSelected = selectedTabIds.has(t.id);
          const isRenaming = renamingTabId === t.id;
          const dropBefore =
            dragState?.overId === t.id && dragState.side === "before";
          const dropAfter =
            dragState?.overId === t.id && dragState.side === "after";
          const isDragging = dragState?.draggingId === t.id;
          const kind = t.content.kind;
          const classes = [
            "tab-pill",
            `tab-pill-kind-${kind}`,
            isActive ? "is-active" : "",
            isSelected ? "is-multi-selected" : "",
            dropBefore ? "drop-before" : "",
            dropAfter ? "drop-after" : "",
            isDragging ? "is-dragging" : "",
          ]
            .filter(Boolean)
            .join(" ");
          return (
            <div
              key={t.id}
              className={classes}
              role="tab"
              aria-selected={isActive}
              draggable={!isRenaming}
              onClick={(e) => !isRenaming && onPillClick(t.id, e)}
              onContextMenu={(e) => onContextMenu(t.id, e)}
              onDoubleClick={() => setRenamingTabId(t.id)}
              onDragStart={(e) => onDragStart(t.id, e)}
              onDragOver={(e) => onDragOver(t.id, e)}
              onDragEnter={(e) => onPaneDragEnter(t.id, e)}
              onDragEnd={onDragEnd}
              onDrop={(e) => onDrop(t.id, e)}
              data-testid="tab-pill"
              data-tab-id={t.id}
              data-tab-name={t.name}
              data-tab-kind={kind}
              data-tab-active={isActive ? "true" : "false"}
            >
              {isRenaming ? (
                <RenameInput
                  initial={t.name}
                  onCommit={(name) => onCommitRename(t.id, name)}
                  onCancel={() => setRenamingTabId(null)}
                />
              ) : (
                <>
                  {kind === "diff" && (
                    <span
                      className="tab-pill-kind-icon"
                      aria-hidden="true"
                      title="Diff tab"
                    >
                      Δ
                    </span>
                  )}
                  <span className="tab-pill-label" title={t.name}>
                    {t.name}
                  </span>
                  <button
                    type="button"
                    className={
                      confirmingCloseId === t.id
                        ? "tab-pill-close danger"
                        : "tab-pill-close"
                    }
                    title={
                      confirmingCloseId === t.id
                        ? "Click again to confirm closing this tab"
                        : "Close tab"
                    }
                    aria-label={
                      confirmingCloseId === t.id
                        ? `Confirm close tab ${t.name}`
                        : `Close tab ${t.name}`
                    }
                    data-testid="tab-pill-close"
                    data-confirming={
                      confirmingCloseId === t.id ? "true" : "false"
                    }
                    onClick={(e) => onCloseTab(t.id, e)}
                  >
                    <Icon
                      name={confirmingCloseId === t.id ? "confirm" : "close"}
                    />
                  </button>
                </>
              )}
            </div>
          );
        })}
      </div>
      <button
        type="button"
        className="tab-bar-new"
        title="New tab"
        aria-label="New tab"
        onClick={onNewTab}
        data-testid="tab-bar-new"
      >
        <Icon name="plus" />
      </button>
      {contextMenu && (
        <TabContextMenu
          state={contextMenu}
          selectedCount={selectedTabIds.size}
          otherTabsCount={Math.max(0, tabs.length - 1)}
          confirmingCloseOthers={confirmingCloseOthers}
          targetPaneCount={(() => {
            const t = tabs.find((tt) => tt.id === contextMenu.tabId);
            return t ? tabPaneCount(t) : 0;
          })()}
          targetBoundPaneCount={(() => {
            const t = tabs.find((tt) => tt.id === contextMenu.tabId);
            return t ? tabBoundPaneCount(t) : 0;
          })()}
          onClose={() => {
            setConfirmingCloseOthers(false);
            closeMenu();
          }}
          onRename={() => {
            setRenamingTabId(contextMenu.tabId);
            setConfirmingCloseOthers(false);
            closeMenu();
          }}
          onCloseTab={() => {
            onCloseTab(contextMenu.tabId);
            setConfirmingCloseOthers(false);
            closeMenu();
          }}
          onCloseOthers={() => {
            if (!confirmingCloseOthers) {
              setConfirmingCloseOthers(true);
              return;
            }
            onCloseOthers(contextMenu.tabId);
            setConfirmingCloseOthers(false);
            closeMenu();
          }}
          onRearrange={(layout) => {
            onRearrangeTab(contextMenu.tabId, layout);
            setConfirmingCloseOthers(false);
            closeMenu();
          }}
          onFontSizeBump={(delta) => {
            const cur =
              getTabFontSize(contextMenu.tabId) ??
              resolveFontSize(contextMenu.tabId, appFontSize);
            const next = Math.max(
              MIN_FONT_SIZE,
              Math.min(MAX_FONT_SIZE, cur + delta),
            );
            setTabFontSize(contextMenu.tabId, next);
          }}
          onFontSizeReset={() => {
            clearTabFontSize(contextMenu.tabId);
            setConfirmingCloseOthers(false);
            closeMenu();
          }}
          currentTabFontSize={
            getTabFontSize(contextMenu.tabId) ??
            resolveFontSize(contextMenu.tabId, appFontSize)
          }
          hasTabFontOverride={getTabFontSize(contextMenu.tabId) !== null}
          onMergeSelected={(layout) => {
            onMergeSelected(layout);
            setConfirmingCloseOthers(false);
            closeMenu();
          }}
          onPopOut={() => {
            void invoke("open_tab_window", { tabId: contextMenu.tabId });
            setConfirmingCloseOthers(false);
            closeMenu();
          }}
          onMovePanes={() => {
            setMovePanesTabId(contextMenu.tabId);
            setConfirmingCloseOthers(false);
            closeMenu();
          }}
        />
      )}
      {movePanesTabId &&
        (() => {
          const tab = tabs.find((t) => t.id === movePanesTabId);
          if (!tab) return null;
          return (
            <MovePanesDialog
              tab={tab}
              sessions={sessions}
              client={client}
              onArmNextNewTab={onArmNextNewTab}
              onClose={() => setMovePanesTabId(null)}
            />
          );
        })()}
    </div>
  );
}

interface RenameInputProps {
  initial: string;
  onCommit: (name: string) => void;
  onCancel: () => void;
}

function RenameInput({ initial, onCommit, onCancel }: RenameInputProps) {
  const [value, setValue] = useState(initial);
  const inputRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    inputRef.current?.focus();
    inputRef.current?.select();
  }, []);

  return (
    <input
      ref={inputRef}
      className="tab-pill-rename"
      value={value}
      onChange={(e) => setValue(e.target.value)}
      onKeyDown={(e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          onCommit(value);
        } else if (e.key === "Escape") {
          e.preventDefault();
          onCancel();
        }
      }}
      onBlur={() => onCommit(value)}
      onClick={(e) => e.stopPropagation()}
    />
  );
}

interface ContextMenuProps {
  state: ContextMenuState;
  selectedCount: number;
  otherTabsCount: number;
  confirmingCloseOthers: boolean;
  /// Pane count of the right-clicked tab. The Rearrange submenu only
  /// makes sense when there are 2+ panes — a single pane has nothing
  /// to rearrange, so the entire section is omitted.
  targetPaneCount: number;
  /// Session-bound pane count of the right-clicked tab. Rearrange drops
  /// empty placeholder panes, so this — not `targetPaneCount` — is the
  /// count the concrete grid arrangements are enumerated and labeled from.
  targetBoundPaneCount: number;
  /// Effective font size for the right-clicked tab (override or
  /// app-default). Rendered as a numeric chip in the menu so the user
  /// can see what their +/- clicks are nudging.
  currentTabFontSize: number;
  /// True when the right-clicked tab carries a per-tab override (vs
  /// inheriting from app-default). Drives whether the "Reset to
  /// default" item appears.
  hasTabFontOverride: boolean;
  onClose: () => void;
  onRename: () => void;
  onCloseTab: () => void;
  onCloseOthers: () => void;
  onRearrange: (layout: RearrangeLayout) => void;
  onFontSizeBump: (delta: number) => void;
  onFontSizeReset: () => void;
  onMergeSelected: (layout: "tile_horizontal" | "tile_vertical") => void;
  onPopOut: () => void;
  /// Open the "move panes to a new tab" dialog for the right-clicked tab.
  /// Only offered when the tab has 3+ session-bound panes.
  onMovePanes: () => void;
}

function TabContextMenu(p: ContextMenuProps) {
  useEscape(p.onClose);
  const { ref, position } = useClampedMenuPosition<HTMLUListElement>({
    x: p.state.x,
    y: p.state.y,
  });
  const gridShapes = gridArrangements(p.targetBoundPaneCount);
  // Aspect-aware auto: pick columns from the live pane-area shape so a wide
  // window favors more columns. Sent as an explicit `cols` (not 0) because
  // the daemon's auto has no viewport.
  const autoCols = bestFitGridCols(p.targetBoundPaneCount, paneAreaAspect());
  const autoRows = Math.ceil(p.targetBoundPaneCount / autoCols);
  return (
    <div
      className="context-menu-backdrop"
      onClick={p.onClose}
      onContextMenu={(e) => {
        e.preventDefault();
        p.onClose();
      }}
    >
      <ul
        ref={ref}
        className="context-menu"
        style={{ left: position.left, top: position.top }}
        onClick={(e) => e.stopPropagation()}
      >
        <li>
          <button type="button" onClick={p.onRename}>
            Rename tab
          </button>
        </li>
        <li>
          <button type="button" onClick={p.onPopOut}>
            Pop out tab
          </button>
        </li>
        {p.targetPaneCount >= 2 && (
          <>
            <li className="context-menu-separator" aria-hidden="true" />
            <li className="context-menu-label">Rearrange panes</li>
            {gridShapes.length > 0 ? (
              <MenuSubmenu
                label={"  Grid"}
                dataTestId="tab-context-rearrange-grid-menu"
              >
                <li>
                  <button
                    type="button"
                    onClick={() =>
                      p.onRearrange({ kind: "grid", cols: autoCols })
                    }
                    data-testid="tab-context-rearrange-grid"
                  >
                    Auto ({autoCols} cols × {autoRows} rows)
                  </button>
                </li>
                <li className="context-menu-separator" aria-hidden="true" />
                {gridShapes.map((shape) => (
                  <li key={`grid:${shape.cols}`}>
                    <button
                      type="button"
                      onClick={() =>
                        p.onRearrange({ kind: "grid", cols: shape.cols })
                      }
                      data-testid="tab-context-rearrange-grid-cols"
                      data-cols={shape.cols}
                    >
                      {shape.cols} cols × {shape.rows} rows
                    </button>
                  </li>
                ))}
              </MenuSubmenu>
            ) : (
              <li>
                <button
                  type="button"
                  onClick={() =>
                    p.onRearrange({ kind: "grid", cols: autoCols })
                  }
                  data-testid="tab-context-rearrange-grid"
                >
                  &nbsp;&nbsp;Grid (auto)
                </button>
              </li>
            )}
            <li>
              <button
                type="button"
                onClick={() => p.onRearrange({ kind: "horizontal" })}
                data-testid="tab-context-rearrange-horizontal"
              >
                &nbsp;&nbsp;Side by side
              </button>
            </li>
            <li>
              <button
                type="button"
                onClick={() => p.onRearrange({ kind: "vertical" })}
                data-testid="tab-context-rearrange-vertical"
              >
                &nbsp;&nbsp;Stacked
              </button>
            </li>
            {p.targetBoundPaneCount >= 3 && (
              <li>
                <button
                  type="button"
                  onClick={p.onMovePanes}
                  data-testid="tab-context-move-panes"
                >
                  &nbsp;&nbsp;Move panes to new tab…
                </button>
              </li>
            )}
          </>
        )}
        <li className="context-menu-separator" aria-hidden="true" />
        <li className="context-menu-label">
          Tab font size{" "}
          <span
            className={
              p.hasTabFontOverride
                ? "context-menu-chip context-menu-chip-emph"
                : "context-menu-chip"
            }
            title={
              p.hasTabFontOverride
                ? "Tab override active"
                : "Using app default"
            }
          >
            {p.currentTabFontSize}px
          </span>
        </li>
        <li>
          <button
            type="button"
            onClick={() => p.onFontSizeBump(1)}
            data-testid="tab-context-font-bump"
            disabled={p.currentTabFontSize >= MAX_FONT_SIZE}
          >
            &nbsp;&nbsp;Increase
          </button>
        </li>
        <li>
          <button
            type="button"
            onClick={() => p.onFontSizeBump(-1)}
            data-testid="tab-context-font-shrink"
            disabled={p.currentTabFontSize <= MIN_FONT_SIZE}
          >
            &nbsp;&nbsp;Decrease
          </button>
        </li>
        {p.hasTabFontOverride && (
          <li>
            <button
              type="button"
              onClick={p.onFontSizeReset}
              data-testid="tab-context-font-reset"
            >
              &nbsp;&nbsp;Reset to app default
            </button>
          </li>
        )}
        <li className="context-menu-separator" aria-hidden="true" />
        <li>
          <button type="button" onClick={p.onCloseTab}>
            Close tab
          </button>
        </li>
        {p.otherTabsCount > 0 && (
          <li>
            <button
              type="button"
              className={p.confirmingCloseOthers ? "danger" : undefined}
              onClick={p.onCloseOthers}
              data-testid="tab-context-close-others"
              data-confirming={p.confirmingCloseOthers ? "true" : "false"}
            >
              {p.confirmingCloseOthers
                ? `Confirm closing ${p.otherTabsCount} other tab${p.otherTabsCount === 1 ? "" : "s"}`
                : "Close other tabs"}
            </button>
          </li>
        )}
        {p.selectedCount >= 2 && (
          <>
            <li className="context-menu-separator" aria-hidden="true" />
            <li className="context-menu-label">
              Merge {p.selectedCount} selected into new tab
            </li>
            <li>
              <button
                type="button"
                onClick={() => p.onMergeSelected("tile_horizontal")}
                data-testid="tab-context-merge-horizontal"
              >
                &nbsp;&nbsp;Side by side (horizontal)
              </button>
            </li>
            <li>
              <button
                type="button"
                onClick={() => p.onMergeSelected("tile_vertical")}
                data-testid="tab-context-merge-vertical"
              >
                &nbsp;&nbsp;Stacked (vertical)
              </button>
            </li>
          </>
        )}
      </ul>
    </div>
  );
}
