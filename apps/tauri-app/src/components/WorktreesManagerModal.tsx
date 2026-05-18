import { useCallback, useEffect, useMemo, useState } from "react";
import type { DaemonClient } from "../api";
import type {
  DaemonMessage,
  RootWorktreeEntry,
  RootWorktreeStatus,
} from "../types";
import { useEscape, useFocusReturn } from "../utils/a11y";
import { logToFile } from "../utils/logger";
import Icon from "./Icon";

interface Props {
  client: DaemonClient | null;
  worktreesRoot: { path: string; isOverride: boolean } | null;
  onClose: () => void;
}

interface SnapshotState {
  root: string;
  isOverride: boolean;
  entries: RootWorktreeEntry[];
}

interface DeleteConfirmState {
  kind: "single" | "bulk";
  // Paths the daemon will receive as `delete_worktree_at` targets. For
  // bulk this is the set of Stale entries at confirm-open time.
  paths: string[];
  // Display summary for the confirm dialog: total bytes + entry count.
  totalBytes: number;
  entryCount: number;
}

/// Worktrees management modal. Lists every `wt.<branch>/` group under
/// the daemon's worktrees root, cross-referenced against the session
/// registry, and offers per-row + bulk Delete. Backed by the
/// `inspect_worktrees_root` / `delete_worktree_at` protocol pair; the
/// daemon pushes a fresh `worktrees_root_snapshot` after every
/// successful delete which this component re-renders from.
export default function WorktreesManagerModal({
  client,
  worktreesRoot,
  onClose,
}: Props) {
  useEscape(onClose);
  useFocusReturn();

  const [snapshot, setSnapshot] = useState<SnapshotState | null>(null);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<DeleteConfirmState | null>(null);

  // Initial fetch + every reconnect.
  useEffect(() => {
    if (!client) return;
    setPending(true);
    setError(null);
    client.send({ type: "inspect_worktrees_root" });
  }, [client]);

  // Subscribe to snapshot replies. The daemon also pushes one after every
  // successful delete; both code paths land here.
  useEffect(() => {
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "worktrees_root_snapshot") return;
      setSnapshot({
        root: detail.root,
        isOverride: detail.is_override,
        entries: detail.entries,
      });
      setPending(false);
    };
    window.addEventListener("rt:worktrees_root_snapshot", handler);
    return () =>
      window.removeEventListener("rt:worktrees_root_snapshot", handler);
  }, []);

  // Daemon errors arrive on the generic `error` message; surface them so
  // a refused delete (active session) doesn't fail silently.
  useEffect(() => {
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "error") return;
      // Only consume messages that look like ours. The generic "error"
      // channel is shared across the app — leave others alone.
      if (
        detail.message.includes("worktree") ||
        detail.message.includes("worktrees")
      ) {
        setError(detail.message);
        setPending(false);
      }
    };
    window.addEventListener("rt:error", handler);
    return () => window.removeEventListener("rt:error", handler);
  }, []);

  const onRefresh = useCallback(() => {
    if (!client) return;
    setPending(true);
    setError(null);
    client.send({ type: "inspect_worktrees_root" });
  }, [client]);

  const onConfirmDelete = useCallback(() => {
    if (!client || !confirm) return;
    setPending(true);
    setError(null);
    for (const path of confirm.paths) {
      client.send({ type: "delete_worktree_at", path });
    }
    logToFile(
      "info",
      `worktrees_manager: requested delete of ${confirm.paths.length} ` +
        `group${confirm.paths.length === 1 ? "" : "s"}`,
    );
    setConfirm(null);
  }, [client, confirm]);

  const staleEntries = useMemo(
    () =>
      snapshot?.entries.filter((e) => e.status.kind === "stale") ?? [],
    [snapshot],
  );

  const onAskBulkDelete = useCallback(() => {
    if (staleEntries.length === 0) return;
    const totalBytes = staleEntries.reduce(
      (sum, e) => sum + (e.size_bytes ?? 0),
      0,
    );
    setConfirm({
      kind: "bulk",
      paths: staleEntries.map((e) => e.path),
      totalBytes,
      entryCount: staleEntries.length,
    });
  }, [staleEntries]);

  const onAskDeleteOne = useCallback((entry: RootWorktreeEntry) => {
    setConfirm({
      kind: "single",
      paths: [entry.path],
      totalBytes: entry.size_bytes ?? 0,
      entryCount: 1,
    });
  }, []);

  const rootLabel =
    snapshot?.root ?? worktreesRoot?.path ?? "(daemon not connected)";
  const isOverride = snapshot?.isOverride ?? worktreesRoot?.isOverride ?? false;

  return (
    <div
      className="modal-backdrop modal-backdrop-preview"
      data-testid="worktrees-manager-modal"
    >
      <div
        className="modal modal-wide"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-label="Manage worktrees"
      >
        <header className="modal-header">
          <h2>Manage worktrees</h2>
          <button
            type="button"
            className="modal-close"
            onClick={onClose}
            aria-label="Close dialog"
            title="Close"
          >
            <Icon name="close" />
          </button>
        </header>
        <div className="modal-body">
          <p className="settings-section-hint">
            Root: <code>{rootLabel}</code>
            {isOverride ? " — user override" : " — default"}
          </p>
          <div className="settings-row" style={{ gap: "0.5rem" }}>
            <button
              type="button"
              onClick={onRefresh}
              disabled={!client || pending}
              data-testid="worktrees-manager-refresh"
            >
              Refresh
            </button>
            <button
              type="button"
              className="danger"
              onClick={onAskBulkDelete}
              disabled={!client || pending || staleEntries.length === 0}
              data-testid="worktrees-manager-bulk-delete"
            >
              Delete all stale ({staleEntries.length})
            </button>
          </div>
          {error !== null && (
            <p className="error-text" data-testid="worktrees-manager-error">
              {error}
            </p>
          )}
          {snapshot === null && pending && (
            <p>Scanning worktrees root…</p>
          )}
          {snapshot !== null && snapshot.entries.length === 0 && (
            <p data-testid="worktrees-manager-empty">
              No managed worktrees under this root.
            </p>
          )}
          {snapshot !== null && snapshot.entries.length > 0 && (
            <ul
              className="worktrees-list"
              data-testid="worktrees-manager-list"
            >
              {snapshot.entries.map((entry) => (
                <WorktreeRow
                  key={entry.path}
                  entry={entry}
                  onDelete={() => onAskDeleteOne(entry)}
                  disabled={!client || pending}
                />
              ))}
            </ul>
          )}
        </div>
        <footer className="modal-footer">
          <button
            type="button"
            className="primary"
            onClick={onClose}
            data-testid="worktrees-manager-close"
          >
            Close
          </button>
        </footer>
      </div>
      {confirm !== null && (
        <DeleteConfirmDialog
          confirm={confirm}
          onCancel={() => setConfirm(null)}
          onConfirm={onConfirmDelete}
        />
      )}
    </div>
  );
}

interface RowProps {
  entry: RootWorktreeEntry;
  onDelete: () => void;
  disabled: boolean;
}

function WorktreeRow({ entry, onDelete, disabled }: RowProps) {
  const statusLabel = humanStatus(entry.status);
  const sizeLabel =
    entry.size_bytes === null ? "—" : humanSize(entry.size_bytes);
  const mtimeLabel =
    entry.last_modified_unix === null
      ? "—"
      : humanRelativeTime(entry.last_modified_unix);
  const deleteDisabled =
    disabled || entry.status.kind === "active";
  return (
    <li className="worktrees-row" data-testid="worktrees-manager-row">
      <div className="worktrees-row-header">
        <span className={`worktrees-status worktrees-status-${entry.status.kind}`}>
          {statusLabel}
        </span>
        <code className="worktrees-row-path">{entry.path}</code>
      </div>
      <div className="worktrees-row-meta">
        <span>Branch: <code>{entry.branch_slug}</code></span>
        <span>Size: {sizeLabel}</span>
        <span>Modified: {mtimeLabel}</span>
        {entry.session_id !== null && (
          <span>Session: <code>{entry.session_id}</code></span>
        )}
      </div>
      {entry.members.length > 0 && (
        <details className="worktrees-row-members">
          <summary>{entry.members.length} member{entry.members.length === 1 ? "" : "s"}</summary>
          <ul>
            {entry.members.map((m) => (
              <li key={m.worktree_path}>
                <code>{m.repo_name_hint}</code>
                {m.repo_path !== null ? (
                  <> ← <code>{m.repo_path}</code></>
                ) : (
                  <> (repo unreachable — will fall back to fs delete)</>
                )}
              </li>
            ))}
          </ul>
        </details>
      )}
      <div className="worktrees-row-actions">
        <button
          type="button"
          className="danger"
          onClick={onDelete}
          disabled={deleteDisabled}
          title={
            entry.status.kind === "active"
              ? "Stop the active session before deleting"
              : "Delete this worktree group"
          }
          data-testid="worktrees-manager-row-delete"
        >
          Delete
        </button>
      </div>
    </li>
  );
}

interface ConfirmProps {
  confirm: DeleteConfirmState;
  onCancel: () => void;
  onConfirm: () => void;
}

function DeleteConfirmDialog({ confirm, onCancel, onConfirm }: ConfirmProps) {
  useEscape(onCancel);
  const message =
    confirm.kind === "single"
      ? "Delete this worktree group?"
      : `Delete ${confirm.entryCount} stale worktree group${confirm.entryCount === 1 ? "" : "s"}?`;
  const sizeNote =
    confirm.totalBytes > 0
      ? ` Frees ~${humanSize(confirm.totalBytes)} on disk.`
      : "";
  return (
    <div
      className="modal-backdrop"
      data-testid="worktrees-manager-confirm"
    >
      <div
        className="modal"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
      >
        <header className="modal-header">
          <h2>Confirm delete</h2>
        </header>
        <div className="modal-body">
          <p>
            {message}
            {sizeNote}
          </p>
          <p className="settings-section-hint">
            The daemon will run <code>git worktree remove --force</code> per
            member where the originating repo is reachable, and fall back
            to filesystem delete otherwise.
          </p>
        </div>
        <footer className="modal-footer">
          <button
            type="button"
            onClick={onCancel}
            data-testid="worktrees-manager-confirm-cancel"
          >
            Cancel
          </button>
          <button
            type="button"
            className="danger primary"
            onClick={onConfirm}
            data-testid="worktrees-manager-confirm-ok"
          >
            Delete
          </button>
        </footer>
      </div>
    </div>
  );
}

function humanStatus(status: RootWorktreeStatus): string {
  switch (status.kind) {
    case "active":
      return "Active";
    case "detached":
      return "Detached";
    case "stale":
      return "Stale";
    case "unknown":
      return "Unknown";
  }
}

function humanSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const kb = bytes / 1024;
  if (kb < 1024) return `${kb.toFixed(1)} KB`;
  const mb = kb / 1024;
  if (mb < 1024) return `${mb.toFixed(1)} MB`;
  const gb = mb / 1024;
  return `${gb.toFixed(2)} GB`;
}

function humanRelativeTime(unixSeconds: number): string {
  const nowSec = Math.floor(Date.now() / 1000);
  const delta = nowSec - unixSeconds;
  if (delta < 60) return `${delta}s ago`;
  if (delta < 3600) return `${Math.floor(delta / 60)}m ago`;
  if (delta < 86400) return `${Math.floor(delta / 3600)}h ago`;
  if (delta < 604800) return `${Math.floor(delta / 86400)}d ago`;
  const date = new Date(unixSeconds * 1000);
  return date.toISOString().slice(0, 10);
}
