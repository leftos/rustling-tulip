# Native client: open items' rulings

The rulings the native client's open items need before they are briefed. Each item is one line in [MAIN.md](./MAIN.md), which orders them; the client's settled design is in [docs/native-client.md](../native-client.md), and what each feature must do is in [native-client-parity.md](./native-client-parity.md), whose lines are ticked as items land. An item's section here is deleted in the commit that lands it; the file goes when the last one does.

## Working rules

- **Landing:** each item is committed to `main` as soon as it is green, and pushed at checkpoints (a finished wave), not after every item.
- **Autonomy:** drive the native client to done without stopping between items or waves. Technical decisions are settled in the session and recorded here as rulings; only questions about behaviour the user will see go to the user. Phases 5 and 6 are split into brief-sized items from their parity sections when their wave comes up, and the split goes to the user before anything is dispatched.
- **Phase 4 scope:** its named features plus every parity line not claimed by Phase 5 (drag-and-drop, pop-outs) or Phase 6 (remote). Every item mounts into `RootView` (`lib.rs`), so concurrent items rebase onto `main` before landing. Concurrency ceiling 3. Dependencies: P4.11 → P4.15b (the launch path); P4.4b → P4.12c (the target lock). P4.16 changes the counts P4.2, P4.3 and P4.13a built, all landed.

## P4.4b Worktrees tab and Manage worktrees

The Worktrees settings tab (root path, Browse, Save, Reset, override indicator) and the Manage worktrees modal. Parity: "Worktrees".

- Settled from Tauri (orchestrator): the manager opens only from the Settings button and closes Settings when it launches a session; daemon errors stay toasts (no `request_id` added); Refresh is disabled while a snapshot is pending; Delete all stale sends one `DeleteWorktreeAt` per path; confirms focus Cancel; "Launch session here" opens the spawn dialog with the target locked and the existing worktree pinned.
- Rulings (user): a row's title is the group's `.rt-group` name, with status, path, branch, size and age under it; an older-layout folder with no marker shows its path as the title.
- Settled (orchestrator): the folder picker gets a seam in `RootDeps` so Browse is specced (GPUI's test platform can't prompt), and Shell… uses it too; a bulk delete clears pending once every delete has answered (any `Error` counts); `Saved` shows only after `WorktreesRootChanged` echoes the saved path; one shared age/size format (the spawn form's); the members list is a `N members ▸` toggle in the focus ring; a pinned worktree missing from the list stays as a synthetic option; the locked target shows as its disabled chip; closing the manager without launching returns focus to Settings' `Manage worktrees…`.
- This item builds the spawn form's target lock and worktree pin, which P4.12c reuses.

## P4.10 Repos and workspaces

+ Repo with a folder picker that remembers its last folder, + Workspace, remove repo / workspace (inline two-click; with live sessions a dialog), the workspace creator (from repos or a `.code-workspace` file), the "VS Code workspace detected" prompt, "Add repo" / "Add workspace" on DIR and SH containers, and the no-repos states in the sidebar, the main area and the spawn dialog. Parity: "Toolbar", "Remove repo/workspace", "DIR/SH containers", "Workspace creator", "VS Code workspace detected", "Empty states", "Target picker", "File pickers remember…".

## P4.11 Containers

Launch last (▶, double-click, "Launch last again ▸" current / new / named tab / edit first; trusted configs open the dialog), the full container menu (spawn here / in tab, presets entry, Explorer, VS Code, copy path, Remove), the container row's keyboard fold and last-launch summary, the Detached banner and its stop all, "Resume all (N)", and the spawn dialog's container and tab-container entry points. Parity: "Container row", "Container context menu", "Launch last again", "Repos view" banner, "Detached container stop all", "Entry points".

## P4.12 Sessions

The session menu's Duplicate ▸ (Shift opens the prefilled dialog), Move to ▸, Add to current / new tab and Reveal worktree; the pane header's status dot, runtime, trusted and headless chips and repo:branch chips; the abandoned overlay (last prompt, Resume / Dismiss) and orphan banner; the leaf's tooltip, trusted marker and orphan / abandoned / inactive tags with inline Resume / Dismiss; the display-label order; auto-discard of worktree-less sessions that exit on their own. Parity: "Session context menu", "Pane header", "Abandoned overlay", "Session leaf", "Display label order", "Sessions without a worktree…".

- Split (orchestrator): **P4.12a** labels everywhere through `display_label`, the label tooltip, leaf tags and inline buttons, pane header chips, abandoned overlay, orphan banner, auto-discard; **P4.12b** the menu rows (Duplicate ▸, Move to ▸, Add to / Open in new tab, Reveal); **P4.12c** Shift-duplicate prefill, on the target lock P4.4b builds.
- Rulings (user): Reveal opens the worktree folder itself (as Tauri does), shown only for sessions with their own worktree; auto-discard skips headless sessions, which stay so their stats can be read; with a diff tab active, "Add to current tab" becomes "Add to new tab" and opens one, as a leaf click does.
- Settled (orchestrator): Duplicate into an existing tab places by `pane_target_for_session`; Duplicate and Move to list grid tabs only; submenus swap rows with ‹ Back; member chips get a second header row only for sessions with members; the orphan banner names the session's runtime instead of Tauri's fixed "claude"; auto-discard also skips abandoned sessions; the rename field keeps native's seed; a Shift-duplicate with no stored config opens the dialog with defaults on the source's repo.
- P4.12c: `SpawnForm::open` seeds the Spawn defaults (trusted, approval mode, Codex sandbox) and has no prefill of those three fields yet; the duplicate prefill must beat the defaults.

## P4.13c Undo shelf

Settled from Tauri (orchestrator): undo covers tab close (not Close others or merge), close pane only, and move to an existing tab; the shelf is its own bottom-centre layer, 8 s, at most 3 entries, buttons only. Parity: "Undo shelf".

## P4.15 Preset wizard

Split in two. **P4.15a** sources (file, folder, inline, GitHub issue ranges) and variables (toggle, file, folder, text, required). **P4.15b** preview (grouped by tab, max panes per tab, script commands) and launching (progress, counts, Cancel, Select launched, Stop all), sticky progress and failure toasts one per job, the first tab created made active, preset-launched sessions highlighted; needs P4.11. Parity: "Preset wizard", "Sticky preset progress…", "Preset-launched sessions highlighted".

## P4.16 Exclude a session from busy tracking

New, not a parity line. A per-session setting so a session that is always busy (a dev server, a watcher) stops counting as busy. Rulings (user): an excluded session leaves the window title's M, the tab's busy badge and the leaf's attention highlight; it still sends OS notifications. The flag lives on the daemon's session, like the appearance overrides, as an additive `#[serde(default)]` protocol field, so every client sees it. Resume and session recovery keep it; Duplicate does not. It is toggled by a checkable "Don't count as busy" item in the session menu, and an excluded leaf carries a small muted marker.

## P4.17 Keep an untouched layout when panes come and go

New, not a parity line. When a tab's layout was last set by picking a layout type (side by side, grid, …) and the user has not resized its panes since, adding or removing a session rebuilds that same layout type over the new pane count, so side by side stays evenly balanced. Once the user drags a divider, the tab keeps its sizes and adding or removing a pane falls back to today's behaviour. Rulings (user): the "untouched since picked" flag is per client, since two clients can show the same sessions in different tabs and layouts (a laptop attached remotely beside the desktop's big screen); it is saved with that tab's layout, which the daemon already keys by client id. Both pickers set it: Rearrange ▸ and the first-connect chooser.

## Phase 5 — windows and drag-and-drop

Pane / tab / session pop-outs as windows of one process, pane drag-and-drop with edge overlays, sidebar and tab drag-to-reorder. Split into items from the parity checklist's drag-and-drop and pop-out lines when its wave comes up.

## Phase 6 — remote and cutover

- Connection picker, LAN pairing and the pinned-TLS tunnel, built on the shared client-core crate the mobile app also uses (MA1 in [mobile-app.md](./mobile-app.md)), recovered from `remote.rs` on the `tauri-last` tag.
- Autostart: `remote.rs` and `autostart.rs` are recovered from the `tauri-last` tag.
- Remote file transfer's client half ([remote-file-transfer.md](./remote-file-transfer.md), FT.2–FT.4).
- Cutover: the installer ships the native client, and on uninstall deletes the daemon's `rustling-tulip-daemon` login `Run` value; update CLAUDE.md.
