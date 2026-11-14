# Mobile app (native iPhone app on GPUI)

A phone client for the daemon: check on sessions, answer them, and work in them from anywhere. The phone app is a native GPUI app that shares the desktop client's Rust (`apps/native`): the same models, chat rules, terminal and connection code in a phone layout, one more client of the same daemon.

The daemon, tracer and protocol stay the source of truth. The evidence for the route is the spike in [gpui-ios-spike.md](../research/2026-11-08-gpui-ios-spike.md); the remote boundary is [remote-access.md](./remote-access.md); native push is [push-relay.md](./push-relay.md).

The iPhone web app in `apps/pwa` is frozen: it is still built, shipped and served as it stands, and gets no new features or fixes (see "The web app (frozen)").

## Rulings

- **A native GPUI app, iPhone first.** The phone app is the desktop in another form factor, "the same as desktop except in a different UI form factor with all the same innards". The spike on Zed's experimental iOS backend says go for iPhone and wait on Android: Zed has no Android backend, and the community one is AGPL-3.0. Android follows once a backend fits, at the cost of the platform seam alone, since the layout is written once.
- **The web app is frozen.** No new features or fixes go into `apps/pwa`. Daemon-side work any phone client uses stays open: push reliability, the push relay (RT-1137), and the host side of pairing (PW.3b). The Expo / React Native route in [native-mobile-path.md](../research/2026-11-07-native-mobile-path.md) did not weigh GPUI and is set aside.
- **Scope, in phases:** monitor and respond first (Home and its Needs you tray), then the session screen with its chat, then the full terminal.
- **Reach: Tailscale.** The phone runs the Tailscale app and reaches the daemon's tailnet listener (`crates/daemon/src/tailnet_listener.rs`) with its `tailscale cert` TLS, a public-CA certificate the phone's own trust store accepts with nothing pinned. The app embeds no tailnet node: an embedded node is a tailnet device of its own, which the whois owner rule would not see as the owner's phone.

  A native phone app may also send alerts through an outbound push relay (nothing comes in to the PC) and reach the PC over SSH as the desktop client does. The daemon still accepts network connections only from the tailnet: exposing it to the internet or reaching it through an inbound relay stays the "attach from outside the tailnet" non-goal in [architecture.md](../architecture.md#non-goals).
- **Per-device credentials:** each approved phone holds its own device token (TS.3b, RT-166), revocable from the native client's Settings by the block list (RT-96). The native app keeps it in the iOS Keychain, so the web app's storage-eviction re-pair case does not arise.
- **Audience and distribution:** the owner is in the Apple Developer Program, which signs the macOS release, so the native app uses a signed iOS build, TestFlight and APNs. These phases ship to the owner through TestFlight internal testing, which needs no App Review.

  A public listing needs its own design first: App Review wants a reviewer to reach a live backend and an app that works without installing another app (guidelines 2.1 and 4.2.3(i)), which a tailnet-only app needing the Tailscale app does not meet without a demo mode.
- **Push:** native push goes through the relay [push-relay.md](./push-relay.md) designs (RT-1137), which holds the Apple and Firebase keys that every user's self-hosted daemon cannot hold; the payload is encrypted daemon to phone and the app's notification extension decrypts it. The daemon's own Web Push for the web app is unchanged (see "Push").
- **Recent output:** for monitor and respond, the daemon renders the session's screen to text (the visible screen and the last lines) and sends that, not raw scrollback.
- **Size:** when the phone and the desktop show the same session, the PTY takes the size of the client that last sent input to it, or of the phone when its terminal opens (RT-184); Claude's terminal UI redraws on each switch.
- **What the app shows and how it alerts** is in [off-orca.md](./off-orca.md) (sections 2, 3 and 5).

## Phone screens

Owner rulings from the lane mocks under `docs/plans/canvases/` (RT-143), made for the web app and kept for the native one; the phone borrows from herdr's collie (`X:\dev\_ext\AltanS\collie`).

- **Home** (`mobile-home`, lane C): collie's grouped list, whose rows never move when their state changes, with a swipeable Needs you tray on top holding one compact card at a time. Yes/no and permission prompts are answered from the tray; anything longer opens the session. The app's badge counts Needs you only.
- **Session screen** (`mobile-session`, lane C): the conversation renders as chat, and the live terminal sits behind a Terminal toggle at the right end of the header, hidden for an ended session and for one whose chat is not mirrored, which shows the terminal as its main view. With no prompt pending, Quick and Keys sit behind composer buttons as in collie; a menu no hook reports falls back to the keys.
- **Chat look** (`mobile-chat`, lane A): collie's stream. Your turns are bubbles and Claude's prose is plain; each tool call is a one-line card (an Edit opens to its diff, a Bash shows its last output lines), and four or more calls in a row fold into one row naming the counts and the newest step.
- **Prompts** (`mobile-prompts`, lane C): a pending prompt rises as a bottom sheet over the chat, terminal and composer, with the answer buttons (each typing the option's number, never its label), Esc, up and down; the header's Terminal toggle shows Claude's dialog as drawn; a swipe puts it away. An answered prompt becomes a one-line receipt in the chat.
- **Pairing** (`mobile-pairing`, lane B): the phone asks; a Windows toast on the host PC names the device and its Tailscale login, and Approve shows that request's code. The native app pairs through the desktop client's Pair dialog (`device_pairing.rs`), with the code typed: it has no camera scanner.

  Settings → Remote access keeps a Waiting devices row where a missed toast is found again, and a code minted by `rustling-tulipd pair` over SSH approves a phone with no one at the PC.

## Native app design

These are the native app's settled design decisions; the phases below build them.

- **The phone's code lives in the desktop library.** Phone views are a `phone` module of `rustling_tulip_native` (`apps/native/src/phone/`), compiled on every platform, so the UI specs drive them on Windows against the fake daemon and `native-shot` draws them at a phone's size. The iOS app is its own workspace member, `apps/ios` (package `rustling-tulip-ios`), a thin `main` over `phone::run`; it builds on Windows too, so the workspace stays one build.

  Reason: every phone view is then tested where the rest of the client is, and only the device-only parts need the Mac.
- **`RootView` stays the state owner.** The phone root renders `RootView`'s state in its own layout: the message folds (`on_message`, `fold_chat_or_agents`) and the per-host model are not forked. Reason: the owner's "all the same innards"; a second root with its own folds would drift from the desktop's.
- **The phone has no This PC and no layout home.** It starts no local daemon and never runs `ensure_running`; every host it connects is a saved remote host. It shows one session at a time, so it sends no tab or layout message and keeps no tabs. It sends no presence report, so the at-desk hold reads only desktops.

  Its OS notification, sound, taskbar badge and toast-window seams are no-ops while the app runs: notices draw inside the window, and alerts come by push.
- **Hosts on the phone are typed.** The phone adds a tailnet host by its MagicDNS name and port, as the desktop's add form does, and pairs through the Pair dialog. It has no "On your tailnet" scan, which runs the `tailscale` CLI, and an iPhone app cannot start a child process.
- **TLS on iOS uses the platform verifier.** A tailnet dial on iOS trusts the system trust store through `rustls-platform-verifier`; `rustls-native-certs` reads certificate files iOS does not have. The desktop keeps its native-certs path, which its tests and the live tier prove.
- **The device token store on iOS is the Keychain.** `KeyringTokens` gains an iOS store from `apple-native-keyring-store`, under the same service and user names. The config dir on iOS is under the app container's `Library/Application Support`.
- **Return is text on iOS, so a lone `\n` is the Return action.** `gpui_ios` delivers Return as an inserted `"\n"`, never as a key event. `TextInput::replace_text_in_range` treats an insertion that is exactly `"\n"` as Return on every platform: `Submit` in a single-line field, a line break in a multi-line one (the desktop's Enter rule there). A pasted block holding newlines arrives whole and is text. The phone's composer submits with a Send button.

  Reason for every platform: the desktop never inserts a lone `"\n"` through the input handler, since Enter is a key binding there, so one code path serves both and is tested on Windows.
- **Fields ask for the keyboard.** A tap on an editable `TextInput` focuses it and calls `window.request_virtual_keyboard()`, and `text_length_utf16` reports the text's UTF-16 length, without which iOS raises no keyboard. A read-only input accepts no text input, so iOS raises no keyboard for it. iOS delivers Return as an inserted lone `"\n"` with no range: a single-line input submits on it, committing any composition, and a multi-line input breaks the line. Fields start with every keyboard assistance off (autocorrect, autocapitalize, suggestions), which branch names, hosts and codes want; `TextInput::set_text_input_configuration` turns it on where it helps, and the composer's action key reads Send.
- **Paste is a button.** The iOS backend has no edit menu and delivers no hardware-keyboard shortcut, so the composer and the terminal's key bar carry a Paste button over GPUI's clipboard API.
- **Reconnect on return to the foreground.** iOS suspends a backgrounded app and its sockets, so the phone redials every saved host that is not connected the moment it is visible again, skipping the backoff wait, and relies on push for alerts while away.
- **SSH on the phone is an in-process client.** The phone reaches the PC first through the tailnet listener, which needs nothing new. SSH follows as its own phase through `russh` behind the `open_ssh` seam (`NetDeps.open_ssh`), speaking the same bridge protocol.

  The phone makes its own ed25519 key in the Keychain and shows its public half to add to the PC's `authorized_keys`, trusts a host key on first use and refuses a changed one, and takes `user@host[:port]` only, since the phone has no SSH config. The desktop keeps spawning `ssh`, which honours the user's SSH config, agent and known hosts.
- **Push needs a tailnet host.** A phone registers for push over a tailnet connection, whose device record the daemon keys the subscription to; an SSH bridge connection is the owner's `Local` login with no device record, and stays so. A PC the phone reaches only over SSH sends it no push; the phone has the Tailscale app in any case, so adding that PC as a tailnet host too brings its pushes.

  Reason: the device-token check stays in one place, and an SSH device identity would be a second pairing scheme.
- **Portrait iPhone only.** The app declares iPhone and portrait, as the spike did. Landscape and iPad are not designed here.
- **The GPUI pin.** The client takes GPUI from Zed's git at one revision, since the crate has had no crates.io release since 0.2.2. It moves by decision, about once a quarter, never per commit, with the desktop pass of MA.5 each time. If Zed drops or breaks `gpui_ios`, a pinned revision still builds.
- **The first-mouse fix rides on a fork.** At the pin the vendored text-measure fix is upstream (each measure truncates its own copy of the runs), but the first-mouse fix is not: `gpui_windows` answers `WM_MOUSEACTIVATE` without marking the press, and `gpui_macos` marks only a left press. It is carried as one commit on the owner's fork of Zed at the pin, `leftos/zed`, and every GPUI crate comes from that fork's revision; `vendor/gpui` goes.

  Reason: a path copy of a crate from Zed's workspace needs its workspace-inherited manifest rewritten by hand and its sibling dependencies pointed back at the git source on every pin move, while a fork keeps one commit to rebase and lets Cargo resolve the workspace. The fix is offered upstream when the owner chooses; a pin that carries it drops the fork.
- **Builds and distribution.** The Mac mini builder builds the iOS targets, and `tools/ios/` holds the scripts: a simulator run that installs, launches and screenshots the app (the agents' runtime check), and a signed device build uploaded to TestFlight.

  The signing set (an Apple Distribution certificate, an App Store provisioning profile with the Push Notifications capability, an App Store Connect API key) lives in Private Config on G:, as the macOS set does. The bundle id is `dev.leftos.rustling-tulip.phone`, which the relay's APNs topic names.
- **The notification extension is Rust.** The notification service extension that decrypts a relay push is built with cargo like the app, an `objc2` subclass of `UNNotificationServiceExtension` linking `push-crypto`, so the iOS build stays cargo-only. A Swift extension calling `push-crypto` through a C ABI is the fallback if a hand-assembled Rust `.appex` will not load.
- **What needs a real iPhone.** The simulator renders through the Mac's GPU and shares the Mac's network, so a real device, the owner's tailnet, and battery, memory and frame cost are proved only on the owner's iPhone. Those steps are hand checks for the owner, and their results are recorded under "Measured on a real iPhone" below.

## Phases

Item ids are MA.1, MA.2, … (the native mobile app), distinct from the web app's PW ids, the relay's PR ids and the macOS plan's M0–M4. Each step is one brief and names its proving command. Steps that touch `crates/protocol/src/lib.rs` or `crates/daemon/src/server.rs` run one at a time; none here plans to. Each step that adds a source file adds its row to `tools/test-map.json`.

### Phase 1: the GPUI upgrade (branch `feat/gpui-upgrade`)

`branch: feat/gpui-upgrade`. Every later step sits on this upgrade, so it lands on its own as a desktop change, with a full visual and live pass, merged by `/ship`.

Risks the spike left open: compile-clean is not behaviour-clean for the desktop (the `secondary-` keystroke panic shows it); line breaking (UAX #14), Taffy 0.15, pixel snapping, subpixel text and the frame limit on unfocused windows change what the desktop draws with no compile error; the pinned revision is experimental upstream with no changelog; and the lock is regenerated whole, moving other crates' versions.

- [x] **MA.1 Take the spike's GPUI upgrade onto `feat/gpui-upgrade`.** Cut `feat/gpui-upgrade` from `main` and cherry-pick the spike's WIP commit `a4376cf3` (local branch `rt-1152-gpui-ios`), leaving out what only the phone example needs: `apps/native/examples/phone_sessions.rs` and its plist, the `gpui_ios` target dependency, `RootView::phone_session_list` and `phone_list_scroll_y`, and the `RT-1152` source comments.

  Resolve it against what `main` gained since the spike's base, migrating every new call site by the spike note's List 2 patterns (`focus(window, cx)`, `flex_grow(1.)`, `Modifiers::secondary(cx)`, `gpui::Anchor`, `gpui_platform::application()`, the `windows` features). The `[patch.crates-io]` `gpui` entry is left for MA.4.

  Files: `Cargo.toml`, `Cargo.lock`, `apps/native/Cargo.toml`, `crates/agents/claude/accounts/src/locks.rs`, and the library files List 2 names plus those `main` added since: a mechanical sweep of one pattern list, so the file bound does not apply.

  Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/ma1-check.log --timeout 3600 --slot heavy -- cargo check --workspace --lib --bins --all-features`

  Waits on: nothing. Owner's iPhone: not needed.
- [x] **MA.2 Fix the upgrade's test and example targets.** The spike left 50 errors in tests and examples (its List 2, "Left"): `ScrollEvent` needs `source`, `KeyDownEvent` needs `prefer_character_input`, `gpui_platform::application()` in the examples, added arguments and borrows that escape. Fix those and any `main` added since; a spec that now fails is fixed when the failure is the test's, and booked as an **add** when it is the product's.

  Files: `apps/native/tests/support/mod.rs`, the `apps/native/tests/ui/` specs List 2 names, `apps/native/examples/shot.rs`, `apps/native/examples/diff_bench.rs`, and the test modules of `viewer_md_view.rs`, `quick_jump_view.rs`, `session_menu.rs`, `folder_picker.rs` and `text_input.rs`: a mechanical sweep.

  Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/ma2-native.log --timeout 3600 --slot heavy -- cargo nextest run -p rustling-tulip-native --cargo-profile test-release`

  Waits on: MA.1. Owner's iPhone: not needed.
- [x] **MA.3 Clear the upgrade's warnings and runtime traps.** Move `ScrollWheelEvent` and `on_scroll_wheel` to `ScrollEvent` and `on_scroll` (`chat_view.rs`, `diff_view.rs`, `link_menu_view.rs`, `session_menu.rs`, `term_view.rs`), and replace the driver's `secondary-v` keystroke (`driver/handlers.rs`), which `Keystroke::parse` now refuses.

  Raise `rust-version` in `Cargo.toml` and `.gate/toolchain.toml` to the floor the pinned GPUI needs (it uses `cfg_select!`), found by building with that floor, and keep `cargo deny check` green on the regenerated lock (`deny.toml`). The UI specs build `RootView`, which registers every key binding the app binds at launch, so a refused keystroke fails a spec rather than a launch.

  Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/ma3-clippy.log --timeout 3600 --slot heavy -- cargo clippy --workspace --all-targets --all-features -- -D warnings`

  Waits on: MA.2. Owner's iPhone: not needed.
- [x] **MA.4 Carry the first-mouse fix on the pinned revision and drop `vendor/gpui`.** Before the brief, the orchestrator confirms with the owner the creation of the fork `leftos/zed` (a GitHub write).

  Re-apply the first-mouse part of commit `1d1b92cb9` to `gpui_windows` (mark the press after an `HTCLIENT` `WM_MOUSEACTIVATE`) and `gpui_macos` (mark and clear on the next press of any button) as one commit on the fork at the pin, and take `gpui`, `gpui_platform` and later `gpui_ios` from the fork's revision.

  Remove `vendor/gpui`, the workspace `exclude` and the `[patch.crates-io]` `gpui` entry, and allow the fork's git source in `deny.toml`. The text-measure fix is upstream at the pin; `ellipsis_real_text` re-proves it in MA.5's smoke run. The docs sweep updates the Vendored gpui row in `docs/architecture.md`, the Stack line in `docs/native-client.md` and CLAUDE.md's layout line.

  Files: `Cargo.toml`, `Cargo.lock`, `apps/native/Cargo.toml`, `deny.toml`, `vendor/gpui/` (deleted).

  Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/ma4-terminal.log --timeout 3600 --slot heavy -- cargo nextest run -p rustling-tulip-native --cargo-profile test-release --test ui ui_terminal`

  Waits on: MA.3. Owner's iPhone: not needed.
- [x] **MA.5 Pass the desktop checks and merge the upgrade.** Run the full suite, then `.\rt.ps1 native-e2e`, `.\rt.ps1 native-smoke` and `.\rt.ps1 native-drive-check` (each called bare, since `rt.ps1` gates itself), `pwsh -NoProfile -File tools/rt-mac.ps1 test` for macOS, and every `.\rt.ps1 native-shot` view on `main` and on the branch. The orchestrator shows the owner each view whose shot changed; a change the owner does not accept is a fix on the branch or an **add**.

  Hand check for the owner on the Windows PC and the Mac (not the iPhone): a click that brings the window forward answers no question, wheel and trackpad scrolling in the terminal, chat and sidebar, text crispness, a pop-out's terminal while its window is unfocused (the frame limit), and IME in a field. Then `/ship` merges `feat/gpui-upgrade` with its changelog bullet.

  Proof: `.\rt.ps1 native-e2e`

  Waits on: MA.4. Owner's iPhone: not needed; the owner's desktop hand check is.

### Phase 2: the phone app's foundations

Risks the spike left open: the iOS backend is experimental, from a single upstream, and has no native edit menu (selection handles, the paste menu) and no hardware-keyboard shortcuts; nothing has run on a real iPhone; TLS roots and the Keychain store are untested on iOS; Return is never a key event.

- [x] **MA.6 Build the phone app for the iOS simulator.** Add the workspace member `apps/ios` (package `rustling-tulip-ios`): a `main` calling `rustling_tulip_native::phone::run`, its `Info.plist` (iPhone, portrait, bundle id `dev.leftos.rustling-tulip.phone`), and `gpui_ios` under `cfg(target_os = "ios")` for the status bar style. `phone::run` opens one full-screen window padded by `window.insets()`, drawing an empty state.

  `tools/ios/build.ps1` runs on the Mac: it builds for `aarch64-apple-ios-sim` or `aarch64-apple-ios`, assembles the `.app` and signs it ad hoc for the simulator. `tools/ios/sim-run.ps1` runs on this PC as one pinned heavy gate run on `builder@mac-mini`: it builds, boots a simulator, installs and launches the app, waits for its first log line and copies a screenshot back under `.tmp/ios/`. Both iOS Rust targets are installed on the Mac builder already, and `tools/ios/build.ps1` adds a target when it is missing. `tools/ios/sim-run.ps1` is the gated build-and-launch proof; `tools/sim/sim.ps1` boots, screenshots and drives a simulator over ssh, outside the gate.

  Files: `Cargo.toml`, `apps/ios/Cargo.toml`, `apps/ios/src/main.rs`, `apps/ios/Info.plist`, `apps/native/src/phone/mod.rs`, `tools/ios/build.ps1`, `tools/ios/sim-run.ps1`.

  Proof: `pwsh -NoProfile -File tools/ios/sim-run.ps1`

  Waits on: MA.5. Owner's iPhone: not needed.
- [x] **MA.7 Make `TextInput` work with the soft keyboard and Return.** Test `text_length_utf16` (characters outside the BMP, such as emoji, count two units), call `window.request_virtual_keyboard()` in `TextInput::on_mouse_down`, treat an inserted lone `"\n"` as Return (`Submit` in a single-line field, a line break in a multi-line one; a pasted block stays text), and give `TextInput` a `TextInputConfiguration` setter (action key label, autocorrect, autocapitalize).

  Files: `apps/native/src/text_input.rs`.

  Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/ma7-input.log --timeout 1800 --slot heavy -- cargo nextest run -p rustling-tulip-native --cargo-profile test-release --lib text_input`

  Waits on: MA.5. Owner's iPhone: not needed (MA.12 checks it on the device).
- [x] **MA.8 Give iOS its TLS roots, Keychain token store and config dir.** `net.rs`'s `os_tls_config` builds from `rustls-platform-verifier` under `cfg(target_os = "ios")`; `KeyringTokens::open` and `install_store` gain the iOS Keychain store from `apple-native-keyring-store` (the step confirms the crate's feature for it); `daemon_client::config_dir` resolves to the app container's `Library/Application Support` on iOS. A target with no store still bails as now.

  Files: `apps/native/Cargo.toml`, `apps/native/src/net.rs`, `crates/daemon-client/Cargo.toml`, `crates/daemon-client/src/device_tokens.rs`, `crates/daemon-client/src/lib.rs`.

  Proof: `pwsh -NoProfile -File tools/rt-mac.ps1 check -p rustling-tulip-ios --target aarch64-apple-ios-sim`

  Waits on: MA.6. Owner's iPhone: not needed (MA.12 proves TLS and the Keychain at run time).
- [x] **MA.9 Build the phone root over `RootView` with no This PC.** `RootDeps` gains the phone shape: no local host and no `ensure_running`, saved remote hosts connected at start, no tab or layout message sent, no presence report, and no-op notification, sound, badge and toast-window seams.

  The phone root draws the session list (`sidebar_panel` with `SidebarChrome::Phone`, at the window's width inside the safe area) while a saved host's connection is open, and otherwise a first-run screen to add a tailnet host by name and port and open the Pair dialog; a tailnet host with no token offers Pair.

  The step was split into MA.9a (the phone shape on `RootDeps`), MA.9b (add a host and Pair) and MA.9c (the session list). Beyond the files listed, the step touches `toast_window.rs`, `connection.rs`, the test support and `examples/shot.rs` (MA.9a) and `sidebar.rs` and `sidebar_view.rs` (MA.9c).

  Files: `apps/native/src/lib.rs`, `apps/native/src/hosts.rs`, `apps/native/src/phone/mod.rs`, `apps/native/src/phone/hosts_view.rs`, `apps/native/src/sidebar.rs`, `apps/native/src/sidebar_view.rs`, `apps/native/tests/ui/ui_phone_root.rs` (with its module line in the `ui` binary).

  Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/ma9-phone-root.log --timeout 1800 --slot heavy -- cargo nextest run -p rustling-tulip-native --cargo-profile test-release --test ui ui_phone_root`

  Waits on: MA.6, MA.7. Owner's iPhone: not needed.
- [x] **MA.10 Reconnect when the phone app returns to the foreground.** A pure rule in `phone/lifecycle.rs` (visible again: redial every enabled saved host whose connection closed or failed, skipping the backoff wait; hidden: nothing sent), wired from the window's visibility callback in `PhoneRoot` (`apps/native/src/phone/mod.rs`) to the existing Reconnect now path (`RootView::redial_saved_hosts`, `NetCommand::Restart`). A host that is open, connecting, on its first attempt, auth-failed or not approved is left alone.

  Files: `apps/native/src/phone/lifecycle.rs`, `apps/native/src/phone/mod.rs`, `apps/native/src/hosts.rs`.

  Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/ma10-lifecycle.log --timeout 1800 --slot heavy -- cargo nextest run -p rustling-tulip-native --cargo-profile test-release --lib phone::lifecycle`

  Waits on: MA.9. Owner's iPhone: not needed (MA.12 checks it on the device).
- [x] **MA.11 Sign the device app and upload it to TestFlight.** This is the relay's "native app's signing step". Once, by the owner: the App ID `dev.leftos.rustling-tulip.phone` with the Push Notifications capability, its App Store Connect app record, an Apple Distribution certificate, an App Store provisioning profile and an App Store Connect API key that may upload builds, saved in Private Config on G:.

  Then `tools/ios/build.ps1 -Target device -Sign -BuildNumber <n>` builds `aarch64-apple-ios` in release, embeds the profile and signs with the `aps-environment` entitlement and a Keychain access group the notification extension will share. `tools/ios/testflight.ps1` reads the signing set into its own process only and runs the build and the upload on the Mac as one pinned heavy gate run, the build number the UTC time `yyyyMMddHHmm`, computed on the PC, so it rises each upload.

  Files: `tools/ios/build.ps1`, `tools/ios/testflight.ps1`, `apps/ios/Info.plist`, `apps/ios/app.entitlements`, `tools/installer/README.md` (iOS signing section).

  Proof: `pwsh -NoProfile -File tools/ios/testflight.ps1`

  Waits on: MA.8, MA.9, MA.10. Owner's iPhone: not needed; the owner's one-time Apple setup is.

### Phase 3: the first real iPhone and daemon

- [ ] **MA.12 First run on the owner's iPhone against the PC's daemon.** Hand check for the owner, with the TestFlight build and the Tailscale app on: add the PC by its MagicDNS name, pair with the code from the host's approval (or `rustling-tulipd pair`), see the session list, scroll and tap; type into the host field with the soft keyboard, Return included; lock and unlock the phone and switch apps, the list reconnecting; quit and relaunch, the token surviving in the Keychain.

  Then leave the list open for an hour with sessions working and note the battery drop, the phone's warmth and the app's render rate from its log. The orchestrator records the results under "Measured on a real iPhone" and books each failure as an **add**.

  Proof: the owner's hand check.

  Waits on: MA.11. Owner's iPhone: needed.

### Phase 4: phone screens

These wait on MA.12, so no screen is built before the device and the connection are known to work. Risks the spike left open: the render rate the simulator showed while a spinner runs, and its battery cost on a phone; terminal input from a soft keyboard, which has no Esc, Ctrl or arrows; accessibility, since AccessKit is not known to be wired on iOS; marked text, dictation and the emoji keyboard, unmeasured.

- [ ] **MA.13 Build the phone's Home.** The Home ruling: the grouped list whose rows never move, the Needs you tray above it with one compact card at a time and swipe between cards, yes/no and permission answers typed as the option's number from the tray (the desktop's Needs you answer path), a row's long press opening Stop and Resume in a bottom sheet, and a tap opening the session. Touch targets at least 44 pt; no shortcut hints or hover-only affordances.

  Files: `apps/native/src/phone/home_view.rs`, `apps/native/src/phone/tray.rs`, `apps/native/src/phone/root_view.rs`, `apps/native/examples/shot.rs` (view `phone-home`, a portrait phone viewport with the spike's measured insets), `apps/native/tests/ui/ui_phone_home.rs`.

  Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/ma13-home.log --timeout 1800 --slot heavy -- cargo nextest run -p rustling-tulip-native --cargo-profile test-release --test ui ui_phone_home`

  Waits on: MA.12. Owner's iPhone: not needed (MA.16 checks it on the device).
- [ ] **MA.14 Build the phone's session screen with its chat.** The header (back, label, status, the Terminal toggle), the desktop's chat view at the phone's width in the chat look ruled above, the composer (a multi-line `TextInput` with Send, Paste, Quick and Keys buttons), the prompt bottom sheet (answer buttons typing the option's number, Esc, up, down; a swipe puts it away), and back by the button or the back gesture.

  Files: `apps/native/src/phone/session_view.rs`, `apps/native/src/phone/prompt_sheet.rs`, `apps/native/src/chat_view.rs`, `apps/native/src/phone/root_view.rs`, `apps/native/examples/shot.rs` (view `phone-session`), `apps/native/tests/ui/ui_phone_session.rs`.

  Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/ma14-session.log --timeout 1800 --slot heavy -- cargo nextest run -p rustling-tulip-native --cargo-profile test-release --test ui ui_phone_session`

  Waits on: MA.13. Owner's iPhone: not needed (MA.16 checks it on the device).
- [ ] **MA.15 Build the phone's terminal.** The Terminal toggle replaces the chat, composer and sheet with the session's terminal, a fresh attach each time it opens, claiming the PTY size for the phone (RT-184). `TerminalPane`'s input handler reports its UTF-16 length, asks for the keyboard on a tap and sends an inserted lone `"\n"` as Return (`\r`). A key bar under it carries Esc, Tab, a sticky Ctrl, the arrows, Paste and Quick, whose reply is typed into the terminal.

  Files: `apps/native/src/term_view.rs`, `apps/native/src/phone/terminal_view.rs`, `apps/native/src/phone/key_bar.rs`, `apps/native/src/phone/session_view.rs`, `apps/native/examples/shot.rs` (view `phone-terminal`), `apps/native/tests/ui/ui_phone_terminal.rs`.

  Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/ma15-terminal.log --timeout 1800 --slot heavy -- cargo nextest run -p rustling-tulip-native --cargo-profile test-release --test ui ui_phone_terminal`

  Waits on: MA.14. Owner's iPhone: not needed (MA.16 checks it on the device).
- [ ] **MA.16 Check the phone screens on the owner's iPhone.** Hand check for the owner, on a fresh TestFlight build (`tools/ios/testflight.ps1`): answer a permission prompt from the tray, open a session and read its chat, send a reply with Send and with Return in a multi-line field, paste with the Paste button, and answer a prompt from the sheet.

  Then open the terminal and type with the key bar (Esc, Ctrl+C, arrows), dictate into the composer, type an emoji, and leave a working session's chat open for an hour noting battery and warmth. The orchestrator records the results under "Measured on a real iPhone" and books each failure as an **add**.

  Proof: the owner's hand check.

  Waits on: MA.15. Owner's iPhone: needed.

### Phase 5: push through the relay

The native app's side of [push-relay.md](./push-relay.md)'s contract; the relay's own steps are PR.1 to PR.9 there (RT-1511 to RT-1519), and PR.8 is the hand check that proves a relay push end to end. Risks: `gpui_ios`'s app delegate may not expose the remote-notification callbacks, so the device token may need a method added to the delegate class at run time; a hand-assembled Rust `.appex` is unproven.

- [ ] **MA.17 Build the notification service extension.** A Rust `.appex` (`apps/ios-notify`, package `rustling-tulip-ios-notify`): an `objc2` subclass of `UNNotificationServiceExtension` that reads the push's `e`, decrypts it with `push-crypto`'s RFC 8291 decrypt using the install's key pair and auth secret from the shared Keychain access group, and parses the `PushNotice`.

  It sets the title, body and badge, and puts `host` and `session_id` in the notification's user info; on any failure it leaves the fallback alert as it came. The decrypt-and-build step is a plain function tested on Windows; `tools/ios/build.ps1` embeds the `.appex` in the app's `PlugIns`.

  Files: `Cargo.toml`, `apps/ios-notify/Cargo.toml`, `apps/ios-notify/src/main.rs`, `apps/ios-notify/src/notice.rs`, `apps/ios-notify/Info.plist`, `tools/ios/build.ps1`.

  Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/ma17-notify.log --timeout 1800 --slot heavy -- cargo nextest run -p rustling-tulip-ios-notify --cargo-profile test-release`

  Waits on: MA.11, PR.1, PR.2. Owner's iPhone: not needed (PR.8 checks it on the device).
- [ ] **MA.18 Register the phone for push.** A Notifications switch in the phone's host screen: on, it asks for notification permission, registers for remote notifications, makes or loads the install's P-256 key pair and auth secret in the shared Keychain access group (`push-crypto`), and sends `RegisterPush` with the `apns` kind and the build's environment over that host's tailnet connection.

  On each open it compares `PushStatus.device_token` with the current token and re-registers when they differ, as the web app does with its endpoint. The register-or-compare rule is a pure model tested on Windows; the platform calls sit behind a `PushPlatform` seam with a fake.

  Files: `apps/native/src/phone/push.rs`, `apps/native/src/phone/push_ios.rs`, `apps/native/src/phone/hosts_view.rs`, `apps/native/Cargo.toml`, `apps/native/tests/ui/ui_phone_push.rs`.

  Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/ma18-push.log --timeout 1800 --slot heavy -- cargo nextest run -p rustling-tulip-native --cargo-profile test-release --test ui ui_phone_push`

  Waits on: MA.12, PR.1, PR.4. Owner's iPhone: not needed (PR.8 checks it on the device).
- [ ] **MA.19 Open the session a notification names.** A tap on a notification opens the app at that session on that host (`host` and `session_id` from the user info): the phone root shows the session screen once the host's session list holds it, and says the host is not paired when it is not. The badge follows the Needs you count while the app runs.

  Files: `apps/native/src/phone/push_ios.rs`, `apps/native/src/phone/root_view.rs`, `apps/native/tests/ui/ui_phone_push.rs`.

  Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/ma19-open.log --timeout 1800 --slot heavy -- cargo nextest run -p rustling-tulip-native --cargo-profile test-release --test ui ui_phone_push`, then PR.8's hand check on the owner's iPhone.

  Waits on: MA.14, MA.17, MA.18. Owner's iPhone: needed for PR.8.

### Phase 6: SSH from the phone

Risks: a new SSH dependency (`russh`, pure Rust, its licence and advisories checked by `cargo deny`); key handling on the phone; the PC's SSH server must allow the phone's key.

- [x] **MA.20 Add the in-process SSH transport.** `ssh_inproc.rs`: connect to `user@host[:port]` with `russh`, authenticate with the phone's ed25519 key (made on first use, kept in the device token store's Keychain under its own user name), and check the host key against the saved one (none saved: report it for trust; a different one: refuse with the fingerprint).

  It runs the saved remote command (default `rustling-tulipd bridge`) on an exec channel and speaks the line protocol as `ssh.rs`'s `LineSocket` does; `NetDeps.open_ssh` uses it on iOS, with no saved host key until MA.21: an unknown key stops the connection in `AuthFailed` and sends the view `NetEvent::HostKeyUnverified { fingerprint }`, a changed one stops it with the fingerprints in the reason, and neither redials until a Restart. It compiles on every platform, so its tests run on Windows against an in-process `russh` server.

  The crypto backend is `ring` (no aws-lc, no RSA keys).

  Files: `Cargo.toml`, `Cargo.lock`, `apps/native/Cargo.toml`, `apps/native/src/lib.rs`, `apps/native/src/ssh_inproc.rs`, `apps/native/src/net.rs`, `deny.toml`.

  Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/ma20-ssh.log --timeout 1800 --slot heavy -- cargo nextest run -p rustling-tulip-native --cargo-profile test-release --lib ssh_inproc`

  Waits on: MA.9. Owner's iPhone: not needed.
- [ ] **MA.21 Add SSH hosts on the phone.** The phone's add form gains an SSH mode (`user@host[:port]`, remote command), shows the phone's public key with Copy, and on a first connection shows the host key's fingerprint with Trust and Cancel; a trusted fingerprint is saved in the host's `saved_hosts` record (an additive field).

  Then a hand check for the owner on the iPhone: add the PC over SSH, trust its key, see the session list, and see a changed host key refused.

  Files: `apps/native/src/phone/hosts_view.rs`, `apps/native/src/connection_picker.rs`, `apps/native/src/hosts.rs`, `apps/native/tests/ui/ui_phone_root.rs`.

  Proof: `pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/ma21-ssh-hosts.log --timeout 1800 --slot heavy -- cargo nextest run -p rustling-tulip-native --cargo-profile test-release --test ui ui_phone_root`, then the owner's hand check.

  Waits on: MA.20, MA.12. Owner's iPhone: needed for the hand check.

## After the MA phases

The web app's open features wait for the native app and are built there once its screens stand, each from its own issue: Linear cards (RT-142), files (RT-106), the Accounts screen (RT-112) and dictation (RT-83, which on the native app is the keyboard's own dictation key into the composer, checked in MA.16).

**Retiring the web app:** once MA.16's hand check and PR.8 pass, the web app is removed in an item of its own (`apps/pwa`, `tools/protocol-ts`, the tailnet router's static routes and the `-Pwa` build steps), since the native app then replaces it. Whether the daemon's direct Web Push sender goes with it is decided in that item.

## Measured on a real iPhone

Nothing yet: MA.12 and MA.16 fill this section.

## How it connects

```text
iPhone: native app, Tailscale app connected
  ├─ wss://{name}:{port}/ws ──tailscale cert TLS──▶ daemon tailnet listener: Hello with the device token, then the usual protocol
  └─ ssh user@host (MA.20) ──in-process SSH───────▶ rustling-tulipd bridge on the PC, the same protocol over its stdio
daemon ──outbound, encrypted body──▶ push relay ──▶ APNs ──▶ iPhone notification extension (on or off the tailnet)
```

What exists: the opt-in tailnet listener (switched by `remote.json` and `ConfigureRemote`) with `tailscale cert` TLS and certificate renewal, the owner rule in `crates/tailnet`, constant-time secret checks (`crates/daemon/src/secret.rs`), device tokens and pairing (`crates/daemon/src/server/pairing.rs`, `handshake.rs`), and the tailnet router (`build_tailnet_router_with_pwa` in `crates/daemon/src/server/remote.rs`).

The router's `/ws` upgrade accepts the web app's `Origin` exactly `https://{name}:{port}` (invariant 6 in `remote-access.md`) and a native client's upgrade, which sends no `Origin`, as the desktop's tailnet dial already does. On a tailnet connection `Hello.auth_token` is the device's own token.

The native app needs no new message for its screens: Home reads `SessionSnapshot`'s status and hook-reported `pending_input`, the session screen the chat mirror, and the terminal the PTY stream. Its push registration is the relay design's (`PushRegistration::Apns`, PR.2).

## Push

The daemon's Web Push, which the web app uses. Native push goes through the relay in [push-relay.md](./push-relay.md).

- **VAPID keys:** a P-256 key pair the daemon generates the first time push is enabled, kept in a machine-local folder beside the tailnet certificate's (`ProjectDirs::data_local_dir().join("push")`, `%LOCALAPPDATA%\leftos\rustling-tulip\data\push\`, overridable with `RUSTLING_TULIP_PUSH_DIR` for tests), never in `state.json` and never in the roaming config dir. Replacing the keys invalidates every subscription, so nothing rotates them; deleting the folder is the reset.

  The JWT's subject is the web app's origin URL (`https://{name}:{port}`), its audience the push service's origin and its `exp` under a day; one token per push service origin is cached and re-signed at most once an hour (Apple asks for no more; a bad JWT answers 403, [pwa-iphone.md](../research/pwa-iphone.md) §1).
- **One subscription per device.** The page subscribes from the tap on its Notifications switch, in the installed app (iOS requires the gesture and Home Screen install), with `userVisibleOnly: true`, and sends `register_push` over `/ws`: a `kind`-tagged `registration` (`{endpoint, p256dh, auth}` for `web_push`), the three switches (Needs you, Finished, `hold_at_desk`) and a `request_id`.

  The daemon stores it under the device the connection's token belongs to (`push/subscriptions.json`), so a new endpoint from the same device replaces the old one.

  Blocking, forgetting or re-pairing the device deletes its subscription (RT-96). Each app open compares the subscription it holds with the daemon's and re-registers when both hold one and they differ; keyed by device, a re-subscribe never duplicates. When the phone holds none it changes nothing (the Apple forum report of `getSubscription()` returning null while pushes still arrive, [pwa-iphone.md](../research/pwa-iphone.md) §1).
- **What is sent:** a session entering Needs you (`AwaitingInput`), and a session finishing (a `Stop` with nothing running) when the device's Finished switch is on. The switches live with the device's subscription on the daemon. Wave 5's attention feed (AT.3) and presence routing (AT.6) later replace the trigger.
- **Desktop activity suppresses the phone.** A per-device setting (`hold_at_desk`), on by default, holds the device's pushes while the owner is at the desk. Active is fixed, not configurable: a desktop client reports it while its window is focused and its own machine had keyboard or mouse input in the last 180 seconds, both required (the native client reads the idle time on Windows and macOS; elsewhere focus alone counts).

  The owner is at the desk while any connected client last reported active. A held Needs you push is delivered when the owner leaves the desk, if the session still needs the user; a held Finished push is dropped.
- **Declarative Web Push payload:** `{"web_push": 8030, "notification": {"title": …, "body": …, "navigate": "https://{name}:{port}/s/<session id>", "app_badge": "<Needs you count>"}}`.

  iOS 18.4 shows it with no service worker and never revokes for it; on iOS 16.4 to 18.3 the service worker's `push` handler shows the same JSON, and its `notificationclick` handler opens the `navigate` URL, closing the notification only after the open succeeds (collie's order, `web/src/sw.ts`). The payload holds the session's label and status words only, never terminal output: it shows on the Lock Screen and must stay under APNs' 4 KB.
- **Delivery options:** `TTL` of 6 hours, `Urgency: high`, and a `Topic` per session (`rt-` and 12 base64url characters of the session id's SHA-256, 15 characters) so an offline phone gets the latest alert per session. A test pins the topic rule Apple enforces: `[A-Za-z0-9_-]`, 32 characters at most, length not ≡ 1 (mod 4).
- **Errors:** 404 and 410 delete the subscription. 400 `BadDeviceToken` and other permanent failures count against a subscription only when the same push service accepted another subscription in that round, and five in a row delete it (collie's rule, `bridge/push.ts`); with one device that never fires, so it is logged instead.

  403 is the sender's fault (the VAPID JWT) and never prunes; 413 is a payload bug and is logged with its size; 429 and 5xx back off. Every failure logs the status and the push service's `reason`.
- **Sender:** RFC 8291 payload encryption (`aes128gcm`) and RFC 8292 VAPID signing with the RustCrypto `p256`, `hkdf` and `aes-gcm` crates (`ring` cannot take the fixed ephemeral key the test vector needs), sent with the daemon's `reqwest` (no redirects, no proxy, endpoints held to the push services' hosts); proved by RFC 8291's Appendix A test vector (`crates/daemon/src/push/`).

  The Windows host needs outbound HTTPS to `*.push.apple.com`; the phone needs no tailnet connection to receive.

## The web app (frozen)

The iPhone Progressive Web App in `apps/pwa`, installed from the daemon's tailnet listener with Safari's Share, Add to Home Screen; platform facts with their sources are in [pwa-iphone.md](../research/pwa-iphone.md). It gets no new features or fixes. How to install and pair it is [phone-app.md](../phone-app.md), and its hand checklist is section 13 of [remote-hand-checklist.md](../remote-hand-checklist.md).

- **Stack:** a TypeScript web UI (Vite, pnpm, oxlint, oxfmt, vitest) whose wire types are generated from `crates/protocol` by `tools/protocol-ts` (staleness test `generated_protocol_ts_is_current`); the full terminal drawn by xterm.js from the PTY stream; the service worker hand-written, as collie's is.

  Its logic sits in pure reducers behind injected seams (storage, push, QR scanning, badge, visibility), the socket on the standard `WebSocket` surface, and its stored pairing record is `{host, port, token}`.
- **Serving:** the built app ships as a `pwa/` folder read in place and never copied into the binaries cache: the daemon serves `RUSTLING_TULIP_PWA_DIR` (a test and dev override), else `pwa/` in the install folder it is given (`Dirs::bin_templates` in `crates/daemon/src/paths.rs`), else `pwa/` beside the running exe (a macOS app bundle keeps it in `Contents/Resources/pwa`). It is built by `rt.ps1 build -Pwa` and the installer, so a daemon build needs no node or pnpm.

  `GET` routes on the tailnet router only serve the page, its hashed assets, `manifest.webmanifest`, the service worker and icons, with any other path that is not a file answered by the page so a deep link (`/s/<session id>`) opens the app; the loopback router serves none of it.

  Hashed assets are `public, max-age=31536000, immutable`, everything else `no-cache`; every response carries a same-origin `Content-Security-Policy` (inline styles allowed for xterm.js) and `X-Content-Type-Options: nosniff`, since the host's `*.ts.net` name is public in Certificate Transparency logs.
- **No state-changing HTTP route.** Every action, push registration included, travels over `/ws` after `Hello`, so the device-token check stays in one place (`server/handshake.rs`).
- **A stable origin** (invariant 11): the MagicDNS name and the configured port. Changing either makes a new origin, so the phone reinstalls the web app, pairs again and subscribes again. The WebSocket dies whenever the app leaves the foreground, so the page reconnects and resyncs on `visibilitychange` and relies on push for alerts.
- **Badge:** the Home Screen icon's badge counts Needs you only. The page sets it with `navigator.setAppBadge(n)` on every snapshot while it runs, and every push carries the count in `app_badge`. A push must show a notification, so no push lowers the badge silently.
- **Accepted losses:** no home-screen widget (RT-126) and no notification actions (Safari supports none); a tap opens the app at the session.

Its phases PW.1 to PW.9 are built: the app shell, protocol types and the connection, pairing on the phone, Home, the session screen's first cut, Web Push, the badge, the install guide and hand checklist, and the full terminal.

PW.3b, the host side of pairing (RT-166), stays open, since any phone pairs through it: the Windows toast naming the device and its Tailscale login with Approve, and the Waiting devices row in Settings → Remote access (the rows and dialog are built in `apps/native/src/pairing_view.rs`; `rustling-tulipd pair` is delivered). PW.10 to PW.14 (chat, Linear cards, files, accounts, dictation) are frozen; their outcomes move to the native app (MA.14 and "After the MA phases").

## Prior art

From herdr's collie (`X:\dev\_ext\AltanS\collie`, MIT, read at 82c6bba7), which serves its PWA over Tailscale:

- **Serving.** Collie's bridge serves the Vite build from `web/dist` (`bridge/server.ts`, `WEB_DIR` and `serveStatic`) with `cacheControlFor`: hashed `assets/` immutable, everything else `no-cache`. Collie puts `tailscale serve --bg 8787` in front for HTTPS and trusts `Tailscale-User-Login`; rustling-tulip terminates TLS itself and asks whois about the real peer, so no header is trusted.
- **Pairing.** `bridge/pairing.ts`: a one-time code stored as its SHA-256 with a 10-minute life and five attempts, a 256-bit device token returned once and stored as its SHA-256, and both files re-read per request so a CLI-minted code needs no restart.
- **Push.** `bridge/push.ts`: one JSON file of subscriptions with metadata, prune on 404/410, same-origin-witnessed eviction after five failures, the device naming the endpoint it replaces, and the base64-length topic rule.
- **Terminal.** Collie draws Herdr's rendered rows and refuses an emulator in the page ([ADR 0008](https://github.com/AltanS/collie/blob/HEAD/.adr/0008-collie-does-not-run-a-terminal-emulator.md)); the native app runs the desktop's `alacritty_terminal` grid instead.

From Orca (`github.com/stablyai/orca`) and ThinkTerm (`github.com/RoversX/ThinkTerm`, GPLv3: ideas only):

- **Revocation (RT-96).** Orca gives each paired client its own token, disconnects its live connections the moment it is revoked, and replaces an unused access link when a new one is made (`docs/site/content/docs/remote-servers.mdx`).
- **Version check both ways.** Orca's handshake answers `client-too-old` or `server-too-old` with the version required, so the phone can say "update the desktop app" instead of failing (`src/shared/protocol-compat.ts`). A phone build older than its daemon is the same case.
- **Retries that apply once.** Orca's phone journals each spend of a rate-limit reset credit so a retry over a flaky link cannot spend two (`docs/site/content/docs/mobile.mdx`). Any phone action that is not naturally idempotent, such as an account switch or create-session, needs the same.
- **Push only when away.** Orca pushes to the phone only while the desktop is locked or idle for 180 s or more, and pushes anyway when it cannot read presence (`src/main/notifications/desktop-away-state.ts`).
- **Size.** ThinkTerm's browser client claims a tab at its own size, and the desktop takes it back only on interaction there (`docs/thinkterm/web-access.md`), which matches the size ruling above.

## Open questions

None open. The decisions under "Native app design" were made by the orchestrator from the spike, the research notes and the code, and are open to the owner.
