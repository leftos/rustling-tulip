# Add codex as an alternative agent

## Context

The daemon currently shells out to the `claude` CLI for every interactive and headless session. Users want to spawn `codex` (OpenAI Codex CLI) sessions through the same UI — same sidebar, same scrollback, same orphan recovery — without rewriting the spawn pipeline twice. The goal is per-session agent choice (claude or codex), defaulting to whatever the user picked last for that repo.

The CLIs are similar enough to share a single spawn path: both support `--add-dir <path>` (repeatable), `--model`, working-directory selection, and a "bypass all safety" flag. They diverge on permission/sandbox semantics and on how the initial prompt is delivered (claude `-p <text>` vs codex positional `[PROMPT]`).

Scope decisions (from the interview):
- **Interactive only** for codex — headless mode stays claude-only for now (codex `exec` JSON parser is a follow-up).
- **Agent-aware permission UI** — codex shows its own `--sandbox` enum + a `--yolo` toggle; claude keeps its existing picker.
- **Workspace sessions supported** for codex via `--add-dir` (confirmed in OpenAI docs).
- **Last-used agent per repo** is remembered in `state.json`.

## Wire protocol (`crates/protocol/src/lib.rs`)

Bump `PROTOCOL_VERSION` `4 → 5`. Add types:

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Agent {
    #[default]
    Claude,
    Codex,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CodexSandbox {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl CodexSandbox {
    #[must_use]
    pub fn as_cli_arg(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}
```

Add fields (all `#[serde(default)]` so v4 messages still decode):
- `SpawnRequest`: `agent: Agent`, `codex_sandbox: Option<CodexSandbox>`. Reuse the existing `dangerously_skip_permissions: bool` — for codex it maps to `--yolo` (alias of `--dangerously-bypass-approvals-and-sandbox`). Reuse `model: Option<String>` (both CLIs accept `--model <id>`). For codex, `permission_mode` is ignored.
- `SessionSnapshot`: `agent: Agent`. Daemon always populates this so the UI can label the session.
- `RepoEntry`: `last_agent: Option<Agent>` for the dialog's per-repo default.

## Daemon

### Spawn path (`crates/daemon/src/server.rs`)

- Rename `claude_program()` (line 1592) → `agent_program(agent: Agent) -> String`. For `Agent::Claude` keep `RUSTLING_TULIP_CLAUDE` / `"claude"` default. For `Agent::Codex` use `RUSTLING_TULIP_CODEX` / `"codex"`.
- In `spawn_interactive_session()` (line 1107), branch arg construction on `req.agent`:
  - **Claude branch** — unchanged from today.
  - **Codex branch** — build args as:
    - `--add-dir <path>` for every member after the first (same loop as claude — codex accepts the identical flag).
    - `--model <id>` if `req.model.is_some()`.
    - When `dangerously_skip_permissions` is true: `--yolo`. Otherwise, if `codex_sandbox.is_some()`: `--sandbox <value>` via `CodexSandbox::as_cli_arg()`.
    - Initial prompt: append as **positional** trailing arg (not `-p`). Skip when `prompt_injector` is set, matching claude's behavior.
- In `spawn_headless_session()` (line 1312), reject codex early with `anyhow!("headless mode is not yet supported for codex; use interactive mode")`. Return the error to the client like other spawn validation errors.
- Persist `agent` on the `SessionRecord` (passed through to `SessionSnapshot` via `snapshot()`).
- On every successful spawn against a single repo or workspace, update `RepoEntry::last_agent` for the targeted repo(s) and re-save `state.json` (existing pattern in `server.rs` for tab/workspace updates).

### Session record (`crates/daemon/src/session.rs`)

Add `agent: Agent` to `SessionRecord` (around line 36). Default to `Agent::Claude` in `snapshot()` for legacy orphan-meta files that lack it (covered below). Surface it in `SessionSnapshot`.

### Orphan recovery (`crates/daemon/src/orphan.rs`)

- Add `agent: Option<Agent>` (`#[serde(default)]`) and keep the existing `program_name: Option<String>` field. At write time, set `program_name = Some("codex")` for codex sessions; `is_session_alive` already matches `program_name` substring-insensitively, so no logic change there.
- When reading legacy `meta.json` without `agent`, default to `Agent::Claude` (matches today's only behavior).

### State persistence (`crates/daemon/src/state.rs`)

`RepoEntry::last_agent: Option<Agent>` (serde-default `None`). Add a single helper `persist_last_agent(repo_id, agent)` used from both single-repo and workspace spawns.

## Frontend

### Types (`apps/tauri-app/src/types.ts`)

Mirror the Rust additions: `Agent = "claude" | "codex"`, `CodexSandbox = "read-only" | "workspace-write" | "danger-full-access"`, fields on `SpawnRequest`, `SessionSnapshot`, `RepoEntry`. Bump the `PROTOCOL_VERSION` constant.

### Spawn dialog (`apps/tauri-app/src/components/SpawnDialog.tsx`)

- Add an **Agent** segmented control near the top of both `SingleForm` (line 435) and `WorkspaceForm` (line 608). Two pills: "claude" / "codex".
- Default selection: `repo.last_agent ?? "claude"` for single-repo; `workspace.members[0].last_agent ?? "claude"` for workspace.
- When `agent === "codex"`:
  - In the mode picker, disable the "headless" option with a tooltip "headless mode is not yet supported for codex".
  - In the Advanced section (line 273), replace the `PermissionMode` dropdown with a `CodexSandbox` dropdown (`read-only / workspace-write / danger-full-access`). Re-label the existing "Dangerously skip permissions" toggle to "Yolo (skip all approvals + sandbox)".
- Send `agent` and `codex_sandbox` in the `SpawnRequest`.

### Session display (`apps/tauri-app/src/components/SessionPane.tsx`)

In the header (line 51), append a small agent badge after the mode suffix: ` · claude` / ` · codex`. Optionally mirror in `Sidebar.tsx` `SessionLeaf` (line 323) and `SessionWindow` (`App.tsx`).

### xterm + pop-out

No changes — both are agent-agnostic byte pipes.

## Critical files to touch

- `crates/protocol/src/lib.rs` — types + version bump.
- `crates/daemon/src/server.rs` — spawn-args branching, `agent_program`, headless guard, `last_agent` write-through.
- `crates/daemon/src/session.rs` — `SessionRecord.agent`.
- `crates/daemon/src/orphan.rs` — `OrphanMeta.agent` (+ `program_name = "codex"` on write).
- `crates/daemon/src/state.rs` — `RepoEntry.last_agent`.
- `apps/tauri-app/src/types.ts` — mirrored TS types + version bump.
- `apps/tauri-app/src/components/SpawnDialog.tsx` — agent picker + agent-aware fields.
- `apps/tauri-app/src/components/SessionPane.tsx` — agent badge.

## Reuse

- `agent_program()` follows the existing `RUSTLING_TULIP_CLAUDE` env-override pattern (server.rs:1592) — just parameterized.
- `--add-dir` loop body is copied verbatim from the claude branch — codex uses the identical flag.
- `is_session_alive` (orphan.rs:128) already keys off `program_name`, so codex orphans get free liveness checks once we set `program_name = "codex"`.
- `dangerously_skip_permissions: bool` is repurposed for codex `--yolo` rather than adding a new field.
- `RepoEntry` mutate-then-save pattern (used by `default_use_worktree`) is the template for `last_agent` persistence.

## Verification

1. `cargo clippy --all-targets --all-features -- -D warnings` — zero warnings.
2. `cargo fmt --check`.
3. `cargo test -p protocol` — add a round-trip test for v5 messages with `agent: Codex` + `codex_sandbox: WorkspaceWrite`, plus one that decodes a v4-shaped `SpawnRequest` (missing `agent`) and confirms `Agent::Claude` default.
4. `cargo test -p daemon` — new unit test for codex arg assembly: assert that a workspace `SpawnRequest` with `agent: Codex`, `codex_sandbox: WorkspaceWrite`, prompt `"hi"`, and two members produces args `["--add-dir", "<m1>", "--sandbox", "workspace-write", "hi"]` (factor the arg-builder out of `spawn_interactive_session` if needed to make it testable).
5. `cd apps/tauri-app && pnpm typecheck`.
6. Manual e2e: install codex CLI (`npm i -g @openai/codex`) or point `RUSTLING_TULIP_CODEX` at a fake binary; in the app, open the spawn dialog, pick "codex", spawn against a single repo and a workspace, confirm both attach with live PTY, scrollback persists, and the session header shows ` · codex`.
7. Orphan check: with a live codex session running, `taskkill /F /IM rustling-tulipd.exe`, restart the app, confirm the codex session reattaches in read-only mode with the badge intact.
8. Backward-compat check: with the new daemon, load an existing `state.json` (no `last_agent`) and an existing `sessions/<id>/meta.json` (no `agent`) — both should decode cleanly and the missing fields default to `None` / `Claude`.

## Implementation notes (post-pickup)

Things that drifted from the original design once the code landed; recorded so future readers don't chase ghosts:

- **`RepoEntry` lives in `crates/protocol/src/lib.rs`, not `crates/daemon/src/state.rs`.** The detail plan's narrative referred to `state.rs` for `RepoEntry`; the actual addition went into `protocol::RepoEntry` because it crosses the wire. The "Critical files" list already named the protocol crate.
- **`persist_last_agent` lives in `crates/daemon/src/registry.rs`**, alongside `set_repo_worktree_default` / `set_workspace_worktree_default`. The detail plan placed it in `server.rs`. The registry-side helper follows the existing `state.mutate(|s| ...)` pattern.
- **`SessionRegistry::insert_orphan` was the third place needing `agent`.** Detail plan covered `SessionRecord` and orphan meta but missed the reattach path at `session.rs:229`. Without it codex orphans would reattach labeled as claude.
- **`meta_from_record` gained a positional arg** (`agent: Agent`). Already `#[expect(clippy::too_many_arguments)]`; one more slot stays under the same expectation.
- **Wire types use no `#[serde(default)]` for new fields.** Per the master-plan meta-decision (protocol breaking changes are free): `SpawnRequest.agent`, `SpawnRequest.codex_sandbox`, `SessionSnapshot.agent` are required at v5. Only on-disk artifacts keep defaults: `RepoEntry.last_agent` (state.json), `OrphanMeta.agent` (per-session sidecar), `PresetEntry.agent` and `PresetEntry.codex_sandbox` (user-edited `.rustling-tulip/presets.json`).
- **PresetEntry got both `agent` and `codex_sandbox`** (interview answer: include presets in this round). Threaded through `presets::build_spawn_request`. No dedicated codex-vs-headless guard in presets — presets always spawn `SessionMode::Interactive`, so the server-level `agent == Codex && mode == Headless` reject already covers them.
- **`SpawnConfig::extend_args` → `extend_claude_args`** to make the claude-specific branching obvious. The codex side uses a free `build_codex_args(cfg, members, initial_prompt)` for testability; it lives in `server.rs` next to the rename.
- **Headless guard placement.** Plan said "reject in `spawn_headless_session`"; implementation rejects earlier in `spawn_session` itself so the user gets the error before any git/worktree work runs. A `debug_assert!` in `spawn_headless_session` documents the invariant.
- **`codex --add-dir` for the interactive TUI is unconfirmed in Context7 docs.** The `--add-dir` flag is documented for `codex exec`; the bare `codex` interactive command is documented to accept `--sandbox`, `-m/--model`, `--yolo`, and a positional prompt, but `--add-dir` is not explicitly listed. The arg builder still emits it. If interactive `codex` rejects `--add-dir`, workspace-mode codex sessions will fail to spawn — fix is a one-line change in `build_codex_args` to drop or rename the flag.
- **Snap-back on agent flip.** When the user flips agent to codex while headless is selected, `SpawnDialog` snaps `runMode` back to interactive (`useEffect` on `agent + runMode`). Headless radio stays visually disabled with a tooltip.
- **Model list is claude-named.** The Advanced section's Model `<select>` still uses `CLAUDE_MODELS`. For codex sessions the dropdown shows a muted hint suggesting the user type a codex model id manually. Adding a proper `CODEX_MODELS` list is a follow-up if/when there's a stable enumeration of codex model ids worth pinning.
- **Sidebar agent badge** uses `tree-kind-tag` styling (the same idiom as `WS`/`REPO` tags) and only renders for `agent === "codex"`. The CSS class `tree-kind-tag` already exists and was reused as-is.
