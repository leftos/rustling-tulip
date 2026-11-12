# Follow dispatch runs

Status: design, no step started. Linear: RT-31 ("Follow DeepSeek dispatch runs as read-only sessions"); `docs/plans/MAIN.md` is a snapshot generated from Linear, so nothing here edits it by hand. Terms this doc adds (**live-run file**, **live-run folder**, **stop marker**, **pane follow**) go into the `docs/README.md` glossary in DF.13; **dispatch run** is already there.

## Problem

The user-level `dispatch` tool (`~/.claude/skills/dispatch`, launched as `uv run --project <package> python -m dispatch`) runs an implementer brief as `claude -p --agent implementer --output-format stream-json --verbose`, usually against a DeepSeek backend. `executor.run_claude` opens `<run_dir>/stream.jsonl` and hands it to the child as stdout, so the stream exists only under the worktree's `.tmp/dispatch/<stamp>-<backend>/` folder until `summary.txt` and `report.md` land.

The user wants to watch a running dispatch in the native client, and to stop one that is going wrong, without the dispatch tool ever needing the daemon.

The native client shows a live run as a read-only leaf (tag `DS`, the brief's title) in the repo or workspace container its worktree belongs to. A click opens the run's stream in a pane of a daemon tab, so it can be split and placed beside the terminal that launched it. The user can watch and Stop it (kill the run's process tree), never message it.

## Design

### 1. What the tool writes today, and what it adds

Each run folder already holds a `run.json` the tool writes before the spawn (`run._write_run_json`: `backend`, `contextTokens`, `sessionId`, `runner_pid`, `runner_start`) and extends after `Popen` (`executor.record_child`: `child_pid`, `child_start`). The `*_start` fields are Windows FILETIME integers from `procinfo.process_start`, null off Windows. The tool's own `dispatch watch` follower checks liveness with them (`procinfo.runner_alive`, an exact creation-time compare).

`run.json` is per worktree, so a reader would have to find and watch every worktree's `.tmp` tree, and it has no title, tier or worktree. The tool therefore adds one central record per running `claude` process, the live-run file (section 3), in one folder the daemon watches (section 2). The tool never depends on the daemon: it writes the file whether or not anything reads it.

### 2. Live-run folder

The daemon resolves one folder, in order:

1. `RUSTLING_TULIP_DISPATCH_DIR` when set and non-empty (made absolute against the current dir). The live tier and driven runs point it under `.tmp` (section 13). A set value that cannot be made absolute leaves no folder, never the agent's: an isolated daemon must not read the owner's real runs.
2. Else the first folder a registered agent plugin names through a new defaulted `Agent` method (`live_runs_dir()`, `None` by default; the `AgentPlugin` slot and blanket adapter carry it). The Claude plugin returns `$DISPATCH_LOG_DIR/live` when that variable is set, else `<user home>/.claude/dispatch/live`: the dispatch tool's own rule (`log.py`'s `log_dir()`), which ignores `CLAUDE_CONFIG_DIR`, so the plugin does too rather than following the config dir its transcripts use.
3. Else no folder: the daemon runs no watcher and does not list the feature (section 5).

The resolution lives in `crates/daemon/src/paths.rs` as `resolve_dispatch_live_dir() -> Option<PathBuf>`, beside `resolve_push_dir` and `resolve_tailnet_dir`. The daemon creates the folder before watching it, since a `notify` watch on a missing folder fails.

Why the tool's tree and not the RT config dir: the tool writes only under its own directory whether or not rustling-tulip is installed, and never has to know RT's `ProjectDirs` layout or honour `RUSTLING_TULIP_CONFIG_DIR`. Why a plugin method: the default path contains `.claude`, which `tools/check-agent-names.ps1` rejects outside `crates/agents/claude/`; the plugin owns where its CLI keeps things, so no allow-list entry is needed.

### 3. Live-run file

One file per `claude` process: `<live>/<run_id>.json`, UTF-8 JSON, LF, written as `<run_id>.json.tmp` then `os.replace`, so a reader never sees half a file. `run_id` is `<run dir name>-<child_pid>`, since the run dir name alone can collide across two worktrees started in the same second. A resume, automatic or manual, is a new `claude` process with its own run dir, so it gets its own file and its own leaf.

The pid fields use `run.json`'s names and units, so the daemon's guard is the same exact check the tool's `runner_alive` makes.

```json
{
  "schema": 1,
  "run_id": "20260928-141503-deepseek-flash-51234",
  "title": "DF.5 Daemon watcher and tailer",
  "backend": "deepseek-flash",
  "tier": 2,
  "session_id": "6f1c0d6e-1a7b-4c39-9d0e-3b8a2f1e7c55",
  "worktree": "D:\\orca\\workspaces\\rustling-tulip\\acornworm",
  "run_dir": "D:\\orca\\workspaces\\rustling-tulip\\acornworm\\.tmp\\dispatch\\20260928-141503-deepseek-flash",
  "stream": "D:\\orca\\workspaces\\rustling-tulip\\acornworm\\.tmp\\dispatch\\20260928-141503-deepseek-flash\\stream.jsonl",
  "child_pid": 51234,
  "child_start": 134036037030000000,
  "runner_pid": 50120,
  "runner_start": 134036037010000000,
  "started_at": "2026-09-28T14:15:03Z",
  "resumed_from": null,
  "auto_resume": false
}
```

- `schema`: 1. The daemon skips a file whose schema it does not know, logging it once; unknown extra fields are ignored, so the tool may add fields without a bump.
- `title`: the brief's first `# ` heading, else its first non-empty line, trimmed to 120 chars. A manual resume uses `Resume: ` plus the message's first line; the automatic resume uses the parent's title plus ` (auto-resume)`.
- `backend`: an opaque string, shown on the leaf's hover and the view's header. No code matches on it.
- `tier`, `resumed_from` (a conversation id), `auto_resume`: optional, shown on hover and in the view's header.
- `session_id`: the dispatching session's `CLAUDE_CODE_SESSION_ID` (the same value as `run.json`'s `sessionId`), nullable for a run started from a plain shell. It links the run to the RT session that launched it (section 7) and gives RT-499's Dashboard nesting a direct link.
- `agent`: optional agent id naming the stream's format; absent means `agent_registry::default_agent()`. The tool omits it.
- `worktree`: the `--root` the run works in. `run_dir`: the run's output folder. `stream`: the stream file the daemon tails.
- `child_pid`, `child_start`: the `claude` child; Stop kills this tree. `runner_pid`, `runner_start`: the Python dispatch process, shown on hover, used by nobody to kill.
- `started_at`: UTC, written right after `Popen`; the guard's fallback when `child_start` is null (section 6).

A **stop marker** `<live>/<run_id>.stop` (empty file) is written by the daemon just before it kills a run (section 8). The watcher ignores `*.tmp` and `*.stop`.

### 4. Model: a separate message family, not a `SessionSnapshot`

A dispatch run gets its own snapshot and messages rather than a new `SessionMode`. `SessionMode` has no `#[serde(other)] Unknown`, so a new mode is a breaking protocol change.

Reusing `Headless` instead would let a client try to stop, discard, recover, park or tab the run as a session, putting a "not a dispatch" guard on every session path (`history/`, the recover dialog, `meta.json` sidecars, attention and notifications, `ListWorktrees` in-use marking, idle-exit blockers, tab placement). The daemon does not own the process, so most of those actions mean nothing for it.

The cost is a second list in the native client's sidebar model and a view of its own. Nothing is persisted by the daemon: the live-run folder and the stream files are the state, apart from a pane's follow (section 11), which lives in the tab layout.

### 5. Protocol (additive, no version bump)

New types in `crates/protocol/src/lib.rs`:

- `DispatchRunSnapshot { run_id, title, backend, tier: Option<u8>, worktree, main_checkout: Option<String>, run_dir, started_at, status: DispatchRunStatus, metrics: SessionMetrics, recent_actions: Vec<String>, agent_session_id: Option<String>, session_id: Option<String>, resumed_from: Option<String>, auto_resume: bool }`. Every field after `status` is `#[serde(default)]`.

  `main_checkout` is the worktree's main checkout, resolved by the daemon (section 6). `agent_session_id` is the run's own conversation id from the stream's `init` line; `session_id` is the dispatching session's, from the file.
- `DispatchRunStatus { Starting, Working, Stopping, Ended, #[serde(other)] Unknown }`: `Starting` until the stream's first status event (the `init` line), `Working` after it, `Stopping` between a Stop and the run's removal, `Ended` after the stream's final status (the `result` line; the file usually goes moments later).
- `FEATURE_DISPATCH_RUNS` (`"dispatch_runs"`) in the `FEATURE_*` block. `Welcome.features` lists it only when the daemon watches a live folder, as `gate_state` is listed only where the hub holds a gate state. A client draws no `DS` leaves from a daemon that does not list it.

`DaemonMessage` variants:

- `DispatchRuns { runs: Vec<DispatchRunSnapshot> }`: the full list, sent after `Welcome` beside `Sessions`, and again to a connection whose broadcast stream lagged.
- `DispatchRunUpdated { run: DispatchRunSnapshot }`: a run appeared or changed; at most one per run per tailer poll (250 ms), every line of a poll folded into it. Updates and removals are sent under the runs lock, so a client sees them in the order the runs changed.
- `DispatchRunRemoved { run_id }`: the file went, or the pid died.

`ClientMessage` variant:

- `StopDispatchRun { run_id, request_id: Option<String> }`. Success shows as `DispatchRunUpdated` (`Stopping`) then `DispatchRunRemoved`; failure is `DaemonMessage::ActionFailed` with the `request_id` echoed to the requester only. A second Stop for a run already `Stopping` is a no-op.

The phone app's types are regenerated (`cargo run -p protocol-ts`, committing `apps/pwa/src/protocol.ts`); the phone app shows nothing for these messages yet.

### 6. Daemon: watcher, tailer, liveness (`crates/daemon/src/dispatch_runs.rs`, new)

- **Watcher.** One `notify_debouncer_full` debouncer on the live-run folder, `RecursiveMode::NonRecursive`, 100 ms, as `git_watch.rs` sets one up. A change to a `.json` file re-reads that file through one idempotent `upsert(run_id)`; the removal of a `.json` file removes the run. A full folder scan runs at startup and every 30 s and calls the same `upsert`, covering missed events. A file that fails to parse is retried on the next event or scan, never dropped on one failure.
- **Main checkout.** On first sight the daemon resolves `main_checkout`: a worktree whose `.git` is a gitfile pointing under `<repo>/.git/worktrees/<name>` resolves to `<repo>` through `repo_path_for_worktree` (moved from `worktrees_admin.rs` into `git.rs` as `pub(crate)`). A plain checkout or a missing or unreadable worktree leaves it `None`, and the client places the run by its worktree path, as an unregistered folder's sessions are placed. The function moves unchanged, so a submodule-layout gitdir resolves to its super repo, which places the run under a real repo.
- **Tailer.** One task per run, polling the file's `stream` path every 250 ms from its last byte offset. It keeps a trailing partial line until its newline arrives, resets to 0 if the file shrinks, waits for a stream file that does not exist yet, and stops on the run's removal (a cancel token). The first read replays the whole file, which rebuilds a run's state after a daemon restart or a late discovery (a stream is a few MB at most).
- **Parsing.** Each line goes through `agent_registry::plugin(&agent).parse_headless_line(line)`, the same seam a headless session uses (`crates/daemon/src/headless.rs`), so the daemon never names the parser.

  The events feed a pure accumulator, `StreamState { status, metrics, recent_actions, agent_session_id }` with `apply(&mut self, HeadlessEvent)`, which the headless session path shares. `HeadlessEvent` gains an additive `SessionId(String)` that the Claude plugin emits from the `system`/`init` line. The 200-entry recent-actions cap (`RECENT_ACTIONS_CAP`) stays in one place.
- **Liveness.** Every 2 s the daemon checks each run's process guard, which answers alive, dead or unknown, dead only on evidence, as the tool's `runner_alive` does. On Windows it opens the `child_pid` process: no such pid is dead; any other open failure (another account's process, a transient error) is unknown; an exit code read that is not still-active is dead; a creation FILETIME (`GetProcessTimes`) read that differs from `child_start` is dead; an unreadable one is unknown. When `child_start` is null (a tool off Windows), it compares the process's start time from `sysinfo` in whole seconds with `started_at`, accepting 60 s before to 5 s after; a start it cannot read is unknown. Unknown counts as alive: the run stays listed and its file is kept.

  A run whose guard says dead is removed and its live-run file deleted: the tool crashed or was killed before its `finally`. An orphan `.stop` with no `.json` beside it is deleted too. The tool never deletes other runs' files.
- **Idle exit and keep-awake.** A live run does not keep the daemon running (`idle_exit.rs` is unchanged); a daemon that exits loses nothing, because the next one rescans and replays. A run linked to a session (section 7) makes that session read as busy to keep-awake, which builds its busy set from session snapshots as the native client does, so the "When busy" mode keeps the machine awake while the dispatching session's run works; a run linked to no session does not.

### 7. A linked run counts as background work

A session's snapshot reports `background: true` while a live run whose `session_id` equals that session's `conversation_id` exists. It is computed in `SessionRecord::snapshot()` as `record.background || a live run is linked`, never written into `record.background`, whose clear-on-status-change rule would otherwise fight a run that is still going.

Mechanism: the session registry owns one shared `DispatchLinks` set (conversation ids with at least one live run), and each record gets a clone of the handle when the registry inserts it, so every existing `snapshot()` caller sees the link without a new argument.

When a run linked to a conversation appears or goes, the watcher updates the set and then pushes a `SessionUpdated` for each session with that conversation id through the registry's usual update path. A record whose conversation id changes picks the link up on its next snapshot.

Why one home: every reader of `SessionSnapshot.background` (the sidebar and footer, the Dashboard's "waiting on background", push's Finished-versus-Needs-you decision in `push/trigger.rs`, the CLI's `bg` column, RT-91's clear-and-continue refusal) learns about live runs with no change. The daemon's own readers of `record.background` (the turn-end hold in `hook_status.rs`, the Dashboard's now line in `dashboard/mod.rs`) keep reading the record's flag on purpose; `idle_exit.rs` reads neither, and `keep_awake.rs` reads the snapshots, so it counts a linked run (section 6).

A run with no `session_id`, or one matching no session, affects no session. A `/clear` in the dispatching session starts a new conversation id, after which the run no longer links to it; that is accepted.

### 8. Stop

`StopDispatchRun` runs on its own task (it does I/O):

1. The run is unknown or already removed: `ActionFailed "The run already ended"`, nothing killed. The run is `Stopping`: no-op.
2. Open the guard (section 6) and keep it open. A failed guard: `ActionFailed "The run already ended"`.
3. Mark the run `Stopping` and broadcast it.
4. Write `<run_id>.stop`.
5. Kill the tree: `taskkill /T /F /PID <child_pid>` with `CREATE_NO_WINDOW`, the same tree kill the tool uses on timeout (`gitops.kill_tree`). On Windows a process id is never reused while a handle to its process is open, so holding the guard's handle across `taskkill` closes the pid-reuse window. Off Windows the daemon kills the single matched process through the same `sysinfo` process object it checked, as the tool does.
6. A failed `taskkill`: `ActionFailed` naming its exit status, and the run returns to its previous status. The marker stays; the tool removes it with the live file.

Killing `claude`'s tree, not the tool's, lets the Python process see the child exit, see the marker, record the run as `stopped` and skip its automatic resume, then remove its own live-run file. A bare kill of `claude` without the marker would get the run auto-resumed, since the tool resumes a run that exits with code 2 and a session id.

A tailnet client may Stop a run, by the same rule as `StopSession`, which the daemon allows from a tailnet connection.

### 9. Restart, crash and many clients

- **Daemon restart mid-run.** The run is not the daemon's child, so it keeps going. The new daemon's startup scan finds the file, the guard passes, and the tailer replays the stream from byte 0, so clients get the full state. A Stop issued just before the restart has either killed the tree or not happened.
- **Tool crash** (Python killed, `claude` orphaned or dead): while `claude` lives, the run stays listed and Stop still works through `child_pid`. Once `claude` is gone, the next liveness tick drops the run and deletes the file. **Machine reboot**: every pid is gone, and the startup scan drops and deletes every stale file.
- **Pid reuse**: the exact creation-time compare rejects a recycled pid, so the daemon never lists, or kills, an unrelated process.
- **Many clients**: every connection gets the same broadcasts. A Stop from one shows `Stopping` and then removal on all of them.

### 10. Races: actors, invariants, sites

Actors:

1. The dispatch tool (Python runner): writes `<live>/<run_id>.json` (tmp then `os.replace`), removes it and any `.stop` in a `finally`, and reads `.stop` after the child exits.
2. The `claude` child: appends to the stream file under `child_pid`.
3. The daemon watcher: debounced events plus the 30 s rescan, both calling one idempotent `upsert(run_id)`.
4. One tailer task per run.
5. The liveness tick, which drops runs with a dead or reused pid and deletes their file.
6. The Stop handler on a connection task.
7. Clients, which receive `DispatchRunUpdated` and `DispatchRunRemoved` and may send a second Stop.
8. The session registry, whose snapshots read the `DispatchLinks` set.

Invariants (each gets a test):

- A reader never acts on half a live-run file: the tool replaces atomically, and the daemon retries a parse failure on the next event or scan rather than dropping the run.
- `DispatchRunRemoved` is sent at most once per `run_id`; an update for a removed id is ignored until a new file appears.
- The marker is written before the kill, so the tool can never see its child exit without the marker (order: guard, `Stopping`, `.stop`, `taskkill`).
- Stop on a removed run or a failed guard answers `ActionFailed` and kills nothing; a second Stop while `Stopping` is a no-op.
- The guard's process handle stays open from the creation-time compare until `taskkill` returns, so the pid cannot be reused in between (Windows). Off Windows a window between check and kill remains, as it does in `orphan.rs`'s session kill.
- The tailer replays from byte 0 on first sight, never emits a partial line, waits for a stream file that does not exist yet, and stops when its run is removed.
- The daemon's deletion of a stale file and the tool's `unregister` may race; both tolerate a missing file.
- On a daemon restart, the startup scan runs before the tab prune (section 11), so a pane following a run that survived keeps its follow.
- The `DispatchLinks` lock is never held while taking a record lock: the watcher updates the set, releases it, then pushes the session updates.
- A stopped run is never auto-resumed (tool side); a resume's own run dir gets its own file and leaf.

Sites: `crates/daemon/src/dispatch_runs/` (watch, scan, tailer, liveness, guard, `links.rs`), `crates/daemon/src/server/dispatch_runs.rs` (Stop), `crates/daemon/src/server/connection.rs` (broadcast forward and lag resend), `crates/daemon/src/server/handshake.rs` (initial list), `crates/daemon/src/session.rs` (snapshot), `crates/daemon/src/main.rs` (startup order).

Tool-side sites: the tool's `executor.py` (register, unregister, marker check) and `run.py` (skip resume). The tool's `dispatch watch` liveness check is independent and deletes nothing of the daemon's. Tests wait on events or observations with a hang guard, never on wall time (`docs/testing.md`, `load-proof-tests`).

### 11. Pane follow: a run's view in a daemon tab

`GridNode::Pane` gains `follow: Option<PaneFollow>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`). `PaneFollow` is an internally tagged enum with `DispatchRun { run_id }` and `#[serde(other)] Unknown`. RT-30 (read-only subagent streams, `docs/plans/subagent-streams.md`) adds its subagent variant to the same enum rather than a second pane field. A follow names what a pane shows instead of a session, so no fake session id ever reaches a session path.

Rules:

- A pane holds either a session or a follow, never both. `CreateTab`, `SplitPane` and `ReplacePaneSession` gain the same defaulted `follow` field; a message carrying both a session and a follow is refused with `Error` (echoing `request_id` where the message has one). `ReplacePaneSession` with a session clears the pane's follow.
- A pane with a follow and no session is not empty: `has_any_session` and the empty-pane rules of rearrange, re-fit and close treat it as bound. `MovePane` and `SwapPanes` carry the follow with the pane. The pane's `host` names the daemon whose run it follows, as it names a session's.
- The daemon never clears a follow when its run is removed: a pane exists only because a user opened the run, and Q2 keeps an open viewer's last state until it is closed. Each client keeps the last snapshot of a followed run in memory until no pane follows it; a client with no snapshot (it connected after the run ended) draws "This run has ended" with the run id.
- At daemon startup, after the first live-folder scan, a local pane whose `DispatchRun` follow names no live run is cleared, beside `tabs::prune_sessions_not_in`, the same boundary at which a pane loses a session that did not survive the restart. A pane of another host is not this daemon's to judge.
- Old tab layouts load unchanged (the field defaults to `None`), pinned by a test that loads a tab layout in the oldest supported release's shape. An older client ignores the field and sees an empty pane.

### 12. Native client

- **Model.** Each host's `HostSlice` keeps its own `dispatch_runs`, folded from the three daemon messages in `RootView`'s message handling (`net.rs` only forwards). `build_containers` places each run with `find_container_for_cwd(main_checkout.or(worktree))`: a registered repo's container, the workspace it belongs to, else a `Dir` container for that path, keyed `{host key}::{key}` like a session's. `Leaf` gains `kind: LeafKind { Session, Dispatch }`.
- **Leaf.** Runtime tag `DS`, label the run's `title`, status dot `Starting→Spawning`, `Working→Working`, `Stopping|Ended→Stopped`, `Unknown→Stopped`. Dispatch leaves sort after the container's sessions, newest first, and are not reorderable. In the tabs view they sit in Unbound unless a pane follows them. Hover: backend, tier, started time, worktree; the snapshot carries no runner pid, and adding it is RT-1558.
- **Open.** A click focuses the pane already following the run, else sends `CreateTab` with a `DispatchRun` follow; dropping the leaf on a pane edge sends `SplitPane` with the follow, so the run can sit beside the terminal that launched it. The leaf's context menu offers only Open, Stop and "Copy run folder".
- **View** (`apps/native/src/dispatch_view.rs`, new; the native client has no headless view to share). `grid_view.rs` draws a pane with a `DispatchRun` follow through it, and a pane with an `Unknown` follow as a muted note. Header row: the title, `DS · <backend> · tier <n>`, the status, and a **Stop** button. Below it a stats line `{status} · {cost} · {in} in · {out} out` (cost and tokens through `dashboard_rows::format_cost` and `format_tokens`) and the snapshot's `recent_actions` in the terminal font, newest at the bottom, scrolled to the end while the user has not scrolled up (a scroll per pane, reset when the pane follows another run). There is no input box. The header row is the pane's own header: title, chip and status, with Stop a fixed control outside the narrow-header overflow.

  The view shows those 200 lines and nothing more: no "Show all" and no message that loads the full stream. The run folder, which the menu copies, holds the full `stream.jsonl`.
- **Stop.** The view's button and the menu item arm, then confirm, like the session menu's destructive items; confirming sends `StopDispatchRun` with a `request_id`, and an `ActionFailed` for it shows as other action failures do. The view shows no pending-question line for a run blocked on `dispatch ask`.
- **When the run goes.** The leaf goes on `DispatchRunRemoved`. A pane following it keeps the run's last snapshot, shows `ended` and the run folder path, and closes only when the user closes it. The stats line then starts with `ended` and a line reads "Ended. Run folder: {run_dir}"; a whole `DispatchRuns` list that leaves the run out ends it too. A followed run the client never saw reads "This run has ended: {run_id}". A `Welcome` snapshots the host's followed runs before the slice is forgotten (an address change does so before the remote slice clears), so the re-list decides, and a reconnect to a daemon without `dispatch_runs` ends them. The live list wins over a kept snapshot, and an entry no pane follows is dropped when the tabs change.

### 13. Isolation

A test or driven daemon must never show, or stop, the owner's real runs. `RUSTLING_TULIP_DISPATCH_DIR` therefore joins:

- the isolated daemon's environment in `crates/dev-daemon/src/env.rs` (`{scratch}/dispatch-live`, as `RUSTLING_TULIP_PUSH_DIR` is), which the live tier's `LiveDaemon`, the driver MCP's launches and the smoke tier's driven client all pass on through `IsolatedDaemon::envs()`, so neither `rt.ps1` nor the client profile needs it (the daemon reads it itself in `paths.rs`);
- the `isolation` check: a `DISPATCH_DIR_VAR` constant in `crates/daemon-client/src/isolation.rs`, checked with `check_isolated_var` by the driver gate (`apps/native/src/driver_gate.rs`), as `REMOTE_FILES_DIR_VAR` is, and by the isolated daemon's launch (RT-1541). It stays out of `ISOLATED_DIR_VARS` so the update sandbox and `folder-bench`, which never read runs, are untouched.

### 14. The agent seam

`check-agent-names.ps1` scans `crates/`, `apps/` and `tools/`. The design keeps every agent name inside `crates/agents/claude/`: the default folder comes from the plugin (section 2), the stream parser is reached through the registry (section 6), `backend` is an opaque string, and fixtures that spell a backend or `.claude` path live in test code only. `DS`, "dispatch", `dispatch_runs` and `RUSTLING_TULIP_DISPATCH_DIR` name no agent. Doc comments say "dispatch run", never the backend's vendor.

## The tool change (user-level, outside this repo)

This repo's steps are dormant (an empty folder, no leaves) until the dispatch tool writes live-run files. The change is requested through `~/.claude`'s inbox (`cross-repo-coordination`) with the schema in section 3 and made there under the tool's own gate (`uv run ruff check . && uv run ruff format --check . && uv run ty check && uv run pytest -q`).

- **New `dispatch/live.py`.** `live_dir()` returns `log_dir() / "live"`. A `LiveRun` dataclass with the section 3 fields; `register(run) -> Path` (tmp then `os.replace`, UTF-8, LF); `unregister(path)` (missing-file tolerant, also removes a leftover `<run_id>.stop`); `stop_requested(run_id) -> bool`. A write failure warns on stderr and never fails the run, the same policy as `log.record_run`.
- **`executor.run_claude(brief, cwd, env, argv, out_dir, timeout_s, *, clock)`** gains a required keyword `live: LiveRunSpec` (title, backend, tier, session_id, worktree, resumed_from, auto_resume). It registers right after `record_child`, which already has the child's pid and start, taking the runner's pid and start from `run.json`.

  It unregisters in a `finally` around the block that holds the child, covering `_watch`'s timeout and stall kills and exceptions. `RunOutcome` gains `stopped: bool`, set from `stop_requested` once `_watch` returns (not inside its poll loop).
- **`run.py`.** `brief_title(text)` implements the title rule. `dispatch` and `resume` build the spec; the automatic resume passes the parent's title with ` (auto-resume)`. A stopped run skips the automatic resume. `status.json` gets `stopped: true`, the summary line reads `STATUS stopped | …`, the exit code is 2 (`_exit_code`), and the run-log line's `status` is `stopped`.
- **`mailbox.py`.** `_RUNNER_STATUS_KEYS` lists the new `stopped` key.
- **`SKILL.md`.** A "Live runs" paragraph (folder, file, removal, the stop marker, `STATUS stopped`); the plan-execution fallback treats `stopped` as the user's decision, not a failure to retry. Its advice to steer a live run rather than stop it stands: Stop in RT is the user's call, behind a confirm.
- **Tests.** `tests/test_live.py`: the written schema, atomic replace, unregister removing both files, a warning on an unwritable folder. `test_executor.py`: the file exists while a fake child runs and is gone after exit and after timeout; `stopped` is set when the marker exists. `test_dispatch.py`: a stopped run is not auto-resumed and logs `stopped`.

## Answered questions

- **Q1. Where the run's view opens:** a pane in a daemon tab, so a run can be split and placed beside the terminal that launched it (owner). Carried by the pane follow (section 11) so no fake session id reaches the session paths, shared with RT-30.
- **Q2. What an ended run leaves behind:** the leaf goes at once, and an open view keeps its last state until closed (owner). A run that ends while no view is open leaves no trace in RT; the run log and `report.md` still hold it.
- **Q3. What Stop kills:** `claude`'s tree plus the stop marker, so the tool logs `stopped` and skips the automatic resume. The orchestrator's background command then returns `STATUS stopped`, which the tool's skill tells it to treat as the user's decision.
- **Q4. Stop confirmation:** arm, then confirm, on the view's button and the menu item (owner). A stray click must not kill a tier-2 run mid-edit and leave the worktree half-changed.
- **Q5. Other stream formats:** the optional `agent` field names the plugin that parses the stream; absent means the default agent. Every dispatch backend runs through `claude -p` today.

## Related items

- **RT-30** (view subagents read-only): shares `PaneFollow`. Whichever of RT-30 and RT-31 reaches DF.8 first defines the enum; the other adds a variant.
- **RT-499** (nest dispatch runs on the Dashboard): waits on DF.1 to DF.6, and nests a run under the session its `session_id` names.
- **RT-91** (clear and continue): refuses while background work runs; DF.7 makes a linked live run count.
- **RT-1187** (push trigger reading a removed session's update): `push/trigger.rs` reads the background flag DF.7 extends; no file overlap with any DF step beyond a test.

## Checklist

Proving commands run from the repo root through the gate, written here as `gate … -- <command>` for `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/<step>.log --timeout 1800 --slot heavy -- <command>`. DF.11 calls `rt.ps1` bare, since it gates its own steps. Hotspots: `crates/protocol/src/lib.rs`, `crates/daemon/src/server.rs`, `apps/native/src/lib.rs`.

- [x] **DF.1 Protocol types.** The dispatch-run snapshot, status, three daemon messages and `FEATURE_DISPATCH_RUNS` on the wire. `StopDispatchRun` lands with its handler in DF.6, since the daemon's `dispatch` must handle every client message.
  - Files: `crates/protocol/src/lib.rs`, `apps/pwa/src/protocol.ts` (regenerated with `cargo run -p protocol-ts`).
  - Proof: `gate … -- cargo nextest run -p protocol --cargo-profile test-release dispatch_run` (a round trip per message, an unknown status decodes as `Unknown`, a snapshot missing every defaulted field decodes), then `gate … -- cargo nextest run -p protocol-ts --cargo-profile test-release generated_protocol_ts_is_current`.
  - Waits on: nothing. Hotspot: protocol.
- [x] **DF.2 Stream state.** `HeadlessEvent::SessionId` from the Claude plugin's `init` line, and a pure `StreamState` accumulator the headless session path shares, keeping the recent-actions cap in one place.
  - Files: `crates/agents/api/src/lib.rs`, `crates/agents/claude/src/lib.rs`, `crates/daemon/src/stream_state.rs` (new), `crates/daemon/src/headless.rs`, `crates/daemon/src/session.rs`, `crates/daemon/src/main.rs`.
  - Proof: `gate … -- cargo nextest run -p agent-api -p agent-claude -p daemon --cargo-profile test-release -E 'test(/headless|stream_state/)'`, then `.\rt.ps1 clippy` for the seam check.
  - Waits on: nothing. Hotspot: none.
- [x] **DF.3 Live folder and live-run file parse.** `Agent::live_runs_dir()` (default `None`, implemented by the Claude plugin), `resolve_dispatch_live_dir()` with `RUSTLING_TULIP_DISPATCH_DIR`, and the schema-1 parse (`.tmp` and `.stop` ignored, unknown schema skipped, `agent` defaulted).
  - Files: `crates/agents/api/src/lib.rs`, `crates/agents/claude/src/lib.rs`, `crates/daemon/src/paths.rs`, `crates/daemon/src/dispatch_runs.rs` (new, parse only), `crates/daemon/src/main.rs`.
  - Proof: `gate … -- cargo nextest run -p agent-claude -p daemon --cargo-profile test-release -E 'test(/dispatch_runs::parse|dispatch_dir|live_runs_dir/)'`, then `.\rt.ps1 clippy`.
  - Waits on: DF.2 (same agent files). Hotspot: none.
- [x] **DF.4 Isolation sites.** `RUSTLING_TULIP_DISPATCH_DIR` under `.tmp` for every isolated daemon and driven run (`DISPATCH_DIR_VAR`, checked like `REMOTE_FILES_DIR_VAR`).
  - Files: `crates/daemon-client/src/isolation.rs`, `crates/daemon/src/paths.rs`, `crates/dev-daemon/src/env.rs`, `apps/native/src/driver_gate.rs`.
  - Proof: `gate … -- cargo nextest run -p daemon-client -p dev-daemon --cargo-profile test-release -E 'test(/isolat|dispatch_dir/)'`, then `gate … -- cargo nextest run -p rustling-tulip-native --cargo-profile test-release --features driver -E 'test(/driver_gate/)'`.
  - Waits on: DF.3. Hotspot: none.
- [x] **DF.5 Watcher, tailer, liveness.** Debouncer plus 30 s rescan, the 250 ms tailer (partial line, shrink, missing file), the exact creation-time guard with stale-file deletion, and main-checkout resolution with `repo_path_for_worktree` moved to `git.rs`.
  - Files: `crates/daemon/src/dispatch_runs.rs`, `crates/daemon/src/git.rs`, `crates/daemon/src/worktrees_admin.rs`.
  - Proof: `gate … -- cargo nextest run -p daemon --cargo-profile test-release -E 'test(/dispatch_runs::|repo_path_for_worktree/)'`. The tests use a scratch folder and a live-run file for the test's own child process: appended stream lines show as actions, killing the child removes the run and deletes its file, a dead-pid file is dropped at startup, and a file whose start time does not match is rejected.
  - Waits on: DF.2, DF.3. Hotspot: none.
- [x] **DF.6 Server wiring and Stop.** The hub holds the runs; `DispatchRuns` follows `Welcome`, with `dispatch_runs` in the feature list only when a folder is watched; broadcasts with the lag resend; `ClientMessage::StopDispatchRun { run_id, request_id }` added to the protocol with its handler (guard held, `Stopping`, marker, tree kill, `ActionFailed` with `request_id`).
  - Files: `crates/protocol/src/lib.rs` and the regenerated `apps/pwa/src/protocol.ts` (the client message), `crates/daemon/src/server.rs`, `crates/daemon/src/server/handshake.rs`, `crates/daemon/src/server/connection.rs`, `crates/daemon/src/server/dispatch_runs.rs` (new), `crates/daemon/src/main.rs`.
  - Proof: `gate … -- cargo nextest run -p daemon --cargo-profile test-release -E 'test(/dispatch_run|handshake/)'`, including `stop_dispatch_run_kills_the_tree_and_writes_the_marker` with a parent and a grandchild process.
  - Waits on: DF.1, DF.5. Hotspot: `server.rs`.
- [x] **DF.7 Background flag from a linked run.** `SessionRecord::snapshot()` reports `background` while a live run's `session_id` matches the record's `conversation_id`, through the registry's `DispatchLinks`, and the watcher pushes session updates when a link changes.
  - Files: `crates/daemon/src/session.rs`, `crates/daemon/src/dispatch_runs.rs`, `crates/daemon/src/push/trigger.rs` (a test only).
  - Proof: `gate … -- cargo nextest run -p daemon --cargo-profile test-release -E 'test(/dispatch_link|trigger::/)'` (a linked run sets the flag, its removal clears it, an unmatched run changes nothing, an idle session with a linked run is not Finished).
  - Waits on: DF.5. Hotspot: none.
- [x] **DF.8 Pane follow.** `follow: Option<PaneFollow>` on `GridNode::Pane`, `CreateTab`, `SplitPane` and `ReplacePaneSession`, and the daemon's tab rules: exclusive with a session, never empty, carried by moves and swaps, cleared at startup after the first live-folder scan; plus an old-layout load test.
  - Files: `crates/protocol/src/lib.rs`, `apps/pwa/src/protocol.ts`, `crates/daemon/src/tabs.rs`, `crates/daemon/src/server/tabs.rs`, `crates/daemon/src/main.rs`, plus any `GridNode::Pane` destructuring the compiler names.
  - Proof: `gate … -- cargo nextest run -p protocol -p daemon --cargo-profile test-release -E 'test(/follow/)'`, then the `generated_protocol_ts_is_current` run.
  - Waits on: DF.1, DF.6. Hotspot: protocol.
- [x] **DF.9 Sidebar DS leaves.** Per-host run lists, placement by `main_checkout` else worktree, `LeafKind::Dispatch`, the `DS` tag, dot, hover and ordering, a click or drop that opens a follow pane, and the Open / Stop / Copy run folder menu.
  - Files: `apps/native/src/sidebar.rs`, `apps/native/src/sidebar_view.rs`, `apps/native/src/session_menu.rs`, `apps/native/src/lib.rs`, `apps/native/tests/ui/ui_sidebar.rs`.
  - Proof: `gate … -- cargo nextest run -p rustling-tulip-native --cargo-profile test-release -E 'test(/sidebar::.*dispatch|ui_sidebar::.*dispatch/)'`, covering placement (repo, workspace member, unregistered path, no `main_checkout`), the leaf appearing, updating and going, a click sending `CreateTab` with the follow, and Stop arming then sending. Then `.\rt.ps1 native-shot main`.
  - Waits on: DF.1, DF.8. Hotspot: `lib.rs`.
- [x] **DF.10 Dispatch view and Stop.** A pane with a `DispatchRun` follow draws the header, stats and recent actions; Stop arms, then confirms; a removed run's pane keeps its last snapshot.
  - Files: `apps/native/src/dispatch_view.rs` (new), `apps/native/src/grid_view.rs`, `apps/native/src/lib.rs`, `apps/native/tests/ui/ui_dispatch.rs` (new), `apps/native/tests/ui/main.rs`, `tools/test-map.json`.
  - Proof: `gate … -- cargo nextest run -p rustling-tulip-native --cargo-profile test-release --test ui ui_dispatch`, `.\rt.ps1 test -Validate`, and a native-shot of the view shown before and after.
  - Waits on: DF.8, DF.9. Hotspot: `lib.rs`.
- [x] **DF.11 Live e2e.** A real daemon shows a fake run's leaf with streamed actions, Stop kills the child, and the leaf goes.
  - Files: `apps/native/tests/e2e_dispatch.rs` (new), `crates/agents/claude/fake/index.mjs` (a slow stream-json mode), `apps/native/tests/support/live.rs`, `rt.ps1`, `tools/test-map.json`.
  - Proof: `.\rt.ps1 native-e2e -Test e2e_dispatch -Filter 'test(/dispatch/)'`.
  - Waits on: DF.4, DF.6, DF.10. Hotspot: none.
- [ ] **DF.12 The tool change.** The dispatch tool writes and removes live-run files and honours the stop marker, as "The tool change" says.
  - Files: an inbox request to `~/.claude` naming `dispatch/live.py`, `executor.py`, `run.py`, `mailbox.py`, `SKILL.md` and the tests.
  - Proof: the tool's gate green there, then one real tier-1 run watched and stopped from the native client.
  - Waits on: DF.3 (schema fixed) for the request; DF.10 for the real-run proof. Hotspot: none.
- [x] **DF.13 Docs.** The durable docs describe the feature, and this design goes once nothing open links it (RT-499 links it).
  - Files: `CLAUDE.md` (env row), `docs/on-disk.md` (live-run folder and file), `docs/wire-protocol.md` (messages and feature flag), `docs/README.md` glossary (live-run file, live-run folder, stop marker, pane follow), `docs/architecture.md` (the decisions, the "no headless view" line, Task Index rows), `docs/native-client.md` (the `DS` leaf and view).
  - Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/df13.log --timeout 600 --slot light -- prek run --all-files`.
  - Waits on: DF.1 to DF.11. Hotspot: none.
