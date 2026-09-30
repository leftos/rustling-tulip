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
  architecture.md - components, what the product does, task index
  native-client.md - the native client's design decisions
  plans/MAIN.md - the main plan: open work in waves
  plans/        - subplans for open items (completed/ holds older finished designs)
```

## Build

```powershell
# daemon + tracer + native client, then sweep target/ output older than 14 days
.\rt.ps1 build

# build, then run the native client
.\rt.ps1
```

`.\rt.ps1 help` lists the other commands.

## Run

The native client auto-starts the daemon if it isn't already running. Daemon listens on a random loopback port; connection details are written to `%APPDATA%\leftos\rustling-tulip\config\daemon.json` (override the directory with `RUSTLING_TULIP_CONFIG_DIR`).

## Docs

Start with [docs/architecture.md](docs/architecture.md) for how the pieces fit and what the product does, and [docs/plans/MAIN.md](docs/plans/MAIN.md) for the open work.

## Glossary

- **Main plan**: `docs/plans/MAIN.md`, the one index of open work, one line an item in working order; a landed line is deleted.
- **Wave**: a release-sized bundle of main-plan items that share owning files, so one implementer reads those files once and one review covers the bundle; each wave names its files, review and verification.
- **Subplan**: a `docs/plans/*.md` file holding the design and rulings for open items, linked from the main plan and deleted once its last item lands and its durable text is promoted into `docs/`.
- **Petal**: the approved refreshed look for the native client (graphite ground, tulip-coral accent, status shapes), its settled design in `docs/native-client.md` ("Petal chrome", "Theme and fonts").
- **Native client**: the GPUI + `alacritty_terminal` desktop client under `apps/native`; see `docs/native-client.md`.
- **Parity checklist**: `docs/plans/native-client-parity.md`, every user-visible Tauri feature with its source file (paths on the `tauri-last` tag); the native client reaches parity when it is all ticked.
- **tauri branch**: the maintenance branch holding the earlier Tauri desktop app, cut from the `tauri-last` tag. Tauri hotfixes are made there, daemon fixes are cherry-picked there from `main`, and the Tauri installer is built there; `main` keeps protocol 22 decodable for it.
- **Spike**: throwaway code that proves an approach works, kept outside the main build (`spikes/`) and deleted once its code is ported.
- **E2E tier / smoke tier**: the native client's opt-in test layers above the in-process UI specs. The e2e tier drives the client in-process against a real daemon isolated under `.tmp/`; the smoke tier launches the real exe in a cloaked window. They run through `rt.ps1 native-e2e` and `native-smoke`.
- **Re-ask**: the native client resending a queued in-place spawn, unchanged, when its checkout prompt's turn comes, so the daemon answers with current numbers instead of the stale prompt being shown.
- **Forwarder**: the daemon's per-connection task that streams one session's PTY output to one client; `LoadScrollback` replaces it.
- **P1.1, P4.4b, …**: native client item ids, as phase number and item number (a letter for a split item); the open ones' rulings are in `docs/plans/native-client.md`.
- **HS.1, SA.1, DB.1, SS.1, DF.1, FT.1**: step ids in the subplans `hook-status.md`, `spoken-alerts.md` (SA alerts, DB Dashboard), `subagent-streams.md`, `dispatch-follow.md` and `remote-file-transfer.md` under `docs/plans/`.
- **Session history**: the daemon's record of ended sessions, one `history/<id>.json` per session under the config dir, kept 7 days; see `docs/architecture.md` (Session history and recovery) and `docs/native-client.md` (Session recovery).
- **Unexpected end**: a session whose tracer was lost (killed or crashed) rather than one that exited or was closed; these are pre-ticked for recovery.
- **Status glyph**: the shape-and-colour mark for a session's status in the native client (working arc, asking diamond, waiting ring, idle dot); see `docs/native-client.md`.
- **Unseen turn**: an agent session whose turn ended while this client wasn't showing it focused; it shows the waiting ring until focused.
- **Needs You**: the native client's rail panel listing every session waiting on the user, longest wait first; see `docs/native-client.md`.
- **`status_since`**: the daemon's stamp of a session's last status change, on its snapshot and kept across a reattach; Needs You measures waits from it.
- **Recovery**: respawning a session from its history entry with `claude --resume <conversation id>`, as a Claude session or as a shell that types the command.
- **Folder-only entry**: a history entry with no spawn config (imported from a tracer log, or a standalone session); it is recovered by running claude in its folder with its `--add-dir` set, never by checking out a branch.
- **MA1, MA2, …**: item ids in `docs/plans/mobile-app.md`, the iOS and Android app's phases (distinct from the macOS plan's M0–M4).
- **Relay**: the planned `crates/relay` service on a small VPS that joins a daemon's outbound connection to the phone's, forwarding bytes it cannot read; the pinned TLS runs end to end through it.
- **Shared client core**: the planned Rust crate holding the pinned-TLS connect, host profiles and pairing, used by both the native client's remote mode and the mobile app.
- **Tracer-log import**: the startup pass that rebuilds history entries from `logs/tracer-<id>.log` files for sessions that ended before history existed.
- **Claude provider**: the service a Claude session's `claude` CLI talks to, Anthropic (the default) or DeepSeek, chosen per spawn and kept by every respawn; not an agent.
- **Provider table**: the daemon's built-in, not user-editable list of the environment variables each non-Anthropic provider sets, removes and locks (`crates/daemon/src/agents/providers.rs`).
- **Routing variables**: the `ANTHROPIC_*` and `CLAUDE_*` environment variables that point Claude Code at another endpoint, credential and model; built at each spawn, never stored.
- **Locked key**: a routing variable choosing the endpoint or the credential, which a session's environment rows may not set; a spawn that tries is refused.
- **DK.1**: step ids in `deepseek-sessions.md` (DeepSeek Claude sessions), distinct from dispatch-follow's `DF` steps and `DS` leaf tag.
- **Agent skill pack**: the `rt-*` skills and the `rt-worker` agent under `agent-pack/`, installed into `~/.claude` so an agent can fan work out, review it and merge it.
- **Fan-out**: an agent splitting a task into independent parts, each run by a worker in its own worktree.
- **Worker**: a subagent or rustling-tulip session running one brief in one worktree.
- **Brief**: a worker's instructions: tree root, files, the change, and a proving command.
- **Fan-out ledger**: the parent's event log for one fan-out, in the repo's git common dir.
- **Fix round**: review findings sent back to the worker that made the change.
- **Env reference**: an environment-row value written `${env:NAME}`, resolved from the daemon's environment at spawn; only the reference is stored and echoed.
- **claude-swap / cswap**: the user's multi-account switcher for Claude Code (`github.com/leftos/claude-swap`), whose store and rules the daemon's accounts module shares; see `docs/plans/accounts.md`.
- **Account store**: cswap's folder of saved logins (`~/.claude-swap-backup` on Windows), read and written by both cswap and the daemon.
- **Active account**: the Claude login in `.credentials.json` that every Anthropic Claude session on the machine uses; a switch changes it for all of them at once.
- **Auto-switch**: moving the active account to another one when its binding usage window reaches the threshold, run by the daemon or by `cswap auto`, never both.
- **Binding window**: whichever of an account's usage windows (5-hour, 7-day, per-model) is fullest; its percentage decides auto-switch.
- **Warm-up gate**: auto-switch making no decision until every usable account has been polled since it started.
- **Quarantine**: an account whose refresh token was rejected (`invalid_grant`), skipped until it is logged in again.
- **Claim lease**: a short-lived marker in the account store saying one process is fetching an account's usage, so another does not fetch it too.
- **Connection origin**: whether a client connected on the local port (`Local`) or through the LAN listener or relay (`Remote`); account administration is local only.
- **CA.0, CA.1, …**: step ids in `docs/plans/accounts.md` (Claude accounts), distinct from `agent-cli.md`'s AC steps.
- **Feature marker**: `branch: feat/<name>` on a `docs/plans/MAIN.md` line; every item under it lands on the `feat/<name>` branch instead of `main` (user-level `nextup` §3, "Feature branches").
- **Feature PR**: the draft pull request from a marker's `feat/<name>` into `main`, opened with the marker and merged with `--rebase` by `/ship` once every line under the marker is ticked.
