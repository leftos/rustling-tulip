# Performance monitor

A Task Manager-like view of how much CPU and memory each pane uses, with its whole process tree, beside the machine's total CPU and memory. Mock: [canvases/perf-monitor/index.html](canvases/perf-monitor/index.html) (lane A is the rail view, B the full tab, C the footer pill; [shot.png](canvases/perf-monitor/shot.png) is its render).

## Settled

- **Placement:** the rail view (lane A) and the footer pill (lane C) ship first; the full Performance tab (lane B) follows as its own item.
- **Hosts:** every connected host. Each daemon samples its own machine and its own sessions' process trees and sends them to its clients; the view groups by host the way the sidebar does. A client samples nothing itself.
- **Tree:** one group per pane (session), whose row adds up its whole process tree: the tracer, the shell, the agent and everything it started (builds, compilers, MCP servers). A group for the app's own processes (client and daemon), and one "Other processes" row for the rest of the machine, so each column's total equals the machine-wide meter.
- **Units:** CPU as a percentage of the whole machine, as Task Manager shows it; memory as each process's private working set (on macOS, its footprint).

## Steps

- [ ] Daemon: sample the machine (CPU, memory used and total) and each live session's process tree on a fixed interval while at least one client is subscribed, and nothing while none is; send it as one additive `DaemonMessage`, with a `ClientMessage` to subscribe and unsubscribe. The tracer's process-tree code (`crates/tracer/src/proc_tree.rs`, `job_object.rs`) is where a session's processes are already found.
- [ ] Client: the rail view (lane A): meters for CPU and memory with this app's share, the tree sorted by CPU with Name, CPU and Memory, rows expand and collapse, a pane row selects its pane, PID and command line on hover.
- [ ] Client: the footer pill (lane C) showing the focused host's CPU and memory, amber above 80 % CPU or 90 % memory, its hover listing the busiest panes, its click opening the rail view.
- [ ] Client: the full tab (lane B): PID, a last-minute sparkline, threads, command line, a filter, By pane / By tab / Flat, Go to pane, and End process tree behind a confirmation.

## Open questions

- The sampling interval (Task Manager's default is 1 s) and how many samples the sparkline keeps.
- Whether End process tree on a remote host is allowed, and under which rule.
