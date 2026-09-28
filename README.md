# rustling-tulip

Multi-repo Claude Code wrapper. Native desktop client + long-lived Rust daemon that orchestrates many parallel `claude` sessions across repos and across coordinated multi-repo "workspaces".

## Layout

```
crates/
  protocol/     - shared message types (daemon <-> client)
  daemon/       - long-lived background daemon (WS server, PTY pool, registry)
  daemon-client/ - client-side daemon supervision shared by the clients
apps/
  native/       - native desktop client (GPUI + alacritty_terminal)
tools/
  e2e/fake-claude/ - fake `claude` CLI shim for the native e2e tier
docs/
  plans/        - design docs and milestone plans
```

## Build

```powershell
# daemon + tracer + native client
.\rt.ps1 build

# build, then run the native client
.\rt.ps1
```

`.\rt.ps1 help` lists the other commands.

## Run

The native client auto-starts the daemon if it isn't already running. Daemon listens on a random loopback port; connection details are written to `%APPDATA%\leftos\rustling-tulip\config\daemon.json` (override the directory with `RUSTLING_TULIP_CONFIG_DIR`).

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

- **Native client**: the GPUI + `alacritty_terminal` desktop client under `apps/native`; see `docs/plans/native-client.md`.
- **Parity checklist**: `docs/plans/native-client-parity.md`, every user-visible Tauri feature with its source file (paths on the `tauri-last` tag); the native client reaches parity when it is all ticked.
- **tauri branch**: the maintenance branch holding the earlier Tauri desktop app, cut from the `tauri-last` tag. Tauri hotfixes are made there, daemon fixes are cherry-picked there from `main`, and the Tauri installer is built there; `main` keeps protocol 22 decodable for it.
- **Spike**: throwaway code that proves an approach works, kept outside the main build (`spikes/`) and deleted once its code is ported.
- **E2E tier / smoke tier**: the native client's opt-in test layers above the in-process UI specs. The e2e tier drives the client in-process against a real daemon isolated under `.tmp/`; the smoke tier launches the real exe in a cloaked window. They run through `rt.ps1 native-e2e` and `native-smoke`.
- **Re-ask**: the native client resending a queued in-place spawn, unchanged, when its checkout prompt's turn comes, so the daemon answers with current numbers instead of the stale prompt being shown.
- **Forwarder**: the daemon's per-connection task that streams one session's PTY output to one client; `LoadScrollback` replaces it.
- **P1.1, P1.2, …**: item ids in `docs/plans/native-client.md`, as phase number and item number.
- **Session history**: the daemon's record of ended sessions, one `history/<id>.json` per session under the config dir, kept 7 days; see `docs/plans/session-recovery.md`.
- **Unexpected end**: a session whose tracer was lost (killed or crashed) rather than one that exited or was closed; these are pre-ticked for recovery.
- **Recovery**: respawning a session from its history entry with `claude --resume <conversation id>`, as a Claude session or as a shell that types the command.
- **Folder-only entry**: a history entry with no spawn config (imported from a tracer log, or a standalone session); it is recovered by running claude in its folder with its `--add-dir` set, never by checking out a branch.
- **MA1, MA2, …**: item ids in `docs/plans/mobile-app.md`, the iOS and Android app's phases (distinct from the macOS plan's M0–M4).
- **Relay**: the planned `crates/relay` service on a small VPS that joins a daemon's outbound connection to the phone's, forwarding bytes it cannot read; the pinned TLS runs end to end through it.
- **Shared client core**: the planned Rust crate holding the pinned-TLS connect, host profiles and pairing, used by both the native client's remote mode and the mobile app.
- **Tracer-log import**: the startup pass that rebuilds history entries from `logs/tracer-<id>.log` files for sessions that ended before history existed.
