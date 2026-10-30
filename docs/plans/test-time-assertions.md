# Wall-clock time assertions in the tests: inventory

Convention already in the code (not in any doc): a "hang guard" is >= 120 s and fails as "timed out"/"hung" (tracer/src/hook.rs:588-597 `GUARD`, daemon headless.rs:304 `HANG_GUARD`, e2e_recover.rs:34-38 `HANG_GUARD` 300 s, live.rs:37 `START_TIMEOUT` 300 s). Sites below marked COMPLIANT already follow it.

## Totals (offending sites; "sites" = distinct fix points, helper call-site counts are separate, measured with rg line counts, definition included where noted)

| Crate | Offending fix points | Notes |
|---|---|---|
| crates/tracer | 4 in tests/shutdown.rs (1 test) + 7 in src/hook.rs tests (5 elapsed asserts, 1 short budget test, 1 held-open test) | supervisor.rs tests (watchdog) real-sleep but outcome load-independent: not offending |
| crates/daemon | 12 helpers/sites (see list) driving about 70 call sites | 60 s timeouts listed as borderline (14 sites) |
| crates/tailnet | 1 (cli.rs `slow_cli_returns_timeout`) | |
| crates/daemon-client | 0 (supervisor/lib/autostart/device_tokens tests have no timing) | |
| crates/protocol, tracer-protocol, agents/* | 0 (rg for Instant/elapsed/timeout/sleep/Duration::from: no match; agents `quiet_for: 1500 ms` is a config value asserted by equality, not time) | |
| apps/native tests/ (e2e_live, e2e_remote, ui_quit, support/live.rs) | 1 shared helper pair (2 fns, 45 call sites) + 4 own sites | e2e_recover.rs, live.rs START_TIMEOUT, ui specs using `h.advance` (TestClock): COMPLIANT |
| apps/native src/ #[cfg(test)] | net.rs: 6 helpers (about 70 call sites) + 2 absence windows; folder_picker.rs 2; syntax.rs 1 | `Instant::now()` in 100+ other native unit tests is only a base instant handed to a state machine: COMPLIANT |
| apps/native tests/smoke_window.rs | 0 must-fix, 2 notes (120 s guards, STALL) | |
| crates/agents/claude/fake, tools/e2e/fake-tailscale | 0 (fake-claude `sleep 600` index.mjs:416 and `timeout: 5000` index.mjs:486 are payload data; fake-tailscale has no timing) | |
| .config/nextest.toml, rt.ps1, tools/test-map.json, SKILL.md | no machine-alone / load-critical list, no routing to `--slot critical` / `--alone` exists (only SKILL.md:34 says "no `--slot critical`" for implementer gates; tools/gate.ps1 documents the flags) | see Config |

---------------------------------------------------------------------------------------------------

## A. crates/tracer

Status: done (part A).

### A1. tests/shutdown.rs (test `tracer_exits_after_stop_while_a_grandchild_holds_the_console`, the only test in the file)
All four are shared-by-this-one-test helpers; fix the helper, the test is unchanged.
- :82-94 `connect()`: poll `Stream::connect` every 50 ms, fails when `started.elapsed() >= CONNECT_TIMEOUT` (10 s, :32). Category: deadline poll failing the test. Replace: keep the retry loop but bound it by a hang guard (>= 120 s); or wait on an event: the tracer's pipe appears when it is ready, no seam needed. 
- :142-151 `wait_for_exit()`: polls `try_wait` every 100 ms for EXIT_TIMEOUT 10 s (:34), then the test bails "did not exit within {EXIT_TIMEOUT}" (:184-190). This IS the behaviour pinned (tracer exits after Stop with a grandchild holding the console). Replace: block on the child's exit (`spawn_blocking(|| child.wait())`) under a hang guard (120 s+); the cdb stack capture at :185 then runs on the hang. No product clock involved.
- :177 `tokio::time::timeout(READY_TIMEOUT 15 s, wait_for_ready)`: short failing timeout. Replace: hang guard.
- :240-251 `remove_test_dir()`: retries `remove_dir_all` every 100 ms for CLEANUP_TIMEOUT 5 s and returns Err (fails the test). Replace: retry under a hang guard, or make failure to delete non-fatal (warn) since it is cleanup.
- :225-233 `run_cdb` CDB_TIMEOUT 30 s: only runs after the test already failed, kills cdb; diagnostic, not a pass/fail bound. Leave, or raise to a guard for consistency.

### A2. src/hook.rs `mod tests` (:571+). Constants GUARD/LONG_BUDGET 120 s, HELD_OPEN_BUDGET 600 s (:591-597).
- COMPLIANT in size: `wait_for_consumed` (:1320, 120 s deadline then asserts the count), `recv_timeout(GUARD)` at :735, `LONG_BUDGET` passed as the product budget in every case.
- Form to change (elapsed assert, even at 120 s): `assert!(started.elapsed() < GUARD ...)` at :652-655 (`stops_at_the_first_json_value`), :677 (`missing_daemon_json_drains_stdin_and_returns`), :703 (`failed_connect_returns`), :725-728 (`a_silent_listener_is_no_response`), :1026-1029 (`blocked_stdin_with_an_unusable_prefix_returns_at_once`). Category: elapsed assert (hang guard shaped). Replace: drop the `started`/`elapsed` lines.

  For the cases where the elapsed check is the only proof that the shim did not wait for EOF (`:652`, `:1026`), prove it by event: make `BlockingAfter` (the blocking reader fixture) record that a read past the prefix was attempted (AtomicBool) and assert it stayed false; a run that does block fails by the nextest slow-timeout / hang guard, which is the "hung" failure. `run()` already takes `budget: Duration` (a seam) so no product change.
- `a_silent_listener_is_no_response` (:708-747): product budget `Duration::from_millis(200)` (:722) must elapse on the real clock against a listener that holds the connection open (`silent_daemon` :1174-1188). Category: short real-time product timeout (a loaded machine can exhaust 200 ms before connect/proof, giving `Skipped` instead of `NoResponse`).

  The product reads the deadline from `Instant::now()` and arms OS socket timeouts, so there is no injectable clock and tokio `pause` does not apply (sync std code). Replace: have `silent_daemon` close the connection after reading the head and pass LONG_BUDGET; `Outcome::NoResponse` also covers "closed connection" (hook.rs:43-45). The deadline branch itself would then be untested: OPEN DECISION 3.

### A3. src/supervisor.rs tests (:1086-1101) `watchdog_warns_at_every_deadline_while_stalled`, `watchdog_is_silent_once_shutdown_reaches_exiting`
`watch_shutdown` really sleeps 10 ms and 30 ms (supervisor.rs:589-594, `std::thread::sleep`). The asserted value (list of warnings) is independent of how long the sleep takes: NOT offending. Only burns 40 ms. Optional seam: pass a sleeper closure.

---------------------------------------------------------------------------------------------------

## B. crates/tailnet

Status: done (part A).

- cli.rs:468-486 `slow_cli_returns_timeout`: 200 ms product timeout (`Cli.timeout`) against a stand-in (`ping -n 6` / `sleep 5`) that runs about 5 s. Category: short real-time product timeout; a passing run needs the 200 ms timer to fire before the child exits and, on a loaded host, the spawn to finish.

  Product already uses `tokio::time::timeout(timeout, finish)` (cli.rs:153) so it IS on tokio's clock. Replace: `#[tokio::test(start_paused = true)]`; with paused time the timer auto-advances while the child is pending; needs tokio `test-util` in crates/tailnet dev-dependencies (not verified). No product change.
- cli.rs:459-466 `missing_binary_is_not_installed` has `timeout: 5 s` that is never waited on: COMPLIANT.

---------------------------------------------------------------------------------------------------

## C. crates/daemon (src, all inside `#[cfg(test)]`; no tests/ dir exists in any crate except tracer)

Status: open (part B).

### C0. COMPLIANT, do not touch
- Every `#[tokio::test(start_paused = true)]` test: idle_exit.rs (:364-453, 9 tests, `assert_no_shutdown_for` absence windows are virtual), pty_state.rs (all 63 tokio tests at :1754-3345 are start_paused; the `advance` helper is a virtual sleep), inject.rs (:548-722, :756; the `elapsed` asserts at :560/:574/:771 read the paused clock), discovery.rs (:780-842).

  Also tailnet_listener.rs (about 45 tests; elapsed asserts at :2523-2530, :2731-2741, :4351-4357 are on the paused clock), connection.rs:863-889, tracer_client.rs:902, hooks.rs:321, pairing.rs:1030 (`tokio::time::pause()` then 1 s virtual steps), cli_pair/devices/hook_status (`Instant::now()` only as a base passed to the code under test).
- 120 s and longer guards: headless.rs HANG_GUARD (:304, :365, :436, :447), handshake.rs:616 (`elapsed < 120 s`, hang guard shaped; same note as A2), :796, :846, :874; connection.rs:768, :884; pairing.rs:1254; hooks.rs:536/566; test_support.rs:573; tailnet_listener.rs:2380, :2478, :2777 (`wait_until` local helper), :3993; vscode_watch.rs:535; remote.rs timeouts below are 30 s (borderline, C3).
- Real-clock tailnet tests using `quick_options()` (50 ms intervals, :2765): they wait on observations (`wait_until`/`wait_for_cert_calls` 120 s) and assert counts, never durations.

### C1. Shared helpers (fix the helper, call sites follow)
| Helper | file:line | Bound | Category | Call sites (rg) | Replacement |
|---|---|---|---|---|---|
| `collect` | file_fetch.rs:374-385 | `timeout(10 s, rx.recv())` per message | short failing timeout | 5 (6 matches incl. def) | await `rx.recv()` directly; hang guard 120 s+ |
| inline | file_fetch.rs:469 (`cancel_stops_stream_without_done`) | `timeout(10 s, task)` | short failing timeout | 1 | hang guard |
| `history::test_support::wait_for_entry` | history.rs:1729-1740 | 500 x 10 ms = 5 s then `panic!` | sleep-poll deadline failing | 4 calls (session.rs, recovery.rs, tracer_client.rs, spawn.rs) | await the history write event; there is none public: poll with a hang guard (>= 120 s), or add a `Notify`/broadcast on `history::write` |
| `history::test_support::sidecar_gone` | history.rs:1718-1726 | 6000 x 10 ms = 60 s, returns false | borderline (60 s) | 4 calls (headless.rs x2, session.rs x2) | raise to guard; same event note |
| `server::test_support::wait_until` | test_support.rs:295-303 | 500 x 10 ms = 5 s | deadline poll | 3 calls, all sessions.rs | guard (>= 120 s) |
| `server::test_support::next_message` | test_support.rs:412-417 | `timeout(5 s, rx.recv())` | short failing timeout | 9 external calls (connection.rs 6, sessions.rs 3) + 1 inside `only_update` | direct `recv()` under hang guard |
| `server::test_support::only_update` | test_support.rs:421-434 | after the update, `timeout(100 ms, recv)` must be Err | absence window | 7 calls (sessions.rs) | no time-free proof of absence: drain what the forwarder already queued after a barrier message (send a second known update/ping through the same ordered stream and assert it is the next message); or use a paused clock if the forwarder is tokio-only |
| `sessions.rs::next_updates` | sessions.rs:2323-2345 | `timeout(5 s)` per message + `timeout(100 ms)` absence | both | 2 calls | same as above |
| `keep_awake::tests Harness::next_hold` | keep_awake.rs:390-394 | `recv_timeout(WAIT 5 s)` | short failing timeout | 15 | switch `Recorder` to a tokio unbounded channel and `.recv().await` under hang guard |
| `keep_awake::tests Harness::stays_put` | keep_awake.rs:397-399 | `recv_timeout(QUIET 300 ms)` is_err | absence window | 9 | the watcher (keep_awake.rs:247-285) is purely event-driven (no timer), so run these 8 tests as `#[tokio::test(start_paused = true)]` (current_thread, not `multi_thread` as now at keep_awake.rs `flavor = "multi_thread"` x8) and `tokio::time::sleep(QUIET)` virtual after yielding; the sleep completes only once the watcher is idle, which makes the absence deterministic. No product change |
| keep_awake inline | keep_awake.rs:591, :597 (`status_is_published_on_the_watch_and_the_broadcast`) | `timeout(WAIT 5 s)` x2 | short failing timeout | 2 | hang guard |
| `presets::tests recv_job_update` | presets.rs:2445-2455 | `timeout(1 s, rx.recv())` | short failing timeout | 4 calls (5 matches incl. def) | hang guard |
| `session.rs wait_until_gate_is_shared` | session.rs:2071-2081 | 5 s `Instant` deadline spin | deadline poll | 2 calls (:2049, :2103) | hang guard (it is a spin on `Arc::strong_count`, the event is the count) |
| `tracer_client::tests wait_for_meta_gone` | tracer_client.rs:1019-1026 | 500 x 10 ms = 5 s, silent (no assert; later assert fails) | deadline poll | 1 call | guard |

### C2. Single-test sites
- session.rs:1644-1688 `exit_end_is_recorded_before_stopped_broadcast`: :1671-1676 `timeout(200 ms, next_stopped)` must be Err while a history write lock is held (absence window), then `timeout(5 s)` at :1679.

  Replace the absence: while holding the lock, send a barrier (the lifecycle task reaches the parked write; observable how? not exposed): OPEN DECISION 4; or drop the absence and keep the ordering assertion that already exists (:1682 reads the history entry when `Stopped` is observed, which proves record-before-broadcast). The 5 s becomes a guard.
- session.rs:1705-1736 `an_exited_session_drops_its_pending_input`: `timeout(5 s, next_stopped)` -> guard.
- session.rs:2162-2195 `a_spawn_write_after_the_session_ended_writes_nothing`: `timeout(5 s, poll meta_file 10 ms)` -> guard (or use `sidecar_gone`-style event).
- sessions.rs:1296-1320: `timeout(5 s, out_rx.recv())` -> direct recv under guard.
- sessions.rs:1707-1716: 500 x 10 ms loop then `assert!(load_meta(..).is_err())` (sleep-poll with a 5 s failing deadline) -> use `sidecar_gone` after it is raised to a guard.
- settings.rs: 3 tests with `wait = 5 s` failing timeouts on a broadcast receiver: keep-awake mode test (:587, :608), `set_agent_settings_validates_persists_and_broadcasts` (:740, :761), `set_agent_command_persists_broadcasts_and_a_blank_pair_clears_it` (:791, :804, :819). Replace: `events.recv().await` loops under a hang guard.
- headless.rs:473-493 `kill_is_idempotent_and_safe_after_exit`: `sleep(300 ms)` so the short-lived child has exited before `kill()`. Category: sleep-then-act. Replace: `sidecar_gone(...)` / wait for the exit waiter event (as the sibling tests at :368 and :404 do).
- conversations/pre_create.rs:506-516 `pre_create_that_prints_nothing_records_no_conversation`: the stop future is `sleep(1500 ms)` (absence window: nothing printed in 1.5 s). `run_until(spec, parse, stop)` already takes the stop future as a seam. Replace: stop on an event, e.g. the script prints a non-id line then writes a `done` file, and the stop future waits for that file (like `wait_for_tree` does); delete the sleep.
- pre_create.rs:344/:438-447 `PID_FILE_CEILING` 60 s in `wait_for_tree` (3 call sites: :463, :482, :499), fails via `expect` on the missing file: borderline 60 s -> raise to guard.
- instance_lock.rs:134-165 `acquire_waits_for_holder_released_within_wait`: holder `sleep(300 ms)` before release, `recv_timeout(3 s)`, product `acquire(.., 3 s)`, and `assert!(waited >= 200 ms)` (elapsed assert). Category: elapsed assert + sleep the next step relies on. Product: `acquire(dir, wait)` loops on `Instant::now()` + `std::thread::sleep(RETRY_INTERVAL)` (instance_lock.rs:52-60), no seam.

  Replace: delete the elapsed assert; have the holder set an `AtomicBool released` just before `drop(lock)` and the main thread assert `released.load()` after `acquire` returns Some (proves it waited for the release with no clock); holder release is triggered by a channel the main thread fires from a helper thread started after `acquire` has failed once (needs a retry hook: OPEN DECISION 5), or accept holder release on a channel signalled right after `held_tx`.
- instance_lock.rs:109-120 `second_acquire_returns_none_while_first_is_held`: `acquire(.., 200 ms)` really waits 200 ms then expects None. Category: absence window on the product clock. Replace: `Duration::ZERO` pins "held -> None" with no wait (the retry loop under a non-zero wait is the thing it also exercises: needs the same seam as above).
- inject.rs:743-754 `marker_found_after_arriving_mid_window`: not paused (the neighbouring tests at :548-:756 are); producer `sleep(200 ms)` must land inside the product's `VERIFY_WINDOW` on the real clock. Category: sleep the next step relies on / must-beat window. Replace: `start_paused = true` (the product uses tokio time, like `marker_absent_times_out` at :756). No product change.
- tailnet_listener.rs: three helpers with a 60 s failing timeout (borderline): `hello_reply_from` :2851, `http_get` :2902, the plain-http read :2951 (`hello_reply`/`hello_reply_from`/`http_get` 14 matches incl. defs). `next_remote_status` remote.rs:1269 30 s (6 matches incl. def). session.rs 60 s timeouts at :2364, :2428, :2516, :2567, :2573, :2625, :2656, :2664. If "minutes" means >= 2 min, raise these to 120 s: OPEN DECISION 1.
- Not offending, noted: session.rs:2390 and :2617 spin 1000 us / 100 us between sends in load generators (pacing, no assertion on elapsed).
- Not tests: server/git.rs:908 (500 ms sleep) and lock_finder.rs:90 / windows_impl.rs:205 are production sleeps.

---------------------------------------------------------------------------------------------------

## D. apps/native

Status: done (part A), except the open site below.

- Open for part B: `apps/native/src/ssh.rs` tests use a 30 s `BACKSTOP` as a failing `tokio::time::timeout` (about :457, 7 uses), the same form as the `net.rs` BACKSTOP; not in the inventory above.
- `apps/native/tests/e2e_ssh.rs` is not inventoried either; it dropped its bounds through the shared `Harness::wait_until` and `wait_for_row`, which take no timeout now.

### D1. Shared helper: tests/support/live.rs `Harness::wait_until` (:1016-1034) and `wait_for_row` (:1038-1062)
Both take a caller `timeout: Duration`, spin `run_until_parked` + `thread::sleep(50 ms)` and `assert!` on the deadline. Category: shared helper, short failing timeout. The callers pass:
- e2e_live.rs constants CONNECT 20 s (:30), SPAWN 30 s (:31), ECHO 15 s (:32); e2e_remote.rs CONNECT 20 s (:27), SERVING 60 s (:30), PAIRING 60 s (:32), SPAWN 30 s (:33), ECHO 15 s (:34), FETCH 60 s (:36); ui_quit.rs 10 s x2 (:756, :766).
- Call sites measured with rg line counts: `wait_until(` e2e_live 12, e2e_remote 23, ui_quit 2 (37); `wait_for_row(` e2e_live 6, e2e_remote 2 (8). Total 45 call sites pass a 10-60 s failing bound.
- COMPLIANT callers: e2e_recover.rs (14 `wait_until` calls, all `HANG_GUARD` 300 s).
- Replacement: wait on the observation (the loop already does: it polls the observed state, the sleep is only the poll cadence); remove the short bound by making the helper ignore/raise the argument to a hang guard constant (suggest reuse `HANG_GUARD` 300 s from e2e_recover.rs, hoisted into support/live.rs), drop the `timeout` parameter and the constants in a mechanical edit, keep "timed out after ... waiting for {what}" as the "hung" message.

  The daemon and net threads run on real clocks, so no clock seam applies (live.rs:1013-1015 says so).

### D2. tests/e2e_live.rs own sites
- :277 `std::thread::sleep(Duration::from_secs(4))` in `live_hook_status_reports_working_asking_and_idle` (:207): sleep 4 s, then asserts the newest snapshot is still `Working` ("a quiet screen showing the spinner left Working"). Category: absence window / sleep-then-assert.

  The same rule is pinned with a paused clock in pty_state.rs (`advance(5 s)` tests at :1808-1819 and :2025-2039: verify before deleting). Replace: delete the sleep and the assertion (rely on the paused pty_state tests); if a live check is wanted, use the status stream as barrier: wait for N more status checks, which the daemon does not expose today.
- :427-436 `survivors_under` (used once, :462 in `live_kill_leaves_no_tracer_behind`): polls `processes_under` for `EXIT` 5 s (:34) and returns survivors, then the test asserts empty. Category: short failing deadline. Replace: poll to a hang guard; `EXIT` constant deleted.

### D3. tests/ui/ui_quit.rs `shutdown_drains_the_daemon_then_quits` (was `shutdown_does_not_reconnect_after_close`; :732-779) and its `fake_daemon` (:683-729)
- :693 `set_read_timeout(10 s)` on the daemon socket (failing via expect), :718-725 `deadline = now + 1500 ms` counting later connections (absence window), plus the two 10 s `h.wait_until` (:756, :766; D1).
- Replace: the positive assertions (`drain == Some(true)`, `ensures == 1`, quit requested once) stay. For "no reconnect after the close" there is no event; move the pin to the connection state machine unit test (`apps/native/src/connection.rs` mod tests; `backoff_delay` connection.rs:108) and delete the 1.5 s window, or give the net thread a clock seam: OPEN DECISION 6. The read timeout becomes a guard.
- The sibling `stuck_shutdown_offers_force_quit_after_five_seconds` (:781) uses `h.advance` (TestClock): COMPLIANT.

### D4. tests/e2e_remote.rs
No timing of its own beyond the D1 constants and `place_first_session`, `pair`, `serve_remote`, `wait_not_approved`, `wait_open` helpers (:615-790) that pass them through: fixed by D1.

### D5. tests/smoke_window.rs
- COMPLIANT in kind, 120 s: PAINT_TIMEOUT :68, PANE_TIMEOUT :80, WINDOW_TIMEOUT :100, CONNECT_TIMEOUT :103, KEYS_TIMEOUT :106, PROMPT_TIMEOUT :109, REPLY_TIMEOUT :112, SOCKET_TIMEOUT :115; `wait_for_line(within)` :324 (2 callers, both KEYS_TIMEOUT). All fail with "no X within ...". 120 s is the same size as the daemon guards; e2e_recover uses 300 s. OPEN DECISION 1.
- :125 `STALL` 5 s used as `all_posted.recv_timeout(STALL)` (:998): a cap on how long the UI thread stays frozen, not a pass/fail bound (the thaw thread unblocks on the `posted` send). Replace the cap with a plain `recv()` (the sender is always sent or dropped) to remove the last `recv_timeout`; low value.
- :122 `PROBE_WAIT` 1 s: retry cadence of `probe_attached` (:638-663); failure only after KEYS_TIMEOUT. Leave.
- :77 `SETTLE_GAP` 250 ms: sleep between two captures until two frames match (observation loop). Leave.

### D6. apps/native/src `#[cfg(test)]`
- net.rs tests (cfg(test) at :1105): 
  - `reaches` :1229-1238 (5 s busy-poll; 4 matches incl. def) used by `restart_during_ensure_restarts_the_attempt` (:1262) and the silent-server test at :1311: short failing deadline -> guard.
  - `saw_closed` :1194-1209 (5 s; 2 matches incl. def): same.
  - BACKSTOP 30 s (:1142) behind `seen_until` (:1708-1723, `recv_timeout`), `saw` (:1734-1750, `tokio::time::timeout`), `next_state` (:1764-1780): line counts in net.rs: `seen_until(` 20, `next_hello(` 19, `saw_state(` 23, `saw(` 19, `next_state(` 8 (each incl. its def and some nested uses). Category: shared helper, 30 s failing timeout. Replacement: raise BACKSTOP to a hang guard (>= 120 s); the tests wait on events already.
  - `wait_out_a_reconnect` :1977-1979 = `sleep(backoff_delay(0) * 4)` (2 s), 2 callers (:2088 `not_approved_host_is_not_redialled...` family, :2531), each followed by `assert_eq!(seen.try_recv().ok(), None, "... dialled again")`. Category: absence window. Product: the net thread owns a private current-thread tokio runtime (`run_thread`, net.rs) and backoff comes from `connection::backoff_delay` (BACKOFF_BASE 500 ms, BACKOFF_CAP 10 s, connection.rs:20-21, :108).

    Replace: assert the no-redial rule on the state machine (`connection.rs` unit), delete the two sleeps; or build the net runtime with `start_paused` behind a test seam (auto-advance would also fire WELCOME_TIMEOUT and the TLS waits while real sockets are pending: risky). OPEN DECISION 6.
- folder_picker.rs tests (windows module): `WAIT` 5 s (:301) passed as the must-beat timeout in `an_existing_folder_is_kept` (:327) and `a_missing_folder_is_dropped` (:336): short failing bound -> guard. `a_check_that_outlasts_its_wait_is_dropped` (:342-354): stand-in probe `sleep(500 ms)` vs product `recv_timeout(20 ms)` (:114): race-by-clock (a descheduled test thread past 500 ms returns `Some`).

  Replace: the stuck probe blocks on a channel/`Barrier` the test releases after the assertion; the 20 ms timeout can stay (a longer wait only makes the timeout more certain), but better to pass a tiny timeout and gate the probe, since the product `start_folder_check(start, timeout, probe)` already takes both as parameters (seam exists).
- syntax.rs:450-461 `a_set_cancel_flag_gives_up_with_nothing`: `assert!(started.elapsed() < TIME_LIMIT (2 s), "stops at once")`. Category: elapsed assert. Replace: delete the line; `Err(Uncoloured::Cancelled)` already proves the cancel path is taken before the deadline (SLOW = 120 s is passed as the deadline); if "stops at once" matters, assert on work done (a counter of lines processed, which `highlight` does not expose today: OPEN DECISION 7).
- COMPLIANT: syntax.rs:438-448 `a_passed_deadline_gives_up_with_nothing` (deadline is an argument; `now` is already past), launch_last.rs:454-484 and every `Instant::now()` base in notices/pairing/device_pairing/undo/recover/scrollback_load/quit/spawn_form/spawn_preview/shell_marks/term_view/viewer_tab/branch_fate/copied/diff_tab tests (injected `now`).
- Not assertions: diff_model.rs:769-814 `spike_build_timings`, tests/ui/ui_diff_view.rs:421-465 `spike_draw_timings` are `#[ignore]` print-only benchmarks (no assert); examples/diff_bench.rs, examples/shot.rs are not tests. Candidates for deletion or a CPU/op-count budget.

### D7. tests/ui and tests/support/mod.rs
All 150+ `h.advance(...)` calls go through `TestClock` (support/mod.rs:343-356, `Harness::advance` :1888): COMPLIANT. ui_target_switch.rs:452 passes the real `Instant::now` as `now` to a spec that never waits on it, ui_viewer.rs:1046 sets a file mtime explicitly: neither asserts time.

---------------------------------------------------------------------------------------------------

## E. Config and docs

- `.config/nextest.toml:13` `slow-timeout = { period = "60s" }`: no `terminate-after`, so it only marks SLOW and never fails a test; `serial-live` (:8, :15-17) covers e2e_live, e2e_recover, e2e_remote, smoke_window. Nothing in it lists machine-alone/load-critical tests. A hang guard in nextest terms would be `terminate-after = N` (period x N); e2e_recover's 300 s and START_TIMEOUT 300 s would need N >= 6 (or a per-test override): OPEN DECISION 2.
- `rt.ps1`: gates use `Invoke-Gated -Slot heavy|light` only (lines 553-1023); no `critical` or `--alone` anywhere in rt.ps1. `tools/gate.ps1:35-77` documents `-Slot critical` / `--alone` as launcher pass-through. `tools/test-map.json`: no load-critical entries.

  `.claude/skills/rustling-tulip-nextup/SKILL.md:34`: "No reached tests, no full suite, no load-burn, no `--slot critical`" (implementer gates). So nothing to unwind in routing; the only doc text tied to wall-clock bounds is docs/architecture.md:235 (below).

## DOC DRIFT
- docs/architecture.md:235 (Task Index row "Diagnose a failing or load-sensitive daemon test") teaches reproducing a failure "that only appears under load" by freezing the process "for longer than the bound under test". After the new owner rule no test has a bound to beat; row should say a load-only failure is a wall-clock assertion to remove.

  Code side: the bound-free guards (hook.rs:588-597, headless.rs:433-436). Doc side is stale; owning doc docs/architecture.md (Task Index). Also CLAUDE.md:84 describes the 60 s slow-timeout; still true (report-only), no drift.
- Code comments that restate the old model and should be reworded with the fixes: live.rs:1013-1015, e2e_live.rs:1975-1976 (`net.rs:1975` "the passing run always waits it out"), keep_awake.rs:302-306, session.rs 1640-1643 (no drift, just context).

## TASK INDEX GAP
The answer took many searches beyond the Task Index row.

Missing row: "Remove a wall-clock assertion from a test / add a hang guard" -> `docs/` (the new rule, to be written, with the 120 s / 300 s sizes) -> `.config/nextest.toml` (slow-timeout, terminate-after).

Then the shared helpers: `apps/native/tests/support/live.rs` (`wait_until`, `wait_for_row`), `crates/daemon/src/server/test_support.rs` (`wait_until`, `next_message`, `only_update`), `crates/daemon/src/history.rs` `test_support` (`wait_for_entry`, `sidecar_gone`), `apps/native/src/net.rs` tests (`seen_until`, `saw`, `BACKSTOP`) -> the paused-clock examples (`idle_exit.rs` tests, `pty_state.rs` `advance`) -> the injected-clock harness (`apps/native/tests/support/mod.rs` `TestClock`).

## MISSING ENTRY POINT
- No glossary entry or doc for "hang guard" (used in code comments in tracer/src/hook.rs, daemon/src/headless.rs, e2e_recover.rs, session/keep_awake tests) nor for the "must-beat bound" rule; docs/README.md has no testing-rules section. docs/architecture.md and docs/README.md exist (docs/architecture.md lower-case; Task Index present).

## OPEN DECISIONS
1. Hang-guard size. Code today uses 120 s (hook.rs, headless.rs, daemon tests, smoke_window.rs) and 300 s (e2e_recover.rs, live.rs START_TIMEOUT). 60 s (history `sidecar_gone`, pre_create `PID_FILE_CEILING`, session.rs x8, tailnet_listener x3) and 30 s (remote.rs, net.rs BACKSTOP) are in between. Pick one floor (suggest 120 s for in-process, 300 s for live/smoke) and say whether smoke_window's 120 s stays.
2. Whether nextest enforces the guard (`slow-timeout = { period = "60s", terminate-after = N }`, per-binary overrides for the serial-live group) or each helper does, and whether a test that merely hangs without a helper should be killed by nextest.
3. tracer hook `a_silent_listener_is_no_response`: keep the deadline branch covered (needs an injectable clock in `hook::run`: the std `Instant` deadline and `TcpStream::set_*_timeout` make a fake clock impractical) or cover only the "connection closed" branch and drop the 200 ms budget test.

   Decided (orchestrator, amended): the 200 ms test became a unit test of the proof read with a deadline already spent (`hook::tests::a_proof_answer_read_after_the_deadline_is_no_response`) plus an end-to-end test on `LONG_BUDGET` against a fake daemon that closes after the request head (`a_listener_that_closes_without_answering_is_no_response`).
4. session.rs `exit_end_is_recorded_before_stopped_broadcast` absence: drop the 200 ms "no Stopped yet" window (ordering is still proven by the history read at :1682) or add a test hook that signals "writer parked on the history lock".
5. instance_lock `acquire`: add a retry hook / injectable sleeper to prove "waited for the holder" without time, or reduce the test to the released-flag ordering.
6. apps/native net absence tests (`wait_out_a_reconnect` x2, ui_quit `fake_daemon` 1.5 s window): add a clock seam to the net thread's runtime (and which timers) or move the no-redial rule to `connection.rs` unit tests and delete the integration absence checks.
7. syntax.rs "stops at once": delete the elapsed assert, or add an op counter to `highlight` as a measured-work budget.
8. Whether the `#[ignore]` timing spikes (diff_model.rs, ui_diff_view.rs, examples/diff_bench.rs) are deleted or kept as print-only.
9. Whether the `elapsed < GUARD` hang-guard-shaped asserts in tracer/src/hook.rs (5) and daemon handshake.rs:616 are removed (form forbidden by the rule's literal text) or allowed as guards; recommendation: remove, since an unreturned `run()` already fails as hung.
10. Whether to add a per-test event (Notify/broadcast) in `history::write` and the sidecar delete path, so `wait_for_entry`/`sidecar_gone` stop polling at 10 ms.
