# Keep the host awake while sessions are live

## Goal

The host OS must not idle-sleep while rustling-tulip has work running. A
`claude` session that is mid-task (or parked at its prompt waiting for the
user to come back) is silently killed by system sleep on laptops with
aggressive power plans.

## Decisions (settled with the user 2026-09-07)

- **Owner: the daemon, not the window.** The daemon outlives the Tauri window
  while sessions are live and self-exits when idle (`idle_exit.rs`), so it is
  the process whose lifetime matches "the app is running". The hold is held
  while **any session has a live child** — the same predicate the idle-exit
  watcher uses (`blocks_exit`: not abandoned, not `Stopped`/`Error`). Busy vs
  idle status is deliberately *not* consulted: status detection is heuristic
  and a misdetected idle would let the box sleep mid-task.
- **System only.** Prevent idle system sleep; let the display turn off on its
  own timer (`ES_SYSTEM_REQUIRED` without `ES_DISPLAY_REQUIRED` on Windows;
  `caffeinate -i` on macOS).
- **Toggle, default on.** Persisted daemon-side in `state.json`
  (`keep_awake: bool`, `#[serde(default)]` = `true`) next to the existing
  `worktrees_root_override` host setting, so it applies with no window open.
  Surfaced on the Settings → General tab.

## Design

### Daemon — `crates/daemon/src/keep_awake.rs`

- `pub struct Status { enabled: bool, active: bool }` — `active` is true while
  the OS hold is currently engaged (`enabled && any live session`).
- `pub trait Inhibitor: Send { fn set(&mut self, hold: bool); }` — the OS
  boundary, injectable so the watcher is unit-testable with a recorder.
  - **Windows:** `SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED)`
    to hold, `SetThreadExecutionState(ES_CONTINUOUS)` to release. The flag is
    **per-thread**, so the native inhibitor owns a dedicated `std::thread`
    (`rt-keepawake`) driven over an `mpsc` channel; tokio tasks migrate
    between workers and must not call the API directly. A zero return is
    logged at `warn`, never fatal. Adds the `Win32_System_Power` feature to the
    daemon's existing `windows = "0.61"` dependency — no new crate. (The
    `keepawake` crate was considered and rejected: it pins `windows` 0.62,
    which would compile a second copy of the crate, and it calls the API on
    the caller's thread.)
  - **macOS:** spawn `/usr/bin/caffeinate -i -w <daemon pid>` to hold, kill
    the child to release. `-w` makes caffeinate exit on its own if the daemon
    dies abruptly.
  - **Other:** no-op that logs once at `warn`.
- `pub fn spawn(sessions, enabled_rx: watch::Receiver<bool>, status_tx:
  watch::Sender<Status>, state_events: broadcast::Sender<StateEvent>,
  inhibitor: Box<dyn Inhibitor>)` — a tokio task, structured like
  `idle_exit::run`: on every session event or setting change, re-derive
  `desired = enabled && sessions.snapshots().any(blocks_exit)` from registry
  state (never from event payloads), call `inhibitor.set` only on a transition,
  log the transition at `info`, then publish `Status` to the watch (for the
  initial-state push) and as `StateEvent::KeepAwakeStatus` (for live UI).
- `idle_exit::blocks_exit` becomes `pub(crate)` and is reused as-is.

### Persistence — `crates/daemon/src/state.rs`

- `PersistedState.keep_awake: bool` with `#[serde(default = "default_true")]`.
- `AppState::keep_awake() -> bool`, `AppState::set_keep_awake(bool)`.

### Protocol (additive; no version bump)

- `ClientMessage::SetKeepAwake { enabled: bool }` — persist + re-evaluate.
- `DaemonMessage::KeepAwakeStatus { enabled: bool, active: bool }` — sent in
  the initial-state push after `Welcome` and broadcast on every change.
- `StateEvent::KeepAwakeStatus { enabled, active }` forwarded by
  `client_session` like `LanStatus`.
- `Hub` gains `keep_awake_enabled: Arc<watch::Sender<bool>>` and
  `keep_awake_status: watch::Receiver<Status>`.

### Frontend

- `types.ts` / `api.ts` mirrors; `App.tsx` state `keepAwake: {enabled, active}
  | null` + `keep_awake_status` arm; Settings → General gets a toggle button
  (`data-testid="settings-keep-awake-toggle"`) in the same row style as the
  LAN tab's autostart toggle, with a hint that says whether the hold is
  currently active.

## Checklist

- [x] Daemon: `keep_awake.rs` (trait + watcher + Windows/macOS/other
      inhibitors), `state.rs` field + accessors, protocol variants, `Hub`
      wiring, initial-state push, `SetKeepAwake` handler, `StateEvent`
      forwarding. Unit tests: watcher transitions with a recording inhibitor;
      `state.json` without the field loads as enabled; setter round-trips.
- [x] Frontend: types/api mirrors, `App.tsx` state + dispatch, Settings →
      General toggle. `pnpm typecheck` + oxlint clean;
      `scripts/check_protocol_mirror.py` clean.
- [x] Docs: `CLAUDE.md` on-disk layout note (state.json also carries host
      settings), `docs/plan.md` shipped bullet, move this file to
      `docs/plans/completed/`.

## Verification

- `powercfg /requests` on Windows lists `rustling-tulipd.exe` under SYSTEM
  while a session is live and drops it after the last session stops or the
  toggle is switched off.
- `pmset -g assertions` on macOS shows `PreventUserIdleSystemSleep` from
  `caffeinate` under the same conditions.
