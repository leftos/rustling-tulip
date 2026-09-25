# Plan: Multi-Repo Claude Code Wrapper ("rustling-tulip")
<!-- plan-doc-hygiene: 2026-09-24 314d519 -->

A Tauri desktop app that orchestrates many parallel Claude Code sessions across repos,
including coordinated multi-repo "workspace" sessions where a single `claude` instance
operates across several linked repos at once via `--add-dir`. From one window: see every
session's status, attach to any of them, spawn single-repo or workspace sessions with
worktrees created automatically. Daemon keeps everything alive across app restarts.

## Architecture

```
┌────────────────────────┐          ┌────────────────────────────┐
│  Tauri app (client)    │  WS+JSON │  Daemon (long-lived)       │
│  - React/TS UI         │ ───────► │  - Session supervisor      │
│  - xterm.js panes      │ ◄─────── │  - PTY pool (ConPTY)       │
│  - Activity bar        │          │  - rt-tracer.exe sidecar   │
│  - Source control bar  │          │  - Worktree orchestrator   │
└────────────────────────┘          └──────────┬─────────────────┘
                                               │ spawns
                                               ▼
                                    ┌────────────────────────────┐
                                    │  claude / codex CLI        │
                                    │  (interactive or headless) │
                                    └────────────────────────────┘
```

Daemon is a standalone Rust binary; Tauri app is a client. No Anthropic API calls — the daemon shells out to the `claude` or `codex` CLI. Wire protocol is JSON over localhost WebSocket (see `crates/protocol/src/lib.rs`); the current version and the versions still supported are in `protocol-version.json`.

## Shipped

### Phases 0–6
- **Phase 0** — PowerShell workspace launchers (`claude-ws.ps1`) for yaat and towercab-3d
- **Phase 1** — Daemon spine: WS server, auth, PTY sessions, state persistence, Tauri app with xterm.js
- **Phase 2** — Workspace sessions: multi-repo spawn with worktrees, `--add-dir`, VSCode `.code-workspace` auto-detect
- **Phase 3** — Headless sessions: `--print --output-format stream-json` adapter, event log, cost/token from session jsonl
- **Phase 4** — Attention model: awaiting-input detection, OS notifications, tray badge
- **Phase 5** — Polish: per-session config, scrollback ring, pop-out windows, orphan recovery, resizable panes
- **Phase 6** — Git tracking: changed-files panel, inline diff, commit history, open-in-forge, stage/unstage/commit, `.git` watcher

### Post-Phase-6
- **Upgrade-survivable sessions** — `rt-tracer.exe` PTY supervisor survives daemon restarts; orphan reattach replays ring buffer. See `docs/plans/completed/upgrade-survivable-sessions.md`.
- **E2E harness** — wdio + tauri-driver + fake-claude + side-channel WS; config-dir isolated to `.tmp/e2e/`; 11 spec files. Multi-window helper (`src/popout.ts`) enables pop-out window specs via WebDriver `getWindowHandles()` + `switchToWindow`. See `docs/plans/completed/e2e-test-coverage-strategy.md`.
- **Codex support** — per-session `Agent` enum (Claude / Codex); `build_codex_args` in `server.rs`; headless stays claude-only; workspace prelude injected for cross-repo path clarity. See `docs/plans/completed/add-support-for-codex.md`.
- **Source Control sidebar** — VSCode-style activity-bar sidebar; path-folded ChangesTree; Monaco diff tabs; stage/unstage/commit/discard/stash; paginated history. See `docs/plans/completed/source-control-sidebar.md`.
- **UX audit** — 52 iteration passes; all code-evidence findings resolved. Detached bucket gets a "Stop all" inline action (iter 51); pop-out window findings closed as won't-fix (iter 52). See `docs/ux-audit.md` for full history.
- **Drag-to-reorder** — tabs, repos/workspaces, and session leaves all reorderable via drag.
- **Design language** — Geist font + cool-neutral palette; hardcoded colors/spacing replaced with design tokens.
- **Daemon-status footer** — persistent connection status bar + troubleshooting flyout.
- **NSIS installer** — bundles `rustling-tulipd.exe` + `rt-tracer.exe` as sidecars.
- **Two-mode workspace creator** — repo-list mode or VS Code `.code-workspace` file import.
- **Fresh worktree fork points** — spawns no longer cut branches from a long-stale local default. Auto-detected bases resolve to their remote-tracking counterpart (`main` → `origin/main`); the dialog background-fetches on open, offers remote refs in the base picker, reports "N commits behind origin/main", and prompts Reuse / Recreate-from-base when a worktree already exists at the target path instead of silently binding to it. See `docs/plans/completed/worktree-fork-point-staleness.md`.
- **Discard branch fate** — "delete worktree" reaps the session branch when its commits already landed on the base or its remote by ancestry *or* patch equivalence (`git cherry`), so cherry-picked and rebased work counts as merged; every delete-worktree gesture (pane close, context menu, stopped-pane overlay, quit) goes through one confirm modal that names each member's branch fate and offers keep/delete when work is unlanded. Spawn dialog base-branch pickers moved from `<datalist>` to `BranchCombobox`. See `docs/plans/completed/discard-branch-fate.md`.
- **Daemon-picked worktree names** — random `wt/<adjective>-<noun>` names come from the daemon (`SuggestBranchName`), which rejects any name in a member repo's local or remote refs or with a worktree dir already on disk. Launch-last and preset launches spawn with `WorktreeReusePolicy::RefuseLeftover`, so a branch-only leftover is refused with a modal naming its tip and staleness instead of being attached silently. `pnpm run doctor` in `tools/e2e` checks the msedgedriver major against the installed WebView2. See `docs/plans/completed/worktree-branch-leftovers.md`.
- **Duplicate on a fresh branch** — duplicating a worktree session spawns the clone on a daemon-picked `wt/` name under `RefuseLeftover` instead of replaying the source's branch (which either collided with the running source's worktree or could attach a leftover). A pinned source's pin is dropped too, so the clone always gets its own worktree under the daemon root. In-place duplicates still replay the branch. See `docs/plans/completed/duplicate-fresh-branch.md` and `docs/plans/completed/duplicate-drop-pin.md`.
- **Remote LAN access** — opt-in `0.0.0.0` TLS listener (self-signed cert + fingerprint pinning/TOFU), off by default; a pinned-TLS loopback tunnel in the Tauri app bridges the webview WS to the remote daemon. Per-client tab/pane layouts (sessions stay global to the daemon); host auto-start on login (HKCU `Run`); mDNS discovery + short-code pairing. See `docs/plans/completed/remote-lan-access.md`.
- **Terminal path opening** — ctrl-click on a path hands it to the OS default handler (`ShellExecuteExW` via the opener plugin on Windows) instead of always shelling out to VS Code; a `:line[:col]` ref still goes to VS Code with `-g`, since no OS handler can honor one, and an unassociated type gets the OS's own "open with" picker. The link provider now reads a window of buffer rows instead of one, so a path broken across rows is stitched back together: soft wraps merge on the buffer's `isWrapped` flag, hard wraps merge only when the row above is flush against its wrap column and the break falls mid-path-token, and the merged reading is sent ahead of the un-stitched fragment so the backend opens whichever exists. See `docs/plans/completed/terminal-path-open.md`.
- **Keep the host awake** — the daemon holds an OS idle-sleep inhibitor (`SetThreadExecutionState` on a dedicated thread on Windows, `caffeinate -i` on macOS) while any session has a live child, using the same liveness predicate as idle-exit. Display sleep is untouched. Persisted `keep_awake` host setting in `state.json` (default on), toggled from Settings → General; `KeepAwakeStatus` reports whether the hold is engaged. See `docs/plans/completed/keep-awake.md`.
- **Launch into an existing worktree** — worktrees are addressed by path instead of derived branch name, so a worktree left behind by a gone session (`RootWorktreeStatus::Stale`) can be launched into from the Manage Worktrees modal and the spawn dialog. See `docs/plans/completed/launch-into-existing-worktree.md`.
- **Bug hunt** — full-codebase audit, 2026-07-29 against `c4c2676`: 12 findings, all fixed with regression tests (headless Stop hang, `BracketedPasteTracker` carry, nested build-dir filter, preset `%VAR%` expansion). See `docs/plans/completed/bug-hunt.md`.
- **Main UX improvements** — the 2026-05-13 UX review's lifecycle, identity, colour, spawn-safety and source-control passes. Its three open plain-shell cwd re-homing items moved to the native client's parity checklist ("Plain-shell sessions regroup under the container matching their live cwd"). See `docs/plans/completed/main-ux-improvements.md`.

## Open

Priority order for `/nextup`: **Current focus** first, then the sections below it top to bottom.

### Current focus: native client
Replace the Tauri/WebView2 frontend with a native GPUI + `alacritty_terminal` client (`apps/native`); the daemon, tracer and protocol stay. The Tauri app is frozen to bug fixes (user, 2026-09-23). Phases, rulings and brief-sized items: [native-client.md](./plans/native-client.md); feature-by-feature scope: [native-client-parity.md](./plans/native-client-parity.md).
- [ ] **Next up:** Phase 1, continuing at P1.7d (shells and empty-pane spawn) and P1.9 (scrollback request id), then P1.7c-C, P1.10d and P1.8. P1.1–P1.6, P1.7a–c and P1.10a–e are done. See [native-client.md](./plans/native-client.md) for the full list
- [ ] Remote file transfer: fetch a file from the host to the remote client by Ctrl-clicking it or through a "Fetch file…" popup. The daemon and protocol half (FT.1) has landed. The client side lands with Phase 6, and the Ctrl-click trigger also needs Phase 2. See [remote-file-transfer.md](./plans/remote-file-transfer.md)
- [x] Security fix: `GetFileSnapshot` / `GetFileDiff` now pass the client-supplied `path` through `file_fetch::confine_path` / `check_relative` before using it. `repo_target_or_err` also canonicalizes `worktree_path` before checking it's under the worktrees root.

### Auto-update
`tauri-plugin-updater` is ~2 hours of in-app work but blocked until a signed release
pipeline exists (no GH Actions pipeline, no signing cert, no hosted manifest).

### Open question: `--add-dir` hook/settings propagation
Does `claude --add-dir` propagate hooks and `settings.json` from each additional root,
or only from the primary `cwd`? Not yet verified empirically. Relevant to workspace
sessions where member repos may have their own `CLAUDE.md` / hooks.

### macOS compatibility — greenlit 2026-05-30
- [ ] **Next up:** produce the detailed macOS implementation plan from the phased
      map in `docs/plans/macos-compat.md`, then start at Phase M0. First settle
      the two open scoping decisions with the user (IPC transport: `#[cfg]` alias
      vs `interprocess`; distribution: dev-only vs signed `.dmg`) — both are in
      that doc's "Decisions to make during planning".

Substantially portable already: PTY via `portable-pty`, config/data dirs via
`directories`, most OS calls already dual-armed (`#[cfg(not(windows))]`). One
architectural blocker — the tracer↔daemon IPC is Windows named pipes with no Unix
path (needs Unix domain sockets) — plus a `job_object` module compile-gate,
autostart (LaunchAgent), and a macOS bundle target. Full catalog + phasing in
`docs/plans/macos-compat.md`.

### Tooling cleanups (singles)
Pre-existing warnings and traps noticed during the 2026-09-24 native-client session.
- [ ] Drop the unused `Unicode-DFS-2016` entry from `deny.toml` `[licenses] allow`; `cargo deny check` reports it as never encountered
- [ ] Clear cargo's future-incompatibility warning for `proc-macro-error2` v2.0.1: find which dependency pulls it in (`cargo tree -i proc-macro-error2`) and update it if a newer release drops it. Waits for the next gpui release (checked 2026-09-25): the chain is `gpui` 0.2.2 (latest) → `stacksafe` 0.1.4 (latest 0.1.x) → `stacksafe-macro` 0.1.4 → `proc-macro-error2`, whose last commit is from 2024-09. `stacksafe-macro` 1.0.3 no longer uses it, and zed's main branch already has `stacksafe = "1.0"`. A `[patch]` can't cross from 0.1 to 1.0, so bump gpui when a release after 0.2.2 ships
- [ ] Fix the 8 Information-level PSScriptAnalyzer findings in `rt.ps1` (PSAvoidUsingPositionalParameters on `Join-Path`)
- [x] Add a trap to the `rustling-tulip-nextup` profile: a fresh worktree can't run workspace clippy (or pass the prek clippy hook) until the Tauri sidecars are copied into `apps/tauri-app/src-tauri/binaries/`, an empty `apps/tauri-app/dist/` exists, and the Windows SDK `rc.exe` dir is on PATH. As built: the trap says to run `.\rt.ps1 build` (which stages the sidecars) and create `dist/`. The `rc.exe` part didn't reproduce: clippy passed in five worktrees on 2026-09-25 with no `rc.exe` on PATH
- [ ] Keep Rust build output from filling the dev drive (user, 2026-09-25: the rustling-tulip folders took 180 GB of the 250 GB drive, and a full drive broke a gate with LNK1180). Each concurrent worktree needs its own `CARGO_TARGET_DIR`, 13–37 GB each: a shared one mixed up two trees' `protocol` builds. Add to the `rustling-tulip-nextup` profile's landing step: delete the item's target dir with its worktree. Also consider a periodic `cargo sweep` of the main `target/` (59 GB). Done 2026-09-25: `D:\.cargo\config.toml` sets `[profile.dev.package."*"] debug = false` for every Cargo project under D: (confirmed: `serde` compiles with no debuginfo flag, `protocol` with `-C debuginfo=2`). The size saving is not yet measured; compare `target/` after the next full rebuild. Also done 2026-09-25: the profile's GPUI trap now gives each worktree its own in-tree `target/`, which `git worktree remove` deletes with the worktree, instead of a shared `CARGO_TARGET_DIR`. Still open: the `cargo sweep` of the main `target/` and the size measurement
- [ ] Agents read 25–27 files before their first edit (friction `d2` ×5, ~$48; four of the runs were in the frozen Tauri app, one in `crates/daemon/src/git.rs`): write a task index for the code that lives on, `docs/architecture.md` linked from CLAUDE.md's "Project shape", with rows like "add a daemon message" (`crates/protocol/src/lib.rs` → `crates/daemon/src/server.rs` → `apps/native` handler), "change worktree/git behaviour" (`git.rs`, `workspace.rs`, `git_watch.rs`) and "add a native client view", naming the files in order for briefs to cite

## Out of scope

- Sub-agent / Task-tool interception or isolation
- Auto-discovery of repos (registry is manual only)
- Attach beyond the LAN — internet exposure, cloud relay, SSH tunneling (LAN-scoped remote access shipped; see Shipped)
- Cloud sync of registry or sessions
- Mobile companion app
