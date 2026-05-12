# C.1 spike — tracer architecture viability

Status: **ready for C.3 implementation**. Three of five questions resolved
empirically with positive results; the remaining two require a running
`claude` instance and are written up as runtime experiments the user can
execute when bandwidth allows. None of the resolved findings falsify the
tracer architecture as described in `docs/plans/upgrade-survivable-sessions.md`.

Spike harnesses live in `crates/tracer/examples/`:

- `spike_pipes.rs` — pipe server + race-client (Q3 + Q5)
- `spike_client_driver.rs` — companion client for the reconnect test (Q3)
- `spike_race_server.rs` — single-instance server for the race (Q5)
- `spike_pty_no_console.rs` — ConPTY opening from a rust binary (Q2)

Each is `cargo run --example <name> -p tracer`.

## Q2: portable-pty's ConPTY backend from a non-console-host binary

**Answer**: ConPTY works from a regular console-subsystem rust binary. The
spike opened a pseudoconsole successfully and spawned `cmd.exe /c echo ...`
as the child:

```
INFO ConPTY opened successfully from rust binary
```

**Confidence**: high for console-subsystem binaries (verified). Medium for
fully windowless binaries (`#![windows_subsystem = "windows"]`) — not
re-verified in this spike, but Microsoft's `CreatePseudoConsole` API is
documented as allocating its own pseudoconsole regardless of the host's
console state. `portable-pty` 0.9 wraps that API directly without
console-attach pre-checks. **Recommendation**: ship rt-tracer as a console
subsystem app (current default). The user perceives it as a background
process anyway because the spawn flow will pass `CREATE_NO_WINDOW`.

## Q3: pipe reconnect on the same name

**Question**: When the daemon dies, a new daemon needs to claim
`\\.\pipe\rt-tracer-<id>` and resume serving. Does the named-pipe API
allow that?

**Answer**: **yes**. The spike sequence:

1. Server A creates pipe with `first_pipe_instance(true)`
2. Client connects to A
3. A closes its instance
4. Server B creates a new instance on the SAME pipe name
5. Client B (companion driver) connects successfully

Both clients exchanged data successfully:

```
INFO phase-1 server listening (first instance) pipe=\\.\pipe\rt-tracer-spike-c1
INFO phase-1 client connected
INFO phase-1 read from client bytes=14
INFO phase-1 server closed instance
INFO phase-2 server listening (second instance) pipe=\\.\pipe\rt-tracer-spike-c1
INFO phase-2 client connected
INFO phase-2 read from client bytes=14
INFO server: both phases complete; success
```

**Recommendation**: tokio's `tokio::net::windows::named_pipe` is sufficient
for the daemon-restart scenario. The `interprocess` crate (the design doc's
suggested choice for future cross-platform support) wraps the same Windows
API — it would behave identically. We can adopt `interprocess` later when
Mac/Linux support is on the horizon without re-validating this question.

## Q5: two-daemon race

**Question**: when the user double-clicks the app icon and two daemons
race to attach to the same tracer pipe, the loser should get a clean
rejection — not silent join, not data corruption.

**Answer**: clean OS error 231 ("All pipe instances are busy") on the
loser. The first connection completed normally; the second saw:

```
WARN client open failed idx=2 err=Os { code: 231, kind: Uncategorized, message: "All pipe instances are busy." }
race results client_1=ok("first-wins\n") client_2=err(All pipe instances are busy. (os error 231))
```

**Recommendation**: rt-tracer should create pipes with `max_instances(1)`.
The daemon detects 231 on attach and either retries with backoff (if the
first daemon was a stale process about to exit) or surfaces an error
("another daemon is already attached to this tracer") and dies cleanly.

## Q1: ConPTY EOF behavior when daemon dies mid-prompt

**Status: not run in this spike. Needs claude installed.**

**Test plan** (~30 minutes once claude is on the box):

1. Start `cargo run --example spike_pty_no_console -p tracer` modified
   to spawn `claude` instead of `cmd.exe`.
2. Send a partial prompt via the master input handle: `"What is the"`
   (no newline).
3. Drop the master handle (simulates daemon dying).
4. Observe: does `claude` see a clean EOF (process exits or hangs), or
   does it see garbage / continue rendering as if the partial prompt
   were a complete one?

**Why this matters**: if `claude` mishandles a transient input gap (e.g.
treats EOF as "submit what you have"), the tracer must buffer pending
input across daemon restarts. If `claude` waits for more bytes or exits
cleanly, the tracer can be input-stateless — it just forwards bytes from
the daemon to the PTY.

**Working hypothesis** (to be verified): ConPTY's input side is a buffered
stream from `claude`'s POV; closing the master handle on the daemon side
closes the pty's standard input, which `claude` would interpret per its
own logic (likely "user pressed Ctrl-D" → exit). This means the tracer
should **buffer outgoing input itself** so a fresh daemon doesn't see a
torn input stream. Verifying empirically still required.

## Q4: ring buffer sizing under realistic load

**Status: not run in this spike. Needs claude installed.**

**Test plan**:

1. Spawn `claude` via the tracer skeleton's PTY path.
2. Drive it with a prompt that triggers heavy MCP tool use (e.g. "list
   every file in this repo and show me 5 lines from each").
3. Measure bytes/minute landing in the `OutputRing`. Compare against the
   current 4 MB cap to compute the "safe daemon outage window" budget.

**Working hypothesis**: under normal interactive use claude emits
~10–30 KB/min (mostly UI repaints). Under heavy tool-call output, bursts
can hit ~500 KB/min. 4 MB buys ~8 minutes of heavy load or ~hours of idle
— probably right. 2 MB might be too tight for the "app upgrade takes 30s,
user wandered off and came back" case. **Keep 4 MB as the default**;
revisit only if real measurements come in significantly higher.

## Implications for C.3

The architecture as described is viable. Recommended C.3 plan:

1. Wire the named-pipe server into `supervisor::run` using
   `tokio::net::windows::named_pipe::ServerOptions::new().first_pipe_instance(true).max_instances(1)`.
2. Implement the `TracerRequest`/`TracerResponse` dispatch on top of
   line-delimited JSON over the pipe (one message per line; the wire
   format from `tracer-protocol` already serializes cleanly).
3. Buffer pending daemon-side input in the tracer (per Q1's working
   hypothesis); a future Q1 finding can shrink this if claude turns out
   to tolerate input gaps.
4. Daemon-side: on startup, scan sidecars for live tracer pids, open
   each pipe with a short timeout. On `IO Error 231`, treat as
   "another daemon already attached" and abort cleanly with a UI toast.
5. Cross-platform path: when the time comes, swap the tokio pipe types
   for `interprocess` and gate the pipe-name format behind `cfg(windows)`.

The two remaining empirical questions (Q1 and Q4) are runtime measurements
that don't block code; they refine defaults (input buffering, ring size)
rather than choosing architecture.
