# fake-claude

A deterministic stand-in for the `claude` CLI, so tests get a long-running PTY child without a real `claude` install or Anthropic credentials. The native client's live tier (`.\rt.ps1 native-e2e`, `apps/native/tests/support/live.rs`) points the daemon at it.

- `index.mjs` is the Node script: it prints a stable `[fake-claude] ready` banner, echoes stdin lines back, reports an EOT-terminated paste as `RT_PASTE_RESULT bytes=<n> sha=<hex>`, sets the window title on `/rename <title>`, streams deterministic output on `/stream`, and exits on `/exit` or SIGTERM/SIGBREAK. It accepts and ignores the `--add-dir`, `-p`, `--model` and `--permission-mode` flags the daemon may pass. A crash is appended to `fake-claude-crash.log` in the OS temp dir.
- `fake-claude.cmd` (Windows) and `fake-claude.sh` (POSIX) are shims that run `index.mjs` under `node`, forwarding every argument.

Wire it in by setting `RUSTLING_TULIP_CLAUDE` to the shim's path; the daemon resolves the CLI from that variable at spawn time (`crates/daemon/src/agents/claude.rs`). Node must be on `PATH`.
