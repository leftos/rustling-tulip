# macOS Compatibility — Investigation & Porting Plan

## Status

**Greenlit 2026-05-30 — macOS is now a committed target.**

**M1 and M2 implemented 2026-05-30 (code-complete, Windows build/clippy/test/deny
green; pending verification on a real Mac).** Scope this round: M1 (IPC over
`interprocess` local sockets) and M2 (Unix process-tree cleanup via `killpg`),
both on `main`. M3 (macOS autostart LaunchAgent) is on `main` too, in
`crates/daemon-client/src/autostart.rs`, compile-checked and unit-tested on macOS
(RT-246); only the check that launchd loads it at login on a real Mac is open. Distribution scope was set to
**local dev builds only** — packaging/signing (M4) stays deferred. Both open
decisions are now settled (see "Decisions"). What's left is runtime verification
on macOS (and a Windows runtime reattach smoke test, since M1 replaced the
named-pipe transport on the primary platform).

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

There **was** one architectural blocker: the tracer ↔ daemon IPC was Windows
named pipes with no Unix path, and M1 replaced them with `interprocess` local
sockets on both platforms. Everything else is either already done, a one-line
`#[cfg]` gate, a contained feature (autostart), or build/packaging config.

### The blockers, ranked

> **Superseded:** items 1 and 2 landed on `main` (M1 and M2); item 3, the LaunchAgent arm, is on `main` in `crates/daemon-client/src/autostart.rs` (RT-246); item 4 stays deferred with local dev builds only.

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

**Watch:** the Windows side just shipped a re-arm retry fix (see `archive/remote-lan-access.md` → Post-merge robustness fixes). The Unix path needs an equivalent reconnect story so a daemon restart reattaches to a still-running tracer over the socket.

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
      `PR_SET_PDEATHSIG` on macOS) — accepted. The macOS check is the next line.
- [x] Grandchildren die when a session is killed on macOS (see "macOS verification"). zsh's job control puts each background job in a process group of its own, so `killpg` on the shell's group alone misses them. The tracer therefore also reads the session's processes when the Stop arrives, before it kills the shell (`proc_tree::snapshot`: every process whose session id is the shell's pid, plus a parent-pid walk from the shell), and on shutdown, after the group kill, kills that snapshot merged with a fresh session-id scan (`proc_tree::kill_tree`: SIGTERM per group and per pid, then SIGKILL only to a pid whose start time still matches).

## 3. Autostart on login

Note: the Tauri app is no longer on `main`; it lives on the `tauri` branch (tag `tauri-last`). Its `autostart.rs` was recovered into `crates/daemon-client/src/autostart.rs`, macOS arm included. The `apps/tauri-app/...` paths, frontend files and `pnpm tauri` commands in this plan refer to that branch.

- `apps/tauri-app/src-tauri/src/autostart.rs` — Windows arm (HKCU `Run` via
  `winreg`, lines ~12–49); non-Windows `get_autostart` returns `false` (~54–62)
  and `set_autostart` errors "unsupported" (~67–78). `winreg` is already
  `[target.'cfg(windows)'.dependencies]`.
- Frontend already gates the toggle when unsupported
  (`SettingsModal.tsx` ~931–944, hides when the value is `null`).
- [ ] macOS arm, on `main` in `crates/daemon-client/src/autostart.rs` (compile-checked and unit-tested on macOS, RT-246): it
      writes/removes `~/Library/LaunchAgents/dev.leftos.rustling-tulip.daemon.plist`
      (hand-emitted XML — no `plist` dep), `ProgramArguments` from
      `daemon_client::locate_daemon_binary` then `--detach` (the same hand-over to the cached copy as the Windows `Run` value), `AbandonProcessGroup` true so launchd spares the cached daemon, `RunAtLoad`, and no `KeepAlive`. **File-only** (no
      `launchctl load` on toggle: that would start a second daemon while the
      app's supervisor runs one; the file gives next-login parity with the
      Windows `Run` key). The plist it writes passes `plutil -lint` on the Mac mini (see "macOS verification"). Left open: *verify on a real Mac that launchd loads it at login, which needs the owner at the MacBook Air;
      consider an `EnvironmentVariables`/PATH key if the daemon can't find
      `claude` in launchd's minimal env.*

## 4. Build, bundle, packaging

- The shipped client is `apps/native` (GPUI); its installer is Phase 6 (Wave 3), Windows-first.
- [x] Add a macOS bundle for `apps/native` (an `.app` carrying `rustling-tulipd` and `rt-tracer`, then a `.dmg`) beside the Windows installer that Phase 6 builds: `tools/installer/pack-macos.ps1`, run on a Mac, makes an ad-hoc signed arm64 bundle, its dmg and the update zip (`tools/installer/README.md` "macOS app and disk image").
- [x] Code-signing + notarization: `tools/installer/pack-macos-signed.ps1`, run on this PC, runs `pack-macos.ps1 -Sign` on the Mac mini, which signs the binaries, the `.app` and the `.dmg` with the Developer ID Application certificate yaat uses (hardened runtime, no entitlements file), notarizes and staples the app and the dmg with its App Store Connect notary key, and checks `codesign --verify`, `spctl` and `stapler validate` (`tools/installer/README.md` "macOS code signing").
- The macOS dev loop runs from the Windows PC: `.\rt.ps1 mac check|build|clippy|test [<extra>]` sends one cargo command to the Mac mini builder (`builder@mac-mini`) as a pinned heavy gate run, logged at `.tmp\rt-mac-<sub-verb>.log`. Extra cargo arguments use long forms (`--package <crate>`), since `-p` binds to `rt.ps1`'s `-PrintOnly`. There is no `rt.sh`.
- [x] The Mac mini builder has Xcode's Metal Toolchain, so gpui's build script compiles its shaders there.
- [x] `mac clippy` runs clean with `-D warnings`.

- [ ] Accounts on macOS ([accounts.md](./accounts.md), RT-916): the forked `claude-accounts` crate gains Unix locks and reads and writes the live Claude credential through the Keychain (on macOS it is not in `.credentials.json`), allowing for Claude Code's roughly 30-second Keychain cache before a switch takes effect. Accounts shows on macOS once it lands.

## Native client

A smoke run of `rustling-tulip-native` on the Mac mini builder (arm64), driven over SSH with no person at the console. Nothing found blocks a launch: the client builds, opens its window, starts a daemon, and that daemon runs a shell and a Claude session whose hooks reach it, also from a binaries folder whose path has a space.

The run was isolated: every daemon and client had `RUSTLING_TULIP_CONFIG_DIR`, `RUSTLING_TULIP_BINARIES_DIR` and `RUSTLING_TULIP_WORKTREES_DIR` (and the tailnet, remote-files and updates folders) under a scratch `.tmp` folder, `RUSTLING_TULIP_CLAUDE` pointing at `crates/agents/claude/fake/fake-claude.sh`, and `CLAUDE_CONFIG_DIR` under the same `.tmp`.

How it was built and launched:

- Build: `.\rt.ps1 mac build --package rustling-tulip-native --package daemon --package tracer` builds the three debug binaries through the gate, gpui included. The output stays on the builder in the gate's work folder for the worktree, `~/gate-remote/work/<worktree>-<hash>/target/debug/`, so no separate clone is needed. The smoke copied `rustling-tulip-native`, `rustling-tulipd` and `rt-tracer` from there into one folder, so the daemon and tracer sit beside the client.
- Launch form that worked from SSH: a `.command` file that sources the environment and runs `exec <folder>/rustling-tulip-native > <log> 2>&1`, opened with `open -n <file>.command`. It starts in the logged-in GUI session, through Terminal.app, which stays open beside the client.
- Launch form that failed: `launchctl asuser $(id -u) env ... rustling-tulip-native` from an SSH login prints `Could not switch to audit session 0x186a2: 1: Operation not permitted`; it needs root.
- `node` is not on the builder's SSH PATH (it lives under `~/gate-tools/node/bin` and only a login shell adds it), so the smoke put that folder on PATH for fake-claude. A Finder-launched client meets the same launchd-minimal PATH. RT-1022 answers it for the daemon: at start it takes the login shell's PATH when its own is launchd-minimal (`docs/daemon-cli.md`, "Login PATH on macOS"), so the agents, shells, git and gh it runs find their programs. The client's own `code` lookup is not covered.

Checks:

- [x] The window opens: the window list (`CGWindowListCopyWindowInfo`) shows an on-screen, layer-0 window owned by the client's pid, 1000 by 672 points.
- [ ] A screenshot of the window: `screencapture -x` from SSH fails with `could not create image from display`, as the SSH context has no Screen Recording grant. Not a client problem.
- [x] The client starts the daemon: `native.log` shows `daemon binary cache: populated`, `spawning daemon from cache` (template beside the client exe, cached copy under the isolated binaries folder), `wait_for_handshake: daemon ready` and `connected to daemon`. `pgrep -fl rustling-tulipd` shows the cached copy running with `--start-failure-file` under the isolated `logs/`.
- [x] The daemon writes `daemon.json` in the isolated config folder, with the port, pid, `supported_versions` and `build_version`.
- [x] A plain shell session through the daemon's verbs: `spawn --cwd` starts `/bin/zsh` under `rt-tracer` over a Unix socket in the temp folder, `send <id> "echo hi"` types it, and `read` shows `echo hi` then `hi`; `sessions` lists it `idle`.
- [x] A Claude session with fake-claude: `spawn --agent claude` prints the `[fake-claude] ready` banner, and after `send` of a message `watch` shows `idle`, then `working`, then `idle`. `daemon.log` shows `status set by a hook event` for `UserPromptSubmit` and `turn end held`, so the hook commands reach the daemon.
- [x] The same Claude check with a second daemon whose `RUSTLING_TULIP_BINARIES_DIR` has a space (`.tmp/bin dir`): `hook-settings.json` names the hook program as `.../bin dir/rt-tracer-<hash>` with its arguments in a separate list, and the status goes `idle`, `working`, `idle` with the same hook log lines. fake-claude runs the hook as a program with an argument list.
- [x] The real Claude CLI (Claude Code 2.1.318, darwin-arm64) runs a hook's `command` as a program with `args` as its argument list, with no shell between: a probe at `~/rt-1018 scratch/bin dir/hook probe` in the daemon's exact entry shape fired for `SessionStart` and `UserPromptSubmit`, with and without `async`, and received `$0` and every argument whole (`/tmp/x y` as one argument; `$HOME`, `a;b`, `*` and `it's` literal). The CLI's `--debug hooks` log prints the command flattened into one line, which does not show how it was split. Hooks can be checked on a machine with no Claude login: `SessionStart` and `UserPromptSubmit` fire before the "Not logged in" exit of `claude -p … --settings <file>` run with a scratch `CLAUDE_CONFIG_DIR`.
- [x] Clean stop: `kill` of each session, `rustling-tulipd shutdown` of each daemon and a TERM to the client leave no `rt-tracer`, `rustling-tulipd` or fake-claude process, and the tracer sockets are gone from the temp folder.
- [ ] A shell tab takes keys in the window, Cmd+C and Cmd+V copy and paste (or only Ctrl does), and a Claude session's status shows in the sidebar and pane: not run. Driving the window needs `osascript` and System Events, which hung without an Automation or Accessibility grant, so these need a person at the console or over RustDesk.

What the logs say on macOS that is not a launch blocker:

- The client logs `opening the device token store, holding no device tokens: device tokens live in Windows Credential Manager; there is no store off Windows`, so tailnet device tokens are not kept on macOS.
- The daemon logs `the secret store could not be installed` (Credential Manager only) and the summarizer cannot read a saved key, as expected: macOS refuses sealed secrets for now (RT-665's decisions; RT-1010 documents it).
- The daemon logs `the machine's identifier could not be read; the daemon id is not checked against this machine`: the stamp reads `/etc/machine-id`, which macOS does not have, so a copied config folder on a Mac keeps the other machine's daemon id. The macOS machine identifier is `IOPlatformUUID` (from `ioreg`).
- A debug build of the client logs `GPUI was compiled in test mode`.

## macOS verification

A run of M1, M2 and M3 on the Mac mini builder (arm64), driven over SSH. `.\rt.ps1 mac build --package daemon --package tracer` built the debug `rustling-tulipd` and `rt-tracer`, which were copied side by side into a scratch folder. Every daemon started there had `RUSTLING_TULIP_CONFIG_DIR`, `RUSTLING_TULIP_BINARIES_DIR`, `RUSTLING_TULIP_WORKTREES_DIR`, `RUSTLING_TULIP_TAILNET_DIR`, `RUSTLING_TULIP_REMOTE_FILES_DIR` and `RUSTLING_TULIP_UPDATES_DIR` under the scratch folder's `.tmp`, was started with `nohup <folder>/rustling-tulipd &`, and wrote `daemon.json` in the isolated config folder. Sessions were plain shells (`/bin/zsh`), driven with the daemon's session verbs.

Checks:

- [x] M1, tracer reattach: `spawn --cwd` started a shell under `rt-tracer` on a Unix socket in the temp folder, and after `send <id> "echo before"` `read` showed `echo before` then `before`. `rustling-tulipd shutdown` printed `stopped the daemon`; `pgrep -fl rustling-tulipd` then found nothing, and `pgrep -fl rt-tracer` still showed the session's tracer with the same pid. A second daemon started with the same environment logged `reattach_orphans: begin orphan_count=1` and `tracer reattach succeeded` for the session id; `sessions` listed the same id `idle`, `read` showed the earlier `echo before` and `before` lines, and after `send <id> "echo after"` `read` showed `echo after` then `after`.
- [x] M2, session tree kill: in a new `/bin/zsh` session `send` of `sleep 600 & sh -c 'sleep 601 &' ; echo started` left `sleep 600` in a process group of its own and `sleep 601` reparented to pid 1 in the exited `sh`'s group. `kill <id>` returned 0 and `wait <id> --status stopped` printed `stopped`; the tracer log shows `stop request received, killing child`, `tree kill: 1 processes, 1 groups` and `exiting`, and `pgrep -f '^sleep 60[01]$'` then printed nothing.
- An earlier run, with only the shell's process group killed, failed. In a new shell session `send` of `sleep 600 & sh -c 'sleep 601 &' ; echo started` printed `[1] <pid>` and `started`. `ps` showed the shell `/bin/zsh` as its own group leader (its pgid equals its pid), `sleep 600` in a process group of its own (zsh's job control gives each job one), and `sleep 601` reparented to pid 1 in the exited `sh`'s job group. `kill <id>` returned 0, `wait <id> --status stopped` printed `stopped`, the tracer log shows `stop request received, killing child` and `exiting`, and the tracer was gone. `sleep 600` was gone too, but `sleep 601` was still running 28 seconds later, with parent 1. So `killpg` on the shell's group does not reach a job's own group; a job the shell still knows died (the run does not show how, though zsh hangs up its own jobs when it gets SIGHUP), but a process that left the shell's job table (a grandchild of a backgrounded subshell) survives. The run killed that `sleep 601` by its pid.
- [x] M3, the LaunchAgent file: `.\rt.ps1 mac test --package daemon-client` passed 132 of 132 on the Mac mini, among them `autostart::tests::the_launch_agent_detaches_and_abandons_its_process_group`, which writes the plist through the macOS `set_at` to a temp path, reads it back and removes it. That test leaves no file, so the plist text it builds (the template in `autostart.rs` with the test's program path) was copied into a file in the scratch folder: `plutil -lint` printed `OK`, and `plutil -p` showed `Label`, `ProgramArguments` (the daemon path, then `--detach`), `AbandonProcessGroup` true and `RunAtLoad` true. The builder account's own `~/Library/LaunchAgents` was not touched.
- [ ] M3, launchd starts the daemon at login (log out, log in, a daemon is running): needs the owner at the MacBook Air.
- [x] Clean stop: `kill` of the remaining session and `rustling-tulipd shutdown` left no `rt-tracer` or `rustling-tulipd` process and no tracer socket in the temp folder.

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
- **Daemon process mgmt (client side)**: `kill_pid`, `spawn_daemon`, path
  normalization and binary discovery in `crates/daemon-client` — all dual-arm.
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
  `joinPath` handle both separators; remote-mode (tailnet) code is platform-neutral.

## Suggested phasing (when greenlit)

- [x] **Phase M0 — compile on macOS.** `mod job_object` was already gated; the
      named-pipe imports (the real blocker) are gone after M1. macOS bundle
      target intentionally skipped (distribution = local dev only). Confirmed
      on the Mac mini: `mac build` and `mac clippy` run clean (RT-917).
- [x] **Phase M1 — tracer IPC over `interprocess` local sockets.** Transport
      replaced on both platforms; daemon-restart reattach preserved via a
      serialized accept loop. Verified on macOS: a shell session survives a
      daemon shutdown and the next daemon reattaches to it (see "macOS
      verification"). The Windows runtime reattach smoke is not part of that run.
- [x] **Phase M2 — process-tree cleanup.** `#[cfg(unix)]` on tracer shutdown, mirroring the Windows job object: `killpg` (SIGTERM → SIGKILL) on the shell's group, then a kill of every process in the shell's session, background jobs and reparented grandchildren included, read before the shell dies. Relies on `portable-pty` making the child a session leader. Verified on macOS (see "macOS verification").
- [ ] **Phase M3 — autostart (LaunchAgent).** On `main` (`crates/daemon-client/src/autostart.rs`, RT-246). Writes/removes
      `~/Library/LaunchAgents/dev.leftos.rustling-tulip.daemon.plist` with `--detach`, `RunAtLoad`, `AbandonProcessGroup` and no `KeepAlive` (file-only;
      no `launchctl load` on toggle, to avoid a double daemon). The plist passes `plutil -lint` on macOS (see "macOS verification"). Left unticked: *verify on a real Mac that launchd loads it at login*, which needs the owner at the MacBook Air.
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
