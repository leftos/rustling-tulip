# Spike RT-84: hook payloads when a turn ends with background work running

CLI: `claude --version` printed `2.1.308 (Claude Code)`; the TUI banner showed `Claude Code v2.1.308`. Model `--model haiku` (resolved `claude-haiku-5-5`).

## Method

- Scratch dir `X:\dev\rustling-tulip\.tmp\spike-rt84\`, one fresh `git init` folder per run (`work-<run>\`). Nothing outside it was touched.
- Logger `hooklog.mjs` (node): appends `{t: epoch ms at process start, label, pid, event, payload: <stdin JSON>}` to `log-<run>.jsonl`.

  `mksettings.mjs` writes `settings-<run>.json` with one async exec-form hook (`"command": "C:\\Program Files\\nodejs\\node.EXE", "args": [hooklog.mjs, "inject", <log>], "async": true`, no matcher) for `PreToolUse, PostToolUse, PostToolUseFailure, PermissionRequest, Notification, UserPromptSubmit, Stop, StopFailure, SubagentStart, SubagentStop, SessionStart, SessionEnd`.
- Every run: `--settings <file> --setting-sources project,local --strict-mcp-config --model haiku`, with every `CLAUDE*`/`ANTHROPIC*` env var stripped from the child.
- `-p` runs (`run.sh`): `--permission-mode bypassPermissions --output-format stream-json --verbose`, `--max-turns 5-8`; stream output kept in `out-<run>.jsonl`.
- TUI runs (`tui.py`, pywinpty + pyte via `uv run`): ConPTY 140x45, trust dialog accepted, `--allowedTools Bash Agent`; the TUI came up in `auto` permission mode. Driver notes and screen dumps in `notes-<run>.txt`.
- `show.mjs <log>` prints a log sorted by start time. Below, times are `+ms` from the run's `SessionStart` (hook start times, so async completion order does not matter). Common fields (`session_id`, `transcript_path`, `cwd`, `hook_event_name`, `effort`, the TUI's `scratchpad_dir`) are dropped.

Runs: c1, c2, c3 (`-p`), t4, t5 (TUI), each a few haiku turns (c1's two results reported $0.0047 and $0.0052).

## Case 1: background Bash, turn ends while it runs

c1 (`-p`): "Bash with run_in_background: `sleep 20; echo done-sleeping`, reply started".

```
+0      SessionStart      {"source":"startup"}
+346    UserPromptSubmit  {"prompt_id":"a83198c5-…","prompt":"Use the Bash tool exactly once with run_in_background…"}
+1920   PreToolUse        {"tool_name":"Bash","tool_input":{"command":"sleep 20; echo done-sleeping","description":"Sleep 20 seconds then echo done-sleeping","run_in_background":true},"tool_use_id":"toolu_019up2xQ…"}
+4553   PostToolUse       {"tool_name":"Bash",…,"tool_response":{"stdout":"","stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false,"backgroundTaskId":"btzw7sew7"},"duration_ms":2623}
+5403   Stop              {"prompt_id":"a83198c5-…","stop_hook_active":false,"last_assistant_message":"started",
                           "background_tasks":[{"id":"btzw7sew7","type":"shell","status":"running","description":"Sleep 20 seconds then echo done-sleeping","command":"sleep 20; echo done-sleeping"}],
                           "session_crons":[]}
+24840  UserPromptSubmit  {"prompt_id":"d59724b4-…","prompt":"<task-notification>\n<task-id>btzw7sew7</task-id>\n<tool-use-id>toolu_019up2xQ…</tool-use-id>\n<output-file>C:\\Users\\lefto\\AppData\\Local\\Temp\\claude\\…\\tasks\\btzw7sew7.output</output-file>\n<status>completed</status>\n<summary>Background command \"Sleep 20 seconds then echo done-sleeping\" completed (exit code 0)</summary>\n</task-notification>"}
+26771  Stop              {"prompt_id":"d59724b4-…","last_assistant_message":"The background sleep finished with exit code 0. …","background_tasks":[],"session_crons":[]}
+28289  SessionEnd        {"reason":"other"}
```

- Shell element shape: `{"id", "type":"shell", "status":"running", "description", "command"}`. `id` equals `PostToolUse.tool_response.backgroundTaskId`.
- When the shell finishes, the CLI starts a new turn by itself: `UserPromptSubmit` (new `prompt_id`) whose `prompt` is the `<task-notification>` block, then a normal `Stop` with the task gone from `background_tasks`. No dedicated hook event marks the task's end. `-p` did not exit at the first `Stop`; it waited for the task and its notification turn, then ended.
- The stream-json side (not hooks) carries `system` messages `background_tasks_changed` (full list), `task_started`, `task_updated {patch:{status:"completed"}}` and `task_notification`.

TUI (t5) is the same: `Stop` +28880 with `background_tasks:[{"id":"b0g5g1b8q","type":"shell","status":"running",…}]`; the shell finished while the TUI sat idle at the prompt, and +42719 `UserPromptSubmit` `<task-notification>…<task-id>b0g5g1b8q</task-id>…` fired, then `Stop` +44325 with `background_tasks: []`.

## Case 2: background subagent, turn ends while it runs

c2 (`-p`): Agent tool, `run_in_background: true`, `general-purpose`, the agent runs a foreground `sleep 15`.

```
+2103   PreToolUse     {"tool_name":"Agent","tool_input":{"description":"sleep helper","prompt":"…","subagent_type":"general-purpose","run_in_background":true},"tool_use_id":"toolu_01QaUozW…"}
+2130   SubagentStart  {"prompt_id":"b4f7010c-…","agent_id":"a2bfb811c49c06f72","agent_type":"general-purpose"}
+2159   PostToolUse    {"tool_name":"Agent",…,"tool_response":{"isAsync":true,"status":"async_launched","agentId":"a2bfb811c49c06f72","description":"sleep helper","resolvedModel":"claude-haiku-5-5","outputFile":"…\\tasks\\a2bfb811c49c06f72.output","canReadOutputFile":true,"canContinueAgent":true},"duration_ms":6}
+3033   Stop           {"prompt_id":"b4f7010c-…","last_assistant_message":"launched",
                        "background_tasks":[{"id":"a2bfb811c49c06f72","type":"subagent","status":"running","description":"sleep helper","agent_type":"general-purpose"}],"session_crons":[]}
+3719   PreToolUse     {"prompt_id":"b4f7010c-…","agent_id":"a2bfb811c49c06f72","agent_type":"general-purpose","tool_name":"Bash","tool_input":{"command":"sleep 15",…}}
+21272  PostToolUse    {"agent_id":"a2bfb811c49c06f72","agent_type":"general-purpose","tool_name":"Bash",…,"duration_ms":17541}
+22057  SubagentStop   {"prompt_id":"b4f7010c-…","agent_id":"a2bfb811c49c06f72","agent_type":"general-purpose","stop_hook_active":false,
                        "agent_transcript_path":"C:\\Users\\lefto\\.claude\\projects\\…\\subagents\\agent-a2bfb811c49c06f72.jsonl","last_assistant_message":"ok",
                        "background_tasks":[{"id":"a2bfb811c49c06f72","type":"subagent","status":"running",…}],"session_crons":[]}
+22090  UserPromptSubmit {"prompt_id":"b0a6323f-…","prompt":"<task-notification>\n<task-id>a2bfb811c49c06f72</task-id>\n<tool-use-id>toolu_01QaUozW…</tool-use-id>…"}
+23135  Stop           {"prompt_id":"b0a6323f-…","last_assistant_message":"noted","background_tasks":[],"session_crons":[]}
```

- Yes, it is in `background_tasks`: `{"id":<agent_id>, "type":"subagent", "status":"running", "description", "agent_type"}`. The `id` equals `SubagentStart.agent_id`.
- `SubagentStart` fires at launch (between the Agent `PreToolUse` and `PostToolUse`) with `agent_id` and `agent_type`; `SubagentStop` fires when it finishes, with `agent_id`, `agent_type`, `agent_transcript_path`, `last_assistant_message`, and its own `background_tasks` snapshot.
- **`SubagentStop`'s `background_tasks` still lists the stopping agent as `running`** (c2 and t4). The snapshot is taken before the task record updates, so it cannot be used to clear the state.
- The subagent's own tool events carry `agent_id` and `agent_type`; main-thread events never carry them. They keep firing after the main `Stop`, while the main thread is idle.
- The subagent's tool events carry the main thread's **current** `prompt_id`, not the turn that launched it. In t4 the agent's `PostToolUse`/`SubagentStop` carried the `prompt_id` of a later "hi" turn.
- A foreground Bash inside the subagent did not appear in `background_tasks` (the stream showed it as `task_started … owned_by_subagent: true, is_backgrounded: false, parent_task_id: <agent>`).

TUI (t4) differs in the completion message. Haiku omitted `run_in_background` from the Agent call, yet the TUI backgrounded the agent anyway (`tool_response.isAsync: true, status: "async_launched"`; the screen said "Backgrounded agent"). The agent ended through a `SubagentHandback` tool (`PreToolUse`/`PostToolUse` with `agent_id`), then `SubagentStop` (no `last_assistant_message` field).

The CLI then submitted `UserPromptSubmit` with `prompt` = `<agent-message from="adfbf049d1cdc225f">\n[Subagent hand-back] The text below is the final report…</agent-message>`, not a `<task-notification>`. The next `Stop` no longer listed the agent. Whichever wrapper is used, a completion is an auto-submitted `UserPromptSubmit` followed by a `Stop` carrying the updated list.

## Case 3: CronCreate and ScheduleWakeup (`-p` works)

Both tools exist in `-p` (deferred; the model loaded them through `ToolSearch`). c3:

```
+4333   PreToolUse   {"tool_name":"CronCreate","tool_input":{"cron":"* * * * *","prompt":"reply with the word tick","recurring":true}}
+4339   PostToolUse  {… "tool_response":{"id":"cc6750c9","humanSchedule":"Every minute","recurring":true,"durable":false,"nextFireAt":1792696823953.5442}}
+4592   PreToolUse   {"tool_name":"ScheduleWakeup","tool_input":{"delaySeconds":60,"reason":"…","prompt":"reply with the word woke","noop":false}}
+4599   PostToolUse  {… "tool_response":{"scheduledFor":1792696860000,"clampedDelaySeconds":60,"wasClamped":false}}
+7053   Stop         {"last_assistant_message":"scheduled","background_tasks":[],
                      "session_crons":[{"id":"cc6750c9","schedule":"* * * * *","recurring":true,"prompt":"reply with the word tick"},
                                       {"id":"d62c3318","schedule":"21 12 * * *","recurring":false,"prompt":"reply with the word woke"}]}
+72393  UserPromptSubmit {"prompt_id":"78eb066b-…","prompt":"reply with the word woke"}
+73109  Stop         {"last_assistant_message":"woke","background_tasks":[],"session_crons":[{"id":"cc6750c9","schedule":"* * * * *","recurring":true,"prompt":"reply with the word tick"}]}
+74637  SessionEnd   {"reason":"other"}
```

- `session_crons` element: `{"id", "schedule", "recurring", "prompt"}`. A `ScheduleWakeup` appears there as a one-shot cron (`recurring: false`, `schedule` pinned to the fire minute and hour, `"21 12 * * *"`), with an id of its own (not returned by the tool).
- A fired wakeup is a `UserPromptSubmit` whose `prompt` is the wakeup's prompt text verbatim. Nothing in the payload marks it as scheduled rather than typed.
- The recurring every-minute cron did not fire in the 67 s the process lived (`nextFireAt` was 30 s after creation). `-p` waited for the one-shot wakeup, ran its turn and exited with the recurring cron still listed. Recurring crons were not tried in the TUI.

## Case 4: the prompt-suggestion agent

Not seen in 2.1.308. Across six TUI `Stop`s (t4: 3, t5: 3; t5 waited 12 s after each of two `Stop`s), no `SubagentStart` and no `SubagentStop` with `agent_type: ""` fired. The input box showed an empty `❯` after each turn, no suggestion placeholder. In HS.0 (2.1.301) it fired `SubagentStop` 1.5-2 s after every TUI `Stop`.

Whether it fires `SubagentStart` could not be answered, because it did not run at all. Whether that is a version change or a server-side flag could not be told from here. Every `SubagentStart`/`SubagentStop` seen in this spike had a non-empty `agent_type` and a matching task in `background_tasks`.

## Case 5: interrupt while background work runs (TUI, t4)

Turn 1 launched a background shell (`sleep 50`) and a background agent. Turn 2 ran a foreground `ping -n 31`, and Esc interrupted it 2 s after its `PreToolUse`.

```
+13192  Stop  {"last_assistant_message":"started","background_tasks":[{"id":"adfbf049d1cdc225f","type":"subagent","status":"running",…},{"id":"bq7tunood","type":"shell","status":"running",…}]}
+20013  UserPromptSubmit "Run the Bash command ping -n 31 …"
+21838  PreToolUse Bash "ping -n 31 127.0.0.1 > /dev/null"
        (Esc at +23928: no event; screen "Interrupted · What should Claude do instead?")
+29667  UserPromptSubmit "Reply with the single word hi."
+31050  Stop  {"last_assistant_message":"hi","background_tasks":[<same subagent, running>,<same shell, running>]}
+37509  PostToolUse (agent_id=adfbf…) Bash ping -n 26   ← the background agent kept running through the Esc
+39143  SubagentStop (agent_id=adfbf…)  background_tasks still lists it running
+39172  UserPromptSubmit "<agent-message from=\"adfbf049d1cdc225f\">…"
+41825  Stop  {"background_tasks":[{"id":"bq7tunood","type":"shell","status":"running",…}]}
```

- Esc interrupts only the foreground turn. It fires nothing, as in HS.0, and the background shell and agent keep running with unchanged records.
- After an interrupt there is no `Stop` until the next turn ends, so between the Esc and the next `Stop` the hooks give no fresh list.
- `/exit` while the shell was still running produced no `SessionEnd` within 15 s (the driver then killed the process; the screen after `/exit` was not captured, presumably a confirm). `/exit` with nothing running (t5) fired `SessionEnd {"reason":"prompt_input_exit"}` at once, under the `/exit` input's own new `prompt_id`. Killing a task through the TUI's task manager was not tried.

## Signals

- **Turn ended, background still running:** the latest **main-thread** `Stop` (no `agent_id` field) has a non-empty `background_tasks` array containing an element with `status == "running"`. Treat `session_crons` separately: a non-empty array means "something scheduled", including a pending `ScheduleWakeup` (`recurring: false`), but nothing is running.
- **Nothing running:** the latest main-thread `Stop` has `background_tasks: []` (and `session_crons: []` if crons count).
- **The end of background work** is never its own event. It shows up as an auto-submitted `UserPromptSubmit` (prompt starting `<task-notification>` or `<agent-message from=`), which correctly moves the session to `Working`, followed by a `Stop` whose list no longer has that id. So "background" is cleared by the next `UserPromptSubmit`/`Stop`, with no extra bookkeeping. A cron or wakeup fire is the same auto `UserPromptSubmit`, with no marker.
- **No SubagentStart/Stop counter is needed.** `background_tasks` already lists background subagents (`type: "subagent"`) beside shells (`type: "shell"`). A counter would also be wrong in two ways seen here: `SubagentStop`'s own `background_tasks` still shows the agent as running, and foreground (non-background) subagents fire the same Start/Stop pair. The prompt-suggestion agent's `agent_type: ""` `SubagentStop` from HS.0 did not appear in 2.1.308.
- **Subagent events after the main `Stop`:** a background agent's `PreToolUse`/`PostToolUse`/`SubagentStop` fire while the main thread is idle. They carry `agent_id`, and the main thread's current `prompt_id`, so they must not promote the session to `Working` or be ordered by `prompt_id`. Filter them by the presence of `agent_id`.
- **Gaps the daemon must cover itself:** an Esc mid-turn leaves the last `Stop`'s list as the only data until the next `Stop`, and that list stays valid since background work survives Esc. An interrupted notification turn also has no `Stop`. A session that dies or is killed with background work gets no clearing event beyond `SessionEnd`, which did not come in 15 s after `/exit` with a running shell.

## Not exercised

- Case 4 `SubagentStart` for the prompt-suggestion agent: the agent never ran in 2.1.308 here, so there was nothing to observe.
- A recurring cron firing: it did not fire in the `-p` run's 67 s, and it was not tried in the TUI.
- The `/exit` screen with a running background task, and killing a task from the TUI's `↓ to manage` panel.
- A task finishing with a failure status (`status` other than `running`/`completed`), or a `background_tasks` element whose `status` is not `running`: every element seen was `running`, and finished tasks were dropped from the list rather than kept with another status.

Files: `X:\dev\rustling-tulip\.tmp\spike-rt84\` (`hooklog.mjs`, `mksettings.mjs`, `run.sh`, `tui.py`, `show.mjs`, `log-*.jsonl`, `out-c*.jsonl`, `notes-t*.txt`).
