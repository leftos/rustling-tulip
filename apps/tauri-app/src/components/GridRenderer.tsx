import { useCallback, useEffect, useRef, useState } from "react";
import type { DaemonClient } from "../api";
import { clampMenuCoord } from "../utils/a11y";
import {
  tabGrid,
  type GridNode,
  type PaneDropEdge,
  type SessionSnapshot,
  type SplitDirection,
  type SplitPlace,
  type TabEntry,
} from "../types";
import { collectPanes } from "../utils/grid";
import SessionPane from "./SessionPane";
import EmptyPane from "./EmptyPane";
import PaneCloseDialog from "./PaneCloseDialog";

const DRAG_MIME = "text/x-rt-pane";

interface Props {
  tab: TabEntry;
  /// Full tab list — threaded down to SessionPane so the session
  /// context menu's "Move to → <tab>" submenu can enumerate
  /// destinations. The active tab is implicitly the source.
  tabs: TabEntry[];
  client: DaemonClient;
  sessions: SessionSnapshot[];
  subscribePty: (sessionId: string, cb: (b64: string) => void) => () => void;
  focusedPaneId: string | null;
  onFocusPane: (paneId: string) => void;
  onSpawnInPane: (paneId: string) => void;
  hasRepos: boolean;
  /// Arm the next-new-tab activation flag at the App level. Called right
  /// before an extract-to-new-tab send so the freshly broadcast
  /// `tab_updated` auto-switches the user into the new tab.
  onArmNextNewTab: () => void;
  /// Arm the next-tab-update-for-this-tab to focus a freshly created pane.
  /// Called right before a split send so the new pane (which the daemon
  /// allocates the id for) gets focused on the resulting `tab_updated`.
  /// Receives the tab id + the pane ids already present at split time —
  /// the new pane id is whichever isn't in the set.
  onArmFocusNewPane: (tabId: string, knownPaneIds: Set<string>) => void;
  /// True when this GridRenderer is mounted inside a pop-out window
  /// (TabWindow). Threads through to EmptyPane so the "Spawn one here"
  /// button is replaced by a hint, since the pop-out has no SpawnDialog.
  inPopout?: boolean;
}

export default function GridRenderer({
  tab,
  tabs,
  client,
  sessions,
  subscribePty,
  focusedPaneId,
  onFocusPane,
  onSpawnInPane,
  hasRepos,
  onArmNextNewTab,
  onArmFocusNewPane,
  inPopout,
}: Props) {
  const grid = tabGrid(tab);
  if (!grid) {
    return (
      <div className="grid-root grid-root-non-grid" data-testid="grid-root-non-grid">
        <p className="empty">
          This tab has no pane layout.
        </p>
      </div>
    );
  }
  // Snapshot of pane ids at render time. Captured once per render and
  // passed by reference to PaneChrome's split handler — the handler reads
  // it at click time, so renames between render and click are fine.
  const knownPaneIds = new Set(collectPanes(grid).map((p) => p.pane_id));
  return (
    <div className="grid-root">
      <NodeRenderer
        node={grid}
        path={[]}
        tabId={tab.id}
        tabs={tabs}
        client={client}
        sessions={sessions}
        subscribePty={subscribePty}
        focusedPaneId={focusedPaneId}
        onFocusPane={onFocusPane}
        onSpawnInPane={onSpawnInPane}
        hasRepos={hasRepos}
        onArmNextNewTab={onArmNextNewTab}
        onArmFocusNewPane={onArmFocusNewPane}
        knownPaneIds={knownPaneIds}
        inPopout={inPopout ?? false}
      />
    </div>
  );
}

interface NodeProps {
  node: GridNode;
  path: number[];
  tabId: string;
  tabs: TabEntry[];
  client: DaemonClient;
  sessions: SessionSnapshot[];
  subscribePty: (sessionId: string, cb: (b64: string) => void) => () => void;
  focusedPaneId: string | null;
  onFocusPane: (paneId: string) => void;
  onSpawnInPane: (paneId: string) => void;
  hasRepos: boolean;
  onArmNextNewTab: () => void;
  onArmFocusNewPane: (tabId: string, knownPaneIds: Set<string>) => void;
  knownPaneIds: Set<string>;
  inPopout: boolean;
}

function NodeRenderer(props: NodeProps) {
  if (props.node.kind === "pane") {
    return <PaneChrome {...props} node={props.node} />;
  }
  return <SplitRenderer {...props} node={props.node} />;
}

interface SplitProps extends Omit<NodeProps, "node"> {
  node: Extract<GridNode, { kind: "split" }>;
}

function SplitRenderer(props: SplitProps) {
  const { node, path } = props;
  const [localRatio, setLocalRatio] = useState(node.ratio);
  useEffect(() => {
    setLocalRatio(node.ratio);
  }, [node.ratio]);

  const containerRef = useRef<HTMLDivElement | null>(null);
  const dragRef = useRef<{ active: boolean; lastRatio: number } | null>(null);

  const onPointerDown = useCallback(
    (e: React.PointerEvent<HTMLDivElement>) => {
      e.preventDefault();
      e.stopPropagation();
      (e.currentTarget as Element).setPointerCapture(e.pointerId);
      dragRef.current = { active: true, lastRatio: node.ratio };
    },
    [node.ratio],
  );

  const onPointerMove = useCallback(
    (e: React.PointerEvent<HTMLDivElement>) => {
      const state = dragRef.current;
      if (!state || !state.active) return;
      const rect = containerRef.current?.getBoundingClientRect();
      if (!rect) return;
      const next =
        node.direction === "horizontal"
          ? (e.clientX - rect.left) / Math.max(1, rect.width)
          : (e.clientY - rect.top) / Math.max(1, rect.height);
      const clamped = Math.min(0.95, Math.max(0.05, next));
      state.lastRatio = clamped;
      setLocalRatio(clamped);
    },
    [node.direction],
  );

  const onPointerUp = useCallback(
    (e: React.PointerEvent<HTMLDivElement>) => {
      const state = dragRef.current;
      if (!state || !state.active) return;
      (e.currentTarget as Element).releasePointerCapture(e.pointerId);
      dragRef.current = null;
      props.client.send({
        type: "set_pane_ratio",
        tab_id: props.tabId,
        split_path: path,
        ratio: state.lastRatio,
      });
    },
    [props.client, props.tabId, path],
  );

  const isHorizontal = node.direction === "horizontal";
  const style: React.CSSProperties = isHorizontal
    ? {
        gridTemplateColumns: `minmax(0, ${localRatio}fr) 4px minmax(0, ${1 - localRatio}fr)`,
      }
    : {
        gridTemplateRows: `minmax(0, ${localRatio}fr) 4px minmax(0, ${1 - localRatio}fr)`,
      };

  const dividerClass = isHorizontal
    ? "grid-divider grid-divider-h"
    : "grid-divider grid-divider-v";

  return (
    <div
      ref={containerRef}
      className={isHorizontal ? "grid-split grid-split-h" : "grid-split grid-split-v"}
      style={style}
    >
      <NodeRenderer {...props} node={node.first} path={[...path, 0]} />
      <div
        className={dividerClass}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerCancel={onPointerUp}
        role="separator"
        aria-orientation={isHorizontal ? "vertical" : "horizontal"}
      />
      <NodeRenderer {...props} node={node.second} path={[...path, 1]} />
    </div>
  );
}

interface PaneChromeProps extends Omit<NodeProps, "node"> {
  node: Extract<GridNode, { kind: "pane" }>;
}

interface PaneContextMenuState {
  x: number;
  y: number;
}

function PaneChrome(props: PaneChromeProps) {
  const { node, tabId, client, sessions, subscribePty, focusedPaneId } = props;
  const session = node.session_id
    ? (sessions.find((s) => s.id === node.session_id) ?? null)
    : null;
  const isFocused = focusedPaneId === node.pane_id;
  const [dropEdge, setDropEdge] = useState<PaneDropEdge | null>(null);
  // Mirror of dropEdge for use inside onDrop — React state updates from
  // the last onDragOver may not have flushed by the time drop fires, and
  // we want the drop edge to exactly match the previewed overlay rather
  // than re-deriving from cursor coords (which can jitter across the
  // dx/dy boundary between the dragover and drop events).
  const dropEdgeRef = useRef<PaneDropEdge | null>(null);
  const [contextMenu, setContextMenu] = useState<PaneContextMenuState | null>(
    null,
  );
  const paneRef = useRef<HTMLDivElement | null>(null);

  // Safety net: any drag ending anywhere in the window (successful drop,
  // ESC cancel, or release outside any droppable) must clear this pane's
  // overlay. Without this, the source pane's overlay — set when the
  // cursor briefly hovered itself before moving to the real destination —
  // stays stuck because onDragLeave's child-target heuristic missed it.
  useEffect(() => {
    const clear = () => {
      dropEdgeRef.current = null;
      setDropEdge(null);
    };
    window.addEventListener("dragend", clear);
    window.addEventListener("drop", clear);
    return () => {
      window.removeEventListener("dragend", clear);
      window.removeEventListener("drop", clear);
    };
  }, []);

  const onClick = useCallback(() => {
    props.onFocusPane(node.pane_id);
  }, [props, node.pane_id]);

  const sendSplit = useCallback(
    (direction: SplitDirection, place: SplitPlace) => {
      // Arm the focus-new-pane signal before sending. The daemon's
      // tab_updated will arrive with one more pane than the snapshot;
      // App diffs and focuses the new one.
      props.onArmFocusNewPane(tabId, props.knownPaneIds);
      client.send({
        type: "split_pane",
        tab_id: tabId,
        pane_id: node.pane_id,
        direction,
        place,
        new_session_id: null,
      });
    },
    [client, tabId, node.pane_id, props],
  );

  const closePaneOnly = useCallback(() => {
    client.send({ type: "close_pane", tab_id: tabId, pane_id: node.pane_id });
  }, [client, tabId, node.pane_id]);

  const stopSessionAndClosePane = useCallback(
    (removeWorktree: boolean) => {
      if (!session) return;
      client.send({
        type: "stop_session",
        session_id: session.id,
        cleanup: session.members.map((m) => ({
          repo_id: m.repo_id,
          remove_worktree: removeWorktree,
        })),
      });
      client.send({
        type: "close_pane",
        tab_id: tabId,
        pane_id: node.pane_id,
      });
    },
    [client, session, tabId, node.pane_id],
  );

  // Pane × button: empty pane closes immediately (nothing to protect);
  // bound pane opens the 4-option confirm dialog. Previously × sent
  // close_pane outright — the pane disappeared from the layout and the
  // session orphaned into the sidebar with no prompt about its worktree.
  const [closeConfirm, setCloseConfirm] = useState(false);
  const onClose = useCallback(() => {
    if (!session) {
      closePaneOnly();
      return;
    }
    setCloseConfirm(true);
  }, [session, closePaneOnly]);

  const onExtract = useCallback(() => {
    props.onArmNextNewTab();
    client.send({
      type: "extract_to_new_tab",
      source_tab_id: tabId,
      pane_ids: [node.pane_id],
      name: null,
    });
  }, [client, tabId, node.pane_id, props]);

  const onDragStart = useCallback(
    (e: React.DragEvent) => {
      e.dataTransfer.effectAllowed = "move";
      e.dataTransfer.setData(DRAG_MIME, `${tabId}:${node.pane_id}`);
    },
    [tabId, node.pane_id],
  );

  const onDragOver = useCallback(
    (e: React.DragEvent) => {
      // Only accept our own pane-drag payload; ignore other DnD activity.
      if (!e.dataTransfer.types.includes(DRAG_MIME)) return;
      e.preventDefault();
      e.dataTransfer.dropEffect = "move";
      const rect = paneRef.current?.getBoundingClientRect();
      if (!rect) return;
      const x = (e.clientX - rect.left) / Math.max(1, rect.width);
      const y = (e.clientY - rect.top) / Math.max(1, rect.height);
      const next = computeEdge(x, y);
      dropEdgeRef.current = next;
      setDropEdge((prev) => (prev === next ? prev : next));
    },
    [],
  );

  const onDragLeave = useCallback((e: React.DragEvent) => {
    // Use cursor bounds rather than currentTarget==target — the previous
    // heuristic kept the overlay stuck when the user dragged out of a
    // pane while the cursor was over a child element (the terminal,
    // chips, header text), because dragleave's target is the child.
    const rect = paneRef.current?.getBoundingClientRect();
    if (!rect) return;
    const { clientX, clientY } = e;
    if (
      clientX < rect.left ||
      clientX >= rect.right ||
      clientY < rect.top ||
      clientY >= rect.bottom
    ) {
      dropEdgeRef.current = null;
      setDropEdge(null);
    }
  }, []);

  const onDrop = useCallback(
    (e: React.DragEvent) => {
      const payload = e.dataTransfer.getData(DRAG_MIME);
      // Use the edge captured by the last dragover so the actual drop
      // matches the overlay the user saw. Recomputing from cursor coords
      // here can flip across the dx/dy boundary between events and
      // produce a vertical split when the preview promised horizontal.
      const edge = dropEdgeRef.current;
      dropEdgeRef.current = null;
      setDropEdge(null);
      if (!payload || !edge) return;
      e.preventDefault();
      const sep = payload.indexOf(":");
      if (sep === -1) return;
      const srcTab = payload.slice(0, sep);
      const srcPane = payload.slice(sep + 1);
      if (srcTab === tabId && srcPane === node.pane_id) return; // self-drop
      client.send({
        type: "move_pane",
        src_tab_id: srcTab,
        src_pane_id: srcPane,
        dst_tab_id: tabId,
        dst_pane_id: node.pane_id,
        edge,
      });
    },
    [client, tabId, node.pane_id],
  );

  const onContextMenu = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    setContextMenu({ x: e.clientX, y: e.clientY });
  }, []);

  const classes = ["grid-pane", isFocused ? "is-focused" : ""]
    .filter(Boolean)
    .join(" ");

  return (
    <div
      ref={paneRef}
      className={classes}
      onPointerDownCapture={onClick}
      onDragOver={onDragOver}
      onDragLeave={onDragLeave}
      onDrop={onDrop}
    >
      <div className="grid-pane-controls" onContextMenu={onContextMenu}>
        <span
          className="grid-pane-drag"
          draggable
          onDragStart={onDragStart}
          title="Drag pane to move"
          aria-hidden="true"
        >
          ⠿
        </span>
        <button
          type="button"
          className="grid-pane-btn"
          title="Split right (Shift+click: split left)"
          aria-label="Split pane horizontally; hold Shift to place the new pane on the left"
          onClick={(e) =>
            sendSplit("horizontal", e.shiftKey ? "first" : "second")
          }
        >
          ▶|
        </button>
        <button
          type="button"
          className="grid-pane-btn"
          title="Split down (Shift+click: split up)"
          aria-label="Split pane vertically; hold Shift to place the new pane on top"
          onClick={(e) =>
            sendSplit("vertical", e.shiftKey ? "first" : "second")
          }
        >
          ▼=
        </button>
        <button
          type="button"
          className="grid-pane-btn grid-pane-btn-close"
          title="Close pane"
          aria-label="Close pane"
          onClick={onClose}
        >
          ×
        </button>
      </div>
      <div className="grid-pane-body">
        {session ? (
          <SessionPane
            session={session}
            client={client}
            tabs={props.tabs}
            subscribePty={subscribePty}
            onHeaderDragStart={onDragStart}
            tabId={tabId}
          />
        ) : (
          <EmptyPane
            hasRepos={props.hasRepos}
            inPopout={props.inPopout}
            onSpawn={() => props.onSpawnInPane(node.pane_id)}
          />
        )}
      </div>
      {dropEdge && <DropOverlay edge={dropEdge} />}
      {closeConfirm && session && (
        <PaneCloseDialog
          session={session}
          onCancel={() => setCloseConfirm(false)}
          onClosePaneKeepSession={() => {
            closePaneOnly();
            setCloseConfirm(false);
          }}
          onCloseAndStopKeepWorktree={() => {
            stopSessionAndClosePane(false);
            setCloseConfirm(false);
          }}
          onCloseAndStopRemoveWorktree={() => {
            stopSessionAndClosePane(true);
            setCloseConfirm(false);
          }}
        />
      )}
      {contextMenu && (
        <PaneContextMenu
          state={contextMenu}
          onClose={() => setContextMenu(null)}
          onExtract={() => {
            onExtract();
            setContextMenu(null);
          }}
          onCloseItem={() => {
            onClose();
            setContextMenu(null);
          }}
        />
      )}
    </div>
  );
}

function computeEdge(x: number, y: number): PaneDropEdge {
  if (x > 0.4 && x < 0.6 && y > 0.4 && y < 0.6) return "replace";
  const dx = Math.abs(x - 0.5);
  const dy = Math.abs(y - 0.5);
  if (dx > dy) return x < 0.5 ? "left" : "right";
  return y < 0.5 ? "top" : "bottom";
}

function DropOverlay({ edge }: { edge: PaneDropEdge }) {
  const cls = `grid-drop-overlay grid-drop-${edge}`;
  return <div className={cls} aria-hidden="true" />;
}

interface PaneContextMenuProps {
  state: PaneContextMenuState;
  onClose: () => void;
  onExtract: () => void;
  onCloseItem: () => void;
}

function PaneContextMenu(p: PaneContextMenuProps) {
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
        style={{
          left: clampMenuCoord(p.state.x, 200),
          top: clampMenuCoord(p.state.y, 200, "height"),
        }}
        onClick={(e) => e.stopPropagation()}
      >
        <li>
          <button type="button" onClick={p.onExtract}>
            Move to new tab
          </button>
        </li>
        <li>
          <button type="button" onClick={p.onCloseItem}>
            Close pane
          </button>
        </li>
      </ul>
    </div>
  );
}
