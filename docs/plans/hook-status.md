# Hook-reported agent status

Delivered; its durable decisions now live in the docs below (`docs/architecture.md`, `CLAUDE.md`, `README.md`), and this file stays while RT-25 links it.

Take a session's working / awaiting input / idle status from the agent's own hooks instead of guessing it from PTY output, and keep `crates/daemon/src/pty_state.rs`'s heuristic as the fallback. Claude Code is hooked through a settings file, and an opted-in Codex session through the `-c` tables its spawn passes. Index line: [MAIN.md](./MAIN.md), Wave 2. Background and VelaTerm's prior art were in [borrowed-ideas.md](./borrowed-ideas.md) until that entry was removed on delivery.

Items that build on this one: "Spoken alerts when an agent waits" ([spoken-alerts.md](./spoken-alerts.md)) needs the waiting signal plus the pending `AskUserQuestion` questions and permission prompts on the daemon; the "Needs You" view ([borrowed-ideas.md](./borrowed-ideas.md)) groups sessions by this status; the mobile app's MA6 ([mobile-app.md](./mobile-app.md)) shows live status and answers prompts.

## Problem

`pty_state.rs` infers status from the byte stream: prompt regexes (`matches_prompt`: "Do you want to", a `❯` followed on its line by an option number such as `1.`, `AskUserQuestion` framing), an output-versus-input volume budget, a terminal-title heartbeat and a 2.5 s idle timeout (`IDLE_AFTER`). It is version-fragile by design: a TUI wording change breaks `AwaitingInput`, a quiet tool run can read as `Idle`, and it knows nothing about *what* the agent is asking. The alerts item needs the question text and the agent's last message, which no byte heuristic can give reliably.

## Sources

All Claude Code and Codex facts below are from the current docs, fetched as Markdown (`<page>.md`).

- **Handler types.** Hooks may be `command`, `http`, `mcp_tool`, `prompt` or `agent`. An `http` hook POSTs the event's JSON input with `Content-Type: application/json`; its `headers` values interpolate `$VAR` / `${VAR}` only for names listed in `allowedEnvVars`. https://code.claude.com/docs/en/hooks#http-hook-fields
- **HTTP failure is non-blocking.** Non-2xx, connection failure and a non-JSON 2xx body are all non-blocking errors; "execution continues". A timeout cancels the hook. https://code.claude.com/docs/en/hooks#http-response-handling
- **Hooks block by default.** "By default, hooks block Claude's execution until they complete." `"async": true` runs a hook in the background and "is only available on `type: "command"` hooks"; async hooks cannot return decisions. Default timeout is 600 s for `command` and `http`. https://code.claude.com/docs/en/hooks#run-hooks-in-the-background, https://code.claude.com/docs/en/hooks#common-fields
- **Exec form on Windows.** A command hook with `args` runs without a shell; on Windows `command` must resolve to a real `.exe`. https://code.claude.com/docs/en/hooks (exec form note under "Command hook fields")
- **`--settings`** takes "Path to a settings JSON file or an inline JSON string. Values you set here override the same keys in your `settings.json` files for this session. Keys you omit keep their file-based values." https://code.claude.com/docs/en/cli-reference
- **Merging.** `--settings` sits second in precedence, below managed settings and above local, project and user; it merges "by the same rules as the other levels" (https://code.claude.com/docs/en/settings#settings-precedence). Lists merge instead of overriding (https://code.claude.com/docs/en/settings#lists-merge-instead-of-overriding), and "Hook entries merge across settings levels rather than replacing each other" (https://code.claude.com/docs/en/hooks, "Hook locations"). So an injected hook adds to the user's and the repo's hooks; it does not replace them.
- **What can switch hooks off.** `disableAllHooks`, `allowManagedHooksOnly`, and `allowedHttpHookUrls` (when defined at any level, an HTTP hook runs only if its URL matches the merged allowlist). https://code.claude.com/docs/en/hooks#disable-or-remove-hooks, https://code.claude.com/docs/en/hooks ("Hook locations")
- **Workspace trust.** In an interactive session Claude Code "holds back hooks from every settings file, including your own `~/.claude/settings.json`, until you accept the workspace trust dialog". https://code.claude.com/docs/en/hooks#workspace-trust
- **Common input.** Every event carries `session_id` (Claude's conversation id), `transcript_path`, `cwd`, `permission_mode`, `hook_event_name`, and more. https://code.claude.com/docs/en/hooks#common-input-fields
- **Events used here.** `SessionStart` (`source`: `startup`, `resume`, `clear`, `compact`, `fork`), `UserPromptSubmit`, `PreToolUse` / `PostToolUse` / `PostToolUseFailure` (`tool_name`, `tool_input`, `tool_use_id`), `PermissionRequest` (`tool_name`, `tool_input`, `permission_suggestions`; fires the moment Claude asks, while the `permission_prompt` notification waits about six seconds), `Notification` (`message`, `title`, `notification_type`: `permission_prompt`, `idle_prompt` about 60 s after a reply, `elicitation_dialog`, …), `Elicitation` (an MCP server asks for input), `Stop` (`last_assistant_message`, `background_tasks`; "Does not run if the stoppage occurred due to a user interrupt"), `StopFailure` (API errors), `SessionEnd` (`reason`). https://code.claude.com/docs/en/hooks#sessionstart, https://code.claude.com/docs/en/hooks#permissionrequest, https://code.claude.com/docs/en/hooks#notification, https://code.claude.com/docs/en/hooks#stop, https://code.claude.com/docs/en/hooks#sessionend
- **`AskUserQuestion` input.** `PreToolUse` matches `AskUserQuestion`; its `tool_input.questions` is an array of `{question, header, options[{label}], multiSelect}`. `ExitPlanMode`'s input gets the plan text and `planFilePath` injected. https://code.claude.com/docs/en/hooks#askuserquestion
- **Codex.** Codex has `SessionStart`, `SessionEnd`, `PreToolUse`, `PermissionRequest`, `PostToolUse`, `UserPromptSubmit`, `Stop`, `Interrupt` and more; only command handlers run; hooks are read from `~/.codex/hooks.json`, `~/.codex/config.toml` and the repo's `.codex/`.

  A `-c hooks.<Event>=…` value carries a hook table for one run, which is how the opt-in injects them (HS.10); no file is written. https://learn.chatgpt.com/docs/hooks (redirected from https://developers.openai.com/codex/hooks)

## The daemon today

Re-read this section against `pty_state.rs` and `session.rs` before HS.6: the status now persists in the sidecar and a reattached session restores it (2c16f41, b378bcc).

- **Spawn.** `server/spawn.rs::spawn_interactive_session` builds argv through `AgentBackend::build_interactive_args` (`crates/daemon/src/agents/claude.rs`, `codex.rs`, `cursor.rs`), with the cross-agent `CommonSpawnFields` from `agents/mod.rs`. Claude gets `--session-id <uuid>` (new) or `--resume <id>` (recovery).

  The argv is logged at `info!` and handed to the tracer client in a `PtySpawnSpec` with `env: merged_env(&cfg.extra_env)`; the tracer runs the child from its content-addressed copy under the binaries cache (`binary_cache.rs`).

  Because the injected hook settings file, or a Codex spawn's `-c` hook command, names that copy as its hook command, the cached tracer path is resolved before the argv is built (`tracer_client::cached_tracer_exe`), and the spawn hands that same path to `tracer_client::spawn_with_tracer`, so the commands and the running supervisor name one cache copy.
- **HTTP.** `build_loopback_router` in `server.rs` serves `/ws`, `/health`, `/shutdown` (bearer auth against the in-memory `auth_token`, compared with `secret::constant_time_eq`) and `/hook/{session_id}` on the loopback listener; the tailnet listener has its own router, `build_tailnet_router`, with `/ws` and `/health` only. The loopback port is ephemeral (`127.0.0.1:0`) and changes on every daemon start; `daemon.json` carries the current port and token.
- **Status.** `protocol::SessionStatus` is `Spawning | Idle | Working | AwaitingInput | Stopped | Error` on `SessionSnapshot.status`. Neither it nor `AttentionReason` has a `#[serde(other)] Unknown`, so neither can gain a variant without a protocol bump (CLAUDE.md "When to bump").

  `pty_state::watch` writes `rec.status` and sends an `AttentionEvent` (forwarded as `DaemonMessage::Attention`) on each transition into `AwaitingInput`. Today `AwaitingInput` means "a permission or question prompt is up" and `Idle` means "the turn is over".
- **Headless.** `claude --print --output-format stream-json` sessions are parsed in `ClaudeBackend::handle_headless_line`; they already get structured status and need no hooks.

## Design

### Transport: a hook shim that reads `daemon.json` on every call

The injected hooks are **command hooks in exec form, `async: true`**, running a small subcommand of the session's own tracer binary: `<cached rt-tracer.exe> hook --session <rt session id> --config-dir <dir>`. The shim reads the event JSON from stdin, reads `daemon.json` from the config dir for the current port and auth token, and exits 0 whatever happens.

On one connection it first sends `GET /proof` with a random `X-RT-Challenge` and goes on only when the listener answers `204` with an `x-rt-proof` that is the HMAC of the challenge under a key derived from this run's token (RT-208, `tracer_protocol::daemon_proof`); a listener that cannot prove it is the daemon is sent nothing more. Then it POSTs the body verbatim to `http://127.0.0.1:<port>/hook/<rt session id>` with `Authorization: Bearer <token>` and an `X-RT-Hook-Time` header (nanoseconds, taken when the shim starts). It is hand-written HTTP/1.1 over `std::net::TcpStream` with a 2 s budget for the whole exchange; the tracer depends on `getrandom` for the challenge and on `tracer-protocol`'s `hmac`/`sha2` for the proof.

Why not `type: "http"` hooks posting straight to the daemon, as VelaTerm does:

- The daemon's port changes on every start, but tracer-backed sessions outlive the daemon. A URL baked into a running Claude's settings would point at a dead port after the first daemon restart, for the rest of the session. The shim looks the port up on each call.
- HTTP hooks cannot be `async`, so every `PreToolUse` would block Claude on a round trip to the daemon (up to the timeout when the daemon is wedged).
- A user's `allowedHttpHookUrls` silently blocks HTTP hooks.

Why the tracer binary hosts the shim: the running tracer holds its cached copy open, and `binary_cache::gc` keeps every copy a live sidecar references, so the shim's path stays valid for exactly the session's lifetime, across rebuilds and installer upgrades. The tracer's `main` dispatches on a leading `hook` argument before `Cli::parse`, so the daemon's existing tracer invocation is unchanged and the tracer ABI (`docs/tracer-abi.md`) is untouched.

Cost: one short process per hook event, off Claude's critical path (async). Async hooks finish out of order (the spike saw a `PostToolUse` land before the `PreToolUse` that started 114 ms earlier), so ordering is required: the daemon orders events by `X-RT-Hook-Time` and drops one at or before the last it applied for that session, or one with no usable time.

### The injected settings

At spawn the daemon creates `sessions/<id>/` and writes `hook-settings.json` there, then passes `--settings <that path>`; that directory is the one the sidecar's `meta.json` later joins. A file, not inline JSON: inline JSON would cross the tracer's Windows command line with nested quoting, and argv is logged. The file holds only a `hooks` object, so every other key keeps the user's value, and hook entries add to the user's and the repo's hooks. Events and matchers:

| Event (matcher) | Why |
| - | - |
| `SessionStart` | Liveness: the first event proves hooks run in this session; also carries the conversation id |
| `UserPromptSubmit` | Turn started |
| `PreToolUse` (`AskUserQuestion\|ExitPlanMode`) | A question or plan approval is up, with its text |
| `PostToolUse`, `PostToolUseFailure` (all) | A tool finished: a granted permission or an answered question is over |
| `PermissionRequest` (all) | A permission prompt is up, immediately |
| `Notification` (`permission_prompt\|elicitation_dialog\|elicitation_url_dialog\|idle_prompt`) | Backstop for prompts `PermissionRequest` misses (sandbox network requests) |
| `Elicitation` | An MCP server asks for input |
| `Stop`, `StopFailure` | Turn over, with the last assistant message |
| `SessionEnd` | Conversation switched or ended |

The file is removed with the rest of `sessions/<id>/` on a graceful stop. It contains no secret: the token is read from `daemon.json` at call time.

`SubagentStop` is not subscribed: the CLI's own prompt-suggestion agent fires it 1.5-2 s after every interactive `Stop`, with an empty `agent_type`, so it says nothing about work. Should it ever be added, an empty `agent_type` must not count as work.

Only the primary cwd's settings load: hooks and `env` in an `--add-dir` root's `.claude/settings.json` do not. A workspace session therefore gets only `members[0]`'s repo hooks, and the injected `--settings` file is unaffected.

### Auth

The shim presents the daemon's own token from `daemon.json`, read fresh on each call, so a restarted daemon's new token just works. A per-session token would add nothing: the agent runs as the same user and can read `daemon.json` and its own environment anyway, and a per-session token would have to be persisted in `meta.json` to survive a daemon restart.

The `POST /hook/{session_id}` route is mounted on the loopback router only (`build_loopback_router`); the tailnet router (`build_tailnet_router`) never serves it. Axum 0.8 names the parameter in braces (`{session_id}`, not `:session_id`). The route answers `204` at once for any authenticated request (an unknown session id is logged at `debug!` and ignored) and `401` otherwise; the shim ignores both.

### Mapping events to status

A new `crates/daemon/src/hook_status.rs` parses the payload tolerantly (unknown events and fields are ignored) into a `HookSignal { status, pending }`:

| Event | Status | Pending input |
| - | - | - |
| `SessionStart` | unchanged (`Idle` on `startup`) | cleared |
| `UserPromptSubmit`, `PreToolUse` (other tools), `PostToolUse`, `PostToolUseFailure` | `Working` | cleared |
| `PreToolUse` `AskUserQuestion` | `AwaitingInput` | `Question { questions }` |
| `PreToolUse` `ExitPlanMode` | `AwaitingInput` | `PlanApproval { plan }` (truncated; the plan is optional, `tool_input` can be `{}`) |
| `PermissionRequest` `AskUserQuestion` | `AwaitingInput` | `Question { questions }` (same `tool_input`) |
| `PermissionRequest` `ExitPlanMode` | `AwaitingInput` | `PlanApproval { plan }` (same `tool_input`) |
| `PermissionRequest` (other tools) | `AwaitingInput` | `Permission { tool_name, summary }` (`command`, `file_path` or the first 200 chars of `tool_input`) |
| `Notification` `permission_prompt` / `elicitation_*` | `AwaitingInput` | kept, else `Other { message }` |
| `Elicitation` | `AwaitingInput` | `Other { message }` naming the MCP server |
| `Notification` `idle_prompt`, `Stop`, `StopFailure` | `Idle` | cleared; an `idle_prompt` is ignored while a hook-reported prompt is pending |
| `SessionEnd` | unchanged | cleared |

`Stop` maps to `Idle`, keeping today's meaning of the two states (see open question 3). The daemon keeps `Stop`'s `last_assistant_message` (capped at 8 KiB) on the session record, off the wire, for the alerts summarizer. `StopFailure` adds a `recent_actions` line. Hook transitions go through the same transition path as the heuristic, so `Attention { AwaitingInput }` still fires on entry to `AwaitingInput` and nowhere else.

`PermissionRequest` fires for `AskUserQuestion` and `ExitPlanMode` too, 90-300 ms after their `PreToolUse`, so it is mapped by `tool_name`; mapped as one `Permission` row it would overwrite the richer pending input. `PostToolUse` for `ExitPlanMode` is the approval signal.

A subagent's tool calls fire the same hooks while the main agent may be waiting on the user. Its events carry a non-empty `agent_id` (`agent_type` alone also marks a main agent run with `--agent`, so it is not the marker), and they never end or replace a pending main-agent prompt; a subagent `PermissionRequest` raises a prompt only when none is pending.

`SessionStart` with `source` `clear` or `fork` carries a new conversation id (`clear` and `fork` still unverified against a live CLI); the daemon writes the record's `claude_session_id` whenever a `SessionStart` reports a different one, with no source gate, so `claude --resume <id>` is a no-op because the id matches. The id is mirrored into the sidecar, so a reattach keeps it, and the recover dialog (`history.rs`) resumes the conversation the session was really in.

### Coexisting with the heuristic

Each session starts heuristic-driven. The first mapped hook event that passes the ordering check makes it **hook-driven**, reset only on a daemon restart until the next event arrives; a malformed or unmapped event changes nothing. A plain shell's `SessionEnd` is the one thing that clears the flag, handing its status back to the shell classifier. While hook-driven:

- Hooks own every promotion into `AwaitingInput` and `Working`; the heuristic's classifier stops promoting and stops sending attention events. Both watchers honour the flag: the plain-shell watcher never lets its output classifier overwrite a hook-reported status.
- The heuristic keeps two demotions, in agent sessions and plain shells alike. The silence is counted from the later of the last PTY output and the last hook-reported status or stamp change, so a hook status that lands while the PTY is quiet still ends, and the demotion re-reads the record before acting, so a hook status written but not yet broadcast is never demoted at an old deadline. First, `Working` to `Idle` after `IDLE_AFTER` of PTY silence: `Stop` does not fire on a user interrupt (Esc), and Claude's TUI repaints at least once a second while it works, so silence during hook-reported `Working` means the turn was interrupted. The spike confirmed Esc mid-tool and Esc mid-text fire no closing event, so this demotion is required.- Second, `AwaitingInput` to `Idle` once the prompt's regex stops matching and the screen goes quiet for `IDLE_AFTER`. Rejecting a plan, denying or cancelling a permission and dismissing a question fire no hook, so a prompt dismissed from another client would otherwise leave the session `AwaitingInput` until the next prompt. A plain shell's watcher reads the screen for the same matcher, so a hook-reported `AwaitingInput` there drops to `Idle` once the prompt is off screen and the PTY has been quiet for `IDLE_AFTER`. Either silence demotion in a plain shell also clears `hook_driven`, so the shell's own classifier runs again until the next mapped hook event.
- The owner's own input also leaves `AwaitingInput`: any key the owner sends to a session while it shows a pending prompt clears `AwaitingInput` at once and clears `pending_input`. A later `PreToolUse` or `PostToolUse` promotes the session again, so an answered prompt that continues the turn returns to `Working`.

A session whose hooks never run (Codex without the hooks toggle, Cursor, a `disableAllHooks` or `allowManagedHooksOnly` user, an older `claude`) stays heuristic-driven with no change in behaviour. Claude needs no setting to turn the feature off; Codex's hooks are off by default and opt in through the Settings "Report status from Codex hooks" toggle.

An untrusted folder is heuristic-driven only until the user accepts the trust dialog in the terminal, then turns hook-driven at `SessionStart`. No hook fires while the trust dialog or the external-imports dialog that follows it is up. The prompt regexes do not match the trust dialog's "❯ No, exit" list (its option lines carry no number), so the dialog is not read as a prompt until RT-74 covers it. A fresh folder under a trusted parent still shows the dialog, and a new worktree should be expected to show it too (unverified for the worktrees path).

### Protocol changes

Additive only; `supported` does not change.

- `SessionSnapshot` gains `#[serde(default)] pending_input: Option<PendingInput>`. The sidecar's `meta.json` carries it too, as an optional `pending_input` member beside `status` (absent when nothing is pending); a reattach restores it only when the restored status is `AwaitingInput`.
- `PendingInput` is a new tagged enum (`#[serde(tag = "kind", rename_all = "snake_case")]`) with `Question { questions: Vec<PendingQuestion> }`, `Permission { tool_name, summary }`, `PlanApproval { plan }`, `Other { message }` and `#[serde(other)] Unknown`. `PendingQuestion` is `{ question, header, options: Vec<String>, multi_select }`.
- `SessionStatus` and `AttentionReason` are not touched.

### Resumed, duplicated and recovered sessions

Every interactive Claude spawn writes its own settings file under its own rt session id, whether it starts fresh, resumes (`--resume`, which keeps the conversation id), is duplicated (`duplicate_session` builds a new spawn) or is recovered through `RecoverSessions`.

A recovery into a plain shell builds its injector in `history.rs` with a `{rt-hook-settings}` slot; the spawn (`server/spawn.rs::spawn_plain_shell_session`) writes the shell session's settings file, since only it knows the rt session id, and its injector types `claude --settings <path> --resume <id>`, or drops the clause when it wrote no file.

A tracer-reattached session keeps its original settings; its shim finds the new daemon through `daemon.json`. Events fired while no daemon runs are lost; the session shows the heuristic's status until the next event.

### Headless, Codex, Cursor

- Headless Claude sessions keep the stream-json parser and get no hooks.
- Codex is hooked only when the spawn opts in (`AgentOptions::Codex::hooks`, the Settings "Report status from Codex hooks" toggle, off by default). An opted-in spawn passes one `-c hooks.<Event>=…` table for each of `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, `PostToolUse` and `Stop`, whose command handler is the same tracer shim, plus `--dangerously-bypass-hook-trust`; no settings file is written. Opting out leaves Codex heuristic-driven.
- Cursor stays heuristic-driven.

### Prompt injectors

`InjectorStartup::AgentTui` keeps its quiet-period wait.

`SessionStart` fires about 50 ms after the input box appears, and after every startup dialog, so in a hook-driven session it is a possible earlier start signal than the quiet period; using it is optional and not part of this design. `verify_mode_marker` stays as is; `UserPromptSubmit`'s `permission_mode` arrives only after the prompt is sent, so the daemon logs a `warn!` when it differs from the spawn's `--permission-mode` value, which makes a failed plan-mode entry visible in `daemon.log`. The injector's own intent is not stored anywhere, so a spawn with no flag (the Shift+Tab plan-mode preset among them) is never compared.

### Prior art to weigh before HS.3, HS.4 and HS.6

Orca (`github.com/stablyai/orca`) and ThinkTerm (`github.com/RoversX/ThinkTerm`, GPLv3: ideas only) hit problems this design will meet. Each line names the step that settles it; only the input-side exit from `AwaitingInput` (first line) is adopted.

- **Interrupts from the input side (HS.6).** HS.6 adopts the input side for leaving `AwaitingInput` only: the owner's key clears it at once, without Orca's settle window. For an Esc interrupt the silence demotion stays. Orca's client reports an input intent (`plain-escape` or `ctrl-c`) and the server re-checks status 500 ms later (`src/shared/agent-interrupt-intent.ts`, `AGENT_INTERRUPT_SETTLE_MS`).

  It ignores Ctrl+C from Codex, where it copies text or leaves a side chat, and for agents whose Esc also closes views only the agent's own evidence ends the turn. The daemon already sees every input byte, so it could do this without the client.
- **Late tool hooks after an interrupt (HS.4, HS.6).** Async `PreToolUse` / `PostToolUse` / `PostToolUseFailure` events can arrive after the user cancelled a turn and flip the session back to `Working`. Orca holds a cancelled session against those three events for 15 s, while a new `UserPromptSubmit` still reopens it (`src/shared/agent-hook-cancel-verdict-latch.ts`). Ordering by `X-RT-Hook-Time` alone does not catch this, since the late events are newer than the interrupt. The spike saw no late event within 10-12 s of an interrupt, but its turns were short.
- **Background work after `Stop` (HS.1, HS.6).** Orca keeps a Claude session working while a non-agent background task or a scheduled wakeup is pending (`claudeRunningNonAgentTask`, `claudeTaskWakeupPending` in `src/main/agent-hooks/server/server-claude-status-rules.ts`). `Stop`'s `background_tasks` field (Sources above) carries the same fact; mapping a `Stop` with running background tasks straight to `Idle` reports the session done while work continues.
- **Replayed events (HS.1, HS.10).** Orca marks events replayed after a restart (`isReplay`, used in `src/main/agent-hooks/first-work-branch-rename.ts`) so they do not count as new work, and its shim exits when an agent re-fires imported hooks (`src/main/agent-hooks/grok-replay-guard.ts`). Codex and Cursor hooks need the same check against double reporting if they ever share Claude's hook files.
- **The shim's stdin (HS.3).** The shim must read stdin even when it fails early, or the agent gets a broken pipe mid-write, and it must return after the first complete JSON value rather than waiting for EOF, since some agents never close stdin (`src/main/agent-hooks/hook-stdin-contract.ts`).
- **Which signal wins (HS.6).** ThinkTerm's arbitration (`docs/agent-status.md`): the agent's own report counts only while its process is provably alive; a matching screen rule outranks the report's state, because reports miss Esc and cancel transitions; a report older than 6 h, or more than 5 min in the future, stops counting. A `Stop` that never fires after a kill is the case this guards against.
- **Hysteresis on the fallback (HS.6).** ThinkTerm confirms leaving its waiting state, and working to idle, over 3 checks 150 ms apart capped at 700 ms, and publishes entering the waiting state and explicit reports at once (`mux/src/agent_status/hysteresis.rs`). The heuristic's own share of this is RT-74 (Steadier screen-based status); HS.6 only needs hook signals to bypass it.

## Open questions

Answered (user): Q3 `Stop` means `Idle`, as today; `AwaitingInput` is kept for real questions (AskUserQuestion, permission requests, plan approval).

Settled (orchestrator, technical): Q1 (a) async command hooks through the tracer's `hook` subcommand; Q2 (a) hooks own promotions, the heuristic only demotes (`Working` after silence, `AwaitingInput` once its prompt is gone) and the owner's key clears `AwaitingInput`; Q4 (a) the daemon's token from `daemon.json`; Q5 (a) `rt-tracer.exe hook`; Q6 (a) Codex gets hooks only through `codex -c`, per run, which HS.10 confirmed carries them.

1. **Hook transport.** (a) Recommended: async command hooks running the tracer's `hook` subcommand, which reads `daemon.json` each call. Worst case: one extra process per tool call, and a hook event lost while the daemon is down. (b) `type: "http"` hooks posting straight to the daemon, with the settings file rewritten on each daemon start. Worst case: Claude does not re-read a `--settings` file mid-session (unverified), so every session outliving a daemon restart loses hook status for good. (c) The shim writes to the tracer's named pipe and the tracer relays, buffering while the daemon is down. Worst case: a tracer ABI addition and the most code, for events a restart would otherwise drop.
2. **Hooks versus heuristic.** (a) Recommended: hooks own promotions; the heuristic may only demote `Working` to `Idle` after silence and `AwaitingInput` to `Idle` once its prompt is gone and the screen is quiet; the owner's key clears `AwaitingInput`.
  Worst case: a tool that prints nothing for 2.5 s while the TUI also stops repainting flips to `Idle` early. (b) Hooks alone. Worst case: after an Esc interrupt the dot stays `Working` until the next prompt. (c) Heuristic stays primary; hooks only fill `pending_input`. Worst case: none of the reliability gain, only the question text.
3. **What `Stop` means.** (a) Recommended: `Idle`, as today; the alerts item fires on the stop through its own signal and the kept `last_assistant_message`. Worst case: a client that wants "turn finished" has to watch `Working → Idle`. (b) `AwaitingInput`, as VelaTerm does. Worst case: every finished turn raises an `Attention` and an OS notification (P4.3), which is noisy with many sessions.
4. **Auth.** (a) Recommended: the daemon's token from `daemon.json`, read per call. Worst case: none beyond today's trust boundary (any same-user process can already read it). (b) A per-session token in the child's environment, persisted in `meta.json`. Worst case: more state to keep in step across restarts, with no attacker it keeps out.
5. **Which binary hosts the shim.** (a) Recommended: a `hook` subcommand of `rt-tracer.exe`, run from the session's cached copy. Worst case: the tracer binary grows a second job. (b) A new `rt-hook.exe`. Worst case: another binary for the binaries cache, the installer and `gc` to track per session. (c) A subcommand of `rustling-tulipd.exe`. Worst case: its cached copy is not pinned by the session, so `gc` can prune it under a live session.
6. **Codex.** (a) Recommended: HS.10 checks `codex -c` for a per-invocation hooks table and implements it only if it works; otherwise Codex stays on the heuristic. Settled: it works, and an opted-in spawn passes the tables. Worst case: Codex never gets hook status. (b) Write hooks into `~/.codex/hooks.json`. Worst case: rustling-tulip edits the user's Codex configuration, which the design forbids.

## Unverified by the spike

[The HS.0 note](../spikes/hook-status.md) leaves these open; each is checked where the step that depends on it lands.

- The user settings layer (`~/.claude/settings.json`) merging with `--settings`: the runs kept it out with `--setting-sources`, and the docs say lists merge.
- `SessionStart` for `clear`, `fork` and an in-session `/resume`: only `startup` and `claude --resume` ran (HS.7).
- `StopFailure`: no run hit an API error.
- Late `PreToolUse` / `PostToolUse` events after an interrupt: none arrived in 10-12 s, on short turns.
- The trust dialog in a new rustling-tulip worktree under the data directory: expected from other fresh folders, not run.

## Steps

- [x] **HS.0 Spike: verify the CLI behaviour this design leans on.** Done: [docs/spikes/hook-status.md](../spikes/hook-status.md).

  With a real `claude` on Windows: a `--settings` file's hooks run alongside a user hook and a repo `.claude/settings.json` hook; async exec-form command hooks run a `.exe` with `args`; hooks from `--settings` wait for workspace trust in a fresh worktree; which events fire for an `ExitPlanMode` approval, a denied permission and an Esc interrupt; whether `SessionStart` fires before the first prompt; whether `claude --add-dir` loads hooks and `settings.json` from each additional root or only from the primary cwd. Record the runs in `docs/spikes/hook-status.md`. Proof: the spike note, with each answer quoted from a run.
- [x] **HS.1 Event parsing and mapping.** New `crates/daemon/src/hook_status.rs`: tolerant payload types and `map_event(&Value) -> Option<HookSignal>` per the mapping table, with `PermissionRequest` mapped by `tool_name` (`AskUserQuestion` to `Question`, `ExitPlanMode` to `PlanApproval`, else `Permission`) and `PlanApproval`'s plan optional. Proof: table-driven unit tests, one per row, plus an unknown event, a malformed body and an `ExitPlanMode` payload with `tool_input: {}`.
- [x] **HS.2 Protocol: `pending_input`.** `crates/protocol/src/lib.rs`: `PendingInput`, `PendingQuestion`, the `SessionSnapshot` field; the daemon's snapshot builder (`session.rs`) fills it. Proof: a round-trip test and a `#[serde(other)]` test for an unknown `kind`.
- [x] **HS.3 The shim.** `crates/tracer/src/main.rs` dispatches a leading `hook` argument to a new `crates/tracer/src/hook.rs` (stdin, `daemon.json`, POST, always exit 0). Proof: tests against a local `TcpListener` fake asserting the path, bearer token, `X-RT-Hook-Time` and verbatim body; missing `daemon.json` and a refused connection both exit 0 within the budget.
- [x] **HS.4 The daemon route.** `server.rs`: loopback-only `POST /hook/{session_id}` (Axum 0.8 path syntax, in braces rather than `:session_id`), bearer check through `secret::constant_time_eq`, `204` at once, hand-off to the session's hook state with out-of-order drop by `X-RT-Hook-Time` (required: async hooks finish out of order).

  Done: the ordering check, the stamp and the state writes are one atomic registry update, so two concurrent events cannot stamp and then apply out of order; the body is read at most 1 MiB and a larger or unreadable body is treated as empty and still answered `204`.

  A malformed or unmapped event changes nothing and does not advance the stamp, so it cannot silence a later event; an event for a `Stopped` or `Error` session leaves status and pending input untouched but still advances the ordering time; and the record's runtime-only `last_hook_time` is neither in the sidecar nor on the wire, so a daemon restart resets it.

  Proof: router `oneshot` tests for `401` without or with a wrong token, `204` for an unknown session, a stale event ignored; a test that the tailnet router has no `/hook` route.
- [x] **HS.5 Settings injection.** `agents/mod.rs` `CommonSpawnFields` gains a required `hook_settings: Option<&Path>` set by every caller; `agents/claude.rs` has the `build_hook_settings` JSON builder and pushes `--settings <path>` for interactive spawns, fresh and resumed, never headless; `spawn_interactive_session` creates `sessions/<id>/`, writes the file and passes its path, and a duplicate's spawn gets its own from the same path because `duplicate_session` runs through `spawn_session`, which mints the new id downstream. A cached tracer that cannot be resolved or a failed write logs a `warn!` and spawns without hooks, and a spawn that then fails removes the file it wrote and the `sessions/<id>/` directory only when it created it. Proof: unit tests on the argv (fresh, `--resume`, headless without it), on the JSON (events, matchers, exec form, `async: true`, the cached tracer path), and on the writer (the literal `hook-settings.json` name, the Claude-only gate, and a failed spawn leaving no session dir).
- [x] **HS.6 Arbitration.** `pty_state.rs`: a per-session hook-driven flag; hook signals applied through the shared transition path (so `Attention` fires once on entry to `AwaitingInput`); the classifier silenced while hook-driven except two demotions, `Working → Idle` after silence and `AwaitingInput → Idle` once the prompt's regex stops matching and the screen is quiet for `IDLE_AFTER`, both in agent sessions and plain shells, with the deadline restarting on each hook-reported status or stamp change.

  Any owner key sent to a hook-driven session while a prompt is pending clears `AwaitingInput` and `pending_input` at once.

  Proof: paused-clock tokio tests: a hook `AwaitingInput` survives PTY output that the classifier reads as `Working`; hook `Working` plus silence demotes to `Idle`; hook `AwaitingInput` with the prompt gone from the screen plus silence demotes to `Idle`, while a still-matching prompt does not; an owner key while a prompt is pending returns the session to `Idle` at once; a session with no hook events behaves exactly as today (existing tests unchanged).
- [x] **HS.7 Conversation id and last message.** `hook_status.rs` / `session.rs`: a `SessionStart` reporting a different `session_id` updates `claude_session_id` (validated by `transcripts::is_plain_id`, no source gate), and the id is mirrored into the sidecar through `orphan::RecordMirror`, so a restart keeps it;

  `Stop` stores `last_assistant_message` trimmed and capped at 8 KiB on the record, off the wire; `StopFailure` adds a `recent_actions` line; a `permission_mode` mismatch on `UserPromptSubmit` against the spawn's `--permission-mode` logs a `warn!`. Proof: record tests after each event, a sidecar round-trip through `SessionRegistry::update`, a `RecordMirror::matches` test on the id alone, and a `history.rs` test that an ended session's entry carries the hook-updated id — each also confirmed to fail against its deliberate mutation.
- [x] **HS.8 Shell recovery path.** `history.rs`'s `shell_request` carries `claude --settings {rt-hook-settings} --resume <id>` on the shell-recovery injector (`HOOK_SETTINGS_SLOT`), since the rt session id exists only once the spawn mints it.

  `server/spawn.rs::spawn_plain_shell_session` writes `sessions/<id>/hook-settings.json` through `write_hook_settings`, spawns through `tracer_client::spawn_with_tracer` on the same cached tracer the file names, and renders the slot through `render_shell_resume`: the quoted path when the file was written, and the clause dropped — `claude --resume <id>` — when it was not or the path holds a double quote.

  A failed spawn discards the file and the `sessions/<id>/` dir it created; a natural exit leaves the file for the graceful stop that removes the whole dir.

  A recovered shell's status is hook-driven from its first mapped event: `watch_plain_shell` honours `hook_driven`, so its output classifier never overwrites a hook report; its idle tick demotes a hook-reported `Working` after `IDLE_AFTER` of silence, and a hook-reported `AwaitingInput` once the prompt is off screen; either demotion, like a `SessionEnd`, clears the flag, handing the status back to the shell classifier until the next mapped hook event.

  Proof: the `history.rs` test asserting the slot command; `server.rs` tests for the quoted path, the dropped clause with no settings or an unquotable path, the failed spawn's cleanup and the shell-only `SessionEnd` clear; and `pty_state.rs` `hysteresis_tests` for the classifier, silence and `AwaitingInput` cases.
- [x] **HS.9 End to end.** `tools/e2e/fake-claude/index.mjs` reads `--settings`, and on scripted cues runs the configured hook commands with the matching JSON on stdin (`UserPromptSubmit`, `PreToolUse AskUserQuestion`, `Stop`). A new case in `apps/native/tests/e2e_live.rs` asserts the snapshot goes `Working → AwaitingInput` with the question in `pending_input`, then `Idle`. Proof: `.\rt.ps1 native-e2e` green.
- [x] **HS.10 Codex.** `codex -c` accepts a hooks table per invocation. `agents/codex.rs` (`hook_shim_for`, `codex_hook_overrides`) injects the same shim for `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, `PostToolUse` and `Stop`, plus `--dangerously-bypass-hook-trust`, on an interactive fresh or resumed spawn whose `AgentOptions::Codex.hooks` is set.

  A spawn with hooks off adds nothing, and headless never adds them. The toggle is the Settings "Report status from Codex hooks" (off by default). Proof: `agents/codex.rs` `mod tests` (`fresh_spawn_with_hooks_registers_every_event_and_the_bypass`, `resume_spawn_with_hooks_registers_every_event_and_the_bypass`, `hooks_off_or_a_missing_shim_add_nothing`, `headless_never_adds_hooks`).
- [x] **HS.11 Docs.** CLAUDE.md's "Status detection" invariant and the `sessions/<id>/` layout line (`hook-settings.json`); a README glossary entry for "hook shim" and "hook-driven"; delete the first item of `borrowed-ideas.md`; promote the design's lasting decisions into `docs/architecture.md` (including that a workspace session loads only `members[0]`'s repo hooks, since `--add-dir` roots load none) and delete this doc. Proof: the diff. The doc stays while RT-25 links it, and its deletion is RT-25's closing step.
