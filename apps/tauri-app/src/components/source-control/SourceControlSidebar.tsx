/**
 * Global Source Control sidebar — Phases A–C of
 * `docs/plans/source-control-sidebar.md`.
 *
 * Tracks the focused pane's repo; manual override is available when more
 * than one repo is registered. Phase C adds the staged/unstaged split and
 * write actions (stage, unstage, commit) — error reporting is via toast.
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
import ResizableSplit from "../ResizableSplit";
import ChangesTree, { type RowAction } from "./ChangesTree";
import DiscardConfirmDialog from "./DiscardConfirmDialog";
import StashesSection from "./StashesSection";

type Tab = "changes" | "history";

const COMMIT_LIMIT = 50;
const STORAGE_KEY = "rt.sourceControl.repoOverride";

interface Props {
  repos: RepoEntry[];
  focusedRepoId: string | null;
  client: DaemonClient;
  /// Called with the freshly opened diff tab's id so the host can switch to
  /// it. Passed through to `ChangesView` for the per-row click handler.
  onActivateTab: (tabId: string) => void;
}

export default function SourceControlSidebar({
  repos,
  focusedRepoId,
  client,
  onActivateTab,
}: Props) {
  const [tab, setTab] = useState<Tab>("changes");
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
  const followingFocus = override === null && focusedRepoId !== null;

  if (repos.length === 0) {
    return (
      <aside className="source-control" data-testid="source-control-sidebar">
        <header className="source-control-header">Source control</header>
        <p className="empty" style={{ padding: "12px 14px" }}>
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
              <option value="">Follow focused pane</option>
              {repos.map((r) => (
                <option key={r.id} value={r.id}>
                  {r.name}
                </option>
              ))}
            </select>
          )}
        </div>
        {activeRepo && (
          <div className="source-control-active-repo" title={activeRepo.path}>
            <strong>{activeRepo.name}</strong>
            {followingFocus && (
              <span className="muted small" style={{ marginLeft: 6 }}>
                · following focus
              </span>
            )}
          </div>
        )}
      </header>
      <div className="source-control-tabs">
        <button
          type="button"
          className={tab === "changes" ? "tab active" : "tab"}
          onClick={() => setTab("changes")}
          data-testid="source-control-tab-changes"
        >
          Changes
        </button>
        <button
          type="button"
          className={tab === "history" ? "tab active" : "tab"}
          onClick={() => setTab("history")}
          data-testid="source-control-tab-history"
        >
          History
        </button>
      </div>
      {activeRepoId && tab === "changes" && (
        <ChangesView
          activeRepoId={activeRepoId}
          activeRepoName={activeRepo?.name ?? ""}
          client={client}
          onActivateTab={onActivateTab}
        />
      )}
      {activeRepoId && tab === "history" && (
        <HistoryView activeRepoId={activeRepoId} client={client} />
      )}
    </aside>
  );
}

// ---------- Changes ----------

interface ChangesViewProps {
  activeRepoId: string;
  activeRepoName: string;
  client: DaemonClient;
  onActivateTab: (tabId: string) => void;
}

function ChangesView({
  activeRepoId,
  activeRepoName,
  client,
  onActivateTab,
}: ChangesViewProps) {
  const [indexChanges, setIndexChanges] = useState<GitFileChange[] | null>(null);
  const [worktreeChanges, setWorktreeChanges] = useState<GitFileChange[] | null>(null);
  const [commitMessage, setCommitMessage] = useState("");
  const [pendingOp, setPendingOp] = useState<string | null>(null);
  const [errorBanner, setErrorBanner] = useState<string | null>(null);
  const [pendingDiscard, setPendingDiscard] = useState<string[] | null>(null);

  useEffect(() => {
    setIndexChanges(null);
    setWorktreeChanges(null);
    setErrorBanner(null);
    setCommitMessage("");
    client.send({ type: "repo_status", repo_id: activeRepoId });
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "repo_status" || detail.repo_id !== activeRepoId)
        return;
      setIndexChanges(detail.index_changes);
      setWorktreeChanges(detail.worktree_changes);
    };
    window.addEventListener("rt:repo_status", handler);
    return () => window.removeEventListener("rt:repo_status", handler);
  }, [activeRepoId, client]);

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

  const openDiff = (path: string, bucket: "index" | "worktree") => {
    void openDiffTab(client, {
      repoId: activeRepoId,
      path,
      against: bucket === "index" ? "HEAD" : null,
    }).then((tabId) => {
      if (tabId) onActivateTab(tabId);
    });
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

  return (
    <div
      className="git-list source-control-list source-control-changes-full"
      data-testid="source-control-changes-list"
    >
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
            >
              ×
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
                  selectedPath={null}
                  onSelect={(path) => openDiff(path, "index")}
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
                  selectedPath={null}
                  onSelect={(path) => openDiff(path, "worktree")}
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
            data-testid="source-control-open-in-forge"
          >
            open in forge ↗
          </button>
        </div>
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

// ---------- History ----------

interface HistoryViewProps {
  activeRepoId: string;
  client: DaemonClient;
}

function HistoryView({ activeRepoId, client }: HistoryViewProps) {
  const [commits, setCommits] = useState<GitCommit[] | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [detailDiff, setDetailDiff] = useState<string | null>(null);

  useEffect(() => {
    setCommits(null);
    setSelected(null);
    setDetailDiff(null);
    client.send({
      type: "list_commits",
      repo_id: activeRepoId,
      branch: null,
      limit: COMMIT_LIMIT,
    });
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "commits" || detail.repo_id !== activeRepoId) return;
      setCommits(detail.commits);
    };
    window.addEventListener("rt:commits", handler);
    return () => window.removeEventListener("rt:commits", handler);
  }, [activeRepoId, client]);

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
    <ResizableSplit
      storageKey="source-control.history"
      defaultSize={260}
      minSize={180}
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
          <ul className="list">
            {commits.map((c) => (
              <li
                key={c.sha}
                className={
                  c.sha === selected ? "list-item selected" : "list-item"
                }
                onClick={() => setSelected(c.sha)}
                data-testid="source-control-history-row"
              >
                <span className="commit-sha">{c.short_sha}</span>
                <span className="list-item-label" title={c.subject}>
                  {c.subject}
                </span>
                <span className="list-item-meta small">
                  {c.author_name.split(" ")[0]} ·{" "}
                  {c.authored_at.slice(0, 10)}
                </span>
              </li>
            ))}
          </ul>
        )}
      </div>
      <DiffView diff={detailDiff} testId="source-control-history-diff" />
    </ResizableSplit>
  );
}

// ---------- DiffView (shared) ----------

function DiffView({ diff, testId }: { diff: string | null; testId?: string }) {
  if (diff === null) {
    return (
      <div className="diff-pane empty" data-testid={testId}>
        select an item
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
    <pre className="diff-pane" data-testid={testId}>
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
