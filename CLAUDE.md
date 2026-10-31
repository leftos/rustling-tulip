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
tools/e2e/fake-ssh/     fake `ssh` shim (checks the client's argv, then runs `rustling-tulipd bridge` locally) used by the live tier's SSH specs
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
.\rt.ps1 clippy           # the agent seam check (tools/check-agent-names.ps1) first, then the strict lint pass (-D warnings)
.\rt.ps1 fmt              # cargo fmt --all
.\rt.ps1 clean            # cargo clean
.\rt.ps1 native           # build daemon + tracer, then run the native client (extra args: a session id to focus)
.\rt.ps1 native-e2e       # native client specs (e2e_live, e2e_recover, e2e_remote, e2e_ssh) against a real daemon, built with the e2e-loopback-peers feature, isolated under .tmp/ (fake-claude, fake-tailscale and fake-ssh need node); then the sealed-secret spec again on this PC in its own gate call (it needs Credential Manager and skips when GATE_BOX_JOB is set; .gate/local.toml keeps its -SealedSecretOnly re-entry here)
.\rt.ps1 native-smoke     # launch the native client exe in a cloaked, never-focused window; check it connects and takes keys
.\rt.ps1 native-shot main # PNG of the client window over a fake daemon (views: main, main-compact, source-control, diff, settings, settings-claude, spawn, viewer, viewer-md, dashboard, connections, connections-ssh, menu, update, chat, summary, themes, themes-edit) in .tmp\shots\
.\rt.ps1 installer        # release build, then the NSIS installer at target\installer\rustling-tulip-<version>-setup.exe and its .sha256 beside it, which the app's updater checks a download against (needs NSIS 3; tools/installer/README.md)
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
# e2e_live.rs, e2e_recover.rs, e2e_remote.rs, e2e_ssh.rs and smoke_window.rs are #[ignore]d here; run them through rt.ps1 native-e2e / native-smoke
```

Tests run with cargo-nextest although the full suite measured 1.41x slower than `cargo test`, because the daemon's git-fixture tests (RT-70) dominate it. `.config/nextest.toml` sets fail-fast off, no retries, a 60 s slow-timeout that kills a test still running at 300 s (a hang guard; it fails as TIMEOUT) and a `serial-live` test group that runs `e2e_live`, `e2e_recover`, `e2e_remote`, `e2e_ssh` and `smoke_window` one at a time, with a 120 s period killed at 720 s. The protocol, tracer-protocol and daemon-client crates set `doctest = false`.

On a paused tokio clock (`#[tokio::test(start_paused = true)]`) a `timeout` or `wait_until` deadline is a count of timer steps, not a hang guard: tokio skips idle virtual time in microseconds of real time, so any real OS call on the awaited path (a socket bind, a file read) that fails for a few milliseconds fails the test. A test whose wait crosses real I/O runs on the real clock with short intervals and waits on the event, its deadline a hang guard that fails as `hung`; only tests that assert a virtual schedule stay paused, with the OS kept off their path.

The repo is in `test mode: stable`: every landed change's last gate runs the full suite (`cargo nextest run --workspace --cargo-profile test-release`). The **test map**, `tools/test-map.json` (keys `central`, `ignore`, `rows` of source paths to nextest filtersets), still drives `.\rt.ps1 test -Reached` for a quick run while iterating.

The release step is cutting a versioned release from the Linear release pipeline's `vNext`, only after a green full suite (procedure: `docs/release.md`); `v0.1.0-alpha` was the first.

Every cargo command `rt.ps1` runs goes through the repo's gate wrapper, `pwsh -NoProfile -File tools/gate.ps1 run …` (`Invoke-Gated`, logging to `.tmp/rt-<name>.log`), so a bare `.\rt.ps1 test` routes its nextest run to a builder machine; inside a gate's command (`GATE_SLOT_HELD` set by a local gate, `GATE_BOX_JOB` by a routed run on a builder; `Test-UnderGate`) it runs them directly, and the native client the `launch` verbs open holds no slot.

Nothing calls `gate.exe` directly. The wrapper hands the call to the installed gate.exe (installed from the gate repo with `pwsh tools/install.ps1`), or runs the command directly on a machine without one.

`rustling-tulipd pair` prints a one-time device pairing code from the running daemon (it needs the daemon already running, reads `daemon.json` and takes no instance lock; `crates/daemon/src/cli_pair.rs`); the owner types the code on a device whose pairing request is waiting.

`rustling-tulipd shutdown` stops the running daemon the way the client retires one: it sends `/shutdown` only to a listener that has proven it holds the token, leaves every tracer alive, creates no folders and starts no log (`crates/daemon/src/cli_shutdown.rs`, `daemon_client::shutdown_in`); the installer runs it on install and uninstall. The `/proof` key context and the `/proof` and `/shutdown` routes are therefore a contract with every released daemon.

`rustling-tulipd apply-update --from <staging> --to <dir> [--old-version <v>] [--update] [--relaunch]` is what the installer runs from the staged daemon to swap the installed files (`crates/daemon/src/cli_apply_update.rs`, `docs/auto-update.md`): it stops the daemon, renames each file to `.old` and the staged one into place, rolls every file back on a failure, writes `last-result.json`, and with `--relaunch` starts the client.

Its exit codes: 0 applied, 1 bad arguments, 2 the daemon would not stop, 3 an exe still runs from the install folder, 4 another update holds the lock, 5 a file failed and was rolled back. `rustling-tulipd --version` prints the daemon's version.

`rustling-tulipd bridge` is what an SSH client runs on the host: it relays one compact JSON protocol message per line between its stdin and stdout and the loopback daemon's WebSocket, starting the daemon first when none runs (`daemon_client::ensure_running`) and filling the `Hello` token from the host's handshake so the token never leaves the host (`crates/daemon/src/cli_bridge.rs`).

It exits 0 when the daemon closes the WebSocket, and after stdin EOF once the daemon has closed or 5 s have passed (it relays replies still in flight and sends its Close only then, since the daemon drops queued replies once it reads one); a daemon it starts on Windows inherits none of its standard handles. On failure it prints one stderr line `rustling-tulipd bridge: <error>`, leaves stdout empty and exits 1. It starts no tracing, takes no instance lock, rotates no log, creates no folders and attaches no console.

`rustling-tulipd screen <session id> [--fixture <path>]` shows how the running daemon reads one session's screen (it needs the daemon already running and proves the token like `pair`; `crates/daemon/src/cli_screen.rs`, the daemon's side in `crates/daemon/src/server/screen.rs`). It prints one line per rule (`<priority>  <name>  <region>  matched|vetoed by guard N|no match  <preview>`), `winner: <name> (<verdict>)` or `winner: none (no_match)`, any `rejected:` lines, `title: <title>` when there is one, then `grid <cols>x<rows>:` and the rows.

With `--fixture` it then writes the trimmed rows (LF) to exactly that path, refusing an existing file, and prints `wrote <path>`. It refuses at once against a daemon whose `Welcome.features` lack `screen_dump`, and on any failure prints one stderr line `rustling-tulipd screen: <error>` and exits 1.

The native client auto-spawns the daemon on first connect via `daemon_client::ensure_running` (`crates/daemon-client`), passing the protocol versions it speaks. The spawn breaks away from the caller's job; when that job forbids breakaway it launches the daemon through WMI `Win32_Process.Create` with the caller's full environment, outside the job (`wmi_launch.rs`, bounded at 30 s), and only if that fails starts it inside the job, logging which path ran. The daemon writes `port` + `auth_token` + `pid` + `supported_versions` + `build_version` to `daemon.json` in the config dir below; clients read that to connect.

A daemon sharing none of the client's versions is retired and replaced. The native client speaks only the current version, and the daemon's `supported` (`protocol-version.json`) lists only the current version too.

A daemon whose `build_version` is older than the client's own (a missing one counts as older) is retired and replaced too, with the reason `older build`; one whose `build_version` is newer is never retired, and `ensure_running` fails with `DaemonIsNewer` ("daemon is newer … update the client") instead.

## E2E tests

The native client has three test tiers:

- **UI specs** — `cargo nextest run -p rustling-tulip-native --cargo-profile test-release --test ui` (filter by module: `ui_terminal`, `ui_sidebar`, `ui_tabs`, `ui_session_actions`, …): in-process GPUI against a scripted fake daemon; no real window, input or daemon.

  Every spec file is a module of the one `tests/ui/main.rs` binary (a new spec is `tests/ui/ui_<name>.rs` plus its `mod` line there), since linking and starting a binary per file cost more than the tests themselves; the crate's lib has `doctest = false`.
- **Live e2e** — `.\rt.ps1 native-e2e`: builds daemon + tracer in test-release and runs `tests/e2e_live.rs`, `tests/e2e_recover.rs`, `tests/e2e_remote.rs` and `tests/e2e_ssh.rs` under `nextest --run-ignored only` against a real daemon isolated under `.tmp/`; `e2e_remote` drives the daemon's tailnet listener as a tailnet client (pairing and approval, token reconnect, owner rule, refused commands, revoke, remote file fetch, remote access off). Needs `node` for the fake-claude, fake-tailscale and fake-ssh shims.

  `e2e_ssh` drives the native client's SSH target through the fake ssh shim to a real `rustling-tulipd bridge` (connect and list sessions, typed input, a fetched file under `remote-files/ssh-devbox/…`, reconnect after the ssh process is killed, the bridge starting a stopped daemon in the same isolated dirs, a refused login showing ssh's stderr).

  The daemon it runs is built with the off-by-default `e2e-loopback-peers` Cargo feature, which makes the tailnet listener admit loopback peers (judged by the real owner rule) and logs a startup warning "built with e2e-loopback-peers: the tailnet listener admits loopback peers; never ship this build"; only `native-e2e` builds with it, and the installer build refuses it.

  Every live daemon gets `RUSTLING_TULIP_TAILSCALE` (the fake-tailscale shim) and `RUSTLING_TULIP_TAILNET_DIR` (`<root>/tailnet`), so no spec reaches the real Tailscale or the real certificate dir.
- **OS smoke** — `.\rt.ps1 native-smoke`: builds daemon + tracer in test-release and launches the native client exe in a cloaked window that never takes focus, under `nextest --run-ignored only` (`tests/smoke_window.rs`) and checks it connects and that posted keys reach the shell. `native-smoke` takes no test filter, and a failed spec keeps its dir under `.tmp/native-e2e/<test>-<pid>/`, whose `sessions/<id>/scrollback.bin` is the exact PTY output.

To look at a UI change, run `.\rt.ps1 native-shot <view>` (`main`, `main-compact` for the sidebar's Compact leaves, `source-control`, `diff`, `settings`, `settings-claude`, `spawn`, `viewer`, `viewer-md`, `dashboard`, `connections` for the host picker with a saved tailnet and SSH host, the saved tailnet host "box" showing "Not paired" and "3 sessions last time", `connections-ssh` for its `+ Add host` form switched to SSH after a passing Test connection, `menu` for the sessions panel's ⋯ menu open over the repo rows with a toast over the panes, `update` for the footer's `Update {version} ready` notice, `chat` for a session pane in Chat mode with its chat drawn, `summary` for one in Summary mode with a line open under a pinned permission card, `themes` for the Themes modal over Settings, on the row after the theme in use, with a user theme, a broken file and a file named after a built-in, `themes-edit` for the theme editor on a duplicate of Nord): it writes a PNG of the real client window over a scripted fake daemon (`apps/native/examples/shot.rs`) to `.tmp\shots\` and prints its path (it draws Nord, the default theme; the UI specs pin Contrast in the harness); Read the PNG, and show the user before and after shots for any visual choice.

The `crates/agents/claude/fake/` shim (`fake-claude.cmd` or `fake-claude.sh`, plus `index.mjs`) replaces the real CLI in the e2e tier. The plugin returns it from `Agent::fake_cli` (`agent_api::FakeCli`, feature `test-support`), and the live harness (`apps/native/tests/support/live.rs`) sets each registered agent's override variable (`program_env_var()`, `RUSTLING_TULIP_CLAUDE` for Claude) to its fake through `agent-registry`, so the daemon path-resolves the CLI from that variable at spawn time.

It also points the plugin's home variable at `<root>/<agent id>-home` and puts the plugin's stub name first on `PATH`; a registered plugin with no fake panics.

The Claude fake plays the chat mirror's side of a session too. It echoes the draft typed or pasted into the prompt box as `❯ <text>`, writes a transcript to `<CLAUDE_CONFIG_DIR>/projects/<encoded cwd>/<id>.jsonl` (the id from `--session-id` or `--resume`, nothing without either), and runs a turn for any submitted line that is not blank or a `/` command: a `user` record and `UserPromptSubmit`, the spinner, the answer, a `turn_duration` record, `Stop` and the idle box.

The cue `rt-perm` in a message makes that turn write a `Bash` `tool_use` record, run `PermissionRequest` and draw the permission dialog, where a bare `1` or `2` allows (the `tool_result`, `PostToolUse`, the answer) and `3` denies. The input lines `/rt-hook prompt`, `/rt-hook ask` and `/rt-hook bg` run hooks for an open turn, an AskUserQuestion dialog and a `Stop` listing a running shell (`crates/agents/claude/fake/index.mjs`, its README).

The `tools/e2e/fake-tailscale/` shim (`fake-tailscale.cmd`, `.sh` and `index.mjs`, see its README) answers `whoami`, `whois`, `status` and `cert` from the JSON state file named by `RUSTLING_TULIP_FAKE_TAILSCALE_STATE`; it is wired in through `RUSTLING_TULIP_TAILSCALE`.

The `tools/e2e/fake-ssh/` shim (`fake-ssh.cmd`, `.sh` and `index.mjs`, see its README) stands in for `ssh` through `RUSTLING_TULIP_SSH`: it checks the client's argv shape (`-T`, `-o BatchMode=yes`, `--` directly before the host, one remote command starting with `rustling-tulipd`), appends `{pid, argv}` to a log, and runs `rustling-tulipd bridge` with the test daemon's isolated environment.

Its JSON state file, named by `RUSTLING_TULIP_FAKE_SSH_STATE`, holds `bridge`, `env`, `fail` and `log`; a `fail` entry makes it print that stderr and exit with that code, and an unexpected argv or remote command exits 2 with `fake-ssh: unexpected argv: <json>` or `fake-ssh: unexpected remote command: <json>`.

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
| `RUSTLING_TULIP_SSH` | Native client: path to the `ssh` binary it runs for an SSH host; an empty value counts as unset | `ssh` (PATH lookup), else `%SystemRoot%\System32\OpenSSH\ssh.exe` |
| `RUSTLING_TULIP_UPDATE_FEED` | Native client: base URL of the releases feed the updater reads, the `/repos/leftos/rustling-tulip/releases` path being appended (tests point it at a local server); a dev build checks for updates only when it is set (`apps/native/src/update.rs`, `docs/auto-update.md`) | `https://api.github.com` |
| `RUSTLING_TULIP_UPDATES_DIR` | Native client: download root for update installers and `last-result.json`; a non-empty value is used as given, an empty one counts as unset | `%LOCALAPPDATA%\leftos\rustling-tulip\data\updates\` |
| `RUSTLING_TULIP_OFFSCREEN_WINDOW` | Native client: open the window cloaked and never activate it (the smoke tier sets it) | unset |

## Where things live on disk

Both sides resolve the config dir via the `directories` crate as `ProjectDirs::from("dev", "leftos", "rustling-tulip").config_dir()`. On Windows that expands to `%APPDATA%\leftos\rustling-tulip\config\` (note the `leftos\` + `\config\` segments — `directories` inserts them, so a plain `%APPDATA%\rustling-tulip\` path is wrong). Layout under that root:

- `state.json` — persisted repos + workspaces + tabs, plus daemon-side host settings that must apply with no window open (see `crates/daemon/src/state.rs`).

  They are `worktrees_root_override`, `keep_awake_mode`: `"off"`, `"when_busy"` (the default) or `"always"`, `agent_commands`: `{"<agent serde name>": {"program", "args"}}` for the Settings launch command, only agents that set one, a blank pair removing the entry, `agent_settings`: `{"<agent id>": {<option>: <value>}}`, each agent's validated Settings page values.
- `state.json.corrupt-<UTC stamp>` — a `state.json` that failed to parse, moved aside (or its bytes written beside it) before the daemon starts on defaults; the 3 newest are kept, and the secret sweep counts the `${secret:<id>}` references a text scan finds in each, so no sealed value they name is deleted (`crates/daemon/src/state.rs` `keep_corrupt_state`, `crates/daemon/src/main.rs`).
- `daemon.json` — handshake (port + auth_token + pid + supported_versions + build_version, the daemon's package version); written on daemon start, removed on graceful shutdown.
- `daemon-id` — this daemon's stable id, a bare UUID with no newline, sent in `Welcome.daemon_id`. Created on the first start; a file that is empty or not a UUID is replaced with a new id (warn "the daemon id file is empty or not a UUID; replacing it with a new id"), and one that cannot be read or written fails startup with "loading the daemon id" (`crates/daemon/src/daemon_id.rs`, `paths.rs` `daemon_id_file`).
- `sessions/<id>/meta.json` + `scrollback.bin` — orphan-recovery sidecar and PTY scrollback ring. `hook-settings.json` sits beside them for an interactive Claude spawn (passed as `--settings`) and for a recovered shell with a hook agent and a conversation id; a graceful stop deletes it with the sidecar.

  `summary.json` sits beside `meta.json` too: the session's summary ring as a protocol `SessionSummary` whose `lines` is the whole ring (at most 12 lines, none older than 2 hours, newest first). It is written atomically right after each ring update and only while `meta.json` exists (it never creates the folder), restored by the orphan, reattached and abandoned inserts (a missing, corrupt or emptied ring restores no summary), and deleted with the sidecar (`crates/daemon/src/summarizer/turns.rs`, `crates/daemon/src/session.rs`).

  The sidecar's `program_name` follows the record (the shell's label once an agent typed into its shell has ended, the agent again on its next `SessionStart`), and `agent_in_shell` and `shell_label` mark a session whose PTY child is the user's shell, so a reattach runs the shell's status watcher (absent means the agent is the child).

  A recovered shell's `hook_agent` (the plugin that maps its hook events; absent means none, and the hook route then maps nothing for the session), `hook_options` (the recovered selection's options, an empty object by default) and `hook_trusted` (whether the recovered run was trusted, false by default) are stored there too, written once at spawn, and restored by the orphan, reattached and abandoned inserts; the shell's hook events are mapped with them, not with its own spawn config.
- `remote.json` — opt-in tailnet access switch `{enabled, port}`, absent until remote access is turned on; the daemon writes it (temp file and rename) whenever `ConfigureRemote` changes it, and an identical request writes nothing; a missing, unreadable or zero-port file means off, and the port defaults to 8787 (`crates/daemon/src/remote_config.rs`).
- `history/<id>.json` — session history: one entry per ended session, holding how it ended, its spawn config and folders, its `busy_excluded` flag when true, its `agent` (absent for a shell) with the `options` and `trusted` flag an import recorded, and its conversation id `conversation_id`. A tracer-log import infers a DeepSeek `provider` option from its `--model`.

  For a shell-hosted agent it reads the agent note in the tracer log instead, giving the entry that agent with its options, trust, extra folders and conversation id, and falls back to the plain-shell entry when the agent is unregistered or the note unreadable.

  An entry is written on every end path, written at startup for each dead non-headless sidecar that has none (a session a reboot, a crash or a killed process tree ended: `TracerLost`, or its true end when the tracer log shows one), backfilled at startup from `logs/tracer-*.log`, and pruned after 7 days (`crates/daemon/src/history.rs`).

  The recover dialog lists these and `RecoverSessions` respawns them: an agent entry with its plugin's resume (Claude: `claude --resume`), as `RecoverAs::Agent`, `RegisterRepoThenAgent`, `ShellResume` (a shell typing the resume) or `Shell` (a bare shell).
- `contested/<agent id>.json` — the conversation ids that discovery left unclaimed because two or more sessions waiting in one folder were each eligible for them: a JSON array of `{id, contested_at}`, one file per agent, written atomically when an id becomes contested, entries older than 7 days dropped, the directory created on first write; a stored id is never claimed (`crates/daemon/src/conversations/contested.rs`, `Dirs::contested_dir`).
- `activity/session-<id>.jsonl` — the status-change log the Dashboard's two-hour activity track reads: one `{at, status}` line per status change and per insert, lines older than 2 hours pruned at start and every 60 s (the newest older line kept, so the status at the window's start is known), a file deleted once its session has no sidecar and its newest line is 2 hours old. `activity/daemon.json` holds this daemon's `started_at`, an `alive_at` stamped every 60 s and the restart gaps inside the window (`crates/daemon/src/activity_log.rs`, `Dirs::activity_dir`).
- `env-secrets.json` — index of the secret env-row values sealed into Windows Credential Manager (`{id, key, created_at}`, never a value); the values are Credential Manager entries under the service `rustling-tulip` (`rustling-tulip:<8 hex of the config dir's SHA-256>` while `RUSTLING_TULIP_CONFIG_DIR` is set), user `env/<KEY>/<id>`. Stored configs carry `${secret:<id>}` references instead (`crates/daemon/src/env_secrets.rs`, `docs/env-secrets.md`).
- `state.json`'s `summarizer` block — `{enabled, provider, key_source, daily_cap_usd}`, secret-free and absent until first saved; the defaults are built at read time (the first registered CLI summarizer, enabled, a $1.00 cap, the `stored` key source) (`crates/daemon/src/state.rs`).
- `summarizer-spend.json` — the summarizer's spend ledger, `{"date": "YYYY-MM-DD", "spent_usd": n}` for the current local day, written atomically (`crates/daemon/src/summarizer/ledger.rs`, `Dirs`).
- The Dashboard summarizer's API keys are Credential Manager entries under the same service naming, one per provider, user `summarizer/<provider id>`; they have no index entry (`crates/daemon/src/summarizer/keys.rs`, `docs/plans/dashboard.md`).
- Device tokens (native client, one per remote host) are Credential Manager entries under the same service naming, user `device/<lower-cased host>` (no port); a host with no entry is dialled with an empty token (`crates/daemon-client/src/device_tokens.rs`, `CredentialManagerTokens`).
- `devices.json` — the tailnet devices approved to connect: their SHA-256 token hashes, never a token, with each device's `last_seen` and its `revoked` flag (a blocked device); `ForgetDevice` removes a record (`crates/daemon/src/devices.rs`, `DeviceStore`).
- `daemon.lock` — the single-instance lock; a second daemon on the same config dir exits without touching anything.
- `logs/daemon.log` — daemon tracing output. Rotated on each daemon start: the previous run survives as `daemon.log.old`; while it runs, the file rolls to `daemon.log.1` each time it would pass 5 MiB (`daemon_client::capped_log`), and the tracer-log import reads all three (see `crates/daemon/src/main.rs::init_tracing`).
- `logs/autostart.log` — the login launch's short log: `rustling-tulipd --detach` (the command the HKCU `Run` value `rustling-tulip-daemon` starts at login, written by `daemon_client::autostart`; the client only adds `--detach` to an entry that already names its own daemon and never repoints one) copies itself into the binary cache, starts that copy and exits (`crates/daemon/src/detach.rs`). Truncated on each such launch.
- `logs/native.log` — native client (`apps/native`) tracing output, also mirrored to stderr (on Windows only a debug build has a console to show it: a build without debug assertions is a GUI-subsystem exe, so an installed launch opens no console window). Rotated on each launch to `native.log.old`, and rolled to `native.log.1` each time it would pass 5 MiB while the client runs.
- Outside the config dir, the installed native client (the installer's `uninstall.exe` beside the exe) writes its toast registration at each start: the Start menu shortcut `%APPDATA%\Microsoft\Windows\Start Menu\Programs\rustling-tulip.lnk` (app id `leftos.RustlingTulip`, activator CLSID) and the registry key `HKCU\Software\Classes\CLSID\{354BE9DC-6D82-4269-8883-D13CE3E38648}\LocalServer32` (`"<exe>" -ToastActivated`); the uninstaller deletes the key (`apps/native/src/toast_activation.rs`).
- `themes/<slug>.json` — the native client's user themes, one per file (`{name, base?, terminal_source?, ui, terminal}`, every colour present); `ThemeStore::save` writes `.<slug>.json.tmp` and renames it into place and never overwrites an existing slug; the selected one is `native-ui.json`'s `theme`, and the boot saves "My colours" there from a released file's app colours.

  A `*.json` there whose name is not a valid id, or is a built-in theme's id, is listed with an error and left unread, and a failed save removes its `.<slug>.json.tmp` (`apps/native/src/theme_store.rs`, `docs/native-client.md` "Theme and fonts").
- `client-id-native` — per-install client identity (a bare UUID) that the native client sends in `Hello`; tab layouts are keyed by it.
- `native-ui.json` — native client UI state that stays on this machine (`apps/native/src/sidebar.rs`). It holds the sidebar width, the sidebar's collapsed flag and the collapsed containers (a host group's fold too, as `host:local` or `host:<host key>`), the Repos view's Active filter (`repo_filter`, `"all"` default or `"active"`; a missing key or unknown value loads as `"all"`), the active tab, the quick-shell folder, the folder Add repo opens at (`last_repo_dir`) and the folder the workspace creator's `.code-workspace` picker opens at (`last_vscode_dir`).

  It also holds the app-level terminal font, the interface font (`ui_font_family`, absent for the bundled Inter), the interface text size (`ui_text_size`, a whole number from −2 to +4, absent means 0, out of range clamps on load, a non-whole number loads as 0 with a warning), the interface weight (`ui_font_weight`: 400, 500 or 600, absent means 400, anything else loads as 400 with a warning), the theme (`theme`: a built-in's id or a user theme's file slug, absent or null for the default built-in, Nord; read at boot, written by the Themes modal's Use), `app_appearance` (the app accent and terminal background that releases up to v0.5.2-alpha saved; read only so the boot can move them into a user theme with `theme_store::migrate_app_colors`, absent once moved), the shared list of recent custom colours, per-tab font sizes, the activity rail's view (`activity`), and the source-control panel's pinned repo, collapsed sections and split heights.

  It holds the diff tabs' include-whitespace and highlight toggles, whether a Markdown viewer tab opens rendered (`viewer_markdown_rendered`, default true; a missing key loads as true), and the per-host remote session counts (`host_session_counts`, `{"<lower-cased MagicDNS name>": <count>}`, or `"ssh:<host string>"` for an SSH host, only for hosts in `saved_hosts`; kept from each saved remote host's own session list, for a switched-off host's "N sessions last time").

  It holds the open pop-out windows in open order (`popouts`, each `{"key": {"kind": "pane"|"tab"|"session", …ids}, "window": <the window shape, or null until first moved>}`; a session key also carries `"host": "{host key}"` for a remote host's session and omits it for This PC; restored once per launch once the tabs and sessions arrive; a missing key or a non-list value loads as empty, and an entry whose key does not decode is dropped).

  It holds the Settings modal's Spawn default (`spawn` holds only the trusted-launch default; Claude's approval mode and provider are the daemon's `agent_settings`, not stored here), General, Notifications and App title choices (`general`, `notifications`, `title`).

  General includes `general.check_updates` (default on; off stops the updater's automatic checks and downloads), the sidebar's leaf density (`general.leaf_density`), the sessions it draws under repos (`general.sessions_under_repos`: `"every"` default, `"busy"`, `"none"`), how it sorts repos (`general.repo_sort`: `"needs_you"` default, `"active"`, `"manual"`; a missing key or unknown value loads as `"needs_you"`), and `general.open_text_externally`, default off.

  Notifications holds the `Needs you` and `Finished` toggles (`notifications.awaiting_input` and `notifications.finished`) and the skip choice.

  Its `saved_hosts` list (the hosts the host manager offers) is what the client connects at launch: This PC, then each enabled saved host in saved order. The host manager writes it (adding or picking a host saves it on, a row's switch sets its `enabled`, and Forget rewrites the list).

  Each entry has a `kind`, `"tailnet"` when missing: a tailnet entry is `{name, port}` (written without `kind`; one that fails the DNS-name check is dropped on load), an SSH entry is `{"kind": "ssh", "name": "<host string>", "remote_command": "<optional>"}` with no port. An entry may carry `"enabled": false` for a host switched off; only a disabled host writes the key, and a missing key means enabled. An unknown kind, or an SSH host string that fails its check, is dropped on load. The SSH add form (`Add and connect`) writes an SSH entry (with no port), re-adding a name replaces the whole record, and Forget removes it.

  Its `session_views` map holds each session's remembered pane view per host: `{"<host key>": {"<session id>": "chat" | "summary"}}`, the host key `"local"` for this PC, else the key `host_session_counts` uses; only chat and summary entries are written (a terminal is the default), a host with none is left out, and a save prunes only the connected host's ended sessions, once its session list has arrived (a missing or malformed value loads as empty, with a warning for a wrong type).

  Its `unc_hosts` list (network hosts terminal links may open) is edited by hand; the client re-reads it on each click and never writes it. The native client's bundled OFL fonts live in `apps/native/assets/fonts/` (sources and versions in its README), and the large-file hook exempts them. The attribution for the built-in themes' source colour schemes is `apps/native/assets/themes/NOTICE.md`.

When debugging spawn/connect/shutdown issues, both `daemon.log` and the client's log (`native.log`) together tell the full story — neither alone is enough.

Worktrees live under a **separate** root resolved via `ProjectDirs::data_local_dir().join("worktrees")` (overridable with `RUSTLING_TULIP_WORKTREES_DIR`). On Windows that's `%LOCALAPPDATA%\leftos\rustling-tulip\data\worktrees\` — machine-local, doesn't roam, and known-writable regardless of where the source repo lives.

Per-session worktree paths are `<worktrees-root>/wt.<branch-slug>/<repo-slug>` for a single repo and `<worktrees-root>/wt.<branch-slug>/<workspace-slug>/<offset>` for a workspace member, where the **anchor** is the common path-component prefix of the member repos' *parents* on `members[0]`'s drive and `<offset>` is each member's path below it.

This preserves inter-member relative paths inside a workspace session — if `repo1` references `repo2` as `../repo2` in source space, the same reference resolves inside the worktree set.

Members on another drive are anchored per drive under `<workspace-slug>/<drive anchor>/<offset>`; that folder's first segment takes a `-2`, `-3`, … suffix when the plain name would equal a primary-drive member's path or contain one (`…\ws\Y-2\b`). Slugs come from `git::name_slug`; repos and workspaces share one slug namespace, and saving a name whose slug is taken is refused.

A group folder has a `<slug>.rt-group` marker beside it (`{"kind","name"}`), which Manage worktrees reads to list each group separately; a group's members are its linked worktrees, a member nested inside another included (a submodule or plain clone inside one is not listed), and a workspace marker makes a one-member group launch as that workspace.

A spawn reuses the worktree git reports as holding the branch, skipping one git marks prunable (its folder is gone): that stale registration alone is removed with `git worktree remove --force` before the add or a Recreate, and a spawn never runs `git worktree prune`, which would also drop a worktree on a detached drive (any layout, so older `wt.<branch>/<sanitized-anchor>/…` worktrees are still reused) and refuses a folder at its path that isn't one of the repo's worktrees. Path construction lives in `git::workspace_worktree_paths` / `git::single_worktree_path`.

The tailnet listener's certificate lives in a third machine-local root beside the worktrees one, `ProjectDirs::data_local_dir().join("tailnet")` (overridable with `RUSTLING_TULIP_TAILNET_DIR`), `%LOCALAPPDATA%\leftos\rustling-tulip\data\tailnet\` on Windows: `<name>.crt` and `<name>.key`, written by `tailscale cert` for the host's MagicDNS name (no trailing dot).

It is not under the config dir, so the private key never roams (`crates/daemon/src/paths.rs` `resolve_tailnet_dir`, `crates/daemon/src/tailnet_listener.rs`).

Files the native client fetches from a remote host live in a fourth machine-local root beside those, `daemon_client::remote_files_dir()` (overridable with `RUSTLING_TULIP_REMOTE_FILES_DIR`), `%LOCALAPPDATA%\leftos\rustling-tulip\data\remote-files\` on Windows: `<host MagicDNS name, lower-cased>/<repo-name>/<rel-path>`, with Windows-unsafe names sanitised; an SSH host's folder is `ssh-<host string, lower-cased and sanitised>` (`ssh-me@devbox_2222`).

Auto-update downloads live in a machine-local root beside the others, `%LOCALAPPDATA%\leftos\rustling-tulip\data\updates\` on Windows (overridable with `RUSTLING_TULIP_UPDATES_DIR`): one `<version>-<random>\` folder per download, holding the verified installer, and `last-result.json` (`{from, message, ok, to}`), which `rustling-tulipd apply-update` writes and the client reads once after the restart.

Older folders and the `.old` files an update left in the install folder are deleted at the next launch (`apps/native/src/update.rs`, `docs/auto-update.md`).

The Dashboard summarizer runs the agent's CLI in a fifth machine-local root beside those, `ProjectDirs::data_local_dir().join("summarizer")` (no override variable), `%LOCALAPPDATA%\leftos\rustling-tulip\data\summarizer\` on Windows: nothing lives there but the CLI's working folder, which is kept empty and never emptied by the daemon. It is created on first use (`crates/daemon/src/paths.rs` `resolve_summarizer_dir`, `crates/daemon/src/summarizer/cli.rs`).

A fetch writes a hidden `.fetch-<launch stamp>-<n>.part` beside the target and renames it over the target when the file is done; a failed or cancelled fetch deletes its part. Nothing under it is ever pruned (`apps/native/src/file_fetch.rs`).

A fetched `.html` or `.htm` page that opens when done brings the relative media it names with it, saved at the same `<host>/<repo-name>/<rel-path>` place (never outside the repo root) and each fetched again if already on disk (`apps/native/src/html_media.rs`, `docs/native-client.md` "Fetched files").

## Architecture invariants

**Single source of truth for the wire protocol.** All daemon/client messages live in `crates/protocol/src/lib.rs` as tagged enums (`#[serde(tag = "type", rename_all = "snake_case")]`).

When adding a message, update both directions (`ClientMessage` + `DaemonMessage` if it's a request/response) and the matching handler arm in the domain module's `handle` under `crates/daemon/src/server/` plus its variant in `dispatch`'s or-pattern in `crates/daemon/src/server.rs` and the native client's handling (`RootView`'s message folds, `on_message` in `apps/native/src/lib.rs`, and chat messages fold in `apps/native/src/chat_view.rs` (`fold_chat_or_agents`); `net.rs` only forwards). Changes stay additive where they can (see "When to bump" below).

**App code never names an agent.** The daemon, protocol, native client and tracer ask the registry and the plugin's descriptor; agent-specific behaviour lives in `crates/agents/<agent>/`. `tools/check-agent-names.ps1` (a prek hook and the first step of `.\rt.ps1 clippy`) fails on `claude`, `anthropic` or `deepseek` elsewhere, apart from test code and the hits listed in `tools/agent-names-allow.txt`.

**Saved user data survives every update.** The app is released and used by more than its owner. A change to the shape of anything a user set or saved (`native-ui.json`, `state.json`, presets, themes, history, `devices.json`, `remote.json`, the Credential Manager entries) still reads the shape of every release users may still run, `v0.5.1-alpha` and later: a renamed or removed field is migrated on load to where its value now lives, never dropped, and the change carries a test that loads a file in the oldest such release's shape. Data no released build wrote (a format added since the last release tag) needs no migration.

Runtime state may be lost on an update across a breaking change: live sessions over a tracer-ABI or sidecar break, scrollback, activity logs and summary rings. The release notes say so when a release breaks them (`docs/auto-update.md`).

**When to bump `protocol-version.json`.** *Additive* changes (new variant on a tagged enum, new `#[serde(default)]` field on a struct, new message type) are NOT a protocol bump. The range-based handshake (`SUPPORTED_PROTOCOL_VERSIONS`) + the `InboundClientMessage::Unknown` / `InboundDaemonMessage::Unknown` parse wrappers absorb unknown top-level types.

Nested enums that grow over time (`TabLayout`, `RearrangeLayout`, `InjectorStep`, `PresetVariableKind`, `TabContent`) each carry a `#[serde(other)] Unknown` unit variant that absorbs unrecognized values *in place* — the containing message keeps decoding. New nested enums should follow the same `#[serde(other)]` pattern from day one.

*Breaking* changes — renaming a field, removing a variant, changing semantics — DO require a bump. The installer updates the client and its local daemon together, and the handshake refuses a remote host that shares no version with the client, so a bump sets `version` to the new number and `supported` to only the versions the new daemon can serve, normally just the new one (`[25]`, not `[25, 24]`).

Before every breaking bump, verify that `daemon_client::ensure_running` retires the old healthy daemon through the HTTP `/shutdown` path and spawns the new daemon, so tracer-backed sessions reattach instead of leaving the client stuck at `auth_failed` (`crates/daemon-client/src/supervisor.rs`: `classify_existing_daemon`, `retire_daemon`, and the test `native_client_retires_daemon_that_speaks_only_an_older_version`).

**On-disk layout under `%APPDATA%\rustling-tulip\`** (see `crates/daemon/src/paths.rs`):
- `state.json` — repo + workspace registry plus host settings (never sessions)
- `daemon.json` — handshake (port, token, pid, supported protocol versions, build version)
- `daemon-id` — the daemon's stable UUID, written atomically on first start
- `sessions/<id>/meta.json` — orphan-recovery sidecar; written at spawn before the session's first snapshot is sent (so a session any client has seen survives a daemon crash), deleted on graceful stop
- `sessions/<id>/hook-settings.json` — the hooks document an interactive Claude spawn passes as `--settings` (a recovered shell's `shell_resume` command names its path); written at spawn, deleted on a graceful stop
- `sessions/<id>/summary.json` — the session's summary ring (a protocol `SessionSummary`); written atomically after each ring update while `meta.json` exists, deleted with the sidecar
- `sessions/<id>/scrollback.bin` — 2 MB ring (trims to about 1.5 MB on overflow, cut at the next line start within 4 KiB, off the async workers), held open by the session's scrollback gate while it runs and replayed via `LoadScrollback` on attach
- `sessions/<id>/scrollback.truncated` — flag file iff the ring overflowed

Every temp-file-and-rename write in the daemon (sidecars, `state.json`, `daemon.json`, the indexes and ledgers above) goes through `atomic_file::write_atomic`: an owner-only temp file unique per call, flushed with `sync_all` before `std::fs::rename` replaces the target (retried briefly while another process holds it), so a power cut leaves the old bytes or the new ones, never a torn file.

Sessions are deliberately **not** in `state.json` — they're rebuilt from sidecars on startup so the daemon can survive restarts without a single fragile state blob.

**Orphan recovery + tracer reattach (Phase C.3).** On startup, `main.rs` reads all `meta.json` sidecars and partitions live vs dead via `orphan::is_session_alive` (sysinfo: a tracer-backed sidecar's `tracer_pid` must still be an `rt-tracer` process whose start time equals the sidecar's `tracer_started_at`, stamped at spawn, so a reused pid is neither reattached nor killed (a sidecar without the stamp, from an older build or an unreadable start time, falls back to the name check); a sidecar without tracer fields matches its `pid` against its `program_name`, which otherwise just names the program the user runs, the leaf's label).

For sidecars with `tracer_pid` + `tracer_pipe` set (post-C.3 spawns), `server::recovery::reattach_orphans` (`crates/daemon/src/server/recovery.rs`) connects to the still-running `rt-tracer.exe` over its named pipe and rebuilds a fully-functional `PtyHandle` — IO, resize, and kill all work as if the session had been freshly spawned.

The sidecar also keeps the session's `busy_excluded` flag (absent means false), kept current on every registry update and restored on reattach, and its last `status`, `status_since` and `status_seq` (the snapshot's status sequence), rewritten on every status change. A reattached session comes back `Idle` or `AwaitingInput` with its old stamp and sequence, and any other stored status comes back `Idle` stamped now with the sequence raised by one.

Its status watcher then reads the replayed screen once its startup hold ends: a visible spinner makes it `Working`, an idle prompt box with no dialog clears `AwaitingInput`, and a screen no rule matches leaves the restored status.

A hook-reported prompt's `pending_input` is written beside `status` (absent when nothing is pending) and restored only with a restored `AwaitingInput`. Every sidecar write goes through `SessionRegistry::sync_sidecar`, which writes the record's mirrored fields under one lock. Reattach failures route the session to the abandoned bucket (sidebar Resume button, B.2 UX).

A reattached interactive session with no conversation id, whose plugin discovers its conversation after launch, gets its discovery waiter and poller again once every orphan has reattached and every abandoned sidecar is inserted, so a sibling's held id is never claimed (`reattach_baseline`, `restart_discovery` in `server/recovery.rs`).

A session that ended before then gets none. In the restarted discovery, launch time is the session's start time, the baseline is empty and the folder is the one it started in, so a conversation created while no daemon watched is still found.

A session whose exit arrives while Windows is logging off or shutting down (`GetSystemMetrics(SM_SHUTTINGDOWN)`, `host_shutdown.rs`) and that the user did not stop is not ended: the daemon writes no history entry, marks it abandoned, takes a final look for its conversation and keeps its sidecar with `ended_during_host_shutdown` set, so the next start records it as `TracerLost` whatever its exit code. A sidecar whose tracer is gone is recorded as ended at start (see the history entry above), and once the daemon has bound and written its handshake, `recovery::auto_resume` resumes the qualifying ones by itself, one at a time, with no prompt, off switch or retry (`claude --resume <that id>`), then broadcasts `ResumeReport`.

A qualifying one is an interactive Claude session with a spawn config, its own conversation id and an existing transcript, its worktrees still pinned to their folders, its in-place checkout still on its branch. The rest stay abandoned, listed in Recover.

Pre-C.3 sidecars without tracer fields fall through to the read-only `insert_orphan` path; their underlying children almost never survive a daemon death, so that branch is effectively only for downgrade scenarios.

**A Claude session's status comes from its hooks and from rules over its rendered screen** (`pty_state.rs`, `screen.rs`, `screen/rules.rs`; design in `docs/plans/screen-status.md`).

The watcher feeds each session's PTY output into a per-session grid (`alacritty_terminal`, the crate the client renders with) and reads it with the session's own agent's rules (`screen/rules.rs`: `CompiledRules` compiles the plugin's `ScreenRules`, `rules_for(agent)` caches one set per agent; Claude's table is data in `crates/agents/claude/src/rules.rs`) a short debounce after output pauses.

A rule table fails closed: one bad rule (a structural problem, an unknown part, an empty pattern, over 128 rules, a pattern over 512 characters, over 8 guards, a regex that does not compile) refuses the whole table (`ScreenRules::validate` in agent-api, `CompiledRules::try_compile`), the daemon logs one error "screen rules: the agent's table is refused; its screen has no rules", and that agent's screen reading sets no status. To see why a live session reads as it does, run `rustling-tulipd screen <session id>`: it prints the rule trace (`CompiledRules::explain`) and the grid.

There is no output/input volume classifier, no title heartbeat and no silence demotion for a Claude session: a quiet screen that still shows the spinner stays `Working`.

A session turns hook-driven on its first mapped hook event that passes the ordering check, from Claude Code's hooks, injected through the settings file its spawn writes. A hook event applies at once. A session whose hooks never run is screen-driven only.

A screen-derived change publishes at once when it enters `AwaitingInput` from a dialog footer or when a spinner line promotes the session to `Working`. Leaving `AwaitingInput` and `Working` to `Idle` publish only after hysteresis: 3 agreeing checks at least 150 ms apart, or the first agreeing check 700 ms after the first.

Every screen write is refused when a hook event, the owner's key or an exit moved the record (status, hook stamp or hook-driven flag) since the read. After any such foreign write, and when the watcher starts, the screen is left unread for 700 ms. A reading from the title alone, an overlay or a screen no rule matches never writes.

A plain shell keeps its own classifier (command output outpacing echo promotes to `Working`, `SHELL_IDLE_AFTER` of silence ends it) until it is hook-driven or its screen has shown Claude for a whole confirmation window; it then runs the same screen check. Once the shell's prompt has been back on the grid's last row for that window, `claude` has ended: the shell leaves hook-driven mode (clearing `background` and ending a `Working` or `AwaitingInput` turn at `Idle`) and returns to its classifier.

An interactive Claude session whose agent is typed into the user's shell (`agent_in_shell`; see "Tracer-backed PTY sessions") runs that same shell watcher. The daemon announces the agent's end once per run with `DaemonMessage::AgentExited { session_id }` on the session-event stream: on its `SessionEnd` hook (a `/clear` reason is not an exit) or, when no hook said so, when `hand_shell_back` sees the prompt return.

The record's `program_name` becomes the shell's label then and `claude` again on the next `SessionStart`; the session itself ends only when the shell exits.

A subagent's hook event (an `agent_id` in its input) never ends or replaces the main agent's pending prompt and never promotes the session to `Working`, and a subagent `PermissionRequest` raises one only when none is pending.

A main-agent `Stop` whose `background_tasks` lists a running task, or whose `session_crons` holds a one-shot cron, sets the snapshot's `background` flag beside `Idle` (RT-84); any status change, a stop, a park or an exit clears it, and it is never written to the sidecar.

The screen sets it too, with no hook: while the record is `Idle` and the screen body reads `Idle`, a `· N shell(s)` item on the mode line under the prompt box or a running-subagent row in the panel (the Claude table's `background_shell` and `background_subagents` rules, verdict `background`, read apart from the state rules by `CompiledRules::read_background`) sets the flag after the same confirmation window as a state change. The screen clears only a flag it set itself, so a hook-set flag survives a footer-less frame, and after a reattach the flag comes back from the replayed screen.

A `Notification` `idle_prompt` does not leave `AwaitingInput` while a hook-reported prompt is pending.

**The chat mirror serves a Claude session's chat from its transcript and its screen, and types into its PTY** (`chat_mirror.rs`, `transcript_tail.rs`, `server/chat.rs`, the plugin's `chat_entries.rs` and `dialogs.rs`; design in `docs/architecture.md` "Chat mirror").

The terminal stays the source of truth and each fact has one source: history from the tailed transcript, the open dialog with its numbered options and the composer state from the status watcher's screen grid read by the plugin's dialog parsers, live answer text from the MessageDisplay hook, and "a prompt is pending" from `pending_input`. No fact falls back to a second source, and the mirror builds no second grid.

A chat message or answer is typed into the PTY as keys under the session's typing lane, gated on the settled screen, and refused (`busy`, `chat_refused`, `invalid_request`) rather than queued by the daemon. A recipe runs group by group with a screen check after each and stops when the screen does not move, the owner types or 30 s pass. The chat never resizes the PTY, and a send or answer is never an owner key.

The running-work strip (`ChatWork`) and the summary view (`Summary`, `SummaryAppend`) ride on the same loaders. Summary lines are written by the Dashboard's summarizer service through its queue, cache and daily cap, and stay template lines (the turn's first user line, its duration and tool count) when no model text comes back. Only the Claude plugin supplies the mirror's parts, so a session of any other agent is refused a chat with `unsupported_message`.

The status watcher also returns the terminal to the main conversation for every Claude session when an agent is in view and a Heads up notice is up, an AskUserQuestion closes, or the box holds the notice's own draft (`view_drive.rs`; design in `docs/architecture.md` "Chat mirror"). It types its keys straight into the PTY, never as owner input: one key per settled read, at most 12 keys and 10 s per drive, no key while a dialog is up. A draft in the agent's box is stashed with ctrl+s only when no stash is held (the stash is one slot), restored in the agent's view afterwards or returned to main's box, and clients hear of it through `DaemonMessage::SessionNotice` on the session-event stream. Every multi-key writer to a PTY (this view switch, the spawn injector, a chat send) takes the session's typing lane (`typing_lane.rs`), and the watcher skips its view return while another writer holds it; the user's own keys never take the lane.

An owner key sent to a hook-driven session with a pending prompt clears `AwaitingInput` and `pending_input` at once (a `SendInput` marked `automatic`, the client's query replies, wheel and pointer-motion reports, never does), and a status watcher write (a screen read or the shell classifier) that moves a session off `AwaitingInput` clears `pending_input` in the same update.

Token and cost figures exist only for headless runs, where they come from the CLI's stream-json (`crates/agents/claude/src/lib.rs`, `parse_headless_line`, applied by `crates/daemon/src/headless.rs`), so an interactive session's snapshot carries none. Headless mode (`claude --print --output-format stream-json`) uses `crates/daemon/src/headless.rs` for structured events.

**Multi-repo workspace sessions** spawn one `claude` process with `cwd = members[0].worktree_path` and `--add-dir <path>` for every additional member. Worktrees are created/reused per member via `git -C <repo> worktree add` under the shared `<worktrees-root>/wt.<branch-slug>/<workspace-slug>/` directory (see "Where things live on disk" — the layout preserves inter-member relative paths within a workspace).

The "reuse-or-create" policy is encoded in `workspace.rs`. VS Code `.code-workspace` files are auto-detected when adding a repo and surfaced as a `VscodeWorkspaceSuggestion` event.

**Tracer-backed PTY sessions (Phase C.3).** Every interactive and plain-shell PTY child is spawned under a per-session `rt-tracer.exe` supervisor process. The daemon talks to it over an `interprocess` local socket (`tracer_protocol::socket_name`: `\\.\pipe\rt-tracer-<session-id>` on Windows, a Unix socket elsewhere).

The tracer owns the master ConPTY handle and survives daemon restarts — when the daemon dies the tracer keeps draining child output to its internal ring buffer (4 MB cap, oldest-bytes-drop on overflow), and a freshly-started daemon reattaches via `tracer_client::reattach` and replays the ring to catch up.

Spawn site: `crates/daemon/src/tracer_client.rs::spawn`. Tracer ABI: `crates/tracer-protocol/src/lib.rs` — frozen surface; additive changes (new fields with `#[serde(default)]`, new variants with `#[serde(other)] Unknown`) are not a bump. Headless (`claude --print`) does NOT go through the tracer — it's piped stdio and lives in `crates/daemon/src/headless.rs`. The tracer binary must be present next to `rustling-tulipd.exe`: `cargo build` emits both into `target/<profile>/`.

An interactive Claude session's tracer child is the user's shell (`RUSTLING_TULIP_SHELL`, else the platform's usual shell), not `claude`; the tracer never runs or knows the agent, and only logs an optional `--agent-note <json>` (given before the program, for an agent typed into the shell and never for env rows) as the line `rt-tracer agent note agent_note=<json>` after `rt-tracer starting`, which the tracer-log import reads (`server/spawn.rs` `shell_agent_note`, `tracer_log.rs`).

Once the prompt is quiet the daemon clears the input line and types `<cd to the start folder> && <quoted claude command line>` (`server/spawn/shell.rs` plans and quotes it per shell family, `inject.rs` `run_chain` types it).

The initial prompt or a preset's script is typed into Claude's TUI only after Claude is up (`wait_for_agent`, `pty_state::agent_up`), and dropped after 15 s with the warn "the agent did not start; the initial prompt was not typed".

A shell outside pwsh, powershell, cmd, sh, bash, zsh, dash and ksh (and WSL bash), a cmd line carrying `%` or `!`, and legacy PowerShell's argument passing host no agent: Claude is then the PTY child, as it is for a plugin without `shell_resume`.

So does an agent that runs through cmd.exe (a `.cmd` or `.bat` shim, or cmd.exe itself) typed into pwsh, powershell or a POSIX shell when an argument holds `%`, `!`, `"`, `^`, `&`, `|`, `<`, `>`, CR or LF; the direct spawn then refuses any such argument with a `Couldn't start session` failure naming the character and the argument, before the tracer starts (`direct_args` in `server/spawn/shell.rs`).

`daemon.log` says which fallback ran ("the agent's command line cannot be typed into this shell; spawning the agent directly", "the agent cannot be typed into this shell; spawning the agent directly", "no shell to host the agent; spawning the agent directly").

A hosted launch logs "spawn_interactive_session: the agent is typed into the shell".

**Live git watcher** (`crates/daemon/src/git_watch.rs`). Each registered repo gets a recursive `notify`-based watcher with a 750 ms debounce. Inside the debouncer callback, `classify_event` filters paths against a hand-maintained allowlist: only `.git/index`, `.git/HEAD`, `.git/refs/**`, `.git/reftable/**` (a reftable-backend repo's refs; it wakes the stash refresh too), a handful of in-progress operation markers, and any non-excluded working-tree path can wake the refresher.

`.git/objects/`, `.git/logs/HEAD`, `FETCH_HEAD`, lock files, and well-known build/cache dirs (`target/`, `node_modules/`, `dist/`, `.next/`, `.venv/`, `__pycache__/`, …) are ignored so a `cargo build` or `pnpm install` doesn't spin up `git status` forever.

The refresher tracks two flags — `status` and `stash` — and only invokes `git stash list` when a stash ref actually changed. The refresher parks while `Hub.client_count` is 0 (an RAII `ClientCountGuard` in `client_session` maintains the count); the next reconnect triggers one catch-up `repo_status` + `stash_list` before resuming event-driven refresh. The daemon accepts multiple WS clients, so each client window opens its own connection.

## Wire-protocol gotchas

- All binary payloads (PTY input/output, scrollback) cross the wire as `data_b64`. Don't add raw-bytes fields.
- The `Hello` message must be the first thing a client sends after WS upgrade. New clients send both `protocol_version` (scalar back-compat) and `protocol_versions: Vec<u32>`. The daemon picks the highest mutually supported version from its `SUPPORTED_PROTOCOL_VERSIONS` const and echoes it in `Welcome.protocol_version`. An empty intersection or token mismatch closes the connection.

  `Hello.auth_token` is the daemon's `daemon.json` token on a loopback connection; on a tailnet connection the daemon never compares it to that token, and the value is the device's own token. A tailnet connection has 10 s to send `Hello`; the loopback listener keeps the un-timed handshake. A tailnet connection also gets a WebSocket Close when the listener server that accepted it ends (remote access turned off or its port changed, among others); loopback connections never do. A tailnet connection (served or pairing) is also pinged every 15 s and closed after 3 unanswered pings (no miss counts while one of its requests is being handled), and closed when one frame takes longer than 30 s plus 1 s per 64 KiB to send; a loopback connection, the SSH bridge's included, is never pinged and never has a send timed out.

  A tailnet `Hello` whose token is not a valid device token (none, unknown, another node's, revoked, the daemon's own, or an unreadable store) is not an `AuthFailed`: it gets `Welcome`, then `DevicePairFailed { reason: "device not approved" }`, and stays open as a pairing connection.

  That connection accepts only `DevicePairRequest` / `DevicePairCancel` / `DevicePairRedeem` (anything else gets the same `DevicePairFailed`), and `ListPendingDevices`, `ApproveDevice`, `DenyDevice`, `MintDevicePairCode`, `ListDevices`, `RevokeDevice` and `ForgetDevice` work on loopback only (a tailnet connection gets `Error "device approval is local-only"` for `ForgetDevice`). `DumpScreen` is loopback only too: a tailnet connection gets `Error "screen dump is local-only"`.

  `RevokeDevice` blocks the device and closes every served tailnet connection of it with a WebSocket Close, also when the flag write fails (the reply is then an `Error`).

  A blocked device's `DevicePairRequest` and redeem get `DevicePairFailed { reason: "device not approved" }`, and `ApproveDevice` for its request gets `Error "this device is blocked; forget it before approving it again"`; only `ForgetDevice` clears a block, answered with `Devices`. Every `Devices` reply carries `DeviceInfo.connected`, the device's served connection count.

  `Welcome.features` is the daemon's capability list: a client keeps unknown strings and asks only about ones it knows (`Connection::supports`), and a feature is listed only once the daemon fully honours it (`DAEMON_FEATURES` in `crates/daemon/src/server/handshake.rs`).
- A pane or tab content that names a session on another daemon carries the optional `host` (`GridNode::Pane`, `TabContent::Diff`, and `CreateTab`, `SplitPane`, `ReplacePaneSession`, `OpenDiffTab`; omitted when absent, and absent means this daemon). The daemon stores the string and never interprets it. A local session removal, the startup tab prune and the abandoned-session rebind touch only panes with no `host`, and a diff tab opened with `host` set skips the repo registry check. `ForgetHostSessions { host, session_ids, all, request_id }` closes that host's listed sessions' panes (or all of its panes) in every client layout as a local removal does, and a failed layout write answers `Error` with code `tab_refused` and the `request_id`.
- On connect the daemon sends `Welcome`, then `Agents` (the plugins' descriptors), `AgentSettings`, `Repos`, then `Sessions`; a client can rely on the descriptors being known before the first snapshot. `agent` is an `AgentId` string everywhere on the wire, and `None` for a plain shell.
- Unknown message types (forward-compat path) hit `InboundClientMessage::Unknown` in the daemon and `InboundDaemonMessage::Unknown` in the native client (`crates/protocol/src/lib.rs`); the native client's `apps/native/src/net.rs` logs a `warn!` with the type tag and keeps reading (a frame that fails to decode at all is logged as an `error!`). Neither side closes the connection. A frame the daemon cannot decode that carries a top-level string `request_id` is answered, on loopback, served tailnet and pairing connections alike: `Error { code: "unsupported_message", message: "unsupported message type: <type>" }`, or, when its `type` is known, `Error { code: "invalid_request", message: "invalid <type> message: a field did not decode" }`; one without a `request_id` still gets nothing (logged), and on a pairing connection either frame still counts toward the stray-frame limit.
- Every `DaemonMessage::Error` carries a `code` (`Option<String>`, `#[serde(default)]`), a snake_case string from the catalogue in `crates/protocol/src/error_codes.rs` (`ALL` lists them); a client branches on the code, never on the message text, and keeps a code it does not know as an opaque string. A new refusal picks an existing code or adds one there.
- The chat messages (`crates/daemon/src/chat_mirror.rs`, `crates/daemon/src/server/chat.rs`, design in `docs/architecture.md` "Chat mirror"): `LoadChat` answers `Chat { entries, hidden, screen, request_id }` with the newest 500 entries (`CHAT_REPLY_CAP`), or `Error` `not_found` "unknown session: <id>" or `unsupported_message` "chat is not offered for agent `<id>`"; a missing transcript gives an empty `Chat`. Entries keep the transcript's order. A fresh mirror's first reply is empty: the tail's first read is applied without per-line sends, and when it ends every chat loader gets one unsolicited `Chat` (below) and every summary loader one unsolicited `Summary`. A connection's loader carries a chat flag and a summary flag: chat messages go only to chat loaders, `Summary` and `SummaryAppend` (`crates/daemon/src/summary_lines.rs`, one `StretchLine` per turn or window of a long turn) only to summary loaders, and the mirror lives while any loader holds either.

  `ChatWork` (the running-work strip's subagents, shells and wakeup, built in `crates/daemon/src/chat_work.rs`) goes to every chat or summary loader once: to a new loader right after its reply, to all after the first transcript read, and on each change, never on a timer.

  `ChatAppend { entries, removed }` goes to loaders only; `removed` (serde default) lists ids that leave the chat, so one append carries a transcript answer and the live id it replaces. A live answer is `Answer { id: "live:<message key>" or "live:prompt:<prompt key>", live: true }`, its pieces joined with "\n" and sent at most every 100 ms (the first at once); a live line without a prompt key is not shown, and lines that arrive before the first read ends are held until it does. A user prompt in the transcript ends the live answers of every other prompt (their ids go in `removed` of the append carrying it), and a later piece of an ended prompt is dropped under any message key, held pieces included; a live-line follower that lags reads on from the oldest line its channel still holds. A replacement is sent to a loader only for an entry at or after the first entry its last `Chat` carried (its reply or the latest reset), so a client never gets one for a row it does not hold.

  An unsolicited `Chat` (`request_id: None`, newest 500 and `hidden`) is a full reset, sent when a tail's first read ends (if anything changed), when a restarted transcript has been re-read, and for a new conversation id once the new file's first read ends (at once when it has no transcript path), opening with the system row "Conversation cleared" (id `cleared:<id>`, only when both conversations are known). `ChatScreenChanged` goes to loaders only, on a change beyond the cursor; prompt ids are `p<n>` from one daemon-wide counter and stay while the same dialog is up, `composer: unknown` means no reading yet, and a plain-shell check feeds no chat.

  `ChatAnswer` types a prompt's recipe (`crates/daemon/src/chat_answer.rs`, spawned from `server/chat.rs`) under the session's typing lane, `busy` at once while a send or another answer holds it. A `prompt_id` that is not the mirror's current prompt and the screen's prompt identity is refused `chat_refused` "the prompt is no longer up"; the text is checked (`check_typed_text`, no control character at all) and the recipe built before any key; the terminal returns to main first only while no dialog is up; after each key group the runner waits up to 3 s for a settled reading that holds the step's `ScreenExpect` (a `SameDialog` step needs the dialog visibly changed, an unreadable screen never counts), and no movement, a failed check, an owner key or the 30 s ceiling stops it with no further key.

  Once the answered prompt is gone, `SessionRegistry::clear_answered_prompt_if` (shared with `SendInput`) clears a hook-driven record's `AwaitingInput` and `pending_input` only when the pending prompt's kind matches the answered one; a Heads up never clears, and an answer is never an owner key. "No, and tell Claude this" presses No, waits for the box to be ready, then sends the text under the same lane turn (`inject::type_message_under`) and is refused in an agent's view. Refusals are `chat_refused`, or `invalid_request` for an empty answer or one holding a control character.

  `ChatSend` types the text into the PTY (`inject::type_message`, spawned from `server/chat.rs`) and has no success reply: the text is checked first (`\r\n` becomes `\n`; empty text, C0, DEL or C1 control characters and text over 64 KiB are `invalid_request`), then the send takes the session's typing lane (`busy` at once when a send or answer holds it, else after waiting 250 ms), gates on the settled screen and refuses with `chat_refused` for a pending hook prompt, a draft in the box in any view, a dialog, an overlay, unknown state or an agent ended in its shell.

  An open agent view is driven to main key by key and gated again; the text then goes in as a bracketed paste, and under the record and screen locks the send re-checks (no dialog, paste shown, no owner key since it began, no pending prompt), adds a pending chat entry and presses Enter. The entry is `ChatEntry::User { id: "pending:<n>", pending: true }`, sent to loaders just before the Enter and removed, oldest first by trimmed text, when the transcript's `user` or `queued_command` record arrives (the `ChatAppend` carries its id in `removed`), or by a restart, a conversation switch or the mirror's drop; a `/` or `!` command adds none. A send is never an owner key. `UnloadChat` and a closing connection unload; the tail stops when the last loader leaves, an ended session's chat stays loaded read-only, and only the session's removal drops it. Every send to loaders happens under the one mirror lock, taken inside the screen grid lock and never the reverse; the `connection.rs` guard (`connection_watches`) unloads a connection's chats when it drops.
- `ClientMessage::Resize` carries `claim: bool` (`#[serde(default)]`, absent means false). The daemon ignores an unclaimed `Resize` for a session a client has already sized (`SessionRecord.applied_size`, the cols and rows last applied to the PTY, in memory, never in the sidecar) and applies a claimed one or the first for a session nobody has sized; an ignored one logs at debug `resize ignored: unclaimed and the session is already sized`.

  A claimed one to the applied size is skipped, PTY and screen grid alike, with debug `resize skipped: the session is already at this size`.

  A client sets `claim` true when the pane had input in the last 5 s or when the client itself just resized the pane (its window, a split divider, the sidebar dragged, folded or unfolded, or its own font change), and false otherwise: an attach, a re-bind, a reconnect, a tab switch, a focus change, or another client's layout or appearance change (`docs/native-client.md`, "Size and replies").
- `SessionSnapshot` is the canonical session shape — daemon emits `Sessions` (list), `SessionUpdated` (single), `SessionRemoved` (id only). Don't add ad-hoc session-shaped messages elsewhere. Its `status_seq` (u64, default 0) rises by one on each status change, including a reattach or orphan/abandoned rebuild that changes the stored status.
- A client that needs to know which reply answers its request sets the optional `request_id` on `SpawnSession` / `DuplicateSession` / `LoadScrollback` / `SetSessionAppearance`.

  The daemon echoes it only on the reply it sends to that requester (`SessionUpdated` or `Scrollback`, `CheckoutConfirmRequired` when an in-place spawn needs a checkout confirm, or `Error` / `ActionFailed` on failure). The git reads `RepoStatus` / `ListStashes` / `ListCommits` / `GetCommit` / `GetRemoteUrl` take one too; it comes back only on the `Error` that answers a failed read.

  The spawn previews `PreviewSpawn` / `PreviewWorkspaceSpawn` take one as well; it comes back on their `SpawnPreview` / `WorkspaceSpawnPreview` reply, or on the `Error` when the preview fails. Broadcasts never carry it. A spawn's, a duplicate's and an appearance change's `SessionUpdated` echo travels on the requester's ordered session-event stream (`SessionEvent::Updated` carries its origin connection), so the requester gets one copy, carrying the id, and it never overtakes an older broadcast.

  A connection whose session-event stream lags is sent a fresh `Sessions` list, then its spawn replies again with their `request_id`; one whose tab-event stream lags is sent its client's fresh `Tabs` list. A `Scrollback` with `forwarder_restarted: true` means the old forwarder has stopped and the new one starts after this reply, so output a client held back before it is already in the history. A `Scrollback` with no `request_id` is a resync: when one session's queued output on a connection would pass 4 MiB (`PTY_SESSION_BUDGET`), its forwarder drops the live feed and, once that queue drains, sends one fresh snapshot (or, if the session exited meanwhile, its file scrollback with `forwarder_restarted: false`); the native client drops it while the pane's own load is pending and otherwise starts the pane over from it as a fresh attach.

  The tab-layout requests `CreateTab` / `MergeTabs` / `MovePane` / `ExtractToNewTab` / `SplitPane` / `ClosePane` / `SwapPanes` / `ReorderTabs` take one too; it comes back only on the `Error` that refuses the request (success is a broadcast `TabUpdated` or `TabRemoved`, which carries none), and a refusal without one still gets `Error { request_id: None }`. `ListSessionHistory`, `RecoverSessions` and `ListEnvNames` echo theirs on their `SessionHistory`, `RecoverResult` and `EnvNames` replies. `ListPresets` echoes its id on `Presets` and on the `Error` (code `presets_failed`) that answers a failed list; the native container menu fills its preset rows only from the reply carrying its own id.

  `LoadChat` and `LoadSummary` echo theirs on `Chat` and `Summary` and on the `Error` that refuses the load; `ChatSend` and `ChatAnswer` have no success reply and take one only to name the `Error` that refuses them, which the native chat routes back to the composer or card it answers. Neither `ChatAppend` nor an unsolicited `Chat` or `Summary` carries one.

  `GetSessionDetail` echoes its id on `SessionDetail`, and on the `Error "unknown session: <id>"` that answers a session the facts cache does not hold. `SuggestBranchName` echoes its id on `BranchNameSuggestion`, and on the `Error` that answers a failed suggestion. `SetAgentCommand` and `SetAgentSettings` take one too; it comes back only on the `Error` that refuses the request (success is the `AgentCommands` / `AgentSettings` broadcast, which carries none), and the native Settings modal routes each refusal to the apply it answers, placing a launch-command or worktrees-root refusal by its code (`launch_program_refused`, `launch_args_refused`, `worktrees_root_refused`).

## Style and lints

Workspace `Cargo.toml` enforces clippy pedantic + denies on `unwrap_used`, `panic`, `dbg_macro`, `todo`, `print_*`, `exit`, etc. Use `tracing::{error,warn,info,debug}` instead of `println!`. Use `expect_used = "warn"` — prefer `?` and `anyhow::Context`. Non-test code has no `.expect()` allowance; the `#[expect(clippy::expect_used, reason = "...")]` attributes that exist are on test modules.

Rust edition 2024 on the `stable` channel (`rust-toolchain.toml` pins the channel, not a version; let-chains and other current features are in use). Profile `release` uses `lto = "thin"`, `codegen-units = 1`, `strip = true`.

## Plan files

Every task is a Linear issue in team RT, grouped into one project per wave; the rules are in `~/.claude/docs/plan-operations.md`, and this repo's project order is in the `rustling-tulip-nextup` profile. `docs/plans/MAIN.md` is a generated snapshot of it: never edit it by hand; change Linear and regenerate it.

A task, idea or steer that comes up mid-task gets an **add** first. New designs go in `docs/plans/*.md` with `- [ ]` checklists for their steps and are linked from their issue's description or their project's content.

Once nothing open links a design file, its durable decisions are promoted into `docs/` (`architecture.md`, `native-client.md`, or a doc of their own) and the file is deleted in that commit. `docs/plans/archive/` holds designs finished before the plan moved to Linear, kept for their rationale; nothing new moves there. A native feature also ticks the lines it delivered in `docs/plans/native-client-parity.md`.

## Things that are deferred / not implemented

Don't go looking for these — they're explicitly out of scope until their plan item comes up:

- **Code signing / notarization** of the native client's installer (Phase 6) and the binaries inside it — there is no signing cert, so an unsigned bundle trips SmartScreen.
- **Sub-agent / Task-tool interception**, **attach from outside the tailnet** by internet exposure or a relay, **cloud sync** — explicit non-goals; the tailnet listener is the one way in from another machine over the network, and clients over OpenSSH are being built (`docs/plans/ssh-remote.md`; `docs/architecture.md` "Non-goals", `docs/plans/remote-access.md`). Viewing a subagent's transcript read-only is planned (Wave 10, "Subagent streams"; design in `docs/plans/subagent-streams.md`). The mobile app is planned (`docs/plans/mobile-app.md`) but not started.
