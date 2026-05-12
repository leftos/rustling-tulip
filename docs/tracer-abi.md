# `rt-tracer` ABI — stability contract

`rt-tracer.exe` is a per-session supervisor binary that owns the PTY master
handle and the `claude` child process. The daemon talks to it over a named
pipe using the protocol defined in `crates/tracer-protocol`.

This file documents what changes count as a "bump" and the rollout
expectations for each. **Treat this as a public stability contract** — every
live session is bound to a specific tracer binary version, so the cost of
breaking changes is high.

## Status: Phase C.2 (skeleton)

As of this commit, the tracer protocol crate (`tracer-protocol`) and the
tracer binary skeleton (`rt-tracer`) exist but are not wired into the daemon's
spawn path. The daemon still spawns `claude` directly via `portable-pty`.

Phase C.3 (a future iter, gated by the C.1 empirical spike) will switch the
daemon's spawn path to invoke `rt-tracer` instead. Until then, this protocol
is forward-only: the daemon won't emit any of these messages in production.

## Message set

### Daemon → tracer (`TracerRequest`)

| variant | purpose |
|---|---|
| `Input { data_b64 }` | bytes to feed into the child's PTY stdin |
| `Resize { cols, rows }` | resize the PTY (xterm `WindowSize`) |
| `Status` | one-shot health probe; reply via `TracerResponse::Status` |
| `Stop` | shut down: kill child, drain ring, exit |

### Tracer → daemon (`TracerResponse`)

| variant | purpose |
|---|---|
| `Output { data_b64 }` | PTY output chunk; replayed on (re)connect |
| `Status { child_pid, child_alive, ring_bytes }` | reply to `Status` |
| `Exited { code }` | child process exited |
| `Error { message }` | async error not tied to a specific request |

### Handshake

The daemon sends `TracerHello { version, supported }` as the first message after
the pipe is connected. The tracer replies `TracerWelcome { version, supported }`
echoing the negotiated version. Negotiation is `max(common)` of advertised
sets. An empty intersection closes the pipe.

## What counts as a bump

**NOT a bump** (additive, ride on `#[serde(default)]` + `Unknown` wrapper):

- A new `TracerRequest` variant (older tracers see `InboundTracerRequest::Unknown` and log + drop)
- A new `TracerResponse` variant (older daemons see `InboundTracerResponse::Unknown`)
- A new `#[serde(default)]` field on an existing message
- New enum variants on nested types

**IS a bump** (requires `TRACER_VERSION += 1` and adding the previous
version to `SUPPORTED_TRACER_VERSIONS` for one release cycle):

- Renaming a field (existing peers can't find it under the new name)
- Removing a variant (existing peers may emit it)
- Changing semantics of an existing field (e.g. switching `code` from "exit
  status" to "signal number")
- Anything that requires both sides to upgrade in lockstep

## Rollout

Each release ships a single `rt-tracer.exe` bundled alongside `rustling-tulipd`.
The daemon spawns tracers from `<exe_dir>/rt-tracer.exe` (resolved via
`std::env::current_exe`). Live tracers from the previous release stay tied
to their old binary — they don't get hot-upgraded.

When the user installs a new daemon, the new daemon scans the sidecar
registry for live tracer pids and reconnects via `\\.\pipe\rt-tracer-<id>`.
Tracers that don't respond within a short timeout are considered crashed and
their sessions are flipped to abandoned (Phase B.2 path).

## Cross-platform plans

Today: Windows-only via named pipes (`\\.\pipe\rt-tracer-<id>`). The
`pipe_name(session_id)` helper centralizes the format.

When Linux/macOS support is on the horizon, swap to Unix domain sockets via
the `interprocess` crate. The protocol itself is platform-agnostic — only
the transport changes. The pipe-name helper would gain a target-os cfg gate.

## Spike (C.1) — required before C.3

Before the daemon's spawn path is rewritten, five empirical questions need
answers (per `docs/plans/upgrade-survivable-sessions.md`):

1. Does the ConPTY input pipe close cleanly when `claude` is mid-prompt, or
   does the child see garbage? Determines whether the tracer needs to buffer
   pending input across daemon reconnects.
2. Is `portable-pty`'s ConPTY backend usable from a windowless rust binary?
   The tracer is invoked as a background process without an attached console.
3. Does `interprocess::os::windows::named_pipe` support the reconnect
   scenario (server closes, new server claims the same name)?
4. Empirical ring buffer size: how much output does claude emit per minute
   under heavy MCP-tool-call load? Pick 2 MB vs 4 MB after measurement.
5. Two-daemon race: when the user double-clicks the app icon and two daemons
   try to connect to the same tracer pipe, what's the expected outcome?
   Probably first-come-first-served with a clear rejection for the second.

Each of these questions is small enough to answer with a single-day
prototype. The aggregate gates whether the tracer architecture as described
is viable. If any answer comes back "this doesn't work on Windows ConPTY",
the design pivots.
