# Testing

The test tiers, the runner's settings, the fake CLIs the live tier uses, and how to look at a UI change. The commands that run them are in [CLAUDE.md](../CLAUDE.md), "Common commands".

## Test runner

Tests run with cargo-nextest although the full suite measured 1.41x slower than `cargo test`, because the daemon's git-fixture tests (RT-70) dominate it. `.config/nextest.toml` sets fail-fast off, no retries, a 60 s slow-timeout that kills a test still running at 300 s (a hang guard; it fails as TIMEOUT) and a `serial-live` test group that runs `e2e_live`, `e2e_recover`, `e2e_remote`, `e2e_ssh`, `e2e_multi_host`, `e2e_update` and `smoke_window` one at a time, with a 120 s period killed at 720 s. The protocol, tracer-protocol and daemon-client crates set `doctest = false`.

`links` is a private module of the native crate, so probing its predicates (the spaced readings, the stitch rule) takes a verbatim copy of `apps/native/src/links.rs` in a standalone crate under `.tmp`, run with `--local`: a routed run gets no copy of gitignored files.

On a paused tokio clock (`#[tokio::test(start_paused = true)]`) a `timeout` or `wait_until` deadline is a count of timer steps, not a hang guard: tokio skips idle virtual time in microseconds of real time, so any real OS call on the awaited path (a socket bind, a file read) that fails for a few milliseconds fails the test. A test whose wait crosses real I/O runs on the real clock with short intervals and waits on the event, its deadline a hang guard that fails as `hung`; only tests that assert a virtual schedule stay paused, with the OS kept off their path.

## Test tiers

The native client has three test tiers, and the driver MCP server has a live spec of its own:

- **UI specs** — `cargo nextest run -p rustling-tulip-native --cargo-profile test-release --test ui` (filter by module: `ui_terminal`, `ui_sidebar`, `ui_tabs`, `ui_session_actions`, …): in-process GPUI against a scripted fake daemon; no real window, input or daemon.

  The harness (`apps/native/tests/support/mod.rs`) resolves a remote leaf's `leaf-<host key>::<id>` selectors per host (the `leaf_tab_pill_on` and `leaf_buttons_on` hooks). Its `in_model` returns true for any selector without a model hook, such as the pane header buttons (`pane-popout-*`), so a positive check on one needs a click plus an outcome. A click updates `focused_pane` only after `window.activate_window()`, since the test window starts inactive.

  GPUI's `Frame::clear()` does not clear `debug_bounds` (gpui 0.2.2, `src/window.rs`), so within one window a selector drawn once keeps its bounds and stays "painted" in later frames: a UI spec cannot prove an element disappeared after it was drawn. Probe absence on a window that never drew it, or assert the model or state (the focus ring, the request) instead.

  gpui's test platform uses `NoopTextSystem`, which never slices text by its runs, so a UI spec cannot catch a run-boundary fault in text shaping. `apps/native/tests/ellipsis_real_text.rs` can: it runs a `compute_layout` chain in a real window on the real text system, and `.\rt.ps1 native-smoke` runs it beside the smoke specs.

  Every spec file is a module of the one `tests/ui/main.rs` binary (a new spec is `tests/ui/ui_<name>.rs` plus its `mod` line there), since linking and starting a binary per file cost more than the tests themselves; the crate's lib has `doctest = false`.
- **Live e2e** — `.\rt.ps1 native-e2e`: builds daemon + tracer in test-release and runs `tests/e2e_live.rs`, `tests/e2e_recover.rs`, `tests/e2e_remote.rs`, `tests/e2e_ssh.rs`, `tests/e2e_multi_host.rs` and `tests/e2e_update.rs` (a remote update driven through the host-update sandbox: the daemon runs from a scratch install, downloads and checks a release from `UpdateFeed`, and records the installer instead of starting it; and a host telling its connected clients to update, each by its kind) under `nextest --run-ignored only` against a real daemon isolated under `.tmp/`; `e2e_remote` drives the daemon's tailnet listener as a tailnet client (pairing and approval, token reconnect, owner rule, refused commands, revoke, remote file fetch, a folder fetch landing byte-exact (`remote_folder_fetch_lands_the_tree_byte_exact`), remote access off). Needs `node` for the fake-claude, fake-tailscale and fake-ssh shims.

  `e2e_remote`, `e2e_ssh` and `e2e_multi_host` open the window as production does (`Harness::open_hosts`, `apps/native/tests/support/live.rs`): This PC runs on its own isolated home daemon and the daemon under test is a remote host beside it, on a real net thread of its own. Every daemon a spec starts is its own `LiveDaemon::start(<unique name>)`, since its roots, tracer pipe prefix and binaries dir derive from that name, and a spec has one fake ssh per process, which any ssh alias reaches.

  A live daemon is an `IsolatedDaemon` (`crates/dev-daemon`; run elevated, it creates its scratch `config` and `binaries` folders as protected roots, and an existing ordinary one there fails as `could not make <path>: protected root <path> is unsafe: …`); a start that fails keeps its scratch folder under `.tmp/native-e2e/`, and a hung start's message ends with `daemon.log`'s last lines.

  `e2e_ssh` drives the native client's SSH host through the fake ssh shim to a real `rustling-tulipd bridge` (connect and list sessions, typed input, a fetched file under `remote-files/ssh-devbox/…`, a fetched folder under its ssh folder (`a_folder_fetched_over_ssh_lands_under_its_ssh_folder`), reconnect after the ssh process is killed, the bridge starting a stopped daemon in the same isolated dirs, a refused login showing ssh's stderr).

  `e2e_multi_host` runs two real isolated daemons in one window: This PC on a home daemon and an SSH host through the fake ssh shim and `rustling-tulipd bridge`.

  Its specs: the host header's `+ Shell` on the remote host runs the shell there and echoes (`plus_shell_on_a_remote_host_runs_there_and_echoes`), a mixed tab runs each pane on its own host (`a_mixed_tab_runs_each_pane_on_its_own_host`), a killed ssh dims only its own host and reconnects (`a_killed_ssh_dims_only_its_host_and_reconnects`). With two ssh aliases reaching one daemon, switching on the earlier-saved alias takes the daemon and moves the pane (`switching_on_the_earlier_alias_takes_the_daemon_and_its_panes`).

  The daemon it runs is built with the off-by-default `e2e-loopback-peers` Cargo feature, which makes the tailnet listener admit loopback peers (judged by the real owner rule) and logs a startup warning "built with e2e-loopback-peers: the tailnet listener admits loopback peers; never ship this build"; only `native-e2e` builds with it, and the installer build refuses it.

  Every live daemon gets `RUSTLING_TULIP_TAILSCALE` (the fake-tailscale shim) and `RUSTLING_TULIP_TAILNET_DIR` (`<root>/tailnet`), so no spec reaches the real Tailscale or the real certificate dir, and `RUSTLING_TULIP_ACCOUNTS_DIR` (`<root>/accounts`, not created up front: the account store makes it owner-only on first open), so none reaches the user's saved Claude logins, and `RUSTLING_TULIP_PUSH_DIR` (`<root>/push`), so none reaches the user's push keys and subscriptions, and `RUSTLING_TULIP_UPDATES_DIR` (`<root>/updates`), so none cleans or reads the user's downloaded installers or `last-result.json`.

  A remote-update spec points the daemon at `UpdateFeed` (`apps/native/tests/support/update_feed.rs`), a local stand-in for the releases feed serving one release, its installer and the installer's `.sha256`, through `RUSTLING_TULIP_UPDATE_FEED`.

  A daemon built with that feature also honours `RUSTLING_TULIP_ACCOUNTS_BASE_URL`, a loopback http(s) origin the account service's profile, usage and refresh requests go to (anything else is ignored with `RUSTLING_TULIP_ACCOUNTS_BASE_URL is not a loopback http(s) origin; ignored`; when it applies the daemon warns `built with e2e-loopback-peers: the account service's profile, usage and refresh requests go to RUSTLING_TULIP_ACCOUNTS_BASE_URL; never ship this build`). `ProfileServer` (`apps/native/tests/support/profile_server.rs`) is the loopback server a spec points it at: it answers `GET /api/oauth/profile` with uuids derived from the email as the fake CLI derives them, and gives up on a stalled connection after 10 s.

  The login specs in `e2e_live.rs` run the daemon-driven account login against the fake CLI and a `ProfileServer`: a login that succeeds adds the account, a failed one reports why, a cancel leaves no scratch folder. The fake CLI's `FAKE_CLAUDE_LOGIN` knob reaches the login child through the daemon's environment. They are Windows only (they skip elsewhere); run them with `.\rt.ps1 native-e2e -Test e2e_live -Filter 'test(/login/)'`.

  A failed live spec's `daemon.log` and `tracer-<id>.log` stay on the builder under `D:\gate-remote\work\<tree>\.tmp\native-e2e\<test>-<pid>\config\logs\` and can be read over ssh; the gate's own log carries a per-process spawn and exit trace, which shows grandchildren that outlive their parent.
- **OS smoke** — `.\rt.ps1 native-smoke`: builds daemon + tracer in test-release and launches the native client exe, built with `--features driver`, in a cloaked window that never takes focus, under `nextest --run-ignored only` (`tests/smoke_window.rs` and `tests/ellipsis_real_text.rs`) and checks it connects and that posted keys reach the shell. The `driven::` specs launch it as a driven run and prove the driver's posted `click`, `drag`, `wheel` and right-click on the real exe. `native-smoke` takes no test filter and leaves out the live no-focus checks, which have their own verb: `.\rt.ps1 native-drive-check [-HandsOff]` runs only the `live_drive_` specs. `driven::live_drive_leaves_the_foreground_and_cursor_alone` fails unless a scripted drive (click, type_text, key, drag, wheel, screenshot) leaves the desktop's foreground window and cursor where they were, or when the desktop has no foreground window to compare.

  `the_crash_message_box_shows_its_text_and_closes` shows the Windows crash message box from inside the test process and proves its dialog carries the message's text before closing it. It refuses on the interactive desktop, failing at once instead of opening its box there.

  `driven::live_drive_app_toasts_show_on_top_without_focus` launches the driven client with `RUSTLING_TULIP_DRIVER_TOASTS=1`, picks the app's own toasts in Settings (scrolling the page with `wheel` until the choice shows), makes a fake Claude session ask a permission, and fails unless a toast window of the client shows visible, topmost, `WS_EX_NOACTIVATE` and not cloaked, with the foreground window and cursor unmoved. `.gate/desktop.toml` `patterns` routes it to a builder's logged-on desktop; run on this PC it refuses unless `-HandsOff` says you have stopped using the mouse and keyboard. A failed spec of either verb keeps its dir under `.tmp/native-e2e/<test>-<pid>/`, whose `sessions/<id>/scrollback.bin` is the exact PTY output.

The driver MCP server's other specs (`tools/rt-driver-mcp/tests/server.rs`) run in the normal nextest run. Its live spec, `driver_mcp_launch_ping_screenshot_stop`, is `#[ignore]`d: it launches a real driven client and needs the daemon, tracer and driver client built in test-release. `tools/rt-driver-mcp/live-check.ps1` builds them and runs the ignored specs, so run it as one gate job with `--slot heavy`, since it builds: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/<name>.log --timeout 3600 --slot heavy -- pwsh -NoProfile -File tools/rt-driver-mcp/live-check.ps1`.

The phone app (`apps/pwa`) has its own checks, and the full suite runs them: `.\rt.ps1 test` runs `pnpm --dir apps/pwa install --frozen-lockfile`, then `pnpm --dir apps/pwa typecheck`, then `pnpm --dir apps/pwa test` (vitest), after the Rust suites, and fails on the first failing step. A run given a test filter (`.\rt.ps1 test <filter>`) runs only what the filter names and skips the phone app's checks, saying so; a full or reached run without pnpm fails before any suite starts.

A reached run runs the same checks when a selected row in `tools/test-map.json` carries `"pwa": true`: the `apps/pwa/**` row is that flag alone, and the `crates/protocol/**` and `tools/protocol-ts/**` rows carry it beside their filter because a protocol change regenerates `apps/pwa/src/protocol.ts`. `-PrintOnly` prints `reached: phone app checks` for such a selection. The prek hooks lint and format-check the folder through `tools/pwa-hook.ps1`.

## Driving the daemon's CLI from a script

`rustling-tulipd` is a GUI-subsystem program: PowerShell's `&` does not wait for it and loses its output; a redirect from `cmd` keeps it (`crates/daemon/tests/cli_console.rs` runs the exe under `cmd /c` with a redirect). A script that runs its verbs starts it with `System.Diagnostics.ProcessStartInfo` and redirected standard output and error, then reads both and the exit code.

A daemon a client, `bridge` or verb starts inherits none of the caller's handles and cannot hold a remote SSH command's channel open, but a script that starts one over SSH still wraps the local `ssh` in a timeout, so a regression fails the run instead of hanging it.

Measuring an elevated daemon against an ordinary one on a builder needs the real ordinary profile: under a `RUSTLING_TULIP_CONFIG_DIR` override both profiles resolve to one folder. With no daemon running, rename the real ordinary config folder aside, set `RUSTLING_TULIP_BINARIES_DIR` and `RUSTLING_TULIP_WORKTREES_DIR` to scratch folders and `RUSTLING_TULIP_CLAUDE` to a path that does not exist, and rename the folder back afterwards. An SSH logon of an administrator is High; a Medium process comes from a scheduled task with `-LogonType Interactive -RunLevel Limited` while the console session is logged on, and a daemon such a task starts comes through the WMI launch, since the task's job refuses breakaway.

## Reproducing a terminal fault

The CLI verbs send no `Resize`, so reproducing a fault at a given width needs a small client that sends `hello`, then `resize` with `claim: true`.

Keep scratch repos outside the checkout, and in a driven run use the `scratch_repo` folder `launch` returns: a scratch folder under this repo makes Claude ask about the repo's `.mcp.json` server (`rt-driver`) before the first prompt.

A session's `scrollback.bin` holds the raw ConPTY output and can be replayed into any emulator, which makes it the first stop for a garbled-terminal report. ConPTY writes no sync marks into it, so split it into frames on `ESC[?25h`. It is a ring whose offsets move while the session runs, so copy it first and read the copy.

A real Claude TUI can run with no driver token against a loopback mock of the API: set `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, an isolated `CLAUDE_CONFIG_DIR` and `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`, and seed that folder's `.claude.json` with `hasCompletedOnboarding` and the project's `hasTrustDialogAccepted`. The mock makes the TUI ask exact questions, with no login and no model spend. A fresh Claude home also shows an external-imports dialog and an auto-mode prompt before the first prompt: seed `hasClaudeMdExternalIncludesWarningShown` in `.claude.json` against the first. Set `RUSTLING_TULIP_TRACER_PIPE_PREFIX` too, so the rig's tracers do not share pipe names with a running daemon's.

More a real-Claude rig needs:

- `ANTHROPIC_*` variables do not pass the daemon's env keep-list into the session, so set them in the isolated home's `settings.json` `env`.
- In plan mode the request's last message has role `system`, so a mock must look for the last `user` message.
- A mock's `ExitPlanMode` with no plan shows a stub dialog no rule reads.
- A session folder under this repo sees `.mcp.json`, so the CLI shows the `rt-driver` MCP dialog first.
- Startup dialogs change with the session's mode, so check the screen before sending scripted keys.
- `permissions.defaultMode: "default"` in `settings.json` is needed for a mock's tool to raise a permission prompt.

Sending `/compact` to a session from Git Bash needs `MSYS_NO_PATHCONV=1`, or the shell rewrites the leading `/` into a Windows path.

On a daemon that is not serving the tailnet listener, the debug line `no push is sent kind=Finished` shows the real trigger's decision for each Finished.

`AnswerPrompt` with `allow` on a question probes `dialog_for` without typing anything: the reply is "does not fit" when the dialog passed and "Couldn't read this prompt; answer it in its pane." when it did not, so one dialog can be probed at every grid size.

Such a rig's `cargo build` goes through the gate without `--local`: a routed build brings `target/debug/*.exe` back to this PC. `--local` fits the rig's own run, which drives this PC's processes.

## Looking at a UI change

To look at a UI change, run `.\rt.ps1 native-shot <view>`: it writes a PNG of the real client window over a scripted fake daemon (`apps/native/examples/shot.rs`) to `.tmp\shots\` and prints its path (it draws Nord, the default theme; the UI specs pin Contrast in the harness). Read the PNG, and show the user before and after shots for any visual choice.

The views:

- `main`, `source-control`, `diff`, `settings`, `viewer`, `viewer-md`, `dashboard`: those screens as named; `dashboard` is seeded to the By status view.
- `settings-claude`: Settings on Claude's page scrolled to its end, the end of Launch command and then Hooks.
- `dashboard-compact`: the Dashboard's Compact view over scripted agents in several repos, a workspace and a folder, at 1920x1080.
- `main-compact`: the sidebar's Compact leaves.
- `main-narrow`: the tulip tab split three ways, its narrow pane headers collapsed to the ⋯ overflow button.
- `settings-dashboard`: Settings on its Dashboard tab with a keyed provider, a stored key, some spend and a Test result.
- `spawn`: the spawn dialog with its Target list open.
- `jump`: the jump palette open over the main window with an empty query.
- `connections`: the host manager with This PC connected, the saved tailnet host "box" showing "Needs pairing" with its `Pair` and `Reconnect now` buttons, and a saved SSH host switched Off; This PC's row shows its version.
- `connections-ssh`: its `+ Add host` form switched to SSH after a passing Test connection.
- `connections-scan-error`: the same host manager over a tailnet scan that failed with a long Tailscale error, drawn on its own lines under "On your tailnet".
- `menu`: the sessions panel's ⋯ menu open over the repo rows with a toast over the panes.
- `update`: the footer's `Update {version} ready` notice.
- `update-countdown`: the sticky toast `Updating in 10 s` of a host update counting down, with its `Asked by` detail and `Cancel` button.
- `update-host`: the confirm dialog of a host update over the Connections dialog: the host's sessions with their status, and `Cancel` / `Update {host}`.
- `update-host-breaks`: the same dialog for a release that ends sessions and breaks the protocol: the sessions warning and the protocol warning above the buttons.
- `update-notes`: the What's new window over a ready 0.99.0.
- `chat`: a session pane in Chat mode with its chat drawn over a two-question prompt card whose options carry descriptions and an open Tail box on the running Bash card.
- `chat-compact`: the chat in Compact mode at its bottom, showing a collapsed run line of a read, a search and a run call.
- `summary`: one in Summary mode with a line open under a pinned permission card.
- `themes`: the Themes modal over Settings, on the row after the theme in use, with a user theme, a broken file and a file named after a built-in.
- `themes-edit`: the theme editor on a duplicate of Nord.
- `multi-host`: the sidebar grouped by host: This PC, "box" online with a repo and a session in the first tab's second pane, and "lab" offline.
- `spawn-hosts`: the spawn dialog over those two hosts with its closed Target field.
- `open-shell`: the Open a shell dialog with a folder path too long for its field, scrolled to the caret at its end.
- `safe-start`: the prompt a start after a crashed one raises, "The last start crashed" with Open tabs and Start without tabs, over the window with no tab drawn.
- `panes-accent`: four panes in a 2x2 grid, the Theme pane focused, beside the Rose, Periwinkle and Lilac panes a chosen colour stores an accent and a frame for.
- `panes-accent-chosen`: the same grid with the Periwinkle pane focused.
- `settings-hosts`: Settings on the Claude page with the host selector showing This PC and a remote host on 0.9.0 wearing the Behind tag.
- `settings-accounts`: Settings on the Accounts tab, scrolled to the auto-switch block and its forecast.
- `settings-sounds`: Settings on the Notifications tab with the Needs you sound list open.
- `accounts`: the footer's accounts popover open over scripted accounts. The shots' fake daemon lists `accounts`, so every view shows the accounts chip.
- `toast`: the app's own toast cards in a tall window: a permission Needs you with Allow and Deny, a Finished card, and a "Needs you · web" card with three long, wrapped answers carrying descriptions.
- `gates`: the Gates panel over a scripted gate state: busy, held, free and stale heavy slots with a gate waiting, a busy light slot, the remote pool and the queue, then the Recent runs block (good and bad rows) and the Last 7 days block, with the footer's `Gates` pill; slot, waiting and queue rows show the usual time after the time held or the age, and the pill carries the failure tail for a run that failed in the last 10 minutes. Only this view's fake daemon lists `gate_state`, and it answers `GateHistory` with a scripted journal.

The `support` builders for account specs (`apps/native/tests/support/mod.rs`) are `account`, `auto_switch`, `accounts_reply` and `accounts_updated`; `Harness::reconnect_with_features` redoes the handshake with extra `Welcome.features` (`tests/ui/ui_accounts.rs` uses it to list `accounts`).

An OS toast press is tested through the `NotifyRecorder` notifier: `Harness::notified_launches` lists the launch argument each notification carried, `Harness::activate_os_toast(index)` presses the notification at `index`, and `activate_toast_arg(arg)` presses with a raw argument, both delivering it through the window's activation sink as the toast activator does. `h.cx.deactivate_window()` simulates an inactive main window.

## iPhone Simulator on the Mac mini

`tools/sim/sim.ps1` drives Xcode's iPhone Simulator on the Mac mini builder from this PC, for a run that needs the phone app on a real simulator: `setup` installs idb 1.7.8 into `~/.local` with no admin rights, `boot` starts a device, `open`, `shot`, `describe`, `tap`, `text`, `swipe` and `key` drive it, and `shutdown` stops it. Screenshots come back to this PC over scp, and no Simulator window opens on the Mac. See [tools/sim/README.md](../tools/sim/README.md).

## Fake CLIs

The `crates/agents/claude/fake/` shim (`fake-claude.cmd` or `fake-claude.sh`, plus `index.mjs`) replaces the real CLI in the e2e tier. The plugin returns it from `Agent::fake_cli` (`agent_api::FakeCli`, feature `test-support`), and the live harness (`apps/native/tests/support/live.rs`) sets each registered agent's override variable (`program_env_var()`, `RUSTLING_TULIP_CLAUDE` for Claude) to its fake through `agent-registry`, so the daemon path-resolves the CLI from that variable at spawn time.

It also points the plugin's home variable at `<root>/<agent id>-home` and puts the plugin's stub name first on `PATH`; a registered plugin with no fake panics. That PATH stub only prints a marker: a spec that needs the fake started by hand types its full path (from `RUSTLING_TULIP_CLAUDE`) or a wrapper that calls it.

The Claude fake plays the chat mirror's side of a session too. It echoes the draft typed or pasted into the prompt box as `❯ <text>`, writes a transcript to `<CLAUDE_CONFIG_DIR>/projects/<encoded cwd>/<id>.jsonl` (the id from `--session-id` or `--resume`; started with neither, it mints a uuid and names the transcript after it), and runs a turn for any submitted line that is not blank or a `/` command: a `user` record and `UserPromptSubmit`, the spinner, the answer, a `turn_duration` record, `Stop` and the idle box.

It loads the user layer's hooks from `<CLAUDE_CONFIG_DIR>/settings.json` and runs them before the `--settings` hooks for each event, printing `user-settings-invalid <path>` when that file is malformed (a missing one goes to `fake-claude.log` in the OS temp dir, not the screen); every start runs `SessionStart` (`source` `resume` under `--resume`, else `startup`, tested against a group's `matcher`), so a spec can show a hand-started run reporting through user hooks (`user_hooks_report_a_hand_started_agents_conversation` in `e2e_live.rs`).

The cue `rt-perm` in a message makes that turn write a `Bash` `tool_use` record, run `PermissionRequest` and draw the permission dialog, where a bare `1` or `2` allows (the `tool_result`, `PostToolUse`, the answer) and `3` denies. The input lines `/rt-hook prompt`, `/rt-hook ask` and `/rt-hook bg` run hooks for an open turn, an AskUserQuestion dialog and a `Stop` listing a running shell (`crates/agents/claude/fake/index.mjs`, its README).

It also stands in for `claude auth login`: run as `auth login …` it writes a subscription login the account store captures, or fails or hangs, as the `FAKE_CLAUDE_LOGIN` knob says (see its README). That mode's `fake_login` test in `agent-claude` runs the shim under `node`, so it needs `node` on `PATH` and is Windows-only.

The `tools/e2e/fake-tailscale/` shim (`fake-tailscale.cmd`, `.sh` and `index.mjs`, see its README) answers `whoami`, `whois`, `status` and `cert` from the JSON state file named by `RUSTLING_TULIP_FAKE_TAILSCALE_STATE`; it is wired in through `RUSTLING_TULIP_TAILSCALE`.

The `tools/e2e/fake-ssh/` shim (`fake-ssh.cmd`, `.sh` and `index.mjs`, see its README) stands in for `ssh` through `RUSTLING_TULIP_SSH`: it checks the client's argv shape (`-T`, `-o BatchMode=yes`, `--` directly before the host, one remote command starting with `rustling-tulipd`), appends `{pid, argv}` to a log, and runs `rustling-tulipd bridge` with the test daemon's isolated environment.

Its JSON state file, named by `RUSTLING_TULIP_FAKE_SSH_STATE`, holds `bridge`, `env`, `fail` and `log`; a `fail` entry makes it print that stderr and exit with that code, and an unexpected argv or remote command exits 2 with `fake-ssh: unexpected argv: <json>` or `fake-ssh: unexpected remote command: <json>`.
