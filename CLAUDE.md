# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project shape

**Migration in progress:** the Tauri client is being replaced by a native GPUI client (`apps/native`, plan in `docs/plans/native-client.md`). The Tauri app is frozen to bug fixes; new client features go to the native client only (user, 2026-09-23).

`rustling-tulip` is a Tauri desktop client + a long-lived Rust daemon that orchestrates many parallel `claude` CLI sessions across single repos and multi-repo "workspaces". The daemon owns all PTYs and child processes; the Tauri app is just a client. **No code in this repo calls the Anthropic API directly** — the daemon always shells out to the `claude` CLI, which is the stable boundary.

```
crates/protocol/        shared wire types (serde JSON over WS) — the contract between daemon and clients
crates/daemon/          binary = rustling-tulipd: WS server, PTY pool, registry, git/scrollback/orphan logic
crates/tracer/          binary = rt-tracer.exe: per-session ConPTY supervisor that survives daemon restarts
crates/tracer-protocol/ stable ABI between daemon and tracer (additive-only; see docs/tracer-abi.md)
crates/daemon-client/   client-side daemon supervision (ensure-running, handshake, config dir, client identity, stop) shared by the clients
apps/native/            binary = rustling-tulip-native (thin main.rs over a lib, rustling_tulip_native): GPUI + alacritty_terminal desktop client replacing the Tauri app (see docs/plans/native-client.md); tests/ui_*.rs drive RootView::with_transport against a scripted fake daemon
apps/tauri-app/
  src-tauri/            Rust side: spawns the daemon, exposes Tauri commands (file picker, pop-out window)
  src/                  React 19 + xterm.js frontend (Monaco editor for diffs)
tools/e2e/              WebdriverIO end-to-end test suite + fake-claude CLI shim
docs/plan.md            full architecture and phased rollout (Phase 0–6)
docs/plans/*.md         follow-up designs (source-control sidebar, codex support, etc.)
```

## Common commands

PowerShell on Windows is the primary dev environment. `rt.ps1` in the repo root is a convenience wrapper for the most common tasks:

```powershell
# Convenience wrapper (recommended)
.\rt.ps1 build            # cargo build (workspace)
.\rt.ps1 build --release  # cargo build --release
.\rt.ps1 clippy           # strict lint pass (-D warnings)
.\rt.ps1 test             # cargo test (workspace)
.\rt.ps1 restart          # stop running daemon + relaunch
.\rt.ps1 installer        # produce NSIS bundle
.\rt.ps1 installer -Fast  # same, minus LTO + LZMA (dev iteration, not shippable)
.\rt.ps1 native           # build daemon + tracer, then run the native client (optional session id to attach to)
.\rt.ps1 native-e2e       # native client specs against a real daemon isolated under .tmp/ (fake-claude needs node)
.\rt.ps1 native-smoke     # launch the native client exe in a cloaked, never-focused window; check it connects and takes keys
.\rt.ps1 help             # usage summary
```

`installer` writes `target/release/bundle/nsis/rustling-tulip_<version>_x64-setup.exe`.
It stages `rustling-tulipd` and `rt-tracer` into `apps/tauri-app/src-tauri/binaries/`
as `<name>-<target-triple>.exe` and bundles both as Tauri `externalBin` sidecars, so
the installed app is self-contained. The bundle is **unsigned** — see "Things that are
deferred". Staging copies only when the target is older than the source, so a
`target/release/` binary that cargo left stale silently ships stale; if a binary's
timestamp looks wrong, delete it and rebuild rather than trusting "Finished".

Raw cargo / pnpm equivalents when you need them:

```powershell
# Workspace build
cargo build
cargo build --release

# Lint — workspace lints are pedantic + deny on unwrap/panic/etc; CI must be warning-free
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt
cargo deny check          # advisories, licenses, source allowlist (see deny.toml)

# Run a single test
cargo test -p daemon <test_name>
cargo test -p protocol

# Native client UI specs: in-process GPUI (test-support), fake daemon, no real window or input
cargo test -p rustling-tulip-native --test ui_terminal   # also ui_sidebar, ui_tabs, ui_session_actions
# e2e_live.rs and smoke_window.rs are #[ignore]d here; run them through rt.ps1 native-e2e / native-smoke

# Frontend (apps/tauri-app)
cd apps/tauri-app
pnpm install
pnpm dev                  # vite dev server only
pnpm tauri dev            # full desktop app — starts daemon supervisor + vite
pnpm typecheck            # tsc --noEmit
pnpm build                # tsc -b && vite build
```

The Tauri app auto-spawns the daemon on first connect via `daemon_client::ensure_running` (`crates/daemon-client`). The daemon writes `port` + `auth_token` + `pid` + `supported_versions` to `daemon.json` in the config dir below; clients read that to connect. Each client passes `ensure_running` the protocol versions it speaks, and a daemon sharing none of them is retired and replaced. The native client speaks only the current version; the frozen Tauri app is pinned to protocol 22 (`apps/tauri-app/src/types.ts` and `TAURI_PROTOCOL_VERSIONS` in `src-tauri/src/lib.rs`), so `supported` must keep 22 until cutover.

## E2E tests

The suite lives in `tools/e2e/` and uses WebdriverIO + tauri-driver. One-time machine setup:

```powershell
cargo install tauri-driver --locked
cargo install --git https://github.com/chippers/msedgedriver-tool
# msedgedriver-tool extracts msedgedriver.exe into its cwd — run it from a
# scratch dir, then move the driver onto PATH.
cd $env:TEMP; & "$HOME/.cargo/bin/msedgedriver-tool.exe"; Move-Item -Force msedgedriver.exe "$HOME/.cargo/bin/msedgedriver.exe"
```

Running tests:

```powershell
cd tools/e2e
pnpm install              # one-time dep install
pnpm run doctor           # validate tauri-driver + msedgedriver are on PATH, and
                          # that msedgedriver's major matches the installed WebView2
                          # (Edge auto-updates; a stale driver fails every spec).
                          # `run` is required: bare `pnpm doctor` hits pnpm's own
                          # builtin doctor command and prints nothing.
pnpm test                 # run all WebdriverIO specs
pnpm host                 # start interactive test host (accepts JSON commands over stdin)
```

The `fake-claude/` shim (`fake-claude.cmd` + `index.mjs`) replaces the real CLI during tests. It is wired in via the `RUSTLING_TULIP_CLAUDE` environment variable — the daemon path-resolves the CLI binary from that var at spawn time.

## Environment variables

| Variable | Purpose | Default |
|---|---|---|
| `RUSTLING_TULIP_CLAUDE` | Path to `claude` binary | `claude` (PATH lookup) |
| `RUSTLING_TULIP_CODEX` | Path to `codex` binary | `codex` (PATH lookup) |
| `RUSTLING_TULIP_SHELL` | Shell used for plain-shell sessions | auto-detect |
| `RUSTLING_TULIP_CONFIG_DIR` | Config dir override (useful for e2e test isolation) | `%APPDATA%\leftos\rustling-tulip\config\` |
| `RUSTLING_TULIP_WORKTREES_DIR` | Worktrees root override | `%LOCALAPPDATA%\leftos\rustling-tulip\data\worktrees\` |
| `RUSTLING_TULIP_OFFSCREEN_WINDOW` | Native client: open the window cloaked and never activate it (the smoke tier sets it); the Tauri app uses it for its e2e window | unset |

## Where things live on disk

Both sides resolve the config dir via the `directories` crate as `ProjectDirs::from("dev", "leftos", "rustling-tulip").config_dir()`. On Windows that expands to `%APPDATA%\leftos\rustling-tulip\config\` (note the `leftos\` + `\config\` segments — `directories` inserts them, so a plain `%APPDATA%\rustling-tulip\` path is wrong). Layout under that root:

- `state.json` — persisted repos + workspaces + tabs, plus daemon-side host settings (`worktrees_root_override`, `keep_awake`) that must apply with no window open (see `crates/daemon/src/state.rs`).
- `daemon.json` — handshake (port + auth_token + pid + supported_versions); written on daemon start, removed on graceful shutdown.
- `sessions/<id>/meta.json` + `scrollback.bin` — orphan-recovery sidecar and PTY scrollback ring.
- `logs/daemon.log` — daemon tracing output. Rotated on each daemon start: the previous run survives as `daemon.log.old` (see `crates/daemon/src/main.rs::init_tracing`).
- `logs/app.log` — Tauri side log file, written via the `log_message` invoke command (see `apps/tauri-app/src-tauri/src/lib.rs`). Frontend code calls it through `apps/tauri-app/src/utils/logger.ts`. Rotated on each app boot: the previous launch survives as `app.log.old`.
- `logs/native.log` — native client (`apps/native`) tracing output, also mirrored to stderr. Rotated on each launch to `native.log.old`.
- `client-id` / `client-id-native` — per-install client identity (a bare UUID) that the Tauri app and the native client send in `Hello`; tab layouts are keyed by it, so the two clients keep separate layouts.
- `native-ui.json` — native client UI state that stays on this machine: sidebar width, the sidebar's collapsed flag and the collapsed containers, the active tab, the quick-shell folder, the app-level terminal font and colours, the shared list of recent custom colours, per-tab font sizes, the activity rail's view (`activity`), and the source-control panel's pinned repo, collapsed sections and split heights, the diff tabs' include-whitespace toggle (`apps/native/src/sidebar.rs`). Its `unc_hosts` list (network hosts terminal links may open) is edited by hand; the client re-reads it on each click and never writes it. The native client's bundled OFL fonts live in `apps/native/assets/fonts/` (sources and versions in its README), and the large-file hook exempts them.

When debugging spawn/connect/shutdown issues, both `daemon.log` and `app.log` together tell the full story — neither alone is enough.

Worktrees live under a **separate** root resolved via `ProjectDirs::data_local_dir().join("worktrees")` (overridable with `RUSTLING_TULIP_WORKTREES_DIR`). On Windows that's `%LOCALAPPDATA%\leftos\rustling-tulip\data\worktrees\` — machine-local, doesn't roam, and known-writable regardless of where the source repo lives. Per-session worktree paths are `<worktrees-root>/wt.<branch-slug>/<sanitized-anchor>/<rel-to-anchor>`, where the **anchor** is the common path-component prefix of all member-repo *parents* and `<rel-to-anchor>` is each member's offset from that anchor. This layout preserves inter-member relative paths inside a workspace session — if `repo1` references `repo2` as `../repo2` in source space, the same reference resolves inside the worktree pair. Cross-drive workspace members (no common ancestor) fall back to leaf-name placement under the first member's anchor and lose relativity for the cross-drive member only. Path construction lives in `git::workspace_worktree_paths`.

## Architecture invariants

**Single source of truth for the wire protocol.** All daemon/client messages live in `crates/protocol/src/lib.rs` as tagged enums (`#[serde(tag = "type", rename_all = "snake_case")]`). When adding a message, update both directions (`ClientMessage` + `DaemonMessage` if it's a request/response) and the matching match arms in `crates/daemon/src/server.rs` (handler) and `apps/tauri-app/src/api.ts` + `types.ts` (TS mirror — there's no codegen, the TS types are hand-maintained to match the Rust enums).

**When to bump `protocol-version.json`.** *Additive* changes (new variant on a tagged enum, new `#[serde(default)]` field on a struct, new message type) are NOT a protocol bump. The range-based handshake (`SUPPORTED_PROTOCOL_VERSIONS`) + the `InboundClientMessage::Unknown` / `InboundDaemonMessage::Unknown` parse wrappers absorb unknown top-level types. Nested enums that grow over time (`TabLayout`, `RearrangeLayout`, `InjectorStep`, `PresetVariableKind`) each carry a `#[serde(other)] Unknown` unit variant that absorbs unrecognized values *in place* — the containing message keeps decoding. *Breaking* changes — renaming a field, removing a variant, changing semantics — DO require a bump. When bumping, keep the current version and every still-decodable prior version in `supported` (for example, a v18 daemon/client that can still speak v17 should advertise `[18, 17]`, not `[18]`). Only make `supported` a singleton when the new app cannot safely consume the older daemon's runtime messages. In that singleton case, verify `daemon_client::ensure_running` can retire the old healthy daemon through the HTTP `/shutdown` path and spawn the new daemon so tracer-backed sessions reattach instead of leaving the app stuck at `auth_failed`. New nested enums should follow the same `#[serde(other)]` pattern from day one.

**On-disk layout under `%APPDATA%\rustling-tulip\`** (see `crates/daemon/src/paths.rs`):
- `state.json` — repo + workspace registry plus host settings (never sessions)
- `daemon.json` — handshake (port, token, pid, supported protocol versions)
- `sessions/<id>/meta.json` — orphan-recovery sidecar; written at spawn, deleted on graceful stop
- `sessions/<id>/scrollback.bin` — 2 MB ring (trims to 1.5 MB on overflow), replayed via `LoadScrollback` on attach
- `sessions/<id>/scrollback.truncated` — flag file iff the ring overflowed

Sessions are deliberately **not** in `state.json` — they're rebuilt from sidecars on startup so the daemon can survive restarts without a single fragile state blob.

**Orphan recovery + tracer reattach (Phase C.3).** On startup, `main.rs` reads all `meta.json` sidecars and partitions live vs dead via `orphan::is_session_alive` (sysinfo by pid + program-name match). For sidecars with `tracer_pid` + `tracer_pipe` set (post-C.3 spawns), `server::reattach_orphans` connects to the still-running `rt-tracer.exe` over its named pipe and rebuilds a fully-functional `PtyHandle` — IO, resize, and kill all work as if the session had been freshly spawned. Reattach failures route the session to the abandoned bucket (sidebar Resume button, B.2 UX). Pre-C.3 sidecars without tracer fields fall through to the read-only `insert_orphan` path; their underlying children almost never survive a daemon death, so that branch is effectively only for downgrade scenarios.

**Status detection is heuristic for interactive PTY mode** (`pty_state.rs`): regex against known TUI prompts + an output/input byte-volume comparison + a terminal-title-change heartbeat (`osc_title::TitleActivity` — a repainting spinner/elapsed-time title counts as activity) + idle timeout. It's intentionally coarse and version-fragile; the authoritative source for tokens/cost is the `claude` CLI's own per-session jsonl at `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`, tailed independently. Headless mode (`claude --print --output-format stream-json`) uses `crates/daemon/src/headless.rs` for structured events.

**Multi-repo workspace sessions** spawn one `claude` process with `cwd = members[0].worktree_path` and `--add-dir <path>` for every additional member. Worktrees are created/reused per member via `git -C <repo> worktree add` under the shared `<worktrees-root>/wt.<branch-slug>/<sanitized-anchor>/` directory (see "Where things live on disk" — the layout preserves inter-member relative paths within a workspace). The "reuse-or-create" policy is encoded in `workspace.rs`. VS Code `.code-workspace` files are auto-detected when adding a repo and surfaced as a `VscodeWorkspaceSuggestion` event.

**Tracer-backed PTY sessions (Phase C.3).** Every interactive and plain-shell PTY child is spawned under a per-session `rt-tracer.exe` supervisor process; the daemon talks to it over a named pipe at `\\.\pipe\rt-tracer-<session-id>`. The tracer owns the master ConPTY handle and survives daemon restarts — when the daemon dies the tracer keeps draining child output to its internal ring buffer (4 MB cap, oldest-bytes-drop on overflow), and a freshly-started daemon reattaches via `tracer_client::reattach` and replays the ring to catch up. Spawn site: `crates/daemon/src/tracer_client.rs::spawn`. Tracer ABI: `crates/tracer-protocol/src/lib.rs` — frozen surface; additive changes (new fields with `#[serde(default)]`, new variants with `#[serde(other)] Unknown`) are not a bump. Headless (`claude --print`) does NOT go through the tracer — it's piped stdio and lives in `crates/daemon/src/headless.rs`. The tracer binary must be present next to `rustling-tulipd.exe`: `cargo build` emits both into `target/<profile>/` for the dev flow, and the installer ships both as Tauri `externalBin` sidecars (see "Common commands").

**Live git watcher** (`crates/daemon/src/git_watch.rs`). Each registered repo gets a recursive `notify`-based watcher with a 750 ms debounce. Inside the debouncer callback, `classify_event` filters paths against a hand-maintained allowlist: only `.git/index`, `.git/HEAD`, `.git/refs/**`, a handful of in-progress operation markers, and any non-excluded working-tree path can wake the refresher. `.git/objects/`, `.git/logs/HEAD`, `FETCH_HEAD`, lock files, and well-known build/cache dirs (`target/`, `node_modules/`, `dist/`, `.next/`, `.venv/`, `__pycache__/`, …) are ignored so a `cargo build` or `pnpm install` doesn't spin up `git status` forever. The refresher tracks two flags — `status` and `stash` — and only invokes `git stash list` when a stash ref actually changed. The refresher parks while `Hub.client_count` is 0 (an RAII `ClientCountGuard` in `client_session` maintains the count); the next reconnect triggers one catch-up `repo_status` + `stash_list` before resuming event-driven refresh. **The pop-out window** is the same React bundle reloaded with `?session=<id>` — `App.tsx` branches on the query param to render `SessionWindow` instead of the full sidebar layout. The daemon already accepts multiple WS clients, so each window opens its own connection.

## Wire-protocol gotchas

- All binary payloads (PTY input/output, scrollback) cross the wire as `data_b64`. Don't add raw-bytes fields.
- The `Hello` message must be the first thing a client sends after WS upgrade. New clients send both `protocol_version` (scalar back-compat) and `protocol_versions: Vec<u32>`. The daemon picks the highest mutually supported version from its `SUPPORTED_PROTOCOL_VERSIONS` const and echoes it in `Welcome.protocol_version`. An empty intersection or token mismatch closes the connection.
- Unknown message types (forward-compat path) hit `InboundClientMessage::Unknown` in the daemon (`crates/protocol/src/lib.rs`) and a default arm in `App.tsx::handleMessage`. Both log + drop without crashing the connection.
- `SessionSnapshot` is the canonical session shape — daemon emits `Sessions` (list), `SessionUpdated` (single), `SessionRemoved` (id only). Don't add ad-hoc session-shaped messages elsewhere.
- A client that needs to know which reply answers its request sets the optional `request_id` on `SpawnSession` / `DuplicateSession` / `LoadScrollback` / `SetSessionAppearance`. The daemon echoes it only on the reply it sends to that requester (`SessionUpdated` or `Scrollback`, `CheckoutConfirmRequired` when an in-place spawn needs a checkout confirm, or `Error` / `ActionFailed` on failure). The git reads `RepoStatus` / `ListStashes` / `ListCommits` / `GetCommit` / `GetRemoteUrl` take one too; it comes back only on the `Error` that answers a failed read. Broadcasts never carry it. A spawn's, a duplicate's and an appearance change's `SessionUpdated` echo travels on the requester's ordered session-event stream (`SessionEvent::Updated` carries its origin connection), so the requester gets one copy, carrying the id, and it never overtakes an older broadcast. A connection whose session-event stream lags is sent a fresh `Sessions` list, then its spawn replies again with their `request_id`. A `Scrollback` with `forwarder_restarted: true` means the old forwarder has stopped and the new one starts after this reply, so output a client held back before it is already in the history.

## Style and lints

Workspace `Cargo.toml` enforces clippy pedantic + denies on `unwrap_used`, `panic`, `dbg_macro`, `todo`, `print_*`, `exit`, etc. Use `tracing::{error,warn,info,debug}` instead of `println!`. Use `expect_used = "warn"` — prefer `?` and `anyhow::Context`. The two existing `.expect()` allowances live in `apps/tauri-app/src-tauri/src/lib.rs` for Tauri builder errors with explicit `#[expect(... reason = "...")]`.

Rust edition 2024 on the `stable` channel (`rust-toolchain.toml` pins the channel, not a version; let-chains and other current features are in use). Profile `release` uses `lto = "thin"`, `codegen-units = 1`, `strip = true`.

Frontend uses TypeScript strict + React 19 + xterm.js (`@xterm/xterm` + `@xterm/addon-fit`). PTY output is high-volume — keep it out of React state, use a `Map<sessionId, Set<listener>>` ref pattern (see `App.tsx`). Monaco editor (`monaco-editor`) is used for diff viewing in the source-control sidebar.

## Plan files

`docs/plan.md` is the canonical plan with checklist-tracked phases. When completing a planned task, tick the checkbox. New designs go in `docs/plans/*.md` with `- [x]` / `- [ ]` checklists for actionable items. Completed designs move to `docs/plans/completed/` (git-mv as part of the shipping commit).

## Things that are deferred / not implemented

Don't go looking for these — they're explicitly out of scope until the corresponding plan item is unchecked:

- **Auto-update** for the desktop app (`tauri-plugin-updater`) — deferred until a signed release pipeline exists.
- **Code signing / notarization** of the installer and the binaries inside it — no signing cert, so the NSIS bundle `.\rt.ps1 installer` produces is unsigned and trips SmartScreen. This is the only remaining blocker for a release pipeline; bundling itself works (see "Common commands").
- **Sub-agent / Task-tool interception**, **multi-machine attach**, **cloud sync**, **mobile app** — explicit non-goals.
