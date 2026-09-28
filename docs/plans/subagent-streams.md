# View subagent streams as clickable sessions

Design for the `docs/plans/MAIN.md` item (Wave 9) "View subagent streams as clickable sessions". A Claude session's running subagents appear as a foldable set of rows under its sidebar leaf (agent type and description); a click opens a read-only view of that subagent's transcript in a pane; a row goes when its subagent finishes. Viewing is read-only: no interception, messaging or stopping (those stay out of scope, `docs/architecture.md` non-goals).

## Terms

- **Subagent**: a child agent a Claude session starts with the `Agent` tool (formerly `Task`). It runs inside the parent `claude` process; it has no PTY or process of its own.
- **Launcher**: the transcript that started a subagent: the session's main transcript for a first-layer subagent, or another subagent's transcript for a nested one.
- **Run**: one stretch of a subagent's work from start (or resume) to its final reply. A subagent resumed with `SendMessage` has several runs under one id.
- **Sidecar**: the `agent-<id>.meta.json` file Claude Code writes beside a subagent transcript.

## Findings

### Where Claude Code writes subagent transcripts

Documented: "find IDs in the transcript files at `~/.claude/projects/{project}/{sessionId}/subagents/`. Each transcript is stored as `agent-{agentId}.jsonl`" (https://code.claude.com/docs/en/sub-agents, "Subagent transcripts"). Transcripts persist within their session, survive main-conversation compaction, and are deleted after `cleanupPeriodDays` (30 by default). The `SubagentStart` and `SubagentStop` hooks receive `agent_id`, `agent_type` and `agent_transcript_path`; `SubagentStop` also gets `last_assistant_message` (https://code.claude.com/docs/en/hooks, "SubagentStart", "SubagentStop").

Observed on this machine (Claude Code 2.1.284, read-only survey of `C:\Users\lefto\.claude\projects`): 120 `subagents/` folders, 1,281 `agent-*.jsonl` files and 1,284 `*.meta.json` sidecars. Every transcript sits exactly at `<project>/<session-id>/subagents/`; there is no older top-level `agent-*.jsonl` layout left here. The layout:

```
<claude home>/projects/<encoded cwd>/
  <session-id>.jsonl                       main transcript (the daemon already knows <session-id>)
  <session-id>/subagents/
    agent-<agentId>.jsonl                  one subagent's transcript, appended live while it runs
    agent-<agentId>.meta.json              sidecar, written at spawn
    agent-<agentId>.forked-skill.json      only for a skill run with `context: fork`
  <session-id>/tool-results/               large tool outputs, not needed here
```

`agentId` is `a` plus 16 hex digits and matches the file name. Nested subagents are written flat into the same folder, not below their launcher.

**Sidecar fields** (tallied over all 1,284): always `agentType`, `spawnDepth` (1 = started by the main conversation), `requestShape` (`background` or `foreground`), `requestNonInteractive`; usually `description` and `toolUseId` (the launcher's `Agent` tool_use id); `parentAgentId` on nested ones; `name` on forked skills (which have no `toolUseId`); occasionally `model`, `isFork`, `spawnedWithWorktree` / `worktreePath` / `worktreeBranch`, `stoppedByUser`, `worktreeCleanlyRemoved`. The five `foreground` depth-1 sidecars carry neither `description` nor `toolUseId`.

**Transcript lines** carry `type` (`user`, `assistant`, `attachment`), `isSidechain: true`, `agentId`, `sessionId` (the parent's), `uuid` / `parentUuid`, `cwd`, `timestamp`, `version`. `assistant` lines hold one content block each (`text`, `thinking` or `tool_use`) under `message.content`, with `message.stop_reason` and `message.usage`; `user` lines hold the first prompt (a string) and then `tool_result` blocks. That is the same `{type, message: {content}}` shape the daemon's stream-json parser already reads (`crates/daemon/src/agents/claude.rs:191-234`). A worktree-isolated subagent's lines carry the worktree as `cwd`, but its transcript stays under the parent session's folder.

### How the parent records spawn and completion

- **Spawn**: an `assistant` line with a `tool_use` block, `name: "Agent"`, `id` = the sidecar's `toolUseId`, `input` keys `description`, `prompt`, `subagent_type`, `run_in_background` (optional `name`, `model`, `isolation`).
- **Launch reply**: a `user` line with the matching `tool_result` and a `toolUseResult` object. For a background subagent: `status: "async_launched"`, `isAsync: true`, `agentId`, `description`, `outputFile`, `resolvedModel`. All 1,203 `Agent` replies found here are `async_launched`: in an interactive session fork mode is on by default and every subagent runs in the background (https://code.claude.com/docs/en/sub-agents, "Run subagents in foreground or background"). A foreground subagent's result is the `tool_result` itself, so its completion is the reply.
- **Completion of a background run**: a `queue-operation` line (`operation: "enqueue"`) and then a `user` line with `origin.kind: "task-notification"`, both with a `<task-notification>` text holding `<task-id>` (the agentId), `<tool-use-id>`, `<output-file>`, `<status>`, `<summary>`, `<result>`, `<usage>`. Status values seen (these notifications also cover background shell commands): `completed`, `failed`, `killed`, `stopped`. The notification is written to the launcher, so a nested subagent's completion lands in its launcher's `agent-*.jsonl`. This text format is not documented.
- **Resume**: `SendMessage` to a finished subagent starts a new run under the same id ("Resuming starts a new run of the agent under the same ID", sub-agents page); the transcript grows again and a second notification follows. One sample here has three runs, three `end_turn` replies and three notifications.

### How a running subagent is told from a finished one

A run ends when the model replies without a tool call. In the transcript that is an `assistant` line with `stop_reason: "end_turn"` and a `text` block, followed only by `attachment` lines (hook results such as `SubagentStop` when the user has that hook). Over 400 sampled transcripts, the last `assistant` line was `end_turn` without `tool_use` in 396; the 4 others ended on a `tool_use` (stopped or still running). A killed or failed run never writes that final reply, so only the launcher's notification (`killed`, `failed`, `stopped`), the sidecar's `stoppedByUser`, or the parent process ending marks it. Timestamps agree: in the samples the final reply precedes the launcher's `enqueue` by one to two seconds. The `outputFile` under `%TEMP%\claude\...\tasks\` is empty on this machine and is not used.

### What the daemon has today

- **Conversation id**: every interactive Claude spawn gets `--session-id <uuid>`, or `--resume <id>` which keeps the id (`crates/daemon/src/server.rs:3617-3623`, `crates/daemon/src/agents/claude.rs:60-72`). It is on `SessionRecord.claude_session_id` (`crates/daemon/src/session.rs:170-173`) and `SessionSnapshot.claude_session_id` (`crates/protocol/src/lib.rs:519-523`); `None` for shells, headless runs and other agents. With `members[0].worktree_path` as cwd, the main transcript path is fully known.
- **Transcript paths**: `crates/daemon/src/transcripts.rs` resolves the Claude home (honouring `CLAUDE_CONFIG_DIR`, `claude_home`), encodes the project folder (`encode_project_dir`) and builds `project_dir` (private today). It scans heads for titles for the recover dialog; it does not tail.
- **No live transcript tailer**: `rg` over `crates/daemon/src` finds no code tailing `<session-id>.jsonl` for tokens or cost, although `CLAUDE.md` says it is "tailed independently". Metrics come only from the headless stream-json parser (`agents/claude.rs:117-167`), which already turns `assistant` blocks into `tool: <name>` / `assistant: <snippet>` entries. This feature brings the first tailer.
- **Watcher pattern**: `crates/daemon/src/git_watch.rs` parks its refresher while `Hub.client_count` is 0 and catches up on reconnect; the subagent watcher follows it.
- **Native headless view**: `apps/native/src/headless_view.rs` draws a stats bar over a numbered, capped log with a show-all button (`HeadlessBody`, lines 28-70), fed from `SessionSnapshot.recent_actions`. A transcript view can reuse its layout with its own entries.
- **Panes and tabs**: `GridNode::Pane { pane_id, session_id }` (`crates/protocol/src/lib.rs:1124-1132`); `ReplacePaneSession` / `SplitPane` bind a session to a pane (lines 2446-2486). `TabContent` (lines 1218-1244) has no `#[serde(other)] Unknown`, so a new tab kind would break protocol 22's tab lists; a new pane field with `#[serde(default)]` does not. `protocol-version.json` is 23 with `[23, 22]` supported.
- **Sidebar**: `Leaf` (`apps/native/src/sidebar.rs:77`) and `leaf_row` / `LeafRows` (`apps/native/src/sidebar_view.rs:331`, `:526`) draw session rows; collapsed state persists in `native-ui.json`.

## Design

### Daemon: discovery and state

A new `crates/daemon/src/subagents.rs` owns everything that reads subagent files. For each live Claude session with a `claude_session_id`, one watcher task looks at `<project dir>/<session-id>/subagents/`, the folder's absence meaning "none yet". Every second while a client is connected (parked at `client_count` 0, one catch-up scan on reconnect, as `git_watch` does) it lists the folder and, for each `agent-<id>.jsonl` whose length changed, reads only the new bytes from the last offset. It also tails the launcher transcripts for `<task-notification>` lines, filtering by substring before any JSON parse, since the main transcript runs to megabytes.

Per subagent it keeps: `agent_id`, `parent_agent_id`, `agent_type`, `description` (from the sidecar, else the launcher's `Agent` input, else the first prompt's first line), `started_at`, the transcript offset, and a state from these rules, applied in order:

1. The parent session is no longer running (`Stopped` / `Exited` / removed): **finished**.
2. The transcript was last written before the current `claude` child started (a daemon restart or `--resume` found it from an earlier process, and a background subagent dies with its process): **finished**.
3. The sidecar has `stoppedByUser`, or the launcher's newest notification for the id has a terminal status and is newer than the transcript's last line: **finished**.
4. The last `assistant` line is `end_turn` with no `tool_use` and only `attachment` lines follow: **finished**.
5. Otherwise **running**. New lines after a finished state (a resume) make it running again.

Rules 1-2 never need the undocumented formats; rules 3-4 carry the version risk, and their parsers get pinned tests built from the field names above. The session's snapshot carries only running subagents; a subagent whose row is gone keeps its record while a pane still watches it.

### Protocol (additive; protocol 22 keeps decoding)

- `SessionSnapshot.subagents: Vec<SubagentInfo>` with `#[serde(default)]`: `SubagentInfo { agent_id, parent_agent_id: Option<String>, agent_type, description: Option<String>, started_at }`. It rides the existing `Sessions` / `SessionUpdated` broadcasts and the lag resync; a start or finish emits one `SessionUpdated`. The list is session state rather than a session-shaped message, so the "`SessionSnapshot` is canonical" rule holds. A v22 client ignores the unknown field.
- `ClientMessage::WatchSubagent { session_id, agent_id, request_id: Option<String> }` and `UnwatchSubagent { session_id, agent_id }`.
- `DaemonMessage::SubagentTranscript { session_id, agent_id, entries, hidden: u32, finished: bool, request_id }` (the reply: the newest entries up to a cap, with `hidden` counting the older ones) then `SubagentTranscriptAppend { session_id, agent_id, entries, finished }` to watchers only. Both are unknown top-level types to a v22 client, which `InboundDaemonMessage::Unknown` absorbs.
- `TranscriptEntry` (tag `kind`, `#[serde(other)] Unknown` from day one): `Prompt { text }`, `Text { text }`, `ToolUse { name, summary }` (one line from the input: a path, a command, a pattern), `ToolResult { is_error, first_line }`, `Nested { agent_id, agent_type, description }` for an `Agent` call. `thinking` blocks are skipped. Text is capped per entry so one reply can't carry megabytes.
- Pane binding: `GridNode::Pane` gains `#[serde(default)] subagent_id: Option<String>`, and `SplitPane` / `ReplacePaneSession` gain the same optional field. A subagent pane keeps `session_id` = the parent, so the daemon's existing cleanup (the binding goes when the session is removed) covers it, and the installed Tauri app shows the parent's terminal in that pane. No `TabContent` variant is added.

`cargo test -p protocol v22_compat` gains a case: a v22 snapshot and a v22 pane decode with `subagents` empty and `subagent_id` `None`.

### Native client

- **Rows**: a leaf whose snapshot has `subagents` shows a fold toggle and a count; unfolded, one indented row per subagent reads `<agent type> · <description>`, nested ones indented under their launcher. Rows disappear with the next snapshot that drops them. The fold state is per session in `native-ui.json` beside the collapsed containers; the default is folded. Rows take no attention highlight.
- **Click**: opens the transcript in a pane with the same placement the leaf click uses for a session (focus the pane already showing it, else bind the focused empty pane, else split beside the parent's pane), sending `SplitPane` / `ReplacePaneSession` with `subagent_id`. The pane sends `WatchSubagent` when it appears and `UnwatchSubagent` when it closes or rebinds.
- **View**: a new `apps/native/src/subagent_view.rs` reuses `headless_view`'s structure: a header bar (agent type, description, `running` / `finished`, entry count) over the numbered, capped log with its show-all button, one row per `TranscriptEntry`. It takes no keyboard input beyond copy and scrolling; the pane focus rules match the headless body. When the subagent finishes, the view stays, marked finished, until the pane is closed (see open question 2).

### Limits

- **Nested subagents**: listed under their launcher by `parentAgentId`; Claude Code allows three layers by default (`CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH`, sub-agents page).
- **Background vs foreground**: interactive sessions run every subagent in the background, so the notification path is the common one. Foreground runs (a `-p` session, `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1`) complete through the reply `tool_result` and rule 4.
- **Codex and Cursor**: no subagents; nothing is watched for them. Plain shells and headless runs have no `claude_session_id`, so headless Claude runs are out until the daemon records the id from the stream-json `system` init event.
- **Resumed sessions**: `--resume` keeps the id, so the folder is the same; subagents from the earlier process are finished by rule 2.
- **`/clear` and forked conversations**: they start a new conversation id the daemon doesn't know, so later subagents are missed. The hook-reported status design (`docs/plans/hook-status.md`, HS.7) updates `claude_session_id` on `SessionStart` with `clear` / `resume` / `fork`; the watcher follows the record's id, so that step closes this.
- **Format drift**: sidecar fields and the notification text are undocumented and may change between Claude Code versions; the watcher degrades to rules 1, 2 and 4.

## Open questions

Answered (user): Q3 (a) full reply text plus one line per tool call and result; Q2 (a) an open view stays, marked finished, until closed; Q4 (a) nested rows indented under their launcher; Q5 (a) no badge on a collapsed container. The pane field that shows a subagent is shared with the dispatch-run pane ([dispatch-follow.md](./dispatch-follow.md) Q1). Settled (orchestrator, technical): Q1 (c) files always, the `SubagentStart` / `SubagentStop` hooks preferred once hook-status lands.

1. **Source of truth for start and stop.** (a) Files only, as designed (recommended): works with no Claude settings change and for sessions spawned before the feature; worst case: a Claude Code update renames sidecar fields and descriptions fall back to the first prompt line. (b) `SubagentStart` / `SubagentStop` hooks through the hook-reported status item's inline `--settings`, files for the transcript body only: documented fields; worst case: this feature waits on that item and misses sessions started without the hook. (c) Both, hooks preferred when present: the most exact; worst case: two code paths to test.
2. **What a finished subagent's open view does.** (a) Stays, marked finished, until closed (recommended); worst case: stale panes pile up in a long session. (b) Closes with its row; worst case: the final reply vanishes while the user reads it. (c) Becomes an empty placeholder; worst case: same loss as (b) with an extra click.
3. **Transcript fidelity.** (a) Full assistant text, one line per tool call and result (recommended); worst case: long replies make a long log. (b) Headless-style one-line snippets only; worst case: the user can't read what the subagent concluded. (c) Full tool inputs and outputs too; worst case: megabyte transcripts over the wire and a slow view.
4. **Nested subagent rows.** (a) Indented under their launcher (recommended); worst case: a reviewer with many verifiers makes a deep list. (b) Flat, one level; worst case: the tree is lost. (c) Hidden, reachable from the launcher's `Nested` entries only; worst case: a busy nested agent is invisible from the sidebar.
5. **Rows while the parent session's leaf is hidden** (its container collapsed). (a) Nothing extra (recommended); worst case: running subagents go unnoticed. (b) A count badge on the container; worst case: badge noise.

## Checklist

- [ ] **SS.1 Subagent folder reader.** New `crates/daemon/src/subagents.rs` (pure, no IO beyond reading a given folder): parse sidecars and transcript tails from an offset into `SubagentRecord` with rules 3-5; make `transcripts::project_dir` `pub(crate)`. Proving tests in the module on temp folders: a sidecar alone is running; an `end_turn` text reply plus trailing attachments is finished; growth after that is running again; `stoppedByUser` is finished; `parentAgentId` is kept; a transcript with no sidecar takes its description from the first prompt; garbage lines are skipped.
- [ ] **SS.2 Launcher notifications.** In `subagents.rs`, parse `<task-notification>` text from `queue-operation` and `user` lines into (agent id, status, timestamp), substring-filtered before JSON parse; rule 3 uses it. Tests: each status value, a shell-task notification (non-agent id) ignored, a notification older than the transcript's last line ignored, a nested launcher's transcript as the source.
- [ ] **SS.3 Protocol types.** `crates/protocol/src/lib.rs`: `SubagentInfo`, `SessionSnapshot.subagents`, `WatchSubagent` / `UnwatchSubagent`, `SubagentTranscript` / `SubagentTranscriptAppend`, `TranscriptEntry` with `Unknown`, `subagent_id` on `GridNode::Pane`, `SplitPane`, `ReplacePaneSession`. Tests: round-trips; an unknown `TranscriptEntry` kind decodes to `Unknown`; the new `v22_compat` cases decode.
- [ ] **SS.4 Watcher task.** `crates/daemon/src/subagents.rs` + `server.rs`: one task per live Claude session, one-second scan, parked at `client_count` 0, rules 1-2 from the session record, `SessionUpdated` on each start or finish, stopped when the session is removed. Test in `server.rs` tests with `CLAUDE_CONFIG_DIR` pointed at a temp home: writing a sidecar and transcript yields a snapshot with the subagent; writing its `end_turn` reply drops it; stopping the session drops all.
- [ ] **SS.5 Transcript watch.** `server.rs` handlers for `WatchSubagent` / `UnwatchSubagent`; entry building (`TranscriptEntry` from `assistant` / `user` lines, capped text, `Nested` for `Agent` calls) in `subagents.rs`. Tests: the reply echoes `request_id` and caps with `hidden`; appends reach only watchers; an unknown agent id answers `Error` with the `request_id`.
- [ ] **SS.6 Pane binding.** `crates/daemon/src/server.rs` (tab/grid code): store and clear `subagent_id` on `SplitPane` / `ReplacePaneSession`; a Tauri-style `ReplacePaneSession` without the field clears it. Tests: bind, rebind, session removal clears both fields.
- [ ] **SS.7 Sidebar rows.** `apps/native/src/sidebar.rs` (`Leaf` gains subagent rows and a fold flag, persisted in `native-ui.json`), `apps/native/src/sidebar_view.rs` (fold toggle, indented rows). Test in `apps/native/tests/ui_sidebar.rs`: a scripted snapshot with two subagents shows them unfolded after a toggle click; the next snapshot without one drops its row; the fold survives a reload of the UI state.
- [ ] **SS.8 Transcript view.** New `apps/native/src/subagent_view.rs` (reusing `headless_view` layout pieces), `apps/native/src/net.rs` (the two new messages), pane rendering in `grid_view.rs`. Test in a new `apps/native/tests/ui_subagents.rs`: clicking a row sends `SplitPane` with `subagent_id` and `WatchSubagent`; the fake daemon's reply renders its rows; an append adds a row; `finished: true` shows the finished mark; closing the pane sends `UnwatchSubagent`.
- [ ] **SS.9 Live e2e.** `tools/e2e/fake-claude/index.mjs` gains a mode that writes a sidecar and a transcript under `$CLAUDE_CONFIG_DIR/projects/<encoded cwd>/<session-id>/subagents/` and finishes it after a delay; `apps/native/tests/e2e_live.rs` spec: the row appears, the view streams, the row goes. Run with `.\rt.ps1 native-e2e`.
- [ ] **SS.10 Docs.** `CLAUDE.md`: the subagent watcher under the architecture invariants and the correction that the daemon has no interactive tokens tailer; `docs/plans/MAIN.md`: delete the item's line; promote the design's lasting decisions into `docs/architecture.md` and delete this doc.
