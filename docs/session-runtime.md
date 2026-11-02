# Session runtime

How the daemon keeps sessions alive and knows what they are doing: recovery across restarts, status, the chat mirror, workspace spawns, tracers and the git watcher. Components and the task index are in [architecture.md](architecture.md).

## Orphan recovery and tracer reattach

On startup, `main.rs` reads all `meta.json` sidecars and partitions live vs dead via `orphan::is_session_alive`, which uses sysinfo.

A tracer-backed sidecar's `tracer_pid` must still be an `rt-tracer` process whose start time equals the sidecar's `tracer_started_at`, stamped at spawn, so a reused pid is neither reattached nor killed (a sidecar without the stamp, from an older build or an unreadable start time, falls back to the name check). A sidecar without tracer fields matches its `pid` against its `program_name`, which otherwise just names the program the user runs, the leaf's label.

For sidecars with `tracer_pid` + `tracer_pipe` set (post-C.3 spawns), `server::recovery::reattach_orphans` (`crates/daemon/src/server/recovery.rs`) connects to the still-running `rt-tracer.exe` over its named pipe and rebuilds a fully-functional `PtyHandle` — IO, resize, and kill all work as if the session had been freshly spawned.

The sidecar also keeps the session's `busy_excluded` flag (absent means false), kept current on every registry update and restored on reattach, and its last `status`, `status_since` and `status_seq` (the snapshot's status sequence), rewritten on every status change. A reattached session comes back `Idle` or `AwaitingInput` with its old stamp and sequence, and any other stored status comes back `Idle` stamped now with the sequence raised by one.

Its status watcher then reads the replayed screen once its startup hold ends: a visible spinner makes it `Working`, an idle prompt box with no dialog clears `AwaitingInput`, and a screen no rule matches leaves the restored status.

A hook-reported prompt's `pending_input` is written beside `status` (absent when nothing is pending) and restored only with a restored `AwaitingInput`. Every sidecar write goes through `SessionRegistry::sync_sidecar`, which writes the record's mirrored fields under one lock. Reattach failures route the session to the abandoned bucket (sidebar Resume button, B.2 UX).

A reattached interactive session with no conversation id, whose plugin discovers its conversation after launch, gets its discovery waiter and poller again once every orphan has reattached and every abandoned sidecar is inserted, so a sibling's held id is never claimed (`reattach_baseline`, `restart_discovery` in `server/recovery.rs`).

A session that ended before then gets none. In the restarted discovery, launch time is the session's start time, the baseline is empty and the folder is the one it started in, so a conversation created while no daemon watched is still found.

A session whose exit arrives while Windows is logging off or shutting down (`GetSystemMetrics(SM_SHUTTINGDOWN)`, `host_shutdown.rs`) and that the user did not stop is not ended: the daemon writes no history entry, marks it abandoned, takes a final look for its conversation and keeps its sidecar with `ended_during_host_shutdown` set, so the next start records it as `TracerLost` whatever its exit code. A sidecar whose tracer is gone is recorded as ended at start (see the history entry in [on-disk.md](on-disk.md)), and once the daemon has bound and written its handshake, `recovery::auto_resume` resumes the qualifying ones by itself, one at a time, with no prompt, off switch or retry (`claude --resume <that id>`), then broadcasts `ResumeReport`.

A qualifying one is an interactive Claude session with a spawn config, its own conversation id and an existing transcript, its worktrees still pinned to their folders, its in-place checkout still on its branch. The rest stay abandoned, listed in Recover.

Pre-C.3 sidecars without tracer fields fall through to the read-only `insert_orphan` path; their underlying children almost never survive a daemon death, so that branch is effectively only for downgrade scenarios.

## Session status

A Claude session's status comes from its hooks and from rules over its rendered screen (`pty_state.rs`, `screen.rs`, `screen/rules.rs`; design in `docs/plans/screen-status.md`).

The watcher feeds each session's PTY output into a per-session grid (`alacritty_terminal`, the crate the client renders with) and reads it with the session's own agent's rules (`screen/rules.rs`: `CompiledRules` compiles the plugin's `ScreenRules`, `rules_for(agent)` caches one set per agent; Claude's table is data in `crates/agents/claude/src/rules.rs`) a short debounce after output pauses.

A rule table fails closed: one bad rule (a structural problem, an unknown part, an empty pattern, over 128 rules, a pattern over 512 characters, over 8 guards, a regex that does not compile) refuses the whole table (`ScreenRules::validate` in agent-api, `CompiledRules::try_compile`), the daemon logs one error "screen rules: the agent's table is refused; its screen has no rules", and that agent's screen reading sets no status. To see why a live session reads as it does, run `rustling-tulipd screen <session id>`: it prints the rule trace (`CompiledRules::explain`) and the grid.

There is no output/input volume classifier, no title heartbeat and no silence demotion for a Claude session: a quiet screen that still shows the spinner stays `Working`.

A session turns hook-driven on its first mapped hook event that passes the ordering check, from Claude Code's hooks, injected through the settings file its spawn writes. A hook event applies at once, except a main-agent `Stop`, whose `Idle` is held (below). A session whose hooks never run is screen-driven only.

A screen-derived change publishes at once when it enters `AwaitingInput` from a dialog footer or when a spinner line promotes the session to `Working`. Leaving `AwaitingInput` and `Working` to `Idle` publish only after hysteresis: 3 agreeing checks at least 150 ms apart, or the first agreeing check 700 ms after the first.

Every screen write is refused when a hook event, the owner's key or an exit moved the record (status, hook stamp or hook-driven flag) since the read. After any such foreign write, and when the watcher starts, the screen is left unread for 700 ms. A reading from the title alone, an overlay or a screen no rule matches never writes.

A plain shell keeps its own classifier (command output outpacing echo promotes to `Working`, `SHELL_IDLE_AFTER` of silence ends it) until it is hook-driven or its screen has shown Claude for a whole confirmation window; it then runs the same screen check. Once the shell's prompt has been back on the grid's last row for that window, `claude` has ended: the shell leaves hook-driven mode (clearing `background` and ending a `Working` or `AwaitingInput` turn at `Idle`) and returns to its classifier.

An interactive Claude session whose agent is typed into the user's shell (`agent_in_shell`; see "Tracer-backed PTY sessions") runs that same shell watcher. The daemon announces the agent's end once per run with `DaemonMessage::AgentExited { session_id }` on the session-event stream: on its `SessionEnd` hook (a `/clear` reason is not an exit) or, when no hook said so, when `hand_shell_back` sees the prompt return.

The record's `program_name` becomes the shell's label then and `claude` again on the next `SessionStart`; the session itself ends only when the shell exits.

A subagent's hook event (an `agent_id` in its input) never ends or replaces the main agent's pending prompt, never promotes the session to `Working` and never ends a turn at `Idle`, and a subagent `PermissionRequest` raises one only when none is pending.

A main-agent `Stop` does not land `Idle` itself: it parks its report in the runtime-only `SessionRecord.turn_end` (`TurnEnd`), the status stays `Working`, and the `background` flag and any `WaitingAlert` wait in it, so a stop that a Stop hook blocks never reads as finished. A subagent's `Stop` never holds, and `StopFailure` lands `Idle` at once, keeping what a hold already carries.

The screen watcher is the only resolver of a held turn end. The `turn_hooks_running` rule (verdict `hooks_running`) keeps the hold while the hooks row is up; a screen that reads idle lands `Idle`; and with no sign of hooks on the screen, the first settled frame after the foreign-write hold (about 700 ms after the `Stop`) lands it.

`TURN_END_CEILING` (60 s) lands `Idle` on a settled frame that shows no spinner, and drops the hold on a spinner, a title-only `Working` reading or an overlay, writing no `Idle` on any of them.

A main hook event during the hold drops it, since the turn went on, and a later `Stop` replaces it, keeping an earlier alert unless a new one arrives. Any status change takes the hold, and any move to `Idle` lands the alert and the `background` flag it carried, except `hand_shell_back`, which lands the alert and leaves `background` false.

The log says `turn end held`, `turn end landed` (`how`: `screen`, `ceiling`, `no_evidence`, `weak_idle`, `agent_end`, `hand_back`, `status_change`) and `turn end dropped` (`why`: `ceiling_overlay`, `ceiling_spinner`, `hook`, `replaced`, `status_change`). The summarizer's refresher skips a record with a held turn end, so it makes no billed call.

A main-agent `Stop` whose `background_tasks` lists a running task, or whose `session_crons` holds a one-shot cron, sets the snapshot's `background` flag beside `Idle` (RT-84); any status change, a stop, a park or an exit clears it, and it is never written to the sidecar.

The screen sets it too, with no hook: while the record is `Idle` and the screen body reads `Idle`, a `· N shell(s)` item on the mode line under the prompt box or a running-subagent row in the panel (the Claude table's `background_shell` and `background_subagents` rules, verdict `background`, read apart from the state rules by `CompiledRules::read_background`) sets the flag after the same confirmation window as a state change. The screen clears only a flag it set itself, so a hook-set flag survives a footer-less frame, and after a reattach the flag comes back from the replayed screen.

A `Notification` `idle_prompt` does not leave `AwaitingInput` while a hook-reported prompt is pending.

## Chat mirror

The chat mirror serves a Claude session's chat from its transcript and its screen, and types into its PTY (`chat_mirror.rs`, `transcript_tail.rs`, `server/chat.rs`, the plugin's `chat_entries.rs` and `dialogs.rs`; design in `docs/architecture.md` "Chat mirror").

The terminal stays the source of truth and each fact has one source: history from the tailed transcript, the open dialog with its numbered options and the composer state from the status watcher's screen grid read by the plugin's dialog parsers, live answer text from the MessageDisplay hook, and "a prompt is pending" from `pending_input`. No fact falls back to a second source, and the mirror builds no second grid.

A chat message or answer is typed into the PTY as keys under the session's typing lane, gated on the settled screen, and refused (`busy`, `chat_refused`, `invalid_request`) rather than queued by the daemon. A recipe runs group by group with a screen check after each and stops when the screen does not move, the owner types or 30 s pass. The chat never resizes the PTY, and a send or answer is never an owner key.

The running-work strip (`ChatWork`) and the summary view (`Summary`, `SummaryAppend`) ride on the same loaders. Summary lines are written by the Dashboard's summarizer service through its queue, cache and daily cap, and stay template lines (the turn's first user line, its duration and tool count) when no model text comes back. Only the Claude plugin supplies the mirror's parts, so a session of any other agent is refused a chat with `unsupported_message`.

The status watcher also returns the terminal to the main conversation for every Claude session when an agent is in view and a Heads up notice is up, an AskUserQuestion closes, or the box holds the notice's own draft (`view_drive.rs`; design in `docs/architecture.md` "Chat mirror"). It types its keys straight into the PTY, never as owner input: one key per settled read, at most 12 keys and 10 s per drive, no key while a dialog is up. A draft in the agent's box is stashed with ctrl+s only when no stash is held (the stash is one slot), restored in the agent's view afterwards or returned to main's box, and clients hear of it through `DaemonMessage::SessionNotice` on the session-event stream. Every multi-key writer to a PTY (this view switch, the spawn injector, a chat send) takes the session's typing lane (`typing_lane.rs`), and the watcher skips its view return while another writer holds it; the user's own keys never take the lane.

An owner key sent to a hook-driven session with a pending prompt clears `AwaitingInput` and `pending_input` at once (a `SendInput` marked `automatic`, the client's query replies, wheel and pointer-motion reports, never does), and a status watcher write (a screen read or the shell classifier) that moves a session off `AwaitingInput` clears `pending_input` in the same update.

Token and cost figures exist only for headless runs, where they come from the CLI's stream-json (`crates/agents/claude/src/lib.rs`, `parse_headless_line`, applied by `crates/daemon/src/headless.rs`), so an interactive session's snapshot carries none. Headless mode (`claude --print --output-format stream-json`) uses `crates/daemon/src/headless.rs` for structured events.

## Multi-repo workspace sessions

Multi-repo workspace sessions spawn one `claude` process with `cwd = members[0].worktree_path` and `--add-dir <path>` for every additional member. Worktrees are created/reused per member via `git -C <repo> worktree add` under the shared `<worktrees-root>/wt.<branch-slug>/<workspace-slug>/` directory (see [on-disk.md](on-disk.md) — the layout preserves inter-member relative paths within a workspace).

The "reuse-or-create" policy is encoded in `workspace.rs`. VS Code `.code-workspace` files are auto-detected when adding a repo and surfaced as a `VscodeWorkspaceSuggestion` event.

## Tracer-backed PTY sessions

Every interactive and plain-shell PTY child is spawned under a per-session `rt-tracer.exe` supervisor process. The daemon talks to it over an `interprocess` local socket (`tracer_protocol::socket_name`: `\\.\pipe\rt-tracer-<session-id>` on Windows, a Unix socket elsewhere).

The tracer owns the master ConPTY handle and survives daemon restarts — when the daemon dies the tracer keeps draining child output to its internal ring buffer (4 MB cap, oldest-bytes-drop on overflow), and a freshly-started daemon reattaches via `tracer_client::reattach` and replays the ring to catch up.

Spawn site: `crates/daemon/src/tracer_client.rs::spawn`. Tracer ABI: `crates/tracer-protocol/src/lib.rs` — frozen surface; additive changes (new fields with `#[serde(default)]`, new variants with `#[serde(other)] Unknown`) are not a bump. Headless (`claude --print`) does NOT go through the tracer — it's piped stdio and lives in `crates/daemon/src/headless.rs`. The tracer binary must be present next to `rustling-tulipd.exe`: `cargo build` emits both into `target/<profile>/`.

An interactive Claude session's tracer child is the user's shell (`RUSTLING_TULIP_SHELL`, else the platform's usual shell), not `claude`; the tracer never runs or knows the agent, and only logs an optional `--agent-note <json>` (given before the program, for an agent typed into the shell and never for env rows) as the line `rt-tracer agent note agent_note=<json>` after `rt-tracer starting`, which the tracer-log import reads (`server/spawn.rs` `shell_agent_note`, `tracer_log.rs`).

Once the prompt is quiet the daemon clears the input line and types `<cd to the start folder> && <quoted claude command line>` (`server/spawn/shell.rs` plans and quotes it per shell family, `inject.rs` `run_chain` types it).

The initial prompt or a preset's script is typed into Claude's TUI only after Claude is up (`wait_for_agent`, `pty_state::agent_up`), and dropped after 15 s with the warn "the agent did not start; the initial prompt was not typed".

A shell outside pwsh, powershell, cmd, sh, bash, zsh, dash and ksh (and WSL bash), a cmd line carrying `%` or `!`, and legacy PowerShell's argument passing host no agent: Claude is then the PTY child, as it is for a plugin without `shell_resume`.

So does an agent that runs through cmd.exe (a `.cmd` or `.bat` shim, or cmd.exe itself) typed into pwsh, powershell or a POSIX shell when an argument holds `%`, `!`, `"`, `^`, `&`, `|`, `<`, `>`, CR or LF; the direct spawn then refuses any such argument with a `Couldn't start session` failure naming the character and the argument, before the tracer starts (`direct_args` in `server/spawn/shell.rs`).

`daemon.log` says which fallback ran ("the agent's command line cannot be typed into this shell; spawning the agent directly", "the agent cannot be typed into this shell; spawning the agent directly", "no shell to host the agent; spawning the agent directly").

A hosted launch logs "spawn_interactive_session: the agent is typed into the shell".

## Live git watcher

The watcher is `crates/daemon/src/git_watch.rs`. Each registered repo gets a recursive `notify`-based watcher with a 750 ms debounce. Inside the debouncer callback, `classify_event` filters paths against a hand-maintained allowlist: only `.git/index`, `.git/HEAD`, `.git/refs/**`, `.git/reftable/**` (a reftable-backend repo's refs; it wakes the stash refresh too), a handful of in-progress operation markers, and any non-excluded working-tree path can wake the refresher.

`.git/objects/`, `.git/logs/HEAD`, `FETCH_HEAD`, lock files, and well-known build/cache dirs (`target/`, `node_modules/`, `dist/`, `.next/`, `.venv/`, `__pycache__/`, …) are ignored so a `cargo build` or `pnpm install` doesn't spin up `git status` forever.

The refresher tracks two flags — `status` and `stash` — and only invokes `git stash list` when a stash ref actually changed. The refresher parks while `Hub.client_count` is 0 (an RAII `ClientCountGuard` in `client_session` maintains the count); the next reconnect triggers one catch-up `repo_status` + `stash_list` before resuming event-driven refresh. The daemon accepts multiple WS clients, so each client window opens its own connection.
