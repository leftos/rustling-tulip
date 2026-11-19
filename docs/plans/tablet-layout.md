# Tablet and landscape layout

The phone app on the iPad, in both orientations, and on the iPhone in landscape. The layout follows the window's size, not the device model, so one rule serves an iPad in portrait and an iPhone on its side. It is built from the phone's own views ([mobile-app.md](./mobile-app.md), "Native app design"): no desktop views move to the phone.

The options were put as lanes in [canvases/tablet-layout/index.html](./canvases/tablet-layout/index.html); the answers are below.

## Owner rulings

- **Devices:** the iPad, in portrait and landscape, and the iPhone in landscape. The iPad runs full screen only: no Split View or Stage Manager window sizes (`UIRequiresFullScreen` stays on).
- **iPad: two panes** (lane A). The Home list, with its Needs you tray, stays docked on the left and the selected session's page fills the right: its Chat or Terminal page under the same header and Terminal toggle as the phone. One session shows at a time and there are no tabs or splits, in both orientations.
- **iPhone landscape: terminal first** (lane 2). Home in landscape is the same two panes as the iPad: the list beside the selected session's page. A session page shown in landscape fills the screen with its terminal and key bar and no header; the key bar carries Back and the session's status.
- **Hardware keyboard and pointer on the iPad:** planned in Backlog as its own issue, not in vNext. The iPad ships touch-only first.
- **Placement:** release vNext.

## Orchestrator decisions (open to the owner)

- **One layout rule, from the window.** A pure function in the phone module, `PhoneLayout::of(viewport, insets)`, picks `Single` (the phone in portrait, as today), `TwoPane` (the usable width is at least 700 pt) or, for a session's terminal, `TerminalFull` (the usable height is under 500 pt, the iPhone on its side). It is plain Rust, tested on Windows like `content_padding`, and every phone view reads it rather than the device.
- **The list pane is 320 pt wide;** the session pane takes the rest. With no session selected the session pane says "Pick a session". Home's rows, groups and Needs you tray are unchanged, and the selected row is marked.
- **What opens where in two panes.** A row's tap selects its session in the right pane without leaving Home; a notification's tap does the same. The prompt sheet rises over the session pane only, so the list and its tray stay usable. Settings, its pages and the add-host form cover the whole window, as on the phone. The phone files viewer and browser ([native-client.md](../native-client.md) "Phone shape") open in the session pane.
- **Terminal first on the iPhone.** Rotating to landscape while a session page is open shows its terminal full screen; Back there returns to the two panes with that session's Chat page. In two panes, the Terminal toggle opens the full-screen terminal. A session with no terminal (ended, or a chat-only view) stays on its Chat page in the session pane. Rotating back to portrait returns to the page the session showed before.
- **The terminal re-claims its size on a rotation.** A rotation or a pane change that resizes an open terminal sends the new size as the phone's terminal does when it opens (RT-184), so Claude's terminal UI redraws at the new width.
- **The bundle:** `UIDeviceFamily` gains 2 (iPad); `UISupportedInterfaceOrientations` gains both landscapes, and `UISupportedInterfaceOrientations~ipad` lists all four. `tools/ios/sim-run.ps1` gains a `-Device` parameter so the same run launches on an iPad simulator.

## Steps

- [x] The layout rule: `PhoneLayout::of` and its tests, read by `PhoneRoot`.
- [x] Two panes on the iPad and in phone landscape: the list pane, the session pane and its empty state, where the prompt sheet, Settings and a notification's tap go; `native-shot` views `phone-ipad`, `phone-ipad-portrait` and `phone-landscape-home`.
- [ ] Terminal first on the iPhone in landscape: the full-screen terminal, the key bar's Back and status, the rotation rules; `native-shot` view `phone-landscape-terminal`.
- [x] The bundle and the size claim: the `Info.plist` keys, the terminal's re-claim on a resize, and `sim-run.ps1 -Device` proving a launch on an iPad simulator.
- [ ] Hand check through TestFlight on an iPad in both orientations and an iPhone on its side: rotate in a session, answer a prompt from the tray, open a terminal and type.

## Later: hardware keyboard and pointer (Backlog)

GPUI's iOS backend delivers no hardware key events (its window has no `pressesBegan`) and reports no hover. Supporting an iPad keyboard means a change to the owner's gpui fork that turns `UIPress` events into GPUI key events with modifiers, so the terminal takes keys as on the desktop and the desktop's keybindings work; and a pointer change that reports hover from a `UIHoverGestureRecognizer`. Its own issue holds the design pass.
