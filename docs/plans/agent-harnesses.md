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

- `protocol::Agent` (`crates/protocol/src/lib.rs` ~31) has no `#[serde(other)] Unknown`, unlike the nested enums CLAUDE.md requires to carry one. A new variant in a `Sessions` or `SessionUpdated` broadcast fails to decode in a client that predates it, the protocol-22 Tauri app included, so adding harnesses needs either a per-connection downgrade of the agent field for older protocol versions, or a protocol bump (Open question 9).
- `AgentOptions` is a tagged enum with one variant per agent; each new harness adds a variant (additive) and its options.

## Design (to be completed from the answers)

- One backend module per harness under `crates/daemon/src/agents/`, a `protocol::Agent` variant and an `AgentOptions` variant each, and a `RUSTLING_TULIP_<NAME>` program override per harness as for Claude and Codex.
- Conversation id capture per tier, as the existing three do: Tier A passes a chosen id; Tier B pre-creates; Tier C finds it after launch. The id lands in `agent_conversation_id`, so own-agent recovery covers every harness that can resume.
- The spawn dialog's runtime choice becomes a dropdown (`spawn_form.rs` state, `spawn_view.rs` view); its grouping, the handling of uninstalled CLIs and the per-harness options follow the answers below.

## Open questions

Answered (user): Q1 (c) every kept harness in one design pass, which with Q2 is twelve (Amp out); Q2 (a) WSL-only CLIs are left out; Q3 (a) a harness with no extra-folder flag runs in the first folder with a warning naming the rest, as Cursor does; Q4 (a) Tier C ids come from scanning each CLI's own files on disk, nothing written to the user's config; Q5 (a) installed harnesses first, the rest greyed with "not installed" and the install command in a tooltip, the daemon checking PATH and each override at startup and when the dialog opens; Q6 (b) each harness's own options in the dialog, one options block per harness; Q7 (b) headless runs for the new harnesses too, one stream parser per harness from its JSON output; Q8 (a) sign-in is the user's: a signed-out CLI shows its own prompt or error in its pane; Q9 (c) a connection that negotiated an older protocol version never receives sessions of an agent it cannot decode (`Sessions`, `SessionUpdated` and `SessionRemoved` filtered per connection), and `Agent` gains `#[serde(other)] Unknown` so clients from then on decode a later harness in place. The filter's open edge, a layout in the older client holding a pane for a session it never hears about, is the design's to settle.

1. **What ships first.** (a) Recommended: Tier A in one batch (Copilot, Pi, Gemini, Qwen), then Antigravity on its own. How: Tier A is argument building on the Claude pattern with a chosen id. Worst case: Antigravity, the name the user asked for, waits for a second batch. (b) Antigravity and Pi first, the two the user named. Worst case: Antigravity's id capture is unverified, so the first batch carries a spike. (c) All thirteen in one design pass. Worst case: a long branch before anything ships.
2. **WSL-only CLIs (Amp).** (a) Recommended: leave them out. Worst case: Amp users run it in a plain shell under WSL. (b) Spawn through `wsl.exe`. Worst case: path translation, worktrees and the tracer supervisor all need a WSL story.
3. **Harnesses with no extra-folder flag.** (a) Recommended: run them in the first folder with a warning, as Cursor does today. Worst case: a workspace session on Pi sees only its first repo, and only the log says so. (b) Disable workspace targets for them in the dialog. Worst case: no multi-repo use of those harnesses at all.
4. **Id capture for Tier C.** (a) Recommended: scan the CLI's own files on disk, as Codex's rollout scan does, and write nothing into the user's config. Worst case: two sessions of one harness in one folder are told apart only by timing, as Codex's are. (b) Pass hooks or status-line scripts per launch where the CLI allows it. Worst case: some CLIs only read them from the user's own settings, which the daemon would have to edit.
5. **The dropdown.** (a) Recommended: installed harnesses first, then the rest greyed with "not installed" and the install command in a tooltip; the daemon checks PATH (and each override) at startup and on dialog open. Worst case: a CLI installed while the dialog is open shows greyed until it reopens. (b) Hide uninstalled ones. Worst case: a user never learns a harness is supported.
6. **Per-harness options.** (a) Recommended: the dialog's generic Plan and Auto-approve switches, mapped by each backend to its own flags, plus Model as free text; harness-only options wait. Worst case: a harness's finer modes (Gemini's `--approval-mode` values, Goose's `GOOSE_MODE`) are unreachable from the dialog. (b) Each harness's own options in the dialog. Worst case: one options block per harness to build and keep current.
7. **Headless.** (a) Recommended: interactive only for the new harnesses. Worst case: no headless runs of them until asked for. (b) Headless too, from each CLI's JSON stream. Worst case: one stream parser per harness.
8. **Sign-in.** (a) Recommended: the user's problem; a spawn that fails because the CLI is signed out shows the CLI's own message in its pane. Worst case: a first launch lands on a login screen. (b) Detect "installed and signed in" for the dropdown. Worst case: a per-harness auth probe to build and keep working.
9. **Older clients and the closed `Agent` enum.** (a) Recommended: add `#[serde(other)] Unknown` to `Agent` now, and have the daemon send a new harness's sessions to a connection that negotiated an older protocol version with `agent: claude` and a label naming the harness, so the Tauri app keeps decoding. Worst case: the Tauri app shows a Pi session with Claude's icon and offers Claude actions on it. (b) Bump the protocol and drop 22 from `supported` once the Tauri app is retired. Worst case: the installed Tauri app stops connecting until then.
