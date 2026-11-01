# RT-324 map: driving the native client headless

PROTOCOL: 1 overlay: none (`docs/agents/explore.md` absent), read CLAUDE.md; 2 README: skipped; 3 Task Index: read the `docs/architecture.md` Task index rows "Run the tests" (:123) and "Look at a UI change" (:126), no row for driving/injecting input.

4 ck: skipped (one-repo question, Task Index rows + CLAUDE.md answered the docs side; not run); 5 design docs: RT-316/RT-42 read from Linear; 6 source: yaat/towercab/delve docs + driver, smoke_window.rs, shot.rs, offscreen.rs, tests/support/mod.rs, gpui-0.2.2 windows events; 7 tree search: done for `debug_selector`, actions, RootView accessors.

Root anchored: `X:/dev/rustling-tulip` (branch `main`).

## Answer in one paragraph

Build an **in-process driver endpoint inside the native client, off unless an env switch is set**, that runs on GPUI's UI thread and injects synthetic input with `Window::dispatch_event(PlatformInput)` / `dispatch_keystroke` (gpui-0.2.2 `window.rs:3581`, `:3540`), reads state through `RootView` accessors, and captures with the existing `PrintWindow(PW_RENDERFULLCONTENT)` of the cloaked window (no render-to-image exists in gpui 0.2.2).

Front it with a small stdio MCP server (or CLI) that launches the isolated client and talks to the endpoint over a per-pid named pipe. This is the yaat shape, and the same pattern towercab/delve reach by other means. OS-posted messages stay as the smoke tier's job. The cheapest first step is not a new server: extend `shot.rs`/the smoke tier into a "script of steps, PNG per step" command (RT-316), then lift that into the endpoint.

## (1) What each example does

| Repo | Pattern | What worked | What they had to fix (from docs) |
|---|---|---|---|
| yaat (Avalonia) | In-app hook: named pipe `yaat-automation-<pid>` + per-pid discovery file, switched on by `YAAT_AUTOMATION=1`; MCP stdio server `yaat-client-driver` (`tools/Yaat.ClientDriver.Mcp/`) over it; UI Automation + posted/real input kept only for CRC (an app they do not own). `docs/client-driver-mcp.md`, `docs/DRIVING_CONVENTIONS.md` | Pipe route never takes focus/cursor; screenshot rendered by the app (`RenderTargetBitmap`) shows what is really drawn; `wait_for`/`wait_until`/`batch_drive`; app tools (`call_app_tool`) for fragile setup; error ring appended to every result; file picker answers queued | Survey: no outside-in tool avoids stealing focus. Outside routes rejected: posted input (Avalonia reads Shift/Ctrl from real keyboard state, posted click calls SetFocus), separate desktop (UIA cannot cross), headless host. Windows needed `WS_EX_NOACTIVATE` (SW_SHOWNOACTIVATE leaves it activatable; focus handed on when the window in front minimizes), Z-order to bottom before first show, DWM cloak before first show (exit 3 if cloak fails), no `WindowState` changes (activate), one gate pinned by a source-scanning test. Cloaked window still captured by WGC (30 fps) and PrintWindow; minimized window stops rendering. Posted-input hygiene: a move before every press (MOUSELEAVE), double-click = two clicks, refuse modifiers/control chars. Element ids stale between calls so batch by selector. Server runs from a shadow copy (MSB3027). Isolated app-data dir. |
| towercab-3d (Tauri/WebView2) | Engine-side: Playwright MCP attaches over CDP (port 9222); dev-only `window.__tc3d` hook for state; no server of its own. `docs/DRIVING_CONVENTIONS.md` | CDP input and screenshots work with window hidden (`--agent`: created `visible:false, focus:false`, frame-capped, hidden window keeps rendering); hook for camera/projection/readPixels | `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` was overwritten by the host (CDP attach impossible), `CloseMainWindow` cannot find hidden window, window-state plugin saved visible flag, 0.9 s flash before first hide, isolated app data not met yet, additive effects missed by screenshots (read buffer). |
| delve-the-dungeon (Godot) | Engine MCP: `godot-mcp` (own repo `X:/dev/godot-mcp`), bridge injected via `override.cfg` beside `project.godot`; no `docs/DRIVING_CONVENTIONS.md` in delve (its lessons are in `docs/development/driving-the-client.md`, 55 KB; rules in `docs/GODOT_CONVENTIONS.md`) | `click` by node path/text, screenshots, live node get/set, `wait_for`, `batch_drive`; "quiet" run = off-screen, unfocused, silent, frame-capped (`Engine.max_fps` 60), real input kept out by the window never receiving it (not by swallowing) | Window must be created unfocused/off-screen via `override.cfg` (Godot creates it focused before any script); hidden desktop optional; real gamepads still leak in; bridge teardown check; build/import through gate (`prepWrapper`); sessions named per worktree. |
| User rulebook | `C:\Users\lefto\.claude\conventions\driving.md` (NOT `docs/conventions/DRIVING_CONVENTIONS.md`; revision 11), synced by `conventions-sync` (`C:\Users\lefto\.claude\skills\conventions-sync\SKILL.md`, stack `driving`) | ~70 rules in slug-headed sections: Server plumbing, Driving an app, App-side hook | See (5). |

The rule that decides the design (`drive-owned-app-in-process`): outside-in drivers all end up foregrounding or moving the cursor; the ones that never do run a hook inside the app.

## Rustling-tulip starting point (facts)

- **Cloaked window**: `apps/native/src/offscreen.rs`. `RUSTLING_TULIP_OFFSCREEN_WINDOW` non-empty: window opened with `show:false, focus:false` (`lib.rs:336-345`, `shot.rs:277-293`), then `show_cloaked` (offscreen.rs:26) spawns a thread that does SetWindowPos to the primary work-area origin at `WINDOW_SIZE` (1000x640 logical), `DwmSetWindowAttribute(DWMWA_CLOAK)`, `ShowWindow(SW_SHOWNOACTIVATE)` (offscreen.rs:51-92).

  Must never call gpui `activate` (it sends a synthetic Alt via SendInput and SetForegroundWindow, offscreen.rs:7-10). The window carries `WS_EX_NOACTIVATE`, is placed at the bottom of the Z order before it is shown, and a failed cloak exits the process with code 3 (`CLOAK_FAILED_EXIT`, yaat's pattern) so the window is never shown; notifications suppressed (`notify.rs:115`, `lib.rs:761`).
- **Smoke** `apps/native/tests/smoke_window.rs` + `rt.ps1:975` (`Invoke-NativeSpecFile -TestFiles smoke_window -Slot light`): isolated `LiveDaemon` (support/live.rs; sets the three overrides), seeds a shell and the client's layout (`seed_layout`, avoids the first-connect chooser), launches the exe, `find_window` by pid, `assert_out_of_the_way` (DWMWA_CLOAKED != 0 and not foreground), waits for `connected to daemon` in `<config>/logs/native.log`.

  **Keys**: `press()` posts only `WM_KEYDOWN`+`WM_KEYUP` with scan code (smoke_window.rs:518-537); gpui's `handle_keydown_msg` calls `translate_message` when the app does not handle the key, which makes `WM_CHAR` (events.rs:372-407, 428). `KEY_GAP` 100 ms because the WM_CHAR queues behind later keys (:102-107).

  **Clicks**: `click_at` posts MOUSEMOVE, LBUTTONDOWN(MK_LBUTTON), LBUTTONUP (:506). **Capture**: `PrintWindow(PW_CLIENTONLY|PW_RENDERFULLCONTENT)` into a DIB (:632-676), pixel probes by layout constants. Verification of typing reads the daemon scrollback over a second WebSocket (`plain_lines`, :231).
- **Shot** `apps/native/examples/shot.rs` + `rt.ps1:984`: no daemon; `RootView::with_transport(RootDeps{...})` (`lib.rs:294-321`, :826) over a `FakeDaemon` thread answering `NetCommand::Send`; window cloaked.

  It drives with `Window::dispatch_keystroke` for ctrl-3 / ctrl-, / ctrl-shift-n (shot.rs:351-353) and a posted click for the rail (`win32::click`, :1126, which also parks the pointer at (-10,-10)); waits `LOAD_WAIT` 1.5 s and `DRIVE_WAIT` 1 s (sleeps, not conditions), checks `view.reached(root)` via RootView accessors, `write_settled` (two equal captures, rejects all-black) writes PNG.

  Six views only. Note it already mixes in-process key dispatch with posted clicks, because a posted key carries no modifiers.
- **UI-spec harness** `apps/native/tests/support/mod.rs`: in-process `TestAppContext`/`VisualTestContext` with a scripted fake daemon, `click_on(selector)`/`center(selector)` using `debug_selector` tags, `simulate_keystrokes`, `drag`, `right_click`, `root_update`.

  **Trap**: gpui's `debug_selector` is a no-op unless the `test-support` feature is on (gpui `elements/div.rs:705-721`), and gpui's bounds map is crate-private (`window.rs:682`), so enabling `test-support` in a driver build would not expose element bounds. A real client has no selectors; the `driver` feature's registry (`apps/native/src/driver/registry.rs`) answers by name instead: view code names an element with `.driver_id(|| ...)` (`apps/native/src/driver_id.rs`), which adds an absolute canvas child that records the element's padding-box bounds and the part visible under the content mask (scroll containers keep `.debug_selector`).
- **How GPUI on Windows takes input** (`gpui-0.2.2/src/platform/windows/events.rs`, window procedure): WM_MOUSEMOVE (:283, StartTrackingMouse TME_LEAVE, so a real-cursor-elsewhere window gets WM_MOUSELEAVE, same trap as Avalonia: post a move before each press), WM_LBUTTONDOWN etc. (:81).

  It also takes WM_MOUSEWHEEL (:520: lParam is **screen** coords, converted with ScreenToClient, so the poster must ClientToScreen; delta uses system wheel-scroll-lines; Shift is read from real keyboard), WM_KEYDOWN (:98), WM_CHAR (:100), WM_IME_STARTCOMPOSITION/COMPOSITION (:102-103, driven by the real IME context via Imm APIs, so posting cannot fake composition), WM_ACTIVATE (:42), WM_SETCURSOR.

  **Modifiers come from `GetKeyState`** (`is_virtual_key_pressed`, :1460; `current_modifiers()` on mouse move/wheel). Consequences: posted Ctrl/Shift/Alt chords and modified clicks/wheel do not work (and a real held key by the owner leaks in), exactly as yaat found for Avalonia. Posted plain keys, letters (via TranslateMessage), clicks, drags (MOUSEMOVE with MK_LBUTTON) and wheel work, proven by smoke/shot for key, click.
  Right-click and hover tooltips on the real cloaked exe are measured working with posted messages. Unmeasured here: posted wheel and drag on the real exe.
- **Capture routes**: gpui 0.2.2 has no render-to-image/read-pixels API (searched `render_to_image|read_pixels|screenshot` in the crate: nothing). `PrintWindow(PW_RENDERFULLCONTENT)` of the cloaked window is proven (smoke + shot).

  WGC (yaat's route) is the alternative for video; PrintWindow can block the render thread 1-5 s on some apps (yaat doc) but was fine for stills here. In-process capture is limited to the same PrintWindow/GDI (call it from a non-UI thread, as shot.rs does with a `capture` thread, because PrintWindow sends WM_PRINT to the UI thread and would deadlock if called on it).
- **Existing "act on the app" surface**: `RootView` public accessors/mutators in `apps/native/src/lib.rs` (e.g. `active_tab_id` :1154, `tab_ids` :1160, `active_pane_ids` :1210, `pane_grid_text` :1232, `pane_input_handler` :1240 (an `InputHandler`, so IME-style `replace_and_mark_text_in_range` can be called in-process), `focus_container_row` :1527, `launch_last_row`, `sidebar_containers`, `settings_open`, `spawn_dialog_open`, `activity`, `set_app_font`...).

  Most are `pub` for the spec harness and read-only; mutations in specs go through `root_update`. Key actions are bound in `bind_keys` (`text_input.rs:65`; shot uses "ctrl-3", "ctrl-,", "ctrl-shift-n"). The wire is also a surface: `NetCommand::Send(ClientMessage)` (RootDeps.tx) is how the UI talks to the daemon; an agent can equally spawn/seed state straight on the daemon over its WebSocket (what the smoke does), no UI needed.
- **Isolation trap**: a driven client must start against an isolated daemon with all of `RUSTLING_TULIP_CONFIG_DIR`, `RUSTLING_TULIP_BINARIES_DIR`, `RUSTLING_TULIP_WORKTREES_DIR` under `.tmp/` (`support/live.rs` `LiveDaemon::envs()`; CLAUDE.md env table; also `RUSTLING_TULIP_TAILNET_DIR` if remote is touched).

  The client auto-spawns a daemon via `ensure_running` and, with defaults, would adopt or retire the owner's live daemon (CLAUDE.md "The native client auto-spawns the daemon"); the client also writes `native-ui.json` and `client-id-native` into the config dir. The driver must refuse to launch unless all three are set to paths under `.tmp/`, and must also set `RUSTLING_TULIP_CLAUDE` to the fake-claude shim for any Claude session.

## (2) Recommended design

**Option A (recommended): in-process driver endpoint + thin MCP/CLI over it.**
- Gate: a Cargo feature `driver` (not default, not in the installer build) **plus** an env switch `RUSTLING_TULIP_DRIVER=1` read once at startup, and refuse to start unless `RUSTLING_TULIP_OFFSCREEN_WINDOW` is set and the three dirs are under a `.tmp/` ancestor. (yaat ships the pipe in release gated at run time; towercab compiles it out of prod via the dev flag; the rulebook prefers compile-out where the build tool can: `dev-flag-gates-hook`.)
- Transport: `\\.\pipe\rt-driver-<pid>` via the `interprocess` crate already used by the tracer (`CLAUDE.md` tracer section), current-user only, line-delimited JSON `{id, method, params}` with coded errors + hints, discovery file per pid under `%TEMP%`, as yaat. Request handlers hop to the GPUI thread through a channel and `cx.spawn`/`AsyncApp::update_window`, never blocking the UI thread; PrintWindow runs on a worker thread (shot.rs already does).
- Methods v1: `ping`, `state` (JSON from RootView accessors: active tab, tabs, panes, sessions, overlays open, window size/scale), `key` (Keystroke strings via `Window::dispatch_keystroke`, so Ctrl/Shift chords work without the real keyboard), `type_text` (per-char KeyDown with `key_char`, or the pane `InputHandler`).

  Then come `click/move/drag/wheel` (build `MouseDownEvent`/`MouseMoveEvent`/`ScrollWheelEvent` and `Window::dispatch_event`, positions in logical px; modifiers explicit, so no GetKeyState leak), `action` (named command: open settings, new shell, select tab...). Then come `wait_for` (poll state predicate with bounded timeout, return last value), `screenshot` (PrintWindow to PNG path), `pane_text` (grid text of a pane, replacing reading scrollback over a second socket), `log_tail`.
- MCP server: new crate (e.g. `tools/rt-driver-mcp/`, Rust, stdio, fixed tool surface: `launch`, `stop`, `call(method, params)`, `screenshot`, `batch`), owning the isolated daemon (reuse `tests/support/live.rs` logic, which should move to a shared dev crate), launching the client with the env, returning pid; stdout JSON-RPC only, shadow-copy launch, named-session-per-call (rulebook rules). A plain CLI (`rt.ps1 native-drive <script.json>`) is the same code path and is what RT-316 asks for.
- GPUI makes easy: `dispatch_event`/`dispatch_keystroke` are public on `Window` (`dispatch_keystroke` needs no feature, shot.rs uses it in a normal build); RootView is already a testable object with accessors; capture proven; `RootView::with_transport` allows a **fake-daemon mode** of the same driver (no daemon at all) for pure-UI checks, which is the existing shot fixture.
- GPUI makes hard: no `debug_selector` outside test-support, so element addressing by name is the `driver` feature's registry (`driver::registry`), with semantic actions/accessors and logical coordinates from layout constants beside it; no accessibility tree/UIA to scrape (gpui draws its own scene), so `list_windows`/`find_elements` equivalents must be built by us.

  There is no render-to-image, so capture depends on a window existing and on PrintWindow. Synthetic events skip the OS focus/IME path (IME composition, drag-and-drop from Explorer, native menus/dialogs untestable this way; native folder/file pickers must be queue-answered through `RootDeps.pick_folder/pick_file`, which are already injectable closures, shot.rs:256-259). `PrintWindow` must run off the UI thread.

**Option B: OS-posted messages to the cloaked window from an external driver** (what smoke does). Pros: tests the real window-procedure path (WM_*), zero client changes, already works for keys/clicks.

Cons: no Ctrl/Shift/Alt chords or modified clicks (GetKeyState), a real held key from the owner leaks in (yaat doc, `posted-input-hygiene`), no wheel-at-point without ClientToScreen math, IME impossible, timing races (KEY_GAP 100 ms; RT-42), no state reads (needs a second WebSocket to the daemon and pixel probes), coordinates need DPI math (`physical()`), a cloaked window is invisible to UIA-style tools (yaat). Keep it for the smoke tier as a thin "real wndproc still works" check.

**Option C: Windows-MCP (user scope) / UIA screen tools**: foreground/cursor moving; gpui has no UIA tree anyway. Rejected (rulebook `drive-owned-app-in-process`).

**Hidden desktop** (CreateProcess `lpDesktop`, yaat/godot-mcp measured it works for screenshots and input) removes the cloak-flash risk but is untested with gpui's DirectX renderer here; unmeasured, leave as a later spike.

Tradeoff summary: A costs a feature-gated crate surface and a maintained method list (and the synthetic path skips `translate_message`/IME), B costs nothing but is limited to what an OS message can say without modifiers. A also fixes RT-42's class of flake because waits are on app state, not sleeps.

## (3) Steps (each ~<=100 implementer calls)

1. **S1 Cloak hardening + isolation guard** (done). Files: `apps/native/src/offscreen.rs`, `apps/native/tests/smoke_window.rs`, `crates/daemon-client/src/isolation.rs`. The window gets `WS_EX_NOACTIVATE` and the bottom of the Z order, the process exits with code 3 if the cloak fails (also for the `shot` and `diff_bench` examples), and `daemon_client::isolation::check_isolated` refuses unless the three dirs are each absolute with a `.tmp` folder strictly above them; nothing calls it until the driver endpoint (S3).

   Tests: unit tests `isolated_env_refuses_a_dir_outside_tmp` and its siblings in `isolation.rs`, and the smoke spec `smoke_window_has_noactivate_and_sits_below_foreground` (windows-only, `#[ignore]`, run by `native-smoke`, which now runs four specs).
2. **S2 Scripted drive-and-capture (RT-316 core) without an endpoint**. Files: new `apps/native/examples/drive.rs` (or a `shot.rs` mode) + `rt.ps1` verb `native-drive`; steps JSON `{key|click|type|wait_state|shot}`; for state waits, reuse `RootView` accessors (fake-daemon mode, `with_transport`).

   Red-first test: `drive_script_parses_and_rejects_unknown_step` and `drive_waits_on_state_not_sleep` (unit tests in the example, like `shot.rs` tests at :1333). Add views for the exited overlay, abandoned overlay and empty main area to `shot.rs` (RT-316 first ask) with `every_view_has_a_fixture` extended.
3. **S3 Driver core in the client (feature `driver`)**. Part A is done: `apps/native/src/driver/{protocol.rs,handlers.rs}` answer `ping`, `state`, `find` and `key` (one keystroke per request, since gpui flushes emitted events only when an update ends) in-process, with `tests/ui/ui_driver.rs`; a reply to an undecodable line carries its id when it has a usable one, else the reserved id 0. `click` is not built: `Window::dispatch_event` returns the crate-private `DispatchEventResult` (gpui-0.2.2 `window.rs:1273`), so the route is RT-723's to choose. Part B adds the pipe, the gate (exit 4 when not isolated), the discovery file and `screenshot`; it calls `handle` through `cx.update_window` (never a `WindowHandle::<RootView>::update`, which would borrow the view `state` reads) and waits for a frame after input before answering. Files: new `apps/native/src/driver/{mod.rs,protocol.rs,handlers.rs}`, `Cargo.toml` feature, `lib.rs` hook call in `open_main_window` after window creation, `docs/native-client.md` section. Methods `ping`, `state`, `key`, `click`, `screenshot`, coded errors.

   Red-first: a `ui` spec module `tests/ui/ui_driver.rs` (add `mod` in `tests/ui/main.rs`) named `driver_key_ctrl_comma_opens_settings` and `driver_unknown_method_lists_methods`, using the in-process harness (no pipe), plus pipe round-trip test `driver_pipe_answers_ping` over a stub.
4. **S4 Input breadth + waits**. `type_text`, `move/drag/wheel/right_click`, `wait_for`, `pane_text`, `action`. Red-first: `driver_drag_resizes_the_sidebar`, `driver_wheel_scrolls_the_pane_scrollback`, `driver_wait_for_times_out_with_last_value`, each in `tests/ui/ui_driver.rs`; wheel/drag use `Window::dispatch_event`.
5. **S5 MCP server crate** `tools/rt-driver-mcp/` (stdio, fixed surface `launch/stop/call/batch/screenshot`, shadow-copy launcher, named sessions, error feed from `native.log`), launches isolated daemon + client (reuse S1 helper, `LiveDaemon` moved to a shared dev crate). Red-first: `stdio_smoke_every_line_is_json_rpc` (like yaat `smoke.ps1`) and `launch_refuses_without_isolated_dirs`. `.mcp.json` entry.
6. **S6 Live check + docs**: `rt.ps1 native-drive-check` (asserts foreground window and cursor position unchanged across a scripted drive, like yaat `live-check.ps1 -Background`; needs the owner's OK to run on their desktop), `docs/DRIVING_CONVENTIONS.md` (see 5), Task Index rows, CLAUDE.md commands/tier text, `tools/test-map.json` rows for the new files (`.\rt.ps1 test -Validate` must pass). Red-first: `test_map_covers_driver_files` via the existing `-Validate`.
Optional S7: recording (WGC) and fake-daemon mode for `driver`; later.

## (4) Relation to RT-316 and RT-42

- **RT-316** (Backlog; "native-shot views for overlays and empty area, and a drive-and-capture command"; the RT-305 debugger's rig was deleted): its first half (new shot views) is S2 and independent of the driver; its second half (one command that launches an isolated client, drives it with clicks and keys and saves a PNG per step) is the S2 CLI first, then the same script format becomes the MCP `batch` in S5.

  Do S2 inside RT-316 and keep RT-324 as the umbrella; RT-324 supersedes only the "posted-input" rig part by moving to in-process events, so scripts must not bake in screen coords (use logical px/actions).
- **RT-42** (Backlog, vNext, `smoke_posted_keys_reach_the_shell` failed once, "no rt-smoke-marker within 30 s", passed unchanged; suspects PT.5's `pane_area` left edge past the 52 px rail, or posting keys before the shell's prompt).

  Reading the current spec: it already waits for the prompt (`wait_for_prompt`, :293), retries typing until it lands (:778-790) and clicks the pane centre; remaining race candidates: a click that lands before the pane has attached its session, a WM_CHAR queued behind later keys (KEY_GAP comment :102-107), and the click `pane_center` computed from constants that can drift from the layout.

  None re-measured in this run. RT-324's endpoint removes the whole class for driven checks (state wait, not sleeps; `pane_text` instead of scrollback socket), but RT-42 should stay open for the OS-posted path the smoke tier keeps; its fix does not block RT-324.

## (5) A `docs/DRIVING_CONVENTIONS.md` for this repo?

Yes. `conventions-sync` supports a `driving` stack (SKILL.md description lists DRIVING; script `~/.claude/skills/conventions-sync/scripts/conventions_sync.py status --stack driving`), the rulebook is `C:\Users\lefto\.claude\conventions\driving.md`, and yaat/towercab/godot-mcp each hold a repo file with a ledger `docs/.conventions-sync-driving.json`.

Rustling-tulip has neither file and CLAUDE.md does not name a stack list in a nextup profile hook (not read here: the `rustling-tulip-nextup` profile was not checked).

Seed it on the first driver step (S1 or S3), not before: first sync = create the file with title/intro and every rulebook rule `user-new`, decline what does not apply with reasons (CDP/webview args, gamepad ids, Godot-only, hook injected per run, refcounted hook file; and, until S5, the Server plumbing section: "an app driven through someone else's server declines the first" in the rulebook intro).

Rules pulled but not met get an **add** to the plan (skill "Unmet pull"), which will include the RT-324 steps themselves. The repo should also add the nextup pre-loop hook line naming `--stack driving` once the file exists.

## DEPENDS ON

- RT-267 (agent seam): no technical dependency for S1-S5; the driver reads `RootView` state, and RT-267 moves Claude support behind `agents/` in the daemon (`docs/plans/agent-plugins.md:263`). Risk is only merge conflicts in `lib.rs`/`net.rs` and fixture message shapes (shot's `check_reply_shapes` catches wire drift).
- RT-35 (native download sink for fetched files): adds a file picker/sink; the driver's picker queue (`RootDeps.pick_folder/pick_file`) must cover it; no blocker.
- RT-57 (resume sessions a reboot killed): adds a startup dialog; a driven client must start with a state that does not show it, or the driver needs a `dismiss`/force path (like the layout chooser the smoke avoids by seeding); no blocker for S1-S3.
- RT-317 (PTY output fix, not read in this run, title only from the task text): any driven check of terminal text (`pane_text`, typing echo) is only as good as the PTY output path; do S4 `pane_text` tests after RT-317 or expect the same flake.
(None of the four Linear issues other than RT-316/RT-42 was opened; claims about them rest on `docs/plans/MAIN.md` titles and plan-file references.)

## BRANCH

Work on a feature branch from `main` (current branch: `main`). One branch per step S1..S6 (small, mergeable), the owner's repo rule is a feature branch PR for marked features (commit `e35c200`: "docs: land marked features through a feature branch PR"); RT-324 is the umbrella issue. Do not edit `docs/plans/MAIN.md` by hand.

## OPEN DECISIONS

1. **Endpoint vs OS-posted only.** Recommend: in-process endpoint (A), posted messages kept in smoke. Worst case: the synthetic path (`dispatch_event`) hides a bug the real wndproc path has (e.g. the TranslateMessage/WM_CHAR route), so a driven check passes and the owner finds the bug; mitigation: the smoke tier stays and S6 live check.
2. **Compile-out feature vs run-time env only.** Recommend: Cargo feature `driver` plus env switch plus offscreen+isolation preconditions. Worst case: the feature is accidentally enabled in a shipped build, exposing a local pipe (current-user only) that can inject input; mitigation: installer build asserts the feature off, test pins it.
3. **Element addressing**: semantic actions + logical coordinates (recommended) beside the `driver` feature's registry of named elements' bounds (`driver::registry`); gpui `test-support` does not serve, since its debug selectors are a no-op without it and the bounds map is crate-private. Worst case: coordinates drift after a layout change and a script clicks the wrong thing; mitigation: results report what was hit (the rulebook `refuse-before-pressing`), prefer actions.
4. **MCP server in Rust vs PowerShell/CLI only.** Recommend: CLI/script runner first (S2), MCP (S5) after it proves useful. Worst case: building S5 before the owner uses S2 wastes the cost; cost of the opposite order is none.
5. **Capture**: keep PrintWindow stills; WGC only if video is wanted. Worst case: PrintWindow returns black/stale for a surface the compositor does not redraw while cloaked (shot.rs already rejects all-black).
6. **Cloak vs hidden desktop vs off-screen position.** Recommend: stay cloaked (proven here). Worst case: a brief flash before the cloak (towercab measured 0.9 s of show before app code; here the window is created with `show:false` so it should not flash; not measured in this run).
7. **Seed `docs/DRIVING_CONVENTIONS.md` now or with S1.** Recommend with S1. Worst case: a stale file of unmet rules; the ledger tracks them.

## DOC DRIFT

- `CLAUDE.md:94` says smoke "checks it connects and that posted keys reach the shell"; matches `smoke_window.rs` (it also checks paint and pane redraw, understated, not wrong). No drift found there.
- `docs/architecture.md:126` lists the six shot views; matches `shot.rs:107-114`. No drift.
- RT-316's text says "native-shot... RT-303/304 could not be shown": no doc to correct. No other drift found in the pages read.

## TASK INDEX GAP

The question took roughly 25 reads/searches beyond the Task Index row and one doc. Missing row: "Drive the running native client headless (inject input, capture, read state)" -> `docs/native-client.md` (OS smoke :227, Screenshots :228), `apps/native/src/offscreen.rs`, `apps/native/tests/smoke_window.rs`, `apps/native/examples/shot.rs`, `apps/native/tests/support/{mod.rs,live.rs}`, `apps/native/src/lib.rs` (RootDeps :294, RootView accessors), then `docs/DRIVING_CONVENTIONS.md` once it exists.

## MISSING ENTRY POINT

- `docs/agents/explore.md` (repo overlay) does not exist.
- `docs/DRIVING_CONVENTIONS.md` does not exist (see 5).
- Glossary: no glossary was found for terms such as "cloaked", "smoke tier", "shot"; the glossary is in `docs/README.md`.
