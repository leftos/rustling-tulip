# Testing

The test tiers, the runner's settings, the fake CLIs the live tier uses, and how to look at a UI change. The commands that run them are in [CLAUDE.md](../CLAUDE.md), "Common commands".

## Test runner

Tests run with cargo-nextest although the full suite measured 1.41x slower than `cargo test`, because the daemon's git-fixture tests (RT-70) dominate it. `.config/nextest.toml` sets fail-fast off, no retries, a 60 s slow-timeout that kills a test still running at 300 s (a hang guard; it fails as TIMEOUT) and a `serial-live` test group that runs `e2e_live`, `e2e_recover`, `e2e_remote`, `e2e_ssh`, `e2e_multi_host` and `smoke_window` one at a time, with a 120 s period killed at 720 s. The protocol, tracer-protocol and daemon-client crates set `doctest = false`.

On a paused tokio clock (`#[tokio::test(start_paused = true)]`) a `timeout` or `wait_until` deadline is a count of timer steps, not a hang guard: tokio skips idle virtual time in microseconds of real time, so any real OS call on the awaited path (a socket bind, a file read) that fails for a few milliseconds fails the test. A test whose wait crosses real I/O runs on the real clock with short intervals and waits on the event, its deadline a hang guard that fails as `hung`; only tests that assert a virtual schedule stay paused, with the OS kept off their path.

## Test tiers

The native client has three test tiers, and the driver MCP server has a live spec of its own:

- **UI specs** — `cargo nextest run -p rustling-tulip-native --cargo-profile test-release --test ui` (filter by module: `ui_terminal`, `ui_sidebar`, `ui_tabs`, `ui_session_actions`, …): in-process GPUI against a scripted fake daemon; no real window, input or daemon.

  Every spec file is a module of the one `tests/ui/main.rs` binary (a new spec is `tests/ui/ui_<name>.rs` plus its `mod` line there), since linking and starting a binary per file cost more than the tests themselves; the crate's lib has `doctest = false`.
- **Live e2e** — `.\rt.ps1 native-e2e`: builds daemon + tracer in test-release and runs `tests/e2e_live.rs`, `tests/e2e_recover.rs`, `tests/e2e_remote.rs`, `tests/e2e_ssh.rs` and `tests/e2e_multi_host.rs` under `nextest --run-ignored only` against a real daemon isolated under `.tmp/`; `e2e_remote` drives the daemon's tailnet listener as a tailnet client (pairing and approval, token reconnect, owner rule, refused commands, revoke, remote file fetch, remote access off). Needs `node` for the fake-claude, fake-tailscale and fake-ssh shims.

  `e2e_remote`, `e2e_ssh` and `e2e_multi_host` open the window as production does (`Harness::open_hosts`, `apps/native/tests/support/live.rs`): This PC runs on its own isolated home daemon and the daemon under test is a remote host beside it, on a real net thread of its own. Every daemon a spec starts is its own `LiveDaemon::start(<unique name>)`, since its roots, tracer pipe prefix and binaries dir derive from that name, and a spec has one fake ssh per process, which any ssh alias reaches.

  A live daemon is an `IsolatedDaemon` (`crates/dev-daemon`); a start that fails keeps its scratch folder under `.tmp/native-e2e/`, and a hung start's message ends with `daemon.log`'s last lines.

  `e2e_ssh` drives the native client's SSH host through the fake ssh shim to a real `rustling-tulipd bridge` (connect and list sessions, typed input, a fetched file under `remote-files/ssh-devbox/…`, reconnect after the ssh process is killed, the bridge starting a stopped daemon in the same isolated dirs, a refused login showing ssh's stderr).

  `e2e_multi_host` runs two real isolated daemons in one window: This PC on a home daemon and an SSH host through the fake ssh shim and `rustling-tulipd bridge`.

  Its specs: the host header's `+ Shell` on the remote host runs the shell there and echoes (`plus_shell_on_a_remote_host_runs_there_and_echoes`), a mixed tab runs each pane on its own host (`a_mixed_tab_runs_each_pane_on_its_own_host`), a killed ssh dims only its own host and reconnects (`a_killed_ssh_dims_only_its_host_and_reconnects`). With two ssh aliases reaching one daemon, switching on the earlier-saved alias takes the daemon and moves the pane (`switching_on_the_earlier_alias_takes_the_daemon_and_its_panes`).

  The daemon it runs is built with the off-by-default `e2e-loopback-peers` Cargo feature, which makes the tailnet listener admit loopback peers (judged by the real owner rule) and logs a startup warning "built with e2e-loopback-peers: the tailnet listener admits loopback peers; never ship this build"; only `native-e2e` builds with it, and the installer build refuses it.

  Every live daemon gets `RUSTLING_TULIP_TAILSCALE` (the fake-tailscale shim) and `RUSTLING_TULIP_TAILNET_DIR` (`<root>/tailnet`), so no spec reaches the real Tailscale or the real certificate dir.
- **OS smoke** — `.\rt.ps1 native-smoke`: builds daemon + tracer in test-release and launches the native client exe in a cloaked window that never takes focus, under `nextest --run-ignored only` (`tests/smoke_window.rs`) and checks it connects and that posted keys reach the shell. `native-smoke` takes no test filter, and a failed spec keeps its dir under `.tmp/native-e2e/<test>-<pid>/`, whose `sessions/<id>/scrollback.bin` is the exact PTY output.

The driver MCP server's other specs (`tools/rt-driver-mcp/tests/server.rs`) run in the normal nextest run. Its live spec, `driver_mcp_launch_ping_screenshot_stop`, is `#[ignore]`d: it launches a real driven client and needs the daemon, tracer and driver client built in test-release. `tools/rt-driver-mcp/live-check.ps1` builds them and runs the ignored specs, so run it as one gate job with `--slot heavy`, since it builds: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/<name>.log --timeout 3600 --slot heavy -- pwsh -NoProfile -File tools/rt-driver-mcp/live-check.ps1`.

## Looking at a UI change

To look at a UI change, run `.\rt.ps1 native-shot <view>`: it writes a PNG of the real client window over a scripted fake daemon (`apps/native/examples/shot.rs`) to `.tmp\shots\` and prints its path (it draws Nord, the default theme; the UI specs pin Contrast in the harness). Read the PNG, and show the user before and after shots for any visual choice.

The views:

- `main`, `source-control`, `diff`, `settings`, `settings-claude`, `viewer`, `viewer-md`, `dashboard`: those screens as named; `dashboard` is seeded to the By status view.
- `dashboard-compact`: the Dashboard's Compact view over scripted agents in several repos, a workspace and a folder, at 1920x1080.
- `main-compact`: the sidebar's Compact leaves.
- `settings-dashboard`: Settings on its Dashboard tab with a keyed provider, a stored key, some spend and a Test result.
- `spawn`: the spawn dialog with its Target list open.
- `connections`: the host manager with This PC connected, the saved tailnet host "box" showing "Not paired" with its `Pair` and `Reconnect now` buttons and "3 sessions last time", and a saved SSH host switched Off.
- `connections-ssh`: its `+ Add host` form switched to SSH after a passing Test connection.
- `menu`: the sessions panel's ⋯ menu open over the repo rows with a toast over the panes.
- `update`: the footer's `Update {version} ready` notice.
- `chat`: a session pane in Chat mode with its chat drawn over a two-question prompt card whose options carry descriptions and an open Tail box on the running Bash card.
- `summary`: one in Summary mode with a line open under a pinned permission card.
- `themes`: the Themes modal over Settings, on the row after the theme in use, with a user theme, a broken file and a file named after a built-in.
- `themes-edit`: the theme editor on a duplicate of Nord.
- `multi-host`: the sidebar grouped by host: This PC, "box" online with a repo and a session in the first tab's second pane, and "lab" offline.
- `spawn-hosts`: the spawn dialog over those two hosts with its closed Target field.
- `settings-hosts`: Settings on the Claude page with the host selector showing This PC.
- `settings-sounds`: Settings on the Notifications tab with the Needs you sound list open.

## Fake CLIs

The `crates/agents/claude/fake/` shim (`fake-claude.cmd` or `fake-claude.sh`, plus `index.mjs`) replaces the real CLI in the e2e tier. The plugin returns it from `Agent::fake_cli` (`agent_api::FakeCli`, feature `test-support`), and the live harness (`apps/native/tests/support/live.rs`) sets each registered agent's override variable (`program_env_var()`, `RUSTLING_TULIP_CLAUDE` for Claude) to its fake through `agent-registry`, so the daemon path-resolves the CLI from that variable at spawn time.

It also points the plugin's home variable at `<root>/<agent id>-home` and puts the plugin's stub name first on `PATH`; a registered plugin with no fake panics.

The Claude fake plays the chat mirror's side of a session too. It echoes the draft typed or pasted into the prompt box as `❯ <text>`, writes a transcript to `<CLAUDE_CONFIG_DIR>/projects/<encoded cwd>/<id>.jsonl` (the id from `--session-id` or `--resume`, nothing without either), and runs a turn for any submitted line that is not blank or a `/` command: a `user` record and `UserPromptSubmit`, the spinner, the answer, a `turn_duration` record, `Stop` and the idle box.

The cue `rt-perm` in a message makes that turn write a `Bash` `tool_use` record, run `PermissionRequest` and draw the permission dialog, where a bare `1` or `2` allows (the `tool_result`, `PostToolUse`, the answer) and `3` denies. The input lines `/rt-hook prompt`, `/rt-hook ask` and `/rt-hook bg` run hooks for an open turn, an AskUserQuestion dialog and a `Stop` listing a running shell (`crates/agents/claude/fake/index.mjs`, its README).

The `tools/e2e/fake-tailscale/` shim (`fake-tailscale.cmd`, `.sh` and `index.mjs`, see its README) answers `whoami`, `whois`, `status` and `cert` from the JSON state file named by `RUSTLING_TULIP_FAKE_TAILSCALE_STATE`; it is wired in through `RUSTLING_TULIP_TAILSCALE`.

The `tools/e2e/fake-ssh/` shim (`fake-ssh.cmd`, `.sh` and `index.mjs`, see its README) stands in for `ssh` through `RUSTLING_TULIP_SSH`: it checks the client's argv shape (`-T`, `-o BatchMode=yes`, `--` directly before the host, one remote command starting with `rustling-tulipd`), appends `{pid, argv}` to a log, and runs `rustling-tulipd bridge` with the test daemon's isolated environment.

Its JSON state file, named by `RUSTLING_TULIP_FAKE_SSH_STATE`, holds `bridge`, `env`, `fail` and `log`; a `fail` entry makes it print that stderr and exit with that code, and an unexpected argv or remote command exits 2 with `fake-ssh: unexpected argv: <json>` or `fake-ssh: unexpected remote command: <json>`.
