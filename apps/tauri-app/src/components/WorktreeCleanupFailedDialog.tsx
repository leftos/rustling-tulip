import { useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type {
  RetryWorktreeTarget,
  WorktreeCleanupFailure,
  WorktreeLockingProcess,
} from "../types";
import { useAutoFocus, useEscape, useFocusReturn } from "../utils/a11y";

interface Props {
  sessionId: string;
  sessionLabel: string;
  failures: WorktreeCleanupFailure[];
  /// User picked "Kill processes & retry". Caller sends
  /// `retry_worktree_cleanup` with the selected pids + every failure's
  /// `(member_path, repo_path)` pair, then dismisses this dialog.
  onKillAndRetry: (kill_pids: number[], targets: RetryWorktreeTarget[]) => void;
  /// User picked "Ignore". Caller just closes the dialog — the daemon
  /// has nothing to clean up server-side (the discard already happened).
  onIgnore: () => void;
}

/// Surfaced when `discard_session` (or its retry) couldn't fully
/// remove one or more member worktree dirs — typically because a
/// build / dev-server grandchild of the killed claude process kept
/// the directory pinned via open file handles.
///
/// On Windows the daemon's Restart Manager probe populates
/// `locking_processes` per failure; on other platforms that array is
/// empty and the dialog falls back to a generic "couldn't remove"
/// message with just the path + the underlying error.
export default function WorktreeCleanupFailedDialog({
  sessionId,
  sessionLabel,
  failures,
  onKillAndRetry,
  onIgnore,
}: Props) {
  const ignoreRef = useRef<HTMLButtonElement | null>(null);
  useEscape(onIgnore);
  useAutoFocus(ignoreRef);
  useFocusReturn();

  // Unique pids across all failures, each defaulting to "selected" so
  // the user just hits Kill & retry to act on every locker.
  const allPids = useMemo(() => {
    const seen = new Set<number>();
    const list: WorktreeLockingProcess[] = [];
    for (const f of failures) {
      for (const p of f.locking_processes) {
        if (!seen.has(p.pid)) {
          seen.add(p.pid);
          list.push(p);
        }
      }
    }
    return list;
  }, [failures]);

  const [selectedPids, setSelectedPids] = useState<Set<number>>(
    () => new Set(allPids.map((p) => p.pid)),
  );

  const togglePid = (pid: number) => {
    setSelectedPids((prev) => {
      const next = new Set(prev);
      if (next.has(pid)) next.delete(pid);
      else next.add(pid);
      return next;
    });
  };

  const onClickKillAndRetry = () => {
    const targets: RetryWorktreeTarget[] = failures.map((f) => ({
      member_path: f.member_path,
      repo_path: f.repo_path,
    }));
    onKillAndRetry(Array.from(selectedPids), targets);
  };

  const onClickOpenFolder = (memberPath: string) => {
    // Parent of the member dir — the wt.<branch>/ wrapper — so the
    // user can see siblings if it's a workspace session.
    const parent = parentDir(memberPath);
    void invoke("reveal_in_explorer", { path: parent }).catch(() => {
      // The Tauri command logs failures; nothing to do here.
    });
  };

  const hasLockers = allPids.length > 0;

  return (
    <div
      className="modal-backdrop modal-backdrop-destructive"
      data-testid="worktree-cleanup-failed-dialog"
    >
      <div
        className="modal worktree-cleanup-failed-modal"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-label={`Worktree cleanup failed for ${sessionLabel}`}
        data-session-id={sessionId}
      >
        <header className="modal-header">
          <h2>Worktree didn't clean up</h2>
          <button
            type="button"
            className="link"
            onClick={onIgnore}
            aria-label="Close"
            data-testid="worktree-cleanup-failed-close"
          >
            ✕
          </button>
        </header>
        <div className="modal-body">
          <p>
            Session <strong>{sessionLabel}</strong> stopped, but{" "}
            {failures.length === 1
              ? "its worktree directory is still on disk"
              : `${failures.length} of its worktree directories are still on disk`}
            . On Windows this usually means a child process (a dev server,
            a build, your editor) still has files open inside it.
          </p>
          <div className="worktree-cleanup-failures">
            {failures.map((f) => (
              <div
                key={f.member_path}
                className="worktree-cleanup-failure"
                data-testid="worktree-cleanup-failure-row"
              >
                <div className="worktree-cleanup-failure-header">
                  <code className="path">{f.member_path}</code>
                  <button
                    type="button"
                    className="link small"
                    onClick={() => onClickOpenFolder(f.member_path)}
                  >
                    Open folder
                  </button>
                </div>
                <div className="worktree-cleanup-failure-reason muted small">
                  {f.reason}
                </div>
                {f.locking_processes.length > 0 && (
                  <ul className="worktree-cleanup-failure-processes">
                    {f.locking_processes.map((p) => (
                      <LockingProcessRow
                        key={p.pid}
                        proc={p}
                        selected={selectedPids.has(p.pid)}
                        onToggle={() => togglePid(p.pid)}
                      />
                    ))}
                  </ul>
                )}
              </div>
            ))}
          </div>
          {!hasLockers && (
            <p className="muted small">
              The Restart Manager couldn't identify any specific process
              holding these dirs open. The "Kill processes & retry"
              option below will just re-run the cleanup — sometimes a
              transient antivirus or indexer hold clears on the second
              try.
            </p>
          )}
        </div>
        <footer className="modal-footer pane-close-dialog-footer">
          <button
            ref={ignoreRef}
            type="button"
            onClick={onIgnore}
            data-testid="worktree-cleanup-failed-ignore"
          >
            Ignore
          </button>
          <button
            type="button"
            className="danger"
            onClick={onClickKillAndRetry}
            data-testid="worktree-cleanup-failed-retry"
          >
            {hasLockers
              ? selectedPids.size === allPids.length
                ? `Kill ${selectedPids.size} process${selectedPids.size === 1 ? "" : "es"} & retry`
                : selectedPids.size === 0
                  ? "Retry without killing"
                  : `Kill ${selectedPids.size} selected & retry`
              : "Retry cleanup"}
          </button>
        </footer>
      </div>
    </div>
  );
}

function LockingProcessRow({
  proc,
  selected,
  onToggle,
}: {
  proc: WorktreeLockingProcess;
  selected: boolean;
  onToggle: () => void;
}) {
  const cmdline = proc.cmdline ?? "";
  // Truncate to ~80 chars in the row; full string is in the title attr.
  const truncated =
    cmdline.length > 80 ? `${cmdline.slice(0, 77)}…` : cmdline;
  return (
    <li className="worktree-cleanup-locker">
      <label className="worktree-cleanup-locker-label">
        <input
          type="checkbox"
          checked={selected}
          onChange={onToggle}
          data-testid={`worktree-cleanup-locker-pid-${proc.pid}`}
        />
        <span className="worktree-cleanup-locker-name">{proc.name}</span>
        <span className="worktree-cleanup-locker-pid muted small">
          pid {proc.pid}
        </span>
      </label>
      {cmdline && (
        <div
          className="worktree-cleanup-locker-cmdline muted small"
          title={cmdline}
        >
          {truncated}
        </div>
      )}
    </li>
  );
}

function parentDir(p: string): string {
  // Handle both Windows backslashes and forward slashes. We don't have
  // a cross-platform path lib in the renderer, but the daemon only
  // ever sends absolute paths, so the last separator is always present.
  const idx = Math.max(p.lastIndexOf("\\"), p.lastIndexOf("/"));
  return idx > 0 ? p.slice(0, idx) : p;
}
