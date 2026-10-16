# Spike: the Claude Code hook behaviour hook-reported status leans on

2026-10-16. Step HS.0 of [hook-status.md](../plans/hook-status.md) (Linear RT-25). Question: does the real `claude` CLI on Windows behave the way the design's injected settings, mapping table and arbitration assume?

`claude --version` printed `2.1.301 (Claude Code)` before the runs. The CLI updated itself to 2.1.302 during the last runs (the TUI footer showed "✔ Update installed · Restart to update"), and every session ran the 2.1.301 binary it started with. Afterwards `claude --version` printed `2.1.302 (Claude Code)`.

## How the runs were made

- **Hook logger.** `hooklog.exe <label> <logfile>`: a 30-line Rust program built with `rustc`. It reads stdin to EOF and appends one line per call: the epoch ms at process start, the label, the pid, `event=<hook_event_name>`, any `SPIKE_*` env vars, and stdin flattened to one line. `hooklog.mjs` is the same logger for `node.exe`.
- **Settings.** Every layer is a file holding only a `hooks` object. Each event gets one async exec-form hook, the shape the design injects: `{"type": "command", "command": "<abs path>\\hooklog.exe", "args": ["inject", "<log>"], "async": true}`. There is no matcher, so every tool and notification type is logged. The `local-node` layer uses `"command": "C:\\Program Files\\nodejs\\node.EXE", "args": ["<abs path>\\hooklog.mjs", "local-node", "<log>"]`.
- **Isolation.** Every session ran on the real config with `--setting-sources project,local --strict-mcp-config --model haiku`. That flag keeps `~/.claude/settings.json` out, so its hooks, Moshi among them, never ran. The TUI runs also stripped every `CLAUDE*` and `ANTHROPIC*` variable from the child's environment.

  The spike itself runs inside a Claude session. Without the strip the child inherits `CLAUDE_CODE_CHILD_SESSION` (the TUI warned "Transcript saving is off — inherited CLAUDE_CODE_CHILD_SESSION marker") and `CLAUDE_CODE_PLUGIN_DIRS`. The daemon is not affected: `passthrough_env` in `server.rs` passes only an allowlist.
- **TUI.** The TUI runs drove `claude.exe` through a ConPTY (`uv run --with pywinpty --with pyte`): a pyte screen, keystrokes written to the PTY, the hook log read between steps. The driver handled the startup dialogs: Down then Enter on the trust dialog, Enter ("No") on the external-imports dialog.
- **Folders.** Each folder was a fresh `git init` folder. Trusted-folder runs used `.tmp\spike\<run>\` in this worktree. The trust run (Q3) used `%TEMP%\rt-hs0-spike\<run>\`. Before that run, a probe of `~/.claude.json` printed `'C:/Users/lefto' hasTrustDialogAccepted= False` and listed no other ancestor of `%TEMP%`.

  The `%TEMP%` folders were deleted afterwards. The scripts and logs are untracked, under `.tmp\spike\` in the `hs-0` worktree.

| Run | Mode | What it did |
| - | - | - |
| r1 | `-p` | `say hi` with `--settings` (`inject`) + `.claude/settings.json` (`repo`) + `.claude/settings.local.json` (`local-node`) |
| r6 | `-p` | `say hi` from `r6a` with `--add-dir r6b`; both roots have hooks and an `env` key |
| r6t | TUI | the same layout, no prompt typed |
| r3d | TUI | fresh untrusted `%TEMP%` folder: trust dialog, external-imports dialog, idle, `say hi`, `/exit` |
| r5 | TUI | `--resume` of r3d's conversation, no prompt typed |
| r4a | TUI | `--permission-mode plan`; plan approval rejected with Esc |
| r4e | TUI | `--permission-mode plan`; plan written, approval accepted with `1` |
| r4b-approved | TUI | `--permission-mode default`; Bash permission prompt approved (the Down keys meant for "No" did not register) |
| r4b | TUI | Bash permission prompt denied with `4` (No), then a second one cancelled with Esc |
| r4f | TUI | `AskUserQuestion` dismissed with Esc |
| r4c | TUI | Esc while text streams |
| r4d | TUI | Esc while a Bash tool runs (`ping -n 40 127.0.0.1`) |

Three earlier TUI launches died at the trust dialog before any prompt: the driver chose "No, exit", the dialog's default. One `-p` run was repeated because a log-format bug swapped fields. That makes 16 launches, 12 of them with a prompt.

## Q1. A `--settings` file's hooks run beside the repo's hooks

They do. `--settings`, `.claude/settings.json` and `.claude/settings.local.json` each fired their own hook for every event. The user layer was not run (see the end of this section).

r1 (`-p`), first field the epoch ms, then the layer's label:

```
1792168137555 repo pid=56712 argc=3 event=SessionStart ...
1792168137705 local-node pid=50660 argc=4 event=SessionStart ...
1792168137827 inject pid=40988 argc=3 event=SessionStart ...
1792168143551 repo pid=51484 argc=3 event=UserPromptSubmit ...
1792168143617 inject pid=36572 argc=3 event=UserPromptSubmit ...
1792168143653 local-node pid=37068 argc=4 event=UserPromptSubmit ...
1792168147226 repo ... event=Stop ...   1792168147276 local-node ... event=Stop ...   1792168147375 inject ... event=Stop ...
1792168149165 repo ... event=SessionEnd ...   1792168149166 inject ... event=SessionEnd ...   1792168149216 local-node ... event=SessionEnd ...
```

The TUI shows the same thing. In r4a, all three layers logged `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PermissionRequest` and `Notification`, 21 lines for 7 events, and all three logged `SessionEnd`.

**User layer: unverified.** `CLAUDE_CONFIG_DIR` is honoured: `claude auth status` with it set printed `"configDirectory": "X:\\dev\\rustling-tulip.wt\\hs-0\\.tmp\\spike\\cfg"`. But it also printed `"loggedIn": false`. A scratch config would have needed a copy of the OAuth credentials or an API key, and the orchestrator ruled both out.

The settings docs list the user file as the fifth precedence level, below `claude --settings`. They say "When you set the same list key ... in more than one file, Claude Code combines the lists instead of picking one" (https://code.claude.com/docs/en/settings, "Lists merge instead of overriding"). The design's Sources also quote the hooks page: "Hook entries merge across settings levels rather than replacing each other".

`--setting-sources` turns the user layer off without turning `--settings` off: every run here got its `inject` hooks with `user` left out.

**For the design.** HS.5 stands as written: the injected file adds to the repo's and the local file's hooks and replaces none of them.

## Q2. Async exec-form hooks run a `.exe`, with the event JSON on stdin

They do. The same holds for `node.exe` with a `.mjs` in `args`. Every `inject` and `repo` line above came from `hooklog.exe`, spawned with `"args"` and `"async": true`; `argc=3` is the exe plus its two args. Every `local-node` line came from `node.EXE` with `hooklog.mjs` (`argc=4`). Each line carries the event JSON read from stdin, for example r1's `SessionStart`:

```
{"session_id":"7372439f-c3cc-4e13-86fd-519d8d2e3586","transcript_path":"C:\\Users\\lefto\\.claude\\projects\\X--dev-rustling-tulip-wt-hs-0--tmp-spike-r1\\7372439f-...jsonl","cwd":"X:\\dev\\rustling-tulip.wt\\hs-0\\.tmp\\spike\\r1","hook_event_name":"SessionStart","source":"startup"}
```

Claude closed stdin after the body: `read_to_string` returned in every call.

Async hooks complete out of order. r4a's log, in file order:

```
1792168722591 repo PostToolUse
1792168722477 repo PreToolUse
```

The `PostToolUse` hook appended its line before the `PreToolUse` hook that started 114 ms earlier. In `-p` mode, the async `Stop` and `SessionEnd` hooks still ran before the process exited (r1, above).

**For the design.** HS.3 and HS.5 stand. Ordering by a start-time header (`X-RT-Hook-Time`, HS.4) is needed, not just defensive.

## Q3. Hooks from `--settings` wait for workspace trust

They wait. No hook fires while the trust dialog is up, or while the external-imports dialog that follows it is up. `SessionStart` fires the moment the input box appears after the last dialog.

r3d, in a fresh `%TEMP%` folder, driver notes (epoch ms) interleaved with the hook log:

```
[1792168595244] matched /Yes, I trust this folder/
[1792168603246] HOOKS 8 s into the trust dialog (0):
[1792168604056] send Enter (trust: yes)
[1792168605820] SCREEN external imports dialog:   "Allow external CLAUDE.md file imports?"
[1792168605820] HOOKS while the external-imports dialog is up (0):
[1792168605820] send Enter (imports: no)
[1792168607578] input box ready
  1792168607620 repo SessionStart
  1792168607624 inject SessionStart
```

The external-imports dialog appears because `~/.claude/CLAUDE.md` imports a file outside the folder. `--setting-sources` does not keep the user's CLAUDE.md out.

Trust is not inherited the way it looked. `~/.claude.json` lists `X:/dev` with `hasTrustDialogAccepted` true, yet every fresh `git init` folder under `X:\dev\rustling-tulip.wt\hs-0\.tmp\spike\` showed the trust dialog (r4a, r4b, r4c, r6t). A session in a new rustling-tulip worktree, under `%LOCALAPPDATA%\leftos\rustling-tulip\data\worktrees\`, should be expected to show it too. This spike did not run that path.

**For the design.** "Coexisting with the heuristic" lists "an untrusted folder" among sessions whose hooks never run. More precisely, a session in an untrusted folder stays heuristic-driven until the user accepts the dialog in the terminal, then turns hook-driven at `SessionStart`. The heuristic has to cover the dialogs: today's prompt regexes and the `AwaitingInput` attention cover the trust dialog's "❯ No, exit" choice list.

## Q4. Events for a plan approval, a denied permission and an Esc interrupt

Each of the three prompt kinds fires `PreToolUse`, then `PermissionRequest`, then a `permission_prompt` `Notification` about 6 s later. These are plan approval (`ExitPlanMode`), a question (`AskUserQuestion`) and a tool permission. Accepting one fires `PostToolUse`. Rejecting or dismissing one fires nothing at all, and an Esc interrupt fires nothing either: no `Stop`, no `PostToolUseFailure`, no `PermissionDenied`.

Timelines (epoch ms; `inject` layer):

| Run | Events while the prompt is up | User action | Events after |
| - | - | - | - |
| r4a plan, rejected | `PreToolUse ExitPlanMode` 727153, `PermissionRequest ExitPlanMode` 727449, `Notification` 733415 | Esc at 847083 | none until `SessionEnd` 856082 |
| r4e plan, approved | `PreToolUse ExitPlanMode` 407434, `PermissionRequest` 407647, `Notification` 413579 | `1` at 415778 | `PostToolUse ExitPlanMode` 415830, `PreToolUse PowerShell` 418173, `PostToolUse` 419238, `Stop` 420769, `SubagentStop` 422839 |
| r4b-approved Bash | `PreToolUse Bash` 913583, `PermissionRequest` 913660, `Notification` 919994 | Enter (Yes) at 925354 | `PostToolUse` 927017, `Stop` 928652, `SubagentStop` 930199 |
| r4b Bash, No | `PreToolUse Bash` 961832, `PermissionRequest` 961952, `Notification` 968006 | `4` (No) at 972147 | none in 10 s; the TUI shows "Interrupted · What should Claude do instead?" |
| r4b Bash, Esc | `PreToolUse Bash` 985788, `PermissionRequest` 985875 | Esc at 988009, before the 6 s notification | none in 10 s |
| r4f question | `PreToolUse AskUserQuestion` 461385, `PermissionRequest AskUserQuestion` 461477, `Notification` 467445 | Esc at 468515 | none in 10 s; the TUI shows "User declined to answer questions" |
| r4d Esc mid-tool | `UserPromptSubmit` 359387, `PreToolUse Bash` 361844 | Esc at 367277 | none in 12 s; "Interrupted · What should Claude do instead?" |
| r4c Esc mid-text | `UserPromptSubmit` 156131 | Esc at 158587 while the story streamed | none in 10 s; the next prompt fired `UserPromptSubmit` 169466 and `Stop` 171718 |

No late `PostToolUse` or `PostToolUseFailure` arrived after either interrupt within the 10-12 s watched. Orca's late-event case (Prior art) did not show here, but these turns were short.

Payloads, trimmed of the common fields:

- `PreToolUse` for `ExitPlanMode` carries the plan: `"tool_input": {"plan": "1. Open PowerShell ...\n2. Run `echo hello` ...\n", "planFilePath": "C:\\Users\\lefto\\.claude\\plans\\plan-how-to-print-toasty-thunder.md"}`. `PermissionRequest` carries the same `tool_input`.

  When the model called `ExitPlanMode` without writing a plan (r4a), both carried `"tool_input": {}` and the TUI said "No plan found". On approval, `PostToolUse` has `"tool_input": {}` with the plan moved to `"tool_response": {"plan": ..., "isAgent": false, "filePath": ...}`, and `permission_mode` has already changed from `plan` to `auto`.
- `PreToolUse` and `PermissionRequest` for `AskUserQuestion` both carry `"tool_input": {"questions": [{"question": "Do you prefer tea or coffee?", "header": "Drink", "options": [{"label": "Tea", "description": "You prefer tea."}, {"label": "Coffee", "description": "You prefer coffee."}], "multiSelect": false}]}`.
- `PermissionRequest` for Bash: `{"permission_mode": "default", "hook_event_name": "PermissionRequest", "tool_name": "Bash", "tool_input": {"command": "echo hi > out.txt", "description": "Write \"hi\" into out.txt in the working directory"}, "permission_suggestions": [{"type": "addDirectories", ...}]}`. `PermissionRequest` has no `tool_use_id`; `PreToolUse` does.
- `Notification`: `{"hook_event_name": "Notification", "message": "Claude needs your permission", "notification_type": "permission_prompt"}` for Bash and for `AskUserQuestion`, and `"message": "Claude Code needs your approval for the plan"` for `ExitPlanMode`. A question is a `permission_prompt`, not an `elicitation_dialog`. The notification came 6.0-6.3 s after `PermissionRequest` each time, and only if the prompt was still up.
- `Notification` `idle_prompt`: `"message": "Claude is waiting for your input"`, 60.1 s after `Stop` (r4c: `Stop` 1792169026051, `Notification` 1792169086192).
- `Stop`: `{"permission_mode": "auto", "effort": {"level": "medium"}, "hook_event_name": "Stop", "stop_hook_active": false, "last_assistant_message": "Hi! What can I help you with in ...?", "background_tasks": [], "session_crons": []}`.
- `UserPromptSubmit`: `{"prompt_id": "50d478b6-...", "permission_mode": "auto", "hook_event_name": "UserPromptSubmit", "prompt": "say hi"}`. Every event of a turn carries that turn's `prompt_id`.
- `SubagentStop` fired 1.5-2 s after every TUI `Stop`, with `"agent_type": ""` and `"last_assistant_message": "cat out.txt"`. That is the CLI's own prompt-suggestion agent, whose suggestion the input box showed as placeholder text. It does not come in `-p`.
- `StopFailure` never fired; no run hit an API error. `PermissionDenied` never fired; the docs scope it to auto mode's classifier.
- A Bash call that the CLI's own validator refused ("Blocked: standalone sleep 30") fired no `PreToolUse` and no `PostToolUseFailure` (r4c's first turn).

**For the design.** This contradicts the mapping table and the arbitration rule in several places:

1. `PermissionRequest` also fires for `AskUserQuestion` and `ExitPlanMode`, 90-300 ms after their `PreToolUse`. Mapped as the table says (`Permission { tool_name, summary }`), it would overwrite the `Question` or `PlanApproval` pending input the `PreToolUse` just set. HS.1 must map `PermissionRequest` by `tool_name`: `AskUserQuestion` to `Question`, `ExitPlanMode` to `PlanApproval` (same `tool_input`), anything else to `Permission`.
2. Leaving `AwaitingInput` has no event when the user says no. Rejecting a plan, denying or cancelling a permission and dismissing a question all end the turn silently. Under "Coexisting with the heuristic" (hooks own all of `AwaitingInput`, and the heuristic may only demote `Working`), such a session stays `AwaitingInput` until the next prompt. HS.6 needs an exit, for example:
   - the heuristic may also demote a hook-reported `AwaitingInput` to `Idle` once its prompt regex no longer matches and the PTY has been quiet for `IDLE_AFTER`;
   - or the input side: the daemon sees the user's keystroke while a prompt is pending, and settles to `Idle` if no `PostToolUse` or `PreToolUse` follows within a short window (Orca's interrupt-intent pattern in Prior art).
3. Esc mid-tool and Esc mid-text both leave a hook-reported `Working` with no closing event, as the docs say for `Stop`. The design's silence demotion is required, not a fallback for rare cases.
4. `PlanApproval { plan }` must allow a missing plan: `tool_input` can be `{}`.
5. `PostToolUse` for `ExitPlanMode` is the approval signal, and its `permission_mode` is already the new mode. HS.7's `permission_mode` check should read `UserPromptSubmit` only, as the design says, not tool events.
6. Do not subscribe to `SubagentStop`. If it is ever added, an empty `agent_type` must not count as work: it fires after every `Stop`.
7. The `Notification` matcher in "The injected settings" is right, but its `permission_prompt` mostly duplicates `PermissionRequest` 6 s late. Its value is the backstop the design names, plus `idle_prompt`.

## Q5. `SessionStart` fires before the first prompt

It does, as the input box appears and before anything is typed. `source` is `startup` for a fresh session and `resume` for `--resume`. With `--resume`, `session_id` is the resumed conversation's own id, unchanged.

r3d, nothing typed yet (the prompt went in at 1792168615583):

```
[1792168607578] input box ready
[1792168615583] HOOKS 8 s after the input box came up, nothing typed (2):
  1792168607624 inject SessionStart
  1792168607620 repo SessionStart
{"session_id":"4938c164-1f01-4cdd-ac3e-fc1a5e2e1237", ..., "hook_event_name":"SessionStart","source":"startup","model":"claude-haiku-5-5"}
```

r5, `claude ... --resume 4938c164-1f01-4cdd-ac3e-fc1a5e2e1237`, nothing typed:

```
[1792168666576] input box ready
  1792168666929 inject SessionStart
      1 "hook_event_name":"SessionStart","source":"resume"
      2 "session_id":"4938c164-1f01-4cdd-ac3e-fc1a5e2e1237"
```

The TUI's `SessionStart` also carries `scratchpad_dir` and `model`. Neither `SessionStart` nor `SessionEnd` carries `permission_mode`. `SessionEnd`'s `reason` was `prompt_input_exit` after `/exit` and `other` at the end of a `-p` run.

**For the design.** "Mapping events to status" says `SessionStart` with `source` `resume` "carries a new conversation id". For `claude --resume <id>` it carries the same id, so HS.7's update is a no-op there; `clear`, `fork` and an in-session `/resume` were not run.

"Prompt injectors" says `SessionStart` "fires before the input box is known to accept keystrokes". Measured, it fired 42-46 ms after the input box drew in r3d. In r6t it fired before the driver's next screen check saw the box. It also waits for every startup dialog. That makes it a better earliest-start signal for `InjectorStartup::AgentTui` than the quiet period alone, at least in a hook-driven session. Whether to use it is a choice for a later step, not a correction HS.0 forces.

## Q6. `--add-dir` loads settings from the primary cwd only

Only the primary cwd's settings load. Hooks, and the `env` key, in an `--add-dir` root's `.claude/settings.json` and `.claude/settings.local.json` did not load, in `-p` or in the TUI.

r6 (`-p`): `r6a` (the cwd) has `primary` hooks and `env: {SPIKE_PRIMARY: 1}`. `r6b` (`--add-dir`) has `addroot` and `addroot-local` hooks and `SPIKE_ADDROOT` / `SPIKE_ADDROOT_LOCAL` env keys.

```
1792168222056 inject pid=47700 argc=3 event=SessionStart env=[SPIKE_PRIMARY=1] ...
1792168222137 primary pid=59612 argc=3 event=SessionStart env=[SPIKE_PRIMARY=1] ...
1792168224294 primary ... event=UserPromptSubmit env=[SPIKE_PRIMARY=1] ...   1792168224294 inject ... event=UserPromptSubmit ...
1792168225750 primary ... event=Stop env=[SPIKE_PRIMARY=1] ...   1792168225762 inject ... event=Stop ...
```

r6t (TUI, same layout) logged `inject SessionStart`, `primary SessionStart`, `primary SessionEnd` and `inject SessionEnd`, all with `env=[SPIKE_PRIMARY=1]`, and no `addroot` line. A `DirectoryAdded` hook in r6's injected file did not fire for the `--add-dir` given at launch.

**For the design.** No step changes, since the injected hooks travel by `--settings`. A workspace session gets only `members[0]`'s repo hooks and settings, and that is already true today. HS.11's architecture note could say so where it describes workspace sessions.

## Design changes these answers call for

- **HS.1 mapping:** map `PermissionRequest` by `tool_name`: `AskUserQuestion` to `Question`, `ExitPlanMode` to `PlanApproval`, anything else to `Permission`. As written, it overwrites the richer pending input set 0.1-0.3 s earlier. `PlanApproval`'s plan is optional (`tool_input` can be `{}`).
- **HS.6 arbitration:** add an exit from hook-reported `AwaitingInput`. A rejected plan, a denied or cancelled permission and a dismissed question fire no event at all, so "hooks own all of `AwaitingInput`" leaves the session stuck until the next prompt. Options: let the silence demotion also cover `AwaitingInput` once the prompt regex stops matching, or use the input-side settle from Prior art. This needs a ruling before HS.6.
- **HS.6 arbitration:** the `Working` to `Idle` silence demotion is confirmed necessary. Esc mid-tool and Esc mid-text both fire nothing.
- **"Coexisting with the heuristic":** an untrusted folder is heuristic-driven only until trust is accepted. A fresh `git init` folder under a trusted parent still showed the dialog.
- **"Mapping events to status" / HS.7:** `SessionStart` `resume` from `claude --resume <id>` keeps the same `session_id`. Reword "carries a new conversation id" to cover `clear` and `fork` (unverified here) and say `--resume` keeps it.
- **"Prompt injectors":** `SessionStart` fires within about 50 ms of the input box appearing, after every startup dialog. The text saying it comes before the input box accepts keys is wrong for 2.1.301; using it as the injector's start signal is optional.
- **Not to subscribe:** `SubagentStop` (the prompt-suggestion agent fires it after every `Stop`, `agent_type: ""`).
- **No change:** `--settings` layering with the repo and local files (Q1), async exec-form `.exe` and `node.exe` hooks with JSON on stdin (Q2), and out-of-order completion, which justifies `X-RT-Hook-Time` ordering (HS.4).
