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
| Protocol | `crates/protocol/` | The daemon/client contract: tagged enums `ClientMessage` and `DaemonMessage`, `SessionSnapshot`, and the forward-compatible `Unknown` wrappers. Additive changes need no version bump; a breaking change follows CLAUDE.md "When to bump `protocol-version.json`". |
| Daemon | `crates/daemon/` (binary `rustling-tulipd`) | WS server and HTTP `/shutdown` (`server.rs`), session registry and lifecycle (`registry.rs`, `session.rs`, which serialises each session's sidecar writes and its delete under one per-session gate), persisted repos, workspaces, tabs and host settings (`state.rs`), per-agent spawn arguments (`agents/`), env references in spawn env rows resolved from the process, user and system environment (`user_env.rs`), secret env rows sealed into Windows Credential Manager (`env_secrets.rs`), status heuristics (`pty_state.rs`, `osc_title.rs`), headless stream-json sessions (`headless.rs`), scrollback ring (`scrollback.rs`), orphan recovery (`orphan.rs`), session history and recovery (`history.rs`, `transcripts.rs`, `tracer_log.rs`), git and worktrees (`git.rs`, `git_write.rs`, `git_inspect.rs`, `git_watch.rs`, `workspace.rs`, `worktrees_admin.rs`, `worktree_cleanup.rs`, `branch_fate.rs`, `branch_names.rs`), LAN access (`lan.rs`, `pairing.rs`, `discovery.rs`, `secret.rs`), keep-awake (`keep_awake.rs`), single-instance lock (`instance_lock.rs`), login launch (`detach.rs`). |
| Tracer | `crates/tracer/` (binary `rt-tracer.exe`) | One supervisor process per interactive or shell session. It owns the ConPTY, keeps draining output into a 4 MB ring while no daemon is attached, and survives daemon restarts. |
| Tracer protocol | `crates/tracer-protocol/` | The daemon/tracer ABI, frozen and additive-only; see [tracer-abi.md](./tracer-abi.md). |
| Daemon client | `crates/daemon-client/` | Client-side supervision shared by the clients: ensure-running, health probe, protocol compatibility check, graceful `/shutdown`, config dir, client identity. |
| Native client | `apps/native/` (binary `rustling-tulip-native`) | The GPUI + `alacritty_terminal` desktop client; see [native-client.md](./native-client.md). |
| fake-claude | `tools/e2e/fake-claude/` | A scripted stand-in for the `claude` CLI used by the native e2e tier, selected with `RUSTLING_TULIP_CLAUDE`. |

The earlier Tauri desktop app is retired and not on `main`: the `tauri` branch, cut from the `tauri-last` tag, keeps it as history only, and daemon fixes are not cherry-picked to it.

## What it does

- **Sessions**: interactive Claude, Codex and Cursor sessions, plain shells, and headless Claude sessions (`claude --print --output-format stream-json`, parsed into status, tokens, cost and a recent-actions log). Each session's agent is chosen per spawn. Status (working, awaiting input, idle, stopped) comes from a heuristic over the PTY stream; a session awaiting input raises an OS notification in each native client, unless its skip setting says the owner is already looking at the session.
- **Workspaces**: a named set of registered repos. A workspace session runs one agent with its cwd in the first member's worktree and `--add-dir` for every other member; worktrees keep the members' relative paths (layout in CLAUDE.md, "Where things live on disk"). A VS Code `.code-workspace` file found beside a repo is offered as a workspace.
- **Survivable sessions**: every interactive and shell session runs under `rt-tracer.exe`, so a daemon restart or upgrade reattaches to live sessions and replays their ring. A lost tracer leaves its session abandoned (Resume / Dismiss) rather than discarded.
- **Session history and recovery**: every ended session gets a history entry (how it ended, its spawn config, its conversation id), kept 7 days and backfilled from tracer logs; `RecoverSessions` respawns chosen entries, including sessions still in the Abandoned group. A Claude entry resumes with `claude --resume`, on the provider it ran on (`HistoryEntry::claude_provider()`; a tracer-log import infers it from `--model`), and a DeepSeek entry never recovers as a shell. A Codex or Cursor entry recovers as its own agent in the same folder (own-agent recovery): resumed when its CLI can, else a fresh run with no first prompt. Its id is captured at spawn, kept in `agent_conversation_id` apart from Claude's `claude_session_id` so no reader runs `claude --resume` on a Codex id. Codex has no caller-chosen id, so `codex_rollout.rs` polls `$CODEX_HOME/sessions/` for the rollout file whose first line names the session's cwd, claiming it only while exactly one Codex session waits in that folder (rollouts seen while two wait go to `codex-contested.json`). Cursor's id comes from `cursor-agent create-chat` before the spawn, which then runs `--resume <id>`. A multi-folder standalone recovery passes the extra folders as `--add-dir` to Claude and Codex; Cursor takes one `--workspace` folder, so it runs in the first and logs a warning naming the rest; the native client offers it from the rail's Recover dialog (`docs/native-client.md`, Session recovery).
- **Secret env rows**: a spawn dialog env row under a secret key (`KEY`, `TOKEN`, `SECRET` or `PASSWORD` segment, or ticked Secret) is sealed before the spawn config is captured: its value moves into Windows Credential Manager and every stored file and echo carries `${secret:<id>}`, resolved again at each spawn and respawn; startup seals older files and sweeps unreferenced ids (see [env-secrets.md](./env-secrets.md)).
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
| Change the supported protocol versions or retire a client generation | CLAUDE.md "When to bump `protocol-version.json`" → `protocol-version.json` → `crates/protocol/build.rs` → `crates/protocol/src/lib.rs` (`ClientMessage::Hello`, `DaemonMessage::Welcome`, `DaemonHandshake::supported`) → `crates/daemon/src/server.rs` (`handshake`, `negotiate_protocol_version`) → `crates/daemon-client/src/supervisor.rs` (`classify_existing_daemon`, `retire_daemon` and their tests) → `apps/native/src/net.rs` (`NATIVE_PROTOCOL_VERSIONS`) |
| Change what a session snapshot carries | `crates/protocol/src/lib.rs` (`SessionSnapshot`) → `crates/daemon/src/session.rs` (snapshot builder) → `apps/native/src/sidebar.rs` |
| Add a per-session daemon flag that clients read | `crates/protocol/src/lib.rs` (`SessionSnapshot`, the `ClientMessage` that sets it) → `crates/daemon/src/session.rs` (`SessionRecord`, snapshot, `record_mirror`) → `crates/daemon/src/orphan.rs` (`OrphanMeta`, `RecordMirror`) → `crates/daemon/src/history.rs` (`entry_from_record`) → `crates/daemon/src/server.rs` (the handler, `duplicate_session`, `resume_abandoned`, `recover_one`) → the client readers (native: `apps/native/src/tabs.rs` `tab_session_counts`, `footer.rs`, `sidebar.rs` leaf and `shows_attention`, `session_actions.rs` and `session_menu.rs` for the toggle); `busy_excluded` is the model |
| Change spawn arguments for an agent | `crates/daemon/src/agents/mod.rs`, `agents/claude.rs`, `agents/codex.rs`, `agents/cursor.rs`, `crates/daemon/src/spawn_plan.rs` |
| Change PTY or tracer behaviour | `crates/daemon/src/tracer_client.rs`, `crates/daemon/src/pty.rs`, `crates/tracer/src/supervisor.rs`, `crates/tracer-protocol/src/lib.rs`, [tracer-abi.md](./tracer-abi.md) |
| Change OS notifications (toasts), their toggles or when they are skipped | `crates/protocol/src/lib.rs` (`AttentionReason`) → `crates/daemon/src/pty_state.rs` (`on_transition`), `session.rs` (`fan_out_attention`) → `apps/native/src/notify.rs` (the toast and the `skips` gate) → `apps/native/src/sidebar.rs` (`NotificationSettings`, `NotifySkip`, the attention set) → `apps/native/src/lib.rs` (`session_on_screen`, `overlay_open`) → `apps/native/src/settings_view.rs` (Notifications tab) → `apps/native/tests/ui/ui_notifications.rs` |
| Change status detection | `crates/daemon/src/pty_state.rs`, `crates/daemon/src/osc_title.rs` |
| Change worktree or git behaviour | `crates/daemon/src/git.rs`, `workspace.rs`, `worktrees_admin.rs`, `worktree_cleanup.rs`, `branch_fate.rs`, `branch_names.rs`, `git_write.rs`, `git_watch.rs` |
| Change the remote (Tailscale) listener or tailnet access | [plans/remote-access.md](./plans/remote-access.md) → `crates/tailnet/src/{cli,rule,model}.rs` → `crates/daemon/src/tailnet_listener.rs` → `crates/daemon/src/server.rs` (`build_router`, `handshake`, `ws_handler`, `start_lan_listener` until TS.5) → `crates/daemon/src/paths.rs` → `crates/daemon/Cargo.toml` → `apps/native/src/net.rs` |
| Change persisted host state | `crates/daemon/src/state.rs`, `crates/daemon/src/paths.rs` |
| Change session history or recovery | `crates/daemon/src/history.rs`, `transcripts.rs`, `tracer_log.rs`, `codex_rollout.rs` (a Codex session's conversation id); the dialog: `apps/native/src/recover.rs`, `recover_view.rs`, `spawns.rs` (`place_several`) |
| Change daemon startup or supervision | `crates/daemon/src/main.rs`, `instance_lock.rs`, `binary_cache.rs`, `orphan.rs`, `crates/daemon-client/src/supervisor.rs` |
| Add a daemon capability (Welcome feature) | `crates/protocol/src/lib.rs` (`Welcome`, `FEATURE_*`) → `crates/daemon/src/server.rs` (`DAEMON_FEATURES`, `welcome_message`) → `apps/native/src/connection.rs` (`supports`) → `apps/native/tests/support/mod.rs` (`daemon_features`) |
| Add a native client view or dialog | a plain-Rust model module in `apps/native/src/` → its `*_view.rs` → mounted and routed in `apps/native/src/lib.rs` (`RootView`) → a spec module `apps/native/tests/ui/ui_<name>.rs` (with its `mod` line in `tests/ui/main.rs`) over `tests/support/mod.rs` |
| Add a sidebar container action or toolbar button | `apps/native/src/sidebar.rs` (the model, `native-ui.json` state) → `sidebar_view.rs` (the toolbar and its `⋯` menu, `container_row`) → `session_menu.rs` (`container_menu_layer`) → routed in `lib.rs` → `tests/ui/ui_sidebar.rs` (or a spec module of its own, as `ui_repos.rs`) |
| Ask the user for a folder in the native client | `apps/native/src/folder_picker.rs` (the client's own Windows picker, with a start folder), `lib.rs` (`FolderPicker`, which specs replace) |
| Change the sidebar or activity rail | `apps/native/src/sidebar.rs`, `sidebar_view.rs`, `activity_bar.rs`; the Needs You panel: `needs_you.rs`, `needs_you_view.rs` |
| End one-line text in `…` in the native client | `docs/native-client.md` ("Truncating text with `…`"), `apps/native/src/ellipsis.rs` (`ellipsized`, `truncating`, the debug probe), the view that draws the text, `apps/native/tests/ui/ui_ellipsis.rs` |
| Change the client's colours | `apps/native/src/palette.rs` (every UI colour), `theme.rs` (the terminal palette), `appearance.rs` (accent and background presets) |
| Change tabs or panes | `apps/native/src/tabs.rs`, `tab_bar.rs`, `tab_menu.rs`, `grid_view.rs`, `pane_menu.rs`, daemon `crates/daemon/src/tabs.rs` |
| Correlate a tab-layout request with its reply (`request_id` on `CreateTab`, `MergeTabs`, `MovePane`, `ExtractToNewTab`, `SplitPane`), the pending create, the closing mark, the in-flight tab-layout requests | `crates/protocol/src/lib.rs` (`ClientMessage` tab variants) → `crates/daemon/src/server.rs` (tab arms in `dispatch`, `send_tab_failure`, `move_pane`) → `apps/native/src/tabs.rs` (`arm_create`, `disarm_create`, closing marks) → `apps/native/src/notice_view.rs` (`fail_request`) → `apps/native/src/spawns.rs` → `apps/native/src/undo.rs` → `apps/native/src/pane_menu.rs` → `apps/native/src/pane_close_view.rs` → `apps/native/src/grid_view.rs` (`split_pane`) → `apps/native/src/move_panes_view.rs` → `apps/native/src/tab_bar.rs` → `apps/native/tests/ui/ui_tabs.rs`, `ui_panes.rs` → `apps/native/tests/support/mod.rs` |
| Change the terminal pane | `apps/native/src/term_view.rs`, `term.rs`, `term_input.rs`, `keys.rs`, `mouse.rs`, `links.rs`, `shell_marks.rs` |
| Change the spawn dialog | `apps/native/src/spawn_form.rs`, `spawn_view.rs`, `spawn_preview.rs`, `combobox.rs`, `spawns.rs` |
| Change how env rows are stored | [env-secrets.md](./env-secrets.md), `crates/protocol/src/env_rows.rs` (`is_secret_key`, `reference`) → `crates/daemon/src/env_secrets.rs` → `crates/daemon/src/server.rs` (`resolve_env_refs`, the `spawn_session` seal) → `user_env.rs` → `apps/native/src/spawn_form.rs`, `spawn_view.rs` |
| Prefill the spawn dialog from a session (Shift-duplicate) | `apps/native/src/session_menu.rs` (`duplicate_to`, `on_spawn_config`), `session_actions.rs` (`Prefills`), `spawn_form.rs` (`Lock`, `apply_prefill`), `spawn_view.rs` (`SpawnEntry::Duplicate`); protocol `GetSpawnConfig` / `SpawnConfigReply` |
| Change buttons, text fields, dialogs, menus or toasts | `apps/native/src/buttons.rs` (button and field looks, the focus ring), `session_menu.rs` (`dialog_button`, `bordered_button`, `menu_frame`, `popover_frame`, `backdrop`), `notice_view.rs` (`modal_panel`, `toast_frame`), `palette.rs` |
| Change session actions and menus | `apps/native/src/session_menu.rs`, `session_actions.rs`, `branch_fate.rs`, `discard_confirm.rs` |
| Change Settings | `apps/native/src/settings_view.rs`, `appearance.rs`, `appearance_view.rs`, `appearance_stops.rs`, `sidebar.rs` (`UiState` in native-ui.json), `lib.rs`; specs `tests/ui/ui_settings.rs`, `ui_appearance_editor.rs`, `ui_worktrees.rs` |
| Add or change keyboard stops, Tab order or a focus ring in a native dialog | `docs/native-client.md` ("Keyboard reachability") → `apps/native/src/buttons.rs` (`focus_ring`, `field_frame`, `field_frame_with_ring`) → the dialog's stop list (`settings_view.rs` `SettingsControl`, `settings_controls`, `step_settings_focus`, `on_settings_key`; `appearance_stops.rs`; `recover.rs` `focus_order`; `spawn_form.rs` `move_focus`; `worktrees_manager.rs`) → key routing in `lib.rs` → `tests/ui/ui_settings.rs` (the `settings_focus` probe) |
| Change source control or diffs | `apps/native/src/source_control.rs`, `source_control_view.rs`, `changes_view.rs`, `sc_writes.rs`, `stashes.rs`, `history.rs`, `diff_model.rs`, `diff_view.rs`, `diff_tab.rs`, `syntax.rs` |
| Run the tests | Everything: `.\rt.ps1 test` (`cargo nextest run --workspace --cargo-profile test-release`); the tests a change reaches: `.\rt.ps1 test -Reached`; UI specs: `cargo nextest run -p rustling-tulip-native --cargo-profile test-release --test ui ui_<name>` (`tests/ui/`); live e2e: `.\rt.ps1 native-e2e` (`tests/e2e_live.rs`, `tests/e2e_recover.rs`); OS smoke: `.\rt.ps1 native-smoke` (`tests/smoke_window.rs`) |
| Change how tests run or which tests a change reaches | `rt.ps1` (`Invoke-Test`), `tools/test-map.json`, `.config/nextest.toml`, `.pre-commit-config.yaml`, the `rustling-tulip-nextup` profile's gates (`.claude/skills/rustling-tulip-nextup/SKILL.md`) |
| Look at a UI change, or show before and after shots | `.\rt.ps1 native-shot <view>` (`apps/native/examples/shot.rs`: views `main`, `main-compact`, `source-control`, `diff`, `settings`, `spawn` over a scripted fake daemon); read the PNG it prints. A new view is a fixture plus the keys or rail click that reach it in that file |

## Non-goals

- Sub-agent / Task-tool interception or isolation (viewing a subagent's transcript read-only is planned).
- Auto-discovery of repos: the registry is manual.
- Attach beyond the LAN by internet exposure or SSH tunnelling. The mobile app's relay ([plans/mobile-app.md](./plans/mobile-app.md)) is the one planned way in from outside.
- Cloud sync of the registry or sessions.
