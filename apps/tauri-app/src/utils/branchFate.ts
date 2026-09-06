import type { BranchFate, MemberBranchFate } from "../types";

/// Which button set the delete-worktree confirm dialog offers.
///
/// - `delete_all`: every branch the daemon would touch already landed, so a
///   single "delete worktree and branch" button is safe.
/// - `choose`: at least one branch holds work that landed nowhere the daemon
///   checked (or carries a fate this build can't read), so the user picks
///   between keeping and deleting. `lostCommits` is what "delete" destroys,
///   or null when any contributing count is unknown.
/// - `worktree_only`: no branch is eligible for deletion at all; only the
///   worktree goes.
export type DiscardChoice =
  | { kind: "delete_all" }
  | { kind: "choose"; lostCommits: number | null }
  | { kind: "worktree_only" };

/// One sentence explaining what a discard does to this member's branch,
/// shown next to the repo name and branch in the confirm dialog.
export function describeMemberFate(m: MemberBranchFate): string {
  const fate = m.fate;
  switch (fate.kind) {
    case "will_delete":
      return describeWillDelete(fate.into, fate.via);
    case "kept_by_default":
      return describeKept(fate.unique_commits, fate.checked_against);
    case "untouched":
      return describeUntouched(fate.reason);
    default:
      return "unknown state; left alone unless you choose delete";
  }
}

function describeWillDelete(into: string, via: string): string {
  switch (via) {
    case "patch_equivalent":
      return `already in ${into} (cherry-picked or rebased)`;
    case "ancestry":
      return `already merged into ${into}`;
    default:
      return `already in ${into}`;
  }
}

function describeKept(
  uniqueCommits: number | null,
  checkedAgainst: string[],
): string {
  if (uniqueCommits === null) {
    return "couldn't determine whether its commits landed";
  }
  const commits = uniqueCommits === 1 ? "1 commit" : `${uniqueCommits} commits`;
  if (checkedAgainst.length === 0) {
    return `${commits} not found anywhere the daemon checked`;
  }
  return `${commits} not in ${checkedAgainst.join(", ")}`;
}

function describeUntouched(reason: string): string {
  switch (reason) {
    case "external_worktree":
      return "not managed by rustling-tulip; left alone";
    case "checked_out_elsewhere":
      return "checked out in another worktree; left alone";
    case "branch_missing":
      return "branch no longer exists";
    default:
      return "left alone";
  }
}

/// True for a `kind` this build doesn't recognize. Such a member is treated
/// as "might hold work" — it forces the explicit keep/delete choice and
/// makes the commit count unknown.
function isUnknownFate(fate: BranchFate): boolean {
  return (
    fate.kind !== "will_delete" &&
    fate.kind !== "kept_by_default" &&
    fate.kind !== "untouched"
  );
}

function needsExplicitChoice(fate: BranchFate): boolean {
  return fate.kind === "kept_by_default" || isUnknownFate(fate);
}

/// Commits a "delete branch" would destroy across every member, or null when
/// any member's contribution can't be counted.
function lostCommitCount(members: MemberBranchFate[]): number | null {
  let total = 0;
  for (const { fate } of members) {
    if (isUnknownFate(fate)) return null;
    if (fate.kind !== "kept_by_default") continue;
    if (fate.unique_commits === null) return null;
    total += fate.unique_commits;
  }
  return total;
}

/// Reduce every member's fate to the one decision the dialog has to ask for.
/// An empty member list means nothing is deletable, same as all-untouched.
export function discardChoice(members: MemberBranchFate[]): DiscardChoice {
  if (members.some((m) => needsExplicitChoice(m.fate))) {
    return { kind: "choose", lostCommits: lostCommitCount(members) };
  }
  if (members.some((m) => m.fate.kind === "will_delete")) {
    return { kind: "delete_all" };
  }
  return { kind: "worktree_only" };
}
