import { useEffect, useState } from "react";
import { listStashes, type DaemonClient } from "../../api";
import type { DaemonMessage, GitStash } from "../../types";

interface Props {
  repoId: string;
  // `null` = the registered repo's main tree. `string` = a session worktree
  // path; all stash mutations target that worktree's index, and event
  // filtering compares this exact value to the `worktree_path` field on
  // incoming `stashes` messages.
  worktreePath: string | null;
  client: DaemonClient;
}

/**
 * Collapsible STASHES section for the source-control sidebar.
 *
 * Loads the current stash list on mount + on every repo change. Daemon-side
 * stash mutations broadcast a fresh `stashes` snapshot via the `state_events`
 * channel, so the list refreshes automatically without explicit re-requests.
 *
 * Each stash row exposes Pop / Apply / Drop actions; the Push button + input
 * sit in the section header so a user can stash without leaving the sidebar.
 */
export default function StashesSection({ repoId, worktreePath, client }: Props) {
  const [stashes, setStashes] = useState<GitStash[] | null>(null);
  const [collapsed, setCollapsed] = useState(true);
  const [pushMessage, setPushMessage] = useState("");
  const [pending, setPending] = useState<string | null>(null);

  useEffect(() => {
    setStashes(null);
    setPending(null);
    void listStashes(client, repoId, worktreePath).then((list) => setStashes(list));
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (
        detail.type !== "stashes" ||
        detail.repo_id !== repoId ||
        (detail.worktree_path ?? null) !== worktreePath
      )
        return;
      setStashes(detail.stashes);
      setPending(null);
    };
    window.addEventListener("rt:stashes", handler);
    return () => window.removeEventListener("rt:stashes", handler);
  }, [client, repoId, worktreePath]);

  const sendPush = () => {
    setPending("stash_push");
    client.send({
      type: "stash_push",
      repo_id: repoId,
      message: pushMessage.trim(),
      worktree_path: worktreePath,
    });
    setPushMessage("");
  };
  const sendPop = (stashId: string) => {
    setPending("stash_pop");
    client.send({
      type: "stash_pop",
      repo_id: repoId,
      stash_id: stashId,
      worktree_path: worktreePath,
    });
  };
  const sendApply = (stashId: string) => {
    setPending("stash_apply");
    client.send({
      type: "stash_apply",
      repo_id: repoId,
      stash_id: stashId,
      worktree_path: worktreePath,
    });
  };
  const sendDrop = (stashId: string) => {
    setPending("stash_drop");
    client.send({
      type: "stash_drop",
      repo_id: repoId,
      stash_id: stashId,
      worktree_path: worktreePath,
    });
  };

  const count = stashes?.length ?? 0;
  const isLoading = stashes === null;

  return (
    <section
      className="stashes-section"
      data-testid="source-control-stashes-section"
    >
      <div
        className="stashes-header"
        onClick={() => setCollapsed((c) => !c)}
        role="button"
        tabIndex={0}
        aria-expanded={!collapsed}
        aria-label={`Stashes (${count})`}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            setCollapsed((c) => !c);
          }
        }}
        data-testid="source-control-stashes-toggle"
      >
        <span className="tree-caret" aria-hidden="true">
          {collapsed ? "▸" : "▾"}
        </span>
        <span className="bucket-label">Stashes</span>
        <span className="bucket-count">{count}</span>
      </div>
      {!collapsed && (
        <div className="stashes-body">
          <div className="stash-push-row">
            <input
              type="text"
              className="stash-push-input"
              value={pushMessage}
              onChange={(e) => setPushMessage(e.target.value)}
              placeholder="Message (optional)"
              aria-label="Stash message"
              data-testid="source-control-stash-push-message"
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  sendPush();
                }
              }}
            />
            <button
              type="button"
              className="stash-push-button"
              onClick={sendPush}
              disabled={pending === "stash_push"}
              data-testid="source-control-stash-push"
            >
              {pending === "stash_push" ? "Stashing…" : "Stash"}
            </button>
          </div>
          {isLoading ? (
            <p className="empty">loading…</p>
          ) : count === 0 ? (
            <p className="empty">no stashes</p>
          ) : (
            <ul
              className="stash-list"
              data-testid="source-control-stash-list"
            >
              {stashes.map((s) => (
                <li
                  key={s.id}
                  className="stash-row"
                  data-testid="source-control-stash-row"
                  data-stash-id={s.id}
                >
                  <div className="stash-meta">
                    <span className="stash-id">{s.id}</span>
                    <span className="stash-subject" title={s.subject}>
                      {s.subject}
                    </span>
                  </div>
                  <div className="stash-actions">
                    <button
                      type="button"
                      className="link"
                      onClick={() => sendPop(s.id)}
                      disabled={pending !== null}
                      aria-label={`Pop ${s.id}`}
                      data-testid="source-control-stash-pop"
                    >
                      pop
                    </button>
                    <button
                      type="button"
                      className="link"
                      onClick={() => sendApply(s.id)}
                      disabled={pending !== null}
                      aria-label={`Apply ${s.id}`}
                      data-testid="source-control-stash-apply"
                    >
                      apply
                    </button>
                    <button
                      type="button"
                      className="link danger"
                      onClick={() => sendDrop(s.id)}
                      disabled={pending !== null}
                      aria-label={`Drop ${s.id}`}
                      data-testid="source-control-stash-drop"
                    >
                      drop
                    </button>
                  </div>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
    </section>
  );
}
