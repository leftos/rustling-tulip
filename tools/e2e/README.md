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

# 2. Have msedgedriver-tool download an Edge WebDriver matching your installed
#    Edge/WebView2 runtime. It extracts msedgedriver.exe into its *cwd*, so run
#    it from a scratch directory and move the binary where you want it.
cd $env:TEMP; & "$HOME/.cargo/bin/msedgedriver-tool.exe"; Move-Item -Force msedgedriver.exe "$HOME/.cargo/bin/msedgedriver.exe"

# 3. Make sure msedgedriver.exe is on PATH (or copy it next to tauri-driver.exe).
$env:PATH = "$HOME/.cargo/bin;$env:PATH"

# 4. Install Node deps for this package (step 2 left you in $env:TEMP).
cd <repo>/tools/e2e
pnpm install

# 5. Sanity check. `pnpm run doctor`, not `pnpm doctor` — pnpm has a builtin
#    command by that name that shadows the script and prints nothing.
pnpm run doctor
```

The `doctor` script reports anything missing with copy-pasteable fixes. It also
compares `msedgedriver --version` against the WebView2 Evergreen runtime version
in the registry and fails on a major mismatch — Edge/WebView2 auto-updates, and a
driver left behind produces `session not created: This version of Microsoft Edge
WebDriver only supports Microsoft Edge version <N>` on every spec. The fix is
step 2 above; doctor prints it verbatim.

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

## Parallelism

Specs run several at a time. `wdio.conf.ts` namespaces every shared resource
by the worker's `WDIO_WORKER_ID`, so each spec file gets its own config dir
(and therefore its own `daemon.json`, and therefore its own daemon), worktrees
root, binary cache, tracer pipe prefix, and pair of driver ports
(`4444 + slot*2` for tauri-driver, `+1` for the msedgedriver it fronts).

Sharing any one of those is what previously forced `maxInstances: 1` — every
worker would have read the same `daemon.json` and driven a single daemon,
seeing each other's sessions. The binary cache matters for a subtler reason:
daemon/tracer reaping is scoped by executable path, so a shared cache would
let one worker's startup sweep kill another worker's live tracers.

Worker count defaults to roughly one per two physical cores (clamped to 2–6).
Override it when bisecting a failure that only appears under concurrency:

```powershell
$env:RT_E2E_WORKERS = "1"; pnpm test
```

The build happens once in `onPrepare`; `beforeSession` only asserts the
binaries exist. Re-running cargo per spec file used to cost a second each
*and* serialize workers behind cargo's target-directory lock.

`tsconfig.json` includes the `DOM` lib because `browser.execute` callbacks are
authored here but run in the WebView. (It can't say so inline — the repo's
`check-json` hook rejects comments.)

## How it works

- `src/handshake.ts` mirrors `crates/daemon/src/paths.rs` to find
  `daemon.json`. In tests the harness sets `RUSTLING_TULIP_CONFIG_DIR`,
  `RUSTLING_TULIP_WORKTREES_DIR`, `RUSTLING_TULIP_BINARIES_DIR`, and
  `RUSTLING_TULIP_TRACER_PIPE_PREFIX` so daemon state, sessions, logs,
  worktrees, cached process images, and tracer pipes all live under
  `.tmp/e2e/w-<cid>/`. The config override is honored by both `Dirs::ensure`
  in the daemon and `config_dir` in the Tauri app; the binary override keeps
  test daemon/tracer process cleanup scoped away from regular `rt.ps1`
  launches — and away from sibling workers.
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
