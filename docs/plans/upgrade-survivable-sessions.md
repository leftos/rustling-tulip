# Upgrade-survivable sessions

## Context

Today, upgrading the rustling-tulip app forces users to lose every claude
session the daemon is tracking. Two distinct failure modes are tangled
together:

1. **Protocol mismatch.** Any non-trivial change bumps `PROTOCOL_VERSION`,
   and `Hello` rejects mismatched peers hard. A user who installs a new
   client cannot connect to their still-running older daemon even if every
   message they'd actually exchange is identical.
2. **Process-lifecycle coupling.** Claude child processes are tied to the
   daemon's ConPTY master handle. When the daemon dies (graceful shutdown,
   crash, or kill), the master handle closes, claude's stdin EOFs, and
   claude exits. The sidecar-based orphan recovery (`is_claude_alive` over
   stored PIDs) only catches the rare case where Windows hasn't yet reaped
   the child — in practice it never fires.

Recent iter 56 (protocol 14 → 15 for `RearrangeTab` + `AutoGrid`) made the
trap concrete: the user has a v14 daemon holding live sessions they care
about, but the v15 client they just built refuses to talk to it. Working
around this required a `v14-hold` worktree pinned at the last v14 commit.

The two problems can be solved independently. This plan separates them so
the easier win (problem 1) doesn't get blocked behind the harder one
(problem 2).

## Goals

- **A working upgrade flow that doesn't strand sessions.** A user who
  installs a new app version should, at minimum, be able to keep using
  their already-running daemon until they choose to restart it. At best,
  they should be able to restart the daemon without losing live claude
  processes.
- **Backwards-compatible by default.** Adding a new message, variant, or
  field should not require a protocol bump. Bumps are reserved for actual
  incompatibilities (renamed fields, removed messages, changed semantics).
- **Graceful degradation when restart is unavoidable.** If the user does
  restart the daemon (or the daemon crashes), the recovery experience
  should make resumption obvious: show the killed session's last prompt /
  config, offer one-click duplicate to relaunch into the same state.

## Non-goals

- Live migration of in-flight claude turns. If a daemon restarts in the
  middle of claude generating a response, that response is lost — the
  child either dies or gets reset. We're trying to preserve **session
  identity and continuation context**, not in-flight output.
- Cross-machine resumption. Sessions still live on the machine where they
  spawned.
- Backwards-compatible old client → new daemon for arbitrary version
  spreads. We commit to N-1 compatibility on read (current daemon
  understands previous protocol), not N-5.

## Phase A — Protocol forward-compatibility

Goal: a new app version can talk to an older daemon without forcing the
user to restart anything.

### A.1 — Range-based handshake ✅ shipped

Replace `protocol_version: u32` in `Hello` / `Welcome` with a range or
set, and negotiate at connect time.

- [x] Wire shape: `Hello { protocol_versions: Vec<u32> }` (client) →
      `Welcome { protocol_version: u32, supported_versions: Vec<u32> }`
      (daemon picks the highest mutually supported version and echoes it).
      Old peers send/receive `protocol_version: u32` as today; the new
      fields are `#[serde(default)]` so older serialisations decode.
- [x] Build-time constant `SUPPORTED_PROTOCOL_VERSIONS: &[u32]` in
      `protocol/build.rs`. The current daemon advertises every version it
      can speak; the current client advertises every version it can
      tolerate downgrading to.
- [x] Daemon-side: negotiated version captured at handshake (threading
      through `dispatch`/forwarders deferred until a feature actually
      needs gating).
- [x] Client-side: negotiated version stored on `ConnectionState.open`
      so UI can read it when feature gating is needed.
- [x] Test: protocol crate unit tests `hello_decodes_with_scalar_only`
      and `welcome_decodes_with_scalar_only` confirm back-compat.

### A.2 — Additive-by-default enums ✅ shipped (top-level only)

`ClientMessage` and `DaemonMessage` are tagged enums; unknown variants
currently error. We need a graceful fallback.

- [x] Parse-boundary wrappers `InboundClientMessage` / `InboundDaemonMessage`
      with `Known(...)` and `Unknown { type_tag, raw }` variants. Daemon's
      `recv_loop` and handshake route through `InboundClientMessage::from_json_str`;
      unknown types log + drop instead of crashing.
- [x] TS side: tail-of-switch `logToFile("warn", ...)` in
      `App.tsx::handleMessage` for the symmetric direction.
- [x] Per-enum `Unknown` fallback for nested enums (`RearrangeLayout`,
      `TabLayout`, `PresetVariableKind`, `InjectorStep`). Each carries a
      `#[serde(other)] Unknown` unit variant; the containing message keeps
      decoding when a new variant appears. Match sites handle Unknown
      with a sensible fallback (e.g. `TabLayout::Unknown` →
      `BalancedHorizontal`).
- [x] Decision rule recorded in CLAUDE.md: "Adding a new variant or
      `#[serde(default)]` field is not a protocol bump. Renaming,
      removing, or changing semantics is. When in doubt, ask before
      bumping."

### A.3 — Sidecar version negotiation ✅ shipped

`sessions/<id>/meta.json` carries spawn config and runtime metadata. New
fields land via `#[serde(default)]` today, but removals or shape changes
would break older daemons trying to read newer sidecars (and vice versa).

- [x] Added `on_disk_version: u32` to `OrphanMeta` with serde default
      shim → `CURRENT_SIDECAR_VERSION` (1) when missing.
- [x] `migrate_sidecar(value) -> Result<OrphanMeta>` seam in `orphan.rs`;
      today routes everything to the current shape.
- [x] Refuse to load sidecars with `on_disk_version > MAX_KNOWN_SIDECAR_VERSION`;
      `read_all_metas` logs a warn and skips the entry, protecting the
      downgrade scenario.

### A.4 — Acceptance criteria for Phase A

- [ ] A v15 client connects to the v14 daemon used in this session's
      iter-56 trap. New v15-only features (rearrange, auto-grid presets,
      font-size persistence is already client-only) gracefully degrade or
      dim; everything else works.
- [ ] A v14 client connects to a v15 daemon. The daemon doesn't emit
      v15-only broadcasts to it.
- [ ] No protocol bump for purely additive changes going forward.

## Phase B — Recovery polish (bridge to Phase C)

Even with perfect protocol compatibility, the daemon dies sometimes
(crash, reboot, user-initiated restart). Today the recovery UX is poor:
sessions vanish, and the user reconstructs from memory. Make recovery
explicit and one-click before tackling the much harder problem of keeping
the children alive.

### B.1 — Capture continuation context ✅ shipped (last_prompt only)

- [ ] DEFERRED: `recent_actions` tail in sidecar. Not strictly needed for
      Resume; would be nice for UI display of "what was this session
      doing?"
- [x] `OrphanMeta.last_prompt: Option<String>` captures the user's
      initial prompt at spawn (all three spawn paths thread it through
      `meta_from_record`). The Resume handler replays it.

### B.2 — "Killed session" recovery surface ✅ shipped

- [x] Startup partitioning: `main.rs` keeps both live and dead sidecars
      (previously dead were silently deleted). Live → `insert_orphan`;
      dead → `insert_abandoned` (status=Stopped, is_abandoned=true).
- [x] Abandoned sessions surface with `SessionSnapshot.is_abandoned = true`.
      Sidebar shows the badge + a Resume button + a Dismiss button.
      `ClientMessage::ResumeAbandoned` spawns a fresh session from the
      captured spawn config + last_prompt, then deletes the abandoned
      sidecar. `DiscardAbandoned` is the dismiss path.
- [ ] DEFERRED: "Resume all abandoned" bulk button. Clients can loop over
      abandoned snapshots; UI affordance can land later.

### B.3 — Graceful shutdown improvements ✅ shipped

- [x] `ClientMessage::Shutdown` grows a `drain: bool` field (default
      true). `drain: false` leaves sidecars in place so the next daemon
      start surfaces them in the Abandoned bucket — children still die
      pre-Phase-C, but the recovery context (spawn config + last_prompt)
      is preserved.
- [x] Exit dialog gains an "Abandon & quit" button (between "Stop
      sessions & quit" and "Quit, leave running"), only shown when there
      are active sessions. Wires to `sendShutdown(drain=false)`.

### B.4 — Acceptance criteria for Phase B

- [ ] Daemon kill -9 → restart → sidebar shows every previously-alive
      session under "Abandoned" with its label and last prompt; Resume
      relaunches into the same branch/config and replays the prompt.
- [ ] Graceful Quit → restart shows no abandoned sessions (drained
      cleanly).

## Phase C — Tracer-per-session supervisor

Goal: live claude processes survive daemon restarts.

The fundamental constraint is that ConPTY isn't designed for master-handle
transfer across processes. The cleanest workaround is a tiny supervisor
process per session that owns the pseudoconsole and survives the daemon.

### C.1 — Architecture

```
daemon ──spawn──► rt-tracer.exe ──ConPTY──► claude
   │                   │
   └───── named ───────┘
         pipe
```

- The tracer is a small Rust binary (one source file, no heavy deps). It
  owns the ConPTY master handle and the claude child handle.
- It exposes a per-session named pipe (`\\.\pipe\rt-tracer-<session_id>`)
  speaking a minimal stable protocol: `Input { data_b64 }`,
  `Output { data_b64 }`, `Resize { cols, rows }`, `Status`, `Stop`.
- When the daemon dies, the tracer detects the broken pipe and goes into
  "waiting for daemon" mode. It keeps the ConPTY open, keeps reading
  claude output into a bounded ring buffer (2-4 MB), and waits.
- When a new daemon starts, it scans `sessions/*/tracer.pipe` (or a
  registry file with PIDs), reconnects, replays the buffered output, and
  resumes normal operation.

### C.2 — Tracer protocol (separate from main protocol) ✅ shipped

Designed to be **far more stable** than the main protocol — every change
here is a forced re-spawn for affected sessions, which we want to avoid.

- [x] Versioned the same way as the main protocol (range-based handshake)
      but bumped much less aggressively. New crate `crates/tracer-protocol`
      with `TRACER_VERSION = 1`, `SUPPORTED_TRACER_VERSIONS`, and a
      `negotiate()` helper.
- [x] Smallest possible surface: `TracerRequest { Input, Resize, Status, Stop }`
      and `TracerResponse { Output, Status, Exited, Error }`. No git, no
      config, no preset machinery.
- [x] Documented as a public stability contract in `docs/tracer-abi.md`.

### C.3 — Implementation (DEFERRED to spike-gated iter)

The skeleton (C.2b) shipped: `crates/tracer/` builds a working
`rt-tracer.exe` that spawns a PTY child and reads its output into a
bounded ring. The named-pipe server and daemon-integration changes
below are explicitly gated on the C.1 spike answering the open
questions about ConPTY behavior, console-less binary safety, and pipe
reconnect semantics.

- [x] New crate `crates/tracer` with binary `rt-tracer`. CLI surface,
      PTY spawn, output ring buffer all in place.
- [ ] Daemon spawn path swap (the big change) — gated on spike
- [ ] Daemon registry: tracer pids + pipe names alongside sidecars
- [ ] Daemon startup: scan + ping + reconnect logic
- [ ] Tracer self-cleanup: drain ring → scrollback file on exit
- [ ] Tracer detached from daemon's process group / job object
- [ ] Bundle `rt-tracer.exe` in the Tauri installer

### C.4 — Acceptance criteria for Phase C

- [ ] Daemon kill -9 mid-claude-thinking → claude continues running
      (visible in Task Manager), buffering its output to the tracer ring.
- [ ] New daemon starts → reconnects to the tracer → user sees the
      buffered output replay in the same session UI without re-running
      anything.
- [ ] User-initiated app upgrade (replace exe, restart daemon) does not
      lose any session.
- [ ] Tracer crash (rare) flips the affected session to abandoned and
      surfaces a clear error.

## Open questions / decisions to validate

- [ ] Does ConPTY's input pipe close cleanly when claude is mid-prompt,
      or does it leave garbage in claude's input buffer? Empirical test
      before committing to the tracer architecture — if claude
      mis-handles a transient input gap, the tracer needs to buffer
      pending input as well as output.
- [ ] Is `portable-pty`'s ConPTY backend safe to use from a tracer that
      doesn't have a console of its own? Check whether the tracer needs
      to be a console subsystem app or can run windowless.
- [ ] Named pipes vs Unix sockets via crate `interprocess` — picks for us
      depending on cross-platform plans. We currently only ship Windows
      so named pipes are fine; if Mac/Linux is on the horizon, the IPC
      choice should be cross-platform from day one.
- [ ] Tracer ring buffer size — 2 MB is what the daemon uses for
      scrollback; 4 MB might be safer if the tracer is the last line of
      defence during a long daemon outage.
- [ ] What happens when two daemons race to connect to the same tracer
      (e.g. user double-clicks the app icon)? Probably first-come-first-
      served with a "daemon already attached" rejection for the second.

## Phasing and priority

- **Phase A** is the immediate win — removes the upgrade-blocks-progress
  trap for every release going forward. Estimated 2-3 iters.
- **Phase B** delivers visible recovery UX even before Phase C lands and
  is mostly mechanical work on the existing orphan-recovery code. 1-2
  iters.
- **Phase C** is the heavy lift — a new binary, a new IPC layer, a new
  versioning surface. Worth doing if the user (or future users) routinely
  loses long-running claude sessions to daemon restarts. 4-6 iters of
  focused work plus a beta period to shake out the cross-process edge
  cases.

The recommended order is A → B → C. Phase A can ship independently of B
and C; Phase B builds on Phase A's protocol compat to add new recovery
messages without breaking older daemons; Phase C plugs into Phase B's
abandoned/recovered state machine.
