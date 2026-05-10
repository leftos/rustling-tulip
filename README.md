# rustling-tulip

Multi-repo Claude Code wrapper. Tauri desktop app + long-lived Rust daemon that orchestrates many parallel `claude` sessions across repos and across coordinated multi-repo "workspaces".

## Layout

```
crates/
  protocol/     - shared message types (daemon <-> client)
  daemon/       - long-lived background daemon (WS server, PTY pool, registry)
apps/
  tauri-app/    - desktop client (Rust src-tauri + React + xterm.js)
docs/
  plans/        - design docs and milestone plans
```

## Build

```powershell
# daemon + protocol
cargo build

# Tauri app
cd apps/tauri-app
pnpm install
pnpm tauri dev
```

## Run

The Tauri app auto-starts the daemon if it isn't already running.
Daemon listens on a random loopback port; connection details are written to
`%APPDATA%\rustling-tulip\daemon.json`.

## Phase status

- [x] Phase 0: standalone `claude-ws.ps1` launchers in workspace member repos
- [x] Phase 1: daemon spine + single-repo PTY sessions
- [x] Phase 2: multi-repo workspace sessions (incl. VS Code `.code-workspace` auto-detect)
- [x] Phase 3: headless mode + structured state
- [x] Phase 4: notifications + attention model
- [~] Phase 5: polish — resizable persistent panes done; per-session config,
       pop-out sessions, auto-update, orphan-reattach still pending

See `docs/plan.md` for the full plan.
