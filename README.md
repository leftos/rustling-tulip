# rustling-tulip

Multi-repo Claude Code wrapper. Tauri desktop app + long-lived Rust daemon that orchestrates many parallel `claude` sessions across repos and across coordinated multi-repo "workspaces".

## Layout

```
crates/
  protocol/     - shared message types (daemon <-> client)
  daemon/       - long-lived background daemon (WS server, PTY pool, registry)
  daemon-client/ - client-side daemon supervision shared by the clients
apps/
  native/       - native desktop client (GPUI + alacritty_terminal), replacing tauri-app
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

The Tauri app auto-starts the daemon if it isn't already running. Daemon listens on a random loopback port; connection details are written to `%APPDATA%\leftos\rustling-tulip\config\daemon.json` (override the directory with `RUSTLING_TULIP_CONFIG_DIR`).

## Phase status

- [x] Phase 0: standalone `claude-ws.ps1` launchers in workspace member repos
- [x] Phase 1: daemon spine + single-repo PTY sessions
- [x] Phase 2: multi-repo workspace sessions (incl. VS Code `.code-workspace` auto-detect)
- [x] Phase 3: headless mode + structured state
- [x] Phase 4: notifications + attention model
- [x] Phase 5: polish — resizable persistent panes, per-session config
       (model / permission mode / env), orphan-session reattach, scrollback
       persistence, pop-out windows. Auto-update is **deferred** until a
       distribution channel (signing + release pipeline) exists.
- [x] Phase 6: git tracking layer (per-session diff viewer, commit history,
       "open in forge" links)

See `docs/plan.md` for the full plan and `docs/plans/` for follow-up designs.

## Glossary

- **Native client**: the GPUI + `alacritty_terminal` desktop client under `apps/native` that replaces the Tauri app; see `docs/plans/native-client.md`.
- **Parity checklist**: `docs/plans/native-client-parity.md`, every user-visible Tauri feature with its source file; the native client reaches parity when it is all ticked.
- **Tauri freeze**: the Tauri app takes bug fixes only while the native client catches up; new features go to the native client.
- **Spike**: throwaway code that proves an approach works, kept outside the main build (`spikes/`) and deleted once its code is ported.
- **E2E tier / smoke tier**: the native client's opt-in test layers above the in-process UI specs. The e2e tier drives the client in-process against a real daemon isolated under `.tmp/`; the smoke tier launches the real exe in a cloaked window. They run through `rt.ps1 native-e2e` and `native-smoke`.
- **Re-ask**: the native client resending a queued in-place spawn, unchanged, when its checkout prompt's turn comes, so the daemon answers with current numbers instead of the stale prompt being shown.
- **Forwarder**: the daemon's per-connection task that streams one session's PTY output to one client; `LoadScrollback` replaces it.
- **P1.1, P1.2, …**: item ids in `docs/plans/native-client.md`, as phase number and item number.
- **Session history**: the daemon's record of ended sessions, one `history/<id>.json` per session under the config dir, kept 7 days; see `docs/plans/session-recovery.md`.
- **Unexpected end**: a session whose tracer was lost (killed or crashed) rather than one that exited or was closed; these are pre-ticked for recovery.
- **Recovery**: respawning a session from its history entry with `claude --resume <conversation id>`, as a Claude session or as a shell that types the command.
- **Folder-only entry**: a history entry with no spawn config (imported from a tracer log, or a standalone session); it is recovered by running claude in its folder with its `--add-dir` set, never by checking out a branch.
- **Tracer-log import**: the startup pass that rebuilds history entries from `logs/tracer-<id>.log` files for sessions that ended before history existed.
