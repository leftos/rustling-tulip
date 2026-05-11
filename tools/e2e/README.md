# `@rustling-tulip/e2e` — end-to-end harness for the Tauri app

This package gives you two ways to exercise the real Tauri app + daemon
end-to-end on Windows:

1. **An interactive "driver host"** that launches the app under
   `tauri-driver` and accepts JSON commands over a local HTTP socket. Use it
   from a second terminal (or from Claude via `Bash`) for exploratory testing.
2. **A WebdriverIO test runner** that runs canned regression specs against the
   same setup.

Both modes use [`tauri-driver`](https://v2.tauri.app/develop/tests/webdriver/),
which on Windows wraps Microsoft Edge WebDriver to drive the WebView2-backed
Tauri window.

## One-time machine setup

```powershell
# 1. The two binaries that tauri-driver depends on (Windows).
cargo install tauri-driver --locked
cargo install --git https://github.com/chippers/msedgedriver-tool

# 2. Have msedgedriver-tool download a matching Edge WebDriver next to itself.
#    (It picks the version that matches your installed Microsoft Edge runtime.)
& "$HOME/.cargo/bin/msedgedriver-tool.exe"

# 3. Make sure msedgedriver.exe is on PATH (or copy it next to tauri-driver.exe).
$env:PATH = "$HOME/.cargo/bin;$env:PATH"

# 4. Install Node deps for this package.
cd tools/e2e
pnpm install

# 5. Sanity check.
pnpm doctor
```

The `doctor` script reports anything missing with copy-pasteable fixes.

## Interactive driving

Open two terminals from `tools/e2e/`.

```powershell
# Terminal 1 — long-lived host. Builds the Tauri debug binary the first time
# (use --build to force a rebuild), starts tauri-driver, opens the app, and
# waits for JSON commands on http://127.0.0.1:47999.
pnpm host
```

```powershell
# Terminal 2 — drive it.
pnpm exec rt-e2e screenshot ../../.tmp/before.png
pnpm exec rt-e2e ws-send '{"type":"add_repo","path":"X:/dev/rustling-tulip","name":null}'
pnpm exec rt-e2e click "[data-testid=spawn-session-btn]"
pnpm exec rt-e2e wait "[data-testid=session-pane]" 5000
pnpm exec rt-e2e eval "globalThis.__rt_terms.values().next().value?.buffer.active.getLine(0)?.translateToString()"
pnpm exec rt-e2e screenshot ../../.tmp/after.png
pnpm exec rt-e2e stop
```

Every subcommand prints JSON (or a path) on stdout — friendly to scripted use.

## Smoke spec

```powershell
pnpm test
```

Runs `tests/e2e/specs/webview.spec.ts` under tauri-driver + wdio: launches
the real Tauri shell, bypasses the native "add repo" dialog by sending
`add_repo` over the daemon's WS API directly, spawns a session against the
bundled fake-claude binary, and asserts the prompt banner appears in the
xterm buffer.

## How it works

- `src/handshake.ts` mirrors `crates/daemon/src/paths.rs` to find
  `daemon.json`. In tests the harness sets `RUSTLING_TULIP_CONFIG_DIR` to
  `.tmp/e2e/config/` so the daemon writes its state, sessions, and logs
  there instead of the user's real `%APPDATA%\leftos\rustling-tulip\config\`
  (Windows) / XDG dir. The override is honored by both `Dirs::ensure` in
  the daemon and `config_dir` in the Tauri app.
- `src/ws-client.ts` opens a side-channel WebSocket to the daemon. The Tauri
  app already has its own WS client; the side channel exists so tests can
  send `add_repo` etc. without driving the OS file dialog (which is
  unreachable from WebDriver). The daemon broadcasts state changes to all
  connected clients, so the UI updates either way.
- `src/driver.ts` spawns `tauri-driver`, builds (or finds) the Tauri debug
  binary, and constructs a WebdriverIO browser session against it.
- `src/host.ts` wraps the driver in a long-lived HTTP control socket on
  `127.0.0.1:47999`. `src/cli.ts` is a thin client — it POSTs JSON commands
  to the host.
- `fake-claude/index.mjs` is a Node script that imitates the `claude` CLI
  enough for the daemon's PTY status detector to register an interactive
  session. Wire it in via the `RUSTLING_TULIP_CLAUDE` env var (the daemon
  honors it at `crates/daemon/src/server.rs:claude_program`).

## Caveats

- Tauri/WebView2 means the WebDriver session is a single window. Driving the
  pop-out tab/session windows requires a second WebDriver session — not
  implemented yet.
- Native OS dialogs (file picker, confirm exit, system notifications) are
  invisible to WebDriver. The harness routes around them via the daemon WS.
- `cargo deny check` is not run here; the e2e harness runs against built
  artifacts and doesn't ship to users.
