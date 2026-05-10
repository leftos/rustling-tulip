import { useEffect, useState } from "react";
import { open as openInShell } from "@tauri-apps/plugin-shell";
import type { DaemonClient } from "../api";
import type {
  DaemonMessage,
  GitCommit,
  GitFileChange,
  GitRemoteUrl,
  SessionMember,
} from "../types";

interface Props {
  members: SessionMember[];
  client: DaemonClient;
}

type Tab = "changes" | "history";

const COMMIT_LIMIT = 50;

export default function GitPanel({ members, client }: Props) {
  const [tab, setTab] = useState<Tab>("changes");
  const [activeRepoId, setActiveRepoId] = useState<string>(
    members[0]?.repo_id ?? "",
  );

  return (
    <div className="git-panel">
      <header className="git-tabs">
        <button
          type="button"
          className={tab === "changes" ? "tab active" : "tab"}
          onClick={() => setTab("changes")}
        >
          Changes
        </button>
        <button
          type="button"
          className={tab === "history" ? "tab active" : "tab"}
          onClick={() => setTab("history")}
        >
          History
        </button>
        {members.length > 1 && (
          <select
            value={activeRepoId}
            onChange={(e) => setActiveRepoId(e.target.value)}
            className="repo-picker"
          >
            {members.map((m) => (
              <option key={m.repo_id} value={m.repo_id}>
                {m.repo_name}
              </option>
            ))}
          </select>
        )}
      </header>
      {tab === "changes" ? (
        <ChangesView
          members={members}
          activeRepoId={activeRepoId}
          client={client}
        />
      ) : (
        <HistoryView
          activeRepoId={activeRepoId}
          activeMember={members.find((m) => m.repo_id === activeRepoId)}
          client={client}
        />
      )}
    </div>
  );
}

// ---------- Changes ----------

function ChangesView({
  members,
  activeRepoId,
  client,
}: {
  members: SessionMember[];
  activeRepoId: string;
  client: DaemonClient;
}) {
  const member = members.find((m) => m.repo_id === activeRepoId);
  const [changes, setChanges] = useState<GitFileChange[] | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [diff, setDiff] = useState<string | null>(null);

  useEffect(() => {
    if (!activeRepoId) return;
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
    if (!selected || !activeRepoId) return;
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
    <div className="git-split">
      <div className="git-list">
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
        {member && (
          <div className="git-meta">
            {member.repo_name} · {member.branch}
            <button
              type="button"
              className="link"
              onClick={() => openInForge(client, activeRepoId, member.branch)}
            >
              open in forge ↗
            </button>
          </div>
        )}
      </div>
      <DiffView diff={diff} />
    </div>
  );
}

// ---------- History ----------

function HistoryView({
  activeRepoId,
  activeMember,
  client,
}: {
  activeRepoId: string;
  activeMember: SessionMember | undefined;
  client: DaemonClient;
}) {
  const [commits, setCommits] = useState<GitCommit[] | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [detailDiff, setDetailDiff] = useState<string | null>(null);

  useEffect(() => {
    if (!activeRepoId) return;
    setCommits(null);
    setSelected(null);
    setDetailDiff(null);
    client.send({
      type: "list_commits",
      repo_id: activeRepoId,
      branch: activeMember?.branch ?? null,
      limit: COMMIT_LIMIT,
    });
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "commits" || detail.repo_id !== activeRepoId) return;
      setCommits(detail.commits);
    };
    window.addEventListener("rt:commits", handler);
    return () => window.removeEventListener("rt:commits", handler);
  }, [activeRepoId, activeMember?.branch, client]);

  useEffect(() => {
    if (!selected || !activeRepoId) return;
    setDetailDiff(null);
    client.send({
      type: "get_commit",
      repo_id: activeRepoId,
      sha: selected,
    });
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
    <div className="git-split">
      <div className="git-list">
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
      <DiffView diff={detailDiff} />
    </div>
  );
}

function DiffView({ diff }: { diff: string | null }) {
  if (diff === null) {
    return <div className="diff-pane empty">select an item</div>;
  }
  if (diff.length === 0) {
    return <div className="diff-pane empty">(empty diff)</div>;
  }
  return (
    <pre className="diff-pane">
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
  if (line.startsWith("+++") || line.startsWith("---") || line.startsWith("diff "))
    return "diff-meta";
  if (line.startsWith("@@")) return "diff-hunk";
  if (line.startsWith("+")) return "diff-add";
  if (line.startsWith("-")) return "diff-del";
  return "";
}

function openInForge(client: DaemonClient, repoId: string, _branch: string) {
  client.send({ type: "get_remote_url", repo_id: repoId });
  const handler = (ev: Event) => {
    window.removeEventListener("rt:remote_url", handler);
    const detail = (ev as CustomEvent<GitRemoteUrl & { type: "remote_url" }>)
      .detail;
    if (detail.repo_id !== repoId) return;
    if (detail.web_url) {
      void openInShell(detail.web_url);
    } else {
      console.warn("unrecognized forge for", detail.raw_url);
    }
  };
  window.addEventListener("rt:remote_url", handler);
}
