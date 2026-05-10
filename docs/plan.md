# Plan: Multi-Repo Claude Code Wrapper ("rustling-tulip")

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

State persisted to `%APPDATA%\rustling-tulip\state.json` (registry + sessions). Transcripts/PTY scrollback persisted to per-session log files in `%APPDATA%\rustling-tulip\sessions\<id>\`.

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

- [ ] `X:\dev\yaat\claude-ws.ps1` — runs `claude` with `--add-dir X:\dev\yaat-server --dangerously-skip-permissions`
- [ ] `X:\dev\towercab-3d\claude-ws.ps1` — runs `claude` with `--add-dir X:\dev\towercab-3d-vnas --dangerously-skip-permissions`

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
- [ ] Workspace skeleton, `cargo deny` config, `clippy` pedantic lints, `oxlint`/`oxfmt` for TS
- [ ] `protocol` crate: message types + version field
- [ ] `daemon`: WS server, auth handshake, session supervisor, portable-pty
- [ ] State persistence to `%APPDATA%\rustling-tulip\state.json`
- [ ] Tauri shell: connect to daemon, list/spawn/attach single-repo sessions
- [ ] xterm.js wired to PTY stream
- [ ] Repo registry: add/remove repos, list branches
- [ ] Manual smoke test: spawn 2 sessions in 2 repos, attach to each, type into them

### Phase 2 — Workspaces (the differentiator)
- [ ] Workspace registry: define + list workspaces
- [ ] Workspace spawn flow: branch input + per-member preview
- [ ] Reuse-or-create branch policy with preview before commit
- [ ] Multi-dir `claude` invocation (`--add-dir` per extra member)
- [ ] Aggregate diff preview across member worktrees
- [ ] Cleanup dialog with per-worktree checkboxes (default checked = clean, unchecked = dirty)
- [ ] **VSCode `.code-workspace` auto-detect**: when adding a repo, scan for `*.code-workspace` files; if found, parse the `folders` array and suggest creating a matching rustling-tulip Workspace. Resolve relative paths against the workspace file's location. Optional file-watcher keeps members in sync when the user edits the `.code-workspace`.
- [ ] Test on real workspaces: yaat/yaat-server, towercab-3d/towercab-3d-vnas (both have `.code-workspace` files)

### Phase 3 — Headless + structured state
- [ ] `claude --print --output-format stream-json` adapter
- [ ] Stream-json parser → structured events → session state
- [ ] Headless session UI (no terminal, just structured event log + result panel)
- [ ] Tail `~/.claude/projects/<...>/*.jsonl` for cost/token regardless of mode
- [ ] Activity ring buffer feeding "last action" sidebar entries

### Phase 4 — Attention model
- [ ] Awaiting-input detector for PTY mode (regex against known TUI prompts)
- [ ] OS notifications via `tauri-plugin-notification`
- [ ] System tray badge with attention count
- [ ] Dashboard row highlight + sound (configurable)
- [ ] Stopped/error transitions also fire notifications

### Phase 5 — Polish
- [ ] Per-session config: model override, permission mode, env vars
- [ ] Session log viewer (full transcript, scrollback persisted across restarts)
- [ ] Pop-out session into its own Tauri window
- [ ] Auto-update for the desktop app (`tauri-plugin-updater`)
- [ ] Crash-recovery: daemon detects orphaned `claude` processes on startup and reattaches state
- [ ] **Persistent resizable panes**: every divider in the app (sidebar/main, terminal/git split, changes-list/diff split, headless-stats/log split) is drag-resizable with widths persisted to settings. Reuse a single `ResizableSplit` component so the behavior is consistent across panes.

### Phase 6 — Git tracking layer

A multi-repo git inspection surface on top of session worktrees. Same data is visible whether the session is single-repo or workspace-spanning; in workspace sessions every view groups by member repo.

- [ ] Changed-files panel with two presentation modes:
  - **Flat view**: one row per changed file with `repo_name / path` + status (M/A/D/R/?), sortable by path or repo.
  - **Tree view**: collapsible directory tree per member repo, status icons on each node (file + directory rollup).
- [ ] Inline file diff viewer: click a file → side-by-side or unified diff (`git diff` / `git diff --staged`).
- [ ] Commit history panel per member repo: `git log --oneline -n <N>` with author / date / message / sha. Click a commit to see its files + per-file diff.
- [ ] **Open in GitHub**: parse `git remote get-url origin`, support `https://github.com/...` and `git@github.com:...` forms, also GitLab and Bitbucket variants. Build URLs for:
  - File at HEAD on the current branch
  - File at a specific commit (with optional line range)
  - Commit page
  - Branch compare against base
- [ ] Stage/unstage operations from the changed-files panel (checkbox toggle = `git add` / `git restore --staged`).
- [ ] Watch `.git` for changes to keep the views live without manual refresh.
- [ ] Honor `.gitignore` and large-binary policy: don't try to render diffs >1 MB unless the user clicks "load anyway".

Daemon-side: extend the existing protocol with `ListCommits`, `GetCommit`, `GetFileDiff`, `StageFiles`, `RemoteUrl` (and corresponding `Daemon` responses). UI-side: a tabbed git panel in the session view, plus a top-level "Repo" tab for repos that aren't currently in any session.

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
