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

### A.1 — Range-based handshake

Replace `protocol_version: u32` in `Hello` / `Welcome` with a range or
set, and negotiate at connect time.

- [ ] Wire shape: `Hello { protocol_versions: Vec<u32> }` (client) →
      `Welcome { protocol_version: u32, supported_versions: Vec<u32> }`
      (daemon picks the highest mutually supported version and echoes it).
      Old peers send/receive `protocol_version: u32` as today; the new
      fields are `#[serde(default)]` so older serialisations decode.
- [ ] Build-time constant `SUPPORTED_PROTOCOL_VERSIONS: &[u32]` in
      `protocol/build.rs`. The current daemon advertises every version it
      can speak; the current client advertises every version it can
      tolerate downgrading to.
- [ ] Daemon-side: store the negotiated version on the `ClientSession`
      and use it to gate features (skip emitting v15-only messages to a
      v14 client; skip parsing v15-only fields from a v14 client).
- [ ] Client-side: expose the negotiated version to `App.tsx` so feature
      flags can dim UI (e.g. greyed "Rearrange panes" with a "requires
      daemon vN+" tooltip when running against an older daemon).
- [ ] Test: protocol crate unit test confirming `Hello` with a stale
      `protocol_version` scalar still decodes (back-compat for old
      clients connecting to new daemons).

### A.2 — Additive-by-default enums

`ClientMessage` and `DaemonMessage` are tagged enums; unknown variants
currently error. We need a graceful fallback.

- [ ] Add a `#[serde(other)]` catch-all variant `Unknown(serde_json::Value)`
      on `ClientMessage` and `DaemonMessage`. The daemon's dispatch
      ignores `Unknown` with a `warn!` log; the client treats `Unknown`
      from the daemon the same way.
- [ ] Same treatment for nested tagged enums where new variants are
      likely (`PresetVariableKind`, `TabLayout`, `RearrangeLayout`,
      `InjectorStep`). Where ignoring isn't safe (e.g. a `TabLayout` we
      can't render), fall back to the closest known variant
      (`BalancedHorizontal`) and log.
- [ ] Decision rule recorded in CLAUDE.md: "Adding a new variant or
      `#[serde(default)]` field is not a protocol bump. Renaming,
      removing, or changing semantics is. When in doubt, ask before
      bumping."

### A.3 — Sidecar version negotiation

`sessions/<id>/meta.json` carries spawn config and runtime metadata. New
fields land via `#[serde(default)]` today, but removals or shape changes
would break older daemons trying to read newer sidecars (and vice versa).

- [ ] Add `on_disk_version: u32` to `OrphanMeta`. Default to the current
      version when missing (back-compat with sidecars written before this
      field existed).
- [ ] Write a `migrate_sidecar(version, json) -> OrphanMeta` shim. Today
      it's a no-op; the seam is what we want, not the migration logic.
- [ ] Refuse to load sidecars with `on_disk_version > MAX_KNOWN`; surface
      as a daemon-startup warning. This protects a downgrade scenario (v15
      daemon writes a sidecar, user rolls back to v14) from corrupting
      live state.

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

### B.1 — Capture continuation context

- [ ] Persist the last `N` lines of each session's `recent_actions` /
      injected prompt in the sidecar (currently only the spawn config is
      retained). Bound to keep sidecars small (1 KB cap per session is
      fine).
- [ ] Persist the most recent successful user input (the prompt that was
      injected at spawn, plus any subsequent injector-driven text). Lets
      a "Resume" action replay that exact prompt against a new clone.

### B.2 — "Killed session" recovery surface

- [ ] On startup, the daemon partitions sidecars into three buckets: live
      (claude PID still alive — keep as today), orphan-stopped (claude
      gone but exited cleanly — already covered by stopped status), and
      **abandoned** (claude gone, last status was non-terminal — daemon
      died mid-session).
- [ ] Abandoned sessions surface in the sidebar under a new "Abandoned"
      heading with a "Resume" button. Clicking it spawns a duplicate from
      the captured spawn config and replays the last known prompt. The
      abandoned sidecar is consumed on resume.
- [ ] Bulk "Resume all abandoned" action at the top of the heading for
      the post-crash case.

### B.3 — Graceful shutdown improvements

- [ ] `ClientMessage::Shutdown` currently kills every session as a side
      effect. Add a `Shutdown { drain: bool }` parameter. `drain: true`
      stops sessions cleanly (today's behaviour); `drain: false` flips
      every session to "abandoned" in its sidecar without killing the
      child — useful for the upgrade case once Phase C makes children
      actually survive. Until Phase C lands, `drain: false` is a no-op
      different from `true` only in that it doesn't fire the cleanup
      worktree-remove path.
- [ ] App-level Quit dialog asks: "Stop all sessions, or leave them for
      next launch?" (the second option is greyed with "requires tracer
      v1+" until Phase C ships).

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

### C.2 — Tracer protocol (separate from main protocol)

Designed to be **far more stable** than the main protocol — every change
here is a forced re-spawn for affected sessions, which we want to avoid.

- [ ] Versioned the same way as the main protocol (range-based handshake)
      but bumped much less aggressively.
- [ ] Smallest possible surface: I/O, resize, status, stop. No git
      operations, no config, no preset machinery — those stay in the
      daemon and don't need to survive its restart.
- [ ] Documented as a public stability contract in `docs/tracer-abi.md`.

### C.3 — Implementation

- [ ] New crate `crates/tracer` with binary `rt-tracer`. Reuses the
      `protocol` crate's PTY size types but defines its own message enum.
- [ ] Daemon spawn path: instead of spawning claude directly via
      `portable-pty`, spawn `rt-tracer.exe` and pass it the claude
      command line + cwd. Tracer does the actual `portable-pty` spawn.
- [ ] Daemon registry: tracer pids and pipe names persisted alongside
      sidecars so the next daemon can find them.
- [ ] Daemon startup: scan, ping each tracer pipe, reconnect to those
      that respond. Tracers whose pipes don't respond are considered
      crashed and their sessions are flipped to abandoned (Phase B
      machinery handles UI).
- [ ] Tracer self-cleanup: on `Stop` or claude exit, tracer drains the
      ring, writes a final scrollback chunk to disk, and exits.
- [ ] Tracer detached from daemon's process group / job object so killing
      the daemon doesn't cascade.
- [ ] Bundle `rt-tracer.exe` in the Tauri installer; ensure the daemon
      can locate it via a deterministic path (next to its own exe).

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
