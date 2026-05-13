import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Window } from "@tauri-apps/api/window";
import type { DaemonClient } from "../api";
import { clampMenuCoord } from "../utils/a11y";
import {
  clearPanePoppedOut,
  isPanePoppedOut,
  subscribePoppedPaneChanges,
} from "../utils/poppedPanes";
import { sessionDisplayLabel } from "../utils/sessionLabel";
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
import Icon from "./Icon";
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
  onTabsSnapshotUndo: (
    tabs: TabEntry[],
    message: string,
    restoreFocusedPaneId: string | null,
  ) => void;
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
  onTabsSnapshotUndo,
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
        tab={tab}
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
        onTabsSnapshotUndo={onTabsSnapshotUndo}
        knownPaneIds={knownPaneIds}
        inPopout={inPopout ?? false}
      />
    </div>
  );
}

interface NodeProps {
  node: GridNode;
  path: number[];
  tab: TabEntry;
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
  onTabsSnapshotUndo: (
    tabs: TabEntry[],
    message: string,
    restoreFocusedPaneId: string | null,
  ) => void;
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
  const {
    node,
    tab,
    tabId,
    tabs,
    client,
    sessions,
    subscribePty,
    focusedPaneId,
    onTabsSnapshotUndo,
  } = props;
  const session = node.session_id
    ? (sessions.find((s) => s.id === node.session_id) ?? null)
    : null;
  const [poppedOut, setPoppedOut] = useState(() =>
    isPanePoppedOut(node.pane_id),
  );
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

  useEffect(() => {
    setPoppedOut(isPanePoppedOut(node.pane_id));
    return subscribePoppedPaneChanges(() => {
      setPoppedOut(isPanePoppedOut(node.pane_id));
    });
  }, [node.pane_id]);

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
    const message = session
      ? `Closed pane "${sessionDisplayLabel(session)}"`
      : "Closed empty pane";
    onTabsSnapshotUndo([tab], message, node.pane_id);
    client.send({ type: "close_pane", tab_id: tabId, pane_id: node.pane_id });
  }, [client, session, tab, tabId, node.pane_id, onTabsSnapshotUndo]);

  const closePaneAndDiscardSession = useCallback(
    (removeWorktree: boolean) => {
      if (!session) return;
      const cleanup = session.members.map((m) => ({
        repo_id: m.repo_id,
        remove_worktree: removeWorktree,
      }));
      if (session.status !== "stopped" && session.status !== "error") {
        client.send({
          type: "stop_session",
          session_id: session.id,
          cleanup: cleanup.map((action) => ({
            ...action,
            remove_worktree: false,
          })),
        });
      }
      client.send({
        type: "discard_session",
        session_id: session.id,
        cleanup,
      });
    },
    [client, session],
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
      const sourceSnapshot =
        tabs.find((candidate) => candidate.id === srcTab) ??
        (tab.id === srcTab ? tab : null);
      const destinationSnapshot =
        tabs.find((candidate) => candidate.id === tabId) ?? tab;
      onTabsSnapshotUndo(
        [sourceSnapshot, destinationSnapshot].filter(
          (candidate): candidate is TabEntry => candidate !== null,
        ),
        edge === "replace" && srcTab === tabId ? "Swapped panes" : "Moved pane",
        srcPane,
      );
      client.send({
        type: "move_pane",
        src_tab_id: srcTab,
        src_pane_id: srcPane,
        dst_tab_id: tabId,
        dst_pane_id: node.pane_id,
        edge,
      });
    },
    [client, tab, tabs, tabId, node.pane_id, onTabsSnapshotUndo],
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
      data-pane-id={node.pane_id}
      onPointerDownCapture={onClick}
      onDragOver={onDragOver}
      onDragLeave={onDragLeave}
      onDrop={onDrop}
    >
      {/* Floating pane chrome — rendered only for empty panes, which
          have no inner header to host the buttons. Live sessions render
          the same controls inline inside the session header instead, so
          they no longer overlap the Pop-out/Stop buttons. */}
      {!session && (
        <div className="grid-pane-controls" onContextMenu={onContextMenu}>
          <span
            className="grid-pane-drag"
            draggable
            onDragStart={onDragStart}
            title="Drag pane to move; drop on the center of another pane to swap"
            aria-hidden="true"
          >
            <Icon name="drag" />
          </span>
          <button
            type="button"
            className="grid-pane-btn"
            title="Split right (Shift+click: split left)"
            aria-label="Split pane horizontally; hold Shift to place the new pane on the left"
            data-testid="grid-pane-split-right"
            onClick={(e) =>
              sendSplit("horizontal", e.shiftKey ? "first" : "second")
            }
          >
            <Icon name="splitRight" />
          </button>
          <button
            type="button"
            className="grid-pane-btn"
            title="Split down (Shift+click: split up)"
            aria-label="Split pane vertically; hold Shift to place the new pane on top"
            data-testid="grid-pane-split-down"
            onClick={(e) =>
              sendSplit("vertical", e.shiftKey ? "first" : "second")
            }
          >
            <Icon name="splitDown" />
          </button>
          <button
            type="button"
            className="grid-pane-btn grid-pane-btn-close"
            title="Close pane"
            aria-label="Close pane"
            data-testid="grid-pane-close"
            onClick={onClose}
          >
            <Icon name="close" />
          </button>
        </div>
      )}
      <div className="grid-pane-body">
        {session && poppedOut ? (
          <PoppedOutPaneCard paneId={node.pane_id} session={session} />
        ) : session ? (
          <SessionPane
            session={session}
            client={client}
            tabs={tabs}
            subscribePty={subscribePty}
            onTabsSnapshotUndo={onTabsSnapshotUndo}
            onHeaderDragStart={onDragStart}
            tabId={tabId}
            paneId={node.pane_id}
            onSplitRight={(e) =>
              sendSplit("horizontal", e.shiftKey ? "first" : "second")
            }
            onSplitDown={(e) =>
              sendSplit("vertical", e.shiftKey ? "first" : "second")
            }
            onExtractToTab={onExtract}
            onClosePane={onClose}
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
          onClosePaneDiscardSessionKeepWorktree={() => {
            closePaneAndDiscardSession(false);
            setCloseConfirm(false);
          }}
          onClosePaneDiscardSessionRemoveWorktree={() => {
            closePaneAndDiscardSession(true);
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

function PoppedOutPaneCard({
  paneId,
  session,
}: {
  paneId: string;
  session: SessionSnapshot;
}) {
  const onFocusPopout = useCallback(() => {
    void invoke("open_pane_window", { paneId });
  }, [paneId]);

  const onDockBack = useCallback(async () => {
    clearPanePoppedOut(paneId);
    const win = await Window.getByLabel(`pane-${paneId}`);
    await win?.close();
  }, [paneId]);

  return (
    <div
      className="terminal-placeholder popped-out-pane-card"
      data-testid="popped-out-pane"
      data-pane-id={paneId}
      data-session-id={session.id}
    >
      <div className="popped-out-pane-title">
        {sessionDisplayLabel(session)}
      </div>
      <div className="muted">
        This pane is popped out into its own window. Its slot stays reserved
        here so it can dock back into this tab.
      </div>
      <div className="session-exited-actions">
        <button
          type="button"
          onClick={onFocusPopout}
          data-testid="popped-out-pane-focus"
        >
          Focus window
        </button>
        <button
          type="button"
          onClick={() => {
            void onDockBack();
          }}
          data-testid="popped-out-pane-dock"
        >
          Dock back
        </button>
      </div>
    </div>
  );
}

function computeEdge(x: number, y: number): PaneDropEdge {
  if (x > 0.35 && x < 0.65 && y > 0.35 && y < 0.65) return "replace";
  const dx = Math.abs(x - 0.5);
  const dy = Math.abs(y - 0.5);
  if (dx > dy) return x < 0.5 ? "left" : "right";
  return y < 0.5 ? "top" : "bottom";
}

function DropOverlay({ edge }: { edge: PaneDropEdge }) {
  const cls = `grid-drop-overlay grid-drop-${edge}`;
  return (
    <div
      className={cls}
      aria-hidden="true"
      data-drop-edge={edge}
    >
      {edge === "replace" && <span>Swap panes</span>}
    </div>
  );
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
