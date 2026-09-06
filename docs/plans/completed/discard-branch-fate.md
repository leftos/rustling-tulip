# Discard: branch fate prompt + patch-equivalence merge check

Shipped 2026-09-06.

## The gap

"Close session and delete worktree" reaps the session branch only when
`git branch -d` accepts it, i.e. when the branch tip is an ancestor of its
upstream (or of the primary checkout's `HEAD` when no upstream is set).
Work that landed by cherry-pick, squash, or rebase has different commit
hashes, so the branch is always kept, and nothing in the UI says which way
it will go. The user has to find out from `daemon.log` or from the next
spawn's collision prompt.

## Decisions (user-confirmed 2026-09-06)

- **"Landed" targets:** the session's resolved base branch plus its
  remote-tracking counterpart when one exists (e.g. `main` and
  `origin/main`). Not every local branch.
- **Merged test:** ancestry (`merge-base --is-ancestor`) OR patch
  equivalence (`git cherry <target> <branch>` prints no `+` lines) against
  any target. A conflict-edited cherry-pick still reads as unique, which
  errs safe. Squash merges will not match; the prompt exists for that case.
- **One shared confirm modal.** Every "delete worktree" click routes into a
  single `DeleteWorktreeDialog` that asks the daemon for each member's
  branch fate and then offers either "Delete worktree and branch" (all
  merged) or the explicit pair "Delete worktree, keep branch" /
  "Delete worktree and branch" (anything unmerged).
- **No unprompted path survives.** The stopped-pane overlay button and the
  quit-time "Stop sessions, remove worktrees" both go through the modal;
  quit walks the worktree sessions one at a time and cancel aborts the
  quit.

## Wire protocol (additive, no version bump)

- `CleanupAction.branch: BranchCleanup` with `#[serde(default)]`;
  `enum BranchCleanup { Auto, Keep, Delete, #[serde(other)] Unknown }`.
  `Auto` (the default and the meaning of `Unknown`) is today's rule
  upgraded to the patch-equivalence check. `Keep` never touches the branch.
  `Delete` is `git branch -D`.
- `ClientMessage::PreviewDiscard { session_id }` →
  `DaemonMessage::DiscardPreview { session_id, members: Vec<MemberBranchFate> }`.
- `MemberBranchFate { repo_id, repo_name, branch, fate: BranchFate }`,
  `BranchFate` tagged on `kind`:
  - `will_delete { into: String, via: MergeEvidence }` where
    `MergeEvidence = ancestry | patch_equivalent`.
  - `kept_by_default { unique_commits: Option<u32>, checked_against: Vec<String> }`
    — `None` when git failed and the count is unknown.
  - `untouched { reason: UntouchedReason }` with
    `external_worktree | checked_out_elsewhere | branch_missing`; the
    daemon will not delete the branch under any `BranchCleanup`.
  - `#[serde(other)] unknown`.

## Steps

- [x] **Daemon + protocol.** `git::branch_merge_status` replaces
      `delete_branch_if_merged` (the `-d` rule is gone, not kept alongside).
      Targets resolved from the session's stored `base_branch` through the
      same path `spawn_plan::resolve_base_for_create` uses. `discard_session`
      honours `BranchCleanup`. New `PreviewDiscard` handler. Unit tests for
      ancestry-merged, cherry-picked, remote-only-landed, unmerged-with-count,
      checked-out-elsewhere, and the three `BranchCleanup` arms.
- [x] **TS mirror + dialog + routing.** `types.ts` / `api.ts` mirror.
      `DeleteWorktreeDialog` component (loading state, per-member rows,
      button set by fate, safe option autofocused). `App.tsx` owns the
      request: a `rt:request_delete_worktree` event carrying
      `{ sessionId, closePane?, stopFirst }` opens the modal; confirm performs
      close_pane → stop_session → discard_session with the chosen
      `BranchCleanup`. Pane-close dialog, the three context-menu entries, and
      the stopped-pane overlay button all dispatch that event instead of
      sending `discard_session` themselves.
- [x] **Quit flow.** "Stop sessions, remove worktrees" collects a
      `BranchCleanup` per worktree session through the same modal (`n of m`
      hint), then runs today's bulk stop + discard with those choices. Cancel
      returns to the exit dialog.
- [x] **E2E.** `pane-close-stop`, `session-stop-lifecycle`, and
      `exit-confirm-worktrees` updated for the modal. New assertions: fresh
      branch → "Delete worktree and branch"; unique commit → keep/delete pair
      with the count; cherry-picked onto main → single delete button with the
      patch-equivalent wording.
- [x] Move this file to `docs/plans/completed/` in the shipping commit.

## Also shipped

- [x] Spawn dialog base-branch field does not populate (reported
      2026-09-06). Cause: both base fields (`spawn-single-base-branch`,
      `spawn-workspace-base-branch`) are a raw `<input list>` + `<datalist>`,
      and Chromium filters datalist options by the current text, so once the
      field is seeded with `origin/main` the only match is `origin/main`
      itself. The Branch field already uses `BranchCombobox`, built for
      exactly this failure. Fix: render both base fields with
      `BranchCombobox` (`branches = [...remoteBranches, ...knownBranches]`,
      `allowCreate={false}`; the workspace field today lists remotes only,
      so add its local branches too), delete both `<datalist>`s.

## Follow-up

The random-name collision that makes leftover branches dangerous in the
first place is tracked in `docs/plans/worktree-branch-leftovers.md`.
