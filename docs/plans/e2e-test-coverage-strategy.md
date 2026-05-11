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
- **WebDriverIO smoke spec** as the pattern for new specs — full app+daemon+fake-claude
  path under tauri-driver.

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

### Bucket B — spot-check via screenshots, you still eyeball (~25 items)

- "Tab pills truncate awkwardly at common widths" — I can screenshot, but the call on
  whether it looks bad is the user's.
- "Status badge contrast in light/dark mode" — I can render, the user judges.
- "Drop-zone overlay is visible enough during drag" — I can drive, but can't tell whether
  it's distracting or subtle.
- "Pane resize feels smooth" — feel isn't measurable.
- Most of the visual / sensory items in the Global and Terminal buckets.

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

- [ ] Add testids to `PresetLaunchDialog` (preview list, launch button, prompt-sources
      controls)
- [ ] Add testids to `SpawnDialog` (single/workspace radio, repo select, branch input,
      run-mode radios, spawn button)
- [ ] Add testids to `GitPanel` (status/commits/file-diff tabs, file rows, diff pane)
- [ ] Spike a second-WebDriver-session helper so pop-out windows become testable

## High-leverage move: failing-test → fix → green

Rather than convert the whole checklist, pick the highest-impact audit findings and write
each as a failing wdio spec, then fix until green. That's where the harness pays off
most. The cost of a wdio iteration (tauri-driver bring-up, debug-binary launch, spec run)
is too high for "is this finding actually a bug or did I misread the code?" — hand-run
for triage, wdio for documenting & locking in fixes.

Candidate first specs:

- [ ] **File/folder preset launch enables button.** Spec preconditions a preset with a
      `folder` source (fixture dir), opens `PresetLaunchDialog`, asserts the launch
      button is enabled and `previewPrompts.length > 0`. Fails today
      (`PresetLaunchDialog.tsx:83-87, 544-554`); passes after fix.
- [ ] **Daemon error surfacing.** Spec sends a known-bad spawn over WS (e.g. unknown
      repo_id), asserts a visible error region appears in the DOM. Requires
      introducing the error surface — the spec defines done.
- [ ] **Repo remove confirms before destroying live sessions.** Spec adds a repo,
      spawns a session, clicks the × on the repo, asserts a confirmation surface (modal
      or two-state button) and that the session is still attached until the user
      confirms. Fails today (`Sidebar.tsx:276-287`).
- [ ] **`user-select: none` doesn't block content selection.** Spec asserts that
      `getComputedStyle` on a `.diff-pane`, `.headless-log`, or session-label element
      reports `user-select: text` (or `auto`). Fails today (`styles.css:25-31`).
- [ ] **Pop-out session window auto-closes on stop.** Blocked on second-WebDriver
      session helper. Tracks the audit finding at `App.tsx:344-351`.

## Net effect on the audit workflow

- Roughly a third of the hand-test list becomes scriptable.
- Every confirmed audit finding gains a path to a regression test, so fixes don't rot.
- Sensory and multi-window stuff stays the user's job until the testid gaps above and
  the second-WebDriver-session helper close.
- Hand-running for triage stays the right default; promote findings to wdio once they're
  understood enough to be worth locking in.
