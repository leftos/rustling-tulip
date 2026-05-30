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
- **E2E harness** — wdio + tauri-driver + fake-claude + side-channel WS; config-dir isolated to `.tmp/e2e/`; 11 spec files. Multi-window helper (`src/popout.ts`) enables pop-out window specs via WebDriver `getWindowHandles()` + `switchToWindow`. See `docs/plans/e2e-test-coverage-strategy.md`.
- **Codex support** — per-session `Agent` enum (Claude / Codex); `build_codex_args` in `server.rs`; headless stays claude-only; workspace prelude injected for cross-repo path clarity. See `docs/plans/completed/add-support-for-codex.md`.
- **Source Control sidebar** — VSCode-style activity-bar sidebar; path-folded ChangesTree; Monaco diff tabs; stage/unstage/commit/discard/stash; paginated history. See `docs/plans/completed/source-control-sidebar.md`.
- **UX audit** — 52 iteration passes; all code-evidence findings resolved. Detached bucket gets a "Stop all" inline action (iter 51); pop-out window findings closed as won't-fix (iter 52). See `docs/ux-audit.md` for full history.
- **Drag-to-reorder** — tabs, repos/workspaces, and session leaves all reorderable via drag.
- **Design language** — Geist font + cool-neutral palette; hardcoded colors/spacing replaced with design tokens.
- **Daemon-status footer** — persistent connection status bar + troubleshooting flyout.
- **NSIS installer** — bundles `rustling-tulipd.exe` + `rt-tracer.exe` as sidecars.
- **Two-mode workspace creator** — repo-list mode or VS Code `.code-workspace` file import.
- **Remote LAN access** — opt-in `0.0.0.0` TLS listener (self-signed cert + fingerprint pinning/TOFU), off by default; a pinned-TLS loopback tunnel in the Tauri app bridges the webview WS to the remote daemon. Per-client tab/pane layouts (sessions stay global to the daemon); host auto-start on login (HKCU `Run`); mDNS discovery + short-code pairing. See `docs/plans/remote-lan-access.md`.

## Open

### Auto-update
`tauri-plugin-updater` is ~2 hours of in-app work but blocked until a signed release
pipeline exists (no GH Actions pipeline, no signing cert, no hosted manifest).

### Open question: `--add-dir` hook/settings propagation
Does `claude --add-dir` propagate hooks and `settings.json` from each additional root,
or only from the primary `cwd`? Not yet verified empirically. Relevant to workspace
sessions where member repos may have their own `CLAUDE.md` / hooks.

### macOS compatibility
Explored 2026-05-30 (investigation only, not greenlit). Substantially portable
already: PTY via `portable-pty`, config/data dirs via `directories`, most OS calls
already dual-armed (`#[cfg(not(windows))]`). One architectural blocker — the
tracer↔daemon IPC is Windows named pipes with no Unix path (needs Unix domain
sockets) — plus a `job_object` module compile-gate, autostart (LaunchAgent), and a
macOS bundle target. Full catalog + phased plan in `docs/plans/macos-compat.md`.

## Out of scope

- Sub-agent / Task-tool interception or isolation
- Auto-discovery of repos (registry is manual only)
- Attach beyond the LAN — internet exposure, cloud relay, SSH tunneling (LAN-scoped remote access shipped; see Shipped)
- Cloud sync of registry or sessions
- Mobile companion app
