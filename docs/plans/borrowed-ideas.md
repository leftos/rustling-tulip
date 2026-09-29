# Ideas borrowed from Orca and VelaTerm

Features from two open-source agent managers that rustling-tulip lacks and could take over. Neither replaces rustling-tulip: neither has multi-repo workspace sessions, and VelaTerm's sessions die with its app. Both are MIT-licensed; ideas are free to take, and copied code keeps its MIT notice.

- **Orca**: an Electron agent development environment, `github.com/stablyai/orca`, docs at `onorca.dev/docs`. A daemon owns its PTYs, and there is one worktree per task.
- **VelaTerm**: a Tauri 2 terminal and agent manager, `github.com/vlinx-io/VelaTerm`. Sessions are kept in a project → group → session tree, and its PTYs live in the app process.

Items are in priority order. Each needs a design pass (the questions listed) before it is brief-sized.

- [ ] **Hook-reported agent status.** Take working / awaiting input / idle from Claude Code's own hooks instead of guessing from output, and keep the `pty_state.rs` heuristic as the fallback for agents without hooks. VelaTerm does this without touching the user's config: at spawn it passes an inline `--settings` JSON whose HTTP hooks post to its local server with the session id and a token (`src-tauri/src/agent/inject.rs`, `build_claude_settings` near line 116). Its mapping: `UserPromptSubmit` / `PreToolUse` / `PostToolUse` mean working, `Stop` means waiting, `Notification(permission_prompt|elicitation_dialog)` means asking, and `Notification(idle_prompt)` means idle. Codex has lifecycle hooks too (same file, near line 175). Orca reads the terminal's OSC title plus the agents' hooks.
  - Where the hooks post: a new authenticated HTTP route on the daemon (it already serves `/shutdown`), keyed by session id.
  - Whether an inline `--settings` merges with, or replaces, the user's own hooks and the repo's `.claude/settings.json`. Verify against the CLI before relying on it.
  - How the prompt injectors' mode checks use the new states.
- [ ] **Conversation view (GUI mode).** Show a Claude session as a chat instead of the terminal UI: the user's messages, the agent's answers and reasoning, one card per tool call, and permission requests and questions answered with buttons. Model, effort and permission mode can change without a restart, and messages typed mid-turn are queued. A session switches between terminal and conversation view, keeping its conversation: the agent restarts with `--resume`, and VelaTerm asks first if a turn is running. VelaTerm drives Claude as a two-way protocol peer with `claude --print --verbose --input-format stream-json --output-format stream-json` (`src-tauri/src/agent/chat/protocol.rs` near line 56, the engine in `chat/engine.rs`), and Codex through its app-server. The user guide is `docs/manuals/conversation-view_*.md`. rustling-tulip's headless mode (`crates/daemon/src/headless.rs`) already parses the stream-json output. What's missing is the two-way input, the permission prompts and the chat UI in the native client.
  - Billing: measured in [gui-mode-billing.md](../spikes/gui-mode-billing.md). It runs on the subscription's usage windows today (`apiKeySource: none`, the plan's 5-hour and 7-day windows in `rate_limit_event`). The CLI counts it as SDK-style use, which a paused Anthropic billing change would move to a separate credit. The terms allow it for personal use only, so keep the terminal UI as the fallback.
  - How permission requests reach the client: the stream-json permission-prompt tool, or a hook.
  - How the native client renders a chat: a new view kind next to the terminal and the diff tabs.
  - Ruling (user, 2026-09-28): a session remembers its last view (kept on the daemon's session); a new Claude session starts in the terminal view.
- [ ] **"Needs You" view.** One place listing every session waiting on the user, grouped as Needs you / Working / Done / Idle, where clicking a card focuses that session's pane. Orca ships this as an experimental kanban board (`onorca.dev/docs/model/agents-sessions`). It's more useful once hook-reported status exists.
  - Ruling (user, 2026-09-28): a view on the native client's activity rail, beside Sessions and Source control, not a sidebar filter.
- [ ] **Link a worktree to an issue or PR.** When spawning, paste or search a GitHub issue or PR URL; the session and its sidebar row show the link, and the PR's state once it exists. Orca does this for GitHub, GitLab, Linear and Jira (`onorca.dev/docs/model/worktrees`). It sits next to the presets' GitHub-issue-range prompts.
  - Ruling (user, 2026-09-28): GitHub only, through `gh`; no provider abstraction.
- [ ] **Agents that drive rustling-tulip.** A small CLI, run inside a session, that talks to the daemon over its WebSocket: spawn a child session (optionally in a new worktree) with a first prompt, send text to another session, and read another session's recent output or transcript. VelaTerm's `vspawn` / `vtell` / `vrefer` / `vsearch` (`src-tauri/src/agent/spawn_cli.rs`, `tell.rs`, `cli_client.rs`) and herdr's socket API do this, and each ships a skill that teaches the agent to use them.
  - How a session authenticates: a per-session token in its environment, never the daemon's full `auth_token`.
  - Which daemon messages the CLI may send.
  - Ruling (user, 2026-09-28): a spawned child sits in its own repo or workspace container like any session, and its leaf carries a `↳ <parent label>` tag; no nesting under the parent leaf.
- **Plan, then execute in parallel (dropped).** A planner session splits a task and the user approves the split; then one executor session per part runs in its own worktree, and work that falls short goes back to the same executor with its context. VelaTerm: `src-tauri/src/agent/plan_execute.rs` and `docs/manuals/planning-and-execution_*.md`. It builds on the item above and on workspace sessions.
  - Ruling (user): not built into rustling-tulip. Agents already spawn subagents in worktrees well, and an agent is the best party to review and merge their results, so an agent skill pack takes its place (MAIN.md, "Agent skill pack").
- [ ] **Resume agents after a reboot.** A reboot kills the tracers too. On the next start, offer to resume the sessions that were live at shutdown, not only through the Recover dialog. Orca records each live agent's conversation id at quit and injects `--resume <id>` into the restored pane on a cold start (stablyai/orca PR #5240). The session history and Recover already hold what's needed; this is the automatic prompt.
  - Ruling (user, 2026-09-28): prompt on start ("Resume all", "Choose…", "Dismiss"); a dismissed prompt leaves the sessions in Recover. Never resume without asking.
  - How to tell a reboot from a user Stop: the sessions whose tracers were lost with no end recorded.
- [x] **Share the workspace design on Orca issue #1099** (multi-repo workspaces). Posted with the user's approval: https://github.com/stablyai/orca/issues/1099#issuecomment-5878340045. It describes the anchor-free worktree layout the daemon builds (CLAUDE.md, "Where things live on disk"). The text as posted:

  > 🤖 Posted by Claude Code on behalf of @leftos.
  >
  > For what it's worth, here's how multi-repo workspaces work in rustling-tulip, a daemon-plus-client agent manager I've been building. A workspace is a named set of registered repos. Spawning a session into it on branch `X` creates (or reuses) one worktree per member, all under one folder for that branch and workspace: `<worktrees-root>/wt.<branch>/<workspace>/<member offset>`. A member's offset is its path below the folder the members share, so a relative reference between members in source (`../../libs/core`) still resolves between the worktrees. One agent process runs with its cwd in the first member's worktree and `--add-dir <worktree>` for every other member. Members on different drives share no folder, so the relative path between them can't be kept.
  >
  > For example, a workspace `shop` of two repos, spawned on branch `feature/login`:
  >
  > | Member | Source repo | Worktree |
  > |---|---|---|
  > | 1 | `D:\src\apps\web` | `<root>\wt.feature-login\shop\apps\web` |
  > | 2 | `D:\src\libs\core` | `<root>\wt.feature-login\shop\libs\core` |
  >
  > Both repos sit under `D:\src`, so each keeps its offset below it (`apps\web`, `libs\core`). `web` refers to `core` as `../../libs/core`, and that still resolves between the two worktrees. The agent starts with its cwd in `<root>\wt.feature-login\shop\apps\web` and `--add-dir <root>\wt.feature-login\shop\libs\core`. `<root>` is the worktrees root, `%LOCALAPPDATA%\leftos\rustling-tulip\data\worktrees` by default. Happy to go into more detail if it helps.
