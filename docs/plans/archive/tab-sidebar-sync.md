# Tab ↔ sidebar bidirectional sync

## Why

User feedback (hand-test, 2026-05-11): *"Tab organization should be reflected in the treeview sidebar on the left. So I should be able to also rearrange terminals between tabs by moving them around in the treeview."*

Today the sidebar groups sessions by their **infrastructure source** (workspace → repo → detached). Tabs group them by **current view layout**. These are orthogonal axes, and the sidebar reveals only the first one. The user wants the second axis surfaced *and* manipulable from the tree.

The user's literal wording leaves two valid reads — both must be answered before implementation:

1. *Tab membership should be visible in the existing tree* (annotation, not restructure).
2. *The tree should be organized by tabs* (restructure, replace or augment the container-based grouping).

This plan documents both possible directions, lists the work each entails, and gates implementation on the design pick. The protocol already has the moves we'd need (`MovePane`, `ExtractToNewTab`, `ReplacePaneSession`) — no wire changes.

## Existing surfaces (read these before designing)

- `apps/tauri-app/src/components/Sidebar.tsx` — tree builder is `buildContainers` at the bottom; container kinds are workspace / repo / detached. Sessions render via `SessionLeaf`.
- `apps/tauri-app/src/components/TabBar.tsx` — already implements pane-cross-tab drag via `text/x-rt-pane` data type. `onPaneDragEnter` activates the hovered tab so the user can drop onto a pane in it; `onDrop` on a pill sends `MovePane`.
- `apps/tauri-app/src/utils/grid.ts` — `collectPanes(node)` enumerates pane_id + session_id pairs; `tabHasBoundSessions(tab)` is already used by the close-confirm path.
- `crates/protocol/src/lib.rs:798` — `MovePane { src_tab_id, src_pane_id, dst_tab_id, dst_pane_id, edge }`.
- `crates/protocol/src/lib.rs:821` — `ExtractToNewTab { source_tab_id, pane_ids, name }`.

## Design options

### Option 1: Annotate-and-drag (minimal restructure)

Keep the current workspace/repo/detached tree. **Each `SessionLeaf` gains:**

- a small `[tab-name]` pill (or icon) showing which tab(s) the session is bound to (a session can be referenced by multiple panes across multiple tabs, though it's rare);
- `draggable={true}` with `text/x-rt-pane` payload carrying `{ tab_id, pane_id }` of the first/primary pane currently holding this session, so the existing TabBar drop targets accept it;
- context-menu entry "Move to tab → \<existing\> / + New tab" backed by `MovePane` / `ExtractToNewTab`.

**Pros:** Tiny diff. No regression. Drag-source compatibility means the existing tab-pill drop targets work without changes. Existing wdio specs unaffected.

**Cons:** Doesn't *literally* reorganize the tree by tabs. A user reading the feedback strictly might still feel underserved.

**Scope:** ~3 spec files (rename, drag-source, context-menu); 1–2 days.

### Option 2: Tab-first tree (full restructure)

Replace `buildContainers` with `buildTabContainers`: top-level nodes are **tabs**; each child is a session bound to a pane in that tab. A "Not in any tab" bucket catches sessions that are alive but unreferenced (orphans, paneless sessions).

**Pros:** Direct match to user phrasing. The tree *is* the tab organization.

**Cons:**

- Loses the workspace/repo grouping context (knowing which repo a session belongs to is useful when reading the tree). Mitigation: per-leaf subtitle with `<repo>:<branch>`.
- Sessions referenced by multiple panes appear multiple times under different tabs (already true for the grid, but new in the sidebar).
- Spawn-target context (workspace × → repo × → "+ spawn") doesn't fit; the "+ spawn" toolbar buttons might need a separate "Repos & workspaces" panel.

**Scope:** Larger refactor (~1 week). Requires rethinking the spawn-target flow.

### Option 3: Dual-tree (toggle)

Sidebar toolbar gains a `[Repos | Tabs]` segmented control. Each view is independently rendered. Persisted to `localStorage`.

**Pros:** No regression; tab-first available when wanted.

**Cons:** Two trees to maintain. Likely lower-value than committing to one model.

**Scope:** ~Option 1 + ~Option 2 wiring + a toggle. Heaviest.

## Decisions reached (2026-05-11)

User picked **Option 3 (dual-tree toggle)**, with inline tab pill placement, and `[unbound]` styled muted-italic / clickable for the no-binding case.

## Tasks shipped

- [x] `Sidebar.tsx` accepts `tabs: TabEntry[]` and `client: DaemonClient` through `ContainerNode` → `SessionLeaf`.
- [x] `sessionTabBindings(sessionId, tabs)` + `firstLeafPane(tab)` helpers in `utils/grid.ts`.
- [x] Inline `TabPill` component on every session leaf: `T:<tabName>` (one binding), `T:×N` accent-bordered (multiple, with title listing all), `[unbound]` dashed-muted button (zero).
- [x] Bound leaves are `draggable={true}` with `text/x-rt-pane` payload `${tabId}:${paneId}` (same format as `GridRenderer`'s `DRAG_MIME`) so existing TabBar + grid drop targets accept the drag.
- [x] Clicking the `[unbound]` button fires `create_tab { initial_session_id }` — fastest path to recover from a tab close that orphaned a live session.
- [x] CSS for `.tree-tab-pill` / `.tree-tab-pill.unbound` / `.tree-tab-pill.multi` / `.tree-leaf.is-draggable`.
- [x] `[Repos | Tabs]` segmented `sidebar-view-toggle` in the sidebar header. Persisted to `localStorage` under `rt.sidebar.view`. `aria-selected` reflects active.
- [x] `buildTabContainers(tabs, sessions)` builds tab-first containers (kind: `tab`), with a trailing `unbound` pseudo-container for sessions no tab references. CSS adds `TAB` / `UNB` kind tags.
- [x] Tab-view banner explains the unbound bucket: *"Click the [unbound] pill on a session to open it in a new tab."*
- [x] Container context menu suppressed for `tab` / `unbound` / `detached` kinds (read-only groupings).
- [x] New wdio spec `tools/e2e/tests/e2e/specs/sidebar-tab-view.spec.ts` (3 tests): toggle defaults to Repos, bound session shows `T:<name>` pill + flips to tab-view container, clicking `[unbound]` after a close rebinds via `create_tab`.

## Deferred to a follow-up

- Right-click context menu on session leaves (e.g. "Move to tab → ..." with a submenu) — drag-source covers the move-pane affordance already; the menu is polish for accessibility / discoverability.
- "Bind to existing tab" for `[unbound]` sessions (currently it always creates a new tab; binding to an existing tab via `SplitPane { new_session_id }` or `ReplacePaneSession` on an empty pane is doable but adds picker UI surface).
- Container-level drag in tab-view: dragging a tab container to reorder. The TabBar already supports this; adding it in the tree would be a second surface for the same `ReorderTabs` action.
- Reverse direction (drag from tab pill to tree). Pill drags already mean tab-reorder; conflating with tree-drop would be confusing.

## Verification — completed

- `pnpm typecheck` (apps/tauri-app + tools/e2e): clean.
- Full wdio suite (`pnpm test` from `tools/e2e`): 6 specs / 16 tests passing.
- `cargo clippy --all-targets --all-features -- -D warnings`: clean (no daemon changes this iter).

## Out of scope (for this plan)

- Drag-from-tabbar-to-tree (the reverse direction). Tab pills already drag for tab-reorder; adding tree as a drop target conflates the two.
- Splitting/joining panes from the tree. The tree is for membership; topology stays in the grid.
- Saved tab layouts / templates. Separate concern.

## Verification

After implementation:

```powershell
cargo clippy --all-targets --all-features -- -D warnings
cd apps/tauri-app; pnpm typecheck
cd ..\..\tools\e2e; pnpm test
```

Hand-check: spawn 3 sessions across 2 repos, drag a session leaf in the tree onto a different tab pill, confirm:

- The session migrates (the source tab loses it, the destination tab gains it).
- The pill on the leaf updates.
- The grid reflects the move on the next tick.
