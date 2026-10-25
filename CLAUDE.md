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
crates/daemon-client/   client-side daemon supervision (ensure-running, handshake, config dir, client identity, stop) shared by the clients
crates/agents/api/      package agent-api: the typed Agent trait (type Options) and its blanket AgentPlugin adapter, SpawnContext, HeadlessEvent, validate, workspace_prelude, and the conformance check (feature test-support)
crates/agents/claude/   package agent-claude: the Claude plugin (ClaudeOptions and descriptor, argv, hook settings document, stream-json parsing, default_model, the DeepSeek provider table, the conversation transcripts it finds)
crates/agents/registry/ package agent-registry: the compiled-in plugin list (plugin(&AgentId), all(), descriptors(), default_agent())
crates/tailnet/         Tailscale CLI wrapper (whois, whoami, status, cert) and the owner rule that decides which tailnet peers may connect (docs/plans/remote-access.md)
apps/native/            binary = rustling-tulip-native (thin main.rs over a lib, rustling_tulip_native; depends on crates/tailnet for the host picker's peer scan): GPUI + alacritty_terminal desktop client (see docs/native-client.md); tests/ui/ (one test binary, a module per spec) drive RootView::with_transport against a scripted fake daemon
crates/agents/claude/fake/ fake-claude CLI shim the Claude plugin hands the native e2e tier through fake_cli
tools/e2e/fake-tailscale/ fake `tailscale` CLI shim (whoami, whois, status, cert from a JSON state file) used by the live tier's remote specs
docs/architecture.md    components, what the product does, and a task index: which files to read, in order, for each kind of change
docs/native-client.md   the native client's settled design decisions
docs/env-secrets.md     how secret env rows are sealed into Credential Manager, and what is and isn't protected
docs/plans/MAIN.md      the plan snapshot, generated from Linear team RT: every open item by wave, in working order
docs/plans/*.md         design files: designs and rulings for open items, linked from their Linear issues
```

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
.\rt.ps1 test             # cargo nextest run --workspace --cargo-profile test-release (fails pointing at `setup` without cargo-nextest)
.\rt.ps1 test -Reached    # run only the tests the change reaches: a changed file's row in tools/test-map.json, else the full suite (-Base <ref> sets the diff base: branch.<name>.base, else main's merge-base with HEAD)
.\rt.ps1 test -Reached -PrintOnly   # print the selection ("full suite: <file> is central" / "is unmapped", "reached: <filter>", or "no tests reached") and run nothing
.\rt.ps1 test -Validate   # check tools/test-map.json ("test map ok: <n> rows; <m> native source and test files covered")
.\rt.ps1 clippy           # strict lint pass (-D warnings)
.\rt.ps1 fmt              # cargo fmt --all
.\rt.ps1 clean            # cargo clean
.\rt.ps1 native           # build daemon + tracer, then run the native client (extra args: a session id to focus)
.\rt.ps1 native-e2e       # native client specs (e2e_live, e2e_recover, e2e_remote) against a real daemon, built with the e2e-loopback-peers feature, isolated under .tmp/ (fake-claude and fake-tailscale need node)
.\rt.ps1 native-smoke     # launch the native client exe in a cloaked, never-focused window; check it connects and takes keys
.\rt.ps1 native-shot main # PNG of the client window over a fake daemon (views: main, main-compact, source-control, diff, settings, settings-claude, spawn, viewer, viewer-md) in .tmp\shots\
.\rt.ps1 installer        # release build, then the NSIS installer at target\installer\rustling-tulip-<version>-setup.exe (needs NSIS 3; tools/installer/README.md)
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

# Tests run with cargo-nextest and build with the test-release profile (Cargo.toml): optimised like release, but without LTO
# and with incremental builds, so an edit rebuilds in seconds instead of minutes. --release is for shipping builds only.
# nextest's own --profile picks a config profile, not the build profile: the build profile is --cargo-profile.
cargo nextest run --workspace --cargo-profile test-release
cargo nextest run -p daemon --cargo-profile test-release <test_name>
cargo nextest run -p protocol --cargo-profile test-release

# Native client UI specs: in-process GPUI (test-support), fake daemon, no real window or input
cargo nextest run -p rustling-tulip-native --cargo-profile test-release --test ui ui_terminal   # one ui binary; the filter picks a spec module (ui_sidebar, ui_tabs, …)
# e2e_live.rs, e2e_recover.rs, e2e_remote.rs and smoke_window.rs are #[ignore]d here; run them through rt.ps1 native-e2e / native-smoke
```

Tests run with cargo-nextest although the full suite measured 1.41x slower than `cargo test`, because the daemon's git-fixture tests (RT-70) dominate it. `.config/nextest.toml` sets fail-fast off, no retries, a 60 s slow-timeout and a `serial-live` test group that runs `e2e_live`, `e2e_recover`, `e2e_remote` and `smoke_window` one at a time. The protocol, tracer-protocol and daemon-client crates set `doctest = false`.

The repo is in `test mode: stable`: every landed change's last gate runs the full suite (`cargo nextest run --workspace --cargo-profile test-release`). The **test map**, `tools/test-map.json` (keys `central`, `ignore`, `rows` of source paths to nextest filtersets), still drives `.\rt.ps1 test -Reached` for a quick run while iterating.

The release step is cutting a versioned release from the Linear release pipeline's `vNext`, only after a green full suite (procedure: `docs/release.md`); `v0.1.0-alpha` was the first.

Every cargo command `rt.ps1` runs goes through the repo's gate wrapper, `pwsh -NoProfile -File tools/gate.ps1 run …` (`Invoke-Gated`, logging to `.tmp/rt-<name>.log`), so a bare `.\rt.ps1 test` routes its nextest run to a builder machine; inside a gate's command (`GATE_SLOT_HELD` set) it runs them directly, and the native client the `launch` verbs open holds no slot.

Nothing calls `gate.exe` directly. The wrapper hands the call to the installed gate.exe (installed from the gate repo with `pwsh tools/install.ps1`), or runs the command directly on a machine without one.

`rustling-tulipd pair` prints a one-time device pairing code from the running daemon (it needs the daemon already running, reads `daemon.json` and takes no instance lock; `crates/daemon/src/cli_pair.rs`); the owner types the code on a device whose pairing request is waiting.

The native client auto-spawns the daemon on first connect via `daemon_client::ensure_running` (`crates/daemon-client`), passing the protocol versions it speaks. The daemon writes `port` + `auth_token` + `pid` + `supported_versions` to `daemon.json` in the config dir below; clients read that to connect.

A daemon sharing none of the client's versions is retired and replaced. The native client speaks only the current version, and the daemon's `supported` (`protocol-version.json`) lists only the current version too.

## E2E tests

The native client has three test tiers:

- **UI specs** — `cargo nextest run -p rustling-tulip-native --cargo-profile test-release --test ui` (filter by module: `ui_terminal`, `ui_sidebar`, `ui_tabs`, `ui_session_actions`, …): in-process GPUI against a scripted fake daemon; no real window, input or daemon.

  Every spec file is a module of the one `tests/ui/main.rs` binary (a new spec is `tests/ui/ui_<name>.rs` plus its `mod` line there), since linking and starting a binary per file cost more than the tests themselves; the crate's lib has `doctest = false`.
- **Live e2e** — `.\rt.ps1 native-e2e`: builds daemon + tracer in test-release and runs `tests/e2e_live.rs`, `tests/e2e_recover.rs` and `tests/e2e_remote.rs` under `nextest --run-ignored only` against a real daemon isolated under `.tmp/`; `e2e_remote` drives the daemon's tailnet listener as a tailnet client (pairing and approval, token reconnect, owner rule, refused commands, revoke, remote file fetch, remote access off). Needs `node` for the fake-claude and fake-tailscale shims.

  The daemon it runs is built with the off-by-default `e2e-loopback-peers` Cargo feature, which makes the tailnet listener admit loopback peers (judged by the real owner rule) and logs a startup warning "built with e2e-loopback-peers: the tailnet listener admits loopback peers; never ship this build"; only `native-e2e` builds with it, and the installer build refuses it. Every live daemon gets `RUSTLING_TULIP_TAILSCALE` (the fake-tailscale shim) and `RUSTLING_TULIP_TAILNET_DIR` (`<root>/tailnet`), so no spec reaches the real Tailscale or the real certificate dir.
- **OS smoke** — `.\rt.ps1 native-smoke`: builds daemon + tracer in test-release and launches the native client exe in a cloaked window that never takes focus, under `nextest --run-ignored only` (`tests/smoke_window.rs`) and checks it connects and that posted keys reach the shell.

To look at a UI change, run `.\rt.ps1 native-shot <view>` (`main`, `main-compact` for the sidebar's Compact leaves, `source-control`, `diff`, `settings`, `settings-claude`, `spawn`, `viewer`, `viewer-md`): it writes a PNG of the real client window over a scripted fake daemon (`apps/native/examples/shot.rs`) to `.tmp\shots\` and prints its path; Read the PNG, and show the user before and after shots for any visual choice.

The `crates/agents/claude/fake/` shim (`fake-claude.cmd` or `fake-claude.sh`, plus `index.mjs`) replaces the real CLI in the e2e tier. The plugin returns it from `Agent::fake_cli` (`agent_api::FakeCli`, feature `test-support`), and the live harness (`apps/native/tests/support/live.rs`) sets each registered agent's override variable (`program_env_var()`, `RUSTLING_TULIP_CLAUDE` for Claude) to its fake through `agent-registry`, so the daemon path-resolves the CLI from that variable at spawn time. It also points the plugin's home variable at `<root>/<agent id>-home` and puts the plugin's stub name first on `PATH`; a registered plugin with no fake panics.

The `tools/e2e/fake-tailscale/` shim (`fake-tailscale.cmd`, `.sh` and `index.mjs`, see its README) answers `whoami`, `whois`, `status` and `cert` from the JSON state file named by `RUSTLING_TULIP_FAKE_TAILSCALE_STATE`; it is wired in through `RUSTLING_TULIP_TAILSCALE`.

## Environment variables

| Variable | Purpose | Default |
|---|---|---|
| `RUSTLING_TULIP_CLAUDE` | Path to `claude` binary; it also overrides a program set in Settings' launch command (the extra arguments still apply); `daemon.log` logs each registered agent's override at start (`<agent id> binary override status`, fields `variable` and `value`) | `claude` (PATH lookup) |
| `RUSTLING_TULIP_TAILSCALE` | Path to the `tailscale` CLI that `crates/tailnet` runs | `%ProgramFiles%\Tailscale\tailscale.exe`, else `tailscale` (PATH lookup) |
| `DEEPSEEK_API_KEY` | DeepSeek key; the Claude plugin's spawn environment reads it at each DeepSeek-provider Claude spawn through the daemon's key lookup (process environment, then the Windows user scope, `crates/daemon/src/user_env.rs`; table in `crates/agents/claude/src/providers.rs`) and passes it to the child only as `ANTHROPIC_AUTH_TOKEN`; never stored | unset (the spawn is refused) |
| `RUSTLING_TULIP_SHELL` | Shell used for plain-shell sessions and as the PTY child that hosts an interactive Claude session | auto-detect |
| `RUSTLING_TULIP_CONFIG_DIR` | Config dir override (useful for e2e test isolation) | `%APPDATA%\leftos\rustling-tulip\config\` |
| `RUSTLING_TULIP_WORKTREES_DIR` | Worktrees root override | `%LOCALAPPDATA%\leftos\rustling-tulip\data\worktrees\` |
| `RUSTLING_TULIP_BINARIES_DIR` | Cached-binary root override: the content-addressed copies of the daemon, tracer and CLI shims a spawn runs (`crates/daemon/src/paths.rs`) | `%LOCALAPPDATA%\leftos\rustling-tulip\data\binaries\` |
| `RUSTLING_TULIP_TAILNET_DIR` | Tailnet listener's certificate dir override (useful for e2e test isolation) | `%LOCALAPPDATA%\leftos\rustling-tulip\data\tailnet\` |
| `RUSTLING_TULIP_REMOTE_FILES_DIR` | Native client: download root for files fetched from a remote host (`daemon_client::remote_files_dir()`; useful for e2e test isolation) | `%LOCALAPPDATA%\leftos\rustling-tulip\data\remote-files\` |
| `RUSTLING_TULIP_OFFSCREEN_WINDOW` | Native client: open the window cloaked and never activate it (the smoke tier sets it) | unset |

## Where things live on disk

Both sides resolve the config dir via the `directories` crate as `ProjectDirs::from("dev", "leftos", "rustling-tulip").config_dir()`. On Windows that expands to `%APPDATA%\leftos\rustling-tulip\config\` (note the `leftos\` + `\config\` segments — `directories` inserts them, so a plain `%APPDATA%\rustling-tulip\` path is wrong). Layout under that root:

- `state.json` — persisted repos + workspaces + tabs, plus daemon-side host settings (`worktrees_root_override`, `keep_awake_mode`: `"off"`, `"when_busy"` (the default) or `"always"`, `agent_commands`: `{"<agent serde name>": {"program", "args"}}` for the Settings launch command, only agents that set one, a blank pair removing the entry, `agent_settings`: `{"<agent id>": {<option>: <value>}}`, each agent's validated Settings page values) that must apply with no window open (see `crates/daemon/src/state.rs`).
- `daemon.json` — handshake (port + auth_token + pid + supported_versions); written on daemon start, removed on graceful shutdown.
- `sessions/<id>/meta.json` + `scrollback.bin` — orphan-recovery sidecar and PTY scrollback ring. `hook-settings.json` sits beside them for an interactive Claude spawn (passed as `--settings`) and for a recovered shell with a hook agent and a conversation id; a graceful stop deletes it with the sidecar.

  The sidecar's `program_name` follows the record (the shell's label once an agent typed into its shell has ended, the agent again on its next `SessionStart`), and `agent_in_shell` and `shell_label` mark a session whose PTY child is the user's shell, so a reattach runs the shell's status watcher (absent means the agent is the child).

  A recovered shell's `hook_agent` (the plugin that maps its hook events; absent means none, and the hook route then maps nothing for the session) is stored there too and restored by the orphan, reattached and abandoned inserts.
- `remote.json` — opt-in tailnet access switch `{enabled, port}`, absent until remote access is turned on; the daemon writes it (temp file and rename) whenever `ConfigureRemote` changes it, and an identical request writes nothing; a missing, unreadable or zero-port file means off, and the port defaults to 8787 (`crates/daemon/src/remote_config.rs`).
- `history/<id>.json` — session history: one entry per ended session (how it ended, its spawn config and folders, its `busy_excluded` flag when true, its `agent` (absent for a shell) with the `options` and `trusted` flag an import recorded (a tracer-log import infers a DeepSeek `provider` option from its `--model`), its conversation id `conversation_id`), written on every end path, written at startup for each dead non-headless sidecar that has none (a session a reboot, a crash or a killed process tree ended: `TracerLost`, or its true end when the tracer log shows one), backfilled at startup from `logs/tracer-*.log`, pruned after 7 days (`crates/daemon/src/history.rs`).

  The recover dialog lists these and `RecoverSessions` respawns them: an agent entry with its plugin's resume (Claude: `claude --resume`), as `RecoverAs::Agent`, `RegisterRepoThenAgent`, `ShellResume` (a shell typing the resume) or `Shell` (a bare shell).
- `contested/<agent id>.json` — the conversation ids that discovery left unclaimed because two or more sessions waiting in one folder were each eligible for them: a JSON array of `{id, contested_at}`, one file per agent, written atomically when an id becomes contested, entries older than 7 days dropped, the directory created on first write; a stored id is never claimed (`crates/daemon/src/conversations/contested.rs`, `Dirs::contested_dir`).
- `env-secrets.json` — index of the secret env-row values sealed into Windows Credential Manager (`{id, key, created_at}`, never a value); the values are Credential Manager entries under the service `rustling-tulip` (`rustling-tulip:<8 hex of the config dir's SHA-256>` while `RUSTLING_TULIP_CONFIG_DIR` is set), user `env/<KEY>/<id>`. Stored configs carry `${secret:<id>}` references instead (`crates/daemon/src/env_secrets.rs`, `docs/env-secrets.md`).
- Device tokens (native client, one per remote host) are Credential Manager entries under the same service naming, user `device/<lower-cased host>` (no port); a host with no entry is dialled with an empty token (`crates/daemon-client/src/device_tokens.rs`, `CredentialManagerTokens`).
- `devices.json` — the tailnet devices approved to connect: their SHA-256 token hashes, never a token, with each device's `last_seen` and its `revoked` flag (a blocked device); `ForgetDevice` removes a record (`crates/daemon/src/devices.rs`, `DeviceStore`).
- `daemon.lock` — the single-instance lock; a second daemon on the same config dir exits without touching anything.
- `logs/daemon.log` — daemon tracing output. Rotated on each daemon start: the previous run survives as `daemon.log.old` (see `crates/daemon/src/main.rs::init_tracing`).
- `logs/autostart.log` — the login launch's short log: `rustling-tulipd --detach` (the command the HKCU `Run` value `rustling-tulip-daemon` starts at login, written by `daemon_client::autostart`; the client only adds `--detach` to an entry that already names its own daemon and never repoints one) copies itself into the binary cache, starts that copy and exits (`crates/daemon/src/detach.rs`). Truncated on each such launch.
- `logs/native.log` — native client (`apps/native`) tracing output, also mirrored to stderr (on Windows only a debug build has a console to show it: a build without debug assertions is a GUI-subsystem exe, so an installed launch opens no console window). Rotated on each launch to `native.log.old`.
- `client-id-native` — per-install client identity (a bare UUID) that the native client sends in `Hello`; tab layouts are keyed by it.
- `native-ui.json` — native client UI state that stays on this machine: sidebar width, the sidebar's collapsed flag and the collapsed containers, the active tab, the quick-shell folder, the folder Add repo opens at (`last_repo_dir`), the folder the workspace creator's `.code-workspace` picker opens at (`last_vscode_dir`), the app-level terminal font and colours, the interface font (`ui_font_family`, absent for the bundled Geist), the shared list of recent custom colours, per-tab font sizes, the activity rail's view (`activity`), and the source-control panel's pinned repo, collapsed sections and split heights, the diff tabs' include-whitespace and highlight toggles, whether a Markdown viewer tab opens rendered (`viewer_markdown_rendered`, default true; a missing key loads as true), the per-host remote session counts (`host_session_counts`, `{"<lower-cased MagicDNS name>": <count>}`, only for hosts in `saved_hosts`; the import-remote-sessions modal compares against it), and the Settings modal's Spawn default (`spawn` holds only the trusted-launch default; Claude's approval mode and provider are the daemon's `agent_settings`, not stored here), General (the sidebar's leaf density among them, `general.leaf_density`, and `general.open_text_externally`, default off), Notifications (the `Needs you` and `Finished` toggles, `notifications.awaiting_input` and `notifications.finished`, and the skip choice) and App title choices (`general`, `notifications`, `title`) (`apps/native/src/sidebar.rs`).

  Its `saved_hosts` list (the tailnet hosts the host picker offers, `{name, port}` each; an entry that fails the DNS-name check is dropped on load) and `last_target` (`{name, port}`, absent or null for this PC) set the host the client boots onto; the host picker writes both (a switch sets `last_target` and adds the host, or updates its saved port, and Forget rewrites `saved_hosts`).

  Its `unc_hosts` list (network hosts terminal links may open) is edited by hand; the client re-reads it on each click and never writes it. The native client's bundled OFL fonts live in `apps/native/assets/fonts/` (sources and versions in its README), and the large-file hook exempts them.

When debugging spawn/connect/shutdown issues, both `daemon.log` and the client's log (`native.log`) together tell the full story — neither alone is enough.

Worktrees live under a **separate** root resolved via `ProjectDirs::data_local_dir().join("worktrees")` (overridable with `RUSTLING_TULIP_WORKTREES_DIR`). On Windows that's `%LOCALAPPDATA%\leftos\rustling-tulip\data\worktrees\` — machine-local, doesn't roam, and known-writable regardless of where the source repo lives.

Per-session worktree paths are `<worktrees-root>/wt.<branch-slug>/<repo-slug>` for a single repo and `<worktrees-root>/wt.<branch-slug>/<workspace-slug>/<offset>` for a workspace member, where the **anchor** is the common path-component prefix of the member repos' *parents* on `members[0]`'s drive and `<offset>` is each member's path below it.

This preserves inter-member relative paths inside a workspace session — if `repo1` references `repo2` as `../repo2` in source space, the same reference resolves inside the worktree set.

Members on another drive are anchored per drive under `<workspace-slug>/<drive anchor>/<offset>`; that folder's first segment takes a `-2`, `-3`, … suffix when the plain name would equal a primary-drive member's path or contain one (`…\ws\Y-2\b`). Slugs come from `git::name_slug`; repos and workspaces share one slug namespace, and saving a name whose slug is taken is refused.

A group folder has a `<slug>.rt-group` marker beside it (`{"kind","name"}`), which Manage worktrees reads to list each group separately; a group's members are its linked worktrees, a member nested inside another included (a submodule or plain clone inside one is not listed), and a workspace marker makes a one-member group launch as that workspace.

A spawn reuses the worktree git reports as holding the branch (any layout, so older `wt.<branch>/<sanitized-anchor>/…` worktrees are still reused) and refuses a folder at its path that isn't one of the repo's worktrees. Path construction lives in `git::workspace_worktree_paths` / `git::single_worktree_path`.

The tailnet listener's certificate lives in a third machine-local root beside the worktrees one, `ProjectDirs::data_local_dir().join("tailnet")` (overridable with `RUSTLING_TULIP_TAILNET_DIR`), `%LOCALAPPDATA%\leftos\rustling-tulip\data\tailnet\` on Windows: `<name>.crt` and `<name>.key`, written by `tailscale cert` for the host's MagicDNS name (no trailing dot).

It is not under the config dir, so the private key never roams (`crates/daemon/src/paths.rs` `resolve_tailnet_dir`, `crates/daemon/src/tailnet_listener.rs`).

Files the native client fetches from a remote host live in a fourth machine-local root beside those, `daemon_client::remote_files_dir()` (overridable with `RUSTLING_TULIP_REMOTE_FILES_DIR`), `%LOCALAPPDATA%\leftos\rustling-tulip\data\remote-files\` on Windows: `<host MagicDNS name, lower-cased>/<repo-name>/<rel-path>`, with Windows-unsafe names sanitised.

A fetch writes a hidden `.fetch-<launch stamp>-<n>.part` beside the target and renames it over the target when the file is done; a failed or cancelled fetch deletes its part. Nothing under it is ever pruned (`apps/native/src/file_fetch.rs`).

A fetched `.html` or `.htm` page that opens when done brings the relative media it names with it, saved at the same `<host>/<repo-name>/<rel-path>` place (never outside the repo root) and each fetched again if already on disk (`apps/native/src/html_media.rs`, `docs/native-client.md` "Fetched files").

## Architecture invariants

**Single source of truth for the wire protocol.** All daemon/client messages live in `crates/protocol/src/lib.rs` as tagged enums (`#[serde(tag = "type", rename_all = "snake_case")]`).

When adding a message, update both directions (`ClientMessage` + `DaemonMessage` if it's a request/response) and the matching handler arm in the domain module's `handle` under `crates/daemon/src/server/` plus its variant in `dispatch`'s or-pattern in `crates/daemon/src/server.rs` and the native client's handling (`RootView`'s message folds, `on_message` in `apps/native/src/lib.rs`; `net.rs` only forwards). Changes stay additive where they can (see "When to bump" below).

**No backward compatibility before the first release.** The app is unreleased: build as if from scratch. A breaking change to the wire, `state.json`, presets, sidecars or history is simply made, with no migration, legacy decoder, tolerant decode or backup; data that no longer decodes is ignored, and live sessions or history from an older build need not survive an upgrade. The bump rules below still apply to the version number.

**When to bump `protocol-version.json`.** *Additive* changes (new variant on a tagged enum, new `#[serde(default)]` field on a struct, new message type) are NOT a protocol bump. The range-based handshake (`SUPPORTED_PROTOCOL_VERSIONS`) + the `InboundClientMessage::Unknown` / `InboundDaemonMessage::Unknown` parse wrappers absorb unknown top-level types.

Nested enums that grow over time (`TabLayout`, `RearrangeLayout`, `InjectorStep`, `PresetVariableKind`) each carry a `#[serde(other)] Unknown` unit variant that absorbs unrecognized values *in place* — the containing message keeps decoding. New nested enums should follow the same `#[serde(other)]` pattern from day one.

*Breaking* changes — renaming a field, removing a variant, changing semantics — DO require a bump. There is no older client to keep, so a bump sets `version` to the new number and `supported` to only the versions the new daemon can serve, normally just the new one (`[25]`, not `[25, 24]`).

Before every breaking bump, verify that `daemon_client::ensure_running` retires the old healthy daemon through the HTTP `/shutdown` path and spawns the new daemon, so tracer-backed sessions reattach instead of leaving the client stuck at `auth_failed` (`crates/daemon-client/src/supervisor.rs`: `classify_existing_daemon`, `retire_daemon`, and the test `native_client_retires_daemon_that_speaks_only_an_older_version`).

**On-disk layout under `%APPDATA%\rustling-tulip\`** (see `crates/daemon/src/paths.rs`):
- `state.json` — repo + workspace registry plus host settings (never sessions)
- `daemon.json` — handshake (port, token, pid, supported protocol versions)
- `sessions/<id>/meta.json` — orphan-recovery sidecar; written at spawn, deleted on graceful stop
- `sessions/<id>/hook-settings.json` — the hooks document an interactive Claude spawn passes as `--settings` (a recovered shell's `shell_resume` command names its path); written at spawn, deleted on a graceful stop
- `sessions/<id>/scrollback.bin` — 2 MB ring (trims to 1.5 MB on overflow), replayed via `LoadScrollback` on attach
- `sessions/<id>/scrollback.truncated` — flag file iff the ring overflowed

Sessions are deliberately **not** in `state.json` — they're rebuilt from sidecars on startup so the daemon can survive restarts without a single fragile state blob.

**Orphan recovery + tracer reattach (Phase C.3).** On startup, `main.rs` reads all `meta.json` sidecars and partitions live vs dead via `orphan::is_session_alive` (sysinfo: a tracer-backed sidecar's `tracer_pid` must still be an `rt-tracer` process; a sidecar without tracer fields matches its `pid` against its `program_name`, which otherwise just names the program the user runs, the leaf's label).

For sidecars with `tracer_pid` + `tracer_pipe` set (post-C.3 spawns), `server::recovery::reattach_orphans` (`crates/daemon/src/server/recovery.rs`) connects to the still-running `rt-tracer.exe` over its named pipe and rebuilds a fully-functional `PtyHandle` — IO, resize, and kill all work as if the session had been freshly spawned.

The sidecar also keeps the session's `busy_excluded` flag (absent means false), kept current on every registry update and restored on reattach, and its last `status` and `status_since`, rewritten on every status change. A reattached session comes back `Idle` or `AwaitingInput` with its old stamp, and any other stored status comes back `Idle` stamped now.

Its status watcher then reads the replayed screen once its startup hold ends: a visible spinner makes it `Working`, an idle prompt box with no dialog clears `AwaitingInput`, and a screen no rule matches leaves the restored status.

A hook-reported prompt's `pending_input` is written beside `status` (absent when nothing is pending) and restored only with a restored `AwaitingInput`. Every sidecar write goes through `SessionRegistry::sync_sidecar`, which writes the record's mirrored fields under one lock. Reattach failures route the session to the abandoned bucket (sidebar Resume button, B.2 UX).

A sidecar whose tracer is gone is recorded as ended at start (see the history entry above), and once the daemon has bound and written its handshake, `recovery::auto_resume` resumes the qualifying ones by itself, one at a time, with no prompt, off switch or retry (an interactive Claude session with a spawn config, its own conversation id and an existing transcript, its worktrees still pinned to their folders, its in-place checkout still on its branch; `claude --resume <that id>`), then broadcasts `ResumeReport`; the rest stay abandoned, listed in Recover.

Pre-C.3 sidecars without tracer fields fall through to the read-only `insert_orphan` path; their underlying children almost never survive a daemon death, so that branch is effectively only for downgrade scenarios.

**A Claude session's status comes from its hooks and from rules over its rendered screen** (`pty_state.rs`, `screen.rs`, `screen/rules.rs`; design in `docs/plans/screen-status.md`).

The watcher feeds each session's PTY output into a per-session grid (`alacritty_terminal`, the crate the client renders with) and reads it with the session's own agent's rules (`screen/rules.rs`: `CompiledRules` compiles the plugin's `ScreenRules`, `rules_for(agent)` caches one set per agent; Claude's table is data in `crates/agents/claude/src/rules.rs`) a short debounce after output pauses.

There is no output/input volume classifier, no title heartbeat and no silence demotion for a Claude session: a quiet screen that still shows the spinner stays `Working`.

A session turns hook-driven on its first mapped hook event that passes the ordering check, from Claude Code's hooks, injected through the settings file its spawn writes. A hook event applies at once. A session whose hooks never run is screen-driven only.

A screen-derived change publishes at once when it enters `AwaitingInput` from a dialog footer or when a spinner line promotes the session to `Working`. Leaving `AwaitingInput` and `Working` to `Idle` publish only after hysteresis: 3 agreeing checks at least 150 ms apart, or the first agreeing check 700 ms after the first.

Every screen write is refused when a hook event, the owner's key or an exit moved the record (status, hook stamp or hook-driven flag) since the read. After any such foreign write, and when the watcher starts, the screen is left unread for 700 ms. A reading from the title alone, an overlay or a screen no rule matches never writes.

A plain shell keeps its own classifier (command output outpacing echo promotes to `Working`, `SHELL_IDLE_AFTER` of silence ends it) until it is hook-driven or its screen has shown Claude for a whole confirmation window; it then runs the same screen check. Once the shell's prompt has been back on the grid's last row for that window, `claude` has ended: the shell leaves hook-driven mode (clearing `background` and ending a `Working` or `AwaitingInput` turn at `Idle`) and returns to its classifier.

An interactive Claude session whose agent is typed into the user's shell (`agent_in_shell`; see "Tracer-backed PTY sessions") runs that same shell watcher. The daemon announces the agent's end once per run with `DaemonMessage::AgentExited { session_id }` on the session-event stream: on its `SessionEnd` hook (a `/clear` reason is not an exit) or, when no hook said so, when `hand_shell_back` sees the prompt return.

The record's `program_name` becomes the shell's label then and `claude` again on the next `SessionStart`; the session itself ends only when the shell exits.

A subagent's hook event (an `agent_id` in its input) never ends or replaces the main agent's pending prompt and never promotes the session to `Working`, and a subagent `PermissionRequest` raises one only when none is pending.

A main-agent `Stop` whose `background_tasks` lists a running task, or whose `session_crons` holds a one-shot cron, sets the snapshot's `background` flag beside `Idle` (RT-84); any status change, a stop, a park or an exit clears it, and it is never written to the sidecar. A `Notification` `idle_prompt` does not leave `AwaitingInput` while a hook-reported prompt is pending.

An owner key sent to a hook-driven session with a pending prompt clears `AwaitingInput` and `pending_input` at once (a `SendInput` marked `automatic`, the client's query replies, wheel and pointer-motion reports, never does), and a status watcher write (a screen read or the shell classifier) that moves a session off `AwaitingInput` clears `pending_input` in the same update.

Token and cost figures exist only for headless runs, where they come from the CLI's stream-json (`crates/agents/claude/src/lib.rs`, `parse_headless_line`, applied by `crates/daemon/src/headless.rs`), so an interactive session's snapshot carries none. Headless mode (`claude --print --output-format stream-json`) uses `crates/daemon/src/headless.rs` for structured events.

**Multi-repo workspace sessions** spawn one `claude` process with `cwd = members[0].worktree_path` and `--add-dir <path>` for every additional member. Worktrees are created/reused per member via `git -C <repo> worktree add` under the shared `<worktrees-root>/wt.<branch-slug>/<workspace-slug>/` directory (see "Where things live on disk" — the layout preserves inter-member relative paths within a workspace).

The "reuse-or-create" policy is encoded in `workspace.rs`. VS Code `.code-workspace` files are auto-detected when adding a repo and surfaced as a `VscodeWorkspaceSuggestion` event.

**Tracer-backed PTY sessions (Phase C.3).** Every interactive and plain-shell PTY child is spawned under a per-session `rt-tracer.exe` supervisor process. The daemon talks to it over an `interprocess` local socket (`tracer_protocol::socket_name`: `\\.\pipe\rt-tracer-<session-id>` on Windows, a Unix socket elsewhere).

The tracer owns the master ConPTY handle and survives daemon restarts — when the daemon dies the tracer keeps draining child output to its internal ring buffer (4 MB cap, oldest-bytes-drop on overflow), and a freshly-started daemon reattaches via `tracer_client::reattach` and replays the ring to catch up.

Spawn site: `crates/daemon/src/tracer_client.rs::spawn`. Tracer ABI: `crates/tracer-protocol/src/lib.rs` — frozen surface; additive changes (new fields with `#[serde(default)]`, new variants with `#[serde(other)] Unknown`) are not a bump. Headless (`claude --print`) does NOT go through the tracer — it's piped stdio and lives in `crates/daemon/src/headless.rs`. The tracer binary must be present next to `rustling-tulipd.exe`: `cargo build` emits both into `target/<profile>/`.

An interactive Claude session's tracer child is the user's shell (`RUSTLING_TULIP_SHELL`, else the platform's usual shell), not `claude`; the tracer is unchanged and does not know which program its child is. Once the prompt is quiet the daemon clears the input line and types `<cd to the start folder> && <quoted claude command line>` (`server/spawn/shell.rs` plans and quotes it per shell family, `inject.rs` `run_chain` types it).

The initial prompt or a preset's script is typed into Claude's TUI only after Claude is up (`wait_for_agent`, `pty_state::agent_up`), and dropped after 15 s with the warn "the agent did not start; the initial prompt was not typed".

A shell outside pwsh, powershell, cmd, sh, bash, zsh, dash and ksh (and WSL bash), a cmd line carrying `%` or `!`, and legacy PowerShell's argument passing host no agent: Claude is then the PTY child, as it is for a plugin without `shell_resume`, and `daemon.log` says which fallback ran ("the agent's command line cannot be typed into this shell; spawning the agent directly", "the agent cannot be typed into this shell; spawning the agent directly", "no shell to host the agent; spawning the agent directly").

A hosted launch logs "spawn_interactive_session: the agent is typed into the shell".

**Live git watcher** (`crates/daemon/src/git_watch.rs`). Each registered repo gets a recursive `notify`-based watcher with a 750 ms debounce. Inside the debouncer callback, `classify_event` filters paths against a hand-maintained allowlist: only `.git/index`, `.git/HEAD`, `.git/refs/**`, a handful of in-progress operation markers, and any non-excluded working-tree path can wake the refresher.

`.git/objects/`, `.git/logs/HEAD`, `FETCH_HEAD`, lock files, and well-known build/cache dirs (`target/`, `node_modules/`, `dist/`, `.next/`, `.venv/`, `__pycache__/`, …) are ignored so a `cargo build` or `pnpm install` doesn't spin up `git status` forever.

The refresher tracks two flags — `status` and `stash` — and only invokes `git stash list` when a stash ref actually changed. The refresher parks while `Hub.client_count` is 0 (an RAII `ClientCountGuard` in `client_session` maintains the count); the next reconnect triggers one catch-up `repo_status` + `stash_list` before resuming event-driven refresh. The daemon accepts multiple WS clients, so each client window opens its own connection.

## Wire-protocol gotchas

- All binary payloads (PTY input/output, scrollback) cross the wire as `data_b64`. Don't add raw-bytes fields.
- The `Hello` message must be the first thing a client sends after WS upgrade. New clients send both `protocol_version` (scalar back-compat) and `protocol_versions: Vec<u32>`. The daemon picks the highest mutually supported version from its `SUPPORTED_PROTOCOL_VERSIONS` const and echoes it in `Welcome.protocol_version`. An empty intersection or token mismatch closes the connection.

  `Hello.auth_token` is the daemon's `daemon.json` token on a loopback connection; on a tailnet connection the daemon never compares it to that token, and the value is the device's own token. A tailnet connection has 10 s to send `Hello`; the loopback listener keeps the un-timed handshake. A tailnet connection also gets a WebSocket Close when the listener server that accepted it ends (remote access turned off or its port changed, among others); loopback connections never do.

  A tailnet `Hello` whose token is not a valid device token (none, unknown, another node's, revoked, the daemon's own, or an unreadable store) is not an `AuthFailed`: it gets `Welcome`, then `DevicePairFailed { reason: "device not approved" }`, and stays open as a pairing connection.

  That connection accepts only `DevicePairRequest` / `DevicePairCancel` / `DevicePairRedeem` (anything else gets the same `DevicePairFailed`), and `ListPendingDevices`, `ApproveDevice`, `DenyDevice`, `MintDevicePairCode`, `ListDevices`, `RevokeDevice` and `ForgetDevice` work on loopback only (a tailnet connection gets `Error "device approval is local-only"` for `ForgetDevice`).

  `RevokeDevice` blocks the device and closes every served tailnet connection of it with a WebSocket Close, also when the flag write fails (the reply is then an `Error`). A blocked device's `DevicePairRequest` and redeem get `DevicePairFailed { reason: "device not approved" }`, and `ApproveDevice` for its request gets `Error "this device is blocked; forget it before approving it again"`; only `ForgetDevice` clears a block, answered with `Devices`. Every `Devices` reply carries `DeviceInfo.connected`, the device's served connection count.

  `Welcome.features` is the daemon's capability list: a client keeps unknown strings and asks only about ones it knows (`Connection::supports`), and a feature is listed only once the daemon fully honours it (`DAEMON_FEATURES` in `crates/daemon/src/server/handshake.rs`).
- On connect the daemon sends `Welcome`, then `Agents` (the plugins' descriptors), `AgentSettings`, `Repos`, then `Sessions`; a client can rely on the descriptors being known before the first snapshot. `agent` is an `AgentId` string everywhere on the wire, and `None` for a plain shell.
- Unknown message types (forward-compat path) hit `InboundClientMessage::Unknown` in the daemon and `InboundDaemonMessage::Unknown` in the native client (`crates/protocol/src/lib.rs`); the native client's `apps/native/src/net.rs` logs a `warn!` with the type tag and keeps reading (a frame that fails to decode at all is logged as an `error!`). Neither side closes the connection.
- `ClientMessage::Resize` carries `claim: bool` (`#[serde(default)]`, absent means false). The daemon ignores an unclaimed `Resize` for a session a client has already sized (`SessionRecord.size_set`, in memory, never in the sidecar) and applies a claimed one or the first for a session nobody has sized; an ignored one logs at debug `resize ignored: unclaimed and the session is already sized`. A client sets `claim` true when the pane had input in the last 5 s or when the client itself just resized the pane (its window, a split divider, the sidebar dragged, folded or unfolded, or its own font change), and false otherwise: an attach, a re-bind, a reconnect, a tab switch, a focus change, or another client's layout or appearance change (`docs/native-client.md`, "Size and replies").
- `SessionSnapshot` is the canonical session shape — daemon emits `Sessions` (list), `SessionUpdated` (single), `SessionRemoved` (id only). Don't add ad-hoc session-shaped messages elsewhere.
- A client that needs to know which reply answers its request sets the optional `request_id` on `SpawnSession` / `DuplicateSession` / `LoadScrollback` / `SetSessionAppearance`.

  The daemon echoes it only on the reply it sends to that requester (`SessionUpdated` or `Scrollback`, `CheckoutConfirmRequired` when an in-place spawn needs a checkout confirm, or `Error` / `ActionFailed` on failure). The git reads `RepoStatus` / `ListStashes` / `ListCommits` / `GetCommit` / `GetRemoteUrl` take one too; it comes back only on the `Error` that answers a failed read.

  The spawn previews `PreviewSpawn` / `PreviewWorkspaceSpawn` take one as well; it comes back on their `SpawnPreview` / `WorkspaceSpawnPreview` reply, or on the `Error` when the preview fails. Broadcasts never carry it. A spawn's, a duplicate's and an appearance change's `SessionUpdated` echo travels on the requester's ordered session-event stream (`SessionEvent::Updated` carries its origin connection), so the requester gets one copy, carrying the id, and it never overtakes an older broadcast.

  A connection whose session-event stream lags is sent a fresh `Sessions` list, then its spawn replies again with their `request_id`; one whose tab-event stream lags is sent its client's fresh `Tabs` list. A `Scrollback` with `forwarder_restarted: true` means the old forwarder has stopped and the new one starts after this reply, so output a client held back before it is already in the history.

  The tab-layout requests `CreateTab` / `MergeTabs` / `MovePane` / `ExtractToNewTab` / `SplitPane` / `ClosePane` / `SwapPanes` / `ReorderTabs` take one too; it comes back only on the `Error` that refuses the request (success is a broadcast `TabUpdated` or `TabRemoved`, which carries none), and a refusal without one still gets `Error { request_id: None }`. `ListSessionHistory`, `RecoverSessions` and `ListEnvNames` echo theirs on their `SessionHistory`, `RecoverResult` and `EnvNames` replies.

  `SuggestBranchName` echoes its id on `BranchNameSuggestion`, and on the `Error` that answers a failed suggestion.

## Style and lints

Workspace `Cargo.toml` enforces clippy pedantic + denies on `unwrap_used`, `panic`, `dbg_macro`, `todo`, `print_*`, `exit`, etc. Use `tracing::{error,warn,info,debug}` instead of `println!`. Use `expect_used = "warn"` — prefer `?` and `anyhow::Context`. Non-test code has no `.expect()` allowance; the `#[expect(clippy::expect_used, reason = "...")]` attributes that exist are on test modules.

Rust edition 2024 on the `stable` channel (`rust-toolchain.toml` pins the channel, not a version; let-chains and other current features are in use). Profile `release` uses `lto = "thin"`, `codegen-units = 1`, `strip = true`.

## Plan files

Every task is a Linear issue in team RT, grouped into one project per wave; the rules are in `~/.claude/docs/plan-operations.md`, and this repo's project order is in the `rustling-tulip-nextup` profile. `docs/plans/MAIN.md` is a generated snapshot of it: never edit it by hand; change Linear and regenerate it.

A task, idea or steer that comes up mid-task gets an **add** first. New designs go in `docs/plans/*.md` with `- [ ]` checklists for their steps and are linked from their issue's description or their project's content.

Once nothing open links a design file, its durable decisions are promoted into `docs/` (`architecture.md`, `native-client.md`, or a doc of their own) and the file is deleted in that commit. `docs/plans/archive/` holds designs finished before the plan moved to Linear, kept for their rationale; nothing new moves there. A native feature also ticks the lines it delivered in `docs/plans/native-client-parity.md`.

## Things that are deferred / not implemented

Don't go looking for these — they're explicitly out of scope until their plan item comes up:

- **Auto-update** for the native client — deferred until its installer (Phase 6) and a signed release pipeline exist.
- **Code signing / notarization** of the native client's installer (Phase 6) and the binaries inside it — there is no signing cert, so an unsigned bundle trips SmartScreen.
- **Sub-agent / Task-tool interception**, **attach from outside the tailnet** (internet exposure, SSH tunnelling, a relay), **cloud sync** — explicit non-goals; the tailnet listener is the one way in from another machine (`docs/architecture.md` "Non-goals", `docs/plans/remote-access.md`). Viewing a subagent's transcript read-only is planned (Wave 10, "Subagent streams"; design in `docs/plans/subagent-streams.md`). The mobile app is planned (`docs/plans/mobile-app.md`) but not started.
