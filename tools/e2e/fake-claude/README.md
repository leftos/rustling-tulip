# fake-claude

A deterministic stand-in for the agent CLIs the daemon spawns — `claude`, `codex` and `cursor-agent` — so tests get a long-running PTY child without a real install, real credentials or a logged-in account. The native client's live tier (`.\rt.ps1 native-e2e`, `apps/native/tests/support/live.rs`) points the daemon at it, and `apps/native/tests/e2e_recover.rs` drives the codex and cursor modes.

`FAKE_AGENT` picks the mode, `claude` when unset. A spec sets it as an env row on the spawn, so one shim serves every agent while the daemon still resolves each CLI from its own variable.

- **claude** (the default): `index.mjs` prints a stable `[fake-claude] ready` banner, echoes stdin lines back, reports an EOT-terminated paste as `RT_PASTE_RESULT bytes=<n> sha=<hex>`, sets the window title on `/rename <title>`, streams deterministic output on `/stream`, and exits on `/exit` or SIGTERM/SIGBREAK. It accepts and ignores the `--add-dir`, `-p`, `--model` and `--permission-mode` flags the daemon may pass. A crash is appended to `fake-claude-crash.log` in the OS temp dir.
- **codex** (`FAKE_AGENT=codex`): on the first input line it writes the thread's rollout under `$CODEX_HOME` (else `~/.codex`) as `sessions/YYYY/MM/DD/rollout-<local time>-<thread id>.jsonl`, whose name and `session_meta` first line — with `payload.cwd`, `payload.id` and `payload.source: "cli"` — are what the daemon's `codex_rollout` reader accepts, so the daemon captures the thread id while the session runs. argv `resume … <id>` prints `RT_RESUMED <id>` and writes no rollout: a resumed thread already has its file. The thread id is the constant `0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b`.
- **cursor** (`FAKE_AGENT=cursor`): `create-chat` prints the chat id constant `3f1c2b9e-7a4d-4e8f-9b2a-6c5d4e3f2a1b` and exits: the daemon runs it to pre-create a chat and reads that id off the first line of its stdout. A spawn's `--resume <id>` — a fresh spawn's too, since it opens the pre-created chat — prints `RT_RESUMED <id>`.
- `fake-claude.cmd` (Windows) and `fake-claude.sh` (POSIX) are shims that run `index.mjs` under `node`, forwarding every argument.

Wire it in by setting `RUSTLING_TULIP_CLAUDE`, `RUSTLING_TULIP_CODEX` and `RUSTLING_TULIP_CURSOR_AGENT` to the shim's path; the daemon resolves each CLI from its own variable at spawn time (`crates/daemon/src/agents/`). Node must be on `PATH`.

Both id constants are repeated in `apps/native/tests/e2e_recover.rs` (`CODEX_THREAD_ID`, `CURSOR_CHAT_ID`), which asserts a session's recorded conversation id and the `RT_RESUMED` line against them: changing one means changing both.
