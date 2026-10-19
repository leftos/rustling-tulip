# A runtime dropdown of every popular agent harness

Design for the "A runtime dropdown of every popular agent harness" line in [MAIN.md](./MAIN.md) (Backlog, Agent CLIs): the spawn dialog's runtime choice grows from Claude / Codex / Cursor / shell into a dropdown of the popular terminal agent CLIs, Google Antigravity and Pi among them (user). Step ids are `AH.n`, written once the open questions below are answered.

## Problem

The daemon runs three agent CLIs, each through a backend in `crates/daemon/src/agents/` (`AgentBackend`: `program_env_var`, `default_program`, `supports_headless`, `build_interactive_args`, `build_headless_args`), picked by the closed `protocol::Agent` enum (`Claude`, `Codex`, `Cursor`) and its `AgentOptions` options. Every other harness is reachable only by typing its command into a plain-shell session, which gets no status, no resume, no Recover and no chip.

## Findings

Research note: [docs/research/2026-09-30-agent-harnesses.md](../research/2026-09-30-agent-harnesses.md) (primary sources, versions and dates per harness; a support matrix of 13). In short:

- **Antigravity** has a terminal CLI, `agy` (release 1.2.14, native on Windows), running the same agent as the Antigravity 2.0 desktop app. Gemini CLI is still released separately.
- **Pi** is `pi` (earendil-works/pi, MIT), with `--session-id <id>` to open or create a chosen session.
- Ranked by how cheaply each fits the backend shape (fresh spawn, resume, conversation id known for Recover):
  - **Tier A, the caller chooses the id up front** (as Claude's `--session-id`): GitHub Copilot CLI, Pi, Gemini CLI, Qwen Code; Goose by session name.
  - **Tier B, pre-create the conversation** (as Cursor's `create-chat`): Amp (WSL-only on Windows, no prompt on the command line).
  - **Tier C, find the id after launch** (as the Codex rollout scan): Antigravity CLI, Factory Droid, Kimi Code CLI, OpenCode, Kiro CLI, Crush, Cline CLI.
- Extra folders (`--add-dir` or equivalent) exist only for Copilot, Gemini, Qwen and Kimi (Antigravity's launch flag is unverified); the rest see one folder, as Cursor does.
- Dropped: Kimi CLI (Python; archived, replaced by Kimi Code CLI), Aider (no release since 2025-08, no resume by id), Kilo CLI (unverified OpenCode fork), Mistral Vibe and OpenHands CLI (little adoption).

### Found in the code

- `protocol::Agent` (`crates/protocol/src/lib.rs` ~31) has no `#[serde(other)] Unknown`, unlike the nested enums CLAUDE.md requires to carry one. A new variant in a `Sessions` or `SessionUpdated` broadcast fails to decode in a client that predates it, so a harness variant is a breaking change for such a client (Open question 9).
- `AgentOptions` is a tagged enum with one variant per agent; each new harness adds a variant (additive) and its options.

## The two named first: Pi and the DeepSeek Harness

The user's steer is to plan Pi and the DeepSeek Harness (`dsh`, `dsh-tui`) ahead of the rest of the matrix. Both fit the backend shape; the DSH path also reuses the Claude hook contract.

### Pi (`pi`, earendil-works/pi, MIT)

Research: [the harness note](../research/2026-09-30-agent-harnesses.md), "Pi".

- **Spawn.** `pi` in the session's cwd; a positional message opens the TUI with it as the first prompt.
- **Conversation id, Tier A.** `--session-id <id>` opens the exact id or creates it (letters, digits, `.`, `_`, `-`). The daemon mints the id as it does Claude's `--session-id`, and Recover passes the same flag.
- **Resume.** `--session-id <id>`. Sessions live under `~/.pi/agent/sessions/`, grouped by cwd.
- **Status.** No hooks and no status file: PTY heuristic only, with the existing title and prompt rules.
- **Options.** `--model <pattern>[:thinking]`, `--provider`, `--thinking`; `-a`/`--approve` trusts project config for the run. Pi has no approval prompts by design.
- **Extra folders.** None; a workspace session runs in `members[0]` with the warning Q3 settled.
- **Headless.** `--mode json` streams JSON events, so Q7 (b)'s parser applies.
- **Program override.** `RUSTLING_TULIP_PI`.

### DeepSeek Harness (`dsh`, `dsh-tui` profile)

Facts from the installed `dsh` launcher (`dsh --help`), the Orca reference integration under `X:\dev\_ext\stablyai\orca` (`src/main/dsh/hook-settings.ts`, `src/main/dsh/dsh-home-patch.ts`, `src/shared/dsh-launch-command.ts`, `src/shared/agent-resume-argv.ts`, `src/shared/tui-agent-config.ts`, `src/shared/dsh-terminal-title.ts`), and the DSH notes in the user-level `nextup` skill.

- **Spawn.** `dsh --profile dsh-tui --patch <overlay> .` in the cwd; `dsh-tui` and `dst` are launchers that boot the same profile. A bare launch opens the session browser, while the positional `.` opens the composer for the cwd.
- **Prompt.** Stdin after the composer is ready (Orca's `stdin-after-start`); the composer's `❯` glyph is the ready signal.
- **Conversation id.** The hooks bridge reports Claude-shaped payloads with `session_id`, so the first `SessionStart` fills `agent_conversation_id` exactly as RT-25 does for Claude. Without hooks, the id is found in `$DSH_HOME/storages/session_projcache/sessions/<id>.json`.
- **Resume.** `dsh --profile dsh-tui --resume <id>` with the cwd preserved, since DSH keys sessions by workspace path.
- **Status, the hook route.** A per-invocation patch layer (`--patch <file>`) adds one loader row: `- insert: [{id: rt-agent-hooks, name: '@deepseek-ai/dsh-hooks-claude-code', config: {configPath: <hooks json path>}}]`. The hooks file is Claude's shape. Its events are `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse` and `Stop`: the bridge fires no `Notification`, `PermissionRequest` or `SessionEnd`, so `AwaitingInput` comes from the `PreToolUse` matchers and the prompt heuristic, and the end of a session is the PTY exit, not a hook. Nothing is written to `$DSH_HOME` or the user's profiles; the patch file lives under `sessions/<id>/`.
- **Status, the fallback.** The PTY heuristic plus the terminal title: DSH titles carry `🐋`, and its idle prefix `✦` is Gemini's working glyph, so a title rule keys on the whale before any other agent's glyph.
- **Approvals and sandbox.** The harness's own profile configuration decides; the dialog's trusted toggle maps to no documented per-spawn flag.
- **Headless.** `dsh headless "<task>"` answers one task, prints the result and exits — a profile, not a flag, so headless support follows the interactive path.
- **Program override.** `RUSTLING_TULIP_DSH`, pointing at the `dsh` launcher because it owns `--patch`.

### Verification before the briefs

1. `dsh --profile dsh-tui --patch <overlay>` applies the overlay to the TUI profile per invocation, and the bridge reads the `configPath` it names.
2. The bridge's `SessionStart` payload carries the same session id that `--resume <id>` accepts.
3. A spawn from a worktree keeps the composer's cwd, and resume works after a daemon restart.

These are one spike, AH.1, whose note lands in `docs/spikes/dsh-tui.md`.

## Design (to be completed from the answers)

- One backend module per harness under `crates/daemon/src/agents/`, a `protocol::Agent` variant and an `AgentOptions` variant each, and a `RUSTLING_TULIP_<NAME>` program override per harness as for Claude and Codex.
- Conversation id capture per tier, as the existing three do: Tier A passes a chosen id; Tier B pre-creates; Tier C finds it after launch. The id lands in `agent_conversation_id`, so own-agent recovery covers every harness that can resume.
- The spawn dialog's runtime choice becomes a dropdown (`spawn_form.rs` state, `spawn_view.rs` view); its grouping, the handling of uninstalled CLIs and the per-harness options follow the answers below.

## Open questions

Answered (user): The shipping order (Q1) is Pi and the DeepSeek Harness first, the rest of the matrix after them. Q1 (c) every kept harness in one design pass, which with Q2 is twelve (Amp out); Q2 (a) WSL-only CLIs are left out; Q3 (a) a harness with no extra-folder flag runs in the first folder with a warning naming the rest, as Cursor does; Q4 (a) Tier C ids come from scanning each CLI's own files on disk, nothing written to the user's config; Q5 (a) installed harnesses first, the rest greyed with "not installed" and the install command in a tooltip, the daemon checking PATH and each override at startup and when the dialog opens; Q6 (b) each harness's own options in the dialog, one options block per harness; Q7 (b) headless runs for the new harnesses too, one stream parser per harness from its JSON output; Q8 (a) sign-in is the user's: a signed-out CLI shows its own prompt or error in its pane; Q9 (c) a connection that negotiated an older protocol version never receives sessions of an agent it cannot decode (`Sessions`, `SessionUpdated` and `SessionRemoved` filtered per connection), and `Agent` gains `#[serde(other)] Unknown` so clients from then on decode a later harness in place. The filter's open edge, a layout in the older client holding a pane for a session it never hears about, is the design's to settle.

1. **What ships first.** (a) Recommended: Tier A in one batch (Copilot, Pi, Gemini, Qwen), then Antigravity on its own. How: Tier A is argument building on the Claude pattern with a chosen id. Worst case: Antigravity, the name the user asked for, waits for a second batch. (b) Antigravity and Pi first, the two the user named. Worst case: Antigravity's id capture is unverified, so the first batch carries a spike. (c) All thirteen in one design pass. Worst case: a long branch before anything ships.
2. **WSL-only CLIs (Amp).** (a) Recommended: leave them out. Worst case: Amp users run it in a plain shell under WSL. (b) Spawn through `wsl.exe`. Worst case: path translation, worktrees and the tracer supervisor all need a WSL story.
3. **Harnesses with no extra-folder flag.** (a) Recommended: run them in the first folder with a warning, as Cursor does today. Worst case: a workspace session on Pi sees only its first repo, and only the log says so. (b) Disable workspace targets for them in the dialog. Worst case: no multi-repo use of those harnesses at all.
4. **Id capture for Tier C.** (a) Recommended: scan the CLI's own files on disk, as Codex's rollout scan does, and write nothing into the user's config. Worst case: two sessions of one harness in one folder are told apart only by timing, as Codex's are. (b) Pass hooks or status-line scripts per launch where the CLI allows it. Worst case: some CLIs only read them from the user's own settings, which the daemon would have to edit.
5. **The dropdown.** (a) Recommended: installed harnesses first, then the rest greyed with "not installed" and the install command in a tooltip; the daemon checks PATH (and each override) at startup and on dialog open. Worst case: a CLI installed while the dialog is open shows greyed until it reopens. (b) Hide uninstalled ones. Worst case: a user never learns a harness is supported.
6. **Per-harness options.** (a) Recommended: the dialog's generic Plan and Auto-approve switches, mapped by each backend to its own flags, plus Model as free text; harness-only options wait. Worst case: a harness's finer modes (Gemini's `--approval-mode` values, Goose's `GOOSE_MODE`) are unreachable from the dialog. (b) Each harness's own options in the dialog. Worst case: one options block per harness to build and keep current.
7. **Headless.** (a) Recommended: interactive only for the new harnesses. Worst case: no headless runs of them until asked for. (b) Headless too, from each CLI's JSON stream. Worst case: one stream parser per harness.
8. **Sign-in.** (a) Recommended: the user's problem; a spawn that fails because the CLI is signed out shows the CLI's own message in its pane. Worst case: a first launch lands on a login screen. (b) Detect "installed and signed in" for the dropdown. Worst case: a per-harness auth probe to build and keep working.
9. **Older clients and the closed `Agent` enum.** Settled: RT-168 retired the Tauri app and dropped 22 from `supported`, so no connection negotiates an older protocol version. A new `Agent` variant is a breaking change only for a client that predates it, and is bumped per CLAUDE.md "When to bump". The answer's `#[serde(other)] Unknown` on `Agent` still stands; its per-connection filter has no older version to act on while `supported` is `[23]`.

## Steps

- [ ] **AH.1 Spike: DSH per-invocation patch, hook payload and resume.** Run the installed `dsh` from a worktree: (a) `dsh --profile dsh-tui --patch <overlay>` applies the row and the `@deepseek-ai/dsh-hooks-claude-code` bridge reads the `configPath`; (b) the `SessionStart` payload's `session_id` is the id `--resume` accepts; (c) the composer's cwd is the worktree and resume survives a restart. Proof: `docs/spikes/dsh-tui.md`, each answer quoted from a run.
- [ ] **AH.2 Pi backend.** `crates/daemon/src/agents/pi.rs`, a `protocol::Agent::Pi` variant and `AgentOptions::Pi` options, `RUSTLING_TULIP_PI`, the dialog's runtime entry, and a backend test per tier: a fresh spawn passes `--session-id <id>`, resume passes the recorded id, headless uses `--mode json`. Proof: argv tests, one parser test, and a UI spec that the runtime appears in the dialog.
- [ ] **AH.3 DSH backend.** `crates/daemon/src/agents/dsh.rs`, a `protocol::Agent::Dsh` variant and `AgentOptions::Dsh`, `RUSTLING_TULIP_DSH`, and the per-session hooks: the spawn writes `sessions/<id>/dsh-hooks.json` and a patch overlay naming it, passes `--patch <overlay>`, seeds the prompt over stdin after the composer is ready, and records `agent_conversation_id` from `SessionStart`. Resume passes `--resume <id>` with the cwd preserved; the terminal-title rule keys on `🐋`. Proof: a settings-file test, an argv test, the hook mapping through `hook_status::map_event`, and a UI spec.
- [ ] **AH.4 The runtime dropdown.** Replace the spawn dialog's segmented runtime choice with the dropdown Q5 settled: installed harnesses first, the rest greyed with the install command, checked at startup and on dialog open. Proof: UI specs for the ordering, the greyed rows and the per-harness options block.
