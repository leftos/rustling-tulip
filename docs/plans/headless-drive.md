# Driving the native client headless (RT-324)

The design behind the driver endpoint, its MCP server and the cloaked, isolated window a driven client runs in. What each piece does is specified in `docs/native-client.md` ("Driver endpoint", "Driver MCP server", the OS smoke tier under "Tests"); the lessons behind them are in [DRIVING_CONVENTIONS.md](../DRIVING_CONVENTIONS.md).

## Design

An **in-process driver endpoint inside the native client**, built with the off-by-default `driver` Cargo feature and off unless `RUSTLING_TULIP_DRIVER=1` is set. It runs on GPUI's UI thread, injects keys with `Window::dispatch_keystroke`, posts pointer input as window messages to its own hwnd, reads state through `RootView` accessors and captures with `PrintWindow(PW_RENDERFULLCONTENT)` of the cloaked window.

A small stdio MCP server (`tools/rt-driver-mcp/`) launches the isolated client and talks to the endpoint over a per-pid named pipe. The rule that decides the shape (`drive-owned-app-in-process`): outside-in drivers all end up foregrounding the window or moving the cursor; only a hook inside the app never does.

- **Gate:** the feature, the env switch, `RUSTLING_TULIP_OFFSCREEN_WINDOW` set, and the config, binaries and worktrees dirs each under a `.tmp` ancestor (`daemon_client::isolation::check_isolated`); otherwise the client exits (code 4). A driven client must also point `RUSTLING_TULIP_CLAUDE` at the fake-claude shim for any Claude session, since an unisolated client would adopt or retire the owner's live daemon.
- **Window:** cloaked through `show_cloaked` (`apps/native/src/offscreen.rs`) with `WS_EX_NOACTIVATE`, at the bottom of the Z order; a failed cloak exits with code 3 so the window is never shown. gpui's `activate` is never called, since it sends a synthetic Alt and takes the foreground.
- **Element addressing:** semantic actions and logical coordinates, plus the `driver` feature's registry of named elements' bounds (`driver::registry`, named in view code with `DriverId::driver_id`). gpui's `debug_selector` is a no-op without `test-support` and its bounds map is crate-private, so `test-support` does not serve. gpui draws its own scene, so there is no accessibility tree to scrape.
- **Capture:** gpui 0.2.2 has no render-to-image, so capture is `PrintWindow` run off the UI thread (it sends `WM_PRINT` to the UI thread and would deadlock on it), and WGC only if video is ever wanted.
- **Native pickers** are answered through the injectable `RootDeps.pick_folder` and `pick_file` closures; IME composition, drag-and-drop from Explorer and native menus are untestable on this path.
- **Fake-daemon mode:** `RootView::with_transport` over a fake daemon, the `shot` example's fixture, serves pure-UI checks with no daemon.

### How input reaches gpui on Windows

- **Modifiers come from `GetKeyState`**, so posted Ctrl, Shift and Alt chords, and modified clicks and wheel, do not work, and a key the owner holds leaks in. Hence keys go through `Window::dispatch_keystroke`, and a `click` with `modifiers` is dispatched in-process through `Window::dispatch_event` (RT-1818). A `move`, `drag` or `wheel` with `modifiers` is refused.
- **Pointer route (RT-723):** `PostMessageW` of `WM_MOUSEMOVE`, then the button down and up, in physical client pixels converted from the request's logical point or the named element's visible-rect centre at the window's scale; move, drag and wheel take the same path.

  A move precedes every press (the window gets `WM_MOUSELEAVE` while the real cursor is elsewhere), and the wheel's lParam is in screen coordinates, so the poster converts with `ClientToScreen`. Clicks, right-click and hover tooltips are measured working on the cloaked window.
- **Keys:** a posted `WM_KEYDOWN` and `WM_KEYUP` only; gpui turns an unhandled key into `WM_CHAR` itself, which queues behind later keys, so the smoke tier spaces keys.
- Posted input reaches only a real window, so in-process UI specs cannot prove it; the smoke tier does. The smoke tier keeps the real window-procedure path as a thin check, since the synthetic path skips `translate_message` and IME.
- **Rejected:** Windows-MCP and UI Automation screen tools (they move the foreground and the cursor, and gpui has no UIA tree), and an outside-in posted-message driver as the main route (no chords, no state reads, DPI math at every call, a cloaked window invisible to UIA).

### Waits

Waits are on app state (`wait_for`), never sleeps, which also removes RT-42's class of flake from driven checks; `pane_text` reads a pane's grid instead of the daemon scrollback over a second socket.

## Steps

S1 to S6 have landed: cloak hardening and the isolation guard, the scripted drive-and-capture, the driver core, input breadth and waits, the MCP server (`LiveDaemon` moved to the shared dev crate `crates/dev-daemon`, `apps/native/tests/support/live.rs` keeping a wrapper), and the live check `rt.ps1 native-drive-check` with its docs.

The live check is routed by `.gate/desktop.toml` to a builder's logged-on desktop and refused on this PC without `-HandsOff`; it asserts the foreground window and cursor position are unchanged across a scripted drive.

What stays open under RT-324:

- **S7 (optional):** recording through WGC, and a fake-daemon mode of the driver.
- **Hidden desktop** (`CreateProcess` `lpDesktop`): measured working for screenshots and input in other projects, untested with gpui's DirectX renderer here; a later spike. The window is created with `show:false`, so the cloak should not flash before it applies; not measured.
- **Scripts bake in no screen coordinates:** logical pixels, named elements and actions only, so a layout change does not click the wrong thing; results report what was hit.

## Relation to RT-316 and RT-42

- **RT-316** (shot views for the overlays and the empty area, and a drive-and-capture command): the scripted drive-and-capture is its command, and the MCP `batch` reuses the same script format. RT-324 supersedes only its posted-input rig, by moving to in-process events.
- **RT-42** (`smoke_posted_keys_reach_the_shell` failed once with no marker within 30 s): the candidates are a click landing before the pane has attached its session, a `WM_CHAR` queued behind later keys, and a pane-centre computed from constants that drift from the layout. It stays open for the OS-posted path the smoke tier keeps and does not block RT-324.

## Dependencies

- A native file picker or sink (RT-35) must be covered by the picker queue.
- A startup dialog (RT-57) must not show on a driven client's seeded state, or the driver needs a dismiss path.
- Terminal-text checks (`pane_text`, typing echo) are only as good as the PTY output path (RT-317).
