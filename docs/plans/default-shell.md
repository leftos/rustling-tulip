# Default shell setting

Each host's Settings chooses the shell that plain-shell sessions run and that hosts an interactive agent: Automatic (today's order), one of the common shells the host has installed, or a custom command. Linked from RT-844.

## Today

`crates/daemon/src/server/spawn/shell.rs` `resolve_shell_program` picks the shell: `RUSTLING_TULIP_SHELL`, else the first of `pwsh.exe`, `powershell.exe`, `cmd.exe` on PATH (Unix: `$SHELL`, then bash, zsh, sh). Its two callers are `host_shell` (an agent typed into a shell) and `spawn_plain_shell_session` (plain shells, quick shells, `rustling-tulipd spawn`, Recover) in `crates/daemon/src/server/spawn.rs`.

Whether a shell can host an agent is `ShellFamily::of` (from the program's file stem): PowerShell, cmd and POSIX shells can; anything else spawns the agent directly. Shell integration (`plain_shell_args`) exists for pwsh/powershell, cmd (OSC 7 only), bash and zsh.

## Decisions

Orchestrator decisions, open to the owner:

1. **Per host.** The setting lives in each daemon's `state.json` beside `agent_commands`, as `#[serde(default)] default_shell: Option<DefaultShell>`, where `DefaultShell { id: String, command: Option<String> }`. Absent, or an id this build does not know, reads as Automatic. No released shape has the field, so no migration; a test loads a `v0.5.1-alpha`-shaped `state.json` and reads Automatic.
2. **The choices.** Windows: Automatic, PowerShell 7 (`pwsh`), Windows PowerShell (`powershell`), Command Prompt (`cmd`), Git Bash (`git-bash`), WSL (`wsl`), Nushell (`nu`), Custom. macOS and Linux: Automatic, bash, zsh, fish, Nushell, sh, Custom.
3. **Detection is the daemon's**, since the shell runs on its host. It reports each choice with `available`, the resolved `path` and `hosts_agents` in a new `DaemonMessage::ShellChoices`, sent only in answer to a `ClientMessage::ListShellChoices` the Settings page sends when it opens, so a shell installed since connect shows up; connect sends `DefaultShell` alone.
   - PATH lookup (`which`) for pwsh, powershell, cmd, nu, fish, zsh, bash (Unix only), sh.
   - Git Bash: from `git.exe` on PATH (`<root>\cmd\git.exe` → `<root>\bin\bash.exe`), else `%ProgramFiles%\Git\bin\bash.exe`, else `%LOCALAPPDATA%\Programs\Git\bin\bash.exe`. Never `which bash` on Windows, which can return the WSL launcher.
   - WSL: `%SystemRoot%\System32\wsl.exe` exists. Distros are not listed (that costs a process and UTF-16 parsing); a WSL with no distro fails at spawn with WSL's own message on screen.
4. **Resolution order at spawn:** `RUSTLING_TULIP_SHELL` first (tests and the dev daemon rely on it), then the setting, then Automatic. A chosen shell that is not available at spawn falls back to Automatic with a `warn` (`shell: chosen shell unavailable; using automatic`, fields `id`, `path`), so a Recover never fails on a removed shell.
5. **Custom command** is split by the Windows command-line rules on Windows (backslashes kept, `"` groups) and by POSIX shell words elsewhere; a command that names an existing file whole is the program. The first word is the program, its file stem the label, and the program is resolved: a path must exist and a bare name goes through PATH, else the spawn falls back to Automatic with the unavailable warn. With no arguments of its own it gets that label's integration (a custom path to `pwsh.exe` behaves like pwsh); with arguments it runs exactly as given. Saving refuses a control character or an unclosed quote (`launch_args_refused`) and a command with no program (`launch_program_refused`).
   - **Recover's resume line**: a Recover that types a resume line into a shell that cannot host it (nu, fish, WSL until DS.3) runs Automatic instead, with the info log `shell: chosen shell cannot host the resume line; using automatic`.
6. **Hosting an agent** goes by `ShellFamily::of`. Git Bash classifies as POSIX by its stem: a test pins `launch_line` for a Windows cwd and agent path under it. WSL, Nushell and fish get families of their own in DS.3, so an agent is typed into them too; until DS.3 lands they keep the direct spawn, and the Settings row says so under the dropdown: "Agents start on their own, not inside {shell}."
7. **WSL's plain shell** runs `wsl.exe --cd <cwd>` with label `wsl`; Nushell and fish run bare. None gets an integration script (both emit OSC 7 and 133 themselves in recent versions).
8. **Remote edit** is allowed, as for the agent launch command: a tailnet device can already type any command into a shell.
9. **Live sessions keep their shell**: the setting is read at spawn, as the agent command is; Restart and Resume take the current value.
10. **Settings UI** (native): a Terminal row "Default shell" on each host's Settings page, a dropdown built from the sound dropdown's list with unavailable entries at `DISABLED_OPACITY` and a tooltip "Not installed on {host}"; choosing Custom shows a command field below it. A daemon without the `default_shell` feature shows the row disabled with the launch row's unsupported tooltip pattern.

## Steps

- [x] DS.1 Daemon and protocol: `DefaultShell` in `crates/daemon/src/state.rs`, detection and resolution in `shell.rs` (`resolve_shell_program` takes the setting), `SetDefaultShell` / `DefaultShell` / `ListShellChoices` / `ShellChoices` messages and feature string `default_shell` in `crates/protocol/src/lib.rs`, the handler arm in `crates/daemon/src/server/settings.rs`, the connect send in `server/handshake.rs`, the Git Bash `launch_line` test, the old-shape `state.json` test. Docs: `docs/on-disk.md`, `docs/wire-protocol.md`, `docs/session-runtime.md`, CLAUDE.md's `RUSTLING_TULIP_SHELL` row.
- [ ] DS.2 Native Settings row: `apps/native/src/settings_view.rs` (per-host mirror, dropdown with disabled entries, Custom field, unsupported state), the folds in `apps/native/src/lib.rs`, a UI spec per state, a `native-shot` of the row. Docs: `docs/native-client.md`.
- [ ] DS.3 Host agents in WSL, Nushell and fish: a `ShellFamily` each in `shell.rs` with its own quoting, `launch_line` and `clear_line` (WSL: Windows paths to `/mnt/<drive>/…`, the agent CLI the distro can run), the plugin's `shell_resume` line typed the same way, and the Settings note dropped for them. Docs: `docs/session-runtime.md`.
