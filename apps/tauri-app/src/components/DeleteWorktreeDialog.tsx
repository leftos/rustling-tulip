import { useEffect, useMemo, useRef, useState } from "react";
import type { DaemonClient } from "../api";
import type {
  BranchCleanup,
  DaemonMessage,
  MemberBranchFate,
  SessionSnapshot,
} from "../types";
import { useAutoFocus, useEscape, useFocusReturn } from "../utils/a11y";
import type { DiscardChoice } from "../utils/branchFate";
import { describeMemberFate, discardChoice } from "../utils/branchFate";
import { sessionDisplayLabel } from "../utils/sessionLabel";

/// How long to wait for the daemon's `discard_preview` before falling back to
/// the explicit keep/delete choice with no branch information.
const PREVIEW_TIMEOUT_MS = 10_000;

interface Props {
  session: SessionSnapshot;
  client: DaemonClient;
  /// 1-based position when the caller walks several sessions in a row.
  /// Renders "Session {index} of {total}" in the header; omitted otherwise.
  progress?: { index: number; total: number };
  /// Which backdrop stacking layer to paint on. "exit-queue" is for the
  /// quit-time walk, whose prompts open from inside the exit dialog and so
  /// have to sit above it.
  layer?: "destructive" | "exit-queue";
  onCancel: () => void;
  /// The user's answer. The caller owns the close_pane / stop_session /
  /// discard_session sends that follow.
  onConfirm: (branch: BranchCleanup) => void;
}

type DialogMode = "loading" | "choose" | "delete_all" | "worktree_only";

/// Confirmation modal for every "delete worktree" gesture. Asks the daemon
/// what a discard would do to each member's branch, then offers either a
/// single delete button (everything already landed) or the explicit
/// keep/delete pair with the commit count at risk. The safe option is
/// autofocused so a stray Enter never destroys unlanded work.
export default function DeleteWorktreeDialog({
  session,
  client,
  progress,
  layer = "destructive",
  onCancel,
  onConfirm,
}: Props) {
  const [members, setMembers] = useState<MemberBranchFate[] | null>(null);
  const [timedOut, setTimedOut] = useState(false);
  const cancelRef = useRef<HTMLButtonElement | null>(null);
  const keepRef = useRef<HTMLButtonElement | null>(null);

  useEscape(onCancel);
  useFocusReturn();

  useEffect(() => {
    client.send({ type: "preview_discard", session_id: session.id });
  }, [client, session.id]);

  useEffect(() => {
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "discard_preview") return;
      if (detail.session_id !== session.id) return;
      setMembers(detail.members);
      setTimedOut(false);
    };
    window.addEventListener("rt:discard_preview", handler);
    return () => window.removeEventListener("rt:discard_preview", handler);
  }, [session.id]);

  useEffect(() => {
    if (members !== null) return;
    const timer = window.setTimeout(() => {
      setMembers([]);
      setTimedOut(true);
    }, PREVIEW_TIMEOUT_MS);
    return () => window.clearTimeout(timer);
  }, [members]);

  const choice = useMemo<DiscardChoice | null>(
    () => (members === null ? null : discardChoice(members)),
    [members],
  );
  const mode = resolveMode(choice, timedOut);
  const lostCommits =
    !timedOut && choice?.kind === "choose" ? choice.lostCommits : null;

  useAutoFocus(cancelRef, mode !== "choose");
  useAutoFocus(keepRef, mode === "choose");

  const label = sessionDisplayLabel(session);
  return (
    <div
      className={`modal-backdrop modal-backdrop-${layer}`}
      data-testid="delete-worktree-dialog"
    >
      <div
        className="modal"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-label={`Delete worktree for session ${label}`}
      >
        <header className="modal-header">
          <h2>Delete worktree?</h2>
          {progress && (
            <span className="muted small">
              Session {progress.index} of {progress.total}
            </span>
          )}
          <button
            type="button"
            className="link"
            onClick={onCancel}
            aria-label="Cancel"
            data-testid="delete-worktree-dialog-close"
          >
            ✕
          </button>
        </header>
        <div className="modal-body">
          <p>
            Removing the worktree for <strong>{label}</strong>.
          </p>
          <DialogBody members={members} timedOut={timedOut} />
        </div>
        <footer className="modal-footer pane-close-dialog-footer">
          <button
            ref={cancelRef}
            type="button"
            onClick={onCancel}
            data-testid="delete-worktree-cancel"
          >
            Cancel
          </button>
          <DialogActions
            mode={mode}
            lostCommits={lostCommits}
            keepRef={keepRef}
            onConfirm={onConfirm}
          />
        </footer>
      </div>
    </div>
  );
}

function resolveMode(
  choice: DiscardChoice | null,
  timedOut: boolean,
): DialogMode {
  if (timedOut) return "choose";
  if (choice === null) return "loading";
  return choice.kind;
}

/// Danger-button label for the choose mode — the commit count the delete
/// would destroy, or an explicit admission that it's unknown.
function deleteAndBranchLabel(lostCommits: number | null): string {
  if (lostCommits === null) {
    return "Delete worktree and branch (commit count unknown)";
  }
  if (lostCommits === 1) {
    return "Delete worktree and branch (loses 1 commit)";
  }
  return `Delete worktree and branch (loses ${lostCommits} commits)`;
}

function DialogBody({
  members,
  timedOut,
}: {
  members: MemberBranchFate[] | null;
  timedOut: boolean;
}) {
  if (members === null) {
    return (
      <p className="muted small" data-testid="delete-worktree-dialog-loading">
        Checking branch state…
      </p>
    );
  }
  return (
    <>
      {timedOut && (
        <p className="muted small" data-testid="delete-worktree-dialog-timeout">
          Couldn't determine branch state.
        </p>
      )}
      <div className="delete-worktree-members">
        {members.map((m) => (
          <div
            key={m.repo_id}
            className="delete-worktree-member"
            data-testid="delete-worktree-member"
          >
            <span className="delete-worktree-member-repo">{m.repo_name}</span>
            <code>{m.branch}</code>
            <span className="delete-worktree-member-fate">
              {describeMemberFate(m)}
            </span>
          </div>
        ))}
      </div>
    </>
  );
}

function DialogActions({
  mode,
  lostCommits,
  keepRef,
  onConfirm,
}: {
  mode: DialogMode;
  lostCommits: number | null;
  keepRef: React.RefObject<HTMLButtonElement | null>;
  onConfirm: (branch: BranchCleanup) => void;
}) {
  if (mode === "loading") return null;
  if (mode === "worktree_only") {
    return (
      <button
        type="button"
        className="danger"
        onClick={() => onConfirm("keep")}
        data-testid="delete-worktree-only"
      >
        Delete worktree
      </button>
    );
  }
  if (mode === "delete_all") {
    return (
      <button
        type="button"
        className="danger"
        onClick={() => onConfirm("delete")}
        data-testid="delete-worktree-and-branch"
      >
        Delete worktree and branch
      </button>
    );
  }
  return (
    <>
      <button
        ref={keepRef}
        type="button"
        onClick={() => onConfirm("keep")}
        data-testid="delete-worktree-keep-branch"
      >
        Delete worktree, keep branch
      </button>
      <button
        type="button"
        className="danger"
        onClick={() => onConfirm("delete")}
        data-testid="delete-worktree-and-branch"
      >
        {deleteAndBranchLabel(lostCommits)}
      </button>
    </>
  );
}
