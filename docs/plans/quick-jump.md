# Quick jump and typed target search

Design for two native-client features that share one matcher: the jump palette (RT-930), opened with Ctrl+Shift+P and typed partial-match search in the spawn dialog's Target list (RT-931, with RT-692). Every decision below is the orchestrator's unless marked as the owner's; each is open to the owner.

## What the owner asked

- RT-930: Ctrl+K takes the user to an existing pane, tab or scroll position, and can pre-select a repo or workspace in the sidebar.
- RT-931: the New Session popup's Target combobox takes typed searches with partial-match autocomplete, so "ya" finds "[WS] yaat".

## Decisions

1. **One matcher, plain Rust.** A new module `apps/native/src/fuzzy.rs` ranks candidates against a query, with unit tests; no new dependency (`Cargo.lock` has no fuzzy crate, and the need is small). Matching is case-insensitive over a candidate's searchable text. A candidate matches when every query character appears in order. It ranks by match class first, then by the order the caller gives:
   - a prefix of the whole text;
   - a prefix of a word (after a space, `-`, `_`, `/`, `\` or `.`);
   - a contiguous substring;
   - an in-order subsequence.

   An empty query matches everything in the caller's order.
2. **Searchable text excludes kind tags.** The `[REPO]`, `[WS]` and `[DIR]` tags and the remote ` · host` suffix are drawn but not matched as the name; the host name is matched as a second, lower-ranked field, so "ya" finds "[WS] yaat" by its name and "build" finds a target on the `buildbox` host.
3. **Typed search in the Target list (RT-931).** When the Target list is open, typing goes into a query field at the top of the list, and the list narrows to the matching targets, grouped per host as today, ranked within each group by decision 1. The highlight moves to the best match after each edit. Enter picks the highlight; Up and Down move through the narrowed list; Escape clears a non-empty query, and closes the list when the query is empty. Typing a letter while the closed Target field has keyboard focus opens the list with that letter as the query. No match shows one dimmed row, "No matching repo or workspace". State lives in `spawn_form.rs` (the query, a `filtered_targets()` that every index-taking method uses); `spawn_view.rs` renders.
4. **RT-692 lands with RT-931.** The narrowed list must scroll to its highlight, which is RT-692's first half; clicks inside the list (rows, scrollbar, query field) must not close it, its second half. One brief does both: a `ScrollHandle` on `SpawnDialog` for the Target list, scrolled to the highlight after each move or edit, and an outside-click test that counts the deferred list as inside.
5. **The palette (RT-930).** Ctrl+Shift+P (Cmd+Shift+P on macOS) opens a palette over the main window: a query field and a ranked list, in the look of the existing popups (`popup_panel`), centred near the top of the window. The key is the owner's ruling: it is the palette key of VS Code and Windows Terminal, it matches the app's other Ctrl+Shift+letter actions, and it leaves Ctrl+K (readline's and Claude Code's kill-to-end) to the terminal. The key is taken in the root key capture before the terminal sees it, like Ctrl+Shift+N. Pressing it again, or Escape, closes the palette. In a pop-out window the palette opens in the main window.
6. **Palette entries**, across every connected host, each with its kind tag:
   - `[PANE]`: each pane on screen in any tab (its session's label); Enter activates its tab and focuses the pane (`show_session_on`).
   - `[TAB]`: each tab (its title); Enter activates it (`show_tab`).
   - `[SESSION]`: each session in the sidebar that is in no pane; Enter opens it the way a sidebar click does.
   - `[REPO]`, `[WS]`, `[DIR]`: each sidebar container; Enter selects it in the sidebar (decision 7).

   With an empty query the order is panes and tabs by last focus, then sessions, then containers in sidebar order; a typed query ranks by decision 1 with that order as the tiebreak.
7. **"Scroll" and "pre-select" mean a sidebar selection.** Jumping to a container (or a session not in a pane) expands its folded parents, gives its row the sidebar's keyboard focus highlight, and scrolls the sidebar so the row is in view. A selected container becomes the **preselected target**: the next New Session opened from the toolbar, the menu or Ctrl+Shift+N starts with it as the Target, ahead of the focused pane's session (`spawn_form::initial_target` gains a `preselected` input that wins). The preselection clears when the user clicks another sidebar row, focuses a pane, or a spawn uses it. The sidebar list gains a `ScrollHandle` and a plain-Rust row-offset helper.
8. **Testable core, thin view.** Palette state (entries, query, ranking, highlight, Enter's `JumpAction`) lives in `apps/native/src/quick_jump.rs` with unit tests; `quick_jump_view.rs` renders it; `lib.rs` (`on_key_capture`, `RootView`'s overlay layer, a new `Overlay` variant in `overlay_owner.rs`) only routes. UI specs: `apps/native/tests/ui/ui_quick_jump.rs` (new, with a `tools/test-map.json` row) and the spawn specs `ui_spawn_combobox.rs`, `ui_spawn_dialog.rs`. A `jump` view in `native-shot` shows the palette.

## Steps

- [x] QJ.1 (RT-931 with RT-692): `fuzzy.rs` with its unit tests; the Target list's query, narrowing, highlight, scroll to highlight and inside clicks; spawn specs.
- [ ] QJ.2 (RT-930): `quick_jump.rs` entries and actions; the preselected target in `initial_target`; the sidebar's scroll handle and row-offset helper.
- [ ] QJ.3 (RT-930): the palette view, the Ctrl+Shift+P and Cmd+Shift+P route, the `jump` shot view, `ui_quick_jump.rs`, and a shortcut list in `docs/native-client.md`.

QJ.2 and QJ.3 build on QJ.1's matcher, so RT-930 waits on RT-931.
