# E2E harness resync

The WebdriverIO suite has drifted from the current app in several independent
ways. Discovered 2026-07-23 while validating the paste-fidelity work — the
`terminal-paste` spec (and ~20 others that spawn a session) can't run to
completion until these are fixed. None are related to the paste change; they
are pre-existing rot in `tools/e2e/`.

## Blockers found (in the order they surface)

- [x] **Stale `spawn_session` payload.** Every spec's `spawnFakeClaude`/spawn
  helper sent the old flat `agent` / `permission_mode` / `codex_sandbox`
  fields. `protocol::SpawnRequest` now carries a single `agent_options` tagged
  enum (`#[serde(tag = "kind")]`: `claude` / `codex` / `cursor`). The daemon
  couldn't deserialize the old shape and logged `ignoring unknown client
  message type … type_tag=spawn_session`, so no session ever spawned. Fixed by
  a shared `buildSpawnMessage` / `spawnSession` helper in
  `tools/e2e/src/session-helpers.ts`; all specs send `agent_options`.

- [x] **LayoutChooser modal blocks session-pane interaction.** A fresh e2e
  client hits `layout_init_required` and renders `LayoutChooser`; its backdrop
  intercepts clicks. Fixed by a shared `dismissLayoutChooser` (clicks
  `layout-choose-empty`) called in every spec's `before` hook, and a shared
  `openSessionPane` (context-menu "Open in new tab") since single-click on an
  unbound session is now a deliberate no-op. Also: the harness now shares the
  UI's `client_id` (read from `<config>/client-id`) so side-channel
  `create_tab` lands in the layout the UI renders (tab layouts are per-client).

- [x] **Second-wave drift (the 8 specs that survived the first two fixes).**
  Run the failing subset, triage each from its real error + screenshot:
  - `pane-controls-discoverability` — clicking the already-active
    `activity-btn-sessions` collapses the sidebar (VS Code toggle). Added
    `ensureSessionsSidebarOpen` (checks `aria-expanded`).
  - `preset-launch-progress` — asserted the stop against the sidebar row,
    which re-renders/hides for a stopped unbound preset session. Rewritten to
    assert the daemon's `session_updated(stopped)` over the side-channel
    (behavior, not DOM), and to confirm the record is not removed.
  - `source-control-diff` — cached `active-repo` handle went stale on
    follow-active-pane re-render; re-query each poll and widen the follow
    timeout (the decoy repo is `repos[0]` until the fixture pane's focus
    propagates).
  - `session-color-customization` — two causes. (1) `openSessionPane` for a
    2nd session in a spec silently failed: "Open in new tab" only auto-activates
    the new tab when no tab was already active (App.tsx `pendingTabActivate`;
    `sendOpenInNewTab` never arms it), so the background tab's pane never
    mounted. Fixed in the shared helper by activating the new tab via its
    tab-bar pill. (2) After reload the app restores the first tab as active, so
    the session's pane isn't in the DOM to inspect — the post-reload check now
    verifies persistence via the sidebar row's re-applied accent plus the
    daemon's full session-appearance snapshot instead of the pane.
  - `standalone-shell-launch` — modal shell renders in a narrow split pane, so
    the prompt path soft-wraps and split the folder name across rows;
    `waitForBufferText` now joins wrapped continuation rows (xterm
    `line.isWrapped`).
  - `undo-shelf` — bind gesture is now double-click (single-click is a no-op);
    binding is intentionally not undoable (see below).
  - `spawn-defaults` — edit-before-launch target is locked (see below).
  - `popout-session-autoclose` — app bug fixed (see below).

## App behavior decisions / fixes made during resync

- **Edit-before-launch locks the target** (user-confirmed intended). The dialog
  shows a read-only `spawn-target-implied` label, not `spawn-target-select`;
  test updated to match.
- **Binding a session into a pane is not undoable** (user-confirmed intended;
  the "Bound session" undo entry was dropped in 20d92bb). Test rewritten to
  verify the bind, not an undo.
- **Pop-out orphaning bug fixed** (app change, `App.tsx`). The pane/tab/session
  pop-out close effects used `if (state.tabs.length === 0) return` to skip the
  pre-hydration empty state, which also skipped the post-hydration "last tab
  removed" case — so discarding a session whose pane was in the only tab left
  the pop-out open on a "Pane not found" placeholder. Now latched behind
  `tabsHydratedRef` / `sessionsHydratedRef` so a genuine empty-after-hydration
  state closes the window.
- **New tabs auto-activate on explicit add** (app change, user request). The
  tab-bar "+" button (`TabBar.onNewTab`) and the session context menu's "Open
  in new tab" (`sendOpenInNewTab`) now arm `onArmNextNewTab()` before
  `create_tab`, so a user-initiated new tab becomes active instead of opening
  in the background. Side-channel `create_tab` (harness-driven) is unaffected —
  only the two UI gestures arm activation; preset launches keep their own
  current-tab logic.

## Machine setup note (not a code change)

- `msedgedriver` must match the installed WebView2/Edge major. The bundled app
  reported Edge 150 while the cached driver was 147, so every session failed
  with `session not created: … only supports Microsoft Edge version 147`. Fix:
  re-run `& "$HOME/.cargo/bin/msedgedriver-tool.exe"` to fetch the matching
  driver. Worth having `pnpm doctor` compare driver major vs the app's WebView2
  major and warn on mismatch.

## Related

- Paste-fidelity work that surfaced this: `terminal-paste.spec.ts` now asserts
  byte-exact fidelity (sha256 via the `fake-claude` shim) instead of only
  checking the first/last marker. See also the instrumentation in
  `apps/tauri-app/src/components/Terminal.tsx`.
- [e2e-test-coverage-strategy.md](./e2e-test-coverage-strategy.md)
