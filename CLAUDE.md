# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project shape

`rustling-tulip` is a Tauri desktop client + a long-lived Rust daemon that orchestrates many parallel `claude` CLI sessions across single repos and multi-repo "workspaces". The daemon owns all PTYs and child processes; the Tauri app is just a client. **No code in this repo calls the Anthropic API directly** — the daemon always shells out to the `claude` CLI, which is the stable boundary.

```
crates/protocol/        shared wire types (serde JSON over WS) — the contract between daemon and clients
crates/daemon/          binary = rustling-tulipd: WS server, PTY pool, registry, git/scrollback/orphan logic
apps/tauri-app/
  src-tauri/            Rust side: spawns the daemon, exposes Tauri commands (file picker, pop-out window)
  src/                  React 19 + xterm.js frontend
docs/plan.md            full architecture and phased rollout (Phase 0–6)
docs/plans/*.md         follow-up designs (source-control sidebar, codex support, etc.)
```

## Common commands

PowerShell on Windows is the primary dev environment.

```powershell
# Workspace build (daemon + protocol + tauri rust side)
cargo build
cargo build --release

# Lint — workspace lints are pedantic + deny on unwrap/panic/etc; CI must be warning-free
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt
cargo deny check          # advisories, licenses, source allowlist (see deny.toml)

# Run a single test
cargo test -p daemon <test_name>
cargo test -p protocol

# Frontend (apps/tauri-app)
cd apps/tauri-app
pnpm install
pnpm dev                  # vite dev server only
pnpm tauri dev            # full desktop app — starts daemon supervisor + vite
pnpm typecheck            # tsc --noEmit
pnpm build                # tsc -b && vite build
```

The Tauri app auto-spawns the daemon on first connect via `daemon_supervisor::ensure_running`. The daemon writes `port` + `auth_token` + `pid` to `daemon.json` in the config dir below; clients read that to connect.

## Where things live on disk

Both sides resolve the config dir via the `directories` crate as `ProjectDirs::from("dev", "leftos", "rustling-tulip").config_dir()`. On Windows that expands to `%APPDATA%\leftos\rustling-tulip\config\` (note the `leftos\` + `\config\` segments — `directories` inserts them, so a plain `%APPDATA%\rustling-tulip\` path is wrong). Layout under that root:

- `state.json` — persisted repos + workspaces + tabs (see `crates/daemon/src/state.rs`).
- `daemon.json` — handshake (port + auth_token + pid); written on daemon start, removed on graceful shutdown.
- `sessions/<id>/meta.json` + `scrollback.bin` — orphan-recovery sidecar and PTY scrollback ring.
- `logs/daemon.log` — daemon tracing output. Truncated on each daemon start (see `crates/daemon/src/main.rs::init_tracing`).
- `logs/app.log` — Tauri side log file, written via the `log_message` invoke command (see `apps/tauri-app/src-tauri/src/lib.rs`). Frontend code calls it through `apps/tauri-app/src/utils/logger.ts`. Truncated on each app boot.

When debugging spawn/connect/shutdown issues, both `daemon.log` and `app.log` together tell the full story — neither alone is enough.

## Architecture invariants

**Single source of truth for the wire protocol.** All daemon/client messages live in `crates/protocol/src/lib.rs` as tagged enums (`#[serde(tag = "type", rename_all = "snake_case")]`). When adding a message, update both directions (`ClientMessage` + `DaemonMessage` if it's a request/response) and the matching match arms in `crates/daemon/src/server.rs` (handler) and `apps/tauri-app/src/api.ts` + `types.ts` (TS mirror — there's no codegen, the TS types are hand-maintained to match the Rust enums).

**When to bump `protocol-version.json`.** *Additive* changes (new variant on a tagged enum, new `#[serde(default)]` field on a struct, new message type) are NOT a protocol bump. The range-based handshake (`SUPPORTED_PROTOCOL_VERSIONS`) + the `InboundClientMessage::Unknown` / `InboundDaemonMessage::Unknown` parse wrappers absorb unknown-type messages without crashing the connection. *Breaking* changes — renaming a field, removing a variant, changing semantics of an existing field — DO require a bump. When the daemon picks up a new version, also append it to `supported` so older clients can still negotiate. Caveat: introducing a new variant in a **nested** enum (e.g. `TabLayout`, `RearrangeLayout`) drops the entire containing message to the top-level `Unknown` handler on older peers — fine for forward-compat, but lossy. If that loss matters for the feature, plan to add a per-enum `Unknown(serde_json::Value)` fallback in the same iter (deferred from Phase A.2).

**On-disk layout under `%APPDATA%\rustling-tulip\`** (see `crates/daemon/src/paths.rs`):
- `state.json` — repo + workspace registry only (never sessions)
- `daemon.json` — handshake (port, token, pid)
- `sessions/<id>/meta.json` — orphan-recovery sidecar; written at spawn, deleted on graceful stop
- `sessions/<id>/scrollback.bin` — 2 MB ring (trims to 1.5 MB on overflow), replayed via `LoadScrollback` on attach
- `sessions/<id>/scrollback.truncated` — flag file iff the ring overflowed

Sessions are deliberately **not** in `state.json` — they're rebuilt from sidecars on startup so the daemon can survive restarts without a single fragile state blob.

**Orphan recovery (Phase 5).** On startup, `main.rs` reads all `meta.json` sidecars, partitions live vs dead via `is_claude_alive` (sysinfo by pid), and inserts survivors into the registry with `pty = None / headless = None`. The frontend renders these in read-only mode using the `is_orphan` flag on `SessionSnapshot` (true when status isn't terminal but no live stdio handle exists). Don't add code that assumes every active session has a live PTY — always check the handle is `Some`.

**Status detection is heuristic for interactive PTY mode** (`pty_state.rs`): regex against known TUI prompts + idle timeout. It's intentionally coarse and version-fragile; the authoritative source for tokens/cost is the `claude` CLI's own per-session jsonl at `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`, tailed independently. Headless mode (`claude --print --output-format stream-json`) uses `crates/daemon/src/headless.rs` for structured events.

**Multi-repo workspace sessions** spawn one `claude` process with `cwd = members[0].worktree_path` and `--add-dir <path>` for every additional member. Worktrees are created/reused per member via `git -C <repo> worktree add`. The "reuse-or-create" policy is encoded in `workspace.rs`. VS Code `.code-workspace` files are auto-detected when adding a repo and surfaced as a `VscodeWorkspaceSuggestion` event.

**The pop-out window** is the same React bundle reloaded with `?session=<id>` — `App.tsx` branches on the query param to render `SessionWindow` instead of the full sidebar layout. The daemon already accepts multiple WS clients, so each window opens its own connection.

## Wire-protocol gotchas

- All binary payloads (PTY input/output, scrollback) cross the wire as `data_b64`. Don't add raw-bytes fields.
- The `Hello` message must be the first thing a client sends after WS upgrade. New clients send both `protocol_version` (scalar back-compat) and `protocol_versions: Vec<u32>`. The daemon picks the highest mutually supported version from its `SUPPORTED_PROTOCOL_VERSIONS` const and echoes it in `Welcome.protocol_version`. An empty intersection or token mismatch closes the connection.
- Unknown message types (forward-compat path) hit `InboundClientMessage::Unknown` in the daemon (`crates/protocol/src/lib.rs`) and a default arm in `App.tsx::handleMessage`. Both log + drop without crashing the connection.
- `SessionSnapshot` is the canonical session shape — daemon emits `Sessions` (list), `SessionUpdated` (single), `SessionRemoved` (id only). Don't add ad-hoc session-shaped messages elsewhere.

## Style and lints

Workspace `Cargo.toml` enforces clippy pedantic + denies on `unwrap_used`, `panic`, `dbg_macro`, `todo`, `print_*`, `exit`, etc. Use `tracing::{error,warn,info,debug}` instead of `println!`. Use `expect_used = "warn"` — prefer `?` and `anyhow::Context`. The two existing `.expect()` allowances live in `apps/tauri-app/src-tauri/src/lib.rs` for Tauri builder errors with explicit `#[expect(... reason = "...")]`.

Frontend uses TypeScript strict + React 19 + xterm.js (`@xterm/xterm` + `@xterm/addon-fit`). PTY output is high-volume — keep it out of React state, use a `Map<sessionId, Set<listener>>` ref pattern (see `App.tsx`).

## Plan files

`docs/plan.md` is the canonical plan with checklist-tracked phases. When completing a planned task, tick the checkbox. New designs go in `docs/plans/*.md` with `- [x]` / `- [ ]` checklists for actionable items.

## Things that are deferred / not implemented

Don't go looking for these — they're explicitly out of scope until the corresponding plan item is unchecked:

- **Auto-update** for the desktop app (`tauri-plugin-updater`) — deferred until a signed release pipeline exists.
- **Stage/unstage from the git panel** — the panel is read-only; `StageFiles` is in the protocol but not wired up.
- **Live `.git` watcher** — git views re-fetch on user action only.
- **Sub-agent / Task-tool interception**, **multi-machine attach**, **cloud sync**, **mobile app** — explicit non-goals.
