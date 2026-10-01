# Native client: open items' rulings

The rulings the native client's open items need before they are briefed. Each item is one line in [MAIN.md](./MAIN.md), which orders them; the client's settled design is in [docs/native-client.md](../native-client.md), and what each feature must do is in [native-client-parity.md](./native-client-parity.md), whose lines are ticked as items land. An item's section here is deleted in the commit that lands it; the file goes when the last one does.

## Working rules

- **Landing:** each item is committed to `main` as soon as it is green, and pushed at checkpoints (a finished wave), not after every item.
- **Autonomy:** drive the native client to done without stopping between items or waves. Technical decisions are settled in the session and recorded here as rulings; only questions about behaviour the user will see go to the user. Phases 5 and 6 are split into brief-sized items from their parity sections when their wave comes up, and the split goes to the user before anything is dispatched.
- **Phase 4 scope:** its named features plus every parity line not claimed by Phase 5 (drag-and-drop, pop-outs) or Phase 6 (remote). Every item mounts into `RootView` (`lib.rs`), so concurrent items rebase onto `main` before landing. Concurrency ceiling 3. Dependencies: P4.11 → P4.15b (the launch path). P4.12c builds on the spawn form's target lock (`spawn_form.rs` `Lock`: a locked target plus an optional pinned worktree and `share_confirmed`), which P4.4b added for "Launch session here". P4.16 changes the counts P4.2, P4.3 and P4.13a built, all landed.

## P4.10 Repos and workspaces

+ Repo with a folder picker that remembers its last folder, + Workspace, remove repo / workspace (inline two-click; with live sessions a dialog), the workspace creator (from repos or a `.code-workspace` file), the "VS Code workspace detected" prompt, "Add repo" / "Add workspace" on DIR and SH containers, and the no-repos states in the sidebar, the main area and the spawn dialog. Parity: "Toolbar", "Remove repo/workspace", "DIR/SH containers", "Workspace creator", "VS Code workspace detected", "Empty states", "Target picker", "File pickers remember…".

## P4.11 Containers

Launch last (▶, double-click, "Launch last again ▸" current / new / named tab / edit first; trusted configs open the dialog), the full container menu (spawn here / in tab, presets entry, Explorer, VS Code, copy path, Remove), the container row's keyboard fold and last-launch summary, the Detached banner and its stop all, "Resume all (N)", and the spawn dialog's container and tab-container entry points. Parity: "Container row", "Container context menu", "Launch last again", "Repos view" banner, "Detached container stop all", "Entry points".

## P4.12 Sessions

The session menu's Duplicate ▸ (Shift opens the prefilled dialog), Move to ▸, Add to current / new tab and Reveal worktree; the pane header's status dot, runtime, trusted and headless chips and repo:branch chips; the abandoned overlay (last prompt, Resume / Dismiss) and orphan banner; the leaf's tooltip, trusted marker and orphan / abandoned / inactive tags with inline Resume / Dismiss; the display-label order; auto-discard of worktree-less sessions that exit on their own. Parity: "Session context menu", "Pane header", "Abandoned overlay", "Session leaf", "Display label order", "Sessions without a worktree…".

- Split (orchestrator): **P4.12a** labels everywhere through `display_label`, the label tooltip, leaf tags and inline buttons, pane header chips, abandoned overlay, orphan banner, auto-discard; **P4.12c** Shift-duplicate prefill, on the target lock P4.4b builds.
- Rulings (user): Reveal opens the worktree folder itself (as Tauri does), shown only for sessions with their own worktree; auto-discard skips headless sessions, which stay so their stats can be read; with a diff tab active, "Add to current tab" becomes "Add to new tab" and opens one, as a leaf click does.
- Settled (orchestrator): Duplicate into an existing tab places by `pane_target_for_session`; Duplicate and Move to list grid tabs only; submenus swap rows with ‹ Back; member chips get a second header row only for sessions with members; the orphan banner names the session's runtime instead of Tauri's fixed "claude"; auto-discard also skips abandoned sessions; the rename field keeps native's seed; a Shift-duplicate with no stored config opens the dialog with defaults on the source's repo.
- P4.12c: `SpawnForm::open` seeds the Spawn defaults (trusted, approval mode, Codex sandbox) and has no prefill of those three fields yet; the duplicate prefill must beat the defaults.


## P4.15 Preset wizard

Split in two. **P4.15a** sources (file, folder, inline, GitHub issue ranges) and variables (toggle, file, folder, text, required). **P4.15b** preview (grouped by tab, max panes per tab, script commands) and launching (progress, counts, Cancel, Select launched, Stop all), sticky progress and failure toasts one per job, the first tab created made active, preset-launched sessions highlighted; needs P4.11. Parity: "Preset wizard", "Sticky preset progress…", "Preset-launched sessions highlighted".

## P4.16 Exclude a session from busy tracking

New, not a parity line. A per-session setting so a session that is always busy (a dev server, a watcher) stops counting as busy. Rulings (user): an excluded session leaves the window title's M, the tab's busy badge and the leaf's attention highlight; it still sends OS notifications. The flag lives on the daemon's session, like the appearance overrides, as an additive `#[serde(default)]` protocol field, so every client sees it. Resume and session recovery keep it; Duplicate does not. It is toggled by a checkable "Don't count as busy" item in the session menu, and an excluded leaf carries a small muted marker.

## P4.17 Keep an untouched layout when panes come and go

New, not a parity line. When a tab's layout was last set by picking a layout type (side by side, grid, …) and the user has not resized its panes since, adding or removing a session rebuilds that same layout type over the new pane count, so side by side stays evenly balanced. Once the user drags a divider, the tab keeps its sizes and adding or removing a pane falls back to today's behaviour. Rulings (user): the "untouched since picked" flag is per client, since two clients can show the same sessions in different tabs and layouts (a laptop attached remotely beside the desktop's big screen); it is saved with that tab's layout, which the daemon already keys by client id. Both pickers set it: Rearrange ▸ and the first-connect chooser.

## Phase 5 — windows and drag-and-drop

Pane / tab / session pop-outs as windows of one process, pane drag-and-drop with edge overlays, sidebar and tab drag-to-reorder. The split below goes to the user with its open questions before anything is dispatched. Items run in build order: drag-and-drop first (P5.1–P5.5), then pop-outs (P5.6–P5.11), then docs (P5.12). Each is a plain-Rust model with unit tests under a thin GPUI view, and ticks the parity lines it delivers; parity lines are cited by their opening words and section in [native-client-parity.md](./native-client-parity.md).

### What already exists

- **Protocol**: drag-and-drop needs no change. `MovePane` with `PaneDropEdge` (`Left`, `Right`, `Top`, `Bottom`, `Replace`, `OuterLeft`/`OuterRight`/`OuterTop`/`OuterBottom`), `ReorderTabs`, `ReorderContainers`, `ReorderSessions`, `SplitPane { new_session_id }` and `ReplacePaneSession` are all in `crates/protocol/src/lib.rs`. The daemon's move (`crates/daemon/src/server.rs` `move_pane`, `crates/daemon/src/tabs.rs` `insert_adjacent` / `insert_in`) keeps the moved pane's id, swaps sessions for a same-tab `Replace`, gives the destination pane the source's session for a cross-tab `Replace` (its old session goes unbound), wraps the whole root for an outer edge, refuses moving a tab's only pane within that tab, and removes a source tab left empty.
- **Client**: `TabsModel` folds `TabsReordered` (`tabs.rs` `reorder`) and `SidebarModel` folds `ContainersReordered` / `SessionsReordered` and orders by them (`sidebar.rs` `apply_container_order`, `apply_session_order`). `tabs::pane_rects`, `tabs::pick_balanced_drop_target` (the pane menu's Move to ▸, `pane_menu.rs`), `tabs::size_drivers`, `undo::MOVED_PANE`, `RootView::record_undo` and `TabsModel::mark_closing_if_last_pane` exist. No GPUI drag-and-drop API is used yet: divider and sidebar-width drags are raw mouse move / up on the root (`lib.rs` `on_drag_move`, `on_drag_end`). A pane's terminal is a `TerminalPane` entity in a `PaneSlot` keyed by pane id (`grid_view.rs`); app shortcuts are the root's `capture_key_down` (`lib.rs`).
- **Tauri reference** (tag `tauri-last`, `apps/tauri-app/src/`): `components/GridRenderer.tsx` `computeEdge` (outer band 0.12 of the pane on edges touching the grid boundary, the nearer at a corner; centre box 0.35–0.65 swaps; else the nearer axis's half) and `PoppedOutPaneCard`; `components/TabBar.tsx` (before / after by the pill's half, a pane entering a pill activates it, a pane dropped on a pill placed by `pickBalancedDropTarget`); `components/Sidebar.tsx` (container and leaf reorder by the row's half, leaves reorder only inside workspace, repo and tab containers, a bound leaf also carries its first pane); `PaneWindow.tsx`, `TabWindow.tsx`, `SessionWindow.tsx`; `src-tauri/src/lib.rs` `open_pane_window` / `open_tab_window` / `open_session_window` (an open window is focused, 1100×720, min 700×400).

### GPUI 0.2.2

Read from `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/gpui-0.2.2/src/`.

- `on_drag(value, constructor)` (`elements/div.rs` 499) starts a typed drag once the pointer moves more than 2 px from a press (`DRAG_THRESHOLD`, `div.rs` 47); the constructor returns the ghost view, drawn only inside the window the drag started in (`window.rs` 2048–2078).
- `on_drag_move::<T>` (`div.rs` 282) fires on every move of a `T` drag, inside or outside the element, with the element's `bounds`, so each target tests the pointer against its bounds itself. `drag_over::<T>` (`div.rs` 938) styles an element while a `T` hovers it. `on_drop::<T>` (`div.rs` 462, 976) fires on mouse up over the hovered hitbox, gated by `can_drop` (`div.rs` 473). Any mouse up ends the drag (`window.rs` 3716–3725); `cx.stop_active_drag(window)` (`app.rs` 1949) cancels one, for Esc.
- The drag is app-wide (`App.active_drag`, `app.rs` 545), but on Windows a press captures the mouse to its window (`platform/windows/events.rs` 451 `SetCapture`, 482 `ReleaseCapture`), so every move and the release reach the source window only: a drop on another window never fires.
- `cx.open_window(WindowOptions, build)` (`app.rs` 943) opens further windows of the one app. `WindowOptions` carries `window_bounds`, `display_id`, `window_min_size`, `focus` and `show` (`platform.rs` 1093–1134). `window.activate_window()` (`window.rs` 4112), `window.set_window_title` (`window.rs` 1779), `window.on_window_should_close` (`window.rs` 4329), `cx.on_window_closed` (`app.rs` 1806) and `cx.windows()` (`app.rs` 919) cover focusing, titling and closing.
- A window redraws when any entity its last draw read notifies (`app.rs` 794–824 `detect_accessed_entities`, 2034 `notify`), so a pop-out whose root reads `RootView` repaints with it.
- Specs: `VisualTestContext` `simulate_mouse_down` / `simulate_mouse_move` / `simulate_mouse_up` (`app/test_context.rs` 726–762) drive a drag; `TestAppContext::windows()` (`app/test_context.rs` 341) lists the open windows.

### Rulings

- One process, shared state: "In GPUI this becomes one process with shared state and several windows." (`docs/plans/native-client-parity.md`, Riskiest for the port, 5)
- Testable core: "State a feature adds … lives in plain-Rust modules with unit tests; the GPUI `*_view.rs` renders it and forwards events. Every feature mounts into `RootView` (`lib.rs`)" (`docs/native-client.md`, Stack).
- Undo: "swap has no native gesture until Phase 5's pane drag-and-drop, which records its own undo" (parity, "Undo shelf for closed tab…"). A drop records "Moved pane", or "Swapped panes" for a same-tab centre drop, as the pane menu's Move to ▸ does.
- Every pane has a header to drag: "empty panes keep the header's buttons and add the empty-pane menu instead of floating buttons" (parity, "Split right/down…").
- Leaf click: "a single click places an unbound leaf in the active tab, by the user's ruling, so there is no double-click action" (parity, "Click a leaf…").
- Pop-out windows: "Pop-outs have no app shortcuts and no Pop out button of their own"; "Opening an existing pop-out focuses it; title is the session or tab name; 1100×720 (min 700×400); "not found" state"; a session pop-out "stays open after the session stops"; a tab pop-out has "spawning disabled with a hint; closes when the tab is removed" (parity, Pop-out windows).
- Quit: "Closing the window runs the quit flow" (`docs/native-client.md`, Window). Only the main window's close does; a pop-out's close docks it back (Tauri `PaneWindow.tsx` `onCloseRequested`).
- Size driver: "only one pane per session (its size driver) answers terminal queries" (`docs/native-client.md`, Terminal). A pop-out's terminal takes part in `tabs::size_drivers`.
- Unseen turns clear "when … a focused pane shows it", and "The unseen set is per window and not saved" (`docs/native-client.md`, Status glyphs). A session focused in a pop-out counts as shown.
- Protocol: changes stay additive and keep protocol 22 decodable (CLAUDE.md, Architecture invariants).
- Verification: "UI hand-test (multi-window and drag can't be fully specced)" (MAIN.md, Wave 9); "a visible result no test can prove lands on green gates and is listed for a hand-test" (MAIN.md, Gates).

### P5.1 Drag model: payloads, drop edges, reorder

- **Does**: the plain-Rust half of every drag. `DragPayload` (`Pane { tab_id, pane_id }`, `Tab(tab_id)`, `Container(ContainerRef)`, `Leaf { session_id, container_id }`); `drop_edge(x, y, outer) -> PaneDropEdge` over the pointer's fraction of the pane (geometry per Q7); `outer_edges(grid, bounds)` over `tabs::pane_rects`, the sides of each pane that touch the grid's boundary; `pane_drop(payload, dst_tab, dst_pane, edge) -> Option<ClientMessage>` with the undo label (a self-drop, a drop on a diff tab and a same-tab move of a tab's only pane give none); `reorder(ids, dragged, target, side) -> Vec<String>`, the side taken from the pointer's half of the row or pill. No parity line alone; P5.2–P5.5 build on it.
- **Files**: new `apps/native/src/drag.rs`; `tabs.rs` (`outer_edges` beside `pane_rects`); `lib.rs` (`mod drag`).
- **Core / view**: all core, no view.
- **Proof**: `cargo test -p rustling-tulip-native --lib drag::` and `tabs::tests::outer_edges`, red first: `centre_box_is_replace`, `nearer_axis_picks_the_half`, `outer_band_only_where_the_pane_touches_the_grid_edge`, `grid_corner_takes_the_nearer_outer_edge`, `self_drop_sends_nothing`, `only_pane_cannot_move_within_its_tab`, `swap_is_labelled_swapped_panes`, `reorder_before_and_after`, `reorder_onto_itself_is_unchanged`, `outer_edges_of_a_two_by_two_grid`.
- **Needs**: Q7.

### P5.2 Pane drag-and-drop in the shown tab

- **Does**: drag a pane by its header (Q8) with a ghost of its label; while a pane drag is over a pane, that pane draws the edge overlay (the half, the centre box or the outer strip, tinted with the accent); the release sends P5.1's `MovePane`, records its undo entry and marks a one-pane source tab closing; Esc cancels. Parity: "**(hard)** Pane drag and drop from the header or ⠿ handle: edge overlay for splits, centre swap, outer band splits at the top level, across tabs" (Tabs & panes; "across tabs" completes in P5.3), and the swap gesture "Undo shelf…" waits for.
- **Files**: `grid_view.rs` (header `on_drag`, pane `on_drag_move` / `on_drop`, overlay); `lib.rs` (`RootView.pane_drop: Option<(String, PaneDropEdge)>`, cleared on drop, Esc and mouse up); `undo.rs` (`SWAPPED_PANES`); `pane_menu.rs` (the Move to ▸ send and its undo become one helper both paths call); new `tests/ui_pane_drag.rs`; `tests/support/mod.rs` (a `drag(from, to)` helper over the simulated mouse).
- **Core / view**: the edge, the message and the undo label come from `drag.rs`; the view hit-tests the pointer against each pane's bounds and draws the overlay.
- **Proof**: `cargo test -p rustling-tulip-native --test ui_pane_drag`, red first: `drop_on_right_half_moves_the_pane_right`, `drop_on_centre_swaps_and_offers_undo`, `drop_in_the_outer_band_splits_at_the_top_level`, `drop_on_itself_sends_nothing`, `overlay_follows_the_pointer_and_clears_on_release`, `esc_cancels_the_drag`; `ui_panes` and `ui_tabs` stay green.
- **Needs**: P5.1; Wave 2's stale-closing-mark item (drags make a rolled-back `MovePane` common); Petal's PT.7 (landed; the header it drags); Q8, Q12, Q14.

### P5.3 Tab strip drag

- **Does**: drag a pill to reorder, with an insertion mark before or after the hovered pill by its half, applied locally at once and sent as `ReorderTabs` (the daemon's `TabsReordered` reconciles); a pane drag over a pill activates that tab (Q10), so the pane can drop on a pane there; a pane dropped on a pill goes where `pick_balanced_drop_target` says, with "Moved pane" undo; a diff tab's pill takes no pane. Parity: "Drag to reorder; a pane dragged over a pill activates that tab; dropping on a pill places the pane automatically" (Tabs & panes), and "across tabs" of the pane drag line.
- **Files**: `tab_bar.rs`; `tabs.rs` (`TabsModel::reorder` callable for the local apply); new `tests/ui_tab_drag.rs`.
- **Core / view**: `drag::reorder`, `TabsModel::reorder`, `pick_balanced_drop_target`; the view draws the mark and routes the drops.
- **Proof**: `cargo test -p rustling-tulip-native --test ui_tab_drag`, red first: `pill_dropped_after_another_reorders_the_tabs`, `reorder_shows_before_the_daemon_echo`, `pane_over_a_pill_activates_that_tab`, `pane_dropped_on_a_pill_moves_it_balanced_with_undo`, `diff_pill_refuses_a_pane`.
- **Needs**: P5.2; Wave 2 (its `tab_bar.rs` and `tabs.rs` items land first); Q10.

### P5.4 Sidebar drag-to-reorder

- **Does**: in the Repos view, drag workspace and repo containers (sent as `ReorderContainers` with every workspace and repo in the new order; SH, DIR and Detached neither drag nor take drops); in the Tabs view, drag tab containers, which reorder the tabs with the strip's own `Tab` payload, so a pill and a tab container drop on each other; drag a leaf within its workspace, repo or tab container (`ReorderSessions`; leaves in cwd containers, and a leaf dropped in another container, do not reorder). An insertion line before or after by the row's half; the order applies locally until the daemon's broadcast. Parity: "Drag-reorder containers, tab containers (shared with the TabBar) and leaves, saved on the daemon" (Sidebar).
- **Files**: `sidebar.rs` (local apply of container and session order); `sidebar_view.rs`; `tab_bar.rs` (the pill accepts a tab-container drop); new `tests/ui_sidebar_drag.rs`.
- **Core / view**: `drag::reorder` and `SidebarModel`'s local order, unit-tested in `sidebar.rs`; the view draws the line.
- **Proof**: `cargo test -p rustling-tulip-native --lib sidebar::tests::local_order` and `--test ui_sidebar_drag`, red first: `container_dropped_before_another_sends_the_full_order`, `shell_and_dir_containers_do_not_drag`, `tab_container_reorders_the_strip`, `leaf_reorders_within_its_repo`, `leaf_dropped_in_another_container_does_nothing`, `local_order_holds_until_the_echo`.
- **Needs**: P5.1; P5.3 (the shared tab payload); Petal's PT.6a / PT.6b (landed) and Wave 3's P4.10 / P4.11 (they reshape the rows it drags).

### P5.5 Drag a leaf onto a pane or a tab pill

- **Does**: a leaf dragged over the grid draws the same overlay and drops per Q9: a bound leaf moves its pane (the one in the active tab, else its first binding); an unbound leaf is placed with `SplitPane { new_session_id }` or `ReplacePaneSession`; onto a pill, it goes where `tabs::pane_target_for_session` says in that tab. Parity: "Drag a leaf onto a pane or a tab pill" (Sidebar).
- **Files**: `drag.rs` (`leaf_drop` mapping); `sidebar_view.rs` (the leaf's payload); `grid_view.rs` and `tab_bar.rs` (accept `Leaf`); `tests/ui_sidebar_drag.rs`.
- **Core / view**: `leaf_drop` is plain Rust with its tests; the views accept the payload.
- **Proof**: `drag::tests` red first: `bound_leaf_moves_its_active_tab_pane`, `unbound_leaf_on_a_half_splits_with_the_session`, `unbound_leaf_on_the_centre_replaces`; `--test ui_sidebar_drag`: `leaf_dropped_on_a_pane_edge_places_it`, `leaf_dropped_on_a_pill_places_it_in_that_tab`.
- **Needs**: P5.2, P5.3, P5.4; Q9.

### P5.6 Pop-out model and a second window

- **Does**: `popouts.rs`: the pop-outs (`Pane { tab_id, pane_id }`, `Tab(tab_id)`, `Session(session_id)`), keyed so opening one twice focuses the first; `prune(tabs, sessions)`, the pop-outs to close (a pane pop-out when its pane or session goes, a tab pop-out when its tab goes; a session pop-out stays after its session stops and shows "not found" once the daemon removes it); titles (the session's display label, the tab name); the window options (1100×720, min 700×400, opened cloaked through `offscreen::show_cloaked` under `RUSTLING_TULIP_OFFSCREEN_WINDOW`). `popout_view.rs`: `PopoutView`, the root of each pop-out window, holding a `WeakEntity<RootView>` and drawing from it (Q1), with no app-shortcut `capture_key_down`. `RootView::open_popout` / `dock_back` move a pane's `TerminalPane` entity into the pop-out and back, and a pop-out's `on_window_should_close` docks it back. No menu entry yet; specs call `open_popout`. This proves the risky part first: a `TerminalPane` keeps keys, IME and focus when drawn in a second window. Parity: groundwork for Pop-out windows.
- **Files**: new `popouts.rs`; new `popout_view.rs`; `lib.rs`; `grid_view.rs` (draw a pane slot outside the grid); new `tests/ui_popout.rs`; `tests/support/mod.rs` (reach the second window).
- **Core / view**: `popouts.rs` holds the rules and titles; the view draws and forwards.
- **Proof**: `cargo test -p rustling-tulip-native --lib popouts::` red first: `opening_twice_focuses_the_first`, `pane_popout_closes_when_its_pane_goes`, `pane_popout_closes_when_its_session_goes`, `session_popout_outlives_a_stop`, `tab_popout_closes_with_its_tab`; `--test ui_popout`: `popped_pane_takes_keys_in_its_window`, `closing_the_popout_docks_the_pane_back`, `app_shortcuts_do_nothing_in_a_popout`. The OS tier stays green.
- **Needs**: Q1. It shares no model with P5.1–P5.5 but follows them, to avoid rebasing `grid_view.rs` and `lib.rs`.

### P5.7 Pane pop-out

- **Does**: "Pop out" in the pane header and in the session menu (a session with a pane pops the pane in the active tab, else its first; with none, P5.8's session pop-out); the main grid keeps the pane's slot as a card with the session label, Focus window and Dock back (Q3); the pop-out's toolbar: status glyph, label, the tab it is docked in, member chips, runtime and trusted chips, two-step Stop, Dock back; the terminal takes focus when the window opens. A popped pane that is moved in the main window keeps its pop-out, since the daemon keeps the pane id. Parity: "**(hard)** Pane pop-out…" and "Pop-outs have no app shortcuts…" (Pop-out windows); Pop out in "Pane header…" and "Session context menu…" (Sessions); "Focus goes to the terminal after spawn and when a pop-out opens" (Terminal).
- **Files**: `popout_view.rs`; `grid_view.rs` (the card, the header button); `session_menu.rs` (the row); `popouts.rs` (`pane_to_pop(session, tabs, active)`); `lib.rs`; `tests/ui_popout.rs`.
- **Core / view**: `pane_to_pop` and the toolbar's parts (reusing `grid_view::header_parts`) are plain Rust; the view draws.
- **Proof**: `popouts::tests::pane_to_pop_prefers_the_active_tab` red first; `--test ui_popout`: `header_pop_out_opens_a_window_and_leaves_a_card`, `dock_back_from_the_card_closes_the_window`, `focus_window_raises_the_open_popout`, `session_menu_pops_the_active_tab_pane`, `popout_stop_asks_twice`, `popout_closes_when_its_session_is_removed`, `focused_popout_clears_the_unseen_turn`.
- **Needs**: P5.6; the session menu rows it sits beside (landed with P4.12b); Q3, Q6, Q13.

### P5.8 Session pop-out

- **Does**: a session with no pane opens in its own window: a terminal attached for it alone (scrollback first, as a pane attaches; counted by `tabs::size_drivers`, so only one view answers queries), Stop and Close window; it stays open after the session stops and shows "not found" once the session is gone; a leaf click on its session acts per Q13. Parity: "Session pop-out (session with no pane)…" and the "not found" state of "**(hard)** Opening an existing pop-out focuses it…" (Pop-out windows).
- **Files**: `popout_view.rs`; `grid_view.rs` (a terminal slot outside the grid, keyed `popout:<session id>`); `tabs.rs` (`size_drivers` counts pop-out terminals); `lib.rs`; `tests/ui_popout.rs`.
- **Core / view**: `size_drivers` and `popouts.rs`'s rules, unit-tested; the view hosts the terminal.
- **Proof**: `tabs::tests::size_drivers_count_a_session_popout` red first; `--test ui_popout`: `session_popout_loads_scrollback_and_takes_keys`, `session_popout_stays_after_stop`, `session_popout_shows_not_found_when_removed`, `leaf_click_on_a_popped_session` (Q13).
- **Needs**: P5.6, P5.7 (the menu row); Q13.

### P5.9 Grids in any window

- **Does**: the split tree, divider drags, pane drops and the pane-level menus and dialogs work in whichever window draws the tab: `grid_bounds` and the `Drag` state keyed by window, and the pane-close dialog, empty-pane menu, session menu and toasts drawn where Q6 says. The main window looks and behaves as before. This is what "Tab pop-out: full grid (split, drag, close)" needs.
- **Files**: `lib.rs`; `grid_view.rs`; `pane_menu.rs`; `pane_close_view.rs`; `popout_view.rs`; `tests/ui_popout.rs`.
- **Core / view**: a window-keyed layout and overlay-owner map in plain Rust, unit-tested; the views look up their window's entry.
- **Proof**: red first: `divider_drag_in_a_second_window_sets_its_ratio`, `pane_close_dialog_opens_in_the_window_that_asked` (Q6); `ui_panes`, `ui_pane_drag` and `ui_tabs` stay green.
- **Needs**: P5.6; Q6.

### P5.10 Tab pop-out

- **Does**: "Pop out" in the tab menu opens the tab in its own window: the full grid (split, drag within it, close) or a diff tab (its `DiffSlot` moves over), Close window, spawn buttons disabled with a hint that spawning happens in the main window, closed when the tab is removed; the main strip shows the tab per Q2. Parity: "Tab pop-out…" (Pop-out windows) and Pop out in "Tab menu…" (Tabs & panes).
- **Files**: `tab_menu.rs`; `tab_bar.rs`; `popout_view.rs`; `diff_tab_view.rs`; `lib.rs`; `tests/ui_popout.rs`.
- **Core / view**: the menu line and the strip's marker come from `tab_menu.rs` / `tabs.rs` models; the view draws.
- **Proof**: red first: `tab_popout_shows_its_grid_and_splits`, `diff_tab_pops_out`, `spawn_is_disabled_in_a_tab_popout`, `removing_the_tab_closes_its_popout`, `main_strip_shows_a_popped_tab` (Q2), `tab_menu_offers_pop_out`.
- **Needs**: P5.9, P5.3; Q2, Q11.

### P5.11 Pop-outs across restarts

- **Does** (as Q4 (a) and Q5 (a) recommend): the open pop-outs saved in native-ui.json (kind, id, rect and monitor through `window_state.rs`), saved on move and resize with the layout's 500 ms debounce, reopened once the tabs and sessions arrive, and dropped when their pane, tab or session is gone. With Q4 (b) this item shrinks to docking everything back on quit, folded into P5.6.
- **Files**: `sidebar.rs` (`UiState.popouts`); `window_state.rs`; `popouts.rs`; `lib.rs`; `tests/ui_popout.rs`.
- **Core / view**: `popouts::restore_plan(saved, tabs, sessions)` and `window_state::restore_options`, unit-tested.
- **Proof**: red first: `popouts::tests::restore_skips_a_gone_pane`, `popouts::tests::restore_waits_for_tabs_and_sessions`; `--test ui_popout`: `popouts_reopen_after_the_layout_arrives`, `a_popout_on_a_missing_monitor_opens_on_the_primary`.
- **Needs**: P5.7, P5.8, P5.10; Q4, Q5.

### P5.12 Phase 5 docs

- **Does** (orchestrator): `docs/native-client.md` gains a "Windows and drag-and-drop" section from these rulings and the answers below; CLAUDE.md's native-ui.json line gains the pop-out list; the README glossary gains pop-out, edge overlay and outer band; the parity lines are ticked; this section is deleted. The hand-test list: drags with a real mouse (threshold, overlays, Esc), pop-outs on a second monitor, a restart with pop-outs open.
- **Needs**: P5.1–P5.11.

### Open questions

Answered (user): Q1–Q14 all (a): one shared connection and `RootView`; a tab pop-out moves out and leaves its pill with a card; a popped pane keeps its slot as a card; pop-outs reopen where they were after a restart, kept in native-ui.json with no protocol change; a pop-out's overlays open in that window; Tauri's drop fractions; the whole pane header starts a drag; a leaf drop moves a bound pane or places an unbound session; a pill activates as a drag enters; no cross-window drag; edge and centre drops clear the untouched flag, pill drops and removals rebuild; a leaf click raises the pop-out showing its session; a cross-tab centre drop keeps the daemon's unbind and names the displaced session in the undo entry.

1. **Do pop-out windows share the main window's daemon connection?** (a) **Recommended**: one connection and one `RootView`; each pop-out's root holds a weak handle to it and draws its tab, pane or session from it, and GPUI repaints every window whose draw read `RootView` when it changes. Worst case: a busy `RootView` repaints every open pop-out. (b) Each pop-out has its own connection and state, as Tauri's did. Worst case: each window reloads scrollback, and two views of one session fight over its size. (c) One connection, with each pop-out keeping its own model fed from a copy of the network events. Worst case: two copies of the state drift apart.
2. **What does a tab pop-out leave in the main window?** (a) **Recommended**: the tab moves out: its pill stays in the strip with a pop-out mark, and showing it gives a card with Focus window / Dock back, as a popped pane does. Worst case: the same tab can't be watched in both windows. (b) A mirror, as Tauri did ("The tab and its sessions stay in the main window"): the grid shows in both windows. Worst case: every terminal in the tab is parsed and drawn twice, and each pane id needs two terminals. (c) The tab moves out and its pill is hidden until docked. Worst case: the user loses track of it, and Ctrl+1–9 renumber.
3. **Does a popped-out pane keep its slot in the main grid?** (a) **Recommended**: yes, as a card (label, Focus window, Dock back), as Tauri did. Worst case: a large card holds space the other panes could use. (b) The main grid lays out without it until it docks back. Worst case: the drawn tree no longer matches the daemon's, so divider paths and drops need a second mapping.
4. **What does a pop-out come back as after a restart?** (a) **Recommended**: it reopens at its saved place and monitor once the tabs and sessions arrive, and is dropped if its pane, tab or session is gone. Worst case: a window reopens for a pane the user had forgotten. (b) Everything docks back on quit and nothing is saved. Worst case: long-lived pop-outs are popped out again after every restart. (c) As Tauri: the flag is kept and the card shown, but the window opens only on Focus window. Worst case: stale cards after a restart.
5. **Where is pop-out state kept, and does it touch the protocol?** (a) **Recommended**: native-ui.json, beside the main window's rect; no protocol change, since the daemon's layout still holds the pane in its tab. Worst case: another client on another machine does not see this machine's pop-outs, which is intended. (b) An additive field on the daemon's per-client tab layout. Worst case: a protocol change and its v22 check for a per-machine concern. (c) Memory only. Worst case: as Q4 (b).
6. **Where do the menus, dialogs and toasts a pop-out opens appear?** (a) **Recommended**: in that pop-out window; each overlay records the window that opened it. Worst case: more plumbing in P5.9, and every overlay needs a window owner. (b) In the main window, which is raised. Worst case: the user's eyes jump between windows, possibly across monitors. (c) Pop-outs open none: closing a pane there only closes the pane. Worst case: discarding a session or deleting its worktree needs the main window.
7. **Pane drop geometry.** (a) **Recommended**: Tauri's fractions: an outer band of 12% of the pane on each side that touches the grid's edge (the nearer wins at a corner), a centre box from 35% to 65% on both axes that swaps, else the half of the nearer axis; the overlay tints that half, the centre box or a full-length strip along the grid's edge. Worst case: on a small pane the outer band is a few pixels wide. (b) Fixed sizes: a 24 px outer band and a centre box of a third. Worst case: on a large pane the centre box is huge and easy to hit by accident. (c) No outer band in the panes; four drop strips along the grid's edges appear during a drag. Worst case: extra bars flash in on every drag.
8. **What starts a pane drag?** (a) **Recommended**: the whole pane header except its buttons and chips. Worst case: a header click that wobbles more than GPUI's 2 px threshold starts a drag instead of focusing the pane. (b) Only a ⠿ grip at the header's left end. Worst case: a small target to hit. (c) Both.
9. **What does dragging a leaf onto a pane or a pill do?** (a) **Recommended**: a bound leaf moves its pane (active tab first), like a pane drag; an unbound leaf is placed: on a half, `SplitPane` with the session; on the centre, `ReplacePaneSession` (the replaced session goes unbound, not stopped); on an outer band, as that pane's half; on a pill, where a normal spawn would go in that tab. Worst case: a centre drop unbinds a session the user wanted on screen (undo brings it back). (b) As Tauri: only a bound leaf drops on panes and pills; an unbound leaf only reorders. Worst case: dragging an unbound leaf onto a pane does nothing. (c) A leaf drop always adds the session as another view and leaves any existing pane. Worst case: the same session in two panes, with sizing fights.
10. **When does a pill activate under a pane drag?** (a) **Recommended**: as soon as the drag enters it, as Tauri did. Worst case: sweeping across the strip flicks through the tabs. (b) After the drag rests on it for 500 ms. Worst case: it feels sluggish.
11. **Can a drag cross windows?** GPUI on Windows sends every move and the release to the window the drag started in, so a drop on another window never fires. (a) **Recommended**: no cross-window drag; panes move between windows with Dock back and Move to ▸, and each window's drags stay inside it. Worst case: dragging a pane from a pop-out to the main window does nothing. (b) Custom routing: a release outside the source window is hit-tested against the other windows' screen bounds and dropped there without a hover overlay. Worst case: DPI and monitor maths put drops in the wrong place, blind. (c) Tear-off only: releasing a pane or pill outside every window pops it out there. Worst case: a sloppy drop opens a window by accident.
12. **How does a drag treat a tab whose layout is untouched since picked (P4.17)?** (a) **Recommended**: a drop on a pane's edge or centre is a hand arrangement and clears the flag in the destination tab; a drop on a pill and a source tab losing a pane count as adding and removing, so an untouched tab rebuilds its picked layout type. Worst case: the rebuild moves panes the user did not touch. (b) Any drag clears the flag in both tabs. Worst case: dragging one pane out leaves a balanced tab unbalanced. (c) Drags ignore the flag and both tabs rebuild. Worst case: the spot the user dropped on is undone at once.
13. **What does a leaf click do when its session shows only in a pop-out?** (a) **Recommended**: it raises that pop-out (the popped pane's window or the session pop-out) and clears attention; an unbound session with a session pop-out is not also placed in the active tab. Worst case: to put it in the grid the user closes the pop-out or uses Add to current tab. (b) The click acts as today (jump to the pane's card, or place an unbound leaf) and the pop-out stays. Worst case: the same session drawn twice.
14. **What does a centre drop on a pane in another tab do?** The daemon gives the destination pane the dragged session and leaves the destination's old session unbound. (a) **Recommended**: keep that, with the displaced session named in the undo entry. Worst case: a session drops off the screen until Undo or the Unbound list. (b) Swap across tabs: the client sends two `ReplacePaneSession`s. Worst case: the two sends are not atomic, and a failure between them leaves one session in two panes. (c) A centre drop across tabs acts as a drop on the pill (balanced placement). Worst case: the drop does not land where the overlay pointed.

## Phase 6 — remote and cutover

- Connection picker, LAN pairing and the pinned-TLS tunnel, built on the shared client-core crate the mobile app also uses (MA1 in [mobile-app.md](./mobile-app.md)), recovered from `remote.rs` on the `tauri-last` tag.
- Autostart: `remote.rs` and `autostart.rs` are recovered from the `tauri-last` tag.
- Remote file transfer's client half ([remote-file-transfer.md](./remote-file-transfer.md), FT.2–FT.4).
- Cutover: the installer ships the native client, and on uninstall deletes the daemon's `rustling-tulip-daemon` login `Run` value; update CLAUDE.md.
