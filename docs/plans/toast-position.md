# Toast position

Let the user choose where the app's own toast windows appear on screen. Mock: [canvases/toast-position/index.html](canvases/toast-position/index.html), rendered as [shot.png](canvases/toast-position/shot.png).

## Settled

- **Spots:** eight anchors around the edge of the work area, clockwise from top left: top left, top middle, top right, middle right, bottom right, bottom middle, bottom left, middle left. There is no centre spot. Bottom right, today's placement, is the default, and a missing setting reads as bottom right.
- **Picker:** lane A of the mock, in Settings › Notifications: a small monitor drawing with the eight spots to click, a caption naming the chosen one, and a "Show a test toast" button. The choice waits for OK or Apply, like every setting.
- **Stacking:** the newest toast sits nearest its anchor's edge. Top-row spots stack downward and bottom-row spots stack upward. Middle-row spots put the newest toast's top on the work area's middle line and stack older ones below it, so the newest never moves as the stack grows. A stack too tall for the work area drops the oldest Finished toast first, then the oldest of any kind, as today.
- **Middle columns:** a top-middle or bottom-middle toast is centred horizontally in the work area.

## Steps

- [ ] `place` and `layout` in `apps/native/src/toast_stack.rs` take the anchor. One test per anchor pins each toast's top-left for a three-toast stack in a fixed work area, plus the overflow rule at a top and a middle anchor.
- [ ] The setting: a new `native-ui.json` field defaulting to bottom right, read through the Notifications page's pending edit and applied at OK or Apply; a missing or unknown value reads as bottom right. It is a saved-data shape change, so it carries a test that loads a v0.10.1-alpha `native-ui.json` without the field.
- [ ] The picker (lane A) in Settings › Notifications, keyboard-reachable (arrow keys move between spots), with "Show a test toast" showing a sample at the staged spot.
- [ ] Toast windows re-place at once when the applied anchor changes, on Windows and macOS (`apps/native/src/toast_window.rs`).
