# Plan: Multi-Repo Claude Code Wrapper ("rustling-tulip")

> **Picking up work?** Phases 0–5 are done and Phase 6 is ~95% complete (only the `.git` filesystem watcher is still deferred — see below). All current work lives under the [Post-Phase-6 plan order](#post-phase-6-plan-order) section near the bottom — find the first unchecked item there and follow the linked detail plan in `docs/plans/`.

## Context

**What this is.** A Tauri desktop app that orchestrates many parallel Claude Code sessions across many repos — including coordinated multi-repo "workspace" sessions where a single Claude Code instance operates across several linked repos at once (e.g. `yaat` + `yaat-server`, `towercab-3d` + `towercab-3d-vnas`).

**The gap it fills.** Existing tools each solve part of the problem: `claude` itself supports multiple sessions but no cross-session dashboard; tmux/zellij multiplex but don't understand Claude state; Crystal manages parallel Claude sessions but is single-repo. None of them coordinate worktrees across multiple linked repos for cross-repo features, which is the user's primary unmet need.

**Outcome.** From one window: see every running session, its status/cost/last action; click to attach to any of them; spawn new sessions (single-repo or multi-repo workspace) with worktrees created automatically; daemon keeps everything alive across app restarts; OS notifications when an agent needs input.

## Decisions (already locked from interview)

| | |
|---|---|
| UI | Tauri desktop app (Rust backend + TS frontend) |
| Drive mechanism | Hybrid: PTY for interactive, `claude --print --output-format stream-json` for headless |
| Worktree integration | Manual: branch picker UI at spawn time |
| Persistence | Long-lived daemon owns processes; Tauri app is a client |
| Tracking | Status, token/cost, recent activity, diff preview |
| Notifications | OS notification + tray badge + dashboard highlight |
| Repo registry | Manually added via picker |
| "Spawn new agent" means | A new top-level `claude` session |
| Workspace branches | Same branch name across all members |
| Branch conflicts | Reuse where exists, create where doesn't |
| Cleanup | Per-worktree checkboxes, smart defaults |

## Architecture

```
┌────────────────────────┐         ┌────────────────────────────┐
│  Tauri app (client)    │  WS+JSON │  Daemon (long-lived)       │
│  - React/TS UI         │ ───────► │  - Session supervisor      │
│  - xterm.js panes      │ ◄─────── │  - PTY pool (portable-pty) │
│  - Tauri OS notify     │          │  - Stream-json parser      │
└────────────────────────┘          │  - Worktree orchestrator   │
                                    │  - Registry/state store    │
                                    └──────────┬─────────────────┘
                                               │ spawns
                                               ▼
                                    ┌────────────────────────────┐
                                    │  claude CLI processes      │
                                    │  (interactive or headless) │
                                    └────────────────────────────┘
```

- **Daemon**: standalone Rust binary, runs as a user service. Owns all PTYs, state, and child processes. Crashes/restarts of the Tauri app don't kill sessions.
- **Tauri app**: Rust backend handles WS connection to daemon + Tauri commands; React frontend renders dashboard, terminals (xterm.js), workspace/session forms.
- **Transport**: localhost WebSocket + JSON messages. Daemon writes its `port` and `auth_token` to `%APPDATA%\rustling-tulip\daemon.json`; clients read it. Single-machine, no network exposure.
- **No Anthropic API direct calls** — daemon always shells out to `claude` CLI. Keeps the boundary stable across Claude Code updates and avoids re-implementing its logic.

## Domain model

```
Repo:        { id, name, path, default_branch }
Workspace:   { id, name, member_repo_ids: [...] }   // optional grouping; sessions can target one
Session:     {
  id, label, kind: Single|Workspace,
  members: [{ repo_id, branch, worktree_path }],     // 1 entry for Single, N for Workspace
  status: Spawning|Idle|Working|AwaitingInput|Stopped|Error,
  mode: Interactive|Headless,
  process: { pid, started_at, exit_code? },
  metrics: { input_tokens, output_tokens, cost_usd, last_activity_at },
  recent_actions: [string; ring buffer of last N],
  notify: { os: bool, sound: bool },
}
```

Persistence layout under `%APPDATA%\rustling-tulip\`:

- `state.json` — repo + workspace registry only.
- `daemon.json` — WS handshake (port, auth token, daemon pid).
- `sessions/<id>/meta.json` — orphan-recovery sidecar (pid, label, kind, mode, members, started_at). Written at spawn, deleted on graceful stop.
- `sessions/<id>/scrollback.bin` — PTY/headless output ring buffer (2 MB cap, trims to 1.5 MB on overflow).
- `sessions/<id>/scrollback.truncated` — empty flag file present iff the ring overflowed at some point.

Sessions themselves are *not* listed in `state.json` — they live in the sidecar dir so the daemon can rebuild them on startup without re-reading a single big state blob.

## Spawn flows

### Single-repo session
1. User clicks "New session" → repo picker (from registry) → branch picker (existing + "new").
2. Daemon: `git -C <repo> worktree add <worktree_path> <branch>` (or `... -b <branch> <base>` if new).
3. Daemon spawns `claude` with `cwd = worktree_path` under a PTY. Streams stdout/stderr to client over WS.

### Workspace session (cross-repo)
1. User clicks "New workspace session" → workspace picker → enter branch name.
2. Daemon resolves per-repo state: for each member, branch exists? Show preview:
   ```
   yaat        feat/new-thing   (NEW from main)
   yaat-server feat/new-thing   (REUSE existing branch)
   ```
3. User confirms. Daemon creates worktrees in each member (reuse-or-create policy).
4. Daemon spawns one `claude` process: `cwd = members[0].worktree_path`, plus `--add-dir <path>` for each additional member. Single Claude Code session sees all worktrees.
5. Diff preview, cleanup, status — all aggregate across members.

## Component breakdown

### 1. `crates/daemon` — Rust daemon binary
- WS server (axum or tokio-tungstenite) on loopback only
- Auth: bearer token in connection upgrade headers
- Session supervisor: spawns/monitors `claude` processes, restarts on crash if configured
- PTY layer: `portable-pty` crate (Windows ConPTY support is mature)
- Stream-json parser: structured events from `claude --output-format stream-json` for headless mode
- Heuristic state detector for PTY mode: idle = no output for N seconds; awaiting-input = match known prompt regexes from `claude` TUI
- Cost tracker: parse `~/.claude/projects/.../session_*.jsonl` (Claude Code's own per-session log) for authoritative token/cost numbers — both modes write these
- Worktree orchestrator: shells out to `git worktree` directly (don't depend on user's `wt` tool)
- File watcher: on member worktree dirs, drives diff-preview
- State persistence: serde_json to disk on every mutation; load on startup

### 2. `apps/tauri-app` — Tauri 2 desktop app
- Rust side: WS client to daemon, OS notifications via `tauri-plugin-notification`, system tray via `tauri-plugin-system-tray`
- TS/React frontend:
  - Sidebar: repos, workspaces, sessions (with status badges)
  - Main pane: tabs/split for active sessions
  - Terminal renderer: `xterm.js` with addons (`fit`, `web-links`, `serialize`)
  - Spawn dialogs: single-repo and workspace flows
  - Diff preview panel (per session) using `git diff` output
- Single-window for v1; "pop out session to new window" deferred

### 3. Protocol (daemon ↔ client)
JSON over WS. Examples:
```jsonc
// client → daemon
{ "type": "list_sessions" }
{ "type": "spawn_session", "kind": "workspace", "workspace_id": "...", "branch": "feat/x" }
{ "type": "attach", "session_id": "..." }
{ "type": "send_input", "session_id": "...", "data": "..." }
{ "type": "stop_session", "session_id": "...", "cleanup": [{ "repo_id": "...", "remove_worktree": true }] }

// daemon → client
{ "type": "session_state", "session": { ...full snapshot... } }
{ "type": "pty_output", "session_id": "...", "data_b64": "..." }
{ "type": "metrics", "session_id": "...", "tokens_in": 1234, "cost_usd": 0.04 }
{ "type": "attention", "session_id": "...", "reason": "awaiting_input" | "stopped" | "error" }
```
Versioned: every message carries a `v` field; daemon rejects mismatched majors.

## Critical paths and external integrations

- **`claude` CLI invocation**:
  - Interactive: `claude` (no flags), PTY-attached
  - Headless: `claude --print --output-format stream-json --verbose -p "<prompt>"`
  - Multi-dir: append `--add-dir <path>` per extra member
  - Cwd determines the "primary" repo for the session
- **Git worktrees**: `git -C <repo> worktree add [-b <branch>] <path> [<base>]`; `git -C <repo> worktree remove <path>`
- **Claude Code session logs**: `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl` — the daemon tails these for authoritative token/cost data without needing to parse stdout

## File layout (proposed)

```
rustling-tulip/
├── Cargo.toml                      # workspace
├── crates/
│   ├── daemon/                     # standalone daemon binary
│   ├── protocol/                   # shared message types (serde)
│   └── pty-runner/                 # PTY abstraction over portable-pty
├── apps/
│   └── tauri-app/
│       ├── src-tauri/              # Rust side, depends on `protocol`
│       └── src/                    # React + TS + xterm.js
├── docs/plans/
│   └── milestones.md               # phased rollout (see below)
└── README.md
```

## Phased rollout

### Phase 0 — Immediate workspace launchers (`.ps1` wrappers)

Goal: ship a useful multi-repo Claude launcher today, before any of the daemon work begins. Proves the `--add-dir` model end-to-end and is what the user uses while the full app is being built.

Two scripts, one per workspace:

- [x] `X:\dev\yaat\claude-ws.ps1` — runs `claude` with `--add-dir X:\dev\yaat-server --dangerously-skip-permissions`
- [x] `X:\dev\towercab-3d\claude-ws.ps1` — runs `claude` with `--add-dir X:\dev\towercab-3d-vnas --dangerously-skip-permissions`

Both follow the same shape:

```powershell
#Requires -Version 7.0
$ErrorActionPreference = 'Stop'
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$Sibling   = Join-Path (Split-Path -Parent $ScriptDir) '<sibling-repo>'
if (-not (Test-Path $Sibling)) { throw "Sibling repo not found: $Sibling" }
& claude --add-dir $Sibling --dangerously-skip-permissions @args
```

Notes:
- Named `claude-ws.ps1` (not `claude.ps1`) so it never shadows the real `claude` command on PATH.
- `@args` splats any extra args through to `claude` (e.g. `-p "..."`, `--model`, etc).
- Exits with `claude`'s exit code naturally because `&` invocation and `$ErrorActionPreference = 'Stop'` propagate non-zero exits via PowerShell's `$LASTEXITCODE` semantics.
- Script resolves the sibling path relative to its own location, so it works whether the user's cwd is the primary repo, the sibling, or somewhere else entirely.
- These are **per-repo conveniences**, not committed across repos for now — list them in `.gitignore` if you don't want them tracked, or commit if you do (user's call per repo).

### Phase 1 — Daemon spine + single-repo PTY (MVP)
- [x] Workspace skeleton, `cargo deny` config, `clippy` pedantic lints, `oxlint`/`oxfmt` for TS
- [x] `protocol` crate: message types + version field
- [x] `daemon`: WS server, auth handshake, session supervisor, portable-pty
- [x] State persistence to `%APPDATA%\rustling-tulip\state.json`
- [x] Tauri shell: connect to daemon, list/spawn/attach single-repo sessions
- [x] xterm.js wired to PTY stream
- [x] Repo registry: add/remove repos, list branches
- [x] Manual smoke test: spawn 2 sessions in 2 repos, attach to each, type into them

### Phase 2 — Workspaces (the differentiator)
- [x] Workspace registry: define + list workspaces
- [x] Workspace spawn flow: branch input + per-member preview
- [x] Reuse-or-create branch policy with preview before commit
- [x] Multi-dir `claude` invocation (`--add-dir` per extra member)
- [x] Aggregate diff preview across member worktrees
- [x] Cleanup dialog with per-worktree checkboxes (default checked = clean, unchecked = dirty)
- [x] **VSCode `.code-workspace` auto-detect**: when adding a repo, scan for `*.code-workspace` files; if found, parse the `folders` array and suggest creating a matching rustling-tulip Workspace. Resolve relative paths against the workspace file's location. Optional file-watcher keeps members in sync when the user edits the `.code-workspace`.
- [x] Test on real workspaces: yaat/yaat-server, towercab-3d/towercab-3d-vnas (both have `.code-workspace` files)

### Phase 3 — Headless + structured state
- [x] `claude --print --output-format stream-json` adapter
- [x] Stream-json parser → structured events → session state
- [x] Headless session UI (no terminal, just structured event log + result panel)
- [x] Tail `~/.claude/projects/<...>/*.jsonl` for cost/token regardless of mode
- [x] Activity ring buffer feeding "last action" sidebar entries

### Phase 4 — Attention model
- [x] Awaiting-input detector for PTY mode (regex against known TUI prompts)
- [x] OS notifications via `tauri-plugin-notification`
- [x] System tray badge with attention count
- [x] Dashboard row highlight + sound (configurable)
- [x] Stopped/error transitions also fire notifications

### Phase 5 — Polish
- [x] Per-session config: model override, permission mode, env vars
- [x] Session log viewer (full transcript, scrollback persisted across restarts) — daemon-side ring buffer at `<sessions_dir>/<id>/scrollback.bin` (2 MB cap, trims to 1.5 MB with `.truncated` flag); replayed via `LoadScrollback`/`Scrollback` protocol messages on attach.
- [x] Pop-out session into its own Tauri window — `open_session_window` Tauri command opens a labeled window loading the same React bundle with `?session=<id>`; `App.tsx` branches on the query param to render `SessionWindow` (toolbar + reused `SessionPane`) instead of the full sidebar layout.
- [ ] Auto-update for the desktop app (`tauri-plugin-updater`) — **deferred** until a distribution channel exists (no GH Actions release pipeline, no signing cert, no hosted manifest). In-app pieces are ~2 hours of work when the pipeline is ready. → tracked in [Post-Phase-6 plan order](#post-phase-6-plan-order).
- [x] Crash-recovery: daemon detects orphaned `claude` processes on startup and reattaches state — sidecar `<sessions_dir>/<id>/meta.json` records pid + members + label at spawn; startup partitions live vs dead via `is_claude_alive` (sysinfo) and surfaces survivors via `SessionRegistry::insert_orphan` with `pty/headless = None`. Frontend shows a "PTY stream lost" banner via the new `is_orphan` field on `SessionSnapshot`.
- [x] **Persistent resizable panes**: every divider in the app (sidebar/main, terminal/git split, changes-list/diff split, headless-stats/log split) is drag-resizable with widths persisted to settings. Reuse a single `ResizableSplit` component so the behavior is consistent across panes.

### Phase 6 — Git tracking layer

A multi-repo git inspection surface on top of session worktrees. Same data is visible whether the session is single-repo or workspace-spanning; in workspace sessions every view groups by member repo.

- [x] Changed-files panel with two presentation modes:
  - **Flat view**: one row per changed file with `repo_name / path` + status (M/A/D/R/?), sortable by path or repo.
  - **Tree view**: collapsible directory tree per member repo, status icons on each node (file + directory rollup).
- [x] Inline file diff viewer: click a file → side-by-side or unified diff (`git diff` / `git diff --staged`).
- [x] Commit history panel per member repo: `git log --oneline -n <N>` with author / date / message / sha. Click a commit to see its files + per-file diff.
- [x] **Open in GitHub**: parse `git remote get-url origin`, support `https://github.com/...` and `git@github.com:...` forms, also GitLab and Bitbucket variants. Build URLs for:
  - File at HEAD on the current branch
  - File at a specific commit (with optional line range)
  - Commit page
  - Branch compare against base
- [x] Stage/unstage operations from the changed-files panel (checkbox toggle = `git add` / `git restore --staged`) — **shipped via the Source Control sidebar's Phase C** (iter 9, commit `3998040`). The per-session `GitPanel` is gone; staging now happens from the global `SourceControlSidebar` with per-row `+`/`−` buttons, bulk stage-all / unstage-all, a commit input above STAGED with `Ctrl+Enter` submit, and post-write `RepoStatus` broadcast so pop-out windows stay in sync.
- [x] Watch `.git` for changes to keep the views live without manual refresh — see `docs/plans/completed/git-fs-watcher.md`. Daemon spawns one debounced `notify`-driven watcher per registered repo; on any file change the supervisor re-runs `repo_status` + `stash_list` and broadcasts both through the existing `StateEvent::RepoStatus` / `StateEvent::Stashes` channels. Repo add/remove drives the per-repo watcher lifecycle.
- [x] Honor `.gitignore` and large-binary policy: don't try to render diffs >1 MB unless the user clicks "load anyway".

Daemon-side: extend the existing protocol with `ListCommits`, `GetCommit`, `GetFileDiff`, `StageFiles`, `RemoteUrl` (and corresponding `Daemon` responses). UI-side: a tabbed git panel in the session view, plus a top-level "Repo" tab for repos that aren't currently in any session.

## Post-Phase-6 plan order

Once Phase 6 closed, work moved to subordinate plans tracked under `docs/plans/`. This section is the canonical pick-up point: **an agent resuming work on this repo should find the first unchecked item below and read the linked plan.** Each entry names the detail plan, current state, and the exit criterion that ticks the box.

### Meta-decisions that shape this list

- **Protocol breaking changes are free.** Nothing has shipped yet, so subordinate plans that propose `#[serde(default)]` shims, dual-decode paths, or coordinated `PROTOCOL_VERSION` bumps can simplify: just change the wire format. The version bumps as needed (`PROTOCOL_VERSION` is at 10 as of iter 14 — Codex support, preset preview, terminal-title split, RepoStatus index/worktree split, TabContent enum, discard/stash, history-pagination offset have each rolled it forward); on-disk migration shims still exist where state.json shape changed (`OrphanMeta`, `RepoEntry`, `TabEntry`).
- **Don't reorder casually.** The order below is deliberate: each step either unblocks the next (e.g. config-dir isolation made the harness safe to iterate against) or is sequenced to keep small-and-isolated work ahead of large refactors.
- **New designs go in `docs/plans/*.md`.** Add a new entry to this section when you introduce a new plan, and tick the box when the entry's exit criterion is met.
- **Completed plans move to `docs/plans/completed/`.** When a plan's exit criterion is fully met (all tasks ticked, verification done, no in-flight deferred work that belongs to the *same* plan), `git mv` it under `docs/plans/completed/` rather than leaving it in the top-level. Keeps the active set scannable. Update any pointers in this file. *Deferred follow-up work belongs in a new plan, not as unchecked items in the moved one.*
- **Iter entries are one-liners (iter 49+).** Per-finding progress lives in `docs/ux-audit.md` as checkboxes. The iter prose below stays verbose for iters 14–48 (historical archive). From iter 49 onward each iter is a single line: `*Iteration N (sha):* headline. Audit items closed: X, Y. Detail in commit message.` Don't dump design rationale here when the commit message + audit doc + code already carry it.

### Ordered checklist

- [x] **E2E config-dir isolation** — see `docs/plans/e2e-test-coverage-strategy.md`. Plumb `RUSTLING_TULIP_CONFIG_DIR` through `crates/daemon/src/paths.rs::Dirs::ensure`, `apps/tauri-app/src-tauri/src/lib.rs::config_dir`, and the harness so the test daemon writes to `.tmp/e2e/config/` instead of real `%APPDATA%`. **Done in `094a2d3`** — verified the wdio smoke spec writes to the tmpdir and real `%APPDATA%\leftos\rustling-tulip\config\` is untouched.

- [x] **Codex support** — see `docs/plans/add-support-for-codex.md` (+ the "Implementation notes" section at the bottom for drift recorded post-pickup). Per-session agent choice (claude or codex) sharing the spawn/orphan/UI plumbing; headless stays claude-only. Wire format bumped to `PROTOCOL_VERSION 5` (no `#[serde(default)]` shims on wire types — only on on-disk artifacts: `OrphanMeta.agent`, `RepoEntry.last_agent`, `PresetEntry.agent`/`codex_sandbox`). **Code complete and all automated checks green** (cargo fmt/clippy/test workspace-wide, pnpm typecheck). Manual e2e against a real codex install still pending — including the unresolved question of whether interactive `codex` (not just `codex exec`) accepts `--add-dir` for workspace multi-root sessions. If interactive codex rejects `--add-dir`, fix is a one-line change in `build_codex_args` in `crates/daemon/src/server.rs`.

- [ ] **UX audit triage** — see `docs/ux-audit.md` and the failing-test→fix→green candidates listed in `docs/plans/e2e-test-coverage-strategy.md:121-138`. Close the testid gaps in `PresetLaunchDialog`, `SpawnDialog`, `GitPanel` first so specs read clean, then drive the top audit findings through wdio specs. Ongoing — stop when the polish-to-feature ratio gets uncomfortable, then move to the sidebar.

    *Iteration 1 (this commit):* testid sweep on `PresetLaunchDialog` / `SpawnDialog` / `GitPanel` (plus `SessionPane` view-toggle so the user-select spec can reach the git tab); `user-select: text` rule for `.diff-pane` / `.headless-log` / `.preset-preview-list` / `.terminal-host` / `.session-title h2` in `apps/tauri-app/src/styles.css`; daemon-round-trip fix for file/folder preset previews — new `ClientMessage::PreviewPreset` + `DaemonMessage::PresetPreview` / `PresetPreviewError` variants (`PROTOCOL_VERSION` 5 → 6), `presets::preview_prompts` wrapping the existing `resolve_prompts`, server handler at `crates/daemon/src/server.rs::handle_client_message`, `previewPreset` helper in `apps/tauri-app/src/api.ts`, and an async preview-stage path in `PresetLaunchDialog.tsx` with loading/error states and ticket-based race protection. Two new wdio specs (`tools/e2e/tests/e2e/specs/preset-launch-folder.spec.ts`, `user-select.spec.ts`); the full multi-spec suite (preset-launch + user-select + webview) runs green in a single `pnpm test`. Also synced the stale `tools/e2e/src/handshake.ts` constant (4 → 6) and added pid-liveness probing to `readHandshake` so stale `daemon.json` files from prior wdio sessions are rejected, letting `waitForHandshake` poll until the new supervisor writes a fresh one; webview spec also caught up to the Codex protocol bump (`agent` + `codex_sandbox` fields on `spawn_session`). Next slice candidates: open-in-forge branch URL, daemon error toast.

    *Iteration 2:* **Open-in-forge branch URL** — `apps/tauri-app/src/api.ts::branchUrl` builds `/tree/<branch>` (github), `/-/tree/<branch>` (gitlab), `/src/<branch>` (bitbucket); falls back to repo home for unknown forges. Also fixed the listener leak in the old inline `openInForge` by promoting it to a Promise-based `getRemoteUrl(client, repoId)` helper in `api.ts` (2s timeout + per-call cleanup, matching the `loadScrollback`/`listPresets`/`previewPreset` pattern). **Daemon error toast** — new `apps/tauri-app/src/components/ErrorToast.tsx`, bottom-right stacked, error (red) + warning (amber) severity, 8s auto-dismiss with manual `×`. Routed: `DaemonMessage::Error` (red), `preset_launch_failed` (amber with partial counts). Spawn-failure correlation deferred — toasting all daemon Errors already surfaces spawn-fail messages, just without a "spawn failed" label. One new wdio spec (`error-toast.spec.ts`) exercises the real path via a dev-only `window.__rt_daemon_client` global. Harness gains: `RUSTLING_TULIP_OFFSCREEN_WINDOW=1` env-flag and `env_flag()` helper in `apps/tauri-app/src-tauri/src/lib.rs` so test windows are positioned at (-32000, -32000) with skip-taskbar — no more app popups during test runs; per-session daemon reset in `wdio.conf.ts::beforeSession` (calls `shutdownExistingDaemon` + unlinks `daemon.json` before each spec) to fix daemon accumulation across specs. Full multi-spec suite (error-toast + preset-launch + user-select + webview): 4 spec files, 11 tests, all green. Next slice candidates: stage/unstage protocol wire-up (or skip and head to the Source Control sidebar).

    *Iteration 3:* **Confirmations on destructive actions.** Workspace remove + repo remove (when no live sessions) + tab close (when bound sessions present) + "Close other tabs" all gained inline two-state confirms in the SessionPane-Stop pattern. Repo remove **with live sessions** opens a new `RepoRemoveDialog` modal with 3-way choice: Cancel, Remove anyway (sessions move to Detached), Stop sessions and remove. New `tabHasBoundSessions` helper in `utils/grid.ts`. Two daemon-side fixes pulled in as collateral: (1) `write_handshake` now uses a pid-tagged tmp file (`daemon.json.tmp.<pid>`) so concurrent daemons don't stomp each other's mid-write file during wdio runs; (2) **repo/workspace updates now broadcast** via a new `state_events: broadcast::Sender<StateEvent>` on `Hub` plus a per-client `spawn_state_forwarder`, replacing all the per-client `out_tx.send(DaemonMessage::Repos/Workspaces)` calls — closes the audit's "registry updates aren't broadcast to all clients" finding. New wdio spec (`repo-remove-confirm.spec.ts`) drives the 3-way modal end-to-end. Full suite: 5 spec files, 13 tests, all green. Next slice candidates: real-user hand-test findings appended below (onboarding, identity, workspace preview layout, Detached banner, keyboard/focus, pane control crowding) — see `docs/ux-audit.md` "Findings from hand-test 2026-05-11".

    *Iteration 4:* **Identity and trust** (per the hand-test feedback's "fix identity and trust first" priority). Four fixes: **(A)** sidebar `+ Session` is disabled when `repos.length === 0` with an explanatory `title`; the dead-end-state `EmptyState` in `App.tsx` now renders a primary "+ Add repo" button instead of just a text hint, so the only path from a fresh install is the right one. **(B)** the OSC-title parser no longer overwrites `rec.label` — the canonical name set at spawn time (`<repo>:<branch>` or user override) stays sticky. Terminal-emitted titles like `C:\WINDOWS\system32\cmd.exe` now land on a new optional `SessionSnapshot.terminal_title` field (`#[serde(default)]`, mirrored in `OrphanMeta` so orphan reattach restores the annotation) and surface as a tooltip in `SessionPane` / `Sidebar` / `SessionWindow` only when distinct from the label. **(C)** the workspace spawn `.preview-table` had `overflow: auto` without flex-shrink protection, so it collapsed to a few pixels when other modal content competed for space — pinned `flex: 0 0 auto` + `min-height: 80px`. **(D)** the Detached bucket now renders a `tree-children-banner` explaining why sessions land there (repo/workspace removed, or repo rolled into a workspace post-spawn). No new wdio specs this iter — changes are presentation/state-shape and covered by the existing suite running green. Next slice candidates: keyboard/focus (autofocus modals, tabIndex on leaves, aria-label on icon buttons, useEscape hook), pane control crowding (`⠿`/`▶|`/`▼=`/`×` show-on-hover redesign), and the new ask from the user mid-iter: **tab/sidebar bidirectional sync** — reflect tab membership in the treeview so terminals can be moved between tabs by manipulating the tree (see `docs/plans/` follow-up; substantial enough for its own plan).

    *Iteration 5:* **Tab ↔ sidebar bidirectional sync** — see `docs/plans/completed/tab-sidebar-sync.md`. User picked the dual-tree toggle option. Sidebar header gains a `[Repos | Tabs]` segmented control (`sidebar-view-toggle`, persisted via `localStorage` key `rt.sidebar.view`). New `buildTabContainers(tabs, sessions)` in `Sidebar.tsx` renders tabs as top-level containers (kind: `tab`) with a trailing `unbound` pseudo-container. Every session leaf gains an inline `TabPill`: `T:<tab-name>` (one binding), accent `T:×N` (multiple — title attr lists), or muted-italic `[unbound]` button that fires `create_tab { initial_session_id }` to rebind. Bound leaves are `draggable={true}` with the same `text/x-rt-pane` payload format the grid uses, so existing tab-pill + pane drop targets accept the drag without further wiring. Helpers `sessionTabBindings` + `firstLeafPane` added to `utils/grid.ts`. One new wdio spec (`sidebar-tab-view.spec.ts`, 3 tests): toggle defaults to Repos, bound session shows `T:<name>` pill + flips to tab-view container, clicking `[unbound]` after a close rebinds via `create_tab`. Full suite: 6 spec files, 16 tests, all green. Deferred to follow-up: right-click context menu on session leaves, "bind to existing tab" picker (vs always creating a new tab), container-level drag in tab-view.

    *Iteration 6:* **Keyboard / focus accessibility + pane control tone-down.** New `apps/tauri-app/src/utils/a11y.ts` exposes `useEscape(handler, enabled)` (document-level keydown subscription) and `useAutoFocus(ref)`. All three modals (`SpawnDialog`, `RepoRemoveDialog`, `PresetLaunchDialog`) now close on Escape and add `role="dialog"` + `aria-modal="true"` + `aria-label`. `RepoRemoveDialog` autofocuses its Cancel button (least-destructive default — Enter no longer commits a destructive action by accident). Both context menus (`ContainerContextMenu` in `Sidebar.tsx`, `TabContextMenu` in `TabBar.tsx`) also close on Escape. Session leaves gain `tabIndex={0}` + `aria-label={"Session " + label}` + Enter/Space onKeyDown to match click; expandable container headers gain `aria-expanded` + `aria-label` + the same keyboard-toggle wiring. `[unbound]` pill, pane split/close buttons, and SpawnDialog × all gain explicit `aria-label`s on top of their `title` attrs. Pane controls (`⠿`/`▶|`/`▼=`/`×`) now sit at `opacity: 0.45` for focused panes (was full opacity), rising to 1 on hover or `:focus-within` — the focused pane's controls still discoverable but quieter at the typing cursor. New wdio spec (`keyboard-a11y.spec.ts`, 3 tests): Escape dismisses RepoRemoveDialog + Cancel is autofocused, session leaves are tabIndex=0 with role=button + aria-label, container headers expose aria-expanded. Full suite: 7 spec files, 19 tests, all green. Deferred: focus-trap inside modals (Tab/Shift-Tab cycling stays scoped to the modal), arrow-key navigation between sidebar leaves, and a true icon overhaul for the pane controls (current symbols are still glyphs, just labeled).

    *Iteration 7:* **Source Control sidebar — Phase A** (`docs/plans/source-control-sidebar.md` Phase A, partial). The per-session `Terminal | Git` toggle is gone; git access is now a global, activity-bar-driven sidebar. New `apps/tauri-app/src/components/ActivityBar.tsx` (left rail, two icons: Sessions / Source control, persisted via `localStorage` key `rt.activity`). New `apps/tauri-app/src/components/source-control/SourceControlSidebar.tsx` mirrors the deleted `GitPanel`'s data plumbing (`repo_status` / `list_commits` / `get_file_diff` / `get_commit`) but driven by the focused pane's first member repo (`apps/tauri-app/src/App.tsx::focusedRepoId` derivation), with a manual override picker (key `rt.sourceControl.repoOverride`) for when multiple repos are registered. `App.tsx` layout becomes `<ActivityBar /> + <ResizableSplit>(<Sidebar | SourceControlSidebar />, <main>)</ResizableSplit>`. `apps/tauri-app/src/components/GitPanel.tsx` deleted. The `view: "git"` toggle removed from `SessionPane.tsx`. New wdio spec (`source-control-sidebar.spec.ts`, 4 tests): ActivityBar renders with Sessions active by default, clicking Source Control swaps sidebars + persists the choice, the source-control sidebar shows the registered repo + Changes tab by default, and `session-view-git` testid no longer appears anywhere. Full suite: 8 spec files, 23 tests, all green. **Phase A partial**: deferred sub-items in the source-control plan are the path-folded `ChangesTree`, paginated `GraphList`, dedicated `RepoHeader` split-out, and the `RepoStatus` index/worktree status protocol split. Each of those moves with its own dependent phase (B–E) or in a "Phase A part 2" follow-up.

    *Iteration 8:* **Source Control sidebar — Phase A part 2: path-folded ChangesTree.** New `apps/tauri-app/src/components/source-control/ChangesTree.tsx` replaces the flat list in the source-control sidebar's Changes view with a VSCode-style recursive tree. Helper `apps/tauri-app/src/utils/changesTree.ts::buildChangesTree(changes)` projects the flat `GitFileChange[]` into a hierarchy and folds single-child intermediate folders (`apps/tauri-app/src/components` collapses to one row). Each file row keeps its `M`/`A`/`D`/`U`/`R` status badge, now color-coded; each folder row shows the aggregate file count. Per-folder expand/collapse via a `Set<string>` collapsed-paths in component state (folders default to expanded, matching the existing sessions tree). Keyboard accessible: every row is `tabIndex=0` with Enter/Space handlers and `aria-expanded`/`aria-label`. No protocol changes (existing `repo_status` payload is sufficient). Suite stays at 8 specs / 23 tests — the existing source-control-sidebar spec covers the empty-tree path and the tree's rendering surface is layered onto existing testids (`source-control-changes-list` is still the host).

    *Iteration 9:* **Source Control sidebar — Phase C: stage / unstage / commit** (commit `3998040`). `PROTOCOL_VERSION` 6 → 7. `RepoStatus` reply splits into `index_changes` + `worktree_changes` (porcelain X/Y columns) so the sidebar can render VSCode-style STAGED + CHANGES sections independently — a file with both staged and unstaged edits appears in both buckets with different status chars. New `crates/daemon/src/git_write.rs` module shells `git add --` / `git restore --staged --` / `git commit -m` and replies with `CommitOk { repo_id, sha, short_sha }` (success) or `GitWriteError { repo_id, operation, error }` (failure). Post-write status refresh fans out via a new `StateEvent::RepoStatus` broadcast so pop-out windows see the change without re-requesting. UI: STAGED + CHANGES bucket headers with bulk stage-all / unstage-all + per-row `+`/`−` actions revealed on hover/focus, commit message textarea with `Ctrl+Enter` submit, inline error banner + toast on `GitWriteError`. New wdio spec (`source-control-write.spec.ts`, 4 tests): unstaged change in worktree bucket → stage flips it to STAGED → unstage flips it back → stage again + Commit succeeds and worktree goes clean. New parser unit tests in `git_inspect.rs` cover the X/Y bucket split, rename follow-up entries, and the empty input. Full suite: 8 specs / 24 tests, all green.

    *Iteration 10:* **Pre-paint offscreen window position for e2e** (commit `da4d000`). The previous fix moved the window to `(-32000, -32000)` from the Tauri `setup` hook, but `setup` runs *after* the window has been created at its `tauri.conf.json` default position, so the window briefly flashed on screen before the move. Fixed by mutating `Context::config_mut()` to set `window.x` / `y` / `skip_taskbar` *before* `Builder::run`, so the window is constructed offscreen from frame zero. The `setup` hook just logs env state now. Gated on `RUSTLING_TULIP_OFFSCREEN_WINDOW`, so normal users see the window appear at the default position as before.

    *Iteration 11:* **TabContent enum refactor** (`docs/plans/source-control-sidebar.md` Phase B iter 11, commit `26999a3`). Prerequisite for diff tabs: replace `TabEntry { id, name, grid, created_at }` with `TabEntry { id, name, content: TabContent, created_at }` where `TabContent::Grid { grid }` is the only variant for now. Adds a custom `Deserialize` impl that migrates legacy state.json (top-level `grid` field) so existing on-disk state keeps loading. Helper methods `TabEntry::grid()` / `grid_mut()` return `Option` for callers that mutate panes; daemon pane handlers (split, close, ratio, replace, move, merge, extract, prune) thread through new `tabs::grid_or_err` / `grid_or_err_mut` helpers so a future non-Grid tab kind cleanly errors instead of silently no-oping. `PROTOCOL_VERSION` 7 → 8. Behavior preserved — no new features, no Diff variant yet. Sets up iter 12 (Monaco diff tabs) which adds the Diff variant without further reshaping the protocol.

    *Iteration 12:* **Source Control sidebar — Phase B: Monaco diff tabs from the sidebar** (commit `94ac0d6`). Architecture shift from the original Phase B plan: a diff is its own **tab kind** (`TabContent::Diff { repo_id, path, against }`), not a pane inside a grid (user pref). Pane-level operations are unchanged. New protocol messages: `ClientMessage::OpenDiffTab { id, repo_id, path, against }` + `DaemonMessage::DiffTabOpened { id, tab_id }` (dedup keyed on the triple — clicking the same file twice focuses the existing tab); `ClientMessage::GetFileSnapshot { id, repo_id, path, against }` + `DaemonMessage::FileSnapshot { id, repo_id, path, against, old, new, language }` + `DaemonMessage::FileSnapshotError`. Daemon helpers: `git_inspect::file_snapshot` (dual-side fetch — worktree+index, HEAD+index, or sha-anchored) + `git_inspect::language_for_path` for the Monaco language hint. Eager `monaco-editor` bundle with the editor worker wired through Vite's `?worker` import (`apps/tauri-app/src/utils/monacoSetup.ts`); language tokenizers auto-split into per-language lazy chunks. New `apps/tauri-app/src/components/DiffPane.tsx` wraps `monaco.editor.createDiffEditor` (read-only, side-by-side, vs-dark). `App.tsx` + `TabWindow.tsx` branch on `tab.content.kind`; pop-out support is automatic — the existing `open_tab_window` + `?tab=<id>` plumbing carries diff tabs through unchanged. `SourceControlSidebar`'s `ChangesView` click handler sends `open_diff_tab` and activates the returned tab id; the inline `<pre>` diff is gone. `TabBar` adds a `Δ` glyph + `tab-pill-kind-diff` styling so diff tabs are visually distinguishable from grid tabs. New wdio spec (`source-control-diff.spec.ts`, 3 tests): clicking a changed file opens a diff tab, clicking the same file again focuses the existing tab (no duplicate), closing the diff tab removes it. Bundle: main chunk 165KB → 1.13MB gzipped (eager Monaco). Full suite: 9 specs / 27 tests, all green.

    *Iteration 13:* **Source Control sidebar — Phase D: discard + stash.** `PROTOCOL_VERSION` 8 → 9. New protocol surface: `ClientMessage::DiscardChanges { repo_id, paths }`, `StashPush`, `ListStashes`, `StashPop`, `StashApply`, `StashDrop`, plus `DaemonMessage::Stashes { repo_id, stashes: Vec<GitStash> }` and a new `GitStash { id, subject, created_at }` type. Daemon-side `git_write::discard` partitions input via `git status --porcelain -z` into tracked (→ `git restore -- <paths>`) vs untracked (→ `git clean -fd -- <paths>`) — matches VSCode's "discard" semantics without conflating it with unstaging. Stash helpers shell to `git stash push -u [-m <message>]` / `list --format=%gd%x1f%gs%x1f%aI` / `pop`/`apply`/`drop`. New `handle_stash_write` helper on the server mirrors the existing `handle_git_write` but broadcasts both a fresh `RepoStatus` and a fresh `Stashes` snapshot via a new `StateEvent::Stashes` variant, so multiple sidebar instances stay in sync after any push/pop/drop. UI: per-row `↺` discard glyph (red on hover) alongside the `+` stage glyph in the worktree bucket; `DiscardConfirmDialog` modal lists the affected files and pops a destructive `Discard N changes` button with Cancel autofocused (Escape-dismissible via `useEscape`). New `StashesSection` (collapsible, between CHANGES and the open-in-forge footer) with a message input + Push button in the header and per-row Pop/Apply/Drop link-style actions. `ChangesTree` extended from a single optional `rowAction` to a `rowActions: RowAction[]` so the worktree bucket can stack two actions per file; `BucketHeader` similarly accepts an array of actions so the worktree bucket gets a `Discard all` next to `Stage all`. Full workspace clippy + 83 tests still green; `pnpm typecheck` clean. No new wdio specs this iteration — Phase D's destructive actions are hard to drive cleanly in the headless harness (the discard modal needs a real dirty worktree the spec controls), and the surface is small enough that the existing `source-control-write.spec.ts` style would just be repetition. Defer the wdio coverage to a follow-up that sets up a dedicated test repo fixture.

    *Iteration 57:* **Persistent terminal font size at app/tab/session scopes.** Settings modal slider for app-wide default (`Settings.terminal.font_size`), tab right-click menu and session right-click menu carry Increase / Decrease / Reset items, Ctrl+= / Ctrl+- adjust focused session, Ctrl+Shift+= / Ctrl+Shift+- adjust active tab, Ctrl+0 resets both. Resolver walks session→tab→app-default; live updates via CustomEvent so xterm `options.fontSize` mutates in place + `fit()` + daemon resize without remounting (scrollback + PTY preserved). `pruneOverrides` drops stale entries on tab/session close.

    *Iteration 56:* **Rearrange tab panes + auto-grid preset layout.** New `ClientMessage::RearrangeTab { tab_id, layout }` + `RearrangeLayout` enum (horizontal / vertical / balanced / grid) so a tab's layout can be flipped without re-spawning. Tab right-click → Rearrange panes appears when the tab has 2+ panes. Ctrl+Shift+G rearranges the active tab to auto-grid. New `TabLayout::AutoGrid` preset variant builds a true 2D rows-of-columns tree (vs `BalancedHorizontal` which produces a 1D strip with uneven column widths); yaat bug-triage preset switched to `auto_grid` so 6-prompt launches arrange 3×2. Protocol 14 → 15.

    *Iteration 50:* **Ctrl+V paste pass-through + Shift+Enter newline.** Terminal.tsx custom-keymap handler now returns `false` for Ctrl/Cmd+V so xterm bails out of its keydown processing without `preventDefault()`-ing the event; the browser then fires its native `paste` event on xterm's helper textarea and xterm's built-in paste DOM-listener handles it (covers Ctrl+V, Ctrl+Shift+V, Shift+Insert, right-click — no `navigator.clipboard.readText` permission prompt; bracketed-paste mode honoured). Shift+Enter sends `\` + CR with explicit `event.preventDefault()` so the textarea doesn't also get an Enter inserted; Claude/codex TUIs read the sequence as "newline-in-input without submit", bash reads it as a line continuation. Matches the `terminal-setup` bindings Claude Code injects into VS Code. Audit item refined: copy/paste hotkeys + multiline (iter 44 + 50).

    *Iteration 49:* **Settings modal (Notifications / Sidebar / Spawn defaults).** New `utils/settings.ts` (single localStorage key `rt.settings`, migrates legacy `rt.sidebar.view`) + `components/SettingsModal.tsx`. Trigger: ⚙ in sidebar header + `Ctrl/Cmd+,` shortcut. Notification handler now gates `sendNotification` on the per-event toggle. SpawnDialog reads `skip_permissions_default` / `default_permission_mode` / `default_codex_sandbox` for initial form state. Audit items closed: Notification permission re-request. Detail in commit.

    *Iteration 48:* **Drag-source pill visual feedback.** One audit finding (partial) closed. While dragging a tab pill, the source pill now gets an `is-dragging` class (opacity 0.45 in CSS). Pre-iter-48 the source pill looked identical to its neighbours during the drag, so dropping on self (which is a silent no-op) gave the user no visible cue about what they'd grabbed. The reciprocal CSS rule sits next to the existing `.drop-before` / `.drop-after` indicators. The audit's broader "no feedback either" gripe is now addressed: source = 45% opacity, target = accent-coloured inset edge. Self-target = source pill stays dim with no edge highlight ⇒ "you're hovering yourself, nothing will happen" is now legible at a glance.

    *Iteration 47:* **Preset preview tab grouping.** One audit finding closed. The audit asked: "Preview stage says `→ X tabs` but cannot tell the user when each tab will hold which prompts." New `PreviewPromptList` component in `PresetLaunchDialog.tsx` renders the prompt list grouped by destination tab when a cap is set (and multiple tabs would result) — each chunk gets a `Tab N of M` header above its rows. Falls through to the original flat list when `cap === null` or `prompts.length <= cap` (single-tab case). Uses the same sequential `prompts.slice(i, i + cap)` partition the daemon applies in `presets.rs::launch`. New `.preset-preview-group` + `.preset-preview-group-header` styling (dashed-bottom muted-uppercase label).

    *Iteration 46:* **Shift-click splits left/up + folder-picker path-separator normalization.** Two audit findings closed. **(A)** `PaneChrome.sendSplit` now takes `place` as a parameter; the split-right (▶|) and split-down (▼=) buttons read `e.shiftKey` and pass `"first"` instead of `"second"`. Tooltips advertise the modifier (`"Split right (Shift+click: split left)"`); `aria-label`s are spelled out for screen readers. Closes the audit's "`split_pane` is hard-wired to `place: 'second'`" finding without adding two new buttons of glyph clutter. **(B)** `PresetLaunchDialog.joinPath` now detects Windows base paths (drive letter `^[A-Za-z]:` or UNC `\\server`), strips trailing/leading separators, and normalises the relative half to the native separator. Pre-iter-46 a Windows base like `X:\dev\foo` joined to `bar/baz` produced `X:\dev\foo/bar/baz` (mixed) on the heuristic miss; now it consistently produces `X:\dev\foo\bar\baz`. Empty `rel` short-circuits to `trimmedBase` so we don't emit a trailing slash.

    *Iteration 45:* **Tab close confirm for multi-pane tabs + pane drop on tab pill.** Two audit findings closed. **(A)** `onCloseTab` now arms the existing two-state confirm when the tab has 2+ panes too, not only when it carries bound sessions. Closing a multi-pane tab destroys the persisted split tree even when no sessions are bound, and recreating it by hand is tedious — the audit explicitly asked for this gate. New helper `tabPaneCount(tab)` in `utils/grid.ts`. **(B)** Tab pills now accept `text/x-rt-pane` drops in addition to `text/x-rt-tab`. The handler resolves the target tab's first leaf pane via `firstLeafPane` and routes a `move_pane` with `edge: "right"` so the dragged pane becomes a new sibling on the right of the target. Self-target (the same pane already being the destination's first leaf) is a no-op so the parent split doesn't churn. Pre-iter-45 the pill activated the target tab on dragenter but silently dropped the gesture, forcing users to drag through the pill to a pane in the activated tab — awkward when the target had no visible drop edge.

    *Iteration 44:* **Terminal Ctrl+Shift+C/V + exit-dialog stuck-daemon warning.** Two audit findings closed. **(A)** `Terminal.tsx` now binds copy/paste via xterm's `attachCustomKeyEventHandler`: Ctrl+Shift+C copies the current selection to the OS clipboard (no-op when nothing's selected, falls through to default behaviour); Ctrl+Shift+V reads from the clipboard and forwards the contents through `send_input` (gated by the same `statusRef` guard the keystroke path uses, so a paste into a dying PTY is skipped). Both handlers return `false` to keep xterm from sending `\x03`/`\x16` instead — bare Ctrl+C/V still work since the modifier check requires Shift. **(B)** `ExitConfirmDialog` gains `stuck` + `onForceQuit` props; the App's `onStopAndQuit` no longer auto-closes the window after 2 s. Instead a stuck-timer flips `exitStuck` so the dialog swaps to a warning banner ("Daemon not responding after 2 seconds…") and a single `Force quit` button. Pre-iter-44 the 2 s timeout silently dropped the user out while the daemon was still up, which then surfaced as "daemon supervisor failed" on the next launch. New `.modal-warning` CSS rule (warn-tinted background + border, code-tag styling for inline `<code>` references). No protocol changes; no new wdio specs (the stuck path needs a wedged daemon fixture that's not yet in the harness).

    *Iteration 43:* **Workspace path prelude for the agent.** When a workspace session spawns with 2+ members, the daemon emits a small note mapping each member's `repo_name → worktree_path` so the agent doesn't follow stale absolute or `..\<sibling>` paths from `CLAUDE.md` / `AGENTS.md`. New `build_workspace_prelude(&[SessionMember]) -> Option<String>` helper returns `None` for single-member sessions. **Claude** (interactive + headless): the prelude rides on `--append-system-prompt` so it's invisible to the user. **Codex** (interactive only — codex has no headless mode here): the prelude is prepended to the positional prompt with a blank-line separator, since codex exposes no system-prompt flag. When no user prompt is set, the prelude alone becomes the prompt. Skipped entirely when a `prompt_injector` is attached (preset launches own their prompt delivery). Verified end-to-end against yaat + yaat-server worktrees: with claude the dynamic-system-prompt + `--add-dir` already covered the cross-repo path, so the prelude is defensive belt-and-suspenders; with codex the prelude converts a silent broken-path failure (codex synthesized `X:\dev\yaat.wt\yaat-server` from CLAUDE.md's `..\yaat-server` reference — a path that doesn't exist) into a correct answer. 71 daemon tests green (4 new); workspace clippy clean.

    *Iteration 42:* **Worktree-default semantics + spawn loading state.** Two audit findings closed. **Worktree toggle** — `toggleUseWorktree` in both forms drops the `client.send({type: "set_*_worktree_default"})` that fired on every toggle. The choice stays local to the dialog; the submit handler now sends the daemon update only when `useWorktree` differs from the persisted default (so Cancel leaves the prior default untouched). **Spawn loading** — App's `onSpawned` now pushes an info-severity toast `Spawning session… · Worktree creation may take a few seconds.` so the user has visible feedback during the daemon round-trip for worktree resolution. The dialog still closes immediately (keeping it open with a spinner was rejected — the user should be able to queue another spawn without waiting for the previous one's worktree).

    *Iteration 41:* **Branch suggestion stickiness + tab reorder optimistic.** Two audit findings closed. **Branch suggestion** — `useBranchField` adds a required `targetKey` arg (`repo:<id>` / `workspace:<id>`) and reads/writes a module-scoped `branchSuggestionCache: Map<string, string>`. Cancel-then-reopen for the same target preserves the suggestion; user-edit clears it (next reopen suggests fresh); successful submit clears it too (next spawn for the same target starts clean). Cache is session-scoped (module-level) so it doesn't pollute localStorage. **Tab reorder optimistic** — TabBar's drop handler invokes a new App-level `onLocalReorder(orderedIds)` callback that reshuffles `state.tabs` immediately, then sends `reorder_tabs` to the daemon. The existing `tabs_reordered` reducer reconciles on broadcast (no-op for same-order). Defensive: unknown ids filtered, forgotten tabs appended.

    *Iteration 39 + 40:* **aria-label sweep + PTY input guard + history placeholder.** Four audit findings closed across two batched iterations (combined commit). PresetLaunchDialog's close ✕ gains `aria-label="Close dialog"` — the last unlabeled close × in the audit. Terminal threads `session.status` through a `statusRef` and the `onData` handler skips `send_input` when status is `stopped`/`error` — closes the keystroke-to-dying-PTY leak. Source Control sidebar's history `DiffView` gains an optional `placeholder` prop; the history call site passes "Select a commit to view its diff." (the old shared "select an item" generic). Changes-view diffs already moved to Monaco diff tabs in iter 12 so they don't hit the placeholder path at all.

    *Iteration 38:* **Preset menu failure state + spawn dialog autofocus.** Two audit findings closed. **Preset submenu** — `listPresets` now returns a discriminated `{ ok: true, entries } | { ok: false, reason }` instead of `PresetEntry[]`; the 2s timeout resolves with `{ ok: false, reason: "timed out (daemon not responding)" }`. Sidebar's preset cache stores the discriminated result; the submenu renders "Launch preset… (failed to load)" with the reason as a tooltip when `!ok`, distinct from "(none defined)" (`ok && entries.length === 0`) and "(loading)" (cache miss in-flight). **Spawn dialog autofocus** — `SingleForm` and `WorkspaceForm` add a `useAutoFocus(branchInputRef)` so keyboard users land on the most-edited field with the random worktree name preselected (already an `<input value>` so it's selected for overwrite). PresetLaunchDialog intentionally stays without autofocus — its source-stage radio group is the natural first focus target.

    *Iteration 37:* **Pane controls hover-only + delete SessionDiff dead code.** Two audit findings closed. **Pane controls** — dropped the `.is-focused .grid-pane-controls { opacity: 0.45 }` baseline rule that left the drag/split/extract/close glyphs half-visible on the focused pane regardless of cursor position. They now fade in only on `:hover` or `:focus-within`, so on narrow panes they no longer overlap the session header's Pop-out/Stop buttons. Keyboard users still reach them via Tab (focus-within at full opacity). **SessionDiff** — deleted the dead protocol surface and its implementation: `ClientMessage::SessionDiff` + `DaemonMessage::SessionDiff` + `MemberDiff` struct from `crates/protocol/src/lib.rs`; server handler + `compute_session_diff` async fn from `crates/daemon/src/server.rs`; dispatch case + `MemberDiff` interface from `apps/tauri-app/src/types.ts` and `App.tsx`. `PROTOCOL_VERSION` bumped 10 → 11; handshake.ts mirror updated. The Source Control sidebar handles per-repo dirty-file display, making the per-session aggregate redundant. Workspace clippy + 84 tests green; pnpm typecheck clean.

    *Iteration 36:* **Cap headless recent_actions list.** Two audit findings closed (one this iter, one retroactively). `HeadlessView` now slices to the last `HEADLESS_RECENT_ACTIONS_TAIL = 200` entries by default with a "Show all N entries (earlier M hidden)" button that opts into the full list. Stable composed `<li>` keys (`sliceStart + idx`) so React reconciliation doesn't churn on slice changes. The "Diff body has no virtualization" audit finding is retroactively closed — the iter-12 Monaco diff tab switch already handles this natively (50k-line diffs scroll smoothly via Monaco's virtualization).

    *Iteration 35:* **Tab activation race fix.** One audit finding closed. `AppState.pendingTabActivate` reshapes from `boolean` → `number`. Each `onArmNextNewTab` (merge_tabs / extract_to_new_tab) and each spawn with a `newTab` intent increments the counter; each `tab_updated` for an unseen tab id decrements it and activates the new tab. Previously the flag was consumed by whichever new tab arrived first, leaving subsequent rapid-fire spawns landing on inactive tabs. The "track by tab id" approach the audit suggested would be stricter but more invasive — the counter is sufficient for the spawn-race case and avoids tracking session→tab correlation across the spawn pipeline.

    *Iteration 34:* **Orphan kill + pop-out empty-pane coherence.** Two audit findings closed. **Orphan stop now actually kills the underlying process** — `stop_session` branches on `had_live_handle = pty.is_some() || headless.is_some()`; when neither survived (orphan case), it loads the sidecar `meta.json` via the new `orphan::load_meta` helper and calls the new `orphan::kill_pid` to send SIGKILL/TerminateProcess to the recorded pid. Both helpers gate on `is_session_alive` first so a recycled pid can't get hit. Frontend orphan banner text updated to match: "Use Stop to kill the recorded PID and clean up, then spawn a new session." **Pop-out empty pane now coherent** — `EmptyPane` gains an `inPopout` prop threaded from `TabWindow` via `GridRenderer.inPopout`. When in a pop-out, the "Spawn one here" button (which silently no-op'd because TabWindow's `onSpawnInPane` was empty) is replaced by an explanatory hint: "Use the main window to spawn a session, then drag it here." Workspace clippy + 68 daemon tests green; pnpm typecheck clean.

    *Iteration 33:* **Modal z-index layers + focus return.** Two audit findings closed. New `useFocusReturn()` hook in `utils/a11y.ts` captures `document.activeElement` at modal mount and re-focuses it on unmount (deferred to next tick so React's DOM teardown doesn't clobber the restore); skips when the captured element has been removed from the DOM. Wired into every modal: SpawnDialog, PresetLaunchDialog, WorkspaceCreator, RepoRemoveDialog, ExitConfirmDialog, DiscardConfirmDialog, VscodeSuggestionToast. Modal stacking: new modifier classes `modal-backdrop-destructive` (z 120), `modal-backdrop-vscode` (z 130), `modal-backdrop-exit` (z 140) layered on top of the base `.modal-backdrop` (z 100). Ordering encodes intent: exit beats vscode beats destructive beats ordinary forms.

    *Iteration 32:* **Global keyboard shortcuts.** One audit finding closed. New `useKeyboardShortcuts` hook in `utils/a11y.ts` accepts a list of `{ key, ctrlOrMeta?, shift?, handler }` bindings and subscribes at document level; it skips when focus is in an editable surface (input / textarea / select / contenteditable / `.xterm` host) and when any modal is open (the App's `anyModalOpen` derivation gates the binding list). App.tsx wires: **Ctrl/Cmd+T** (new tab via `create_tab`), **Ctrl/Cmd+N** (spawn dialog, gated on `repos.length > 0`), **Ctrl/Cmd+Tab** + **Ctrl/Cmd+Shift+Tab** (cycle tabs forward/back, only when 2+ tabs), and **Ctrl/Cmd+1..9** (activate tab at index N-1; slot N is only registered if a tab exists at that index). Pop-out windows (`?tab=` / `?session=`) deliberately get no shortcuts to keep the OS-level chrome predictable. F2-to-rename is deferred; Ctrl+W deliberately not bound because it collides with Tauri/Windows window-close.

    *Iteration 31:* **WebSocket auto-reconnect.** One audit finding closed. The App-level connection effect was previously fire-and-forget — `connectDaemon(handshake)` ran once on mount and any subsequent close left the app frozen with no recovery short of a full restart. Refactor extracts the connect-and-subscribe block into a `connect()` closure and reschedules it via `setTimeout` whenever the connection state flips to `closed` (or `ensureDaemonStarted` itself throws). Exponential backoff: `500ms * 2^attempt` capped at 10s; resets to 0 on every successful `open`. `auth_failed` is terminal — no retry. Cleanup on App unmount cancels the pending timer. Each reconnect attempt is logged via `logToFile` so `app.log` records the recovery path. Sidebar connection badge (iter 21) already surfaces non-open states from every view, so the user sees the reconnect cycle live.

    *Iteration 30:* **Preset launch UX polish.** Three audit findings closed (two newly + one retroactively). Variables stage now validates required prompted variables (`!optional && trim().length === 0`) inline: empty required fields get a `.input-invalid` outline + "Required — fill in before continuing." hint, the Next button is disabled, and the button's tooltip lists the labels that still need filling. `VariableInput` gains an `invalid` prop wired into `className` + `aria-invalid` on the underlying input (file_path / folder_path / text variants). Preview stage gains an italicised caveat right below the prompt count: "Launching is one-shot — there's no in-app cancel once the daemon starts spawning." Audit's "Preset launch failure is `console.error` only" finding closed retroactively (iter 2 already wired the warning-severity toast).

    *Iteration 29:* **Post-split focus.** One audit finding closed. New `onArmFocusNewPane(tabId, knownPaneIds: Set<string>)` callback on `GridRenderer` is called by `PaneChrome.sendSplit` right before sending `split_pane`. The App stores the snapshot in `pendingPaneFocusRef`; the next `tab_updated` handler for the matching tab id walks the new grid, finds the pane id that wasn't in the snapshot, and sets `focusedPaneId` to it. Disarms after the first matching update so a stale arm doesn't yank focus later. Mirrored locally in `TabWindow` (pop-out) via a useEffect on the `tab` prop — the pop-out window owns its own focus state independently of the main App. `collectPanes` from `utils/grid.ts` is the diff helper.

    *Iteration 28:* **Tab safety: merge layout picker.** One audit finding closed. Tab context menu's `Merge N selected into new tab` entry now splits into two under a small uppercase label header: `Side by side (horizontal)` and `Stacked (vertical)`. Both flow to the daemon's `merge_tabs` message via `onMergeSelected(layout)`. The other "tab safety" items from the audit — close-confirm on tabs with bound sessions, close-others-confirm — were already resolved in iter 3 with the existing `tabHasBoundSessions` gate + `confirmingCloseOthers` arm, so this iteration is just the layout picker.

    *Iteration 27:* **Spawn dialog hardening: empty-repo state + env validation + no backdrop dismiss.** Four audit findings closed. New `EmptyRepoState` panel renders in place of the form when `repos.length === 0` — title "No repos registered", explanatory paragraph, Cancel + accent **+ Add repo** buttons; the + Add repo button closes the dialog and triggers the same `onAddRepo` callback the sidebar's Add repo button uses, so a fresh install user has a coherent flow from the (already-iter-4-disabled) toolbar button → empty state → directory picker. Env var rows gain inline validation against `/^[A-Za-z_][A-Za-z0-9_]*$/` plus cross-row duplicate detection; invalid keys get a `.input-invalid` outline + "Key must match …" hint, duplicates get "Duplicate key — only the last row wins". Submit gated on a shared `envRowsAreValid` helper in both SingleForm and WorkspaceForm. Backdrop click is no longer a close trigger on the spawn dialog — Escape, Cancel, and × still work, but a stray click outside silently destroying form state is gone. Audit's "Spawn failures surface only as console.error" finding closed retroactively (iter 2 + 22 already routed every daemon Error through the toast stack and disarmed pending spawn intent).

    *Iteration 26:* **Preset launch progress toast + auto-activate restraint.** Two audit findings closed. `pushToast` learns an optional `key` dedup field that lets a toast be replaced in place (same React id, bumped `generation`) instead of pushed as a new one — the auto-dismiss timer keys on `generation` so streaming progress updates restart the dismiss clock. New `sticky` flag on `ToastEntry` keeps the toast around regardless of the dismiss timer; flipping sticky off (e.g. on final `launched === total`) lets the dismiss fire normally. New `info` severity (accent-coloured) for non-error progress chrome. Preset-launch progress now emits a sticky `Launching preset 'X' — N / total sessions` info toast that updates in place; when launch completes the message flips to `Preset 'X' launched` and unsticks. Auto-activate now only fires for the FIRST tab created during a launch (kicks off context for the user) — subsequent ticks no longer hijack focus.

    *Iteration 25:* **Workspace name uniqueness + context-menu viewport clamping.** Two audit findings closed. Daemon's `upsert_workspace` now rejects empty/whitespace-only names AND case-insensitive trimmed-name collisions against OTHER workspaces (a rename-to-itself no-ops through fine). The daemon's `error` reply surfaces through the existing toast plumbing — no UI changes needed beyond the daemon-side guard. New shared `clampMenuCoord(raw, size, axis?)` helper in `utils/a11y.ts` clamps context-menu positions against the window edges with a 12px margin; applied to all three context menus (sidebar containers, tab pills, panes). Right-clicking near the bottom-right corner now keeps the menu on screen.

    *Iteration 24:* **Exit dialog button priority swap + orphan count + spawn dialog hint clarity.** Three audit findings closed. ExitConfirmDialog button order is now `Cancel · Stop sessions & quit · **Quit, leave running**` — "Quit, leave running" is the primary (accent-coloured, autofocused) since that's the recommended path matching the long-lived-daemon design goal; the destructive option keeps `danger` styling but is no longer the visual focal point. The dialog also accepts a new `orphanSessionCount` so orphan sessions get an explicit "(N orphans — daemon can't stop these; they'll stay running)" annotation alongside the active count. SpawnDialog's "Disabled because skip-permissions is on" hint expands into "Ignored while {skip-permissions|yolo} is on — claude will run without --permission-mode. The dropdown value is preserved for when you toggle it off." so the user knows both the effective behaviour and the retention semantics.

    *Iteration 23:* **Status colour differentiation + force-expand respect.** Two audit findings closed. `.status-spawning` is now a hollow accent ring (transparent fill, 1.5px border) while `.status-working` stays solid accent — `spawning` and `working` no longer collide visually. Sidebar's `forceExpand` set drops `attentionSessions` and keeps only the user-initiated `highlightedSessionIds`, so a backgrounded session firing attention no longer rudely re-expands the container the user just collapsed. To compensate for the lost visibility, container headers gain a `container-attention-chip` (`!` in a warn-coloured circle) when any of their sessions are attention-flagged — visible regardless of collapse state.

    *Iteration 22:* **Pop-out window titles + spawn-intent leak fix.** Two audit findings closed. `SessionWindow` and `TabWindow` now call `getCurrentWebviewWindow().setTitle(...)` in a `useEffect` keyed on the relevant label/name, so the OS-level window title updates from the raw UUID set by `open_session_window` / `open_tab_window` to the human-readable session label / tab name — and tracks renames live. Daemon `error` handler now disarms `pendingSpawnIntentRef` before pushing the toast, fixing the "next spawn inherits the rejected intent's routing" leak the audit flagged.

    *Iteration 21:* **Sidebar connection badge + TabWindow chrome + notification denial logging.** Three audit findings closed. Sidebar header now renders a compact connection badge that hides itself when the daemon connection is `open` and surfaces a warn/error chip with the reason as a tooltip for every other state — disconnection is now visible from any view, not just the EmptyState. `TabWindow` header gains a right-justified "Close window" button + flex layout that ellipsises long tab names. The notification-permission startup effect now logs to `app.log` when `requestPermission()` returns anything other than `"granted"` — at least there's a paper trail when attention notifications go silent. `Sidebar`'s previously-dead `connection` prop is finally consumed.

    *Iteration 20:* **Modal a11y sweep on remaining dialogs.** WorkspaceCreator, VscodeSuggestionToast, and ExitConfirmDialog gain `useEscape`, `role="dialog"`, `aria-modal="true"`, descriptive `aria-label`, and `aria-label` on close-`×` buttons. ExitConfirmDialog also gets `useAutoFocus` on its Cancel button (least-destructive default) and gates Escape on `!busy` so a mid-shutdown Escape doesn't fight the in-flight quit. With this, every component using `.modal-backdrop` is now keyboard-dismissable and properly announced to assistive tech. SpawnDialog + PresetLaunchDialog autofocus still parked — both are large forms where "first meaningful input" depends on form state; left for follow-up if it bites in practice.

    *Iteration 19:* **Pop-out window cleanup.** Three audit findings closed. Session pop-out now closes itself when its session disappears from the registry (parallel effect to the existing tab-pop-out close-on-tab-remove). `SessionPane` treats both `?session=` and `?tab=` query params as "inside a pop-out window", so the inner SessionPane no longer renders a redundant Pop-out button when reused inside `TabWindow`. The inner `SessionPane` Stop button (and its exit-code label) is hidden when the pane sits inside a session pop-out — `SessionWindow`'s chrome toolbar already exposes a Stop button, and having both was confusing because the chrome one was single-click while the inner one was two-step.

    *Iteration 18:* **Spawn dialog hardening: double-submit guard + preview-staleness fix.** Two audit items closed. Both `SingleForm.submit` and `WorkspaceForm.submit` now flip a `submittedRef` on first call so a fast double-click can't fire two `spawn_session` messages before React unmounts the dialog. The workspace form's preview-reset effect now depends on `useWorktree` too — previously toggling worktree mode after a Preview kept the stale table around, claiming paths that no longer matched the active mode. No protocol changes.

    *Iteration 17:* **Tab default-name + activate-on-merge/extract.** Three UX audit items closed. Daemon `make_tab` now picks the next free default name (`"Tab"`, `"Tab 2"`, `"Tab 3"`, ...) based on the current tab list — closed slots get reused so the numbering doesn't grow monotonically. Applies to plain `create_tab`, `merge_tabs`, and `extract_to_new_tab`. Frontend gains a new `onArmNextNewTab` App-level callback wired through `TabBar` (for `merge_tabs`) and `GridRenderer.PaneChrome` (for `extract_to_new_tab`) so both operations flip `pendingTabActivate` before sending — the resulting tab now auto-activates instead of the user landing on a stale source tab. Pop-out `TabWindow` passes a no-op for the callback since it has no tab bar of its own. New `next_default_name_picks_smallest_free` unit test in `crates/daemon/src/tabs.rs`.

    *Iteration 16:* **Attention-state cleanup + status-dot a11y.** Closes three audit findings on the sidebar's attention model. `session_updated` handler now clears the session id from `attentionSessions` when the status transitions back to `working`/`idle`/`spawning` — previously a self-resolving `awaiting_input` → `working` left the warning glyph stuck until the user clicked the leaf. `session_removed` handler now also drops the id from `attentionSessions` (previously the dead id leaked forever). Status dots in `Sidebar` and `SessionPane` gain `title="status: <state>"` + `aria-label="status <state>"` + `role="img"` so screen-reader users and hover-tooltip users have a way to read the colour-only state.

    *Iteration 15:* **`.git` filesystem watcher.** New `crates/daemon/src/git_watch.rs`: per-repo `notify-debouncer-full` (debounce 250ms) watches each registered repo's root recursively; on any burst the per-repo refresher task runs `repo_status` + `stash_list` and broadcasts `StateEvent::RepoStatus` + `StateEvent::Stashes` so every connected sidebar updates without re-requesting. The supervisor subscribes to `StateEvent::Repos` for lifecycle — adding a repo spawns its watcher, removing one drops the handle which (via the debouncer's drop → tx drop → rx returns None chain) terminates the refresher task cleanly. No protocol changes (existing `StateEvent::RepoStatus` / `Stashes` channels carry the broadcasts) and no frontend changes (the sidebar already listens for these events). Deps: `notify = "8"` + `notify-debouncer-full = "0.7"`. The `.git/objects/` filtering optimization mentioned in the plan was deferred — `git status` itself filters output cleanly even when the watcher fires on `objects/` churn, and the broadcast is idempotent. With this iter, Phase 6 finally fully ticks (its last open item was the watcher) and the plan moves to `docs/plans/completed/`. Workspace clippy clean, 83 cargo tests pass, `pnpm typecheck` clean.

    *Iteration 14:* **Source Control sidebar — Phase E: history pagination.** `PROTOCOL_VERSION` 9 → 10. `ClientMessage::ListCommits` gains an `offset: u32` (`#[serde(default)]`, mapped to `git log --skip=<offset>`); `DaemonMessage::Commits` echoes the requested `offset` so the client can distinguish a fresh listing (replace state) from a load-more append (push onto existing). `git_inspect::list_commits` threads the new arg. `HistoryView` in `SourceControlSidebar.tsx` keeps an accumulating `commits` list; the **load more** button at the bottom requests the next page with `offset = current.length` and is hidden once the daemon returns a partial batch (the exhaustion signal). Defensive sha-based de-dup guards against a daemon refresh overlapping a load-more. Renamed `COMMIT_LIMIT` → `COMMIT_PAGE_SIZE` to reflect the new role. Multi-file commit-diff-as-Monaco-tab was explicitly **out of scope** for Phase E (the original plan suggested it could come along but it's substantial enough for its own follow-up plan — keep the existing flat-text detail view for now). With this, the source-control-sidebar plan ticks all 5 phases (A–E) and migrates to `docs/plans/completed/`. Workspace clippy clean, 83 cargo tests pass, `pnpm typecheck` clean.

- [x] **Source Control sidebar** — see `docs/plans/completed/source-control-sidebar.md`. VSCode-style activity-bar-driven sidebar with Monaco diff editor, stage/unstage/commit/discard/stash, paginated history. Phases A (read-only sidebar + path-folded ChangesTree, iters 7–8), B (Monaco diff tabs as their own tab kind, iters 11–12), C (stage/unstage/commit, iter 9), D (discard + stash, iter 13), and E (history pagination, iter 14) are all **shipped**. Commit-as-multi-file-Monaco-diff was explicitly carved out of Phase E and is parked for a follow-up plan if the user wants it.

- [x] **`.git` filesystem watcher** — see `docs/plans/completed/git-fs-watcher.md`. Daemon spawns a per-repo `notify` watcher debounced at 250ms; on any burst the supervisor re-runs `git_inspect::repo_status` + `git_write::stash_list` and broadcasts via the existing `StateEvent::RepoStatus` / `StateEvent::Stashes` channels. Watcher lifecycle tracks repo add/remove (subscribed to `StateEvent::Repos`). No protocol or frontend changes — the sidebar already listens on `rt:repo_status` / `rt:stashes`. Iter 15.

- [ ] **Auto-update for the desktop app** — Phase 5's deferred `tauri-plugin-updater` work. Stays parked until a signed-release pipeline exists (no GH Actions release pipeline, no signing cert, no hosted manifest). In-app pieces are ~2 hours of work when the pipeline is ready.

## Verification

End-to-end checks at each phase:

**Phase 0**
- `cd X:\dev\yaat; .\claude-ws.ps1` then ask "list top-level files in both repos" — confirm Claude sees yaat *and* yaat-server
- Repeat for towercab-3d / towercab-3d-vnas
- Pass-through: `.\claude-ws.ps1 -p "echo hi"` returns headless output

**Phase 1**
- Start daemon → start app → connect lights green
- Add 2 repos, spawn a session in each; type `claude --version` in one and confirm scrollback in the other is unaffected
- Close app, confirm sessions still running (`tasklist | findstr claude`); reopen app, sessions reattach

**Phase 2**
- Add workspace `yaat-stack` = [yaat, yaat-server]
- Spawn workspace session on branch `feat/wrapper-test`; confirm `git worktree list` in both repos shows the new worktree
- In Claude session, ask "list all top-level files you can see" — expect files from both repos
- End session, confirm cleanup checkboxes correctly mark dirty/clean per repo

**Phase 3**
- Spawn headless session with prompt "summarize package.json"; confirm stream-json events render in event log; final result shows; cost/tokens populate from session jsonl

**Phase 4**
- Spawn session that hits a permission prompt; confirm OS notification fires within 2 seconds; clicking it focuses the session pane
- Stop a session externally (`taskkill`); confirm dashboard transitions to Stopped and notification fires

**Phase 5**
- Kill the daemon mid-session; confirm app reconnects and reattaches PTY without losing scrollback
- Pop out session, close pop-out, reopen — state preserved

## Risks and known unknowns

- **Status detection from PTY is heuristic.** Claude Code's TUI rendering can change between versions; the regex set for "awaiting input" detection is fragile. Mitigation: lean on stream-json (Phase 3) for headless sessions; for interactive, accept coarse status (idle/working/stopped) and rely on OS notification for the structured `attention` event when we can detect it.
- **Multi-dir scope of Claude Code.** `--add-dir` is the documented mechanism for multi-root sessions but its semantics around hooks, settings.json discovery, and CLAUDE.md merging across roots may need probing. **Verify with Context7 / docs early in Phase 2** before designing the workspace UI around assumptions.
- **Windows PTY.** ConPTY via `portable-pty` is the right pick, but mouse forwarding, resize signals, and CRLF handling need explicit testing.
- **Token/cost source-of-truth.** Parsing Claude Code's own session jsonl is more reliable than scraping stdout, but its format is undocumented and may change. Keep the parser tolerant; fall back to parsing the `claude` summary line on session end.
- **Daemon as a Windows service vs auto-start.** v1: launch daemon on first app start, leave it running. v2 maybe: install as a Windows service or scheduled task at logon. Defer until Phase 5.

## Out of scope (explicitly deferred)

- Sub-agent (Task tool) interception/isolation — chosen "top-level only"
- Auto-discovery / scanning roots — chosen "manual registry only"
- Per-session per-recipient notification config — chosen "uniform OS+tray model"
- Multi-machine / SSH attach — desktop app is local-only
- Cloud sync of registry/sessions
- Mobile companion app

## VSCode workspace integration

When a user adds a repo whose directory (or any sibling/parent within reasonable depth) contains a `.code-workspace` file, the daemon emits a `VscodeWorkspaceSuggestion` event. The Tauri app shows a one-shot "Create workspace 'foo' from foo.code-workspace?" prompt. On accept:

- Each `folders[].path` is resolved relative to the `.code-workspace` file's directory.
- If any resolved path is not yet a registered repo, register it automatically (using the leaf directory name).
- Create a rustling-tulip Workspace named after the `.code-workspace` filename stem with all folders as members.
- Optionally enable a file watcher: edits to the `.code-workspace` re-sync the Workspace member list (additions register new repos; removals are not auto-applied — surface as a prompt).

The `linked_vscode_workspace` field on a `WorkspaceEntry` records the source path so we don't re-prompt on every repo add and so re-syncs know what to read.

## Open questions to resolve before Phase 2

- [ ] Does `claude --add-dir` propagate hooks/settings from each root, or only the cwd? (Test empirically + check Context7 docs)
- [ ] When a workspace member is *itself* nested inside another (uncommon, but possible), how should worktree paths be laid out? Default proposal: sibling directory `<repo>.wt/<branch-slug>/`.
- [ ] Should workspace session spawn refuse if any member repo has uncommitted changes on the branch we're reusing? Default proposal: warn but allow.
