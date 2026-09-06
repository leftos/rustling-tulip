/// Branch-name suggestions come from the daemon: it draws a
/// `wt/<adjective>-<noun>` pair that exists in no member repo's local or
/// remote refs, so a spawn never lands on the leftover branch of a discarded
/// session. This module holds the request/reply plumbing the spawn surfaces
/// share — building the target, keying it, and recognizing the reply.

import type { DaemonMessage, SuggestTarget } from "../types";

/// How long a caller waits for `branch_name_suggestion` before giving up.
/// The daemon walks every member repo's refs to answer, so the budget is
/// generous; past it the caller stops showing a pending state.
export const BRANCH_SUGGESTION_TIMEOUT_MS = 10_000;

/// Whatever the spawn dialog's target picker produced: a repo or a workspace
/// and its id.
export interface TargetLike {
  kind: "repo" | "workspace";
  id: string;
}

/// Wire form of a target selection.
export function suggestTargetFor(sel: TargetLike): SuggestTarget {
  return sel.kind === "repo"
    ? { kind: "repo", repo_id: sel.id }
    : { kind: "workspace", workspace_id: sel.id };
}

/// Cache/route key for a target. Shared by the spawn dialog's per-target
/// suggestion cache and by launch-last's in-flight request map, so a reply
/// lands in the slot its request came from.
export function suggestTargetKey(t: SuggestTarget): string {
  return t.kind === "repo"
    ? `repo:${t.repo_id}`
    : `workspace:${t.workspace_id}`;
}

/// The suggested name when `msg` answers the request keyed by `key`, else
/// null. Every open spawn form sees every reply, so each one filters on its
/// own key rather than taking the first name that arrives.
export function matchesSuggestion(
  msg: DaemonMessage,
  key: string,
): string | null {
  if (msg.type !== "branch_name_suggestion") return null;
  return suggestTargetKey(msg.target) === key ? msg.name : null;
}
