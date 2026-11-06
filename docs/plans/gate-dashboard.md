# Gate dashboard in the client

Design for RT-55 ("Gate dashboard in the client"), re-cut by RT-64 around the `gate-state` crate. Step ids are `GD.n`, sub-issues of RT-55. Every command in a step runs through `tools/gate.ps1 run …` (`pwsh -NoProfile -File tools/gate.ps1 run <command>`), one gate call per step.

## What it is

Every build and test in this repo, and in the user's other repos, runs through the gate, which lets only a few heavy and a few light commands run at once across the machine. With many sessions open, a session whose build seems stuck is often just waiting for a slot, and nothing in the client says so.

The client shows the daemon host's gate state: each slot busy, held or free; for a busy slot its command, working folder, log and time held; the gates waiting for a slot; the critical band; the remote and builder pools; and, later, recent runs and the last seven days' top commands. The daemon reads the state with the crates leftos/gate publishes and pushes it to clients in an additive protocol message. ops-tower's Gates tab is the reference to compare against until RT-928 retires it.

## Owner rulings

- Where it shows (RT-55, 2026-10-28): "a rail view plus a footer strip (owner)", not a Dashboard tab. The view is `Activity::Gates` on the activity rail, beside Sessions, Needs You and Source control; the rail item carries a busy-slots badge. It stays hidden only until a gate has ever run on that host (owner, Q7). The daemon reports gate state, idle included, once that host's gate has run at least once, and reports none before that. The client shows the item whenever the host reports gate state. Mock: `docs/plans/canvases/ops-tower-absorb/ot-gates/index.html`, lane A.
- What a slot row shows: two lines. Line 1 is `heavy 0` in muted text, the short command, and right-aligned the time held. Line 2, muted and ellipsized, is the folder label and the last log line. The tooltip carries the full command, the full folder, the resolved log path, the ceiling (`timeoutSeconds`) and the process count.
- Refresh: the daemon polls every 2 s while a client is connected, sends on change and parks at zero clients.
- Waiting gates are listed under their pool with folder, command and time waited.
- Only the daemon host's gates, headed with its `machine` name: a tailnet client sees the host's slots, never its own machine's, and never an aggregate.
- The badge counts busy plus held slots across both pools (every slot taken), in the accent colour, hidden at 0.
- A click on a busy row opens its log in the OS default app, only when the client is on the daemon's machine; the right-click menu copies the command, the folder or the log path. The owner's third choice, a Stop gate item, is dropped (see Decisions).

## Decisions

Orchestrator decisions, not owner rulings:

- **Source.** The daemon takes `gate-state` and `proc-table`, plus `gate-wire` if a part needs it. They are the one reader; the daemon has no gate logic of its own.
- **Wire.** Additive, no protocol bump. The daemon's own typed structs are converted from gate-state's `DashboardState`, which derives `Serialize` only, so the client cannot reuse it and a claim-file change never reaches the protocol. Every field past the ids is `#[serde(default)]` and the new enums carry `#[serde(other)] Unknown`.
- **Slot counts.** `SetGateSlotCount` is local only, like account administration: a tailnet connection is refused. The daemon never caches the counts. The client and ops-tower may both write `slot-counts.json`: each write is gate-state's atomic per-kind read-modify-write, so the last write wins and the file is never torn.
- **No stop button.** gate-state has no stop API, and a stop would bring Windows job-object code back into the daemon. A gate-side `gate stop <pid>` is the better home and gets its own issue.
- **Platform.** Only 64-bit Windows has live inputs. Any other daemon sends `GateState { state: None }` or nothing, does not list `FEATURE_GATE_STATE`, and starts no poll; the rail item never shows.
- **Elevated daemon.** Its `LOCALAPPDATA` and `Local\` namespace may differ from the user's gates; the poll uses whatever `live_inputs` reads, and the GD.4 test notes the limit.
- **Process snapshot.** The reader's table snapshot is also what DB.8 (RT-488) needs to place claims under sessions by ancestry; the reader publishes its pid, parent and created times beside the state so DB.8 does not walk the process table a second time. DB.8 designs the consumer.
- **Dashboard consumers.** The Dashboard takes gate holds under sessions (DB.8, RT-488) and gate ticks in the two-hour tracks (DB.20, RT-732) from the daemon-side state on `Hub`, never from the wire message.

## The dependency

`gate-state` and `proc-table` (and `gate-wire` if needed) are git dependencies on `https://github.com/leftos/gate`, pinned once in the workspace `[workspace.dependencies]` at the rev the accounts crates already used, so one git source is one lock entry and the tree holds one `proc-table`. They are not in ops-tower.

`deny.toml` has `unknown-git = "deny"`, so its `[sources]` section gains `allow-git = ["https://github.com/leftos/gate"]`, or `cargo deny check` fails. A later bump to a newer gate rev is its own change, after a compile check.

## The daemon reader

New `crates/daemon/src/gate_state.rs`, a thin wrapper over the crates.

- `read_once(inputs, table, mutex, cwd)` is generic over the `ProcessTable`, `MutexProbe` and `CwdReader` traits and calls `dashboard_state`, so it compiles and is tested on every platform with `EmptyTable` and fakes.
- The Windows wrapper (`cfg(all(windows, target_pointer_width = "64"))`) composes `live_inputs()`, one `Win32ProcessTable.snapshot()`, `Win32MutexProbe` and `Win32CwdReader`. `live_state()` no longer exists. `StateInputs` picks the gate's generation itself (`Generation::detect`, state under `gate` or `gate\v1`), so the daemon holds no layout code.
- A pure conversion maps `DashboardState` to the wire `GateState`. A claim's raw `Value` is parsed into `GateClaim` with every field defaulted and unknown fields ignored.
- The reader blocks: `dashboard_state` reads claim, ticket and count files and the log tail, and the table snapshot calls ToolHelp and reads each process's PEB. It never sleeps, and runs under `spawn_blocking`.
- `apply_slot_count` blocks longer: it retries its write up to ten times with a 100 ms sleep, about 0.9 s worst case. It runs under `spawn_blocking`, never on the dispatch loop.
- The history read (`live_history(&mut JournalCache, processes, repo_of)`) does file I/O: hold the cache in one `Mutex`, run it under `spawn_blocking`, and pass `repo_of` from the daemon's registry (worktree folder to repo; ops-tower's rule maps `<repo>.wt\<name>` to `<repo>`). A journal read error shows no history and never takes the slots down. The daemon never calls the journal writers.
- Send-on-change compares the converted state with `now` and the age fields zeroed, since the client computes time held from `started` and `arrived`.

## Wire messages

In `crates/protocol/src/lib.rs`; the features string is `FEATURE_GATE_STATE = "gate_state"`, listed in `Welcome.features` only by a daemon with the reader enabled. Timestamps are `DateTime<Utc>`.

- `DaemonMessage::GateState { state: Option<GateState> }`, sent at initial state from the hub's watch and on change. `None` means no gate state.
- `GateState { machine, processors, now, critical, queue, pools, remote, builders, covered }`.
- `GatePool { kind, count, default, set, problem, slots, waiting }`, with `GateRemotePool { count, problem, slots }` and `GateBuilderPool { id, name, host, os, count, problem, routed, slots }`.
- `GateSlot { index, name, state, claim, stale, processes, last_line, critical, extra }`, with `GateClaim { pid, started, cwd, command, log, timeout_seconds }` for `claim` and `stale`.
- `GateQueueTicket { kind, holds, remote, pool, pinned, pid, arrived, age_seconds, cwd, command_line, reason, expected }`, `GateWaiting { pid, since, cwd, command_line }`, and `GateCoveredRun` for `covered`.
- Enums `GateSlotKind` (heavy, light) and `GateSlotState` (free, busy, held), each with `#[serde(other)] Unknown`.
- `ClientMessage::SetGateSlotCount { kind, count: Option<u32>, request_id }`: `None` clears to the formula. The daemon validates with `SlotCount::new` (1 to 64), runs `apply_slot_count` off the runtime, replies with the `request_id` echo per the existing reply convention in `docs/wire-protocol.md`, and repolls at once so the push carries the new count. Local only.
- `ClientMessage::GateHistory { request_id }` and `DaemonMessage::GateHistory { request_id, runs, open, summary }`, a separate pull: sent when the view opens and every 15 s while it is visible, carrying recent runs (including the `Parent` run state and `parent_pid`) and one summary row per repo and command over seven days.
- No log message: "open log" reuses `file_tail` (`FEATURE_FILE_TAIL`) or the default-app opener.

## Client model and views

- `apps/native/src/gates.rs`, plain Rust: `RootView.gate_state: Option<GateState>`, set by the `GateState` arm of `RootView::on_message` and cleared on disconnect. `busy_count` is the badge (busy plus held). `rows` yields per pool, heavy first, a header ("HEAVY · 2 of 3 busy") and the slots in index order with extra slots last; queue tickets and waiting gates come oldest first. `short_command` is the program's file name without folder or extension, then the rest. `folder_label` is the last two path components, the full path in the tooltip. Time held uses `needs_you::waited` from `started`.
- `Activity::Gates` joins `sidebar.rs`'s enum before `Sessions`, which stays the `#[default] #[serde(other)]` last variant, so an older build reading `"activity": "gates"` shows Sessions. `activity_bar.rs` gains an item after Source control with an icon in `assets.rs`, left out when `gate_state` is `None`. When the state is `None` while `Gates` is the stored activity, the panel shows Sessions and the stored value is kept.
- `gates_view.rs`: a header "GATES · <machine>", the critical block, each pool with its count stepper (1 to 64, posting `SetGateSlotCount`) and two-line rows, the queue, the remote and builder pools, and the log-open and copy menu. A free slot is one muted line, with the stale claim's pid when stale. A repaint task runs every second while the panel is visible and a busy slot is under a minute old, else every 30 s.
- The footer strip is part of GD.7. History (recent runs, last seven days) is GD.8.

## Steps

Order: GD.1, GD.2, GD.3, GD.4, GD.5. GD.6 needs GD.2 and runs beside GD.3 to GD.5 against a scripted daemon. GD.7 needs GD.5 and GD.6, GD.8 needs GD.7, GD.9 is last. GD.2 and GD.4 edit `crates/protocol/src/lib.rs`, the daemon's dispatch and `Cargo.lock`, so they run after the accounts items that edit the same files have merged to `main` (RT-924).

- [x] **GD.1 Dependency** (RT-995). `gate-state` and `proc-table` are pinned once in the workspace `[workspace.dependencies]`, at the leftos/gate rev the accounts crates already used, so the tree holds one `proc-table`; the daemon, `agent-claude` and `claude-accounts` take them with `workspace = true`. `crates/daemon/src/gate_state.rs` holds only the smoke test `dashboard_state_reads_two_empty_pools`, and reaches the crate as `::gate_state`, since the module shares its name.

  That rev's `gate-state` and `gate-router` raise the lock's floors for `regex`, `flate2` and `toml`; a later rev bump may raise them again, so it runs `cargo update -p regex -p flate2 -p toml` (or the crates its error names) in the same change.
- [x] **GD.2 Protocol** (RT-996). The wire types and messages above are in `crates/protocol/src/lib.rs` (`GateState` boxed in its `DaemonMessage` variant), with `GateSlotCountSet` answering `SetGateSlotCount`, the `gate_write_failed` code, and `GateHistory`'s reply carrying `runs`, `open` and `summary`. The daemon answers both requests `unsupported_message` from an interim `dispatch` arm, which GD.5 and GD.8 replace.
- [x] **GD.3 Reader and conversion** (RT-997). `crates/daemon/src/gate_state.rs` has `to_wire`, `read_once`, the 64-bit Windows `read_live` and `same_state`. `read_once` reports no state where `<local app data>\gate` does not exist, since no gate has ever run there, and an idle gate with both pools where it does.
- [x] **GD.4 Daemon wiring** (RT-998). The hub holds the current gate state in a `watch` (`Hub.gate_state`, a `GateHub`), written by the 2 s poll (`gate_state::spawn_poll`) from an injected `GateReader` closure run on `spawn_blocking`, which parks at zero clients and broadcasts `StateEvent::GateState` only on a change.
  It is enabled exactly on 64-bit Windows, elevated or not; such a daemon lists `FEATURE_GATE_STATE` and sends the current state at initial state and after a lagged state stream.
- [x] **GD.5 Slot-count write** (RT-999). `server/gate.rs` answers `SetGateSlotCount` from a task of its own: it looks up the counts file through `GateHub`'s counts-path seam (the live inputs' `slot_count_path` in the daemon, a temp file in tests) and runs `apply_slot_count` on a blocking thread, then replies `GateSlotCountSet` and signals the poll's `repoll`, so the poll stays the only publisher and the next push carries the new count.
  A count outside 1 to 64 or an unknown kind is `invalid_request`, a failed write or a counts file that cannot be found is `gate_write_failed`, a daemon without gate state answers `unsupported_message`, each echoing the `request_id`; `host_only_refusal` refuses a tailnet connection with `local_only`.
- [x] **GD.6 Client model and rail item** (RT-1000). `RootView.gate_state` holds This PC's state only (another host's is ignored), cleared on This PC's `Welcome` and when its connection closes. `RootView::shown_activity` reads a stored Gates as Sessions while there is no state. The Gates panel draws, for now, only the `GATES · <machine>` header and one line per pool from `gates::rows`; GD.7 replaces it with the view.
- [x] **GD.7 Gates view and footer strip** (RT-1001). `gates_view.rs` draws the panel under a `GATES · <machine>` header with the core count: per local pool a header with its busy and waiting counts and the 1 to 64 stepper that sends `SetGateSlotCount` (a refusal shows under the pool), two-line slot rows and the gates waiting for it; then the critical block, the remote and builder pools and the queue.

  A click on a busy row opens its log, its right-click menu copies the command, folder or log path, and the footer's `Gates <taken>/<total>` pill shows the panel. Unlike lane A, a row's second line is the folder label and last log line rather than the repo and session, the time is the time held with no usual (GD.8 added the usual), and the header keeps the other panels' 26 px height.
- [x] **GD.8 History.** Part A (RT-1047) landed: the daemon's reply; parts B (RT-1048) and C (RT-1049) are the client. Files: `crates/daemon/src/server/gate.rs`, `crates/daemon/src/gate_state.rs` (`Mutex<JournalCache>`, `repo_of`), `apps/native/src/gates.rs`, `apps/native/src/gates_view.rs`, the UI spec. Recent runs and the last seven days, pulled every 15 s while the view is open. Proof: a daemon test over a small copied journal fixture (`RunState::Parent` handled; a journal read error shows no history and the slots stay up) and a UI spec; `tools/gate.ps1 run cargo nextest run -p daemon --cargo-profile test-release gate` and `… -p rustling-tulip-native --cargo-profile test-release --test ui ui_gates`. Depends on GD.7.

  Part B (RT-1048) landed: the Gates view draws Recent runs (last 2 h) and Last 7 days (passed runs, foldable) from `GateHistory`, pulled when the view opens and every 15 s, one request at a time.

  Part C (RT-1049) landed: usual times on slot, waiting and queue rows; the footer's failure tail for a run that failed in the last 10 minutes; history fetched every 60 s while the view is hidden.
- [ ] **GD.9 Docs.** Files: `docs/architecture.md` (the daemon component line and a Task Index row for the gate view, over files that exist by then), `docs/native-client.md` (a Gates section beside Needs You), `docs/wire-protocol.md` if GD.2 left it, `docs/README.md` (glossary: gate slot, claim file, busy, held, free and stale slot, waiting gate, Gates view, `GD.n` in the step-id entry), `docs/on-disk.md` (the `activity` values), `CHANGELOG.md`, `docs/plans/native-client-parity.md` if it gains a line. Then delete this file and regenerate `docs/plans/MAIN.md` from Linear. Proof: `rg "gate-dashboard" docs README.md CLAUDE.md` finds no pointer at this file. Depends on GD.8.

## Retired

The daemon does not keep its own reader; gate-state is the one reader. Gone with it: the `gate_slots.rs` module and its `Probe`, the Windows mutex and job-object probe and its `windows` features, the slot-count formula and environment reading, the claim-file name filter, the `sysinfo` waiting scan, the PowerShell-era layout notes, and the hand-test against the old dashboard script. There is no Stop gate.

## Open questions

- Whether gate-state reads the same `Local\` namespace from an elevated daemon: unmeasured, so GD.4 records the limit.
- Whether newer gate heads after the pinned rev change the API gate-state exposes: unmeasured; take a newer rev only after a compile check, in its own change, which also moves the lock's `regex`, `flate2` and `toml` floors if the new rev raises them.
