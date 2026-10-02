# `.git` filesystem watcher

## Context

Phase 6 (the git tracking layer) shipped a `RepoStatus` model that the
source-control sidebar uses to render STAGED + CHANGES buckets and the
STASHES list. Every refresh today is **user-action triggered** — the
sidebar re-requests on tab switch, on focus change, or after the daemon
broadcasts a fresh status following its own write (`StateEvent::RepoStatus`
fan-out from `handle_git_write` / `handle_stash_write`).

External tools (terminal `git commit`, an editor's "save", `git stash` in a
sibling terminal, another rustling-tulip Claude session committing through
its PTY) bypass the daemon entirely, so the sidebar happily shows stale
state until the user clicks something. The watcher closes that gap: any
mutation under a registered repo's tree triggers a debounced `git status` +
`git stash list` refresh, broadcast through the existing
`StateEvent::RepoStatus` + `StateEvent::Stashes` channels.

## Goals

- Live `Changes` view for every registered repo, no manual refresh.
- Live `Stashes` section ditto.
- Survives the noisy directories (`node_modules`, `target`, `.venv`) without
  the daemon pegging a core. Debounce coalesces bursts; `git status` itself
  respects `.gitignore` so the broadcast payload is always clean even when
  the watcher fires on an ignored path.

## Out of scope

- Live history (`Commits`) refresh. The history view is paginated and
  user-driven; auto-refresh there is a re-scroll hazard. Manual refresh via
  tab switch keeps Phase E pagination simple.
- Watching the file diff for an already-open diff tab. Monaco tabs are
  snapshot views, not live ones. Reopen-to-refresh is fine.
- Cross-process locking against the daemon's own writes. Our writes already
  broadcast through `handle_git_write` / `handle_stash_write`; the watcher
  will see them again and trigger a duplicate refresh, but it's cheap and
  the broadcast is idempotent in the client. (If profiling shows the dup
  refresh wastes work, we can short-circuit via an
  `Arc<AtomicU64> last_self_write_ts` per repo.)

## Library choice

`notify` 8.2 + `notify-debouncer-full` 0.7 — `notify-debouncer-full` adds
event coalescing on top of `notify`'s raw OS-level events with a tunable
debounce window. Both are the standard pick in the Rust ecosystem;
`notify-debouncer-mini` exists but `-full` keeps event kinds so we can
filter cheap. Windows uses `ReadDirectoryChangesW`; macOS uses FSEvents;
Linux uses inotify.

## Design

```
crates/daemon/src/git_watch.rs (new)
   GitWatch { handles: HashMap<repo_id, RepoWatcher> }
   RepoWatcher = { _debouncer, _stopper: oneshot::Sender<()> }
   ::start(state, hub.state_events, dirs) -> spawns one task that subscribes
       to StateEvent::Repos to keep `handles` in sync with the registry,
       creating/dropping watchers as repos are added/removed.
```

Per-repo flow on a watcher event burst:

1. Debouncer fires `Vec<DebouncedEvent>` after `~250ms` of quiescence.
2. Skip if every event sits under `<repo>/.git/objects/` or `<repo>/.git/logs/`
   — those mutate on `git fetch`/`git gc` without changing
   user-visible status. (Optimization; defer if it complicates the diff.)
3. Run `git_inspect::repo_status(&repo)` and `git_write::stash_list(&repo)`
   in parallel.
4. Broadcast `StateEvent::RepoStatus` and `StateEvent::Stashes`. The
   existing per-client `spawn_state_forwarder` task fans both out.

If the refresh fails (e.g. the repo was just removed mid-debounce), log and
continue — the watcher keeps running until it's explicitly removed.

## Critical files

- `crates/daemon/Cargo.toml` — add `notify = "8"`, `notify-debouncer-full = "0.7"`.
- `crates/daemon/src/git_watch.rs` (new) — `GitWatch` lifecycle + per-repo
  debouncer task.
- `crates/daemon/src/server.rs::run` — instantiate `GitWatch`, seed with
  initial repo list, subscribe to `state_events.subscribe()` so it picks
  up adds/removes.
- `crates/daemon/src/main.rs` or `lib.rs` — module declaration only.

No protocol changes; no frontend changes. The sidebar already listens for
`rt:repo_status` and `rt:stashes` events broadcast from the same
`StateEvent` channel.

## Verification

- [x] **Repo registration plumbing** — when a repo is added, a watcher is
      created; when removed, the watcher's debouncer task is stopped.
- [x] **Status refresh on file change** — manually edit a tracked file in a
      registered repo, observe the sidebar's CHANGES bucket update within
      ~500ms without user action.
- [x] **Stash refresh on `git stash push`** — run `git stash push` in an
      external terminal, observe the sidebar's STASHES section update.
- [x] **Noisy directories don't pin the daemon** — touching files under
      `node_modules/` should still trigger a refresh, but the result is
      filtered by `.gitignore` so the broadcast payload is unchanged
      (verified by inspecting the broadcast `index_changes` / `worktree_changes`
      vecs for stability).
- [x] `cargo clippy --all-targets --all-features -- -D warnings` clean.
- [x] `cargo test --workspace` green.

## Tasks

- [x] Add `notify` + `notify-debouncer-full` deps to daemon.
- [x] Create `crates/daemon/src/git_watch.rs` with `GitWatch` + the per-repo
      debounced refresh task.
- [x] Wire into `server::run` after the `Hub` is constructed.
- [x] Subscribe to `StateEvent::Repos` so adds/removes drive the watcher
      lifecycle (no separate add-repo / remove-repo hook needed).
- [x] Smoke-test by manually dirtying a registered repo and watching the
      sidebar refresh.
