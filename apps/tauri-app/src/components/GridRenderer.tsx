import { useCallback, useEffect, useRef, useState } from "react";
import type { DaemonClient } from "../api";
import type {
  GridNode,
  SessionSnapshot,
  SplitDirection,
  TabEntry,
} from "../types";
import SessionPane from "./SessionPane";
import EmptyPane from "./EmptyPane";

interface Props {
  tab: TabEntry;
  client: DaemonClient;
  sessions: SessionSnapshot[];
  subscribePty: (sessionId: string, cb: (b64: string) => void) => () => void;
  focusedPaneId: string | null;
  onFocusPane: (paneId: string) => void;
  onSpawnInPane: (paneId: string) => void;
  hasRepos: boolean;
}

export default function GridRenderer({
  tab,
  client,
  sessions,
  subscribePty,
  focusedPaneId,
  onFocusPane,
  onSpawnInPane,
  hasRepos,
}: Props) {
  return (
    <div className="grid-root">
      <NodeRenderer
        node={tab.grid}
        path={[]}
        tabId={tab.id}
        client={client}
        sessions={sessions}
        subscribePty={subscribePty}
        focusedPaneId={focusedPaneId}
        onFocusPane={onFocusPane}
        onSpawnInPane={onSpawnInPane}
        hasRepos={hasRepos}
      />
    </div>
  );
}

interface NodeProps {
  node: GridNode;
  path: number[];
  tabId: string;
  client: DaemonClient;
  sessions: SessionSnapshot[];
  subscribePty: (sessionId: string, cb: (b64: string) => void) => () => void;
  focusedPaneId: string | null;
  onFocusPane: (paneId: string) => void;
  onSpawnInPane: (paneId: string) => void;
  hasRepos: boolean;
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
  // Local optimistic ratio while the user drags. Synced to props.node.ratio on
  // pointerup (when we send SetPaneRatio) and whenever the daemon broadcasts
  // an updated tab snapshot.
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

function PaneChrome(props: PaneChromeProps) {
  const { node, tabId, client, sessions, subscribePty, focusedPaneId } = props;
  const session = node.session_id
    ? (sessions.find((s) => s.id === node.session_id) ?? null)
    : null;
  const isFocused = focusedPaneId === node.pane_id;

  const onClick = useCallback(() => {
    props.onFocusPane(node.pane_id);
  }, [props, node.pane_id]);

  const sendSplit = useCallback(
    (direction: SplitDirection) => {
      client.send({
        type: "split_pane",
        tab_id: tabId,
        pane_id: node.pane_id,
        direction,
        place: "second",
        new_session_id: null,
      });
    },
    [client, tabId, node.pane_id],
  );

  const onClose = useCallback(() => {
    client.send({ type: "close_pane", tab_id: tabId, pane_id: node.pane_id });
  }, [client, tabId, node.pane_id]);

  const classes = ["grid-pane", isFocused ? "is-focused" : ""]
    .filter(Boolean)
    .join(" ");

  return (
    <div className={classes} onPointerDownCapture={onClick}>
      <div className="grid-pane-controls">
        <button
          type="button"
          className="grid-pane-btn"
          title="Split right"
          onClick={() => sendSplit("horizontal")}
        >
          ▶|
        </button>
        <button
          type="button"
          className="grid-pane-btn"
          title="Split down"
          onClick={() => sendSplit("vertical")}
        >
          ▼=
        </button>
        <button
          type="button"
          className="grid-pane-btn grid-pane-btn-close"
          title="Close pane"
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
            subscribePty={subscribePty}
          />
        ) : (
          <EmptyPane
            hasRepos={props.hasRepos}
            onSpawn={() => props.onSpawnInPane(node.pane_id)}
          />
        )}
      </div>
    </div>
  );
}
