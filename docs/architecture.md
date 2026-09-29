# Architecture

rustling-tulip orchestrates many parallel `claude` (and `codex`) CLI sessions across single repos and multi-repo workspaces from one native window. A long-lived daemon owns every PTY and child process; the desktop client is only a client, and sessions outlive it. Nothing in the repo calls the Anthropic API: the daemon shells out to the CLI. Open work is in [plans/MAIN.md](./plans/MAIN.md); the native client's design decisions are in [native-client.md](./native-client.md).

## Overview

```
┌────────────────────────┐          ┌────────────────────────────┐
│  Native client (GPUI)  │  WS+JSON │  Daemon (long-lived)       │
│  - alacritty_terminal  │ ───────► │  - Session supervisor      │
│    panes               │ ◄─────── │  - PTY pool (ConPTY)       │
│  - Activity rail       │          │  - rt-tracer.exe sidecar   │
│  - Source control      │          │  - Worktree orchestrator   │
└────────────────────────┘          └──────────┬─────────────────┘
                                               │ spawns
                                               ▼
                                    ┌────────────────────────────┐
                                    │  claude / codex CLI        │
                                    │  (interactive or headless) │
                                    └────────────────────────────┘
```

The wire protocol is JSON over a localhost WebSocket (`crates/protocol/src/lib.rs`); the current version and the versions still supported are in `protocol-version.json`. The daemon accepts several clients at once, each with its own connection and its own tab layout.

## Components

| Component | Path | Role |
|---|---|---|
| Protocol | `crates/protocol/` | The daemon/client contract: tagged enums `ClientMessage` and `DaemonMessage`, `SessionSnapshot`, and the forward-compatible `Unknown` wrappers. Additive changes only while protocol 22 must decode (`cargo test -p protocol v22_compat`). |
| Daemon | `crates/daemon/` (binary `rustling-tulipd`) | WS server and HTTP `/shutdown` (`server.rs`), session registry and lifecycle (`registry.rs`, `session.rs`, which serialises each session's sidecar writes and its delete under one per-session gate), persisted repos, workspaces, tabs and host settings (`state.rs`), per-agent spawn arguments (`agents/`), env references in spawn env rows resolved from the process and user environment (`user_env.rs`), status heuristics (`pty_state.rs`, `osc_title.rs`), headless stream-json sessions (`headless.rs`), scrollback ring (`scrollback.rs`), orphan recovery (`orphan.rs`), session history and recovery (`history.rs`, `transcripts.rs`, `tracer_log.rs`), git and worktrees (`git.rs`, `git_write.rs`, `git_inspect.rs`, `git_watch.rs`, `workspace.rs`, `worktrees_admin.rs`, `worktree_cleanup.rs`, `branch_fate.rs`, `branch_names.rs`), LAN access (`lan.rs`, `pairing.rs`, `discovery.rs`, `secret.rs`), keep-awake (`keep_awake.rs`), single-instance lock (`instance_lock.rs`), login launch (`detach.rs`). |
| Tracer | `crates/tracer/` (binary `rt-tracer.exe`) | One supervisor process per interactive or shell session. It owns the ConPTY, keeps draining output into a 4 MB ring while no daemon is attached, and survives daemon restarts. |
| Tracer protocol | `crates/tracer-protocol/` | The daemon/tracer ABI, frozen and additive-only; see [tracer-abi.md](./tracer-abi.md). |
| Daemon client | `crates/daemon-client/` | Client-side supervision shared by the clients: ensure-running, health probe, protocol compatibility check, graceful `/shutdown`, config dir, client identity. |
| Native client | `apps/native/` (binary `rustling-tulip-native`) | The GPUI + `alacritty_terminal` desktop client; see [native-client.md](./native-client.md). |
| fake-claude | `tools/e2e/fake-claude/` | A scripted stand-in for the `claude` CLI used by the native e2e tier, selected with `RUSTLING_TULIP_CLAUDE`. |

The earlier Tauri desktop app is not on `main`: it lives on the `tauri` maintenance branch, cut from the `tauri-last` tag, where its hotfixes are made and its installer is built. Daemon fixes land on `main` first and are cherry-picked there. The daemon keeps protocol 22 in `supported` for the installed Tauri app.

## What it does

- **Sessions**: interactive Claude, Codex and Cursor sessions, plain shells, and headless Claude sessions (`claude --print --output-format stream-json`, parsed into status, tokens, cost and a recent-actions log). Each session's agent is chosen per spawn. Status (working, awaiting input, idle, stopped) comes from a heuristic over the PTY stream; attention raises an OS notification.
- **Workspaces**: a named set of registered repos. A workspace session runs one agent with its cwd in the first member's worktree and `--add-dir` for every other member; worktrees keep the members' relative paths (layout in CLAUDE.md, "Where things live on disk"). A VS Code `.code-workspace` file found beside a repo is offered as a workspace.
- **Survivable sessions**: every interactive and shell session runs under `rt-tracer.exe`, so a daemon restart or upgrade reattaches to live sessions and replays their ring. A lost tracer leaves its session abandoned (Resume / Dismiss) rather than discarded.
- **Session history and recovery**: every ended session gets a history entry (how it ended, its spawn config, its Claude conversation id), kept 7 days and backfilled from tracer logs; `RecoverSessions` respawns chosen entries with `claude --resume`, including sessions still in the Abandoned group; the native client offers it from the rail's Recover dialog (`docs/native-client.md`, Session recovery).
- **Worktrees**: spawns create or reuse a worktree per member under a machine-local root. Branch names are daemon-picked `wt/<adjective>-<noun>` names that no member repo already uses. Fork points resolve to the remote-tracking base (`main` → `origin/main`), with a background fetch and a "commits behind" preview; an existing worktree or a leftover branch is offered as reuse or recreate, never bound silently. Worktrees are addressed by path, so a worktree left by a gone session can be launched into. Duplicating a worktree session spawns the clone on a fresh branch.
- **Discard with branch fate**: deleting a worktree reaps its branch when the commits already landed on the base or its remote by ancestry or patch equivalence (`git cherry`); otherwise the confirm names each member's branch and offers keep or delete.
- **Source control**: per-worktree changed files, staging, commit, discard, stashes, paginated history, commit detail, open in forge, and side-by-side diffs, refreshed by a `.git` watcher that ignores objects, logs and build directories and parks while no client is connected.
- **Remote LAN access**: an opt-in TLS listener on `0.0.0.0` with a self-signed certificate, fingerprint pinning, mDNS discovery and short-code pairing; tab layouts are per client, sessions are global. The native client's remote mode arrives with its Phase 6 (see [plans/MAIN.md](./plans/MAIN.md)).
- **Remote file fetch (daemon half)**: `FetchFile` streams a file under a registered repo or session worktree in chunks, confined by one path-confinement helper that `GetFileSnapshot` and `GetFileDiff` share.
- **Host behaviour**: the daemon holds an idle-sleep inhibitor while any session has a live child (`keep_awake`, default on); a second daemon on the same config dir exits at once; the startup reap and binary-cache GC touch only tracers and binaries this config dir owns; the login entry runs `rustling-tulipd --detach`, which copies itself into the binary cache and starts that copy.

## Task index

Where to start for common changes, in the order a change usually flows through the files.

| Task | Files |
|---|---|
| Add a daemon/client message | `crates/protocol/src/lib.rs` → `crates/daemon/src/server.rs` (handler) → `apps/native/src/net.rs` (`on_daemon_message`) → the view that consumes it; the `add-protocol-message` skill walks it |
| Change what a session snapshot carries | `crates/protocol/src/lib.rs` (`SessionSnapshot`) → `crates/daemon/src/session.rs` (snapshot builder) → `apps/native/src/sidebar.rs` |
| Change spawn arguments for an agent | `crates/daemon/src/agents/mod.rs`, `agents/claude.rs`, `agents/codex.rs`, `agents/cursor.rs`, `crates/daemon/src/spawn_plan.rs` |
| Change PTY or tracer behaviour | `crates/daemon/src/tracer_client.rs`, `crates/daemon/src/pty.rs`, `crates/tracer/src/supervisor.rs`, `crates/tracer-protocol/src/lib.rs`, [tracer-abi.md](./tracer-abi.md) |
| Change status detection | `crates/daemon/src/pty_state.rs`, `crates/daemon/src/osc_title.rs` |
| Change worktree or git behaviour | `crates/daemon/src/git.rs`, `workspace.rs`, `worktrees_admin.rs`, `worktree_cleanup.rs`, `branch_fate.rs`, `branch_names.rs`, `git_write.rs`, `git_watch.rs` |
| Change persisted host state | `crates/daemon/src/state.rs`, `crates/daemon/src/paths.rs` |
| Change session history or recovery | `crates/daemon/src/history.rs`, `transcripts.rs`, `tracer_log.rs`; the dialog: `apps/native/src/recover.rs`, `recover_view.rs`, `spawns.rs` (`place_several`) |
| Change daemon startup or supervision | `crates/daemon/src/main.rs`, `instance_lock.rs`, `binary_cache.rs`, `orphan.rs`, `crates/daemon-client/src/supervisor.rs` |
| Add a native client view or dialog | a plain-Rust model module in `apps/native/src/` → its `*_view.rs` → mounted and routed in `apps/native/src/lib.rs` (`RootView`) → a spec in `apps/native/tests/ui_*.rs` over `tests/support/mod.rs` |
| Change the sidebar or activity rail | `apps/native/src/sidebar.rs`, `sidebar_view.rs`, `activity_bar.rs`; the Needs You panel: `needs_you.rs`, `needs_you_view.rs` |
| Change the client's colours | `apps/native/src/palette.rs` (every UI colour), `theme.rs` (the terminal palette), `appearance.rs` (accent and background presets) |
| Change tabs or panes | `apps/native/src/tabs.rs`, `tab_bar.rs`, `tab_menu.rs`, `grid_view.rs`, `pane_menu.rs`, daemon `crates/daemon/src/tabs.rs` |
| Change the terminal pane | `apps/native/src/term_view.rs`, `term.rs`, `term_input.rs`, `keys.rs`, `mouse.rs`, `links.rs`, `shell_marks.rs` |
| Change the spawn dialog | `apps/native/src/spawn_form.rs`, `spawn_view.rs`, `spawn_preview.rs`, `combobox.rs`, `spawns.rs` |
| Change session actions and menus | `apps/native/src/session_menu.rs`, `session_actions.rs`, `branch_fate.rs`, `discard_confirm.rs` |
| Change Settings | `apps/native/src/settings_view.rs`, `appearance.rs`, `appearance_view.rs`, `sidebar.rs` (`UiState` in native-ui.json) |
| Change source control or diffs | `apps/native/src/source_control.rs`, `source_control_view.rs`, `changes_view.rs`, `sc_writes.rs`, `stashes.rs`, `history.rs`, `diff_model.rs`, `diff_view.rs`, `diff_tab.rs`, `syntax.rs` |
| Run the tests | UI specs: `cargo test -p rustling-tulip-native --test ui_<name>`; live e2e: `.\rt.ps1 native-e2e` (`tests/e2e_live.rs`, `tests/e2e_recover.rs`); OS smoke: `.\rt.ps1 native-smoke` (`tests/smoke_window.rs`) |

## Non-goals

- Sub-agent / Task-tool interception or isolation (viewing a subagent's transcript read-only is planned).
- Auto-discovery of repos: the registry is manual.
- Attach beyond the LAN by internet exposure or SSH tunnelling. The mobile app's relay ([plans/mobile-app.md](./plans/mobile-app.md)) is the one planned way in from outside.
- Cloud sync of the registry or sessions.
