# Launch a session into an existing worktree

Today a worktree can only be reached by *deriving* it: you name a branch, the
daemon computes `<worktrees-root>/wt.<branch-slug>/<anchor>/<rel>`, and if that
directory happens to exist already the reuse policy binds to it. That works
right up until the derivation stops matching reality — which is exactly the
case for a worktree left behind by a session that is gone.

A worktree whose group no session references is already a first-class daemon
concept: `RootWorktreeStatus::Stale`. The Manage Worktrees modal lists those
groups with size and last-modified. The only thing you can do with one is
delete it.

This plan makes such a worktree launchable: address it by **path**, not by
branch name, from both the Manage Worktrees modal and the spawn dialog.

## Why path-addressing rather than branch names

Branch-name reuse silently does the wrong thing whenever the worktree's
checked-out branch no longer slugs back to its own directory name:

```
group dir:      wt.feature-foo/X/dev/repo
HEAD inside it: main                       (the user switched branches in there)

spawn { branch_name: "main", use_worktree: true }
  -> derives wt.main/X/dev/repo
  -> that directory does not exist
  -> creates a SECOND worktree, leaving the first untouched
```

Detached HEAD has no branch name to send at all. Path-addressing sidesteps
both, and additionally makes hand-made worktrees outside the RT root
launchable for free.

## Decisions

Settled with the user before implementation:

| Question | Decision |
|---|---|
| Entry points | Manage Worktrees modal row action **and** the spawn dialog picker. No sidebar context-menu entry. |
| Addressing | Path-pinned spawn target carrying the concrete worktree path(s). |
| Launchable groups | All, **including Active** — a second agent in a live tree is allowed, behind a confirm. |
| Workspace member mismatch | Fill the gaps: members present in the group are pinned, missing members get a worktree created at the derived path (same group dir), extra directories ignored. |
| Single-repo picker scope | All of the repo's worktrees (today's behavior), with RT-managed ones annotated. |
| Unregistered originating repo | Launch disabled with a reason. No silent auto-registration. |
| Modal Launch behavior | Opens the spawn dialog pre-filled and pinned, so agent / mode / model / prompt stay chooseable. |

## Protocol (v22)

Pinning is additive; the `WorktreeInfo` change is not, hence the bump.
`supported` stays a singleton `[22]`, matching the current posture.

```rust
/// A workspace member bound to a specific existing worktree directory.
pub struct PinnedMemberWorktree { pub repo_id: String, pub path: String }

SpawnTarget::Single {
    …,
    #[serde(default)] existing_worktree: Option<String>,
}

SpawnTarget::Workspace {
    …,
    #[serde(default)] existing_worktrees: Vec<PinnedMemberWorktree>,
}
```

Semantics of a pinned member:

- Requires `use_worktree: true`. The pinned + in-place combination is
  contradictory and is rejected with a clear error rather than silently
  ignoring one of the two.
- The daemon skips path derivation **and** worktree creation; `cwd` is the
  pinned path verbatim.
- A pinned path that no longer exists is a hard error naming the path. It is
  not resurrected — a replay of a config whose tree was deleted should say so.
- The pinned path bypasses the `active_sessions_using_worktree` refusal (the
  user was shown the Active status and confirmed), logged at `warn!`.
- `SessionMember::branch` comes from the pinned worktree's actual HEAD, so the
  sidebar shows the branch that is really checked out. Detached HEAD falls back
  to the group's branch slug.
- The pin **survives** into the persisted `SpawnConfig`. Unlike
  `WorktreeReusePolicy::RecreateFromBase` — a one-shot answer about one
  collision — a pin is a deliberate choice of target, so Resume, Duplicate, and
  "launch last again" all belong back in the same tree.

`WorktreeInfo::is_active: bool` is replaced by `status: RootWorktreeStatus`
(`is_active` was exactly `status == Active`, and the picker needs the
stale-vs-detached distinction), plus RT-group annotations populated only for
worktrees under the RT root: `group_path`, `size_bytes`, `last_modified_unix`.

`RootWorktreeEntry` gains `launch: Option<WorktreeLaunchTarget>` and
`launch_blocked_reason: Option<String>`, where `WorktreeLaunchTarget` is an
internally-tagged enum over `Single` / `Workspace` with a `#[serde(other)]
Unknown` catch-all.

## Tasks

### Protocol

- [x] `PinnedMemberWorktree`, `existing_worktree`, `existing_worktrees`
- [x] `WorktreeInfo`: `is_active` → `status`, plus group annotations
- [x] `RootWorktreeEntry`: `launch` + `launch_blocked_reason`
- [x] `WorktreeLaunchTarget` with `#[serde(other)] Unknown`
- [x] Bump `protocol-version.json` to 22, `supported: [22]`
- [x] TS mirrors in `types.ts` / `api.ts`

### Daemon

- [x] `spawn_single`: pinned branch — validate, skip create, skip in-use guard,
      read real HEAD for the member
- [x] `ResolvedMember` gains `pinned: bool`; `resolve_workspace` applies the
      pins and derives the rest; `ensure_branches` skips creation for pinned
      members
- [x] `spawn_workspace`: in-use guard applies to unpinned members only
- [x] Reject pinned + `use_worktree: false`
- [x] `worktrees_admin::scan_root` resolves a launch target per group
      (member `repo_path` → registered repo; multi-member → the workspace
      containing them) and sets a blocked reason otherwise
- [x] `ListWorktrees` annotates its reply from the same scan

### Frontend

- [x] Manage Worktrees modal: per-row Launch, disabled with the blocked reason,
      confirm when the group is Active
- [x] App: route the modal's Launch into a pinned spawn prefill
- [x] Spawn dialog single form: annotated picker, pin by path, suppress
      branch / base / dice / fork-point preview in existing mode
- [x] Spawn dialog workspace form: the New/Existing toggle and group picker it
      has never had

### Tests

- [x] Unit: pinned single spawn (happy path, missing path, in-place rejection)
- [x] Unit: workspace fill-the-gaps — pinned members untouched, missing member
      created in the same group dir
- [x] Unit: launch-target resolution — single, workspace, unregistered repo,
      partial workspace overlap
- [x] E2E: spawn with a worktree → discard the session record → the group reads
      Stale → Launch from the modal → the new session's `worktree_path` equals
      the original
