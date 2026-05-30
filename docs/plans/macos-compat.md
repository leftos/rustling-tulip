# macOS Compatibility — Investigation & Porting Plan

## Status

**Greenlit 2026-05-30 — macOS is now a committed target.** No code has changed
yet: this document is the map from the initial investigation sweep (every
Windows-specific surface, what each needs on macOS, and the phasing below). The
**next step is a detailed implementation plan** built on the phasing in
"Suggested phasing", then execution starting at Phase M0. Two scoping decisions
(IPC transport, distribution) are still open — see "Decisions to make during
planning"; settle them with the user before/while writing the detailed plan. The
phase checkboxes below are the work items.

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

- [ ] Decide transport abstraction (manual `#[cfg]` alias vs `interprocess`).
- [ ] Abstract `create_pipe`/`connect_with_retry` over a platform transport;
      keep handshake + frame loops generic.
- [ ] Unix socket-name derivation (replace `\\.\pipe\…`); ensure cleanup of stale
      socket files (bind fails `AddrInUse` on a leftover path — unlink-then-bind).
- [ ] Port the transient-error retry semantics (Windows `ERROR_PIPE_BUSY`/231,
      `ERROR_ACCESS_DENIED`/5) to the Unix equivalents (`ECONNREFUSED`/`ENOENT`
      while the peer re-binds).

**Watch:** the Windows side just shipped a re-arm retry fix (see
`remote-lan-access.md` → Post-merge robustness). The Unix path needs an
equivalent reconnect story so a daemon restart reattaches to a still-running
tracer over the socket.

## 2. `job_object` — compile gate + Unix process-tree cleanup

- `crates/tracer/src/main.rs:20` — `mod job_object;` is **ungated**; `job_object.rs`
  imports `windows::Win32::System::JobObjects::*`. The call site
  (`supervisor.rs:162`) is already `#[cfg(windows)]`-gated, so only the module
  declaration leaks.
- [ ] Gate the module: `#[cfg(windows)] mod job_object;` (makes macOS compile).
- [ ] Implement the Unix equivalent so worktree cleanup isn't wedged by surviving
      grandchildren: spawn the PTY child in its own session/process-group
      (`setsid`/`pre_exec`) and, on tracer shutdown/kill, `killpg` the group.
      macOS has no `PR_SET_PDEATHSIG`, so death-of-parent cleanup must be explicit
      on the shutdown path. Mirror the existing "best-effort, log-and-proceed"
      contract.

## 3. Autostart on login

- `apps/tauri-app/src-tauri/src/autostart.rs` — Windows arm (HKCU `Run` via
  `winreg`, lines ~12–49); non-Windows `get_autostart` returns `false` (~54–62)
  and `set_autostart` errors "unsupported" (~67–78). `winreg` is already
  `[target.'cfg(windows)'.dependencies]`.
- Frontend already gates the toggle when unsupported
  (`SettingsModal.tsx` ~931–944, hides when the value is `null`).
- [ ] macOS arm: write/remove `~/Library/LaunchAgents/dev.leftos.rustling-tulip.daemon.plist`
      pointing at the daemon binary (`daemon_supervisor::locate_daemon_binary`),
      toggled via `launchctl load/unload`. Candidate dep: `plist` (vet with
      `cargo deny`) or hand-emit the small XML.

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

- [ ] **Phase M0 — compile on macOS.** Gate `mod job_object`; add the macOS bundle
      target; confirm `cargo build` + `cargo clippy` are clean on macOS for all
      crates. (No runtime yet — IPC still stubbed/absent.)
- [ ] **Phase M1 — tracer IPC over Unix sockets.** The core of the port; gets
      interactive + plain-shell PTY sessions actually working, including
      daemon-restart reattach.
- [ ] **Phase M2 — process-tree cleanup.** `setsid`/`killpg` so worktree removal
      isn't wedged by survivors.
- [ ] **Phase M3 — autostart (LaunchAgent).**
- [ ] **Phase M4 — packaging + signing/notarization** for distribution.

## Decisions to make during planning

- [x] **Is macOS a real target?** Yes — greenlit 2026-05-30. Invest in the real
      IPC abstraction (Phase M1), not just a compile-time stub.
- [ ] **IPC transport for the port:** manual `#[cfg]` transport alias over
      `AsyncRead + AsyncWrite` (no new dependency) vs the `interprocess` crate
      (fewer `#[cfg]`s, a new dependency to vet with `cargo deny`). Settle with
      the user before writing the M1 detail.
- [ ] **Distribution scope:** local dev builds only, or signed/notarized `.dmg`?
      (Signing is the long pole and is Apple-account-dependent — it scopes Phase
      M4.) Settle with the user.
