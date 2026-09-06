import type { BranchCleanup, SessionSnapshot } from "../types";

/// The quit-time walk that asks for one branch fate per per-session
/// worktree before the bulk stop + discard runs. Held in app state while
/// the confirm dialog is open on `pending[0]`.
export interface ExitWorktreeQueue {
  /// Session ids still waiting for an answer, in prompt order.
  pending: string[];
  /// Answers collected so far, keyed by session id. A session with no
  /// entry is discarded under "auto".
  choices: Record<string, BranchCleanup>;
  /// How many sessions the walk started with. Fixed for the life of the
  /// queue so the "n of m" hint doesn't renumber when a session vanishes.
  total: number;
}

/// The sessions a quit-time "remove worktrees" would actually take a
/// worktree from: still running, reachable by the daemon, and owning a
/// per-session worktree. Stopped, orphaned, and worktree-less sessions are
/// still stopped by the shutdown, they just have nothing to ask about.
export function worktreeSessionsForExit(
  sessions: SessionSnapshot[],
): SessionSnapshot[] {
  return sessions.filter(
    (s) => s.status !== "stopped" && !s.is_orphan && s.has_per_session_worktree,
  );
}

/// Open a walk over every worktree session. Null when there is nothing to
/// ask about, which means the caller can shut down straight away.
export function startExitQueue(
  sessions: SessionSnapshot[],
): ExitWorktreeQueue | null {
  const targets = worktreeSessionsForExit(sessions);
  if (targets.length === 0) return null;
  return {
    pending: targets.map((s) => s.id),
    choices: {},
    total: targets.length,
  };
}

/// Store one session's answer and take it out of the queue.
export function recordExitChoice(
  queue: ExitWorktreeQueue,
  sessionId: string,
  branch: BranchCleanup,
): ExitWorktreeQueue {
  return {
    pending: queue.pending.filter((id) => id !== sessionId),
    choices: { ...queue.choices, [sessionId]: branch },
    total: queue.total,
  };
}

/// Drop pending sessions the daemon no longer reports — another window
/// discarded them, or they ended while the walk was open. They keep no
/// entry in `choices`, so the shutdown treats them as "auto".
export function skipVanished(
  queue: ExitWorktreeQueue,
  liveIds: Set<string>,
): ExitWorktreeQueue {
  return {
    pending: queue.pending.filter((id) => liveIds.has(id)),
    choices: { ...queue.choices },
    total: queue.total,
  };
}

/// 1-based position of the session being asked about, for the dialog's
/// "Session n of m" hint.
export function exitProgress(queue: ExitWorktreeQueue): {
  index: number;
  total: number;
} {
  return { index: queue.total - queue.pending.length + 1, total: queue.total };
}
