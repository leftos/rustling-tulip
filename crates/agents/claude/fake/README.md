# fake-claude

A deterministic stand-in for the `claude` CLI the daemon spawns, so tests get a long-running PTY child without a real install, real credentials or a logged-in account. The native client's live tier (`.\rt.ps1 native-e2e`, `apps/native/tests/support/live.rs`) points the daemon at it.

`index.mjs` prints a stable `[fake-claude] ready` banner, then paints a bottom region the way Claude's TUI does, redrawn in place under the lines it prints, so the daemon's screen rules read it.

The region shows the idle prompt box (`❯ ` between two `─` rules over an auto-mode footer), the `· Cooking…` spinner line over the box while a turn runs, the `⎿  Interrupted · …` row after an Esc, the `✻ Churned for …` line over a `1 shell` footer after a turn that leaves a background shell, and an `AskUserQuestion` dialog ending in `Esc to cancel`.

Every paint sets the title to `◐ <name>` while working and `✳ <name>` otherwise (the name is the last `/rename` title, else `Claude Code`), and a resize repaints the region.

An Esc that arrives alone, with nothing after it for 50 ms, is the Esc key: during a turn it paints the interrupt row and runs no hook, as Claude fires none on Esc, and outside a turn it is ignored; an Esc with anything after it is ordinary input.

It echoes stdin lines back, reports an EOT-terminated paste as `RT_PASTE_RESULT bytes=<n> sha=<hex>`, sets the window title on `/rename <title>`, streams deterministic output on `/stream`, reports one environment variable on `/env <NAME>` as `RT_ENV <NAME> unset` (unset or empty), `RT_ENV <NAME> sha256=<hex>` (a name ending in `_KEY` or `_TOKEN`: the SHA-256 of the UTF-8 value, never the value) or `RT_ENV <NAME> <value>`, and exits on `/exit` or SIGTERM/SIGBREAK.

It accepts and ignores the `--add-dir`, `-p`, `--model` and `--permission-mode` flags the daemon may pass. It reads the `--settings` hooks document the daemon injects: on `/rt-hook prompt`, `/rt-hook ask` and `/rt-hook bg` it runs the configured hook commands with the event JSON on stdin.

The prompt cue leaves its turn open under the spinner, the ask cue paints the question dialog and then runs `Stop` 5 s later and paints the idle box, and the bg cue ends its turn with a `Stop` that lists a running background shell (the payloads are in `index.mjs`).

A crash is appended to `fake-claude-crash.log` in the OS temp dir.

`fake-claude.cmd` (Windows) and `fake-claude.sh` (POSIX) are shims that run `index.mjs` under `node`, forwarding every argument.

The Claude plugin hands this folder to the live tier through `fake_cli` (`crates/agents/claude/src/lib.rs`, feature `test-support`): the harness reads it from every plugin in `agent-registry` and sets the plugin's program variable, `RUSTLING_TULIP_CLAUDE` here, to the shim's path, and the daemon resolves the CLI from that variable at spawn time. Node must be on `PATH`.
