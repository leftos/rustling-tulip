# GPUI on iOS: the session list in the simulator (RT-1152)

Date: 2026-11-08. Branch `rt-1152-gpui-ios`, base `main` at `66ff1332`. A spike: nothing here lands on `main`.

Whether the phone app can become the desktop client's own Rust in a phone layout. Repo facts name the file; engine facts name the Zed PR or the source file at the revision below.

## Answer

**Go for iPhone, wait on Android.** At Zed revision `d2a4fb6ded7c470be25d1541281da3882495b65b`, the whole `rustling_tulip_native` library compiles for `aarch64-apple-ios-sim` with no `apps/native` source change beyond the GPUI upgrade, links, and runs. The real `sidebar_panel` drew in the simulator from a fake daemon, scrolled by touch with momentum, took taps through its own handlers, and the client's own `TextInput` took soft-keyboard text.

Three gaps are real work, none a stopper: Return reaches a field only as a `\n` in its text, never as a key event; an iPhone app cannot spawn `ssh` or read a root store the way the desktop does; and the backend is experimental, on a git pin rather than a crates.io release. Android has no Zed backend at all.

What this does not prove: a physical iPhone (it links, it was not run), real battery or memory cost, a real daemon connection, and the terminal or chat views on a phone. See "Not measured".

## The revision

`d2a4fb6ded7c470be25d1541281da3882495b65b` (2026-11-08), Zed's `main` on the day. Ruling: this rather than the backend's own merge `a47f721832` (zed-industries/zed#63068, 2026-10-14), since 8 follow-ups to `gpui_ios` landed after it, including the `ScrollEvent` rename, trackpad scrolling, the back gesture and Universal Links (#65805, #65806, #65858).

`gpui` 0.2.2 on crates.io came from `69e2130295` (2025-10-21, from its `.cargo_vcs_info.json`). From there to this revision is 8,655 Zed commits, 821 of them touching `crates/gpui*`. The crate still calls itself 0.2.2 and has had no crates.io release in that span, so a git pin is the only way to take it.

## Measures

Mac mini builder: Xcode 27.1, rustc 1.99.1 (Zed pins 1.98.1; stable was used through `RUSTUP_TOOLCHAIN`), simulator iPhone 18 Pro on iOS 27.0 (the iPhone 18 Pro device type does not exist on the installed 27.1 runtime). Logs named below were under `.tmp/` in the worktree (gitignored, so local to the run), the app's log went with the simulator, and the screenshots are kept in [2026-11-08-gpui-ios-spike/](2026-11-08-gpui-ios-spike/).

| Measure | Result | Evidence |
|---|---|---|
| Stock `gpui_ios` example builds | Yes, 184 s cold, 36.8 MB debug binary | `mac-stock.log`, `stock-example.png` |
| Native client library for iOS sim | Compiles; 1 error, in `crates/agents/claude/accounts/src/locks.rs:1190`, not in `apps/native` | `ios-check-1.log`, `ios-check-2.log` |
| Example target for iOS sim | Builds and links, 142.6 MB debug binary | `ios-build-1.log` |
| Example for a physical iPhone (`aarch64-apple-ios`, release, thin LTO, 1 codegen unit) | Cold 3 m 13 s over 460 crates; links; 32.6 MB binary, 14.6 MB gzipped | `ios-device-release.log` |
| Incremental rebuild after a one-file edit | 7 to 8 s | `ios-build-2.log` to `ios-build-5.log` |
| Launch to the shell's first line | 110 ms to 385 ms in the simulator | app log (`rt1152.log` in the app's tmp) |
| Real session list renders | Yes: header, filter pills, toolbar, repo rows, 48 leaves with status glyphs | `session-list.png` |
| Safe areas | `window.insets()` reports top 62, bottom 34 on the iPhone 18 Pro; padding by them clears the status bar and home indicator | `session-list.png` status strip |
| Touch scrolling | Works, including momentum: a 0.12 s flick ran from 0 to -1113 pt, decaying over about 2.7 s, with a new offset every 21 ms | app log `scroll:` lines (20:35:38.441 to 20:35:41.118) |
| Tap on a row | Reaches the real handler: the client sent `create_tab` with the tapped session's id, in both the plain and keyboard-up states | app log `client sent:` lines |
| Soft keyboard | Rises and lowers; `insets.ime.bottom` goes 0 to 301 to 0, and the layout follows | `keyboard-composer.png` |
| Text input | Every character arrives through `replace_text_in_range`; delete through `deleteBackward` | app log `composer text:` lines |
| Tap outside the fields | Dismisses the keyboard, and the same tap still lands on the row under it | app log (20:38:36.477 then 20:38:36.521) |
| Return, soft key and hardware keycode | Inserts a literal `\n`. No key event: the keystroke counter stayed at 0 through all typing and both Return kinds | `keyboard-composer.png`, `keyboard-single-line.png`, app log |
| Return in a single-line `TextInput` | The field holds `abc\n` and emits no `TextInputEvent::Submit` | `keyboard-single-line.png`; no `line event:` in the app log |
| Hardware-keyboard shortcuts | Not delivered. `crates/gpui_ios/src/ios` has no `pressesBegan`, and #63068's body says so; the simulator run had no shortcut to send | source at the revision |
| Render rate | About 42 renders per second with sessions showing `working` (1873 in 45 s); that the spinner drives it is my reading, not measured apart | status strip in `keyboard-single-line.png` |

The example is `apps/native/examples/phone_sessions.rs`: `RootView::with_transport` over an in-process fake daemon sending 48 sessions across 4 repos, `sidebar_panel` at the viewport's width through a new `RootView::phone_session_list`, and two real `TextInput`s. Its `.app` is the binary, `phone_sessions.Info.plist` and an ad hoc `codesign`. The stock example's `build-simulator.sh` is the whole packaging recipe; a signed TestFlight build was not attempted.

### What getting there took on iOS

- `KeyBinding::new("secondary-a", …)` panics at startup on every platform at this revision: `Keystroke::parse` refuses `secondary-` without an OS. The compiler cannot catch it. Four bindings in `text_input.rs` hit it, and the first launch died there.
- `TextInput` had to implement `text_length_utf16`. The iOS bridge decides keyboard eligibility in `reconcile_text_input` from `document_range`, which is `None` for a handler with no length. Without it the caret drew and focus was right, but no keyboard rose and no text arrived.
- The field's tap has to call `window.request_virtual_keyboard()` as well as focus it, as Zed's `examples/input.rs` does. The example does it in the box around the field; the real change is one line in `TextInput::on_mouse_down`.
- `crates/agents/claude/accounts/src/locks.rs`: `lock_description` is gated on `accounts_locks`, set only on the platforms with locks, yet `put_back` calls it unconditionally under `unix`. On iOS that is a compile error.

### Return, in detail

`gpui_ios`'s `insert_text` is `handler.replace_text_in_range(None, text)`, so Return is text and `KeyDownEvent` never fires. The client's own rule that Enter submits and Shift+Enter breaks a line (`text_input.rs` `Enter` and `Newline` actions) therefore never runs. `TextInputConfiguration::input_action` labels the key (`Send`, `Done`) but does not change what it delivers.

From the app's log, typing, Return by keycode, more typing, the soft Return key, the soft delete key, then the single-line field:

```
20:37:55.318 composer text: [hello tulip]
20:37:58.149 composer text: [hello tulip\n]
20:37:59.867 composer text: [hello tulip\nsecond]
20:38:22.638 composer text: [hello tulip\nsecond\n]
20:38:24.290 composer text: [hello tulip\nsecond]
20:38:27.652 line text: [abc]
20:38:29.340 line text: [abc\n]
```

The workable answer without waiting on Zed: `TextInput::replace_text_in_range` on iOS treats an inserted `"\n"` as the Return action, emitting `Submit` in a single-line field and following the composer's rule in a multi-line one. A pasted block arrives through the same call, so the check is on the whole inserted string being one `\n`. I did not build this; it is the first task of a phone client.

## What carries over from `apps/native`

Measured by [share.py](2026-11-08-gpui-ios-spike/share.py): non-blank, non-comment lines before each file's test module, each file classified once. A file counts as a view if it holds `div()`, `.child(`, `canvas(`, `impl Render` or `impl Element`, and as logic naming gpui if it mentions gpui and renders nothing. Desktop-shell files are a hand-picked name list in the script. Total 166,713 lines.

| Bucket | Lines | Share | Largest files |
|---|---:|---:|---|
| Plain Rust, no gpui | 72,948 | 43.8% | `sidebar`, `spawn_form`, `tabs`, `net`, `drag`, `file_fetch`, `recover`, `dashboard_rows`, `session_actions` |
| Logic naming gpui types, renders nothing | 10,622 | 6.4% | `notify`, `file_fetch_view`, `viewer_tab`, `hosts`, `fonts`, `term_input` |
| Views | 66,225 | 39.7% | `settings_view`, `lib`, `session_menu`, `grid_view`, `spawn_view`, `sidebar_view`, `chat_view`, `term_view` |
| Desktop shell | 16,918 | 10.1% | `update`, `connection_picker*`, `open`, `popouts`, `driver/*`, `shell_view`, `ssh` |

So about half (50.2%) carries over as logic, unchanged: the models (`sidebar`), chat rules (`chat`, `chat_tail`, 1,352 lines), terminal state (`term`, 1,252 lines), Markdown blocks (`md_blocks`), and the connection (`net`, `connection`). All 166,713 lines already compile for iOS, so "rewritten" below does not mean "will not build".

The views split further than the buckets show. The spike drew `sidebar_view` unchanged and it reads well: rows are about 52 pt tall, a fair thumb target, but `Ctrl N` on the Session button and anything that waits on hover are desktop talk.

Those 66,225 lines need a phone layout and touch targets, not a new toolkit. `lib.rs` (5,223) is `RootView`, whose `render` assumes a desktop window; the phone needs its own root around the same state. How much of `settings_view` a phone wants is an owner's call.

## Risks

- **Experimental backend, single upstream.** #63068 calls itself initial support. Native edit menus (selection handles, the paste menu) and networking and TLS workarounds are deferred upstream, and Return plus shortcuts are not delivered. Nothing in Zed's repo shows Zed itself running on it, so what keeps it working is the crate's own tests and its contributors: a revision can break it unannounced.
- **A git pin brings churn.** 8,655 commits between our version and this pin, no releases, no changelog beyond PR bodies. Zed's `[patch.crates-io]` does not reach a dependent, so `async-task` and `calloop` forks must be repeated in our root manifest and kept in step. Zed's `Cargo.toml` is the only record of the right revisions. The lock had to be regenerated whole (`futures` and `js-sys` conflicts), dropping from 932 to 886 packages.
- **Compile-clean is not behaviour-clean for the desktop.** The `secondary-` panic shows it. Layout and text changes the compiler says nothing about are in range: UAX #14 line breaking (#64624), Taffy to 0.15 (#65950), pixel snapping (#54728), 30 fps throttling of unfocused windows (#52970), and subpixel text (#45423). The desktop was compiled, never run, at this revision: the upgrade needs a full `native-shot` and live-tier pass.
- **TLS.** `rustls-native-certs` 0.8.5 compiles on iOS through its plain `unix` path (`cfg(all(unix, not(target_os = "macos")))`), which reads certificate files iOS does not have, so `net.rs`'s tailnet dial would find no roots at runtime. A phone needs `rustls-platform-verifier` or bundled roots. Read from the crate's source, not run.
- **No child processes.** `apps/native/src/ssh.rs` spawns `ssh` through `tokio::process::Command`; `crates/tailnet/src/cli.rs` spawns `tailscale`. iOS allows neither. The owner's SSH option needs an in-process client (a Rust crate such as `russh`, not evaluated here), with its own key handling. The bridge's protocol is unchanged, so `net.rs` can sit on it.
- **Device tokens.** `crates/daemon-client/src/device_tokens.rs` gates its store on `any(windows, target_os = "macos")`; on iOS it takes the `not(any(...))` branch at line 159, with no store. A Keychain implementation is new, small work.
- **Render rate and battery.** About 42 renders per second while a spinner shows is fine on a Mac and unmeasured on a phone. `Animation`'s max FPS (#62579), `WindowOptions`' Low Power Mode and thermal intervals (#66281) and the phase-locked spinners (#62332) are the tools.
- **Terminal input on a phone.** `term_input.rs` builds on key events, and a phone has only text plus whatever on-screen key row the app draws. Worth a spike of its own before promising the full terminal.
- **Accessibility.** Rendering is all GPU; AccessKit (#56065) is new upstream and is not known to be wired on iOS. `idb describe` was not tried here.
- **Licences.** Zed's GPUI crates are Apache-2.0. The Android route's crate below is AGPL-3.0.

## Android

Zed has no Android backend at this revision: no `crates/gpui_android`, though `WindowInsets`' docs and `ScrollPhysics::android()` already name it (#60496, #63505). The community route is [itsbalamurali/gpui-mobile](https://github.com/itsbalamurali/gpui-mobile): 494 stars, last pushed 2026-10-23, Vulkan or GL through wgpu, `cargo-ndk` and a custom `GpuiInputActivity` for IME composition.

It pins `gpui-pre` `=0.3.6`, a third-party publication rather than Zed's git, and is **AGPL-3.0**, which matters for a product released to others. Not built, as the brief says. [maddada/Ghostex](https://github.com/maddada/Ghostex) (1,632 stars) advertises GPUI iOS and Android apps with a Tailscale connection, which suggests the path is walkable; its claim, not checked here.

A phone client on GPUI writes its layout once. So Android can follow on `gpui-mobile` or a future Zed backend at the cost of that seam alone, which is why waiting costs little.

## Recommendation

**Go for iPhone, wait on Android.** The owner's "same as desktop except in a different UI form factor with all the same innards" holds at the code level: half the client is plain logic that compiled untouched, and the real list and real text field ran after four small fixes, all listed above.

The order that follows:

1. Upgrade the desktop's GPUI on its own, as an ordinary landing with a full visual and live pass, since every later step sits on it. The branch's upgrade edits are the starting point. 50 test and example errors remain, listed below.
2. A phone root around `RootView`'s state, with TLS roots, a Keychain token store and Return handled as above, on a physical iPhone through TestFlight.
3. Then chat and terminal, each with the usual spec first. Push by the outbound relay the owner allowed is its own design, unaffected by the toolkit.

Move the pin by decision, about once a quarter, not per commit. If Zed drops or breaks `gpui_ios`, a pinned revision still builds; the cost is then carrying the backend ourselves, which is about 3,300 lines.

## Not measured

- A physical iPhone: built and linked only. Simulators render through the Mac's GPU, so frame timing here says little about a phone.
- Memory, battery and thermal behaviour.
- Marked text and dictation, the emoji keyboard, selection, the paste menu, and landscape (the example allows portrait only).
- A real daemon, TLS, or the tailnet from the phone: the fake daemon is in-process.
- Long-press, since no view binds it yet, and the row's right-click menus, which the desktop reaches by a mouse button a phone does not have. `sidebar_panel`'s menus render in `RootView::render`, outside the panel the example draws.
- The desktop client at this revision: `cargo check` on Windows and macOS passes for library and binary; it was never launched.

## List 1: what `apps/native` could use

From 0.2.2 to the revision, read from the PR titles and `Release Notes:` lines of the 821 commits touching `crates/gpui*` (`gh`, `git log`), keeping what has a place here. Each becomes its own issue if wanted.

### For a phone

- Touch events, long press, touch drag, platform scroll physics (#63373, #63561, #63645, #63505): long press replaces right-click on `session_menu.rs`, `pane_menu.rs`, `tab_menu.rs`; drag reorders rows in `drag.rs`.
- `WindowInsets` and `request_virtual_keyboard` (#60496): every phone screen, and `TextInput`.
- `TextInputConfiguration` (`input_action`, autocorrect, autocapitalize; #63068): `text_input.rs`: a `Send` key and no autocorrect on the composer and on branch names.
- `on_back_gesture` (#65806): the phone's navigation stack.
- Universal Links through the incoming-URL callback (#65858): the `/s/<session id>` deep link in the plan's conventions.
- Long-press tooltips (#63948): `tooltip` in `lib.rs`.
- `container_query` (#60774): one `sidebar_view`, `dashboard_compact_view` that adapts by width instead of the desktop's compact variants.
- `Window::is_visible` and `on_visibility_changed` (#64107), system wake callback (#59831): reconnect in `net.rs`, and `presence.rs`.

### For the desktop

- `Anchored::attach` and centred anchors (#47154): `session_menu.rs`, `pane_menu.rs` and `tab_menu.rs` flip by hand today.
- `Window::render_to_image` on Windows, offscreen on macOS (#63012, #66344): `examples/shot.rs` and `driver/` screenshots, replacing `PrintWindow` and the `Win32_Storage_Xps` feature.
- Accessibility identifiers and AccessKit (#61926, #56065): `driver_id.rs` and screen readers across views.
- Clipboard DIB pasting on Windows, better clipboard support (#45695, #51807): `clipboard_image.rs` reads the Win32 clipboard itself.
- System notification APIs (#61189): `notify.rs`.
- Window attention (#58779): `taskbar_badge.rs`, `toast_window.rs`.
- Windows Restart Manager shutdown (#62987): `quit.rs`, `update.rs` (the installer's restart).
- Native file drag-out on macOS (#58161, #61940): `file_fetch.rs` and `drag.rs`.
- `ListState` tail and measurement fixes (#53101, #54570, `set_item_measurement_readiness`): `chat_view.rs`, `chat_tail.rs`.
- Better div scrolling (#61797), scroll fixes (#61600): `scrollbar.rs`, `sidebar_view.rs`.
- Configurable tooltip delay (#58291): `tooltip`.
- Inset shadows, `BoxShadow` builder (#57685, #57743): `notice_view.rs` `drop_shadow`.
- Spring animations, phase-locked spinners, per-`Animation` max FPS (#62778, #62332, #62579): `status_glyph.rs`.
- Configurable inactive, Low Power Mode and thermal frame limits (#62628, #66281): `lib.rs`, the window options.
- Subpixel text, BGR layouts, line split and cursor APIs (#45423, #55174, #60831, #64185): `fonts.rs`, `term_view.rs`.
- Binary-data `Svg`, exact-size rasterizing (#52319, #62770): `runtime_icon.rs`, `assets.rs`.
- `text_edit_editable_range` (#63294): `text_input.rs`'s read-only prompts.
- Hover listeners that survive keyboard input, cursor hide on keyboard input (#63614, #55664): `term_view.rs`, `hovered_container` in `sidebar_view.rs`.
- Frame-time overlay, hang monitor, input latency histogram, benchmarks (#62749, #64944, #53822, #58191): dev tooling, `examples/diff_bench.rs`.
- Hot reload through Subsecond (#65738): UI iteration in debug builds.

## List 2: every breaking change, with its call sites

Line numbers are `main` at `66ff1332`, as rustc's first pass named them. All compile errors in the library are fixed on this branch; the test and example errors are counted and left, since the session list does not need them. The count of rounds: 151 errors, then 25 once those were fixed (32 with the library's tests), then 14, then 0.

### Compile errors in the library, first pass (151)

- `FocusHandle::focus(window)` takes `cx` too, 119 sites:   
  `appearance_view.rs` 3, `changes_view.rs` 2, `chat_view.rs` 3, `cleanup_failed_view.rs` 5, `connection_picker_view.rs` 2, `dashboard_view.rs` 3, `device_pairing_view.rs` 2, `devices_view.rs` 2, `diff_tab_view.rs` 2, `fetch_dialog_view.rs` 2, `grid_view.rs` 4, `host_path_prompt_view.rs` 2, `layout_chooser_view.rs` 1, `lib.rs` 5, `move_panes_view.rs` 3, `notice_view.rs` 8, `open_view.rs` 1, `pairing_view.rs` 3
  `pane_close_view.rs` 1, `pane_menu.rs` 3, `popout_view.rs` 1, `preset_wizard_view.rs` 1, `quick_jump_view.rs` 1, `quit_view.rs` 6, `recover_view.rs` 1, `repo_remove_view.rs` 1, `session_menu.rs` 8, `settings_view.rs` 7, `shell_view.rs` 2, `sidebar_view.rs` 1, `source_control_view.rs` 1, `spawn_view.rs` 5, `stash_view.rs` 1, `tab_bar.rs` 3, `term_view.rs` 2, `text_input.rs` 1, `themes_view.rs` 4, `toast_view.rs` 1
  `viewer_tab_view.rs` 5, `vscode_suggestion_view.rs` 2, `whats_new_view.rs` 3, `workspace_creator_view.rs` 4, `worktrees_manager_view.rs` 1
- `flex_grow()` and `flex_shrink()` take a factor, 8 sites: `connection_picker_view.rs:1268`, `dashboard_compact_view.rs:594`, `dashboard_compact_view.rs:641`, `dashboard_view.rs:1074`, `dashboard_view.rs:1589`, `dashboard_view.rs:1658`, `spawn_view.rs:2169`, `spawn_view.rs:2595`
- `Modifiers::secondary()` takes `cx`, 3 sites: `lib.rs:3029`, `term_view.rs:1599`, `term_view.rs:1730`
- `Window::blur()` takes `cx`, 1 site: `chat_view.rs:967`
- `ShapedLine::paint` takes `align` and `align_width`, 2 sites: `term_view.rs:2681`, `term_view.rs:2702`
- `LineWrapper::wrap_line` takes an `IndentAdjustment`, 1 site: `toast_view.rs:193`
- `ScrollHandle::max_offset()` returns a `Point`, not a `Size`, 4 sites: `chat_view.rs:2307`, `chat_view.rs:2824`, `scrollbar.rs:168`, `settings_view.rs:1824`
- `track_scroll` on `uniform_list` borrows its handle, 2 sites: `diff_view.rs:820`, `viewer_tab_view.rs:1027`
- `gpui::Corner` is now `gpui::Anchor`, 2 sites: `pane_menu.rs:7`, `sidebar_view.rs:46`
- `BoxShadow` gained `inset`, 1 site: `notice_view.rs:961`
- `AsyncApp::update` no longer returns a `Result` (#45809), 1 site: `notify.rs:1588`
- `ImageFormat` gained `Ico` and `Pnm`, 1 site: `paste_upload.rs:85`
- `windows` features no longer arrive through gpui, 4 sites: `folder_picker.rs:332`, `folder_picker.rs:358`, `folder_picker.rs:391`, `folder_picker.rs:410`
- The same two moves again, where the first pass listed them apart: `.flex_grow()` at `sidebar_view.rs:1250`, `.secondary()` at `viewer_md_view.rs:142`.

### Knock-on in the library: helpers that now need a mutable `cx`

Second and third pass, 39 errors: `focus()` needs `&mut App`, so helpers holding `&App`, `&Context` or no `cx` changed signature, and their callers with them. No logic changed.

- Took `window` alone, gained `cx`: `devices_view.rs` `focus_device_block_confirm`, `pairing_view.rs` `focus_pairing_dialog`, `settings_view.rs` `focus_settings_control`, `term_view.rs` `focus`, `viewer_tab_view.rs` `focus_active_viewer_tab`.
- `cx: &App` to `&mut App`: `appearance_view.rs` `focus_appearance_stop` and `move_editor_focus`, `chat_view.rs` `blur_pane`, `diff_tab_view.rs` `focus_active_diff_tab`, `viewer_tab_view.rs` `return_stranded_find_focus`.
- `cx: &Context<Self>` to `&mut`: `grid_view.rs` `focus_pane_view`, `focus_active_pane`, `focus_pane_in`, `note_body`; `popout_view.rs` `focus_window_terminal`; `lib.rs` `unfocus_popped_pane`.
- A closure now takes the `cx` it is handed instead of capturing one: `chat_view.rs:1517`, `lib.rs:2068`, `grid_view.rs:1445`, `grid_view.rs:2776`.
- `prefers_viewer` in `viewer_md_view.rs:142` has no `cx`, so it reads the platform modifier by `cfg!` rather than `Modifiers::secondary`.

### Left: tests and examples (50 distinct errors)

- this method takes 1 argument but 0 arguments were supplied, 14: `examples/diff_bench.rs:203`, `tests/ui/ui_diff_view.rs:424`, `tests/ui/ui_popout.rs:513`, `tests/ui/ui_popout.rs:638`, `tests/ui/ui_popout.rs:639`, `tests/ui/ui_popout.rs:749`, `tests/ui/ui_popout.rs:778`, `tests/ui/ui_popout.rs:1582`, `tests/ui/ui_popout.rs:1963`, `tests/ui/ui_popout.rs:2861`, `tests/ui/ui_popout.rs:3039`, `tests/ui/ui_popout.rs:3082`, `tests/ui/ui_popout.rs:3463`, `tests/ui/ui_popout.rs:3599`
- this function takes 1 argument but 0 arguments were supplied, 12: `tests/ui/ui_file_fetch.rs:521`, `tests/ui/ui_folder_fetch.rs:136`, `tests/ui/ui_host_routing.rs:561`, `tests/ui/ui_links.rs:52`, `tests/ui/ui_links.rs:58`, `tests/ui/ui_multi_host.rs:3929`, `tests/ui/ui_remote.rs:164`, `tests/ui/ui_remote.rs:211`, `tests/ui/ui_viewer.rs:68`, `tests/ui/ui_viewer.rs:75`, `tests/ui/ui_viewer_md.rs:257`, `viewer_md_view.rs:1119`
- missing field `source` in initializer of `ScrollEvent`, 7: `tests/support/mod.rs:3622`, `tests/ui/../support/mod.rs:3622`, `tests/ui/ui_chat.rs:481`, `tests/ui/ui_chat.rs:3400`, `tests/ui/ui_ellipsis.rs:243`, `tests/ui/ui_history.rs:174`, `tests/ui/ui_spawn_combobox.rs:535`
- missing field `prefer_character_input` in initializer of `KeyDownEvent`, 4: `tests/support/mod.rs:3763`, `tests/support/mod.rs:3772`, `tests/ui/../support/mod.rs:3763`, `tests/ui/../support/mod.rs:3772`
- borrowed data escapes outside of function, 3: `examples/shot.rs:1594`, `quick_jump_view.rs:384`, `tests/ui/ui_themes_edit.rs:605`
- no method named `block` found for struct `BackgroundExecutor` in the current sco, 3: `folder_picker.rs:510`, `folder_picker.rs:517`, `folder_picker.rs:532`
- no associated function or constant named `new` found for struct `gpui, 2: `examples/diff_bench.rs:37`, `examples/shot.rs:181`
- mismatched types, 2: `examples/shot.rs:852`, `examples/shot.rs:1136`
- this method takes 2 arguments but 1 argument was supplied, 2: `tests/ui/ui_diff_view.rs:29`, `text_input.rs:1499`
- borrowed data escapes outside of closure, 1: `session_menu.rs:4874`

### Silent, found by reading or running

- `KeyBinding::new("secondary-…")` and `Keystroke::parse("secondary-…")` return an error or panic at runtime, in the same revision that keeps `Modifiers::secondary`: `text_input.rs:75-78` (fixed, panicked at launch); `driver/handlers.rs:33` (`PASTE_KEYSTROKE`, a `Keystroke::parse` that now errors; left, the driver feature is off here).
- Manifest: `gpui` no longer unifies `windows` features for us; `apps/native/Cargo.toml` adds `Win32_UI_Shell_Common` (the source of the `folder_picker.rs` errors below).
- Manifest: root `[patch.crates-io]` for `async-task` and `calloop`; `gpui_platform` as a dependency; `Application::new()` is now `gpui_platform::application()`: `main.rs:173` (fixed), `examples/shot.rs:181`, `examples/diff_bench.rs:37` (left).
- macOS minimum 12 (Monterey). Rust: Zed pins 1.98.1 and uses `cfg_select!`, against the workspace's `rust-version` 1.92, which has to rise; I did not pin down the exact floor.
- Deprecated, not broken: `ScrollWheelEvent` and `on_scroll_wheel`, 12 warnings (`chat_view.rs`, `diff_view.rs`, `session_menu.rs`, `term_view.rs`).
