# UX audit

A code-evidence audit of the rustling-tulip desktop client surface (Tauri shell + React/xterm.js frontend) plus a parallel checklist of behaviours that need a human at the keyboard to validate. The findings only cover UX-visible issues; architecture/refactor concerns and items called out as deferred in `CLAUDE.md` are out of scope.

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
- [Section 2 — Hand-test checklist](#section-2--hand-test-checklist)

---

## Section 1 — Code-evidence findings

### Sidebar / repo & workspace management

- **Destructive remove with no confirmation (inline × button).** Clicking the `×` next to a repo or workspace fires `remove_repo` / `remove_workspace` immediately. There is no "Are you sure?" or undo. Same path from the context menu's "Remove repo" / "Remove workspace" entries.
  - File: `apps/tauri-app/src/components/Sidebar.tsx:276-287`, `apps/tauri-app/src/components/Sidebar.tsx:199-204`.
  - Suggested direction: confirm step (inline two-state button like SessionPane's Stop, or a small modal) before sending the remove.
- **Removing a repo silently abandons its live sessions.** The daemon's `remove_repo` only retains-filters the registry; sessions running against that repo stay alive but their container disappears from the tree and they re-appear under "Detached" with no visible explanation.
  - Files: `crates/daemon/src/registry.rs:79-86`; rendering in `apps/tauri-app/src/components/Sidebar.tsx:516-561` (Detached bucket only labelled "Sessions whose containing repo or workspace is no longer registered" via `hoverTitle`).
  - Suggested direction: confirmation step that lists active sessions and offers "Remove anyway / stop them too / cancel".
- ~~**No name-uniqueness on workspaces.**~~ **Resolved (iter 25).** `upsert_workspace` now rejects empty/whitespace-only names AND case-insensitive trimmed-name collisions against OTHER workspaces (a rename-to-itself no-ops through fine). The daemon's `error` reply surfaces through the existing toast plumbing.
- ~~**`Sidebar` accepts a `connection` prop but never renders it.**~~ **Resolved (iter 21).** Sidebar header now renders a compact connection badge that hides itself when the connection is `open` and surfaces a `badge-warn` or `badge-err` chip (with the reason as a tooltip) for every other state. Visible from any view, not just the EmptyState.
- ~~**Force-expand silently overrides the user's collapse state.**~~ **Resolved (iter 23).** `attentionSessions` no longer participates in `forceExpand` (only the user-initiated `highlightedSessionIds` does). A new `container-attention-chip` (`!` in a small warn-coloured circle) appears on a container's header whenever at least one of its sessions is in `attentionSessions`, so the warning is still visible from a collapsed container without disturbing the user's layout choice.
- ~~**Attention flag is never cleared automatically.**~~ **Resolved (iter 16).** `session_updated` clears the id from `attentionSessions` on transition back to `working`/`idle`/`spawning`. `stopped`/`error` still count as attention-worthy and require user acknowledgement.
- ~~**`session_removed` leaks the id in `attentionSessions`.**~~ **Resolved (iter 16).** Removal handler now flushes the id from `attentionSessions` too.
- **Preset context-menu loading state never times out.** When `listPresets` returns empty after 2 s (the `api.ts` timeout), the cache stores `[]` and the menu shows "Launch preset… (none defined)". But while the request is in flight, the menu shows "Launch preset… (loading)" — if the daemon dies mid-request the user sees "loading" forever until they reopen.
  - Files: `apps/tauri-app/src/api.ts:177-200`, `apps/tauri-app/src/components/Sidebar.tsx:418-454`.
  - Suggested direction: explicit "Failed to load" state instead of silent fallback to `[]`.
- ~~**Sidebar context menu is anchored to raw cursor coords with no bounds check.**~~ **Resolved (iter 25).** New `clampMenuCoord` helper in `utils/a11y.ts` clamps `(x, y)` against `window.innerWidth/Height - menu size - 12px margin`. Applied to all three context menus (sidebar's `ContainerContextMenu`, `TabBar`'s `TabContextMenu`, `GridRenderer`'s pane context menu).
- **No empty state for a sidebar that has workspaces but only orphan/detached sessions.** The empty-state copy fires only when `containers.length === 0`. A detached bucket is `kind === "detached"` and counts as a container, so the user sees "Detached" with red tag and a session list but no narrative explaining what to do about it.
  - File: `apps/tauri-app/src/components/Sidebar.tsx:550-561`.
  - Suggested direction: hover title is good; add an inline action like "stop all" or "re-register repo" near the bucket header.

### Spawn dialog

- ~~**Repo dropdown can be empty.**~~ **Resolved (iter 27).** The sidebar toolbar already disabled `+ Session` when `repos.length === 0` (iter 4), and the dialog itself now renders a dedicated `EmptyRepoState` panel ("No repos registered" + Cancel / + Add repo buttons) instead of the form when invoked with no repos. The + Add repo button closes the dialog and triggers the directory picker via the same `onAddRepo` callback the sidebar's Add repo button uses.
- ~~**Backdrop click discards all typed input with no warning.**~~ **Resolved (iter 27).** Backdrop click is no longer a close trigger on the spawn dialog. The dialog still has three ways to dismiss (Escape, Cancel, ✕) but a stray click outside silently destroying form state is gone.
- **No loading state after Submit.** `submit` fires `spawn_session` then immediately `onSpawned()` + `onClose()`; the dialog vanishes. Until the daemon's `session_updated` arrives (which can take several seconds for worktree creation) the user is left staring at the empty state with no indication that a spawn is in flight.
  - File: `apps/tauri-app/src/components/SpawnDialog.tsx:511-531` (single), `715-735` (workspace).
  - Suggested direction: optimistic toast/inline progress, or keep the dialog visible with a spinner until the new `session_updated` is observed.
- ~~**Spawn failures surface only as `console.error`.**~~ **Resolved (iter 2 + 22).** Every `DaemonMessage::Error` now hits the bottom-right toast stack via `pushToast(..., severity: "error", message: "Daemon error", detail: msg.message)`. The same handler also disarms any armed spawn intent, so a daemon-rejected spawn doesn't leave stale pane/tab routing for the next attempt. Routing the toast specifically to the spawn dialog (vs the global stack) is still deferred — the cost (correlating an error reply to a request id) outweighs the benefit when the toast is already prominent.
- ~~**`spawning` and `working` share the same accent-blue dot.**~~ **Resolved (iter 23).** `.status-spawning` is now a hollow accent ring (transparent fill + 1.5px border) while `.status-working` stays solid accent. A glance differentiates "still bootstrapping" from "actively running".
- ~~**Workspace form has no "Cancel preview" / refresh affordance.**~~ **Resolved (iter 18).** Preview-reset `useEffect` now depends on `useWorktree` too, so toggling worktree mode clears the stale preview table and requires an explicit re-Preview.
- **Random worktree branch name regenerates every dialog open.** Closing and reopening the spawn dialog (even on the same target) gives a different `wt/<adj>-<noun>` suggestion, so users who Cancel-then-reopen lose their intended branch name.
  - File: `apps/tauri-app/src/components/SpawnDialog.tsx:388-431` (`useBranchField`, regenerates on mount via `useState(defaultBranch)` + `defaultBranch` change effect; mount alone seeds `randomWorktreeBranchName()`).
  - Suggested direction: persist last-suggested name per-target in component state at the App level, or accept this as a freshness feature with a "regenerate" button.
- ~~**`Spawn → Spawn` double-click submits twice.**~~ **Resolved (iter 18).** Both `SingleForm.submit` and `WorkspaceForm.submit` guard via a `submittedRef = useRef(false)` flip that survives synchronous double-clicks before the dialog unmounts.
- ~~**Permission-mode dropdown silently retains a value while disabled.**~~ **Resolved (iter 24).** Inline hint is now explicit: "Ignored while {skip-permissions|yolo} is on — claude will run without --permission-mode. The dropdown value is preserved for when you toggle it off." So the user knows both what will happen (no flag sent) and that the value is intentionally remembered for later. Same pattern for the codex sandbox dropdown.
- ~~**Env var rows have no key-uniqueness or `KEY=value` syntax check.**~~ **Resolved (iter 27).** Each env row now validates the key against `/^[A-Za-z_][A-Za-z0-9_]*$/` and flags duplicates against other rows in the same dialog. Invalid keys get a `.input-invalid` outline + inline error; duplicates get a "Duplicate key — only the last row wins" hint. Submit is gated on `envRowsAreValid` in both SingleForm and WorkspaceForm.
- **Worktree-default toggle persists immediately on click.** Toggling "Create a worktree" fires `set_repo_worktree_default` / `set_workspace_worktree_default` to the daemon for the current repo/workspace, which is fine — but cancelling the spawn dialog leaves the changed default behind. The toggle in this dialog has dual purpose ("for this spawn" + "remember as default") with no UI to separate them.
  - File: `apps/tauri-app/src/components/SpawnDialog.tsx:495-504`, `689-698`.
  - Suggested direction: separate the two intents, or only persist on a successful spawn.

### Tabs + panes (grid, drag/drop, pop-out)

- ~~**All freshly-created tabs are named "Tab".**~~ **Resolved (iter 17).** `make_tab` picks the next free default name (`"Tab"`, `"Tab 2"`, `"Tab 3"`, ...) based on the current tab list, with gap-filling so closed-and-reopened slots reuse their old number. Applies to plain `create_tab`, `merge_tabs`, and `extract_to_new_tab`.
- ~~**Merge-tabs is hard-coded to `tile_horizontal` layout.**~~ **Resolved (iter 28).** Context menu now splits the merge entry into two: "Side by side (horizontal)" and "Stacked (vertical)" under a `Merge N selected into new tab` label. Layout flows through `onMergeSelected(layout)` to the daemon `merge_tabs` message.
- ~~**Merge doesn't switch to the new tab.**~~ **Resolved (iter 17).** `TabBar.onMergeSelected` calls a new `onArmNextNewTab` App-level callback that flips `pendingTabActivate` before sending `merge_tabs`, so the resulting `tab_updated` auto-activates the merged tab.
- **Tab pill close × has no confirmation.** Closing the last pane-bearing tab destroys all its grid layout. Sessions survive but their tab/grid placement is gone — including across all pop-outs.
  - File: `apps/tauri-app/src/components/TabBar.tsx:50-56`.
  - Suggested direction: confirm when the tab has 2+ panes or contains active sessions, or offer an undo toast.
- **Context-menu "Close other tabs" sends one `close_tab` per other tab in a loop.** That's N WebSocket messages with no batch primitive, and no confirmation — same issue as above amplified.
  - File: `apps/tauri-app/src/components/TabBar.tsx:110-120`.
  - Suggested direction: confirm; consider adding a batched `close_tabs` message, although that's protocol scope.
- **Reorder is daemon-round-tripped, no optimistic update.** Drop fires `reorder_tabs`, then the tab list re-renders only after `tabs_reordered` arrives. On slow links the dropped pill jumps back to its old position briefly.
  - File: `apps/tauri-app/src/components/TabBar.tsx:184-203`.
  - Suggested direction: optimistically update local state; reconcile when `tabs_reordered` arrives.
- **Drag a tab onto itself: nothing happens but no feedback either.** `if (!src || src === tabId) return;` silently noops. With no drop-target highlight, the user can't tell whether the gesture failed because of the self-target or a sticky drag state.
  - File: `apps/tauri-app/src/components/TabBar.tsx:190`.
  - Suggested direction: don't show `drop-before/after` indicator on the source pill (currently filtered via `dragState.draggingId !== tabId`, but still confusing).
- **Pane drag onto a tab pill activates the tab but offers no drop target.** Dropping on a pill is ignored (only pane-to-pane drops via `MovePane` work). The "activate-on-enter" UX means the user can drag, hover over a different tab to switch to it, but if they release on a pill nothing happens.
  - Files: `apps/tauri-app/src/components/TabBar.tsx:158-167`, `171-178`.
  - Suggested direction: accept pane drop on a pill as "create a new pane in the target tab and place the source there"; or render a "drop here" hint informing the user to release over a pane.
- **`split_pane` is hard-wired to `place: "second"`.** The protocol supports `first` (new pane on the left/top) but no UI ever sets that. Users can only split right and split down.
  - File: `apps/tauri-app/src/components/GridRenderer.tsx:190-202`.
  - Suggested direction: either add buttons for "split left / split up" or pick one direction and remove the protocol option.
- ~~**After a split, focus stays on the source pane.**~~ **Resolved (iter 29).** New `onArmFocusNewPane(tabId, knownPaneIds)` callback on `GridRenderer` is called by `PaneChrome.sendSplit` right before sending `split_pane`. The App stores the snapshot in `pendingPaneFocusRef`; the next `tab_updated` for the matching tab diffs against the snapshot and sets `focusedPaneId` to whichever pane id wasn't there before. Mirrored locally in `TabWindow` (pop-out) since the pop-out has its own focus state. Disarms after the first matching update so the arm doesn't outlive its intent.
- ~~**`Move to new tab` in pane context menu is silent.**~~ **Resolved (iter 17).** `GridRenderer.PaneChrome.onExtract` calls `onArmNextNewTab` before sending `extract_to_new_tab`, so the new tab gets activated.
- **Pane controls overlay covers the session header on small panes.** Absolute-positioned at `top: 4px; right: 4px;` — on narrow panes the close `×` overlaps `session-actions` (Pop out / Stop) buttons.
  - File: `apps/tauri-app/src/styles.css:605-637`.
  - Suggested direction: move controls into a dedicated chrome bar above the session header on small widths, or hide overlay buttons when session controls are within N px.
- ~~**Pop-out session window never auto-closes when its session is stopped or removed.**~~ **Resolved (iter 19).** Parallel `useEffect` watches `popoutSessionId` and closes the window via `getCurrentWindow().close()` when the session disappears from `state.sessions`.
- ~~**Pop-out session window has TWO Stop buttons with different behaviours.**~~ **Resolved (iter 19).** Inner `SessionPane`'s Stop button (and exit-code label) is hidden when `isPopoutWindow`; the chrome `SessionWindow` toolbar's single-click Stop remains. One destructive control per window.
- ~~**`isPopoutWindow` in `SessionPane` only checks `?session`, not `?tab`.**~~ **Resolved (iter 19).** Now treats both `?session=` and `?tab=` as pop-out modes; session panes inside a `TabWindow` no longer show a redundant "Pop out" button.
- ~~**Pop-out tab window's "Spawn one here" is a no-op.**~~ **Resolved (iter 34).** `EmptyPane` gains an `inPopout` prop threaded down from `TabWindow` via `GridRenderer.inPopout`. When `inPopout` is true the "Spawn one here" button is replaced by an explanatory hint: "Use the main window to spawn a session, then drag it here." The pop-out's empty-pane state is now coherent — no button that silently no-ops, and the no-sidebar fallback is replaced by the drag-in workflow that actually works there.
- ~~**TabWindow has no chrome controls.**~~ **Resolved (iter 21).** Header now flex-aligns the tab name (with ellipsis when long, full name in tooltip) and a right-justified "Close window" button. The button only closes the pop-out window; the tab and its sessions stay in the main window's tab list. Rename + "Bring back to main" remain deferred.
- **TabWindow's focusedPaneId state is independent from the main window's.** Each window's pane focus is tracked locally. If the user spawns into a focused empty pane from the main window, only the main window sees the focus change. Probably intentional — flagged in hand-test.
  - File: `apps/tauri-app/src/components/TabWindow.tsx:21`.
- **`SessionWindow` ignores the daemon-emitted `Repos` / `Workspaces` payloads.** Each pop-out window opens a fresh WebSocket and receives the initial state, but the SessionWindow doesn't subscribe to repo changes — irrelevant for it directly, but if the session disappears nothing flushes the stale snapshot.

### Terminal

- **Body-level `user-select: none` prevents selecting diff text and headless logs.** `body { user-select: none; -webkit-user-select: none; }` is global. `.terminal-container` is xterm-managed (xterm injects its own selection handling) but `.diff-pane`, `.headless-log`, `.preset-preview-list`, and `recent_actions` are all unselectable. Users cannot copy a diff line, an event log entry, or a session label.
  - File: `apps/tauri-app/src/styles.css:25-31`.
  - Suggested direction: restrict the rule to chrome (sidebar, tab bar, modals) and re-enable selection on content surfaces (`.diff-pane`, `.headless-log`, `.terminal-host`, `.preset-preview-list`, plus the SessionPane label).
- ~~**Orphan banner instructs to "Stop the session and spawn a new one" — but Stop on an orphan does nothing to the underlying claude process.**~~ **Resolved (iter 34).** Daemon `stop_session` now branches on `had_live_handle = pty.is_some() || headless.is_some()`: when neither survived (orphan case), it loads the sidecar `meta.json` via the new `orphan::load_meta` helper and calls `orphan::kill_pid` to send SIGKILL/TerminateProcess to the recorded pid. The kill is name-checked first (`is_session_alive`) so a recycled pid won't get hit. The frontend orphan banner text is updated to match: "Use Stop to kill the recorded PID and clean up, then spawn a new session." (Tested with `cargo test -p daemon` — 68/68 green; the existing `partition_live` test exercises the name-match path on which `kill_pid` relies.)
- ~~**No reconnection on socket close.**~~ **Resolved (iter 31).** The App-level connection effect now wraps its connect-and-subscribe block in a `connect()` callback and reschedules it via `setTimeout` whenever the WS hits `closed` (or `ensureDaemonStarted` itself throws). Exponential backoff: `500ms * 2^attempt`, capped at 10s; attempt counter resets to 0 on every successful `open`. `auth_failed` is terminal — no retry, as that's a config issue. Cleanup on unmount cancels the pending timer. Sidebar connection badge (iter 21) already surfaces the non-`open` state from any view, so the user sees "disconnected" / "connecting" / "auth failed" without needing the EmptyState.
- **No copy-paste hot-keys in xterm config.** Standard xterm allows Ctrl+Shift+C / Ctrl+Shift+V but the config doesn't bind explicit copy/paste handlers. Combined with `user-select: none` on body (xterm's own selection still works inside the terminal), users may not realise selection is active.
  - File: `apps/tauri-app/src/components/Terminal.tsx:25-40`.
  - Suggested direction: bind explicit keymap or surface a "right-click to copy selection" hint.
- **Headless `recent_actions` list grows unbounded in the UI.** `HeadlessView` renders all entries. Daemon may cap it, but the UI does no virtualization. Long-running headless sessions could render thousands of `<li>` entries.
  - File: `apps/tauri-app/src/components/SessionPane.tsx:171-185`.
  - Suggested direction: window the list (`react-window` or a simple max-N + "load more"), or fade older entries.
- **PTY input is sent on every keystroke even when the session is stopped.** `Terminal` is unmounted when `session.status === "stopped"` (line 142 SessionPane), but the `onData` handler in xterm fires after dispose — fine. However, between the daemon emitting Stopped and the UI receiving `session_updated`, keystrokes are forwarded to a dying PTY, the daemon may log warnings.
  - File: `apps/tauri-app/src/components/Terminal.tsx:76-84`.
  - Suggested direction: guard `send_input` on the latest known session status (low priority).

### Git panel

- **"Open in forge ↗" goes to the repo home, not the branch.** `openInForge` requests `get_remote_url`, gets the repo's parsed web URL, and opens it. The branch is never appended to the URL.
  - Files: `apps/tauri-app/src/components/GitPanel.tsx:317-331`, `crates/daemon/src/git_inspect.rs:193-…`.
  - Suggested direction: append `/tree/<branch>` for github, `/-/tree/<branch>` for gitlab, etc. The `branch` argument is already plumbed (`_branch: string`).
- **`openInForge` listener leak.** Each click registers a `rt:remote_url` listener that removes itself only on the first matching event. If the daemon never replies (e.g. network repo, no remote configured) the listener stays attached forever. Three rapid clicks register three handlers — each will call `openInShell` when the response arrives.
  - File: `apps/tauri-app/src/components/GitPanel.tsx:317-331`.
  - Suggested direction: 2 s timeout + cleanup like `loadScrollback` / `listPresets`; debounce on the button.
- **No surface for the read-only-ness of the panel.** `CLAUDE.md` flags Stage/unstage as deferred, but there's no visual note in the Changes view explaining that the file rows are click-to-view-only. New users may try to right-click for stage actions and find nothing.
  - File: `apps/tauri-app/src/components/GitPanel.tsx:144-162`.
  - Suggested direction: footer text "Read-only · stage from your terminal" or remove the visual cursor:pointer on the rows.
- **Diff body has no virtualization, no syntax limit.** A 50 k-line diff renders as 50 k `<span>` elements, all at once. For a generated `pnpm-lock.yaml` change this freezes the renderer.
  - File: `apps/tauri-app/src/components/GitPanel.tsx:295-305`.
  - Suggested direction: cap at N lines with "show more" or virtualize.
- **`HistoryView` commit list is capped at `COMMIT_LIMIT = 50` with no "load more".** Users on long-history branches see only the most recent 50 commits.
  - File: `apps/tauri-app/src/components/GitPanel.tsx:20`, `196-214`.
  - Suggested direction: paginated `list_commits` plus a "load more" button at the bottom of the list.
- **Changes / History views re-issue their request on every `activeRepoId` change but don't cancel previous in-flight handlers.** Two rapid switches register two `rt:repo_status` listeners; the second remains until cleanup. Subsequent responses for the stale repo will be dropped via the id check but listener registration accumulates briefly.
  - File: `apps/tauri-app/src/components/GitPanel.tsx:92-129`, `196-246`.
  - Suggested direction: probably OK in practice; tighten if you see lag.
- **`select an item` placeholder is the same for both Changes and History.** When the user switches from Changes (with a selected file diff visible) to History, the right pane shows "select an item" with no hint that they're now in History.
  - File: `apps/tauri-app/src/components/GitPanel.tsx:289-294`.
  - Suggested direction: contextual placeholder ("Select a file to view the working-tree diff" / "Select a commit to view its changes").
- **`ResizableSplit` storage key `git.list` is shared across every git panel mount.** Resizing the file list in tab A persists for every git panel in every tab/pop-out. May be desirable, but not configurable.
  - File: `apps/tauri-app/src/components/GitPanel.tsx:132-138`, `249-254`.

### Preset launch

- **File and folder prompt sources cannot be launched from the UI.** `computePreview` only parses `inline`; for `file`/`folder` it returns `[]` (comment: "would require a daemon round-trip; for v1, show a placeholder count via the source picker stage"). `canSubmit` requires `previewPrompts.length > 0`. Result: any preset declaring `file` or `folder` in `prompt_sources` has a broken launch path — the preview stage shows "0 prompts" with disabled "Launch 0 sessions" button.
  - Files: `apps/tauri-app/src/components/PresetLaunchDialog.tsx:83-87`, `544-554`, `467-477`.
  - Suggested direction: either send a preview-only request to the daemon (it already parses files in `presets.rs`) and populate `previewPrompts`, or drop `canSubmit > 0` when source is file/folder (treat as "trust the daemon").
- ~~**Preset launch has no progress UI.**~~ **Resolved (iter 26).** A sticky info-severity toast `Launching preset 'X' — N / total sessions` updates in place via the new `pushToast` dedup-key path. When `launched >= total`, the toast flips to `Preset 'X' launched` and unsticks so it auto-dismisses naturally.
- ~~**Preset launch failure is `console.error` only.**~~ **Resolved (iter 2).** The `preset_launch_failed` message handler now also pushes a warning-severity toast: `Preset 'X' launch failed · <error> · N session(s), M tab(s) partial`. The "Open partial tab / Stop partial sessions" affordances suggested in the audit are explicit follow-up scope — the toast at least makes the failure visible.
- ~~**Auto-activate-tab during preset launch hijacks user focus.**~~ **Resolved (iter 26).** Auto-activate now only fires on the FIRST tab created during a launch (`msg.tab_ids.length === 1`) — kicks off context for the user, then the launch runs silently with the progress toast keeping them informed.
- ~~**No way to cancel a launch in progress.**~~ **Resolved (iter 30).** Preview stage now renders an italicised caveat right below the prompt count: "Launching is one-shot — there's no in-app cancel once the daemon starts spawning. To stop a partial launch you'd have to stop each spawned session by hand, or kill the daemon process." A protocol-level cancel is still deferred — the audit's "at minimum surface that it's irreversible" ask is met.
- **Folder-source picker pre-fills with `repo.path + relative_path` regardless of file separator.** `joinPath` tries to match base separator style but falls back to `/` on mixed input. Windows users with mixed `\` / `/` paths could see ugly results, though the daemon will normalize.
  - File: `apps/tauri-app/src/components/PresetLaunchDialog.tsx:503-508`.
- ~~**Variable inputs offer no validation.**~~ **Resolved (iter 30).** Variables stage now derives a `missingRequiredVariables` set (prompt_at_launch + !optional + empty trimmed value). The Next button is disabled when any required variable is empty, with a tooltip listing the labels that still need filling. Empty required fields get a `.input-invalid` outline (red border) + inline "Required — fill in before continuing." hint. `aria-invalid` is wired on the input so screen readers announce it.
- **Preview stage says "→ X tabs" but cannot tell the user when each tab will hold which prompts.** No grouping visualisation, just a count.
  - File: `apps/tauri-app/src/components/PresetLaunchDialog.tsx:437-449`.

### Exit flow

- ~~**"No active sessions" message hides orphan sessions.**~~ **Resolved (iter 24).** Exit dialog now receives a separate `orphanSessionCount` and appends "(N orphans — daemon can't stop these; they'll stay running)" to the session-count line so the user knows `Stop sessions & quit` won't kill them.
- **Backdrop click closes the dialog only when `!busy`.** Good; but during `busy` the user is stuck — no way to abort the shutdown attempt if the daemon hangs.
  - File: `apps/tauri-app/src/components/ExitConfirmDialog.tsx:21`.
  - Suggested direction: after 2 s of `busy`, surface a "Daemon not responding — force quit" button.
- **`onStopAndQuit` resolves the shutdown promise on `closed`-or-`disconnected` connection state, NOT on the daemon actually quitting.** The 2 s timeout falls back to force-closing the window. If shutdown was slow but successful, fine. If shutdown is silently stuck, the daemon is left running — and on next launch, the user will see daemon-supervisor errors.
  - File: `apps/tauri-app/src/App.tsx:392-422`.
  - Suggested direction: probe `daemon.json` pid liveness during the 2 s window; if the daemon is still up after timeout, warn explicitly.
- ~~**Three buttons of similar visual weight, ambiguous priority order.**~~ **Resolved (iter 24).** Button order is now Cancel · Stop sessions & quit · **Quit, leave running** (primary, accent-coloured, autofocused). The destructive option keeps its `danger` class but is no longer the visual focal point. Matches the design goal of "long-lived daemon by default".
- ~~**No Escape key to cancel.**~~ **Resolved (iter 20).** `useEscape(onCancel, !busy)` — Escape dismisses while idle, no-ops during in-flight shutdown.
- **Closing a pop-out window invokes nothing — the main window has its own exit-confirm only.** The pop-out only listens for `onCloseRequested` in `App.tsx:354-370`, which is gated on `if (popoutSessionId || popoutTabId) return;`. So pop-out close is unhandled; the OS closes the window immediately. That's actually probably correct, but worth flagging.

### Global: keyboard, focus, accessibility, theming, error surfacing

- ~~**No global keyboard shortcuts at all.**~~ **Resolved (iter 32).** New `useKeyboardShortcuts` hook in `utils/a11y.ts` binds document-level keydown handlers; skips when focus is in an editable surface (input/textarea/contenteditable/xterm) and when any modal is open. App.tsx wires: **Ctrl/Cmd+T** (new tab), **Ctrl/Cmd+N** (spawn session, gated on `repos.length > 0`), **Ctrl/Cmd+Tab** + **Ctrl/Cmd+Shift+Tab** (cycle tabs forward/back, only when 2+ tabs exist), **Ctrl/Cmd+1..9** (activate tab at index N-1; only registered for slots that have a tab). Pop-out windows don't get any shortcuts to keep the surface predictable. F2-to-rename and Ctrl+W (collides with browser/Tauri window-close) deliberately not bound.
- ~~**No Escape to dismiss modals.**~~ **Resolved across iters 6 + 20.** `useEscape` hook now wired into SpawnDialog, PresetLaunchDialog, RepoRemoveDialog, DiscardConfirmDialog (iter 6 + 13), and WorkspaceCreator, VscodeSuggestionToast, ExitConfirmDialog (iter 20). ExitConfirmDialog gates the binding on `!busy` so a mid-shutdown Escape doesn't fight the in-flight quit.
- **No autofocus on modals (except WorkspaceCreator).** *Partially resolved.* RepoRemoveDialog + DiscardConfirmDialog (iter 6 + 13) and ExitConfirmDialog (iter 20) autofocus Cancel (least-destructive default). SpawnDialog and PresetLaunchDialog still leak focus to the previously focused element when opened — both are large forms where "first meaningful input" is non-trivial (which field?) so left for follow-up.
- ~~**No focus return after modal close.**~~ **Resolved (iter 33).** New `useFocusReturn()` hook in `utils/a11y.ts` captures `document.activeElement` on modal mount and restores focus on unmount (defers to next tick so React's DOM teardown doesn't clobber the restored focus). Skips when the previously focused element has been removed from the DOM by the time the modal unmounts. Wired into SpawnDialog, PresetLaunchDialog, WorkspaceCreator, RepoRemoveDialog, ExitConfirmDialog, DiscardConfirmDialog, and VscodeSuggestionToast.
- **Sidebar tree leaves are `role="button"` but have no `tabIndex` or keyboard handler.** Screen reader announces "button" but Enter / Space do nothing.
  - File: `apps/tauri-app/src/components/Sidebar.tsx:262-263`, `328-329`.
  - Suggested direction: add `tabIndex={0}` and an `onKeyDown` that maps Enter/Space to `onSelect`.
- **No `aria-label` on close × buttons.** The literal `×` is non-descriptive for screen readers.
  - Files: `Sidebar.tsx:285`, `SpawnDialog.tsx:115`, `WorkspaceCreator.tsx:45`, `VscodeSuggestionToast.tsx:30`, `PresetLaunchDialog.tsx:129`, `TabBar.tsx:262-270`.
  - Suggested direction: `aria-label="Close dialog"` / `"Remove repo"` / `"Close tab"` on each.
- ~~**No `role="dialog"` / `aria-modal` on modals.**~~ **Resolved across iters 6 + 13 + 20.** Every component using `.modal-backdrop` now adds `role="dialog"` + `aria-modal="true"` + a descriptive `aria-label` on the inner `.modal`. Pure-toast surfaces (`ErrorToast`) are intentionally NOT marked as dialogs — they don't take focus or block input.
- ~~**Status dot uses colour as the only differentiator.**~~ **Resolved (iter 16).** Status dots in `Sidebar` and `SessionPane` gained `title="status: <state>"` + `aria-label="status <state>"` + `role="img"` so screen-reader users and hover-tooltip users get the underlying state.
- ~~**Modal stacking order is render-order, all at `z-index: 100`.**~~ **Resolved (iter 33).** Three new modifier classes layered on top of `.modal-backdrop`: `modal-backdrop-destructive` (z 120, used by RepoRemoveDialog + DiscardConfirmDialog), `modal-backdrop-vscode` (z 130, VscodeSuggestionToast), `modal-backdrop-exit` (z 140, ExitConfirmDialog). Ordinary forms (SpawnDialog, PresetLaunchDialog, WorkspaceCreator) keep the base z 100. The ordering reflects intent: exit (system-level quit) wins over everything; daemon-pushed events outrank confirms; destructive confirms outrank ordinary forms.
- **All daemon `error` messages are silenced.** The only handler is `console.error("daemon error:", msg.message);` — Tauri apps don't have a visible dev console for end users.
  - File: `apps/tauri-app/src/App.tsx:848-850`.
  - Suggested direction: dedicated error-toast component; route every Error there.
- **`preset_launch_failed` lands in `console.error` AND a window event, but no listener consumes the event.** Same fate as Error.
  - File: `apps/tauri-app/src/App.tsx:822-833`.
- **Unused protocol surface: `SessionDiff` request/response is wired in `App.tsx`'s switch but no UI sends `session_diff` and no listener subscribes to `rt:session_diff`.** Dead code on both sides of the boundary.
  - File: `apps/tauri-app/src/App.tsx:813` (dispatch only); no `rt:session_diff` consumers anywhere.
  - Suggested direction: surface in the SessionPane header (e.g. "3 files dirty" chip), OR delete the protocol message until needed.
- **Daemon repo/workspace updates are NOT broadcast to all clients.** `add_repo`, `remove_repo`, `upsert_workspace`, `remove_workspace`, `set_*_worktree_default` all send `Repos` / `Workspaces` only via `out_tx` (the per-client channel). A pop-out window connected at the same time won't see the change until it reconnects.
  - File: `crates/daemon/src/server.rs:466-508`, `670-682`.
  - Suggested direction: fan these out via the broadcast channel like tabs/sessions.
- ~~**No `lang="…"` on `<html>`, no `<title>` for popped-out windows beyond Tauri builder's `Session — <id>` / `Tab — <id>` raw-UUID labels.**~~ **Resolved (iter 22).** `<html lang="en">` was already set; pop-out `SessionWindow` / `TabWindow` now call `getCurrentWebviewWindow().setTitle(...)` with the session label / tab name on mount and on every label/name change, so renames propagate to the OS taskbar and alt-tab list.
- ~~**Connection badge is only visible in the EmptyState.**~~ **Resolved (iter 21).** Sidebar header now always renders a connection badge for non-`open` states (hidden in the happy path). Live sessions that lose connection now show the warn/error chip in the persistent sidebar chrome.
- **`AppState.pendingTabActivate` is reset for the first new tab but tracks no actual tab id.** If two spawns race (e.g. user mashes Spawn), the flag is consumed by whichever `tab_updated` arrives first — the second new tab is created but not activated.
  - File: `apps/tauri-app/src/App.tsx:737-756`.
  - Suggested direction: store the spawn intent's session-id alongside the flag; activate only when the matching tab arrives.
- ~~**`spawnTargetPaneRef` is cleared on dialog close, but the pending intent ref (`pendingSpawnIntentRef`) is NOT cleared if the spawn-message round-trip fails.**~~ **Resolved (iter 22).** Daemon `error` handler now disarms the pending spawn intent before pushing the toast — a daemon-rejected spawn no longer leaves stale routing armed for the next spawn.
- **Notification permission is requested on every app start.** *Partially resolved (iter 21).* The startup effect now `logToFile("warn", ...)` when `requestPermission()` returns anything other than `"granted"`, so `app.log` has an explanation when attention notifications stay silent. A real settings UI to inspect/re-trigger is still out of scope (no settings surface exists yet).

### Dev / testing workflow

Strictly speaking outside the user-facing UX surface, but a user-visible risk if a developer runs the E2E harness against their daily-driver install — flagged here so it's tracked alongside the rest of the audit.

- ~~**E2E harness writes to the user's real config dir.**~~ **Resolved.** `RUSTLING_TULIP_CONFIG_DIR` is honored by `Dirs::ensure` (`crates/daemon/src/paths.rs`) and `config_dir` (`apps/tauri-app/src-tauri/src/lib.rs`); `tools/e2e/wdio.conf.ts` sets it to `.tmp/e2e/config/` per run and wipes the dir after killing any prior test daemon. Verified end-to-end: the wdio smoke spec writes to the tmpdir, real `%APPDATA%\leftos\rustling-tulip\config\` is untouched.

---

## Findings from hand-test 2026-05-11

Run-through of the live app (after iter 1–3 fixes). Bottom line: the core idea is useful, but the UX currently feels like an internal operator console. The biggest problems are orientation, trust, and recovery — not aesthetics. Numbered roughly in priority order.

1. **First-run onboarding is misleading.** `+ Session` is enabled with no repos. Opens a full spawn form with an empty repo dropdown, random branch name, disabled Spawn button, and no explanation. New users hit a dead end immediately.
   - Suggested direction: disable `+ Session` when `repos.length === 0`. In the spawn dialog itself, show an empty-state CTA with a button that closes the dialog and opens the directory picker.
   - Files: `apps/tauri-app/src/components/Sidebar.tsx:127-132` (toolbar `+ Session` always enabled), `apps/tauri-app/src/components/SpawnDialog.tsx:535-546` (empty repo dropdown).

2. **Workspace creation disrupts mental model.** Creating a workspace from two repos moves their existing single-repo sessions into `Detached` with only a tiny `?` marker. Feels like the app lost or orphaned the sessions, even though they're still running.
   - Suggested direction: the Detached bucket needs a banner explaining what happened ("These sessions were running before their repo got grouped into a workspace. They're still alive and attached.") plus a per-session "reattach to workspace" action.
   - Files: `apps/tauri-app/src/components/Sidebar.tsx:516-561` (Detached rendering).

3. **Session identity is too weak.** Sidebar and pane header show e.g. `C:\WINDOWS\system32\cmd.exe`. With tabs all named "Tab" the user can't tell what is what.
   - Suggested direction: label priority should be `user-provided label > "<repo>:<branch> · <agent>" > terminal title`. Terminal title should be hover-only / tooltip metadata, not the primary visible label.
   - Files: `apps/tauri-app/src/components/SessionPane.tsx` (header `<h2>`), `apps/tauri-app/src/components/Sidebar.tsx` (session row label).

4. **Workspace preview is effectively broken.** Clicking Preview creates the table in the DOM, but its container collapses to ~2 px high in normal layout — user can't actually read the multi-repo worktree plan before spawning.
   - Suggested direction: audit the flex parent chain. The `.preview-table` container likely needs `min-height` or its parent needs `flex: 1 1 auto` instead of `flex: 0 1 auto`.
   - Files: `apps/tauri-app/src/components/SpawnDialog.tsx:938-968` (`.preview-table`), `apps/tauri-app/src/styles.css` (selector definitions).

5. **Risky actions lack recovery.** *Partially addressed in iter 3* — repo remove / workspace remove / tab close / close-other-tabs now have confirms. Remaining: detached-bucket session stop, undo-last-action shelf, and a longer-undo window for tab close.

6. **Keyboard/focus accessibility is weak.** Spawn dialog doesn't autofocus, focus stays behind the modal. Several `role="button"` / `role="tab"` divs have no `tabIndex` or keyboard handlers. Icon/glyph buttons have `title` but no `aria-label`.
   - Suggested direction: whole-app sweep. Add a `useEscape(onClose)` hook + autofocus to every modal; add `tabIndex={0}` + Enter/Space handlers to tree leaves; add `aria-label` to all `×` and icon-only buttons.

7. **Pane controls are cryptic and crowded.** The `⠿`, `▶|`, `▼=`, `×` overlay near the session header is compact but not self-evident, and crowds the Pop out / Stop buttons on narrow panes.
   - Suggested direction: either re-design (text+icon labels), only show on hover, or move into a dedicated chrome strip above the session header on small widths.
   - Files: `apps/tauri-app/src/components/GridRenderer.tsx` (pane controls), `apps/tauri-app/src/styles.css:605-637`.

What works as-is: terminal attach flow, Stop session two-step confirm (which is the pattern we replicated for repo / workspace / tab in iter 3), workspace creation itself (simple + autofocused), Git panel Changes/History structure.

**Recommended priority:** identity + trust first (stable labels, fixed workspace preview, Detached explanation, confirms ✓), then keyboard/focus and modal layout. Visual polish matters less until users can confidently tell what will happen and recover when they click the wrong thing.

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
