# macOS Compatibility — Investigation & Porting Plan

## Status

**Greenlit 2026-05-30 — macOS is now a committed target.**

**M1–M3 implemented 2026-05-30 (code-complete, Windows build/clippy/test/deny
green; pending verification on a real Mac).** Scope this round: M1 (IPC over
`interprocess` local sockets), M2 (Unix process-tree cleanup via `killpg`), M3
(macOS autostart LaunchAgent). Distribution scope was set to **local dev builds
only** — packaging/signing (M4) stays deferred. Both open decisions are now
settled (see "Decisions"). What's left is runtime verification on macOS (and a
Windows runtime reattach smoke test, since M1 replaced the named-pipe transport
on the primary platform).

Note discovered during execution: `mod job_object;` was **already**
`#[cfg(windows)]`-gated (`crates/tracer/src/main.rs`), so the doc's "compile
blocker #2" was stale. The real compile blocker was the unconditional
named-pipe imports, which M1 removed entirely (the transport is now
`interprocess` on both platforms).

## Executive summary

The project is **substantially portable already**. The PTY layer is built on
`portable-pty` (ConPTY on Windows, `forkpty`/`openpty` on Unix), the config/data
dirs go through the `directories` crate (`~/Library/Application Support/…` on
macOS), and most OS-touching code already carries `#[cfg(not(windows))]` /
`#[cfg(target_os = "macos")]` arms (process kill/spawn, shell resolution, reveal
in Finder, open URL, VS Code launch, path normalization).

There is **one architectural blocker**: the tracer ↔ daemon IPC is Windows named
pipes with no Unix path. Everything else is either already done, a one-line
`#[cfg]` gate, a contained feature (autostart), or build/packaging config.

### The blockers, ranked

1. **Tracer IPC transport (HARD).** Named pipes (`tokio::net::windows::named_pipe`)
   in both the tracer server and the daemon client, plus the `\\.\pipe\…` name
   derivation. macOS needs Unix domain sockets. This is the real work.
2. **`job_object` module won't compile on macOS (COMPILE BLOCKER, trivial gate +
   real Unix follow-up).** `crates/tracer/src/main.rs:20` declares `mod job_object;`
   ungated; the module imports the `windows` crate (a `cfg(windows)`-only dep), so
   a macOS build fails to compile. Gating the `mod` is one line; replacing the
   *functionality* (kill-the-whole-descendant-tree on tracer exit, so worktree
   cleanup isn't wedged by surviving `node`/dev-server grandchildren) needs a Unix
   `setsid` + `killpg` equivalent.
3. **Autostart (MODERATE).** `winreg` HKCU Run key → macOS `LaunchAgent` plist.
4. **Packaging (TRIVIAL–MODERATE).** Bundle target + signing/notarization.

Rough estimate: **3–5 focused days** for a working macOS build (the IPC port is
~2 of those), excluding Apple Developer signing/notarization setup.

---

## 1. Tracer ↔ daemon IPC — the architectural blocker

Named pipes are baked into three places with no Unix arm:

- `crates/tracer/src/supervisor.rs:27` — `use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions}`.
- `crates/tracer/src/supervisor.rs:402` — `create_pipe(first, name) -> NamedPipeServer`; the re-arm/accept loop (~450–492) and `handle_client` (~550–765) are all typed on `NamedPipeServer`.
- `crates/daemon/src/tracer_client.rs:25` — `use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient}`; `connect_with_retry` (~365), `handshake_and_wire` (~429), the reader/writer halves (~537, ~596) are typed on `NamedPipeClient`.
- Pipe-name derivation: `crates/tracer-protocol/src/lib.rs:~149` (`format!(r"\\.\pipe\rt-tracer-{session_id}")`) and `crates/daemon/src/tracer_client.rs:~139` (`pipe_name_with_prefix`).

The wire protocol itself (`crates/tracer-protocol`) is **pure serde JSON + base64**
payloads — fully portable, no changes.

**macOS approach.** Replace the named-pipe transport with `tokio::net::UnixListener`
/ `UnixStream` over a socket file (e.g. `$XDG_RUNTIME_DIR`/`std::env::temp_dir()` →
`rt-tracer-<session_id>.sock`). The frame I/O (line-delimited JSON over a
`BufReader`, split read/write halves) is identical once the stream type is
abstracted. Two viable shapes:

- A `#[cfg]`-selected transport type alias + per-platform `connect`/`bind`
  helpers, keeping the existing handshake/wire code generic over
  `AsyncRead + AsyncWrite`. Preferred — least churn.
- The `interprocess` crate (cross-platform local sockets) — fewer `#[cfg]`s but a
  new dependency to vet against `cargo deny`.

- [x] Decide transport abstraction → `interprocess` (replaces named pipes on
      both platforms; no `#[cfg]` alias). The frame-write helpers were already
      generic; `handle_client`/`handshake_and_wire`/read+write loops are now
      generic over `AsyncRead + AsyncWrite`.
- [x] `create_pipe` → persistent `interprocess` `Listener` with a serialized
      accept loop; `connect_with_retry` → `Stream::connect`. Handshake + frame
      loops unchanged.
- [x] Socket-name derivation in `tracer_protocol::socket_name` (Windows
      namespaced `rt-tracer-<id>`; Unix length-bounded `<tmp>/rt-tracer-<16hex>.sock`
      for the `sun_path` limit). Stale-socket unlink-before-bind + shutdown
      removal on Unix.
- [x] Retry semantics ported to portable `io::ErrorKind` (`NotFound` /
      `ConnectionRefused`) plus the Windows `ERROR_PIPE_BUSY`/231 raw-code check.

**Watch:** the Windows side just shipped a re-arm retry fix (see
`remote-lan-access.md` → Post-merge robustness). The Unix path needs an
equivalent reconnect story so a daemon restart reattaches to a still-running
tracer over the socket.

## 2. `job_object` — compile gate + Unix process-tree cleanup

- `crates/tracer/src/main.rs:20` — `mod job_object;` is **ungated**; `job_object.rs`
  imports `windows::Win32::System::JobObjects::*`. The call site
  (`supervisor.rs:162`) is already `#[cfg(windows)]`-gated, so only the module
  declaration leaks.
- [x] Module already gated `#[cfg(windows)] mod job_object;` (was stale in the
      original doc).
- [x] Unix equivalent implemented: `portable-pty` already `setsid`s the PTY
      child (session leader), and `kill_process_group` does `killpg(child_pid,
      SIGTERM)` then `SIGKILL` on the tracer shutdown path. Best-effort +
      logged. A tracer killed with SIGKILL still leaks survivors (no
      `PR_SET_PDEATHSIG` on macOS) — accepted. *Verify on macOS that
      `portable-pty` makes the child a group leader and grandchildren die.*

## 3. Autostart on login

- `apps/tauri-app/src-tauri/src/autostart.rs` — Windows arm (HKCU `Run` via
  `winreg`, lines ~12–49); non-Windows `get_autostart` returns `false` (~54–62)
  and `set_autostart` errors "unsupported" (~67–78). `winreg` is already
  `[target.'cfg(windows)'.dependencies]`.
- Frontend already gates the toggle when unsupported
  (`SettingsModal.tsx` ~931–944, hides when the value is `null`).
- [x] macOS arm implemented in `autostart.rs`: write/remove
      `~/Library/LaunchAgents/dev.leftos.rustling-tulip.daemon.plist` (hand-emitted
      XML — no `plist` dep), `ProgramArguments` from
      `daemon_supervisor::locate_daemon_binary`, `RunAtLoad`. **File-only** (no
      `launchctl load` on toggle: that would start a second daemon while the
      app's supervisor runs one; the file gives next-login parity with the
      Windows `Run` key). *Verify on macOS that launchd loads it at login;
      consider an `EnvironmentVariables`/PATH key if the daemon can't find
      `claude` in launchd's minimal env.*

## 4. Build, bundle, packaging

- `apps/tauri-app/src-tauri/tauri.conf.json:33` — `"targets": ["nsis"]`
  (Windows-only). The icon set already includes `icons/icon.icns` (Tauri
  auto-selects it on macOS), so no new art is needed.
- [ ] Add a macOS bundle target (`"dmg"`/`"app"`, or platform-conditional
      `targets`). The `bundle.active = true` + `externalBin`/`resources` work to
      ship `rustling-tulipd` + `rt-tracer` is still deferred on **all** platforms
      (see root `CLAUDE.md` → "Things that are deferred") — macOS inherits that.
- [ ] Code-signing + notarization (Developer ID cert, `entitlements.plist`,
      `notarytool`). Build-machine/CI concern, not source. Needed for distribution
      but not for local dev runs.
- `rt.ps1` is a Windows-only dev convenience wrapper. A macOS port would want a
  `rt.sh` (or a `cargo xtask`) sibling; the underlying `cargo`/`pnpm` commands are
  identical. Not a blocker for `cargo build` / `pnpm tauri dev`.

## 5. Minor cleanups (work as-is, optionally tidy)

- `crates/daemon/src/main.rs:208–213` — `is_tracer_image` strips `.exe`
  unconditionally; resilient on macOS (falls through to full-name match) but does
  a needless strip. Optional `#[cfg(windows)]` tidy.
- `crates/daemon/src/lock_finder.rs` — non-Windows `processes_holding` is a real
  no-op stub returning `Vec::new()` (Restart-Manager has no portable analog).
  Core behavior fine; showing lock-holders on macOS later would need `lsof`/
  `libproc` (nice-to-have, not a blocker). The Unix terminate path (~75–97,
  SIGTERM/SIGKILL via `sysinfo`) is a real implementation.
- `apps/tauri-app/src/utils/sessionLabel.ts:~66–78` — `isNoisyShellTitle` matches
  hardcoded `.exe` names; benign on macOS (never matches), so noisy-title
  filtering simply won't trigger for mac shells. Optional: add `bash`/`zsh`/`sh`
  bare names if the filtering is wanted on macOS.

## 6. Already portable — no work needed

Confirmed dual-arm or platform-neutral during the sweep:

- **PTY**: `portable-pty` (`crates/tracer/src/supervisor.rs`) — cross-platform.
- **Tracer spawn**: `tracer_client.rs` has both `#[cfg(windows)]` (CREATE_NO_WINDOW)
  and `#[cfg(not(windows))]` arms; exe-name (`rt-tracer[.exe]`) is gated.
- **Daemon process mgmt (Tauri side)**: `kill_pid` (`lib.rs:255–284`),
  `spawn_daemon` (`daemon_supervisor.rs:510–541`), path normalization
  (`daemon_supervisor.rs:355–369`), binary discovery (`394–437`) — all dual-arm.
- **Reveal/open**: `reveal_in_explorer` (`lib.rs:297–320`, 3-platform: explorer/
  `open`/`xdg-open`), `open_url` (`378–402`), `spawn_vscode` (`462–519`, `code` on
  PATH for Unix).
- **Shell resolution**: `server.rs:4393–4439` — Windows `pwsh/powershell/cmd` vs
  Unix `$SHELL → bash/zsh/sh`.
- **Paths**: `paths.rs` (`directories::ProjectDirs`), verbatim-prefix strip is a
  Unix no-op; worktree path building splits on both separators and strips drive
  colons defensively.
- **Orphan recovery**: `orphan.rs` `pid_matches` uses substring matching, resilient
  to `.exe` presence/absence.
- **`.cmd`/`.bat` shim expansion**: `server.rs:4269–4360` is `#[cfg(windows)]` and
  guarded by extension checks — never runs on macOS.
- **Headless mode**: `headless.rs:70–74` CREATE_NO_WINDOW is gated; Unix is a no-op.
- **Frontend path handling**: `api.ts` `parentDir` and `PresetLaunchDialog.tsx`
  `joinPath` handle both separators; remote-mode/LAN/mDNS code is platform-neutral.

## Suggested phasing (when greenlit)

- [x] **Phase M0 — compile on macOS.** `mod job_object` was already gated; the
      named-pipe imports (the real blocker) are gone after M1. macOS bundle
      target intentionally skipped (distribution = local dev only). *Pending: a
      real `cargo build`/`cargo clippy` on macOS to confirm.*
- [x] **Phase M1 — tracer IPC over `interprocess` local sockets.** Transport
      replaced on both platforms; daemon-restart reattach preserved via a
      serialized accept loop. Code-complete; *pending macOS + Windows runtime
      verification.*
- [x] **Phase M2 — process-tree cleanup.** `#[cfg(unix)]` `killpg` (SIGTERM →
      SIGKILL) on tracer shutdown, mirroring the Windows job object. Relies on
      `portable-pty` making the child a session leader — *verify on macOS.*
- [x] **Phase M3 — autostart (LaunchAgent).** Writes/removes
      `~/Library/LaunchAgents/dev.leftos.rustling-tulip.daemon.plist` (file-only;
      no `launchctl load` on toggle, to avoid a double daemon). *Verify on macOS.*
- [ ] **Phase M4 — packaging + signing/notarization** for distribution.
      Deferred (local dev only this round).

## Decisions to make during planning

- [x] **Is macOS a real target?** Yes — greenlit 2026-05-30. Invest in the real
      IPC abstraction (Phase M1), not just a compile-time stub.
- [x] **IPC transport for the port:** `interprocess` crate (v2.4.2) chosen — one
      code path on both platforms (named pipe on Windows, Unix socket on
      macOS/Linux), no `#[cfg]` transport alias. Its `0BSD` transitive deps
      (`doctest-file`, `recvmsg`) were added to the `deny.toml` allow-list.
- [x] **Distribution scope:** local dev builds only this round. No installer /
      signing / notarization — `pnpm tauri dev` + `cargo build` are enough for
      development; M4 stays deferred.
