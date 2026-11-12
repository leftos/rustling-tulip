# Transcript view (RT-1581, then RT-30)

A pane that follows a dispatch run, and later a session's running subagent, shows the run's real transcript the way watching Claude Code in a terminal does: the assistant's text, each tool call with its input, the tool's output, following live and read-only. It replaces the follow pane's list of recent actions, which stays only as the fallback for a host whose daemon lacks the messages below.

Owner ruling: every subagent session and every dispatch run can be followed read-only inside the app with its actual transcript, nested under its host session in the sidebar (RT-30, RT-1580).

## Decisions

These are the orchestrator's decisions (not owner rulings), settled from the code and the existing chat mirror; each is open to the owner.

- **One entry type.** The transcript reuses the chat mirror's `ChatEntry` (`Answer`, `Tool` with `input` and `result: Option<ToolOutcome>`, `Thinking`, `System`, `TurnEnd`), parsed by the Claude plugin's `chat_entries` with its caps (`TOOL_INPUT_CAP`, `TOOL_RESULT_CAP`).

  Stream-json `assistant` and `user` records carry `uuid`, `parent_tool_use_id` and the same `message.content` blocks as transcript records (checked on a real DeepSeek run's `stream.jsonl`), so no new entry type and no second parser are needed. Other stream record types (`system`, `tool_progress`, `result`) yield no entry.
- **Subscription.** Explicit `ClientMessage::FollowTranscript { source, request_id }` and `UnfollowTranscript { source }`, mirroring `LoadChat` and `UnloadChat`. `TranscriptSource` is an internally tagged enum with `#[serde(other)] Unknown`: `DispatchRun { run_id }` now, `Subagent { .. }` with RT-30.
- **Replies.** `DaemonMessage::Transcript { source, entries, hidden, request_id }` is the backfill; `TranscriptAppend { source, entries, ended }` goes to followers only, with the chat's replace-by-id rule (a tool result re-sends its call's card under the same id). Neither rides the run snapshot, which every client receives.
- **One fold.** The daemon builds each run's entry log in the tailer's `fold`, always, under the same generation guard, and clears it on `restart` and `replace`. The follow reply is built and the follower registered under the lock `fold` takes, so an append is either in the backfill or sent after it. `recent_actions` stays beside it as the fallback.
- **Caps.** The log keeps the last 500 entries (`CHAT_REPLY_CAP`), counting the dropped ones in `hidden`. A result for a dropped call is dropped.
- **Thinking.** `Thinking` entries are not drawn in the transcript view.
- **Look.** The view reads like Claude Code in a terminal, in the terminal font: an answer is `● {text}`; a tool call is `● {name}({summary})` with its output under it as `  ⎿  {line}`, the first 5 lines and then `     … +{n} lines`; a call still waiting for its result shows `  ⎿  Running…`; a failed result is drawn in the error colour. A click on a tool call's rows expands its output to the whole capped text and collapses it again. System rows are muted.
- **Result line.** The stream's `result` record shows only in the pane's stats line (status, cost, tokens), as today.
- **Ended runs.** A run that ended keeps its last transcript in the client while the pane stays open, as the kept snapshot does. A run that ended before the client connected shows the existing gone note; reading an ended run's `stream.jsonl` from its run dir is out of scope.
- **Reconnect.** After `Welcome` the client re-sends `FollowTranscript` for each followed run and replaces its model with the reply, ignoring a reply whose `request_id` is stale.
- **Branch.** Lands on `main` step by step: every step is additive and the pane falls back to recent actions until the client steps land.

## Steps

- [x] TV.1 Protocol: the four messages and `TranscriptSource` in `crates/protocol/src/lib.rs`, round-trip tests, `apps/pwa/src/protocol.ts` regenerated.
- [x] TV.2 Parse: the agent's existing `chat_entries(line, known)` reads stream-json records as they are, proved by a real stream-json fixture.
- [x] TV.3 Daemon: the entry log in `RunEntry`, folded in `fold`; follow and unfollow handled in `DispatchRuns`, a follower dropped at its first failed send and at run removal.
- [x] TV.4 Client: `apps/native/src/transcript_view.rs` (backfill, append upsert by id, the expanded set, the terminal-style lines), the follow and unfollow sends and the message fold in `RootView`, `dispatch_view.rs` drawing the transcript in place of recent actions once one has arrived, a `ui_transcript` spec.
- [x] TV.5 Shot and e2e: the `dispatch` native-shot fixture gains a transcript; fake-claude's stream mode writes `user`/`tool_result` records; `e2e_dispatch` follows a transcript.
- [ ] TV.6 Subagent source (RT-30, after RT-1510): `TranscriptSource::Subagent` and `PaneFollow` variant, a subagent-file tail feeding the same log.
