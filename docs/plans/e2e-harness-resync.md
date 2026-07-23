# E2E harness resync

The WebdriverIO suite has drifted from the current app in several independent
ways. Discovered 2026-07-23 while validating the paste-fidelity work — the
`terminal-paste` spec (and ~20 others that spawn a session) can't run to
completion until these are fixed. None are related to the paste change; they
are pre-existing rot in `tools/e2e/`.

## Blockers found (in the order they surface)

- [ ] **Stale `spawn_session` payload.** Every spec's `spawnFakeClaude`/spawn
  helper sends the old flat `agent` / `permission_mode` / `codex_sandbox`
  fields. `protocol::SpawnRequest` now carries a single `agent_options` tagged
  enum (`#[serde(tag = "kind")]`: `claude` / `codex` / `cursor`). The daemon
  can't deserialize the old shape and logs `ignoring unknown client message
  type … type_tag=spawn_session`, so no session ever spawns. Fixed in
  `terminal-paste.spec.ts` only (send `agent_options: { kind: "claude",
  permission_mode: null }`); every other spawn helper still needs it. Consider
  a shared `spawnSession` helper in `tools/e2e/src/` so the shape lives in one
  place. The e2e `ClientMessage` type for `spawn_session` is
  `{ type: "spawn_session"; [k: string]: unknown }` — a loose index signature —
  which is why the stale payload typechecks but fails at runtime; tightening it
  to mirror `SpawnRequest` would have caught this at `tsc`.

- [ ] **LayoutChooser modal blocks session-pane interaction.** After a session
  spawns, a fresh e2e client hits `layout_init_required` and the app renders
  the `LayoutChooser` modal (`data-testid="layout-chooser"`,
  `App.tsx:2943`). Its `.modal-backdrop` intercepts the `sidebar-session`
  click in `openSessionPane`. No spec dismisses it. Needs a shared step that
  resolves the chooser (pick an arrangement) before interacting with panes.

- [ ] **Sweep for further drift.** Only the first two blockers per spec have
  been observed; fixing them will likely expose more (selectors, message
  shapes, timing). Run the full suite after each fix and triage.

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
