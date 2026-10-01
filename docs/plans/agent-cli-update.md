# Update an agent CLI before launching it

Design for the "Self-update an agent CLI before launching it" line in [MAIN.md](./MAIN.md) (Backlog): every Claude, Codex or Cursor spawn first tries to bring that CLI to its latest version, then launches it (user). The daemon owns the update, since it owns every spawn; the client only shows the result and carries the setting. Step ids are `AU.n`.

## Problem

A session runs whatever version of the CLI is on disk when it starts. Claude Code's native install updates itself in the background, but the new version only takes effect on the next start; Codex and Cursor installed through a package manager never update unless someone runs the upgrade by hand (this machine runs `codex-cli 0.156.1` while winget offers `0.159.1`, from `winget list --id OpenAI.Codex -e`). With many sessions spawned a day, the daemon is the natural place to update each CLI once, before the spawns that follow, without making every spawn wait on the network.

## Findings

### Claude Code (`claude`)

- **Update command.** `claude update` (alias `upgrade`): "Check for updates and install if available", no options besides `-h` (`claude update --help`, 2.1.285). It follows the `autoUpdatesChannel` setting (`latest` default, or `stable`) and refuses to go below `minimumVersion` ([setup, Configure release channel / Pin a minimum version](https://code.claude.com/docs/en/setup#configure-release-channel)).
- **Output.** On an install it reports `Successfully updated from <old version> to version <new version>`; when current, `Claude Code is up to date (<version>)`; Homebrew, WinGet and apk installs report `Claude is up to date!` instead ([setup, Update manually](https://code.claude.com/docs/en/setup#update-manually)). "`claude update` now announces the target version before downloading" ([CHANGELOG.md](https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md), 2.1.169). Exit codes are not documented; not verified.
- **Background updater.** "Claude Code checks for updates on startup and periodically while running. Updates download and install in the background, then take effect the next time you start Claude Code." Native installs only; Homebrew, WinGet and Linux package installs do not auto-update ([setup, Auto-updates](https://code.claude.com/docs/en/setup#auto-updates)). `DISABLE_AUTOUPDATER=1` stops only the background check, and `claude update` still works; `DISABLE_UPDATES` blocks every update path including `claude update` ([setup, Disable auto-updates](https://code.claude.com/docs/en/setup#disable-auto-updates); CHANGELOG 2.1.118). `CLAUDE_CODE_PACKAGE_MANAGER_AUTO_UPDATE=1` makes Claude run `winget upgrade` itself on a WinGet install ([setup, note under Auto-updates](https://code.claude.com/docs/en/setup#auto-updates)).
- **Windows installs.** Native: `irm https://claude.ai/install.ps1 | iex`; WinGet: `winget install Anthropic.ClaudeCode`, upgraded with `winget upgrade Anthropic.ClaudeCode`; npm: `npm install -g @anthropic-ai/claude-code`, upgraded with `npm install -g @anthropic-ai/claude-code@latest` (never `npm update -g`) ([setup](https://code.claude.com/docs/en/setup)). On this machine (native install) `%USERPROFILE%\.local\bin\claude.exe` is a 243,751,072-byte file, the same size as `%USERPROFILE%\.local\share\claude\versions\2.1.285`, with 2.1.283 and 2.1.284 kept beside it (`ls -la ~/.local/bin ~/.local/share/claude/versions`): on Windows the launcher is a copy of the current version, not a symlink, so every session started from it holds that one file open.
- **Updating while other instances run.** "On WinGet the upgrade may fail while Claude Code is running because Windows locks the executable. In that case Claude Code shows the manual command instead." ([setup](https://code.claude.com/docs/en/setup#auto-updates)). For the native install, CHANGELOG entries show the updater works around the lock and has had races: "Windows: Fixed update failures caused by `claude.exe` being in use showing a generic error instead of telling you to close other sessions and retry" (2.1.154); "Fixed Windows update rollback: if a Windows update fails, Claude Code now restores the original executable by copy" (2.1.153); "Auto-updater on Windows now stops retrying within a session once `claude.exe` is held by another process" (2.1.169); "Fixed Windows auto-update failures that could leave `claude.exe` missing" (2.1.217); "Windows: Fixed a race in which Claude Code sessions updating at the same moment could delete each other's `claude.exe` backup, which could leave no `claude.exe` behind" (2.1.281); "Fixed false 'Another process is currently updating Claude' error when running `claude update` while another instance is already on the latest version" (2.0.67), which shows the CLI takes its own update lock ([CHANGELOG.md](https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md)). Whether `claude update` on Windows today succeeds while other sessions run from `.local\bin\claude.exe` (by renaming the held file aside) or fails with "close other sessions" is not verified: no update was run for this research.

### Codex (`codex`)

- **Update command.** `codex update`: "Update Codex to the latest version" (`codex update --help`, codex-cli 0.156.1). It detects how Codex was installed and runs that method's command: `npm install -g @openai/codex`, `bun install -g …`, `pnpm add -g …`, `vp install -g …`, `brew upgrade --cask codex`, or for the standalone Windows install `powershell -ExecutionPolicy Bypass -c "$env:CODEX_NON_INTERACTIVE=1; irm https://chatgpt.com/codex/install.ps1 | iex"`; with no detectable method it fails with "Could not detect the Codex installation method. Please update manually" ([`codex-rs/tui/src/update_action.rs`](https://github.com/openai/codex/blob/main/codex-rs/tui/src/update_action.rs), [`codex-rs/cli/src/main.rs` `run_update_command`](https://github.com/openai/codex/blob/main/codex-rs/cli/src/main.rs)).
- **Output and exit.** It prints ``Updating Codex via `<cmd>`...``, then on success "🎉 Update ran successfully! Please restart Codex."; a failing package-manager command makes it bail with ``"`<cmd>` failed with status <status>"`` (non-zero exit through `anyhow`) ([`main.rs` `run_update_action`](https://github.com/openai/codex/blob/main/codex-rs/cli/src/main.rs)). It does not say "already latest": it re-runs the install command either way, so "updated" versus "already current" is only visible by comparing `codex --version` before and after (prints `codex-cli 0.156.1`).
- **WinGet installs are not detected.** The install method comes from the executable's path: a standalone release under `CODEX_HOME\packages\standalone\…`, npm/pnpm/bun shims (via `CODEX_MANAGED_BY_*` env), Homebrew on macOS, else `InstallMethod::Other` ([`codex-rs/install-context/src/lib.rs` `install_method_from_exe`](https://github.com/openai/codex/blob/main/codex-rs/install-context/src/lib.rs)). The WinGet package layout is recognised for its resources, but its method falls through to `Other`, so `codex update` on a WinGet install fails with the "Could not detect" message (read from source; not run). WinGet's `OpenAI.Codex` is a `portable (zip)` installer (`winget show OpenAI.Codex`), unpacked into `%LOCALAPPDATA%\Microsoft\WinGet\Packages\OpenAI.Codex_…\` with `codex.exe` in `WinGet\Links` a symlink to `codex-x86_64-pc-windows-msvc.exe` there (`ls -la …\WinGet\Links`).
- **Background updater.** None. With `check_for_update_on_startup` (default `true`; "Set to `false` only if your Codex updates are centrally managed", [`config_toml.rs`](https://github.com/openai/codex/blob/main/codex-rs/config/src/config_toml.rs)) the TUI refreshes a cached latest version at most every 20 hours in the background and shows a banner ([`codex-rs/tui/src/updates.rs` `get_upgrade_version`](https://github.com/openai/codex/blob/main/codex-rs/tui/src/updates.rs)), and at startup may show a blocking "Update now / Not now / Don't remind" screen when it knows an update command ([`codex-rs/tui/src/update_prompt.rs`](https://github.com/openai/codex/blob/main/codex-rs/tui/src/update_prompt.rs)). A session spawned by the daemon can therefore open on that screen instead of the prompt; updating first removes the cause.
- **Updating while other instances run.** The standalone Windows installer keeps each version in its own release folder and points a `current` junction and the visible `bin` junction at it (`Ensure-Junction`, [`scripts/install/install.ps1`](https://github.com/openai/codex/blob/main/scripts/install/install.ps1), lines 1108 and 1129; visible bin `%LOCALAPPDATA%\Programs\OpenAI\Codex\bin`, line 939), so running sessions keep their old folder. A WinGet portable upgrade rewrites the exe in place; WinGet reports `0x8A150101 APPINSTALLER_CLI_ERROR_INSTALL_PACKAGE_IN_USE` and `0x8A150103 APPINSTALLER_CLI_ERROR_INSTALL_FILE_IN_USE` for held files and `0x8A15002B APPINSTALLER_CLI_ERROR_UPDATE_NOT_APPLICABLE` for "No applicable update found" ([winget-cli returnCodes.md](https://github.com/microsoft/winget-cli/blob/master/doc/windows/package-manager/winget/returnCodes.md)). Whether a portable upgrade of `OpenAI.Codex` fails while a Codex session runs is inferred from those codes, not measured. npm's behaviour when a global package's exe is held is not verified either.

### Cursor Agent (`cursor-agent` / `agent`)

- **Update command.** `cursor-agent update` (also `agent update`): "Update Cursor Agent to the latest version", no options (`cursor-agent update --help`, 2026.09.28-64d2043; [Parameters](https://cursor.com/docs/cli/reference/parameters)). In a session, `/update` does the same ([CLI changelog, June 22, 2026](https://cursor.com/docs/cli/changelog)). Its output and exit codes are not documented; not verified.
- **Background updater.** "Cursor CLI will try to auto-update by default" ([Installation](https://cursor.com/docs/cli/installation)); `--disable-auto-update` turns background updates off, and "parallel auto-updates can't corrupt an install" ([CLI changelog, January 2026](https://cursor.com/docs/cli/changelog)). The flag is not listed in `cursor-agent --help`, so it is unverified in the installed version. The release channel is `channel` in `%USERPROFILE%\.cursor\cli-config.json` ([Configuration](https://cursor.com/docs/cli/reference/configuration)).
- **Windows install.** `irm 'https://cursor.com/install?win32=true' | iex` ([Installation](https://cursor.com/docs/cli/installation)). On this machine it lives in `%LOCALAPPDATA%\cursor-agent\`: `cursor-agent.cmd` runs `cursor-agent.ps1`, which picks the newest folder under `versions\` (`2026.09.28-64d2043`) and runs its `node.exe index.js` (read from the two files). The daemon's `resolve_agent_program` finds no `node_modules` beside that `.cmd`, so it launches `cmd.exe /d /c …\cursor-agent.cmd`.
- **Updating while other instances run.** Versions sit side by side, and "Running processes now mark their install directory as in use so cleanup skips it" ([CLI changelog, July 13, 2026](https://cursor.com/docs/cli/changelog)). An update while sessions run is safe by design.

### The daemon today (read from the code)

- Each backend names its override variable and default program: `RUSTLING_TULIP_CLAUDE` / `claude` (`agents/claude.rs`), `RUSTLING_TULIP_CODEX` / `codex` (`agents/codex.rs`), `RUSTLING_TULIP_CURSOR_AGENT` / `cursor-agent` (`agents/cursor.rs`, missing from CLAUDE.md's environment table). `AgentBackend::resolve_program` (`agents/mod.rs`) returns the variable's value or the default name.
- Interactive spawns then run `server.rs::resolve_agent_program`, which `which`-resolves the name and turns an npm `.cmd` shim into its `node_modules\…\bin\<stem>.exe` or `node.exe <stem>.js`, else `cmd.exe /d /c <shim>`. Headless spawns (`spawn_headless_session`) pass `resolve_program()` unresolved.
- Every spawn, including presets (`presets.rs`), duplicates, recover and the in-place checkout confirm, goes through `server.rs::spawn_session`, which does the git and worktree work before `spawn_interactive_session` / `spawn_headless_session`. `spawn_interactive_session` already waits up to `PRECREATE_TIMEOUT` (5 s) for Cursor's `create-chat` before the tracer spawn.
- `dispatch` is awaited inside each connection's receive loop (`server.rs`, the `dispatch(hub, parsed, ctx).await` in the recv loop), so a `SpawnSession` that waits blocks every later message from that client, including `SendInput` to its other sessions, until it returns. Preset launches already run in a `tokio::spawn`ed task for this reason ("so the dispatcher returns immediately and the connection stays responsive while sessions spawn").
- `spawn_plan.rs` holds base-ref resolution and `SpawnFailure`, not program resolution; it is not touched here.
- Host settings that must apply with no window open live in `state.json` (`PersistedState::keep_awake`, `state.rs`) and travel as `ClientMessage::SetKeepAwake` / `DaemonMessage::KeepAwakeStatus`.

## Design

### The updater (`crates/daemon/src/agent_update.rs`)

Plain Rust with the process runner behind a trait, so every path is tested with a fake.

```rust
pub enum UpdatePlan {
    /// The CLI's own command: `claude update`, `codex update`, `cursor-agent update`.
    SelfUpdate { program: String, prepend: Vec<String>, args: Vec<String> },
    /// No update: the reason is logged once (an override path, the setting off, an install the CLI cannot update itself).
    Skip(SkipReason),
}

pub enum UpdateOutcome {
    Updated { from: String, to: String },
    Current { version: String },
    Failed { reason: String, in_use: bool, version: Option<String> },
    TimedOut { version: Option<String> },   // still running in the background
    Skipped(SkipReason),
    Throttled { last: Box<UpdateOutcome>, at: Instant },
}

pub trait Runner: Send + Sync {
    fn run(&self, program: &str, args: &[String], timeout: Duration) -> BoxFuture<'_, RunResult>;
}

pub struct AgentUpdater { /* per-Agent single-flight slot, last outcome, clock, runner */ }

impl AgentUpdater {
    pub fn plan(agent: Agent, resolved: &ResolvedProgram, override_set: bool, enabled: bool) -> UpdatePlan;
    /// Start (or join) the update of `agent`'s CLI; returns a handle the spawn awaits later.
    pub fn begin(&self, agent: Agent, plan: UpdatePlan) -> UpdateTicket;
}
pub async fn wait(ticket: UpdateTicket, limit: Duration) -> UpdateOutcome;
```

- **Plan per CLI (Open question 1: the CLI's own command only).** Claude: `claude update`. Codex: `codex update`, except a WinGet install (resolved path under `%LOCALAPPDATA%\Microsoft\WinGet\`), which `codex update` cannot detect: `Skip(SkipReason::Unsupported)`, logged once per daemon run. Cursor: `cursor-agent update` through the same program and prepend the spawn uses (`cmd.exe /d /c …\cursor-agent.cmd update`). A WinGet Claude still runs `claude update`, which reports "Claude is up to date!" and reads as `Current` unless its version changes.
- **Outcome from versions, not from text.** `<program> --version` runs before and after the update (5 s timeout each). A different version is `Updated`; the same version with exit 0 is `Current`; anything else is `Failed` with the last non-empty line of the combined output as `reason`, and `in_use: true` when the output names a held file ("in use", "close other sessions"). This works whatever each CLI prints, and none of the three documents exit codes.
- **Single-flight per CLI (Open question 4).** One slot per `Agent` holding a shared future (`futures::future::Shared`). A spawn that finds an update of its CLI running joins it; a second CLI's update runs in parallel. The update child runs with `kill_on_drop(false)` and `CREATE_NO_WINDOW`, from a temp working directory (as `codex update` itself does, so a project's config never steers the updater), with the daemon's environment plus the user-scope variables `user_env` reads.
- **Throttle (Open question 3).** The slot keeps the last outcome and its time. Within the window a spawn gets `Throttled` at once and runs nothing; after `Updated` or `Current` the window is 1 hour, after `Failed`/`TimedOut` 10 minutes. In memory only: a daemon restart checks again on the next spawn.
- **Waiting (Open question 2).** `spawn_session` calls `begin` at its start, right after the agent is known and before the git and worktree work, so the update overlaps it; it awaits `wait(ticket, 20 s)` just before `resolve_program`. A limit reached returns `TimedOut` and the spawn launches what is on disk; the update keeps running and its outcome lands in the slot for the next spawn.
- **Connection stays responsive.** Because `SpawnSession` and `DuplicateSession` are awaited in the receive loop, those two arms move their `spawn_session` call into a `tokio::spawn`ed task with `hub.clone()`, as the preset launch already does; the reply still travels on the registry broadcast with its `request_id`, and a failure is sent through the connection's `out_tx` clone (Open question 2 names what happens if this is not done).

### Outcomes the user sees (Open question 6)

- `daemon.log`: every outcome at `info!` (`Updated`, `Current`, `Throttled` at `debug!`) or `warn!` (`Failed`, `TimedOut`), with agent, program, plan, versions, elapsed time and reason.
- The spawned session's first `recent_actions` entry after "session started": "claude 2.1.284 → 2.1.285", or "claude not updated: <reason>; launched 2.1.284".
- A client toast for a failure, once per failure streak per CLI (the first `Failed` after a success, or after the daemon starts), carried by a new additive `DaemonMessage::AgentUpdateFailed { agent, version, reason, in_use }` broadcast. No toast for `Updated`, `Current`, `Throttled`, `TimedOut` or offline failures after the first.

### The setting (Open question 7)

- `PersistedState::agent_updates: bool`, `#[serde(default = "default_true")]`, beside `keep_awake` in `state.rs`, read at each spawn.
- Additive `ClientMessage::SetAgentUpdates { enabled }` and `DaemonMessage::AgentUpdatesStatus { enabled }`, sent at initial state and broadcast on change, like `SetKeepAwake` / `KeepAwakeStatus`.
- Settings → General gains "Update Claude, Codex and Cursor before launching" (on by default).

### Overrides and tests (Open question 8)

- When the agent's override variable (`program_env_var()`) is set, `plan` returns `Skip(SkipReason::Override)`: the program is a test shim (`RUSTLING_TULIP_CLAUDE` points at `tools/e2e/fake-claude/fake-claude.cmd` in the live tier, `apps/native/tests/support/live.rs`) or a build the user chose. So the e2e tiers never touch the network and never see a delay.

### Protocol (additive, no bump)

`ClientMessage::SetAgentUpdates`, `DaemonMessage::AgentUpdatesStatus`, `DaemonMessage::AgentUpdateFailed` in `crates/protocol/src/lib.rs`. New variants reach an older native client and the installed Tauri app (protocol 22) as `InboundDaemonMessage::Unknown`; no v22 message changes, so `v22_compat` stays green and `supported` does not change.

### What it does not do

- It does not stop, restart or relaunch running sessions after an update.
- It does not install a CLI that is missing; a spawn of a CLI `which` cannot find fails as today.
- It does not change the CLIs' own background updaters unless Open question 9 rules otherwise.

## Steps

Order: AU.1 → AU.2 → AU.3 → AU.4; AU.5 needs AU.4; AU.6 needs AU.3 and AU.5; AU.7 needs AU.4; AU.8 last. `server.rs` is touched by AU.4 and AU.5, `protocol/src/lib.rs` by AU.5.

- [ ] **AU.1 Measure the Windows lock behaviour.** A throwaway script (not committed) on a machine where each CLI is one version behind: start one session of each CLI, then run `claude update`, `codex update` (standalone install; and on a WinGet install, to confirm it refuses) and `cursor-agent update`, recording exit code, output, elapsed time and whether the running session survives; repeat with no session running. Record the results in this subplan's Findings, replacing the "not verified" lines. Proof: the table in Findings; Open questions 1, 5 and 9 re-read against it before AU.3.
- [ ] **AU.2 Plan and outcome, pure.** New `crates/daemon/src/agent_update.rs` with `UpdatePlan`, `UpdateOutcome`, `SkipReason`, `plan(agent, resolved, override_set, enabled)`, `classify(before, after, run_result)` and `parse_version(agent, stdout)` (`2.1.285 (Claude Code)`, `codex-cli 0.156.1`, `2026.09.28-64d2043`); its `mod` line in `main.rs`. Proof, red first: `cargo test -p daemon agent_update` with `plan_uses_self_update_for_native_claude`, `plan_skips_a_winget_codex_as_unsupported`, `plan_runs_cursor_through_its_cmd_shim`, `plan_skips_when_the_override_is_set`, `plan_skips_when_the_setting_is_off`, `classify_reads_a_version_change_as_updated`, `classify_reads_the_same_version_as_current`, `classify_flags_in_use_from_text`, `classify_keeps_the_last_output_line_as_reason`, `parse_version_reads_each_cli`.
- [ ] **AU.3 Runner, single-flight and throttle.** `Runner` trait, the `tokio::process` runner (`CREATE_NO_WINDOW`, temp cwd, timeout, `kill_on_drop(false)`), `AgentUpdater::begin`, `wait`, the per-agent slot and the 1 h / 10 min windows behind an injectable clock. Proof, red first, with a fake runner and a paused tokio clock: `two_spawns_of_one_cli_share_one_run`, `claude_and_codex_updates_run_in_parallel`, `a_success_is_reused_within_the_hour`, `a_failure_is_retried_after_ten_minutes`, `wait_returns_timed_out_and_the_run_finishes_into_the_slot`, `a_runner_error_is_failed_not_a_panic`; one Windows-only test running a real `cmd /c` script that prints a version and exits 1, read as `Failed` with its last line.
- [ ] **AU.4 Spawn wiring.** `Hub.agent_updater`; `spawn_session` calls `begin` before the git work and `wait` before program resolution for interactive and headless modes (never plain shells); the `recent_actions` entry; the `SpawnSession` and `DuplicateSession` arms run `spawn_session` in a spawned task. Files: `crates/daemon/src/server.rs`, `agent_update.rs`, `agents/mod.rs` (a `ResolvedProgram` the plan reads, if `resolve_agent_program` moves there). Proof: `test_hub` tests with a fake runner: a Claude spawn with the override unset runs the update before the tracer spawn and records "claude 1.0.0 → 1.0.1"; a spawn with `RUSTLING_TULIP_CLAUDE` set runs nothing; a runner that never finishes delays the spawn by the limit only; a `SendInput` sent while a spawn waits is handled before the spawn returns; existing spawn, duplicate, preset and recover tests stay green. `cargo test -p daemon`.
- [ ] **AU.5 Setting and failure toast, protocol and daemon.** `PersistedState::agent_updates`; `SetAgentUpdates`, `AgentUpdatesStatus`, `AgentUpdateFailed`; initial state; the once-per-streak rule. Files: `crates/protocol/src/lib.rs`, `crates/daemon/src/state.rs`, `server.rs`, `agent_update.rs`. Proof, red first: `set_agent_updates_round_trips`, `agent_update_failed_round_trips`, `v22_compat` in `-p protocol`; `agent_updates_defaults_to_enabled_for_old_state_files` in `state.rs`; `test_hub` tests that `SetAgentUpdates { enabled: false }` persists, broadcasts and makes the next spawn skip, and that two failures in a row send one `AgentUpdateFailed` and a success re-arms it.
- [ ] **AU.6 Native client.** The Settings → General toggle, `AgentUpdatesStatus` and `AgentUpdateFailed` arms in `RootView::on_message` (`lib.rs`), a warning toast "Couldn't update Codex (in use by running sessions); launched 0.156.1". Files: `apps/native/src/lib.rs`, the Settings General view, `tests/support/mod.rs` builders. Proof: `ui_settings`-style spec: the toggle reflects `AgentUpdatesStatus` and sends `SetAgentUpdates`; an `AgentUpdateFailed` shows one toast naming the CLI and reason. `cargo test -p rustling-tulip-native --test ui ui_settings` (or the spec file that owns General).
- [ ] **AU.7 Live tier.** In `apps/native/tests/e2e_live.rs`, a spec that spawns a fake-claude session with `RUSTLING_TULIP_CLAUDE` set and asserts `daemon.log` holds the "skipped: override" line and no update child ran. Proof: `.\rt.ps1 native-e2e`.
- [ ] **AU.8 Docs.** `docs/architecture.md` (the daemon's components gain `agent_update.rs`; a task-index row for "agent CLI updates"); `docs/native-client.md` (the Settings toggle and the toast); CLAUDE.md's environment table gains `RUSTLING_TULIP_CURSOR_AGENT` and the note that an override skips updates, and its `state.json` line gains `agent_updates`; the README glossary (the terms below and `AU.n` in the step-id entry); delete the MAIN.md line and this subplan once promoted.

## Glossary terms this subplan coins

- **Agent update**: the daemon running a CLI's own update command before a spawn of that CLI.
- **Update window**: how long a finished agent update is reused before the next spawn of that CLI checks again.
- **Single-flight**: spawns of the same CLI that start while its update runs all wait on that one run instead of starting their own.

## Open questions

Answered (user): Q1 (b) each CLI's own update command only, an install it cannot update (Codex from WinGet) logged as unsupported and skipped; Q2 (a) up to 20 s, then launch the installed version; Q3 (a) 1 h after success, 10 min after failure, in memory; Q4 (a) one update per CLI, same-CLI spawns join it; Q5 (a) update anyway, an in-use failure launches the installed version; Q6 (a) log, recent actions and one toast per failure streak; Q7 (a) one `agent_updates` host setting with a Settings → General toggle; Q8 (a) a set override skips updates for that CLI; Q9 (a) the CLIs' own background updaters are left as they are; Q10 (a) every Claude, Codex or Cursor spawn, never plain shells. All are folded into the Design and Steps above.

1. **Which command updates each CLI.**
   - (a) *Recommended.* The CLI's own command (`claude update`, `codex update`, `cursor-agent update`), except a WinGet install of Claude or Codex, which the daemon upgrades with `winget upgrade --id <id> -e --silent --disable-interactivity --accept-source-agreements --accept-package-agreements`, detected by a resolved path under `%LOCALAPPDATA%\Microsoft\WinGet\`. Worst case: WinGet takes several seconds to refresh its source on every check, and a 20 s wait runs out on a slow network.
   - (b) The CLI's own command only; an install it cannot update (Codex from WinGet) is logged as unsupported and skipped. Worst case: this machine's Codex never updates, which is the case the item exists for.
   - (c) The daemon detects the install method for every CLI and calls the package manager or installer script itself. Worst case: the daemon re-implements three vendors' install detection and breaks when one of them changes its layout.
2. **How long a spawn waits.**
   - (a) *Recommended.* Up to 20 s, overlapping the spawn's git and worktree work; past that, launch the installed version while the update finishes in the background for the next spawn. The `SpawnSession` and `DuplicateSession` arms move into a spawned task so the client's other sessions keep taking input. Worst case: the first spawn after a release launches the old version, and a fresh `claude` download (the binary is 243,751,072 bytes on this machine) on a slow link always takes that path.
   - (b) Wait for the update to finish, capped at 120 s. Worst case: a spawn on a flaky network sits for two minutes before the user sees anything.
   - (c) Never wait: start the update and launch at once; the new version applies from the next spawn. Worst case: a spawn right after a release always runs the old version, and on Windows the update races the launch for the same `claude.exe`.
3. **Reusing a recent check.**
   - (a) *Recommended.* After `Updated` or `Current`, no check for 1 hour; after a failure or timeout, 10 minutes; kept in memory. Worst case: a release in that hour reaches spawns up to an hour late.
   - (b) Check on every spawn. Worst case: a preset launching eight sessions runs one update and seven `--version` pairs, and an offline laptop waits on every spawn.
   - (c) 12 hours, persisted in the config dir. Worst case: a fix the user is waiting on arrives half a day late, and a new state file has to be kept.
4. **Several spawns at once.**
   - (a) *Recommended.* One update per CLI at a time: spawns of the same CLI join the running update; different CLIs update in parallel. Worst case: a Claude and a Codex WinGet upgrade run together and one waits on WinGet's own install lock.
   - (b) One update at a time across all CLIs. Worst case: a Cursor spawn waits behind a slow Claude download it has nothing to do with.
   - (c) No coordination; each spawn runs its own update and relies on the CLI's own lock. Worst case: Claude's updater has already had a race between concurrent updates that left no `claude.exe` (CHANGELOG 2.1.281), and eight parallel spawns invite it.
5. **Windows refusing to replace a binary that running sessions hold.**
   - (a) *Recommended.* Run the update anyway: Cursor keeps versions side by side, Codex standalone swaps a junction, and Claude's native updater handles a held `claude.exe` itself; an in-use failure is `Failed { in_use: true }`, the spawn launches the installed version, and the 10-minute window stops retries. AU.1 measures what each CLI actually does. Worst case: with any Claude session always open, a WinGet Claude never updates and the user gets one toast per daemon run saying so.
   - (b) Skip the update while any live session of that CLI exists. Worst case: a user who keeps one session open all day never gets an update, silently.
   - (c) (a), plus launching Claude's versioned binary (`%USERPROFILE%\.local\share\claude\versions\<v>`) directly instead of the launcher copy, so rustling-tulip sessions never hold `.local\bin\claude.exe`. Worst case: the daemon depends on an undocumented layout, and Claude's version cleanup may delete a version a running session was started from.
6. **Whether the user sees a failed or offline update.**
   - (a) *Recommended.* Every outcome in `daemon.log`, a line in the session's recent actions, and one warning toast per failure streak per CLI. Worst case: an offline laptop shows one "Couldn't update" toast each time the daemon starts.
   - (b) `daemon.log` and recent actions only. Worst case: a CLI that has failed to update for weeks goes unnoticed until someone reads the log.
   - (c) A toast on every failure and every update. Worst case: a preset of eight sessions produces eight toasts.
7. **The opt-out.**
   - (a) *Recommended.* One host setting, `agent_updates` in `state.json` (on by default), with a Settings → General toggle, carried like `keep_awake`. Worst case: a user who wants Claude updated but Codex pinned has to pin Codex another way (its `check_for_update_on_startup = false` does not stop `codex update`).
   - (b) One toggle per CLI. Worst case: three settings for a feature most users leave on.
   - (c) An environment variable on the daemon only (`RUSTLING_TULIP_AGENT_UPDATES=0`). Worst case: the daemon started from the login `Run` entry does not see a variable set later, and a phone or LAN client cannot change it.
8. **The override variables (`RUSTLING_TULIP_CLAUDE`, `RUSTLING_TULIP_CODEX`, `RUSTLING_TULIP_CURSOR_AGENT`).**
   - (a) *Recommended.* A set override skips the update for that CLI, logged once. The e2e fake-claude never meets an update, and a user pointing at a pinned build keeps it. Worst case: a user who set the variable only to pick one of two installed copies gets no updates for it.
   - (b) Update the override's program too. Worst case: the live e2e tier runs `fake-claude.cmd update`, which the shim does not know, and every e2e spawn logs a failure and waits for it.
   - (c) Skip only when the override points inside the repo's `tools/e2e/`. Worst case: a test shim anywhere else gets "updated", and the rule hides a path check in production code.
9. **The CLIs' own background updaters.**
   - (a) *Recommended.* Leave them as they are; the daemon's update before launch is in addition. Worst case: a Claude session's background updater and the daemon's `claude update` run at the same moment and one of them reports the other's lock.
   - (b) Pass `DISABLE_AUTOUPDATER=1` to Claude sessions (and `--disable-auto-update` to Cursor, once AU.1 confirms the flag exists), so the daemon is the only updater while the setting is on. Worst case: with the setting off, nothing updates any CLI, and a user who turned it off to stop network use is surprised that Claude also stopped.
   - (c) Leave them on only when the setting is off. Worst case: the same as (b) with the setting on, and a rule the user has to know to predict.
10. **Which spawns update.**
    - (a) *Recommended.* Every interactive and headless spawn of Claude, Codex or Cursor through `spawn_session`, including presets, duplicates, recover and resume; plain shells never. Worst case: recovering twenty sessions after a reboot waits once on the update and then spawns them all.
    - (b) Only spawns the user starts by hand (not presets, recover or resume). Worst case: a preset-only user never gets an update.
    - (c) Only the first spawn of each CLI after the daemon starts. Worst case: a daemon that runs for a week launches week-old CLIs.
