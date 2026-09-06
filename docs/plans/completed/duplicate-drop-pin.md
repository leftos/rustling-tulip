# Duplicate of a pinned session gets its own worktree

Shipped 2026-09-06. Closes the edge noted in
`docs/plans/completed/duplicate-fresh-branch.md`.

## The gap

`SpawnConfig::to_duplicate_request` substitutes a fresh branch and
`RefuseLeftover` for a worktree duplicate but leaves the pin
(`Single::existing_worktree`, `Workspace::existing_worktrees`) as cloned.
The daemon's pin path returns before the branch or policy is consulted,
so a duplicate of a pinned session still runs in the source's pinned
directory: two sessions in one checkout, and the fresh name is discarded.

## Decision

A worktree duplicate always gets its own branch and its own worktree
under the daemon's root. The pin is dropped along with the name
substitution; the clone forks from the stored `base_branch` like any
fresh spawn. Resume keeps the plain clone and its pins.

## Steps

- [x] **Protocol.** `to_duplicate_request` clears `existing_worktree` /
      `existing_worktrees` whenever it substitutes the branch. Tests:
      pinned single and pinned workspace lose their pins; in-place and
      `None` cases untouched.
- [x] **E2E.** In `session-duplicate.spec.ts`: pin a session to a worktree
      created by hand (`git worktree add`), duplicate it, assert the clone's
      branch is a pool name, its `worktree_path` differs from the pin and
      lives under the e2e worktrees root, and the source still runs.
- [x] Ship as ONE commit and push straight to `main` (user-authorized
      2026-09-06), moving this file to `docs/plans/completed/` in it.
