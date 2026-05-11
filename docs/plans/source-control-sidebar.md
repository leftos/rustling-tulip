# VSCode-style Source Control sidebar

## Context

The current git view in rustling-tulip is a per-session in-pane toggle (`SessionPane.tsx:125-140` → `GitPanel.tsx`) that renders flat lists of changed files + commits with a raw `<pre>` diff. The user wants a VSCode-inspired sidebar with a path treeview, recent commit list, write actions (stage/unstage/commit/discard/stash), and a Monaco-based diff viewer.

The redesign moves Git from a session-scoped pane view to a global activity-bar-driven sidebar that follows the focused session by default. Diffs open as first-class tabs in the existing tab bar so they share the grid/split/popout plumbing with terminal panes. This is a meaningful protocol change (panes can now hold non-session content) and crosses every layer of the stack, so the plan is phased — each phase ships independently and leaves the app in a working state.

## Goals

- Activity bar (left rail) switches between **Sessions** sidebar (current) and **Source Control** sidebar (new).
- Source Control sidebar = repo header + CHANGES tree + STASHES list + GRAPH (commit list with pagination). Tracks the focused session's repo by default; manual repo override via dropdown.
- Clicking a file opens a side-by-side diff in a new tab using Monaco's `DiffEditor`. Diff tabs are draggable, splittable, and pop-out-able like terminal tabs.
- Full write actions: stage, unstage, discard (with confirm), commit, stash push/pop/apply/drop.
- Read-only `Terminal | Git` toggle in `SessionPane` is removed.

## Out of scope

- Live `.git` filesystem watcher (still user-action-refresh, per existing CLAUDE.md deferred list). Daemon broadcasts updates only after its own writes.
- Multi-branch graph rendering with lanes. Flat list with bullets + "load more" pagination only.
- Push / pull / fetch / branch creation / merge UI. These run from the terminal; sidebar surfaces nothing for remotes beyond the existing "Open in forge".
- Conflict resolution UI.
- Auto-update of the changes list when external tools modify files.

## Architecture

```
┌──┬───────────────────┬──────────────────────────────┐
│☰ │ SOURCE CONTROL    │   tab bar [Term 1][diff: App │
│⌘ ├───────────────────┤   ─────────────────────────  │
│  │ rustling-tulip ▾  │                              │
│  │   main*           │   Diff editor (Monaco)       │
│  ├───────────────────┤   old │ new                  │
│  │ ▼ STAGED   (2)    │                              │
│  │   M lib.rs       │   OR terminal grid           │
│  │ ▼ CHANGES  (14)   │                              │
│  │   ▾ apps/         │                              │
│  │     M App.tsx     │                              │
│  │   ▾ tools/        │                              │
│  │     U …           │                              │
│  │ Message…  [Commit]│                              │
│  ├───────────────────┤                              │
│  │ ▶ STASHES (3)     │                              │
│  ├───────────────────┤                              │
│  │ GRAPH             │                              │
│  │ ● add: wdio smoke │                              │
│  │ ● fix: spawn …    │                              │
│  │ [ load more ]     │                              │
└──┴───────────────────┴──────────────────────────────┘
```

### Pane-content extension (new in protocol)

The protocol currently encodes pane content as `GridNode::Pane { pane_id, session_id: Option<String> }` (`crates/protocol/src/lib.rs:263-280`). To let diff tabs share the grid with terminals, replace `session_id: Option<String>` with a tagged `PaneContent` enum:

```rust
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaneContent {
    Session { session_id: String },
    Diff {
        repo_id: String,
        path: String,
        against: Option<String>,  // None = working tree vs index, "HEAD"/sha otherwise
        // Optional label override; defaults to "diff: <basename>".
        label: Option<String>,
    },
}

pub enum GridNode {
    Pane { pane_id: String, content: Option<PaneContent> },
    Split { ... },
}
```

`PROTOCOL_VERSION` bumps `4 → 5`. State migration: `state.json` deserialize uses a serde `#[serde(default)]` shim — if it finds the old `session_id` field, treat it as `Some(PaneContent::Session { session_id })`. New writes only use the new shape; the shim runs once on first daemon start.

`SplitPane.new_session_id` and `ReplacePaneSession.session_id` are renamed/widened to accept `Option<PaneContent>`. A new client message `OpenDiffTab { repo_id, path, against }` is the sugar path used by the sidebar — daemon creates a new tab with one diff pane.

## Phased rollout

### Phase A — Activity bar + read-only sidebar shell

- [x] Add `apps/tauri-app/src/components/ActivityBar.tsx` (left rail, two icons: Sessions / Source Control). Persist active section in `localStorage` (key `rt.activity`).
- [x] Add `apps/tauri-app/src/components/source-control/` directory:
  - [x] `SourceControlSidebar.tsx` — root component, accepts `client`, `repos`, `focusedRepoId`. (No separate `RepoHeader.tsx` for Phase A — the header is inlined; will split out in Phase C when stage/unstage adds enough surface to justify it.)
  - [ ] `RepoHeader.tsx` — *deferred to Phase C*; iter 7 inlined the header into `SourceControlSidebar` since it was small and the file split was premature.
  - [ ] `ChangesTree.tsx` — *deferred to follow-up.* Iter 7 ships a flat list (same shape as the deleted `GitPanel`) so Phase A is functionally equivalent to the in-pane git view it replaces. Path-folded tree + status badges land in a "Phase A part 2" iter once the rest of the audit-triage settles.
  - [ ] `GraphList.tsx` — *deferred to Phase E* per its existing scope (pagination); iter 7 keeps the `COMMIT_LIMIT=50` cap inline in `SourceControlSidebar`'s HistoryView.
- [x] Refactor `App.tsx` layout: `<ActivityBar /> + <Sessions sidebar | SourceControlSidebar /> + <main grid>`.
- [x] Wire focused-pane → focused-repo: derives from active tab → focused pane → `pane.session_id` → `session.members[0].repo_id`. Override picker pins a repo when desired (`rt.sourceControl.repoOverride` in localStorage).
- [x] Remove `view: "git"` from `SessionPane.tsx`. Sessions only show Terminal / Events now.
- [x] Delete `GitPanel.tsx` entirely.
- [ ] Daemon `RepoStatus` extension: *deferred to Phase C* (the staging phase that needs the split). Iter 7 kept the existing `status: String` shape — sufficient for the read-only Phase A view.

### Phase B — Monaco diff editor + diff-tab protocol extension

- [ ] Bump `PROTOCOL_VERSION` 4 → 5.
- [ ] Define `PaneContent` enum in `crates/protocol/src/lib.rs` and update `GridNode::Pane`. Add the serde shim for old `session_id` field.
- [ ] Update `crates/daemon/src/tabs.rs` everywhere it constructs/manipulates `GridNode::Pane`. Tests in `lib.rs:982-1050`.
- [ ] Update `crates/daemon/src/server.rs` handlers: `SplitPane`, `ReplacePaneSession`, `MovePane`, `ExtractToNewTab`, `MergeTabs` — preserve diff content across moves.
- [ ] Add `ClientMessage::OpenDiffTab { repo_id, path, against }`. Handler creates a new tab with one diff pane via the same `make_tab` path. Daemon broadcasts `TabUpdated` like any other tab op.
- [ ] Update `apps/tauri-app/src/types.ts` and every grid/pane consumer:
  - [ ] `GridRenderer.tsx` reads `pane.content?.kind`. New branch for `kind === "diff"` renders `<DiffPane />`.
  - [ ] `apps/tauri-app/src/components/DiffPane.tsx` — wraps Monaco's `DiffEditor`. Sends `GetFileDiff` for the `against` revision and `git show <rev>:<path>` for the "old" content (new daemon endpoint `GetFileContent { repo_id, path, against }` returning the raw file at that ref).
  - [ ] `EmptyPane.tsx` and `SessionPane.tsx` are unchanged; the new path is purely additive at the renderer level.
- [ ] Bundle Monaco eagerly: install `monaco-editor` directly, configure Vite with `monaco-editor/esm/vs/editor/editor.api`. Theme inherits dark from CSS variables. Pre-load only the diff editor (not the full editor language workers) to keep bundle weight down where possible.
- [ ] Pop-out windows for diff tabs: reuse `open_session_window` plumbing in `apps/tauri-app/src-tauri/src/lib.rs` — generalize to `open_pane_window` that takes a `pane_id`. Old `open_session_window` becomes a wrapper.
- [ ] Sidebar click handler: `client.send({ type: "open_diff_tab", repo_id, path, against: null })` then on the resulting `TabUpdated` arrival, activate that tab.

### Phase C — Staging + commit

- [ ] New file `crates/daemon/src/git_write.rs` for write helpers. Each helper takes `repo: &Path` plus action-specific args and shells out to `git` (re-using the `run_git` pattern from `git_inspect.rs:12-31`, factored to a shared `git_cmd.rs`). Helpers:
  - `stage(paths)` → `git add -- <paths>`
  - `unstage(paths)` → `git restore --staged -- <paths>`
  - `commit(message)` → `git commit -m <message>` (let hooks run; do **not** pass `--no-verify`)
- [ ] New protocol messages (`ClientMessage`):
  - `StageFiles { repo_id, paths: Vec<String> }`
  - `UnstageFiles { repo_id, paths: Vec<String> }`
  - `Commit { repo_id, message: String }`
- [ ] Daemon responds with refreshed `RepoStatus` for the same `repo_id` so clients re-render automatically. On commit success, also push a fresh `Commits` snapshot for the active branch. Errors return `Error { message }`.
- [ ] **Broadcast on writes:** repo-status responses fan out via the broadcast channel (not per-client `out_tx`), so a pop-out window with the sidebar open sees changes from the main window. This matches the existing fix-it note in `docs/ux-audit.md:271-273`.
- [ ] `ChangesTree.tsx` splits into two sub-sections: STAGED (from `index_status != ' '`) and CHANGES (from `worktree_status != ' '`). Per-row `+`/`−` action buttons stage/unstage individual files; section header has bulk stage/unstage.
- [ ] Commit message input + Commit button live above the STAGED section. Button disabled when message empty OR no staged files. Submitting disables both until `RepoStatus` round-trips.

### Phase D — Discard + stash

- [ ] Write helpers: `discard(paths)` → `git restore -- <paths>` (worktree only, leaves index alone). For deleted-and-staged files, also `git restore --staged --worktree -- <paths>` to fully revert.
- [ ] `DiscardChanges { repo_id, paths }` client message. **Two-step confirm in the UI** (modal listing the files; "Discard N changes" red button + Cancel). Never auto-confirm.
- [ ] Stash helpers:
  - `stash_push(message)` → `git stash push -u -m <message>`
  - `stash_list()` → `git stash list --format=%H%x1f%gs%x1f%aI`
  - `stash_pop(stash_id)`, `stash_apply(stash_id)`, `stash_drop(stash_id)` → corresponding `git stash` subcommands
- [ ] Protocol: `StashPush { repo_id, message }`, `ListStashes { repo_id }`, `StashPop { repo_id, stash_id }`, `StashApply { repo_id, stash_id }`, `StashDrop { repo_id, stash_id }`. Daemon response `Stashes { repo_id, stashes: Vec<GitStash> }` where `GitStash { id, subject, created_at }`.
- [ ] `StashesSection.tsx` — collapsible section between CHANGES and GRAPH. Each stash row has a context menu (pop, apply, drop).

### Phase E — Graph pagination

- [ ] Extend `ListCommits` with `offset: u32` (default 0). Daemon adds `--skip` to the `git log` invocation.
- [ ] `GraphList.tsx` keeps an accumulating commit list. "Load more" button at the bottom requests the next batch. No hard cap.
- [ ] Clicking a commit opens a multi-file diff tab (Phase B path with `against: Some(sha)`). New pane content variant or a sibling tab content for "commit-diff" — defer until Phase E lands; for the first cut, clicking a commit can keep the existing flat-text detail view inside the sidebar.

## Critical files (touched across phases)

### Protocol
- `crates/protocol/src/lib.rs` — `PaneContent` enum, `PROTOCOL_VERSION`, all new message variants, `GitStash` type, extended `GitFileChange`. Add tests for the `session_id` → `PaneContent::Session` serde shim (mirror existing `grid_node_pane_omits_session_id_default` style at line 1022).

### Daemon
- `crates/daemon/src/git_inspect.rs` — split `RepoStatus` to two-char form; factor `run_git` into shared `git_cmd.rs`.
- `crates/daemon/src/git_write.rs` (**new**) — write helpers.
- `crates/daemon/src/server.rs` — handlers for `OpenDiffTab`, `StageFiles`, `UnstageFiles`, `Commit`, `DiscardChanges`, `StashPush`, `ListStashes`, `StashPop`, `StashApply`, `StashDrop`. Switch git-status responses from per-client (`out_tx`) to broadcast.
- `crates/daemon/src/tabs.rs` — `make_tab` accepts `Option<PaneContent>` instead of `Option<String>` session id; ditto split/extract/merge.
- `crates/daemon/src/state.rs` — backwards-compat deserialize of state.json's pane content.

### Frontend
- `apps/tauri-app/src/App.tsx` — activity-bar layout split; derive `focusedRepoId`; remove the in-pane git toggle prop plumbing.
- `apps/tauri-app/src/components/ActivityBar.tsx` (**new**)
- `apps/tauri-app/src/components/source-control/` (**new directory** — `SourceControlSidebar.tsx`, `RepoHeader.tsx`, `ChangesTree.tsx`, `StagedSection.tsx`, `StashesSection.tsx`, `GraphList.tsx`, `CommitInput.tsx`).
- `apps/tauri-app/src/components/DiffPane.tsx` (**new**) — Monaco diff wrapper.
- `apps/tauri-app/src/components/GridRenderer.tsx` — `pane.content.kind` branching: `"session"` → `SessionPane`, `"diff"` → `DiffPane`.
- `apps/tauri-app/src/components/SessionPane.tsx` — remove `View` type, git toggle, `GitPanel` import/render (lines 5-6, 20, 24, 125-152).
- `apps/tauri-app/src/components/GitPanel.tsx` — **delete** after Phase A.
- `apps/tauri-app/src/types.ts` — mirror every protocol change.
- `apps/tauri-app/src/api.ts` — typed wrappers for the new messages.
- `apps/tauri-app/src/main.tsx` and Vite config — Monaco bundler hookup, web worker entrypoints.

### Existing patterns to reuse
- Tree expand/collapse: `Sidebar.tsx:67-74` (`useState<Set<string>>` pattern) — reuse the idiom for `ChangesTree`.
- Resizable split: `ResizableSplit` (already used by `GitPanel`) — sidebar internals can use it for the Changes / Graph divider.
- Context menus: `ContainerContextMenu` in `Sidebar.tsx:382-433` — match its anchoring + backdrop pattern for file-row and stash-row context menus.
- Listener pattern: `DaemonClient` event dispatch via `window.addEventListener("rt:<msg>", ...)` (see `GitPanel.tsx:98-105`) — reuse it for the new write responses, with explicit cleanup on every request (no more leaks like `openInForge`).
- `run_git` helper at `git_inspect.rs:12-31` → factor to a shared module before adding writes.

## Verification

### Per phase
- **Phase A**: open sidebar, dirty the working tree, see the tree render. Switch focused panes between sessions in different repos and confirm the sidebar follows. Pin a repo from the dropdown; confirm it doesn't change when the focus does. Cold-start with no sessions open; sidebar still works with manually picked repo. The `Terminal | Git` toggle is gone from every `SessionPane`.
- **Phase B**: click a file → diff opens as a new tab. Drag the diff tab to a split next to a terminal — both render. Pop out the diff tab via tab context menu. Close the diff tab; underlying changes still visible in sidebar. Restart the daemon — state.json deserializes cleanly (old states had only sessions, new states have both kinds). `cargo test -p protocol` covers the migration shim.
- **Phase C**: stage one file → STAGED section updates; commit message → Commit → status refreshes and the new commit appears in GRAPH. Trigger a pre-commit hook failure → `Error` message surfaces in a toast (uses the existing dead `error` plumbing in `App.tsx:848-850` — fix it as part of this phase). Open a pop-out window with the sidebar visible, stage from the main window, confirm the pop-out updates (broadcast change).
- **Phase D**: stage a file, click discard → confirm modal lists exactly that file → discard → file restored. Stash with a message → STASHES section gets the new entry; pop → working tree changes return. Drop → entry disappears.
- **Phase E**: scroll to the bottom of GRAPH → "load more" → next batch appends. No hard cap.

### Cross-cutting
- `cargo clippy --all-targets --all-features -- -D warnings` — keep the workspace at zero warnings.
- `cargo test -p daemon -p protocol`.
- `pnpm typecheck` from `apps/tauri-app/`.
- `pnpm tauri dev` — end-to-end smoke: spawn a session, dirty the worktree, stage, commit, switch to GRAPH, click load more, open a diff in a split next to the terminal, pop it out.
- Smoke spec in `tools/e2e/tests/e2e/specs/` extended (or a new `source-control.spec.ts`) covering at minimum: open sidebar, click a file, assert diff tab appears, assert tree renders status indicators.
- Manual: kill the daemon mid-commit (best done while a long pre-commit hook runs) — confirm the UI does not hang and the staged state is recoverable on relaunch.
