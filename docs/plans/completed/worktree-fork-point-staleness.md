# Worktree fork-point staleness

Shipped 2026-07-30.

## The bug

A session spawned onto a fresh branch could fork from a `main` that was
hundreds of commits behind the remote, with nothing in the UI or the logs
saying so. The branch looked fresh; work that had landed in between was
simply missing, and it surfaced later as "why is this file gone".

Two independent causes, both silent:

1. **The base ref was a local branch name.** `spawn_single` resolved a base
   (explicit → persisted `repo.default_branch` → detection → current branch →
   `"main"`) and handed it straight to `git worktree add -b <new> <path>
   <base>`. Bare `main` resolves against `refs/heads/` first, so it always
   meant the *local* ref. The daemon never ran `git fetch`, and
   `list_branches` only enumerated `refs/heads/`, so `origin/main` was never
   offered in the picker either. In a repo driven entirely through worktrees
   the local default never advances — every session forks off it, works, and
   pushes to the remote — so it sits wherever it was last checked out by hand.

2. **Worktree reuse ignored the base entirely.** Both `spawn_single` and
   `workspace::ensure_branches` skipped the add when a directory already
   existed at the target path (`if worktree_path.exists() { … }`), binding to
   whatever fork point that worktree was created with. The
   `active_sessions_using_worktree` guard only rejected worktrees held by
   *live* sessions, so a dead session's leftover directory was reused with no
   signal.

## What shipped

- [x] `git.rs` primitives: `fetch` (bounded by a 20s timeout, forced
      non-interactive so a credential prompt can't wedge the per-repo lock,
      `kill_on_drop` so an abandoned fetch is reaped), `list_remote_branches`,
      `remotes`, `full_ref_exists`, `remote_tracking_for`, `resolve_base_ref`,
      `ahead_behind`, `head_short_sha`, `delete_branch`.
- [x] `run_git` split into `run_git` / `run_git_network` over a shared inner
      so only network calls carry the hardened env.
- [x] `spawn_plan.rs`: `resolve_base_for_create` (single source of truth for
      the base chain, shared by `spawn_single` and `resolve_workspace`) and
      `fork_point` (the staleness measurements).
- [x] Auto-detected bases upgrade to their remote-tracking counterpart. An
      **explicit** caller value is honored verbatim — the dialog instead seeds
      the field with `origin/<default>` so the choice is visible and editable
      rather than applied invisibly.
- [x] Protocol (additive, no version bump): `PreviewSpawn` / `SpawnPreview`,
      `FetchRepo` / `RepoFetched`, `remote_branches` on `Branches`, seven
      fork-point fields on `MemberSpawnPreview`, and `WorktreeReusePolicy`
      threaded through both `SpawnTarget` variants.
- [x] `SpawnConfig::from_request` resets the reuse policy via
      `SpawnTarget::with_reuse_policy_reset`, so a one-shot "recreate" decision
      can't be replayed by duplicate / launch-last and delete a worktree
      nobody was asked about.
- [x] Spawns log their fork point (`head`, `behind_base`,
      `base_behind_remote`) — the one line that answers the original question.
- [x] Dialog: background fetch on open (never blocks a launch; a failure
      degrades to cached refs with a visible caveat), live debounced preview,
      remote refs in the base-branch datalist, a "N commits behind
      origin/main" callout, and a Reuse / Recreate-from-base choice on
      collision with a data-loss warning when the existing worktree is dirty.

## Follow-up: two bugs the e2e spec found

Adding `tools/e2e/tests/e2e/specs/spawn-fork-point.spec.ts` surfaced two
defects that unit tests and typecheck had both missed.

- [x] **The base-branch field clobbered user input.** `defaultBranch` settles
      asynchronously, and the background fetch revises it a *second* time a
      beat later. The seed effect had no notion of "the user has typed here",
      so a base branch typed right after opening the dialog was silently
      overwritten. Seeding now stops on first edit and resumes on a
      repo/workspace switch.
- [x] **`branch_exists` blanked out the staleness figures.** Both preview
      paths only resolved a base when the branch did *not* exist — but a
      leftover worktree always has its branch, so the collision notice could
      never say how stale that worktree was, in exactly the case the feature
      exists for. Worse, the workspace recreate path passed that empty base to
      `worktree add` *after* deleting the branch, which fails outright.
      `ResolvedMember` now carries `resolved_base` alongside the
      creation-only `effective_base`.

## Tests

`crates/daemon/src/git.rs` grows a real-git integration harness
(`init_stale_repo`) that builds a repo whose local `main` trails
`origin/main` by N commits — the exact shape of the bug. It covers remote
preference, the no-remote fallback, the `origin/origin/main` compounding
guard, ahead/behind counting, that a worktree based on `origin/main` skips
the stale local branch, and that the remove → delete-branch → re-add recreate
sequence actually moves the fork point (without the branch delete the re-add
fails on "branch already exists" and the stale worktree survives).

`workspace.rs` covers the multi-repo recreate path, which e2e does not reach:
recreating a member whose branch already exists forks from `resolved_base`,
and reuse leaves both HEAD and uncommitted work untouched.

`spawn-fork-point.spec.ts` drives the real app against a fixture repo whose
local `main` trails `origin/main`: the base field defaults to the remote ref,
a stale explicit base is reported with its behind-count, a leftover worktree
prompts rather than being silently reused, and choosing recreate leaves the
worktree level with `origin/main` (asserted with `git rev-list` against the
spawned session's actual worktree path, not against the UI's own claim).

## Deliberately not done

- No blocking fetch on the spawn path. Freshness is never worth an unbounded
  network stall on launch; the background fetch plus the visible staleness
  figure covers the same ground without the latency.
- In-place (`use_worktree: false`) checkouts keep using the local branch —
  you can't check out a remote-tracking ref in place without detaching HEAD.
