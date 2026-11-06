# Gate log tail

Clicking a gate opens its log inside rustling-tulip, following it as the gate writes it, instead of handing it to the OS default app (`open_gate_log` in `apps/native/src/gates_view.rs` today). Mock: [canvases/gate-log-tail/index.html](canvases/gate-log-tail/index.html), rendered as [shot.png](canvases/gate-log-tail/shot.png).

## Settled

- **Where:** a log tab (mock lane A), named after the gate's command and placed beside the session tabs. Clicking the same gate again focuses its tab. The tab has the Pop out that panes have, to watch the log in its own window.
- **Following:** the tab shows the last 2,000 lines, with a "Showing the last 2,000 lines of N" note when it cuts, kept scrolled to the end like `tail -f`. Scrolling up pauses following, and Follow resumes it. A log over the file viewer's 2 MiB limit still opens, since only its tail is read.
- **Header:** the command, the slot, the gate's live state and elapsed time, Follow, Copy path and Open in default app (today's behaviour, kept as a button).
- **When the gate ends:** the tab stays open, following stops at the end, the chip turns Passed or Failed with the duration, and a failed run gets a result banner (mock lane B's banner).
- **Rows that open a log:** a running slot, a queued run once it has a log, and a recent run.
- **Hosts:** every host's gates, through that host's daemon, the way Chat's Tail boxes reach a remote host.

## Security boundary

The existing `ClientMessage::TailFile` follows only files inside a session's own folders, so a client, possibly on another machine, cannot read an arbitrary path. A gate log can be anywhere the gate's `--log` named. The gate follow therefore names a gate, not a path: the client sends the gate's run id, and the daemon resolves the log path from its own gate state (the claim's `log`, `apps/native/src/gates.rs` reads it from the same source), refusing a run it does not know. A client never supplies the path. Its reads, its bytes-first-then-new-lines stream and its cap on concurrent follows match `TailFile`'s.

## Steps

- [ ] Daemon and protocol: `TailGateLog { run_id, request_id }` and `UntailGateLog { run_id }`, answered by the same `FileTail`-shaped stream (or a `GateLogTail` message carrying it), under a new `Welcome.features` entry. The path comes from the daemon's gate state only. Tests: a known run streams its log's tail then new lines; an unknown run is refused; a run whose log does not exist yet is followed once it appears.
- [ ] Client: the log tab (lane A), with the 2,000-line tail, follow and pause, the header, the end-of-run chip and banner, and focusing an already-open tab. UI specs for opening, following, pausing on scroll, resuming and a failed run's banner.
- [ ] Client: Pop out for the log tab, as panes have.
- [ ] Gate rows open the tab, and Open in default app moves to the header. Remote hosts' gate rows use their host's daemon.
