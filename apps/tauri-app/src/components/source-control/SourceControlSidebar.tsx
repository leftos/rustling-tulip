/**
 * Global Source Control sidebar — Phase A of `docs/plans/source-control-sidebar.md`.
 *
 * Replaces the per-session `GitPanel` toggle with a single sidebar that
 * tracks the focused pane's repo. Manual override is available when more
 * than one repo is registered. The data plumbing (`repo_status` /
 * `list_commits` / `get_file_diff` / `get_commit` round-trips) is unchanged
 * from the deleted `GitPanel.tsx` — only the host moves.
 *
 * Phase A keeps the read-only, flat-list shape on purpose. Tree-folded
 * changes, paginated graph, stage/unstage/commit, and Monaco diff are
 * scheduled for phases B–E.
 */
import { useEffect, useMemo, useState } from "react";
import { open as openInShell } from "@tauri-apps/plugin-shell";
import { getRemoteUrl, type DaemonClient } from "../../api";
import type {
  DaemonMessage,
  GitCommit,
  GitFileChange,
  RepoEntry,
} from "../../types";
import ResizableSplit from "../ResizableSplit";

type Tab = "changes" | "history";

const COMMIT_LIMIT = 50;
const STORAGE_KEY = "rt.sourceControl.repoOverride";

interface Props {
  repos: RepoEntry[];
  focusedRepoId: string | null;
  client: DaemonClient;
}

export default function SourceControlSidebar({
  repos,
  focusedRepoId,
  client,
}: Props) {
  const [tab, setTab] = useState<Tab>("changes");
  // null = follow focused pane; string = user-pinned override.
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

  // Resolve the active repo. Override wins; else follow focus; else fall
  // back to the first registered repo (still useful as a no-session
  // browsing surface).
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
}

function ChangesView({ activeRepoId, activeRepoName, client }: ChangesViewProps) {
  const [changes, setChanges] = useState<GitFileChange[] | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [diff, setDiff] = useState<string | null>(null);

  useEffect(() => {
    setChanges(null);
    setSelected(null);
    setDiff(null);
    client.send({ type: "repo_status", repo_id: activeRepoId });
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "repo_status" || detail.repo_id !== activeRepoId)
        return;
      setChanges(detail.changes);
    };
    window.addEventListener("rt:repo_status", handler);
    return () => window.removeEventListener("rt:repo_status", handler);
  }, [activeRepoId, client]);

  useEffect(() => {
    if (!selected) return;
    setDiff(null);
    client.send({
      type: "get_file_diff",
      repo_id: activeRepoId,
      path: selected,
      against: null,
    });
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (
        detail.type !== "file_diff" ||
        detail.repo_id !== activeRepoId ||
        detail.path !== selected
      )
        return;
      setDiff(detail.diff);
    };
    window.addEventListener("rt:file_diff", handler);
    return () => window.removeEventListener("rt:file_diff", handler);
  }, [activeRepoId, selected, client]);

  return (
    <ResizableSplit
      storageKey="source-control.changes"
      defaultSize={260}
      minSize={180}
      direction="vertical"
    >
      <div
        className="git-list source-control-list"
        data-testid="source-control-changes-list"
      >
        {!changes ? (
          <p className="empty">loading…</p>
        ) : changes.length === 0 ? (
          <p className="empty">working tree clean</p>
        ) : (
          <ul className="list">
            {changes.map((c) => (
              <li
                key={c.path}
                className={
                  c.path === selected ? "list-item selected" : "list-item"
                }
                onClick={() => setSelected(c.path)}
                data-testid="source-control-changes-row"
              >
                <span className={`file-status status-${c.status}`}>
                  {c.status}
                </span>
                <span className="list-item-label" title={c.path}>
                  {c.path}
                </span>
              </li>
            ))}
          </ul>
        )}
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
      </div>
      <DiffView diff={diff} testId="source-control-changes-diff" />
    </ResizableSplit>
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
  // The global sidebar has no specific branch context (the focused-pane's
  // branch may not match the repo's primary HEAD), so open the repo home
  // and let the forge route from there.
  void openInShell(remote.web_url);
}
