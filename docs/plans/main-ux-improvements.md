# Main UX improvements plan

Follow-up plan from the 2026-05-13 main-UX review. The app already has strong
foundational pieces: durable daemon sessions, tab/pane grids, source control,
settings, pop-outs, and an e2e harness. The next UX pass should focus on trust,
orientation, and reducing repeated-session friction.

## Goals

- Make session lifecycle actions predictable and recoverable.
- Keep session identity stable even when terminals emit noisy titles.
- Let users visually identify important shells with quick preset colors or a
  custom color.
- Reduce the chance of launching an agent with more authority than intended.
- Make the common launch path fast without hiding advanced control.
- Make non-repo shell launches cheap for a default folder and precise for any
  user-chosen folder.
- Improve source-control usability inside the narrow activity sidebar.
- Replace cryptic controls with affordances users can discover quickly.
- Add undo/recovery where actions mutate tabs, panes, sessions, or worktrees.
- Make preset launches observable and cancellable enough for large batches.

## Non-goals

- No broad visual redesign. Keep the existing dark, dense, VS Code-like shell.
- No new cloud service, account system, or sync layer.
- No automatic repo discovery unless a later plan explicitly takes it on.
- No replacement of terminal-first git workflows with a full IDE.
- No speculative workflow engines. Add protocol surfaces only for outcomes this
  plan actually needs.

## Cross-cutting validation

- [ ] For every UI-facing slice, capture before/after screenshots with the
      isolated `tools/e2e` host or WDIO specs.
- [ ] Add a focused e2e regression for each deterministic behavior change.
- [ ] Use hand testing only for feel, density, drag nuance, OS dialogs, and
      anything not reliably exposed through WebDriver.
- [ ] Keep `.tmp/` outputs untracked and do not write screenshots into docs.
- [ ] Run the narrow frontend/Rust checks for files touched by each slice.

## Phase 1 - Stop, removal, and recovery semantics

### Pain point

Today, `Stop` reads like "kill the process but leave the session record here".
In the reviewed app run, stopping an interactive worktree-backed session removed
the session from the pane and sidebar, leaving only an empty pane. That makes the
button feel closer to "Stop and remove" than "Stop".

### Desired behavior

`Stop` should terminate the underlying process and leave the user at a stopped
session surface with next actions. Removal should be a separate explicit action:
remove pane, park session, delete worktree, or discard the session record.

### Lifecycle vocabulary

- **Running**: the session record has a live PTY/headless handle or is waiting
  for one during spawn.
- **Stopped**: the process is no longer running, but the session record and any
  pane bindings remain so the user can restart, remove the pane, park the
  worktree, or discard the record.
- **Parked**: the process is stopped, the pane binding is removed, and the
  session remains inactive in the sidebar with its worktree retained.
- **Discarded**: the session record is removed from the daemon registry and any
  pane/sidebar references are pruned.
- **Orphan**: the daemon still has a session record but lost the live I/O
  handle while the child process may still exist.
- **Abandoned**: the daemon recovered a sidecar for a session whose child
  process is gone, so the record can only be resumed by spawning a replacement.

### Tasks

- [x] Define lifecycle vocabulary in one place: running, stopped, parked,
      discarded, orphan, abandoned.
- [x] Split protocol/daemon behavior so explicit `stop_session` retains a
      stopped `SessionSnapshot` instead of pruning it from tabs.
- [x] Keep `discard_session` as the destructive "remove record and optionally
      remove worktree" path.
- [x] Audit self-exit behavior separately from user-clicked Stop; keep or
      intentionally revise the current auto-discard behavior for sessions
      without per-session worktrees.
- [x] Update `SessionPane` stopped state to offer `Restart`, `Remove pane`,
      `Keep worktree`, and `Remove worktree` actions where applicable.
- [x] Rename any remaining destructive labels that still imply a weaker action.
- [x] Update pane-close and exit wording so "stop", "remove", "park", and
      "delete worktree" are used consistently.
- [x] Add e2e coverage: click `Stop`, confirm the stopped placeholder remains.
- [x] Add e2e coverage: remove/discard path actually removes the pane/sidebar
      session and cleans worktree only when requested.

## Phase 2 - Stable session identity

### Pain point

Terminal-emitted titles can be noisy. The review run showed the primary session
title as `C:\Windows\system32\cmd.exe`, while the useful repo/branch context was
only a chip. That weakens orientation in the sidebar, tab, pane, and pop-out
surfaces.

### Desired behavior

The primary display name should be stable and user/workflow-oriented. Terminal
titles should remain visible, but as secondary context unless the user explicitly
renames the session to that value.

### Tasks

- [x] Define display precedence: explicit user label, canonical repo/workspace
      label, then fallback runtime label.
- [x] Demote OSC/terminal title to tooltip or secondary chip by default.
- [x] Keep a visible runtime chip (`claude`, `codex`, `pwsh`, `cmd`, etc.).
- [x] Update `sessionDisplayLabel` and all callers to use the new hierarchy.
- [x] Ensure custom rename still wins over daemon-generated and terminal titles.
- [x] Review sidebar row truncation so the canonical label remains readable.
- [x] Add a per-session color setting that applies to that shell's pane gutter
      and sidebar tree row.
- [x] Offer 12 named default color presets for fast assignment plus a custom
      color picker.
- [x] Preview preset colors in the color submenu using the same tree-row accent
      treatment the selected session will get.
- [x] Persist the chosen color with the session record and restore it after
      daemon/app restart.
- [x] Add e2e coverage: an emitted `cmd.exe` terminal title does not replace
      the primary repo/branch label.
- [x] Add e2e coverage: a user rename becomes the primary label everywhere.
- [x] Add e2e coverage: assigning a preset color updates the pane gutter and
      sidebar tree row.
- [x] Add e2e coverage: a custom color survives session refresh/reload.

## Phase 3 - Safer spawn defaults and authority visibility

### Pain point

Fresh settings previously defaulted to trusted launch
(`--dangerously-skip-permissions` / `--yolo`). That is convenient but risky for
a launcher that can run agents across worktrees and workspaces. The spawn dialog
surfaces the checkbox, but the default was easy to accept without thought.

### Desired behavior

Safe should be the default for new installs. High-authority launches should be
visible at spawn time and on the running session. Existing user preferences can
be honored, but new defaults should not silently grant broad authority.

### Tasks

- [x] Change the new-install default for trusted launch to off.
- [x] Preserve existing saved user settings through migration or explicit
      compatibility logic.
- [x] Add a clear "trusted launch" visual state when enabled in the spawn
      dialog.
- [x] Add a session header/sidebar badge for sessions launched with elevated
      authority.
- [x] Reword Claude and Codex permission labels into a consistent vocabulary.
- [x] Decide whether "launch last" can replay elevated authority without
      confirmation; rule: elevated launch-last opens the full spawn dialog for
      explicit review instead of replaying immediately.
- [x] Add e2e coverage for default setting on a fresh config dir.
- [x] Add e2e coverage that elevated sessions show a visible badge.

## Phase 4 - Faster common spawn path

### Pain point

The spawn dialog is powerful, but the common path is heavy: runtime, placement,
target, worktree mode, branch, base branch, run mode, permissions, and advanced
config are all present at once. Repeated launches should require less ceremony.

### Desired behavior

The default path should feel like "launch the thing I normally launch here",
with advanced configuration still one click away. The user should be able to
inspect or override the full configuration before committing.

Primary launch presets:

- Repo/workspace row: replay the saved spawn config in the current tab when it
  exists; otherwise open the full spawn dialog scoped to that container.
- Empty pane: open the full spawn dialog and route the result back into that
  pane.
- Existing tab: open the spawn dialog with that tab pinned as the placement
  target.

### Tasks

- [x] Define the primary spawn presets for a repo, workspace, empty pane, and
      existing tab.
- [x] Add a quick action for spawning a non-repo-tied shell in the user's
      default folder.
- [x] Add a modal path for spawning a non-repo-tied shell in a specific folder.
- [x] Persist and display the default folder used by non-repo shell quick
      launches.
- [x] Ensure non-repo shell sessions are clearly grouped as standalone, not
      under any registered repo/workspace.
- [x] Add a compact "Launch last" or "Launch default" path where a last
      spawn config exists.
- [x] Show a concise summary of the config that will be replayed before launch.
- [x] Keep branch/worktree controls visible only when they affect the selected
      launch mode.
- [x] Consider moving base branch into advanced/new-worktree details unless
      it differs from the repo default.
- [x] Make "current tab", "new tab", and explicit tab placement read as a
      compact segmented placement control.
- [x] Add e2e coverage for launch-last using the saved spawn config.
- [x] Add e2e coverage for switching from compact path to full edit path.
- [x] Add e2e coverage for quick non-repo shell launch into the default folder.
- [x] Add e2e coverage for modal non-repo shell launch into a chosen folder.

## Phase 5 - Source Control density and focus

### Pain point

The Source Control sidebar has a lot of value in a narrow column: commit box,
clean/changes state, stashes, repo metadata, forge link, history, and diff
placeholder. In the review screenshot, history rows were cramped and a
horizontal scrollbar appeared.

### Desired behavior

Changes and commit actions should remain the first-class source-control task.
History should be available without competing for the same narrow vertical and
horizontal space when the user is trying to stage/commit.

### Tasks

- [x] Revisit default Source Control split sizes and minimum heights.
- [x] Remove avoidable horizontal overflow in history and diff-placeholder
      areas.
- [x] Add a collapsed or focus mode for History when the Changes section needs
      space.
- [x] Keep the commit message box visible only when useful, or collapse it
      when the working tree is clean.
- [x] Surface stash count without forcing stashes to consume vertical space.
- [x] Add a "refresh" affordance for status/history if automatic updates are
      intentionally limited.
- [x] Show history authors as appended chips so subject text remains the
      flexible, truncated field.
- [x] Add e2e coverage that the Source Control sidebar has no horizontal
      overflow at default width.
- [x] Add screenshot checks for clean repo, dirty repo, and history-selected
      states.

## Phase 6 - Discoverable controls and icon affordances

### Pain point

Several controls are efficient once learned but cryptic on first contact:
activity glyphs, pane split icons (`>|`, `v=`), extract, close, and disabled
toolbar actions. Tooltips help after hover, but the base state should communicate
the action more clearly.

### Desired behavior

Common controls should be recognizable before hover. Rare controls can stay
compact but need consistent labels, accessible names, and grouping.

### Tasks

- [x] Inventory icon-only controls across activity bar, tab bar, pane header,
      sidebar rows, source-control rows, and pop-out windows.
- [x] Replace ASCII glyphs with a consistent icon set or a local minimal icon
      component, without adding a dependency unless justified.
- [x] Replace activity-bar and pane split/extract/close glyphs with local SVG
      icons.
- [x] Replace sidebar container action and tab close/new glyphs with local SVG
      icons.
- [x] Group pane actions visually: split, move/pop-out, close.
- [x] Prefer text labels for destructive or uncommon actions where space allows.
- [x] Make per-shell color controls discoverable from the session context menu
      and session header without crowding common stop/split controls.
- [x] Make disabled toolbar actions visually and textually explain why they are
      disabled.
- [x] Ensure every icon-only button has a useful `aria-label` and tooltip.
- [x] Add e2e/a11y assertions for accessible names on activity and pane
      icon-only controls.
- [x] Add e2e assertions for disabled sidebar toolbar reasons.
- [x] Capture screenshots for normal and small-pane headers.

## Phase 7 - Undo and recovery shelf

### Pain point

The old UX audit still tracks undo-last-action and longer tab-close undo as open
substantial features. That matters because this app mutates long-lived state:
tabs, panes, sessions, worktrees, daemon state, and source-control changes.

### Desired behavior

Reversible actions should produce a short-lived recovery affordance. Destructive
actions that cannot be undone should remain explicit confirmations.

### Tasks

- [x] Classify actions as reversible, replayable, or destructive.
- [x] Start with close-tab undo as the first reversible UI-state action.
- [ ] Extend undo to close pane, move pane, and remove session binding.
- [x] Add an undo shelf/toast host that can show one or more recent actions.
- [x] Keep enough local state to restore a closed tab without guessing.
- [x] Add daemon-backed `restore_tab` for exact tab reinsert/active-state undo.
- [x] Do not offer undo for worktree deletion, discard changes, stash drop, or
      daemon stop unless the underlying operation is genuinely reversible.
- [x] Add e2e coverage: close tab then undo restores the tab and active state.
- [ ] Add e2e coverage: close pane then undo restores the pane layout.

### Action classification

- [x] Reversible UI-state actions: close tab, close pane, move pane, extract pane
      to a tab, bind an unbound session into a tab, tab reorder, and sidebar
      reorder. These can be undone with recent local snapshots plus existing
      tab/pane commands.
- [x] Replayable session actions: launch last, duplicate session, resume
      inactive session, resume abandoned session, and preset launch. Undo should
      not pretend to reverse these; the safer recovery path is a clear stop or
      dismiss action for the spawned session(s).
- [x] Destructive or external actions: worktree deletion, source-control discard,
      stash drop, daemon stop, repo/workspace removal, and terminal process stop.
      Keep explicit confirmations; do not show undo unless the underlying data is
      actually restorable.

## Phase 8 - Preset launch observability and cancellation

### Pain point

Preset launch is currently one-shot and warns that there is no in-app cancel
once the daemon starts spawning. That is acceptable for small presets, but a
mistaken large launch becomes manual cleanup.

### Desired behavior

Launching many prompts should look like a queue with progress. The user should
be able to cancel before the next session spawns and see what already launched.

### Tasks

- [ ] Model preset launch as a daemon-visible job with id, target, prompt count,
      created sessions, status, and error state.
- [ ] Add progress events: resolving prompts, running script commands,
      spawning N of M, completed, cancelled, failed.
- [ ] Add a cancel message that stops future spawns and leaves already-created
      sessions alone unless the user explicitly cleans them up.
- [ ] Update `PresetLaunchDialog` to show progress after submit instead of
      closing immediately for large jobs.
- [ ] Add a compact preset-job toast or sidebar entry after the dialog closes.
- [ ] Make partial-launch cleanup discoverable: select launched sessions, stop
      all from this preset, or leave them running.
- [ ] Add daemon tests for cancel-before-first-spawn and cancel-mid-launch.
- [ ] Add e2e coverage for a multi-prompt preset: progress appears, cancel
      stops later spawns, existing sessions remain visible.

## Suggested sequence

1. [x] Phase 1: Stop, removal, and recovery semantics.
2. [x] Phase 2: Stable session identity.
3. [x] Phase 3: Safer spawn defaults and authority visibility.
4. [x] Phase 4: Faster common spawn path.
5. [x] Phase 5: Source Control density and focus.
6. [x] Phase 6: Discoverable controls and icon affordances.
7. [ ] Phase 7: Undo and recovery shelf.
8. [ ] Phase 8: Preset launch observability and cancellation.

## Open decisions

- [x] Existing installs keep trusted launch enabled when that value is
      already saved; new installs seed it off.
- [x] Should explicit Stop always retain a stopped session, even for sessions
      without a per-session worktree?
- [x] Should terminal OSC titles ever become primary automatically, or only
      after the user chooses to promote/rename?
- [ ] Should Source Control History be a collapsible section, a separate sidebar
      mode, or a secondary pane below Changes?
- [ ] Should the undo shelf be app-global or scoped to the active tab/sidebar?
