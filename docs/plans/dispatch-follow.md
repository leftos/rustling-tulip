# Follow DeepSeek dispatches

Status: design, no step started. Index line: "Follow DeepSeek dispatches" in `docs/plans/MAIN.md` (Wave 9). Terms this doc adds (**dispatch run**, **live-run file**, **live-run folder**, **stop marker**) go into the README glossary in DF.10.

## Problem

The user-level `dispatch` skill (`~/.claude/skills/dispatch`) runs an implementer brief as `claude -p --agent implementer --output-format stream-json --verbose`, launched by `uv run python -m dispatch` in the background. Its stdout goes to `<worktree>/.tmp/dispatch/<stamp>-<backend>/stream.jsonl` (`executor.run_claude`), and nothing else sees it until the run ends and `summary.txt` / `report.md` land. An agentic IDE shows a subagent's progress; it cannot show this one. The user wants to watch a running dispatch in the native client, and to stop one that is going wrong, without the dispatch tool ever needing the daemon.

Settled rulings (from the index line): the tool registers each run with a small live-run file in a folder the daemon watches and removes it when the run ends; a crashed run's entry is dropped once its pid is gone; the tool never depends on the daemon running; the native client shows a live run as a read-only leaf (tag `DS`, the brief's title) in the repo or workspace container its worktree belongs to; a click opens a headless-style view of its stream; the user can watch and Stop it (kill the run's process tree), never message it.

## Design

### 1. Live-run folder

`~/.claude/dispatch/live/`, i.e. `live/` under the dispatch tool's own log dir (`DISPATCH_LOG_DIR`, default `~/.claude/dispatch`, `dispatch/log.py::log_dir`).

The daemon resolves it in order: `RUSTLING_TULIP_DISPATCH_DIR` (new; the e2e tier points it into `.tmp/` so a test daemon can neither show nor stop real runs), else `$DISPATCH_LOG_DIR/live`, else `<home>/.claude/dispatch/live`. It lives in `paths.rs` as `resolve_dispatch_live_dir()` beside `resolve_worktrees_dir`, and the daemon creates the folder before watching it (a `notify` watch on a missing folder fails).

Why the tool's tree and not the RT config dir: the tool writes only under its own directory whether or not rustling-tulip is installed, it never has to know RT's `ProjectDirs` layout or honour `RUSTLING_TULIP_CONFIG_DIR`, and any other watcher (a shell `ls`, a later tool) can read the same folder. The cost is one RT-side env var for isolation.

### 2. Live-run file

One file per `claude` process: `<live>/<run_id>.json`, UTF-8 JSON, LF, written as `<run_id>.json.tmp` then `os.replace`, so the daemon never reads half a file. `run_id` is `<run dir name>-<pid>` (the run dir name alone can collide across two worktrees started in the same second). An automatic or manual resume is a new `claude` process with its own run dir, so it gets its own file and its own leaf.

```json
{
  "schema": 1,
  "run_id": "20260928-141503-deepseek-flash-51234",
  "title": "DF.4 Daemon watcher and tailer",
  "backend": "deepseek-flash",
  "tier": 2,
  "worktree": "D:\\orca\\workspaces\\rustling-tulip\\acornworm",
  "run_dir": "D:\\orca\\workspaces\\rustling-tulip\\acornworm\\.tmp\\dispatch\\20260928-141503-deepseek-flash",
  "pid": 51234,
  "tool_pid": 50120,
  "started_at": "2026-09-28T14:15:03Z",
  "resumed_from": null,
  "auto_resume": false
}
```

- `schema`: 1. The daemon skips (and logs once) a file whose schema it does not know; unknown extra fields are ignored, so the tool may add fields without a bump.
- `title`: the brief's first `# ` heading, else its first non-empty line, trimmed to 120 chars. A resume uses `Resume: ` plus the message's first line; the automatic resume uses the parent's title plus ` (auto-resume)`.
- `worktree`: the `--root` the run works in (resolved top level). `run_dir`: the run's output folder; the stream is always `<run_dir>/stream.jsonl`.
- `pid`: the `claude` child (`Popen.pid`); Stop kills this tree. `tool_pid`: the Python dispatch process, informational (shown on hover, used by nobody to kill).
- `started_at`: UTC, written right after `Popen`. With `pid` it is the pid-reuse guard (section 7).
- `tier`, `resumed_from` (a session id), `auto_resume`, `backend`: shown on the leaf's hover and the view's header.

A **stop marker** `<live>/<run_id>.stop` (empty file) is written by the daemon just before it kills a run (section 6). The daemon's watcher ignores `*.tmp` and `*.stop`.

### 3. Model: a separate message family, not a `SessionSnapshot`

Option A, a new `SessionMode::Dispatch` (or `Headless` plus a `dispatch` field) on `SessionSnapshot`:
- For: the sidebar grouping, leaf rendering, headless view and tabs view already take `SessionSnapshot`; no new client list.
- Against: `SessionMode` has no `#[serde(other)] Unknown`, so a new mode makes a v22 decoder fail on the whole `Sessions` list, breaking the installed Tauri app. Reusing `Headless` instead lets Tauri show the run as a normal headless session it could try to stop, discard, recover or put in a tab. And every session path would need a "not a dispatch" guard: `history/` entries and the recover dialog, `meta.json` sidecars, Stop/Discard/Park, attention and notifications, `ListWorktrees` in-use marking, idle-exit blockers, tab placement. The daemon does not own the process, so most of those actions mean nothing for it.

Option B, **chosen**: a dispatch-run family with its own snapshot. It costs a second list in the native client's sidebar model and a small view-model seam in the headless body (DF.6, DF.7), and touches no session path. Nothing is persisted by the daemon: the live-run folder and the stream files are the state.

### 4. Protocol (additive, no version bump)

New types in `crates/protocol/src/lib.rs`:

- `DispatchRunSnapshot { run_id, title, backend, tier: Option<u8>, worktree, main_checkout: Option<String>, run_dir, started_at, status: DispatchRunStatus, metrics: SessionMetrics, recent_actions: Vec<String>, claude_session_id: Option<String>, resumed_from: Option<String>, auto_resume: bool }`. Every field after `status` is `#[serde(default)]`. `main_checkout` is the worktree's main checkout path, resolved by the daemon (section 5).
- `DispatchRunStatus { Starting, Working, Stopping, Ended, #[serde(other)] Unknown }`: `Starting` until the stream's `system`/`init` line, `Working` after it, `Stopping` between a Stop and the file's removal, `Ended` after the `result` line (the file usually goes moments later).

`DaemonMessage` variants:
- `DispatchRuns { runs: Vec<DispatchRunSnapshot> }`: the full list, sent after `Welcome` beside `Sessions`, and again to a connection whose dispatch stream lagged.
- `DispatchRunUpdated { run: DispatchRunSnapshot }`: a run appeared or changed, throttled to one per run per 500 ms.
- `DispatchRunRemoved { run_id }`: the file went, or the pid died.

`ClientMessage` variant:
- `StopDispatchRun { run_id, request_id: Option<String> }`. Success shows as `DispatchRunUpdated` (`Stopping`) then `DispatchRunRemoved`; failure is `ActionFailed` with the `request_id` echoed to the requester only, as spawns do. A second Stop for the same run is a no-op.

The daemon sends the dispatch family only to connections whose negotiated version is 23 or later, so the Tauri app (pinned to 22) never sees it. The `v22_compat` tests stay green, and DF.1 adds a round-trip test per new message and one proving an unknown `DispatchRunStatus` decodes as `Unknown`.

### 5. Daemon: watcher, tailer, liveness (`crates/daemon/src/dispatch_runs.rs`, new)

- **Watcher.** One `notify_debouncer_full` debouncer on the live-run folder, `RecursiveMode::NonRecursive`, 100 ms (as `git_watch.rs`'s init watcher). A change to a `.json` file triggers a re-read of that file; the removal of a `.json` file removes the run. It also runs a full folder scan at startup and every 30 s, to cover missed events.
- **Container resolution.** On first sight, the daemon resolves `main_checkout`: if `<worktree>/.git` is a directory, it is the worktree itself; if it is a gitfile, it uses `repo_path_for_worktree` (moved from `worktrees_admin.rs` into `git.rs` as `pub(crate)`). A missing or unreadable worktree leaves it `None`.
- **Tailer.** One task per run, polling `<run_dir>/stream.jsonl` every 250 ms from its last byte offset. It keeps a trailing partial line until its newline arrives, and resets to 0 if the file shrinks. The first read replays the whole file, which rebuilds a run's state after a daemon restart or late discovery (a stream is a few MB at most). Lines go through the Claude stream-json state machine that `agents/claude.rs::handle_headless_line` uses today, extracted in DF.2 as a pure `StreamState::apply(&mut self, line)` holding status, metrics, the 200-entry `recent_actions` and the `init` session id. Headless sessions and dispatch runs then share one parser.
- **Liveness.** Every 2 s the daemon refreshes the tracked pids through `sysinfo` (as `orphan::pid_matches` does). A pid counts as the run's while it exists and its process start time falls between 60 s before and 5 s after `started_at`. A run whose pid is gone or fails the guard is removed and its file deleted: the tool crashed or was killed before its `finally`. The tool never deletes other runs' files, since stdlib Python on Windows has no safe liveness check (`os.kill(pid, 0)` calls `TerminateProcess`).
- **Idle exit.** Live runs do not keep the daemon up (`idle_exit.rs` is unchanged). A daemon that exits loses nothing, because the next one rescans.

### 6. Stop

`StopDispatchRun` → the daemon re-checks the pid guard (failure → `ActionFailed "The run already ended"`), marks the run `Stopping`, writes `<run_id>.stop`, then runs `taskkill /T /F /PID <pid>` with `CREATE_NO_WINDOW` (the same tree kill the tool uses on timeout, `gitops.kill_tree`). Killing `claude`'s tree, not the tool's, lets the Python process see the child exit, see the marker, record the run as stopped (section 8) and remove its own live file. A non-Windows build kills the single pid, as the tool does. A failed `taskkill` → `ActionFailed` naming the exit status, and the run goes back to its previous status.

### 7. Restart, crash and many clients

- **Daemon restart mid-run.** The dispatch run is not the daemon's child, so it keeps going. The new daemon's startup scan finds the file, the pid guard passes, and the tailer replays the stream from byte 0, so clients get the full log. A Stop issued just before the restart has already killed the tree or has not happened.
- **Tool crash** (Python killed, `claude` orphaned or dead): while `claude` lives, the run stays listed and Stop still works through `pid`. Once `claude` is gone, the next liveness tick drops the run and deletes the file. **Machine reboot**: every pid is gone, and the startup scan drops and deletes every stale file.
- **Pid reuse**: the start-time window rejects a recycled pid, so the daemon never lists, or kills, an unrelated process.
- **Many clients**: every protocol-23+ connection gets the same broadcasts. A Stop from one shows `Stopping` and then removal on all of them. LAN clients see runs like local ones; Stop from a LAN client is allowed like `StopSession`.

### 8. Native client

- **Model.** `SidebarModel` keeps `dispatch_runs: Vec<DispatchRunSnapshot>`, applying the three daemon messages in `on_daemon_message` (`net.rs`). `build_containers` places each run with `find_container_for_cwd(main_checkout.or(worktree))`: a registered repo's container, or the workspace it belongs to, else a `Dir` container for that path. `Leaf` gains `kind: LeafKind { Session, Dispatch }`. A dispatch leaf's `runtime` is `DS`, its label the run's `title`, and its status dot maps `Starting→Spawning`, `Working→Working`, `Stopping|Ended→Stopped`. Dispatch leaves sort after the container's sessions, newest first, and are not reorderable. In the tabs view they sit in Unbound unless a viewer shows them. Hover: backend, tier, started time, worktree.
- **View.** A click opens (or focuses) a client-local viewer tab for the run (Q1). Its body is the existing headless body: `HeadlessBody::of` takes a `HeadlessSource` (status label, metrics, recent actions) built from either a `SessionSnapshot` or a `DispatchRunSnapshot`, so the stats bar, the 200-row tail and "Show all" are shared. Its header row shows the title, `DS · <backend> · tier <n>` and a **Stop** button (arm-then-confirm, like the session menu's destructive items). There is no input box, and the leaf's context menu offers only Open, Stop and "Copy run folder".
- **When the run goes.** The leaf goes on `DispatchRunRemoved`. An open viewer keeps the run's last snapshot, shows `ended` and the run folder path, and closes only when the user closes it (Q2).

## Change to the `dispatch` skill (user-level, outside this repo)

Describe-only here; the change is made in `~/.claude/skills/dispatch` under its own gate (`uv run ruff check . && uv run ruff format --check . && uv run ty check && uv run pytest -q`).

- **New `dispatch/live.py`.** `live_dir()` returns `log_dir() / "live"`. It holds a `LiveRun` dataclass with the schema fields above, plus `register(live) -> Path` (tmp then `os.replace`, LF), `unregister(path)` (which also removes a leftover `<run_id>.stop`) and `stop_requested(run_id) -> bool`. A write failure warns on stderr and never fails the run, the same policy as `log.record_run`.
- **`executor.run_claude`** takes a required `live: LiveRunSpec` (title, backend, tier, resumed_from, auto_resume, worktree, run_dir). After `Popen` it registers with `process.pid`, `os.getpid()` and `started_at`, and unregisters in a `finally` that also covers the timeout path and exceptions. `RunOutcome` gains `stopped: bool`, true when `stop_requested` holds once `wait` returns.
- **`run.py`.** `brief_title(text)` implements the title rule. `dispatch` and `resume` build the spec, and the auto-resume passes the parent's title with ` (auto-resume)`. A stopped run skips the automatic resume. `status.json` gets `stopped: true`, the summary line reads `STATUS stopped | …`, the exit code is 2, and the run-log line's `status` is `stopped`.
- **`SKILL.md`.** It gains a "Live runs" paragraph (folder, file, removal, what the stop marker means, `STATUS stopped` in the summary line); the `plan-execution` fallback treats `stopped` as the user's decision, not a failure to retry.
- **Tests.** `tests/test_live.py` covers the written schema, atomic replace, unregister removing both files and a warning on an unwritable folder. `test_executor.py` checks that the file exists while a fake child runs and is gone after exit and after timeout, and that `stopped` is set when the marker exists. `test_dispatch.py` checks that a stopped run is not auto-resumed and logs `stopped`.

## Open questions

Answered (user): Q1 (2) a pane in a daemon tab, so a run can be split and placed beside the terminal that launched it; the design must carry that without a fake session id reaching the v22 Tauri app or the session paths (follow the `subagent_id` on `GridNode::Pane` pattern from [subagent-streams.md](./subagent-streams.md), for example a `dispatch_run_id` on the pane, and share it with that item); Q4 (1) Stop arms, then confirms; Q2 the leaf goes when the run ends, and an open viewer keeps its last state until closed. Settled (orchestrator, technical): Q3 (1) Stop kills `claude`'s tree and writes the stop marker, so the tool logs `stopped` and skips auto-resume; Q5 (1) Claude stream-json only, `schema` bumps if that changes.

- **Q1. Where the run's view opens.** (1, recommended) A client-local viewer tab that never reaches the daemon: no Tauri or `TabContent` impact, not persisted (runs are ephemeral). Worst case: the native tab strip gains a second tab source to keep in step. (2) A pane in a daemon tab whose `session_id` is `dispatch:<run_id>`, getting splits and moves for free. Worst case: `TabContent`/pane handlers and the v22 Tauri app see a session id that is not in `Sessions`, and every pane path needs a guard. (3) An overlay drawer over the content area. Worst case: a run can't be watched beside the terminal that launched it.
- **Q2. What an ended run leaves behind.** (1, recommended) The leaf goes at once, and an open viewer keeps the last state until closed. Worst case: a run that ends while its viewer is closed leaves no trace in RT (the run log and `report.md` still hold it). (2) The daemon keeps ended runs as `Ended` leaves until dismissed or the daemon restarts. Worst case: the folder is no longer the whole state, and the leaf list grows across a long session. (3) The leaf and the viewer both go at once. Worst case: the final report line the user was reading vanishes.
- **Q3. What Stop kills.** (1, recommended) `claude`'s tree plus the stop marker: the tool logs `stopped` and skips auto-resume. Worst case: the orchestrator's background command returns and it reads `STATUS stopped`, which it has to handle. (2) The tool's tree (`tool_pid`): no marker needed. Worst case: no `status.json` or run-log line, and the orchestrator sees a killed command with no summary. (3) `claude`'s tree without a marker. Worst case: the tool auto-resumes the run the user just stopped.
- **Q4. Stop confirmation.** (1, recommended) Arm-then-confirm on the viewer's button and the menu item. Worst case: one extra click. (2) Immediate. Worst case: a stray click kills a tier-2 run mid-edit, leaving the worktree half-changed.
- **Q5. Codex or other backends.** (1, recommended) Parse only Claude stream-json now, since every dispatch backend runs through `claude -p`, and use `schema` to add a `format` field if that changes. Worst case: a later non-Claude executor shows "No events yet…" until the field lands. (2) Add `format: "claude-stream-json"` to schema 1 now. Worst case: a field nothing reads.

## Checklist

- [ ] **DF.1 Protocol types.** `crates/protocol/src/lib.rs`: `DispatchRunSnapshot`, `DispatchRunStatus`, the three `DaemonMessage` variants and `StopDispatchRun`. Proof: `cargo test -p protocol dispatch_run` (round-trip per message, unknown status → `Unknown`, a snapshot missing every defaulted field decodes) and `cargo test -p protocol v22_compat` green.
- [ ] **DF.2 Shared stream-json state machine.** `crates/daemon/src/agents/claude.rs`: extract `StreamState::apply`; `handle_headless_line` calls it through `registry.update`. Proof: new unit tests feed `init`/`assistant`/`result` lines and assert actions, metrics, status and session id; the existing `headless.rs` and `session.rs` tests pass.
- [ ] **DF.3 Live-run file and folder.** `crates/daemon/src/paths.rs` (`resolve_dispatch_live_dir`, `RUSTLING_TULIP_DISPATCH_DIR`), `crates/daemon/src/dispatch_runs.rs` (schema-1 parse, `.tmp`/`.stop` ignored, unknown schema skipped), `repo_path_for_worktree` moved to `git.rs`. Proof: `cargo test -p daemon dispatch_runs::parse` on the example above, the env override, a gitfile worktree resolving its main checkout.
- [ ] **DF.4 Watcher, tailer, liveness.** `crates/daemon/src/dispatch_runs.rs`: debouncer, 30 s rescan, 250 ms tailer with partial-line and shrink handling, 2 s pid guard with stale-file deletion. Proof: a tokio test in a scratch folder writes a live-run file for its own child (`ping -n 20 127.0.0.1`), appends stream lines and sees the actions, then kills the child and sees the run removed and the file deleted; a file naming a dead pid at startup is dropped.
- [ ] **DF.5 Server wiring and Stop.** `crates/daemon/src/server.rs`, `main.rs`: send `DispatchRuns` after `Welcome` to 23+ connections, broadcast updates/removals, `StopDispatchRun` (guard, marker, `taskkill /T /F`, `ActionFailed` with `request_id`). `rt.ps1` sets `RUSTLING_TULIP_DISPATCH_DIR` for `native-e2e`. Proof: a server test where a v22 connection receives no dispatch message and a v23 one does; `stop_dispatch_run_kills_the_tree_and_writes_the_marker` with a `cmd /c ping` parent and grandchild.
- [ ] **DF.6 Native sidebar leaves.** `apps/native/src/sidebar.rs`, `sidebar_view.rs`, `net.rs`: the run list, placement by `main_checkout`, `LeafKind::Dispatch`, `DS` tag, hover, menu. Proof: `sidebar.rs` unit tests for placement (repo, workspace member, unregistered path) and `cargo test -p rustling-tulip-native --test ui_sidebar dispatch` (leaf appears, updates, goes on removal).
- [ ] **DF.7 Native dispatch viewer and Stop.** `apps/native/src/headless.rs`, `headless_view.rs` (`HeadlessSource`), the viewer tab per Q1, Stop per Q3/Q4. Proof: new `apps/native/tests/ui_dispatch.rs` (click opens the viewer with the stats and log, Stop arms then sends `StopDispatchRun`, removal keeps the viewer on its last state per Q2) and the existing headless specs.
- [ ] **DF.8 Live e2e.** `apps/native/tests/e2e_dispatch.rs`, `tools/e2e/fake-claude/index.mjs` (a mode that streams stream-json lines slowly): the test registers a live-run file for a fake-claude child in the isolated folder. Proof: `.\rt.ps1 native-e2e` shows the leaf with streamed actions, Stop kills the child, and the leaf goes.
- [ ] **DF.9 The `dispatch` skill change** (user-level, outside this repo, landed separately): `live.py`, `executor.py`, `run.py`, `SKILL.md`, tests as in "Change to the `dispatch` skill". Proof: the skill's gate green, then one real `deepseek-flash` tier-1 run watched and stopped from the native client.
- [ ] **DF.10 Docs.** `CLAUDE.md` (env var row for `RUSTLING_TULIP_DISPATCH_DIR`, the live-run folder under "Where things live on disk", the dispatch messages under the wire-protocol notes), README glossary (dispatch run, live-run file, live-run folder, stop marker), delete the index line in `docs/plans/MAIN.md`, promote the design's lasting decisions into `docs/architecture.md`, and delete this doc. Proof: `prek run` clean.
