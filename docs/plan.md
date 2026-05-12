# Plan: Multi-Repo Claude Code Wrapper ("rustling-tulip")

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

Daemon is a standalone Rust binary; Tauri app is a client. No Anthropic API calls — the
daemon shells out to the `claude` or `codex` CLI. Wire protocol is JSON over localhost
WebSocket (see `crates/protocol/src/lib.rs`). Current `PROTOCOL_VERSION`: 15.

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
- **E2E harness** — wdio + tauri-driver + fake-claude + side-channel WS; config-dir isolated to `.tmp/e2e/`; 10 spec files, 24+ tests. See `docs/plans/e2e-test-coverage-strategy.md`.
- **Codex support** — per-session `Agent` enum (Claude / Codex); `build_codex_args` in `server.rs`; headless stays claude-only; workspace prelude injected for cross-repo path clarity. See `docs/plans/completed/add-support-for-codex.md`.
- **Source Control sidebar** — VSCode-style activity-bar sidebar; path-folded ChangesTree; Monaco diff tabs; stage/unstage/commit/discard/stash; paginated history. See `docs/plans/completed/source-control-sidebar.md`.
- **UX audit** — 11 iteration passes covering: testids, preset launch fix, error toast, destructive confirmations, identity/trust, tab↔sidebar sync, keyboard a11y, pane controls, global shortcuts, auto-reconnect, modal a11y sweep, attention-state cleanup, worktree-default UX, drag feedback, settings modal, paste/multiline, and more. See `docs/ux-audit.md` for item-by-item status.
- **Drag-to-reorder** — tabs, repos/workspaces, and session leaves all reorderable via drag.
- **Design language** — Geist font + cool-neutral palette; hardcoded colors/spacing replaced with design tokens.
- **Daemon-status footer** — persistent connection status bar + troubleshooting flyout.
- **NSIS installer** — bundles `rustling-tulipd.exe` + `rt-tracer.exe` as sidecars.
- **Two-mode workspace creator** — repo-list mode or VS Code `.code-workspace` file import.

## Open

### UX audit — 4 remaining code findings
See `docs/ux-audit.md` for the full tracked list. What's left:

1. **No empty state for workspaces-but-orphans-only sidebar** — `Sidebar.tsx:550`. When all sessions under a workspace are orphaned/detached, the container shows as empty with no explanation.
2. **`TabWindow.focusedPaneId` is independent** — `TabWindow.tsx:21`. Pop-out tab windows track their own focus state separately from the main window, causing inconsistency.
3. **`SessionWindow` ignores broadcast `Repos`/`Workspaces`** — the pop-out session window doesn't update its local registry state when the daemon broadcasts registry changes.
4. **Pop-out close has no confirmation** — `App.tsx:354`. Closing the pop-out OS window skips the exit confirmation that the main window shows.

### E2E — second-WebDriver-session helper
Pop-out windows require a second wdio `browser` session, which isn't wired up yet. This
blocks `tools/e2e/tests/e2e/specs/` coverage of the pop-out auto-close finding above.
Entry point: `tools/e2e/wdio.conf.ts`. See `docs/plans/e2e-test-coverage-strategy.md`.

### Auto-update
`tauri-plugin-updater` is ~2 hours of in-app work but blocked until a signed release
pipeline exists (no GH Actions pipeline, no signing cert, no hosted manifest).

### Open question: `--add-dir` hook/settings propagation
Does `claude --add-dir` propagate hooks and `settings.json` from each additional root,
or only from the primary `cwd`? Not yet verified empirically. Relevant to workspace
sessions where member repos may have their own `CLAUDE.md` / hooks.

## Out of scope

- Sub-agent / Task-tool interception or isolation
- Auto-discovery of repos (registry is manual only)
- Multi-machine / SSH attach
- Cloud sync of registry or sessions
- Mobile companion app
