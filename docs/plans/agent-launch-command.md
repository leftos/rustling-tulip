# A customizable agent launch command (RT-360)

Owner's ask: "make the agent launch command customizable in the Settings so I can pass extra arguments or an alias."

Map: the explorer's report (`.tmp/map-rt360.md` while it exists) on HEAD b3cdf7e4. Every decision below is the orchestrator's (ship-fast mode, `rustling-tulip-nextup`), open to the owner.

## Today

The program is `RUSTLING_TULIP_CLAUDE` or `claude` (`resolve_program`, `crates/agents/api/src/lib.rs`), found on PATH by `resolve_agent_program` (`crates/daemon/src/server/spawn/shell.rs`, which also turns an npm `.cmd` shim into the real executable). Every interactive spawn resolves it in `spawn_interactive_session`, which Restart, Resume, auto-resume and Recover all reach through `spawn_session`. Headless calls `resolve_program` directly. The plugin builds every argument.

## Decisions

1. **A host setting, per agent.** The daemon's `state.json` gains `agent_commands`, a map from agent id to `{ program, args }` (both strings, blank meaning the default). It belongs to the host that spawns: auto-resume runs with no client connected, and a remote client edits the remote host's command. It is read at spawn time and never copied into a session's `SpawnConfig`, so Restart and Resume use the current command.
2. **Program**: blank is the plugin's own program. A set program replaces it and goes through `resolve_agent_program` like the default (the shim fixup included). `RUSTLING_TULIP_CLAUDE` still wins over it; the field then shows locked with the note "Set by RUSTLING_TULIP_CLAUDE".
3. **Extra arguments** go after the plugin's argv. In the direct spawn they are split by POSIX shell-word rules (quotes and backslashes), the one documented rule. In the shell form (RT-359) the program word and the extra arguments are typed verbatim, unquoted, so an alias or function from the shell's profile resolves; the plugin's own arguments stay quoted as RT-359 types them.
4. **Collisions are refused on save**: an extra argument that is a flag the plugin owns is refused naming the flag. The plugin lists its own flags (a new `reserved_args` on the agent seam); Claude's are `--settings`, `--resume`, `--session-id`, `--model`, `--permission-mode`, `--dangerously-skip-permissions`, `--add-dir`, `--append-system-prompt`, `-p`, `--print`, `--output-format`, `--verbose`.
5. **Validation on save**: a newline or other control character is refused in either field (the shell form types the text at a prompt). The daemon does not look the program up at save time, since it cannot see an alias; a bad command shows as a spawn error in the direct form, or as the shell's "not recognized" in the pane.
6. **Headless runs are excluded**: they keep the plugin's program and arguments, since extra arguments could break the stream-json parse. The Settings row says so.
7. **Remote editing is allowed**: an approved tailnet device can already type any command into a shell session, so editing the launch command grants nothing new. The setter is not added to `host_only_refusal`.
8. **Wire**: additive (no protocol bump): `SetAgentCommand { agent, program, args }` refused with an `Error` naming the field and the reason; `AgentCommands { commands }` sent at handshake and on every change; a `Welcome.features` entry so a client greys the rows on a daemon without it.
9. **Settings**: the agent's page (Claude) gains a "Launch command" section with two text rows, Program and Extra arguments, applied on Enter as the Worktrees root field is, with the refusal shown under the row; locked while not connected. A one-line note: "Used for new sessions, Restart and Resume; not for headless runs."
10. **Names stay the agent's**: a session's `program_name` and the history import's program inference keep the agent's name, whatever program the command names.
11. The agent CLI update plan (`docs/plans/agent-cli-update.md`) treats a set program as an override and skips the update, as it does for `RUSTLING_TULIP_CLAUDE`.

## Parts

- [x] **A. Daemon and protocol**: `agent_commands` in `state.rs`, the messages and handler, `reserved_args` on the seam, the program and extra arguments at every interactive spawn form (direct and RT-359's typed shell form). Lands after RT-313 (same files) and RT-359 part A (the typed shell form).

  Settled in the code: `AgentCommands` lists every agent, blank when unset, with `program_env` naming an overriding variable; the feature is `agent_commands`. Claude reserves 16 flags (decision 4's 12 plus `-r`, `--continue`, `-c`, `--fork-session`). Extra arguments with an unclosed quote are refused on save, and a spawn whose stored extras cannot be split is refused pointing at Settings. The typed shell form is `<program word bare> <plugin args quoted> <extras verbatim>`, PowerShell without `&` for a bare word; an env var, a blank setting or a setting naming an existing file keeps the quoted absolute path with the extras applied, and cmd with `%` or `!` in the extras spawns directly. Recover's shell form is unchanged until AP.3b.
- [ ] **B. Native Settings rows**: the Launch command section on the Claude page and the `AgentCommands` arm. Lands after part A.
