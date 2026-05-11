import { useCallback, useEffect, useRef, useState } from "react";
import type { DaemonClient } from "../api";
import type { TabEntry } from "../types";

interface Props {
  tabs: TabEntry[];
  activeTabId: string | null;
  client: DaemonClient;
  onActivate: (tabId: string) => void;
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

export default function TabBar({ tabs, activeTabId, client, onActivate }: Props) {
  const [selectedTabIds, setSelectedTabIds] = useState<Set<string>>(new Set());
  const [renamingTabId, setRenamingTabId] = useState<string | null>(null);
  const [contextMenu, setContextMenu] = useState<ContextMenuState | null>(null);
  const [dragState, setDragState] = useState<DragState | null>(null);
  const lastClickedRef = useRef<string | null>(null);

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
      client.send({ type: "close_tab", tab_id: tabId });
    },
    [client],
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

  const onMergeSelected = useCallback(() => {
    if (selectedTabIds.size < 2) return;
    const ordered = tabs
      .filter((t) => selectedTabIds.has(t.id))
      .map((t) => t.id);
    client.send({
      type: "merge_tabs",
      tab_ids: ordered,
      name: null,
      layout: "tile_horizontal",
    });
    setSelectedTabIds(new Set());
  }, [tabs, selectedTabIds, client]);

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
      if (!dragState || dragState.draggingId === tabId) return;
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
    },
    [dragState],
  );

  const onDragEnd = useCallback(() => {
    setDragState(null);
  }, []);

  const onDrop = useCallback(
    (tabId: string, e: React.DragEvent) => {
      e.preventDefault();
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
      client.send({ type: "reorder_tabs", ordered_ids: order });
    },
    [tabs, dragState, client],
  );

  if (tabs.length === 0) {
    return (
      <div className="tab-bar tab-bar-empty">
        <button type="button" className="tab-bar-new" onClick={onNewTab}>
          + New tab
        </button>
      </div>
    );
  }

  return (
    <div className="tab-bar">
      <div className="tab-bar-list" role="tablist">
        {tabs.map((t) => {
          const isActive = t.id === activeTabId;
          const isSelected = selectedTabIds.has(t.id);
          const isRenaming = renamingTabId === t.id;
          const dropBefore =
            dragState?.overId === t.id && dragState.side === "before";
          const dropAfter =
            dragState?.overId === t.id && dragState.side === "after";
          const classes = [
            "tab-pill",
            isActive ? "is-active" : "",
            isSelected ? "is-multi-selected" : "",
            dropBefore ? "drop-before" : "",
            dropAfter ? "drop-after" : "",
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
              onDragEnd={onDragEnd}
              onDrop={(e) => onDrop(t.id, e)}
            >
              {isRenaming ? (
                <RenameInput
                  initial={t.name}
                  onCommit={(name) => onCommitRename(t.id, name)}
                  onCancel={() => setRenamingTabId(null)}
                />
              ) : (
                <>
                  <span className="tab-pill-label" title={t.name}>
                    {t.name}
                  </span>
                  <button
                    type="button"
                    className="tab-pill-close"
                    title="Close tab"
                    onClick={(e) => onCloseTab(t.id, e)}
                  >
                    ×
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
        onClick={onNewTab}
      >
        +
      </button>
      {contextMenu && (
        <TabContextMenu
          state={contextMenu}
          selectedCount={selectedTabIds.size}
          onClose={closeMenu}
          onRename={() => {
            setRenamingTabId(contextMenu.tabId);
            closeMenu();
          }}
          onCloseTab={() => {
            onCloseTab(contextMenu.tabId);
            closeMenu();
          }}
          onCloseOthers={() => {
            onCloseOthers(contextMenu.tabId);
            closeMenu();
          }}
          onMergeSelected={() => {
            onMergeSelected();
            closeMenu();
          }}
        />
      )}
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
  onClose: () => void;
  onRename: () => void;
  onCloseTab: () => void;
  onCloseOthers: () => void;
  onMergeSelected: () => void;
}

function TabContextMenu(p: ContextMenuProps) {
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
        className="context-menu"
        style={{ left: p.state.x, top: p.state.y }}
        onClick={(e) => e.stopPropagation()}
      >
        <li>
          <button type="button" onClick={p.onRename}>
            Rename tab
          </button>
        </li>
        <li>
          <button type="button" onClick={p.onCloseTab}>
            Close tab
          </button>
        </li>
        <li>
          <button type="button" onClick={p.onCloseOthers}>
            Close other tabs
          </button>
        </li>
        {p.selectedCount >= 2 && (
          <>
            <li className="context-menu-separator" aria-hidden="true" />
            <li>
              <button type="button" onClick={p.onMergeSelected}>
                Merge {p.selectedCount} selected into new tab
              </button>
            </li>
          </>
        )}
      </ul>
    </div>
  );
}
