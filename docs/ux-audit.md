# UX audit

A code-evidence audit of the rustling-tulip desktop client surface (Tauri shell + React/xterm.js frontend) plus a parallel checklist of behaviours that need a human at the keyboard to validate. The findings only cover UX-visible issues; architecture/refactor concerns and items called out as deferred in `CLAUDE.md` are out of scope.

**Tracking convention.** Each finding is a `- [ ]` / `- [x]` checkbox. Resolved items keep an `(iter N)` annotation only — the original "Suggested direction:" prose stays for context, but the long "Resolved" paragraphs that used to live here got compressed in 2026-05-11 to keep the doc scannable. The full design reasoning for any closed finding lives in the matching commit message + the iter entry in `docs/plan.md` (iters 14–48 verbose; iters 49+ one-liners).

## Table of contents

- [Section 1 — Code-evidence findings](#section-1--code-evidence-findings)
  - [Sidebar / repo & workspace management](#sidebar--repo--workspace-management)
  - [Spawn dialog](#spawn-dialog)
  - [Tabs + panes (grid, drag/drop, pop-out)](#tabs--panes-grid-dragdrop-pop-out)
  - [Terminal](#terminal)
  - [Git panel](#git-panel)
  - [Preset launch](#preset-launch)
  - [Exit flow](#exit-flow)
  - [Global: keyboard, focus, accessibility, theming, error surfacing](#global-keyboard-focus-accessibility-theming-error-surfacing)
  - [Dev / testing workflow](#dev--testing-workflow)
- [Findings from hand-test 2026-05-11](#findings-from-hand-test-2026-05-11)
- [Section 2 — Hand-test checklist](#section-2--hand-test-checklist)

---

## Section 1 — Code-evidence findings

### Sidebar / repo & workspace management

- [x] **Destructive remove with no confirmation (inline × button).** Clicking the `×` next to a repo or workspace fires `remove_repo` / `remove_workspace` immediately — no "Are you sure?" or undo. *Resolved (iter 3).*
- [x] **Removing a repo silently abandons its live sessions.** Daemon's `remove_repo` retain-filters the registry; sessions stay alive but their container disappears and they re-appear under "Detached". *Resolved (iter 3 — RepoRemoveDialog modal lists active sessions + 3-way choice.)*
- [x] **No name-uniqueness on workspaces.** *Resolved (iter 25.)*
- [x] **`Sidebar` accepts a `connection` prop but never renders it.** *Resolved (iter 21 — header connection badge for non-`open` states.)*
- [x] **Force-expand silently overrides the user's collapse state.** *Resolved (iter 23 — attentionSessions dropped from forceExpand; container header gains attention chip instead.)*
- [x] **Attention flag is never cleared automatically.** *Resolved (iter 16.)*
- [x] **`session_removed` leaks the id in `attentionSessions`.** *Resolved (iter 16.)*
- [x] **Preset context-menu loading state never times out.** *Resolved (iter 38 — discriminated `{ok, entries}|{ok:false, reason}`.)*
- [x] **Sidebar context menu is anchored to raw cursor coords with no bounds check.** *Resolved (iter 25 — `clampMenuCoord` in `utils/a11y.ts`.)*
- [ ] **No empty state for a sidebar that has workspaces but only orphan/detached sessions.** The empty-state copy fires only when `containers.length === 0`. A detached bucket is `kind === "detached"` and counts as a container, so the user sees "Detached" with red tag and a session list but no narrative explaining what to do about it.
  - File: `apps/tauri-app/src/components/Sidebar.tsx:550-561`.
  - Suggested direction: hover title is good; add an inline action like "stop all" or "re-register repo" near the bucket header. (Iter 4 added a banner; the open work is the inline action.)

### Spawn dialog

- [x] **Repo dropdown can be empty.** *Resolved (iter 27 — `EmptyRepoState` panel.)*
- [x] **Backdrop click discards all typed input with no warning.** *Resolved (iter 27 — backdrop click no longer closes.)*
- [x] **No loading state after Submit.** *Resolved (iter 42 — info-severity toast.)*
- [x] **Spawn failures surface only as `console.error`.** *Resolved (iter 2 + 22 — error toast + intent disarm.)*
- [x] **`spawning` and `working` share the same accent-blue dot.** *Resolved (iter 23 — hollow ring vs solid.)*
- [x] **Workspace form has no "Cancel preview" / refresh affordance.** *Resolved (iter 18 — preview-reset depends on `useWorktree`.)*
- [x] **Random worktree branch name regenerates every dialog open.** *Resolved (iter 41 — `branchSuggestionCache` keyed on target.)*
- [x] **`Spawn → Spawn` double-click submits twice.** *Resolved (iter 18 — `submittedRef`.)*
- [x] **Permission-mode dropdown silently retains a value while disabled.** *Resolved (iter 24 — explicit hint.)*
- [x] **Env var rows have no key-uniqueness or `KEY=value` syntax check.** *Resolved (iter 27 — regex + duplicate detection.)*
- [x] **Worktree-default toggle persists immediately on click.** *Resolved (iter 42 — local-only until submit.)*

### Tabs + panes (grid, drag/drop, pop-out)

- [x] **All freshly-created tabs are named "Tab".** *Resolved (iter 17 — next-free default name.)*
- [x] **Merge-tabs is hard-coded to `tile_horizontal` layout.** *Resolved (iter 28 — horizontal/vertical picker.)*
- [x] **Merge doesn't switch to the new tab.** *Resolved (iter 17 — `onArmNextNewTab`.)*
- [x] **Tab pill close × has no confirmation.** *Resolved across iters 3 (bound sessions) + 45 (2+ panes). Closing a multi-pane or session-bearing tab now arms a two-state confirm.*
- [x] **Context-menu "Close other tabs" sends one `close_tab` per other tab in a loop.** *Resolved (iter 3 — two-state confirm; N-message loop preserved as protocol scope.)*
- [x] **Reorder is daemon-round-tripped, no optimistic update.** *Resolved (iter 41 — `onLocalReorder`.)*
- [x] **Drag a tab onto itself: nothing happens but no feedback either.** *Resolved (iter 48 — source pill gets `is-dragging` class at 45% opacity, so self-target is legibly different from a real drop edge.)*
- [x] **Pane drag onto a tab pill activates the tab but offers no drop target.** *Resolved (iter 45 — pill `onDrop` accepts `text/x-rt-pane` and routes via `move_pane` with `edge: "right"`.)*
- [x] **`split_pane` is hard-wired to `place: "second"`.** *Resolved (iter 46 — Shift+click on split-right/down inverts to `place: "first"`; tooltip advertises.)*
- [x] **After a split, focus stays on the source pane.** *Resolved (iter 29 — `onArmFocusNewPane`.)*
- [x] **`Move to new tab` in pane context menu is silent.** *Resolved (iter 17 — `onArmNextNewTab`.)*
- [x] **Pane controls overlay covers the session header on small panes.** *Resolved (iter 37 — hover/focus-within only.)*
- [x] **Pop-out session window never auto-closes when its session is stopped or removed.** *Resolved (iter 19.)*
- [x] **Pop-out session window has TWO Stop buttons with different behaviours.** *Resolved (iter 19.)*
- [x] **`isPopoutWindow` in `SessionPane` only checks `?session`, not `?tab`.** *Resolved (iter 19.)*
- [x] **Pop-out tab window's "Spawn one here" is a no-op.** *Resolved (iter 34 — drag-in hint instead.)*
- [x] **TabWindow has no chrome controls.** *Resolved (iter 21 — Close window button + ellipsised tab name.)*
- [ ] **TabWindow's focusedPaneId state is independent from the main window's.** Each window's pane focus is tracked locally. Probably intentional — flagged in hand-test.
  - File: `apps/tauri-app/src/components/TabWindow.tsx:21`.
- [ ] **`SessionWindow` ignores the daemon-emitted `Repos` / `Workspaces` payloads.** Each pop-out window opens a fresh WebSocket and receives the initial state, but doesn't subscribe to repo changes — irrelevant for it directly, but if the session disappears nothing flushes the stale snapshot. (Iter 19's close-on-disappear handles the session-removed case; the repo/workspace listener gap remains.)

### Terminal

- [x] **Body-level `user-select: none` prevents selecting diff text and headless logs.** *Resolved (iter 1 — selection re-enabled on `.diff-pane` / `.headless-log` / `.preset-preview-list` / `.terminal-host` / `.session-title h2`.)*
- [x] **Orphan banner instructs to "Stop the session and spawn a new one" — but Stop on an orphan does nothing to the underlying claude process.** *Resolved (iter 34 — `orphan::kill_pid` + banner copy updated.)*
- [x] **No reconnection on socket close.** *Resolved (iter 31 — exponential backoff capped at 10 s.)*
- [x] **No copy-paste hot-keys in xterm config.** *Resolved (iter 44 — Ctrl+Shift+C/V via `attachCustomKeyEventHandler`.)*
- [x] **Headless `recent_actions` list grows unbounded in the UI.** *Resolved (iter 36 — tail 200 + "Show all".)*
- [x] **PTY input is sent on every keystroke even when the session is stopped.** *Resolved (iter 40 — `statusRef` guard.)*

### Git panel

The per-session `GitPanel` is gone since iter 7 (replaced by the global Source Control sidebar). Remaining items below are tracked against either the sidebar or stale code that no longer exists.

- [x] **"Open in forge ↗" goes to the repo home, not the branch.** *Resolved (iter 2 — `branchUrl` builds `/tree/<branch>` / `/-/tree/<branch>` / `/src/<branch>`.)*
- [x] **`openInForge` listener leak.** *Resolved (iter 2 — Promise-based `getRemoteUrl` with 2s timeout.)*
- [x] **No surface for the read-only-ness of the panel.** *Resolved (iter 9 — Source Control sidebar Phase C ships stage/unstage/commit.)*
- [x] **Diff body has no virtualization, no syntax limit.** *Resolved (iter 12 — Monaco diff tabs.)*
- [x] **`HistoryView` commit list is capped at `COMMIT_LIMIT = 50` with no "load more".** *Resolved (iter 14 — paginated `list_commits`.)*
- [x] **Changes / History views re-issue their request on every `activeRepoId` change but don't cancel previous in-flight handlers.** *Resolved (Source Control sidebar replaces `GitPanel` with proper `useEffect` cleanup — the listener is removed on repo change.)*
- [x] **`select an item` placeholder is the same for both Changes and History.** *Resolved (iter 40 — History gains a dedicated placeholder; Changes diffs moved to Monaco tabs.)*
- [x] **`ResizableSplit` storage key `git.list` is shared across every git panel mount.** *Stale — `GitPanel.tsx` deleted in iter 7.*

### Preset launch

- [x] **File and folder prompt sources cannot be launched from the UI.** *Resolved (iter 1 — `PreviewPreset` daemon round-trip.)*
- [x] **Preset launch has no progress UI.** *Resolved (iter 26 — sticky info toast.)*
- [x] **Preset launch failure is `console.error` only.** *Resolved (iter 2 — warn-severity toast.)*
- [x] **Auto-activate-tab during preset launch hijacks user focus.** *Resolved (iter 26 — first tab only.)*
- [x] **No way to cancel a launch in progress.** *Resolved (iter 30 — caveat copy. Protocol-level cancel still deferred.)*
- [x] **Folder-source picker pre-fills with `repo.path + relative_path` regardless of file separator.** *Resolved (iter 46 — `joinPath` detects Windows base + normalises rel.)*
- [x] **Variable inputs offer no validation.** *Resolved (iter 30 — `missingRequiredVariables` + `aria-invalid`.)*
- [x] **Preview stage says "→ X tabs" but cannot tell the user when each tab will hold which prompts.** *Resolved (iter 47 — `PreviewPromptList` groups by destination tab.)*

### Exit flow

- [x] **"No active sessions" message hides orphan sessions.** *Resolved (iter 24 — separate `orphanSessionCount`.)*
- [x] **Backdrop click closes the dialog only when `!busy`. During `busy` the user is stuck.** *Resolved (iter 44 — stuck-daemon warning + force-quit button after 2 s.)*
- [x] **`onStopAndQuit` resolves the shutdown promise on `closed`-or-`disconnected` connection state, NOT on the daemon actually quitting.** *Resolved (iter 44 — 2 s auto-close removed; stuck path surfaces explicit force-quit affordance instead.)*
- [x] **Three buttons of similar visual weight, ambiguous priority order.** *Resolved (iter 24.)*
- [x] **No Escape key to cancel.** *Resolved (iter 20.)*
- [ ] **Closing a pop-out window invokes nothing — the main window has its own exit-confirm only.** OS closes pop-out windows immediately. Probably correct, but worth flagging.
  - File: `apps/tauri-app/src/App.tsx:354-370`.

### Global: keyboard, focus, accessibility, theming, error surfacing

- [x] **No global keyboard shortcuts at all.** *Resolved (iter 32 — Ctrl/Cmd+T/N/Tab/1..9.)*
- [x] **No Escape to dismiss modals.** *Resolved across iters 6 + 20.*
- [x] **No autofocus on modals (except WorkspaceCreator).** *Resolved (iter 38 — branch input on spawn dialogs.)*
- [x] **No focus return after modal close.** *Resolved (iter 33 — `useFocusReturn`.)*
- [x] **Sidebar tree leaves are `role="button"` but have no `tabIndex` or keyboard handler.** *Resolved (iter 6 — `tabIndex={0}` + Enter/Space.)*
- [x] **No `aria-label` on close × buttons.** *Resolved across iters 6 + 39.*
- [x] **No `role="dialog"` / `aria-modal` on modals.** *Resolved across iters 6 + 13 + 20.*
- [x] **Status dot uses colour as the only differentiator.** *Resolved (iter 16 — title + aria-label + role="img".)*
- [x] **Modal stacking order is render-order, all at `z-index: 100`.** *Resolved (iter 33 — z-layered destructive/vscode/exit.)*
- [x] **All daemon `error` messages are silenced.** *Resolved (iter 2 — error toast.)*
- [x] **`preset_launch_failed` lands in `console.error` AND a window event, but no listener consumes the event.** *Resolved (iter 2 — warn toast.)*
- [x] **Unused protocol surface: `SessionDiff` request/response.** *Resolved (iter 37 — deleted; `PROTOCOL_VERSION` 10 → 11.)*
- [x] **Daemon repo/workspace updates are NOT broadcast to all clients.** *Resolved (iter 3 — `state_events` broadcast.)*
- [x] **No `lang="…"` on `<html>`, no `<title>` for popped-out windows beyond raw-UUID labels.** *Resolved (iter 22 — `setTitle`.)*
- [x] **Connection badge is only visible in the EmptyState.** *Resolved (iter 21.)*
- [x] **`AppState.pendingTabActivate` is reset for the first new tab but tracks no actual tab id.** *Resolved (iter 35 — counter.)*
- [x] **`spawnTargetPaneRef` is cleared on dialog close, but the pending intent ref (`pendingSpawnIntentRef`) is NOT cleared if the spawn-message round-trip fails.** *Resolved (iter 22.)*
- [x] **Notification permission is requested on every app start.** *Resolved (iter 49 — Settings modal shows permission status + Re-request button, plus per-event toggles for awaiting_input / stopped / error.)*

### Dev / testing workflow

Strictly speaking outside the user-facing UX surface, but a user-visible risk if a developer runs the E2E harness against their daily-driver install — flagged here so it's tracked alongside the rest of the audit.

- [x] **E2E harness writes to the user's real config dir.** *Resolved (commit `094a2d3` — `RUSTLING_TULIP_CONFIG_DIR` plumbed; harness writes to `.tmp/e2e/config/`.)*

---

## Findings from hand-test 2026-05-11

Run-through of the live app (after iter 1–3 fixes). Bottom line: the core idea is useful, but the UX currently feels like an internal operator console. The biggest problems are orientation, trust, and recovery — not aesthetics.

- [x] **1. First-run onboarding is misleading.** `+ Session` enabled with no repos, opening a full spawn form with empty dropdown. *Resolved (iter 4 — toolbar gating + EmptyState `+ Add repo` button.)*
- [x] **2. Workspace creation disrupts mental model.** Existing single-repo sessions move into Detached with only a `?` marker. *Resolved (iter 4 — Detached banner explains; iter 5 — `[unbound]` pill rebinds.)*
- [x] **3. Session identity is too weak.** OSC-emitted terminal titles overwrote canonical labels. *Resolved (iter 4 — canonical label sticks; `terminal_title` surfaces as tooltip only.)*
- [x] **4. Workspace preview is effectively broken.** `.preview-table` collapsed to ~2 px when other modal content competed for space. *Resolved (iter 4 — `flex: 0 0 auto` + `min-height: 80px`.)*
- [x] **5. Risky actions lack recovery.** *Partially addressed in iter 3 (repo/workspace/tab confirms).* Remaining items below.
  - [ ] Detached-bucket session stop currently lives in the per-session Stop button. Audit asked for inline bucket-level "stop all" affordance.
  - [ ] Undo-last-action shelf — substantial feature, not started.
  - [ ] Longer-undo window for tab close — substantial feature, not started.
- [x] **6. Keyboard/focus accessibility is weak.** *Resolved (iter 6 — `useEscape` + `useAutoFocus` + tabIndex/Enter/Space on tree leaves + aria-labels.)*
- [x] **7. Pane controls are cryptic and crowded.** *Resolved (iter 6 — quieter baseline + aria-labels; iter 37 — hover/focus-within only.)*

**Recommended priority (historical):** identity + trust first, then keyboard/focus and modal layout. Visual polish matters less until users can confidently tell what will happen and recover when they click the wrong thing.

---

## Section 2 — Hand-test checklist

### Sidebar / repo & workspace management

- [ ] With 40+ repos in the sidebar tree, does the tree still scroll smoothly when expanding/collapsing nodes?
- [ ] When a workspace has 8 member repos, all with 2-3 sessions each, does the auto-expand-on-attention flicker the tree visibly?
- [ ] Remove a repo while a session is running against it. Does the session keep streaming PTY output in its tab even though the parent has moved to "Detached"?
- [ ] Right-click on the bottom edge of a tall sidebar — does the context menu clip below the window edge?
- [ ] Right-click the same repo twice in a row — does the second context menu show stale preset cache or re-fetch?
- [ ] In a workspace whose member repo got removed, does the workspace render with N-1 members and behave correctly on spawn?
- [ ] When the daemon disconnects, can you still see active sessions in the sidebar, and what happens if you click one?
- [ ] Click on a session that is currently in `awaiting_input` state — does the attention warning clear when the session is in the active pane, or only after the user types in it?
- [ ] Workspace name with leading/trailing whitespace — accepted? Stripped? Both forms of "foo " and "foo" coexist?
- [ ] Try to add the same repo path twice. Is it deduped on the daemon side, or do you see two identical containers in the sidebar?

### Spawn dialog

- [ ] Does Esc dismiss the spawn dialog without firing `onSpawned`?
- [ ] Open the dialog, type a branch name, click outside the dialog — does the input vanish silently?
- [ ] Open the dialog with zero repos registered — what does the user see, and how do they recover?
- [ ] Submit spawn, then immediately open the dialog again before the first `session_updated` arrives — does the second spawn target the correct pane?
- [ ] Submit spawn twice in <300 ms — do you get one session or two?
- [ ] Toggle "Create a worktree" several times. After Cancel, does the persisted `default_use_worktree` reflect your last toggle (yes, this is on purpose, but verify)?
- [ ] On a repo with no commits / no default branch, what does the Branch field default to? Does spawning succeed?
- [ ] Workspace form: change `useWorktree` AFTER clicking Preview. Does the preview update or stale?
- [ ] Headless run mode + empty prompt — Spawn button stays disabled. Type prompt, click Spawn, switch to interactive mode without closing — what carries over?
- [ ] Add an env var with key `=`, with key `1foo`, with value containing newlines. What does the daemon do with each?
- [ ] In workspace form with workspace dropdown empty (no workspaces), can you somehow submit?
- [ ] In Single form: switch repos rapidly; do branch-list reads race so an old repo's branches appear in the new repo's datalist?
- [ ] Open the dialog with `initial_target.kind = "workspace"` but zero workspaces — does it gracefully fall back to Single mode?

### Tabs + panes

- [ ] Drag a tab pill onto its own position — any visual "drop here" feedback?
- [ ] Drag a pane onto its own drag-grip — does anything happen, and should it?
- [ ] Open 30 tabs. Does the tab bar scroll horizontally smoothly? Does the active tab stay visible when you scroll?
- [ ] Tab name exactly equal to "Tab" × 5 — are all five visually distinguishable somehow (drag/drop position, ordering)?
- [ ] Start renaming a tab via double-click, then right-click another tab and pick "Close other tabs". Does the renaming tab get closed mid-edit?
- [ ] Multi-select 3 tabs with Ctrl+click, then drag one of them. Does the drag carry all 3 or just the dragged tab?
- [ ] Merge 3 selected tabs. Does the result tab auto-activate? If not, do you have to scroll the tab bar to find it?
- [ ] Use "Extract to new tab" on a pane from tab N. Where does the new tab appear in the tab bar, and is it activated?
- [ ] Split a pane horizontally then immediately split the new pane vertically (without clicking it). Does the split actually go where you expected?
- [ ] Drag a pane from tab A onto a tab B pill; tab B activates. Now release over a pane in tab B — drop succeeds. Now do the same but release over the tab B pill itself. Anything happens?
- [ ] In a tab with 8 panes, drag pane #1 onto the "replace" centre of pane #8. Where do panes #2..#7 end up?
- [ ] Deep grid: 4 levels of splits. Drag the divider at level 4 — does the persisted ratio actually take effect, or get clobbered by an ancestor re-render?
- [ ] Close the active tab repeatedly — does `activeTabId` always fall through to the next-or-prev tab, or sometimes to a stale id?
- [ ] Open a tab pop-out, then close the parent tab in the main window — does the pop-out close itself?
- [ ] Open a SESSION pop-out, then stop the session from the main window — does the pop-out keep showing exit-code state?
- [ ] Open a tab pop-out and click "Spawn one here" in an empty pane inside it. Does anything happen (expected: nothing — see finding)?
- [ ] Pop-out tab: rename the tab from the main window. Does the pop-out's `<h2>` re-render with the new name?
- [ ] Open the same session in two panes in the same tab. Do both terminals receive PTY output? Type in one — does it appear in the other?
- [ ] Stop a session that is currently rendered in 3 panes across 2 tabs. Are all three placeholder-fixed at once?

### Terminal

- [ ] Try to select text in a terminal pane with the mouse — does xterm's selection work despite body `user-select: none`?
- [ ] Try to copy a session label (`<h2>` in the header) — can you?
- [ ] Try to copy a diff line from the git panel — can you?
- [ ] Try to copy a line from `recent_actions` in a headless session — can you?
- [ ] Send Ctrl+Shift+C from the terminal — does xterm copy the selection, or does the browser intercept?
- [ ] Resize the window in 10 px steps — does PTY resize fire on every step or debounce?
- [ ] Open scrollback for a session whose ring overflowed. Does the "[earlier output discarded]" line render in yellow with `\x1b[33m` colour?
- [ ] Kill the daemon process while a terminal is attached. What does the terminal pane look like — frozen, blank, error banner?
- [ ] Orphan session: click Stop. What happens to the underlying `claude` process (check with `tasklist` / `ps`)?
- [ ] Send 1 MB of output in a single PTY burst (e.g. `cat large.txt`). Does the UI freeze, drop frames, or stay responsive?

### Git panel

- [ ] Open a diff for a 50 k-line generated file. Does the panel become responsive within a reasonable time?
- [ ] Click "Open in forge ↗" — does the URL include the session's branch, or just the repo root?
- [ ] Click "Open in forge ↗" 5 times rapidly — how many browser tabs open?
- [ ] Disconnect the daemon. Click "Open in forge ↗" — does the listener leak, error, or hang?
- [ ] In a multi-repo workspace session, switch repo via the repo-picker dropdown rapidly. Does the right pane (diff) ever show stale content for the previous repo?
- [ ] On the History tab: scroll to the bottom of 50 commits — is there any indication "this is all you get"?
- [ ] Stage a file from the terminal while the Changes view is open. Does the panel update? (Expected: no — re-fetch on user action only.)
- [ ] Click a deleted file in Changes. Does the diff show the deletion correctly?
- [ ] Right-click a file in Changes — any context menu (expected: none)?
- [ ] Resize the file list / diff splitter — does the size persist when you switch tabs and come back?

### Preset launch

- [ ] Try to launch a preset whose `prompt_sources` includes only `file`. Can you actually launch from the UI? Confirm or disconfirm the finding.
- [ ] Try with `folder` only — same question.
- [ ] Launch a preset that generates 30 prompts. Watch the active tab change as the daemon progresses — annoying or useful?
- [ ] Cancel a 30-prompt launch mid-way (close the daemon, or wait). What's the visible end state?
- [ ] Trigger `preset_launch_failed` (e.g. by deleting the preset's source repo mid-launch). Does the user see anything?
- [ ] Preset variables: leave a non-optional `text` variable empty. What happens on Launch?
- [ ] Open the preset dialog, click Next without picking a file. Is the Next button disabled correctly?
- [ ] Set `max_panes_per_tab` to 0, to -1, to "abc", to 99999. Validation behaviour?
- [ ] Launch a preset that creates 100 tabs (max_panes_per_tab=1, 100 prompts). Does the tab bar render gracefully?

### Exit flow

- [ ] Click "Stop sessions & quit" while no sessions exist. Does the dialog show "0 sessions are active"?
- [ ] With 2 orphan sessions and 0 live sessions, what does the dialog say? Does "Stop sessions & quit" actually clean up the orphan processes?
- [ ] Kill the daemon out-of-band, then close the main window. Does the dialog show the right state, and does "Stop sessions & quit" hang?
- [ ] After "Quit, leave running", the app closes. Restart it. Are the prior sessions reattached as orphans? Are tabs preserved?
- [ ] Open the exit dialog, then trigger a `vscode_workspace_suggestion` from the daemon (or open the WorkspaceCreator). What stacks on top of what?
- [ ] During `busy`, the dialog is uncancellable. How long until the 2 s timeout closes the window?
- [ ] Press Escape with the exit dialog open — does anything happen?
- [ ] Drag the divider during shutdown — any pointer-capture leaks?

### Global / accessibility / theming

- [ ] Tab through the UI with the keyboard from a fresh launch. What's the focus order? Are any controls unreachable?
- [ ] With a screen reader, navigate the sidebar tree. Are session statuses announced (currently only colour)?
- [ ] On Windows high-contrast mode, are all interactive elements still distinguishable?
- [ ] Resize the window to 700×400. Is anything unreachable or clipped (modal too tall, tab bar overflowing without scroll affordance)?
- [ ] Reload via DevTools (Ctrl+R). Does the active tab restore via `localStorage`? Do popouts re-connect?
- [ ] Open the app, immediately kill the daemon, then click `+ Session`. Does the modal open onto an empty repo list?
- [ ] With two clients connected (main + pop-out), add a repo from main. Does the pop-out see it after some time, or only on reconnect? (Expected: no — see finding.)
- [ ] Notifications: deny permission on first launch. Trigger `awaiting_input` on a session. Do you get any UI signal at all (beyond the sidebar dot)?
- [ ] Try opening the same session window pop-out twice — does `open_session_window` focus the existing window or open a duplicate?
- [ ] What's the window title of a pop-out — the session label or the raw UUID?
- [ ] Force a daemon `error` reply (e.g. malformed `move_pane`). Where does the message appear?
