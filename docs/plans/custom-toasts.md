# The app's own toasts (RT-704)

Owner request (RT-704): a Settings choice that has the client draw its own interactive toasts, in place of the OS's. The toasts work the same on every platform and carry "Go to pane" and the pending prompt's answers.

Rulings below are orchestrator decisions (ship-fast profile), open to the owner. The code map behind them is in the RT-704 issue thread; the facts each brief needs are restated in its brief.

## Rulings

- **Setting.** `native-ui.json` `notifications.toast_style`: `"os"` (the default) or `"app"`. A missing key, an unknown value or a value that is not a string loads as `"os"`, the way `skip_when` loads (`apps/native/src/sidebar.rs` `skip_or_default`); an unknown value logs a warning, and the rest of the file still loads. A test loads a `v0.5.1-alpha`-shape `notifications` block without the key.

  The Settings › Notifications row sits under "Skip notifications when". It is labelled `Show toasts as`, with the choices `Windows notifications` and `The app's own`. Off Windows the row is not drawn until a macOS window shim exists.
- **One or the other.** With `"app"`, the Needs you and Finished toasts are the app's own and never also the OS's. Pairing toasts stay OS toasts.
- **Gate.** The same `raise_toast` gate as the OS toast: the Needs you and Finished toggles, the skip rule, and the user-busy half of `Notifier::suppressed()` (`SHQueryUserNotificationState`). The OS's per-app toast setting does not apply to the app's own toast. Under `offscreen::requested()` no toast window opens.
- **Sound.** The app's own sound plays as it does today (`sound_with_toast`). The app's toast has no OS sound at all (RT-703's rule).
- **Window.** One GPUI window per toast: `WindowKind::PopUp`, no titlebar, `show: false`, transparent background, not movable or resizable. From a short thread (GPUI's window state is borrowed inside the open callback), the window gets `WS_EX_NOACTIVATE | WS_EX_TOPMOST` with `SetWindowPos(HWND_TOPMOST, …, SWP_NOACTIVATE)` and `ShowWindow(SW_SHOWNOACTIVATE)`. `offscreen.rs`'s `never_activate` is shared, not copied. The window never calls GPUI `activate`.
- **Place.** The bottom-right of the work area (`GetMonitorInfoW` `rcWork`) of the monitor holding the main window, else the primary monitor. The newest toast is nearest the corner. Toasts are 360 logical px wide, with an 8 px gap and a 16 px margin, and at most 4 are shown. Overflow drops the oldest Finished toast first, never a Needs you while one could go instead.
- **One per session.** Toasts are keyed by (host, session id). A later toast for the same session replaces the earlier one in place and restarts its timer.
- **Durations.** Finished lasts 6 s. Needs you without answer buttons lasts 30 s. Needs you with answer buttons lasts until it is answered, dismissed (×), the session leaves `AwaitingInput` or its `status_since` changes, the session is removed, or the skip rule newly applies, with a 10-minute cap. Hovering pauses every timer.
- **Answers.** The buttons are the Dashboard's ask block (`dashboard_rows::ask_block`): Allow and Deny for a permission, and up to 3 options for a single single-choice question. They are sent with `answer_from_dashboard` (`AnswerPrompt` to `host_of_session`), so a remote host's session routes like the Dashboard's. A sent answer shows as sent until the snapshot moves. A refusal brings the buttons back, through the Dashboard's answer state. Any other prompt shows only Go to pane.
- **Go to pane.** Activates the main window, then `select_session`.
- **Hosts.** A host's toasts are dropped when it disconnects or sends a new `Welcome`. A session from a host other than This PC shows the host's name in the toast.
- **Look.** The app's theme. A rounded card, with an accent stripe by kind (Needs you uses the rail's Asking token; Finished is muted), a title line, a two-line body and the permission command in the ask block's mono box. Below that sit the buttons (choices first, then `Go to pane`), and × is top-right. No animation. The look is shown as a `native-shot toast` capture at landing, with no separate mock.
- **Protocol.** No change.

## Steps

- [ ] A: the `toast_style` setting, its Settings row and the `raise_toast` branch (still raising OS toasts for `"os"`)
- [ ] B: the plain-Rust toast stack (`toast_stack.rs`): keys, kinds, push-or-update, expiry, hover, overflow, placement maths, unit-tested
- [ ] C: the Windows shim (`toast_window.rs`): non-activating topmost window, work area per monitor, the user-busy check split out of `suppressed()`
- [ ] D1: the toast view and its RootView host: open, place, update and close windows, × and Go to pane, timers, `native-shot toast`
- [ ] D2: answer buttons, the sent and refused states, dismissal on session and host changes
- [ ] F: docs (`docs/native-client.md` Notifications, CLAUDE.md's `native-ui.json` sentence), changelog, test-map rows; then this file's rulings are promoted and it is deleted

Order: A, then B and C side by side, then D1, then D2, then F.
