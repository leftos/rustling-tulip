# Performance monitor

A Task Manager-like view of the machine's CPU and memory, for the machines the user chooses to track (owner ruling, RT-1092: no per-session process trees for now). Mock: [canvases/perf-monitor/index.html](canvases/perf-monitor/index.html) (lane A is the rail view, B the full tab, C the footer pill; [shot.png](canvases/perf-monitor/shot.png) is its render).

## Settled

- **Placement:** the rail view (lane A) and the footer pill (lane C) ship first; the full Performance tab (lane B) follows as its own item.
- **Hosts:** every connected host. Each daemon samples its own machine and sends it to its clients; the pills show the hosts the user tracks. A client samples nothing itself. A daemon has a source on Windows and macOS only; elsewhere it does not list `perf_monitor`.
- **Tree:** per-pane process trees (one group per session, the app's own processes, an "Other processes" row) are the later step below, not part of the machine sample.
- **Units:** CPU as a percentage of the whole machine, as Task Manager shows it (Windows: `GetSystemTimes`; macOS: `host_statistics` ticks); memory as the machine's memory in use, with the total (Windows: total physical minus available; macOS: sysinfo's used memory).

## Steps

- [ ] Daemon, step 1 (owner ruling: only the overall machine CPU and RAM usage): sample the machine (CPU, memory used and total) on a fixed interval while at least one client is subscribed, and nothing while none is; send it as one additive `DaemonMessage`, with a `ClientMessage` to subscribe and unsubscribe (`WatchPerf`, `Perf`). The wire, the sampling service and the OS sources (Windows, macOS) are in.
- [ ] Daemon, a later step: each live session's process tree, sampled only while the performance dashboard's pane is visible (owner ruling on RT-1089 and RT-1597). Nothing finds a session's processes yet: the tracer's `proc_tree.rs` only kills a tree on Unix and `job_object.rs` is a Windows kill-on-close job, so this step writes the tree walk.
- [ ] Client: the rail view (lane A): meters for CPU and memory with this app's share, the tree sorted by CPU with Name, CPU and Memory, rows expand and collapse, a pane row selects its pane, PID and command line on hover.
- [ ] Client: the footer pill (lane C) showing the CPU and memory of the machines the user chooses (chosen by a right-click on the pills), amber above 80 % CPU or 90 % memory, its hover listing the busiest panes, its click opening the rail view.
- [ ] Client: the full tab (lane B): PID, a last-minute sparkline, threads, command line, a filter, By pane / By tab / Flat, Go to pane, and End process tree behind a confirmation.

## Open questions

- The sampling interval (Task Manager's default is 1 s) and how many samples the sparkline keeps.
- Whether End process tree on a remote host is allowed, and under which rule.
