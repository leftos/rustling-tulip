# Gate dashboard in the client

Design for the "Gate dashboard in the client" line in [MAIN.md](./MAIN.md) (Backlog and singles). The source it reads is the user-level gate, `~/.claude/tools/gate/gate.ps1` (this repo's `tools/gate.ps1` is a launcher for it), and the localhost page it replaces for this purpose is `~/.claude/tools/gate/gate-dashboard.ps1`. Step ids are `GD.n`.

## Problem

Every build and test in this repo, and in the user's other repos, runs through the gate, which lets only a few heavy and a few light commands run at once across the machine. With many sessions open, a session whose build seems stuck is often just waiting for a slot, and nothing in the client says so: the only view of the slots is `gate-dashboard.ps1`, a separate PowerShell process serving a page on `http://localhost:8765/` that has to be started by hand and kept in a browser tab. The client already sits beside every session; it should show which slots are busy, held or free, what each busy slot runs, and which gates are waiting, without another process.

## Settled

- A view on the native client's activity rail, beside Sessions, Needs You and Source control.
- Its rail item carries a busy-slots badge.
- The item is hidden when no gate state exists.
- The daemon reads the slot mutexes and claim files itself (no PowerShell, no `gate-dashboard.ps1` process) and pushes the state to clients in a new additive protocol message.
- The view shows what `gate-dashboard.ps1` shows: each slot busy, held or free; for a busy slot its command, working folder, log and time held; and the gates waiting for a slot.

## What the gate leaves on the machine

Quoted from `gate.ps1` (its help text and `Enter-Slot`, `Get-ClaimPath`, `Write-SlotClaim`, `Exit-Slot`) and `gate-dashboard.ps1`:

- **Slot mutexes.** Heavy slots are the named mutexes `Local\gate-slot-0` to `Local\gate-slot-<n-1>`, n being `GATE_HEAVY_SLOTS` or, unset, `max(1, floor((logical processors - 1) / 4))`. Light slots are `Local\gate-light-slot-0` to `Local\gate-light-slot-<m-1>`, m being `GATE_LIGHT_SLOTS` or `max(1, floor((logical processors - 1) / 2))` (`$slotPools`: `Prefix = 'gate-slot-'`, `Threads = 4`; `Prefix = 'gate-light-slot-'`, `Threads = 2`). A gate holds its mutex for its whole run; a waiting gate opens each mutex only for the instant it tries it (`WaitOne(0)`), then sleeps 2 s. A killed gate abandons its mutex, and the next gate takes it.
- **Claim files.** A gate holding a slot writes `$env:LOCALAPPDATA\gate\slots\<mutex name without Local\>.json` (for example `%LOCALAPPDATA%\gate\slots\gate-light-slot-1.json`), written to `<path>.<pid>.tmp` and moved into place, compact JSON with the fields `kind` (`heavy`/`light`), `index`, `pid`, `processStart` (the gate's process start time, UTC ISO 8601), `started` (when it took the slot), `cwd` (the caller's location), `command` (the command line, each word quoted as the gate passes it), `log` (the `-Log` value as given, so often relative to `cwd`, e.g. `.tmp/test.log`) and `timeoutSeconds` (a number). It deletes the file before releasing the mutex. A killed gate leaves its file behind; the next gate on that slot overwrites it. Writing the file is best effort, so a slot can be held with no claim.
- **Self-test slots** carry `GATE_TEST_SLOT_PREFIX` in front of the names (`Local\<prefix>gate-slot-<i>`); the dashboard skips any claim whose base name does not match `^gate-(light-)?slot-\d+$`.
- **Gate jobs.** Each gate's command runs in a job named `Local\gate-job-<the gate's pid>`, which holds the command's whole process tree.
- **Slot states** (`gate-dashboard.ps1`): *busy* when a claim names a live gate (its `pid` is running and that process's start time is within 2 s of `processStart`); *held* when the mutex is open but there is no live claim (a gate copy older than claim files); *free* otherwise, with a claim whose gate is gone shown as *stale*. A claim for an index past the pool's count shows as an *extra* slot.
- **Waiting gates** have no claim. The dashboard looks for them only when every non-extra slot of a kind is taken ("A gate takes a free slot within 2 s, so with one free no gate of the kind is waiting"): processes whose command line matches `gate\.ps1\S*\s.*-Slot\s+<kind>\b`, that are not a busy slot's `pid`, and that have no child other than `conhost.exe` (a nested gate, run with `GATE_SLOT_HELD=1`, has a child and does not count). Their folder is read from the process's PEB.
- **`-Once`** prints `{ now, machine, processors, pools: [ { kind, count, slots: [ { index, name, state, claim, stale, processes, lastLine, extra } ], waiting: [ { pid, since, cwd, commandLine } ] } ] }`, where `processes` counts the busy gate's descendants and `lastLine` is the last non-empty line of the log's final 4 KB, read with `ReadWrite, Delete` sharing.
- The gate runs on Windows only (`gate: this gate runs on Windows only`, exit 2).

## Design

### Daemon: the reader (`crates/daemon/src/gate_slots.rs`)

Plain Rust with the OS behind a trait, so the whole state machine is tested against a temp folder and a fake probe.

```rust
pub struct Config {
    pub claim_dir: PathBuf,          // %LOCALAPPDATA%\gate\slots
    pub heavy_count: u32,
    pub light_count: u32,
}

pub trait Probe {
    /// Whether a mutex of this name (without `Local\`) exists.
    fn mutex_open(&self, name: &str) -> bool;
    /// A live process's start time (seconds since the epoch), or None.
    fn start_time(&self, pid: u32) -> Option<u64>;
    /// How many processes the gate's job `Local\gate-job-<pid>` holds, or None.
    fn job_processes(&self, gate_pid: u32) -> Option<u32>;
    /// Processes that could be waiting gates: pid, start time, command line, cwd, child names.
    fn gate_processes(&self) -> Vec<ProcessInfo>;
}

pub fn read_claims(dir: &Path) -> BTreeMap<String, ClaimRead>;          // Ok(Claim) | Unreadable(String)
pub fn slot_count(threads: u32, set: Option<&str>, processors: u32) -> u32;
pub fn read_state(config: &Config, probe: &dyn Probe) -> Option<protocol::GateState>;
pub fn last_log_line(log: &str, cwd: &str) -> Option<String>;
```

- `read_claims` lists `*.json` in `claim_dir`, keeps names matching `^gate-(light-)?slot-\d+$` (so self-test claims and `.tmp` files are skipped), and parses each into a `Claim` with serde (`#[serde(rename_all = "camelCase")]`, every field `#[serde(default)]`, unknown fields ignored). A file that fails to parse becomes `Unreadable(reason)` and its slot shows the reason, as the dashboard does.
- `slot_count` is the gate's formula: the variable when set to a whole number above 0, else `max(1, (processors - 1) / threads)`. The variable is read from the daemon's environment, then from `HKCU\Environment` through `user_env::user_scope_value` (the daemon started from the login `Run` entry may predate a variable the user set later); Open question 8.
- `read_state` returns `None` when `claim_dir` does not exist and no slot mutex of either pool is open: that is "no gate state", which hides the rail item (Open question 7). Otherwise, for each pool, the slot indexes `0..count` plus any index a claim names past `count` (`extra: true`), each classified busy / held / free / stale exactly as the dashboard does. A busy slot gets `processes` from `job_processes` and `last_line` from `last_log_line`. When no non-extra slot of a pool is free, the pool's `waiting` list comes from `gate_processes` with the dashboard's regex, holder and child rules.
- `last_log_line` resolves a relative `log` against the claim's `cwd` (the gate reads a relative `-Log` from the caller's working directory), opens it with read, write and delete sharing so the gate is never locked out, reads the last 4 KB and returns the last non-empty line, trimmed to 300 characters. A read failure returns `(could not read the log: <reason>)`, as the dashboard does.

### Daemon: the Windows probe

In the same module, `#[cfg(windows)] mod windows_probe`:

- `mutex_open`: `OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, "Local\\<name>")` from `windows::Win32::System::Threading`, closing the handle at once. Opening does not take the mutex. `ERROR_FILE_NOT_FOUND` means absent; `ERROR_ACCESS_DENIED` (an elevated holder) counts as open; any other error is logged once at `warn!` and counts as absent. The `Local\` namespace is per logon session, and the daemon runs in the user's session (started by the client or by `rustling-tulipd --detach` from the `Run` entry), so it sees the same names the gates create.
- `job_processes`: `OpenJobObjectW(JOB_OBJECT_QUERY, false, "Local\\gate-job-<pid>")` and `QueryInformationJobObject(JobObjectBasicAccountingInformation)`'s `ActiveProcesses`. This counts the gate's whole tree, including processes whose parent has exited, which the dashboard's parent-chain walk misses, and costs two calls instead of a process-table walk.
- `start_time` and `gate_processes`: `sysinfo` (already a daemon dependency, used by `orphan.rs`). `start_time` refreshes only the claim pids; `gate_processes` refreshes the whole table with command lines and cwd (`ProcessRefreshKind::new().with_cmd(..).with_cwd(..)`) and runs only when a pool is full. Whether `sysinfo` 0.32 reads another user process's cwd on Windows is unmeasured; GD.3 tests it, and a `None` cwd shows the row without a folder.

No new crate. The daemon's existing `windows = "0.61"` dependency gains the `Win32_System_Threading` and `Win32_System_JobObjects` features (the tracer already builds with both, so they add no new code to the lockfile).

### Daemon: polling and the push

- `gate_slots::start(hub pieces)` spawns one tokio task at hub start (`start_hub_tasks`, `server.rs`), Windows only. It holds a `tokio::sync::watch::Sender<Option<GateState>>`, whose receiver lives on `Hub` as `gate_slots`, like `keep_awake_status`.
- It polls every 2 s (the dashboard's page interval and the gate's own retry interval) through `tokio::task::spawn_blocking`, since the probe does file and process I/O. After each read it compares with the last state and, only on a change, sends it on the watch and broadcasts `StateEvent::GateSlots(state)`. Polling rather than a `notify` watcher on the claim folder: a killed gate leaves its claim behind, a pid dies, and a mutex opens or closes without any file changing, so a watcher would still need the poll (Open question 3).
- It parks while `Hub.client_count` is 0, as the git refresher does, and reads once on the next connect before resuming.
- `send_initial_state` sends `DaemonMessage::GateSlots { state }` from the watch's current value, so a freshly connected client has the state without waiting for a change; the per-connection state-event loop maps `StateEvent::GateSlots` to the same message.
- Time held is not in the message: the client computes it from `started`, so the state changes only when a slot, a claim, a process count, a waiting gate or a last log line changes. A slot running a build whose log grows changes every tick; the message stays small (a few hundred bytes a slot).
- On any platform but Windows the task is not started, the watch stays `None`, and the rail item never shows. The gate itself runs only on Windows.

### Protocol (additive, no bump)

In `crates/protocol/src/lib.rs`:

```rust
/// The daemon host's gate slots. Sent at initial state and broadcast when they change.
/// `None`: no gate state on the host (no claim folder and no slot mutex open), or a
/// platform the gate does not run on.
DaemonMessage::GateSlots { state: Option<GateState> }

pub struct GateState { pub machine: String, pub processors: u32, pub pools: Vec<GatePool> }
pub struct GatePool { pub kind: GateSlotKind, pub count: u32, pub slots: Vec<GateSlot>, pub waiting: Vec<WaitingGate> }
pub struct GateSlot {
    pub index: u32, pub state: GateSlotState, pub extra: bool,
    pub claim: Option<GateClaim>, pub stale: Option<GateClaim>, pub unreadable: Option<String>,
    pub processes: Option<u32>, pub last_line: Option<String>,
}
pub struct GateClaim {
    pub pid: u32, pub started: DateTime<Utc>, pub cwd: String, pub command: String,
    pub log: String, pub timeout_seconds: f64,
}
pub struct WaitingGate { pub pid: u32, pub since: DateTime<Utc>, pub cwd: Option<String>, pub command_line: String }
pub enum GateSlotKind { Heavy, Light, #[serde(other)] Unknown }
pub enum GateSlotState { Busy, Held, Free, #[serde(other)] Unknown }
```

- Every struct field past the ids is `#[serde(default)]`, and the two enums carry `#[serde(other)] Unknown` from day one, so a later field or state decodes in an older client.
- A new `DaemonMessage` variant is additive: an older native client and the installed Tauri app (protocol 22) receive it as `InboundDaemonMessage::Unknown` and log it. No v22 message changes, `v22_compat` stays green, and `supported` does not change.
- The daemon's own copy of the claim (`gate_slots::Claim`, camelCase, matching the file) is separate from the wire `GateClaim` (snake_case), so a change to the gate's file format never touches the protocol.

### Client model (`apps/native/src/gates.rs`)

Plain Rust, no GPUI:

- `RootView.gate_state: Option<GateState>`, set by the `DaemonMessage::GateSlots` arm in `RootView::on_message` (`lib.rs`) and cleared to `None` on disconnect, where `set_keep_awake(None)` is cleared today.
- `fn busy_count(state: &GateState) -> usize`: the badge number (Open question 6).
- `fn rows(state: &GateState) -> Vec<PoolRows>`: per pool, heavy first, a header ("HEAVY · 2 of 3 busy"), then one `SlotRow { index, state, command, folder, held_for_since, last_line, processes, log }` per slot in index order, extra slots last, and the waiting gates after the slots, oldest first.
- `fn short_command(command: &str) -> String`: the program's file name without its folder or extension, then the rest (`C:\Users\…\cargo.exe test -p daemon` reads `cargo test -p daemon`).
- `fn folder_label(cwd: &str) -> String`: the last two path components (`rustling-tulip\apps`), the full path in the tooltip.
- Time held uses `needs_you::waited` (the same "12s", "4m", "1h 5m" format).

### Client view (`apps/native/src/gates_view.rs`) and rail item

- `Activity::Gates` joins `sidebar.rs`'s enum before `Sessions`, which stays the `#[default] #[serde(other)]` last variant, so an older build reading `"activity": "gates"` from `native-ui.json` shows Sessions.
- `activity_bar.rs` gains a fourth `Item` after Source control, with a gauge or stacked-bars icon in `assets.rs` drawn to the rail's 18 px stroke, `badge: busy_count`, the accent colour and the noun "busy" ("Gates (2 busy)"). The rail builds its items as a list, so the Gates item is left out when `gate_state` is `None`.
- `sidebar_view.rs`'s `main_row` gains `Activity::Gates => self.gates_view(width, cx)`; when the state is `None` while `Gates` is the stored activity, the panel shows Sessions and the stored value is kept, so the view returns when the state does.
- The panel: a header row in the panels' header style reading "GATES · <machine>", then each pool's header and rows (Open question 1). A free slot is one muted line ("light 2 · free", "· stale claim from pid 1234" when stale). An empty `waiting` list shows nothing.
- A repaint task like Needs You's: every 1 s while the panel is visible and a busy slot is under a minute old, else every 30 s, stopped when the panel is folded or switched.

### What it does not do

- It shows only the daemon host's gates: the mutexes are in the host's logon session and the claims in its `%LOCALAPPDATA%`. A LAN client sees the host's slots, named by `machine`, never its own machine's (Open question 5).
- It does not start, stop or reorder gates unless Open question 2 rules otherwise.
- `gate-dashboard.ps1` stays as it is; nothing here changes the gate.

## Steps

Order: GD.1 → GD.2 → GD.3 → GD.4; GD.5 needs GD.1; GD.6 needs GD.4 and GD.5; GD.7 needs GD.6; GD.8 last. `protocol/src/lib.rs` is touched by GD.1 and GD.7 (`StopGate`), `server.rs` by GD.4 and GD.7 (its handler).

- [ ] **Re-decide Q8 against the gate's counts file, before GD.2.** The gate no longer reads `GATE_HEAVY_SLOTS` / `GATE_LIGHT_SLOTS`: every gate re-reads `%LOCALAPPDATA%\gate\slot-counts.json` (optional `heavy` and `light`, whole numbers 1 to 64; a missing key means the formula, a bad value is ignored for its kind with a `gate: ignored …` line) on each try for a slot, and `gate-dashboard.ps1` sets it through `POST /api/slot-counts` and reports `count`, `default`, `set` and `problem` per pool (its `Get-SlotCount` holds the reading rules). Q8's answer (a), the `slot_count` spec (the variable, then `HKCU\Environment`) and GD.6's hand-test (`GATE_LIGHT_SLOTS=1` in two shells) all rest on the removed variables; settle what the daemon reads and update those three places.
- [ ] **GD.1 Protocol.** `DaemonMessage::GateSlots`, `GateState`, `GatePool`, `GateSlot`, `GateClaim`, `WaitingGate`, `GateSlotKind`, `GateSlotState`. File: `crates/protocol/src/lib.rs`. Proof, red first: `gate_slots_round_trips`, `gate_slots_none_round_trips`, `gate_slot_state_unknown_decodes_in_place`, `gate_slot_kind_unknown_decodes_in_place`, `gate_slot_missing_optional_fields_default`, `inbound_daemon_message_without_gate_slots_reads_it_as_unknown` (an `InboundDaemonMessage` built without the variant, or the raw JSON through the wrapper); `cargo test -p protocol` (includes `v22_compat`).
- [ ] **GD.2 The reader.** New `crates/daemon/src/gate_slots.rs` (`Config`, `Probe`, `Claim`, `read_claims`, `slot_count`, `read_state`, `last_log_line`) and its `mod` line in `main.rs`. Proof, red first, on temp folders with fixture claim files and a fake `Probe`: no folder and no open mutex is `None`; a folder with no claims is every slot free; a live claim (pid alive, start time within 2 s) is busy with its fields; a claim whose pid is gone, or whose pid's start time differs by more than 2 s, is free with `stale`; an open mutex without a live claim is held; an unparsable claim shows its reason; a claim past `count` is an extra slot; a `GATE_TEST_SLOT_PREFIX`-style name and a `.tmp` file are skipped; `slot_count` at 1, 8 and 32 processors, with a set value and with junk; waiting gates listed only when every non-extra slot is taken, excluding holders and processes with a non-`conhost.exe` child, and matching `-Slot light` only to the light pool; `last_log_line` with a relative log against `cwd`, a log over 4 KB, a log of blank lines, a missing log, and a log held open for writing by the test. `cargo test -p daemon gate_slots`.
- [ ] **GD.3 The Windows probe.** `windows_probe` in `gate_slots.rs`; the two `windows` features in `crates/daemon/Cargo.toml`. Proof, red first, Windows-only tests: `mutex_probe_sees_a_mutex_the_test_holds` (the test creates `Local\rt-gd-<uuid>-gate-slot-0` with `CreateMutexW` and the probe reports it open, then closed after the handle drops); `job_probe_counts_a_named_jobs_processes` (the test creates a job named `Local\gate-job-<a spawned child's pid>`, assigns a sleeping child, and reads 1); `start_time_matches_sysinfo_for_the_test_process`; `gate_processes_reads_a_childs_command_line_and_cwd` (a spawned `pwsh -NoProfile -Command Start-Sleep 30` in a temp cwd). `cargo test -p daemon gate_slots`; `cargo deny check` for the feature change.
- [ ] **GD.4 Daemon wiring.** `gate_slots::start` (the 2 s poll, `spawn_blocking`, send on change, park at zero clients); `Hub.gate_slots`; `StateEvent::GateSlots`; the arm in the per-connection state loop; `send_initial_state`; the `test_hub` constructor. Files: `crates/daemon/src/gate_slots.rs`, `crates/daemon/src/server.rs`. Proof, red first: a paused-clock test of the poll loop with a scripted fake probe (two identical reads send once, a change sends again, zero clients parks it and a connect reads once); a `test_hub` test that a new connection's initial state carries `GateSlots` from the watch and that a `StateEvent::GateSlots` reaches a connected client. `cargo test -p daemon`.
- [ ] **GD.5 Client model.** New `apps/native/src/gates.rs` (`busy_count`, `rows`, `short_command`, `folder_label`) and its `mod` line. Proof, red first: unit tests for the badge count per Open question 6's answer, the pool order and slot order (extra last, waiting oldest first), a free, held, stale and unreadable row's text, `short_command` on a full path, a `.cmd` shim run through `cmd.exe /d /s /c`, and a quoted argument, and `folder_label` on a drive root, a UNC path and a two-level path. `cargo test -p rustling-tulip-native --lib gates`.
- [ ] **GD.6 Rail item and view.** `Activity::Gates` in `sidebar.rs`; the fourth item, its icon in `assets.rs`, hidden when `gate_state` is `None`; `main_row`'s arm with the Sessions fallback; new `gates_view.rs`; the `on_message` arm and the clear on disconnect in `lib.rs`; the repaint task; `tests/support/mod.rs` builders for a `GateState`. Proof: new `tests/ui/ui_gates.rs` against the scripted fake daemon: no `GateSlots` shows no Gates item; `GateSlots { state: None }` shows none; a state with two busy slots shows the item with badge "2" while Sessions is the panel; clicking it lists both pools with the busy slots' short commands and folders; a later `GateSlots` freeing a slot drops the badge to "1"; `None` after that hides the item and the panel shows Sessions while the stored activity stays `gates`; a disconnect hides it; `"activity": "gates"` round-trips in `native-ui.json` and an unknown value loads as Sessions. Existing `ui_sidebar`, `ui_needs_you` and `ui_source_control` specs stay green. `cargo test -p rustling-tulip-native --test ui ui_gates`; hand-test with two real gates running in this repo and one waiting (`GATE_LIGHT_SLOTS=1` in two shells), checking the rows against `pwsh ~/.claude/tools/gate/gate-dashboard.ps1 -Once`.
- [ ] **GD.7 Row actions (Open question 2: (c)).** A click on a busy row opens its log (the resolved path) through `open.rs`'s default-app opener, off the UI thread, disabled with a tooltip when the client is not on the daemon's machine; the right-click menu copies the command, the folder or the log path, and ends with **Stop gate…**, which opens a confirm naming the command, folder and time held ("Another session may be waiting on this run; it will see the gate end with exit 124."). A confirmed stop sends an additive `ClientMessage::StopGate { pid, request_id }`; the daemon acts only when `pid` is the gate pid of a claim it read on its latest poll with a live, start-time-matching process (anything else is refused with `ActionFailed` "That gate has already ended"), then terminates the job `Local\gate-job-<pid>` (`TerminateJobObject`), which ends the gate and its children, as `gate.ps1 -StopTree` does, and repolls at once. Files: `crates/protocol/src/lib.rs` (`StopGate`), `crates/daemon/src/gate_slots.rs` (`stop_gate` with the claim check, the job termination behind `Probe`), `crates/daemon/src/server.rs` (the handler), `gates_view.rs`, `gates.rs` (the resolved log path). Proof, red first: `stop_gate_round_trips` and `v22_compat` in `-p protocol`; `gate_slots` unit tests that a pid not in the latest claims, a dead pid and a start-time mismatch are refused and a live claimed pid reaches the fake probe's terminate; a Windows-only test that `stop_gate` ends a job the test created with a sleeping child; a `test_hub` test that `StopGate` for an unknown pid replies `ActionFailed` with the `request_id`; `ui_gates.rs` specs that a click hands the resolved path to a fake opener, the menu copies each value, and Stop gate… sends `StopGate` only after the confirm; a unit test that a relative log resolves against `cwd`.
- [ ] **GD.8 Docs.** `docs/architecture.md` (the daemon's components list gains `gate_slots.rs`; a task-index row for the gate view: `gate_slots.rs`, `protocol` `GateSlots`, `gates.rs`, `gates_view.rs`, `activity_bar.rs`); `docs/native-client.md` (a Gates section beside Needs You); CLAUDE.md's `native-ui.json` line if it lists the `activity` values; the README glossary (the terms below and `GD.n` in the step-id entry); `docs/plans/native-client-parity.md` if it gains a line; delete the MAIN.md line and this subplan once promoted.

## Glossary terms this subplan coins

- **Gate slot**: one of the machine-wide named mutexes (`Local\gate-slot-<i>` heavy, `Local\gate-light-slot-<i>` light) a gate holds while its command runs; the count per pool caps how many gated commands run at once.
- **Claim file**: the JSON a gate holding a slot writes to `%LOCALAPPDATA%\gate\slots\<slot name>.json`, naming its pid, command, folder, log and ceiling.
- **Busy / held / free / stale slot**: a slot whose claim names a live gate; one whose mutex is open with no live claim; one with neither; a free slot whose leftover claim names a gate that is gone.
- **Waiting gate**: a gate process that found every slot of its kind taken and retries every 2 s, with no claim yet.
- **Gates view**: the native client's rail view showing the daemon host's gate slots, pushed by the daemon in `GateSlots`.

## Open questions

Answered (user): Q1 (a) two-line rows; Q2 (c) open the log, copy values, and a confirmed Stop gate (GD.7); Q3 (a) a 2 s poll, sent on change; Q4 (a) waiting gates listed under their pool; Q5 (a) only the daemon host's gates, headed with its machine name; Q6 (a) the badge counts busy plus held slots; Q7 (a) hidden only until a gate has ever run on the machine; Q8 (a) slot counts from the daemon's environment, then `HKCUnvironment`, then the gate's formula.

1. **What a slot row shows.**
   - (a) *Recommended.* A busy row has two lines: `heavy 0` in muted text, the short command, and right-aligned the time held; line 2, muted and ellipsized, the folder label and the last log line. The tooltip carries the full command, the full folder, the resolved log path, the ceiling (`timeoutSeconds`) and the process count. Worst case: a long test filter pushes the useful part of the command past the ellipsis, and the user hovers to read it.
   - (b) One line: short command and time held only, everything else in the tooltip. Worst case: two busy `cargo test` rows look the same until hovered, since the folder that tells them apart is hidden.
   - (c) Dashboard parity: three lines adding the process count and a bar of time held against the ceiling. Worst case: with six slots the panel scrolls, and the rows the user wants are below the fold.
2. **Whether the view can open the log or stop a gate.**
   - (a) *Recommended.* A click opens the log in the OS default app (on the daemon's machine only), and the right-click menu copies the command, folder or log path; no stop. Worst case: a user who wants a hung gate gone still opens a shell and runs `gate.ps1 -StopTree <pid>`.
   - (b) Read-only: no click action. Worst case: reading why a build is slow means finding the log path in the tooltip and opening it by hand.
   - (c) (a) plus a Stop item that asks the daemon, after a confirm, to terminate `Local\gate-job-<pid>` and the gate process. Worst case: the user stops another session's build mid-write, that session's agent reads exit 124 as its own timeout, and retries into the same slot.
3. **Refresh cadence.**
   - (a) *Recommended.* Poll every 2 s while a client is connected, broadcast only on change, park at zero clients. Worst case: a slot that frees and is retaken within 2 s never shows free, and the time held jumps back to zero.
   - (b) A `notify` watcher on the claim folder for claim writes and deletes, plus a 10 s poll for dead pids, mutexes and waiting gates. Worst case: a killed gate's slot reads busy for up to 10 s after it died.
   - (c) Poll every 5 s. Worst case: a short light test run (under 5 s) never appears at all.
4. **Whether waiting gates are listed.**
   - (a) *Recommended.* Listed under their pool with folder, command and time waited, found only when every slot of the kind is taken, as the dashboard does. Worst case: a full process-table read with command lines every 2 s while a pool stays full; its cost is unmeasured and GD.3 measures it.
   - (b) Only a count in the pool header ("3 of 3 busy · 2 waiting"), from the same scan. Worst case: the user sees two waiting but not which sessions they belong to.
   - (c) Not listed. Worst case: a session sitting on `gate: waiting for a heavy slot` looks idle in the view, which is the confusion the view exists to end.
5. **Gates of other machines.**
   - (a) *Recommended.* Only the daemon host's gates, headed with its `machine` name; a LAN client sees the host's slots, never its own machine's, and never an aggregate. Worst case: a user on the laptop reads the host's slots as the laptop's, until they read the header.
   - (b) The host's gates, and the daemon does not send `GateSlots` to LAN connections at all. Worst case: a user watching host builds from the laptop has no view of why they wait.
   - (c) Each client also reads its own machine's gates locally and shows both. Worst case: the client grows a second reader and the Windows probe, duplicating the daemon's, for a case no one has asked for.
6. **What the badge counts.**
   - (a) *Recommended.* Busy plus held slots across both pools (every slot taken), in the accent colour, hidden at 0. Worst case: an old gate copy holding a slot without a claim keeps the badge at 1 with no command to show for it.
   - (b) Busy slots only. Worst case: all slots held by old gate copies read as a quiet machine.
   - (c) Taken slots, turning amber (`WARNING`) when any gate waits. Worst case: during a long parallel build session the badge is amber most of the time and stops meaning anything.
7. **When the item is hidden ("no gate state").**
   - (a) *Recommended.* When the claim folder does not exist and no slot mutex is open, which is a machine where no gate has written a claim. Once any gate has run, the item stays, showing free slots when idle. Worst case: on a machine that ran one gate once, the item stays for good.
   - (b) Whenever no slot is busy or held and no gate waits. Worst case: the rail item appears and disappears with every build, and the icons below it shift under the pointer.
   - (c) (a), plus a Settings → General toggle to hide it. Worst case: a setting for one rail item that no other item has.
8. **Where the slot counts come from.**
   - (a) *Recommended.* The daemon's `GATE_HEAVY_SLOTS` / `GATE_LIGHT_SLOTS`, then the same names under `HKCU\Environment`, then the gate's formula; claims past the count still show as extra slots. Worst case: a session that sets its own count for one run shows its slots as extra until it ends.
   - (b) The daemon's environment only, as the dashboard does. Worst case: a daemon started before the user set the variable shows the default count for its whole life.
   - (c) The formula only. Worst case: a user who raised the heavy count sees slots they configured shown as extra.
