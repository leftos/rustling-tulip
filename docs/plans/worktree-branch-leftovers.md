# Random worktree names must never land on a leftover branch

Reported 2026-09-06. Follow-up split out of
`docs/plans/completed/discard-branch-fate.md`.

## The gap

The name pool in `apps/tauri-app/src/utils/randomName.ts` is 16 × 16 = 256
`wt/<adjective>-<noun>` combinations with no existence check, so a leftover
branch is re-picked after a few dozen sessions. The spawn dialog surfaces
that collision (Sep 1), but "launch last" (`App.tsx`
`randomWorktreeBranchName()` call) spawns without the dialog under the
default `Reuse` policy, and `workspace::ensure_branches` then attaches the
branch-only leftover at its stale tip with nothing but an `info!` line. The
user and their agents then work on months-old code until someone checks the
branch.

The discard-side fix (patch-equivalence reap plus the keep/delete prompt)
shrinks the pool of leftovers but does not remove it: a branch deliberately
kept is still one random draw away from being attached again.

## Steps

- [ ] Generated names skip existing refs: the daemon picks the name
      (`SuggestBranchName { repo_id | workspace_id }`) against
      `refs/heads` + `refs/remotes` of every member, and widens the pool
      with a short random suffix.
- [ ] Non-dialog spawns (launch last, duplicate, presets) that arrive with
      a branch-only leftover and `Reuse` are refused with a
      `BranchLeftoverCollision`-style error naming the tip and staleness,
      instead of silently attaching. A dialog-confirmed spawn keeps today's
      explicit reuse/recreate choice.
