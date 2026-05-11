import { useEffect, useMemo, useState } from "react";
import { type ConnectionState, type DaemonClient, listPresets } from "../api";
import {
  tabGrid,
  type PresetEntry,
  type PresetTarget,
  type RepoEntry,
  type SessionSnapshot,
  type TabEntry,
  type WorkspaceEntry,
} from "../types";
import { clampMenuCoord, useEscape } from "../utils/a11y";
import { collectPanes, sessionTabBindings } from "../utils/grid";
import {
  sessionDisplayLabel,
  sessionLabelTooltip,
  sessionRuntimeLabel,
} from "../utils/sessionLabel";
import { saveSettings, useSettings } from "../utils/settings";

/// Drag MIME shared with the grid + tab bar so sidebar leaves can be dropped
/// into existing pane drop zones. Payload is `${tabId}:${paneId}` — identical
/// to GridRenderer's DRAG_MIME format. Kept in sync by convention; if this
/// drifts, multi-surface drag stops working.
const DRAG_MIME = "text/x-rt-pane";

export type SpawnInitialTarget =
  | { kind: "repo"; repo_id: string }
  | { kind: "workspace"; workspace_id: string };

export interface RepoRemoveIntent {
  repoId: string;
  repoName: string;
  liveSessions: SessionSnapshot[];
}

interface Props {
  repos: RepoEntry[];
  workspaces: WorkspaceEntry[];
  sessions: SessionSnapshot[];
  /// Used to compute and display which tab(s) each session is currently
  /// open in. A session leaf renders a `[T:<tab-name>]` pill when bound, or
  /// `[unbound]` when no pane references it. Drives the drag-source payload
  /// in iter 5.B as well.
  tabs: TabEntry[];
  client: DaemonClient;
  /// Session ids visually highlighted in the tree because they appear in
  /// the currently-active tab. Multiple sessions can be highlighted at once
  /// (one tab can hold several panes referencing different sessions).
  highlightedSessionIds: Set<string>;
  attentionSessions: Set<string>;
  connection: ConnectionState | { kind: "init" } | { kind: "error"; reason: string };
  onAddRepo: () => void;
  onRemoveRepo: (id: string) => void;
  /// Called when the user clicks × on a repo that has live sessions.
  /// The app opens a confirmation modal with a 3-way choice
  /// (cancel / remove anyway / stop sessions and remove).
  onRemoveRepoWithLiveSessions: (intent: RepoRemoveIntent) => void;
  onRemoveWorkspace: (id: string) => void;
  onSelectSession: (id: string) => void;
  onOpenSpawn: (initial?: SpawnInitialTarget) => void;
  onOpenWorkspaceCreator: () => void;
  onRevealInExplorer: (path: string) => void;
  onLaunchPreset: (preset: PresetEntry, target: PresetTarget) => void;
  /// Open the Settings modal (iter 49). Gear icon in the sidebar header
  /// + Ctrl/Cmd+, in App-level keyboard shortcuts both invoke this.
  onOpenSettings: () => void;
}

type ContainerKind = "workspace" | "repo" | "detached" | "tab" | "unbound";

/// Sidebar tree organization. "container" groups by workspace/repo/detached
/// (matches the daemon's registry view); "tab" groups by which tab each
/// session is currently open in. User-toggled, persisted to localStorage.
type SidebarView = "container" | "tab";
/// Render a small connection-state badge for the sidebar header. Returns
/// `null` when the connection is `open` so the happy path stays uncluttered;
/// any other state surfaces as a coloured chip with a tooltip carrying the
/// reason. Closes the audit's "Connection badge is only visible in the
/// EmptyState" finding for non-EmptyState views.
function renderConnectionBadge(
  state:
    | ConnectionState
    | { kind: "init" }
    | { kind: "error"; reason: string },
): React.ReactNode {
  if (state.kind === "open") return null;
  if (state.kind === "init") {
    return (
      <span
        className="badge badge-warn sidebar-connection-badge"
        data-testid="sidebar-connection-badge"
      >
        starting
      </span>
    );
  }
  if (state.kind === "connecting") {
    return (
      <span
        className="badge badge-warn sidebar-connection-badge"
        data-testid="sidebar-connection-badge"
      >
        connecting
      </span>
    );
  }
  if (state.kind === "auth_failed") {
    return (
      <span
        className="badge badge-err sidebar-connection-badge"
        title={state.reason}
        data-testid="sidebar-connection-badge"
      >
        auth failed
      </span>
    );
  }
  if (state.kind === "error") {
    return (
      <span
        className="badge badge-err sidebar-connection-badge"
        title={state.reason}
        data-testid="sidebar-connection-badge"
      >
        error
      </span>
    );
  }
  return (
    <span
      className="badge badge-warn sidebar-connection-badge"
      title={state.reason}
      data-testid="sidebar-connection-badge"
    >
      disconnected
    </span>
  );
}


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
  // The current sidebar view is a per-window selection but it also serves
  // as the persisted default for the next window — same key, same value.
  // Source-of-truth is `settings.sidebar.default_view` (managed by
  // `utils/settings.ts`), seeded on first load from the legacy
  // `rt.sidebar.view` localStorage key.
  const [settings] = useSettings();
  const [view, setView] = useState<SidebarView>(
    () => settings.sidebar.default_view,
  );
  const updateView = (next: SidebarView) => {
    setView(next);
    saveSettings({
      ...settings,
      sidebar: { default_view: next },
    });
  };
  const containers = useMemo(
    () =>
      view === "tab"
        ? buildTabContainers(props.tabs, props.sessions)
        : buildContainers(props.repos, props.workspaces, props.sessions),
    [view, props.tabs, props.repos, props.workspaces, props.sessions],
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

  // Presets are fetched on demand when the context menu opens. Per-target
  // cache keeps subsequent opens snappy; null means "not fetched yet",
  // Discriminated cache so the submenu can distinguish "fetched empty"
  // from "fetched failed / timed out" — previously both fell through to
  // `[]` and the menu was stuck on "(loading)" forever.
  type PresetCacheEntry =
    | { ok: true; entries: PresetEntry[] }
    | { ok: false; reason: string };
  const [presetCache, setPresetCache] = useState<Map<string, PresetCacheEntry>>(
    () => new Map(),
  );
  const currentTarget = contextMenu ? menuTarget(contextMenu.container) : null;
  const currentTargetKey = currentTarget ? targetCacheKey(currentTarget) : null;
  const currentPresets =
    currentTargetKey === null ? null : (presetCache.get(currentTargetKey) ?? null);

  useEffect(() => {
    if (!currentTarget || currentTargetKey === null) return;
    if (presetCache.has(currentTargetKey)) return;
    void listPresets(props.client, currentTarget).then((result) => {
      setPresetCache((prev) => {
        const next = new Map(prev);
        next.set(currentTargetKey, result);
        return next;
      });
    });
  }, [currentTarget, currentTargetKey, presetCache, props.client]);

  // Force-expand any container whose **highlighted** session lives inside.
  // Highlighting is user-initiated (clicking into a pane with that session),
  // so the expand-on-discovery is welcome. Attention used to also force the
  // expansion, but that overrode the user's collapse decision for sessions
  // they intentionally backgrounded — audit's "Force-expand silently
  // overrides the user's collapse state". The container header surfaces a
  // distinct `has-attention` rollup chip instead, so the warning is still
  // visible without disturbing the layout.
  const forceExpand = useMemo(() => {
    const out = new Set<string>();
    for (const c of containers) {
      for (const s of c.sessions) {
        if (props.highlightedSessionIds.has(s.id)) {
          out.add(c.key);
          break;
        }
      }
    }
    return out;
  }, [containers, props.highlightedSessionIds]);

  // Sets of container keys whose sessions include at least one
  // attention-flagged id. Computed once per render and threaded into the
  // container header below so collapsed containers can still show a
  // warning chip.
  const containersWithAttention = useMemo(() => {
    const out = new Set<string>();
    for (const c of containers) {
      for (const s of c.sessions) {
        if (props.attentionSessions.has(s.id)) {
          out.add(c.key);
          break;
        }
      }
    }
    return out;
  }, [containers, props.attentionSessions]);

  const connectionBadge = renderConnectionBadge(props.connection);

  return (
    <aside className="sidebar" data-testid="sidebar">
      <header className="sidebar-header">
        <span className="brand">rustling-tulip</span>
        {connectionBadge}
        <button
          type="button"
          className="sidebar-settings-btn"
          onClick={props.onOpenSettings}
          aria-label="Open settings"
          title="Settings (Ctrl+,)"
          data-testid="sidebar-settings-btn"
        >
          ⚙
        </button>
        <div
          className="sidebar-view-toggle"
          role="tablist"
          aria-label="Sidebar view"
          data-testid="sidebar-view-toggle"
        >
          <button
            type="button"
            role="tab"
            aria-selected={view === "container"}
            className={view === "container" ? "active" : ""}
            onClick={() => updateView("container")}
            title="Group by workspace/repo"
            data-testid="sidebar-view-container"
          >
            Repos
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={view === "tab"}
            className={view === "tab" ? "active" : ""}
            onClick={() => updateView("tab")}
            title="Group by tab"
            data-testid="sidebar-view-tab"
          >
            Tabs
          </button>
        </div>
      </header>

      <div className="sidebar-toolbar">
        <button
          type="button"
          className="primary small"
          onClick={
            props.repos.length > 0 ? () => props.onOpenSpawn() : undefined
          }
          disabled={props.repos.length === 0}
          title={
            props.repos.length === 0
              ? "Register a repo first"
              : "Spawn a new session"
          }
          data-testid="sidebar-add-session"
        >
          + Session
        </button>
        <button
          type="button"
          className="link"
          onClick={props.onAddRepo}
          data-testid="sidebar-add-repo"
        >
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
          data-testid="sidebar-add-workspace"
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
                hasAttention={containersWithAttention.has(c.key)}
                onToggle={() => toggle(c.key)}
                tabs={props.tabs}
                client={props.client}
                highlightedSessionIds={props.highlightedSessionIds}
                attentionSessions={props.attentionSessions}
                onSelectSession={props.onSelectSession}
                onRemoveRepo={props.onRemoveRepo}
                onRemoveRepoWithLiveSessions={props.onRemoveRepoWithLiveSessions}
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
          presets={currentPresets}
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
            if (c.kind === "repo") {
              const live = c.sessions.filter((s) => s.status !== "stopped");
              if (live.length > 0) {
                props.onRemoveRepoWithLiveSessions({
                  repoId: c.id,
                  repoName: c.name,
                  liveSessions: live,
                });
              } else {
                props.onRemoveRepo(c.id);
              }
            } else if (c.kind === "workspace") props.onRemoveWorkspace(c.id);
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
          onLaunchPreset={(preset) => {
            const target = menuTarget(contextMenu.container);
            if (target) props.onLaunchPreset(preset, target);
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
  /// True iff at least one of the container's sessions currently sits in
  /// the `attentionSessions` set. Shown as a small chip on the container
  /// header so a collapsed container still surfaces a "something inside
  /// wants your attention" signal — closes the audit finding about
  /// attention being invisible when the user collapses a container.
  hasAttention: boolean;
  onToggle: () => void;
  tabs: TabEntry[];
  client: DaemonClient;
  highlightedSessionIds: Set<string>;
  attentionSessions: Set<string>;
  onSelectSession: (id: string) => void;
  onRemoveRepo: (id: string) => void;
  onRemoveRepoWithLiveSessions: (intent: RepoRemoveIntent) => void;
  onRemoveWorkspace: (id: string) => void;
  onContextMenu: (x: number, y: number) => void;
}

function ContainerNode(p: ContainerNodeProps) {
  const c = p.container;
  const hasChildren = c.sessions.length > 0;
  const headerClasses = ["tree-row", "tree-container", `tree-container-${c.kind}`]
    .filter(Boolean)
    .join(" ");

  /// Inline two-state confirm for workspace remove and repo remove without
  /// live sessions. The repo-with-live-sessions case bypasses this and
  /// opens a modal directly, since the user needs the 3-way choice
  /// (cancel / remove anyway / stop sessions and remove).
  const [confirming, setConfirming] = useState(false);

  const onRemoveClick = () => {
    if (c.kind === "repo") {
      const live = c.sessions.filter((s) => s.status !== "stopped");
      if (live.length > 0) {
        p.onRemoveRepoWithLiveSessions({
          repoId: c.id,
          repoName: c.name,
          liveSessions: live,
        });
        return;
      }
    }
    if (!confirming) {
      setConfirming(true);
      return;
    }
    setConfirming(false);
    if (c.kind === "repo") p.onRemoveRepo(c.id);
    else if (c.kind === "workspace") p.onRemoveWorkspace(c.id);
  };

  const onContext = (e: React.MouseEvent) => {
    // Only `workspace` / `repo` containers have a context menu. Detached,
    // tab, and unbound are read-only groupings.
    if (c.kind !== "workspace" && c.kind !== "repo") return;
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
        tabIndex={hasChildren ? 0 : undefined}
        aria-expanded={hasChildren ? !p.collapsed : undefined}
        aria-label={hasChildren ? `${c.kind} ${c.name}` : undefined}
        onKeyDown={
          hasChildren
            ? (e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.preventDefault();
                  p.onToggle();
                }
              }
            : undefined
        }
        title={c.hoverTitle}
        data-testid={`sidebar-container-${c.kind}`}
        data-container-id={c.id}
        data-container-name={c.name}
      >
        <span className="tree-caret" aria-hidden="true">
          {hasChildren ? (p.collapsed ? "▸" : "▾") : ""}
        </span>
        <span className="tree-kind-tag">
          {c.kind === "workspace"
            ? "WS"
            : c.kind === "repo"
              ? "REPO"
              : c.kind === "tab"
                ? "TAB"
                : c.kind === "unbound"
                  ? "UNB"
                  : "?"}
        </span>
        <span className="tree-label">{c.name}</span>
        {c.sessions.length > 0 && (
          <span className="list-item-meta">{c.sessions.length}</span>
        )}
        {p.hasAttention && (
          <span
            className="container-attention-chip"
            title="A session in this container is awaiting input or has stopped/errored"
            aria-label="container has attention-flagged session"
            data-testid="container-attention-chip"
          >
            !
          </span>
        )}
        {c.removable && (
          <>
            <button
              type="button"
              className={
                confirming
                  ? "list-item-action danger"
                  : "list-item-action"
              }
              title={
                confirming
                  ? "Click again to confirm"
                  : c.kind === "workspace"
                    ? "Remove workspace"
                    : "Remove repo"
              }
              aria-label={
                confirming
                  ? `Confirm removal of ${c.kind} ${c.name}`
                  : `Remove ${c.kind} ${c.name}`
              }
              data-testid={`sidebar-remove-${c.kind}`}
              data-confirming={confirming ? "true" : "false"}
              onClick={(e) => {
                e.stopPropagation();
                onRemoveClick();
              }}
            >
              {confirming ? "✓?" : "×"}
            </button>
            {confirming && (
              <button
                type="button"
                className="list-item-action"
                title="Cancel"
                aria-label="Cancel removal"
                data-testid={`sidebar-remove-${c.kind}-cancel`}
                onClick={(e) => {
                  e.stopPropagation();
                  setConfirming(false);
                }}
              >
                ⌫
              </button>
            )}
          </>
        )}
      </div>
      {hasChildren && !p.collapsed && (
        <ul className="tree-children">
          {c.kind === "detached" && (
            <li className="tree-children-banner" data-testid="detached-banner">
              These sessions are still alive, but their owning repo or
              workspace is no longer registered (or got rolled into a
              workspace after they spawned). Use the session header to stop or
              pop them out.
            </li>
          )}
          {c.kind === "unbound" && (
            <li className="tree-children-banner" data-testid="unbound-banner">
              These sessions are alive but no tab currently references them.
              Click the <span className="tree-tab-pill unbound">unbound</span>
              {" "}pill on a session to open it in a new tab.
            </li>
          )}
          {c.sessions.map((s) => (
            <SessionLeaf
              key={s.id}
              session={s}
              tabs={p.tabs}
              client={p.client}
              selected={p.highlightedSessionIds.has(s.id)}
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
  tabs: TabEntry[];
  client: DaemonClient;
  selected: boolean;
  needsAttention: boolean;
  onSelect: (id: string) => void;
}

function SessionLeaf(p: SessionLeafProps) {
  const s = p.session;
  const isPlainShell = s.mode === "plain_shell";
  const isCodex = !isPlainShell && s.agent === "codex";
  const bindings = useMemo(
    () => sessionTabBindings(s.id, p.tabs),
    [s.id, p.tabs],
  );
  const draggable = bindings.length > 0 && !s.is_orphan;
  const primaryBinding = bindings[0] ?? null;
  const onDragStart = (e: React.DragEvent) => {
    if (!primaryBinding) return;
    e.dataTransfer.effectAllowed = "move";
    e.dataTransfer.setData(
      DRAG_MIME,
      `${primaryBinding.tab_id}:${primaryBinding.pane_id}`,
    );
  };
  const onBindUnbound = (e: React.MouseEvent) => {
    e.stopPropagation();
    p.client.send({
      type: "create_tab",
      name: null,
      initial_session_id: s.id,
    });
  };
  const classes = [
    "tree-row",
    "tree-leaf",
    p.selected ? "selected" : "",
    p.needsAttention ? "needs-attention" : "",
    s.is_orphan ? "is-orphan" : "",
    isPlainShell ? "is-shell" : "",
    isCodex ? "is-codex" : "",
    draggable ? "is-draggable" : "",
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <li>
      <div
        className={classes}
        onClick={() => p.onSelect(s.id)}
        role="button"
        tabIndex={0}
        aria-label={`Session ${s.label}`}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            p.onSelect(s.id);
          }
        }}
        draggable={draggable}
        onDragStart={onDragStart}
        data-testid="sidebar-session"
        data-session-id={s.id}
        data-session-status={s.status}
        data-session-agent={s.agent}
        data-tab-binding-count={bindings.length}
      >
        {isPlainShell ? (
          <span
            className="status-glyph"
            aria-hidden="true"
            title="Plain shell session"
          >
            {">_"}
          </span>
        ) : (
          <span
            className={`status-dot status-${s.status}`}
            title={`status: ${s.status}`}
            aria-label={`status ${s.status}`}
            role="img"
          />
        )}
        <span
          className="tree-label"
          title={sessionLabelTooltip(s)}
        >
          {sessionDisplayLabel(s)}
        </span>
        {(() => {
          const runtime = sessionRuntimeLabel(s);
          if (!runtime) return null;
          return (
            <span
              className="tree-kind-tag"
              title={`Running ${runtime}`}
              data-testid="session-runtime-tag"
            >
              {runtime}
            </span>
          );
        })()}
        {p.needsAttention && <span className="badge badge-warn small">!</span>}
        {s.is_orphan && (
          <span className="list-item-meta" title="Reattached after daemon restart; PTY detached">
            orphan
          </span>
        )}
        <TabPill
          bindings={bindings}
          sessionId={s.id}
          onBindUnbound={onBindUnbound}
        />
      </div>
    </li>
  );
}

interface TabPillProps {
  bindings: Array<{ tab_id: string; tab_name: string; pane_id: string }>;
  sessionId: string;
  onBindUnbound: (e: React.MouseEvent) => void;
}

/// Inline pill showing which tab(s) reference this session. Drives the
/// "where is this session right now?" mental model and (when bound) provides
/// the drag-source data-attrs that wdio specs key off of. Clicking the
/// `[unbound]` variant fires CreateTab { initial_session_id } to bind the
/// session to a new tab — fastest path to recover from a tab close.
function TabPill(p: TabPillProps) {
  if (p.bindings.length === 0) {
    return (
      <button
        type="button"
        className="tree-tab-pill unbound"
        title="No tab references this session — click to open it in a new tab"
        aria-label="Open session in a new tab (currently unbound)"
        data-testid="session-tab-pill-unbound"
        onClick={p.onBindUnbound}
      >
        unbound
      </button>
    );
  }
  if (p.bindings.length === 1) {
    const b = p.bindings[0]!;
    return (
      <span
        className="tree-tab-pill"
        title={`Open in tab "${b.tab_name}"`}
        data-testid="session-tab-pill"
        data-tab-id={b.tab_id}
      >
        T:{b.tab_name}
      </span>
    );
  }
  const summary = p.bindings.map((b) => b.tab_name).join(", ");
  return (
    <span
      className="tree-tab-pill multi"
      title={`Open in ${p.bindings.length} tabs: ${summary}`}
      data-testid="session-tab-pill"
      data-tab-binding-count={p.bindings.length}
    >
      T:×{p.bindings.length}
    </span>
  );
}

interface ContextMenuProps {
  state: ContextMenuState;
  presets:
    | { ok: true; entries: PresetEntry[] }
    | { ok: false; reason: string }
    | null;
  onClose: () => void;
  onSpawn: () => void;
  onRemove: () => void;
  onReveal: () => void;
  onCopyPath: () => void;
  onLaunchPreset: (preset: PresetEntry) => void;
}

function ContainerContextMenu(p: ContextMenuProps) {
  const c = p.state.container;
  const canReveal = c.fsPath !== null;
  const canLaunchPreset = c.kind === "repo" || c.kind === "workspace";
  const removeLabel =
    c.kind === "workspace" ? "Remove workspace" : "Remove repo";
  useEscape(p.onClose);

  // Clamp the menu against the window so a right-click near the
  // bottom-right corner doesn't render the menu beyond the viewport.
  // Estimates: menus average ~200px wide, ~300px tall (presets push higher
  // — overlap-safe, viewport-stable beats pixel-perfect). The 12px margin
  // keeps the menu off the edge.
  const clampedLeft = clampMenuCoord(p.state.x, 200);
  const clampedTop = clampMenuCoord(p.state.y, 300, "height");
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
        style={{ left: clampedLeft, top: clampedTop }}
        onClick={(e) => e.stopPropagation()}
      >
        <li>
          <button type="button" onClick={p.onSpawn}>
            Spawn new session
          </button>
        </li>
        {canLaunchPreset && (
          <PresetSubmenu presets={p.presets} onLaunch={p.onLaunchPreset} />
        )}
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

function PresetSubmenu({
  presets,
  onLaunch,
}: {
  presets:
    | { ok: true; entries: PresetEntry[] }
    | { ok: false; reason: string }
    | null;
  onLaunch: (preset: PresetEntry) => void;
}) {
  if (presets === null) {
    return (
      <li>
        <button type="button" disabled>
          Launch preset… (loading)
        </button>
      </li>
    );
  }
  if (!presets.ok) {
    return (
      <li>
        <button type="button" disabled title={presets.reason}>
          Launch preset… (failed to load)
        </button>
      </li>
    );
  }
  if (presets.entries.length === 0) {
    return (
      <li>
        <button type="button" disabled title="No .rustling-tulip/presets.json in this repo">
          Launch preset… (none defined)
        </button>
      </li>
    );
  }
  return (
    <>
      {presets.entries.map((preset) => (
        <li key={`${preset.source_repo_id}:${preset.id}`}>
          <button type="button" onClick={() => onLaunch(preset)}>
            Launch preset · {preset.name}
          </button>
        </li>
      ))}
    </>
  );
}

function menuTarget(c: TreeContainer): PresetTarget | null {
  if (c.kind === "repo") return { kind: "repo", repo_id: c.id };
  if (c.kind === "workspace") return { kind: "workspace", workspace_id: c.id };
  return null;
}

function targetCacheKey(target: PresetTarget): string {
  return target.kind === "repo"
    ? `repo:${target.repo_id}`
    : `workspace:${target.workspace_id}`;
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

/// Build the "tab" view: top-level containers are tabs in their declared
/// order, each holding the sessions whose panes the tab references. A
/// trailing "Unbound" pseudo-container catches alive sessions that no tab
/// pane references (rare — happens after CloseTab on a tab whose panes were
/// the only references). The same SessionLeaf component renders here as in
/// the container view, so the inline tab pill still appears (handy when a
/// session is referenced by multiple tabs).
function buildTabContainers(
  tabs: TabEntry[],
  sessions: SessionSnapshot[],
): TreeContainer[] {
  const sessionById = new Map(sessions.map((s) => [s.id, s] as const));
  const referencedIds = new Set<string>();
  const out: TreeContainer[] = [];
  for (const tab of tabs) {
    const tabSessions: SessionSnapshot[] = [];
    const seen = new Set<string>();
    const grid = tabGrid(tab);
    if (grid) {
      for (const pane of collectPanes(grid)) {
        if (!pane.session_id) continue;
        if (seen.has(pane.session_id)) continue;
        seen.add(pane.session_id);
        referencedIds.add(pane.session_id);
        const s = sessionById.get(pane.session_id);
        if (s) tabSessions.push(s);
      }
    }
    out.push({
      key: `tab:${tab.id}`,
      kind: "tab",
      id: tab.id,
      name: tab.name,
      hoverTitle: `Tab "${tab.name}"`,
      fsPath: null,
      sessions: tabSessions,
      removable: false,
    });
  }
  const unbound = sessions.filter((s) => !referencedIds.has(s.id));
  if (unbound.length > 0) {
    out.push({
      key: "unbound",
      kind: "unbound",
      id: "",
      name: "Unbound",
      hoverTitle:
        "Sessions alive but not referenced by any tab. Click the unbound pill on a session to open it in a new tab.",
      fsPath: null,
      sessions: [...unbound].sort((a, b) => a.label.localeCompare(b.label)),
      removable: false,
    });
  }
  return out;
}

