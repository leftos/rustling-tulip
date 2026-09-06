# Duplicate spawns a worktree session on its own fresh branch

Shipped 2026-09-06. Closes the gap left by
`docs/plans/completed/worktree-branch-leftovers.md`.

## The gap

`DuplicateSession` replays the source's stored `SpawnConfig` verbatim:
same branch name, and a reuse policy that `with_reuse_policy_reset`
always pins to `Reuse`. For a worktree session that means:

- Duplicating a **running** worktree session fails with "worktree in use"
  because the clone targets the same directory the source still holds.
- Duplicating a **stopped or parked** worktree session binds the clone to
  the source's worktree, and if that directory was removed by hand while
  the branch stayed, the clone attaches the branch-only leftover at its
  stale tip with no prompt. This is the only replay path that still
  bypasses `RefuseLeftover`.

In-place duplicates (`use_worktree = false`) target a real branch by the
user's choice and are unaffected.

## Decision

A worktree duplicate is "another session like this one", not "another
process in the same checkout". The daemon gives it a fresh name from
`branch_names::suggest` and spawns it with `RefuseLeftover`, exactly the
way launch-last does from the client. In-place duplicates keep replaying
the stored branch. The shift-click "duplicate with dialog" path is
unchanged: it prefills the dialog, whose collision panel already handles
an existing name.

## Steps

- [x] **Daemon.** `SpawnConfig::to_clone_request` stays as the verbatim
      clone for resume. New `SpawnConfig::to_duplicate_request(fresh_branch:
      Option<String>)` in the protocol crate: for `Single`/`Workspace` with
      `use_worktree`, substitute the branch and set `RefuseLeftover`;
      otherwise identical to the clone. The `DuplicateSession` handler
      resolves the suggest target from the stored target (repo or
      workspace id), asks `branch_names::suggest`, and spawns. Doc comments
      on `DuplicateSession` and `with_reuse_policy_reset` updated. Unit
      tests: worktree single/workspace get the fresh name + policy; in-place
      and standalone are untouched.
- [x] **E2E.** New `session-duplicate.spec.ts`: duplicating a running
      worktree plain-shell session over ws yields a second running session
      whose branch is a fresh `wt/<adj>-<noun>` different from the source
      and whose worktree dir exists; duplicating an in-place session keeps
      the branch. Also a branch-only leftover under the source's name does
      not affect the duplicate.
- [x] Ship as ONE commit and push straight to `main` (user-authorized
      2026-09-06), moving this file to `docs/plans/completed/` in it.
