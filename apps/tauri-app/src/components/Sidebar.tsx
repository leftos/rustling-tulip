import { useMemo, useState } from "react";
import type { ConnectionState } from "../api";
import type { RepoEntry, SessionSnapshot, WorkspaceEntry } from "../types";

export type SpawnInitialTarget =
  | { kind: "repo"; repo_id: string }
  | { kind: "workspace"; workspace_id: string };

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
  onOpenSpawn: (initial?: SpawnInitialTarget) => void;
  onOpenWorkspaceCreator: () => void;
  onRevealInExplorer: (path: string) => void;
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
  /// Filesystem path for repo containers; null for workspaces (which span
  /// multiple repos) and the detached pseudo-container.
  fsPath: string | null;
  sessions: SessionSnapshot[];
  // True iff this container can be removed via the inline × button.
  removable: boolean;
}

interface ContextMenuState {
  x: number;
  y: number;
  container: TreeContainer;
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

  const [contextMenu, setContextMenu] = useState<ContextMenuState | null>(null);
  const closeMenu = () => setContextMenu(null);

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
        <button
          type="button"
          className="primary small"
          onClick={() => props.onOpenSpawn()}
        >
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
                onContextMenu={(x, y) =>
                  setContextMenu({ x, y, container: c })
                }
              />
            );
          })}
        </ul>
      )}

      {contextMenu && (
        <ContainerContextMenu
          state={contextMenu}
          onClose={closeMenu}
          onSpawn={() => {
            const c = contextMenu.container;
            if (c.kind === "repo") {
              props.onOpenSpawn({ kind: "repo", repo_id: c.id });
            } else if (c.kind === "workspace") {
              props.onOpenSpawn({ kind: "workspace", workspace_id: c.id });
            }
            closeMenu();
          }}
          onRemove={() => {
            const c = contextMenu.container;
            if (c.kind === "repo") props.onRemoveRepo(c.id);
            else if (c.kind === "workspace") props.onRemoveWorkspace(c.id);
            closeMenu();
          }}
          onReveal={() => {
            const path = contextMenu.container.fsPath;
            if (path) props.onRevealInExplorer(path);
            closeMenu();
          }}
          onCopyPath={() => {
            const path = contextMenu.container.fsPath;
            if (path) void navigator.clipboard.writeText(path);
            closeMenu();
          }}
        />
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
  onContextMenu: (x: number, y: number) => void;
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

  const onContext = (e: React.MouseEvent) => {
    if (c.kind === "detached") return;
    e.preventDefault();
    p.onContextMenu(e.clientX, e.clientY);
  };

  return (
    <li>
      <div
        className={headerClasses}
        onClick={hasChildren ? p.onToggle : undefined}
        onContextMenu={onContext}
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

interface ContextMenuProps {
  state: ContextMenuState;
  onClose: () => void;
  onSpawn: () => void;
  onRemove: () => void;
  onReveal: () => void;
  onCopyPath: () => void;
}

function ContainerContextMenu(p: ContextMenuProps) {
  const c = p.state.container;
  const canReveal = c.fsPath !== null;
  const removeLabel =
    c.kind === "workspace" ? "Remove workspace" : "Remove repo";

  // Backdrop catches clicks/keys to close. The menu itself is positioned at
  // the cursor; max-bounds-checking is left to the browser for now (the
  // panel is small and the sidebar is the main interaction surface).
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
          <button type="button" onClick={p.onSpawn}>
            Spawn new session
          </button>
        </li>
        <li>
          <button type="button" onClick={p.onReveal} disabled={!canReveal}>
            Open in file explorer
          </button>
        </li>
        <li>
          <button type="button" onClick={p.onCopyPath} disabled={!canReveal}>
            Copy path
          </button>
        </li>
        <li className="context-menu-separator" aria-hidden="true" />
        <li>
          <button type="button" className="danger" onClick={p.onRemove}>
            {removeLabel}
          </button>
        </li>
      </ul>
    </div>
  );
}

function buildContainers(
  repos: RepoEntry[],
  workspaces: WorkspaceEntry[],
  sessions: SessionSnapshot[],
): TreeContainer[] {
  const workspaceById = new Map(workspaces.map((w) => [w.id, w] as const));
  const repoById = new Map(repos.map((r) => [r.id, r] as const));

  // Repos that are members of any workspace don't get their own top-level
  // container — they live under the workspace instead. Their single-repo
  // sessions (if any) still appear in the Detached bucket below.
  const memberRepoIds = new Set<string>();
  for (const w of workspaces) {
    for (const id of w.member_repo_ids) memberRepoIds.add(id);
  }

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
      // Single-repo session: parent is members[0].repo_id, but only if that
      // repo is still a top-level container. If the repo became a workspace
      // member, the session has no obvious home and we surface it as detached.
      const primaryRepoId = s.members[0]?.repo_id;
      if (
        primaryRepoId &&
        repoById.has(primaryRepoId) &&
        !memberRepoIds.has(primaryRepoId)
      ) {
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
      fsPath: null,
      sessions: sortSessions(wsSessions.get(w.id) ?? []),
      removable: true,
    });
  }

  const sortedRepos = [...repos]
    .filter((r) => !memberRepoIds.has(r.id))
    .sort((a, b) => a.name.localeCompare(b.name));
  for (const r of sortedRepos) {
    out.push({
      key: `repo:${r.id}`,
      kind: "repo",
      id: r.id,
      name: r.name,
      hoverTitle: r.path,
      fsPath: r.path,
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
      fsPath: null,
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
