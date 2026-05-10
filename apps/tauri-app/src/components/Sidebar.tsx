import { useMemo, useState } from "react";
import type { ConnectionState } from "../api";
import type { RepoEntry, SessionSnapshot, WorkspaceEntry } from "../types";

interface Props {
  repos: RepoEntry[];
  workspaces: WorkspaceEntry[];
  sessions: SessionSnapshot[];
  selectedSessionId: string | null;
  attentionSessions: Set<string>;
  connection: ConnectionState | { kind: "init" } | { kind: "error"; reason: string };
  onAddRepo: () => void;
  onRemoveRepo: (id: string) => void;
  onRemoveWorkspace: (id: string) => void;
  onSelectSession: (id: string) => void;
  onOpenSpawn: () => void;
  onOpenWorkspaceCreator: () => void;
}

type ContainerKind = "workspace" | "repo" | "detached";

interface TreeContainer {
  key: string;
  kind: ContainerKind;
  // For "workspace" / "repo": the entity id. For "detached": "".
  id: string;
  name: string;
  // Subtitle line for repos (path) — hover-only via title attribute.
  hoverTitle?: string;
  sessions: SessionSnapshot[];
  // True iff this container can be removed via the inline × button.
  removable: boolean;
}

export default function Sidebar(props: Props) {
  const containers = useMemo(
    () => buildContainers(props.repos, props.workspaces, props.sessions),
    [props.repos, props.workspaces, props.sessions],
  );

  // Containers default to expanded. Local UI state; not persisted.
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const toggle = (key: string) =>
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });

  // Force-expand any container whose selected/attention session lives inside.
  const forceExpand = useMemo(() => {
    const out = new Set<string>();
    for (const c of containers) {
      for (const s of c.sessions) {
        if (
          s.id === props.selectedSessionId ||
          props.attentionSessions.has(s.id)
        ) {
          out.add(c.key);
          break;
        }
      }
    }
    return out;
  }, [containers, props.selectedSessionId, props.attentionSessions]);

  return (
    <aside className="sidebar">
      <header className="sidebar-header">
        <span className="brand">rustling-tulip</span>
      </header>

      <div className="sidebar-toolbar">
        <button type="button" className="primary small" onClick={props.onOpenSpawn}>
          + Session
        </button>
        <button type="button" className="link" onClick={props.onAddRepo}>
          + Repo
        </button>
        <button
          type="button"
          className="link"
          onClick={
            props.repos.length >= 2 ? props.onOpenWorkspaceCreator : undefined
          }
          disabled={props.repos.length < 2}
          title={
            props.repos.length < 2
              ? "Register at least 2 repos first"
              : "Create a workspace"
          }
        >
          + Workspace
        </button>
      </div>

      {containers.length === 0 ? (
        <p className="empty" style={{ padding: "12px 14px" }}>
          No repos or workspaces yet — start with{" "}
          <button type="button" className="link inline" onClick={props.onAddRepo}>
            + Repo
          </button>
          .
        </p>
      ) : (
        <ul className="tree">
          {containers.map((c) => {
            const isCollapsed = collapsed.has(c.key) && !forceExpand.has(c.key);
            return (
              <ContainerNode
                key={c.key}
                container={c}
                collapsed={isCollapsed}
                onToggle={() => toggle(c.key)}
                selectedSessionId={props.selectedSessionId}
                attentionSessions={props.attentionSessions}
                onSelectSession={props.onSelectSession}
                onRemoveRepo={props.onRemoveRepo}
                onRemoveWorkspace={props.onRemoveWorkspace}
              />
            );
          })}
        </ul>
      )}
    </aside>
  );
}

interface ContainerNodeProps {
  container: TreeContainer;
  collapsed: boolean;
  onToggle: () => void;
  selectedSessionId: string | null;
  attentionSessions: Set<string>;
  onSelectSession: (id: string) => void;
  onRemoveRepo: (id: string) => void;
  onRemoveWorkspace: (id: string) => void;
}

function ContainerNode(p: ContainerNodeProps) {
  const c = p.container;
  const hasChildren = c.sessions.length > 0;
  const headerClasses = ["tree-row", "tree-container", `tree-container-${c.kind}`]
    .filter(Boolean)
    .join(" ");

  const onRemove = () => {
    if (c.kind === "repo") p.onRemoveRepo(c.id);
    else if (c.kind === "workspace") p.onRemoveWorkspace(c.id);
  };

  return (
    <li>
      <div
        className={headerClasses}
        onClick={hasChildren ? p.onToggle : undefined}
        role={hasChildren ? "button" : undefined}
        title={c.hoverTitle}
      >
        <span className="tree-caret" aria-hidden="true">
          {hasChildren ? (p.collapsed ? "▸" : "▾") : ""}
        </span>
        <span className="tree-kind-tag">
          {c.kind === "workspace" ? "WS" : c.kind === "repo" ? "REPO" : "?"}
        </span>
        <span className="tree-label">{c.name}</span>
        {c.sessions.length > 0 && (
          <span className="list-item-meta">{c.sessions.length}</span>
        )}
        {c.removable && (
          <button
            type="button"
            className="list-item-action"
            title={c.kind === "workspace" ? "Remove workspace" : "Remove repo"}
            onClick={(e) => {
              e.stopPropagation();
              onRemove();
            }}
          >
            ×
          </button>
        )}
      </div>
      {hasChildren && !p.collapsed && (
        <ul className="tree-children">
          {c.sessions.map((s) => (
            <SessionLeaf
              key={s.id}
              session={s}
              selected={s.id === p.selectedSessionId}
              needsAttention={p.attentionSessions.has(s.id)}
              onSelect={p.onSelectSession}
            />
          ))}
        </ul>
      )}
    </li>
  );
}

interface SessionLeafProps {
  session: SessionSnapshot;
  selected: boolean;
  needsAttention: boolean;
  onSelect: (id: string) => void;
}

function SessionLeaf(p: SessionLeafProps) {
  const s = p.session;
  const classes = [
    "tree-row",
    "tree-leaf",
    p.selected ? "selected" : "",
    p.needsAttention ? "needs-attention" : "",
    s.is_orphan ? "is-orphan" : "",
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <li>
      <div className={classes} onClick={() => p.onSelect(s.id)} role="button">
        <span className={`status-dot status-${s.status}`} />
        <span className="tree-label" title={s.label}>
          {s.label}
        </span>
        {p.needsAttention && <span className="badge badge-warn small">!</span>}
        {s.is_orphan && (
          <span className="list-item-meta" title="Reattached after daemon restart; PTY detached">
            orphan
          </span>
        )}
      </div>
    </li>
  );
}

function buildContainers(
  repos: RepoEntry[],
  workspaces: WorkspaceEntry[],
  sessions: SessionSnapshot[],
): TreeContainer[] {
  const workspaceById = new Map(workspaces.map((w) => [w.id, w] as const));
  const repoById = new Map(repos.map((r) => [r.id, r] as const));

  // Group sessions by their owning container.
  const wsSessions = new Map<string, SessionSnapshot[]>();
  const repoSessions = new Map<string, SessionSnapshot[]>();
  const detached: SessionSnapshot[] = [];

  for (const s of sessions) {
    if (s.workspace_id) {
      if (workspaceById.has(s.workspace_id)) {
        pushTo(wsSessions, s.workspace_id, s);
      } else {
        detached.push(s);
      }
    } else {
      // Single-repo session: parent is members[0].repo_id.
      const primaryRepoId = s.members[0]?.repo_id;
      if (primaryRepoId && repoById.has(primaryRepoId)) {
        pushTo(repoSessions, primaryRepoId, s);
      } else {
        detached.push(s);
      }
    }
  }

  const sortSessions = (arr: SessionSnapshot[]) =>
    [...arr].sort((a, b) => a.label.localeCompare(b.label));

  const out: TreeContainer[] = [];

  // Workspaces first (typically fewer, and they're the differentiating concept).
  const sortedWorkspaces = [...workspaces].sort((a, b) =>
    a.name.localeCompare(b.name),
  );
  for (const w of sortedWorkspaces) {
    out.push({
      key: `ws:${w.id}`,
      kind: "workspace",
      id: w.id,
      name: w.name,
      sessions: sortSessions(wsSessions.get(w.id) ?? []),
      removable: true,
    });
  }

  const sortedRepos = [...repos].sort((a, b) => a.name.localeCompare(b.name));
  for (const r of sortedRepos) {
    out.push({
      key: `repo:${r.id}`,
      kind: "repo",
      id: r.id,
      name: r.name,
      hoverTitle: r.path,
      sessions: sortSessions(repoSessions.get(r.id) ?? []),
      removable: true,
    });
  }

  if (detached.length > 0) {
    out.push({
      key: "detached",
      kind: "detached",
      id: "",
      name: "Detached",
      hoverTitle: "Sessions whose containing repo or workspace is no longer registered",
      sessions: sortSessions(detached),
      removable: false,
    });
  }

  return out;
}

function pushTo<K, V>(map: Map<K, V[]>, key: K, value: V) {
  const arr = map.get(key);
  if (arr) arr.push(value);
  else map.set(key, [value]);
}
