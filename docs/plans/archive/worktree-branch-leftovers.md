# Random worktree names must never land on a leftover branch

Shipped 2026-09-06. Follow-up split out of
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

## Decisions (user-confirmed 2026-09-06)

- **The daemon picks random names.** `SuggestBranchName` draws from the
  same word pool but rejects any name present in `refs/heads` or (prefix
  stripped) `refs/remotes` of every member repo. The dialog seed, the dice
  button, and launch-last all ask the daemon; the client-side generator is
  deleted. No suffix: names stay `wt/<adjective>-<noun>`; a numeric suffix
  is used only when all 256 are taken.
- **Non-dialog spawns refuse leftovers.** New
  `WorktreeReusePolicy::RefuseLeftover` (additive, not the serde default):
  create fresh, and error with an `ActionFailed` modal naming the branch,
  its tip, and how far it trails the base when either the worktree dir or
  the branch already exists. Launch-last and preset launches send it. The
  dialog keeps sending an explicit `reuse` / `recreate_from_base` because
  its collision panel is always shown first. `duplicate_session` replays
  the source's stored policy untouched.

## Wire protocol (additive, no version bump)

- `WorktreeReusePolicy::RefuseLeftover` (`refuse_leftover`).
- `ClientMessage::SuggestBranchName { target: SuggestTarget }` where
  `SuggestTarget` is tagged on `kind`: `repo { repo_id }` |
  `workspace { workspace_id }` | `#[serde(other)] unknown`.
- `DaemonMessage::BranchNameSuggestion { target: SuggestTarget, name }`.

## Steps

- [x] **Daemon + protocol.** `branch_names.rs` owns the word pool,
      `taken_names` (local + prefix-stripped remote refs across repos), and
      `pick_free_name` with an injectable RNG. `SuggestBranchName` handler.
      `RefuseLeftover` enforced in `materialize_single_worktree` and as a
      pre-pass in `workspace::ensure_branches` (no member is created when
      any member has a leftover); the refusal is a `SpawnFailure` with tip
      and staleness from `spawn_plan::fork_point`. Presets send
      `RefuseLeftover`.
- [x] **Frontend.** TS mirror. `randomName.ts` deleted. The spawn dialog's
      branch field and dice button request a suggestion per target and
      cache the reply; launch-last requests one, then spawns with
      `worktree_reuse: "refuse_leftover"`.
- [x] **E2E.** Suggestion avoids an existing branch (fixture with 255 of
      the 256 names taken → the free one is suggested). A `spawn_session`
      with `refuse_leftover` onto a branch-only leftover yields
      `action_failed`. Existing specs that read the seeded branch field
      wait for the async suggestion.
- [x] **E2E doctor version check** (user request 2026-09-06): `pnpm doctor`
      in `tools/e2e` only checks PATH presence, so a `msedgedriver` behind
      the installed Edge/WebView2 major fails every spec with
      `session not created`. Doctor must compare the driver's major version
      with the installed Edge/WebView2 and fail with the fix command. Also
      document `pnpm run doctor`: bare `pnpm doctor` is pnpm 10's builtin
      command and never reaches the script.
- [x] Ship as ONE commit and push straight to `main` (user-authorized
      2026-09-06), moving this file to `docs/plans/completed/` in it.
