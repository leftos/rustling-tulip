# Subagent streams (RT-30, RT-406)

The design for RT-30 ("View a session's running subagents as read-only sessions") and RT-406 ("Show a session's subagents under it in the sidebar"). A Claude session's subagents show as rows nested under its sidebar leaf, exactly as its dispatch runs do.

A click on one opens a pane that follows the subagent's real transcript read-only, through the same transcript messages and view a dispatch run's pane uses ([transcript-view.md](./transcript-view.md)). Viewing is read-only: no interception, messaging or stopping, which stay non-goals (`docs/architecture.md` "Non-goals").

This design replaces step TV.6 ("Subagent source") of [transcript-view.md](./transcript-view.md): the subagent source, its pane follow and its tail are the steps below.

## Owner rulings

Recorded on RT-30, RT-1580 and RT-406; quoted as given.

1. "Every subagent session and every dispatch can be followed as readonly inside the app, and they should all be nested and indented under their host session, not flat under the repo as if they're full sessions."
2. A followed run shows its actual transcript, as watching a Claude Code session in the terminal does: the assistant's text, each tool call with its input, the tool's output; not a list of `system:` events.
3. "A session that has subagents or dispatch runs nested under it in the sidebar can be collapsed and expanded, as a repo group collapses its sessions."
4. "In the sidebar's Tabs view, subagents and dispatch runs never show under Unbound. What decides is whether their host (main) session is bound to a tab: they show, nested under their host, when the host is bound, and are hidden from the Tabs view when it is not."

Earlier owner answers that still hold: an open subagent view stays, marked finished, until the user closes it; nested subagents are indented under their launcher; a collapsed container shows no extra badge for its subagents. The earlier fidelity answer (full reply text plus one line per tool call) is superseded by ruling 2.

## Terms

- **Subagent**: a child agent a Claude session starts with the `Agent` tool. It runs inside the session's `claude` process and has no PTY or process of its own.
- **Host session**: the RT session whose conversation started the subagent, directly or through other subagents. Every subagent row nests under its host session's row.
- **Launcher**: the transcript that started a subagent: the host's main transcript for a first-layer subagent, another subagent's for a nested one.
- **Subagent sidecar**: the `agent-<id>.meta.json` file Claude Code writes beside a subagent transcript.
- **Typed prompt**: a main-thread `user` record that is a prompt the user sent, not a tool result and not a task notification that woke the agent.

## What Claude Code writes

Documented at https://code.claude.com/docs/en/sub-agents ("Subagent transcripts"): each subagent transcript is `agent-{agentId}.jsonl` under `~/.claude/projects/{project}/{sessionId}/subagents/`, kept within the session and deleted after `cleanupPeriodDays`.

```
<claude home>/projects/<encoded cwd>/
  <conversation id>.jsonl                  main transcript
  <conversation id>/subagents/
    agent-<agentId>.jsonl                  one subagent's transcript, appended live while it runs
    agent-<agentId>.meta.json              sidecar, written at spawn and rewritten as it runs
```

Nested subagents are written flat into the same folder; the sidecar's `parentAgentId` names the launcher. `agentId` is `a` followed by hex digits and matches the file name.

- **Sidecar fields** (undocumented): `agentType`, `description`, `toolUseId` (the launcher's `Agent` tool_use id), `spawnDepth` (1 = started by the main conversation), `parentAgentId` on nested ones, `stoppedByUser`. Recent sidecars (Claude Code 2.1.322 on this machine) also carry `finished: true` once the subagent has finished, which the reader does not use yet (step SS.3).
- **Transcript lines** carry `type` (`user`, `assistant`, `attachment`, and newer request-bookkeeping types), `isSidechain: true`, `agentId`, `uuid`, `timestamp` and the same `message.content` blocks as a main transcript, so the chat parser reads them unchanged (step SS.2 proves it with a fixture).
- **Completion**: a run ends on an `assistant` reply with `stop_reason: "end_turn"` and no tool call; a background run's launcher also gets a `<task-notification>` record (`completed`, `failed`, `killed`, `stopped`).

  That notification then wakes the main agent with a new turn (a `UserPromptSubmit` whose prompt is the notification), so "the host's next turn" follows a subagent's finish at once and cannot be the rule that removes its row.
- **Resume**: `SendMessage` to a finished subagent starts a new run under the same id, and its transcript grows again.

## What is built

Each is a symbol to read before building on it.

- **Discovery** (the Claude plugin, the Dashboard's DB.4): `SubagentReader` in `crates/agents/claude/src/subagents.rs` lists the folder, follows each transcript with an incremental `Transcript`, reads sidecars, joins each subagent to its launch and builds a tree of `agent_api::SubagentNode`.

  A node carries `agent_id`, `parent_agent_id`, `agent_type`, `description`, `model`, `effort`, `started_at`, `finished_at`, `state`, `end_status` and `children`. The tree is reached only through `ActivityReader::read`, whose `ActivitySnapshot.subagents` carries it, built by the plugin's `activity_reader(&ActivitySource)`.
- **The Dashboard's use of it**: `crates/daemon/src/dashboard/mod.rs` reads every listed session's `ActivityReader` every `TICK` while a Dashboard is watched, and drops finished nodes from before the current turn (`TreeContext::before_turn`). The sidebar cannot reuse it: it reads nothing while no Dashboard is watched.
- **Transcript wire** (TV.1): `ClientMessage::FollowTranscript { source, request_id }`, `UnfollowTranscript { source }`, `DaemonMessage::Transcript { source, entries, hidden, request_id }`, `TranscriptAppend { source, entries, ended }`, and `TranscriptSource::{DispatchRun { run_id }, Unknown}` in `crates/protocol/src/lib.rs`.
- **Transcript log** (TV.3, commit ae5c850c0 on `rt-1581-tv3-daemon`): `TranscriptLog` in `crates/daemon/src/dispatch_runs/transcript.rs`.

  `apply` upserts by id and caps at `CHAT_REPLY_CAP`; `fold` parses lines with `plugin.chat_entries(line, known)`; `follow` sends the backfill and registers under the caller's lock; `restarted` and `end` tell the followers. `DispatchRuns::follow_transcript` refuses any source but `DispatchRun` with `UNSUPPORTED_MESSAGE`, and `server.rs` routes both follow messages to `server/dispatch_runs.rs`.
- **File tail**: `transcript_tail::spawn(path, start_offset, client_count, on_event)` in `crates/daemon/src/transcript_tail.rs` (`TailEvent::{Line, Restarted, CaughtUp}`), which waits for a file that does not exist yet and parks at `client_count` 0. The chat mirror uses it.
- **Pane follow** (DF.8): `GridNode::Pane.follow: Option<PaneFollow>`, `PaneFollow::{DispatchRun { run_id }, Unknown}`; `CreateTab`, `SplitPane` and `ReplacePaneSession` carry it; `tabs::prune_follows_not_in` clears dead run follows at daemon start.
- **Sidebar nesting** (RT-1580), in `apps/native/src/sidebar.rs` and `sidebar_view.rs`, described in `docs/native-client.md` "Sessions under repos":
  - the model: `Leaf.depth`, `Leaf.host_session`, `Leaf.nested`, `Leaf.collapsed`, `LeafKind::{Session, Dispatch}`, `nest_runs` and `nest_after` (repos view), `nest_tab_runs` and `linked_on` (tabs view), `session_fold_key`, `SidebarModel::toggle_session_fold`;
  - the drawing: `shown_leaves`, `NEST_INDENT` (14 px), `session_fold` (the chevron, tooltip "Hide its dispatch runs" / "Show its dispatch runs") and `RootView::toggle_session_fold`.
- **Session data**: `SessionSnapshot.conversation_id`; the record's folder from `history::primary_cwd` and its rows from `conversations::lookup_rows(&extra_env)`, as `chat_mirror.rs` resolves a transcript path.

## Decisions

Every decision here is an orchestrator decision (not an owner ruling), settled from the code above, unless it quotes a ruling.

### Identity and wire

- **What names a subagent.** The host session's RT session id plus the agent id: `PaneFollow::Subagent { session_id, agent_id }` and `TranscriptSource::Subagent { session_id, agent_id }`.

  Both survive a daemon restart: session ids survive through tracer reattach, and agent ids are written in the files. The conversation id is not part of it: the daemon resolves the folder, conversation and rows from the session's record when a follow arrives, so a client never hands the daemon a path or a conversation.
- **The same transcript messages.** A subagent follow is `FollowTranscript` with the `Subagent` source, answered by `Transcript` and then `TranscriptAppend`, carrying the chat mirror's `ChatEntry` from the plugin's `chat_entries`. No new message, no new entry type.
- **The subagent list rides `SessionSnapshot`.** `SessionSnapshot.subagents: Vec<SubagentSnapshot>`, `#[serde(default, skip_serializing_if = "Vec::is_empty")]`, rather than a message family of its own.

  Why: the list is part of a session's state, and every row needs its host's snapshot anyway; it reaches every client through `Sessions`, `SessionUpdated`, the lag resync and `Welcome` with no new path; a removed session takes its subagents with it, so no list can outlive its host; and RT-30's comment asks for exactly this ("whether a session still has a live background subagent, on the snapshot", for RT-91 and the running-work strip).

  A dispatch run got a family of its own because it is not a session and outlives its host; a subagent is neither. The cost is a `SessionUpdated` per change, so the snapshot carries only fields that change when a subagent starts, finishes or first replies, never its open call or last text.
- **`SubagentSnapshot`** (new, in `crates/protocol/src/lib.rs`), every field after `agent_id` defaulted:
  - `agent_id: String`, `parent_agent_id: Option<String>`, `depth: u8` (1 for a first-layer subagent);
  - `agent_type`, `description`, `model`, `effort`: each `Option<String>`;
  - `state: AgentState` (the existing enum, which has `#[serde(other)] Unknown`), `end_status: Option<String>`;
  - `started_at`, `finished_at`: each `Option<DateTime<Utc>>`.

  The list is in tree order: each first-layer subagent, oldest first, followed by its subtree.
- **Additive, no bump.** A new struct, a defaulted field, and two new variants on enums that already carry `#[serde(other)] Unknown`. No `Welcome.features` entry: a daemon that sends no `subagents` draws no rows, and only a daemon that lists subagents is asked to follow one. `apps/pwa/src/protocol.ts` is regenerated; the phone app shows nothing new.
- **Saved data.** A pane's follow lives in the tab layout. Layouts from released builds load unchanged (no new field), and no released build wrote a `Subagent` follow, so nothing migrates. An older client reading a layout with one sees an `Unknown` follow and draws its muted note.

### Daemon: the list

- **Discovery stays in the plugin.** The daemon reads `ActivitySnapshot.subagents` through `agent_registry::plugin(&agent)?.activity_reader(&source)` and never names a file, a field or an agent.

  Files are the only source. The `SubagentStart` / `SubagentStop` hooks are not used: the file reader is built, tested and covers sessions started without hooks, and hooks would add a second path to save at most one tick of latency.
- **One list service** (`crates/daemon/src/subagents/mod.rs`, new): one task for the daemon's life that ticks every 2 s while `client_count` is above 0, parks at 0 and makes one catch-up pass when a client returns (the `git_watch` and `transcript_tail` pattern).

  Each pass takes the live agent sessions: an agent, a conversation id and a folder; not parked, inactive or abandoned; status not `Stopped` or `Exited`. It keeps one `ActivityReader` per session keyed by conversation id, folder and process start (a change of any makes a fresh reader, as the Dashboard's `ReaderKey` does), and reads them on the blocking pool.

  It builds each session's list and writes it through `SessionRegistry::update`, which sends the `SessionUpdated`, only when it differs from the record's. A session that leaves the live set gets an empty list once.
- **Its own readers.** The service does not share the Dashboard's readers: the Dashboard reads only while watched and diffs per watcher, while the list must read whenever a client is connected. The cost is a second incremental read of the same files while a Dashboard is open.
- **The record holds the list.** `SessionRecord.subagents: Vec<SubagentSnapshot>`, runtime-only (never in the sidecar or history), copied by `SessionRecord::snapshot()`. After a daemon restart it starts empty and the first pass refills it.
- **Which subagents are listed, and when one leaves.** A first-layer subagent is listed with its whole subtree while it or any descendant runs (`Running` or `Queued`).

  A finished or failed one stays listed while it ended after both the host's newest typed prompt and the start of the host's current agent process, newest five finished first-layer subagents at most. The next typed prompt, a restart of the agent, or a sixth finish removes it.

  Why not "gone when it finishes": a short subagent would vanish before it could be clicked, against ruling 1. Why not "until the host's next turn", the Dashboard's rule: a background subagent's finish wakes the main agent at once, so its row would vanish the moment it finished. The typed prompt is the point where the user has moved on; the cap keeps an autonomous loop that never sees a typed prompt from growing the list without end.
- **The typed-prompt time comes from the plugin.** `ActivitySnapshot.last_typed_prompt_at: Option<DateTime<Utc>>`: the newest main-thread `user` record the plugin's `line_facts` calls a prompt whose `origin.kind` is not `task-notification`. The daemon compares times only.
- **The sidecar's `finished` flag.** `SubagentReader` treats a sidecar with `finished: true` as finished (after rule 2, before rule 3 of its header), which ends a run whose last reply is not read yet and a run killed without a final reply. A sidecar without the field keeps today's rules.

### Daemon: the tail

- **One tail per followed subagent** (`crates/daemon/src/subagents/tail.rs`, new): `SubagentTails` keyed by `(session_id, agent_id)`, each holding a `TranscriptLog`, the `TailHandle` of a `transcript_tail::spawn` from byte 0, and the session's plugin. `Line` events fold through `TranscriptLog::fold` under the tails lock; `Restarted` calls `restarted`.

  The path comes from a new plugin method, `subagent_transcript_path(folder, conversation_id, agent_id, rows)`, defaulted to `None`, which the Claude plugin builds from `transcripts::transcript_path` and `subagents_dir`, refusing ids that are not plain.
- **The tail lives exactly while followed.** The first follow starts it. The last unfollow, the last follower's connection closing (a guard dropped with the connection, as `Dashboard::watch_guard` is) or the host session's removal stops it and drops its log; session removal first sends each follower `TranscriptAppend { ended: true }`. A later follow replays the file from byte 0.
- **Follow routing.** A new `crates/daemon/src/server/transcripts.rs` takes both follow messages and routes by source: `DispatchRun` to `DispatchRuns`, `Subagent` to `SubagentTails`, `Unknown` to `Error` with `UNSUPPORTED_MESSAGE`.

  A `Subagent` follow whose session is not listed, whose agent has no transcript path, or whose agent id is refused answers `Error` with `NOT_FOUND` and the `request_id`. Tailnet clients may follow, as they may follow a run: it is read-only.
- **`ended` means the host is gone.** A subagent that finishes is not `ended`: a resume appends to the same file. Its state shows in the pane's header from `SubagentSnapshot`, not from the transcript messages.
- **Startup prune.** At daemon start, after sessions are restored and beside `tabs::prune_sessions_not_in`, a local pane whose `Subagent` follow names a session that did not survive is cleared, as a dead run's follow is. A pane of another host is not this daemon's to judge.

### Native client: rows

- **Exactly like DS leaves** (ruling 1). `LeafKind` gains `Subagent`. A subagent leaf is a `Leaf` placed in its host's container right after the host session's leaf, with `host_session: Some(host id)` and `depth` its `SubagentSnapshot.depth` (1 to 3; a deeper one draws at 3). Its whole card, stripe included, is indented `NEST_INDENT` per level.

  Under one host the subagents come first, in tree order, then the host's dispatch runs, oldest first, as the Dashboard's `agent_tree` orders them. `Leaf.nested` counts both, and a new `Leaf.nested_subagents` counts the subagents for the chevron's wording.
- **Leaf content.** Id: the agent id. Runtime tag `SA`, drawn as the `DS` tag is. Label: the description, else the agent type, else `Subagent`. Subline: agent type and model joined with ` · `. Hover: agent type, model and effort, started time, and how it ended (`Finished`, or the end status).

  Status: `Running` and `Queued` read `Working`; `Finished` reads `Idle`, so the label dims like a seen idle session; `Failed` and `Unknown` read `Stopped`. No tab pill, chips, attention, busy count, Needs-you entry or notification; a subagent never makes its container active or adds to its count.
- **Fold** (ruling 3). The host's existing chevron and `session_fold_key` fold subagents and runs together; nested rows start open. The tooltip names what it hides: "Hide its subagents", "Hide its dispatch runs" or "Hide its subagents and dispatch runs" ("Show …" when folded). Subagent rows have no fold of their own.
- **Filters.** A subagent row shows only when its host's row shows and is not folded (`shown_leaves` already decides by `host_session`), whatever the Sessions-under-repos mode.
- **Tabs view** (ruling 4). `nest_tab_runs` nests a host's subagents under its leaf in every tab that shows the host. A subagent is never listed in Unbound, never as a followed leaf of its own, and nowhere in the Tabs view while no tab shows its host, even when a pane follows it.
- **The depth setting is dropped.** RT-406's Off / one level / full tree to depth 3 setting is not built. Ruling 1 says every subagent is followable and nested, which Off and one level would break, and ruling 3's fold is the way to hide them. The tree is always drawn to depth 3. RT-406's title and the sidebar-display canvas's option for it are out of date once this lands.
- **Click and drag.** A click focuses the pane already following the subagent, else asks This PC's daemon for a new tab whose pane follows it (`CreateTab` with the `Subagent` follow, the pane's host the session's host), as `open_dispatch_run` does.

  The leaf drags as `DragPayload::Subagent`, dropped as `drag::run_drop` drops a run: a pane's half edge splits it with a new following pane, the centre or an empty pane's edge makes that pane follow; a tab pill refuses it; it never reorders. A right-click opens nothing: Open is the only action, and read-only rules out Stop.

### Native client: the pane

- **The same view.** `apps/native/src/subagent_view.rs` (new) draws a pane with a `Subagent` follow: a header row (the description, `SA · <agent type> · <model>`, the state) over the body `transcript_view.rs` (TV.4) draws, in the terminal-style lines TV.4 settles. No stats line and no input. `grid_view.rs` routes the follow to it and titles the pane "Subagent".
- **Following.** The client derives a `TranscriptSource` from any pane follow (`DispatchRun` and `Subagent`), sends `FollowTranscript` to the pane's host when such a pane appears and `UnfollowTranscript` when it closes or rebinds, and re-follows after `Welcome`, replacing its model with the reply, as TV.4 does for runs.
- **Gone and finished.** The client keeps the last `SubagentSnapshot` of each followed subagent while a pane follows it. A finished subagent's pane shows its state and keeps its transcript; one no longer listed keeps its last snapshot and transcript until closed.

  `ended: true` (the host session was removed) adds "Its session has ended". A follow refused with `NOT_FOUND` with nothing kept (for example after a restart) reads "This subagent is no longer available".

### Scope

- **A dispatch run's own subagents are not listed.** A run is not a session the daemon's registry holds, and its transcript view already shows each `Agent` call and its result. Listing them would need a reader keyed by the run's conversation and worktree, which is a separate item if wanted.
- **Agent seam.** Protocol, daemon and client name no agent: `SubagentSnapshot`, `SA`, `subagents` and `subagent_transcript_path` name none, and every path, field and file name stays in `crates/agents/claude/`.

## Actors and invariants

Actors:

1. The host's `claude` process: appends to `agent-<id>.jsonl`, writes and rewrites `agent-<id>.meta.json`, appends task notifications to the launcher's transcript.
2. The list service's task: one pass per tick, readers on the blocking pool, list writes through `SessionRegistry::update`.
3. One tail task per followed subagent (`transcript_tail`), folding lines into its `TranscriptLog` under the tails lock.
4. Connection tasks: `FollowTranscript`, `UnfollowTranscript`, and the follow guard dropped when the connection closes.
5. The session registry: status changes, conversation id changes (`/clear`, resume), a new agent process, session removal.
6. Daemon startup: session restore, then the tab prune.
7. Clients: fold `SessionUpdated`, follow and unfollow, re-follow after `Welcome`.

Invariants (each gets a test):

- **A subagent file growing.** Every complete line reaches a follower exactly once, in file order: the backfill is built and the follower registered under the tails lock that `fold` takes, so a line is in the backfill or in a later append, never both and never neither. A partial line is never parsed.
- **A new subagent appearing.** It is listed on the first pass after its files exist and a launch joins it (`SubagentReader`'s join rules); a sidecar that does not parse yet keeps it out until it does. The tail of a subagent followed before its file exists waits for the file and sends an empty backfill first.
- **A session ending.** A session whose agent process stops gets an empty list on the next pass; its removal sends `ended: true` to every follower of its subagents and stops their tails. Panes keep their content and close only when the user closes them.
- **A daemon restart.** Nothing is persisted: the record's list starts empty and the first pass after a client connects rebuilds it from the files; subagents last written before the agent process started read finished (`SubagentReader` rule 1).

  Clients re-follow after `Welcome` and each tail replays from byte 0. The startup prune runs after session restore, so a pane following a subagent of a session that survived keeps its follow.
- **A follow mid-file.** A follow always reads from byte 0; the log keeps the newest `CHAT_REPLY_CAP` entries and counts the rest in `hidden`; a tool result whose call was dropped is dropped.
- **A conversation change.** A `/clear` or resume gives the record a new conversation id or process start, so the next pass uses a fresh reader and lists the new conversation's subagents; a tail already running keeps its resolved path until it is unfollowed.
- **Lock order.** The record lock is never held while reading files; the tails lock is never held while taking a record lock (a follow resolves the path from the record, releases it, then takes the tails lock).
- **No idle traffic.** A pass whose lists match the records sends nothing; with no client connected the service and every tail read nothing.
- **No path from the client.** The daemon builds a subagent path only from its own record and the plugin, after the plugin accepts both ids as plain.

## Steps

Each step lands on `main` on its own and keeps the suite green. Hotspot files are marked.

- [ ] **SS.1 Protocol.** `SubagentSnapshot`, `SessionSnapshot.subagents`, `PaneFollow::Subagent { session_id, agent_id }`, `TranscriptSource::Subagent { session_id, agent_id }`. The daemon's `SessionRecord::snapshot()` sends an empty list; `grid_view.rs`'s two exhaustive `PaneFollow` matches get an arm (the muted placeholder, title "Subagent").

  Files: `crates/protocol/src/lib.rs` (hotspot), `apps/pwa/src/protocol.ts` (regenerated), `crates/daemon/src/session.rs`, `apps/native/src/grid_view.rs`.

  Tests: round trips of both variants and of a full `SubagentSnapshot`; a snapshot without `subagents` decodes with it empty, and an empty list is left out; `unknown_pane_follow_kind_decodes_as_unknown` and `an_unknown_transcript_source_decodes_as_unknown` use `"subagent"` as their unknown kind today and move to a kind no build knows; `generated_protocol_ts_is_current`. Depends on: TV.1 (landed).
- [ ] **SS.2 Plugin: the subagent transcript path.** `subagent_transcript_path(folder, conversation_id, agent_id, rows)` on the typed `Agent` trait (default `None`), `AgentPlugin` and the blanket adapter; the Claude plugin's implementation over `transcripts::transcript_path` and `subagents_dir`, refusing ids that are not plain.

  Files: `crates/agents/api/src/lib.rs`, `crates/agents/claude/src/lib.rs`, `crates/agents/claude/src/transcripts.rs`, `crates/agents/claude/tests/fixtures/transcripts/subagent-run.jsonl` (new: a redacted real subagent transcript with a prompt, a reply, a tool call and its result).

  Tests: the path honours the `CLAUDE_CONFIG_DIR` row; a non-plain conversation or agent id gives `None`; `chat_entries` over the fixture yields the user prompt, the answer and one tool card with its result (in `chat_entries.rs`'s tests). Depends on: nothing.
- [ ] **SS.3 Plugin: typed-prompt time and the sidecar's `finished`.** `ActivitySnapshot.last_typed_prompt_at`, set by the Claude activity reader; `SubagentReader` reads the sidecar's `finished: true` as finished.

  Files: `crates/agents/api/src/activity.rs`, `crates/agents/claude/src/activity.rs`, `crates/agents/claude/src/subagents.rs`.

  Tests: a typed prompt moves the time and a task-notification record does not; a sidecar with `finished: true` finishes a subagent whose last reply holds a tool call; one without the field keeps rule 4. Depends on: nothing.
- [ ] **SS.4 Daemon: the record's list.** `SessionRecord.subagents`, runtime-only, copied by `snapshot()`.

  Mechanical: every `SessionRecord` literal gains `subagents: Vec::new()` (the compiler names them: `session.rs`, `server/spawn.rs`, `server/test_support.rs`, `keep_awake.rs`, `idle_exit.rs`, `history.rs`, `pty_state.rs`), a one-line change each, which is why this step exceeds five files.

  Test in `session.rs`: a record with a list snapshots it; the sidecar written for it carries no list. Depends on: SS.1.
- [ ] **SS.5 Daemon: the list service.** The tick task, readers, the listing rule (running subtree; finished after the typed prompt and the process start; five finished at most), the flattening to `SubagentSnapshot` in tree order, and compare-and-write through `SessionRegistry::update`; started beside the Dashboard's ticker.

  Files: `crates/daemon/src/subagents/mod.rs` (new), `crates/daemon/src/subagents/tests.rs` (new), `crates/daemon/src/server.rs` (hotspot: construct and spawn), `crates/daemon/src/main.rs` (the `mod` line).

  Tests, with a scripted reader factory as the Dashboard's tests use: a running subagent reaches the snapshot with one `SessionUpdated`; an unchanged pass sends none; a finished one stays until a newer typed prompt; one finished before the process start is never listed; a sixth finish drops the oldest; a nested running child keeps its finished parent listed; a stopped session's list empties once; at `client_count` 0 no reader is called. Depends on: SS.3, SS.4.
- [ ] **SS.6 Daemon: share the transcript log.** Move `dispatch_runs/transcript.rs` to `crates/daemon/src/transcript_log.rs` as `pub(crate)` with its tests; `DispatchRuns` uses it unchanged.

  Files: `crates/daemon/src/dispatch_runs/transcript.rs` (moved), `crates/daemon/src/transcript_log.rs`, `crates/daemon/src/dispatch_runs/mod.rs`, `crates/daemon/src/main.rs` (the `mod` line).

  Test: the moved tests and `dispatch_runs/tests.rs` pass unchanged. Depends on: TV.3 landing on `main`.
- [ ] **SS.7 Daemon: subagent tails and follow routing.** `SubagentTails` (started by the first follow; stopped by the last unfollow, a closed connection, or session removal with `ended: true`), the follow guard, and `server/transcripts.rs` routing both follow messages by source.

  Files: `crates/daemon/src/subagents/tail.rs` (new), `crates/daemon/src/server/transcripts.rs` (new), `crates/daemon/src/server.rs` (hotspot: the `Hub` field, and the or-pattern moving to `transcripts::handle`), `crates/daemon/src/server/dispatch_runs.rs` (its follow arms go), `crates/daemon/src/server/connection.rs` (the guard).

  Tests on a temp `CLAUDE_CONFIG_DIR`: a follow answers the backfill with its `request_id`; appended lines reach only followers, each once; a follow before the file exists gets an empty backfill and then the lines; an unknown session or a refused id answers `NOT_FOUND`; the last unfollow and a closed connection stop the tail; removing the session sends `ended: true`; a dispatch-run follow still works. Depends on: SS.1, SS.2, SS.6.
- [ ] **SS.8 Daemon: startup prune.** Clear a local pane's `Subagent` follow whose session did not survive, beside `prune_sessions_not_in`.

  Files: `crates/daemon/src/tabs.rs`, `crates/daemon/src/main.rs`. Tests in `tabs.rs`: a follow of a restored session stays, one of a lost session clears, another host's pane is untouched. Depends on: SS.1.
- [ ] **SS.9 Client: sidebar model.** `LeafKind::Subagent`, `Leaf.nested_subagents`, a `subagent_leaf(host, host id, &SubagentSnapshot)` builder, and nesting in `nest_after` (subagents, then runs) for the repos view and in `nest_tab_runs` for the tabs view. `sidebar_view.rs`'s exhaustive `LeafKind` matches get an arm (no drag payload yet, dimmed with the host).

  Files: `apps/native/src/sidebar.rs`, `apps/native/src/subagents.rs` (new, the twin of `dispatch_runs.rs`), `apps/native/src/sidebar_view.rs`, `apps/native/src/lib.rs` (hotspot: the `mod` line).

  Tests in `sidebar.rs`: rows follow the host in tree order at depths 1 and 2, before its runs; `nested` and `nested_subagents` count them; a folded host hides them; in the tabs view a bound host nests them in each tab that shows it, and an unbound host's are in no container, never Unbound, even while a pane follows one. Depends on: SS.1.
- [ ] **SS.10 Client: rows drawn and opened.** The `SA` tag, label, subline, hover and status mapping; the chevron's wording by kind; `RootView::open_subagent` (focus the follower, else `CreateTab` to This PC with the follow); a `find_tab_following_subagent` beside `find_tab_following`.

  Files: `apps/native/src/sidebar_view.rs`, `apps/native/src/lib.rs` (hotspot), `apps/native/src/tabs.rs`, `apps/native/tests/ui/ui_sidebar.rs`.

  Spec: a scripted snapshot with two subagents draws both indented under the host with `SA`; the chevron reads "Hide its subagents and dispatch runs" when both kinds are nested, and folds them; a click sends `CreateTab` with the `Subagent` follow to This PC; a second click focuses that pane. Depends on: SS.9.
- [ ] **SS.11 Client: drag.** `DragPayload::Subagent` and `drag::subagent_drop`, the twin of `run_drop`.

  Files: `apps/native/src/drag.rs`, `apps/native/src/sidebar_view.rs`, `apps/native/tests/ui/ui_sidebar_drag.rs`. Spec: a drop on a pane's half edge sends `SplitPane` with the follow, on its centre `ReplacePaneSession` with the follow; a tab pill refuses it; it never reorders. Depends on: SS.10.
- [ ] **SS.12 Client: the pane.** `subagent_view.rs` (the header over the TV.4 transcript body), `grid_view.rs` routing, the follow source derived from any pane follow, the kept last snapshot and the gone notes.

  Files: `apps/native/src/subagent_view.rs` (new), `apps/native/src/grid_view.rs`, `apps/native/src/lib.rs` (hotspot), `apps/native/tests/ui/ui_subagents.rs` (new, with its `mod` line in `tests/ui/main.rs`).

  Spec: a pane with the follow sends `FollowTranscript` with the `Subagent` source to the pane's host; the reply's answer and tool card draw; an append adds a row; the header follows the snapshot's state; a snapshot that drops the subagent leaves the transcript and the last header; `ended: true` shows the session-ended note; closing the pane sends `UnfollowTranscript`; a `Welcome` re-follows. Depends on: SS.10 and TV.4.
- [ ] **SS.13 Shot and live e2e.** A `subagents` `native-shot` view (a host with nested subagent and run rows beside an open subagent pane); a fake-claude mode that writes a sidecar and a transcript under `$CLAUDE_CONFIG_DIR/projects/<encoded cwd>/<conversation id>/subagents/`, appends to it, then finishes it; an `e2e_live` spec run through `.\rt.ps1 native-e2e -Test e2e_live`.

  Files: `apps/native/examples/shot.rs`, `crates/agents/claude/fake/index.mjs`, `crates/agents/claude/fake/README.md`, `apps/native/tests/e2e_live.rs`.

  Spec: the row appears under its host; a click opens a pane that streams the appended lines; the row reads finished; a typed prompt removes it while the open pane keeps its transcript. Depends on: SS.5, SS.7, SS.12.
- [ ] **SS.14 Docs.** Promote the lasting decisions, then delete this file:
  - the list service and the tails to `docs/session-runtime.md`, the wire to `docs/wire-protocol.md`, the rows and the pane to `docs/native-client.md` ("Sessions under repos"), a task-index row to `docs/architecture.md`;
  - `SA` and "typed prompt" to the glossary in `docs/README.md`, the `subagents` shot view to `docs/testing.md` and CLAUDE.md's view list;
  - TV.6 removed from [transcript-view.md](./transcript-view.md).

  Depends on: every step above.
