# herdr: engineering learnings for the daemon

Researched 2026-10-27 against the herdr checkout at `X:\dev\_ext\herdrdev\herdr` (v0.9.5, 13 commits behind `upstream/master`; the local patch adds two Claude detection rules and nothing else). Where [herdr-comparison.md](./herdr-comparison.md) asks whether rustling-tulip could be built from herdr plugins, this note asks which of herdr's implementation techniques our daemon should take.

Four read-only passes each covered one subsystem and checked every "we lack X" claim against our source. Line numbers are from the files as read on that date. Each section ranks its items by value against cost: value high, med or low, cost S, M or L.

Defects found here and the ideas the owner picked are Linear issues under RT parent "Daemon robustness and ideas from the herdr read". The agent-CLI points are folded into [agent-cli.md](../plans/agent-cli.md) ("From herdr").

Overall: herdr's server is no model for ours. Its pane processes die with a server restart on Windows, its event history is not durable, and its local auth is a socket ACL, while our tracer, lag resync and per-device credentials are ahead. What it does better is robustness detail:

- writes flushed to disk, and unloadable state kept rather than overwritten (A.1, A.2, D.2)
- a size-capped log (D.1)
- write timeouts, bounded queues and a liveness ping toward a stalled client (C.3)
- host-shutdown and job-object handling on Windows (A.3, A.4)
- error codes, status sequence numbers and an atomic prompt-and-wait for agent callers (C.1, C.2, C.5)
- rule tooling for screen detection (B.1, B.2, B.5)

## A. Process lifecycle, persistence and restore

Herdr tree: X:/dev/_ext/herdrdev/herdr (paths relative to it). Ours: X:/dev/rustling-tulip (paths relative to it).
Ranked by value/cost, best first. Every "we lack" was checked against our source in this run.

---------------------------------------------------------------------------

### 1. Durable atomic writes: fsync + write-through replace + parent-dir sync (value HIGH-MED, cost S)

**herdr:** `src/persist/io.rs:49-67` `save_json_to_path`: write tmp, `sync_all()` ("A rename can survive a hard crash before unflushed file contents reach disk", :57), replace via `platform::replace_file` (Windows: `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`, `src/platform/windows.rs:352-383`), then `sync_parent_directory`.

On any failure the tmp is deleted and the old file is untouched. Test `failed_replacement_preserves_saved_files_and_retries` (`io.rs:236-280`) locks the destination on Windows and asserts old bytes survive and a retry succeeds. Writes follow symlinks first (`resolve_write_target`, :22-43) so dotfile managers keep working (not relevant to us).

**us today:** no `sync_all`/write-through anywhere in `crates/` (Grep). `orphan::write_atomic` (`crates/daemon/src/orphan.rs:214-227`), `state::persist` (`crates/daemon/src/state.rs:419-425`), `write_handshake` (`server.rs:484-507`) all do `secret::write_private` (`secret.rs:76-95`: open, write_all, drop, no flush to disk) then `std::fs::rename`.

Our own comment at `orphan.rs:283-285` says torn sidecars from "power loss or standby mid-rewrite" happen in practice, and we quarantine them after the fact. NTFS can persist the rename before the data, leaving a zero-length/torn file.

**change:** add `write_atomic_durable` used by `state.json`, `remote.json`, `devices.json`, history entries: `File::sync_all()` before rename, `MoveFileExW(.., MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)` on Windows (windows crate already a dep in daemon), skip dir fsync on Windows (no equivalent; write-through covers it) and do it on unix.

Add a bounded retry (3 x ~25 ms) on `ERROR_SHARING_VIOLATION`/`ACCESS_DENIED` for the replace (AV/indexer holding the target). Keep sidecar `meta.json` rewrites on the cheap path OR coalesce them (see item 11) since `sync_sidecar` fires on every status change; fsync per status flap would be costly. State writes are rare, so fsync there is free.

---------------------------------------------------------------------------

### 2. Never overwrite an unloadable `state.json`: preserve it, keep rolling backups (value MED, cost S)

**herdr:** `SessionWriter` (`src/persist/writer.rs:9-76`) carries `protect_unloaded`.

If the file existed but did not load (corrupt, or newer version: `io.rs:94-133` returns None and warns, "from a newer herdr version, ignoring"), the first save/clear copies the exact bytes to `session-backups/` (keep 3, `writer.rs:123,126-186`), publishes via `.pending` + fsync + rename so a half copy never looks complete (`copy_recovery`, :188-219), and refuses to mutate the live file if the preserve fails (test :471-493).

Separately, `preserve_snapshot_history` (:81-120) keeps up to 48 timestamped copies of the last good layout, at most one per 15 min, skipping a copy whose layout fingerprint equals the previous one (:111-116), names carry a 39-digit nanosecond stamp + pid and are made monotonic against clock rollback (:139-151). User-named files in those dirs are never pruned (:523-537).

**us today:** `state.rs:124-131`: unparseable `state.json` -> `warn!("state.json corrupt, starting fresh")`, `PersistedState::default()`, flag `state_file_corrupt` (:119) only gates one startup sweep. The next `mutate` -> `persist` (:419) overwrites the corrupt file, so repos/workspaces/tabs/layouts/`last_spawn_config` are gone with no copy.

(Sidecars are better: `orphan.rs:354-365` quarantines to `meta.json.corrupt`.) Note CLAUDE.md "No backward compatibility before the first release" excuses schema breaks, not torn writes or a downgrade (valid JSON from a newer build gets overwritten).

**change:** in `AppState::load_or_default`, when parse fails: rename to `state.json.corrupt-<unix ms>` (keep last 3) before returning default, mirroring `quarantine_meta`. When the file parses as JSON but fails the schema or has a higher version stamp, copy instead of rename and set a "do not write until preserved" flag.

Optional second step: one rolling `state.json.bak` refreshed when the registry content changes and is >15 min older. No herdr-sized machinery needed (our state is small); cap at 3 + 1.

---------------------------------------------------------------------------

### 3. Host shutdown/logoff must not turn live sessions into "exited" (value MED, cost S)

**herdr:** `platform/windows.rs:251-266`: exit code `0xC000013A` (STATUS_CONTROL_C_EXIT) classified `ChildExitReason::Interrupted`; `host_shutdown_in_progress()` polls `GetSystemMetrics(SM_SHUTTINGDOWN)`.

The server loop checks it every tick and, when set, saves the session *before* applying the shutdown-time process exits (`src/server/headless.rs:445-450, 705-719`); Interrupted exits force a checkpoint and a pane-exit checkpoint is not overwritten by the later empty state (`src/app/session.rs:105-129`, tests `app/mod.rs:3265-3374`). Stop reason is recorded first-wins with caller attribution (`src/server/shutdown.rs:31-57`).

**us today:** the offline path is right: `history.rs:51-53, 178-249` treats a tracer-log exit with `STATUS_CONTROL_C_EXIT` as lost (`TracerLost`, eligible for auto-resume). The live path is not: `history::end_for_exit` (`history.rs:75-80`) maps `PtyExit::Code(code)` straight to `SessionEnd::Exited{code}`, and `session.rs:1060-1110 handle_exit` then marks the session plain Stopped, fires `AttentionReason::Stopped`, and `delete_sidecar`s it.

So when logoff/shutdown kills the shell while the daemon (and its tracer pipe) is still alive long enough to relay `Exited(0xC000013A)`, the session is recorded as an ordinary exit and `auto_resume` never offers it (it only looks at abandoned/lost sessions). No `SM_SHUTTINGDOWN` use anywhere (Grep).

**change:** in `pty::PtyExit` classification (or `handle_exit`), treat `Code(STATUS_CONTROL_C_EXIT)` on Windows, and any exit seen while `SM_SHUTTINGDOWN != 0`, as `TracerLost`-equivalent: set `is_abandoned`, keep the sidecar, no Stopped attention. Share the constant with `history.rs`.

Add a test next to `handle_exit` tests. (A child that genuinely died of Ctrl-C status while the OS is NOT shutting down stays an exit only if you gate on SM_SHUTTINGDOWN; the code-only variant is what the offline importer already does, so consistency argues for matching it.)

---------------------------------------------------------------------------

### 4. Escape a kill-on-close job when the daemon is launched (value MED, cost M)

**herdr:** `platform/windows.rs:1454-1462, 1595-1639`: before launching the server it asks `IsProcessInJob` + `QueryInformationJobObject` whether the current job has `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`; if so it launches via WMI `Win32_Process.Create` (`launch_server_daemon_with_wmi`, :1462-1527; passes explicit env block, `DETACHED_PROCESS`), which is created outside the caller's job.

`current_process_is_detached_server_daemon` (:1632) additionally requires no console and a non-killing job, and has a child-process test (:3975-4030). Reason: a launcher in a kill-on-close job (OpenSSH-on-Windows sessions, some IDE/CI/task runners) would take the "background" server down with it.

**us today:** `daemon-client/src/supervisor.rs:844-860 spawn_daemon` uses `CREATE_NO_WINDOW | DETACHED_PROCESS` only; `daemon/src/detach.rs:87-89` the same; tracers via `spawn_detached.rs:98-110` use `CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT`.

No `CREATE_BREAKAWAY_FROM_JOB`, no job detection (Grep `BREAKAWAY|IsProcessInJob`: none). If the client is started from a kill-on-close job, closing it kills the daemon AND every tracer (they inherit the job), defeating "sessions survive". The tracer's own job (`tracer/src/job_object.rs`) is for the PTY child tree and is fine.

**change:** helper `launcher_job_kills_on_close()` (port of herdr :1595-1624, our `windows` crate already used in `conversations/job.rs`). If true: try `CREATE_BREAKAWAY_FROM_JOB` (works only when the job has BREAKAWAY_OK; CreateProcess fails with ACCESS_DENIED otherwise), then fall back to WMI like herdr (`wmi` crate; or a `schtasks`/COM route).

Apply to `spawn_daemon`, `detach.rs`, and the tracer spawn in `spawn_detached.rs` (tracers inherit the daemon's job otherwise). Log which path ran. Worth a field-check first: how often is our real launch path inside such a job (installer-launched, Start-menu, autostart Run key are not; `ssh`-launched are).

---------------------------------------------------------------------------

### 5. Scrollback persistence: keep the file handle, move IO off the tokio worker, trim on a line boundary (value MED, cost S-M)

**herdr:** per-pane IO runs on dedicated threads, never the async runtime: reader/writer/control threads on Windows (`src/pty/actor.rs:229-301`), one poll loop on unix; the session save runs on its own named thread, coalesced (`src/app/session.rs:62-88`: if one is running, re-arm 250 ms instead of stacking) with a 5 s debounce (`app/mod.rs:44`).

Truncation of a text history respects UTF-8 and cuts to the next newline so replay never starts mid-line (`src/pane.rs:1811-1824 truncate_handoff_history`; handoff replay cap `MAX_REPLAY_BYTES_PER_PANE = 8 KiB`, `server/handoff.rs:31`). Pane history persistence is opt-in (`config/model.rs:1082`) because it can hold secrets, and is a separate file from the layout so it can be dropped alone (`io.rs:69-77`).

**us today:** `session.rs:1033-1049 deliver_chunk` runs on the lifecycle tokio task and calls `scrollback::append` (`scrollback.rs:42-68`) for every PTY chunk: `create_dir_all` + `OpenOptions::open` + `write_all` + close + `metadata()` = 4+ syscalls per <=4 KiB tracer read (`tracer/supervisor.rs:232`), each close a Defender scan opportunity on Windows.

Every ~512 KiB `trim` (`scrollback.rs:70-84`) does a blocking `fs::read` of 2 MB + `fs::write` of 1.5 MB + rename on that same worker. The trim offset is a raw byte offset (`:72`), so replay can start in the middle of an escape sequence or a multi-byte char (we do re-assert bracketed paste, `termstate`, but not other sticky modes). The file is also created with default ACLs, not `write_private`.

**change:** (a) per-session writer owning an open `File` + tracked length (no stat per chunk), buffered and flushed on a short timer and before any snapshot request is served (the snapshot arm at `session.rs:988-1008` is in the same task, so "flush then read" keeps the exactness guarantee documented at :996-1004).

(b) do trim via `spawn_blocking` or a per-session writer thread. (c) after choosing `keep_from`, advance to the next `\n` (bounded, e.g. 4 KiB search, fall back to raw offset) so the replay head is a line start; optionally also skip a leading partial CSI/OSC (`ESC` without terminator).

(d) create with `secret::write_private`-style owner-only mode on unix. Keep the 2 MB/1.5 MB numbers.

---------------------------------------------------------------------------

### 6. PID-reuse guard that does not rely on the process name (value LOW-MED, cost S)

**herdr:** identity is `(pid, creation_time)`: `platform/windows.rs:1891-1913 process_start_token` / `live_pane_process_group` compare the creation time before trusting a pid, and `process_belongs_to_pane` additionally checks a per-pane env marker (`HERDR_PANE_RUNTIME_ID`, :614, :1172) so a recycled pid in another pane is rejected.

**us today:** `orphan.rs:424-461 is_session_alive` is `name.contains("rt-tracer")` for the recorded `tracer_pid` (doc comment :415-418 calls it the PID-reuse guard). Any other `rt-tracer` process (another session's tracer, the e2e daemon's tracers, a stale copy from the binary cache) that inherits a recycled pid passes it.

`kill_pid` (:472-488) then kills `meta.pid` (the PTY child pid, not the tracer) after only that check, so a stale sidecar whose tracer is alive but whose child pid was recycled would signal an unrelated process. Reattach would still fail the pipe handshake (pipe name carries the session id), which limits the damage to the kill path and to "alive" misclassification.

**change:** store `tracer_started_at` (sysinfo `Process::start_time()`, epoch seconds) next to `tracer_pid` in the sidecar at spawn (additive `#[serde(default)]`), and require equality (+/- 2 s) in `pid_matches`; in `kill_pid` kill the tracer (or verify `meta.pid`'s start time too) instead of just `meta.pid`. Cheap; sysinfo is already used.

---------------------------------------------------------------------------

### 7. Coalesce queued resizes (value LOW, cost S)

**herdr:** unix keeps one pending resize slot that later requests overwrite (`src/pty/actor/unix.rs:59-64, 202-226`), so a drag-resize storm costs one ioctl. On Windows resize is on its own control thread so a slow `ResizePseudoConsole` cannot block reads/writes (`src/pty/actor.rs:275-301`).

**us today:** the thread split is already there (`tracer/supervisor.rs:323-343`, `spawn_blocking` per resize), but the channel is unbounded and every queued `(cols, rows)` is applied in turn, each causing a ConPTY reflow and redraw. The daemon dedups same-size claims (CLAUDE.md "Wire-protocol gotchas"), not intermediate sizes.

**change:** in the resize task, after `recv()` drain `try_recv()` and apply only the last size (3 lines).

---------------------------------------------------------------------------

### 8. Bounded input queue with explicit Full/Closed (value LOW, cost S)

**herdr:** user input goes through `mpsc::channel(1024)` with `try_send`, returning `Full`/`Closed` to the caller (`src/pty/actor.rs:99-115, 220`); a "submit text then Enter" request is one unit that finishes the Enter even if the caller times out, so a timeout cannot leave a half-typed prompt (`actor.rs:375-380`), and `shutdown()` gives a queued Enter 250 ms before teardown (:191-198).

**us today:** `tracer_client.rs:639-641` input/resize/stop channels are unbounded; a wedged child plus a paste flood grows daemon memory without limit (the tracer writer paces and blocks, `tracer/supervisor.rs:286-318`, but nothing pushes back to the sender). `inject.rs` types text and Enter as separate steps with its own timeouts.

**change:** bound `input_tx` (e.g. 1024 frames) and surface a "terminal busy" error on Full; keep text+Enter atomic in `inject::run_chain`. Low priority; only matters under abuse.

---------------------------------------------------------------------------

### 9. Validate persisted ids before typing them into a shell (value LOW, cost S)

**herdr:** `src/agent_resume.rs:5-8, 58-89, 352-361` caps id length (512), path length, argv count (64) and bytes (8 KiB), rejects control characters and apostrophes, and requires a bare command name because restore *types* the command into the pane shell. Dedupe key per (source, agent, ref) so two panes never resume one conversation (`:321`, `persist/restore.rs:836-851`); we have this via `hub.recovering.claim` / `RecoveryClaims`.

**us today:** `server/spawn/shell.rs:309-390` quotes per shell family well (ahead of herdr on quoting, see "Ahead"), but does not reject control characters other than CR/LF in a conversation id or folder before they are typed; a `\x03` (Ctrl-C) or `\x04` in a hand-edited `meta.json`/`history/*.json` would act on the PTY, not as text.

**change:** one `valid_typed_value()` (no `char::is_control`, length cap) applied in `quote_args`/`launch_line`, returning `None` like the cmd refusals already do.

---------------------------------------------------------------------------

### 10. Stagger and pre-validate restores (value LOW, cost S)

**herdr:** restored agent panes are not spawned during restore; they carry a `pending_agent_resume_plan` and start later at the pane's real size, one per `startup_per_agent_delay_ms` (`app/agent_resume.rs:48-97`, config `startup_per_agent_delay_ms`), after a short wait for the host theme (:25-41, `PENDING_AGENT_RESUME_THEME_WAIT` 750 ms).

A missing `cwd` is checked before spawn and becomes a visible `restore_error` with the plan cleared, never a retry loop (`:257-264`); a failed spawn keeps the pane as a placeholder with its session ref (`persist/restore.rs:443-472, 733-760`), and `pane_restore_startup` suppresses saved screen history when a native resume will repopulate it (`restore.rs:813-863`).

**us today:** `recovery.rs:561-597 auto_resume_with` is sequential with rich gating (transcript exists, worktree pinned, branch matches, not in use, `:630-658`), which is better than herdr's. Missing: any spacing between spawns (N claude starts back to back), and the resumed PTY starts at the default size until a client claims a resize.

**change:** optional `sleep(250ms)` between `auto_resume_one` calls; nothing else. Skip the size point (our client resizes with `claim`).

---------------------------------------------------------------------------

### 11. Coalesce sidecar rewrites (value LOW, cost M) - only if item 1's fsync is applied to sidecars

`sync_sidecar` (`session.rs:653-658`) rewrites `meta.json` synchronously on every status change under the sidecar gate. Herdr marks dirty and debounces 5 s (`app/mod.rs:44`, `app/session.rs:14-26`) because its save is heavy. Not needed unless fsync is added to the sidecar path; if it is, add a per-session "dirty" bit + 1 s trailing flush (the gate logic in `session.rs:472-478, 686+` already serialises writers) and force-flush on spawn, exit and daemon shutdown.

---------------------------------------------------------------------------

### 12. Smaller, mostly diagnostic (LOW, S each)

- **Stop attribution.** herdr records the first stop reason with the caller (pid + name + parent) and logs it once (`server/shutdown.rs:31-57`, `session.rs` socket peer lookup). Our `shutdown_handler` (`server.rs:524-539`) logs only "HTTP /shutdown requested". Let `daemon_client::shutdown_in` send `X-Caller: pid/exe` and log it; helps the "who killed my daemon" investigations.
- **Thread spawn that reports failure.** `src/thread_spawn.rs:12-47`: `spawn_named` returns `io::Result` (OS thread-limit EAGAIN would otherwise panic and kill a long-lived loop) and `spawn_named_with` hands the payload back on failure, with a test hook to inject failures. `tracer/supervisor.rs:230, 286` use bare `std::thread::spawn` (panic = tracer dies = session lost). Switch to `Builder::spawn`, log and exit cleanly.
- **Socket cleanup only if owned.** `remove_socket_file_if_owned` + identity check on shutdown (`server/headless/lifecycle.rs:368-382`); irrelevant to our named pipes, noted for the unix build only.
- **Suspended-create + job assign for helper commands.** `StatusCommandGuard` (`platform/windows.rs:1249-1383`) creates helper shells `CREATE_SUSPENDED`, assigns them to a kill-on-close job, then resumes by enumerating threads (no race where the child escapes before assignment).

  Our tracer assigns the PTY child after spawn (`tracer/src/job_object.rs:76-96`, logs "grandchildren may survive kill" on failure, `supervisor.rs:188-199`). If portable-pty cannot spawn suspended, ignore; the window is milliseconds.

---------------------------------------------------------------------------

### Not worth copying (herdr does it, we should not)

- **Process-tree kill by snapshot + HUP/TERM/KILL ladder** (`src/pane.rs:1760-1808`, `platform/windows.rs:2719-2746`): on Windows every signal is `TerminateProcess` over a toolhelp snapshot taken once (Hangup is a no-op), so descendants spawned after the snapshot survive. Our job-object kill-on-close (`tracer/src/job_object.rs`, `tracer/supervisor.rs:175-200`) is strictly better.
- **Live handoff** (`server/handoff.rs`, `pty/actor/unix.rs` quiesce/dup/commit/rollback): fd passing over `SCM_RIGHTS`, unix only, and the tracer already gives us restart survival. The only idea worth stealing is the explicit commit/rollback protocol with versioned manifest and `expected_version` check (`handoff.rs:36-54, 233-262`) if we ever do a tracer-to-tracer upgrade.
- **Serialising the rendered screen as ANSI** for restore (`persist/snapshot.rs:416-462`, `pane.rs:3575`): needs a terminal core that can export state; we hold an `alacritty_terminal` grid per session for status detection, so it is possible (L cost) and would fix sticky-mode loss in trimmed scrollback, but raw replay is working today.

### Where we are clearly ahead (one line each)

- Out-of-process per-session supervisor (rt-tracer) so PTYs, children and a 4 MB ring survive daemon restart; herdr's server restart kills panes on Windows.
- Reattach with ring replay and a lossless, bounded lifecycle queue, with exit ordered after the last chunk (`session.rs:972-1021`); herdr's `PaneDied` is sent from an independent `child.wait()` thread (`pane.rs:2747-2768`) with no ordering against the reader.
- Per-session sidecar gate that prevents a late status write from resurrecting a deleted sidecar (`session.rs:472-478, 653-700`), plus `.corrupt` quarantine of torn sidecars (`orphan.rs:354-365`) and a future-version refusal (`:384`).
- ConPTY input pacing (`PACING_CHUNK_BYTES`/`PACING_DELAY`, `tracer/supervisor.rs:286-318`) to avoid overrunning the console input buffer.
- Job-object kill of the whole PTY child tree, race-free, vs herdr's snapshot kill.
- Unattended auto-resume gated on transcript, pinned worktrees, branch and in-use checks, with per-session claims (`recovery.rs:602-658`); history entries recovered from tracer logs for sessions a reboot ended.
- Shell command quoting per family (pwsh/cmd/posix, cmd `%`/`!` refusal, typographic quotes) in `server/spawn/shell.rs:309-390`; herdr restricts to a bare command and rejects apostrophes.
- Secret-row sealing into Credential Manager so sidecars/state never hold literals.

---------------------------------------------------------------------------

DOC DRIFT: none found. CLAUDE.md "Where things live on disk" (2 MB ring, trims to 1.5 MB; `.truncated` flag) matches `crates/daemon/src/scrollback.rs:22-28`. Not exhaustively diffed against docs/architecture.md (skipped).

TASK INDEX GAP: not evaluated (docs/architecture.md Task Index not read in this run). Suggested row if wanted: "make persisted state crash-safe / recover from a torn state.json" -> `crates/daemon/src/secret.rs` (write_private), `crates/daemon/src/orphan.rs` (write_atomic, quarantine_meta), `crates/daemon/src/state.rs` (load_or_default, persist), `crates/daemon/src/scrollback.rs`.

MISSING ENTRY POINT: none observed (docs entry points were not opened in this run, so this is unverified).

## B. Agent detection and status

Paths: herdr = `X:\dev\_ext\herdrdev\herdr` (branch `claude-background-work-busy`); ours = `X:\dev\rustling-tulip`. Line numbers are from the files as read in this run.

Our herdr patch (not upstream): `src/detect/manifests/claude.toml` rules `background_shell_working` (lines 35-53) and `background_subagent_working` (55-71), version bumped to 2026.10.14.2. Mirrored in `distribution/agent-detection/claude.toml`.

### Ranked learnings (value/cost, best first)

#### 1. Wire the existing "after the last rule" region into `ScreenRegion` (value med, cost S)
- herdr: region `after_last_horizontal_rule` (`src/detect/manifest.rs:1300`, `1467-1478`) is where the footer under the prompt box is read (mode line, `· N shell`, subagent rows, "Esc to cancel" forms). Rules at `claude.toml:35-71`, `117-133`, `231-245`.

  Other regions: `whole_recent`, `prompt_box_body`, `above_prompt_box`, `last_non_empty_above_prompt_box`, `bottom_lines(N)`, `bottom_non_empty_lines(N)`, `top_non_empty_lines(N)` (engine 3), `osc_title`, `osc_progress`, and Codex prompt-marker regions (`manifest.rs:1094-1117`).
- ours: `Screen::after_last_horizontal_rule()` exists and is unit-tested (`crates/daemon/src/screen.rs:177`, tests 279-317) but no rule can read it: `ScreenRegion` has only `BottomNonEmpty`, `PromptBoxBody`, `AbovePromptBox`, `Title`, `Unknown` (`crates/agents/api/src/screen_rules.rs:45-63`; `region_lines` at `crates/daemon/src/screen/rules.rs:293-301`). Verified by search: `after_last_horizontal_rule` appears only in its definition and its own tests, with no caller.
- change: add `ScreenRegion::AfterLastRule` (and `TopNonEmpty { rows }` for trust-dialog-at-top screens) with the matching arm in `region_lines`. Additive: `#[serde(other)] Unknown` already absorbs old readers. Prerequisite for learning 3.

#### 2. Rule trace ("explain") and a capture-to-fixture command (value high, cost S core / M with a protocol surface)
- herdr: `herdr agent explain <pane> --json` returns every rule evaluated with `matched`, priority, region, matcher evidence, a 240-char region preview, the manifest source (bundled/remote/override) and version, fallback reason and warnings (`DetectionExplain`, `manifest.rs:28-47`; `evaluate_loaded_manifest` 431-517; JSON at 821-874; CLI `src/cli/agent.rs:67`, also `--file PATH --agent LABEL` to explain a saved screen).

  `herdr agent read <pane> --source detection` prints exactly the text detection sees. `scripts/capture_agent_screen.py` records repeated pane reads labelled idle/working/blocked/done (`STATE_CHOICES`). AGENTS.md:35, 199 make this the mandatory workflow for any rule change.
- ours: `CompiledRules::read` returns only the winning rule name (`crates/daemon/src/screen/rules.rs:93-109`, `Reading` 50-56); `pty_state.rs` logs it at debug (`pty_state.rs` check, around line 418). There is no way to ask why higher rules did not match, and no command that dumps the regions a session's rules see. Fixtures were captured by hand from spikes and collie (`screen/fixtures/README.md`).
- change: (a) `CompiledRules::explain(&Screen) -> Vec<RuleTrace { name, priority, region, matched, vetoed_by_not, region_preview }>` (S); use it in the `every_fixture_reads_its_state` failure message.

  (b) `rustling-tulipd screen <session-id> [--explain] [--fixture]` (like `cli_pair.rs`) that prints the grid rows, the regions and the trace, and with `--fixture` writes a `claude--v<cli version>-<state>.txt` ready for `screen/fixtures/` (M, new protocol request or a daemon-local read of the running session).
- value: Claude's TUI changes weekly; the rule loop is our most repeated maintenance task.

#### 3. Screen-derived `background` flag, so it survives restart and works without hooks (value med-high, cost M)
- herdr (our patch and upstream): background work is read from the screen and mapped to Working: footer `· N shell|monitors` in the after-last-rule region (`claude.toml:45`), subagent panel rows with elapsed and token counter (63), `Waiting for N background agents to finish` above the prompt box (33), `N MCP tasks still running` (81).

  Each has `not` guards for dialog footers (a dialog overlays the footer): `do you want to proceed?`, `esc to cancel`, `waiting for permission`, `tab to amend`, `ctrl+e to explain`.
- ours: `background` is hook-only by ruling (docs/plans/screen-status.md decision 6): set by a main-agent `Stop` whose payload lists a running task or one-shot cron (`crates/agents/api/src/hook_vocabulary.rs:46-90`), cleared by any status change, "never written to the sidecar" (CLAUDE.md "Orphan recovery", the `background` paragraph).

  Tests pin that the screen leaves it alone: `pty_state.rs:2033-2036` (a hook-idle session with `background` set, fed the background-shell frame, must keep the flag), `pty_state.rs:3257-3276` (subagent rows keep Idle). So after a daemon restart or reattach a session with a running background shell reads plain Idle, and a session with hooks off never shows `background`.
- change: do NOT copy herdr's mapping to Working (our ruling keeps Idle + flag). Add a screen verdict (e.g. `ScreenVerdict::Background`, additive with the existing `Unknown` fallback) from the four regexes above, applied only when status is Idle, behind the same hysteresis and record-moved check, set/cleared by the screen on reattach (`insert_reattached`) and when no hook has run.

  Fixtures already exist: `claude--idle-background-shell`, `claude--v2301-done-shell-running--w100`, `claude--v2302-working-agents-panel--w119`. Revisit decision 6 in screen-status.md.

#### 4. Rule composition: nested all/any/not, region-wide (multi-line) regex, case-insensitive contains (value med, cost M)
- herdr: a rule or gate carries `contains` (lower-cased both sides, `manifest.rs:1185`, `1202`), `regex` (over the whole region text, can span wrapped lines, `1236`), `line_regex` (any line, `1241-1246`), plus nested `all`, `any`, `not` gates (depth 8, `MAX_GATE_DEPTH` 269; evaluation 1227-1274).

  Examples: `bash_permission_prompt` ANDs "do you want to proceed?" with an option-line `any` (`claude.toml:201-229`); `live_blocked_form` ANDs "esc to cancel" with an `any` of confirm/select footers (117-133); `live_blocked_form_named_session` is one multi-line regex anchored to `\z` so later free-form output cannot satisfy it (145). Priority: highest wins, first in file wins ties (`manifest.rs:464-467`).
- ours: one `matcher` plus a flat `not` list per rule (`screen_rules.rs:24-40`); matchers hit if any single row matches (`screen/rules.rs:283-288`); `Contains` is case-exact (285); no conjunction, so a dialog is recognised by one distinctive footer string only.

  Verified in `agents/claude/src/rules.rs:67-106`: every dialog rule is a single footer string at priority 980. A footer wrapped across two grid rows at narrow width would not match a row-wise `Contains` (unmeasured: the fixtures are w100/w119 and wider).
- change: add `LineMatcher::All(Vec<LineMatcher>)` / `Any(..)` (custom decoder already maps unknown kinds to `Unknown`, `screen_rules.rs:90-102`), a `Region` flavour of `Regex` that joins the region's rows with `\n`, and lower-case both sides in `Contains`. Keep per-row semantics as the default so existing rules do not move; pin with the existing fixtures.

#### 5. Validate a whole rule table and fail closed (value med, cost S now, more once rules load from files)
- herdr: `validate_manifest` rejects the whole manifest on any bad rule: empty rule list, rule cap 128, gate depth 8, 512 gates, 1024 matchers, 512 chars per matcher (`manifest.rs:268-273`, `915-962`); a gate with no positive matcher (a rule of only `not`, which would match everything) is rejected (`991-993`, `1082-1088`); `skip_state_update` is allowed only with `state = "unknown"` and no `visible_*` flags (`931-944`); regexes are compiled at load (`1073-1080`).

  A rejected override or remote manifest falls back to the next layer with a `warning` shown in `explain` (`589-686`).
- ours: `CompiledRules::try_compile` refuses the whole table when any rule has a structural problem, an unknown part, an empty pattern, a pattern that does not compile, or breaks a cap (128 rules, 512 characters per pattern, 8 guards), through `ScreenRules::validate`; the daemon logs one error and that agent's screen sets no status. Dropping, say, an `overlay_*` Keep rule at 990 would let a lower rule decide silently, which is why a partial table is never used. The Claude table is also test-pinned (`claude_rules_keep_the_pre_seam_table`, `agents/claude/src/rules.rs`).
- change: a `ScreenRules::validate()` used by a conformance test in `agents/api/src/conformance.rs` (no empty matcher, no guard-only rule, unique names, descending priority) and, later, by the file loader, which should reject the whole file rather than skip a rule.

#### 6. Out-of-band rule updates: local override file first, remote catalog not worth it yet (value med for override / low for remote, cost M / L)
- herdr: three layers per agent, override `~/.config/herdr/agent-detection/<agent>.toml` > cached remote > bundled (`manifest.rs:589-686`, `override_path` 1119).

  Remote: catalog `https://herdr.dev/agent-detection/index.toml` (`schema_version = 1`, entries `{id, path}`, rejects `://`, absolute and `..` paths, `manifest_update.rs:16`, `349-387`), fetched with `curl` (5 s connect, 15 s total, 256 KiB cap, `509-557`) every 30 min (`app/mod.rs:42`, `app/runtime.rs:124-141`), opt-out `update.manifest_check` (`app/mod.rs:650`) and the global background-updates policy.

  Each manifest carries a dotted-numeric `version` (date based, `2026.10.14.2`), `min_engine_version` checked against `MANIFEST_ENGINE_VERSION = 3` (`manifest_update.rs:15`, `manifest.rs:903-911`), is rejected if older than cached or bundled (`manifest.rs:760-775`) or if content changed without a version bump (`manifest_update.rs:328-336`), written atomically with fsync (`439-471`), and recorded in `status.toml` per agent (`124-145`).

  Activation without restart: `reload_manifests_for_agents` (`manifest.rs:290-318`) and `herdr server reload-agent-manifests` (`cli/server.rs:22`, resets the detection runtimes). `scripts/agent_detection_manifest_check.py` validates bundled vs published (sha256, version bump, engine) and `just release-docs-check` blocks release on drift (AGENTS.md:205). Trust model: TLS and size caps only, no signature (verified: no signature code in `manifest_update.rs`).
- ours: rules are Rust data in the plugin (`agents/claude/src/rules.rs:33-164`); `screen_rules.rs:6-9` and screen-status.md decision 3 plan "a rule file can later load one" (RT-74), nothing built.
- change: since sessions survive a daemon restart in their tracers, a restart is already a cheap "reload", which makes a hot-reload channel low value for us.

  Do the local override only: `<config>/screen-rules/<agent>.json` (the table already serialises, `rules.rs:188`), loaded in `rules_for` (`screen/rules.rs:119`) over the plugin's table, with learning 5's validation, a `min_engine` integer (herdr's choice of rejecting an unknown-schema file and falling back to bundled is safer than our `Unknown`-and-skip), and a `warning` in the learning 2 trace. Skip the remote catalog: no distribution channel, auto-update is deferred (CLAUDE.md "deferred").
- note on per-agent-version rules: herdr has none (verified: `ManifestRule` fields `manifest.rs:154-182` and `AgentManifest` 141-151 carry no CLI-version field). It unions old and new patterns in one rule, e.g. the spinner regex covers Braille (<= 2.1.227) and half-circles (2.1.228) (`claude.toml:13-14`), and keeps a low-priority legacy catch-all (see 7). We do the same by naming fixtures by CLI version (`v2301`, `v2302`).

#### 7. Low-priority catch-all dialog rule (value low-med, cost S)
- herdr: `legacy_no_prompt_blocker` at priority 300 matches broad phrases ("do you want to" with "yes" or "❯", "waiting for permission", "review your answers") unless a bare `❯` line is present (`claude.toml:247-265`), so a footer reworded by a new Claude version still reads blocked until someone updates the specific rules.
- ours: only exact footers at 980 (`agents/claude/src/rules.rs:67-106`); a reworded footer is invisible to the screen. Hook-driven sessions are covered by `PermissionRequest`, so this matters for no-hook sessions and for dialog exits.
- change: one rule at about 300 with a guard on the prompt-box idle row. Risk is chat text containing "Do you want to", hence the AND with an option line (needs learning 4).

#### 8. Make third-party status reporting data, not a Rust crate (value low-med, cost M/L)
- herdr: `pane.report_agent {pane_id, source, agent, state: idle|working|blocked|unknown, message, seq, agent_session_id, resume_argv}` (`app/api/panes.rs:1554-1597`); the agent label is free-form (`normalize_reported_agent_label`, `app/api_helpers.rs:191-200`), a "custom-agent" works (`terminal/state.rs:3929`); `seq` must increase per `source` (`metadata_tokens.rs:15-41`, 32 sources cap); `pane.report_metadata` adds TTL'd key/value tokens (`metadata_tokens.rs`).

  Detection of "which agent runs here" is by foreground process group (`pane.rs:1028-1128`, unix `tcgetpgrp`) with N-miss confirmation (`AGENT_MISS_CONFIRMATION_ATTEMPTS`), a 3 s startup grace (`agent_detection.rs:12`, `pane.rs:1141-1155`), and "agent suspended to background, ignore the screen" (`pane.rs:1136`). Adding a built-in agent still takes code (the `Agent` enum, label tables, `BUNDLED_MANIFESTS` `manifest.rs:240-263`) plus one TOML; the report API needs none.
- ours: a new agent is a compiled-in plugin crate through `agent_api::Agent` (`crates/agents/api/src/lib.rs`); the hook route maps through the session's plugin (`server/hooks.rs`). Foreground-pgrp detection does not exist on ConPTY, which is why identity comes from `SessionStart`/`SessionEnd` and prompt return (`pty_state.rs:720-756`).
- change: if a second agent ever returns (Codex/Cursor were dropped, RT-266), expose the Claude-shaped `hook_vocabulary::map` as the reusable mapper for Claude-compatible hook emitters (herdr notes Grok imports Claude hooks, `integration/claude_settings.rs:17`), and consider a generic loopback `POST /status/{session}` taking `{state, message, seq}`. Do not copy the foreground-process machinery.

#### 9. Daemon-held "done, unseen" state (value low-med, cost M)
- herdr: Idle plus `seen == false` is `done` (`app/api_helpers.rs:96-107`); `seen` is cleared on a Working/Blocked to Idle completion transition when the pane is not on screen and set on any non-Idle state or when the user views it (`app/actions.rs:1844-1847`, `455-473`); the first Idle after an agent process is acquired does not count as a completion (`terminal/state.rs:257-264`); attention order Blocked 4 > Done 3 > Working 2 > Idle 1 (`api_helpers.rs:1-9`).

  It is on the wire for every API consumer, and `agent_view_eval.rs` is a declarative All/Any/Not/Eq/In/Exists filter and sort DSL over those fields.
- ours: "finished" is a client-side edge, `notify::finished` (`apps/native/src/notify.rs:259-270`: idle, no background, not a shell, previous Working or Idle-with-background; a first sight raises nothing), plus a per-client unseen set (`apps/native/src/footer.rs:23`). It handles background work better than herdr. Gap: a second client or the mobile app does not share the unseen set.
- change: only when the mobile app (docs/plans/mobile-app.md) needs it: a daemon-side `unseen_finish` seq on `SessionSnapshot`, cleared by a client `MarkSeen`. Not before.

#### 10. OSC 9;4 progress as a signal (value low, cost S)
- herdr: region `osc_progress` and rule `osc_progress_idle` (`^4;0`, priority 250, `claude.toml:275-280`); the OSC state is cleared on agent change so a replacement agent starts blank (`pane.rs:1105-1108`).
- ours: only the OSC 0/2 title is tracked (`screen.rs:58-70`); no 9;4. Title is already the lowest-priority rule and never writes alone (`Reading.from_title`, `screen/rules.rs:54`). Whether our title is reset when `hand_shell_back` runs is unmeasured. Add only if Claude's progress setting proves useful.

### How the two sources are merged (for the record)

- herdr, Claude: the hook reports identity only (`src/integration/assets/claude/herdr-agent-state.sh:54-55` exits for anything but `SessionStart`; it calls `pane.report_agent_session`); state is 100% screen. For agents with a full-lifecycle hook (`detect/mod.rs:330-340`: pi, omp, mastracode, opencode, kilo, kimi) the hook state is authority and the screen is ignored while that agent's process is the detected foreground process (`terminal/state.rs:2032-2037`, `2091-2099`, `844-852`).

  For other hook states: effective state = Blocked if a visible screen blocker is newer than the hook and the hook is not Blocked, else the hook state, else the screen state (`state.rs:2404-2420`, `2078-2089`). A screen Idle never overrides a hook Working and a visible Working never overrides a hook Idle (tests `state.rs:3934`, `3998`), so an interrupt leaves a hook-fed Working stale; this works for them only because the full-lifecycle plugins report interrupts.
- herdr hysteresis (`pane/agent_detection.rs:5-77`): 3 confirmations, 100 ms recheck, 700 ms cap, applied only to Working to plain-Idle; a rule flagged `visible_idle` (prompt box seen) bypasses it (`49`); Blocked and every entry publish at once; no rule matching defaults a known agent to Idle (`manifest.rs:550-552`, `DEFAULT_KNOWN_AGENT_IDLE_FALLBACK`).
- ours: hooks promote and map every state; the screen promotes only to Working (spinner) and AwaitingInput (dialog) and owns the exits no hook reports, behind 3 checks / 150 ms / 700 ms; any hook or owner-key write holds the screen for 700 ms and refuses a screen write over a moved record (`pty_state.rs:71-83`, `agree` 304-327, `moved_since` 671; docs/plans/hook-status.md "Coexisting with the screen rules").

### Where we are ahead (one line each)

- Hook-driven full status with `pending_input`, ordering by `X-RT-Hook-Time`, subagent exclusion by `agent_id`, `background_tasks`/`session_crons` (`hook_vocabulary.rs:46-90`); herdr's Claude hook is SessionStart identity only.
- The screen demotes a stale hook `Working` after Esc (no hook fires); herdr keeps a hook Working over a screen Idle (`state.rs:3934`).
- Hysteresis also covers AwaitingInput to Idle and requires the same target/record across the window (`pty_state.rs:304-327`); herdr debounces only Working to plain Idle.
- A screen no rule matches never writes (`CLAUDE.md`, screen-status decisions); herdr defaults to Idle.
- 37 committed, pinned fixtures (`screen/rules.rs:486-640`, `fixtures/README.md`); herdr deliberately has none for bundled rules (AGENTS.md:201: "Do not add tests that classify captured or invented CLI screens against bundled agent rules") and proves rules only by live smoke through hot reload (AGENTS.md:199, 203). Their warning still applies: fixtures prove the rules run as written, not that the current CLI looks the same, so learning 2's capture command is how we stay current.
- Event-driven checks after output with a 40 ms debounce (`pty_state.rs:64-68`) instead of herdr's polling plus idle-scan skipping by content sequence (`pane/agent_detection.rs:91-105`, `110-147`).
- `background` as a flag beside Idle keeps "Finished" notifications correct (`notify.rs:259-270`); herdr's fork maps background work to Working.
- No reliance on a unix foreground process group: works on ConPTY with agent-in-shell handoff (`pty_state.rs:720-756`).

### Not applicable (checked)

- `src/server/alt_screen_read.rs`: drives wheel events to harvest an alt-screen app's scrollback for `pane.read`, nothing to do with status. We read Claude transcripts from its files instead.
- `src/server/autodetect.rs`: launching/attaching to the herdr server on `herdr` with no subcommand, not agent detection.
- `src/app/tab_bar_status.rs`: user-configured shell commands rendered in the tab bar, not detection.
- `src/app/agents.rs`: `agent start/wait` with a 3 s settle (`AGENT_START_SETTLE_DELAY`) and 30 s default / 300 s max timeout; equivalent to our `wait_for_agent` / `agent_up` (`inject.rs`, 15 s).

## C. Protocol, socket API, CLI and remote

Roots: ours X:/dev/rustling-tulip, herdr X:/dev/_ext/herdrdev/herdr (patch branch, TOML-only patch, so source = upstream a few commits behind). Herdr paths below are relative to its root, ours to ours.

### How herdr's surface is layered (context for the items)

Three separate things, easy to conflate:

1. JSON socket API. Newline-delimited JSON, one request per connection, `{"id","method","params"}` -> `{"id","result"}` or `{"id","error":{"code","message"}}` (`docs/next/website/src/content/docs/socket-api.mdx:610-630`, `src/api/server.rs:328-520`). Versioned by a single integer `protocol` returned by `ping` (`src/api/server.rs:559-572`) and by `ServerCapabilities`. The CLI is a thin projection of it.
2. Private binary protocol for same-install attach (bincode, `[u32 len][payload]`, `PROTOCOL_VERSION = 22`, exact match required: `src/protocol/wire.rs:20,1656-1767`). Not stable across builds.
3. "Endpoint generation 1": the frozen JSON handshake (`endpoint.hello.v1`/`welcome.v1`) carried inside the binary frame's `EndpointControl{kind,data}` escape hatch, with codec lists, capability strings and a `methods` list (`src/protocol/endpoint.rs:1-198`, `src/server/client_transport.rs:738-848`).

   This is what makes client and server builds not need to match. Rules in the module doc (endpoint.rs:1-9): new JSON fields optional or defaulted, new enums need an `Unknown`, unknown named controls ignored.

Our wire is one layer (JSON over WS) with a range handshake, so layers 2 and 3 are mostly not our problem. The transferable parts are layer 1 and the policies.

### Ranked list (value/cost)

#### 1. Machine-readable error `code` on every refusal, and an answer to unknown requests (value HIGH, cost S)

Herdr: `ErrorBody { code, message }` (`src/api/schema/response.rs:36-40`). Codes are stable strings and are the contract.

They are `invalid_request`, `unknown_method`/`unsupported_method`, `not_found`, `timeout`, `events_lost`, `server_unavailable`, `protocol_mismatch`, `agent_blocked`, `agent_prompt_stalled`, `agent_not_ready`, `agent_target_ambiguous`, `invalid_regex`, `workspace_group_close_required`, `connection_local_only` (server.rs:289,370,575,595; wait.rs:41,87; agents.rs:171; app/agents.rs:308).

The skill tells agents to branch on them ("do not blindly resubmit after timeout/stalled", SKILL.md:171). Malformed requests still get a reply: id is recovered only when the line parses and has an unambiguous top-level string id, else empty id (server.rs:350-376). A host that sends an unknown method gets `unsupported_method` naming it (`src/server/client_transport.rs:97-105`).

Ours: `DaemonMessage::Error { message, request_id }` (`crates/protocol/src/lib.rs`) had no code; `ActionFailed { title, detail, hint }` is UI prose.

An unknown client message type is logged and dropped with no reply (`crates/daemon/src/server/connection.rs`, the `InboundClientMessage::Unknown` arm, verified), so a newer client waiting on a reply just times out. The planned `/agent/v1` routes use HTTP statuses 401/403/408/429 and CLI exit codes 0/2/3/4/5 (`docs/plans/agent-cli.md:47,99`) but no body code.

Change: (a) additive `#[serde(default)] code: Option<String>` on `Error`, defined as a small enum-backed catalogue in `crates/protocol`; (b) agent routes return `{"error":{"code","message"}}` in the body in addition to HTTP status, and the agent CLI prints that JSON on stderr; (c) for `InboundClientMessage::Unknown` that carries a `request_id` in `raw`, reply `Error{code:"unsupported_message", request_id}` instead of `continue`.

Codes to start with, taken from herdr: `not_found`, `ambiguous_target`, `target_blocked` (send refused because AwaitingInput, our Q7), `timeout`, `prompt_stalled`, `limit_exceeded`, `denied`.

#### 2. Atomic `prompt + wait` with an activity gate, identity pin and pre-write validation (value HIGH, cost M; lands in AC.5)

Herdr's `agent.prompt` takes an optional `wait {until, timeout_ms}` and does submit-then-wait in one request "avoiding a race between separate calls" (socket-api.mdx:114; handler `src/api/wait.rs:177-322`). The details we do not have in the plan:

- Validate everything that can fail before touching the PTY, and start the completion waiter before submitting, so a refused waiter spawn fails with nothing sent (`src/app/api/agents.rs:97-127,140-207`; test `agent_prompt_waiter_spawn_failure_fails_before_submitting`, agents.rs:676).
- Refuse if the target is blocked on a dialog, before writing a byte: `agent_blocked` (agents.rs:168-177). Same intent as our Q7 hold on `AwaitingInput`, but herdr refuses outright rather than holding.
- Activity gate: if the target was not `working` at submit, require observed `working` or `blocked` within 5 s, else `agent_prompt_stalled` (wait.rs:20,253-265). Without it, a prompt that never registered returns instantly because the agent is still `idle`/`done`. The caller's timeout includes submission time (SKILL.md:147).
- Pin the occupant: the wait captures the pane's `terminal_id` and agent kind and fails with not-running if a different occupant appears (wait.rs:235-242,375-383,416-419). A replacement cannot satisfy the wait.
- A wait also terminates on lifecycle events (agent released/exited) replayed from before submission (wait.rs:302-306), so a fast exit is not missed.
- Submission is `text` then Enter as one ordered unit with a 300 ms gap (`AGENT_PROMPT_SUBMIT_DELAY`, agents.rs:13, `queue_user_input_submission`, agents.rs:260-267), bracketed-paste aware (`encode_api_submission_parts`, app/api_helpers.rs:48).

Ours: plan has separate `send` and `wait` verbs (agent-cli.md:45-46), `wait` a long-poll on idle/ended/awaiting, 600 s per call (agent-cli.md:46). That has exactly the race above: `send` then `wait --idle` returns immediately if the status has not flipped yet.

Change: add `send --wait [--timeout]` (one route, one connection) that (1) validates target/scope/status, (2) registers the status waiter, (3) types via the existing `inject.rs` bracketed-paste path, (4) requires a Working/Awaiting transition (or the target already Working) within N seconds else code `prompt_stalled`, (5) then waits for settled.

Compare against the session record's identity (session id + a spawn generation) so a Restart/Resume under the same id does not satisfy the wait. Keep the standalone `wait` for the "already running" case.

#### 3. Stalled-peer handling: write timeout, bounded outbound queue, app-level liveness (value HIGH for remote/mobile, cost M)

Herdr: every client has two lanes (`src/server/client_transport.rs:120-127`): `control` (reliable queue) and `render` (capacity one, droppable: `try_send_render` returns `Full` if a frame is pending, lines 320-331). A skipped frame sets `render_pending` and the next tick does a full repaint (`src/server/clients.rs:161,445-459`).

Writers have timeouts: 5 s on the API socket (`STREAM_WRITE_TIMEOUT`, server.rs:33), 30 s on observers, with a test that a stalled observer releases both reader and writer (client_transport.rs:45,1766). Handshake read timeout 4 s (line 42). A `health_check` capability with `endpoint.health.ping/pong` exists specifically so "a quiet broken connection cannot remain Online indefinitely" (endpoint.rs:27-28, `src/server/client_endpoint_control.rs:1-20`, socket-api.mdx:947-950).

Ours (verified by reading and grep):
- `out_tx` is `mpsc::unbounded_channel` (`crates/daemon/src/server/connection.rs:91`); each PTY forwarder does `out.send(PtyOutput{..})` into it with no bound (`crates/daemon/src/server/sessions.rs:401-413`). A slow or half-dead tailnet peer makes the backlog grow in daemon memory (base64 inflated).
- `send_message` is `sender.send(..).await` with no timeout (connection.rs:328-336), and `send_pump` has none either (connection.rs:604-632). A peer that vanishes without FIN stalls that connection's pump indefinitely.
- No WS Ping/Pong or keepalive anywhere in daemon, daemon-client or tailnet_listener (grep `Message::Ping|keepalive|TcpKeepalive` over `crates/` finds only the tracer's own keepalive, `crates/tracer/src/supervisor.rs:912-987`; `apps/native/src/net.rs` has only a welcome timeout, net.rs:623-634). Whether the tailnet TLS layer sets TCP keepalive: unmeasured.
- Only file fetch has a bounded queue (`file_tx`, cap 4, connection.rs:94).

We cannot drop PTY bytes the way herdr drops frames (ours is an append-only byte stream), but we already have the right resync primitive: `LoadScrollback` + `forwarder_restarted` (sessions.rs:371-416, lib.rs:3811). Change:
1. Wrap the `send` in `send_pump` in `tokio::time::timeout` (say 30 s) and end the connection on expiry (close, as the existing `end_within` path does).
2. Give the PTY lane a byte budget per connection. Over budget: abort that session's forwarder, send a `Lagged{session_id}`-style notice (or just `Scrollback` with `forwarder_restarted:true` plus a fresh forwarder), exactly as the event lane already does for session events (sessions.rs:753-784). That is herdr's `render_pending` idea mapped onto our stream.
3. App-level `Ping`/`Pong` (additive messages, feature string `health_check` in `DAEMON_FEATURES`, handshake.rs:199) on tailnet connections only; client marks the host Offline after N missed pongs.

#### 4. Skew contract for a session-pinned agent CLI (value HIGH, cost S; changes AC.2/AC.7)

Why it applies: our agent CLI is the session's cached `rt-tracer-<hash>.exe`, kept for the session's life (agent-cli.md:78), while the daemon restarts and upgrades under it. That is herdr's client/server-skew situation exactly.

Herdr: every CLI call first does a `ping`, compares `protocol`, and fails with `protocol_mismatch` that says which side is older and what to do (`src/cli.rs:777-809`, `src/cli/protocol_guard.rs:16-43`, tests protocol_guard.rs:59-101).

The rest of the API is "ignore unknown fields; unsupported methods are normal errors" (socket-api.mdx:957-958); `pong` carries `capabilities` (`src/api/schema/server.rs:41-58`); the skill says "A missing method is not permission to stop or upgrade a server" (SKILL.md:211).

The CLI never replays a request after an ambiguous failure (`src/cli/target.rs:117-119`).

Ours: routes version by path `/agent/v1` (agent-cli.md:30), nothing yet on CLI/daemon mismatch behaviour.

Change: (a) `GET /agent/v1/self` (already `whoami`) returns `{api: 1, daemon_version, verbs: [...]}`; the CLI calls it first only when a verb returns 404/`unsupported_message`, to produce "daemon too old / CLI too old" with a code `version_mismatch`.

(b) CLI decodes replies tolerantly (no `deny_unknown_fields`). (c) rule in `agent-cli.md`: `spawn`/`send` are never auto-retried after a transport error; the error says "the request may have been applied; run `list` before retrying" (herdr SKILL.md:103 says this verbatim for SSH).

A cheap server-side companion: remember the last N `request_id`s per session token and answer a repeat with the first result, so retries become safe.

#### 5. Monotonic per-session status counter (value MED-HIGH, cost S)

Herdr `AgentInfo.state_change_seq: u64` (`src/api/schema/agents.rs:243`, set at `src/app/agents.rs:401`) increments on each state change; waits accept "only changes after seq N" (`after_state_change_seq`, wait.rs:249,274) and view sorts use it.

Ours has a timestamp `status_since` only: grep for `state_change_seq|status_seq` over `crates/` finds nothing, `status_since` appears in session.rs/orphan.rs/protocol. Timestamps tie and go backwards on reattach (CLAUDE.md: reattached sessions keep their old stamp).

Change: `status_seq: u64` on `SessionSnapshot` (`#[serde(default)]`) bumped under the registry lock where status moves; `wait`/`send --wait` take `--after-seq`; the AC.5 tests can then assert transitions without sleeping. Also gives the native client a cheap "did I miss a change" check after a lag resync.

#### 6. `wait --match/--regex` on the rendered screen, not on raw bytes (value MED, cost S-M)

Herdr `pane.wait_for_output` (`src/api/wait.rs:22-130`, `schema/events.rs:94-112`): substring or regex per line over a text read of the pane (`visible` / `recent` / `recent-unwrapped` / `detection`), `strip_ansi` default true, polled every 100 ms server-side.

It matches existing content immediately ("searches the selected snapshot immediately", SKILL.md:189); an invalid regex is rejected before waiting (`invalid_regex`); timeout optional; the loop stops when the client disconnects (`should_stop_connection`, server.rs:924-933, test server.rs:1636).

`recent` is silently remapped to `recent-unwrapped` for matching so a soft wrap cannot split a match (`src/api/subscriptions.rs:12-19`). The subscription form `pane.output_matched` is edge-triggered (`currently_matching`, subscriptions.rs:376-379) so it fires once per appearance. It also returns `revision` (`ResponseResult::OutputMatched`, response.rs:191) so a caller can tell stale from fresh reads.

Ours: planned `read --output` is a scrollback tail through `strip_ansi` (agent-cli.md:44, AC.6) and `wait` is status-only; no output wait.

Change: add `wait --match TEXT | --regex RE [--source screen|recent]` to AC.5, evaluated against the daemon's existing alacritty grid (`crates/daemon/src/screen.rs`, `pty_state.rs`) joined across wraps, not the raw ANSI byte ring, so escape sequences and wraps cannot split a match.

Event-driven (re-evaluate on the grid's debounce tick we already have) rather than herdr's 100 ms poll. Keep our 600 s cap per call (herdr's indefinite default is worse for a long-poll). Return `revision` (grid write count).

#### 7. Windows Codex prompt-submit quirk (value MED if Codex is used on ConPTY, cost S)

`src/app/api/agents.rs:15-33`: Codex's Windows input reader does not surface bracketed paste, treats the paste as a "paste burst" and rewrites a following Enter to a newline until an idle timeout. Size-based delays are guesses; the fix is to send a non-character key (Right arrow) after the paste and before Enter, because Codex flushes a buffered burst synchronously on such a key. Also Copilot needs a focus-gained event before synthetic Enter (agents.rs:238-249).

Ours: bracketed paste is tracked in `termstate.rs`, and `inject.rs` has its own timing steps, but grep for "paste burst|paste boundary" finds neither the quirk nor a per-agent hook. Change: put a per-agent "submit boundary" in the plugin descriptor (agent-api `Agent` trait) and use it in `send`, rather than naming agents in the daemon (CLAUDE.md invariant). Only matters when `send` targets Codex on Windows.

#### 8. Generate a JSON Schema artifact and fail the build when it is stale (value MED, cost M; S if limited to `/agent/v1`)

How herdr does it (all verified):
- Derive `schemars::JsonSchema` on `Request`, `SuccessResponse`, `ErrorResponse`, `EventEnvelope`, `SubscriptionEventEnvelope` and every param/result type (`src/api/schema.rs:36-41`, `schema/response.rs:24-42`; `schemars = 1.2.1` in Cargo.toml:54).
- The bundle is built in a test, not a build script: `protocol_schema_document()` makes `{ $schema, title, schema_version: 1, protocol: PROTOCOL_VERSION, schemas:{request,success_response,error_response,event,subscription_event} }` and rewrites each `#/$defs/..` ref to `#/schemas/<name>/..` so the bundle is self-contained (`src/api/schema/tests.rs:5-46`; a test forbids leftover `#/$defs/` refs, tests.rs:156-180).
- The artifact `docs/next/api/herdr-api.schema.json` is committed; `generated_protocol_schema_artifact_is_current` compares byte-for-byte and fails with the regeneration command; `HERDR_UPDATE_API_SCHEMA=1` rewrites it (tests.rs:183-207). Reviewers see the diff in the PR.
- The same JSON is `include_str!`-embedded and printed by `herdr api schema [--json|--output]` (`src/cli/api.rs:1-97`), so the schema always matches the binary. The release script treats the file as a shipped asset (`scripts/test_release.py:78`, `nix/package.nix:45`).
- Bonus contract test: for every method in the schema, synthesize a sample value from the schema itself, parse it as `Request`, and check name/flags round trip (`src/api/schema/operations.rs:64-148`, `operation_catalog_matches_every_wire_method`). Adding a method without keeping schema, serde and tables in sync fails a test.
- Not guaranteed by herdr: the schema does not classify a change as breaking or additive; that stays convention (endpoint.rs:7-9).

Ours: hand-written serde enums in an 8268-line `crates/protocol/src/lib.rs`; no schema artifact; `schemars` is only a transitive dep (Cargo.toml files do not name it; Cargo.lock lists it). The additive-vs-breaking rule in CLAUDE.md is enforced by review alone, and `protocol-version.json` is hand-bumped (`crates/protocol/build.rs`).

Change: start with the new `/agent/v1` request/response types only (small, no `Unknown` or `serde_json::Value` fields to fight): derive `JsonSchema`, commit `docs/agent-api.schema.json`, add the staleness test and the sample round-trip test, embed with `include_str!` and expose `rt-tracer agent schema` (useful for the skill pack).

Extending to the whole WS protocol is a separate, larger step: it needs schemars attributes for the `Unknown` wrappers (`InboundClientMessage` is a manual wrapper, lib.rs:4050) and base64 string fields; value is a visible diff on every wire edit, which makes the "no bump needed" calls auditable.

#### 9. One operation table drives names, flags, and the advertised method list (value LOW-MED, cost S)

`define_operations!` (`src/api/schema/operations.rs:10-56`, used at schema.rs:45-148) declares each method once with its params type, wire name and two flags (`changes_ui`, `client_shell`). The same table produces the serde enum, the `OPERATIONS` catalogue, the `methods` list sent in the handshake welcome (client_transport.rs:830-836) and the allow-check for the client-shell lane (`supports_client_shell_method_name`).

Clients disable only the actions whose method is missing instead of disconnecting (socket-api.mdx:949-953). Ours: `DAEMON_FEATURES` is three hand-listed strings (handshake.rs:199-203), and the rule "list a feature only once it is fully honoured" is by convention. Worth it for the agent verbs (item 4's `verbs` list generated from one table); not worth retrofitting to `ClientMessage`.

#### 10. Agent-native CLI ergonomics to copy into agent-cli.md (value MED-HIGH, cost S each)

From `skills/herdr/SKILL.md`, `src/cli/*`, and `src/main.rs`:
- Ship the skill inside the binary: `herdr --skill` prints `include_str!("../skills/herdr/SKILL.md")` so the text always matches the build (main.rs:449-450,737); `--help` footers point agents at it ("SKIP if a Herdr skill is already in your context", cli.rs:47-55). Our Q11 picks a skill in the repo the user links manually (agent-cli.md:127). Because our CLI is session-pinned, embedding gives version-exact docs: `rt-tracer agent skill`.
- Self-gating: injected `HERDR_ENV=1`; the skill's first step is `test "$HERDR_ENV" = 1` and stop otherwise (SKILL.md:10-16). Ours has `RT_SESSION_ID`/`RT_AGENT_TOKEN` (agent-cli.md:34); add an explicit marker (`RT_ENV=1`) or gate on `RT_AGENT_TOKEN`.
- Output contract: success JSON on stdout, errors as the same JSON on stderr with exit 1, syntax errors exit 2 (SKILL.md:214; cli.rs:732-740). Creation responses return every id the next call needs (`.result.pane.pane_id`), and ids are opaque, stable, and never reused after close (SKILL.md:61-69). Ours: plain text by default and `--json` (agent-cli.md:47); recommend JSON-first for agents plus the error code from item 1, and keep our finer exit codes.
- Target resolution: a command takes a unique live name (`[a-z][a-z0-9_-]{0,31}`) or an id; names follow the occupant and clear on exit; ambiguity is an error that lists candidates (`agent_target_ambiguous`, `src/app/agents.rs:297-312`). Ours: `send <session>`; define whether labels are accepted and make duplicates ambiguous-by-error.
- `--current` and `--no-focus` as first-class flags so an agent never steals the user's focus or targets "the focused pane" that may belong to the user (SKILL.md:77,206-207). For us: spawn/send never change the user's active tab/focus unless `--focus`.
- Verbatim text operands: prompt text beginning with `--wait` or `-h` stays text, via a clap escape applied to declared "verbatim" positional slots (`src/cli/projection.rs:186-260`). Directly relevant to `send "..."` and `spawn --prompt`.
- Discovery by running the group name with no subcommand prints the synopsis and exits 2; synopsis lines are generated from the clap declarations so they cannot drift (projection.rs:79-156, cli.rs:109-114). Rule to copy: "never probe a mutating command by omitting arguments" (SKILL.md:43).
- CLI flags derived from API param structs: `cli_params!` declares each field once with its serde attributes and its CLI binding (`src/cli/projection.rs:649-663`, example `src/api/schema/server.rs:6-33`), and `Leaf{name,about,id,arguments,method}` maps a subcommand to a `Method` (projection.rs:47-60).

  The CLI cannot gain a flag the API lacks. For three or four verbs this is overkill; for the planned verb set a plain shared `Params` struct per verb used by both the route and the CLI achieves the same.

#### 11. Do not replay, and make spawn idempotent by `request_id` (value MED, cost S-M)

Herdr never retries a mutating request after an ambiguous failure (`src/cli/target.rs:117-119`; "A connection failure does not prove a mutation was not applied: inspect remote state before retrying", SKILL.md:103). Ours has `request_id` on SpawnSession/DuplicateSession/etc. (CLAUDE.md "Wire-protocol gotchas"; lib.rs:2613) but only for reply routing.

Whether `daemon-client`/the native client auto-resend after a reconnect: unmeasured. Change: daemon keeps a small LRU of `(connection-independent client_id, request_id) -> reply` for spawn-class requests so a resend after reconnect returns the original result instead of a second session. Also covers the `/agent/v1/spawn` retry case (item 4).

#### 12. Event model: what to take, what not to (value LOW-MED, cost S)

Herdr: one process-wide `EventHub` ring of the last 512 `(u64 seq, event)` pairs (`src/api/event_hub.rs:19-67`), each subscription keeps a cursor; a cursor older than `first-1` is `Lost` and the server sends `events_lost` with the original request id and closes the subscription (`src/api/subscriptions.rs:326-340`; test event_hub.rs:92). Subscriptions are polled every 100 ms by a thread per subscriber (`CONNECTION_POLL_INTERVAL`, server.rs:30,858-890).

Doc contract (socket-api.mdx:796-817): history not durable; on `events_lost` resubscribe, wait for `subscription_started`, then take `session.snapshot` on another connection; "events invalidate cached state; they cannot be unconditionally replayed over a snapshot" because snapshot and events share no sequence boundary. Subscribe-by-type with per-pane filters; invalid pane in any entry rejects the whole request (`stream_subscriptions`, server.rs:811-856).

Ours is stronger for UI clients (see "where we're ahead"). Take only: (a) the written rule "events are invalidations; reconcile by read" in `docs/architecture.md` for the agent routes; (b) for `GET /agent/v1/wait`, a cursor (item 5) instead of a stream. Do not copy the 100 ms polling or the close-on-lag.

#### 13. Remote bridge design: what transfers (value LOW, cost varies)

Herdr (SSH only): the local `herdr --remote host` runs `ssh host herdr remote-client-bridge` and exposes a 0600 local socket that the TUI client connects to (`src/remote/attach.rs:30-106`, `src/remote/host.rs:6-38`).

The remote half starts the server if needed (`host.rs:40-63`), refuses to attach if the server lacks the generation-1 endpoint (host.rs:48-58), and the local launcher can install/upgrade the remote binary from a release manifest, then decides keep/handoff/stop by capability (`src/remote/restart_policy.rs:16-62`, e.g. no `health_check` => must restart).

A separate `remote-api-bridge` with `--check` printing `herdr-api-bridge-v1` (`src/remote.rs:15-39`) lets `herdr --machine X <any api command>` run against the remote socket (`src/cli/target.rs:66-100`). The remote bridge process has an idle watchdog: 60 s with no positive-byte I/O on a suspend-inclusive clock => `process::exit(1)` so abandoned `ssh` children do not accumulate (`src/platform/remote_bridge.rs:8-60`).

Transfers to us: (a) the idle watchdog idea, on the server side of item 3 (the tailnet listener should reap idle/half-open sockets); (b) capability-driven "must the remote restart" planning is what our `classify_existing_daemon`/`retire_daemon` already do from `supported_versions`; (c) cross-host agent CLI (`--machine`) is out of scope for us. Nothing to change in remote-access.md.

#### 14. Terminal streaming model: raw bytes vs rendered state (value MED for the mobile app, cost L; do not do now)

Herdr never forwards raw PTY bytes.

The server owns a virtual terminal, renders each client's view, and sends either (client-shell) a semantic `PaneSurface` frame (`src/protocol/wire.rs:1469`).

That frame can come as sparse deltas against a numbered base (`SurfaceDelta{base_projection_revision, base_surface_revision, ..}`, `src/protocol/surface_delta.rs:15-22`), cell reuse and scroll patches, each an opt-in flag in the client hello (`surface_reuse|surface_delta|surface_scroll`, endpoint.rs:56-66), or (terminal attach) already-diffed ANSI bytes with `seq` and a `full` flag (`TerminalFrame`, render_stream.rs:15-31,116-154).

Per-client state is a baseline plus a "repaint pending" flag, so memory per client is O(screen) and a slow client just gets the latest state. Cost to them: the server does all rendering per client, a graphics and keyboard-protocol translation layer, and frozen bincode variant order (wire.rs:1381).

Ours: raw bytes (`PtyOutput.data_b64`) plus a 2 MB scrollback ring replayed on attach, client renders with alacritty_terminal (CLAUDE.md). That is the right call for a native GPUI client and for tracer survival. It is the weak point only for lagging remote clients (item 3) and for a thin mobile client that should not run a terminal emulator.

If mobile (`docs/plans/mobile-app.md`) needs it: add an opt-in `screen` mode that sends the daemon's existing grid (`crates/daemon/src/screen.rs`) as a snapshot plus row patches with `revision`s, and use the same snapshot as the lag resync (cheaper than replaying 2 MB). Treat as a separate design.

#### 15. Auth on the local socket (value LOW, cost M; informational)

Herdr has no token. API socket is `0600` (`src/api/server.rs:29,304-306`); on Windows the named pipe is created with SDDL `D:P(A;;GA;;;SY)(A;;GA;;;OW)` (SYSTEM and owner only) at bind time (`src/ipc.rs:177-205`, test ipc.rs:404), and an elevated server rejects unelevated same-user clients unless `server.allow_unelevated_clients` is set (`src/server/headless/bootstrap.rs:35-38`, `src/cli/spec.rs:180-183`).

Identity inside a pane is advisory: `HERDR_PANE_ID` env plus a `caller_pane_id` parameter that the server believes (socket-api.mdx:144-165, 253-257). "Do not control a session from outside Herdr" is a skill instruction, not enforcement.

Ours: loopback TCP port + token in `daemon.json`, plus (planned) per-session bearer tokens that scope the agent CLI (agent-cli.md:26,34). That is stronger than herdr for agent identity. The only thing to borrow is optional: an owner-only DACL named pipe would remove the loopback TCP surface for the agent CLI (any local account can reach 127.0.0.1:port; the token is what stops them), which agent-cli.md Q1(c) already considered and rejected.

No change recommended; if the tailnet-off default ever needs a local-only story, the interprocess crate plus that SDDL is the template. The elevated-server note matters: running the daemon elevated would silently lock out a normal-privilege client; not a case we handle or test (unmeasured).

#### 16. Smaller, mostly free

- Retired API names return the standard `unknown_method` error with the requester's id, not a silent close (`src/api/server.rs:271-293`; test 1296). Copy when we retire a message.
- `pane.report_agent` family as an authority protocol: `source` string with strict charset, `seq` for out-of-order reports (ignored if <= last accepted), `ttl_ms` 1..86400000, token maps with per-source slot caps, `agent`/`applies_to_source` guards, `resume_argv` validated (≤64 elements, ≤8 KiB, command name not a path, no control chars; socket-api.mdx:674-771).

  Relevant to our hook route (`/hook/{session_id}`, `crates/daemon/src/hook_status.rs`): we have an ordering check per CLAUDE.md; the `ttl` and size caps on any externally reported strings are cheap hardening. Detection owner agent covers the rest.
- Input durability for client shells: server tracks presses it forwarded and injects releases on abrupt disconnect (`shell_held_inputs`, `src/server/clients.rs:172-173`). Our clients send terminal bytes so there are no releases; only a mouse-reporting TUI left mid-drag after a client drop would matter. Skip.
- Geometry limits at handshake (`MAX_CLIENT_SHELL_DIMENSION=4096`, cells ≤ 1,000,000, input payload ≤ 1 MiB, ≤4096 input events per message; `client_transport.rs:47-69,117-118`) and a 2 MiB frame cap (wire.rs:24). Our `Resize` and `SendInput` size bounds: unmeasured; worth a check when item 3 is done.
- Per-connection write of `ServerShutdown{reason}` so clients exit gracefully (wire.rs:1406). Ours: tailnet connections get a WS Close when the listener ends (CLAUDE.md); loopback do not. Fine.

### Where we are ahead (one line each)

- Lag handling keeps the connection and resyncs in-band: whole `Sessions` list, then re-sent spawn replies with their `request_id`, receiver re-subscribed before the list is read so no gap (`crates/daemon/src/server/sessions.rs:753-806`). Herdr closes the subscription with `events_lost` and makes the client rebuild, and it polls every 100 ms (server.rs:889).
- Per-session credentials with hash-only storage and revocation for agents (agent-cli.md:34), versus herdr's filesystem-only trust and an unauthenticated `caller_pane_id`.
- Range-negotiated versions plus `Welcome.features` and `Unknown` wrappers (`handshake.rs:199-230`) are cheaper to evolve than herdr's exact-match private `PROTOCOL_VERSION` (wire.rs:1749-1767) whose bincode variant order is frozen.
- Per-device pairing, revocation and an owner rule on a TLS tailnet listener; herdr remote is SSH only and has no per-device credentials in core.
- Request/reply ordering guarantees across the byte stream (reply queued before the new forwarder's first `PtyOutput`, sessions.rs:365-416) and multi-request multiplexing on one connection; herdr is one request per connection.
- Resize arbitration between clients (`claim`, CLAUDE.md) and per-client layouts keyed by client id; herdr picks a "foreground client" by last activity (clients.rs:140).
- Tracer-backed session survival on Windows, which herdr lacks (herdr-comparison.md:21).
- Bounded queue already used for file fetch (connection.rs:94); item 3 extends the idea to the PTY lane.

## D. Operational robustness, worktrees and tests

Herdr checkout: X:\dev\_ext\herdrdev\herdr (v0.9.5, local patch branch, 13 behind upstream/master). Ours: X:\dev\rustling-tulip.
Ranked by value/cost, best first.

### 1. Size-capped log writer (our daemon.log is unbounded within one run)

- Herdr: `RotatingFileMakeWriter` in src/logging.rs:388-544. It is a `MakeWriter` behind `Arc<Mutex<State>>` that tracks `current_size`, rotates when `current_size + incoming > max_bytes` (default 5 MiB, `DEFAULT_MAX_LOG_BYTES` :9), and keeps N numbered generations (`.1`, `.2`; default 0 means delete and restart, :10).

  Any IO error flips a `disabled` flag, so logging never panics or blocks (:430-475). Separate files per role (herdr-client.log, herdr-server.log). Own filter env `HERDR_LOG`, default `herdr=info` (:22-23). Chatty routine API methods are demoted to debug unless they fail (:61-117).
- Ours: crates/daemon/src/main.rs:700-768. `init_tracing` renames the previous log to `.old` and truncates at START only; within a run the file grows without bound. The default filter is `info,daemon=debug` (:702), and git.rs:90 logs `git invocation: begin` at info on EVERY git call (plus a completion line), so a long-lived daemon with the live git watcher writes continuously. CLAUDE.md "Where things live on disk" confirms rotation is per start only.
- Change: replace the `Mutex<File>` writer at main.rs:725 with a size-capped writer (copy the ~100 lines; keep `daemon.log.old` as generation 1 at startup, plus roll at e.g. 10-20 MiB mid-run). Demote git.rs:90 `begin` to debug and keep the existing "slow >500 ms" info path. The Windows trap: renaming an open file fails, so close the handle before rename (herdr does `self.file.take()` at logging.rs:510). Same for apps/native native.log.
- Value: high (long-lived daemon, disk growth, and the `.old` only covers the previous run). Cost: S.

### 2. Quarantine an undecodable state.json instead of overwriting it

- Herdr: config parse failure falls back to defaults but says so loudly (`config parse error: ...; using defaults`, src/config/io.rs:172-178), and the file on disk is never rewritten by the loader.
- Ours: crates/daemon/src/state.rs:124-131 logs `state.json corrupt, starting fresh` and continues with `PersistedState::default()`. The next `persist()` (state.rs:419-424) renames over `state.json`, so the user's repos, workspaces and layouts are destroyed with only one warn line. We already have the right pattern for sidecars: orphan.rs:354-363 `quarantine_meta` renames to `meta.json.corrupt`.
- Change: in `load_or_default`'s Err arm, rename `state.json` to `state.json.corrupt` (or timestamped, keep last 3) before continuing, and surface `state_file_corrupt()` (state.rs:180, already exists) to the client as a one-time banner. CLAUDE.md says "no backward compatibility before first release", but after v0.1.0-alpha a decode failure from a schema slip becomes a data-loss bug.
- Value: med-high. Cost: S.

### 3. nextest: make hung tests die (no terminate-after today)

- Herdr: CI wraps jobs in `timeout-minutes` (ci.yml:33,66), and the integration harness has an owner-pid marker plus a 1 s watchdog thread that kills leaked servers whose runtime dir vanished (tests/support/mod.rs:16-17,629-656,639-680), plus panic hook, ctrlc and atexit cleanup (:565-587). A killed test run does not leak servers.
- Ours: .config/nextest.toml:13 has `slow-timeout = { period = "60s" }` with no `terminate-after`, so a hung live e2e test (real daemon + tracers, serial-live group) blocks the suite forever; nextest only warns SLOW. Cleanup relies on `Drop for LiveDaemon` (apps/native/tests/support/live.rs:376-396) and the tracers' kill-on-close job. A nextest SIGKILL on timeout skips `Drop`, so processes can leak after a timeout; a process-level watchdog is not present (checked live.rs: only Drop/kill_tree).
- Change: `slow-timeout = { period = "60s", terminate-after = 5 }` for default, a longer override for the `serial-live` filter (e.g. period 120 s, terminate-after 5), and optionally a `[profile.ci]`. Then add the cheap half of herdr's trick to live.rs: write `<root>/.rt-test-owner-pid` and have `LiveDaemon::start` sweep daemons whose owner pid is dead (taskkill by pid read from daemon.json).
- Value: med. Cost: S (timeout) / M (sweeper).

### 4. `git worktree add` failures after a hand-deleted worktree (prune, locked, prunable)

- Herdr: `parse_worktree_list_porcelain` keeps `bare`/`detached`/`prunable` flags (src/worktree.rs:425-492), prunable entries are skipped when matching branch holders (src/app/api/worktrees.rs:76,548).

  Removal has a recovery ladder for "is not a working tree" including verifying a leftover checkout's `.git` gitdir actually points into this repo's worktrees dir before `remove_dir_all` (worktree.rs:346-423), and dirty-vs-submodule removal errors are classified with `LC_ALL=C` forced (worktree.rs:194-199,327).

  Request-scoped `-c safe.directory=<repo>` for untrusted-owner repos (:160-169). Honest note: herdr does NOT handle locked worktrees or long paths either (no `locked`, `longpaths` hits in src/worktree.rs).
- Ours: git.rs:320-340 `parse_worktree_list` drops `prunable`/`locked`/`bare`. `existing_or_derived_worktree` (git.rs:1010-1036) falls back to a derived path when the holder dir is gone, but nothing runs `git worktree prune` before `worktree_add` (git.rs:414-434; `worktree_prune` is called only from worktree_cleanup.rs:141 after a removal).

  With a registered-but-missing worktree (user deleted the folder, Defender quarantine, a failed fs fallback), git still considers the branch checked out/registered at the old path, so the add is expected to fail ("already checked out" / "missing but already registered").

  I did not run this; add a test. Also worktree_cleanup.rs:48-90 uses a single `--force`; git needs `--force --force` for a LOCKED worktree, and `worktree prune` never drops locked entries, so a locked one would be fs-deleted but stay registered and keep its branch "checked out".
- Change: (a) call `worktree_prune` at the start of the add path when `worktree_holding_branch` reports a holder whose dir is missing; (b) parse `prunable`/`locked` in `parse_worktree_list` and ignore prunable holders; (c) `worktree remove -f -f` on the retry (worktree_cleanup.rs:58); (d) optionally `-c core.longpaths=true` on Windows for add/remove (deep node_modules).
- Where we are ahead: our cleanup ladder (retry after 250 ms for Defender, fs fallback, empty-ancestor pruning, `.rt-group` markers, workspace-aware layout) is richer than herdr's single-repo model; also per-repo async mutex (git.rs:31-53) and non-interactive network env + 20 s fetch timeout (git.rs:22-29) have no herdr equivalent.
- Value: med. Cost: S.

### 5. A GitHub Actions CI + tag-triggered release (we have no .github at all)

- Herdr: ci.yml: `concurrency` with cancel-in-progress (:25-27), matrix ubuntu/macos/windows with `fail-fast: false` and per-OS nextest filters (:55-64), `timeout-minutes`, SHA-pinned actions, cache saves only on push to master so PR caches do not evict master's (:20-23), explicit Windows ConPTY smoke step (:197-203), a Windows-only packaging job. Conventional-commit validation job.

  Release workflows (release.yml:23-33) refuse to publish unless both `GITHUB_ACTOR` and the rerunning actor have repo-admin permission; tags are protected by a ruleset and published releases are immutable (AGENTS.md "Release Channels"). Maintenance scripts (release.py, changelog.py) have their own unit tests, and workflows are tested as code (scripts/release-workflows.test.ts, run by `just maintenance-test`, justfile:15-17).
- Ours: `ls .github` fails: no workflows. Gates run only through tools/gate.ps1 on the dev machine/builder; release is the 9-step manual list in docs/release.md (installer built locally, `gh release create` by hand).
- Change: add `.github/workflows/ci.yml` (windows-latest: fmt --check, clippy -D warnings, `cargo deny check`, nextest `--cargo-profile test-release`; skip native-e2e/smoke which need a desktop session, or run them on a self-hosted runner), `concurrency` cancel, timeouts, `rust-cache` saved on push only. Add `release.yml` on `v*` tags that builds the installer and attaches it plus a `.sha256` (prereq for item 8). Keep the admin-actor guard idea.
- Value: high once others contribute / before first public alpha; Cost: M.

### 6. Config/settings diagnostics: per-section tolerance and a `check` command

- Herdr: `Config::load` (src/config/io.rs:131-181) falls back to defaults on a parse error but reports `config parse error: <toml error with line>; using defaults`.

  Unknown keys are found with `serde_ignored` and reported with a dotted path (io.rs:441-526, `Cargo.toml` dep serde_ignored); the reload path `load_live_config` (io.rs:245-386) parses section by section, so an invalid `[ui]` keeps the CURRENT value of that section and lists it in `invalid_sections` while the others apply, and says "keeping current config".

  `config_diagnostic_summary` (:202-243) turns the list into one banner line; `herdr config check` (src/cli.rs:309-333) prints diagnostics and exits 1. Reload is explicit (keybind or API, src/app/mod.rs:811, app/api.rs:967), not file-watching. Tolerates a UTF-8 BOM, even a mid-file one produced by line edits (io.rs:103-120). scripts/config_reference_check.py fails CI if the documented key list drifts from the serde structs.
- Ours: state.json is all-or-nothing (state.rs:124-131). native-ui.json is already per-key tolerant (CLAUDE.md says unknown values load as defaults) but silently, so a typo or an unknown key is invisible. `agent_settings` are validated per agent already (agent-api validate).
- Change: (a) report unknown/invalid keys in native-ui.json and state.json host settings as a toast/log line (serde_ignored is a small dep); (b) `rustling-tulipd check` or `rt.ps1` verb printing the same diagnostics; (c) if we ever add hot reload, copy the keep-current-per-section semantics. No file watching needed.
- Value: low-med. Cost: M.

### 7. Git refresh: cheap identity probe + demand-driven work (compare with the notify watcher)

- Herdr: polling, not watching. `GIT_REMOTE_STATUS_REFRESH_INTERVAL` 1.5 s (src/app/mod.rs:40), repo discovery every 5 min (:41), one background thread at a time with a `due_after_in_flight` flag so a trigger during a refresh queues exactly one more (src/app/git_refresh.rs:109-175).

  Branch/HEAD/upstream are read straight from `.git/HEAD`, refs, packed-refs and config files with stat-stamped dependency tracking (`FileDep`, src/workspace/git/status.rs:48-62,254-292) and a fallback to `git symbolic-ref`/`rev-parse` only for reftable repos (`git_ref_storage_is_reftable`, status.rs:52,255).

  Work is demand-driven: only compute branch / ahead-behind when a sidebar token needs it (git_refresh.rs:184-194). Items for workspaces in the same repo are deduplicated by repo-root cache key (git_refresh.rs:218-249). No dirty-state watching.
- Ours: notify recursive watcher + 750 ms debounce + hand-maintained allowlist (git_watch.rs:144-198, CLAUDE.md "Live git watcher"), parked while no client is connected. This is a better UX for dirty state (herdr has none), and our noisy-dir exclusion at any depth is stronger. Gaps I verified or flagged:
  - Reftable: `classify_git_internal` (git_watch.rs:183-198) only knows `refs/**`, `packed-refs` (:100) and a few files; grep for `reftable` in git_watch.rs found nothing, so in a reftable repo (git 2.45+, `extensions.refStorage=reftable`) a ref-only change (e.g. `git commit --allow-empty`, `git branch -f`, a push updating remote refs) writes only `.git/reftable/*` and is dropped by the `_ => None` arm. Add `["reftable", ..] => STATUS`.
  - `.git` as a FILE (linked worktree or submodule registered as a repo): HEAD/index live in `<common>/worktrees/<name>/`, outside the watched `repo_root`. I did not verify how `Active` promotion treats this (watcher is keyed on a `.git` entry existing); write a test with a registered linked worktree. Herdr resolves `git_dir` vs `git_common_dir` explicitly (status.rs:53-56).
  - Cheap pre-check before `git status`: herdr's stat-stamped fingerprint avoids spawning git when nothing moved. We could hash (HEAD, index mtime, packed-refs mtime) per repo to skip spawning git when the debounced event batch changed nothing material.
- Value: low-med (reftable is rare today, rising). Cost: S (reftable arm) / M (fingerprint skip).

### 8. When auto-update lands: how herdr downloads, verifies and swaps on Windows

- Herdr (update.rs + distribution/install.ps1; the Windows path does NOT replace a running exe):
  - Manifest fetch via `curl -sfL --retry 3 --connect-timeout 10 --max-time 20` and serde_json (update.rs:336-360); stable `https://herdr.dev/latest.json`, preview `preview.json`; the manifest carries version, per-target `download_url` + `sha256`, announcement, and `endpoint_generation`/protocol so an older updater knows whether a running server must be replaced (AGENTS.md "Stable client endpoint contract"; justfile/ release.yml:462 `--protocol --endpoint-generation`).
  - Download to a temp file named with the pid, then SHA-256 verify via a streaming `verify_sha256` (src/checksum.rs:9-40), delete on mismatch, refuse if the Windows asset has no checksum (update.rs:719-722). On Unix, write-test the install dir first (update.rs:629-639), `chmod 0755`, and atomically `rename` over the running exe (the old inode stays alive, update.rs:690-693).
  - Windows: embed the installer (`include_str!("../distribution/install.ps1")`, update.rs:701), write it to temp, run `powershell -NoProfile -ExecutionPolicy Bypass -File ... -LocalPackagePath ... -LocalPackageSha256 ...` with `PSModulePath` removed (update.rs:756-775).

    The script takes an exclusive file-lock mutex (install.ps1:525-551), re-verifies the digest, expands into `releases\.staging.<name>.<pid>`, runs `herdr.exe --version` from the staged copy, checks the package is complete.

    It then moves any existing dir to `.backup.*`, `Move-DirectoryWithRetry` (retries ~100 ms on IOException/UnauthorizedAccess for antivirus/indexer holds, rolls back the backup on failure; install.ps1:480-496,857-870), then retargets a managed junction `current` -> `releases\<ver>` (refuses to retarget a junction it does not own; :564-601), updates user PATH, and prunes to N retained releases (:639-667).

    The running exe sits in its own version folder and is never overwritten; the next launch resolves through the junction.
  - Integrity is SHA-256 from the same-origin manifest only (no signature): it detects corruption, not a compromised host. Herdr shows the digest in the CI step (`.sha256` assets, preview.yml:244-281) and takes it from the GitHub asset digest (preview.yml:373-376). Do not copy that as the trust model; plan Authenticode or minisign once the signed release pipeline exists.
  - Test seam: `HERDR_FAKE_UPDATE_VERSION` fakes an available update with stub notes (update.rs:33,2350) so the UI flow can be driven without a release. Post-update "what's new" is persisted (`release_notes::save_pending`, update.rs:2164).
- Ours: no auto-update (CLAUDE.md "deferred"). Related primitives we already have: content-addressed binary cache with in-use-aware GC (crates/daemon/src/binary_cache.rs:105-150) and the client retiring an old daemon over `/shutdown` using the protocol range (supervisor.rs). Those cover the daemon-swap half (herdr's whole `live_handoff` machinery is a separate agent's area).
- Change (design input, nothing to build now): install into `%LOCALAPPDATA%\...\releases\<ver>\` with a junction or small launcher stub; staging + `--version` smoke + backup/rollback + retry on Windows file holds; `.sha256` in `latest.json` produced by the release workflow (item 5); install lock so two clients do not race; keep last 2 releases; a fake-update env var for UI tests.
- Value: high later; Cost: L.

### 9. Plugin/installation lease via shared file lock

- Herdr: each managed plugin installation holds a `.in-use` file; a server `lock_shared()`s it for as long as it runs the plugin, and cleanup takes an exclusive try-lock so it cannot delete a checkout something still uses, with no PID bookkeeping (src/plugin_installations.rs:9-59,63-70, `TryLockError`).
- Ours: binary_cache::gc decides in-use from the live process list (binary_cache.rs:105-150, with path normalisation for verbatim / case, tests :333-346). The lease trick would make it crash-safe and race-free where a cached binary is about to be spawned between the process scan and the delete. Probably unnecessary; keep for a plugin / release-dir store later.
- Value: low. Cost: S.

### 10. Structured log vocabulary

- Herdr: a small set of helper fns emit events with stable fields `event="pane.spawn.start" subsystem="pane" outcome="started|ok|error|timeout"` (logging.rs:41-386). Greppable timelines, `outcome != ok` always at warn+.
- Ours: free-form messages with fields (e.g. git.rs:83-90). Fine today; worth doing only if we start grepping logs in e2e assertions (the live harness already keeps failed dirs under .tmp/native-e2e).
- Value: low. Cost: M.

### Where we are ahead (one line each)

- Lints: our workspace `[workspace.lints.clippy]` pedantic + deny on unwrap/panic/print (Cargo.toml:44-46) vs herdr's bare `clippy.toml` (`too-many-arguments-threshold = 11`) with no `[lints]` table and `unwrap()` banned only by convention (AGENTS.md "Code Conventions").
- Test selection: tools/test-map.json `-Reached` and the `serial-live` nextest group; herdr has `just test-one` and per-OS filters only. Single `tests/ui` binary to cut link time.
- Fake-agent tier: fake-claude and fake-tailscale shims through `fake_cli`, GPUI specs against a scripted daemon, `native-shot` screenshots; herdr tests agents with synthetic manifests and live smoke by hand (AGENTS.md "Agent Detection Updates").
- Worktree lifecycle: groups, markers, cross-drive anchors, discard ladder, branch-fate (item 4).
- Git concurrency control: per-repo lock, fail-fast network env, timeouts (git.rs:22-53) and an RAII client-count park of the refresher.
- Persistence safety: atomic temp+rename with owner-only perms (state.rs:419) and secret sealing into Credential Manager; herdr config is plain TOML.
- Update tooling herdr ships that we do not need: package-manager detection (brew, mise, nix) and curl subprocess HTTP.

### Things herdr does that are NOT worth copying

- Notifications: `ToastDelivery::{Terminal,System,Off,Herdr}` forwarded to the host terminal as OSC (src/server/notifications.rs:9-19, src/terminal_notify.rs); our native toast (toast_activation.rs) is the right shape for a GUI client. Sound config (src/sound.rs, src/config/sound.rs) is a TUI nicety.
- `render_prof.rs`: an env-gated (`HERDR_RENDER_PROF`) per-second counter/duration aggregator (render_prof.rs:1-50); useful pattern if we need a render profiler, otherwise skip. Their `bench-render-scale` with 1 vs 15+ panes (AGENTS.md "Multiplicative performance paths") is a good discipline for the native client's pane count scaling.
- Frozen wire fixtures (tests/fixtures/endpoint-*-v1.json, "never update a generation-1 expectation") are right for a released protocol. We are pre-release with a declared no-backcompat rule; revisit after v0.1.0-alpha for the `/proof` and `/shutdown` contract CLAUDE.md calls a contract with every released daemon (I found no frozen fixture test for them; grep only hit src files, so unmeasured whether an in-crate test pins them).

DOC DRIFT
- none found between the docs and code for the areas read. (CLAUDE.md "logs/daemon.log rotated on each daemon start" matches main.rs:704-768; it is accurate but documents the unbounded-in-run behaviour as a feature.)

TASK INDEX GAP
- Not applicable to herdr (no architecture doc). For our docs/architecture.md I did not open the Task Index; a row "log rotation, state.json load/corruption handling, nextest config" -> crates/daemon/src/main.rs (init_tracing), state.rs, .config/nextest.toml would have saved three searches.

MISSING ENTRY POINT
- herdr: no docs/README.md and no docs/ARCHITECTURE.md (docs/ is website content: next/, preview/, versions/); AGENTS.md is the only map, and it carries Task-Index-like guidance inline.
- herdr: no overlay file for read-only explore agents.
