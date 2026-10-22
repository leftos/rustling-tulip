# fake-claude

A deterministic stand-in for the `claude` CLI the daemon spawns, so tests get a long-running PTY child without a real install, real credentials or a logged-in account. The native client's live tier (`.\rt.ps1 native-e2e`, `apps/native/tests/support/live.rs`) points the daemon at it.

`index.mjs` prints a stable `[fake-claude] ready` banner, echoes stdin lines back, reports an EOT-terminated paste as `RT_PASTE_RESULT bytes=<n> sha=<hex>`, sets the window title on `/rename <title>`, streams deterministic output on `/stream`, reports one environment variable on `/env <NAME>` as `RT_ENV <NAME> unset` (unset or empty), `RT_ENV <NAME> sha256=<hex>` (a name ending in `_KEY` or `_TOKEN`: the SHA-256 of the UTF-8 value, never the value) or `RT_ENV <NAME> <value>`, and exits on `/exit` or SIGTERM/SIGBREAK. It accepts and ignores the `--add-dir`, `-p`, `--model` and `--permission-mode` flags the daemon may pass, and reads the `--settings` hooks document the daemon injects: on `/rt-hook prompt` and `/rt-hook ask` it runs the configured hook commands with the event JSON on stdin, the ask cue printing a prompt line and then running `Stop` 5 s later (the payloads are in `index.mjs`). A crash is appended to `fake-claude-crash.log` in the OS temp dir.

`fake-claude.cmd` (Windows) and `fake-claude.sh` (POSIX) are shims that run `index.mjs` under `node`, forwarding every argument.

Wire it in by setting `RUSTLING_TULIP_CLAUDE` to the shim's path; the daemon resolves the CLI from that variable at spawn time (`crates/daemon/src/agents/`). Node must be on `PATH`.
