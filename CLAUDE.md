# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project shape

**The client is the native GPUI app** (`apps/native`, design in `docs/native-client.md`, open work in Linear team RT, snapshot in `docs/plans/MAIN.md`). The earlier Tauri app is retired and not on `main`: the `tauri` branch, cut from the `tauri-last` tag, keeps it as history only. Nothing is fixed or built there, and daemon fixes are not cherry-picked to it.

`rustling-tulip` is a native desktop client + a long-lived Rust daemon that orchestrates many parallel `claude` CLI sessions across single repos and multi-repo "workspaces". The daemon owns all PTYs and child processes; the client is just a client. **No code in this repo calls the Anthropic API directly** — the daemon always shells out to the `claude` CLI, which is the stable boundary.

```
crates/protocol/        shared wire types (serde JSON over WS) — the contract between daemon and clients
crates/daemon/          binary = rustling-tulipd: WS server, PTY pool, registry, git/scrollback/orphan logic
crates/tracer/          binary = rt-tracer.exe: per-session ConPTY supervisor that survives daemon restarts
crates/tracer-protocol/ stable ABI between daemon and tracer (additive-only; see docs/tracer-abi.md)
crates/daemon-client/   client-side daemon supervision (ensure-running, handshake, config dir, client identity, stop, and the `isolation` check that a driven run's config, binaries and worktrees dirs are absolute and under a `.tmp` folder) shared by the clients
crates/agents/api/      package agent-api: the typed Agent trait (type Options) and its blanket AgentPlugin adapter, SpawnContext, HeadlessEvent, validate, workspace_prelude, and the conformance check (feature test-support)
crates/agents/claude/   package agent-claude: the Claude plugin (ClaudeOptions and descriptor, argv, hook settings document, stream-json parsing, default_model, the DeepSeek provider table, the conversation transcripts it finds)
crates/agents/registry/ package agent-registry: the compiled-in plugin list (plugin(&AgentId), all(), descriptors(), default_agent())
crates/dev-daemon/      package dev-daemon (publish = false, never in the installer): `IsolatedDaemon`, an isolated rustling-tulipd under a `.tmp` scratch folder (refuses any other folder before it spawns), shared by the native client's live tests (`LiveDaemon` in apps/native/tests/support/live.rs wraps it) and the headless driver's tools
crates/tailnet/         Tailscale CLI wrapper (whois, whoami, status, cert) and the owner rule that decides which tailnet peers may connect (docs/plans/remote-access.md)
apps/native/            binary = rustling-tulip-native (thin main.rs over a lib, rustling_tulip_native; depends on crates/tailnet for the host picker's peer scan): GPUI + alacritty_terminal desktop client (see docs/native-client.md); tests/ui/ (one test binary, a module per spec) drive RootView::with_transport against a scripted fake daemon
crates/agents/claude/fake/ fake-claude CLI shim the Claude plugin hands the native e2e tier through fake_cli
tools/e2e/fake-tailscale/ fake `tailscale` CLI shim (whoami, whois, status, cert from a JSON state file) used by the live tier's remote specs
tools/e2e/fake-ssh/     fake `ssh` shim (checks the client's argv, then runs `rustling-tulipd bridge` locally) used by the live tier's SSH specs
tools/folder-bench/     package folder-bench (publish = false, never in the installer): the dev-only harness that generates deterministic folder fixtures and times folder-transfer candidates from this PC to the builder, against an isolated daemon there; see its README.md
tools/rt-driver-mcp/    package rt-driver-mcp (never in the installer): the stdio MCP server `rt-driver` (registered in .mcp.json) that launches an isolated driven native client and calls its driver endpoint; see docs/native-client.md "Driver MCP server"
README.md               the user-facing feature showcase and install steps, refreshed each release (docs/release.md)
CHANGELOG.md            what changed in each release, for users; each landed item adds its bullet under Unreleased
docs/README.md          the docs start page: one line per doc under docs/, the developer layout, build and run notes, and the glossary
docs/architecture.md    components, what the product does, and a task index: which files to read, in order, for each kind of change
docs/native-client.md   the native client's settled design decisions
docs/auto-update.md     how the native client finds, verifies and installs a published release
docs/env-secrets.md     how secret env rows are sealed into Credential Manager, and what is and isn't protected
docs/plans/MAIN.md      the plan snapshot, generated from Linear team RT: every open item by wave, in working order
docs/plans/*.md         design files: designs and rulings for open items, linked from their Linear issues
```

## Reference docs

Read the matching doc before changing or debugging its area; each holds rules the code relies on and the types do not show.

- [docs/on-disk.md](docs/on-disk.md): any file or folder the app reads or writes, such as `state.json`, `native-ui.json`, session sidecars, `daemon.json`, logs, history, ledgers, Credential Manager entries, worktree paths and atomic writes. Debugging spawn, connect or shutdown starts with its log notes.
- [docs/wire-protocol.md](docs/wire-protocol.md): any `ClientMessage` or `DaemonMessage`: the handshake, `Welcome.features`, `request_id` echoes, `Error` codes, chat messages, resize claims, tailnet connection rules.
- [docs/session-runtime.md](docs/session-runtime.md): session status (hooks, screen rules, `Working`, `AwaitingInput`, `Idle`, `background`), orphan recovery and tracer reattach, the chat mirror, an agent typed into the shell, workspace spawns, the git watcher.
- [docs/testing.md](docs/testing.md): writing or running UI specs and the live and smoke tiers, the fake CLIs, what each `native-shot` view draws, nextest settings, paused-clock tests.
- [docs/daemon-cli.md](docs/daemon-cli.md): the `rustling-tulipd` subcommands (`pair`, `shutdown`, `apply-update`, `bridge`, `screen`, and the session-driving verbs `sessions`, `read`, `spawn`, `send`, `kill`, `wait` and `watch`), their exit codes and output, and how a client starts, retires or refuses a daemon.

## Common commands

PowerShell on Windows is the primary dev environment. `rt.ps1` in the repo root is a convenience wrapper for the most common tasks:

```powershell
# Convenience wrapper (recommended)
.\rt.ps1                  # build daemon + tracer, then run the native client (same as `launch` / `native`)
.\rt.ps1 build            # build only: daemon, tracer and native client, then sweep target/ output older than 14 days (cargo-sweep)
.\rt.ps1 build -Release   # same, release profile (-Release also applies to launch, restart, native, native-smoke, native-shot; native-e2e refuses it, since its test-only daemon must stay in target\test-release)
.\rt.ps1 setup            # install/check Windows build prerequisites via winget (Git, Node.js, Rust, C++ Build Tools), plus cargo-sweep and cargo-nextest
.\rt.ps1 stop             # kill any running daemon and tracers; remove the stale handshake
.\rt.ps1 restart          # build daemon + tracer, stop the daemon (sessions survive in their tracers), run the native client
.\rt.ps1 test             # cargo nextest run --workspace --cargo-profile test-release, then the driver specs (-p rustling-tulip-native --features driver); fails pointing at `setup` without cargo-nextest
.\rt.ps1 test -Reached    # run only the tests the change reaches: a changed file's row in tools/test-map.json, else the full suite (-Base <ref> sets the diff base: branch.<name>.base, else main's merge-base with HEAD)
.\rt.ps1 test -Reached -PrintOnly   # print the selection ("full suite: <file> is central" / "is unmapped", "reached: <filter>", "reached (features driver): <filter>", or "no tests reached") and run nothing
.\rt.ps1 test -Validate   # check tools/test-map.json ("test map ok: <n> rows; <m> native source and test files covered")
.\rt.ps1 clippy           # the agent seam check (tools/check-agent-names.ps1) first, then the strict lint pass (-D warnings)
.\rt.ps1 fmt              # cargo fmt --all
.\rt.ps1 clean            # cargo clean
.\rt.ps1 native           # build daemon + tracer, then run the native client (extra args: a session id to focus)
.\rt.ps1 native-e2e       # native client specs (e2e_live, e2e_recover, e2e_remote, e2e_ssh, e2e_multi_host) against a real daemon, built with the e2e-loopback-peers feature, isolated under .tmp/ (fake-claude, fake-tailscale and fake-ssh need node), as one gate call that .gate/desktop.toml's `session` patterns send to a builder's console session, whose Credential Manager the sealed-secret spec needs
.\rt.ps1 native-e2e -Test e2e_ssh -Filter 'test(/reconnect/)'   # one or more spec files (comma-separated) and a nextest filterset, in the same routed gate call
.\rt.ps1 native-smoke     # launch the native client exe in a cloaked, never-focused window; check it connects and takes keys, and (built with the driver feature) that the driver's posted pointer input acts (leaves out the live no-focus checks, `native-drive-check`)
.\rt.ps1 native-drive-check   # only the live no-focus checks: a scripted drive, and the app's own toast windows, must leave the desktop's foreground window and cursor unchanged; the gate routes it to a builder's logged-on desktop, and run on this PC it refuses unless -HandsOff
.\rt.ps1 native-shot main # PNG of the client window over a fake daemon (views: main, main-compact, source-control, diff, settings, settings-claude, spawn, viewer, viewer-md, dashboard, dashboard-compact, connections, connections-ssh, menu, update, chat, summary, themes, themes-edit, multi-host, spawn-hosts, settings-hosts, settings-sounds, update-notes, chat-compact) in .tmp\shots\
.\rt.ps1 installer        # release build, then the NSIS installer at target\installer\rustling-tulip-<version>-setup.exe and its .sha256 beside it, which the app's updater checks a download against, signed with Azure Artifact Signing; `-Unsigned` for a test build (needs NSIS 3; tools/installer/README.md)
.\rt.ps1 help             # usage summary
pwsh -NoProfile -File tools/rt-driver-mcp/install.ps1   # build and install the rt-driver MCP server that .mcp.json starts (release build through the gate)
pwsh -NoProfile -File tools/rt-driver-mcp/live-check.ps1   # the driver MCP's live spec; run it as one gate job with --slot heavy (docs/testing.md)
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

# Tests run with cargo-nextest and build with the test-release profile (Cargo.toml): optimised like release, but without LTO
# and with incremental builds, so an edit rebuilds in seconds instead of minutes. --release is for shipping builds only.
# nextest's own --profile picks a config profile, not the build profile: the build profile is --cargo-profile.
cargo nextest run --workspace --cargo-profile test-release
cargo nextest run -p rustling-tulip-native --cargo-profile test-release --features driver -E 'test(/^driver::/) | test(/^ui_driver_registry::/)'   # the driver specs, which need the off-by-default driver feature; the full suite runs this after the workspace run
cargo nextest run -p daemon --cargo-profile test-release <test_name>
cargo nextest run -p protocol --cargo-profile test-release

# Native client UI specs: in-process GPUI (test-support), fake daemon, no real window or input
cargo nextest run -p rustling-tulip-native --cargo-profile test-release --test ui ui_terminal   # one ui binary; the filter picks a spec module (ui_sidebar, ui_tabs, …)
cargo nextest run -p rustling-tulip-native --cargo-profile test-release --features driver --test ui ui_driver_registry   # the driver registry spec runs only with the off-by-default driver feature
# e2e_live.rs, e2e_recover.rs, e2e_remote.rs, e2e_ssh.rs, e2e_multi_host.rs and smoke_window.rs are #[ignore]d here; run them through rt.ps1 native-e2e / native-smoke
```

The repo is in `test mode: stable`: every landed change's last gate runs the full suite (`cargo nextest run --workspace --cargo-profile test-release`, then the driver specs with `-p rustling-tulip-native --features driver`; `.\rt.ps1 test` runs both). The **test map**, `tools/test-map.json` (keys `central`, `ignore`, `rows` of source paths to nextest filtersets), still drives `.\rt.ps1 test -Reached` for a quick run while iterating.

The release step is cutting a versioned release from the Linear release pipeline's `vNext`, only after a green full suite (procedure: `docs/release.md`); `v0.1.0-alpha` was the first.

Every cargo command `rt.ps1` runs goes through the repo's gate wrapper, `pwsh -NoProfile -File tools/gate.ps1 run …` (`Invoke-Gated`, logging to `.tmp/rt-<name>.log`), so a bare `.\rt.ps1 test` routes its nextest run to a builder machine; inside a gate's command (`GATE_SLOT_HELD` set by a local gate, `GATE_BOX_JOB` by a routed run on a builder; `Test-UnderGate`) it runs them directly, and the native client the `launch` verbs open holds no slot.

Nothing calls `gate.exe` directly. The wrapper hands the call to the installed gate.exe (installed from the gate repo with `pwsh tools/install.ps1`), or runs the command directly on a machine without one.

Four rules keep agents' gate runs routable:
- A command that names another worktree's files is gated with `--cd <worktree>`, never from the main checkout, where the gate keeps it on this PC; `rt.ps1` passes `--cd` to its own tree, so call it bare or by its absolute path from any shell.
- One gate call per step: `cargo clippy …` and `cargo nextest run …` route on their own, and a `pwsh -Command` chain of them matches no route pattern and runs here. A live spec needs its daemon build in the same job, which `rt.ps1 native-e2e -Test <file> -Filter <set>` does.
- An environment variable a test needs goes with `--env NAME=value`, which carries it to a routed run, never `--local`.
- `GATE_BOX_JOB` is the gate's to set: setting it by hand makes `rt.ps1` (`Test-UnderGate`) skip its own gating.

`.gate/desktop.toml` declares the routed commands that need a builder's logged-on desktop (`patterns`: `native-shot`'s `cargo run … --example shot`, which opens a window, and `rt.ps1 native-drive-check`, which reads the desktop's foreground window and cursor), only its console session (`session`: `rt.ps1 native-e2e`, for the Credential Manager vault) or a hidden desktop of their own (`hidden`: `rt.ps1 native-smoke`, whose cloaked window never needs the logged-on one); the PC consents to each in the gate's `remote.json`.

To look at a UI change, run `.\rt.ps1 native-shot <view>`, Read the PNG it writes to `.tmp\shots\`, and show the user before and after shots for any visual choice.

## Environment variables

| Variable | Purpose | Default |
|---|---|---|
| `RUSTLING_TULIP_CLAUDE` | Path to `claude` binary; it also overrides a program set in Settings' launch command (the extra arguments still apply); `daemon.log` logs each registered agent's override at start (`<agent id> binary override status`, fields `variable` and `value`) | `claude` (PATH lookup) |
| `RUSTLING_TULIP_TAILSCALE` | Path to the `tailscale` CLI that `crates/tailnet` runs | `%ProgramFiles%\Tailscale\tailscale.exe`, else `tailscale` (PATH lookup) |
| `DEEPSEEK_API_KEY` | DeepSeek key; the Claude plugin's spawn environment reads it at each DeepSeek-provider Claude spawn through the daemon's key lookup (process environment, then the Windows user scope, `crates/daemon/src/user_env.rs`; table in `crates/agents/claude/src/providers.rs`) and passes it to the child only as `ANTHROPIC_AUTH_TOKEN`; never stored | unset (the spawn is refused) |
| `RUSTLING_TULIP_SHELL` | Shell used for plain-shell sessions and as the PTY child that hosts an interactive Claude session | auto-detect |
| `RUSTLING_TULIP_CONFIG_DIR` | Config dir override (useful for e2e test isolation) | `%APPDATA%\leftos\rustling-tulip\config\` (an elevated daemon: under `...\data\elevated\`, or under the config dir when that is overridden) |
| `RUSTLING_TULIP_WORKTREES_DIR` | Worktrees root override | `%LOCALAPPDATA%\leftos\rustling-tulip\data\worktrees\` (an elevated daemon: under `...\data\elevated\`, or under the config dir when that is overridden) |
| `RUSTLING_TULIP_BINARIES_DIR` | Cached-binary root override: the content-addressed copies of the daemon, tracer and CLI shims a spawn runs (`crates/daemon/src/paths.rs`) | `%LOCALAPPDATA%\leftos\rustling-tulip\data\binaries\` (an elevated daemon: under `...\data\elevated\`, or under the config dir when that is overridden) |
| `RUSTLING_TULIP_TAILNET_DIR` | Tailnet listener's certificate dir override (useful for e2e test isolation) | `%LOCALAPPDATA%\leftos\rustling-tulip\data\tailnet\` (an elevated daemon: under `...\data\elevated\`, or under the config dir when that is overridden) |
| `RUSTLING_TULIP_REMOTE_FILES_DIR` | Native client: download root for files fetched from a remote host (`daemon_client::remote_files_dir()`; useful for e2e test isolation) | `%LOCALAPPDATA%\leftos\rustling-tulip\data\remote-files\` |
| `RUSTLING_TULIP_SSH` | Native client: path to the `ssh` binary it runs for an SSH host; an empty value counts as unset | `ssh` (PATH lookup), else `%SystemRoot%\System32\OpenSSH\ssh.exe` |
| `RUSTLING_TULIP_UPDATE_FEED` | Native client: base URL of the releases feed the updater reads, the `/repos/leftos/rustling-tulip/releases?per_page=100` path being appended (tests point it at a local server); a dev build checks for updates only when it is set (`apps/native/src/update.rs`, `docs/auto-update.md`) | `https://api.github.com` |
| `RUSTLING_TULIP_UPDATES_DIR` | Native client: download root for update installers and `last-result.json`; a non-empty value is used as given, an empty one counts as unset | `%LOCALAPPDATA%\leftos\rustling-tulip\data\updates\` |
| `RUSTLING_TULIP_OFFSCREEN_WINDOW` | Native client: open the window cloaked and never activate it: it carries `WS_EX_NOACTIVATE`, opens at the bottom of the Z order, and the process exits with code 3 if the cloak fails, so it is never shown (the smoke tier sets it; `shot` and `diff_bench` honour it the same way) | unset |
| `RUSTLING_TULIP_DRIVER` | Native client, driver builds only: unset or empty is a normal run; `1` is a driven run, which needs the `driver` feature, `RUSTLING_TULIP_OFFSCREEN_WINDOW`, the three isolated dirs under `.tmp` and every registered agent's override variable, else the client exits 4 before touching any folder (`driver run refused: <reason>` on stderr); any other value exits 4 too (`apps/native/src/driver_gate.rs`, `docs/native-client.md` "Driver endpoint") | unset |
| `RUSTLING_TULIP_DRIVER_TOASTS` | Native client, driver builds only: exactly `1` in a driven run shows the app's own toast windows on the desktop the run sits on while the main window stays cloaked, for the live toast check `native-drive-check` runs on a builder; any other value, or a run that is not driven, shows none, and it never refuses a run (`driver_gate::driver_toasts_wanted`) | unset |

## Architecture invariants

**Single source of truth for the wire protocol.** All daemon/client messages live in `crates/protocol/src/lib.rs` as tagged enums (`#[serde(tag = "type", rename_all = "snake_case")]`).

When adding a message, update both directions (`ClientMessage` + `DaemonMessage` if it's a request/response) and the matching handler arm in the domain module's `handle` under `crates/daemon/src/server/` plus its variant in `dispatch`'s or-pattern in `crates/daemon/src/server.rs` and the native client's handling (`RootView`'s message folds, `on_message` in `apps/native/src/lib.rs`, and chat messages fold in `apps/native/src/chat_view.rs` (`fold_chat_or_agents`); `net.rs` only forwards). Changes stay additive where they can (see "When to bump" below).

**App code never names an agent.** The daemon, protocol, native client and tracer ask the registry and the plugin's descriptor; agent-specific behaviour lives in `crates/agents/<agent>/`. `tools/check-agent-names.ps1` (a prek hook and the first step of `.\rt.ps1 clippy`) fails on `claude`, `anthropic` or `deepseek` elsewhere, apart from test code and the hits listed in `tools/agent-names-allow.txt`.

**A feature that spends the user's subscription is opt-in.** Anything that runs an agent CLI or calls a paid provider without a direct user action (the Dashboard summarizer and any later one) is off until the user turns it on; its default is off, and a missing setting reads as off. A user action that starts a run (a spawn, a preset, a Test button) is not covered.

**Saved user data survives every update.** The app is released and used by more than its owner. A change to the shape of anything a user set or saved (`native-ui.json`, `state.json`, presets, themes, history, `devices.json`, `remote.json`, the Credential Manager entries) still reads the shape of every release users may still run, `v0.5.1-alpha` and later: a renamed or removed field is migrated on load to where its value now lives, never dropped, and the change carries a test that loads a file in the oldest such release's shape. Data no released build wrote (a format added since the last release tag) needs no migration.

Runtime state may be lost on an update across a breaking change: live sessions over a tracer-ABI or sidecar break, scrollback, activity logs and summary rings. The release notes say so when a release breaks them (`docs/auto-update.md`).

**When to bump `protocol-version.json`.** *Additive* changes (new variant on a tagged enum, new `#[serde(default)]` field on a struct, new message type) are NOT a protocol bump. The range-based handshake (`SUPPORTED_PROTOCOL_VERSIONS`) + the `InboundClientMessage::Unknown` / `InboundDaemonMessage::Unknown` parse wrappers absorb unknown top-level types.

Nested enums that grow over time (`TabLayout`, `RearrangeLayout`, `InjectorStep`, `PresetVariableKind`, `TabContent`) each carry a `#[serde(other)] Unknown` unit variant that absorbs unrecognized values *in place* — the containing message keeps decoding. New nested enums should follow the same `#[serde(other)]` pattern from day one.

*Breaking* changes — renaming a field, removing a variant, changing semantics — DO require a bump. The installer updates the client and its local daemon together, and the handshake refuses a remote host that shares no version with the client, so a bump sets `version` to the new number and `supported` to only the versions the new daemon can serve, normally just the new one (`[25]`, not `[25, 24]`).

Before every breaking bump, verify that `daemon_client::ensure_running` retires the old healthy daemon through the HTTP `/shutdown` path and spawns the new daemon, so tracer-backed sessions reattach instead of leaving the client stuck at `auth_failed` (`crates/daemon-client/src/supervisor.rs`: `classify_existing_daemon`, `retire_daemon`, and the test `native_client_retires_daemon_that_speaks_only_an_older_version`).

## Style and lints

Workspace `Cargo.toml` enforces clippy pedantic + denies on `unwrap_used`, `panic`, `dbg_macro`, `todo`, `print_*`, `exit`, etc. Use `tracing::{error,warn,info,debug}` instead of `println!`. Use `expect_used = "warn"` — prefer `?` and `anyhow::Context`. Non-test code has no `.expect()` allowance; the `#[expect(clippy::expect_used, reason = "...")]` attributes that exist are on test modules.

Rust edition 2024 on the `stable` channel (`rust-toolchain.toml` pins the channel, not a version; let-chains and other current features are in use). Profile `release` uses `lto = "thin"`, `codegen-units = 1`, `strip = true`.

## Plan files

Every task is a Linear issue in team RT, grouped into one project per wave; the rules are in `~/.claude/docs/plan-operations.md`, and this repo's project order is in the `rustling-tulip-nextup` profile. `docs/plans/MAIN.md` is a generated snapshot of it: never edit it by hand; change Linear and regenerate it.

A task, idea or steer that comes up mid-task gets an **add** first. New designs go in `docs/plans/*.md` with `- [ ]` checklists for their steps and are linked from their issue's description or their project's content.

Once nothing open links a design file, its durable decisions are promoted into `docs/` (`architecture.md`, `native-client.md`, or a doc of their own) and the file is deleted in that commit. `docs/plans/archive/` holds designs finished before the plan moved to Linear, kept for their rationale; nothing new moves there. A native feature also ticks the lines it delivered in `docs/plans/native-client-parity.md`.

## Things that are deferred / not implemented

Don't go looking for these — they're explicitly out of scope until their plan item comes up:

- **Notarization and macOS code signing** (RT-665). Windows signing is done: `.\rt.ps1 installer` signs the binaries and the setup exe (`tools/installer/README.md`).
- **Sub-agent / Task-tool interception**, **attach from outside the tailnet** by internet exposure or a relay, **cloud sync** — explicit non-goals; the tailnet listener is the one way in from another machine over the network, and clients over OpenSSH are being built (`docs/plans/ssh-remote.md`; `docs/architecture.md` "Non-goals", `docs/plans/remote-access.md`). Viewing a subagent's transcript read-only is planned (Wave 10, "Subagent streams"; design in `docs/plans/subagent-streams.md`). The mobile app is planned (`docs/plans/mobile-app.md`) but not started.
