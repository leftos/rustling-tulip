# E2E test coverage vs hand-testing — strategy

How the `tools/e2e` harness (wdio + tauri-driver + side-channel WS + fake-claude) changes the
split between work an agent can verify automatically and work the user still has to
hand-test. Companion to `docs/ux-audit.md`.

## What the harness gives us

- **Real Tauri shell under `tauri-driver`.** Long-lived WebDriver session via `pnpm host`,
  drivable from Bash with subcommands: `screenshot`, `click`, `type`, `wait`, `eval`,
  `dump-html`, `ws-send`, `ws-recent`, `status`, `shutdown`.
- **Side-channel WS to the daemon** for `add_repo`, `spawn_session`, `stop_session` etc.
  without going through native OS dialogs.
- **`fake-claude` stand-in** for the CLI so tests are deterministic and fast (no model
  calls, predictable banner).
- **`globalThis.__rt_terms` in dev builds** so `eval` can read xterm scrollback even
  though xterm renders to a canvas (DOM scraping doesn't work).
- **`globalThis.__rt_console` in dev builds** captures every `console.*` call plus
  `window.error` / `unhandledrejection` into a bounded ring. Lets specs assert
  "boot produced no errors" trivially and gives the failure dump real signal.
  Tree-shaken from production bundles (`if (import.meta.env.DEV)` guard).
- **`captureFailureDump` auto-diagnostics** (wired in `wdio.conf.ts` via
  `afterTest` + `afterHook`, so before-hook crashes are caught too). On any
  failure: URL, title, body snippet, captured console, WebDriver browser logs,
  and a screenshot all land in `.tmp/e2e/<test>.{json,png}` with a printed
  summary inline. No more DevTools copy-paste to diagnose flakes.
- **WebDriverIO smoke spec** (`webview.spec.ts`) as the pattern for new specs —
  full app+daemon+fake-claude path under tauri-driver.
- **Mtime-gated dev rebuild** keyed off a `.e2e-dev-build` marker in `dist/`.
  A re-run with no source changes hits ~6s; a re-run after a JS edit or a
  prior `pnpm tauri build` clobber forces a `vite build --mode development`
  in ~14s. Cargo handles incremental rebuilds of the app + daemon directly
  (no more `pnpm tauri build` wrapper, which forced a full prod vite build
  every time).

## Re-bucketed against the audit's 78 hand-test items

### Bucket A — confirmable end-to-end (~30 items)

Anything that's a deterministic UI state transition reachable from the daemon's protocol +
sidebar surface. Examples:

- "Removing a repo with active sessions leaves them in Detached" → ws-send `remove_repo`,
  screenshot, check DOM.
- "Empty pane after stopping all sessions" → spawn, stop, check `data-testid=session-pane`
  count.
- "Stale `pendingTabActivate` after rapid actions" → drive in sequence, assert active tab
  id.
- Most of the "what state is the UI in after sequence X" tests in the Tabs + panes and
  Sidebar buckets.
- The Section 1 top-three findings can be turned into **failing tests right now**, which
  is the highest-leverage outcome — they document the bug AND prevent regression once
  fixed.

### Bucket B — spot-check via screenshots, you still eyeball (~20 items)

Some of these moved to Bucket A once the per-iteration cost dropped and the dump
includes `getComputedStyle`-reachable state. Examples that promoted:

- "Status badge contrast" → assertable via `getComputedStyle().color` + a contrast
  ratio check. No human eye needed.
- "Drop-zone overlay is visible during drag" → check the overlay element exists
  with non-`display:none` styles during the WDIO drag sequence.
- "Tab pill truncate behaviour" → measure `scrollWidth > clientWidth` plus a
  computed-style ellipsis assertion.

Still genuinely eyeball-only:

- "Pane resize feels smooth" — feel isn't measurable from the DOM.
- "Modal animation jank" — frame-rate questions need a profiler, not WDIO.
- Most truly aesthetic items in the Global / Terminal buckets.

### Bucket C — still purely manual (~23 items)

- Anything involving the OS shell: file picker, system notifications, taskbar/dock state.
- **Anything in a pop-out window.** The harness README notes pop-out tab/session windows
  require a second WebDriver session, not wired up yet. That blocks the audit's pop-out
  findings (several).
- Real-world data volume: 40+ repos, 200 sessions, 50-pane tabs — I can synthesize via
  ws-send floods, but rendering-perf judgements are still the user's.
- Drag-and-drop nuance: WebDriver's mouse-down/move/up sequence often differs from real
  HTML5 DnD events; some drop-target behavior may need hand verification even if I can
  simulate it.

## Testid gaps to close before going after specific findings

The current testid sweep covers sidebar shell, tab bar/pills, session pane, terminal
container — but **not** the spawn dialog internals, git panel, or preset launch dialog.
The audit's top finding (file/folder presets unlaunchable) lives in `PresetLaunchDialog`
which has no testids. Workaround possible with `eval` + structural selectors, but adding
testids first would make specs readable and stable.

- [x] Add testids to `PresetLaunchDialog` (preview list, launch button, prompt-sources
      controls)
- [x] Add testids to `SpawnDialog` (single/workspace radio, repo select, branch input,
      run-mode radios, spawn button)
- [x] `GitPanel` — moot; component deleted in iter 7, replaced by the global Source
      Control sidebar (`SourceControlSidebar.tsx`)
- [x] Spike a second-WebDriver-session helper so pop-out windows become testable
- [x] **Config-dir isolation for the harness.** `RUSTLING_TULIP_CONFIG_DIR` is now
      honored by the daemon's `Dirs::ensure` (`crates/daemon/src/paths.rs`) and the
      Tauri app's path resolvers (`apps/tauri-app/src-tauri/src/lib.rs::config_dir`),
      and `tools/e2e/wdio.conf.ts` points at `.tmp/e2e/config/` per run. Verified:
      the wdio smoke spec writes state to the tmpdir, real
      `%APPDATA%\leftos\rustling-tulip\config\` is untouched.

## High-leverage move: failing-test → fix → green

Pick the highest-impact audit findings and write each as a failing wdio spec, then fix
until green. That's where the harness pays off most.

The iteration cost has dropped enough that wdio is now competitive with hand-running
for triage, not just for locking in fixes: ~6s on a no-change re-run, ~14s on a JS
edit (the prepare phase is mtime-gated; cargo handles its own incremental check). Plus
the diagnostics dump means a failing spec already tells you URL + body + console +
screenshot without further investigation. Hand-run when you want to *see* the app
behave; reach for a wdio spec as soon as the question is "what state is the UI in
after sequence X?".

Candidate first specs:

- [x] **File/folder preset launch enables button.** `tools/e2e/tests/e2e/specs/preset-launch-folder.spec.ts`
      — preconditions a folder-source preset, asserts launch button enabled and preview
      list populated.
- [x] **Daemon error surfacing.** `tools/e2e/tests/e2e/specs/error-toast.spec.ts` —
      sends a bad spawn over `window.__rt_daemon_client`, asserts `ErrorToast` appears.
- [x] **Repo remove confirms before destroying live sessions.** `tools/e2e/tests/e2e/specs/repo-remove-confirm.spec.ts`
      — drives the 3-way `RepoRemoveDialog` modal end-to-end (cancel / remove anyway /
      stop-and-remove).
- [x] **`user-select: none` doesn't block content selection.** `tools/e2e/tests/e2e/specs/user-select.spec.ts`
      — asserts `getComputedStyle` on `.diff-pane` / `.headless-log` / `.preset-preview-list`
      / `.terminal-host` / `.session-title h2` reports `user-select: text`.
- [x] **Pop-out session window auto-closes on stop.** `tools/e2e/tests/e2e/specs/popout-session-autoclose.spec.ts`
      — uses `captureNewWindow` from `src/popout.ts` to open the pop-out via
      the "Pop out" button, assert the `.session-window-root` renders, stop
      the session via the side-channel WS, and assert the window handle
      disappears from `getWindowHandles()`.
      **Note:** the `captureNewWindow` helper relies on msedgedriver enumerating
      all WebView2 windows in the same process via `getWindowHandles()`. This
      is the standard WebDriver multi-window pattern for WebView2 apps; if
      tauri-driver routes each Tauri window to its own session, the helper will
      throw a clear error and the test will need the `multiremote` path instead.

## Net effect on the audit workflow

- Roughly **half** of the hand-test list becomes scriptable (was "a third" before the
  iteration-cost drop and the diagnostics dump promoted several Bucket-B items).
- Every confirmed audit finding gains a path to a regression test, so fixes don't rot.
- Truly sensory items (feel, animation jank) and multi-window flows stay the user's
  job until the testid gaps above and the second-WebDriver-session helper close.
- Wdio is now cheap enough for triage — write the failing spec first, hand-run only
  when you want to *see* it (or when the question is genuinely sensory).
- Once config-dir isolation lands, the harness becomes safe to run on a developer
  machine without grooming `state.json` afterward.
