# Native client (replace the Tauri/WebView2 frontend)

Decision (user, 2026-09-23): move the desktop client off the web stack. The daemon, tracer and `crates/protocol` stay as they are; only `apps/tauri-app` is replaced. Chosen stack: GPUI (Zed's UI framework) + `alacritty_terminal`, with Iced as the fallback if GPUI's API churn becomes a problem. The native client can run side by side with the Tauri app against the same daemon (it accepts multiple clients), so parity is reached feature by feature before Tauri is retired.

## Spike

`spikes/native-client/` is a standalone cargo workspace (kept out of the main workspace's build, clippy and cargo-deny). It reads `daemon.json`, connects, sends `Hello`, picks a session, loads its scrollback, then streams `PtyOutput` into `alacritty_terminal` and paints the grid with GPUI. Keys, paste (Ctrl+Shift+V, bracketed when the child asks) and wheel scrollback work.

```powershell
cd spikes/native-client
cargo run --release -- <session-id>   # omit the id to attach to the first live interactive/shell session
```

Known spike limits: it sends `Resize`, so the attached session's PTY follows this window's size (the Tauri pane attached to the same session resizes it back); terminal replies (`PtyWrite`, e.g. cursor-position reports) are dropped so the child doesn't get two answers; no selection/copy, no IME, no link detection; wide glyphs are shaped per glyph, not verified against CJK/emoji.

- [x] Spike: GPUI window attaches to a daemon session and renders it with `alacritty_terminal`
- [x] Hand-test the spike against a plain-shell session and against `claude` (colors, cursor, typing echo, scrollback, resize)
- [ ] Decide the client's crate layout in the main workspace (lints, cargo-deny licenses for GPUI's tree)
- [ ] Inventory the Tauri app's features into a parity checklist (sidebar, tabs/panes, spawn dialog, source-control panel + diff view, settings, pop-out windows, notifications)
- [ ] Choose the diff-view approach (the Monaco replacement)
- [ ] Replace the WebdriverIO e2e suite's driver
