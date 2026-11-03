# Rustling-tulip driving conventions

These are the rules for letting an agent drive the running native client.

A driven run is the client built with the off-by-default `driver` Cargo feature and launched with `RUSTLING_TULIP_DRIVER=1`: it opens its window cloaked and never activated (`RUSTLING_TULIP_OFFSCREEN_WINDOW`), against an isolated daemon whose dirs sit under `.tmp`, and answers the **driver endpoint**, a line protocol on the named pipe `rt-driver-<pid>` (`apps/native/src/driver/`, gated by `apps/native/src/driver_gate.rs`).

Agents reach it through the stdio MCP server `rt-driver` (`tools/rt-driver-mcp/`), whose tools launch a session, call an endpoint method, run a batch, take a screenshot and stop.

The endpoint's methods, the pipe, the discovery file and the MCP tools are specified in [native-client.md](native-client.md) ("Driver endpoint", "Driver MCP server", and the OS smoke tier under "Tests"); the design and its steps are in [plans/headless-drive.md](plans/headless-drive.md). This file holds the lessons behind them.

The comment under each heading (`<!-- rule: slug -->`, with `scope: project` when the rule means nothing outside this repo) is the rule's identity for the user-level `conventions-sync` skill, which trades driving lessons with the owner's other projects. A rule that describes something this repo does not do yet says so ("Not met"), and its work is in the plan.

This repo declines the rules about CDP, webviews and browser pages (it has no webview), about Godot's engine and gamepads, about hooks injected per run or shared between servers (its hook is compiled in behind the `driver` feature and switched on by `RUSTLING_TULIP_DRIVER`), about recording video (not built), about real input and about driving a third-party app (rt owns the app and drives it only in process or by posted messages).

## Server plumbing

### Run the server from a copy outside the build folder, and stamp the build in every result
<!-- rule: run-server-from-copy -->

`.mcp.json` registers `tools/rt-driver-mcp/launch.ps1`, which runs the installed copy and never builds: `install.ps1` builds the release exe through the gate and copies it into `%LOCALAPPDATA%\leftos\rustling-tulip\data\driver-mcp\<version>-<hash>\`, naming the current folder in `current.txt`, so an install adds a folder and stops no running server, and with nothing installed `launch.ps1` exits naming `install.ps1`.

A running server locks its own exe, so building the tree under a live session would otherwise fail (in other projects MSBuild's `MSB3027` on every commit whose hook built the solution, and robocopy exit status 8 or more on an install over it).

The client the server drives follows the same rule: `launch` shadow-copies the driven client, daemon and tracer out of the build folder into the session's scratch `bin\`, so a rebuild never meets a running file and a running session keeps the build it started with.

Every build stamps `X.Y.Z+<sha>`, and the server reports it in `initialize`'s `serverInfo`. Not met: the `launch` result does not carry the server's stamp or the client's version from its discovery file, and an install does not list the sessions still on an older version, so a friction report cannot name the build that ran.
Source: `tools/rt-driver-mcp/launch.ps1`, `install.ps1`, `src/launch.rs`, `src/rpc.rs`; other projects' install scripts and their documented failures; MSBuild error MSB3027; robocopy exit codes. Seen: 0 here (seeded from godot-mcp, yaat).

### Keep stdout for protocol frames, and keep the protocol pipe away from child processes
<!-- rule: stdio-protocol-only -->

Nothing reaches the server's stdout but JSON-RPC lines: its log goes to stderr and to `<repo_root>/.tmp/driver/rt-driver-mcp-<pid>.log`, and `launch.ps1` writes its own errors to stderr and runs no build.

A child must not inherit the server's stdin: on Windows a child started while the server blocks reading the MCP pipe can stall until that read returns (an app launch hung until stdin closed in another project). So every child the server starts (the client, the isolated daemon) gets its own stdin, closed at once (`Stdio::null()`), and its stdout and stderr go to null or a log file of its own, never the server's pipes.

A grandchild that keeps a redirected pipe open (a leftover build node in another project) holds the reader open after its parent exits, so a reader stops a few seconds after the process it reads has exited, and says so.
Source: `tools/rt-driver-mcp/src/launch.rs`, `src/main.rs`; a documented stdin hang in another project; Windows docs for PROC_THREAD_ATTRIBUTE_HANDLE_LIST. Seen: 0 here (seeded from godot-mcp, yaat).

### Refuse a bad call by name, and make every refusal name the fix
<!-- rule: refusals-name-the-fix -->

Every tool refuses an argument it does not take, naming it and the arguments it takes (`this tool has no argument 'sesion'; it takes: …`), and a value of the wrong kind names the argument and the kind (`build_dir takes a string, not true`). A tool failure is a result with `isError`, never a JSON-RPC error, so its message reaches the agent.

A refusal carries the next step: an unknown endpoint method lists the methods, an unknown element names the near names in the registry, an unknown pane lists the active tab's panes, a call without a live session lists the sessions, a build folder without the driver build names the build commands, and a control character in `type_text` says to press Enter, Tab or Escape with `key`.

A refused call ran nothing, so the agent fixes the call instead of debugging the app.
Source: `tools/rt-driver-mcp/src/tools.rs`, `src/launch.rs`, `apps/native/src/driver/handlers.rs`; another project's C# MCP SDK error handling, where only an `McpException`'s message reached the client. Seen: 0 here (seeded from godot-mcp, yaat).

### Count every ceiling in load-adjusted time, with a wall-time backstop and a stall kill
<!-- rule: load-adjusted-timeouts -->

Other agents' builds and test runs load this machine, and a fixed wall-clock timeout turns that load into false failures.

Every ceiling the server enforces, caller-set timeouts included, should run on a clock that keeps wall time on an idle machine and advances by the machine's free share while other work loads it (sampled once a second with `GetSystemTimes`, the server's own work subtracted, never below 5%), with a backstop at 5x the value in wall time and a kill for a child that neither logs nor uses CPU for 120 s; the error says which fired, with the load figures.

Durations of an action (a drag's steps) are not ceilings, and a timeout is never a pause: holding for a span of app time needs an app-time wait. Measured in another project: integration lanes ran at 15-35% load-adjusted time against wall on a loaded machine.

Not met: the server's ceilings (the discovery wait, the pipe's reply guard, the busy-pipe retry) and the endpoint's `wait_for` and frame waits are all wall time.
Source: `tools/rt-driver-mcp/src/launch.rs`, `src/pipe.rs`, `apps/native/src/driver/wait.rs`; another project's architecture doc and changelog; Windows docs for GetSystemTimes. Seen: 0 here (seeded from godot-mcp).

### Match every reply to its request, cancel what timed out, then say whether the app is busy or stuck
<!-- rule: cancel-timed-out-requests -->

Every request line carries an id and every reply echoes it, and the server's pipe client drops a reply to another id rather than read it as the answer; a refusal of the line itself comes back under the reserved id 0. A call that fails (a broken pipe, the reply guard, an unreadable line) drops the connection, and the next call opens a fresh one, since the endpoint may still answer the old request.

When a request's time passes, the server should send the endpoint a cancel for it, so a `wait_for` ends at its next poll and the endpoint is free for the next call, and a lost connection should cancel every running request.

Then diagnose the timeout: a short `ping` tells a busy client (it answers) from a stuck UI thread, and a stuck one's error adds the client process's CPU over a short sample, its thread count and the last lines of its log, sampled from the client's own pid. With a debugger attached (`CheckRemoteDebuggerPresent`) a call should fail at once as "most likely paused at a breakpoint".

Not met: there is no cancel method, a timed-out call is not diagnosed, and the debugger check is not built.
Source: `tools/rt-driver-mcp/src/pipe.rs`, `apps/native/src/driver/pipe.rs`; other projects' decision records, changelog fixes and pipe-client tests; Windows docs for CheckRemoteDebuggerPresent. Seen: 0 here (seeded from godot-mcp, yaat).

### Bound every wait on both sides, and return its outcome as data with the last values seen
<!-- rule: waits-bounded-outcome-as-data -->

The endpoint clamps `wait_for`'s timeout to 100..=60000 ms and its interval to 20..=1000 ms, and the server waits for any reply for its reply guard, which is longer than the longest clamped wait, so the transport never cuts a full-length wait short.

A wait that times out is a result, not an error: `wait_for` always answers ok with `{met, waited_ms, last}`, `last` being the final value the predicate read, and a `batch` answers `{passed, steps, failedAt}` rather than an MCP error, so the agent reads `met` and `passed`. Where a timeout must be an error, it carries the last observation, so an agent can tell "never matched" from "matched the wrong value".
Source: `apps/native/src/driver/wait.rs`, `tools/rt-driver-mcp/src/pipe.rs`, `src/tools.rs`; another project's wait and batch tools. Seen: 0 here (seeded from yaat).

### Own the processes you start by handle, and own the app's real process, not its launcher
<!-- rule: own-launched-processes -->

The server keeps a live process handle (`Child`) for each client it launched and the isolated daemon behind it, because Windows reuses pids: it stops only what it started, never anything matched by pid alone. The client is started directly, never through a wrapper, and its discovery file is matched by pid and process creation time, so a recycled pid cannot pass for it. `stop` kills the client's tree and the daemon and removes the session's scratch folder, and stdin closing stops every session.

A launch that ends without an endpoint (an early exit, the discovery deadline, a failed ping) kills what it started and reports the client's exit status with its stderr and `native.log` tail, so a client that died is reported by its exit code, not by the timeout that raced it. A killed process reports exited before it releases its working folder (18-38 ms later, measured in another project), so wait on the handle before touching the folder.

Not met: the server puts nothing in a job object, so a server that crashes leaves its clients and daemons running, and a stop does not report the processes left running, how long the quit took or the exit code.
Source: `tools/rt-driver-mcp/src/launch.rs`, `src/session.rs`, `crates/dev-daemon`; other projects' process-management code and documented exit-code and folder-lock measurements; Windows docs for job objects and TerminateProcess. Seen: 0 here (seeded from godot-mcp, yaat).

### Name every session and keep no default one
<!-- rule: no-default-session -->

One server process serves one client, and several agents in that client share it and cannot be told apart, so a default session one agent set would send another agent's calls, a stop included, to its client. Every `rt-driver` tool takes the session name the caller gave `launch`; a call without a live session's name is refused listing the live ones, and a `launch` under a live name is refused. Calls to one session run one at a time, in order, since the endpoint answers one request at a time.
Source: `tools/rt-driver-mcp/src/tools.rs`, its test `call_without_a_session_is_refused_listing_sessions`; another project's decision record, which replaced a default-session design. Seen: 0 here (seeded from godot-mcp).

### Keep a fixed tool surface, annotate every tool, and give agents the tools at install
<!-- rule: fixed-tool-surface -->

Endpoint methods reach agents through the fixed `call` and `batch` tools rather than one MCP tool each, because an agent spawned with a `tools:` line never sees a tool added after it was configured. Every tool sets all of `readOnlyHint`, `destructiveHint`, `idempotentHint` and `openWorldHint` (false) explicitly, and `tools_list_pins_the_surface_and_annotations` pins them.

Give each tool a class from its annotations (read, drive), and have the install rewrite the `tools:` line of every agent file marked with its classes, since an agent missing a tool falls back to a workaround. Not met: `install.ps1` touches no agent file, and no agent definition lists the `mcp__rt-driver__*` tools.
Source: `tools/rt-driver-mcp/src/tools.rs`, `tests/server.rs`, `install.ps1`; other projects' decision records; the MCP tool annotation hints. Seen: 0 here (seeded from godot-mcp, yaat).

### Keep element ids in one server-lifetime registry, and batch by selector
<!-- rule: one-id-registry -->

Ids go stale between calls (a menu item one call found was gone by the click, in another project), so the endpoint hands out no element ids at all: a view names an element with `.driver_id(|| ...)`, the `driver` feature's registry records its bounds each frame, and every call and every `batch` step addresses it by that name, resolved when the call runs. Should ids ever be added, they come from one registry for the server's lifetime, and a batch still addresses elements by name.
Source: `apps/native/src/driver_id.rs`, `apps/native/src/driver/registry.rs`; another project's element registry, batch tool and driving friction log. Seen: 0 here (seeded from yaat).

### Smoke-test the server over stdio exactly as registered, and drive an unreleased build without installing it
<!-- rule: test-server-over-stdio -->

`stdio_smoke_every_line_is_json_rpc` starts the built server the way `.mcp.json` does, runs `initialize`, `tools/list` and harmless calls, and fails on any stdout line that is not a JSON-RPC frame; `tools_list_pins_the_surface_and_annotations` pins the tool list and every annotation. Neither opens a window or sends input.

An unreleased client build is driven without installing anything: `launch` takes the tree's own build folder (`target/test-release` by default, or `build_dir`), and the live spec `driver_mcp_launch_ping_screenshot_stop` launches a real session, pings it, takes a screenshot and stops it.
Source: `tools/rt-driver-mcp/tests/server.rs`, `src/tools.rs`; other projects' smoke and drive-harness scripts. Seen: 0 here (seeded from godot-mcp, yaat).

### Test against the real app and hook, and check live that the desktop never moved
<!-- rule: test-against-real-host -->

The endpoint is tested in process by the `ui_driver` specs against the real `RootView` over a scripted fake daemon, and on the real exe by the driven smoke specs in `apps/native/tests/smoke_window.rs`, which launch it cloaked against an isolated daemon and drive it through the pipe; posted pointer input reaches only a real window, so only the smoke tier proves it.

Because the driver promises background driving, it has a live check: `rt.ps1 native-drive-check` runs `driven::live_drive_leaves_the_foreground_and_cursor_alone`, which launches the real client, drives it (a click, typing into a pane, a drag, a wheel, a screenshot) and asserts after every step that the desktop's foreground window and cursor position are unchanged and the client's window is still cloaked and not foreground.

Beside it, `smoke_window_has_noactivate_and_sits_below_foreground` checks `WS_EX_NOACTIVATE` and the Z order.

The check reads the input state of a logged-on desktop, so `.gate/desktop.toml` routes it to a builder's, and run on this PC it refuses without `-HandsOff` ("Say hands off" below).

A wall-clock bound in such a test is set by the slow path it rules out, never a small multiple of the fast one: a lone process saw timers about 30 ms late on a 75%-busy machine, and a full parallel suite has shown 2.5 s (measured in another project). Not met: no check minimises a window in front of the client and asserts the client never takes the foreground.
Source: `apps/native/tests/smoke_window.rs`, `apps/native/tests/ui/ui_driver.rs`, `rt.ps1`, `.gate/desktop.toml`; other projects' test suites and live-check scripts; Windows docs for WS_EX_NOACTIVATE. Seen: 0 here (seeded from godot-mcp, yaat).

### Post every handler's UI work with its request's cancellation token
<!-- rule: ui-post-with-request-token -->

Endpoint requests run on gpui's UI thread, queued from the pipe's connection tasks. Work queued for a request whose connection has gone must be withdrawn, not run: otherwise it still reads or changes the UI after the client has gone (a later request waits behind it, and a `wait_for` polls on to its timeout for nobody).

A test of that race freezes it on the queue (queue a request, close its connection, then let the dispatcher run, and assert the work did not run) rather than watching a time window, which passes vacuously or fails spuriously under load.

Not met: the dispatcher loop runs every queued call and only logs that the connection closed before its reply.
Source: `apps/native/src/driver/endpoint.rs`, `apps/native/src/driver/pipe.rs`; another project's in-app hook wait handler and a dispatcher-queue test of a cancelled wait. Seen: 0 here (seeded from yaat).

## Driving an app

### Drive an app you own from inside its process
<!-- rule: drive-owned-app-in-process -->

A survey of desktop-automation MCP servers found none that drives an app from outside its process and fully avoids stealing focus (Windows-MCP, terminator, WinAppDriver, NovaWindows and FlaUI-MCP all bring the window forward or move the cursor); the ones that never do run a hook inside the app. gpui draws its own scene and has no UI Automation tree to read anyway.

So the native client is driven through its driver endpoint, in process: keys go through `Window::dispatch_keystroke`, state comes from `RootView`'s accessors, and pointer input is posted to the client's own window. Outside tools (Windows-MCP, UI Automation) are never used on it.
Source: `apps/native/src/driver/`, `docs/plans/headless-drive.md`; a survey of desktop-automation MCP servers in another project. Seen: 0 here (seeded from yaat).

### Drive the real app, and write down what each lesser route cannot see
<!-- rule: real-app-over-frontend-only -->

Drive the real client against a real isolated daemon (an `rt-driver` session) for anything that touches the daemon, the PTYs or the window procedure. Each lesser route is a different app, and what it cannot see is written down here so a run that "shows nothing" is not mistaken for a bug:

- `native-shot` runs the real window over a fake daemon scripted in `examples/shot.rs`: no daemon, PTY, tracer, git watcher or agent CLI runs, and only the views it fixtures exist.
- The UI specs run headless GPUI (`test-support`): no real window, window procedure, posted input, cloak or `PrintWindow` capture, and text is shaped as fixed 0.6 em glyphs.
- The OS smoke specs without the driver post window messages from outside: no state reads, no chords or modified clicks, and hover-only UI never shows.

In another project a plain browser pointed at the frontend got the SPA's HTML for every API request and counted as remote mode, and that fact reached its docs five months after the route was first documented.
Source: `docs/native-client.md` ("Tests"), `apps/native/examples/shot.rs`, `apps/native/tests/support/mod.rs`; another project's instructions and history on driving routes. Seen: 0 here (seeded from towercab-3d).

### Launch isolated and in the background, start driving when the hook answers, and close what you opened
<!-- rule: launch-isolated-lifecycle -->

A driven client runs against an isolated daemon whose config, binaries and worktrees dirs are absolute and under a `.tmp` folder, so the owner's real sessions, settings and logs stay untouched: a client with default dirs attaches to the user's real daemon and resizes their PTYs, and a daemon whose binaries dir is not under `.tmp` reaps the installed app's tracers.

The driver gate refuses a driven run otherwise (exit 4), and `launch` starts an `IsolatedDaemon` (`crates/dev-daemon`) under `<repo_root>/.tmp/driver/<session>-<server pid>/`, with every registered agent's override pointed at its fake CLI.

The client is ready when its discovery file appears and the endpoint answers `ping`, not when the process starts. The agent starts it itself, in the background, and stops it with `stop` when finished; a client the owner already had running is never driven.

A worktree session passes `repo_root` (or a `build_dir`) explicitly, since the server's defaults resolve against the checkout it was started for, and an agent's shell can reset to the main checkout between calls.
Source: `apps/native/src/driver_gate.rs`, `crates/daemon-client/src/isolation.rs`, `tools/rt-driver-mcp/src/launch.rs`, `src/tools.rs`; other projects' launch tools, instructions and driving friction logs. Seen: 0 here (seeded from towercab-3d, yaat).

### Build before launch, and build before stopping on a restart
<!-- rule: build-before-launch -->

Every launch should run a prep first: build the driven client, daemon and tracer when their output is missing or older than their sources (the newest tracked source time against the output, plus a stamp touched after each green build), through the repo's gate, and refuse the launch on a red build with the parsed compiler errors and the profile built. A restart builds first, then stops, then launches, so a red build leaves the old session running.

Not met: `launch` refuses a build folder missing the driver build, naming the build commands, but never builds, and launches a stale build without saying so.
Source: `tools/rt-driver-mcp/src/launch.rs`; another project's decision record and changelog fixes. Seen: 0 here (seeded from godot-mcp).

### Start the app quiet and out of the user's way, set at window creation
<!-- rule: quiet-by-default -->

A driven run is unfocused, out of sight and silent, and each part is set at window creation, because later is too late: gpui activates any window it shows, so with `RUSTLING_TULIP_OFFSCREEN_WINDOW` the window (and every pop-out) is opened with `show: false, focus: false`, then `offscreen::show_cloaked` gives it `WS_EX_NOACTIVATE`, moves it to the bottom of the Z order, cloaks it with DWM and shows it with `SW_SHOWNOACTIVATE`.

A cloak that fails exits with code 3, so no uncloaked window reaches the desktop.

Only `WS_EX_NOACTIVATE` keeps a shown window from being activated: showing without activating leaves it activatable, so when the window in front of it is minimised Windows hands the foreground to the next top-level window in Z order. A window created by a process that holds the foreground right is created above every window, hence the move to the bottom before the first show.

Sound is off through the injected `SilentSounder`, never by muting the app's own setting, so the app's mute stays testable and nothing leaks into saved settings; toasts are suppressed too.

Keep real input out by the window never receiving it (cloaked, never activated), not by swallowing it in the app.
Source: `apps/native/src/offscreen.rs`, `apps/native/src/lib.rs`; other projects' decision records and window timings measured with a 5 ms poller; Windows docs for WS_EX_NOACTIVATE, SW_SHOWNOACTIVATE and DWM cloaking. Seen: 0 here (seeded from godot-mcp, yaat).

### Say "hands off" before driving a window on the user's desktop
<!-- rule: say-hands-off-first -->

When the app runs on the user's own desktop and is driven with input the user's own mouse and keyboard can disturb, or by a check that takes the desktop's foreground, tell the user "hands off" first: any input from them breaks the run.

A driven `rt-driver` session needs no warning: its window is cloaked and never activated, and input reaches it in process or as messages posted to its own window, which the user's mouse and keyboard cannot reach.

One run does need it: `rt.ps1 native-drive-check` reads the foreground window and cursor of the desktop it runs on, so `.gate/desktop.toml` routes it to a builder's logged-on desktop, and run on this PC it refuses unless `-HandsOff` says the user has stopped using the mouse and keyboard.

Posted input is not fully out of reach: gpui reads modifier state from the real keyboard (`GetKeyState`), so a key the user holds leaks into a posted click or wheel.
Source: `rt.ps1` (`Invoke-NativeDriveCheck`), `.gate/desktop.toml`, `docs/native-client.md` ("Driver endpoint"); another project's instructions and app-check procedure. Seen: 0 here (seeded from towercab-3d).

### Default to posted background input, refuse what it cannot deliver, and post it like a real mouse
<!-- rule: posted-input-hygiene -->

The pointer methods (`click`, `drag`, `move`, `wheel`) post window messages to the client's own hwnd, which leaves the cursor and the foreground alone and reaches the cloaked window. Keys and text go through `dispatch_keystroke` in process instead, so chords work without the real keyboard.

Refuse what posting cannot deliver rather than send it wrong: gpui reads modifiers from `GetKeyState`, not the message, so a pointer request with `modifiers` is `refused` with a hint naming `key` or `action`; `type_text` refuses control characters; and a request is checked whole before anything is posted, so a refusal sends nothing.

Post like a real mouse: each press follows a posted move of its own, since gpui answers a move with `TrackMouseEvent` and gets `WM_MOUSELEAVE` at once because the real cursor is elsewhere, and hover-only UI therefore needs `action` `set_hover`.

A double click is two posted clicks, each a move, a press and a release. Two separate clicks near each other should wait out the double-click time plus 50 ms, or a text box can read them as a double click (seen in another project); not met: nothing spaces separate `click` calls.
Source: `apps/native/src/driver/pointer.rs`, `apps/native/src/driver/handlers.rs`, `docs/native-client.md` ("Driver endpoint", "Tests"); another project's posted-input code and a live incident; Windows docs for TrackMouseEvent, WM_MOUSELEAVE and CS_DBLCLKS. Seen: 0 here (seeded from yaat).

### Take input coordinates in one space, map them in one place, and project world points into it
<!-- rule: one-coordinate-space -->

Element bounds from `find`, every pointer target and `state`'s window size share one space, logical pixels in window coordinates, converted to physical client pixels at the window's scale in one place (`driver/pointer.rs`). A server that clicked in one space while its element list reported another missed small buttons in a letterboxed window in another project.

A screenshot's pixels are physical, so on a scaled monitor a point read off a screenshot is not a pointer target: aim at an element by name, or divide by `state`'s scale. A point outside the window is refused naming the window's size.
Source: `apps/native/src/driver/pointer.rs`, `apps/native/src/driver/registry.rs`, `docs/native-client.md` ("Driver endpoint"); other projects' decision records and a replaced server's misses; Windows docs for per-monitor DPI awareness. Seen: 0 here (seeded from godot-mcp, towercab-3d, yaat).

### Resolve the target and check what the press will hit before pressing anything
<!-- rule: refuse-before-pressing -->

Check an element target before any event: an unknown name is `not_found` with the near names, and an element a clipping ancestor hides is `not_visible`, both before anything is posted.

After the aim, the element the app itself has under that point should be the target or inside it, or the press is not sent and the error names what covers it with both rects (an open menu or a modal over the target counts). Results should say what was really hit (pressed on, released on), the first thing to read when a click seems to do nothing. Never reach a tab, row or menu entry by setting its state directly when the check is of the input a user makes.

Not met: the endpoint does no hit check, and a pointer result reports only the point it aimed at.
Source: `apps/native/src/driver/pointer.rs`, `apps/native/src/driver/registry.rs`; another project's decision record and changelog fixes. Seen: 0 here (seeded from godot-mcp).

### Judge an action by its effect: re-list windows and read values back
<!-- rule: check-action-effects -->

A tool that reported success is not proof the action worked: in another project a modal an action opened went unreported and two save confirmations stacked up unseen, and a set-text call reported success on a box that reverted.

After an input, read the effect back before judging the result: `state` (its `overlay` names an open dialog or menu), `pane_text`, or `find` for the element the action should have shown. A pointer result carrying `"drawn": false` means no frame drew after the input within the endpoint's wait, so what was read may predate it.
Source: `apps/native/src/driver/handlers.rs`, `apps/native/src/driver/endpoint.rs`; another project's driving friction log over three sessions. Seen: 0 here (seeded from yaat).

### Wait on a condition after every action, never a sleep, and batch fixed sequences
<!-- rule: wait-for-conditions -->

A drive is launch, look, act, wait, check, then stop. After each act, wait with `wait_for` on one condition that names the effect: a `state` path equals a value or exists, a pane's text contains a string, or an element is visible; it polls between frames inside the client, and a timeout answers with the last value read.

Wait on the value needed, not "not empty" (a placeholder holds too early): the live drive check waits for the pane's prompt to name the scratch folder before typing, not for any text.

Without such waits, in another project, hitting a given moment depended on tool-call latency and a hand-written runner carried fixed 500 ms, 3000 ms and 8000 ms sleeps; `examples/shot.rs` still waits fixed times. Because timeouts stretch under load, a timeout is never a pause.

A fixed sequence goes through one `batch` call, which stops at the first error and closes the round-trip gap between steps.

Not met: no wait reads the client's log output (a line in `native.log`, checked by the server from a point taken before the call), and none waits for "the UI changed" against a baseline when the effect cannot be named in advance.
Source: `apps/native/src/driver/wait.rs`, `tools/rt-driver-mcp/src/tools.rs`, `apps/native/tests/smoke_window.rs`; other projects' wait and batch tools, a driving friction log and an issue on a clock's start. Seen: 0 here (seeded from godot-mcp, yaat).

### Wait out a heavy load on the signal that it finished
<!-- rule: wait-after-heavy-transition -->

Some transitions leave the client busy or half-loaded after the call that started them returned, and reads in that span describe the old screen. Each one here has a signal to wait on before reading state or sending input:

- A client start: the discovery file and a `ping` answer (`launch` waits for both), then the daemon connection (`native.log` logs `connected to daemon`) and the tab layout, or the first-connect chooser, which a driven run avoids by seeding the client's layout as the smoke specs do.
- A new shell or agent session: its pane exists in `state`, then its prompt shows in `pane_text`, before typing into it.

A measured stall is a fact about one run, not a settle time the app guarantees (about 10 s of dropped input after the window appeared, in another project).
Source: `tools/rt-driver-mcp/src/launch.rs`, `apps/native/tests/smoke_window.rs`, `docs/native-client.md` ("Tests"); other projects' instructions and launch docs. Seen: 0 here (seeded from towercab-3d, yaat).

### Read and set state through the app's own hook, never by scraping text, held keys or UI paths
<!-- rule: hook-over-scraping -->

When a run needs a value the UI only renders, a pose it can reach only by a long key or click path, or setup behind a fragile UI path, that is a missing endpoint method or `action`, not a scripting problem.

The endpoint already reads `state` from `RootView`'s accessors and a pane's visible rows with `pane_text` (replacing the smoke tier's read of scrollback over a second daemon socket), and named `action`s open settings, a shell, the spawn dialog, a tab, a pane or a panel without walking the UI.

In another project an agent read a camera heading by scraping status-bar text and aimed it with timed key presses, slow and imprecise, until the hook got get, set and look-at calls over the store the UI uses.
Source: `apps/native/src/driver/handlers.rs`, `docs/native-client.md` ("Driver endpoint"); driving friction lists in two other projects. Seen: 0 here (seeded from towercab-3d, yaat).

### Force rare conditions through a dev-only control, and know what it replaces
<!-- rule: force-conditions-dev-panel -->

Do not wait for a rare real condition to test what renders it: a dev-only control forces it. Here the fake CLIs are that control: a driven run's agent overrides point at fake-claude (and the remote specs at fake-tailscale and fake-ssh), which play a scripted agent, tailnet or SSH host, and `native-shot`'s fake daemon holds sessions in every status.

Know what a fake replaces: a fake agent CLI exercises the client and daemon around the agent, never the real CLI's output. In another project a weather panel's Apply replaced all weather, clouds included, so a run that applied rain lost the clouds.
Source: `crates/agents/claude/fake/`, `tools/e2e/`, `apps/native/examples/shot.rs`; another project's instructions and a rendering design. Seen: 0 here (seeded from towercab-3d).

### Know what a capture leaves out, and capture from inside the app where you can
<!-- rule: what-capture-misses -->

Every capture route misses something, and nothing in the result says so. `screenshot` captures the main window with `PrintWindow(PW_RENDERFULLCONTENT)`, which works while it is cloaked and covered; menus, modals and tooltips gpui draws inside that window are in it, but a pop-out window (a viewer or Dashboard opened out), a native file or folder picker and a toast are separate OS windows and are not.

`PrintWindow` sends `WM_PRINT` to the UI thread, so it runs on a worker thread, never the UI thread, where it would deadlock. A window that is not shown, not cloaked by the client, foreground, or captured black is refused rather than returned.

Return a small inline preview plus the full-size path, saved under an ignored folder by pid (`<config dir>/driver-shots/<pid>-<n>.png`), so two clients never write the same file. Not met: the MCP `screenshot` returns the full PNG inline with no small preview (at most 480 px wide), and there is no baseline compare with a per-channel tolerance (2/255 in another project).
Source: `apps/native/src/driver/capture.rs`, `apps/native/src/driver/endpoint.rs`, `tools/rt-driver-mcp/src/tools.rs`; other projects' decision records, changelog fixes and a driving friction log. Seen: 0 here (seeded from godot-mcp, towercab-3d, yaat).

### Answer file dialogs before the action that opens them
<!-- rule: queue-file-answers -->

A native file or folder picker runs outside the app's windows, where posted input cannot fill it, and in a driven run it would open on the user's desktop. In a driven run the client should open no dialog: the agent queues one answer (a path or a cancel) per dialog before the action, each dialog takes the oldest, and one opened on an empty queue fails at once. `RootDeps.pick_folder` and `pick_file` are already injectable closures, which is the seam.

Not met: a driven run uses the system pickers, and the endpoint has no queue.
Source: `apps/native/src/lib.rs` (`RootDeps`), `apps/native/src/folder_picker.rs`; another project's driving friction log and dialog-queue tool. Seen: 0 here (seeded from yaat).

### Name what the driver cannot reach, and the route for each
<!-- rule: name-out-of-reach-ui -->

The endpoint reaches only what the client draws in its main window. Out of reach: native file and folder pickers (a queued answer is the route, "Answer file dialogs" above), toasts, pop-out windows' input and capture, IME composition (driven by the real IME context, which posting cannot fake), drag and drop from Explorer, and modified pointer input (refused; a chord goes through `key`, or a named `action`). Hover-only UI never shows under posted input; `action` `set_hover` sets it.

A refusal is a named error saying the surface is out of reach, returned before any input is sent. An agent that does not know this burns turns trying to click a native dialog. Not met: an action that opens a native picker is not refused in a driven run.
Source: `docs/native-client.md` ("Driver endpoint"), `docs/plans/headless-drive.md`; another project's instructions. Seen: 0 here (seeded from towercab-3d).

### Send the app's logs to one file and read its tail, allowing for the flush
<!-- rule: unified-log-tail -->

A driven client writes its log to `<config dir>/logs/native.log` under the session's scratch folder, beside the isolated daemon's `daemon.log`; agents read its tail, never the whole file, and the MCP server appends its last lines to an error. Allow for the logger's flush before concluding a line is absent.

Log a frame-timing line only when the frame rate drops below a floor (50 fps in another project), so a slowdown is diagnosable from the file without noise. Not met: the client logs no frame timing.
Source: `tools/rt-driver-mcp/src/session.rs`, `docs/on-disk.md`; another project's dev wrapper and logger. Seen: 0 here (seeded from towercab-3d).

### Log driving friction as it happens, in a tracked file, and build the tools from it
<!-- rule: friction-log-per-session -->

Every session that drives the client appends to a tracked friction log: the tool and arguments, what happened against what was needed, and the workaround, with recurrences noted against earlier entries; a fix ticks the entry off. Notes in `.tmp` are lost to the next session. Turn each item into an endpoint method, an `action`, a tool or a driving note here; in two other projects the log drove the hook's design line by line.

Not met: the repo has no driving friction log.
Source: friction logs and plan history in two other projects. Seen: 0 here (seeded from towercab-3d, yaat).

## App-side hook

### Compile the hook out of release builds where the build tool can
<!-- rule: dev-flag-gates-hook -->

The whole endpoint (the pipe, the handlers, the registry's recording) sits behind the off-by-default `driver` Cargo feature, which the installer build never enables, so no runtime setting can turn it on in a release; without the feature `driver_id` only forwards to `debug_selector`. The gate in `driver_gate.rs` is compiled into every build only to refuse `RUSTLING_TULIP_DRIVER=1` in a build without the feature. Every extension of the endpoint is feature-gated like the rest.
Source: `apps/native/Cargo.toml`, `apps/native/src/driver_gate.rs`, `apps/native/src/driver_id.rs`; another project's hook code and Vite's import.meta.env.DEV. Seen: 0 here (seeded from towercab-3d).

### Turn the hook on with one environment switch, read once at startup
<!-- rule: one-env-switch -->

`RUSTLING_TULIP_DRIVER=1`, exactly, read once in `main` before tracing starts or any folder, log or registry entry is touched, turns on the endpoint; `launch` always sets it.

Any other non-empty value, a build without the feature, a missing `RUSTLING_TULIP_OFFSCREEN_WINDOW`, dirs that fail `check_isolated` or an unset agent override refuses the run with `driver run refused: <reason>` on stderr and exit code 4, before any native start-up; a failed cloak exits with its own code, 3. The pipe's DACL grants only the current user and refuses remote clients.
Source: `apps/native/src/driver_gate.rs`, `apps/native/src/offscreen.rs`, `apps/native/src/driver/pipe.rs`; another project's startup and automation-mode code. Seen: 0 here (seeded from yaat).

### Put every activating or out-of-process UI path behind one seam, pinned by source tests
<!-- rule: one-gate-source-pinned -->

An app activates itself from many places, and the framework does too: gpui activates any window it shows, and its activate path sends a synthetic Alt through `SendInput` and calls `SetForegroundWindow`. One seam should decide every activation (the main window, pop-outs, a toast's "Go to pane", `cx.activate`), and a source-scanning test should fail on any raw activate call outside it; native pickers likewise go through one factory with a source test that no picker is built elsewhere.

Not met: each site checks `offscreen::requested()` itself (the main window, pop-outs, `cx.activate`), the toast path calls `activate_window` relying on toasts being suppressed, and no source test pins it.
Source: `apps/native/src/offscreen.rs`, `apps/native/src/lib.rs`, `apps/native/src/notify.rs`; another project's background-driving design, its source tests and a decompile of Avalonia. Seen: 0 here (seeded from yaat).

### Install the hook from the code that owns what it exposes, and remove only your own instance
<!-- rule: install-owner-cleanup-own -->

The endpoint is started in `open_main_window`, in the closure that creates the window and the `RootView` it reads and drives, and holds the view weakly, so it never outlives or replaces it. Its cleanup removes only its own discovery file (its pid) when the app quits; the start-up sweep deletes only files whose pid is not running or whose `started_at` is not that pid's creation time.
Source: `apps/native/src/lib.rs`, `apps/native/src/driver/endpoint.rs`; another project's hook install code. Seen: 0 here (seeded from towercab-3d).

### Advertise the hook's endpoint with an atomic per-pid discovery file in a fixed place
<!-- rule: discovery-file-per-pid -->

The client listens on the per-pid pipe `rt-driver-<pid>`, current user only, and a name already held is fatal, never retried under another.

Once a frame has drawn with the window cloaked and the listener exists, it writes `%TEMP%\rustling-tulip-driver\<pid>.json` (`pid`, `pipe`, `started_at` as the process creation `FILETIME`, `version`) through a temp file and a rename, outside the config dir, so the driver finds a client whatever dirs either runs with. The driver matches the file by pid and creation time before trusting it and pings before using the pipe.
Source: `apps/native/src/driver/endpoint.rs`, `apps/native/src/driver/pipe.rs`, `tools/rt-driver-mcp/src/launch.rs`; another project's transport code and pipe directory. Seen: 0 here (seeded from yaat).

### Compile the wire types into both sides from one source, and version them
<!-- rule: shared-wire-types -->

Keep the endpoint's request, reply and error types in one place with no UI dependency and compile them into both the client and the MCP server, so the two cannot drift in shape; report a protocol version from `ping` and the discovery file, bumped as methods and fields are added, and have the server compare it, so a skewed client is named rather than seen as a decode failure.

Not met: the types live in `apps/native/src/driver/protocol.rs` only, the server builds requests and reads replies as untyped JSON, and it does not compare the client's version.
Source: `apps/native/src/driver/protocol.rs`, `tools/rt-driver-mcp/src/pipe.rs`, `tools/rt-driver-mcp/Cargo.toml`; another project's driver project file and protocol version. Seen: 0 here (seeded from yaat).

### Name each feature's hook calls in its design
<!-- rule: design-names-hook-surface -->

A new feature's design names the endpoint methods, `action`s, `state` fields and `driver_id` names that will let an agent verify it, so the endpoint grows with the features instead of after them.
Source: four feature designs in another project. Seen: 0 here (seeded from towercab-3d).

### Let the app declare its commands with a marker of its own, checked before any app code runs
<!-- rule: opt-in-by-name -->

The client's named commands are its `action`s, declared in one table in `driver/handlers.rs` and read from the live build, so an unknown name's hint lists exactly what the running client has. Each takes its arguments as an object keyed by name (`select_tab {"id"}`), checked before any app code runs.

Each command should have an availability check that returns why it cannot run now, or nothing, asked before every call and shown in a list of the actions with their arguments, and a command returns what happened rather than the agent waiting out a settle time. Read app state the same way, in one call in one frame, as `state` does.

Not met: there is no method that lists the actions with their arguments and availability; only `open_spawn` refuses while another dialog is open.
Source: `apps/native/src/driver/handlers.rs`; other projects' decision records and app-command attribute code. Seen: 0 here (seeded from godot-mcp, yaat).

### Answer every request with a stable error code and a hint, and cancel it when its connection goes
<!-- rule: coded-errors-never-throw -->

The endpoint answers every failure (an unreadable or oversized line, an unknown method, bad params, a handler error) with a stable code (`unknown_method`, `bad_params`, `not_found`, `not_visible`, `refused`), a message and, where it helps, a hint; an unknown method's hint lists the methods, and codes may be added but never respelled. Requests on a connection are answered one at a time, in order.

A disconnect should cancel the running request (the one case left unanswered, since nobody is left). Not met: a request whose connection has gone still runs to its end ("Post every handler's UI work" above).
Source: `apps/native/src/driver/protocol.rs`, `apps/native/src/driver/pipe.rs`, `apps/native/src/driver/endpoint.rs`; another project's hook dispatcher and error codes. Seen: 0 here (seeded from yaat).

### Feed the app's errors into every tool result
<!-- rule: error-feed-on-results -->

A button that "does nothing" had usually logged an error, so the app's errors belong in every tool result.

The client should keep a sequenced, capped queue of the errors it logs (a tracing layer appending under a lock), and every endpoint reply should carry the errors logged from the call's start until it was ready (repeats collapsed, oldest first, capped, with a count of those left out); the server appends them to every tool result, success and error alike, so agents read them before the next step.

Partly met: an endpoint or session error appends the last lines of the session's `native.log`, the **error feed**; a successful call carries nothing, and the tail is every log line, not the errors.
Source: `tools/rt-driver-mcp/src/session.rs`, `src/tools.rs`; other projects' hook loggers, dispatchers, call-tool filters and their tests. Seen: 0 here (seeded from godot-mcp, yaat).

### Fail loudly when what the hook reads is not ready
<!-- rule: hook-throws-when-unready -->

A method whose window or view is not ready answers with a named error, never empty data: `screenshot` refuses a window that is not shown, not cloaked by the client, foreground or from another process, and a capture that came back black, and a reply whose frame never drew says `"drawn": false`. So an agent reading too early gets a cause, not a black rectangle.
Source: `apps/native/src/driver/endpoint.rs`, `apps/native/src/driver/capture.rs`; another project's hook code. Seen: 0 here (seeded from towercab-3d).

### Never block the app's main thread waiting on the app, and bound what runs there
<!-- rule: never-block-main-thread -->

Endpoint requests run on gpui's UI thread, so a call that blocks on work queued for a later frame deadlocks. Each reply waits for a frame drawn after its request asynchronously, at most 5 s; `wait_for` polls between frames in the endpoint, never blocking the UI thread; and `PrintWindow`, which sends `WM_PRINT` to the UI thread, and the PNG encoding run on a worker thread.

The endpoint calls handlers through `cx.update_window`, never a `WindowHandle::<RootView>::update`, which would hold the view `state` reads.
Source: `apps/native/src/driver/endpoint.rs`, `apps/native/src/driver/wait.rs`, `apps/native/src/driver/capture.rs`, `docs/plans/headless-drive.md`; other projects' hook command code and a measured spike. Seen: 0 here (seeded from godot-mcp, yaat).

### Click through the control's own action or a synthetic pointer, never both, and say which ran
<!-- rule: semantic-click-first -->

A semantic command (`action`) and a pointer press (`click`) are separate methods, and one call never does both (doing both fired a button twice in another project); each result names what ran (`{"action": name}` or `{"at": …}`).

Typing goes in as one keystroke per character with its `key_char`, so the app's own text input runs, and chords as keystrokes the app's key bindings see. Avoid the framework's private input API: gpui's `Window::dispatch_event` returns a crate-private type and cannot be called from outside gpui, so pointer input is posted instead.
Source: `apps/native/src/driver/handlers.rs`, `apps/native/src/driver/pointer.rs`, `docs/plans/headless-drive.md`; another project's background-driving design and a survey of in-app automation protocols. Seen: 0 here (seeded from yaat).
