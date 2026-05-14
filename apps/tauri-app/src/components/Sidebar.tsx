import { useEffect, useMemo, useState } from "react";
import {
  listPresets,
  type ConnectionState,
  type DaemonClient,
  type DaemonHandshake,
} from "../api";
import {
  tabGrid,
  type AppearanceOverrides,
  type ContainerRef,
  type PresetEntry,
  type PresetTarget,
  type RepoEntry,
  type SessionSnapshot,
  type SpawnConfig,
  type TabEntry,
  type VscodeWorkspaceSuggestion,
  type WorkspaceEntry,
} from "../types";
import { clampMenuCoord, useEscape } from "../utils/a11y";
import { collectPanes, sessionTabBindings } from "../utils/grid";
import {
  sessionDisplayLabel,
  sessionLabelTooltip,
  sessionRuntimeLabel,
} from "../utils/sessionLabel";
import { sessionAccentStyle } from "../utils/sessionColor";
import { copyToClipboard } from "../utils/clipboard";
import { saveSettings, useSettings, type Settings } from "../utils/settings";
import { resolveAppearance, resolveAppearanceLayers } from "../utils/appearance";
import DaemonFooter from "./DaemonFooter";
import Icon from "./Icon";
import { AppearanceModal } from "./AppearanceEditor";
import SessionContextMenu, {
  type DuplicateTarget,
  type SessionContextMenuState,
} from "./SessionContextMenu";

/// Drag MIME shared with the grid + tab bar so sidebar leaves can be dropped
/// into existing pane drop zones. Payload is `${tabId}:${paneId}` — identical
/// to GridRenderer's DRAG_MIME format. Kept in sync by convention; if this
/// drifts, multi-surface drag stops working.
const DRAG_MIME = "text/x-rt-pane";

export type SpawnInitialTarget =
  | { kind: "repo"; repo_id: string }
  | { kind: "workspace"; workspace_id: string };

/// Where "Launch last again" sends the relaunched session.
/// `current_tab` resolves to the active tab when it can host panes;
/// otherwise it falls through to a fresh tab (handled in App.tsx).
/// `tab` pins the placement to a specific existing tab id.
export type LaunchLastPlacement =
  | { kind: "current_tab" }
  | { kind: "new_tab" }
  | { kind: "tab"; tabId: string };

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
  /// used by pane drops.
  tabs: TabEntry[];
  client: DaemonClient;
  /// Session ids visually highlighted in the tree because they appear in
  /// the currently-active tab. Multiple sessions can be highlighted at once
  /// (one tab can hold several panes referencing different sessions).
  highlightedSessionIds: Set<string>;
  attentionSessions: Set<string>;
  connection: ConnectionState | { kind: "init" } | { kind: "error"; reason: string };
  /// Handshake captured by App after `ensureDaemonStarted` succeeds.
  /// Threaded down to the DaemonFooter so the flyout can render port +
  /// pid + protocol-version chips. `null` until the first successful
  /// supervisor call (boot, or after an intentional Stop).
  handshake: DaemonHandshake | null;
  /// True iff the user explicitly hit Stop from the footer. Suppresses
  /// the auto-reconnect cycle in App.tsx and changes the footer text
  /// from the transient "disconnected" to the intentional "stopped".
  daemonStopRequested: boolean;
  /// Footer "Restart daemon" — sends graceful shutdown via WS (auto-
  /// reconnect respawns) or bumps the connection version when stopped.
  onRestartDaemon: () => void;
  /// Footer "Stop daemon" — pid-kill via Tauri command + set the
  /// stop-requested flag so auto-reconnect doesn't immediately respawn.
  onStopDaemon: () => void;
  /// Optimistic tab-reorder hook from App.tsx. Mirrors the TabBar's
  /// same-named prop: applied immediately on drop so the row doesn't
  /// snap back while the daemon's `tabs_reordered` broadcast is in
  /// flight. Only used in the Tabs view of the sidebar; the Repos view
  /// has its own separate reorder pathway (task #11).
  onLocalReorderTabs: (orderedIds: string[]) => void;
  /// Manual sidebar-container order from App state. Empty = fall back
  /// to alphabetical (workspaces first, then repos). Drives both the
  /// `buildContainers` iteration and the drag-and-drop reorder
  /// (which sends `reorder_containers` over the WS).
  containerOrder: ContainerRef[];
  /// Optimistic container-reorder hook — mirrors `onLocalReorderTabs`
  /// but applied wholesale to `containerOrder` in App state.
  onLocalReorderContainers: (ordered: ContainerRef[]) => void;
  /// Per-container session display order from App state (daemon-backed).
  /// Key is workspace id, repo id, or tab id. Sessions not in the list
  /// are appended in their incoming order.
  sessionOrder: Map<string, string[]>;
  /// Optimistic session-order update — mirrors `onLocalReorderTabs` but
  /// keyed by container id. Called from ContainerNode on drop before the
  /// WS confirmation arrives.
  onLocalReorderSessions: (containerId: string, orderedIds: string[]) => void;
  onAddRepo: () => void;
  onAddRepoPath: (path: string) => void;
  onRemoveRepo: (id: string) => void;
  /// Called when the user removes a repo that has live sessions.
  /// The app opens a confirmation modal with a 3-way choice
  /// (cancel / remove anyway / stop sessions and remove).
  onRemoveRepoWithLiveSessions: (intent: RepoRemoveIntent) => void;
  onRemoveWorkspace: (id: string) => void;
  onSelectSession: (id: string) => void;
  onOpenSpawn: (initial?: SpawnInitialTarget) => void;
  /// Open the spawn dialog with a "land in this tab" intent armed.
  /// Driven by right-clicking a tab container in the sidebar's Tabs
  /// view → "Spawn session here". App.tsx wires the post-spawn
  /// follow-up (`addToTab`) so the freshly created session attaches
  /// to an empty pane in the target tab (or, when full, a fresh
  /// split on the right).
  onOpenSpawnIntoTab: (tabId: string) => void;
  /// Replay the last spawn config for a repo or workspace. App.tsx looks
  /// up `last_spawn_config` and either re-sends `spawn_session` (config
  /// present) or opens the spawn dialog pre-targeted to the container
  /// (config absent). The submenu in the container context menu surfaces
  /// the three placements (current / new / per-tab); double-click on a
  /// container row passes `current_tab`.
  onLaunchLast: (
    target: SpawnInitialTarget,
    placement: LaunchLastPlacement,
  ) => void;
  onEditLastSpawn: (target: SpawnInitialTarget) => void;
  /// Active main-window tab. Threaded down so the "Launch last in current
  /// tab" submenu entry can be disabled when no tab is selected.
  activeTabId: string | null;
  /// Instant duplicate from the session context menu. App-level handler
  /// arms a pending spawn intent (`newTab` or `addToTab` depending on
  /// `target`) before sending DuplicateSession, so the freshly spawned
  /// clone auto-focuses into the chosen destination instead of landing
  /// in the Unbound container.
  onDuplicateSession: (sessionId: string, target: DuplicateTarget) => void;
  /// Shift-click on a session's "Duplicate" context-menu entry. App-level
  /// handler fetches the source's SpawnConfig and opens the spawn dialog
  /// pre-filled with it.
  onDuplicateSessionWithDialog: (sessionId: string) => void;
  onOpenWorkspaceCreator: () => void;
  standaloneShellDefaultDir: string | null;
  onLaunchStandaloneShellDefault: () => void;
  onOpenStandaloneShellDialog: () => void;
  onRevealInExplorer: (path: string) => void;
  onLaunchPreset: (preset: PresetEntry, target: PresetTarget) => void;
  /// Open the Settings modal. The sidebar header button and Ctrl/Cmd+,
  /// App-level keyboard shortcut both invoke this.
  onOpenSettings: () => void;
}

type ContainerKind =
  | "workspace"
  | "repo"
  | "cwd"
  | "standalone"
  | "detached"
  | "tab"
  | "unbound";

/// Sidebar tree organization. "container" groups by workspace/repo/detached
/// (matches the daemon's registry view); "tab" groups by which tab each
/// session is currently open in. User-toggled, persisted to localStorage.
type SidebarView = "container" | "tab";

interface TreeContainer {
  key: string;
  kind: ContainerKind;
  // For "workspace" / "repo": the entity id. For "detached": "".
  id: string;
  name: string;
  // Subtitle line for repos/cwd paths — hover-only via title attribute.
  hoverTitle?: string;
  /// Filesystem path for repo containers; null for workspaces (which span
  /// multiple repos) and the detached pseudo-container.
  fsPath: string | null;
  sessions: SessionSnapshot[];
  // True iff this container can be removed from its inline action.
  removable: boolean;
}

interface ContextMenuState {
  x: number;
  y: number;
  container: TreeContainer;
}

interface AppearanceEditorState {
  container: TreeContainer;
  draft: AppearanceOverrides;
}

type VscodeWorkspaceScanState = VscodeWorkspaceSuggestion | null | "pending";

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
        : buildContainers(
            props.repos,
            props.workspaces,
            props.sessions,
            props.containerOrder,
          ),
    [
      view,
      props.tabs,
      props.repos,
      props.workspaces,
      props.sessions,
      props.containerOrder,
    ],
  );
  const cwdActionPaths = useMemo(() => {
    const seen = new Set<string>();
    const out: string[] = [];
    for (const c of containers) {
      if ((c.kind !== "cwd" && c.kind !== "standalone") || !c.fsPath) {
        continue;
      }
      const key = normalizeFsPathForCompare(c.fsPath);
      if (seen.has(key)) continue;
      seen.add(key);
      out.push(c.fsPath);
    }
    return out;
  }, [containers]);
  const [vscodeWorkspaceByPath, setVscodeWorkspaceByPath] = useState<
    Map<string, VscodeWorkspaceScanState>
  >(new Map());

  useEffect(() => {
    if (!props.client) return;
    return props.client.onMessage((msg) => {
      if (msg.type !== "vscode_workspaces_scanned") return;
      const key = normalizeFsPathForCompare(msg.path);
      setVscodeWorkspaceByPath((prev) => {
        const next = new Map(prev);
        next.set(key, msg.suggestions[0] ?? null);
        return next;
      });
    });
  }, [props.client]);

  useEffect(() => {
    if (!props.client) return;
    const missing = cwdActionPaths.filter(
      (path) => !vscodeWorkspaceByPath.has(normalizeFsPathForCompare(path)),
    );
    if (missing.length === 0) return;
    setVscodeWorkspaceByPath((prev) => {
      const next = new Map(prev);
      for (const path of missing) {
        next.set(normalizeFsPathForCompare(path), "pending");
      }
      return next;
    });
    for (const path of missing) {
      props.client.send({ type: "scan_vscode_workspaces", path });
    }
  }, [cwdActionPaths, props.client, vscodeWorkspaceByPath]);

  const abandonedCount = useMemo(
    () => props.sessions.filter((s) => s.is_abandoned).length,
    [props.sessions],
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
  const [sessionMenu, setSessionMenu] =
    useState<SessionContextMenuState | null>(null);
  const closeSessionMenu = () => setSessionMenu(null);
  const [appearanceEditor, setAppearanceEditor] =
    useState<AppearanceEditorState | null>(null);

  /// Tab-reorder drag-and-drop state, mirrored from TabBar's DragState.
  /// `text/x-rt-tab` is the shared MIME so a pill dragged from the
  /// TabBar can be dropped onto a sidebar row and vice versa. `null`
  /// while no drag is in flight.
  const [tabDragState, setTabDragState] = useState<{
    draggingTabId: string;
    overTabId: string | null;
    side: "before" | "after";
  } | null>(null);

  const onTabContainerDragStart = (tabId: string, e: React.DragEvent) => {
    e.dataTransfer.effectAllowed = "move";
    e.dataTransfer.setData("text/x-rt-tab", tabId);
    setTabDragState({ draggingTabId: tabId, overTabId: null, side: "before" });
  };

  const onTabContainerDragOver = (tabId: string, e: React.DragEvent) => {
    // Only react to in-flight tab drags (started from this sidebar OR
    // from the TabBar). Pane drags use a different MIME and should fall
    // through to the existing leaf-drop handlers.
    if (!e.dataTransfer.types.includes("text/x-rt-tab")) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = "move";
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    // Vertical layout — flip the before/after axis from horizontal
    // (TabBar) to top/bottom for the sidebar.
    const side: "before" | "after" =
      e.clientY < rect.top + rect.height / 2 ? "before" : "after";
    setTabDragState((prev) =>
      prev && (prev.overTabId !== tabId || prev.side !== side)
        ? { ...prev, overTabId: tabId, side }
        : prev,
    );
  };

  const onTabContainerDragEnd = () => setTabDragState(null);

  const onTabContainerDrop = (targetTabId: string, e: React.DragEvent) => {
    e.preventDefault();
    const payload = e.dataTransfer.getData("text/x-rt-tab");
    const local = tabDragState;
    setTabDragState(null);
    if (!payload || payload === targetTabId) return;
    const side = local?.overTabId === targetTabId ? local.side : "after";
    const order = props.tabs.map((t) => t.id);
    const fromIdx = order.indexOf(payload);
    if (fromIdx === -1) return;
    order.splice(fromIdx, 1);
    const toIdxBase = order.indexOf(targetTabId);
    if (toIdxBase === -1) return;
    const toIdx = side === "before" ? toIdxBase : toIdxBase + 1;
    order.splice(toIdx, 0, payload);
    // Optimistic local apply + send. Daemon broadcast reconciles on
    // arrival; identical orders are a no-op for the reducer.
    props.onLocalReorderTabs(order);
    props.client.send({ type: "reorder_tabs", ordered_ids: order });
  };

  /// Repo/workspace container reorder — separate state + MIME from the
  /// tab-reorder above. Wire payload format is `<kind>:<id>` so the
  /// drop handler can reconstruct a `ContainerRef` without a JSON
  /// round-trip. Drop logic mirrors the tab path: splice the dragged
  /// ref into the current order at the target's before/after position.
  const [containerDragState, setContainerDragState] = useState<{
    dragging: ContainerRef;
    over: ContainerRef | null;
    side: "before" | "after";
  } | null>(null);

  const encodeContainerRef = (ref: ContainerRef) => `${ref.kind}:${ref.id}`;
  const decodeContainerRef = (s: string): ContainerRef | null => {
    const sep = s.indexOf(":");
    if (sep === -1) return null;
    const kind = s.slice(0, sep);
    const id = s.slice(sep + 1);
    if (kind === "repo") return { kind: "repo", id };
    if (kind === "workspace") return { kind: "workspace", id };
    return null;
  };

  const onContainerDragStart = (ref: ContainerRef, e: React.DragEvent) => {
    e.dataTransfer.effectAllowed = "move";
    e.dataTransfer.setData("text/x-rt-container", encodeContainerRef(ref));
    setContainerDragState({ dragging: ref, over: null, side: "before" });
  };

  const onContainerDragOver = (ref: ContainerRef, e: React.DragEvent) => {
    if (!e.dataTransfer.types.includes("text/x-rt-container")) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = "move";
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    const side: "before" | "after" =
      e.clientY < rect.top + rect.height / 2 ? "before" : "after";
    setContainerDragState((prev) => {
      if (!prev) return prev;
      if (
        prev.over &&
        prev.over.kind === ref.kind &&
        prev.over.id === ref.id &&
        prev.side === side
      ) {
        return prev;
      }
      return { ...prev, over: ref, side };
    });
  };

  const onContainerDragEnd = () => setContainerDragState(null);

  const onContainerDrop = (targetRef: ContainerRef, e: React.DragEvent) => {
    e.preventDefault();
    const payload = e.dataTransfer.getData("text/x-rt-container");
    const local = containerDragState;
    setContainerDragState(null);
    const dragged = decodeContainerRef(payload);
    if (!dragged) return;
    if (dragged.kind === targetRef.kind && dragged.id === targetRef.id) return;
    const side =
      local?.over &&
      local.over.kind === targetRef.kind &&
      local.over.id === targetRef.id
        ? local.side
        : "after";
    // Use the rendered iteration as the source of truth for the new
    // ordering — covers both manual (containerOrder is non-empty) and
    // default (alphabetical) cases. Filter to only repo/workspace kinds
    // since those are the participants in this ordering.
    const current: ContainerRef[] = containers
      .filter((c) => c.kind === "repo" || c.kind === "workspace")
      .map((c) =>
        c.kind === "repo"
          ? ({ kind: "repo", id: c.id } as ContainerRef)
          : ({ kind: "workspace", id: c.id } as ContainerRef),
      );
    const fromIdx = current.findIndex(
      (r) => r.kind === dragged.kind && r.id === dragged.id,
    );
    if (fromIdx === -1) return;
    current.splice(fromIdx, 1);
    const toIdxBase = current.findIndex(
      (r) => r.kind === targetRef.kind && r.id === targetRef.id,
    );
    if (toIdxBase === -1) return;
    const toIdx = side === "before" ? toIdxBase : toIdxBase + 1;
    current.splice(toIdx, 0, dragged);
    props.onLocalReorderContainers(current);
    props.client.send({ type: "reorder_containers", ordered: current });
  };

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
  const addSessionDisabledReason =
    props.repos.length === 0
      ? "Register a repo to spawn repo-tied sessions."
      : null;
  const workspaceDisabledReason =
    props.repos.length < 2
      ? "Register at least 2 repos to create a workspace."
      : null;

  return (
    <aside className="sidebar" data-testid="sidebar">
      <header className="sidebar-header">
        <span className="brand">rustling-tulip</span>
        <button
          type="button"
          className="sidebar-settings-btn"
          onClick={props.onOpenSettings}
          aria-label="Open settings"
          title="Settings (Ctrl+,)"
          data-testid="sidebar-settings-btn"
        >
          <Icon name="settings" />
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
          aria-describedby={
            addSessionDisabledReason
              ? "sidebar-add-session-disabled-reason"
              : undefined
          }
          title={
            addSessionDisabledReason ?? "Spawn a new session"
          }
          data-testid="sidebar-add-session"
        >
          + Session
          {addSessionDisabledReason && (
            <span
              id="sidebar-add-session-disabled-reason"
              className="toolbar-disabled-reason"
            >
              needs repo
            </span>
          )}
        </button>
        <button
          type="button"
          className="link"
          onClick={props.onLaunchStandaloneShellDefault}
          title={`Open a standalone shell in ${
            props.standaloneShellDefaultDir ?? "the default folder"
          }`}
          data-testid="sidebar-add-shell-default"
        >
          + Shell
        </button>
        <button
          type="button"
          className="link"
          onClick={props.onOpenStandaloneShellDialog}
          title="Choose a folder for a standalone shell"
          data-testid="sidebar-add-shell-folder"
        >
          Shell...
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
          aria-describedby={
            workspaceDisabledReason
              ? "sidebar-add-workspace-disabled-reason"
              : undefined
          }
          title={
            workspaceDisabledReason ?? "Create a workspace"
          }
          data-testid="sidebar-add-workspace"
        >
          + Workspace
          {workspaceDisabledReason && (
            <span
              id="sidebar-add-workspace-disabled-reason"
              className="toolbar-disabled-reason"
            >
              needs 2 repos
            </span>
          )}
        </button>
        {abandonedCount >= 2 && (
          <button
            type="button"
            className="link"
            onClick={() => props.client.send({ type: "resume_all_abandoned" })}
            title={`Resume all ${abandonedCount} abandoned sessions`}
            data-testid="sidebar-resume-all-abandoned"
          >
            Resume all ({abandonedCount})
          </button>
        )}
      </div>

      {containers.length === 0 ? (
        <p className="empty">
          No repos or workspaces yet — start with{" "}
          <button type="button" className="link inline" onClick={props.onAddRepo}>
            + Repo
          </button>
          .
        </p>
      ) : (
        <ul className="tree">
          {containers.map((c) => {
            const isCollapsed = collapsed.has(c.key);
            // Tab and repo/workspace containers participate in DIFFERENT
            // drag-reorder paths (different MIMEs, different protocol
            // messages, different state buckets). Compute both side and
            // dragging flags and pass whichever applies — the row
            // renders a single drop-indicator either way.
            const isTabKind = c.kind === "tab";
            const isReorderableContainer =
              c.kind === "repo" || c.kind === "workspace";

            let dropSide: "before" | "after" | null = null;
            let isDragging = false;
            let dragHandlers:
              | {
                  onDragStart: (e: React.DragEvent) => void;
                  onDragOver: (e: React.DragEvent) => void;
                  onDragEnd: () => void;
                  onDrop: (e: React.DragEvent) => void;
                }
              | undefined;

            if (isTabKind) {
              dropSide =
                tabDragState?.overTabId === c.id ? tabDragState.side : null;
              isDragging = tabDragState?.draggingTabId === c.id;
              dragHandlers = {
                onDragStart: (e) => onTabContainerDragStart(c.id, e),
                onDragOver: (e) => onTabContainerDragOver(c.id, e),
                onDragEnd: onTabContainerDragEnd,
                onDrop: (e) => onTabContainerDrop(c.id, e),
              };
            } else if (isReorderableContainer) {
              const ref: ContainerRef =
                c.kind === "repo"
                  ? { kind: "repo", id: c.id }
                  : { kind: "workspace", id: c.id };
              const over = containerDragState?.over;
              dropSide =
                over && over.kind === ref.kind && over.id === ref.id
                  ? containerDragState.side
                  : null;
              isDragging =
                containerDragState?.dragging.kind === ref.kind &&
                containerDragState?.dragging.id === ref.id;
              dragHandlers = {
                onDragStart: (e) => onContainerDragStart(ref, e),
                onDragOver: (e) => onContainerDragOver(ref, e),
                onDragEnd: onContainerDragEnd,
                onDrop: (e) => onContainerDrop(ref, e),
              };
            }
            const lastSpawnConfig = lastSpawnConfigFor(
              c,
              props.repos,
              props.workspaces,
            );
            const vscodeWorkspace = c.fsPath
              ? vscodeWorkspaceByPath.get(normalizeFsPathForCompare(c.fsPath))
              : null;

            return (
              <ContainerNode
                key={c.key}
                container={c}
                collapsed={isCollapsed}
                hasAttention={containersWithAttention.has(c.key)}
                onToggle={() => toggle(c.key)}
                tabs={props.tabs}
                client={props.client}
                repos={props.repos}
                workspaces={props.workspaces}
                settings={settings}
                highlightedSessionIds={props.highlightedSessionIds}
                attentionSessions={props.attentionSessions}
                onSelectSession={props.onSelectSession}
                onAddRepoPath={props.onAddRepoPath}
                vscodeWorkspaceSuggestion={
                  vscodeWorkspace && vscodeWorkspace !== "pending"
                    ? vscodeWorkspace
                    : null
                }
                onRemoveRepo={props.onRemoveRepo}
                onRemoveRepoWithLiveSessions={props.onRemoveRepoWithLiveSessions}
                onRemoveWorkspace={props.onRemoveWorkspace}
                onLaunchLast={props.onLaunchLast}
                lastSpawnSummary={spawnConfigSummary(lastSpawnConfig)}
                onContextMenu={(x, y) =>
                  setContextMenu({ x, y, container: c })
                }
                onSessionContextMenu={(x, y, session) =>
                  setSessionMenu({ x, y, session })
                }
                dropSide={dropSide}
                isDragging={isDragging}
                dragHandlers={dragHandlers}
                sessionOrder={props.sessionOrder.get(c.id)}
                onReorderSessions={(ids) => {
                  // Only workspace/repo/tab containers have daemon-backed
                  // session-order buckets. Cwd containers are transient.
                  if (
                    c.kind === "workspace" ||
                    c.kind === "repo" ||
                    c.kind === "tab"
                  ) {
                    props.onLocalReorderSessions(c.id, ids);
                    props.client.send({
                      type: "reorder_sessions",
                      container_id: c.id,
                      ordered_ids: ids,
                    });
                  }
                }}
              />
            );
          })}
        </ul>
      )}

      {contextMenu && (
        <ContainerContextMenu
          state={contextMenu}
          presets={currentPresets}
          tabs={props.tabs}
          activeTabId={props.activeTabId}
          lastSpawnSummary={spawnConfigSummary(
            lastSpawnConfigFor(
              contextMenu.container,
              props.repos,
              props.workspaces,
            ),
          )}
          onLaunchLast={(placement) => {
            const target = containerLaunchTarget(contextMenu.container);
            if (target) props.onLaunchLast(target, placement);
            closeMenu();
          }}
          onEditLastSpawn={() => {
            const target = containerLaunchTarget(contextMenu.container);
            if (target) props.onEditLastSpawn(target);
            closeMenu();
          }}
          onAppearance={() => {
            const container = contextMenu.container;
            const draft = appearanceForContainer(
              container,
              props.repos,
              props.workspaces,
            );
            if (draft) {
              setAppearanceEditor({ container, draft });
            }
            closeMenu();
          }}
          onClose={closeMenu}
          onSpawn={() => {
            const c = contextMenu.container;
            if (c.kind === "repo") {
              props.onOpenSpawn({ kind: "repo", repo_id: c.id });
            } else if (c.kind === "workspace") {
              props.onOpenSpawn({ kind: "workspace", workspace_id: c.id });
            } else if (c.kind === "tab") {
              props.onOpenSpawnIntoTab(c.id);
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
            if (path) void copyToClipboard(path, "path").catch(() => {});
            closeMenu();
          }}
          onLaunchPreset={(preset) => {
            const target = menuTarget(contextMenu.container);
            if (target) props.onLaunchPreset(preset, target);
            closeMenu();
          }}
        />
      )}

      {appearanceEditor && (
        <AppearanceModal
          title={`${appearanceEditor.container.name} appearance`}
          value={appearanceEditor.draft}
          inherited={resolveAppearanceLayers(null, null, settings.appearance)}
          inheritLabel="Inherit app"
          onChange={(appearance) => {
            setAppearanceEditor((current) =>
              current ? { ...current, draft: appearance } : current,
            );
            if (appearanceEditor.container.kind === "repo") {
              props.client.send({
                type: "set_repo_appearance",
                repo_id: appearanceEditor.container.id,
                appearance,
              });
            } else if (appearanceEditor.container.kind === "workspace") {
              props.client.send({
                type: "set_workspace_appearance",
                workspace_id: appearanceEditor.container.id,
                appearance,
              });
            }
          }}
          onClose={() => setAppearanceEditor(null)}
        />
      )}

      {sessionMenu && (
        <SessionContextMenu
          state={sessionMenu}
          tabs={props.tabs}
          repos={props.repos}
          workspaces={props.workspaces}
          client={props.client}
          onClose={closeSessionMenu}
          onDuplicate={(withDialog, target) => {
            const sid = sessionMenu.session.id;
            if (withDialog) {
              props.onDuplicateSessionWithDialog(sid);
            } else {
              props.onDuplicateSession(sid, target);
            }
            closeSessionMenu();
          }}
        />
      )}

      <DaemonFooter
        connection={props.connection}
        handshake={props.handshake}
        sessionCount={props.sessions.length}
        stopRequested={props.daemonStopRequested}
        onRestart={props.onRestartDaemon}
        onStop={props.onStopDaemon}
      />
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
  repos: RepoEntry[];
  workspaces: WorkspaceEntry[];
  settings: Settings;
  highlightedSessionIds: Set<string>;
  attentionSessions: Set<string>;
  onSelectSession: (id: string) => void;
  onAddRepoPath: (path: string) => void;
  vscodeWorkspaceSuggestion: VscodeWorkspaceSuggestion | null;
  onRemoveRepo: (id: string) => void;
  onRemoveRepoWithLiveSessions: (intent: RepoRemoveIntent) => void;
  onRemoveWorkspace: (id: string) => void;
  /// Double-click on a repo/workspace row replays its `last_spawn_config`
  /// in the active tab. Threaded down so the container row's onDoubleClick
  /// can fire it without touching parent state.
  onLaunchLast: (
    target: SpawnInitialTarget,
    placement: LaunchLastPlacement,
  ) => void;
  lastSpawnSummary: string | null;
  onContextMenu: (x: number, y: number) => void;
  /// Threads through to each SessionLeaf so a right-click on a session
  /// opens the per-session context menu (Duplicate, …).
  onSessionContextMenu: (
    x: number,
    y: number,
    session: SessionSnapshot,
  ) => void;
  /// Drop-side hint for whichever drag this row participates in (tab
  /// reorder OR repo/workspace container reorder — they're mutually
  /// exclusive per row). Drives the `.drop-before` / `.drop-after`
  /// indicator. `null` when this row isn't the current drop target.
  dropSide: "before" | "after" | null;
  /// True iff this row is the active drag source (renders at reduced
  /// opacity so the user sees the original row "lift").
  isDragging: boolean;
  /// Drag handlers wired only for reorderable kinds (tab, repo,
  /// workspace). `undefined` for detached/unbound rows — those are
  /// passive groupings, not reorderable resources.
  dragHandlers:
    | {
        onDragStart: (e: React.DragEvent) => void;
        onDragOver: (e: React.DragEvent) => void;
        onDragEnd: () => void;
        onDrop: (e: React.DragEvent) => void;
      }
    | undefined;
  /// Local-UI session order override for this container. `undefined`
  /// means "not set yet, use incoming alpha order". The array contains
  /// session IDs in the user-dragged sequence; sessions not in the list
  /// (newly spawned) are appended in their original incoming order.
  sessionOrder: string[] | undefined;
  onReorderSessions: (orderedIds: string[]) => void;
}

function ContainerNode(p: ContainerNodeProps) {
  const c = p.container;
  const hasChildren = c.sessions.length > 0;
  const headerClasses = [
    "tree-row",
    "tree-container",
    `tree-container-${c.kind}`,
    p.dropSide === "before" ? "drop-before" : "",
    p.dropSide === "after" ? "drop-after" : "",
    p.isDragging ? "is-dragging" : "",
  ]
    .filter(Boolean)
    .join(" ");

  /// Inline two-state confirm for workspace remove and repo remove without
  /// live sessions. The repo-with-live-sessions case bypasses this and
  /// opens a modal directly, since the user needs the 3-way choice
  /// (cancel / remove anyway / stop sessions and remove).
  const [confirming, setConfirming] = useState(false);
  const [confirmingStopAll, setConfirmingStopAll] = useState(false);

  const onStopAllDetached = () => {
    if (!confirmingStopAll) {
      setConfirmingStopAll(true);
      return;
    }
    setConfirmingStopAll(false);
    for (const s of c.sessions) {
      if (s.status !== "stopped") {
        p.client.send({
          type: "stop_session",
          session_id: s.id,
          cleanup: s.members.map((m) => ({
            repo_id: m.repo_id,
            remove_worktree: false,
          })),
        });
      }
    }
  };

  const [leafDragState, setLeafDragState] = useState<{
    draggingId: string;
    overId: string | null;
    side: "before" | "after";
  } | null>(null);

  const displayedSessions = useMemo(
    () => applySessionOrder(c.sessions, p.sessionOrder),
    [c.sessions, p.sessionOrder],
  );

  const onLeafDragStart = (sessionId: string) => {
    setLeafDragState({ draggingId: sessionId, overId: null, side: "before" });
  };

  const onLeafDragOver = (sessionId: string, e: React.DragEvent) => {
    if (!e.dataTransfer.types.includes("text/x-rt-session")) return;
    e.preventDefault();
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    const side: "before" | "after" =
      e.clientY < rect.top + rect.height / 2 ? "before" : "after";
    setLeafDragState((prev) =>
      prev && (prev.overId !== sessionId || prev.side !== side)
        ? { ...prev, overId: sessionId, side }
        : prev,
    );
  };

  const onLeafDragEnd = () => setLeafDragState(null);

  const onLeafDrop = (targetId: string, e: React.DragEvent) => {
    e.preventDefault();
    const draggingId = e.dataTransfer.getData("text/x-rt-session");
    const local = leafDragState;
    setLeafDragState(null);
    if (!draggingId || draggingId === targetId) return;
    const side = local?.overId === targetId ? local.side : "after";
    const currentIds = displayedSessions.map((s) => s.id);
    const fromIdx = currentIds.indexOf(draggingId);
    if (fromIdx === -1) return;
    currentIds.splice(fromIdx, 1);
    const toIdxBase = currentIds.indexOf(targetId);
    if (toIdxBase === -1) return;
    const toIdx = side === "before" ? toIdxBase : toIdxBase + 1;
    currentIds.splice(toIdx, 0, draggingId);
    p.onReorderSessions(currentIds);
  };

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
    // Detached and unbound containers remain read-only groupings.
    // Workspace / repo containers get the full menu (spawn, presets,
    // reveal, remove). Tab containers get a minimal menu — currently
    // just "Spawn session here" so the new session lands in this tab.
    if (c.kind !== "workspace" && c.kind !== "repo" && c.kind !== "tab") {
      return;
    }
    e.preventDefault();
    p.onContextMenu(e.clientX, e.clientY);
  };

  /// Double-click on a repo or workspace row replays its last spawn
  /// config in the current tab. Falls through to the spawn dialog when
  /// the container has never been launched — App.tsx owns that branch.
  /// Tab/detached/unbound containers ignore double-click.
  const onDoubleClick = () => {
    const target = launchLastTargetFor(c);
    if (!target) return;
    p.onLaunchLast(target, { kind: "current_tab" });
  };

  const onQuickLaunchLast = (e: React.MouseEvent) => {
    e.stopPropagation();
    const target = launchLastTargetFor(c);
    if (!target) return;
    p.onLaunchLast(target, { kind: "current_tab" });
  };

  // Tab + repo/workspace containers participate in drag-to-reorder
  // (different MIMEs, but same `dragHandlers` shape). Detached/unbound
  // pass undefined handlers and stay non-draggable.
  const isDraggable = p.dragHandlers !== undefined;
  const kindLabel =
    c.kind === "workspace"
      ? "WS"
      : c.kind === "repo"
        ? "REPO"
        : c.kind === "tab"
          ? "TAB"
          : c.kind === "cwd"
            ? "DIR"
          : c.kind === "standalone"
            ? "SH"
            : c.kind === "unbound"
              ? "UNB"
              : "?";

  return (
    <li>
      <div
        className={headerClasses}
        onClick={hasChildren ? p.onToggle : undefined}
        onDoubleClick={onDoubleClick}
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
        data-container-path={c.fsPath ?? undefined}
        draggable={isDraggable}
        onDragStart={p.dragHandlers?.onDragStart}
        onDragOver={p.dragHandlers?.onDragOver}
        onDragEnd={p.dragHandlers?.onDragEnd}
        onDrop={p.dragHandlers?.onDrop}
      >
        <div className="tree-container-primary">
          {hasChildren ? (
            <button
              type="button"
              className="tree-toggle-chip"
              title={p.collapsed ? `Expand ${c.name}` : `Collapse ${c.name}`}
              aria-label={p.collapsed ? `Expand ${c.name}` : `Collapse ${c.name}`}
              aria-expanded={!p.collapsed}
              data-testid="tree-toggle-chip"
              onClick={(e) => {
                e.stopPropagation();
                p.onToggle();
              }}
            >
              {p.collapsed ? "▸" : "▾"}
            </button>
          ) : (
            <span className="tree-toggle-chip-placeholder" aria-hidden="true" />
          )}
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
          {p.lastSpawnSummary && (
            <button
              type="button"
              className="list-item-action tree-launch-action"
              title={`Launch last in current tab: ${p.lastSpawnSummary}`}
              aria-label={`Launch last for ${c.name}`}
              data-testid="container-launch-last-quick"
              onClick={onQuickLaunchLast}
            >
              <Icon name="play" />
            </button>
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
                <Icon name={confirming ? "confirm" : "close"} />
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
                  <Icon name="close" />
                </button>
              )}
            </>
          )}
          {c.kind === "detached" &&
            c.sessions.some((s) => s.status !== "stopped") && (
              <>
                <button
                  type="button"
                  className={
                    confirmingStopAll
                      ? "list-item-action danger"
                      : "list-item-action"
                  }
                  title={
                    confirmingStopAll
                      ? "Click again to stop all detached sessions"
                      : "Stop all detached sessions"
                  }
                  aria-label={
                    confirmingStopAll
                      ? "Confirm stop all detached sessions"
                      : "Stop all detached sessions"
                  }
                  data-testid="sidebar-stop-all-detached"
                  data-confirming={confirmingStopAll ? "true" : "false"}
                  onClick={(e) => {
                    e.stopPropagation();
                    onStopAllDetached();
                  }}
                >
                  <Icon name={confirmingStopAll ? "confirm" : "stop"} />
                </button>
                {confirmingStopAll && (
                  <button
                    type="button"
                    className="list-item-action"
                    title="Cancel"
                    aria-label="Cancel stop all"
                    data-testid="sidebar-stop-all-detached-cancel"
                    onClick={(e) => {
                      e.stopPropagation();
                      setConfirmingStopAll(false);
                    }}
                  >
                    <Icon name="close" />
                  </button>
                )}
              </>
            )}
          {(c.kind === "cwd" || c.kind === "standalone") && c.fsPath && (
            <>
              <button
                type="button"
                className="list-item-action"
                title={`Add repo: ${c.fsPath}`}
                aria-label={`Add repo ${c.name}`}
                data-testid="sidebar-cwd-add-repo"
                onClick={(e) => {
                  e.stopPropagation();
                  p.onAddRepoPath(c.fsPath!);
                }}
              >
                <Icon name="repo" />
              </button>
              {p.vscodeWorkspaceSuggestion && (
                <button
                  type="button"
                  className="list-item-action"
                  title={`Add workspace: ${p.vscodeWorkspaceSuggestion.suggested_name}`}
                  aria-label={`Add workspace ${p.vscodeWorkspaceSuggestion.suggested_name}`}
                  data-testid="sidebar-cwd-add-workspace"
                  onClick={(e) => {
                    e.stopPropagation();
                    p.client.send({
                      type: "accept_vscode_workspace_suggestion",
                      suggestion: p.vscodeWorkspaceSuggestion!,
                      watch: false,
                    });
                  }}
                >
                  <Icon name="workspace" />
                </button>
              )}
            </>
          )}
        </div>
        <div className="tree-container-meta">
          <span className="tree-kind-tag">{kindLabel}</span>
          {p.lastSpawnSummary && (
            <span
              className="tree-launch-summary"
              title={`Last launch: ${p.lastSpawnSummary}`}
              data-testid="container-launch-summary"
            >
              {p.lastSpawnSummary}
            </span>
          )}
        </div>
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
          {(c.kind === "cwd" || c.kind === "standalone") && c.fsPath && (
            <li
              className="tree-children-banner cwd-banner"
              data-testid={c.kind === "standalone" ? "standalone-cwd-banner" : "cwd-banner"}
            >
              <span>{c.fsPath}</span>
              <span className="tree-children-banner-actions">
                <button
                  type="button"
                  className="link inline"
                  data-testid="cwd-banner-add-repo"
                  onClick={() => p.onAddRepoPath(c.fsPath!)}
                >
                  Add repo
                </button>
                {p.vscodeWorkspaceSuggestion && (
                  <button
                    type="button"
                    className="link inline"
                    data-testid="cwd-banner-add-workspace"
                    onClick={() =>
                      p.client.send({
                        type: "accept_vscode_workspace_suggestion",
                        suggestion: p.vscodeWorkspaceSuggestion!,
                        watch: false,
                      })}
                  >
                    Add workspace
                  </button>
                )}
              </span>
            </li>
          )}
          {displayedSessions.map((s) => (
            <SessionLeaf
              key={s.id}
              session={s}
              tabs={p.tabs}
              client={p.client}
              repos={p.repos}
              workspaces={p.workspaces}
              settings={p.settings}
              selected={p.highlightedSessionIds.has(s.id)}
              needsAttention={p.attentionSessions.has(s.id)}
              onSelect={p.onSelectSession}
              onContextMenu={p.onSessionContextMenu}
              isDraggingForReorder={leafDragState?.draggingId === s.id}
              reorderDropSide={
                leafDragState?.overId === s.id ? leafDragState.side : null
              }
              onReorderDragStart={() => onLeafDragStart(s.id)}
              onReorderDragOver={(e) => onLeafDragOver(s.id, e)}
              onReorderDragEnd={onLeafDragEnd}
              onReorderDrop={(e) => onLeafDrop(s.id, e)}
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
  repos: RepoEntry[];
  workspaces: WorkspaceEntry[];
  settings: Settings;
  selected: boolean;
  needsAttention: boolean;
  onSelect: (id: string) => void;
  /// Right-clicking the leaf opens the session context menu (Duplicate
  /// for now; more entries will land here). State is lifted to the
  /// Sidebar component so only one menu is open at a time.
  onContextMenu: (x: number, y: number, session: SessionSnapshot) => void;
  /// True while this leaf is the active drag source for list reorder
  /// (reduces opacity so the original row "lifts").
  isDraggingForReorder: boolean;
  /// Drop-side hint when this leaf is the current reorder drop target.
  reorderDropSide: "before" | "after" | null;
  onReorderDragStart: () => void;
  onReorderDragOver: (e: React.DragEvent) => void;
  onReorderDragEnd: () => void;
  onReorderDrop: (e: React.DragEvent) => void;
}

function SessionLeaf(p: SessionLeafProps) {
  const s = p.session;
  const isPlainShell = s.mode === "plain_shell";
  const isCodex = !isPlainShell && s.agent === "codex";
  const accentColor = resolveAppearance(
    p.settings,
    p.repos,
    p.workspaces,
    s,
  ).values.accent_color;
  const bindings = useMemo(
    () => sessionTabBindings(s.id, p.tabs),
    [s.id, p.tabs],
  );
  // Pane-drop drag: only when the session is bound to a tab pane.
  const paneDraggable = bindings.length > 0 && !s.is_orphan;
  const primaryBinding = bindings[0] ?? null;
  const onDragStart = (e: React.DragEvent) => {
    e.dataTransfer.effectAllowed = "move";
    // Always set the session-reorder MIME (list reorder works for any session).
    e.dataTransfer.setData("text/x-rt-session", s.id);
    // Also set the pane-drop MIME when bound to a tab pane.
    if (primaryBinding) {
      e.dataTransfer.setData(
        DRAG_MIME,
        `${primaryBinding.tab_id}:${primaryBinding.pane_id}`,
      );
    }
    p.onReorderDragStart();
  };
  const onContextMenu = (e: React.MouseEvent) => {
    e.preventDefault();
    p.onContextMenu(e.clientX, e.clientY, s);
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
    s.is_inactive ? "is-inactive" : "",
    isPlainShell ? "is-shell" : "",
    isCodex ? "is-codex" : "",
    accentColor ? "has-session-accent" : "",
    s.elevated_authority ? "has-elevated-authority" : "",
    paneDraggable ? "is-draggable" : "",
    p.isDraggingForReorder ? "is-dragging" : "",
    p.reorderDropSide === "before" ? "drop-before" : "",
    p.reorderDropSide === "after" ? "drop-after" : "",
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <li>
      <div
        className={classes}
        style={sessionAccentStyle(accentColor)}
        onClick={() => p.onSelect(s.id)}
        role="button"
        tabIndex={0}
        aria-label={`Session ${sessionDisplayLabel(s)}`}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            p.onSelect(s.id);
          }
        }}
        draggable={true}
        onDragStart={onDragStart}
        onDragOver={p.onReorderDragOver}
        onDragEnd={p.onReorderDragEnd}
        onDrop={p.onReorderDrop}
        onContextMenu={onContextMenu}
        data-testid="sidebar-session"
        data-session-id={s.id}
        data-session-status={s.status}
        data-session-agent={s.agent}
        data-session-color={accentColor ?? ""}
        data-session-elevated={s.elevated_authority ? "true" : "false"}
        data-tab-binding-count={bindings.length}
      >
        <span
          className={`status-dot status-${s.status}`}
          title={`status: ${s.status}`}
          aria-label={`status ${s.status}`}
          role="img"
        />
        <span
          className="tree-label"
          title={sessionLabelTooltip(s)}
        >
          {sessionDisplayLabel(s)}
        </span>
        {(() => {
          const runtime = sessionRuntimeLabel(s);
          if (!runtime) return null;
          const title = s.elevated_authority
            ? `Running ${runtime}; approval prompts were bypassed`
            : `Running ${runtime}`;
          return (
            <span
              className="tree-kind-tag"
              title={title}
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
        {s.is_abandoned && (
          <span
            className="list-item-meta"
            title={
              s.last_prompt
                ? `Daemon crashed mid-run. Last prompt:\n${s.last_prompt}`
                : "Daemon crashed mid-run"
            }
          >
            abandoned
          </span>
        )}
        {s.is_abandoned && (
          <button
            type="button"
            className="btn-inline"
            onClick={(e) => {
              e.stopPropagation();
              p.client.send({ type: "resume_abandoned", session_id: s.id });
            }}
            title="Spawn a fresh session from the captured config and replay the prompt"
            data-testid="sidebar-session-resume"
          >
            Resume
          </button>
        )}
        {s.is_abandoned && (
          <button
            type="button"
            className="btn-inline btn-quiet"
            onClick={(e) => {
              e.stopPropagation();
              p.client.send({ type: "discard_abandoned", session_id: s.id });
            }}
            title="Dismiss this abandoned session without resuming"
            data-testid="sidebar-session-discard"
          >
            Dismiss
          </button>
        )}
        {s.is_inactive && (
          <span
            className="list-item-meta"
            title={
              s.has_per_session_worktree
                ? `Parked. Worktree kept on disk:\n${s.worktree_paths.join("\n")}`
                : "Parked"
            }
          >
            inactive
          </span>
        )}
        {s.is_inactive && (
          <button
            type="button"
            className="btn-inline"
            onClick={(e) => {
              e.stopPropagation();
              window.dispatchEvent(
                new CustomEvent("rt:pane_session_restart", {
                  detail: { sessionId: s.id, tabId: null },
                }),
              );
            }}
            title="Spawn a fresh session that reuses this worktree"
            data-testid="sidebar-session-resume-inactive"
          >
            Resume
          </button>
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
  tabs: TabEntry[];
  activeTabId: string | null;
  lastSpawnSummary: string | null;
  onLaunchLast: (placement: LaunchLastPlacement) => void;
  onEditLastSpawn: () => void;
  onAppearance: () => void;
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
  const canLaunchLast = c.kind === "repo" || c.kind === "workspace";
  const canCustomizeAppearance = c.kind === "repo" || c.kind === "workspace";
  const isTab = c.kind === "tab";
  const removeLabel =
    c.kind === "workspace" ? "Remove workspace" : "Remove repo";
  const activeTab = p.activeTabId
    ? p.tabs.find((t) => t.id === p.activeTabId)
    : null;
  const hasLastConfig = p.lastSpawnSummary !== null;
  const lastConfigDisabledTitle = hasLastConfig
    ? undefined
    : "No saved spawn config — launch the spawn dialog at least once first.";
  useEscape(p.onClose);

  // Clamp the menu against the window so a right-click near the
  // bottom-right corner doesn't render the menu beyond the viewport.
  // Estimates: menus average ~200px wide, ~300px tall (presets push higher
  // — overlap-safe, viewport-stable beats pixel-perfect). The 12px margin
  // keeps the menu off the edge.
  const clampedLeft = clampMenuCoord(p.state.x, 200);
  const clampedTop = clampMenuCoord(p.state.y, isTab ? 60 : 400, "height");
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
            {isTab ? "Spawn session here" : "Spawn new session"}
          </button>
        </li>
        {canLaunchLast && (
          <>
            <li className="context-menu-separator" aria-hidden="true" />
            <li className="context-menu-label">Launch last again</li>
            {p.lastSpawnSummary && (
              <li
                className="context-menu-summary"
                title={p.lastSpawnSummary}
                data-testid="container-launch-last-summary"
              >
                {p.lastSpawnSummary}
              </li>
            )}
            <li>
              <button
                type="button"
                disabled={!hasLastConfig || !activeTab}
                onClick={() => p.onLaunchLast({ kind: "current_tab" })}
                title={
                  lastConfigDisabledTitle ??
                  (!activeTab
                    ? "No active tab — nothing to launch into."
                    : `Replay last spawn into "${activeTab.name}"`)
                }
                data-testid="container-launch-last-current"
              >
                &nbsp;&nbsp;In current tab
                {activeTab && (
                  <span className="context-menu-hint">{activeTab.name}</span>
                )}
              </button>
            </li>
            <li>
              <button
                type="button"
                disabled={!hasLastConfig}
                onClick={() => p.onLaunchLast({ kind: "new_tab" })}
                title={
                  lastConfigDisabledTitle ?? "Replay last spawn in a fresh tab"
                }
                data-testid="container-launch-last-new"
              >
                &nbsp;&nbsp;In new tab
              </button>
            </li>
            {p.tabs.length > 0 && (
              <>
                <li className="context-menu-label">In tab</li>
                {p.tabs.map((t) => (
                  <li key={`launch-last:${t.id}`}>
                    <button
                      type="button"
                      disabled={!hasLastConfig}
                      onClick={() =>
                        p.onLaunchLast({ kind: "tab", tabId: t.id })
                      }
                      title={
                        lastConfigDisabledTitle ??
                        `Replay last spawn into "${t.name}"`
                      }
                    >
                      &nbsp;&nbsp;{t.name}
                    </button>
                  </li>
                ))}
              </>
            )}
            <li>
              <button
                type="button"
                disabled={!hasLastConfig}
                onClick={p.onEditLastSpawn}
                title={
                  lastConfigDisabledTitle ??
                  "Open the full spawn dialog with this config pre-filled"
                }
                data-testid="container-launch-last-edit"
              >
                &nbsp;&nbsp;Edit before launch
              </button>
            </li>
          </>
        )}
        {/* Tab containers don't expose preset / reveal / remove — they're
            view groupings, not resources to manage. Keep the menu small. */}
        {!isTab && (
          <>
            <li className="context-menu-separator" aria-hidden="true" />
            {canCustomizeAppearance && (
              <li>
                <button
                  type="button"
                  onClick={p.onAppearance}
                  data-testid="container-context-appearance"
                >
                  Appearance...
                </button>
              </li>
            )}
            {canLaunchPreset && (
              <PresetSubmenu
                presets={p.presets}
                onLaunch={p.onLaunchPreset}
              />
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
          </>
        )}
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

function appearanceForContainer(
  c: TreeContainer,
  repos: RepoEntry[],
  workspaces: WorkspaceEntry[],
): AppearanceOverrides | null {
  if (c.kind === "repo") {
    return repos.find((repo) => repo.id === c.id)?.appearance ?? null;
  }
  if (c.kind === "workspace") {
    return (
      workspaces.find((workspace) => workspace.id === c.id)?.appearance ?? null
    );
  }
  return null;
}

/// Build a `SpawnInitialTarget` from a sidebar container that can host
/// a launch. Returns `null` for tab/detached/unbound rows — those aren't
/// launch sources. Shared by the double-click handler and the context-
/// menu's submenu.
function launchLastTargetFor(c: TreeContainer): SpawnInitialTarget | null {
  if (c.kind === "repo") return { kind: "repo", repo_id: c.id };
  if (c.kind === "workspace")
    return { kind: "workspace", workspace_id: c.id };
  return null;
}

/// Sidebar-render alias for `launchLastTargetFor`. Kept under a distinct
/// name at the menu call site to make the intent explicit (we're building
/// the relaunch target for the *currently right-clicked* container).
function containerLaunchTarget(c: TreeContainer): SpawnInitialTarget | null {
  return launchLastTargetFor(c);
}

function lastSpawnConfigFor(
  c: TreeContainer,
  repos: RepoEntry[],
  workspaces: WorkspaceEntry[],
): SpawnConfig | null {
  if (c.kind === "repo") {
    return repos.find((r) => r.id === c.id)?.last_spawn_config ?? null;
  }
  if (c.kind === "workspace") {
    return workspaces.find((w) => w.id === c.id)?.last_spawn_config ?? null;
  }
  return null;
}

function spawnConfigSummary(config: SpawnConfig | null): string | null {
  if (!config) return null;
  const parts = [
    config.agent === "claude" ? "Claude" : "Codex",
    sessionModeLabel(config.mode),
  ];
  const target = spawnTargetSummary(config);
  if (target) parts.push(target);
  if (config.model) parts.push(config.model);
  if (!config.dangerously_skip_permissions) {
    const authority = authoritySettingSummary(config);
    if (authority) parts.push(authority);
  }
  if (config.dangerously_skip_permissions) parts.push("trusted");
  if (config.extra_env.length > 0) parts.push(`${config.extra_env.length} env`);
  return parts.join(" · ");
}

function sessionModeLabel(mode: SpawnConfig["mode"]): string {
  if (mode === "plain_shell") return "shell";
  return mode;
}

function spawnTargetSummary(config: SpawnConfig): string | null {
  const target = config.target;
  if (target.kind === "standalone") return "standalone shell";
  if (target.use_worktree) {
    const base = target.base_branch ?? target.branch_name;
    return `new worktree from ${base}`;
  }
  return target.branch_name;
}

function authoritySettingSummary(config: SpawnConfig): string | null {
  if (config.agent === "claude") {
    if (config.permission_mode === "accept_edits") return "accept edits";
    if (config.permission_mode === "bypass_permissions") return "bypass permissions";
    return config.permission_mode;
  }
  return config.codex_sandbox;
}

function targetCacheKey(target: PresetTarget): string {
  return target.kind === "repo"
    ? `repo:${target.repo_id}`
    : `workspace:${target.workspace_id}`;
}

/// Merge `sessions` with `order` (an array of session IDs). Sessions present
/// in `order` come first in that sequence; any session not in `order`
/// (e.g. newly spawned since the last drag) is appended in its original
/// position. Stale ids in `order` (removed sessions) are silently dropped.
function applySessionOrder(
  sessions: SessionSnapshot[],
  order: string[] | undefined,
): SessionSnapshot[] {
  if (!order || order.length === 0) return sessions;
  const byId = new Map(sessions.map((s) => [s.id, s]));
  const result: SessionSnapshot[] = [];
  const seen = new Set<string>();
  for (const id of order) {
    const s = byId.get(id);
    if (s) {
      result.push(s);
      seen.add(id);
    }
  }
  for (const s of sessions) {
    if (!seen.has(s.id)) result.push(s);
  }
  return result;
}

function containerRefMatches(ref: ContainerRef, c: TreeContainer): boolean {
  if (ref.kind === "repo") return c.kind === "repo" && c.id === ref.id;
  return c.kind === "workspace" && c.id === ref.id;
}

function applyContainerOrder(
  workspaces: TreeContainer[],
  repos: TreeContainer[],
  order: ContainerRef[],
): TreeContainer[] {
  if (order.length === 0) {
    // No manual order set: default to workspaces (alpha) then repos
    // (alpha) — matches the pre-task-#11 behavior so a fresh install
    // doesn't look any different until the user actively reorders.
    return [...workspaces, ...repos];
  }
  // Build the output in two passes: known ordering first, then anything
  // the order list doesn't mention (e.g., a repo registered after the
  // last reorder — keeps it visible without forcing the user to
  // re-drag).
  const seen = new Set<string>();
  const out: TreeContainer[] = [];
  for (const ref of order) {
    const key = `${ref.kind}:${ref.id}`;
    if (seen.has(key)) continue;
    const all = ref.kind === "repo" ? repos : workspaces;
    const found = all.find((c) => containerRefMatches(ref, c));
    if (found) {
      seen.add(key);
      out.push(found);
    }
  }
  for (const w of workspaces) {
    if (!seen.has(`workspace:${w.id}`)) out.push(w);
  }
  for (const r of repos) {
    if (!seen.has(`repo:${r.id}`)) out.push(r);
  }
  return out;
}

function buildContainers(
  repos: RepoEntry[],
  workspaces: WorkspaceEntry[],
  sessions: SessionSnapshot[],
  containerOrder: ContainerRef[],
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
  const cwdSessions = new Map<string, SessionSnapshot[]>();
  const standaloneContainers: TreeContainer[] = [];
  const detached: SessionSnapshot[] = [];

  for (const s of sessions) {
    if (s.kind === "standalone") {
      const cwd = s.current_cwd ? normalizeFsPath(s.current_cwd) : null;
      standaloneContainers.push({
        key: `standalone:${s.id}`,
        kind: "standalone",
        id: s.id,
        name: sessionDisplayLabel(s),
        hoverTitle: cwd ?? "Shell launched outside registered repos and workspaces",
        fsPath: cwd,
        sessions: [s],
        removable: false,
      });
    } else if (s.mode === "plain_shell" && s.current_cwd) {
      const cwdHome = findContainerForCwd(
        s.current_cwd,
        repos,
        workspaces,
        memberRepoIds,
      );
      if (cwdHome.kind === "workspace") {
        pushTo(wsSessions, cwdHome.id, s);
      } else if (cwdHome.kind === "repo") {
        pushTo(repoSessions, cwdHome.id, s);
      } else {
        pushTo(cwdSessions, cwdHome.path, s);
      }
    } else if (s.workspace_id) {
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

  // Build the workspace + repo container pools separately so the manual
  // ordering pass can weave them however the user has dragged. Both
  // pools start in alphabetical order so the "no manual order" path
  // matches the pre-task-#11 layout exactly.
  const sortedWorkspaces = [...workspaces].sort((a, b) =>
    a.name.localeCompare(b.name),
  );
  const workspaceContainers: TreeContainer[] = sortedWorkspaces.map((w) => ({
    key: `ws:${w.id}`,
    kind: "workspace",
    id: w.id,
    name: w.name,
    fsPath: null,
    sessions: sortSessions(wsSessions.get(w.id) ?? []),
    removable: true,
  }));

  const sortedRepos = [...repos]
    .filter((r) => !memberRepoIds.has(r.id))
    .sort((a, b) => a.name.localeCompare(b.name));
  const repoContainers: TreeContainer[] = sortedRepos.map((r) => ({
    key: `repo:${r.id}`,
    kind: "repo",
    id: r.id,
    name: r.name,
    hoverTitle: r.path,
    fsPath: r.path,
    sessions: sortSessions(repoSessions.get(r.id) ?? []),
    removable: true,
  }));

  const out: TreeContainer[] = applyContainerOrder(
    workspaceContainers,
    repoContainers,
    containerOrder,
  );

  for (const c of standaloneContainers.sort((a, b) =>
    a.name.localeCompare(b.name),
  )) {
    out.push(c);
  }

  for (const [path, pathSessions] of [...cwdSessions.entries()].sort((a, b) =>
    pathLeafName(a[0]).localeCompare(pathLeafName(b[0])),
  )) {
    out.push({
      key: `cwd:${path}`,
      kind: "cwd",
      id: path,
      name: pathLeafName(path),
      hoverTitle: path,
      fsPath: path,
      sessions: sortSessions(pathSessions),
      removable: false,
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

type CwdHome =
  | { kind: "workspace"; id: string }
  | { kind: "repo"; id: string }
  | { kind: "cwd"; path: string };

function findContainerForCwd(
  cwd: string,
  repos: RepoEntry[],
  workspaces: WorkspaceEntry[],
  memberRepoIds: Set<string>,
): CwdHome {
  const normalizedCwd = normalizeFsPath(cwd);
  let bestRepo: RepoEntry | null = null;
  for (const repo of repos) {
    if (!pathContains(repo.path, cwd)) continue;
    if (
      !bestRepo ||
      normalizeFsPath(repo.path).length > normalizeFsPath(bestRepo.path).length
    ) {
      bestRepo = repo;
    }
  }
  if (bestRepo) {
    const workspace = workspaces.find((w) =>
      w.member_repo_ids.includes(bestRepo.id),
    );
    if (workspace) return { kind: "workspace", id: workspace.id };
    if (!memberRepoIds.has(bestRepo.id)) return { kind: "repo", id: bestRepo.id };
  }
  return { kind: "cwd", path: normalizedCwd };
}

function pathContains(parent: string, child: string): boolean {
  const p = normalizeFsPathForCompare(parent);
  const c = normalizeFsPathForCompare(child);
  if (p === c) return true;
  const sep = p.includes("\\") ? "\\" : "/";
  return c.startsWith(`${p}${sep}`);
}

function normalizeFsPath(path: string): string {
  const windowsLike = /^[A-Za-z]:[\\/]/.test(path) || path.includes("\\");
  const normalized = windowsLike ? path.replace(/\//g, "\\") : path;
  if (normalized.length <= 3) return normalized;
  return windowsLike
    ? normalized.replace(/\\+$/g, "")
    : normalized.replace(/\/+$/g, "");
}

function normalizeFsPathForCompare(path: string): string {
  return normalizeFsPath(path).toLowerCase();
}

function pathLeafName(path: string): string {
  const trimmed = path.replace(/[\\/]+$/g, "");
  const parts = trimmed.split(/[\\/]/);
  return parts[parts.length - 1] || path;
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
