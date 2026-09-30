# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project shape

**The client is the native GPUI app** (`apps/native`, design in `docs/native-client.md`, open work in `docs/plans/MAIN.md`). The earlier Tauri app is not on `main`: it lives on the `tauri` maintenance branch, cut from the `tauri-last` tag. Tauri hotfixes are made there, daemon fixes land on `main` and are cherry-picked there, and the Tauri installer is built there.

`rustling-tulip` is a native desktop client + a long-lived Rust daemon that orchestrates many parallel `claude` CLI sessions across single repos and multi-repo "workspaces". The daemon owns all PTYs and child processes; the client is just a client. **No code in this repo calls the Anthropic API directly** — the daemon always shells out to the `claude` CLI, which is the stable boundary.

```
crates/protocol/        shared wire types (serde JSON over WS) — the contract between daemon and clients
crates/daemon/          binary = rustling-tulipd: WS server, PTY pool, registry, git/scrollback/orphan logic
crates/tracer/          binary = rt-tracer.exe: per-session ConPTY supervisor that survives daemon restarts
crates/tracer-protocol/ stable ABI between daemon and tracer (additive-only; see docs/tracer-abi.md)
crates/daemon-client/   client-side daemon supervision (ensure-running, handshake, config dir, client identity, stop) shared by the clients
apps/native/            binary = rustling-tulip-native (thin main.rs over a lib, rustling_tulip_native): GPUI + alacritty_terminal desktop client (see docs/native-client.md); tests/ui_*.rs drive RootView::with_transport against a scripted fake daemon
tools/e2e/fake-claude/  fake-claude CLI shim used by the native e2e tier
docs/architecture.md    components, what the product does, and a task index: which files to read, in order, for each kind of change
docs/native-client.md   the native client's settled design decisions
docs/plans/MAIN.md      the main plan: every open item, in working order, grouped into waves
docs/plans/*.md         subplans: designs and rulings for open items, linked from MAIN.md
```

## Common commands

PowerShell on Windows is the primary dev environment. `rt.ps1` in the repo root is a convenience wrapper for the most common tasks:

```powershell
# Convenience wrapper (recommended)
.\rt.ps1                  # build daemon + tracer, then run the native client (same as `launch` / `native`)
.\rt.ps1 build            # build only: daemon, tracer and native client, then sweep target/ output older than 14 days (cargo-sweep)
.\rt.ps1 build -Release   # same, release profile (-Release also applies to launch, restart, native, native-e2e, native-smoke, native-shot)
.\rt.ps1 setup            # install/check Windows build prerequisites via winget (Git, Node.js, Rust, C++ Build Tools), plus cargo-sweep
.\rt.ps1 stop             # kill any running daemon and tracers; remove the stale handshake
.\rt.ps1 restart          # build daemon + tracer, stop the daemon (sessions survive in their tracers), run the native client
.\rt.ps1 test             # cargo test (workspace)
.\rt.ps1 clippy           # strict lint pass (-D warnings)
.\rt.ps1 fmt              # cargo fmt --all
.\rt.ps1 clean            # cargo clean
.\rt.ps1 native           # build daemon + tracer, then run the native client (extra args: a session id to focus)
.\rt.ps1 native-e2e       # native client specs against a real daemon isolated under .tmp/ (fake-claude needs node)
.\rt.ps1 native-smoke     # launch the native client exe in a cloaked, never-focused window; check it connects and takes keys
.\rt.ps1 native-shot main # PNG of the client window over a fake daemon (views: main, source-control, diff, settings, spawn) in .tmp\shots\
.\rt.ps1 help             # usage summary
```

Raw cargo equivalents when you need them:

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
# e2e_live.rs, e2e_recover.rs and smoke_window.rs are #[ignore]d here; run them through rt.ps1 native-e2e / native-smoke
```

The native client auto-spawns the daemon on first connect via `daemon_client::ensure_running` (`crates/daemon-client`), passing the protocol versions it speaks. The daemon writes `port` + `auth_token` + `pid` + `supported_versions` to `daemon.json` in the config dir below; clients read that to connect. A daemon sharing none of the client's versions is retired and replaced. The native client speaks only the current version; the daemon keeps protocol 22 in `supported` for the installed Tauri app (pinned to 22), and the `v22_compat` tests in `crates/protocol` (`cargo test -p protocol v22_compat`) fail if a change stops 22 decoding.

## E2E tests

The native client has three test tiers:

- **UI specs** — `cargo test -p rustling-tulip-native --test ui_*` (`ui_terminal`, `ui_sidebar`, `ui_tabs`, `ui_session_actions`, …): in-process GPUI against a scripted fake daemon; no real window, input or daemon.
- **Live e2e** — `.\rt.ps1 native-e2e`: builds daemon + tracer and runs `tests/e2e_live.rs` and `tests/e2e_recover.rs` against a real daemon isolated under `.tmp/`. Needs `node` for the fake-claude shim.
- **OS smoke** — `.\rt.ps1 native-smoke`: launches the native client exe in a cloaked window that never takes focus (`tests/smoke_window.rs`) and checks it connects and that posted keys reach the shell.

To look at a UI change, run `.\rt.ps1 native-shot <view>` (`main`, `source-control`, `diff`, `settings`, `spawn`): it writes a PNG of the real client window over a scripted fake daemon (`apps/native/examples/shot.rs`) to `.tmp\shots\` and prints its path; Read the PNG, and show the user before and after shots for any visual choice.

The `tools/e2e/fake-claude/` shim (`fake-claude.cmd` + `index.mjs`) replaces the real CLI in the e2e tier. It is wired in via the `RUSTLING_TULIP_CLAUDE` environment variable — the daemon path-resolves the CLI binary from that var at spawn time.

## Environment variables

| Variable | Purpose | Default |
|---|---|---|
| `RUSTLING_TULIP_CLAUDE` | Path to `claude` binary | `claude` (PATH lookup) |
| `RUSTLING_TULIP_CODEX` | Path to `codex` binary | `codex` (PATH lookup) |
| `RUSTLING_TULIP_SHELL` | Shell used for plain-shell sessions | auto-detect |
| `RUSTLING_TULIP_CONFIG_DIR` | Config dir override (useful for e2e test isolation) | `%APPDATA%\leftos\rustling-tulip\config\` |
| `RUSTLING_TULIP_WORKTREES_DIR` | Worktrees root override | `%LOCALAPPDATA%\leftos\rustling-tulip\data\worktrees\` |
| `RUSTLING_TULIP_OFFSCREEN_WINDOW` | Native client: open the window cloaked and never activate it (the smoke tier sets it) | unset |

## Where things live on disk

Both sides resolve the config dir via the `directories` crate as `ProjectDirs::from("dev", "leftos", "rustling-tulip").config_dir()`. On Windows that expands to `%APPDATA%\leftos\rustling-tulip\config\` (note the `leftos\` + `\config\` segments — `directories` inserts them, so a plain `%APPDATA%\rustling-tulip\` path is wrong). Layout under that root:

- `state.json` — persisted repos + workspaces + tabs, plus daemon-side host settings (`worktrees_root_override`, `keep_awake`) that must apply with no window open (see `crates/daemon/src/state.rs`).
- `daemon.json` — handshake (port + auth_token + pid + supported_versions); written on daemon start, removed on graceful shutdown.
- `sessions/<id>/meta.json` + `scrollback.bin` — orphan-recovery sidecar and PTY scrollback ring.
- `history/<id>.json` — session history: one entry per ended session (how it ended, its spawn config and folders, its Claude conversation id), written on every end path, backfilled at startup from `logs/tracer-*.log`, pruned after 7 days (`crates/daemon/src/history.rs`). The recover dialog lists these and `RecoverSessions` respawns them with `claude --resume`.
- `codex-contested.json` — Codex rollout ids found while two Codex sessions waited in one folder, which no session may claim as its conversation (`crates/daemon/src/codex_rollout.rs`); entries drop after 7 days.
- `daemon.lock` — the single-instance lock; a second daemon on the same config dir exits without touching anything.
- `logs/daemon.log` — daemon tracing output. Rotated on each daemon start: the previous run survives as `daemon.log.old` (see `crates/daemon/src/main.rs::init_tracing`).
- `logs/autostart.log` — the login launch's short log: `rustling-tulipd --detach` (what the HKCU `Run` entry runs) copies itself into the binary cache, starts that copy and exits (`crates/daemon/src/detach.rs`). Truncated on each such launch.
- `logs/app.log` — the installed Tauri app's log file (its source is on the `tauri` branch). Rotated on each app boot: the previous launch survives as `app.log.old`.
- `logs/native.log` — native client (`apps/native`) tracing output, also mirrored to stderr. Rotated on each launch to `native.log.old`.
- `client-id` / `client-id-native` — per-install client identity (a bare UUID) that the Tauri app (`client-id`) and the native client (`client-id-native`) send in `Hello`; tab layouts are keyed by it, so the two clients keep separate layouts.
- `native-ui.json` — native client UI state that stays on this machine: sidebar width, the sidebar's collapsed flag and the collapsed containers, the active tab, the quick-shell folder, the app-level terminal font and colours, the shared list of recent custom colours, per-tab font sizes, the activity rail's view (`activity`), and the source-control panel's pinned repo, collapsed sections and split heights, the diff tabs' include-whitespace and highlight toggles, and the Settings modal's General (the sidebar's leaf density among them, `general.leaf_density`), Notifications and App title choices (`general`, `notifications`, `title`) (`apps/native/src/sidebar.rs`). Its `unc_hosts` list (network hosts terminal links may open) is edited by hand; the client re-reads it on each click and never writes it. The native client's bundled OFL fonts live in `apps/native/assets/fonts/` (sources and versions in its README), and the large-file hook exempts them.

When debugging spawn/connect/shutdown issues, both `daemon.log` and the client's log (`native.log`, or `app.log` for the Tauri app) together tell the full story — neither alone is enough.

Worktrees live under a **separate** root resolved via `ProjectDirs::data_local_dir().join("worktrees")` (overridable with `RUSTLING_TULIP_WORKTREES_DIR`). On Windows that's `%LOCALAPPDATA%\leftos\rustling-tulip\data\worktrees\` — machine-local, doesn't roam, and known-writable regardless of where the source repo lives. Per-session worktree paths are `<worktrees-root>/wt.<branch-slug>/<repo-slug>` for a single repo and `<worktrees-root>/wt.<branch-slug>/<workspace-slug>/<offset>` for a workspace member, where the **anchor** is the common path-component prefix of the member repos' *parents* on `members[0]`'s drive and `<offset>` is each member's path below it. This preserves inter-member relative paths inside a workspace session — if `repo1` references `repo2` as `../repo2` in source space, the same reference resolves inside the worktree set. Members on another drive are anchored per drive under `<workspace-slug>/<drive anchor>/<offset>`; that folder's first segment takes a `-2`, `-3`, … suffix when the plain name would equal a primary-drive member's path or contain one (`…\ws\Y-2\b`). Slugs come from `git::name_slug`; repos and workspaces share one slug namespace, and saving a name whose slug is taken is refused. A group folder has a `<slug>.rt-group` marker beside it (`{"kind","name"}`), which Manage worktrees reads to list each group separately; a group's members are its linked worktrees, a member nested inside another included (a submodule or plain clone inside one is not listed), and a workspace marker makes a one-member group launch as that workspace. A spawn reuses the worktree git reports as holding the branch (any layout, so older `wt.<branch>/<sanitized-anchor>/…` worktrees are still reused) and refuses a folder at its path that isn't one of the repo's worktrees. Path construction lives in `git::workspace_worktree_paths` / `git::single_worktree_path`.

## Architecture invariants

**Single source of truth for the wire protocol.** All daemon/client messages live in `crates/protocol/src/lib.rs` as tagged enums (`#[serde(tag = "type", rename_all = "snake_case")]`). When adding a message, update both directions (`ClientMessage` + `DaemonMessage` if it's a request/response) and the matching match arms in `crates/daemon/src/server.rs` (handler) and the native client's handling (`apps/native/src/net.rs`, `on_daemon_message`). Changes stay additive and keep protocol 22 decodable while the installed Tauri app is in use (`cargo test -p protocol v22_compat`).

**When to bump `protocol-version.json`.** *Additive* changes (new variant on a tagged enum, new `#[serde(default)]` field on a struct, new message type) are NOT a protocol bump. The range-based handshake (`SUPPORTED_PROTOCOL_VERSIONS`) + the `InboundClientMessage::Unknown` / `InboundDaemonMessage::Unknown` parse wrappers absorb unknown top-level types. Nested enums that grow over time (`TabLayout`, `RearrangeLayout`, `InjectorStep`, `PresetVariableKind`) each carry a `#[serde(other)] Unknown` unit variant that absorbs unrecognized values *in place* — the containing message keeps decoding. *Breaking* changes — renaming a field, removing a variant, changing semantics — DO require a bump. When bumping, keep the current version and every still-decodable prior version in `supported` (for example, a v18 daemon/client that can still speak v17 should advertise `[18, 17]`, not `[18]`). Only make `supported` a singleton when the new app cannot safely consume the older daemon's runtime messages. In that singleton case, verify `daemon_client::ensure_running` can retire the old healthy daemon through the HTTP `/shutdown` path and spawn the new daemon so tracer-backed sessions reattach instead of leaving the app stuck at `auth_failed`. New nested enums should follow the same `#[serde(other)]` pattern from day one.

**On-disk layout under `%APPDATA%\rustling-tulip\`** (see `crates/daemon/src/paths.rs`):
- `state.json` — repo + workspace registry plus host settings (never sessions)
- `daemon.json` — handshake (port, token, pid, supported protocol versions)
- `sessions/<id>/meta.json` — orphan-recovery sidecar; written at spawn, deleted on graceful stop
- `sessions/<id>/scrollback.bin` — 2 MB ring (trims to 1.5 MB on overflow), replayed via `LoadScrollback` on attach
- `sessions/<id>/scrollback.truncated` — flag file iff the ring overflowed

Sessions are deliberately **not** in `state.json` — they're rebuilt from sidecars on startup so the daemon can survive restarts without a single fragile state blob.

**Orphan recovery + tracer reattach (Phase C.3).** On startup, `main.rs` reads all `meta.json` sidecars and partitions live vs dead via `orphan::is_session_alive` (sysinfo by pid + program-name match). For sidecars with `tracer_pid` + `tracer_pipe` set (post-C.3 spawns), `server::reattach_orphans` connects to the still-running `rt-tracer.exe` over its named pipe and rebuilds a fully-functional `PtyHandle` — IO, resize, and kill all work as if the session had been freshly spawned. The sidecar also keeps the session's last `status` and `status_since`, rewritten on every status change, so a reattached session comes back `AwaitingInput` with its old stamp while the replayed prompt is on screen (any other status comes back `Idle`, restamped once the replay settles), and its status watcher starts from that state. Every sidecar write goes through `SessionRegistry::sync_sidecar`, which writes the record's mirrored fields under one lock. Reattach failures route the session to the abandoned bucket (sidebar Resume button, B.2 UX). Pre-C.3 sidecars without tracer fields fall through to the read-only `insert_orphan` path; their underlying children almost never survive a daemon death, so that branch is effectively only for downgrade scenarios.

**Status detection is heuristic for interactive PTY mode** (`pty_state.rs`): regex against known TUI prompts + an output/input byte-volume comparison + a terminal-title-change heartbeat (`osc_title::TitleActivity` — a repainting spinner/elapsed-time title counts as activity) + idle timeout. It's intentionally coarse and version-fragile; the authoritative source for tokens/cost is the `claude` CLI's own per-session jsonl at `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`, tailed independently. Headless mode (`claude --print --output-format stream-json`) uses `crates/daemon/src/headless.rs` for structured events.

**Multi-repo workspace sessions** spawn one `claude` process with `cwd = members[0].worktree_path` and `--add-dir <path>` for every additional member. Worktrees are created/reused per member via `git -C <repo> worktree add` under the shared `<worktrees-root>/wt.<branch-slug>/<workspace-slug>/` directory (see "Where things live on disk" — the layout preserves inter-member relative paths within a workspace). The "reuse-or-create" policy is encoded in `workspace.rs`. VS Code `.code-workspace` files are auto-detected when adding a repo and surfaced as a `VscodeWorkspaceSuggestion` event.

**Tracer-backed PTY sessions (Phase C.3).** Every interactive and plain-shell PTY child is spawned under a per-session `rt-tracer.exe` supervisor process; the daemon talks to it over a named pipe at `\\.\pipe\rt-tracer-<session-id>`. The tracer owns the master ConPTY handle and survives daemon restarts — when the daemon dies the tracer keeps draining child output to its internal ring buffer (4 MB cap, oldest-bytes-drop on overflow), and a freshly-started daemon reattaches via `tracer_client::reattach` and replays the ring to catch up. Spawn site: `crates/daemon/src/tracer_client.rs::spawn`. Tracer ABI: `crates/tracer-protocol/src/lib.rs` — frozen surface; additive changes (new fields with `#[serde(default)]`, new variants with `#[serde(other)] Unknown`) are not a bump. Headless (`claude --print`) does NOT go through the tracer — it's piped stdio and lives in `crates/daemon/src/headless.rs`. The tracer binary must be present next to `rustling-tulipd.exe`: `cargo build` emits both into `target/<profile>/`.

**Live git watcher** (`crates/daemon/src/git_watch.rs`). Each registered repo gets a recursive `notify`-based watcher with a 750 ms debounce. Inside the debouncer callback, `classify_event` filters paths against a hand-maintained allowlist: only `.git/index`, `.git/HEAD`, `.git/refs/**`, a handful of in-progress operation markers, and any non-excluded working-tree path can wake the refresher. `.git/objects/`, `.git/logs/HEAD`, `FETCH_HEAD`, lock files, and well-known build/cache dirs (`target/`, `node_modules/`, `dist/`, `.next/`, `.venv/`, `__pycache__/`, …) are ignored so a `cargo build` or `pnpm install` doesn't spin up `git status` forever. The refresher tracks two flags — `status` and `stash` — and only invokes `git stash list` when a stash ref actually changed. The refresher parks while `Hub.client_count` is 0 (an RAII `ClientCountGuard` in `client_session` maintains the count); the next reconnect triggers one catch-up `repo_status` + `stash_list` before resuming event-driven refresh. The daemon accepts multiple WS clients, so each client window opens its own connection.

## Wire-protocol gotchas

- All binary payloads (PTY input/output, scrollback) cross the wire as `data_b64`. Don't add raw-bytes fields.
- The `Hello` message must be the first thing a client sends after WS upgrade. New clients send both `protocol_version` (scalar back-compat) and `protocol_versions: Vec<u32>`. The daemon picks the highest mutually supported version from its `SUPPORTED_PROTOCOL_VERSIONS` const and echoes it in `Welcome.protocol_version`. An empty intersection or token mismatch closes the connection.
- Unknown message types (forward-compat path) hit `InboundClientMessage::Unknown` in the daemon and `InboundDaemonMessage::Unknown` in the native client (`crates/protocol/src/lib.rs`); the native client's `apps/native/src/net.rs` logs a `warn!` with the type tag and keeps reading (a frame that fails to decode at all is logged as an `error!`). Neither side closes the connection.
- `SessionSnapshot` is the canonical session shape — daemon emits `Sessions` (list), `SessionUpdated` (single), `SessionRemoved` (id only). Don't add ad-hoc session-shaped messages elsewhere.
- A client that needs to know which reply answers its request sets the optional `request_id` on `SpawnSession` / `DuplicateSession` / `LoadScrollback` / `SetSessionAppearance`. The daemon echoes it only on the reply it sends to that requester (`SessionUpdated` or `Scrollback`, `CheckoutConfirmRequired` when an in-place spawn needs a checkout confirm, or `Error` / `ActionFailed` on failure). The git reads `RepoStatus` / `ListStashes` / `ListCommits` / `GetCommit` / `GetRemoteUrl` take one too; it comes back only on the `Error` that answers a failed read. The spawn previews `PreviewSpawn` / `PreviewWorkspaceSpawn` take one as well; it comes back on their `SpawnPreview` / `WorkspaceSpawnPreview` reply, or on the `Error` when the preview fails. Broadcasts never carry it. A spawn's, a duplicate's and an appearance change's `SessionUpdated` echo travels on the requester's ordered session-event stream (`SessionEvent::Updated` carries its origin connection), so the requester gets one copy, carrying the id, and it never overtakes an older broadcast. A connection whose session-event stream lags is sent a fresh `Sessions` list, then its spawn replies again with their `request_id`. A `Scrollback` with `forwarder_restarted: true` means the old forwarder has stopped and the new one starts after this reply, so output a client held back before it is already in the history.

## Style and lints

Workspace `Cargo.toml` enforces clippy pedantic + denies on `unwrap_used`, `panic`, `dbg_macro`, `todo`, `print_*`, `exit`, etc. Use `tracing::{error,warn,info,debug}` instead of `println!`. Use `expect_used = "warn"` — prefer `?` and `anyhow::Context`. Non-test code has no `.expect()` allowance; the `#[expect(clippy::expect_used, reason = "...")]` attributes that exist are on test modules.

Rust edition 2024 on the `stable` channel (`rust-toolchain.toml` pins the channel, not a version; let-chains and other current features are in use). Profile `release` uses `lto = "thin"`, `codegen-units = 1`, `strip = true`.

## Plan files

`docs/plans/MAIN.md` is the main plan: every open item is one line there, in working order (High priority, then the waves, then the backlog), and the next item is the first line from the top. A landed line is deleted in its landing commit; `git log` is the record. New designs go in `docs/plans/*.md` with `- [ ]` checklists for their steps and are linked from a MAIN.md line. When a subplan's last item lands, its durable decisions are promoted into `docs/` (`architecture.md`, `native-client.md`, or a doc of their own) and the subplan is deleted in that commit. `docs/plans/completed/` holds designs finished before this convention, kept for their rationale; nothing new moves there. A native feature also ticks the lines it delivered in `docs/plans/native-client-parity.md`.

## Things that are deferred / not implemented

Don't go looking for these — they're explicitly out of scope until the corresponding plan item is unchecked:

- **Auto-update** for the native client — deferred until its installer (Phase 6) and a signed release pipeline exist.
- **Code signing / notarization** of the native client's installer (Phase 6) and the binaries inside it — there is no signing cert, so an unsigned bundle trips SmartScreen.
- **Sub-agent / Task-tool interception**, **multi-machine attach**, **cloud sync** — explicit non-goals. Viewing a subagent's transcript read-only is planned (`docs/plans/MAIN.md`, "View subagent streams"). The mobile app is planned (`docs/plans/mobile-app.md`) but not started.
