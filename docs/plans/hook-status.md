# Hook-reported agent status

Take a Claude session's working / awaiting input / idle status from Claude Code's own hooks instead of guessing it from PTY output, and keep `crates/daemon/src/pty_state.rs`'s heuristic as the fallback. Index line: [plan.md](../plan.md), "Ideas borrowed from Orca and VelaTerm". Background and VelaTerm's prior art: [borrowed-ideas.md](./borrowed-ideas.md), first item.

Items that build on this one: "Spoken and phone alerts when an agent waits" ([plan.md](../plan.md)) needs the waiting signal plus the pending `AskUserQuestion` questions and permission prompts on the daemon; the "Needs You" view ([borrowed-ideas.md](./borrowed-ideas.md)) groups sessions by this status; the mobile app's MA6 ([mobile-app.md](./mobile-app.md)) shows live status and answers prompts.

## Problem

`pty_state.rs` infers status from the byte stream: prompt regexes (`matches_prompt`: "Do you want to", numbered choices plus a `❯`, `AskUserQuestion` framing), an output-versus-input volume budget, a terminal-title heartbeat and a 2.5 s idle timeout (`IDLE_AFTER`). It is version-fragile by design: a TUI wording change breaks `AwaitingInput`, a quiet tool run can read as `Idle`, and it knows nothing about *what* the agent is asking. The alerts item needs the question text and the agent's last message, which no byte heuristic can give reliably.

## Sources

All Claude Code facts below are from the current docs, fetched as Markdown (`<page>.md`).

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
- **Codex.** Codex has `SessionStart`, `SessionEnd`, `PreToolUse`, `PermissionRequest`, `PostToolUse`, `UserPromptSubmit`, `Stop`, `Interrupt` and more; only command handlers run; hooks are read from `~/.codex/hooks.json`, `~/.codex/config.toml` and the repo's `.codex/`; the docs name no per-invocation override. https://learn.chatgpt.com/docs/hooks (redirected from https://developers.openai.com/codex/hooks)

## The daemon today

- **Spawn.** `server.rs::spawn_interactive_session` builds argv through `AgentBackend::build_interactive_args` (`crates/daemon/src/agents/claude.rs`, `codex.rs`, `cursor.rs`), with the cross-agent `CommonSpawnFields` from `agents/mod.rs`. Claude gets `--session-id <uuid>` (new) or `--resume <id>` (recovery). The argv is logged at `info!` and handed to `tracer_client::spawn` in a `PtySpawnSpec` with `env: merged_env(&cfg.extra_env)`; the tracer runs the child from its content-addressed copy under the binaries cache (`binary_cache.rs`).
- **HTTP.** `build_router` in `server.rs` serves `/ws`, `/health`, `/shutdown` (bearer auth against the in-memory `auth_token`, compared with `secret::constant_time_eq`) and `/pair`. The same router is served on the loopback listener and on the opt-in LAN TLS listener. The loopback port is ephemeral (`127.0.0.1:0`) and changes on every daemon start; `daemon.json` carries the current port and token.
- **Status.** `protocol::SessionStatus` is `Spawning | Idle | Working | AwaitingInput | Stopped | Error` on `SessionSnapshot.status`. Neither it nor `AttentionReason` has a `#[serde(other)] Unknown`, so neither can gain a variant while protocol 22 must decode. `pty_state::watch` writes `rec.status` and sends an `AttentionEvent` (forwarded as `DaemonMessage::Attention`) on each transition into `AwaitingInput`. Today `AwaitingInput` means "a permission or question prompt is up" and `Idle` means "the turn is over".
- **Headless.** `claude --print --output-format stream-json` sessions are parsed in `ClaudeBackend::handle_headless_line`; they already get structured status and need no hooks.

## Design

### Transport: a hook shim that reads `daemon.json` on every call

The injected hooks are **command hooks in exec form, `async: true`**, running a small subcommand of the session's own tracer binary: `<cached rt-tracer.exe> hook --session <rt session id> --config-dir <dir>`. The shim reads the event JSON from stdin, reads `daemon.json` from the config dir for the current port and auth token, POSTs the body verbatim to `http://127.0.0.1:<port>/hook/<rt session id>` with `Authorization: Bearer <token>` and an `X-RT-Hook-Time` header (nanoseconds, taken when the shim starts), and exits 0 whatever happens. It is a hand-written HTTP/1.1 POST over `std::net::TcpStream` with a 2 s budget, so the tracer gains no dependency.

Why not `type: "http"` hooks posting straight to the daemon, as VelaTerm does:

- The daemon's port changes on every start, but tracer-backed sessions outlive the daemon. A URL baked into a running Claude's settings would point at a dead port after the first daemon restart, for the rest of the session. The shim looks the port up on each call.
- HTTP hooks cannot be `async`, so every `PreToolUse` would block Claude on a round trip to the daemon (up to the timeout when the daemon is wedged).
- A user's `allowedHttpHookUrls` silently blocks HTTP hooks.

Why the tracer binary hosts the shim: the running tracer holds its cached copy open, and `binary_cache::gc` keeps every copy a live sidecar references, so the shim's path stays valid for exactly the session's lifetime, across rebuilds and installer upgrades. The tracer's `main` dispatches on a leading `hook` argument before `Cli::parse`, so the daemon's existing tracer invocation is unchanged and the tracer ABI (`docs/tracer-abi.md`) is untouched.

Cost: one short process per hook event, off Claude's critical path (async). Async hooks can finish out of order; the daemon orders events by `X-RT-Hook-Time` and drops one older than the last it applied for that session.

### The injected settings

At spawn the daemon writes `sessions/<id>/hook-settings.json` (beside `meta.json`) and passes `--settings <that path>`. A file, not inline JSON: inline JSON would cross the tracer's Windows command line with nested quoting, and argv is logged. The file holds only a `hooks` object, so every other key keeps the user's value, and hook entries add to the user's and the repo's hooks. Events and matchers:

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

### Auth

The shim presents the daemon's own token from `daemon.json`, read fresh on each call, so a restarted daemon's new token just works. A per-session token would add nothing: the agent runs as the same user and can read `daemon.json` and its own environment anyway, and a per-session token would have to be persisted in `meta.json` to survive a daemon restart. The `/hook/:session_id` route is mounted on the loopback listener only; `build_router` splits into a shared router plus a loopback-only extension so the LAN TLS listener never serves it. The route answers `204` at once for any authenticated request (an unknown session id is logged at `debug!` and ignored) and `401` otherwise; the shim ignores both.

### Mapping events to status

A new `crates/daemon/src/hook_status.rs` parses the payload tolerantly (unknown events and fields are ignored) into a `HookSignal { status, pending }`:

| Event | Status | Pending input |
| - | - | - |
| `SessionStart` | unchanged (`Idle` on `startup`) | cleared |
| `UserPromptSubmit`, `PreToolUse` (other tools), `PostToolUse`, `PostToolUseFailure` | `Working` | cleared |
| `PreToolUse` `AskUserQuestion` | `AwaitingInput` | `Question { questions }` |
| `PreToolUse` `ExitPlanMode` | `AwaitingInput` | `PlanApproval { plan }` (truncated) |
| `PermissionRequest` | `AwaitingInput` | `Permission { tool_name, summary }` (`command`, `file_path` or the first 200 chars of `tool_input`) |
| `Notification` `permission_prompt` / `elicitation_*` | `AwaitingInput` | kept, else `Other { message }` |
| `Elicitation` | `AwaitingInput` | `Other { message }` naming the MCP server |
| `Notification` `idle_prompt`, `Stop`, `StopFailure` | `Idle` | cleared |
| `SessionEnd` | unchanged | cleared |

`Stop` maps to `Idle`, keeping today's meaning of the two states (see open question 3). The daemon keeps `Stop`'s `last_assistant_message` (capped at 8 KiB) on the session record, off the wire, for the alerts summarizer. `StopFailure` adds a `recent_actions` line. Hook transitions go through the same transition path as the heuristic, so `Attention { AwaitingInput }` still fires on entry to `AwaitingInput` and nowhere else.

`SessionStart` with `source` `clear`, `resume` or `fork` carries a new conversation id; the daemon updates the record's `claude_session_id`, so the recover dialog (`history.rs`) resumes the conversation the session was really in.

### Coexisting with the heuristic

Each session starts heuristic-driven. The first authenticated hook event makes it **hook-driven** for the rest of its life (reset only on a daemon restart, until the next event arrives). While hook-driven:

- Hooks own every promotion and all of `AwaitingInput`; the heuristic's classifier stops writing status and stops sending attention events.
- The heuristic keeps one power: demoting `Working` to `Idle` after `IDLE_AFTER` of PTY silence. `Stop` does not fire on a user interrupt (Esc), and Claude's TUI repaints at least once a second while it works, so silence during hook-reported `Working` means the turn was interrupted.

A session whose hooks never run (Codex, Cursor, a `disableAllHooks` or `allowManagedHooksOnly` user, an untrusted folder, an older `claude`) stays heuristic-driven with no change in behaviour. No setting is needed to turn the feature off.

### Protocol changes

Additive only; protocol 22 keeps decoding and `supported` does not change.

- `SessionSnapshot` gains `#[serde(default)] pending_input: Option<PendingInput>`. A v22 client ignores the unknown field.
- `PendingInput` is a new tagged enum (`#[serde(tag = "kind", rename_all = "snake_case")]`) with `Question { questions: Vec<PendingQuestion> }`, `Permission { tool_name, summary }`, `PlanApproval { plan }`, `Other { message }` and `#[serde(other)] Unknown`. `PendingQuestion` is `{ question, header, options: Vec<String>, multi_select }`.
- `SessionStatus` and `AttentionReason` are not touched.

### Resumed, duplicated and recovered sessions

Every interactive Claude spawn writes its own settings file under its own rt session id, whether it starts fresh, resumes (`--resume`, which keeps the conversation id), is duplicated (`duplicate_session` builds a new spawn) or is recovered through `RecoverSessions`. The recovery path that types `claude --resume <id>` into a plain shell (`history.rs`, the shell-recovery injector) writes the settings file for the shell's session and types `claude --settings <path> --resume <id>`. A tracer-reattached session keeps its original settings; its shim finds the new daemon through `daemon.json`. Events fired while no daemon runs are lost; the session shows the heuristic's status until the next event.

### Headless, Codex, Cursor

- Headless Claude sessions keep the stream-json parser and get no hooks.
- Codex has the right events but reads hooks only from `~/.codex/` and the repo's `.codex/`; writing there would edit the user's configuration, which this design never does. Codex's generic `-c key=value` config override might carry a hooks table per invocation; that is unverified and is the first thing HS.10 checks. Until then Codex stays heuristic-driven.
- Cursor stays heuristic-driven.

### Prompt injectors

`InjectorStartup::AgentTui` keeps its quiet-period wait. Hooks give it nothing earlier to wait on: `SessionStart` fires before the input box is known to accept keystrokes. `verify_mode_marker` stays as is; `UserPromptSubmit`'s `permission_mode` arrives only after the prompt is sent, so the daemon logs a `warn!` when it differs from the injector's expected mode, which makes a failed plan-mode entry visible in `daemon.log`.

## Open questions

Answered (user): Q3 `Stop` means `Idle`, as today; `AwaitingInput` is kept for real questions (AskUserQuestion, permission requests, plan approval). Settled (orchestrator, technical): Q1 (a) async command hooks through the tracer's `hook` subcommand; Q2 (a) hooks own promotions and `AwaitingInput`, the heuristic only demotes after silence; Q4 (a) the daemon's token from `daemon.json`; Q5 (a) `rt-tracer.exe hook`; Q6 (a) Codex gets hooks only if `codex -c` carries them per run.

1. **Hook transport.** (a) Recommended: async command hooks running the tracer's `hook` subcommand, which reads `daemon.json` each call. Worst case: one extra process per tool call, and a hook event lost while the daemon is down. (b) `type: "http"` hooks posting straight to the daemon, with the settings file rewritten on each daemon start. Worst case: Claude does not re-read a `--settings` file mid-session (unverified), so every session outliving a daemon restart loses hook status for good. (c) The shim writes to the tracer's named pipe and the tracer relays, buffering while the daemon is down. Worst case: a tracer ABI addition and the most code, for events a restart would otherwise drop.
2. **Hooks versus heuristic.** (a) Recommended: hooks own promotions and `AwaitingInput`; the heuristic may only demote `Working` to `Idle` after silence. Worst case: a tool that prints nothing for 2.5 s while the TUI also stops repainting flips to `Idle` early. (b) Hooks alone. Worst case: after an Esc interrupt the dot stays `Working` until the next prompt. (c) Heuristic stays primary; hooks only fill `pending_input`. Worst case: none of the reliability gain, only the question text.
3. **What `Stop` means.** (a) Recommended: `Idle`, as today; the alerts item fires on the stop through its own signal and the kept `last_assistant_message`. Worst case: a client that wants "turn finished" has to watch `Working → Idle`. (b) `AwaitingInput`, as VelaTerm does. Worst case: every finished turn raises an `Attention` and an OS notification (P4.3), which is noisy with many sessions.
4. **Auth.** (a) Recommended: the daemon's token from `daemon.json`, read per call. Worst case: none beyond today's trust boundary (any same-user process can already read it). (b) A per-session token in the child's environment, persisted in `meta.json`. Worst case: more state to keep in step across restarts, with no attacker it keeps out.
5. **Which binary hosts the shim.** (a) Recommended: a `hook` subcommand of `rt-tracer.exe`, run from the session's cached copy. Worst case: the tracer binary grows a second job. (b) A new `rt-hook.exe`. Worst case: another binary for the binaries cache, the installer and `gc` to track per session. (c) A subcommand of `rustling-tulipd.exe`. Worst case: its cached copy is not pinned by the session, so `gc` can prune it under a live session.
6. **Codex.** (a) Recommended: HS.10 checks `codex -c` for a per-invocation hooks table and implements it only if it works; otherwise Codex stays on the heuristic. Worst case: Codex never gets hook status. (b) Write hooks into `~/.codex/hooks.json`. Worst case: rustling-tulip edits the user's Codex configuration, which the design forbids.

## Steps

- [ ] **HS.0 Spike: verify the CLI behaviour this design leans on.** With a real `claude` on Windows: a `--settings` file's hooks run alongside a user hook and a repo `.claude/settings.json` hook; async exec-form command hooks run a `.exe` with `args`; hooks from `--settings` wait for workspace trust in a fresh worktree; which events fire for an `ExitPlanMode` approval, a denied permission and an Esc interrupt; whether `SessionStart` fires before the first prompt. Record the runs in `docs/spikes/hook-status.md`. Proof: the spike note, with each answer quoted from a run.
- [ ] **HS.1 Event parsing and mapping.** New `crates/daemon/src/hook_status.rs`: tolerant payload types and `map_event(&Value) -> Option<HookSignal>` per the mapping table. Proof: table-driven unit tests, one per row, plus an unknown event and a malformed body returning `None`.
- [ ] **HS.2 Protocol: `pending_input`.** `crates/protocol/src/lib.rs`: `PendingInput`, `PendingQuestion`, the `SessionSnapshot` field; the daemon's snapshot builder (`session.rs`) fills it. Proof: a round-trip test, a `#[serde(other)]` test for an unknown `kind`, and `cargo test -p protocol v22_compat` still green with a snapshot carrying the field.
- [ ] **HS.3 The shim.** `crates/tracer/src/main.rs` dispatches a leading `hook` argument to a new `crates/tracer/src/hook.rs` (stdin, `daemon.json`, POST, always exit 0). Proof: tests against a local `TcpListener` fake asserting the path, bearer token, `X-RT-Hook-Time` and verbatim body; missing `daemon.json` and a refused connection both exit 0 within the budget.
- [ ] **HS.4 The daemon route.** `server.rs`: loopback-only `POST /hook/:session_id` (split `build_router`), bearer check through `secret::constant_time_eq`, `204` at once, hand-off to the session's hook state with out-of-order drop by `X-RT-Hook-Time`. Proof: router `oneshot` tests for `401` without or with a wrong token, `204` for an unknown session, a stale event ignored; a test that the LAN router has no `/hook` route.
- [ ] **HS.5 Settings injection.** `agents/mod.rs` `CommonSpawnFields` gains a required `hook_settings: Option<&Path>` set by every caller; `agents/claude.rs` writes `hook-settings.json` (a `build_hook_settings` fn) and pushes `--settings <path>` for interactive spawns, fresh and resumed, never headless; `spawn_interactive_session` and `duplicate_session` pass it. Proof: unit tests on the argv (fresh, `--resume`, headless without it) and on the JSON (events, matchers, exec form, `async: true`, the cached tracer path).
- [ ] **HS.6 Arbitration.** `pty_state.rs`: a per-session hook-driven flag; hook signals applied through the shared transition path (so `Attention` fires once on entry to `AwaitingInput`); the classifier silenced while hook-driven except the `Working → Idle` silence demotion. Proof: paused-clock tokio tests: a hook `AwaitingInput` survives PTY output that the classifier reads as `Working`; hook `Working` plus silence demotes to `Idle`; a session with no hook events behaves exactly as today (existing tests unchanged).
- [ ] **HS.7 Conversation id and last message.** `hook_status.rs` / `session.rs`: `SessionStart` with `clear` / `resume` / `fork` updates `claude_session_id`; `Stop` stores `last_assistant_message` (capped) on the record; `StopFailure` adds a `recent_actions` line; a `permission_mode` mismatch on `UserPromptSubmit` against the injector logs a `warn!`. Proof: unit tests on the record after each event, and a `history.rs` test that an ended session's entry carries the updated conversation id.
- [ ] **HS.8 Shell recovery path.** `history.rs`: the shell-recovery injector writes the shell session's settings file and types `claude --settings <path> --resume <id>`. Proof: the existing test at the `content: format!("claude --resume …")` site updated to assert the new command.
- [ ] **HS.9 End to end.** `tools/e2e/fake-claude/index.mjs` reads `--settings`, and on scripted cues runs the configured hook commands with the matching JSON on stdin (`UserPromptSubmit`, `PreToolUse AskUserQuestion`, `Stop`). A new case in `apps/native/tests/e2e_live.rs` asserts the snapshot goes `Working → AwaitingInput` with the question in `pending_input`, then `Idle`. Proof: `.\rt.ps1 native-e2e` green.
- [ ] **HS.10 Codex.** Check whether `codex -c` accepts a hooks table per invocation. If it does, `agents/codex.rs` injects the same shim for `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, `PostToolUse`, `Stop` with an argv test; if not, record the finding in this doc and leave Codex on the heuristic. Proof: the argv test, or the recorded finding.
- [ ] **HS.11 Docs.** CLAUDE.md's "Status detection" invariant and the `sessions/<id>/` layout line (`hook-settings.json`); a README glossary entry for "hook shim" and "hook-driven"; tick the index line in `docs/plan.md` and the first item of `borrowed-ideas.md`; move this doc to `docs/plans/completed/`. Proof: the diff.
