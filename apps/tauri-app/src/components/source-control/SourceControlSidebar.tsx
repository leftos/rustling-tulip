/**
 * Global Source Control sidebar.
 *
 * Tracks the focused pane's repo; manual override is available when more
 * than one repo is registered. The sidebar exposes staged/unstaged changes,
 * history, and write actions; error reporting is via toast.
 */
import { useEffect, useMemo, useState } from "react";
import { open as openInShell } from "@tauri-apps/plugin-shell";
import { getRemoteUrl, openDiffTab, type DaemonClient } from "../../api";
import type {
  DaemonMessage,
  GitCommit,
  GitFileChange,
  RepoEntry,
} from "../../types";
import { useClampedMenuPosition, useEscape } from "../../utils/a11y";
import Icon from "../Icon";
import ResizableSplit from "../ResizableSplit";
import ChangesTree, { type RowAction } from "./ChangesTree";
import DiscardConfirmDialog from "./DiscardConfirmDialog";
import StashesSection from "./StashesSection";

const COMMIT_PAGE_SIZE = 50;
const STORAGE_KEY = "rt.sourceControl.repoOverride";

interface Props {
  repos: RepoEntry[];
  focusedRepoId: string | null;
  client: DaemonClient;
  /// Called with the freshly opened diff tab's id so the host can switch to
  /// it. Passed through to `ChangesView` for the per-row click handler.
  onActivateTab: (tabId: string) => void;
}

type ChangeBucket = "index" | "worktree";

interface SelectedChange {
  repoId: string;
  bucket: ChangeBucket;
  path: string;
}

interface FileContextMenuState {
  bucket: ChangeBucket;
  path: string;
  x: number;
  y: number;
}

export default function SourceControlSidebar({
  repos,
  focusedRepoId,
  client,
  onActivateTab,
}: Props) {
  const [refreshSeq, setRefreshSeq] = useState(0);
  const [selectedChange, setSelectedChange] = useState<SelectedChange | null>(
    null,
  );
  const [changesNeedSpace, setChangesNeedSpace] = useState(false);
  const [historyCollapsed, setHistoryCollapsed] = useState(false);
  const [override, setOverride] = useState<string | null>(() => {
    try {
      return localStorage.getItem(STORAGE_KEY);
    } catch {
      return null;
    }
  });
  const updateOverride = (next: string | null) => {
    setOverride(next);
    try {
      if (next === null) localStorage.removeItem(STORAGE_KEY);
      else localStorage.setItem(STORAGE_KEY, next);
    } catch {
      /* best-effort */
    }
  };

  const activeRepoId = useMemo(() => {
    if (override && repos.some((r) => r.id === override)) return override;
    if (focusedRepoId && repos.some((r) => r.id === focusedRepoId)) {
      return focusedRepoId;
    }
    return repos[0]?.id ?? null;
  }, [override, focusedRepoId, repos]);
  const activeRepo = repos.find((r) => r.id === activeRepoId) ?? null;
  const isAuto = override === null;
  const followingFocus = isAuto && focusedRepoId !== null;
  // Auto mode has two sub-states: actively following a focused pane,
  // or sitting on the fallback (first repo) because no pane is focused.
  // The user complaint was that "Follow focused pane" reads as if there
  // is always a focused pane to follow — surface the fallback state too.
  // Only meaningful when 2+ repos exist; in the single-repo case the
  // picker is hidden anyway and there's nothing to fall back from.
  const autoNoActive = isAuto && focusedRepoId === null && repos.length > 1;

  useEffect(() => {
    setChangesNeedSpace(false);
    setHistoryCollapsed(false);
  }, [activeRepoId]);

  useEffect(() => {
    setHistoryCollapsed(changesNeedSpace);
  }, [changesNeedSpace]);

  const refreshSourceControl = () => {
    setRefreshSeq((value) => value + 1);
  };

  if (repos.length === 0) {
    return (
      <aside className="source-control" data-testid="source-control-sidebar">
        <header className="source-control-header">Source control</header>
        <p className="empty">
          Register a repo from the Sessions sidebar to inspect changes here.
        </p>
      </aside>
    );
  }

  return (
    <aside className="source-control" data-testid="source-control-sidebar">
      <header className="source-control-header">
        <div className="source-control-header-row">
          <span className="brand">Source control</span>
          <div className="source-control-header-actions">
            <button
              type="button"
              className="source-control-refresh-button"
              onClick={refreshSourceControl}
              aria-label="Refresh source-control status and history"
              title="Refresh status and history"
              data-testid="source-control-refresh"
            >
              <Icon name="refresh" />
            </button>
            {repos.length > 1 && (
              <select
                className="repo-picker"
                value={override ?? ""}
                onChange={(e) =>
                  updateOverride(e.target.value === "" ? null : e.target.value)
                }
                aria-label="Pin source-control sidebar to a specific repo"
                data-testid="source-control-repo-picker"
              >
                <option value="">Auto · follow active pane</option>
                {repos.map((r) => (
                  <option key={r.id} value={r.id}>
                    {r.name}
                  </option>
                ))}
              </select>
            )}
          </div>
        </div>
        {activeRepo && (
          <div
            className="source-control-active-repo"
            title={activeRepo.path}
            data-testid="source-control-active-repo"
          >
            <strong>{activeRepo.name}</strong>
            {followingFocus && (
              <span className="muted small inline-note">
                · following active pane
              </span>
            )}
            {autoNoActive && (
              <span
                className="muted small inline-note"
                title="No pane is focused right now — falling back to the first registered repo. Pick a session in the sidebar to follow it, or pin a repo from the picker above."
              >
                · no active pane
              </span>
            )}
          </div>
        )}
      </header>
      {activeRepoId && (
        <div
          className={
            historyCollapsed
              ? "source-control-body source-control-history-collapsed"
              : "source-control-body"
          }
        >
          <ResizableSplit
            storageKey="source-control.sidebar"
            defaultSize={420}
            minSize={180}
            direction="vertical"
          >
            <ChangesView
              activeRepoId={activeRepoId}
              activeRepoName={activeRepo?.name ?? ""}
              client={client}
              refreshSeq={refreshSeq}
              selectedChange={selectedChange}
              onSelectedChange={setSelectedChange}
              onActivateTab={onActivateTab}
              onChangesNeedSpace={setChangesNeedSpace}
            />
            <HistoryView
              activeRepoId={activeRepoId}
              client={client}
              collapsed={historyCollapsed}
              refreshSeq={refreshSeq}
              onCollapsedChange={setHistoryCollapsed}
            />
          </ResizableSplit>
        </div>
      )}
    </aside>
  );
}

// ---------- Changes ----------

interface ChangesViewProps {
  activeRepoId: string;
  activeRepoName: string;
  client: DaemonClient;
  refreshSeq: number;
  selectedChange: SelectedChange | null;
  onSelectedChange: (selected: SelectedChange) => void;
  onActivateTab: (tabId: string) => void;
  onChangesNeedSpace: (needsSpace: boolean) => void;
}

function ChangesView({
  activeRepoId,
  activeRepoName,
  client,
  refreshSeq,
  selectedChange,
  onSelectedChange,
  onActivateTab,
  onChangesNeedSpace,
}: ChangesViewProps) {
  const [indexChanges, setIndexChanges] = useState<GitFileChange[] | null>(null);
  const [worktreeChanges, setWorktreeChanges] = useState<GitFileChange[] | null>(null);
  const [contextMenu, setContextMenu] = useState<FileContextMenuState | null>(null);
  const [commitMessage, setCommitMessage] = useState("");
  const [pendingOp, setPendingOp] = useState<string | null>(null);
  const [errorBanner, setErrorBanner] = useState<string | null>(null);
  const [pendingDiscard, setPendingDiscard] = useState<string[] | null>(null);

  useEffect(() => {
    setIndexChanges(null);
    setWorktreeChanges(null);
    setContextMenu(null);
    setErrorBanner(null);
    setCommitMessage("");
    onChangesNeedSpace(false);
  }, [activeRepoId, onChangesNeedSpace]);

  useEffect(() => {
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "repo_status" || detail.repo_id !== activeRepoId)
        return;
      setIndexChanges(detail.index_changes);
      setWorktreeChanges(detail.worktree_changes);
      onChangesNeedSpace(
        detail.index_changes.length > 0 || detail.worktree_changes.length > 0,
      );
      setPendingOp((op) => (op === "commit" ? op : null));
    };
    window.addEventListener("rt:repo_status", handler);
    client.send({ type: "repo_status", repo_id: activeRepoId });
    return () => window.removeEventListener("rt:repo_status", handler);
  }, [activeRepoId, client, onChangesNeedSpace, refreshSeq]);

  // Listen for write errors + commit confirmation across the lifetime of
  // this view.
  useEffect(() => {
    const onError = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (
        detail.type !== "git_write_error" ||
        detail.repo_id !== activeRepoId
      )
        return;
      setPendingOp(null);
      setErrorBanner(`${detail.operation}: ${detail.error}`);
    };
    const onOk = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "commit_ok" || detail.repo_id !== activeRepoId)
        return;
      setPendingOp(null);
      setCommitMessage("");
      setErrorBanner(null);
    };
    window.addEventListener("rt:git_write_error", onError);
    window.addEventListener("rt:commit_ok", onOk);
    return () => {
      window.removeEventListener("rt:git_write_error", onError);
      window.removeEventListener("rt:commit_ok", onOk);
    };
  }, [activeRepoId]);

  const openDiff = (path: string, bucket: ChangeBucket) => {
    onSelectedChange({ repoId: activeRepoId, path, bucket });
    void openDiffTab(client, {
      repoId: activeRepoId,
      path,
      against: bucket === "index" ? "HEAD" : null,
    }).then((tabId) => {
      if (tabId) onActivateTab(tabId);
    });
  };
  const openContextMenu = (
    path: string,
    bucket: ChangeBucket,
    e: React.MouseEvent,
  ) => {
    onSelectedChange({ repoId: activeRepoId, path, bucket });
    setContextMenu({ path, bucket, x: e.clientX, y: e.clientY });
  };

  const stagedPaths = useMemo(
    () => (indexChanges ?? []).map((c) => c.path),
    [indexChanges],
  );
  const worktreePaths = useMemo(
    () => (worktreeChanges ?? []).map((c) => c.path),
    [worktreeChanges],
  );

  const sendStage = (paths: string[]) => {
    if (paths.length === 0) return;
    setPendingOp("stage");
    client.send({ type: "stage_files", repo_id: activeRepoId, paths });
  };
  const sendUnstage = (paths: string[]) => {
    if (paths.length === 0) return;
    setPendingOp("unstage");
    client.send({ type: "unstage_files", repo_id: activeRepoId, paths });
  };
  const sendCommit = () => {
    const trimmed = commitMessage.trim();
    if (trimmed.length === 0 || stagedPaths.length === 0) return;
    setPendingOp("commit");
    client.send({
      type: "commit_repo",
      repo_id: activeRepoId,
      message: trimmed,
    });
  };
  const requestDiscard = (paths: string[]) => {
    if (paths.length === 0) return;
    setPendingDiscard(paths);
  };
  const confirmDiscard = () => {
    if (!pendingDiscard || pendingDiscard.length === 0) {
      setPendingDiscard(null);
      return;
    }
    setPendingOp("discard");
    client.send({
      type: "discard_changes",
      repo_id: activeRepoId,
      paths: pendingDiscard,
    });
    setPendingDiscard(null);
  };

  const stageAction: RowAction = {
    glyph: "+",
    label: "Stage",
    onClick: (path) => sendStage([path]),
    testId: "source-control-row-stage",
  };
  const discardAction: RowAction = {
    glyph: "↺",
    label: "Discard",
    onClick: (path) => requestDiscard([path]),
    testId: "source-control-row-discard",
    variant: "danger",
  };
  const unstageAction: RowAction = {
    glyph: "−",
    label: "Unstage",
    onClick: (path) => sendUnstage([path]),
    testId: "source-control-row-unstage",
  };

  const canCommit =
    stagedPaths.length > 0 &&
    commitMessage.trim().length > 0 &&
    pendingOp !== "commit";
  const showCommitBox =
    stagedPaths.length > 0 ||
    commitMessage.trim().length > 0 ||
    pendingOp === "commit";

  return (
    <div
      className="git-list source-control-list source-control-changes-full"
      data-testid="source-control-changes-list"
    >
      {showCommitBox && (
        <div className="source-control-commit">
          <textarea
            className="commit-input"
            value={commitMessage}
            onChange={(e) => setCommitMessage(e.target.value)}
            placeholder="Commit message (Ctrl+Enter to commit)"
            rows={2}
            aria-label="Commit message"
            data-testid="source-control-commit-message"
            onKeyDown={(e) => {
              if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
                e.preventDefault();
                sendCommit();
              }
            }}
          />
          <button
            type="button"
            className="commit-button"
            disabled={!canCommit}
            onClick={sendCommit}
            data-testid="source-control-commit-submit"
          >
            {pendingOp === "commit" ? "Committing…" : "Commit"}
          </button>
        </div>
      )}
        {errorBanner && (
          <div
            className="source-control-error"
            role="alert"
            data-testid="source-control-error"
          >
            {errorBanner}
            <button
              type="button"
              className="dismiss"
              onClick={() => setErrorBanner(null)}
              aria-label="Dismiss error"
              title="Dismiss error"
            >
              <Icon name="close" />
            </button>
          </div>
        )}
        {indexChanges === null || worktreeChanges === null ? (
          <p className="empty">loading…</p>
        ) : indexChanges.length === 0 && worktreeChanges.length === 0 ? (
          <p className="empty">working tree clean</p>
        ) : (
          <>
            {indexChanges.length > 0 && (
              <section
                className="changes-bucket"
                data-testid="source-control-bucket-index"
              >
                <BucketHeader
                  label="Staged Changes"
                  count={indexChanges.length}
                  actions={[
                    {
                      label: "Unstage all",
                      onClick: () => sendUnstage(stagedPaths),
                      disabled: pendingOp !== null,
                      testId: "source-control-unstage-all",
                    },
                  ]}
                />
                <ChangesTree
                  changes={indexChanges}
                  selectedPath={
                    selectedChange?.repoId === activeRepoId &&
                    selectedChange.bucket === "index"
                      ? selectedChange.path
                      : null
                  }
                  onSelect={(path) => openDiff(path, "index")}
                  onRowContextMenu={(path, e) =>
                    openContextMenu(path, "index", e)
                  }
                  rowActions={[unstageAction]}
                />
              </section>
            )}
            {worktreeChanges.length > 0 && (
              <section
                className="changes-bucket"
                data-testid="source-control-bucket-worktree"
              >
                <BucketHeader
                  label="Changes"
                  count={worktreeChanges.length}
                  actions={[
                    {
                      label: "Discard all",
                      onClick: () => requestDiscard(worktreePaths),
                      disabled: pendingOp !== null,
                      testId: "source-control-discard-all",
                      variant: "danger",
                    },
                    {
                      label: "Stage all",
                      onClick: () => sendStage(worktreePaths),
                      disabled: pendingOp !== null,
                      testId: "source-control-stage-all",
                    },
                  ]}
                />
                <ChangesTree
                  changes={worktreeChanges}
                  selectedPath={
                    selectedChange?.repoId === activeRepoId &&
                    selectedChange.bucket === "worktree"
                      ? selectedChange.path
                      : null
                  }
                  onSelect={(path) => openDiff(path, "worktree")}
                  onRowContextMenu={(path, e) =>
                    openContextMenu(path, "worktree", e)
                  }
                  rowActions={[discardAction, stageAction]}
                />
              </section>
            )}
          </>
        )}
        <StashesSection repoId={activeRepoId} client={client} />
        <div className="git-meta">
          {activeRepoName}
          <button
            type="button"
            className="link"
            onClick={() => openInForge(client, activeRepoId)}
            title="Open repository in forge"
            data-testid="source-control-open-in-forge"
          >
            Open in forge
          </button>
        </div>
        {contextMenu && (
          <FileContextMenu
            state={contextMenu}
            pendingOp={pendingOp}
            onClose={() => setContextMenu(null)}
            onOpenDiff={() => openDiff(contextMenu.path, contextMenu.bucket)}
            onStage={() => sendStage([contextMenu.path])}
            onUnstage={() => sendUnstage([contextMenu.path])}
            onDiscard={() => requestDiscard([contextMenu.path])}
          />
        )}
        {pendingDiscard && (
          <DiscardConfirmDialog
            paths={pendingDiscard}
            onCancel={() => setPendingDiscard(null)}
            onConfirm={confirmDiscard}
          />
        )}
    </div>
  );
}

interface BucketAction {
  label: string;
  onClick: () => void;
  disabled: boolean;
  testId: string;
  variant?: string;
}

interface BucketHeaderProps {
  label: string;
  count: number;
  actions: BucketAction[];
}

function BucketHeader({ label, count, actions }: BucketHeaderProps) {
  return (
    <div className="changes-bucket-header">
      <span className="bucket-label">{label}</span>
      <span className="bucket-count">{count}</span>
      {actions.map((action) => (
        <button
          key={action.testId}
          type="button"
          className={
            action.variant ? `bucket-action ${action.variant}` : "bucket-action"
          }
          disabled={action.disabled}
          onClick={action.onClick}
          data-testid={action.testId}
        >
          {action.label}
        </button>
      ))}
    </div>
  );
}

interface FileContextMenuProps {
  state: FileContextMenuState;
  pendingOp: string | null;
  onClose: () => void;
  onOpenDiff: () => void;
  onStage: () => void;
  onUnstage: () => void;
  onDiscard: () => void;
}

function FileContextMenu({
  state,
  pendingOp,
  onClose,
  onOpenDiff,
  onStage,
  onUnstage,
  onDiscard,
}: FileContextMenuProps) {
  useEscape(onClose);
  const disabled = pendingOp !== null;
  const closeAfter = (action: () => void) => {
    action();
    onClose();
  };
  const { ref: menuRef, position } = useClampedMenuPosition<HTMLUListElement>({
    x: state.x,
    y: state.y,
  });
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
        ref={menuRef}
        className="context-menu"
        style={{ left: position.left, top: position.top }}
        onClick={(e) => e.stopPropagation()}
        data-testid="source-control-file-context-menu"
      >
        <li>
          <button
            type="button"
            onClick={() => closeAfter(onOpenDiff)}
            data-testid="source-control-context-open-diff"
            data-file-path={state.path}
          >
            {state.bucket === "index" ? "Open Staged Changes" : "Open Changes"}
          </button>
        </li>
        {state.bucket === "index" ? (
          <li>
            <button
              type="button"
              disabled={disabled}
              onClick={() => closeAfter(onUnstage)}
              data-testid="source-control-context-unstage"
              data-file-path={state.path}
            >
              Unstage Changes
            </button>
          </li>
        ) : (
          <>
            <li>
              <button
                type="button"
                disabled={disabled}
                onClick={() => closeAfter(onStage)}
                data-testid="source-control-context-stage"
                data-file-path={state.path}
              >
                Stage Changes
              </button>
            </li>
            <li className="context-menu-separator" aria-hidden="true" />
            <li>
              <button
                type="button"
                className="danger"
                disabled={disabled}
                onClick={() => closeAfter(onDiscard)}
                data-testid="source-control-context-discard"
                data-file-path={state.path}
              >
                Discard Changes
              </button>
            </li>
          </>
        )}
      </ul>
    </div>
  );
}

// ---------- History ----------

interface HistoryViewProps {
  activeRepoId: string;
  client: DaemonClient;
  collapsed: boolean;
  refreshSeq: number;
  onCollapsedChange: (collapsed: boolean) => void;
}

function HistoryView({
  activeRepoId,
  client,
  collapsed,
  refreshSeq,
  onCollapsedChange,
}: HistoryViewProps) {
  const [commits, setCommits] = useState<GitCommit[] | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [detailDiff, setDetailDiff] = useState<string | null>(null);
  // `true` once the last batch came back shorter than the requested page —
  // means the daemon's `git log` is exhausted, no Load more button.
  const [exhausted, setExhausted] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);

  useEffect(() => {
    setCommits(null);
    setSelected(null);
    setDetailDiff(null);
    setExhausted(false);
    setLoadingMore(false);
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "commits" || detail.repo_id !== activeRepoId) return;
      setExhausted(detail.commits.length < COMMIT_PAGE_SIZE);
      setLoadingMore(false);
      if (detail.offset === 0) {
        setCommits(detail.commits);
      } else {
        setCommits((existing) => {
          if (existing === null) return detail.commits;
          // Defensive de-dup by sha — a daemon refresh during pagination
          // could overlap. Preserve append order.
          const seen = new Set(existing.map((c) => c.sha));
          const next = [...existing];
          for (const c of detail.commits) {
            if (!seen.has(c.sha)) {
              next.push(c);
              seen.add(c.sha);
            }
          }
          return next;
        });
      }
    };
    window.addEventListener("rt:commits", handler);
    client.send({
      type: "list_commits",
      repo_id: activeRepoId,
      branch: null,
      limit: COMMIT_PAGE_SIZE,
      offset: 0,
    });
    return () => window.removeEventListener("rt:commits", handler);
  }, [activeRepoId, client, refreshSeq]);

  const loadMore = () => {
    if (!commits || loadingMore || exhausted) return;
    setLoadingMore(true);
    client.send({
      type: "list_commits",
      repo_id: activeRepoId,
      branch: null,
      limit: COMMIT_PAGE_SIZE,
      offset: commits.length,
    });
  };

  useEffect(() => {
    if (!selected) return;
    setDetailDiff(null);
    client.send({ type: "get_commit", repo_id: activeRepoId, sha: selected });
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (
        detail.type !== "commit_detail" ||
        detail.repo_id !== activeRepoId ||
        detail.detail.commit.sha !== selected
      )
        return;
      const lines = [
        `${detail.detail.commit.short_sha} ${detail.detail.commit.subject}`,
        `Author: ${detail.detail.commit.author_name} <${detail.detail.commit.author_email}>`,
        `Date:   ${detail.detail.commit.authored_at}`,
        "",
        detail.detail.body,
        "",
        "Files:",
        ...detail.detail.changes.map((c) => `  ${c.status}  ${c.path}`),
      ];
      setDetailDiff(lines.join("\n"));
    };
    window.addEventListener("rt:commit_detail", handler);
    return () => window.removeEventListener("rt:commit_detail", handler);
  }, [activeRepoId, selected, client]);

  return (
    <section
      className="source-control-history-panel"
      data-testid="source-control-history-panel"
    >
      <div className="source-control-section-title source-control-history-title">
        <button
          type="button"
          className="source-control-history-toggle"
          aria-expanded={!collapsed}
          onClick={() => onCollapsedChange(!collapsed)}
          data-testid="source-control-history-toggle"
        >
          <span className="tree-caret" aria-hidden="true">
            {collapsed ? "▸" : "▾"}
          </span>
          <span>History</span>
          <span className="bucket-count">{commits?.length ?? 0}</span>
        </button>
      </div>
      {!collapsed && (
        <ResizableSplit
          storageKey="source-control.history"
          defaultSize={220}
          minSize={110}
          direction="vertical"
        >
          <div
            className="git-list source-control-list"
            data-testid="source-control-history-list"
          >
            {!commits ? (
              <p className="empty">loading…</p>
            ) : commits.length === 0 ? (
              <p className="empty">no commits</p>
            ) : (
              <>
                <ul className="list">
                  {commits.map((c) => (
                    <li
                      key={c.sha}
                      className={
                        c.sha === selected
                          ? "list-item history-commit-row selected"
                          : "list-item history-commit-row"
                      }
                      onClick={() => setSelected(c.sha)}
                      title={commitHoverText(c)}
                      aria-label={commitHoverText(c)}
                      data-testid="source-control-history-row"
                    >
                      <span className="commit-sha">{c.short_sha}</span>
                      <span
                        className="commit-subject"
                        data-testid="source-control-history-subject"
                      >
                        {c.subject}
                      </span>
                      <span
                        className="commit-author-chip"
                        title={commitAuthor(c)}
                        data-testid="source-control-history-author"
                      >
                        {c.author_name}
                      </span>
                      <span className="commit-hover-card" aria-hidden="true">
                        <span className="commit-hover-subject">
                          {c.subject}
                        </span>
                        <span>
                          <span className="commit-hover-label">SHA</span>
                          {c.sha}
                        </span>
                        <span>
                          <span className="commit-hover-label">Author</span>
                          {commitAuthor(c)}
                        </span>
                        <span>
                          <span className="commit-hover-label">Date</span>
                          {c.authored_at}
                        </span>
                      </span>
                    </li>
                  ))}
                </ul>
                {!exhausted && (
                  <button
                    type="button"
                    className="history-load-more"
                    onClick={loadMore}
                    disabled={loadingMore}
                    data-testid="source-control-history-load-more"
                  >
                    {loadingMore ? "loading…" : "load more"}
                  </button>
                )}
              </>
            )}
          </div>
          <DiffView
            diff={detailDiff}
            testId="source-control-history-diff"
            placeholder="Select a commit to view its diff."
            wrap
          />
        </ResizableSplit>
      )}
    </section>
  );
}

function commitHoverText(commit: GitCommit): string {
  return [
    commit.subject,
    `SHA: ${commit.sha}`,
    `Author: ${commitAuthor(commit)}`,
    `Date: ${commit.authored_at}`,
  ].join("\n");
}

function commitAuthor(commit: GitCommit): string {
  const email = commit.author_email.trim();
  if (email.length === 0) {
    return commit.author_name;
  }
  return `${commit.author_name} <${email}>`;
}

// ---------- DiffView (shared) ----------

function DiffView({
  diff,
  testId,
  placeholder,
  wrap,
}: {
  diff: string | null;
  testId?: string;
  /// Contextual placeholder for the empty (nothing-selected) state.
  /// Defaults to a generic message; the Changes and History views
  /// override this so the user knows which view they're in even when
  /// nothing is selected.
  placeholder?: string;
  wrap: boolean;
}) {
  if (diff === null) {
    return (
      <div className="diff-pane empty" data-testid={testId}>
        {placeholder ?? "select an item"}
      </div>
    );
  }
  if (diff.length === 0) {
    return (
      <div className="diff-pane empty" data-testid={testId}>
        (empty diff)
      </div>
    );
  }
  return (
    <pre
      className={wrap ? "diff-pane diff-pane-wrap" : "diff-pane"}
      data-testid={testId}
    >
      {diff.split("\n").map((line, i) => (
        // eslint-disable-next-line react/no-array-index-key
        <span key={i} className={diffLineClass(line)}>
          {line}
          {"\n"}
        </span>
      ))}
    </pre>
  );
}

function diffLineClass(line: string): string {
  if (
    line.startsWith("+++") ||
    line.startsWith("---") ||
    line.startsWith("diff ")
  )
    return "diff-meta";
  if (line.startsWith("@@")) return "diff-hunk";
  if (line.startsWith("+")) return "diff-add";
  if (line.startsWith("-")) return "diff-del";
  return "";
}

async function openInForge(
  client: DaemonClient,
  repoId: string,
): Promise<void> {
  const remote = await getRemoteUrl(client, repoId);
  if (!remote || !remote.web_url) {
    return;
  }
  void openInShell(remote.web_url);
}
