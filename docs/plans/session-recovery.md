# Recover ended sessions with `claude --resume`

Status: steps 0–4 of "Order of work" have landed (daemon history, transcript matching, tracer-log importer, `RecoverSessions`, the Tauri dialog), with the glossary entries, the CLAUDE.md `history/` line and `apps/native/tests/e2e_recover.rs`. Open: step 5, the native client's rail button, badge and dialog (Design 6), and its `tests/ui_recover.rs` specs.

## Context

On 2026-09-27, a dev daemon's startup tracer reap killed all 7 of the user's live RT sessions: 5 Claude sessions, plus 2 pwsh shells that each had `claude` running inside them. Nothing was left to bring them back.

- RT never tells claude which conversation id to use and never records it.
- The existing "Resume" (`ResumeAbandoned`, `crates/daemon/src/server.rs:5033`) only replays the spawn config plus the first prompt, into a new conversation.
- It needs `meta.json`, but the exit watcher (`attach_lifecycle`, `crates/daemon/src/session.rs:586`) deletes that the moment a child exits, and a killed tracer counts as an exit.

The goal: a native-client "Recover sessions" flow. It lists sessions that ended in the last 7 days and recovers one or several at once. Each comes back in the same repo, workspace or folder, running `claude --resume <conversation id>`.

## Decisions

- **Source:** the daemon keeps a history of ended sessions. New Claude spawns get `--session-id <uuid>`, so their conversation is known exactly. For shells, and for Claude sessions that predate this feature, the conversation is found by matching Claude transcripts on folder and time.
- **Shells that ran claude:** each row offers:
  - Claude session in the repo, when the folder is a registered repo.
  - Register the folder as a repo, then a Claude session, when the folder is an unregistered git repo.
  - A plain shell in that folder that runs `claude --resume <id>`, always available.
  - Default: Claude session when registered, else plain shell.
- **Scope:** everything that ended in the last 7 days, newest first. Unexpected ends (tracer lost, daemon lost it) are grouped by when they happened and pre-ticked. User-closed sessions are listed unticked.
- **Backfill:** import older ends from `logs/tracer-<id>.log`, so today's 7 show up too.
- **Other rules:**
  - Both clients: the Tauri app first, then the native client.
  - Headless sessions are not recoverable (claude `--print` runs) and are left out of the list.
  - Codex and Cursor sessions are out of scope.

## Design

### 1. Daemon: session history (`crates/daemon/src/history.rs`, new)

**Storage.** One file per ended session, `<config>/history/<rt-session-id>.json`, written atomically (same tmp+rename helper as `orphan.rs` sidecars). A history entry holds:
- `session_id`, `label`, `kind` (claude or shell), `mode`
- `spawn_config` (the existing `SpawnConfig`, `crates/protocol/src/lib.rs:710`) and `members` (repo id, name, branch, worktree path)
- `workspace_id`, `primary_cwd`, `current_cwd` (the shell's last OSC 7 folder)
- `started_at`, `ended_at`
- `end` (see below)
- `claude_session_id: Option<String>`
- `source`: `record` or `tracer_log`
- `recovered_at: Option<..>`

**End reasons.**
- `Exited { code }`: the child exited on its own.
- `StoppedByUser`: `stop_session`, or the client discarding it (e.g. the frozen Tauri app's auto-discard).
- `TracerLost`: pipe EOF with `read_loop` returning -1 (`tracer_client.rs:604`). This means the tracer was killed or crashed.
- `DaemonShutdown`: drained shutdown.
- Only `TracerLost` counts as "unexpected".

**Where entries are written.** Every end path writes the entry before it deletes `meta.json`:
- the `attach_lifecycle` exit watcher (`session.rs:570-590`)
- `stop_session` (`server.rs:5639`)
- `discard_session` (`:5463`)
- `discard_abandoned` (`:5110`)
- `shutdown_all` (`:2694`)
- The first write for a session wins, so a Stop followed by the exit watcher keeps `StoppedByUser`, not `Exited`.

**Retention.** Startup prunes entries older than 7 days. Recovered entries stay, marked `recovered_at`, until they age out. That keeps the importer from re-adding them.

**Conversation ids.**
- `ClaudeBackend::build_interactive_args` (`crates/daemon/src/agents/claude.rs:28`) adds `--session-id <uuid>` on every interactive spawn. The uuid is a fresh `Uuid::new_v4()`, stored as `claude_session_id` on `SessionRecord`, `OrphanMeta` (additive `#[serde(default)]` field, no version bump per `orphan.rs:22`) and the history entry.
- A spawn that carries a resume id emits `--resume <id>` instead, with no `--session-id` and no initial prompt, and keeps that id as `claude_session_id`. `--resume` without `--fork-session` keeps the same conversation id; this is checked in the fake-claude test below.
- Both are plumbed through `CommonSpawnFields` (`agents/mod.rs:22`) from a new additive field `SpawnRequest.resume_conversation: Option<String>`.
- Not recorded in the history entry, since it's not needed: `SpawnConfig` stays free of the resume id, so it is not replayed.

### 2. Daemon: transcript matching (`crates/daemon/src/transcripts.rs`, new)

**Claude home.** `$CLAUDE_CONFIG_DIR`, else `~/.claude`.

**Project folder name.** Every character outside `[A-Za-z0-9-]` in the cwd becomes `-`, so `D:\` becomes `D--` and `D:\in-the-sky` becomes `D--in-the-sky`. Confirmed against today's real folders. Before any match is accepted, the transcript's own first `"cwd"` field is compared with the folder, ignoring case and separator differences.

**Candidates** for a history entry lacking `claude_session_id`, or for a shell:
- the `*.jsonl` files in that project folder whose last write falls between the session's `started_at` and `ended_at + 2 min`
- newest first, up to 5
- Each candidate carries its id, last-write time, and a short title: the `summary` line if present, else the first user message trimmed to 80 characters.
- An entry with a known `claude_session_id` gets that single candidate, provided its file exists.

Candidates are computed when the list is requested, not stored.

### 3. Daemon: tracer-log importer (`crates/daemon/src/history.rs`)

At startup, after orphan recovery, scan `logs/tracer-*.log` files modified in the last 7 days. Skip any whose session id is live, abandoned, or already in history.

From each log, parse:
- `rt-tracer starting session_id=… cwd=…`
- `supervisor: about to spawn child program=… args=[…]`
- the last line's timestamp, as `ended_at`

The end reason:
- `StoppedByUser` if the log has `Stop request received`
- `Exited` if it has `child exited` with no Stop
- otherwise `TracerLost` (the log just stops)

Map the log to a target:
- **As built:** imported entries never get a `spawn_config`, because every in-place `SpawnTarget` names a branch and checks it out if the repo has moved off it.
- **Claude program:**
  - A cwd equal to a registered repo's path fills `kind`, the repo id and name in `members`.
  - A cwd plus `--add-dir` set matching a workspace's members fills `kind`, `workspace_id` and the member ids.
  - Otherwise `members` holds bare paths.
  - Recovery runs claude in `members[0]` with the others as `--add-dir` (`SpawnTarget::Standalone { cwd, add_dirs }`).
  - `--dangerously-skip-permissions` and `--model` are read from the command line into `skip_permissions` and `model`.
- **Shell program:** a folder-only entry with `current_cwd = cwd`.

Imported entries have `source: tracer_log`, no label (the UI shows the folder), and no `claude_session_id`, so transcript matching fills it in. The parser is tested against real log lines copied from this machine's logs.

### 4. Protocol (`crates/protocol/src/lib.rs`, additive only, no version bump)

- `ClientMessage::ListSessionHistory { request_id: Option<String> }` → `DaemonMessage::SessionHistory { entries: Vec<HistoryEntry> }`. Entries include candidates. The daemon also broadcasts `SessionHistory` when the history changes: after an unexpected end, and after a recovery.
- `ClientMessage::RecoverSessions { items: Vec<RecoverItem>, request_id }`. Each `RecoverItem` is `{ history_id, conversation_id: Option<String>, how: RecoverAs }`, where `RecoverAs` is `Claude`, `RegisterRepoThenClaude { path }`, `Shell`, or an `Unknown` fallback via `#[serde(other)]`.
- After every item has run, the daemon replies once with `RecoverResult { request_id, results }`, one result per item: the new session id or an error (see the wire contract below).
- The new nested enums get `#[serde(other)] Unknown` from day one.
- The Tauri TS mirror is not extended (frozen). Its default arm drops the unknown broadcast with a log line; this is checked in the protocol round-trip tests.

#### Wire contract (pinned so the Tauri UI and the daemon can be built in parallel)

All messages are snake_case tagged, as elsewhere in the protocol.

**`list_session_history` and its reply.** The client sends:

```json
{"type":"list_session_history","request_id":"r1"}
```

The daemon replies with `session_history`. It also broadcasts the same message, with `request_id: null`, whenever the history changes:

```json
{"type":"session_history","request_id":"r1","items":[
  {"entry":{"session_id":"<rt id>","label":"yaat:main","kind":"workspace","mode":"interactive","agent":"claude",
            "spawn_config":{},"members":[],"workspace_id":"ws1",
            "primary_cwd":"D:\\yaat","current_cwd":null,"program_name":"claude",
            "started_at":"2026-09-27T11:09:41Z","ended_at":"2026-09-27T18:29:06Z",
            "end":{"type":"tracer_lost"},"claude_session_id":null,"source":"tracer_log","recovered_at":null},
   "candidates":[{"id":"85573bb1-c581-489e-baaa-94d5a384744c","last_active":"2026-09-27T18:27:37Z","title":"Fix the …"}],
   "folder_is_git_repo":true,"folder_repo_id":"repo-yaat"}
]}
```

- `entry` is `HistoryEntry` (Design 1). The `kind`, `mode`, `agent`, `spawn_config` and `members` values use the same shapes as `SessionSnapshot` and `SpawnConfig` today.
- `end` is one of:
  - `{"type":"exited","code":N}`
  - `{"type":"stopped_by_user"}`
  - `{"type":"tracer_lost"}`
  - `{"type":"daemon_shutdown"}`
  - Unknown values are kept as `unknown`.
- `source` is `record` or `tracer_log`.
- `candidates` are newest first. When `claude_session_id` is known, `candidates` holds just that one conversation.
- `folder_is_git_repo` is true when the folder (`current_cwd`, else `primary_cwd`) is a git repo, registered or not.
- `folder_repo_id` is the registered repo whose path is that folder, else null.

**`recover_sessions` and its reply.** The client sends:

```json
{"type":"recover_sessions","request_id":"r2","items":[
  {"history_id":"<rt id>","conversation_id":"<uuid or null>","how":{"type":"claude"}},
  {"history_id":"<rt id>","conversation_id":"<uuid>","how":{"type":"register_repo_then_claude","path":"D:\\foo"}},
  {"history_id":"<rt id>","conversation_id":"<uuid or null>","how":{"type":"shell"}}
]}
```

After every item has run, the daemon replies to the requester only:

```json
{"type":"recover_result","request_id":"r2","results":[
  {"history_id":"<rt id>","session_id":"<new rt id>","error":null},
  {"history_id":"<rt id>","session_id":null,"error":"worktree D:\\… could not be recreated: …"}
]}
```

- The new sessions arrive through the normal `session_updated` broadcasts.
- An unknown `how` type decodes as `unknown`, and that item fails with an error.

### 5. Daemon: recovery (`server.rs`, new `recover_sessions` handler)

Per item:
- **`Claude`:** `spawn_config.to_clone_request()` (`protocol/src/lib.rs:832`, which keeps the branch and reuses the worktree, so the cwd, and with it claude's project key, matches), with `resume_conversation = conversation_id`, then `spawn_session`. Workspace targets rebuild `--add-dir` and the prelude through the existing `spawn_workspace`. Folder-only Claude entries go to a new `SpawnTarget::Standalone { cwd }` path for the Claude agent (today it is shell-only), running claude in that folder with `--resume`.
- **`RegisterRepoThenClaude { path }`:** register the folder through `register_repo()`, the same function `AddRepo` uses. As built, recovery then runs claude standalone in that folder with the entry's other members as `--add-dir`. It doesn't use an in-place `Single` target, because that could check out a branch.
- **`Shell`:** a plain-shell spawn in `current_cwd` (or `primary_cwd`), with a `prompt_injector` (`InjectorStep::Text { content: "claude --resume <id>", newline: true }` after a short startup `Delay`, `lib.rs:1213`) when a conversation id is chosen. Without one, a bare shell.
- **After each successful spawn:** set `recovered_at` on the entry and broadcast the updated `SessionHistory`.
- **Failures:** a missing worktree that cannot be recreated, or a conversation file that no longer exists, come back as `ActionFailed` for that item only. The other items still run.

### 6. Native client (`apps/native`)

- **Rail button.** A "Recover sessions" item on the activity rail (`activity_bar.rs:71`, above the Settings gear), with a badge counting unrecovered `TracerLost` entries (reusing `badge_text`). It opens the dialog. The client requests `ListSessionHistory` on connect and keeps the broadcast list in its model.
- **Dialog** (`recover_view.rs` + `recover.rs` state, modelled on `layout_chooser.rs` and the `session_menu.rs` helpers `backdrop` / `dialog_button` / `menu_item`):
  - Rows are grouped by end burst: unexpected ends within 60 s of each other form one group, "7 sessions lost at 11:29".
  - Each row has: a checkbox; the label, or the folder for imported rows; repo, workspace or folder; the end time and reason; a conversation choice (the candidates, each with its title and last-active time; the default is the known id, else the newest); and for shell and folder-only rows, a "Recover as" choice with only the options valid for that folder.
  - Pre-ticked: unrecovered `TracerLost` rows.
  - Disabled, with the reason shown: rows with no candidate and nothing else to offer, and rows already recovered.
  - Buttons: "Recover N", "Select all", "Cancel". Keyboard: arrows move, space toggles, Enter recovers, Esc closes.
  - "Is this folder a git repo?" (needed for `RegisterRepoThenClaude`) is answered by the daemon as a `folder_is_git_repo` flag on the entry. The client never touches the filesystem, so remote clients work too.
- **After recovery:** new sessions are placed as normal spawns (`spawns.rs` placement), and the dialog closes when every item has answered. Failures stay listed with their messages.

### 7. Docs

- Glossary entries for "session history", "recovery", "unexpected end", in the start-here doc the glossary rule names.
- A CLAUDE.md line for `<config>/history/`, under "Where things live on disk".
- Tick the plan item in `docs/plan.md`.

## Order of work (briefs)

0. **Land `fix/daemon-single-instance` first.** It has uncommitted work from the killed implementer run. Review the diff, run tests only (no daemon launch), commit, merge.
1. **History, part 1 (daemon + protocol types):** `history.rs` storage, end-reason capture on every end path, retention, and `--session-id` on Claude spawns.
2. **History, part 2:** `transcripts.rs` matching, the tracer-log importer, `ListSessionHistory` / `SessionHistory`.
3. **Recovery:** `RecoverSessions` with the three `RecoverAs` paths, `resume_conversation` plumbing, Standalone Claude target.
4. **Tauri app first** (done): the TS mirror, a "Recover sessions" button with its badge and the Design 6 dialog in React shipped in the Tauri app, which now lives on the `tauri` branch; `main` has no TS mirror, so this step does not apply there.
5. **Native client:** rail button, badge and dialog, as in Design 6.
6. Docs and plan tick. Then an e2e run through `.\rt.ps1 native-e2e` only.

## Verification

- **Unit tests (daemon):**
  - A history entry is written with the right `end` for each end path: exit code, Stop, discard, and tracer EOF (-1).
  - First-write-wins.
  - Retention prune.
  - The cwd → project-folder encoding (`D:\`, `D:\in-the-sky`, a UNC path, a path with spaces and dots).
  - Candidate selection from a fixture `CLAUDE_CONFIG_DIR` (time window, cwd mismatch rejected, newest first, title extraction).
  - The tracer-log parser on real log lines (killed, Stop, child exit; claude with `--add-dir`; pwsh).
  - Claude args: `--session-id` on normal spawns, `--resume` instead of it with no prompt when resuming.
  - `RecoverSessions` for each `RecoverAs`, including one failing item not blocking the others.
- **Protocol:** round-trip tests for the new messages; the unknown `RecoverAs` value lands in `Unknown`.
- **Native UI tests** (`tests/ui_recover.rs`, fake daemon from `tests/support/mod.rs`):
  - The badge count.
  - The dialog lists grouped rows with the unexpected ones pre-ticked.
  - The recover-as choices match the flags.
  - "Recover" sends one `RecoverSessions` with the chosen items.
  - Failures stay listed.
- **E2E:** only through `.\rt.ps1 native-e2e` (fully isolated config, binaries, worktrees and pipe prefix). Never launch a daemon by hand on this machine (memory `dev-daemon-isolation`).
  - Spawn a fake-claude session.
  - Kill its tracer through the harness.
  - Recover it, and assert fake-claude received `--resume <id>` with the same id it was spawned with via `--session-id`.
  - Fake-claude must log its argv; extend `tools/e2e/fake-claude/index.mjs` if it doesn't already.
- **Gates:** `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt`, and the daemon, protocol and native test suites.
- **Manual, after merge and restart:** the rail badge shows today's 7 imported entries; recovering a D:\ shell row offers "Plain shell" only, since D:\ is not a git repo.
