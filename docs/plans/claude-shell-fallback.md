# A Claude session keeps its pane as a shell when Claude exits (RT-359)

Owner's ask: "A session started with Claude as the agent shouldn't autoclose once Claude closes. It should offer a little timed toast to close it, but the user should be able to continue using their default shell in that pane otherwise."

Map: the explorer's report (`.tmp/map-rt359.md` while it exists) on HEAD 7d702c79. Every decision below is the orchestrator's (ship-fast mode, `rustling-tulip-nextup`), open to the owner.

## Today

An interactive Claude session's PTY child is `claude` itself under its `rt-tracer.exe`. When Claude exits, `watch_exit` (`crates/daemon/src/session.rs`) writes the history entry and marks the record Stopped with an exit code. The native client then sends `DiscardSession` for a session that exited by itself and has no per-session worktree (`exit_discard` in `apps/native/src/lib.rs`, `self_exited` in `session_actions.rs`), so the pane closes; with a worktree it shows the exited overlay.

## Decisions

1. **Mechanism: the shell is the PTY child and Claude is launched inside it.** An interactive Claude spawn starts the user's default shell (`resolve_shell_program`, `plain_shell_args`, as a plain shell does) and clears the input line and types `<cd to the start folder> && <the Claude command line>` through the `ShellPrompt` injector once the prompt is quiet, the path Recover's "Shell running claude --resume" already takes (`history::shell_request`, `inject.rs`). An unknown shell, a cmd line with `%` or `!`, or legacy PowerShell argument passing falls back to the direct spawn. No tracer change. Rejected: the tracer starting a shell after its child exits (rewrites the tracer's one-child job, kill and `Exited` path); launching through the shell's startup arguments (a script per shell, `cmd /K` the least predictable).
2. **The command line** is the plugin's `interactive_args` with the absolute program path, quoted for the detected shell family (PowerShell, cmd, POSIX sh). The initial prompt is not on the command line: it is typed into Claude's TUI once Claude is up, as a preset's `AgentTui` step is today; it waits for the agent to be up (hook-driven, or the screen reads Claude) and is dropped after 15 s.
3. **The record stays Interactive**, agent Claude, with its spawn config and conversation id, so Restart, Resume, auto-resume and Recover work as today; its status runs on the plain-shell watcher (`watch_plain_shell`), which already follows Claude inside a shell (hook-driven while Claude runs, `hand_shell_back` when the prompt returns). `program_name` follows the child: the shell's name once Claude has ended, `claude` again if Claude is run again in it. The record keeps `agent_in_shell` and `shell_label`, mirrored into the sidecar so a reattach runs the shell's watcher. Recover of an interactive entry uses its start folder, not the shell's last one.
4. **Claude has ended** when its `SessionEnd` hook arrives with a reason other than `clear` (`hook_status.rs`; a `/clear` is not an exit), else when `hand_shell_back` sees the shell's prompt back. The daemon then sends a new additive one-shot `DaemonMessage::AgentExited { session_id }` on the session-event stream, once per Claude run.
5. **The toast**: on `AgentExited` the client raises an ordinary timed toast (today's lifetime, no countdown) reading "Claude exited. The pane is a shell now." with a **Close session** action, which sends `StopSession` then `DiscardSession` with no cleanup (worktrees stay on disk), as `branch_fate::confirm_messages` does. Ignoring the toast keeps the shell.
6. **When the shell itself exits**, today's rules apply (a worktreeless session that exited by itself closes; one with a worktree shows the exited overlay).
7. **Gate**: the plugin's `AgentCaps.shell_resume` (Claude sets it); a plugin without it keeps the direct spawn. Headless runs are unchanged.
8. **Environment**: the plugin's `EnvPlan` and the env rows apply to the shell, so commands run in the shell after Claude see them (a DeepSeek session's routing variables included).
9. **Keep-awake and idle exit** see a live shell (RT-313): Always holds while it lives; When busy only while it prints.

## Parts

- [x] **A. Daemon**: the shell spawn with the typed launch for an `shell_resume` agent, the Interactive record on the plain-shell watcher, `program_name` following the child, `AgentExited`. Files: `server/spawn.rs`, `server/spawn/shell.rs`, `inject.rs`, `pty_state.rs`, `hook_status.rs`, `session.rs`, `crates/protocol/src/lib.rs`, `server.rs`. Lands after RT-267 AP.2b-ii-b (shares `spawn.rs`, `session.rs`).
- [ ] **B. Native client**: the `AgentExited` arm, the toast and its Close action. Files: `apps/native/src/lib.rs`, `notices.rs`, `notice_view.rs`, a UI spec. Lands after RT-352 (shares `lib.rs`).
