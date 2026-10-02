# Agents that drive rustling-tulip

Design for the "Agents that drive rustling-tulip" line in [MAIN.md](./MAIN.md) ("Ideas needing a design pass"), steps `AC.n`. Origin, prior art and rulings: [borrowed-ideas.md](./borrowed-ideas.md). VelaTerm's version: `src-tauri/src/agent/cli_client.rs` (hidden `vlx-term` subcommands behind `vspawn` / `vrefer` / `vsearch` shims that POST JSON to its local hook service with `VLX_SESSION_ID` and `VLX_TOKEN` from the environment), `tell.rs`, `plan_execute.rs`, and `docs/manuals/planning-and-execution_*.md` (a spawn confirmation card that `--yes` skips, and a task-split review that nothing skips). Builds on [hook-status.md](./hook-status.md) (HS.3's shim, HS.4's loopback-only router, HS.7's kept last message).

## Problem

An agent in one session can't start, instruct or read another session. A user who wants a task fanned out across worktrees spawns each session by hand, pastes each prompt, and carries results between panes. This design gives a session a small CLI for that. Splitting a task, reviewing the parts and merging them stay the agent's job: the user ruled out a built-in plan-then-execute feature, since an agent handles review and merging best, and the agent skill pack ([agent-skill-pack.md](./agent-skill-pack.md)) teaches that.

## Rulings

- **Placement (user).** A spawned child sits in its own repo or workspace container like any session, and its leaf carries a `↳ <parent label>` tag; no nesting under the parent leaf.
- **Auth (borrowed-ideas.md).** A per-session token in the session's environment, never the daemon's full `auth_token`.

## The agent CLI

### What the code does today (measured)

- `/ws` → `client_session` (`crates/daemon/src/server.rs`): the first frame must be `Hello` carrying the daemon's `auth_token` (checked with `secret::constant_time_eq`); after it, `dispatch` runs any `ClientMessage` with no per-message permission check, including `Shutdown`, `ConfigureLan`, `StartPairing`, `DiscardSession` and `SetWorktreesRoot`. Every connection holds a `ClientCountGuard` (which wakes the `git_watch` refreshers), subscribes to every session, tab, preset and state event, and is sent `push_initial_state`.
- `build_router` serves `/ws`, `/health`, `/shutdown` and `/pair` on both the loopback listener and the opt-in LAN TLS listener. HS.4 plans a loopback-only extension for `/hook`.
- PTY env: `tracer_client::tracer_command` has no `env_clear`; it sets `spec.env`, `RUSTLING_TULIP_TRACER_OWNER` (the config dir) and `RUSTLING_TULIP_TRACER_LOG`, and `crates/tracer/src/supervisor.rs` forwards every variable except the log one to the child. A PTY child therefore already knows the config dir and can read `daemon.json`, full token included. Headless (`headless.rs`) calls `env_clear`, passes `spec.env`, and runs with stdin null.
- `crates/tracer/src/main.rs` has no subcommands yet (one clap `Cli` with trailing program args); HS.3 plans a `hook` dispatch before `Cli::parse`. The tracer has clap, serde_json and tokio, and no HTTP or WebSocket client. Its cached copy is named `rt-tracer-<hex16>.exe` (`binary_cache.rs`), so it has no stable name on `PATH`.
- `SendInput` writes raw bytes to the PTY; `termstate.rs` tracks whether the child enabled bracketed paste; `strip_ansi` exists twice (`pty_state.rs`, `inject.rs`); `transcripts.rs` locates a conversation's jsonl; tokens come from `lan::generate_auth_token`.

### What the token is for

Not a boundary against a same-user process: any such process can read `daemon.json` (hook-status.md, Q4). The token tells the daemon which session is calling (for the parent link, the limits and the scope below), confines the sanctioned path to a short verb list, and is revoked per session. The CLI reads `daemon.json` for the port only.

### Transport: HTTP routes on the loopback router

Each CLI call is one request and one reply, so the CLI uses `/agent/v1/<verb>` routes on the loopback-only router (HS.4's split; AC.4 makes the split if HS.4 has not landed), `Authorization: Bearer <agent token>`. The WebSocket stays the clients' path: a CLI connection there would count as a client, receive every broadcast, and need an allowlist on every `dispatch` arm. The routes are not part of the wire protocol and version by path. Alternatives: Open question 1.

### Session environment

Every tracer-backed and headless spawn (interactive, plain shell, headless; fresh, resumed, duplicated, recovered) gets `RT_SESSION_ID`, `RT_AGENT_TOKEN` (a fresh `lan::generate_auth_token`), `RT_CONFIG_DIR` and `RT_CLI` (the absolute path of the session's cached tracer). The record and `meta.json` keep only `agent_token_sha256` (`#[serde(default)]`), so a reattached session keeps a valid token after a daemon restart; the daemon verifies by hashing the presented token and comparing with `constant_time_eq`. The token is valid while the session is live and revoked when it ends, parks or is discarded. `PtySpawnSpec` and `HeadlessSpec` print variable names only in `Debug` (DK.6 adds this; AC.2 does if DK has not landed).

### Verbs

| CLI (`$RT_CLI agent …`) | Route | Does |
| - | - | - |
| `whoami` | `GET /agent/v1/self` | The caller's id, label, container, branch, depth |
| `list` | `GET /agent/v1/sessions` | Sessions in the caller's scope: id, label, status, `status_since`, branch, worktree paths |
| `spawn` | `POST /agent/v1/spawn` | A child session with a first prompt, after approval (below) |
| `send <session>` | `POST /agent/v1/send` | Text to a session's input, submitted |
| `read <session>` | `GET /agent/v1/read` | `--last` (the last assistant message), `--transcript [N]`, or `--output [bytes]` (scrollback tail, ANSI stripped) |
| `wait <session>…` | `GET /agent/v1/wait` | Long-poll until one or all targets are `idle`, `ended` or `awaiting`, at most 600 s a call |

Nothing else is reachable: no stop, discard, git, settings or shutdown (Open question 3). Scope is the caller's family, its parent and its descendants, plus sessions the user approves for it (Open question 4). A prompt or message comes from the argument, `--file <path>` or stdin, capped at 16 KiB (it travels as argv to the tracer); output is plain text, or JSON with `--json`; exit codes: 0 ok, 2 usage, 3 refused, 4 denied or unanswered, 5 no daemon.

### Spawn

- **Where.** The caller's container: the same repo, workspace or standalone folder. Without `--worktree`, the child runs in the caller's folder (pinned through `existing_worktree` / `existing_worktrees`, or the caller's branch in place). With `--worktree [--branch B]`, a new branch (default: `branch_names`' suggestion from the caller's branch) is created from the caller's branch in a new worktree through the normal spawn path; uncommitted changes do not travel.
- **What.** The caller's `SpawnConfig` (agent, model, agent options, `extra_env`), overridden by `--agent`, `--model` and `--permission-mode`, with authority capped at the caller's (Open question 9). `--label` names it.
- **Parent.** The record carries `parent: SessionParent { session_id, label }` (the caller's display label at spawn), persisted in `meta.json` and the history entry; Restart, Resume and Recover keep it, Duplicate drops it.
- **Approval.** The daemon raises an agent request (below); the CLI blocks until the user answers or `--timeout` (default 300 s) runs out (Open question 5). Limits (depth, children, totals, rate) are checked before the request is raised (Open question 6).

### Send, read, wait

- **Send** goes through the same write path as the prompt injector's `Text` step: bracketed paste when `termstate` says the target enabled it, then Enter, prefixed `[from <caller label>] ` so the receiver and the user see its origin. Headless and orphan targets are refused; a target that is not `Idle` follows Open question 7.
- **Read**: `--last` is HS.7's kept `last_assistant_message` (until HS.7 lands, the transcript's last assistant entry); `--transcript` is the last N user and assistant text entries of the target's jsonl (`claude_session_id` through `transcripts.rs`), a tool call one line each; `--output` is the scrollback tail through one shared `strip_ansi` (this is its third use, so AC.6 consolidates the two copies). Caps: Open question 8.
- **Wait** returns each target's status and `status_since` when the condition holds or the call times out.

### Agent requests on the wire

Additive only; protocol 22 keeps decoding and `supported` does not change.

- `SessionSnapshot.parent: Option<SessionParent>` (`#[serde(default)]`).
- `AgentRequest { id, from_session_id, from_label, created_at, kind: AgentRequestKind }`; `AgentRequestKind` is tagged (`kind`, snake_case): `Spawn { container_label, branch, worktree, agent, prompt }`, `Access { target_session_id, target_label }`, `#[serde(other)] Unknown`.
- `DaemonMessage::AgentRequests { requests }`: the full pending list, broadcast on every change and sent in `push_initial_state`.
- `ClientMessage::AnswerAgentRequest { id, approve: bool, prompt: Option<String> }` (an edited prompt). The Tauri app logs `AgentRequests` as unknown and cannot answer, so its user's requests time out.

### Native client

- Leaf: `↳ <parent label>` after the label, using the live parent's `display_label` when the parent is still listed, else the stored label; tooltip "Spawned by <label>".
- Requests: a sticky card in the notices stack per request, with Approve, Edit prompt and Deny, and the caller's leaf flagged for attention; the Needs You view lists open requests as a "Request" row once NY.3 is in.

### The CLI's home

A second subcommand of the tracer, dispatched before `Cli::parse` like HS.3's `hook`: `agent` in `crates/tracer/src/agent_cli.rs`, sharing a hand-written HTTP/1.1 client (`crates/tracer/src/http.rs`) with the hook shim, so the tracer gains no dependency, and the path in `RT_CLI` stays valid for the session's life (`binary_cache::gc` keeps copies a live sidecar references). Alternatives: Open question 10. How an agent learns it exists: Open question 11.

## Steps

Order: AC.1 → AC.2 → AC.3 → AC.4 → AC.5 and AC.6 → AC.7 → AC.8 → AC.9 → AC.10. AC.4 builds on HS.4's router split and AC.7 on HS.3's HTTP client when those land first; otherwise each makes them. The gates are the nextup profile's (`.claude/skills/rustling-tulip-nextup/SKILL.md`, "Agents and gates").

- [ ] **AC.1 Protocol.** `crates/protocol/src/lib.rs`: `SessionParent`, `SessionSnapshot.parent`, `AgentRequest`, `AgentRequestKind` (with `Unknown`), `DaemonMessage::AgentRequests`, `ClientMessage::AnswerAgentRequest`. Proof: round-trip tests for each, an unknown `kind` decoding as `Unknown`, a snapshot without `parent`; `cargo test -p protocol` (includes `v22_compat`).
- [ ] **AC.2 Token and environment.** New `crates/daemon/src/agent_token.rs` (generate, hash, verify); `session.rs` (`agent_token_sha256`, `parent` on the record, snapshot fills `parent`); `orphan.rs` (`OrphanMeta` fields, `#[serde(default)]`); `server.rs` spawn paths, `tracer_client.rs`, `headless.rs` (the four variables; name-only `Debug`); revoke on end, park, discard. Proof: `agent_token` unit tests (verify, wrong token, revoked); a `tracer_client` test that `tracer_command`'s `get_envs()` has `RT_AGENT_TOKEN`; a meta round trip holding the hash and never the token; a `Debug` test with no token text; `cargo test -p daemon -- agent_token tracer_client orphan`.
- [ ] **AC.3 The core.** New `crates/daemon/src/agent_api.rs`, plain functions: caller scope, limits, `child_spawn_request(caller, args) -> Result<SpawnRequest, Refusal>` (placement, inheritance, authority cap, prompt cap), send gating by target status, read caps. Proof: table-driven tests: family and non-family targets, each limit at and past its bound, worktree and in-place placement for a repo, a workspace and a standalone caller, a flag asking for more authority than the caller has, a 16 KiB + 1 prompt; `cargo test -p daemon agent_api`.
- [ ] **AC.4 Routes and requests.** `server.rs`: loopback-only `/agent/v1/*`, bearer check, the pending-request registry with timeout, `AgentRequests` broadcast and initial push, the `AnswerAgentRequest` arm, spawns through `spawn_session` with `parent` set. Proof: router `oneshot` tests: `401` without or with a wrong token, the LAN router has no `/agent` route, a spawn waits and an approval yields a session whose snapshot has `parent`, a denial answers `403`, no answer `408`, a limit `429`; `cargo test -p daemon agent_routes`.
- [ ] **AC.5 Send and wait.** `agent_api.rs` / `server.rs`: send delivery (bracketed paste from `termstate`, the origin prefix, Open question 7's rule) and the `wait` long-poll on status changes. Proof: paused-clock tokio tests: a send to an idle target writes the pasted bytes and Enter, a send to a busy target follows the ruling, `wait` returns on the transition and on timeout; `cargo test -p daemon -- agent_send agent_wait`.
- [ ] **AC.6 Read sources.** `agent_api.rs`, `transcripts.rs` (tail of N entries), one shared `strip_ansi` replacing the copies in `pty_state.rs` and `inject.rs`; `--last` from HS.7's field when present. Proof: a fixture jsonl giving the last N text entries with tool calls folded, caps honoured, the existing `strip_ansi` tests moved and green; `cargo test -p daemon -- transcripts agent_read`.
- [ ] **AC.7 The CLI.** `crates/tracer/src/main.rs` dispatches a leading `agent`; new `agent_cli.rs` and `http.rs`. Proof: tests against a local `TcpListener` fake asserting each verb's path, bearer and body, `--file` and stdin input, `--json`, exit codes 2 to 5, a missing `RT_AGENT_TOKEN` naming the variable; `cargo test -p tracer agent_cli`.
- [ ] **AC.8 Native client.** `apps/native/src/sidebar.rs`, `sidebar_view.rs` (the `↳` tag), `net.rs` (`AgentRequests`), new `agent_requests.rs` and `agent_requests_view.rs` (the cards), `lib.rs`, `tests/support/mod.rs` (`SessionBuilder::parent`, a request builder). Proof: new `tests/ui/ui_agent_requests.rs` (a card per request, Approve and Deny send `AnswerAgentRequest`, an edited prompt is sent, a card leaves when the list drops it) and a `ui_sidebar` case (the tag with a live and with a gone parent); `cargo test -p rustling-tulip-native --test ui ui_agent_requests --test ui ui_sidebar`.
- [ ] **AC.9 Live tier.** `tools/e2e/fake-claude/index.mjs`: a `/rt <args>` cue that runs `$RT_CLI agent <args>` and prints its output; `apps/native/tests/e2e_live.rs`: session A spawns a child, the test approves it through `LiveClient`, the child appears with `parent`, A sends to it and reads its output back, and A's token stops working once A is stopped. Proof: `.\rt.ps1 native-e2e` through the gate (heavy).
- [ ] **AC.10 Skill and docs.** Open question 11's choice; `docs/architecture.md` (component line, task-index row "Change the agent CLI": `agent_api.rs`, `agent_token.rs`, `crates/tracer/src/agent_cli.rs`); `docs/native-client.md` (the tag, the cards); CLAUDE.md (the `RT_*` variables, `agent_token_sha256` in the `sessions/<id>/` line); README glossary (below); borrowed-ideas.md's entry and this doc deleted once promoted. Proof: the diff.

## Glossary terms this subplan coins

- **Agent CLI**: `rt-tracer.exe agent …`, run inside a session to spawn, message, read and wait on other sessions; found through `RT_CLI`.
- **Agent token**: a session's own credential for the agent CLI, in `RT_AGENT_TOKEN`; the daemon keeps only its hash.
- **Family**: a session's parent and descendants, the sessions its agent CLI may reach without asking.
- **Agent request**: a spawn or access waiting on the user's approval.

## Open questions

Answered (user): Q1 (a) loopback HTTP routes with a per-session bearer token; Q2 (a) the token lives while its session does, its hash in `meta.json`, revoked on end, park or discard; Q3 (a) `whoami`, `list`, `spawn`, `send`, `read`, `wait`; Q4 (a) the family plus sessions the user approves for the caller; Q5 (a) every spawn raises a request, with "Allow further spawns from this session"; Q6 (a) depth ≤ 2, ≤ 8 live children per session, ≤ 16 agent-spawned sessions in all, one spawn request per 10 s; Q7 (user's own answer) an `Idle` or `Working` target gets the text at once (the CLI queues mid-turn input), an `AwaitingInput` target holds it until the session is `Working` again; Q8 (a) output 16 KiB by default and 256 KiB at most, transcript 20 entries and 200 at most, last message 8 KiB; Q9 (a) a child's permission mode is inherited and capped at the caller's; Q10 (a) an `agent` subcommand of the session's cached `rt-tracer.exe`, found through `RT_CLI`; Q11 (a) a skill shipped in the repo plus `agent help`. Q12–Q14 (the plan split, executors and merging) fell with the plan-then-execute feature, which the user dropped.

1. **Transport.** (a) Recommended: HTTP routes on the loopback-only router. How: one request per CLI call, bearer agent token. Worst case: a `wait` long-poll holds one connection per waiting agent, and a runaway loop of short waits floods `daemon.log` with request lines. (b) The WebSocket with a scoped `Hello` (an `agent_token` field) and an allowlist on every `dispatch` arm. Worst case: each CLI call counts as a client, wakes git refreshers and receives every broadcast; one missed arm in the allowlist hands an agent `Shutdown`. (c) The tracer's named pipe, relayed by the tracer. Worst case: a tracer ABI addition and the most code, for calls that fail anyway while no daemon runs.
2. **Token lifetime.** (a) Recommended: valid while the session is live, hash in `meta.json`, revoked on end, park or discard. Worst case: a leaked token works until its session ends, which for a long-lived session is days. (b) Derived as an HMAC of the session id with a persistent key file, nothing stored per session. Worst case: revoking one session's token needs a deny-list anyway, and a leaked key file mints tokens for every session. (c) In memory only, dead after a daemon restart. Worst case: every session outliving a restart loses its CLI for good.
3. **Which verbs.** (a) Recommended: `whoami`, `list`, `spawn`, `send`, `read`, `wait`. Worst case: an agent that spawned a runaway child cannot stop it; the user must. (b) Also `stop` for the caller's own descendants. Worst case: a confused parent stops its children mid-work and their state is lost with the session. (c) Every `ClientMessage` but a denylist of admin ones. Worst case: git writes and discards through a path no one reviewed.
4. **Target scope.** (a) Recommended: the family, plus any other session the user approves for this caller through an `Access` request (remembered for the caller's life). Worst case: a prompt-injected agent asks for access to many sessions and the user clicks through the cards. (b) Any session, no approval. Worst case: an agent reading untrusted text sends instructions into every running session. (c) Family only. Worst case: "tell the session on branch X" needs the user to relay it by hand.
5. **Approval before a child spawns.** (a) Recommended: every spawn raises a request, with an "Allow further spawns from this session" choice on the card. Worst case: a user who ticked it is back to limits-only for that session. (b) No approval, limits only, a toast per spawn. Worst case: an agent in a loop spawns up to the limits before the user reads the first toast. (c) One approval per caller for up to N spawns. Worst case: N children with prompts the user never saw.
6. **Limits and loops.** (a) Recommended: depth at most 2 (a user's session → a child → its children), at most 8 live children per session, at most 16 live agent-spawned sessions in all, one spawn request per 10 s per session. Worst case: 16 Claude processes and worktrees at once exhaust memory or the plan's usage window. (b) Totals only, no depth limit. Worst case: a chain of agents each spawning one child walks down to the total. (c) Depth 1: children cannot spawn. Worst case: a coordinating agent spawned through the CLI cannot fan its work out.
7. **Send to a session that is not idle.** (a) Recommended: `Working` → held and delivered on the next `Idle` (a later send to the same target appends); `AwaitingInput` → refused, since typed text could answer a permission prompt ("1" means yes). Worst case: held messages pile up behind a session that never goes idle. (b) Deliver at once whatever the status. Worst case: an agent's message approves a permission prompt in another session. (c) Refuse unless `Idle`. Worst case: agents busy-loop retrying against a working target.
8. **Read limits.** (a) Recommended: `--output` 16 KiB by default, 256 KiB at most; `--transcript` 20 entries, 200 at most; `--last` 8 KiB. Worst case: an agent reading 256 KiB of TUI output in a loop fills its own context. (b) No caps. Worst case: one read of a 2 MB scrollback exhausts the caller's context in one call. (c) Transcript and last message only, no raw output. Worst case: plain shells and Codex sessions cannot be read at all.
9. **A child's authority.** (a) Recommended: inherited and capped at the caller's: `dangerously_skip_permissions` and `bypassPermissions` only if the caller has them. Worst case: a bypass-mode parent spawns bypass-mode children freely (the card shows it). (b) Any flag may be requested, and the card flags an elevation. Worst case: a default-mode agent talks the user into a bypass child with one click. (c) Children always start in default mode. Worst case: executors stop on every permission prompt and a plan stalls until the user answers each.
10. **Where the CLI lives.** (a) Recommended: an `agent` subcommand of the session's cached `rt-tracer.exe`, found through `RT_CLI`. Worst case: the tracer binary carries a third job, and agents must type `& $env:RT_CLI agent …` rather than a short name. (b) Its own `rt-agent.exe` in the binaries cache, with a per-session shim folder on `PATH`. Worst case: another binary for the cache, the installer and `gc` to track per session, and `.cmd` shim quoting mangling prompts. (c) A subcommand of `rustling-tulipd.exe`. Worst case: its cached copy is not pinned by the session, so `gc` can prune it under a live one.
11. **How agents learn the CLI.** (a) Recommended: a skill shipped in the repo that the user links into `~/.claude/skills` once, plus `agent help` as the reference. Worst case: sessions never use the CLI until the user installs the skill. (b) An `--append-system-prompt` line on every Claude spawn naming `$RT_CLI`. Worst case: every session's context grows, and agents start spawning children unasked. (c) The line only on children the CLI spawned. Worst case: the first session in a chain still does not know.
